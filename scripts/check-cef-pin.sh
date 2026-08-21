#!/usr/bin/env bash
# Assert that the CEF crate in the lockfile and the CEF framework on disk name
# the same build, and that both match dist/cef/pin.env.
#
#     scripts/check-cef-pin.sh            # lockfile always; framework if present
#     scripts/check-cef-pin.sh --strict   # a missing framework is a failure
#
# Split out of the bundler so CI can run the cheap half on any runner without
# the 343 MB download. The lockfile half is the one that actually drifted:
# cef-dll-sys's own check accepts any framework OLDER than the crate
# (download-cef-2.3.2 src/lib.rs:100 — `archive <= expected`), so the spike
# built 151.3.23 bindings against a 151.3.18 framework from start to finish
# and was never told.
#
# THERE ARE TWO LOCKFILES, and until Slice C3 this checked only one. C2 wrote
# it while `cef` lived solely in `spike/cef-osr`, so the spike's lockfile was
# the only place the crate appeared. C1 then linked CEF into eave-app itself
# — behind `--features web`, but into the ROOT lockfile — and inherited the
# exact 151.3.23-against-151.3.18 mismatch this script exists to catch, in
# the one lockfile it was not looking at. The check that only covers the
# probe and not the product is worse than none: it reports "consistent" over
# a shipping build that isn't. Both are checked now, and a lockfile with no
# cef in it says so and passes, so this stays correct if either side drops
# the dependency.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cef_load_pin

NEED_FW=0
[[ "${1:-}" == "--strict" ]] && NEED_FW=1

echo "cef pin: $CEF_CRATE+$CEF_VERSION (chromium $CEF_CHROMIUM, $CEF_DISTRIBUTION)"

# One framework, two lockfiles. `cef_check_pin` always reports on the
# framework it is given, so its framework lines appear once per lockfile —
# the same two lines twice. That is deliberate rather than tidied away: each
# block reads as a self-contained verdict on one lockfile, which is what you
# want when the failure is "the product is pinned wrong and the probe is
# fine". Only the first call is allowed to treat a MISSING framework as
# fatal, so `--strict` still fails exactly once instead of twice.
rc=0
need_fw="$NEED_FW"
CEF_DIR="$(cef_resolve_path)"
for entry in "Cargo.toml:$ROOT/Cargo.lock" "spike/cef-osr:$ROOT/spike/cef-osr/Cargo.lock"; do
  echo "── ${entry%%:*} ──"
  cef_check_pin "$CEF_DIR" "${entry#*:}" "$need_fw" || rc=1
  need_fw=0
done

[[ "$rc" == 0 ]] && echo "cef pin: consistent" || echo "cef pin: INCONSISTENT" >&2
exit "$rc"
