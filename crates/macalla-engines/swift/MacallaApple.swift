// SPDX-License-Identifier: MIT
//
// The Swift half of the Apple SpeechTranscriber engine.
//
// Exposed to Rust as a flat C ABI: no Swift types cross the boundary, and the
// only things that come back are integers and heap C strings the caller frees
// with `macalla_apple_string_free`.
//
// Results cross as JSON. Transcripts are small and arrive a few times a second,
// so the encode costs nothing against the paste budget, and it buys
// a boundary with no struct layout to keep in step between two languages.
//
// THREADING: every entry point here blocks the calling thread while an async
// Swift task runs. Call them from the pipeline thread. Never from the main
// thread and never from the audio callback -- the first deadlocks, the second
// is a dropout.

import AVFoundation
import CoreMedia
import Foundation
import Speech

/// A failure with a message the Rust side can log. `String` alone cannot be
/// thrown, and a dedicated type keeps the message out of the control flow.
private struct BridgeError: Error {
    let message: String
}

/// Hands a buffer to `AVAudioConverter`'s input block exactly once.
///
/// The block is `@Sendable` under Swift 6 even though the conversion is
/// synchronous, so a captured buffer and a captured `var` both trip strict
/// concurrency. Boxing them says what is actually true: one thread touches
/// this, and it is this one.
private final class ConverterInput: @unchecked Sendable {
    private var buffer: AVAudioPCMBuffer?
    init(_ buffer: AVAudioPCMBuffer) { self.buffer = buffer }
    /// The buffer the first time, nil every time after.
    func take() -> AVAudioPCMBuffer? {
        defer { buffer = nil }
        return buffer
    }
}

/// Carries a value out of a `Task` back to the blocked caller. Safe because the
/// semaphore below is what orders the write against the read.
private final class Box<T>: @unchecked Sendable {
    var value: T
    init(_ value: T) { self.value = value }
}

/// Run an async closure from a synchronous C entry point and wait for it.
private func blocking<T>(_ body: @escaping @Sendable () async -> T) -> T {
    let box = Box<T?>(nil)
    let done = DispatchSemaphore(value: 0)
    Task.detached {
        box.value = await body()
        done.signal()
    }
    done.wait()
    return box.value!
}

private func cString(_ s: String) -> UnsafeMutablePointer<CChar>? {
    strdup(s)
}

/// Apple's `Locale.identifier` is "en_US"; everything on the Rust side speaks
/// BCP-47, which is "en-US". Convert at the boundary rather than in three
/// places downstream. `.bcp47` does more than swap the separator -- it also
/// normalises script and variant subtags.
private func bcp47(_ locale: Locale) -> String {
    locale.identifier(.bcp47)
}

private func json(_ any: Any) -> String {
    guard let data = try? JSONSerialization.data(withJSONObject: any),
          let text = String(data: data, encoding: .utf8)
    else { return "[]" }
    return text
}

// MARK: - Capability

/// 1 when SpeechTranscriber can run on this machine, 0 otherwise.
///
/// Measured 2026-09-08: true with Apple Intelligence switched off. It is not a
/// prerequisite for this API, unlike FoundationModels.
@_cdecl("macalla_apple_is_available")
public func macallaAppleIsAvailable() -> Int32 {
    SpeechTranscriber.isAvailable ? 1 : 0
}

/// JSON array of BCP-47 tags the engine supports at all.
@_cdecl("macalla_apple_supported_locales")
public func macallaAppleSupportedLocales() -> UnsafeMutablePointer<CChar>? {
    cString(blocking {
        json(await SpeechTranscriber.supportedLocales.map(bcp47).sorted())
    })
}

/// JSON array of BCP-47 tags whose assets are actually installed.
///
/// Supported is not installed. On the reference machine English and German were
/// present and Spanish and Italian were not, so onboarding has to be able to
/// tell the difference rather than offering a language that will fail.
@_cdecl("macalla_apple_installed_locales")
public func macallaAppleInstalledLocales() -> UnsafeMutablePointer<CChar>? {
    cString(blocking {
        json(await SpeechTranscriber.installedLocales.map(bcp47).sorted())
    })
}

/// Free a string returned by any function here.
@_cdecl("macalla_apple_string_free")
public func macallaAppleStringFree(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}

// MARK: - Streaming session

/// One open dictation.
///
/// Owns the analyzer, the input continuation and the task collecting results.
/// `@unchecked Sendable` because `pending` is only touched under `lock`, and
/// `finished` and `stalled` only by the one thread that drives the session.
private final class Session: @unchecked Sendable {
    /// How many converted buffers may wait for the analyzer, a tenth of a
    /// second each, so 30 s of audio. Live audio arrives in real time and the
    /// analyzer keeps up, so a dictation never comes near it. A file pushed as
    /// fast as it decodes did: measured 2026-09-10, a 62-minute file peaked at
    /// 601.7 MB a second after decoding, then fell as the analyzer drained a
    /// queue holding most of the file.
    static let queueLimit = 300
    /// How long `push` waits for room before deciding the analyzer has
    /// stopped taking audio.
    static let queueWaitSeconds = 10

