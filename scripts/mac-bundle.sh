#!/usr/bin/env bash
# Sourceable library for assembling macOS .app bundles that contain CEF.
# Not runnable on its own — `source` it. Used by scripts/bundle-cef-app.sh,
# scripts/verify-cef-bundle.sh and scripts/package-macos.sh.
#
# WHY THIS EXISTS AT ALL, given cef-rs ships `bundle-cef-app`: that tool
# writes ONE `Info.plist` body and reuses it for the app and all five helper
# bundles, so every bundle in the tree gets the same `CFBundleIdentifier` and
# the same `CFBundleName` (`cef/src/build_util/mac.rs:83-93` passes
# `bundle_info.clone()` into `create_app` unchanged). Six bundles, one
# identity. Measured on 2026-08-21: all six read `com.eave.cefosr`. macOS
# treats the identifier as the unit of nearly everything it decides about an
# app — TCC grants, LaunchServices registration, code-signing designated
# requirements, notarization tickets, defaults domains — so five processes
# claiming to be the app they are a child of is not a cosmetic defect.
# There is no knob for it: `BundleInfo.identifier` is a single `String`
# consumed by every `create_app` call. Post-fixing the plists it wrote would
# work today — the plists are not sealed by the linker's adhoc signature —
# but it leaves a wrong bundle on disk between the two steps and re-opens
# every time the crate is bumped. So we assemble it ourselves; the layout is
# a directory copy and six plists, and owning it lets us check the pin the
# crate declines to check (see dist/cef/pin.env).

# ---------------------------------------------------------------- the pin ---

# Absolute path to the repo root, from wherever this file was sourced.
MAC_BUNDLE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# cef_load_pin: read dist/cef/pin.env into the environment.
cef_load_pin() {
  local pin="$MAC_BUNDLE_ROOT/dist/cef/pin.env"
  [[ -f "$pin" ]] || { echo "mac-bundle: missing $pin" >&2; return 1; }
  # shellcheck source=/dev/null
  source "$pin"
}

# cef_resolve_path: echo the CEF distribution directory.
#
# Deliberately the same default the spike used and the same variable the
# cef-dll-sys build script reads, so there is exactly one place a CEF lives on
# a machine and `cargo build` and the bundler cannot disagree about where.
# TWO LAYOUTS, because cef-dll-sys's build script uses whichever it finds.
# If CEF_PATH already holds a distribution it is used in place; if CEF_PATH
# does not exist, or holds one whose archive.json it rejects, the script
# downloads into $CEF_PATH/<version>/cef_<os>_<arch>/ instead and uses that.
# So the directory that ends up holding the framework after a first build is
# NOT the directory you set. Resolve, do not assume.
cef_resolve_path() {
  local base="${CEF_PATH:-$HOME/.local/share/cef}"
  local arch
  case "$(uname -m)" in
    arm64) arch=aarch64 ;;
    *)     arch="$(uname -m)" ;;
  esac
  local versioned="$base/${CEF_VERSION:-}/cef_macos_$arch"
  if [[ -d "$base/Chromium Embedded Framework.framework" ]]; then
    echo "$base"
  elif [[ -d "$versioned/Chromium Embedded Framework.framework" ]]; then
    echo "$versioned"
  else
    echo "$base"
  fi
}

# cef_framework_version <cef_dir>: the CFBundleShortVersionString of the
# framework, normalised. CEF writes "151.3.18.0"; the pin says "151.3.18".
cef_framework_version() {
  local plist="$1/Chromium Embedded Framework.framework/Resources/Info.plist"
  [[ -f "$plist" ]] || return 1
  /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist" 2>/dev/null |
    sed 's/\.0$//'
}

