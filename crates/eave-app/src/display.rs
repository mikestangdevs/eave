//! The display tree: transcript → virtualized row list.
//!
//! Spec 03 §Rendering: "Row per markdown top-level block; stable ids
//! `{message}#{part}.{block}`; row version = content fingerprint (FNV) ×
//! streaming bit; prefix/suffix diff producing one minimal splice; per-row
//! render cache keyed by fingerprint" and "`remeasure_items` on settle —
//! live→complete flips remeasure, never splice."
//!
//! This module is pure (no gpui): it owns block caching, row identity,
//! fingerprints, and the diff. The UI layer maps [`DiffOps`] onto
//! `ListState::splice` / `remeasure_items`. Purity is what lets the
//! fence-close/no-splice acceptance run headless against the real fixture.

use std::sync::Arc;

use eave_wire::model::{ApprovalStatus, Delivery, Row, ToolStatus, Transcript};

use crate::markdown::{split_blocks, Block};

/// Stable row identity: `{message}.{block}` (v0 messages have one part, so
/// the spec's `{part}` collapses; the id space keeps room for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowId {
    pub msg: u32,
    pub block: u32,
}

/// Reserved sentinel message index (kept out of the id space). The
/// terminal-outcome line itself is the shared projection's `Row::Marker`
/// ("session closed: …") — the display list adds no row of its own for it
/// (a duplicate closed line shipped in Stage D and was caught by Stage E's
/// kill-affordance verification).
pub const CLOSED_MSG: u32 = u32::MAX;

/// One virtualized row: identity + version. Content is fetched through
/// [`DisplayList::block`] / the transcript at render time — rows stay
/// `Copy` so the O(rows) rebuild per burst never clones strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRow {
    pub id: RowId,
    /// FNV-1a of the content × streaming bit × status. Equal fingerprint ⇒
    /// the cached measure and render for this row are still valid.
    pub fingerprint: u64,
}

/// What the UI must do to its `ListState` after an update.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffOps {
    /// Replace `old_range` with `new_count` rows (one minimal splice).
    pub splice: Option<(std::ops::Range<usize>, usize)>,
    /// Rows whose content changed in place (same id): remeasure, never
    /// splice — preserves size hints and scroll anchors (spec 03).
    pub remeasure: Option<std::ops::Range<usize>>,
    /// The whole list was rebuilt (reset/epoch change): `ListState::reset`.
    pub reset: bool,
}

impl DiffOps {
    pub fn is_noop(&self) -> bool {
        self.splice.is_none() && self.remeasure.is_none() && !self.reset
    }
}

/// FNV-1a, the spec-named fingerprint hash.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn fnv1a_more(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Salt mixed into the tail block's fingerprint while its message is live
/// — the spec's "streaming bit" (spec 03 §Rendering).
const STREAMING_SALT: u64 = 0x9e37_79b9_7f4a_7c15;

/// Per-message cache entry: blocks are re-split and re-hashed only when
/// the message's revision changed (during streaming that is one message
/// per burst). The `Arc` is the per-row render cache the spec names —
/// render fetches blocks by reference, no re-parse for untouched rows.
struct MsgEntry {
    /// `Transcript::revs` value this entry was built from; `u64::MAX`
    /// means "unknown, always rebuild" (transcripts mutated without
    /// revision tracking).
    rev: u64,
    base_fp: u64,
    blocks: Arc<Vec<Block>>,
    /// Fingerprint per block, without the streaming bit (mixed in at row
    /// build, so the tail↔settled flip needs no rehash).
    block_fps: Vec<u64>,
}

#[derive(Default)]
pub struct DisplayList {
    msgs: Vec<MsgEntry>,
    rows: Vec<DisplayRow>,
}

fn msg_content_hash(row: &Row) -> u64 {
    match row {
        Row::User { text, delivery } => {
            let tag: u8 = match delivery {
                Delivery::Pending => 1,
                Delivery::Delivered => 2,
                Delivery::Undelivered => 3,
            };
            fnv1a_more(fnv1a(&[0, tag]), text.as_bytes())
        }
        Row::Assistant { text, live } => {
            fnv1a_more(fnv1a(&[1, *live as u8]), text.as_bytes())
        }
        Row::Tool { id, name, status } => {
            let tag: u8 = match status {
                ToolStatus::Running => 1,
                ToolStatus::Finished => 2,
                ToolStatus::Interrupted => 3,
            };
            fnv1a_more(fnv1a_more(fnv1a(&[2, tag]), id.as_bytes()), name.as_bytes())
        }
        Row::Marker { text } => fnv1a_more(fnv1a(&[3]), text.as_bytes()),
        // Slice K: the status is part of the content — an answered card
        // must re-render, and a still-pending one must not.
        Row::Approval {
            request_id,
            tool,
            description,
            path,
            input_json,
            status,
        } => {
            let tag: u8 = match status {
                ApprovalStatus::Pending => 1,
                ApprovalStatus::Approved => 2,
                ApprovalStatus::Denied { .. } => 3,
                ApprovalStatus::Withdrawn { .. } => 4,
            };
            let mut h = fnv1a(&[4, tag]);
            for part in [
                request_id.as_str(),
                tool.as_str(),
                description.as_str(),
                path.as_deref().unwrap_or(""),
                input_json.as_str(),
            ] {
                h = fnv1a_more(h, part.as_bytes());
            }
            if let ApprovalStatus::Denied { message } = status {
                h = fnv1a_more(h, message.as_bytes());
            }
            h
        }
    }
}

