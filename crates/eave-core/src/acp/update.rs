//! `session/update` → [`SessionEvent`].
//!
//! ACP's streaming notification carries seven or so `sessionUpdate` kinds,
//! five of which mean the same thing to eave no matter which agent sent
//! them. Those five live here, parameterised by the handful of places the
//! agents genuinely differ — where the tool's NAME is on the wire, what
//! counts as its input, and whether reasoning is journaled.
//!
//! The kinds an agent owns (Grok's `turn_completed`, Hermes'
//! `current_mode_update`) are matched by the adapter before it delegates
//! here; anything this decoder has never heard of becomes
//! `SessionEvent::System` with the line verbatim, so a wire addition
//! reaches the journal without a parser change.

use serde_json::Value;

use crate::acp::wire::malformed;
use crate::events::{cap_payload, SessionEvent};
use crate::tool::{self, ToolCall};

/// Where an agent puts the tool's real name.
#[derive(Clone, Copy)]
pub enum ToolNaming {
    /// The vendor's own name from `_meta.<key>.name`, falling back to the
    /// human `title`. Grok puts the real tool name on the wire, so eave
    /// shows the real tool name.
    VendorMeta(&'static str),
    /// ACP's enumerated `kind`.
    ///
    /// For Hermes the tool name is not on the wire at all: for the tools
    /// that matter (`terminal`, `write_file`, `patch`, `read_file`) the
    /// adapter passes no `rawInput` and folds the name into a human title
    /// like `"terminal: cargo test"`. Splitting that on `": "` would
    /// recover the name most of the time and silently invent one the rest
    /// of the time. `kind` is an enumerated protocol value that is always
    /// present and never a guess.
    ProtocolKind,
}

/// What eave records as a tool call's input.
#[derive(Clone, Copy)]
pub enum ToolInput {
    /// ACP's `rawInput` — the arguments, and nothing else.
    RawInput,
    /// The whole update. For an agent that sends no `rawInput`, the update
    /// is the only place the human title survives.
    WholeUpdate,
}

/// Whether the agent's reasoning reaches the journal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Thoughts {
    /// Dropped entirely.
    Dropped,
    /// Kept verbatim as `System`, never as assistant text — recording
    /// chain-of-thought as the reply would put words in the agent's mouth.
    Journaled,
}

/// The per-agent knobs of the shared decoder.
pub struct UpdateSpec {
    pub tool_naming: ToolNaming,
    pub tool_input: ToolInput,
    pub thoughts: Thoughts,
    /// Journal a non-terminal `tool_call_update` (`pending`/`in_progress`)
    /// as `System`. Either way it never closes the call.
    pub progress_events: bool,
    /// Accumulate assistant deltas so the adapter can flush one
    /// `AssistantText` when the turn settles. Only agents whose turn end
    /// arrives as an RPC reply need this.
    pub accumulate_turn_text: bool,
}

/// The protocol's own stream state: which tool calls are outstanding, and
/// what the turn in progress has said.
///
/// A `Vec` rather than a set because the order tools were started in is
/// the order they are reported closed when a turn ends without resolving
/// them; the `contains` guard makes it a set in every way that matters.
#[derive(Default)]
pub struct AcpStream {
    outstanding: Vec<String>,
    turn_text: String,
    /// How many replayed `session/update`s have been absorbed since
    /// `session/load` went out, or `None` when nothing is being replayed.
    ///
    /// **Why a gate is needed at all.** ACP's `session/load` answers by
    /// re-sending the conversation as ordinary `session/update`
    /// notifications, which arrive BEFORE the load response. They are
    /// indistinguishable from live traffic by shape — the same
    /// `agent_message_chunk`, `tool_call`, `turn_completed` — so without a
    /// gate a resumed session appends its entire history to the journal a
    /// second time, as if the agent had just said all of it again.
    ///
    /// comet drops them too, for a stated reason that is worth repeating
    /// because eave's is the same: *"the doc already holds the history"*
    /// (`comet/crates/harness/src/acp/mod.rs`, `request_draining`). eave's
    /// journal holds it, and the journal is what replays on cold start.
    ///
    /// This was unreachable until slice CD. Before it, the engine resumed
    /// under eave's own uuid — an id neither agent ever issued — so
    /// `session/load` always errored and no replay was ever received.
    /// Fixing resume is what made this live.
    replaying: Option<u32>,
    /// Tool calls eave could not type, in start order (Slice AG1 #14).
    ///
    /// A `plan` update names no tool call, so the decoder has to know
    /// which one it belongs to. Keeping only the UNTYPED ones is what
    /// makes that safe: a plan may fill a row eave had nothing better
    /// for, and can never turn a `cargo test` row into a checklist.
    untyped: Vec<String>,
    /// The tool call that closed most recently, when eave could not type
    /// it. Cleared by any typed close, because the plan belongs to the
    /// call that just ended and not to an older one.
    last_untyped_close: Option<String>,
}

