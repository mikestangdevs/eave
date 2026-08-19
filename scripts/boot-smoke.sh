#!/usr/bin/env bash
# One boot smoke per fixture (Stage J step 4; the render-mode loop went
# away with the opaque build on 2026-08-19).
#
# This is all that is left of the scripted-drive rig. Clicking is no
# longer scripted at all: every interaction the old rig drove through the
# window server — rail affordances, the two-step delete, selection,
# composer, badges, the diff pane — is asserted hermetically by the
# harness in `crates/eave-app/src/ui_harness.rs`, where a synthetic click
# lands on a measured hitbox and cannot miss.
#
# What is left needs a REAL compositor and therefore cannot move: that
# the binary boots, opens a window, renders real frames through Metal in
# both render modes, and holds its scroll anchor while doing it. The
# replay assert is the self-checking part; a human still has to look at
# the window for anything about how it LOOKS.
#
# Usage: scripts/boot-smoke.sh [speed]   (default speed 30 ≈ 16 s per mode)
set -euo pipefail
cd "$(dirname "$0")/.."

SPEED="${1:-30}"
# Two fixtures:
#   real-claude   — a genuine capture; the pacing and markdown the
#                   streaming/anchor work was tuned against.
#   approvals     — SYNTHESISED (Slice K), and labelled so. There is no
#                   real capture of an approval exchange through eave's
#                   own wire yet, and fabricating records into the real
#                   capture would destroy what makes it worth having.
#                   Its job is the one thing the hermetic harness cannot
#                   do: render the approval card through Metal, at real
#                   glyph metrics, and prove the Pending→settled height
#                   change never moves the scroll anchor.
FIXTURES=(
  "fixtures/real-claude-2026-08-16.tap.jsonl"
  "fixtures/approvals-synthetic.tap.jsonl"
)
for f in "${FIXTURES[@]}"; do
  [ -f "$f" ] || { echo "boot-smoke: missing $f" >&2; exit 1; }
done

# Never let a smoke run append to the real dogfood log.
export EAVE_DOGFOOD_LOG="${EAVE_DOGFOOD_LOG:-${TMPDIR:-/tmp}/eave-boot-smoke.md}"

cargo build --workspace

# One render mode. Until 2026-08-19 this looped glass and opaque, because
# eave shipped both; the opaque build was deleted by ruling ("i never want
# it") and with it the ×2 convention every report before N3 quotes. Glass is
# the only look, so a single pass per fixture IS full coverage now — if you
# find yourself re-adding a mode loop, the design changed.
fail=0
for fixture in "${FIXTURES[@]}"; do
  echo "── boot smoke: $(basename "$fixture") ──"
  out="$(EAVE_REPLAY="$fixture" EAVE_REPLAY_SPEED="$SPEED" EAVE_REPLAY_ASSERT=1 \
         ./target/debug/eave 2>/dev/null | grep REPLAY_ASSERT || true)"
  echo "${out:-<no assert line — the app did not boot>}"
  case "$out" in
    *PASS*) ;;
    *) fail=1 ;;
  esac
done

if [ "$fail" -ne 0 ]; then
  echo "boot-smoke: FAILED" >&2
  exit 1
fi
echo "boot-smoke: every fixture booted and held anchor"
