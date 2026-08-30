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

/// The window's other bound, and the one a live walk proved was needed.
///
/// **A line bound alone does not bound anything.** Measured 2026-08-30: a
/// Hermes turn's entire reasoning arrived as ONE unbroken line of 249
/// characters, and `lines()` counts a 50 000-character paragraph as one
/// line just the same — so a lines-only window would hand the whole thing
/// to the layout engine and call itself bounded. One element is cheap;
/// shaping 50 000 characters of text is not.
///
/// 8 KiB is far more than [`WINDOW_LINES`] of ordinary prose, so on a
/// line-broken trace this never binds and the line rule decides. It exists
/// for the shape that has no lines to cut on.
const WINDOW_BYTES: usize = 8 * 1024;

/// What [`window`] kept, and what it had to drop to keep it.
pub struct Window<'a> {
    pub text: &'a str,
    /// Lines dropped off the front. Zero when nothing was dropped.
    pub elided_lines: usize,
    /// The cut landed INSIDE a line, because the trace had no line break
    /// to cut on. The note must not state a line count for this —
    /// [`reasoning_box`] drops the number rather than print one that is
    /// nearly right.
    pub mid_line: bool,
}

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
pub fn window(text: &str) -> Window<'_> {
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
    if start > 0 {
        // Slide forward to the start of the next paragraph. `j` is always
        // just past a newline, so it is always a char boundary.
        let mut j = start;
        let mut scanned = 0usize;
        while scanned < ANCHOR_LINES && j < bytes.len() {
            let eol = body[j..].find('\n').map_or(bytes.len(), |o| j + o);
            if body[j..eol].trim().is_empty() {
                start = (eol + 1).min(bytes.len());
                break;
            }
            j = eol + 1;
            scanned += 1;
        }
    }
    // The byte bound. Applied AFTER the line rule so a line-broken trace
    // is decided by lines and this never binds; it catches the trace with
    // no lines to cut on.
    let mut mid_line = false;
    if bytes.len() - start > WINDOW_BYTES {
        let mut cut = bytes.len() - WINDOW_BYTES;
        while cut < bytes.len() && !body.is_char_boundary(cut) {
            cut += 1;
        }
        // Prefer a line boundary if one is close: a window that opens on a
        // whole line is worth a little of the budget. Only if there is
        // none do we admit to cutting mid-line.
        match body[cut..].find('\n').filter(|o| *o <= WINDOW_BYTES / 8) {
            Some(o) => cut += o + 1,
            None => mid_line = true,
        }
        start = start.max(cut);
    }
    if start == 0 {
        // Short enough to render whole. Hand back the ORIGINAL text, not
        // `body`: nothing was dropped, so nothing should look dropped.
        return Window {
            text,
            elided_lines: 0,
            mid_line: false,
        };
    }
    // Every line that ends before the cut is a line the operator cannot
    // see. When the cut is mid-line this undercounts by the partial one,
    // which is exactly why `mid_line` suppresses the count rather than
    // printing a number that is nearly right.
    let elided_lines = bytes[..start].iter().filter(|&&b| b == b'\n').count();
    Window {
        text: &body[start..],
        elided_lines,
        mid_line,
    }
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
    let Window {
        text,
        elided_lines,
        mid_line,
    } = window(text);
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
    let note = if mid_line {
        // No line count, because the cut was not on a line. Claiming
        // `N earlier lines` here would be a number that is nearly right,
        // which is worse than no number.
        Some(SharedString::from("… earlier reasoning"))
    } else if elided_lines > 0 {
        Some(SharedString::from(format!(
            "… {elided_lines} earlier line{}",
            if elided_lines == 1 { "" } else { "s" }
        )))
    } else {
        None
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
        let w = window(&t);
        assert_eq!(w.text, t.as_str());
        assert_eq!(w.elided_lines, 0);
        assert!(!w.mid_line);
    }

    #[test]
    fn an_empty_thought_windows_to_nothing() {
        let w = window("");
        assert_eq!(w.text, "");
        assert_eq!(w.elided_lines, 0);
    }

    #[test]
    fn a_trailing_newline_does_not_mint_an_empty_last_line() {
        let t = format!("{}\n", numbered(3));
        let w = window(&t);
        assert_eq!(w.elided_lines, 0);
        assert_eq!(w.text.lines().count(), 3);
    }

    #[test]
    fn a_long_thought_keeps_its_newest_lines_and_states_what_it_dropped() {
        let t = numbered(1_000);
        let w = window(&t);
        assert_eq!(w.text.lines().count(), WINDOW_LINES);
        assert!(w.text.starts_with("line 920"), "got {:?}", &w.text[..16]);
        assert!(w.text.ends_with("line 999"));
        assert_eq!(w.elided_lines, 920);
        assert!(!w.mid_line, "a line-broken trace cuts on a line");
    }

    #[test]
    fn every_line_is_either_kept_or_counted() {
        for n in [WINDOW_LINES + 1, 5_000, 50_000] {
            let t = numbered(n);
            let w = window(&t);
            assert!(w.text.lines().count() <= WINDOW_LINES, "unbounded at {n}");
            assert_eq!(
                w.text.lines().count() + w.elided_lines,
                n,
                "lost a line at {n}"
            );
        }
    }

    #[test]
    fn the_window_opens_on_a_paragraph_when_one_is_near() {
        let mut lines: Vec<String> = (0..1_000).map(|i| format!("line {i}")).collect();
        let blank_at = 1_000 - WINDOW_LINES + 3;
        lines[blank_at] = String::new();
        let t = lines.join("\n");
        let w = window(&t);
        assert!(
            w.text.starts_with(&format!("line {}", blank_at + 1)),
            "window opened mid-paragraph: {:?}",
            &w.text[..16]
        );
        assert_eq!(w.elided_lines, blank_at + 1);
    }

    #[test]
    fn a_paragraph_break_too_deep_to_reach_is_not_chased() {
        let mut lines: Vec<String> = (0..1_000).map(|i| format!("line {i}")).collect();
        let blank_at = 1_000 - WINDOW_LINES + ANCHOR_LINES + 5;
        lines[blank_at] = String::new();
        let t = lines.join("\n");
        let w = window(&t);
        assert_eq!(w.text.lines().count(), WINDOW_LINES);
        assert!(w.text.starts_with("line 920"));
    }

    /// **The case a live walk proved was real.** A Hermes turn's whole
    /// reasoning arrived as ONE unbroken line, and `lines()` counts a huge
    /// paragraph as one line just the same — so a lines-only window would
    /// hand the whole thing to the layout engine and call itself bounded.
    #[test]
    fn one_enormous_line_is_still_bounded() {
        let t = "x".repeat(200_000);
        let w = window(&t);
        assert!(
            w.text.len() <= WINDOW_BYTES,
            "a single line escaped the window: {} bytes",
            w.text.len()
        );
        assert!(w.mid_line, "there was no line boundary to cut on");
        assert!(w.text.ends_with('x'), "the window keeps the NEWEST text");
    }

    /// The count is suppressed rather than printed nearly-right.
    #[test]
    fn a_mid_line_cut_states_no_line_count() {
        let t = "y".repeat(100_000);
        let w = window(&t);
        assert!(w.mid_line);
        // The renderer keys the note off `mid_line` first; this pins that
        // there is something to key off.
        assert_eq!(
            w.elided_lines, 0,
            "no whole line was dropped — there are none"
        );
    }

    /// A line-broken trace is decided by the LINE rule; the byte bound
    /// exists for the shape that has no lines, and must not bind here.
    #[test]
    fn the_byte_bound_does_not_bind_on_ordinary_prose() {
        let t = numbered(1_000);
        let w = window(&t);
        assert!(!w.mid_line);
        assert_eq!(w.text.lines().count(), WINDOW_LINES);
        assert!(w.text.len() < WINDOW_BYTES, "80 lines of prose should fit");
    }

    /// A multi-byte character must not be split.
    #[test]
    fn the_byte_cut_lands_on_a_character_boundary() {
        let t = "é".repeat(100_000);
        let w = window(&t);
        assert!(w.text.len() <= WINDOW_BYTES);
        assert!(
            w.text.starts_with('é'),
            "cut through a multi-byte character"
        );
    }

    #[test]
    fn the_box_windows_and_says_so() {
        let src = include_str!("bezel_activity.rs");
        let prod = &src[..src
            .rfind("#[cfg(test)]")
            .expect("bezel_activity.rs has a test module")];
        assert_eq!(
            prod.matches("} = window(text);").count(),
            1,
            "the box should window its own text, once"
        );
        assert_eq!(
            prod.matches("reasoning-elided").count(),
            1,
            "a windowed trace must state what it dropped"
        );
        // Both shapes of drop have to be sayable: a line count when the
        // cut was on a line, and no count when it was not.
        assert_eq!(
            prod.matches("… earlier reasoning").count(),
            1,
            "a mid-line cut has no honest line count to state"
        );
        assert!(
            prod.contains("earlier line{}"),
            "a whole-line drop should still state how many"
        );
    }
}