impl AcpStream {
    /// Arm the replay gate. Called when `session/load` goes out — and only
    /// then: a fresh `session/new` replays nothing, so arming it there
    /// would swallow the first live turn.
    pub fn begin_replay(&mut self) {
        self.replaying = Some(0);
    }

    /// Should this update be absorbed as replay rather than decoded?
    ///
    /// Counts as it absorbs, so the session can state how much history it
    /// skipped instead of skipping it silently.
    pub fn replay_absorbs(&mut self) -> bool {
        match &mut self.replaying {
            Some(n) => {
                *n += 1;
                true
            }
            None => false,
        }
    }

    /// Disarm the gate at the `session/load` response, and say what was
    /// absorbed.
    ///
    /// The count is journaled rather than discarded because "eave dropped
    /// 412 replayed updates here" is a fact about the resume, and a silent
    /// drop is indistinguishable from a provider that replayed nothing —
    /// which is what a broken `session/load` also looks like.
    pub fn end_replay(&mut self) -> Option<SessionEvent> {
        let absorbed = self.replaying.take()?;
        Some(SessionEvent::System {
            subtype: "acp_replay_absorbed".into(),
            raw: serde_json::json!({
                "absorbed": absorbed,
                "why": "session/load re-sends the conversation as live updates; \
                        the journal already holds it",
            })
            .to_string(),
        })
    }

    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    pub fn is_outstanding(&self, id: &str) -> bool {
        self.outstanding.iter().any(|o| o == id)
    }

    /// Take every tool still open, in the order they were started.
    pub fn take_outstanding(&mut self) -> Vec<String> {
        // Slice AG1 #14: a call swept up as an orphan will never close,
        // so it can never be named by a plan. Dropping the ids with it
        // is what keeps `untyped` a mirror of `outstanding` rather than
        // a list that only ever grows over a long session.
        self.untyped.clear();
        self.last_untyped_close = None;
        std::mem::take(&mut self.outstanding)
    }

    /// Take the turn's assistant text, leaving the buffer empty.
    pub fn take_turn_text(&mut self) -> String {
        std::mem::take(&mut self.turn_text)
    }

