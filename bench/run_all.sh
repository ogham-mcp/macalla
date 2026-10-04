#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
for repo in "$@"; do
  echo "=== $repo ==="
  # --reasoning off is mandatory, not optional. With thinking on, a hybrid
  # model's output lands in reasoning_content and the grammar constrains the
  # thinking channel, scoring 0/20 for a model that is actually fine.
  llama-server -hf "$repo:${QUANT:-Q4_K_M}" --port 8899 --host 127.0.0.1 \
    -c 2048 -ngl 99 --no-webui --reasoning off \
    > /tmp/llama-bench.log 2>&1 &
  SERVER=$!
  for _ in $(seq 1 180); do curl -sf http://127.0.0.1:8899/health >/dev/null 2>&1 && break; sleep 1; done
  if curl -sf http://127.0.0.1:8899/health >/dev/null 2>&1; then
    python3 bench/tier1_full.py || true
    echo "  RSS_MB $(( $(ps -o rss= -p $SERVER | tr -d ' ') / 1024 ))"
  else
    echo "  server failed to start"; tail -3 /tmp/llama-bench.log
  fi
  kill $SERVER 2>/dev/null || true
  wait $SERVER 2>/dev/null || true
  sleep 2
done
