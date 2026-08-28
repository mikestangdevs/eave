//! Slice N3 — the loaders. The first consumer of N1's frame drive.
//!
//! Everything here is a *live-state* shape: a working indicator that says
//! "this is moving right now", and skeletons that say "this has not
//! arrived yet". Before N3 eave stated those two facts with a static dot
//! and the single character `…`, which are the same picture whether the
//! thing is working or wedged.
//!
//! Three rules the shapes are built to, and each one is load-bearing:
//!
//!   - **One period, one epoch.** Every indicator in the window rides
//!     [`WORK_PULSE`] on [`motion::pulse_delta`], so a rail dot and the
//!     transcript trailer are phase-locked instead of wobbling against
//!     each other. The shell asks the clock ONCE per frame and hands the
//!     same phase to every site (`Shell::render`).
//!   - **Fixed slots.** A cell only ever changes its opacity, inside a
//!     container whose size does not depend on the phase. Nothing an
//!     indicator does is layout-visible, which is what lets one sit
//!     inside a virtualized transcript row without touching the row's
//!     cached height (polish-inventory §6.12).
//!   - **`None` is the resting form, not a blank.** Reduced motion, and
//!     any site with nothing in flight, get the element eave drew before
//!     this slice: a solid dot, a solid bar. A working indicator that
//!     vanishes when the machine asks for less motion would have removed
//!     the *fact*, not the motion.
//!
//! The design values are all in [`theme`] (the standing constraint: no
//! design number outside it). What lives here is the arithmetic and the
//! elements.

use gpui::{
    div, linear_color_stop, linear_gradient, prelude::*, px, relative, App, Div, EntityId, Hsla,
};

use crate::motion::{self, MotionSpec, EASE_IN_OUT};
use crate::theme;

/// The one timeline every indicator in the window shares. Repeating: it
/// is read through [`motion::pulse_delta`], never through gpui's
/// `with_animation` (progress.md's standing trap — a mounted animation
/// element requests a redraw every DISPLAY frame and cost comet 36% CPU
/// for one spinner).
pub const WORK_PULSE: MotionSpec = MotionSpec::new(theme::WORK_PULSE_MS, EASE_IN_OUT);

/// This frame's phase for every indicator in the window, or `None` when
/// nothing should move.
///
/// `working` is the shell's answer to "is anything in this window
/// mid-flight" — the ONE place that decides whether the pulse clock gets
/// a lease this frame. False means no lease, so the clock parks and the
/// window goes back to scheduling nothing at all (N1's invariant).
///
/// The reduced-motion branch is here rather than left to the clock on
/// purpose. `pulse_delta` already honours the preference — it returns a
/// static 0.0 and never even constructs the global — but a frozen phase
/// and no phase are different *pictures*: 0.0 would render the cell grid
/// stopped mid-wave, which looks broken, where `None` renders the solid
/// dot the slot has always carried.
pub fn pulse(working: bool, view: EntityId, cx: &mut App) -> Option<f32> {
    if !working || cx.reduce_motion() {
        return None;
    }
    Some(motion::pulse_delta(&WORK_PULSE, view, cx))
}

/// One cell's opacity factor: rest at [`theme::WORK_CELL_REST`], full at
/// the crest, `offset` turns of the period ahead of the shared phase.
///
/// The crest is a triangle run through the spec's own curve, which is
/// what makes this a *catalog* shape rather than a second easing
/// implementation: one smooth rise and fall per period, symmetric about
/// the midpoint because [`EASE_IN_OUT`] is.
fn cell_alpha(phase: f32, offset: f32) -> f32 {
    let t = (phase + offset).rem_euclid(1.0);
    let triangle = if t < 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
    let crest = WORK_PULSE.curve.eval(triangle);
    theme::WORK_CELL_REST + (1.0 - theme::WORK_CELL_REST) * crest
}

/// A cell's tint at `phase`, or the full color when nothing is moving.
fn tint(color: Hsla, phase: Option<f32>, offset: f32) -> Hsla {
    match phase {
        Some(p) => color.opacity(cell_alpha(p, offset)),
        None => color,
    }
}

/// The working indicator sized to a state-dot slot: four cells walking
/// the perimeter clockwise, one turn per period — a rotation with no
/// rotating element, and no transform primitive needed to draw it.
///
/// `slot` is the size of the dot being replaced ([`theme::STATE_DOT`],
/// [`theme::TOOL_DOT`]); the indicator is exactly that big, so the flip
/// between resting and working moves nothing around it.
pub fn work_dot(slot: f32, color: Hsla, phase: Option<f32>) -> Div {
    if phase.is_none() {
        return div().size(px(slot)).rounded_full().bg(color);
    }
    // Four cells and one gap across the slot in each axis.
    let cell = (slot - theme::WORK_DOT_GAP) / 2.0;
    let quarter = |q: usize| square(cell, tint(color, phase, q as f32 / 4.0));
    div()
        .size(px(slot))
        .flex()
        .flex_col()
        .justify_between()
        // Clockwise from the top-left: 0, 1 across the top; 3, 2 back
        // along the bottom.
        .child(pair(quarter(0), quarter(1)))
        .child(pair(quarter(3), quarter(2)))
}

fn pair(left: Div, right: Div) -> Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .justify_between()
        .child(left)
        .child(right)
}

fn square(size: f32, color: Hsla) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size / 2.0))
        .bg(color)
}

