//! The `AgentSession` trait: the engine's uniform handle on one running
//! agent-CLI process, regardless of provider (locked decision: internal
//! trait; native impls; ACP is one impl behind it, not the foundation).
//!
//! Spawning is provider-specific and stays on the concrete type (it returns
//! the concrete session plus its event receiver); everything the engine does
//! *after* spawn goes through this trait object.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::proc::ProcIdentity;

/// What agent a SESSION runs, as opposed to what agent the engine
/// defaults to (Slice P2). Carried on the session, persisted in its
/// journal header at open, and therefore the same across a restart and a
/// `--resume` — the engine's own `EAVE_AGENT` default only fills the gaps.
///
/// Every field is optional and every absent field means "the engine's
/// default", so a session opened before this existed reads back as
/// `AgentConfig::default()` and behaves exactly as it did. That is the
/// whole compatibility story: no format version moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Provider family — decides which adapter spawns it and, critically,
    /// **which permission posture proof applies**. `None` = the engine's
    /// configured provider. See [`posture_is_verified`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The CLI binary. `None` = the engine's `agent_program`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Passed verbatim as `--model`. `None` = whatever the CLI defaults to;
    /// eave does not invent a model name, and does not validate this one —
    /// an unknown model is the CLI's error to state, not eave's to guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra CLI flags, verbatim and in order, appended after eave's own.
    /// Deliberately unvalidated and deliberately LAST — see the refusal in
    /// [`conflicts_with_eave_flags`], which is the one thing that is not
    /// allowed through: a flag that would undo eave's permission posture.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

/// Every provider whose permission posture an adapter has declared proven.
///
/// **Derived from the registry, not maintained here.** This used to be a
/// hand-kept `[&str; 1]`, which meant the list of providers the engine
/// would spawn and the list of adapters that exist were two facts that
/// could disagree. They are now one fact: an adapter declares
/// [`crate::provider::PostureProof`] as a required associated const, and
/// this reads it.
///
/// Slice K established what makes the Claude CLI ask before it writes —
/// `--permission-prompt-tool stdio` with no `--permission-mode`, verified
/// against the CLI's own `init.permissionMode` rather than assumed — and
/// every part of that proof is Claude-specific and lives in `claude.rs`.
/// It transfers to no other CLI, which is exactly why each adapter has to
/// state its own.
pub fn posture_verified_providers() -> Vec<&'static str> {
    crate::provider::posture_verified_providers()
}

/// Is this provider's permission posture proven? **Fail-closed by
/// construction**, exactly like Slice K's `ASKING_MODES`: an unrecognised
/// provider is refused, never spawned on the assumption that it behaves
/// like Claude. A provider with no registered adapter is unrecognised; so
/// is one whose adapter declares its own posture unproven.
///
/// This is the door Slice K closed, and the reason it is guarded here
/// rather than at the UI: per-session config makes it possible to name any
/// binary you like, and spawning an unverified one would reintroduce the
/// v0 auto-accept blocker by the side door — a session writing to disk
/// with nothing able to review it.
pub fn posture_is_verified(provider: &str) -> bool {
    crate::provider::adapter(provider).is_some_and(|a| a.posture.is_verified())
}

