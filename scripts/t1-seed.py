#!/usr/bin/env python3
"""Seed a data dir for the Slice T1 capture: projects, tabs, and bookmarks.

    t1-seed.py <data-dir>

T1 added three surfaces and none of them can be made from the command
line, so all three are written as journals and the engine folds them —
the same reasoning `c5-seed.py` and `d1-seed-tabs.py` record. What the
shot then shows is real: the strip is a projection of `tabs.jsonl` and the
bar is a projection of `bookmarks.jsonl`.

Three files:

* `sessions/<uuid>/{meta.json,journal.jsonl}` — two sessions, so the
  project has chats in it and not just pages. No agent ever ran in either.

* `browser/tabs.jsonl` — a GROUP carrying a `workspace`, two tabs inside
  it (one page, one session), and two loose tabs outside it. The group is
  written LAST on purpose: `strip::drawn_order` is what lifts projects
  above loose tabs, and a fixture that pre-sorted them would photograph
  the fixture instead of the rule.

* `browser/bookmarks.jsonl` — two links and a folder with two links in
  it. The folder is the shot that matters for the removal menu: a folder's
  row is named differently from a link's because removing it takes the
  subtree.

The ACTIVE tab is the group's PAGE tab, because the bookmarks bar only
draws over a page (`bookmarks_bar(showing_page)`) — a capture whose active
tab were a session would photograph a chat and no bar, truthfully and
uselessly.
"""

import json
import os
import sys
import time

WORKSPACE = os.path.expanduser("~/Desktop/eave/wt-e3")
GROUP = "g-eave"

SESSIONS = [
    ("33333333-3333-4333-8333-333333333333", "the tab strip", WORKSPACE),
    ("55555555-5555-4555-8555-555555555555", "package the dmg", WORKSPACE),
]

# (id, url, title) — the two pages that live inside the project, and the
# two that do not.
IN_PROJECT_PAGE = ("t-gpui", "https://docs.rs/gpui/latest/gpui/", "gpui - Rust")
LOOSE = [
    ("t-std", "https://doc.rust-lang.org/std/", "std - Rust"),
    ("t-cef", "https://bitbucket.org/chromiumembedded/cef/", "chromiumembedded / cef"),
]

BOOKMARKS = [
    # (id, parent, title, url)  — url None makes a folder
    ("b-gh", None, None, "https://github.com/"),
    ("b-docs", None, "docs", None),
    ("b-gpui", "b-docs", None, "https://docs.rs/gpui/latest/gpui/"),
    ("b-std", "b-docs", None, "https://doc.rust-lang.org/std/"),
    ("b-cef", None, "cef", "https://bitbucket.org/chromiumembedded/cef/"),
]


def write_log(data, name, kind, events, now):
    """One `EventLog` file: a header line, then one `{seq,ts_ms,event}` per
    line.

    The envelope is not decoration. A bare list of events is what the first
    version of this script wrote, and the engine moved BOTH files aside as
    `*.corrupt-<ts>` and started empty — the app came up, drew a truthful
    picture of an empty browser, and would have been photographed as one.
    """
    d = os.path.join(data, "browser")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "w", encoding="utf-8") as f:
        f.write(
            json.dumps({"format_version": 1, "kind": kind, "created_ts_ms": now}) + "\n"
        )
        for i, event in enumerate(events, start=1):
            f.write(json.dumps({"seq": i, "ts_ms": now + i, "event": event}) + "\n")


def seed_sessions(data, now):
    for i, (sid, title, cwd) in enumerate(SESSIONS):
        d = os.path.join(data, "sessions", sid)
        os.makedirs(d, exist_ok=True)
        meta = {
            "format_version": 1,
            "provider": "claude-code",
            "cwd": cwd,
            "session_id": sid,
            "state": "idle",
            "child": None,
            "title": title,
        }
        with open(os.path.join(d, "meta.json"), "w") as f:
            json.dump(meta, f, indent=2)
        header = {
            "format_version": 1,
            "session_id": sid,
            "provider": "claude-code",
            "cwd": cwd,
            "created_ts_ms": now - (len(SESSIONS) - i) * 60_000,
        }
        with open(os.path.join(d, "journal.jsonl"), "w") as f:
            f.write(json.dumps(header) + "\n")


def seed_tabs(data, now):
    ev = []

    # The loose tabs first, so the group is genuinely later in the log
    # AND later in the operator's ordering.
    for i, (tid, url, title) in enumerate(LOOSE):
        ev.append({"kind": "opened", "id": tid, "index": i, "ts_ms": now})
        ev.append(
            {"kind": "navigated", "id": tid, "url": url, "title": title, "ts_ms": now}
        )

    ev.append(
        {
            "kind": "group_created",
            "id": GROUP,
            "title": "wt-e3",
            "workspace": WORKSPACE,
            "index": len(LOOSE),
            "ts_ms": now,
        }
    )
    tid, url, title = IN_PROJECT_PAGE
    ev.append({"kind": "opened", "id": tid, "group": GROUP, "index": 0, "ts_ms": now})
    ev.append(
        {"kind": "navigated", "id": tid, "url": url, "title": title, "ts_ms": now}
    )
    ev.append(
        {
            "kind": "opened",
            "id": "t-chat",
            "group": GROUP,
            "index": 1,
            "content": {"kind": "session", "session_id": SESSIONS[0][0]},
            "ts_ms": now,
        }
    )
    # Active is the project's PAGE tab: the bar only draws over a page.
    ev.append({"kind": "activated", "id": IN_PROJECT_PAGE[0]})

    write_log(data, "tabs.jsonl", "tabs", ev, now)


def seed_bookmarks(data, now):
    ev = []
    seen = {}
    for bid, parent, title, url in BOOKMARKS:
        index = seen.get(parent, 0)
        seen[parent] = index + 1
        e = {"kind": "added", "id": bid, "index": index, "ts_ms": now}
        if parent is not None:
            e["parent"] = parent
        if title is not None:
            e["title"] = title
        if url is not None:
            e["url"] = url
        ev.append(e)
    write_log(data, "bookmarks.jsonl", "bookmarks", ev, now)


def main() -> None:
    data = sys.argv[1]
    now = int(time.time() * 1000)
    seed_sessions(data, now)
    seed_tabs(data, now)
    seed_bookmarks(data, now)
    print(
        "seeded %d sessions, 1 project (%s), %d bookmarks into %s"
        % (len(SESSIONS), WORKSPACE, len(BOOKMARKS), data)
    )


if __name__ == "__main__":
    main()