/// Load wash on the address pill. The element *is* the pill
/// (`inset_0` + [`theme::RADIUS_PANEL`]); a moving gradient fills it,
/// so the colour follows the rounded shape instead of a square slice.
/// Canvas quads ignored the radius and painted a rectangle (the glitch
/// in the screenshot).
pub fn address_load(t: f32, alpha: f32) -> Div {
    let shown = t.clamp(0.0, 1.0);
    let wash = theme::live().opacity(theme::ADDRESS_LOAD_WASH * alpha.clamp(0.0, 1.0));
    let el = div()
        .debug_selector(|| "address-load".into())
        .absolute()
        .inset_0()
        .rounded(px(theme::RADIUS_PANEL))
        .overflow_hidden();
    if shown >= 1.0 {
        el.bg(wash)
    } else {
        let tip = theme::ADDRESS_LOAD_TIP;
        let hold = (shown - tip).clamp(0.0, 1.0);
        let end = shown.max(hold + 0.02).min(1.0);
        el.bg(linear_gradient(
            90.0,
            linear_color_stop(wash, hold),
            linear_color_stop(wash.opacity(0.0), end),
        ))
    }
}

/// The in-flow form: a short row of cells with the crest sweeping across
/// it. Used where a caret used to sit, and in the transcript trailer.
///
/// The stagger is half a period spread over the whole row, so the crest
/// crosses the bar once per period and the row always shows a gradient
/// rather than blinking in unison.
pub fn work_bar(color: Hsla, phase: Option<f32>) -> Div {
    let n = theme::WORK_BAR_CELLS;
    let cells = (0..n).map(|i| {
        let offset = 0.5 * i as f32 / n as f32;
        square(theme::WORK_BAR_CELL, tint(color, phase, offset))
    });
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::WORK_BAR_GAP))
        .children(cells)
}

/// Skeleton rows for a not-yet-arrived state: fixed-height slots, each
/// holding a bar that pulses in place. The row heights are the line
/// height of the text the skeleton stands in for, so the arriving content
/// lands in the rhythm the skeleton was already keeping.
///
/// `rows` is capped at the width table's length — the widths cycle, and a
/// caller asking for more rows than there are widths would repeat the
/// pattern, which reads as a texture rather than as text.
pub fn skeleton(rows: usize, phase: Option<f32>) -> Div {
    let widths = theme::SKELETON_WIDTHS;
    let n = rows.min(widths.len());
    let bars = (0..n).map(|i| {
        // Stagger down the rows: the crest travels the block instead of
        // every row flashing together.
        let offset = 0.5 * i as f32 / n.max(1) as f32;
        div()
            .h(px(theme::SKELETON_ROW_H))
            .flex()
            .items_center()
            .child(
                div()
                    .h(px(theme::SKELETON_BAR_H))
                    .w(relative(widths[i]))
                    .rounded(px(theme::SKELETON_BAR_H / 2.0))
                    .bg(tint(theme::overlay_strong(), phase, offset)),
            )
    });
    div().flex().flex_col().children(bars)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rest floor is the whole reason a state dot can become a
    /// working indicator: it never reaches invisible, so a dot that
    /// exists keeps saying the session exists. And the crest reaches
    /// full, or the indicator would read as permanently dimmed.
    #[test]
    fn a_cell_rests_at_the_floor_and_crests_at_full() {
        // Written as literals rather than derived from the constants
        // under test (progress.md trap, Slice N1): an expectation
        // computed from WORK_CELL_REST passes for any value of it.
        assert!((cell_alpha(0.0, 0.0) - 0.28).abs() < 1e-5, "rest moved");
        assert!((cell_alpha(0.5, 0.0) - 1.0).abs() < 1e-5, "crest moved");
        // The whole domain stays inside the unit interval — an opacity
        // outside it is a paint bug that only shows on one frame.
        for i in 0..=2_000u32 {
            let p = i as f32 / 1_000.0; // two full periods, phase unwrapped
            for offset in [0.0, 0.125, 0.25, 0.375, 0.5, 0.75] {
                let a = cell_alpha(p, offset);
                assert!(
                    (0.28..=1.0).contains(&a),
                    "cell_alpha({p}, {offset}) = {a} left [rest, 1]"
                );
            }
        }
    }

    /// The wave has to actually travel, or the "gradient" is four cells
    /// blinking in unison. At any phase the four perimeter cells of a dot
    /// must not all be at the same brightness.
    #[test]
    fn the_crest_travels_instead_of_blinking_in_unison() {
        for i in 0..40u32 {
            let p = i as f32 / 40.0;
            let alphas: Vec<f32> = (0..4).map(|q| cell_alpha(p, q as f32 / 4.0)).collect();
            let spread = alphas.iter().cloned().fold(f32::MIN, f32::max)
                - alphas.iter().cloned().fold(f32::MAX, f32::min);
            assert!(
                spread > 0.3,
                "phase {p} lit every cell alike (spread {spread}) — no wave"
            );
        }
        // Opposite corners of the perimeter walk are half a period apart,
        // so one is at rest exactly when the other is at the crest.
        assert!((cell_alpha(0.5, 0.0) - cell_alpha(0.5, 0.5)).abs() > 0.6);
    }

    /// One period, one loop: the phase is cyclic, so an indicator that
    /// has been up for an hour looks identical to one a second old.
    #[test]
    fn the_wave_is_periodic() {
        for i in 0..50u32 {
            let p = i as f32 / 50.0;
            assert!((cell_alpha(p, 0.0) - cell_alpha(p + 1.0, 0.0)).abs() < 1e-4);
            assert!((cell_alpha(p, 0.0) - cell_alpha(p, 1.0)).abs() < 1e-4);
        }
    }

    /// A skeleton never renders more rows than it has widths for.
    #[test]
    fn the_skeleton_row_count_is_bounded_by_the_width_table() {
        assert_eq!(theme::SKELETON_ROWS, theme::SKELETON_WIDTHS.len());
    }
}
