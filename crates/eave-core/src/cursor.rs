//! Native Cursor adapter: spawn `cursor-agent acp`, speak ACP JSON-RPC
//! over its pipes, own its process group, kill it with proof of exit.
//!
//! # Why ACP and not print mode
//!
//! Slice AY (`docs/reports/stage-ay-cursor-adapter.md`) established that
//! `cursor-agent -p --output-format stream-json` cannot route a permission
//! question to eave: the CLI writes both halves of `interaction_query`
//! itself, and shell/file tools simply run. That surface stays dark.
//!
//! `cursor-agent acp` is Cursor's own ACP v1 agent — the surface MonoCode
//! drives (`monocode/src/lib/harness/cursor.ts`). It holds a session open
//! across turns (`session/new` then `session/prompt`) and it sends
//! `session/request_permission` to the client before a write. That is the
//! same shape as Grok and Hermes, so this adapter sits on the shared ACP
//! core rather than cloning Claude's stream-JSON parser.
//!
//! comet retired *adapter-mediated* ACP for Cursor (wrapping print mode
//! through a generic harness). This is not that: the binary itself serves
//! ACP, the way `grok agent stdio` and `hermes acp` do.
//!
//! # Permission posture
//!
//! eave never passes `--force`, `--yolo`, `--trust`, `--approve-mcps` or
//! `--sandbox` — Slice AY recovered those as the flags that stop Cursor
//! asking. They are refused again at spawn if they arrive as
//! `extra_args`, because `session::conflicts_with_eave_flags` names
//! Claude's flags and would miss them.
//!
//! If `session/new` reports `modes.currentModeId`, that value is checked
//! against [`ASKING_MODES`] the same way Hermes is. A missing mode is not
//! a refusal: Cursor's ACP session does not always carry a mode axis, and
//! inventing one from print-mode's hardcoded `permissionMode: "default"`
//! is the trap Slice AY exists to name.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
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

const CURSOR: AcpSpec = AcpSpec {
    provider_id: "cursor-agent",
    acp_args: spawn_args,
    child_env: |cfg| ChildEnv::sanitized().and_extra(&cfg.extra_env),
    update_channels: &["session/update"],
    update: UpdateSpec {
        tool_naming: ToolNaming::ProtocolKind,
        tool_input: ToolInput::RawInput,
        thoughts: Thoughts::Dropped,
        progress_events: false,
        accumulate_turn_text: false,
    },
    asking_modes: ASKING_MODES,
};

/// Session modes that provably ask, when Cursor states one at all.
/// `default` is Hermes' and Claude's asking word; `ask` is the obvious
/// synonym. Anything else, including one this build has never heard of,
/// is refused — fail-closed, same rule as the other adapters. A session
/// that states no mode is not this list's problem.
const ASKING_MODES: &[&str] = &["default", "ask"];

/// First JSON-RPC id handed to a `session/prompt`. Above the handshake
/// ids this file owns.
const RPC_PROMPT_BASE: i64 = 16;
/// `authenticate` after `initialize`. Best-effort: MonoCode sends
/// `cursor_login` and ignores a failure, because a logged-in CLI still
/// opens a session without it.
const RPC_AUTH: i64 = 3;
/// `session/set_model` after `session/new`, when the spawn named one.
const RPC_SET_MODEL: i64 = 4;

#[derive(Default)]
pub struct CursorProto {
    session_id: Option<String>,
    stream: AcpStream,
    pending: PendingApprovals,
    announced: bool,
    next_prompt_id: i64,
    /// Native `--model` / `session/set_model` id, if the spawn named one.
    /// Carried here because `cursor-agent acp` takes no `--model` on argv
    /// (MonoCode sets it as protocol after the session exists).
    model: Option<String>,
}

