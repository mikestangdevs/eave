#!/usr/bin/env bash
# Every eave run that dies leaves an artifact. This is the thing that makes
# that true.
#
#     scripts/crash-watch.sh run -- ./scripts/p3-capture.sh      # wrap a run
#     scripts/crash-watch.sh decode ~/Library/Logs/.../eave-*.ips
#     scripts/crash-watch.sh recent 5                            # last N eave reports
#
# WHY THIS EXISTS. Slice C5 recorded "one segfault in five capture runs" from
# a single `.ips` it happened to notice. There were SEVEN reports in that ten
# minute window and six of them were a *different* crash — a `already
# borrowed` abort, not a segfault. Nobody looked, because nothing pointed at
# the directory and a capture run that aborts on the way out still leaves its
# screenshot behind. A crash that only shows up if somebody thinks to go
# looking is a crash that gets counted wrong. See docs/reports/stage-c6-segfault.md.
#
# WHAT IT CANNOT DO, and this is the part that misled C5. The Chromium
# Embedded Framework in the bundle is STRIPPED: it exports a few hundred
# symbols across 208 MB. Every `Chromium Embedded Framework` frame in a
# report is therefore resolved to the nearest *exported* symbol, which is
# routinely tens of megabytes away, and the name it prints is unrelated to
# the code that ran. C5's "a stack of V8 frames" was six frames named
# `v8_internal_simulator_ProbeMemory +26400000` — a name and an offset that
# together mean "somewhere in the middle of Chromium". This decoder prints
# those frames as `<cef+OFFSET>` and refuses to repeat the symbol, because
# the symbol is a lie. Real names need CEF's separate ~2 GB debug-symbol
# archive, which is not on this machine.
#
# The `eave`, gpui, AppKit and libsystem frames ARE real — those images carry
# their symbols — and in practice they are the ones that name the bug.
set -euo pipefail

REPORTS="$HOME/Library/Logs/DiagnosticReports"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACTS="${EAVE_CRASH_DIR:-$ROOT/target/crashes}"

decode() {
  python3 - "$1" <<'PY'
import json, io, sys, os, subprocess

_DEMANGLE_CACHE = {}

def demangle(*syms):
    """Real Rust v0 demangling via llvm-cxxfilt, batched.

    `xcrun llvm-cxxfilt` handles the v0 scheme (`_RNvNt...`) as well as the
    legacy one. Hand-rolling this was tried and produced `_NtCsaQ::XJs::gpui`
    for what is actually `<gpui::app::App>::new_app::{closure#3}` — close
    enough to look right and wrong enough to mislead, which is the failure
    mode this whole script exists to stop.
    """
    want = [s for s in syms if s and s not in _DEMANGLE_CACHE]
    if want:
        try:
            out = subprocess.run(['xcrun', 'llvm-cxxfilt'],
                                 input=chr(10).join(want), capture_output=True,
                                 text=True, timeout=20)
            lines = out.stdout.splitlines()
            if len(lines) == len(want):
                _DEMANGLE_CACHE.update(zip(want, lines))
        except Exception:
            pass
    return [_DEMANGLE_CACHE.get(s, s) for s in syms]


path = sys.argv[1]
raw = open(path).read().split('\n', 1)
head = json.loads(raw[0])
d = json.load(io.StringIO(raw[1]))

ex = d.get('exception', {})
term = d.get('termination', {})
sig = ex.get('signal') or term.get('indicator', '?')
kind = ex.get('type', '?')
sub = ex.get('subtype')
asi = d.get('asi', {})

print(f"  file      {os.path.basename(path)}")
print(f"  app       {head.get('app_name')} {head.get('app_version')}   {head.get('timestamp')}")
print(f"  exception {kind}  {sig}" + (f"  {sub}" if sub else ""))
if asi:
    for k, v in asi.items():
        print(f"  message   {k}: {'; '.join(v) if isinstance(v, list) else v}")

imgs = d.get('usedImages', [])
ft = d.get('faultingThread', 0)
th = d['threads'][ft]
print(f"  thread    {ft} {th.get('name','')} {th.get('queue','')}")
print("  stack     (innermost first; cef frames are UNSYMBOLICATED — see header)")

names = demangle(*[f.get('symbol', '') for f in th['frames']])

for i, fr in enumerate(th['frames']):
    im = imgs[fr['imageIndex']]
    name = im.get('name', '?')
    off = fr.get('imageOffset', 0)
    if 'Chromium Embedded Framework' in name:
        # The symbol here is the nearest export, often megabytes away. Do
        # not print it; it has named the wrong thing before.
        line = f"<cef+0x{off:x}>"
    else:
        line = names[i] or f'+{off}'
    print(f"    {i:3}  {name:<26}  {line[:96]}")
PY
}

