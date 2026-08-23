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
    div, prelude::*, px, size, App, Bounds, ContentMask, Corners, Element, ElementId, FocusHandle,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Point, Window,
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
    let (right_down_id, right_up_id) = (id.clone(), id.clone());
    let (mid_down_id, mid_up_id) = (id.clone(), id.clone());
    let down_focus = focus.clone();
    let right_focus = focus.clone();

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
            // ── THE RIGHT BUTTON (Slice E1) ──────────────────────────
            //
            // Until this slice `page_element` forwarded LEFT and nothing
            // else, so a right-click on a page never reached Chromium at
            // all. That is half of why the context menu did nothing; the
            // other half was that no `CefContextMenuHandler` was supplied
            // (`web/menu.rs`). Either absence alone produces the same
            // symptom, which is why fixing one and testing would have read
            // as "still broken".
            //
            // Both edges are sent. Blink raises `contextmenu` on the DOWN
            // edge, so the UP is not what opens the menu — but a page that
            // sees a button go down and never come up is a page holding a
            // press forever, and `mousedown` handlers that expect a pair
            // (a canvas, a map, a custom control) are left mid-gesture.
            .on_mouse_down(gpui::MouseButton::Right, move |ev, window, cx| {
                // Focus first, exactly as the left button does: the menu is
                // about this page, and what follows it — Copy, ⌘F — is
                // keyboard work on a surface that has to own the keyboard.
                window.focus(&right_focus, cx);
                super::tabs::with_browser(&right_down_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&right_down_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::RIGHT,
                        false,
                        ev.click_count as i32,
                    );
                    input::focus(b, true);
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            .on_mouse_up(gpui::MouseButton::Right, move |ev, _window, _cx| {
                super::tabs::with_browser(&right_up_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&right_up_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::RIGHT,
                        true,
                        ev.click_count as i32,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            // ── THE MIDDLE BUTTON (G3) ───────────────────────────────
            //
            // Open-link-in-new-tab is muscle memory, and until this it did
            // nothing on a page — for the same two-absences-one-symptom
            // reason the right button had: `page_element` forwarded LEFT
            // and RIGHT and nothing else, AND no `on_open_urlfrom_tab` was
            // supplied (`tabs.rs`). Either gap alone gives a middle-click
            // that silently does nothing, so fixing one and testing would
            // have read as "still broken".
            //
            // **No focus grab.** Middle-clicking a link is not a claim on
            // the keyboard — the operator has said "keep this, I am still
            // reading here", and stealing focus to the page they are
            // deliberately not moving to would be the wrong half of the
            // gesture. The right and left buttons focus; this one does not.
            .on_mouse_down(gpui::MouseButton::Middle, move |ev, _window, _cx| {
                super::tabs::with_browser(&mid_down_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&mid_down_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::MIDDLE,
                        false,
                        ev.click_count as i32,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            // Both edges, for the reason the right button states: Blink
            // raises `auxclick` on the UP edge, so omitting it would send a
            // press that never ends and no click at all.
            .on_mouse_up(gpui::MouseButton::Middle, move |ev, _window, _cx| {
                super::tabs::with_browser(&mid_up_id, |b| {
                    input::mouse_button(
                        b,
                        to_page(&mid_up_id, ev.position),
                        &ev.modifiers,
                        MouseButtonType::MIDDLE,
                        true,
                        ev.click_count as i32,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
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
                // **Not while the find bar is up, and the capture rig is
                // what found this.** gpui dispatches key events leaf-first
                // on the bubble phase, so this listener runs BEFORE
                // `Shell::on_key` — and neither stops the other. Typing
                // `eave` into the find bar therefore also typed it into
                // the document: harmless-looking on prose, except that
                // space scrolls, `/` opens a site's own find, and any
                // focused field on the page silently collects the query.
                //
                // The bar is a surface eave owns over a page it does not,
                // and while it is up it owns the text. Everything else
                // still reaches the page — see `Shell::find_key`, which
                // deliberately answers `false` for the keys that scroll.
                if super::find_is_open(&key_id) {
                    return;
                }
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

thread_local! {
    /// How many frames in a row this tab has been painted at a size that is
    /// not the size it was asked for, and when that run started.
    ///
    /// **Both numbers, because neither is the whole answer.** Frames are
    /// what the artefact is made of; milliseconds are how long it is on
    /// screen. They come apart precisely where this slice's fix lives: the
    /// pump kick does not reduce the number of wrong-size *paints* — a
    /// window that jumps to a new size paints once at the old frame either
    /// way — it reduces how long that one paint stays up, because nothing
    /// asks gpui to redraw until a new frame arrives and nothing produces a
    /// new frame until CEF is pumped. A page at `QUIET_MS` is pumped every
    /// 100 ms; one that has just been told it resized is pumped every 8.
    static STALE: RefCell<HashMap<String, (u32, std::time::Instant)>> =
        RefCell::new(HashMap::new());
}

/// Count, and narrate, the frames spent at the wrong size.
///
/// **This is the slice's measurement, not decoration.** "Resize is smooth
/// now" is not a claim a screenshot can settle: the artefact is one or two
/// frames long, and a shutter that takes 200 ms to open will always find
/// the steady state. What CAN be measured is how many frames were painted
/// before Chromium's answer arrived, and that number is the whole
/// difference the pump kick makes — a page at `QUIET_MS` waits up to 100 ms
/// (six frames at 60) to be pumped, and one at `BUSY_MS` waits eight.
///
/// Under `EAVE_WEB_STRETCH=1` the counter still runs and still reports, so
/// the before and the after are the same instrument on the same binary.
/// The only difference is what those frames LOOKED like: scaled, or drawn
/// at one device pixel per device pixel and clipped.
fn note_fit(tab_id: &str, fits: bool, fw: f32, fh: f32, want_w: f32, want_h: f32) {
    if !super::host::traced() {
        return;
    }
    STALE.with(|s| {
        let mut s = s.borrow_mut();
        let entry = s
            .entry(tab_id.to_string())
            .or_insert((0, std::time::Instant::now()));
        if fits {
            if entry.0 > 0 {
                eprintln!(
                    "[web] resize settled at {want_w:.0}x{want_h:.0} after {} stale frame(s), {} ms",
                    entry.0,
                    entry.1.elapsed().as_millis()
                );
                entry.0 = 0;
            }
        } else {
            if entry.0 == 0 {
                entry.1 = std::time::Instant::now();
                eprintln!(
                    "[web] resize wants {want_w:.0}x{want_h:.0}, frame is {fw:.0}x{fh:.0}"
                );
            }
            entry.0 += 1;
        }
    });
}

/// `EAVE_WEB_STRETCH=1` — put the resize behaviour back the way it was
/// before Slice E1, for the length of one measurement.
///
/// **A knob rather than an edit, and the reason is C7's.** That slice found
/// a comment in `host.rs` that had been wrong for two slices because
/// re-checking it required changing the code, and kept
/// `EAVE_WEB_NO_HEARTBEAT` so the next person could re-measure instead of
/// re-reasoning. This is the same shape: with it set, the stale frame is
/// scaled into the new bounds and no pump is asked for on a view change —
/// exactly what `a4b3983` did — so the capture rig can photograph the
/// before and the after from **one binary** rather than asking a reader to
/// trust that two builds differed only here.
///
/// Read once. It is consulted from `paint`, which runs at frame rate.
fn stretch_for_measurement() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("EAVE_WEB_STRETCH").is_ok())
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
        let scale = window.scale_factor();
        let view = osr::View {
            width: f32::from(bounds.size.width).round() as i32,
            height: f32::from(bounds.size.height).round() as i32,
            scale,
            origin_x: f32::from(bounds.origin.x),
            origin_y: f32::from(bounds.origin.y),
        };
        let change = osr::set_view(&self.tab_id, view);
        if change != osr::ViewChange::Same {
            super::tabs::with_browser(&self.tab_id, |b| {
                if let Some(host) = b.host() {
                    // **`NotifyScreenInfoChanged` first, then `WasResized`,
                    // and one does not stand in for the other.**
                    // `WasResized` re-asks `GetViewRect` and nothing else;
                    // the device scale factor is answered by
                    // `GetScreenInfo`, which CEF caches and re-reads only
                    // when told to. Sending only `WasResized` after a drag
                    // to a display with a different backing scale leaves the
                    // page laid out at the right logical size and rasterised
                    // at the old resolution — a soft page, and no error
                    // anywhere.
                    if change == osr::ViewChange::Rescaled {
                        host.notify_screen_info_changed();
                    }
                    host.was_resized();
                }
            });
            // **Belt and braces, and the measurement says so.** The
            // reasoning for asking is sound on its face: under an external
            // pump nothing in Chromium runs until we run it, and a page
            // that is merely open is pumped at `QUIET_MS` — 100 ms, or six
            // frames at 60 — so the correctly-sized frame could arrive a
            // tenth of a second after the window had already moved.
            //
            // It does not. `scripts/e1-resize-sweep.sh` runs the same sweep
            // with this branch and with `EAVE_WEB_STRETCH=1` (which skips
            // these two lines, among others) and both settle in **one
            // stale frame, 8–24 ms** — with 2 s between resizes, which is
            // five times `BUSY_TAIL_MS`, so the busy tail had long expired.
            // The reason is that `WasResized` queues work inside Chromium
            // and Chromium then raises its own `OnScheduleMessagePumpWork`
            // — which is not suppressed here, because this is not inside a
            // `CefDoMessageLoopWork` call. CEF asks before we would have.
            //
            // Kept because it costs a channel send and it does not depend
            // on Chromium's scheduler staying helpful. **Stated as measured
            // rather than as the fix**, so the next reader does not quote
            // it as the reason resizes are prompt: they were already.
            if !stretch_for_measurement() {
                super::host::note_activity();
                super::host::request_pump();
            }
        }

        if let Some(frame) = osr::frame(&self.tab_id) {
            // ── THE RESIZE TRANSIENT (Slice E1) ──────────────────────────
            //
            // `paint_surface` scales its buffer to fill the bounds it is
            // given, which is right in the steady state and a **stretch** in
            // every frame between "the window changed size" and "Chromium
            // answered". A page is not a video: it has a layout, and a
            // layout stretched horizontally is not a smaller rendering of
            // the same thing, it is the wrong picture.
            //
            // So: paint the stale frame at one device pixel per device
            // pixel, anchored top-left, clipped to the space the element
            // actually has. Growing, the new strip of the well shows what is
            // under it for a frame or two; shrinking, the overhang is
            // clipped. Both are *the page, correct, and momentarily the
            // wrong size* — which is what every browser on this platform
            // does while you drag its corner, and is honest in a way that
            // stretching is not.
            let (fw, fh) = frame.logical(scale);
            let want_w = f32::from(bounds.size.width);
            let want_h = f32::from(bounds.size.height);
            // Half a logical pixel of slack: the coded size is a rounded
            // count of device pixels, so exact float equality is the wrong
            // test and would put the steady state on the transient path at
            // odd scales.
            let fits = (fw - want_w).abs() < 0.5 && (fh - want_h).abs() < 0.5;

            note_fit(&self.tab_id, fits, fw, fh, want_w, want_h);

            if fits || stretch_for_measurement() {
                // Bottom corners match the card; the header owns the top of
                // the card so the page's top edge is square against it.
                window.paint_surface_with_corners(
                    bounds,
                    frame.buffer,
                    Corners {
                        top_left: px(0.),
                        top_right: px(0.),
                        bottom_right: px(theme::RADIUS_PANEL),
                        bottom_left: px(theme::RADIUS_PANEL),
                    },
                );
            } else {
                let at = Bounds {
                    origin: bounds.origin,
                    size: size(px(fw), px(fh)),
                };
                // Square corners on the transient, deliberately. The surface
                // is anchored top-left and does not end where the card does,
                // so the card's radius would round the wrong edge; the mask
                // cuts it square instead for the frame or two a resize
                // lasts. A momentarily square corner beats a rounded one in
                // the wrong place.
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    window.paint_surface(at, frame.buffer);
                });
            }
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
        // Outside the `if let` above, not inside it: a popup is its own
        // surface and does not stop existing because the page frame is
        // momentarily absent.
        //
        // Square corners, deliberately: this is a menu sitting ON the page,
        // not the page itself, and rounding it to the card's radius would
        // round a widget that Chromium already drew its own shadow and
        // border into.
        if let Some((buffer, rect)) = osr::popup(&self.tab_id) {
            let at = Bounds {
                origin: gpui::point(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
                size: size(px(rect.width), px(rect.height)),
            };
            window.paint_surface(at, buffer);
        }
    }
}
