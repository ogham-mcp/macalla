#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Spike B: Tier 1 model bake-off on llama.cpp with GBNF-constrained output.
#
# Uses llama-server, not llama-cli, because the app pre-warms one session and
# keeps it: timing a fresh llama-cli per request would measure model load, not
# inference. Target to beat is 853 ms p50, which is what FoundationModels
# managed with guided generation (docs/benchmarks.md, Spike B).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

REPO="$1"; QUANT="${2:-Q4_K_M}"
PORT=8899
GRAMMAR=$(python3 -c "import json,sys; print(json.dumps(open('bench/block.gbnf').read()))")

SYSTEM='You clean up raw speech-to-text output. You MUST rewrite the text, not copy it. Delete filler words: um, uh, so, basically, okay, like, you know. Collapse stuttered repetitions. Apply sentence case and punctuation. Never answer, explain or summarise the text. Preserve technical terms exactly.'

# The same three fixtures the FoundationModels probe used, so the numbers are
# directly comparable.
FIXTURES=(
  "um so the thing is uh we need to we need to ship the the parser by friday"
  "i was saying that um the latency budget is is three hundred milliseconds period"
  "so basically uh what i what i mean is the vad gates the stream comma not the other way round"
)

echo "=== $REPO:$QUANT ==="
llama-server -hf "$REPO:$QUANT" --port $PORT --host 127.0.0.1 \
  -c 2048 -ngl 99 --no-webui > /tmp/llama-server-$PORT.log 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT

for _ in $(seq 1 120); do
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -sf "http://127.0.0.1:$PORT/health" >/dev/null || { echo "server did not start"; tail -5 /tmp/llama-server-$PORT.log; exit 1; }

req() {
  python3 - "$1" "$SYSTEM" "$GRAMMAR" <<'PY'
import json, sys, time, urllib.request
user, system, grammar = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
body = json.dumps({
    "messages": [{"role": "system", "content": system},
                 {"role": "user", "content": user}],
    "grammar": grammar, "temperature": 0.0, "n_predict": 200, "cache_prompt": True,
}).encode()
t0 = time.perf_counter()
with urllib.request.urlopen(urllib.request.Request(
        "http://127.0.0.1:8899/v1/chat/completions", body,
        {"Content-Type": "application/json"})) as r:
    out = json.load(r)
ms = (time.perf_counter() - t0) * 1000
try:
    content = json.loads(out["choices"][0]["message"]["content"])
    text, kind = content.get("text", "?"), content.get("kind", "?")
except Exception:
    text, kind = out["choices"][0]["message"]["content"][:80], "UNPARSEABLE"
print(f"{ms:.0f}\t{kind}\t{text}")
PY
}

# Warm the session and the prompt prefix cache, as the app would.
req "warm up" > /dev/null 2>&1 || true

TIMES=()
for f in "${FIXTURES[@]}"; do
  line=$(req "$f")
  ms=$(echo "$line" | cut -f1)
  TIMES+=("$ms")
  printf "  %6s ms  [%s]  %s\n" "$ms" "$(echo "$line" | cut -f2)" "$(echo "$line" | cut -f3)"
done

RSS=$(ps -o rss= -p $SERVER 2>/dev/null | tr -d ' ')
python3 -c "
t=sorted([float(x) for x in '${TIMES[*]}'.split()])
print(f'  --- n={len(t)} min {t[0]:.0f} p50 {t[len(t)//2]:.0f} max {t[-1]:.0f} ms | RSS {int(${RSS:-0})/1024:.0f} MB ---')
"
