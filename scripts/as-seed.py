#!/usr/bin/env python3
"""Seed five sessions into a data dir, for the Slice AS capture.

There is still no way to create a session from the command line, so these
are hand-written `meta.json` + header-only `journal.jsonl` pairs and **no
agent ever ran in any of them**. What the rail and the strip then state
about them is real — the engine computes it from what is written here.

Two of the five are unnamed and fall back to `claude code`. Slice AO's
caveat 5 records unnamed-sessions-in-one-repo as the normal case in the
operator's own configuration rather than the edge one, and it is also the
case a tab has the least room to disambiguate.
"""
import json
import os
import sys
import time

ROWS = [
    ("11111111-1111-4111-8111-111111111111", "chase the rail flake", "~/Desktop/eave/eave"),
    ("22222222-2222-4222-8222-222222222222", None, "~/Desktop/eave/eave"),
    ("33333333-3333-4333-8333-333333333333", "the tab strip", "~/Desktop/eave"),
    ("44444444-4444-4444-8444-444444444444", None, "~"),
    ("55555555-5555-4555-8555-555555555555", "package the dmg", "~/Desktop/eave/eave"),
]


def main() -> None:
    data = sys.argv[1]
    now = int(time.time() * 1000)
    for i, (sid, title, cwd) in enumerate(ROWS):
        cwd = os.path.expanduser(cwd)
        d = os.path.join(data, "sessions", sid)
        os.makedirs(d, exist_ok=True)
        meta = {
            "format_version": 1,
            "provider": "claude-code",
            "cwd": cwd,
            "session_id": sid,
            "state": "idle",
            "child": None,
        }
        if title:
            meta["title"] = title
        with open(os.path.join(d, "meta.json"), "w") as f:
            json.dump(meta, f, indent=2)
        header = {
            "format_version": 1,
            "session_id": sid,
            "provider": "claude-code",
            "cwd": cwd,
            "created_ts_ms": now - (len(ROWS) - i) * 60_000,
        }
        with open(os.path.join(d, "journal.jsonl"), "w") as f:
            f.write(json.dumps(header) + "\n")
    print("seeded %d sessions into %s" % (len(ROWS), data))


if __name__ == "__main__":
    main()
