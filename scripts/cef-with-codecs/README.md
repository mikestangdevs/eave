# CEF with proprietary codecs — pin 151.3.18

**2026-08-29: GATE met.** Artefact at `~/.local/share/cef-codecs`
(SHA1 `5e0a810c384cbdf9eb8fc96b9fdf3b985d4a205d`). Live H.264 PASS,
VP9 still plays, `pin.env` SHA1 written, `check-cef-pin.sh --strict`
consistent with `CEF_PATH=$HOME/.local/share/cef-codecs`. **Do not
start another Chromium checkout.** Report:
`docs/reports/stage-br1-codecs.md`.

Drop-in replacement for the Spotify `minimal` tarball that used to be
named in `dist/cef/pin.env` (`8245a003…`, `proprietary_codecs=false`).
Same CEF commit, same Chromium tag, same crate (`151.6.0+151.3.18`).
Different GN args, so a different SHA1.

This is build infrastructure. It does not change `crates/eave-app/src/web/`.

## Why a self-build

Spotify's public CDN (`cef-builds.spotifycdn.com`) is the trusted
source of *this pin* and is what `~/.local/share/cef` holds today:

```
cef_binary_151.3.18+gbeff58d+chromium-151.0.7922.138_macosarm64_minimal.tar.bz2
sha1 8245a003c52c01811310b9b7d026e92dff859626
```

Every flavour on that CDN (`standard` / `minimal` / `client`) is built
with `proprietary_codecs=false` and `ffmpeg_branding=Chromium`. There
is no Chrome-branded artefact of 151.3.18 from Spotify, from
chromiumembedded, or from any other provenance we would load into a
browser framework. Random GitHub "cef with codecs" tarballs are a
supply-chain decision we are not making.

So: self-build at `beff58dbc4d0fd12b3eafea8f5314ce22e649078` /
`refs/tags/151.0.7922.138`.

## Disk

A checkout+official build wants **150–200 GB**. `build-cef-with-codecs.sh`
refuses to start if `--download-dir` has less than 200 GiB free.

Do not run it on this laptop's Data volume.

## Build (remote / self-hosted Mac)

```
scripts/cef-with-codecs/build-cef-with-codecs.sh \
    --download-dir /Volumes/Big/cef-151.3.18
```

`--dry-run` prints the `automate-git.py` line and stops. Xcode 26+,
Python 3, ninja, 32 GiB RAM preferred.

GN args (also in `gn-args.gn`):

```
is_official_build=true
is_component_build=false
proprietary_codecs=true
ffmpeg_branding=Chrome
```

`enable_widevine=true` is a CEF *required* arg — do not set it here.

## Install (sidecar, not the live cache)

```
scripts/cef-with-codecs/install-local.sh \
    /Volumes/Big/cef-151.3.18/.../cef_binary_*_macosarm64_minimal.tar.bz2
```

Default dest: `~/.local/share/cef-codecs`. Refuses to write
`~/.local/share/cef`. Pass `--write-pin` only when this artefact is
the one we will ship; that rewrites `CEF_ARCHIVE_SHA1_arm64` and
leaves `CEF_VERSION` / `CEF_CRATE` / `CEF_CHROMIUM` alone.

```
export CEF_PATH=$HOME/.local/share/cef-codecs
scripts/check-cef-pin.sh --strict
scripts/bundle-app.sh --features web --out /tmp/eave-br1
scripts/boot-smoke.sh --web --cef --out /tmp/eave-br1-smoke
```

## Widevine

See `inspect-widevine.sh` and `docs/reports/stage-br1-codecs.md`.
The empty `WidevineCdm/` on the public framework is a runtime miss
(no on-demand `CefComponentUpdater::Update`, probe sessions shorter
than the 600–900s background delay). It is not `enable_widevine=false`.

## Trap

Never extensions. Content blocking is `CefResourceRequestHandler`, not MV3.
