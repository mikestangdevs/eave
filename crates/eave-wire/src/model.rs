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

use eave_core::events::{ContextWindow, SessionEvent, TurnTokens};
use eave_core::journal::{JournalEvent, Outcome, Record};
use eave_core::tool::ToolCall;

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

/// Where one permission question stands (Slice K). Derived from the
/// records, like everything else here: the request, the decision, and the
/// withdrawal are all journaled, so a resubscribing client rebuilds the
/// exact pending set by replay with no separate channel and nothing to go
/// stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// Open. The agent is parked on this and will wait forever.
    Pending,
    Approved,
    /// The operator's words, handed to the model verbatim.
    Denied {
        message: String,
    },
    /// Nobody answered and the question is gone — the provider withdrew
    /// it, or the process ended under it. It can never be answered now,
    /// and a card that pretends otherwise is a lie.
    Withdrawn {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Finished,
    Interrupted,
}

/// The one line a tool row states about what it was asked to do.
///
/// **The typed call is the answer whenever there is one.** The adapter that
/// received the tool call decoded it against the provider's own schema
/// (`eave_core::tool`), so it knows a Codex `commandExecution` and a Pi
/// `bash` are both a command in a way this function never could.
///
/// `None` for `call` is not "no detail" — it is a record written **before**
/// adapters decoded anything, and every one of those is a Claude record,
/// because Claude was the only provider eave had. So the fallback below is
/// exactly what it always was: a map from a Claude tool name to the one key
/// an operator wants to read. It is kept, unchanged, so a journal from
/// before this slice still renders the way it did — deleting it would have
/// silently downgraded every old transcript.
///
/// The keys are the ones the real CLI sends, read off the capture corpus at
/// `docs/research/approvals-capture-2026-08-18/` (Bash → `command`, Read and
/// Write → `file_path`); the rest follow the same tool's documented input
/// and are marked where they are unverified against a capture.
pub fn tool_detail(call: Option<&ToolCall>, name: &str, input: &str) -> Option<String> {
    if let Some(call) = call {
        return call.detail();
    }
    if input.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    // Verified against captures: Bash, Read, Write.
    // Unverified (documented shapes, no capture yet): the rest.
    let key = match name {
        "Bash" | "BashOutput" => "command",
        "Read" | "Write" | "NotebookEdit" => "file_path",
        "Edit" | "MultiEdit" => "file_path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" => "description",
        _ => return None,
    };
    let raw = v.get(key)?.as_str()?;
    // One line: a heredoc or a multi-line command states its first line and
    // says so, rather than making one row as tall as a script.
    let mut lines = raw.lines();
    let first = lines.next()?.trim_end();
    if first.is_empty() {
        return None;
    }
    Some(if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    })
}

