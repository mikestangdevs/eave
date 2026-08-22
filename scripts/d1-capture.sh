#!/usr/bin/env bash
# Slice D1 — photograph the frame, open and shut.
#
# Mechanics are `as-capture.sh`'s and every one of them cost an earlier slice
# a wrong conclusion (docs/progress.md §Traps): never `pgrep` for the app
# (eave is one binary in two roles and the newest match is normally the
# ENGINE, which owns no window) — `$!` only; capture BY WINDOW ID
# (`screencapture -l`), never screen-wide, and never `-R` (it hangs on this
# machine and killing it wedges `replayd`); poll Quartz for the window
# instead of sleeping at it; re-raise between shots, because `screencapture`
# leaves the window non-frontmost; reap the engine by PARENTAGE.
#
# **This one deliberately does NOT build the CEF bundle.** `c5-capture.sh`
# does, because it is photographing an omnibox over a real page and CEF
# resolves its framework relative to its own bundle path. What D1 changed is
# the FRAME — who owns the window's edges, where the strip lives, what a
# collapse leaves behind — and every one of those is visible over a
# transcript. A ~3 minute, ~360 MB bundle build to photograph a rail would
# be a cost paid for nothing, and on this machine (99% disk, swap at 16.7 of
# 17.4 GB) it is a cost that has OOM-killed two builds today.
#
# The page HEADER — the address, the two arrows, reload — is therefore NOT
# in these shots. It is asserted by four harness tests and stated as
# uncaptured in the report rather than implied.
#
# ⚠️ `bundle-app.sh --features web` overwrites `target/debug/eave` with a web
# build, and a loose web binary PANICS in cef-rs's `library_loader.rs:20`
# rather than degrading. If a previous capture left one there, this script's
# `cargo build` puts the plain binary back — which is why it builds rather
# than assuming.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

echo "building (no --features web, on purpose — see the header)…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# Short path: the engine's unix socket is bounded by SUN_LEN (~104 bytes) and
# a `mktemp -d` under $TMPDIR has burned a slice before.
DATA="${TMPDIR:-/tmp}/d1"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"
python3 "$ROOT/scripts/as-seed.py" "$DATA"
python3 "$ROOT/scripts/d1-seed-tabs.py" "$DATA"

export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/d1-shot-log.md"
EAVE_DATA_DIR="$DATA" "$BIN" >/dev/null 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"
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
click() { raise; python3 "$ROOT/scripts/as-click.py" "$((WX + $1))" "$((WY + $2))"; sleep 1; }
shot() {
  raise; sleep 2
  screencapture -x -o -l "$WINID" "$SHOTS/$1.png"
  echo "captured $SHOTS/$1.png ($(stat -f%z "$SHOTS/$1.png") bytes)"
}

sleep 6
shot "d1-frame-open"

# **The toggle is at a DIFFERENT x in each state, and the first cut of this
# script missed that.** It lives on the pane header, and the header starts
# where the content region starts — so with the rail open that is
# `RAIL_W (248) + hairline + SPACE_3 (12)` plus half the 28 px control, and
# with the rail shut it is `TRAFFIC_CLEARANCE (86)` plus the same half,
# because the header then owns the window's own left edge. One coordinate
# for both states clicks the rail's empty top region in one of them.
click 275 16
shot "d1-frame-shut"

click 100 16
shot "d1-frame-reopened"

# ── the guard ──────────────────────────────────────────────────────────────
#
# **`cmp -s` is not strong enough and this script shipped that mistake once.**
# The first pair differed — by a hover wash on one row, because the missed
# click still moved the pointer — and a byte comparison called it a pass
# while the rail was plainly still on screen in the "shut" shot.
#
# So the assertion is about the FRAME: sample x=100, y=400. With the menu
# open that point is inside the rail; with it shut the rail is gone and the
# content is there. Two different surfaces, so two different pixels.
probe() { python3 "$ROOT/scripts/d1-probe.py" "$SHOTS/$1.png" 100 400; }
OPEN_PX="$(probe d1-frame-open)"
SHUT_PX="$(probe d1-frame-shut)"
BACK_PX="$(probe d1-frame-reopened)"
echo "probe @100,400 — open:[$OPEN_PX] shut:[$SHUT_PX] reopened:[$BACK_PX]"
if [ "$OPEN_PX" = "$SHUT_PX" ]; then
  echo "FAIL: x=100 is the same surface open and shut — the toggle click did not land" >&2
  exit 1
fi
if [ "$OPEN_PX" != "$BACK_PX" ]; then
  echo "FAIL: reopening did not restore the rail ([$OPEN_PX] -> [$BACK_PX])" >&2
  exit 1
fi
echo "ok: the rail is at x=100 when open, gone when shut, and back on reopen"
