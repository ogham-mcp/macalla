// SPDX-License-Identifier: MIT
//
// The Swift half of Apple's on-device translation backend.
//
// A flat C ABI, the same shape as crates/macalla-enhancers/swift: only
// integers, C strings and one C function pointer cross, and the caller frees
// every returned string with `macalla_tr_string_free`.
//
// Codes returned by `macalla_tr_status` and `macalla_tr_translate`, read by
// src/apple.rs:
//   0  installed, or finished
//   1  the pair is supported but not installed on this Mac
//   2  the pair is unsupported (either language, or the two together)
//   4  stopped: the caller's callback asked to stop, or the system cancelled
//   5  any other failure; `macalla_tr_translate` puts a message in `errorOut`
//   6  `macalla_tr_translate` ran out of the time it was given; the session is
//      cancelled, and a message is in `errorOut`
//
// THREADING: every function here but `macalla_tr_serve_main` blocks the
// calling thread until the system answers. Call them from a worker thread,
// never the main thread: the framework finishes its work on the main queue,
// so a blocked main thread deadlocks it (found 2026-09-27: `status` answered
// in 40 ms with the main run loop served, and never with it blocked). A
// program whose main thread has no event loop of its own, such as the CLI,
// serves it with `macalla_tr_serve_main` meanwhile. The translate callback is
// called on the calling thread, never on another.

import Foundation
import Translation

/// Carries a value out of a `Task` back to the blocked caller. Safe because the
/// semaphore orders the write against the read.
private final class Box<T>: @unchecked Sendable {
    var value: T
    init(_ value: T) { self.value = value }
}

/// Runs `work` on a detached task and blocks until it returns.
private func blocking<T: Sendable>(_ work: @escaping @Sendable () async -> T) -> T? {
    let box = Box<T?>(nil)
    let done = DispatchSemaphore(value: 0)
    Task.detached {
        box.value = await work()
        done.signal()
    }
    // Asking the system about a pair is quick; a minute means it is stuck.
    if done.wait(timeout: .now() + .seconds(60)) == .timedOut {
        return nil
    }
    return box.value
}

/// High fidelity where the SDK and the OS have the choice (macOS 26.4): the
/// model the spike measured, and the one that works offline with Apple
/// Intelligence on. It is a preference: the framework falls back to the
/// downloadable models on a Mac without Apple Intelligence.
private func availability() -> LanguageAvailability {
    if #available(macOS 26.4, *) {
        return LanguageAvailability(preferredStrategy: .highFidelity)
    }
    return LanguageAvailability()
}

private func session(_ source: Locale.Language, _ target: Locale.Language) -> TranslationSession {
    if #available(macOS 26.4, *) {
        return TranslationSession(
            installedSource: source, target: target, preferredStrategy: .highFidelity)
    }
    return TranslationSession(installedSource: source, target: target)
}

/// Whether `source` into `target` can be translated now: 0, 1 or 2, or 5 if
/// the system did not answer.
@_cdecl("macalla_tr_status")
public func macallaTrStatus(
    _ source: UnsafePointer<CChar>,
    _ target: UnsafePointer<CChar>
) -> Int32 {
    let from = Locale.Language(identifier: String(cString: source))
    let to = Locale.Language(identifier: String(cString: target))
    let status = blocking { () async -> Int32 in
        switch await availability().status(from: from, to: to) {
        case .installed: return 0
        case .supported: return 1
        case .unsupported: return 2
        @unknown default: return 5
        }
    }
    return status ?? 5
}

/// Every language the system can translate, one BCP-47 code per line, as a
/// heap C string; or null if the system did not answer.
@_cdecl("macalla_tr_languages")
public func macallaTrLanguages() -> UnsafeMutablePointer<CChar>? {
    let codes = blocking { () async -> [String] in
        await availability().supportedLanguages.map { $0.minimalIdentifier }
    }
    guard let codes else { return nil }
    return strdup(codes.joined(separator: "\n"))
}

/// The code for a thrown error, matched by case, never by NSError code: every
/// `TranslationError` has code 1.
private func code(of error: any Error) -> Int32 {
    if TranslationError.notInstalled ~= error { return 1 }
    if TranslationError.unsupportedSourceLanguage ~= error
        || TranslationError.unsupportedTargetLanguage ~= error
        || TranslationError.unsupportedLanguagePairing ~= error
    {
        return 2
    }
    if TranslationError.alreadyCancelled ~= error || error is CancellationError { return 4 }
    return 5
}

