#!/usr/bin/env bash
# Slice AR - photograph the `/` palette.
# Slice AR2 - and the rows that come out of the CLI's own inventory.
#
# Run it as `scripts/ar-capture.sh ar2` for the AR2 shots. The only change
# AR2 made is to the fixture's `init` record, which now carries the
# `slash_commands` array the real CLI sends - which is what makes the shot
# show `/context` and `/compact`, rows no filesystem scan can ever produce.
# Not a fifth copy of these twenty lines of Quartz plumbing: AR's report
# already counted four re-derivations of them and a fifth would be worse
# than an edit.
#
# The palette's only door is the keyboard, and progress.md records that
# synthetic keyboard events do not reach this gpui app at all (gpui derives
# `key_char` from the NSEvent's `characters`, which a synthesised event has
# none of). So a capture that tried to TYPE a slash would be photographing a
# failure. `EAVE_SLASH_DEMO=1` seeds the composer's text at boot and runs the
# same `sync_slash_palette` a keystroke runs - the same shape, and the same
# reason, as P1's `EAVE_OVERLAY_DEMO` and Q2's `EAVE_LAUNCH_AT_BOOT`.
#
# Two more things this needs that the other captures do not:
#   * a SESSION, because the palette is keyed to a session's cwd. The
#     wire-tap replay path provides one.
#   * that session's cwd pointing at a real DIRECTORY, because discovery
#     reads `<cwd>/.claude/skills`. The replay rig's synthetic rail row
#     normally carries the fixture FILE's path, so `EAVE_REPLAY_CWD`
#     overrides it.
#
# The user-scope and plugin rows in the shot are the operator's REAL
# `~/.claude` - this is the real discovery path, not a mock.
#
# Mechanics borrowed from scripts/shot.sh and scripts/aq-capture.sh, and each
# one cost an earlier slice a wrong conclusion:
#   * never `pgrep` for the window's owner - eave is one binary in two roles
#     and the newest match is the ENGINE, which owns no window. The pid comes
#     from our own `$!`.
#   * capture BY WINDOW ID, so occlusion cannot photograph the wrong thing.
#   * poll Quartz for the window instead of sleeping at it.
#   * BUILD, do not merely check.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
SHOTS="${EAVE_SHOTS:-$ROOT/../docs/shots}"
PREFIX="${1:-ar}"
mkdir -p "$SHOTS"

echo "building…"
cargo build --quiet
BIN="$ROOT/target/debug/eave"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

WORK="$(mktemp -d)"
FIXTURE="$WORK/session.tap.jsonl"
SID="ar00ar00-0000-4000-8000-000000000000"
PROJ="$WORK/project"

# A project with a skill and a command of its own, so the shot shows the
# PROJECT scope leading the list. Every other row in the picture comes from
# the operator's real ~/.claude - user skills and plugin skills - because
# this is the real discovery path and not a mock.
mkdir -p "$PROJ/.claude/skills/ship-a-slice" "$PROJ/.claude/commands"
cat > "$PROJ/.claude/skills/ship-a-slice/SKILL.md" <<'SK'
---
name: ship-a-slice
description: Cut a slice branch, run the full suite unpiped, and write the report caveats-first.
---
body
SK
cat > "$PROJ/.claude/commands/proof.md" <<'SK'
---
description: Re-measure every ink against the surface it is actually drawn on.
---
body
SK

# A SETTLED journal - the palette is what this shot is about, so nothing
# else in the window should be mid-flight competing with it.
BARE="$WORK/session-bare.tap.jsonl"
python3 - "$FIXTURE" "$SID" "$PROJ" "$BARE" <<'PY'
import json, sys
path, sid, proj, bare = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
T0 = 1786890000000
def frame(ts, item):
    return str(ts) + "\t" + json.dumps({"id": 100, "item": item})
lines = [
    frame(T0, {"type": "reset", "session_id": sid, "epoch": 1, "header": {
        "format_version": 1, "session_id": sid, "provider": "claude-code",
        "cwd": proj, "created_ts_ms": T0}}),
]
def ev(seq, event):
    return {"seq": seq, "ts_ms": T0 + seq * 10, "event": event}
records = [
    ev(1, {"kind": "user_sent", "text": "What is left before this slice can ship?"}),
    ev(2, {"kind": "user_delivered"}),
    # Slice AR2: the CLI announces its own complete command list in this
    # frame, and the palette now takes its NAMES from here. The mix is the
    # shape of a real one - the project's two, a real user skill, a real
    # plugin skill, and the built-ins that exist in no file anywhere. And
    # `clear`, which the CLI really does announce and the palette really
    # does refuse (verified inert in -p mode), so the shot shows the
    # carve-out by its absence.
    ev(3, {"kind": "provider", "event": {"kind": "init", "session_id": sid,
                                         "permission_mode": "default",
                                         "slash_commands": [
                                             "ship-a-slice", "proof",
                                             "context", "compact", "model",
                                             "clear", "config", "doctor"]}}),
    ev(4, {"kind": "provider", "event": {"kind": "assistant_text", "text":
           "The suite is green and both gates are clean. What is left is the "
           "report, and one screenshot of the palette itself."}}),
]
# The init record plays at T0, WITH the reset, and the rest 200ms later.
# Slice AR2 made that ordering load-bearing rather than cosmetic: the
# palette takes its names from the inventory in that record, and
# `EAVE_SLASH_DEMO` seeds the composer as soon as a session exists. Leaving
# init in the later batch is a race the camera can lose, and losing it
# photographs the fallback while claiming to photograph the feature.
def emit(target, recs):
    out = lines + [
        frame(T0, {"type": "events", "session_id": sid,
                   "epoch": 1, "records": recs[:3]}),
        frame(T0 + 200, {"type": "events", "session_id": sid,
                         "epoch": 1, "records": recs[3:]}),
    ]
    open(target, "w").write("\n".join(out) + "\n")
    print("  fixture: " + str(len(out)) + " frames -> " + target)

emit(path, records)
# The unknown-provider shot needs a session whose CLI announced NOTHING.
# With an inventory in the frame the palette has names to offer and the
# "eave does not know where <provider> keeps its skills" sentence is no
# longer true - which is correct behaviour and the wrong photograph.
quiet = [dict(r) for r in records]
quiet[2] = ev(3, {"kind": "provider", "event": {"kind": "init",
                                                "session_id": sid,
                                                "permission_mode": "default"}})
emit(bare, quiet)
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

shoot "$PREFIX-palette" EAVE_REPLAY="$FIXTURE" EAVE_REPLAY_SPEED=1 \
      EAVE_REPLAY_CWD="$PROJ" EAVE_REPLAY_PROVIDER=claude-code \
      EAVE_SLASH_DEMO=1
# The same window with nothing typed, so the palette can be compared
# with the composer it grows out of.
shoot "$PREFIX-composer" EAVE_REPLAY="$FIXTURE" EAVE_REPLAY_SPEED=1 \
      EAVE_REPLAY_CWD="$PROJ" EAVE_REPLAY_PROVIDER=claude-code
# The state the FIRST capture found by accident and that is now a distinct
# sentence: a provider eave has no roots table for. Worth photographing on
# purpose, because "you have none" and "I do not know where to look" are
# different facts and the palette used to say the first for both.
shoot "$PREFIX-unknown-provider" EAVE_REPLAY="$BARE" EAVE_REPLAY_SPEED=1 \
      EAVE_REPLAY_CWD="$PROJ" EAVE_REPLAY_PROVIDER=hermes EAVE_SLASH_DEMO=1
echo "done"