    let analyzer: SpeechAnalyzer
    let transcriber: SpeechTranscriber
    let continuation: AsyncStream<AnalyzerInput>.Continuation
    /// One per buffer that may wait; the analyzer gives them back as it reads.
    let slots: DispatchSemaphore
    let converter: AVAudioConverter
    let inputFormat: AVAudioFormat
    let analyzerFormat: AVAudioFormat

    private let lock = NSLock()
    private var pending: [[String: Any]] = []
    private var collector: Task<Void, Never>?
    private var finished = false
    private var stalled = false

    /// What Rust sends: 16 kHz mono float, the one format the pipeline works in.
    static let pipelineFormat = AVAudioFormat(
        commonFormat: .pcmFormatFloat32,
        sampleRate: 16_000,
        channels: 1,
        interleaved: false
    )

    private init(
        analyzer: SpeechAnalyzer,
        transcriber: SpeechTranscriber,
        continuation: AsyncStream<AnalyzerInput>.Continuation,
        slots: DispatchSemaphore,
        converter: AVAudioConverter,
        inputFormat: AVAudioFormat,
        analyzerFormat: AVAudioFormat
    ) {
        self.analyzer = analyzer
        self.transcriber = transcriber
        self.continuation = continuation
        self.slots = slots
        self.converter = converter
        self.inputFormat = inputFormat
        self.analyzerFormat = analyzerFormat
    }

    static func open(locale identifier: String) async -> Result<Session, BridgeError> {
        guard let inputFormat = pipelineFormat else {
            return .failure(BridgeError(message: "could not describe 16 kHz mono float audio"))
        }
        let transcriber = SpeechTranscriber(
            locale: Locale(identifier: identifier),
            transcriptionOptions: [],
            // Volatile results are the live preview. They are shown in the
            // overlay and never pasted; `isFinal` is what separates them.
            reportingOptions: [.volatileResults],
            attributeOptions: []
        )
        guard let analyzerFormat = await SpeechAnalyzer.bestAvailableAudioFormat(
            compatibleWith: [transcriber]
        ) else {
            return .failure(BridgeError(message: "no audio format the analyzer accepts for \(identifier)"))
        }
        guard let converter = AVAudioConverter(from: inputFormat, to: analyzerFormat) else {
            return .failure(BridgeError(message: "cannot convert 16 kHz mono to the analyzer's format"))
        }

        let analyzer = SpeechAnalyzer(modules: [transcriber])
        do {
            try await analyzer.prepareToAnalyze(in: analyzerFormat)
        } catch {
            return .failure(BridgeError(message: "preparing the analyzer for \(identifier): \(error)"))
        }

        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        let slots = DispatchSemaphore(value: queueLimit)
        let session = Session(
            analyzer: analyzer,
            transcriber: transcriber,
            continuation: continuation,
            slots: slots,
            converter: converter,
            inputFormat: inputFormat,
            analyzerFormat: analyzerFormat
        )
        session.startCollecting()
        do {
            try await analyzer.start(inputSequence: MeteredInput(base: stream, slots: slots))
        } catch {
            return .failure(BridgeError(message: "starting the analyzer for \(identifier): \(error)"))
        }
        return .success(session)
    }

    /// Drain `transcriber.results` into `pending` for as long as the analyzer
    /// runs. Results arrive whenever the engine has something; the Rust side
    /// collects them on its own schedule with `poll`.
    private func startCollecting() {
        collector = Task { [weak self] in
            guard let self else { return }
            do {
                for try await result in self.transcriber.results {
                    self.append([
                        "text": String(result.text.characters),
                        "is_final": result.isFinal,
                        "start_ms": Self.milliseconds(result.range.start),
                        "end_ms": Self.milliseconds(result.range.end),
                    ])
                }
            } catch {
                self.append([
                    "text": "",
                    "is_final": false,
                    "start_ms": 0,
                    "end_ms": 0,
                    "error": "\(error)",
                ])
            }
        }
    }

    private static func milliseconds(_ time: CMTime) -> Int {
        let seconds = CMTimeGetSeconds(time)
        return seconds.isFinite ? Int((seconds * 1000).rounded()) : 0
    }

    private func append(_ result: [String: Any]) {
        lock.lock()
        pending.append(result)
        lock.unlock()
    }

