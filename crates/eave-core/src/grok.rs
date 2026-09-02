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
//!   trip).
//! - **eave now forces it back on** (Slice TN2, 2026-09-02): the
//!   top-level `grok --permission-mode default` — documented for TUI and
//!   headless, and measured to reach `agent stdio`: with the operator's
//!   `always-approve` config in place, a bare spawn's
//!   `_x.ai/sessions/changed` said `yolo: true` and the same spawn with
//!   the flag said `yolo: false`, and the prompt still ran. The flag
//!   goes BEFORE `agent` (it is `grok`'s, not `agent`'s), and the engine
//!   filters any `--permission-mode` from a session's own `extra_args`,
//!   so ambient config cannot undo it either way.
//! - And it still **verifies**: the session's own `_x.ai/sessions/changed`
//!   record carries `yolo: true|false` before the first `tool_call`, and
//!   `yolo: true` is refused ([`posture_refusal`]) — a Grok that ignored
//!   the flag would be caught, not trusted.
//!
//! Fail-closed by construction, exactly like Slice K: a session whose
//! posture is never stated at all is refused at the first tool call rather
//! than assumed benign ([`GrokProto::note_tool_call`]). `--always-approve`
//! is never passed, and `--no-leader` is always passed — see [`spawn`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::acp::permission::{self, PendingApprovals};
use crate::acp::update::{AcpStream, Thoughts, ToolInput, ToolNaming, UpdateSpec};
use crate::acp::wire::{self, Frame, SharedStdin};
use crate::acp::{
    self, AcpChild, AcpSpec, ChildEnv, ChildSpec, HandshakeError, HandshakeGate, ModeVerdict,
};
use crate::events::SessionEvent;
use crate::proc::ProcIdentity;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};

/// Grok's ACP quirks, in one place. Everything not named here is the
/// shared core's (`crate::acp`).
const GROK: AcpSpec = AcpSpec {
    provider_id: "grok",
    acp_args: spawn_args,
    // Nothing beyond eave's own variables: Grok's auto-approve posture
    // lives in `~/.grok/config.toml`, not in the environment, so there is
    // nothing here to scrub. It is caught on the wire instead — see
    // [`posture_refusal`].
    child_env: |cfg| ChildEnv::sanitized().and_extra(&cfg.extra_env),
    // Grok emits session updates under BOTH the standard ACP notification
    // and its own vendor channel, and they are not interchangeable in
    // practice: `tool_call` arrives on `session/update` while
    // `turn_completed` arrives on `_x.ai/session_notification` (capture
    // 2026-08-20). Reading only the standard one loses the end of every
    // turn.
    update_channels: &["session/update", "_x.ai/session_notification"],
    update: UpdateSpec {
        // Grok puts its real tool name on the wire, so eave shows the real
        // tool name rather than ACP's coarse `kind`.
        tool_naming: ToolNaming::VendorMeta(GROK_TOOL_META),
        tool_input: ToolInput::RawInput,
        // Reasoning is not the reply, and the Claude adapter surfaces only
        // assistant text too.
        thoughts: Thoughts::Dropped,
        progress_events: false,
        // Grok reports the end of a turn on the wire (`turn_completed`),
        // so there is no RPC reply to flush accumulated text at.
        accumulate_turn_text: false,
    },
    asking_modes: &[GROK_MODE_ASKS],
};

/// Where Grok hangs its own tool metadata on an ACP tool call.
const GROK_TOOL_META: &str = "x.ai/tool";

/// The posture string for a session that routes permission questions to
/// eave, and the one for a session that answers them itself. These are
/// eave's names for what `yolo` means, not strings Grok emits.
const GROK_MODE_ASKS: &str = "default";
const GROK_MODE_YOLO: &str = "always-approve";

/// First JSON-RPC id handed to a `session/prompt`. Above the two handshake
/// ids the shared core owns, and only ever increasing, so a late response
/// can always be attributed.
/// `session/set_model` (Slice TN2). Its own id so its reply can never be
/// read as a prompt's — the `_` arm below decodes a prompt response, and
/// a `{}` result there would be a turn nobody ran.
const RPC_SET_MODEL: i64 = 4;
const RPC_PROMPT_BASE: i64 = 16;

fn set_model_frame(session_id: &str, model: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": RPC_SET_MODEL,
        "method": "session/set_model",
        "params": { "sessionId": session_id, "modelId": model },
    })
}

/// ACP session state and the outstanding-tool set, kept together because
/// both are read by the trait methods off the reader task.
#[derive(Default)]
pub struct GrokProto {
    session_id: Option<String>,
    stream: AcpStream,
    pending: PendingApprovals,
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
        self.stream.in_flight()
    }

    pub fn pending_approvals(&self) -> usize {
        self.pending.len()
    }

    pub fn approval_is_open(&self, request_id: &str) -> bool {
        self.pending.is_open(request_id)
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
    child: AcpChild,
    shared: Arc<Shared>,
}

struct Shared {
    proto: Mutex<GrokProto>,
    /// Settled once `session/new` (or `session/load`) returns.
    /// `send_user_message` waits on this rather than racing the handshake.
    ready: HandshakeGate,
}

