#!/usr/bin/env bash
# Photograph Slice T1: projects, the bookmarks bar, and the removal menu.
#
#     scripts/t1-capture.sh [name-prefix]
#
# WHY THIS DOES NOT USE dev.sh, run-app.sh, OR shot.sh.
#
#   * `dev.sh` and `run-app.sh` both run `stop-eave.py`, which kills EVERY
#     eave on the machine from EVERY checkout. Three other worktrees were
#     running apps when this was written. A capture is not a reason to end
#     somebody else's session.
#   * `shot.sh` runs `cargo build` (no `--features web`) and launches the
#     loose binary. T1's bar only draws over a page, and a page needs the
#     CEF bundle. So this builds the bundle and launches THAT.
#
# It never kills anything. It binds to the pid it launched, because there
# are normally several eave windows on this screen and the largest one is
# usually somebody else's — the first run of this capture found a window,
# photographed nothing, and the window belonged to wt-c1.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

PREFIX="${1:-t1}"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
DATA="${EAVE_T1_DATA:-/tmp/et2}"     # short: SUN_LEN caps the socket path
mkdir -p "$SHOTS"

echo "building the bundle (web) …"
scripts/bundle-app.sh --features web >/dev/null
APP="$ROOT/target/bundle/eave.app"

echo "seeding $DATA …"
rm -rf "$DATA"
python3 scripts/t1-seed.py "$DATA"

open -n "$APP" --env "EAVE_DATA_DIR=$DATA" --env EAVE_REDUCE_MOTION=1 \
  --stdout "$DATA/out.log" --stderr "$DATA/err.log"

# The app pid is the one WITHOUT --engine, from this checkout only.
PID=""
for _ in $(seq 1 30); do
  PID="$(pgrep -f "$ROOT/target/bundle/eave.app/Contents/MacOS/eave\$" | head -1 || true)"
  [ -n "$PID" ] && break
  sleep 1
done
[ -n "$PID" ] || { echo "no app process" >&2; exit 1; }
echo "app pid $PID (ours)"

# The engine MUST have accepted the seed. It moves a journal it cannot parse
# to `<name>.corrupt-<ts>` and starts empty — the app then comes up and draws
# a truthful photograph of an empty browser. That is how the first version of
# this capture nearly shipped a picture of nothing.
sleep 8
if ls "$DATA"/browser/*.corrupt-* >/dev/null 2>&1; then
  echo "the engine rejected the seed:" >&2
  ls "$DATA"/browser/*.corrupt-* >&2
  exit 1
fi
echo "seed accepted"

WIN="$(python3 - "$PID" <<'PY'
import sys, time, Quartz
mine = int(sys.argv[1])
for _ in range(45):
    best = None
    for w in Quartz.CGWindowListCopyWindowInfo(
            Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
            Quartz.kCGNullWindowID):
        if w.get("kCGWindowOwnerPID") != mine:
            continue
        b = w["kCGWindowBounds"]
        a = b["Width"] * b["Height"]
        if a < 100000:
            continue
        if best is None or a > best[1]:
            best = (w["kCGWindowNumber"], a)
    if best:
        print(best[0]); break
    time.sleep(1)
PY
)"
[ -n "$WIN" ] || { echo "no window for pid $PID" >&2; exit 1; }
echo "window $WIN"

raise_it() {
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" >/dev/null
  sleep 1
}

shoot() {
  local name="$1"
  local out="$SHOTS/$PREFIX-$name.png"
  raise_it
  for i in 1 2 3; do
    if perl -e 'alarm 45; exec @ARGV' screencapture -o -x -l "$WIN" "$out" 2>/dev/null; then
      echo "  → $out"
      return 0
    fi
    sleep 2
  done
  # Distinguish the two failures that look alike. A TCC denial is INSTANT
  # and says "could not create image"; a hang is the timeout. Reporting one
  # as the other has already cost this repo a wrong conclusion in both
  # directions.
  echo "FAILED: $name" >&2
  if ! screencapture -x /tmp/.t1-tcc-probe.png 2>&1 | grep -q .; then
    echo "  (display capture works — this is a window problem)" >&2
  else
    echo "  Screen Recording is not granted to this terminal." >&2
    echo "  System Settings → Privacy & Security → Screen Recording," >&2
    echo "  then RESTART the terminal — TCC only re-reads on launch." >&2
  fi
  rm -f /tmp/.t1-tcc-probe.png
  return 1
}

FAILED=0

# 1. The frame at rest: the project above the loose tabs with the TABS
#    divider between them, and the bookmarks bar over a live page.
shoot 01-project-and-bar || FAILED=1

# 2. A bookmark folder open, showing its direct children.
#
# NOT AUTOMATED, deliberately. `as-click.py` takes SCREEN COORDINATES, not
# a selector, and the folder chip's position cannot be derived without
# first seeing the frame. Guessing a point and photographing the result
# would produce a shot that looks deliberate and proves nothing — the app
# is left running instead so shot 1 can be measured and the coordinate
# read off it. Once it is known:
#
#     python3 scripts/as-click.py <x> <y>      # the `docs` folder chip
#     scripts/snap-window.sh eave $SHOTS/$PREFIX-02-folder-open.png
#
# The same goes for shot 3 (right-click → Remove): `e1-rclick.py` posts a
# right click at a point, and the point comes off shot 1.

echo
if [ "$FAILED" = 0 ]; then
  echo "captured into $SHOTS"
else
  echo "some shots failed — see above. The app is STILL RUNNING (pid $PID)" >&2
  echo "so you can look at it directly; quit it from its own menu." >&2
fi
exit "$FAILED"
