//! Harness support for every test that puts a real agent process on this
//! machine. Nothing here ships.
//!
//! ## Why this file exists
//!
//! `ClaudeCodeSession` deliberately does **not** kill its child on drop
//! (`kill_on_drop(false)`, and the child is `setsid`'d into its own
//! process group). That is a product guarantee, not an oversight: a
//! session outlives the UI, and parent-pid suicide was rejected outright
//! (research: waku pain points; progress.md §Known failures). The
//! consequence is that in TESTS the harness is the only thing standing
//! between a run and an orphaned agent — on every exit path a test has:
//! a normal return, an early `?`, an `.expect()` on a timeout, and a
//! panic.
//!
//! Slice N4 fixed one call site's *return* path. It could not fix the
//! panic path at all, and a panic is the path load actually takes: a
//! failing run leaks the agent of every test that was holding one.
//! Measured 2026-08-20: 34 live `fake-agent` processes on this machine,
//! all `--session-id test-<uuid>` (this file's own id shape), all
//! reparented to init.
//!
//! ## What it does about it
//!
//! Two mechanisms, because one is not enough:
//!
//! 1. [`ReapedSession`] — an RAII wrapper whose `Drop` proves the group
//!    is gone. Drop runs during unwind, so a panicking test cleans up
//!    after itself. This is the fix.
//! 2. [`arm_leak_guard`] — a process-exit check that FAILS the run if any
//!    child of this test binary is still alive. This is the guard: it
//!    catches the next call site that forgets, including one that never
//!    heard of `ReapedSession`. It costs one `waitpid` syscall — no `ps`
//!    scan, no timer, nothing that could ever kill a session.
//!
//! Neither can help a run that is `SIGKILL`ed from outside; nothing can.
//! That case is stated in `docs/reports/stage-ae-orphans-and-race.md`.

#![allow(dead_code)]

use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eave_core::claude::ClaudeCodeSession;
use eave_core::events::SessionEvent;
use eave_core::grok::GrokSession;
use eave_core::proc::{self, ProcIdentity};
use eave_core::process;
use eave_core::session::{AgentSession, SessionError, SpawnConfig};
use eave_core::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};
use tokio::sync::mpsc::Receiver;

/// Spawn an agent that this binary is answerable for: the leak guard is
/// armed, the identity is remembered so the guard can name it, and the
/// returned handle reaps its process group when it drops.
///
/// Prefer this over `ClaudeCodeSession::spawn` in every test. Reaching for
/// the raw constructor means opting out of the only thing that cleans up
/// after a panic.
pub fn spawn_agent(
    cfg: &SpawnConfig,
) -> Result<(ReapedSession, Receiver<SessionEvent>), SessionError> {
    arm_leak_guard();
    let (session, rx) = ClaudeCodeSession::spawn(cfg)?;
    remember(session.identity());
    Ok((ReapedSession(Some(session)), rx))
}

/// The same, for the Grok adapter (Slice AV). A Grok child is setsid'd
/// into its own group exactly like a Claude one, so it needs exactly the
/// same reaping — only the concrete session type differs.
pub fn spawn_grok_agent(
    cfg: &SpawnConfig,
) -> Result<(ReapedSession<GrokSession>, Receiver<SessionEvent>), SessionError> {
    arm_leak_guard();
    let (session, rx) = GrokSession::spawn(cfg)?;
    remember(session.identity());
    Ok((ReapedSession(Some(session)), rx))
}

/// A session that proves its process group is gone before the test that
/// owns it finishes — whether it finishes by returning or by panicking.
///
/// Derefs to `ClaudeCodeSession`, so `session.kill().await` and every
/// other `AgentSession` method still read exactly as they did. A test that
/// kills explicitly (because the kill IS what it is testing) keeps doing
/// so; drop then finds the group already empty and does nothing.
///
/// Generic over the concrete session type so every adapter gets the same
/// cleanup; it defaults to `ClaudeCodeSession` so the name still means what
/// it did before there was more than one adapter.
pub struct ReapedSession<S: AgentSession = ClaudeCodeSession>(Option<S>);

impl<S: AgentSession> Deref for ReapedSession<S> {
    type Target = S;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("session taken")
    }
}

impl<S: AgentSession> DerefMut for ReapedSession<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("session taken")
    }
}

impl<S: AgentSession> Drop for ReapedSession<S> {
    fn drop(&mut self) {
        let Some(session) = self.0.take() else {
            return;
        };
        let id = session.identity();
        // The child was spawned via `setsid`, so its pgid is its pid —
        // the same equivalence `lifecycle.rs` already asserts on.
        if let Err(pgid) = reap_group_blocking(&id) {
            // Loud: a leak that the guard would otherwise report as an
            // anonymous "a child is still alive" is named here instead.
            // Not a panic — a panicking Drop during an unwind aborts the
            // process and would replace the test's real failure with
            // noise.
            eprintln!(
                "eave test harness: could not prove process group {pgid} gone; \
                 pid {} may be orphaned",
                id.pid
            );
        }
    }
}

