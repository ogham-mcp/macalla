#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Settle the question of whether vocabulary is the problem, with REAL speech.
#
# Everything measured so far used `say`-synthesised fixtures. A synthetic "um"
# is a crisply pronounced word; a human "um" is a vocalisation with different
# acoustics, and the engines may treat the two differently. This records you
# and runs both engines on the same audio.
#
#   ./bench/record-and-compare.sh            # 20 seconds
#   ./bench/record-and-compare.sh 45         # longer
#
# Dictate naturally. Do NOT try to be fluent -- hesitate, restart, say "um" and
# "you know" as you normally would. The disfluency is the measurement.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

SECONDS_TO_RECORD="${1:-20}"
OUT="fixtures/audio/human-$(date +%Y%m%d-%H%M%S).aiff"
PROBE="${MACALLA_APPLE_PROBE:-/tmp/macalla-spikea2}"
VENV="${MACALLA_PK_VENV:-${TMPDIR:-/tmp}/macalla-pk}"

# Build the Apple probe if it is not already there, so this script works from a
# clean checkout.
if [[ ! -x "$PROBE" ]]; then
  echo "building the Apple probe..."
  swiftc -O -parse-as-library docs/spike-a2-latency-probe.swift -o "$PROBE" 2>/dev/null \
    || echo "  (could not build; Apple results will be skipped)"
fi
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "creating the parakeet venv (first run only)..."
  uv venv "$VENV" -q 2>/dev/null && uv pip install -q --python "$VENV/bin/python" parakeet-mlx 2>/dev/null \
    || echo "  (could not create; Parakeet results will be skipped)"
fi

echo
echo "About to record ${SECONDS_TO_RECORD}s."
echo "Speak naturally. Hesitations and restarts are the point -- say 'um',"
echo "restart a sentence, use your own technical vocabulary."
echo
SCRIPT_TO_READ="${MACALLA_SCRIPT:-fixtures/dictation-script.md}"
if [[ -f "$SCRIPT_TO_READ" ]]; then
  echo "  Script to read: $SCRIPT_TO_READ"
  echo
  echo "  Read the CONTROL first on a new machine, mic or voice:"
  echo "    MACALLA_SCRIPT=fixtures/dictation-script-control.md $0 30"
  echo "  Both engines should be near-perfect on it. If they are not, the"
  echo "  setup is the problem and domain results from it mean little."
  echo
fi
for i in 5 4 3 2 1; do printf "  starting in %d...\r" "$i"; sleep 1; done
printf "  RECORDING NOW -- speak.        \n"
# ffmpeg with avfoundation: no extra install, and ":0" is the default input.
ffmpeg -hide_banner -loglevel error -f avfoundation -i ":0" \
  -t "$SECONDS_TO_RECORD" -ac 1 -ar 22050 -y "$OUT" \
  || { echo "Recording failed. Grant microphone access to your terminal in" >&2
       echo "System Settings > Privacy & Security > Microphone." >&2; exit 1; }

printf "  done.                          \n"

# A recording nobody spoke into produces two empty transcripts and looks like an
# engine failure. Check the level and say so instead.
LEVEL=$(ffmpeg -hide_banner -i "$OUT" -af volumedetect -f null - 2>&1 \
        | sed -n 's/.*mean_volume: \(-*[0-9.]*\) dB.*/\1/p')
if [[ -n "$LEVEL" ]]; then
  echo "  mean level: ${LEVEL} dB"
  if awk "BEGIN{exit !(${LEVEL} < -40)}"; then
    echo
    echo "  THAT IS ROOM TONE, NOT SPEECH. Normal dictation sits around -25 to -30 dB."
    echo "  Either nothing was said, or the input is too quiet. Both engines will"
    echo "  return nothing, which is not an engine failure."
    echo
    echo "  Check System Settings > Sound > Input, then run this again."
    exit 1
  fi
fi

echo
echo "=== APPLE SpeechTranscriber ==="
if [[ -x "$PROBE" ]]; then
  "$PROBE" "$OUT" 2>&1 | grep "FINAL" | sed 's/^ *[0-9]*  */  /' || echo "  (no final results)"
else
  echo "  probe unavailable"
fi

echo
echo "=== PARAKEET TDT v3 ==="
if [[ -x "$VENV/bin/python" ]]; then
  "$VENV/bin/python" - "$OUT" <<'PY'
import sys
from parakeet_mlx import from_pretrained
m = from_pretrained("mlx-community/parakeet-tdt-0.6b-v3")
print("  " + m.transcribe(sys.argv[1]).text.strip())
PY
else
  echo "  parakeet venv unavailable at $VENV"
fi

echo
echo "What to look for:"
echo "  - Does either engine transcribe your 'um' and 'uh'?"
echo "  - Does either punctuate and capitalise?"
echo "  - Which gets your technical vocabulary right?"
echo
echo "Audio kept at $OUT (gitignored). Add the transcripts to"
echo "bench/engine-fixtures.json if they are worth keeping."
