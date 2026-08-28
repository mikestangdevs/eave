#!/usr/bin/env bash
# Slice AF — prove the selection wash clears its ground IN COMPOSITED
# PIXELS, not in tokens.
#
# `theme.rs`'s floor tests are arithmetic on the token: they composite
# `overlay_strong()` over a synthetic ground with `over()` and compare
# `units()`. That is the right test and it is not the same claim as this
# one. Between the token and the eye there is a real compositor, a glass
# surface sampling a real desktop, a fade with a current opacity, and a
# colour space. This script reads the number back off the screen.
#
# It answers two questions, and the SECOND one is the one that needed a
# screen:
#
#   1. Is the row you are IN a different colour from the rail around it,
#      by the margin the design says it must be. NOTE HONESTLY: this one
#      does not discriminate the bug Slice AF fixed. The selection wash
#      cleared the GROUND before the fix too (~19/255 at alpha 0.09); what
#      it did not clear was its NEIGHBOUR, the hover wash, and that is
#      asserted in `theme.rs::adjacent_states_are_a_step_apart`, not here.
#      This check is a regression guard, not the slice's evidence.
#   2. Does the compositor put the token where the arithmetic says it
#      does. `theme.rs` reasons in `over()` and `units()`; the screen goes
#      through a real compositor, a glass surface sampling a real desktop,
#      a fade with a current opacity, and a colour space. Nothing in the
#      suite had ever checked that those two agree. They do, to within a
#      couple of channel units — which is what licenses every floor in
#      `theme.rs` to be a claim about what a person sees.
#
# ---------------------------------------------------------------------
# WHAT THIS SCRIPT DELIBERATELY DOES NOT DO, AND WHY
#
# It does not photograph a HOVERED row. Two attempts failed and the
# failures are worth more than the shots would have been:
#
#   1. A synthesized `CGEventCreateMouseEvent(kCGEventMouseMoved)` posted
#      to `kCGHIDEventTap` moves the cursor and produces NO hover fade in
#      this app. The `new` button's interior measured 18/255 with the
#      pointer resting on it — bit-identical to the rail ground beside
#      it. Whether gpui ignores synthetic moves, or ignores moves while
#      the window is not key, is UNDETERMINED. Do not read this as "hover
#      is broken"; read it as "this instrument cannot see hover", which
#      is the same shape as the System Events trap in progress.md.
#   2. Clicking `new` at its measured offset did not deselect the row —
#      the main pane still read `nothing sent yet` rather than showing
#      the launcher. Whether the click missed a small target or `+ new`
#      no longer deselects is likewise UNDETERMINED, and worth one
#      focused look by somebody who can drive the app by hand.
#
# So the hover rung of the ladder is asserted by `theme.rs` and by nothing
# else. Stated here rather than left to be assumed.
# ---------------------------------------------------------------------
#
# Mechanics that each cost an earlier slice a wrong conclusion (see
# docs/progress.md §Traps):
#   * launch by ABSOLUTE path, or `pgrep -f` will not find the process;
#   * budget ~60 s a shot, or a slow shutter reads as a permission wall;
#   * a shot showing another app is OCCLUSION, not bad coordinates;
#   * `screencapture` leaves the window NON-FRONTMOST. A click issued
#     after a shot lands in whatever is behind — silently. Re-raise.
#   * System Events drives input ONLY. Window geometry comes from Quartz,
#     which measures a gpui window to the pixel at the same instant
#     System Events reports it has none.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${1:-$ROOT/docs-shots}"
mkdir -p "$OUT"

BIN="$ROOT/target/debug/eave"
# BUILD, do not merely check. `cargo test` builds test binaries and leaves
# `target/debug/eave` alone, so a run that only checked for the file would
# measure the last build — which is how Q2's first shot showed a layout
# that had already been fixed.
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

# The floor this asserts, quoted from the source of truth so the script
# cannot drift from the constant it is checking.
FLOOR="$(grep -A1 'pub const STATE_DELTA_FLOOR' crates/eave-app/src/theme.rs \
         | grep -o '[0-9]\+\.[0-9]\+' | head -1)"
echo "STATE_DELTA_FLOOR = $FLOOR/255 (read from theme.rs)"

DATA="$(mktemp -d)/eave-af"; mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/af-proof-log.md"

boot() {
  EAVE_DATA_DIR="$DATA" EAVE_LAUNCH_AT_BOOT="$1" "$BIN" >/dev/null 2>&1 &
  # 16 s, not 9: under a loaded machine a 9 s wait found no window and the
  # geometry probe crashed on a None. A short wait reads as a crash.
  sleep 16
  PID="$(pgrep -n -f "$BIN" || true)"
  [ -n "$PID" ] || { echo "app did not start" >&2; exit 1; }
}

boot 1                                    # seed one session
kill "$PID" 2>/dev/null || true; sleep 2
[ "$(find "$DATA" -name journal.jsonl | wc -l | tr -d ' ')" -ge 1 ] \
  || { echo "seeding produced no session" >&2; exit 1; }

