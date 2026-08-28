//! Native Pi adapter: spawn the `pi` CLI in `--mode rpc`, parse its JSONL
//! event stream, own its process group, kill it with proof of exit.
//!
//! Structurally this is [`crate::claude`] with a different wire format, and
//! it deliberately keeps that file's shape — same reader/stderr/supervisor
//! task split, same `kill_initiated` stale-pipe rule, same "verify the
//! posture, never assume it" spine. Read `claude.rs` first; only the
//! differences are explained here.
//!
//! # Pi announces nothing, so eave asks
//!
//! Claude Code opens with a `system/init` line that names both the session
//! id and the permission mode. **Pi's RPC mode emits no startup banner at
//! all** — the first bytes on stdout are the response to whatever the
//! client sent first (verified 2026-08-20: a bare `pi --mode rpc` sits
//! silent until spoken to). So this adapter opens with a two-command
//! handshake of its own, [`PROBE_STATE_ID`] and [`PROBE_GATE_ID`], and
//! synthesises the `Init` event from the answers. Until both land there is
//! no `Init`, and [`PiSession::send_user_message`] refuses.
//!
//! # The permission posture is something eave installs, not something Pi has
//!
//! This is the load-bearing difference from Claude and the reason this
//! adapter is longer than it looks like it should be.
//!
//! Claude Code has a built-in permission system; Slice K's job was to find
//! the flag combination that routes its questions to eave. **Pi has no such
//! system.** Its own security documentation is explicit — "Pi does not
//! include a built-in sandbox. Built-in tools can read files, write files,
//! edit files, and run shell commands with the permissions of the pi
//! process" — and there is no `--permission-mode`, no
//! `--permission-prompt-tool`, and no approval message anywhere in the RPC
//! protocol. Measured, not inferred: a bare `pi --mode rpc` driven to call
//! `bash` executed the command with **zero** approval traffic on stdout
//! (capture 2026-08-20, "NO GATE" run).
//!
//! What Pi does have is an extension API whose `tool_call` hook fires
//! *before* a tool executes and can refuse it, and whose `ctx.ui.*` dialogs
//! are transported over RPC as `extension_ui_request` / `extension_ui_response`
//! frames. So eave ships its own gate ([`GATE_SOURCE`]), loads it with
//! `-e`, and **is** Pi's permission system for the life of the session.
//!
//! That inverts the trust question. For Claude the risk is a CLI that
//! reports a mode which does not ask; here the risk is a gate that failed
//! to load, leaving a Pi that asks nothing of anybody. So the gate
//! registers a marker command and this adapter refuses the session unless
//! `get_commands` names it, sourced from the exact path we wrote — see
//! [`PiStream::verify_gate`]. Fail-closed, same principle as Slice K's
//! `ASKING_MODES`, applied to a different failure.
//!
//! # One thing this adapter must never do
//!
//! Pi's RPC protocol has a client-invoked `{"type":"bash"}` command that
//! runs a shell command directly. It is dispatched as a `user_bash`
//! extension event, **not** as `tool_call`, so [`GATE_SOURCE`] does not see
//! it and cannot refuse it. It is an unmediated shell escape. eave never
//! sends it, and nothing in this file constructs it; if a future slice
//! wants "run a command in the session's shell", it needs its own approval
//! story rather than this one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::events::{cap_payload, SessionEvent};
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{
    AgentSession, ApprovalDecision, ImageRef, KillReport, SessionError, SessionWatch, SpawnConfig,
};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

const STDERR_RING_CAP: usize = 4096;
const EVENT_CHANNEL_CAP: usize = 256;
const MALFORMED_LINE_CAP: usize = 512;

/// Correlation ids for the opening handshake. Namespaced so they cannot
/// collide with anything the engine might send later.
const PROBE_STATE_ID: &str = "eave-init-state";
const PROBE_GATE_ID: &str = "eave-init-gate";

/// The slash command [`GATE_SOURCE`] registers purely so its presence can
/// be proven over the wire. It does nothing when invoked; it exists to be
/// *listed*.
const GATE_MARKER_COMMAND: &str = "eave-gate-installed";

/// The `title` of the `ctx.ui.input` dialog the gate raises. Any other
/// dialog on the wire belongs to some other extension and is refused
/// rather than shown as an approval.
const GATE_DIALOG_TITLE: &str = "eave-approval-v1";

/// What `Init::permission_mode` reports for a verified Pi session.
///
/// Deliberately not one of Claude's mode names: this is not a mode Pi is
/// running in, it is a gate eave installed. A reader who sees this string
/// should come here and learn that, rather than assume Pi has a posture of
/// its own.
const GATE_POSTURE: &str = "eave-gate";

/// How long the opening handshake may take before the session is refused.
///
/// **Measured, not picked.** Six consecutive cold `pi --mode rpc` starts on
/// the development machine (2026-08-20) answered both probes in 596, 611,
/// 621, 634, 1044 and 1053 ms — the spread is Node process start plus
/// on-the-fly TypeScript transpilation of the gate. 10 s is roughly ten
/// times the worst observation, which leaves room for a cold page cache or
/// a loaded machine while still being a bound rather than a hang.
///
/// This is not an attention clock and deliberately does not live in
/// `thresholds.rs`, which owns "every attention/kill timing constant": it
/// is a spawn-time protocol deadline local to this adapter.
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(10);

/// eave's approval gate, as a Pi extension.
///
/// Written to a temp file at spawn and loaded with `-e` (see
/// [`gate_extension_path`]). It is embedded here rather than shipped as a
/// data file so that the gate can never be a different version from the
/// adapter that depends on it, and so eave has no install story beyond its
/// own binary.
///
/// Every path out of the `tool_call` handler that is not an explicit
/// operator `allow` returns `{ block: true }`. That includes an
/// unparseable answer, a cancelled dialog, and a thrown exception — Pi
/// additionally treats a handler that throws as blocking, so the gate is
/// fail-closed twice over.
///
/// It also registers [`GATE_MARKER_COMMAND`], which is the only reason
/// eave can prove over the wire that this file loaded at all.
pub const GATE_SOURCE: &str = r#"// Generated by eave (crates/eave-core/src/pi.rs). Do not edit.
//
// eave's approval gate for Pi. Pi has no built-in permission system, so
// this hook IS the permission system: every tool call is held here until
// eave answers, and anything that is not an explicit "allow" blocks.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const DIALOG_TITLE = "eave-approval-v1";