# cef_check_pin: refuse to build unless the framework on disk, the archive it
# came from, and the crate in the lockfile all name the SAME CEF build.
#
# This is the check download-cef declines to make. Its own check is
# `archive_version <= crate_version` (download-cef-2.3.2 src/lib.rs:100), so
# an older framework than the bindings passes in silence — the direction that
# hurts, because the bindings describe structs the framework has not got.
# Ours is equality.
# cef_check_pin <cef_dir> [lockfile] [framework-required]
#
# framework-required defaults to 1. CI passes 0 for the cheap half of the
# check: the lockfile can be verified on any runner in milliseconds, and it is
# the half that actually drifted (the spike carried a 151.3.23 crate against a
# 151.3.18 framework for its whole life). Making the 343 MB download a
# precondition for noticing that would be the wrong way round.
cef_check_pin() {
  local cef_dir="$1" lock="${2:-}" need_fw="${3:-1}"
  local fail=0
  local skip_fw=0

  local have
  if ! have="$(cef_framework_version "$cef_dir")"; then
    if [[ "$need_fw" == 1 ]]; then
      echo "  ✗ no framework at $cef_dir" >&2
      echo "    set CEF_PATH, or let spike/cef-osr's build script download it" >&2
      return 1
    fi
    echo "  · no framework at $cef_dir — lockfile checked, framework not"
    skip_fw=1
  elif [[ "$have" == "$CEF_VERSION" ]]; then
    echo "  ✓ framework $have  ($cef_dir)"
  else
    echo "  ✗ framework is $have, pin says $CEF_VERSION" >&2
    fail=1
  fi

  # archive.json is what download-cef leaves behind to record the tarball.
  # Its absence is a fact worth stating, not a failure: a framework can be
  # placed by hand and still be the right one.
  local aj="$cef_dir/archive.json"
  local arch_key; arch_key="$(uname -m)"
  if [[ "$skip_fw" == 1 ]]; then aj=""; fi
  local pinned_archive; pinned_archive="$(eval "echo \${CEF_ARCHIVE_${arch_key}:-}")"
  local pinned_sha; pinned_sha="$(eval "echo \${CEF_ARCHIVE_SHA1_${arch_key}:-}")"
  if [[ "$skip_fw" == 1 ]]; then
    :
  elif [[ -z "$pinned_archive" ]]; then
    echo "  · $arch_key has no pinned archive — version checked, hash not"
  elif [[ ! -f "$aj" ]]; then
    echo "  · no archive.json — framework placed by hand, hash not checked"
  else
    local name sha
    name="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["name"])' "$aj")"
    sha="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("sha1",""))' "$aj")"
    if [[ "$name" == "$pinned_archive" && "$sha" == "$pinned_sha" ]]; then
      echo "  ✓ archive $name"
    else
      echo "  ✗ archive on disk is $name ($sha)" >&2
      echo "    pin says            $pinned_archive ($pinned_sha)" >&2
      fail=1
    fi
  fi

  # The lockfile half. `cef 151.6.0+151.3.18` means bindings generated against
  # CEF build 151.3.18 — cargo ignores build metadata in version REQUIREMENTS,
  # so `=151.6.0` in a manifest cannot express this and the lockfile is the
  # only place the pair is written down by cargo at all.
  if [[ -n "$lock" && ! -f "$lock" ]]; then
    echo "  · no $lock — nothing in this tree depends on the cef crate"
  elif [[ -n "$lock" ]]; then
    local want="$CEF_CRATE+$CEF_VERSION" bad=0 got
    local pkg
    for pkg in cef cef-dll-sys; do
      got="$(awk -v p="$pkg" '
        $0 == "name = \"" p "\"" { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }
      ' "$lock")"
      if [[ "$got" != "$want" ]]; then
        echo "  ✗ $lock pins $pkg = $got, pin says $want" >&2
        bad=1
      fi
    done
    if [[ "$bad" == 0 ]]; then
      echo "  ✓ lockfile cef + cef-dll-sys = $want"
    else
      # TWO INVOCATIONS, not one. `cargo update -p cef -p cef-dll-sys
      # --precise <v>` moves `cef` and leaves `cef-dll-sys` where it was,
      # silently and with exit 0 — measured on this lockfile in Slice C3,
      # which is how the product build ended up half-pinned. `--precise`
      # applies to the resolution as a whole, and once `cef` is settled
      # `cef-dll-sys` is already "compatible" and never reconsidered.
      # Whoever reads this line is fixing a broken pin; give them the
      # command that actually fixes it.
      local dir; dir="$(dirname "$lock")"
      echo "    fix: (cd $dir &&" >&2
      echo "          cargo update -p cef --precise $want &&" >&2
      echo "          cargo update -p cef-dll-sys --precise $want)" >&2
      fail=1
    fi
  fi

  return "$fail"
}

