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
/// A card's default height. **Not the viewport** — walked 2026-09-07 and
/// ruled on: *"a canvas is free form. not rigid."* Cards a whole window
/// tall are a stack of screens, not a canvas; a fixed, comfortable card
/// is what makes the plane read as a plane the moment it opens.
pub const CARD_H: f32 = 460.0;

/// The plane's zoom. 1.0 is one canvas point per window point.
pub const ZOOM_MIN: f32 = 0.3;
pub const ZOOM_MAX: f32 = 2.0;
/// One press of ⌘+ / ⌘−, as a ratio — zoom is multiplicative, so the
/// steps feel even at both ends.
pub const ZOOM_STEP: f32 = 1.2;
/// The ground's dot lattice, in canvas points. Ruled on in the same run:
/// *"there is no canvas dotted background. its just a black void."* The
/// dots are what make a pan legible — without them the plane moves and
/// nothing on screen says so.
pub const DOT_SPACING: f32 = 28.0;
pub const DOT_SIZE: f32 = 2.0;
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

/// A grid card's height: [`CARD_H`], or the viewport if that is shorter.
///
/// **It is NOT the viewport height.** The first cut made a card a whole
/// window tall — CL1's strip, carried over — and the walk of 2026-09-07
/// ruled on what that produces: *"a canvas is free form. not rigid."* A
/// row per screen is a stack, and the plane only reads as a plane when
/// more than one card is in front of you.
pub fn grid_h(view_h: f32) -> f32 {
    if !view_h.is_finite() || view_h <= 0.0 {
        return CARD_H;
    }
    CARD_H.min(view_h - 2.0 * GUTTER).max(CARD_H_MIN)
}

/// Where the `ix`th untouched card sits, of `n` untouched cards in all.
/// Row-major, wrapping at [`grid_cols`] — so one row at a wide window is
/// CL1's strip, and a narrow window wraps into a grid that scrolls down.
/// `n` is taken because the ROW HEIGHT depends on whether the grid wraps;
/// see [`grid_h`].
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

/// How much of the content must stay on screen, on each axis. **The plane
/// roams freely and still cannot be lost.**
///
/// A viewport of overscroll (the first answer to *"a canvas is free
/// form"*) let every card leave the screen: content shorter than the
/// window, panned by a whole window, is entirely outside it, and the
/// operator is then looking at an empty lattice with no way to tell which
/// way home is. Bounding the OVERLAP instead keeps both properties — you
/// can push the content into any corner, and a corner is as far as it
/// goes.
fn keep_on_screen(view: f32) -> f32 {
    (view / 3.0).clamp(GUTTER, 240.0)
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
        // Every axis roams — including one whose content is narrower than
        // the window, which is what *"full screen is fixed to up and down
        // scroll"* was about. What bounds it is the OVERLAP: the viewport
        // and the content must always share this much.
        let keep = keep_on_screen(view).min(size);
        // The viewport's left edge may reach the content's right edge less
        // `keep`; its right edge must stay past the content's left edge
        // plus `keep`.
        let hi = lo + size - keep;
        let lo_ = lo + keep - view;
        pan.clamp(lo_.min(hi), hi.max(lo_))
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
    // Inflated by a card: a page that stops compositing the moment its
    // card touches the viewport's edge blinks white as you pan past it.
    let view = CardRect::new(pan_x, pan_y, view_w, view_h).inflate(CARD_W);
    rects
        .iter()
        .enumerate()
        .filter(|(_, r)| r.intersects(&view))
        .map(|(i, _)| i)
        .collect()
}