/// SIGTERM, then SIGKILL, each verified — the blocking twin of
/// `ClaudeCodeSession::kill`, because `Drop` cannot await. Proof of exit
/// is the (pid, start_time) guard going false AND the group emptying;
/// nothing here reports success on a delivered signal alone.
///
/// **No signal is ever sent to a pid whose identity no longer matches.**
/// A dead leader's pgid can be reused, and `killpg` on a reused pgid
/// would hit somebody else's processes — in a suite that runs ten tests
/// at once, "somebody else" is another test's agent. A broken identity
/// guard reads as "already gone", never as "signal it anyway".
///
/// Returns `Err(pgid)` if exit could not be proven.
fn reap_group_blocking(id: &ProcIdentity) -> Result<(), i32> {
    let pgid = id.pid;
    if !proc::identity_matches(id) {
        return Ok(());
    }
    let _ = process::killpg(pgid, libc::SIGTERM);
    if wait_until_gone(id, KILL_GRACE) {
        return Ok(());
    }
    if proc::identity_matches(id) {
        let _ = process::killpg(pgid, libc::SIGKILL);
    }
    if wait_until_gone(id, KILL_VERIFY_WINDOW) {
        return Ok(());
    }
    Err(pgid)
}

/// Proof of exit: the recorded process is gone AND its group is empty.
/// Both, because a group member can outlive its leader.
fn gone(id: &ProcIdentity) -> bool {
    !proc::identity_matches(id) && !process::group_alive(id.pid)
}

fn wait_until_gone(id: &ProcIdentity, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        // Reap our own zombie. `#[tokio::test]` builds a CURRENT-THREAD
        // runtime, and this loop is blocking that one thread — so the
        // supervisor task that would normally `wait()` the child cannot
        // run while we are here. Without this call the child exits, stays
        // a zombie, and a zombie is still a member of its process group:
        // `killpg(pgid, 0)` keeps succeeding and exit is never provable.
        // `WNOHANG`, so an agent that is still alive costs nothing.
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(id.pid, &mut status, libc::WNOHANG) };
        if gone(id) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

static SPAWNED: Mutex<Vec<ProcIdentity>> = Mutex::new(Vec::new());
static ARMED: OnceLock<()> = OnceLock::new();

fn remember(id: ProcIdentity) {
    SPAWNED.lock().unwrap().push(id);
}

/// Arm the process-exit leak check, once per test binary. Idempotent, and
/// only binaries that actually spawn agents pay for it.
pub fn arm_leak_guard() {
    ARMED.get_or_init(|| unsafe {
        libc::atexit(at_exit_check);
    });
}

/// Runs after libtest has printed its summary. A live child at this point
/// is a process that will be reparented to init the instant we return —
/// an orphan — so the run does not get to report success.
///
/// `waitpid(-1, WNOHANG)` is the whole check: one syscall, no `ps` scan,
/// no fork. A positive return is a child that already exited (a zombie we
/// reap here and move past — dead is not leaked); `0` means at least one
/// child is ALIVE; `ECHILD` means there are none left.
extern "C" fn at_exit_check() {
    let alive = loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if r > 0 {
            continue;
        }
        break r == 0;
    };
    if !alive {
        return;
    }
    let named: Vec<i32> = SPAWNED
        .lock()
        .map(|ids| {
            ids.iter()
                .filter(|id| proc::identity_matches(id))
                .map(|id| id.pid)
                .collect()
        })
        .unwrap_or_default();
    let detail = if named.is_empty() {
        "an agent this binary never registered (spawned outside \
         support::spawn_agent?)"
            .to_string()
    } else {
        format!("still-live pids {named:?}")
    };
    let msg = format!(
        "\n\
         LEAKED CHILD PROCESS: this test binary is exiting with a child still \n\
         alive — {detail}.\n\
         It is about to be reparented to init and outlive the run. Every test \n\
         that spawns an agent must prove it gone; use \n\
         `support::spawn_agent`, whose Drop reaps even when the test panics.\n\
         (docs/reports/stage-ae-orphans-and-race.md)\n"
    );
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        // `_exit`, not `exit`: we are already inside exit(), and stdio has
        // been flushed by the Rust runtime before atexit handlers run.
        // 101 is the code libtest itself uses for a failing run.
        libc::_exit(101);
    }
}

// ---------------------------------------------------------------------------
// Provider-agnostic adoption (Slice AU)
// ---------------------------------------------------------------------------

/// Register an already-spawned agent with the leak guard and reap its
/// process group when the returned guard drops.
///
/// The provider-agnostic twin of [`spawn_agent`], for adapters whose
/// constructor is not `ClaudeCodeSession::spawn`. [`ReapedSession`] cannot
/// serve them: it owns a concrete `ClaudeCodeSession`, and the session
/// handle a caller wants to keep calling is the other adapter's own type.
/// So this guard holds only the identity — which is all the reaper ever
/// needed — and the caller keeps its session by value.
///
/// Every reason `support/mod.rs` exists applies unchanged: the child is
/// `setsid`'d into its own group and is deliberately not killed on drop,
/// so in tests this guard is the only thing between a panicking run and an
/// orphan.
#[must_use = "dropping this immediately reaps the agent you just spawned"]
pub fn adopt(identity: ProcIdentity) -> Reaped {
    arm_leak_guard();
    remember(identity);
    Reaped(identity)
}

/// Proves one agent's process group is gone when it drops — including
/// during the unwind of a panicking test.
pub struct Reaped(ProcIdentity);

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Err(pgid) = reap_group_blocking(&self.0) {
            // Loud, but not a panic: a panicking Drop during an unwind
            // aborts the process and would replace the test's real
            // failure with noise.
            eprintln!(
                "eave test harness: could not prove process group {pgid} gone; \
                 pid {} may be orphaned",
                self.0.pid
            );
        }
    }
}
