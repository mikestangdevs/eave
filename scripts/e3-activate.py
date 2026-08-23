#!/usr/bin/env python3
"""Activate tabs over the engine's own wire.

    e3-activate.py <data-dir> <tab-id> [tab-id ...]

Exists because a browser is only created for the ACTIVE tab (C5's rule), so
anything that wants N live browsers has to activate N tabs — and the strip
cannot be clicked (`docs/progress.md`: the app draws no tabs at all). This
sends the same `tab_activate` request the strip would.
"""
import json
import os
import socket
import sys
import time


def main():
    data, tabs = sys.argv[1], sys.argv[2:]
    path = os.path.join(data, "engine.sock")
    for _ in range(300):
        if os.path.exists(path):
            break
        time.sleep(0.05)
    else:
        sys.exit("no engine socket")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    f = s.makefile("rwb")
    n = 0

    def call(method, params):
        nonlocal n
        n += 1
        f.write((json.dumps({"id": n, "method": method, "params": params}) + "\n").encode())
        f.flush()
        while True:
            line = f.readline()
            if not line:
                sys.exit("engine closed")
            msg = json.loads(line)
            if msg.get("id") == n:
                return msg

    call("hello", {"proto": 1})
    for tab in tabs:
        call("tab_activate", {"tab_id": tab})
        # Long enough for the browser to be created and to start loading;
        # the point is that it EXISTS, not that it finished.
        time.sleep(1.2)


if __name__ == "__main__":
    main()
