#!/usr/bin/env bash
# Build and assemble a runnable macOS .app. ONE command: it runs cargo
# itself, so there is no build step to forget and no order to get wrong.
#
#     scripts/bundle-app.sh                     # debug eave.app
#     scripts/bundle-app.sh --release
#     scripts/bundle-app.sh cef-osr-spike       # the CEF spike's app
#     scripts/bundle-app.sh --out /tmp/x eave
#
# WHY A BUNDLE STEP EXISTS NOW. Until CEF, `cargo build` produced something
# you could run — target/debug/eave opened a window. With CEF in the binary it
# does not, and it does not fail in a way that says so: CEF resolves its
# framework and its five helper .apps relative to its own bundle path, so a
# loose executable dies inside CefInitialize. The bundle is not packaging any
# more, it is the build output. So this script runs cargo, and boot-smoke runs
# this script, and there is no path through the dev loop that produces a
# binary nobody can launch.
#
# It also replaces cef-rs's `bundle-cef-app`, which gives all six bundles the
# same CFBundleIdentifier and cannot be told otherwise — see the note at the
# top of scripts/mac-bundle.sh for why that is a shipping blocker and why
# post-fixing its output was rejected.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cd "$ROOT"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

PROFILE=debug
OUT=""
TARGET=eave
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) PROFILE=release; shift ;;
    --debug)   PROFILE=debug; shift ;;
    --out)     OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    -*)        echo "bundle-app: unknown flag $1" >&2; exit 2 ;;
    *)         TARGET="$1"; shift ;;
  esac
done

# The target table.
#
# NEEDS_CEF is the whole difference. eave is 0 today and becomes 1 the moment
# eave-app links cef — at which point it gets the framework and the five
# helpers with no other change here, because the helper identifiers are
# derived from whatever CFBundleIdentifier the app's own plist declares.
case "$TARGET" in
  eave)
    MANIFEST="Cargo.toml"
    CARGO_ARGS=(-p eave-app)
    BIN="eave"
    HELPER_BIN=""
    # eave's plist is hand-written and carries its own reasoning (the
    # identifier note in particular). It is a template, not a generated file,
    # and this script substitutes into it rather than replacing it.
    PLIST_TEMPLATE="dist/macos/Info.plist"
    RESOURCES=""
    ICON_PNG="dist/macos/icon-1024.png"
    ICNS_NAME="eave.icns"
    NEEDS_CEF=0
    ;;
  cef-osr-spike)
    MANIFEST="spike/cef-osr/Cargo.toml"
    CARGO_ARGS=()
    BIN="cef-osr-spike"
    HELPER_BIN="cef-osr-spike-helper"
    PLIST_TEMPLATE=""
    IDENTIFIER="com.eave.cefosr"
    RESOURCES="spike/cef-osr/resources/mac"
    ICON_PNG=""
    ICNS_NAME=""
    NEEDS_CEF=1
    ;;
  *)
    echo "bundle-app: no such target '$TARGET' (known: eave, cef-osr-spike)" >&2
    exit 2
    ;;
esac

# The version. eave's lives in [workspace.package]; a standalone probe's lives
# in [package]. Same single-source-of-truth rule either way.
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$MANIFEST" |
  sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || VERSION="$(sed -n '/^\[package\]/,/^\[/p' "$MANIFEST" |
  sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "bundle-app: no version in $MANIFEST" >&2; exit 1; }

# ------------------------------------------------------------- the pin ------

if [[ "$NEEDS_CEF" == 1 ]]; then
  cef_load_pin
  export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"
  echo "── pin: lockfile ──"
  # The lockfile half runs BEFORE the build, because it is instant and it is
  # the half that catches drift. The framework half runs AFTER, because on a
  # clean machine there is no framework yet — the first build downloads ~343 MB
  # through cef-dll-sys's build script — and refusing to build for want of a
  # thing the build produces is a bootstrap nobody can complete.
  cef_check_pin "$(cef_resolve_path)" "$(dirname "$MANIFEST")/Cargo.lock" 0
fi

# ------------------------------------------------------------- the build ----

echo "── build ($PROFILE) ──"
BUILD_FLAGS=(--manifest-path "$MANIFEST" "${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}")
[[ "$PROFILE" == release ]] && BUILD_FLAGS+=(--release)
cargo build "${BUILD_FLAGS[@]}" --bin "$BIN"
[[ -n "$HELPER_BIN" ]] && cargo build "${BUILD_FLAGS[@]}" --bin "$HELPER_BIN"

if [[ "$NEEDS_CEF" == 1 ]]; then
  # Re-resolve: a first build may have downloaded into the versioned layout
  # under CEF_PATH rather than into CEF_PATH itself.
  echo "── pin: framework ──"
  CEF_DIR="$(cef_resolve_path)"
  cef_check_pin "$CEF_DIR" "" 1
fi

BIN_DIR="$ROOT/target/$PROFILE"
[[ "$MANIFEST" == "Cargo.toml" ]] || BIN_DIR="$ROOT/$(dirname "$MANIFEST")/target/$PROFILE"
for b in "$BIN" ${HELPER_BIN:+"$HELPER_BIN"}; do
  [[ -x "$BIN_DIR/$b" ]] || { echo "bundle-app: no $BIN_DIR/$b after build" >&2; exit 1; }
done

# ------------------------------------------------------------- assemble -----

