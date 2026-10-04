# Models

Every model Macalla can use, its licence, and the obligations that come with
it. A model is not added here until its licence has been read.

## Speech to text

In the app: Apple SpeechTranscriber, Parakeet TDT 0.6B v3, Nemotron 3.5 ASR
streaming 0.6B, and Whisper small.en, medium.en and medium. Every other row
was evaluated, or is a candidate, and is not offered.

| Model | Runtime | Licence | Obligation | Status |
|---|---|---|---|---|
| **Apple SpeechTranscriber** | `Speech.framework` | Apple system component | none beyond the OS licence | macOS default |
| **Parakeet TDT 0.6B v3** | transcribe.cpp, `parakeet-tdt-0.6b-v3-Q8_0.gguf` from handy-computer | **CC-BY-4.0** | **Attribution required** in About and here | default off macOS |
| Whisper large-v3-turbo | transcribe.cpp | MIT | none | not in the app; a candidate for languages the others miss |
| Moonshine v2 | transcribe.cpp | MIT | none | not in the app yet; may come later for low-end hardware |
| Canary 180M Flash | transcribe.cpp | **CC-BY-4.0** | **Attribution required** | re-measured 2026-09-10 on transcribe.cpp, not taken: a 2.5 min file silently came back as 102 of 388 words, Spanish came back empty, clinical WER 0.203 (sherpa-onnx had measured 0.125); see `docs/benchmarks.md` |
| Whisper small.en | transcribe.cpp, `whisper-small.en-Q8_0.gguf` from handy-computer | **Apache-2.0**, as the Hugging Face cards for these files and for OpenAI's originals declare (OpenAI's GitHub repository is MIT) | Licence and notices kept with the model if it is redistributed; Macalla downloads the published file and credits OpenAI and handy-computer in About | evaluated 2026-09-10: English only, about 430 MB peak; with the dictionary as its initial prompt it wrote both technical fixtures exactly; chosen 2026-09-10 as the light English option off macOS. In the catalogue and selectable since 2026-09-10, with the dictionary as its prompt; see `docs/benchmarks.md` |
| Whisper medium.en | transcribe.cpp, `whisper-medium.en-Q8_0.gguf` from handy-computer | **Apache-2.0**, as the Hugging Face cards for these files and for OpenAI's originals declare (OpenAI's GitHub repository is MIT) | Licence and notices kept with the model if it is redistributed; Macalla downloads the published file and credits OpenAI and handy-computer in About | evaluated 2026-09-10: English only, about 1.1 GB peak; the most accurate English Whisper without a prompt; chosen 2026-09-10 as the accurate English option off macOS. In the catalogue and selectable since 2026-09-10, with the dictionary as its prompt |
| Whisper medium | transcribe.cpp, `whisper-medium-Q8_0.gguf` from handy-computer | **Apache-2.0**, as the Hugging Face cards for these files and for OpenAI's originals declare (OpenAI's GitHub repository is MIT) | Licence and notices kept with the model if it is redistributed; Macalla downloads the published file and credits OpenAI and handy-computer in About | evaluated 2026-09-10: multilingual, about 1.1 GB peak, covers languages Parakeet lacks; chosen 2026-09-10 as the universal option off macOS. In the catalogue and selectable since 2026-09-10, with the dictionary as its prompt |
| Whisper tiny.en, base.en | transcribe.cpp, from handy-computer | MIT | none | evaluated 2026-09-10, not in the app: 137–215 MB peak, the weakest accuracy without a prompt |
| Parakeet Realtime EOU 120M | transcribe.cpp | **NVIDIA Open Model License** | **Attribution required**: "Licensed by NVIDIA Corporation under the NVIDIA Open Model License" | not in the app; a candidate for live preview |
| Nemotron 3.5 ASR streaming 0.6B | transcribe.cpp, `nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf` from handy-computer | **OpenMDW-1.1** | Whoever distributes the model keeps a copy of the agreement and its origin notices with it; no conditions on output. Macalla downloads the published file rather than shipping it, and credits NVIDIA and handy-computer in About | evaluated and chosen 2026-09-10 as the streaming option off macOS, Parakeet staying the default: first text about 1.2 s after speech starts, accuracy level with Parakeet on the reference fixtures; see `docs/benchmarks.md`. In the catalogue and selectable on the Engine card since 2026-09-10 |
| Granite Speech 4.1 2B | transcribe.cpp, `granite-speech-4.1-2b-Q4_K_M.gguf` from handy-computer | **Apache-2.0** | Licence and NOTICE kept with the model if it is redistributed | evaluated 2026-09-10, not taken: peak footprint 2.2–2.5 GB at Q4_K_M, 4.7–6.8× Parakeet's compute, fails a 2.5 min file, no streaming, keyword biasing not reachable through transcribe.cpp; see `docs/benchmarks.md` |
| Canary 1B v2 | transcribe.cpp, `canary-1b-v2-Q8_0.gguf` from handy-computer | **CC-BY-4.0** | **Attribution required** | evaluated 2026-09-10, not taken: Parakeet's 25 languages and accuracy at 1.2–1.8× the compute and about 340 MB more memory; a 2.5 min file fails in one call; see `docs/benchmarks.md` |
| Qwen3-ASR 0.6B | transcribe.cpp, `Qwen3-ASR-0.6B-Q8_0.gguf` from handy-computer | **Apache-2.0** | Licence and NOTICE kept with the model if it is redistributed | evaluated 2026-09-10, not taken: best clinical WER measured (0.141), but nine of Parakeet's languages missing, about 590 MB more memory, no streaming through transcribe.cpp, and a 2.5 min file fails in one call; see `docs/benchmarks.md` |
| Voxtral Mini 4B Realtime | transcribe.cpp, `Voxtral-Mini-4B-Realtime-2602-Q4_K_M.gguf` from handy-computer | **Apache-2.0** | Licence and NOTICE kept with the model if it is redistributed | evaluated 2026-09-10: the most accurate engine measured on long speech and technical terms, and it streams. **A 2.83 GB download that needs 4.2–5.6 GB of memory**, above SPEC §7's default budget, and 13–23× Parakeet's one-shot compute. Not in the app yet; it could be offered as an opt-in large model, never a default. Until then, to use Voxtral, choose Mistral as the cloud transcription service, whose default model is `voxtral-mini-latest`, or OpenRouter, which lists Voxtral speech-to-text models (checked 2026-10-04); see `docs/benchmarks.md` |

