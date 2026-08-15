//! Provider-agnostic session events, produced by an adapter's stream parser
//! and consumed by the engine (and, later, folded into the journal — spec 02).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Provider announced itself; `session_id` is the provider-native resume
    /// cursor (spec 02 §Resume contract).
    Init { session_id: String },
    /// Incremental assistant text (from partial-message stream events).
    TextDelta { text: String },
    /// A complete assistant text block (the post-stream snapshot message).
    AssistantText { text: String },
    /// A tool call became outstanding. Emitted once per tool id.
    ToolUseStarted { id: String, name: String },
    /// A previously outstanding tool call resolved.
    ToolUseFinished { id: String },
    /// The CLI echoed a user message back (`--replay-user-messages`).
    UserEcho { text: String },
    /// Provider-terminal turn event (`result` line).
    TurnCompleted { subtype: String, is_error: bool },
    /// A line that was not valid JSON or lacked a type tag. Surfaced, never
    /// fatal — the parser keeps going (a misbehaving CLI must not take the
    /// session state machine down with it).
    Malformed { line: String, error: String },
    /// A well-formed line of a type this parser does not know. Kept visible
    /// rather than silently dropped.
    Unknown { event_type: String },
    /// The child exited. Always the last event on the channel.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        stderr_tail: String,
    },
}
