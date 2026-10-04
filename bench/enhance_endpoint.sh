#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Times models behind an OpenAI-compatible endpoint with the same fixtures and
# scoring as bench/enhance_ab.sh, so their rows sit next to Apple Intelligence's.
#
#   bench/enhance_endpoint.sh <macalla> <endpoint> <out-dir> <model> [<model> ...]
#   KEY_ENV=OPENAI_API_KEY bench/enhance_endpoint.sh <macalla> https://api.openai.com/v1 <out-dir> <model>
#   KEY_ENV=ANTHROPIC_API_KEY bench/enhance_endpoint.sh <macalla> anthropic <out-dir> claude-haiku-4-5
#
# KEY_ENV names the environment variable holding a cloud provider's key; it is
# passed on as --key-env, so the key itself never appears on a command line.
#
# Per model: the first request in a new process, which includes loading the
# model if the server had not already, then the 20 fixtures twice. The endpoint
# is an argument and is not written to any file here. The fixtures are
# synthetic, but they still leave this Mac: run it only against a server you
# chose to send text to.
#
# "loaded before" is read from Ollama's /api/ps when the server has it, and
# says "unknown" otherwise.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

BIN=${1:?macalla binary}
ENDPOINT=${2:?endpoint address}
OUT=${3:?out dir}
shift 3
if (($# == 0)); then echo "name at least one model"; exit 1; fi
FIRST="remind me to book the room for the design review on thursday afternoon"
mkdir -p "$OUT"
KEY_ARGS=()
if [[ -n ${KEY_ENV:-} ]]; then KEY_ARGS=(--key-env "$KEY_ENV"); fi
# "anthropic" in place of an address uses Anthropic's own API (enhance --anthropic).
if [[ $ENDPOINT == anthropic ]]; then PROVIDER_ARGS=(--anthropic); else PROVIDER_ARGS=(--endpoint "$ENDPOINT"); fi

python3 -c 'import json; print("\n".join(c["raw"] for c in json.load(open("bench/fixtures.json"))["cases"]))' \
  > "$OUT/fixtures.txt"
host=$(python3 -c 'import sys, urllib.parse as u; p = u.urlsplit(sys.argv[1]); print(f"{p.scheme}://{p.netloc}")' "$ENDPOINT")

loaded_models() {
  if [[ $ENDPOINT == anthropic ]]; then echo unknown; return; fi
  curl -s --max-time 5 "$host/api/ps" 2>/dev/null | python3 -c '
import json, sys
try:
    print(",".join(m["name"] for m in json.load(sys.stdin).get("models", [])) or "none")
except Exception:
    print("unknown")'
}

: > "$OUT/first.jsonl"
for model in "$@"; do
  name=${model//[^A-Za-z0-9.]/_}
  before=$(loaded_models)
  row=$("$BIN" enhance "${PROVIDER_ARGS[@]}" --model "$model" ${KEY_ARGS[@]+"${KEY_ARGS[@]}"} --json "$FIRST" 2>/dev/null || true)
  echo "{\"label\":\"$model, loaded before: $before\",\"row\":${row:-null}}" >> "$OUT/first.jsonl"
  for run in 1 2; do
    "$BIN" --debug enhance "${PROVIDER_ARGS[@]}" --model "$model" ${KEY_ARGS[@]+"${KEY_ARGS[@]}"} --json \
      < "$OUT/fixtures.txt" > "$OUT/$name-$run.jsonl" 2> "$OUT/$name-$run.err" || true
    echo "done $model run $run"
  done
done

python3 bench/enhance_ab_score.py "$OUT"
