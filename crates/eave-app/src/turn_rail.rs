//! **Slice AG1 #12 — the jump-to-turn rail.**
//!
//! A column of ticks in the transcript's left gutter, one per turn: the
//! active one brightens, hovering names the prompt, clicking jumps to it.
//! All three GPUI references ship one — comet's `crates/ui/src/rail.rs`
//! ("a left vertical minimap of the user's prompts"), waku's
//! `NAVIGATION_RAIL_*` in `src/app/transcript.rs`, and t3code's
//! `TimelineMinimap` — which makes it the one item in AG1 that is
//! unanimous in framework rather than merely convergent.
//!
//! # What this module is not allowed to do
//!
//! **It never owns transcript measurement.** Both GPUI references built
//! this as a SEPARATE list that only asks the transcript to scroll, and
//! that is the property that makes it safe here: eave's list carries
//! anchor asserts, mending and a freeze margin, and a navigation aid that
//! took a view on row heights would be arguing with all three.
//!
//! The jump itself is therefore not new machinery. [`ScrollMode::Pinned`]
//! already existed for the send-anchor flip — *"pin the given transcript
//! row's first display row to the viewport top, re-asserted on every
//! update"* — which is exactly a jump-to-turn, including the part that is
//! easy to miss: because it re-asserts, a jump survives a streaming append
//! and a fold transition instead of being scrolled out from under the
//! reader by the next batch.
//!
//! **And it scrolls by ITEM, never by pixels.** comet's rail glides, over
//! a per-frame row-height EMA, because rows above the viewport are
//! unmeasured. This file does not, and the reason is written down two
//! screens from where it would have been made: `arm_replay_anchor`'s
//! *"THE RULE: ANCHOR BY ITEM INDEX, NEVER BY PIXELS — a `ListState` row
//! that no frame has laid out is ZERO PIXELS TALL in the sum-tree that a
//! pixel scroll walks"*. A pixel glide across a long unmeasured history is
//! the ~12% approvals failure of `docs/reports/stage-c4-anchor.md` with a
//! nicer easing curve on it.
//!
//! # Density, stated rather than hidden
//!
//! Every turn gets a tick. When there are more turns than fit, the rail
//! does not drop the older ones — it compresses, t3code's model
//! (`resolveTimelineMinimapHeightStyle` + `resolveTimelineMinimapTopPercent`):
//! rail height is `min(count × SLOT, viewport × RATIO)` and a tick's slot
//! is `height / count`, so one formula covers both regimes and the slots
//! never overlap. comet's alternative — a hard `MAX_RAIL_TICKS = 12` — was
//! rejected on this codebase's own rule: a map that silently shows twelve
//! of two hundred turns is the collapsed group with no attention rollup,
//! and the `omitted > 0` diff stat, in a third costume.
//!
//! The honest limit that remains: past roughly forty turns in a standard
//! window the slots are thinner than a comfortable click target. Nothing
//! is hidden and the ticks visibly crowd — the rail LOOKS dense because it
//! IS — and ⌥⌘↑/↓ walks turns exactly at any density.

use unicode_segmentation::UnicodeSegmentation;

use crate::display::DisplayList;
use eave_wire::model::{Row, TurnSpan};

/// The rail's inset from the well's left edge.
pub const RAIL_LEFT: f32 = 12.0;
/// The hit strip's width — the whole strip is the target, not the mark.
pub const RAIL_W: f32 = 20.0;
/// Clear space demanded between the strip and the reading column.
///
/// **t3code names the failure this prevents** — *"a fixed-width hover
/// strip would then sit on top of the message text and swallow its
/// pointer events"* (`resolveTimelineMinimapHitStripWidth`). eave's
/// transcript rows are all wrapped in `Shell::column`, a centered
/// `CONTENT_MAX_W` cap inside a `GUTTER` pad, so the gutter this rail
/// lives in shrinks as the well narrows — exactly the geometry that note
/// is about.
pub const RAIL_GAP: f32 = 16.0;
/// The visible mark, centered in the strip.
pub const TICK_W: f32 = 12.0;
pub const TICK_H: f32 = 2.0;
/// One turn's slot at full spacing (waku's `NAVIGATION_RAIL_TURN_HEIGHT`,
/// which is its 2 px tick plus a 10 px gap).
pub const SLOT: f32 = 12.0;
/// Share of the viewport the rail may occupy before it compresses.
pub const VIEWPORT_RATIO: f32 = 0.72;
/// Fewest turns worth a rail.
///
/// waku's gate, and for waku's reason: a rail over one turn is not a map
/// of anything, it is a dot beside the only thing on screen.
pub const MIN_TURNS: usize = 2;
/// Graphemes of the prompt kept for the hover label (waku's cap for the
/// same string). Grapheme-aware, not `chars`: a cut mid-cluster renders a
/// broken glyph, and the label is the only thing telling a reader which
/// turn a 2 px mark belongs to.
pub const PROMPT_GRAPHEMES: usize = 100;

