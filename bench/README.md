# bench

Tier 1 model bake-off harness. See `docs/benchmarks.md` for results.

```sh
./bench/run_all.sh <hf-repo> [<hf-repo> ...]     # defaults to Q4_K_M
QUANT=Q6_K ./bench/run_all.sh <hf-repo>          # pick a quantisation
```

`bench/fixtures.json` holds 20 raw-dictation cases with the filler words that
must go and the technical terms that must survive. `bench/block.gbnf` is the
grammar. `bench/tier1_full.py` scores a running `llama-server`.

## Apple Intelligence timing

```sh
./bench/enhance_ab.sh <baseline-macalla> <candidate-macalla> [out-dir]
IDLE=300 ./bench/enhance_ab.sh ...               # longer idle before each first request
```

Runs the 20 fixtures through `macalla enhance --unguided` on two builds in the
order A, B, B, A, then times the candidate's first request after idle with and
without `--prewarm`. It refuses to start while a CI job is running on the Mac
and waits for the system model to answer in under 1.5 s. `enhance_ab_score.py`
prints p50, p95 and the slowest request per run, how many replies the guard
rejected, and whether runs give the same replies.

Plain text is the default request shape since 2026-09-16, so `--unguided`
asks for what a current build already does by default; the script keeps the
flag so it also gets plain text from a `macalla` built before that change.
`--guided` asks for the older, schema-first shape instead, for a comparison
run.

It then scores each run's final text against the `reference` in
`bench/fixtures.json`: the Tier 0 words of each case, punctuated and
capitalised as the default prompt asks, with the dictionary's spellings. It
counts pieces with every capital and mark right, words with the wrong capitals,
and marks missing, added or wrong, and scores Tier 0's own text as the
baseline. In a reference, `(,)` after a word is an optional comma and `(-)` an
optional hyphen, `{.;:}` accepts any one of those marks, and `~` before a word
leaves its case unscored. A hyphen or dash in a reply separates the words either
side, as the guard's comparison does, so "ninety-five" scores as two words.
The references are one reader's judgement: a fixture that almost no run gets
exact is printed, and is worth re-reading before blaming the models.

```sh
./bench/enhance_endpoint.sh <macalla> <endpoint> <out-dir> <model> [<model> ...]
```

The same fixtures and scoring against models behind an OpenAI-compatible
endpoint: per model, the first request (with the model loading if it was not
already) and then two runs of 20. The fixtures leave the Mac, so point it only
at a server you chose to send text to.

`macalla enhance` applies the dictionary as dictation does: its corrections
before Tier 0, and its written forms in the prompt. Runs recorded before that
change (2026-09-11) used no dictionary; `--no-dictionary` reproduces them.

## Rules earned the hard way

**Answer the question on the smallest model that can answer it.** The
quantisation comparison was first run on a 4B model because another dictation app ships
one — 7 GB of downloads for a model that failed every quality gate. The same
finding fell out of the 0.5B we actually ship, for under 1 GB, and *that* run
is the one that changed the default. Reasoning about someone else's model
choice is not the same as reasoning about the question.

**`--reasoning off` is mandatory**, and `run_all.sh` passes it. With thinking
on, a hybrid model's output lands in `reasoning_content` and the grammar
constrains the thinking channel — scoring 0/20 for a model that is fine.

**Use `llama-server`, never `llama-cli` per request.** The app pre-warms one
session and keeps it; timing a fresh process measures model load, not
inference.

**Pre-fetch before benchmarking.** `run_all.sh` waits 180 s for health; a
multi-gigabyte first download outruns that. Pull with
`llama-cli -hf <repo>:<quant> -p hi -n 2 -st` first.

**Delete rejected weights.** Numbers survive in `docs/benchmarks.md`, and
`-hf` re-fetches on demand. Models land in `~/.cache/huggingface/hub`, shared
with every other project on the machine.

## What the scores mean

Gates: `terms_kept` ≥ 0.95, `content_dropped` ≤ 0.10, `invented` ≤ 0.05,
`schema_valid` 20/20, and `p50_ms` ≤ 300 for the default slot.

`filler_removed` is **reported, not gated** — filler removal is Tier 0's job.
The column measures whether a model rewrites at all rather than echoing, which
predicts behaviour on what Tier 0 cannot do.

Each of `content_dropped`, `invented` and `kind_correct` was added *after* a
model had already been recommended, and each one changed the answer. If a new
question comes up, assume the rubric is missing a column before assuming the
models are equivalent.

## Engine word error rate

```sh
python3 bench/engine-wer.py bench/wer-input.json
```

`bench/wer-input.json` holds the reference and the saved transcripts for
control-harvard, tech-terms and clinical. Each transcript's key names the
runtime it came from:

- `apple`: Apple SpeechTranscriber.
- `parakeet-mlx`: Parakeet TDT v3 on MLX, an early run.
- `canary-sherpa`: Canary 180M Flash on sherpa-onnx, the run in "Canary 180M
  Flash — a third engine".
- `parakeet-tcpp`: the transcribe.cpp build Macalla ships, from
  `macalla transcribe --raw --engine parakeet-tdt-v3` on 2026-09-12. It passes
  through the default voice gate, as dictation does.

Neither the MLX nor the sherpa-onnx build ships, so their clinical scores (0.188
and 0.125) are not Macalla's. The shipped Parakeet transcript scores 0.219 on
clinical. That is one word fewer than the 0.234 in the "Nemotron 3.5" and
"Canary 180M Flash and Whisper" sections, whose transcripts were not saved.
Canary 180M's transcribe.cpp transcript was not saved either, and it is not in
the model catalogue, so the CLI cannot rerun it.
