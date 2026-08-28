#!/usr/bin/env bash
# Install a codecs-build tarball as a drop-in CEF_PATH, without touching
# ~/.local/share/cef (the Spotify public 151.3.18 every other lane is
# mapped against).
#
#     scripts/cef-with-codecs/install-local.sh /path/to/cef_binary_*_minimal.tar.bz2
#
# Writes:
#   $CEF_PATH/Chromium Embedded Framework.framework
#   $CEF_PATH/archive.json          # name + sha1, what check-cef-pin.sh reads
#   $CEF_PATH/GN_ARGS               # the flags this artefact was built with
#
# Default CEF_PATH is ~/.local/share/cef-codecs.
#
# Does NOT rewrite dist/cef/pin.env unless you pass --write-pin. The SHA1
# of a self-build is not the Spotify SHA1; writing it is a deliberate
# swap. Until then, check-cef-pin.sh will say "archive on disk is … pin
# says …" if you point CEF_PATH at this tree *and* an archive.json is
# present. After --write-pin, equality is: same CEF_VERSION / CEF_CRATE /
# CEF_CHROMIUM, new SHA1.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cef_load_pin

TARBALL=""
WRITE_PIN=0
DEST="${CEF_PATH:-$HOME/.local/share/cef-codecs}"

usage() {
  sed -n '2,22p' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --write-pin) WRITE_PIN=1; shift ;;
    --dest) DEST="$2"; shift 2 ;;
    --dest=*) DEST="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "install-local: unknown flag $1" >&2; exit 2 ;;
    *) TARBALL="$1"; shift ;;
  esac
done

if [[ -z "$TARBALL" || ! -f "$TARBALL" ]]; then
  echo "install-local: pass the *_minimal.tar.bz2 this build produced" >&2
  exit 2
fi

# Refuse to clobber the live public cache unless the operator named it.
PUBLIC="$HOME/.local/share/cef"
if [[ "$(cd "$DEST" 2>/dev/null && pwd)" == "$(cd "$PUBLIC" 2>/dev/null && pwd)" ]]; then
  echo "install-local: refusing to install over $PUBLIC" >&2
  echo "  other lanes' bundles are mapped against that framework." >&2
  echo "  pick --dest (default ~/.local/share/cef-codecs) and set CEF_PATH." >&2
  exit 1
fi

NAME="$(basename "$TARBALL")"
SHA1="$(shasum -a 1 "$TARBALL" | awk '{print $1}')"
echo "tarball: $NAME"
echo "sha1:    $SHA1"
echo "dest:    $DEST"

STAGE="$(mktemp -d /tmp/cef-codecs-extract.XXXXXX)"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

echo "extracting…"
tar -xjf "$TARBALL" -C "$STAGE"
INNER="$(find "$STAGE" -maxdepth 2 -type d -name 'cef_binary_*' | head -n 1)"
if [[ -z "$INNER" ]]; then
  INNER="$STAGE"
fi
if [[ ! -d "$INNER/Chromium Embedded Framework.framework" ]]; then
  # macOS layouts vary: Release/ or the tarball root.
  if [[ -d "$INNER/Release/Chromium Embedded Framework.framework" ]]; then
    INNER="$INNER/Release"
  else
    echo "install-local: no Chromium Embedded Framework.framework inside $NAME" >&2
    find "$STAGE" -maxdepth 3 -type d | head
    exit 1
  fi
fi

mkdir -p "$DEST"
# Whole inner tree. cef-dll-sys wants the framework plus include/, cmake/,
# libcef_dll/, CMakeLists.txt — make_distrib puts them next to each other.
rsync -a "$INNER/" "$DEST/"

python3 - <<PY
import json, pathlib
p = pathlib.Path("$DEST") / "archive.json"
p.write_text(json.dumps({
    "type": "minimal",
    "name": "$NAME",
    "sha1": "$SHA1",
    "codecs": True,
    "gn": "is_official_build=true is_component_build=false proprietary_codecs=true ffmpeg_branding=Chrome",
}, indent=2) + "\n")
print(f"wrote {p}")
PY

cp "$ROOT/scripts/cef-with-codecs/gn-args.gn" "$DEST/GN_ARGS"

HAVE="$(cef_framework_version "$DEST" || true)"
echo "framework version: ${HAVE:-unknown}"
if [[ "$HAVE" != "$CEF_VERSION" ]]; then
  echo "install-local: framework is '$HAVE', pin says '$CEF_VERSION'" >&2
  echo "  this is not a drop-in. Do not --write-pin." >&2
  exit 1
fi

echo
echo "pin.env fragment (SHA1 is the only number that should move):"
echo "  CEF_ARCHIVE_arm64=$NAME"
echo "  CEF_ARCHIVE_SHA1_arm64=$SHA1"

if [[ "$WRITE_PIN" == 1 ]]; then
  PIN="$ROOT/dist/cef/pin.env"
  python3 - "$PIN" "$NAME" "$SHA1" <<'PY'
import pathlib, re, sys
pin, name, sha = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = pin.read_text()
text, n1 = re.subn(r'^CEF_ARCHIVE_arm64=.*$', f'CEF_ARCHIVE_arm64={name}', text, count=1, flags=re.M)
text, n2 = re.subn(r'^CEF_ARCHIVE_SHA1_arm64=.*$', f'CEF_ARCHIVE_SHA1_arm64={sha}', text, count=1, flags=re.M)
if n1 != 1 or n2 != 1:
    sys.exit(f"install-local: failed to rewrite archive fields (n1={n1} n2={n2})")
pin.write_text(text)
print(f"wrote {pin}")
PY
  echo "pin.env SHA1 updated. CEF_VERSION / CEF_CRATE / CEF_CHROMIUM unchanged."
else
  echo "pin.env not written (pass --write-pin when the artefact is the one we will ship)."
fi

echo
echo "use:"
echo "  export CEF_PATH=$DEST"
echo "  scripts/check-cef-pin.sh --strict"
echo "  # then bundle with that CEF_PATH; do not restamp ~/.local/share/cef"
