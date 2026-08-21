//! The turn model: what a turn is, and **what will close it**.
//!
//! One state machine with two owners. The engine's
//! [`crate::attention::Classifier`] folds it to run the attention clocks;
//! the app folds the same records to caption the working trailer. Before
//! this module those were two hand-written copies of the same arithmetic
//! (`attention.rs`'s `turn_balance` and `ui.rs`'s twin field), which is the
//! shape that lets two correct-looking edits disagree about whether a
//! session is working.
//!
//! # The settle contract
//!
//! comet solves the same problem with `Harness::deterministic_turn_end() ->
//! bool` (`comet/crates/harness/src/lib.rs:70-79`), consumed at
//! `comet/crates/engine/src/sessions.rs:1368` to decide whether to arm a
//! 120 s quiesce watchdog — plus a *second*, 20 s watchdog at
//! `sessions.rs:1391` for turns the agent starts on its own, plus two env
//! knobs to override both.
//!
//! That second watchdog is the tell. **`deterministic_turn_end()` is a
//! property of the harness, but determinism is a property of the turn
//! shape.** comet's own comment says so: the boolean returns `true` for
//! claude native, and then a turn the agent begins on its own "can never
//! receive a harness Done", so the boolean is already wrong for one of two
//! turn shapes on every harness that answers `true`. Each new turn shape
//! costs another constant, another knob and another `if`.
//!
//! So the property lives on the **turn**, recorded when it opens
//! ([`TurnStart`]) and mapped to its contract in exactly one place
//! ([`TurnStart::settle`]). eave needs no `deterministic_turn_end()` at all:
//! nothing is asked of any adapter, because the turn shape is observable
//! from the event stream eave already journals verbatim.
//!
//! # The three shapes, and the evidence for each
//!
//! Slice CG's survey (`docs/research/turn-start-evidence.md`) settled this
//! from real captures rather than from reasoning, and it produced a THIRD
//! shape neither the brief nor comet had. All three were confirmed by
//! replaying `fixtures/claude-background-subagent-2.1.228.jsonl` (comet's
//! live CLI 2.1.228 capture) through eave's own `ClaudeStream` — see
//! `tests/background_subagent_replay.rs`:
//!
//! | shape | what opens it | in that capture |
//! | --- | --- | --- |
//! | [`TurnStart::Prompted`] | eave delivered a prompt | the operator's turn, closed by the `result` at line 60 |
//! | [`TurnStart::Announced`] | the provider announced a turn of its own | the second `system/init` at line 78, same `session_id`, closed by the `result` at line 119 |
//! | [`TurnStart::Unannounced`] | agent work simply appeared | lines 63-73 — a subagent's `Bash`, an `ApprovalRequested`, and an `AssistantText`, after the operator's turn ended and before the wake turn was announced. **No terminal frame covers them.** |
//!
//! **comet's 20 s self-turn watchdog is deliberately not ported.** CG found
//! comet contradicts itself — `sessions.rs:1385` says a self-started turn
//! "can never receive a harness Done" while `claude/mod.rs:387-390` and
//! `codex/mod.rs:294-297` return `deterministic_turn_end() -> true` with
//! "Done is the CLI's own terminal frame, **for wake turns too**". The
//! watchdog was built for the adapter-mediated ACP lane comet then retired;
//! eave's claude and codex are native, which is the lane comet came back to.
//! Announced turns are therefore `Promised`, and eave waits for their frame.
//!
//! **comet's `RESUME_GATE` (`sessions.rs:1766-1790`) is not ported either,
//! and this is a place eave is ahead.** comet detects a self-continued turn
//! by *timing* — fresh output more than a second after parking — because its
//! normalizer throws the second `init` away (`claude/normalize.rs:296-298`
//! returns early once `saw_init`). eave keeps the frame, so eave keys on the
//! frame instead of on a stopwatch.
//!
//! # No new clock
//!
//! An unpromised turn settles on the clock eave already runs:
//! `QUIET_AFTER` elapsed with nothing wire-marked in flight, which is
//! exactly the premise of `AttentionSnapshot::Quiet { reason:
//! WaitReason::Idle }`. There is no duration constant in this file. Nothing
//! here acts, either — settling a turn is a status change, never a kill
//! (spec 01 rejects timers that act; the reap ceiling was removed
//! 2026-08-15).

use crate::events::SessionEvent;
use crate::journal::{JournalEvent, Record};

