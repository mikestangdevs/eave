//! Attention classification (spec 01 §States, §Attention clocks).
//!
//! A pure state machine over journal records + injected clocks + CPU-sample
//! verdicts — no timers, no process handles, so every transition is unit-
//! testable at synthetic time. The engine drives one [`Classifier`] per
//! live session and broadcasts each transition to subscribers as a
//! transient `attention` frame (spec 05); nothing here is journaled —
//! attention is derived, reversible surface state, not history.
//!
//! The clocks are wire-fact-aware, never silence-aware (spec 01; research:
//! claude-stream-notes.md): the CLI lingers after `turn_completed` awaiting
//! stdin, so **idle between turns is normal** — no clock runs unless a turn
//! is open (`UserDelivered` seen, `TurnCompleted` not yet). In-flight state
//! is read off the wire: `system/status {status: requesting}` marks an API
//! request, `tool_use`/`tool_result` (confirmed by the `task_started`/
//! `task_notification` bracket) mark tool execution. CPU sampling is only
//! the tiebreaker that hardens `quiet` into `unresponsive`.

use serde::{Deserialize, Serialize};

use crate::events::SessionEvent;
use crate::journal::{JournalEvent, Record};
use crate::thresholds::AttentionThresholds;

/// What a `quiet` session is waiting on — the three cases the UI must keep
/// distinguishable (build directive 2026-08-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitReason {
    /// A tool call is outstanding (crossed `TOOL_EXEC_AFTER`).
    Tool { name: String, since_ms: u64 },
    /// An API request is in flight (`status: requesting` seen, no response
    /// bytes yet).
    Api { since_ms: u64 },
    /// Mid-turn with nothing wire-marked in flight and no output.
    Idle,
}

/// The transient attention overlay on a live session. `Running` means "no
/// notice" — it is broadcast so a prior notice provably clears.
///
/// Slice K note on why `AwaitingApproval` is a STATE and not another
/// [`WaitReason`]: every `WaitReason` lives inside `Quiet`, and `Quiet`
/// means "alive and ambiguous — here is a kill affordance". A session
/// parked on a permission question is neither ambiguous nor a candidate
/// for killing; the only thing wrong with it is that nobody has clicked
/// yet. Filing it under `Quiet` would offer to kill a session that is
/// working exactly as designed, which is the specific failure the slice
/// was told to prevent. It is also immediate rather than clock-gated: the
/// operator learns they are being waited on at once, not two minutes
/// later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttentionSnapshot {
    Running,
    /// Alive and ambiguous. Escalating notice only; persists indefinitely;
    /// never a kill (spec 01).
    Quiet {
        since_ms: u64,
        reason: WaitReason,
    },
    /// Alive but demonstrably doing nothing: zero CPU across N consecutive
    /// samples with nothing in flight. Evidence shown, kill affordance
    /// offered, **never auto-killed** (spec 01).
    Unresponsive {
        since_ms: u64,
        zero_samples: u32,
        sampled_ms: u64,
    },
    /// The session is parked on the operator: one or more permission
    /// questions are open and the agent cannot proceed until a human
    /// answers (Slice K). No clock runs in this state — not `QUIET_AFTER`,
    /// not `TOOL_EXEC_AFTER` — and `unresponsive` is unreachable from it,
    /// because a process waiting on a person legitimately burns no CPU
    /// (the same reasoning that already exempts an in-flight API request).
    /// The session waits indefinitely; eave never decides for the
    /// operator.
    AwaitingApproval {
        since_ms: u64,
        /// How many questions are open. The rail states the count; the
        /// questions themselves live on the session's own transcript
        /// stream (watch isolation invariant, spec 05 §Watch).
        pending: u64,
        /// The tool named by the oldest open question.
        tool: String,
    },
    /// Provable death from OS facts — immediate, no timer (spec 01). The
    /// journal's terminal truth still comes from the recorder; this is the
    /// surface overlay carrying the concrete cause.
    Failed {
        since_ms: u64,
        cause: String,
    },
}

