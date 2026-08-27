//! The shared ACP core: one implementation of Agent Client Protocol v1 for
//! every eave adapter whose agent speaks it natively.
//!
//! # Why this exists, and what it deliberately does not cover
//!
//! Three of eave's six adapters — Grok (`grok agent stdio`), Hermes
//! (`hermes acp`) and Cursor (`cursor-agent acp`) — speak the same wire:
//! newline-framed JSON-RPC 2.0 over stdio, `initialize` →
//! `session/new`|`session/load` → `session/prompt`, with `session/update`
//! notifications and `session/request_permission` requests coming back.
//! Grok and Hermes were written independently and ended up with the same
//! protocol implemented twice, down to the sentinel-identity fallback and
//! the three-step kill; Cursor joined them on this core because
//! `cursor-agent acp` is Cursor's own ACP surface (print mode cannot
//! ask — Slice AY).
//!
//! comet reached the same conclusion first and wrote it down. Its
//! `crates/harness/src/lib.rs` header records a REVERSAL: adapter-mediated
//! ACP for Claude, Codex and Cursor was retired because "the adapters held
//! prompt turns open for background work the CLIs themselves settle
//! eagerly, manufacturing done-status bugs the native wires don't have",
//! and the shared `AcpHarness` was kept **only** for agents built
//! ground-up on ACP. eave's `claude.rs` and `codex.rs` are native drivers
//! for exactly that reason and are not part of this core. Neither is
//! `pi.rs`: eave installs a permission gate into an agent that has none,
//! which is not a protocol concern.
//!
//! # What is shared and what is not
//!
//! Shared, because it is the protocol or the process rather than the
//! agent: [`child`] (spawn, supervise, kill), [`wire`] (framing and the
//! five frames eave sends), [`update`] (`session/update` decoding), and
//! [`permission`] (the pending-question table and the option vocabulary).
//!
//! Not shared, because it is genuinely per-agent: **how the posture is
//! proven**. Grok reads a vendor field (`yolo`) off its own notification
//! channel; Hermes reads `modes.currentModeId` off the wire AND runs an
//! out-of-band `hermes approvals test` probe, because its bypasses live in
//! config and environment where no wire can see them; Cursor refuses the
//! flags that stop it asking (`--force`/`--yolo`/`--trust`) at spawn and
//! checks `modes.currentModeId` when the session states one. That is the
//! reason eave cannot collapse the adapters into one state machine the
//! way comet did — comet's ACP agents share a posture policy (auto-accept
//! everything), so it has nothing to keep apart.
//!
//! # The ordering hazards comet documents
//!
//! `comet/docs/research/acp.md:108` records three ordering bugs it had to
//! fix twice, all rooted in `crates/harness/src/jsonrpc.rs:6-10`: responses
//! resolve through a shared pending map while notifications ride a separate
//! `Incoming` channel, so a response can overtake the notifications that
//! preceded it on the pipe.
//!
//! # The session lifecycle eave does NOT implement (slice CL, 2026-08-21)
//!
//! ACP v0.11 exposes six session methods. eave implements two — `new` and
//! `load` — and refused the other four after checking each against three
//! questions: does the wire support it *per agent*, does eave have a
//! caller, and does it fight something eave already decided.
//!
//! The capability matrix, captured live from the installed binaries on
//! 2026-08-21 (see [`handshake::Advertisement`]):
//!
//! | method | grok 1.0.5 | Hermes 0.20.4 | verdict |
//! | --- | --- | --- | --- |
//! | `session/list` | advertised | advertised | refused: no caller |
//! | `session/fork` | **absent** | advertised | refused: no wire, no caller |
//! | `session/resume` | advertised | advertised | refused: no gain over `load` |
//! | `session/close` | advertised | **absent** | refused: no measurable effect |
//!
//! - **`list` and `fork` fight the journal.** eave's inventory of sessions
//!   is its journal (spec 02), not the agent's store. `session/list`
//!   returns grok's own `~/.grok/sessions` — sessions eave never created,
//!   missing sessions eave has. `fork` would mint an agent-side session
//!   whose copied history exists only inside the agent, inverting "the
//!   journal is the history of record". Neither has a caller today, and
//!   grok answers `-32601 Method not found` to `session/fork` outright.
//! - **`resume` buys nothing.** Grok's own build strings gloss it as
//!   "Skipping session replay (session/resume, or a noReplay load)" — it
//!   is `load` without the replay. eave already discards that replay
//!   (`begin_replay`/`end_replay`, because the journal holds it), so the
//!   only effect would be to swap a spec-stable method for one the ACP
//!   schema marks **UNSTABLE** on the single path slice CD had just
//!   repaired. Probed side by side against the same session id, `load` and
//!   `resume` returned the same result and the same four notifications.
//! - **`close` does not close anything eave can observe.** Probed live: it
//!   answers in ~80 ms with `{"x.ai/closeOutcome":"closed"}` and **the
//!   process stays alive**, so it can never stand in for the kill. An A/B
//!   over eave's exact argv (`grok agent --no-leader stdio`) found the
//!   on-disk session identical with and without it, nothing registered in
//!   `active_sessions.json` to leak, and SIGTERM alone sufficient (rc 143,
//!   no SIGKILL) — while close left a stray `relocations/<id>.lock` that a
//!   plain kill does not. Hermes does not implement it at all: its
//!   `acp_adapter/server.py` has no `close_session` handler. So the kill
//!   contract (spec 01 §Kill & verify) is untouched — SIGTERM → SIGKILL →
//!   bounded straggler scan → `KillReport`, with `KillUnproven` reachable
//!   exactly as before.
//!
//! What the slice DID fix is that eave could not answer the first question
//! from its own records: both adapters dropped the entire `initialize`
//! result. See [`handshake::Advertisement`].
//!
//! **eave has one ordered reader per session.** Responses, notifications
//! and server requests are decoded from the same `stdout` line stream, in
//! stdout order, and every resulting event goes down one `mpsc` channel in
//! that order. comet's hazards (a) "flush the channel after `session/load`
//! resolves" and (b) "drain queued updates before emitting Done" cannot
//! arise here — there is no second path to drain. Hazard (c), EOF right
//! after a final response, is handled without a grace window: the reader
//! settles the handshake explicitly at EOF rather than letting a waiter
//! block forever.

