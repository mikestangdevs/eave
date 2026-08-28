#!/usr/bin/env bash
# Slice AL — one window shot of the RAIL with a row selected, so somebody
# can look at the type scale and the selection bar instead of reading
# arithmetic about them.
#
# Mechanics are lifted wholesale from `ag-ink-proof.sh`, and each one cost a
# previous slice a wrong conclusion (docs/progress.md §Traps):
#   * launch by ABSOLUTE path or `pgrep -f` will not find the process — and
#     on this machine there are usually two other worktrees' `eave`
#     binaries alive, so the path is what disambiguates them;
#   * `EAVE_LAUNCH_AT_BOOT=1` once to seed a session, then a second boot
#     with it off, because the launcher hides the rail's content otherwise;
#   * `screencapture -l <window-id>`, never a screen-wide shot: AG's first
#     ink capture photographed a Terminal window and confidently reported a
#     211/255 skew;
#   * budget ~50 s for the shutter under load — a slow shutter is not a
#     permission wall;
#   * NO hover is photographed. A synthesized `CGEventMouseMoved` produces
#     no hover fade in this app (measured, af-state-proof.sh), so every
#     hovered combination in this slice is asserted by `theme.rs` and by
#     nothing else.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${1:-$ROOT/docs-shots}"
mkdir -p "$OUT"

BIN="$ROOT/target/debug/eave"
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-al"; mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/al-shot-log.md"

# The APP, not the engine: `$BIN --engine --data-dir …` is a child of the
# same binary, so `pgrep -n -f "$BIN"` returns whichever raced last. That is
# the bug that made the first run of this script photograph nothing —
# Quartz had no window for the pid it was handed.
app_pid() {
  pgrep -f "$BIN" | while read -r p; do
    case "$(ps -o command= -p "$p" 2>/dev/null)" in
      *--engine*) ;;
      *) echo "$p" ;;
    esac
  done | tail -1
}
reap() { pkill -f "$BIN" 2>/dev/null || true; sleep 2; }

boot() {
  EAVE_DATA_DIR="$DATA" EAVE_LAUNCH_AT_BOOT="$1" "$BIN" >/dev/null 2>&1 &
  sleep 16
  PID="$(app_pid)"
  [ -n "$PID" ] || { echo "app did not start" >&2; exit 1; }
}

trap reap EXIT

boot 1
# Reap BOTH the app and its engine child. A second instance against a data
# dir the first still holds gets "engine busy" and never opens a window.
reap
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

# Select the first rail row, so the selection bar has something to mark.
ROW_X=$((WX + ${ROW_DX:-124})); ROW_Y=$((WY + ${ROW_DY:-120}))
osascript -e "tell application \"System Events\" to click at {$ROW_X, $ROW_Y}" >/dev/null 2>&1 || \
  echo "note: could not click; grant accessibility to the terminal"
sleep 1
# Park the pointer off the window entirely — see the hover trap above.
python3 - "$((WX + WW - 40))" "$((WY + WH - 40))" <<'PY'
import sys, Quartz
Quartz.CGEventPost(Quartz.kCGHIDEventTap, Quartz.CGEventCreateMouseEvent(
    None, Quartz.kCGEventMouseMoved, (float(sys.argv[1]), float(sys.argv[2])), 0))
PY
sleep 2

echo "capturing (~50 s under load — not a hang)"
screencapture -x -o -t png -l "$WID" "$OUT/al-rail.png"
reap
echo "captured $(ls -la "$OUT/al-rail.png" | awk '{print $5}') bytes -> $OUT/al-rail.png"
