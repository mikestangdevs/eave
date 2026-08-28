//! The activity pattern — a model working in public.
//!
//! Ported from `bezel/apps/gallery/src/patterns/agent.rs`. The two library
//! pieces are [`bezel::ui::widgets::Takeover`] (the section opens while the
//! run streams and yields the moment you press the header) and
//! [`bezel::ui::scroll::follow`] (the box pins to its newest line).
//! Everything else is a `div`.
//!
//! The `ActivityDemo` specimen went with the old theme editor (2026-08-25,
//! the Customize Theme rebuild) — the product's [`reasoning_box`] is what
//! this file is for now.

use gpui::{div, linear_color_stop, linear_gradient, prelude::*, px, ScrollHandle, SharedString};
use bezel::ui::scroll::{self, FollowState};

/// How tall the reasoning box may get before it scrolls — Tailwind's
/// `max-h-40`, which is what the source caps it at.
pub const BOX_MAX: f32 = 160.0;
const FADE: f32 = 20.0;

/// Product activity box: the turn's thought, follow-scrolled, Takeover
/// on the header. Words stay eave's ("working for 12s") — the register
/// is instrument; the *shape* is bezel's.
pub fn reasoning_box(
    text: &str,
    scroll: &ScrollHandle,
    follow: &FollowState,
    bg: gpui::Hsla,
    border: gpui::Hsla,
    ink: gpui::Hsla,
) -> gpui::Div {
    let lines: Vec<SharedString> = if text.is_empty() {
        Vec::new()
    } else {
        text.lines()
            .map(|l| SharedString::from(l.to_string()))
            .collect()
    };
    div()
        .ml(px(10.0))
        .pl(px(12.0))
        .border_l_1()
        .border_color(border)
        .child(
            div()
                .relative()
                .max_h(px(BOX_MAX))
                .child(
                    div()
                        .id("session-reasoning")
                        .max_h(px(BOX_MAX))
                        .overflow_y_scroll()
                        .track_scroll(scroll)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .pr(px(14.0))
                                .children(lines.into_iter().map(|line| {
                                    div()
                                        .text_size(px(theme::SMALL_SIZE))
                                        .line_height(px(theme::SMALL_LINE))
                                        .text_color(ink)
                                        .child(line)
                                })),
                        ),
                )
                .child(
                    div().absolute().top_0().left_0().right_0().h(px(FADE)).bg(
                        linear_gradient(
                            180.0,
                            linear_color_stop(bg, 0.0),
                            linear_color_stop(bg.opacity(0.0), 1.0),
                        ),
                    ),
                )
                .child(scroll::follow(scroll, follow)),
        )
}

use crate::theme;
