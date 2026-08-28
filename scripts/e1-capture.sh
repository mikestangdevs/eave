#!/usr/bin/env bash
# Slice E1 — photograph the find bar, the page's context menu and the zoom
# fact over a real page, and MEASURE a live resize.
#
# Mechanics are `c5-capture.sh`'s, and every one of them cost an earlier
# slice a wrong conclusion (docs/progress.md §Traps):
#   * never `pgrep` for the app — eave is one binary in two roles and the
#     newest match is normally the ENGINE, which owns no window. `$!` only.
#   * capture BY WINDOW ID (`screencapture -l`), never screen-wide: a shot
#     that comes back showing a browser is occlusion, not bad coordinates.
#     (`screencapture -R` hangs on this machine and killing it wedges
#     `replayd`.)
#   * poll Quartz for the window instead of sleeping at it.
#   * `screencapture` leaves the window non-frontmost, so anything issued
#     after a shot lands in whatever is behind it. Re-raise between shots.
#   * a raised window is not a KEY window: click before typing.
#   * reap the ENGINE as well as the app, BY PARENTAGE.
#
# ⚠️ **It must not run `stop-eave.py`, and neither `dev.sh` nor `run-app.sh`
# may be used to launch it.** Those kill every eave on the machine from
# every checkout, and this repo is worked on by several sessions in
# several worktrees at once — a capture that takes a peer's window down is
# a capture that costs somebody else an hour. Everything here is scoped to
# the process this script started (`$!`) and to a per-run data dir.
#
# THREE THINGS THIS ONE ADDS OVER C5.
#
# 1. **A local fixture, not a network fetch.** `scripts/e1-page.html`
#    contains the word `eave` seventeen times, one link, and two pinned
#    probe squares. The find tally is then checkable against arithmetic
#    rather than against itself, and a red run means eave broke rather than
#    that a website changed.
#
# 2. **It right-clicks.** `scripts/e1-rclick.py`, for the same reason
#    `as-click.py` exists at all — and this is the first thing in eave that
#    has ever needed the right button.
#
# 3. **It resizes the window and measures what came out.** The stretch is
#    one or two frames long, so a shutter cannot catch it; what the shot
#    CAN settle is whether the steady state is a re-laid-out page or a
#    scaled one, which `scripts/e1-probe.py` reads off the two squares.
#    The transient itself is measured from the trace
#    (`[web] resize settled ... after N stale frame(s)`), and
#    `EAVE_WEB_STRETCH=1` runs the same sweep with the pre-E1 behaviour so
#    the before and the after come from ONE binary.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
mkdir -p "$SHOTS"

APP="$ROOT/target/bundle/eave.app"
BIN="$APP/Contents/MacOS/eave"
CEF="$APP/Contents/Frameworks/Chromium Embedded Framework.framework"
BUILD_LOG="${TMPDIR:-/tmp}/e1-capture-build.log"

if [[ -x "$BIN" && -d "$CEF" ]]; then
  # Incremental: compile and restamp the Mach-Os inside the bundle that is
  # already here. `bundle-app.sh` re-dittos 317 MB of Chromium and deletes
  # the .app first, which is right for a cold assemble and wasteful here.
  echo "rebuilding into the existing bundle…"
  cargo build --offline --features web --bin eave --bin eave-web-helper \
    >"$BUILD_LOG" 2>&1 || { echo "build failed" >&2; tail -30 "$BUILD_LOG" >&2; exit 1; }
  install -m 755 "$ROOT/target/debug/eave" "$BIN"
  for entry in "Helper" "Helper (GPU)" "Helper (Renderer)" "Helper (Plugin)" "Helper (Alerts)"; do
    install -m 755 "$ROOT/target/debug/eave-web-helper" \
      "$APP/Contents/Frameworks/eave ${entry}.app/Contents/MacOS/eave ${entry}"
  done
else
  echo "building and bundling (--features web)…"
  scripts/bundle-app.sh --features web >"$BUILD_LOG" 2>&1 ||
    { echo "bundle failed" >&2; tail -30 "$BUILD_LOG" >&2; exit 1; }
fi
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# A SHORT data dir. The engine opens a unix socket inside it and the path
# has to fit in SUN_LEN (~104 bytes). Per-run as well, because CEF takes an
# exclusive lock on its cache root — and because a peer's eave is very
# likely holding the real one.
DATA="/tmp/eave-e1-$$"
rm -rf "$DATA"; mkdir -p "$DATA/sessions"
PAGE="file://$ROOT/scripts/e1-page.html"
python3 "$ROOT/scripts/e1-seed.py" "$DATA" "$PAGE"

