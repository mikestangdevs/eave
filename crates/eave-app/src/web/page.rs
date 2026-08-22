//! The page, as an ordinary gpui element.
//!
//! This file is short, and that is the finding. A browser frame reaches the
//! screen through `Window::paint_surface` — the same primitive livekit video
//! uses — so it sorts with quads and text, is clipped by the content mask,
//! and is *seen by anything drawn after it*. Frosted chrome over a live web
//! page costs nothing here because it is not a special case: `BackdropBlur`
//! snapshots the framebuffer at its own draw order, and the page is already
//! in it.
//!
//! The one rule that is not obvious: **geometry is decided in `paint`, not
//! in `render`.** Anything computed in `render` from a fact that only exists
//! after layout is stale by a frame, and gpui will not repaint to correct it
//! — from gpui's side the layout change was already handled. That is not a
//! flicker; the wrong frame stays. So the size CEF is told, and the origin
//! input is measured against, are both published from here.

use std::cell::RefCell;
use std::collections::HashMap;

use cef::{ImplBrowser, ImplBrowserHost, MouseButtonType};
use gpui::{
    div, prelude::*, px, App, Bounds, Corners, Element, ElementId, FocusHandle, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, Window,
};

use super::{input, osr};
use crate::theme;

thread_local! {
    /// One focus handle per tab, kept across frames. A fresh handle every
    /// render would mean focus was lost on every frame — which is not a
    /// subtle bug, it is "typing does nothing".
    static FOCUS: RefCell<HashMap<String, FocusHandle>> = RefCell::new(HashMap::new());
}

fn focus_handle(tab_id: &str, cx: &mut App) -> FocusHandle {
    FOCUS.with(|f| {
        f.borrow_mut()
            .entry(tab_id.to_string())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    })
}

/// Window-space point to page space.
///
/// The element publishes its own origin at paint, so this subtraction is
/// the whole conversion — there is no title-bar offset to guess at, and
/// guessing one cost the spike a capture run.
fn to_page(tab_id: &str, at: Point<Pixels>) -> Point<Pixels> {
    let v = osr::view(tab_id);
    gpui::point(at.x - gpui::px(v.origin_x), at.y - gpui::px(v.origin_y))
}

/// The page for a tab, or `None` if that tab has no live browser.
///
/// A div carrying the input listeners, wrapping the surface. The listeners
/// are here rather than on the shell because every one of them needs the
/// element's own geometry to mean anything.
pub fn page_element(tab_id: &str, cx: &mut App) -> Option<gpui::AnyElement> {
    if !super::tabs::has_browser(tab_id) {
        return None;
    }
    let focus = focus_handle(tab_id, cx);
    let id = tab_id.to_string();

    let forward = |id: &str, f: &dyn Fn(&cef::Browser)| {
        super::tabs::with_browser(id, |b| f(b));
    };
    let _ = forward;

    let (down_id, up_id, move_id, wheel_id, key_id) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let down_focus = focus.clone();

    // What Chromium says the pointer should be over whatever is under it.
    // Read at render because that is where gpui takes it; kept fresh by the
    // pump's redraw gate, which now moves on a chrome change and not only on
    // a delivered frame (`host::refresh_if_painted`). Without that, hovering
    // a link on a page that is not repainting would set the cursor on the
    // NEXT mouse move instead of this one.
    let cursor = super::tabs::chrome(tab_id)
        .map(|c| c.cursor)
        .unwrap_or_default();

    Some(
        div()
            .size_full()
            .cursor(cursor)
            .track_focus(&focus)
            .on_mouse_down(gpui::MouseButton::Left, move |ev, window, cx| {
                // Focus first. A page that has never been clicked receives
                // no key events at all — proven in the spike, where a
                // keys-only run produced zero and the identical run after
                // one click produced all of them.
                window.focus(&down_focus, cx);
                super::tabs::with_browser(&down_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&down_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::LEFT,
                        false,
                        ev.click_count as i32,
                    );
                    input::focus(b, true);
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            .on_mouse_up(gpui::MouseButton::Left, move |ev, _window, _cx| {
                super::tabs::with_browser(&up_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&up_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::LEFT,
                        true,
                        ev.click_count as i32,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            .on_mouse_move(move |ev, _window, _cx| {
                super::tabs::with_browser(&move_id, |b| {
                    // A move with the button held is a DRAG. CEF has no
                    // separate drag event — the held button rides the same
                    // modifier bitfield as shift — and omitting it is why a
                    // naive port can click and scroll but never select text.
                    let held = ev.pressed_button == Some(gpui::MouseButton::Left);
                    input::mouse_move(
                        b,
                        to_page(&move_id, ev.position),
                        &ev.modifiers,
                        held,
                        false,
                    );
                });
            })
            .on_scroll_wheel(move |ev, window, _cx| {
                super::tabs::with_browser(&wheel_id, |b| {
                    let d = ev.delta.pixel_delta(window.line_height());
                    input::wheel(
                        b,
                        to_page(&wheel_id, ev.position),
                        &ev.modifiers,
                        f32::from(d.x) as i32,
                        f32::from(d.y) as i32,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            .on_key_down(move |ev, _window, _cx| {
                super::tabs::with_browser(&key_id, |b| {
                    input::key(b, &ev.keystroke);
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            .child(PageSurface { tab_id: id })
            .into_any_element(),
    )
}

struct PageSurface {
    tab_id: String,
}

impl IntoElement for PageSurface {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PageSurface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        // The page is exactly the size of the space it was given, and the
        // origin published here is what makes input coordinates exact: page
        // space and element space become the same space, so there is no
        // offset to guess at. (A guessed 28 pt title-bar offset cost the
        // spike a capture run.)
        let view = osr::View {
            width: f32::from(bounds.size.width).round() as i32,
            height: f32::from(bounds.size.height).round() as i32,
            scale: window.scale_factor(),
            origin_x: f32::from(bounds.origin.x),
            origin_y: f32::from(bounds.origin.y),
        };
        if osr::set_view(&self.tab_id, view) {
            super::tabs::with_browser(&self.tab_id, |b| {
                if let Some(host) = b.host() {
                    host.was_resized();
                }
            });
        }

        if let Some(buffer) = osr::frame(&self.tab_id) {
            // Bottom corners match the card; the header owns the top of
            // the card so the page's top edge is square against it.
            window.paint_surface_with_corners(
                bounds,
                buffer,
                Corners {
                    top_left: px(0.),
                    top_right: px(0.),
                    bottom_right: px(theme::RADIUS_PANEL),
                    bottom_left: px(theme::RADIUS_PANEL),
                },
            );
        }

        // A `<select>` dropdown, an autofill list, a colour picker.
        //
        // Chromium paints these as a SECOND surface with its own size and
        // its own position inside the view, and eave used to drop them —
        // so every `<select>` on the web opened a menu that could not be
        // seen and still ate the next click. Painted after the page and at
        // the rect CEF gave, which is already in view coordinates, so the
        // element's own origin is the only conversion.
        //
        // Square corners, deliberately: this is a menu sitting ON the page,
        // not the page itself, and rounding it to the card's radius would
        // round a widget that Chromium already drew its own shadow and
        // border into.
        if let Some((buffer, rect)) = osr::popup(&self.tab_id) {
            let at = Bounds {
                origin: gpui::point(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
                size: gpui::size(px(rect.width), px(rect.height)),
            };
            window.paint_surface(at, buffer);
        }
    }
}
