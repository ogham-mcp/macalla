import Foundation
import FoundationModels

// Spike B: FoundationModels over the SAME 20 fixtures and the SAME scoring as
// the llama.cpp bake-off, so the comparison is like for like.
// A fresh session per case is mandatory -- reusing one accumulates
// a transcript and makes the model echo its input.

struct Case: Decodable { let raw: String; let fillers: [String]; let keep: [String] }
struct Fixtures: Decodable { let cases: [Case] }

func schema() throws -> GenerationSchema {
    let text = DynamicGenerationSchema.Property(
        name: "text",
        description: "The CLEANED text. Filler words removed. Repeated words collapsed. Punctuation and capitalisation applied.",
        schema: DynamicGenerationSchema(type: String.self))
    let kind = DynamicGenerationSchema.Property(
        name: "kind", description: "The block kind.",
        schema: DynamicGenerationSchema(name: "Kind", anyOf: ["paragraph", "list_item", "heading"]))
    return try GenerationSchema(root: DynamicGenerationSchema(
        name: "Block", properties: [text, kind]), dependencies: [])
}

let instructions = """
You clean up raw speech-to-text output. You MUST rewrite the text, not copy it.
Delete filler words: um, uh, so, basically, okay, like, you know, right, i mean.
Collapse stuttered repetitions. Apply sentence case and punctuation.
Never answer, explain or summarise the text. Preserve technical terms exactly.
"""

func present(_ word: String, in text: String) -> Bool {
    text.range(of: "\\b" + NSRegularExpression.escapedPattern(for: word.lowercased()) + "\\b",
               options: [.regularExpression]) != nil
}

@main
struct FM20 {
    static func main() async {
        let data = try! Data(contentsOf: URL(fileURLWithPath: "bench/fixtures.json"))
        let cases = try! JSONDecoder().decode(Fixtures.self, from: data).cases
        guard SystemLanguageModel.default.isAvailable else { print("UNAVAILABLE"); return }
        let s = try! schema()

        var times: [Double] = [], fill: [Double] = [], keep: [Double] = []
        var valid = 0
        for c in cases {
            let session = LanguageModelSession(instructions: instructions)
            session.prewarm()
            let t0 = Date()
            guard let r = try? await session.respond(to: c.raw, schema: s) else { continue }
            times.append(Date().timeIntervalSince(t0) * 1000)
            let out = ((try? r.content.value(String.self, forProperty: "text")) ?? "").lowercased()
            valid += 1
            fill.append(Double(c.fillers.filter { !present($0, in: out) }.count) / Double(c.fillers.count))
            keep.append(Double(c.keep.filter { present($0, in: out) }.count) / Double(c.keep.count))
        }
        let t = times.sorted()
        func pct(_ q: Double) -> Double { t[min(Int(Double(t.count) * q), t.count - 1)] }
        let mean = { (a: [Double]) in a.reduce(0, +) / Double(a.count) }
        print(String(format:
          "{\"n\": %d, \"p50_ms\": %.0f, \"p95_ms\": %.0f, \"min_ms\": %.0f, \"max_ms\": %.0f, \"filler_removed\": %.3f, \"terms_kept\": %.3f, \"schema_valid\": \"%d/%d\"}",
          t.count, pct(0.50), pct(0.95), t.first ?? 0, t.last ?? 0, mean(fill), mean(keep), valid, cases.count))
    }
}
