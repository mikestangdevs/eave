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
cargo build --release -p eave-app

rm -rf "$APP" "$DMG" "$APP_TARBALL"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/eave" "$APP/Contents/MacOS/eave"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Icon. Unlike comet's, this step is OPTIONAL: eave has no app-icon artwork
# yet (Slice Q1 drew the in-app glyph family, not a 1024 px app mark), and a
# missing icon must not be the thing that stops us shipping. Drop a pre-masked
# dist/macos/icon-1024.png in — squircle, margins and shadow baked, because
# sips cannot alpha-mask — and this starts producing a real icns with no
# other change. Until then the bundle carries the generic app icon and
# CFBundleIconFile simply names a file that isn't there, which is benign.
if [[ -f "$ROOT/dist/macos/icon-1024.png" ]]; then
  ICONSET="$OUT_DIR/eave.iconset"
  rm -rf "$ICONSET" && mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ROOT/dist/macos/icon-1024.png" \
      --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$ROOT/dist/macos/icon-1024.png" \
      --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/eave.icns"
  rm -rf "$ICONSET"
else
  echo "package-macos: no dist/macos/icon-1024.png — bundling without an icon"
fi

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  # Hardened runtime + secure timestamp are both notarization requirements.
  # No --deep: Apple deprecated it, and the bundle is a single Mach-O anyway.
  codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP"
else
  # Ad-hoc signature so the app launches at all on Apple silicon.
  codesign --force --sign - "$APP"
fi

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
