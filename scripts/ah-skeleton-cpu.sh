#!/usr/bin/env bash
# Slice AH — measure what an ETERNAL SKELETON costs, in a real running app.
#
# The bug's whole signature is CPU. A files rail whose snapshot never
# arrives draws skeleton rows, which look like loading, which looks fine —
# nobody finds this by looking. So this script does not look. It boots the
# real binary, puts it into the state, and reads the process's own CPU
# time off the kernel.
#
# ---------------------------------------------------------------------
# THE INSTRUMENT, AND WHY IT IS A STUB RATHER THAN THE ENGINE
#
# `ah-mute-engine.py` speaks spec 05's wire on the data dir's socket and
# answers everything EXCEPT `files` — accepted, never answered. That is
# not a way of misusing the engine: the engine answers a `files`
# subscription in every failure mode I could construct
# (`every_files_subscription_answers_at_both_scopes_and_every_cwd`,
# crates/eave-engine/tests/files.rs). The state under measurement is a
# TRANSPORT state — an engine wedged, stopped, or mid-reconnect — and the
# stub is the smallest honest way to hold one still.
#
# The APP is the shipping binary, unmodified, with no debug hook and no
# env var that suppresses an answer. Only its counterparty is synthetic.
#
# The same state is reachable with the real engine and no stub at all:
# `kill -STOP` the engine, then click the files rail's "this turn"
# segment. That toggle is the one thing in the UI that clears the held
# snapshot locally (`Shell::set_files_scope`), and the feed loop answers
# it with `client.unsubscribe(files_id).await` — a unary request that
# waits for a reply a stopped engine never sends, so the session loop
# parks inside that await and nothing re-subscribes. It is not what this
# script drives, because on this machine a System Events click could not
# be aimed at eave's window while a second eave was on screen (see the
# report's caveats).
#
# ---------------------------------------------------------------------
# WHY CPU **TIME**, NOT PERCENT
#
# This machine runs at load 170-300 with other agents' builds on it, so an
# instantaneous %CPU sample measures the machine — progress.md records
# that same instrument answering 11-15% and 0.00-0.53% for the SAME state.
# Cumulative CPU seconds charged to the pid over a fixed wall window is
# the same number under any load. `top -stats time` reports it to
# hundredths.
#
# Traps this obeys (docs/progress.md §Traps):
#   * a data dir under /tmp, never a long path — the engine socket hits
#     SUN_LEN (~104 bytes) and dies with `path must be shorter than
#     SUN_LEN`, which reads as "the app did not start";
#   * launch by ABSOLUTE path or `pgrep -f` will not find it;
#   * `pgrep -fl fake-agent` before trusting any number;
#   * EAVE_REDUCE_MOTION=0 is REQUIRED. Under the machine default
#     (reduceMotion=1) `loaders::pulse` returns None, no clock is ever
#     constructed, and the bug costs nothing at all. See the report.
#   * `screencapture -l <window-id>`, never `-R <rect>`: a second eave on
#     screen at the same rect photographs itself instead.
#
# Usage: scripts/ah-skeleton-cpu.sh [/path/to/eave]   (default: the
# workspace debug build). Run it once per binary — the point is the pair.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="${1:-$ROOT/target/debug/eave}"
WINDOW_S="${AH_WINDOW_S:-60}"
OUT="${OUT:-${TMPDIR:-/tmp}}"

leaked="$(pgrep -f fake-agent | wc -l | tr -d ' ')"
[ "$leaked" = "0" ] || echo "WARNING: $leaked fake-agent processes alive — another suite is running, load is not yours"
[ -x "$BIN" ] || { echo "no binary at $BIN (cargo build first)" >&2; exit 1; }
echo "binary: $BIN"
echo "load:   $(uptime | sed 's/.*load averages*: //')"

DATA="$(mktemp -d /tmp/ah-XXXX)"
export EAVE_REDUCE_MOTION=0
export EAVE_DATA_DIR="$DATA"

cleanup() {
  [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "${MUTE_PID:-}" ] && kill "$MUTE_PID" 2>/dev/null || true
}
trap cleanup EXIT

python3 "$ROOT/scripts/ah-mute-engine.py" "$DATA" > "$DATA/mute.log" 2>&1 &
MUTE_PID=$!
sleep 2
grep -q "mute engine on" "$DATA/mute.log" || { echo "mute engine did not start" >&2; exit 1; }

EAVE_NO_SPAWN=1 "$BIN" >/dev/null 2>&1 &
APP_PID=$!
# 18 s, not 9: under a loaded machine a short wait finds no window and the
# geometry probe crashes on a None. A short wait reads as a crash.
sleep 18
kill -0 "$APP_PID" || { echo "app did not start" >&2; exit 1; }
grep -q "swallowed files" "$DATA/mute.log" \
  || { echo "the app never asked for a file list — nothing is being measured" >&2; exit 1; }
echo "app pid $APP_PID — files subscription accepted and never answered"

cpu_secs() {
  python3 -c "
import sys
t = sys.argv[1]
print(0.0 if ':' not in t else float(t.split(':')[0])*60 + float(t.split(':')[1]))" \
    "$(top -l 1 -pid "$1" -stats time 2>/dev/null | tail -1 | tr -d ' ')"
}

sample() {
  local label="$1" a b
  a="$(cpu_secs "$APP_PID")"
  sleep "$WINDOW_S"
  b="$(cpu_secs "$APP_PID")"
  python3 -c "
import sys
a, b, w = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
print('AH_CPU %-12s %.2f cpu-seconds over %.0fs = %.1f%% of a core'
      % (sys.argv[4], b - a, w, 100 * (b - a) / w))" "$a" "$b" "$WINDOW_S" "$label"
}

# The first window straddles the wait; the second is entirely past it, and
# on a fixed binary the two must differ. On an unfixed one they do not,
# which is the whole finding.
sample straddling
sample settled

WID="$(python3 - "$APP_PID" <<'PY'
import sys, Quartz
pid = int(sys.argv[1])
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    b = w['kCGWindowBounds']
    if w.get('kCGWindowOwnerPID') == pid and b['Width'] > 300:
        print(w['kCGWindowNumber'])
        break
PY
)"
if [ -n "$WID" ]; then
  screencapture -x -o -l "$WID" "$OUT/ah-files-rail.png"
  echo "shot: $OUT/ah-files-rail.png (the rail should read a stated fact, not skeleton rows)"
else
  echo "note: no window found for $APP_PID — the CPU numbers still stand"
fi
