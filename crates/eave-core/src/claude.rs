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

use std::collections::HashMap;
use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::events::SessionEvent;
use crate::media::{self, EncodedImage, ImageRef};
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, RewindOutcome, RewindTarget, SessionError,
    SessionWatch, SpawnConfig,
};
use crate::stream::ClaudeStream;
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

/// How long a control request that expects a REPLY may wait for one.
///
/// Only `rewind_conversation` does; `interrupt` is fire-and-forget. The
/// bound exists because the reply is synchronous with the operator's
/// click, and a CLI that never answers must not park the UI: on expiry
/// eave reports the timeout as a refusal, which is true — the
/// conversation was not rewound.
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Outbound control requests still waiting for their reply, by
    /// `request_id` (Slice AG1 #10).
    ///
    /// Empty except for the few milliseconds one is in flight, which is
    /// what makes the reader's check affordable: an uncontended
    /// `is_empty()` per line, and no second parse of any frame in the
    /// normal case.
    pending_control: Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>,
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
        if let Some(cursor) = &cfg.resume {
            // Claude is the one provider whose resume cursor equals the id
            // eave proposed, because `--session-id` is honoured. So this
            // reads `cfg.resume` rather than `cfg.session_id` not to change
            // behaviour here but to stop this file being the template the
            // other adapters copied — the cursor is the provider's fact.
            cmd.args(["--resume", cursor]);
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
        // Tuning (`crate::tuning`): both knobs are argv-only on this CLI
        // (2.1.258 has `set_model` and `set_max_thinking_tokens` control
        // requests but no `set_effort` and no fast-mode one), so a change
        // is a restart — the engine's job, not this adapter's.
        cmd.args(tuning_args(&cfg.tuning));
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
            pending_control: Mutex::new(HashMap::new()),
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
                // A reply to one of eave's OWN control requests, claimed
                // before the parser sees it. `stream.rs` drops every
                // `control_response` — correctly, since the echo of a
                // decision eave already journalled carries nothing new —
                // so a request that expects an ANSWER has to take it here.
                if claim_control_reply(&reader_shared, &line) {
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

/// The reference registration (Slice AT). Nothing about how this adapter
/// behaves changed when the registry landed — `spawn` below forwards to
/// the inherent one, byte for byte the same process with the same flags.
/// What changed is that the engine now finds it by id instead of naming
/// the type.
impl crate::provider::AgentAdapter for ClaudeCodeSession {
    const PROVIDER_ID: &'static str = "claude-code";

    /// Slice K's proof, and the pointer to where it is enforced: this
    /// adapter's stdout reader checks the CLI's own `init.permissionMode`
    /// against [`ASKING_MODES`] on every spawn and emits
    /// `SessionEvent::PostureRefused` when it cannot prove the CLI asks —
    /// see [`posture_refusal`]. The engine kills the session on that
    /// event. It is a per-spawn runtime check, not a claim in a table.
    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "claude.rs `posture_refusal`: the CLI's own init.permissionMode is checked \
                   against ASKING_MODES on every spawn, and a session that cannot prove it asks \
                   emits PostureRefused and is killed",
    };

    const REWIND: crate::provider::RewindSupport = crate::provider::RewindSupport::Native {
        evidence: "claude.rs `rewind`: the `rewind_conversation` control request, on the same \
                   channel `interrupt` uses. DRIVEN LIVE 2026-08-31 against 2.1.251 — two \
                   prompts, then a rewind to the first: it answered `rewound: true` with the \
                   target's own text in `prefillText`, and the agent then denied any earlier \
                   message existed, so the truncation reaches the CONTEXT and is not cosmetic.",
    };

    const TUNING: crate::tuning::TuningSupport = crate::tuning::TuningSupport {
        effort: crate::tuning::EffortSupport::Levels {
            levels: &["low", "medium", "high", "xhigh", "max"],
            applies: crate::tuning::Applies::NextSpawn,
            evidence: "`claude --help` 2.1.258: `--effort <level>` \"(low, medium, high, xhigh, \
                       max)\" — measured 2026-09-02. Argv only: the control channel has \
                       `set_model` and `set_max_thinking_tokens` but no `set_effort`.",
        },
        speed: crate::tuning::SpeedSupport::Toggle {
            applies: crate::tuning::Applies::NextSpawn,
            evidence: "2.1.258 carries the settings key `fastMode` and a headless \
                       disabled-reason `sdk_opt_in_required` (\"Fast mode is not available \
                       in the Agent SDK\"), so eave opts in with `--settings \
                       '{\"fastMode\":true}'`. The opt-in path was read from the binary \
                       2026-09-02, NOT walked live: a paid-plan check (\"Fast mode requires \
                       a paid subscription\") sits in front of it and eave cannot see \
                       which answer this machine gets.",
        },
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        ClaudeCodeSession::spawn(cfg)
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

    async fn send_user_message(
        &mut self,
        text: &str,
        images: &[ImageRef],
    ) -> Result<(), SessionError> {
        write_frame(&self.stdin, &user_frame(text, &media::encode_all(images)?)).await
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

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        let request_id = format!(
            "eave-interrupt-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" },
        });
        write_frame(&self.stdin, &frame.to_string()).await
    }

    /// `rewind_conversation` — the CLI's own truncation, on the same
    /// control channel `interrupt` uses.
    ///
    /// **Measured before it was written** (2026-08-31, 2.1.251): two
    /// prompts, then a rewind to the first, answered
    /// `{"rewound":true,"targetMessageUuid":…,"prefillText":"Say exactly:
    /// ALPHA","precedingAssistantUuid":null}`. Asked afterwards what it
    /// had been told first, the agent answered that the conversation had
    /// only just begun — so this truncates the CONTEXT, and eave is not
    /// merely hiding rows.
    ///
    /// Every refusal is the CLI's own word, forwarded verbatim. eave does
    /// not pre-check any of them, and the reason is `last_seen_msg_id`:
    /// `unseen later turn` is a race the client CANNOT evaluate, because
    /// the fact it turns on is whether the provider's tail moved after
    /// eave last rendered it. Asking is the only correct implementation.
    async fn rewind(&mut self, target: &RewindTarget) -> Result<RewindOutcome, SessionError> {
        let Some(msg_id) = target.msg_id.as_deref() else {
            // Not a refusal from Claude — a refusal by eave, because a
            // rewind has to name a message and this row never got a name.
            return Ok(RewindOutcome::Refused {
                reason: "eave never saw an id for that message".into(),
            });
        };
        let request_id = format!(
            "eave-rewind-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending_control
            .lock()
            .unwrap()
            .insert(request_id.clone(), tx);

        let mut request = serde_json::json!({
            "subtype": "rewind_conversation",
            "target_message_uuid": msg_id,
            // **Deliberately false, and this is the load-bearing choice
            // in the whole slice.**
            //
            // `true` lets the CLI interrupt a running turn and rewind
            // through it — but the turn's messages are then DISCARDED, so
            // the `result` frame that would have closed it never arrives.
            // eave's attention ledger has a documented hole exactly there
            // (a turn nothing can close makes a healthy session report
            // `quiet`, then `unresponsive`), and that hole is not fixed.
            // Manufacturing the one input it cannot survive, to save the
            // operator a click, is a bad trade.
            //
            // So a turn in flight makes the CLI answer `turn running`,
            // eave shows that sentence, and the operator presses Stop
            // first — through the interrupt path that already settles a
            // turn properly.
            "interrupt_if_running": false,
        });
        if let Some(seen) = &target.last_seen_msg_id {
            request["last_seen_user_message_uuid"] = serde_json::Value::String(seen.clone());
        }
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if let Err(e) = write_frame(&self.stdin, &frame.to_string()).await {
            self.shared
                .pending_control
                .lock()
                .unwrap()
                .remove(&request_id);
            return Err(e);
        }

        let reply = match tokio::time::timeout(CONTROL_REPLY_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            // Timed out, or the reader task went away with the process.
            // Both leave the map holding a sender nobody will ever use.
            _ => {
                self.shared
                    .pending_control
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                return Ok(RewindOutcome::Refused {
                    reason: "the CLI did not answer the rewind".into(),
                });
            }
        };
        Ok(rewind_outcome(&reply))
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
///
/// `pub` since Slice S2, read-only: the Agents pane's test walks it to
/// hold the pane's posture words and the rail's to the same fact. The
/// gate itself still lives here, in [`posture_refusal`].
pub const ASKING_MODES: [&str; 2] = ["default", "manual"];

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

/// Claude stream-json user turn. Image blocks are T3's
/// `buildClaudeImageContentBlock` (base64 source object).
fn user_frame(text: &str, images: &[EncodedImage]) -> String {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for img in images {
        content.push(media::claude_image_block(img));
    }
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": content },
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

/// Read `rewind_conversation`'s reply.
///
/// Pure, and separate from the send, so the shapes below can be pinned by
/// tests against captured frames rather than a live CLI.
///
/// **A missing `rewound` is a refusal, never a success.** The one thing
/// this must never do is report a rewind that did not happen: the
/// operator would carry on typing into a conversation still holding
/// everything they asked to discard.
fn rewind_outcome(reply: &serde_json::Value) -> RewindOutcome {
    let response = reply.get("response");
    let subtype = response
        .and_then(|r| r.get("subtype"))
        .and_then(serde_json::Value::as_str);
    if subtype == Some("error") {
        return RewindOutcome::Refused {
            reason: response
                .and_then(|r| r.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the CLI refused the rewind without saying why")
                .to_string(),
        };
    }
    // `response.response` — the payload is nested one deeper than the
    // envelope's own status, which is why this is not a single `get`.
    let payload = response.and_then(|r| r.get("response"));
    let rewound = payload
        .and_then(|p| p.get("rewound"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !rewound {
        return RewindOutcome::Refused {
            reason: payload
                .and_then(|p| p.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the CLI refused the rewind without saying why")
                .to_string(),
        };
    }
    RewindOutcome::Rewound {
        prefill: payload
            .and_then(|p| p.get("prefillText"))
            .and_then(serde_json::Value::as_str)
            // An empty prefill is not a prefill: seeding the composer with
            // "" and seeding it with nothing are the same act, and `Some("")`
            // would make every caller check for it.
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    }
}

/// Hand a `control_response` to whoever is waiting for it, if anyone is.
///
/// Returns `true` when the line was claimed, and the reader then drops it
/// — the same fate `stream.rs` gives every other control response, so no
/// frame is handled twice and none becomes a journal record.
///
/// The `is_empty` guard is the whole reason this is affordable on the hot
/// path: nothing is ever pending except while an operator's rewind is in
/// flight, so the common case is one uncontended lock and no JSON parse.
fn claim_control_reply(shared: &Shared, line: &str) -> bool {
    if shared.pending_control.lock().unwrap().is_empty() {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if v.get("type").and_then(serde_json::Value::as_str) != Some("control_response") {
        return false;
    }
    // The id sits INSIDE `response`, not beside `type` — the shape of an
    // outbound reply is not the mirror of an inbound request. Read off a
    // live 2.1.251 reply, not assumed.
    let Some(id) = v
        .get("response")
        .and_then(|r| r.get("request_id"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let waiting = shared.pending_control.lock().unwrap().remove(id);
    match waiting {
        Some(tx) => {
            let _ = tx.send(v);
            true
        }
        // Somebody else's reply (an approval echo). Not ours to eat.
        None => false,
    }
}

/// Best-effort write from the reader task, which has no one to report to:
/// a closed pipe means the child is already going away.
async fn write_line(stdin: &SharedStdin, line: &str) {
    let _ = write_frame(stdin, line).await;
}

/// The argv this CLI takes for a session's tuning (Slice TN1). `--effort`
/// is the flag `--help` documents; fast mode has no flag, so it rides
/// `--settings` as a JSON string — the binary's own headless
/// disabled-reason is `sdk_opt_in_required`, and this is the opt-in.
/// Nothing for the default: absent is the CLI's own choice, and an empty
/// `--effort` is not the same thing.
fn tuning_args(t: &crate::tuning::Tuning) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(level) = &t.effort {
        out.push("--effort".into());
        out.push(level.clone());
    }
    if t.fast {
        out.push("--settings".into());
        out.push(r#"{"fastMode":true}"#.into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::RewindOutcome;

    /// Slice TN1. The default passes NOTHING; a level is `--effort`
    /// verbatim; fast mode is the settings opt-in, as JSON the CLI's
    /// `--settings` accepts inline.
    #[test]
    fn tuning_is_argv_and_the_default_is_silence() {
        use crate::tuning::Tuning;
        assert!(tuning_args(&Tuning::default()).is_empty());
        let both = tuning_args(&Tuning {
            effort: Some("xhigh".into()),
            fast: true,
        });
        assert_eq!(
            both,
            vec!["--effort", "xhigh", "--settings", r#"{"fastMode":true}"#]
        );
        let v: serde_json::Value = serde_json::from_str(&both[3]).expect("settings is JSON");
        assert_eq!(v["fastMode"], true);
        assert!(!<ClaudeCodeSession as crate::provider::AgentAdapter>::TUNING
            .levels()
            .is_empty());
    }

    /// **Verbatim from the wire**, 2026-08-31, claude 2.1.251 driven in
    /// eave's exact spawn posture: two prompts, then a rewind to the
    /// first. Written down rather than paraphrased because the nesting is
    /// the part that is easy to get wrong — the envelope carries
    /// `subtype`/`request_id`, and the payload is one level deeper again.
    const REAL_REWIND_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"rw1","response":{"rewound":true,"targetMessageUuid":"fb99bae1-4e74-4061-964c-c05b50d897c5","prefillText":"Say exactly: ALPHA","precedingAssistantUuid":null}}}"#;

    fn outcome(line: &str) -> RewindOutcome {
        rewind_outcome(&serde_json::from_str(line).expect("valid json"))
    }

    #[test]
    fn the_captured_reply_is_a_rewind_that_hands_back_the_message() {
        assert_eq!(
            outcome(REAL_REWIND_REPLY),
            RewindOutcome::Rewound {
                prefill: Some("Say exactly: ALPHA".into())
            }
        );
    }

    /// The CLI's own refusals, each a real string from its handler. eave
    /// forwards them verbatim rather than mapping them onto a vocabulary
    /// of its own — "turn running" and "unseen later turn" call for
    /// different actions from the operator, and a house word for both
    /// would erase that.
    #[test]
    fn a_refusal_is_reported_in_the_clis_own_words() {
        for reason in [
            "turn running",
            "prompt pending",
            "commands queued",
            "target not found",
            "stale target",
            "unseen later turn",
        ] {
            let line = format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"r","response":{{"rewound":false,"prefillText":null,"error":"{reason}"}}}}}}"#
            );
            assert_eq!(
                outcome(&line),
                RewindOutcome::Refused {
                    reason: reason.into()
                }
            );
        }
    }

    /// **The one failure mode that must never happen.** A reply eave
    /// cannot read is a rewind that did NOT happen — reporting success
    /// would drop the operator's rows while the agent still remembers
    /// every one of them, and nothing on screen would say so.
    #[test]
    fn an_unreadable_reply_is_a_refusal_and_never_a_success() {
        for line in [
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{}}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r"}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r","error":"boom"}}"#,
            r#"{"type":"control_response"}"#,
            r#"{}"#,
        ] {
            assert!(
                matches!(outcome(line), RewindOutcome::Refused { .. }),
                "must not read as rewound: {line}"
            );
        }
    }

    /// An error envelope carries its reason beside `subtype`, not nested.
    #[test]
    fn an_error_envelope_states_its_own_reason() {
        assert_eq!(
            outcome(
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"r","error":"boom"}}"#
            ),
            RewindOutcome::Refused {
                reason: "boom".into()
            }
        );
    }

    /// `Some("")` and `None` would make every caller test for both. An
    /// empty prefill is no prefill.
    #[test]
    fn an_empty_prefill_is_no_prefill() {
        assert_eq!(
            outcome(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"rewound":true,"prefillText":""}}}"#
            ),
            RewindOutcome::Rewound { prefill: None }
        );
    }

    /// A rewind cannot be asked about a message the provider never named,
    /// and eave refuses it ITSELF rather than writing a frame the CLI
    /// would reject (`target_message_uuid` must be a string).
    ///
    /// **Told apart by the stdin, which is closed.** With no `msg_id` the
    /// answer is `Refused` — reached without touching the pipe. With one,
    /// the same call dies on the closed pipe instead. Two different
    /// results from one broken session is the proof that the `None` arm
    /// returns before `write_frame`, and an assertion on the refusal alone
    /// could not tell them apart.
    #[tokio::test]
    async fn a_message_with_no_provider_id_never_reaches_the_pipe() {
        let (_tx, exit_rx) = watch::channel(None);
        let mut session = ClaudeCodeSession {
            identity: ProcIdentity {
                pid: 1,
                start_tvsec: 0,
                start_tvusec: 0,
            },
            pgid: 1,
            stdin: Arc::new(tokio::sync::Mutex::new(None)),
            shared: Arc::new(Shared {
                stream: Mutex::new(ClaudeStream::new()),
                pending_control: Mutex::new(HashMap::new()),
                last_activity: Mutex::new(Instant::now()),
                saw_init: AtomicBool::new(false),
                kill_initiated: AtomicBool::new(false),
            }),
            exit_rx,
            stdout_eof: Arc::new(AtomicBool::new(false)),
            exited_flag: Arc::new(AtomicBool::new(false)),
        };

        let unnamed = session.rewind(&RewindTarget::default()).await;
        assert!(
            matches!(unnamed, Ok(RewindOutcome::Refused { .. })),
            "an unaddressable message is refused here, not on the wire: {unnamed:?}"
        );
        assert!(
            session.shared.pending_control.lock().unwrap().is_empty(),
            "nothing may be registered for a request that was never sent"
        );

        let named = session
            .rewind(&RewindTarget {
                msg_id: Some("u-1".into()),
                ..Default::default()
            })
            .await;
        assert!(
            matches!(named, Err(SessionError::StdinClosed)),
            "with an id it really does try the pipe — which is what makes \
             the assertion above mean something: {named:?}"
        );
        assert!(
            session.shared.pending_control.lock().unwrap().is_empty(),
            "a failed write must take its waiter back out of the map — a \
             sender left behind is a slot that is never claimed and never \
             freed, and the reader would pay to parse every frame forever"
        );
    }

    /// The reader claims a reply only for a request eave is waiting on.
    /// An approval echo shares the frame TYPE and must be left alone —
    /// eating it would silently drop another slice's traffic.
    #[test]
    fn the_reader_claims_only_its_own_replies() {
        let shared = Shared {
            stream: Mutex::new(ClaudeStream::new()),
            pending_control: Mutex::new(HashMap::new()),
            last_activity: Mutex::new(Instant::now()),
            saw_init: AtomicBool::new(false),
            kill_initiated: AtomicBool::new(false),
        };
        // Nothing pending: the hot-path guard refuses without parsing.
        assert!(!claim_control_reply(&shared, REAL_REWIND_REPLY));

        let (tx, mut rx) = oneshot::channel();
        shared
            .pending_control
            .lock()
            .unwrap()
            .insert("rw1".into(), tx);
        // Somebody else's reply, with one of ours outstanding.
        assert!(!claim_control_reply(
            &shared,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"someone-else"}}"#
        ));
        assert!(rx.try_recv().is_err(), "our waiter must still be waiting");
        assert_eq!(shared.pending_control.lock().unwrap().len(), 1);

        assert!(claim_control_reply(&shared, REAL_REWIND_REPLY));
        assert!(rx.try_recv().is_ok(), "the waiter is handed the reply");
        assert!(
            shared.pending_control.lock().unwrap().is_empty(),
            "a claimed reply is removed, so a second copy is not eaten"
        );
    }
}