export default function (pi: ExtensionAPI) {
	// Liveness marker: eave proves this file loaded by finding this
	// command in `get_commands`. It intentionally does nothing.
	pi.registerCommand("eave-gate-installed", {
		description: "eave approval gate liveness marker",
		handler: async () => {},
	});

	pi.on("tool_call", async (event, ctx) => {
		const payload = JSON.stringify({
			v: 1,
			toolCallId: event.toolCallId,
			tool: event.toolName,
			input: event.input,
		});
		let answer: string | undefined;
		try {
			// No timeout: eave never auto-approves and never auto-denies,
			// so this waits for a human for as long as it takes.
			answer = await ctx.ui.input(DIALOG_TITLE, payload);
		} catch (err) {
			return { block: true, reason: "eave: approval gate failed: " + String(err) };
		}
		if (answer === undefined) {
			return { block: true, reason: "eave: operator did not approve this tool call" };
		}
		let decision: { decision?: string; message?: string };
		try {
			decision = JSON.parse(answer);
		} catch {
			return { block: true, reason: "eave: unparseable approval decision" };
		}
		if (decision && decision.decision === "allow") return;
		const msg =
			decision && typeof decision.message === "string" && decision.message
				? decision.message
				: "eave: denied by operator";
		return { block: true, reason: msg };
	});
}
"#;

/// Well-known `--model` values for Pi, for the launcher's model picker.
///
/// **Pi needs no separate provider axis in the UI.** `--model` accepts
/// `provider/id` and carries the provider with it (`pi --help`: "Model
/// pattern or ID (supports \"provider/id\"...)"), so eave still passes
/// exactly one string and `--provider` is never sent. That is why
/// `SpawnConfig::model` did not have to grow a second field.
///
/// Every id below is copied from Pi 0.84.2's own bundled catalog at
/// `@earendil-works/pi-ai/dist/providers/data/<provider>.json`, not from
/// memory — the Anthropic entries are the three the catalog itself labels
/// "(latest)". The list is a convenience, not a validation: eave passes
/// `--model` verbatim and an unknown model is Pi's error to state.
///
/// It deliberately does not start with the launcher's `MODEL_DEFAULT`
/// sentinel, which is an app-level concept; the launcher prepends it.
pub const MODELS: &[&str] = &[
    "anthropic/claude-opus-4-5",
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-haiku-4-5",
    "openai/gpt-5.3-codex",
    "google/gemini-3.1-pro-preview",
    "xai/grok-4.6",
];

type ExitFacts = (Option<i32>, Option<i32>);
type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// Where the opening handshake has got to. `send_user_message` waits on
/// this rather than racing it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Posture {
    Pending,
    Verified,
    Refused(String),
}

struct Shared {
    stream: Mutex<PiStream>,
    last_activity: Mutex<Instant>,
    /// See [`crate::claude`]: protocol input is dropped from kill
    /// initiation, because descendants holding inherited pipes may emit
    /// stale lines after the leader dies.
    kill_initiated: AtomicBool,
}

pub struct PiSession {
    identity: ProcIdentity,
    pgid: i32,
    /// Shared for the same reason Claude's is: the engine answering an
    /// approval and the reader task refusing a foreign dialog are two
    /// writers, and a control frame must never interleave with a prompt
    /// mid-line.
    stdin: SharedStdin,
    shared: Arc<Shared>,
    posture_rx: watch::Receiver<Posture>,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
}

impl PiSession {
    /// Spawn one `pi --mode rpc` process in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    ///
    /// The gate extension is written to disk before the spawn: a session
    /// whose gate cannot be materialised must not start at all, rather
    /// than start ungated.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let gate = gate_extension_path().map_err(SessionError::Spawn)?;

        let mut cmd = Command::new(&cfg.program);
        cmd.current_dir(&cfg.cwd)
            .args([
                "--mode", "rpc",
                // The gate. Explicit `-e` paths load even when extension
                // discovery is disabled and even before project trust is
                // resolved, so this survives an operator who has turned
                // discovery off (Pi security docs, "Project Trust").
                "-e",
            ])
            .arg(&gate)
            // `--session-id` is create-if-missing AND resume-if-present
            // (verified 2026-08-20: a second start on the same id reattached
            // to the same file with its four messages intact and no
            // "creating a new session" warning). So unlike Claude there is
            // no separate resume flag to choose between, and `cfg.resume`
            // needs no branch here — the id alone is the resume cursor.
            .args(["--session-id", &cfg.session_id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // Pi's `--model` takes `provider/id` and carries the provider with
        // it, so eave still needs exactly one string here (see
        // `launcher::PROVIDERS`). Absent model = no flag at all, which is
        // not the same as an empty one.
        if let Some(model) = &cfg.model {
            cmd.args(["--model", model]);
        }
        // `extra_args` has already been filtered of Claude's posture flags
        // at the engine boundary; Pi's own must-not-override set is
        // different and is applied here (see `conflicts_with_pi_flags`).
        for arg in cfg
            .extra_args
            .iter()
            .filter(|a| !conflicts_with_pi_flags(a))
        {
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
            // Same reasoning as claude.rs: a child that exits before we can
            // fingerprint it gets a sentinel identity that can never match,
            // and a live-but-uninspectable child stays a hard error.
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
            stream: Mutex::new(PiStream::new(gate.clone())),
            last_activity: Mutex::new(Instant::now()),
            kill_initiated: AtomicBool::new(false),
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (posture_tx, posture_rx) = watch::channel(Posture::Pending);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        // The opening handshake. Sent before the reader starts so the
        // answers cannot be missed; order matters only in that `get_state`
        // is asked first, so the session id is known by the time the gate
        // answer decides whether to emit `Init`.
        let handshake_stdin = Arc::clone(&stdin);
        tokio::spawn(async move {
            let state = serde_json::json!({ "id": PROBE_STATE_ID, "type": "get_state" });
            let gate = serde_json::json!({ "id": PROBE_GATE_ID, "type": "get_commands" });
            write_line(&handshake_stdin, &state.to_string()).await;
            write_line(&handshake_stdin, &gate.to_string()).await;
        });

        let reader_shared = Arc::clone(&shared);
        let reader_tx = events_tx.clone();
        let reader_eof = Arc::clone(&stdout_eof);
        let reader_stdin = Arc::clone(&stdin);
        let reader_posture = posture_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            // Pi's RPC framing is strict JSONL on LF only, and warns that
            // splitting on Unicode separators is non-compliant because
            // U+2028/U+2029 are legal inside JSON strings. `AsyncBufReadExt::lines`
            // splits on LF (handling a trailing CR) and on nothing else,
            // which is exactly the required behavior.
            while let Ok(Some(line)) = lines.next_line().await {
                *reader_shared.last_activity.lock().unwrap() = Instant::now();
                if reader_shared.kill_initiated.load(Ordering::SeqCst) {
                    continue;
                }
                let events = reader_shared.stream.lock().unwrap().parse_line(&line);
                for event in events {
                    match &event {
                        // The handshake succeeded: Pi named its session and
                        // the gate proved it loaded.
                        SessionEvent::Init { .. } => {
                            let _ = reader_posture.send(Posture::Verified);
                        }
                        // The handshake failed. The session is refused and
                        // `send_user_message` will never write a prompt.
                        SessionEvent::PostureRefused { reason, .. } => {
                            let _ = reader_posture.send(Posture::Refused(reason.clone()));
                        }
                        // A dialog raised by some extension other than the
                        // gate. eave cannot present it, so it is dismissed
                        // immediately with a stated refusal rather than
                        // left to park the turn on a question nobody can
                        // see (claude.rs amendment 2, same reasoning).
                        SessionEvent::DialogRefused { request_id, .. } => {
                            let frame = serde_json::json!({
                                "type": "extension_ui_response",
                                "id": request_id,
                                "cancelled": true,
                            });
                            write_line(&reader_stdin, &frame.to_string()).await;
                        }
                        _ => {}
                    }
                    let _ = reader_tx.send(event).await;
                }
            }
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
            // A process that died before answering the handshake leaves
            // `send_user_message` waiting on a watch channel that will
            // never change. Close it with the reason, so the caller gets
            // the refusal instead of the deadline.
            if *posture_tx.borrow() == Posture::Pending {
                let _ = posture_tx.send(Posture::Refused(format!(
                    "the Pi CLI exited before it could prove eave's approval gate was \
                     installed, so no tool call it might have made could have been \
                     reviewed; stderr: {}",
                    stderr_tail.trim()
                )));
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
                posture_rx,
                exit_rx,
                stdout_eof,
                exited_flag,
            },
            events_rx,
        ))
    }

