//! Loader for captured wire-tap fixtures (`fixtures/*.tap.jsonl`).
//!
//! Format: one line per received wire frame, `<recv_ts_ms>\t<raw frame
//! JSON>`. Frames are the loose response envelopes of spec 05; only
//! subscription items matter here. The Stage D build directive requires
//! tuning against real pacing and real markdown — this is that data.

use std::path::Path;

use eave_wire::{ResponseBody, ResponseEnvelope, SubscriptionItem};

/// One replayable step: a subscription item plus its capture timestamp.
pub struct Step {
    pub ts_ms: u64,
    pub item: SubscriptionItem,
}

pub fn load(path: &Path) -> std::io::Result<Vec<Step>> {
    let raw = std::fs::read_to_string(path)?;
    let mut steps = Vec::new();
    for line in raw.lines() {
        let Some((ts, json)) = line.split_once('\t') else {
            continue;
        };
        let Ok(ts_ms) = ts.parse::<u64>() else {
            continue;
        };
        let Ok(env) = serde_json::from_str::<ResponseEnvelope>(json) else {
            continue;
        };
        if let ResponseBody::Item(item) = env.body {
            steps.push(Step { ts_ms, item });
        }
    }
    Ok(steps)
}

/// Repo-relative default fixture path (tests run from the workspace).
pub fn default_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/real-claude-2026-08-16.tap.jsonl")
}
