#!/usr/bin/env bash
# Slice E1 — measure a live resize, in FRAMES.
#
#     scripts/e1-resize-sweep.sh              # this branch's behaviour
#     scripts/e1-resize-sweep.sh --stretch    # the behaviour before E1
#
# Split out of `e1-capture.sh` because the two need different things from
# the machine and only one of them is fragile. A screenshot needs the
# window to be KEY — synthetic keystrokes go to whatever is — and this
# machine currently has another agent's `dev.sh` relaunching its own eave
# on a watcher, which steals frontmost and takes the keyboard with it.
# **A resize needs none of that**: `set size of window 1 of process` reaches
# a window that is not frontmost, and the number this script wants is read
# out of the app's own trace rather than off the screen. So the measurement
# survives a machine the photograph cannot.
#
# WHAT IT MEASURES. `web/page.rs` counts, per tab, the frames painted at a
# size that is not the size the element was asked for, and narrates the run:
#
#     [web] resize wants 1032x756, frame is 832x697
#     [web] resize settled at 1032x756 after N stale frame(s), M ms
#
# **Both numbers, and M is the one that moves.** A window that jumps to a
# new size paints once at the old frame either way, so N is 1 in both
# builds; what the pump kick changes is how long that one paint stays on
# screen, because nothing asks gpui to redraw until a new frame arrives and
# nothing produces a new frame until CEF is pumped. What those N frames
# LOOKED like is the other half, and it is what `--stretch` restores:
# scaled to fit (before) against drawn at one device pixel per device pixel
# and clipped (after).
#
# It does not `stop-eave.py` and it does not kill anything by name — several
# checkouts of this repo run at once on this machine. Its own app is reaped
# by `$!`, its helpers by parentage, and its engine by the per-run data dir
# (the engine is detached on purpose and is NOT a child).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
# ⚠️ `if…then…fi`, never `[ test ] && cmd`, as a whole statement under
# `set -e`. A bare `[ "$1" = "--stretch" ] && STRETCH=1` returns 1 when the
# flag is absent, and `set -e` exits the script — silently, before any
# output, with status 1. This script shipped that mistake once and it made
# the default run produce an EMPTY LOG, which reads as "the app printed
# nothing" rather than "the script never started". docs/progress.md has the
# same trap recorded twice already.
STRETCH=0
if [ "${1:-}" = "--stretch" ]; then STRETCH=1; fi

APP="$ROOT/target/bundle/eave.app"
BIN="$APP/Contents/MacOS/eave"
[ -x "$BIN" ] || { echo "no bundle at $APP — run scripts/bundle-app.sh --features web" >&2; exit 1; }

DATA="/tmp/eave-e1rs-$$"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"
python3 "$ROOT/scripts/e1-seed.py" "$DATA" "file://$ROOT/scripts/e1-page.html" >/dev/null

SUFFIX=""
if [ "$STRETCH" = 1 ]; then
  SUFFIX="-stretch"
  export EAVE_WEB_STRETCH=1
fi
LOG="${TMPDIR:-/tmp}/e1-resize-sweep${SUFFIX}.log"
EAVE_DATA_DIR="$DATA" EAVE_WEB_TRACE=1 "$BIN" >"$LOG" 2>&1 &
APP_PID=$!
trap 'pkill -P "$APP_PID" 2>/dev/null || true; kill "$APP_PID" 2>/dev/null || true; pkill -f -- "--data-dir $DATA" 2>/dev/null || true; sleep 1; rm -rf "$DATA" 2>/dev/null || true' EXIT

for _ in $(seq 1 60); do
  C="$(python3 "$ROOT/scripts/as-window.py" "$APP_PID" || true)"
  if [ -n "$C" ]; then
    read -r _ _ _ cw _ <<<"$C"
    [ "$cw" -ge 800 ] && break
  fi
  sleep 1
done
kill -0 "$APP_PID" 2>/dev/null || { echo "our eave died before it drew a window" >&2; tail -20 "$LOG" >&2; exit 1; }

# Wait for the FIRST frame rather than sleeping at it: the sweep's first
# step would otherwise be measured against a page that had never painted,
# and "0 stale frames" would mean "there was nothing to be stale".
for _ in $(seq 1 40); do
  grep -q "accel_paint #0" "$LOG" && break
  sleep 0.5
done
grep -q "accel_paint #0" "$LOG" || { echo "the page never painted" >&2; tail -20 "$LOG" >&2; exit 1; }
sleep 2

WHICH="this branch"
if [ "$STRETCH" = 1 ]; then WHICH="PRE-E1, EAVE_WEB_STRETCH=1"; fi
echo "sweep ($WHICH):"
for size in "1280 820" "980 820" "1420 900" "1080 780" "1340 700" "1080 761"; do
  read -r w h <<<"$size"
  osascript -e "tell application \"System Events\" to set size of window 1 of (first process whose unix id is $APP_PID) to {$w, $h}" >/dev/null 2>&1 || {
    echo "  resize to ${w}x${h} failed — is our window still there?" >&2
    kill -0 "$APP_PID" 2>/dev/null || { echo "  our eave is gone (a peer's stop-eave.py kills every eave on this machine)" >&2; exit 1; }
  }
  sleep 2
done

echo
grep -E "resize (wants|settled)" "$LOG" || echo "(no resize lines — nothing changed size)"
echo
echo -n "stale frames per settle: "
grep -oE "after [0-9]+ stale" "$LOG" | grep -oE "[0-9]+" | paste -sd, - || true
echo -n "milliseconds per settle:  "
grep -oE "stale frame\(s\), [0-9]+ ms" "$LOG" | grep -oE "[0-9]+" | paste -sd, - || true
echo "log: $LOG"
