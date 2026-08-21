#!/usr/bin/env bash
# One boot smoke per fixture (Stage J step 4; the render-mode loop went
# away with the opaque build on 2026-08-19).
#
# This is all that is left of the scripted-drive rig. Clicking is no
# longer scripted at all: every interaction the old rig drove through the
# window server — rail affordances, the two-step delete, selection,
# composer, badges, the diff pane — is asserted hermetically by the
# harness in `crates/eave-app/src/ui_harness.rs`, where a synthetic click
# lands on a measured hitbox and cannot miss.
#
# What is left needs a REAL compositor and therefore cannot move: that
# the binary boots, opens a window, renders real frames through Metal in
# both render modes, and holds its scroll anchor while doing it. The
# replay assert is the self-checking part; a human still has to look at
# the window for anything about how it LOOKS.
#
# IT RUNS THE BUNDLE, NOT THE LOOSE BINARY (Slice C2, 2026-08-21). It used
# to run ./target/debug/eave. Once eave links CEF that stops being a runnable
# artefact at all — CEF resolves its framework and its five helper .apps
# relative to its own bundle path, so a loose executable dies inside
# CefInitialize — and a smoke that exercises a path nothing ships is worth
# very little anyway. `scripts/bundle-app.sh` runs cargo itself, so this is
# still one command and still builds what it tests. `--unbundled` restores the
# old behaviour for a fast inner loop.
#
# Usage: scripts/boot-smoke.sh [speed] [--unbundled] [--cef|--no-cef] [--web]
#          speed      default 30 ≈ 16 s per fixture
#          --cef      also smoke the bundled CEF app. Default: on if one has
#                     already been bundled, off otherwise — bundling it is a
#                     90 s build and 362 MB, which is not an inner-loop cost.
#          --web      build eave itself WITH the browser (`--features web`)
#                     and smoke a real page in it. Same cost reasoning as
#                     --cef, so same opt-in; this is the one that proves the
#                     thing we actually ship.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

SPEED=30
BUNDLED=1
CEF=auto
WEB=0
for arg in "$@"; do
  case "$arg" in
    --unbundled) BUNDLED=0 ;;
    --bundled)   BUNDLED=1 ;;
    --cef)       CEF=1 ;;
    --no-cef)    CEF=0 ;;
    --web)       WEB=1 ;;
    [0-9]*)      SPEED="$arg" ;;
    *) echo "boot-smoke: unknown argument $arg" >&2; exit 2 ;;
  esac
done

# `--web` changes what the fixture runs below are running: the SAME asserts,
# from a bundle that has Chromium linked into it and 317 MB of framework
# beside it. That is the point rather than an aside — the replay asserts are
# the only automated check that eave still boots and holds its scroll anchor,
# and until now they only ever ran against a build with no browser in it.
#
# Written as an `if` and not `[ "$WEB" = 1 ] && BUNDLE_FLAGS+=(...)`, because
# under `set -e` that one-liner EXITS THE SCRIPT when WEB is 0: the test is
# the last command, it returns 1, and the shell takes the whole line as a
# failed statement. Silent, and it looks like the smoke passed with no output.
BUNDLE_FLAGS=()
if [ "$WEB" = 1 ]; then BUNDLE_FLAGS+=(--features web); fi

# Two fixtures:
#   real-claude   — a genuine capture; the pacing and markdown the
#                   streaming/anchor work was tuned against.
#   approvals     — SYNTHESISED (Slice K), and labelled so. There is no
#                   real capture of an approval exchange through eave's
#                   own wire yet, and fabricating records into the real
#                   capture would destroy what makes it worth having.
#                   Its job is the one thing the hermetic harness cannot
#                   do: render the approval card through Metal, at real
#                   glyph metrics, and prove the Pending→settled height
#                   change never moves the scroll anchor.
FIXTURES=(
  "fixtures/real-claude-2026-08-16.tap.jsonl"
  "fixtures/approvals-synthetic.tap.jsonl"
)
for f in "${FIXTURES[@]}"; do
  [ -f "$f" ] || { echo "boot-smoke: missing $f" >&2; exit 1; }
done