impl CursorProto {
    fn new(model: Option<String>) -> Self {
        Self {
            next_prompt_id: RPC_PROMPT_BASE,
            model,
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

pub struct CursorSession {
    child: AcpChild,
    shared: Arc<Shared>,
}

struct Shared {
    proto: Mutex<CursorProto>,
    ready: HandshakeGate,
}

impl CursorSession {
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        if let Some(bad) = posture_undoing_arg(&cfg.extra_args) {
            return Err(SessionError::Spawn(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "agent flag {bad:?} would undo the permission posture eave spawns Cursor \
                     under (`cursor-agent acp` with no --force/--yolo/--trust); eave will not \
                     run a session whose writes it cannot review"
                ),
            )));
        }

        let shared = Arc::new(Shared {
            proto: Mutex::new(CursorProto::new(cfg.model.clone())),
            ready: HandshakeGate::new(),
        });

        let resume_id = acp::resume_cursor(cfg);
        let session_cwd = cfg.cwd.display().to_string();
        let reader_shared = Arc::clone(&shared);

        let (child, events_rx) = AcpChild::spawn(
            ChildSpec {
                program: cfg.program.clone(),
                cwd: cfg.cwd.clone(),
                args: (CURSOR.acp_args)(cfg),
                env: (CURSOR.child_env)(cfg),
            },
            move |mut lines, ctx, tx| async move {
                wire::try_write(&ctx.stdin, &initialize_frame()).await;
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
                        let _ = tx.send(event).await;
                    }
                }
                reader_shared.ready.settle_if_pending(
                    "the cursor-agent process ended before it opened an ACP session, so eave has \
                     nothing to send a prompt to",
                );
            },
        )?;

        Ok((Self { child, shared }, events_rx))
    }

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

/// `cursor-agent acp`. Model is protocol, not argv — see [`CursorProto`].
fn spawn_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut args = vec!["acp".to_string()];
    args.extend(cfg.extra_args.iter().cloned());
    args
}

/// Flags Slice AY recovered as the ones that stop Cursor asking, plus
/// print-mode itself (`-p`/`--print`) so a session cannot be talked onto
/// the surface that writes both halves of an approval.
pub fn posture_undoing_arg(args: &[String]) -> Option<String> {
    for arg in args {
        let head = arg.split('=').next().unwrap_or(arg);
        if matches!(
            head,
            "--force"
                | "--yolo"
                | "--trust"
                | "--approve-mcps"
                | "--auto-review"
                | "--sandbox"
                | "--print"
                | "-p"
        ) {
            return Some(arg.clone());
        }
    }
    None
}

fn initialize_frame() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": wire::RPC_INITIALIZE,
        "method": "initialize",
        "params": {
            "protocolVersion": wire::PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
                "_meta": { "parameterizedModelPicker": true },
            },
            "clientInfo": { "name": "eave", "version": "0" },
        },
    })
}

fn authenticate_frame() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": RPC_AUTH,
        "method": "authenticate",
        "params": { "methodId": "cursor_login" },
    })
}

fn set_model_frame(session_id: &str, model: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": RPC_SET_MODEL,
        "method": "session/set_model",
        "params": { "sessionId": session_id, "modelId": model },
    })
}

fn cancel_frame(session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id },
    })
}

fn result_frame(rpc_id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": rpc_id, "result": result })
}

