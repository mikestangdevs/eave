//! Slice CV1 — **Canvas mode**, the pure half.
//!
//! Columns (CL1) laid every open row side by side at a fixed pitch on one
//! horizontal axis. Canvas is that strip with the two constraints lifted:
//! a card carries its own RECT on an unbounded plane, and the plane pans
//! on both axes. Nothing else changes — the same [`ColumnId`]s, the same
//! order out of the sidebar, the same one-selection rule.
//!
//! The continuity is deliberate and is what makes the pivot cheap: a
//! canvas whose cards have never been moved is laid out by [`grid_slot`],
//! which puts them in the sidebar's order at [`CARD_W`] — CL1's own
//! `COLUMN_W` — and one viewport tall. So **an untouched canvas at rest
//! is the strip**, and every difference is something the operator did:
//! wrapped it (the grid flows into rows and scrolls down forever), moved
//! a card, or resized one.
//!
//! Three rules the plane lives by, all of them arithmetic here rather
//! than in a listener:
//!
//! - **A placement is the operator's, a slot is ours.** A card the
//!   operator has never touched has NO entry: it is placed by
//!   [`grid_slot`] from its index every frame, so opening a tab reflows
//!   the untouched cards around it. The first drag or resize writes an
//!   entry, and from then on that card is where it was left — reflow
//!   never moves it again. [`tidy`] is the one way back, and it is
//!   explicit because it discards work.
//! - **The plane is bounded by its content, not by the window.** There is
//!   no page count and no end-stop at a card's edge: [`content_bounds`]
//!   is the union of what is placed, and [`clamp_pan`] allows half a
//!   viewport of overscroll past it in every direction — enough to drag a
//!   card out into open space, never enough to lose the lot off-screen.
//! - **Overlap is allowed.** Cards may sit over one another; nothing
//!   resolves collisions. A canvas that shoves its neighbours when you
//!   drop a card is not a canvas, and the reference the operator gave
//!   overlaps freely.

use std::collections::HashMap;
use std::ops::Range;

use crate::columns::{ColumnId, COLUMN_GUTTER, COLUMN_W};

/// A card's default width — CL1's column width, so an untouched canvas
/// is the strip.
pub const CARD_W: f32 = COLUMN_W;
/// The gap the grid leaves between cards, and the plane's own margin.
pub const GUTTER: f32 = COLUMN_GUTTER;
/// The smallest a card may be resized to. Below this a chat's composer
/// and a page's header stop being usable, which is the floor that
/// matters — not a round number.
pub const CARD_W_MIN: f32 = 320.0;
pub const CARD_H_MIN: f32 = 200.0;
/// The largest. A card past this is a mode, not a card; the ⛶ control is
/// how you get a full-well view of one.
pub const CARD_W_MAX: f32 = 4000.0;
pub const CARD_H_MAX: f32 = 4000.0;
/// A grid card is one viewport tall, within these bounds — so a canvas
/// nobody has touched fills the well exactly the way the strip did.
pub const GRID_H_MIN: f32 = 360.0;
pub const GRID_H_MAX: f32 = 900.0;
/// A dropped card lands on this lattice, so cards placed by hand still
/// line up with each other and with the grid. Small enough that the drag
/// reads as free.
pub const SNAP: f32 = 12.0;
/// How wide the resize band inside a card's right and bottom edges is.
pub const GRIP_BAND: f32 = 7.0;
/// How long a programmatic pan (tidy, a selection coming into view)
/// takes. Same curve and duration as the strip's glide, for the same
/// reason: a move the operator did not make has to be followed by eye.
pub const PAN_GLIDE_MS: u64 = 260;