impl DisplayList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, ix: usize) -> Option<&DisplayRow> {
        self.rows.get(ix)
    }

    /// The markdown block behind an assistant row.
    pub fn block(&self, id: RowId) -> Option<&Block> {
        self.msgs
            .get(id.msg as usize)?
            .blocks
            .get(id.block as usize)
    }

    /// Display index of the first row of message `msg` (for scroll pinning).
    pub fn first_row_of_msg(&self, msg: u32) -> Option<usize> {
        self.rows.iter().position(|r| r.id.msg == msg)
    }

    /// Fold the transcript into rows and report the minimal list ops.
    ///
    /// Non-assistant transcript rows are one display row each; assistant
    /// rows explode into one display row per top-level markdown block.
    pub fn update(&mut self, transcript: &Transcript) -> DiffOps {
        // 1. Refresh the per-message block cache, re-splitting and
        //    re-hashing only messages whose revision changed — O(changed)
        //    per burst, not O(transcript).
        for (ix, row) in transcript.rows.iter().enumerate() {
            let rev = transcript.revs.get(ix).copied().unwrap_or(u64::MAX);
            let need = match self.msgs.get(ix) {
                Some(e) => e.rev != rev || rev == u64::MAX,
                None => true,
            };
            if need {
                let blocks = match row {
                    Row::Assistant { text, .. } => Arc::new(split_blocks(text)),
                    // Non-assistant rows don't go through markdown.
                    _ => Arc::new(Vec::new()),
                };
                let block_fps = blocks
                    .iter()
                    .map(|block| {
                        let mut h = fnv1a(&[10]);
                        h = fnv1a_more(h, block.source().as_bytes());
                        if let Block::Fence { closed, lang, .. } = block {
                            h = fnv1a_more(h, lang.as_bytes());
                            h = fnv1a_more(h, &[*closed as u8]);
                        }
                        h
                    })
                    .collect();
                let entry = MsgEntry {
                    rev,
                    base_fp: msg_content_hash(row),
                    blocks,
                    block_fps,
                };
                if ix < self.msgs.len() {
                    self.msgs[ix] = entry;
                } else {
                    self.msgs.push(entry);
                }
            }
        }
        let shrunk = self.msgs.len() > transcript.rows.len();
        self.msgs.truncate(transcript.rows.len());

        // 2. Rebuild the (Copy) row list from cached fingerprints — no
        //    hashing here, just index math over 10k-scale rows.
        let mut next: Vec<DisplayRow> = Vec::with_capacity(self.rows.len() + 4);
        for (ix, row) in transcript.rows.iter().enumerate() {
            let entry = &self.msgs[ix];
            match row {
                Row::Assistant { live, .. } => {
                    let n = entry.block_fps.len();
                    for (bix, fp) in entry.block_fps.iter().enumerate() {
                        // Streaming bit only on the still-growing tail
                        // block, so settled earlier blocks of a live
                        // message keep stable fingerprints across bursts.
                        let streaming = *live && bix + 1 == n;
                        next.push(DisplayRow {
                            id: RowId {
                                msg: ix as u32,
                                block: bix as u32,
                            },
                            fingerprint: if streaming { fp ^ STREAMING_SALT } else { *fp },
                        });
                    }
                    if n == 0 {
                        // Live message with no text yet: placeholder row so
                        // the cursor block has somewhere to blink.
                        next.push(DisplayRow {
                            id: RowId {
                                msg: ix as u32,
                                block: 0,
                            },
                            fingerprint: entry.base_fp,
                        });
                    }
                }
                _ => next.push(DisplayRow {
                    id: RowId {
                        msg: ix as u32,
                        block: 0,
                    },
                    fingerprint: entry.base_fp,
                }),
            }
        }
        // 3. Diff.
        let ops = if shrunk || (self.rows.is_empty() && next.is_empty()) {
            // A shrink only happens on reset/replay-from-scratch.
            DiffOps {
                reset: !next.is_empty() || !self.rows.is_empty(),
                ..Default::default()
            }
        } else {
            diff(&self.rows, &next)
        };
        self.rows = next;
        ops
    }
}