/// Flags a caller may not smuggle through [`AgentConfig::extra_args`],
/// because they would undo the posture proof rather than extend it.
///
/// `--permission-mode` is the whole of Slice K's finding: supplying ANY
/// value for it stops the CLI asking (capture H — `acceptEdits` produces
/// zero questions), and `--permission-prompt-tool` is the flag that routes
/// questions to eave in the first place. Re-specifying either from config
/// would let a session opt itself out of review while still reporting a
/// verified provider.
pub fn conflicts_with_eave_flags(arg: &str) -> bool {
    let head = arg.split('=').next().unwrap_or(arg);
    matches!(
        head,
        "--permission-mode" | "--permission-prompt-tool" | "--session-id" | "--resume"
    )
}

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// The agent CLI binary. Tests point this at the fake-agent harness.
    pub program: PathBuf,
    pub cwd: PathBuf,
    /// The id eave PROPOSES for a fresh session, so a cursor exists before
    /// the first turn does (research: waku.md §2, driver/claude.rs).
    ///
    /// This is a proposal, not a fact. Claude Code and Pi accept it —
    /// `claude --session-id` and `pi --session-id` both create-under-the-
    /// given-name. The ACP agents do not: Grok and Hermes mint their own id
    /// at `session/new` and Codex mints a thread id, and ACP offers no way
    /// to propose one. For those, this field never reaches the wire.
    ///
    /// **Never use this as a resume cursor.** See [`SpawnConfig::resume`].
    pub session_id: String,
    /// Continue an existing provider session, naming **the provider's own
    /// id for it** — the cursor the provider issued last time, replayed
    /// from the journal's last `Init { session_id }`.
    ///
    /// `None` = start fresh.
    ///
    /// This was a `bool` until 2026-08-20, and the bool was a Claude-shaped
    /// assumption that broke every other provider. Claude resumes by the
    /// id eave named, so `resume: true` plus [`SpawnConfig::session_id`]
    /// was sufficient there and the three adapters that came later copied
    /// the idiom — `cfg.resume.then(|| cfg.session_id.clone())` — into
    /// Grok, Hermes and Codex, none of which have ever heard of that id.
    /// The result was a resume that silently opened a NEW session and lost
    /// the conversation. `Option<String>` makes the broken state
    /// unrepresentable: you cannot ask to resume without saying what.
    ///
    /// `AgentSession::provider_session_id()` is where the cursor comes
    /// from. It existed on every adapter, was asserted by their tests, and
    /// had no consumer anywhere in the engine until this slice.
    pub resume: Option<String>,
    /// Extra environment for the child. The inherited environment is always
    /// sanitized of `EAVE_*` first (spec 01 §Spawn; research: bb.md §1).
    pub extra_env: Vec<(String, String)>,
    /// Resolved from the session's [`AgentConfig`] (Slice P2), not from the
    /// engine. `None` = do not pass `--model` at all, which is not the same
    /// as passing an empty one.
    pub model: Option<String>,
    /// Resolved from the session's [`AgentConfig`], appended after eave's
    /// own flags. Already filtered by [`conflicts_with_eave_flags`] at the
    /// engine boundary — the adapter trusts this list.
    pub extra_args: Vec<String>,
}

impl SpawnConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>, session_id: String) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            session_id,
            resume: None,
            extra_env: Vec::new(),
            model: None,
            extra_args: Vec::new(),
        }
    }
}

/// A cheap, clonable window onto the OS-facts the attention monitor needs
/// every tick (spec 01 §Attention clocks / `failed`): shared flags the
/// adapter's own reader/exit tasks maintain. Deliberately not the session
/// itself — the monitor must never contend with send/kill for the live
/// handle.
#[derive(Clone)]
pub struct SessionWatch {
    pub identity: ProcIdentity,
    stdout_eof: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SessionWatch {
    pub fn new(
        identity: ProcIdentity,
        stdout_eof: std::sync::Arc<std::sync::atomic::AtomicBool>,
        exited: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            identity,
            stdout_eof,
            exited,
        }
    }

    /// The CLI's stdout reader saw EOF (or a read error). With the process
    /// still alive this is provable death of the stream (spec 01 `failed`).
    pub fn stdout_eof(&self) -> bool {
        self.stdout_eof.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The exit waiter has facts; the `Exited → Closed` path owns
    /// everything from here.
    pub fn exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The operator's answer to one permission question (Slice K). There are
/// exactly two — eave never auto-approves, never auto-denies, and never
/// times out into either; a session waits indefinitely on a human, the
/// same principle as never auto-killing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// The message reaches the model verbatim as the tool result, so
    /// "denied — commit first" is a genuinely different instruction from
    /// "denied" (capture D, 2026-08-18).
    Deny {
        message: String,
    },
}

/// What a verified kill established (spec 01 §Kill & verify).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillReport {
    /// The grace period expired and the group was SIGKILLed.
    pub escalated: bool,
    /// The leader had already exited before the kill began.
    pub already_exited: bool,
    /// Pids found by the single bounded `ps` scan and SIGKILLed individually.
    pub stragglers_killed: Vec<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("failed to spawn agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not record (pid, start_time) identity for pid {pid}")]
    IdentityUnavailable { pid: i32 },
    #[error("agent stdin is closed")]
    StdinClosed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Exit could not be proven: the leader survived SIGKILL, or group
    /// members remain after the straggler scan. Never reported as killed
    /// on hope (spec 01 §Kill step 4).
    #[error("kill unproven: surviving pids {remaining:?}")]
    KillUnproven { remaining: Vec<i32> },
}

