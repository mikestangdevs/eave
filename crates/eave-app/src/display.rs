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

use eave_core::tool::ToolCall;
use eave_wire::model::{ApprovalStatus, Delivery, Row, ToolStatus, Transcript, TurnSpan};

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

/// Block index of a turn's fold row (Slice AT1).
///
/// A fold row's `RowId.msg` is the index of the row that OPENED the turn,
/// not the first row it hides. That is waku's scar, carried verbatim
/// (`waku/src/app/transcript.rs:899-905`): anchoring the divider at the
/// first hidden row strands it mid-turn every time the agent thinks before
/// it speaks, because the first hidden row is then somewhere down inside
/// the turn rather than at its head. Anchored at the top, the fold row
/// keeps one stable identity no matter what the turn later turns out to
/// contain.
///
/// It shares `msg` with the turn's user row and is told apart by this
/// block index, which no real markdown block can collide with — an
/// assistant message with `u32::MAX` blocks does not fit in memory.
pub const FOLD_BLOCK: u32 = u32::MAX;

/// Fewest hidden rows worth collapsing. One row replaced by one summary
/// row is not a fold, it is a substitution — and it would cost the reader
/// the actual tool row to save nothing.
const FOLD_MIN_HIDDEN: usize = 2;

/// What a folded turn did, counted from the rows it hides (Slice AT1, gap
/// #6). Pure data: [`summarize_turn`] is a function of rows and nothing
/// else, which is what lets it be unit-tested against a fixture turn
/// without a window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnSummary {
    pub reads: usize,
    pub commands: usize,
    /// Counted by DISTINCT FILE, not by call — t3code's rule
    /// (`MessagesTimeline.logic.ts:277-360`). Three edits to one file are
    /// one changed file, because that is what the reader is counting.
    pub files_changed: usize,
    pub searches: usize,
    /// Everything typed but not in a bucket above, plus every tool this
    /// build could not decode. Counted so the sentence can admit there was
    /// more, never silently dropped.
    pub other: usize,
    /// How many transcript rows the fold hides.
    pub hidden: usize,
    /// **Lifted onto the collapsed row so a fold never hides red.** Any
    /// failed or interrupted tool in the turn sets this; the row keeps the
    /// failure tint whether or not it is open.
    pub has_failure: bool,
    /// From [`TurnSpan::elapsed_ms`]; `None` when no clock covered the
    /// turn, and then the row simply does not claim one.
    pub elapsed_ms: Option<u64>,
}

impl TurnSummary {
    /// t3code's sentence: "Read 3 files, ran 2 commands, changed 4 files".
    ///
    /// Empty when the turn did nothing countable, so the caller can fall
    /// back to the bare hidden count rather than print a sentence with no
    /// clauses in it.
    pub fn sentence(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                one.to_string()
            } else {
                format!("{n} {many}")
            }
        };
        if self.reads > 0 {
            parts.push(format!("read {}", plural(self.reads, "a file", "files")));
        }
        if self.commands > 0 {
            parts.push(format!(
                "ran {}",
                plural(self.commands, "a command", "commands")
            ));
        }
        if self.searches > 0 {
            parts.push(format!(
                "ran {}",
                plural(self.searches, "a search", "searches")
            ));
        }
        if self.files_changed > 0 {
            parts.push(format!(
                "changed {}",
                plural(self.files_changed, "a file", "files")
            ));
        }
        if self.other > 0 {
            parts.push(format!("{} more", self.other));
        }
        match parts.len() {
            0 => String::new(),
            1 => parts.remove(0),
            _ => {
                let last = parts.pop().unwrap();
                format!("{}, {last}", parts.join(", "))
            }
        }
    }
}

