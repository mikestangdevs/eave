//! Slice CL1 — **Columns mode**, the pure half.
//!
//! The content area has two layouts. FOCUS is the well as it has always
//! been: one row of the sidebar, full width. COLUMNS replaces the well
//! with a horizontally scrolling strip, one column per OPEN sidebar row,
//! in the sidebar's order. The sidebar is the index; the strip is its
//! content laid out side by side.
//!
//! Everything in here is arithmetic over a scroll offset, a viewport
//! width and a list — no `Window`, no entity — so the rules the strip
//! lives by are testable without a frame:
//!
//! - **one selection, always.** Selecting a row scrolls its column into
//!   view; an operator scroll selects the column under the viewport's
//!   centre. The two directions never fight because only an OPERATOR
//!   gesture re-derives the selection from the scroll (`centered`), and a
//!   selection-driven move is the MINIMAL one (`scroll_into_view`) — it
//!   never re-centres, so it never lands on a different column's centre
//!   and flips the selection back.
//! - **virtualization.** Only the columns intersecting the viewport, plus
//!   one either side, are laid out and painted (`mounted_range`); the
//!   rest are fixed-width empty slots so the strip keeps its length.
//!   Pages composite only inside the viewport proper (`visible_range`).
//! - **order is the sidebar's.** The strip holds no order of its own:
//!   `merge_order` takes what the rail's paint recorded (every open row,
//!   collapsed children included) and appends anything live the paint did
//!   not place, in the engine's order, so nothing open is unreachable.
//!
//! The horizontal-gesture rule the strip states in code (`Shell::
//! render_columns_well`): a wheel over a PAGE surface is that page's —
//! `web::page::page_element` stops propagation, so it is swipe-back
//! (BR3) or the page's own scroll; over headers, gutters and chat columns
//! it reaches the strip, which moves only on a horizontal-DOMINANT delta
//! (`strip_delta`), so a vertical scroll through a transcript with a
//! little sideways drift does not walk the strip.

use std::collections::HashSet;
use std::ops::Range;

/// The content area's layout. Persisted per window in `frame.json` under
/// [`LAYOUT_KEY`], like the rail's collapse state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// One well, the selected row — the layout eave had until CV1. Kept,
    /// reachable from any card's ⛶, and no longer the default: the
    /// operator's pivot of 2026-09-07 made the plane the product and this
    /// the room you go into to concentrate on one thing.
    Focus,
    /// The horizontal strip of columns: every card a column wide, one
    /// row, one axis. Kept, and no longer the door anything opens —
    /// [`LayoutMode::Canvas`] is the strip with both constraints lifted,
    /// and an untouched canvas at a wide window IS this. Reachable only
    /// from a `frame.json` that already names it.
    Columns,
    /// Slice CV1 — the plane. Cards carry their own rect, the grid flows
    /// and wraps, and the view pans on both axes. This is the third slot
    /// CL1 reserved, taken as a rename exactly as its note said it would
    /// be, rather than as a new enum. **The default, and the product.**
    #[default]
    Canvas,
}

impl LayoutMode {
    /// The key written to `frame.json`.
    pub fn key(self) -> &'static str {
        match self {
            LayoutMode::Focus => "focus",
            LayoutMode::Columns => "columns",
            LayoutMode::Canvas => "canvas",
        }
    }

    /// The mode a `frame.json` key names. Anything unknown is the
    /// DEFAULT, not Focus by name — so the answer follows the product
    /// when the default moves, which is the whole point of a default.
    /// `"reserved"`, the key CL1 never wrote, lands there too.
    pub fn from_key(key: &str) -> Self {
        match key {
            "focus" => LayoutMode::Focus,
            "columns" => LayoutMode::Columns,
            "canvas" => LayoutMode::Canvas,
            _ => LayoutMode::default(),
        }
    }

    /// Whether the well draws the fixed strip.
    pub fn is_columns(self) -> bool {
        matches!(self, LayoutMode::Columns)
    }

    /// Whether the well draws the plane.
    pub fn is_canvas(self) -> bool {
        matches!(self, LayoutMode::Canvas)
    }

    /// Whether the well lays out CARDS at all — the strip or the plane,
    /// as opposed to Focus's single row. Every call site that used to ask
    /// `is_columns` about *the well's shape* asks this instead; the ones
    /// that ask about the strip's own arithmetic still ask `is_columns`.
    pub fn lays_out_cards(self) -> bool {
        matches!(self, LayoutMode::Columns | LayoutMode::Canvas)
    }
}

