#!/usr/bin/env bash
# Slice BZ — photograph the surfaces bezel is being adopted for, before and
# after, so "improved it" is a comparison and not an opinion.
#
# Usage: scripts/bz-capture.sh <label>
#          label   goes into the filename: docs/shots/bz-<label>-*.png
#
# It replays a REAL fixture rather than seeding a synthetic session, because
# the surfaces in question are the transcript's: a tool call with a name, a
# detail and an elapsed figure only exists if something actually ran. The
# 2026-08-16 Claude capture has all of them.
#
# Mechanics are `d1-capture.sh`'s and each cost an earlier slice a wrong
# conclusion (docs/progress.md §Traps): never `pgrep` for the app (eave is one
# binary in two roles and the newest match is normally the ENGINE, which owns
# no window) — `$!` only; capture BY WINDOW ID, never `-R` (it hangs on this
# machine and killing it wedges `replayd`); poll Quartz for the window rather
# than sleeping at it; re-raise between shots because `screencapture` leaves
# the window non-frontmost; keep the app's stderr, because a rig that discards
# it cannot explain its own failures.
#
# **It does NOT build the CEF bundle.** These are transcript surfaces and
# every one of them paints over a fixture with no page in it; a ~90 s, 362 MB
# bundle build to photograph a tool row is a cost paid for nothing. It DOES
# run `cargo build`, because `bundle-app.sh --features web` leaves a web
# binary at `target/debug/eave` and a loose web binary panics in cef-rs's
# `library_loader.rs:20` rather than degrading.
#
# The replay is run SLOWLY and photographed mid-flight: at the end of a
# fixture the app exits, so a shot taken after it has drained is a shot of
# nothing. `EAVE_REPLAY_SPEED` is rows-per-second.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
LABEL="${1:?usage: bz-capture.sh <label>}"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

echo "building (no --features web, on purpose — see the header)…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# Short path: the engine's unix socket is bounded by SUN_LEN (~104 bytes) and
# a `mktemp -d` under $TMPDIR has burned a slice before.
DATA="${TMPDIR:-/tmp}/bz"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"

LOG="${TMPDIR:-/tmp}/bz-app.log"
export EAVE_REDUCE_MOTION=0
EAVE_DATA_DIR="$DATA" \
EAVE_REPLAY="$ROOT/fixtures/real-claude-2026-08-16.tap.jsonl" \
EAVE_REPLAY_SPEED=3 \
  "$BIN" >"$LOG" 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep); stderr at $LOG"
reap() { pkill -P "$APP_PID" 2>/dev/null || true; kill "$APP_PID" 2>/dev/null || true; }
trap reap EXIT

WIN=""
for _ in $(seq 1 45); do
  WIN="$(python3 "$ROOT/scripts/as-window.py" "$APP_PID" || true)"
  [ -n "$WIN" ] && break
  sleep 1
done
[ -n "$WIN" ] || { echo "no window from pid $APP_PID — stderr:" >&2; tail -20 "$LOG" >&2; exit 1; }
read -r WINID WX WY WW WH <<<"$WIN"
echo "window $WINID at ${WX},${WY} ${WW}x${WH}"

raise() {
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 \
    || echo "note: could not raise; a shot of another app is occlusion"
  sleep 1
}
shot() {
  raise; sleep 1
  screencapture -x -o -l "$WINID" "$SHOTS/bz-$LABEL-$1.png"
  echo "captured bz-$LABEL-$1.png ($(stat -f%z "$SHOTS/bz-$LABEL-$1.png") bytes)"
}

# Let enough of the fixture land that tool rows are on screen, then shoot
# twice a few seconds apart — the transcript is still growing, so two shots
# are two different depths through the same surfaces.
# Three depths, not one. The fixture's TOOL CALLS land early and scroll away
# as prose accumulates, so a single late shot photographs a transcript with
# no tool row in it — which is how the first cut of this script produced two
# perfectly good pictures of the wrong surface.
sleep 7
shot "tools"

# Open a diff. The changed-file list is in the rail's lower half and a file
# row toggles its diff pane — without this, the diff surface is never on
# screen and a "before/after" pair of the diff is two pictures of a file
# LIST. Coordinates are window-relative; the rail is 228 wide and the first
# file row sits just under the `changes` header.
raise
python3 "$ROOT/scripts/as-click.py" "$((WX + 90))" "$((WY + 565))" || true
sleep 2
shot "diff"
sleep 8
shot "transcript-a"
sleep 8
shot "transcript-b"

# The app must still have been alive for those to mean anything. A replay
# that had already exited would leave the last window contents on screen and
# photograph perfectly.
if kill -0 "$APP_PID" 2>/dev/null; then
  echo "ok: app was alive for both shots"
else
  echo "FAIL: the app exited before the shots — the fixture drained; lower EAVE_REPLAY_SPEED" >&2
  exit 1
fi