impl GrokSession {
    /// Spawn one `grok agent stdio` process in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let shared = Arc::new(Shared {
            proto: Mutex::new(GrokProto::new()),
            ready: HandshakeGate::new(),
        });

        let resume_id = acp::resume_cursor(cfg);
        let session_cwd = cfg.cwd.display().to_string();
        let reader_shared = Arc::clone(&shared);

        let (child, events_rx) = AcpChild::spawn(
            ChildSpec {
                program: cfg.program.clone(),
                cwd: cfg.cwd.clone(),
                args: (GROK.acp_args)(cfg),
                env: (GROK.child_env)(cfg),
            },
            move |mut lines, ctx, tx| async move {
                // Open the ACP handshake. Everything else is driven by
                // what comes back.
                wire::try_write(&ctx.stdin, &wire::initialize_frame()).await;
                while let Some(line) = lines.next().await {
                    let (events, frames) = {
                        let mut proto = reader_shared.proto.lock().unwrap();
                        proto.handle_line(
                            &line,
                            resume_id.as_deref(),
                            &session_cwd,
                            &reader_shared.ready,
                        )
                    };
                    for frame in frames {
                        wire::try_write(&ctx.stdin, &frame).await;
                    }
                    for event in events {
                        // Receiver gone just means nobody is listening;
                        // keep draining so the child never blocks on a
                        // full pipe.
                        let _ = tx.send(event).await;
                    }
                }
                // The stream ended. A caller blocked in `await_session`
                // would otherwise wait on a handshake that can no longer
                // complete: the sender lives here, so settling it as a
                // stated failure is the reader's last act.
                reader_shared.ready.settle_if_pending(
                    "the grok process ended before it opened an ACP session, so Eave has \
                     nothing to send a prompt to",
                );
            },
        )?;

        Ok((Self { child, shared }, events_rx))
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
///   operator config cannot move the work out from under us. comet
///   independently landed on the same flag for a different reason
///   (`comet/crates/harness/src/acp/mod.rs:239` — a stale leader reads as
///   total silent non-response); two apps, one flag, two failure modes.
/// - **No `--always-approve`, ever.** The whole point of the slice is that
///   eave is the thing being asked.
///
/// `-m` goes before the subcommand because it is an option of `grok
/// agent`, not of `stdio`. An absent model means no `-m` at all, which is
/// not the same as an empty one (Slice P2).
fn spawn_args(cfg: &SpawnConfig) -> Vec<String> {
    // `--permission-mode` is `grok`'s own flag, so it precedes the
    // subcommand. `default` is the posture that asks (the possible values
    // are `default, acceptEdits, auto, dontAsk, bypassPermissions, plan`;
    // `bypassPermissions` IS `--always-approve`). Measured 2026-09-02 to
    // override `[ui] permission_mode = "always-approve"` in config.toml.
    let mut args = vec![
        "--permission-mode".to_string(),
        GROK_MODE_ASKS.to_string(),
        "agent".to_string(),
        "--no-leader".to_string(),
    ];
    if let Some(model) = &cfg.model {
        args.push("-m".into());
        args.push(model.clone());
    }
    // Tuning: argv only (`grok agent --reasoning-effort`, alias `--effort`),
    // an option of `grok agent` like `-m`, so before the subcommand. Absent
    // = the CLI's remembered per-model choice, which is what a bare start
    // gets. No fast lane on this CLI.
    if let Some(level) = &cfg.tuning.effort {
        args.push("--reasoning-effort".into());
        args.push(level.clone());
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
    /// settles the channel at EOF.
    async fn await_session(&self) -> Result<String, SessionError> {
        self.shared
            .ready
            .session_id()
            .await
            .map_err(|err| match err {
                HandshakeError::Refused(reason) => SessionError::Io(std::io::Error::other(reason)),
                HandshakeError::Gone => SessionError::StdinClosed,
            })
    }

    fn stdin(&self) -> &SharedStdin {
        self.child.stdin()
    }
}

#[async_trait]
impl crate::provider::AgentAdapter for GrokSession {
    const PROVIDER_ID: &'static str = GROK.provider_id;

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "grok.rs `posture_refusal`: the ACP session's own `yolo` field is read off the \
                   wire before the first tool call, and a session that reports `yolo: true` emits \
                   PostureRefused and is killed. Slice AV verified denial is not advisory — a \
                   denied write left the directory empty and the turn ended PermissionRejected. \
                   Since Slice TN2 (2026-09-02) this is verify-AND-correct: Eave spawns \
                   `grok --permission-mode default agent … stdio`, measured to turn a machine's \
                   `permission_mode = \"always-approve\"` config into `yolo: false` on the wire, \
                   and the verification stays in front of the first tool call regardless.",
    };

    const REWIND: crate::provider::RewindSupport = crate::provider::RewindSupport::None {
        reason: "Grok's ACP wire has no conversation rewind: `session/load` only. Its binary \
                 contains `session/fork`, but as a Rust MODULE PATH and never as a quoted \
                 method name — checked 2026-08-31.",
    };

    const TUNING: crate::tuning::TuningSupport = crate::tuning::TuningSupport {
        effort: crate::tuning::EffortSupport::Levels {
            levels: &["low", "medium", "high", "xhigh"],
            applies: crate::tuning::Applies::NextSpawn,
            evidence: "`grok agent --reasoning-effort <EFFORT>` (1.0.13 `--help`, no enum \
                       printed). The words are from a live `session/new` 2026-09-02: \
                       `availableModels[]._meta.reasoningEfforts` lists low/medium/high/xhigh \
                       for grok-4.6 (default high) and low/medium/high for grok-4.5; with the \
                       flag set to `low` the same handshake's `_x.ai/models/update` echoed \
                       `reasoningEffort: low`. Union across models; argv only — the ACP surface \
                       has `session/set_model` but no effort setter.",
        },
        speed: crate::tuning::SpeedSupport::None {
            reason: "no fast lane on this CLI: `grok agent --help` has no speed/fast option and \
                     the binary's only `speed` strings are scroll settings (2026-09-02).",
        },
        model: crate::tuning::ModelSupport::Switch {
            applies: crate::tuning::Applies::NextTurn,
            evidence: "ACP `session/set_model { sessionId, modelId }`; grok's `session/new` \
                       answers with `availableModels[]` and its binary carries the method \
                       (`grok.rs` note of 2026-09-02: \"has `session/set_model` but no effort \
                       setter\"). A refused id is grok's own error response, journalled and \
                       shown.",
        },
    };

    const TERMINAL: crate::provider::TerminalSupport = crate::provider::TerminalSupport::Native {
        evidence: "`grok --resume <ID>`. Its `--help` (read 2026-09-01) documents `-r, --resume \
                   [<SESSION_ID_OR_TITLE>]` with \"UUID-shaped values always mean IDs\", which \
                   is what makes an id unambiguous against the title matching it also does. \
                   Grok mints its own session ids and eave already journals the one it \
                   announced (see the module header), so that is the cursor handed over. The \
                   binary's own name for itself is \"Grok Build TUI\".",
    };

    fn tui_args(cursor: Option<&str>) -> Option<Vec<String>> {
        // `--resume` with no value resumes the most recent session for the
        // cwd, which is a DIFFERENT conversation than the one being handed
        // over whenever the operator has two open on one checkout. So a
        // session with no cursor gets a fresh TUI, never a bare `--resume`.
        Some(match cursor {
            Some(id) => vec!["--resume".into(), id.into()],
            None => Vec::new(),
        })
    }

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        GrokSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for GrokSession {
    fn identity(&self) -> ProcIdentity {
        self.child.identity()
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
        self.child.last_activity()
    }

    fn watch(&self) -> SessionWatch {
        self.child.watch()
    }

    /// `session/set_model`, live (Slice TN2). The effort level is
    /// argv-only on this CLI and never reaches here — the engine restarts
    /// for it, and that argv carries the model too.
    async fn tune(&mut self, tuning: &crate::tuning::Tuning) -> Result<(), SessionError> {
        let Some(model) = &tuning.model else {
            return Ok(());
        };
        let session_id = self.await_session().await?;
        wire::write_frame(self.stdin(), &set_model_frame(&session_id, model)).await
    }

    async fn send_user_message(
        &mut self,
        text: &str,
        images: &[crate::media::ImageRef],
    ) -> Result<(), SessionError> {
        let session_id = self.await_session().await?;
        let id = self.shared.proto.lock().unwrap().next_prompt_id();
        wire::write_frame(
            self.stdin(),
            &wire::prompt_frame(id, &session_id, text, &crate::media::encode_all(images)?),
        )
        .await
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
                ApprovalDecision::Allow => match pending.option_of_kind("allow_once") {
                    Some(option) => (pending.rpc_id.clone(), Some(option)),
                    None => return Ok(false),
                },
                // A deny is ALWAYS expressible: the named reject option if
                // there is one, and ACP's own `cancelled` outcome if there
                // is not. Refusing must never fail for want of vocabulary.
                ApprovalDecision::Deny { .. } => (
                    pending.rpc_id.clone(),
                    pending.option_of_kind("reject_once"),
                ),
            }
        };
        permission::deliver(self.stdin(), &rpc_id, option_id.as_deref()).await?;
        // Only now is the question closed: a failed write leaves it open so
        // the operator can try again rather than facing a dead card.
        self.shared.proto.lock().unwrap().pending.close(request_id);

        if let ApprovalDecision::Deny { message } = decision {
            if !message.trim().is_empty() {
                let session_id = self.await_session().await?;
                let id = self.shared.proto.lock().unwrap().next_prompt_id();
                wire::write_frame(
                    self.stdin(),
                    &wire::prompt_frame(id, &session_id, message, &[]),
                )
                .await?;
            }
        }
        Ok(true)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    /// Unsupported, and stated in one place only: the reason an operator
    /// reads lives on `REWIND`, so this never grows a second sentence
    /// that could drift from it.
    async fn rewind(
        &mut self,
        _target: &crate::session::RewindTarget,
    ) -> Result<crate::session::RewindOutcome, SessionError> {
        Ok(crate::session::RewindOutcome::Unsupported)
    }

    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.child.kill().await
    }
}