#[async_trait]
impl crate::provider::AgentAdapter for CursorSession {
    const PROVIDER_ID: &'static str = CURSOR.provider_id;

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "cursor.rs: spawn is `cursor-agent acp` (MonoCode's permission-routing \
                   surface; Slice AY's print mode cannot ask and is not used). `--force` / \
                   `--yolo` / `--trust` / `--approve-mcps` / `--sandbox` / `--print` are \
                   refused at spawn (`posture_undoing_arg`). `session/request_permission` is \
                   the review channel. If `session/new` reports `modes.currentModeId`, it is \
                   checked against ASKING_MODES and anything else is PostureRefused.",
    };

    const REWIND: crate::provider::RewindSupport = crate::provider::RewindSupport::None {
        reason: "Cursor's ACP agent has no conversation rewind. The vendored ACP library routes \
                 a `session/fork`, but cursor-agent never implements the handler it dispatches \
                 to, so the call answers methodNotFound — checked 2026-08-31, and a count of \
                 the string alone would have read as support.",
    };

    const TUNING: crate::tuning::TuningSupport = crate::tuning::TuningSupport {
        effort: crate::tuning::EffortSupport::None {
            reason: "cursor-agent takes effort INSIDE the model id (`--help` 2026.08.11: \
                     `claude-opus-4-8[context=1m,effort=high,fast=false]`), and the words each \
                     model accepts are only listed by `--list-models`, which answered \
                     \"Authentication required\" on this machine 2026-09-02. Not offered until \
                     measured — the mechanics are a `session/set_model` resend.",
        },
        speed: crate::tuning::SpeedSupport::None {
            reason: "same seam and the same unmeasured list as the effort level: `fast=` inside \
                     the model id, listable only after `agent login`.",
        },
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        CursorSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for CursorSession {
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
                ApprovalDecision::Allow => match pending.option_of_kind("allow_once") {
                    Some(option) => (pending.rpc_id.clone(), Some(option)),
                    None => return Ok(false),
                },
                ApprovalDecision::Deny { .. } => (
                    pending.rpc_id.clone(),
                    pending.option_of_kind("reject_once"),
                ),
            }
        };
        permission::deliver(self.stdin(), &rpc_id, option_id.as_deref()).await?;
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
        let session_id = match self.shared.proto.lock().unwrap().session_id() {
            Some(id) => id.to_string(),
            None => return Ok(()),
        };
        wire::write_frame(self.stdin(), &cancel_frame(&session_id)).await
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