/// The before/after an EDIT already carries in its own input, for the
/// providers that never send a diff block (Slice AG1 #4, the second half).
///
/// **The first half of #4 shipped ACP-only.** ACP agents report an edit as
/// `{type:"diff", path, oldText, newText}` in the tool result, which
/// `eave_core::acp::update::first_diff` lifts onto `ToolUseFinished`. Claude
/// Code sends no such block — but it never had to, because an `Edit`'s
/// before-text is the tool's own **argument**, and `input` has held it
/// verbatim all along (`eave_core::tool` documents in as many words that the
/// old/new strings are "deliberately absent" from `ToolCall` because "they
/// are already on `input`"). So this reads a fact eave already stores; it
/// adds nothing to the journal and needs no adapter change, and every
/// transcript already on disk gains its diffs on the next cold start.
///
/// **Keyed on the shape, never on the tool's name.** `old_string` +
/// `new_string` + `file_path` together mean one thing and cannot be
/// mistaken for another tool's arguments, whereas a name table would have
/// to be kept per provider and would quietly render nothing the day a CLI
/// renamed its editor. A shape that does not match yields `None`, which is
/// the row exactly as it renders today — a wrong guess here costs a missing
/// diff, never a false one.
///
/// **`Write` is deliberately excluded.** Its input carries `content` and no
/// before-text, so the only diff derivable from it is "every line added" —
/// true when the file was created and a lie when it was overwritten, and
/// the input does not say which. ACP's absent `oldText` is a different
/// fact: there the AGENT said it created the file.
///
/// **MultiEdit shows its first edit only**, for `first_diff`'s reason —
/// concatenating edits would invent a change nobody made, and picking the
/// largest would make the row's meaning depend on content size.
///
/// No cap is applied: `input` was bounded by
/// `eave_core::events::TOOL_PAYLOAD_CAP` before it was stored, and these
/// are slices of it.
///
/// **The key names are the CLI's documented schema, NOT read off a
/// capture.** `tool_detail` above cites
/// `docs/research/approvals-capture-2026-08-18/` for its verified keys and
/// that directory is no longer in the tree, so there was nothing here to
/// check these against. The shape gate is what makes that safe: three keys
/// that must all be present and all be strings, so a schema that turns out
/// to differ yields `None` and the row renders exactly as it does today.
pub fn diff_from_input(input: &str) -> Option<eave_core::events::ToolDiff> {
    if input.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let path = v.get("file_path")?.as_str()?;
    // MultiEdit nests the same pair inside `edits`; a bare Edit carries it
    // at the top level.
    let edit = match v.get("edits").and_then(|e| e.as_array()) {
        Some(edits) => edits.first()?,
        None => &v,
    };
    let old = edit.get("old_string")?.as_str()?;
    let new = edit.get("new_string")?.as_str()?;
    // An edit that changed nothing is not a change. Drawing it would give
    // the row a diff pane stating that a file stayed as it was.
    if old == new {
        return None;
    }
    Some(eave_core::events::ToolDiff {
        path: path.to_string(),
        old_text: old.to_string(),
        new_text: new.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    User {
        text: String,
        delivery: Delivery,
        /// The id the PROVIDER gave this message, once it echoed one back
        /// (Slice AG1 #10). This is the address a rewind is asked about,
        /// so a row without one cannot be a rewind target — which is the
        /// whole of the gate on the surface.
        ///
        /// `None` covers three different situations that must not be
        /// confused: the provider names nothing addressable (Codex, Pi,
        /// every ACP agent), the echo has not arrived yet (a send in
        /// flight), and the journal predates this field. All three are
        /// honestly "eave cannot rewind to this row", which is why one
        /// `None` is enough and no second state is invented.
        provider_msg_id: Option<String>,
    },
    /// Assistant text; `live` while streaming deltas, settled by the
    /// snapshot or a turn end.
    Assistant { text: String, live: bool },
    /// A tool call. `input` is the provider's request object as compact
    /// JSON and `result` its reply as text — both already bounded by
    /// `eave_core::events::TOOL_PAYLOAD_CAP` before they reach here, so a
    /// row is never the thing that has to defend the render budget.
    ///
    /// `call` is the adapter's decode of that same input, and `None` means
    /// the record predates adapters decoding anything — see
    /// [`tool_detail`], which is where the two are reconciled.
    Tool {
        id: String,
        name: String,
        status: ToolStatus,
        input: String,
        result: String,
        is_error: bool,
        call: Option<ToolCall>,
        /// The before/after for an edit, when the provider reported one
        /// (Slice AG1 #4). `None` for every provider that does not, and
        /// for journals written before the field existed — both of which
        /// are honestly "no recorded before-text", not "no change".
        ///
        /// Arrives on the FINISH, not the start: an edit's old text is
        /// part of what came back, so a running tool row carries `None`
        /// and gains it when the tool settles.
        diff: Option<eave_core::events::ToolDiff>,
    },
    /// The agent is asking to run one tool (Slice K). Every fact an
    /// operator needs to decide is on the row: an approval given without
    /// knowing what was approved is the failure the slice exists to
    /// prevent.
    Approval {
        request_id: String,
        tool: String,
        description: String,
        path: Option<String>,
        input_json: String,
        status: ApprovalStatus,
    },
    /// Non-content markers rendered inline: session start, turn ends,
    /// interruptions, terminal outcome.
    Marker { text: String },
}

/// One turn's extent in [`Transcript::rows`], with the only clock a fold
/// can honestly read.
///
/// **Why this lives here and not in the app.** A settled turn's duration
/// is not reconstructible from `rows` alone — a `Row` carries no time —
/// and the records that *do* carry `ts_ms` are consumed here, in `apply`,
/// which is the one place that knows both the timestamp and the row index
/// it just produced. Deriving it a second time in the surface would mean a
/// second copy of "what opens a turn", and this codebase has already paid
/// for that once: see the note on [`crate::model::Transcript`]'s sibling
/// in `eave_core::turn`, written after `attention.rs` and `ui.rs` kept two
/// turn counters that disagreed.
///
/// No protocol change: nothing new is journalled, sent, or asked of any
/// adapter. This is the existing projection recording, for itself, a fact
/// it was already being handed and throwing away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSpan {
    /// Index in `rows` of the row that opened this turn — always the
    /// `Row::User` the prompt produced.
    pub start_row: usize,
    /// The opening record's `ts_ms`, not a wall clock: a reader replaying
    /// history must report the turn's real age, not the age of its own
    /// subscription.
    pub started_ms: u64,
    /// `None` while the turn is still open. An UNANNOUNCED turn is closed
    /// by nothing on the wire, so it stays `None` forever and never folds
    /// — the conservative half of that trade, taken deliberately.
    pub ended_ms: Option<u64>,
    /// The journal seq of the `Checkpoint` record taken immediately
    /// before this turn was dispatched (Slice AG1 #11) — which is also
    /// the address of the git ref holding the bytes, so this one number
    /// is the whole handle a client needs to ask for a restore.
    ///
    /// The captured tree is deliberately NOT carried here. It would make
    /// `TurnSpan` allocate and stop being `Copy`, and no client has a use
    /// for it: the engine reads it back from the record the seq names,
    /// which is the same place a cold replay would.
    ///
    /// `None` for a session outside a git repository, for one whose git
    /// refused, and for every turn journalled before this slice existed.
    /// All three are one fact to a reader — there is nothing to restore,
    /// so nothing offers to.
    pub checkpoint_seq: Option<u64>,
}

impl Transcript {
    /// The checkpoint taken before the turn opening at `start_row`
    /// (Slice AG1 #11).
    ///
    /// A binary search, not a scan: this is asked once per user row per
    /// paint, and a linear `find` would be a walk over all turns for
    /// every turn — waku's quadratic scar, which AG1 #12 names and
    /// refuses. `turns` is pushed in increasing `start_row` order by
    /// `note_turn` and the only removal is a suffix truncation, so it is
    /// sorted by construction.
    pub fn checkpoint_at(&self, start_row: usize) -> Option<u64> {
        let ix = self
            .turns
            .binary_search_by_key(&start_row, |t| t.start_row)
            .ok()?;
        self.turns[ix].checkpoint_seq
    }
}

impl TurnSpan {
    /// How long the turn ran, once it has ended.
    pub fn elapsed_ms(&self) -> Option<u64> {
        self.ended_ms.map(|e| e.saturating_sub(self.started_ms))
    }
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
    /// Every `/name` the agent CLI said it would accept, from the most
    /// recent `Init` in this session's log (Slice AR2).
    ///
    /// **`None` and `Some(vec![])` are different facts and both happen.**
    /// `None` = no `Init` has been replayed — a session that has never run,
    /// or one whose replay has not reached its first frame. `Some(empty)` =
    /// an `Init` that said nothing, which — because the field is
    /// `#[serde(default)]` — covers both a CLI with no commands and a CLI
    /// predating the field. This type keeps that distinction faithfully and
    /// takes no view on it; what to DO about an empty announcement is a
    /// policy, and it lives at the consumer (`SlashCache::rebuild` treats
    /// it as no announcement and falls back to a filesystem scan).
    ///
    /// Derived by replay like every other field here, which is the whole
    /// reason it needs no channel of its own: the `Init` record is in the
    /// journal permanently, so a session that has ever run reports its
    /// inventory again on every later open without spawning anything. What
    /// that inventory describes is the LAST run — install a plugin and this
    /// says so only after the session next starts.
    ///
    /// Last-wins rather than first: a resumed session writes a second
    /// `Init`, and the newer one is the one describing the process that
    /// just started.
    pub inventory: Option<Vec<String>>,
    /// What the last settled turn cost, in the provider's own words
    /// (Slice CC). `None` until a provider states one — most journals
    /// written before this slice never will, and a session driving an
    /// adapter with no usage on its wire never will either.
    pub turn_tokens: Option<TurnTokens>,
    /// How full the model's context window was when the provider last
    /// said (Slice CC).
    ///
    /// **Kept apart from `turn_tokens` on purpose, and updated apart from
    /// it.** These arrive on different frames at different times — an ACP
    /// `usage_update` is context-only and lands mid-turn, an ACP prompt
    /// response is bill-only and lands at the end — so each is last-wins
    /// on its own. Folding them together would let a gauge erase the last
    /// turn's cost, and a bill erase the level.
    pub context: Option<ContextWindow>,
    /// The current turn's reasoning, accumulated from `agent_thought_chunk`
    /// System events. Empty when the provider drops thoughts (Grok) or
    /// never sent any. Cleared on the next `UserSent` so a new turn does
    /// not inherit the last one. Kept after `TurnCompleted` so the
    /// activity box can still show "what it thought" the way bezel's
    /// Takeover does.
    pub thought: String,
    /// When the first thought chunk of this turn arrived, so the activity
    /// header can say how long it thought without inventing a clock.
    pub thought_started_ms: Option<u64>,
    /// Turn extents, in order, maintained by `apply` (Slice AT1).
    ///
    /// Only turns eave itself opened with a prompt appear here. A turn the
    /// agent started on its own has no `UserSent` to anchor it, so it gets
    /// no span and is never folded — stating less rather than guessing a
    /// boundary that is not on the wire.
    pub turns: Vec<TurnSpan>,
    /// A `Checkpoint` record that has arrived but whose turn has not
    /// opened yet. The two are adjacent by construction — the engine
    /// writes the checkpoint one line before the `UserSent`, inside the
    /// send lock — so this holds a value for the width of one record and
    /// no longer. It is NOT an `Option<TurnSpan>` waiting to be filled:
    /// a checkpoint whose send then failed to journal must be dropped by
    /// the next capture, not attached to whatever turn happens along.
    pending_checkpoint: Option<u64>,
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
            let rows_before = self.rows.len();
            self.apply_event(&rec.event);
            self.note_turn(rec, rows_before);
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

    /// Maintain [`Transcript::turns`] alongside the row fold (Slice AT1).
    ///
    /// Called with the row count from *before* the event was applied, so
    /// the row a `UserSent` just pushed is identified by index rather than
    /// by re-deciding what a prompt row looks like.
    fn note_turn(&mut self, rec: &Record, rows_before: usize) {
        match &rec.event {
            JournalEvent::UserSent { .. } => {
                // Guarded on the row actually appearing: the span must
                // point at a row that exists, or the fold would bound a
                // turn by an index into nothing.
                if self.rows.len() > rows_before {
                    self.turns.push(TurnSpan {
                        start_row: rows_before,
                        started_ms: rec.ts_ms,
                        ended_ms: None,
                        checkpoint_seq: self.pending_checkpoint.take(),
                    });
                }
            }
            // Stashed, not attached: the turn it belongs to opens on the
            // NEXT record. `replace` rather than a guarded set, so a
            // checkpoint whose send never journalled is discarded by the
            // next one instead of being inherited by a later turn it
            // never described.
            JournalEvent::Checkpoint { .. } => {
                self.pending_checkpoint = Some(rec.seq);
            }
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. } => {
                self.close_turn(rec.ts_ms)
            }
            JournalEvent::Provider {
                event: SessionEvent::TurnCompleted { .. },
            } => self.close_turn(rec.ts_ms),
            _ => {}
        }
    }

    /// End the open turn, if one is open. First closer wins: a turn
    /// interrupted and then reported complete ended when it was
    /// interrupted, which is when the work stopped.
    fn close_turn(&mut self, ts_ms: u64) {
        if let Some(t) = self.turns.last_mut() {
            if t.ended_ms.is_none() {
                t.ended_ms = Some(ts_ms);
            }
        }
    }

    fn apply_event(&mut self, event: &JournalEvent) {
        match event {
            JournalEvent::Provider { event } => self.apply_provider(event),
            JournalEvent::UserSent { text } => {
                self.thought.clear();
                self.thought_started_ms = None;
                self.push_row(Row::User {
                    text: text.clone(),
                    delivery: Delivery::Pending,
                    // Filled by the provider's own echo, if it sends one.
                    provider_msg_id: None,
                });
            }
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
            // The operator rewound, and the provider agreed (Slice AG1
            // #10). The record is an APPEND — this is where it becomes a
            // shorter transcript.
            //
            // Addressed by the provider's own message id, the same address
            // the provider was asked with, so a cold replay lands on
            // exactly the row the live session did.
            //
            // **The target row goes too.** "Revert to here" means that
            // message is unsent: Claude hands its text back as
            // `prefillText` precisely so it can be edited and sent again,
            // and leaving it on screen would show as history a message the
            // agent no longer knows about.
            //
            // Note what is deliberately NOT undone: `turn_tokens` and
            // `context` still describe a turn that may have just been
            // dropped. That is not an oversight — those tokens were really
            // spent, and a bill does not un-bill because the answer was
            // discarded. Leaving them is the honest reading, and it makes
            // the live fold and the cold replay agree, which is the
            // property that actually matters here.
            JournalEvent::Rewound {
                through_msg_id,
                dropped,
            } => {
                let target = self.rows.iter().position(|r| {
                    matches!(r, Row::User { provider_msg_id: Some(id), .. } if id == through_msg_id)
                });
                // No match is not a corruption: a journal can be replayed
                // by a client that trimmed its head, and a record naming a
                // row that is not here has nothing to do. Silently doing
                // nothing beats truncating to a guessed index.
                if let Some(ix) = target {
                    self.rows.truncate(ix);
                    self.revs.truncate(ix);
                    // A span anchored at or after the cut points into rows
                    // that no longer exist. `start_row` is an INDEX, so
                    // every surviving span keeps its meaning only because
                    // the cut is a suffix — this would be wrong for any
                    // removal from the middle.
                    self.turns.retain(|t| t.start_row < ix);
                    // The discarded turn's reasoning goes with the turn.
                    self.thought.clear();
                    self.thought_started_ms = None;
                    // **Say it happened.** Rows vanishing with nothing to
                    // explain them is the diff stat's `omitted > 0` lie in
                    // another costume: an operator scrolling up would find
                    // history simply different, and no way to tell a
                    // rewind from a bug.
                    self.push_row(Row::Marker {
                        text: match dropped {
                            1 => "rewound — 1 turn discarded".to_string(),
                            n => format!("rewound — {n} turns discarded"),
                        },
                    });
                }
            }
            // The capture itself adds no row: it happened before the
            // prompt the operator typed, and a transcript line for
            // "eave hashed your directory" is noise on every single turn.
            // Its visible form is the affordance on the turn it belongs
            // to, which `note_turn` attaches.
            JournalEvent::Checkpoint { .. } => {}
            // A restore, by contrast, is ALWAYS said out loud. Files on
            // the operator's disk moved, and the transcript is the only
            // place that records when — the same argument `Rewound` makes
            // about rows, with more at stake, because git will not
            // remember this for us.
            JournalEvent::Restored { files, .. } => {
                self.push_row(Row::Marker {
                    text: match files.len() {
                        0 => "restored — the files already matched this point".to_string(),
                        1 => "restored files to before this turn — 1 file changed".to_string(),
                        n => format!("restored files to before this turn — {n} files changed"),
                    },
                });
            }
            JournalEvent::ApprovalDecided {
                request_id,
                allowed,
                message,
            } => self.set_approval_status(
                request_id,
                if *allowed {
                    ApprovalStatus::Approved
                } else {
                    ApprovalStatus::Denied {
                        message: message.clone().unwrap_or_default(),
                    }
                },
            ),
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

    /// Accumulate this turn's reasoning.
    ///
    /// **Two wire shapes reach here and this is the only place either of
    /// them lands.** ACP agents send `agent_thought_chunk` as a `System`
    /// frame; Claude sends a typed [`SessionEvent::ThoughtDelta`] decoded
    /// from its `thinking_delta`. Keeping the accumulation in one function
    /// is what stops two decoders becoming two behaviours — the clock in
    /// particular, which starts at the FIRST chunk of the turn whichever
    /// provider sent it, and is cleared with the thought on the next
    /// `UserSent`.
    fn push_thought(&mut self, text: &str) {
        // An empty chunk is not a thought. The decoders drop these at the
        // wire, but a journal written before they did still replays them,
        // and starting the clock on one would report a session as having
        // thought for an hour about nothing.
        if text.is_empty() {
            return;
        }
        if self.thought_started_ms.is_none() {
            self.thought_started_ms = Some(eave_core::journal::now_ms());
        }
        self.thought.push_str(text);
    }

    fn apply_provider(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Init {
                session_id,
                slash_commands,
                ..
            } => {
                // Slice AR2. Stamped even when empty: "this CLI announced
                // nothing" is a fact a fallback must be able to see, and it
                // is not the same fact as "nothing has run yet".
                self.inventory = Some(slash_commands.clone());
                self.push_row(Row::Marker {
                    text: format!("session started ({session_id})"),
                });
            }
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
            // Keyed by id, not appended: an adapter may restate a start
            // once the arguments it did not have at announcement time
            // arrive (the Claude CLI announces a `tool_use` with an empty
            // `input` and fills it a line later). A second row for one
            // tool would be a lie about how many ran, so the restatement
            // updates the row in place — which is also what makes the
            // chip's argument appear mid-flight rather than never.
            SessionEvent::ToolUseStarted {
                id,
                name,
                input,
                call,
            } => match self
                .rows
                .iter()
                .position(|r| matches!(r, Row::Tool { id: rid, .. } if rid == id))
            {
                Some(ix) => {
                    if let Row::Tool {
                        name: n,
                        input: i,
                        call: c,
                        ..
                    } = &mut self.rows[ix]
                    {
                        n.clone_from(name);
                        i.clone_from(input);
                        c.clone_from(call);
                    }
                    self.touch(ix);
                }
                None => self.push_row(Row::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Running,
                    input: input.clone(),
                    result: String::new(),
                    is_error: false,
                    call: call.clone(),
                    diff: None,
                }),
            },
            SessionEvent::ToolUseFinished {
                id,
                result,
                is_error,
                diff,
            } => {
                self.finish_tool(id, result, *is_error, diff.clone());
            }
            // The CLI's echo of our own message — the `UserSent` row already
            // renders it; a duplicate row would be merge-by-accident. The
            // echo is kept for ONE thing: the id the provider just gave
            // that message (Slice AG1 #10).
            //
            // Order-paired the same way `UserDelivered` is, and for the
            // same reason: the echo answers the oldest send that has not
            // been named yet. Stamping by text would merge two identical
            // prompts onto one row, and a rewind would then target the
            // wrong turn — silently, since both rows read the same.
            SessionEvent::UserEcho {
                provider_msg_id: Some(id),
                ..
            } => {
                for ix in 0..self.rows.len() {
                    if let Row::User {
                        provider_msg_id: slot @ None,
                        ..
                    } = &mut self.rows[ix]
                    {
                        *slot = Some(id.clone());
                        self.touch(ix);
                        break;
                    }
                }
            }
            SessionEvent::UserEcho { .. } => {}
            SessionEvent::TurnCompleted { is_error, .. } => {
                self.settle_live_rows();
                if *is_error {
                    self.rows.push(Row::Marker {
                        text: "turn completed with error".into(),
                    });
                }
            }
            SessionEvent::ApprovalRequested {
                request_id,
                tool,
                description,
                path,
                input_json,
                ..
            } => self.push_row(Row::Approval {
                request_id: request_id.clone(),
                tool: tool.clone(),
                description: description.clone(),
                path: path.clone(),
                input_json: input_json.clone(),
                status: ApprovalStatus::Pending,
            }),
            SessionEvent::ApprovalWithdrawn { request_id, reason } => self.set_approval_status(
                request_id,
                ApprovalStatus::Withdrawn {
                    reason: reason.clone(),
                },
            ),
            SessionEvent::DialogRefused { dialog, .. } => self.push_row(Row::Marker {
                text: format!(
                    "the agent asked a {dialog} question; eave cannot present dialogs yet \
                     and told it so"
                ),
            }),
            SessionEvent::PostureRefused { mode, reason } => self.push_row(Row::Marker {
                text: format!("refused to run in permission mode {mode}: {reason}"),
            }),
            // Stage E consumes these from the journal; Stage C does not
            // render them (spec 01 §Attention clocks).
            // Slice CC: instrumentation, not transcript. It updates two
            // fields and pushes no row — a token count is not something
            // that happened in the conversation, and a row per turn would
            // put a number between every message.
            SessionEvent::TokenUsage { turn, context } => {
                if turn.is_some() {
                    self.turn_tokens = *turn;
                }
                if context.is_some() {
                    self.context = *context;
                }
            }
            SessionEvent::System { subtype, raw } => {
                // The ACP spelling. Claude's arrives typed, below.
                if subtype == "agent_thought_chunk" {
                    if let Some(text) = thought_text(raw) {
                        self.push_thought(&text);
                    }
                }
            }
            // Slice AG1 #14. A plan names the tool call it belongs to
            // and says exactly one thing about it: its decoded form is a
            // checklist. Status, result and input are untouched — the
            // call has already finished, and what a tool RETURNED is not
            // what it MEANS.
            //
            // No row is pushed when the id names nothing. The decoder
            // only emits this for a call it watched close, so that is
            // unreachable today; if it ever becomes reachable, a plan
            // with no row is a plan with no place to go, and the journal
            // still holds it.
            SessionEvent::PlanUpdate { call_id, items } => {
                if let Some(ix) = self
                    .rows
                    .iter()
                    .position(|r| matches!(r, Row::Tool { id, .. } if id == call_id))
                {
                    if let Row::Tool { call, .. } = &mut self.rows[ix] {
                        *call = Some(ToolCall::Todo {
                            items: items.clone(),
                        });
                    }
                    self.touch(ix);
                }
            }
            SessionEvent::ThoughtDelta { text } => self.push_thought(text),
            // Slice CJ. A child thread's frame is recorded, never
            // rendered: eave has no subagent surface, and painting a
            // child's prose or tool row into the parent's transcript is
            // exactly the confusion attribution exists to prevent. The
            // journal holds it fully typed for the slice that builds one.
            SessionEvent::ForeignThread { .. } => {}
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
                // The process that asked is gone, so the question can
                // never be answered — it is withdrawn, not still pending
                // (Slice K). This is also what makes crash-tail repair
                // need no synthetic closer for approvals.
                Row::Approval { status, .. } if *status == ApprovalStatus::Pending => {
                    *status = ApprovalStatus::Withdrawn {
                        reason: "the session ended before anyone answered".into(),
                    };
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
        self.rows
            .iter()
            .enumerate()
            .rev()
            .find_map(|(ix, row)| match row {
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

    /// Open permission questions, oldest first (Slice K). The UI's pending
    /// set — derived, never held separately.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|r| {
            matches!(
                r,
                Row::Approval {
                    status: ApprovalStatus::Pending,
                    ..
                }
            )
        })
    }

    fn set_approval_status(&mut self, request_id: &str, status: ApprovalStatus) {
        for ix in 0..self.rows.len() {
            if let Row::Approval {
                request_id: rid,
                status: s,
                ..
            } = &mut self.rows[ix]
            {
                if rid == request_id {
                    *s = status;
                    self.touch(ix);
                    return;
                }
            }
        }
    }

    /// Resolve a tool row: its status, its reply and the provider's error
    /// flag land together, because they are one fact arriving at one moment
    /// and a row that took them separately could render "done" with the
    /// previous call's output.
    fn finish_tool(
        &mut self,
        id: &str,
        result: &str,
        is_error: bool,
        diff: Option<eave_core::events::ToolDiff>,
    ) {
        for ix in 0..self.rows.len() {
            if let Row::Tool {
                id: rid,
                status,
                input: i,
                result: r,
                is_error: e,
                diff: d,
                ..
            } = &mut self.rows[ix]
            {
                if rid == id {
                    *status = ToolStatus::Finished;
                    *r = result.to_string();
                    *e = is_error;
                    // Slice AG1 #4, second half. The provider's own diff
                    // wins whenever it sent one: it reports what the edit
                    // DID, while the input only says what was asked. They
                    // agree on a successful edit and the renderer already
                    // refuses to draw either on a failed one.
                    *d = diff.or_else(|| diff_from_input(i));
                    self.touch(ix);
                    return;
                }
            }
        }
    }

    fn set_tool_status(&mut self, id: &str, status: ToolStatus) {
        for ix in 0..self.rows.len() {
            if let Row::Tool {
                id: rid, status: s, ..
            } = &mut self.rows[ix]
            {
                if rid == id {
                    *s = status;
                    self.touch(ix);
                    return;
                }
            }
        }
    }
}

/// Pull the thought text out of an ACP `agent_thought_chunk` frame. The
/// journal stores the raw line; the activity box wants the words.
fn thought_text(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let update = v.pointer("/params/update").unwrap_or(&v);
    let text = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .or_else(|| update.get("text").and_then(|t| t.as_str()))?;
    (!text.is_empty()).then(|| text.to_string())
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

    /// The name-keyed fallback, exercised the way it is actually reached:
    /// a record with no typed call, which only a journal written before
    /// this slice can be.
    ///
    /// The inputs here are copied from the real capture corpus
    /// (`docs/research/approvals-capture-2026-08-18/`), not invented, because
    /// a summariser tested against its own author's guess about the wire is
    /// testing nothing.
    #[test]
    fn a_pre_typing_tool_row_still_states_what_it_was_asked_to_do() {
        assert_eq!(
            tool_detail(
                None,
                "Bash",
                r#"{"command":"echo hello-from-bash > out.txt","description":"Write \"hello-from-bash\" to out.txt"}"#
            )
            .as_deref(),
            Some("echo hello-from-bash > out.txt")
        );
        assert_eq!(
            tool_detail(
                None,
                "Write",
                r#"{"file_path":"/tmp/x/out.txt","content":"one"}"#
            )
            .as_deref(),
            Some("/tmp/x/out.txt")
        );
        // A tool whose summary key we do not know states its name alone.
        assert_eq!(tool_detail(None, "SomeMcpTool", r#"{"whatever":1}"#), None);
        // ...and so does one whose input never arrived (a pre-slice journal).
        assert_eq!(tool_detail(None, "Bash", ""), None);
        // Malformed input is not a panic and not a lie.
        assert_eq!(tool_detail(None, "Bash", "{not json"), None);
    }

    /// The typed call wins outright, and it is the only thing that CAN
    /// answer for a provider whose tool is not called `Bash`: the
    /// name-keyed fallback has never heard of Codex's `commandExecution`,
    /// and the raw input it would have to guess from is not even the same
    /// shape.
    #[test]
    fn a_typed_call_answers_where_the_name_keyed_guess_cannot() {
        let codex = ToolCall::Run {
            command: "cargo test --workspace".into(),
        };
        assert_eq!(
            tool_detail(
                Some(&codex),
                "commandExecution",
                r#"{"command":"cargo test --workspace","cwd":"/repo"}"#
            )
            .as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            tool_detail(None, "commandExecution", r#"{"command":"cargo test"}"#),
            None,
            "this is the defect the typed call closes; if it ever answers, \
             the fallback learned a name and this test should say so"
        );
        // An adapter that looked and did not recognise the tool says so,
        // and does not fall back to guessing.
        assert_eq!(
            tool_detail(
                Some(&ToolCall::Unknown {
                    name: "Bash".into()
                }),
                "Bash",
                r#"{"command":"ls"}"#
            ),
            None
        );
    }

    /// A row is one line tall however long the command is.
    #[test]
    fn a_multi_line_command_states_its_first_line_and_says_so() {
        let d = tool_detail(None, "Bash", r#"{"command":"cat <<EOF\nline two\nEOF"}"#).unwrap();
        assert_eq!(d, "cat <<EOF …");
        assert!(!d.contains('\n'), "a tool detail must never be multi-line");
    }

    use super::*;

    fn rec(seq: u64, event: JournalEvent) -> Record {
        Record {
            seq,
            ts_ms: 0,
            event,
        }
    }

    /// **Slice CC.** The two token facts fold independently, and a
    /// record carrying one must never clear the other.
    ///
    /// This is the projection-side half of the same distinction the event
    /// makes: a context gauge arrives mid-turn (ACP `usage_update`, Codex
    /// `thread/tokenUsage/updated`) and a bill arrives at the end (the
    /// settled prompt response, Claude's `result` line). Fold them into
    /// one slot and every gauge wipes the last turn's cost off the
    /// screen.
    #[test]
    fn a_context_gauge_does_not_erase_the_last_turns_bill() {
        let usage = |turn, context| JournalEvent::Provider {
            event: SessionEvent::TokenUsage { turn, context },
        };
        let bill = TurnTokens::stated(Some(19_412), Some(455), Some(19_000), Some(400));
        let gauge = |used| {
            Some(ContextWindow {
                used_tokens: used,
                limit_tokens: Some(200_000),
            })
        };

        let mut t = Transcript::new();
        t.apply(1, &[rec(1, usage(bill, gauge(19_867)))]).unwrap();
        assert_eq!(t.turn_tokens, bill);
        assert_eq!(t.context, gauge(19_867));

        // A gauge-only record moves the level and leaves the bill alone.
        t.apply(1, &[rec(2, usage(None, gauge(21_000)))]).unwrap();
        assert_eq!(t.context, gauge(21_000));
        assert_eq!(t.turn_tokens, bill, "the last bill is still the last bill");

        // A bill-only record moves the bill and leaves the level alone.
        let next = TurnTokens::stated(Some(21_000), Some(9), None, None);
        t.apply(1, &[rec(3, usage(next, None))]).unwrap();
        assert_eq!(t.turn_tokens, next);
        assert_eq!(t.context, gauge(21_000));

        // And it is instrumentation, not conversation: no rows.
        assert!(t.rows.is_empty(), "{:?}", t.rows);
    }

    /// A journal that predates the slice replays to no token facts at
    /// all, rather than to zeroes.
    #[test]
    fn a_journal_with_no_usage_records_projects_no_token_facts() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[rec(
                1,
                JournalEvent::Provider {
                    event: SessionEvent::AssistantText { text: "hi".into() },
                },
            )],
        )
        .unwrap();
        assert_eq!(t.turn_tokens, None);
        assert_eq!(t.context, None);
    }

    /// **Slice AR2.** The inventory is derived by replay like every other
    /// fact here, and that is what saves it a channel of its own.
    ///
    /// The `Init` record is in the journal permanently, so a session that
    /// has ever run reports its CLI's own command list on every later open
    /// with nothing live and nothing spawned. A resumed session writes a
    /// second `Init` and the newer one wins — it describes the process that
    /// is actually running.
    #[test]
    fn the_inventory_is_replayed_and_the_latest_init_wins() {
        let init = |names: &[&str]| JournalEvent::Provider {
            event: SessionEvent::Init {
                session_id: "s1".into(),
                permission_mode: Some("default".into()),
                slash_commands: names.iter().map(|n| (*n).to_string()).collect(),
            },
        };
        let mut t = Transcript::new();
        assert_eq!(
            t.inventory, None,
            "nothing has run, so nothing has been announced — and that is \
             not the same fact as an empty announcement"
        );

        t.apply(1, &[rec(1, init(&["context", "model"]))]).unwrap();
        assert_eq!(
            t.inventory.as_deref(),
            Some(["context".to_string(), "model".to_string()].as_slice())
        );

        // A second process segment: resumed, and its CLI now has one more.
        t.apply(1, &[rec(2, init(&["context", "model", "dataviz"]))])
            .unwrap();
        assert_eq!(
            t.inventory.as_ref().map(Vec::len),
            Some(3),
            "the newest Init describes the CLI that is running now"
        );

        // A CLI that announced nothing is a DIFFERENT fact from a session
        // that has never run, and the projection keeps them apart. What to
        // do about it is the palette's policy, not this type's.
        t.apply(1, &[rec(3, init(&[]))]).unwrap();
        assert_eq!(t.inventory.as_deref(), Some([].as_slice()));

        // ...and a resubscribe drops it, because the replay that follows
        // rebuilds it. A stale inventory surviving a reset would outlive
        // the session it described.
        t.reset();
        assert_eq!(t.inventory, None);
    }

    // ── Slice AG1 #10: rewind ────────────────────────────────────────

    fn echo(id: &str) -> JournalEvent {
        JournalEvent::Provider {
            event: SessionEvent::UserEcho {
                text: String::new(),
                provider_msg_id: Some(id.into()),
            },
        }
    }

    fn user_ids(t: &Transcript) -> Vec<Option<String>> {
        t.rows
            .iter()
            .filter_map(|r| match r {
                Row::User {
                    provider_msg_id, ..
                } => Some(provider_msg_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The echo names the OLDEST unnamed send, the same order-pairing
    /// `UserDelivered` uses.
    ///
    /// **Two identical prompts is the case that decides it.** Matching on
    /// text would put both ids on the first row and leave the second
    /// unaddressable — and a rewind would then silently target the wrong
    /// turn, with both rows reading the same on screen.
    #[test]
    fn an_echo_names_the_oldest_unnamed_send_even_when_two_read_alike() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::UserSent {
                        text: "same".into(),
                    },
                ),
                rec(2, echo("u-1")),
                rec(
                    3,
                    JournalEvent::UserSent {
                        text: "same".into(),
                    },
                ),
                rec(4, echo("u-2")),
            ],
        )
        .unwrap();
        assert_eq!(
            user_ids(&t),
            vec![Some("u-1".to_string()), Some("u-2".to_string())]
        );
    }

    /// A provider that names nothing leaves every row unaddressable, and
    /// that is the whole gate: no id, no rewind control.
    #[test]
    fn an_echo_without_an_id_leaves_the_row_unaddressable() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "hi".into() }),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: SessionEvent::UserEcho {
                            text: "hi".into(),
                            provider_msg_id: None,
                        },
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(user_ids(&t), vec![None]);
    }

    /// The fold: the target row and everything after it goes, and the
    /// transcript SAYS so.
    #[test]
    fn a_rewind_drops_the_target_and_everything_after_and_says_it_did() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "one".into() }),
                rec(2, echo("u-1")),
                rec(
                    3,
                    JournalEvent::Provider {
                        event: SessionEvent::AssistantText { text: "a".into() },
                    },
                ),
                rec(4, JournalEvent::UserSent { text: "two".into() }),
                rec(5, echo("u-2")),
                rec(
                    6,
                    JournalEvent::Provider {
                        event: SessionEvent::AssistantText { text: "b".into() },
                    },
                ),
                rec(
                    7,
                    JournalEvent::Rewound {
                        through_msg_id: "u-2".into(),
                        dropped: 1,
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            user_ids(&t),
            vec![Some("u-1".to_string())],
            "the rewound message is UNSENT — its text came back as a prefill"
        );
        assert!(
            t.rows
                .iter()
                .any(|r| matches!(r, Row::Assistant { text, .. } if text == "a")),
            "everything before the target survives: {:?}",
            t.rows
        );
        assert!(
            !t.rows
                .iter()
                .any(|r| matches!(r, Row::Assistant { text, .. } if text == "b")),
            "the discarded turn's reply must go with it: {:?}",
            t.rows
        );
        assert!(
            matches!(t.rows.last(), Some(Row::Marker { text }) if text.contains("rewound")
                && text.contains('1')),
            "rows vanishing with nothing to explain them is unreadable: {:?}",
            t.rows
        );
        assert_eq!(
            t.revs.len(),
            t.rows.len(),
            "revs is parallel to rows and must be cut with it"
        );
    }

    /// A span anchored past the cut points into rows that are gone.
    #[test]
    fn a_rewind_drops_the_turn_spans_it_invalidates() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "one".into() }),
                rec(2, echo("u-1")),
                rec(
                    3,
                    JournalEvent::Provider {
                        event: SessionEvent::TurnCompleted {
                            subtype: "success".into(),
                            is_error: false,
                        },
                    },
                ),
                rec(4, JournalEvent::UserSent { text: "two".into() }),
                rec(5, echo("u-2")),
            ],
        )
        .unwrap();
        assert_eq!(t.turns.len(), 2);
        t.apply(
            1,
            &[rec(
                6,
                JournalEvent::Rewound {
                    through_msg_id: "u-2".into(),
                    dropped: 1,
                },
            )],
        )
        .unwrap();
        assert_eq!(t.turns.len(), 1, "the dropped turn's span goes with it");
        assert!(
            t.turns.iter().all(|s| s.start_row < t.rows.len()),
            "no span may point past the rows"
        );
    }

    /// A record naming a row this client does not hold is a no-op, not a
    /// guess. Truncating to a fallback index would discard real history.
    #[test]
    fn a_rewind_naming_an_unknown_row_changes_nothing() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "one".into() }),
                rec(2, echo("u-1")),
            ],
        )
        .unwrap();
        let before = t.rows.len();
        t.apply(
            1,
            &[rec(
                3,
                JournalEvent::Rewound {
                    through_msg_id: "nobody".into(),
                    dropped: 9,
                },
            )],
        )
        .unwrap();
        assert_eq!(t.rows.len(), before);
    }

    /// **Live and cold replay must agree.** The live session folds the
    /// record as it arrives; a client opening the journal later folds the
    /// same record after everything around it. Same rows, or the
    /// transcript changes when you reopen it.
    #[test]
    fn a_replayed_rewind_lands_exactly_where_the_live_one_did() {
        let records = vec![
            rec(1, JournalEvent::UserSent { text: "one".into() }),
            rec(2, echo("u-1")),
            rec(3, JournalEvent::UserSent { text: "two".into() }),
            rec(4, echo("u-2")),
            rec(
                5,
                JournalEvent::Rewound {
                    through_msg_id: "u-2".into(),
                    dropped: 1,
                },
            ),
        ];
        let mut live = Transcript::new();
        for r in &records {
            live.apply(1, std::slice::from_ref(r)).unwrap();
        }
        let mut cold = Transcript::new();
        cold.apply(1, &records).unwrap();
        assert_eq!(live.rows, cold.rows);
        assert_eq!(live.turns.len(), cold.turns.len());
    }

    #[test]
    fn undelivered_send_is_distinct_from_sent() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(1, JournalEvent::UserSent { text: "hi".into() }),
                rec(2, JournalEvent::UserDelivered {}),
                rec(
                    3,
                    JournalEvent::UserSent {
                        text: "lost".into(),
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
        assert_eq!(
            t.rows[0],
            Row::User {
                text: "hi".into(),
                delivery: Delivery::Delivered,
                provider_msg_id: None
            }
        );
        assert_eq!(
            t.rows[1],
            Row::User {
                text: "lost".into(),
                delivery: Delivery::Undelivered,
                provider_msg_id: None
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
                            input: String::new(),
                            call: None,
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

    /// Two providers spell reasoning differently and both land in one
    /// place, with one clock.
    ///
    /// Claude sends a typed `ThoughtDelta` decoded from `thinking_delta`;
    /// ACP agents send their own `session/update` frame as `System`. If
    /// these accumulated separately the box would show one provider's
    /// reasoning and not the other's — which is exactly the state eave was
    /// in until 2026-08-30, when the Claude half was decoded to nothing.
    #[test]
    fn reasoning_from_either_wire_shape_lands_in_one_place() {
        const ACP: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"content":{"text":"and then","type":"text"},"sessionUpdate":"agent_thought_chunk"}}}"#;
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: SessionEvent::ThoughtDelta {
                            text: "first ".into(),
                        },
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: SessionEvent::System {
                            subtype: "agent_thought_chunk".into(),
                            raw: ACP.into(),
                        },
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            t.thought, "first and then",
            "one accumulator, two spellings"
        );
        assert!(
            t.thought_started_ms.is_some(),
            "the clock starts at the first chunk whoever sent it"
        );
        // Reasoning is not a message: it must not push a row.
        assert!(t.rows.is_empty(), "reasoning drew a transcript row");

        // The next prompt clears it — a thought belongs to its turn.
        t.apply(
            1,
            &[rec(
                3,
                JournalEvent::UserSent {
                    text: "next".into(),
                },
            )],
        )
        .unwrap();
        assert_eq!(t.thought, "", "a turn's reasoning outlived its turn");
        assert!(
            t.thought_started_ms.is_none(),
            "the clock outlived its turn"
        );
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

    /// A restated start is the SAME tool: one row, updated in place.
    ///
    /// The Claude CLI announces a `tool_use` with an empty `input` and
    /// sends the arguments a line later, so the adapter emits the start
    /// twice for one tool. A second row would be a lie about how many ran,
    /// and it is also how the chip's argument reaches the screen at all.
    #[test]
    fn a_restated_tool_start_updates_its_row_rather_than_adding_one() {
        let started = |input: &str, call: Option<ToolCall>| SessionEvent::ToolUseStarted {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: input.into(),
            call,
        };
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: started(
                            "{}",
                            Some(ToolCall::Run {
                                command: String::new(),
                            }),
                        ),
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: started(
                            r#"{"command":"cargo test"}"#,
                            Some(ToolCall::Run {
                                command: "cargo test".into(),
                            }),
                        ),
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(t.rows.len(), 1, "one tool ran, not two: {:?}", t.rows);
        match &t.rows[0] {
            Row::Tool {
                status,
                call,
                input,
                ..
            } => {
                assert_eq!(*status, ToolStatus::Running, "still running, not restarted");
                assert_eq!(
                    call.as_ref().and_then(ToolCall::detail).as_deref(),
                    Some("cargo test")
                );
                assert!(input.contains("cargo test"));
            }
            other => panic!("expected a tool row, got {other:?}"),
        }
    }

    /// **Slice AG1 #14.** A plan lands on the tool row that produced it,
    /// and changes exactly one thing about it.
    ///
    /// The row has already FINISHED by the time the plan arrives — Hermes
    /// sends the `plan` update immediately after the `todo` tool's
    /// completion — so this pins that the plan does not reopen the row,
    /// does not push a second one, and does not touch the result the tool
    /// actually returned. What a tool returned is not what it meant.
    #[test]
    fn a_plan_retypes_the_finished_tool_row_and_leaves_everything_else_alone() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseStarted {
                            id: "tc-caac208d6c1a".into(),
                            name: "other".into(),
                            input: r#"{"title":"todo (3 items)"}"#.into(),
                            call: Some(ToolCall::Unknown {
                                name: "other".into(),
                            }),
                        },
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseFinished {
                            id: "tc-caac208d6c1a".into(),
                            result: "**Todo list**".into(),
                            is_error: false,
                            diff: None,
                        },
                    },
                ),
                rec(
                    3,
                    JournalEvent::Provider {
                        event: SessionEvent::PlanUpdate {
                            call_id: "tc-caac208d6c1a".into(),
                            items: vec![
                                eave_core::tool::TodoItem {
                                    text: "Write the health-check handler".into(),
                                    done: true,
                                },
                                eave_core::tool::TodoItem {
                                    text: "Wire up the route".into(),
                                    done: false,
                                },
                            ],
                        },
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(t.rows.len(), 1, "one tool ran, not two: {:?}", t.rows);
        match &t.rows[0] {
            Row::Tool {
                status,
                call,
                result,
                ..
            } => {
                assert_eq!(
                    *status,
                    ToolStatus::Finished,
                    "the plan must not reopen the row"
                );
                assert_eq!(result, "**Todo list**", "the tool's own result, untouched");
                assert_eq!(
                    call.as_ref().and_then(ToolCall::detail).as_deref(),
                    Some("1/2 · Wire up the route"),
                    "the chip states progress and what is next"
                );
            }
            other => panic!("expected a tool row, got {other:?}"),
        }
    }

    /// A plan naming a call this transcript has never seen adds nothing.
    /// The decoder only emits one for a call it watched close, so this is
    /// unreachable today — and a checklist grafted onto the wrong row, or
    /// onto no row at all, is worse than one the operator never saw.
    #[test]
    fn a_plan_for_an_unknown_call_pushes_no_row() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[rec(
                1,
                JournalEvent::Provider {
                    event: SessionEvent::PlanUpdate {
                        call_id: "tc-nobody".into(),
                        items: vec![eave_core::tool::TodoItem {
                            text: "Add a test".into(),
                            done: false,
                        }],
                    },
                },
            )],
        )
        .unwrap();
        assert!(t.rows.is_empty(), "{:?}", t.rows);
    }

    /// **Slice CJ.** A child thread's frames are journaled fully typed, and
    /// the transcript still renders none of them: eave has no subagent
    /// surface, and a subagent's prose or shell command painted into the
    /// parent's transcript reads as the session's own words. Attribution
    /// exists to prevent exactly that confusion, so a slice that removed a
    /// narrowing must not accidentally add a surface.
    ///
    /// The turn end is the one that matters most — through the wrapper it
    /// must not settle the live rows or stamp the turn's outcome.
    #[test]
    fn a_child_threads_frames_render_nothing_in_the_parents_transcript() {
        let mut t = Transcript::new();
        let child = |event| JournalEvent::Provider {
            event: SessionEvent::ForeignThread {
                thread_id: "01a00fe7-e358-7a02-8745-e7a0793013ce".into(),
                event: Box::new(event),
            },
        };
        t.apply(
            1,
            &[
                rec(
                    1,
                    child(SessionEvent::TextDelta {
                        text: "beta".into(),
                    }),
                ),
                rec(
                    2,
                    child(SessionEvent::ToolUseStarted {
                        id: "exec-1".into(),
                        name: "commandExecution".into(),
                        input: r#"{"command":"echo beta"}"#.into(),
                        call: Some(ToolCall::Run {
                            command: "echo beta".into(),
                        }),
                    }),
                ),
                rec(
                    3,
                    child(SessionEvent::TurnCompleted {
                        subtype: "completed".into(),
                        is_error: false,
                    }),
                ),
            ],
        )
        .unwrap();
        assert!(
            t.rows.is_empty(),
            "no row belongs to a thread eave is not driving: {:?}",
            t.rows
        );
        assert_eq!(t.turn_tokens, None);
    }

    /// Slice AG1 #4, second half: a Claude `Edit` gains a diff from the
    /// argument it always carried, with no diff block from the provider.
    ///
    /// This is the case the ACP half could not reach, and it is the one
    /// that matters most here: Claude Code is the provider eave was built
    /// on, and until now every one of its edits rendered as a wall of
    /// result text.
    #[test]
    fn a_claude_edit_gains_its_diff_from_the_input_it_already_carried() {
        let t = edited(
            "Edit",
            r#"{"file_path":"src/a.rs","old_string":"one\ntwo\n","new_string":"one\nTWO\n"}"#,
            None,
        );
        assert_eq!(
            tool_diff(&t),
            Some(eave_core::events::ToolDiff {
                path: "src/a.rs".into(),
                old_text: "one\ntwo\n".into(),
                new_text: "one\nTWO\n".into(),
            })
        );
    }

    /// The provider's own diff always wins. It reports what the edit DID;
    /// the input only says what was asked for.
    #[test]
    fn a_reported_diff_is_never_replaced_by_the_derived_one() {
        let reported = eave_core::events::ToolDiff {
            path: "reported.rs".into(),
            old_text: "a\n".into(),
            new_text: "b\n".into(),
        };
        let t = edited(
            "Edit",
            r#"{"file_path":"asked.rs","old_string":"x\n","new_string":"y\n"}"#,
            Some(reported.clone()),
        );
        assert_eq!(tool_diff(&t), Some(reported));
    }

    /// `Write` gets nothing. Its input has `content` and no before-text, so
    /// the only diff derivable from it draws every line as an addition —
    /// true if the file was created, a lie if it was overwritten, and the
    /// input does not say which.
    #[test]
    fn a_write_gets_no_derived_diff_because_it_states_no_before_text() {
        let t = edited(
            "Write",
            r#"{"file_path":"src/a.rs","content":"one\ntwo\n"}"#,
            None,
        );
        assert_eq!(tool_diff(&t), None);
    }

    /// MultiEdit shows the first edit only — `first_diff`'s rule, for
    /// `first_diff`'s reason: merging them would invent a change nobody
    /// made, and taking the largest would make the row depend on size.
    #[test]
    fn a_multi_edit_draws_its_first_edit_and_does_not_merge_them() {
        let t = edited(
            "MultiEdit",
            r#"{"file_path":"src/a.rs","edits":[
                {"old_string":"one\n","new_string":"ONE\n"},
                {"old_string":"two\n","new_string":"TWO\n"}]}"#,
            None,
        );
        let d = tool_diff(&t).expect("a multi-edit drew nothing");
        assert_eq!(d.old_text, "one\n");
        assert_eq!(d.new_text, "ONE\n", "the second edit was merged in");
    }

    /// An edit whose strings match changed nothing, and a diff pane stating
    /// that a file stayed as it was is noise, not a fact worth a pane.
    #[test]
    fn an_edit_that_changed_nothing_draws_no_diff() {
        let t = edited(
            "Edit",
            r#"{"file_path":"src/a.rs","old_string":"same\n","new_string":"same\n"}"#,
            None,
        );
        assert_eq!(tool_diff(&t), None);
    }

    /// The restated start is what carries the arguments (Claude announces
    /// the tool with `{}` and fills it a line later), so the derivation
    /// must read the row's CURRENT input, not the one it was born with.
    /// Without the in-place update this row would finish holding `{}`.
    #[test]
    fn the_derived_diff_reads_the_restated_input_not_the_empty_first_one() {
        let started = |input: &str| SessionEvent::ToolUseStarted {
            id: "t1".into(),
            name: "Edit".into(),
            input: input.into(),
            call: None,
        };
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: started("{}"),
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: started(
                            r#"{"file_path":"src/a.rs","old_string":"one\n","new_string":"ONE\n"}"#,
                        ),
                    },
                ),
                rec(
                    3,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseFinished {
                            id: "t1".into(),
                            result: "ok".into(),
                            is_error: false,
                            diff: None,
                        },
                    },
                ),
            ],
        )
        .unwrap();
        assert!(
            tool_diff(&t).is_some(),
            "the finish read the placeholder input, not the restated one"
        );
    }

    /// A tool that never finished holds no diff: an edit's row gains it
    /// when the tool settles, the same moment the ACP half does.
    #[test]
    fn a_running_edit_holds_no_diff_yet() {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[rec(
                1,
                JournalEvent::Provider {
                    event: SessionEvent::ToolUseStarted {
                        id: "t1".into(),
                        name: "Edit".into(),
                        input: r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#.into(),
                        call: None,
                    },
                },
            )],
        )
        .unwrap();
        assert_eq!(tool_diff(&t), None);
    }

    /// One `Edit` start, one finish, and the diff the finish ended up with.
    fn edited(
        name: &str,
        input: &str,
        reported: Option<eave_core::events::ToolDiff>,
    ) -> Transcript {
        let mut t = Transcript::new();
        t.apply(
            1,
            &[
                rec(
                    1,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseStarted {
                            id: "t1".into(),
                            name: name.into(),
                            input: input.into(),
                            call: None,
                        },
                    },
                ),
                rec(
                    2,
                    JournalEvent::Provider {
                        event: SessionEvent::ToolUseFinished {
                            id: "t1".into(),
                            result: "ok".into(),
                            is_error: false,
                            diff: reported,
                        },
                    },
                ),
            ],
        )
        .unwrap();
        t
    }

    fn tool_diff(t: &Transcript) -> Option<eave_core::events::ToolDiff> {
        t.rows.iter().find_map(|r| match r {
            Row::Tool { diff, .. } => diff.clone(),
            _ => None,
        })
    }
}
