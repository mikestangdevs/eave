#!/usr/bin/env bash
# Slice P3 — the replay-assert sweep the directive asks for:
# 12/12 in EACH reduced-motion state, so 24 runs total.
#
# 12 = 3 runs x 2 fixtures x 2 render modes. The reduced-motion axis is on
# top of that because this machine's OS preference is ON: a run that
# inherits it exercises the resting form of everything the motion slices
# built, and a run with EAVE_REDUCE_MOTION=0 exercises the moving form.
# Neither is the "real" one — both ship.
#
# The assert being swept is the anchor invariant (jumps=0). Slice P3 adds
# no motion and no row heights, so a jump here would mean the glyph swap
# changed a row's measured height — which is exactly the regression an
# icon slice can cause and the reason this runs at all.
set -euo pipefail
cd "$(dirname "$0")/.."

RUNS="${1:-3}"
SPEED="${2:-30}"
export EAVE_DOGFOOD_LOG="${TMPDIR:-/tmp}/p3-replay-log.md"

FIXTURES=(
  "fixtures/real-claude-2026-08-16.tap.jsonl"
  "fixtures/approvals-synthetic.tap.jsonl"
)

pass=0; fail=0; failed_lines=()
for reduced in inherit 0; do
  for mode in glass opaque; do
    for fixture in "${FIXTURES[@]}"; do
      for i in $(seq 1 "$RUNS"); do
        if [ "$mode" = opaque ]; then export EAVE_VIBRANCY=0; else unset EAVE_VIBRANCY; fi
        if [ "$reduced" = 0 ]; then export EAVE_REDUCE_MOTION=0; else unset EAVE_REDUCE_MOTION; fi
        out="$(EAVE_REPLAY="$fixture" EAVE_REPLAY_SPEED="$SPEED" EAVE_REPLAY_ASSERT=1 \
               "$PWD/target/debug/eave" 2>/dev/null | grep REPLAY_ASSERT || true)"
        tag="rm=$reduced $mode $(basename "$fixture" .tap.jsonl) #$i"
        case "$out" in
          *PASS*) pass=$((pass+1)); echo "PASS  $tag  ${out}" ;;
          *)      fail=$((fail+1)); failed_lines+=("$tag :: ${out:-<no assert line>}")
                  echo "FAIL  $tag  ${out:-<no assert line>}" ;;
        esac
      done
    done
  done
done

echo "───────────────────────────────────────────"
echo "REPLAY TOTAL: $pass passed, $fail failed"
for l in "${failed_lines[@]:-}"; do [ -n "$l" ] && echo "  failed: $l"; done
[ "$fail" -eq 0 ]