/// How this turn began. Recorded when it opens; never asked of the
/// provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStart {
    /// The operator asked, and eave delivered the prompt.
    Prompted,
    /// The provider announced a turn of its own on the wire — for claude, a
    /// second `system/init` carrying the `session_id` this segment already
    /// knows (`docs/research/turn-start-evidence.md` §1; capture line 78).
    Announced,
    /// Agent work simply appeared with nothing announcing a turn: a
    /// subagent's tool call, its permission question, its prose. Confirmed
    /// at capture lines 63-73, between the operator's `result` and the wake
    /// turn's announcement.
    Unannounced,
}

/// What will close this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSettle {
    /// A terminal frame is owed: something on the wire announced this turn
    /// and will terminate it. eave waits for that frame and **never times
    /// out**.
    Promised,
    /// Nobody announced it, so nobody owes anything and no terminal frame is
    /// coming. Settles on quiescence.
    Unpromised,
}

impl TurnStart {
    /// The single place a turn shape's settle contract is decided. A fourth
    /// shape cannot be added without answering here — which is the whole
    /// difference from a boolean on the adapter, where a new shape silently
    /// inherits whatever the provider last claimed.
    pub fn settle(self) -> TurnSettle {
        match self {
            // Both ends are on the wire: eave delivered the prompt, or the
            // provider announced the turn. CG verified the wake turn's
            // terminal `result` at capture line 119.
            TurnStart::Prompted | TurnStart::Announced => TurnSettle::Promised,
            TurnStart::Unannounced => TurnSettle::Unpromised,
        }
    }
}

/// What folding one record meant to the classifier that owns the ledger.
/// Returned rather than re-derived, so the record match lives in exactly
/// one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    /// Engine-side bookkeeping or a synthetic closer — not a CLI byte, so
    /// no clock resets.
    Silent,
    /// A byte from the CLI.
    Activity,
    /// A byte from the CLI that also changed the open-question set.
    Approvals,
}

/// The provider frames that MEAN the agent produced work of its own.
///
/// Deliberately not "any byte": opening an unpromised turn puts a sentence
/// on the operator's screen, so only frames that unambiguously carry agent
/// output earn it. A `status` heartbeat, a token-usage statement or an
/// unparseable line is not the agent working.
///
/// **Provider-neutral by construction.** [`SessionEvent`] is already
/// normalised across the five adapters, so this is a table over frame
/// kinds, never a branch per provider — which is what makes the model hold
/// even if a given CLI's self-started work is indistinguishable from
/// ordinary tool traffic. It does not need to be distinguishable: whether a
/// turn was open is an eave-side fact, and that is the whole question.
///
/// Exhaustive on purpose — a new [`SessionEvent`] variant must come here
/// and decide.
fn marks_agent_work(event: &SessionEvent) -> bool {
    match event {
        SessionEvent::TextDelta { .. }
        | SessionEvent::AssistantText { .. }
        | SessionEvent::ToolUseStarted { .. }
        | SessionEvent::ApprovalRequested { .. } => true,
        SessionEvent::Init { .. }
        | SessionEvent::ToolUseFinished { .. }
        | SessionEvent::UserEcho { .. }
        | SessionEvent::TurnCompleted { .. }
        | SessionEvent::System { .. }
        | SessionEvent::ApprovalWithdrawn { .. }
        | SessionEvent::DialogRefused { .. }
        | SessionEvent::PostureRefused { .. }
        | SessionEvent::TokenUsage { .. }
        | SessionEvent::Malformed { .. }
        | SessionEvent::Unknown { .. }
        | SessionEvent::Exited { .. } => false,
    }
}

/// The open turn, what is in flight inside it, and when it last heard
/// anything. Pure: no timers, no process handles, no clock of its own —
/// every method that needs "now" is handed it.
#[derive(Debug, Clone)]
pub struct TurnLedger {
    /// Open PROMPTED turns: `UserDelivered` = +1, a terminal frame = −1.
    ///
    /// A signed balance (floored at −1, capped at 8) instead of a bool
    /// because the recorder's two input channels are unordered: a fast CLI
    /// can get its `TurnCompleted` journaled *before* the `UserDelivered`
    /// that opened the turn, and a bool would leave a phantom turn open
    /// forever (a ~40 % flake, caught by the idle-between-turns acceptance
    /// test 2026-08-16). With the balance, the pair nets to closed in
    /// either order.
    prompted: i32,
    /// `UserSent` seen, its `UserDelivered` not yet. **The guard that keeps
    /// the same channel race from manufacturing a phantom UNPROMISED turn**:
    /// in the reordered case the whole turn's output is journaled while
    /// `prompted` is still 0, and every one of those frames marks agent
    /// work. eave has a prompt outstanding, so that work is owed to the
    /// prompt, not begun by the agent.
    sent_unconfirmed: i32,
    /// When the operator asked, from the `UserSent` record's own timestamp
    /// — so a session opened mid-turn reports the turn's real age and not
    /// the age of the subscription.
    prompted_started_ms: Option<u64>,
    /// When the provider announced a turn of its own, `None` when it has
    /// not — claude's second `system/init` (capture line 78).
    announced_since_ms: Option<u64>,
    /// When unannounced agent work started, `None` when none is open.
    unannounced_since_ms: Option<u64>,
    /// The `session_id` the FIRST `Init` of this process segment carried.
    ///
    /// A later `Init` under the same id is the provider announcing a turn;
    /// a later `Init` under a DIFFERENT id is another session multiplexed
    /// onto this stream and must never promote itself into a turn (comet's
    /// genus gate, `comet/crates/proto/src/agent.rs:203`). Cleared by
    /// `Closed`, so a resumed segment's own first `Init` is a process
    /// saying hello and not a turn.
    segment_session: Option<String>,
    /// Outstanding tool calls, oldest first: (id, name, since_ms).
    outstanding: Vec<(String, String, u64)>,
    /// `status: requesting` seen; cleared by any assistant output.
    api_since: Option<u64>,
    /// Permission questions the operator has not answered, oldest first:
    /// (request_id, tool, since_ms).
    approvals: Vec<(String, String, u64)>,
    last_activity_ms: u64,
}

