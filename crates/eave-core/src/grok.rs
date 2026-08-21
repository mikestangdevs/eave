//! Native Grok adapter: spawn `grok agent stdio`, speak ACP (Agent Client
//! Protocol) JSON-RPC over its pipes, own its process group, kill it with
//! proof of exit.
//!
//! # Why ACP and not the headless printer
//!
//! Grok 1.0.5 has **two** machine-readable surfaces, and this adapter uses
//! the second one deliberately:
//!
//! - `grok -p <prompt> --output-format streaming-json|json` is a real
//!   headless mode, but `-p` is documented as "Single-turn prompt. Prints
//!   the response to stdout and exits". One turn, then gone: it cannot
//!   carry [`AgentSession::send_user_message`], and it has no channel on
//!   which eave could answer a permission question.
//! - `grok agent stdio` is a full **bidirectional** ACP v1 agent
//!   (`agentCapabilities.loadSession: true`, `sessionCapabilities`
//!   list/resume/close, verified 2026-08-20). It is multi-turn, it
//!   announces a session id, it can be resumed, and — the point of the
//!   whole slice — it asks the *client* for permission before it runs a
//!   tool.
//!
//! Nothing here scrapes a TUI. The "Grok Build TUI" framing in `--help` is
//! the default entry point, not the only one.
//!
//! # Permission posture (the part that gates everything)
//!
//! eave's rule is that a session must be able to PROVE it asks before it
//! writes, and Slice K's Claude proof (`--permission-prompt-tool stdio`
//! plus a verified `init.permissionMode`) transfers to nothing else. The
//! Grok equivalent, established empirically on 2026-08-20:
//!
//! - Grok's ACP path **asks by default**. With a stock config it sends
//!   `session/request_permission` to the client before a write, offering
//!   `allow_always` / `allow_once` / `reject_once`. Answering
//!   `reject-once` provably stops the tool (the file was never created)
//!   and ends the turn with `stopReason: "cancelled"`,
//!   `cancellationCategory: "PermissionRejected"`.
//! - **But that default is silently overridable by ambient operator
//!   config.** `~/.grok/config.toml` `[ui] permission_mode =
//!   "always-approve"` makes the same session run with **zero** permission
//!   requests — the tool call is auto-resolved internally
//!   (`pending_interaction` → `interaction_resolved`, no client round
//!   trip). There is no CLI flag on `grok agent stdio` that forces asking
//!   back on, so eave cannot *make* it ask.
//! - What eave can do is **refuse to run when it is not asking**, and that
//!   is verifiable: the session's own `_x.ai/sessions/changed` record
//!   carries `yolo: true|false`, and it arrives before the first
//!   `tool_call`. `yolo: true` = auto-approve = refused
//!   ([`posture_refusal`]).
//!
//! Fail-closed by construction, exactly like Slice K: a session whose
//! posture is never stated at all is refused at the first tool call rather
//! than assumed benign ([`GrokProto::note_tool_call`]). `--always-approve`
//! is never passed, and `--no-leader` is always passed — see [`spawn`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::events::{cap_payload, SessionEvent};
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

const STDERR_RING_CAP: usize = 4096;
const EVENT_CHANNEL_CAP: usize = 256;

/// The ACP version eave speaks. Grok 1.0.5 answers `initialize` with
/// `protocolVersion: 1`.
const ACP_PROTOCOL_VERSION: i64 = 1;

/// JSON-RPC ids eave owns for the two handshake calls. Prompt ids start
/// above them and only ever increase, so a late response can always be
/// attributed.
const RPC_INITIALIZE: i64 = 1;
const RPC_SESSION: i64 = 2;
const RPC_PROMPT_BASE: i64 = 16;

type ExitFacts = (Option<i32>, Option<i32>);
type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// One permission question Grok is parked on, and the two facts needed to
/// answer it: the JSON-RPC id it asked with, and the option ids it offered.
/// The option ids are **the provider's own strings**, captured from the
/// request rather than hardcoded — Grok varies them by tool kind
/// (`allow-edits-session` for an edit, and nothing guarantees a Bash call
/// spells its options the same way).
struct PendingPermission {
    rpc_id: Value,
    allow: Option<String>,
    reject: Option<String>,
}

/// ACP session state and the outstanding-tool set, kept together because
/// both are read by the trait methods off the reader task.
#[derive(Default)]
pub struct GrokProto {
    session_id: Option<String>,
    outstanding: HashSet<String>,
    pending: HashMap<String, PendingPermission>,
    /// The posture the session reports (`yolo`): `Some(false)` = it asks,
    /// `Some(true)` = it auto-approves, `None` = it has not said yet.
    yolo: Option<bool>,
    /// `Init` has been emitted. It is emitted **once**, at the moment the
    /// posture is first known, so the event carries both facts eave needs
    /// rather than a session id now and a posture later.
    announced: bool,
    next_prompt_id: i64,
}

impl GrokProto {
    fn new() -> Self {
        Self {
            next_prompt_id: RPC_PROMPT_BASE,
            ..Default::default()
        }
    }

    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    pub fn pending_approvals(&self) -> usize {
        self.pending.len()
    }

    pub fn approval_is_open(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn next_prompt_id(&mut self) -> i64 {
        let id = self.next_prompt_id;
        self.next_prompt_id += 1;
        id
    }
}

pub struct GrokSession {
    identity: ProcIdentity,
    pgid: i32,
    /// Shared because two writers exist: the engine answering an approval
    /// or sending a message, and the reader task driving the ACP
    /// handshake. Serialized by the mutex so a JSON-RPC frame can never
    /// interleave with another mid-line.
    stdin: SharedStdin,
    shared: Arc<Shared>,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    /// Publishes the ACP session id once `session/new` (or `session/load`)
    /// returns. `send_user_message` waits on this rather than racing the
    /// handshake.
    ready_rx: watch::Receiver<Option<SessionReady>>,
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
}

struct Shared {
    proto: Mutex<GrokProto>,
    last_activity: Mutex<Instant>,
    /// Set at kill initiation; the reader drops all protocol input from
    /// then on — descendants holding inherited pipes may emit stale lines
    /// after the leader dies (spec 01 §Kill).
    kill_initiated: AtomicBool,
}

impl GrokSession {
    /// Spawn one `grok agent stdio` process in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let mut cmd = Command::new(&cfg.program);
        cmd.current_dir(&cfg.cwd);
        cmd.args(spawn_args(cfg));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

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
            // Same reasoning as the Claude adapter: a fast child can exit
            // before we fingerprint it, and a zombie is not inspectable.
            // Confirm the exit and record a sentinel identity that can
            // never match; a child that is alive yet uninspectable stays a
            // hard error.
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
            proto: Mutex::new(GrokProto::new()),
            last_activity: Mutex::new(Instant::now()),
            kill_initiated: AtomicBool::new(false),
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = watch::channel(None);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        // Open the ACP handshake. Everything else is driven by what comes
        // back, in the reader task.
        let resume_id = cfg.resume.then(|| cfg.session_id.clone());
        let session_cwd = cfg.cwd.display().to_string();
        let handshake_stdin = Arc::clone(&stdin);
        tokio::spawn(async move {
            write_line(&handshake_stdin, &initialize_frame()).await;
        });

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
                let (events, frames) = {
                    let mut proto = reader_shared.proto.lock().unwrap();
                    proto.handle_line(&line, resume_id.as_deref(), &session_cwd, &ready_tx)
                };
                for frame in frames {
                    write_line(&reader_stdin, &frame).await;
                }
                for event in events {
                    // Receiver gone just means nobody is listening; keep
                    // draining so the child never blocks on a full pipe.
                    let _ = reader_tx.send(event).await;
                }
            }
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

