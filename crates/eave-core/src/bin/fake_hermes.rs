//! fake-hermes: a stand-in for the `hermes` CLI that speaks ACP.
//!
//! `fake_agent.rs` speaks Claude's stream-JSON on stdout and answers no
//! subcommands. Hermes is a different shape in both respects — newline
//! JSON-RPC 2.0 with request/reply correlation and agent→client requests,
//! plus the `config get` and `approvals test` subcommands eave's posture
//! preflight shells out to — so it gets its own stand-in rather than a
//! sixth `FAKE_MODE` bolted onto a parser it does not share.
//!
//! Everything the real binary does that eave depends on is here, and
//! nothing else. Behaviour is chosen via env (`FAKE_HERMES_*`) rather than
//! argv, so the real `HermesSession::spawn` argv — `acp` and all — is what
//! the tests actually run.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn emit(v: &Value) {
    let mut out = std::io::stdout().lock();
    let mut line = v.to_string();
    line.push('\n');
    out.write_all(line.as_bytes()).unwrap();
    out.flush().unwrap();
}

fn emit_raw(s: &str) {
    let mut out = std::io::stdout().lock();
    out.write_all(s.as_bytes()).unwrap();
    out.write_all(b"\n").unwrap();
    out.flush().unwrap();
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}

/// `hermes config get --json <key>`.
///
/// Reproduces the real binary's two shapes, which is the whole reason
/// eave's `config_get` discriminates on "did it parse as JSON": a set key
/// prints JSON, an unset key prints an English sentence, **and both exit
/// 0** (verified against Hermes 0.20.4, 2026-08-20).
fn config_get(key: &str) -> ! {
    if key == "terminal.env_type" {
        match std::env::var("FAKE_HERMES_ENV_TYPE") {
            Ok(v) => emit_raw(&json!(v).to_string()),
            Err(_) => emit_raw(&format!("Config key not set: {key}")),
        }
    } else {
        emit_raw(&format!("Config key not set: {key}"));
    }
    std::process::exit(0)
}

/// `hermes approvals test --json ... -- <command>`.
///
/// `FAKE_HERMES_VERDICT` drives the verdict; the exit code follows the
/// real binary's mapping (0 allow, 2 ask-approval, 3 deny).
fn approvals_test() -> ! {
    let verdict = env("FAKE_HERMES_VERDICT", "ask-approval");
    if verdict == "no-json" {
        // The shape eave must not read as a pass: a binary that answered
        // something other than a verdict.
        emit_raw("hermes: unrecognized arguments");
        std::process::exit(2)
    }
    let code = match verdict.as_str() {
        "allow" => 0,
        "hardline-deny" => 3,
        _ => 2,
    };
    emit(&json!({ "verdict": verdict, "exit_code": code }));
    std::process::exit(code)
}

struct Acp {
    mode: String,
    session_mode: String,
    session_id: String,
}

