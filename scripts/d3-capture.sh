#!/usr/bin/env bash
# Slice D3 stage 1 — photograph the rail at rest and mid-drag.
#
# Mechanics are `d1-capture.sh`'s, and every one of them cost an earlier
# slice a wrong conclusion (docs/progress.md §Traps): never `pgrep` for the
# app (eave is one binary in two roles and the newest match is normally the
# ENGINE, which owns no window) — `$!` only; capture BY WINDOW ID
# (`screencapture -l`), never screen-wide and never `-R` (it hangs on this
# machine and killing it wedges `replayd`); poll Quartz for the window
# rather than sleeping at it; re-raise between shots, because
# `screencapture` leaves the window non-frontmost; reap the engine by
# PARENTAGE.
#
# No CEF bundle, for D1's reason: what stage 1 changed is the rail's
# geometry, which is visible over a transcript, and a ~3 minute ~360 MB
# bundle build to photograph a sidebar is a cost paid for nothing on a
# machine at 95% disk.
#
# **The mid-drag shot leaves a mouse button held down between two
# processes.** `d3-drag.py grab` exits with the button down and
# `d3-drag.py drop` releases it; the EXIT trap below runs `drop`
# unconditionally, because a stuck left button survives this script and
# makes the desktop unusable.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

echo "building (no --features web, on purpose — see the header)…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# Short path: the engine's unix socket is bounded by SUN_LEN (~104 bytes).
DATA="${TMPDIR:-/tmp}/d3"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"
python3 "$ROOT/scripts/as-seed.py" "$DATA"
python3 "$ROOT/scripts/d1-seed-tabs.py" "$DATA"

export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/d3-shot-log.md"
# **The app's stderr is kept, not discarded.** A capture rig that throws it
# away cannot explain its own failures, and this script's guard has already
# caught one it could not explain (a drag that ended at RAIL_W_MIN instead of
# where it was released) precisely because the log was going to /dev/null.
APPLOG="${TMPDIR:-/tmp}/d3-app.log"
EAVE_DATA_DIR="$DATA" "$BIN" >"$APPLOG" 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"

RELEASED=0
cleanup() {
  # The button first: a stuck drag outlives this script.
  [ "$RELEASED" = "1" ] || python3 "$ROOT/scripts/d3-drag.py" drop 400 400 2>/dev/null || true
  pkill -P "$APP_PID" 2>/dev/null || true
  kill "$APP_PID" 2>/dev/null || true
}
trap cleanup EXIT

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
shot() {
  raise; sleep 2
  screencapture -x -o -l "$WINID" "$SHOTS/$1.png"
  echo "captured $SHOTS/$1.png ($(stat -f%z "$SHOTS/$1.png") bytes)"
}

SEAM_HOVER=228
sleep 6
shot "d3-rail-rest"

# **A resize you cannot discover is not a feature.** The seam is a hairline
# with no grip drawn on it, so the ONLY thing that says the rail can be
# dragged is the pointer changing over it -- which makes the pointer the
# affordance, and it has to be photographed like one. `-C` is what puts the
# cursor in the shot; without it this frame is indistinguishable from
# `d3-rail-rest` and proves nothing.
raise
python3 "$ROOT/scripts/d3-hover.py" "$((WX + SEAM_HOVER))" "$((WY + 400))"
sleep 1
screencapture -x -o -C -l "$WINID" "$SHOTS/d3-rail-seam-cursor.png"
echo "captured $SHOTS/d3-rail-seam-cursor.png (with the pointer)"

# ── mid-drag ───────────────────────────────────────────────────────────────
#
# The seam sits at the rail's width; the handle straddles it, so grabbing at
# exactly RAIL_W is inside the target either way. 228 is `theme::RAIL_W` —
# a literal here because this script cannot read the crate, and checked by
# the probe below rather than trusted.
SEAM=228
TO=$((SEAM + 84))          # a visible widening, still inside RAIL_W_MAX 320
INSIDE=$((SEAM + 40))      # past the resting seam, short of the dragged one
probe() { python3 "$ROOT/scripts/d1-probe.py" "$SHOTS/$1.png" "$2" 400; }
REST_PX="$(probe d3-rail-rest $INSIDE)"