# Report foreign eaves; never kill one. Several sessions work in several
# checkouts of this repo at once, and a capture that takes a colleague's
# window down costs them an hour.
FOREIGN="$(ps -eo pid,command | grep -E "eave\.app/Contents/MacOS/eave$" | grep -v "$ROOT" || true)"
if [ -n "$FOREIGN" ]; then
  echo "⚠️  another checkout's eave is running. This rig does not kill it —"
  echo "    but if its dev.sh relaunches mid-run, the keyboard goes there"
  echo "    and this run will fail at the first shot_changed guard:"
  echo "$FOREIGN" | sed 's/^/      /'
fi

APP_LOG="${TMPDIR:-/tmp}/e1-capture-app.log"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/e1-shot-log.md"
# `EAVE_WEB_STRETCH=1` runs the same binary with the pre-E1 resize
# behaviour, so the before and the after are one artefact. **Exported
# rather than written as an inline `${VAR:+VAR=1}` prefix**: bash expands
# variables after it has parsed the command, so the expansion lands in
# COMMAND position and the run dies with `EAVE_WEB_STRETCH=1: command not
# found`. Which is what the first attempt did.
if [ -n "${EAVE_WEB_STRETCH:-}" ]; then export EAVE_WEB_STRETCH=1; fi
EAVE_DATA_DIR="$DATA" EAVE_WEB_TRACE=1 "$BIN" >"$APP_LOG" 2>&1 &
APP_PID=$!
echo "app pid $APP_PID (ours, by \$! — never by pgrep)"
# **The engine is NOT a child, and `pkill -P` therefore misses it.** eave
# detaches it on purpose (`eave-engine/src/client.rs:107`: "the engine is
# detached on purpose; we never wait on it"), so parentage — which is the
# rule every other capture script in here follows, and the right rule for
# the CEF helpers — leaves one engine per run alive holding a temp data
# dir. Two runs of this script leaked two of them before the second run
# noticed.
#
# So the engine is reaped by its DATA DIR, which is unique to this run and
# is the one thing that cannot match a peer's process. Never by the name
# `eave`: this machine routinely has several checkouts running at once and
# a name match would take a colleague's window down.
reap() {
  pkill -P "$APP_PID" 2>/dev/null || true
  kill "$APP_PID" 2>/dev/null || true
  pkill -f -- "--data-dir $DATA" 2>/dev/null || true
}
# `rm -rf` on the way out is best-effort: CEF is still flushing its cache
# root as the process dies, so "Directory not empty" is a race and not a
# failure — and under `set -e` that was once enough to make a successful
# capture exit 1.
trap 'reap; sleep 1; rm -rf "$DATA" 2>/dev/null || true' EXIT

# **Wait for a window that is actually the SHELL.** `as-window.py` takes
# the largest window belonging to this process tree over 200x200, which is
# the right rule for a plain gpui app and not enough for a CEF one: a run
# of this script came back with a 260x332 window, photographed it, and
# reported "the page did not paint". The shell is ~1080x761; nothing else
# eave puts on screen is close.
WIN=""
for _ in $(seq 1 60); do
  CANDIDATE="$(python3 "$ROOT/scripts/as-window.py" "$APP_PID" || true)"
  if [ -n "$CANDIDATE" ]; then
    read -r _ _ _ cw _ <<<"$CANDIDATE"
    if [ "$cw" -ge 800 ]; then WIN="$CANDIDATE"; break; fi
  fi
  sleep 1
done
[ -n "$WIN" ] || { echo "no shell window from pid $APP_PID" >&2; tail -40 "$APP_LOG" >&2; exit 1; }
read -r WINID WX WY WW WH <<<"$WIN"
echo "window $WINID at ${WX},${WY} ${WW}x${WH}"

raise() {
  osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 \
    || echo "note: could not raise; a shot of another app is occlusion"
  sleep 1
}

