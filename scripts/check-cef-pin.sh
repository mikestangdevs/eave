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
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cef_load_pin

NEED_FW=0
[[ "${1:-}" == "--strict" ]] && NEED_FW=1

echo "cef pin: $CEF_CRATE+$CEF_VERSION (chromium $CEF_CHROMIUM, $CEF_DISTRIBUTION)"
cef_check_pin "$(cef_resolve_path)" "$ROOT/spike/cef-osr/Cargo.lock" "$NEED_FW"
rc=$?
[[ "$rc" == 0 ]] && echo "cef pin: consistent" || echo "cef pin: INCONSISTENT" >&2
exit "$rc"
