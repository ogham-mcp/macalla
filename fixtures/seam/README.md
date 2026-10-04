# Seam fixtures

Cases for the seam pass, which rejoins the segments of one dictation at
assembly. Each case is a list of committed engine results, in order, and the
document they must assemble to: `plain` is the pasted text, `markdown` (where
given) pins the block kinds too.

Two engine shapes, because the boundaries do not fall in the same places:

- `apple` — an engine that owns segmentation and punctuates and capitalises
  each result on its own. Cuts fall at pauses, mid-sentence included, and the
  engine restarts its casing at every cut.
- `vad` — an engine fed VAD-cut segments, lowercase and unpunctuated. Cuts
  fall where the speaker breathes.

Cases marked "real dictation" are transcripts from actual use, kept verbatim,
because they are what found the problem. Run with
`cargo test -p macalla-core seam`.
