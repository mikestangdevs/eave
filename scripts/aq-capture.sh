#!/usr/bin/env bash
# Slice AQ — photograph the three surfaces this slice owns: the approval
# card's approve/deny pair, the popover, and the composer.
#
# The hermetic harness can assert that a button is on screen and what it
# emits. It cannot answer "does this look like the same app as the
# launcher", because that is a question about composited pixels — real
# font metrics, the frost sampling a real desktop, and a colour space.
#
# TWO STATES, TWO ROUTES, and neither of them clicks or types (progress.md:
# synthetic mouse-moved produces no hover fade and synthetic keyboard events
# do not reach this gpui app at all, so a capture that depended on either
# would be photographing a failure):
#
#   * the approval card + composer — `EAVE_REPLAY=<fixture>`, the wire-tap
#     replay path. A hand-written fixture is the only way to put a PENDING
#     permission question in front of a real compositor without driving a
#     real agent: `EAVE_LAUNCH_AT_BOOT` opens a session but sends nothing,
#     and the fake agent's `approval-hang` mode needs a prompt typed at it.
#   * the popover — `EAVE_OVERLAY_DEMO=open`, which exists precisely
#     because the only way into a menu is a click.
#
# Mechanics borrowed from scripts/shot.sh, and each one cost an earlier
# slice a wrong conclusion:
#   * never `pgrep` for the window's owner — eave is one binary in two
#     roles and the newest match is the ENGINE, which owns no window. The
#     pid comes from our own `$!`.
#   * capture BY WINDOW ID, so occlusion cannot photograph the wrong thing.
#   * poll Quartz for the window instead of sleeping at it.
#   * BUILD, do not merely check: `cargo test` leaves target/debug/eave at
#     whatever the last build was.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
PREFIX="${1:-aq}"
mkdir -p "$SHOTS"

echo "building…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

WORK="$(mktemp -d)"
FIXTURE="$WORK/approval.tap.jsonl"
SID="aq00aq00-0000-4000-8000-000000000000"

# A journal that ends on an OPEN permission question. Shapes come from
# `eave_core::journal::Record` / `SessionEvent`; the wire envelope is
# spec 05's loose response with a subscription item in it.
python3 - "$FIXTURE" "$SID" <<'PY'
import json, sys
path, sid = sys.argv[1], sys.argv[2]
T0 = 1786890000000
def frame(ts, item):
    return f'{ts}\t' + json.dumps({"id": 100, "item": item})
lines = [
    frame(T0, {"type": "reset", "session_id": sid, "epoch": 1, "header": {
        "format_version": 1, "session_id": sid, "provider": "claude-code",
        "cwd": "/Users/you/Desktop/eave/eave", "created_ts_ms": T0}}),
]
def ev(seq, event):
    return {"seq": seq, "ts_ms": T0 + seq * 10, "event": event}
records = [
    ev(1, {"kind": "user_sent", "text":
           "Tidy up the scratch files under target/ and drop the old build log."}),
    ev(2, {"kind": "user_delivered"}),
    ev(3, {"kind": "provider", "event": {"kind": "init", "session_id": sid,
                                         "permission_mode": "default"}}),
    ev(4, {"kind": "provider", "event": {"kind": "assistant_text", "text":
           "I'll clear the scratch files. The build log is the only one that "
           "isn't regenerated, so I'll ask before removing it."}}),
    ev(5, {"kind": "provider", "event": {"kind": "tool_use_started",
                                         "id": "toolu_1", "name": "Bash",
                                         "input": "{\"command\":\"ls target/scratch\"}"}}),
    ev(6, {"kind": "provider", "event": {"kind": "tool_use_finished",
                                         "id": "toolu_1",
                                         "result": "build.log\nold.tmp\n",
                                         "is_error": False}}),
    ev(7, {"kind": "provider", "event": {
        "kind": "approval_requested",
        "request_id": "req-aq1",
        "tool": "Bash",
        "display_name": "Bash",
        "description": "removes target/scratch/build.log",
        "path": "target/scratch/build.log",
        "input_json": "{\"command\":\"rm -f target/scratch/build.log\","
                      "\"description\":\"remove the stale build log\"}"}}),
]
lines.append(frame(T0 + 200, {"type": "events", "session_id": sid,
                              "epoch": 1, "records": records}))
lines.append(frame(T0 + 400, {"type": "attention", "session_id": sid,
                              "attention": {"state": "awaiting_approval",
                                            "since_ms": T0, "pending": 1,
                                            "tool": "Bash"}}))
open(path, "w").write("\n".join(lines) + "\n")
print(f"  fixture: {len(lines)} frames -> {path}")
PY

shoot() {                                  # shoot <name> <env...>
  local name="$1"; shift
  local pid win winid
  echo "--- $name"
  env "$@" EAVE_REDUCE_MOTION=0 EAVE_DOGFOOD_LOG="$WORK/dogfood.md" \
      EAVE_DATA_DIR="$WORK/data-$name" "$BIN" >/dev/null 2>&1 &
  pid=$!
  echo "  app pid $pid (ours, by \$! — never by pgrep)"
  win=""
  for _ in $(seq 1 45); do
    win="$(python3 - "$pid" <<'PY' || true
import sys, subprocess, Quartz
root = int(sys.argv[1]); pids = {root}
try:
    out = subprocess.run(["pgrep", "-P", str(root)], capture_output=True, text=True).stdout
    pids |= {int(x) for x in out.split()}
except Exception:
    pass
best = None
for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID):
    if w.get("kCGWindowOwnerPID") not in pids:
        continue
    b = w["kCGWindowBounds"]
    if b["Width"] < 200 or b["Height"] < 200:
        continue
    a = b["Width"] * b["Height"]
    if best is None or a > best[1]:
        best = (w["kCGWindowNumber"], a)
if best:
    print(best[0])
PY
)"
    [ -n "$win" ] && break
    sleep 1
  done
  if [ -z "$win" ]; then
    echo "  no window from pid $pid" >&2; kill "$pid" 2>/dev/null || true; return 1
  fi
  winid="$win"
  osascript -e "tell application \"System Events\" to set frontmost of \
    (first process whose unix id is $pid) to true" 2>/dev/null || true
  sleep 5
  screencapture -x -o -l "$winid" "$SHOTS/$name.png"
  echo "  captured $SHOTS/$name.png ($(stat -f%z "$SHOTS/$name.png") bytes)"
  # Reap and VERIFY (Slice AE's rule): kill returning 0 says the signal was
  # delivered and nothing about whether the process went away. The engine is
  # the APP's child, so it is matched by the throwaway data dir it carries.
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  pkill -f "$WORK/data-$name" 2>/dev/null || true
  sleep 1
  pgrep -lf "$WORK/data-$name" >&2 && echo "  LEAK above" >&2 || true
}

shoot "$PREFIX-approval" EAVE_REPLAY="$FIXTURE" EAVE_REPLAY_SPEED=1
shoot "$PREFIX-menu" EAVE_OVERLAY_DEMO=open
echo "done"
