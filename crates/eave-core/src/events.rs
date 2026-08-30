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

/// What one unit of finished model work cost, in the provider's own words
/// (Slice CC).
///
/// **`input` is the WHOLE prompt, cache included**, and `cache_read` /
/// `cache_write` are subsets of it, stated separately. That convention is
/// not eave's invention — it is what the providers that define the term
/// define it as:
///
/// - Codex app-server: `tokenUsage.last.totalTokens` = `inputTokens` +
///   `outputTokens`, with `cachedInputTokens` a portion of `inputTokens`
///   (verified against comet's live 0.146.1 capture,
///   `comet/crates/harness/tests/fixtures/codex/live-0.146.1-multi-agent.jsonl:35`).
/// - Grok: *"ACP `_meta.usage.inputTokens` (PromptUsage) is still the
///   **full** prompt sum; only the headless projector subtracts cache"* —
///   grok 1.0.5's own bundled documentation.
///
/// Claude and pi are the exceptions, and their adapters convert: both
/// follow the Messages API, whose `input_tokens` counts only the tokens
/// that were neither read from nor written to cache (pi computes its own
/// `totalTokens` as `input + output + cacheRead + cacheWrite`, which is
/// only correct if the buckets are disjoint). So `stream.rs` and `pi.rs`
/// add three disjoint stated counts. That is exact arithmetic on numbers
/// the provider stated, and it is the only arithmetic in this slice —
/// nothing here is ever derived from message length, character count, or
/// any other proxy.
///
/// Every field is `Option` because a provider that did not say a number
/// gets `None`. Never a zero: "the provider reported no cache tokens" and
/// "the provider does not report cache tokens" are different facts and a
/// zero would erase the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TurnTokens {
    /// The whole prompt sent, cache included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    /// Tokens the model generated, reasoning included where the provider
    /// folds it in (all five do).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// The portion of `input` that was served from cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    /// The portion of `input` that was written to cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

impl TurnTokens {
    /// Build one only if the provider stated at least one number.
    ///
    /// An all-`None` `TurnTokens` claims a usage statement was made when
    /// none was; this is the constructor that refuses to manufacture one,
    /// so a `Some(TurnTokens)` anywhere downstream always carries a fact.
    pub fn stated(
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
    ) -> Option<Self> {
        let any = input.is_some() || output.is_some();
        let any = any || cache_read.is_some() || cache_write.is_some();
        any.then_some(Self {
            input,
            output,
            cache_read,
            cache_write,
        })
    }
}

/// How full the model's context window is right now (Slice CC).
///
/// This is a *level*, not a *cost*: it goes down when the agent compacts
/// and it is the number that decides whether the next turn fits. The ACP
/// schema names it exactly — `used` is "Tokens currently in context",
/// `size` is "Total context window size in tokens"
/// (`acp/schema.py::_UsageUpdate`, agent-client-protocol 1.x).
///
/// `used_tokens` is not optional: a context statement that does not say
/// how much is in context is not a statement. `limit_tokens` is, because
/// pi reports the level and never the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextWindow {
    pub used_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_tokens: Option<u64>,
}

impl ContextWindow {
    /// 0.0..=1.0 of the window in use, when the provider stated a ceiling.
    ///
    /// `None` when it did not, and `None` for a stated ceiling of zero —
    /// which is a provider bug, not a full window, and dividing by it
    /// would report `inf` as a percentage.
    pub fn fraction(&self) -> Option<f32> {
        let limit = self.limit_tokens.filter(|l| *l > 0)?;
        Some(self.used_tokens as f32 / limit as f32)
    }
}

