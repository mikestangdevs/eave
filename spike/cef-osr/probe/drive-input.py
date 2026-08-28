#!/usr/bin/env python3
"""Drive the OSR input probe with REAL OS events.

Synthetic CGEvents go to the window server, then AppKit, then gpui, then our
translation layer, then CEF. Posting events straight into CefBrowserHost
would have skipped the two hops that actually carry the risk.
"""
import subprocess, sys, time
import Quartz

def window_bounds(owner="eave CEF OSR probe"):
    for w in Quartz.CGWindowListCopyWindowInfo(
        Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements,
        Quartz.kCGNullWindowID,
    ):
        if owner in (w.get("kCGWindowOwnerName") or ""):
            b = w["kCGWindowBounds"]
            if b["Width"] * b["Height"] > 100000:
                return int(b["X"]), int(b["Y"]), int(b["Width"]), int(b["Height"])
    return None

def post(ev):
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, ev)
    time.sleep(0.06)

def move(x, y):
    post(Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventMouseMoved, (x, y), 0))

def click(x, y, flags=0, count=1):
    move(x, y)
    for _ in range(count):
        d = Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDown, (x, y), 0)
        u = Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseUp, (x, y), 0)
        if flags:
            Quartz.CGEventSetFlags(d, flags)
            Quartz.CGEventSetFlags(u, flags)
        post(d); post(u)

def drag(x0, y0, x1, y1, steps=14):
    move(x0, y0)
    post(Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDown, (x0, y0), 0))
    for i in range(1, steps + 1):
        x = x0 + (x1 - x0) * i / steps
        y = y0 + (y1 - y0) * i / steps
        post(Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseDragged, (x, y), 0))
    post(Quartz.CGEventCreateMouseEvent(None, Quartz.kCGEventLeftMouseUp, (x1, y1), 0))

KEYMAP = {c: k for k, c in zip(
    [0,11,8,2,14,3,5,4,34,38,40,37,46,45,31,35,12,15,1,17,32,9,13,7,16,6],
    "abcdefghijklmnopqrstuvwxyz")}

def type_text(s):
    for ch in s:
        code = KEYMAP.get(ch.lower())
        if code is None:
            continue
        d = Quartz.CGEventCreateKeyboardEvent(None, code, True)
        u = Quartz.CGEventCreateKeyboardEvent(None, code, False)
        post(d); post(u)

def scroll(x, y, ticks, per=-8):
    move(x, y)
    for _ in range(ticks):
        post(Quartz.CGEventCreateScrollWheelEvent(None, Quartz.kCGScrollEventUnitPixel, 1, per))

def main():
    b = window_bounds()
    if not b:
        print("no probe window"); sys.exit(1)
    X, Y, W, H = b
    print(f"driving window at {X},{Y} {W}x{H}")
    # gpui's content view spans the whole window, title bar included, and the
    # page surface is laid out at its origin — so window point == page point,
    # with no title-bar offset. Coordinates below were read off a screenshot
    # of the running page rather than guessed.
    def at(px_, py_):
        return (X + px_, Y + py_)

    print(">> focus + click the text field")
    click(*at(165, 130))
    time.sleep(0.4)

    print(">> type into the form")
    type_text("hello")
    time.sleep(0.6)

    print(">> select text by dragging")
    drag(*at(240, 230), *at(360, 230))
    time.sleep(0.6)

    print(">> open the native <select>")
    click(*at(146, 177))
    time.sleep(1.2)
    print(">> pick the third option")
    click(*at(146, 177 + 62))
    time.sleep(0.8)

    print(">> cmd-click the link")
    click(*at(90, 278), flags=Quartz.kCGEventFlagMaskCommand)
    time.sleep(0.6)

    print(">> scroll")
    scroll(*at(600, 400), 30)
    time.sleep(1.0)
    print("done")

main()