/// The cards that are laid out and painted: **all of them**.
///
/// Ruled on 2026-09-07: *"windows are disapearing when i zoom in and not
/// rendering when i zoom out. everything should always be rendered in
/// view at all times."* CL1 virtualized because a STRIP is unbounded in
/// one direction and its columns are viewport-sized — mounting them all
/// was a real cost. A plane's cards are small and the operator arranges
/// them to be looked at together; a card that empties itself because the
/// zoom moved it a little outside a margin is the plane lying about what
/// is on it. The `pan`/`view` arguments are kept so the signature still
/// says what it is about, and so a future budget has somewhere to go.
pub fn mounted(rects: &[CardRect], _pan_x: f32, _pan_y: f32, _view_w: f32, _view_h: f32) -> Vec<usize> {
    (0..rects.len()).collect()
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

/// What a wheel delta does to the plane, in WINDOW points: the pan, or
/// nothing because the gesture belongs to a card.
///
/// **The rule is CLICKED IN, not hovered** — ruled on 2026-09-07: *"when i
/// hover over a window (not clicked in) i cant move the canvas. i can only
/// go side to side. i dont think hover should affect canvas movement."*
/// CL1's rule (a card owns the vertical wherever the pointer sits) is
/// right for a STRIP, where every column is a page you are reading. On a
/// plane the pointer is over a card most of the time simply because cards
/// cover the plane, and a gesture whose meaning changes under the cursor
/// cannot be aimed.
///
/// So the plane takes both axes everywhere except over the ONE card the
/// operator clicked into, which keeps its own vertical scroll because it
/// holds the transcript and the composer. Selection is a deliberate act;
/// hovering is not.
pub fn wheel_pan(dx: f32, dy: f32, over_selected: bool) -> Option<(f32, f32)> {
    if dx == 0.0 && dy == 0.0 {
        return None;
    }
    if over_selected {
        // The selected card's own scroll owns the vertical; a
        // horizontal-dominant delta still walks the plane.
        return crate::columns::strip_delta(dx, dy).map(|d| (d, 0.0));
    }
    Some((-dx, -dy))
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

pub fn clamp_zoom(z: f32) -> f32 {
    if !z.is_finite() {
        return 1.0;
    }
    z.clamp(ZOOM_MIN, ZOOM_MAX)
}

/// One step of the zoom, in or out.
pub fn zoom_step(zoom: f32, delta: i32) -> f32 {
    let z = if delta > 0 { zoom * ZOOM_STEP } else { zoom / ZOOM_STEP };
    clamp_zoom(z)
}

/// The canvas point under a VIEWPORT point at this pan and zoom.
pub fn to_canvas(pan_x: f32, pan_y: f32, zoom: f32, vx: f32, vy: f32) -> (f32, f32) {
    let z = clamp_zoom(zoom);
    (pan_x + vx / z, pan_y + vy / z)
}

/// The pan that keeps the canvas point under `(vx, vy)` fixed while the
/// zoom changes — so zooming with the pointer over a card holds that card
/// still, which is the only zoom that can be aimed.
pub fn zoom_about(
    pan_x: f32,
    pan_y: f32,
    from: f32,
    to: f32,
    vx: f32,
    vy: f32,
) -> (f32, f32) {
    let (cx_, cy_) = to_canvas(pan_x, pan_y, from, vx, vy);
    let to = clamp_zoom(to);
    (cx_ - vx / to, cy_ - vy / to)
}

/// The part of the plane a viewport covers, in CANVAS points — which is
/// the window's size divided by the zoom. Everything that reasons about
/// what is on screen goes through here, so the zoom is applied once.
pub fn viewport(pan_x: f32, pan_y: f32, view_w: f32, view_h: f32, zoom: f32) -> CardRect {
    let z = clamp_zoom(zoom);
    CardRect::new(pan_x, pan_y, (view_w / z).max(0.0), (view_h / z).max(0.0))
}

/// Where a card under the pointer sits, and where it lands: the same
/// answer. **There is no drop lattice** — ruled on 2026-09-07, *"a canvas
/// is free form. not rigid."* A 12 px snap was imperceptible while the
/// card was moving and still meant a card could not be put exactly where
/// it was let go, which is the one thing a canvas promises.
pub fn moved(start: &CardRect, dx: f32, dy: f32) -> CardRect {
    CardRect::new(start.x + dx, start.y + dy, start.w, start.h)
}

/// Kept as the name the drag path uses while a card is in hand.
pub fn moved_free(start: &CardRect, dx: f32, dy: f32) -> CardRect {
    moved(start, dx, dy)
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

/// Where a resized card lands — the same as under the hand, for
/// [`moved`]'s reason. Clamped to the size bounds and nothing else.
pub fn resized(start: &CardRect, grip: Grip, dx: f32, dy: f32) -> CardRect {
    resized_free(start, grip, dx, dy)
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
        // **A card is a CARD, not a screen** — the 2026-09-07 ruling,
        // *"a canvas is free form. not rigid."* The first cut made a grid
        // row a whole viewport tall, which put exactly one card in front
        // of the operator and read as a stack rather than a plane.
        let view_h = 800.0;
        assert_eq!(grid_h(view_h), CARD_H);
        assert!(CARD_H < view_h - 2.0 * GUTTER, "and it is smaller than the window");
        let rects = layout(&order, &placed, view_w, view_h);
        assert!(rects[2].y < view_h, "the second row starts on screen: {rects:?}");
        // A window shorter than a card shrinks it rather than clipping it.
        assert_eq!(grid_h(300.0), CARD_H_MIN.max(300.0 - 2.0 * GUTTER));
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

    /// The plane's reach on an axis that HAS somewhere to go: the far end
    /// is the content's end, less the overlap the clamp keeps on screen.
    #[test]
    fn the_pan_reaches_the_content_and_keeps_a_grip_on_it() {
        let rects = vec![
            CardRect::new(0.0, 0.0, 600.0, 400.0),
            CardRect::new(3000.0, 2000.0, 600.0, 400.0),
        ];
        let (vw, vh) = (1000.0, 700.0);
        let b = content_bounds(&rects, vw, vh);
        assert_eq!(b.x, -GUTTER);
        assert_eq!(b.right(), 3600.0 + GUTTER);
        assert!(b.w > vw && b.h > vh, "precondition: both axes can move");
        // Far past the right: the viewport's LEFT edge reaches the
        // content's right edge, less what stays on screen.
        let (px, _) = clamp_pan(99999.0, 0.0, &b, vw, vh);
        assert_eq!(px, b.x + b.w - keep_on_screen(vw));
        assert!(CardRect::new(px, 0.0, vw, vh).intersects(&b), "still holding on");
        // Far past the left: the viewport's RIGHT edge stays past the
        // content's left edge by the same amount.
        let (px, py) = clamp_pan(-99999.0, -99999.0, &b, vw, vh);
        assert_eq!(px, b.x + keep_on_screen(vw) - vw);
        assert_eq!(py, b.y + keep_on_screen(vh) - vh);
        // NaN reads as the origin rather than poisoning the plane.
        let (px, py) = clamp_pan(f32::NAN, f32::NAN, &b, vw, vh);
        assert_eq!((px, py), (b.x, b.y));
    }

    /// Content smaller than the window does not slide about: the axis
    /// pins to the content's origin, give or take the overscroll.
    /// **Every axis roams, and the content can never be lost.** Two
    /// rulings met at once. *"full screen is fixed to up and down scroll i
    /// cant move around at all"* — an earlier cut PINNED an axis whose
    /// content was narrower than the window, which on a wide window is the
    /// whole x axis. And the answer to that, a whole viewport of
    /// overscroll, let every card leave the screen: panned by a window,
    /// content shorter than the window is entirely outside it, and the
    /// operator is looking at an empty lattice. What is bounded is the
    /// OVERLAP.
    #[test]
    fn every_axis_roams_but_the_content_never_leaves_the_screen() {
        let rects = vec![CardRect::new(0.0, 0.0, 400.0, 300.0)];
        let (vw, vh) = (2000.0, 1400.0);
        let b = content_bounds(&rects, vw, vh);
        assert!(b.w < vw && b.h < vh, "precondition: smaller than the window");
        // Pushed as far as it goes in each direction, the content and the
        // viewport still meet.
        for (ax, ay) in [(-9e9, -9e9), (9e9, 9e9), (9e9, -9e9)] {
            let (px, py) = clamp_pan(ax, ay, &b, vw, vh);
            let view = CardRect::new(px, py, vw, vh);
            assert!(view.intersects(&b), "lost the content at ({ax}, {ay}): {view:?} vs {b:?}");
        }
        // It really does roam: a wide window's x axis is not pinned.
        let far = clamp_pan(9e9, 0.0, &b, vw, vh).0;
        assert!(far > b.x + 100.0, "x roams: {far}");
        // A small nudge is taken verbatim — the plane is not sticky.
        assert_eq!(clamp_pan(7.0, -3.0, &b, vw, vh), (7.0, -3.0));
        // And a TALL canvas keeps its ordinary scroll range.
        let tall = vec![CardRect::new(0.0, 0.0, 400.0, 9000.0)];
        let b = content_bounds(&tall, vw, vh);
        let (_, py) = clamp_pan(9e9, 9e9, &b, vw, vh);
        assert!(py > 9000.0 - vh, "the far end reaches the content's end: {py}");
        let view = CardRect::new(0.0, py, vw, vh);
        assert!(view.intersects(&b));
    }

    #[test]
    fn visible_is_what_meets_the_viewport_and_mounted_is_a_card_more() {
        let rects = vec![
            CardRect::new(0.0, 0.0, 500.0, 400.0),    // in
            CardRect::new(1200.0, 0.0, 500.0, 400.0), // one card to the right
            CardRect::new(9000.0, 0.0, 500.0, 400.0), // far away
        ];
        // Visible is the viewport plus a card, so a page does not blink
        // white as its card touches the edge.
        let v = visible(&rects, 0.0, 0.0, 1000.0, 700.0);
        assert_eq!(v, vec![0, 1]);
        assert!(!v.contains(&2), "the far one does not composite");
        // **Everything is mounted, always** — the 2026-09-07 ruling.
        assert_eq!(mounted(&rects, 0.0, 0.0, 1000.0, 700.0), vec![0, 1, 2]);
        assert_eq!(mounted(&rects, 9e9, 9e9, 1000.0, 700.0), vec![0, 1, 2]);
        // No viewport yet: nothing composites, and no panic.
        assert!(visible(&rects, 0.0, 0.0, 0.0, 0.0).is_empty());
        assert_eq!(span(&v), 0..2);
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

    /// **Hover does not decide whose gesture it is** — the 2026-09-07
    /// ruling. Only the card the operator has clicked INTO keeps the
    /// vertical; everywhere else, including over other cards, the plane
    /// takes both axes.
    #[test]
    fn only_the_card_you_clicked_into_keeps_the_wheel() {
        // Over the ground, or over a card that is merely hovered: the
        // plane moves on both axes.
        for over_selected in [false] {
            assert_eq!(wheel_pan(0.0, -30.0, over_selected), Some((0.0, 30.0)));
            assert_eq!(wheel_pan(-12.0, -30.0, over_selected), Some((12.0, 30.0)));
            assert_eq!(wheel_pan(-40.0, 2.0, over_selected), Some((40.0, -2.0)));
        }
        // Over the SELECTED card the vertical is the card's, and only a
        // horizontal-dominant delta reaches the plane.
        assert_eq!(wheel_pan(0.0, -30.0, true), None, "the card scrolls itself");
        assert_eq!(wheel_pan(-12.0, -30.0, true), None, "vertical-dominant is the card's");
        assert_eq!(wheel_pan(-40.0, 2.0, true), Some((40.0, 0.0)), "x still walks the plane");
        // Nothing is nothing, either way.
        assert_eq!(wheel_pan(0.0, 0.0, false), None);
        assert_eq!(wheel_pan(0.0, 0.0, true), None);
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

    /// **Free form, not rigid** — the 2026-09-07 ruling. A card lands
    /// EXACTLY where it was let go: no lattice, and the same answer under
    /// the hand as on the drop, so the card never jumps when released.
    #[test]
    fn a_move_lands_exactly_where_it_was_let_go() {
        let r = CardRect::new(100.0, 100.0, 500.0, 400.0);
        let m = moved(&r, 37.0, -5.0);
        assert_eq!((m.x, m.y), (137.0, 95.0), "no lattice");
        assert_eq!((m.w, m.h), (500.0, 400.0), "a move is a move");
        assert_eq!(moved_free(&r, 37.0, -5.0), m, "the drop does not move it again");
        let g = resized(&r, Grip::Corner, 13.0, 7.0);
        assert_eq!((g.w, g.h), (513.0, 407.0));
        assert_eq!(resized_free(&r, Grip::Corner, 13.0, 7.0), g);
    }

    /// The zoom is multiplicative, bounded, and holds the point under the
    /// pointer still — the only zoom that can be aimed.
    #[test]
    fn the_zoom_steps_evenly_and_holds_the_point_under_the_pointer() {
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        assert_eq!(clamp_zoom(99.0), ZOOM_MAX);
        assert_eq!(clamp_zoom(0.0), ZOOM_MIN);
        assert_eq!(zoom_step(1.0, 1), ZOOM_STEP);
        assert!(zoom_step(ZOOM_MAX, 1) == ZOOM_MAX, "clamps at the top");
        assert!(zoom_step(ZOOM_MIN, -1) == ZOOM_MIN, "and at the bottom");
        // The canvas point under the pointer before and after a zoom is
        // the same point.
        let (pan, focus) = ((120.0f32, 40.0f32), (300.0f32, 220.0f32));
        let before = to_canvas(pan.0, pan.1, 1.0, focus.0, focus.1);
        let to = zoom_step(1.0, 1);
        let next = zoom_about(pan.0, pan.1, 1.0, to, focus.0, focus.1);
        let after = to_canvas(next.0, next.1, to, focus.0, focus.1);
        assert!((before.0 - after.0).abs() < 0.01, "{before:?} vs {after:?}");
        assert!((before.1 - after.1).abs() < 0.01);
        // A zoomed-in window covers LESS of the plane.
        let vp = viewport(0.0, 0.0, 1000.0, 800.0, 2.0);
        assert_eq!((vp.w, vp.h), (500.0, 400.0));
        let vp = viewport(0.0, 0.0, 1000.0, 800.0, 0.5);
        assert_eq!((vp.w, vp.h), (2000.0, 1600.0));
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