    /// Decode one `params.update` payload. `line` is the raw frame, kept
    /// verbatim on anything this decoder does not model.
    pub fn decode(&mut self, spec: &UpdateSpec, update: &Value, line: &str) -> Vec<SessionEvent> {
        match kind_of(update) {
            "agent_message_chunk" => match chunk_text(update) {
                Some(text) => {
                    if spec.accumulate_turn_text {
                        self.turn_text.push_str(&text);
                    }
                    vec![SessionEvent::TextDelta { text }]
                }
                None => Vec::new(),
            },
            "agent_thought_chunk" => match spec.thoughts {
                Thoughts::Dropped => Vec::new(),
                Thoughts::Journaled => vec![system(update, line)],
            },
            // ACP names a user message nothing the client can address
            // later, so there is no id to carry — see `UserEcho`.
            "user_message_chunk" => vec![SessionEvent::UserEcho {
                text: chunk_text(update).unwrap_or_default(),
                provider_msg_id: None,
            }],
            "tool_call" => {
                let Some(id) = tool_call_id(update) else {
                    return vec![malformed(line, "tool_call without toolCallId".into())];
                };
                if !self.is_outstanding(&id) {
                    self.outstanding.push(id.clone());
                }
                // Slice CB x CA: typing happens here, once, for both ACP
                // agents. It reads the name the spec resolved rather than
                // re-deriving it, so Grok's `_meta["x.ai/tool"]` and
                // Hermes' title convention each reach the decoder already
                // settled.
                let name = tool_name(spec.tool_naming, update);
                let call = decode_acp_tool(update, &name);
                // Slice AG1 #14: remember whether eave managed to type
                // this call, so a `plan` that follows its close knows
                // whether it is allowed to speak for the row. A restated
                // start that DOES type revokes the permission.
                match (
                    matches!(call, ToolCall::Unknown { .. }),
                    self.untyped.iter().position(|u| u == &id),
                ) {
                    (true, None) => self.untyped.push(id.clone()),
                    (false, Some(at)) => {
                        self.untyped.remove(at);
                    }
                    _ => {}
                }
                vec![SessionEvent::ToolUseStarted {
                    call: Some(call),
                    id,
                    name,
                    input: tool_input(spec.tool_input, update),
                }]
            }
            "tool_call_update" => {
                let Some(id) = tool_call_id(update) else {
                    return vec![malformed(
                        line,
                        "tool_call_update without toolCallId".into(),
                    )];
                };
                // An agent sends several updates per call — title
                // refinements, diffs, progress. Only a terminal status
                // closes one; treating a refinement as a completion would
                // settle the row while the tool was still running.
                let status = update.get("status").and_then(Value::as_str);
                let is_error = match status {
                    Some("completed") => false,
                    Some("failed") => true,
                    _ => {
                        return if spec.progress_events {
                            vec![SessionEvent::System {
                                subtype: format!(
                                    "tool_call_progress:{}",
                                    status.unwrap_or_default()
                                ),
                                raw: line.to_string(),
                            }]
                        } else {
                            Vec::new()
                        };
                    }
                };
                self.outstanding.retain(|o| o != &id);
                // Slice AG1 #14. Hermes sends the `plan` immediately after
                // the `todo` tool's completion, from the same synchronous
                // sequence, at both of its call sites
                // (`acp_adapter/events.py:253`, `server.py:1587`, each
                // gated on `tool_name == "todo"`). That adjacency is what
                // the plan arm reads; a typed close clears it.
                self.last_untyped_close = match self.untyped.iter().position(|u| u == &id) {
                    Some(at) => {
                        self.untyped.remove(at);
                        Some(id.clone())
                    }
                    None => None,
                };
                let content = update.get("content");
                vec![SessionEvent::ToolUseFinished {
                    id,
                    result: cap_payload(&flatten_content(content)),
                    // The agent's own verdict, not an inference from text.
                    is_error,
                    // Slice AG1 #4: keep the OTHER half of the diff block.
                    // `flatten_content` above takes `newText` as the result
                    // and is right to — what the file became is what the
                    // tool returned. This keeps the before-text beside it
                    // so a row can draw the change rather than the outcome.
                    diff: first_diff(content),
                }]
            }
            // Slice AG1 #14. ACP's own plan update, and the only
            // STRUCTURED statement of an agent's checklist on this wire —
            // the `todo` tool call beside it carries its items as a human
            // preview string (`- pending: Wire up the route`) and nothing
            // else, so reading the plan here is not a preference between
            // two sources, it is the difference between decoding a fact
            // and re-parsing prose.
            //
            // A plan eave cannot place still reaches the journal verbatim,
            // which is what it did before this arm existed. eave does not
            // invent a row for it: a checklist attached to the wrong tool
            // call is worse than one the operator never saw.
            "plan" => match self.last_untyped_close.take() {
                Some(call_id) => vec![SessionEvent::PlanUpdate {
                    call_id,
                    items: plan_items(update),
                }],
                None => vec![system(update, line)],
            },
            // Slice CC: ACP's own context gauge. `used` is "Tokens
            // currently in context" and `size` is "Total context window
            // size in tokens" (`_UsageUpdate`, agent-client-protocol 1.x);
            // Grok's serde table names the same four fields in `struct
            // UsageUpdate with 4 elements` (1.0.5 binary, read offline).
            // An update with no `used` is not a gauge, so it falls through
            // to the verbatim record rather than becoming a zero.
            "usage_update" => match crate::acp::usage::from_usage_update(update) {
                Some(ev) => vec![ev],
                None => vec![system(update, line)],
            },
            _ => vec![system(update, line)],
        }
    }
}

