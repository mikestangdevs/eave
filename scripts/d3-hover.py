#!/usr/bin/env python3
"""Park the pointer at a SCREEN point, with no click.

Separate from `as-click.py` because the thing being photographed here is the
POINTER, not a result: the rail's seam is a hairline with no grip drawn on
it, so the only thing that says it can be dragged is the cursor changing
over it. A click would move the pointer and then also do something.

Run with the PATH `python3` — `/usr/bin/python3` has no Quartz module.
"""
import sys
import time

import Quartz


def main() -> None:
    x, y = float(sys.argv[1]), float(sys.argv[2])
    Quartz.CGEventPost(
        Quartz.kCGHIDEventTap,
        Quartz.CGEventCreateMouseEvent(
            None,
            Quartz.kCGEventMouseMoved,
            Quartz.CGPointMake(x, y),
            Quartz.kCGMouseButtonLeft,
        ),
    )
    time.sleep(0.5)


if __name__ == "__main__":
    main()