/// A card's rectangle in canvas points. Not a `gpui::Bounds`: this module
/// is the pure half and holds no frame types, the same rule `columns.rs`
/// keeps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl CardRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Half-open on both axes, so two cards that share an edge do not
    /// count as overlapping — which is what the grid produces.
    pub fn intersects(&self, other: &CardRect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn inflate(&self, d: f32) -> CardRect {
        CardRect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    pub fn union(&self, other: &CardRect) -> CardRect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        CardRect::new(x, y, r - x, b - y)
    }

    /// A rect with every field finite and both extents at least the
    /// minimum. A saved rect that is not this is discarded, not repaired
    /// in place: a `NaN` origin would put a card nowhere and a zero
    /// height would make it unclickable, and in both cases the grid slot
    /// is a better answer than a rescued number.
    pub fn is_sane(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.w >= CARD_W_MIN
            && self.h >= CARD_H_MIN
            && self.w <= CARD_W_MAX
            && self.h <= CARD_H_MAX
    }
}

/// What the operator has moved or resized, by [`ColumnId::stem`]. A card
/// with no entry is placed by the grid; see the module note.
pub type Placements = HashMap<String, CardRect>;

/// How many cards the grid puts in a row at this viewport width. At
/// least one, always — a window narrower than a card still shows it,
/// clipped, rather than showing nothing.
pub fn grid_cols(view_w: f32) -> usize {
    if !view_w.is_finite() || view_w <= 0.0 {
        return 1;
    }
    let usable = view_w - GUTTER;
    let per = CARD_W + GUTTER;
    ((usable / per).floor() as i64).max(1) as usize
}

/// A grid card's height: the viewport, less the margins, within bounds.
pub fn grid_h(view_h: f32) -> f32 {
    if !view_h.is_finite() || view_h <= 0.0 {
        return GRID_H_MIN;
    }
    (view_h - 2.0 * GUTTER).clamp(GRID_H_MIN, GRID_H_MAX)
}

/// Where the `ix`th untouched card sits. Row-major, wrapping at
/// [`grid_cols`] — so one row at a wide window is CL1's strip, and a
/// narrow window wraps into a grid that scrolls down.
pub fn grid_slot(ix: usize, view_w: f32, view_h: f32) -> CardRect {
    let cols = grid_cols(view_w);
    let h = grid_h(view_h);
    let col = ix % cols;
    let row = ix / cols;
    CardRect::new(
        GUTTER + col as f32 * (CARD_W + GUTTER),
        GUTTER + row as f32 * (h + GUTTER),
        CARD_W,
        h,
    )
}

/// Every card's rect this frame: the operator's placement where there is
/// one, the grid slot where there is not.
///
/// The grid index counts only the UNPLACED cards, so moving one card out
/// of the way does not leave a hole in the grid behind it — the rest
/// close up. That is the behaviour a reflow has to have for the grid to
/// stay a grid while cards are being pulled out of it.
pub fn layout(
    order: &[ColumnId],
    placed: &Placements,
    view_w: f32,
    view_h: f32,
) -> Vec<CardRect> {
    let mut out = Vec::with_capacity(order.len());
    let mut slot = 0usize;
    for col in order {
        match placed.get(&col.stem()).filter(|r| r.is_sane()) {
            Some(r) => out.push(*r),
            None => {
                out.push(grid_slot(slot, view_w, view_h));
                slot += 1;
            }
        }
    }
    out
}

/// Re-flow everything into the grid: the one way back from a canvas the
/// operator has rearranged. Returns the placements to KEEP, which is
/// none of them — tidy clears the map rather than writing every card's
/// grid slot into it, so a later window resize reflows to the new width
/// instead of freezing yesterday's columns.
pub fn tidy(placed: &mut Placements) {
    placed.clear();
}

/// The plane's extent: everything placed, plus the plane's own margin.
/// Empty gives a viewport-sized box at the origin so the pan clamp has
/// something to work with on an empty canvas.
pub fn content_bounds(rects: &[CardRect], view_w: f32, view_h: f32) -> CardRect {
    let mut it = rects.iter();
    let Some(first) = it.next() else {
        return CardRect::new(0.0, 0.0, view_w.max(1.0), view_h.max(1.0));
    };
    let mut b = *first;
    for r in it {
        b = b.union(r);
    }
    b.inflate(GUTTER)
}

/// How far past its content the plane may be panned: half a viewport in
/// each direction. Enough to drag a card into open space and to see that
/// you have reached the edge; never enough to lose the content entirely.
fn overscroll(view: f32) -> f32 {
    (view / 2.0).max(GUTTER)
}

