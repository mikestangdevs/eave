//! Provider-agnostic session events, produced by an adapter's stream parser
//! and consumed by the engine (and folded into the journal — spec 02).

use serde::{Deserialize, Serialize};

/// The most of one tool payload eave will record, in bytes.
///
/// A cap is not optional here. Tool results are the largest thing eave has
/// ever written per record — a build log or a `Read` of a big file arrives
/// whole — and the journal is append-only and **replays in full on cold
/// start** (spec 02), so an uncapped payload makes startup grow with the
/// agent's verbosity rather than with the conversation's length. comet
/// reached a 917 KB chat document that was 85% tool text before they capped
/// theirs.
///
/// 8 KiB is **chosen, not derived**: it is comfortably more than any tool
/// call in the capture corpus and small enough that a thousand of them are
/// megabytes rather than gigabytes. Measure a real session before moving it.
pub const TOOL_PAYLOAD_CAP: usize = 8 * 1024;

/// Bound a tool payload, stating what was removed.
///
/// The elision is in the MIDDLE on purpose: a command is at the start of its
/// input and a test run's verdict is at the end of its output, so tail
/// truncation throws away the half that answers "did it work".
pub fn cap_payload(s: &str) -> String {
    if s.len() <= TOOL_PAYLOAD_CAP {
        return s.to_string();
    }
    // Halve the budget either side, then walk back to a char boundary so the
    // result is never split mid-codepoint.
    let half = TOOL_PAYLOAD_CAP / 2;
    let mut head = half;
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - half;
    while tail < s.len() && !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n… {} bytes elided …\n{}",
        &s[..head],
        s.len() - head - (s.len() - tail),
        &s[tail..]
    )
}

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
    ///
    /// `input` is the provider's own `input` object re-serialised compactly
    /// and bounded by [`TOOL_PAYLOAD_CAP`]. It is `#[serde(default)]` because
    /// every journal written before this field existed lacks it: those
    /// records still load, and their rows state the tool without stating
    /// what it was asked to do, which is exactly what they knew.
    ToolUseStarted {
        id: String,
        name: String,
        #[serde(default)]
        input: String,
    },
    /// A previously outstanding tool call resolved.
    ///
    /// `result` is the provider's `tool_result.content`, flattened to text
    /// and bounded the same way; `is_error` is the provider's own flag, not
    /// an inference from the text.
    ToolUseFinished {
        id: String,
        #[serde(default)]
        result: String,
        #[serde(default)]
        is_error: bool,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap elides the MIDDLE and states the byte count.
    ///
    /// Tail truncation is the obvious implementation and the wrong one: a
    /// command is at the start of its input and a test run's verdict is at
    /// the end of its output, so cutting the tail throws away the half that
    /// answers "did it work".
    #[test]
    fn a_capped_payload_keeps_both_ends_and_says_what_it_dropped() {
        let short = "cargo test";
        assert_eq!(cap_payload(short), short, "under the cap, untouched");

        let long: String = std::iter::repeat_n('x', TOOL_PAYLOAD_CAP * 3).collect();
        let capped = cap_payload(&long);
        assert!(capped.len() < long.len());
        assert!(
            capped.contains("bytes elided"),
            "an elision that does not state itself is a lie: {capped:.80}"
        );
        // Both ends survived.
        assert!(capped.starts_with('x') && capped.ends_with('x'));
    }

    /// A multi-byte codepoint must never be split by the cap.
    #[test]
    fn the_cap_never_splits_a_character() {
        // Every char is 4 bytes, so a byte-halving cut lands mid-codepoint
        // unless it walks back to a boundary.
        let long: String = std::iter::repeat_n('🙂', TOOL_PAYLOAD_CAP).collect();
        let capped = cap_payload(&long);
        assert!(capped.contains("bytes elided"));
        // The assertion is that this is reachable at all: an invalid split
        // would have panicked on the slice inside `cap_payload`.
        assert!(capped.chars().filter(|c| *c == '🙂').count() > 0);
    }

    /// A journal written before the payload fields existed still loads, and
    /// says what it knew: the tool ran, and nothing about what it was asked.
    ///
    /// This is the whole back-compat contract of the slice. eave replays its
    /// journal in full on cold start (spec 02), so a format change that
    /// cannot read yesterday's file is a format change that loses history.
    #[test]
    fn a_journal_written_before_this_slice_still_loads() {
        let old = r#"{"kind":"tool_use_started","id":"toolu_1","name":"Bash"}"#;
        let ev: SessionEvent = serde_json::from_str(old).expect("old record must load");
        assert_eq!(
            ev,
            SessionEvent::ToolUseStarted {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: String::new(),
            }
        );

        let old = r#"{"kind":"tool_use_finished","id":"toolu_1"}"#;
        let ev: SessionEvent = serde_json::from_str(old).expect("old record must load");
        assert_eq!(
            ev,
            SessionEvent::ToolUseFinished {
                id: "toolu_1".into(),
                result: String::new(),
                is_error: false,
            }
        );
    }
}
