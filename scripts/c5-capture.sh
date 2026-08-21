#!/usr/bin/env bash
# Slice C5 — photograph the strip and the omnibox over a real page.
#
# Mechanics are `as-capture.sh`'s, and every one of them cost an earlier
# slice a wrong conclusion (docs/progress.md §Traps):
#   * never `pgrep` for the app — eave is one binary in two roles and the
#     newest match is normally the ENGINE, which owns no window. `$!` only.
#   * capture BY WINDOW ID (`screencapture -l`), never a screen-wide shot:
#     a shot that comes back showing a browser is occlusion, not bad
#     coordinates. (`screencapture -R` hangs on this machine, and killing
#     it wedges `replayd`.)
#   * poll Quartz for the window instead of sleeping at it.
#   * `screencapture` leaves the window non-frontmost, so anything issued
#     after a shot lands in whatever is behind it. Re-raise between shots.
#   * reap the ENGINE as well as the app, by parentage.
#
# TWO THINGS THIS ONE ADDS.
#
# 1. **It runs the BUNDLE, with `--features web`.** A loose `target/debug/eave`
#    cannot render a page at all — CEF resolves its framework and its five
#    helper `.app`s relative to its own bundle path — so a capture of the
#    omnibox over a page has to come out of the shipped artefact. That is a
#    ~3 minute build and ~360 MB, which is why this is a script and not a
#    step in the boot smoke.
#
# 2. **It types.** A synthesized `CGEventKeyboardEvent` DOES reach a gpui
#    window — corrected 2026-08-21, having been recorded as impossible —
#    but only after a click has given the window focus. So every run clicks
#    an inert part of the window first. Without that click the keystrokes
#    are delivered to whatever was frontmost and the shot is of an omnibox
#    that never opened, which reads as a code bug.
#
# The strip's contents are seeded into B1's own journal rather than driven
# through the interface (`scripts/c5-seed.py`): the strip is a projection of
# that file, so seeding it is the most direct statement of what the shot is
# of. History is seeded for the same reason — an omnibox photographed over
# an empty history is a photograph of a text field.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

echo "building and bundling (--features web)…"
BUILD_LOG="${TMPDIR:-/tmp}/c5-capture-build.log"
scripts/bundle-app.sh --features web >"$BUILD_LOG" 2>&1 ||
  { echo "bundle failed" >&2; tail -30 "$BUILD_LOG" >&2; exit 1; }
APP="$ROOT/target/bundle/eave.app"
BIN="$APP/Contents/MacOS/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# A SHORT data dir. The engine opens a unix socket inside it and the path
# has to fit in SUN_LEN (~104 bytes), which a temp dir under a long TMPDIR
# does not — it fails with "path must be shorter than SUN_LEN" and reads
# like a CEF problem. Per-run as well, because CEF takes an exclusive lock
# on its cache root and a shared one would mean this cannot run while eave
# is open.
DATA="/tmp/eave-c5-$$"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"
python3 "$ROOT/scripts/c5-seed.py" "$DATA"

export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/c5-shot-log.md"
EAVE_DATA_DIR="$DATA" "$BIN" >"${TMPDIR:-/tmp}/c5-capture-app.log" 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"
reap() { pkill -P "$APP_PID" 2>/dev/null || true; kill "$APP_PID" 2>/dev/null || true; }
# `rm -rf` on the way out is best-effort: CEF is still flushing its cache
# root as the process dies, so a "Directory not empty" here is a race and
# not a failure — and under `set -e` it was enough to make a successful
# capture exit 1.
trap 'reap; sleep 1; rm -rf "$DATA" 2>/dev/null || true' EXIT

WIN=""
for _ in $(seq 1 60); do
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
# A CLICK, not a raise. `screencapture` leaves the window non-frontmost and
# a raised window is not a KEY window — a key event posted to a non-key
# window is delivered to whatever IS key, which from a screenshot is
# indistinguishable from the app ignoring the keystroke. The first run of
# this script raised and typed, and both shots came back byte-identical.
#
# The point is chosen to be inert: the titlebar, past the strip's `+` and
# well clear of the traffic lights.
focus() {
  raise
  python3 "$ROOT/scripts/as-click.py" "$((WX + WW - 120))" "$((WY + 8))"
  sleep 1
}
shot() {
  raise
  sleep 2
  screencapture -x -o -l "$WINID" "$SHOTS/$1.png"
  echo "captured $SHOTS/$1.png ($(stat -f%z "$SHOTS/$1.png") bytes)"
}

# Give CEF time to create the browser and paint. The page is a real network
# fetch, so this is the one part of the run that is not hermetic — the shot
# is still valid if it fails (the strip is what is being photographed) and
# the app log says so either way.
sleep 12

# Focus. A raised window is not a KEY window and neither typing nor hover
# reaches one — click an inert part of the titlebar first, well clear of
# every control.
focus
shot "c5-strip"

# ⌘L, then a prefix that several seeded visits share, so the dropdown in the
# shot has real ranked rows in it rather than one row that only proves the
# field accepts characters.
focus
python3 "$ROOT/scripts/c5-type.py" "cmd-l" "${C5_QUERY:-doc}"
sleep 2
shot "c5-omnibox"

if cmp -s "$SHOTS/c5-strip.png" "$SHOTS/c5-omnibox.png"; then
  echo "the two shots are byte-identical — the keystrokes did not land" >&2
  exit 1
fi

echo done