impl TurnLedger {
    pub fn new(now_ms: u64) -> Self {
        Self {
            prompted: 0,
            sent_unconfirmed: 0,
            prompted_started_ms: None,
            announced_since_ms: None,
            unannounced_since_ms: None,
            segment_session: None,
            outstanding: Vec::new(),
            api_since: None,
            approvals: Vec::new(),
            last_activity_ms: now_ms,
        }
    }

    /// The turn that is open right now, and how it began.
    ///
    /// Only one is ever set at a time: a delivered prompt clears both
    /// agent-started stamps (it owns the turn from there), and an
    /// announcement clears the unannounced stamp (it upgrades the ambient
    /// work into a turn with a frame owed). The order here is the
    /// precedence anyway, so the accessor is total either way.
    pub fn open(&self) -> Option<TurnStart> {
        if self.prompted > 0 {
            Some(TurnStart::Prompted)
        } else if self.announced_since_ms.is_some() {
            Some(TurnStart::Announced)
        } else if self.unannounced_since_ms.is_some() {
            Some(TurnStart::Unannounced)
        } else {
            None
        }
    }

    pub fn in_turn(&self) -> bool {
        self.open().is_some()
    }

    /// When the open turn began, in journal ms. `None` when nothing is open,
    /// or when a promised turn's `UserSent` was never seen — the indicator
    /// still states that something is running, but no start time is ever
    /// invented from when a reader happened to attach.
    pub fn started_ms(&self) -> Option<u64> {
        match self.open()? {
            TurnStart::Prompted => self.prompted_started_ms,
            TurnStart::Announced => self.announced_since_ms,
            TurnStart::Unannounced => self.unannounced_since_ms,
        }
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms
    }

    pub fn oldest_tool(&self) -> Option<(&str, u64)> {
        self.outstanding
            .first()
            .map(|(_, name, since)| (name.as_str(), *since))
    }

    pub fn tools_outstanding(&self) -> bool {
        !self.outstanding.is_empty()
    }

    pub fn api_since(&self) -> Option<u64> {
        self.api_since
    }

    pub fn approvals(&self) -> &[(String, String, u64)] {
        &self.approvals
    }

