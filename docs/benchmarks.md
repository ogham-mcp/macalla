# Benchmarks and spike findings

Every figure here states the machine it was measured on. A number without its
hardware is not a result.

## Reference machine

| | |
|---|---|
| Model | Apple M1 Pro |
| Memory | 16 GB |
| OS | macOS 26.6.2 (build 25G83) |
| Apple Intelligence | **On** as of 2026-09-08. Spike A was run *before* it was enabled and re-run after; results identical either way. |
| Toolchain | Command Line Tools only — full Xcode **not** installed |
| Swift | 6.3.3, target `arm64-apple-macosx26.0` |
| Rust | 1.98.1 |

No x86 machine is available. No x86 figure is claimed anywhere in this
repository, the README, or on the landing page.

---

## Spike A — Apple SpeechTranscriber

Part 1 (availability and capability) run 2026-09-08. Part 2
(latency and segment boundaries) still outstanding — it needs audio through the
analyzer, which needs the workspace.

### Part 1 — availability, locales, assets

Probe: a standalone Swift executable calling `SpeechTranscriber.isAvailable`,
`.supportedLocales`, `.installedLocales` and `AssetInventory.status(forModules:)`.
Compiled with `swiftc -parse-as-library` against
`/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`.

**Result: the premise holds.**

```text
macOS: Version 26.6.2 (Build 25G83)
SpeechTranscriber.isAvailable: true
AssetInventory.status(en-US): supported
maximumReservedLocales: 5
```

#### 1. It works with Apple Intelligence off — confirmed on the machine

`isAvailable` is `true` and `AssetInventory.status` for `en-US` reports
`supported`, with Apple Intelligence switched off in System Settings. This was
the blocking assumption behind promoting Apple SpeechTranscriber to the Phase 1
default, and it is now verified rather than inferred from
documentation.

#### 2. 30 supported locales, not 10

Published summaries said roughly ten languages. The framework reports **30
locales** across 10 language families:

| Language | Locales |
|---|---|
| English | `en_AU` `en_CA` `en_GB` `en_IE` `en_IN` `en_NZ` `en_SG` `en_US` `en_ZA` |
| German | `de_AT` `de_CH` `de_DE` |
| Spanish | `es_CL` `es_ES` `es_MX` `es_US` |
| French | `fr_BE` `fr_CA` `fr_CH` `fr_FR` |
| Italian | `it_CH` `it_IT` |
| Portuguese | `pt_BR` `pt_PT` |
| Chinese | `zh_CN` `zh_HK` `zh_TW` |
| Cantonese | `yue_CN` |
| Japanese | `ja_JP` |
| Korean | `ko_KR` |

Regional variants matter here: `de_AT`/`de_CH` and `es_MX`/`es_CL` are separate
models, so the Tier 0 per-language tables should key on language, not
locale, or they will fragment.

#### 3. Twelve locales are already installed — German and English need no download

```text
installedLocales (12): de_AT, de_CH, de_DE, en_AU, en_CA, en_GB,
                       en_IE, en_IN, en_NZ, en_SG, en_US, en_ZA
```

English and German dictation work on this machine **today, with zero
download**. Spanish and Italian are supported but not installed, so they need
an `AssetInstallationRequest` first.

Consequence for onboarding: the claim "no Engine card on macOS, there
is nothing to download" is true for English and German and **false for Spanish
and Italian**. The Engine card cannot be dropped outright — it must appear when
the selected language is supported-but-not-installed, and drive
`AssetInventory.assetInstallationRequest(supporting:)`. Filed as a correction
against the onboarding design.

#### 4. `maximumReservedLocales: 5`

Locale assets are reserved against a per-app cap of five. A user switching
between more than five languages needs `AssetInventory.release(reservedLocale:)`
handling; this is not a case the pipeline can ignore.

#### 5. Full Xcode is **not** required to build against Speech.framework

The probe compiled and ran with Command Line Tools alone. `Speech.framework` and
its `arm64e-apple-macos.swiftinterface` are present in the CLT SDK, alongside a
`MacOSX26.sdk`.