        // Supervisor: reap the leader, drain both readers, then emit the
        // terminal Exited event and publish the exit facts.
        let super_exited = Arc::clone(&exited_flag);
        tokio::spawn(async move {
            let status = child.wait().await;
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
                ready_rx,
                stdout_eof,
                exited_flag,
            },
            events_rx,
        ))
    }
}

/// The argv eave spawns Grok with, as a pure function so the safety
/// properties below are testable without a process.
///
/// Two of these are contracts, not preferences:
///
/// - **`--no-leader`.** `grok agent` will otherwise attach to a shared
///   leader process (`[cli] use_leader` in config.toml, or `--leader`) and
///   run the real work THERE — outside the process group eave setsid'd and
///   signals. eave would report a verified kill while the agent kept
///   running in a process it never owned. Passing it explicitly means
///   operator config cannot move the work out from under us.
/// - **No `--always-approve`, ever.** The whole point of the slice is that
///   eave is the thing being asked.
///
/// `-m` goes before the subcommand because it is an option of `grok
/// agent`, not of `stdio`. An absent model means no `-m` at all, which is
/// not the same as an empty one (Slice P2).
fn spawn_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut args = vec!["agent".to_string(), "--no-leader".to_string()];
    if let Some(model) = &cfg.model {
        args.push("-m".into());
        args.push(model.clone());
    }
    args.push("stdio".into());
    // Verbatim and last, already filtered of anything that would undo the
    // posture at the engine boundary (`session::conflicts_with_eave_flags`).
    args.extend(cfg.extra_args.iter().cloned());
    args
}

