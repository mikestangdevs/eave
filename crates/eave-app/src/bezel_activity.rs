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

/// **AG1 #9 — waku's sliding-window anchor, and the only half of that row
/// eave can reach.** The row asks for two things: a window on the live
/// trace, and a fold of the settled one. There is no settled trace to
/// fold — [`reasoning_box`] is only ever built by `work_trailer`, which
/// returns early unless a turn is OPEN, and `thought` is cleared on the
/// next `UserSent`. A settled trace would be new surface, not a window.
///
/// How many lines of a live trace the box will hold. [`BOX_MAX`] over
/// `SMALL_LINE` plus the 4 px gap is a shade over eight visible lines, so
/// this is ten screens of scroll-back: generous for a peek, and bounded,
/// which is the whole point. Before it, every paint allocated one
/// `SharedString` and built one element **per line of the entire
/// thought** — and a reasoning model emits thousands of them in a turn.
/// The cost grew with the thinking while the box displaying it stayed
/// 160 px tall and pinned to its newest line.
pub const WINDOW_LINES: usize = 80;

/// How far into the window [`window`] will look for a paragraph break
/// before giving up. A ragged first line is a smaller cost than an anchor
/// that eats the window hunting for one.
const ANCHOR_LINES: usize = WINDOW_LINES / 4;

/// The tail of a reasoning trace, and how many lines it drops off the
/// front.
///
/// The backward scan is bounded by [`WINDOW_LINES`], not by the thought,
/// which is the property being bought. The forward slide to a blank line
/// is waku's anchor: a window that opens mid-sentence reads as a
/// corrupted trace rather than a partial one.
///
/// The elided count costs one pass over the dropped bytes — a `u8`
/// compare with no allocation, against N heap `SharedString`s and N laid
/// out elements per paint, which is what it replaces.
pub fn window(text: &str) -> (&str, usize) {
    // `lines()` already ignores one trailing newline; drop it here too so
    // the two agree about how many lines there are.
    let body = text.strip_suffix('\n').unwrap_or(text);
    let bytes = body.as_bytes();
    let mut start = 0usize;
    let mut seen = 0usize;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\n' {
            seen += 1;
            if seen == WINDOW_LINES {
                start = i + 1;
                break;
            }
        }
    }
    if start == 0 {
        // Short enough to render whole. Hand back the ORIGINAL text, not
        // `body`: nothing was dropped, so nothing should look dropped.
        return (text, 0);
    }
    // Slide forward to the start of the next paragraph. `j` is always
    // just past a newline, so it is always a char boundary.
    let mut anchored = start;
    let mut j = start;
    let mut scanned = 0usize;
    while scanned < ANCHOR_LINES && j < bytes.len() {
        let eol = body[j..].find('\n').map_or(bytes.len(), |o| j + o);
        if body[j..eol].trim().is_empty() {
            anchored = (eol + 1).min(bytes.len());
            break;
        }
        j = eol + 1;
        scanned += 1;
    }
    // Every line that ends before the cut is a line the operator cannot
    // see, and `anchored` always sits just past a newline, so the two
    // counts are the same number.
    let elided = bytes[..anchored].iter().filter(|&&b| b == b'\n').count();
    (&body[anchored..], elided)
}

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
    let (text, elided) = window(text);
    let lines: Vec<SharedString> = if text.is_empty() {
        Vec::new()
    } else {
        text.lines()
            .map(|l| SharedString::from(l.to_string()))
            .collect()
    };
    // A window that says nothing is a window that lies: the box would
    // simply appear to begin where the thinking began. `… N more lines`
    // on a failed tool is the house form for this, and it counts lines
    // for the same reason — they are the thing that was dropped.
    let note = (elided > 0).then(|| {
        SharedString::from(format!(
            "… {elided} earlier line{}",
            if elided == 1 { "" } else { "s" }
        ))
    });
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
                                .children(note.map(|n| {
                                    div()
                                        .debug_selector(|| "reasoning-elided".into())
                                        .text_size(px(theme::SMALL_SIZE))
                                        .line_height(px(theme::SMALL_LINE))
                                        .text_color(ink)
                                        .child(n)
                                }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_thought_shorter_than_the_window_is_left_alone() {
        let t = numbered(WINDOW_LINES);
        assert_eq!(window(&t), (t.as_str(), 0));
    }

    #[test]
    fn an_empty_thought_windows_to_nothing() {
        assert_eq!(window(""), ("", 0));
    }

    #[test]
    fn a_trailing_newline_does_not_mint_an_empty_last_line() {
        let t = format!("{}\n", numbered(3));
        let (out, elided) = window(&t);
        assert_eq!(elided, 0);
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn a_long_thought_keeps_its_newest_lines_and_states_what_it_dropped() {
        let t = numbered(1_000);
        let (out, elided) = window(&t);
        assert_eq!(out.lines().count(), WINDOW_LINES);
        assert!(out.starts_with("line 920"), "got {:?}", &out[..16]);
        assert!(out.ends_with("line 999"));
        assert_eq!(elided, 920);
    }

    #[test]
    fn every_line_is_either_kept_or_counted() {
        for n in [WINDOW_LINES + 1, 5_000, 50_000] {
            let t = numbered(n);
            let (out, elided) = window(&t);
            assert!(out.lines().count() <= WINDOW_LINES, "unbounded at {n}");
            assert_eq!(out.lines().count() + elided, n, "lost a line at {n}");
        }
    }

    #[test]
    fn the_window_opens_on_a_paragraph_when_one_is_near() {
        let mut lines: Vec<String> = (0..1_000).map(|i| format!("line {i}")).collect();
        // Three lines into what the raw cut would keep, so the anchor is
        // inside its budget.
        let blank_at = 1_000 - WINDOW_LINES + 3;
        lines[blank_at] = String::new();
        let t = lines.join("\n");
        let (out, elided) = window(&t);
        assert!(
            out.starts_with(&format!("line {}", blank_at + 1)),
            "window opened mid-paragraph: {:?}",
            &out[..16]
        );
        assert_eq!(elided, blank_at + 1);
    }

    #[test]
    fn a_paragraph_break_too_deep_to_reach_is_not_chased() {
        let mut lines: Vec<String> = (0..1_000).map(|i| format!("line {i}")).collect();
        // Past the anchor budget: taking it would eat a quarter of the
        // window to tidy the first line.
        let blank_at = 1_000 - WINDOW_LINES + ANCHOR_LINES + 5;
        lines[blank_at] = String::new();
        let t = lines.join("\n");
        let (out, _) = window(&t);
        assert_eq!(out.lines().count(), WINDOW_LINES);
        assert!(out.starts_with("line 920"));
    }

    #[test]
    fn the_box_windows_and_says_so() {
        let src = include_str!("bezel_activity.rs");
        let prod = &src[..src
            .rfind("#[cfg(test)]")
            .expect("bezel_activity.rs has a test module")];
        assert_eq!(
            prod.matches("let (text, elided) = window(text);").count(),
            1,
            "the box should window its own text, once"
        );
        assert_eq!(
            prod.matches("reasoning-elided").count(),
            1,
            "a windowed trace must state what it dropped"
        );
    }
}
