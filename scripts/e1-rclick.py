#!/usr/bin/env python3
"""One RIGHT click at a screen point, posted through Quartz.

`as-click.py`'s twin, and it exists because Slice E1 is the first thing in
eave that cares about the right button at all — up to `a4b3983`,
`web/page.rs` forwarded LEFT and nothing else, so a right-click on a page
never reached Chromium.

Same two rules as the left-button version, and for the same reasons:
Quartz rather than System Events (System Events' `click at` does not reach
this app's titlebar; the HID tap does), and move-then-press (a press with
no prior move is delivered at the pointer's previous location by some
AppKit paths, which reads as "the click did nothing" from a screenshot).

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
            None, kind, point, Quartz.kCGMouseButtonRight
        )
        Quartz.CGEventPost(Quartz.kCGHIDEventTap, event)

    post(Quartz.kCGEventMouseMoved)
    time.sleep(0.15)
    post(Quartz.kCGEventRightMouseDown)
    time.sleep(0.08)
    post(Quartz.kCGEventRightMouseUp)


if __name__ == "__main__":
    main()