impl Acp {
    /// Serve one turn: whatever `FAKE_HERMES_MODE` says a turn looks like,
    /// then the `session/prompt` reply that ends it.
    fn prompt(&self, id: &Value) {
        match self.mode.as_str() {
            "normal" => {
                self.chunk("hello from fake hermes");
            }
            "tools" => {
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "call-1",
                        "title": "terminal: cargo test",
                        "kind": "execute",
                        "status": "pending",
                    }},
                }));
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "call-1",
                        "status": "in_progress",
                    }},
                }));
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "call-1",
                        "status": "completed",
                        "content": [{ "type": "content",
                                      "content": { "type": "text", "text": "42 passed" }}],
                    }},
                }));
                self.chunk("done");
            }
            // A tool that starts and then nothing: no completion, no turn
            // reply, ever. The only mode in which `in_flight_tools()` can
            // be sampled deterministically — everywhere else the adapter's
            // parser has already seen the completion by the time the start
            // event reaches the test's end of the channel.
            "tool-pending" => {
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "call-stuck",
                        "title": "terminal: sleep 999",
                        "kind": "execute",
                    }},
                }));
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
            // A tool that starts and is never resolved, but whose TURN
            // ends: that is what eave's orphan sweep is for.
            "orphan-tool" => {
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "call-orphan",
                        "title": "terminal: sleep 999",
                        "kind": "execute",
                    }},
                }));
            }
            // Ask permission for a shell command, then report what came
            // back so a test can assert the decision actually landed.
            "approval" => {
                emit(&json!({
                    "jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
                    "params": {
                        "sessionId": self.session_id,
                        "toolCall": {
                            "toolCallId": "perm-check-1",
                            "title": "Run a dangerous command: rm -rf ./build",
                            "kind": "execute",
                            "status": "pending",
                            "content": [{ "type": "content", "content": {
                                "type": "text",
                                "text": "Run a dangerous command\n$ rm -rf ./build" }}],
                            "rawInput": { "command": "rm -rf ./build",
                                          "description": "Run a dangerous command" },
                        },
                        "options": [
                            { "optionId": "allow_once", "kind": "allow_once", "name": "Allow once" },
                            { "optionId": "allow_always", "kind": "allow_always", "name": "Allow always" },
                            { "optionId": "deny", "kind": "reject_once", "name": "Deny" },
                        ],
                    },
                }));
                let answer = read_json().expect("client answered the permission request");
                let option = answer
                    .get("result")
                    .and_then(|r| r.get("outcome"))
                    .and_then(|o| o.get("optionId"))
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    .to_string();
                self.chunk(&format!("decision={option}"));
            }
            // Ask permission offering an option set eave cannot answer
            // from, so the refusal-to-guess path is reachable.
            "approval-odd-options" => {
                emit(&json!({
                    "jsonrpc": "2.0", "id": 901, "method": "session/request_permission",
                    "params": {
                        "sessionId": self.session_id,
                        "toolCall": { "toolCallId": "perm-check-2", "title": "Something new",
                                      "kind": "other" },
                        "options": [
                            { "optionId": "proceed", "kind": "allow_once", "name": "Proceed" },
                        ],
                    },
                }));
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
            // A client method eave declined at `initialize`.
            "client-request" => {
                emit(&json!({
                    "jsonrpc": "2.0", "id": 902, "method": "fs/read_text_file",
                    "params": { "sessionId": self.session_id, "path": "/etc/passwd" },
                }));
                let _ = read_json();
                self.chunk("asked anyway");
            }
            "malformed" => {
                emit_raw("{not json at all");
                emit_raw(r#"{"jsonrpc":"2.0"}"#);
                self.chunk("still alive");
            }
            // Switch the posture out from under eave mid-session.
            "mode-flip" => {
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": "dont_ask",
                    }},
                }));
            }
            // Slice CC: the two token facts, in the shapes the ACP
            // schema defines — a `usage_update` gauge mid-turn and a
            // first-class `usage` on the settled prompt response. The
            // NUMBERS are this fixture's; the SHAPES are
            // `acp/schema.py`'s `_UsageUpdate` and `Usage`.
            "usage" => {
                self.chunk("counting");
                emit(&json!({
                    "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": self.session_id, "update": {
                        "sessionUpdate": "usage_update",
                        "used": 21884,
                        "size": 200000,
                        "cost": { "amount": 0.02, "currency": "USD" },
                    }},
                }));
                emit(&json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "stopReason": "end_turn",
                    "usage": {
                        "inputTokens": 21884,
                        "outputTokens": 455,
                        "cachedReadTokens": 19000,
                        "cachedWriteTokens": 0,
                        "thoughtTokens": 12,
                        "totalTokens": 22339,
                    },
                }}));
                return;
            }
            _ => {}
        }
        emit(&json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" }}));
    }

    fn chunk(&self, text: &str) {
        emit(&json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": { "sessionId": self.session_id, "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text },
            }},
        }));
    }
}

fn read_json() -> Option<Value> {
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).ok()?;
    if n == 0 {
        return None;
    }
    serde_json::from_str(&line).ok()
}

fn serve_acp() -> ! {
    let mode = env("FAKE_HERMES_MODE", "normal");
    // A binary that dies before it can shake hands — a missing dependency,
    // a bad install, a crash on startup. eave must fail the next send
    // rather than wait on a session that will never exist.
    if mode == "die-at-start" {
        std::process::exit(70);
    }
    let session_mode = env("FAKE_HERMES_SESSION_MODE", "default");
    let mut acp = Acp {
        mode,
        session_mode,
        session_id: String::new(),
    };

    while let Some(msg) = read_json() {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => emit(&json!({
                "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": 1,
                    "agentInfo": { "name": "fake-hermes", "version": "0.0.0" },
                    "agentCapabilities": { "loadSession": true },
                },
            })),
            "session/new" | "session/load" => {
                // `session/load` is asked for an id; `session/new` mints
                // one, exactly as the real adapter does.
                acp.session_id = msg
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| env("FAKE_HERMES_SESSION_ID", "hermes-session-1"));
                let mut result = json!({ "modes": { "currentModeId": acp.session_mode,
                                                    "availableModes": [] }});
                if method == "session/new" {
                    result["sessionId"] = json!(acp.session_id);
                }
                if acp.session_mode == "unstated" {
                    result["modes"] = json!({ "availableModes": [] });
                }
                emit(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            "session/prompt" => acp.prompt(&id),
            _ => emit(&json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("no such method {method}") },
            })),
        }
    }
    std::process::exit(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("acp") => serve_acp(),
        Some("config") if args.get(1).map(String::as_str) == Some("get") => {
            config_get(args.last().map(String::as_str).unwrap_or(""))
        }
        Some("approvals") if args.get(1).map(String::as_str) == Some("test") => approvals_test(),
        other => {
            eprintln!("fake-hermes: unsupported invocation {other:?}");
            std::process::exit(64)
        }
    }
}