# Never let a smoke run append to the real dogfood log.
export EAVE_DOGFOOD_LOG="${EAVE_DOGFOOD_LOG:-${TMPDIR:-/tmp}/eave-boot-smoke.md}"

if [ "$BUNDLED" = 1 ]; then
  # cargo's warnings go to a log rather than the terminal; a smoke run that
  # buries its two assert lines under 40 lines of `unused_unsafe` is a smoke
  # run nobody reads. The log is printed if the bundle step fails.
  BUILD_LOG="${TMPDIR:-/tmp}/eave-boot-smoke-build.log"
  scripts/bundle-app.sh "${BUNDLE_FLAGS[@]+"${BUNDLE_FLAGS[@]}"}" >"$BUILD_LOG" 2>&1 ||
    { echo "boot-smoke: bundle failed" >&2; tail -30 "$BUILD_LOG" >&2; exit 1; }
  APP="$ROOT/target/bundle/eave.app"
  EAVE="$APP/Contents/MacOS/eave"
  echo "boot-smoke: running from the bundle ($APP)"
else
  cargo build --workspace
  EAVE="$ROOT/target/debug/eave"
  echo "boot-smoke: running the loose binary — NOT what ships"
fi

# One render mode. Until 2026-08-19 this looped glass and opaque, because
# eave shipped both; the opaque build was deleted by ruling ("i never want
# it") and with it the ×2 convention every report before N3 quotes. Glass is
# the only look, so a single pass per fixture IS full coverage now — if you
# find yourself re-adding a mode loop, the design changed.
fail=0
for fixture in "${FIXTURES[@]}"; do
  echo "── boot smoke: $(basename "$fixture") ──"
  out="$(EAVE_REPLAY="$fixture" EAVE_REPLAY_SPEED="$SPEED" EAVE_REPLAY_ASSERT=1 \
         "$EAVE" 2>/dev/null | grep REPLAY_ASSERT || true)"
  echo "${out:-<no assert line — the app did not boot>}"
  case "$out" in
    *PASS*) ;;
    *) fail=1 ;;
  esac
done

# ---------------------------------------------------------------- WEB -------

# A real page, in eave's own bundle, through eave's own tab model. The
# fixture runs above prove eave still BOOTS with Chromium linked; this proves
# the browser in it works, which is a different claim and the one C1 could
# only make by hand with a screenshot.
#
# What is asserted is a DELIVERED FRAME — `[web] accel_paint #N` on stderr,
# which `EAVE_WEB_TRACE=1` emits from `OnAcceleratedPaint`. That is the exact
# line where the four silent-failure defects of C1 diverge: a framework that
# was never dlopen'ed, an async browser creation raced by a load_url, a
# CefApp dropped after initialize, and a pump that stops after two calls all
# produce a browser that exists, reports no error anywhere, and never paints.
# Frame count is the only signal that distinguishes them from success.
if [ "$WEB" = 1 ] && [ "$BUNDLED" = 1 ]; then
  if [ ! -x "$APP/Contents/MacOS/eave-web-helper" ] &&
     [ ! -d "$APP/Contents/Frameworks/eave Helper.app" ]; then
    echo "── web: FAILED — bundle has no CEF helpers ──" >&2
    fail=1
  else
    echo "── web: a page in eave ──"
    PAGE="file://$ROOT/spike/cef-osr/probe/static.html"
    [ -f "$ROOT/spike/cef-osr/probe/static.html" ] ||
      PAGE="data:text/html,<title>smoke</title><body%20style=background:%23c0ffee>"

    # A THROWAWAY DATA DIR, and a short one. The engine opens a unix socket
    # inside it and the path has to fit in SUN_LEN (~104 bytes), which a
    # temp dir under a long TMPDIR does not — it fails with "path must be
    # shorter than SUN_LEN" and reads like a CEF problem. It also has to be
    # per-run: CEF takes an exclusive lock on its cache root, so reusing the
    # developer's real data dir would mean this smoke cannot run while eave
    # is open.
    WEB_DIR="/tmp/eave-smoke-$$"
    rm -rf "$WEB_DIR"; mkdir -p "$WEB_DIR"

    out="$(EAVE_DATA_DIR="$WEB_DIR" EAVE_WEB_TRACE=1 EAVE_WEB_AT_BOOT="$PAGE" \
           perl -e 'alarm 45; exec @ARGV' "$EAVE" 2>&1 || true)"
    frames="$(printf '%s' "$out" | grep -c 'accel_paint' || true)"
    if [ "${frames:-0}" -gt 0 ]; then
      size="$(printf '%s' "$out" | sed -n 's/.*accel_paint #[0-9]* [^ ]* \([0-9]*x[0-9]*\).*/\1/p' | head -1)"
      echo "web: a page painted in eave's bundle (${frames} traced frame(s), ${size:-size unknown})"
    else
      echo "web: FAILED — eave's bundle opened a tab and never painted a page" >&2
      printf '%s\n' "$out" | grep -E '^\[web\]|CefInitialize|Chromium' | tail -20 >&2
      fail=1
    fi
    rm -rf "$WEB_DIR"
  fi
