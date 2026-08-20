//! fake-agent: a stand-in for an agent CLI that can misbehave on demand.
//!
//! Required by the build directive ("write a fake agent CLI harness early")
//! — it can hang (silent or busy), crash, emit malformed JSON, ignore
//! SIGTERM, spawn group members, and echo tool cycles, all speaking Claude
//! Code's stream-JSON so the real parser is exercised.
//!
//! Behavior is chosen via env (`FAKE_MODE`) rather than argv so the real
//! `ClaudeCodeSession` spawn path — argv and all — is what tests run.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

static TERM_SEEN: AtomicBool = AtomicBool::new(false);

extern "C" fn note_term(_sig: libc::c_int) {
    TERM_SEEN.store(true, Ordering::SeqCst);
}

fn emit(v: serde_json::Value) {
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

/// The session id the real CLI would use: `--resume <id>` or
/// `--session-id <id>` from argv (the caller-chosen id is honored verbatim,
/// as the real CLI does — verified 2026-08-15, claude-stream-notes.md), with
/// `FAKE_SESSION_ID` env as an override and a fixed fallback.
fn argv_session() -> (Option<String>, bool) {
    let args: Vec<String> = std::env::args().collect();
    for (i, a) in args.iter().enumerate() {
        if a == "--resume" {
            return (args.get(i + 1).cloned(), true);
        }
        if a == "--session-id" {
            return (args.get(i + 1).cloned(), false);
        }
    }
    (None, false)
}

fn session_id() -> String {
    if let Ok(id) = std::env::var("FAKE_SESSION_ID") {
        return id;
    }
    argv_session().0.unwrap_or_else(|| "fake-session-1".into())
}

/// The posture the CLI announces (Slice K). The real CLI states this in
/// its `init` frame and eave refuses any session that cannot prove it
/// asks, so the fake states it too — `FAKE_PERMISSION_MODE` is how the
/// tests drive the refusal paths (`acceptEdits` = a mode that never asks,
/// `none` = a CLI that says nothing at all).
fn emit_init() {
    let mode = std::env::var("FAKE_PERMISSION_MODE").unwrap_or_else(|_| "default".into());
    if mode == "none" {
        emit(json!({ "type": "system", "subtype": "init", "session_id": session_id() }));
    } else {
        emit(json!({
            "type": "system",
            "subtype": "init",
            "session_id": session_id(),
            "permissionMode": mode,
        }));
    }
}

fn emit_text(text: &str) {
    emit(json!({
        "type": "stream_event",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text },
        },
    }));
    emit(json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": text }] },
        "session_id": session_id(),
    }));
}

/// A bare streaming delta with no per-tick snapshot — how the real CLI
/// actually streams (the captured fixture holds 136 deltas against 3
/// snapshots; snapshots come at content-block end, not per delta).
fn emit_delta(text: &str) {
    emit(json!({
        "type": "stream_event",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text },
        },
    }));
}

fn emit_result() {
    emit(json!({ "type": "result", "subtype": "success", "is_error": false }));
}

fn hang_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

// ---- Slice AV: the ACP (Grok) protocol ------------------------------
//
// `GrokSession` speaks JSON-RPC over the same pipes, so these modes answer
// an `initialize`/`session/new`/`session/prompt` handshake instead of
// printing Claude's stream-JSON. Everything else about the harness — the
// process group, the SIGTERM behaviours, the hang modes — is protocol
// agnostic and is reused by the Grok tests unchanged.

/// The session id the fake ACP agent issues. Grok mints its own at
/// `session/new` (it accepts no caller-chosen id), so this is deliberately
/// NOT derived from argv the way `session_id()` is.
fn acp_session_id() -> String {
    std::env::var("FAKE_ACP_SESSION")
        .unwrap_or_else(|_| "01a0fake-0000-7000-8000-000000000001".into())
}

fn acp_result(id: &serde_json::Value, result: serde_json::Value) {
    emit(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

/// A session update on the standard ACP channel.
fn acp_update(update: serde_json::Value) {
    emit(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": acp_session_id(), "update": update },
    }));
}

/// A session update on Grok's vendor channel. `turn_completed` really does
/// arrive only here, so the tests must see it here too.
fn acp_vendor_update(update: serde_json::Value) {
    emit(json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/session_notification",
        "params": { "sessionId": acp_session_id(), "update": update },
    }));
}

