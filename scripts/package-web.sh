#!/usr/bin/env bash
# Assemble eave.app with Chromium inside it (Slice C1).
#
# SUPERSEDED, and should be deleted rather than maintained. `slice-c2` landed
# `scripts/bundle-app.sh` as the one bundler in the tree, and it does this
# properly — six distinct helper identifiers, a real version pin, entitlements.
# This exists only because that branch was not on this one when a bundle was
# needed to verify against. The one thing here worth moving across is the
# CARGO shim below. See docs/reports/stage-c1-osr.md, caveat 4.
#
# `cargo build` alone does not produce a runnable artifact once CEF is in the
# build: the binary only runs from inside an assembled `.app`, beside a
# 323 MB framework and five nested helper bundles. This is that step.
#
# WHY THE CARGO SHIM
# `bundle-cef-app` builds the helper itself, with a hard-coded
# `cargo build --bin <helper>` and no way to pass `--features`. Our helper
# needs `--features web` (the CEF dependency is optional, so that the test
# suite never has to fetch Chromium). It takes the cargo binary from `$CARGO`,
# so a shim that appends the feature to `build` — and only to `build`, since
# a stray `--features` on `cargo metadata` is an error — is the seam that
# exists. Better than making `web` a default feature, which would put a
# 343 MB download in the path of `cargo test`.
set -euo pipefail

cd "$(dirname "$0")/.."
export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

shim="$(mktemp -d)/cargo"
cat > "$shim" <<'SHIM'
#!/usr/bin/env bash
if [ "${1:-}" = "build" ]; then
    shift
    exec cargo build --features web "$@"
fi
exec cargo "$@"
SHIM
chmod +x "$shim"

# The icon. eave has no app-icon artwork yet (Slice Q1 drew the in-app glyph
# family, not a 1024 px app mark), and `package-macos.sh` deliberately treats
# that as "ship without one". `bundle-cef-app` is less forgiving: it fails
# outright on a missing `.icns`. So synthesize a blank one when there is no
# artwork — same stance, and dropping a real `dist/macos/icon-1024.png` in
# starts producing a real icon here with no other change.
icns="crates/eave-app/resources/mac/eave.icns"
if [[ -f dist/macos/icon-1024.png ]]; then
    src=dist/macos/icon-1024.png
else
    echo "package-web: no dist/macos/icon-1024.png — bundling a blank icon"
    src="$(mktemp -d)/blank.png"
    # A transparent 1024 square. `sips` cannot create one from nothing, so
    # this goes through Python's imaging-free PNG writer: a 1024×1024 RGBA
    # of zeroes, which is a legal PNG and an invisible icon.
    python3 - "$src" <<'PY'
import struct, sys, zlib
n = 1024
raw = b"".join(b"\x00" + b"\x00" * (n * 4) for _ in range(n))
def chunk(tag, data):
    c = tag + data
    return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw))
       + chunk(b"IEND", b""))
open(sys.argv[1], "wb").write(png)
PY
fi
iconset="$(mktemp -d)/eave.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$src" \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
mkdir -p "$(dirname "$icns")"
iconutil -c icns "$iconset" -o "$icns"

# Both binaries first, so a compile error is a compile error rather than a
# bundling failure three steps later.
cargo build -p eave-app --features web --bins

CARGO="$shim" bundle-cef-app eave \
    -i com.eave.app \
    -d "eave" \
    "$@"

cat <<'NOTE'

Bundled: ./eave.app

  - A rebuild does not update the bundle. `bundle-cef-app` COPIES the binary
    in, so `cargo build` followed by running the .app silently runs the old
    one. Re-run this script.
  - The spike's "never re-sign this bundle" warning USED TO BE HERE and was
    wrong: slice-c2 re-signed five variants and all ran. The scary
    `codesign -v` message comes from the downloaded CEF framework, which
    fails verification out of the tarball, so every bundle inherits it.
NOTE