/// Count what a turn's rows did (Slice AT1, gap #6).
///
/// **Classifies off the adapter's typed decode first.** `ToolCall` is the
/// adapter's own reading of the provider's schema, so a Codex
/// `commandExecution` and a Pi `bash` both count as one command without
/// this function knowing either name. The name-keyed fallback below it
/// exists only for rows written before adapters decoded anything, which
/// are all Claude rows — the same reasoning, and the same key set, as
/// `eave_wire::model::tool_detail`.
pub fn summarize_turn(rows: &[Row]) -> TurnSummary {
    let mut s = TurnSummary {
        hidden: rows.len(),
        ..Default::default()
    };
    // Distinct changed files. A Vec, not a set: a turn's edits are a
    // handful, and this runs once per turn per settle — an allocation of
    // a hash map here would cost more than the linear scan it replaces.
    let mut changed: Vec<&str> = Vec::new();
    // A nested fn rather than a closure: the borrowed path outlives the
    // call, and a closure's inferred lifetimes cannot say so.
    fn note_changed<'a>(path: Option<&'a str>, changed: &mut Vec<&'a str>, other: &mut usize) {
        match path {
            Some(p) if !p.is_empty() => {
                if !changed.iter().any(|c| *c == p) {
                    changed.push(p);
                }
            }
            // A multi-file patch that named no single path is still a
            // change, and it is not a file we can name — so it is counted
            // as work rather than as a file.
            _ => *other += 1,
        }
    }
    for row in rows {
        let Row::Tool {
            name,
            status,
            is_error,
            call,
            ..
        } = row
        else {
            continue;
        };
        if *is_error || matches!(status, ToolStatus::Interrupted) {
            s.has_failure = true;
        }
        match call {
            Some(ToolCall::Read { .. }) | Some(ToolCall::List { .. }) => s.reads += 1,
            Some(ToolCall::Run { .. }) => s.commands += 1,
            Some(ToolCall::Grep { .. })
            | Some(ToolCall::Glob { .. })
            | Some(ToolCall::WebSearch { .. })
            | Some(ToolCall::Fetch { .. }) => s.searches += 1,
            Some(ToolCall::Write { path }) | Some(ToolCall::Edit { path }) => {
                note_changed(Some(path.as_str()), &mut changed, &mut s.other)
            }
            Some(ToolCall::Patch { path }) => {
                note_changed(path.as_deref(), &mut changed, &mut s.other)
            }
            Some(ToolCall::Todo { .. }) | Some(ToolCall::Task { .. }) => s.other += 1,
            // Undecoded, or decoded as a tool this build does not know.
            Some(ToolCall::Unknown { .. }) | None => match name.as_str() {
                "Bash" | "BashOutput" => s.commands += 1,
                "Read" | "NotebookRead" => s.reads += 1,
                "Grep" | "Glob" | "WebSearch" | "WebFetch" => s.searches += 1,
                "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
                    // No typed path to dedupe on; count it as work rather
                    // than inventing a filename.
                    s.other += 1
                }
                _ => s.other += 1,
            },
        }
    }
    s.files_changed = changed.len();
    s
}

/// What the surface tells the projection about folding (Slice AT1).
#[derive(Debug, Clone, Copy, Default)]
pub struct FoldInput<'a> {
    /// Turn extents from the transcript projection.
    pub turns: &'a [TurnSpan],
    /// Turn start rows the reader has opened by hand. Expansion is view
    /// state, like `open_tools` — never a property of the transcript.
    pub expanded: &'a [usize],
}

impl FoldInput<'_> {
    /// Fold nothing: the projection's behaviour before this slice, and
    /// what every caller that has no turn state passes.
    pub fn unfolded() -> Self {
        Self {
            turns: &[],
            expanded: &[],
        }
    }
}

