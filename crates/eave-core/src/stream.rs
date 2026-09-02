//! Claude Code stream-JSON line parser.
//!
//! Contract observed from the CLI in `-p --input-format stream-json
//! --output-format stream-json --verbose --include-partial-messages` mode
//! (research: waku.md §2; waku-core/src/driver/claude.rs is the proven
//! native path). The parser also owns the outstanding-`tool_use` set —
//! the "nothing in flight" input to the attention clocks (spec 01).

use std::collections::HashSet;

use serde_json::Value;

use crate::events::{ContextWindow, FastLane, SessionEvent, TurnTokens};
use crate::tool::{self, TodoItem, ToolCall};

const MALFORMED_LINE_CAP: usize = 512;

#[derive(Default)]
pub struct ClaudeStream {
    outstanding: HashSet<String>,
    /// Tool ids whose arguments have actually been seen.
    ///
    /// Separate from `outstanding` because the CLI announces a tool call
    /// **before** it has any arguments: under
    /// `--include-partial-messages` the `content_block_start` for a
    /// `tool_use` carries `"input": {}` and the real object arrives one
    /// line later on the assistant snapshot (verified against
    /// docs/research/approvals-capture-2026-08-18/capE-bash.jsonl, lines
    /// 21 and 26). Deduplicating on `outstanding` alone therefore emitted
    /// the empty announcement and dropped the arguments on the floor —
    /// which is why every Claude tool row said `Bash` and nothing else.
    tool_input_seen: HashSet<String>,
    session_id: Option<String>,
    /// The model `system/init` named, kept so the `result` line's
    /// `modelUsage` map can be indexed by it (Slice CC). The map is keyed
    /// by model id and holds a row for every model the turn touched —
    /// including the little `claude-haiku-*` side calls — so "the context
    /// window" is only answerable once you know which row is the session's.
    model: Option<String>,
    /// Prompt size of the most recent model request, in tokens
    /// (Slice CC): the sum of the three disjoint stated buckets on the
    /// last `message_start`/`message_delta` usage.
    ///
    /// **It cannot come from the `result` line.** `result.usage` is the
    /// SUM over the turn's model requests — verified in
    /// `docs/research/claude-stream-capture-tool-turn.jsonl`, where two
    /// requests of 26,853 and 26,967 prompt tokens settle as a result
    /// line reporting 53,820 — so reading occupancy off it would double
    /// on every tool call and pass the window before the window filled.
    context_used: Option<u64>,
    /// Permission requests the CLI is parked on right now (Slice K). Held
    /// here so the adapter can answer a still-open question and ignore a
    /// stale one; the durable truth is the journal.
    pending: HashSet<String>,
    /// The envelope's `request_id` lives one level up from the `request`
    /// object; stashed while parsing so the nested parse can name it.
    pending_request_id: Option<String>,
}

/// The `modelUsage` row for `model`: the exact key, else the one that
/// starts with it followed by a bracket — `init` says `claude-opus-5[1m]`
/// and a `set_model` names `claude-opus-5` (Slice TN2), and both are one
/// model.
fn model_usage_row<'a>(model_usage: &'a Value, model: &str) -> Option<&'a Value> {
    if let Some(row) = model_usage.get(model) {
        return Some(row);
    }
    let map = model_usage.as_object()?;
    map.iter()
        .find(|(k, _)| {
            k.strip_prefix(model)
                .is_some_and(|rest| rest.starts_with('['))
        })
        .map(|(_, v)| v)
}

impl ClaudeStream {
    /// The model this process now runs under (Slice TN2), after a
    /// `set_model` the CLI accepted — so the next `result`'s `modelUsage`
    /// is read from the right row.
    pub fn set_model(&mut self, model: String) {
        self.model = Some(model);
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Number of outstanding tool calls (spec 01: while > 0, `QUIET_AFTER`
    /// does not run; `TOOL_EXEC_AFTER` applies instead).
    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }

    /// Permission requests the CLI is currently parked on (Slice K).
    pub fn pending_approvals(&self) -> usize {
        self.pending.len()
    }

    /// Is this request still open? A decision for anything else is a race
    /// (already withdrawn, already answered) and must not reach the pipe.
    pub fn approval_is_open(&self, request_id: &str) -> bool {
        self.pending.contains(request_id)
    }

    /// Close one request: answered by the operator, or withdrawn by the
    /// provider.
    pub fn resolve_approval(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id)
    }

