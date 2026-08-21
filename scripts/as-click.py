#!/usr/bin/env python3
"""One left click at a SCREEN point, posted through Quartz.

System Events' `click at` reaches this app's rails and does not reach its
titlebar: six runs that clicked the rail toggle produced six byte-identical
screenshots, while the same call 100 px lower selected a session every time.
`CGEventPost` works at both, and it is what docs/progress.md §Traps already
recommends for buttons.

The move-then-press ordering is deliberate. A press posted with no prior
move is delivered at the pointer's previous location by some AppKit paths,
which reads as "the click did nothing" from a screenshot.

Run with the PATH `python3` — `/usr/bin/python3` has no Quartz module.
"""
import sys
import time

import Quartz


def main() -> None:
    x, y = float(sys.argv[1]), float(sys.argv[2])
    point = Quartz.CGPointMake(x, y)

    def post(kind):
        event = Quartz.CGEventCreateMouseEvent(
            None, kind, point, Quartz.kCGMouseButtonLeft
        )
        Quartz.CGEventPost(Quartz.kCGHIDEventTap, event)

    post(Quartz.kCGEventMouseMoved)
    time.sleep(0.15)
    post(Quartz.kCGEventLeftMouseDown)
    time.sleep(0.08)
    post(Quartz.kCGEventLeftMouseUp)


if __name__ == "__main__":
    main()