This contradicts what the project's earlier notes asserted on 2026-09-08
("full Xcode and the macOS 26 SDK are a hard prerequisite for a local macOS
build"). They have been corrected.

**Scope of the claim, precisely.** Verified: `swiftc` builds a standalone
executable that imports `Speech` and calls `SpeechTranscriber` and
`AssetInventory`, under CLT only. **Not yet verified:** that a Swift static
library links cleanly into a Rust binary under CLT, and that Tauri's macOS app
bundling works without full Xcode. Those are Phase 1 questions. The prerequisite
is therefore downgraded from "required" to "not required for the framework
itself; unconfirmed for the full bridge and bundle".

### Part 2 — latency and segment boundaries

Probe: `docs/spike-a2-latency-probe.swift`. Fixture audio synthesised with
`say -v Daniel -r 175` (`fixtures/audio/*.aiff`) so the run is reproducible and
needs no microphone. Audio is fed to `SpeechAnalyzer` in 100 ms slices with a
100 ms sleep between them, so it arrives at roughly the rate a speaker
produces it — feeding faster would measure throughput, not responsiveness.

| Fixture | Audio | First result | Finals | Trailing finalisation |
|---|---|---|---|---|
| `three-sentences` | 9.37 s | 4158 ms wall, 159 ms behind its audio | 3 | **127 ms** (corrected, see below) |
| `run-on` | 7.15 s | 4166 ms wall, 167 ms behind | 1 | **190 ms** (corrected) |
| `short-burst` | 0.59 s | 666 ms wall, 74 ms behind | 1 | **73 ms** (corrected) |

#### 1. Finals land on sentence boundaries — the blocking question, answered yes

`three-sentences` produced exactly three finalised results, one per sentence,
contiguous and gapless:

```text
 4312 ms   0.00- 2.82 s  FINAL  The latency budget is 300 milliseconds.
 8205 ms   2.82- 5.88 s  FINAL  We enhance each segment while you are still speaking.
10011 ms   5.88- 9.36 s  FINAL  Only the trailing segment is in flight when you release the key.
```

These are better boundaries than VAD would produce: semantic rather than
acoustic. Per-segment enhancement can run on them directly.

**Silero VAD is confirmed redundant as a *segmenter* on this path.** It remains
required as a *gate* — to decide when to open and close the stream, and, once
cloud engines land, to cap billing and keep silence off the wire.

#### 2. But segmentation follows sentences, not time

`run-on` — 7.15 seconds without a sentence break — produced **one** final
covering the whole utterance. A speaker who does not pause gets no
segmentation, and therefore none of the per-segment concurrency the design
depends on. Long unpunctuated speech collapses to the sequential case.

This is a real limitation on the key feature, not a tuning problem. It needs
measuring against actual dictation before anything is claimed publicly.

#### 3. Nothing appears for the first ~4 seconds

For both longer fixtures the first result arrived at about 4.15 s wall,
covering audio 0.00–4.00 s. Once it arrives it is only ~160 ms behind the audio
it covers, and fills in token by token — but before that there is nothing at
all. Volatile range ends land at 4.00 s and 7.80 s, which looks like a fixed
analysis window of roughly four seconds.

Consequence for the overlay: live partial text will be blank for the first four
seconds of any dictation, leaving the waveform as the only feedback. That is a
UX decision to take deliberately, not a bug to fix.

#### 4. CORRECTED — the engine is fast; this measurement was wrong

**The figure originally published here, "~645 ms trailing finalisation", was a
measurement error.** It is retained in this heading rather than deleted because
it drove a spec change and a published write-up, and quietly replacing it would
be worse than showing it.

The probe computed finalisation as *final result wall time − audio duration*.
But the feed loop sleeps 100 ms between 100 ms slices **plus** the time to read,
convert and yield each one, so it runs about 5% slower than real time. Over nine
seconds of audio that accumulates half a second of overhead which was attributed
to the engine.

Re-derived from the same raw output already recorded above:

| Fixture | Audio | Fed by | Final at | vs audio end (reported) | **vs feed end (correct)** |
|---|---|---|---|---|---|
| three-sentences | 9.37 s | 9884 ms | 10011 ms | 641 ms | **127 ms** |
| run-on | 7.15 s | 7607 ms | 7797 ms | 647 ms | **190 ms** |
| short-burst | 0.59 s | 630 ms | 703 ms | 113 ms | **73 ms** |

Spike C, which starts its clock when feeding stops rather than at the audio's
nominal end, independently measured 64–202 ms across five fixtures. Two methods
agree.

**Apple SpeechTranscriber finalises the trailing segment in roughly 130 ms, not
645 ms.**

Two further claims built on the bad number are also retracted:

- *"It plateaus, so it is not the machine."* The apparent plateau was feed
  overhead growing with audio length, masking the real cost. Corrected, engine
  latency **does** scale with trailing-segment length: 73 ms for 0.59 s,
  127 ms for 3.48 s, 190 ms for 7.14 s.
- *"The engine consumes 645 ms before Tier 1 begins."* Repeated in several
  places as a reason latency headroom mattered. The real figure is ~130 ms, and
  **Tier 1 is the dominant cost, not the engine.**

The lesson is in `docs/findings.md`, Piece 1: end of *audio content* and end of
*input* are different instants, and the gap between them was our own harness.

#### 5. A dictionary fixture fell out for free

`run-on` transcribed "ship the **passer** by Friday" for "parser" — the exact
use case for the cross-engine dictionary, produced accidentally by the default
engine on the first run. Kept as a dictionary fixture.

### Part 3 — technical vocabulary and homophones

Fixtures written as the **spoken** form, with the **written** form as the
expected output — which is precisely the dictionary's job. Vocabulary drawn
from work actually done rather than invented: Terraform, Terragrunt, Azure,
AWS, VNet, HCL, kubectl, Postgres, pgvector, Supabase, Proxmox, LXC,
Tailscale, MCP, Ogham.

#### Correct with no help at all

`Terraform` · `HCL` · `AWS` · `Proxmox` · `LXC` · `MCP`

Worth knowing, because it means the dictionary does not need to carry them.

#### Eleven real dictionary cases

| Spoken | Engine produced | Should be |
|---|---|---|
| terra grunt | `Terra Grunt` | `Terragrunt` |
| azure | `azure` | `Azure` |
| V net | `V net` | `VNet` |
| kube control | `cube control` | `kubectl` |
| post gres | `postgress` | `Postgres` |
| P G vector | `PG vector` | `pgvector` |
| embeddings | `embodings` | `embeddings` |
| supa base | `Superbase` | `Supabase` |
| tail scale | `tail scale` | `Tailscale` |
| ogham | `Ogam` | `Ogham` |

Seeded into `fixtures/dictionary/seed.json`. The 0.1 exit criteria ask for ten seeded
technical terms fixed across engines; these are those
ten, measured rather than guessed.

Two are worth calling out. **`kube control` → `kubectl` is the exact example
the exit criteria already use** — it arrived on the first run without being sought.
And the engine cannot spell **`Ogham`**, the name of the project it belongs to.

#### Homophones — good, but the failures are all technical

Correct: `their`/`there` · `write`/`right` · `root`/`route` (all four in one
sentence) · `principal`/`principle` · `affect`/`effect` · `two`/`too` ·
`site`/`cite`.

Wrong:

| Spoken | Engine produced |
|---|---|
| Clear the **cache** before you count the cash | "Clear the **cash** before you count the cash" |
| The **queue** took its cue | "The **Q** took its cue" |
| failed to **sync** | "failed to **sink**" |

Every failure is a case where the technical sense lost to the everyday word.
"Clear the cache" is unambiguous to any developer and the engine still got it
wrong, because its language model's prior does not know what kind of document
this is.

**The dictionary cannot fix these, and it is important to be clear why.**
Phonetic post-correction maps a spoken form to a written one. Here both
spellings are ordinary English words: a rule rewriting `cash` → `cache` would
break "count the cash" in the very same sentence. This class of error is
outside the dictionary's scope.

What can address it:

- **Engine vocabulary biasing** — Apple accepts contextual strings, so
  supplying the user's dictionary terms as context may shift the prior. Worth
  testing; it is already the first dictionary layer in the design.
- **Tier 1** — the LLM sees the whole sentence and has the context the acoustic
  model lacks. This is a use for Tier 1 beyond tidying, and it argues for
  passing dictionary terms into the prompt as the design's third dictionary layer already
  requires.

Neither is verified. Both should be measured before any claim is made about
homophone handling.

#### Homographs test the wrong component

Homographs — `read`, `live`, `lead`, `record` — are spelled identically
whichever sense is meant, so an STT engine emits the same characters either
way and there is nothing to get wrong. They are a **text-to-speech** problem:
the TTS voice has to choose a pronunciation, and picking the wrong one is
audible and wrong. Homograph fixtures belong with the TTS work in the agent
interface, not here.

#### An unexpected normalisation

"They're going to deploy" came back as "**They are** going to deploy". The
engine expanded the contraction unprompted.

That conflicts with the rule that enhancement preserves the speaker's words:
here the *engine* changed them before any of our code ran. It is minor, but it
means Tier 0 and Tier 1 are not the only things rewriting the user, and the
seam-pass fixtures should not assume the transcript is verbatim.

---

#### Limits of this measurement

Fixtures are synthesised speech at a steady rate, not a person. Real dictation
has hesitation, varied prosody and background noise, all of which move sentence
boundaries. Feeding used 100 ms slices; a real capture path may use smaller
buffers. Finding 2 especially — that run-on speech does not segment — should be
re-measured against real dictation before it is relied on or published.

---

### Re-run with Apple Intelligence on (2026-09-08)

Identical: `isAvailable: true`, 30 supported locales, the same 12 installed,
`maximumReservedLocales: 5`. SpeechTranscriber is independent of Apple
Intelligence **in both directions** — enabling it neither unlocks nor changes
the speech path. The Part 1 result stands, and it stays a portability fact
worth keeping: users who cannot or will not enable Apple Intelligence still get
the default engine.

---

## Spike A2 — Parakeet TDT v3 via sherpa-onnx

Not started.

## Spike B — Tier 1 model bake-off

In progress. The llama.cpp shortlist is not yet run; what
follows is the **FoundationModels** candidate, newly available because Apple
Intelligence was enabled on 2026-09-08.

### FoundationModels as a Tier 1 backend

Probe: `docs/spike-b-fm-probe.swift`. Five and then three fixture utterances of
raw dictation, cleaned into a two-field schema (`text`, `kind ∈ {paragraph,
list_item, heading}`).

**Built without the `@Generable` macro.** The macro plugin
(`FoundationModelsMacros`) ships with Xcode and is absent from Command Line
Tools, so the probe uses the runtime `DynamicGenerationSchema` /
`GenerationSchema` API instead. This is not a workaround to be replaced later —
it is the API a Rust bridge has to use anyway, because Macalla's Block schema
lives in `macalla-core` and is only known at runtime. The macro is for schemas
fixed in Swift source, which ours is not.

#### A methodology error worth recording

The first run reused **one `LanguageModelSession` across all fixtures**. It
produced latencies climbing 4.0 s → 14.0 s and output that was a verbatim copy
of the input — no cleanup at all. Both were artefacts of the reused session:
`LanguageModelSession` accumulates a transcript, so each call re-processed a
longer context, and the growing transcript of unmodified input appears to have
taught the model to echo.

**A fresh session per segment is therefore mandatory, not a preference.** This
is a direct constraint on the Tier 1 implementation, and it is the opposite of
what the design assumed for the llama.cpp path, where a pre-warmed session with a
KV-cached prefix is the optimisation. The two backends want opposite lifecycle
handling.

#### Results — fresh session per segment

| Variant | min | **p50** | max |
|---|---|---|---|
| Guided (schema), instruction in session | 756 ms | **853 ms** | 7552 ms |
| Guided (schema), instruction repeated in prompt | 762 ms | **882 ms** | 1087 ms |
| Unguided, plain string out | 503 ms | **511 ms** | 681 ms |

The 7552 ms outlier is the first session created in the process — session
construction and first inference carry a one-off cost that pre-warming does not
fully hide.

**Guided generation costs roughly 340 ms** over plain string output (853 vs
511 p50). That is the price of schema conformance, and it is not negligible
against a 300 ms budget.

#### Quality — good

```text
in : um so the thing is uh we need to we need to ship the the parser by friday
out: We need to ship the parser by Friday.

in : i was saying that um the latency budget is is three hundred milliseconds period
out: The latency budget is three hundred milliseconds.

in : so basically uh what i what i mean is the vad gates the stream comma not the other way round
out: So basically what I mean is the VAD gates the stream, comma not the other way round.
```

Fillers removed, stuttered repetitions collapsed, sentence case and punctuation
applied, technical terms preserved (`VAD` correctly capitalised). This is the
Tier 1 job done properly.

One failure mode visible in the third example: spoken punctuation was
*converted and also left in place* — "the stream, comma not the other way".
Tier 0 handles spoken punctuation deterministically before Tier 1 ever sees it, so this is not Tier 1's job; but it confirms the ordering of the pipeline stages
matters and must not be inverted.

#### Verdict

FoundationModels is a **viable Tier 1 backend on macOS**: good quality, zero
model download, no model RAM against the memory budget, no licence to track.

It **does not meet the 300 ms release-to-paste budget on its own** — p50 853 ms
guided is 2.8× over. That is not a blocker, because the design already requires
that a Tier 1 result which misses the budget is dropped in favour of Tier 0
output with history back-filled. It does mean the trailing segment after
key-release will frequently fall back to Tier 0 on this backend.

#### Corroborated by use, not just by probe

KB, 2026-09-08, after enabling Apple Intelligence on this M1: there is
"a very bit of latency here compared to, say, sending the text directly out to
a cloud model and having it sent back — it's a little bit slow on an M1
machine. Still workable, but doable." The lived impression and the p50 agree,
which is the most confidence we can have in a number this early.

Two consequences follow, and neither is a problem for the architecture:

1. **A cloud round-trip may genuinely be faster than on-device here.** That is
   allowed and already designed for — Tier 2 is cloud enhancement,
   explicitly opt-in and off by default. What this changes
   is Tier 2's *value*: it is worth more, sooner, than its backlog position
   assumes. It does not change the default, which stays local.
2. **The 300 ms budget was never a per-utterance inference target.** It is the
   release-to-paste budget for the *trailing* segment only. Everything earlier
   is enhanced while the user is still speaking, so an 850 ms per-segment cost
   is hidden for all but the last segment. This is the load-bearing idea in
   the design, and this measurement is the first evidence that the project needs
   it rather than merely benefits from it.

**Target for the llama.cpp shortlist: beat 853 ms p50 with schema conformance
on this machine.** A 0.5B model at Q4 should be substantially faster than a
~3B system model, and that is now the central open question of Spike B — if
nothing clears ~300 ms, the trailing segment falls back to Tier 0 by design
rather than by failure, but the headline claim rests on how often that happens. If a 0.5–1.5B GGUF model cannot, FoundationModels wins on
macOS for zero download and zero RAM, and llama.cpp remains the engine
everywhere else.

### The Tier 1 rubric

The 0.1 exit criteria require that enhanced output beat Tier 0 "by a documented rubric".
This is that rubric. It is applied by `bench/tier1_full.py` over the 20
fixtures in `bench/fixtures.json`, mechanically — no model is judged by
eyeball, because eyeballing three samples already produced two wrong calls in
this document.

| Measure | What it catches | Threshold |
|---|---|---|
| `p50_ms` | typical per-segment cost | ≤ 300 ms to be viable as the **default** |
| `p95_ms` | the tail a user actually notices | ≤ 2× p50; a long tail is disqualifying |
| `RSS_MB` | memory against the 2.5 GB active-memory budget | ≤ 1 GB for the cross-platform default, since Parakeet also has to fit |
| `schema_valid` | constrained decoding actually holding | **20/20, no exceptions** |
| `terms_kept` | technical terms surviving verbatim | ≥ 0.95 |
| `content_dropped` | meaning silently lost | ≤ 0.10 |
| `invented` | words the user never said | ≤ 0.05 |
| `filler_removed` | *not* a quality gate — see below | reported, not thresholded |

**`filler_removed` is deliberately not a pass/fail gate.** Filler removal is
Tier 0's job: deterministic, from an editable list, running before
Tier 1 sees the text. What the column actually measures is whether a model
rewrites at all rather than echoing its input, which predicts behaviour on
what Tier 0 cannot do — sentence case, punctuation placement, structure. It is
diagnostic, not decisive.

**`content_dropped` and `invented` are the ones that matter**, and they were
added late, after this document already contained a model recommendation. The
original columns measured latency, schema validity and term retention and said
nothing about whether *meaning survived* — a model can score well on all three
while quietly dropping a clause or inventing one. `content_dropped` counts
non-filler input words absent from the output; `invented` counts output words
never spoken. Case and punctuation are normalised away, so neither penalises
Tier 1 for doing its job.

Two limits of this rubric, stated so nobody over-reads it:

- It cannot tell an *intentional* colloquialism from a speech error. That is a
  judgement call and no mechanical score reaches it.
- **It silently scores a reasoning model 0/20** rather than reporting the real
  cause — see Granite 4.2 below. The harness should detect an empty `content`
  with a populated `reasoning_content` and say so.

### The bake-off — llama.cpp shortlist versus FoundationModels

20 raw-dictation fixtures (`bench/fixtures.json`), same prompt, same two-field
schema, on the reference machine. llama.cpp models run under `llama-server`
with a GBNF grammar (`bench/block.gbnf`) and a warmed prefix cache, because the
app pre-warms one session — timing a fresh `llama-cli` per request would
measure model load, not inference. FoundationModels runs with a **fresh session
per case**, which is mandatory: a reused session degrades, as described below.

Quality is scored mechanically, not eyeballed:

- `filler_removed` — fraction of listed filler words absent from the output.
- `terms_kept` — fraction of listed technical terms still present.

A model that copies its input scores 0 and 1.0; one that rewrites freely scores
well then badly. Both need to be high.

| Model | Licence | p50 | p95 | RSS | filler | terms_kept | dropped | invented | schema |
|---|---|---|---|---|---|---|---|---|---|
| **Qwen2.5-0.5B-Instruct** Q4_K_M | Apache-2.0 | **278 ms** | **344 ms** | **538 MB** | 0.60 | **0.983** | 0.074 | 0.024 | 20/20 |
| Granite 3.1 1B a400m MoE | Apache-2.0 | 281 ms | 395 ms | 965 MB | 0.475 | 0.883 | **0.139** | **0.070** | 20/20 |
| SmolLM2-1.7B-Instruct Q4_K_M | Apache-2.0 | 421 ms | 532 ms | 1502 MB | 0.35 | 0.950 | **0.048** | 0.007 | 20/20 |
| Qwen2.5-1.5B-Instruct Q4_K_M | Apache-2.0 | 458 ms | 703 ms | 1115 MB | 0.45 | 0.950 | — | — | 20/20 |
| **Granite 4.1 3B** | Apache-2.0 | 730 ms | 813 ms | 2230 MB | **0.925** | 0.950 | 0.089 | **0.000** | 20/20 |
| Granite 4.2 3B | Apache-2.0 | — | — | 2398 MB | — | — | — | — | **0/20** |
| FoundationModels (Apple) | system | 787 ms | **9377 ms** | 0 (system) | 1.000 | 0.930 | — | — | 19/20 |

Llama-3.2-3B and Gemma were not tested: non-permissive licences, and permissive
models already cover both ends of the trade.

#### Granite 4.2: thinking must be off, and then it fails on a different gate

Its first run scored 0/20 because everything it produced landed in
`reasoning_content` while `content` came back empty — the grammar was
constraining the thinking channel:

```text
content   : ''
reasoning : '{ "text": "um so the thing is uh we need to...", "kind": "heading" }'
```

**That was a configuration problem, not a model class problem.** `llama-server
--reasoning off` fixes it, and the model then returns 20/20 valid schema. An
earlier draft of this document excluded reasoning models wholesale; that was
too strong and is retracted.

Re-run with thinking disabled:

| | p50 | p95 | RSS | filler | terms_kept | dropped | invented | schema |
|---|---|---|---|---|---|---|---|---|
| Granite 4.2 3B, `--reasoning off` | 813 ms | 922 ms | 2368 MB | **1.000** | 0.958 | **0.233** | 0.032 | 20/20 |

It removes **every** filler and keeps technical terms well — but drops
**23.3%** of content, more than double the 0.10 threshold. It is an
over-aggressive rewriter: it produces clean text that is missing pieces of what
was said.

Two things follow:

- **Thinking off is a hard configuration requirement for Tier 1**, not a
  reason to reject a model. Cleaning dictation is a mechanical rewrite; a
  thinking pass buys nothing and costs latency on the one path where latency is
  the product. Any hybrid model must be run with reasoning disabled.
- **`content_dropped` earned its place.** Without it, Granite 4.2 looks like the
  best model in the field — perfect filler removal, good term retention, low
  invention, valid schema throughout. The gate that caught it is the one added
  last, after the field had already been "decided" once.

The harness still has a blind spot worth fixing: it reported the first run as a
zero score rather than naming the cause, which invites the conclusion that a
capable model is broken.

#### What the new candidates actually showed

**Granite 3.1 1B MoE — the efficiency claim held, the quality claim did not.**
It matched Qwen2.5-0.5B on speed (281 ms vs 278 ms) despite nominally being a
larger model, which is exactly what a 400M-active MoE should do. But it was the
**worst local model on every quality measure**: `terms_kept` 0.883 (below the
0.95 threshold), `content_dropped` 0.139 and `invented` 0.070 — both over
threshold. It fails the rubric.

**Granite 4.1 3B — the hallucination claim verified.** `invented` came in at
**0.000**, the only model to achieve it, with the best filler removal of any
local model by a wide margin (0.925 against Qwen's 0.60). IBM's
"enterprise-grade guardrails, avoids making up text" reputation is, on this
task, measurably real.

Its costs are latency (730 ms p50) and memory (2230 MB, which alone nearly
exhausts the 2.5 GB active-memory budget and leaves no room for Parakeet).

#### This partially retracts an earlier claim in this document

The Spike B write-up said the low filler-removal scores were "more likely a
prompt limitation than a model ceiling". Granite 4.1 3B reached 0.925 **on the
identical prompt** that gave Qwen2.5-0.5B 0.60. So a real capability gap
exists, and prompt tuning alone will not close it. Prompt tuning remains
worthwhile; it is no longer the whole explanation.

#### The wider open-weight field

Tested on 2026-09-08 after the first decision, restricted to permissive
licences. All run with `--reasoning off`.

| Model | Licence | p50 | p95 | RSS | filler | terms | dropped | invented | schema |
|---|---|---|---|---|---|---|---|---|---|---|
| Qwen3-1.7B | Apache-2.0 | 468 | 2758 | 1473 MB | 0.925 | 0.800 | 0.331 | 0.000 | 16/20 |
| SmolLM3-3B | Apache-2.0 | 730 | 847 | 2134 MB | 0.950 | 0.983 | 0.142 | 0.038 | 20/20 |
| Phi-4-mini-instruct | **MIT** | 802 | 1016 | 2717 MB | 1.000 | 0.983 | 0.141 | 0.017 | 20/20 |

**Qwen3-1.7B fails four gates.** Despite being the newer generation of the
family that supplies our default, it drops a third of the content, keeps only
0.800 of technical terms, produced invalid schema on 4 of 20 cases, and has a
2758 ms tail. Newer is not better here.

**Phi-4-mini is the most permissively licensed model tested** — MIT, more open
than Apache-2.0 — and ties for best technical-term retention (0.983). It fails
on content: 0.141 dropped against a 0.10 threshold, at 2717 MB and 802 ms.
Worth revisiting if the rubric's content threshold is ever relaxed, and worth
remembering for its licence.

**SmolLM3-3B** is the same story as its 3B peers: excellent term retention,
too much content loss.

#### The field is one axis, and one model sits off it

Ordering everything tested by how aggressively it removes fillers shows the
trade almost perfectly:

| filler_removed | content_dropped | Model |
|---|---|---|
| 0.35 | **0.048** | SmolLM2-1.7B |
| 0.60 | 0.074 | Qwen2.5-0.5B |
| **0.925** | **0.089** | **Granite 4.1 3B** |
| 0.950 | 0.142 | SmolLM3-3B |
| 1.000 | 0.141 | Phi-4-mini |
| 1.000 | 0.233 | Granite 4.2 3B |

Cleanup aggression and content loss move together: a model that strips more
fillers strips more of everything. This is not a set of independent quality
scores, it is a single dial, and picking a model is mostly picking a position
on it.

**Granite 4.1 3B is the one model off the curve.** It removes 0.925 of fillers
— the aggression of a 3B rewriter — while dropping only 0.089 and inventing
*nothing*. Every other model at that aggression level loses 14% or more. That,
rather than any single column, is why it holds the quality slot.

Granite 3.1 1B MoE is off the curve in the other direction: low aggression
(0.475) with high loss (0.139), which is the worst combination available.

#### Three models pass every quality gate

Not one. `terms_kept` ≥ 0.95, `content_dropped` ≤ 0.10, `invented` ≤ 0.05,
schema 20/20:

| Model | Position on the dial | Use |
|---|---|---|
| **Qwen2.5-0.5B** | moderate | **default** — the only one also meeting p50 ≤ 300 ms |
| **SmolLM2-1.7B** | most conservative — **lowest content loss in the entire field** (0.048) and near-zero invention (0.007) | a "minimal intervention" option worth offering |
| **Granite 4.1 3B** | most aggressive without the usual cost | **quality option** |

SmolLM2-1.7B was under-sold in the first write-up, which judged it on
`filler_removed` — the one column that is explicitly not a gate. On the
measures that are gates it is the safest model tested.

#### Revised decision

**Default stays Qwen2.5-0.5B-Instruct Q4_K_M.** It is the only model that meets
the p50 ≤ 300 ms threshold, it passes every quality gate in the rubric, and at
538 MB it leaves room for Parakeet within the memory budget. Given Spike A part 2
showed the engine finalising in ~130 ms and Spike C put Tier 1 at 302 ms of a
441 ms total, the model is the dominant cost and its latency is where the
headroom has to come from.

**Granite 4.1 3B replaces FoundationModels as the recommended quality option.**
It is strictly better on the numbers that matter: 730 ms vs 787 ms p50, an
813 ms tail against 9377 ms, zero invented words, and 20/20 schema validity
against 19/20. FoundationModels retains exactly one advantage — it costs no
memory, because it is the OS's model. Granite 4.1 3B's 2230 MB confines it to
macOS, where the Apple engine holds no model RAM of its own.

So the shape is: fast and permissive by default, accurate on request, and the
system model as a zero-memory fallback.

#### Two corrections to earlier readings in this document

**A three-sample eyeball called Qwen2.5-0.5B "too weak".** It was wrong. Over
20 cases the 0.5B model is the fastest, the smallest, and the best at
preserving technical terms. Parameter count did not predict quality here:
Qwen2.5-1.5B is both slower *and* worse than the 0.5B on every measure.

**The first scoring run used substring matching**, so "so" matched inside
"also" and successful removals were counted as failures. Fixed to word
boundaries before any of the numbers above were taken.

#### What `filler_removed` actually measures — and why it is not decisive

Filler removal is **Tier 0's job**, not Tier 1's. The design gives Tier 0
deterministic filler removal from an editable list, running *before* Tier 1
ever sees the text. So a low `filler_removed` score does not mean fillers reach
the user.

What the metric really measures is whether the model *rewrites at all* rather
than echoing its input — instruction-following propensity. That still matters,
because it predicts behaviour on the things Tier 0 cannot do: sentence case,
punctuation placement, and structure. But it must not be read as a quality
verdict on its own.

Judged on what Tier 1 uniquely contributes, `terms_kept` is the more important
column, and the 0.5B model leads it.

#### FoundationModels' tail latency is disqualifying for the live path

Its cleanup quality is decisively the best — it removed **every** filler in
every case, which no local model came close to. But:

- **p95 of 9377 ms.** One dictation in twenty would stall for nine seconds.
- p50 of 787 ms is 2.75× the 0.5B model.
- It failed one case outright (19/20).
- It retains the fewest technical terms (0.930) — the flip side of rewriting
  most aggressively, and the exact failure mode the >40% change guard exists
  to catch.

The tail comes from the mandatory fresh session per segment. There is no
tuning available: a reused session was measured degrading to 14 s and echoing
its input.

#### Decision — Qwen2.5-0.5B-Instruct Q4_K_M

Fastest p50, best p95 by a factor of 22, smallest resident set by a wide
margin, best technical-term retention, valid schema on every case, and
**Apache-2.0**.

At 527 MB it leaves substantial headroom under the 2.5 GB active-memory budget —
which matters because Spike C showed Tier 1 is the dominant cost at 302 ms of a
441 ms release-to-enhanced total.

**Gemma-3-1B and Llama-3.2-1B were deliberately not tested.** Both carry
non-permissive custom licences, and adding a
dependency under one needs a deliberate decision. An Apache-2.0 model won on every axis, so the licence
question stays closed rather than being opened and then argued.

FoundationModels remains available as an alternative backend —
its quality advantage is real, and a user who prefers it and tolerates the tail
should be able to choose it. It is not the default.

#### The prompt is an untuned lever

Every model got the same prompt, and the best filler removal among local models
was 0.60. That is more likely a prompt limitation than a model ceiling, and
the prompt is already user-editable with a reset. Prompt tuning
should be attempted before any model is judged inadequate.

## Quantisation: does 6-bit buy anything over 4-bit?

Prompted by another app shipping a 6-bit quantisation where we ship 4-bit.
Same 20 fixtures, same prompt, temperature 0, grammar-constrained.

### Qwen3.5-4B — the prose is the same, the structure is not

| Quant | p50 | p95 | RSS | filler | terms | dropped | invented | schema |
|---|---|---|---|---|---|---|---|---|
| Q4_K_M | 1169 ms | 1355 ms | 4072 MB | 1.000 | 0.908 | 0.182 | 0.039 | 20/20 |
| Q6_K | 1290 ms | 1418 ms | 4695 MB | 1.000 | 0.908 | 0.182 | 0.039 | 20/20 |

Every quality column is **identical to three decimal places**, which was
suspicious enough to check directly. Diffing the raw outputs:

- The **`text` field differs on 1 of 20** — and it is a capitalisation call,
  `VNet` (Q4, correct) against `vNet` (Q6).
- The **`kind` field differs on 5 of 20**, and Q6 is right every time. Q4
  labelled plain sentences as `heading` or `list_item`.

So the metrics were identical because **they only scored the `text` field.**
Quantisation barely touched the prose and measurably degraded the structural
classification.

That makes sense mechanically: the cleaned text is many tokens, each heavily
constrained by grammar and context, so lower precision rarely flips one. The
block type is a **single token chosen from three grammar-permitted
alternatives** — a low-confidence decision where quantisation noise decides the
outcome.

Qwen3.5-4B fails the rubric regardless (`content_dropped` 0.182, `terms_kept`
0.908, 4 GB, 1169 ms), so it is not a candidate. The quantisation finding is
what the run was for.

### The same test on the model we actually ship

| Qwen2.5-0.5B | p50 | p95 | RSS | terms | dropped | invented | **kind_correct** | schema |
|---|---|---|---|---|---|---|---|---|
| Q4_K_M | 292 ms | 344 ms | 577 MB | 0.983 | 0.074 | 0.024 | **0.75** | 20/20 |
| **Q6_K** | 303 ms | 403 ms | 678 MB | 0.983 | 0.079 | 0.019 | **0.85** | 20/20 |

Every fixture is a plain sentence, so `paragraph` is correct for all twenty —
ground truth the rubric had not been using.

**Q6_K costs 11 ms at p50, 59 ms at p95, and 101 MB. It buys ten percentage
points of structural accuracy.** At Q4 one dictation in five gets the wrong
block type; at Q6 it is closer to one in seven. A spurious heading is far more
visible to a user than 11 ms of latency, and 678 MB still leaves most of the
memory budget.

Caveats: n=20 on one machine, and Q6's 303 ms p50 sits marginally over the
300 ms threshold — within noise, but stated rather than rounded away.

### The finding underneath the finding

Even at Q6, **three dictations in twenty get the wrong block type**. That is
not a quantisation problem or a model problem. It is an architecture problem:

**Tier 1 should not be classifying block type at all.** The design already gives
Tier 0 spoken structure commands (`new list`, `heading`, `bullet`) and list
cadence detection — deterministic, sub-millisecond, and *correct by
construction* because it responds to what the user actually said. Asking Tier 1
to re-derive the block type from the text is redundant work on the latency-
critical path, and it introduces an error the deterministic layer had already
eliminated.

The fix is not more bits or a bigger model. It is narrowing Tier 1's contract:
it returns cleaned **text**, and the block type stays whatever Tier 0
determined. That removes an entire failure class and shrinks the grammar.

## Spike C — end-to-end release to enhanced text

Probe: `bench/spike-c.swift`. **Not a simulation.** Fixture audio is fed to
`SpeechAnalyzer` at real-time pace; on key release — the instant feeding stops —
the clock starts, and stops when enhanced text is available. Both timed
components are real: Apple SpeechTranscriber finalising the trailing segment,
and Tier 1 (Qwen2.5-0.5B Q6_K) over HTTP to a warmed `llama-server`. VAD is
absent because the engine owns segmentation on this path, and Tier 0 is
sub-millisecond.

| Fixture | Engine | Tier 1 | **Release → enhanced** |
|---|---|---|---|
| short-burst | 64 ms | 252 ms | **315 ms** |
| homophones | 116 ms | 302 ms | **417 ms** |
| tech-terms | 167 ms | 274 ms | **441 ms** |
| three-sentences | 127 ms | 381 ms | **508 ms** |
| run-on | 202 ms | 537 ms | **739 ms** |
| **p50** | **127 ms** | **302 ms** | **441 ms** |

### Tier 1 is the dominant cost, not the engine

302 ms of a 441 ms p50 is the language model; the engine is 127 ms. That is the
opposite of what this document said before the correction in Spike A part 2,
and it moves where optimisation effort belongs.

### The 300 ms budget is reachable, and it is a product decision

Release → enhanced is 441 ms p50, over budget by 141 ms. But the design already
specifies the answer: if Tier 1 has not returned within budget, paste Tier 0
output and back-fill history.

Taking that path, **release → paste is ~130 ms** — the engine alone, since
Tier 0 and the dictionary are sub-millisecond.

| Path | Release → paste | Text quality |
|---|---|---|
| Tier 0 fallback | **~130 ms** | deterministic cleanup only |
| Wait for Tier 1 | **~441 ms** | full cleanup |

Both sit far below the multi-second pause of a record-then-transcribe app. The
choice is a product one, and it should turn on how good Tier 1's output
actually is.

### Tier 1 damaged the text on 3 of 5 fixtures

This is the first time Tier 1 output has been read in context rather than
scored, and it is not reassuring:

```text
trailing: ...ship the passer by Friday...
cleaned : ...ship the passers by Friday...       <- invented a plural

trailing: We deployed to AWS as well, and cube control showed the pods running.
cleaned : We deployed to AWS, and the cube control showed the pods running.
                             ^ dropped "as well"    ^ inserted "the"
```

`content_dropped` 0.079 and `invented` 0.019 both pass the rubric, and the
output is still visibly worse than its input on real sentences. Two
consequences:

- **Waiting 441 ms to make the text worse is a bad trade.** The Tier 0 fallback
  may be the better default until Tier 1's prompt is tuned.
- The rubric's thresholds are probably too loose. They were set before any
  end-to-end output had been read in context.

### Caveats

n=5, one machine, synthesised speech, one Tier 1 model. The trailing segment is
a full sentence here because the engine segments on sentences; a different
engine changes both columns.

## Tier 0 versus Tier 1 — the comparison nothing had run

The 0.1 exit criteria make "enhanced output beats Tier 0 by a documented rubric" a
requirement. Every measurement before this compared **models to each other**, or
Tier 1 to the **raw transcript**. Nothing had asked whether the language model
beats deterministic cleanup — the question that decides whether Tier 1 earns
302 ms on the latency-critical path.

Tier 0 is now implemented (`crates/macalla-core/src/tier0/`), so it can be.
Same 20 fixtures, same scoring.

| | Tier 0 | Tier 1 (Qwen2.5-0.5B Q6_K) |
|---|---|---|
| `content_dropped` | **0.069** | 0.083 |
| `invented` | **0.000** | 0.019 |
| strictly worse than the other | — | **4 of 20** |

**Tier 0 wins both columns, and invents nothing at all** — as it structurally
cannot, being a token filter with no generative step.

### What Tier 1 does to the text

```text
src: um the postgres database uses pgvector for the embeddings you know
T0 : The postgres database uses pgvector for the embeddings you know
T1 : The PostgreSQL database uses pgvector for the embeddings.

src: right so we need to we need to measure the p ninety five not just the median
T0 : We need to measure the p ninety five not just the median
T1 : Right, we need to measure the p value ninety-five not just the median.

src: uh the clipboard has to be restored after we we paste the text
T0 : The clipboard has to be restored after we paste the text
T1 : The clipboard needs to be restored after we paste the text.
```

`postgres` became `PostgreSQL` — Tier 1 rewrote a technical term, which is the
dictionary's job and the one thing it exists to get right. `p ninety five`
gained an invented "value". `has to be` became `needs to be` for no reason.

**Tier 1 is rewriting when it should be punctuating.** That is a prompt problem
before it is a model problem, and the prompt has never been tuned.

### Read fairly, Tier 1 does add something

These metrics only measure deletion and invention. Tier 1 supplies terminal
punctuation and commas the speaker never said, which Tier 0 deliberately will
not — Tier 0 only inserts punctuation that was actually spoken. That is a real
contribution and this comparison does not credit it.

The honest conclusion is narrower than "Tier 1 loses": **Tier 1 as currently
prompted is not worth 302 ms, because it damages more than it adds.** Tuning
the prompt to forbid rewriting is the next experiment, not replacing the model.

### Consequence for the default

Until that is settled, the **Tier 0 fallback path is the better default**:
~130 ms release-to-paste, zero invention, and no rewriting of technical terms.

---

## What an open-source dictation app does about this

The app is Apache-2.0, whose licence permits reading it for architecture.

**It has no cleanup prompt.** There is no language model in its cleanup
path at all — `Sources/WisprApp/Utilities/FillerWordCleaner.swift` (sebsto/wispr,
Apache-2.0) is deterministic regex filler removal, and that is the whole of it. A
shipping dictation app reached the same conclusion this comparison just did.

Two things were adopted from reading that file, both observations rather than code:

**Its filler list is unambiguous sounds only** — `um, uh, ah, oh, eh, er,
erm, hmm, hm, mhm, uh-huh` plus French equivalents. No "so", "basically",
"like", "okay". That independently validates the `always` / `sentence_initial`
split here: it does not attempt the ambiguous words either. Our `always` list
was extended to match it.

**A deleted filler strands its punctuation.** "Well, um, I think" becomes
"Well, , I think" unless the separator is handled. It solves it with two regex
passes; this is a token-stream implementation of the same observation, with the
source cited in the code. Fixed and tested.

## What other open-source dictation apps do — all three ship deterministic filler removal

Both others are MIT, so their licence permits reading them for architecture.

**Nobody relies on the model to clean up the text.** All three run deterministic
filler removal, and the two that also offer a language model
run it *on top* of that, never instead of it.

### Another open-source app

Twelve fillers, whole-word case-insensitive regex, plus sentence
re-capitalisation so removing a leading "Um" does not leave a lowercase start.

Two details worth having:

- Its source notes that its model-based cleanup removes fillers too, and that
  this deterministic pass runs even when that is off. Deterministic cleanup is
  the floor, not the fallback.
- **If nothing matched, it returns the input byte-for-byte** rather than
  re-rendering it, so a filler-free dictation is never re-spaced or
  re-capitalised. A cheap way to guarantee the cleanup layer cannot damage text
  it had no reason to touch.

### A third open-source app

The most careful of the three, and better than ours was on one axis. The split
described below was adopted from `src-tauri/src/audio_toolkit/text.rs` (Handy,
MIT).

It splits fillers by **cross-language safety**, not just by ambiguity within a
language:

- `UNIVERSAL_FILLER_WORDS` — tokens that are not lexical words in any language
  their models output, including Cyrillic `хм`, `ммм`.
- `gated_filler_words_for_language` — only removed with evidence for the output
  language, because *the same token is a real word elsewhere*: **Portuguese
  "um" is "a/an", German "um" is "at/around", Spanish "ha" is "has".**

And an `OutputLanguageEvidence` hierarchy — user-selected, model-constrained,
model-detected, text-detected — that **fails closed**: unknown language means
filler removal is skipped rather than applied speculatively.

It also has optional LLM post-processing (bring-your-own
key, provider and prompt selectable) as a separate opt-in layer above the
deterministic one. Same two-tier shape as ours.

### This found a real bug in our Tier 0

Our filler split was ambiguity *within* a language. Its split is ambiguity
*across* languages, and we had neither the split nor the fail-closed default:
`Rules::for_language(None)` returned **English**, so a dictation with no
language evidence would have stripped "um" from Portuguese text where it means
"a/an".

Fixed. Tier 0 now carries three lists on two axes — `always` (safe in any
language), `language_gated` (safe once the language is known), and
`sentence_initial` (real words in this language, removed only when they open an
utterance) — and an unknown language now gets an empty ruleset rather than
English. Tested.

### One more convergence

That app matches custom vocabulary with **soundex plus Levenshtein
distance**. The design specifies Double Metaphone plus normalised edit distance
for the same job, arrived at independently. Two projects reaching the same
shape is reasonable evidence the shape is right.

## Tier 1 has no job on the Apple path

The finding that changes the project. Every Tier 1 evaluation before this ran on
**hand-written unpunctuated fixtures** — "um so the thing is uh we need to...".
Apple SpeechTranscriber does not produce text like that. It produces:

```text
I ran Terra Grunt against the azure landing zone, then Terraform applied the
V net and the HCL was valid.
```

Already punctuated. Already capitalised. No filler sounds, because an acoustic
model trained on clean transcripts does not emit them. So every measurement so
far was scoring a job the model does not actually have.

### The real chain, on real engine output

Fifteen captured SpeechTranscriber transcripts (`bench/engine-fixtures.json`),
through Tier 0, then Tier 1, with four prompt candidates.

**Tier 0 changed 1 of 15** — it removed a leading "So". Correct and safe.

| Prompt | Changed | dropped | invented | **Damaged** |
|---|---|---|---|---|
| current | 4/15 | 0.066 | 0.060 | **4/15** |
| narrow | 1/15 | 0.008 | 0.008 | **1/15** |
| invisible | 2/15 | 0.017 | 0.021 | **2/15** |
| narrow_examples | 1/15 | 0.017 | 0.017 | **1/15** |

**Every change Tier 1 made was damage. Its improvement rate is zero.** Not
"low" — zero. Across four prompts spanning permissive to maximally restrictive,
not one produced a change that improved the text.

```text
in : We deployed to AWS as well, and cube control showed the pods running.
out: We deployed to AWS and the cube control showed the pods running.

in : The postgress database uses PG vector for embodings.
out: The PostgreSQL database utilizes the PG vector for embedding.
```

### The prompt was never the problem

The prompt bake-off on unpunctuated fixtures showed a clean tradeoff — permissive
prompts punctuated 16/20 but damaged 5/20; restrictive prompts damaged 0/20 but
punctuated only 3/20. That looked like a tuning problem with a sweet spot to
find.

There is no sweet spot, because there is no work. The restrictive prompts scored
"0 damage" by doing nothing, and doing nothing is the correct behaviour when the
input is already clean.

### What is actually wrong with the engine output

`postgress` · `cube control` · `Ogam` · `Superbase` · `passer` · `cash` for
`cache`. Every one is a **vocabulary** error, not a cleanup error — and the
dictionary is the layer that fixes them. Tier 1 does not fix them,
and changes words nobody asked it to: `postgress` should become `Postgres`, the
word that was said, and the model wrote `PostgreSQL`. That is the same database
under its full name, but which form comes out is the dictionary's choice, not
the model's. (Corrected 2026-09-11: this line first called it a different
product.)

**On the Apple path the key feature is the dictionary, not the language
model.**

### Caveats, and they matter

- **n=15, synthesised speech.** `say` at a steady rate is more fluent than a
  person. Real dictation has hesitation, restarts and run-on sentences, and the
  engine may punctuate it worse. This needs confirming against human speech
  before it is final.
- **One engine.** Parakeet may not punctuate at all, in which case Tier 1 has
  real work on that path. Phase 2 should measure it rather than assume.
- **One model.** Granite 4.1 3B invents nothing and might behave differently —
  though it cannot help with vocabulary either, which is the actual failure.

### The headline claim is ON HOLD, not changed

Decided 2026-09-08: **do not touch the headline claim until Parakeet is measured.**

The finding above says Tier 1 has no job *on the Apple path*, which is one
engine of six. The headline leads with "cleanup that keeps pace with speech", and rewriting
that on single-engine evidence would be the same mistake as the 645 ms figure —
a headline changed on a measurement that turned out to be narrower than it
looked.

If Parakeet does not punctuate, Tier 1 has real work off macOS and the headline stands as
written. If Parakeet does punctuate, it needs rewriting and the dictionary
becomes the stated key feature. **Measure first.**

Anyone reading this section: the headline claim is not stale, it is pending.

### What follows

Tier 1 should be **off by default on the Apple path** until it can be shown to
help. It costs 302 ms and, on this evidence, buys damage. The plan was to do exactly
this: report the numbers rather than tune silently.

The Tier 0 fallback path — engine, dictionary, Tier 0, paste — is ~130 ms and
strictly better output than the same chain with Tier 1 in it.

## The decision matrix

The Tier 1 finding is **engine-dependent**, not universal: Tier 1 has no job on
the Apple path *because Apple punctuates*. Parakeet may be different. So the
pipeline is derived from engine capability rather than fixed — the same shape as
`EngineCaps::owns_segmentation`, which already decides whether VAD segments or
merely gates.

Implemented in `crates/macalla-core/src/pipeline.rs`, so the matrix is
executable and tested rather than a table that drifts.

| Engine | punctuates | capitalises | emits fillers | segments | measured |
|---|---|---|---|---|---|
| **Apple SpeechTranscriber** | yes | yes | **no** | yes | **2026-09-08, n=15** |
| Parakeet TDT v3 | ? | ? | ? | no | **not measured** |
| Parakeet Realtime EOU | ? | ? | ? | yes | not measured |
| Whisper large-v3-turbo | ? | ? | ? | no | not measured |
| Moonshine v2 | ? | ? | ? | no | not measured |
| Cloud realtime | ? | ? | ? | yes | not measured |

Which resolves to:

| Stage | Apple | Unmeasured engine | Why |
|---|---|---|---|
| VAD segments | no | yes | engine owns segmentation |
| **Dictionary** | **yes** | **yes** | **never skipped** — vocabulary errors are what every engine gets wrong, and no engine fixes them for us |
| Tier 0 fillers | no | yes | Apple does not transcribe filler sounds |
| Tier 0 structure | **yes** | **yes** | **never skipped** — "new paragraph" is an instruction from the user, not a transcription artefact, and no engine acts on it |
| **Tier 1** | **no** | yes | measured: it damaged the text on every change it made |

### Two stages never turn off

**The dictionary.** Every engine gets vocabulary wrong — `postgress`,
`cube control`, `Superbase`, `Ogam` — and none of them fixes it. This is the
layer that survives every engine swap, which is what makes it the durable
key feature rather than Tier 1.

**Tier 0's structure commands.** When a user says "new paragraph" they are
issuing an instruction, not producing text to be transcribed. No engine acts on
it, so something always must.

### Unmeasured engines get the full pipeline

`TextCaps::measured` gates the whole thing. If nobody has measured an engine's
output, its capability flags are **ignored** and every stage runs — even if the
flags claim otherwise. A test pins this.

The asymmetry is deliberate. Wrongly assuming an engine cleans up means shipping
raw disfluency to the user with nothing downstream to catch it. Wrongly assuming
it does not costs some latency and produces a measurable regression we will see.
Fail toward doing the work.

### What this makes visible

Five of six engines are unmeasured. That is not a gap to feel bad about — it is
now *legible*, and the plan is conservative until each one is measured. Phase 2
should measure Parakeet before assuming anything, because if Parakeet does not
punctuate then Tier 1 has real work on that path and the whole Tier 1
investment is justified off macOS.

## Spike A2 — Parakeet TDT v3, and the headline decision

Measured with `parakeet-mlx` against `mlx-community/parakeet-tdt-0.6b-v3`, on
the **same audio fixtures** as Apple, so the comparison is like for like.

### Parakeet punctuates and capitalises too

```text
The latency budget is 300 milliseconds. We enhance each segment while you are
still speaking. Only the trailing segment is in flight when you release the key.
```

Full sentence terminators, capitalisation, commas. It also beat Apple on
vocabulary in the same clip — **`Azure`, `VNet` and `parser` correct**, where
Apple produced `azure`, `V net` and `passer`.

### But it transcribes fillers, and Apple does not

The decisive test: one deliberately disfluent clip, both engines.

```text
spoken   : um so the thing is uh we need to we need to ship the the parser by
           friday. er and then, hmm, run the benchmarks you know.

Parakeet : Um so the thing is uh we need to we need to ship the the passer by
           Friday uh and then um run the benchmarks you know.

Apple    : So the thing is that we need to, we need to ship the passer by
           Friday here and then. | Run the benchmarks, you know.
```

**Parakeet keeps every filler and every stutter. Apple discards them all.**

Two side notes worth recording: Apple *added* words nobody said — "that" and
"here" — which is engine hallucination, not cleanup. And both engines heard
`passer` for `parser` on disfluent audio, though Parakeet got it right on the
fluent clip. Vocabulary is the shared weakness.

### The capability table, now measured for two engines

| Engine | punctuates | capitalises | emits fillers | measured |
|---|---|---|---|---|
| Apple SpeechTranscriber | yes | yes | **no** | 2026-09-08 |
| **Parakeet TDT v3** | **yes** | **yes** | **yes** | **2026-09-08** |
| Whisper, Moonshine, cloud | ? | ? | ? | not measured |

They differ on exactly one flag, and it is the one that decides whether Tier 0
has work. Encoded and tested in `pipeline.rs`.

### What this decides, and what it does not

**Tier 0 is vindicated on the Parakeet path.** Filler removal and stutter
collapse have real work to do there — deterministic, sub-millisecond, and now
demonstrably necessary rather than theoretical.

**Tier 1 still has no punctuation job on either engine.** Both punctuate and
capitalise. That was the last plausible role for it after Tier 0 takes the
fillers, and neither engine leaves it vacant.

So the answer to the headline hold is: **the streaming-LLM claim does not survive
on either measured engine.** What survives, and is now measured twice, is
Tier 0 plus the dictionary — sub-millisecond cleanup, and vocabulary correction
that both engines demonstrably need.

### The caveat that stops this being final

Fixtures are `say`-synthesised. Synthetic "um" is a crisply pronounced word;
a human "um" is a vocalisation with different acoustics, and an engine may
treat the two differently. That cuts both ways — Apple may discard real fillers
less reliably, and Parakeet may transcribe them differently.

**This needs one session of real human dictation before the headline claim is rewritten.**
It is a cheap test and it is the only thing standing between this evidence and a
decision.

## Real human speech, and the finding it overturned

Recorded 2026-09-08 with `bench/record-and-compare.sh`, reading
`fixtures/dictation-script.md` aloud. Mean level −26.8 dB, which is normal
dictation. Both engines on identical audio.

### Apple DOES transcribe fillers. The synthetic measurement was wrong

```text
Um, so the thing is, uh, we need to, we need to ship the parcel by Friday.
Um, The HCM was valid.
Hmm, uh, We deploy to AWS as well.
```

Every filler present. On 15 `say`-synthesised fixtures Apple discarded **all**
of them, and `TextCaps::apple_speech` was set to `emits_fillers: false` on that
basis.

Synthetic "um" is a crisply articulated word; a human "um" is a vocalisation,
and the engine treats them as different things. **The synthetic measurement was
not imprecise, it gave the opposite answer.** Corrected in `pipeline.rs`.

This is the second time today a measurement's *method* produced a wrong
headline rather than a wrong number — the first was attributing harness feed
overhead to engine latency. Both were caught by measuring the same thing a
different way.

### Both engines punctuate and capitalise against a person too

That part held. So Tier 1 still has no punctuation job on either path, and
**Tier 0 now has work on both** rather than only on Parakeet.

### Technical vocabulary — Parakeet ahead, but not by the margin first reported

The first recording ran out of time and only captured a third of the script.
On that partial data this document said "Parakeet 5 of 9, Apple 0 of 9". The
full read corrects it:

| Spoken | Apple | Parakeet |
|---|---|---|
| parser | **`parser`** | **`parser`** |
| Terragrunt | `terra grunt` | `TerraGrunt` |
| Azure | `azure` | **`Azure`** |
| Terraform | `terraform` | **`Terraform`** |
| VNet | **`VNet`** | **`VNet`** |
| HCL | **`HCL`** | **`HCL`** |
| kubectl | `Cube CTL` | `kube control kubectl` |
| Postgres | `postgrass` | **`Postgres`** |
| pgvector | `PG vector` | `PG vector` |
| Supabase | `Superbase` | `Superbase` |
| Proxmox | **`Proxmox`** | **`Proxmox`** |
| LXC | **`LXC`** | `LXE` |
| Tailscale | `tail scale` | **`Tailscale`** |
| Ogham | `OM` | `OM` |
| MCP | **`MCP`** | **`MCP`** |

**Parakeet 9 of 15. Apple 6 of 15.** Parakeet is still ahead, and ahead on the
harder terms, but "0 of 9" was an artefact of a truncated recording and is
withdrawn.

Neither gets `Ogham`, `Supabase`, `pgvector` or `kubectl`. Those need the
dictionary's explicit `spoken` forms; no engine and no phonetic metric will
reach them.

### Apple made general-English errors Parakeet did not

Not vocabulary — ordinary words:

| Spoken | Apple | Parakeet |
|---|---|---|
| the latency budget | `The latest say budget` | **`the latency budget`** |
| the trailing segment | `the training segment` | **`the trailing segment`** |
| showed the pods running | `should the pod's running` | **`showed the pods running`** |

### The homophones fail on both engines, in opposite directions

The most interesting result of the run.

```text
spoken   : Clear the cache before you count the cash.
Apple    : Clear the cash  before you count the cash.      <- both to the common word
Parakeet : Clear the cache before you count the cache.     <- both to the technical word

spoken   : The queue took its cue from the sink that failed to sync.
Apple    : The queue took its cue from the sink that failed to sink.
Parakeet : The queue took its queue from the sync that failed to sync.
```

**Apple collapses toward the everyday word; Parakeet collapses toward the
technical one.** That is the same prior showing up in both results: Parakeet
has a technical-domain bias, which wins it the vocabulary table and loses it
`cash` and `cue`.

Neither is fixable by the dictionary, for the reason established earlier: both
spellings are ordinary English, so a rewrite rule breaks the other sense in the
same sentence. And note the two engines would need **opposite** corrections,
so this cannot be a single engine-agnostic rule either.

### Fillers, confirmed on a full read

Apple: `Um`, `huh?`, `Uh`, `Mm`. Parakeet: `Um`, `huh?`. Both transcribe them,
confirming the correction above. Apple emitted more of them than Parakeet on
this take.

### What this does to the pipeline

| | Before (synthetic) | After (real speech) |
|---|---|---|
| Apple emits fillers | no | **yes** |
| Tier 0 on Apple | skipped | **runs** |
| Tier 0 on Parakeet | runs | runs |
| Tier 1 on either | no job | no job |
| Dictionary | essential | **more essential** |

Nine of fifteen technical terms wrong on the macOS default engine, and six of
fifteen on Parakeet, in one read-through. Neither engine gets `Ogham`,
`Supabase`, `pgvector` or `kubectl`. That is the key feature, measured.

### Caveats

One speaker, one accent, one recording, one room. Accent and microphone will
both move these numbers, and a 40-second read is not a work session. But it is
real speech, and it was enough to overturn a finding that 15 synthetic fixtures
had established confidently.

## A second domain, and a control

The headline claim is that a clinician's or a lawyer's vocabulary fails the same way an
engineer's does. That was an assertion. Two things were added to test it.

### The control: Harvard Sentences

Everything measured before this compared engines on *domain* vocabulary and
found them wanting. That only means something if the engines are otherwise fine
on this voice, microphone and room. Harvard Sentences (IEEE 1969, public domain,
`fixtures/dictation-script-control.md`) are phonetically balanced everyday words
— deliberately not domain vocabulary.

| Engine | Harvard List 1 |
|---|---|
| **Parakeet TDT v3** | **10 of 10 word-perfect** |
| Apple SpeechTranscriber | 2 errors: `blew the sheet` for "Glue the sheet", `the park truck` for "the parked truck" |

So both engines are essentially fine on common English. **The failure is
specific to domain vocabulary**, which is what makes the headline claim falsifiable rather
than merely plausible — and it survived the falsification.

Read the control before any domain comparison on a new machine, microphone or
voice. If both engines do badly on it, the domain numbers from that setup mean
much less than they appear to.

### The second domain: clinical

`fixtures/dictation-script-clinical.md` — drug names, symptoms, investigations.
Nothing exotic; a discharge summary.

| Engine | Terms correct |
|---|---|
| Apple SpeechTranscriber | 8 of 17 |
| Parakeet TDT v3 | 11 of 16 |

Apple's failures are not near-misses:

```text
hypertension     -> high potension
dyspnoea         -> disneal
levothyroxine    -> Continually both thyroxine
omeprazole       -> a mecrozole
atorvastatin     -> Atavastat into
She reports      -> She records
```

Parakeet's are close but wrong, which is its pattern across both domains:

```text
atorvastatin     -> atovastatin
rosuvastatin     -> rosovastatin
levothyroxine    -> levothyroxin
omeprazole       -> omeprozole
```

**The pattern is identical to the infrastructure domain** — same ordering, same
failure shapes, same rough hit rate (Apple 6/15 and 8/17; Parakeet 9/15 and
11/16). The headline cross-domain claim holds.

### One error worth singling out

Parakeet turned **`dyspnoea` into `dyspnea`** — it Americanised a British
clinical spelling. That is the same class of error as rewriting `postgres` to
`PostgreSQL`, and it will follow a British user into *every* domain, not just
this one. A dictionary entry fixes it; nothing else in the stack will.

### What a standard test set can and cannot tell you

Harvard Sentences, the Rainbow Passage, LibriSpeech and Common Voice all measure
**general recognition accuracy**. They are built from common vocabulary on
purpose, so they cannot say anything about the problem this product exists to solve.

Use them as the **control**, never as the measurement:

| | Purpose |
|---|---|
| Harvard Sentences | is the engine working at all, on this voice and mic? |
| Domain script | does it get *your* words right? |

Both engines scoring well on the control and badly on a domain script is the
result that matters. Either one alone is not evidence.

## Canary 180M Flash — a third engine, and an impression tested

KB's read, from using another dictation app: Canary handles technical terms well. Worth
testing, because impressions are what benchmarks are for.

**It is NVIDIA**, same NeMo lineage as Parakeet. `nvidia/canary-180m-flash`,
**CC-BY-4.0** — the same licence and attribution obligation as Parakeet TDT v3.
Four languages: en, de, es, fr. Crucially there is a sherpa-onnx conversion
(`csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr`), which is the
runtime already specified, so it needs no new dependency.

### Word error rate, identical audio, reference transcripts

`bench/engine-wer.py`, `bench/wer-input.json`. Substitutions + deletions +
insertions over reference length. Deletions are reported separately because a
model that silently drops a clause fails differently from one that mishears a
word, and WER alone hides that.

| Fixture | Apple | Parakeet | **Canary 180M** |
|---|---|---|---|
| Harvard control | 0.025 | **0.000** | 0.013 |
| Technical | 0.200 | **0.133** | 0.200 |
| **Clinical** | 0.281 | 0.188 | **0.125** |

**The impression holds on clinical vocabulary** — Canary has the lowest WER
of the three, with zero deletions, beating Parakeet by a third. On general
English Parakeet is word-perfect and Canary near it. On the infrastructure
fixture Parakeet leads.

So no engine wins outright. Canary is genuinely strong on domain vocabulary,
which is precisely the axis this project cares about.

### But it dropped whole clauses on the 75-second human recording

```text
spoken : I ran Terragrunt against the Azure landing zone, and then Terraform
         applied the VNet.
Canary : I ran TerraForm applied the VNet.
```

It also omitted the homophones line and the latency line entirely. Apple and
Parakeet transcribed the same audio in full.

That is a **more serious failure than any vocabulary error** — a user cannot see
what is missing.

#### Investigated: it is not chunking, and it is not deterministic

Chunking was the obvious fix, and it made things worse. Investigating further:

| Test | Result |
|---|---|
| 15 s chunks, one recogniser reused | chunk 0 decoded, 1–4 empty |
| 15 s chunks, **fresh recogniser each** | identical — so not reuse |
| Chunk 2 alone, first decode on a fresh recogniser | empty |
| Chunk 2 contiguous / copied / round-tripped via WAV / pre-resampled to 16 kHz | empty every way — so not array handling |
| 5 s sliding windows across the file | **0–50 s all empty, 50–60 s decoded** |

**The failures are not positional and not reproducible.** The same 15-second
span decoded in one run and returned empty in another. Chunk 0 worked in the
first run and the equivalent window returned empty in the third.

That is the signature of a runtime fault, not a model characteristic. Whether
it is sherpa-onnx's Canary support, this particular ONNX export, or something in
the Python binding, this investigation could not tell — and that uncertainty is
itself the finding.

#### What another open-source app's source says

That app ships Canary as its active model, so it was checked first for a tell.
Its transcription manager loads it via
`CanaryModel::load(&model_path, &Quantization::Int8)` and calls
`canary_engine.transcribe(&audio, &options)` — **the whole buffer, no
chunking**. The crate is `transcribe-rs` 0.3.8 with ONNX features, not
sherpa-onnx directly.

So that app feeds whole audio too. Either `transcribe-rs` handles internally what
sherpa-onnx does not, or it has the same fault and it has gone unnoticed
because dictation clips are short. No tell either way.

#### Decision: Parakeet

Decided: check the other app for a tell, otherwise run with Parakeet. There is no
tell, so **Parakeet remains the default and Canary is not pursued for 0.1.**

The evidence supports it independently of the fault: Parakeet covers 25
languages to Canary's four, is word-perfect on the control, and transcribed the
full 70-second human recording correctly. Canary's clinical WER advantage was
real, but it was measured on a 30-second fixture with a runtime that cannot be
relied on to return the same answer twice.

If Canary is revisited, the first step is `transcribe-rs` rather than
sherpa-onnx, and the gate is a reproducibility check — the same audio decoded
ten times must give the same answer — before any accuracy number is trusted.

### The trade, stated plainly

| | Parakeet TDT v3 | Canary 180M Flash |
|---|---|---|
| Languages | **25** | 4 (en, de, es, fr) |
| General accuracy | **word-perfect on control** | 1 error |
| Clinical vocabulary | 0.188 | **0.125** |
| Long audio | fine | **drops content** |
| Size | ~600M params | **~180M** |
| Licence | CC-BY-4.0 | CC-BY-4.0 |

The point stands: **Parakeet's multilingual coverage is the deciding
factor.** Twenty-five languages against four is not a margin a WER difference on
one fixture overturns, and Italian is
not in Canary's four.

**Parakeet remains the default.** Canary is worth carrying as an
English-and-European option once the long-audio behaviour is understood,
particularly for users whose vocabulary is medical rather than infrastructural.

### A flaw in the first run, recorded

The first version of the technical reference used *spoken* forms — "V net",
"H C L", "kube control" — so every engine was penalised for correctly emitting
`VNet`, `HCL` and `AWS`. That inflated WER for all three equally and produced a
spurious "drops content" flag on all of them. The reference is now the written
form, which is what we actually want the engine to produce.

## A second human take — replication

A different session, a different microphone, 66 seconds, mean −26.9 dB. Same
script. **The audio is never committed**: a voice recording is personal data, so
it lives only in gitignored `fixtures/audio/` and is explicitly excluded from
publishing. The transcripts are the fixture.

This matters more than another data point. A single human read could have been a
fluke, and one of today's findings was already overturned by a change of method.
Two independent takes agreeing is a different quality of evidence.

### Technical vocabulary

| | Take 1 | Take 2 |
|---|---|---|
| Apple SpeechTranscriber | 6 of 15 | **7 of 16** |
| Parakeet TDT v3 | 9 of 15 | **10 of 16** |

Replicated: Parakeet ahead by roughly the same margin, and both engines in the
same band.

### What replicated exactly

**Neither engine can produce `Ogham`.** Take 1: `OM` / `OM`. Take 2: `Om` /
`Ohm`. Four attempts across two engines and two takes, zero successes — for the
name of the project family. `Supabase` and `pgvector` also failed in both takes
on both engines.

**The homophone split held precisely.** Apple collapses to the everyday word,
Parakeet to the technical one:

```text
Apple    : Clear the cash  ... count the cash.   |  the sink that failed to sink
Parakeet : Clear the cache ... count the cache.  |  the sync that failed to sync
```

Same direction, both takes. That is a stable property of the models, not noise.

**Both transcribe fillers.** `Um`, `Hmm`, `huh?` on both engines, both takes —
confirming the correction made against the synthetic fixtures.

### What varied between takes

Worth recording, because it bounds how much a single reading can be trusted:

| Term | Take 1 | Take 2 |
|---|---|---|
| `Tailscale` | Apple `tail scale`, Parakeet **`Tailscale`** | Apple **`Tailscale`**, Parakeet `tail scale` |
| `kubectl` | neither | Parakeet **`kubectl`** (after `kubectrol`) |
| `Postgres` | Apple `postgrass`, Parakeet ✓ | identical |

The `Tailscale` result **reversed between engines**. So a term getting through
once is not evidence it will get through again, and a dictionary entry is the
only thing that makes a correction reliable rather than lucky. That is a
sharper argument for the dictionary than any single-take table.

## An open-source dictation app on the same script — a pipeline, not an engine

Dictated directly into a notepad from `fixtures/dictation-script.md`.

**This is an open-source dictation app (Apache-2.0)**, *not* a commercial cloud
app. An earlier version of this section had it the wrong way round and
drew a conclusion about commercial services from it. That was wrong and is
withdrawn — the commercial cloud app had not been tested at that point.

Its settings show Parakeet V3 active, so this is **Parakeet plus the app's
post-processing**: a comparison of pipelines, not of engines. Which makes it
more useful than a second engine reading would have been, because its raw
engine output should be close to ours.

### Its post-processing, isolated

Same model, same script, so the differences are the app's:

| | Our raw Parakeet | Open-source app |
|---|---|---|
| Fillers | `Um so the thing is, huh?` | `so the thing is, huh` — **`Um` removed** |
| Articles | `I ran Terra Grunt` | `I ran **the** Terra Grunt` |
| | `then Terraform applied` | `then **the** Terraform applied` |
| `Tailscale` | `tail scale` | **`Tailscale`** |
| Homophones | `cache … cache`, `sync … sync` | `cash … cash`, `sink … sink` |

**Its filler removal works** — `Um` is gone, which is its deterministic
filler cleaner doing exactly its job.

**But something inserted two articles nobody spoke**, and flipped both
homophone pairs to the opposite sense from raw Parakeet. Deterministic regex
cannot do either. That is a language-model correction pass, and it is the same failure mode we measured in our own
Tier 1 experiments, visible in a shipping app.

It fixed `tail scale` → `Tailscale`, which is a real win. So its LLM pass is
not useless; it is *unpredictable*, improving one term while inventing two words
and changing two others.

### The five terms nothing gets

`Terragrunt` · `kubectl` · `pgvector` · `Supabase` · `Ogham`

Now failed by Apple, Parakeet, Canary, and a shipping app with both
deterministic and LLM post-processing on top. `LXC` → `LXE` survived its
pipeline too.

**No amount of post-processing recovers a term the engine never heard.** That is
the argument for the dictionary, and it is now four independent systems deep.

### Still untested: a commercial cloud app

A commercial cloud product remained the one comparison not made, and it is the one
that would say whether a cloud service does better on vocabulary. Worth doing.

## A commercial cloud app, and what it tells us

A commercial cloud dictation app. Same script.

**Caveat, and it matters:** the app's **custom
dictionary** had already been configured with terms including AWS. So this is not a like-for-like engine
comparison — it is the product *as a user actually configures it*, which is
the fairer test of the product and the weaker test of the model. Read the
vocabulary numbers with that in mind.

### It still fails the terms nothing gets

| Term | Cloud app |
|---|---|
| `Ogham` | `Omm` |
| `kubectl` | `kube control` |
| `pgvector` | `PG Vector` |
| `LXC` | **`LC`** — worse than Parakeet's `LXE` |
| `Terragrunt` | `TerraGrunt` |
| `Terraform` | `TerraForm` |

Five systems now: Apple, Parakeet, Canary, the open-source app's full pipeline, and a
commercial cloud service with a user-configured dictionary. **All five fail `Ogham`,
`kubectl` and `pgvector`** — and in the cloud app's case that is confirmed to
be because those three were never entered, not because its dictionary failed.

Which is the honest shape of the feature, and it sharpens the design brief: a
dictionary fixes exactly what you put in it, so **what matters is how easily it
gets filled**. Correcting `Ogham` in the cloud app required noticing it was wrong,
opening settings and typing an entry. Most people will not do that three times.
That is why add-from-history is the mechanism here rather than a settings
form.

### The "win" was its dictionary — and the test is fair

The cloud app's dictionary contained **Terragrunt, Terraform, Supabase and
AWS**. It does **not** contain `kubectl`, `pgvector` or `Ogham`. Auto-cleanup is
set to **Medium**.

So the `Supabase` result was not its model outperforming everything else — it
was a dictionary entry firing, exactly as ours would. That claim is withdrawn.

And the failures are now cleanly attributable:

| Term | In its dictionary | Produced | Reading |
|---|---|---|---|
| Supabase | yes | `Supabase` | dictionary worked |
| AWS | yes | `Amazon Web Services` | expansion, presumably intended |
| Terragrunt | yes | `TerraGrunt` | non-canonical casing |
| Terraform | yes | `TerraForm` | non-canonical casing |
| **kubectl** | **no** | `kube control` | **not entered** |
| **pgvector** | **no** | `PG Vector` | **not entered** |
| **Ogham** | **no** | `Omm` | **not entered** |

**This is the fairest comparison in this document**, and its conclusion is the
cleanest: every term in the dictionary was corrected, and every term not in it
failed. The dictionary is what fixes vocabulary. Nothing else does.

On the casing: `TerraGrunt` and `TerraForm` are not the canonical spellings, but
whether that is the product returning the wrong form or the entry having been
made that way is not knowable from outside, and it should not be asserted. What
it does highlight is a design property worth being deliberate about — the
`written` field is the single source of truth, so whatever is entered is what
comes out, exactly. Ours is pinned by a test that takes the cloud app's output as
input:

```text
"TerraGrunt"  ->  "Terragrunt"
"TerraForm"   ->  "Terraform"
"Superbase"   ->  "Supabase"
"PG Vector"   ->  "pgvector"
```

`Terraform` in a commit message is correct and `TerraForm` is wrong in a way a
reader notices, so exact-form output is part of the job rather than a nicety.

### Three products have converged on an aggression dial

The cloud app's cleanup is set to **Medium** — so it has a scale. The open-source app
ships **minimal** and **fullRephrase**. And the model bake-off in this document
found the same axis empirically: cleanup aggression and content loss move
together, and picking a model is mostly picking a position on that dial.

Three independent products, one of them a commercial cloud product, have all
concluded that this is a **user setting rather than a value to tune**. If Tier 1
ever ships enabled, it should offer the same choice instead of one fixed prompt.

Worth noting the tense rewrite — "when you release the key" becoming "when
you've released the key" — happened at **Medium**, not at some maximum setting.
Invention is present in the middle of the scale, in a commercial product.

### It fails every homophone, like everything else

```text
Clear the cache before you count the cache.       <- both to the technical word
The queue took its queue from the sync that failed to sync.
```

Four homophone pairs, four failures. **Nothing tested — free, commercial, local or
cloud — gets these right.** That closes the question: the homophone problem is
not solved by money, model size, or cloud compute.

### Its cleanup is aggressive, and it rewrites

Fillers are gone entirely — no `Um`, no `you know`, and the opening `So` removed.
That is a better filler result than anything else tested.

But it also:

- expanded `AWS` to `Amazon Web Services` (a dictionary entry doing its job)
- rendered `milliseconds` as `ms`
- changed **"when you release the key"** to **"when you've released the key"** —
  a tense rewrite nobody asked for

That last one is invention, in a commercial product, on a sentence that was already
correct. It is the same failure mode we measured in Tier 1 and observed in
the open-source app's pipeline. Three independent systems, same behaviour.

### The finding that matters most

**A commercial cloud app's answer to the vocabulary problem is a custom
dictionary, and its users maintain one.**

That is not a criticism — it is validation. The claim is that the dictionary is the
layer that fixes what engines get wrong. A commercial product arrived at the
same answer.

The argument becomes concrete: **the same correction layer, running locally,
across every engine, for nothing.** Plus a vocabulary that survives changing
engine — which a cloud product's dictionary cannot offer, because it only works
inside that product.

---

## Where the Bluetooth cold-open cost actually is

**2026-09-08, Sony WH-1000XM2 over Bluetooth, cold (idle, first open of the
session).** The measurement is an ignored test,
`where_the_bluetooth_cold_open_cost_actually_lands` in
`crates/macalla-platform/src/audio.rs`.

Opening a microphone on the first press after idle cost 3048 ms on this headset
and lost a whole utterance. `Capture::start` does two things together — it
*builds* the input stream and it *plays* it — and which half holds the three
seconds decides whether the cost can be moved off the keypress at all. Nobody
had separated them.

| Step | Cost |
|---|---|
| Read the device format | **2.2 ms** — reported 16000 Hz, 1 ch, f32 |
| Build the input stream (not playing) | **2788.5 ms** |
| Play the stream | **139.8 ms** |
| Re-read the format afterwards | 16000 Hz — **unchanged** |

**Build is 95% of it.** So the cost is in the half that a prewarm can move: a
stream built at arm time and merely played on the keypress would start in about
140 ms rather than about 2.8 seconds.

### The second finding, which was not the one being looked for

The format read *before* the stream existed already said **16000 Hz**, and it
still said 16000 Hz afterwards. The A2DP-to-HFP rate change that
an open-source dictation app's Bluetooth wait polls for — up to three seconds,
because "Bluetooth devices switch from A2DP (48kHz) to HFP/SCO (16/24kHz) when
the mic is activated" — **did not show up here at all.** CoreAudio reported the
HFP format for this input device regardless of the link state, and the
negotiation cost appeared inside `build_input_stream` instead of as a visible
rate change.

One device, one run, so this is not yet a general claim. But it means the
resampler-configured-for-a-stale-rate failure did not reproduce on this
hardware, and the reason we are protected from it is not that it cannot happen
— it is that cpal now reports `StreamInvalidated` and we act on it.

### The result, measured on a real dictation

**Device start: 31 ms.** Key down at `22:15:04.261`, device reported started at
`22:15:04.292`. Against 3048 ms for the same headset before the change.

| | Before | After |
|---|---|---|
| Device start, first press after idle | 3048 ms | **31 ms** |
| Engine session (Apple SpeechTranscriber) | not measured separately | 237 ms |
| Release to text | — | **79 ms** (budget 300 ms) |

The important property is not the number, it is that **the press cost no longer
depends on the state of the Bluetooth link.** The expensive part happens while
the app waits for a key, so a colder headset makes the *arming* slower and
leaves the keypress alone.

One residual: press *during* the arming window and you pay whatever is left of
it. An earlier run pressed 280 ms after start-up, while the stream was still
building, and saw 405 ms.

### The measurement bug this exposed

The figure printed as "device open" was **268 ms**, not 31. It had always timed
the device open *plus* the engine's streaming session, which was harmless while
a cold device took three seconds and dwarfed everything beside it. Once the
device became fast the engine session was the larger half, and the metric was
mostly reporting something the prewarm does not touch — making a 90x
improvement look like a 10x one.

They are timed separately now. Worth stating plainly because these figures are
quoted here: **a benchmark that measures one thing while claiming another is
worse than no benchmark.**

### The recording indicator, which decides whether this is fixable for free

**An armed stream does not light the orange dot.** Observed 2026-09-08: the
indicator stayed dark while the stream was built and not playing, and lit as
soon as it played.

That second half is the control, and it is the reason the observation is worth
anything. Without confirming the dot *does* light when the stream plays, "I saw
no dot" is indistinguishable from "I was looking at the wrong part of the menu
bar" — which is exactly what happened on the first two attempts at this
measurement. Nothing in Rust can read that dot, so the test stops and waits for
a keypress rather than sleeping on a timer; asking a person to be ready on the
program's schedule is how you collect an answer they did not actually make.

So the fix is free. The stream is built when the hotkey arms and played when
the key goes down: **~140 ms at the keypress instead of ~2.8 s**, with the
indicator still meaning "recording" and not "ready".

If a future macOS lights the dot on an armed stream, the prewarm goes — not the
indicator.

---

## Apple Intelligence as an enhancement provider (2026-09-11)

The provider in `crates/macalla-enhancers`, on the reference machine (M1 Pro,
macOS 26.6.2), driven by `macalla enhance` in a debug build; the time is the
on-device model's. The 20 fixtures in `bench/fixtures.json` after Tier 0, the
default punctuation-only prompt, a fresh session per request, and the guard
that rejects any changed word for that prompt.

| Request | Errors | p50 | p95 | Changed and kept | Left as Tier 0 |
|---|---|---|---|---|---|
| Plain text | 0 | 545 ms | 1301 ms | 17 | 3: two rejected for changed words, one refused by the safety checks |
| Schema with one `text` field | 0 | 613 ms | 1812 ms | 4 | 16: one rejected, fifteen returned as sent |
| Schema first, plain text if it throws (the default) | 0 | 602 ms | 1782 ms | 4 | 16: one rejected, fifteen returned as sent |

Each p95 is the first request of its run.

- **The schema reply mostly does nothing with this prompt.** In the first
  schema run, the four sentences it changed gained capitals and nothing else.
- **Plain text does the job, 70 ms faster at p50.** Spike B's 340 ms gap was
  measured with an earlier prompt and a two-field schema.
- **The plain-text fallback never ran.** The schema call did not throw once, so
  the default behaved as the schema row. It is the default by choice, to be
  revisited when the prompt is tried against the schema.
- **The safety checks refused an ordinary sentence**, "like we should probably
  run terragrunt against the azure landing zone first", as
  `guardrailViolation` ("may contain sensitive or unsafe content"). The
  provider keeps a refused sentence's Tier 0 text rather than failing the
  segment.

### Three earlier runs were thrown away

Earlier runs of the same comparison were spoiled three ways: CI was building on
the same Mac, the modes ran in one order so the first took the model's warm-up,
and the timeout abandoned requests instead of cancelling them. Replies slowed
from about 0.55 s to 3.6 s, then every request in a run timed out at 10 s, and
single requests in new processes took about 7 s until the system's inference
service had drained. Requests left running in that service are the suspect, not
a proven cause. The table comes from runs on a settled service with nothing
else building. Whether cancelling on timeout prevents the pile-up is not yet
measured: nothing timed out in the runs above.

### Apple Intelligence plain text, four runs of one configuration

Same machine, later the same day and after a reboot. `bench/enhance_ab.sh`
sent the 20 fixtures as plain text through two debug builds in the order A, B,
B, A. It started after one request came back in 1,221 ms, with no CI job
running.

**This was meant to compare greedy sampling with temperature 0, and it did
not.** The "greedy" binary has no `macalla_ai_prewarm` symbol, which was added
to the Swift bridge in the same commit as greedy sampling, so its Swift half is
the earlier temperature-0 code and all four runs asked for temperature 0. The
likely cause: the earlier commit was built into the same `target` directory,
its build script wrote the Swift library to the same output directory, and the
later build did not rebuild it. Found when a later link failed on the missing
symbol. The times stand as four runs of one configuration.

| Run | Errors | p50 | Slowest | Left as Tier 0 |
|---|---|---|---|---|
| A1 | 0 | 532 ms | 691 ms | 3 |
| B1 | 0 | 526 ms | 710 ms | 3 |
| B2 | 0 | 524 ms | 694 ms | 3 |
| A2 | 0 | 520 ms | 701 ms | 3 |

- **The replies were identical, 20 of 20,** across all four runs, as one
  configuration should be.
- **No slow first request this time.** The settle request had already woken
  the model, so each run's slowest was the same fixture, the fourth.

### The first request after idle

One request per new process after 90 s with nothing using the model, in the
default reply shape, from the same binary as the runs above. Every other request
was sent with `macalla enhance --prewarm`, but that binary's Swift bridge had no
prewarm, so those requests only waited two seconds longer before asking. **This
says nothing about prewarm**, which is still unmeasured.

| Order | Waited 2 s more | Time |
|---|---|---|
| 1 | no | 1,302 ms |
| 2 | yes | 7,457 ms |
| 3 | no | 7,726 ms |
| 4 | yes | 7,729 ms |
| 5 | no | 1,736 ms |
| 6 | yes | timed out at 10,000 ms |

Four of the six first requests after 90 s idle took over 7 s or timed out.

### Greedy sampling against temperature 0, on checked builds

Rerun the same afternoon on builds checked with `nm`: the temperature-0 binary
has no `macalla_ai_prewarm` symbol and the greedy one has it. Plain text, no
dictionary, in the order A, B, B, A, after a settle request came back in
581 ms.

| Run | Sampling | p50 | Slowest | Left as Tier 0 |
|---|---|---|---|---|
| A1 | temperature 0 | 531 ms | 716 ms | 3 |
| B1 | greedy | 542 ms | 728 ms | 3 |
| B2 | greedy | 541 ms | 721 ms | 3 |
| A2 | temperature 0 | 539 ms | 723 ms | 3 |

The replies were identical, 20 of 20, between the builds. Greedy sampling
changes neither the replies nor the speed here.

### Prewarm, on the checked build

One request per new process after 90 s with nothing using the model, in the
default reply shape and without the dictionary, alternating without and with
`macalla enhance --prewarm`.

| Order | Prewarm | Time |
|---|---|---|
| 1 | no | 7,802 ms |
| 2 | yes | timed out at 10,000 ms |
| 3 | no | 7,764 ms |
| 4 | yes | 3,779 ms |
| 5 | no | 7,756 ms |
| 6 | yes | 3,818 ms |

- **Prewarm halved the first request twice, and the first prewarmed request
  timed out.** Three requests each way. Either way the first request after
  idle takes seconds, far past the 300 ms budget, so Tier 0 is pasted first.
- **Without prewarm the first request took 7.8 s all three times,** against
  1.3 to 7.7 s in the earlier run above. How long the system keeps its model
  ready is not known.

### Slow after a reboot

The first requests after the reboot, 58 minutes in, were before the runs above,
and two single requests timed out at 10 s. Nothing was sent earlier, so how
long the service had been slow is not known. A small Swift program with no timeout sent three sentences
in one process, twice: 7,638, 6,579 and 3,629 ms, then 7,501, 6,603 and
3,639 ms. A minute later the same three, in reverse order and followed by five
fixtures, took 440 to 1,133 ms. Swap was empty and 81% of memory was free, so
memory pressure is not what makes the service slow. What ended it, time or the
requests that ran to completion, is not known.

## Apple Intelligence against models on a local server (2026-09-11)

The same fixtures, default prompt, guard and scoring as the section above, the
same afternoon. The endpoint rows are qwen2.5 at three sizes, all Q4_K_M, on
Ollama 0.34.0 on an idle Mac mini with an M1 and 16 GB of memory,
reached over the network from the reference machine; connecting took 8 to
188 ms in three tries before the run. `bench/enhance_endpoint.sh` sent each
model's first request, then the 20 fixtures twice. The Apple row is runs B1
and B2 above, plain text at temperature 0.

Apple's on-device model is about 3B parameters with 2-bit quantisation-aware
training, according to Apple's *Apple Intelligence Foundation Language Models
Tech Report 2025*
(<https://machinelearning.apple.com/research/apple-foundation-models-tech-report-2025>),
and its assets on the reference machine are named `instruct_3b`. The rows of
the same size are qwen2.5 3B and Granite 4.1 3B, at 4 bits on a different
machine, so the table does not say which model is faster.

| Provider | Runs on | p50 | Slowest | Left as Tier 0 | First request, model loading |
|---|---|---|---|---|---|
| qwen2.5 0.5B, 0.4 GB | Mac mini M1 | 158, 175 ms | 196, 395 ms | 2, both rejected | 1,020 ms |
| qwen2.5 1.5B, 1.0 GB | Mac mini M1 | 336, 342 ms | 468, 438 ms | 4, all rejected | 1,373 ms |
| qwen2.5 3B, 1.9 GB | Mac mini M1 | 648, 632 ms | 828, 917 ms | 2, both rejected | 20,223 ms |
| granite4.1 3B, 2.1 GB | Mac mini M1 | 697, 682 ms | 1,022, 851 ms | 3, all rejected | 1,876 ms |
| Apple Intelligence, plain text | reference machine | 526, 524 ms | 710, 694 ms | 3 | not measured in this shape |

Granite 4.1 3B ran later, in a run that also repeated qwen2.5 3B as a control
while a CI job was building on the reference machine. The control's p50 was 634
and 580 ms and its slowest 760 and 781 ms, so that load did not slow the
requests.

- **Only 0.5B is under the 300 ms budget**, and only at p50: its second run's
  slowest request took 395 ms. With every other provider, Tier 0 pastes first
  and history catches up.
- **Every model answered the same way twice**, 20 of 20.
- **One fixture came back as Tier 0 from every provider**, "We need to measure
  the p ninety five not just the median", and "The postgres database uses
  pgvector for the embeddings you know" from every endpoint model.
- **qwen2.5 3B's first request took 20 s** with the two smaller models still
  loaded on the server, and 1,667 ms in the control run with only Granite
  loaded. So 20 s is not its normal load time; whether the server's memory
  caused it is not known.
- **Quality is not ranked.** The fixtures list the terms that must survive, not
  the right punctuation, and accepted replies differ in ways the guard does not
  check: "What I mean is the VAD gates, the stream, not the other way round."
  (qwen2.5 3B), "What I mean is the Vad gates, the stream not the other way
  round." (0.5B), and "The Ring Buffer Sits Between Capture and the Vad" with
  no full stop (Apple). Read by eye, Granite's accepted replies had the fewest
  such slips, for example "The Whisper model is much slower than Parakeet on
  this machine." and "The notarisation step runs in CI, not locally.", but that
  is a reading, not a score. Scored against a reference punctuation later the
  same day, Granite came out best ("Punctuation and capitals against a
  reference", below).

## The dictionary in the prompt (2026-09-11)

`macalla enhance` now applies the dictionary as dictation does: its
corrections before Tier 0, and its written forms appended to the prompt as
known terms. These runs used the seed, 14 terms, with the same fixtures and
scoring as the runs above without it. The terms counted are the 10 fixture
terms the seed spells, matched as whole words in the seed's case. The endpoint
rows compare run 1 with and without the dictionary; the Apple Intelligence row
is a separate A/B on one build, dictionary off against on, in the order A, B,
B, A.

| Provider | Terms spelled, final text | Left as Tier 0 | p50 | Slowest |
|---|---|---|---|---|
| qwen2.5 0.5B | 9 → 10 of 10 | 2 → 3 | 158 → 166 ms | 196 → 228 ms |
| qwen2.5 1.5B | 7 → 10 | 4 → 3 | 336 → 350 ms | 468 → 449 ms |
| qwen2.5 3B | 6 → 10 | 2 → 4 | 648 → 628 ms | 828 → 734 ms |
| granite4.1 3B | 9 → 10 | 3 → 2 | 697 → 696 ms | 1,022 → 998 ms |
| Apple Intelligence, plain text | 7 → 10 | 3 → 5 | 558 → 577 ms | 738 → 1,464 ms |

- **The dictionary does the spelling before any model sees the text.** Tier 0
  with the dictionary's corrections had all 10 terms spelled for every
  provider, against 4 without, so every final text had them too.
- **It costs the endpoint models no measurable time.** Apple Intelligence's
  slowest request doubled, in both of its dictionary runs.
- **What it does to the rest of the reply depends on the model.** granite4.1
  3B stopped rewriting "Postgres" as "PostgreSQL", so that sentence passed the
  guard; qwen2.5 1.5B got two sentences past the guard that it had not; qwen2.5
  3B returned two sentences with no punctuation; Apple Intelligence was
  rejected twice as often (4 against 2) and title-cased whole sentences, such
  as "The Notarisation Step Runs In CI Not Locally".
- **Each model answered the same way twice** with the dictionary, 20 of 20.
  Apple Intelligence's replies with and without it matched on 11 of 20.

### Reasoning left on

One synthetic sentence sent straight to granite4.2:8b on the same Mac mini and
Ollama 0.34.0, the only model there that reports the thinking capability,
twice each way:

| Reasoning | Time | Tokens generated |
|---|---|---|
| The server's default | 112,484 and 105,593 ms | 1,140 |
| `reasoning_effort: "none"` | 1,569 and 1,396 ms | 15 |

Left on, reasoning made one short sentence take about 70 times as long. The
endpoint adapter now asks for no reasoning when the default prompt is in use,
and leaves it to the server for a prompt the person wrote. OpenRouter reports
that its listing of granite-4.2-8b reasons by default at high effort, and that
some models, such as gemini-3.5-flash-lite, cannot turn reasoning off; a server
that refuses the request is asked again without it.

## Punctuation and capitals against a reference (2026-09-11)

Each fixture in `bench/fixtures.json` now has a reference: its Tier 0 words,
punctuated and capitalised as the default prompt asks, with the seed
dictionary's spellings. `bench/enhance_ab_score.py` scores the final text, which
is the reply, or the Tier 0 text when the guard rejected the reply. It counts
words with the wrong capitals, and after each word a mark that is missing,
added where none belongs, or the wrong one. The runs in the two sections above
were rescored from their saved replies; nothing was sent again.

The references are one reader's judgement, and some choices are left open. A
comma is optional in six fixtures. Between the two clauses of the last fixture
a full stop, semicolon, colon or dash is accepted. The case of "p",
"accessibility", "tailnet" and the "that" after that break is not scored.

Later the same day the references began to accept a dash in place of the comma
before "not" in three fixtures, and "ninety-five" in the ninth, and the scorer
began to split words at a hyphen or dash as the guard then did ("After the
guard change", below). Rescored with both, every run in this section kept its
score.

| Provider | Dictionary | All right | Wrong capitals | Marks missing | Marks added or wrong |
|---|---|---|---|---|---|
| Tier 0 alone | off | 0 of 20 | 17 | 27 | 0 |
| Tier 0 alone | on | 0 | 9 | 27 | 0 |
| qwen2.5 0.5B | off | 12 | 7 | 7 | 1 |
| qwen2.5 0.5B | on | 13 | 5 | 8 | 0 |
| qwen2.5 1.5B | off | 11 | 10 | 7 | 2 |
| qwen2.5 1.5B | on | 14 | 4 | 6 | 1 |
| qwen2.5 3B | off | 9 | 9 | 5 | 3 |
| qwen2.5 3B | on | 11 | 4 | 7 | 1 |
| granite4.1 3B | off | 16 | 5 | 6 | 0 |
| granite4.1 3B | on | 17 | 2 | 4 | 1 |
| Apple Intelligence, plain text | off | 10 | 13 | 7 | 0 |
| Apple Intelligence, plain text | on | 11 | 19 | 12 | 0 |

- **Granite 4.1 3B scores best**, 16 and 17 of 20, at about 700 ms p50 on the
  Mac mini. qwen2.5 0.5B, the only model under 300 ms, gets 12 and 13.
- **Tier 0 alone gets none right.** It ends no sentence with a full stop.
- **The models leave marks out far more often than they add wrong ones.** Wrong
  capitals and missing marks are most of every provider's errors.
- **The dictionary in the prompt helped every provider by one to three
  fixtures.** It cut the endpoint models' wrong capitals, but Apple
  Intelligence's rose from 13 to 19, because it title-cased whole sentences.
- **No provider got the ninth fixture right**: every one left "We need to
  measure the p ninety five not just the median" as Tier 0. Some of those
  replies wrote "ninety-five", which the guard then counted as a changed word
  ("After the guard change", below). Only granite4.1 3B without the dictionary
  got the last one right, with a dash.
- **Every run scored the same as its repeat.**

These fixtures are unpunctuated text. Apple SpeechTranscriber and Parakeet
already punctuate, and on their output every change a model made was damage
("The real chain, on real engine output"). So this ranks providers for an
engine that does not punctuate, and for the synthetic sentences here; it says
nothing about real dictation.

## Transcription through Ollama (2026-09-11)

Ollama 0.34.0 answers the OpenAI transcription request,
`POST /v1/audio/transcriptions` with a multipart audio file, though its
OpenAI-compatibility page does not list it. It needs a model that declares
audio: of the gemma4 tags checked, only `gemma4:e2b-mlx` (NVFP4, 7.5 GB) does,
while the library page lists text and image input for the others. Run on the
same Mac mini (M1, 16 GB) over the network, with the three synthetic fixtures
that have references in `bench/wer-input.json`, converted to 16 kHz mono WAV,
and scored with `bench/engine-wer.py` beside the transcripts already there.
The Parakeet and Canary 180M columns are the transcribe.cpp builds Macalla runs,
from "Nemotron 3.5 ASR streaming 0.6B" and "Canary 180M Flash and Whisper, the
small models" below; until 2026-09-12 their clinical figures were the earlier
MLX and sherpa-onnx runs.

| Fixture | Audio | Apple | Parakeet | Canary 180M | gemma4:e2b-mlx | First request | Second |
|---|---|---|---|---|---|---|---|
| control-harvard | 25.0 s | 0.025 | 0.000 | 0.013 | 0.000 | 9,344 ms | 6,661 ms |
| tech-terms | 11.3 s | 0.200 | 0.133 | 0.200 | 0.067 | 13,639 ms | 4,709 ms |
| clinical | 32.4 s | 0.281 | 0.234 | 0.203 | 0.219 | 11,036 ms | 9,699 ms |

- **Word error rates in the same range as the local engines,** best on the
  technical terms and, on the clinical file, behind Canary 180M and ahead of
  Parakeet, where it
  misheard drug names ("atavastin", "oxyroxin", "amiprazole").
- **Too slow to be the live engine.** A second request took 4.7 to 9.7 s for 11
  to 32 s of audio, network included.
- **The reply's style varies.** Two came back lowercase and unpunctuated with
  "uh", "um" and "you know" kept; the Harvard sentences came back punctuated.
- **Synthetic speech only, three files.** Real speech has reversed a synthetic
  result before, so this says the route works, not how the model hears a
  person.
- **Not taken.** Ollama stays a text provider only; the model was removed after
  this run.

## Cloud models for cleanup (2026-09-11)

The 20 fixtures, the seed dictionary (14 terms), the default prompt and the
strict guard, scored against the reference as in "Punctuation and capitals
against a reference" above, this time sent to cloud providers.
`bench/enhance_endpoint.sh` sent each model's first request, then the 20
fixtures twice. OpenAI, Google's Gemini and Mistral were reached through their
OpenAI-compatible chat completions APIs, and Anthropic through its Messages API
(`macalla enhance --anthropic`). `macalla` read each key from the environment
variable named by `--key-env`. Times are from the reference machine and
include the network.

For the default prompt the adapters ask for the least reasoning a provider
allows: `reasoning_effort: "none"` on the OpenAI-compatible APIs, and thinking
disabled at temperature 0 on Anthropic's. A provider that refuses with HTTP 400
is asked again without the setting. The last column marks the models that
refused; each did so once per run.

| Provider | Model | All right of 20 | p50 | p95 | Rejected by the guard | Refused |
|---|---|---|---|---|---|---|
| OpenAI | gpt-5.6-luna | 19, 19 | 934, 841 ms | 1,107, 1,093 ms | 0, 1 | no |
| Anthropic | claude-haiku-4-5 | 18, 18 | 780, 808 ms | 847, 1,018 ms | 0, 0 | no |
| Google | gemini-3.5-flash-lite | 18, 17 | 628, 620 ms | 730, 817 ms | 1, 1 | reasoning off |
| OpenAI | gpt-4.1-nano | 18, 16 | 642, 624 ms | 948, 753 ms | 0, 0 | reasoning off |
| Google | gemini-3.1-flash-lite | 17, 17 | 695, 744 ms | 993, 1,087 ms | 0, 0 | no |
| Mistral | mistral-small-latest | 17, 17 | 363, 360 ms | 690, 415 ms | 2, 2 | no |
| Anthropic | claude-sonnet-5 | 16, 16 | 1,308, 1,344 ms | 1,606, 1,751 ms | 3, 3 | temperature 0 |
| Google | gemini-2.5-flash-lite | 15, 15 | 588, 558 ms | 715, 640 ms | 3, 3 | no |
| OpenAI | gpt-5.4-nano | 14, 14 | 878, 682 ms | 1,421, 1,035 ms | 0, 0 | no |
| Mistral | ministral-8b-latest | 11, 11 | 434, 470 ms | 828, 673 ms | 6, 6 | reasoning off |
| Mistral | ministral-3b-latest | 11, 11 | 233, 250 ms | 481, 512 ms | 3, 3 | reasoning off |

For comparison, the same fixtures with the dictionary, not through a cloud
provider. The two Ollama rows are the runs in "Punctuation and capitals against
a reference"; the Apple Intelligence row is today's prompt in "Three cleanup
prompts", below.

| Model | Runs on | All right of 20 | p50 | p95 | Rejected by the guard |
|---|---|---|---|---|---|
| granite4.1 3B | Ollama, Mac mini M1, 16 GB | 17, 17 | 696, 710 ms | 871, 849 ms | 2, 2 |
| qwen2.5 0.5B | Ollama, Mac mini M1, 16 GB | 13, 13 | 166, 158 ms | 224, 202 ms | 3, 3 |
| Apple Intelligence, plain text | reference machine | 11, 11 | 596, 574 ms | 785, 753 ms | 4, 4 |

- **gpt-5.6-luna got the most right**, 19 of 20 in both runs, at 841 and 934 ms
  p50. claude-haiku-4-5 came next, 18 in both, at about 800 ms.
- **Nothing that scores above 13 is under the 300 ms budget.**
  ministral-3b-latest is the only cloud model under 300 ms at p50, and it scores
  11, the same as Apple Intelligence; at p95 it takes 481 and 512 ms. qwen2.5
  0.5B on the Mac mini is faster and scores 13. With every other row, Tier 0
  pastes first and history catches up.
- **mistral-small-latest scores with granite4.1 3B**, 17 of 20, in about half
  the time: 360 ms p50 against about 700 ms.
- **The larger or newer model is not always the better one here.**
  claude-sonnet-5 scored 16 against claude-haiku-4-5's 18, and the guard
  rejected three of its replies a run. gpt-5.4-nano scored 14 against
  gpt-4.1-nano's 16 and 18.
- **A modest machine running Ollama is in the same range as the cloud.**
  granite4.1 3B on an M1 Mac mini with 16 GB scored 17, level with
  mistral-small-latest and gemini-3.1-flash-lite, and above five of the eleven
  cloud models. The text still leaves the machine being dictated on, so a server
  is opt-in like a cloud provider.
- **Most models gave the same replies twice**: 20 of 20 for seven models, 19 for
  gpt-5.6-luna, and 18 for gpt-4.1-nano, gemini-3.5-flash-lite and
  claude-sonnet-5.
- **Four models refused `reasoning_effort: "none"`** and were asked again
  without it. For gemini-3.5-flash-lite that means it ran with its default
  thinking, since OpenRouter's model list reports that its reasoning cannot be
  turned off ("Reasoning left on", above). claude-sonnet-5 refused temperature
  0 and ran with thinking disabled and no temperature set.
- **A model's first request took 381 to 2,939 ms.**

### What the replies look like

Two fixtures, as each model returned them in its first run. "Tier 0 kept" marks
a reply the guard rejected, so the Tier 0 text stayed. The Ministral and qwen2.5
0.5B rows come from the runs after the guard change; the rest used the guard as
it was before.

The third fixture. Raw transcript: `so basically uh what i what i mean is the vad
gates the stream not the other way round`. Tier 0: `What i mean is the vad gates
the stream not the other way round`. The reference accepts an optional comma
after "is", and a comma or a dash after "stream": `What I mean is(,) the VAD
gates the stream{,—–} not the other way round.`

| Model | Final text |
|---|---|
| gpt-5.6-luna | What I mean is, the VAD gates the stream, not the other way round. |
| gpt-4.1-nano | What I mean is, the vad gates the stream, not the other way round. |
| claude-haiku-4-5 | What I mean is, the VAD gates the stream, not the other way round. |
| claude-sonnet-5 | What I mean is, the VAD gates the stream, not the other way round. |
| gemini-3.5-flash-lite | What I mean is the VAD gates the stream, not the other way round. |
| mistral-small-latest | What I mean is, the VAD gates the stream, not the other way round. |
| ministral-8b-latest | What I mean is, the vad gates the stream—not the other way round. |
| ministral-3b-latest | What I mean is the vad gates the stream—not the other way round. |
| granite4.1 3B | Tier 0 kept |
| qwen2.5 0.5B | Tier 0 kept |
| Apple Intelligence | Tier 0 kept |

The last fixture. Raw transcript: `like um the enhancement never blocks the paste
that is the whole point`. Tier 0: `Like the enhancement never blocks the paste
that is the whole point`. The reference wants a full stop, semicolon, colon or
dash between the two clauses, and no mark between "the enhancement" and its
verb.

| Model | Final text |
|---|---|
| gpt-5.6-luna | Like the enhancement never blocks the paste, that is the whole point. |
| gpt-4.1-nano | Like the enhancement, never blocks the paste; that is the whole point. |
| claude-haiku-4-5 | Like the enhancement, never blocks the paste. That is the whole point. |
| claude-sonnet-5 | Like the enhancement never blocks the paste, that is the whole point. |
| gemini-3.5-flash-lite | Like the enhancement never blocks the paste, that is the whole point. |
| mistral-small-latest | Like the enhancement, never blocks the paste. That is the whole point. |
| ministral-8b-latest | Like the enhancement never blocks the paste—that is the whole point. |
| ministral-3b-latest | Like the enhancement never blocks the paste, that is the whole point. |
| granite4.1 3B | Like the enhancement never blocks the paste, that is the whole point. |
| qwen2.5 0.5B | Like the enhancement never blocks the paste that is the whole point. |
| Apple Intelligence | Tier 0 kept |

Five models joined the two clauses with a comma, three put a comma between "the
enhancement" and "never blocks", and qwen2.5 0.5B put nothing there at all. Of
the eleven, only ministral-8b-latest matches the reference. The reference is one
reader's judgement; a reader who accepts the comma splice would count five more.

### Apple Intelligence on the reference machine

With the dictionary and prompt A or B, Apple Intelligence got 11 of 20 right in
every run that finished on the reference machine that day: in "Punctuation and
capitals against a reference", in all four runs of those two prompts in "Three
cleanup prompts", below, and in the run after the guard change. Its count did not move. One further pass is not
counted: the system model took 7.2 to 8.2 s to answer a short warm-up request
six times running, and then six of the first seven fixtures ran past the 10 s
limit.

Memory straight afterwards: 980 MB of the 2 GB swap file in use, about 3.7 GB
held by the compressor, and 69,892 swap-outs in the nine hours since boot. That
snapshot came after the failure, so it does not show that memory caused it.

These rows describe one 16 GB M1 Pro, and macOS only. Other platforms have no
Apple Intelligence, so nothing here says how cleanup compares on Windows or
Linux.

### After the guard change

Later the same evening the strict guard stopped counting a hyphen or dash
between two spoken words as a changed word: "ninety-five" for "ninety five", or
"stream—not" for "stream not". Until then it rejected such a reply and kept the
Tier 0 text. The weak models were run again with the new guard and compared with
their runs of the same prompt in "Three cleanup prompts", below, about half an
hour earlier.

| Model | All right, old guard | All right, new guard | Rejected, old guard | Rejected, new guard |
|---|---|---|---|---|
| ministral-3b-latest | 11, 11 | 13, 13 | 3, 3 | 1, 1 |
| ministral-8b-latest | 12, 11 | 14, 14 | 5, 6 | 2, 2 |
| qwen2.5 0.5B | 13, 13 | 13, 13 | 3, 3 | 2, 2 |
| Apple Intelligence, plain text | 11, 11 | 11 | 4, 4 | 4 |

- **Fixture by fixture, the new guard let through fixtures 3, 9 and 20** from
  both Ministral models, and fixture 9 from qwen2.5 0.5B.
- **Not all of the gain is the guard.** Fixture 4 changed between the runs for
  reasons that have nothing to do with dashes, and fixture 3 came back with
  "vad" in lower case, which the reference still counts as wrong. The guard
  accounts for about one more fixture right per run on ministral-3b-latest and
  two on ministral-8b-latest.
- **qwen2.5 0.5B's recovered reply had a comma too many**, "We need to measure
  the p, ninety-five, not just the median.", so its count did not move.
- **Apple Intelligence gained nothing.** Its second pass is the row above; its
  first is the one not counted.
- **The cloud models in the first table were not run again.** Their rejections,
  0 to 3 a run, may include replies like these.

## Three cleanup prompts (2026-09-11)

A test of whether a different default prompt does better on the models that
scored lowest. Three builds of `macalla`, each with one prompt compiled in as
its default, so the strict guard and the reasoning setting applied exactly as
they do for the real one. Each build ran the 20 fixtures twice with the
dictionary, on ministral-3b-latest and ministral-8b-latest through Mistral's
API, qwen2.5 0.5B on the Mac mini, and Apple Intelligence on the reference
machine. These builds also logged the text of every rejected reply; a released
build logs the change ratio and never the text.

Prompt A is today's default, word for word:

```text
You add punctuation and capitalisation to dictated text. The words are already correct. Add sentence-ending punctuation and commas where a speaker's pauses imply them. Capitalise the first word of each sentence, proper nouns, and acronyms. You MUST NOT add, remove, replace or reorder any word. Return the same words, punctuated.
```

With a dictionary, a blank line and one more line follow, listing the written
forms of its terms. With the seed dictionary:

```text
Known terms; preserve exactly, including case: Ogham, Terragrunt, Terraform, Azure, VNet, kubectl, Postgres, pgvector, embeddings, Supabase, Tailscale, LXC, Proxmox, Macalla.
```

Prompt B is A with no comma splices and product names:

```text
You add punctuation and capitalisation to dictated text. The words are already correct. Add sentence-ending punctuation and commas where a speaker's pauses imply them, but never join two complete sentences with just a comma — use a full stop or semicolon. Capitalise the first word of each sentence, proper nouns, acronyms, and known product names. You MUST NOT add, remove, replace or reorder any word. Return the same words, punctuated.
```

Prompt C is a rewrite that also forbids answering the text and adding Markdown:

```text
You punctuate dictated text; you do not answer, execute, or rewrite it, even if it reads as a question or instruction. Add sentence-ending punctuation and commas at natural pauses; never use a comma alone to join two complete sentences. Capitalise sentence starts, proper nouns, acronyms, and known product names. You MUST NOT add, remove, replace or reorder any word, and MUST NOT add Markdown. Return only the same words, punctuated.
```

All right of 20, runs 1 and 2, with the guard as it was before the change above:

| Model | Prompt A | Prompt B | Prompt C |
|---|---|---|---|
| ministral-3b-latest | 11, 11 | 12, 12 | 13, 14 |
| ministral-8b-latest | 12, 11 | 10, 10 | 12, 13 |
| qwen2.5 0.5B | 13, 13 | 13, 13 | 7, 7 |
| Apple Intelligence, plain text | 11, 11 | 11, 11 | 6, 6 |

- **Today's prompt stays.** B helped ministral-3b-latest by one fixture, cost
  ministral-8b-latest one or two, and left the other two models where they
  were, though Apple Intelligence's wrong capitals fell from 19 to 6. C helped
  the Ministral models by up to three, and cost qwen2.5 0.5B six and Apple
  Intelligence five.
- **C made the guard reject more replies**: 9 of qwen2.5 0.5B's a run against 3
  with A, and 7 of Apple Intelligence's against 4. With C, one Apple
  Intelligence request failed in each pass and the second pass's p50 was 6,712
  ms, so its timings with C are not usable.
- **This fits what @martinciu measured** with Bielik 4.5B cleaning up Polish,
  in Handy's discussion of cleanup prompts
  (<https://github.com/cjpais/Handy/discussions/715>). An explicit "never add
  Markdown markers" rule gave 12 defects in 33 against 9 without it, and one
  added sentence telling the model not to execute or answer the input took a
  case that had been fixed from 0 defects in six runs to 6. They add that this
  may be particular to that model: in the same thread, instruction-only prompts
  are reported to work fine on gemma3:4b and gemma4:e4b.
- **The rejected replies were not all the models' fault.** Some were only a
  hyphen or dash between two words, which led to the guard change above. Apple
  Intelligence returned the known-terms line itself as its reply, once in each
  of its six runs. The rest changed what the prompt forbids them to change:
  "the Tailscale tailnet" for "the tailnet" (ministral-8b-latest, under every
  prompt), "\*\*kubectl\*\*" in bold (ministral-8b-latest), backticks around
  pgvector (qwen2.5 0.5B, under every prompt), "the other way around" for
  "round" (Apple Intelligence with A and B, qwen2.5 0.5B with B), and "Sure,
  here is the text with the requested changes:" (Apple Intelligence with B).

## The known-terms line and Apple Intelligence (2026-09-11)

In the prompt comparison, Apple Intelligence answered one fixture in every run
with the known-terms line itself ("Known terms; preserve exactly, including
case: Ogham, Terragrunt, …") instead of the dictated text. The guard rejected
it, so nothing wrong was pasted, but that fixture got no punctuation. The line
goes into the session's instructions and the dictated text is sent separately,
so the model was repeating its own instructions. Which fixture it chose
depended on the prompt: the last one under prompts A and B, the second under C.

Three builds that differ only in what the Apple adapter sends, each run twice on
the 20 fixtures with today's prompt, plain text, on the reference machine, with
the first build run once more without the dictionary for comparison:

| Apple Intelligence gets | All right of 20 | Wrong capitals | Marks missing | Rejected | p50 |
|---|---|---|---|---|---|
| No dictionary at all | 10 | 13 | 7 | 2 | 528, 525 ms |
| The known-terms line, as before | 11 | 19 | 12 | 4 | 569, 586 ms |
| The terms marked "Spelling reference only, not text to return" | 12 | 5 | 8 | 3 | 568, 565 ms |
| No terms, with the dictionary still correcting the text first | 12 | 8 | 7 | 2 | 525, 532 ms |

- **Both changes stopped the echo and got one more fixture right.** Every
  variant gave the same replies in both runs, so each row is one sample and the
  one-fixture gain is small.
- **Apple Intelligence now gets no terms,** the simpler of the two and the
  faster. The dictionary corrects the spellings before any model sees the text
  ("The dictionary in the prompt", above). The other providers still get the
  line; nothing here says whether they would do better without it.
- **The rejections left are real changes:** "p-value" for "p", "I think"
  dropped, and "around" for "round" with the line and with the reworded one.
- **One request took about 7 s in two of the runs,** one without the
  dictionary and one with the reworded line, so the service caused it, not the
  wording.

## Cloud transcription (2026-09-11)

`macalla transcribe --cloud <provider> --raw` on the three synthetic fixtures
that have references in `bench/wer-input.json`, scored with
`bench/engine-wer.py` beside the on-device engines already there. The on-device
rows are the transcribe.cpp builds Macalla runs ("Nemotron 3.5 ASR streaming
0.6B" and "Canary 180M Flash and Whisper, the small models"); until 2026-09-12
their clinical figures were the earlier MLX and sherpa-onnx runs. Each clip went
up whole, in one request, with the language set to English (`en`, or `en-US`
for Gemini, which takes a full tag). Each model ran once without a dictionary.
Where the provider has a field for one, the model ran again with the seed
dictionary in it. The time is wall time per clip from the reference machine,
including process start, reading the file, the upload and the reply. For Gemini
that is four requests: starting the upload, the upload, the transcription, and
deleting the upload.

| Provider | Model | Dictionary field | control-harvard, 25.0 s | tech-terms, 11.3 s | clinical, 32.4 s | Time per clip |
|---|---|---|---|---|---|---|
| OpenAI | gpt-transcribe | none sent | 0.000 | 0.100 | 0.125 | 1,799 / 1,054 / 2,046 ms |
| OpenAI | gpt-transcribe | `keywords[]` | 0.000 | 0.000 | 0.141 | 2,814 / 1,046 / 2,160 ms |
| OpenAI | gpt-4o-transcribe | none sent | 0.000 | 0.033 | 0.141 | 3,491 / 1,250 / 3,596 ms |
| OpenAI | gpt-4o-mini-transcribe | none sent | 0.013 | 0.033 | 0.141 | 1,625 / 1,202 / 1,848 ms |
| Google | gemini-3.5-transcribe | none sent | 0.000 | 0.133 | 0.141 | 6,883 / 6,937 / 7,159 ms |
| Google | gemini-3.5-transcribe | `custom_vocabulary` | 0.000 | 0.000 | 0.141 | 6,419 / 6,590 / 6,691 ms |
| Mistral | voxtral-mini-latest | none sent | 0.000 | 0.133 | 0.219 | 1,621 / 826 / 2,455 ms |
| Mistral | voxtral-mini-latest | `context_bias` | 0.000 | 0.067 | 0.219 | 1,448 / 1,067 / 1,851 ms |
| Cohere | cohere-transcribe-03-2026 | none | 0.000 | 0.133 | 0.203 | 1,585 / 991 / 3,600 ms |
| ElevenLabs | scribe_v2 | none sent | 0.000 | 0.000 | 0.078 | 1,953 / 1,097 / 2,707 ms |
| ElevenLabs | scribe_v2 | `keyterms` | 0.000 | 0.000 | 0.078 | 1,897 / 1,321 / 2,178 ms |
| on device | Apple SpeechTranscriber | | 0.025 | 0.200 | 0.281 | |
| on device | Parakeet TDT v3 | | 0.000 | 0.133 | 0.234 | |
| on device | Canary 180M Flash | | 0.013 | 0.200 | 0.203 | |

- **With the dictionary in its field, a provider got the technical terms
  right.** gpt-transcribe with `keywords[]` and gemini-3.5-transcribe with
  `custom_vocabulary` scored 0.000 on tech-terms, spelling "Terragrunt" and
  "kubectl". Without it they wrote "Terraform" and "terra grant" for Terragrunt,
  and both wrote "kube control". voxtral-mini-latest with `context_bias` fixed
  "Terragrunt" and still wrote "cube control".
- **The dictionary did not help the clinical file, and had nothing to offer
  it:** the seed holds no clinical terms. gemini-3.5-transcribe and
  voxtral-mini-latest scored the same with and without it, and gpt-transcribe
  got slightly worse, from 0.125 to 0.141.
- **scribe_v2 made the fewest errors on every clip**, with or without
  `keyterms`: 0.000, 0.000 and 0.078. It spelled "Terragrunt" and "kubectl"
  with no dictionary, so these clips cannot show what `keyterms` adds; the
  request with them was accepted and brought back the same words, differing
  only by a final full stop on control-harvard. Its five errors on the clinical
  file are four fillers it kept ("um", "uh", "uh", "um"), which the reference
  leaves out, and "dyspnea" for the reference's "dyspnoea": the Americanised
  spelling the clinical script counts as a real error
  (`fixtures/dictation-script-clinical.md`).
- **The on-device engines hold their own on the technical clip and trail on
  the clinical file.** Parakeet's 0.133 on tech-terms equals
  gemini-3.5-transcribe, voxtral-mini-latest and cohere-transcribe-03-2026
  without a dictionary. On the clinical file every cloud model beat Parakeet's
  0.234; Canary 180M's 0.203 tied cohere-transcribe-03-2026 and beat
  voxtral-mini-latest's 0.219. Rerun through `macalla transcribe --raw` on
  2026-09-12, Parakeet scores 0.219 on that file, one word fewer, which ties
  voxtral-mini-latest; that transcript is saved in `bench/wer-input.json`.
- **Most clips took 0.8 to 3.6 s; Gemini's took 6.4 to 7.2 s.** None of this is
  meant to be the live engine. A cloud engine gets the whole recording when the
  key is released, and the on-device engine shows the words while the person is
  still speaking.
- **Gemini's uploads were deleted after use.** Google otherwise keeps an upload
  for 48 hours; after these runs the account listed no files.
- **One request per clip**, so nothing here shows variance. Cohere ran on a
  trial key limited in requests per minute.
- **Not measured:** transcription through OpenRouter.

The tech-terms clip, as each cloud model returned it. The reference:

```text
I ran Terragrunt against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running.
```

| Model | Dictionary field | Transcript |
|---|---|---|
| gpt-transcribe | none sent | I ran Terraform against the Azure landing zone, then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kube control showed the pods running. |
| gpt-transcribe | `keywords[]` | I ran Terragrunt against the Azure landing zone, then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running. |
| gpt-4o-transcribe | none sent | I ran Terraform against the Azure landing zone, then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running. |
| gpt-4o-mini-transcribe | none sent | I ran TerraGrant against the Azure landing zone, then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running. |
| gemini-3.5-transcribe | none sent | I ran terra grant against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well and kube control showed the pods running. |
| gemini-3.5-transcribe | `custom_vocabulary` | I ran Terragrunt against the Azure landing zone, then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running. |
| voxtral-mini-latest | none sent | I ran Terra Grunt against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and Cube Control showed the pods running. |
| voxtral-mini-latest | `context_bias` | I ran Terragrunt against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and cube control showed the pods running. |
| scribe_v2 | none sent | I ran Terragrunt against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running |
| scribe_v2 | `keyterms` | I ran Terragrunt against the Azure landing zone. Then Terraform applied the VNet and the HCL was valid. We deployed to AWS as well, and kubectl showed the pods running |

Word error rate ignores case and punctuation, so "Terra Grunt" is two wrong
words and the comma splice after "landing zone" costs nothing.

### Caveats, for cleanup and transcription alike

- **Synthetic input only**: 20 typed fixtures and three clips spoken by `say`.
  Real speech has reversed a synthetic result before ("Real human speech, and
  the finding it overturned").
- **One or two runs a model, on one evening.** A provider can change the model
  behind a name, and a `-latest` name says so. ministral-3b-latest gave the same
  replies twice in 20 of 20 in one run, and in 18 of 20 about half an hour
  later.
- **Times are one connection's**, from the reference machine to each provider.
- **A build ran on the reference machine for 33 seconds during three OpenAI
  cleanup runs**: both gpt-4.1-nano runs and gpt-5.4-nano's second. They were
  not slower than the clean runs, but they were not measured on a quiet machine.
- **The cleanup fixtures are unpunctuated text.** The on-device engines
  punctuate already, so these rows rank cleanup for an engine that does not
  ("Punctuation and capitals against a reference").
- **Cost was read for one provider only.** Mistral billed €0.0057 for the
  Voxtral transcription runs above (six clips, 137 s of audio) and €0.0039 for
  that day's cleanup requests to its three text models, the prompt comparison
  and the rerun included. The other providers' bills were not read.

## Note on the Apple Intelligence setting

Decided 2026-09-08: Apple Intelligence stays **on for testing** and **off for
day-to-day use**.

This is not a footnote — it decides backend priority. It means the llama.cpp
Tier 1 path is not a hypothetical fallback for other people's machines; it is
**the author's own normal state**. FoundationModels is the measured backend,
but llama.cpp is the one that has to be good, and Spike B's shortlist run
matters more than its "part 2" framing suggests.

It also means dogfooding happens on both paths, which is the right way to find
out whether the `Enhancer` trait's opposite session lifecycles actually hold
up.

---

## Parakeet TDT v3 — transcribe.cpp versus sherpa-onnx

Measured 2026-09-09 on the reference machine. The question: which library
should load Parakeet, and later the other non-Apple engines, given that
transcribe.cpp claims most of the roadmap in one dependency. Evaluated, not
adopted on a README.

### Method

The same thirteen audio fixtures as every other engine measurement here,
converted to 16 kHz mono WAV with `afconvert`. Each file was run twice through
the same loaded model in one process; the second pass is the number reported,
so a cold page cache is not charged to the engine. Peak RSS is `ru_maxrss` at
exit. Both probes are small Rust programs against the published crates, not
the C++ CLIs, because the crate is what Macalla would link.

| | transcribe.cpp | sherpa-onnx |
|---|---|---|
| Crate | `transcribe-cpp` 0.2.3 (safe) + `-sys` | `sherpa-onnx` 1.13.7 (official; `sherpa-rs` is deprecated) |
| Native library | compiled from source in the crate (ggml, 27 MB of C++ in the tarball); needs CMake and a C++ toolchain | prebuilt static archives downloaded from GitHub releases at build time; **no checksum in build.rs** |
| Model | `parakeet-tdt-0.6b-v3-Q8_0.gguf`, 740 MB, from handy-computer on Hugging Face | `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`, 640 MB unpacked, from k2-fsa releases |
| Licence | MIT | Apache-2.0 |
| Acceleration on macOS | Metal (default feature) | CPU only in practice: the `coreml` provider changed nothing |

### Speed, ms of compute per second of audio

| Fixture | audio | tcpp Metal | tcpp CPU | sherpa CPU 4 thr | sherpa CPU 8 thr |
|---|---:|---:|---:|---:|---:|
| clinical | 32.4 s | 21.2 | 51.7 | 83.7 | 87.3 |
| control-harvard | 25.0 s | 18.3 | 52.4 | 82.1 | — |
| human take 2 | 69.7 s | 20.7 | 55.5 | 88.0 | — |
| kevin-technical | 66.3 s | 19.0 | 55.3 | 87.9 | — |
| three-sentences | 9.4 s | 17.6 | 59.7 | 83.8 | 89.1 |
| short-burst | 0.6 s | 48.5 | 103.0 | 123.3 | — |

Across all thirteen: transcribe.cpp on Metal 16–21 ms/s (short-burst aside),
on CPU 50–64 ms/s; sherpa-onnx 82–88 ms/s whatever the thread count. A
ten-second dictation segment costs roughly 180 ms, 550 ms and 850 ms
respectively. Only the first fits under the 300 ms budget without the
segment-overlap trick, and only on the GPU.

### Load, memory, binary

| | tcpp Metal | tcpp CPU | sherpa |
|---|---:|---:|---:|
| Model load, warm | 310 ms | 372 ms | 930–1150 ms |
| Model load, first ever on the machine | **11.6 s** | — | — |
| RSS after load | 871 MB | 1126 MB | 1125 MB |
| Peak RSS over the run | 954 MB | 1652 MB | 1540–1869 MB |
| Probe binary, static, release | 4.0 MB | 3.2 MB | 26.3 MB |
| Time to build the native library, 8 cores | 22–29 s | 0 (download) | |

The 11.6 s first load is the embedded Metal shader library compiling; macOS
caches the result and every later launch was 0.3 s. It has to be paid once per
machine and once per library version, and a first-launch wizard should expect
it rather than look hung. Peak RSS on the Metal path stays under the 1 GB the
cross-platform default was sized for in the Tier 1 bake-off; the CPU paths do
not.

### Accuracy, against the references already in `bench/wer-input.json`

| Fixture | Apple | Parakeet (MLX, earlier) | tcpp Q8_0 | sherpa int8 |
|---|---:|---:|---:|---:|
| control-harvard | 0.025 | 0.000 | 0.013 | 0.013 |
| tech-terms | 0.200 | 0.133 | 0.133 | 0.133 |
| clinical | 0.281 | 0.188 | 0.234 | 0.250 |

Same model, same errors, within a word or two of the MLX run. On the ten
fixtures without a reference the two backends agree word for word on six and
differ on the rest mostly in compounding: transcribe.cpp writes `TerraGrunt`,
`kubectl`, `VNet`, `Tailscale`, `PostgreSQL` where sherpa's int8 build writes
`Terra Grunt`, `cube CTL`, `V Net`, `Tail Scale`, `PostgreS`. Both transcribe
fillers (`Um`, `uh`), as the earlier Parakeet measurement found, so Tier 0
keeps its job on this path. Both runs are deterministic: pass 1 equals pass 2
on every file.

### Two long-input failures, one each

- **sherpa-onnx dropped the last twenty seconds of the 70 s recording**: three
  sentences gone, no error. The same file cut into two 35 s halves transcribes
  completely. The 66 s file was fine, so the limit is somewhere around a
  minute of continuous audio.
- **transcribe.cpp dropped one sentence in the middle of the 66 s recording**
  ("So the latency budget is 300 milliseconds") on Metal and on CPU alike, and
  sherpa kept it. Deterministic, so it is the Q8_0 decode on that audio rather
  than a backend race.

Under dictation neither matters much: batch engines get VAD-sized segments of a
few seconds. For file transcription both matter, and the fix is the same either
way, chunk at silences and never hand a backend a minute of audio in one call.

### Gotchas met on the way

- **Command Line Tools with a stale libc++ directory.** Every C++ compile on
  the reference machine failed with `'mutex' file not found`. The cause is
  `/Library/Developer/CommandLineTools/usr/include/c++/v1` holding three
  leftover headers from 2022, which clang searches before the SDK's complete
  copy and does not fall through. Workaround without touching the toolchain:
  `CXXFLAGS="-isystem $(xcrun --show-sdk-path)/usr/include/c++/v1"`. Any
  ggml-based dependency (this one, whisper-rs, llama-cpp-2) hits it, and so
  will the macOS CI runner, which is the same laptop.
- **transcribe.cpp prints decoder statistics to stderr** on every run. The
  Rust crate exposes `disable_logging`, which the engine wrapper must call once
  at startup or the desktop log fills with it.
- **sherpa-onnx's `coreml` provider is accepted and ignored**, at least for
  this model: identical timings to CPU and nothing in its log.

### Reading

transcribe.cpp is 4.5× faster than sherpa-onnx on the GPU and 1.6× faster on
the CPU, at 60 % of the peak memory and a sixth of the binary size, with the
same accuracy, and it is the only one of the two that can meet the paste
budget for a ten-second segment. It compiles from source under our control
where sherpa's crate downloads unverified binaries. Against it: it is five
months old, its Rust binding is marked "in development", the build needs
CMake and a working C++ toolchain on every platform and CI runner, and one
mid-file sentence drop needs watching. Another open-source app ships it in production, which is
the strongest signal available for something this young.

Streaming is not a distinguishing feature between them for Parakeet TDT: it is an
offline model in both. transcribe.cpp does offer streaming variants
(`parakeet-unified-en`, the Nemotron streaming models) through the same API,
which is where a live-preview path off macOS would come from.

---

## The VAD gate, on real dictations

Measured 2026-09-09 at the laptop, Apple SpeechTranscriber, the
MacBook Pro microphone, `macalla --debug listen --hotkey` with and without
`--vad`. The same sentence each time: "The latency budget is 300 milliseconds.
We enhance each segment while you are still speaking." The question was
whether the gate, a stage added to the path the 300 ms budget measures, moves
release-to-text.

| Take | Gate | Audio | VAD | Release to text |
|---|---|---:|---:|---:|
| 1 | off | 7.2 s | — | 249 ms |
| 2 | off | 14.2 s | — | 162 ms |
| 3 | on | 6.1 s | 3 ms | 246 ms |
| 4 | on | 17.5 s | 7 ms | 105 ms |
| 5 | on | 6.5 s | 3 ms | 107 ms |
| 6 | on, two seconds of silence first | 8.5 s | 3 ms | 231 ms |
| 7 | on, nothing said for three seconds | 3.4 s | 1 ms | 12 ms, "no speech was detected" |

The words came back identical on every take; the only differences were the
engine's own, a comma for a full stop and "you're" for "you are", with or
without the gate. The gate costs 1 to 7 ms and the spread between takes is
ten times that, so on the budget the result is null. On the two cases it
exists for it is not: two seconds of leading silence did not cost a word, and
a silent hold was answered by the gate in 12 ms without the engine being
asked, which is what makes "no speech was detected" honest rather than
inferred from empty text. **The gate is on by default from this measurement.**

## Long files, from two minutes to an hour

Measured 2026-09-10 on a MacBook Pro 18,3 (Apple M1 Pro, 6 performance and 2
efficiency cores, 16 GB), macOS 26.6.2, release build, `macalla --debug
transcribe` with Apple SpeechTranscriber. The question was whether transcribe
survives an hour-long recording, and what it holds in memory to do it.

The audio is synthetic, and that limits what it can say. It is this
project's own specification with the markdown stripped, read by `say -v
Daniel -r 175`, the voice and rate the latency fixtures use: 2.5, 13.1 and
62.3 minutes, as AIFF and as AAC in an `.m4a` made with `afconvert`. One clear
voice, no room, no crosstalk, no second speaker. The word counts below show
whether text survived a long session, not how accurate the engine is on a
real meeting. Peak memory is the maximum resident set size from
`/usr/bin/time -l`. Apple's recogniser runs in its own process,
`localspeechrecognition`; it was sampled once a second and is reported apart.

Apple segments its input itself, so every file below reached it as one
session.

| File | Decoded by | Wall | Macalla peak | Recogniser peak | Words out, of source |
|---|---|---:|---:|---:|---:|
| 2.5 min AIFF | Macalla, before the fix below | 4 s | 92.8 MB | 133.1 MB | 381 of 400 |
| 13.1 min AIFF | Macalla, before the fix below | 29 s | 392.3 MB | 169.7 MB | 1,938 of 2,000 |
| 13.1 min m4a | ffmpeg | 17 s | 144.5 MB | 166.6 MB | 1,937 of 2,000 |
| 2.5 min m4a | Apple's decoder | 3 s | 56.9 MB | 129.0 MB | 380 of 400 |
| 13.1 min m4a | Apple's decoder | 15 s | 199.4 MB | 167.1 MB | 1,939 of 2,000 |
| 62.3 min m4a | ffmpeg | 82 s | 601.7 MB | 176.0 MB | 9,160 of 9,488 |
| 62.3 min m4a | Apple's decoder | 79 s | 832.7 MB | 172.0 MB | 9,169 of 9,488 |

Word counts are `wc -w` on both sides. Most of the gap is how numerals and
symbols are spoken and written back rather than dropped speech: compared as
lowercase words, every run kept 99 per cent or more. Every run exited cleanly
with nothing logged as an error, and the last twelve words of the source were
at the end of every long transcript. The only miss was "notarised" coming
back as "notarized". Recognition ran at 47 to 55 times real time, 79.5 s for
the hour.

**Transcribe takes an hour-long file, in one session, on every decode path.**

### The native reader was the expensive path

The WAV and AIFF reader, the one path that needs no tool at all, was the worst
of the three. It held four whole-file copies at once, and the resampler it
handed the file to consumed its input from the front of a buffer holding all
of it. That is quadratic: the 13-minute file is 5.3 times the 2.5-minute one
and took 28.6 times as long to decode. Both are fixed, the reader by
converting and resampling a block at a time from the file's own bytes, the
resampler by taking its input a few chunks at a time. Same files, same
machine:

| AIFF | Peak before | Peak after | Decode before | Decode after |
|---|---:|---:|---:|---:|
| 2.5 min | 92.8 MB | 50.6 MB | 440 ms | 41 ms |
| 13.1 min | 392.3 MB | 178.1 MB | 12,601 ms | 210 ms |

The same number of words came out before and after. The live microphone path
shares the resampler but pushes a few hundred samples at a time, and never
saw either problem.

### What an hour costs, and why the line is not flat

An hour of decoded 16 kHz float audio is about 228 MB. The two hour-long runs
peaked at 601.7 MB and 832.7 MB, several times that, for three reasons found
in the code rather than guessed from the numbers:

- The engine's input queue had no bound, until the change below. A file was
  pushed as fast as it could be read, far faster than the recogniser consumes
  it, so most of the file waited in the queue. Dictation pushes in real time
  and never builds one.
- Apple's decoder builds the whole file as one array, copies it into a second
  buffer to hand across, and the Rust side copies it once more.
- The ffmpeg reader's vector grows by doubling, which briefly holds the old
  buffer beside the new one.

All of it is inside the 2.5 GB active-memory budget. It is not flat, though.
In a straight line from the 13-minute and hour-long points, which is an
extrapolation and not a measurement, a three-hour file would peak near 1.7 GB
through ffmpeg and 2.3 GB through Apple's decoder. Bounding the queue is the
largest single saving and does not change the engine's interface.

### Bounding the queue

The engine's input queue now holds at most 30 seconds of converted audio, and
a push waits for room, so a file is fed no faster than it is recognised. Same
hour-long file, same machine. Memory here was sampled every half second with
`ps` for the resident set size, so the before peaks differ slightly from the
`/usr/bin/time -l` figures above.

| 62.3 min m4a | Peak before | Peak after | Recognition before | Recognition after |
|---|---:|---:|---:|---:|
| ffmpeg | 603.0 MB | 289.6 MB | 75.4 s | 81.2 s |
| Apple's decoder | 833.7 MB | 490.5 MB | 72.0 s | 74.6 s |

Before, both runs filled the queue within a few seconds of decoding and held
about 600 MB until it drained: through ffmpeg until between 10 and 20 s,
through Apple's decoder until between 40 and 50 s. After, there is no plateau.
Apple's decoder still peaks at 490.5 MB at 2.5 s, the moment decoding ends
and its copies of the file exist together. From 4 s on it stayed between
196.3 and 312.1 MB, against 226.8 to 611.0 MB before.

The transcripts before and after were byte-identical on both paths.
Recognition took 2.6 and 5.8 s longer, one run each. The same hour took 79.5 s
through ffmpeg the day before, so the difference is inside the run-to-run
spread and is not shown to be a cost.

The resident set size is a lower bound between the peaks. Through ffmpeg it
dipped to 91.5 MB at 16.5 s, less than the decoded audio alone, which means
macOS was compressing pages. It then moved by around 100 MB for the rest of
the run. That movement is not the queue, and it has not been looked into.

### The decoder's copies

Apple's decoder used to build the whole file as a Swift array, copy it into a
second buffer to hand across, and have the Rust side copy that again. It now
hands each converted chunk straight across with the length the file is
expected to decode to, and the Rust side sizes one buffer from that and fills
it. Decoding alone, through `decode_file` in a small program under
`/usr/bin/time -l`:

| File | Samples | Peak before | Peak after |
|---|---:|---:|---:|
| 13.1 min m4a | 12,610,841 | 111.4 MB | 63.2 MB |
| 62.3 min m4a | 59,801,219 | 472.0 MB | 243.7 MB |

The sample count and a hash of every sample's bytes were the same before and
after, for both files and for the 11-second tech-terms fixture. An hour of
decoded audio is about 228 MB, so what is left is one copy.

In a whole transcribe, Apple's decoder no longer spikes as decoding ends: 261.9
MB at 2 s, where it had been 489.1 MB. Transcripts were byte-identical to the
run before. The run's peak moved rather than fell, though. Memory climbed
steadily through recognition on both paths, by about 150 MB over the hour, to
411.3 MB through ffmpeg and 416.2 MB through Apple's decoder. The ffmpeg path's
code did not change between the two runs, and it peaked at 289.6 MB in the
first, most likely because macOS compressed pages in that run and not in this
one. Sampled resident size is therefore a poor way to compare runs, and the
climb during recognition is now the largest thing left.

### The climb was autoreleased objects

The Swift side is called from Rust threads, and such a thread has no
autorelease pool that drains while it runs. Objects that Foundation and
AVFoundation autoreleased during every push and every poll therefore stayed
for as long as the thread lived. Each call now drains a pool of its own. Same
hour-long file, measured this time by the peak memory footprint from
`/usr/bin/time -l`, which counts compressed pages:

| 62.3 min m4a | Peak footprint before | Peak footprint after |
|---|---:|---:|
| ffmpeg | 399.1 MB | 246.5 MB |
| Apple's decoder | 399.7 MB | 246.5 MB |

The run before the change happened while the machine was busy, with
recognition taking 208 and 217 s rather than about 75, so its resident-size
curve cannot be compared. Against the earlier unloaded run instead, by
sampled resident size through Apple's decoder: memory rose from 267.9 MB to
416.2 MB during recognition before, and from 253.4 MB to 262.0 MB after, with
recognition taking 72.1 and 72.7 s. Transcripts were byte-identical. What is
left at the peak is about one copy of the decoded audio.

The desktop app's dictation thread is also a Rust thread, and it lives as
long as the app, so the same objects built up there across dictations. That
was not measured in the app.

---

## Nemotron 3.5 ASR streaming 0.6B, against Parakeet and Apple

Measured 2026-09-10 on the reference machine. The question: is Nemotron 3.5,
a multilingual model that streams natively, the live engine that Linux and
Windows lack? Parakeet TDT v3 is offline by construction, so off macOS text
arrives only at each pause.

### Method

The file is handy-computer's transcribe.cpp conversion,
`nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf`, 751,094,240 bytes, SHA-256
checked. It loads through the `transcribe-cpp` 0.2.3 crate Macalla already
uses, as architecture `parakeet`, variant `nemotron-3.5-asr-streaming-0.6b`,
on Metal, and reports streaming support and 32 locales. It takes a locale
tag: `en-US` works, and a bare `en` is refused as an unsupported language.

A small Rust program ran one engine per process under `/usr/bin/time -l`, one
run at a time. One-shot runs went through each file twice and the second pass
is reported; every engine gave identical text on both passes. Streams took
100 ms chunks, once as fast as the engine would take them and once paced at
real time. Long files went through `macalla transcribe --raw` for Parakeet and
Apple, and through the stream for Nemotron. Every English fixture here is a
synthetic `say` voice, and so are the four non-English clips, written for this
run and read by Anna, Thomas, Mónica and Alice.

### Accuracy, against the references in `bench/wer-input.json`

`tech-terms-2` has no entry there; its reference is the expected text in
`fixtures/transcripts/tech-terms-2.json`.

| Fixture | Nemotron 3.5 | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| control-harvard | 0.000 | 0.000 | 0.025 |
| tech-terms | 0.133 | 0.133 | 0.200 |
| tech-terms-2 | 0.190 | 0.286 | 0.381 |
| clinical | 0.219 | 0.234 | 0.281 |

Terms written exactly as expected, fifteen of each:

| | Nemotron 3.5 | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| Technical | 8 | 9 | 6 |
| Clinical | 7 | 10 | 7 |

None of the three wrote `Terragrunt`, `kubectl`, `pgvector`, `Supabase`,
`Tailscale` or `Ogham`, which is the dictionary's job on every engine.
Nemotron ran with `en-US` against a clinical list in British spelling
(`dyspnoea`), which may cost it some clinical terms; that was not separated
out.

Two habits show in the text rather than the score. Nemotron spells numbers
out, "three hundred milliseconds" where Parakeet writes "300", and it runs
some sentences together, "the smooth planks, glue the sheet".

### Streaming

The streamed text was identical to the one-shot text on all four fixtures,
fed fast and fed at real time. Paced at real time:

| | Nemotron 3.5 | Apple |
|---|---:|---:|
| First text, after speech starts | 1,168–1,191 ms | 4,102–4,105 ms |
| Final text, after the audio ends | 31–99 ms | 155–203 ms |

The first text lands at about 1.2 s on every fixture, which looks like a
fixed chunk rather than anything in the audio. The slowest single Nemotron
feed took 131 ms, once, against a 100 ms chunk.

One-shot compute is slower than Parakeet's: 983 ms for 25 s of audio against
727 ms, and 1,121 ms against 680 ms for the 32 s clinical fixture.

### Memory

Peak memory footprint, which counts compressed pages:

| | Nemotron 3.5 | Parakeet TDT v3 |
|---|---:|---:|
| Short fixtures | 925–950 MB | 870–890 MB |
| 2.5 min, one shot | 1,053.9 MB | |
| 13 min, streamed | 990.2 MB | |
| 62 min, streamed | 1,180.9 MB | |

Apple's recogniser runs in its own process, so its in-process figure is not
comparable and is left out.

### Other languages

Synthetic voices, scored with letters beyond a–z counted as letters. Apple
has only English and German installed on this machine.

| Language | Nemotron 3.5 | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| German | 0.000 | 0.000 | 0.000 |
| French | 0.000 | 0.000 | not installed |
| Spanish | 0.037 | 0.000 | not installed |
| Italian | 0.042 | 0.021 | not installed |

Nemotron dropped the opening word of the Spanish clip, "Por", and in French
ran two sentences together.

### Long files

The same synthetic readings as the long-file section above. Recall is the
share of the source's lowercase words that came back.

| File | Nemotron 3.5, streamed | Parakeet, `macalla transcribe` | Apple, `macalla transcribe` |
|---|---|---|---|
| 2.5 min | WER 0.106, recall 0.923 | WER 0.049, recall 0.956 | WER 0.085, recall 0.923 |
| 13 min | WER 0.167, recall 0.888 | failed | WER 0.124, recall 0.902 |
| 62 min | recall 0.871 | crashed | recall 0.899 |

Nemotron streamed the hour in 215.3 s and, in a second run, 219.1 s: about
17 times real time, with the same 9,807 words both times and a peak footprint
of 1,180.9 and 1,183.9 MB. Neither run had the machine to itself; the load
average was between 3.4 and 11 during the first and between 6.0 and 7.0
during the second.

Parakeet's failures are Macalla's, not the model's. `macalla transcribe` cuts
a file at the pauses for a batch engine, and this read speech never pauses
long enough: the 2.5 and 13 minute files each came out as a single segment,
so Parakeet was handed the whole file in one call. At 13 minutes the run
returned an error; at an hour it crashed in a Metal buffer allocation with a
peak footprint of 2,796.3 MB, over the 2.5 GB active budget. File
transcription needs a maximum segment length.

Fixed the same day. The VAD now caps a segment at 30 seconds, the longest a
live dictation runs. Speech that never pauses long enough to close the gate is
cut at the quietest 32 ms frame in the five seconds before the cap, and the
next segment starts exactly where that one ended, so no audio is dropped or
sent twice. The same readings through the same kind of release build, with
Parakeet:

| File | Segments | Wall | Peak footprint | WER | Recall |
|---|---:|---:|---:|---:|---:|
| 2.5 min | 6 | 3.3 s | 908.6 MB | 0.046 (0.049 as one call) | 0.956 |
| 13 min | 29 | 15.0 s | 1,020.8 MB | 0.083 | 0.937 |
| 62 min | 137 | 68.5 s | 1,573.7 MB | | 0.932 |

The load average was between 4.7 and 7.3, most of it the run itself. On both
long readings Parakeet is now the most accurate of the three engines in the
table above, at 52 to 55 times real time. The cut cost nothing on the short
file.

Peak memory still grows with the length of the file, though no segment does.
The likeliest cause, not yet measured, is that the whole decoded file and
every segment cut from it are held at once. An hour fits the 2.5 GB budget;
at this slope a recording of three hours or more would not.

#### Measured and fixed: three copies of the file became one

Measured on 13 September 2026 on a Mac mini (Apple M1, 16 GB, macOS 26.6.2),
not the M1 Pro above. Same readings, release builds, Parakeet, `macalla
transcribe --raw`, two runs per file, one run at a time. Peak footprint is
from `/usr/bin/time -l`, in MiB (bytes divided by 1,048,576). A probe also read
the process footprint after each stage. No wall times are given, because the
runs include model load and started just after a build.

The decoded hour at 16 kHz is 228.1 MiB. On the hour-long file, the footprint
rose by 229.4 MiB at decode and by 456.8 MiB more at segmenting. Cutting the
file held two further copies. A second probe measured the segments at 228.1
MiB. The other 228.7 MiB matches the VAD's frame buffer, which had taken the
whole recording in one push: nothing else in segmenting is that size, and a
unit test showed the buffer growing to the full length of a recording. Freeing them sooner does not lower the peak: dropping the VAD after
segmenting moved the footprint by 0.0 MiB, because freed memory stays in the
process's footprint. So the fix avoids making them. The VAD now takes the
audio about a second at a time and yields each segment as it ends, and each
segment is dropped once the engine returns.

| File | Peak before, MiB | Peak after, MiB |
|---|---:|---:|
| 2.5 min, AIFF | 893.7, 910.5 | 894.6, 893.8 |
| 2.5 min, m4a | 907.0, 906.9 | 892.1, 893.8 |
| 13 min, AIFF | 1,070.3, 1,069.7 | 972.1, 972.1 |
| 13 min, m4a | 1,021.9, 1,036.7 | 924.0, 939.6 |
| 62 min, m4a | 1,569.8, 1,586.2 | 1,120.5, 1,120.4 |

Segments are unchanged at 6, 29 and 137. Every transcript after the fix is
byte-identical to the one before it, on all five files.

One copy of the decoded file is left, about 228 MiB an hour, and for AIFF the
reader's file bytes still count after they are freed (33 MiB on the 13-minute
file). Decoding in chunks would remove both; it is not done. By extrapolation,
not measurement, a two-hour recording now peaks near 1.35 GB and a three-hour
file near 1.6 GB, both under the 2.5 GB budget.

Nemotron through `macalla transcribe`, once it became an engine: the file is
cut by the same VAD, at most 30 seconds a piece, and each piece is streamed on
its own rather than the whole file in one stream.

| File | Segments | WER | Recall | Peak footprint | One stream per file, above |
|---|---:|---:|---:|---:|---|
| 2.5 min | 6 | 0.111 | 0.918 | 971.5 MB | WER 0.106, recall 0.923 |
| 13 min | 29 | 0.174 | 0.880 | 1,089.4 MB | WER 0.167, recall 0.888 |

Cutting the file costs Nemotron under a point of WER and bounds every stream;
it does not close the gap to Parakeet on long readings. The 13-minute file
took 53.0 s of wall time, with the load average not recorded for that run.

### Reading

Nemotron is the first engine measured here that streams off macOS, and it
does so without giving up accuracy on short dictation: best or joint best on
all four reference fixtures, with text a second after speech starts where
Apple takes four, and final text inside 100 ms. It costs about the same
memory as Parakeet. Against it: one-shot compute runs 35–73 % slower than
Parakeet's, it spells numbers out and merges some sentences, it fell behind
Apple on the long readings, and it dropped a word from one of four
non-English clips.

---

## Granite Speech 4.1 2B, against Parakeet and Apple

Measured 2026-09-10 on the reference machine, with the same program, fixtures
and comparison runs as the Nemotron section above. The question: does a
speech-language model trained for keyword-biased recognition earn its size?

The file is handy-computer's transcribe.cpp conversion
`granite-speech-4.1-2b-Q4_K_M.gguf`, 1,602,904,800 bytes, SHA-256 checked. The
Q4_K_M quantisation was chosen because the Q8_0 file alone is larger than the
2.5 GB active budget. It loads as architecture `granite_speech` on Metal and
reports six languages (`en`, `fr`, `de`, `es`, `pt`, `ja`), no streaming, no
language detection, and a longest input of 377,400 ms.

Keyword biasing, the reason to try it, cannot be reached through
`transcribe-cpp` 0.2.3: its run options have no prompt, and the library
documents its initial-prompt feature as Whisper only. What was measured is
plain recognition.

### Accuracy

| Fixture | Granite 4.1 2B | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| control-harvard | 0.000 | 0.000 | 0.025 |
| tech-terms | 0.100 | 0.133 | 0.200 |
| tech-terms-2 | 0.238 | 0.286 | 0.381 |
| clinical | 0.188 | 0.234 | 0.281 |

Nemotron, above, scored 0.133, 0.190 and 0.219 on the last three.

Terms written exactly as expected: 5 of 15 technical, against Parakeet's 9 and
Apple's 6, and 12 of 15 clinical, against 10 and 7. It writes technical names
its own way, `PostgreSQL` for Postgres, `PGVector`, and `ProxMoxLXC` run
together, and it opened the clinical fixture with a word nobody said, "And the
patient". It writes numbers as figures, "500 mg".

The German, French and Spanish clips each came back word for word, but the
German lost most of its commas and full stops, and the Spanish starts in lower
case. Italian is not one of its languages.

### Cost

| | Granite 4.1 2B | Parakeet TDT v3 |
|---|---:|---:|
| 25 s fixture, compute | 3,397 ms | 727 ms |
| 32 s fixture, compute | 3,925 ms | 680 ms |
| Peak footprint, short fixtures | 2,201–2,262 MB | 870–890 MB |
| Load | 650–718 ms | 291–349 ms |

Across the four English fixtures Granite used 4.7 to 6.8 times Parakeet's
compute. The 2.5 minute reading, well inside its advertised longest input,
failed: "decode hit the context/generation cap before end-of-stream". Its peak
footprint on that run was 2,479.6 MB.

### Reading

Granite 4.1 is the most accurate of the three on two of the four fixtures and
the best on clinical terms. Against that, it uses several times Parakeet's
compute, reaches the 2.5 GB active budget with nothing else loaded, cannot
take a 2.5 minute file in one call, does not stream, and the keyword biasing
that made it a candidate is out of the runtime's reach. As things stand it is
not a dictation engine for Macalla.

---

## Canary 1B v2, against Parakeet and Apple

Measured 2026-09-10 on the reference machine, with the same program, fixtures
and comparison runs as the Nemotron section. The question: Canary 180M Flash
lost to Parakeet on languages and on dropping content from long audio. This
model has Parakeet's 25 languages, so does it now win?

The file is handy-computer's transcribe.cpp conversion
`canary-1b-v2-Q8_0.gguf`, 1,144,290,016 bytes, SHA-256 checked. It loads as
architecture `canary` on Metal and reports Parakeet's 25 languages, no
streaming, no language detection, and a longest input of 400,000 ms. The
machine was busy for this run, with a load average between 8.5 and 10.1, so
its compute figures are likely high.

### Accuracy

| Fixture | Canary 1B v2 | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| control-harvard | 0.000 | 0.000 | 0.025 |
| tech-terms | 0.133 | 0.133 | 0.200 |
| tech-terms-2 | 0.190 | 0.286 | 0.381 |
| clinical | 0.234 | 0.234 | 0.281 |

Terms written exactly as expected: 8 of 15 technical and 10 of 15 clinical,
against Parakeet's 9 and 10, and the clinical misses were the same five words
as Parakeet's. The German, French and Spanish clips came back word for word
with their punctuation, and Italian scored 0.021, the same as Parakeet.

### Cost

| | Canary 1B v2 | Parakeet TDT v3 |
|---|---:|---:|
| 25 s fixture, compute | 862 ms | 727 ms |
| 32 s fixture, compute | 1,089 ms | 680 ms |
| Peak footprint, short fixtures | 1,209–1,231 MB | 870–890 MB |
| Load | 408–433 ms | 291–349 ms |

Across the four English fixtures that is 1.2 to 1.8 times Parakeet's compute,
on a busier machine. The 2.5 minute reading, inside its advertised longest
input, failed on the decoder's cap: "decode hit the context/generation cap
before end-of-stream". transcribe.cpp limits a Canary call's transcript to the
decoder's maximum position, 1,024 tokens by default, so Canary needs a file cut
into short segments, which `macalla transcribe` does not yet guarantee.

### Reading

Canary 1B v2 matches Parakeet rather than beating it: the same languages, the
same licence, the same clinical score, one technical term fewer and one
fixture better. It costs more compute and about 340 MB more memory, and a
single call cannot take a 2.5 minute file. It gives Macalla nothing Parakeet
does not already, so it is not taken.

---

## Qwen3-ASR 0.6B, against Parakeet and Apple

Measured 2026-09-10 on the reference machine, with the same program, fixtures
and comparison runs as the sections above. The question: this is the smallest
multilingual candidate, close to Parakeet in file size, but it lacks nine of
Parakeet's European languages. Does its accuracy make up for that?

The file is handy-computer's transcribe.cpp conversion
`Qwen3-ASR-0.6B-Q8_0.gguf`, 850,423,456 bytes, SHA-256 checked. It loads as
architecture `qwen3_asr` on Metal and reports 30 languages with language
detection, no streaming through transcribe.cpp, and a longest input of
5,218,560 ms. Against Parakeet's 25 it lacks Bulgarian, Croatian, Estonian,
Latvian, Lithuanian, Maltese, Slovak, Slovenian and Ukrainian. The machine was
very busy for this run, with a load average between 10.3 and 14.9, so its
compute figures are likely high.

### Accuracy

| Fixture | Qwen3-ASR 0.6B | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| control-harvard | 0.000 | 0.000 | 0.025 |
| tech-terms | 0.200 | 0.133 | 0.200 |
| tech-terms-2 | 0.095 | 0.286 | 0.381 |
| clinical | 0.141 | 0.234 | 0.281 |

Its tech-terms-2 and clinical scores are the lowest of any engine measured
here. Terms written exactly as expected tell a different story: 8 of 15
technical, against Parakeet's 9, and it is the only engine that missed
`Terraform`; 9 of 15 clinical, against Parakeet's 10. A lower error rate on
the clinical fixture did not mean more of its listed terms written exactly.

The French and Spanish clips came back word for word. German scored 0.037,
including "Am Norden" for "Am Morgen", and Italian 0.021, with one word
dropped.

### Cost

| | Qwen3-ASR 0.6B | Parakeet TDT v3 |
|---|---:|---:|
| 25 s fixture, compute | 1,225 ms | 727 ms |
| 32 s fixture, compute | 1,489 ms | 680 ms |
| Peak footprint, short fixtures | 1,467–1,478 MB | 870–890 MB |
| Load | 445–484 ms | 291–349 ms |

That is 1.7 to 2.4 times Parakeet's compute, on a heavily loaded machine. The
2.5 minute reading failed on the generation cap, as Granite and Canary did,
although it advertises 87 minutes.

### Reading

Qwen3-ASR scores best of anything measured here on two of the four fixtures,
the clinical one among them. Against it: nine of Parakeet's languages are
missing, it takes about 590 MB more memory and roughly twice the compute on
this run, it does not stream through the runtime, and a 2.5 minute file fails
in one call. It is not taken now. The clinical result is the one to remember.

### The transcribe.cpp candidates side by side

File sizes are bytes divided by a million; memory is the peak footprint as
`/usr/bin/time -l` reports it. Canary and Qwen3-ASR ran on a busier machine,
so their compute figures lean high. Apple is left out: it is a different
runtime, and its recogniser's memory is in another process.

| Model | File | Peak footprint, short fixtures | Compute, 25 s fixture | clinical WER | tech-terms-2 WER | Streams | 2.5 min in one call |
|---|---:|---:|---:|---:|---:|---|---|
| Parakeet TDT v3, shipped | 740 MB | 870–890 MB | 727 ms | 0.234 | 0.286 | no | yes |
| Nemotron 3.5 streaming | 751 MB | 925–950 MB | 983 ms | 0.219 | 0.190 | yes | yes |
| Granite Speech 4.1 2B | 1,603 MB | 2,201–2,262 MB | 3,397 ms | 0.188 | 0.238 | no | no |
| Canary 1B v2 | 1,144 MB | 1,209–1,231 MB | 862 ms | 0.234 | 0.190 | no | no |
| Qwen3-ASR 0.6B | 850 MB | 1,467–1,478 MB | 1,225 ms | 0.141 | 0.095 | no | no |
| Voxtral Mini 4B Realtime, below | 2,830 MB | 4,166–4,199 MB | 9,747 ms | 0.219 | 0.095 | yes | yes |

---

## Voxtral Mini 4B Realtime, against Parakeet and Apple

Measured 2026-09-10 on the reference machine, with the same program, fixtures
and comparison runs as the sections above. The question: Voxtral is the one
candidate whose smallest file is already larger than the 2.5 GB
active-memory budget. Is it accurate enough to be offered anyway, as a large model a
person opts into?

The file is handy-computer's transcribe.cpp conversion
`Voxtral-Mini-4B-Realtime-2602-Q4_K_M.gguf`, 2,830,493,984 bytes, SHA-256
checked. It loads as architecture `voxtral_realtime` on Metal in 1.2 to 1.6 s
and reports streaming, language detection, 13 languages (`en`, `fr`, `es`,
`de`, `ru`, `zh`, `ja`, `it`, `pt`, `nl`, `ar`, `hi`, `ko`) and no limit on
input length.

Fetching it was its own lesson. A single connection to the Hugging Face CDN
ran at 0.19 MB/s, about 223 minutes for the file; reconnecting gave 1.20 and
then 0.87 MB/s; eight byte ranges fetched in parallel, each dropped and retried
when it fell below 300 KB/s for 20 s, averaged about 3 MB/s over the whole
fetch.

### Accuracy

| Fixture | Voxtral Mini 4B | Parakeet TDT v3 | Apple |
|---|---:|---:|---:|
| control-harvard | 0.013 | 0.000 | 0.025 |
| tech-terms | 0.167 | 0.133 | 0.200 |
| tech-terms-2 | 0.095 | 0.286 | 0.381 |
| clinical | 0.219 | 0.234 | 0.281 |

Terms written exactly as expected: 10 of 15 technical, the most of any engine
measured, and 12 of 15 clinical, level with Granite and ahead of Parakeet's
10. It still missed `Terragrunt`, `kubectl`, `pgvector`, `Supabase` and
`Ogham`, which no engine wrote.

The German, French and Spanish clips came back word for word with their
punctuation, and Italian scored 0.042.

### Streaming

The streamed text was identical to the one-shot text on all four fixtures,
fed fast and fed at real time. Paced at real time:

| | Voxtral Mini 4B | Nemotron 3.5 | Apple |
|---|---:|---:|---:|
| First text, after speech starts | 1,473–2,374 ms | 1,168–1,191 ms | 4,102–4,105 ms |
| Final text, after the audio ends | 557–865 ms | 31–99 ms | 155–203 ms |

The paced run kept up overall, 83.7 s of wall time for about 79 s of audio,
but not smoothly: the slowest single 100 ms feed took 458 to 475 ms, so text
arrives in bursts. That is on this machine's GPU; a machine without one was
not measured and would have much less headroom.

### Cost

| | Voxtral Mini 4B | Parakeet TDT v3 |
|---|---:|---:|
| 25 s fixture, one-shot compute | 9,747 ms | 727 ms |
| 32 s fixture, one-shot compute | 11,893 ms | 680 ms |
| Peak footprint, one-shot on short fixtures | 4,166–4,199 MB | 870–890 MB |
| Peak footprint, streaming | 5,513–5,562 MB | |
| Load | 1,236–1,636 ms | 291–349 ms |

One-shot, that is 13 to 23 times Parakeet's compute. Streamed, it ran at 2.4
to 2.7 times real time: the 2.5 minute reading in 55.9 s, the 13 minute one in
314.2 s and the hour in 1,583.6 s.

### Long files

| File | Voxtral Mini 4B, streamed | Parakeet, `macalla transcribe` | Apple, `macalla transcribe` |
|---|---|---|---|
| 2.5 min | WER 0.031, recall 0.969 | WER 0.049, recall 0.956 | WER 0.085, recall 0.923 |
| 13 min | WER 0.069, recall 0.948 | failed | WER 0.124, recall 0.902 |
| 62 min | recall 0.945, 9,326 words for 9,314 | crashed | recall 0.899 |

Every one of those is the best of any engine measured. The same 2.5 minute
reading one-shot also scored WER 0.031, in a single call, where Granite,
Canary and Qwen3-ASR hit their generation caps. The hour began with the load
average at 12.9, so its time is more likely high than low.

### Reading

Voxtral is the most accurate engine measured here on long speech and on
technical terms, and it streams, with streamed text that matches its one-shot
text. The price is size and speed: a 2.83 GB download, a peak footprint of
5.5 GB while streaming, one-shot compute 13 to 23 times Parakeet's, and live
text that arrives in bursts, one and a half to two and a half seconds behind
speech, with the last words more than half a second after the speaker stops.
It keeps pace with speech on this machine's GPU; without a GPU it was not
measured. It earns a place as a large model a person chooses for accuracy,
with the download and the memory said plainly on its card, not as a default.

---

## Canary 180M Flash and Whisper, the small models

Measured 2026-09-10 on the reference machine, with the same program, fixtures
and comparison runs as the sections above. The question: Parakeet stays the
default off macOS, but someone who dictates only English, or has little
memory, may be better served by something smaller, and Whisper is the one
family here that takes a prompt. Every file is handy-computer's transcribe.cpp
conversion at Q8_0, SHA-256 checked: `canary-180m-flash` 218 MB,
`whisper-tiny.en` 46 MB, `whisper-base.en` 85 MB, `whisper-small.en` 270 MB,
`whisper-medium.en` 831 MB and the multilingual `whisper-medium` 832 MB.

The machine was very busy for most of this pass: the load average ran between
14 and 21 for Canary 180M, Whisper medium, tiny.en and base.en, about 6 for
small.en and under 2 for medium.en. Compute figures for the busy runs are
likely high; the medium.en ones are the cleanest.

### Accuracy without a prompt

| Fixture | Canary 180M | tiny.en | base.en | small.en | medium.en | medium | Parakeet |
|---|---:|---:|---:|---:|---:|---:|---:|
| control-harvard | 0.013 | 0.025 | 0.013 | 0.000 | 0.000 | 0.000 | 0.000 |
| tech-terms | 0.200 | 0.367 | 0.167 | 0.200 | 0.067 | 0.133 | 0.133 |
| tech-terms-2 | 0.286 | 0.429 | 0.333 | 0.286 | 0.190 | 0.190 | 0.286 |
| clinical | 0.203 | 0.375 | 0.297 | 0.250 | 0.234 | 0.219 | 0.234 |

### Whisper with the vocabulary as its initial prompt

Whisper accepts an initial prompt, and transcribe.cpp passes it through, so it
is the one family measured where the dictionary can steer recognition inside
the engine instead of correcting it afterwards. The prompt here was the thirty
terms the fixtures test, separated by commas: the best case, a dictionary that
already holds every word being checked.

| Fixture | tiny.en | base.en | small.en | medium.en | medium |
|---|---:|---:|---:|---:|---:|
| control-harvard | 0.025 | 0.013 | 0.000 | 0.000 | 0.000 |
| tech-terms | 0.100 | 0.033 | 0.000 | 0.000 | 0.000 |
| tech-terms-2 | 0.095 | 0.095 | 0.000 | 0.000 | 0.000 |
| clinical | 0.234 | 0.250 | 0.219 | 0.234 | 0.203 |

Terms written exactly as expected, technical then clinical, out of 15 each:

| | tiny.en | base.en | small.en | medium.en | medium |
|---|---|---|---|---|---|
| No prompt | 5, 6 | 7, 7 | 7, 10 | 8, 11 | 9, 11 |
| With the prompt | 12, 12 | 11, 11 | 14, 12 | 15, 13 | 15, 13 |

Without any prompt, Parakeet wrote 9 and 10, and Voxtral 10 and 12.

The control fixture, which contains none of the terms, scored the same with and
without the prompt on every size, so on this evidence the prompt did not push
its words into speech that lacked them. That is one fixture. A dictionary much
longer than thirty terms, one full of words the speaker is not using, and
silence were not tested.

### Cost

| | Canary 180M | tiny.en | base.en | small.en | medium.en | medium | Parakeet |
|---|---:|---:|---:|---:|---:|---:|---:|
| 25 s fixture, compute | 488 ms | 239 ms | 315 ms | 643 ms | 1,480 ms | 1,501 ms | 727 ms |
| Peak footprint, short fixtures | 290–308 MB | 137–141 MB | 194–197 MB | 430–431 MB | 1,108–1,109 MB | 1,105–1,114 MB | 870–890 MB |

The prompt changed neither compute nor memory by any consistent amount.

### Long audio and other languages

The 2.5 minute reading, one call each:

| | Canary 180M | tiny.en | base.en | small.en | medium.en | medium |
|---|---:|---:|---:|---:|---:|---:|
| WER | 0.827 | 0.057 | 0.052 | 0.057 | 0.054 | 0.044 |
| Words out, of 388 | 102 | 387 | 383 | 381 | 388 | 384 |

Canary 180M returned 102 words of 388 with no error at all: it stopped and said
nothing. Every Whisper size took the whole reading; Parakeet scored 0.049 on the
same file.

Multilingual Whisper medium transcribed the German, French and Spanish clips
word for word and Italian at 0.042. Canary 180M transcribed German and French
word for word but returned nothing for Spanish, one of its four languages.

### Reading

Canary 180M Flash is small and quick, but it silently dropped about three
quarters of a 2.5 minute file and returned nothing for Spanish, and on this
runtime its clinical score, 0.203, is not the 0.125 an earlier sherpa-onnx
measurement found. It is not taken.

Whisper is the most useful result of the day. Without a prompt, small.en and
medium.en are close to Parakeet. With the dictionary as a prompt, small.en and
both medium models wrote both technical fixtures word for word, and every
Whisper size took the 2.5 minute file whole. For English-only use off macOS,
small.en is the light choice at about 430 MB and medium.en the accurate one at
about 1.1 GB, and the multilingual medium covers languages Parakeet lacks at
the same cost. The prompt result is a best case, and needs a realistic
dictionary before it is claimed.

## Whisper as an engine: the dictionary as its prompt

The evaluation above prompted Whisper with exactly the thirty terms the
fixtures test: the best case. As an engine, Whisper's prompt is whatever the
person's dictionary holds, so it was measured again with dictionaries that do
not flatter it, through `macalla transcribe --raw`, which prints the engine's
text before the phonetic pass. Five dictionaries: none; Macalla's 14-term seed
(technical terms, no clinical ones); 120 terms with the thirty tested first
and 90 other tools after them; the same 120 with the tested thirty last; and
100 words from elsewhere entirely, cities, dishes, instruments, flowers and
painters. Six files: the four fixtures, 8.9 s of room tone broken by three
loud noise bursts, and 8 s of room tone alone.

Whisper keeps the last 223 tokens of a prompt and drops the front without
saying so. The engine instead fits whole terms in the dictionary's order and
stops at the first that does not fit: 51 to 66 of the long dictionaries'
terms, depending on the words and the model.

A bare comma list changed how Whisper writes, not only what it writes. With
the 100 unrelated words as a list, medium.en wrote the control fixture and
tech-terms-2 with no capital letters and no punctuation at all, and small.en
wrote the control fixture without a capital. The same words introduced as a
sentence, "Words that may come up: ...", left capitals and punctuation as they
were with no prompt. The engine sends that form, and the table is from it.

Word error rate, with the tested terms written exactly in brackets:

| Model | Dictionary | Terms in the prompt | control-harvard | tech-terms | tech-terms-2 | clinical |
|---|---|---|---|---|---|---|
| small.en | none | none | 0.000 | 0.200 (4/7) | 0.286 (3/8) | 0.219 (11/15) |
| small.en | seed, 14 terms | all 14 | 0.000 | 0.000 (6/7) | 0.000 (8/8) | 0.219 (11/15) |
| small.en | 120, tested first | 60 of 120 | 0.000 | 0.000 (7/7) | 0.048 (7/8) | 0.172 (14/15) |
| small.en | 120, tested last | 66 of 120 | 0.000 | 0.100 (4/7) | 0.190 (4/8) | 0.203 (12/15) |
| small.en | 100 unrelated | 51 of 100 | 0.000 | 0.133 (4/7) | 0.048 (6/8) | 0.203 (12/15) |
| medium.en | none | none | 0.000 | 0.067 (4/7) | 0.190 (4/8) | 0.250 (10/15) |
| medium.en | seed, 14 terms | all 14 | 0.000 | 0.000 (7/7) | 0.000 (8/8) | 0.234 (7/15) |
| medium.en | 120, tested first | 60 of 120 | 0.000 | 0.000 (7/7) | 0.000 (8/8) | 0.156 (15/15) |
| medium.en | 120, tested last | 66 of 120 | 0.000 | 0.067 (4/7) | 0.143 (6/8) | 0.219 (8/15) |
| medium.en | 100 unrelated | 51 of 100 | 0.000 | 0.067 (4/7) | 0.190 (3/8) | 0.234 (11/15) |
| medium | none | none | 0.000 | 0.133 (5/7) | 0.190 (4/8) | 0.266 (8/15) |
| medium | seed, 14 terms | all 14 | 0.000 | 0.000 (7/7) | 0.000 (8/8) | 0.250 (8/15) |
| medium | 120, tested first | 57 of 120 | 0.000 | 0.000 (7/7) | 0.000 (8/8) | 0.203 (14/15) |
| medium | 120, tested last | 61 of 120 | 0.000 | 0.133 (4/7) | 0.095 (5/8) | 0.250 (7/15) |
| medium | 100 unrelated | 52 of 100 | 0.000 | 0.200 (4/7) | 0.143 (5/8) | 0.250 (11/15) |

What that shows:

- **A dictionary that holds the words being said helps every size.** With
  the seed dictionary all three wrote both technical fixtures word for word,
  and with the long one medium.en wrote all fifteen clinical terms.
- **A term cut from the prompt stops helping.** With the tested terms last,
  most were cut, and tech-terms fell back towards its unprompted score.
- **Words in the prompt did not appear in speech that lacked them**, in any
  of the 90 runs, and an English prompt did not pull another language into
  English: multilingual medium wrote a German clip word for word with the
  seed prompt, as it did with none.
- **Room tone never reached the engine**; the voice-activity gate dropped it.
- **Noise bursts did, as two segments, and Whisper wrote words over them in
  13 of 15 runs, prompt or no prompt**: "Thanks for watching!", "you you",
  "Thank you for watching! you". Parakeet, Nemotron and Apple wrote nothing
  on the same file. With the bare-list prompt, multilingual medium once put a
  dictionary word into the noise ("and fastly. Thanks for watching."); with
  the sentence form none did. The guard for this is a voice-activity detector
  that rejects noise before any engine hears it, not the prompt.

The noise file is synthetic and the fixtures are synthetic voices, so these
numbers describe behaviour on those, not a person in a room.

## Apple's recognizers and the dictionary as context

Measured 2026-09-12 on the same MacBook Pro 18,3 (Apple M1 Pro, 16 GB), macOS
26.6.2, en-US, on synthetic voices only.

SpeechAnalyzer takes an `AnalysisContext`, whose `contextualStrings` exist to
steer recognition towards given words: the Apple half of the dictionary's
first layer, as Whisper's prompt is the Whisper half. It was tried two ways.
The first was Macalla's Apple engine, set up to hand the dictionary's written
forms to the analyzer before any audio arrived. The second was a standalone
probe that runs a file through SpeechTranscriber or DictationTranscriber. The
probe also tried DictationTranscriber with a custom language model built from
the same words (`SFCustomLanguageModelData` phrase counts, passed as
`customizedLanguage`).

### SpeechTranscriber, the engine Macalla ships, ignores them

Through `macalla transcribe --raw`, the engine's text before the phonetic
pass, with eight dictionaries:

- none;
- the 14-term seed;
- the 17 technical terms these clips contain;
- a 19-term personal dictionary;
- 120 terms, with the 17 first and again with them last;
- 100 unrelated words;
- the seed plus cache, queue and sync.

The engine sent at most 40 terms. Two dictionaries were run again with all
120 or all 100 sent.

There were ten clips:

- tech-terms and tech-terms-2;
- both homophone clips;
- control-harvard and clinical;
- eight technical terms said one at a time;
- control-harvard under pink noise;
- 8 s of pink room tone;
- noise bursts.

**All 100 transcripts with a dictionary matched the no-dictionary transcript
of the same clip byte for byte**, as did a second no-dictionary run. The
analyzer reported holding every string it was given, so they did arrive.
Through the probe, SpeechTranscriber wrote the same text with and without the
17 terms on all seven clips it was given.

### DictationTranscriber uses them, from further behind

Word error rate against the references in `bench/wer-input.json`, all three
columns through the probe:

| Clip | Words | SpeechTranscriber | DictationTranscriber | DictationTranscriber, 17 terms |
|---|---:|---:|---:|---:|
| tech-terms | 30 | 0.200 | 0.200 | 0.133 |
| control-harvard | 80 | 0.025 | 0.087 | 0.087 |
| control-harvard under noise | 80 | 0.025 | 0.163 | 0.163 |

What the 17 terms changed in DictationTranscriber's text:

- **tech-terms:** "Terra Grant", "Azio", "terraform" and "Vnet" became
  Terragrunt, Azure, Terraform and VNet. "tube control" stayed, and one "and"
  became "on".
- **tech-terms-2:** "post", "PG vector", "embers", "proximal" and "tail
  scale" became Postgres, pgvector, embeddings, Proxmox and Tailscale. The
  full stop between the two sentences went, and "Ogham", missing without the
  terms, stayed missing.
- **The terms said one at a time:** Terraform, VNet and Ollama appeared.
  "Teran" became "Telegram", "cube" became "tube", and "as your", for Azure,
  became "as you".
- **Unchanged:** control-harvard, control-harvard under noise and the
  homophone clip were word for word the same, and noise bursts produced no
  text either way.

### A custom language model

At the default weight the model built from the 17 terms did two things. On
tech-terms it wrote Terragrunt and nothing else new. On tech-terms-2 it wrote
Postgres, pgvector, embeddings, Proxmox and Tailscale, with "Superbase"
becoming "Super bass". Control-harvard was unchanged.

At weight 1.0, for the eight terms said one at a time, it wrote only
"Terragrunt Vnet kubectl Postgres". It wrecked ordinary speech:

- control-harvard: "The Staves radish rices often served rounds";
- the homophone clip: "A. R. Rahman going to deploy their changes Overthere".

The same damage came from 100 unrelated words.

### Homophones

Neither recognizer ever wrote both "cache" and "cash" correctly, nor
queue/cue or sync/sink, under any setting. "Cache" appeared only once: the
model at weight 1.0, built from a dictionary containing it, wrote "cache" for
both words, in a sentence it had already mangled.

### Reading

- **The engine Macalla ships cannot be steered towards the dictionary.** On
  Apple Speech, terms are corrected by the phonetic layer after the engine,
  and only for mishearings it lists.
- **DictationTranscriber can be steered, but starts further behind.** On
  these clips it made 7 errors on the clean control where SpeechTranscriber
  made 2, and 13 under noise. The terms fixed technical words and did not
  touch the rest.
- **A custom language model at full weight is not usable for dictation.**

## A speech detector that does not hear noise

Measured 2026-09-10 on the same MacBook Pro 18,3 (Apple M1 Pro, 16 GB), macOS
26.6.2. The energy detector passes anything loud enough, and in the section
above, noise it passed became text on Whisper. Two neural detectors were put
behind the same `Vad` rules (600 ms of hangover, 120 ms of speech to open,
300 ms of pre-roll, a 30 s cap) and given the same audio:

- **earshot 1.2.2**, pure Rust, its 40 kB of weights compiled in. It decides
  on 16 ms, so it was asked twice per 32 ms frame and the higher score counted.
- **Silero VAD v6**, its probabilities from its own ONNX model
  (`silero_vad_16k_op15.onnx`), run the way silero-vad's own wrapper runs it:
  64 samples of context and the state carried from frame to frame.

A frame is speech at or above the threshold: 0.5, which earshot suggests, and
0.35 as a looser setting.

### Noise

The noise file from the section above: 8.9 s of room tone with three loud
bursts.

| Detector | Segments passed | Audio passed |
|---|---:|---:|
| energy | 2 | 1.8 s |
| earshot, 0.5 | 0 | 0 |
| earshot, 0.35 | 1 | 0.5 s |
| Silero, 0.5 | 0 | 0 |
| Silero, 0.35 | 0 | 0 |

### Speech

Every detector cut the synthetic readings (tech-terms, three-sentences,
clinical, short-burst and the 2.5-minute reading) into the same segments.
Three human readings of the dictation script, one speaker in one room, were cut
differently:

| Reading | Length | energy | earshot, 0.5 | Silero, 0.5 |
|---|---:|---:|---:|---:|
| A, which stops after "embeddings" | 37.1 s | 6 segments, 31.5 s | 6, 24.9 s | 8, 25.5 s |
| B | 69.7 s | 6, 59.6 s | 7, 47.2 s | 10, 46.0 s |
| C | 66.4 s | 6, 61.3 s | 14, 51.7 s | 16, 51.9 s |

Energy kept 35 stretches that one of the models dropped. Given each on its
own, Parakeet wrote nothing on 9 and an interjection or a stock phrase on the
rest ("Mm-hmm", "Okay", "Thank you.", "What is it?"); Apple wrote nothing on
any of them.

To see whether any of that was words, everything outside each detector's
segments was silenced and the whole file transcribed by Parakeet with no
dictionary, so the engine kept its context. WER against the script as read
(116 words, 59 for reading A):

| Reading | energy | earshot, 0.5 | Silero, 0.5 | Silero, 0.35 |
|---|---:|---:|---:|---:|
| A | 0.203 | 0.169 | 0.186 | 0.186 |
| B | 0.155 | 0.138 | 0.147 | 0.164 |
| C | 0.172 | 0.138 | 0.190 | 0.181 |

The words that changed moved both ways: "pods" became "pot's" under earshot
and "pots" under Silero on reading A, and "sync" became "sink" under both on
reading B, which is what the script says. A copy of reading B 20 dB quieter
came back word for word the same under every detector. A copy with pink noise
mixed in changed 11 words against its unmasked transcript under each of
energy, earshot and Silero.

| Detector | Per 32 ms frame |
|---|---:|
| energy | 1.2–1.8 µs |
| earshot | 10.8–13.2 µs |

Those leave out the first file of each run, which paid for warming up (4.4 µs
and 19.8 µs).

Silero was not timed in Rust. Its ONNX model took 0.10–0.11 ms a frame from
Python, which says nothing about a port.

### earshot in Macalla

`macalla transcribe` from release builds before and after the change, the same
flags, the default dictionary.

| Input | Engine | Energy detector | earshot |
|---|---|---|---|
| noise file | Parakeet | 2 segments, no text | 0 segments |
| noise file | Whisper small.en, no dictionary | 2 segments, "Thanks for watching!" | 0 segments |
| noise file | Whisper small.en, seed dictionary | 2 segments, no text | 0 segments |
| reading A | Parakeet | WER 0.186 | 0.186 |
| reading B | Parakeet | WER 0.147 | 0.172 |
| reading C | Parakeet | WER 0.155 | 0.138 |

Across the three readings that is 46 errors in 291 words before and 47 after.

| File | Detector | Segments | Peak footprint | WER | Recall |
|---|---|---:|---:|---:|---:|
| 2.5 min | energy | 6 | 939.2 MB | 0.036 | 0.966 |
| 2.5 min | earshot | 6 | 940.4 MB | 0.036 | 0.966 |
| 13 min | energy | 29 | 1,102.0 MB | 0.083 | 0.937 |
| 13 min | earshot | 29 | 1,102.2 MB | 0.083 | 0.937 |
| 62 min | energy | 137 | 1,591.9 MB | | 0.933 |
| 62 min | earshot | 137 | 1,599.5 MB | | 0.933 |

The time from the model loading to the file being cut, the same work both
times apart from the detector, went from 0.25 s to 0.50 s on the 13-minute
reading and from 1.42 s to 2.61 s on the hour. The hour's wall time is not
compared: the run before shared the machine at a load average of 12.9, the run
after at 4.6.

What this does not cover: noise under a real voice (a café, typing while
talking), more than one speaker and room, and live dictation through the
gate.

## How long the gate waits before it closes

Measured 2026-09-10, same machine. Live dictation through earshot's gate on
Apple, Parakeet and Whisper small.en, held for about a minute at a time with
the gate phrase said at mixed speeds and a cough, was recorded alongside. On
slow, deliberate speech the 600 ms hangover closed the gate in the pauses the
energy detector had held open through breath, so a batch engine was handed
more, shorter pieces ("The Latency. Budgets. Is Three hundred Milliseconds.")
and a cough cut out on its own came back from Whisper as "Cough Cough".

Another open-source app, which has shipped voice-activity detection for a year,
was taken as the reference point. Read from its source (MIT): its default detector is Silero v4
at 0.3 on 30 ms frames, with earshot at 0.5 as an experimental option; a
smoothing layer keeps 450 ms before speech, needs 60 ms of speech to open, and
holds open 450 ms after speech for batch engines and 1,650 ms for streaming
ones; a batch engine is handed the recording's speech joined into one call. No
measurement behind those values was found in its repository.

A throwaway harness ran the real engines on the same audio under each setting.
The tuning set was the four live holds (the phrase four times each, 240
words); the held-out set was two slow holds (the phrase five and a half times,
82 words), the three human readings from the section above (291 words) and
the noise file. Errors are substitutions, deletions and insertions against
the text.

| Setting | Apple | Parakeet | Whisper small.en |
|---|---|---|---|
| energy, 300/120/600, pieces | 4 / 66 | 24 / 55 | 27 / 56, 3 words on noise |
| earshot, 300/120/600, pieces (before this change) | 9 / 71 | 25 / 65 | 21 / 61 |
| earshot, another app's 450/60/450, pieces | 7 / 66 | 43 / 71 | 16 / 68 |
| That app's shape: earshot, one call | 6 / 65 | 1 / 40 | 3 / 60 |
| Another app's default: Silero v4, one call | 6 / 64 | 1 / 48 | 4 / 56 |
| **earshot, 300/120/1,650, pieces** | **4 / 65** | **6 / 46** | **5 / 54** |
| no detector, the whole recording | 5 / 66 | 22 / 66 | 3 / 65, 3 words on noise |

Settings are milliseconds kept before speech / of speech needed to open /
of quiet before closing; cells are tuning / held-out errors. Apple, which
streams, hears what the gate forwards in every row. Every earshot and Silero
setting put no words on the noise file.

That app's timings on Macalla's pieces were the worst row for Parakeet: 60 ms to
open cut 88 pieces from the tuning holds where the default cut 71. That app's
one-call shape was the most accurate on the tuning holds, but it leaves the
whole dictation in flight when the key is released: 1.1 to 1.8 s of engine
time on these one-minute holds, against the 300 ms budget. Keeping Macalla's
pieces and lengthening only the hangover recovered nearly all of it.

The hangover alone, with 300 ms before speech and 120 ms to open:

| Hangover | Apple | Parakeet | Whisper small.en | Last piece at release, Parakeet / Whisper |
|---:|---|---|---|---|
| 600 ms | 9 / 71 | 25 / 65 | 21 / 61 | 165 / 340 ms |
| 1,200 ms | 6 / 69 | 8 / 53 | 5 / 62 | 332 / 404 ms |
| 1,500 ms | 5 / 66 | 6 / 50 | 5 / 56 | 302 / 369 ms |
| 1,650 ms | 4 / 65 | 6 / 46 | 5 / 54 | 274 / 442 ms |
| 2,000 ms | 4 / 66 | 6 / 44 | 4 / 57 | 310 / 426 ms |
| 2,500 ms | 5 / 66 | 5 / 44 | 4 / 56 | 347 / 443 ms |

Errors fall until about 1.5 s and are flat from there to 2.5 s; the
differences inside that range are the size of the difference between two runs
of the same setting (20 and 21 on Whisper). 1,650 ms is the shortest value at
which Parakeet's two slow holds reached their best (2 and 2 errors, against 4
and 3 at 1,500 ms), and it became the default. Keeping 450 ms before speech or
opening after 60 ms made no consistent difference at that hangover. The
release column is the engine time of each hold's last piece with all three
engines sharing the GPU, so it is higher than a dictation on its own; the
comparison between rows holds. On a batch engine a finished sentence now
reaches the engine about a second later during dictation; Apple streams and
is unaffected.

All of it is one speaker in one room.

With 1,650 ms as the default, `macalla transcribe` from release builds before
and after the change, the same flags and the default dictionary:

| Input | Hangover 600 ms | Hangover 1,650 ms |
|---|---|---|
| noise file, Parakeet, and Whisper small.en with and without the seed dictionary | 0 segments | 0 segments |
| three human readings, Parakeet | 47 errors in 291 words, 27 segments | 41 errors, 8 segments |
| 2.5 min reading | 6 segments, WER 0.036, recall 0.966 | the same |
| 13 min reading | 29 segments, WER 0.083, recall 0.937 | the same |
| 62 min reading | 137 segments, recall 0.933 | the same |

The long readings never pause long enough for either hangover to close the
gate, so the 30 s cap cuts them the same way. Peak footprint moved by less
than 20 MB. Wall times are not compared: the second run shared the machine
with the test suite.

## Two streams at once, for a live meeting (2026-09-13)

A live meeting transcribes two streams at the same time: the microphone,
labelled You, and the call's audio, labelled Others. The question was whether
one engine instance can serve both streams, or each needs its own.

Measured on a Mac mini, **not the reference machine**:

| | |
|---|---|
| Model | Mac mini, `Macmini9,1` |
| Chip | Apple M1, 8 cores (4 performance and 4 efficiency) |
| Memory | 16 GB |
| OS | macOS 26.6.2 (build 25G83) |

### How it was measured

- **The probe.** A throwaway probe, built in release mode with the
  repository's release profile against `macalla-core` and `macalla-engines`,
  fed two fixture recordings at the same time.
- **The recordings.**
  - `fixtures/audio/long/short.aiff`: a 148.2 s reading.
  - `fixtures/audio/human-gate-slow-on.aiff`: 131.3 s of one phrase repeated
    with long pauses, 88 words, which is why its real-time factors are low.
- **The feed.**
  - The audio was decoded before the clock started.
  - Each stream was fed in 100 ms chunks, pushed, polled and finished as the
    record loop does.
  - Apple went through its own stream, and Parakeet TDT v3 through the core's
    batch stream, cut by earshot.
  - The two streams ran on two threads released together, and each was timed
    from that release to its final text.
- **The runs.** Every scenario ran in its own process, five times, with 10 s
  between processes.

Apple's API opens a SpeechAnalyzer for each stream, so for Apple one engine
means one analyzer per stream. There is no analyzer to switch between them.
Recognition runs in a system process, `localspeechrecognition`, not in
Macalla's process, so that process's memory was sampled every 20 ms as well.

### Speed

Wall seconds per stream in runs 1 to 5, with the real-time factor (wall time
over audio length) in brackets. The one-stream rows ran each file on its own.

| Scenario | 148.2 s reading | 131.3 s repeated phrase |
|---|---|---|
| Apple, one stream | 2.39, 2.37, 2.36, 2.38, 2.38 (0.016) | 1.19, 1.19, 1.18, 1.18, 1.18 (0.009) |
| Apple, two streams | 2.68, 2.66, 2.65, 2.65, 2.69 (0.018) | 1.57, 1.52, 1.51, 1.55, 1.54 (0.012) |
| Parakeet, one stream | 4.42, 4.34, 4.20, 4.19, 4.20 (0.028 to 0.030) | 2.07, 1.94, 1.91, 1.92, 1.91 (0.015 to 0.016) |
| Parakeet, one load, two streams | 4.87, 4.93, 4.80, 4.80, 4.81 (0.032 to 0.033) | 6.13, 6.16, 6.04, 6.03, 6.06 (0.046 to 0.047) |
| Parakeet, two loads, two streams | 5.39, 5.38, 5.26, 5.30, 5.26 (0.035 to 0.036) | 4.33, 5.02, 4.93, 4.97, 4.92 (0.033 to 0.038) |

- **One Parakeet load.** The two streams take turns on the model, so the
  second file waited for the first: 6.03 to 6.16 s against 1.91 to 2.07 s on
  its own.
- **Two loads.** No faster in total on this M1, where both streams share the
  GPU either way.

### Memory

Peak footprint is the kernel's lifetime peak for the process. Resident size,
in brackets, was sampled every 20 ms. Runs 1 to 5.

| Scenario | Macalla's process: peak footprint (resident) | `localspeechrecognition`: peak footprint |
|---|---|---|
| Apple, one stream, reading | 18, 19, 18, 18, 18 MiB (32) | 43, 44, 43, 43, 44 MiB |
| Apple, one stream, repeated phrase | 16, 15, 15, 16, 15 MiB (30) | 50 MiB every run |
| Apple, two streams | 29 MiB every run (42 to 43) | 75, 78, 79, 75, 78 MiB |
| Parakeet, one stream, reading | 901, 900, 900, 900, 900 MiB (922 to 926) | none |
| Parakeet, one stream, repeated phrase | 880, 879, 879, 879, 879 MiB (903 to 904) | none |
| Parakeet, one load, two streams | 914, 915, 915, 915, 915 MiB (937) | none |
| Parakeet, two loads, two streams | 1,707, 1,717, 1,711, 1,711, 1,709 MiB (1,731 to 1,738) | none |

- **Apple's host process.**
  - Every Apple run started exactly one new `localspeechrecognition` process,
    and never more than one at a time, with one stream or two: both analyzers
    ran in the same host.
  - Three idle `localspeechrecognition` processes that were already running
    held 20 MiB between them and did not grow. Each run began with a fresh
    host, so the host figures are what the streams added.
  - The host's resident size was not sampled.
- **Parakeet's load time.** Its model loaded in 0.30 to 0.32 s, and two loads
  in 0.53 to 0.55 s. The first load after the build compiles its shaders; it
  took 10.3 s and is not in the table.

Against the resource budgets:

- **Apple.** Active memory for the Apple engine and VAD is budgeted at 2.5 GB.
  Two Apple streams used 42 to 43 MiB resident in Macalla's process and at
  most 79 MiB in the host.
- **Parakeet.** An opt-in large engine's active figure is its measured peak,
  stated on its card. One Parakeet load serving two streams peaked at 937 MiB
  resident. Two loads would put 1.7 GB on that card.

### Text

Each stream's final text was compared with the same file on its own in the
same run: as the exact string, and word by word (lower case, punctuation
removed).

- **Parakeet:** identical to the file on its own in every run, with one load
  or two.
- **Apple, one stream:** identical in all five runs.
- **Apple, two streams:** the reading differed from its one-stream text in
  every run:
  - by 1, 1, 3 and 3 word edits in 381 words in four runs, and in punctuation
    only in the fifth;
  - where one stream wrote "plea." two streams wrote "cli.", and where it wrote
    "maybe" they wrote "may be";
  - the rest were commas and capitals.

  The repeated phrase differed once, in punctuation. No word from the other
  stream appeared in either text.

**Known limit:** with a second Apple stream running, a stream's text is not
always the same as it would be alone. The differences are small, and they fall
at phrase edges.

### Limits of this measurement

- **Unpaced.** The audio was fed as fast as each engine accepted it, so the
  real-time factors are throughput with both streams competing flat out. They
  are not the delay a live meeting sees at real-time pace. That delay is
  measured in "At real-time pace" below.
- **Inferred here, measured below.**
  - On a shared Parakeet load, one stream waits for the other's piece in
    flight.
  - At the measured real-time factor of about 0.028, a piece at the 30 s
    segment cap takes about 0.85 s, which puts the longest wait at roughly that.
  - The paced run found no measurable delay added by sharing the load.
- **Not an idle machine.**
  - Running: a terminal and a coding session (about 30% CPU together), and an
    audio-routing app with its daemon (about 5%).
  - No build, test or CI job ran.
  - The one-minute load average before each process was 0.62 to 2.12, highest
    just after a Parakeet run.
- **Fixed order.** The scenarios ran in the same order in every run, so order
  effects are not randomised out.
- **Small sample.** Two recordings, one speaker.

### Reading

- **One engine instance per engine** serves a live meeting.
- **Apple:** one engine with an analyzer per stream, which is the API's shape.
  - The second stream added 25 to 36 MiB in the host.
  - Each stream still ran about 55 times faster than real time.
- **Parakeet:** share one load.
  - Two loads doubled the memory and were no faster on this M1.

### At real-time pace (2026-09-14)

The runs above fed audio as fast as each engine took it. A live meeting feeds
it as it is spoken, so the question here is how long after its audio a final
text arrives, and whether a second stream makes that longer. Nemotron 3.5 ASR
streaming 0.6B was added.

#### How it was measured

- **The machine.** The same Mac mini as above.
- **The probe.** The same throwaway probe, extended with paced scenarios and
  built in release mode. Each stream was fed in 100 ms chunks at real time and
  polled after each push.
- **The recordings.** The same two: the 148.2 s reading and the 131.3 s
  repeated phrase.
- **What was recorded.**
  - For each final text, its delay: the wall time it arrived, less the time
    its audio ended in the recording.
  - The longest push, and the number of pushes over 200 ms.
  - Memory, sampled every 5 s.
- **The runs.** Three rounds of every scenario. Before each round: no build,
  test or CI job running, and a one-minute load average under 1.5.

#### Delay

Median and longest seconds a final arrived after its audio, runs 1 to 3.

| Scenario | 148.2 s reading | 131.3 s repeated phrase |
|---|---|---|
| Apple, one stream | 2.23 / 3.73, 2.23 / 3.83, 2.22 / 3.83 | 2.76 / 4.18, 2.77 / 4.09, 2.76 / 4.09 |
| Apple, two streams | 2.23 / 3.81, 2.22 / 3.72, 2.22 / 3.81 | 2.87 / 4.18, 2.86 / 4.19, 2.87 / 4.19 |
| Parakeet, one stream | 2.85 / 4.24, 2.81 / 4.16, 2.80 / 4.16 | 1.77 / 1.96, 1.76 / 1.97, 1.76 / 1.95 |
| Parakeet, one load, two streams | 2.78 / 4.19, 2.78 / 4.15, 2.79 / 4.20 | 1.76 / 1.96, 1.76 / 1.96, 1.75 / 1.95 |
| Nemotron, one stream | 0.00 to 0.08, the two recordings together | |
| Nemotron, one load, two streams | 0.00, 0.01, -0.00 | 24.74, 24.75, 24.74 |

- **Nemotron** gave one final per stream, at the end, so its figures are one
  delay each rather than a median and a longest.
  - With one stream, the range covers both recordings and was not recorded
    per file.
  - A delay of -0.00 rounds to zero from just below it.
- **Nemotron, one load, two streams.** The repeated phrase's stream waited for
  the whole of the reading's: one of its pushes blocked for 143.2 s in every
  round (143,175 to 143,182 ms), and it finished 24.7 s after its audio.

#### Pushes

- **Apple:** no push took over 2 ms in any scenario.
- **Parakeet:** a push that closes a piece also transcribes it, so pushes
  block with one stream as well as two.
  - One stream: the longest push was 877, 868 and 864 ms on the reading, with
    5 pushes over 200 ms in each run, and 358, 304 and 284 ms on the repeated
    phrase.
  - One load, two streams: 856, 865 and 867 ms on the reading, and 296, 298
    and 293 ms on the repeated phrase.
- **Nemotron, one stream:** no push over 1 ms.

#### Memory

The highest value over the run. All three rounds agreed within 3 MiB.

| Scenario | Probe's footprint | Probe's resident size | `localspeechrecognition`, all processes |
|---|---|---|---|
| Apple, one stream | 15 MiB | 29 MiB | 48 MiB |
| Apple, two streams | 24 MiB | 38 MiB | 66 to 67 MiB |
| Parakeet, one stream | 891 to 892 MiB | 916 to 917 MiB | none |
| Parakeet, one load, two streams | 903 to 906 MiB | 928 to 931 MiB | none |
| Nemotron, one stream | 950 to 951 MiB | 975 to 977 MiB | none |
| Nemotron, one load, two streams | 960 MiB | 985 to 986 MiB | none |

The `localspeechrecognition` column includes idle host processes, which held
about 20 MiB.

#### Limits

- **Not measured.**
  - Two Nemotron loads. Inferred here; measured on 2026-09-22 in "A live
    meeting on Nemotron, in pieces" below: a second load added about 817 MiB.
  - A live microphone and system audio, rather than recordings.
  - Delays over a real meeting's length.
  - Other Macs.
- **Not an idle machine.** A terminal and a coding session were running.
- **Small sample.** Three rounds, two recordings, one speaker.

#### Reading

- **Apple and a shared Parakeet load keep up at real-time pace.** Across one
  and two streams, a final arrived a median of 1.75 to 2.87 s after its audio,
  and 4.24 s at the longest. A second stream did not change that.
- **A shared Nemotron load does not, on the engine's own stream** (the path
  dictation uses). The second stream waits for the whole of the first, and
  finished 24.7 s late on a 131.3 s recording. A live meeting cuts Nemotron
  into pieces instead; see "A live meeting on Nemotron, in pieces" below.

### A live meeting on Nemotron, in pieces (2026-09-22)

The runs above fed each stream through the engine's own stream. For Nemotron
that stream holds the engine for its whole life, which is why a second stream
waited 143.2 s. A live meeting does not use that path: like a file meeting, it
cuts an engine that does not endpoint for itself into pieces at the pauses,
and each piece holds the engine only while it is transcribed. The question
here is whether, on that path, one Nemotron load serves both of a meeting's
streams, or each stream needs its own.

#### How it was measured

- **The machine.** The same Mac mini, now on macOS 27.0 (build 26A428).
- **The path.** Macalla's own live meeting code, not a probe of the engine: a
  small program opened a session on Nemotron 3.5 ASR streaming 0.6B and
  called the live meeting with two sources, built in release mode.
- **The recordings.** The same two, served at real time in 100 ms chunks: the
  148.2 s reading as the microphone, and the 131.3 s repeated phrase as the
  call. The meeting was stopped 0.5 s after the longer recording ended.
- **Three ways.**
  - One stream: the reading alone.
  - Two streams on one load: the engine was wrapped to report that it can
    serve two streams at once, so both streams used the one load.
  - Two streams on two loads: the code as it then stood, which opened a
    second load for the call's stream.
- **What was recorded.** How long after the audio ended the meeting finished,
  the words heard on each stream, and the process's peak footprint (the
  kernel's lifetime peak).
- **The runs.** Three rounds of each, one process per run, 10 s apart.
  Before each run: no build, test or CI job running, and a one-minute load
  average under 1.5.

#### Results

Runs 1 to 3. "Finished" is the time from the end of the longer recording to
the meeting's result, and includes the 0.5 s before the stop.

| Scenario | Finished | Words, microphone / call | Peak footprint |
|---|---|---|---|
| One stream | 0.91, 0.87, 1.89 s | 392 / none | 967, 967, 967 MiB |
| Two streams, one load | 0.93, 0.88, 1.96 s | 392 / 94 | 975, 974, 974 MiB |
| Two streams, two loads | 0.89, 0.91, 0.94 s | 392 / 94 | 1,791, 1,792, 1,791 MiB |

- **Words.** Identical in every run with two streams, one load or two: 17
  turns, 392 words on the microphone's stream and 94 on the call's.
- **The third round.** One stream alone finished 1.89 s after its audio, and
  two streams on one load 1.96 s, while two loads, which ran last in that
  round, finished in 0.94 s. Rounds ran in the same order every time: one
  stream, one load, two loads. So round 3 on its own cannot separate the
  machine at that moment from the scenario; that one stream alone was just as
  late suggests the machine, and rounds 1 and 2 show no difference between
  one load and two.
- **A first load.** Loading the model for the first time took 10.52 s, in a
  separate check before the runs; in the runs, with the file cached, it took
  0.37 to 0.45 s.

#### Limits

- **One machine,** not the reference machine.
- **Two recordings,** one speaker each.
- **Delay at the end only,** not for each piece as it was committed.
- **A first attempt was invalid** and is not in the table: the program asked
  for the engine by a name the registry does not use, and its error output
  was discarded, so every run failed at once and printed nothing. It was
  caught from the empty results, both faults were fixed, and the runs above
  followed.

#### Reading

- **On the path a live meeting uses, one Nemotron load serves both streams.**
  The call's stream kept up, the words did not change, and a second load
  cost about 817 MiB (a peak of at most 975 MiB for one load against at least
  1,791 MiB for two) for nothing measurable.
- **The 143.2 s wait belongs to the engine's own stream,** which dictation
  uses and a live meeting does not.

### A live meeting on Whisper, one instance (2026-09-23)

Whisper runs on the same piece-at-a-time path as Parakeet, so a live meeting
already puts both of its streams on one Whisper instance. This checks that one
instance keeps up at real-time pace, with the largest English Whisper model on
the machine.

#### How it was measured

- **The machine.** The same Mac mini, on macOS 27.0 (build 26A428).
- **The path and the recordings.** As in "A live meeting on Nemotron, in
  pieces" above: Macalla's own live meeting code, the 148.2 s reading as the
  microphone and the 131.3 s repeated phrase as the call, fed at real time in
  100 ms chunks, the meeting stopped 0.5 s after the longer recording ended.
- **The engine.** Whisper medium.en, the slowest English Whisper model here.
- **Two ways.** One stream (the reading alone), and two streams on one
  instance, which is what a live meeting on Whisper does.
- **Timing.** Counted from the moment the meeting's sources started.
- **The runs.** Three rounds of each, one process per run, 10 s apart. Before
  each run: no build, test or CI job running, and a one-minute load average
  under 1.5.

#### Results

Runs 1 to 3. "Finished" is the time from the end of the longer recording to
the meeting's result, and includes the 0.5 s before the stop.

| Scenario | Finished | Words, microphone / call | Peak footprint |
|---|---|---|---|
| One stream | 2.01, 2.00, 2.03 s | 383 / none | 1,147, 1,133, 1,133 MiB |
| Two streams, one instance | 2.02, 2.00, 2.04 s | 383 / 91 | 1,136, 1,136, 1,137 MiB |

- **Words.** Identical in every run with two streams: 18 turns, 383 words on
  the microphone's stream and 91 on the call's.
- **A first load.** Loading the model the first time took 10.34 s; with the
  file cached, 0.41 to 0.45 s.

#### Limits

- **One machine,** not the reference machine.
- **One Whisper model.** The smaller English models are faster on the same
  path and were not run.
- **Delay at the end only,** not for each piece as it was committed.
- **Not comparable to the Nemotron figures' absolute delays**: those were
  timed from before the meeting started, these from the moment its sources
  started. Within each table, one stream and two are timed the same way.
- **A first attempt was invalid**: the program was written against an older
  version of the live meeting code, did not build, and the runs went ahead
  without it, so nothing was measured. The runner now stops when the build
  fails.

#### Reading

- **One Whisper instance serves both of a live meeting's streams.** A second
  stream changed neither when the meeting finished nor the words.

## Meeting summaries across providers (2026-09-14)

The question was which models write a summary whose every item quotes the
transcript, the rule the summary is held to: an item whose quote is not found
word for word in its part of the meeting is dropped, and an action item's
owner is kept only when it is one of the speakers.

Run from the reference machine. Cloud times include the network. The Ollama
rows ran on an Ollama server on a Mac mini M1 with 16 GB, reached over the
network from the reference machine.

### How it was measured

- **The transcript.** A synthetic two-speaker WebVTT file, seven cues and
  about 680 characters, with a release date, two action items with clear
  owners, and one decision. It is small enough to be one part, and it is not a
  recording of real speech.
- **The command.** `macalla summarise <file> --json`, with the default
  instructions and the summary's plain-text request, at one commit for every
  row. Cloud models were reached through their own APIs, or through
  OpenRouter where the row says so, with the key read from the environment.
- **The runs.** One run per model, except granite4.1 3B and Apple
  Intelligence, three runs each. Time is the wall time of the whole command.
- **What "kept" counts.** Items that passed the quote check, as key points,
  decisions and action items. It is not a judgement of how good the summary
  is.

### Results

| Model | Reached through | Kept (points, decisions, actions) | Dropped | Time |
|---|---|---|---|---|
| gpt-5.6-luna | OpenAI | 2, 2, 2 | 0 | 4 s |
| claude-haiku-4-5 | Anthropic | 3, 2, 2 | 0 | 2 s |
| gemini-3.5-flash-lite | Google | 3, 2, 2 | 0 | 2 s |
| ministral-14b-2512 | Mistral | 4, 3, 3 | 0 | 3 s |
| ministral-3b-latest | Mistral | 0, 1, 0 | 12: 6 without a quote, 6 with one too short | 1 s |
| qwen/qwen3.8-flash | OpenRouter | 2, 4, 2 | 0 | 1 s |
| google/gemma-4-26b-a4b-it | OpenRouter | 2, 3, 2 | 0 | 3 s |
| qwen/qwen3.5-27b | OpenRouter | 3, 3, 2 | 0 | 3 s |
| ~z-ai/glm-flash-latest | OpenRouter | 2, 2, 2 | 0 | 6 s |
| deepseek/deepseek-v4-flash | OpenRouter | 4, 3, 2 | 0 | 9 s |
| deepseek/deepseek-v4-flash-0731 | OpenRouter | 4, 3, 2 | 0 | 10 s |
| deepseek/deepseek-chat-v3.1 | OpenRouter | 5, 3, 2 | 0 | 10 s |
| qwen/qwen3-32b | OpenRouter | 3, 2, 2 | 0 | 21 s |
| qwen2.5 3B | Ollama | 4, 2, 2 | 0 | 13 s |
| granite4.1 3B | Ollama | 0, 0, 3 in all three runs | 6, all without a quote | 11, 11, 8 s |
| qwen2.5 1.5B | Ollama | 0, 0, 0 | 10, all without a quote | 4 s |
| qwen2.5 0.5B | Ollama | 0, 0, 0 | 3, outside the three lists | 2 s |

- **Owners.** Wherever an action item was kept, its owner was the speaker who
  said it. No model copied a speaker's label into a quote.
- **Reasoning.** Gemini and Mistral refused a request for no reasoning and were
  asked again without the setting, as the cleanup adapters do.
- **Apple Intelligence, as plain text,** on the reference machine at an
  earlier commit: two runs answered in about 5 s with all 14 reply lines
  dropped, and one did not answer within the 10 s limit then in force. The
  reason, that its lines carried no quote at all, was found by reproducing the
  run on a Mac mini. Asking it for structured output with a required quote is
  measured separately.

### Limits of this measurement

- **One small, synthetic meeting.** A longer meeting, real speech, or more
  speakers could rank the models differently.
- **Single runs** for most models, so the times are indications, not
  distributions.
- **Kept is not quality.** An item can quote correctly and still summarise
  poorly; nothing here scores that.

### Reading

- **Small cloud models and open models of 3B and up** quoted every item on
  this meeting.
- **Local models under 3B did not**, and granite4.1 3B quoted only its action
  items.
- **Apple Intelligence, asked in plain text,** quoted nothing.

## Apple Intelligence: schema-first against plain text (2026-09-16)

The 20 fixtures in `bench/fixtures.json`, scored with
`bench/enhance_ab_score.py`. The `macalla` release binary was built on
2026-09-14. The dictionary default was in use, as in the 2026-09-11 runs
above.

### Apple Intelligence

Ran on the M1 Pro itself (16 GB, macOS 26.7, build 25G229), both shapes from
the same binary: the 20 fixtures twice per shape, with no separate first
request.

- **Schema-first (the old default): exact 0 / 0.** p50 594 / 597 ms. Run 1
  had 4 requests hit the 10 s limit, so its p50 covers the 16 that answered.
  Run 2: 18 left as Tier 0: 17 returned exactly as sent, and 1 rejected by
  the guard.
- **Plain text (the new default; `--unguided` on that binary): exact
  12 / 12.** p50 522 / 530 ms. Unchanged 3 / 3, rejected 2 / 2. Same replies
  in both runs, 20 of 20. Run 1: slowest 9,839 ms, p95 8,093 ms. Run 2:
  slowest and p95 both 620 ms.
- **2026-09-11, same M1 Pro, macOS 26.6.2** ("Apple Intelligence as an
  enhancement provider" above): schema-first changed 4 of 20 fixtures,
  capitals only; plain text changed 17. The same shape held five days
  later, on a newer macOS build.

### Ollama 0.34.1 on a separate M1 Mac mini (16 GB)

Run with `bench/enhance_endpoint.sh`: one first request per model, then the
20 fixtures twice.

Reached from the M1 Pro over the network -- these timings **include**
that network trip, unlike the Apple Intelligence rows above, which ran on the
M1 Pro itself. This is the same-day reference with the same scorer, not a
speed comparison against Apple Intelligence.

| Model | Exact of 20 | p50 ms | p95 ms | Slowest ms | Unchanged | Rejected | Errors |
|---|---|---|---|---|---|---|---|
| granite4.1:3b (Q4_K_M) | 17 / 17 | 752 / 735 | 908 / 884 | 938 / 925 | 2 / 2 | 2 / 2 | 0 / 0 |
| qwen2.5:1.5b (Q4_K_M) | 14 / 14 | 346 / 346 | 458 / 420 | 501 / 451 | 3 / 3 | 3 / 3 | 0 / 0 |
| qwen2.5:3b (Q4_K_M) | 11 / 11 | 649 / 649 | 781 / 771 | 793 / 868 | 4 / 4 | 2 / 2 | 0 / 0 |

- Each model gave the same replies in both runs (20 of 20).
- First request including model load: qwen2.5:1.5b 1,398 ms (nothing loaded
  before); qwen2.5:3b 2,023 ms (qwen2.5:1.5b already loaded); granite4.1:3b
  2,576 ms (both qwen models already loaded).
- Tier 0 alone: 0 of 20.
- granite4.1 3B (17), qwen2.5 1.5B (14) and qwen2.5 3B (11) all reproduce
  their "with the dictionary" scores from "Punctuation and capitals against a
  reference" above exactly, with the same method.

### Reading

- **The point of this section is the accuracy of the two Apple Intelligence
  request shapes.** Schema-first punctuated nothing exactly; plain text got
  12 of 20 exact, in line with the 2026-09-11 comparison. The Ollama rows are
  a same-day reference with the same scorer, not ranked against Apple
  Intelligence: they ran on a different Mac, over the network.
- **Schema-first's run 1 is the slow one.** Four of its requests hit the 10 s
  limit; plain text's own slowest run (run 1) still answered everything, at
  9,839 ms worst case.

## Overlay loudness colours (2026-09-16)

The overlay colours each waveform bar amber when that moment was loud and red
when it was near clipping. These measurements set the thresholds.

**Machine:** MacBook Pro, M1 Pro, 16 GB, macOS 26.7.
**Microphones:** MacBook Pro Microphone, and AirPods Pro over Bluetooth.

### Method

- A throwaway probe reproduces the app's capture chain: downmix to mono,
  then resample to 16 kHz. Every 50 ms poll, it logs the RMS and peak of the
  resampled audio, which is what the overlay's level is computed from.
- One speaker, one session. Per microphone: 10 s of silence, 10 s of normal
  speech, 10 s of loud speech, and 5 s of a shout or a clap.
- A "speech" chunk is a 50 ms poll with RMS above -50 dBFS.

### Levels after resampling (dBFS)

| Microphone | Take | RMS median | RMS max | Peak p95 | Peak max |
|---|---|---|---|---|---|
| MacBook Pro Microphone | silence | -64.7 | -54.5 | -49.6 | -45.0 |
| MacBook Pro Microphone | normal | -32.8 | -20.3 | -10.5 | -8.5 |
| MacBook Pro Microphone | loud | -26.9 | -15.9 | -6.5 | -3.5 |
| MacBook Pro Microphone | shout | -16.3 | 0.0 | +14.8 | +24.4 |
| AirPods Pro | silence | -70.1 | -54.2 | -48.6 | -45.6 |
| AirPods Pro | normal | -27.5 | -12.4 | -10.0 | -4.5 |
| AirPods Pro | loud | -22.6 | -6.9 | -4.9 | +0.1 |
| AirPods Pro | shout | -19.1 | -7.5 | -2.7 | +2.5 |

- MacBook Pro Microphone, shout: 1,464 device samples at full scale. The device
  delivered float samples up to +26 dBFS.
- AirPods Pro, shout: 4 device samples at full scale.

### Thresholds chosen

| Colour | Rule |
|---|---|
| red (near clipping) | peak ≥ -1 dBFS, or RMS ≥ -6 dBFS |
| amber (loud) | otherwise, RMS ≥ -18 dBFS, or peak ≥ -6 dBFS |
| teal | otherwise |

Bar height uses a separate display scale: -55 dBFS RMS draws no bar and
-10 dBFS a full one, linear in dB between. Colours use the thresholds above.

Share of speech chunks coloured amber / red with these thresholds:

| Microphone | Normal | Loud | Shout |
|---|---|---|---|
| MacBook Pro Microphone | 0 / 0 % | 7.0 / 0 % | 33.7 / 28.1 % |
| AirPods Pro | 2.2 / 0 % | 32.1 / 0.6 % | 48.8 / 2.4 % |

The first, unmeasured values put amber at RMS -12 and peak -3 dBFS. On loud
speech they coloured 0 to 1.8 % of chunks amber, which is why they were
lowered.

### Limits

- One speaker, one session, two microphones. An external audio interface was
  not tested.
- The AirPods ran at a lower device rate, over Bluetooth, and some polls came
  back empty.
- Peaks after resampling can exceed full scale, so red means "near clipping",
  not "clipped".

## Long-form video through `macalla transcribe` (2026-10-01)

The first run of Macalla on real-world video rather than fixtures: two public
videos about another dictation app, a one-minute product video by its creator
and an eight-minute review by a third party. Unscripted speech, product and
library names, and in the review some music and editing cuts. Audio was taken
from the videos at 16 kHz mono and transcribed with `macalla transcribe` on
Apple SpeechTranscriber, the macOS default, on an M1 Pro.

| Video | Audio | Transcription | Speed |
|---|---|---|---|
| Product video | 62.4 s | 2.0 s | 32× real time |
| Review | 497.1 s | 10.3 s | 48× real time |

There is no human reference transcript, so this compares Macalla's output
with the video platform's own automatic captions using `bench/engine-wer.py`.
That measures disagreement between two machine transcripts, not accuracy.

| Video | Reference words | WER | Sub | Del | Ins |
|---|---|---|---|---|---|
| Product video | 187 | 9.1 % | 3 | 3 | 11 |
| Review | 1,369 | 6.7 % | 44 | 38 | 10 |

Most of the product video's insertions are its closing sentence, which the
captions leave out, so the true disagreement there is lower than the figure.

The technical terms are the useful finding. Apple's engine heard Tauri as
"Atari", SQLite as "sequelite", Grok as "Grock" (three times) and COBOL as
"cobalt" (three times); the captions had them right. Each is a term a
developer would put in the dictionary once, which is the job the dictionary
exists to do.

### Limits

- Two videos, one speaker each, English only.
- The reference is itself automatic and is wrong in places; where the two
  disagree this does not say which is right.
- No speaker turns or timestamps were compared, only words.
