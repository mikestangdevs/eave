#!/usr/bin/env bash
# Slice BZ — the 10k-row perf gate, run so that it actually reports.
#
# `EAVE_PERF=10k` scrolls 10,000 rows top to bottom at PERF_ROWS_PER_FRAME
# and prints two PERF lines when it finishes. Launched detached it printed
# NOTHING and exited: the probe advances per rendered FRAME, and a gpui window
# that is never raised does not render frames on a loaded machine — so the
# probe never reaches its last row and never reports. Raising it is not
# cosmetic here, it is what makes the measurement happen.
#
# Traps carried from the capture rigs: never `pgrep` for the app (eave is one
# binary in two roles and the newest match is normally the ENGINE) — `$!`
# only; keep stderr, because a rig that discards it cannot explain itself.
# And note `scripts/stop-eave.py`, which `dev.sh`/`run-app.sh` run on every
# rebuild, kills every eave on this machine from EVERY checkout — if this
# reports nothing and the process is gone, suspect a neighbouring session
# before suspecting the probe.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
OUT="${1:?usage: bz-perf.sh <output-file>}"

cargo build --quiet
DATA="${TMPDIR:-/tmp}/bzperf"
rm -rf "$DATA"; mkdir -p "$DATA"

EAVE_PERF=10k EAVE_DATA_DIR="$DATA" "$ROOT/target/debug/eave" >"$OUT" 2>&1 &
APP_PID=$!
echo "perf pid $APP_PID"
reap() { kill "$APP_PID" 2>/dev/null || true; }
trap reap EXIT

for _ in $(seq 1 30); do
  if python3 "$ROOT/scripts/as-window.py" "$APP_PID" >/dev/null 2>&1; then break; fi
  sleep 1
done
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true

# ~20 s of scrolling at 8 rows/frame — but that is 8239 frames, and this
# machine has been seen at load 130 with a peer session building. A 120 s cap
# timed out mid-probe once and reported nothing, which reads exactly like a
# perf regression and is not one. 5 minutes.
# **Re-raise on every poll, not once.** The probe only advances on RENDERED
# frames, and a gpui window that is not frontmost does not get them — so if
# anything takes focus (a browser, a peer session's `open -n`) the probe
# stalls silently and the rig reports nothing, which reads exactly like a
# perf regression. One raise at the start is not enough; frontmost is stolen,
# not held. If this still reports nothing, the machine is in use and the
# honest answer is "unmeasured", not "unchanged".
for _ in $(seq 1 100); do
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true
  if grep -q "^PERF frames=" "$OUT" 2>/dev/null; then break; fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    echo "FAIL: the app exited before reporting — see $OUT" >&2
    tail -20 "$OUT" >&2 || true
    exit 1
  fi
  sleep 3
done
grep -E "^PERF" "$OUT" || { echo "FAIL: no PERF line after 300 s" >&2; exit 1; }
