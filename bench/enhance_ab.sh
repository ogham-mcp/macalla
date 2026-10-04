#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Times two builds of the Apple Intelligence provider against each other, then
# the first request after idle with and without prewarm.
#
#   bench/enhance_ab.sh <baseline-macalla> <candidate-macalla> [out-dir]
#   A_FLAGS=... B_FLAGS=... PREWARM=0 IDLE=90 bench/enhance_ab.sh ...
#
# The A/B runs the 20 fixtures as plain text in the order A, B, B, A, so
# neither build always goes first. A_FLAGS and B_FLAGS add `enhance` flags to
# one side, for example B_FLAGS=--no-dictionary, or the same binary twice with
# A_FLAGS=--no-dictionary. The prewarm test uses the candidate only, so it
# needs `enhance --prewarm`; PREWARM=0 skips it. IDLE sets the wait before each
# first request (default 90 s); how long the system keeps its model loaded is
# not known, so a short IDLE may show no difference. Scored by
# bench/enhance_ab_score.py.
#
# --unguided asks for plain text explicitly, still, though it has been the
# default since 2026-09-16: this script also times a baseline built before
# that change, and that older binary needs the flag to get plain text rather
# than its own schema-first default.
#
# Before timing two builds, check each contains what it is meant to: a flag in
# the CLI proves nothing about the Swift bridge. One A/B compared a build with
# itself because both checkouts shared a target directory (docs/benchmarks.md).
#
# Rules from runs that were thrown away (docs/benchmarks.md):
# - Wait until the system model answers fast before timing anything.
# - Nothing heavy on the Mac, including a CI job on a self-hosted runner.
# - A timeout slows the requests after it; the score marks runs with errors.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

A=${1:?baseline macalla binary}
B=${2:?candidate macalla binary}
OUT=${3:-$(mktemp -d -t enhance-ab)}
IDLE=${IDLE:-90}
A_FLAGS=${A_FLAGS:-}
B_FLAGS=${B_FLAGS:-}
SETTLE="the ferry to inishmore leaves galway docks at half past nine"
FIRST="remind me to book the room for the design review on thursday afternoon"
mkdir -p "$OUT"
echo "results in $OUT"

ci_job() { pgrep -f Runner.Worker >/dev/null; }
if ci_job; then echo "a CI job is running on this Mac; nothing timed"; exit 1; fi

python3 -c 'import json; print("\n".join(c["raw"] for c in json.load(open("bench/fixtures.json"))["cases"]))' \
  > "$OUT/fixtures.txt"

# One request under 1,500 ms, tried every five minutes for up to 40 minutes.
settled=no
for try in 1 2 3 4 5 6 7 8; do
  # shellcheck disable=SC2086 # flags are separate words on purpose
  ms=$("$B" enhance --unguided $B_FLAGS "$SETTLE" 2>/dev/null | head -1 | awk '{print $1}' || true)
  echo "settle check $try: ${ms:-no answer} ms"
  if [[ $ms =~ ^[0-9]+$ ]] && ((ms < 1500)); then settled=yes; break; fi
  sleep 300
done
if [[ $settled != yes ]]; then echo "the model never answered fast; nothing timed"; exit 1; fi

for label in a1 b1 b2 a2; do
  bin=$A
  flags=$A_FLAGS
  if [[ $label == b* ]]; then
    bin=$B
    flags=$B_FLAGS
  fi
  # shellcheck disable=SC2086 # flags are separate words on purpose
  "$bin" --debug enhance --unguided --json $flags < "$OUT/fixtures.txt" \
    > "$OUT/$label.jsonl" 2> "$OUT/$label.err" || true
  echo "done $label"
done

if [[ ${PREWARM:-1} != 0 ]]; then
  : > "$OUT/prewarm.jsonl"
  for label in cold warm cold warm cold warm; do
    sleep "$IDLE"
    if [[ $label == warm ]]; then
      # shellcheck disable=SC2086 # flags are separate words on purpose
      row=$("$B" enhance --prewarm --json $B_FLAGS "$FIRST" 2>/dev/null || true)
    else
      # shellcheck disable=SC2086 # flags are separate words on purpose
      row=$("$B" enhance --json $B_FLAGS "$FIRST" 2>/dev/null || true)
    fi
    echo "{\"label\":\"$label\",\"row\":${row:-null}}" >> "$OUT/prewarm.jsonl"
    echo "done $label"
  done
fi

if ci_job; then echo "WARNING: a CI job started during the run"; fi
python3 bench/enhance_ab_score.py "$OUT"