# The frames that actually name a bug: ours, gpui's, and the toolkit's.
signature() {
  python3 - "$1" <<'PY'
import json, io, sys, subprocess

_DEMANGLE_CACHE = {}

def demangle(*syms):
    """Real Rust v0 demangling via llvm-cxxfilt, batched.

    `xcrun llvm-cxxfilt` handles the v0 scheme (`_RNvNt...`) as well as the
    legacy one. Hand-rolling this was tried and produced `_NtCsaQ::XJs::gpui`
    for what is actually `<gpui::app::App>::new_app::{closure#3}` — close
    enough to look right and wrong enough to mislead, which is the failure
    mode this whole script exists to stop.
    """
    want = [s for s in syms if s and s not in _DEMANGLE_CACHE]
    if want:
        try:
            out = subprocess.run(['xcrun', 'llvm-cxxfilt'],
                                 input=chr(10).join(want), capture_output=True,
                                 text=True, timeout=20)
            lines = out.stdout.splitlines()
            if len(lines) == len(want):
                _DEMANGLE_CACHE.update(zip(want, lines))
        except Exception:
            pass
    return [_DEMANGLE_CACHE.get(s, s) for s in syms]

raw = open(sys.argv[1]).read().split('\n', 1)
d = json.load(io.StringIO(raw[1]))
ex = d.get('exception', {})
imgs = d.get('usedImages', [])
th = d['threads'][d.get('faultingThread', 0)]
named = []
for fr in th['frames']:
    n = imgs[fr['imageIndex']].get('name', '')
    s = fr.get('symbol', '')
    if 'Chromium' in n or not s:
        continue
    if any(t in s for t in ('panic', 'abort', 'pthread_kill', 'rust_begin_unwind',
                            'lang_start', 'call_once', 'backtrace')):
        continue
    named.append(s)
top = demangle(named[0])[0] if named else '?'
print(f"{ex.get('type','?')} {ex.get('signal','')} {ex.get('subtype') or ''} :: {top[:70]}")
PY
}

newest_stamp() {
  # Newest mtime among eave reports, or 0. `find -newermt` needs a file to
  # compare against, so a stamp file is simpler and has no locale problems.
  local s="$1"
  : >"$s"
  local latest
  latest="$(ls -t "$REPORTS"/eave-*.ips 2>/dev/null | head -1 || true)"
  if [ -n "$latest" ]; then
    touch -r "$latest" "$s"
  else
    touch -t 197001010000 "$s"
  fi
}

collect_since() {
  local stamp="$1" label="$2" found=0
  mkdir -p "$ARTIFACTS"
  local f
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    found=$((found + 1))
    local dest="$ARTIFACTS/${label}-$(basename "$f")"
    cp "$f" "$dest"
    echo
    echo "CRASH  $(signature "$f")"
    echo "  kept      $dest"
    decode "$f"
  done < <(find "$REPORTS" -name 'eave-*.ips' -newer "$stamp" 2>/dev/null | sort)
  echo "$found"
}

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

cmd="${1:-}"
case "$cmd" in
  run)
    shift
    if [ "${1:-}" = "--" ]; then shift; fi
    [ $# -gt 0 ] || usage
    label="$(date +%H%M%S)"
    mkdir -p "$ARTIFACTS"
    stamp="$(mktemp)"
    trap 'rm -f "$stamp"' EXIT
    newest_stamp "$stamp"

    # THE PRIMARY ARTIFACT IS THE PROCESS'S OWN OUTPUT, not the .ips.
    # A Rust panic prints its file and line to stderr the instant it
    # happens; the .ips arrives whenever ReportCrash gets round to it,
    # which on a loaded machine is TENS OF SECONDS later and was measured
    # at over a minute here. A collector that samples the directory three
    # seconds after the process dies reports "clean" for a run that
    # aborted — which is how six aborts hid behind one noticed segfault.
    log="$ARTIFACTS/$label-output.log"
    set +e
    "$@" 2>&1 | tee "$log"
    status=${PIPESTATUS[0]}
    set -e

    panics="$(grep -n -E "panicked at|fatal runtime error|RefCell already borrowed" "$log" || true)"
    sig=""
    case "$status" in
      134) sig="SIGABRT (134)" ;;
      139) sig="SIGSEGV (139)" ;;
      133) sig="SIGTRAP (133)" ;;
      137) sig="SIGKILL (137) — did not exit on its own" ;;
    esac

    echo
    if [ -n "$panics" ]; then
      echo "PANIC: the run printed a Rust panic"
      printf '%s\n' "$panics" | sed 's/^/    /'
    fi
    if [ -n "$sig" ]; then echo "SIGNAL: $sig"; fi

    # Now wait for the .ips, generously, and only as a bonus. Poll rather
    # than sleep a flat interval so a fast machine is not punished.
    waited=0
    if [ -n "$sig" ] || [ -n "$panics" ]; then
      while [ "$waited" -lt "${EAVE_CRASH_WAIT:-90}" ]; do
        if [ -n "$(find "$REPORTS" -name 'eave-*.ips' -newer "$stamp" 2>/dev/null)" ]; then break; fi
        sleep 3
        waited=$((waited + 3))
      done
    fi

    out="$(collect_since "$stamp" "$label")"
    n="$(printf '%s' "$out" | tail -1)"
    printf '%s\n' "$out" | sed '$d'
    echo
    echo "  output kept: $log"
    if [ "$n" -gt 0 ]; then
      echo "CRASH-WATCH: $n crash report(s) after ${waited}s, kept in $ARTIFACTS"
      exit 86
    fi
    if [ -n "$panics" ] || [ -n "$sig" ]; then
      echo "CRASH-WATCH: the run died (${sig:-panic}) but no .ips appeared within ${waited}s."
      echo "             The output log above is the artifact. This is normal:"
      echo "             ReportCrash is best-effort and lags badly under load."
      exit 86
    fi
    echo "CRASH-WATCH: clean (command exited $status)"
    exit "$status"
    ;;
  decode)
    [ $# -ge 2 ] || usage
    decode "$2"
    ;;
  recent)
    n="${2:-5}"
    ls -t "$REPORTS"/eave-*.ips 2>/dev/null | head -"$n" | while IFS= read -r f; do
      echo "=== $(signature "$f")"
      echo "    $f"
    done
    ;;
  *) usage ;;
esac