impl GrokSession {
    /// Wait for the ACP handshake to name a session.
    ///
    /// Deliberately **unbounded**: a handshake that never completes is a
    /// hung agent, and eave does not time out into a decision — the
    /// attention clocks classify it and the operator decides (spec 01).
    /// The wait ends by itself when the child dies, because the reader
    /// task drops the sender.
    async fn await_session(&self) -> Result<String, SessionError> {
        let mut rx = self.ready_rx.clone();
        loop {
            let settled = rx.borrow_and_update().clone();
            match settled {
                Some(Ok(id)) => return Ok(id),
                // The handshake failed for a stated reason. Report THAT,
                // not a generic broken pipe: "grok could not resume
                // session X" is the sentence the operator needs.
                Some(Err(reason)) => {
                    return Err(SessionError::Io(std::io::Error::other(reason)));
                }
                None => {}
            }
            if rx.changed().await.is_err() {
                return Err(SessionError::StdinClosed);
            }
        }
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
impl crate::provider::AgentAdapter for GrokSession {
    const PROVIDER_ID: &'static str = "grok";

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "grok.rs `posture_refusal`: the ACP session's own `yolo` field is read off the \
                   wire before the first tool call, and a session that reports `yolo: true` emits \
                   PostureRefused and is killed. Slice AV verified denial is not advisory — a \
                   denied write left the directory empty and the turn ended PermissionRejected. \
                   NOTE: eave cannot FORCE Grok to ask (`grok agent stdio` takes no \
                   --permission-mode), so this is verify-and-refuse, not verify-and-correct: a \
                   machine with `permission_mode = \"always-approve\"` in ~/.grok/config.toml has \
                   every session refused.",
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        GrokSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for GrokSession {
    fn identity(&self) -> ProcIdentity {
        self.identity
    }

    fn provider_session_id(&self) -> Option<String> {
        self.shared
            .proto
            .lock()
            .unwrap()
            .session_id()
            .map(str::to_string)
    }

    fn in_flight_tools(&self) -> usize {
        self.shared.proto.lock().unwrap().in_flight()
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
        let session_id = self.await_session().await?;
        let id = self.shared.proto.lock().unwrap().next_prompt_id();
        write_frame(&self.stdin, &prompt_frame(id, &session_id, text)).await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.proto.lock().unwrap().pending_approvals()
    }

    /// Answer one permission question by naming one of the option ids Grok
    /// itself offered.
    ///
    /// `Ok(false)` means the question was not open — already answered, or
    /// withdrawn under the operator's click. That is a race, not an error,
    /// and it must never reach the pipe: ACP would reject a second response
    /// to the same JSON-RPC id, and eave would have journaled a decision
    /// that never landed.
    ///
    /// **The deny message is delivered as a follow-up prompt, not inside
    /// the rejection.** ACP's rejection outcome is an option id and
    /// nothing else — there is no field an operator's sentence can travel
    /// in, and a denied turn ends immediately (`stopReason: "cancelled"`),
    /// so there is no tool result to attach it to either. Grok's own label
    /// for that option is "No, and tell Grok what to do differently",
    /// which is precisely this two-step: reject, then say why. Sending it
    /// as the next prompt is therefore the vendor's intended flow rather
    /// than an invention, and it is the only way "denied — commit first"
    /// stays a different instruction from "denied". An empty message sends
    /// nothing.
    async fn respond_to_approval(
        &mut self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, SessionError> {
        let (rpc_id, option_id) = {
            let proto = self.shared.proto.lock().unwrap();
            let Some(pending) = proto.pending.get(request_id) else {
                return Ok(false);
            };
            match decision {
                // An allow is never guessed. If Grok offered no
                // `allow_once`, eave has nothing safe to say — picking
                // some other option could grant more than the operator
                // clicked (`allow_always` ends review for the whole
                // session), so the question stays open and answerable.
                ApprovalDecision::Allow => match &pending.allow {
                    Some(option) => (pending.rpc_id.clone(), Some(option.clone())),
                    None => return Ok(false),
                },
                // A deny is ALWAYS expressible: the named reject option if
                // there is one, and ACP's own `cancelled` outcome if there
                // is not. Refusing must never fail for want of vocabulary.
                ApprovalDecision::Deny { .. } => (pending.rpc_id.clone(), pending.reject.clone()),
            }
        };
        write_frame(
            &self.stdin,
            &permission_response_frame(&rpc_id, option_id.as_deref()),
        )
        .await?;
        // Only now is the question closed: a failed write leaves it open so
        // the operator can try again rather than facing a dead card.
        self.shared.proto.lock().unwrap().pending.remove(request_id);

        if let ApprovalDecision::Deny { message } = decision {
            if !message.trim().is_empty() {
                let session_id = self.await_session().await?;
                let id = self.shared.proto.lock().unwrap().next_prompt_id();
                write_frame(&self.stdin, &prompt_frame(id, &session_id, message)).await?;
            }
        }
        Ok(true)
    }

    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.shared.kill_initiated.store(true, Ordering::SeqCst);
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
// ACP protocol plumbing

/// What the handshake produced: the ACP session id, or the reason there
/// will never be one. Carried through a watch channel so a caller blocked
/// in `send_user_message` gets the actual failure instead of hanging on a
/// session that was refused (e.g. `--resume` with an id Grok never issued).
type SessionReady = Result<String, String>;

impl GrokProto {
    /// Parse one JSON-RPC line into events for the engine and frames to
    /// write back. Never fails: malformed input becomes a `Malformed` event
    /// and parsing continues on the next line.
    fn handle_line(
        &mut self,
        line: &str,
        resume_id: Option<&str>,
        cwd: &str,
        ready: &watch::Sender<Option<SessionReady>>,
    ) -> (Vec<SessionEvent>, Vec<String>) {
        let line = line.trim();
        if line.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return (vec![malformed(line, err.to_string())], Vec::new()),
        };

        let method = v.get("method").and_then(Value::as_str);
        let has_id = v.get("id").is_some();
        match (method, has_id) {
            // Agent → client request: it wants an answer on the same id.
            (Some(m), true) => self.handle_request(m, &v, line),
            // Notification: session updates and vendor extensions.
            (Some(m), false) => (self.handle_notification(m, &v, line), Vec::new()),
            // Response to something eave asked.
            (None, true) => self.handle_response(&v, line, resume_id, cwd, ready),
            (None, false) => (
                vec![malformed(
                    line,
                    "neither request, notification, nor response".into(),
                )],
                Vec::new(),
            ),
        }
    }

    fn handle_response(
        &mut self,
        v: &Value,
        line: &str,
        resume_id: Option<&str>,
        cwd: &str,
        ready: &watch::Sender<Option<SessionReady>>,
    ) -> (Vec<SessionEvent>, Vec<String>) {
        let id = v.get("id").and_then(Value::as_i64);
        let error = v.get("error");
        match id {
            Some(RPC_INITIALIZE) => {
                if let Some(err) = error {
                    let reason = format!("grok rejected the ACP handshake: {err}");
                    let _ = ready.send(Some(Err(reason)));
                    return (
                        vec![SessionEvent::System {
                            subtype: "grok_initialize_failed".into(),
                            raw: line.to_string(),
                        }],
                        Vec::new(),
                    );
                }
                // Handshake accepted: open (or reload) the session.
                (Vec::new(), vec![session_frame(resume_id, cwd)])
            }
            Some(RPC_SESSION) => {
                if let Some(err) = error {
                    // The loud case that matters most: `--resume` with an
                    // id Grok never issued. Grok answers `FS_NOT_FOUND`
                    // rather than silently starting a fresh conversation,
                    // and eave passes that on rather than pretending the
                    // history is there.
                    let reason = match resume_id {
                        Some(id) => format!(
                            "grok could not resume session {id:?}: {err}. Grok issues its own \
                             session ids at session/new; an id eave chose is not one of them."
                        ),
                        None => format!("grok could not open a session: {err}"),
                    };
                    let _ = ready.send(Some(Err(reason)));
                    return (
                        vec![SessionEvent::System {
                            subtype: "grok_session_failed".into(),
                            raw: line.to_string(),
                        }],
                        Vec::new(),
                    );
                }
                // `session/new` names the session in its result;
                // `session/load` returns models/metadata and no id,
                // because the id is the one eave just asked to load.
                let session_id = v
                    .get("result")
                    .and_then(|r| r.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| resume_id.map(str::to_string));
                match session_id {
                    Some(id) => {
                        self.session_id = Some(id.clone());
                        let _ = ready.send(Some(Ok(id)));
                    }
                    None => {
                        let reason = "grok opened a session without naming it".to_string();
                        let _ = ready.send(Some(Err(reason.clone())));
                        return (vec![malformed(line, reason)], Vec::new());
                    }
                }
                (Vec::new(), Vec::new())
            }
            // A prompt turn's response. `turn_completed` already reported
            // the outcome as an event; this is the bookend and carries
            // nothing new.
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn handle_request(
        &mut self,
        method: &str,
        v: &Value,
        line: &str,
    ) -> (Vec<SessionEvent>, Vec<String>) {
        let rpc_id = v.get("id").cloned().unwrap_or(Value::Null);
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "session/request_permission" => {
                (vec![self.note_permission(&rpc_id, &params)], Vec::new())
            }
            // eave declares no filesystem or terminal capability at
            // `initialize`, precisely so that Grok performs its own I/O and
            // the permission gate above is the ONE place a write is
            // reviewed. A capability request arriving anyway is answered
            // with a plain JSON-RPC "method not found" and journaled
            // verbatim — never silently serviced, which would make eave the
            // thing performing the write it is supposed to be policing.
            _ => (
                vec![SessionEvent::Unknown {
                    event_type: method.to_string(),
                    raw: line.to_string(),
                }],
                vec![method_not_found_frame(&rpc_id, method)],
            ),
        }
    }

    /// One `session/request_permission`. The option ids are captured from
    /// the request rather than assumed: Grok labels them per tool kind, and
    /// answering with an id it did not offer is not an answer.
    ///
    /// **`allow_always` is captured but never chosen.** eave offers exactly
    /// two decisions (Slice K); a session-wide grant is a third, and
    /// handing it out silently under an "Allow" click would end review for
    /// every later tool call in the session. It travels in
    /// `suggestions_json` as what it is — the provider's own suggestion for
    /// how to stop being asked — for a grants slice that does not exist yet.
    fn note_permission(&mut self, rpc_id: &Value, params: &Value) -> SessionEvent {
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let meta = tool_call
            .get("_meta")
            .and_then(|m| m.get("x.ai/tool"))
            .cloned()
            .unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or(Value::Null);

        let pick = |kind: &str| -> Option<String> {
            options
                .as_array()?
                .iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|o| o.get("optionId").and_then(Value::as_str))
                .map(str::to_string)
        };

        // The request id eave answers with. Grok's own tool-call id is the
        // stable, meaningful choice — it is the same id the tool events
        // carry, so an approval card and its tool row name the same thing.
        let request_id = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| rpc_id.to_string());

        let raw_input = tool_call.get("rawInput").cloned().unwrap_or(Value::Null);
        let title = tool_call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool = meta
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&title)
            .to_string();
        let display_name = meta
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(&tool)
            .to_string();
        // The concrete path, preferring the one Grok itself resolved in
        // `_meta.input.path` over re-deriving it from the raw arguments.
        let path = meta
            .get("input")
            .and_then(|i| i.get("path"))
            .and_then(Value::as_str)
            .or_else(|| raw_input.get("file_path").and_then(Value::as_str))
            .or_else(|| raw_input.get("path").and_then(Value::as_str))
            .map(str::to_string);

        self.pending.insert(
            request_id.clone(),
            PendingPermission {
                rpc_id: rpc_id.clone(),
                allow: pick("allow_once"),
                reject: pick("reject_once"),
            },
        );

        SessionEvent::ApprovalRequested {
            request_id,
            tool,
            display_name,
            description: title,
            path,
            input_json: cap_payload(&raw_input.to_string()),
            suggestions_json: (!options.is_null()).then(|| options.to_string()),
        }
    }
}

impl GrokProto {
    fn handle_notification(&mut self, method: &str, v: &Value, line: &str) -> Vec<SessionEvent> {
        match method {
            // Grok emits session updates under BOTH the standard ACP
            // notification and its own vendor channel, and they are not
            // interchangeable in practice: `tool_call` arrives on
            // `session/update` while `turn_completed` arrives on
            // `_x.ai/session_notification` (capture 2026-08-20). Reading
            // only the standard one loses the end of every turn.
            "session/update" | "_x.ai/session_notification" => {
                let update = v
                    .get("params")
                    .and_then(|p| p.get("update"))
                    .cloned()
                    .unwrap_or(Value::Null);
                self.handle_update(&update, line)
            }
            // The posture channel. This is the only place Grok states
            // whether the session auto-approves, and it lands before the
            // first tool call.
            "_x.ai/sessions/changed" => {
                let mut out = Vec::new();
                let entries = v
                    .get("params")
                    .and_then(|p| p.get("upserted"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for entry in entries {
                    // Only our own session's posture counts: one agent
                    // process can report on sessions eave is not driving.
                    let names_us = match (
                        entry.get("sessionId").and_then(Value::as_str),
                        self.session_id.as_deref(),
                    ) {
                        (Some(id), Some(ours)) => id == ours,
                        _ => false,
                    };
                    if !names_us {
                        continue;
                    }
                    if let Some(yolo) = entry.get("yolo").and_then(Value::as_bool) {
                        out.extend(self.note_posture(yolo));
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn handle_update(&mut self, update: &Value, line: &str) -> Vec<SessionEvent> {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" => match update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
            {
                Some(text) => vec![SessionEvent::TextDelta {
                    text: text.to_string(),
                }],
                None => Vec::new(),
            },
            // Reasoning, not transcript. The Claude adapter surfaces only
            // assistant text too; recording chain-of-thought as if it were
            // the reply would put words in the agent's mouth.
            "agent_thought_chunk" => Vec::new(),
            "user_message_chunk" => match update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
            {
                Some(text) => vec![SessionEvent::UserEcho {
                    text: text.to_string(),
                }],
                None => Vec::new(),
            },
            "tool_call" => self.note_tool_call(update),
            "tool_call_update" => self.note_tool_update(update),
            "turn_completed" => {
                let stop = update
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                // `cancelled` is what a denied tool produces, and a denial
                // is the system working — not an error to flag.
                let is_error = stop == "error" || stop == "refusal";
                vec![SessionEvent::TurnCompleted {
                    subtype: stop,
                    is_error,
                }]
            }
            // Everything else is recorded verbatim rather than dropped, so
            // a Grok update this build has never heard of still reaches the
            // journal (same contract as the Claude adapter's `System`).
            other => vec![SessionEvent::System {
                subtype: other.to_string(),
                raw: line.to_string(),
            }],
        }
    }

    /// A tool call became outstanding — and the fail-closed backstop.
    ///
    /// If a tool is starting and the session has still not stated its
    /// posture, eave has no proof it will ever be asked, so it refuses
    /// here rather than letting the first write through on the assumption
    /// that silence means "asks". This is the Grok analogue of Slice K's
    /// "a CLI that named no mode at all is unproven, never probably fine".
    fn note_tool_call(&mut self, update: &Value) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        if !self.announced {
            out.extend(self.refuse_unstated_posture());
        }
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return out;
        };
        self.outstanding.insert(id.to_string());
        let meta = update
            .get("_meta")
            .and_then(|m| m.get("x.ai/tool"))
            .cloned()
            .unwrap_or(Value::Null);
        let title = update
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = meta
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&title)
            .to_string();
        let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
        out.push(SessionEvent::ToolUseStarted {
            id: id.to_string(),
            name,
            input: cap_payload(&input.to_string()),
        });
        out
    }

    /// A tool call resolved. Grok sends several `tool_call_update`s per
    /// call — title refinements and diffs — and only the ones carrying a
    /// terminal `status` close it.
    fn note_tool_update(&mut self, update: &Value) -> Vec<SessionEvent> {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let status = update.get("status").and_then(Value::as_str);
        let (is_error, done) = match status {
            Some("completed") => (false, true),
            Some("failed") => (true, true),
            _ => (false, false),
        };
        if !done {
            return Vec::new();
        }
        self.outstanding.remove(id);
        vec![SessionEvent::ToolUseFinished {
            id: id.to_string(),
            result: cap_payload(&flatten_content(update.get("content"))),
            is_error,
        }]
    }

    /// The session stated its posture. `Init` is emitted here — once, with
    /// both facts — and a session that auto-approves is refused on the
    /// spot.
    fn note_posture(&mut self, yolo: bool) -> Vec<SessionEvent> {
        if self.announced {
            return Vec::new();
        }
        self.announced = true;
        self.yolo = Some(yolo);
        let Some(session_id) = self.session_id.clone() else {
            return Vec::new();
        };
        let mode = if yolo { GROK_MODE_YOLO } else { GROK_MODE_ASKS };
        let mut out = vec![SessionEvent::Init {
            session_id,
            permission_mode: Some(mode.to_string()),
            // Empty is the honest answer, not a placeholder: this
            // provider has announced no command inventory to eave.
            // ACP's `availableCommands` is not read yet — see Not done.
            slash_commands: Vec::new(),
        }];
        if let Some(refusal) = posture_refusal(Some(mode)) {
            out.push(refusal);
        }
        out
    }

    fn refuse_unstated_posture(&mut self) -> Vec<SessionEvent> {
        self.announced = true;
        let mut out = Vec::new();
        if let Some(session_id) = self.session_id.clone() {
            out.push(SessionEvent::Init {
                session_id,
                permission_mode: None,
                // Empty is the honest answer, not a placeholder: this
                // provider has announced no command inventory to eave.
                // ACP's `availableCommands` is not read yet — see Not done.
                slash_commands: Vec::new(),
            });
        }
        out.extend(posture_refusal(None));
        out
    }
}

/// The posture string for a session that routes permission questions to
/// eave, and the one for a session that answers them itself. These are
/// eave's names for what `yolo` means, not strings Grok emits.
const GROK_MODE_ASKS: &str = "default";
const GROK_MODE_YOLO: &str = "always-approve";

/// Postures that provably ASK. Everything else is refused — including a
/// posture this build has never heard of and a session that stated none:
/// the question "does this review writes?" has no safe default, and a
/// wrong guess here is unreviewed writes on someone's machine (spec 04
/// §Permissions).
fn posture_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    match mode {
        Some(GROK_MODE_ASKS) => None,
        Some(GROK_MODE_YOLO) => Some(SessionEvent::PostureRefused {
            mode: GROK_MODE_YOLO.into(),
            reason: "this Grok session auto-approves every tool call, so eave would never be \
                     asked before it writes. The cause is ambient config — `[ui] permission_mode \
                     = \"always-approve\"` in ~/.grok/config.toml, or /always-approve in a \
                     previous session — and no command-line flag overrides it back on. Clear \
                     that setting and reopen the session."
                .into(),
        }),
        Some(other) => Some(SessionEvent::PostureRefused {
            mode: other.to_string(),
            reason: format!(
                "this Grok session reports permission posture {other:?}, which eave cannot prove \
                 asks before it writes"
            ),
        }),
        None => Some(SessionEvent::PostureRefused {
            mode: "unstated".into(),
            reason: "this Grok session started running tools without ever stating whether it \
                     asks before it writes, so eave cannot prove its writes are reviewable"
                .into(),
        }),
    }
}

/// Flatten an ACP tool-result `content` array to text. Grok mixes shapes:
/// `{type:"content", content:{type:"text", text}}` for output and
/// `{type:"diff", ...}` for edits. Text is taken where it exists; anything
/// else is kept as its own JSON so the record still says what happened
/// instead of silently becoming an empty string.
fn flatten_content(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(text) = item
            .get("content")
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
        {
            out.push(text.to_string());
        } else if let Some(text) = item.get("text").and_then(Value::as_str) {
            out.push(text.to_string());
        } else {
            out.push(item.to_string());
        }
    }
    out.join("\n")
}

fn malformed(line: &str, error: String) -> SessionEvent {
    const MALFORMED_LINE_CAP: usize = 512;
    let mut shown = line.to_string();
    if shown.len() > MALFORMED_LINE_CAP {
        shown.truncate(MALFORMED_LINE_CAP);
    }
    SessionEvent::Malformed { line: shown, error }
}

// ---------------------------------------------------------------------------
// Outbound frames

/// The `initialize` call that opens ACP.
///
/// **eave declares no filesystem and no terminal capability on purpose.**
/// With `fs.writeTextFile: true` Grok delegates its writes to the client —
/// verified 2026-08-20, the write arrived as an `fs/write_text_file`
/// request — which would make eave the process performing the very writes
/// it exists to supervise, and would put a second, silent path to disk
/// beside the permission gate. Declaring nothing keeps Grok's own tools
/// doing Grok's own work, with `session/request_permission` as the single
/// review point. The permission request still fires either way (verified
/// with capabilities off).
fn initialize_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": RPC_INITIALIZE,
        "method": "initialize",
        "params": {
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
            },
        },
    })
    .to_string()
}