// ---------------------------------------------------------------------------
// ACP protocol plumbing

impl GrokProto {
    /// Parse one JSON-RPC line into events for the engine and frames to
    /// write back. Never fails: malformed input becomes a `Malformed` event
    /// and parsing continues on the next line.
    fn handle_line(
        &mut self,
        line: &str,
        resume_id: Option<&str>,
        cwd: &str,
        ready: &HandshakeGate,
    ) -> (Vec<SessionEvent>, Vec<Value>) {
        let line = line.trim();
        if line.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return (vec![wire::malformed(line, err.to_string())], Vec::new()),
        };

        match wire::classify(&v) {
            Frame::Request { method, id } => self.handle_request(method, &id.clone(), &v, line),
            Frame::Notification { method } => {
                (self.handle_notification(method, &v, line), Vec::new())
            }
            Frame::Response { id } => {
                let id = id.as_i64();
                self.handle_response(id, &v, line, resume_id, cwd, ready)
            }
            Frame::Unclassifiable => (
                vec![wire::malformed(
                    line,
                    "neither request, notification, nor response".into(),
                )],
                Vec::new(),
            ),
        }
    }

    fn handle_response(
        &mut self,
        id: Option<i64>,
        v: &Value,
        line: &str,
        resume_id: Option<&str>,
        cwd: &str,
        ready: &HandshakeGate,
    ) -> (Vec<SessionEvent>, Vec<Value>) {
        let error = v.get("error");
        match id {
            Some(wire::RPC_INITIALIZE) => {
                if let Some(err) = error {
                    let reason = format!("grok rejected the ACP handshake: {err}");
                    ready.refuse(reason);
                    return (
                        vec![SessionEvent::System {
                            subtype: "grok_initialize_failed".into(),
                            raw: line.to_string(),
                        }],
                        Vec::new(),
                    );
                }
                // Handshake accepted: open (or reload) the session.
                // A RELOAD replays the conversation as live updates before
                // it answers, so the gate goes up here and comes down at
                // the response below. A fresh `session/new` replays
                // nothing and must not be gated.
                if resume_id.is_some() {
                    self.stream.begin_replay();
                }
                // Record what grok said it can do before moving on. This
                // arm used to return `Vec::new()` and drop the whole
                // result — see `acp::Advertisement` for what that cost.
                let advert = acp::advertisement(v.get("result")).record();
                (vec![advert], vec![wire::session_frame(resume_id, cwd)])
            }
            Some(wire::RPC_SESSION) => {
                if let Some(err) = error {
                    // The loud case that matters most: `--resume` with an
                    // id Grok never issued. Grok answers `FS_NOT_FOUND`
                    // rather than silently starting a fresh conversation,
                    // and eave passes that on rather than pretending the
                    // history is there.
                    let reason = match resume_id {
                        Some(id) => format!(
                            "grok could not resume session {id:?}: {err}. Grok issues its own \
                             session ids at session/new; an id Eave chose is not one of them."
                        ),
                        None => format!("grok could not open a session: {err}"),
                    };
                    ready.refuse(reason);
                    // Disarm even here. A refused load leaves the session
                    // dead, but a gate left armed would silently swallow
                    // anything the agent said on its way out.
                    self.stream.end_replay();
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
                let absorbed = self.stream.end_replay();
                match session_id {
                    Some(id) => {
                        self.session_id = Some(id.clone());
                        ready.ready(id);
                    }
                    None => {
                        let reason = "grok opened a session without naming it".to_string();
                        ready.refuse(reason.clone());
                        return (vec![wire::malformed(line, reason)], Vec::new());
                    }
                }
                (absorbed.into_iter().collect(), Vec::new())
            }
            // A prompt turn's response. `turn_completed` already reported
            // the OUTCOME, so this is the bookend for that — but it is not
            // empty: the settled response is where ACP puts the turn's
            // token bill, and reading the outcome while dropping the bill
            // was the narrowing this slice exists to stop (slice CC).
            // Slice TN2. Success says nothing (the composer already shows
            // the operator's choice); a refusal is grok's own error, kept
            // verbatim under the subtype the transcript renders.
            Some(RPC_SET_MODEL) => (
                error
                    .map(|_| SessionEvent::System {
                        subtype: "error".into(),
                        raw: line.to_string(),
                    })
                    .into_iter()
                    .collect(),
                Vec::new(),
            ),
            _ => (
                crate::acp::usage::from_prompt_response(v.get("result"))
                    .into_iter()
                    .collect(),
                Vec::new(),
            ),
        }
    }

    fn handle_request(
        &mut self,
        method: &str,
        rpc_id: &Value,
        v: &Value,
        _line: &str,
    ) -> (Vec<SessionEvent>, Vec<Value>) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "session/request_permission" => {
                (vec![self.note_permission(rpc_id, &params)], Vec::new())
            }
            // eave declares no filesystem or terminal capability at
            // `initialize`, precisely so that Grok performs its own I/O and
            // the permission gate above is the ONE place a write is
            // reviewed. A capability request arriving anyway is answered
            // with a plain JSON-RPC "method not found" and journaled as the
            // refusal it is — never silently serviced, which would make
            // eave the thing performing the write it is supposed to be
            // policing.
            _ => (
                vec![SessionEvent::DialogRefused {
                    request_id: wire::id_key(rpc_id),
                    dialog: method.to_string(),
                }],
                vec![wire::method_not_found_frame(rpc_id, method)],
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
            .and_then(|m| m.get(GROK_TOOL_META))
            .cloned()
            .unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or(Value::Null);

        // The request id eave answers with. Grok's own tool-call id is the
        // stable, meaningful choice — it is the same id the tool events
        // carry, so an approval card and its tool row name the same thing.
        let request_id = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| wire::id_key(rpc_id));

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

        self.pending.open(request_id.clone(), rpc_id, &options);

        SessionEvent::ApprovalRequested {
            request_id,
            tool,
            display_name,
            description: title,
            path: permission::approval_path(&tool_call, Some(GROK_TOOL_META)),
            input_json: permission::approval_input(&tool_call),
            suggestions_json: permission::suggestions(&options),
        }
    }
}

