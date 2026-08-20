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

const MALFORMED_LINE_CAP: usize = 512;

#[derive(Default)]
pub struct ClaudeStream {
    outstanding: HashSet<String>,
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
            Some("control_cancel_request") => {
                match v.get("request_id").and_then(Value::as_str) {
                    Some(id) => vec![SessionEvent::ApprovalWithdrawn {
                        request_id: {
                            self.pending.remove(id);
                            id.to_string()
                        },
                        reason: "the provider withdrew the request".into(),
                    }],
                    None => vec![malformed(line, "control_cancel_request without request_id".into())],
                }
            }
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
                    suggestions_json: req
                        .get("permission_suggestions")
                        .map(|s| s.to_string()),
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
            Some("content_block_delta") => {
                match ev["delta"].get("text").and_then(Value::as_str) {
                    Some(text) => vec![SessionEvent::TextDelta {
                        text: text.to_string(),
                    }],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn parse_assistant(&mut self, msg: &Value) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        for block in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
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
                            if let Some(id) =
                                block.get("tool_use_id").and_then(Value::as_str)
                            {
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

    /// Register a tool_use block; emits `ToolUseStarted` only on first sight
    /// (the same id arrives via both `content_block_start` and the complete
    /// assistant snapshot).
    fn note_tool_use(&mut self, block: &Value) -> Option<SessionEvent> {
        let id = block.get("id").and_then(Value::as_str)?;
        if self.outstanding.insert(id.to_string()) {
            Some(SessionEvent::ToolUseStarted {
                id: id.to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                // Re-serialised compactly rather than kept verbatim: the
                // provider's whitespace is not a fact about the tool call,
                // and a compact form is what the cap is measured against.
                // A missing `input` is an empty string, not `"null"` — the
                // row then states the tool and nothing more, which is
                // accurate.
                input: block
                    .get("input")
                    .filter(|v| !v.is_null())
                    .map(|v| crate::events::cap_payload(&v.to_string()))
                    .unwrap_or_default(),
            })
        } else {
            None
        }
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
                assert!(suggestions_json.as_ref().unwrap().contains("addDirectories"));
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
            [SessionEvent::ApprovalRequested { path, suggestions_json, .. }] => {
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
        let line = r#"{"type":"system","subtype":"init","session_id":"abc","permissionMode":"default"}"#;
        match s.parse_line(line).as_slice() {
            [SessionEvent::Init { session_id, permission_mode }] => {
                assert_eq!(session_id, "abc");
                assert_eq!(permission_mode.as_deref(), Some("default"));
            }
            other => panic!("got {other:?}"),
        }
        // A CLI that states no mode leaves it unproven — never assumed.
        let bare = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        match ClaudeStream::new().parse_line(bare).as_slice() {
            [SessionEvent::Init { permission_mode, .. }] => assert!(permission_mode.is_none()),
            other => panic!("got {other:?}"),
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
}
