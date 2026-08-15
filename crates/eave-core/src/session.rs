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

use crate::proc::ProcIdentity;

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// The agent CLI binary. Tests point this at the fake-agent harness.
    pub program: PathBuf,
    pub cwd: PathBuf,
    /// Caller-chosen provider session id, so the resume cursor exists before
    /// the first turn does (research: waku.md §2, driver/claude.rs).
    pub session_id: String,
    /// Continue an existing provider session (`--resume`) instead of naming
    /// a new one (`--session-id`).
    pub resume: bool,
    /// Extra environment for the child. The inherited environment is always
    /// sanitized of `EAVE_*` first (spec 01 §Spawn; research: bb.md §1).
    pub extra_env: Vec<(String, String)>,
}

impl SpawnConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>, session_id: String) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            session_id,
            resume: false,
            extra_env: Vec::new(),
        }
    }
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

    /// Outstanding tool calls — the "nothing in flight" input to the
    /// attention clocks (spec 01).
    fn in_flight_tools(&self) -> usize;

    /// Last byte received from the CLI (attention-clock activity source).
    fn last_activity(&self) -> Instant;

    /// Send one user message into the running session.
    async fn send_user_message(&mut self, text: &str) -> Result<(), SessionError>;

    /// Kill with proof of exit: group SIGTERM → grace → group SIGKILL →
    /// group probe under the identity guard → one bounded straggler scan.
    /// `Err(KillUnproven)` when exit cannot be proven (spec 01 §Kill).
    async fn kill(&mut self) -> Result<KillReport, SessionError>;
}
