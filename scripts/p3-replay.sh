#!/usr/bin/env bash
# Slice P3 — the replay-assert sweep the directive asks for:
# 12/12 in EACH reduced-motion state, so 24 runs total.
#
# 12 = 6 runs x 2 fixtures. The reduced-motion axis is on top of that
# because this machine's OS preference is ON: a run that inherits it
# exercises the resting form of everything the motion slices built, and a
# run with EAVE_REDUCE_MOTION=0 exercises the moving form. Neither is the
# "real" one — both ship.
#
# THE RENDER-MODE AXIS IS GONE (Slice C3). It used to read 3 runs x 2
# fixtures x 2 render modes, where the modes were `glass` and `opaque` and
# the switch between them was `EAVE_VIBRANCY=0`. That variable stopped being
# read on 2026-08-19 — see the two notes in `crates/eave-app/src/theme.rs`,
# which say plainly that it "no longer exists and there is no fallback". So
# for three months the `opaque` arm exported an environment variable nothing
# read and then ran the `glass` configuration again: half of every sweep was
# a bit-for-bit duplicate of the other half, reported as coverage.
#
# The run COUNT is deliberately unchanged. The axis was worthless but the
# repetition was not — this assert catches load-dependent layout jumps, and
# those are found by running the same thing many times, not by running two
# things once. Dropping the axis and leaving RUNS at 3 would have quietly
# halved the sweep while every past report still said 12/12; raising the
# default to 6 keeps the real work identical and the headline number
# comparable to every report before this one.
#
# WHAT A FAILURE LOOKS LIKE NOW (Slice C4). The assert no longer has two
# outcomes. `PASS` means it anchored a real measured row AND compared it at
# least once. `FAIL … jump(s)` is the real regression. `FAIL never-anchored`
# and `FAIL anchored-but-never-observed` mean the run tested NOTHING — the
# app was too starved of frames to anchor, which is a fact about the
# machine, not about the code under test. Do not re-run those until green;
# read `checks=` and the `REPLAY_REFUSE` lines this script now prints.
#
# The assert being swept is the anchor invariant (jumps=0). Slice P3 adds
# no motion and no row heights, so a jump here would mean the glyph swap
# changed a row's measured height — which is exactly the regression an
# icon slice can cause and the reason this runs at all.
set -euo pipefail
cd "$(dirname "$0")/.."

RUNS="${1:-6}"
SPEED="${2:-30}"
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/p3-replay-log.md"

FIXTURES=(
  "fixtures/real-claude-2026-08-16.tap.jsonl"
  "fixtures/approvals-synthetic.tap.jsonl"
)

pass=0; fail=0; failed_lines=()
for reduced in inherit 0; do
  for fixture in "${FIXTURES[@]}"; do
    for i in $(seq 1 "$RUNS"); do
      if [ "$reduced" = 0 ]; then export EAVE_REDUCE_MOTION=0; else unset EAVE_REDUCE_MOTION; fi
      # KEEP THE WITNESS. This used to be `| grep REPLAY_ASSERT`, which
      # threw away the only lines that explain a failure: the assert has
      # printed `REPLAY_JUMP from=… to=…` since Stage F and nobody read one
      # for nine slices, because the sweep deleted it before anyone could.
      # Slice C4 diagnosed the approvals anchor jump in an afternoon once
      # the line was visible. Capture everything; summarise; and on
      # anything that is not a clean PASS, print the whole conversation.
      # EAVE_REPLAY_BIN (S1): run a binary OTHER than the shared
      # target/debug/eave. Three sessions share this checkout's target
      # dir, and any one of them running a `--features web` build swaps
      # a cef-linked loose binary under a sweep mid-run — which panics in
      # cef-rs library_loader.rs:20 and reads as "the app did not boot"
      # from run N onward. A sweep that must be trustworthy builds to its
      # own CARGO_TARGET_DIR and names the binary here.
      all="$(EAVE_REPLAY="$fixture" EAVE_REPLAY_SPEED="$SPEED" EAVE_REPLAY_ASSERT=1 \
             "${EAVE_REPLAY_BIN:-$PWD/target/debug/eave}" 2>/dev/null | grep -E '^REPLAY_' || true)"
      out="$(printf '%s\n' "$all" | grep REPLAY_ASSERT || true)"
      tag="rm=$reduced $(basename "$fixture" .tap.jsonl) #$i"
      # A refusal is not a failure by itself — the assert re-arms — but it
      # says the machine was too starved of frames to anchor on the first
      # try, which is the leading indicator for the failure below it.
      noise="$(printf '%s\n' "$all" | grep -E 'REPLAY_(REFUSE|JUMP|WATCH)' || true)"
      case "$out" in
        # `if`, NOT `[ -n "$noise" ] && printf …`. Under `set -e` an
        # `&&` chain whose test fails IS the statement's exit status, so
        # the quiet case — no refusals, which is the normal case — would
        # kill the sweep mid-run. Same footgun bit Slice C3 in
        # `boot-smoke.sh`.
        *PASS*) pass=$((pass+1)); echo "PASS  $tag  ${out}"
                if [ -n "$noise" ]; then
                  printf '%s\n' "$noise" | sed 's/^/        · /'
                fi ;;
        *)      fail=$((fail+1)); failed_lines+=("$tag :: ${out:-<no assert line>}")
                echo "FAIL  $tag  ${out:-<no assert line>}"
                printf '%s\n' "${all:-<no REPLAY_ lines at all — the app did not boot>}" \
                  | sed 's/^/        │ /' ;;
      esac
    done
  done
done

echo "───────────────────────────────────────────"
echo "REPLAY TOTAL: $pass passed, $fail failed"
for l in "${failed_lines[@]:-}"; do [ -n "$l" ] && echo "  failed: $l"; done
[ "$fail" -eq 0 ]
