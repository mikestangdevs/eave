#!/usr/bin/env bash
# Slice Q2 — prove the launcher actually LAUNCHES.
# Slice AI — ...and that it is REACHABLE once the engine is not empty.
#
# The one thing neither the suite nor a screenshot can answer. The harness
# replaces the connection loop with plumbing, so `Command::Launch` reaching
# a real `open_with_agent` is exercised by no test; a capture can only show
# the rest state, not a click producing a session.
#
# So: boot against a throwaway data dir, press start on the first frame via
# EAVE_LAUNCH_AT_BOOT, and then ask the DISK whether a session exists. No
# agent binary is involved — opening a session writes a journal, it does not
# spawn a CLI.
#
# ---------------------------------------------------------------------------
# WHY THERE IS A THIRD CASE, AND WHY THE FIRST TWO COULD NOT CATCH THE BUG
#
# Q2 shipped `resting` and `launched`, and each ran against its OWN fresh
# data dir. Both therefore started from an empty engine — the one state in
# which the connection loop is already sitting in its rest branch, which is
# the only branch that services `Command::Launch`. Against a data dir with
# history the loop boot-selects a session instead, and the session branch
# drops `Launch` on the floor. The hook did nothing at all, for a month,
# and the suite and this script were both green throughout.
#
# `again` is the missing case: the SAME data dir a second time. It is the
# reproduction reported twice by hand — "two consecutive boots against one
# data dir, both with EAVE_LAUNCH_AT_BOOT=1, produced ONE session, not two."
# Two boots that each start a session are two sessions; that is arithmetic
# about the runs, not a number read back out of the thing being tested.
# ---------------------------------------------------------------------------
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="$ROOT/target/debug/eave"
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-launch-proof"
mkdir -p "$DATA"
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/q2-launch-proof-log.md"

# run <label> <launch> <want> [data-dir-name]
#
# The 4th argument is what makes `again` possible: by default a case gets a
# data dir named after itself, i.e. an empty engine. Passing an earlier
# case's name reuses that engine WITH its history.
run() {
  local label="$1" launch="$2" want="$3" dirname="${4:-$1}"
  local dir="$DATA/$dirname"
  mkdir -p "$dir"
  EAVE_DATA_DIR="$dir" EAVE_LAUNCH_AT_BOOT="$launch" "$BIN" >/dev/null 2>&1 &
  local pid=$!
  sleep 6
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  # A session is a directory holding a journal. Count them.
  local got
  got="$(find "$dir" -name journal.jsonl 2>/dev/null | wc -l | tr -d ' ')"
  if [ "$got" = "$want" ]; then
    echo "LAUNCH_PROOF $label sessions=$got PASS"
  else
    echo "LAUNCH_PROOF $label sessions=$got want=$want FAIL"
    return 1
  fi
}

# Resting must create NOTHING. This is the half of Q2 that removed
# behaviour, and the half most likely to regress silently.
run resting 0 0
# Pressing start must create exactly one.
run launched 1 1
# ...and pressing it again, against the engine that first press left behind,
# must create a SECOND one. This is the case that was missing (Slice AI):
# the launcher has to be reachable past a selected session, which means the
# hook has to press `+ new` before it presses start, exactly as a person
# does. `want=2` is the two boots counted, not a constant read from the app.
run again 1 2 launched
echo "q2-launch-proof: the launcher rests, it launches, and it launches again"
