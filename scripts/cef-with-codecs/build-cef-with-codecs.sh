#!/usr/bin/env bash
# Build a drop-in CEF *minimal* distribution at OUR pin, with proprietary
# codecs. Output is a tarball whose version string matches dist/cef/pin.env
# (151.3.18 + chromium 151.0.7922.138) so check-cef-pin.sh equality still
# holds both directions; only the SHA1 changes.
#
#     scripts/cef-with-codecs/build-cef-with-codecs.sh \
#         --download-dir /path/with/200G/free
#
# THIS SCRIPT REFUSES TO RUN ON A VOLUME WITH LESS THAN 200 GiB FREE.
# A Chromium checkout+official build wants 150–200 GB; this machine has
# hit no-space-left mid-bundle before. Pass a download-dir on a disk that
# can carry it (external, CI self-hosted, remote Mac). Do not point it at
# $HOME on a machine that is already at 89 % of Data.
#
# Does not touch ~/.local/share/cef (every other lane's running bundle
# is mapped against that). install-local.sh is the swap.
#
# Not product code. Does not restamp a bundle. Does not launch eave.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cef_load_pin

# Exact pin. If pin.env moves, this script must move with it — a codecs
# build of a *newer* CEF against crate 151.6.0+151.3.18 is the footgun
# check-cef-pin.sh exists to catch.
CEF_BRANCH=7922
CEF_GIT_CHECKOUT=beff58dbc4d0fd12b3eafea8f5314ce22e649078
CHROMIUM_CHECKOUT=refs/tags/151.0.7922.138
# From CHROMIUM_BUILD_COMPATIBILITY.txt at beff58d. automate-git.py also
# reads it from the CEF checkout; this is the number we expect.
DEPOT_TOOLS_CHECKOUT=94e89b10b92cc9d6e58fc8d1b6474b7d29e8a114

NEED_FREE_GIB=200
DOWNLOAD_DIR=""
DRY_RUN=0
FORCE_DISK=0

usage() {
  sed -n '2,22p' "$0"
  echo
  echo "Flags:"
  echo "  --download-dir DIR   Chromium+CEF checkout (required). 200 GiB free."
  echo "  --dry-run            Print the automate-git.py invocation and exit."
  echo "  --force-disk         Override the 200 GiB gate (you will wedge)."
  echo "  --help"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --download-dir) DOWNLOAD_DIR="$2"; shift 2 ;;
    --download-dir=*) DOWNLOAD_DIR="${1#*=}"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --force-disk) FORCE_DISK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "build-cef-with-codecs: unknown argument $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$DOWNLOAD_DIR" ]]; then
  echo "build-cef-with-codecs: --download-dir is required (and must have ${NEED_FREE_GIB} GiB free)." >&2
  echo "  This machine's Data volume is not a candidate. See docs/reports/stage-br1-codecs.md." >&2
  exit 2
fi

# pin.env equality, both directions, before we spend a day compiling.
if [[ "$CEF_VERSION" != "151.3.18" || "$CEF_CHROMIUM" != "151.0.7922.138" || "$CEF_CRATE" != "151.6.0" ]]; then
  echo "build-cef-with-codecs: pin.env is $CEF_CRATE+$CEF_VERSION (chromium $CEF_CHROMIUM)" >&2
  echo "  this script is frozen to 151.6.0+151.3.18 / 151.0.7922.138" >&2
  echo "  bump the constants at the top of this file together with pin.env, or you get the one-sided crate check." >&2
  exit 1
fi

disk_gate() {
  local dir="$1"
  mkdir -p "$dir"
  local avail_kib
  avail_kib="$(df -k "$dir" | awk 'NR==2 {print $4}')"
  local avail_gib=$((avail_kib / 1024 / 1024))
  echo "disk gate: ${avail_gib} GiB free at $dir (need ${NEED_FREE_GIB})"
  if [[ "$avail_gib" -lt "$NEED_FREE_GIB" ]]; then
    if [[ "$FORCE_DISK" == 1 ]]; then
      echo "disk gate: OVERRIDDEN (--force-disk). A mid-build ENOSPC is on you." >&2
      return 0
    fi
    echo "disk gate: REFUSED. Chromium checkout+official build wants 150–200 GB." >&2
    echo "  Spec the remote path (self-hosted Mac / MacStadium / AWS Mac) and re-run there." >&2
    echo "  Do not start a checkout that will wedge this volume." >&2
    exit 1
  fi
}

# Tooling the CEF 151 branch actually asks for (branches_and_building.md).
xcode_gate() {
  local ver
  ver="$(xcodebuild -version 2>/dev/null | awk '/Xcode/{print $2; exit}')"
  echo "xcode: ${ver:-missing}"
  if [[ -z "$ver" ]]; then
    echo "build-cef-with-codecs: Xcode is required (26.0+ for this branch)." >&2
    exit 1
  fi
  local major="${ver%%.*}"
  if [[ "$major" -lt 26 ]]; then
    echo "build-cef-with-codecs: Xcode $ver is older than the 26.0 this CEF branch lists." >&2
    exit 1
  fi
}