impl AttentionSnapshot {
    pub fn state_name(&self) -> &'static str {
        match self {
            AttentionSnapshot::Running => "running",
            AttentionSnapshot::Quiet { .. } => "quiet",
            AttentionSnapshot::Unresponsive { .. } => "unresponsive",
            AttentionSnapshot::AwaitingApproval { .. } => "awaiting_approval",
            AttentionSnapshot::Failed { .. } => "failed",
        }
    }
}

// ---------------------------------------------------------------------------
// OS-facts probe (the `failed` inputs)

/// One tick's OS facts about the live child, assembled by the engine from
/// `proc::pid_state` + the adapter's [`crate::session::SessionWatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFacts {
    /// The exit waiter has facts — the normal `Exited → Closed` path owns
    /// everything from here; never classify over it.
    pub exited: bool,
    /// `pid_state` said Gone, or Live with a start_time mismatch.
    pub vanished: bool,
    pub zombie: bool,
    /// ppid is still the engine (only meaningful while Live).
    pub parent_ok: bool,
    /// The CLI's stdout reached EOF while the process is not exited.
    pub stdout_eof: bool,
    pub in_turn: bool,
}

/// Classify one tick's facts into a provable-death cause, or `None`.
/// Pure so every branch is unit-testable; the engine requires the same
/// verdict on two consecutive ticks before acting, because a normal exit
/// races these facts (stdout EOF and pid teardown land moments before the
/// exit waiter fires).
pub fn probe_failure(f: &ProbeFacts) -> Option<String> {
    if f.exited {
        return None;
    }
    if f.zombie {
        return Some("process exited but was never reaped (zombie)".into());
    }
    if f.vanished {
        return Some("process vanished (pid gone or reused under us)".into());
    }
    if !f.parent_ok {
        return Some("broken parent link (child reparented away from the engine)".into());
    }
    if f.stdout_eof {
        return Some(if f.in_turn {
            "stdout closed mid-turn while the process is alive".into()
        } else {
            "stdout closed while the process is alive".into()
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Classifier

#[derive(Debug)]
pub struct Classifier {
    th: AttentionThresholds,
    /// Open-turn balance: `UserDelivered` = +1, `TurnCompleted` = −1; a
    /// turn is open while > 0 — the only window in which any clock runs.
    /// A signed balance (floored at −1, capped at 8) instead of a bool
    /// because the recorder's two input channels are unordered: a fast CLI
    /// can get its `TurnCompleted` journaled *before* the `UserDelivered`
    /// that opened the turn, and a bool would leave a phantom turn open
    /// forever (real flake, caught by the idle-between-turns acceptance
    /// test). With the balance, the pair nets to closed in either order.
    turn_balance: i32,
    /// Outstanding tool calls, oldest first: (id, name, since_ms).
    outstanding: Vec<(String, String, u64)>,
    /// `status: requesting` seen; cleared by any assistant output.
    api_since: Option<u64>,
    /// Permission questions the operator has not answered, oldest first:
    /// (request_id, tool, since_ms). While this is non-empty the session
    /// is parked on a human and no clock runs (Slice K).
    pending_approvals: Vec<(String, String, u64)>,
    last_activity_ms: u64,
    state: AttentionSnapshot,
    zero_streak: u32,
}

impl Classifier {
    pub fn new(th: AttentionThresholds, now_ms: u64) -> Self {
        Self {
            th,
            turn_balance: 0,
            outstanding: Vec::new(),
            api_since: None,
            pending_approvals: Vec::new(),
            last_activity_ms: now_ms,
            state: AttentionSnapshot::Running,
            zero_streak: 0,
        }
    }

    pub fn snapshot(&self) -> AttentionSnapshot {
        self.state.clone()
    }

    pub fn in_turn(&self) -> bool {
        self.turn_balance > 0
    }

    /// CPU sampling is armed only where it is the tiebreaker: quiet with
    /// nothing wire-marked in flight, or already unresponsive (to notice
    /// recovery). Sampling a busy or tool-running session proves nothing.
    pub fn wants_cpu_sampling(&self) -> bool {
        // Never while parked on the operator (Slice K): a process waiting
        // on a person burns no CPU by design, so sampling it would prove
        // nothing and could only mislead.
        if !self.pending_approvals.is_empty() {
            return false;
        }
        matches!(
            &self.state,
            AttentionSnapshot::Quiet {
                reason: WaitReason::Idle,
                ..
            } | AttentionSnapshot::Unresponsive { .. }
        )
    }

    /// Any byte/state-change from the CLI: reset the clock; a live notice
    /// clears back to running (spec 01 transition table). `Failed` is
    /// sticky — provable death is not undone by a stale descendant line.
    ///
    /// Slice K: while questions are open, activity does NOT clear the
    /// notice — the agent chatting on does not mean the operator answered.
    /// The parked state is owned by the pending set, and
    /// [`Self::settle_approvals`] is the only thing that leaves it.
    fn activity(&mut self, now_ms: u64) -> Option<AttentionSnapshot> {
        self.last_activity_ms = now_ms;
        self.zero_streak = 0;
        if !self.pending_approvals.is_empty() {
            return None;
        }
        match self.state {
            AttentionSnapshot::Quiet { .. } | AttentionSnapshot::Unresponsive { .. } => {
                self.state = AttentionSnapshot::Running;
                Some(self.state.clone())
            }
            _ => None,
        }
    }

    /// Re-derive the parked state from the pending set. Entering is
    /// immediate (the operator is being waited on the moment the question
    /// lands); leaving restores `Running` and restarts the clocks from
    /// now, so an approval that took an hour does not instantly trip
    /// `QUIET_AFTER` on the way out.
    fn settle_approvals(&mut self, now_ms: u64) -> Option<AttentionSnapshot> {
        match self.pending_approvals.first() {
            Some((_, tool, since_ms)) => {
                let want = AttentionSnapshot::AwaitingApproval {
                    since_ms: *since_ms,
                    pending: self.pending_approvals.len() as u64,
                    tool: tool.clone(),
                };
                if self.state == want {
                    return None;
                }
                self.state = want;
                Some(self.state.clone())
            }
            None => {
                if matches!(self.state, AttentionSnapshot::AwaitingApproval { .. }) {
                    self.last_activity_ms = now_ms;
                    self.zero_streak = 0;
                    self.state = AttentionSnapshot::Running;
                    Some(self.state.clone())
                } else {
                    None
                }
            }
        }
    }

    /// Fold one journal record. Returns a snapshot when the state changed.
    pub fn on_record(&mut self, rec: &Record, now_ms: u64) -> Option<AttentionSnapshot> {
        match &rec.event {
            // Engine-side attempt: not a CLI byte; the clock starts at the
            // delivery confirmation below.
            JournalEvent::UserSent { .. } => None,
            JournalEvent::UserDelivered {} => {
                self.turn_balance = (self.turn_balance + 1).min(8);
                self.activity(now_ms)
            }
            JournalEvent::Provider { event } => match event {
                SessionEvent::TurnCompleted { .. } => {
                    // May precede its own UserDelivered (channel race, see
                    // `turn_balance`) — going to −1 pre-cancels it.
                    self.turn_balance = (self.turn_balance - 1).max(-1);
                    self.outstanding.clear();
                    self.api_since = None;
                    // A turn cannot end with a question still open; if the
                    // provider says it did, believe the provider.
                    self.pending_approvals.clear();
                    self.activity(now_ms);
                    self.settle_approvals(now_ms)
                }
                SessionEvent::ToolUseStarted { id, name, .. } => {
                    self.outstanding.push((id.clone(), name.clone(), now_ms));
                    // A tool call is response output: any pending request
                    // has been answered.
                    self.api_since = None;
                    self.activity(now_ms)
                }
                SessionEvent::ToolUseFinished { id, .. } => {
                    self.outstanding.retain(|(tid, _, _)| tid != id);
                    self.activity(now_ms)
                }
                SessionEvent::ApprovalRequested {
                    request_id, tool, ..
                } => {
                    self.api_since = None;
                    self.activity(now_ms);
                    if !self
                        .pending_approvals
                        .iter()
                        .any(|(id, _, _)| id == request_id)
                    {
                        self.pending_approvals
                            .push((request_id.clone(), tool.clone(), now_ms));
                    }
                    self.settle_approvals(now_ms)
                }
                SessionEvent::ApprovalWithdrawn { request_id, .. } => {
                    self.pending_approvals.retain(|(id, _, _)| id != request_id);
                    self.activity(now_ms);
                    self.settle_approvals(now_ms)
                }
                SessionEvent::TextDelta { .. } | SessionEvent::AssistantText { .. } => {
                    self.api_since = None;
                    self.activity(now_ms)
                }
                SessionEvent::System { subtype, raw } => {
                    if subtype == "status" && status_is_requesting(raw) {
                        self.api_since = Some(now_ms);
                    }
                    self.activity(now_ms)
                }
                // Init, UserEcho, Malformed, Unknown, (Exited never lands
                // here — the recorder folds it into Closed): all bytes.
                _ => self.activity(now_ms),
            },
            // Repair/terminal records only appear in replay, which the
            // monitor never classifies over; settle everything anyway.
            JournalEvent::ApprovalDecided { request_id, .. } => {
                self.pending_approvals.retain(|(id, _, _)| id != request_id);
                self.activity(now_ms);
                self.settle_approvals(now_ms)
            }
            JournalEvent::ToolInterrupted { id } => {
                self.outstanding.retain(|(tid, _, _)| tid != id);
                None
            }
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. } => {
                self.turn_balance = 0;
                self.outstanding.clear();
                self.api_since = None;
                self.pending_approvals.clear();
                None
            }
        }
    }

    /// Clock check. Returns a snapshot when a threshold crossing changes
    /// the state.
    pub fn on_tick(&mut self, now_ms: u64) -> Option<AttentionSnapshot> {
        if matches!(self.state, AttentionSnapshot::Failed { .. }) {
            return None;
        }
        // Idle between turns is NORMAL — the CLI lingers on stdin after
        // turn_completed (claude-stream-notes.md). No open turn, no clock.
        if self.turn_balance <= 0 {
            return None;
        }
        // Slice K: a pending human approval holds BOTH clocks (spec 01
        // §Attention clocks). This is the carve-out that keeps a session
        // held across someone's lunch break from being reported as
        // ambiguous and offered a kill.
        if !self.pending_approvals.is_empty() {
            return None;
        }
        let desired: Option<WaitReason> = if let Some((_, name, since)) = self
            .outstanding
            .first()
            .filter(|(_, _, since)| now_ms.saturating_sub(*since) >= self.th.tool_exec_after_ms)
        {
            Some(WaitReason::Tool {
                name: name.clone(),
                since_ms: *since,
            })
        } else if self.outstanding.is_empty()
            && now_ms.saturating_sub(self.last_activity_ms) >= self.th.quiet_after_ms
        {
            // While a tool is outstanding (but under its own threshold) the
            // quiet clock does NOT run (spec 01) — hence the is_empty gate.
            Some(match self.api_since {
                Some(since_ms) => WaitReason::Api { since_ms },
                None => WaitReason::Idle,
            })
        } else {
            None
        };
        match (desired, &self.state) {
            (Some(reason), AttentionSnapshot::Running) => {
                self.zero_streak = 0;
                self.state = AttentionSnapshot::Quiet {
                    since_ms: now_ms,
                    reason,
                };
                Some(self.state.clone())
            }
            // Already quiet/unresponsive: the notice persists (elapsed is
            // derived UI-side from since_ms); reasons only change through
            // activity, which resets to running first.
            _ => None,
        }
    }

    /// One CPU-sample verdict from the engine (`zero` = no CPU consumed
    /// over the sample interval). Only meaningful while sampling is armed.
    pub fn on_cpu_sample(&mut self, zero: bool, now_ms: u64) -> Option<AttentionSnapshot> {
        match &self.state {
            AttentionSnapshot::Quiet {
                reason: WaitReason::Idle,
                ..
            } => {
                if zero {
                    self.zero_streak += 1;
                    if self.zero_streak >= self.th.cpu_samples {
                        // Evidence conclusive: zero CPU sustained, no tool,
                        // no in-flight request (spec 01).
                        self.state = AttentionSnapshot::Unresponsive {
                            since_ms: now_ms,
                            zero_samples: self.zero_streak,
                            sampled_ms: u64::from(self.zero_streak)
                                * self.th.cpu_sample_interval_ms,
                        };
                        return Some(self.state.clone());
                    }
                } else {
                    // CPU active without output: stays quiet, never hardens
                    // (the hang-busy acceptance).
                    self.zero_streak = 0;
                }
                None
            }
            AttentionSnapshot::Unresponsive { since_ms, .. } => {
                if zero {
                    // Evidence keeps accumulating; refresh so the UI's
                    // sample count stays honest.
                    self.zero_streak += 1;
                    self.state = AttentionSnapshot::Unresponsive {
                        since_ms: *since_ms,
                        zero_samples: self.zero_streak,
                        sampled_ms: u64::from(self.zero_streak) * self.th.cpu_sample_interval_ms,
                    };
                    Some(self.state.clone())
                } else {
                    // Partial evidence returned (CPU without output):
                    // unresponsive → quiet (spec 01 transition table).
                    self.zero_streak = 0;
                    self.state = AttentionSnapshot::Quiet {
                        since_ms: now_ms,
                        reason: WaitReason::Idle,
                    };
                    Some(self.state.clone())
                }
            }
            _ => {
                self.zero_streak = 0;
                None
            }
        }
    }

    /// Provable death from OS facts (engine probe, two consecutive ticks).
    /// Immediate and sticky; the recorder still owns the journal's terminal
    /// truth when the process actually exits.
    pub fn fail(&mut self, cause: String, now_ms: u64) -> Option<AttentionSnapshot> {
        if matches!(self.state, AttentionSnapshot::Failed { .. }) {
            return None;
        }
        self.state = AttentionSnapshot::Failed {
            since_ms: now_ms,
            cause,
        };
        Some(self.state.clone())
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

    const TH: AttentionThresholds = AttentionThresholds {
        quiet_after_ms: 120_000,
        tool_exec_after_ms: 600_000,
        cpu_samples: 6,
        cpu_sample_interval_ms: 5_000,
    };

    fn rec(event: JournalEvent) -> Record {
        Record {
            seq: 1,
            ts_ms: 0,
            event,
        }
    }

    fn provider(event: SessionEvent) -> Record {
        rec(JournalEvent::Provider { event })
    }

    fn delivered(c: &mut Classifier, now: u64) {
        c.on_record(&rec(JournalEvent::UserDelivered {}), now);
    }

    fn completed(c: &mut Classifier, now: u64) {
        c.on_record(
            &provider(SessionEvent::TurnCompleted {
                subtype: "success".into(),
                is_error: false,
            }),
            now,
        );
    }

    /// THE regression test (build directive): the CLI lingers after
    /// turn_completed awaiting stdin — hours of that silence is normal.
    #[test]
    fn idle_between_turns_never_trips_any_clock() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(
            &provider(SessionEvent::TextDelta { text: "hi".into() }),
            1_000,
        );
        completed(&mut c, 2_000);
        for t in (3_000..7_200_000).step_by(60_000) {
            assert_eq!(c.on_tick(t), None, "tick at {t} must not classify");
        }
        assert_eq!(c.snapshot(), AttentionSnapshot::Running);
    }

    /// The recorder's extra/provider channels are unordered: a fast CLI's
    /// `TurnCompleted` can be journaled before the `UserDelivered` that
    /// opened the turn. The pair must net to "no open turn" in either
    /// order — a bool here left a phantom turn open forever (caught live
    /// by the idle-between-turns acceptance test, 2026-08-16).
    #[test]
    fn completed_before_delivered_still_closes_the_turn() {
        let mut c = Classifier::new(TH, 0);
        c.on_record(&rec(JournalEvent::UserSent { text: "go".into() }), 0);
        c.on_record(&provider(SessionEvent::TextDelta { text: "r".into() }), 10);
        completed(&mut c, 20); // journaled first (channel race)
        delivered(&mut c, 30); // its own delivery confirmation lands late
        assert!(!c.in_turn(), "reordered pair must net to closed");
        for t in (1_000..7_200_000).step_by(60_000) {
            assert_eq!(c.on_tick(t), None, "tick at {t} must not classify");
        }
        // And the next real turn still opens/classifies normally.
        delivered(&mut c, 7_200_000);
        assert!(c.in_turn());
        assert!(matches!(
            c.on_tick(7_200_000 + TH.quiet_after_ms),
            Some(AttentionSnapshot::Quiet { .. })
        ));
    }

    #[test]
    fn silent_mid_turn_goes_quiet_idle_at_threshold() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(
            &provider(SessionEvent::TextDelta { text: "x".into() }),
            5_000,
        );
        assert_eq!(c.on_tick(5_000 + TH.quiet_after_ms - 1), None);
        match c.on_tick(5_000 + TH.quiet_after_ms) {
            Some(AttentionSnapshot::Quiet {
                reason: WaitReason::Idle,
                ..
            }) => {}
            other => panic!("expected quiet/idle, got {other:?}"),
        }
    }

    #[test]
    fn awaiting_api_is_the_reason_when_requesting_was_marked() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(
            &provider(SessionEvent::System {
                subtype: "status".into(),
                raw: r#"{"type":"system","subtype":"status","status":"requesting"}"#.into(),
            }),
            1_000,
        );
        match c.on_tick(1_000 + TH.quiet_after_ms) {
            Some(AttentionSnapshot::Quiet {
                reason: WaitReason::Api { since_ms: 1_000 },
                ..
            }) => {}
            other => panic!("expected quiet/api, got {other:?}"),
        }
        // Response bytes clear the request mark and the notice.
        assert_eq!(
            c.on_record(
                &provider(SessionEvent::TextDelta { text: "r".into() }),
                200_000
            ),
            Some(AttentionSnapshot::Running)
        );
    }

    #[test]
    fn outstanding_tool_suppresses_quiet_until_its_own_threshold() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(
            &provider(SessionEvent::ToolUseStarted {
                id: "t1".into(),
                name: "Bash".into(),
                input: String::new(),
            }),
            1_000,
        );
        // An 8-minute legitimate tool call: way past QUIET_AFTER, under
        // TOOL_EXEC_AFTER — no notice (the acceptance case).
        for t in (2_000..1_000 + 480_000).step_by(30_000) {
            assert_eq!(c.on_tick(t), None, "tick at {t}");
        }
        match c.on_tick(1_000 + TH.tool_exec_after_ms) {
            Some(AttentionSnapshot::Quiet {
                reason:
                    WaitReason::Tool {
                        name,
                        since_ms: 1_000,
                    },
                ..
            }) => assert_eq!(name, "Bash"),
            other => panic!("expected quiet/tool, got {other:?}"),
        }
        // Tool resolves → running again; quiet clock restarts from there.
        assert_eq!(
            c.on_record(
                &provider(SessionEvent::ToolUseFinished {
                    id: "t1".into(),
                    result: String::new(),
                    is_error: false,
                }),
                700_000
            ),
            Some(AttentionSnapshot::Running)
        );
        assert_eq!(c.on_tick(700_000 + TH.quiet_after_ms - 1), None);
        assert!(matches!(
            c.on_tick(700_000 + TH.quiet_after_ms),
            Some(AttentionSnapshot::Quiet { .. })
        ));
    }

    #[test]
    fn steady_output_never_goes_quiet() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        let mut t = 0;
        while t < 1_000_000 {
            t += TH.quiet_after_ms / 2;
            c.on_record(&provider(SessionEvent::TextDelta { text: ".".into() }), t);
            assert_eq!(c.on_tick(t + 1), None);
        }
    }

    #[test]
    fn zero_cpu_hardens_quiet_into_unresponsive_and_recovers() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_tick(TH.quiet_after_ms);
        assert!(c.wants_cpu_sampling());
        let mut t = TH.quiet_after_ms;
        for i in 1..TH.cpu_samples {
            t += TH.cpu_sample_interval_ms;
            assert_eq!(
                c.on_cpu_sample(true, t),
                None,
                "sample {i} not yet conclusive"
            );
        }
        t += TH.cpu_sample_interval_ms;
        match c.on_cpu_sample(true, t) {
            Some(AttentionSnapshot::Unresponsive {
                zero_samples,
                sampled_ms,
                ..
            }) => {
                assert_eq!(zero_samples, TH.cpu_samples);
                assert_eq!(
                    sampled_ms,
                    u64::from(TH.cpu_samples) * TH.cpu_sample_interval_ms
                );
            }
            other => panic!("expected unresponsive, got {other:?}"),
        }
        // CPU without output: partial evidence → back to quiet, never
        // straight to running (spec 01 transition table).
        match c.on_cpu_sample(false, t + 5_000) {
            Some(AttentionSnapshot::Quiet {
                reason: WaitReason::Idle,
                ..
            }) => {}
            other => panic!("expected quiet, got {other:?}"),
        }
        // A real byte → running.
        assert_eq!(
            c.on_record(
                &provider(SessionEvent::TextDelta { text: "b".into() }),
                t + 6_000
            ),
            Some(AttentionSnapshot::Running)
        );
    }

    #[test]
    fn cpu_activity_keeps_quiet_from_hardening() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_tick(TH.quiet_after_ms);
        let mut t = TH.quiet_after_ms;
        // Alternating and mostly-zero patterns with interleaved activity:
        // the streak resets, unresponsive never fires (hang-busy stays
        // quiet forever).
        for i in 0..40 {
            t += TH.cpu_sample_interval_ms;
            let zero = i % 3 != 2;
            assert_eq!(c.on_cpu_sample(zero, t), None);
        }
        assert_eq!(c.snapshot().state_name(), "quiet");
    }

    fn approval_requested(id: &str, tool: &str) -> Record {
        provider(SessionEvent::ApprovalRequested {
            request_id: id.into(),
            tool: tool.into(),
            display_name: tool.into(),
            description: "writes a file".into(),
            path: Some("/repo/src/main.rs".into()),
            input_json: r#"{"file_path":"/repo/src/main.rs"}"#.into(),
            suggestions_json: None,
        })
    }

    /// THE Slice K regression: a session held on a permission question for
    /// an operator's lunch break must never be reported as ambiguous, and
    /// must never be offered a kill. Held past `QUIET_AFTER` (2 min) AND
    /// past `TOOL_EXEC_AFTER` (10 min) — an hour of it — with the tool
    /// genuinely outstanding the whole time.
    #[test]
    fn a_pending_approval_holds_both_clocks_indefinitely() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(
            &provider(SessionEvent::ToolUseStarted {
                id: "toolu_1".into(),
                name: "Write".into(),
                input: String::new(),
            }),
            1_000,
        );
        // The question lands: parked immediately, not two minutes later.
        match c.on_record(&approval_requested("req-1", "Write"), 1_100) {
            Some(AttentionSnapshot::AwaitingApproval {
                pending: 1,
                ref tool,
                ..
            }) if tool == "Write" => {}
            other => panic!("expected awaiting_approval at once, got {other:?}"),
        }
        // One hour of ticks: both thresholds are long past and neither
        // fires. Nothing offers to kill this session.
        for t in (2_000..3_600_000).step_by(10_000) {
            assert_eq!(c.on_tick(t), None, "tick at {t}ms must not classify");
            assert_eq!(c.snapshot().state_name(), "awaiting_approval");
        }
        // And `unresponsive` is unreachable: sampling is never armed, and
        // even a forced run of zero samples cannot harden this state.
        assert!(!c.wants_cpu_sampling());
        for i in 0..(TH.cpu_samples * 4) {
            assert_eq!(
                c.on_cpu_sample(true, 3_600_000 + u64::from(i) * 5_000),
                None
            );
        }
        assert_eq!(c.snapshot().state_name(), "awaiting_approval");
    }

    /// The agent keeps talking while it waits (it often does — "I need
    /// your permission to write the file", capture A). Chatter is not an
    /// answer: the notice must survive it.
    #[test]
    fn provider_chatter_does_not_clear_the_parked_notice() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(&approval_requested("req-1", "Write"), 1_000);
        for t in (2_000..60_000).step_by(5_000) {
            c.on_record(
                &provider(SessionEvent::TextDelta {
                    text: "waiting…".into(),
                }),
                t,
            );
            assert_eq!(c.snapshot().state_name(), "awaiting_approval");
        }
    }

    /// Answering releases the session, and the clocks restart from the
    /// decision — an approval that took an hour must not instantly trip
    /// `QUIET_AFTER` on the way out.
    #[test]
    fn deciding_releases_the_session_and_restarts_the_clocks() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(&approval_requested("req-1", "Write"), 1_000);
        let decided_at = 3_600_000;
        assert_eq!(
            c.on_record(
                &rec(JournalEvent::ApprovalDecided {
                    request_id: "req-1".into(),
                    allowed: true,
                    message: None,
                }),
                decided_at,
            ),
            Some(AttentionSnapshot::Running)
        );
        // Not instantly quiet despite an hour of wall clock behind it.
        assert_eq!(c.on_tick(decided_at + TH.quiet_after_ms - 1), None);
        assert!(matches!(
            c.on_tick(decided_at + TH.quiet_after_ms),
            Some(AttentionSnapshot::Quiet { .. })
        ));
    }

    /// A withdrawal (the provider interrupted the turn) releases the
    /// session the same way an answer does.
    #[test]
    fn a_withdrawn_request_releases_the_session() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(&approval_requested("req-1", "Bash"), 1_000);
        assert_eq!(
            c.on_record(
                &provider(SessionEvent::ApprovalWithdrawn {
                    request_id: "req-1".into(),
                    reason: "the provider withdrew the request".into(),
                }),
                2_000,
            ),
            Some(AttentionSnapshot::Running)
        );
    }

    /// Several questions can stand at once; the notice names the oldest
    /// and counts them all, and only the last answer releases the session.
    #[test]
    fn the_notice_counts_every_open_question_and_names_the_oldest() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        c.on_record(&approval_requested("req-1", "Write"), 1_000);
        match c.on_record(&approval_requested("req-2", "Bash"), 2_000) {
            Some(AttentionSnapshot::AwaitingApproval {
                pending: 2,
                ref tool,
                since_ms,
            }) if tool == "Write" && since_ms == 1_000 => {}
            other => panic!("expected 2 pending named by the oldest, got {other:?}"),
        }
        // Answering the newer one leaves the session parked on the older.
        match c.on_record(
            &rec(JournalEvent::ApprovalDecided {
                request_id: "req-2".into(),
                allowed: false,
                message: Some("no".into()),
            }),
            3_000,
        ) {
            Some(AttentionSnapshot::AwaitingApproval {
                pending: 1,
                ref tool,
                ..
            }) if tool == "Write" => {}
            other => panic!("expected still parked on req-1, got {other:?}"),
        }
        assert_eq!(c.on_tick(3_600_000), None);
    }

    #[test]
    fn probe_failure_branches() {
        let base = ProbeFacts {
            exited: false,
            vanished: false,
            zombie: false,
            parent_ok: true,
            stdout_eof: false,
            in_turn: true,
        };
        assert_eq!(probe_failure(&base), None);
        assert_eq!(
            probe_failure(&ProbeFacts {
                exited: true,
                zombie: true,
                ..base
            }),
            None
        );
        assert!(probe_failure(&ProbeFacts {
            zombie: true,
            ..base
        })
        .unwrap()
        .contains("zombie"));
        assert!(probe_failure(&ProbeFacts {
            vanished: true,
            ..base
        })
        .unwrap()
        .contains("vanished"));
        assert!(probe_failure(&ProbeFacts {
            parent_ok: false,
            ..base
        })
        .unwrap()
        .contains("parent"));
        assert!(probe_failure(&ProbeFacts {
            stdout_eof: true,
            ..base
        })
        .unwrap()
        .contains("mid-turn"));
        assert!(probe_failure(&ProbeFacts {
            stdout_eof: true,
            in_turn: false,
            ..base
        })
        .unwrap()
        .contains("stdout closed"));
    }

    #[test]
    fn failed_is_sticky_and_immediate() {
        let mut c = Classifier::new(TH, 0);
        delivered(&mut c, 0);
        let snap = c.fail(
            "stdout closed mid-turn while the process is alive".into(),
            500,
        );
        assert!(matches!(snap, Some(AttentionSnapshot::Failed { .. })));
        // A stale descendant byte must not resurrect a provably dead
        // session.
        assert_eq!(
            c.on_record(
                &provider(SessionEvent::TextDelta {
                    text: "ghost".into()
                }),
                600
            ),
            None
        );
        assert_eq!(c.snapshot().state_name(), "failed");
        assert_eq!(c.on_tick(10_000_000), None);
    }
}
