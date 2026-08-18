//! Provider-agnostic session events, produced by an adapter's stream parser
//! and consumed by the engine (and folded into the journal — spec 02).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Provider announced itself; `session_id` is the provider-native resume
    /// cursor (spec 02 §Resume contract). `permission_mode` is the posture
    /// the CLI reports it is actually running in (Slice K) — the fact eave
    /// verifies before letting the session do anything, and the fact the
    /// rail states. `None` = the CLI did not say, which Slice K treats as
    /// unproven, never as "probably fine" (spec 04 §Permissions).
    Init {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
    },
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
    /// The agent asked permission to run one tool and is now parked on the
    /// answer (Slice K). Provider-neutral by construction: the Claude
    /// adapter owns the `control_request {can_use_tool}` encoding, this
    /// says only what a human needs to decide. The session waits
    /// indefinitely — eave never auto-approves, never auto-denies, never
    /// times out into either.
    ApprovalRequested {
        /// The provider's own id for the question; the decision names it.
        request_id: String,
        /// Tool name as the provider calls it (`Write`, `Bash`, …).
        tool: String,
        /// The provider's human-facing label for the tool.
        display_name: String,
        /// One-line summary of what the call does, from the provider.
        /// May be empty — the input is the fact of record.
        description: String,
        /// The concrete filesystem path the action would touch, when the
        /// provider names one. This is the fact an operator most needs and
        /// it is NOT always derivable from the input (a `Bash` redirect
        /// names its path only here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The tool's full input, verbatim JSON. Stated in full because
        /// approving without knowing what you approved is the failure this
        /// slice exists to prevent.
        input_json: String,
        /// The provider's own suggestions for how to stop being asked
        /// (`setMode`/`addRules`/`addDirectories`), verbatim JSON. Slice K
        /// offers NO grants — this is journaled because it is a fact about
        /// the request, and a later grants slice would need it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggestions_json: Option<String>,
    },
    /// The provider withdrew a pending request before anyone answered
    /// (Slice K) — e.g. the turn was interrupted. The card must go away;
    /// an unanswerable question left on screen is a lie.
    ApprovalWithdrawn { request_id: String, reason: String },
    /// The provider asked the operator a *dialog* question
    /// (`AskUserQuestion` and friends), which eave cannot present yet, so
    /// the adapter answered it with a stated refusal (Slice K amendment 2).
    /// Journaled as a fact: a session parked on a question nobody can see
    /// is exactly what this slice exists to prevent.
    DialogRefused { request_id: String, dialog: String },
    /// The CLI announced a permission posture eave will not run under
    /// (Slice K amendment 1). The session is refused, loudly and with the
    /// reason — it must never silently fall back to unreviewed writes.
    PostureRefused { mode: String, reason: String },
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