    /// Restart the silence clock without folding a record — the one case is
    /// leaving `AwaitingApproval`, where an approval that took an hour must
    /// not instantly trip `QUIET_AFTER` on the way out.
    pub fn touch(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    /// Settle the open turn if its own contract says it is over. Returns
    /// whether a turn closed.
    ///
    /// `quiet_after_ms` is the caller's existing `QUIET_AFTER`
    /// ([`crate::thresholds::AttentionThresholds`]) — this module defines no
    /// duration of its own, and a new one here would be comet's watchdog
    /// shape leaking back in.
    pub fn settle(&mut self, now_ms: u64, quiet_after_ms: u64) -> bool {
        match self.open().map(TurnStart::settle) {
            None => false,
            // The provider owes a terminal frame. eave waits for it,
            // indefinitely: manufacturing a Done is the done-status bug
            // class comet's `lib.rs` header records retiring adapters over.
            Some(TurnSettle::Promised) => false,
            // No frame is coming, so quiescence is the settle. Same premise
            // as `WaitReason::Idle`: silent past QUIET_AFTER with nothing
            // wire-marked in flight — an unresolved tool is a command still
            // running, an open question is a human being waited on, and an
            // outstanding API request is a round-trip in progress.
            Some(TurnSettle::Unpromised) => {
                let quiet = self.outstanding.is_empty()
                    && self.approvals.is_empty()
                    && self.api_since.is_none()
                    && now_ms.saturating_sub(self.last_activity_ms) >= quiet_after_ms;
                if quiet {
                    self.unannounced_since_ms = None;
                }
                quiet
            }
        }
    }

    /// Fold one journal record.
    ///
    /// `now_ms` is when this record *happened* in the caller's frame: the
    /// wall clock for the live engine, the record's own `ts_ms` for a reader
    /// replaying a journal. Both are epoch ms, so a replaying reader's
    /// clocks read against `journal::now_ms()` exactly as the engine's do.
    pub fn on_record(&mut self, rec: &Record, now_ms: u64) -> Fold {
        // Decided BEFORE the fold, because the fold is what records a
        // segment's first `Init` — and a segment's first announcement is the
        // process saying hello, not a turn.
        let opened = self.agent_opener(rec);
        let fold = self.fold(rec, now_ms);
        if matches!(fold, Fold::Activity | Fold::Approvals) {
            self.last_activity_ms = now_ms;
        }
        match opened {
            None => {}
            // Only the two agent-started shapes are minted here; `Prompted`
            // is opened by `UserDelivered` inside the fold, because a
            // delivery is eave's own fact and not something read off a frame.
            Some(TurnStart::Prompted) => {}
            Some(TurnStart::Announced) => {
                self.announced_since_ms = Some(now_ms);
                // The announcement supersedes whatever ambient work was
                // running: from here a terminal frame is owed.
                self.unannounced_since_ms = None;
            }
            Some(TurnStart::Unannounced) => self.unannounced_since_ms = Some(now_ms),
        }
        fold
    }

    /// Does this record open a turn the AGENT began?
    ///
    /// Pure, and read against the state as it stood BEFORE the record was
    /// folded. Nothing is asked of the adapter here and no provider is named
    /// — the two rules are over the normalised [`SessionEvent`] every
    /// adapter already emits.
    fn agent_opener(&self, rec: &Record) -> Option<TurnStart> {
        // eave has a prompt outstanding: whatever the agent is doing is owed
        // to that prompt, not begun on its own. This is the guard that keeps
        // the recorder's channel race (see `sent_unconfirmed`) from minting
        // a phantom turn out of an ordinary reply.
        if self.sent_unconfirmed > 0 {
            return None;
        }
        let JournalEvent::Provider { event } = &rec.event else {
            return None;
        };
        match event {
            SessionEvent::Init { session_id, .. } => {
                // An announcement may open a turn from nothing, and may also
                // UPGRADE ambient unannounced work — positive evidence that a
                // frame is now owed, which is the only direction the contract
                // may ever move. It never overrides a live prompted turn.
                let announceable = matches!(self.open(), None | Some(TurnStart::Unannounced));
                (announceable && self.segment_session.as_deref() == Some(session_id.as_str()))
                    .then_some(TurnStart::Announced)
            }
            event => {
                (self.open().is_none() && marks_agent_work(event)).then_some(TurnStart::Unannounced)
            }
        }
    }

    fn fold(&mut self, rec: &Record, now_ms: u64) -> Fold {
        match &rec.event {
            // Engine-side attempt: not a CLI byte; the clock starts at the
            // delivery confirmation below.
            JournalEvent::UserSent { .. } => {
                self.prompted_started_ms = Some(rec.ts_ms);
                self.sent_unconfirmed = (self.sent_unconfirmed + 1).min(8);
                Fold::Silent
            }
            JournalEvent::UserDelivered {} => {
                self.prompted = (self.prompted + 1).min(8);
                self.sent_unconfirmed = (self.sent_unconfirmed - 1).max(0);
                // The prompt takes the turn over: whatever the agent had
                // begun on its own is now part of what the operator asked
                // for, and the provider owes ONE frame for the whole thing.
                //
                // Unverified either way — no capture shows a prompt landing
                // mid-wake-turn, so nothing establishes whether claude then
                // emits one `result` or two (CG unknown #8's neighbourhood).
                // Absorbing is the conservative reading: two frames net this
                // to −1, which the floor absorbs exactly as it absorbs the
                // channel race, whereas keeping both stamps would strand an
                // announced turn waiting for a frame that already came.
                self.announced_since_ms = None;
                self.unannounced_since_ms = None;
                Fold::Activity
            }
            JournalEvent::Provider { event } => match event {
                SessionEvent::TurnCompleted { .. } => {
                    // **The frame closes the turn that was actually open.**
                    //
                    // It used to decrement unconditionally, and that is the
                    // bug CG measured: claude's background-subagent wake
                    // emits a SECOND `result` for a turn the operator never
                    // started, so one prompt produced two decrements, the
                    // balance floored at −1, and the operator's NEXT turn ran
                    // at 0 — `in_turn()` false, attention clocks disarmed and
                    // no working trailer, on a turn that was genuinely
                    // running (`turn-start-evidence.md` §1; capture lines 60
                    // and 119).
                    match self.open() {
                        // `None` = the frame preceded its own UserDelivered
                        // (channel race, see `prompted`); −1 pre-cancels it.
                        Some(TurnStart::Prompted) | None => {
                            self.prompted = (self.prompted - 1).max(-1);
                        }
                        // The frame belongs to the AGENT's turn, so the
                        // prompted balance must not move. Decrementing here
                        // is the whole bug.
                        Some(TurnStart::Announced | TurnStart::Unannounced) => {}
                    }
                    // Whichever was open, the provider says it is over.
                    self.announced_since_ms = None;
                    self.unannounced_since_ms = None;
                    self.outstanding.clear();
                    self.api_since = None;
                    // A turn cannot end with a question still open; if the
                    // provider says it did, believe the provider.
                    self.approvals.clear();
                    Fold::Approvals
                }
                SessionEvent::ToolUseStarted { id, name, .. } => {
                    // Keyed by id, not appended blindly: an adapter may
                    // restate a start once its arguments arrive (the Claude
                    // CLI announces a `tool_use` with an empty `input` and
                    // fills it in a line later), and one tool counted twice
                    // would hold `QUIET_AFTER` off forever. The FIRST
                    // sighting keeps its clock — that is when the tool began.
                    if !self.outstanding.iter().any(|(tid, _, _)| tid == id) {
                        self.outstanding.push((id.clone(), name.clone(), now_ms));
                    }
                    // A tool call is response output: any pending request
                    // has been answered.
                    self.api_since = None;
                    Fold::Activity
                }
                SessionEvent::ToolUseFinished { id, .. } => {
                    self.outstanding.retain(|(tid, _, _)| tid != id);
                    Fold::Activity
                }
                SessionEvent::ApprovalRequested {
                    request_id, tool, ..
                } => {
                    self.api_since = None;
                    if !self.approvals.iter().any(|(id, _, _)| id == request_id) {
                        self.approvals
                            .push((request_id.clone(), tool.clone(), now_ms));
                    }
                    Fold::Approvals
                }
                SessionEvent::ApprovalWithdrawn { request_id, .. } => {
                    self.approvals.retain(|(id, _, _)| id != request_id);
                    Fold::Approvals
                }
                SessionEvent::TextDelta { .. } | SessionEvent::AssistantText { .. } => {
                    self.api_since = None;
                    Fold::Activity
                }
                SessionEvent::Init { session_id, .. } => {
                    // The first announcement of a segment NAMES the session.
                    // Every later one is read against that name by
                    // `agent_opener`, which has already run.
                    if self.segment_session.is_none() {
                        self.segment_session = Some(session_id.clone());
                    }
                    Fold::Activity
                }
                SessionEvent::System { subtype, raw } => {
                    if subtype == "status" && status_is_requesting(raw) {
                        self.api_since = Some(now_ms);
                    }
                    Fold::Activity
                }
                // UserEcho, TokenUsage, DialogRefused, PostureRefused,
                // Malformed, Unknown, (Exited never lands here — the
                // recorder folds it into Closed): all bytes.
                _ => Fold::Activity,
            },
            // Repair/terminal records only appear in replay, which the
            // engine's monitor never classifies over; settle everything
            // anyway.
            JournalEvent::ApprovalDecided { request_id, .. } => {
                self.approvals.retain(|(id, _, _)| id != request_id);
                Fold::Approvals
            }
            JournalEvent::ToolInterrupted { id } => {
                self.outstanding.retain(|(tid, _, _)| tid != id);
                Fold::Silent
            }
            // A segment ended: whatever was open died with the process.
            // **eave needs no watchdog for this and comet does** — the
            // journal records the death, so nothing has to be inferred from
            // silence.
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. } => {
                self.prompted = 0;
                self.sent_unconfirmed = 0;
                self.announced_since_ms = None;
                self.unannounced_since_ms = None;
                self.outstanding.clear();
                self.api_since = None;
                self.approvals.clear();
                // Only a `Closed` ends the PROCESS, and only a process
                // boundary retires the announced session name: a resumed
                // segment's own first `Init` must read as hello, not as a
                // turn. `TurnInterrupted` is a turn boundary inside one
                // live process and leaves the name standing.
                if matches!(rec.event, JournalEvent::Closed { .. }) {
                    self.segment_session = None;
                }
                Fold::Silent
            }
        }
    }
}

