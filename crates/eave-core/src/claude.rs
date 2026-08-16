//! Native Claude Code adapter: spawn the `claude` CLI in stream-JSON mode,
//! parse its output, own its process group, kill it with proof of exit.
//!
//! Flag set follows the proven native path in waku
//! (waku-core/src/driver/claude.rs, research: waku.md §2), including the
//! caller-chosen `--session-id` so the resume cursor exists before the
//! first turn. v0 permission posture is `acceptEdits`
//! (spec 04 §Permissions — dev-loop only, blocked for any public build).

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::events::SessionEvent;
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{AgentSession, KillReport, SessionError, SessionWatch, SpawnConfig};
use crate::stream::ClaudeStream;
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

const STDERR_RING_CAP: usize = 4096;
const EVENT_CHANNEL_CAP: usize = 256;

type ExitFacts = (Option<i32>, Option<i32>);

struct Shared {
    stream: Mutex<ClaudeStream>,
    last_activity: Mutex<Instant>,
    /// Set at kill initiation; the reader drops all protocol input from then
    /// on — descendants holding inherited pipes may emit stale lines after
    /// the leader dies (spec 01 §Kill; research: bb.md pain points).
    kill_initiated: AtomicBool,
}

pub struct ClaudeCodeSession {
    identity: ProcIdentity,
    pgid: i32,
    stdin: Option<ChildStdin>,
    shared: Arc<Shared>,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    /// OS-fact flags for the attention monitor (spec 01 `failed`): stdout
    /// EOF and exit, maintained by the reader/supervisor tasks.
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
}

impl ClaudeCodeSession {
    /// Spawn one CLI process in its own process group with sanitized env and
    /// piped stdio (spec 01 §Spawn). Returns the session handle and its
    /// event stream; `SessionEvent::Exited` is always the final event.
    pub fn spawn(
        cfg: &SpawnConfig,
    ) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let mut cmd = Command::new(&cfg.program);
        cmd.current_dir(&cfg.cwd)
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--replay-user-messages",
                "--permission-mode",
                "acceptEdits",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if cfg.resume {
            cmd.args(["--resume", &cfg.session_id]);
        } else {
            cmd.args(["--session-id", &cfg.session_id]);
        }
        let eave_vars: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("EAVE_"))
            .collect();
        for var in eave_vars {
            cmd.env_remove(var);
        }
        for (k, v) in &cfg.extra_env {
            cmd.env(k, v);
        }
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
            // is not inspectable via proc_pidinfo. That is not an error:
            // confirm the exit and record a sentinel identity that can
            // never match (start_time 0 — no live process predates 1970),
            // so the guard refuses the pid forever. Exit facts still flow
            // through the supervisor. A child that is alive yet
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

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let shared = Arc::new(Shared {
            stream: Mutex::new(ClaudeStream::new()),
            last_activity: Mutex::new(Instant::now()),
            kill_initiated: AtomicBool::new(false),
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        let reader_shared = Arc::clone(&shared);
        let reader_tx = events_tx.clone();
        let reader_eof = Arc::clone(&stdout_eof);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                *reader_shared.last_activity.lock().unwrap() = Instant::now();
                if reader_shared.kill_initiated.load(Ordering::SeqCst) {
                    continue;
                }
                let events = reader_shared.stream.lock().unwrap().parse_line(&line);
                for event in events {
                    // Receiver gone just means nobody is listening; keep
                    // draining so the child never blocks on a full pipe.
                    let _ = reader_tx.send(event).await;
                }
            }
            // EOF (or read error): with the process still alive this is
            // provable stream death — the attention monitor's `failed`
            // input (spec 01). On a normal exit the exit flag lands within
            // moments and wins.
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

        // Supervisor: reap the leader, drain both readers, then emit the
        // terminal Exited event and publish the exit facts.
        let super_exited = Arc::clone(&exited_flag);
        tokio::spawn(async move {
            let status = child.wait().await;
            // Set before draining the readers so a monitor tick between
            // stdout-EOF and this flag sees the exit promptly.
            super_exited.store(true, Ordering::SeqCst);
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
                stdin,
                shared,
                exit_rx,
                stdout_eof,
                exited_flag,
            },
            events_rx,
        ))
    }

    async fn await_exit(&self, window: std::time::Duration) -> bool {
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

    fn exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }
}

#[async_trait]
impl AgentSession for ClaudeCodeSession {
    fn identity(&self) -> ProcIdentity {
        self.identity
    }

    fn provider_session_id(&self) -> Option<String> {
        self.shared
            .stream
            .lock()
            .unwrap()
            .session_id()
            .map(str::to_string)
    }

    fn in_flight_tools(&self) -> usize {
        self.shared.stream.lock().unwrap().in_flight()
    }

    fn last_activity(&self) -> Instant {
        *self.shared.last_activity.lock().unwrap()
    }

    fn watch(&self) -> SessionWatch {
        SessionWatch::new(
            self.identity,
            Arc::clone(&self.stdout_eof),
            Arc::clone(&self.exited_flag),
        )
    }

    async fn send_user_message(&mut self, text: &str) -> Result<(), SessionError> {
        let stdin = self.stdin.as_mut().ok_or(SessionError::StdinClosed)?;
        let line = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": text }],
            },
        });
        let mut payload = line.to_string();
        payload.push('\n');
        stdin.write_all(payload.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.shared.kill_initiated.store(true, Ordering::SeqCst);
        let already_exited = self.exited();
        let mut escalated = false;

        if !already_exited {
            // Group signals are safe without a pid-reuse guard: while any
            // member lives, the pgid stays reserved by the kernel; once the
            // group is empty, killpg is ESRCH — a no-op. The (pid,
            // start_time) guard matters for individual pids (below).
            let _ = process::killpg(self.pgid, libc::SIGTERM);
            if !self.await_exit(KILL_GRACE).await {
                escalated = true;
                let _ = process::killpg(self.pgid, libc::SIGKILL);
                if !self.await_exit(KILL_VERIFY_WINDOW).await {
                    // Leader survived SIGKILL (uninterruptible state) —
                    // exit unproven, never reported killed on hope.
                    return Err(SessionError::KillUnproven {
                        remaining: vec![self.identity.pid],
                    });
                }
            }
        }

        // Leader reaped; now prove the whole group is gone.
        if process::await_group_empty(self.pgid, KILL_VERIFY_WINDOW).await {
            return Ok(KillReport {
                escalated,
                already_exited,
                stragglers_killed: Vec::new(),
            });
        }

        // One bounded straggler scan (spec 01 §Kill step 3). Pids are fresh
        // from the scan, which is the guard for signalling them directly.
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
            Err(SessionError::KillUnproven { remaining: stragglers })
        }
    }
}
