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
      out="$(EAVE_REPLAY="$fixture" EAVE_REPLAY_SPEED="$SPEED" EAVE_REPLAY_ASSERT=1 \
             "$PWD/target/debug/eave" 2>/dev/null | grep REPLAY_ASSERT || true)"
      tag="rm=$reduced $(basename "$fixture" .tap.jsonl) #$i"
      case "$out" in
        *PASS*) pass=$((pass+1)); echo "PASS  $tag  ${out}" ;;
        *)      fail=$((fail+1)); failed_lines+=("$tag :: ${out:-<no assert line>}")
                echo "FAIL  $tag  ${out:-<no assert line>}" ;;
      esac
    done
  done
done

echo "───────────────────────────────────────────"
echo "REPLAY TOTAL: $pass passed, $fail failed"
for l in "${failed_lines[@]:-}"; do [ -n "$l" ] && echo "  failed: $l"; done
[ "$fail" -eq 0 ]