elif [ "$WEB" = 1 ]; then
  echo "── web: skipped — needs the bundle (drop --unbundled) ──"
fi

# ---------------------------------------------------------------- CEF -------

# The CEF bundle's own boot smoke. It proves the ARTEFACT, which is the part
# that stops working silently: a bundle whose helper .apps are misnamed, whose
# framework is the wrong version, or which was signed and then edited, still
# looks exactly like a working one in Finder.
#
# It used to say "nothing in eave links CEF yet", and that is no longer true —
# `--features web` links it, and `--web` above smokes THAT bundle serving a
# real page. This stage stays anyway, and is worth keeping precisely because
# eave now has its own: when a page breaks, the two together answer "is it
# Chromium or is it us?" in one run. The spike is the control.
CEF_APP="$ROOT/target/bundle/cef-osr-spike.app"
if [ "$CEF" = auto ]; then
  [ -d "$CEF_APP" ] && CEF=1 || CEF=0
fi

if [ "$CEF" = 0 ]; then
  echo "── cef: skipped (pass --cef to bundle and smoke it) ──"
elif [ ! -d "spike/cef-osr" ]; then
  echo "── cef: skipped — no spike/cef-osr in this tree ──"
elif [ ! -d "${CEF_PATH:-$HOME/.local/share/cef}/Chromium Embedded Framework.framework" ]; then
  echo "── cef: skipped — no framework at ${CEF_PATH:-$HOME/.local/share/cef} ──"
  echo "        (first build downloads ~343 MB; see dist/cef/pin.env)"
else
  echo "── cef: bundle + boot ──"
  BUILD_LOG="${TMPDIR:-/tmp}/eave-boot-smoke-cef.log"
  if ! scripts/bundle-app.sh cef-osr-spike >"$BUILD_LOG" 2>&1; then
    echo "cef: bundle failed" >&2; tail -30 "$BUILD_LOG" >&2; fail=1
  fi
  grep -E '^(  ✓|  ✗|  ·)' "$BUILD_LOG" || true

  # A LOCAL page, never the network. A run against the default URL on a
  # machine with no route out produces a browser that opens, reports zero
  # frames and hangs — which reads exactly like a broken bundle. Measured
  # 2026-08-21; it cost a wrong conclusion before it was noticed.
  PAGE="file://$ROOT/spike/cef-osr/probe/static.html"

  # `perl -e 'alarm N; exec'` rather than `timeout`, which macOS does not
  # ship. The alarm survives the exec, so the SIGALRM lands on the app.
  out="$(EAVE_CEF_OSR=1 EAVE_CEF_SECS=3 EAVE_CEF_FPS=60 \
         perl -e 'alarm 120; exec @ARGV' \
           "$CEF_APP/Contents/MacOS/cef-osr-spike" --url="$PAGE" 2>&1 || true)"
  frames="$(printf '%s' "$out" | sed -n 's/^frames  *: \([0-9]*\) view.*/\1/p')"
  if [ -n "$frames" ] && [ "$frames" -gt 0 ]; then
    echo "cef: booted from the bundle and painted $frames frame(s) through IOSurface"
  else
    echo "cef: FAILED — no frames from the bundled app" >&2
    printf '%s\n' "$out" | tail -20 >&2
    fail=1
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo "boot-smoke: FAILED" >&2
  exit 1
fi
echo "boot-smoke: every fixture booted and held anchor"