boot 0                                    # boot for the measurement
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" >/dev/null 2>&1 || \
  echo "note: could not raise; a shot of another app is occlusion"
sleep 3

read -r WX WY WW WH < <(python3 - "$PID" <<'PY'
import sys, Quartz
pid = int(sys.argv[1]); best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    if w.get('kCGWindowOwnerPID') != pid:
        continue
    b = w['kCGWindowBounds']; a = b['Width'] * b['Height']
    if best is None or a > best[0]:
        best = (a, b)
if best is None:
    sys.exit("no window for pid %d — the app was not up yet" % pid)
b = best[1]
print(int(b['X']), int(b['Y']), int(b['Width']), int(b['Height']))
PY
)
echo "window ${WW}x${WH} at ${WX},${WY}"

# Select the first rail row. Offsets from the window origin, measured off
# a capture rather than guessed, and overridable so a layout change is a
# flag rather than an edit here.
ROW_X=$((WX + ${ROW_DX:-124})); ROW_Y=$((WY + ${ROW_DY:-120}))
osascript -e "tell application \"System Events\" to click at {$ROW_X, $ROW_Y}" >/dev/null 2>&1 || \
  echo "note: could not click; grant accessibility to the terminal"
sleep 1
# Park the pointer OFF the rail, so whatever hover contributes — even if
# this machine cannot photograph it — contributes nothing to the number.
python3 - "$((WX + WW - 40))" "$((WY + WH - 40))" <<'PY'
import sys, Quartz
Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(
    None, Quartz.kCGEventMouseMoved, (float(sys.argv[1]), float(sys.argv[2])), 0))
PY
sleep 2

echo "capturing (~50 s under load — not a hang)"
screencapture -x -o -t png "$OUT/af-rail-selected.png"
kill "$PID" 2>/dev/null || true

python3 - "$OUT/af-rail-selected.png" "$WX" "$WY" "$FLOOR" <<'PY'
import sys
from collections import Counter
from PIL import Image

path, wx, wy, floor = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4])
im = Image.open(path).convert("RGB")
# The capture is in device pixels; the window rect is in points. Recover
# the scale from the image rather than assuming 2x — an external display
# is 1x and would halve every offset.
s = im.size[0] / 1728.0

def mode(x0, y0, x1, y1):
    """Most common red channel over a region.

    The MODE, not the mean: these regions contain antialiased glyph edges
    and a rounded corner, and a mean would quietly average the card's fill
    with the text drawn on top of it. The mode is the fill.
    """
    c = Counter()
    for lx in range(x0, x1):
        for ly in range(y0, y1):
            c[im.getpixel((int(lx * s), int(ly * s)))[0]] += 1
    return c.most_common(1)[0][0]

card   = mode(wx + 30, wy + 108, wx + 220, wy + 132)
ground = mode(wx + 30, wy + 185, wx + 220, wy + 205)
d = card - ground

# What `theme.rs` says the compositor should produce. `overlay_strong` is
# hsla(220/360, 0.10, 0.90, ALPHA); its red channel — the channel `units()`
# quotes — is 0.89, i.e. 227/255. Both numbers are read out of the source
# so this cannot drift away from the token it is checking.
import re, pathlib
src = pathlib.Path("crates/eave-app/src/theme.rs").read_text()
alpha = float(re.search(
    r"pub fn overlay_strong\(\) -> Hsla \{\s*hsla\([^)]*?,\s*([0-9.]+)\s*\)", src).group(1))
WASH_R = 0.89 * 255.0
predicted = ground + alpha * (WASH_R - ground)
skew = abs(card - predicted)

print(f"  selected row fill : {card}/255")
print(f"  rail ground       : {ground}/255")
print(f"  delta             : {d}/255   floor {floor}")
print(f"  theme.rs predicts : {predicted:.1f}/255 at alpha {alpha}  (skew {skew:.1f})")

ok = True
if d < floor:
    print(f"AF_STATE_PROOF selected_delta={d} floor={floor} FAIL")
    ok = False
else:
    print(f"AF_STATE_PROOF selected_delta={d} floor={floor} PASS")

# 4/255 of slack, chosen to sit above the ~2/255 the colour space and the
# rounded corner's antialiasing actually cost and well below the 8/255
# floor itself — a skew large enough to matter would have to break this
# before it could hide inside a floor.
if skew <= 4.0:
    print(f"AF_TOKEN_FIDELITY skew={skew:.1f}/255 tolerance=4.0 PASS")
else:
    print(f"AF_TOKEN_FIDELITY skew={skew:.1f}/255 tolerance=4.0 FAIL — the "
          f"compositor is not landing the token where theme.rs reasons it "
          f"does, and every floor in that file is a claim about arithmetic "
          f"rather than about a person's eye")
    ok = False
if not ok:
    sys.exit(1)
PY
echo "shot: $OUT/af-rail-selected.png"