impl CursorProto {
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
                    let reason = format!("cursor-agent rejected the ACP handshake: {err}");
                    ready.refuse(reason);
                    return (
                        vec![SessionEvent::System {
                            subtype: "cursor_initialize_failed".into(),
                            raw: line.to_string(),
                        }],
                        Vec::new(),
                    );
                }
                let advert = acp::advertisement(v.get("result")).record();
                (vec![advert], vec![authenticate_frame()])
            }
            Some(RPC_AUTH) => {
                // Best-effort, same as MonoCode: a failed login does not
                // stop session/new. A CLI that is not logged in will fail
                // there instead, with a reason about the session.
                if resume_id.is_some() {
                    self.stream.begin_replay();
                }
                (Vec::new(), vec![wire::session_frame(resume_id, cwd)])
            }
            Some(wire::RPC_SESSION) => {
                if let Some(err) = error {
                    let reason = match resume_id {
                        Some(id) => format!(
                            "cursor-agent could not resume session {id:?}: {err}. Cursor issues \
                             its own session ids at session/new; an id eave chose is not one of \
                             them."
                        ),
                        None => format!("cursor-agent could not open a session: {err}"),
                    };
                    ready.refuse(reason);
                    self.stream.end_replay();
                    return (
                        vec![SessionEvent::System {
                            subtype: "cursor_session_failed".into(),
                            raw: line.to_string(),
                        }],
                        Vec::new(),
                    );
                }
                let session_id = v
                    .get("result")
                    .and_then(|r| r.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| resume_id.map(str::to_string));
                let absorbed = self.stream.end_replay();
                let Some(id) = session_id else {
                    let reason = "cursor-agent opened a session without naming it".to_string();
                    ready.refuse(reason.clone());
                    return (vec![wire::malformed(line, reason)], Vec::new());
                };

                let mode = v
                    .get("result")
                    .and_then(|r| r.get("modes"))
                    .and_then(|m| m.get("currentModeId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                self.session_id = Some(id.clone());
                let mut events: Vec<SessionEvent> = absorbed.into_iter().collect();
                events.extend(self.announce(id.clone(), mode.as_deref()));
                if let Some(refusal) = posture_refusal(mode.as_deref()) {
                    let reason = match &refusal {
                        SessionEvent::PostureRefused { reason, .. } => reason.clone(),
                        _ => "cursor-agent opened in a posture eave will not supervise".into(),
                    };
                    events.push(refusal);
                    ready.refuse(reason);
                    return (events, Vec::new());
                }
                ready.ready(id.clone());

                let mut frames = Vec::new();
                if let Some(model) = &self.model {
                    frames.push(set_model_frame(&id, model));
                }
                (events, frames)
            }
            Some(RPC_SET_MODEL) => {
                // A failed model switch leaves the session on Cursor's
                // default rather than killing it. The operator picked a
                // native id; Cursor saying no is a CLI fact, not a spawn
                // failure.
                (Vec::new(), Vec::new())
            }
            _ => (
                crate::acp::usage::from_prompt_response(v.get("result"))
                    .into_iter()
                    .collect(),
                Vec::new(),
            ),
        }
    }

    fn announce(&mut self, session_id: String, mode: Option<&str>) -> Vec<SessionEvent> {
        if self.announced {
            return Vec::new();
        }
        self.announced = true;
        vec![SessionEvent::Init {
            fast_lane: None,
            session_id,
            permission_mode: Some(mode.unwrap_or("acp").to_string()),
            slash_commands: Vec::new(),
        }]
    }

    fn handle_request(
        &mut self,
        method: &str,
        rpc_id: &Value,
        v: &Value,
        line: &str,
    ) -> (Vec<SessionEvent>, Vec<Value>) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "session/request_permission" => {
                (vec![self.note_permission(rpc_id, &params)], Vec::new())
            }
            "cursor/ask_question" => (
                vec![SessionEvent::System {
                    subtype: "cursor_ask_question".into(),
                    raw: line.to_string(),
                }],
                vec![result_frame(
                    rpc_id,
                    json!({
                        "outcome": {
                            "outcome": "skipped",
                            "reason": "eave does not collect answers to Cursor questions"
                        }
                    }),
                )],
            ),
            "cursor/create_plan" => {
                let plan = params
                    .get("plan")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let event = SessionEvent::System {
                    subtype: "cursor_create_plan".into(),
                    raw: if plan.is_empty() {
                        line.to_string()
                    } else {
                        plan
                    },
                };
                (
                    vec![event],
                    vec![result_frame(rpc_id, json!({ "outcome": { "outcome": "accepted" } }))],
                )
            }
            _ => (
                vec![SessionEvent::DialogRefused {
                    request_id: wire::id_key(rpc_id),
                    dialog: method.to_string(),
                }],
                vec![wire::method_not_found_frame(rpc_id, method)],
            ),
        }
    }

    fn note_permission(&mut self, rpc_id: &Value, params: &Value) -> SessionEvent {
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or(Value::Null);
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
        let kind = tool_call
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string();

        self.pending.open(request_id.clone(), rpc_id, &options);

        SessionEvent::ApprovalRequested {
            request_id,
            tool: kind,
            display_name: title.clone(),
            description: permission::approval_description(&tool_call),
            path: permission::approval_path(&tool_call, None),
            input_json: permission::approval_input(&tool_call),
            suggestions_json: permission::suggestions(&options),
        }
    }

    fn handle_notification(&mut self, method: &str, v: &Value, line: &str) -> Vec<SessionEvent> {
        if CURSOR.carries_updates(method) {
            let update = v
                .get("params")
                .and_then(|p| p.get("update"))
                .cloned()
                .unwrap_or(Value::Null);
            return self.handle_update(&update, line);
        }
        vec![SessionEvent::System {
            subtype: method.to_string(),
            raw: line.to_string(),
        }]
    }

    fn handle_update(&mut self, update: &Value, line: &str) -> Vec<SessionEvent> {
        if self.stream.replay_absorbs() {
            return Vec::new();
        }
        match crate::acp::update::kind_of(update) {
            "current_mode_update" => {
                let mode = update
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match posture_refusal(Some(mode)) {
                    Some(refusal) => vec![refusal],
                    None => vec![SessionEvent::System {
                        subtype: "current_mode_update".into(),
                        raw: line.to_string(),
                    }],
                }
            }
            _ => self.stream.decode(&CURSOR.update, update, line),
        }
    }
}

