#!/usr/bin/env bash
# Live loop for the current product: CEF bundle, relaunched on save.
#
# Usage: scripts/dev.sh
#
# Why this exists: `cargo run -p eave-app` and `target/debug/eave` are the
# wrong artefact. With `--features web` the binary only runs inside
# `target/bundle/eave.app`. Opening `/Applications/eave.app` or the
# `eave/` worktree's debug binary is how you get yesterday's layout.
#
# First launch uses the bundle on disk if Chromium is already inside it,
# so the window appears before the first rebuild. A save under crates/
# rebuilds `--features web`, restamps the bundle binaries (not the 317 MB
# framework, unless it is missing), and relaunches.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
APP="$ROOT/target/bundle/eave.app"
BIN="$APP/Contents/MacOS/eave"
CEF="$APP/Contents/Frameworks/Chromium Embedded Framework.framework"
LOG="${TMPDIR:-/tmp}/eave-dev-build.log"
DEBOUNCE_SEC=1

log() { printf '[eave-dev] %s\n' "$*"; }

bundle_complete() {
  [[ -x "$BIN" && -d "$CEF" ]]
}

stamp() {
  # One number: newest mtime of the sources the bundle is built from.
  python3 - "$ROOT" <<'PY'
import os, sys
root = sys.argv[1]
newest = 0.0
watch = [
    os.path.join(root, "Cargo.toml"),
    os.path.join(root, "Cargo.lock"),
    os.path.join(root, "dist"),
    os.path.join(root, "crates"),
]
for path in watch:
    if os.path.isfile(path):
        newest = max(newest, os.path.getmtime(path))
        continue
    for dirpath, dirnames, filenames in os.walk(path):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for name in filenames:
            if name.endswith((".rs", ".toml", ".svg", ".plist", ".lock", ".env", ".strings")):
                try:
                    newest = max(newest, os.path.getmtime(os.path.join(dirpath, name)))
                except OSError:
                    pass
print(f"{newest:.3f}")
PY
}

restamp() {
  # Incremental path: compile, then overwrite the Mach-Os inside the
  # existing bundle. `bundle-app.sh` re-dittos 317 MB of Chromium every
  # time and deletes the .app first, which is right for a cold assemble
  # and wrong for a save-to-relaunch loop.
  log "cargo build --features web (log: $LOG)"
  if ! cargo build --offline --features web --bin eave --bin eave-web-helper \
        >"$LOG" 2>&1; then
    log "build failed; keeping the current window. last 40 lines:"
    tail -40 "$LOG" >&2
    return 1
  fi
  python3 "$ROOT/scripts/stop-eave.py"
  install -m 755 "$ROOT/target/debug/eave" "$BIN"
  local helper_src="$ROOT/target/debug/eave-web-helper"
  local entry name helper
  for entry in "Helper|helper" "Helper (GPU)|helper.gpu" \
               "Helper (Renderer)|helper.renderer" "Helper (Plugin)|helper.plugin" \
               "Helper (Alerts)|helper.alerts"; do
    name="${entry%%|*}"
    helper="$APP/Contents/Frameworks/eave ${name}.app/Contents/MacOS/eave ${name}"
    install -m 755 "$helper_src" "$helper"
  done
  return 0
}

rebuild() {
  if bundle_complete; then
    restamp
    return $?
  fi
  log "bundling --features web (log: $LOG)"
  if ! "$ROOT/scripts/bundle-app.sh" --features web >"$LOG" 2>&1; then
    log "build failed; keeping the current window. last 40 lines:"
    tail -40 "$LOG" >&2
    return 1
  fi
  return 0
}

launch() {
  python3 "$ROOT/scripts/stop-eave.py"
  log "opening $APP"
  open -n "$APP"
}

if ! bundle_complete; then
  log "no complete CEF bundle yet — first build"
  rebuild || exit 1
fi
launch
log "watching crates/ — save a .rs file to rebuild and relaunch"
log "engine is killed on relaunch so you cannot attach to an old product"

last="$(stamp)"
while true; do
  sleep "$DEBOUNCE_SEC"
  now="$(stamp)"
  if [[ "$now" != "$last" ]]; then
    last="$now"
    # Coalesce a burst of saves (fmt + rust-analyzer + the editor).
    sleep 0.6
    last="$(stamp)"
    if rebuild; then
      # restamp already stopped eave so the new Mach-O is not busy.
      log "opening $APP"
      open -n "$APP"
    fi
  fi
done