/// Hold the pan inside the content plus its overscroll. When the content
/// is smaller than the viewport on an axis, that axis pins to the
/// content's own origin — a canvas with two cards on it does not slide
/// around under the wheel.
pub fn clamp_pan(pan_x: f32, pan_y: f32, content: &CardRect, view_w: f32, view_h: f32) -> (f32, f32) {
    let axis = |pan: f32, lo: f32, size: f32, view: f32| -> f32 {
        if !pan.is_finite() {
            return lo;
        }
        let slack = overscroll(view);
        if size <= view {
            // Smaller than the window: the content's origin, less the
            // slack that centres nothing and surprises no one.
            return pan.clamp(lo - slack, lo + slack);
        }
        pan.clamp(lo - slack, lo + size - view + slack)
    };
    (
        axis(pan_x, content.x, content.w, view_w),
        axis(pan_y, content.y, content.h, view_h),
    )
}

/// The cards whose rect meets the viewport — the ones a page may
/// composite in.
pub fn visible(rects: &[CardRect], pan_x: f32, pan_y: f32, view_w: f32, view_h: f32) -> Vec<usize> {
    if view_w <= 0.0 || view_h <= 0.0 {
        return Vec::new();
    }
    let view = CardRect::new(pan_x, pan_y, view_w, view_h);
    rects
        .iter()
        .enumerate()
        .filter(|(_, r)| r.intersects(&view))
        .map(|(i, _)| i)
        .collect()
}

/// The cards that are laid out and painted: the visible ones, plus
/// anything within one card of the viewport, so a pan of less than a card
/// reveals content that is already mounted rather than a slot filling in.
/// CL1's `mounted_range` in two dimensions.
pub fn mounted(rects: &[CardRect], pan_x: f32, pan_y: f32, view_w: f32, view_h: f32) -> Vec<usize> {
    if view_w <= 0.0 || view_h <= 0.0 {
        return Vec::new();
    }
    let view = CardRect::new(pan_x, pan_y, view_w, view_h).inflate(CARD_W);
    rects
        .iter()
        .enumerate()
        .filter(|(_, r)| r.intersects(&view))
        .map(|(i, _)| i)
        .collect()
}

/// The card under a canvas point, topmost first. Later cards paint over
/// earlier ones, so the hit walks backwards — the same rule the paint
/// uses, stated once.
pub fn hit(rects: &[CardRect], x: f32, y: f32) -> Option<usize> {
    rects.iter().enumerate().rev().find(|(_, r)| r.contains(x, y)).map(|(i, _)| i)
}

/// The minimal pan that brings a card whole into view. Like CL1's
/// `scroll_into_view` it is MINIMAL and never re-centres, so a selection
/// change cannot start a fight with the gesture that derives the
/// selection from the pan. A card larger than the viewport aligns to its
/// top-left, which is where its header is.
pub fn pan_into_view(
    pan_x: f32,
    pan_y: f32,
    rect: &CardRect,
    view_w: f32,
    view_h: f32,
) -> (f32, f32) {
    let axis = |pan: f32, lo: f32, size: f32, view: f32| -> f32 {
        if size >= view || lo < pan {
            lo - GUTTER
        } else if lo + size > pan + view {
            lo + size - view + GUTTER
        } else {
            pan
        }
    };
    (
        axis(pan_x, rect.x, rect.w, view_w),
        axis(pan_y, rect.y, rect.h, view_h),
    )
}

/// Which part of a card a press at this canvas point has taken hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grip {
    /// The right edge — width only.
    Right,
    /// The bottom edge — height only.
    Bottom,
    /// The bottom-right corner — both.
    Corner,
}

