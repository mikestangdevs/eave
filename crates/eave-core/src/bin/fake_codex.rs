//! fake-codex: a scriptable stand-in for `codex app-server`.
//!
//! The sibling of `fake_agent.rs` and built to the same rule — behavior is
//! chosen via env (`FAKE_CODEX_MODE`) rather than argv, so the real
//! `CodexSession::spawn` path, argv and all, is what tests run.
//!
//! It is a **separate binary** rather than another `FAKE_MODE` arm because
//! it speaks a different protocol: `codex app-server` is JSON-RPC over
//! newline-delimited JSON with server-initiated requests, and folding two
//! protocols into one dispatch would make both harder to read than either.
//!
//! Every frame it emits was copied from a live capture of
//! `codex app-server` 0.148.0 or from that binary's own generated JSON
//! Schema (`codex app-server generate-json-schema`) — see the Stage AU
//! report. A fake whose shapes are invented tests the adapter against the
//! author's imagination.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

/// The id `codex app-server` would use for a request it wants answered.
/// An integer, deliberately: JSON-RPC ids are typed, and a client that
/// replies with `"7"` to a request that arrived as `7` is a client the
/// real server would ignore.
const APPROVAL_ID: i64 = 7;
const DIALOG_ID: i64 = 8;
const INFRA_ID: i64 = 9;

