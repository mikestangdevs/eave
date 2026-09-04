//! The overlay a page's popup window is drawn in — 06-browser **C2**.
//!
//! eave is one window and a page may not make it grow a second one (the C3
//! ruling). A popup is drawn as a **near-full overlay over the page that
//! opened it**, with three controls in a gutter down its right-hand side:
//! close, expand into a tab, and split beside the opener.
//! `web/popups.rs` holds why a popup is adopted rather than turned into a
//! tab; this file is only what it looks like.
//!
//! # It is the well, not a dialog, and that was a correction
//! The first cut was a 500×620 card centred over the page — the size the
//! popup had asked for, drawn like `page_dialog`'s prompt. The operator
//! called it a horrible experience on sight, and the reason is worth
//! keeping: **a popup is a PAGE, not a question.** A consent screen, a
//! payment form and a provider's sign-in are all documents somebody has
//! to read and work in, and a dialog-sized frame with a scrollbar in the
//! middle of a 1600 px window makes every one of them worse than the tab
//! it replaced. The reference is Arc's peek: the document takes the well,
//! the page it came from stays visible as a dimmed edge around it, and
//! the controls stand outside the frame where they cannot be mistaken for
//! part of the page.
//!
//! That also settles the size question the row started with. The page's
//! requested geometry is not used — see `popups::page_size`, which
//! records both that CEF 151 no longer reports it and that this design
//! does not want it.
//!
//! # Why this is its own file and not more of `ui.rs`
//! Nothing here needs a `Shell`: the popup is read from `web::popups` and
//! the three controls call into `web`. `ui.rs` is 32,000 lines and two
//! other lanes are living in it, so the mount there is a handful of lines
//! and the surface is here — the same split `composer.rs` already makes
//! for the pieces that can be named without a `Shell`.
//!
//! **One thing does not come along with the split**, and it is written
//! down rather than left to be discovered:
//! `ui_harness::every_clickable_element_says_it_is_clickable` counts click
//! handlers against cursor declarations in the source of `ui.rs`
//! **alone**, so a clickable moved out of that file leaves the accounting
//! silently. [`tests::every_click_here_says_it_is_clickable`] is that
//! property, re-stated for this file.

use gpui::{div, prelude::*, px, AnyElement, App, SharedString};

use crate::motion::HoverLift as _;
use crate::theme;
use crate::web::popups::{self, Popup, BOTTOM, GUTTER, SIDE, TOP};
use crate::{icons, web};

/// The control discs.
///
/// The composer's tool buttons are 26 px around an `ICON_SM`, and this is
/// the same object in a different place, so it is the same size. Smaller
/// is not available: 16 is the icon family's FLOOR (`theme::ICON_SM` — at
/// 14 px Solar Linear's stroke is a hairline that dissolves), so a
/// smaller disc would be a smaller ring around the same glyph.
const BUTTON: f32 = 26.0;

/// How far a control rises under the pointer.
const LIFT: f32 = 2.0;

/// The popup overlay over a tab's page, or `None` when that tab has none.
///
/// Mounted in the well beside the page it belongs to, so a popup on a
/// background tab draws nothing — an overlay over a page nobody is
/// looking at is an overlay over the wrong page.
/// `left` is where the content well starts in window space — the rail's
/// width, or zero when it is collapsed. **The dim is the window's and the
/// card is the well's**, which is the one thing this argument buys: the
/// overlay reaches the address row and both window margins, while the
/// document itself stays over the region it came from instead of sliding
/// half under the rail.
pub fn element(tab_id: &str, left: f32, cx: &mut App) -> Option<AnyElement> {
    let popup = popups::open_for(tab_id)?;
    Some(sheet(&popup, left, cx))
}

fn sheet(popup: &Popup, left: f32, cx: &mut App) -> AnyElement {
    let id = popup.id.clone();
    div()
        .debug_selector(|| "popup-scrim".into())
        .absolute()
        .inset_0()
        .flex()
        .flex_row()
        .items_center()
        // Half a gutter at each end, so the card and its controls are
        // centred in the well **as one object** — see `popups::GUTTER`.
        // The card then fills what is left, which is what makes this an
        // overlay rather than a dialog.
        .pt(px(TOP))
        .pb(px(BOTTOM))
        .pr(px(SIDE + GUTTER / 2.0))
        .pl(px(left + SIDE + GUTTER / 2.0))
        // **A dim, in a DARK ink.** `page_dialog` carries the measurement
        // this is written from: its scrim used to be `overlay_strongest()`
        // and dimmed nothing, because that token is a 20% LIGHT wash and
        // over a page it frosted rather than darkened. Its note ends "if a
        // dim ever comes back … it needs a dark ink" — this is that ink.
        // With the overlay taking nearly the whole well, what the dim
        // actually does is finish the edge: a hairline of dimmed page all
        // the way round, which is the only thing on screen saying this
        // belongs to the document underneath.
        .bg(theme::scrim())
        // G25, and it is INPUT rather than paint: `div().bg(..)` colours a
        // region without taking events, so without this a click in the
        // margin would land in the live page behind it. A popup asking for
        // a password over a page that can put content under where the
        // button will be is the clickjacking shape, and it is exactly the
        // hole `page_dialog` had.
        .occlude()
        .child(
            div()
                .debug_selector(|| "popup-card".into())
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(px(theme::RADIUS_PANEL))
                // The same material as the page dialog, which is the same
                // layer: a card over a live page. Only the header strip
                // ever shows it — the page covers the rest.
                .bg(theme::menu_glass())
                .border_1()
                .border_color(theme::edge_modal())
                .shadow(theme::shadow_modal())
                .child(
                    div()
                        .flex_1()
                        .w_full()
                        .overflow_hidden()
                        // `page_element` is `size_full`, and its own
                        // prepaint publishes these bounds as the browser's
                        // view — so the popup's page is resized to the
                        // card by the ordinary path, with no popup-shaped
                        // exception anywhere in `page.rs`.
                        .children(web::page_element(&id, cx)),
                ),
        )
        .child(controls(popup))
        .into_any_element()
}