/// One turn's mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// The transcript row that opened the turn. This is the `msg` a
    /// [`crate::ui::ScrollMode::Pinned`] takes, and the fold row's anchor
    /// — one index means the same thing in all three places.
    pub start_row: usize,
    /// Where that turn currently sits in the DISPLAY list, which is not
    /// the same number: folding removes rows above it.
    pub display_row: usize,
    /// The prompt's opening, for the hover label.
    pub prompt: String,
}

/// Build one tick per turn.
///
/// **One cursor walk, not a lookup per turn.** The obvious implementation
/// calls `DisplayList::first_row_of_msg` in the loop, and that is a
/// `rows.iter().position(..)` scan from the top — so the obvious
/// implementation is quadratic in a long session. waku paid for this and
/// wrote the receipt at `transcript.rs:451`: *"Message rows keep ascending
/// message order through folding, so one cursor walk resolves every
/// prompt's row. A `position` scan from the top per turn made this
/// quadratic over a long session."* The same premise holds here — a fold
/// row carries its turn's opening row as its `msg` (`display::FOLD_BLOCK`)
/// and hidden rows are removed, so display rows stay in ascending `msg`
/// order — so the same walk works.
pub fn ticks(rows: &[Row], turns: &[TurnSpan], display: &DisplayList) -> Vec<Tick> {
    let drows = display.rows();
    let mut cursor = 0usize;
    let mut out = Vec::with_capacity(turns.len());
    for span in turns {
        let Some(row) = rows.get(span.start_row) else {
            continue;
        };
        let want = span.start_row as u32;
        while cursor < drows.len() && drows[cursor].id.msg < want {
            cursor += 1;
        }
        if cursor >= drows.len() {
            break;
        }
        // A turn whose opening row is not in the display list gets no
        // tick rather than a tick pointing somewhere else. The fold
        // never hides it (`plan_folds` hides `start_row + 1..`), so this
        // is a guard and not a case.
        if drows[cursor].id.msg != want {
            continue;
        }
        out.push(Tick {
            start_row: span.start_row,
            display_row: cursor,
            prompt: snippet(row_text(row), PROMPT_GRAPHEMES),
        });
    }
    out
}

/// The prompt's words. A turn opens on a `Row::User` by construction
/// (`TurnSpan::start_row` — *"always the `Row::User` the prompt
/// produced"*); anything else yields no label rather than a wrong one.
fn row_text(row: &Row) -> &str {
    match row {
        Row::User { text, .. } => text,
        _ => "",
    }
}

/// Whitespace-normalized, grapheme-capped, ellipsized only when something
/// was actually cut.
pub fn snippet(content: &str, max_graphemes: usize) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut graphemes = normalized.graphemes(true);
    let head = graphemes.by_ref().take(max_graphemes).collect::<String>();
    if graphemes.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Which tick the reader is inside: the last one at or above the
/// viewport's top row.
///
/// **`at_end` is not an optimization, it is the bottom-aligned list's
/// answer.** waku short-circuits it (`active_navigation_turn_index`) and
/// eave needs it more, not less: this list is `ListAlignment::Bottom` with
/// follow-tail, so parked at the end its `logical_scroll_top` is a glued
/// anchor that does not report the newest turn's row. Without this the
/// rail would light the wrong tick in the one position the transcript
/// spends most of its life in.
pub fn active(tick_rows: &[usize], top_row: usize, at_end: bool) -> Option<usize> {
    if tick_rows.is_empty() {
        return None;
    }
    if at_end {
        return Some(tick_rows.len() - 1);
    }
    Some(
        tick_rows
            .partition_point(|row| *row <= top_row)
            .saturating_sub(1),
    )
}