# -------------------------------------------------------------- the plists ---

# The five helper bundles CEF's macOS process model requires. The names are
# CEF's, not ours — `CefExecuteProcess` is dispatched to a bundle whose name
# is `<app> Helper (<role>)`, so these strings are load-bearing.
#
# The SUFFIX column is the whole point of this file. Chromium's own convention
# (and Electron's, and Chrome's) is that a helper's identifier is the app's
# with a role appended, so five processes are five distinct identities that
# still read as belonging to one app.
#
# THESE SUFFIXES ARE EFFECTIVELY PERMANENT — the same rule the app's own
# `CFBundleIdentifier` carries in dist/macos/Info.plist, and for the same
# reason, but it needs saying HERE because these strings are generated and
# the app's is hand-written, so nobody reading a plist will find this note.
#
# macOS keys TCC grants (camera, microphone, screen recording — all things a
# browser asks for, and the RENDERER is what asks), LaunchServices records
# and notarization tickets off the identifier. Change one later and every
# machine that ran an older build keeps the old grants against an identity
# nothing claims any more: the user is re-prompted for permissions they
# already gave, and the stale entries are invisible in System Settings
# because no installed app matches them. It is not a thing you can fix in
# an update — `tccutil reset` per-identifier is the only cure and it is a
# support burden, not a migration.
#
# So: adding a SIXTH helper is fine, and renaming any of these five is not.
CEF_HELPERS=(
  "Helper|helper"
  "Helper (GPU)|helper.gpu"
  "Helper (Renderer)|helper.renderer"
  "Helper (Plugin)|helper.plugin"
  "Helper (Alerts)|helper.alerts"
)

# mac_plist_write <path> <exec-name> <bundle-name> <identifier> <version> <is-helper> [icon]
#
# Written as literal XML rather than through PlistBuddy because the bundle is
# a build output that must be byte-identical from one run to the next: a
# reviewer can read this and know what shipped, and `diff` on two bundles
# means something.
mac_plist_write() {
  local path="$1" exec_name="$2" bundle_name="$3" identifier="$4"
  local version="$5" is_helper="$6" icon="${7:-}"

  {
    cat <<XML
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>$bundle_name</string>
	<key>CFBundleExecutable</key>
	<string>$exec_name</string>
	<key>CFBundleIdentifier</key>
	<string>$identifier</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$bundle_name</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleSignature</key>
	<string>????</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
XML
    [[ -n "$icon" ]] && printf '\t<key>CFBundleIconFile</key>\n\t<string>%s</string>\n' "$icon"
    cat <<'XML'
	<key>LSEnvironment</key>
	<dict>
		<key>MallocNanoZone</key>
		<string>0</string>
	</dict>
	<key>LSFileQuarantineEnabled</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
XML
    if [[ "$is_helper" == "1" ]]; then
      # A helper must never own a Dock tile or a menu bar. This is the key
      # that makes five extra processes invisible to the person using the app.
      printf '\t<key>LSUIElement</key>\n\t<string>1</string>\n'
    else
      # Purpose strings live on the MAIN bundle only. TCC reads them from the
      # bundle that owns the responsible process, and after the identifier fix
      # the helpers are no longer that bundle — which is the second reason the
      # shared identifier mattered: with six identical identifiers there was
      # no way to say which bundle a prompt was about.
      cat <<'XML'
	<key>NSCameraUsageDescription</key>
	<string>A page you opened asked to use the camera.</string>
	<key>NSMicrophoneUsageDescription</key>
	<string>A page you opened asked to use the microphone.</string>
	<key>NSBluetoothAlwaysUsageDescription</key>
	<string>A page you opened asked to use Bluetooth.</string>
	<key>NSWebBrowserPublicKeyCredentialUsageDescription</key>
	<string>A page you opened asked to use a passkey.</string>
XML
    fi
    cat <<'XML'
</dict>
</plist>
XML
  } >"$path"
}

