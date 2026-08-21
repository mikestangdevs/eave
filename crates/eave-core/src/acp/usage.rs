//! Token accounting off the ACP wire (slice CC, shared by slice CA).
//!
//! Two facts, deliberately kept apart. A **bill** is what one turn cost;
//! a **level** is how full the context window now is. They are different
//! numbers for the same turn — eave's own capture has a turn billing
//! 53,820 input tokens and leaving 26,967 in context, because a turn with
//! a tool sends the prompt more than once — and a single flat "tokens"
//! figure would have to pick one and misreport the other.
//!
//! comet reached the same split from the other side and stopped short of
//! it: `comet/docs/research/acp.md` records `usage_update` as "a context
//! gauge, not per-turn tokens — deliberately unmapped", because comet's
//! single `Usage{input,output}` event has nowhere safe to put a level and
//! feeding one in would corrupt its rate-limit probes. eave's event has a
//! separate slot that cannot be mistaken for a bill, which removes the
//! reason for the abstention rather than ignoring it.

use serde_json::Value;

use crate::events::{ContextWindow, SessionEvent, TurnTokens};

/// Per-turn tokens off a settled `session/prompt` response.
///
/// ACP carries a first-class `usage` on `PromptResponse` — verified in the
/// generated schema that ships with the `acp` package (`class
/// PromptResponse`, fields `_meta` / `stopReason` / `usage`) and in grok
/// 1.0.5's own serde table, which names `struct PromptResponse with 4
/// elements` over `userMessageId`, `stopReason`, `usage`, `_meta` (read
/// offline out of the shipped binary). comet found it on `_meta` instead
/// at grok 1.0.4 (`comet/crates/harness/src/acp/mod.rs:1665`), so both
/// spellings are read, newest first. Absent → nothing, never a zero.
///
/// **`inputTokens` is the whole prompt, cache included.** Grok's bundled
/// documentation states it outright: *"ACP `_meta.usage.inputTokens`
/// (PromptUsage) is still the **full** prompt sum; only the headless
/// projector subtracts cache."* So this needs none of the arithmetic
/// `claude.rs` does.
///
/// **What eave does NOT claim here.** The ACP schema describes these
/// fields as session totals ("Total input tokens across all turns", "Sum
/// of all token types across session") while both agents eave drives put
/// the settling turn's own numbers in them — Hermes builds it from the
/// last model response's usage (`acp_adapter/server.py`, from
/// `_last_turn_usage`), and grok's docs say "`usage` sums tokens for the
/// prompt". eave records the statement and its arrival point (one settled
/// turn) and takes no position on an aggregation window the wire
/// contradicts itself about.
pub fn from_prompt_response(result: Option<&Value>) -> Option<SessionEvent> {
    let result = result?;
    let usage = result
        .get("usage")
        .or_else(|| result.pointer("/_meta/usage"))
        .or_else(|| result.get("_meta"))?;
    let count = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k))
            .and_then(Value::as_u64)
    };
    let turn = TurnTokens::stated(
        count(&["inputTokens", "input_tokens"]),
        count(&["outputTokens", "output_tokens"]),
        count(&["cachedReadTokens", "cached_read_tokens"]),
        // Grok spells the write bucket `cacheCreationTokens` on the
        // frames verified live at 1.0.5 and `cachedWriteTokens` on the
        // ACP-generic path. Both are read so a STATED zero — which is a
        // fact about the turn — is not lost to a spelling.
        count(&[
            "cachedWriteTokens",
            "cached_write_tokens",
            "cacheCreationTokens",
        ]),
    )?;
    Some(SessionEvent::TokenUsage {
        turn: Some(turn),
        context: None,
    })
}

/// ACP's context gauge from a `usage_update` session update.
///
/// The schema names both facts precisely — `used` is "Tokens currently in
/// context", `size` is "Total context window size in tokens" — which is
/// why this is the one place a context level may come from. An update
/// with no `used` is not a gauge and is journaled verbatim by the caller
/// instead.
///
/// A stated `size` of zero is dropped rather than relayed: the agents
/// themselves treat it as "unknown" (Hermes returns no update at all when
/// its context length is `<= 0`, `acp_adapter/server.py`), and a ceiling
/// of zero would render as a window that is both empty and full.
pub fn from_usage_update(update: &Value) -> Option<SessionEvent> {
    let used_tokens = update.get("used").and_then(Value::as_u64)?;
    Some(SessionEvent::TokenUsage {
        turn: None,
        context: Some(ContextWindow {
            used_tokens,
            limit_tokens: update
                .get("size")
                .and_then(Value::as_u64)
                .filter(|size| *size > 0),
        }),
    })
}