    func push(_ samples: UnsafePointer<Float>, _ count: Int) -> String? {
        if finished { return "the session has already been finished" }
        guard count > 0 else { return nil }
        guard let input = AVAudioPCMBuffer(
            pcmFormat: inputFormat,
            frameCapacity: AVAudioFrameCount(count)
        ), let channel = input.floatChannelData else {
            return "could not allocate an input buffer for \(count) samples"
        }
        channel[0].update(from: samples, count: count)
        input.frameLength = AVAudioFrameCount(count)

        let ratio = analyzerFormat.sampleRate / inputFormat.sampleRate
        let capacity = AVAudioFrameCount(Double(count) * ratio) + 1024
        guard let output = AVAudioPCMBuffer(pcmFormat: analyzerFormat, frameCapacity: capacity)
        else {
            return "could not allocate an analyzer buffer"
        }
        var error: NSError?
        let once = ConverterInput(input)
        converter.convert(to: output, error: &error) { _, status in
            guard let buffer = once.take() else {
                status.pointee = .noDataNow
                return nil
            }
            status.pointee = .haveData
            return buffer
        }
        if let error { return "converting to the analyzer's format: \(error)" }
        guard output.frameLength > 0 else { return nil }
        // Wait for room, so audio arrives no faster than it is analysed. One
        // timeout is enough to know: after it, fail at once rather than wait
        // again on every push.
        if stalled { return "the engine stopped taking audio" }
        if slots.wait(timeout: .now() + .seconds(Self.queueWaitSeconds)) == .timedOut {
            stalled = true
            return "the engine took no audio for \(Self.queueWaitSeconds) seconds"
        }
        if case .terminated = continuation.yield(AnalyzerInput(buffer: output)) {
            // Nobody will read it, so nobody will give the slot back. Dropped
            // silently, as before the queue had a limit.
            slots.signal()
        }
        return nil
    }

    /// Everything collected since the last call.
    func drain() -> [[String: Any]] {
        lock.lock()
        defer { lock.unlock() }
        let out = pending
        pending.removeAll(keepingCapacity: true)
        return out
    }

    /// Close the input and wait for the engine to finalise the trailing
    /// segment. This is the wait Spike C measured at 127 ms on the reference
    /// machine, and it is the bulk of release-to-paste on this path.
    func finish() async -> [[String: Any]] {
        if finished { return drain() }
        finished = true
        continuation.finish()
        do {
            try await analyzer.finalizeAndFinishThroughEndOfInput()
        } catch {
            append(["text": "", "is_final": false, "start_ms": 0, "end_ms": 0,
                    "error": "finalising: \(error)"])
        }
        await collector?.value
        collector = nil
        return drain()
    }
}

/// The analyzer's input, giving a slot back each time the analyzer takes a
/// buffer. With the wait in `Session.push`, this is the stream's backpressure:
/// an AsyncStream can be given a buffering limit, but past it the stream drops
/// buffers rather than making the producer wait, and dropped audio is lost
/// words.
private struct MeteredInput: AsyncSequence, Sendable {
    typealias Element = AnalyzerInput

    let base: AsyncStream<AnalyzerInput>
    let slots: DispatchSemaphore

    struct AsyncIterator: AsyncIteratorProtocol {
        var base: AsyncStream<AnalyzerInput>.Iterator
        let slots: DispatchSemaphore

        mutating func next() async -> AnalyzerInput? {
            guard let input = await base.next() else { return nil }
            slots.signal()
            return input
        }
    }

    func makeAsyncIterator() -> AsyncIterator {
        AsyncIterator(base: base.makeAsyncIterator(), slots: slots)
    }
}

private func handle(_ pointer: UnsafeMutableRawPointer) -> Session {
    Unmanaged<Session>.fromOpaque(pointer).takeUnretainedValue()
}

