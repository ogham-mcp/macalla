// SPDX-License-Identifier: MIT
//
// The Swift half of the Apple Intelligence enhancement provider.
//
// A flat C ABI, the same shape as the speech engine's bridge in
// crates/macalla-engines/swift: only integers and heap C strings cross, and
// the caller frees every string with `macalla_ai_string_free`.
//
// THREADING: `macalla_ai_respond` and `macalla_ai_respond_shaped` block the
// calling thread while the model answers. Call them from a worker thread:
// never the main thread, which deadlocks, and never the audio callback.

import Foundation
import FoundationModels

/// Carries a value out of a `Task` back to the blocked caller. Safe because the
/// semaphore orders the write against the read, and a caller that timed out
/// never reads.
private final class Box<T>: @unchecked Sendable {
    var value: T
    init(_ value: T) { self.value = value }
}

/// 0 available, 1 this Mac is not eligible, 2 Apple Intelligence is switched
/// off, 3 the model is not ready yet, 4 a reason this build does not know.
/// Asked fresh every call: it is a setting the person can change at any time.
@_cdecl("macalla_ai_availability")
public func macallaAiAvailability() -> Int32 {
    switch SystemLanguageModel.default.availability {
    case .available: return 0
    case .unavailable(.deviceNotEligible): return 1
    case .unavailable(.appleIntelligenceNotEnabled): return 2
    case .unavailable(.modelNotReady): return 3
    case .unavailable: return 4
    }
}

/// One field, the text. Tier 1 returns text only and never a block kind.
private func textSchema() throws -> GenerationSchema {
    let text = DynamicGenerationSchema.Property(
        name: "text",
        description: "The dictated text with punctuation and capitalisation applied.",
        schema: DynamicGenerationSchema(type: String.self))
    return try GenerationSchema(
        root: DynamicGenerationSchema(name: "Block", properties: [text]),
        dependencies: [])
}

/// Runs `work` on a detached task and waits up to `timeoutMs` for it. Returns
/// its reply as a heap C string; or null, with either `declinedOut` set or a
/// message in `errorOut`.
///
/// `declinedOut` is set when the model's safety checks refused the text. They
/// refused ordinary dictation on 2026-09-11 ("run terragrunt against the azure
/// landing zone"), so a refusal is not a failure of the provider, and the
/// caller decides what it means.
///
/// After `timeoutMs` the request is cancelled and the caller gets an error.
/// Cancelled rather than abandoned: on 2026-09-11, once requests had timed out,
/// later runs slowed until every request timed out, and requests left running
/// in the system's inference service are the suspect.
private func answer(
    within timeoutMs: UInt32,
    _ declinedOut: UnsafeMutablePointer<Bool>,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>,
    _ work: @escaping @Sendable () async throws -> String
) -> UnsafeMutablePointer<CChar>? {
    let box = Box<Result<String, any Error>?>(nil)
    let done = DispatchSemaphore(value: 0)
    declinedOut.pointee = false
    let task = Task.detached {
        do {
            box.value = .success(try await work())
        } catch {
            box.value = .failure(error)
        }
        done.signal()
    }
    if done.wait(timeout: .now() + .milliseconds(Int(timeoutMs))) == .timedOut {
        task.cancel()
        errorOut.pointee = strdup("Apple Intelligence did not answer within \(timeoutMs) ms")
        return nil
    }
    switch box.value {
    case .success(let reply):
        return strdup(reply)
    case .failure(let error as LanguageModelSession.GenerationError):
        switch error {
        case .guardrailViolation, .refusal:
            declinedOut.pointee = true
        default:
            errorOut.pointee = strdup(String(describing: error))
        }
        return nil
    case .failure(let error):
        errorOut.pointee = strdup(String(describing: error))
        return nil
    case nil:
        errorOut.pointee = strdup("Apple Intelligence gave no answer")
        return nil
    }
}

