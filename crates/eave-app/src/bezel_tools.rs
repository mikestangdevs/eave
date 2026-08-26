//! Tool-call presentation from bezel's gallery
//! (`bezel/apps/gallery/src/patterns/agent.rs`).
//!
//! bezel has no ToolCall type: `step_row` takes strings. What looked like
//! a component is `slice::chunk_by` over consecutive calls of the same
//! verb, plus a bordered box. A lone call is a card; a group of them
//! shares one box and the group's row owns the disclosure.
//!
//! The live transcript is virtualized per tool, so a group header that
//! swallowed three tools would be a display-list change. Consecutive
//! same-name tools still *look* grouped: they share one box by joining
//! their corners, which does not change row count or fingerprints.

use gpui::{div, prelude::*, px, Div};
use eave_wire::model::Row;

use crate::theme;

/// Where this tool sits in a run of the same name.
#[derive(Clone, Copy)]
pub struct Run {
    pub first: bool,
    pub last: bool,
}

/// Consecutive `Row::Tool` of the same `name`, with nothing else between.
pub fn run_at(rows: &[Row], index: usize) -> Run {
    let Some(Row::Tool { name, .. }) = rows.get(index) else {
        return Run {
            first: true,
            last: true,
        };
    };
    let mut start = index;
    while start > 0 {
        match &rows[start - 1] {
            Row::Tool { name: prev, .. } if prev == name => start -= 1,
            _ => break,
        }
    }
    let mut end = index;
    while end + 1 < rows.len() {
        match &rows[end + 1] {
            Row::Tool { name: next, .. } if next == name => end += 1,
            _ => break,
        }
    }
    Run {
        first: start == index,
        last: end == index,
    }
}

/// The box a card and a group share: rounded, bordered, clipping.
pub fn box_(first: bool, last: bool) -> Div {
    let r = px(theme::scaled_radius(theme::RADIUS_PANEL));
    let bz = crate::bezel_theme::theme();
    div()
        .when(first && last, |d| d.rounded(r))
        .when(first && !last, |d| d.rounded_t(r))
        .when(last && !first, |d| d.rounded_b(r))
        .when(first, |d| d.border_t_1())
        .border_l_1()
        .border_r_1()
        .when(last, |d| d.border_b_1())
        .when(!first, |d| d.border_t_1())
        .border_color(bz.border)
        .overflow_hidden()
}
