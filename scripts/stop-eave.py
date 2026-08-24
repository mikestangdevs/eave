#!/usr/bin/env python3
"""Stop eave UI, engine, and CEF helpers.

eave is one binary in two roles. The process that owns the window is
rarely the one a name search returns — that is usually `--engine`, which
outlives the window on purpose and will make the *next* UI attach to an
old product. Kill both.

`--bundle PATH` kills only that `.app` (what `dev.sh` / `run-app.sh`
want). With no flag it still hunts every `/eave.app/Contents/` and every
loose `target/debug/eave`, so a leftover from another checkout cannot
own the window.

Does not match Cursor helpers or this script. Identifies victims by the
path of the binary, never by the substring "eave" alone.

Uses `pgrep` + `ps -p`, never `ps -ax`. Whole-process-table `ps` never
returns on this host (CloudDocs).
"""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import time


def pgrep(*args: str) -> list[int]:
    try:
        out = subprocess.check_output(["pgrep", *args], text=True)
    except subprocess.CalledProcessError as e:
        if e.returncode == 1:
            return []
        raise
    return [int(x) for x in out.split() if x.isdigit()]


def cmdline(pid: int) -> str | None:
    try:
        out = subprocess.check_output(
            ["ps", "-p", str(pid), "-o", "command="], text=True
        )
    except subprocess.CalledProcessError:
        return None
    cmd = out.strip()
    return cmd or None


def victims(bundle: str | None) -> list[int]:
    cand: set[int] = set()
    cand.update(pgrep("-x", "eave"))
    cand.update(pgrep("-f", "/eave.app/Contents/"))
    cand.update(pgrep("-f", "/target/debug/eave"))
    cand.update(pgrep("-f", "/target/release/eave"))
    needle = None
    if bundle:
        needle = os.path.realpath(bundle).rstrip("/")
    me = os.getpid()
    found: list[int] = []
    for pid in cand:
        if pid == me:
            continue
        cmd = cmdline(pid)
        if not cmd or "Cursor Helper" in cmd:
            continue
        if needle is not None:
            if needle in cmd:
                found.append(pid)
            continue
        if "/eave.app/Contents/" in cmd:
            found.append(pid)
            continue
        if "/target/debug/eave" in cmd or "/target/release/eave" in cmd:
            found.append(pid)
    return found


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--bundle",
        help="only kill processes of this .app (realpath-matched)",
    )
    args = parser.parse_args()
    pids = victims(args.bundle)
    for pid in pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.time() + 2.5
    while time.time() < deadline:
        if not victims(args.bundle):
            break
        time.sleep(0.08)
    for pid in victims(args.bundle):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if args.bundle:
        return
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
