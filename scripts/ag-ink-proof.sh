#!/usr/bin/env bash
# Slice AG — prove the INK lands where theme.rs says, in composited pixels.
#
# `scripts/af-state-proof.sh` did this for a SURFACE token: it reads the
# selected row's fill off a PNG and checks it against the alpha arithmetic
# in `theme.rs` (skew 1.8–2.0/255 against a 4/255 tolerance). That is what
# licenses the surface floors to be claims about what a person sees.
#
# Slice AG's floors are about INK on those surfaces, and ink goes through a
# different path: it is opaque, it is shaped by the macOS text system, and
# it is antialiased. None of that had ever been measured. This script reads
# three inks off the selected rail row — the one row that carries all three
# rungs of the ladder at once — and checks each against its token.
#
# WHAT IT MEASURES, AND THE ONE PLACE IT COULD MISLEAD
#
# A text band contains ink, surface, and every antialiased value between.
# So: the SURFACE is the mode of the band (Slice AF's rule — a mean would
# average the fill with the glyphs on it), and the INK is the 99.5th
# percentile. The percentile is a LOWER BOUND on the ink: it is only equal
# to the token where a glyph stem covers a whole device pixel. On this
# machine (2x, 11–12.5 pt text, ~2 device-pixel stems) it does, and the
# measured values land bit-exact. **On a 1x display it would read low, and
# the right conclusion there would be "the instrument lost the stem", not
# "the token moved".** Read a small negative skew as the shutter, not the
# palette.
#
# It measures ONE desktop — this machine's, which is dark. The arithmetic
# in `theme.rs` sweeps 52 backdrops and the hard end is a WHITE desktop; a
# shot here reads ~6:1 where the swept worst case is 3.4:1. This script is
# the bridge from token to pixel, not a substitute for the sweep.
#
# Traps inherited from af-state-proof.sh, all of them earned:
#   * launch by ABSOLUTE path or `pgrep -f` will not find the process;
#   * budget ~60 s a shot — a slow shutter reads as a permission wall;
#   * a shot showing another app is OCCLUSION, not bad coordinates;
#   * `screencapture` leaves the window NON-FRONTMOST; re-raise before any
#     click that follows a shot;
#   * System Events drives input ONLY; geometry comes from Quartz.
#   * No hover is photographed here, for the reason af-state-proof.sh
#     records: a synthesized mouse-move produces no hover fade in this app.
#     The hovered rungs are asserted by `theme.rs` and by nothing else.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${1:-$ROOT/docs-shots}"
mkdir -p "$OUT"

BIN="$ROOT/target/debug/eave"
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-ag"; mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/ag-proof-log.md"

boot() {
  EAVE_DATA_DIR="$DATA" EAVE_LAUNCH_AT_BOOT="$1" "$BIN" >/dev/null 2>&1 &
  sleep 16
  PID="$(pgrep -n -f "$BIN" || true)"
  [ -n "$PID" ] || { echo "app did not start" >&2; exit 1; }
}

boot 1                                    # seed one session
kill "$PID" 2>/dev/null || true; sleep 2
[ "$(find "$DATA" -name journal.jsonl | wc -l | tr -d ' ')" -ge 1 ] \
  || { echo "seeding produced no session" >&2; exit 1; }

boot 0
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" >/dev/null 2>&1 || \
  echo "note: could not raise; a shot of another app is occlusion"
sleep 3

read -r WID WX WY WW WH < <(python3 - "$PID" <<'PY'
import sys, Quartz
pid = int(sys.argv[1]); best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    if w.get('kCGWindowOwnerPID') != pid:
        continue
    b = w['kCGWindowBounds']; a = b['Width'] * b['Height']
    if best is None or a > best[0]:
        best = (a, b, w['kCGWindowNumber'])
if best is None:
    sys.exit("no window for pid %d — the app was not up yet" % pid)
b = best[1]
print(best[2], int(b['X']), int(b['Y']), int(b['Width']), int(b['Height']))
PY
)
echo "window ${WW}x${WH} at ${WX},${WY} (id ${WID})"

ROW_X=$((WX + ${ROW_DX:-124})); ROW_Y=$((WY + ${ROW_DY:-120}))
osascript -e "tell application \"System Events\" to click at {$ROW_X, $ROW_Y}" >/dev/null 2>&1 || \
  echo "note: could not click; grant accessibility to the terminal"
sleep 1
# Park the pointer off the rail: whatever hover contributes, it contributes
# nothing to these numbers.
python3 - "$((WX + WW - 40))" "$((WY + WH - 40))" <<'PY'
import sys, Quartz
Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(
    None, Quartz.kCGEventMouseMoved, (float(sys.argv[1]), float(sys.argv[2])), 0))
PY
sleep 2

echo "capturing (~50 s under load — not a hang)"
screencapture -x -o -t png -l "$WID" "$OUT/ag-rail-ink.png"
kill "$PID" 2>/dev/null || true

python3 - "$OUT/ag-rail-ink.png" "$WW" <<'PY'
import re, sys, pathlib
from collections import Counter
from PIL import Image

path, ww = sys.argv[1], int(sys.argv[2])
im = Image.open(path).convert("RGB")
# `screencapture -l <id>` captures the WINDOW, so the image origin IS the
# window origin and nothing on top of it can get into the frame. The
# earlier screen-wide version of this script measured a Terminal window and
# reported a 211/255 skew — occlusion, exactly the trap af-state-proof.sh
# warns about, caught here only because the number was absurd.
s = im.size[0] / float(ww)          # device pixels per point (2.0 on Retina)
W, H = im.size