/// The resize grip under a point, if any. Only the right, bottom and
/// bottom-right are live: a card is anchored at its top-left, so
/// resizing from the other two edges would move it as well, and a drag
/// that both moves and resizes is a gesture nobody can aim. The header
/// is the move handle and it is a listener, not a hit test.
pub fn grip_at(rect: &CardRect, x: f32, y: f32) -> Option<Grip> {
    if !rect.inflate(GRIP_BAND).contains(x, y) {
        return None;
    }
    let near_r = x >= rect.right() - GRIP_BAND;
    let near_b = y >= rect.bottom() - GRIP_BAND;
    match (near_r, near_b) {
        (true, true) => Some(Grip::Corner),
        (true, false) => Some(Grip::Right),
        (false, true) => Some(Grip::Bottom),
        (false, false) => None,
    }
}

/// Snap a value to the lattice. Applied on DROP, never during the drag:
/// a card that jumps between lattice points under the pointer does not
/// read as being carried.
pub fn snap_to(v: f32) -> f32 {
    (v / SNAP).round() * SNAP
}

/// Where a card under the pointer sits MID-drag: exactly where the hand
/// put it. Unsnapped, because a card that jumps between lattice points
/// while it is being carried does not read as being carried.
pub fn moved_free(start: &CardRect, dx: f32, dy: f32) -> CardRect {
    CardRect::new(start.x + dx, start.y + dy, start.w, start.h)
}

/// Where a moved card lands: the origin the pointer put it at, snapped.
/// The size is untouched — a move is a move.
pub fn moved(start: &CardRect, dx: f32, dy: f32) -> CardRect {
    let r = moved_free(start, dx, dy);
    CardRect::new(snap_to(r.x), snap_to(r.y), r.w, r.h)
}

/// A card's size MID-resize: the grip's axes only, clamped to the size
/// bounds, unsnapped. The origin never moves, which is what makes the
/// gesture aimable.
pub fn resized_free(start: &CardRect, grip: Grip, dx: f32, dy: f32) -> CardRect {
    let w = match grip {
        Grip::Right | Grip::Corner => (start.w + dx).clamp(CARD_W_MIN, CARD_W_MAX),
        Grip::Bottom => start.w,
    };
    let h = match grip {
        Grip::Bottom | Grip::Corner => (start.h + dy).clamp(CARD_H_MIN, CARD_H_MAX),
        Grip::Right => start.h,
    };
    CardRect::new(start.x, start.y, w, h)
}

/// Where a resized card lands: the same axes, snapped to the lattice and
/// THEN clamped — that order, so a card dragged past its floor stops
/// exactly at the floor rather than at the first lattice point above it.
pub fn resized(start: &CardRect, grip: Grip, dx: f32, dy: f32) -> CardRect {
    let w = match grip {
        Grip::Right | Grip::Corner => snap_to(start.w + dx).clamp(CARD_W_MIN, CARD_W_MAX),
        Grip::Bottom => start.w,
    };
    let h = match grip {
        Grip::Bottom | Grip::Corner => snap_to(start.h + dy).clamp(CARD_H_MIN, CARD_H_MAX),
        Grip::Right => start.h,
    };
    CardRect::new(start.x, start.y, w, h)
}

/// The `frame.json` key holding each window's hand placements. Separate
/// from `columns::LAYOUT_KEY` so the two halves are written by the code
/// that owns them: the mode and the pan are one small `Copy` record, the
/// placements are a map that only exists once the operator has moved
/// something.
pub const CANVAS_KEY: &str = "canvas";

/// The pure half of the placement load. Anything that is not a sane rect
/// is dropped — that card falls back to its grid slot, which is always a
/// usable answer.
pub fn cards_from(doc: &serde_json::Value, window: &str) -> Placements {
    let mut out = Placements::new();
    let Some(entry) = doc.get(CANVAS_KEY).and_then(|v| v.get(window)).and_then(|v| v.as_object())
    else {
        return out;
    };
    for (stem, v) in entry {
        let Some(a) = v.as_array() else { continue };
        if a.len() != 4 {
            continue;
        }
        let mut n = [0f32; 4];
        let mut ok = true;
        for (i, x) in a.iter().enumerate() {
            match x.as_f64() {
                Some(f) => n[i] = f as f32,
                None => ok = false,
            }
        }
        let r = CardRect::new(n[0], n[1], n[2], n[3]);
        if ok && r.is_sane() {
            out.insert(stem.clone(), r);
        }
    }
    out
}

