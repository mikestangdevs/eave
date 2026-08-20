//! Native Claude Code adapter: spawn the `claude` CLI in stream-JSON mode,
//! parse its output, own its process group, kill it with proof of exit.
//!
//! Flag set follows the proven native path in waku
//! (waku-core/src/driver/claude.rs, research: waku.md §2), including the
//! caller-chosen `--session-id` so the resume cursor exists before the
//! first turn.
//!
//! Permission posture (Slice K): the CLI is spawned with
//! `--permission-prompt-tool stdio` and **no** `--permission-mode`, which
//! is the only combination that makes it ASK. Dropping the flag entirely
//! does not produce a prompt — it produces a silent auto-deny
//! (`system/permission_denied`, capture A, 2026-08-18); `--permission-mode
//! acceptEdits` produces zero questions at all (capture H). Because the
//! flag is undocumented in `--help` for 2.1.226, the posture is VERIFIED
//! from the CLI's own `init.permissionMode` rather than assumed, and a
//! session that cannot prove it asks is refused (spec 04 §Permissions).

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
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};
use crate::stream::ClaudeStream;
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

const STDERR_RING_CAP: usize = 4096;
const EVENT_CHANNEL_CAP: usize = 256;

type ExitFacts = (Option<i32>, Option<i32>);

struct Shared {
    stream: Mutex<ClaudeStream>,
    last_activity: Mutex<Instant>,
    /// The CLI announced itself at least once. A process that dies without
    /// this never got far enough to state its posture — Slice K's
    /// flag-rejected case (spec 04 §Permissions).
    saw_init: AtomicBool,
    /// Set at kill initiation; the reader drops all protocol input from then
    /// on — descendants holding inherited pipes may emit stale lines after
    /// the leader dies (spec 01 §Kill; research: bb.md pain points).
    kill_initiated: AtomicBool,
}

type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

pub struct ClaudeCodeSession {
    identity: ProcIdentity,
    pgid: i32,
    /// Shared because two writers exist (Slice K): the engine answering an
    /// approval, and the reader task auto-refusing a dialog the moment one
    /// arrives. Serialized by the mutex — a control frame can never
    /// interleave with a user message mid-line.
    stdin: SharedStdin,
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
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
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
                // Slice K: route permission questions to us on stdout
                // instead of auto-denying them. Undocumented but accepted
                // (2.1.226); its absence is caught below, never ignored.
                "--permission-prompt-tool",
                "stdio",
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
        // Slice P2: the session's own model and flags, after eave's. Absent
        // model = no `--model` at all, which is not the same as an empty
        // one. `extra_args` is verbatim and last, and has already been
        // filtered of anything that would undo the posture above
        // (`session::conflicts_with_eave_flags`) at the engine boundary —
        // this is a spawn, not a policy point.
        if let Some(model) = &cfg.model {
            cmd.args(["--model", model]);
        }
        for arg in &cfg.extra_args {
            cmd.arg(arg);
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

        let stdin: SharedStdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let shared = Arc::new(Shared {
            stream: Mutex::new(ClaudeStream::new()),
            last_activity: Mutex::new(Instant::now()),
            saw_init: AtomicBool::new(false),
            kill_initiated: AtomicBool::new(false),
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        let reader_shared = Arc::clone(&shared);
        let reader_tx = events_tx.clone();
        let reader_eof = Arc::clone(&stdout_eof);
        let reader_stdin = Arc::clone(&stdin);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                *reader_shared.last_activity.lock().unwrap() = Instant::now();
                if reader_shared.kill_initiated.load(Ordering::SeqCst) {
                    continue;
                }
                let events = reader_shared.stream.lock().unwrap().parse_line(&line);
                for event in events {
                    // Slice K amendment 1: the posture is verified, never
                    // assumed. A CLI that reports a mode which does not ask
                    // is refused here, at the first frame it sends.
                    if let SessionEvent::Init {
                        permission_mode, ..
                    } = &event
                    {
                        reader_shared.saw_init.store(true, Ordering::SeqCst);
                        if let Some(refusal) = posture_refusal(permission_mode.as_deref()) {
                            let _ = reader_tx.send(event).await;
                            let _ = reader_tx.send(refusal).await;
                            continue;
                        }
                    }
                    // Slice K amendment 2: a dialog eave cannot present is
                    // answered with a stated refusal immediately — silence
                    // would park the session on a question nobody can see.
                    if let SessionEvent::DialogRefused { request_id, dialog } = &event {
                        let frame = refusal_frame(request_id, dialog);
                        write_line(&reader_stdin, &frame).await;
                    }
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
        let super_shared = Arc::clone(&shared);
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
            // Slice K amendment 1, the flag-rejected case: a CLI that
            // never announced itself and complained about the permission
            // flag on stderr could not have asked us anything. State it as
            // a refusal so the transcript says WHY, instead of leaving a
            // bare non-zero exit for the operator to decode.
            if !super_shared.saw_init.load(Ordering::SeqCst)
                && stderr_tail.contains("permission-prompt-tool")
            {
                let _ = events_tx
                    .send(SessionEvent::PostureRefused {
                        mode: "unknown".into(),
                        reason:
                            "the agent CLI rejected --permission-prompt-tool, so eave cannot be \
                             asked to approve anything; eave will not run a session that writes \
                             without review"
                                .into(),
                    })
                    .await;
            }
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
        let line = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": text }],
            },
        });
        write_frame(&self.stdin, &line.to_string()).await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.stream.lock().unwrap().pending_approvals()
    }

