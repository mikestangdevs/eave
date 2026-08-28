#!/usr/bin/env bash
# Slice E3's re-run of the C7 sweep, aimed at the call site E3 added.
#
#     scripts/e3-c7-sweep.sh [runs]
#
# C7's finding: a foreign event loop is lent every borrow you hold, and a
# SIGTERM with a page open aborted 48/48 because Chromium answered the signal
# from inside a pump that held gpui's borrow. Its sweep was a scratchpad
# script that no longer exists (`/tmp/c7-sweep.sh`, named in
# stage-c7-quit.md); this is the same measurement, rebuilt, and pointed at
# the thing E3 changed.
#
# **What is new here is the interleaving, not the signal.** E3 adds a third
# caller of `close_browser` — the eviction clock, on the pump — so the case
# worth hammering is a SIGTERM that lands WHILE an eviction is in flight.
# The threshold is scaled to 2 s and the signal is sent at a spread of
# delays across that window on purpose: a fixed delay would test one phase
# of the cycle repeatedly and call it a sweep.
#
# An abort is exit 134 (SIGABRT). A hang is the timeout firing. Both are
# failures; exit 0 is the pass.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/target/bundle/eave.app/Contents/MacOS/eave"
RUNS="${1:-20}"

[[ -x "$APP" ]] || { echo "no bundle — scripts/bundle-app.sh --features web" >&2; exit 1; }

pass=0; abort=0; hang=0; other=0; skipped=0
for i in $(seq 1 "$RUNS"); do
  DATA="/tmp/e3sw-$$-$i"
  rm -rf "$DATA"; mkdir -p "$DATA/browser"
  python3 - "$DATA" <<'PY'
import json, os, sys, time
data = sys.argv[1]
now = int(time.time() * 1000)
events = [
    {"kind": "opened", "id": "t0", "index": 0, "ts_ms": now},
    {"kind": "navigated", "id": "t0", "url": "about:blank", "title": "a", "ts_ms": now},
    {"kind": "opened", "id": "t1", "index": 1, "ts_ms": now},
    {"kind": "navigated", "id": "t1", "url": "about:blank", "title": "b", "ts_ms": now},
    {"kind": "activated", "id": "t0"},
]
with open(os.path.join(data, "browser", "tabs.jsonl"), "w") as f:
    f.write(json.dumps({"format_version": 1, "kind": "tabs", "created_ts_ms": now}) + "\n")
    for n, ev in enumerate(events, start=1):
        f.write(json.dumps({"seq": n, "ts_ms": now + n, "event": ev}) + "\n")
PY
  EAVE_DATA_DIR="$DATA" EAVE_THRESH_PAGE_EVICT_MS=2000 EAVE_WEB_TRACE=1 \
    "$APP" > "$DATA/app.log" 2>&1 &
  pid=$!

  # **Make an eviction actually happen.** A browser exists only for the tab
  # that has been activated, so a seed alone leaves t1 with nothing to evict
  # and t0 active and therefore exempt — the sweep's first version measured
  # 20 runs of zero evictions and would have reported a pass for a call site
  # it never reached. Activating t1 and then returning to t0 leaves t1 live
  # and idle, which is the only state the eviction clock acts on.
  python3 "$ROOT/scripts/e3-activate.py" "$DATA" t1 t0 >/dev/null 2>&1

  # **Wait for the app to be genuinely up before starting the clock.** The
  # engine is a separate process and opens its socket first, so "the socket
  # exists" says nothing about whether CEF has initialised. Two runs of the
  # previous version signalled an app that was still starting, got exit 143
  # from the default SIGTERM handler (eave installs its own only AFTER
  # `CefInitialize`, deliberately — see `watch_for_signals`), and recorded a
  # failure that had nothing to do with eviction. A run that never reached
  # the code under test is not a data point about it.
  up=0
  for _ in $(seq 1 200); do
    if grep -q "browser created for t1" "$DATA/app.log" 2>/dev/null; then up=1; break; fi
    sleep 0.1
  done
  if [[ $up -eq 0 ]]; then
    kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    echo "run $i  SKIPPED — app never created a browser"
    skipped=$((skipped + 1)); rm -rf "$DATA"; continue
  fi

  # Spread the signal across the eviction cycle: 1.0 s .. 4.75 s.
  delay=$(python3 -c "print(1.0 + ($i % 16) * 0.25)")
  sleep "$delay"
  kill -TERM "$pid" 2>/dev/null

  waited=0
  while kill -0 "$pid" 2>/dev/null && [[ $waited -lt 200 ]]; do
    sleep 0.1; waited=$((waited + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    hang=$((hang + 1)); code="HANG"
  else
    wait "$pid" 2>/dev/null; code=$?
    case "$code" in
      0) pass=$((pass + 1)) ;;
      134) abort=$((abort + 1)) ;;
      *) other=$((other + 1)) ;;
    esac
  fi
  ev=$(grep -c "evicting idle tab" "$DATA/app.log" 2>/dev/null); ev=${ev:-0}
  printf "run %2d  signal@%-5s exit=%-4s evictions=%s\n" "$i" "$delay" "$code" "$ev"
  # An exit that is neither clean nor a known abort is the interesting one;
  # keep its log rather than deleting the evidence.
  if [[ "$code" == "0" ]]; then rm -rf "$DATA"; else echo "  kept $DATA/app.log"; fi
done

echo
echo "runs=$RUNS  clean=$pass  abort=$abort  hang=$hang  other=$other  skipped=$skipped"
[[ $abort -eq 0 && $hang -eq 0 && $other -eq 0 ]] || exit 1
echo "C7 SWEEP PASS"