/// The posture record. `yolo` is the fact eave gates the whole session on.
fn acp_posture(yolo: bool) {
    emit(json!({
        "jsonrpc": "2.0",
        "method": "_x.ai/sessions/changed",
        "params": {
            "upserted": [{
                "sessionId": acp_session_id(),
                "cwd": std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "modelId": "grok-4.6",
                "yolo": yolo,
                "activity": "working",
                "resident": true,
            }],
            "removed": [],
        },
    }));
}

fn acp_read() -> Option<serde_json::Value> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
    }
}

/// Drive one ACP conversation. `mode` selects what happens on the first
/// `session/prompt`.
fn run_acp(mode: &str) {
    let mut turns = 0;
    loop {
        let Some(msg) = acp_read() else { return };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match msg.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => acp_result(
                &id,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": true },
                }),
            ),
            Some("session/new") => {
                acp_result(&id, json!({ "sessionId": acp_session_id() }));
            }
            Some("session/load") => {
                // Resume: the id came from the caller, so the result names
                // metadata only — exactly as the real agent does.
                acp_result(&id, json!({ "models": { "currentModelId": "grok-4.6" } }));
            }
            Some("session/prompt") => {
                turns += 1;
                let text = msg["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                acp_turn(mode, &id, &text, turns);
            }
            _ => {}
        }
    }
}

fn acp_turn(mode: &str, id: &serde_json::Value, text: &str, turns: u32) {
    match mode {
        "acp-asks" => {
            acp_posture(false);
            acp_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": format!("heard: {text}") },
            }));
            acp_vendor_update(json!({
                "sessionUpdate": "turn_completed",
                "stop_reason": "end_turn",
            }));
            acp_result(id, json!({ "stopReason": "end_turn" }));
        }
        // A session running with auto-approve: it states the posture and
        // eave must refuse it before anything happens.
        "acp-yolo" => {
            acp_posture(true);
            acp_update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "working unsupervised" },
            }));
            acp_vendor_update(json!({
                "sessionUpdate": "turn_completed",
                "stop_reason": "end_turn",
            }));
            acp_result(id, json!({ "stopReason": "end_turn" }));
        }
        // A session that never states a posture and starts a tool anyway:
        // the fail-closed backstop.
        "acp-no-posture" => {
            acp_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-fake-1",
                "title": "write",
                "rawInput": { "file_path": "/tmp/eave-fake/x.txt" },
                "_meta": { "x.ai/tool": { "name": "write", "label": "Write" } },
            }));
            acp_vendor_update(json!({
                "sessionUpdate": "turn_completed",
                "stop_reason": "end_turn",
            }));
            acp_result(id, json!({ "stopReason": "end_turn" }));
        }
        // Ask once, then report exactly what the operator decided — the
        // round trip that proves eave's answer reached the agent.
        "acp-approval" | "acp-approval-hang" => {
            // A denial ends the turn, so the operator's follow-up message
            // arrives as a SECOND prompt. Echo it rather than re-asking.
            if turns > 1 {
                acp_update(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": format!("FOLLOWUP:{text}") },
                }));
                acp_vendor_update(json!({
                    "sessionUpdate": "turn_completed",
                    "stop_reason": "end_turn",
                }));
                acp_result(id, json!({ "stopReason": "end_turn" }));
                return;
            }
            acp_posture(false);
            let path = std::env::var("FAKE_APPROVAL_PATH")
                .unwrap_or_else(|_| "/tmp/eave-fake/note.txt".into());
            acp_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-fake-1",
                "title": format!("Write `{path}`"),
                "rawInput": { "file_path": path, "content": "hi" },
                "_meta": { "x.ai/tool": { "name": "write", "label": "Write" } },
            }));
            emit(json!({
                "jsonrpc": "2.0",
                "id": 900,
                "method": "session/request_permission",
                "params": {
                    "sessionId": acp_session_id(),
                    "toolCall": {
                        "toolCallId": "call-fake-1",
                        "kind": "edit",
                        "title": format!("Write `{path}`"),
                        "rawInput": { "file_path": path, "content": "hi" },
                        "_meta": { "x.ai/tool": {
                            "name": "write", "label": "Write", "read_only": false,
                            "input": { "path": path },
                        }},
                    },
                    "options": [
                        { "optionId": "allow-edits-session",
                          "name": "Yes, allow all edits during this session",
                          "kind": "allow_always" },
                        { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                        { "optionId": "reject-once",
                          "name": "No, and tell Grok what to do differently",
                          "kind": "reject_once" },
                    ],
                },
            }));
            if mode == "acp-approval-hang" {
                // Nobody will ever be told the answer: the deterministic
                // shape for the clock-hold and withdrawal tests.
                hang_forever();
            }
            // Wait for the decision, ignoring anything else on stdin.
            loop {
                let Some(reply) = acp_read() else { return };
                if reply.get("id").and_then(|i| i.as_i64()) != Some(900) {
                    continue;
                }
                let outcome = &reply["result"]["outcome"];
                let verdict = match outcome["outcome"].as_str() {
                    Some("selected") => outcome["optionId"].as_str().unwrap_or("?").to_string(),
                    Some(other) => other.to_string(),
                    None => "malformed".to_string(),
                };
                let allowed = verdict == "allow-once";
                acp_update(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-fake-1",
                    "status": if allowed { "completed" } else { "failed" },
                    "content": [{ "type": "content", "content": {
                        "type": "text",
                        "text": format!("DECISION:{verdict}"),
                    }}],
                }));
                acp_update(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": format!("DECISION:{verdict}") },
                }));
                acp_vendor_update(json!({
                    "sessionUpdate": "turn_completed",
                    "stop_reason": if allowed { "end_turn" } else { "cancelled" },
                }));
                acp_result(
                    id,
                    json!({
                        "stopReason": if allowed { "end_turn" } else { "cancelled" },
                    }),
                );
                return;
            }
        }
        other => {
            eprintln!("fake-agent: unknown ACP mode {other}");
            std::process::exit(64);
        }
    }
}