# ── FRONTMOST IS ASSERTED, NOT ASSUMED ─────────────────────────────────────
#
# **A whole run of this script came back with three byte-identical
# screenshots and no error**, because a peer session's `scripts/dev.sh` in
# another checkout was relaunching its own eave (`open -n eave.app`) on
# every save. That steals frontmost between the `raise` and the event, so
# every synthetic keystroke and the right-click went to somebody else's
# window. Nothing failed; the shots simply showed a find bar that never
# opens and a context menu that never appears — which is indistinguishable
# from the slice being broken.
#
# `stop-eave.py` — which `dev.sh` runs on every rebuild — also kills every
# eave on the machine from every checkout, so the same run later took a
# SIGTERM two resizes into a four-step sweep. Neither is fixable from here;
# both are detectable, and a rig that cannot tell "my feature is broken"
# from "somebody took the keyboard" is worse than no rig.
front_pid() {
  osascript -e 'tell application "System Events" to unix id of first process whose frontmost is true' 2>/dev/null || true
}
ensure_front() {
  for _ in $(seq 1 10); do
    kill -0 "$APP_PID" 2>/dev/null || {
      echo "FAIL: our eave (pid $APP_PID) is gone — a peer's stop-eave.py" >&2
      echo "      kills every eave on this machine from every checkout." >&2
      tail -20 "$APP_LOG" >&2
      exit 1
    }
    raise
    [ "$(front_pid)" = "$APP_PID" ] && return 0
  done
  echo "FAIL: eave never became frontmost — something else is taking the" >&2
  echo "      keyboard (a peer checkout's dev.sh relaunching its window?)." >&2
  echo "      frontmost is pid $(front_pid), ours is $APP_PID." >&2
  exit 1
}
# A CLICK, not a raise: a raised window is not a KEY window, and a key
# event posted to a non-key window goes to whatever IS key — which from a
# screenshot is indistinguishable from the app ignoring the keystroke.
#
# **The point is in the PAGE, and `c5-capture.sh`'s point is wrong here.**
# That script clicks the titlebar at `WW - 120, 8`, which was inert in C5's
# frame. In D1's frame that region is the pane header's ADDRESS field, and
# clicking it opens the omnibox — which owns the whole keyboard, so the
# first run of this rig photographed a find bar with an empty field and an
# omnibox containing the word that was meant to go in it.
#
# The empty lower half of the fixture is inert (no link, no control) and
# clicking it also gives the PAGE focus, which is what a person would have
# done before pressing ⌘F anyway.
# **A CLICK, not a raise — and it is needed before EVERY step, not once.**
# `c5-capture.sh` records that a raised window is not a KEY window, and
# `screencapture` leaves the window non-frontmost, so a keystroke issued
# after a shot lands in whatever is behind it. This rig learned the second
# half the hard way: a run typed ⌘0 after a shot, the app stayed at 150%,
# and the resize sweep that followed was measured on a zoomed page. Nothing
# errored, and the number that came out was plausible.
#
# So `focus` opens every interaction, and `type_` is only for keystrokes
# that CONTINUE one.
focus() { ensure_front; python3 "$ROOT/scripts/as-click.py" "$((WX + WW / 2))" "$((WY + WH - 140))"; sleep 1; }
type_() { ensure_front; python3 "$ROOT/scripts/c5-type.py" "$@"; }
shot() {
  ensure_front; sleep 2
  screencapture -x -o -l "$WINID" "$SHOTS/$1.png"
  echo "captured $SHOTS/$1.png ($(stat -f%z "$SHOTS/$1.png") bytes)"
}
# A shot that MUST differ from an earlier one, or the interaction it was
# supposed to capture did not happen. Slice C5 learned the same thing the
# same way and guarded only its last pair; this guards every step, because
# a run that loses the keyboard loses all of them.
shot_changed() {
  shot "$1"
  if cmp -s "$SHOTS/$1.png" "$SHOTS/$2.png"; then
    echo "FAIL: $1.png is byte-identical to $2.png — the interaction before" >&2
    echo "      it did not reach eave. Check the app log for evidence and" >&2
    echo "      check whether another window took frontmost." >&2
    exit 1
  fi
}
# Re-read the window's geometry — a resize moves it.
regeom() {
  read -r WINID WX WY WW WH <<<"$(python3 "$ROOT/scripts/as-window.py" "$APP_PID")"
}
resize() {
  osascript -e "tell application \"System Events\" to set size of window 1 of (first process whose unix id is $APP_PID) to {$1, $2}" >/dev/null
  sleep 1
  regeom
  echo "  window now ${WW}x${WH}"
}

# CEF has to create the browser and paint. A local file, so this is the one
# part that is not a network fetch and 8 s is generous for it.
sleep 10
focus
shot "e1-page"

# ── 1. find in page ────────────────────────────────────────────────────────
# `eave` appears seventeen times in the fixture's rendered text, so the bar
# should say "1 of 17" — a number this rig can check rather than read back.
focus
type_ "cmd-f"
sleep 1
type_ "eave"
sleep 2
shot_changed "e1-find" "e1-page"
focus
type_ "escape"
sleep 1

