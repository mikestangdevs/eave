#!/usr/bin/env bash
# Open THIS checkout's CEF bundle, and only this checkout's.
#
# A loose `target/debug/eave` is not the product once `--features web` is
# on: CEF resolves its framework and five helper .apps relative to the
# bundle path, and a loose web binary panics in cef-rs's library_loader
# instead of degrading. `/Applications/eave.app` and `eave/target/debug/eave`
# are other checkouts (often a day behind, often without Chromium). This
# script stops those, then opens `target/bundle/eave.app`.
#
# If the bundle has no Chromium in it yet, it refuses rather than opening
# a 32 MB shell that looks like eave and cannot draw a page. Run
# `scripts/dev.sh` (or `scripts/bundle-app.sh --features web`) first.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/target/bundle/eave.app"
CEF="$APP/Contents/Frameworks/Chromium Embedded Framework.framework"

python3 "$ROOT/scripts/stop-eave.py"

if [[ ! -x "$APP/Contents/MacOS/eave" ]]; then
  echo "run-app: no binary at $APP — run scripts/dev.sh" >&2
  exit 1
fi
if [[ ! -d "$CEF" ]]; then
  echo "run-app: $APP has no Chromium (bundle is incomplete)." >&2
  echo "         scripts/bundle-app.sh --features web, then this again." >&2
  exit 1
fi

echo "run-app: opening $APP"
open -n "$APP"
