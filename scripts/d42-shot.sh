#!/usr/bin/env bash
# Photograph the rail: build, reseed, launch, bind to OUR pid, shoot.
#
#     scripts/d42-shot.sh <name>        # -> ../docs/shots/<name>.png
#
# WHY THIS DOES NOT USE dev.sh, run-app.sh OR bundle-app.sh's DEFAULT OUT.
#
#   * `dev.sh` and `run-app.sh` run `stop-eave.py`, which kills EVERY eave on
#     the machine from EVERY checkout. Other lanes have apps running.
#   * `--out /tmp/eave-d4`, NOT the in-tree `target/bundle`. `target/` lives
#     under ~/Desktop, which is inside iCloud sync, and bundle-app.sh's
#     `rm -rf` of the old 357 MB bundle wedges there in uninterruptible sleep
#     (state `U`, where kill -9 does nothing). The identical work in /tmp
#     finishes in under a minute. `wt-e3/target/bundle/eave.app` is still
#     half-deleted from the run that discovered this.
#
# IT RESEEDS EVERY SHOT, deliberately. The data dir accumulates: tabs left
# behind by the previous launch, and the rail's persisted WIDTH drifting, made
# consecutive gate shots incomparable — and "no visible change means the stage
# failed" is unreadable when the frame moves on its own between shots.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

NAME="${1:?usage: d42-shot.sh <name>}"
APP=/tmp/eave-d4
DATA=/tmp/etd4
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS" "$DATA"

scripts/bundle-app.sh --out "$APP" --features web >"$DATA/build.log" 2>&1

# Ours only, by path. The largest eave window on this screen is usually a
# peer's — never match on the process NAME.
for P in $(pgrep -f -- "--engine --data-dir $DATA" || true); do kill -TERM "$P" 2>/dev/null || true; done
for P in $(pgrep -f "$APP/eave.app/Contents/MacOS/eave\$" || true); do kill -TERM "$P" 2>/dev/null || true; done
sleep 3
rm -rf "$DATA"
python3 scripts/t1-seed.py "$DATA" >/dev/null

open -n "$APP/eave.app" --env "EAVE_DATA_DIR=$DATA" --env EAVE_REDUCE_MOTION=1 \
  --stdout "$DATA/out.log" --stderr "$DATA/err.log"

PID=""
for _ in $(seq 1 40); do
  PID="$(pgrep -f "$APP/eave.app/Contents/MacOS/eave\$" | head -1 || true)"
  [ -n "$PID" ] && break
  sleep 1
done
[ -n "$PID" ] || { echo "no app process" >&2; exit 1; }
sleep 12

# The engine quarantines a journal it cannot parse and starts EMPTY, and the
# app then draws a perfectly truthful photograph of an empty browser.
if ls "$DATA"/browser/*.corrupt-* >/dev/null 2>&1; then
  echo "the engine rejected the seed:" >&2
  ls "$DATA"/browser/*.corrupt-* >&2
  exit 1
fi

WIN=$(python3 -c "
import Quartz
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionAll | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    if w.get('kCGWindowOwnerPID') == $PID and w.get('kCGWindowName') == 'eave':
        print(w['kCGWindowNumber']); break
")
[ -n "$WIN" ] || { echo "no window for pid $PID" >&2; exit 1; }

osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" >/dev/null 2>&1 || true
sleep 2
screencapture -o -x -l "$WIN" "$SHOTS/$NAME.png"
echo "pid $PID win $WIN -> $SHOTS/$NAME.png"