# **The gesture is retried, and the retry is announced.** The app's own
# arithmetic is right on every attempt — instrumented, it reports
# `grab=228 start=228 -> w=<x>` for each move it receives — but roughly one
# attempt in three never gets the moves. The window server throttles the
# synthetic gesture and re-synthesises it: the app saw x=234.42188,
# 235.17188, 235.46094, 235.75 and then nothing, while THIS SCRIPT only ever
# posts integers. Those four fractional values are the tell. Slowing the
# steps to 0.09 s over 16 (see `d3-drag.py`) reduced it and did not remove
# it, so the rig retries rather than claiming a determinism it does not have.
attempt=0
while :; do
  attempt=$((attempt + 1))
  raise
  python3 "$ROOT/scripts/d3-drag.py" grab "$((WX + SEAM))" "$((WY + 400))"
  python3 "$ROOT/scripts/d3-drag.py" drag "$((WX + SEAM))" "$((WY + 400))" "$((WX + TO))"
  # No `raise` here: raising mid-drag posts an activation click and ends it.
  # The settle is 2 s because the app is still draining moves when the last
  # one is posted.
  sleep 2
  # `-C` — WITH the cursor, and only here. The first cut of this shot came
  # out BYTE-IDENTICAL to the released one, which is the honest answer to
  # "what does a drag in progress look like": the rail follows the pointer
  # live and nothing else changes, because the affordance IS the pointer.
  # `col-resize` is the only thing on screen that says a divider is being
  # held, and `screencapture` omits the cursor unless asked.
  screencapture -x -o -C -l "$WINID" "$SHOTS/d3-rail-mid-drag.png"
  python3 "$ROOT/scripts/d3-drag.py" drop "$((WX + TO))" "$((WY + 400))"
  RELEASED=1
  sleep 1
  shot "d3-rail-dragged"
  if [ "$(probe d3-rail-dragged $INSIDE)" != "$REST_PX" ]; then
    [ "$attempt" -gt 1 ] && echo "note: the drag took $attempt attempts (synthetic input, see the header)"
    break
  fi
  if [ "$attempt" -ge 3 ]; then
    echo "FAIL: three synthetic drags in a row moved nothing" >&2
    echo "--- app stderr ($APPLOG) ---" >&2; tail -40 "$APPLOG" >&2
    exit 1
  fi
  echo "note: attempt $attempt moved nothing; retrying"
  RELEASED=0
done

# ── the guard ──────────────────────────────────────────────────────────────
#
# `cmp -s` is not strong enough and `d1-capture.sh` shipped that mistake
# once: two shots differed by a hover wash from a missed click and a byte
# comparison called it a pass.
#
# So the assertion is about the FRAME. Sample a point that is INSIDE the rail
# only after it widens: x is past the resting seam and short of the dragged
# one. At rest that point is content; dragged, it is rail. Two surfaces, two
# pixels.
DRAG_PX="$(probe d3-rail-dragged $INSIDE)"
echo "probe @${INSIDE},400 — rest:[$REST_PX] dragged:[$DRAG_PX]"
if [ "$REST_PX" = "$DRAG_PX" ]; then
  echo "FAIL: x=$INSIDE is the same surface before and after the drag — the seam did not move" >&2
  echo "--- app stderr ($APPLOG) ---" >&2; tail -40 "$APPLOG" >&2
  exit 1
fi
# And the rail is genuinely there at rest, so "it moved" is not "it vanished".
LEFT_REST="$(probe d3-rail-rest 100)"
LEFT_DRAG="$(probe d3-rail-dragged 100)"
if [ "$LEFT_REST" != "$LEFT_DRAG" ]; then
  echo "FAIL: the rail's own body changed colour across the drag ([$LEFT_REST] -> [$LEFT_DRAG]) — this is measuring something else" >&2
  exit 1
fi
echo "ok: the seam moved outward and the rail is still a rail"
echo "app stderr: $APPLOG"
