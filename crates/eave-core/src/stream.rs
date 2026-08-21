//! Claude Code stream-JSON line parser.
//!
//! Contract observed from the CLI in `-p --input-format stream-json
//! --output-format stream-json --verbose --include-partial-messages` mode
//! (research: waku.md §2; waku-core/src/driver/claude.rs is the proven
//! native path). The parser also owns the outstanding-`tool_use` set —
//! the "nothing in flight" input to the attention clocks (spec 01).

use std::collections::HashSet;

use serde_json::Value;

use crate::events::SessionEvent;
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
    /// Permission requests the CLI is parked on right now (Slice K). Held
    /// here so the adapter can answer a still-open question and ignore a
    /// stale one; the durable truth is the journal.
    pending: HashSet<String>,
    /// The envelope's `request_id` lives one level up from the `request`
    /// object; stashed while parsing so the nested parse can name it.
    pending_request_id: Option<String>,
}

impl ClaudeStream {
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
                            vec![SessionEvent::Init {
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
            Some("user") => self.parse_user(&v["message"]),
            Some("result") => vec![SessionEvent::TurnCompleted {
                subtype: v
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            }],
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

    fn parse_stream_event(&mut self, ev: &Value) -> Vec<SessionEvent> {
        match ev.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let cb = &ev["content_block"];
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.note_tool_use(cb).into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            Some("content_block_delta") => match ev["delta"].get("text").and_then(Value::as_str) {
                Some(text) => vec![SessionEvent::TextDelta {
                    text: text.to_string(),
                }],
                None => Vec::new(),
            },
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

    fn parse_user(&mut self, msg: &Value) -> Vec<SessionEvent> {
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
                                    });
                                }
                            }
                        }
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                out.push(SessionEvent::UserEcho {
                                    text: text.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(Value::String(text)) => out.push(SessionEvent::UserEcho { text: text.clone() }),
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