/// Open a new session, or reload one Grok already knows.
///
/// Note what is NOT here: a caller-chosen session id. Grok issues its own
/// at `session/new` and ACP offers no way to propose one, so
/// `SpawnConfig::session_id` is honoured only in its `resume` role — see
/// the module docs on `provider_session_id`.
fn session_frame(resume_id: Option<&str>, cwd: &str) -> String {
    let (method, mut params) = match resume_id {
        Some(id) => ("session/load", json!({ "sessionId": id })),
        None => ("session/new", json!({})),
    };
    // The SESSION's cwd, passed explicitly. Reading the current process's
    // cwd here would name the engine's directory: the child was spawned
    // with `current_dir`, but this frame is built in eave's own process.
    params["cwd"] = json!(cwd);
    params["mcpServers"] = json!([]);
    json!({ "jsonrpc": "2.0", "id": RPC_SESSION, "method": method, "params": params }).to_string()
}

fn prompt_frame(id: i64, session_id: &str, text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        },
    })
    .to_string()
}

/// Answer one permission request. `Some(option)` selects an id Grok
/// offered; `None` is ACP's own `cancelled` outcome, which eave uses as
/// the deny of last resort when a request arrives without a
/// `reject_once` option — refusing is always expressible, even when the
/// provider forgot to offer a word for it.
fn permission_response_frame(rpc_id: &Value, option: Option<&str>) -> String {
    let outcome = match option {
        Some(id) => json!({ "outcome": "selected", "optionId": id }),
        None => json!({ "outcome": "cancelled" }),
    };
    json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "outcome": outcome } }).to_string()
}