/// One turn's decided fold: which rows vanish, and what the row that
/// replaces them says.
#[derive(Debug, Clone)]
struct Fold {
    /// The turn's opening row — the fold row's anchor and its `msg`.
    start_row: usize,
    /// Half-open range of transcript rows this fold hides.
    hidden: std::ops::Range<usize>,
    summary: TurnSummary,
    fingerprint: u64,
}


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
    /// The folds currently in force, in turn order (Slice AT1). Rebuilt
    /// per update from settled turns only, so a fold transition is a
    /// structural splice at SETTLE — never per frame, and never while the
    /// turn is still streaming.
    folds: Vec<Fold>,
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
        Row::Assistant { text, live } => fnv1a_more(fnv1a(&[1, *live as u8]), text.as_bytes()),
        Row::Tool {
            id,
            name,
            status,
            input,
            result,
            is_error,
            // Deliberately not hashed, on two independent grounds. The
            // typed call only ever changes together with the `input` it
            // was decoded from — both are written from one event — and
            // that write goes through `Transcript::touch`, so the row's
            // REVISION has already changed and this fingerprint is not
            // what the cache consults. Hashing it anyway would cost an
            // allocation per row inside the fold, and the 10k-row gate
            // builds every row cold.
            call: _,
            // Not hashed either. The load-bearing fact is NOT that
            // `result` is hashed alongside it — that was this note's first
            // wording and it is the weaker ground. It is that
            // `finish_tool` unconditionally sets `*status =
            // ToolStatus::Finished`, and `status` IS hashed: it is the
            // `tag` byte below. So a row cannot gain a diff without this
            // fingerprint moving via status, whether or not the result
            // changed.
            //
            // That makes the revisit condition narrow and specific: a
            // SECOND `finish_tool` on an already-Finished row, carrying a
            // result of identical length — identical length because the
            // result hash is a bounded prefix plus length, not the whole
            // payload. Nothing does that today. (Slice AG1 #4; ground
            // sharpened by the AT1 lane, who owns this file.)
            diff: _,
        } => {
            let tag: u8 = match status {
                ToolStatus::Running => 1,
                ToolStatus::Finished => 2,
                ToolStatus::Interrupted => 3,
            };
            let h = fnv1a_more(
                fnv1a_more(fnv1a(&[2, tag, *is_error as u8]), id.as_bytes()),
                name.as_bytes(),
            );
            // Length plus a BOUNDED prefix, not the whole payload. Payloads
            // are already capped at 8 KiB apiece, but this runs per changed
            // row inside the fold, and the 10k-row gate builds every row
            // cold — hashing 8 KiB × 10k would put 80 MB through FNV inside
            // a 4 ms budget. The cost of the shortcut is a false cache hit
            // for two payloads of identical length differing only past the
            // prefix, whose worst case is one row rendering stale until its
            // next change. That trade is why the length is in the hash.
            fn bounded(h: u64, s: &str) -> u64 {
                const PREFIX: usize = 256;
                let mut end = s.len().min(PREFIX);
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                fnv1a_more(fnv1a_more(h, &s.len().to_le_bytes()), &s.as_bytes()[..end])
            }
            bounded(bounded(h, input), result)
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

    /// The summary behind a fold row, by the turn's opening row index.
    pub fn fold_summary(&self, msg: u32) -> Option<&TurnSummary> {
        self.folds
            .iter()
            .find(|f| f.start_row as u32 == msg)
            .map(|f| &f.summary)
    }

    /// Is there a fold in force on the turn opening at this row?
    pub fn is_folded(&self, start_row: usize) -> bool {
        self.folds.iter().any(|f| f.start_row == start_row)
    }

    /// Decide every fold, once, from settled turns (Slice AT1).
    ///
    /// A turn folds only when it has ENDED. The streaming turn never folds
    /// — not by a special case here, but because its span has no
    /// `ended_ms` until a terminal frame closes it, so it cannot reach the
    /// body of this loop.
    fn plan_folds(transcript: &Transcript, fold: &FoldInput) -> Vec<Fold> {
        let rows = &transcript.rows;
        let mut out = Vec::new();
        for (n, span) in fold.turns.iter().enumerate() {
            if span.ended_ms.is_none() || span.start_row >= rows.len() {
                continue;
            }
            if fold.expanded.contains(&span.start_row) {
                continue;
            }
            let end_row = fold
                .turns
                .get(n + 1)
                .map(|t| t.start_row.min(rows.len()))
                .unwrap_or(rows.len());
            if end_row <= span.start_row + 1 {
                continue;
            }
            let turn = &rows[span.start_row + 1..end_row];
            // Defensive, and cheap: a span can only be here if a terminal
            // frame closed it, but a row still marked live inside a closed
            // turn would be a fold over moving content.
            // Three things forbid a fold, and the third is a judgment
            // call, so it is argued rather than asserted.
            //
            // A live row is obvious: a fold over moving content is a fold
            // over a measure that is about to change.
            //
            // **An approval row — ANSWERED OR NOT — stops the fold, and
            // that is a deliberate departure from what waku and t3code
            // do.** Neither of them has eave's approval row, so neither's
            // fold rule speaks to it. A fold summarises WHAT THE AGENT
            // DID: "read 3 files, ran 2 commands". An approval is not
            // that. It is a decision the OPERATOR made, and the sentence
            // has no vocabulary for it — so folding one away replaces "you
            // approved this command" with a count of files, which is not a
            // shorter version of the same fact, it is a different fact.
            // eave's approval row carries every fact needed to decide
            // (`model.rs:145-152`); hiding the record of that decision
            // inside a work summary gives up the thing this transcript is
            // better at than the four references.
            //
            // Stated plainly because it matters for how this is reviewed:
            // this rule also restores the `approvals-synthetic` replay
            // gate, which the fold had shortened below the anchor assert's
            // arming threshold. The gate is what sent me looking; the
            // reason above is why the rule stays. If a reviewer disagrees
            // with the reason, the honest fix is the OTHER one in the
            // report — lengthen the fixture — not this line.
            if turn.iter().any(|r| {
                matches!(r, Row::Assistant { live: true, .. })
                    || matches!(
                        r,
                        Row::Tool {
                            status: ToolStatus::Running,
                            ..
                        }
                    )
                    || matches!(r, Row::Approval { .. })
            }) {
                continue;
            }
            // The answer is the TRAILING run of assistant text. Everything
            // before it — reasoning, tools, interim commentary alike — is
            // what the fold hides. A turn that ended without text has no
            // trailing run, so it folds entirely.
            let mut answer_start = end_row;
            while answer_start > span.start_row + 1
                && matches!(rows[answer_start - 1], Row::Assistant { .. })
            {
                answer_start -= 1;
            }
            let hidden = span.start_row + 1..answer_start;
            if hidden.len() < FOLD_MIN_HIDDEN {
                continue;
            }
            let mut summary = summarize_turn(&rows[hidden.clone()]);
            summary.elapsed_ms = span.elapsed_ms();
            // The fold row's fingerprint is its SENTENCE, not the rows it
            // hides: two turns that did the same work read identically and
            // must measure identically. Settled turns never change, so
            // this is stable for the life of the fold.
            let mut h = fnv1a(&[20, summary.has_failure as u8]);
            for n in [
                summary.reads,
                summary.commands,
                summary.files_changed,
                summary.searches,
                summary.other,
                summary.hidden,
            ] {
                h = fnv1a_more(h, &n.to_le_bytes());
            }
            h = fnv1a_more(h, &summary.elapsed_ms.unwrap_or(0).to_le_bytes());
            out.push(Fold {
                start_row: span.start_row,
                hidden,
                summary,
                fingerprint: h,
            });
        }
        out
    }

    /// Fold the transcript into rows and report the minimal list ops.
    ///
    /// Non-assistant transcript rows are one display row each; assistant
    /// rows explode into one display row per top-level markdown block.
    pub fn update(&mut self, transcript: &Transcript) -> DiffOps {
        self.update_with(transcript, &FoldInput::unfolded())
    }

    /// [`DisplayList::update`], with the turn fold applied (Slice AT1).
    pub fn update_with(&mut self, transcript: &Transcript, fold: &FoldInput) -> DiffOps {
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

        // 2. Decide the folds, then rebuild the (Copy) row list from
        //    cached fingerprints — no hashing here, just index math over
        //    10k-scale rows.
        self.folds = Self::plan_folds(transcript, fold);
        let mut next: Vec<DisplayRow> = Vec::with_capacity(self.rows.len() + 4);
        // Folds are ordered by `start_row` and never overlap, so ONE
        // cursor walks them alongside the rows: O(rows + folds). A scan of
        // the fold list per row would have made this loop quadratic in
        // turns, and the 10k-row gate builds every row cold — the one
        // place in this file where that difference is the budget.
        let mut fold_ix = 0usize;
        for (ix, row) in transcript.rows.iter().enumerate() {
            while fold_ix < self.folds.len() && self.folds[fold_ix].hidden.end <= ix {
                fold_ix += 1;
            }
            if let Some(f) = self.folds.get(fold_ix) {
                // A folded turn contributes its opening row, then ONE fold
                // row carrying the anchor, and nothing else until its
                // answer.
                if f.start_row + 1 == ix {
                    next.push(DisplayRow {
                        id: RowId {
                            msg: f.start_row as u32,
                            block: FOLD_BLOCK,
                        },
                        fingerprint: f.fingerprint,
                    });
                }
                if f.hidden.contains(&ix) {
                    continue;
                }
            }
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

    // ── Slice AT1: the settled transcript ───────────────────────────────

    fn tool(id: &str, name: &str, call: Option<ToolCall>, is_error: bool) -> Row {
        Row::Tool {
            id: id.into(),
            name: name.into(),
            status: if is_error {
                ToolStatus::Finished
            } else {
                ToolStatus::Finished
            },
            input: String::new(),
            diff: None,
            result: String::new(),
            is_error,
            call,
        }
    }

    fn user(text: &str) -> Row {
        Row::User {
            text: text.into(),
            delivery: Delivery::Delivered,
        }
    }

    /// A settled turn: one prompt, `work` rows, then the answer.
    fn settled_turn(work: Vec<Row>, answer: Option<&str>) -> Transcript {
        let mut t = Transcript::new();
        t.push_row(user("go"));
        t.turns.push(TurnSpan {
            start_row: 0,
            started_ms: 1_000,
            ended_ms: Some(13_000),
        });
        for r in work {
            t.push_row(r);
        }
        if let Some(a) = answer {
            t.push_row(Row::Assistant {
                text: a.into(),
                live: false,
            });
        }
        t
    }

    fn folded(t: &Transcript) -> DisplayList {
        let mut d = DisplayList::new();
        d.update_with(
            t,
            &FoldInput {
                turns: &t.turns,
                expanded: &[],
            },
        );
        d
    }

    #[test]
    fn the_summary_counts_edits_by_distinct_file_not_by_call() {
        // Three edits, two of them to the SAME file: two files changed.
        let rows = vec![
            tool("1", "Read", Some(ToolCall::Read { path: "a.rs".into() }), false),
            tool("2", "Read", Some(ToolCall::Read { path: "b.rs".into() }), false),
            tool("3", "Read", Some(ToolCall::Read { path: "c.rs".into() }), false),
            tool("4", "Bash", Some(ToolCall::Run { command: "ls".into() }), false),
            tool("5", "Bash", Some(ToolCall::Run { command: "pwd".into() }), false),
            tool("6", "Edit", Some(ToolCall::Edit { path: "x.rs".into() }), false),
            tool("7", "Edit", Some(ToolCall::Edit { path: "x.rs".into() }), false),
            tool("8", "Write", Some(ToolCall::Write { path: "y.rs".into() }), false),
        ];
        let s = summarize_turn(&rows);
        assert_eq!(s.reads, 3);
        assert_eq!(s.commands, 2);
        assert_eq!(s.files_changed, 2, "x.rs edited twice is ONE changed file");
        assert!(!s.has_failure);
        assert_eq!(s.sentence(), "read 3 files, ran 2 commands, changed 2 files");
    }

    #[test]
    fn one_of_each_reads_as_singular() {
        let rows = vec![
            tool("1", "Read", Some(ToolCall::Read { path: "a.rs".into() }), false),
            tool("2", "Bash", Some(ToolCall::Run { command: "ls".into() }), false),
        ];
        assert_eq!(summarize_turn(&rows).sentence(), "read a file, ran a command");
    }

    #[test]
    fn a_failure_anywhere_in_the_turn_lifts_onto_the_summary() {
        let rows = vec![
            tool("1", "Read", Some(ToolCall::Read { path: "a.rs".into() }), false),
            tool("2", "Bash", Some(ToolCall::Run { command: "ls".into() }), true),
        ];
        assert!(
            summarize_turn(&rows).has_failure,
            "a fold must never hide red"
        );
    }

    #[test]
    fn an_undecoded_claude_row_still_counts_by_name() {
        // `call: None` is every row written before adapters decoded
        // anything, and all of those are Claude rows.
        let rows = vec![
            tool("1", "Bash", None, false),
            tool("2", "Read", None, false),
        ];
        let s = summarize_turn(&rows);
        assert_eq!((s.commands, s.reads), (1, 1));
    }

    #[test]
    fn a_forty_call_turn_settles_into_one_summary_row_plus_its_answer() {
        let work: Vec<Row> = (0..40)
            .map(|i| {
                tool(
                    &i.to_string(),
                    "Bash",
                    Some(ToolCall::Run {
                        command: format!("cmd {i}"),
                    }),
                    false,
                )
            })
            .collect();
        let t = settled_turn(work, Some("done"));
        let d = folded(&t);
        // user row + fold row + the one answer block.
        assert_eq!(d.len(), 3, "40 tool rows collapsed to one");
        assert_eq!(d.row(1).unwrap().id.block, FOLD_BLOCK);
        assert_eq!(d.fold_summary(0).unwrap().commands, 40);
        assert_eq!(d.fold_summary(0).unwrap().hidden, 40);
    }

    #[test]
    fn the_fold_anchor_sits_at_the_top_of_the_turn() {
        // waku's scar (`transcript.rs:899-905`): the agent works BEFORE it
        // speaks, so the first hidden row is not the turn's head. The fold
        // row must still be anchored at the head, or it strands mid-turn.
        let work = vec![
            tool("1", "Bash", Some(ToolCall::Run { command: "a".into() }), false),
            tool("2", "Bash", Some(ToolCall::Run { command: "b".into() }), false),
        ];
        let t = settled_turn(work, Some("answer"));
        let d = folded(&t);
        let fold = d.rows().iter().find(|r| r.id.block == FOLD_BLOCK).unwrap();
        assert_eq!(
            fold.id.msg, 0,
            "anchored at the turn's opening row, not at the first hidden row (1)"
        );
    }

    #[test]
    fn a_turn_that_produced_no_text_folds_entirely() {
        let work = vec![
            tool("1", "Bash", Some(ToolCall::Run { command: "a".into() }), false),
            tool("2", "Bash", Some(ToolCall::Run { command: "b".into() }), false),
        ];
        let t = settled_turn(work, None);
        let d = folded(&t);
        assert_eq!(d.len(), 2, "just the prompt and the fold row");
    }

    #[test]
    fn expanding_restores_every_row() {
        let work: Vec<Row> = (0..8)
            .map(|i| tool(&i.to_string(), "Bash", Some(ToolCall::Run { command: "x".into() }), false))
            .collect();
        let t = settled_turn(work, Some("done"));
        let mut d = DisplayList::new();
        // Unfolded reference: what the transcript looks like with no fold.
        let full = {
            let mut r = DisplayList::new();
            r.update_with(&t, &FoldInput::unfolded());
            r.rows().to_vec()
        };
        d.update_with(
            &t,
            &FoldInput {
                turns: &t.turns,
                expanded: &[],
            },
        );
        assert_eq!(d.len(), 3);
        d.update_with(
            &t,
            &FoldInput {
                turns: &t.turns,
                expanded: &[0],
            },
        );
        assert_eq!(
            d.rows(),
            full.as_slice(),
            "expanded is bit-identical to never-folded: same ids, same fingerprints"
        );
    }

    #[test]
    fn the_streaming_turn_never_folds() {
        let mut t = Transcript::new();
        t.push_row(user("go"));
        // Open span: no `ended_ms`, which is what "still streaming" IS.
        t.turns.push(TurnSpan {
            start_row: 0,
            started_ms: 1_000,
            ended_ms: None,
        });
        for i in 0..5 {
            t.push_row(tool(
                &i.to_string(),
                "Bash",
                Some(ToolCall::Run { command: "x".into() }),
                false,
            ));
        }
        let d = folded(&t);
        assert_eq!(d.len(), 6, "nothing folds while the turn is open");
        assert!(d.rows().iter().all(|r| r.id.block != FOLD_BLOCK));
    }

    #[test]
    fn a_turn_too_small_to_be_worth_folding_is_left_alone() {
        let work = vec![tool("1", "Bash", Some(ToolCall::Run { command: "a".into() }), false)];
        let t = settled_turn(work, Some("done"));
        let d = folded(&t);
        assert!(d.rows().iter().all(|r| r.id.block != FOLD_BLOCK));
    }

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
        assert!(
            ops.splice.is_none(),
            "in-place growth must not splice: {ops:?}"
        );
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
        assert!(
            ops2.splice.is_none(),
            "settle must remeasure, never splice: {ops2:?}"
        );
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