fn emit(v: Value) {
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

fn mode() -> String {
    std::env::var("FAKE_CODEX_MODE").unwrap_or_else(|_| "normal".into())
}

/// The posture the fake reports in its `thread/start` result. The real
/// server echoes back what the client asked for; the lying modes are how
/// the tests reach the refusal paths without a CLI that misbehaves.
fn posture(asked: &Value) -> Option<(Value, Value, Value)> {
    match mode().as_str() {
        "posture-lie" => Some((
            json!("never"),
            json!("auto_review"),
            json!("dangerFullAccess"),
        )),
        "posture-sandbox" => Some((json!("untrusted"), json!("user"), json!("workspaceWrite"))),
        "posture-reviewer" => Some((json!("untrusted"), json!("auto_review"), json!("readOnly"))),
        "posture-silent" => None,
        // Everything else behaves like the real server: repeat back the
        // policy that was actually installed, in the server's own
        // spelling for the sandbox (`read-only` in, `readOnly` out).
        _ => {
            let sandbox = match asked.get("sandbox").and_then(Value::as_str) {
                Some("read-only") => "readOnly",
                Some("workspace-write") => "workspaceWrite",
                Some("danger-full-access") => "dangerFullAccess",
                _ => "unknown",
            };
            Some((
                asked.get("approvalPolicy").cloned().unwrap_or(Value::Null),
                asked
                    .get("approvalsReviewer")
                    .cloned()
                    .unwrap_or(Value::Null),
                json!(sandbox),
            ))
        }
    }
}

fn thread_object(id: &str, cwd: &Value) -> Value {
    json!({
        "id": id,
        "sessionId": id,
        "status": { "type": "idle" },
        "cwd": cwd,
        "cliVersion": "0.148.0-fake",
        "turns": [],
    })
}

fn agent_message(text: &str) {
    emit(json!({
        "method": "item/agentMessage/delta",
        "params": { "threadId": "t", "turnId": "u", "itemId": "m1", "delta": text },
    }));
    emit(json!({
        "method": "item/completed",
        "params": {
            "threadId": "t", "turnId": "u", "completedAtMs": 0,
            "item": { "type": "agentMessage", "id": "m1", "text": text },
        },
    }));
}

fn command_item(status: &str, exit_code: Option<i64>, output: Option<&str>) -> Value {
    json!({
        "type": "commandExecution",
        "id": "c1",
        "command": "echo hello",
        "cwd": "/tmp",
        "status": status,
        "exitCode": exit_code,
        "aggregatedOutput": output,
        "commandActions": [],
    })
}

fn turn_completed(status: &str) {
    emit(json!({
        "method": "turn/completed",
        "params": {
            "threadId": "t",
            "turn": { "id": "u", "items": [], "status": status, "error": null },
        },
    }));
}

fn main() {
    let mode = mode();
    let stdin = std::io::stdin();
    // What the client asked for at `thread/start`, so `handshake-report`
    // can hand it back and a test can assert on the posture eave actually
    // requested rather than on the constant it was built from.
    let mut asked_params = Value::Null;
    let mut asked_method = String::new();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let id = v.get("id").cloned();

        match (method, &id) {
            ("initialize", Some(id)) => {
                if mode == "init-reject" {
                    emit(json!({
                        "id": id,
                        "error": { "code": -32600, "message": "fake-codex refuses initialize" },
                    }));
                    continue;
                }
                emit(json!({
                    "id": id,
                    "result": {
                        "userAgent": "fake-codex/0.148.0",
                        "codexHome": "/tmp/fake-codex",
                        "platformFamily": "unix",
                        "platformOs": "macos",
                    },
                }));
            }
            ("thread/start", Some(id)) | ("thread/resume", Some(id)) => {
                asked_params = v.get("params").cloned().unwrap_or(Value::Null);
                asked_method = method.to_string();
                if mode == "thread-error" {
                    emit(json!({
                        "id": id,
                        "error": { "code": -32600, "message": "no rollout found for thread id" },
                    }));
                    continue;
                }
                if mode == "malformed" {
                    emit_raw("{not json at all");
                    emit_raw("plain prose on stdout");
                }
                // A resumed thread keeps the id it was resumed by; a fresh
                // one is named by the server, which is the whole reason
                // `provider_session_id()` is an Option for this provider.
                let thread_id = asked_params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        std::env::var("FAKE_CODEX_THREAD")
                            .unwrap_or_else(|_| "01a020c6-0000-7000-8000-fakethread01".into())
                    });
                let cwd = asked_params.get("cwd").cloned().unwrap_or(Value::Null);
                let mut result = json!({
                    "thread": thread_object(&thread_id, &cwd),
                    "model": asked_params.get("model").cloned().unwrap_or(json!("gpt-5.5")),
                    "modelProvider": "openai",
                    "cwd": cwd,
                });
                if mode == "unnamed-thread" {
                    result["thread"] = json!({ "sessionId": thread_id });
                }
                if let Some((policy, reviewer, sandbox)) = posture(&asked_params) {
                    result["approvalPolicy"] = policy;
                    result["approvalsReviewer"] = reviewer;
                    result["sandbox"] = json!({ "type": sandbox });
                }
                emit(json!({ "id": id, "result": result }));
                emit(json!({
                    "method": "thread/started",
                    "params": { "thread": thread_object(&thread_id, &cwd) },
                }));
                if mode == "hang-silent" {
                    // Nothing more, ever: the deterministic shape for the
                    // kill tests.
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
                if mode == "infra-request" {
                    emit(json!({
                        "id": INFRA_ID,
                        "method": "currentTime/read",
                        "params": {},
                    }));
                }
            }
            ("turn/start", Some(id)) => {
                let text = v
                    .pointer("/params/input/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                emit(json!({
                    "id": id,
                    "result": { "turn": { "id": "u", "items": [], "status": "inProgress" } },
                }));
                emit(json!({
                    "method": "turn/started",
                    "params": {
                        "threadId": "t",
                        "turn": { "id": "u", "items": [], "status": "inProgress" },
                    },
                }));
                emit(json!({
                    "method": "item/started",
                    "params": {
                        "threadId": "t", "turnId": "u", "startedAtMs": 0,
                        "item": {
                            "type": "userMessage", "id": "um1",
                            "content": [{ "type": "text", "text": text }],
                        },
                    },
                }));
                run_turn(&mode, &asked_params, &asked_method);
            }
            // A JSON-RPC response from eave to something this fake asked.
            ("", Some(id)) => {
                let n = id.as_i64().unwrap_or(-1);
                if n == APPROVAL_ID {
                    let decision = v
                        .pointer("/result/decision")
                        .and_then(Value::as_str)
                        .unwrap_or("<none>");
                    let accepted = decision == "accept";
                    emit(json!({
                        "method": "item/completed",
                        "params": {
                            "threadId": "t", "turnId": "u", "completedAtMs": 0,
                            "item": command_item(
                                if accepted { "completed" } else { "declined" },
                                if accepted { Some(0) } else { None },
                                if accepted { Some("hello") } else { None },
                            ),
                        },
                    }));
                    agent_message(&format!("DECISION:{decision}"));
                    turn_completed("completed");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    return;
                }
                if n == DIALOG_ID {
                    let code = v
                        .pointer("/error/code")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let message = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    agent_message(&format!("DIALOG:{code}:{message}"));
                    turn_completed("completed");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    return;
                }
                if n == INFRA_ID {
                    let code = v
                        .pointer("/error/code")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    agent_message(&format!("INFRA:{code}"));
                    turn_completed("completed");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    return;
                }
            }
            _ => {}
        }
    }
}