impl GrokProto {
    fn handle_notification(&mut self, method: &str, v: &Value, line: &str) -> Vec<SessionEvent> {
        if GROK.carries_updates(method) {
            let update = v
                .get("params")
                .and_then(|p| p.get("update"))
                .cloned()
                .unwrap_or(Value::Null);
            return self.handle_update(&update, line);
        }
        match method {
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
            // Everything else Grok pushes, recorded verbatim.
            //
            // This was `Vec::new()` — a silent drop — and the live probe
            // (2026-08-20, grok 1.0.5) is what exposed it: `strings` over
            // the shipped binary names **204** distinct `_x.ai/...`
            // methods and this adapter handles two, so ~202 vendor
            // notifications were being discarded without a record.
            // Confirmed on real bytes: `_x.ai/task_backgrounded` and
            // `_x.ai/task_completed` — Grok's background-work lifecycle,
            // the frames that answer whether it self-starts — produced
            // NOTHING at all, not even an unknown.
            //
            // Every other adapter in this crate already records what it
            // does not understand (`claude.rs` `System`/`Unknown`,
            // `codex.rs` `foreign_thread:` since slice CH), and
            // `events.rs` states the contract on `Unknown`: "Surfaced,
            // never fatal". Grok was the one that did not.
            other => vec![SessionEvent::System {
                subtype: other.to_string(),
                raw: line.to_string(),
            }],
        }
    }

