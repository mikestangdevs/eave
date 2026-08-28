//! ACP as a wire: newline-framed JSON-RPC 2.0 over a child's stdio.
//!
//! Nothing here knows which agent is on the other end. It knows how a line
//! is classified, what the five frames eave ever sends look like, and how
//! to put one on the pipe without letting two writers interleave mid-line.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;

use crate::events::SessionEvent;
use crate::session::SessionError;

/// The ACP version eave speaks. Both agents answer `initialize` with `1`
/// (grok 1.0.5 and Hermes 0.20.4, verified 2026-08-20).
pub const PROTOCOL_VERSION: i64 = 1;

/// JSON-RPC id of the `initialize` request. Fixed rather than allocated so
/// a reader's state machine can recognise the reply without a table.
pub const RPC_INITIALIZE: i64 = 1;
/// JSON-RPC id of the `session/new` (or `session/load`) request.
pub const RPC_SESSION: i64 = 2;

/// Longest wire-sourced string eave repeats back into the journal. A CLI
/// that prints a megabyte of garbage should not put a megabyte in the
/// record.
///
/// Used for malformed lines and, with the same reasoning and the same
/// number, for the agent label in [`crate::acp::handshake::Advertisement`]
/// — both are strings the AGENT chose, copied into eave's permanent
/// record.
pub const MALFORMED_LINE_CAP: usize = 512;

/// Shared because more than one writer exists on every ACP session: the
/// engine sending a prompt or answering an approval, and the reader task
/// driving the handshake. Serialized by the mutex so a JSON-RPC frame can
/// never interleave with another mid-line.
pub type SharedStdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// One inbound line, classified by JSON-RPC 2.0's own rules: a method with
/// an id is a request, a method without one is a notification, an id
/// without a method is a response, and nothing else is JSON-RPC at all.
pub enum Frame<'a> {
    /// Agent → client request. It wants an answer on the same id.
    Request { method: &'a str, id: &'a Value },
    /// Agent → client notification. No answer is expected or possible.
    Notification { method: &'a str },
    /// A reply to something eave asked.
    Response { id: &'a Value },
    /// Well-formed JSON that is not a JSON-RPC frame.
    Unclassifiable,
}

pub fn classify(v: &Value) -> Frame<'_> {
    match (v.get("method").and_then(Value::as_str), v.get("id")) {
        (Some(method), Some(id)) => Frame::Request { method, id },
        (Some(method), None) => Frame::Notification { method },
        (None, Some(id)) => Frame::Response { id },
        (None, None) => Frame::Unclassifiable,
    }
}

/// A line eave could not read, recorded rather than dropped. Parsing
/// always continues on the next line: a misbehaving CLI must not take the
/// session state machine down with it.
pub fn malformed(line: &str, error: String) -> SessionEvent {
    SessionEvent::Malformed {
        // `chars`, not bytes: truncating mid-codepoint would replace a
        // diagnosable line with a panic.
        line: line.chars().take(MALFORMED_LINE_CAP).collect(),
        error,
    }
}

/// The string form of a JSON-RPC id, used as eave's `request_id`.
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Turn a `request_id` back into the JSON-RPC id it came from, so a reply
/// names the same id in the same type. `3` must go back as the number `3`,
/// not the string `"3"` — a JSON-RPC peer is entitled to reject the latter.
pub fn id_from_key(key: &str) -> Value {
    match serde_json::from_str::<Value>(key) {
        Ok(v @ (Value::Number(_) | Value::String(_))) => v,
        _ => Value::String(key.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Outbound frames

/// The `initialize` call that opens ACP.
///
/// **eave declares no filesystem and no terminal capability, for both
/// agents, on purpose.** With `fs.writeTextFile: true` an ACP agent
/// delegates its writes to the client — verified against grok on
/// 2026-08-20, the write arrived as an `fs/write_text_file` request —
/// which would make eave the process performing the very writes it exists
/// to supervise, and would put a second, silent path to disk beside the
/// permission gate. Declaring nothing keeps the agent's own tools doing
/// the agent's own work, with `session/request_permission` as the single
/// review point. The permission request still fires either way (verified
/// with capabilities off).
pub fn initialize_frame() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": RPC_INITIALIZE,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
            },
        },
    })
}

/// Open a new session, or reload one the agent already knows.
///
/// Note what is NOT here: a caller-chosen session id. Both ACP agents mint
/// their own at `session/new` and ACP offers no way to propose one, so
/// [`crate::session::SpawnConfig::session_id`] is honoured only in its
/// resume role. `cwd` is passed explicitly rather than read from the
/// current process: the child was spawned with `current_dir`, but this
/// frame is built in eave's own process, where the cwd is the engine's.
pub fn session_frame(resume_id: Option<&str>, cwd: &str) -> Value {
    let (method, session) = match resume_id {
        Some(id) => ("session/load", json!(id)),
        None => ("session/new", Value::Null),
    };
    let mut params = json!({ "cwd": cwd, "mcpServers": [] });
    if !session.is_null() {
        params["sessionId"] = session;
    }
    json!({ "jsonrpc": "2.0", "id": RPC_SESSION, "method": method, "params": params })
}

pub fn prompt_frame(
    id: i64,
    session_id: &str,
    text: &str,
    images: &[crate::media::EncodedImage],
) -> Value {
    let mut prompt = vec![json!({ "type": "text", "text": text })];
    for img in images {
        prompt.push(crate::media::acp_image_block(img));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": prompt,
        },
    })
}

/// Answer one permission request. `Some(option)` selects an id the agent
/// offered; `None` is ACP's own `cancelled` outcome, which eave uses as
/// the deny of last resort when a request arrives without a reject option
/// — refusing is always expressible, even when the provider forgot to
/// offer a word for it.
pub fn permission_response_frame(rpc_id: &Value, option: Option<&str>) -> Value {
    let outcome = match option {
        Some(id) => json!({ "outcome": "selected", "optionId": id }),
        None => json!({ "outcome": "cancelled" }),
    };
    json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "outcome": outcome } })
}

/// Refuse a client method eave declined at `initialize`.
///
/// Answered immediately rather than ignored: silence would park the turn
/// on a request nobody can see. And answered with an ERROR rather than
/// serviced, because servicing it would make eave the thing performing the
/// write it is supposed to be policing.
pub fn method_not_found_frame(rpc_id: &Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "error": {
            "code": -32601,
            "message": format!(
                "eave does not implement the ACP client method {method:?} and declared no \
                 such capability at initialize; use your own tools, which are the ones the \
                 operator reviews."
            ),
        },
    })
}

// ---------------------------------------------------------------------------
// Writing

/// Write one JSON-RPC frame to the child's stdin under the shared lock.
pub async fn write_frame(stdin: &SharedStdin, frame: &Value) -> Result<(), SessionError> {
    write_line(stdin, &frame.to_string()).await
}

pub async fn write_line(stdin: &SharedStdin, line: &str) -> Result<(), SessionError> {
    let mut guard = stdin.lock().await;
    let pipe = guard.as_mut().ok_or(SessionError::StdinClosed)?;
    let mut payload = String::with_capacity(line.len() + 1);
    payload.push_str(line);
    payload.push('\n');
    pipe.write_all(payload.as_bytes()).await?;
    pipe.flush().await?;
    Ok(())
}

/// Best-effort write from a reader or handshake task, which has no one to
/// report to: a closed pipe means the child is already going away.
pub async fn try_write(stdin: &SharedStdin, frame: &Value) {
    let _ = write_frame(stdin, frame).await;
}
