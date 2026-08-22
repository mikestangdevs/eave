#!/usr/bin/env python3
"""Seed a data dir for the Slice E1 capture: one session, one page tab.

`c5-seed.py`'s shape, deliberately narrowed. C5 was photographing a strip
and an omnibox, so it needed three tabs and a history to rank. E1 is
photographing things that happen ON one page — a find bar, a context menu,
a zoom fact, and a resize — so a second tab would be a second thing in the
shot that the slice is not about.

The page tab carries a committed `navigated` event, which is what gives a
restored tab a url to reopen at (`web::tabs::drive` uses `tab.url` as the
start url when there is no pending request). `load` is deliberately absent:
it is transient, never journaled, and `BrowserStore::open` resets it.

The url is passed in, because the capture points it at a LOCAL file whose
path depends on the checkout.
"""

import json
import os
import sys
import time

SESSION = ("11111111-1111-4111-8111-111111111111", "page interaction", "~/Desktop/eave/eave")


def write_session(data: str, now: int) -> None:
    sid, title, cwd = SESSION
    d = os.path.join(data, "sessions", sid)
    os.makedirs(d, exist_ok=True)
    meta = {
        "format_version": 1,
        "provider": "claude-code",
        "cwd": os.path.expanduser(cwd),
        "session_id": sid,
        "state": "idle",
        "child": None,
        "title": title,
    }
    with open(os.path.join(d, "meta.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2)
    header = {
        "format_version": 1,
        "session_id": sid,
        "provider": "claude-code",
        "cwd": os.path.expanduser(cwd),
        "created_ts_ms": now - 60_000,
    }
    with open(os.path.join(d, "journal.jsonl"), "w", encoding="utf-8") as f:
        f.write(json.dumps(header) + "\n")


def log(path: str, kind: str, events: list, now: int) -> None:
    """One of B1's three logs: a header line, then `{seq, ts_ms, event}`."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            json.dumps({"format_version": 1, "kind": kind, "created_ts_ms": now}) + "\n"
        )
        for i, event in enumerate(events, start=1):
            f.write(json.dumps({"seq": i, "ts_ms": now + i, "event": event}) + "\n")


def main() -> None:
    data, url = sys.argv[1], sys.argv[2]
    now = int(time.time() * 1000)
    write_session(data, now)
    events = [
        {"kind": "opened", "id": "t-page", "index": 0, "ts_ms": now},
        {
            "kind": "navigated",
            "id": "t-page",
            "url": url,
            "title": "eave — slice E1 fixture",
            "ts_ms": now,
        },
        {"kind": "activated", "id": "t-page"},
    ]
    log(os.path.join(data, "browser", "tabs.jsonl"), "tabs", events, now)
    print(f"seeded 1 session and 1 page tab at {url} in {data}")


if __name__ == "__main__":
    main()