/// The pure half of the placement save: this window's entry written,
/// every other key and window kept. A window with nothing placed has its
/// entry REMOVED, so a canvas that has only ever been tidied leaves no
/// trace in the file and reflows to whatever width it is next opened at.
pub fn put_cards(doc: &mut serde_json::Value, window: &str, placed: &Placements) {
    if !doc.get(CANVAS_KEY).is_some_and(|v| v.is_object()) {
        doc[CANVAS_KEY] = serde_json::Value::Object(Default::default());
    }
    let Some(map) = doc[CANVAS_KEY].as_object_mut() else {
        return;
    };
    if placed.is_empty() {
        map.remove(window);
        return;
    }
    let mut entry = serde_json::Map::new();
    let mut stems: Vec<&String> = placed.keys().collect();
    stems.sort();
    for stem in stems {
        let r = placed[stem];
        entry.insert(stem.clone(), serde_json::json!([r.x, r.y, r.w, r.h]));
    }
    map.insert(window.to_string(), serde_json::Value::Object(entry));
}

/// Drop the placements of cards that are no longer open, so the file
/// does not grow one entry per tab that ever existed. Called where the
/// column feeds are pruned — the same tabs fact, the same observer.
pub fn prune(placed: &mut Placements, order: &[ColumnId]) {
    if placed.is_empty() {
        return;
    }
    let live: std::collections::HashSet<String> = order.iter().map(|c| c.stem()).collect();
    placed.retain(|stem, _| live.contains(stem));
}