fn run_turn(mode: &str, asked: &Value, asked_method: &str) {
    match mode {
        "normal" | "malformed" => {
            agent_message("hello");
            turn_completed("completed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        // Hand back exactly what eave asked for at the handshake, so a
        // test can assert on the request rather than on the constants the
        // request was built from.
        "handshake-report" => {
            agent_message(&format!("HANDSHAKE:{asked_method}:{asked}"));
            turn_completed("completed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        "argv-report" => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            agent_message(&format!("ARGV:{}", args.join(" ")));
            turn_completed("completed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        "env-report" => {
            let leaked: Vec<String> = std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| k.starts_with("EAVE_"))
                .collect();
            let report = if leaked.is_empty() {
                "no EAVE_ vars".to_string()
            } else {
                leaked.join(",")
            };
            agent_message(&format!("ENV:{report}"));
            turn_completed("completed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        "tool-cycle" => {
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": command_item("inProgress", None, None),
                },
            }));
            // A reasoning item in the middle: a real item that is NOT a
            // tool call, so the in-flight count must not move for it.
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": { "type": "reasoning", "id": "r1", "summary": [], "content": [] },
                },
            }));
            std::thread::sleep(std::time::Duration::from_millis(150));
            emit(json!({
                "method": "item/completed",
                "params": {
                    "threadId": "t", "turnId": "u", "completedAtMs": 0,
                    "item": command_item("completed", Some(0), Some("hello")),
                },
            }));
            agent_message("ran it");
            turn_completed("completed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        "tool-fail" => {
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": command_item("inProgress", None, None),
                },
            }));
            emit(json!({
                "method": "item/completed",
                "params": {
                    "threadId": "t", "turnId": "u", "completedAtMs": 0,
                    "item": command_item("failed", Some(2), Some("nope")),
                },
            }));
            turn_completed("failed");
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::process::exit(0);
        }
        "approval-cycle" | "approval-hang" | "approval-withdraw" => {
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": command_item("inProgress", None, None),
                },
            }));
            emit(json!({
                "id": APPROVAL_ID,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "itemId": "c1",
                    "threadId": "t",
                    "turnId": "u",
                    "startedAtMs": 0,
                    "command": "echo hello",
                    "cwd": "/tmp",
                    "reason": "command is not on the trusted list",
                    "availableDecisions": ["accept", "acceptForSession", "decline", "cancel"],
                },
            }));
            if mode == "approval-withdraw" {
                std::thread::sleep(std::time::Duration::from_millis(100));
                emit(json!({
                    "method": "serverRequest/resolved",
                    "params": { "threadId": "t", "requestId": APPROVAL_ID },
                }));
                turn_completed("aborted");
                std::thread::sleep(std::time::Duration::from_millis(100));
                std::process::exit(0);
            }
            if mode == "approval-hang" {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
            // approval-cycle: the main loop answers when the decision
            // arrives.
        }
        "file-approval" => {
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": {
                        "type": "fileChange", "id": "f1", "status": "inProgress",
                        "changes": [
                            { "path": "/tmp/eave-fake/note.txt", "kind": "add", "diff": "+hi" },
                            { "path": "/tmp/eave-fake/other.txt", "kind": "update", "diff": "+x" },
                        ],
                    },
                },
            }));
            // The real params name no path at all — only the item does.
            emit(json!({
                "id": APPROVAL_ID,
                "method": "item/fileChange/requestApproval",
                "params": {
                    "itemId": "f1",
                    "threadId": "t",
                    "turnId": "u",
                    "startedAtMs": 0,
                    "grantRoot": "/tmp/eave-fake",
                },
            }));
        }
        "dialog-request" => {
            emit(json!({
                "id": DIALOG_ID,
                "method": "item/tool/requestUserInput",
                "params": {
                    "itemId": "d1", "threadId": "t", "turnId": "u",
                    "isBlocking": true,
                    "questions": [{ "id": "q1", "prompt": "which one?" }],
                },
            }));
        }
        "tool-hang" => {
            emit(json!({
                "method": "item/started",
                "params": {
                    "threadId": "t", "turnId": "u", "startedAtMs": 0,
                    "item": command_item("inProgress", None, None),
                },
            }));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        "infra-request" => {
            // The request already went out at handshake time; the main
            // loop reports the reply.
        }
        other => {
            eprintln!("fake-codex: unknown FAKE_CODEX_MODE {other}");
            std::process::exit(64);
        }
    }
}