# ── 2. the page's context menu, over a LINK ────────────────────────────────
#
# **The link is FOUND, not assumed.** The fixture puts it on a flat chip of
# one exact colour and `e1-probe.py` reports that chip's centre out of the
# first shot, so the right-click lands where the link actually is. A
# hardcoded offset into a page's prose is a coordinate that goes silently
# wrong after any layout change — and this rig resizes the window later, so
# "any layout change" is something it does to itself.
#
# It is also the run's first real assertion: if the chip is not in the
# shot, the page never painted, and a right-click at a guessed coordinate
# would photograph an empty menu and call it a pass.
LINKPT="$(python3 "$ROOT/scripts/e1-probe.py" "$SHOTS/e1-page.png" \
  --centre link --window-w "$WW" 2>/dev/null || true)"
if [ -z "$LINKPT" ]; then
  echo "FAIL: no link chip in e1-page.png — the page did not paint" >&2
  tail -30 "$APP_LOG" >&2
  exit 1
fi
read -r LX LY <<<"$LINKPT"
echo "link chip centre at window point ${LX},${LY}"
focus
python3 "$ROOT/scripts/e1-rclick.py" "$((WX + LX))" "$((WY + LY))"
sleep 2
shot_changed "e1-menu" "e1-page"
type_ "escape"
sleep 1
# The menu is modal — a click would dismiss it, so escape is issued after
# a bare `ensure_front` and its landing is checked by the shot below.

# ── 3. zoom ────────────────────────────────────────────────────────────────
focus
type_ "cmd-=" "cmd-=" "cmd-="
sleep 2
shot_changed "e1-zoom" "e1-page"
focus
type_ "cmd-0"
sleep 2

# ── 4. the resize sweep ────────────────────────────────────────────────────
echo "resize sweep:"
for size in "1280 820" "980 820" "1420 900" "1080 780"; do
  read -r w h <<<"$size"
  resize "$w" "$h"
  sleep 2
done
shot "e1-resized"

echo
echo "── probes ─────────────────────────────────────────────────────────────"
python3 "$ROOT/scripts/e1-probe.py" "$SHOTS/e1-page.png"
python3 "$ROOT/scripts/e1-probe.py" "$SHOTS/e1-resized.png"
echo
echo "── assertions ─────────────────────────────────────────────────────────"
python3 - "$SHOTS/e1-page.png" "$SHOTS/e1-resized.png" "$ROOT/scripts/e1-probe.py" <<'ASSERT'
"""The three things a resize capture has to settle, checked rather than shown.

Printing two probe lines and letting a reader compare them is what the
first version of this script did, and it is how a run finished at 150%
zoom with nobody noticing: the numbers were right there and both plausible.
"""
import json
import subprocess
import sys

PROBE = sys.argv[3]


def read(path):
    out = subprocess.run(
        [sys.executable, PROBE, path, "--json"], capture_output=True, text=True
    )
    if out.returncode != 0:
        raise SystemExit(f"probe failed on {path}: {out.stderr}")
    return json.loads(out.stdout)


before, after = read(sys.argv[1]), read(sys.argv[2])
fails = []

# The probe squares are 40 logical px. `screencapture` writes 2x here, so
# 80 device px — and a SCALED frame is exactly what makes this number
# something other than 80.
for name, d in (("before", before), ("after", after)):
    for which in ("tl", "br"):
        b = d[which]
        if b is None:
            fails.append(f"{name}: the {which} probe is missing")
            continue
        if abs(b[2] - 80) > 2 or abs(b[3] - 80) > 2:
            fails.append(
                f"{name}: the {which} probe is {b[2]}x{b[3]} device px, not 80x80 — "
                f"the frame was SCALED to fit its bounds, or the page is zoomed"
            )

# Top-left anchored: the page's origin does not move when the window
# resizes, because the well's origin does not.
if before["tl"] and after["tl"] and before["tl"][:2] != after["tl"][:2]:
    fails.append(
        f"the page's top-left moved from {before['tl'][:2]} to {after['tl'][:2]} "
        f"across a resize"
    )

# Pinned to the viewport's bottom-right: only a page that RE-LAID OUT at the
# new size puts it against the new right edge. A merely-scaled frame leaves
# it in the old proportional place.
for name, d in (("before", before), ("after", after)):
    if not d["br"]:
        continue
    right = d["br"][0] + d["br"][2]
    if abs(right - d["image"][0]) > 4:
        fails.append(
            f"{name}: the bottom-right probe ends at x={right} in a "
            f"{d['image'][0]}px image — the page did not lay out at this width"
        )

if fails:
    for f in fails:
        print("FAIL:", f)
    raise SystemExit(1)
print("ok: both probes are 40x40 logical, the page is top-left anchored,")
print("    and it re-laid out against each window's own right edge.")
ASSERT
echo
echo "── the transient, from the trace ──────────────────────────────────────"
grep -E "resize (wants|settled)" "$APP_LOG" || echo "(no resize lines in the trace)"
echo
echo "app log: $APP_LOG"