/// One edit's before and after, as the provider reported it (Slice AG1 #4).
///
/// Bounded by the same [`crate::events::TOOL_PAYLOAD_CAP`] as every other
/// tool payload, and for the same reason: a row must never be the thing
/// that has to defend the render budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiff {
    pub path: String,
    /// What the file was. Empty when the tool created the file — something
    /// that did not exist has no old text, and that is a fact, not a gap.
    #[serde(default)]
    pub old_text: String,
    #[serde(default)]
    pub new_text: String,
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
        /// Every `/name` this CLI will accept, by the CLI's own account
        /// (Slice AR2). The `system/init` frame carries a `slash_commands`
        /// array and it arrives BEFORE the model is invoked, so this is a
        /// complete inventory that costs no tokens to obtain.
        ///
        /// **Complete where a filesystem scan cannot be.** Measured
        /// 2026-08-20 with eave's exact argv: the frame lists 68 names from
        /// `/tmp`; everything reachable on disk accounts for 21 of them.
        /// The rest (`/context`, `/dataviz`, `/compact`, …) are bundled
        /// inside the `claude` executable and exist in no file.
        ///
        /// **Per session and cwd-sensitive**, which is why it rides the
        /// session's own event rather than a global: planting one command
        /// and one skill under a directory's `.claude/` took the same
        /// machine from 68 to 71 in the same minute.
        ///
        /// Empty = the CLI said nothing (an older CLI, or a journal written
        /// by an engine predating this field). Never a guess, and never
        /// filled in from a scan — a reader that wants the disk's answer
        /// has to ask the disk.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        slash_commands: Vec<String>,
    },
    /// Incremental assistant text (from partial-message stream events).
    TextDelta { text: String },
    /// Incremental *reasoning* — the model thinking in public, before or
    /// between its answer text.
    ///
    /// **Typed rather than a `System` frame, and that is the point.** ACP
    /// agents send reasoning in their own wire form and it is decoded from
    /// `System { subtype: "agent_thought_chunk", .. }`; Claude sends a
    /// `thinking_delta` on the partial-message stream, which is not an ACP
    /// frame and must not be dressed as one — a `raw` that claims to be a
    /// `session/update` nobody sent is a journal line that lies. Two wire
    /// shapes, two decoders, and **one place they land**
    /// (`Projection::push_thought`), so they cannot become two behaviours.
    ///
    /// Deliberately NOT agent work for [`crate::turn`]: reasoning arrives
    /// inside a turn something else already opened, and letting it open one
    /// is how a lone chunk mints a phantom `Unannounced` turn. `System`
    /// carrying the ACP spelling is `false` for the same reason, so the two
    /// providers behave identically.
    ThoughtDelta { text: String },
    /// A complete assistant text block (the post-stream snapshot message).
    AssistantText { text: String },
    /// A tool call became outstanding. Emitted once per tool id.
    ///
    /// `input` is the provider's own `input` object re-serialised compactly
    /// and bounded by [`TOOL_PAYLOAD_CAP`]. It is `#[serde(default)]` because
    /// every journal written before this field existed lacks it: those
    /// records still load, and their rows state the tool without stating
    /// what it was asked to do, which is exactly what they knew.
    ///
    /// `call` is the adapter's decode of that same input into
    /// [`crate::tool::ToolCall`] — a lens ADDED over the fact of record,
    /// never a replacement for it. `name` and `input` stay verbatim above,
    /// so nothing this build fails to recognise is lost.
    ///
    /// **`None` and `Some(ToolCall::Unknown{..})` are different facts and
    /// both happen.** `None` = the record was written before adapters
    /// decoded anything, and a reader must fall back to guessing from
    /// `name` (`eave_wire::model::tool_detail` does exactly that).
    /// `Some(Unknown)` = an adapter looked and did not recognise the tool.
    /// Collapsing the two would make every old record claim it had been
    /// examined.
    ToolUseStarted {
        id: String,
        name: String,
        #[serde(default)]
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<crate::tool::ToolCall>,
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
        /// The before/after the provider handed us for an edit, when it
        /// handed us one (Slice AG1 #4).
        ///
        /// **This is a fact eave was already being given and throwing
        /// away.** ACP agents send `{type:"diff", path, oldText, newText}`
        /// in a tool call's content; `flatten_content` keeps `newText` —
        /// correct for a RESULT string, since what the file became is what
        /// the tool returned — and dropped `oldText` on the floor. Without
        /// the old side there is no diff to draw, so the transcript could
        /// only ever show the finished text.
        ///
        /// **Not reconstructible later, which is why it is journalled.**
        /// The engine's own diff (`git.rs::diff`) is against HEAD — the
        /// file's CURRENT state — so it answers a different question the
        /// moment a later call touches the same file. A row is a
        /// historical fact and needs the diff as it was.
        ///
        /// `serde(default)` for the same reason as its siblings: journals
        /// written before this field existed must still load, and they
        /// will read `None`, which is honest — those turns really do have
        /// no recorded before-text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
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
    /// The provider stated something about tokens (Slice CC).
    ///
    /// **Two different facts, never one.** `turn` is what the work just
    /// finished cost; `context` is how full the window is right now. They
    /// are not the same number and they do not even move together: a turn
    /// with two model round-trips bills the prompt TWICE (Claude's
    /// `result.usage` for the tool turn in
    /// `docs/research/claude-stream-capture-tool-turn.jsonl` sums to
    /// 53,820 input tokens) while the context it left behind held 26,967.
    /// Adding the halves, or showing one under the other's label, is the
    /// lying UI this variant exists to make impossible — hence two named
    /// types rather than five sibling integers.
    ///
    /// Both are `Option` because most providers state one and not the
    /// other, and either may arrive alone: an ACP `usage_update` is
    /// context-only, an ACP prompt response is turn-only. A record with
    /// both `None` is never emitted.
    TokenUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<TurnTokens>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<ContextWindow>,
    },
    /// A frame that belongs to a DIFFERENT thread on the same connection
    /// (Slice CJ).
    ///
    /// Codex multiplexes: a `Task` spawns a child thread whose turns
    /// start, run tools and complete on eave's own stdout, each frame
    /// naming its own `threadId`. eave drives exactly one thread, and
    /// every other frame is somebody else's.
    ///
    /// **Attribution, not narrowing.** Slice CH got the ownership right
    /// and the type wrong: it wrote a child's frame out as
    /// `System { subtype: "foreign_thread:<method>" }`, so a child's
    /// `commandExecution`, its `item/completed` and its token usage all
    /// collapsed into one opaque string that no later reader could widen
    /// again. Here the frame is decoded by the same parser it would get
    /// if it were eave's own, and the decode is kept — `event` is a fully
    /// typed [`SessionEvent`], `thread_id` is the provider's own id for
    /// whoever owns it.
    ///
    /// **The wrapper IS the safety property**, and that is why this is a
    /// variant rather than an `Option<String>` field on the events that
    /// can be foreign. Every consumer matches `SessionEvent::TurnCompleted
    /// { .. }`, `ToolUseStarted { .. }` and friends; a field would be
    /// invisible to every one of those patterns, so a child's turn end
    /// would keep closing eave's turn until each site remembered to check
    /// it. Nested inside `ForeignThread` those patterns simply do not
    /// match, so the mistake is not available. The two exhaustive matches
    /// (`turn::marks_agent_work`, `eave_wire::model::apply_provider`)
    /// then force any new consumer to decide what a child's frame means
    /// to it.
    ///
    /// comet reached the same shape independently —
    /// `comet/crates/proto/src/agent.rs:418` is
    /// `Subagent { parent_tool_use_id: String, event: Box<AgentEvent> }`.
    /// The delta is the key: comet tags by the parent tool call, which
    /// requires a child registry it seeds from `subAgentActivity` items
    /// and drops frames it cannot resolve against; eave tags by the
    /// thread the provider itself named, which needs no registry and can
    /// never drop a frame for arriving before one.
    ///
    /// **No subagent surface is built on this.** The UI renders nothing
    /// for it (`model.rs`) and the ledger opens no turn for it
    /// (`turn.rs`) — this slice removes a narrowing, it does not add a
    /// feature.
    ForeignThread {
        /// The provider's own id for the thread that owns `event`.
        thread_id: String,
        event: Box<SessionEvent>,
    },
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
                call: None,
            }
        );

        // The record one slice later — payload but no typed call. It must
        // still load, and it must NOT claim to have been decoded.
        let untyped = r#"{"kind":"tool_use_started","id":"toolu_2","name":"Bash","input":"{\"command\":\"ls\"}"}"#;
        let ev: SessionEvent = serde_json::from_str(untyped).expect("untyped record must load");
        assert_eq!(
            ev,
            SessionEvent::ToolUseStarted {
                id: "toolu_2".into(),
                name: "Bash".into(),
                input: r#"{"command":"ls"}"#.into(),
                call: None,
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
                    diff: None,
                }
        );
    }
    /// A record with no typed call must not grow one on disk.
    ///
    /// `None` is the pre-typing fact, and it is written by leaving the key
    /// out — so a journal replayed and rewritten does not gain a byte, and
    /// so `null` never becomes a third state nobody meant.
    #[test]
    fn an_untyped_record_writes_no_call_key_and_a_typed_one_round_trips() {
        let untyped = SessionEvent::ToolUseStarted {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            call: None,
        };
        let line = serde_json::to_string(&untyped).unwrap();
        assert!(
            !line.contains("call"),
            "an absent typed call must be absent on disk too: {line}"
        );
        assert_eq!(
            serde_json::from_str::<SessionEvent>(&line).unwrap(),
            untyped
        );

        let typed = SessionEvent::ToolUseStarted {
            id: "toolu_2".into(),
            name: "Bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            call: Some(crate::tool::ToolCall::Run {
                command: "ls".into(),
            }),
        };
        let line = serde_json::to_string(&typed).unwrap();
        assert_eq!(serde_json::from_str::<SessionEvent>(&line).unwrap(), typed);
        // The raw input is still the fact of record beside it: typing adds
        // a lens, it never replaces what the provider said.
        assert!(line.contains(r#""input":"{\"command\":\"ls\"}""#), "{line}");
    }

    /// **Slice CC, the same contract from the other side.** Adding a
    /// variant must not change how any older variant reads, and the new
    /// one must survive its own round trip with the halves intact.
    #[test]
    fn adding_token_usage_did_not_disturb_the_records_already_written() {
        for old in [
            r#"{"kind":"init","session_id":"abc"}"#,
            r#"{"kind":"text_delta","text":"hi"}"#,
            r#"{"kind":"turn_completed","subtype":"success","is_error":false}"#,
            r#"{"kind":"system","subtype":"status","raw":"{}"}"#,
            r#"{"kind":"exited","code":0,"signal":null,"stderr_tail":""}"#,
        ] {
            serde_json::from_str::<SessionEvent>(old)
                .unwrap_or_else(|e| panic!("{old} must still load: {e}"));
        }

        let ev = SessionEvent::TokenUsage {
            turn: TurnTokens::stated(Some(26_397), Some(5), Some(15_899), Some(10_496)),
            context: Some(ContextWindow {
                used_tokens: 26_397,
                limit_tokens: Some(1_000_000),
            }),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"token_usage""#), "{json}");
        assert_eq!(serde_json::from_str::<SessionEvent>(&json).unwrap(), ev);
    }

    /// A number the provider did not state stays absent, all the way
    /// through the wire. A zero here would read as "no cache was used",
    /// which is a claim about the run rather than about the report.
    #[test]
    fn an_unstated_count_is_absent_rather_than_zero() {
        let ev = SessionEvent::TokenUsage {
            turn: TurnTokens::stated(Some(10), Some(2), None, None),
            context: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("cache_read"), "{json}");
        assert!(!json.contains("context"), "{json}");
        assert_eq!(serde_json::from_str::<SessionEvent>(&json).unwrap(), ev);
    }

    /// The constructor refuses to invent a usage statement out of four
    /// silences — so `Some(TurnTokens)` downstream always means the
    /// provider said something.
    #[test]
    fn a_provider_that_said_nothing_produces_no_turn_tokens() {
        assert_eq!(TurnTokens::stated(None, None, None, None), None);
        assert!(TurnTokens::stated(None, Some(0), None, None).is_some());
    }

    /// **Slice CJ, back-compat.** eave replays its journal in full on cold
    /// start (spec 02), and a day's worth of records were written under
    /// Slice CH as `System { subtype: "foreign_thread:<method>" }`. Those
    /// records must still load, and they must load as what they are: an
    /// opaque `System` line whose decode was never performed. Silently
    /// re-reading them as [`SessionEvent::ForeignThread`] would be worse
    /// than failing — it would claim a typed decode this build never did,
    /// on bytes it no longer has the parser context for.
    ///
    /// The `kind` tags differ (`system` vs `foreign_thread`), so serde
    /// cannot confuse the two. This test is what keeps that true if anyone
    /// ever reaches for a rename or an alias.
    #[test]
    fn a_journal_written_under_slice_ch_still_loads_as_what_it_said() {
        let ch = r#"{"kind":"system","subtype":"foreign_thread:turn/completed","raw":"{\"method\":\"turn/completed\"}"}"#;
        let ev: SessionEvent = serde_json::from_str(ch).expect("a CH record must still load");
        assert_eq!(
            ev,
            SessionEvent::System {
                subtype: "foreign_thread:turn/completed".into(),
                raw: r#"{"method":"turn/completed"}"#.into(),
            },
            "yesterday's record states what it knew — no more"
        );
        // It is emphatically NOT re-read as an attributed frame: a
        // `System` is not a `ForeignThread`, and a reader that treats it
        // as one would be inventing a thread id nobody wrote down.
        assert!(!matches!(ev, SessionEvent::ForeignThread { .. }));
        assert_eq!(serde_json::to_string(&ev).unwrap(), ch, "byte for byte");

        // And every older shape still loads beside it.
        for old in [
            r#"{"kind":"init","session_id":"abc"}"#,
            r#"{"kind":"tool_use_started","id":"t","name":"Bash"}"#,
            r#"{"kind":"turn_completed","subtype":"success","is_error":false}"#,
            r#"{"kind":"token_usage","turn":{"input":1}}"#,
        ] {
            serde_json::from_str::<SessionEvent>(old)
                .unwrap_or_else(|e| panic!("{old} must still load: {e}"));
        }
    }

    /// The carrier's own contract: the thread is named and the payload
    /// stays a fully typed event through a journal round trip. A wrapper
    /// that flattened its inner event on the way to disk would have
    /// reinvented CH's defect one layer down.
    #[test]
    fn an_attributed_frame_round_trips_with_its_inner_event_still_typed() {
        let ev = SessionEvent::ForeignThread {
            thread_id: "01a00fe7-e358-7a02-8745-e7a0793013ce".into(),
            event: Box::new(SessionEvent::ToolUseStarted {
                id: "exec-1".into(),
                name: "commandExecution".into(),
                input: r#"{"command":"ls"}"#.into(),
                call: Some(crate::tool::ToolCall::Run {
                    command: "ls".into(),
                }),
            }),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains(r#""kind":"foreign_thread""#), "{line}");
        assert!(
            line.contains(r#""kind":"tool_use_started""#),
            "the inner event keeps its own tag rather than becoming text: {line}"
        );
        assert_eq!(serde_json::from_str::<SessionEvent>(&line).unwrap(), ev);
    }

    /// A percentage needs a ceiling, and a ceiling of zero is a provider
    /// bug rather than a full window.
    #[test]
    fn a_context_fraction_needs_a_stated_ceiling() {
        let w = ContextWindow {
            used_tokens: 500,
            limit_tokens: Some(1000),
        };
        assert_eq!(w.fraction(), Some(0.5));
        assert_eq!(
            ContextWindow {
                used_tokens: 500,
                limit_tokens: None
            }
            .fraction(),
            None
        );
        assert_eq!(
            ContextWindow {
                used_tokens: 500,
                limit_tokens: Some(0)
            }
            .fraction(),
            None,
            "dividing by a zero ceiling reports inf, not a full window"
        );
    }
}