/// The index range a `Vec<usize>` of card indices spans, for the callers
/// that still want CL1's `Range` shape. Empty in, empty out.
pub fn span(ixs: &[usize]) -> Range<usize> {
    match (ixs.first(), ixs.last()) {
        (Some(a), Some(b)) => *a..(*b + 1),
        _ => 0..0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str) -> ColumnId {
        ColumnId::Tab(id.into())
    }

    /// The continuity claim the whole slice rests on: a canvas nobody has
    /// touched, at a window wide enough for the strip, IS the strip —
    /// same width, same pitch, same order.
    #[test]
    fn an_untouched_canvas_at_a_wide_window_is_the_strip() {
        let order = vec![tab("a"), tab("b"), tab("c")];
        let placed = Placements::new();
        // Three columns' worth of width, so nothing wraps.
        let view_w = 3.0 * (CARD_W + GUTTER) + GUTTER;
        let rects = layout(&order, &placed, view_w, 800.0);
        assert_eq!(grid_cols(view_w), 3);
        for (i, r) in rects.iter().enumerate() {
            assert_eq!(r.w, CARD_W, "card {i} is a column wide");
            assert_eq!(r.y, GUTTER, "one row: {r:?}");
            assert_eq!(r.x, GUTTER + i as f32 * (CARD_W + GUTTER));
        }
        // The pitch is CL1's.
        assert_eq!(rects[1].x - rects[0].x, crate::columns::COLUMN_PITCH);
    }

    #[test]
    fn a_narrow_window_wraps_the_grid_and_it_scrolls_down() {
        let order: Vec<ColumnId> = (0..5).map(|i| tab(&format!("t{i}"))).collect();
        let placed = Placements::new();
        // Room for two across.
        let view_w = 2.0 * (CARD_W + GUTTER) + GUTTER;
        assert_eq!(grid_cols(view_w), 2);
        let rects = layout(&order, &placed, view_w, 800.0);
        assert_eq!(rects[0].y, rects[1].y, "first row");
        assert!(rects[2].y > rects[0].y, "the third card wrapped");
        assert_eq!(rects[2].x, rects[0].x, "and started a new row at the left");
        assert_eq!(rects[3].y, rects[2].y, "the second row holds two as well");
        assert!(rects[4].y > rects[2].y, "five cards over two columns make three rows");
        // A window too narrow for even one card still shows one.
        assert_eq!(grid_cols(10.0), 1);
        assert_eq!(grid_cols(0.0), 1);
        assert_eq!(grid_cols(f32::NAN), 1);
    }

    /// Moving a card out of the grid closes the gap behind it — the rest
    /// of the grid is still a grid.
    #[test]
    fn the_grid_closes_up_behind_a_card_the_operator_moved() {
        let order = vec![tab("a"), tab("b"), tab("c")];
        let view_w = 3.0 * (CARD_W + GUTTER) + GUTTER;
        let mut placed = Placements::new();
        placed.insert("b".into(), CardRect::new(2000.0, 1400.0, CARD_W, 400.0));
        let rects = layout(&order, &placed, view_w, 800.0);
        assert_eq!(rects[1].x, 2000.0, "b is where it was left");
        assert_eq!(rects[0].x, GUTTER, "a keeps the first slot");
        assert_eq!(rects[2].x, GUTTER + CARD_W + GUTTER, "c moved up into the second");
        // And tidy is the way back.
        tidy(&mut placed);
        let rects = layout(&order, &placed, view_w, 800.0);
        assert_eq!(rects[1].x, GUTTER + CARD_W + GUTTER);
        assert!(placed.is_empty(), "tidy clears rather than writing slots");
    }

    #[test]
    fn an_insane_saved_rect_falls_back_to_the_grid() {
        let order = vec![tab("a")];
        let view_w = 2000.0;
        for bad in [
            CardRect::new(f32::NAN, 0.0, CARD_W, 400.0),
            CardRect::new(0.0, f32::INFINITY, CARD_W, 400.0),
            CardRect::new(0.0, 0.0, 10.0, 400.0),
            CardRect::new(0.0, 0.0, CARD_W, 1.0),
            CardRect::new(0.0, 0.0, 9e9, 400.0),
        ] {
            let mut placed = Placements::new();
            placed.insert("a".into(), bad);
            let rects = layout(&order, &placed, view_w, 800.0);
            assert_eq!(rects[0], grid_slot(0, view_w, 800.0), "{bad:?} was not rescued");
        }
    }

    #[test]
    fn the_pan_is_held_inside_the_content_plus_half_a_viewport() {
        let rects = vec![
            CardRect::new(0.0, 0.0, 600.0, 400.0),
            CardRect::new(3000.0, 2000.0, 600.0, 400.0),
        ];
        let (vw, vh) = (1000.0, 700.0);
        let b = content_bounds(&rects, vw, vh);
        assert_eq!(b.x, -GUTTER);
        assert_eq!(b.right(), 3600.0 + GUTTER);
        // Far past the right edge: clamped to content end + half a view.
        let (px, _) = clamp_pan(99999.0, 0.0, &b, vw, vh);
        assert_eq!(px, b.x + b.w - vw + vw / 2.0);
        // Far past the left: content origin less half a view.
        let (px, py) = clamp_pan(-99999.0, -99999.0, &b, vw, vh);
        assert_eq!(px, b.x - vw / 2.0);
        assert_eq!(py, b.y - vh / 2.0);
        // NaN reads as the origin rather than poisoning the plane.
        let (px, py) = clamp_pan(f32::NAN, f32::NAN, &b, vw, vh);
        assert_eq!((px, py), (b.x, b.y));
    }

    /// Content smaller than the window does not slide about: the axis
    /// pins to the content's origin, give or take the overscroll.
    #[test]
    fn a_canvas_smaller_than_its_window_stays_put() {
        let rects = vec![CardRect::new(0.0, 0.0, 400.0, 300.0)];
        let (vw, vh) = (2000.0, 1400.0);
        let b = content_bounds(&rects, vw, vh);
        assert!(b.w < vw && b.h < vh);
        let (px, py) = clamp_pan(5000.0, 5000.0, &b, vw, vh);
        assert_eq!(px, b.x + vw / 2.0);
        assert_eq!(py, b.y + vh / 2.0);
    }

    #[test]
    fn visible_is_what_meets_the_viewport_and_mounted_is_a_card_more() {
        let rects = vec![
            CardRect::new(0.0, 0.0, 500.0, 400.0),    // in
            CardRect::new(1200.0, 0.0, 500.0, 400.0), // one card to the right
            CardRect::new(9000.0, 0.0, 500.0, 400.0), // far away
        ];
        let v = visible(&rects, 0.0, 0.0, 1000.0, 700.0);
        assert_eq!(v, vec![0]);
        let m = mounted(&rects, 0.0, 0.0, 1000.0, 700.0);
        assert_eq!(m, vec![0, 1], "the neighbour is mounted, the far one is not");
        // No viewport yet: nothing at all, and no panic.
        assert!(visible(&rects, 0.0, 0.0, 0.0, 0.0).is_empty());
        assert!(mounted(&rects, 0.0, 0.0, 0.0, 0.0).is_empty());
        assert_eq!(span(&m), 0..2);
        assert_eq!(span(&[]), 0..0);
    }

    /// Later cards paint over earlier ones, so the hit test walks back.
    #[test]
    fn the_topmost_card_takes_the_point() {
        let rects = vec![
            CardRect::new(0.0, 0.0, 500.0, 400.0),
            CardRect::new(100.0, 100.0, 500.0, 400.0),
        ];
        assert_eq!(hit(&rects, 200.0, 200.0), Some(1));
        assert_eq!(hit(&rects, 20.0, 20.0), Some(0));
        assert_eq!(hit(&rects, -5.0, 20.0), None);
        assert_eq!(hit(&[], 0.0, 0.0), None);
    }

    #[test]
    fn only_the_right_bottom_and_corner_are_grips() {
        let r = CardRect::new(100.0, 100.0, 500.0, 400.0);
        assert_eq!(grip_at(&r, r.right() - 2.0, r.bottom() - 2.0), Some(Grip::Corner));
        assert_eq!(grip_at(&r, r.right() - 2.0, r.y + 200.0), Some(Grip::Right));
        assert_eq!(grip_at(&r, r.x + 200.0, r.bottom() - 2.0), Some(Grip::Bottom));
        assert_eq!(grip_at(&r, r.x + 200.0, r.y + 200.0), None, "the body is not a grip");
        assert_eq!(grip_at(&r, r.x + 2.0, r.y + 2.0), None, "the top-left anchor is not a grip");
        assert_eq!(grip_at(&r, 9000.0, 9000.0), None);
        // The band reaches a little OUTSIDE the card too, so the edge is
        // catchable without landing on the neighbour.
        assert_eq!(grip_at(&r, r.right() + 3.0, r.y + 200.0), Some(Grip::Right));
    }

    #[test]
    fn a_move_snaps_on_drop_and_keeps_the_size() {
        let r = CardRect::new(100.0, 100.0, 500.0, 400.0);
        let m = moved(&r, 37.0, -5.0);
        assert_eq!(m.w, 500.0);
        assert_eq!(m.h, 400.0);
        assert_eq!(m.x % SNAP, 0.0);
        assert_eq!(m.y % SNAP, 0.0);
        assert!((m.x - 137.0).abs() <= SNAP / 2.0);
    }

    #[test]
    fn a_resize_moves_only_its_own_axes_and_clamps() {
        let r = CardRect::new(100.0, 100.0, 500.0, 400.0);
        let right = resized(&r, Grip::Right, 120.0, 900.0);
        assert_eq!((right.x, right.y), (100.0, 100.0), "the origin never moves");
        assert_eq!(right.h, 400.0, "the bottom grip's axis is untouched");
        assert!(right.w > 500.0);
        let bottom = resized(&r, Grip::Bottom, 900.0, 60.0);
        assert_eq!(bottom.w, 500.0);
        assert!(bottom.h > 400.0);
        let corner = resized(&r, Grip::Corner, 60.0, 60.0);
        assert!(corner.w > 500.0 && corner.h > 400.0);
        // Dragged past the floor, a card stops at the floor.
        let tiny = resized(&r, Grip::Corner, -9000.0, -9000.0);
        assert_eq!((tiny.w, tiny.h), (CARD_W_MIN, CARD_H_MIN));
        let huge = resized(&r, Grip::Corner, 9e9, 9e9);
        assert_eq!((huge.w, huge.h), (CARD_W_MAX, CARD_H_MAX));
    }

    /// Minimal, like CL1's — a selection change may not re-centre, or it
    /// would re-derive a different selection from the pan it just wrote.
    #[test]
    fn pan_into_view_moves_the_least() {
        let (vw, vh) = (1000.0, 700.0);
        let inside = CardRect::new(100.0, 100.0, 500.0, 400.0);
        assert_eq!(pan_into_view(0.0, 0.0, &inside, vw, vh), (0.0, 0.0), "already whole");
        let right = CardRect::new(1400.0, 0.0, 500.0, 400.0);
        let (px, _) = pan_into_view(0.0, 0.0, &right, vw, vh);
        assert_eq!(px, 1400.0 + 500.0 - vw + GUTTER, "just enough to show its right edge");
        let left = CardRect::new(-800.0, 0.0, 500.0, 400.0);
        let (px, _) = pan_into_view(0.0, 0.0, &left, vw, vh);
        assert_eq!(px, -800.0 - GUTTER);
        // Bigger than the window: its top-left, where the header is.
        let big = CardRect::new(300.0, 300.0, 3000.0, 3000.0);
        assert_eq!(pan_into_view(0.0, 0.0, &big, vw, vh), (300.0 - GUTTER, 300.0 - GUTTER));
    }

    #[test]
    fn placements_round_trip_through_frame_json_and_keep_their_neighbours() {
        assert_eq!(CANVAS_KEY, "canvas");
        let mut doc = serde_json::json!({
            "splits": {"w1": []},
            "canvas": {"other": {"z": [1.0, 2.0, 400.0, 300.0]}},
        });
        let mut placed = Placements::new();
        placed.insert("a".into(), CardRect::new(12.0, 24.0, 560.0, 700.0));
        placed.insert("b".into(), CardRect::new(600.0, 24.0, 560.0, 700.0));
        put_cards(&mut doc, "w1", &placed);
        assert!(doc.get("splits").is_some(), "another key survived");
        assert!(doc[CANVAS_KEY].get("other").is_some(), "another window survived");
        let back = cards_from(&doc, "w1");
        assert_eq!(back, placed);
        // A window with nothing placed leaves no entry behind.
        put_cards(&mut doc, "w1", &Placements::new());
        assert!(doc[CANVAS_KEY].get("w1").is_none());
        assert!(cards_from(&doc, "w1").is_empty());
        assert!(cards_from(&serde_json::json!({}), "w1").is_empty());
    }

    #[test]
    fn a_saved_rect_that_is_not_a_rect_is_dropped_not_repaired() {
        let doc = serde_json::json!({
            "canvas": {"w1": {
                "ok":     [0.0, 0.0, 560.0, 400.0],
                "short":  [0.0, 0.0, 560.0],
                "wordy":  [0.0, 0.0, "560", 400.0],
                "tiny":   [0.0, 0.0, 4.0, 400.0],
                "notarr": 7,
            }},
        });
        let back = cards_from(&doc, "w1");
        assert_eq!(back.len(), 1);
        assert!(back.contains_key("ok"));
    }

    #[test]
    fn placements_are_pruned_to_what_is_open() {
        let mut placed = Placements::new();
        placed.insert("a".into(), CardRect::new(0.0, 0.0, 560.0, 400.0));
        placed.insert("gone".into(), CardRect::new(0.0, 0.0, 560.0, 400.0));
        prune(&mut placed, &[tab("a"), ColumnId::Diff("/repo".into())]);
        assert_eq!(placed.len(), 1);
        assert!(placed.contains_key("a"));
        // A diff row's stem is the one the placement map uses.
        placed.insert("diff-/repo".into(), CardRect::new(0.0, 0.0, 560.0, 400.0));
        prune(&mut placed, &[ColumnId::Diff("/repo".into())]);
        assert_eq!(placed.len(), 1);
        assert!(placed.contains_key("diff-/repo"));
    }
}