fn main() {
    let mode = std::env::var("FAKE_MODE").unwrap_or_else(|_| "normal".into());

    match mode.as_str() {
        "normal" => {
            emit_init();
            emit_text("hello from fake agent");
            emit_result();
        }
        "hang-silent" => {
            emit_init();
            emit_text("about to go silent");
            hang_forever();
        }
        "hang-busy" => {
            emit_init();
            let mut x: u64 = 0;
            loop {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
        }
        "hang-ignore-term" => {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let _child = if std::env::var("FAKE_SPAWN_CHILD").is_ok() {
                // A group member that also shrugs off SIGTERM, to prove the
                // kill path escalates and verifies the whole group.
                Some(
                    std::process::Command::new("sh")
                        .args(["-c", "trap '' TERM; sleep 600"])
                        .spawn()
                        .expect("spawn stubborn grandchild"),
                )
            } else {
                None
            };
            emit_init();
            hang_forever();
        }
        "chatty-term" => {
            let handler: extern "C" fn(libc::c_int) = note_term;
            unsafe {
                libc::signal(libc::SIGTERM, handler as usize);
            }
            emit_init();
            emit_text("alive before term");
            loop {
                if TERM_SEEN.load(Ordering::SeqCst) {
                    // Stale protocol lines racing the kill — the session
                    // must drop these, not surface them.
                    emit_text("POST_TERM_MARKER one");
                    emit_text("POST_TERM_MARKER two");
                    emit_result();
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "crash" => {
            emit_init();
            emit_text("about to die");
            eprintln!("synthetic failure: disk on fire");
            std::process::exit(3);
        }
        "malformed" => {
            emit_init();
            emit_raw("this is not json at all");
            emit_text("still alive after garbage");
            emit_raw("{\"type\":");
            emit(json!({ "no_type_tag": true }));
            emit_result();
        }
        "tool-cycle" => {
            emit_init();
            emit(json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_1", "name": "Bash" },
                },
            }));
            emit(json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }
                ]},
                "session_id": session_id(),
            }));
            std::thread::sleep(Duration::from_millis(400));
            emit(json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
                ]},
            }));
            emit_text("tool cycle done");
            emit_result();
            std::thread::sleep(Duration::from_millis(100));
        }
        "term-143" => {
            // Mimic the real CLI's SIGTERM behavior: catch it and exit with
            // code 143, not a raw signal status (claude-stream-notes.md
            // §Kill/exit facts).
            let handler: extern "C" fn(libc::c_int) = note_term;
            unsafe {
                libc::signal(libc::SIGTERM, handler as usize);
            }
            emit_init();
            emit_text("alive and catching TERM");
            loop {
                if TERM_SEEN.load(Ordering::SeqCst) {
                    std::process::exit(143);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "tool-hang" => {
            // Open a turn, start a tool, never resolve it — the deterministic
            // mid-turn shape for kill -9 acceptance tests (a tool is
            // outstanding for as long as the test needs).
            emit_init();
            emit(json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_hang", "name": "Bash" },
                },
            }));
            emit(json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "toolu_hang", "name": "Bash", "input": {} }
                ]},
                "session_id": session_id(),
            }));
            hang_forever();
        }
        "slow-stream" => {
            // A long-running turn at human cadence: one small text event
            // every 50 ms, never a result. The deterministic substrate for
            // warm-reconnect acceptance ("kill the UI mid-turn; relaunch;
            // the turn is still streaming") without flood's volume.
            emit_init();
            let mut i: u64 = 0;
            loop {
                emit_delta(&format!("tick {i} "));
                i += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        "flood" => {
            // Emit text events continuously so the recorder is writing the
            // journal near-constantly — the substrate for the
            // kill-during-a-journal-write acceptance test (build directive).
            emit_init();
            let blob = "x".repeat(512);
            let mut i: u64 = 0;
            loop {
                emit_text(&format!("flood {i} {blob}"));
                i += 1;
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        "linger" => {
            // The real CLI's between-turns shape (claude-stream-notes.md):
            // answer each stdin line with a complete turn, then sit idle on
            // stdin at zero CPU. The attention regression that matters most:
            // this idleness is NORMAL — no clock may run after
            // turn_completed (build directive 2026-08-16).
            emit_init();
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => {
                        emit_text("turn done, lingering on stdin");
                        emit_result();
                    }
                }
            }
        }
        "requesting-hang" => {
            // Mark an API request in flight, then hang: the quiet notice
            // must name "awaiting API", and zero CPU must NOT harden it to
            // unresponsive (a request in flight burns no CPU by design).
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            emit(json!({ "type": "system", "subtype": "status", "status": "requesting" }));
            hang_forever();
        }
        "stdout-close" => {
            // Close stdout mid-turn while staying alive: provable stream
            // death → `failed` with the concrete cause, immediately, no
            // timer (spec 01) — and never auto-killed.
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            emit_text("closing my stdout now");
            unsafe {
                libc::close(1);
            }
            hang_forever();
        }
        "stdin-closed" => {
            // Close our stdin so the parent's next pipe write fails with
            // EPIPE while we are demonstrably alive — the deterministic
            // shape for the journaled-but-undelivered send test (spec 02
            // §Delivery state).
            unsafe {
                libc::close(0);
            }
            emit_init();
            hang_forever();
        }
        "resume-probe" => {
            // Report whether this process was continued via --resume and
            // with which id — proves the cold-resume wiring end to end.
            // Like the real CLI, wait for the user message before answering
            // (also makes engine-side delivery deterministic: we are alive
            // and blocked on stdin when the pipe write lands).
            let (_, resumed) = argv_session();
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            emit_text(&format!("RESUME_FLAG:{resumed} ID:{}", session_id()));
            emit_result();
        }
        // One turn stating the process's own cwd, then exit (Slice I2:
        // proves the engine spawns the agent IN the worktree — including
        // on `--resume` respawns). Consumes the user send first so the
        // delivery write never races our exit.
        "cwd-report" => {
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            emit_text(
                &std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "?".into()),
            );
            emit_result();
            std::thread::sleep(Duration::from_millis(100));
        }
        // ---- Slice K: the permission channel -----------------------
        // Ask once, then do exactly what the operator said. The request
        // shape mirrors the real capture (docs/research/
        // approvals-capture-2026-08-18/capC-ppt.jsonl) including the
        // envelope-level request_id and the nested `request` object.
        "approval-cycle" | "approval-hang" => {
            let hang = mode == "approval-hang";
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let tool = std::env::var("FAKE_APPROVAL_TOOL").unwrap_or_else(|_| "Write".into());
            let path = std::env::var("FAKE_APPROVAL_PATH")
                .unwrap_or_else(|_| "/tmp/eave-fake/note.txt".into());
            let rid = std::env::var("FAKE_APPROVAL_ID").unwrap_or_else(|_| "req-1".into());
            emit(json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_k1", "name": tool },
                },
            }));
            emit(json!({
                "type": "control_request",
                "request_id": rid,
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": tool,
                    "display_name": tool,
                    "input": { "file_path": path, "content": "hi" },
                    "description": "note.txt",
                    "permission_suggestions": [
                        { "type": "setMode", "mode": "acceptEdits", "destination": "session" }
                    ],
                    "blocked_path": path,
                    "tool_use_id": "toolu_k1",
                },
            }));
            if hang {
                // Nobody will ever be told the answer: the deterministic
                // shape for the clock-hold and withdrawal tests.
                hang_forever();
            }
            // Wait for the decision, ignoring anything else on stdin.
            loop {
                let mut l = String::new();
                match std::io::stdin().read_line(&mut l) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => {}
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else {
                    continue;
                };
                if v.get("type").and_then(|t| t.as_str()) != Some("control_response") {
                    continue;
                }
                let resp = &v["response"]["response"];
                let allowed = resp.get("behavior").and_then(|b| b.as_str()) == Some("allow");
                let message = resp
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                emit(json!({
                    "type": "user",
                    "message": { "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_k1",
                        "content": if allowed {
                            format!("DECISION:allow wrote {path}")
                        } else {
                            format!("DECISION:deny {message}")
                        },
                        "is_error": !allowed,
                    }]},
                }));
                emit_text(if allowed {
                    "did the thing"
                } else {
                    "stood down"
                });
                emit_result();
                std::thread::sleep(Duration::from_millis(100));
                return;
            }
        }
        // A dialog request eave cannot present. The adapter must answer it
        // with a stated refusal on its own (amendment 2); this mode proves
        // the answer arrives by reporting what it received.
        "dialog-request" => {
            emit_init();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            emit(json!({
                "type": "control_request",
                "request_id": "dlg-1",
                "request": { "subtype": "request_user_dialog", "kind": "ask_user_question" },
            }));
            loop {
                let mut l = String::new();
                match std::io::stdin().read_line(&mut l) {
                    Ok(0) | Err(_) => std::process::exit(0),
                    Ok(_) => {}
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else {
                    continue;
                };
                if v.get("type").and_then(|t| t.as_str()) != Some("control_response") {
                    continue;
                }
                let sub = v["response"]["subtype"].as_str().unwrap_or("");
                let err = v["response"]["error"].as_str().unwrap_or("");
                emit_text(&format!("DIALOG_ANSWER:{sub}:{err}"));
                emit_result();
                std::thread::sleep(Duration::from_millis(100));
                return;
            }
        }
        // The undocumented flag is gone: die exactly as the real CLI does
        // for an unknown option, before announcing anything.
        "reject-permission-flag" => {
            eprintln!("error: unknown option '--permission-prompt-tool'");
            std::process::exit(1);
        }
        // Slice P2: echo the argv eave actually spawned us with, so a
        // process test can prove that a session's configured model and
        // flags reached the child — and in what order — rather than
        // asserting on the SpawnConfig struct the engine built, which
        // would test the engine against itself.
        "argv-report" => {
            emit_init();
            let args: Vec<String> = std::env::args().skip(1).collect();
            emit_text(&args.join(" "));
            emit_result();
        }
        "env-report" => {
            emit_init();
            let leaked: Vec<String> = std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| k.starts_with("EAVE_"))
                .collect();
            let report = if leaked.is_empty() {
                "no EAVE_ vars".to_string()
            } else {
                leaked.join(",")
            };
            emit_text(&report);
            emit_result();
        }
        // Slice AV: the Grok adapter's protocol. These answer JSON-RPC on
        // stdin rather than printing stream-JSON at startup.
        acp if acp.starts_with("acp-") => run_acp(acp),
        other => {
            eprintln!("fake-agent: unknown FAKE_MODE {other}");
            std::process::exit(64);
        }
    }
}