fn method_not_found_frame(rpc_id: &Value, method: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "error": {
            "code": -32601,
            "message": format!(
                "eave declares no {method} capability; it does not act on the filesystem on the \
                 agent's behalf"
            ),
        },
    })
    .to_string()
}

/// Write one JSON-RPC frame to the child's stdin under the shared lock.
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

/// Best-effort write from the reader/handshake tasks, which have no one to
/// report to: a closed pipe means the child is already going away.
async fn write_line(stdin: &SharedStdin, line: &str) {
    let _ = write_frame(stdin, line).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every fixture below is a real line captured from `grok agent stdio`
    // 1.0.5 on 2026-08-20 (probe transcripts in the Slice AV report). Only
    // the model catalogue in SESSION_NEW is elided, because it is 2 KB of
    // pricing metadata this parser never reads.

    const SESSION_NEW: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","models":{"currentModelId":"grok-4.6"}}}"#;

    const SESSIONS_ASKS: &str = r#"{"jsonrpc":"2.0","method":"_x.ai/sessions/changed","params":{"upserted":[{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","title":null,"cwd":"/tmp/probe","isWorktree":false,"modelId":"grok-4.6","reasoningEffort":"high","yolo":false,"activity":"working","resident":true,"lastChangeUnixMs":1787256323207,"origin":{"kind":"local"}}],"removed":[]}}"#;

    const SESSIONS_YOLO: &str = r#"{"jsonrpc":"2.0","method":"_x.ai/sessions/changed","params":{"upserted":[{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","title":null,"cwd":"/tmp/probe","isWorktree":false,"modelId":"grok-4.6","reasoningEffort":"xhigh","yolo":true,"activity":"working","resident":true,"lastChangeUnixMs":1787256223005,"origin":{"kind":"local"}}],"removed":[]}}"#;

    const PERM_REQUEST: &str = r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"01a020c7-3034-77f1-8e9d-f54b94f0a721","toolCall":{"toolCallId":"call-adaf4213-2ff0-4064-a133-be3cf729551d-0","kind":"edit","title":"Write `/tmp/probe2/perm.txt`","rawInput":{"variant":"Write","file_path":"/tmp/probe2/perm.txt","content":"asked\n"},"_meta":{"x.ai/tool":{"version":1,"name":"write","kind":"write","namespace":"opencode","label":"Write","read_only":false,"input":{"path":"/tmp/probe2/perm.txt"}}}},"options":[{"optionId":"allow-edits-session","name":"Yes, allow all edits during this session","kind":"allow_always"},{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject-once","name":"No, and tell Grok what to do differently","kind":"reject_once"}]}}"#;

    const TOOL_CALL: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"tool_call","toolCallId":"call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0","title":"write","rawInput":{"file_path":"/tmp/probe1/hello.txt","content":"hi\n"},"_meta":{"x.ai/tool":{"version":1,"name":"write","kind":"write","namespace":"opencode","label":"Write","read_only":false}}}}}"#;

    const TOOL_DONE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0","status":"completed","content":[{"type":"diff","path":"/tmp/probe1/hello.txt","oldText":"","newText":"hi\n"}]}}}"#;

    const TOOL_REJECTED: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c8-9d94-7a30-8deb-83bc921adb2f","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-65253e28-bd82-438d-8019-0a7373dc4391-0","status":"failed","content":[{"type":"content","content":{"type":"text","text":"User rejected the execution for tool `write`"}}]}}}"#;

    const TURN_CANCELLED: &str = r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"sessionId":"01a020c8-9d94-7a30-8deb-83bc921adb2f","update":{"sessionUpdate":"turn_completed","prompt_id":"1d6acb54","stop_reason":"cancelled","usage":{"numTurns":1}},"_meta":{"cancellationCategory":"PermissionRejected"}}}"#;

    const MSG_CHUNK: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I'll"}}}}"#;

    const SESSION_ID: &str = "01a020c5-af30-7fb3-b886-bcaa75a72299";

    /// A parser plus the channel the handshake reports through.
    struct Harness {
        proto: GrokProto,
        ready_tx: watch::Sender<Option<SessionReady>>,
        ready_rx: watch::Receiver<Option<SessionReady>>,
    }

    impl Harness {
        fn new() -> Self {
            let (ready_tx, ready_rx) = watch::channel(None);
            Self {
                proto: GrokProto::new(),
                ready_tx,
                ready_rx,
            }
        }

        fn feed(&mut self, line: &str) -> (Vec<SessionEvent>, Vec<String>) {
            self.proto
                .handle_line(line, None, "/tmp/probe", &self.ready_tx)
        }

        fn feed_resuming(&mut self, line: &str, id: &str) -> (Vec<SessionEvent>, Vec<String>) {
            self.proto
                .handle_line(line, Some(id), "/tmp/probe", &self.ready_tx)
        }

        /// Drive the handshake to the point a real session has a name.
        fn opened() -> Self {
            let mut h = Self::new();
            h.feed(SESSION_NEW);
            h
        }

        fn ready(&self) -> Option<SessionReady> {
            self.ready_rx.borrow().clone()
        }
    }

    #[test]
    fn the_handshake_opens_a_session_and_then_names_it() {
        let mut h = Harness::new();
        // A successful `initialize` produces exactly one outbound frame:
        // the request to open a session.
        let (events, frames) = h.feed(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        assert!(events.is_empty(), "the handshake is not transcript");
        assert_eq!(frames.len(), 1);
        let sent: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(sent["method"], "session/new");
        assert_eq!(sent["params"]["cwd"], "/tmp/probe");

        h.feed(SESSION_NEW);
        assert_eq!(h.proto.session_id(), Some(SESSION_ID));
        assert_eq!(h.ready(), Some(Ok(SESSION_ID.to_string())));
    }

    /// Grok issues its own session ids, so resume is the ONE place
    /// `SpawnConfig::session_id` is honoured — and it goes out as
    /// `session/load`, not as a proposed id on a new session.
    #[test]
    fn a_resume_asks_grok_to_load_the_id_it_was_given() {
        let mut h = Harness::new();
        let (_, frames) = h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
            "01a020c7-d363-7a50-9f08-37e0cc12db3a",
        );
        let sent: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(sent["method"], "session/load");
        assert_eq!(
            sent["params"]["sessionId"],
            "01a020c7-d363-7a50-9f08-37e0cc12db3a"
        );

        // `session/load` answers with metadata and no id of its own; the
        // session is the one eave just asked for.
        h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":2,"result":{"models":{"currentModelId":"grok-4.6"}}}"#,
            "01a020c7-d363-7a50-9f08-37e0cc12db3a",
        );
        assert_eq!(
            h.proto.session_id(),
            Some("01a020c7-d363-7a50-9f08-37e0cc12db3a")
        );
    }

    /// A resume eave cannot serve must say so. Grok answers an unknown id
    /// with `FS_NOT_FOUND` rather than silently opening a blank session,
    /// and eave passes that on: a caller blocked in `send_user_message`
    /// gets the reason, not a hang.
    #[test]
    fn a_failed_resume_states_the_reason_instead_of_starting_fresh() {
        let mut h = Harness::new();
        let real_error = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Path not found.","data":{"code":"FS_NOT_FOUND","detail":"No such file or directory (os error 2)"}}}"#;
        let (events, frames) = h.feed_resuming(real_error, "not-a-grok-id");
        assert!(frames.is_empty(), "a refused session sends nothing further");
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::System { subtype, .. }] if subtype == "grok_session_failed"
        ));
        let Some(Err(reason)) = h.ready() else {
            panic!(
                "the handshake must settle as a stated failure, got {:?}",
                h.ready()
            );
        };
        assert!(
            reason.contains("not-a-grok-id"),
            "reason names the id: {reason}"
        );
        assert!(h.proto.session_id().is_none());
    }

    #[test]
    fn a_permission_request_becomes_one_answerable_question() {
        let mut h = Harness::opened();
        let (events, frames) = h.feed(PERM_REQUEST);
        assert!(
            frames.is_empty(),
            "eave answers a question when the operator does, not on arrival"
        );
        match events.as_slice() {
            [SessionEvent::ApprovalRequested {
                request_id,
                tool,
                display_name,
                description,
                path,
                input_json,
                suggestions_json,
            }] => {
                // The id an approval is answered with is the same id the
                // tool rows carry, so a card and its tool name one thing.
                assert_eq!(request_id, "call-adaf4213-2ff0-4064-a133-be3cf729551d-0");
                assert_eq!(tool, "write");
                assert_eq!(display_name, "Write");
                assert_eq!(description, "Write `/tmp/probe2/perm.txt`");
                assert_eq!(path.as_deref(), Some("/tmp/probe2/perm.txt"));
                assert!(input_json.contains("asked"));
                assert!(
                    suggestions_json.as_ref().unwrap().contains("allow_always"),
                    "the session-wide grant is recorded as a suggestion"
                );
            }
            other => panic!("expected one ApprovalRequested, got {other:?}"),
        }
        assert_eq!(h.proto.pending_approvals(), 1);
        assert!(h
            .proto
            .approval_is_open("call-adaf4213-2ff0-4064-a133-be3cf729551d-0"));
    }

    /// eave offers two decisions. Grok offers three, and the extra one is a
    /// session-wide grant that would end review for every later tool call —
    /// so "Allow" must bind to `allow_once` and nothing else.
    #[test]
    fn allow_binds_to_the_once_option_never_the_session_wide_grant() {
        let mut h = Harness::opened();
        h.feed(PERM_REQUEST);
        let pending = h
            .proto
            .pending
            .get("call-adaf4213-2ff0-4064-a133-be3cf729551d-0")
            .expect("question is open");
        assert_eq!(pending.allow.as_deref(), Some("allow-once"));
        assert_eq!(pending.reject.as_deref(), Some("reject-once"));
    }

    /// A refusal must always be expressible. When a request arrives with no
    /// `reject_once` option, the deny falls back to ACP's own `cancelled`
    /// outcome rather than failing to answer.
    #[test]
    fn a_deny_is_expressible_even_when_no_reject_option_is_offered() {
        let with_reject = permission_response_frame(&json!(0), Some("reject-once"));
        let parsed: Value = serde_json::from_str(&with_reject).unwrap();
        assert_eq!(parsed["result"]["outcome"]["outcome"], "selected");
        assert_eq!(parsed["result"]["outcome"]["optionId"], "reject-once");

        let bare = permission_response_frame(&json!(0), None);
        let parsed: Value = serde_json::from_str(&bare).unwrap();
        assert_eq!(parsed["result"]["outcome"]["outcome"], "cancelled");
        assert!(
            parsed["result"]["outcome"].get("optionId").is_none(),
            "a cancelled outcome names no option"
        );
    }

    /// The posture gate. A session that auto-approves is announced AND
    /// refused — the engine kills on the refusal, so this is the only thing
    /// standing between an ambient `always-approve` config and unreviewed
    /// writes.
    #[test]
    fn a_session_that_auto_approves_is_announced_and_refused() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(SESSIONS_YOLO);
        match events.as_slice() {
            [SessionEvent::Init {
                session_id,
                permission_mode,
                ..
            }, SessionEvent::PostureRefused { mode, reason }] => {
                assert_eq!(session_id, SESSION_ID);
                assert_eq!(permission_mode.as_deref(), Some("always-approve"));
                assert_eq!(mode, "always-approve");
                assert!(
                    reason.contains("config.toml"),
                    "the refusal must say how to fix it: {reason}"
                );
            }
            other => panic!("expected Init then PostureRefused, got {other:?}"),
        }
    }

    #[test]
    fn a_session_that_asks_is_announced_and_allowed_to_run() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(SESSIONS_ASKS);
        match events.as_slice() {
            [SessionEvent::Init {
                session_id,
                permission_mode,
                ..
            }] => {
                assert_eq!(session_id, SESSION_ID);
                assert_eq!(permission_mode.as_deref(), Some("default"));
            }
            other => panic!("expected a lone Init, got {other:?}"),
        }
        // Announcing is once-only: a later posture record does not re-open
        // the question or duplicate the transcript's Init.
        let (again, _) = h.feed(SESSIONS_ASKS);
        assert!(again.is_empty());
    }

    /// Fail-closed. If a tool starts running and the session never said
    /// whether it asks, eave has no proof and refuses — it does not assume
    /// that silence means "asks".
    #[test]
    fn a_tool_that_starts_before_any_posture_is_stated_is_refused() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(TOOL_CALL);
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::PostureRefused { mode, .. } if mode == "unstated"
            )),
            "an unproven posture must be refused, got {events:?}"
        );
        // The tool is still reported: the transcript says what it was about
        // to do, which is exactly what the operator needs to see.
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolUseStarted { .. })));
    }

    /// One agent process can report on sessions eave is not driving.
    /// Reading someone else's posture would be reading the wrong fact.
    #[test]
    fn a_posture_record_for_another_session_is_ignored() {
        let mut h = Harness::opened();
        let other = SESSIONS_YOLO.replace(SESSION_ID, "01a020ff-0000-0000-0000-000000000000");
        let (events, _) = h.feed(&other);
        assert!(events.is_empty(), "not our session, not our posture");
        assert!(!h.proto.announced);
    }

    #[test]
    fn a_tool_call_and_its_completion_bracket_the_in_flight_count() {
        let mut h = Harness::opened();
        h.feed(SESSIONS_ASKS);
        let (events, _) = h.feed(TOOL_CALL);
        match events.as_slice() {
            [SessionEvent::ToolUseStarted { id, name, input }] => {
                assert_eq!(id, "call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0");
                // The vendor's own tool name, not the display title.
                assert_eq!(name, "write");
                assert!(input.contains("hello.txt"));
            }
            other => panic!("expected ToolUseStarted, got {other:?}"),
        }
        assert_eq!(h.proto.in_flight(), 1);

        let (events, _) = h.feed(TOOL_DONE);
        match events.as_slice() {
            [SessionEvent::ToolUseFinished { id, is_error, .. }] => {
                assert_eq!(id, "call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0");
                assert!(!is_error);
            }
            other => panic!("expected ToolUseFinished, got {other:?}"),
        }
        assert_eq!(h.proto.in_flight(), 0);
    }

    /// Grok sends several `tool_call_update`s per call; only a terminal
    /// status closes one. Treating a title refinement as a completion would
    /// settle the row while the tool was still running.
    #[test]
    fn a_non_terminal_tool_update_does_not_close_the_call() {
        let mut h = Harness::opened();
        h.feed(SESSIONS_ASKS);
        h.feed(TOOL_CALL);
        let refinement = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0","kind":"edit","title":"Write `/tmp/probe1/hello.txt`"}}}"#;
        let (events, _) = h.feed(refinement);
        assert!(events.is_empty(), "no status, no closure: {events:?}");
        assert_eq!(h.proto.in_flight(), 1);
    }

    #[test]
    fn a_denied_tool_reports_the_rejection_as_the_result() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(TOOL_REJECTED);
        match events.as_slice() {
            [SessionEvent::ToolUseFinished {
                result, is_error, ..
            }] => {
                assert!(is_error);
                assert!(
                    result.contains("User rejected"),
                    "the result says why: {result}"
                );
            }
            other => panic!("expected ToolUseFinished, got {other:?}"),
        }
    }

    /// A denial ends the turn with `stop_reason: "cancelled"`. That is the
    /// permission system working, so it must not render as a turn error.
    #[test]
    fn a_turn_cancelled_by_a_denial_is_not_an_error() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(TURN_CANCELLED);
        match events.as_slice() {
            [SessionEvent::TurnCompleted { subtype, is_error }] => {
                assert_eq!(subtype, "cancelled");
                assert!(!is_error, "a denial is the system working, not a fault");
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_streams_as_deltas() {
        let mut h = Harness::opened();
        let (events, _) = h.feed(MSG_CHUNK);
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::TextDelta { text }] if text == "I'll"
        ));
    }

    /// Reasoning is not the reply. Recording it as assistant text would put
    /// words in the agent's mouth that it never said to the operator.
    #[test]
    fn thinking_is_not_recorded_as_assistant_text() {
        let mut h = Harness::opened();
        let thought = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"The user"}}}}"#;
        let (events, _) = h.feed(thought);
        assert!(events.is_empty());
    }

    /// eave declares no filesystem capability precisely so it is never the
    /// process performing the agent's writes. A capability request arriving
    /// anyway is refused on the wire, not quietly serviced.
    #[test]
    fn a_filesystem_request_is_refused_rather_than_serviced() {
        let mut h = Harness::opened();
        let write_req = r#"{"jsonrpc":"2.0","id":7,"method":"fs/write_text_file","params":{"path":"/tmp/probe/x.txt","content":"hi"}}"#;
        let (events, frames) = h.feed(write_req);
        assert_eq!(frames.len(), 1);
        let sent: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(sent["id"], 7);
        assert_eq!(sent["error"]["code"], -32601);
        assert!(sent.get("result").is_none(), "a refusal is not a result");
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::Unknown { event_type, .. }] if event_type == "fs/write_text_file"
        ));
    }

    /// A misbehaving CLI must not take the session state machine down.
    #[test]
    fn a_malformed_line_is_surfaced_and_parsing_continues() {
        let mut h = Harness::opened();
        let (events, _) = h.feed("this is not json at all");
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::Malformed { .. }]
        ));
        // The very next real line still parses.
        let (events, _) = h.feed(MSG_CHUNK);
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::TextDelta { .. }]
        ));
    }

    /// Grok splits session updates across the standard ACP notification and
    /// its own vendor channel, and `turn_completed` only ever arrives on
    /// the vendor one. Reading a single channel loses the end of a turn.
    #[test]
    fn both_notification_channels_carry_session_updates() {
        assert!(TOOL_CALL.contains(r#""method":"session/update""#));
        assert!(TURN_CANCELLED.contains(r#""method":"_x.ai/session_notification""#));
        let mut h = Harness::opened();
        assert!(!h.feed(TOOL_CALL).0.is_empty());
        assert!(!h.feed(TURN_CANCELLED).0.is_empty());
    }

    /// The spawn argv is a safety contract: `--no-leader` keeps the work
    /// inside the process group eave can kill, and `--always-approve` must
    /// never appear no matter what the session was configured with.
    #[test]
    fn the_spawn_argv_keeps_the_work_inside_a_killable_process_group() {
        let cfg = SpawnConfig::new("/bin/grok", "/tmp/probe", "sess-1".into());
        let args = spawn_args(&cfg);
        assert_eq!(args, vec!["agent", "--no-leader", "stdio"]);
        // The subcommand must come after the options it belongs to, or
        // clap rejects the line.
        let leader = args.iter().position(|a| a == "--no-leader").unwrap();
        let stdio = args.iter().position(|a| a == "stdio").unwrap();
        assert!(leader < stdio);
    }

    #[test]
    fn a_configured_model_lands_before_the_subcommand_and_absence_sends_nothing() {
        let mut cfg = SpawnConfig::new("/bin/grok", "/tmp/probe", "sess-1".into());
        assert!(
            !spawn_args(&cfg).contains(&"-m".to_string()),
            "no model means no flag, not an empty one"
        );

        cfg.model = Some("grok-4.6".into());
        let args = spawn_args(&cfg);
        let m = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[m + 1], "grok-4.6");
        assert!(m < args.iter().position(|a| a == "stdio").unwrap());
    }

    /// Session `extra_args` are appended verbatim and last — but they
    /// cannot be used to smuggle back the auto-approve posture, because
    /// eave never adds it and the engine filters the flags that would
    /// undo it.
    #[test]
    fn the_adapter_never_asks_grok_to_auto_approve() {
        let cfg = SpawnConfig::new("/bin/grok", "/tmp/probe", "sess-1".into());
        assert!(!spawn_args(&cfg).iter().any(|a| a == "--always-approve"));
        assert!(!spawn_args(&cfg)
            .iter()
            .any(|a| a.starts_with("--permission")));
    }
}
