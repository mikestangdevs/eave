#!/usr/bin/env bash
# Slice AN — photograph the transcript's empty state.
#
# The hermetic harness can assert that the card is on screen, that its
# headline is at the display rank, and that it clears the composer. It
# cannot answer the question the operator actually asked ("this app still
# looks like booboo caca"), because that is a question about composited
# pixels: real font metrics, the frost sampling a real desktop, and a
# colour space. Only a compositor answers it.
#
# THE STATE THIS NEEDS is "a session exists and has no turns yet", which
# is exactly what `EAVE_LAUNCH_AT_BOOT=1` produces against an empty data
# dir — the same route `af-state-proof.sh` takes, and the reason its own
# comments record seeing `nothing sent yet` in the main pane. Nothing is
# clicked and nothing is typed: progress.md records that synthetic
# keyboard events do not reach this gpui app at all, so a capture that
# depended on typing would be photographing a failure.
#
# Mechanics that each cost an earlier slice a wrong conclusion (see
# docs/progress.md §Traps):
#   * launch by ABSOLUTE path, or `pgrep -f` will not find the process;
#   * budget ~60 s a shot, or a slow shutter reads as a permission wall;
#   * a shot showing another app is OCCLUSION, not bad coordinates;
#   * 16 s to boot, not 9 — under load a short wait finds no window and
#     the geometry probe crashes on a None, which reads as a crash;
#   * BUILD, do not merely check: `cargo test` leaves `target/debug/eave`
#     at whatever the last build was, which is how Q2's first shot
#     photographed a layout that had already been fixed.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${1:-$ROOT/docs-shots}"
mkdir -p "$OUT"

BIN="$ROOT/target/debug/eave"
FAKE="$ROOT/target/debug/fake-agent"
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-an"; mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/an-capture-log.md"
# Point the engine at the fake CLI rather than the real one. The session
# this photographs has no turns, so the agent is never asked for anything
# either way — but a capture script has no business spawning a real
# `claude`, and the orphan class this project spent Slice AE on is exactly
# what happens when one does.
[ -x "$FAKE" ] && export EAVE_AGENT="$FAKE"

# Slice AE's rule, applied to a script: the reap is a property of the run
# (it fires during unwind, not as a statement at the end that a `set -e`
# exit would walk straight past), and it VERIFIES rather than hopes. The
# first version of this script left an app and an engine alive after a
# clean exit, because `kill` returning 0 says the signal was delivered and
# nothing about whether the process went away.
reap() {
  kill "${PID:-}" 2>/dev/null || true
  # The engine and any agent are the APP's children, not this shell's, so
  # they have to be found by pattern. Both carry the throwaway data dir on
  # their command line; the app itself carries only the binary path, so it
  # is matched separately and only within this worktree.
  for _ in 1 2 3; do
    pkill -f "$DATA" 2>/dev/null || true
    pkill -f "^$BIN" 2>/dev/null || true
    sleep 1
    pgrep -f "$DATA" >/dev/null 2>&1 || pgrep -f "^$BIN" >/dev/null 2>&1 || return 0
  done
  pkill -9 -f "$DATA" 2>/dev/null || true
  pkill -9 -f "^$BIN" 2>/dev/null || true
  sleep 1
  if pgrep -lf "$DATA" || pgrep -lf "^$BIN"; then
    echo "LEAK: the processes above outlived the capture" >&2
  fi
}
trap reap EXIT

boot() {
  EAVE_DATA_DIR="$DATA" EAVE_LAUNCH_AT_BOOT="$1" "$BIN" >/dev/null 2>&1 &
  sleep 16
  PID="$(pgrep -n -f "$BIN" || true)"
  [ -n "$PID" ] || { echo "app did not start" >&2; exit 1; }
}

boot 1                                    # seed one never-started session
kill "$PID" 2>/dev/null || true; sleep 2
[ "$(find "$DATA" -name journal.jsonl | wc -l | tr -d ' ')" -ge 1 ] \
  || { echo "seeding produced no session" >&2; exit 1; }

boot 0                                    # boot onto it, empty journal
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" >/dev/null 2>&1 || \
  echo "note: could not raise; a shot of another app is occlusion"
sleep 3

# Geometry from QUARTZ, never System Events — Quartz measures a gpui
# window to the pixel at the same instant System Events reports it has
# none (progress.md §Traps). Probed BEFORE the shutter, because
# `screencapture` leaves the window non-frontmost.
read -r WX WY WW WH < <(python3 - "$PID" <<'PY'
import sys, Quartz
pid = int(sys.argv[1]); best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    if w.get('kCGWindowOwnerPID') != pid:
        continue
    b = w['kCGWindowBounds']; a = b['Width'] * b['Height']
    if best is None or a > best[0]:
        best = (a, b)
if best is None:
    sys.exit("no window for pid %d — the app was not up yet" % pid)
b = best[1]
print(int(b['X']), int(b['Y']), int(b['Width']), int(b['Height']))
PY
)
echo "window ${WW}x${WH} at ${WX},${WY}"

echo "capturing (~50 s under load — not a hang)"
screencapture -x -o -t png "$OUT/an-empty-state.png"
echo "captured $(ls -la "$OUT/an-empty-state.png" | awk '{print $5}') bytes"

# Crop to the window, so this sits beside the before-shots rather than
# needing a different kind of looking. The scale is RECOVERED from the
# image rather than assumed to be 2 — an external display is 1x and would
# halve every offset (af-state-proof.sh learned this one).
python3 - "$OUT/an-empty-state.png" "$WX" "$WY" "$WW" "$WH" <<'PY'
import sys
from PIL import Image
path, wx, wy, ww, wh = sys.argv[1], *map(int, sys.argv[2:6])
im = Image.open(path)
s = im.size[0] / 1728.0
pad = int(24 * s)                        # a little desktop, so the frost reads
box = (max(0, int(wx * s) - pad), max(0, int(wy * s) - pad),
       min(im.size[0], int((wx + ww) * s) + pad),
       min(im.size[1], int((wy + wh) * s) + pad))
im.crop(box).save(path)
print(f"  cropped to {box} at scale {s:g}")
PY
echo "done: $OUT/an-empty-state.png"
