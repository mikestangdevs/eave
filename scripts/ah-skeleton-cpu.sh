#!/usr/bin/env bash
# Slice AH — measure what an ETERNAL SKELETON costs, in a real running app.
#
# The bug's whole signature is CPU. A files rail whose snapshot never
# arrives draws skeleton rows, which look like loading, which looks fine —
# nobody finds this by looking. So this script does not look. It boots the
# real binary against the real engine, puts the app into the state, and
# reads the process's own CPU time off the kernel.
#
# ---------------------------------------------------------------------
# HOW THE STATE IS REACHED, WITH NO FAULT INJECTION IN THE PRODUCT
#
# `SIGSTOP` on the engine, then click the files rail's "this turn"
# segment. That toggle is the ONE thing in the UI that clears the held
# snapshot locally (`Shell::set_files_scope` — the old list describes the
# other basis and may not stay on screen under the new label), and the
# feed loop answers it with `client.unsubscribe(files_id).await`, a unary
# request that waits for a reply. A stopped engine never sends one, so the
# session loop parks inside that await: nothing re-subscribes, nothing
# resets, and the rail waits for a frame that cannot come.
#
# Everything here is the shipping binary. No debug hook, no env var that
# suppresses an answer, no patched engine.
#
# ---------------------------------------------------------------------
# WHY CPU **TIME**, NOT PERCENT
#
# This machine runs at load 200-300 with other builds on it, so an
# instantaneous %CPU sample measures the machine (progress.md records the
# same instrument answering 11-15% and 0.00-0.53% for the same state).
# Cumulative CPU seconds charged to the pid over a fixed wall window is
# the same number under any load. `top -stats time` reports it to
# hundredths.
#
# Traps this obeys (docs/progress.md §Traps):
#   * launch by ABSOLUTE path or `pgrep -f` will not find it;
#   * `pgrep -fl fake-agent` must be 0 before any number is trusted;
#   * EAVE_REDUCE_MOTION=0 is REQUIRED — under the machine default
#     (reduceMotion=1) `loaders::pulse` returns None, no clock is ever
#     constructed, and the bug costs nothing at all. See the report.
#   * `screencapture` leaves the window non-frontmost: re-raise before a
#     click that follows a shot.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${OUT:-$ROOT/docs-shots}"
mkdir -p "$OUT"
BIN="${AH_BIN:-$ROOT/target/debug/eave}"
WINDOW_S="${AH_WINDOW_S:-60}"

leaked="$(pgrep -f fake-agent | wc -l | tr -d ' ')"
[ "$leaked" = "0" ] || { echo "REFUSING TO MEASURE: $leaked fake-agent processes are alive" >&2; exit 1; }
[ -x "$BIN" ] || { echo "no binary at $BIN (cargo build first)" >&2; exit 1; }
echo "binary: $BIN"
echo "load: $(uptime | sed 's/.*load averages*: //')"

DATA="${AH_DATA:-$(mktemp -d)/eave-ah}"; mkdir -p "$DATA"
export EAVE_REDUCE_MOTION=0
export EAVE_DATA_DIR="$DATA"

cleanup() {
  [ -n "${ENGINE_PID:-}" ] && kill -CONT "$ENGINE_PID" 2>/dev/null || true
  [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "${ENGINE_PID:-}" ] && kill "$ENGINE_PID" 2>/dev/null || true
}
trap cleanup EXIT

# 1. Seed one session, then drop the engine that made it. `EAVE_LAUNCH_AT_BOOT`
#    only fires against an EMPTY engine (R1's finding, still live), so the
#    seeding run is its own process.
EAVE_LAUNCH_AT_BOOT=1 "$BIN" >/dev/null 2>&1 &
SEED_PID=$!
sleep 18
kill "$SEED_PID" 2>/dev/null || true
sleep 3
pkill -f "eave --engine --data-dir $DATA" 2>/dev/null || true
sleep 2
[ "$(find "$DATA" -name journal.jsonl | wc -l | tr -d ' ')" -ge 1 ] \
  || { echo "seeding produced no session" >&2; exit 1; }

# 2. A hand-run engine, so the app's respawn cannot heal the SIGSTOP.
"$BIN" --engine --data-dir "$DATA" >/dev/null 2>&1 &
ENGINE_PID=$!
sleep 4
kill -0 "$ENGINE_PID" || { echo "engine did not start" >&2; exit 1; }
echo "engine pid $ENGINE_PID"

EAVE_NO_SPAWN=1 "$BIN" >/dev/null 2>&1 &
APP_PID=$!
sleep 18
kill -0 "$APP_PID" || { echo "app did not start" >&2; exit 1; }
echo "app pid $APP_PID"

osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true
sleep 2

read -r WX WY WW WH < <(python3 - "$APP_PID" <<'PY'
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
    sys.exit("no window for pid %d" % pid)
b = best[1]
print(int(b['X']), int(b['Y']), int(b['Width']), int(b['Height']))
PY
)
echo "window ${WW}x${WH} at ${WX},${WY}"

# CPU seconds charged to a pid, to hundredths.
cpu_secs() {
  local t
  t="$(top -l 1 -pid "$1" -stats time 2>/dev/null | tail -1 | tr -d ' ')"
  python3 -c "
import sys
t=sys.argv[1]
if ':' not in t: print(0.0); raise SystemExit
m,s=t.split(':'); print(float(m)*60+float(s))" "$t"
}

sample() {
  local label="$1" pid="$2" a b
  a="$(cpu_secs "$pid")"
  sleep "$WINDOW_S"
  b="$(cpu_secs "$pid")"
  python3 -c "
import sys
a,b,w=float(sys.argv[1]),float(sys.argv[2]),float(sys.argv[3])
d=b-a
print('AH_CPU %s cpu_seconds=%.2f over %.0fs = %.1f%% of a core' % (sys.argv[4], d, w, 100*d/w))" \
    "$a" "$b" "$WINDOW_S" "$label"
}

screencapture -x -o -t png "$OUT/ah-before-answered.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true
sleep 2

echo "--- baseline: a settled window whose files rail HAS its answer"
sample settled "$APP_PID"

# 3. Wedge the engine, then ask the rail a question it can no longer answer.
kill -STOP "$ENGINE_PID"
sleep 1
# The "this turn" segment. Offsets from the window's TOP-RIGHT, because
# the files rail is right-anchored; overridable so a layout change is a
# flag rather than an edit here.
TOG_X=$((WX + WW - ${AH_TOG_DX:-76})); TOG_Y=$((WY + ${AH_TOG_DY:-166}))
echo "clicking files-scope-latest-turn at $TOG_X,$TOG_Y"
osascript -e "tell application \"System Events\" to click at {$TOG_X, $TOG_Y}" >/dev/null 2>&1 || \
  echo "note: could not click; grant accessibility to the terminal"
sleep 3
screencapture -x -o -t png "$OUT/ah-skeleton-armed.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true

# Past FILES_PATIENCE on a fixed build; irrelevant on an unfixed one.
sleep 25
screencapture -x -o -t png "$OUT/ah-after-patience.png"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $APP_PID) to true" >/dev/null 2>&1 || true
sleep 2

echo "--- the state: a selected session whose files snapshot cannot arrive"
sample unanswered "$APP_PID"

kill -CONT "$ENGINE_PID"
echo "shots: $OUT/ah-before-answered.png $OUT/ah-skeleton-armed.png $OUT/ah-after-patience.png"
