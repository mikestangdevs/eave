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

/// Send: solid when armed, quiet when not. Square, not a stadium.
#[allow(dead_code)]
pub fn send_button(has_draft: bool) -> Stateful<gpui::Div> {
    div()
        .id("send")
        .debug_selector(|| "send".into())
        .flex_none()
        .size(px(TOOL))
        .rounded(px(TOOL_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .hover_fade(
            "send",
            if has_draft {
                theme::solid()
            } else {
                theme::overlay_strong()
            },
            if has_draft {
                theme::inverse_hover()
            } else {
                theme::overlay_strongest()
            },
        )
        .cursor_pointer()
        .child(icons::tagged_icon(
            icons::ARROW_UP,
            theme::ICON_SM,
            if has_draft {
                theme::on_solid()
            } else {
                theme::text_secondary()
            },
            "send-glyph",
        ))
}

/// Stop: a filled square in the armed send plate. MonoCode's Square glyph.
#[allow(dead_code)]
pub fn stop_button() -> Stateful<gpui::Div> {
    div()
        .id("composer-stop")
        .debug_selector(|| "composer-stop".into())
        .flex_none()
        .size(px(TOOL))
        .rounded(px(TOOL_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::solid())
        .hover_fade("composer-stop", theme::solid(), theme::inverse_hover())
        .cursor_pointer()
        .child(
            div()
                .size(px(10.0))
                .rounded(px(2.0))
                .bg(theme::on_solid())
                .debug_selector(|| "composer-stop-glyph".into()),
        )
}

/// Two-line hover copy for the context meter. `None` when the provider
/// stated nothing — the meter itself is omitted then.
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

/// Context meter. Renders nothing without a stated ceiling — a number
/// guessing at a window is worse than no meter (MonoCode's rule, and
/// Slice CC's).
pub fn context_meter(context: Option<ContextWindow>) -> Option<AnyElement> {
    let context = context?;
    let ratio = context.fraction()?;
    let pct = (ratio * 100.0).round() as i32;
    Some(
        div()
            .id("composer-meter")
            .debug_selector(|| "composer-meter".into())
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