The NVIDIA Open Model License is permissive and MIT-compatible: commercial use,
redistribution and derivative models are all granted royalty-free. The
obligations are an attribution notice, inclusion of the agreement when
redistributing the model itself, and not stripping safety guardrails without
substantially similar replacements. It does not propagate to users of the
model's *output*. Verified 2026-09-08 against the licence text at
nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license.

Apple SpeechTranscriber downloads and manages its own assets through the OS.
On the reference machine twelve locales — all English and German variants —
were already installed; Spanish and Italian are supported but require an
install request.

## Voice activity detection

| Model | Runtime | Licence | Obligation | Status |
|---|---|---|---|---|
| **earshot 1.2.2** | pure Rust, the `earshot` crate from pyke; its weights (39,940 bytes) are compiled into the crate | **MIT OR Apache-2.0** | The copyright and licence notice kept with copies, as for every other crate Macalla links | chosen 2026-09-10 as the speech detector on every platform. Its training data is not published: a question on its repository asking how it was trained was open, unanswered, on 2026-09-10 |
| Silero VAD v6 | ONNX, or a port to Rust of its safetensors weights | MIT | The copyright and licence notice kept with copies | evaluated 2026-09-10, not taken: it passed no noise and lost no word in the comparison in `docs/benchmarks.md`, as earshot did, but needs a 1.24 MB download or an ONNX runtime |

## Tier 1 enhancement

**No model runs inside Macalla** (since 2026-09-11). Enhancement uses Apple
Intelligence when it is available, an OpenAI-compatible endpoint the person
runs, or a cloud model with their own key. The llama.cpp rows below were
measured in process before Macalla stopped running models in process on
2026-09-11. They now say which model to run on an endpoint, not which model
Macalla ships.

