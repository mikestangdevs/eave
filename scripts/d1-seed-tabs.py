#!/usr/bin/env python3
"""Seed B1's tab journal so the OPEN section has something in it.

    d1-seed-tabs.py <data-dir>

`as-seed.py` writes sessions and nothing else, which is why Slice D1's first
capture came back with an `OPEN` header and an empty list under it: a
truthful photograph of a rail with no tabs, and a useless one for showing a
strip. The strip is a projection of `browser/tabs.jsonl`, so seeding that
file is the most direct statement of what the shot is of — the same
reasoning `c5-seed.py` records.

Two page tabs and two session tabs, interleaved, because the one thing this
frame has to show is that **both kinds sit in one list in one ordering**. The
page tabs carry a committed `navigated` event each, which is what gives a
restored tab a title to draw; `load` is deliberately absent, being transient
and never journaled.

The ACTIVE tab is a session, so the pane header draws its breadcrumb. A
capture whose active tab were a page would photograph the header's address
arm — which is the better shot, and it needs `--features web` and the CEF
bundle to have a page under it. That is stated in `d1-capture.sh` and in the
report rather than faked with an empty well.
"""

import json
import os
import sys
import time

SESSION_A = "33333333-3333-4333-8333-333333333333"  # "the tab strip"
SESSION_B = "55555555-5555-4555-8555-555555555555"  # "package the dmg"

PAGES = [
    ("t-gpui", "https://docs.rs/gpui/latest/gpui/", "gpui - Rust"),
    ("t-std", "https://doc.rust-lang.org/std/", "std - Rust"),
]


def main() -> None:
    data = sys.argv[1]
    now = int(time.time() * 1000)
    events = []
    index = 0

    def opened(tid, content=None):
        nonlocal index
        ev = {"kind": "opened", "id": tid, "index": index, "ts_ms": now}
        if content is not None:
            ev["content"] = content
        events.append(ev)
        index += 1

    opened(PAGES[0][0])
    events.append(
        {
            "kind": "navigated",
            "id": PAGES[0][0],
            "url": PAGES[0][1],
            "title": PAGES[0][2],
            "ts_ms": now,
        }
    )
    opened("t-sess-a", {"kind": "session", "session_id": SESSION_A})
    opened(PAGES[1][0])
    events.append(
        {
            "kind": "navigated",
            "id": PAGES[1][0],
            "url": PAGES[1][1],
            "title": PAGES[1][2],
            "ts_ms": now,
        }
    )
    opened("t-sess-b", {"kind": "session", "session_id": SESSION_B})
    # `D1_ACTIVE` picks which tab the capture opens on. A PAGE makes the pane
    # header draw its address arm — the shot A′ exists for — and needs the CEF
    # bundle under it; a session makes it draw the breadcrumb and works in any
    # build. Default is the page, because the address is the claim.
    events.append({"kind": "activated", "id": os.environ.get("D1_ACTIVE", "t-gpui")})

    path = os.path.join(data, "browser", "tabs.jsonl")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            json.dumps({"format_version": 1, "kind": "tabs", "created_ts_ms": now}) + "\n"
        )
        for i, event in enumerate(events, start=1):
            f.write(json.dumps({"seq": i, "ts_ms": now + i, "event": event}) + "\n")
    print(f"seeded 4 tabs (2 pages, 2 sessions) in {data}")


if __name__ == "__main__":
    main()
