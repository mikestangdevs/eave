//! The diff row bezel's gallery paints — ported from
//! `bezel/apps/gallery/src/patterns/diff.rs`.
//!
//! bezel has no diff *component*. The finding of that page is that a row is
//! two numbers, a sign and a line of text. Slice BZ took the sign column
//! and left the numbers behind because `DiffLine` does not carry them.
//! They are recovered here from the hunk header (`@@ -old +new @@`) and
//! walked as the lines are painted, so nothing on the wire had to move.

use gpui::{div, prelude::*, px, Div, SharedString};
use eave_wire::{DiffLine, DiffOp};

use crate::theme;

/// `(old, new)` starts from a unified-diff hunk header.
pub fn hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace();
    let _ = parts.next()?; // `@@`
    let old = parts.next()?;
    let new = parts.next()?;
    let parse = |s: &str| -> Option<u32> {
        s.trim_start_matches(['-', '+'])
            .split(',')
            .next()?
            .parse()
            .ok()
    };
    Some((parse(old)?, parse(new)?))
}

/// Advance the two counters for one line and return the numbers that
/// line should show. An add has no old number; a delete has no new.
pub fn numbers(op: DiffOp, old: &mut u32, new: &mut u32) -> (Option<u32>, Option<u32>) {
    match op {
        DiffOp::Add => {
            let n = *new;
            *new = new.saturating_add(1);
            (None, Some(n))
        }
        DiffOp::Del => {
            let o = *old;
            *old = old.saturating_add(1);
            (Some(o), None)
        }
        DiffOp::Ctx => {
            let (o, n) = (*old, *new);
            *old = old.saturating_add(1);
            *new = new.saturating_add(1);
            (Some(o), Some(n))
        }
    }
}

/// One content line: two number columns, a sign, the text. Washes and
/// signs are bezel's; the numbers are the third decision BZ left.
pub fn line(line: &DiffLine, old: Option<u32>, new: Option<u32>) -> Div {
    let bz = crate::bezel_theme::theme();
    let (wash, mark, sign) = match line.op {
        DiffOp::Add => (theme::diff_add_bg(), bz.success, "+"),
        DiffOp::Del => (theme::diff_del_bg(), bz.danger, "-"),
        DiffOp::Ctx => (gpui::transparent_black(), bz.text_faint, " "),
    };
    let number = |value: Option<u32>| {
        div()
            .w(px(theme::DIFF_NUM_W))
            .flex_none()
            .text_align(gpui::TextAlign::Right)
            .text_color(bz.text_faint)
            .child(match value {
                Some(v) => SharedString::from(v.to_string()),
                None => SharedString::default(),
            })
    };
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(theme::SPACE_2))
        .px(px(theme::SPACE_2))
        .bg(wash)
        .font_family(theme::MONO)
        .text_size(px(theme::DIFF_SIZE))
        .line_height(px(theme::DIFF_LINE))
        .child(number(old))
        .child(number(new))
        .child(
            div()
                .w(px(theme::DIFF_SIGN_W))
                .flex_none()
                .text_color(mark)
                .child(sign),
        )
        .child(
            div()
                .min_w(px(0.))
                .text_color(if line.op == DiffOp::Ctx {
                    bz.text_muted
                } else {
                    theme::text_secondary_on_wash()
                })
                .child(SharedString::from(line.text.clone())),
        )
}

/// The gap between hunks — bezel's `Skip` row. No numbers, no sign.
pub fn skip(header: &str) -> Div {
    let bz = crate::bezel_theme::theme();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::SPACE_2))
        .px(px(theme::SPACE_2))
        .py(px(theme::SPACE_1))
        .border_b_1()
        .border_color(bz.border)
        .font_family(theme::MONO)
        .text_size(px(theme::MICRO_SIZE))
        .line_height(px(theme::SMALL_LINE))
        .text_color(bz.text_muted)
        .truncate()
        .child(SharedString::from(header.to_string()))
}
