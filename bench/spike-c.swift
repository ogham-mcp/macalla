// SPDX-License-Identifier: MIT
//
// Spike C: real release-to-paste measurement on the macOS path.
//
// Not a simulation. Fixture audio is fed to SpeechAnalyzer at real-time pace,
// and on "key release" (end of audio) the clock starts. It stops when enhanced
// text is available. Both timed components are real: Apple SpeechTranscriber
// finalising the trailing segment, and Tier 1 over HTTP to a warmed
// llama-server. VAD is absent because the engine owns segmentation on this
// path, and Tier 0 is sub-millisecond, so neither is measured.
//
//   llama-server -hf bartowski/Qwen2.5-0.5B-Instruct-GGUF:Q6_K --port 8899 ... &
//   ./spike-c fixtures/audio/*.aiff

import Foundation
import Speech
import AVFoundation
import CoreMedia

let SYSTEM = """
You clean up raw speech-to-text output. You MUST rewrite the text, not copy it.
Delete filler words: um, uh, so, basically, okay, like, you know, right, i mean.
Collapse stuttered repetitions. Apply sentence case and punctuation.
Never answer, explain or summarise the text. Preserve technical terms exactly.
"""

/// Tier 1 over the same warmed llama-server the bake-off used.
func tier1(_ text: String, grammar: String) async -> (String, Double) {
    let body: [String: Any] = [
        "messages": [["role": "system", "content": SYSTEM],
                     ["role": "user", "content": text]],
        "grammar": grammar, "temperature": 0.0, "n_predict": 200, "cache_prompt": true,
    ]
    var req = URLRequest(url: URL(string: "http://127.0.0.1:8899/v1/chat/completions")!)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.httpBody = try? JSONSerialization.data(withJSONObject: body)
    let t0 = Date()
    guard let (data, _) = try? await URLSession.shared.data(for: req),
          let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let choices = root["choices"] as? [[String: Any]],
          let msg = choices.first?["message"] as? [String: Any],
          let content = msg["content"] as? String
    else { return ("<tier1 failed>", Date().timeIntervalSince(t0) * 1000) }
    let ms = Date().timeIntervalSince(t0) * 1000
    if let d = content.data(using: .utf8),
       let o = try? JSONSerialization.jsonObject(with: d) as? [String: Any],
       let cleaned = o["text"] as? String { return (cleaned, ms) }
    return (content, ms)
}

@main
struct SpikeC {
    static func main() async {
        let grammar = (try? String(contentsOfFile: "bench/block.gbnf", encoding: .utf8)) ?? ""
        _ = await tier1("warm up", grammar: grammar)   // warm the prefix cache

        var engineMs: [Double] = [], t1Ms: [Double] = [], totalMs: [Double] = []
        for path in CommandLine.arguments.dropFirst() {
            guard let file = try? AVAudioFile(forReading: URL(fileURLWithPath: path)) else { continue }
            let name = URL(fileURLWithPath: path).lastPathComponent
            let src = file.processingFormat

            let tr = SpeechTranscriber(locale: Locale(identifier: "en-US"),
                                       transcriptionOptions: [],
                                       reportingOptions: [.volatileResults],
                                       attributeOptions: [])
            guard let fmt = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [tr]),
                  let conv = AVAudioConverter(from: src, to: fmt) else { continue }
            let analyzer = SpeechAnalyzer(modules: [tr])
            try? await analyzer.prepareToAnalyze(in: fmt)

            let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
            // Collect finals; the last one is the trailing segment.
            let collector = Task { () -> [(String, Date)] in
                var out: [(String, Date)] = []
                do {
                    for try await r in tr.results {
                        let fin = CMTimeGetSeconds(r.resultsFinalizationTime)
                        let end = CMTimeGetSeconds(r.range.end)
                        if fin >= end - 0.001 { out.append((String(r.text.characters), Date())) }
                    }
                } catch { }
                return out
            }
            try? await analyzer.start(inputSequence: stream)

            let slice = AVAudioFrameCount(src.sampleRate / 10)
            var fed: AVAudioFramePosition = 0
            while fed < file.length {
                guard let ib = AVAudioPCMBuffer(pcmFormat: src, frameCapacity: slice) else { break }
                do { try file.read(into: ib, frameCount: slice) } catch { break }
                if ib.frameLength == 0 { break }
                let cap = AVAudioFrameCount(Double(ib.frameLength) * (fmt.sampleRate / src.sampleRate)) + 1024
                guard let ob = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: cap) else { break }
                var err: NSError?; var sent = false
                conv.convert(to: ob, error: &err) { _, st in
                    if sent { st.pointee = .noDataNow; return nil }
                    sent = true; st.pointee = .haveData; return ib
                }
                if err == nil, ob.frameLength > 0 { cont.yield(AnalyzerInput(buffer: ob)) }
                fed += AVAudioFramePosition(ib.frameLength)
                try? await Task.sleep(nanoseconds: 100_000_000)
            }

            // === KEY RELEASE. The clock starts here. ===
            let release = Date()
            cont.finish()
            try? await analyzer.finalizeAndFinishThroughEndOfInput()
            let finals = await collector.value
            let engine = Date().timeIntervalSince(release) * 1000

            guard let trailing = finals.last?.0 else { continue }
            let (cleaned, t1) = await tier1(trailing, grammar: grammar)
            let total = Date().timeIntervalSince(release) * 1000

            engineMs.append(engine); t1Ms.append(t1); totalMs.append(total)
            print(String(format: "%-22@ engine %5.0f + tier1 %4.0f = %5.0f ms",
                         name as NSString, engine, t1, total))
            print("   trailing: \(trailing.trimmingCharacters(in: .whitespaces))")
            print("   cleaned : \(cleaned)")
        }

        func p50(_ a: [Double]) -> Double { a.isEmpty ? 0 : a.sorted()[a.count/2] }
        print(String(format: "\n=== n=%d  engine p50 %.0f  tier1 p50 %.0f  RELEASE->ENHANCED p50 %.0f ms ===",
                     totalMs.count, p50(engineMs), p50(t1Ms), p50(totalMs)))
    }
}