ram_note() {
  local bytes gib
  bytes="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
  gib=$((bytes / 1024 / 1024 / 1024))
  echo "ram: ${gib} GiB"
  if [[ "$gib" -lt 32 ]]; then
    echo "ram: WARNING — official Chromium wants ~32 GiB; ${gib} GiB will swap. Prefer the remote builder." >&2
  fi
}

GN_DEFINES="is_official_build=true is_component_build=false proprietary_codecs=true ffmpeg_branding=Chrome"

# automate-git.py: prefer a local chromiumembedded/cef copy (the 32 MB
# overlay at ~/dev/eave/cef-src is one), else fetch master as CEF docs
# require ("use the most recent master version of this script even when
# building release branches").
resolve_automate() {
  if [[ -n "${CEF_AUTOMATE_GIT:-}" && -f "$CEF_AUTOMATE_GIT" ]]; then
    echo "$CEF_AUTOMATE_GIT"
    return
  fi
  if [[ -n "${CEF_SRC:-}" && -f "$CEF_SRC/tools/automate/automate-git.py" ]]; then
    echo "$CEF_SRC/tools/automate/automate-git.py"
    return
  fi
  if [[ -f "$HOME/dev/eave/cef-src/tools/automate/automate-git.py" ]]; then
    echo "$HOME/dev/eave/cef-src/tools/automate/automate-git.py"
    return
  fi
  local dest="$DOWNLOAD_DIR/automate-git.py"
  echo "fetching automate-git.py from chromiumembedded/cef master → $dest" >&2
  curl -fsSL -o "$dest" \
    https://raw.githubusercontent.com/chromiumembedded/cef/master/tools/automate/automate-git.py
  echo "$dest"
}

disk_gate "$DOWNLOAD_DIR"
xcode_gate
ram_note

AUTOMATE="$(resolve_automate)"
echo "automate-git.py: $AUTOMATE"
echo "pin: crate $CEF_CRATE+ $CEF_VERSION  chromium $CEF_CHROMIUM"
echo "cef checkout: $CEF_GIT_CHECKOUT (branch $CEF_BRANCH)"
echo "chromium checkout: $CHROMIUM_CHECKOUT"
echo "depot_tools (expected): $DEPOT_TOOLS_CHECKOUT"
echo "GN_DEFINES: $GN_DEFINES"
echo "download-dir: $DOWNLOAD_DIR"

ARCH_FLAG=()
case "$(uname -m)" in
  arm64) ARCH_FLAG=(--arm64-build) ;;
  x86_64) ARCH_FLAG=(--x64-build) ;;
  *) echo "build-cef-with-codecs: uname -m $(uname -m) is not a pinned arch" >&2; exit 1 ;;
esac

# --minimal-distrib-only matches CEF_DISTRIBUTION=minimal in pin.env.
# --no-debug-build: we ship Release.
# --no-chromium-history: the 150–200 GB number assumes this; full history
# is larger still.
# --with-pgo-profiles: official builds expect them; skipping produces a
# slower binary and can fail the official-build GN assert on some revs.
# --checkout / --chromium-checkout: pin equality. Without them,
# automate-git.py takes the branch tip (this overlay was already at
# 151.0.7922.174, which is NOT our pin).
CMD=(
  python3 "$AUTOMATE"
  --download-dir="$DOWNLOAD_DIR"
  --branch="$CEF_BRANCH"
  --checkout="$CEF_GIT_CHECKOUT"
  --chromium-checkout="$CHROMIUM_CHECKOUT"
  --minimal-distrib-only
  --no-debug-build
  --no-chromium-history
  --with-pgo-profiles
  --build-target=cefsimple
  "${ARCH_FLAG[@]}"
)

echo
echo "will run:"
echo "  GN_DEFINES='$GN_DEFINES' \\"
printf '  '; printf '%q ' "${CMD[@]}"; echo
echo

if [[ "$DRY_RUN" == 1 ]]; then
  echo "dry-run: stopping before depot_tools / chromium."
  exit 0
fi

export GN_DEFINES
"${CMD[@]}"

# CEF writes the tarball under chromium/src/cef/binary_distrib/.
DISTRIB="$(find "$DOWNLOAD_DIR" -type d -name binary_distrib 2>/dev/null | head -n 1 || true)"
if [[ -z "$DISTRIB" ]]; then
  echo "build-cef-with-codecs: build finished but no binary_distrib/ was found under $DOWNLOAD_DIR" >&2
  exit 1
fi

echo
echo "binary_distrib: $DISTRIB"
ls -lh "$DISTRIB" || true
echo
echo "next: scripts/cef-with-codecs/install-local.sh $DISTRIB/cef_binary_${CEF_VERSION}+gbeff58d+chromium-${CEF_CHROMIUM}_macosarm64_minimal.tar.bz2"
echo "  (name may differ by hash abbreviation; pick the *_minimal.tar.bz2)"
echo "then: CEF_PATH=\$HOME/.local/share/cef-codecs scripts/check-cef-pin.sh --strict"