/// The rail's height: full spacing until it would outgrow its share of
/// the viewport, then whatever that share is.
pub fn rail_height(count: usize, viewport_h: f32) -> f32 {
    if count == 0 || viewport_h <= 0.0 {
        return 0.0;
    }
    (count as f32 * SLOT).min(viewport_h * VIEWPORT_RATIO)
}

/// One tick's slot — its share of the rail, and its hit target.
///
/// Slots tile the rail exactly, so two ticks can never overlap and a
/// click can never be eaten by the neighbour painted after it.
pub fn slot_h(count: usize, height: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    height / count as f32
}

/// Distance from the rail's top to the top of tick `index`'s slot.
pub fn slot_top(index: usize, count: usize, height: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    index.min(count - 1) as f32 * slot_h(count, height)
}

/// The gutter this rail has to live in: the clear space between the well's
/// left edge and the reading column, which is [`Shell::column`]'s pad plus
/// half of whatever the `CONTENT_MAX_W` cap leaves over.
///
/// [`Shell::column`]: crate::ui
pub fn gutter(well_w: f32) -> f32 {
    let inner = well_w - crate::theme::GUTTER * 2.0;
    crate::theme::GUTTER + ((inner - crate::theme::CONTENT_MAX_W) / 2.0).max(0.0)
}

