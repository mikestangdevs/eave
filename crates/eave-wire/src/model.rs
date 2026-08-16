//! Transcript projection: fold journal records into renderable rows.
//!
//! This is the *only* transcript state a client holds, and it is derived
//! purely from replayed records — UI is pure projection, resubscribe +
//! replay, never merge (locked decision; spec 03 §Restart survival). It
//! lives in the wire crate so every surface (GPUI shell, tests, a future
//! non-terminal client) renders the same derivation — in particular the
//! delivery rule: a user message is **sent** only once its order-paired
//! `UserDelivered` confirmation is seen; a `Closed` record finalizes every
//! still-pending send as undelivered (spec 02 §Delivery state).

use eave_core::events::SessionEvent;
use eave_core::journal::{JournalEvent, Outcome, Record};

use crate::Cursor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// `UserSent` journaled, confirmation not yet seen. Live sessions only.
    Pending,
    /// The order-paired `UserDelivered` arrived.
    Delivered,
    /// The session closed with no confirmation — the agent provably never
    /// saw this message. Must render visibly distinct from sent.
    Undelivered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Finished,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    User { text: String, delivery: Delivery },
    /// Assistant text; `live` while streaming deltas, settled by the
    /// snapshot or a turn end.
    Assistant { text: String, live: bool },
    Tool { id: String, name: String, status: ToolStatus },
    /// Non-content markers rendered inline: session start, turn ends,
    /// interruptions, terminal outcome.
    Marker { text: String },
}

