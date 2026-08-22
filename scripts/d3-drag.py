#!/usr/bin/env python3
"""Drag the rail's seam, in two halves, so a screenshot can be taken mid-drag.

`grab` posts a move + left-down and EXITS WITH THE BUTTON STILL DOWN; `drop`
posts the final move and the up. Between the two, the caller can raise the
window and run `screencapture` — which is the whole point, because a resize
divider looks like nothing at rest and the only interesting frame is the one
where it is being held.

**A dragged button is process-wide state, so `drop` must always run.** The
caller traps EXIT; this file is deliberately dumb about that.

The move ordering is `as-click.py`'s and for its reason: a press with no
prior move is delivered at the pointer's previous location by some AppKit
paths, which reads as "the drag did nothing" from a screenshot.

`drag` moves in STEPS rather than one jump, and slowly. gpui starts a drag
from a mouse-down plus movement, and a single 60 px teleport can be coalesced
into one event the drag machinery sees after it has already decided the
gesture was a click.

**The step delay is a measured floor, not a guess.** At 0.03 s the app
received four moves out of twelve and stalled 8 px in — and it received them
at *fractional* x (234.42188, 235.17188, 235.46094, 235.75) when this script
only ever posts integers, which is the tell: those are not the events posted,
they are the window server's own interpolation of a gesture it decided to
throttle. One capture in three failed that way. 0.09 s over 16 steps lands
every move.

Run with the PATH `python3` — `/usr/bin/python3` has no Quartz module.
"""
import sys
import time

import Quartz


def post(kind, x, y):
    event = Quartz.CGEventCreateMouseEvent(
        None, kind, Quartz.CGPointMake(float(x), float(y)), Quartz.kCGMouseButtonLeft
    )
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, event)


def main() -> None:
    mode = sys.argv[1]
    x, y = float(sys.argv[2]), float(sys.argv[3])
    if mode == "grab":
        post(Quartz.kCGEventMouseMoved, x, y)
        time.sleep(0.15)
        post(Quartz.kCGEventLeftMouseDown, x, y)
        time.sleep(0.10)
    elif mode == "drag":
        to = float(sys.argv[4])
        steps = 16
        for i in range(1, steps + 1):
            post(Quartz.kCGEventLeftMouseDragged, x + (to - x) * i / steps, y)
            time.sleep(0.09)
    elif mode == "drop":
        post(Quartz.kCGEventLeftMouseDragged, x, y)
        time.sleep(0.08)
        post(Quartz.kCGEventLeftMouseUp, x, y)
    else:
        raise SystemExit(f"unknown mode {mode!r}")


if __name__ == "__main__":
    main()