    /// Provider-native resume cursor, once seen (spec 02).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Parse one line into zero or more events. Never fails: malformed input
    /// becomes a `Malformed` event and parsing continues on the next line.
    pub fn parse_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => return vec![malformed(line, err.to_string())],
        };
        match v.get("type").and_then(Value::as_str) {
            Some("system") => {
                if v.get("subtype").and_then(Value::as_str) == Some("init") {
                    match v.get("session_id").and_then(Value::as_str) {
                        Some(id) => {
                            self.session_id = Some(id.to_string());
                            // Slice CC: which `modelUsage` row is ours.
                            self.model = v.get("model").and_then(Value::as_str).map(str::to_string);
                            vec![SessionEvent::Init {
                                // Slice TN1: the CLI's own fast-lane
                                // answer, kept verbatim so a refusal of
                                // what eave asked for is a journalled fact.
                                fast_lane: v.get("fast_mode_state").and_then(Value::as_str).map(
                                    |state| FastLane {
                                        state: state.to_string(),
                                        disabled_reason: v
                                            .get("fast_mode_disabled_reason")
                                            .and_then(Value::as_str)
                                            .map(str::to_string),
                                    },
                                ),
                                session_id: id.to_string(),
                                // The posture the CLI says it is actually
                                // running in — Slice K verifies this before
                                // the session is allowed to do anything.
                                permission_mode: v
                                    .get("permissionMode")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                // Slice AR2: the CLI's own complete list of
                                // the `/name`s it will accept, announced
                                // before the model is invoked. Kept rather
                                // than dropped — a filesystem scan finds
                                // roughly a third of these, because the rest
                                // are bundled inside the executable.
                                //
                                // Non-strings are skipped rather than
                                // failing the frame: this array is somebody
                                // else's shape, and an `init` that could not
                                // be parsed would take `session_id` and
                                // `permissionMode` down with it — the two
                                // facts the session cannot start without.
                                slash_commands: v
                                    .get("slash_commands")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(Value::as_str)
                                            .map(str::to_string)
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            }]
                        }
                        None => vec![malformed(line, "init without session_id".into())],
                    }
                } else {
                    // Every non-init system line is surfaced with its raw
                    // bytes so the journal can persist it verbatim — Stage E
                    // consumes `status`/`task_*`, and future subtypes must be
                    // recorded without a parser change (build directive
                    // 2026-08-15; research: claude-stream-notes.md).
                    vec![SessionEvent::System {
                        subtype: v
                            .get("subtype")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        raw: line.to_string(),
                    }]
                }
            }
            // The permission channel (Slice K). `--permission-prompt-tool
            // stdio` makes the CLI ask here instead of auto-denying, and the
            // answer goes back on stdin as a `control_response` naming the
            // same request_id. This adapter owns that encoding; everything
            // downstream sees provider-neutral events.
            Some("control_request") => {
                self.pending_request_id = v
                    .get("request_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.parse_control_request(&v["request"], line)
            }
            Some("control_cancel_request") => match v.get("request_id").and_then(Value::as_str) {
                Some(id) => vec![SessionEvent::ApprovalWithdrawn {
                    request_id: {
                        self.pending.remove(id);
                        id.to_string()
                    },
                    reason: "the provider withdrew the request".into(),
                }],
                None => vec![malformed(
                    line,
                    "control_cancel_request without request_id".into(),
                )],
            },
            // Our own decisions, echoed back by the CLI. The engine journals
            // the decision itself at the moment it reaches the pipe, so the
            // echo carries nothing new — dropped, not journaled twice.
            Some("control_response") => Vec::new(),
            Some("stream_event") => self.parse_stream_event(&v["event"]),
            Some("assistant") => self.parse_assistant(&v["message"]),
            Some("user") => self.parse_user(&v["message"], v.get("uuid").and_then(Value::as_str)),
            Some("result") => {
                let mut out = Vec::new();
                // Slice CC: usage first, then the bookend — the cost is a
                // fact about the turn that just ran, not about the next
                // one. Same ordering comet settled on for codex
                // (`comet/crates/harness/src/codex/mod.rs:965`, usage
                // drained before `Done`).
                out.extend(self.turn_usage(v.get("usage"), v.get("modelUsage")));
                out.push(SessionEvent::TurnCompleted {
                    subtype: v
                        .get("subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                });
                out
            }
            Some(other) => vec![SessionEvent::Unknown {
                event_type: other.to_string(),
                raw: line.to_string(),
            }],
            None => vec![malformed(line, "missing type tag".into())],
        }
    }

    /// One `control_request`. `can_use_tool` becomes the operator's
    /// question; every other subtype is a *dialog* eave cannot present yet
    /// and is refused explicitly by the caller — silence would park the
    /// session on a question nobody can see (Slice K amendment 2).
    fn parse_control_request(&mut self, req: &Value, line: &str) -> Vec<SessionEvent> {
        let Some(request_id) = req
            .get("request_id")
            .or_else(|| req.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.pending_request_id.take())
        else {
            return vec![malformed(line, "control_request without request_id".into())];
        };
        match req.get("subtype").and_then(Value::as_str) {
            Some("can_use_tool") => {
                let tool = req
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = req.get("input").cloned().unwrap_or(Value::Null);
                // `blocked_path` is the path the CLI itself named as the
                // reason; for edit-shaped tools the input carries it. A
                // Bash redirect has it ONLY in blocked_path (capture E).
                let path = req
                    .get("blocked_path")
                    .and_then(Value::as_str)
                    .or_else(|| input.get("file_path").and_then(Value::as_str))
                    .or_else(|| input.get("path").and_then(Value::as_str))
                    .or_else(|| input.get("notebook_path").and_then(Value::as_str))
                    .map(str::to_string);
                self.pending.insert(request_id.clone());
                vec![SessionEvent::ApprovalRequested {
                    request_id,
                    display_name: req
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(&tool)
                        .to_string(),
                    tool,
                    description: req
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    path,
                    input_json: input.to_string(),
                    suggestions_json: req.get("permission_suggestions").map(|s| s.to_string()),
                }]
            }
            other => vec![SessionEvent::DialogRefused {
                request_id,
                dialog: other.unwrap_or("unknown").to_string(),
            }],
        }
    }

    /// The `result` line's two token facts (Slice CC).
    ///
    /// `result.usage` is **per turn, not cumulative**: driving one process
    /// through two prompts (`docs/research/usage-capture-2026-08-20/`)
    /// gave `output_tokens` 5 then 4, not 5 then 9. `modelUsage` in the
    /// same capture DID accumulate (`cacheReadInputTokens` 15,899 then
    /// 42,294 = 15,899 + 26,395), so nothing but its constant
    /// `contextWindow` is read from it.
    fn turn_usage(&self, usage: Option<&Value>, model_usage: Option<&Value>) -> Vec<SessionEvent> {
        let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
        let fresh = count("input_tokens");
        let cache_read = count("cache_read_input_tokens");
        let cache_write = count("cache_creation_input_tokens");
        // The Messages API's `input_tokens` counts ONLY the tokens that
        // were neither read from nor written to cache, so the whole prompt
        // is the sum of three disjoint stated counts. Exact arithmetic on
        // what the CLI said — the alternative is reporting 2 where the
        // model read 26,397.
        let input = match (fresh, cache_read, cache_write) {
            (None, None, None) => None,
            (a, b, c) => Some(a.unwrap_or(0) + b.unwrap_or(0) + c.unwrap_or(0)),
        };
        let turn = TurnTokens::stated(input, count("output_tokens"), cache_read, cache_write);
        let context = self.context_used.map(|used| ContextWindow {
            used_tokens: used,
            limit_tokens: self
                .model
                .as_deref()
                .and_then(|m| model_usage_row(model_usage?, m)?.get("contextWindow")?.as_u64()),
        });
        if turn.is_none() && context.is_none() {
            return Vec::new();
        }
        vec![SessionEvent::TokenUsage { turn, context }]
    }

    fn parse_stream_event(&mut self, ev: &Value) -> Vec<SessionEvent> {
        // Slice CC: every model request inside the turn restates the
        // prompt it was sent, on `message_start` and again on
        // `message_delta`. The last one to arrive is what is in context
        // when the turn settles; both spellings are read because an
        // interrupted response leaves only the `message_start`.
        if let Some(kind) = ev.get("type").and_then(Value::as_str) {
            if matches!(kind, "message_start" | "message_delta") {
                let usage = ev.get("usage").or_else(|| ev.pointer("/message/usage"));
                if let Some(used) = prompt_tokens(usage) {
                    self.context_used = Some(used);
                }
            }
        }
        match ev.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let cb = &ev["content_block"];
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.note_tool_use(cb).into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            Some("content_block_delta") => {
                // Three delta shapes share this frame and they are told
                // apart by WHICH KEY carries the string, not by a type
                // tag: answer text is `text`, reasoning is `thinking`,
                // and `signature_delta` carries `signature`, which is a
                // cryptographic receipt for the thinking block and not
                // anything to render.
                //
                // **Reasoning was silently dropped until 2026-08-30**:
                // this arm read `delta.text` and returned nothing for a
                // `thinking_delta`, so the reasoning box could only ever
                // be filled by an ACP agent.
                //
                // **And decoding it is currently a no-op, which is the
                // CLI's doing and is measured, not assumed.** Claude Code
                // 2.1.251 REDACTS the content: every `thinking_delta`
                // carries `"thinking": ""` with an `estimated_tokens`
                // count beside it, and the opening
                // `content_block_start` carries `"thinking": ""` and
                // `"signature": ""` too. eave is shown the SHAPE of the
                // thinking and none of the text.
                //
                // So the empty filter below is load-bearing, not
                // defensive: without it every turn writes a run of
                // `thought_delta` records carrying nothing, and starts
                // the reasoning clock for a trace that will never have a
                // word in it. The decode stays because it is correct and
                // costs nothing, and it starts working the day the CLI
                // stops redacting — the `estimated_tokens` field is a
                // real fact and a different feature, deliberately not
                // built here.
                let delta = &ev["delta"];
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    vec![SessionEvent::TextDelta {
                        text: text.to_string(),
                    }]
                } else if let Some(text) = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    vec![SessionEvent::ThoughtDelta {
                        text: text.to_string(),
                    }]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn parse_assistant(&mut self, msg: &Value) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        for block in msg
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        out.push(SessionEvent::AssistantText {
                            text: text.to_string(),
                        });
                    }
                }
                Some("tool_use") => out.extend(self.note_tool_use(block)),
                _ => {}
            }
        }
        out
    }

    /// `frame_uuid` is the id Claude puts on the OUTER frame, not on
    /// `message` — which is why this takes it as a second argument rather
    /// than reading it here. It is the address
    /// `rewind_conversation`'s `target_message_uuid` expects, and it is
    /// attached only to a TEXT echo: a `tool_result` arrives on a `user`
    /// frame too, and it is not a message anyone can rewind to.
    fn parse_user(&mut self, msg: &Value, frame_uuid: Option<&str>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        match msg.get("content") {
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                                if self.outstanding.remove(id) {
                                    out.push(SessionEvent::ToolUseFinished {
                                        id: id.to_string(),
                                        result: tool_result_text(block),
                                        // The provider's own flag. Never
                                        // inferred from the text: "error"
                                        // appears in plenty of successful
                                        // output.
                                        is_error: block
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                    diff: None,
                });
                                }
                            }
                        }
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.push(SessionEvent::UserEcho {
                                    text: text.to_string(),
                                    provider_msg_id: frame_uuid.map(str::to_string),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(Value::String(text)) => out.push(SessionEvent::UserEcho {
                text: text.clone(),
                provider_msg_id: frame_uuid.map(str::to_string),
            }),
            _ => {}
        }
        out
    }

    /// Register a tool_use block.
    ///
    /// Emits on first sight of an id — the row must appear while the tool
    /// is running, not after — and again, once, when the arguments finally
    /// arrive. The second emission carries the same id, so every consumer
    /// treats it as the same tool: the transcript fold updates that row in
    /// place, the journal's in-flight set is a set, and `outstanding` here
    /// is unchanged. Nothing else repeats: once the arguments are recorded
    /// the id never emits again.
    fn note_tool_use(&mut self, block: &Value) -> Option<SessionEvent> {
        let id = block.get("id").and_then(Value::as_str)?;
        let first_sight = self.outstanding.insert(id.to_string());
        let input = block.get("input").filter(|v| !v.is_null());
        // `{}` is the CLI's placeholder, not an argument list.
        let has_args = input.is_some_and(|v| v.as_object().is_none_or(|o| !o.is_empty()));
        if has_args {
            // False = already recorded; a third sighting says nothing new.
            if !self.tool_input_seen.insert(id.to_string()) {
                return None;
            }
        } else if !first_sight {
            return None;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(SessionEvent::ToolUseStarted {
            id: id.to_string(),
            call: Some(decode_tool(&name, input.unwrap_or(&Value::Null))),
            name,
            // Re-serialised compactly rather than kept verbatim: the
            // provider's whitespace is not a fact about the tool call,
            // and a compact form is what the cap is measured against.
            // A missing `input` is an empty string, not `"null"` — the
            // row then states the tool and nothing more, which is
            // accurate.
            input: input
                .map(|v| crate::events::cap_payload(&v.to_string()))
                .unwrap_or_default(),
        })
    }
}