/// One column's identity: a tab, or a task's `changes` row (which is not
/// a tab — see `Tool::Diff`). Same two kinds of content row the sidebar
/// draws under a task, and nothing else: headers, folders, bookmarks and
/// the pinned grid are sidebar furniture, not content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ColumnId {
    Tab(String),
    Diff(String),
}

impl ColumnId {
    /// The selector stem every element of this column wears
    /// (`column-<stem>`, `column-head-<stem>`, …).
    pub fn stem(&self) -> String {
        match self {
            ColumnId::Tab(id) => id.clone(),
            ColumnId::Diff(root) => format!("diff-{root}"),
        }
    }

    pub fn tab(&self) -> Option<&str> {
        match self {
            ColumnId::Tab(id) => Some(id),
            ColumnId::Diff(_) => None,
        }
    }
}

/// One column's width. One constant, as asked; the strip has no per-column
/// width and no resize handle.
pub const COLUMN_W: f32 = 560.0;
/// The gap between columns.
pub const COLUMN_GUTTER: f32 = 12.0;
/// One column plus one gutter — the strip's period.
pub const COLUMN_PITCH: f32 = COLUMN_W + COLUMN_GUTTER;
/// How long a programmatic move (a selection coming into view, a
/// chord) takes to glide; a wheel never glides.
pub const STRIP_GLIDE_MS: u64 = 260;

/// The strip's whole content width for `n` columns.
pub fn total_width(n: usize) -> f32 {
    if n == 0 {
        0.0
    } else {
        n as f32 * COLUMN_W + (n - 1) as f32 * COLUMN_GUTTER
    }
}

/// Where column `ix` starts, in content coordinates.
pub fn column_left(ix: usize) -> f32 {
    ix as f32 * COLUMN_PITCH
}

/// The furthest the strip can scroll: nothing past the last column.
pub fn max_scroll(n: usize, view_w: f32) -> f32 {
    (total_width(n) - view_w).max(0.0)
}

pub fn clamp_scroll(scroll: f32, n: usize, view_w: f32) -> f32 {
    if !scroll.is_finite() {
        return 0.0;
    }
    scroll.clamp(0.0, max_scroll(n, view_w))
}

/// The columns intersecting the viewport — the ones a page may composite
/// in. Empty when the viewport has no width yet (the first frame).
pub fn visible_range(scroll: f32, view_w: f32, n: usize) -> Range<usize> {
    if n == 0 || view_w <= 0.0 {
        return 0..0;
    }
    let first = (scroll / COLUMN_PITCH).floor().max(0.0) as usize;
    // The column under the viewport's right edge, inclusive; a gutter
    // under the edge still counts the column before it.
    let last = ((scroll + view_w) / COLUMN_PITCH).floor().max(0.0) as usize;
    let first = first.min(n);
    let last = (last + 1).min(n);
    first..last.max(first)
}

/// The columns that are laid out and painted: the visible ones and one
/// either side, so a scroll of less than a column reveals content that
/// is already there rather than a slot filling in.
pub fn mounted_range(scroll: f32, view_w: f32, n: usize) -> Range<usize> {
    let v = visible_range(scroll, view_w, n);
    if v.is_empty() {
        return v;
    }
    v.start.saturating_sub(1)..(v.end + 1).min(n)
}

/// The column under the viewport's centre — what an operator scroll
/// selects. A gutter under the centre resolves to the column before it.
pub fn centered(scroll: f32, view_w: f32, n: usize) -> Option<usize> {
    if n == 0 || view_w <= 0.0 {
        return None;
    }
    let centre = scroll + view_w / 2.0;
    let ix = (centre / COLUMN_PITCH).floor().max(0.0) as usize;
    Some(ix.min(n - 1))
}

