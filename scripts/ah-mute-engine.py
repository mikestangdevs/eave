#!/usr/bin/env python3
"""Slice AH — a wire-speaking engine that answers everything EXCEPT `files`.

The instrument for the eternal skeleton, and it is deliberately NOT the
product. The real engine answers a `files` subscription in every failure
mode I could construct (`every_files_subscription_answers_at_both_scopes_
and_every_cwd` in `crates/eave-engine/tests/files.rs`), so the state under
measurement cannot be produced by misusing it — it is produced by the
TRANSPORT: an engine that is wedged, stopped, or mid-reconnect.

This stands in for that. It speaks spec 05's newline-delimited JSON on the
data dir's `engine.sock`, answers `hello` / `list` / `watch` /
`unsubscribe`, accepts a `subscribe` and a `files` and then says nothing
on either. What the app then does with a selected session and no file
list is the whole question.

Usage:  ah-mute-engine.py <data-dir> [session-id]
"""
import json
import os
import socket
import sys
import threading
import time

PROTO = 1


def session_info(sid, cwd):
    return {
        "session_id": sid,
        "provider": "claude-code",
        "cwd": cwd,
        "created_ts_ms": int(time.time() * 1000),
        "state": "idle",
        "live": False,
        "epoch": 1,
        # 0 so the transcript pane reads "nothing sent yet" — a SETTLED
        # empty state. A non-zero last_seq with no records is its own
        # loading state and would put a second lease in the measurement.
        "last_seq": 0,
    }


def serve(data_dir, sid):
    sock_path = os.path.join(data_dir, "engine.sock")
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    srv.listen(8)
    print(f"mute engine on {sock_path} (session {sid})", flush=True)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn, sid), daemon=True).start()


def handle(conn, sid):
    cwd = os.path.expanduser("~")
    f = conn.makefile("rwb")
    for line in f:
        try:
            env = json.loads(line)
        except ValueError:
            continue
        rid, method = env.get("id"), env.get("method")
        if method == "hello":
            body = {"ok": {"proto": PROTO,
                           "engine": {"pid": os.getpid(),
                                      "start_tvsec": 0, "start_tvusec": 0}}}
        elif method == "list":
            body = {"ok": {"sessions": [session_info(sid, cwd)]}}
        elif method == "watch":
            body = {"item": {"type": "sessions",
                             "sessions": [session_info(sid, cwd)]}}
        elif method == "unsubscribe":
            body = {"ok": {"stopped": True}}
        elif method in ("subscribe", "files"):
            # The point of the whole instrument: accepted, never answered.
            print(f"swallowed {method} on id {rid}", flush=True)
            continue
        else:
            body = {"err": {"message": f"mute engine does not do {method}"}}
        f.write((json.dumps({"id": rid, **body}) + "\n").encode())
        f.flush()


if __name__ == "__main__":
    serve(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "aaaa1111-mute")