| Model | Runtime | Licence | Status |
|---|---|---|---|
| FoundationModels | `FoundationModels.framework` | Apple system component | **first provider** wherever Apple Intelligence reports available |
| **Qwen2.5-0.5B-Instruct** **Q6_K** | llama.cpp | **Apache-2.0** | **first to suggest for an endpoint.** Q6 over Q4: +11 ms, +101 MB, and ten points of structural accuracy (0.85 vs 0.75) |
| **Granite 4.1 3B** Q4_K_M | llama.cpp | **Apache-2.0** | **quality option** for an endpoint (2230 MB) |
| Qwen2.5-1.5B-Instruct Q4_K_M | llama.cpp | Apache-2.0 | evaluated, not chosen |
| SmolLM2-1.7B-Instruct Q4_K_M | llama.cpp | Apache-2.0 | **passes every quality gate** — lowest content loss in the field; candidate "minimal intervention" option |
| Granite 3.1 1B a400m MoE | llama.cpp | Apache-2.0 | evaluated, **fails the rubric** |
| SmolLM3-3B Q4_K_M | llama.cpp | Apache-2.0 | evaluated, fails on content loss |
| Phi-4-mini-instruct Q4_K_M | llama.cpp | **MIT** | evaluated, fails on content loss. Most permissive licence tested |
| Qwen3-1.7B Q4_K_M | llama.cpp | Apache-2.0 | evaluated, fails four gates |
| Granite 4.2 3B | llama.cpp | Apache-2.0 | evaluated, **fails the rubric** (drops 23% of content) |

Suggested on measured results, not reputation — see `docs/benchmarks.md`. The
0.5B model was fastest at p50 and p95, smallest, best at preserving technical
terms, and permissively licensed.

### Configuration requirement: thinking off

Hybrid reasoning models must be run with thinking **disabled**
(`llama-server --reasoning off`). Left on, output lands in `reasoning_content`
and grammar-constrained results never reach the caller. Beyond that, a thinking
pass buys nothing for a mechanical rewrite and costs latency on the one path
where latency is the product.

This is a configuration rule, not an exclusion — Granite 4.2 returns valid
output once thinking is off. It fails for a separate reason: it drops 23% of
content, over double the rubric's threshold.

### Deliberately not evaluated

| Model | Licence | Why not |
|---|---|---|
| Gemma-3-1B | Gemma Terms of Use | Non-permissive custom licence with use restrictions. An Apache-2.0 model won on every axis, so this question stays closed. |
| Llama-3.2-1B | Llama 3.2 Community Licence | Non-permissive custom licence, including an acceptable-use policy and attribution requirements. Same reasoning. |

If either is ever reconsidered, the licence must be read in full and recorded
here **before** the model is benchmarked, not after.

## Never bundle

These may be usable through a vendor API but their weights must never ship with
Macalla, and must never be downloaded by it.

| Weights | Licence | Why |
|---|---|---|
| **Voxtral TTS** | CC BY-**NC** | Non-commercial only. Incompatible with MIT distribution. |
| **XTTS** | Coqui Public Model Licence | Non-commercial restrictions. |

## Reading, not copying

Macalla is MIT. Open-source dictation apps under MIT and Apache-2.0 are read
for architecture; code is not copied, and anything adapted carries a comment
naming the source URL.

With thanks to the projects whose ideas Macalla adopted, each named where it is
used:

| Project | Licence | Adopted |
|---|---|---|
| [Handy](https://github.com/cjpais/Handy) | MIT | Filler removal gated by language, failing closed on an unknown language; the shape of the universal filler list; a starting silence value for streaming models; plain-text fallback when a structured model call fails |
| [sebsto/wispr](https://github.com/sebsto/wispr) | Apache-2.0 | The French filler list and the unambiguous-sounds filler set; the observation that a deleted filler strands its punctuation |
| [Yap](https://github.com/FrigadeHQ/yap) | MIT | Read for architecture; nothing adopted |

**GPL-licensed projects are never read or copied from.** Reading a README for
feature ideas is fine; opening the source is not.

## Local disk hygiene

Bake-off models are fetched by `llama-server -hf` into
`~/.cache/huggingface/hub` and are **not** part of the repository. Rejected
candidates are deleted after evaluation and re-fetched on demand if ever
needed — the numbers live in `docs/benchmarks.md`, so nothing is lost by
removing the weights.

Kept locally: Qwen2.5-0.5B (379 MB), SmolLM2-1.7B (1.0 GB), Granite 4.1 3B
(2.0 GB) — the three that pass every quality gate.

Deleted after evaluation 2026-09-08, recovering 11 GB: Phi-4-mini,
Granite 4.2 3B, SmolLM3-3B, Granite 3.1 3B MoE, Qwen3-1.7B, Qwen2.5-1.5B,
Granite 3.1 1B MoE.