pub mod child;
pub mod handshake;
pub mod permission;
pub mod update;
pub mod usage;
pub mod wire;

pub use child::{AcpChild, AcpLines, ChildContext, ChildEnv, ChildSpec, EVENT_CHANNEL_CAP};
pub use handshake::{advertisement, Advertisement, Handshake, HandshakeError, HandshakeGate};
pub use permission::{PendingApprovals, PendingPermission};
pub use update::{AcpStream, Thoughts, ToolInput, ToolNaming, UpdateSpec};

use crate::session::SpawnConfig;

/// Everything about an ACP agent that the shared core needs to know.
///
/// The argv, the environment policy and the protocol quirks — that is the
/// whole of the agent-specific surface the core consumes. The *posture
/// evidence* is not duplicated here: it already lives as
/// [`crate::provider::AgentAdapter::POSTURE`], which the registry reads, so
/// restating it would be two facts that can disagree.
pub struct AcpSpec {
    /// eave's provider id — the registry key, the catalog key, and the stem
    /// of the `EAVE_PROVIDER_BIN_*` override variable that
    /// [`crate::provider::program_override_var`] builds.
    pub provider_id: &'static str,
    /// Argv that puts the CLI into ACP-serving mode. A function of the
    /// session config because the two agents place their flags
    /// differently: Grok's model option belongs to `grok agent`, before
    /// the `stdio` subcommand, while `hermes acp` takes no model flag at
    /// all.
    pub acp_args: fn(&SpawnConfig) -> Vec<String>,
    /// The child's environment overlay: eave's own variables removed, plus
    /// whatever else this agent's bypasses ride in on.
    pub child_env: fn(&SpawnConfig) -> ChildEnv,
    /// Notification methods carrying a `params.update` payload.
    ///
    /// Standard ACP has one. Grok has two, and they are not
    /// interchangeable: `tool_call` arrives on `session/update` while
    /// `turn_completed` arrives on `_x.ai/session_notification` (capture
    /// 2026-08-20), so reading only the standard channel loses the end of
    /// every turn.
    pub update_channels: &'static [&'static str],
    /// Per-agent knobs of the shared `session/update` decoder.
    pub update: UpdateSpec,
    /// Postures that provably ASK.
    pub asking_modes: &'static [&'static str],
}

impl AcpSpec {
    pub fn carries_updates(&self, method: &str) -> bool {
        self.update_channels.contains(&method)
    }
}

/// The provider's own session id to resume, or `None` for a fresh session.
///
/// **One place, deliberately.** Both ACP agents mint their own ids at
/// `session/new` and ACP offers no way to propose one, so the id eave
/// resumes with has to be the one the PROVIDER issued — not
/// `SpawnConfig::session_id`, which is eave's own uuid and an id neither
/// agent has ever heard of. Slice CD is moving `SpawnConfig::resume` from
/// `bool` to `Option<String>` (the last journalled `Init { session_id }`)
/// to close that; when it lands, this function is the only line that
/// moves.
pub fn resume_cursor(cfg: &SpawnConfig) -> Option<String> {
    // Slice CD: the cursor IS `cfg.resume` now. Both agents here mint their
    // own id at `session/new` and ACP offers no way to propose one, so
    // `cfg.session_id` — the id eave would like — never reaches this wire
    // and must never be handed to `session/load`. The old spelling,
    // `cfg.resume.then(|| cfg.session_id.clone())`, asked these agents to
    // load a session they had never created.
    cfg.resume.clone()
}

/// What eave concluded about a stated permission posture.
///
/// **Fail-closed by construction.** Everything that is not provably an
/// asking posture is refused — including a posture this build has never
/// heard of, and a session that stated none. The question "does this
/// review writes?" has no safe default, and a wrong guess here is
/// unreviewed writes on someone's machine (spec 04 §Permissions).
///
/// The verdict is shared; the *sentence* is not. Each adapter writes its
/// own refusal prose because the fix differs — clearing
/// `~/.grok/config.toml` is not the same advice as changing an ACP session
/// mode — and a refusal that does not say how to fix it wastes the
/// operator's time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeVerdict {
    /// The posture is on the list that provably asks.
    Asks,
    /// A posture eave cannot prove asks.
    Unproven(String),
    /// No posture was stated at all.
    Unstated,
}

pub fn classify_mode(asking_modes: &[&str], mode: Option<&str>) -> ModeVerdict {
    match mode {
        Some(m) if asking_modes.contains(&m) => ModeVerdict::Asks,
        Some(m) => ModeVerdict::Unproven(m.to_string()),
        None => ModeVerdict::Unstated,
    }
}
