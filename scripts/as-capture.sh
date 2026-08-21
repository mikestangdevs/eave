#!/usr/bin/env bash
# Slice AS — photograph the rail OPEN and the rail COLLAPSED, because the
# whole slice is about what happens between those two states.
#
# Mechanics are `shot.sh`'s, and each one cost an earlier slice a wrong
# conclusion (docs/progress.md §Traps):
#   * never `pgrep` for the app — eave is one binary in two roles and the
#     newest match is normally the ENGINE, which owns no window. `$!` only.
#   * capture BY WINDOW ID (`screencapture -l`), never a screen-wide shot:
#     a shot that comes back showing a browser is occlusion, not bad
#     coordinates.
#   * poll Quartz for the window instead of sleeping at it.
#   * `screencapture` leaves the window non-frontmost, so a click issued
#     after a shot lands in whatever is behind it. Re-raise between shots.
#   * NO hover is photographed. A synthesized CGEventMouseMoved produces no
#     hover fade in this app, so nothing here is allowed to depend on one.
#
# One trap this script adds: **System Events' `click at` does not reach this
# app's TITLEBAR.** Six single-click runs at the rail toggle produced six
# byte-identical shots, while the same call 100 px lower selected a session
# every time. Quartz `CGEventPost` at the same point works — which is what
# §Traps already recommends for buttons — so every click here goes through
# `scripts/as-click.py`.
#
# The sessions are hand-seeded `meta.json` + header-only `journal.jsonl`
# pairs: there is still no way to create a session from the command line, so
# no agent ever ran in these. The rail's own facts are real — the engine
# computes them from what is written here.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

echo "building…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-as"
mkdir -p "$DATA/sessions"
python3 "$ROOT/scripts/as-seed.py" "$DATA"

export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/as-shot-log.md"
EAVE_DATA_DIR="$DATA" "$BIN" >/dev/null 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"
# Reap the ENGINE as well as the app. `eave` is one binary in two roles and
# the app spawns its engine as a child; killing only the app leaves the
# engine holding the data dir's flock, which is the orphan class Slice AE
# spent a session on. By parentage (`pkill -P`), never by name.
reap() { pkill -P "$APP_PID" 2>/dev/null || true; kill "$APP_PID" 2>/dev/null || true; }
trap reap EXIT

WIN=""
for _ in $(seq 1 45); do
  WIN="$(python3 "$ROOT/scripts/as-window.py" "$APP_PID" || true)"
  [ -n "$WIN" ] && break
  sleep 1
done
[ -n "$WIN" ] || { echo "no window from pid $APP_PID" >&2; exit 1; }
read -r WINID WX WY WW WH <<<"$WIN"
echo "window $WINID at ${WX},${WY} ${WW}x${WH}"

raise() {
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 \
    || echo "note: could not raise; a shot of another app is occlusion"
  sleep 1
}
click() {
  raise
  python3 "$ROOT/scripts/as-click.py" "$((WX + $1))" "$((WY + $2))"
  sleep 1
}
shot() {
  raise
  sleep 2
  screencapture -x -o -l "$WINID" "$SHOTS/$1.png"
  echo "captured $SHOTS/$1.png ($(stat -f%z "$SHOTS/$1.png") bytes)"
}

sleep 6
# Select a session, so the shot is of a rail with a selection in it rather
# than of the launcher.
click "${AS_ROW_X:-150}" "${AS_ROW_Y:-140}"
shot "as-rail-open"

# The toggle: TRAFFIC_CLEARANCE + SPACE_1 + half of ICON_TOGGLE across, half
# a titlebar down.
click "${AS_TOGGLE_X:-100}" "${AS_TOGGLE_Y:-24}"
shot "as-tabs-collapsed"

echo done
