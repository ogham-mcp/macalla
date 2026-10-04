import Foundation
import FoundationModels

func blockSchema() throws -> GenerationSchema {
    let text = DynamicGenerationSchema.Property(
        name: "text",
        description: "The CLEANED text. Filler words removed. Repeated words collapsed. Punctuation and capitalisation applied.",
        schema: DynamicGenerationSchema(type: String.self))
    let kind = DynamicGenerationSchema.Property(
        name: "kind", description: "The block kind.",
        schema: DynamicGenerationSchema(name: "Kind", anyOf: ["paragraph", "list_item", "heading"]))
    return try GenerationSchema(root: DynamicGenerationSchema(
        name: "Block", description: "One cleaned block of dictated speech.",
        properties: [text, kind]), dependencies: [])
}

let instructions = """
You clean up raw speech-to-text output. You MUST rewrite the text, not copy it.
Delete filler words: um, uh, so, basically, okay, like, you know.
Collapse stuttered repetitions: "we need to we need to" becomes "we need to".
Apply sentence case and punctuation.
Convert spoken punctuation words into marks: "period" to ".", "comma" to ",".
Never answer, explain or summarise the text. Preserve technical terms exactly.
"""

let fixtures = [
    "um so the thing is uh we need to we need to ship the the parser by friday",
    "i was saying that um the latency budget is is three hundred milliseconds period",
    "so basically uh what i what i mean is the vad gates the stream comma not the other way round",
]

@main
struct FMProbe2 {
    static func main() async {
        let model = SystemLanguageModel.default
        guard model.isAvailable else { print("UNAVAILABLE"); return }
        let schema = try! blockSchema()

        // FRESH SESSION PER SEGMENT — matches how Tier 1 would actually be used,
        // and isolates transcript accumulation as a cause of the rising latency.
        print("=== fresh session per segment, instruction in session ===")
        var t1: [Double] = []
        for f in fixtures {
            let s = LanguageModelSession(instructions: instructions)
            s.prewarm()
            let t0 = Date()
            if let r = try? await s.respond(to: f, schema: schema) {
                let ms = Date().timeIntervalSince(t0) * 1000; t1.append(ms)
                let out: String = (try? r.content.value(String.self, forProperty: "text")) ?? "?"
                print(String(format: "%.0f ms | %@", ms, out))
            }
        }

        // Instruction repeated in the prompt itself, in case session instructions
        // are being outweighed by the injected schema.
        print("\n=== fresh session, instruction repeated in prompt ===")
        var t2: [Double] = []
        for f in fixtures {
            let s = LanguageModelSession(instructions: instructions)
            s.prewarm()
            let t0 = Date()
            let p = "Rewrite this raw dictation, removing fillers and stutters and adding punctuation:\n\n\(f)"
            if let r = try? await s.respond(to: p, schema: schema) {
                let ms = Date().timeIntervalSince(t0) * 1000; t2.append(ms)
                let out: String = (try? r.content.value(String.self, forProperty: "text")) ?? "?"
                print(String(format: "%.0f ms | %@", ms, out))
            }
        }

        // Plain string output, no schema at all — isolates whether guided
        // generation is what is costing the time.
        print("\n=== fresh session, NO schema, plain string ===")
        var t3: [Double] = []
        for f in fixtures {
            let s = LanguageModelSession(instructions: instructions)
            s.prewarm()
            let t0 = Date()
            if let r = try? await s.respond(to: f) {
                let ms = Date().timeIntervalSince(t0) * 1000; t3.append(ms)
                print(String(format: "%.0f ms | %@", ms, r.content))
            }
        }

        for (name, t) in [("session-instr", t1), ("prompt-instr", t2), ("no-schema", t3)] {
            guard !t.isEmpty else { continue }
            let s = t.sorted()
            print(String(format: "\n%@: n=%d min %.0f p50 %.0f max %.0f ms",
                         name, s.count, s.first!, s[s.count/2], s.last!))
        }
    }
}