/// ACP `plan.entries` → eave's checklist (Slice AG1 #14).
///
/// **Two states, not three.** `in_progress` is not done — the same fold
/// [`crate::stream`] applies to Claude's `TodoWrite`, and for the same
/// reason: a third mark would be a state eave INVENTED, and the two
/// providers would then disagree about a word neither of them sent.
///
/// **`priority` is read and dropped.** Hermes writes the constant
/// `"medium"` on every entry it builds (`acp_adapter/events.py`), so a
/// card that drew priority would be drawing a constant. When some agent
/// varies it, this is where it becomes a fact.
///
/// **`cancelled` never reaches here.** Hermes maps it to `completed` and
/// prefixes the content with `[cancelled] `, so a cancelled item arrives
/// as done with its cancellation in the text. eave renders that verbatim
/// rather than trying to be cleverer than the agent about its own plan.
///
/// An entry with no `content` is dropped rather than counted: it has
/// nothing to render, and a checklist that says `0/4` while showing three
/// lines is a worse answer than one that says `0/3`.
pub fn plan_items(update: &Value) -> Vec<crate::tool::TodoItem> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let text = entry.get("content").and_then(Value::as_str)?;
            if text.is_empty() {
                return None;
            }
            Some(crate::tool::TodoItem {
                text: text.to_string(),
                // The agent's own word.
                done: entry.get("status").and_then(Value::as_str) == Some("completed"),
            })
        })
        .collect()
}

