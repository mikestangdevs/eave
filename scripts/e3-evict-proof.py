#!/usr/bin/env python3
"""Slice E3's proof: idle page tabs give their browsers back, and the memory
goes with them.

    scripts/e3-evict-proof.py [--tabs 10] [--idle-ms 20000]

# Why this drives the wire instead of the window
A browser is only created for the tab that is ACTIVE (C5's rule), so proving
anything about ten live browsers means activating ten tabs. The strip cannot
be clicked: `docs/progress.md` records, verified against `main`, that the app
draws no tabs at all. So this speaks the engine's own protocol —
newline-delimited JSON on `<data-dir>/engine.sock` — and sends `tab_activate`
itself. That is not a workaround for a broken strip so much as the more
honest instrument: it drives the same requests the strip would send, and it
cannot photograph a lie.

# Why ten ORIGINS and not ten urls
Chromium gives same-site pages one renderer process. Ten `file://` pages
would have shared one, and the run would have "proved" that nine evictions
free almost nothing. Ten loopback ports are ten origins, which is what a
person with ten real tabs open actually has.

# What it measures
Direct children of the app whose executable path contains `Helper` — the
same set, by the same rule, that `web::web_memory()` sums for the rail's
`web · N.N GB in P` line. So the number here and the number on screen are
the same number.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "target", "bundle", "eave.app", "Contents", "MacOS", "eave")


# ---------------------------------------------------------------------------
# A page per origin
# ---------------------------------------------------------------------------

# A page that HOLDS something. A 2 KB page would have proved only that
# eviction reclaims Chromium's own per-browser bookkeeping; the claim worth
# proving is that it reclaims memory in proportion to what the page is
# holding. The typed array is filled rather than merely allocated, because
# an untouched allocation need never become resident.
PAGE_TMPL = """<!doctype html><meta charset=utf-8><title>eave e3 tab {n}</title>
<style>body{{font:16px system-ui;padding:40px}}</style>
<h1>tab {n}</h1>
<script>
  window.__hog = new Uint8Array({mb} * 1024 * 1024).fill({n} % 251 + 1);
  document.title = "eave e3 tab {n}";
</script>
"""


class Quiet(BaseHTTPRequestHandler):
    def do_GET(self):
        body = PAGE_TMPL.format(n=self.server.tab_n, mb=self.server.page_mb).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def serve(n, page_mb):
    """One origin. Port 0 lets the OS pick, so a re-run never collides."""
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Quiet)
    httpd.tab_n = n
    httpd.page_mb = page_mb
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, f"http://127.0.0.1:{httpd.server_address[1]}/"


# ---------------------------------------------------------------------------
# The wire
# ---------------------------------------------------------------------------


class Wire:
    """The engine's protocol, in the smallest form that can ask it things."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.f = self.sock.makefile("rwb")
        self.id = 0
        self.call("hello", {"proto": 1})

    def call(self, method, params=None, timeout=10.0):
        self.id += 1
        env = {"id": self.id, "method": method}
        if params is not None:
            env["params"] = params
        self.f.write((json.dumps(env) + "\n").encode())
        self.f.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.f.readline()
            if not line:
                raise RuntimeError("engine closed the connection")
            msg = json.loads(line)
            # Subscription items carry other ids; this client only makes
            # unary calls, so anything else is somebody else's traffic.
            if msg.get("id") != self.id:
                continue
            # `ResponseBody` is an externally-tagged enum flattened into
            # the envelope: `{"id":N,"ok":...}` or `{"id":N,"err":{...}}`.
            if "err" in msg:
                raise RuntimeError(f"{method} refused: {msg['err'].get('message')}")
            return msg.get("ok")
        raise TimeoutError(f"{method} never answered")


# ---------------------------------------------------------------------------
# What the helpers cost
# ---------------------------------------------------------------------------


