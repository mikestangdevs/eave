//! Provider-agnostic session events, produced by an adapter's stream parser
//! and consumed by the engine (and folded into the journal — spec 02).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    /// A `system` line other than `init`: `status`, `task_started`,
    /// `task_notification`, and anything the CLI adds later. `raw` is the
    /// original line byte-for-byte — the journal persists it verbatim so
    /// Stage E can consume these and future CLI additions are recorded
    /// without a parser change (build directive 2026-08-15; research:
    /// claude-stream-notes.md).
    System { subtype: String, raw: String },
    /// A line that was not valid JSON or lacked a type tag. Surfaced, never
    /// fatal — the parser keeps going (a misbehaving CLI must not take the
    /// session state machine down with it).
    Malformed { line: String, error: String },
    /// A well-formed line of a top-level type this parser does not know
    /// (e.g. `rate_limit_event`). `raw` carries the original line verbatim
    /// for the journal, same rationale as `System`.
    Unknown { event_type: String, raw: String },
    /// The child exited. Always the last event on the channel.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        stderr_tail: String,
    },
}