# --------------------------------------------------------------- signing ----

# mac_sign_bundle <app> <identity> [entitlements-dir]
#
# THE RULE THIS ENCODES: any binary rewrite re-bundles, never re-signs.
#
# The spike lost half an hour to a bundle that launched and exited printing
# nothing, with `codesign -v` saying "code has no resources but signature
# indicates they must be present". A fresh CEF bundle is LINKER-SIGNED: every
# Mach-O carries the adhoc signature the linker put there, the Info.plist is
# `not bound`, and `Sealed Resources=none` (measured 2026-08-21). Run
# `codesign` over the assembled app and it becomes a bundle signature with
# sealed resources and a bound Info.plist — after which editing ANYTHING
# inside, including the plist fixes this file exists to make, invalidates the
# seal and macOS kills the process at exec with no message on any stream.
#
# So signing happens exactly once, at the end of an assembly, and never again.
# Nothing after this function may touch the bundle. If a binary changes, run
# the bundler again — it is a directory copy, it costs seconds, and it is the
# only operation that leaves a correct bundle.
#
# Order is inside-out and explicit. `--deep` is deprecated by Apple and would
# not do the right thing here anyway: the five helpers need DIFFERENT
# entitlements from the app that contains them.
mac_sign_bundle() {
  local app="$1" identity="$2" ents_dir="${3:-}"
  local common=(--force --sign "$identity")

  # Hardened runtime and a secure timestamp are both notarization
  # requirements — and both are meaningless without a real identity, so they
  # are attached only to one.
  #
  # Adhoc + hardened runtime is not merely pointless, it is FATAL with CEF,
  # and it fails in the most confusing way available: library validation
  # restricts dlopen to code signed by the same Team ID, an adhoc signature
  # has no team, and so the app refuses to load its own framework with
  #   "mapping process and mapped file (non-platform) have different Team IDs"
  # before a line of our code runs. Measured 2026-08-21. The entitlements in
  # dist/macos/ do lift it, but a local adhoc rehearsal is not a notarization
  # rehearsal and pretending otherwise would only hide the difference.
  if [[ "$identity" != "-" ]]; then
    common+=(--timestamp --options runtime)
  fi

  # 1. The framework. Its own bundle, signed first because everything above it
  #    seals it.
  local fw="$app/Contents/Frameworks/Chromium Embedded Framework.framework"
  [[ -d "$fw" ]] && codesign "${common[@]}" "$fw"

  # 2. The helpers. JIT + unsigned executable memory: this is V8, and a
  #    hardened runtime without these entitlements is a renderer that dies the
  #    moment a page runs JavaScript.
  local helper_ents=""
  [[ -n "$ents_dir" && -f "$ents_dir/cef-helper.entitlements" ]] &&
    helper_ents="$ents_dir/cef-helper.entitlements"
  local h
  for h in "$app/Contents/Frameworks/"*.app; do
    [[ -d "$h" ]] || continue
    if [[ -n "$helper_ents" ]]; then
      codesign "${common[@]}" --entitlements "$helper_ents" "$h"
    else
      codesign "${common[@]}" "$h"
    fi
  done

  # 3. The app last, which seals everything below it.
  local app_ents=""
  [[ -n "$ents_dir" && -f "$ents_dir/cef-app.entitlements" ]] &&
    app_ents="$ents_dir/cef-app.entitlements"
  if [[ -n "$app_ents" ]]; then
    codesign "${common[@]}" --entitlements "$app_ents" "$app"
  else
    codesign "${common[@]}" "$app"
  fi
}
