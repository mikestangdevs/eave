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

/// Demo file review for the theme editor. Sample rows are bezel's.
pub struct DiffDemo;

impl gpui::Render for DiffDemo {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use bezel::theme::Theme;
        use bezel::ui::widgets::Scaffolding as _;
        let theme = Theme::of(cx).clone();
        theme
            .group_box()
            .mt(px(0.0))
            .child(demo_header(&theme))
            .child(
                div()
                    .id("diff-demo-rows")
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .children(DEMO.iter().map(|row| demo_row(&theme, *row))),
            )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Removed,
    Context,
    Skip,
}

type DemoRow = (Change, Option<u32>, Option<u32>, &'static str);

const DEMO_FILE: &str = "crates/ui/src/scroll.rs";
const DEMO: &[DemoRow] = &[
    (Change::Skip, None, None, "@@ -232,7 +232,12 @@"),
    (
        Change::Context,
        Some(232),
        Some(232),
        "pub fn at_bottom(max_offset: Pixels, offset: Pixels) -> bool {",
    ),
    (Change::Removed, Some(233), None, "    max_offset - offset.abs() <= px(0.0)"),
    (
        Change::Added,
        None,
        Some(233),
        "    if max_offset <= px(0.0) {",
    ),
    (Change::Added, None, Some(234), "        return true;"),
    (Change::Added, None, Some(235), "    }"),
    (
        Change::Added,
        None,
        Some(236),
        "    max_offset - offset.clamp(-max_offset, px(0.0)).abs() <= slack",
    ),
    (Change::Context, Some(234), Some(237), "}"),
];

fn demo_header(theme: &bezel::theme::Theme) -> Div {
    use bezel::ui::icons;
    let count = |wanted: Change| DEMO.iter().filter(|(c, ..)| *c == wanted).count();
    let tally = |value: usize, sign: &'static str, color: gpui::Hsla| {
        div()
            .font_family(theme.font_mono.clone())
            .text_size(px(11.5))
            .text_color(color)
            .child(SharedString::from(format!("{sign}{value}")))
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .py(px(7.0))
        .border_b_1()
        .border_color(theme.border)
        .child(
            icons::icon(icons::DOCUMENT)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.font_mono.clone())
                .text_size(px(12.0))
                .text_color(theme.text)
                .child(DEMO_FILE),
        )
        .child(tally(count(Change::Added), "+", theme.success))
        .child(tally(count(Change::Removed), "−", theme.danger))
}

fn demo_row(theme: &bezel::theme::Theme, (change, old, new, text): DemoRow) -> Div {
    let wash = 0.10;
    let (bg, mark, sign) = match change {
        Change::Added => (theme.success.opacity(wash), theme.success, "+"),
        Change::Removed => (theme.danger.opacity(wash), theme.danger, "-"),
        Change::Context | Change::Skip => (gpui::transparent_black(), theme.text_faint, " "),
    };
    let number = |value: Option<u32>| {
        div()
            .w(px(theme::DIFF_NUM_W))
            .flex_none()
            .text_align(gpui::TextAlign::Right)
            .text_color(theme.text_faint)
            .child(match value {
                Some(v) => SharedString::from(v.to_string()),
                None => SharedString::default(),
            })
    };
    let line = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .px(px(10.0))
        .py(px(1.0))
        .bg(bg)
        .font_family(theme.font_mono.clone())
        .text_size(px(12.0))
        .line_height(px(18.0));
    if change == Change::Skip {
        return line.bg(theme.bg.opacity(0.4)).child(
            div()
                .text_color(theme.text_faint)
                .child(text),
        );
    }
    line.child(number(old))
        .child(number(new))
        .child(div().w(px(theme::DIFF_SIGN_W)).flex_none().text_color(mark).child(sign))
        .child(
            div()
                .min_w_0()
                .text_color(if change == Change::Context {
                    theme.text_muted
                } else {
                    theme.text
                })
                .child(text),
        )
}
