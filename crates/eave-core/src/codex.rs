//! Native Codex adapter: run the `codex` CLI as a long-lived **app-server**
//! over stdio JSON-RPC, parse its notifications into `SessionEvent`s, own
//! its process group, kill it with proof of exit.
//!
//! # Why `app-server` and not `exec`
//!
//! `codex exec --json` is the obvious analogue of Claude's stream-JSON
//! mode and it is the wrong one, for two reasons established by running
//! the CLI (0.148.0, captures in the Stage AU report):
//!
//! 1. **It is one-shot.** `codex exec [PROMPT]` reads its prompt from argv
//!    or stdin *once* and exits when the turn ends; a second turn is a
//!    second process (`codex exec resume <id>`). eave's `AgentSession` is
//!    a live handle you write successive turns into, so exec would make
//!    every turn a new spawn with a new pid — and `identity()`,
//!    `watch()` and `kill()` all name one process.
//! 2. **It has no channel to ask eave anything.** `codex exec --help`
//!    exposes exactly two approval-adjacent flags — `--approve-for-me`
//!    (route approvals to an automatic review subagent) and
//!    `--dangerously-bypass-approvals-and-sandbox`. Neither routes a
//!    question to the caller. Claude's `--permission-prompt-tool stdio`
//!    has no counterpart in exec.
//!
//! `codex app-server` is the mode that has both: it holds a thread open
//! across turns (`thread/start`, then `turn/start` per turn) and it sends
//! the client **JSON-RPC requests** — `item/commandExecution/requestApproval`
//! and `item/fileChange/requestApproval` — that must be answered before
//! the action proceeds. That is the same shape as Slice K's permission
//! channel, and it is what this adapter implements.
//!
//! # Framing
//!
//! Newline-delimited JSON, not LSP `Content-Length` framing. Verified live
//! against the real binary: one JSON object per line in each direction.
//! The server omits `"jsonrpc"` on its own frames and accepts it on ours.
//!
//! # Permission posture (the part that decides whether this may run)
//!
//! eave asks for, and then **verifies**, three things in `thread/start`:
//!
//! | field | eave asks | why |
//! | --- | --- | --- |
//! | `approvalPolicy` | `untrusted` | the strictest of `untrusted` / `on-request` / `never`: only commands Codex itself classes as trusted run unasked, everything else escalates. `never` explicitly "returns execution failures to the model" instead of asking. |
//! | `approvalsReviewer` | `user` | the alternative, `auto_review`, hands the decision to a subagent. eave must be the reviewer or eave is not reviewing. |
//! | `sandbox` | `read-only` | under `workspace-write` the agent writes inside the cwd **without asking**. Read-only is the only setting under which a write is an escalation, and therefore the only one under which eave sees writes at all. |
//!
//! The `thread/start` **response echoes all three back**, and that echo is
//! what this adapter checks — the same discipline as Slice K reading the
//! Claude CLI's own `init.permissionMode` rather than assuming the flag
//! took. Verified live that the echo tracks the request rather than
//! parroting a default: asking for `never`/`auto_review`/`danger-full-access`
//! echoes exactly that back.
//!
//! **What that echo does and does not prove.** It proves the server is
//! configured the way eave asked. It does *not* prove a live approval
//! round trip, because no such capture exists: this machine has no Codex
//! auth, so no turn has ever reached a model and no
//! `item/*/requestApproval` has ever been produced by the real binary. The
//! request and response encodings here come from the CLI's own generated
//! JSON Schema (`codex app-server generate-json-schema`), which is ground
//! truth about the wire and silent about behaviour. Stage AU's report says
//! this again, louder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::events::{cap_payload, ContextWindow, SessionEvent, TurnTokens};
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};
use crate::tool::{self, ToolCall};

const STDERR_RING_CAP: usize = 4096;
/// Codex's fast lane, by the id `model/list` advertises for it
/// (`serviceTiers: [{id: "priority", name: "Fast"}]`, 0.148.0, 2026-09-02).
const CODEX_FAST_TIER: &str = "priority";

const EVENT_CHANNEL_CAP: usize = 256;
const MALFORMED_LINE_CAP: usize = 512;

/// Our own JSON-RPC request ids. Server-initiated requests live in a
/// separate id space (they carry `method`; ours come back carrying
/// `result`/`error` and no method), so these only have to be unique
/// among eave's own calls.
const ID_INITIALIZE: u64 = 1;
const ID_THREAD: u64 = 2;
/// Turn ids start above the fixed handshake ids and never wrap in a
/// session anyone would live through.
const ID_TURN_BASE: u64 = 100;

/// The posture eave asks the app-server for, and the only one it will run
/// under. See the module docs for why each value is the strict one.
const WANT_APPROVAL_POLICY: &str = "untrusted";
const WANT_REVIEWER: &str = "user";
/// What eave sends in `thread/start.sandbox` …
const WANT_SANDBOX_ARG: &str = "read-only";
/// … and what the server calls the same thing when it echoes it back in
/// `thread/start`'s result (`{"type":"readOnly","networkAccess":false}`).
/// The two spellings are the server's, not ours, and they differ.
const WANT_SANDBOX_ECHO: &str = "readOnly";

type ExitFacts = (Option<i32>, Option<i32>);
type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// A server request eave is parked on, keyed by its id rendered as a
/// string. The raw id is kept because JSON-RPC ids are typed — Codex
/// sends integers — and a reply that stringifies one is a reply the
/// server will not match.
#[derive(Debug, Clone)]
struct PendingApproval {
    raw_id: Value,
}

struct Shared {
    proto: Mutex<CodexProtocol>,
    last_activity: Mutex<Instant>,
    /// The effort level / fast lane the NEXT `turn/start` carries
    /// (`crate::tuning`). Per turn on this wire, so `tune` just replaces
    /// it and the next prompt states it. `serviceTier: "priority"` is the
    /// tier id `model/list` names "Fast" (measured 2026-09-02, 0.148.0).
    tuning: Mutex<crate::tuning::Tuning>,
    /// The thread was announced at least once. A process that dies without
    /// this never got far enough to state its posture.
    saw_thread: AtomicBool,
    /// Set at kill initiation; the reader drops all protocol input from
    /// then on — descendants holding inherited pipes may emit stale lines
    /// after the leader dies (spec 01 §Kill).
    kill_initiated: AtomicBool,
}

pub struct CodexSession {
    identity: ProcIdentity,
    pgid: i32,
    /// Shared because three writers exist: the engine sending a turn, the
    /// engine answering an approval, and the reader task driving the
    /// handshake and auto-refusing requests eave cannot present.
    /// Serialized by the mutex — a frame can never interleave mid-line.
    stdin: SharedStdin,
    shared: Arc<Shared>,
    /// `Some(thread_id)` once `thread/start` (or `thread/resume`) has
    /// answered and the posture passed. `send_user_message` waits on this
    /// rather than guessing an id: there is no turn to start before there
    /// is a thread to start it on.
    thread_rx: watch::Receiver<Option<String>>,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    next_turn_id: Arc<AtomicU64>,
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
}