/// What the translation task has for the blocked caller: answers so far, and
/// how it ended once it has.
private final class Inbox: @unchecked Sendable {
    private let lock = NSLock()
    private var answers: [(UInt32, String)] = []
    private var ending: (Int32, String)?
    private var running: TranslationSession?
    let ready = DispatchSemaphore(value: 0)

    func started(_ session: TranslationSession) {
        lock.lock()
        running = session
        lock.unlock()
    }

    func push(_ index: UInt32, _ text: String) {
        lock.lock()
        answers.append((index, text))
        lock.unlock()
        ready.signal()
    }

    func end(_ code: Int32, _ message: String) {
        lock.lock()
        ending = (code, message)
        lock.unlock()
        ready.signal()
    }

    func take() -> ([(UInt32, String)], (Int32, String)?) {
        lock.lock()
        defer { lock.unlock() }
        let taken = answers
        answers = []
        return (taken, ending)
    }

    func cancel() {
        lock.lock()
        let session = running
        lock.unlock()
        session?.cancel()
    }
}

/// Called once per translation, in the order the answers arrive, which
/// Apple does not document to be the order asked: the caller's context, the
/// index of the text it translates, and its translation. Returns whether
/// to carry on.
public typealias MacallaTrEach =
    @convention(c) (UnsafeMutableRawPointer?, UInt32, UnsafePointer<CChar>) -> Bool

/// Translate `count` texts from `source` into `target` in one session, with
/// `translate(batch:)`, and hand each answer to `each` on this thread as it
/// arrives. Each request carries its index as its `clientIdentifier`, and the
/// answer's own identifier is what is handed back, so an answer can never be
/// put on the wrong text here. Returns a code (see the top of this file).
///
/// `limitMs` bounds the whole call: once it runs out the task and the session
/// are cancelled and the call returns 6, so a translation service that stops
/// answering cannot hold the caller's thread for ever. The caller picks it.
@_cdecl("macalla_tr_translate")
public func macallaTrTranslate(
    _ source: UnsafePointer<CChar>,
    _ target: UnsafePointer<CChar>,
    _ texts: UnsafePointer<UnsafePointer<CChar>?>,
    _ count: UInt32,
    _ limitMs: UInt32,
    _ context: UnsafeMutableRawPointer?,
    _ each: MacallaTrEach,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> Int32 {
    let from = Locale.Language(identifier: String(cString: source))
    let to = Locale.Language(identifier: String(cString: target))
    let sources = (0..<Int(count)).map { index in
        texts[index].map { String(cString: $0) } ?? ""
    }
    let inbox = Inbox()
    let task = Task.detached {
        let requests = sources.enumerated().map { index, text in
            TranslationSession.Request(sourceText: text, clientIdentifier: String(index))
        }
        let running = session(from, to)
        inbox.started(running)
        do {
            for try await response in running.translate(batch: requests) {
                guard let id = response.clientIdentifier, let index = UInt32(id) else {
                    inbox.end(5, "an answer came back without the id it was sent with")
                    return
                }
                inbox.push(index, response.targetText)
            }
            inbox.end(0, "")
        } catch {
            inbox.end(code(of: error), String(describing: error))
        }
    }
    let deadline = DispatchTime.now() + .milliseconds(Int(limitMs))
    var answered = 0
    while true {
        if inbox.ready.wait(timeout: deadline) == .timedOut {
            task.cancel()
            inbox.cancel()
            errorOut.pointee = strdup(
                "no end to the batch within \(limitMs) ms: \(answered) of \(count) answers had come")
            return 6
        }
        let (answers, ending) = inbox.take()
        answered += answers.count
        for (index, text) in answers {
            let carryOn = text.withCString { each(context, index, $0) }
            if !carryOn {
                task.cancel()
                inbox.cancel()
                return 4
            }
        }
        if let (code, message) = ending {
            if code == 5 {
                errorOut.pointee = strdup(message)
            }
            return code
        }
    }
}

/// Serve the main run loop, on the main thread, until `done(context)` says
/// the work on another thread has finished: what a program with no event loop
/// of its own does while a function above runs on a worker thread.
@_cdecl("macalla_tr_serve_main")
public func macallaTrServeMain(
    _ context: UnsafeMutableRawPointer?,
    _ done: @convention(c) (UnsafeMutableRawPointer?) -> Bool
) {
    while !done(context) {
        _ = RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
    }
}

/// Free a string returned by any function here.
@_cdecl("macalla_tr_string_free")
public func macallaTrStringFree(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}
