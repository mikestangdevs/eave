#!/usr/bin/env bash
# Assert that an assembled .app is one macOS will actually run, and that the
# three defects the CEF spike found have not come back.
#
#     scripts/verify-cef-bundle.sh target/bundle/cef-osr-spike.app
#
# Called by scripts/bundle-cef-app.sh at the end of every assembly, and
# runnable on its own against any bundle — including one somebody else built,
# which is the point: the checks are properties of the artefact, not of the
# process that made it.
#
# Every check prints a line whether it passes or fails, because a verifier
# that is silent when happy teaches you nothing about what it looked at.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/mac-bundle.sh
source "$ROOT/scripts/mac-bundle.sh"
cef_load_pin

APP="${1:-}"
[[ -n "$APP" && -d "$APP" ]] || { echo "verify-cef-bundle: usage: $0 <path.app>" >&2; exit 2; }
APP="$(cd "$APP" && pwd)"
NAME="$(basename "$APP" .app)"

# This verifier only knows about CEF bundles. A bundle with no framework in it
# is not a failing CEF bundle, it is a different kind of thing, and reporting
# nine violations against eave.app today would be nine pieces of noise that
# teach a reader to ignore the tool.
if [[ ! -d "$APP/Contents/Frameworks/Chromium Embedded Framework.framework" ]]; then
  echo "verify-cef-bundle: $(basename "$APP") contains no CEF framework — nothing here applies"
  exit 2
fi

fails=0
ok()   { echo "  ✓ $*"; }
note() { echo "  · $*"; }
bad()  { echo "  ✗ $*" >&2; fails=$((fails + 1)); }

# PlistBuddy prints "File Doesn't Exist, Will Create: …" to STDOUT when the
# file is missing, so `2>/dev/null` alone hands that string back as if it were
# a value. Guard on the file instead.
plist() { [[ -f "$1" ]] || return 1; /usr/libexec/PlistBuddy -c "Print :$2" "$1" 2>/dev/null; }
# The parenthesised words in `flags=0x20002(adhoc,linker-signed)`.
sigflags() { codesign -d --verbose=2 "$1" 2>&1 | sed -n 's/.*flags=0x[0-9a-f]*(\([^)]*\)).*/\1/p'; }

# ---------------------------------------------------------------- layout ----

echo "layout"
[[ -x "$APP/Contents/MacOS/$NAME" ]] &&
  ok "executable Contents/MacOS/$NAME" ||
  bad "no executable at Contents/MacOS/$NAME"

FW="$APP/Contents/Frameworks/Chromium Embedded Framework.framework"
if [[ -d "$FW" ]]; then
  ok "framework present ($(du -sh "$FW" | cut -f1))"
else
  bad "no Chromium Embedded Framework.framework in Contents/Frameworks"
fi

# DOES THE BINARY ACTUALLY CONTAIN A BROWSER?
#
# Everything above this line checks the SCAFFOLDING — the framework is
# beside the binary, the helpers exist, the identifiers are distinct. None
# of it looks at the binary, so a bundle assembled around an eave built
# WITHOUT `--features web` passed every check and then showed a blank page.
# That is not hypothetical: `target/` is shared, and this checkout has had
# several concurrent cargo invocations in it — a peer running `cargo build`
# or `cargo test` without the feature overwrites `target/debug/eave`
# between this script's cargo step and its copy. The bundle then carries
# 317 MB of Chromium beside a binary that cannot call it, and said
# "eave.app is sound".
#
# `cef_load_library` is the marker because CEF is dlopened on macOS rather
# than linked, so `otool -L` shows nothing either way; the loader symbol is
# present only when the feature compiled it in. Checked ONLY when the
# framework is there — a deliberately feature-off bundle is a real
# configuration and must still verify.
if [[ -d "$FW" ]]; then
  # `strings | grep -q` would be wrong here: `grep -q` exits at the first
  # match, `strings` takes SIGPIPE, and under `set -o pipefail` the pipeline
  # reports failure *because* the marker was found. Count into a variable
  # instead, so the pipeline always runs to completion.
  CEF_MARKERS="$(strings -a "$APP/Contents/MacOS/$NAME" | grep -cF "cef_load_library" || true)"
  if [[ "$CEF_MARKERS" -gt 0 ]]; then
    ok "binary was built with --features web"
  else
    bad "framework is present but the binary has no CEF loader in it — \
built without --features web; every page will be blank"
  fi
fi

# ------------------------------------------------------------- identity ----

echo "identity"
MAIN_PLIST="$APP/Contents/Info.plist"
MAIN_ID="$(plist "$MAIN_PLIST" CFBundleIdentifier)"
[[ -n "$MAIN_ID" ]] && ok "app $MAIN_ID" || bad "app has no CFBundleIdentifier"

# THE DEFECT. cef-rs's bundler hands every bundle the same BundleInfo, so all
# six identifiers come out identical (measured 2026-08-21: six × com.eave.cefosr).
# macOS keys TCC grants, LaunchServices registration, designated requirements,
# notarization tickets and defaults domains off this string; five child
# processes claiming to be their own parent is not cosmetic. Worse, a signing
# target IS a bundle, so identical identifiers make it impossible to give the
# renderer narrower entitlements than the browser process even in principle.
ids=("$MAIN_ID")
found_helpers=0
for entry in "${CEF_HELPERS[@]}"; do
  hname="${entry%%|*}"
  suffix="${entry##*|}"
  hplist="$APP/Contents/Frameworks/$NAME $hname.app/Contents/Info.plist"
  if [[ ! -f "$hplist" ]]; then
    bad "missing helper bundle '$NAME $hname.app'"
    continue
  fi
  found_helpers=$((found_helpers + 1))
  hid="$(plist "$hplist" CFBundleIdentifier)"
  ids+=("$hid")
  want="$MAIN_ID.$suffix"
  [[ "$hid" == "$want" ]] &&
    ok "$hname → $hid" ||
    bad "$hname is '$hid', expected '$want'"

  # A helper with a Dock tile is five extra icons and five extra menu bars.
  [[ "$(plist "$hplist" LSUIElement)" == "1" ]] ||
    bad "$hname is missing LSUIElement=1"
  # Its executable must be the helper binary, not the app's.
  [[ -x "$APP/Contents/Frameworks/$NAME $hname.app/Contents/MacOS/$NAME $hname" ]] ||
    bad "$hname has no executable named '$NAME $hname'"