#[async_trait]
pub trait AgentSession: Send {
    /// The (pid, start_time) identity recorded at spawn.
    fn identity(&self) -> ProcIdentity;

    /// The provider-native resume cursor, once the provider announced it.
    fn provider_session_id(&self) -> Option<String>;

    /// Outstanding provider `tool_use` ids the adapter has seen started
    /// and not yet seen resolved.
    ///
    /// **Tool calls only. It does NOT count open permission questions**
    /// (Slice K), and reconciling that is deliberately not done here:
    ///
    /// - The two are different facts with different consequences. A tool
    ///   in flight means the agent is working; an open question means the
    ///   agent is stopped and the operator is the blocker. Summing them
    ///   into one "busy" number would erase exactly the distinction
    ///   `awaiting_approval` exists to make (spec 01) — and `quiet` vs
    ///   `awaiting_approval` is the difference between offering a kill
    ///   and not.
    /// - The doc comment here used to claim this was "the 'nothing in
    ///   flight' input to the attention clocks". **It is not, and never
    ///   was.** `eave_core::attention::Classifier` folds journal records
    ///   and owns every in-flight notion the clocks use; nothing outside
    ///   tests calls this method at all. The claim was the actual bug —
    ///   a reader who believed it would wire this in and silently
    ///   un-hold the approval carve-out.
    ///
    /// What the wire actually does today, checked rather than assumed
    /// (`docs/research/approvals-capture-2026-08-18/`, captures C, D, E):
    /// every `can_use_tool` request arrives AFTER that tool's own
    /// `tool_use` content block, so a session parked on a permission
    /// question already reports at least one tool in flight and the two
    /// counts happen to agree. That agreement is a property of the CLI's
    /// ordering, not of eave, and nothing enforces it —
    /// `request_user_dialog` (AskUserQuestion, plan mode) is a control
    /// request with no tool anywhere in it.
    ///
    /// So: this is an adapter-level count of one specific thing, and the
    /// rule for any future caller is stated rather than encoded. If you
    /// need "is this session doing anything at all?", you need this AND
    /// [`AgentSession::pending_approvals`], and you must not add them —
    /// today they would double-count the same tool.
    fn in_flight_tools(&self) -> usize;

    /// Last byte received from the CLI (attention-clock activity source).
    fn last_activity(&self) -> Instant;

    /// Clonable OS-facts window for the attention monitor (spec 01).
    fn watch(&self) -> SessionWatch;

    /// Send one user message into the running session.
    async fn send_user_message(&mut self, text: &str) -> Result<(), SessionError>;

    /// Permission questions this session is parked on right now (Slice K).
    fn pending_approvals(&self) -> usize;

    /// Answer one permission question. `Ok(false)` = the question was not
    /// open (already answered, or withdrawn under the operator's click) —
    /// a race, stated as a fact, never an error and never journaled as if
    /// it had landed.
    async fn respond_to_approval(
        &mut self,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, SessionError>;

    /// Kill with proof of exit: group SIGTERM → grace → group SIGKILL →
    /// group probe under the identity guard → one bounded straggler scan.
    /// `Err(KillUnproven)` when exit cannot be proven (spec 01 §Kill).
    async fn kill(&mut self) -> Result<KillReport, SessionError>;
}