    /// Answer one permission question (Slice K). The frame names the same
    /// `request_id` the CLI asked with; the CLI resumes the tool call on
    /// `allow` and hands the operator's `message` to the model verbatim as
    /// the tool result on `deny` (capture D, 2026-08-18).
    ///
    /// `Ok(false)` means the question was not open — already answered, or
    /// withdrawn by the provider under the operator's click. That is a
    /// race, not an error, and it must never reach the pipe: a stale
    /// decision would be ignored by the CLI and journaled by us as if it
    /// had landed.
    async fn respond_to_approval(
        &mut self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, SessionError> {
        if !self
            .shared
            .stream
            .lock()
            .unwrap()
            .approval_is_open(request_id)
        {
            return Ok(false);
        }
        let response = match decision {
            ApprovalDecision::Allow => serde_json::json!({ "behavior": "allow" }),
            ApprovalDecision::Deny { message } => serde_json::json!({
                "behavior": "deny",
                "message": message,
            }),
        };
        let frame = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            },
        });
        write_frame(&self.stdin, &frame.to_string()).await?;
        // Only now is the question closed: a failed write leaves it open so
        // the operator can try again rather than facing a dead card.
        self.shared
            .stream
            .lock()
            .unwrap()
            .resolve_approval(request_id);
        Ok(true)
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
            Err(SessionError::KillUnproven {
                remaining: stragglers,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Permission plumbing (Slice K)

/// Modes that provably ASK. Everything else is refused, including a mode
/// this build has never heard of and a CLI that named none at all: the
/// question "does this posture review writes?" has no safe default, and a
/// wrong guess here is unreviewed writes on someone's machine (spec 04
/// §Permissions, amendment 1).
///
/// `default` is the mode you get by passing no `--permission-mode` at all,
/// and it is the one the capture proves asks. `manual` is accepted on the
/// command line and the CLI reports it back AS `default` (capture M), so it
/// is listed for the day that stops being true.
const ASKING_MODES: [&str; 2] = ["default", "manual"];

fn posture_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    match mode {
        Some(m) if ASKING_MODES.contains(&m) => None,
        Some(m) => Some(SessionEvent::PostureRefused {
            mode: m.to_string(),
            reason: format!(
                "the agent CLI is running in permission mode {m:?}, which never asks \
                 before it writes; eave refuses to supervise a session it cannot review"
            ),
        }),
        None => Some(SessionEvent::PostureRefused {
            mode: "unstated".into(),
            reason: "the agent CLI did not state a permission mode, so eave cannot prove it \
                     will ask before it writes"
                .into(),
        }),
    }
}

/// The stated refusal for a dialog request eave cannot present yet
/// (amendment 2). Answered immediately by the reader task — the model is
/// told plainly, and the turn continues instead of parking forever.
fn refusal_frame(request_id: &str, kind: &str) -> String {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": format!(
                "eave cannot present {kind} dialogs to the operator yet. Do not retry this \
                 tool; ask in your reply text instead and the operator will answer there."
            ),
        },
    })
    .to_string()
}

/// Write one ndjson frame to the child's stdin under the shared lock.
async fn write_frame(stdin: &SharedStdin, line: &str) -> Result<(), SessionError> {
    let mut guard = stdin.lock().await;
    let pipe = guard.as_mut().ok_or(SessionError::StdinClosed)?;
    let mut payload = String::with_capacity(line.len() + 1);
    payload.push_str(line);
    payload.push('\n');
    pipe.write_all(payload.as_bytes()).await?;
    pipe.flush().await?;
    Ok(())
}

/// Best-effort write from the reader task, which has no one to report to:
/// a closed pipe means the child is already going away.
async fn write_line(stdin: &SharedStdin, line: &str) {
    let _ = write_frame(stdin, line).await;
}