/// One session's projected transcript plus its resume cursor.
#[derive(Debug, Default)]
pub struct Transcript {
    pub rows: Vec<Row>,
    /// Per-row revision, parallel to `rows`: bumped on every mutation of
    /// that row. Lets projections (the display tree) skip unchanged rows
    /// without rehashing content — O(changed) folds instead of
    /// O(transcript) per burst. If you mutate `rows` directly (tests),
    /// call [`Transcript::touch`]; `apply` maintains this itself.
    pub revs: Vec<u64>,
    /// Last applied (epoch, seq) — the subscribe cursor for a mid-life
    /// reconnect. A relaunched client starts with `None` and full replay.
    pub cursor: Option<Cursor>,
    /// Terminal state name once a `Closed` record was applied.
    pub closed: Option<String>,
    rev_counter: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop everything — a `Reset` frame arrived (epoch change or first
    /// subscribe); replay from seq 1 follows. The revision counter
    /// survives so rows of the replayed transcript never reuse a revision
    /// a stale projection cache may still hold.
    pub fn reset(&mut self) {
        let counter = self.rev_counter;
        *self = Self::default();
        self.rev_counter = counter;
    }

    /// Apply records in seq order. Records at or below the cursor are
    /// ignored (idempotent against replay overlap); a seq gap is the
    /// caller's signal to resubscribe, surfaced as an error.
    pub fn apply(&mut self, epoch: u64, records: &[Record]) -> Result<(), SeqGap> {
        for rec in records {
            if let Some(c) = self.cursor {
                if c.epoch == epoch && rec.seq <= c.seq {
                    continue;
                }
                if c.epoch == epoch && rec.seq != c.seq + 1 {
                    return Err(SeqGap {
                        expected: c.seq + 1,
                        got: rec.seq,
                    });
                }
            }
            self.apply_event(&rec.event);
            self.cursor = Some(Cursor {
                epoch,
                seq: rec.seq,
            });
        }
        Ok(())
    }

    /// Mark row `ix` as changed. `apply` calls this on every mutation;
    /// call it yourself only when mutating `rows` directly.
    pub fn touch(&mut self, ix: usize) {
        self.rev_counter += 1;
        if self.revs.len() < self.rows.len() {
            self.revs.resize(self.rows.len(), self.rev_counter);
        }
        if let Some(r) = self.revs.get_mut(ix) {
            *r = self.rev_counter;
        }
    }

    /// Append a row with its revision tracked.
    pub fn push_row(&mut self, row: Row) {
        self.rev_counter += 1;
        self.rows.push(row);
        self.revs.push(self.rev_counter);
    }

    fn apply_event(&mut self, event: &JournalEvent) {
        match event {
            JournalEvent::Provider { event } => self.apply_provider(event),
            JournalEvent::UserSent { text } => self.push_row(Row::User {
                text: text.clone(),
                delivery: Delivery::Pending,
            }),
            JournalEvent::UserDelivered {} => {
                // Order-paired: confirm the oldest pending send.
                for ix in 0..self.rows.len() {
                    if let Row::User { delivery, .. } = &mut self.rows[ix] {
                        if *delivery == Delivery::Pending {
                            *delivery = Delivery::Delivered;
                            self.touch(ix);
                            break;
                        }
                    }
                }
            }
            JournalEvent::ToolInterrupted { id } => {
                self.set_tool_status(id, ToolStatus::Interrupted);
            }
            JournalEvent::TurnInterrupted { reason } => {
                self.settle_live_rows();
                self.push_row(Row::Marker {
                    text: format!("turn interrupted: {reason}"),
                });
            }
            JournalEvent::Closed { outcome } => self.apply_closed(outcome),
        }
    }

    fn apply_provider(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Init { session_id } => self.push_row(Row::Marker {
                text: format!("session started ({session_id})"),
            }),
            // Deltas and their snapshot pair with the *live* assistant row,
            // which is not necessarily the last row — engine-side records
            // (UserSent/UserDelivered) interleave with provider events.
            SessionEvent::TextDelta { text } => match self.last_live_assistant_ix() {
                Some(ix) => {
                    if let Row::Assistant { text: t, .. } = &mut self.rows[ix] {
                        t.push_str(text);
                    }
                    self.touch(ix);
                }
                None => self.push_row(Row::Assistant {
                    text: text.clone(),
                    live: true,
                }),
            },
            // The post-stream snapshot replaces the deltas that built it —
            // replaced, never merged (spec 03 §Streaming commit).
            SessionEvent::AssistantText { text } => match self.last_live_assistant_ix() {
                Some(ix) => {
                    if let Row::Assistant { text: t, live } = &mut self.rows[ix] {
                        *t = text.clone();
                        *live = false;
                    }
                    self.touch(ix);
                }
                None => self.push_row(Row::Assistant {
                    text: text.clone(),
                    live: false,
                }),
            },
            SessionEvent::ToolUseStarted { id, name } => self.push_row(Row::Tool {
                id: id.clone(),
                name: name.clone(),
                status: ToolStatus::Running,
            }),
            SessionEvent::ToolUseFinished { id } => {
                self.set_tool_status(id, ToolStatus::Finished);
            }
            // The CLI's echo of our own message — the `UserSent` row already
            // renders it; a duplicate row would be merge-by-accident.
            SessionEvent::UserEcho { .. } => {}
            SessionEvent::TurnCompleted { is_error, .. } => {
                self.settle_live_rows();
                if *is_error {
                    self.rows.push(Row::Marker {
                        text: "turn completed with error".into(),
                    });
                }
            }
            // Stage E consumes these from the journal; Stage C does not
            // render them (spec 01 §Attention clocks).
            SessionEvent::System { .. } => {}
            SessionEvent::Malformed { .. } | SessionEvent::Unknown { .. } => {}
            // Terminal classification renders via the `Closed` record that
            // follows; the raw exit event carries no extra display state.
            SessionEvent::Exited { .. } => {}
        }
    }

    /// `Closed` ends the process segment: every pending send is now
    /// provably undelivered, every running tool display-settles as
    /// interrupted (the process that ran it is gone), and open streaming
    /// text settles. A later `--resume` segment appends after this.
    fn apply_closed(&mut self, outcome: &Outcome) {
        for ix in 0..self.rows.len() {
            let changed = match &mut self.rows[ix] {
                Row::User { delivery, .. } if *delivery == Delivery::Pending => {
                    *delivery = Delivery::Undelivered;
                    true
                }
                Row::Tool { status, .. } if *status == ToolStatus::Running => {
                    *status = ToolStatus::Interrupted;
                    true
                }
                _ => false,
            };
            if changed {
                self.touch(ix);
            }
        }
        self.settle_live_rows();
        let state = outcome.state_name();
        self.closed = Some(state.to_string());
        self.push_row(Row::Marker {
            text: format!("session closed: {state}"),
        });
    }