OUT="${OUT:-$ROOT/target/bundle}"
APP="$OUT/$BIN.app"
# Assemble into a scratch path and rename at the end. A half-written 323 MB
# bundle that looks like a whole one is the kind of thing that gets launched
# at 2am; a rename on the same filesystem is atomic and costs nothing.
STAGE="$OUT/.$BIN.partial"

echo "── assemble ──"
mkdir -p "$OUT"
rm -rf "$STAGE" "$APP"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources" "$STAGE/Contents/Frameworks"
install -m 755 "$BIN_DIR/$BIN" "$STAGE/Contents/MacOS/$BIN"

# Resources, and the icon if the target ships one. A .xib compiles to .nib —
# copying it uncompiled leaves a bundle that looks complete and has no menu.
ICON=""
if [[ -n "$RESOURCES" && -d "$RESOURCES" ]]; then
  while IFS= read -r -d '' src; do
    rel="${src#"$RESOURCES"/}"
    dst="$STAGE/Contents/Resources/$rel"
    mkdir -p "$(dirname "$dst")"
    if [[ "$src" == *.xib ]]; then
      xcrun ibtool --compile "${dst%.xib}.nib" "$src" >/dev/null
    else
      cp "$src" "$dst"
    fi
  done < <(find "$RESOURCES" -type f -print0)
  [[ -f "$RESOURCES/$BIN.icns" ]] && ICON="$BIN.icns"
fi

# The icon. Assembly, not packaging — it was in package-macos.sh, which meant
# the release bundle and the bundle boot-smoke runs were not the same artefact.
# Unlike comet's, this step is OPTIONAL: eave has no app-icon artwork yet
# (Slice Q1 drew the in-app glyph family, not a 1024 px app mark), and a
# missing icon must not be the thing that stops us shipping. Drop a pre-masked
# dist/macos/icon-1024.png in — squircle, margins and shadow baked, because
# sips cannot alpha-mask — and this starts producing a real icns with no other
# change. Until then the bundle carries the generic app icon and
# CFBundleIconFile names a file that isn't there, which is benign.
if [[ -n "$ICON_PNG" && -f "$ROOT/$ICON_PNG" ]]; then
  ICONSET="$STAGE/Contents/Resources/.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ROOT/$ICON_PNG" \
      --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$ROOT/$ICON_PNG" \
      --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$STAGE/Contents/Resources/$ICNS_NAME"
  rm -rf "$ICONSET"
elif [[ -n "$ICON_PNG" ]]; then
  echo "   no $ICON_PNG — bundling without an icon"
fi

if [[ -n "$PLIST_TEMPLATE" ]]; then
  sed "s/__VERSION__/$VERSION/g" "$ROOT/$PLIST_TEMPLATE" >"$STAGE/Contents/Info.plist"
  IDENTIFIER="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$STAGE/Contents/Info.plist")"
else
  mac_plist_write "$STAGE/Contents/Info.plist" \
    "$BIN" "$BIN" "$IDENTIFIER" "$VERSION" 0 "$ICON"
fi

if [[ "$NEEDS_CEF" == 1 ]]; then
  # 317 MB of Chromium. `ditto` rather than `cp -R`: it is Apple's own tool
  # for copying bundles, it preserves extended attributes and permissions
  # exactly, and it handles the Versions/Current symlink layout a framework
  # may use. CEF 151.3.18's framework happens to be FLAT — no Versions
  # directory, zero symlinks in 317 MB, checked — so `cp -R` would do today.
  # That is a property of this release, not of frameworks, and it is not
  # worth re-learning the day CEF changes it.
  echo "   framework ← $CEF_DIR"
  ditto "$CEF_DIR/Chromium Embedded Framework.framework" \
        "$STAGE/Contents/Frameworks/Chromium Embedded Framework.framework"

  # The five helpers: same executable, five bundles, five IDENTIFIERS. This
  # line is the fix — cef-rs writes one identifier into all six.
  for entry in "${CEF_HELPERS[@]}"; do
    name="${entry%%|*}"
    suffix="${entry##*|}"
    helper="$BIN $name"
    hdir="$STAGE/Contents/Frameworks/$helper.app"
    mkdir -p "$hdir/Contents/MacOS"
    install -m 755 "$BIN_DIR/$HELPER_BIN" "$hdir/Contents/MacOS/$helper"
    mac_plist_write "$hdir/Contents/Info.plist" \
      "$helper" "$helper" "$IDENTIFIER.$suffix" "$VERSION" 1
    echo "   helper     $IDENTIFIER.$suffix"
  done
fi

# --------------------------------------------------------------- sign -------

# Unsigned is the default and it WORKS: every Mach-O is already linker-signed
# adhoc, which is all Apple silicon requires in order to exec. Signing is
# opt-in, runs exactly once, and is the last thing that touches the bundle —
# see mac_sign_bundle for the rule and what breaks when it is broken.
if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  echo "── sign ($CODESIGN_IDENTITY) ──"
  mac_sign_bundle "$STAGE" "$CODESIGN_IDENTITY" "$ROOT/dist/macos"
fi

mv "$STAGE" "$APP"
trap - EXIT

# --------------------------------------------------------------- verify -----

if [[ "$NEEDS_CEF" == 1 ]]; then
  echo "── verify ──"
  "$ROOT/scripts/verify-cef-bundle.sh" "$APP"
fi

echo
echo "bundled: $APP  ($(du -sh "$APP" | cut -f1))"
