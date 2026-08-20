#!/usr/bin/env bash
# Slice Q2 — prove the launcher actually LAUNCHES.
#
# The one thing neither the suite nor a screenshot can answer. The harness
# replaces the connection loop with plumbing, so `Command::Launch` reaching
# a real `open_with_agent` is exercised by no test; a capture can only show
# the rest state, not a click producing a session.
#
# So: boot against a throwaway data dir (empty engine -> launcher), press
# start on the first frame via EAVE_LAUNCH_AT_BOOT, and then ask the DISK
# whether a session exists. No agent binary is involved — opening a session
# writes a journal, it does not spawn a CLI.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BIN="$ROOT/target/debug/eave"
cargo build --quiet
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

DATA="$(mktemp -d)/eave-launch-proof"
mkdir -p "$DATA"
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/q2-launch-proof-log.md"

run() {
  local label="$1" launch="$2" want="$3"
  local dir="$DATA/$label"
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
echo "q2-launch-proof: the launcher rests, and it launches"