/// Whether the rail is drawn at all.
///
/// Three conditions, and each is a different reference's scar. The width
/// gate is t3code's and comet's (comet hides the rail below a 48rem
/// container outright); the turn count is waku's; the scrollable check is
/// waku's too — a transcript that fits on screen needs no map of itself.
pub fn shows(scrollable: bool, turn_count: usize, well_w: f32) -> bool {
    scrollable && turn_count >= MIN_TURNS && gutter(well_w) >= RAIL_LEFT + RAIL_W + RAIL_GAP
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::model::{Delivery, Transcript};

    fn user(text: &str) -> Row {
        Row::User {
            text: text.into(),
            delivery: Delivery::Delivered,
            provider_msg_id: None,
        }
    }

    fn assistant(text: &str) -> Row {
        Row::Assistant {
            text: text.into(),
            live: false,
        }
    }

    fn span(start_row: usize, ended: bool) -> TurnSpan {
        TurnSpan {
            start_row,
            started_ms: 1_000,
            ended_ms: ended.then_some(2_000),
            checkpoint_seq: None,
        }
    }

    /// The ordinary case, and the one that proves the tick's two indices
    /// are genuinely different numbers: an assistant message explodes into
    /// one display row per markdown block, so the second turn's display
    /// row has already drifted from its transcript row.
    #[test]
    fn a_tick_carries_the_display_row_and_not_the_transcript_row() {
        let mut t = Transcript::new();
        t.rows = vec![
            user("first prompt"),
            assistant("one\n\ntwo\n\nthree"),
            user("second prompt"),
        ];
        t.revs = vec![0; t.rows.len()];
        let mut d = DisplayList::new();
        d.update(&t);
        let ticks = ticks(&t.rows, &[span(0, true), span(2, true)], &d);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].start_row, 0);
        assert_eq!(ticks[0].display_row, 0);
        assert_eq!(ticks[1].start_row, 2);
        assert!(
            ticks[1].display_row > 2,
            "three markdown blocks should have pushed the second prompt \
             down the display list: {}",
            ticks[1].display_row
        );
    }

    /// The walk must land on the same row `assert_scroll` will, or a click
    /// lights one tick and scrolls to another. This pins the two together
    /// rather than trusting that they agree.
    #[test]
    fn every_tick_agrees_with_the_scroll_pin_it_will_hand_over() {
        let mut t = Transcript::new();
        t.rows = vec![
            user("a"),
            assistant("one\n\ntwo"),
            user("b"),
            assistant("three"),
            user("c"),
            assistant("four\n\nfive\n\nsix"),
        ];
        t.revs = vec![0; t.rows.len()];
        let mut d = DisplayList::new();
        d.update(&t);
        let ticks = ticks(&t.rows, &[span(0, true), span(2, true), span(4, true)], &d);
        assert_eq!(ticks.len(), 3);
        for tick in &ticks {
            assert_eq!(
                Some(tick.display_row),
                d.first_row_of_msg(tick.start_row as u32),
                "tick for row {} disagrees with the pin",
                tick.start_row
            );
        }
    }

    /// waku's quadratic scar, pinned structurally rather than timed: the
    /// cursor never restarts, so the walk is linear. A `position` scan per
    /// turn would still pass every other test in this file.
    #[test]
    fn the_walk_is_one_pass_and_not_a_scan_per_turn() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/turn_rail.rs"))
            .expect("read own source");
        let body = src
            .split("pub fn ticks(")
            .nth(1)
            .and_then(|s| s.split("\nfn row_text").next())
            .expect("locate ticks body");
        assert!(
            !body.contains("first_row_of_msg"),
            "ticks() must not call first_row_of_msg — it is a scan from \
             the top, and calling it per turn is waku's quadratic bug"
        );
        assert!(
            body.contains("while cursor <"),
            "the single cursor walk is what makes ticks() linear"
        );
    }

    #[test]
    fn a_folded_turn_still_gets_a_tick_at_its_prompt() {
        let mut t = Transcript::new();
        t.rows = vec![
            user("please"),
            Row::Marker { text: "one".into() },
            Row::Marker { text: "two".into() },
            Row::Marker {
                text: "three".into(),
            },
            assistant("done"),
            user("again"),
        ];
        t.revs = vec![0; t.rows.len()];
        t.turns = vec![span(0, true), span(5, false)];
        let mut d = DisplayList::new();
        let fold = crate::display::FoldInput {
            turns: &t.turns,
            expanded: &[],
        };
        d.update_with(&t, &fold);
        let turns = t.turns.clone();
        let ticks = ticks(&t.rows, &turns, &d);
        assert_eq!(ticks.len(), 2, "a fold must not cost a turn its tick");
        assert_eq!(ticks[0].start_row, 0);
        assert_eq!(ticks[0].prompt, "please");
        assert_eq!(
            Some(ticks[0].display_row),
            d.first_row_of_msg(0),
            "the fold row shares its turn's msg, so the pin must still \
             resolve to the prompt"
        );
    }

    #[test]
    fn the_open_turn_has_a_tick_too() {
        let mut t = Transcript::new();
        t.rows = vec![user("settled"), assistant("ok"), user("still running")];
        t.revs = vec![0; t.rows.len()];
        let mut d = DisplayList::new();
        d.update(&t);
        let ticks = ticks(&t.rows, &[span(0, true), span(2, false)], &d);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[1].prompt, "still running");
    }

    // ---------------------------------------------------------------
    // Which tick is lit

    #[test]
    fn the_active_tick_is_the_one_the_reader_is_inside() {
        let rows = [0usize, 7, 15];
        assert_eq!(active(&rows, 0, false), Some(0));
        assert_eq!(active(&rows, 6, false), Some(0));
        assert_eq!(active(&rows, 7, false), Some(1));
        assert_eq!(active(&rows, 14, false), Some(1));
        assert_eq!(active(&rows, 15, false), Some(2));
        assert_eq!(active(&rows, 900, false), Some(2));
    }

    /// Above the first prompt — the session header, a replayed banner —
    /// the first turn is still the one being read into.
    #[test]
    fn above_the_first_prompt_the_first_tick_is_active() {
        assert_eq!(active(&[3, 9], 0, false), Some(0));
        assert_eq!(active(&[3, 9], 2, false), Some(0));
    }

    /// The position this list spends most of its life in. A glued
    /// bottom anchor does not report the newest turn's row, so without
    /// the short-circuit the rail lights the wrong tick at rest.
    #[test]
    fn parked_at_the_end_the_newest_turn_is_active() {
        assert_eq!(active(&[0, 7, 15], 0, true), Some(2));
    }

    #[test]
    fn no_turns_means_no_active_tick() {
        assert_eq!(active(&[], 4, false), None);
        assert_eq!(active(&[], 4, true), None);
    }

    // ---------------------------------------------------------------
    // Geometry

    #[test]
    fn a_short_rail_uses_full_spacing() {
        let h = rail_height(4, 600.0);
        assert_eq!(h, 4.0 * SLOT);
        assert_eq!(slot_h(4, h), SLOT);
        assert_eq!(slot_top(0, 4, h), 0.0);
        assert_eq!(slot_top(3, 4, h), 3.0 * SLOT);
    }

    #[test]
    fn a_long_rail_compresses_instead_of_dropping_turns() {
        let viewport = 600.0;
        let count = 200;
        let h = rail_height(count, viewport);
        assert_eq!(h, viewport * VIEWPORT_RATIO);
        let slot = slot_h(count, h);
        assert!(slot < SLOT, "200 turns in 432 px must compress: {slot}");
        // Every turn still has a slot, and the slots still tile exactly.
        assert!((slot_top(count - 1, count, h) + slot - h).abs() < 0.01);
    }

    /// The property the whole density argument rests on. If slots ever
    /// overlapped, the tick painted last would eat its neighbour's clicks
    /// — `debug_bounds` resolves a selector to the LAST-painted element,
    /// so the test suite would go green over an unreachable tick.
    #[test]
    fn slots_never_overlap_at_any_density() {
        for count in [1usize, 2, 7, 40, 41, 200, 1000] {
            let h = rail_height(count, 600.0);
            let slot = slot_h(count, h);
            for ix in 1..count {
                let prev_bottom = slot_top(ix - 1, count, h) + slot;
                let top = slot_top(ix, count, h);
                assert!(
                    top + 1e-3 >= prev_bottom,
                    "count={count} tick {ix} starts at {top}, inside the \
                     previous slot ending at {prev_bottom}"
                );
            }
        }
    }

    #[test]
    fn an_unlaid_out_viewport_yields_no_rail() {
        assert_eq!(rail_height(5, 0.0), 0.0);
        assert_eq!(rail_height(0, 600.0), 0.0);
    }

    // ---------------------------------------------------------------
    // When it is drawn at all

    /// The three refusals, each on its own, so a regression names which
    /// gate it broke.
    #[test]
    fn the_rail_refuses_three_ways() {
        let wide = crate::theme::CONTENT_MAX_W + 400.0;
        assert!(shows(true, 2, wide));
        assert!(
            !shows(false, 9, wide),
            "a transcript that fits needs no map"
        );
        assert!(!shows(true, 1, wide), "one turn is not a map");
        assert!(
            !shows(true, 9, crate::theme::CONTENT_MAX_W + 40.0),
            "a strip must never reach into the reading column"
        );
    }

    /// t3code's hazard, as arithmetic: the strip's right edge plus its
    /// clearance may never pass the content's left edge, at any width.
    #[test]
    fn the_strip_never_reaches_the_reading_column() {
        let mut w = 200.0f32;
        while w < 3000.0 {
            if shows(true, 5, w) {
                assert!(
                    RAIL_LEFT + RAIL_W + RAIL_GAP <= gutter(w),
                    "at well width {w} the strip overlaps the text"
                );
            }
            w += 7.0;
        }
    }

    /// Below the cap the column is not centered at all — it is whatever
    /// the pad leaves — so the gutter is the pad and nothing more.
    #[test]
    fn a_narrow_well_has_only_its_padding_for_a_gutter() {
        assert_eq!(gutter(400.0), crate::theme::GUTTER);
        assert_eq!(
            gutter(crate::theme::CONTENT_MAX_W + crate::theme::GUTTER * 2.0),
            crate::theme::GUTTER
        );
    }

    // ---------------------------------------------------------------
    // The label

    #[test]
    fn a_prompt_label_is_one_line_and_says_when_it_was_cut() {
        assert_eq!(
            snippet("  hello   there \n friend ", 100),
            "hello there friend"
        );
        assert_eq!(snippet("abcdef", 3), "abc…");
        assert_eq!(snippet("abc", 3), "abc", "an exact fit is not truncated");
        assert_eq!(snippet("", 10), "");
    }

    /// A `chars` cut splits this family emoji into a broken glyph, and
    /// the label is the only thing telling a reader which turn a 2 px
    /// mark belongs to.
    #[test]
    fn the_cut_lands_on_a_grapheme_cluster() {
        let s = snippet("👨‍👩‍👧‍👦x", 1);
        assert_eq!(s, "👨‍👩‍👧‍👦…");
    }
}