/// `{"type":"system","subtype":"status","status":"requesting"}` — the wire
/// mark for "API request in flight" (claude-stream-notes.md).
fn status_is_requesting(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(|s| s.as_str().map(|s| s == "requesting"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: u64 = 120_000;

    fn rec(seq: u64, ts_ms: u64, event: JournalEvent) -> Record {
        Record { seq, ts_ms, event }
    }

    fn provider(ts_ms: u64, event: SessionEvent) -> Record {
        rec(1, ts_ms, JournalEvent::Provider { event })
    }

    fn text(ts_ms: u64) -> Record {
        provider(ts_ms, SessionEvent::TextDelta { text: "x".into() })
    }

    fn completed(ts_ms: u64) -> Record {
        provider(
            ts_ms,
            SessionEvent::TurnCompleted {
                subtype: "success".into(),
                is_error: false,
            },
        )
    }

    /// The shape that had no turn at all before this slice: the agent wakes
    /// itself, works, and goes quiet. Opening is what makes the work
    /// visible; quiescence is what closes it, on eave's existing clock.
    #[test]
    fn unannounced_agent_work_opens_a_turn_and_quiesces_out_of_it() {
        let mut l = TurnLedger::new(0);
        l.on_record(&text(1_000), 1_000);
        assert_eq!(l.open(), Some(TurnStart::Unannounced));
        assert_eq!(l.started_ms(), Some(1_000));
        // Not one millisecond early.
        assert!(!l.settle(1_000 + QUIET - 1, QUIET));
        assert_eq!(l.open(), Some(TurnStart::Unannounced));
        assert!(l.settle(1_000 + QUIET, QUIET));
        assert_eq!(l.open(), None);
    }

    /// The other half of the contract, and the one comet's boolean cannot
    /// express: a promised turn NEVER settles on a clock, however long the
    /// silence runs. Ten hours of it.
    #[test]
    fn a_prompted_turn_never_settles_on_the_clock() {
        let mut l = TurnLedger::new(0);
        l.on_record(&rec(1, 0, JournalEvent::UserSent { text: "go".into() }), 0);
        l.on_record(&rec(2, 10, JournalEvent::UserDelivered {}), 10);
        assert_eq!(l.open(), Some(TurnStart::Prompted));
        for t in (0..36_000_000).step_by(QUIET as usize) {
            assert!(!l.settle(t, QUIET), "a promised turn timed out at {t}ms");
        }
        assert_eq!(l.open(), Some(TurnStart::Prompted));
        l.on_record(&completed(36_000_000), 36_000_000);
        assert_eq!(l.open(), None);
    }

    /// The channel race, from the other side. In the reordered journal the
    /// whole turn's output lands while `promised` is still 0, and every one
    /// of those frames marks agent work — without the `UserSent` guard each
    /// one would mint a phantom unpromised turn that outlives the turn it
    /// belonged to.
    #[test]
    fn a_reordered_delivery_pair_never_mints_an_agent_started_turn() {
        let mut l = TurnLedger::new(0);
        l.on_record(&rec(1, 0, JournalEvent::UserSent { text: "go".into() }), 0);
        l.on_record(&text(10), 10);
        assert_eq!(
            l.open(),
            None,
            "output under an outstanding send is owed to that send"
        );
        l.on_record(&completed(20), 20);
        l.on_record(&rec(4, 30, JournalEvent::UserDelivered {}), 30);
        assert_eq!(l.open(), None, "the reordered pair must net to closed");
        assert!(!l.settle(10_000_000, QUIET));
    }

    fn init(ts_ms: u64, session_id: &str) -> Record {
        provider(
            ts_ms,
            SessionEvent::Init {
                session_id: session_id.into(),
                permission_mode: Some("default".into()),
                slash_commands: Vec::new(),
            },
        )
    }

    /// A segment's FIRST announcement is the process saying hello; a second
    /// one under the same name is the provider opening a turn of its own.
    /// The unit form of what the capture proves.
    #[test]
    fn a_second_init_under_the_known_session_announces_a_turn() {
        let mut l = TurnLedger::new(0);
        l.on_record(&init(0, "s1"), 0);
        assert_eq!(l.open(), None, "the first init is hello, not a turn");
        l.on_record(
            &rec(2, 10, JournalEvent::UserSent { text: "go".into() }),
            10,
        );
        l.on_record(&rec(3, 20, JournalEvent::UserDelivered {}), 20);
        l.on_record(&completed(30), 30);
        assert_eq!(l.open(), None);
        l.on_record(&init(40, "s1"), 40);
        assert_eq!(l.open(), Some(TurnStart::Announced));
        assert_eq!(l.started_ms(), Some(40));
        // Promised: a terminal frame is owed and eave waits for it, not for
        // silence. (comet's 20s self-turn watchdog would have parked this.)
        assert!(!l.settle(40 + QUIET * 100, QUIET));
        l.on_record(&completed(50), 50);
        assert_eq!(l.open(), None);
    }

    /// The genus gate: an `init` naming a session this segment has never
    /// seen is another session multiplexed onto the stream, and must never
    /// promote itself into a turn.
    #[test]
    fn an_init_for_a_foreign_session_announces_nothing() {
        let mut l = TurnLedger::new(0);
        l.on_record(&init(0, "s1"), 0);
        l.on_record(&init(10, "somebody-elses-session"), 10);
        assert_eq!(l.open(), None);
    }

    /// A resumed segment opens a new process, which says hello again. That
    /// `init` is not a turn — the `Closed` between them retires the name.
    #[test]
    fn a_resumed_segments_first_init_is_hello_and_not_a_turn() {
        let mut l = TurnLedger::new(0);
        l.on_record(&init(0, "s1"), 0);
        l.on_record(
            &rec(
                2,
                10,
                JournalEvent::Closed {
                    outcome: crate::journal::Outcome::Interrupted,
                },
            ),
            10,
        );
        l.on_record(&init(20, "s1"), 20);
        assert_eq!(
            l.open(),
            None,
            "cold resume was captioned as the agent starting a turn"
        );
        // And the segment renames itself, so ITS second init still announces.
        l.on_record(&init(30, "s1"), 30);
        assert_eq!(l.open(), Some(TurnStart::Announced));
    }

    /// An announcement UPGRADES ambient unannounced work: it is positive
    /// evidence that a terminal frame is now owed. This is the capture's own
    /// order — subagent traffic at line 63, the wake announcement at 78.
    #[test]
    fn an_announcement_upgrades_open_unannounced_work() {
        let mut l = TurnLedger::new(0);
        l.on_record(&init(0, "s1"), 0);
        l.on_record(&text(1_000), 1_000);
        assert_eq!(l.open(), Some(TurnStart::Unannounced));
        l.on_record(&init(2_000, "s1"), 2_000);
        assert_eq!(l.open(), Some(TurnStart::Announced));
        assert_eq!(l.started_ms(), Some(2_000), "the new turn's own clock");
        assert!(
            !l.settle(2_000 + QUIET * 100, QUIET),
            "an upgraded turn waits for its frame"
        );
    }

    /// A prompt lands on top of self-started work: the promised contract
    /// takes over, and the turn now waits for the provider's frame rather
    /// than for silence.
    #[test]
    fn a_prompt_takes_over_an_agent_started_turn() {
        let mut l = TurnLedger::new(0);
        l.on_record(&text(1_000), 1_000);
        assert_eq!(l.open(), Some(TurnStart::Unannounced));
        l.on_record(
            &rec(
                2,
                2_000,
                JournalEvent::UserSent {
                    text: "stop".into(),
                },
            ),
            2_000,
        );
        l.on_record(&rec(3, 2_010, JournalEvent::UserDelivered {}), 2_010);
        assert_eq!(l.open(), Some(TurnStart::Prompted));
        assert_eq!(l.started_ms(), Some(2_000), "the clock is the prompt's now");
        assert!(!l.settle(10_000_000, QUIET));
        // And when it completes, the absorbed unpromised turn does not
        // come back.
        l.on_record(&completed(3_000), 3_000);
        assert_eq!(l.open(), None);
    }

    /// Nothing in flight is the gate, exactly as `WaitReason::Idle` has it:
    /// a tool still running, a question on a human, or an API round trip
    /// each hold an unpromised turn open past the window.
    #[test]
    fn in_flight_work_holds_an_unannounced_turn_open() {
        for (label, hold, release) in [
            (
                "tool",
                provider(
                    1_000,
                    SessionEvent::ToolUseStarted {
                        id: "t1".into(),
                        name: "Bash".into(),
                        input: String::new(),
                        call: None,
                    },
                ),
                provider(
                    1_000,
                    SessionEvent::ToolUseFinished {
                        id: "t1".into(),
                        result: String::new(),
                        is_error: false,
                    },
                ),
            ),
            (
                "approval",
                provider(
                    1_000,
                    SessionEvent::ApprovalRequested {
                        request_id: "r1".into(),
                        tool: "Write".into(),
                        display_name: "Write".into(),
                        description: String::new(),
                        path: None,
                        input_json: "{}".into(),
                        suggestions_json: None,
                    },
                ),
                provider(
                    1_000,
                    SessionEvent::ApprovalWithdrawn {
                        request_id: "r1".into(),
                        reason: "gone".into(),
                    },
                ),
            ),
            (
                "api",
                provider(
                    1_000,
                    SessionEvent::System {
                        subtype: "status".into(),
                        raw: r#"{"status":"requesting"}"#.into(),
                    },
                ),
                text(1_000),
            ),
        ] {
            let mut l = TurnLedger::new(0);
            l.on_record(&text(500), 500);
            l.on_record(&hold, 1_000);
            assert_eq!(l.open(), Some(TurnStart::Unannounced), "{label}");
            assert!(
                !l.settle(1_000 + QUIET * 100, QUIET),
                "{label} in flight must hold the turn open"
            );
            l.on_record(&release, 2_000);
            assert!(l.settle(2_000 + QUIET, QUIET), "{label} released");
        }
    }

    /// A dead process needs no watchdog here: the journal records the
    /// death, so the turn closes on a fact rather than on an inference from
    /// silence (this is the row where comet needs a watchdog and eave does
    /// not).
    #[test]
    fn a_closed_segment_zeroes_both_turn_shapes() {
        for opener in [
            vec![
                rec(1, 0, JournalEvent::UserSent { text: "go".into() }),
                rec(2, 10, JournalEvent::UserDelivered {}),
            ],
            vec![text(10)],
        ] {
            let mut l = TurnLedger::new(0);
            for r in &opener {
                l.on_record(r, r.ts_ms);
            }
            assert!(l.in_turn());
            l.on_record(
                &rec(
                    9,
                    100,
                    JournalEvent::Closed {
                        outcome: crate::journal::Outcome::Interrupted,
                    },
                ),
                100,
            );
            assert_eq!(l.open(), None);
        }
    }

    /// Noise is not work. A journal full of status heartbeats, token
    /// statements and lines this build cannot parse must not caption the
    /// session as working on its own.
    #[test]
    fn provider_noise_never_opens_a_turn() {
        let noise = [
            SessionEvent::Init {
                session_id: "s".into(),
                permission_mode: None,
                slash_commands: Vec::new(),
            },
            SessionEvent::System {
                subtype: "status".into(),
                raw: r#"{"status":"requesting"}"#.into(),
            },
            SessionEvent::UserEcho { text: "go".into() },
            SessionEvent::TokenUsage {
                turn: None,
                context: None,
            },
            SessionEvent::Malformed {
                line: "{".into(),
                error: "eof".into(),
            },
            SessionEvent::Unknown {
                event_type: "rate_limit_event".into(),
                raw: "{}".into(),
            },
            SessionEvent::ToolUseFinished {
                id: "orphan".into(),
                result: String::new(),
                is_error: false,
            },
        ];
        for event in noise {
            let mut l = TurnLedger::new(0);
            let label = format!("{event:?}");
            l.on_record(&provider(1_000, event), 1_000);
            assert_eq!(l.open(), None, "{label} opened a turn");
        }
    }

    /// Every existing record shape still projects to the turn state it
    /// projected to before this slice (spec 02: the journal replays in full
    /// on cold start). The user-turn shape is the whole of what eave used
    /// to model, and it is unchanged, step for step.
    #[test]
    fn a_journal_written_before_this_slice_still_projects_the_same_turn() {
        let mut l = TurnLedger::new(0);
        let steps: Vec<(Record, bool)> = vec![
            (
                rec(1, 0, JournalEvent::UserSent { text: "go".into() }),
                false,
            ),
            (rec(2, 10, JournalEvent::UserDelivered {}), true),
            (
                provider(
                    20,
                    SessionEvent::Init {
                        session_id: "s".into(),
                        permission_mode: Some("default".into()),
                        slash_commands: Vec::new(),
                    },
                ),
                true,
            ),
            (text(30), true),
            (
                provider(
                    40,
                    SessionEvent::ToolUseStarted {
                        id: "t1".into(),
                        name: "Bash".into(),
                        input: String::new(),
                        call: None,
                    },
                ),
                true,
            ),
            (
                provider(
                    50,
                    SessionEvent::ToolUseFinished {
                        id: "t1".into(),
                        result: "ok".into(),
                        is_error: false,
                    },
                ),
                true,
            ),
            (completed(60), false),
        ];
        for (r, want_open) in &steps {
            l.on_record(r, r.ts_ms);
            assert_eq!(
                l.in_turn(),
                *want_open,
                "seq {} projected the wrong turn state",
                r.seq
            );
        }
        // And hours of the CLI lingering on stdin afterwards stay closed.
        assert!(!l.settle(7_200_000, QUIET));
        assert_eq!(l.open(), None);
    }
}