/// The `sessionUpdate` discriminator, so an adapter can claim its own
/// kinds before delegating the rest.
pub fn kind_of(update: &Value) -> &str {
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn system(update: &Value, line: &str) -> SessionEvent {
    SessionEvent::System {
        subtype: kind_of(update).to_string(),
        raw: line.to_string(),
    }
}

fn tool_call_id(update: &Value) -> Option<String> {
    update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn chunk_text(update: &Value) -> Option<String> {
    let text = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)?;
    (!text.is_empty()).then(|| text.to_string())
}

fn tool_name(naming: ToolNaming, update: &Value) -> String {
    let title = || {
        update
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match naming {
        ToolNaming::VendorMeta(key) => update
            .get("_meta")
            .and_then(|m| m.get(key))
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(title),
        ToolNaming::ProtocolKind => update
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string(),
    }
}

fn tool_input(input: ToolInput, update: &Value) -> String {
    let v = match input {
        ToolInput::RawInput => update.get("rawInput").cloned().unwrap_or(Value::Null),
        ToolInput::WholeUpdate => update.clone(),
    };
    cap_payload(&v.to_string())
}

/// Flatten an ACP `ToolCallContent` array to text, so a tool result reads
/// as a result rather than as protocol.
///
/// The agents mix shapes: `{type:"content", content:{type:"text", text}}`
/// for output and `{type:"diff", path, oldText, newText}` for edits. A
/// diff's own bytes ARE the result — what the file became. Anything else
/// (terminal refs, block types added later) is kept as its own JSON, so
/// the record still says what happened instead of silently becoming an
/// empty string.
/// The FIRST `{type:"diff", path, oldText, newText}` block in a tool call's
/// content, as a [`ToolDiff`] (Slice AG1 #4).
///
/// **First, not merged.** An agent may report several diffs in one call.
/// Concatenating them would invent a change nobody made — two hunks in
/// different files are not one diff — and picking the largest would make
/// the row's meaning depend on content size. The first is the one the
/// agent led with, and a row that shows one honest change beats a row that
/// shows a synthesised one. A multi-file edit still says what it did
/// through its result text.
///
/// `oldText` absent is NOT the same as absent-diff: ACP omits it when the
/// tool created the file, so it reads as empty and the row draws a pure
/// addition, which is what happened.
pub fn first_diff(content: Option<&Value>) -> Option<crate::events::ToolDiff> {
    let items = content.and_then(Value::as_array)?;
    items.iter().find_map(|item| {
        if item.get("type").and_then(Value::as_str) != Some("diff") {
            return None;
        }
        Some(crate::events::ToolDiff {
            path: item.get("path").and_then(Value::as_str)?.to_string(),
            old_text: cap_payload(
                item.get("oldText").and_then(Value::as_str).unwrap_or(""),
            ),
            new_text: cap_payload(
                item.get("newText").and_then(Value::as_str).unwrap_or(""),
            ),
        })
    })
}

pub fn flatten_content(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let piece = match item.get("type").and_then(Value::as_str) {
            Some("content") => item
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string),
            Some("diff") => item
                .get("newText")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        out.push(piece.unwrap_or_else(|| item.to_string()));
    }
    out.join("\n")
}

/// Decode an ACP `tool_call` update into a typed [`ToolCall`], keyed by
/// the agent's own tool name first and the protocol's `kind` second.
///
/// The name comes first because **Grok omits `kind` on every `tool_call`
/// update** — it sends one on `session/request_permission`, but not on the
/// stream — so the protocol field alone would type every Grok tool as
/// `Unknown`. Hermes does send `kind`, and falls through to
/// [`tool::from_acp_update`] for everything not named here.
///
/// Both named cases are correct for either agent, which is why they live
/// in the shared decoder rather than in a spec field: a tool called
/// `write` writes a file whoever sent it, and only Grok has
/// `spawn_subagent` at all.
///
/// A tool that types to nothing keeps the AGENT's own name on
/// [`ToolCall::Unknown`] rather than the protocol's empty default — never
/// a guess, and never a blank.
fn decode_acp_tool(update: &Value, name: &str) -> ToolCall {
    let raw = update.get("rawInput").filter(|v| !v.is_null());
    match name {
        // eave has no subagent UI, so unlike comet — which names the chip
        // `Agent: {description}` because that string also names the tab it
        // opens — this is a plain `Task` and nothing keys off it.
        "spawn_subagent" => ToolCall::Task {
            description: raw
                .and_then(|r| tool::text(r, "description"))
                .unwrap_or_default(),
        },
        "write" => ToolCall::Write {
            path: raw
                .and_then(|r| tool::text_any(r, &["file_path", "path", "filePath"]))
                .unwrap_or_default(),
        },
        _ => match tool::from_acp_update(update) {
            ToolCall::Unknown { .. } => ToolCall::Unknown { name: name.into() },
            typed => typed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Hermes' knobs, copied from `hermes.rs` rather than imported so a
    /// change there shows up here as a failing assertion about a real
    /// captured frame instead of silently re-parameterising the test.
    fn hermes() -> UpdateSpec {
        UpdateSpec {
            tool_naming: ToolNaming::ProtocolKind,
            tool_input: ToolInput::WholeUpdate,
            thoughts: Thoughts::Journaled,
            progress_events: false,
            accumulate_turn_text: false,
        }
    }

    fn decode(stream: &mut AcpStream, update: Value) -> Vec<SessionEvent> {
        let line = update.to_string();
        stream.decode(&hermes(), &update, &line)
    }

    /// The three frames Hermes 0.20.4 actually sent, captured 2026-08-30
    /// from one `todo` call driven through `hermes acp` over stdio
    /// (`docs/reports/stage-ag1-14-plan-decoder.md`). Verbatim: the
    /// preview strings, the empty `locations`, the absent `rawInput` and
    /// the constant `priority` are all part of what is under test.
    fn captured_todo_call() -> Value {
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-caac208d6c1a",
            "title": "todo (3 items)",
            "kind": "other",
            "locations": [],
            "content": [{"type": "content", "content": {"type": "text", "text":
                "Updating todo list\n\n- in_progress: Write the health-check handler\n\
                 - pending: Wire up the route\n- pending: Add a test"}}],
        })
    }

    fn captured_todo_close() -> Value {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-caac208d6c1a",
            "kind": "other",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text":
                "**Todo list**\n\n- 🔄 Write the health-check handler\n\
                 - ⏳ Wire up the route\n- ⏳ Add a test\n\n\
                 **Progress:** 0 completed, 1 in progress, 2 pending"}}],
        })
    }

    fn captured_plan() -> Value {
        json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "Write the health-check handler",
                 "priority": "medium", "status": "in_progress"},
                {"content": "Wire up the route", "priority": "medium", "status": "pending"},
                {"content": "Add a test", "priority": "medium", "status": "pending"},
            ],
        })
    }

    /// The whole point of the slice, on the frames the agent really sends.
    ///
    /// Note what the tool call itself gives eave: `kind: "other"`, no
    /// `rawInput`, and its items only inside a human preview string. That
    /// is why the plan is read instead — not a preference between two
    /// sources, but the difference between decoding a fact and re-parsing
    /// prose.
    #[test]
    fn the_hermes_todo_sequence_puts_the_plan_on_the_row_it_belongs_to() {
        let mut stream = AcpStream::default();

        let started = decode(&mut stream, captured_todo_call());
        assert!(
            matches!(
                &started[..],
                [SessionEvent::ToolUseStarted {
                    call: Some(ToolCall::Unknown { .. }),
                    ..
                }]
            ),
            "eave cannot type Hermes' todo call from the wire: {started:?}"
        );

        let closed = decode(&mut stream, captured_todo_close());
        assert!(matches!(
            &closed[..],
            [SessionEvent::ToolUseFinished {
                is_error: false,
                ..
            }]
        ));

        let plan = decode(&mut stream, captured_plan());
        let [SessionEvent::PlanUpdate { call_id, items }] = &plan[..] else {
            panic!("the plan did not land on a tool call: {plan:?}");
        };
        assert_eq!(call_id, "tc-caac208d6c1a", "the call that just closed");
        assert_eq!(
            items,
            &vec![
                crate::tool::TodoItem {
                    text: "Write the health-check handler".into(),
                    done: false,
                },
                crate::tool::TodoItem {
                    text: "Wire up the route".into(),
                    done: false,
                },
                crate::tool::TodoItem {
                    text: "Add a test".into(),
                    done: false,
                },
            ],
            "`in_progress` is not done — two states, never three"
        );
    }

    /// A plan is consumed once. A second one with nothing new to attach to
    /// is a plan eave cannot place, and it goes to the journal rather than
    /// onto whichever row happens to be last.
    #[test]
    fn a_plan_is_placed_once_and_never_reused() {
        let mut stream = AcpStream::default();
        decode(&mut stream, captured_todo_call());
        decode(&mut stream, captured_todo_close());
        assert!(matches!(
            &decode(&mut stream, captured_plan())[..],
            [SessionEvent::PlanUpdate { .. }]
        ));
        assert!(
            matches!(
                &decode(&mut stream, captured_plan())[..],
                [SessionEvent::System { subtype, .. }] if subtype == "plan"
            ),
            "a repeat plan has no call left to speak for"
        );
    }

    /// The guard that makes reading adjacency safe. A plan may fill a row
    /// eave had nothing better for; it may never turn a `cargo test` row
    /// into a checklist.
    #[test]
    fn a_plan_never_speaks_for_a_row_eave_could_type_itself() {
        let mut stream = AcpStream::default();
        let run = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-run",
            "title": "terminal: cargo test",
            "kind": "execute",
        });
        assert!(
            matches!(
                &decode(&mut stream, run)[..],
                [SessionEvent::ToolUseStarted {
                    call: Some(ToolCall::Run { .. }),
                    ..
                }]
            ),
            "the fixture must be a call eave CAN type, or the test proves nothing"
        );
        decode(
            &mut stream,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc-run",
                   "kind": "execute", "status": "completed"}),
        );
        assert!(
            matches!(
                &decode(&mut stream, captured_plan())[..],
                [SessionEvent::System { subtype, .. }] if subtype == "plan"
            ),
            "a typed close must not be claimed by the plan that follows it"
        );
    }

    /// A typed close CLEARS an earlier untyped one: the plan belongs to
    /// the call that just ended, not to an older one still lying around.
    #[test]
    fn a_later_typed_close_takes_the_plan_away_from_an_older_untyped_one() {
        let mut stream = AcpStream::default();
        decode(&mut stream, captured_todo_call());
        decode(&mut stream, captured_todo_close());
        decode(
            &mut stream,
            json!({"sessionUpdate": "tool_call", "toolCallId": "tc-run",
                   "title": "terminal: cargo test", "kind": "execute"}),
        );
        decode(
            &mut stream,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc-run",
                   "kind": "execute", "status": "completed"}),
        );
        assert!(matches!(
            &decode(&mut stream, captured_plan())[..],
            [SessionEvent::System { .. }]
        ));
    }

    /// Before this arm existed a plan reached the journal verbatim, and
    /// that is still what happens to one eave cannot place. The decoder
    /// gained a case; it did not lose the record.
    #[test]
    fn a_plan_with_nothing_to_place_it_on_is_still_journalled_verbatim() {
        let mut stream = AcpStream::default();
        let out = decode(&mut stream, captured_plan());
        let [SessionEvent::System { subtype, raw }] = &out[..] else {
            panic!("expected the verbatim record: {out:?}");
        };
        assert_eq!(subtype, "plan");
        assert!(
            raw.contains("Wire up the route"),
            "the frame, byte for byte"
        );
    }

    /// A cleared plan is a fact — `entries: []` means the agent dropped
    /// its checklist, which is not the same as never having had one.
    #[test]
    fn a_cleared_plan_is_an_empty_checklist_not_a_missing_one() {
        let mut stream = AcpStream::default();
        decode(&mut stream, captured_todo_call());
        decode(&mut stream, captured_todo_close());
        let out = decode(&mut stream, json!({"sessionUpdate": "plan", "entries": []}));
        assert!(matches!(
            &out[..],
            [SessionEvent::PlanUpdate { items, .. }] if items.is_empty()
        ));
    }

    /// `untyped` is a mirror of `outstanding`, not a list that only grows.
    ///
    /// A tool the agent never reports on is swept up as an orphan when the
    /// turn ends, and a call that can never close can never be named by a
    /// plan. Without this a long session accumulates one id per unresolved
    /// `other`-kind call, forever.
    #[test]
    fn the_orphan_sweep_takes_the_untyped_ids_with_it() {
        let mut stream = AcpStream::default();
        decode(&mut stream, captured_todo_call());
        assert_eq!(
            stream.take_outstanding(),
            vec!["tc-caac208d6c1a".to_string()]
        );
        assert!(stream.untyped.is_empty(), "{:?}", stream.untyped);
        assert!(matches!(
            &decode(&mut stream, captured_plan())[..],
            [SessionEvent::System { .. }]
        ));
    }

    /// Hermes maps `cancelled` to `completed` and moves the cancellation
    /// into the text. eave renders that verbatim rather than inventing a
    /// third mark for a state the wire cannot express.
    #[test]
    fn a_cancelled_entry_arrives_done_with_its_cancellation_in_the_text() {
        let items = plan_items(&json!({"entries": [
            {"content": "[cancelled] Wire up the route", "priority": "medium",
             "status": "completed"},
            {"content": "Add a test", "priority": "medium", "status": "in_progress"},
            {"content": "", "priority": "medium", "status": "pending"},
        ]}));
        assert_eq!(
            items,
            vec![
                crate::tool::TodoItem {
                    text: "[cancelled] Wire up the route".into(),
                    done: true,
                },
                crate::tool::TodoItem {
                    text: "Add a test".into(),
                    done: false,
                },
            ],
            "an entry with no content has nothing to render and is not counted"
        );
    }
}