impl CodexSession {
    /// Spawn one `codex app-server` in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    ///
    /// The JSON-RPC handshake happens on the reader task, not here: this
    /// call returns as soon as the process exists, exactly like the Claude
    /// adapter, and `send_user_message` waits for the thread rather than
    /// this constructor blocking on it.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        // The engine's `conflicts_with_eave_flags` names Claude's flags
        // (`--permission-mode`, `--permission-prompt-tool`, …). Codex's
        // posture-undoing flags are different strings, so nothing upstream
        // would catch them and the refusal has to happen here. This is a
        // policy point precisely because the shared one cannot see it.
        if let Some(bad) = posture_undoing_arg(&cfg.extra_args) {
            return Err(SessionError::Spawn(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "agent flag {bad:?} would undo the permission posture Eave spawns Codex \
                     under (approval policy {WANT_APPROVAL_POLICY}, reviewer {WANT_REVIEWER}, \
                     sandbox {WANT_SANDBOX_ARG}); Eave will not run a session whose writes it \
                     cannot review"
                ),
            )));
        }

        let mut cmd = Command::new(&cfg.program);
        cmd.current_dir(&cfg.cwd)
            // `--stdio` is the default transport, stated anyway: the flag
            // is the difference between "we chose stdio" and "we inherited
            // whatever the next release defaults to".
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // Unlike Claude, the model and the working directory are NOT argv:
        // `codex app-server` takes neither, and both are `thread/start`
        // params. `cfg.model` is carried to the handshake below;
        // `cfg.cwd` is both the child's cwd and the thread's `cwd`.
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
            // Same reasoning as the Claude adapter: a child that exited
            // before we could fingerprint it gets a sentinel identity that
            // can never match, so the guard refuses the pid forever. A
            // child that is alive yet uninspectable stays a hard error.
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
            proto: Mutex::new(CodexProtocol::new()),
            last_activity: Mutex::new(Instant::now()),
            tuning: Mutex::new(cfg.tuning.clone()),
            saw_thread: AtomicBool::new(false),
            kill_initiated: AtomicBool::new(false),
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (thread_tx, thread_rx) = watch::channel(None);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        // Everything the reader needs to drive the handshake it is about
        // to be answering.
        let opening = Opening {
            cwd: cfg.cwd.to_string_lossy().into_owned(),
            model: cfg.model.clone(),
            // The thread id is Codex's, minted at thread creation.
            resume_thread: cfg.resume.clone(),
        };

        let reader_shared = Arc::clone(&shared);
        let reader_tx = events_tx.clone();
        let reader_eof = Arc::clone(&stdout_eof);
        let reader_stdin = Arc::clone(&stdin);
        let stdout_task = tokio::spawn(async move {
            // The client speaks first. Everything after this is a reply to
            // something, so the whole protocol can live in one task.
            write_line(&reader_stdin, &initialize_frame()).await;
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                *reader_shared.last_activity.lock().unwrap() = Instant::now();
                if reader_shared.kill_initiated.load(Ordering::SeqCst) {
                    continue;
                }
                let step = reader_shared.proto.lock().unwrap().parse_line(&line);
                for frame in &step.replies {
                    write_line(&reader_stdin, frame).await;
                }
                if let Some(id) = &step.opened_thread {
                    reader_shared.saw_thread.store(true, Ordering::SeqCst);
                    let _ = thread_tx.send(Some(id.clone()));
                }
                if step.handshake_ready {
                    write_line(&reader_stdin, &initialized_frame()).await;
                    write_line(&reader_stdin, &opening.thread_frame()).await;
                }
                for event in step.events {
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
            // A server that died without ever opening a thread never
            // stated a posture, so it never proved it would ask. Say WHY
            // in the transcript instead of leaving a bare non-zero exit
            // for the operator to decode — the same call the Claude
            // adapter makes for its flag-rejected case.
            if !super_shared.saw_thread.load(Ordering::SeqCst) {
                let _ = events_tx
                    .send(SessionEvent::PostureRefused {
                        mode: "unstated".into(),
                        reason: format!(
                            "the Codex app-server exited before it opened a thread, so it \
                             never stated the approval policy it was running under; Eave will \
                             not report a session it could not review. Last stderr: {}",
                            cap_payload(stderr_tail.trim())
                        ),
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
                thread_rx,
                exit_rx,
                next_turn_id: Arc::new(AtomicU64::new(ID_TURN_BASE)),
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

    /// Block until the thread exists, or until the process is gone.
    ///
    /// No timeout, deliberately: the handshake either completes or the
    /// child dies, and both are observable here. Inventing a deadline
    /// would turn a slow start into a fabricated failure — the same
    /// reasoning that keeps eave from ever timing out into an approval
    /// decision.
    async fn thread_id(&self) -> Result<String, SessionError> {
        let mut thread = self.thread_rx.clone();
        let mut exit = self.exit_rx.clone();
        loop {
            if let Some(id) = thread.borrow_and_update().clone() {
                return Ok(id);
            }
            if exit.borrow_and_update().is_some() {
                return Err(no_thread());
            }
            tokio::select! {
                r = thread.changed() => r.map_err(|_| no_thread())?,
                r = exit.changed() => r.map_err(|_| no_thread())?,
            }
        }
    }
}

/// The child is gone and no thread was ever opened, so there is nothing to
/// send a turn to. Said in those words rather than reported as a closed
/// stdin, which is true of the pipe and useless to whoever reads the log.
fn no_thread() -> SessionError {
    SessionError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "the Codex app-server exited before it opened a thread; there is no session to send a \
         turn to",
    ))
}

#[async_trait]
impl crate::provider::AgentAdapter for CodexSession {
    const PROVIDER_ID: &'static str = "codex";

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "codex.rs `posture_refusal`: the app-server's `thread/start` echo is checked \
                   on every spawn for `approvalPolicy=untrusted`, `approvalsReviewer=user` and \
                   `sandbox.type=readOnly`, and a session that cannot prove all three emits \
                   PostureRefused and is killed. `posture_undoing_arg` refuses extra_args that \
                   would undo that (`-c approval_policy=…`, `--sandbox`, `--approve-for-me`). \
                   `sandbox: \"read-only\"` is load-bearing — under `workspace-write` the agent \
                   writes inside the cwd with no request at all.",
    };

    const REWIND: crate::provider::RewindSupport = crate::provider::RewindSupport::None {
        reason: "Codex has a conversation rewind and Eave has not been able to establish its \
                 shape. `thread/revert` and a deprecated `thread/rollback` are both in the \
                 installed app-server (2026-08-31), and `ThreadRevertParams` takes two fields, \
                 but which two is not readable from the binary and there is no Codex auth on \
                 this machine to ask. A revert addresses a TURN COUNT, so a guessed field name \
                 does not fail loudly — it risks discarding the wrong number of turns. This is \
                 the one row where Eave knows support exists and still says no.",
    };

    const TUNING: crate::tuning::TuningSupport = crate::tuning::TuningSupport {
        effort: crate::tuning::EffortSupport::Levels {
            levels: &["low", "medium", "high", "xhigh", "max", "ultra"],
            applies: crate::tuning::Applies::NextTurn,
            evidence: "`turn/start.effort` (\"Override the reasoning effort for this turn and \
                       subsequent turns\", generate-json-schema 0.148.0) and a live `model/list` \
                       2026-09-02: gpt-5.6-sol/terra take low…ultra, gpt-5.6-luna low…max, \
                       gpt-5.5 and gpt-5.2 low…xhigh. This list is the UNION; a level a \
                       particular model lacks is Codex's error to state.",
        },
        speed: crate::tuning::SpeedSupport::Toggle {
            applies: crate::tuning::Applies::NextTurn,
            evidence: "`turn/start.serviceTier`; the same `model/list` names one tier on every \
                       gpt-5.5+ model: `{id: \"priority\", name: \"Fast\"}` (gpt-5.2 has none). \
                       Eave sends the id.",
        },
        model: crate::tuning::ModelSupport::Switch {
            applies: crate::tuning::Applies::NextTurn,
            evidence: "`turn/start` takes `model` beside `effort` and `serviceTier` \
                       (`codex app-server generate-json-schema`, TurnStartParams, 0.148.0, \
                       2026-09-02); the ids come from the live `model/list`.",
        },
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        CodexSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for CodexSession {
    fn identity(&self) -> ProcIdentity {
        self.identity
    }

    fn provider_session_id(&self) -> Option<String> {
        self.shared.proto.lock().unwrap().thread_id.clone()
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

    async fn send_user_message(
        &mut self,
        text: &str,
        images: &[crate::media::ImageRef],
    ) -> Result<(), SessionError> {
        let thread_id = self.thread_id().await?;
        let id = self.next_turn_id.fetch_add(1, Ordering::SeqCst);
        let mut input = vec![json!({ "type": "text", "text": text })];
        for img in crate::media::encode_all(images)? {
            input.push(crate::media::codex_image_item(&img));
        }
        let params = {
            let t = self.shared.tuning.lock().unwrap();
            turn_params(&thread_id, input, &t)
        };
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "turn/start",
            "params": params,
        });
        write_frame(&self.stdin, &frame.to_string()).await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.proto.lock().unwrap().pending.len()
    }

    /// Answer one approval request.
    ///
    /// The reply is a JSON-RPC **response** to the server's request, so it
    /// carries the request's own id — in the id's original JSON type. Codex
    /// numbers its requests, and an id that arrived as `7` and goes back as
    /// `"7"` is a reply the server will not match.
    ///
    /// **The deny message does not reach the model.** Codex's
    /// `CommandExecutionApprovalDecision` and `FileChangeApprovalDecision`
    /// are closed enums — `accept` / `acceptForSession` / `decline` /
    /// `cancel` and two amendment variants — with nowhere to put text.
    /// Claude's `deny` carries the operator's words to the model verbatim
    /// (Slice K, capture D); Codex's does not, and this adapter does not
    /// pretend otherwise by smuggling it in as a `turn/steer` mid-turn.
    /// `Deny { message }` sends `decline` and the message is dropped. That
    /// is a fidelity gap between providers, recorded in the Stage AU
    /// report rather than papered over.
    ///
    /// `Ok(false)` means the question was not open — already answered, or
    /// withdrawn under the operator's click. A race, not an error, and it
    /// must never reach the pipe.
    async fn respond_to_approval(
        &mut self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, SessionError> {
        let Some(pending) = self
            .shared
            .proto
            .lock()
            .unwrap()
            .pending
            .get(request_id)
            .cloned()
        else {
            return Ok(false);
        };
        let verdict = match decision {
            ApprovalDecision::Allow => "accept",
            // `decline` and not `cancel`: `cancel` denies AND interrupts
            // the turn outright. "No, not that" is not "stop everything",
            // and the operator asked for the first one.
            ApprovalDecision::Deny { .. } => "decline",
        };
        let frame = json!({
            "jsonrpc": "2.0",
            "id": pending.raw_id,
            "result": { "decision": verdict },
        });
        write_frame(&self.stdin, &frame.to_string()).await?;
        // Only now is the question closed: a failed write leaves it open
        // so the operator can try again rather than facing a dead card.
        self.shared.proto.lock().unwrap().pending.remove(request_id);
        Ok(true)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    /// Per turn on this wire: the next `turn/start` carries it. Nothing
    /// is written now, so a change during a running turn lands on the
    /// following one, exactly as the schema's "this turn and subsequent
    /// turns" reads.
    async fn tune(&mut self, tuning: &crate::tuning::Tuning) -> Result<(), SessionError> {
        *self.shared.tuning.lock().unwrap() = tuning.clone();
        Ok(())
    }

    /// Unsupported *for now*, and for a different reason than the ACP
    /// adapters: the wire has a revert, eave cannot yet address it. See
    /// `REWIND`.
    async fn rewind(
        &mut self,
        _target: &crate::session::RewindTarget,
    ) -> Result<crate::session::RewindOutcome, SessionError> {
        Ok(crate::session::RewindOutcome::Unsupported)
    }

    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.shared.kill_initiated.store(true, Ordering::SeqCst);
        let already_exited = self.exited();
        let mut escalated = false;

        if !already_exited {
            // Group signals are safe without a pid-reuse guard: while any
            // member lives the pgid stays reserved by the kernel, and once
            // the group is empty killpg is ESRCH — a no-op. The app-server
            // is the reason this matters more here than for Claude: it
            // starts the operator's configured MCP servers as children
            // (observed live: `node_repl` went starting → ready during the
            // handshake), and those are in this group.
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

        // One bounded straggler scan (spec 01 §Kill step 3). Pids are
        // fresh from the scan, which is the guard for signalling them.
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
// Flags that would undo the posture

/// The first of these extra arguments that would put the app-server in a
/// posture eave has not verified, if any.
///
/// Codex's dangerous flags are not Claude's, so
/// `session::conflicts_with_eave_flags` — which names `--permission-mode`
/// and friends — sees none of them. `-c/--config` is the one that matters
/// most: `app-server` accepts it, and `-c approval_policy=never` is a
/// posture change spelled as a config override.
///
/// It takes the whole slice rather than one argument because the shape
/// that matters most is the two-element one: `-c` and its `key=value` are
/// separate argv entries, and a per-argument predicate looking at `-c`
/// alone sees nothing wrong with it.
///
/// A false positive costs the operator one flag they have to justify; a
/// false negative is a session writing to disk with nothing reviewing it.
/// It is deliberately biased toward the first.
pub fn posture_undoing_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let head = arg.split('=').next().unwrap_or(arg);
        if matches!(
            head,
            "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--approve-for-me"
                | "--ask-for-approval"
                | "-a"
                | "--sandbox"
                | "-s"
                | "--profile"
                | "-p"
                | "--add-dir"
                | "--enable"
                | "--disable"
        ) {
            return Some(arg.clone());
        }
        // `-c key=value` in each of its three spellings. What decides is
        // the config KEY, wherever it happens to live.
        let (key, blamed) = if head == "-c" || head == "--config" {
            match arg.split_once('=') {
                // `--config=approval_policy=never`
                Some((_, rest)) => (rest, arg.clone()),
                // `-c approval_policy=never` — the key is the next entry.
                None => match args.get(i + 1) {
                    Some(next) => {
                        i += 1;
                        (next.as_str(), format!("{arg} {next}"))
                    }
                    None => ("", arg.clone()),
                },
            }
        } else if let Some(rest) = head.strip_prefix("-c") {
            // `-capproval_policy=never`
            (rest, arg.clone())
        } else {
            i += 1;
            continue;
        };
        let key = key.split('=').next().unwrap_or(key).trim();
        if key.starts_with("approval")
            || key.starts_with("sandbox")
            || key.starts_with("shell_environment_policy")
            || key.starts_with("features.")
        {
            return Some(blamed);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// The handshake

/// The three facts the reader needs to open a thread once `initialize`
/// answers. Held on the reader task rather than in `Shared`: nothing else
/// reads them, and nothing writes them after spawn.
struct Opening {
    cwd: String,
    model: Option<String>,
    /// `Some(thread_id)` when the engine asked to continue a Codex thread
    /// rather than start one.
    resume_thread: Option<String>,
}

impl Opening {
    fn thread_frame(&self) -> String {
        let mut params = json!({
            "cwd": self.cwd,
            "approvalPolicy": WANT_APPROVAL_POLICY,
            "approvalsReviewer": WANT_REVIEWER,
            "sandbox": WANT_SANDBOX_ARG,
        });
        // Absent model = no `model` key at all, which is not the same as
        // an empty one (P2 caveat 5: eave passes a model verbatim and does
        // not invent one).
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let method = match &self.resume_thread {
            Some(id) => {
                params["threadId"] = json!(id);
                "thread/resume"
            }
            None => "thread/start",
        };
        json!({
            "jsonrpc": "2.0",
            "id": ID_THREAD,
            "method": method,
            "params": params,
        })
        .to_string()
    }
}

/// `turn/start`'s params (Slice TN1). "Override the reasoning effort /
/// service tier for this turn and subsequent turns" (`TurnStartParams`,
/// generate-json-schema 0.148.0). Absent = the model's own default, never
/// a guessed one.
fn turn_params(thread_id: &str, input: Vec<Value>, t: &crate::tuning::Tuning) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": input,
    });
    if let Some(level) = &t.effort {
        params["effort"] = json!(level);
    }
    if t.fast {
        params["serviceTier"] = json!(CODEX_FAST_TIER);
    }
    // Slice TN2: the model rides the same frame. Sent whenever the
    // session's effective model is known, so a switch and a steady state
    // look the same to the app-server.
    if let Some(model) = &t.model {
        params["model"] = json!(model);
    }
    params
}

fn initialize_frame() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": ID_INITIALIZE,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "eave", "title": "Eave", "version": env!("CARGO_PKG_VERSION") },
        },
    })
    .to_string()
}

fn initialized_frame() -> String {
    json!({ "jsonrpc": "2.0", "method": "initialized" }).to_string()
}

// ---------------------------------------------------------------------------
// Protocol

/// What one parsed line asks the reader task to do.
#[derive(Default)]
struct Step {
    events: Vec<SessionEvent>,
    /// Frames to write back before the events are published — a JSON-RPC
    /// response the server is blocked on must not wait behind a full
    /// event channel.
    replies: Vec<String>,
    /// `initialize` answered; send `initialized` and open the thread.
    handshake_ready: bool,
    /// The thread id, the moment it is known and the posture has passed.
    opened_thread: Option<String>,
}

/// Codex app-server line parser and the small amount of state that the
/// nine `AgentSession` methods are answers about.
struct CodexProtocol {
    thread_id: Option<String>,
    /// Items started and not yet completed, restricted to the item types
    /// that are actually tool calls. `in_flight_tools` counts these and
    /// nothing else — an `agentMessage` in progress is the agent talking,
    /// not a tool running.
    outstanding: HashSet<String>,
    /// Item id → the item as it looked when it started. An approval
    /// request for a file change names only its `itemId`; the paths it
    /// would touch are in the item. Bounded by the in-flight set: entries
    /// are dropped on completion.
    items: HashMap<String, Value>,
    pending: HashMap<String, PendingApproval>,
}

impl CodexProtocol {
    fn new() -> Self {
        Self {
            thread_id: None,
            outstanding: HashSet::new(),
            items: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    /// Parse one line. Never fails: malformed input becomes a `Malformed`
    /// event and parsing continues on the next line — a misbehaving CLI
    /// must not take the session state machine down with it.
    fn parse_line(&mut self, line: &str) -> Step {
        let line = line.trim();
        if line.is_empty() {
            return Step::default();
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return Step::from_event(malformed(line, err.to_string())),
        };
        match (v.get("method").and_then(Value::as_str), v.get("id")) {
            (Some(method), Some(id)) => self.server_request(method, id.clone(), &v, line),
            (Some(method), None) => self.notification(method, &v, line),
            (None, Some(id)) => self.response(id, &v, line),
            (None, None) => Step::from_event(SessionEvent::Unknown {
                event_type: "unaddressed".into(),
                raw: line.to_string(),
            }),
        }
    }

    /// A reply to one of eave's own requests.
    fn response(&mut self, id: &Value, v: &Value, line: &str) -> Step {
        let id = id.as_u64();
        let err = v.get("error");
        if id == Some(ID_INITIALIZE) {
            return match err {
                None => Step {
                    handshake_ready: true,
                    ..Default::default()
                },
                Some(e) => Step::from_event(SessionEvent::PostureRefused {
                    mode: "unknown".into(),
                    reason: format!(
                        "the Codex app-server refused `initialize` ({}), so Eave never got to \
                         ask it for a reviewable approval policy and cannot prove it will ask \
                         before it writes",
                        compact(e)
                    ),
                }),
            };
        }
        if id == Some(ID_THREAD) {
            return match (err, v.get("result")) {
                (Some(e), _) => Step::from_event(SessionEvent::PostureRefused {
                    mode: "unknown".into(),
                    reason: format!(
                        "the Codex app-server could not open the thread ({}); with no thread \
                         there is no stated approval policy, and Eave does not run a session \
                         whose posture it cannot read",
                        compact(e)
                    ),
                }),
                (None, Some(result)) => self.thread_opened(result),
                (None, None) => Step::from_event(malformed(
                    line,
                    "thread response carried neither result nor error".into(),
                )),
            };
        }
        // A turn that could not start is a fact the operator needs; the
        // journal keeps the server's own words for it.
        match err {
            Some(_) => Step::from_event(SessionEvent::System {
                subtype: "turn_start_failed".into(),
                raw: line.to_string(),
            }),
            None => Step::default(),
        }
    }

    /// `thread/start` (or `thread/resume`) answered. This is the one place
    /// the posture is checked, and the only gate between a spawned process
    /// and a session that is allowed to do anything.
    fn thread_opened(&mut self, result: &Value) -> Step {
        let Some(id) = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Step::from_event(SessionEvent::PostureRefused {
                mode: "unstated".into(),
                reason: "the Codex app-server opened a thread without naming it, so Eave has \
                         no resume cursor and no request it could correlate an approval with"
                    .into(),
            });
        };
        let policy = result.get("approvalPolicy").and_then(Value::as_str);
        let reviewer = result.get("approvalsReviewer").and_then(Value::as_str);
        let sandbox = result.pointer("/sandbox/type").and_then(Value::as_str);
        let mode = posture_label(policy, reviewer, sandbox);
        // Recorded even when the posture is about to be refused: the
        // thread exists on disk under this id whether or not eave will
        // drive it, and `provider_session_id()` answers "what did the
        // provider call this", not "may we send turns to it". The second
        // question is the `opened_thread` watch below, and only a passing
        // posture sets it.
        self.thread_id = Some(id.clone());
        let init = SessionEvent::Init {
            fast_lane: None,
            session_id: id.clone(),
            permission_mode: Some(mode.clone()),
            // Empty is the honest answer, not a placeholder: this
            // provider has announced no command inventory to eave.
            // the app-server's command list is not read yet — see Not done.
            slash_commands: Vec::new(),
        };
        if let Some(refusal) = posture_refusal(policy, reviewer, sandbox, &mode) {
            // The Init is still emitted: what the CLI said it was doing is
            // a fact worth journaling even — especially — when the answer
            // is that eave will not run under it.
            return Step {
                events: vec![init, refusal],
                ..Default::default()
            };
        }
        Step {
            events: vec![init],
            opened_thread: Some(id),
            ..Default::default()
        }
    }

    /// A request the server expects an answer to.
    fn server_request(&mut self, method: &str, id: Value, v: &Value, line: &str) -> Step {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let key = render_id(&id);
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                self.pending
                    .insert(key.clone(), PendingApproval { raw_id: id });
                Step::from_event(self.approval_event(method, key, &params))
            }
            // Questions eave cannot present. Answered immediately with a
            // stated refusal — silence would park the session on a
            // question nobody can see, which is the failure Slice K's
            // amendment 2 exists to prevent. A JSON-RPC error is also the
            // only honest answer: there is no "the user said nothing"
            // value in these response schemas.
            "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/permissions/requestApproval"
            | "applyPatchApproval"
            | "execCommandApproval" => Step {
                events: vec![SessionEvent::DialogRefused {
                    request_id: key,
                    dialog: method.to_string(),
                }],
                replies: vec![error_reply(
                    &id,
                    &format!(
                        "Eave cannot present {method} to the operator. Do not retry it; say what \
                         you need in your reply text and the operator will answer there."
                    ),
                )],
                ..Default::default()
            },
            // Infrastructure requests eave does not implement (clock
            // reads, token refresh, dynamic tool calls it never declared).
            // Not a dialog and not an approval — the correct answer is
            // "no such method here", and the line is journaled verbatim
            // so an unimplemented call that starts mattering is visible.
            _ => Step {
                events: vec![SessionEvent::System {
                    subtype: method.to_string(),
                    raw: line.to_string(),
                }],
                replies: vec![method_not_found(&id, method)],
                ..Default::default()
            },
        }
    }

    fn approval_event(&self, method: &str, request_id: String, params: &Value) -> SessionEvent {
        let item = params
            .get("itemId")
            .and_then(Value::as_str)
            .and_then(|id| self.items.get(id));
        let file_change = method == "item/fileChange/requestApproval";
        // The full fact of record: what was asked, plus the item it is
        // about when we have it. Approving without knowing what you
        // approved is the failure this event exists to prevent, and for a
        // file change the params name no path at all — the paths are in
        // the item.
        let mut input = json!({ "request": params });
        if let Some(item) = item {
            input["item"] = item.clone();
        }
        let path = if file_change {
            item.and_then(|i| i.pointer("/changes/0/path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            params
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let description = params
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                if file_change {
                    let paths = item
                        .and_then(|i| i.get("changes"))
                        .and_then(Value::as_array)
                        .map(|cs| {
                            cs.iter()
                                .filter_map(|c| c.get("path").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(", ")
                        });
                    paths.filter(|p| !p.is_empty())
                } else {
                    params
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            })
            .unwrap_or_default();
        // The provider's own routes out of being asked, verbatim, for the
        // day a grants slice wants them. eave offers none of them today.
        let mut suggestions = serde_json::Map::new();
        for key in [
            "availableDecisions",
            "proposedExecpolicyAmendment",
            "proposedNetworkPolicyAmendments",
            "grantRoot",
            "additionalPermissions",
        ] {
            if let Some(v) = params.get(key) {
                if !v.is_null() {
                    suggestions.insert(key.to_string(), v.clone());
                }
            }
        }
        SessionEvent::ApprovalRequested {
            request_id,
            tool: if file_change {
                "fileChange".into()
            } else {
                "commandExecution".into()
            },
            display_name: if file_change {
                "Edit files".into()
            } else {
                "Run command".into()
            },
            description: cap_payload(&description),
            path,
            input_json: cap_payload(&input.to_string()),
            suggestions_json: (!suggestions.is_empty())
                .then(|| Value::Object(suggestions).to_string()),
        }
    }

    fn notification(&mut self, method: &str, v: &Value, line: &str) -> Step {
        let params = v.get("params").unwrap_or(&Value::Null);
        // **Thread attribution, before anything else reads the frame.**
        //
        // Codex multiplexes SEVERAL threads over one connection: a `Task`
        // spawns a child thread whose turns start, run tools and complete
        // on the same stdout, each frame naming its own `threadId`. eave
        // drives exactly one of them — the one it opened, held in
        // `self.thread_id` — and every other frame is somebody else's.
        //
        // Without this guard a child's `turn/completed` became eave's
        // `TurnCompleted`. Verified against comet's live 0.146.1
        // multi-agent capture, replayed through this parser: three
        // threads, and the two children complete at lines 104 and 134
        // while the parent runs to line 144. So the parent turn was
        // closed twice, early — and each close clears BOTH `outstanding`
        // and `pending_approvals` (`attention.rs`), which silently drops
        // an open permission question out of the attention model while
        // its card is still on screen.
        //
        // comet names this class at `crates/harness/src/codex/
        // normalize.rs:324` — child turn frames "can never settle the
        // PARENT turn (the exact bug class the explicit table exists
        // for)". eave already applied the same rule to Grok's posture
        // channel (`grok.rs`: "one agent process can report on sessions
        // eave is not driving") and simply had not applied it here.
        //
        // Foreign frames are journaled, never dropped: a child thread
        // doing work IS a fact about this session, and a later subagent
        // slice will want it. It is recorded as somebody else's fact
        // rather than folded into eave's own.
        //
        // **Slice CJ: attributed, not detyped.** CH wrote the foreign
        // frame out as `System { subtype: "foreign_thread:<method>" }` —
        // correct about ownership and wrong about everything else. A
        // child's `commandExecution` (capture lines 68, 82, 112), its
        // `item/completed`, its token usage all collapsed into one opaque
        // string, which is eave's own through-line defect this week:
        // narrowing provider data at the ingest boundary and never being
        // able to widen it again. Attribution and filtering are two
        // concerns and CH fused them; the fusion is what destroyed the
        // type.
        //
        // So the frame is decoded by exactly the parser it would get if
        // it were eave's own, and then WRAPPED in
        // [`SessionEvent::ForeignThread`]. The wrapper is the whole
        // safety property: nothing downstream matches
        // `SessionEvent::TurnCompleted` through it, so a child's turn
        // end cannot reach eave's ledger however many arms are added
        // later. `decode_threaded` is free of `self` for the same reason
        // — a child's tool cannot land in `outstanding` because the
        // decoder has nothing to land it in.
        let foreign = match (
            self.thread_id.as_deref(),
            params.get("threadId").and_then(Value::as_str),
        ) {
            (Some(mine), Some(theirs)) if mine != theirs => Some(theirs.to_string()),
            _ => None,
        };
        // The turn-scoped methods decode identically whoever owns them.
        // What differs is only who the fact gets recorded against.
        if let Some((event, book)) = decode_threaded(method, params, line) {
            return match foreign {
                Some(thread_id) => attributed(thread_id, event),
                None => {
                    self.record(book);
                    Step::from_event(event)
                }
            };
        }
        if let Some(thread_id) = foreign {
            // A thread-scoped method this parser does not decode
            // (`turn/started`, `thread/status/changed`, MCP startup, and
            // whatever 0.149 adds). Verbatim, same contract as an
            // unrecognised frame of eave's own — and still attributed.
            //
            // **This breadth is deliberately kept.** t3code guards only
            // `turn/started` and `turn/completed`
            // (`CodexSessionRuntime.ts`), so a child's `item/*` and
            // `thread/tokenUsage/updated` still reach its parent session.
            // eave attributes every frame that names a thread.
            return attributed(
                thread_id,
                SessionEvent::System {
                    subtype: method.to_string(),
                    raw: line.to_string(),
                },
            );
        }
        match method {
            // The response to `thread/start` already carried the id AND
            // the posture, and it always arrives first (verified live
            // against 0.148.0). This notification carries only the thread
            // — no policy, no reviewer, no sandbox — so a build that ever
            // stopped answering the request would reach the posture check
            // with nothing to check, and be refused. That is the correct
            // failure: fail-closed, not "well, it named a thread".
            "thread/started" => {
                if self.thread_id.is_some() {
                    return Step::from_event(SessionEvent::System {
                        subtype: method.into(),
                        raw: line.to_string(),
                    });
                }
                self.thread_opened(params)
            }
            // The server withdrew a request nobody answered. The card must
            // go away — an unanswerable question left on screen is a lie.
            "serverRequest/resolved" => {
                let Some(id) = params.get("requestId") else {
                    return Step::from_event(malformed(line, "resolved without requestId".into()));
                };
                let key = render_id(id);
                if self.pending.remove(&key).is_some() {
                    Step::from_event(SessionEvent::ApprovalWithdrawn {
                        request_id: key,
                        reason: "the Codex app-server resolved the request without Eave".into(),
                    })
                } else {
                    Step::default()
                }
            }
            // Everything else the server says, kept verbatim so the
            // journal records a CLI addition without a parser change —
            // `error`, `warning`, `thread/status/changed`,
            // MCP startup, and whatever 0.149 adds.
            _ => Step::from_event(SessionEvent::System {
                subtype: method.to_string(),
                raw: line.to_string(),
            }),
        }
    }

    /// Record what a frame of eave's OWN means for its in-flight state.
    ///
    /// Split out of the decoders (Slice CJ) so that *decoding* a frame and
    /// *owning* it are two acts. A child thread's frame runs through the
    /// same decoders and simply never reaches this method, which is why a
    /// child's `commandExecution` can never appear in `in_flight_tools`
    /// and why the decoders needed no `mine: bool` argument — the thing
    /// that would have fused the two concerns all over again.
    fn record(&mut self, book: Bookkeeping) {
        match book {
            Bookkeeping::Nothing => {}
            Bookkeeping::ToolStarted { id, item } => {
                self.outstanding.insert(id.clone());
                self.items.insert(id, item);
            }
            Bookkeeping::ToolFinished { id } => {
                self.outstanding.remove(&id);
                self.items.remove(&id);
            }
        }
    }
}

/// What a decoded frame would have eave record about its OWN session.
///
/// Carried beside the event rather than applied inside the decoder,
/// because the same frame shape arrives for threads eave does not drive
/// and only one of them is eave's.
enum Bookkeeping {
    /// Nothing about eave's in-flight state changes.
    Nothing,
    /// A tool call is outstanding. The item is kept too: an approval
    /// request for a file change names only its `itemId`, and the paths
    /// it would touch live in the item.
    ToolStarted {
        id: String,
        item: Value,
    },
    ToolFinished {
        id: String,
    },
}

/// One frame, attributed to the thread that owns it.
fn attributed(thread_id: String, event: SessionEvent) -> Step {
    Step::from_event(SessionEvent::ForeignThread {
        thread_id,
        event: Box::new(event),
    })
}

/// Decode one turn-scoped notification — a frame that names a `threadId`
/// and therefore belongs to exactly one thread on a multiplexed
/// connection. `None` = not a method this parser decodes.
///
/// **Free of `self` on purpose.** It cannot reach eave's in-flight set,
/// its item cache or its pending approvals, so a child thread's frame can
/// be run through the very parser eave runs its own through with no
/// possibility of the child's facts landing in eave's state. That is what
/// lets attribution stay a wrapper instead of becoming a `to_string()`.
fn decode_threaded(
    method: &str,
    params: &Value,
    line: &str,
) -> Option<(SessionEvent, Bookkeeping)> {
    let plain = |event: SessionEvent| Some((event, Bookkeeping::Nothing));
    match method {
        "item/agentMessage/delta" => match params.get("delta").and_then(Value::as_str) {
            Some(text) => plain(SessionEvent::TextDelta { text: text.into() }),
            None => plain(malformed(line, "agentMessage delta without text".into())),
        },
        "item/started" => Some(decode_item_started(params, line)),
        "item/completed" => Some(decode_item_completed(params, line)),
        "turn/completed" => {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            plain(SessionEvent::TurnCompleted {
                subtype: status.to_string(),
                // The server's own word, not an inference from the text:
                // `completed` is the only success.
                is_error: status != "completed",
            })
        }
        // Slice CC. Codex is the one provider that states both facts in
        // one frame and labels them itself: `tokenUsage.last` is the turn
        // just finished, `tokenUsage.total` is the running session sum,
        // and `modelContextWindow` is the ceiling.
        "thread/tokenUsage/updated" => plain(token_usage(params).unwrap_or_else(|| {
            // A frame shaped differently from the one this build knows
            // still reaches the journal verbatim.
            SessionEvent::System {
                subtype: method.to_string(),
                raw: line.to_string(),
            }
        })),
        _ => None,
    }
}

fn decode_item_started(params: &Value, line: &str) -> (SessionEvent, Bookkeeping) {
    let unbooked = |event| (event, Bookkeeping::Nothing);
    let Some(item) = params.get("item") else {
        return unbooked(malformed(line, "item/started without item".into()));
    };
    let (Some(id), Some(kind)) = (
        item.get("id").and_then(Value::as_str),
        item.get("type").and_then(Value::as_str),
    ) else {
        return unbooked(malformed(line, "item without id or type".into()));
    };
    if kind == "userMessage" {
        return unbooked(SessionEvent::UserEcho {
            text: user_text(item),
            // A thread item has an id, but `thread/revert` addresses a
            // TURN COUNT, not an item — so carrying it here would name a
            // thing the rewind cannot be asked about.
            provider_msg_id: None,
        });
    }
    let Some(name) = tool_name(kind, item) else {
        // Reasoning, plans, agent messages: real items, not tool calls.
        // Journaled as facts; they must never reach `in_flight_tools`,
        // which is the difference between "the agent is working" and "the
        // agent is thinking".
        return unbooked(SessionEvent::System {
            subtype: format!("item/started/{kind}"),
            raw: line.to_string(),
        });
    };
    (
        SessionEvent::ToolUseStarted {
            id: id.to_string(),
            call: Some(decode_tool(kind, item, &name)),
            name,
            input: cap_payload(&tool_input(kind, item)),
        },
        Bookkeeping::ToolStarted {
            id: id.to_string(),
            item: item.clone(),
        },
    )
}

fn decode_item_completed(params: &Value, line: &str) -> (SessionEvent, Bookkeeping) {
    let unbooked = |event| (event, Bookkeeping::Nothing);
    let Some(item) = params.get("item") else {
        return unbooked(malformed(line, "item/completed without item".into()));
    };
    let (Some(id), Some(kind)) = (
        item.get("id").and_then(Value::as_str),
        item.get("type").and_then(Value::as_str),
    ) else {
        return unbooked(malformed(line, "item without id or type".into()));
    };
    if kind == "agentMessage" {
        return unbooked(SessionEvent::AssistantText {
            text: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    if tool_name(kind, item).is_none() {
        return unbooked(SessionEvent::System {
            subtype: format!("item/completed/{kind}"),
            raw: line.to_string(),
        });
    }
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    (
        SessionEvent::ToolUseFinished {
            id: id.to_string(),
            result: cap_payload(&tool_result(kind, item)),
            // `declined` is a failure by the provider's own word: the
            // operator said no and the tool did not run.
            is_error: matches!(status, "failed" | "declined")
                || item.get("error").is_some_and(|e| !e.is_null()),
                    diff: None,
                },
        Bookkeeping::ToolFinished { id: id.to_string() },
    )
}

impl Step {
    fn from_event(event: SessionEvent) -> Self {
        Self {
            events: vec![event],
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Posture

/// The posture as one string, in the order it is decided: what escalates,
/// who reviews it, what the sandbox lets through unasked. This is what the
/// rail shows and what the journal keeps, so it names all three — any one
/// of them alone would misdescribe the session.
fn posture_label(policy: Option<&str>, reviewer: Option<&str>, sandbox: Option<&str>) -> String {
    format!(
        "{}/{}/{}",
        policy.unwrap_or("unstated"),
        reviewer.unwrap_or("unstated"),
        sandbox.unwrap_or("unstated"),
    )
}

/// Fail-closed by construction, exactly like Slice K's `ASKING_MODES`: the
/// session is refused unless all three fields come back as the strict
/// values eave asked for. A field this build has never heard of, and a
/// server that named none at all, are both refusals — "does this posture
/// review writes?" has no safe default.
fn posture_refusal(
    policy: Option<&str>,
    reviewer: Option<&str>,
    sandbox: Option<&str>,
    mode: &str,
) -> Option<SessionEvent> {
    let complaint = match (policy, reviewer, sandbox) {
        (Some(WANT_APPROVAL_POLICY), Some(WANT_REVIEWER), Some(WANT_SANDBOX_ECHO)) => return None,
        (p, _, _) if p != Some(WANT_APPROVAL_POLICY) => format!(
            "its approval policy is {}, not {WANT_APPROVAL_POLICY:?} — Eave asked for the \
             policy under which anything untrusted escalates to a human",
            quoted(p)
        ),
        (_, r, _) if r != Some(WANT_REVIEWER) => format!(
            "approvals are reviewed by {}, not {WANT_REVIEWER:?} — the questions would go to \
             an automatic reviewer instead of to the operator",
            quoted(r)
        ),
        (_, _, s) => format!(
            "its sandbox is {}, not {WANT_SANDBOX_ECHO:?} — under any wider sandbox the agent \
             writes without an approval request, so Eave would never see the writes",
            quoted(s)
        ),
    };
    Some(SessionEvent::PostureRefused {
        mode: mode.to_string(),
        reason: format!(
            "the Codex app-server opened the thread in a posture Eave will not supervise: \
             {complaint}. Eave refuses to run an agent whose writes it cannot review."
        ),
    })
}

fn quoted(v: Option<&str>) -> String {
    match v {
        Some(v) => format!("{v:?}"),
        None => "unstated".into(),
    }
}

// ---------------------------------------------------------------------------
// Item shaping

/// Is this item type a tool call, and what should it be called?
///
/// `None` means "a real item that is not a tool" — reasoning, plans, the
/// agent's own message. Keeping that distinction here is what keeps
/// `in_flight_tools` meaning what its doc comment says it means.
fn tool_name(kind: &str, item: &Value) -> Option<String> {
    let s = |k: &str| {
        item.get(k)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    match kind {
        "commandExecution" => Some("commandExecution".into()),
        "fileChange" => Some("fileChange".into()),
        "webSearch" => Some("webSearch".into()),
        "imageView" => Some("imageView".into()),
        "imageGeneration" => Some("imageGeneration".into()),
        "mcpToolCall" => Some(format!("mcp:{}/{}", s("server"), s("tool"))),
        "dynamicToolCall" => Some(format!("tool:{}", s("tool"))),
        "collabAgentToolCall" => Some(format!("agent:{}", s("tool"))),
        _ => None,
    }
}

/// Decode a thread item into a typed [`ToolCall`].
///
/// `name` is whatever [`tool_name`] already decided to call this item, so
/// an unrecognised kind keeps exactly the label the row shows rather than
/// inventing a second one.
fn decode_tool(kind: &str, item: &Value, name: &str) -> ToolCall {
    match kind {
        "commandExecution" => ToolCall::Run {
            command: tool::text(item, "command").unwrap_or_default(),
        },
        // The app-server states a file operation as a list of changes
        // (`[{path, kind: add|update|delete}]`, capture: fake_codex.rs
        // `file-approval`), never as a tool name. One `add` is a write and
        // one `update` an edit; a delete, or several files at once, is a
        // patch — the same reduction comet settled on
        // (`comet/crates/harness/src/codex/normalize.rs:100`
        // `file_change_call`), reached from the same wire.
        "fileChange" => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let path = |c: &Value| tool::text(c, "path").unwrap_or_default();
            match changes {
                [one] => match one.get("kind").and_then(Value::as_str) {
                    Some("add") => ToolCall::Write { path: path(one) },
                    Some("update") => ToolCall::Edit { path: path(one) },
                    _ => ToolCall::Patch {
                        path: tool::text(one, "path"),
                    },
                },
                _ => ToolCall::Patch { path: None },
            }
        }
        "webSearch" => ToolCall::WebSearch {
            query: tool::text(item, "query").unwrap_or_default(),
        },
        // `mcpToolCall`, `dynamicToolCall`, `collabAgentToolCall`,
        // `imageView`, `imageGeneration`, and whatever 0.149 adds. Named,
        // never guessed — and `tool_name` has already given them a label
        // (`mcp:server/tool`, `tool:name`) that the row renders.
        _ => ToolCall::Unknown { name: name.into() },
    }
}

/// What the tool was asked to do, as compactly as the provider stated it.
fn tool_input(kind: &str, item: &Value) -> String {
    let pick = |keys: &[&str]| {
        let mut m = serde_json::Map::new();
        for k in keys {
            if let Some(v) = item.get(*k) {
                if !v.is_null() {
                    m.insert((*k).to_string(), v.clone());
                }
            }
        }
        Value::Object(m).to_string()
    };
    match kind {
        "commandExecution" => pick(&["command", "cwd"]),
        "fileChange" => pick(&["changes"]),
        "mcpToolCall" => pick(&["server", "tool", "arguments"]),
        "dynamicToolCall" => pick(&["namespace", "tool", "arguments"]),
        "webSearch" => pick(&["query", "action"]),
        _ => compact(item),
    }
}

/// What came back. Not an inference — the provider's own output fields.
fn tool_result(kind: &str, item: &Value) -> String {
    match kind {
        "commandExecution" => {
            let out = item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or("");
            match item.get("exitCode").and_then(Value::as_i64) {
                Some(code) => format!("exit {code}\n{out}"),
                None => out.to_string(),
            }
        }
        "fileChange" => item
            .get("changes")
            .map(compact)
            .unwrap_or_else(|| compact(item)),
        "mcpToolCall" => item
            .get("error")
            .filter(|e| !e.is_null())
            .or_else(|| item.get("result"))
            .map(compact)
            .unwrap_or_else(|| compact(item)),
        _ => compact(item),
    }
}

fn user_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Frames and small helpers

/// JSON-RPC ids are `string | integer`. Rendered to a stable string for
/// eave's event surface; the original is kept for the reply.
fn render_id(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn error_reply(id: &Value, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        // -32603 internal error: the request was well-formed and this
        // client cannot service it. Not -32601, which would claim the
        // method does not exist.
        "error": { "code": -32603, "message": message },
    })
    .to_string()
}

fn method_not_found(id: &Value, method: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("Eave does not implement {method}") },
    })
    .to_string()
}

/// `thread/tokenUsage/updated` → the two token facts (Slice CC).
///
/// The Codex app-server does the separation for eave, and its own live
/// wire proves both readings. From comet's capture at 0.146.1
/// (`comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl`)
/// one thread reports `total.totalTokens` 13,837 → 27,760 → 41,725 →
/// 55,783 while `last.inputTokens` walks 13,747 → 13,858 → 13,944 →
/// 14,037. The first is a running bill and the second is the size of the
/// prompt actually in front of the model; only the second is occupancy,
/// and `total` is therefore read for nothing.
///
/// `last.inputTokens` is the FULL prompt (`totalTokens` = `inputTokens` +
/// `outputTokens` in every row of that capture, with `cachedInputTokens`
/// a portion of the input), so it needs no arithmetic — unlike Claude's.
fn token_usage(params: &Value) -> Option<SessionEvent> {
    let usage = params
        .get("tokenUsage")
        .or_else(|| params.get("token_usage"))?;
    let last = usage.get("last")?;
    let count = |key: &str| last.get(key).and_then(Value::as_u64);
    let input = count("inputTokens");
    let turn = TurnTokens::stated(
        input,
        count("outputTokens"),
        count("cachedInputTokens"),
        count("cacheWriteInputTokens"),
    );
    let context = input.map(|used| ContextWindow {
        used_tokens: used,
        limit_tokens: usage.get("modelContextWindow").and_then(Value::as_u64),
    });
    if turn.is_none() && context.is_none() {
        return None;
    }
    Some(SessionEvent::TokenUsage { turn, context })
}

fn compact(v: &Value) -> String {
    v.to_string()
}

fn malformed(line: &str, error: String) -> SessionEvent {
    let mut cut = MALFORMED_LINE_CAP.min(line.len());
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    SessionEvent::Malformed {
        line: line[..cut].to_string(),
        error,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice TN1. A turn under the default names neither key — Codex's
    /// own default is not a word eave knows. A tuned turn names both,
    /// the tier by the id `model/list` advertises.
    #[test]
    fn a_turn_carries_effort_and_the_fast_tier_only_when_set() {
        use crate::tuning::Tuning;
        let plain = turn_params("t1", vec![json!({"type":"text","text":"hi"})], &Tuning::default());
        assert_eq!(plain["threadId"], "t1");
        assert!(plain.get("effort").is_none() && plain.get("serviceTier").is_none(), "{plain}");
        let tuned = turn_params(
            "t1",
            vec![],
            &Tuning {
                model: None,
                effort: Some("ultra".into()),
                fast: true,
            },
        );
        assert_eq!(tuned["effort"], "ultra");
        assert_eq!(tuned["serviceTier"], "priority");
        assert!(tuned.get("model").is_none(), "no model until one is known");
        // Slice TN2: the model rides the same frame.
        let switched = turn_params(
            "t1",
            vec![],
            &Tuning {
                model: Some("gpt-5.6-sol".into()),
                ..Tuning::default()
            },
        );
        assert_eq!(switched["model"], "gpt-5.6-sol");
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// The shape that a per-argument predicate cannot see: `-c` is
    /// harmless on its own and the posture change is in the NEXT argv
    /// entry. This is the spelling a person actually types.
    #[test]
    fn a_config_override_is_blamed_wherever_its_key_lives() {
        for spelling in [
            &["-c", "approval_policy=never"][..],
            &["--config", "sandbox_mode=danger-full-access"][..],
            &["--config=approval_policy=never"][..],
            &["-capproval_policy=never"][..],
        ] {
            assert!(
                posture_undoing_arg(&args(spelling)).is_some(),
                "{spelling:?} would put the server in an unreviewed posture"
            );
        }
    }

    /// A config override that is not about the posture is the operator's
    /// business. Refusing every `-c` would be safe and would also make the
    /// flag useless.
    #[test]
    fn an_unrelated_config_override_is_allowed_through() {
        assert_eq!(posture_undoing_arg(&args(&["-c", "model=\"o3\""])), None);
        assert_eq!(posture_undoing_arg(&args(&["--strict-config"])), None);
        assert_eq!(posture_undoing_arg(&args(&[])), None);
        // A trailing `-c` with nothing after it is malformed, not
        // dangerous — the CLI will reject it.
        assert_eq!(posture_undoing_arg(&args(&["-c"])), None);
    }

    #[test]
    fn the_flags_that_switch_the_sandbox_or_the_reviewer_off_are_named() {
        for flag in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--approve-for-me",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "--profile",
            "--add-dir",
        ] {
            assert_eq!(
                posture_undoing_arg(&args(&[flag])).as_deref(),
                Some(flag),
                "{flag} must be refused"
            );
        }
    }

    /// Fail-closed on every axis, including the two that are easy to
    /// forget: a value this build has never heard of, and a server that
    /// named none at all.
    #[test]
    fn only_the_one_strict_posture_is_accepted() {
        let ok = ("untrusted", "user", "readOnly");
        assert!(
            posture_refusal(Some(ok.0), Some(ok.1), Some(ok.2), "x").is_none(),
            "the strict posture must be allowed to run"
        );
        for (p, r, s) in [
            ("on-request", "user", "readOnly"),
            ("never", "user", "readOnly"),
            ("untrusted", "auto_review", "readOnly"),
            ("untrusted", "guardian_subagent", "readOnly"),
            ("untrusted", "user", "workspaceWrite"),
            ("untrusted", "user", "dangerFullAccess"),
            ("a-policy-from-2027", "user", "readOnly"),
        ] {
            assert!(
                posture_refusal(Some(p), Some(r), Some(s), "x").is_some(),
                "{p}/{r}/{s} must be refused"
            );
        }
        assert!(posture_refusal(None, None, None, "x").is_some());
        assert!(posture_refusal(Some("untrusted"), Some("user"), None, "x").is_some());
    }

    /// JSON-RPC ids are typed. Codex numbers its requests, and a reply
    /// that stringifies the id is a reply the server will not match — so
    /// the rendered form is for eave's events only and the raw value is
    /// what goes back on the wire.
    #[test]
    fn an_id_renders_for_humans_and_survives_for_the_reply() {
        assert_eq!(render_id(&json!(7)), "7");
        assert_eq!(render_id(&json!("req-1")), "req-1");
        let reply = error_reply(&json!(7), "no");
        assert!(reply.contains(r#""id":7"#), "{reply}");
        assert!(!reply.contains(r#""id":"7""#), "{reply}");
    }

    /// The line between "a tool is running" and "the agent is thinking".
    /// Getting this wrong would make `in_flight_tools` count reasoning as
    /// work in flight, which is what the attention clocks branch on.
    #[test]
    fn only_tool_items_are_counted_as_tools() {
        let item = json!({ "server": "node_repl", "tool": "eval" });
        assert_eq!(
            tool_name("commandExecution", &item).as_deref(),
            Some("commandExecution")
        );
        assert_eq!(
            tool_name("mcpToolCall", &item).as_deref(),
            Some("mcp:node_repl/eval")
        );
        for not_a_tool in [
            "reasoning",
            "agentMessage",
            "userMessage",
            "plan",
            "contextCompaction",
        ] {
            assert_eq!(tool_name(not_a_tool, &item), None, "{not_a_tool}");
        }
    }
}

/// **Slice CC — token accounting.**
///
/// eave has no Codex auth on this machine, so no turn has ever reached a
/// model here and there is no eave-side capture of a usage frame. The
/// lines below are verbatim from comet's live capture of the real binary
/// at 0.146.1 (`comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl`,
/// lines 35 and 46) — the same wire, recorded by the sibling app that
/// does have auth. That provenance is stated rather than hidden: these
/// are real bytes from a real server, and they are not eave's.
#[cfg(test)]
mod usage_tests {
    use super::*;

    const FIRST: &str = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"01a00fe7-c301-7d52-ae1f-41ecda48b29b","turnId":"01a00fe7-c36a-78d0-a6c4-37b2fcb27aca","tokenUsage":{"total":{"totalTokens":13837,"inputTokens":13747,"cachedInputTokens":11008,"cacheWriteInputTokens":0,"outputTokens":90,"reasoningOutputTokens":0},"last":{"totalTokens":13837,"inputTokens":13747,"cachedInputTokens":11008,"cacheWriteInputTokens":0,"outputTokens":90,"reasoningOutputTokens":0},"modelContextWindow":258400}},"emittedAtMs":1786973247805}"#;

    const SECOND: &str = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"01a00fe7-c301-7d52-ae1f-41ecda48b29b","turnId":"01a00fe7-c36a-78d0-a6c4-37b2fcb27aca","tokenUsage":{"total":{"totalTokens":27760,"inputTokens":27605,"cachedInputTokens":22016,"cacheWriteInputTokens":0,"outputTokens":155,"reasoningOutputTokens":0},"last":{"totalTokens":13923,"inputTokens":13858,"cachedInputTokens":11008,"cacheWriteInputTokens":0,"outputTokens":65,"reasoningOutputTokens":0},"modelContextWindow":258400}},"emittedAtMs":1786973250476}"#;

    fn usage(line: &str) -> SessionEvent {
        let v: Value = serde_json::from_str(line).unwrap();
        let mut proto = CodexProtocol::new();
        match proto.parse_line(line).events.as_slice() {
            [event] => event.clone(),
            other => panic!("{v:?} gave {other:?}"),
        }
    }

    /// `last` is the turn, `total` is the session, and only `last` is
    /// read. The second frame is the proof: `total.outputTokens` has
    /// reached 155 while the turn that just ran produced 65.
    #[test]
    fn the_turn_is_read_from_last_and_never_from_the_running_total() {
        let SessionEvent::TokenUsage { turn, context } = usage(SECOND) else {
            panic!("expected TokenUsage");
        };
        let turn = turn.expect("stated");
        assert_eq!(turn.output, Some(65), "65 is the turn; 155 is the session");
        assert_eq!(turn.input, Some(13_858));
        assert_eq!(turn.cache_read, Some(11_008));
        assert_eq!(turn.cache_write, Some(0), "a stated zero is a stated zero");
        assert_eq!(
            context,
            Some(ContextWindow {
                used_tokens: 13_858,
                limit_tokens: Some(258_400),
            })
        );
    }

    /// Occupancy creeps; the bill doubles. Between these two frames the
    /// running total went 13,837 → 27,760 while the window went 13,747 →
    /// 13,858 — 111 tokens, which is what one short exchange adds.
    #[test]
    fn the_context_creeps_where_the_running_total_doubles() {
        let (
            SessionEvent::TokenUsage { context: a, .. },
            SessionEvent::TokenUsage { context: b, .. },
        ) = (usage(FIRST), usage(SECOND))
        else {
            panic!("expected TokenUsage");
        };
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(b.used_tokens - a.used_tokens, 111);
        assert_eq!(a.limit_tokens, b.limit_tokens);
    }

    /// A usage frame this build cannot read is journaled verbatim rather
    /// than dropped or guessed at — the same contract every other unknown
    /// notification gets.
    #[test]
    fn an_unreadable_usage_frame_is_kept_verbatim() {
        let line = r#"{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"outputTokens":1}}}}"#;
        match usage(line) {
            SessionEvent::System { subtype, raw } => {
                assert_eq!(subtype, "thread/tokenUsage/updated");
                assert_eq!(raw, line, "byte for byte");
            }
            other => panic!("got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Thread attribution.

    /// **Replayed through this parser from comet's live capture**, which
    /// is the only multi-thread Codex bytes that exist in either repo:
    /// `comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl`.
    ///
    /// Three threads share the connection. The parent starts at line 9
    /// and completes at line 144; two children complete at lines 104 and
    /// 134, in between. Before the attribution guard eave emitted a
    /// `TurnCompleted` for all three, so the parent turn was closed twice,
    /// early — and each close clears both `outstanding` and
    /// `pending_approvals` in `attention.rs`, silently dropping an open
    /// permission question out of the attention model while its card is
    /// still on screen.
    ///
    /// The assertion is EXACTLY ONE `TurnCompleted`, not "fewer than
    /// three": the parent's own end must still arrive, or the fix would
    /// have traded a turn that closes too early for one that never closes.
    #[test]
    fn a_child_threads_turn_never_settles_eaves_own() {
        const CAPTURE: &str = "/Users/michaelstang/Desktop/eave/comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl";
        let Ok(text) = std::fs::read_to_string(CAPTURE) else {
            // The capture lives in a sibling repo. Absent, this test
            // states that rather than passing on an empty file.
            eprintln!("skipping: {CAPTURE} not present");
            return;
        };

        let mut p = CodexProtocol::new();
        // Adopt the PARENT thread, exactly as `thread/start` would.
        p.thread_id = Some("01a00fe7-c301-7d52-ae1f-41ecda48b29b".into());

        let (mut ours, mut theirs) = (0, 0);
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(method) = v.get("method").and_then(Value::as_str) else {
                continue;
            };
            for ev in p.notification(method, &v, line).events {
                match ev {
                    SessionEvent::TurnCompleted { .. } => ours += 1,
                    // Slice CJ moved the carrier from a `System` subtype
                    // string to the `ForeignThread` wrapper. The rule the
                    // test asserts is unchanged and so is the count.
                    SessionEvent::ForeignThread { .. } => theirs += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(
            ours, 1,
            "one prompt, one turn end: the parent's, at line 144"
        );
        assert!(
            theirs > 0,
            "the children's frames must be RECORDED as somebody else's, not dropped"
        );
    }

    /// Line 82 of the same capture, verbatim: the beta child thread runs a
    /// shell command.
    const CHILD_EXEC: &str = r#"{"method":"item/started","params":{"item":{"type":"commandExecution","id":"exec-eb5c5acb-e779-4020-8af2-f4ffcf4eb07e","pluginId":null,"scriptPath":null,"command":"/usr/bin/zsh -lc 'echo beta-done > beta.txt'","cwd":"/tmp/probe-codex","processId":"62551","source":"unifiedExecStartup","status":"inProgress","commandActions":[{"type":"unknown","command":"echo beta-done > beta.txt"}],"aggregatedOutput":null,"exitCode":null,"durationMs":null},"threadId":"01a00fe7-e358-7a02-8745-e7a0793013ce","turnId":"01a00fe7-e375-7250-b528-014e85e7a18e","startedAtMs":1786973255872},"emittedAtMs":1786973255872}"#;

    const PARENT: &str = "01a00fe7-c301-7d52-ae1f-41ecda48b29b";
    const BETA: &str = "01a00fe7-e358-7a02-8745-e7a0793013ce";

    /// Parse one line as the parent thread would see it.
    fn as_parent(p: &mut CodexProtocol, line: &str) -> Vec<SessionEvent> {
        p.parse_line(line).events
    }

    /// **The defect Slice CJ exists to remove.** CH attributed a child's
    /// frame correctly and then threw the decode away: every child frame
    /// became `System { subtype: "foreign_thread:<method>" }`, one opaque
    /// string standing in for a typed tool call, so nothing downstream
    /// could ever widen it again.
    ///
    /// The frame is decoded by the SAME parser eave runs its own frames
    /// through — `ToolCall::Run` with the command intact — and only then
    /// marked as somebody else's.
    #[test]
    fn a_child_threads_tool_call_arrives_typed_rather_than_stringified() {
        let mut p = CodexProtocol::new();
        p.thread_id = Some(PARENT.into());

        let events = as_parent(&mut p, CHILD_EXEC);
        let [SessionEvent::ForeignThread { thread_id, event }] = events.as_slice() else {
            panic!("expected one attributed frame, got {events:?}");
        };
        assert_eq!(thread_id, BETA, "attributed to the thread that owns it");
        let SessionEvent::ToolUseStarted { id, name, call, .. } = event.as_ref() else {
            panic!("a child's tool call must stay a tool call, got {event:?}");
        };
        assert_eq!(id, "exec-eb5c5acb-e779-4020-8af2-f4ffcf4eb07e");
        assert_eq!(name, "commandExecution");
        assert_eq!(
            call.as_ref(),
            Some(&ToolCall::Run {
                command: "/usr/bin/zsh -lc 'echo beta-done > beta.txt'".into()
            }),
            "the typed decode is the whole point: a string cannot be widened back"
        );
    }

    /// Decoding a child's frame must not RECORD it. The two acts are split
    /// (`decode_threaded` has no `self`, `record` is the only writer), and
    /// this is the assertion that they stay split: a child's shell command
    /// is not eave's outstanding tool, so `in_flight_tools` must not count
    /// it and the item cache — which a later approval reads paths out of —
    /// must not hold it.
    #[test]
    fn a_child_threads_tool_never_enters_eaves_in_flight_set() {
        let mut p = CodexProtocol::new();
        p.thread_id = Some(PARENT.into());
        as_parent(&mut p, CHILD_EXEC);
        assert_eq!(p.in_flight(), 0, "a child's command is not eave's work");
        assert!(
            p.items.is_empty(),
            "nor is its item eave's to read paths from"
        );

        // The same frame under eave's OWN thread is counted, so the test
        // above is about attribution and not about a decoder that quietly
        // stopped booking anything.
        let mine = CHILD_EXEC.replace(BETA, PARENT);
        let mut p = CodexProtocol::new();
        p.thread_id = Some(PARENT.into());
        as_parent(&mut p, &mine);
        assert_eq!(p.in_flight(), 1);
        assert_eq!(p.items.len(), 1);
    }

    /// Replayed from the live capture: every typed thing the children did
    /// survives attribution. Four shell commands across two threads, their
    /// token usage, and their two turn ends — each one a typed
    /// `SessionEvent` inside the wrapper rather than a subtype string.
    ///
    /// The counts are read off the capture (`item/started` with
    /// `type: commandExecution` on a non-parent `threadId`), never off the
    /// code under test.
    #[test]
    fn every_child_frame_the_capture_carries_survives_as_a_typed_event() {
        const CAPTURE: &str = "/Users/michaelstang/Desktop/eave/comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl";
        let Ok(text) = std::fs::read_to_string(CAPTURE) else {
            eprintln!("skipping: {CAPTURE} not present");
            return;
        };

        let mut p = CodexProtocol::new();
        p.thread_id = Some(PARENT.into());

        let (mut commands, mut usage, mut child_ends, mut threads) =
            (Vec::new(), 0, 0, std::collections::BTreeSet::new());
        for line in text.lines() {
            for ev in p.parse_line(line).events {
                let SessionEvent::ForeignThread { thread_id, event } = ev else {
                    continue;
                };
                threads.insert(thread_id);
                match *event {
                    SessionEvent::ToolUseStarted {
                        call: Some(ToolCall::Run { command }),
                        ..
                    } => commands.push(command),
                    SessionEvent::TokenUsage { .. } => usage += 1,
                    SessionEvent::TurnCompleted { .. } => child_ends += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(threads.len(), 2, "two children share the connection");
        assert_eq!(
            commands.len(),
            3,
            "the children's shell commands, typed: lines 68, 82, 112 — got {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("alpha-done"))
                && commands.iter().any(|c| c.contains("beta-done")),
            "both children's work is legible: {commands:?}"
        );
        assert_eq!(usage, 5, "the children's token usage stays a number");
        assert_eq!(child_ends, 2, "both children's turn ends, owned by them");
        assert_eq!(p.in_flight(), 0, "and none of it is eave's in-flight work");
    }
}