def helpers(app_pid):
    """(count, resident bytes) over the CEF helper children.

    Deliberately the same rule as `web_memory()`: direct children only, and
    `Helper` in the executable path — which is what tells Chromium's helpers
    apart from eave's own detached engine child.
    """
    try:
        kids = subprocess.run(
            ["pgrep", "-P", str(app_pid)], capture_output=True, text=True
        ).stdout.split()
    except Exception:
        return 0, 0
    n, total = 0, 0
    for pid in kids:
        out = subprocess.run(
            ["ps", "-o", "rss=,comm=", "-p", pid], capture_output=True, text=True
        ).stdout.strip()
        if not out or "Helper" not in out:
            continue
        rss_kb = out.split()[0]
        n += 1
        total += int(rss_kb) * 1024
    return n, total


def gib(b):
    return f"{b / (1 << 30):.2f} GB"


# ---------------------------------------------------------------------------

def seed(data, tabs):
    """Ten page tabs, each with a committed navigation so it has a title."""
    now = int(time.time() * 1000)
    events = []
    for i, (tid, url) in enumerate(tabs):
        events.append({"kind": "opened", "id": tid, "index": i, "ts_ms": now})
        events.append(
            {
                "kind": "navigated",
                "id": tid,
                "url": url,
                "title": f"tab {i}",
                "ts_ms": now,
            }
        )
    events.append({"kind": "activated", "id": tabs[0][0]})
    path = os.path.join(data, "browser", "tabs.jsonl")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            json.dumps({"format_version": 1, "kind": "tabs", "created_ts_ms": now})
            + "\n"
        )
        for i, ev in enumerate(events, start=1):
            f.write(json.dumps({"seq": i, "ts_ms": now + i, "event": ev}) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tabs", type=int, default=10)
    ap.add_argument("--idle-ms", type=int, default=20000)
    ap.add_argument("--settle", type=float, default=4.0, help="seconds per activation")
    ap.add_argument(
        "--page-mb",
        type=int,
        default=32,
        help="megabytes each page holds live, so the reclaim is page-proportional",
    )
    args = ap.parse_args()

    if not os.path.exists(APP):
        sys.exit(f"no bundle at {APP} — run scripts/bundle-app.sh --features web")

    # SHORT data dir on purpose: the engine's socket lives inside it and a
    # unix socket path has a hard length limit.
    data = f"/tmp/e3-{os.getpid()}"
    shutil.rmtree(data, ignore_errors=True)
    os.makedirs(data)

    servers, tabs = [], []
    for i in range(args.tabs):
        httpd, url = serve(i, args.page_mb)
        servers.append(httpd)
        tabs.append((f"t{i}", url))
    seed(data, tabs)

    env = dict(os.environ)
    env["EAVE_DATA_DIR"] = data
    env["EAVE_WEB_TRACE"] = "1"
    env["EAVE_THRESH_PAGE_EVICT_MS"] = str(args.idle_ms)
    log = open(f"{data}/app.log", "wb")
    app = subprocess.Popen([APP], env=env, stdout=log, stderr=subprocess.STDOUT)
    print(f"app pid {app.pid}, data {data}")

    sock_path = os.path.join(data, "engine.sock")
    for _ in range(200):
        if os.path.exists(sock_path):
            break
        if app.poll() is not None:
            log.flush()
            tail = open(f"{data}/app.log", "rb").read()[-3000:].decode(errors="replace")
            sys.exit(f"app exited {app.returncode} before opening a socket:\n{tail}")
        time.sleep(0.1)
    else:
        app.kill()
        sys.exit("engine never opened its socket")
    time.sleep(1.0)
    w = Wire(sock_path)

    def interrupted():
        """Did somebody else stop this app mid-measurement?

        `scripts/stop-eave.py` kills every eave process on the machine "from
        every checkout" by design, and `dev.sh` / `run-app.sh` both call it.
        A peer session launching the app therefore terminates this run's app
        mid-flight, and the symptom is a `BrokenPipeError` from a socket the
        engine no longer owns — which looks like a defect in eviction and is
        not one. See the report.
        """
        try:
            with open(f"{data}/app.log", "rb") as f:
                return b"quit requested" in f.read()
        except OSError:
            return False

    marks = {}

    def mark(label):
        n, b = helpers(app.pid)
        marks[label] = (n, b)
        print(f"  {label:<28} {n:>3} helpers  {gib(b)}")
        return n, b

    def guard():
        if interrupted():
            sys.exit(
                "\nABORTED — another session stopped this app mid-run "
                "(`quit requested` in the log; scripts/stop-eave.py kills "
                "every eave on the machine). Re-run when no peer is "
                "launching the app."
            )

    print("\n— filling —")
    mark("before any activation")
    for i, (tid, _) in enumerate(tabs):
        guard()
        w.call("tab_activate", {"tab_id": tid})
        time.sleep(args.settle)
        if i == 0:
            mark("after tab 0")
    n_full, b_full = mark(f"after all {args.tabs} tabs")

    # A back stack to lose. Two extra commits on t5, so the restored tab has
    # something a reload could plausibly have destroyed.
    # Not tab 0: tab 0 is where the run parks while the clock runs out, so
    # it is the one tab that must NOT evict. Pick from the far end.
    victim = tabs[-1][0] if args.tabs > 1 else tabs[0][0]
    # Generous: a navigation has to be issued by `drive_tab` on the next
    # snapshot, then load, then commit before it is in the stack. A short
    # sleep here produced a one-entry stack and a back-stack claim that
    # proved nothing.
    w.call("tab_navigate", {"tab_id": victim, "url": tabs[0][1]})
    time.sleep(6.0)
    w.call("tab_navigate", {"tab_id": victim, "url": tabs[1][1]})
    time.sleep(6.0)
    before_nav = w.call("tab_nav", {"tab_id": victim})
    print(f"\n  {victim} nav stack before eviction: {json.dumps(before_nav)[:220]}")

    # Park on tab 0 and let the clock run out on the other nine.
    w.call("tab_activate", {"tab_id": "t0"})
    wait = args.idle_ms / 1000.0 + 8
    print(f"\n— idling {wait:.0f}s past a {args.idle_ms} ms threshold —")
    time.sleep(wait)
    # A peer stopping its own eave stops this one too, and the symptom
    # downstream is "0 helpers, 0.00 GB" — which reads as a spectacular
    # eviction result rather than as a dead app.
    guard()
    n_evicted, b_evicted = mark("after the clock ran out")

    print("\n— restoring —")
    w.call("tab_activate", {"tab_id": victim})
    time.sleep(args.settle + 2)
    guard()
    n_back, b_back = mark(f"after re-activating {victim}")
    after_nav = w.call("tab_nav", {"tab_id": victim})
    print(f"  {victim} nav stack after restore:   {json.dumps(after_nav)[:220]}")

    app.send_signal(signal.SIGTERM)
    try:
        app.wait(timeout=20)
    except subprocess.TimeoutExpired:
        app.kill()
    for s in servers:
        s.shutdown()

    print("\n— verdict —")
    freed = b_full - b_evicted
    print(f"  peak            {marks[f'after all {args.tabs} tabs'][0]:>3} helpers  {gib(b_full)}")
    print(f"  after eviction  {n_evicted:>3} helpers  {gib(b_evicted)}")
    print(f"  freed           {'':>3}          {gib(freed)}")
    print(f"  exit code       {app.returncode}")
    print(f"  log             {data}/app.log")

    evictions = 0
    with open(f"{data}/app.log", "rb") as f:
        evictions = f.read().count(b"evicting idle tab")
    print(f"  evictions in the trace: {evictions}")

    ok = evictions >= args.tabs - 1 and b_evicted < b_full and n_back > n_evicted
    print("\nRESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
