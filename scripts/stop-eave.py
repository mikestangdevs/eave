#!/usr/bin/env python3
"""Stop every eave UI, engine, and CEF helper this machine is running.

eave is one binary in two roles. The process that owns the window is
rarely the one a name search returns — that is usually `--engine`, which
outlives the window on purpose and will make the *next* UI attach to an
old product. Kill both, from every checkout and from /Applications, so
the bundle we launch next is the only eave on screen.

Does not match Cursor helpers or this script. Identifies victims by the
path of the binary, never by the substring "eave" alone.
"""

from __future__ import annotations

import os
import signal
import subprocess
import time


def victims() -> list[int]:
    out = subprocess.check_output(["ps", "-ax", "-o", "pid=,command="], text=True)
    found: list[int] = []
    me = os.getpid()
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        pid_s, _, cmd = line.partition(" ")
        try:
            pid = int(pid_s)
        except ValueError:
            continue
        if pid == me:
            continue
        if "Cursor Helper" in cmd:
            continue
        # Product bundle (UI, engine spawned from it, CEF helpers).
        if "/eave.app/Contents/" in cmd:
            found.append(pid)
            continue
        # Loose cargo output from any worktree — the thing that is not
        # a browser, and the thing last session launched by mistake.
        if "/target/debug/eave" in cmd or "/target/release/eave" in cmd:
            found.append(pid)
    return found


def main() -> None:
    pids = victims()
    for pid in pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.time() + 2.5
    while time.time() < deadline:
        if not victims():
            break
        time.sleep(0.08)
    for pid in victims():
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    data = os.path.join(
        os.path.expanduser("~"), "Library", "Application Support", "eave"
    )
    for name in ("engine.sock", "engine.lock"):
        try:
            os.remove(os.path.join(data, name))
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    main()
