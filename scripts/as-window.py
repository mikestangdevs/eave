#!/usr/bin/env python3
"""Print `id x y w h` for the largest on-screen window owned by a pid or its
children, or nothing.

Never `pgrep`: eave is one binary in two roles and the newest match is
normally the ENGINE, which owns no window. The caller passes the pid it got
from `$!`; this walks that pid's children too, in case the window ends up
owned by something we spawned.
"""
import subprocess
import sys

import Quartz


def main() -> None:
    root = int(sys.argv[1])
    pids = {root}
    try:
        out = subprocess.run(
            ["pgrep", "-P", str(root)], capture_output=True, text=True
        ).stdout
        pids |= {int(x) for x in out.split()}
    except Exception:
        pass
    best = None
    for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID
    ):
        if w.get("kCGWindowOwnerPID") not in pids:
            continue
        b = w["kCGWindowBounds"]
        # Skip shadow and helper surfaces; a real app window is not 40px tall.
        if b["Width"] < 200 or b["Height"] < 200:
            continue
        area = b["Width"] * b["Height"]
        if best is None or area > best[1]:
            best = (w["kCGWindowNumber"], area, b)
    if best:
        b = best[2]
        print("%d %d %d %d %d" % (best[0], b["X"], b["Y"], b["Width"], b["Height"]))


if __name__ == "__main__":
    main()
