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

fn session_id() -> String {
    std::env::var("FAKE_SESSION_ID").unwrap_or_else(|_| "fake-session-1".into())
}

fn emit_init() {
    emit(json!({ "type": "system", "subtype": "init", "session_id": session_id() }));
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

fn emit_result() {
    emit(json!({ "type": "result", "subtype": "success", "is_error": false }));
}

fn hang_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
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
        other => {
            eprintln!("fake-agent: unknown FAKE_MODE {other}");
            std::process::exit(64);
        }
    }
}