/// Open a session for a BCP-47 locale. Returns null on failure and writes the
/// reason to `errorOut`, which the caller frees.
@_cdecl("macalla_apple_stream_open")
public func macallaAppleStreamOpen(
    _ locale: UnsafePointer<CChar>,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> UnsafeMutableRawPointer? {
    let identifier = String(cString: locale)
    switch blocking({ await Session.open(locale: identifier) }) {
    case .success(let session):
        return Unmanaged.passRetained(session).toOpaque()
    case .failure(let failure):
        errorOut?.pointee = cString(failure.message)
        return nil
    }
}

/// Feed 16 kHz mono float samples. Returns null on success, else the reason.
@_cdecl("macalla_apple_stream_push")
public func macallaAppleStreamPush(
    _ pointer: UnsafeMutableRawPointer,
    _ samples: UnsafePointer<Float>,
    _ count: Int
) -> UnsafeMutablePointer<CChar>? {
    // A Rust thread has no autorelease pool that drains while it runs, so what
    // AVFoundation autoreleases here would build up for as long as the thread
    // lives. Measured 2026-09-10 on an hour-long file: about 150 MB over the
    // run before this pool, flat after. The desktop app's dictation thread
    // lives as long as the app. Drain one per call.
    autoreleasepool {
        handle(pointer).push(samples, count).flatMap(cString)
    }
}

/// JSON array of whatever the engine has produced since the last call.
@_cdecl("macalla_apple_stream_poll")
public func macallaAppleStreamPoll(
    _ pointer: UnsafeMutableRawPointer
) -> UnsafeMutablePointer<CChar>? {
    // As for push: JSONSerialization autoreleases too, and poll is called as
    // often, so drain one per call.
    autoreleasepool {
        cString(json(handle(pointer).drain()))
    }
}

/// Close the input, wait for the trailing segment, and return what is left.
@_cdecl("macalla_apple_stream_finish")
public func macallaAppleStreamFinish(
    _ pointer: UnsafeMutableRawPointer
) -> UnsafeMutablePointer<CChar>? {
    let session = handle(pointer)
    return cString(json(blocking { await session.finish() }))
}

/// Release the session. The handle is dead afterwards.
@_cdecl("macalla_apple_stream_close")
public func macallaAppleStreamClose(_ pointer: UnsafeMutableRawPointer) {
    Unmanaged<Session>.fromOpaque(pointer).release()
}

// MARK: - Decoding

/// Decode any audio file the OS understands into 16 kHz mono float.
///
/// AVFoundation handles aiff, wav, caf, mp3, m4a and the rest, so `macalla
/// transcribe` needs no decoding crate and no ffmpeg. Off macOS this has no
/// equivalent yet and that is a Phase 2 problem, alongside Parakeet.
///
/// The samples go to `onChunk` as they are converted, a tenth of a second at a
/// time, and are never gathered on this side. With every chunk comes the
/// length the whole file is expected to decode to, so the caller can size one
/// buffer and fill it. Gathering them here meant a Swift array, a second
/// buffer to hand across and a copy on the Rust side: two copies of the file
/// at once, 472.0 MB for an hour of audio on 2026-09-10.
///
/// Returns false on failure, with the reason in `errorOut`.
@_cdecl("macalla_apple_decode_file")
public func macallaAppleDecodeFile(
    _ path: UnsafePointer<CChar>,
    _ context: UnsafeMutableRawPointer?,
    _ onChunk: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<Float>, Int, Int) -> Void,
    _ errorOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Bool {
    let url = URL(fileURLWithPath: String(cString: path))

    guard let target = Session.pipelineFormat else {
        errorOut?.pointee = cString("could not describe 16 kHz mono float audio")
        return false
    }
    let file: AVAudioFile
    do {
        file = try AVAudioFile(forReading: url)
    } catch {
        // The OS decoder handles aiff, wav, caf, flac, mp3, m4a, mp4 and mov
        // (audio track only) and opus. It does NOT handle WebM, which is what
        // browsers record by default, and the raw CoreAudio code for that is
        // 1954115 -- unreadable unless you already know it means "fmt?".
        errorOut?.pointee = cString(
            "cannot read \(url.lastPathComponent): \(error.localizedDescription). "
            + "Supported: aiff, wav, caf, flac, mp3, m4a, mp4, mov, opus. "
            + "WebM is not; convert it first."
        )
        return false
    }
    let source = file.processingFormat
    guard let converter = AVAudioConverter(from: source, to: target) else {
        errorOut?.pointee = cString("cannot convert \(source.sampleRate) Hz to 16 kHz mono")
        return false
    }

    // A tenth of a second at a time, the same shape the live path uses.
    let chunk = AVAudioFrameCount(source.sampleRate / 10)
    let ratio = target.sampleRate / source.sampleRate
    let expected = Int(Double(file.length) * ratio) + 1024
    var delivered = 0

    while file.framePosition < file.length {
        guard let input = AVAudioPCMBuffer(pcmFormat: source, frameCapacity: chunk) else { break }
        do {
            try file.read(into: input, frameCount: chunk)
        } catch {
            errorOut?.pointee = cString("reading \(url.lastPathComponent): \(error)")
            return false
        }
        if input.frameLength == 0 { break }

        let capacity = AVAudioFrameCount(Double(input.frameLength) * ratio) + 1024
        guard let output = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: capacity) else { break }
        var error: NSError?
        let once = ConverterInput(input)
        converter.convert(to: output, error: &error) { _, status in
            guard let buffer = once.take() else {
                status.pointee = .noDataNow
                return nil
            }
            status.pointee = .haveData
            return buffer
        }
        if let error {
            errorOut?.pointee = cString("converting \(url.lastPathComponent): \(error)")
            return false
        }
        if let channel = output.floatChannelData, output.frameLength > 0 {
            onChunk(context, channel[0], Int(output.frameLength), expected)
            delivered += Int(output.frameLength)
        }
    }

    guard delivered > 0 else {
        errorOut?.pointee = cString("\(url.lastPathComponent) decoded to no audio")
        return false
    }
    return true
}