/// The three ways out, in the gutter beside the card.
///
/// **Close is first**, at the top, nearest where a window's own close
/// would be and where the pointer already is after the card appears; the
/// two that keep the page follow it. Expand and split are absent — not
/// disabled — while the popup has committed nowhere, because both of them
/// open its current address and `about:blank` is not an address
/// (`popups::promote_url`).
fn controls(popup: &Popup) -> AnyElement {
    let keeps_the_page = popups::promote_url(popup).is_some();
    let (close_id, tab_id, split_id) = (
        popup.id.clone(),
        popup.id.clone(),
        popup.id.clone(),
    );
    div()
        .debug_selector(|| "popup-controls".into())
        .flex_none()
        .w(px(GUTTER))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(theme::SPACE_2))
        .pl(px(theme::SPACE_3))
        .child(
            button("popup-close", icons::CLOSE).on_click(move |_, window, _| {
                web::close_popup(&close_id);
                window.refresh();
            }),
        )
        .when(keeps_the_page, |d| {
            d.child(
                button("popup-expand", icons::EXPAND_ARROWS).on_click(move |_, window, _| {
                    web::promote_popup(&tab_id);
                    window.refresh();
                }),
            )
            .child(
                button("popup-split", icons::SPLIT_PANES).on_click(move |_, window, _| {
                    web::split_popup(&split_id);
                    window.refresh();
                }),
            )
        })
        .into_any_element()
}

/// A control in the gutter: a disc, because it stands on the dimmed page
/// rather than on a surface, and a disc is the shape that reads as a
/// control anywhere it lands.
///
/// # It rises under the pointer; it does not fade
/// Every other interactive wash in this shell blends its background
/// (`motion::HoverFade`), and that is right for something drawn ON a
/// surface. These are drawn on eave's dim over somebody else's page,
/// where a background blend reads as the control going translucent — as
/// if it were dissolving into the page rather than answering the pointer.
/// So it lifts 2 px and takes a shadow instead, animated on the same
/// `hover_t` clock as every fade (`motion::HoverLift`), because the
/// inventory rule against gpui's snapping `.hover(|s| ..)` is about the
/// snap, not about which property moves.
///
/// The ink and the disc are `scrim_*` tokens rather than `text()` and an
/// overlay wash: what they sit on is the same dark dim in both
/// appearances, so they are the same in both too.
fn button(selector: &'static str, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    let sel = selector.to_string();
    div()
        .id(SharedString::from(selector))
        .debug_selector(move || sel.clone())
        .flex_none()
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme::scrim_control())
        .cursor_pointer()
        .hover_lift(selector, LIFT, theme::shadow_lift)
        .child(icons::tagged_icon(
            glyph,
            // `ICON_MD`, not `ICON_SM`: the family's stroke is 1.5/24 of
            // whatever size it is drawn at, so 18 px is a 1.125 px stroke
            // where 16 px is 1.00 — the difference between a glyph that
            // holds its own on a dark disc and one that reads as a
            // hairline sketch of itself.
            theme::ICON_MD,
            theme::ink_on_scrim(),
            format!("{selector}-glyph"),
        ))
}

#[cfg(test)]
mod tests {
    /// `ui_harness`'s accounting stops at `ui.rs`, so this file keeps its
    /// own copy of the property: **every click handler states what the
    /// pointer becomes.** Both cursors count, for the reason the original
    /// gives — a field is typed into, not pressed.
    #[test]
    fn every_click_here_says_it_is_clickable() {
        let src = include_str!("popup_sheet.rs");
        // The counts are over the SURFACE, not over this test's own
        // string literals — which name every pattern it looks for.
        let src = &src[..src.find("#[cfg(test)]").expect("a test module")];
        let clicks = src.matches(".on_click(").count();
        // The controls share one constructor, so they carry one
        // `.cursor_pointer()` between them. Counting `fn button(` the way
        // the original counts `row_action(` — and, as there, only because
        // the constructor is checked to really set a cursor.
        let body = &src[src.find("fn button(").expect("the control constructor")..];
        assert!(
            body[..body.find("\n}\n").unwrap()].contains(".cursor_pointer()"),
            "the gutter's control constructor stopped stating its cursor"
        );
        // Every handler in this file hangs off that constructor, which is
        // what makes the check above cover all of them. A handler added to
        // a bare `div()` would break this equality rather than pass
        // unnoticed, which is the whole point.
        let sites = src.matches("button(\"popup").count();
        assert_eq!(
            clicks, sites,
            "a click handler in the popup overlay is not on a control that states its cursor"
        );
        assert!(clicks > 0, "the overlay has no controls at all any more");
    }
}