    /// Block until the opening handshake resolves, then report whether the
    /// session may be spoken to.
    ///
    /// This is the enforcement point for the whole gate story: a caller
    /// cannot send a prompt to a Pi whose approval gate has not been
    /// proven, because there is no other way in.
    async fn await_posture(&self) -> Result<(), SessionError> {
        let mut rx = self.posture_rx.clone();
        let settled = tokio::time::timeout(HANDSHAKE_WINDOW, async {
            loop {
                match &*rx.borrow_and_update() {
                    Posture::Verified => return Ok(()),
                    Posture::Refused(reason) => return Err(reason.clone()),
                    Posture::Pending => {}
                }
                if rx.changed().await.is_err() {
                    return Err("the Pi session ended before its approval gate was proven".into());
                }
            }
        })
        .await;
        match settled {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(refusal(reason)),
            Err(_) => Err(refusal(format!(
                "the Pi CLI did not answer eave's posture handshake within {:?}; eave will not \
                 send work to an agent it cannot prove will ask before it writes",
                HANDSHAKE_WINDOW
            ))),
        }
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

    fn exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }
}

#[async_trait]
impl crate::provider::AgentAdapter for PiSession {
    const PROVIDER_ID: &'static str = "pi";

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "pi.rs: Pi has NO permission system of its own — Slice AX measured a bare \
                   `pi --mode rpc` executing a shell write with zero approval traffic. eave \
                   INSTALLS one: a TypeScript gate on Pi's `tool_call` hook, written \
                   content-addressed to temp and loaded with -e, and the session refuses to send \
                   any prompt unless `get_commands` proves the gate loaded FROM THE EXACT PATH \
                   eave wrote (name alone would let another extension spoof it). \
                   `await_posture` is fail-closed on all four paths: verified, refused, timeout, \
                   and channel-closed. KNOWN GAP: Pi's RPC has an ungated shell escape — \
                   `{\"type\":\"bash\"}` dispatches as `user_bash`, not `tool_call`, so the gate \
                   cannot see it. eave never sends it.",
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        PiSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for PiSession {
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

    /// Send one user message into the running session.
    ///
    /// `streamingBehavior: "steer"` is always set. Pi rejects a `prompt`
    /// sent while the agent is streaming unless the behavior is stated,
    /// and accepts the field harmlessly when idle (both verified
    /// 2026-08-20), so one code path covers typing between turns and
    /// typing during one. "steer" rather than "followUp" because it
    /// delivers after the current tool calls finish and before the next
    /// model call, which is the closest thing Pi has to Claude's queued
    /// user message.
    async fn send_user_message(
        &mut self,
        text: &str,
        _images: &[ImageRef],
    ) -> Result<(), SessionError> {
        // Pi's prompt is a string (`{type:prompt, message}`). There is no
        // content array and T3 has no Pi image encoder. Images still
        // arrive here so this adapter cannot silently drop them — they
        // live in `text` as `@mentions`, which is the path the agent
        // `Read`s.
        self.await_posture().await?;
        let line = serde_json::json!({
            "type": "prompt",
            "message": text,
            "streamingBehavior": "steer",
        });
        write_frame(&self.stdin, &line.to_string()).await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.stream.lock().unwrap().pending_approvals()
    }

    /// Answer one permission question.
    ///
    /// The frame is the `extension_ui_response` the gate's `ctx.ui.input`
    /// call is blocked on, and the answer is a JSON decision in the
    /// dialog's free-text `value` field. `value` is free text on both
    /// sides of the wire, which is why the gate uses `input` rather than
    /// `confirm` — `confirm` returns a bare boolean and could not carry
    /// the operator's deny message.
    ///
    /// On `deny` that message reaches the model verbatim as the tool
    /// result, exactly as Claude's does: verified 2026-08-20, a deny
    /// carrying "eave: denied - commit first" produced
    /// `tool_execution_end { isError: true, content: "eave: denied - commit first" }`
    /// and the command did not run.
    ///
    /// `Ok(false)` means the question was not open — a race, not an error,
    /// and it must never reach the pipe.
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
        let value = match decision {
            ApprovalDecision::Allow => serde_json::json!({ "decision": "allow" }),
            ApprovalDecision::Deny { message } => serde_json::json!({
                "decision": "deny",
                "message": message,
            }),
        };
        let frame = serde_json::json!({
            "type": "extension_ui_response",
            "id": request_id,
            // The gate parses this string; sending it as a string rather
            // than an object is what `ctx.ui.input` hands back.
            "value": value.to_string(),
        });
        write_frame(&self.stdin, &frame.to_string()).await?;
        // Only now is the question closed: a failed write leaves it open
        // so the operator can try again rather than facing a dead card.
        self.shared
            .stream
            .lock()
            .unwrap()
            .resolve_approval(request_id);
        Ok(true)
    }

    /// Identical to Claude's, and deliberately so — this is spec 01 §Kill,
    /// which is about process groups and has nothing to do with the wire
    /// format.
    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.shared.kill_initiated.store(true, Ordering::SeqCst);
        let already_exited = self.exited();
        let mut escalated = false;

