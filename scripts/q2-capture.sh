#!/usr/bin/env bash
# Slice Q2 — one window capture, so somebody can actually LOOK at the
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
# BUILD, do not merely check. `cargo test` builds test binaries and leaves
# `target/debug/eave` alone, so a capture that only checked for the file
# would happily photograph the last build — which is exactly how Q2's
# first shot showed a layout that had already been fixed.
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# The launcher only shows when the engine has NO sessions, so this runs
# against a throwaway data dir and lets eave spawn its own engine. A
# replay fixture would create a session and the launcher would be gone
# before the shutter opened — which is the whole reason this is a separate
# script from q1-capture.sh rather than a flag on it.
DATA="$(mktemp -d)/eave-launcher"
mkdir -p "$DATA"

# Speed 1 keeps the window alive for the fixture's full wall clock, which
# is minutes — plenty for a ~50 s shutter. EAVE_NO_SPAWN keeps it off the
# real CLI; the dogfood log is redirected so a capture never appends to it.
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/q2-capture-log.md"

echo "launching $BIN against $DATA"
EAVE_DATA_DIR="$DATA" "$BIN" >/dev/null 2>&1 &
sleep 10
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
screencapture -x -o -t png "$OUT/q2-launcher.png"
echo "captured $(ls -la "$OUT/q2-launcher.png" | awk '{print $5}') bytes"

kill "$PID" 2>/dev/null || true
echo "done: $OUT/q2-launcher.png"
