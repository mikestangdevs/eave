#!/usr/bin/env bash
# Slice P3 — one window capture, so somebody can actually LOOK at the
# glyphs.
#
# This exists because the hermetic harness provably cannot: `TestAppContext`
# hardcodes `Arc::new(())` as its asset source and `App` exposes no setter,
# so every icon in `ui_harness.rs` lays out its box and paints nothing.
# Whether the pipeline resolves bytes at all is only answerable through a
# real compositor.
#
# The mechanics below are not incidental — each one cost a previous slice a
# wrong conclusion (see docs/progress.md §Traps):
#   * launch by ABSOLUTE path, or `pgrep -f` will not find the process;
#   * budget ~60 s a shot, or a slow shutter reads as a permission wall;
#   * a shot showing another app is OCCLUSION, not bad coordinates.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${1:-$ROOT/docs-shots}"
mkdir -p "$OUT"

BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN — build first" >&2; exit 1; }

FIXTURE="$ROOT/fixtures/real-claude-2026-08-16.tap.jsonl"
[ -f "$FIXTURE" ] || { echo "missing $FIXTURE" >&2; exit 1; }

# Speed 1 keeps the window alive for the fixture's full wall clock, which
# is minutes — plenty for a ~50 s shutter. EAVE_NO_SPAWN keeps it off the
# real CLI; the dogfood log is redirected so a capture never appends to it.
export EAVE_REDUCE_MOTION=0
export EAVE_NO_SPAWN=1
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/p3-capture-log.md"

echo "launching $BIN"
EAVE_REPLAY="$FIXTURE" EAVE_REPLAY_SPEED=1 "$BIN" >/dev/null 2>&1 &
sleep 12
PID="$(pgrep -n -f "$BIN" || true)"
[ -n "$PID" ] || { echo "app did not start" >&2; exit 1; }
echo "pid $PID"

# Raising is not enough to make a window KEY, but for a static glyph shot
# we only need it unoccluded — no mouse-moved events are involved. Try to
# raise it, and do not fail the run if accessibility is not granted.
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" 2>/dev/null || \
  echo "note: could not raise the window; if the shot shows another app, that is occlusion"
sleep 3

echo "capturing (this takes ~50 s under load — not a hang)"
screencapture -x -o -t png "$OUT/p3-full.png"
echo "captured $(ls -la "$OUT/p3-full.png" | awk '{print $5}') bytes"

kill "$PID" 2>/dev/null || true
echo "done: $OUT/p3-full.png"