/// The minimal scroll that shows column `ix` whole. Already whole: the
/// scroll is returned unchanged, which is what keeps a selection change
/// from re-centring the strip and re-deriving a different selection.
pub fn scroll_into_view(scroll: f32, ix: usize, view_w: f32, n: usize) -> f32 {
    if n == 0 || view_w <= 0.0 {
        return clamp_scroll(scroll, n, view_w);
    }
    let ix = ix.min(n - 1);
    let left = column_left(ix);
    let right = left + COLUMN_W;
    let scroll = clamp_scroll(scroll, n, view_w);
    let want = if left < scroll {
        left
    } else if right > scroll + view_w {
        right - view_w
    } else {
        scroll
    };
    clamp_scroll(want, n, view_w)
}

/// ⌘⇧[ / ⌘⇧] — one column along. **Clamps; does not wrap**, for the
/// reason the rail's ⌘⇧↑/↓ walk gives: a bracket is directional, and
/// `strip::cycle` wraps only because ctrl-tab is a cycler. With nothing
/// selected the step enters from the end it travels from.
pub fn step(n: usize, current: Option<usize>, forward: bool) -> Option<usize> {
    if n == 0 {
        return None;
    }
    match current {
        Some(ix) if forward => (ix + 1 < n).then_some(ix + 1),
        Some(ix) => ix.checked_sub(1),
        None if forward => Some(0),
        None => Some(n - 1),
    }
}

/// What a wheel delta does to the strip: the horizontal-dominant part of
/// it, or nothing. A vertical scroll with a little sideways drift is a
/// transcript scroll, not a strip scroll — the axis that dominates owns
/// the gesture, which is also how the outer browser's own scroll
/// containers decide (`allow_concurrent_scroll` is off by default).
///
/// Sign: the delta is the content's motion (natural scrolling), so a
/// negative `dx` — content moving left — REVEALS the columns to the
/// right, which is a larger scroll offset.
pub fn strip_delta(dx: f32, dy: f32) -> Option<f32> {
    if dx == 0.0 || dx.abs() <= dy.abs() {
        return None;
    }
    Some(-dx)
}

/// The strip's order from the sidebar's.
///
/// `walk` is what the rail's paint recorded — every open content row in
/// the order the tree draws them, INCLUDING the rows a collapsed task or
/// project hides (open ≠ visible: a collapsed task's chats and pages are
/// still open). Filtered to what is live NOW (the paint can be a frame
/// behind the snapshot), then every live tab the paint did not place is
/// appended in the engine's order — a session past the rail's window
/// (AG2 #10), a tab that arrived since — so nothing open is missing a
/// column. A tab appears once whatever the walk said.
pub fn merge_order(
    walk: &[ColumnId],
    live_tabs: &[String],
    live_roots: &HashSet<String>,
) -> Vec<ColumnId> {
    let live_tab: HashSet<&str> = live_tabs.iter().map(String::as_str).collect();
    let mut out: Vec<ColumnId> = Vec::with_capacity(walk.len());
    for col in walk {
        let live = match col {
            ColumnId::Tab(id) => live_tab.contains(id.as_str()),
            ColumnId::Diff(root) => live_roots.contains(root),
        };
        if live && !out.contains(col) {
            out.push(col.clone());
        }
    }
    for id in live_tabs {
        let col = ColumnId::Tab(id.clone());
        if !out.contains(&col) {
            out.push(col);
        }
    }
    out
}

/// The `frame.json` key holding each window's layout: mode and scroll.
pub const LAYOUT_KEY: &str = "layout";

/// What `frame.json` remembers of a window's layout.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LayoutPrefs {
    pub mode: LayoutMode,
    /// The strip's scroll, in content pixels. Restored verbatim: the
    /// saved scroll is the one the operator left, and under the
    /// centre-selects rule it is the one that had the selected column in
    /// view.
    pub scroll: f32,
    /// Slice CV1 — the plane's pan, in canvas points. Kept here beside
    /// the scroll rather than with the placements because it is one small
    /// `Copy` pair that every window has, where a placement map exists
    /// only once the operator has moved something.
    pub pan_x: f32,
    pub pan_y: f32,
}

