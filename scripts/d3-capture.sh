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
EAVE_DATA_DIR="$DATA" "$BIN" >/dev/null 2>&1 &
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

sleep 6
shot "d3-rail-rest"

# ── mid-drag ───────────────────────────────────────────────────────────────
#
# The seam sits at the rail's width; the handle straddles it, so grabbing at
# exactly RAIL_W is inside the target either way. 228 is `theme::RAIL_W` —
# written here as a literal because this script cannot read the crate, and
# checked by the probe below rather than trusted.
SEAM=228
TO=$((SEAM + 84))          # a visible widening, still inside RAIL_W_MAX 320
raise
python3 "$ROOT/scripts/d3-drag.py" grab "$((WX + SEAM))" "$((WY + 400))"
python3 "$ROOT/scripts/d3-drag.py" drag "$((WX + SEAM))" "$((WY + 400))" "$((WX + TO))"
# No `raise` here: raising mid-drag would post an activation click and end it.
sleep 1
screencapture -x -o -l "$WINID" "$SHOTS/d3-rail-mid-drag.png"
echo "captured $SHOTS/d3-rail-mid-drag.png"
python3 "$ROOT/scripts/d3-drag.py" drop "$((WX + TO))" "$((WY + 400))"
RELEASED=1
sleep 1
shot "d3-rail-dragged"

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
probe() { python3 "$ROOT/scripts/d1-probe.py" "$SHOTS/$1.png" "$2" 400; }
INSIDE=$((SEAM + 40))
REST_PX="$(probe d3-rail-rest $INSIDE)"
DRAG_PX="$(probe d3-rail-dragged $INSIDE)"
echo "probe @${INSIDE},400 — rest:[$REST_PX] dragged:[$DRAG_PX]"
if [ "$REST_PX" = "$DRAG_PX" ]; then
  echo "FAIL: x=$INSIDE is the same surface before and after the drag — the seam did not move" >&2
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
