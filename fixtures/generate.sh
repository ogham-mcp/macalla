#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Regenerate the audio fixtures. They are NOT committed: the repository keeps
# fixture audio out of git, and `say` reproduces them deterministically on any
# macOS machine, so the script is the artefact and the .aiff files are build
# output.
#
#   ./fixtures/generate.sh
#
# Voice and rate are pinned because changing either moves the sentence
# boundaries the latency fixtures measure.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
mkdir -p audio

VOICE="Daniel"
RATE=175

gen() { say -v "$VOICE" -r "$2" -o "audio/$1.aiff" "$3"; echo "  audio/$1.aiff"; }

echo "regenerating fixtures with voice=$VOICE"

# --- WER control (bench/wer-input.json, docs/benchmarks.md) ---
# The Harvard sentences bench/engine-wer.py scores every engine against.
# Confirmed 2026-09-13: voice=Daniel rate=$RATE reproduces the copy on the
# reference machine (dated 2026-09-08, provenance otherwise unrecorded)
# byte-for-byte -- `cmp` matched, and Apple Speech and Parakeet TDT v3 gave
# identical transcripts on both files. If a future macOS or voice-data
# update ever makes this not match, say so here and note it is no longer
# byte-identical to the file the published control-harvard WER came from.
gen control-harvard $RATE \
  "The birch canoe slid on the smooth planks. Glue the sheet to the dark blue background. It's easy to tell the depth of a well. These days a chicken leg is a rare dish. Rice is often served in round bowls. The juice of lemons makes fine punch. The box was thrown beside the parked truck. The hogs were fed chopped corn and garbage. Four hours of steady work faced us. A large size in stockings is hard to sell."

# --- latency and segmentation (Spike A part 2) ---
gen three-sentences $RATE \
  "The latency budget is three hundred milliseconds. We enhance each segment while you are still speaking. Only the trailing segment is in flight when you release the key."
gen run-on $RATE \
  "so the thing is we need to ship the parser by friday and then run the benchmarks and write up the results before anyone asks about them"
gen short-burst $RATE "Ship it."

# --- technical vocabulary (Spike A part 3) ---
# Written as the SPOKEN form; the written form is what the dictionary must
# produce. Vocabulary drawn from work actually done, not invented.
gen tech-terms 170 \
  "I ran terra grunt against the azure landing zone. Then terra form applied the V net and the H C L was valid. We deployed to A W S as well, and kube control showed the pods running."
gen tech-terms-2 170 \
  "The post gres database uses P G vector for embeddings. Supa base runs on the prox mox L X C over tail scale, and ogham exposes it through M C P."

# --- homophones: these test the ENGINE's language model, not the dictionary.
# Both spellings are valid English, so no phonetic rule can fix them.
gen homophones 170 \
  "They're going to deploy their changes over there. We need to write the right value to the root route. Clear the cache before you count the cash. The queue took its cue from the sink that failed to sync."
gen homophones-2 170 \
  "The principal engineer explained the principle. That will affect the effect on latency. Two nodes are too few to serve the site we cite."

echo "done. Expected transcripts are in fixtures/transcripts/."