fn posture_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    // No mode on the wire is not a refusal: Cursor's ACP session often
    // states none, and inventing one is the Slice AY trap. A stated mode
    // that is not on ASKING_MODES is.
    let Some(mode) = mode else {
        return None;
    };
    match acp::classify_mode(CURSOR.asking_modes, Some(mode)) {
        ModeVerdict::Asks => None,
        ModeVerdict::Unproven(other) => Some(SessionEvent::PostureRefused {
            mode: other,
            reason: format!(
                "this Cursor session reports permission posture {mode:?}, which eave cannot \
                 prove asks before it writes"
            ),
        }),
        ModeVerdict::Unstated => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::Handshake;

    const SESSION_NEW: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"cursor-sess-1"}}"#;
    const SESSION_NEW_YOLO: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"cursor-sess-1","modes":{"currentModeId":"yolo"}}}"#;
    const INIT_OK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}"#;
    const AUTH_OK: &str = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
    const PERM_REQUEST: &str = r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"cursor-sess-1","toolCall":{"toolCallId":"call-1","kind":"edit","title":"Write `/tmp/x.txt`","rawInput":{"path":"/tmp/x.txt"}},"options":[{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject-once","name":"No","kind":"reject_once"}]}}"#;
    const ASK_QUESTION: &str = r#"{"jsonrpc":"2.0","id":7,"method":"cursor/ask_question","params":{"questions":[]}}"#;

    struct Harness {
        proto: CursorProto,
        gate: HandshakeGate,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                proto: CursorProto::new(None),
                gate: HandshakeGate::new(),
            }
        }

        fn with_model(model: &str) -> Self {
            Self {
                proto: CursorProto::new(Some(model.into())),
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

        fn opened() -> Self {
            let mut h = Self::new();
            h.feed(INIT_OK);
            h.feed(AUTH_OK);
            h.feed(SESSION_NEW);
            h
        }

        fn ready(&self) -> Handshake {
            self.gate.state()
        }
    }

    #[test]
    fn the_handshake_authenticates_then_opens_a_session() {
        let mut h = Harness::new();
        let (events, frames) = h.feed(INIT_OK);
        match events.as_slice() {
            [SessionEvent::System { subtype, .. }] if subtype == "acp_capabilities" => {}
            other => panic!("initialize records capabilities: {other:?}"),
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["method"], "authenticate");
        assert_eq!(frames[0]["params"]["methodId"], "cursor_login");

        let (_, frames) = h.feed(AUTH_OK);
        assert_eq!(frames[0]["method"], "session/new");
        assert_eq!(frames[0]["params"]["cwd"], "/tmp/probe");

        let (events, frames) = h.feed(SESSION_NEW);
        assert!(frames.is_empty(), "no model was configured: {frames:?}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Init { session_id, .. } if session_id == "cursor-sess-1")),
            "{events:?}"
        );
        assert_eq!(h.proto.session_id(), Some("cursor-sess-1"));
        assert_eq!(h.ready(), Handshake::Ready("cursor-sess-1".into()));
    }

    #[test]
    fn a_configured_model_is_set_as_protocol_not_argv() {
        let mut cfg = SpawnConfig::new("/bin/cursor-agent", "/tmp/probe", "sess-1".into());
        cfg.model = Some("composer-2.5".into());
        let args = spawn_args(&cfg);
        assert_eq!(args, vec!["acp"]);
        assert!(!args.iter().any(|a| a == "--model" || a == "composer-2.5"));

        let mut h = Harness::with_model("composer-2.5");
        h.feed(INIT_OK);
        h.feed(AUTH_OK);
        let (_, frames) = h.feed(SESSION_NEW);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["method"], "session/set_model");
        assert_eq!(frames[0]["params"]["modelId"], "composer-2.5");
        assert_eq!(frames[0]["params"]["sessionId"], "cursor-sess-1");
    }

    #[test]
    fn a_resume_asks_cursor_to_load_the_id_it_was_given() {
        let mut h = Harness::new();
        h.feed_resuming(INIT_OK, "cursor-old");
        let (_, frames) = h.feed_resuming(AUTH_OK, "cursor-old");
        assert_eq!(frames[0]["method"], "session/load");
        assert_eq!(frames[0]["params"]["sessionId"], "cursor-old");
    }

    #[test]
    fn a_failed_login_still_opens_a_session() {
        let mut h = Harness::new();
        h.feed(INIT_OK);
        let (_, frames) = h.feed(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"not logged in"}}"#);
        assert_eq!(frames[0]["method"], "session/new");
    }

    #[test]
    fn a_yolo_mode_on_session_new_is_refused() {
        let mut h = Harness::new();
        h.feed(INIT_OK);
        h.feed(AUTH_OK);
        let (events, _) = h.feed(SESSION_NEW_YOLO);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::PostureRefused { mode, .. } if mode == "yolo")),
            "{events:?}"
        );
        assert!(matches!(h.ready(), Handshake::Refused(_)));
    }

    #[test]
    fn a_permission_request_becomes_one_answerable_question() {
        let mut h = Harness::opened();
        let (events, frames) = h.feed(PERM_REQUEST);
        assert!(frames.is_empty(), "{frames:?}");
        match events.as_slice() {
            [SessionEvent::ApprovalRequested {
                request_id, tool, display_name, ..
            }] => {
                assert_eq!(request_id, "call-1");
                assert_eq!(tool, "edit");
                assert_eq!(display_name, "Write `/tmp/x.txt`");
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }
        assert!(h.proto.approval_is_open("call-1"));
        let pending = h.proto.pending.get("call-1").expect("open");
        assert_eq!(
            pending.option_of_kind("allow_once").as_deref(),
            Some("allow-once")
        );
        assert_eq!(
            pending.option_of_kind("reject_once").as_deref(),
            Some("reject-once")
        );
    }

    #[test]
    fn cursor_ask_question_is_skipped_not_method_not_found() {
        let mut h = Harness::opened();
        let (events, frames) = h.feed(ASK_QUESTION);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::System { subtype, .. } if subtype == "cursor_ask_question")),
            "{events:?}"
        );
        assert_eq!(frames[0]["id"], 7);
        assert_eq!(frames[0]["result"]["outcome"]["outcome"], "skipped");
        assert!(frames[0].get("error").is_none());
    }

    #[test]
    fn the_spawn_argv_is_acp_and_never_print_mode() {
        let cfg = SpawnConfig::new("/bin/cursor-agent", "/tmp/probe", "sess-1".into());
        assert_eq!(spawn_args(&cfg), vec!["acp"]);
        assert!(!spawn_args(&cfg).iter().any(|a| a == "-p" || a == "--print"));
        assert!(!spawn_args(&cfg).iter().any(|a| a == "--force" || a == "--yolo"));
    }

    #[test]
    fn extra_args_cannot_smuggle_a_bypass() {
        assert_eq!(
            posture_undoing_arg(&["--verbose".into()]),
            None
        );
        for flag in [
            "--force",
            "--force=true",
            "--yolo",
            "--trust",
            "--approve-mcps",
            "--sandbox",
            "--print",
            "-p",
        ] {
            assert_eq!(
                posture_undoing_arg(&[flag.into()]).as_deref(),
                Some(flag),
                "{flag}"
            );
        }
    }

    #[test]
    fn spawn_refuses_a_bypass_flag_before_a_process_exists() {
        let mut cfg = SpawnConfig::new("/bin/cursor-agent", "/tmp/probe", "sess-1".into());
        cfg.extra_args = vec!["--yolo".into()];
        let err = CursorSession::spawn(&cfg).err().expect("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--yolo"), "{msg}");
        assert!(msg.contains("cannot review"), "{msg}");
    }
}