/// The model's reply to `text` under `instructions`, as a heap C string; or
/// null, with either `declinedOut` set or a message in `errorOut` (see
/// `answer`).
///
/// A fresh session every call. A reused one accumulates a transcript, which was
/// measured slowing replies from 4 s to 14 s and making the model echo its
/// input. `guided` asks for the reply through a one-field schema
/// first, and as plain text if that call throws; otherwise plain text only.
@_cdecl("macalla_ai_respond")
public func macallaAiRespond(
    _ instructions: UnsafePointer<CChar>,
    _ text: UnsafePointer<CChar>,
    _ guided: Bool,
    _ timeoutMs: UInt32,
    _ declinedOut: UnsafeMutablePointer<Bool>,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> UnsafeMutablePointer<CChar>? {
    let instructions = String(cString: instructions)
    let text = String(cString: text)
    return answer(within: timeoutMs, declinedOut, errorOut) {
        let session = LanguageModelSession(instructions: instructions)
        // Greedy: always the most likely next token, the mode another dictation app uses. On
        // 2026-09-11 it gave the same replies as temperature 0 on all 20
        // fixtures, for both reply shapes; greedy is the one that names it.
        let options = GenerationOptions(sampling: .greedy)
        if guided {
            do {
                let reply = try await session.respond(to: text, schema: textSchema(), options: options)
                return try reply.content.value(String.self, forProperty: "text")
            } catch where !Task.isCancelled {
                // Plain text when the schema call throws, the shape Handy uses
                // (https://github.com/cjpais/Handy, src-tauri/swift/
                // apple_intelligence.swift; read, not copied). In a fresh
                // session, because a used one carries the failed exchange.
                let fresh = LanguageModelSession(instructions: instructions)
                return try await fresh.respond(to: text, options: options).content
            }
        }
        return try await session.respond(to: text, options: options).content
    }
}

/// The reply shape could not be built into a schema.
private struct ShapeError: Error, CustomStringConvertible {
    let reason: String
    var description: String { "the reply shape could not be read: \(reason)" }
}

/// Builds a `GenerationSchema` from the shape Macalla's core describes, sent as
/// JSON by `crates/macalla-enhancers/src/apple.rs` (`shape_json`):
///
///     {"type": "text"}
///     {"type": "one_of", "name": …, "choices": [ … ]}
///     {"type": "list", "of": { … }}
///     {"type": "record", "name": …, "fields": [{"name", "description", "shape"}]}
///
/// The root must be a record. A record below the root becomes a named
/// dependency and is referred to by name, the form
/// `GenerationSchema(root:dependencies:)` takes, and a name met again is built
/// once. Every property is required. It knows nothing of what the fields mean.
///
/// Apple's pages for these:
/// DynamicGenerationSchema's init(type:), init(name:description:anyOf:),
/// init(arrayOf:minimumElements:maximumElements:),
/// init(name:description:properties:) and init(referenceTo:);
/// DynamicGenerationSchema.Property; GenerationSchema(root:dependencies:).
private final class ShapeReader {
    private var dependencies: [DynamicGenerationSchema] = []
    private var named: Set<String> = []

    func schema(from json: String) throws -> GenerationSchema {
        guard let data = json.data(using: .utf8),
              let node = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              node["type"] as? String == "record"
        else { throw ShapeError(reason: "the root is not a record") }
        let root = try record(node)
        return try GenerationSchema(root: root, dependencies: dependencies)
    }

    private func schema(_ node: [String: Any]) throws -> DynamicGenerationSchema {
        switch node["type"] as? String {
        case "text":
            return DynamicGenerationSchema(type: String.self)
        case "one_of":
            guard let name = node["name"] as? String,
                  let choices = node["choices"] as? [String], !choices.isEmpty
            else { throw ShapeError(reason: "a choice needs a name and at least one choice") }
            return DynamicGenerationSchema(name: name, description: nil, anyOf: choices)
        case "list":
            guard let of = node["of"] as? [String: Any]
            else { throw ShapeError(reason: "a list needs the shape of its items") }
            return DynamicGenerationSchema(arrayOf: try schema(of), minimumElements: nil, maximumElements: nil)
        case "record":
            guard let name = node["name"] as? String
            else { throw ShapeError(reason: "a record needs a name") }
            if !named.contains(name) {
                named.insert(name)
                dependencies.append(try record(node))
            }
            return DynamicGenerationSchema(referenceTo: name)
        default:
            throw ShapeError(reason: "an unknown kind of shape")
        }
    }

    private func record(_ node: [String: Any]) throws -> DynamicGenerationSchema {
        guard let name = node["name"] as? String,
              let fields = node["fields"] as? [[String: Any]]
        else { throw ShapeError(reason: "a record needs a name and fields") }
        let properties = try fields.map { field -> DynamicGenerationSchema.Property in
            guard let fieldName = field["name"] as? String,
                  let shape = field["shape"] as? [String: Any]
            else { throw ShapeError(reason: "a field needs a name and a shape") }
            return DynamicGenerationSchema.Property(
                name: fieldName,
                description: field["description"] as? String,
                schema: try schema(shape),
                isOptional: false)
        }
        return DynamicGenerationSchema(name: name, description: nil, properties: properties)
    }
}

/// The model's reply to `text` under `instructions`, constrained to the shape
/// described by `shape` (JSON, see `ShapeReader`), as its JSON in a heap C
/// string; or null, with either `declinedOut` set or a message in `errorOut`
/// (see `answer`).
///
/// Guided generation: "the framework uses constrained sampling when generating
/// output", and "the model generates properties in the order they're declared"
/// (Apple's guided-generation article). A
/// fresh session and greedy sampling, as for `macalla_ai_respond`. A schema
/// that cannot be built, or a call that throws, is an error: there is no
/// plain-text retry, because a summary asked for as plain text left out every
/// quote when measured on 2026-09-13.
@_cdecl("macalla_ai_respond_shaped")
public func macallaAiRespondShaped(
    _ instructions: UnsafePointer<CChar>,
    _ text: UnsafePointer<CChar>,
    _ shape: UnsafePointer<CChar>,
    _ timeoutMs: UInt32,
    _ declinedOut: UnsafeMutablePointer<Bool>,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> UnsafeMutablePointer<CChar>? {
    let instructions = String(cString: instructions)
    let text = String(cString: text)
    let shape = String(cString: shape)
    return answer(within: timeoutMs, declinedOut, errorOut) {
        let schema = try ShapeReader().schema(from: shape)
        let session = LanguageModelSession(instructions: instructions)
        let reply = try await session.respond(
            to: text, schema: schema, options: GenerationOptions(sampling: .greedy))
        return reply.content.jsonString
    }
}

/// The most recently prewarmed session, kept so the system has a reason to keep
/// the model loaded until a request comes. Requests never use it: each opens a
/// fresh session.
private final class WarmSession: @unchecked Sendable {
    private let lock = NSLock()
    private var session: LanguageModelSession?

    func replace(with next: LanguageModelSession) {
        lock.lock()
        session = next
        lock.unlock()
    }
}

private let warmSession = WarmSession()

/// Ask the system to load the model ahead of a request, as another dictation app does when
/// cleanup is switched on. Returns at once; the loading happens in the
/// background.
@_cdecl("macalla_ai_prewarm")
public func macallaAiPrewarm(_ instructions: UnsafePointer<CChar>) {
    let session = LanguageModelSession(instructions: String(cString: instructions))
    session.prewarm()
    warmSession.replace(with: session)
}

/// Free a string returned by any function here.
@_cdecl("macalla_ai_string_free")
public func macallaAiStringFree(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}
