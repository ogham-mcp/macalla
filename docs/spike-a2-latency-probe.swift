import Foundation
import Speech
import AVFoundation
import CoreMedia

// Spike A part 2: feed fixture audio through SpeechAnalyzer at REAL-TIME pace
// and timestamp every result, so latency means what it means during dictation.
// Feeding faster than real time would measure throughput, not responsiveness.

struct Observed {
    let wallMs: Double          // ms since we began feeding
    let rangeStart: Double      // audio seconds
    let rangeEnd: Double
    let finalizationTime: Double
    let text: String
    let isFinal: Bool
}

func seconds(_ t: CMTime) -> Double {
    t.isNumeric ? CMTimeGetSeconds(t) : Double.nan
}

@main
struct SpikeA2 {
    static func main() async {
        let files = CommandLine.arguments.dropFirst()
        guard !files.isEmpty else { print("usage: spikea2 <file.aiff>..."); return }
        for path in files { await run(path) }
    }

    static func run(_ path: String) async {
        print("\n=== \(URL(fileURLWithPath: path).lastPathComponent) ===")
        guard let file = try? AVAudioFile(forReading: URL(fileURLWithPath: path)) else {
            print("could not open"); return
        }
        let srcFormat = file.processingFormat
        let durationS = Double(file.length) / srcFormat.sampleRate
        print(String(format: "audio: %.2f s @ %.0f Hz", durationS, srcFormat.sampleRate))

        let transcriber = SpeechTranscriber(
            locale: Locale(identifier: "en-US"),
            transcriptionOptions: [],
            reportingOptions: [.volatileResults],
            attributeOptions: [])

        guard let analyzerFormat = await SpeechAnalyzer.bestAvailableAudioFormat(
                compatibleWith: [transcriber]) else {
            print("no compatible audio format"); return
        }
        guard let converter = AVAudioConverter(from: srcFormat, to: analyzerFormat) else {
            print("no converter \(srcFormat) -> \(analyzerFormat)"); return
        }

        let analyzer = SpeechAnalyzer(modules: [transcriber])
        try? await analyzer.prepareToAnalyze(in: analyzerFormat)

        let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
        let start = Date()

        // Collector: drain results as they arrive, timestamping each.
        let collector = Task { () -> [Observed] in
            var seen: [Observed] = []
            do {
                for try await r in transcriber.results {
                    let fin = seconds(r.resultsFinalizationTime)
                    let end = seconds(r.range.end)
                    seen.append(Observed(
                        wallMs: Date().timeIntervalSince(start) * 1000,
                        rangeStart: seconds(r.range.start),
                        rangeEnd: end,
                        finalizationTime: fin,
                        text: String(r.text.characters),
                        isFinal: fin >= end - 0.001))
                }
            } catch { print("results error: \(error)") }
            return seen
        }

        try? await analyzer.start(inputSequence: stream)

        // Feed in 100 ms slices, sleeping between them so the analyzer sees
        // audio arriving at the rate a speaker would produce it.
        let sliceFrames = AVAudioFrameCount(srcFormat.sampleRate / 10)
        var fedFrames: AVAudioFramePosition = 0
        while fedFrames < file.length {
            guard let inBuf = AVAudioPCMBuffer(pcmFormat: srcFormat, frameCapacity: sliceFrames)
            else { break }
            do { try file.read(into: inBuf, frameCount: sliceFrames) } catch { break }
            if inBuf.frameLength == 0 { break }

            let ratio = analyzerFormat.sampleRate / srcFormat.sampleRate
            let outCap = AVAudioFrameCount(Double(inBuf.frameLength) * ratio) + 1024
            guard let outBuf = AVAudioPCMBuffer(pcmFormat: analyzerFormat, frameCapacity: outCap)
            else { break }
            var err: NSError?
            var supplied = false
            converter.convert(to: outBuf, error: &err) { _, status in
                if supplied { status.pointee = .noDataNow; return nil }
                supplied = true; status.pointee = .haveData; return inBuf
            }
            if err == nil, outBuf.frameLength > 0 {
                continuation.yield(AnalyzerInput(buffer: outBuf))
            }
            fedFrames += AVAudioFramePosition(inBuf.frameLength)
            try? await Task.sleep(nanoseconds: 100_000_000)
        }

        let fedAtMs = Date().timeIntervalSince(start) * 1000
        continuation.finish()
        try? await analyzer.finalizeAndFinishThroughEndOfInput()
        let results = await collector.value

        print(String(format: "fed all audio by %.0f ms wall\n", fedAtMs))
        print("  wall_ms  audio_range      kind      text")
        for o in results {
            print(String(format: "  %7.0f  %5.2f-%5.2f s   %-8@  %@",
                         o.wallMs, o.rangeStart, o.rangeEnd,
                         o.isFinal ? "FINAL" : "volatile" as NSString,
                         o.text.trimmingCharacters(in: .whitespaces) as NSString))
        }

        // Responsiveness: for each result, how far behind the audio it covers
        // did it arrive? Feeding is real-time, so wall time and audio time are
        // directly comparable.
        if let firstPartial = results.first {
            print(String(format: "\nfirst result:        %.0f ms wall, covering audio to %.2f s -> %.0f ms behind",
                         firstPartial.wallMs, firstPartial.rangeEnd,
                         firstPartial.wallMs - firstPartial.rangeEnd * 1000))
        }
        let finals = results.filter(\.isFinal)
        print("finalised segments:  \(finals.count)")
        for f in finals {
            print(String(format: "  final covering %5.2f-%5.2f s arrived %.0f ms wall -> %.0f ms after that audio",
                         f.rangeStart, f.rangeEnd, f.wallMs, f.wallMs - f.rangeEnd * 1000))
        }
        if let last = finals.last {
            print(String(format: "trailing finalisation: %.0f ms after end of speech",
                         last.wallMs - durationS * 1000))
        }
    }
}