        if !already_exited {
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
// Spawn plumbing

/// Flags a caller may not smuggle through `AgentConfig::extra_args` for Pi.
///
/// `session::conflicts_with_eave_flags` is Claude's list and is applied at
/// the engine boundary; Pi's overlaps only partly, so the adapter applies
/// its own on top rather than widening a shared constant that three other
/// adapters are landing against this week.
///
/// Two groups, for two different reasons:
///
/// - `--mode`, `--print`/`-p` would change the protocol out from under the
///   parser — a `--print` session is one-shot and would never answer the
///   handshake.
/// - the session-identity flags would move the resume cursor out from
///   under the engine, which is the same reason Claude's list blocks
///   `--session-id` and `--resume`.
///
/// Deliberately **not** blocked: `--no-extensions`/`-ne`, `--tools`,
/// `--exclude-tools`, `--no-tools`. The first keeps explicit `-e` paths
/// (verified 2026-08-20 — the gate loads and works under `-ne`), and the
/// rest can only narrow what the agent may do. An extra `-e` is likewise
/// harmless: a second `tool_call` handler can block a call but has no way
/// to un-block one the gate refused.
pub fn conflicts_with_pi_flags(arg: &str) -> bool {
    let head = arg.split('=').next().unwrap_or(arg);
    matches!(
        head,
        "--mode"
            | "--print"
            | "-p"
            | "--session-id"
            | "--session"
            | "--session-dir"
            | "--continue"
            | "-c"
            | "--resume"
            | "-r"
            | "--fork"
            | "--no-session"
    )
}

/// Materialise [`GATE_SOURCE`] on disk and return the path to pass to `-e`.
///
/// Content-addressed: the filename carries a hash of the source, so a
/// change to the gate lands in a new file and concurrent sessions running
/// the same build share one. Written to a temp name and renamed, because
/// two sessions starting at once must never let Pi read a half-written
/// gate — and a truncated gate is a session with no approvals in it.
fn gate_extension_path() -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("eave-pi-gate-{:016x}.ts", fnv1a(GATE_SOURCE)));
    if matches!(std::fs::read_to_string(&path), Ok(on_disk) if on_disk == GATE_SOURCE) {
        return Ok(path);
    }
    let tmp = path.with_extension(format!("ts.{}.partial", std::process::id()));
    std::fs::write(&tmp, GATE_SOURCE)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// FNV-1a, 64-bit. Used only to name a cache file — it is not a security
/// boundary, and the gate's contents are verified by comparison above, not
/// by trusting the hash.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn refusal(reason: impl Into<String>) -> SessionError {
    SessionError::Io(std::io::Error::other(reason.into()))
}

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

/// Best-effort write from a task with no one to report to: a closed pipe
/// means the child is already going away.
async fn write_line(stdin: &SharedStdin, line: &str) {
    let _ = write_frame(stdin, line).await;
}

// ---------------------------------------------------------------------------
// Wire parser

/// Pi RPC JSONL parser.
///
/// Contract observed from `pi 0.84.2` in `--mode rpc` (captures
/// 2026-08-20; `docs/reports/stage-ax-pi-adapter.md`). Owns the
/// outstanding-tool set and the open-approval set, the same two facts
/// `ClaudeStream` owns and for the same callers.
pub struct PiStream {
    outstanding: HashSet<String>,
    pending: HashSet<String>,
    /// Approval request id → the Pi tool-call id it is holding. Kept so a
    /// withdrawn or answered question can be reconciled without re-reading
    /// the wire.
    approval_tools: HashMap<String, String>,
    session_id: Option<String>,
    /// The `-e` path this session was spawned with. The gate proof
    /// requires the marker command to come from *this* file, not merely
    /// from some file with the right command name.
    gate_path: PathBuf,
    gate_checked: bool,
}

impl PiStream {
    pub fn new(gate_path: PathBuf) -> Self {
        Self {
            outstanding: HashSet::new(),
            pending: HashSet::new(),
            approval_tools: HashMap::new(),
            session_id: None,
            gate_path,
            gate_checked: false,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    pub fn pending_approvals(&self) -> usize {
        self.pending.len()
    }

    pub fn approval_is_open(&self, request_id: &str) -> bool {
        self.pending.contains(request_id)
    }

    pub fn resolve_approval(&mut self, request_id: &str) -> bool {
        self.approval_tools.remove(request_id);
        self.pending.remove(request_id)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Parse one line into zero or more events. Never fails: malformed
    /// input becomes a `Malformed` event and parsing continues.
    pub fn parse_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return vec![malformed(line, err.to_string())],
        };
        let Some(ty) = v.get("type").and_then(Value::as_str) else {
            return vec![malformed(line, "line has no `type` field".into())];
        };
        match ty {
            "response" => self.parse_response(&v, line),
            "extension_ui_request" => self.parse_ui_request(&v, line),
            "tool_execution_start" => {
                let Some(id) = v.get("toolCallId").and_then(Value::as_str) else {
                    return vec![malformed(
                        line,
                        "tool_execution_start without toolCallId".into(),
                    )];
                };
                // Once per id: Pi repeats the id on every
                // `tool_execution_update`, and a second Started for one
                // tool would double the in-flight count.
                if !self.outstanding.insert(id.to_string()) {
                    return Vec::new();
                }
                vec![SessionEvent::ToolUseStarted {
                    id: id.to_string(),
                    name: v
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: cap_payload(&compact(v.get("args"))),
                }]
            }
            "tool_execution_end" => {
                let Some(id) = v.get("toolCallId").and_then(Value::as_str) else {
                    return vec![malformed(
                        line,
                        "tool_execution_end without toolCallId".into(),
                    )];
                };
                if !self.outstanding.remove(id) {
                    return Vec::new();
                }
                vec![SessionEvent::ToolUseFinished {
                    id: id.to_string(),
                    result: cap_payload(&flatten_content(v.pointer("/result/content"))),
                    // Pi's own flag, never inferred from the text.
                    is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
                }]
            }
            "message_update" => match v.pointer("/assistantMessageEvent") {
                Some(ev) if ev.get("type").and_then(Value::as_str) == Some("text_delta") => {
                    let text = ev
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![SessionEvent::TextDelta { text }]
                    }
                }
                // Thinking and tool-call deltas are real but not something
                // eave renders yet; recorded verbatim rather than dropped.
                _ => vec![system(ty, line)],
            },
            "message_end" => self.parse_message_end(&v, line),
            // The session-level settle: Pi will not continue on its own
            // past this point (no retry, no compaction retry, no queued
            // follow-up). `agent_end` is the wrong choice — it fires once
            // per low-level run and can be followed by an automatic retry.
            "agent_settled" => vec![SessionEvent::TurnCompleted {
                subtype: "agent_settled".into(),
                is_error: false,
            }],
            // Everything Pi announces that eave does not model yet is kept
            // verbatim, same contract as claude.rs's `System`: the journal
            // records it and a later slice can consume it without a parser
            // change.
            "agent_start"
            | "agent_end"
            | "turn_start"
            | "turn_end"
            | "message_start"
            | "queue_update"
            | "compaction_start"
            | "compaction_end"
            | "auto_retry_start"
            | "auto_retry_end"
            | "tool_execution_update"
            | "bash_execution_update"
            | "extension_error"
            | "summarization_retry_scheduled"
            | "summarization_retry_attempt_start"
            | "summarization_retry_finished" => {
                vec![system(ty, line)]
            }
            other => vec![SessionEvent::Unknown {
                event_type: other.to_string(),
                raw: line.to_string(),
            }],
        }
    }

