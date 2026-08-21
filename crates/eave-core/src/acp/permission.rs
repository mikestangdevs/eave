//! `session/request_permission` — the reason eave speaks ACP at all.
//!
//! What is shared here is the *mechanics*: which option ids the agent
//! actually offered, where a concrete path hides in the request, and the
//! rule that a decision for a question that is no longer open is a race
//! rather than an error. What is deliberately NOT shared is the policy of
//! which option to pick; that is each adapter's, because each agent offers
//! a different vocabulary and eave never guesses one.
//!
//! **The divergence from comet is here.** comet auto-accepts every
//! permission request — `preferred_allow_option` in
//! `crates/harness/src/acp/normalize.rs:466` picks `allow_always` >
//! `allow_once` > first, so the human is never asked. eave surfaces the
//! question and waits, and no option is ever chosen without an operator's
//! click.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::acp::update::flatten_content;
use crate::acp::wire::{permission_response_frame, write_frame, SharedStdin};
use crate::events::cap_payload;
use crate::session::SessionError;

/// One permission question the agent is parked on, and the facts needed to
/// answer it.
///
/// The option ids are **the provider's own strings**, captured from the
/// request rather than hardcoded: agents vary them by tool kind
/// (`allow-edits-session` for one edit, and nothing guarantees a Bash call
/// spells its options the same way), and answering with an id that was
/// never offered is not an answer.
pub struct PendingPermission {
    /// The JSON-RPC id the answer must name.
    pub rpc_id: Value,
    pub asked_at: Instant,
    /// Every option id the agent put on the table.
    pub offered: Vec<Value>,
}

impl PendingPermission {
    /// The option id offered under `kind`, or `None`.
    pub fn option_of_kind(&self, kind: &str) -> Option<String> {
        self.offered
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|o| o.get("optionId").and_then(Value::as_str))
            .map(str::to_string)
    }

    /// Was this exact option id offered?
    pub fn offers_id(&self, option_id: &str) -> bool {
        self.offered
            .iter()
            .any(|o| o.get("optionId").and_then(Value::as_str) == Some(option_id))
    }
}

/// Every question currently open on one session.
#[derive(Default)]
pub struct PendingApprovals(HashMap<String, PendingPermission>);

impl PendingApprovals {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_open(&self, request_id: &str) -> bool {
        self.0.contains_key(request_id)
    }

    pub fn get(&self, request_id: &str) -> Option<&PendingPermission> {
        self.0.get(request_id)
    }

    pub fn open(&mut self, request_id: String, rpc_id: &Value, options: &Value) {
        self.0.insert(
            request_id,
            PendingPermission {
                rpc_id: rpc_id.clone(),
                asked_at: Instant::now(),
                offered: options.as_array().cloned().unwrap_or_default(),
            },
        );
    }

    /// Close a question. `false` means it was not open.
    pub fn close(&mut self, request_id: &str) -> bool {
        self.0.remove(request_id).is_some()
    }

    /// Questions the agent has been waiting on for longer than it waits.
    pub fn stale(&self, older_than: Duration) -> Vec<String> {
        let now = Instant::now();
        self.0
            .iter()
            .filter(|(_, p)| now.duration_since(p.asked_at) >= older_than)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Put one decision on the wire, then close the question.
///
/// The order is the contract: a failed write leaves the question OPEN so
/// the operator can try again rather than facing a dead card, and eave
/// never journals a decision that did not land.
pub async fn deliver(
    stdin: &SharedStdin,
    rpc_id: &Value,
    option_id: Option<&str>,
) -> Result<(), SessionError> {
    write_frame(stdin, &permission_response_frame(rpc_id, option_id)).await
}

/// The options array, journaled verbatim as what it is: the provider's own
/// suggestion for how to stop being asked (`allow_always`, `allow_session`,
/// `deny_always`). eave takes none of them — a session-wide grant would end
/// review for every later tool call — but it is a fact about the request,
/// so it is recorded.
pub fn suggestions(options: &Value) -> Option<String> {
    options
        .as_array()
        .map(|_| cap_payload(&options.to_string()))
}

/// The concrete path an approval would touch, when the request names one.
///
/// Checked in the order agents actually populate it: an edit approval puts
/// it on the diff block, a command approval never has one, `locations` is
/// ACP's generic fallback, and the raw arguments are the last resort.
pub fn approval_path(tool_call: &Value, vendor_meta_key: Option<&str>) -> Option<String> {
    // A path the agent itself already resolved beats one eave re-derives
    // from the raw arguments.
    if let Some(key) = vendor_meta_key {
        if let Some(p) = tool_call
            .get("_meta")
            .and_then(|m| m.get(key))
            .and_then(|t| t.get("input"))
            .and_then(|i| i.get("path"))
            .and_then(Value::as_str)
        {
            return Some(p.to_string());
        }
    }
    if let Some(Value::Array(blocks)) = tool_call.get("content") {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("diff") {
                if let Some(p) = block.get("path").and_then(Value::as_str) {
                    return Some(p.to_string());
                }
            }
        }
    }
    if let Some(p) = tool_call
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .and_then(|l| l.get("path"))
        .and_then(Value::as_str)
    {
        return Some(p.to_string());
    }
    let raw = tool_call.get("rawInput")?;
    raw.get("file_path")
        .or_else(|| raw.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// What eave records as the thing being approved.
///
/// **Verbatim, and the whole of it**: approving without knowing what you
/// approved is the failure this exists to prevent. `rawInput` when the
/// agent sends one, the whole tool call when it does not — an edit
/// approval's diff lives in `content`, and dropping it would leave the
/// operator approving a path with no idea what goes in it.
pub fn approval_input(tool_call: &Value) -> String {
    match tool_call.get("rawInput") {
        Some(raw) if !raw.is_null() => cap_payload(&raw.to_string()),
        _ => cap_payload(&tool_call.to_string()),
    }
}

/// A human sentence for the request: the agent's own description when it
/// sends one, the flattened content when it does not. Never invented.
pub fn approval_description(tool_call: &Value) -> String {
    tool_call
        .get("rawInput")
        .and_then(|r| r.get("description"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| flatten_content(tool_call.get("content")))
}
