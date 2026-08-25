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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    User {
        text: String,
        delivery: Delivery,
    },
    /// Assistant text; `live` while streaming deltas, settled by the
    /// snapshot or a turn end.
    Assistant {
        text: String,
        live: bool,
    },
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
    Marker {
        text: String,
    },
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
            JournalEvent::UserSent { text } => {
                self.thought.clear();
                self.thought_started_ms = None;
                self.push_row(Row::User {
                    text: text.clone(),
                    delivery: Delivery::Pending,
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
                }),
            },
            SessionEvent::ToolUseFinished {
                id,
                result,
                is_error,
            } => {
                self.finish_tool(id, result, *is_error);
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
                if subtype == "agent_thought_chunk" {
                    if let Some(text) = thought_text(raw) {
                        if self.thought_started_ms.is_none() {
                            self.thought_started_ms = Some(eave_core::journal::now_ms());
                        }
                        self.thought.push_str(&text);
                    }
                }
            }
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
    fn finish_tool(&mut self, id: &str, result: &str, is_error: bool) {
        for ix in 0..self.rows.len() {
            if let Row::Tool {
                id: rid,
                status,
                result: r,
                is_error: e,
                ..
            } = &mut self.rows[ix]
            {
                if rid == id {
                    *status = ToolStatus::Finished;
                    *r = result.to_string();
                    *e = is_error;
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
}
