//! Composer chrome helpers — MonoCode's card, in eave tokens.
//!
//! Layout lives in `ui.rs` because the card is a projection of session
//! state. This file is the pieces that can be named without a `Shell`:
//! placeholders, tool buttons, the send/stop pair, the context meter.

use eave_core::events::ContextWindow;
use gpui::{div, prelude::*, px, AnyElement, Hsla, SharedString, Stateful};

use crate::icons;
use crate::motion::HoverFade as _;
use crate::theme;

/// Docked composer. MonoCode's field placeholder.
pub const PLACEHOLDER_DOCKED: &str = "Ask, build, / for skills, @ for references…";
/// Empty-session cluster. MonoCode's `shell` placeholder.
pub const PLACEHOLDER_EMPTY: &str = "How can I help you today?";
/// Drop overlay. MonoCode's `Drop files to attach`.
#[allow(dead_code)]
pub const DROP_LABEL: &str = "Drop files to attach";

/// 26 px tool / send / stop — MonoCode `size-6.5`.
pub const TOOL: f32 = 26.0;
/// MonoCode `rounded-lg` on the card.
pub const CARD_RADIUS: f32 = theme::RADIUS_SELECT;
/// Tool button corner — MonoCode `rounded-md`.
pub const TOOL_RADIUS: f32 = theme::RADIUS_BUTTON;