    fn last_live_assistant_ix(&self) -> Option<usize> {
        self.rows.iter().enumerate().rev().find_map(|(ix, row)| match row {
            Row::Assistant { live, .. } if *live => Some(ix),
            _ => None,
        })
    }

    fn settle_live_rows(&mut self) {
        for ix in 0..self.rows.len() {
            if let Row::Assistant { live, .. } = &mut self.rows[ix] {
                if *live {
                    *live = false;
                    self.touch(ix);
                }
            }
        }
    }

    fn set_tool_status(&mut self, id: &str, status: ToolStatus) {
        for ix in 0..self.rows.len() {
            if let Row::Tool { id: rid, status: s, .. } = &mut self.rows[ix] {
                if rid == id {
                    *s = status;
                    self.touch(ix);
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqGap {
    pub expected: u64,
    pub got: u64,
}

impl std::fmt::Display for SeqGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "record seq gap: expected {}, got {} — resubscribe",
            self.expected, self.got
        )
    }
}

impl std::error::Error for SeqGap {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64, event: JournalEvent) -> Record {
        Record {
            seq,
            ts_ms: 0,
            event,
        }
    }

    #[test]
    fn undelivered_send_is_distinct_from_sent() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "hi".into() }),
                rec(2, JournalEvent::UserDelivered {}),
                rec(3, JournalEvent::UserSent { text: "lost".into() }),
                rec(
                    4,
                    JournalEvent::Closed {
                        outcome: Outcome::Interrupted,
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            t.rows[0],
            Row::User {
                text: "hi".into(),
                delivery: Delivery::Delivered
            }
        );
        assert_eq!(
            t.rows[1],
            Row::User {
                text: "lost".into(),
                delivery: Delivery::Undelivered
            }
        );
    }

    #[test]
    fn repaired_session_renders_interrupted_not_completed() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseStarted {
                            id: "toolu_1".into(),
                            name: "Bash".into(),
                        },
                    },
                ),
                rec(
                    2,
                    JournalEvent::ToolInterrupted {
                        id: "toolu_1".into(),
                    },
                ),
                rec(
                    3,
                    JournalEvent::TurnInterrupted {
                        reason: "session ended without a terminal event".into(),
                    },
                ),
                rec(
                    4,
                    JournalEvent::Closed {
                        outcome: Outcome::Interrupted,
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(t.closed.as_deref(), Some("interrupted"));
        assert!(matches!(
            t.rows[0],
            Row::Tool {
                status: ToolStatus::Interrupted,
                ..
            }
        ));
        assert!(t
            .rows
            .iter()
            .any(|r| matches!(r, Row::Marker { text } if text.contains("turn interrupted"))));
    }

    #[test]
    fn replay_overlap_is_idempotent_and_gaps_error() {
        let mut t = Transcript::new();
        let r1 = rec(1, JournalEvent::UserSent { text: "a".into() });
        let r2 = rec(2, JournalEvent::UserDelivered {});
        t.apply(1, &[r1.clone(), r2.clone()]).unwrap();
        // Overlap: applying r2 again is a no-op.
        t.apply(1, &[r2]).unwrap();
        assert_eq!(t.rows.len(), 1);
        // Gap: seq 4 after 2 errors.
        let r4 = rec(4, JournalEvent::UserDelivered {});
        assert!(t.apply(1, &[r4]).is_err());
    }

    #[test]
    fn snapshot_replaces_deltas() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: SessionEvent::TextDelta { text: "hel".into() },
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: SessionEvent::TextDelta { text: "lo".into() },
                    },
                ),
                rec(
                    3,
                    JournalEvent::Provider {
                        event: SessionEvent::AssistantText {
                            text: "hello".into(),
                        },
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            t.rows,
            vec![Row::Assistant {
                text: "hello".into(),
                live: false
            }]
        );
    }
}
