#!/usr/bin/env python3
"""Seed a data dir for the Slice C5 capture: sessions, tabs, and history.

Three files, hand-written, because there is still no command-line way to
make any of them:

* `sessions/<uuid>/{meta.json,journal.jsonl}` — the same shape
  `scripts/as-seed.py` writes. No agent ever ran in these; what the rail
  and the strip then say about them is real, because the engine computes
  it from what is here.

* `browser/tabs.jsonl` — B1's own journal, folded at open. This is how the
  capture gets **two page tabs and one session tab** without driving the
  interface to create them: the strip is a projection of this file.

  The two page tabs carry a committed `navigated` event each, which is
  what gives a restored tab a url to reopen at (`web::tabs::drive` uses
  `tab.url` as the start url when there is no pending request). `load` is
  deliberately absent: it is transient, never journaled, and
  `BrowserStore::open` resets it — a restored strip claiming to be busy is
  the lie the durability split exists to prevent.

* `browser/history.jsonl` — visits, so the omnibox's typeahead has
  something to find. **The whole point of the capture is a dropdown with
  real rows in it**, and an omnibox photographed over an empty history is
  a photograph of a text field.

The history is written as `visited` events with a `transition`, which is
what ranks them: `typed` outranks `link`, and `reload` does not count as a
visit at all. The mix here is deliberate rather than uniform, so the
ordering in the shot is the ranking doing its job rather than insertion
order.
"""

import json
import os
import sys
import time

SESSIONS = [
    ("11111111-1111-4111-8111-111111111111", "the tab strip", "~/Desktop/eave/eave"),
]

# (url, title, transition, visits)
HISTORY = [
    ("https://docs.rs/gpui/latest/gpui/", "gpui - Rust", "typed", 9),
    ("https://doc.rust-lang.org/std/", "std - Rust", "typed", 6),
    ("https://docs.rs/serde/latest/serde/", "serde - Rust", "link", 4),
    ("https://github.com/mikestangdevs/zed", "mikestangdevs/zed", "typed", 3),
    ("https://bitbucket.org/chromiumembedded/cef", "chromiumembedded / cef", "link", 2),
    ("https://doc.rust-lang.org/book/", "The Rust Programming Language", "link", 2),
]


def write_sessions(data: str, now: int) -> None:
    for sid, title, cwd in SESSIONS:
        d = os.path.join(data, "sessions", sid)
        os.makedirs(d, exist_ok=True)
        # `as-seed.py`'s exact shape, and the two fields that look
        # optional are not: `child: null` is what makes the meta parse,
        # and the JOURNAL HEADER is what makes the engine list the session
        # at all. A first attempt at this omitted the header and wrote a
        # different meta; the rail said "no sessions" and the strip drew
        # `strip::ORPHAN_SESSION_LABEL` — which is the fallback working
        # correctly over a seed that was wrong.
        meta = {
            "format_version": 1,
            "provider": "claude-code",
            "cwd": os.path.expanduser(cwd),
            "session_id": sid,
            "state": "idle",
            "child": None,
        }
        if title:
            meta["title"] = title
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
    data = sys.argv[1]
    now = int(time.time() * 1000)
    write_sessions(data, now)

    session_id = SESSIONS[0][0]
    pages = [
        ("t-gpui", "https://docs.rs/gpui/latest/gpui/", "gpui - Rust"),
        ("t-std", "https://doc.rust-lang.org/std/", "std - Rust"),
    ]
    tab_events = []
    for i, (tid, url, title) in enumerate(pages):
        tab_events.append(
            {
                "kind": "opened",
                "id": tid,
                "index": i,
                "ts_ms": now,
            }
        )
        tab_events.append(
            {
                "kind": "navigated",
                "id": tid,
                "url": url,
                "title": title,
                "ts_ms": now,
            }
        )
    tab_events.append(
        {
            "kind": "opened",
            "id": "t-session",
            "index": 2,
            "content": {"kind": "session", "session_id": session_id},
            "ts_ms": now,
        }
    )
    # The FIRST page tab is active, so the well holds a document and the
    # omnibox in the shot is over one. A capture whose active tab were the
    # session would photograph the omnibox over a transcript, which is the
    # one arrangement the slice's occlusion claim is not about.
    tab_events.append({"kind": "activated", "id": "t-gpui"})
    log(os.path.join(data, "browser", "tabs.jsonl"), "tabs", tab_events, now)

    day = 86_400_000
    visits = []
    for i, (url, title, transition, count) in enumerate(HISTORY):
        for n in range(count):
            visits.append(
                {
                    "kind": "visited",
                    "url": url,
                    "title": title,
                    # Spread backwards in time, so `frecency` has an age to
                    # decay over rather than every visit landing at once.
                    "ts_ms": now - (i * day) - (n * 3_600_000),
                    "transition": transition,
                }
            )
    log(os.path.join(data, "browser", "history.jsonl"), "history", visits, now)

    print(f"seeded {len(SESSIONS)} session(s), 3 tabs, {len(visits)} visits in {data}")


if __name__ == "__main__":
    main()