/// The pure half of the layout load: this window's entry under
/// [`LAYOUT_KEY`], or the default. A scroll that is not a finite
/// non-negative number reads as 0 rather than failing the whole entry.
pub fn layout_from(doc: &serde_json::Value, window: &str) -> LayoutPrefs {
    let Some(entry) = doc.get(LAYOUT_KEY).and_then(|v| v.get(window)) else {
        return LayoutPrefs::default();
    };
    let mode = entry
        .get("mode")
        .and_then(|v| v.as_str())
        .map(LayoutMode::from_key)
        .unwrap_or_default();
    let scroll = entry
        .get("scroll")
        .and_then(|v| v.as_f64())
        .map(|x| x as f32)
        .filter(|x| x.is_finite() && *x >= 0.0)
        .unwrap_or(0.0);
    // The pan may be negative — the plane has no origin corner the way
    // the strip has a left end — so it is filtered for finiteness only.
    let pan = |k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_f64())
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .unwrap_or(0.0)
    };
    LayoutPrefs { mode, scroll, pan_x: pan("pan_x"), pan_y: pan("pan_y") }
}

/// The pure half of the layout save: this window's entry written, every
/// other key and every other window's entry kept. A window at the
/// defaults has its entry removed rather than written, so the file does
/// not collect an entry per ⌘N window that ever opened.
pub fn put_layout(doc: &mut serde_json::Value, window: &str, prefs: LayoutPrefs) {
    if !doc.get(LAYOUT_KEY).is_some_and(|v| v.is_object()) {
        doc[LAYOUT_KEY] = serde_json::Value::Object(Default::default());
    }
    let Some(map) = doc[LAYOUT_KEY].as_object_mut() else {
        return;
    };
    if prefs == LayoutPrefs::default() {
        map.remove(window);
        return;
    }
    map.insert(
        window.to_string(),
        serde_json::json!({
            "mode": prefs.mode.key(),
            "scroll": prefs.scroll,
            "pan_x": prefs.pan_x,
            "pan_y": prefs.pan_y,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_third_slot_became_the_canvas_and_the_unknown_key_is_the_default() {
        assert_eq!(LayoutMode::from_key("focus"), LayoutMode::Focus);
        assert_eq!(LayoutMode::from_key("columns"), LayoutMode::Columns);
        assert_eq!(LayoutMode::from_key("canvas"), LayoutMode::Canvas);
        // The key CL1 reserved and never wrote, and anything else, read
        // as the DEFAULT rather than as Focus by name.
        assert_eq!(LayoutMode::from_key("reserved"), LayoutMode::default());
        assert_eq!(LayoutMode::from_key("anything"), LayoutMode::default());
        // Each mode round-trips through its own key.
        for m in [LayoutMode::Focus, LayoutMode::Columns, LayoutMode::Canvas] {
            assert_eq!(LayoutMode::from_key(m.key()), m, "{m:?} did not round-trip");
        }
        // The strip and the plane are separate answers; both lay out cards.
        assert!(LayoutMode::Columns.is_columns() && !LayoutMode::Columns.is_canvas());
        assert!(LayoutMode::Canvas.is_canvas() && !LayoutMode::Canvas.is_columns());
        assert!(LayoutMode::Canvas.lays_out_cards() && LayoutMode::Columns.lays_out_cards());
        assert!(!LayoutMode::Focus.lays_out_cards());
    }

    #[test]
    fn widths_tile_exactly() {
        assert_eq!(total_width(0), 0.0);
        assert_eq!(total_width(1), COLUMN_W);
        assert_eq!(total_width(3), 3.0 * COLUMN_W + 2.0 * COLUMN_GUTTER);
        assert_eq!(column_left(2), 2.0 * COLUMN_PITCH);
        assert_eq!(max_scroll(3, 1000.0), total_width(3) - 1000.0);
        assert_eq!(max_scroll(1, 1000.0), 0.0);
        assert_eq!(clamp_scroll(-5.0, 3, 1000.0), 0.0);
        assert_eq!(clamp_scroll(f32::NAN, 3, 1000.0), 0.0);
        assert_eq!(clamp_scroll(9999.0, 3, 1000.0), max_scroll(3, 1000.0));
    }

    /// The visible set is what intersects the viewport; mounted is that
    /// plus one either side; both clamp to the list.
    #[test]
    fn visible_and_mounted_ranges() {
        // A 1000 px viewport at rest shows columns 0 and 1 (0..572 and
        // 572..1132) — the second is cut, and cut still counts.
        assert_eq!(visible_range(0.0, 1000.0, 8), 0..2);
        assert_eq!(mounted_range(0.0, 1000.0, 8), 0..3);
        // Scrolled to the third column's start.
        let s = column_left(2);
        assert_eq!(visible_range(s, 1000.0, 8), 2..4);
        assert_eq!(mounted_range(s, 1000.0, 8), 1..5);
        // At the end, the last column is included and nothing past it.
        let end = max_scroll(8, 1000.0);
        let v = visible_range(end, 1000.0, 8);
        assert_eq!(v.end, 8);
        assert!(v.start <= 6);
        assert_eq!(mounted_range(end, 1000.0, 8).end, 8);
        // No width yet: nothing is visible, nothing is mounted.
        assert_eq!(visible_range(0.0, 0.0, 8), 0..0);
        assert_eq!(mounted_range(0.0, 0.0, 8), 0..0);
        assert_eq!(visible_range(0.0, 1000.0, 0), 0..0);
        // A wide viewport holds every column.
        assert_eq!(visible_range(0.0, 5000.0, 3), 0..3);
        assert_eq!(mounted_range(0.0, 5000.0, 3), 0..3);
    }

    #[test]
    fn the_centred_column_is_the_one_under_the_viewport_middle() {
        assert_eq!(centered(0.0, 1000.0, 8), Some(0));
        // Centre at 1072 → column 1 (572..1132).
        assert_eq!(centered(column_left(1), 1000.0, 8), Some(1));
        // Past the end the centre clamps to the last column.
        assert_eq!(centered(max_scroll(8, 1000.0), 1000.0, 8), Some(7));
        assert_eq!(centered(0.0, 1000.0, 0), None);
        assert_eq!(centered(0.0, 0.0, 3), None);
    }

    /// The selection-driven move is MINIMAL — and that is what keeps the
    /// two directions from fighting.
    #[test]
    fn scroll_into_view_moves_the_least_and_never_recentres() {
        // Already whole in view: no move.
        assert_eq!(scroll_into_view(0.0, 0, 1000.0, 8), 0.0);
        assert_eq!(scroll_into_view(0.0, 1, 1200.0, 8), 0.0);
        // Cut on the right: scroll so its right edge meets the viewport's.
        let s = scroll_into_view(0.0, 1, 1000.0, 8);
        assert_eq!(s, column_left(1) + COLUMN_W - 1000.0);
        // To the left of the viewport: scroll to its left edge.
        assert_eq!(
            scroll_into_view(column_left(5), 2, 1000.0, 8),
            column_left(2)
        );
        // Clamped at both ends.
        assert_eq!(scroll_into_view(0.0, 7, 1000.0, 8), max_scroll(8, 1000.0));
        assert_eq!(scroll_into_view(0.0, 9, 1000.0, 8), max_scroll(8, 1000.0));
        assert_eq!(scroll_into_view(400.0, 0, 1000.0, 8), 0.0);
        // The move that showed column 1 did not put column 2 under the
        // centre — the loop this rule exists to close.
        assert_eq!(centered(s, 1000.0, 8), Some(1));
    }

    #[test]
    fn the_step_clamps_and_enters_from_the_travelling_end() {
        assert_eq!(step(3, Some(0), true), Some(1));
        assert_eq!(step(3, Some(2), true), None);
        assert_eq!(step(3, Some(0), false), None);
        assert_eq!(step(3, Some(2), false), Some(1));
        assert_eq!(step(3, None, true), Some(0));
        assert_eq!(step(3, None, false), Some(2));
        assert_eq!(step(0, None, true), None);
    }

    #[test]
    fn only_a_horizontal_dominant_delta_moves_the_strip() {
        assert_eq!(strip_delta(-30.0, 2.0), Some(30.0));
        assert_eq!(strip_delta(30.0, -2.0), Some(-30.0));
        assert_eq!(strip_delta(3.0, -40.0), None);
        assert_eq!(strip_delta(0.0, 0.0), None);
        assert_eq!(strip_delta(5.0, 5.0), None);
    }

    /// Open ≠ visible: the walk carries a collapsed task's rows and they
    /// keep their columns; a closed tab loses its column; a tab the paint
    /// never placed still gets one, at the end.
    #[test]
    fn merge_order_is_the_sidebars_plus_the_unplaced_and_minus_the_closed() {
        let walk = vec![
            ColumnId::Tab("p1".into()),
            ColumnId::Tab("s1".into()),
            ColumnId::Diff("/repo".into()),
            ColumnId::Tab("p2".into()),
            ColumnId::Tab("closed".into()),
            ColumnId::Diff("/gone".into()),
            ColumnId::Tab("p1".into()),
        ];
        let live: Vec<String> = ["p1", "s1", "p2", "new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let roots: HashSet<String> = ["/repo".to_string()].into_iter().collect();
        assert_eq!(
            merge_order(&walk, &live, &roots),
            vec![
                ColumnId::Tab("p1".into()),
                ColumnId::Tab("s1".into()),
                ColumnId::Diff("/repo".into()),
                ColumnId::Tab("p2".into()),
                ColumnId::Tab("new".into()),
            ]
        );
        assert!(merge_order(&[], &[], &HashSet::new()).is_empty());
    }

    #[test]
    fn layout_round_trips_through_frame_json_and_keeps_its_neighbours() {
        let mut doc = serde_json::json!({
            "rail_groups_collapsed": ["/a"],
            LAYOUT_KEY: { "other-window": { "mode": "columns", "scroll": 12.0 } },
        });
        let prefs = LayoutPrefs {
            mode: LayoutMode::Columns,
            scroll: 1144.5,
            // CV1: the plane's pan rides in the same entry, and may be
            // negative — the plane has no left end the way the strip has.
            pan_x: -240.0,
            pan_y: 96.0,
        };
        put_layout(&mut doc, "primary", prefs);
        assert_eq!(layout_from(&doc, "primary"), prefs);
        assert_eq!(
            layout_from(&doc, "other-window"),
            LayoutPrefs {
                mode: LayoutMode::Columns,
                scroll: 12.0,
                pan_x: 0.0,
                pan_y: 0.0,
            }
        );
        assert_eq!(doc["rail_groups_collapsed"], serde_json::json!(["/a"]));
        // Back to the defaults removes the entry rather than writing it.
        put_layout(&mut doc, "primary", LayoutPrefs::default());
        assert!(doc[LAYOUT_KEY].get("primary").is_none());
        assert_eq!(layout_from(&doc, "primary"), LayoutPrefs::default());
        // A bad scroll reads as 0 without losing the mode.
        let bad = serde_json::json!({ LAYOUT_KEY: { "w": { "mode": "columns", "scroll": -4 } } });
        assert_eq!(
            layout_from(&bad, "w"),
            LayoutPrefs {
                mode: LayoutMode::Columns,
                scroll: 0.0,
                pan_x: 0.0,
                pan_y: 0.0,
            }
        );
        assert_eq!(
            layout_from(&serde_json::json!({}), "w"),
            LayoutPrefs::default()
        );
    }

    #[test]
    fn column_stems_name_the_row() {
        assert_eq!(ColumnId::Tab("t1".into()).stem(), "t1");
        assert_eq!(ColumnId::Diff("/r".into()).stem(), "diff-/r");
        assert_eq!(ColumnId::Diff("/r".into()).tab(), None);
        assert_eq!(ColumnId::Tab("t1".into()).tab(), Some("t1"));
    }
}