src = pathlib.Path("crates/eave-app/src/theme.rs").read_text()
def token(name):
    """The red channel of a text token, read out of theme.rs itself so this
    script cannot drift away from the value it is checking."""
    m = re.search(r"pub fn %s\(\) -> Hsla \{\s*rgb\(0x([0-9a-f]{2})" % name, src)
    if not m:
        sys.exit("could not read %s out of theme.rs" % name)
    return int(m.group(1), 16)
def floor(name):
    m = re.search(r"pub const %s: f32 = ([0-9.]+)" % name, src)
    return float(m.group(1))

def px_at(x, y):
    return im.getpixel((min(int(x * s), W - 1), min(int(y * s), H - 1)))[0]
def row_px(y, x0, x1):
    return [px_at(x, y) for x in range(x0, x1)]

def lin(u):
    u = u / 255.0
    return u / 12.92 if u <= 0.04045 else ((u + 0.055) / 1.055) ** 2.4
def ratio(a, b):
    la, lb = lin(a), lin(b)
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)

# ------------------------------------------------------------------
# LOCATE, do not hardcode. The first version of this script pinned the
# three text bands to offsets measured off one capture, and a run where the
# rail was not where it had been reported a 211/255 skew rather than "I am
# not looking at the rail". Offsets are a silent instrument; a search that
# can fail out loud is not.
RAIL_X0, RAIL_X1 = 30, 220
ground = Counter(row_px(int(H / s) - 60, RAIL_X0, RAIL_X1)).most_common(1)[0][0]
rows = [(y, Counter(row_px(y, RAIL_X0, RAIL_X1)).most_common(1)[0][0])
        for y in range(60, int(H / s) - 60)]
card = [y for y, m in rows if m >= ground + 15]
if len(card) < 40:
    sys.exit("no selected rail row found (rail ground %d/255, %d lit scanlines) "
             "— the row was not selected, or the rail is collapsed" % (ground, len(card)))
top, bot = card[0], card[-1]
fill = Counter(m for y, m in rows if top <= y <= bot).most_common(1)[0][0]
print(f"  selected row spans y {top}..{bot} pt; fill {fill}/255, rail ground {ground}/255")

# Text lines inside the card: scanlines carrying a pixel well above the fill.
lit = [y for y in range(top, bot + 1) if max(row_px(y, 60, RAIL_X1)) > fill + 40]
runs, cur = [], [lit[0]] if lit else []
for y in lit[1:]:
    if y - cur[-1] <= 2:
        cur.append(y)
    else:
        runs.append(cur); cur = [y]
if cur:
    runs.append(cur)
runs = [r for r in runs if len(r) >= 4]
if len(runs) < 3:
    sys.exit("expected 3 text lines on the selected row (label, directory, "
             "affordances); found %d" % len(runs))

# The selected row is the one place all three rungs are painted at once:
# the primary label, the quiet rung demoted to `text_secondary` on the
# directory line, and the affordances promoted to primary because a hover
# on them reaches the third state rung.
BANDS = [
    ("row label",      "text",           runs[0]),
    ("directory line", "text_secondary", runs[1]),
    ("affordances",    "text",           runs[2]),
]

ok = True
measured = {}
for name, tok, run in BANDS:
    px = sorted(v for y in run for v in row_px(y, 60, RAIL_X1))
    ink = px[int(len(px) * 0.995)]
    want = token(tok)
    skew = ink - want
    measured[name] = ink
    print(f"  {name:16s} y {run[0]:3d}..{run[-1]:3d}  ink {ink:3d}/255  "
          f"{tok}() = {want}/255  (skew {skew:+d})")
    # 4/255, the same tolerance af-state-proof.sh uses for a surface token.
    if abs(skew) > 4:
        print(f"AG_INK_FIDELITY {name} skew={skew:+d}/255 tolerance=4 FAIL — the "
              f"text system is not putting {tok}() on the screen, and every "
              f"contrast floor in theme.rs is arithmetic rather than a claim "
              f"about a person's eye")
        ok = False
    else:
        print(f"AG_INK_FIDELITY {name} skew={skew:+d}/255 tolerance=4 PASS")

# And the floor itself, measured rather than computed: the quiet line on the
# selected row against the fill it is drawn on. This is the combination that
# read 2.09:1 in the arithmetic on `aac8cf4`.
r = ratio(measured["directory line"], fill)
f = floor("TEXT_QUIET_CONTRAST_FLOOR")
print(f"  quiet ink {measured['directory line']}/255 on the selection fill "
      f"{fill}/255 -> {r:.2f}:1, floor {f}")
print("  NOTE: `screencapture -l` captures the window LAYER, so the desktop "
      "behind the glass is not in it and the fill above is not the fill a "
      "person sees (a screen-wide shot of the same frame reads 45/255 on "
      "this dark desktop, against 72 here). It runs BRIGHTER, so the ratio "
      "is conservative — but do not compare it with af-state-proof.sh's "
      "numbers, which are screen-wide. The INK is unaffected: it is opaque, "
      "which is why this script measures ink and af-state-proof.sh measures "
      "surfaces.")
print("  (this desktop is dark. theme.rs sweeps 52 backdrops and the hard end "
      "is a WHITE desktop, where the same pair is 3.40:1 — this shot is the "
      "token-to-pixel bridge, not the worst case.)")
if r >= f:
    print(f"AG_QUIET_ON_SELECTION measured={r:.2f} floor={f} PASS")
else:
    print(f"AG_QUIET_ON_SELECTION measured={r:.2f} floor={f} FAIL")
    ok = False

if not ok:
    sys.exit(1)
PY
echo "shot: $OUT/ag-rail-ink.png"