/// Decode one Claude `tool_use` block into a typed [`ToolCall`].
///
/// The names are the CLI's own and the keys are the ones it actually
/// sends: `Bash`→`command`, `Read`/`Write`→`file_path` come off the
/// capture corpus at `docs/research/approvals-capture-2026-08-18/`; the
/// rest follow the same tool's documented input.
///
/// Everything not listed — `BashOutput`, `KillShell`, `ExitPlanMode`, an
/// `mcp__server__tool`, a skill, anything a later CLI adds — is
/// [`ToolCall::Unknown`] under its own name. That includes MCP, which
/// comet gives a variant of its own: eave's row already renders
/// `mcp__playwright__browser_click` in the name column, so a typed
/// `Mcp{server, tool}` would put the same fact on screen twice.
pub fn decode_tool(name: &str, input: &Value) -> ToolCall {
    let text = |key: &str| tool::text(input, key).unwrap_or_default();
    match name {
        "Bash" => ToolCall::Run {
            command: text("command"),
        },
        "Read" => ToolCall::Read {
            path: text("file_path"),
        },
        "Write" => ToolCall::Write {
            path: text("file_path"),
        },
        // `NotebookEdit` names its file `notebook_path`; a key set rather
        // than a second arm so neither spelling can go missing.
        "Edit" | "MultiEdit" | "NotebookEdit" => ToolCall::Edit {
            path: tool::text_any(input, &["file_path", "notebook_path"]).unwrap_or_default(),
        },
        "Grep" => ToolCall::Grep {
            pattern: text("pattern"),
        },
        "Glob" => ToolCall::Glob {
            pattern: text("pattern"),
        },
        "WebFetch" => ToolCall::Fetch { url: text("url") },
        "WebSearch" => ToolCall::WebSearch {
            query: text("query"),
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: tool::text(t, "content").unwrap_or_default(),
                    // The CLI's own word. `in_progress` is not done.
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // The CLI has spelled its subagent spawn both ways across versions.
        "Task" | "Agent" => ToolCall::Task {
            description: text("description"),
        },
        _ => ToolCall::Unknown { name: name.into() },
    }
}

/// Flatten a `tool_result` block's content to text, bounded.
///
/// The provider sends either a bare string (what every call in the
/// 2026-08-18 capture corpus does) or the API's array-of-blocks form. Both
/// are real; the array form keeps only its `text` blocks, joined by
/// newlines, because an image in a tool result has no text to state and a
/// row that rendered `[object]` would be worse than one that renders
/// nothing.
fn tool_result_text(block: &Value) -> String {
    let raw = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    crate::events::cap_payload(&raw)
}

/// Prompt tokens carried by one Messages-API `usage` object: the three
/// disjoint input buckets added (Slice CC). `None` when none of them is
/// present — an object that states no input side states no prompt size.
fn prompt_tokens(usage: Option<&Value>) -> Option<u64> {
    let usage = usage?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    let (fresh, read, write) = (
        count("input_tokens"),
        count("cache_read_input_tokens"),
        count("cache_creation_input_tokens"),
    );
    if fresh.is_none() && read.is_none() && write.is_none() {
        return None;
    }
    Some(fresh.unwrap_or(0) + read.unwrap_or(0) + write.unwrap_or(0))
}

fn malformed(line: &str, error: String) -> SessionEvent {
    let mut line = line.to_string();
    if line.len() > MALFORMED_LINE_CAP {
        let mut cut = MALFORMED_LINE_CAP;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
    }
    SessionEvent::Malformed { line, error }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim lines from the 2026-08-18 capture of claude 2.1.226 in
    /// eave's exact spawn posture (`docs/research/approvals-capture-2026-08-18/`).
    /// Parsing the real bytes, not a hand-written approximation, is the
    /// whole point: the fields we depend on (`blocked_path`, the nested
    /// `request`, the envelope-level `request_id`) are exactly where the
    /// CLI actually puts them.
    const REAL_BASH_REQUEST: &str = r#"{"type":"control_request","request_id":"df432c25-2da3-4a94-977d-99a1bf3931be","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"echo hello-from-bash > out.txt","description":"Write \"hello-from-bash\" to out.txt"},"description":"Write \"hello-from-bash\" to out.txt","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"echo hello-from-bash *"}],"behavior":"allow","destination":"localSettings"},{"type":"addDirectories","directories":["/tmp/pc3"],"destination":"session"}],"blocked_path":"/tmp/pc3/out.txt","tool_use_id":"toolu_01Svext1o24ASnB6tJ8C4CLF"}}"#;

    /// Verbatim from the 2026-08-31 capture of claude 2.1.251 in eave's
    /// exact spawn posture, driving Slice AG1 #10. `isReplay: true` is
    /// what `--replay-user-messages` puts on the echo of a message eave
    /// itself sent, and `uuid` — on the OUTER frame, not on `message` —
    /// is the address `rewind_conversation` takes.
    const REAL_USER_ECHO: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Say exactly: ALPHA"}]},"session_id":"c5d65893-ff4c-44b7-842e-fed21927f89b","parent_tool_use_id":null,"uuid":"fb99bae1-4e74-4061-964c-c05b50d897c5","timestamp":"2026-08-31T13:47:52.448Z","isReplay":true}"#;

    #[test]
    fn a_user_echo_carries_the_frame_uuid_that_a_rewind_is_addressed_with() {
        let mut s = ClaudeStream::new();
        let out = s.parse_line(REAL_USER_ECHO);
        assert!(
            matches!(
                out.as_slice(),
                [SessionEvent::UserEcho { text, provider_msg_id: Some(id) }]
                    if text == "Say exactly: ALPHA"
                        && id == "fb99bae1-4e74-4061-964c-c05b50d897c5"
            ),
            "the echo must carry the OUTER frame's uuid: {out:?}"
        );
    }

    /// A `tool_result` rides a `user` frame too, and it has a uuid — but it
    /// is not a message anybody can rewind to, and eave has no `Row::User`
    /// for it. Naming it would offer a control that addresses the wrong
    /// thing.
    #[test]
    fn a_tool_result_on_a_user_frame_is_never_given_a_rewind_address() {
        let mut s = ClaudeStream::new();
        s.parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"uuid":"11111111-1111-1111-1111-111111111111"}"#;
        let out = s.parse_line(line);
        assert!(
            !out.iter()
                .any(|e| matches!(e, SessionEvent::UserEcho { .. })),
            "a tool result is not a user message: {out:?}"
        );
    }

    /// The other five providers name nothing addressable, and the parser
    /// must not invent one when the CLI omits it either.
    #[test]
    fn a_user_echo_without_a_uuid_is_honestly_unaddressable() {
        let mut s = ClaudeStream::new();
        let out = s.parse_line(r#"{"type":"user","message":{"role":"user","content":"plain"}}"#);
        assert!(
            matches!(
                out.as_slice(),
                [SessionEvent::UserEcho { text, provider_msg_id: None }] if text == "plain"
            ),
            "no uuid on the frame means no address: {out:?}"
        );
    }

    const REAL_CANCEL: &str =
        r#"{"type":"control_cancel_request","request_id":"d9c7cd34-146f-4f74-ac01-db5258ee4070"}"#;

    #[test]
    fn a_real_permission_request_states_the_tool_the_input_and_the_path() {
        let mut s = ClaudeStream::new();
        let events = s.parse_line(REAL_BASH_REQUEST);
        match events.as_slice() {
            [SessionEvent::ApprovalRequested {
                request_id,
                tool,
                display_name,
                description,
                path,
                input_json,
                suggestions_json,
            }] => {
                assert_eq!(request_id, "df432c25-2da3-4a94-977d-99a1bf3931be");
                assert_eq!(tool, "Bash");
                assert_eq!(display_name, "Bash");
                assert_eq!(description, "Write \"hello-from-bash\" to out.txt");
                // The path a Bash redirect would touch exists ONLY in
                // `blocked_path` — it is not in the tool input at all.
                assert_eq!(path.as_deref(), Some("/tmp/pc3/out.txt"));
                assert!(input_json.contains("echo hello-from-bash > out.txt"));
                assert!(suggestions_json
                    .as_ref()
                    .unwrap()
                    .contains("addDirectories"));
            }
            other => panic!("expected one ApprovalRequested, got {other:?}"),
        }
        assert_eq!(s.pending_approvals(), 1);
        assert!(s.approval_is_open("df432c25-2da3-4a94-977d-99a1bf3931be"));
    }

    #[test]
    fn a_write_request_takes_its_path_from_the_tool_input() {
        let mut s = ClaudeStream::new();
        let line = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write","input":{"file_path":"/repo/note.txt","content":"hi"},"description":"note.txt","tool_use_id":"toolu_1"}}"#;
        match s.parse_line(line).as_slice() {
            [SessionEvent::ApprovalRequested {
                path,
                suggestions_json,
                ..
            }] => {
                assert_eq!(path.as_deref(), Some("/repo/note.txt"));
                assert!(suggestions_json.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_cancel_withdraws_the_pending_request() {
        let mut s = ClaudeStream::new();
        s.parse_line(REAL_BASH_REQUEST);
        // A cancel for a different id leaves ours standing.
        s.parse_line(REAL_CANCEL);
        assert_eq!(s.pending_approvals(), 1);
        let mine = r#"{"type":"control_cancel_request","request_id":"df432c25-2da3-4a94-977d-99a1bf3931be"}"#;
        match s.parse_line(mine).as_slice() {
            [SessionEvent::ApprovalWithdrawn { request_id, reason }] => {
                assert_eq!(request_id, "df432c25-2da3-4a94-977d-99a1bf3931be");
                assert!(!reason.is_empty(), "a withdrawal must say why");
            }
            other => panic!("got {other:?}"),
        }
        assert_eq!(s.pending_approvals(), 0);
        assert!(!s.approval_is_open("df432c25-2da3-4a94-977d-99a1bf3931be"));
    }

    /// Enabling the permission channel also enables `AskUserQuestion` and
    /// the plan-mode tools (capture C vs A). Their dialog requests arrive
    /// here as a different subtype and must be recognised — an unanswered
    /// one parks the session exactly like an unanswered permission
    /// (Slice K amendment 2).
    #[test]
    fn a_dialog_request_is_recognised_rather_than_ignored() {
        let mut s = ClaudeStream::new();
        let line = r#"{"type":"control_request","request_id":"d1","request":{"subtype":"request_user_dialog","kind":"ask_user_question"}}"#;
        match s.parse_line(line).as_slice() {
            [SessionEvent::DialogRefused { request_id, dialog }] => {
                assert_eq!(request_id, "d1");
                assert_eq!(dialog, "request_user_dialog");
            }
            other => panic!("got {other:?}"),
        }
        // A dialog is not a permission question: nothing to answer here.
        assert_eq!(s.pending_approvals(), 0);
    }

    #[test]
    fn init_carries_the_permission_mode_the_cli_reports() {
        let mut s = ClaudeStream::new();
        let line =
            r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default"}"#;
        match s.parse_line(line).as_slice() {
            [SessionEvent::Init {
                session_id,
                permission_mode,
                ..
            }] => {
                assert_eq!(session_id, "abc");
                assert_eq!(permission_mode.as_deref(), Some("default"));
            }
            other => panic!("got {other:?}"),
        }
        // A CLI that states no mode leaves it unproven — never assumed.
        let bare = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        match ClaudeStream::new().parse_line(bare).as_slice() {
            [SessionEvent::Init {
                permission_mode, ..
            }] => assert!(permission_mode.is_none()),
            other => panic!("got {other:?}"),
        }
    }

    /// **Slice TN1.** Claude Code states its fast lane on `init`; both
    /// lines are cut from real frames of 2026-09-02 — the second is what
    /// the same CLI answers when spawned with eave's opt-in on a plan
    /// whose extra usage is off.
    #[test]
    fn init_carries_the_clis_own_fast_lane_answer() {
        let asked = r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default","fast_mode_state":"off","fast_mode_disabled_reason":"extra_usage_disabled"}"#;
        match ClaudeStream::new().parse_line(asked).as_slice() {
            [SessionEvent::Init { fast_lane, .. }] => {
                let lane = fast_lane.as_ref().expect("stated");
                assert_eq!(lane.state, "off");
                assert_eq!(lane.refused(), Some("extra_usage_disabled"));
            }
            other => panic!("got {other:?}"),
        }
        let unasked = r#"{"type":"system","subtype":"init","session_id":"abc","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required"}"#;
        match ClaudeStream::new().parse_line(unasked).as_slice() {
            [SessionEvent::Init { fast_lane, .. }] => {
                assert_eq!(
                    fast_lane.as_ref().unwrap().refused(),
                    None,
                    "nothing was asked"
                );
            }
            other => panic!("got {other:?}"),
        }
        let older = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        match ClaudeStream::new().parse_line(older).as_slice() {
            [SessionEvent::Init { fast_lane, .. }] => assert!(fast_lane.is_none()),
            other => panic!("got {other:?}"),
        }
    }

    /// **Slice AR2.** The CLI announces its own complete inventory in the
    /// very first frame it writes, and this parser used to throw it away.
    ///
    /// The line below is a cut-down copy of a real one, captured
    /// 2026-08-20 by spawning `claude` with eave's exact headless argv from
    /// `/tmp` and reading the first stdout line. The real frame carried 68
    /// names; three of them are here, including the plugin-namespaced shape
    /// and the one name in the operator's inventory that begins with an
    /// underscore.
    #[test]
    fn init_carries_the_clis_own_slash_command_inventory() {
        let mut s = ClaudeStream::new();
        let line = r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default","slash_commands":["context","craft-skills:seams","__remote-workflow"],"skills":["craft-skills:seams"],"tools":["Bash"]}"#;
        match s.parse_line(line).as_slice() {
            [SessionEvent::Init { slash_commands, .. }] => {
                assert_eq!(
                    slash_commands,
                    &["context", "craft-skills:seams", "__remote-workflow"],
                    "the frame's own order and its own names, unedited"
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A CLI that does not announce an inventory has announced nothing, and
    /// an empty list is exactly that fact. The two facts eave cannot start
    /// a session without — the id and the posture — must survive a frame
    /// whose `slash_commands` is missing, empty, or the wrong shape
    /// entirely.
    #[test]
    fn an_init_without_a_usable_inventory_still_carries_the_id_and_the_posture() {
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default"}"#,
            r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default","slash_commands":[]}"#,
            r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default","slash_commands":"context"}"#,
            r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default","slash_commands":[{"name":"context"},"model"]}"#,
        ] {
            match ClaudeStream::new().parse_line(line).as_slice() {
                [SessionEvent::Init {
                    session_id,
                    permission_mode,
                    slash_commands,
                    ..
                }] => {
                    assert_eq!(session_id, "abc");
                    assert_eq!(permission_mode.as_deref(), Some("default"));
                    // The object in the last case is skipped; the string
                    // beside it is kept. Partial is better than nothing and
                    // far better than a malformed `init`.
                    assert!(
                        slash_commands.iter().all(|c| c == "model"),
                        "non-strings must be skipped, not guessed at: {slash_commands:?}"
                    );
                }
                other => panic!("{line} gave {other:?}"),
            }
        }
    }

    /// The CLI echoes our own decisions back on stdout. The engine
    /// journals the decision when it reaches the pipe, so the echo would
    /// be a second record of one act — dropped on purpose.
    #[test]
    fn our_own_decision_echo_is_not_journaled_twice() {
        let mut s = ClaudeStream::new();
        let echo = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"allow"}}}"#;
        assert!(s.parse_line(echo).is_empty());
    }

    /// **The bug this slice found.** Under `--include-partial-messages`
    /// the CLI announces a `tool_use` with `"input": {}` and sends the real
    /// arguments a line later on the assistant snapshot. Deduplicating on
    /// the tool id alone emitted the empty announcement and dropped the
    /// arguments, so EVERY Claude tool row in eave rendered as a name and a
    /// status word — the exact defect the tool detail exists to prevent.
    ///
    /// Both lines are taken from
    /// `docs/research/approvals-capture-2026-08-18/capE-bash.jsonl`
    /// (lines 21 and 26), in the order the CLI sends them.
    #[test]
    fn a_tool_announced_before_its_arguments_states_them_when_they_arrive() {
        let mut s = ClaudeStream::new();
        let announced = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01S","name":"Bash","input":{}}}}"#;
        match s.parse_line(announced).as_slice() {
            [SessionEvent::ToolUseStarted { id, name, call, .. }] => {
                assert_eq!(id, "toolu_01S");
                assert_eq!(name, "Bash");
                assert_eq!(
                    call.as_ref(),
                    Some(&ToolCall::Run {
                        command: String::new()
                    }),
                    "the tool IS recognised; it just has no arguments yet"
                );
            }
            other => panic!("expected an announcement, got {other:?}"),
        }
        assert_eq!(s.in_flight(), 1);

        let snapshot = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01S","name":"Bash","input":{"command":"echo hello-from-bash > out.txt","description":"Write to out.txt"}}]}}"#;
        match s.parse_line(snapshot).as_slice() {
            [SessionEvent::ToolUseStarted {
                id, call, input, ..
            }] => {
                assert_eq!(
                    id, "toolu_01S",
                    "the same tool, restated — not a second one"
                );
                assert_eq!(
                    call.as_ref(),
                    Some(&ToolCall::Run {
                        command: "echo hello-from-bash > out.txt".into()
                    })
                );
                assert!(
                    input.contains("hello-from-bash"),
                    "the raw fact of record too"
                );
            }
            other => panic!("expected the arguments to arrive, got {other:?}"),
        }
        assert_eq!(s.in_flight(), 1, "one tool ran, not two");

        // A third sighting of the same arguments says nothing new.
        assert!(
            s.parse_line(snapshot).is_empty(),
            "the arguments were already recorded"
        );
        assert_eq!(s.in_flight(), 1);
    }

    /// Every tool the Claude CLI ships that eave can name, decoded from the
    /// key the CLI actually sends. An unlisted tool is named, never guessed.
    #[test]
    fn claude_tools_decode_to_their_one_argument() {
        let d = |name: &str, input: serde_json::Value| decode_tool(name, &input);
        assert_eq!(
            d("Read", serde_json::json!({"file_path": "/src/lib.rs"})),
            ToolCall::Read {
                path: "/src/lib.rs".into()
            }
        );
        assert_eq!(
            d(
                "NotebookEdit",
                serde_json::json!({"notebook_path": "/nb.ipynb"})
            ),
            ToolCall::Edit {
                path: "/nb.ipynb".into()
            },
            "NotebookEdit spells its file differently and must not be lost"
        );
        assert_eq!(
            d(
                "Grep",
                serde_json::json!({"pattern": "TODO", "path": "/src"})
            ),
            ToolCall::Grep {
                pattern: "TODO".into()
            }
        );
        assert_eq!(
            d(
                "TodoWrite",
                serde_json::json!({"todos": [
                    {"content": "read the brief", "status": "completed"},
                    {"content": "fix the parser", "status": "in_progress"}
                ]})
            )
            .detail()
            .as_deref(),
            Some("1/2 · fix the parser"),
            "`in_progress` is not `completed`"
        );
        assert_eq!(
            d(
                "Task",
                serde_json::json!({"description": "survey comet", "prompt": "…"})
            ),
            ToolCall::Task {
                description: "survey comet".into()
            }
        );
        // MCP, and anything else: named under the CLI's own name, which is
        // what the row's name column already renders.
        assert_eq!(
            d(
                "mcp__playwright__browser_click",
                serde_json::json!({"ref": "e12"})
            ),
            ToolCall::Unknown {
                name: "mcp__playwright__browser_click".into()
            }
        );
    }
}

/// **Slice CC — token accounting, from the capture corpus.**
///
/// Every line below is verbatim from a real `claude` 2.1.226 run in eave's
/// own spawn posture. Two captures are used because they answer two
/// different questions and neither can answer the other's:
///
/// - `docs/research/usage-capture-2026-08-20/two-turn.jsonl` — ONE process,
///   TWO prompts. It is the only thing that can settle whether
///   `result.usage` accumulates across turns, and it says it does not.
/// - `docs/research/claude-stream-capture-tool-turn.jsonl` — one turn with
///   a tool in it, so TWO model requests. It is the only thing that can
///   separate per-turn cost from context occupancy, and it says they are
///   53,820 and 26,967 in the same turn.
#[cfg(test)]
mod usage_tests {
    use super::*;
    use crate::events::{ContextWindow, TurnTokens};

    /// Verbatim, `two-turn.jsonl`: the SECOND turn of a two-turn process.
    const TURN2_DELTA: &str = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null,"stop_details":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":20,"cache_read_input_tokens":26395,"output_tokens":4,"output_tokens_details":{"thinking_tokens":0},"iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":26395,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":20},"type":"message"}]},"context_management":{"applied_edits":[]}},"session_id":"67bfefdc-1b4f-41f8-be19-f4c73db5aa66","parent_tool_use_id":null,"uuid":"c76f96b8-d1ae-4b5b-81a5-9338742ef531"}"#;

    const TURN2_RESULT: &str = r#"{"is_error":false,"duration_api_ms":4556,"num_turns":1,"stop_reason":"end_turn","session_id":"67bfefdc-1b4f-41f8-be19-f4c73db5aa66","total_cost_usd":0.12714599999999998,"usage":{"input_tokens":2,"cache_creation_input_tokens":20,"cache_read_input_tokens":26395,"output_tokens":4,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":20,"ephemeral_5m_input_tokens":0},"inference_geo":"not_available","iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":26395,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":20},"type":"message"}],"speed":"standard"},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":524,"outputTokens":14,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000594,"contextWindow":200000,"maxOutputTokens":32000,"canonicalModel":"claude-haiku-4-5","provider":"firstParty"},"claude-opus-5[1m]":{"inputTokens":4,"outputTokens":9,"cacheReadInputTokens":42294,"cacheCreationInputTokens":10516,"webSearchRequests":0,"costUSD":0.126552,"contextWindow":1000000,"maxOutputTokens":64000,"canonicalModel":"claude-opus-5","provider":"firstParty"}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subtype":"success","api_error_status":null,"result":"beta","ttft_ms":1649,"ttft_stream_ms":966,"time_to_request_ms":8,"type":"result","duration_ms":1655,"uuid":"0478c9d4-b14a-4ad0-bad8-3ef1f28ecfd9"}"#;

    /// Verbatim, `two-turn.jsonl`: the `init` that names the model the
    /// `modelUsage` map must be indexed by. Trimmed of `tools`,
    /// `slash_commands` and `mcp_servers`, which this test does not read.
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/private/tmp","session_id":"67bfefdc-1b4f-41f8-be19-f4c73db5aa66","model":"claude-opus-5[1m]","permissionMode":"default"}"#;

    /// Verbatim, `claude-stream-capture-tool-turn.jsonl`: the two model
    /// requests of a single turn that ran one tool.
    const TOOL_TURN_DELTA_1: &str = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"stop_details":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":10924,"cache_read_input_tokens":15927,"output_tokens":108,"output_tokens_details":{"thinking_tokens":17},"iterations":[{"input_tokens":2,"output_tokens":108,"cache_read_input_tokens":15927,"cache_creation_input_tokens":10924,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":10924},"type":"message"}]},"context_management":{"applied_edits":[]}},"session_id":"4731DB7E-B314-4B77-B5A4-0DA3E6B4865C","parent_tool_use_id":null,"uuid":"edbad992-6ddc-463f-8520-e8a12ca01f77"}"#;

    const TOOL_TURN_DELTA_2: &str = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null,"stop_details":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":114,"cache_read_input_tokens":26851,"output_tokens":4,"output_tokens_details":{"thinking_tokens":0},"iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":26851,"cache_creation_input_tokens":114,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":114},"type":"message"}]},"context_management":{"applied_edits":[]}},"session_id":"4731DB7E-B314-4B77-B5A4-0DA3E6B4865C","parent_tool_use_id":null,"uuid":"e0aadb36-b417-43d1-bc8d-e906469ffff8"}"#;

    /// Verbatim from the same capture, with only the `result` prose (a 12 KB
    /// markdown answer this test does not read) removed.
    const TOOL_TURN_RESULT: &str = r#"{"is_error":false,"duration_api_ms":5780,"num_turns":2,"stop_reason":"end_turn","session_id":"4731DB7E-B314-4B77-B5A4-0DA3E6B4865C","total_cost_usd":0.269784,"usage":{"input_tokens":4,"cache_creation_input_tokens":11038,"cache_read_input_tokens":42778,"output_tokens":112,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":11038,"ephemeral_5m_input_tokens":0},"inference_geo":"not_available","iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":26851,"cache_creation_input_tokens":114,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":114},"type":"message"}],"speed":"standard"},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":536,"outputTokens":14,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000606,"contextWindow":200000,"maxOutputTokens":32000,"canonicalModel":"claude-haiku-4-5","provider":"firstParty"},"claude-fable-5":{"inputTokens":4,"outputTokens":112,"cacheReadInputTokens":42778,"cacheCreationInputTokens":11038,"webSearchRequests":0,"costUSD":0.26917800000000003,"contextWindow":1000000,"maxOutputTokens":64000,"canonicalModel":"claude-fable-5","provider":"firstParty"}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subtype":"success","api_error_status":null,"ttft_ms":2247,"ttft_stream_ms":2066,"time_to_request_ms":17,"type":"result","duration_ms":13430,"uuid":"3767bd82-f187-4c5d-823b-0b9f8d032241"}"#;

    fn usage_of(events: &[SessionEvent]) -> (Option<TurnTokens>, Option<ContextWindow>) {
        match events
            .iter()
            .find(|e| matches!(e, SessionEvent::TokenUsage { .. }))
        {
            Some(SessionEvent::TokenUsage { turn, context }) => (*turn, *context),
            _ => panic!("no TokenUsage in {events:?}"),
        }
    }

    /// The turn's cost is stated in full: the whole prompt, not the
    /// uncached remainder the API's `input_tokens` field names.
    #[test]
    fn a_settled_turn_states_the_whole_prompt_not_the_uncached_remainder() {
        let mut s = ClaudeStream::new();
        s.parse_line(INIT);
        s.parse_line(TURN2_DELTA);
        let events = s.parse_line(TURN2_RESULT);
        let (turn, context) = usage_of(&events);
        let turn = turn.expect("the result line stated usage");
        // 2 fresh + 20 written + 26,395 read. Reporting the bare
        // `input_tokens` here would say the model read two tokens.
        assert_eq!(turn.input, Some(26_417));
        assert_eq!(turn.output, Some(4));
        assert_eq!(turn.cache_read, Some(26_395));
        assert_eq!(turn.cache_write, Some(20));
        // The ceiling comes off the `modelUsage` row for the model `init`
        // named — NOT the haiku row beside it, which advertises 200,000.
        assert_eq!(
            context,
            Some(ContextWindow {
                used_tokens: 26_417,
                limit_tokens: Some(1_000_000),
            })
        );
        // Usage lands before the bookend.
        assert!(matches!(
            events.last(),
            Some(SessionEvent::TurnCompleted { .. })
        ));
    }

    /// **The whole point of the slice.** One turn, two model requests: the
    /// bill is 53,820 input tokens and the context holds 26,967. A build
    /// that read occupancy off the result line would report the window
    /// twice as full as it is, and would do so more the more tools ran.
    /// Claude streams reasoning as `thinking_delta`, and eave dropped every
    /// one of them until 2026-08-30.
    ///
    /// The frame is the same `content_block_delta` that carries answer
    /// text; only the KEY differs. Reading `delta.text` and stopping meant
    /// the reasoning box — which has shipped for slices — could only ever
    /// be filled by an ACP agent. Shapes captured from eave's exact argv.
    #[test]
    fn claude_reasoning_is_decoded_and_not_confused_with_its_answer() {
        let mut s = ClaudeStream::new();
        let ev = |s: &mut ClaudeStream, line: &str| s.parse_line(line);

        // The block opening carries no text of its own.
        assert!(
            ev(&mut s, r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#).is_empty(),
            "a thinking block's start is not a chunk"
        );
        let got = ev(
            &mut s,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"17 x 23. "}}}"#,
        );
        assert_eq!(
            got,
            vec![SessionEvent::ThoughtDelta {
                text: "17 x 23. ".into()
            }],
            "a thinking_delta is reasoning"
        );
        // **What the CLI actually sends today**, captured 2026-08-30: the
        // content is redacted and only a token estimate rides along. An
        // empty chunk must produce NO event — otherwise every turn writes
        // a run of empty records and starts the reasoning clock for a
        // trace that will never have a word in it.
        assert!(
            ev(&mut s, r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#).is_empty(),
            "a redacted thinking chunk journalled an empty thought"
        );
        // A signature is a cryptographic receipt for the block, not text.
        assert!(
            ev(&mut s, r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc123=="}}}"#).is_empty(),
            "a signature_delta rendered as reasoning would put base64 in the box"
        );
        // And the answer still decodes as the answer.
        assert_eq!(
            ev(
                &mut s,
                r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"391"}}}"#
            ),
            vec![SessionEvent::TextDelta { text: "391".into() }],
            "answer text must not become reasoning"
        );
    }

    #[test]
    fn a_turn_with_a_tool_bills_the_prompt_twice_and_the_context_holds_it_once() {
        let mut s = ClaudeStream::new();
        s.parse_line(r#"{"type":"system","subtype":"init","session_id":"4731DB7E","model":"claude-fable-5","permissionMode":"default"}"#);
        s.parse_line(TOOL_TURN_DELTA_1);
        s.parse_line(TOOL_TURN_DELTA_2);
        let events = s.parse_line(TOOL_TURN_RESULT);
        let (turn, context) = usage_of(&events);
        let turn = turn.expect("stated");
        assert_eq!(turn.input, Some(53_820), "4 + 11,038 + 42,778 — the bill");
        let context = context.expect("stated");
        assert_eq!(
            context.used_tokens, 26_967,
            "2 + 114 + 26,851 — the LAST request's prompt, which is what is in the window"
        );
        assert!(
            context.used_tokens < turn.input.unwrap(),
            "these are different facts and this capture is where they part"
        );
        assert_eq!(context.limit_tokens, Some(1_000_000));
    }

    /// Two prompts through one process: `result.usage` reports the turn,
    /// not a running total. Turn one billed 5 output tokens and turn two
    /// billed 4 — a cumulative field would have said 9.
    #[test]
    fn the_result_lines_usage_is_per_turn_and_not_a_running_total() {
        let mut s = ClaudeStream::new();
        s.parse_line(INIT);
        // Turn one, verbatim from the same capture.
        s.parse_line(r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":2,"cache_creation_input_tokens":10496,"cache_read_input_tokens":15899,"output_tokens":5}}}"#);
        let first = s.parse_line(r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":2,"cache_creation_input_tokens":10496,"cache_read_input_tokens":15899,"output_tokens":5},"modelUsage":{"claude-opus-5[1m]":{"cacheReadInputTokens":15899,"contextWindow":1000000}}}"#);
        assert_eq!(usage_of(&first).0.unwrap().output, Some(5));

        s.parse_line(TURN2_DELTA);
        let second = s.parse_line(TURN2_RESULT);
        assert_eq!(
            usage_of(&second).0.unwrap().output,
            Some(4),
            "9 here would mean the field accumulates and every turn after \
             the first would be over-reported"
        );
    }

    /// A `result` line with no `usage` at all — a turn that never reached
    /// the model — states nothing rather than zero.
    #[test]
    fn a_turn_that_never_reached_the_model_reports_no_tokens() {
        let mut s = ClaudeStream::new();
        let events =
            s.parse_line(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::TokenUsage { .. })),
            "silence is the honest answer: {events:?}"
        );
        assert_eq!(events.len(), 1, "just the bookend");
    }

    /// The ceiling is only claimed when the map has the session's own
    /// model in it. An unrecognised model leaves the level stated and the
    /// ceiling unstated, rather than borrowing a number from whichever
    /// other model happens to be in the map.
    #[test]
    fn an_unmatched_model_leaves_the_ceiling_unstated() {
        let mut s = ClaudeStream::new();
        s.parse_line(
            r#"{"type":"system","subtype":"init","session_id":"x","model":"claude-something-new"}"#,
        );
        s.parse_line(TURN2_DELTA);
        let (_, context) = usage_of(&s.parse_line(TURN2_RESULT));
        let context = context.expect("the level is still known");
        assert_eq!(context.used_tokens, 26_417);
        assert_eq!(context.limit_tokens, None);
        assert_eq!(context.fraction(), None);
    }
}