/// Prefix/suffix diff over (id, fingerprint) pairs → one minimal splice
/// plus a remeasure range for rows changed in place (spec 03 §Rendering).
fn diff(prev: &[DisplayRow], next: &[DisplayRow]) -> DiffOps {
    let mut prefix = 0;
    while prefix < prev.len() && prefix < next.len() && prev[prefix] == next[prefix] {
        prefix += 1;
    }
    if prefix == prev.len() && prefix == next.len() {
        return DiffOps::default();
    }
    let mut suffix = 0;
    while suffix < prev.len() - prefix
        && suffix < next.len() - prefix
        && prev[prev.len() - 1 - suffix] == next[next.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mid_prev = &prev[prefix..prev.len() - suffix];
    let mid_next = &next[prefix..next.len() - suffix];

    // Peel the id-preserving head of the middle: those rows changed in
    // place (live growth, fence close, live→complete flip) and are
    // remeasured, never spliced — so a burst that closes a fence AND
    // appends a new block re-creates nothing that is already on screen.
    let keep = mid_prev
        .iter()
        .zip(mid_next)
        .take_while(|(a, b)| a.id == b.id)
        .count();
    let remeasure = (keep > 0).then(|| prefix..prefix + keep);
    let splice = (mid_prev.len() != keep || mid_next.len() != keep)
        .then(|| (prefix + keep..prev.len() - suffix, mid_next.len() - keep));
    DiffOps {
        splice,
        remeasure,
        reset: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::model::Transcript;

    fn assistant(text: &str, live: bool) -> Transcript {
        let mut t = Transcript::new();
        t.push_row(Row::Assistant {
            text: text.into(),
            live,
        });
        t
    }

    /// Replace an assistant row's text/liveness in place, revision-tracked
    /// — what a delta burst does through `Transcript::apply`.
    fn set(t: &mut Transcript, ix: usize, text: &str, live: bool) {
        if let Row::Assistant { text: tx, live: lv } = &mut t.rows[ix] {
            *tx = text.into();
            *lv = live;
        } else {
            panic!("row {ix} is not assistant");
        }
        t.touch(ix);
    }

    #[test]
    fn streaming_growth_is_remeasure_not_splice() {
        let mut d = DisplayList::new();
        let mut t = assistant("hello wo", true);
        d.update(&t);
        set(&mut t, 0, "hello world, more text", true);
        let ops = d.update(&t);
        assert!(ops.splice.is_none(), "in-place growth must not splice: {ops:?}");
        assert_eq!(ops.remeasure, Some(0..1));
    }

    #[test]
    fn new_block_is_one_minimal_splice() {
        let mut d = DisplayList::new();
        let mut t = assistant("para one", true);
        d.update(&t);
        assert_eq!(d.len(), 1);
        // Blank line starts block two: block one changes in place (loses
        // its streaming bit → remeasure) and the new row is a pure tail
        // insertion — nothing on screen is re-created.
        set(&mut t, 0, "para one\n\npara tw", true);
        let ops = d.update(&t);
        assert_eq!(ops.remeasure, Some(0..1));
        let (range, count) = ops.splice.expect("block count grew: splice");
        assert_eq!(range, 1..1);
        assert_eq!(count, 1);
        // Steady growth inside block two leaves block one untouched.
        set(&mut t, 0, "para one\n\npara two grows", true);
        let ops = d.update(&t);
        assert!(ops.splice.is_none());
        assert_eq!(ops.remeasure, Some(1..2));
    }

    #[test]
    fn fence_close_and_settle_flip_is_remeasure() {
        let mut d = DisplayList::new();
        let mut t = assistant("intro\n\n```rust\nlet x = 1;", true);
        d.update(&t);
        let before: Vec<RowId> = d.rows().iter().map(|r| r.id).collect();
        // Fence closes…
        set(&mut t, 0, "intro\n\n```rust\nlet x = 1;\n```", true);
        let ops = d.update(&t);
        assert!(ops.splice.is_none(), "fence close must not splice: {ops:?}");
        // …and the message settles (live→complete).
        set(&mut t, 0, "intro\n\n```rust\nlet x = 1;\n```", false);
        let ops2 = d.update(&t);
        assert!(ops2.splice.is_none(), "settle must remeasure, never splice: {ops2:?}");
        assert!(ops2.remeasure.is_some());
        let after: Vec<RowId> = d.rows().iter().map(|r| r.id).collect();
        assert_eq!(before, after, "row identity stable across close+settle");
    }

    #[test]
    fn untouched_messages_keep_fingerprints_across_bursts() {
        let mut d = DisplayList::new();
        let mut t = assistant("done message\n\nwith two blocks", false);
        t.push_row(Row::Assistant {
            text: "live tail".into(),
            live: true,
        });
        d.update(&t);
        let fp_before: Vec<u64> = d.rows()[..2].iter().map(|r| r.fingerprint).collect();
        set(&mut t, 1, "live tail grows", true);
        let ops = d.update(&t);
        let fp_after: Vec<u64> = d.rows()[..2].iter().map(|r| r.fingerprint).collect();
        assert_eq!(fp_before, fp_after);
        assert_eq!(ops.remeasure, Some(2..3));
    }

    #[test]
    fn reset_rebuild_reports_reset() {
        let mut d = DisplayList::new();
        let mut t = assistant("one", false);
        t.push_row(Row::Marker { text: "x".into() });
        d.update(&t);
        // Epoch change: transcript resets and replays shorter content.
        t.reset();
        t.push_row(Row::Assistant {
            text: "shorter".into(),
            live: true,
        });
        let ops = d.update(&t);
        assert!(ops.reset);
    }
}
