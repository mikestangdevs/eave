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
                            }]
                        }
                        None => vec![malformed(line, "init without session_id".into())],
                    }
                } else {
                    Vec::new()
                }
            }
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
            }],
            None => vec![malformed(line, "missing type tag".into())],
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
            })
        } else {
            None
        }
    }
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