/// A 6.5-size square control. `selector` is both id and debug probe.
#[allow(dead_code)]
pub fn tool_button(
    selector: &'static str,
    glyph: &'static str,
    ink: Hsla,
) -> Stateful<gpui::Div> {
    let sel = selector.to_string();
    div()
        .id(SharedString::from(selector))
        .debug_selector(move || sel.clone())
        .flex_none()
        .size(px(TOOL))
        .rounded(px(TOOL_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .hover_fade(selector, theme::wash_rest(), theme::overlay())
        .cursor_pointer()
        .child(icons::tagged_icon(
            glyph,
            theme::ICON_SM,
            ink,
            format!("{selector}-glyph"),
        ))
}

// **`send_button` and `stop_button` used to live here and are deleted.**
//
// Both were callerless, and both are the reason a dead constructor is not
// merely untidy: each carried the LIVE control's `debug_selector`s — `send`
// / `send-glyph` and `composer-stop` / `composer-stop-glyph`, the same
// literals `ui.rs` paints — so calling either would have put two elements
// on screen under one selector. `debug_bounds` resolves a selector to the
// LAST-PAINTED element, so the harness would have found one, clicked one,
// received the command it expected, and gone green over a composer with two
// send buttons. AG1's gap table reported `stop_button`; `send_button` was
// the same defect standing beside it and is deleted for the same reason.
//
// The live pair is inline in `ui.rs` (the stop control at its
// `session_busy` gate, the send button beside it), which is also where
// `attach_button` lives — so this file is not where composer controls are
// constructed, and a copy here was never going to be the one that ships.
// `every_clickable_element_says_it_is_clickable` counts cursors in `ui.rs`
// alone, which is the mechanical reason the live constructors belong there.

/// Two-line hover copy for the context meter.
///
/// **Wired 2026-08-29.** This was written, unit-tested, and never called
/// from anything that paints — it showed up as a live `never used` warning
/// in every build, which is where the evidence had been sitting. The
/// question the gap table left open was whether it had been superseded by
/// [`context_meter`]; it had not. The meter renders a ring and `69%` and
/// says nothing about the two numbers that produced it, so the copy was not
/// redundant — it was missing.
pub fn meter_copy(context: ContextWindow) -> (String, String) {
    let used = compact(context.used_tokens);
    match (context.fraction(), context.limit_tokens) {
        (Some(r), Some(limit)) => (
            format!("{}% context used", (r * 100.0).round() as i32),
            format!("{} / {} tokens", used, compact(limit)),
        ),
        _ => ("Context used".into(), format!("{used} tokens")),
    }
}

fn compact(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=99_999 => format!("{:.1}k", n as f64 / 1_000.0),
        100_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Ring colour: quiet, then warning at 75%, danger at 90%.
pub fn meter_ink(ratio: f32) -> Hsla {
    if ratio >= 0.90 {
        theme::danger()
    } else if ratio >= 0.75 {
        theme::warning()
    } else {
        theme::text_tertiary()
    }
}

/// What the context meter says when the pointer rests on it.
///
/// A view because that is what `.tooltip()` takes — gpui owns the delay,
/// the placement and the dismissal, which is most of what a tooltip is.
/// Same plate as `PageTooltip` in `ui.rs`, two lines instead of one.
struct MeterTip {
    headline: SharedString,
    detail: SharedString,
}

impl gpui::Render for MeterTip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .px(px(theme::SPACE_2))
            .py(px(theme::SPACE_1))
            .rounded(px(theme::RADIUS_SELECT))
            .bg(theme::menu_glass())
            .border_1()
            .border_color(theme::border())
            .shadow(theme::shadow_panel())
            .flex()
            .flex_col()
            .child(
                div()
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .text_color(theme::text())
                    .child(self.headline.clone()),
            )
            .child(
                div()
                    .text_size(px(theme::MICRO_SIZE))
                    .line_height(px(theme::MICRO_LINE))
                    .text_color(theme::text_tertiary())
                    .child(self.detail.clone()),
            )
    }
}

/// Context meter. Renders nothing without a stated ceiling — a number
/// guessing at a window is worse than no meter (MonoCode's rule, and
/// Slice CC's).
///
/// The ring and the percent are the glance; the exact pair is one hover
/// away through [`meter_copy`]. `69%` is the right thing to read across a
/// composer and the wrong thing to be told when you are deciding whether to
/// compact, which wants `176k / 256k`.
pub fn context_meter(context: Option<ContextWindow>) -> Option<AnyElement> {
    let context = context?;
    let ratio = context.fraction()?;
    let pct = (ratio * 100.0).round() as i32;
    let (headline, detail) = meter_copy(context);
    let headline = SharedString::from(headline);
    let detail = SharedString::from(detail);
    Some(
        div()
            .id("composer-meter")
            .debug_selector(|| "composer-meter".into())
            .tooltip(move |_, cx| {
                let headline = headline.clone();
                let detail = detail.clone();
                cx.new(|_| MeterTip { headline, detail }).into()
            })
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::SPACE_1))
            .child(
                div()
                    .size(px(14.0))
                    .rounded_full()
                    .border_1()
                    .border_color(meter_ink(ratio))
                    .bg(meter_ink(ratio).opacity((ratio * 0.55).clamp(0.08, 0.55))),
            )
            .child(
                div()
                    .text_size(px(theme::MICRO_SIZE))
                    .line_height(px(theme::MICRO_LINE))
                    .text_color(meter_ink(ratio))
                    .child(SharedString::from(format!("{pct}%"))),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_copy_states_the_percent_from_the_window_not_a_guess() {
        let c = ContextWindow {
            used_tokens: 176_000,
            limit_tokens: Some(256_000),
        };
        let (headline, detail) = meter_copy(c);
        assert_eq!(headline, "69% context used");
        assert!(
            detail.contains("176k") && detail.contains("256k"),
            "detail should name both halves: {detail}"
        );
        let no_ceiling = ContextWindow {
            used_tokens: 12,
            limit_tokens: None,
        };
        assert_eq!(no_ceiling.fraction(), None);
        let (h, d) = meter_copy(no_ceiling);
        assert_eq!(h, "Context used");
        assert_eq!(d, "12 tokens");
    }

    /// **The copy above reaches a screen.**
    ///
    /// A structural check on the source, for the reason
    /// `every_clickable_element_says_it_is_clickable` gives for being one:
    /// gpui owns the tooltip's hover delay and shows it in its own
    /// deferred pass, so there is no element in a painted frame to assert
    /// against. What CAN be pinned is the property that actually decayed —
    /// this function was written, tested, and called by nothing that
    /// paints, and the suite above went green the whole time because a
    /// unit test of a pure function cannot see that nobody calls it.
    ///
    /// The two halves are checked separately on purpose: `context_meter`
    /// could keep its tooltip and stop building it from `meter_copy`, and
    /// then this file would be back where it started with a second copy of
    /// the words.
    #[test]
    fn the_meters_hover_copy_is_actually_hung_on_the_meter() {
        let src = include_str!("composer.rs");
        let body = {
            let from = &src[src.find("pub fn context_meter(").unwrap()..];
            &from[..from.find("\n}\n").unwrap()]
        };
        assert!(
            body.contains(".tooltip("),
            "the context meter lost its hover copy and `meter_copy` is dead again"
        );
        assert!(
            body.contains("meter_copy(context)"),
            "the meter grew a tooltip that does not read from `meter_copy`, \
             so the words are written twice"
        );
    }

    /// The dead twins are gone and must stay gone: each carried a LIVE
    /// control's `debug_selector`, so calling one painted two elements
    /// under one selector and `debug_bounds` — which resolves to the
    /// last-painted — would have found one, clicked one, and passed.
    #[test]
    fn no_composer_control_here_claims_a_live_selector() {
        let src = include_str!("composer.rs");
        for stolen in [
            "\"send\".into()",
            "\"send-glyph\"",
            "\"composer-stop\".into()",
            "\"composer-stop-glyph\"",
        ] {
            assert!(
                !src.contains(stolen),
                "{stolen} is painted from two files again — the harness \
                 cannot tell the copies apart"
            );
        }
    }

    #[test]
    fn meter_ink_warns_then_alarms() {
        assert_eq!(meter_ink(0.2), theme::text_tertiary());
        assert_eq!(meter_ink(0.75), theme::warning());
        assert_eq!(meter_ink(0.91), theme::danger());
    }

    #[test]
    fn placeholders_are_monocode_s() {
        assert!(PLACEHOLDER_DOCKED.contains("/ for skills"));
        assert!(PLACEHOLDER_DOCKED.contains("@ for references"));
        assert_eq!(PLACEHOLDER_EMPTY, "How can I help you today?");
    }
}
