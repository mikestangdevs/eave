#!/usr/bin/env bash
# macOS packaging: build the release binary for the host arch and produce
#   target/package/eave-<version>-macos-<arch>.dmg        (user download)
#   target/package/eave-<version>-macos-<arch>-app.tar.gz (bundle, for an
#                                                          updater that swaps
#                                                          the .app in place)
# containing eave.app.
#
# Usage: scripts/package-macos.sh
# Env:   CODESIGN_IDENTITY="Developer ID Application: …"  sign the bundle.
#          Signing happens inside scripts/bundle-app.sh, inside-out, once.
#        NOTARY_KEY_PATH + NOTARY_KEY_ID + NOTARY_ISSUER_ID — App Store
#          Connect API key (.p8) for notarization. All three set → notarize
#          and staple both the app and the dmg, which removes the Gatekeeper
#          warning entirely. Absent → ad-hoc signature, and first launch
#          needs right-click → Open.
#
# Adapted from comet's scripts/package-macos.sh (MIT) — see
# THIRD_PARTY_NOTICES.md. Divergences from theirs are deliberate and marked.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

# The workspace version is the single source of truth; the tag that triggers
# a release must match it (see .github/workflows/release.yml).
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$ROOT/Cargo.toml" |
  sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || {
  echo "package-macos: no version in [workspace.package] of Cargo.toml" >&2
  exit 1
}

ARCH="$(uname -m)"
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/eave.app"
DMG="$OUT_DIR/eave-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/eave-$VERSION-macos-$ARCH-app.tar.gz"

cd "$ROOT"

# ONE ASSEMBLER. This used to build, lay out the bundle, write the plist,
# make the icns and sign — all inline, which meant the artefact a release
# shipped was assembled by different code from the one boot-smoke ran and the
# one a developer launched. scripts/bundle-app.sh is now the only thing that
# builds a .app in this repo; it runs cargo itself, so this is still one
# command from a clean tree.
#
# It also signs, and it is the LAST thing to touch the bundle. Nothing below
# this line may write inside "$APP" — see mac_sign_bundle for what a stale
# seal costs. That is why the icon moved into the bundler rather than staying
# here: it was a write into an already-signed bundle waiting to happen.
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}" \
  "$ROOT/scripts/bundle-app.sh" --release --out "$OUT_DIR" eave

rm -f "$DMG" "$APP_TARBALL"

# notarize <path>: submit to Apple and wait for the verdict. A rejection can
# still exit 0 depending on the notarytool version — the `stapler staple`
# after each call then has no ticket to attach and fails the build for us.
notarize() {
  xcrun notarytool submit "$1" \
    --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" \
    --issuer "$NOTARY_ISSUER_ID" --wait
}
NOTARIZE=false
[[ -n "${NOTARY_KEY_PATH:-}" && -n "${NOTARY_KEY_ID:-}" && -n "${NOTARY_ISSUER_ID:-}" ]] &&
  NOTARIZE=true

if $NOTARIZE; then
  # Staple the bundle BEFORE tarring it: an updater swaps the .app with no
  # dmg involved, so the tarball's copy must carry its own ticket to pass
  # Gatekeeper offline.
  ZIP="$OUT_DIR/eave-notarize.zip"
  ditto -c -k --keepParent "$APP" "$ZIP"
  notarize "$ZIP"
  rm -f "$ZIP"
  xcrun stapler staple "$APP"
fi

tar -czf "$APP_TARBALL" -C "$OUT_DIR" eave.app
echo "packaged: $APP_TARBALL"

# The dmg is the drag-into-Applications layout. dmgbuild writes the .DS_Store
# (icon view, positions) directly rather than scripting Finder, so it works on
# a headless CI runner. No background artwork yet — that pairs with the icon.
python3 -c 'import dmgbuild' 2>/dev/null ||
  python3 -m pip install --quiet --user dmgbuild 2>/dev/null ||
  python3 -m pip install --quiet --user --break-system-packages dmgbuild

APP="$APP" DMG="$DMG" python3 - <<'PY'
import os
import dmgbuild

app = os.environ["APP"]
icns = os.path.join(app, "Contents/Resources/eave.icns")
settings = {
    "format": "UDZO",
    "files": [app],
    "symlinks": {"Applications": "/Applications"},
    "show_status_bar": False,
    "show_tab_view": False,
    "show_toolbar": False,
    "show_pathbar": False,
    "show_sidebar": False,
    "default_view": "icon-view",
    "window_rect": ((200, 120), (660, 400)),
    "icon_size": 104,
    "text_size": 12,
    "icon_locations": {"eave.app": (165, 195), "Applications": (495, 195)},
}
if os.path.exists(icns):
    settings["icon"] = icns
dmgbuild.build_dmg(filename=os.environ["DMG"], volume_name="eave", settings=settings)
PY

if $NOTARIZE; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
fi
echo "packaged: $DMG"
