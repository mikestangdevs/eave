//! The process half of an ACP session: spawn it into its own group, watch
//! its pipes, and kill it with proof of exit.
//!
//! Every line of this used to exist twice, verbatim, in `grok.rs` and
//! `hermes.rs` — including the sentinel-identity fallback, the stderr ring,
//! the supervisor ordering and the three-step kill. None of it is
//! agent-specific: it is spec 01 §Spawn and §Kill, which apply to any child
//! eave owns. What IS agent-specific rides in on [`ChildSpec`].

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{mpsc, watch};

use crate::acp::wire::SharedStdin;
use crate::events::SessionEvent;
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{KillReport, SessionError, SessionWatch};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

/// Bytes of the child's stderr kept for the exit report.
const STDERR_RING_CAP: usize = 4096;
/// Depth of the event channel between a reader task and the engine.
pub const EVENT_CHANNEL_CAP: usize = 256;

/// The child's exit code and terminating signal, whichever it had.
pub type ExitFacts = (Option<i32>, Option<i32>);

/// Exactly what eave does to the inherited environment before running an
/// agent process — and, for Hermes, before running the out-of-band probe
/// that vouches for it.
///
/// **One value, applied identically everywhere it is used, and that
/// identity is the point.** A probe that runs in a different environment
/// than the session vouches for a posture nobody is going to run under.
///
/// The order below is load-bearing: the session's own `extra_env` is
/// applied LAST, so a caller who deliberately re-sets a scrubbed variable
/// beats the scrub — and then the probe, seeing the same variable, answers
/// honestly and the session is refused. The hole closes itself precisely
/// because both sides see the same environment.
#[derive(Clone, Debug, Default)]
pub struct ChildEnv {
    remove: Vec<String>,
    set: Vec<(String, String)>,
}