    fn parse_response(&mut self, v: &Value, line: &str) -> Vec<SessionEvent> {
        match v.get("id").and_then(Value::as_str) {
            Some(PROBE_STATE_ID) => {
                if let Some(id) = v.pointer("/data/sessionId").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                }
                Vec::new()
            }
            Some(PROBE_GATE_ID) => self.verify_gate(v),
            _ => vec![system("response", line)],
        }
    }

    /// The posture proof.
    ///
    /// Pi will happily run without eave's gate — that is the whole finding
    /// this adapter exists around — so a session is refused unless
    /// `get_commands` names [`GATE_MARKER_COMMAND`] as coming from the
    /// exact `-e` file this spawn wrote. Anything else is refused: a failed
    /// command, a missing marker, a marker from a path we did not write, or
    /// a handshake that reached here without a session id.
    ///
    /// There is no "probably fine" branch, for the same reason Slice K's
    /// `ASKING_MODES` has none: guessing wrong here is unreviewed `bash` on
    /// the operator's machine.
    fn verify_gate(&mut self, v: &Value) -> Vec<SessionEvent> {
        // Idempotent: a second answer to the probe must not re-announce a
        // session that is already running.
        if self.gate_checked {
            return Vec::new();
        }
        self.gate_checked = true;

        if v.get("success").and_then(Value::as_bool) != Some(true) {
            return vec![gate_refusal(format!(
                "the Pi CLI refused eave's `get_commands` probe ({}), so eave cannot prove its \
                 approval gate loaded",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            ))];
        }
        let installed = v
            .pointer("/data/commands")
            .and_then(Value::as_array)
            .map(|cmds| {
                cmds.iter().any(|c| {
                    c.get("name").and_then(Value::as_str) == Some(GATE_MARKER_COMMAND)
                        && c.pointer("/sourceInfo/path").and_then(Value::as_str)
                            == self.gate_path.to_str()
                })
            })
            .unwrap_or(false);
        if !installed {
            return vec![gate_refusal(
                "the Pi CLI did not load eave's approval gate extension. Pi has no permission \
                 system of its own, so without the gate nothing would ask before it ran `bash`, \
                 wrote a file or edited one; eave refuses to supervise a session it cannot \
                 review",
            )];
        }
        let Some(session_id) = self.session_id.clone() else {
            return vec![gate_refusal(
                "the Pi CLI proved eave's approval gate but never stated its session id, so the \
                 session has no resume cursor and could not be reopened",
            )];
        };
        vec![SessionEvent::Init {
            session_id,
            permission_mode: Some(GATE_POSTURE.into()),
            // Empty is the honest answer, not a placeholder: this
            // provider has announced no command inventory to eave.
            // Pi's `get_commands` is read for the gate proof, not for the palette.
            slash_commands: Vec::new(),
        }]
    }

    /// A dialog from an extension.
    ///
    /// Exactly one shape is an approval: an `input` titled
    /// [`GATE_DIALOG_TITLE`], which only [`GATE_SOURCE`] raises. Every
    /// other *dialog* method blocks a turn on a question eave has no way
    /// to show, so it is refused; the fire-and-forget methods expect no
    /// answer and are simply recorded.
    fn parse_ui_request(&mut self, v: &Value, line: &str) -> Vec<SessionEvent> {
        let Some(request_id) = v.get("id").and_then(Value::as_str) else {
            return vec![malformed(line, "extension_ui_request without id".into())];
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
        let is_gate =
            method == "input" && v.get("title").and_then(Value::as_str) == Some(GATE_DIALOG_TITLE);

        if is_gate {
            // The gate puts its payload in `placeholder`, which is free
            // text on the wire.
            let raw = v
                .get("placeholder")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            let tool = payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let input = payload.get("input");
            if let Some(tool_call_id) = payload.get("toolCallId").and_then(Value::as_str) {
                self.approval_tools
                    .insert(request_id.to_string(), tool_call_id.to_string());
            }
            self.pending.insert(request_id.to_string());
            return vec![SessionEvent::ApprovalRequested {
                request_id: request_id.to_string(),
                display_name: tool.clone(),
                tool,
                // Pi states no human-facing summary for a tool call; the
                // input below is the fact of record. Inventing a sentence
                // here would be eave putting words in the provider's
                // mouth.
                description: String::new(),
                path: input
                    .and_then(|i| i.get("path"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_json: cap_payload(&compact(input)),
                // Pi offers no "stop asking me" affordances, so there is
                // nothing to record. `None` is the honest answer, not an
                // empty object.
                suggestions_json: None,
            }];
        }

        match method {
            "select" | "confirm" | "input" | "editor" => vec![SessionEvent::DialogRefused {
                request_id: request_id.to_string(),
                dialog: method.to_string(),
            }],
            _ => vec![system("extension_ui_request", line)],
        }
    }

    fn parse_message_end(&mut self, v: &Value, line: &str) -> Vec<SessionEvent> {
        let msg = v.get("message");
        let role = msg
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = flatten_content(msg.and_then(|m| m.get("content")));
        match role {
            "assistant" if !text.is_empty() => vec![SessionEvent::AssistantText { text }],
            // Pi echoes the user's own message back as a `user` message,
            // which is what `--replay-user-messages` gets from Claude.
            "user" if !text.is_empty() => vec![SessionEvent::UserEcho { text }],
            // Tool results arrive here too, but `tool_execution_end`
            // already reported them; emitting both would double every tool
            // in the transcript.
            _ => vec![system("message_end", line)],
        }
    }
}

fn gate_refusal(reason: impl Into<String>) -> SessionEvent {
    SessionEvent::PostureRefused {
        mode: "ungated".into(),
        reason: reason.into(),
    }
}

fn system(subtype: &str, line: &str) -> SessionEvent {
    SessionEvent::System {
        subtype: subtype.to_string(),
        raw: line.to_string(),
    }
}

fn malformed(line: &str, error: String) -> SessionEvent {
    let mut shown = line.to_string();
    if shown.len() > MALFORMED_LINE_CAP {
        shown.truncate(MALFORMED_LINE_CAP);
    }
    SessionEvent::Malformed { line: shown, error }
}

/// One JSON value, re-serialised compactly. Absent becomes `{}` rather
/// than `null`, so a journal row reads as "no arguments" instead of "the
/// argument was null".
fn compact(v: Option<&Value>) -> String {
    match v {
        Some(Value::Null) | None => "{}".to_string(),
        Some(v) => v.to_string(),
    }
}

/// Flatten Pi's `content` array to text.
///
/// Pi content blocks are `{"type":"text","text":...}`; anything else
/// (images, tool calls) has no text to contribute and is skipped rather
/// than rendered as JSON noise.
fn flatten_content(v: Option<&Value>) -> String {
    let Some(arr) = v.and_then(Value::as_array) else {
        return v.and_then(Value::as_str).unwrap_or_default().to_string();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    parts.join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> PiStream {
        PiStream::new(PathBuf::from("/tmp/eave-pi-gate-test.ts"))
    }

    /// A `get_commands` answer that names the gate, sourced from the path
    /// this stream was built with. Shaped from a real capture
    /// (2026-08-20).
    fn gate_ok(path: &str) -> String {
        serde_json::json!({
            "id": PROBE_GATE_ID,
            "type": "response",
            "command": "get_commands",
            "success": true,
            "data": { "commands": [
                { "name": "eave-gate-installed",
                  "description": "eave approval gate liveness marker",
                  "source": "extension",
                  "sourceInfo": { "path": path, "source": "cli", "scope": "temporary" } },
                { "name": "llama", "description": "Manage llama.cpp router models",
                  "source": "extension",
                  "sourceInfo": { "path": "<inline:llama.cpp>", "source": "inline" } },
            ]},
        })
        .to_string()
    }

    fn state_ok(id: &str) -> String {
        serde_json::json!({
            "id": PROBE_STATE_ID, "type": "response", "command": "get_state", "success": true,
            "data": { "sessionId": id, "messageCount": 0, "isStreaming": false },
        })
        .to_string()
    }

    /// The happy handshake: Pi names its session, the gate proves it
    /// loaded, and only then does the adapter announce the session as
    /// running under eave's posture.
    #[test]
    fn a_verified_gate_announces_the_session() {
        let mut s = stream();
        assert!(
            s.parse_line(&state_ok("eave-abc-123")).is_empty(),
            "the state probe answers no user-visible event on its own"
        );
        assert_eq!(s.session_id(), Some("eave-abc-123"));
        let events = s.parse_line(&gate_ok("/tmp/eave-pi-gate-test.ts"));
        assert_eq!(
            events,
            vec![SessionEvent::Init {
                session_id: "eave-abc-123".into(),
                permission_mode: Some("eave-gate".into()),
                // Empty is the honest answer, not a placeholder: this
                // provider has announced no command inventory to eave.
                // Pi's `get_commands` is read for the gate proof, not for the palette.
                slash_commands: Vec::new(),
            }]
        );
    }

    /// **The blocking case.** Pi runs `bash` without asking anyone, so a
    /// Pi whose gate did not load must be refused rather than driven.
    #[test]
    fn a_session_without_the_gate_is_refused() {
        let mut s = stream();
        s.parse_line(&state_ok("eave-abc-123"));
        let no_gate = serde_json::json!({
            "id": PROBE_GATE_ID, "type": "response", "command": "get_commands",
            "success": true,
            "data": { "commands": [ { "name": "llama", "source": "extension",
                      "sourceInfo": { "path": "<inline:llama.cpp>" } } ] },
        })
        .to_string();
        match s.parse_line(&no_gate).as_slice() {
            [SessionEvent::PostureRefused { reason, .. }] => {
                assert!(
                    reason.contains("approval gate"),
                    "the refusal must say what is missing: {reason}"
                );
            }
            other => panic!("an ungated Pi must be refused, got {other:?}"),
        }
    }

    /// A marker command with the right *name* but the wrong *file* proves
    /// nothing: some other extension could register it, and then eave
    /// would believe in a gate that is not there.
    #[test]
    fn a_marker_from_a_file_eave_did_not_write_is_refused() {
        let mut s = stream();
        s.parse_line(&state_ok("eave-abc-123"));
        let events = s.parse_line(&gate_ok("/somewhere/else/impostor.ts"));
        assert!(
            matches!(events.as_slice(), [SessionEvent::PostureRefused { .. }]),
            "the gate must be proven by path, not by name alone: {events:?}"
        );
    }

    /// A gate that loaded but a session that was never named leaves no
    /// resume cursor, so it is refused too — fail-closed on both halves of
    /// the handshake, not just the interesting one.
    #[test]
    fn a_gate_without_a_session_id_is_refused() {
        let mut s = stream();
        let events = s.parse_line(&gate_ok("/tmp/eave-pi-gate-test.ts"));
        assert!(
            matches!(events.as_slice(), [SessionEvent::PostureRefused { .. }]),
            "no session id means no resume cursor: {events:?}"
        );
    }

    /// The real approval frame, byte-for-byte from the live capture
    /// (2026-08-20, "deny" run). This is the shape the gate produces and
    /// the one an operator's card is built from.
    #[test]
    fn a_gate_dialog_becomes_an_approval_request() {
        let mut s = stream();
        let line = r#"{"type":"extension_ui_request","id":"4cc0c4ed-27ce-472c-a774-19f50a4cd3e6","method":"input","title":"eave-approval-v1","placeholder":"{\"v\":1,\"toolCallId\":\"call_stub_1\",\"tool\":\"bash\",\"input\":{\"command\":\"echo EAVE_GATE_BREACH > /tmp/eave_breach.txt\"}}"}"#;
        let events = s.parse_line(line);
        match events.as_slice() {
            [SessionEvent::ApprovalRequested {
                request_id,
                tool,
                input_json,
                path,
                suggestions_json,
                ..
            }] => {
                assert_eq!(request_id, "4cc0c4ed-27ce-472c-a774-19f50a4cd3e6");
                assert_eq!(tool, "bash");
                assert!(
                    input_json.contains("EAVE_GATE_BREACH"),
                    "the operator must see what they are approving: {input_json}"
                );
                assert_eq!(*path, None, "a bash call names no path");
                assert_eq!(*suggestions_json, None);
            }
            other => panic!("expected one approval, got {other:?}"),
        }
        assert_eq!(s.pending_approvals(), 1);
        assert!(s.approval_is_open("4cc0c4ed-27ce-472c-a774-19f50a4cd3e6"));
        assert!(s.resolve_approval("4cc0c4ed-27ce-472c-a774-19f50a4cd3e6"));
        assert_eq!(s.pending_approvals(), 0);
        assert!(
            !s.approval_is_open("4cc0c4ed-27ce-472c-a774-19f50a4cd3e6"),
            "an answered question must not accept a second decision"
        );
    }

    /// A file-touching tool names its path, because that is the fact an
    /// operator most needs before saying yes.
    #[test]
    fn a_write_approval_states_its_path() {
        let mut s = stream();
        let payload =
            serde_json::json!({ "v": 1, "toolCallId": "call_2", "tool": "write",
                                "input": { "path": "/repo/src/main.rs", "content": "fn main() {}" } })
            .to_string();
        let line = serde_json::json!({
            "type": "extension_ui_request", "id": "req-2", "method": "input",
            "title": GATE_DIALOG_TITLE, "placeholder": payload,
        })
        .to_string();
        match s.parse_line(&line).as_slice() {
            [SessionEvent::ApprovalRequested { path, tool, .. }] => {
                assert_eq!(tool, "write");
                assert_eq!(path.as_deref(), Some("/repo/src/main.rs"));
            }
            other => panic!("expected an approval, got {other:?}"),
        }
    }

    /// A dialog from somebody else's extension is refused, not shown as an
    /// approval — eave must never render an arbitrary extension's question
    /// as "may this tool run?".
    #[test]
    fn a_foreign_dialog_is_refused_rather_than_shown_as_an_approval() {
        let mut s = stream();
        let line = serde_json::json!({
            "type": "extension_ui_request", "id": "req-9", "method": "confirm",
            "title": "Clear session?", "message": "All messages will be lost.",
        })
        .to_string();
        assert_eq!(
            s.parse_line(&line),
            vec![SessionEvent::DialogRefused {
                request_id: "req-9".into(),
                dialog: "confirm".into(),
            }]
        );
        assert_eq!(
            s.pending_approvals(),
            0,
            "a refused dialog is not an open approval"
        );
    }

    /// A fire-and-forget notification expects no answer, so it must not be
    /// refused — replying to one would put an unsolicited frame on the
    /// wire.
    #[test]
    fn a_notification_is_recorded_not_answered() {
        let mut s = stream();
        let line = r#"{"type":"extension_ui_request","id":"n-1","method":"notify","message":"hi","notifyType":"info"}"#;
        assert!(matches!(
            s.parse_line(line).as_slice(),
            [SessionEvent::System { .. }]
        ));
    }

    /// Start and matching resolve, from the live capture. `in_flight`
    /// counts tools and only tools.
    #[test]
    fn a_tool_starts_and_resolves_exactly_once() {
        let mut s = stream();
        let start = r#"{"type":"tool_execution_start","toolCallId":"call_stub_1","toolName":"bash","args":{"command":"echo hi"}}"#;
        match s.parse_line(start).as_slice() {
            [SessionEvent::ToolUseStarted { id, name, input }] => {
                assert_eq!(id, "call_stub_1");
                assert_eq!(name, "bash");
                assert!(input.contains("echo hi"));
            }
            other => panic!("expected a tool start, got {other:?}"),
        }
        assert_eq!(s.in_flight(), 1);

        // Pi repeats the id on progress updates; only the first start counts.
        let update = r#"{"type":"tool_execution_update","toolCallId":"call_stub_1","toolName":"bash","args":{},"partialResult":{"content":[]}}"#;
        s.parse_line(update);
        assert_eq!(s.in_flight(), 1, "a progress update is not a second tool");

        let end = r#"{"type":"tool_execution_end","toolCallId":"call_stub_1","toolName":"bash","result":{"content":[{"type":"text","text":"hi\n"}]},"isError":false}"#;
        match s.parse_line(end).as_slice() {
            [SessionEvent::ToolUseFinished {
                id,
                result,
                is_error,
            }] => {
                assert_eq!(id, "call_stub_1");
                assert_eq!(result, "hi\n");
                assert!(!is_error);
            }
            other => panic!("expected a tool finish, got {other:?}"),
        }
        assert_eq!(s.in_flight(), 0);
        assert!(
            s.parse_line(end).is_empty(),
            "a repeated end must not drive the count negative or re-report"
        );
    }

    /// A denied tool comes back with Pi's own error flag set and the
    /// operator's words as the result — the capture that proves a deny
    /// message reaches the model.
    #[test]
    fn a_denied_tool_reports_the_operators_words() {
        let mut s = stream();
        s.parse_line(
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{}}"#,
        );
        let end = r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"eave: denied - commit first"}],"details":{}},"isError":true}"#;
        match s.parse_line(end).as_slice() {
            [SessionEvent::ToolUseFinished {
                result, is_error, ..
            }] => {
                assert_eq!(result, "eave: denied - commit first");
                assert!(is_error, "Pi's own flag, not an inference from the text");
            }
            other => panic!("expected a tool finish, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_streams_then_lands() {
        let mut s = stream();
        let delta = r#"{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"done"}}"#;
        assert_eq!(
            s.parse_line(delta),
            vec![SessionEvent::TextDelta {
                text: "done".into()
            }]
        );
        let end = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        assert_eq!(
            s.parse_line(end),
            vec![SessionEvent::AssistantText {
                text: "done".into()
            }]
        );
    }

    /// A tool-call message_end must not also become assistant text: the
    /// tool was already reported by `tool_execution_end`.
    #[test]
    fn a_tool_call_message_is_not_reported_as_text() {
        let mut s = stream();
        let end = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}]}}"#;
        assert!(
            matches!(s.parse_line(end).as_slice(), [SessionEvent::System { .. }]),
            "a tool-call message carries no assistant text"
        );
    }

    #[test]
    fn the_users_own_message_comes_back_as_an_echo() {
        let mut s = stream();
        let line = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"run the bash tool"}]}}"#;
        assert_eq!(
            s.parse_line(line),
            vec![SessionEvent::UserEcho {
                text: "run the bash tool".into()
            }]
        );
    }

    /// `agent_settled` is the terminal turn event; `agent_end` is not,
    /// because Pi can retry after one.
    #[test]
    fn the_turn_completes_on_settled_not_on_agent_end() {
        let mut s = stream();
        assert!(
            matches!(
                s.parse_line(r#"{"type":"agent_end","messages":[],"willRetry":true}"#)
                    .as_slice(),
                [SessionEvent::System { .. }]
            ),
            "agent_end can be followed by a retry, so it does not complete the turn"
        );
        assert_eq!(
            s.parse_line(r#"{"type":"agent_settled"}"#),
            vec![SessionEvent::TurnCompleted {
                subtype: "agent_settled".into(),
                is_error: false,
            }]
        );
    }

    /// A misbehaving CLI must not take the session state machine down.
    #[test]
    fn malformed_input_is_surfaced_and_survivable() {
        let mut s = stream();
        assert!(matches!(
            s.parse_line("not json at all").as_slice(),
            [SessionEvent::Malformed { .. }]
        ));
        assert!(matches!(
            s.parse_line(r#"{"no":"type"}"#).as_slice(),
            [SessionEvent::Malformed { .. }]
        ));
        assert!(s.parse_line("   ").is_empty());
        // Still working afterwards.
        assert_eq!(
            s.parse_line(r#"{"type":"agent_settled"}"#).len(),
            1,
            "the parser must keep going after garbage"
        );
    }

    #[test]
    fn an_unmodelled_event_is_kept_verbatim() {
        let mut s = stream();
        let line = r#"{"type":"some_future_event","detail":42}"#;
        assert_eq!(
            s.parse_line(line),
            vec![SessionEvent::Unknown {
                event_type: "some_future_event".into(),
                raw: line.to_string(),
            }]
        );
    }

    /// Every offered model names a provider, because Pi's `--model`
    /// carries the provider and a bare id would silently fall back to
    /// Pi's default provider (google) instead of the one the operator
    /// picked.
    #[test]
    fn every_offered_model_names_its_provider() {
        assert!(!MODELS.is_empty());
        for m in MODELS {
            let (provider, id) = m.split_once('/').unwrap_or_else(|| {
                panic!("{m:?} must be a `provider/id` pair, not a bare model id")
            });
            assert!(!provider.is_empty() && !id.is_empty(), "{m:?} is malformed");
            assert!(
                !id.contains('/'),
                "{m:?} has more than one separator; Pi reads the first as the provider"
            );
        }
        let mut sorted = MODELS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), MODELS.len(), "a duplicate model choice");
    }

    /// The flags that would take the session out from under eave, and the
    /// ones that must stay allowed.
    #[test]
    fn pi_flags_that_would_undo_the_spawn_are_refused() {
        for bad in [
            "--mode",
            "--mode=rpc",
            "-p",
            "--print",
            "--session-id",
            "--session",
            "--session-dir",
            "--continue",
            "-c",
            "--resume",
            "-r",
            "--fork",
            "--no-session",
        ] {
            assert!(conflicts_with_pi_flags(bad), "{bad} must be refused");
        }
        // Narrowing what the agent may do is always allowed, and `-ne`
        // still honors the explicit `-e` that carries the gate.
        for ok in [
            "--no-extensions",
            "-ne",
            "--tools",
            "--exclude-tools",
            "--no-tools",
            "-e",
            "--thinking",
            "--append-system-prompt",
        ] {
            assert!(!conflicts_with_pi_flags(ok), "{ok} must be allowed");
        }
    }

    /// The gate lands on disk, is content-addressed, and a second spawn of
    /// the same build reuses the same file rather than racing it.
    #[test]
    fn the_gate_is_written_once_and_reused() {
        let a = gate_extension_path().expect("gate must materialise");
        let b = gate_extension_path().expect("gate must materialise");
        assert_eq!(a, b, "one build, one gate file");
        let on_disk = std::fs::read_to_string(&a).expect("gate is readable");
        assert_eq!(on_disk, GATE_SOURCE);
        assert!(
            a.to_string_lossy().contains("eave-pi-gate-"),
            "the gate is recognisable on disk: {a:?}"
        );
        assert!(
            !a.with_extension(format!("ts.{}.partial", std::process::id()))
                .exists(),
            "the partial write must not be left behind"
        );
    }

    /// The gate the adapter ships must actually be the thing the adapter
    /// looks for. These three strings are one contract split across a
    /// TypeScript file and a Rust parser, and nothing else checks that
    /// they agree.
    #[test]
    fn the_gate_source_and_the_parser_agree_on_the_contract() {
        assert!(
            GATE_SOURCE.contains(&format!("\"{GATE_MARKER_COMMAND}\"")),
            "the gate must register the command the posture proof looks for"
        );
        assert!(
            GATE_SOURCE.contains(&format!("\"{GATE_DIALOG_TITLE}\"")),
            "the gate must raise its dialog under the title the parser matches"
        );
        assert!(
            GATE_SOURCE.contains("tool_call"),
            "the gate must hook the event that fires before a tool runs"
        );
    }

    // -----------------------------------------------------------------
    // Live tests. These drive the real `pi` binary and are `#[ignore]`d
    // because CI has no Pi installed. They need no credentials and no
    // network — the handshake is answered before any model is contacted.
    //
    //     cargo test -p eave-core --lib pi::tests::live -- --ignored --nocapture

    /// End-to-end proof of the posture: a real `pi --mode rpc`, spawned
    /// exactly the way the engine spawns it, loads eave's gate and says so.
    ///
    /// If this fails after a Pi upgrade, the extension API or the RPC
    /// handshake moved, and **the correct response is to fix the gate, not
    /// to relax the check** — a Pi that cannot prove its gate is a Pi that
    /// runs shell commands without asking.
    #[tokio::test]
    #[ignore = "requires the real `pi` CLI on PATH"]
    async fn live_a_real_pi_proves_the_gate_and_names_its_session() {
        let cwd = std::env::temp_dir().join(format!("eave-pi-live-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("scratch cwd");
        let mut cfg = SpawnConfig::new("pi", &cwd, "eave-live-probe-1".to_string());
        // Keep the operator's real session store out of it entirely.
        cfg.extra_args = vec!["--no-session".into()];

        let (mut session, mut events) = PiSession::spawn(&cfg).expect("pi must spawn");
        let init = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(ev) = events.recv().await {
                match ev {
                    SessionEvent::Init { .. } | SessionEvent::PostureRefused { .. } => {
                        return Some(ev)
                    }
                    SessionEvent::Exited { stderr_tail, .. } => {
                        panic!("pi exited during the handshake: {stderr_tail}")
                    }
                    _ => {}
                }
            }
            None
        })
        .await
        .expect("the handshake must not hang");

        match init {
            Some(SessionEvent::Init {
                permission_mode, ..
            }) => {
                assert_eq!(permission_mode.as_deref(), Some(GATE_POSTURE));
            }
            other => panic!("the gate must load in a real Pi, got {other:?}"),
        }
        assert!(session.kill().await.is_ok(), "the session must die cleanly");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The negative control, and the reason the gate exists at all: the
    /// same Pi, spawned **without** the `-e` gate, never asks anything.
    ///
    /// This test does not assert on eave code — it asserts on Pi's
    /// behavior, so that the day Pi grows a permission system of its own,
    /// this fails and somebody re-reads the design.
    #[tokio::test]
    #[ignore = "requires the real `pi` CLI on PATH"]
    async fn live_pi_without_the_gate_registers_no_approval_surface() {
        let mut child = tokio::process::Command::new("pi")
            .args(["--mode", "rpc", "--no-session", "-ne"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("pi must spawn");
        let mut stdin = child.stdin.take().expect("stdin piped");
        tokio::spawn(async move {
            let _ = stdin
                .write_all(b"{\"id\":\"g\",\"type\":\"get_commands\"}\n")
                .await;
        });
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .expect("pi must answer")
            .expect("output");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains(GATE_MARKER_COMMAND),
            "a bare Pi must not carry eave's gate — if it does, this test is lying to us"
        );
    }
}
