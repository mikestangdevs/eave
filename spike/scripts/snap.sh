#!/bin/bash
# Capture the frontmost window of a named process, by window id.
#
# `screencapture` on this machine hangs intermittently: region capture (-R)
# every time, window capture (-l) occasionally — and killing a hung one wedges
# `replayd`, which breaks every later shot. So: bounded, retried, terminated
# politely, and a half-written temp file is promoted rather than discarded.
# (Carried over verbatim from the webview spike's rig, which learned this the
# expensive way.)
set -u
PROC="$1"; OUT="$2"

read -r WINID WX WY WW WH <<<"$(python3 - "$PROC" <<'PY'
import sys, Quartz
name = sys.argv[1]
best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID):
    if name not in (w.get("kCGWindowOwnerName") or ""):
        continue
    b = w["kCGWindowBounds"]
    area = b["Width"] * b["Height"]
    if area < 10000:
        continue
    if best is None or area > best[1]:
        best = (w["kCGWindowNumber"], area, b)
if best:
    _, _, b = best
    print(best[0], int(b["X"]), int(b["Y"]), int(b["Width"]), int(b["Height"]))
PY
)"
if [ -z "${WINID:-}" ]; then echo "no window found for $PROC"; exit 1; fi
echo "window $WINID at ${WX},${WY} ${WW}x${WH}"

tries=0
while [ $tries -lt 3 ]; do
  tries=$((tries + 1))
  rm -f "$OUT" "$(dirname "$OUT")/.$(basename "$OUT")"
  screencapture -x -o -l "$WINID" "$OUT" &
  sc=$!
  ( sleep 12; kill -TERM $sc 2>/dev/null ) & guard=$!
  wait $sc 2>/dev/null || true
  kill "$guard" 2>/dev/null || true
  if [ -s "$OUT" ]; then echo "captured $OUT ($(stat -f%z "$OUT") bytes)"; exit 0; fi
  tmp="$(dirname "$OUT")/.$(basename "$OUT")"
  if [ -s "$tmp" ]; then mv "$tmp" "$OUT"; echo "promoted temp file -> $OUT"; exit 0; fi
  echo "attempt $tries produced nothing, retrying"; sleep 2
done
echo "FAILED to capture $OUT"; exit 1
