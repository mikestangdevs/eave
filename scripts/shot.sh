#!/usr/bin/env bash
# One reliable window capture. This replaces the launch-and-pgrep dance that
# every previous capture script open-codes, because that dance has three bugs
# and each one cost a slice a wrong conclusion:
#
#   1. `pgrep -n -f target/debug/eave` returns THE ENGINE, not the app.
#      eave is one binary in two roles — the app spawns
#      `eave --engine --data-dir …` as a child, so the *newest* matching
#      process is normally the engine, which owns no window. The Quartz probe
#      then finds nothing and the run concludes "no window". Fixed here by
#      never guessing: we capture `$!` from our own launch.
#
#   2. A full-screen `screencapture` photographs whatever is in front.
#      A shot that comes back showing a browser is OCCLUSION, not bad
#      coordinates. Fixed by capturing BY WINDOW ID (`screencapture -l`).
#
#   3. `sleep 10` is not a readiness check. A first launch behind a pending
#      macOS Files-and-Folders grant has taken over 28 s to put up a window,
#      and a short sleep reads that as "the app did not start". Fixed by
#      polling Quartz for the window until a real deadline.
#
# Stale processes from earlier runs are reported but NOT killed — this script
# no longer cares about them, and killing another session's app is not its
# call to make.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# Docs live OUTSIDE the repo. A repo-relative docs/ path is how two previous
# agents ended up writing into a stray tree that nothing reads.
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"

NAME="${1:?usage: shot.sh <name> [seconds-before-shutter]}"
SETTLE="${2:-4}"
DEADLINE=45

mkdir -p "$SHOTS"

# BUILD, do not merely check. `cargo test` builds test binaries and leaves
# target/debug/eave alone, so a capture that only checked for the file would
# happily photograph the previous build.
echo "building…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# Report, do not kill. Distinguish the two roles by argv, which is the only
# thing that tells them apart.
STALE="$(pgrep -f "$BIN" || true)"
if [ -n "$STALE" ]; then
  echo "note: eave processes already running (not touching them):"
  for p in $STALE; do
    if ps -o command= -p "$p" | grep -q -- --engine; then role=engine; else role=app; fi
    echo "  pid $p  role=$role"
  done
fi

DATA="${EAVE_SHOT_DATA:-$(mktemp -d)/eave-shot}"
mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/shot-dogfood.md"

echo "launching against $DATA"
EAVE_DATA_DIR="$DATA" "$BIN" >/dev/null 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"

# Poll for the window rather than sleeping at it. Also walks children, in case
# the window ends up owned by a process we spawned rather than by us.
WIN=""
for _ in $(seq 1 $DEADLINE); do
  WIN="$(python3 - "$APP_PID" <<'PY' || true
import sys, subprocess, Quartz
root = int(sys.argv[1])
pids = {root}
try:
    out = subprocess.run(["pgrep", "-P", str(root)], capture_output=True, text=True).stdout
    pids |= {int(x) for x in out.split()}
except Exception:
    pass
best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") not in pids:
        continue
    b = w["kCGWindowBounds"]
    area = b["Width"] * b["Height"]
    # Skip shadow/helper surfaces; a real app window is not 40px tall.
    if b["Width"] < 200 or b["Height"] < 200:
        continue
    if best is None or area > best[1]:
        best = (w["kCGWindowNumber"], area, b)
if best:
    print("%d %d %d" % (best[0], best[2]["Width"], best[2]["Height"]))
PY
)"
  [ -n "$WIN" ] && break
  sleep 1
done

if [ -z "$WIN" ]; then
  echo "no window from pid $APP_PID within ${DEADLINE}s" >&2
  kill "$APP_PID" 2>/dev/null || true
  exit 1
fi

WINID="$(echo "$WIN" | cut -d' ' -f1)"
echo "window id $WINID  $(echo "$WIN" | cut -d' ' -f2)x$(echo "$WIN" | cut -d' ' -f3)"

osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" 2>/dev/null || true
sleep "$SETTLE"

OUTFILE="$SHOTS/$NAME.png"
# -l takes the window by id, so occlusion cannot photograph the wrong thing.
screencapture -x -o -l "$WINID" "$OUTFILE"
echo "captured $OUTFILE ($(stat -f%z "$OUTFILE") bytes)"

kill "$APP_PID" 2>/dev/null || true
wait "$APP_PID" 2>/dev/null || true
echo "done"