done
[[ "$found_helpers" == 5 ]] &&
  ok "5 helper bundles" ||
  bad "$found_helpers helper bundles, expected 5"

distinct="$(printf '%s\n' "${ids[@]}" | sort -u | wc -l | tr -d ' ')"
[[ "$distinct" == "${#ids[@]}" ]] &&
  ok "${#ids[@]} bundles, $distinct distinct identifiers" ||
  bad "${#ids[@]} bundles share only $distinct identifiers — the cef-rs defect is back"

# Purpose strings belong to the bundle TCC will name in a prompt, which after
# the identifier fix is the app and never a helper.
[[ -n "$(plist "$MAIN_PLIST" NSCameraUsageDescription)" ]] &&
  ok "app carries TCC purpose strings" ||
  bad "app has no NSCameraUsageDescription — a getUserMedia prompt will crash it"

# ------------------------------------------------------------------ pin ----

echo "pin"
FW_VER="$(plist "$FW/Resources/Info.plist" CFBundleShortVersionString | sed 's/\.0$//')"
[[ "$FW_VER" == "$CEF_VERSION" ]] &&
  ok "bundled framework is $FW_VER" ||
  bad "bundled framework is '$FW_VER', pin says $CEF_VERSION"

# ------------------------------------------------------------ signature ----

# THE RULE: any binary rewrite re-bundles, never re-signs.
#
# A fresh bundle is LINKER-SIGNED throughout — the adhoc signature the linker
# embedded, Info.plist `not bound`, `Sealed Resources=none`. That state runs on
# Apple silicon and is what we ship today.
#
# Once anything runs `codesign` over the assembled app, the app grows sealed
# resources and a bound Info.plist, and every later edit inside the bundle —
# a rebuilt binary copied over, a plist corrected — invalidates the seal.
# `codesign --verify` then says exactly what the spike saw:
#
#     code has no resources but signature indicates they must be present
#
# Under an adhoc signature the app still launches; under a real Developer ID
# signature, or a hardened runtime, it does not. Either way it is a bundle
# whose signature is a lie, and the fix is never to re-sign it — it is to run
# the bundler again, which takes seconds.
echo "signature"
main_flags="$(sigflags "$APP")"
note "app signature: ${main_flags:-none}"

if [[ "$main_flags" == *linker-signed* ]]; then
  ok "unsigned distribution — every Mach-O still carries its linker signature"
  for entry in "${CEF_HELPERS[@]}"; do
    hname="${entry%%|*}"
    hpath="$APP/Contents/Frameworks/$NAME $hname.app"
    [[ -d "$hpath" ]] || continue
    [[ "$(sigflags "$hpath")" == *linker-signed* ]] ||
      bad "'$hname' lost its linker signature — something re-signed it in place"
  done
else
  # Signed mode. Now the seal is real and staleness is fatal, so check it.
  out="$(codesign --verify --strict "$APP" 2>&1)"; rc=$?
  if [[ "$rc" == 0 ]]; then
    ok "signed bundle verifies"
  elif [[ "$out" == *"code has no resources"* ]]; then
    bad "signed, then rewritten in place — re-bundle, do not re-sign"
    bad "  $out"
  else
    bad "signature does not verify: $out"
  fi

  # Hardened runtime without the entitlements below is the configuration that
  # dies, and both deaths were measured on 2026-08-21:
  #   · no disable-library-validation → dlopen of the CEF framework is refused
  #     ("different Team IDs"), the process aborts before our code runs. CEF
  #     ships adhoc-signed by its own project, so its team identity is never
  #     ours unless we re-sign 214 MB of Chromium on every build.
  #   · no allow-jit → the app launches, the browser opens, and every page is
  #     blank forever. The only trace is one line: "V8 process OOM (Failed to
  #     reserve virtual memory for CodeRange)".
  if [[ "$main_flags" == *runtime* ]]; then
    ents="$(codesign -d --entitlements - --xml "$APP" 2>/dev/null)"
    [[ "$ents" == *disable-library-validation* ]] &&
      ok "hardened runtime + disable-library-validation" ||
      bad "hardened runtime without disable-library-validation — dlopen of the framework will be refused"
    for entry in "${CEF_HELPERS[@]}"; do
      hname="${entry%%|*}"
      hpath="$APP/Contents/Frameworks/$NAME $hname.app"
      [[ -d "$hpath" ]] || continue
      hents="$(codesign -d --entitlements - --xml "$hpath" 2>/dev/null)"
      [[ "$hents" == *allow-jit* ]] ||
        bad "'$hname' has no allow-jit entitlement — pages will render blank"
    done
  else
    note "no hardened runtime — not notarizable, which is today's posture"
  fi
fi

echo
if [[ "$fails" == 0 ]]; then
  echo "verify-cef-bundle: $NAME.app is sound"
else
  echo "verify-cef-bundle: $fails problem(s) in $NAME.app" >&2
fi
exit $(( fails > 0 ))
