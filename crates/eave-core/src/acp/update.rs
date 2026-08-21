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
}

impl AcpStream {
    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    pub fn is_outstanding(&self, id: &str) -> bool {
        self.outstanding.iter().any(|o| o == id)
    }

    /// Take every tool still open, in the order they were started.
    pub fn take_outstanding(&mut self) -> Vec<String> {
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
            "user_message_chunk" => vec![SessionEvent::UserEcho {
                text: chunk_text(update).unwrap_or_default(),
            }],
            "tool_call" => {
                let Some(id) = tool_call_id(update) else {
                    return vec![malformed(line, "tool_call without toolCallId".into())];
                };
                if !self.is_outstanding(&id) {
                    self.outstanding.push(id.clone());
                }
                vec![SessionEvent::ToolUseStarted {
                    id,
                    name: tool_name(spec.tool_naming, update),
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
                vec![SessionEvent::ToolUseFinished {
                    id,
                    result: cap_payload(&flatten_content(update.get("content"))),
                    // The agent's own verdict, not an inference from text.
                    is_error,
                }]
            }
            _ => vec![system(update, line)],
        }
    }
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
