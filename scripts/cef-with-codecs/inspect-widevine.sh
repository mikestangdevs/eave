#!/usr/bin/env bash
# Why WidevineCdm/ is empty on this embed, measured rather than guessed.
#
#     scripts/cef-with-codecs/inspect-widevine.sh /tmp/eave-br-xxxx
#
# The CDM is not in the CEF tarball (Google does not allow distributing
# it). enable_widevine=true is a REQUIRED CEF GN arg, so the public
# Spotify 151.3.18 already compiles the installer in. An empty directory
# at <data-dir>/browser/chromium/WidevineCdm is therefore a runtime miss,
# not a build exclusion.
#
# Chromium's Widevine component id (desktop): oimompecagnajdejgnnjijobebaeigek
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <eave --data-dir>" >&2
  exit 2
fi
DATA="$1"
CACHE="$DATA/browser/chromium"
echo "data-dir: $DATA"
echo "chromium cache: $CACHE"

if [[ ! -d "$CACHE" ]]; then
  echo "no chromium cache — this data-dir was never used by a --features web boot"
  exit 1
fi

echo
echo "── WidevineCdm/ ──"
if [[ -e "$CACHE/WidevineCdm" ]]; then
  ls -la "$CACHE/WidevineCdm" || true
  find "$CACHE/WidevineCdm" -maxdepth 3 \( -type f -o -type d \) | head -50
else
  echo "(absent)"
fi

echo
echo "── component_crx_cache / Component Updater ──"
find "$CACHE" -maxdepth 3 \( -iname '*component*' -o -iname '*widevine*' -o -iname '*cdm*' \) 2>/dev/null | head -80

echo
echo "── Last-Known-Good / Local State hints ──"
for f in "$CACHE/Local State" "$CACHE/Default/Preferences"; do
  if [[ -f "$f" ]]; then
    echo "# $f"
    python3 - "$f" <<'PY' || true
import json, sys
p = sys.argv[1]
try:
    d = json.load(open(p))
except Exception as e:
    print("  unreadable:", e)
    sys.exit(0)
def walk(obj, prefix=""):
    if isinstance(obj, dict):
        for k, v in obj.items():
            lk = str(k).lower()
            if any(s in lk for s in ("widevine", "cdm", "component")):
                print(f"  {prefix}{k}: {v!r}"[:240])
            walk(v, prefix + str(k) + ".")
walk(d)
PY
  fi
done

echo
echo "What this can tell you:"
echo "  empty WidevineCdm/ + no versioned subdir  → the component never landed"
echo "  versioned subdir present, Bitmovin still No DRM → CDM loaded but VMP/host verification refused it"
echo "  nothing at all → installer never ran (too-short session, or no on-demand Update())"
echo
echo "To *enable the fetch* after a codecs swap (product code, not this script):"
echo "  CefComponentUpdater::GetComponentUpdater()->Update("
echo "      \"oimompecagnajdejgnnjijobebaeigek\","
echo "      CEF_COMPONENT_UPDATE_PRIORITY_FOREGROUND, callback)"
echo "  on the UI thread after CefInitialize. Background delay is 600–900s;"
echo "  a BR-AUDIT-length probe will not wait it out."
echo
echo "Scope: 'CDM present and loads on the Bitmovin demo', not Netflix."
echo "Netflix-class playback also gates on VMP signing (a Google agreement)."