    fn handle_update(&mut self, update: &Value, line: &str) -> Vec<SessionEvent> {
        // Above the match, not inside it: a replayed `turn_completed` is
        // as wrong as a replayed message chunk, and would settle a turn
        // that has not started.
        if self.stream.replay_absorbs() {
            return Vec::new();
        }
        match crate::acp::update::kind_of(update) {
            // Grok reports the end of a turn on the wire rather than in the
            // prompt's RPC reply.
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
            // The fail-closed backstop. If a tool is starting and the
            // session has still not stated its posture, eave has no proof
            // it will ever be asked, so it refuses HERE rather than letting
            // the first write through on the assumption that silence means
            // "asks". This is the Grok analogue of Slice K's "a CLI that
            // named no mode at all is unproven, never probably fine".
            "tool_call" => {
                let mut out = Vec::new();
                if !self.announced {
                    out.extend(self.refuse_unstated_posture());
                }
                out.extend(self.stream.decode(&GROK.update, update, line));
                out
            }
            _ => self.stream.decode(&GROK.update, update, line),
        }
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
            fast_lane: None,
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
                fast_lane: None,
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

/// Postures that provably ASK — the verdict comes from the shared,
/// fail-closed rule ([`crate::acp::classify_mode`]); the sentence is
/// Grok's own, because the fix is Grok's own.
fn posture_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    match acp::classify_mode(GROK.asking_modes, mode) {
        ModeVerdict::Asks => None,
        ModeVerdict::Unproven(other) if other == GROK_MODE_YOLO => {
            Some(SessionEvent::PostureRefused {
                mode: GROK_MODE_YOLO.into(),
                reason: "this Grok session auto-approves every tool call, so Eave would never be \
                         asked before it writes — and it does so DESPITE Eave's own \
                         `--permission-mode default` on its argv, which is measured to \
                         override `[ui] permission_mode = \"always-approve\"` in \
                         ~/.grok/config.toml. Either this Grok no longer honours the flag or \
                         something switched the session after it started (/always-approve). \
                         Reopen the session; if it repeats, the CLI changed."
                    .into(),
            })
        }
        ModeVerdict::Unproven(other) => Some(SessionEvent::PostureRefused {
            mode: other.clone(),
            reason: format!(
                "this Grok session reports permission posture {other:?}, which Eave cannot prove \
                 asks before it writes"
            ),
        }),
        ModeVerdict::Unstated => Some(SessionEvent::PostureRefused {
            mode: "unstated".into(),
            reason: "this Grok session started running tools without ever stating whether it \
                     asks before it writes, so Eave cannot prove its writes are reviewable"
                .into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::Handshake;

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

    /// A parser plus the gate the handshake reports through.
    struct Harness {
        proto: GrokProto,
        gate: HandshakeGate,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                proto: GrokProto::new(),
                gate: HandshakeGate::new(),
            }
        }

        fn feed(&mut self, line: &str) -> (Vec<SessionEvent>, Vec<Value>) {
            self.proto.handle_line(line, None, "/tmp/probe", &self.gate)
        }

        fn feed_resuming(&mut self, line: &str, id: &str) -> (Vec<SessionEvent>, Vec<Value>) {
            self.proto
                .handle_line(line, Some(id), "/tmp/probe", &self.gate)
        }

        /// Drive the handshake to the point a real session has a name.
        fn opened() -> Self {
            let mut h = Self::new();
            h.feed(SESSION_NEW);
            h
        }

        /// The gate's settled state. Read directly rather than awaited:
        /// these tests feed lines synchronously, so a wait would have
        /// nothing left to wait for.
        fn ready(&self) -> Handshake {
            self.gate.state()
        }
    }

    #[test]
    fn the_handshake_opens_a_session_and_then_names_it() {
        let mut h = Harness::new();
        // A successful `initialize` produces exactly one outbound frame:
        // the request to open a session.
        let (events, frames) = h.feed(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{},"resume":{},"close":{}}}}}"#,
        );
        // The handshake is still not TRANSCRIPT — nothing here is anything
        // grok said to the operator. But it is no longer silent either:
        // slice CL records the capability advertisement, because dropping
        // the whole `initialize` result is how eave ended up unable to
        // answer which session methods its own agents implement.
        match events.as_slice() {
            [SessionEvent::System { subtype, raw }] if subtype == "acp_capabilities" => {
                assert!(
                    raw.contains(r#""sessionCapabilities":["close","list","resume"]"#),
                    "{raw}"
                );
            }
            other => panic!("the handshake records capabilities and nothing else: {other:?}"),
        }
        assert_eq!(frames.len(), 1);
        let sent = &frames[0];
        assert_eq!(sent["method"], "session/new");
        assert_eq!(sent["params"]["cwd"], "/tmp/probe");

        h.feed(SESSION_NEW);
        assert_eq!(h.proto.session_id(), Some(SESSION_ID));
        assert_eq!(h.ready(), Handshake::Ready(SESSION_ID.to_string()));
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
        let sent = &frames[0];
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
        let Handshake::Refused(reason) = h.ready() else {
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
        assert_eq!(
            pending.option_of_kind("allow_once").as_deref(),
            Some("allow-once")
        );
        assert_eq!(
            pending.option_of_kind("reject_once").as_deref(),
            Some("reject-once")
        );
    }

    /// A refusal must always be expressible. When a request arrives with no
    /// `reject_once` option, the deny falls back to ACP's own `cancelled`
    /// outcome rather than failing to answer.
    #[test]
    fn a_deny_is_expressible_even_when_no_reject_option_is_offered() {
        let parsed = wire::permission_response_frame(&Value::from(0), Some("reject-once"));
        assert_eq!(parsed["result"]["outcome"]["outcome"], "selected");
        assert_eq!(parsed["result"]["outcome"]["optionId"], "reject-once");

        let parsed = wire::permission_response_frame(&Value::from(0), None);
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
            [SessionEvent::ToolUseStarted {
                id,
                name,
                input,
                call,
            }] => {
                assert_eq!(id, "call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0");
                // The vendor's own tool name, not the display title.
                assert_eq!(name, "write");
                assert!(input.contains("hello.txt"));
                // Slice CB, decoded in the shared ACP core: Grok sends no
                // `kind` on `tool_call`, so this types by the vendor name
                // out of `_meta["x.ai/tool"]`. A `Write` that named no
                // path would be the failure worth catching — the row would
                // say "wrote a file" and not which.
                assert_eq!(
                    call.as_ref(),
                    Some(&crate::tool::ToolCall::Write {
                        path: "/tmp/probe1/hello.txt".into()
                    }),
                    "a typed write must carry its path"
                );
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

    /// Slice AG1 #4 — the before-text survives instead of being discarded.
    ///
    /// `flatten_content` keeps `newText` as the result, which is right:
    /// what the file became is what the tool returned. But an inline diff
    /// needs the other side, and until this slice `oldText` was read past
    /// and dropped. This is the real Grok capture, so the shape is the
    /// agent's and not one eave invented.
    #[test]
    fn a_grok_edit_keeps_the_text_the_file_had_before() {
        let mut h = Harness::new();
        h.feed(TOOL_CALL);
        let (events, _) = h.feed(TOOL_DONE);
        match events.as_slice() {
            [SessionEvent::ToolUseFinished { result, diff, .. }] => {
                // The result is unchanged by this slice.
                assert_eq!(result, "hi\n", "the result stopped being newText");
                let d = diff.as_ref().expect("the diff block was discarded");
                assert_eq!(d.path, "/tmp/probe1/hello.txt");
                assert_eq!(d.new_text, "hi\n");
                // This capture CREATES the file, so the old side is
                // genuinely empty — a pure addition, not a missing fact.
                assert_eq!(d.old_text, "", "a creation has no before-text");
            }
            other => panic!("expected ToolUseFinished, got {other:?}"),
        }
    }

    /// ...and a real modification carries a real before-text, which is the
    /// case a creation cannot prove.
    #[test]
    fn a_grok_edit_over_existing_content_carries_both_sides() {
        let mut h = Harness::new();
        h.feed(TOOL_CALL);
        let done = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-adbc8e1b-54ac-438e-b439-1b75e3de6766-0","status":"completed","content":[{"type":"diff","path":"a.txt","oldText":"one\ntwo\n","newText":"one\nTWO\n"}]}}}"#;
        let (events, _) = h.feed(done);
        match events.as_slice() {
            [SessionEvent::ToolUseFinished { diff, .. }] => {
                let d = diff.as_ref().expect("no diff");
                assert_eq!(d.old_text, "one\ntwo\n");
                assert_eq!(d.new_text, "one\nTWO\n");
                assert_ne!(d.old_text, d.new_text, "both sides are the same");
            }
            other => panic!("expected ToolUseFinished, got {other:?}"),
        }
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
    /// anyway is refused on the wire, not quietly serviced — and the
    /// refusal is VISIBLE.
    ///
    /// `DialogRefused`, not `Unknown`: every other eave adapter
    /// (`stream.rs`, `codex.rs`, `pi.rs`, `hermes.rs`) reports an
    /// unservable client request that way, and `eave-wire`'s model renders
    /// it as a marker row. Grok alone used to emit `Unknown`, which the
    /// model draws as nothing at all — so an agent trying to route a write
    /// through eave left no trace an operator could see.
    #[test]
    fn a_filesystem_request_is_refused_rather_than_serviced() {
        let mut h = Harness::opened();
        let write_req = r#"{"jsonrpc":"2.0","id":7,"method":"fs/write_text_file","params":{"path":"/tmp/probe/x.txt","content":"hi"}}"#;
        let (events, frames) = h.feed(write_req);
        assert_eq!(frames.len(), 1);
        let sent = &frames[0];
        assert_eq!(sent["id"], 7);
        assert_eq!(sent["error"]["code"], -32601);
        assert!(sent.get("result").is_none(), "a refusal is not a result");
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::DialogRefused { request_id, dialog }]
                    if dialog == "fs/write_text_file" && request_id == "7"
            ),
            "the refusal must reach the transcript, got {events:?}"
        );
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
        assert_eq!(
            args,
            vec![
                "--permission-mode",
                "default",
                "agent",
                "--no-leader",
                "stdio"
            ]
        );
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
        let args = spawn_args(&cfg);
        assert!(!args.iter().any(|a| a == "--always-approve"));
        // Slice TN2: the ONE permission flag eave passes is the asking
        // posture, and it is `grok`'s flag, so it precedes `agent`.
        let modes: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.starts_with("--permission"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(modes.len(), 1, "{args:?}");
        assert_eq!(args[modes[0]], "--permission-mode");
        assert_eq!(args[modes[0] + 1], GROK_MODE_ASKS);
        assert!(modes[0] < args.iter().position(|a| a == "agent").unwrap());
        assert_ne!(GROK_MODE_ASKS, "bypassPermissions");
    }

    // -----------------------------------------------------------------
    // Slice CC, decoded in the shared ACP core (`acp::usage`) after slice
    // CA moved the protocol out of this file. The tests stay HERE because
    // they drive `GrokProto::handle_line` end to end — a decoder that is
    // correct in isolation and unreachable from the wire is not a feature.

    /// The settled prompt response carries the turn's bill on a
    /// first-class `usage` (ACP 1.x, grok 1.0.5).
    #[test]
    fn a_settled_prompt_response_states_the_turns_tokens() {
        let mut h = Harness::opened();
        let line = r#"{"jsonrpc":"2.0","id":100,"result":{"stopReason":"end_turn","usage":{"totalTokens":50103,"inputTokens":48210,"outputTokens":1893,"cachedReadTokens":41000,"cachedWriteTokens":0,"thoughtTokens":412}}}"#;
        match h.feed(line).0.as_slice() {
            [SessionEvent::TokenUsage { turn, context }] => {
                let turn = turn.expect("stated");
                // Taken as sent: grok's own manual says the ACP
                // `inputTokens` is already the full prompt sum, so adding
                // the cache buckets here would double-count 41,000.
                assert_eq!(turn.input, Some(48_210));
                assert_eq!(turn.output, Some(1_893));
                assert_eq!(turn.cache_read, Some(41_000));
                assert_eq!(turn.cache_write, Some(0));
                assert_eq!(
                    *context, None,
                    "a bill is not a level; the gauge rides usage_update"
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// comet found the same numbers on `_meta` at grok 1.0.4
    /// (`comet/crates/harness/src/acp/mod.rs:1665`). Both spellings are
    /// read so an older grok is not silently dropped.
    #[test]
    fn the_older_meta_spelling_comet_found_still_reads() {
        let mut h = Harness::opened();
        let line = r#"{"jsonrpc":"2.0","id":100,"result":{"stopReason":"end_turn","_meta":{"inputTokens":812,"outputTokens":45}}}"#;
        match h.feed(line).0.as_slice() {
            [SessionEvent::TokenUsage { turn, .. }] => {
                let turn = turn.expect("stated");
                assert_eq!(turn.input, Some(812));
                assert_eq!(turn.output, Some(45));
                assert_eq!(turn.cache_read, None, "1.0.4 did not say, so eave does not");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A response with no usage anywhere emits nothing at all. Silence is
    /// the honest answer; a zeroed record would read as a free turn.
    #[test]
    fn a_response_without_usage_emits_nothing() {
        let mut h = Harness::opened();
        let line = r#"{"jsonrpc":"2.0","id":100,"result":{"stopReason":"end_turn"}}"#;
        assert!(h.feed(line).0.is_empty());
    }

    /// `usage_update` is the context gauge and ONLY the context gauge —
    /// the distinction comet recorded in `docs/research/acp.md` and
    /// deliberately left unmapped. eave maps it because its event has a
    /// separate slot for it that cannot be mistaken for a bill.
    #[test]
    fn a_usage_update_states_the_level_and_never_the_bill() {
        let mut h = Harness::opened();
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"usage_update","used":22457,"size":1000000,"cost":{"amount":0.01,"currency":"USD"}}}}"#;
        match h.feed(line).0.as_slice() {
            [SessionEvent::TokenUsage { turn, context }] => {
                assert_eq!(*turn, None, "a gauge says nothing about what a turn cost");
                let context = context.expect("stated");
                assert_eq!(context.used_tokens, 22_457);
                assert_eq!(context.limit_tokens, Some(1_000_000));
                assert!((context.fraction().unwrap() - 0.022457).abs() < 1e-6);
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A `usage_update` shaped differently from the schema's is journaled
    /// verbatim, not guessed at.
    #[test]
    fn an_unreadable_usage_update_is_kept_verbatim() {
        let mut h = Harness::opened();
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"usage_update","cost":{"amount":0.01,"currency":"USD"}}}}"#;
        match h.feed(line).0.as_slice() {
            [SessionEvent::System { subtype, raw }] => {
                assert_eq!(subtype, "usage_update");
                assert_eq!(raw, line);
            }
            other => panic!("got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // The `session/load` replay gate.

    /// **A resumed session must not journal its history twice.**
    ///
    /// `session/load` answers by re-sending the whole conversation as
    /// ordinary `session/update` notifications, which arrive BEFORE the
    /// load response and are indistinguishable from live traffic by
    /// shape. Without a gate, resuming a session appends everything the
    /// agent has ever said to the journal a second time.
    ///
    /// This became reachable with slice CD. Before it, the engine resumed
    /// under eave's own uuid — an id Grok never issued — so `session/load`
    /// always errored and no replay was ever received. Fixing resume is
    /// what made this live, which is the reason it is tested here rather
    /// than left as a note.
    #[test]
    fn a_resumed_session_absorbs_the_replay_instead_of_journaling_it_twice() {
        let mut h = Harness::new();
        let id = "01a020c5-af30-7fb3-b886-bcaa75a72299";
        // Handshake, resuming: the reply asks for `session/load`.
        h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
            id,
        );

        // Grok now replays the conversation. Every one of these would be a
        // duplicate record.
        for replayed in [
            MSG_CHUNK,
            TOOL_CALL,
            TOOL_DONE,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"01a020c5-af30-7fb3-b886-bcaa75a72299","update":{"sessionUpdate":"turn_completed","stop_reason":"end_turn"}}}"#,
        ] {
            let (events, _) = h.feed_resuming(replayed, id);
            assert!(
                events.is_empty(),
                "replayed update must be absorbed, got {events:?}"
            );
        }

        // The load settles. The absorption is STATED, not silent: a silent
        // drop is indistinguishable from a provider that replayed nothing,
        // which is also what a broken `session/load` looks like.
        let (events, _) = h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":2,"result":{"models":{"currentModelId":"grok-4.6"}}}"#,
            id,
        );
        match events.as_slice() {
            [SessionEvent::System { subtype, raw }] => {
                assert_eq!(subtype, "acp_replay_absorbed");
                assert!(raw.contains(r#""absorbed":4"#), "{raw}");
            }
            other => panic!("expected the absorption record, got {other:?}"),
        }

        // And the gate is DOWN: live traffic after the load is decoded.
        let (events, _) = h.feed_resuming(MSG_CHUNK, id);
        assert!(
            matches!(events.as_slice(), [SessionEvent::TextDelta { .. }]),
            "post-load traffic is live, got {events:?}"
        );
    }

    /// A FRESH session replays nothing, so the gate must never arm for one
    /// — arming it would swallow the first live turn.
    #[test]
    fn a_fresh_session_is_never_gated() {
        let mut h = Harness::new();
        h.feed(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#);
        h.feed(SESSION_NEW);
        let (events, _) = h.feed(MSG_CHUNK);
        assert!(
            matches!(events.as_slice(), [SessionEvent::TextDelta { .. }]),
            "a fresh session's first words are live, got {events:?}"
        );
    }

    /// A load that FAILS must still lower the gate. The session is dead
    /// either way, but an armed gate would silently swallow whatever the
    /// agent said on its way out — including a refusal an operator needs.
    #[test]
    fn a_failed_load_lowers_the_gate() {
        let mut h = Harness::new();
        let id = "01a020c5-af30-7fb3-b886-bcaa75a72299";
        h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
            id,
        );
        h.feed_resuming(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"FS_NOT_FOUND"}}"#,
            id,
        );
        let (events, _) = h.feed_resuming(MSG_CHUNK, id);
        assert!(
            matches!(events.as_slice(), [SessionEvent::TextDelta { .. }]),
            "a lowered gate lets the agent's last words through, got {events:?}"
        );
    }

    // -----------------------------------------------------------------
    // The live self-start capture (grok 1.0.5, probe 2026-08-20).

    fn selfstart_capture() -> String {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/grok-selfstart-1.0.5.jsonl"
        );
        std::fs::read_to_string(p).expect("fixture")
    }

    /// **The settled prompt response bills the WHOLE turn**, verified
    /// live at grok 1.0.5.
    ///
    /// This test was written to prove the opposite and disproved itself,
    /// which is why it is worth keeping. The response `_meta` carries
    /// BOTH `inputTokens: 18431` — the last model call — and
    /// `_meta.usage.inputTokens: 53677` with `modelCalls: 3`, the turn.
    /// Slice CC's fallback chain reads `_meta.usage` first, so it was
    /// already correct; reading `_meta` directly would under-report this
    /// turn by 2.9x, and by more the more model calls it made.
    ///
    /// What the probe DID find: Grok spells the cache-write bucket
    /// `cacheCreationTokens` here, where the ACP-generic path spells it
    /// `cachedWriteTokens`. The stated zero was being dropped.
    #[test]
    fn the_settled_response_bills_the_whole_turn_not_its_last_model_call() {
        let mut h = Harness::opened();
        let mut billed = Vec::new();
        for line in selfstart_capture().lines() {
            for ev in h.feed(line).0 {
                if let SessionEvent::TokenUsage { turn: Some(t), .. } = ev {
                    billed.push(t);
                }
            }
        }
        assert_eq!(billed.len(), 1, "one turn, one bill: {billed:?}");
        let t = billed[0];
        assert_eq!(t.input, Some(53_677), "the whole turn, not its last call");
        assert_eq!(t.output, Some(292));
        assert_eq!(t.cache_read, Some(46_848));
        // Grok spells the write bucket `cacheCreationTokens` here and
        // `cachedWriteTokens` on the ACP path; a zero it STATED is not the
        // same as a silence.
        assert_eq!(
            t.cache_write,
            Some(0),
            "a zero Grok STATED is not the same as a silence"
        );
        assert_ne!(
            t.input,
            Some(18_431),
            "18,431 is the response `_meta` — the last model call only"
        );
    }

    /// Grok pushes `task_backgrounded` / `task_completed` for work it puts
    /// in the background — verified live; the pair is absent from every
    /// capture in comet and was recorded as UNKNOWN until this probe.
    ///
    /// In THIS capture both land inside the turn, because the backgrounded
    /// command was an `echo` that finished immediately. A long-running one
    /// would outlive `turn_completed`, which is the shape slice CF's
    /// `Unannounced` exists for. eave records both verbatim today and
    /// opens no turn from them — stated here so the gap is a decision
    /// with a test, not an oversight.
    #[test]
    fn grok_pushes_its_background_task_lifecycle_and_eave_records_it() {
        let mut h = Harness::opened();
        let (mut backgrounded, mut completed) = (0, 0);
        for line in selfstart_capture().lines() {
            for ev in h.feed(line).0 {
                if let SessionEvent::System { subtype, .. } = &ev {
                    if subtype.contains("task_backgrounded") {
                        backgrounded += 1;
                    }
                    if subtype.contains("task_completed") {
                        completed += 1;
                    }
                }
            }
        }
        assert!(backgrounded > 0, "the backgrounding push must be recorded");
        assert!(completed > 0, "the completion push must be recorded");
    }

    /// Slice TN1. `--reasoning-effort` is an option of `grok agent`, so
    /// it sits before `stdio` like `-m`; the default passes nothing.
    #[test]
    fn a_model_switch_is_acp_set_model_on_its_own_id() {
        let f = set_model_frame("sess-1", "grok-4.6");
        assert_eq!(f["method"], "session/set_model");
        assert_eq!(f["params"]["sessionId"], "sess-1");
        assert_eq!(f["params"]["modelId"], "grok-4.6");
        // Its reply must never be read as a prompt's, so the id is below
        // the prompt range and is not the handshake's either.
        assert_eq!(f["id"], RPC_SET_MODEL);
        assert!(RPC_SET_MODEL < RPC_PROMPT_BASE);
        assert!(RPC_SET_MODEL != wire::RPC_INITIALIZE && RPC_SET_MODEL != wire::RPC_SESSION);
    }

    #[test]
    fn a_reasoning_effort_is_argv_before_the_subcommand() {
        let mut cfg = SpawnConfig::new("/bin/false", "/tmp", "s".into());
        assert!(!spawn_args(&cfg)
            .iter()
            .any(|a| a == "--reasoning-effort"));
        cfg.tuning.effort = Some("xhigh".into());
        let args = spawn_args(&cfg);
        let e = args
            .iter()
            .position(|a| a == "--reasoning-effort")
            .expect("flag");
        assert_eq!(args[e + 1], "xhigh");
        assert!(e < args.iter().position(|a| a == "stdio").unwrap());
    }
}