impl ChildEnv {
    /// The floor every eave child gets: none of eave's own variables reach
    /// it (spec 01 §Spawn; research: bb.md §1).
    pub fn sanitized() -> Self {
        Self {
            remove: std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| k.starts_with("EAVE_"))
                .collect(),
            set: Vec::new(),
        }
    }

    /// Additional variables to scrub — each one an approval bypass the
    /// operator's shell could be carrying.
    pub fn scrubbing(mut self, vars: &[&str]) -> Self {
        self.remove.extend(vars.iter().map(|s| s.to_string()));
        self
    }

    pub fn setting(mut self, key: &str, value: &str) -> Self {
        self.set.push((key.to_string(), value.to_string()));
        self
    }

    /// The session's own environment, applied last. See the type's docs
    /// for why "last" is the safe end.
    pub fn and_extra(mut self, extra: &[(String, String)]) -> Self {
        self.set.extend(extra.iter().cloned());
        self
    }

    pub fn apply(&self, cmd: &mut Command) {
        for var in &self.remove {
            cmd.env_remove(var);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}

/// What an agent contributes to its own spawn: where its binary is, what
/// argv puts it in ACP-serving mode, and what its environment must not
/// carry.
pub struct ChildSpec {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub env: ChildEnv,
}

/// The half of a live child a reader task needs: somewhere to write, the
/// activity clock to touch, and the flag that says the kill has started.
#[derive(Clone)]
pub struct ChildContext {
    pub stdin: SharedStdin,
    last_activity: Arc<Mutex<Instant>>,
    kill_initiated: Arc<AtomicBool>,
}

/// The child's stdout, read a line at a time with the two housekeeping
/// rules every ACP reader has to obey.
pub struct AcpLines {
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    ctx: ChildContext,
}

impl AcpLines {
    /// The next protocol line, or `None` at EOF.
    ///
    /// Every line touches the activity clock even when it is dropped —
    /// the attention monitor measures silence on the pipe, not silence in
    /// the transcript. Lines arriving after a kill has been initiated are
    /// dropped rather than parsed: descendants holding inherited pipes may
    /// emit stale frames after the leader dies (spec 01 §Kill).
    pub async fn next(&mut self) -> Option<String> {
        loop {
            let line = self.lines.next_line().await.ok().flatten()?;
            *self.ctx.last_activity.lock().unwrap() = Instant::now();
            if self.ctx.kill_initiated.load(Ordering::SeqCst) {
                continue;
            }
            return Some(line);
        }
    }
}

/// One `setsid`'d agent process, its pipes, and the proof machinery around
/// its exit.
pub struct AcpChild {
    identity: ProcIdentity,
    /// The child was `setsid`'d, so its pgid is its pid.
    pgid: i32,
    ctx: ChildContext,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
    /// Side tasks stopped when the leader is reaped (Hermes' staleness
    /// sweep). A task that outlived its session would poll a state machine
    /// nothing can answer.
    at_exit: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl AcpChild {
    /// Spawn the agent in its own process group with sanitized env and
    /// piped stdio (spec 01 §Spawn), and start `reader` over its stdout.
    /// `SessionEvent::Exited` is always the final event on the channel.
    pub fn spawn<F, Fut>(
        spec: ChildSpec,
        reader: F,
    ) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError>
    where
        F: FnOnce(AcpLines, ChildContext, mpsc::Sender<SessionEvent>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut cmd = Command::new(&spec.program);
        cmd.current_dir(&spec.cwd);
        cmd.args(&spec.args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        spec.env.apply(&mut cmd);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(SessionError::Spawn)?;
        let pid = child.id().expect("freshly spawned child has a pid") as i32;
        let identity = match proc::identity_of(pid) {
            Some(id) => id,
            // A fast child can exit before we fingerprint it, and a zombie
            // is not inspectable. Confirm the exit and record a sentinel
            // identity that can never match; a child that is alive yet
            // uninspectable stays a hard error.
            None => match child.try_wait() {
                Ok(Some(_)) => ProcIdentity {
                    pid,
                    start_tvsec: 0,
                    start_tvusec: 0,
                },
                _ => return Err(SessionError::IdentityUnavailable { pid }),
            },
        };

        let ctx = ChildContext {
            stdin: Arc::new(tokio::sync::Mutex::new(child.stdin.take())),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            kill_initiated: Arc::new(AtomicBool::new(false)),
        };
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);
        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        let lines = AcpLines {
            lines: BufReader::new(stdout).lines(),
            ctx: ctx.clone(),
        };
        let reader_fut = reader(lines, ctx.clone(), events_tx.clone());
        let reader_eof = Arc::clone(&stdout_eof);
        let stdout_task = tokio::spawn(async move {
            reader_fut.await;
            // EOF (or read error): with the process still alive this is
            // provable stream death — the attention monitor's `failed`
            // input (spec 01).
            reader_eof.store(true, Ordering::SeqCst);
        });

        let ring: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
        let ring_writer = Arc::clone(&ring);
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut ring = ring_writer.lock().unwrap();
                ring.extend(&buf[..n]);
                while ring.len() > STDERR_RING_CAP {
                    ring.pop_front();
                }
            }
        });

        // Supervisor: reap the leader, stop the side tasks, drain both
        // readers, then emit the terminal Exited event and publish the
        // exit facts. That order is the contract `Exited` is last.
        let at_exit: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let super_at_exit = Arc::clone(&at_exit);
        let super_exited = Arc::clone(&exited_flag);
        tokio::spawn(async move {
            let status = child.wait().await;
            super_exited.store(true, Ordering::SeqCst);
            for task in super_at_exit.lock().unwrap().drain(..) {
                task.abort();
            }
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let facts: ExitFacts = match &status {
                Ok(s) => (s.code(), s.signal()),
                Err(_) => (None, None),
            };
            let stderr_tail = {
                let mut ring = ring.lock().unwrap();
                String::from_utf8_lossy(ring.make_contiguous()).into_owned()
            };
            let _ = events_tx
                .send(SessionEvent::Exited {
                    code: facts.0,
                    signal: facts.1,
                    stderr_tail,
                })
                .await;
            let _ = exit_tx.send(Some(facts));
        });

        Ok((
            Self {
                identity,
                pgid: pid,
                ctx,
                exit_rx,
                stdout_eof,
                exited_flag,
                at_exit,
            },
            events_rx,
        ))
    }

    /// Stop `task` when the leader is reaped.
    pub fn abort_at_exit(&self, task: tokio::task::JoinHandle<()>) {
        self.at_exit.lock().unwrap().push(task);
    }

    pub fn identity(&self) -> ProcIdentity {
        self.identity
    }

    pub fn stdin(&self) -> &SharedStdin {
        &self.ctx.stdin
    }

    pub fn last_activity(&self) -> Instant {
        *self.ctx.last_activity.lock().unwrap()
    }

    pub fn watch(&self) -> SessionWatch {
        SessionWatch::new(
            self.identity,
            Arc::clone(&self.stdout_eof),
            Arc::clone(&self.exited_flag),
        )
    }

    pub fn exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }

    async fn await_exit(&self, window: Duration) -> bool {
        let mut rx = self.exit_rx.clone();
        let waited = tokio::time::timeout(window, async {
            loop {
                if rx.borrow_and_update().is_some() {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await;
        matches!(waited, Ok(true))
    }

    /// SIGTERM, then SIGKILL, then one bounded straggler scan — each step
    /// verified (spec 01 §Kill). Nothing here reports success on a
    /// delivered signal alone.
    pub async fn kill(&self) -> Result<KillReport, SessionError> {
        self.ctx.kill_initiated.store(true, Ordering::SeqCst);
        let already_exited = self.exited();
        let mut escalated = false;

        if !already_exited {
            // Group signals are safe without a pid-reuse guard: while any
            // member lives the pgid stays reserved by the kernel; once the
            // group is empty, killpg is ESRCH — a no-op.
            let _ = process::killpg(self.pgid, libc::SIGTERM);
            if !self.await_exit(KILL_GRACE).await {
                escalated = true;
                let _ = process::killpg(self.pgid, libc::SIGKILL);
                if !self.await_exit(KILL_VERIFY_WINDOW).await {
                    return Err(SessionError::KillUnproven {
                        remaining: vec![self.identity.pid],
                    });
                }
            }
        }

        if process::await_group_empty(self.pgid, KILL_VERIFY_WINDOW).await {
            return Ok(KillReport {
                escalated,
                already_exited,
                stragglers_killed: Vec::new(),
            });
        }

        // Pids are fresh from the scan, which is the guard for signalling
        // them directly rather than through the group.
        let _ = process::killpg(self.pgid, libc::SIGKILL);
        let stragglers = process::scan_group_pids(self.pgid)
            .await
            .unwrap_or_default();
        for &pid in &stragglers {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        if process::await_group_empty(self.pgid, KILL_VERIFY_WINDOW).await {
            Ok(KillReport {
                escalated,
                already_exited,
                stragglers_killed: stragglers,
            })
        } else {
            Err(SessionError::KillUnproven {
                remaining: stragglers,
            })
        }
    }
}
