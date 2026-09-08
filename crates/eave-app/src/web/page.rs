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
use std::ops::Range;

use cef::{ImplBrowser, ImplBrowserHost, MouseButtonType, PaintElementType};
use gpui::{
    div, prelude::*, px, size, App, Bounds, ContentMask, Corners, Element, ElementId,
    ExternalPaths, FocusHandle, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Modifiers, Pixels, Point, Window,
};

use super::{input, osr};
use crate::theme;

thread_local! {
    /// One focus handle per tab, kept across frames. A fresh handle every
    /// render would mean focus was lost on every frame — which is not a
    /// subtle bug, it is "typing does nothing".
    static FOCUS: RefCell<HashMap<String, FocusHandle>> = RefCell::new(HashMap::new());
    /// Which tab last had `CefBrowserHost::SetFocus(true)`. Gated so a
    /// render that already put the keyboard here does not re-enter
    /// Chromium every frame.
    static CEF_FOCUS: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn focus_handle(tab_id: &str, cx: &mut App) -> FocusHandle {
    FOCUS.with(|f| {
        f.borrow_mut()
            .entry(tab_id.to_string())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    })
}

/// This tab's gpui focus handle — the same one [`page_element`] tracks.
///
/// `None` when the tab has no live browser. Focusing a handle whose
/// element is not in the tree is how the whole window's actions go
/// silent (Slice CM); callers must only use this when the page is
/// mounted.
pub fn page_focus_handle(tab_id: &str, cx: &mut App) -> Option<FocusHandle> {
    if !super::tabs::has_browser(tab_id) {
        return None;
    }
    Some(focus_handle(tab_id, cx))
}

/// Tell Chromium which page, if any, owns the keyboard.
///
/// gpui focus and CEF focus are two knobs. Clicking the page already
/// turns both; `Shell::sync_composer_focus` used to turn only gpui's,
/// and it turned it onto the *shell*, so a document never kept either.
/// Site shortcuts (`/` on GitHub, `j`/`k` on a feed) listen in the
/// page and never fired.
pub fn set_keyboard_page(tab_id: Option<&str>) {
    CEF_FOCUS.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.as_deref() == tab_id {
            return;
        }
        // Record first: `SetFocus` can re-enter this render, and a
        // second pass that still sees the old slot would recurse.
        let old = slot.clone();
        *slot = tab_id.map(str::to_string);
        drop(slot);
        if let Some(old) = old.as_deref() {
            // **A composition that loses focus is KEPT, not thrown away.**
            // Half-typed kana is text the person meant; every native field
            // on the platform commits it when focus moves, and discarding
            // it would lose work silently. `ime_finish` is the "accept as
            // is" call — `ime_cancel` is Escape, and the two are only a
            // flag apart in CEF while being opposites to a user.
            if super::ime::is_composing(old) {
                super::tabs::with_browser(old, input::ime_finish);
                super::ime::clear(old);
            }
            super::tabs::with_browser(old, |b| input::focus(b, false));
        }
        if let Some(id) = tab_id {
            super::tabs::with_browser(id, |b| input::focus(b, true));
        }
    });
}

/// Window-space point to page space.
///
/// The element publishes its own origin at paint, so this subtraction is
/// the whole conversion — there is no title-bar offset to guess at, and
/// guessing one cost the spike a capture run.
///
/// **CV1 added the divide, and it is the input half of a zoomed card.** A
/// card drawn at 40% is 224 window pixels wide and is still a 560-pixel
/// page; a click 100 px in from its left edge is at page x 250, not 100.
/// Without this every pointer event on a zoomed card lands short of where
/// it was aimed, by exactly the factor the card is drawn at.
fn to_page(tab_id: &str, at: Point<Pixels>) -> Point<Pixels> {
    let v = osr::view(tab_id);
    let z = v.zoom_or_one();
    gpui::point(
        (at.x - gpui::px(v.origin_x)) / z,
        (at.y - gpui::px(v.origin_y)) / z,
    )
}

fn apply_in_step(
    tab_id: &str,
    is_over: bool,
    at: Point<Pixels>,
    m: &Modifiers,
    paths: &[std::path::PathBuf],
) {
    match super::drag::note_in_over(tab_id, is_over) {
        super::drag::InEvent::Enter => {
            super::tabs::with_browser(tab_id, |b| {
                input::files_enter(b, to_page(tab_id, at), m, paths);
            });
        }
        super::drag::InEvent::Over => {
            super::tabs::with_browser(tab_id, |b| {
                input::files_over(b, to_page(tab_id, at), m);
            });
        }
        super::drag::InEvent::Leave => {
            super::tabs::with_browser(tab_id, |b| input::files_leave(b));
        }
        super::drag::InEvent::Quiet => return,
    }
    super::host::note_activity();
    super::host::request_pump();
}

fn apply_out_events(at: Point<Pixels>, m: &Modifiers, events: Vec<(String, super::drag::InEvent)>) {
    if events.is_empty() {
        return;
    }
    for (id, ev) in events {
        super::tabs::with_browser(&id, |b| match ev {
            super::drag::InEvent::Enter => input::out_enter(b, to_page(&id, at), m),
            super::drag::InEvent::Over => input::out_over(b, to_page(&id, at), m),
            super::drag::InEvent::Leave => input::out_leave(b),
            super::drag::InEvent::Quiet => {}
        });
    }
    super::host::note_activity();
    super::host::request_pump();
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
    let (exit_id, drop_id, over_id) = (id.clone(), id.clone(), id.clone());
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
            .on_mouse_up(gpui::MouseButton::Left, move |ev, _window, cx| {
                if super::drag::take_eat_mouse_up() {
                    return;
                }
                if super::drag::out_is_live() {
                    super::tabs::with_browser(&up_id, |b| {
                        input::out_drop(b, to_page(&up_id, ev.position), &ev.modifiers);
                    });
                    if let Some(source) = super::drag::out_source() {
                        super::tabs::with_browser(&source, |b| {
                            input::out_end(b, to_page(&source, ev.position));
                        });
                    }
                    super::drag::end_out();
                    super::host::note_activity();
                    super::host::request_pump();
                    return;
                }
                if cx.has_active_drag() {
                    return;
                }
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
            .on_mouse_up_out(gpui::MouseButton::Left, move |_, _, _| {
                // Capture-phase, and it fires *before* a drop on another
                // pane. Queue a cancel; paint flushes it only if the drag
                // is still live after that drop. See `drag::request_out_cancel`.
                super::drag::request_out_cancel();
            })
            .on_mouse_move(move |ev, _window, cx| {
                if cx.has_active_drag() {
                    // An OS file drag (or a tab/bookmark drag) is in
                    // flight. Forwarding this as a held mouse-move would
                    // look like a selection to Blink.
                    return;
                }
                if super::drag::out_is_live() {
                    apply_out_events(
                        ev.position,
                        &ev.modifiers,
                        super::drag::note_out_over(&move_id, true),
                    );
                    return;
                }
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
                // `start_dragging` runs synchronously inside that
                // mouse-move. If it took the drag, this move is already
                // the first over.
                if super::drag::out_is_live() {
                    apply_out_events(
                        ev.position,
                        &ev.modifiers,
                        super::drag::note_out_over(&move_id, true),
                    );
                }
            })
            .on_mouse_exit(move |ev, _, _| {
                if super::drag::out_is_live() {
                    apply_out_events(
                        ev.position,
                        &ev.modifiers,
                        super::drag::note_out_over(&exit_id, false),
                    );
                }
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
            .on_scroll_wheel(move |ev, _window, cx| {
                if let Some(delta) = input::swipe_nav(ev) {
                    super::tabs::request(super::PageRequest::Go {
                        tab_id: wheel_id.clone(),
                        delta,
                    });
                    super::host::note_activity();
                    super::host::request_pump();
                    cx.stop_propagation();
                    return;
                }
                super::tabs::with_browser(&wheel_id, |b| {
                    let (dx, dy, precise) = input::wheel_delta(ev);
                    input::wheel(
                        b,
                        to_page(&wheel_id, ev.position),
                        &ev.modifiers,
                        dx,
                        dy,
                        precise,
                    );
                });
                super::host::note_activity();
                super::host::request_pump();
                cx.stop_propagation();
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
                //
                // This listener only runs while THIS handle is on the
                // dispatch path. `Shell::sync_composer_focus` used to
                // steal gpui focus back to the shell on every render
                // whenever the composer was unmounted, so a page tab
                // never stayed here and site shortcuts never fired.
                if super::find_is_open(&key_id) {
                    return;
                }
                // **Nor while an IME is composing** — the same shape as the
                // find bar above, and the same defect if it is missing.
                // While a composition is live those keystrokes belong to
                // the IME: `PageInput` is already turning them into
                // `ImeSetComposition`, and sending them here as well would
                // type the romaji into the page *and* compose the kana.
                if super::ime::is_composing(&key_id) {
                    return;
                }
                super::tabs::with_browser(&key_id, |b| {
                    input::key(b, &ev.keystroke);
                });
                super::host::note_activity();
                super::host::request_pump();
            })
            // ── DROP TARGET (A5) ─────────────────────────────────────
            //
            // Finder file drops arrive as `ExternalPaths` plus synthetic
            // mouse events (`window.rs` FileDrop). `TabDrag` is a different
            // payload type and a different region, so it does not collide.
            // The page has one mount (`page_element`), so unlike D11/D12
            // this does not need duplicating across split vs unsplit.
            .on_drag_move::<ExternalPaths>(move |ev, _, _| {
                let Some(paths) = ev.dragged_item().downcast_ref::<ExternalPaths>() else {
                    return;
                };
                let over = ev.bounds.contains(&ev.event.position);
                apply_in_step(
                    &over_id,
                    over,
                    ev.event.position,
                    &ev.event.modifiers,
                    paths.paths(),
                );
            })
            .on_drop(move |paths: &ExternalPaths, window, _| {
                let at = window.mouse_position();
                let m = window.modifiers();
                apply_in_step(&drop_id, true, at, &m, paths.paths());
                super::tabs::with_browser(&drop_id, |b| {
                    input::files_drop(b, to_page(&drop_id, at), &m);
                });
                let dropped = drop_id.clone();
                for leftover in super::drag::end_in() {
                    if leftover != dropped {
                        super::tabs::with_browser(&leftover, |b| input::files_leave(b));
                    }
                }
                super::drag::eat_next_mouse_up();
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
fn note_fit(tab_id: &str, fits: bool, fw: f32, fh: f32, want_w: f32, want_h: f32, scale: f32) {
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
                    "[web] {tab_id}: resize settled at {want_w:.0}x{want_h:.0} after {} stale frame(s), {} ms",
                    entry.0,
                    entry.1.elapsed().as_millis()
                );
                entry.0 = 0;
            }
        } else {
            // Print at the start of a run and then every 120 frames, so a
            // run that NEVER settles is visible. A once-only line reads
            // exactly like a transient that resolved.
            if entry.0 == 0 {
                entry.1 = std::time::Instant::now();
            }
            if entry.0 % 120 == 0 {
                eprintln!(
                    "[web] {tab_id}: resize wants {want_w:.0}x{want_h:.0} @{scale:.2} \
                     ({:.0}x{:.0} device), frame is {fw:.0}x{fh:.0}, stale {}",
                    want_w * scale,
                    want_h * scale,
                    entry.0,
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

/// True while the operator is dragging one of eave's own dividers — the
/// split handle or the DevTools dock handle.
///
/// **Why a divider is not a window corner.** E1 decided that a stale frame
/// is painted one device pixel per device pixel and clipped, never
/// stretched, because a layout stretched horizontally is *the wrong
/// picture* rather than a smaller one. That is right for a window resize:
/// the size changes, Chromium answers within a frame or two, and the
/// uncovered strip shows for that long.
///
/// A divider drag is a different shape. Every pointer move is a **new
/// size**, so the pane that is growing never catches up while the pointer
/// is down, and the uncovered strip is not a transient — it is a black
/// band that tracks the pointer for the whole gesture. That is what the
/// operator reported. Between two wrong pictures, a briefly-scaled page
/// that follows the divider reads as the panel resizing; a black band
/// reads as the panel being broken.
///
/// So this narrows E1's rule rather than reversing it: stretch **only**
/// while a divider is actually moving. On mouse-up the flag clears, the
/// next frame that fits is painted 1:1, and E1's behaviour is back. The
/// pump path above is deliberately left alone — Chromium is still asked to
/// reflow on every paint, so the stretch is what is on screen *until* the
/// real frame arrives, not instead of it.
///
/// Set from the shell's render, which is the one place that can see both
/// drags at once.
///
/// **Thread-local, not a `static`, and the suite is what says so.** Both
/// ends of this live on gpui's main thread — `Shell::render` writes it,
/// `PageSurface::paint` reads it, same thread, same frame — so a
/// thread-local is not a compromise here. It was a process-global
/// `AtomicBool` first, and that version passed alone and **failed in the
/// full `--features web` run**: the harness runs tests in parallel, every
/// other test's render publishes `false`, and they were all writing to one
/// cell. Green-alone-red-together is the tell.
///
/// This is the opposite call to G1's permission mirror, which is an
/// `RwLock` precisely because it is read from a thread other than the one
/// that writes it. The rule is not "prefer one" — it is that the choice
/// follows which threads actually touch it.
fn live_divider() -> bool {
    LIVE_DIVIDER.with(|c| c.get())
}

/// How many paints after a size change a surface may still be painted
/// stretched. Small enough that a page cannot sit visibly scaled, large
/// enough to cover the couple of pumps Chromium needs to reflow.
const RESIZE_SETTLE_PAINTS: u32 = 8;

thread_local! {
    /// Per surface: the last size and device scale it was asked to paint
    /// at, and how many paints have happened since either last changed.
    static LAST_ASK: std::cell::RefCell<
        std::collections::HashMap<String, ((f32, f32, f32), u32)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// True while this surface's size is still moving.
///
/// **Why this exists on top of [`live_divider`].** That flag is set from
/// the shell, which can only see the dividers the SHELL owns. Once the
/// DevTools dock became a real dock, the resizer between the page and the
/// panels became **DevTools' own** — the shell has no drag state for it,
/// so the stretch stopped happening exactly where it was needed and the
/// page tore on every resize. This asks the only question that is true in
/// both cases: *is the size I am being asked to paint at different from
/// the size I was asked for last time?*
///
/// Self-clearing, and bounded on purpose. While a drag is running every
/// paint sees a new size and the counter stays at zero. When it stops, the
/// counter climbs; if Chromium has not caught up within
/// [`RESIZE_SETTLE_PAINTS`] we stop stretching and fall back to E1's
/// honest 1:1-and-clipped, rather than leaving a page visibly scaled for
/// ever because a frame never arrived.
fn size_is_moving(tab_id: &str, want_w: f32, want_h: f32, scale: f32) -> bool {
    LAST_ASK.with(|m| {
        let mut m = m.borrow_mut();
        let entry = m
            .entry(tab_id.to_string())
            .or_insert(((want_w, want_h, scale), 0));
        let (last, since) = entry;
        // **The scale counts as a size change, and CV1 is why.** Zooming a
        // card leaves the logical view alone and moves only the density,
        // so a scale-only test would call the surface settled while every
        // frame in flight is still a picture at the old density — and the
        // stale-frame path would clip it instead of scaling it, which
        // makes a zoom flash the page at the wrong size. A rescale is a
        // resize of the buffer; treat it as one.
        if (last.0 - want_w).abs() >= 0.5
            || (last.1 - want_h).abs() >= 0.5
            || (last.2 - scale).abs() >= 0.001
        {
            *last = (want_w, want_h, scale);
            *since = 0;
            return true;
        }
        *since = since.saturating_add(1);
        *since < RESIZE_SETTLE_PAINTS
    })
}

/// A surface that has gone away should not leave its last size behind for
/// the next tab that reuses the id.
pub(super) fn forget_resize_state(tab_id: &str) {
    LAST_ASK.with(|m| {
        m.borrow_mut().remove(tab_id);
    });
}

thread_local! {
    static LIVE_DIVIDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// See [`live_divider`]. Called every render with the current drag state,
/// so it is self-correcting: a drag that ends without a mouse-up (a
/// dropped grab, a window deactivation) clears on the next frame rather
/// than leaving every page stretched for ever.
pub fn set_live_divider(on: bool) {
    LIVE_DIVIDER.with(|c| c.set(on));
}

/// The flag `paint` actually consults, so the shell's rule is tested
/// against the real thing with the feature on rather than only against
/// the feature-off stub's record.
#[cfg(test)]
pub(super) fn live_divider_for_test() -> bool {
    live_divider()
}

/// What CEF is told about an element box of `box_w`×`box_h`, painted at
/// `zoom` window pixels per element pixel on a display whose backing factor
/// is `display`.
///
/// Returns the **logical** page size, the device scale factor, and the
/// guarded zoom — in that order, because that is the order they are needed.
///
/// **The logical size is the box itself, and that is the fix.** The element
/// is measured in the card's own coordinates, which do not move when the
/// zoom does — so the page is laid out at 560×460 whatever the plane is
/// zoomed to, and the zoom is spent on the device scale factor instead.
/// Chromium rasterises the same layout at `display × zoom` density, exactly
/// as it would for a retina display. Before this, the card was told it *was*
/// 224 pixels wide at 40% and reflowed into a phone width with desktop type
/// still in it — the operator's word for it was corruption.
///
/// **Zooming OUT does not touch Chromium at all, and that is deliberate.**
/// The obvious density is `display × zoom`, and it was built and measured
/// first: below 1.0 Chromium simply does not follow. Traced — asked for
/// 558×410 at 1.00, it kept answering with a 1012×744 buffer, stale for
/// hundreds of consecutive frames and never settling, which on screen is
/// the page drawn at twice its size and clipped to the card. So the ask is
/// floored at the display's own factor: zoomed out, the page keeps
/// rendering at full density and the GPU samples it down into a smaller
/// box, which is a supersampled miniature and if anything *cleaner* than a
/// small raster would be. Zoomed IN past 100% the ask does rise with the
/// zoom, because there magnifying a texture is the thing that would look
/// soft.
///
/// It also makes the whole 50–100% band free: the view never changes, so
/// Chromium is never asked to re-render and there is no settling to wait
/// for while the plane is being zoomed. **With `canvas::ZOOM_MAX` at actual
/// size that band is the whole plane**, so today a page card's density is
/// simply constant and CEF never hears about the zoom at all. The
/// magnification branch is kept, dormant, because it is the correct rule
/// and not a workaround: if the ceiling ever rises, the density has to rise
/// with it or a magnified card is a stretched texture.
///
/// The identity that keeps a card sharp is `logical × ask == the buffer`,
/// and gpui paints that buffer into `box × display × zoom` on screen —
/// equal at and above 100%, an integer downsample below it.
fn card_view(box_w: f32, box_h: f32, display: f32, zoom: f32) -> (f32, f32, f32, f32) {
    let z = if zoom > 0.01 && zoom.is_finite() { zoom } else { 1. };
    (box_w, box_h, display * z.max(1.0), z)
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
        cx: &mut App,
    ) {
        // **The IME's way in — 06-browser A3.** Registered here rather than
        // in `render` for this file's standing reason: it needs `bounds`,
        // and geometry is decided in `paint`. `handle_input` keeps it only
        // while this page's handle is focused, which is the same condition
        // `on_key_down` needs to be on the dispatch path — so the two
        // keyboard paths are live together or not at all, and there is no
        // state in which text has nowhere to land.
        if let Some(focus) = page_focus_handle(&self.tab_id, cx) {
            window.handle_input(
                &focus,
                PageInput {
                    tab_id: self.tab_id.clone(),
                },
                cx,
            );
        }

        // An OS file drag that left the window (`FileDrop::Exited`)
        // clears `active_drag` without a mouse event we hear. If we still
        // think this page is over, tell Chromium it isn't.
        if super::drag::in_is_over(&self.tab_id) && !cx.has_active_drag() {
            super::tabs::with_browser(&self.tab_id, |b| input::files_leave(b));
            let _ = super::drag::note_in_over(&self.tab_id, false);
        }

        // Mouse-up on chrome queued a cancel last event. If a drop on
        // another pane already ended the drag, this is a no-op.
        if super::drag::flush_out_cancel() {
            if let Some(source) = super::drag::out_source() {
                super::tabs::with_browser(&source, |b| input::out_cancel(b));
            }
            super::drag::end_out();
        }

        // The page is exactly the size of the space it was given, and the
        // origin published here is what makes input coordinates exact: page
        // space and element space become the same space, so there is no
        // offset to guess at. (A guessed 28 pt title-bar offset cost the
        // spike a capture run.)
        //
        // **CV1: the zoom moves the RASTER, never the layout.** A card on
        // the canvas is drawn at `zoom` window pixels per page pixel, and
        // the naive thing — telling CEF the size it is drawn at — is what
        // the operator saw and called corruption: at 40% a 560×460 card
        // became a 224×184 viewport, so the page re-laid-out into a phone
        // width with the same 13 px type in it. A miniature is not a
        // narrower page.
        //
        // So the logical size is the card's size **at 100%** and never
        // changes with the zoom, and the zoom goes into the device scale
        // factor instead. Chromium then rasterises the *same layout* at
        // `display × zoom` density — which is exactly what it already does
        // for a retina display, and the reason `ViewChange::Rescaled`
        // exists. The buffer that comes back is `bounds` worth of device
        // pixels, so `paint_surface` draws it one for one and the card is
        // a sharp miniature rather than a resampled one.
        // `dev_scale` is what CEF is ASKED for; what gpui paints at is
        // `window.scale_factor()`, and below 100% they are deliberately
        // different — see `card_view`.
        let (logical_w, logical_h, dev_scale, zoom) = card_view(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            window.display_scale_factor(),
            window.element_scale(),
        );
        let view = osr::View {
            width: logical_w.round() as i32,
            height: logical_h.round() as i32,
            scale: dev_scale,
            // **In WINDOW space**, because this is what input is measured
            // from and a pointer arrives in the window's coordinates. The
            // element's own bounds are in the card's space, so this is the
            // one place the scale is spent on the way out.
            origin_x: f32::from(bounds.origin.x) * zoom,
            origin_y: f32::from(bounds.origin.y) * zoom,
            zoom,
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
                    // A split pane that is not engine-`active` still has to
                    // layout. WasResized alone is enough for the focused
                    // browser; an unfocused OSR compositor often keeps the
                    // last frame until it is invalidated. Without this,
                    // dragging the window (or the divider) reflows one
                    // pane and leaves the other stretched or clipped.
                    host.invalidate(PaintElementType::VIEW);
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
                // Live window resize runs AppKit's tracking loop; the async
                // pump does not. Pump here so Chromium answers on this frame
                // rather than after mouse-up. See `host::pump_now`.
                super::host::pump_now();
            }
        }

        // ── THE RESIZE TRANSIENT (Slice E1) ──────────────────────────────
        //
        // `paint_surface` scales its buffer to fill the bounds it is given,
        // which is right in the steady state and a **stretch** in every
        // frame between "the window changed size" and "Chromium answered".
        // A page is not a video: it has a layout, and a layout stretched
        // horizontally is not a smaller rendering of the same thing, it is
        // the wrong picture.
        //
        // So: paint the stale frame at one device pixel per device pixel,
        // anchored top-left, clipped to the space the element actually has.
        // Growing, the new strip of the well shows what is under it for a
        // frame or two; shrinking, the overhang is clipped. Both are *the
        // page, correct, and momentarily the wrong size* — which is what
        // every browser on this platform does while you drag its corner,
        // and is honest in a way that stretching is not.
        //
        // Bottom corners match the card; the header owns the top of the
        // card so the page's top edge is square against it.
        //
        // **A popup's page is the exception, and it is the whole card**
        // (Slice C2): the overlay has no header over it, so a square top
        // edge would be two sharp corners inside a rounded frame — the
        // one place the page's own rectangle would show through the card
        // it is supposed to be.
        let radius = px(theme::RADIUS_PANEL);
        let square = px(0.);
        let all_four = super::popups::is_popup_id(&self.tab_id);
        let corners = Corners {
            top_left: if all_four { radius } else { square },
            top_right: if all_four { radius } else { square },
            bottom_right: radius,
            bottom_left: radius,
        };
        // **In PAGE pixels, not window pixels.** Everything below compares
        // a frame against what was asked for, and what was asked for is the
        // logical view — so on a zoomed card these do not move when the
        // zoom does. What moves is `dev_scale`, and `size_is_moving` reads
        // that too.
        let want_w = logical_w;
        let want_h = logical_h;
        // **Measured in DEVICE pixels, and a logical half-pixel was not
        // good enough.** The buffer Chromium hands back is a whole number
        // of device pixels; dividing it by a device scale that is not 1 or
        // 2 lands on a logical size that is a fraction out, and a
        // half-pixel test then calls a perfectly good frame stale forever.
        // Traced on the canvas: `wants 554x294, frame is 555x294` for 644
        // consecutive frames over 44 seconds, pumping Chromium twice a
        // paint and painting the clipped fallback the whole time. Two
        // device pixels is the tolerance because both sides round.
        let dev_want_w = want_w * dev_scale;
        let dev_want_h = want_h * dev_scale;
        let fit_of = |frame: &osr::Frame| {
            let (fw, fh) = frame.logical(dev_scale);
            let fits = (frame.src_width as f32 - dev_want_w).abs() <= 2.0
                && (frame.src_height as f32 - dev_want_h).abs() <= 2.0;
            (fw, fh, fits)
        };
        // Asked once per paint, and it MUTATES the counter, so it must not
        // be called again inside this frame.
        let moving = size_is_moving(&self.tab_id, want_w, want_h, dev_scale);
        let mut frame = osr::frame(&self.tab_id);
        if let Some(ref f) = frame {
            let (fw, fh, mut fits) = fit_of(f);
            // Still the wrong size: keep pumping on this stack for as long
            // as the drag is producing paints. `change == Same` after the
            // first live-resize tick, so the branch above does not run
            // again until the view actually moves.
            //
            // Two pumps is a ceiling, not a delay: Invalidate queues a
            // compositor frame, the first `CefDoMessageLoopWork` often runs
            // the resize task, the second collects the frame. Stop early
            // when it fits. Needed for the unfocused split pane, which
            // otherwise stays one drag behind the focused one.
            if !fits && !stretch_for_measurement() {
                super::tabs::with_browser(&self.tab_id, |b| {
                    if let Some(host) = b.host() {
                        host.invalidate(PaintElementType::VIEW);
                    }
                });
                for _ in 0..2 {
                    super::host::pump_now();
                    frame = osr::frame(&self.tab_id);
                    if let Some(ref f) = frame {
                        let next = fit_of(f);
                        fits = next.2;
                        note_fit(&self.tab_id, fits, next.0, next.1, want_w, want_h, dev_scale);
                        if fits {
                            break;
                        }
                    }
                }
            } else {
                note_fit(&self.tab_id, fits, fw, fh, want_w, want_h, dev_scale);
            }
            if let Some(frame) = frame {
                let (fw, fh, fits) = fit_of(&frame);
                // **A stretch is only honest across a SMALL gap.** Scaling
                // the last frame to a size a little different from its own
                // reads as the page resizing. Scaling it across a large
                // jump — a pane that just went from a third of the well to
                // all of it — reads as the page exploding, which is
                // exactly what the operator saw when the dock's layout
                // still switched modes on open. Past this factor, fall
                // back to E1's 1:1-and-clipped: momentarily the wrong
                // size, but never the wrong picture.
                const MAX_STRETCH: f32 = 1.5;
                let ratio = |a: f32, b: f32| if a > b { a / b.max(1.0) } else { b / a.max(1.0) };
                let modest =
                    ratio(fw, want_w) <= MAX_STRETCH && ratio(fh, want_h) <= MAX_STRETCH;
                // **On a zoomed card a stale frame is the wrong
                // RESOLUTION, never the wrong picture — so scale it.**
                // E1's rule is right where it was written: during a window
                // resize the LAYOUT is changing under you, and a scaled
                // frame lies about where the text went. On the canvas the
                // layout is pinned to the card's own size and only the
                // density moves, so every frame this tab produces is a
                // rendering of the same page and scaling one is exactly
                // what a zoom is. The clipped fallback here is what the
                // operator photographed at 81%: the page in the top-left
                // half of its card with black around it.
                let pinned_layout = zoom != 1.0;
                if fits
                    || stretch_for_measurement()
                    || (pinned_layout && modest)
                    || ((live_divider() || moving) && modest)
                {
                    window.paint_surface_with_corners(bounds, frame.buffer, corners);
                } else {
                    // `fw`/`fh` are page pixels and so is the element's
                    // box, so the stale frame is anchored top-left at its
                    // own size and the element scale does the rest.
                    let at = Bounds {
                        origin: bounds.origin,
                        size: size(px(fw), px(fh)),
                    };
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        window.paint_surface_with_corners(at, frame.buffer, corners);
                    });
                }
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
        // **Deliberately NOT through the resize-transient path above.** A
        // popup is placed by a rect Chromium computed for the CURRENT view;
        // it has no stale-size problem to solve, and clipping it to the
        // page's bounds or anchoring it top-left would move a menu that is
        // already where it belongs.
        //
        // Square corners, deliberately: this is a menu sitting ON the page,
        // not the page itself, and rounding it to the card's radius would
        // round a widget that Chromium already drew its own shadow and
        // border into.
        if let Some((buffer, rect)) = osr::popup(&self.tab_id) {
            // The rect is in the page's own coordinates and so is the
            // element's box, so this is the same arithmetic it always was
            // and the element scale carries it onto the screen.
            let at = Bounds {
                origin: gpui::point(bounds.origin.x + px(rect.x), bounds.origin.y + px(rect.y)),
                size: gpui::size(px(rect.width), px(rect.height)),
            };
            window.paint_surface(at, buffer);
        }
    }
}

// ---------------------------------------------------------------------------
// The keyboard, the other half — IME (06-browser A3)
// ---------------------------------------------------------------------------

/// The page as a text input, so an IME can compose into it.
///
/// # Why this exists
/// `gpui::InputHandler` is "a 1:1 exposure of `NSTextInputClient`" — the
/// protocol AppKit talks to when text is being *composed* rather than
/// typed. Registering one is what turns `k`,`a` into `か` and a candidate
/// list instead of two Latin letters, and what makes `⌥e e` produce `é`.
/// Without it, Japanese, Chinese and Korean cannot be entered into a page
/// at all.
///
/// # Why it holds only a tab id
/// Chromium owns the composition; it lives in the renderer. Everything
/// here either forwards to CEF or answers from [`super::ime`]'s mirror of
/// what Chromium last reported. There is no second copy of the text, for
/// the same reason `ComposerState` keeps none: one draft, one home.
///
/// # Registered in `paint`, and only while focused
/// `Window::handle_input` keeps the handler only `if focus_handle
/// .is_focused(..)`, which is the same condition `on_key_down` needs to be
/// on the dispatch path. So the two keyboard paths arrive and leave
/// together — there is no state in which one is live and the other is not.
struct PageInput {
    tab_id: String,
}

impl gpui::InputHandler for PageInput {
    /// Chromium's view of the selection, when it has offered one.
    ///
    /// `None` is a legitimate answer and not a gap: AppKit treats it as
    /// "no selection known" and still composes. eave cannot do better —
    /// CEF exposes no way to read the focused field's contents, so an
    /// invented range would be a confident wrong answer where a `None` is
    /// an honest missing one.
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<gpui::UTF16Selection> {
        super::ime::selected_range(&self.tab_id).map(|range| gpui::UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, _cx: &mut App) -> Option<Range<usize>> {
        super::ime::marked_range(&self.tab_id)
    }

    /// **eave cannot read the page's text, and says so.**
    ///
    /// This is what an IME uses for reconversion and for context-aware
    /// candidates. CEF has no API for the focused field's contents, so the
    /// honest answer is `None`; composition works without it, reconversion
    /// does not. Returning `Some("")` instead would tell macOS the field
    /// is empty, which is a different and worse claim.
    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    /// Text lands in the page — **every character, not only IME commits.**
    ///
    /// See the `CHAR` note in [`super::input::key`]: with a handler
    /// registered, macOS delivers ordinary printable keys here too. That
    /// is not a quirk to work around, it is the fix for the other half of
    /// A3 — the character now comes from AppKit's own interpretation of
    /// the key, so a French or German layout finally types its own
    /// letters instead of the US ones `windows_key_code` guessed at.
    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        // **Which call inserts depends on whether a composition is open,
        // and getting this wrong is silent.** `ImeCommitText` *completes* a
        // composition; with none open Chromium has nothing to complete and
        // the call does nothing at all — no error, no text, no
        // `beforeinput`. That was the first cut of A3, and it took a live
        // walk to find because every test, the compile and the handler
        // registration were all green while typing did nothing.
        let kind = super::ime::insert_kind(&self.tab_id);
        if super::tabs::trace() {
            eprintln!("[web] ime insert {text:?} via={kind:?} tab={}", self.tab_id);
        }
        super::tabs::with_browser(&self.tab_id, |b| match kind {
            super::ime::Insert::Commit => super::input::ime_commit(b, text),
            super::ime::Insert::Char => super::input::char_text(b, text),
        });
        // The composition, if there was one, is over.
        super::ime::clear(&self.tab_id);
        super::host::note_activity();
        super::host::request_pump();
    }

    /// The provisional stage: `か` on screen, underlined, uncommitted.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if super::tabs::trace() {
            eprintln!("[web] ime mark {new_text:?} tab={}", self.tab_id);
        }
        let units = new_text.encode_utf16().count();
        let selection = new_selected_range.clone().unwrap_or(units..units);
        super::tabs::with_browser(&self.tab_id, |b| {
            super::input::ime_set_composition(b, new_text, selection.clone())
        });
        // Recorded AFTER the send, so a tab with no browser never looks
        // like it is composing — `is_composing` gates the raw key path,
        // and a tab stuck true there has a dead keyboard.
        super::ime::set_marked(&self.tab_id, units, new_selected_range);
        super::host::note_activity();
        super::host::request_pump();
    }

    /// Composition abandoned — Escape, or a click elsewhere.
    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut App) {
        super::tabs::with_browser(&self.tab_id, super::input::ime_cancel);
        super::ime::clear(&self.tab_id);
        super::host::note_activity();
        super::host::request_pump();
    }

    /// **Where the candidate window goes.**
    ///
    /// The one question whose wrong answer is unmistakable: get it wrong
    /// and the IME panel sits in the window's top-left corner while you
    /// type into a field in the middle of the page.
    ///
    /// Falls back to the element's own bounds when Chromium has reported
    /// nothing yet — the first keystroke of a composition arrives before
    /// `on_ime_composition_range_changed` does, and a panel on the page is
    /// better than a panel in the corner.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let view = osr::view(&self.tab_id);
        let boxes = super::ime::bounds(&self.tab_id);
        let rect = super::ime::pick_rect(&boxes, &range_utf16)?;
        // **In the ELEMENT's space, not the window's**, because gpui puts
        // an input handler's answers back into window coordinates itself
        // (`PlatformInputHandler`'s scale). `View::origin` is stored in
        // window coordinates — it is what raw pointer events are measured
        // from, and those take no such path — so it is the origin that has
        // to come back here, not the rect.
        let z = view.zoom_or_one();
        let win = super::ime::to_window(&rect, view.origin_x / z, view.origin_y / z);
        Some(Bounds {
            origin: gpui::point(px(win.x), px(win.y)),
            size: gpui::size(px(win.width), px(win.height)),
        })
    }

    /// Not offered. This is for clicking *inside* composing text, and
    /// answering it would need the page's own text layout, which lives in
    /// the renderer.
    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    /// The element's own box, so macOS has something to fall back to.
    fn element_bounds(&mut self, _window: &mut Window, _cx: &mut App) -> Option<Bounds<Pixels>> {
        let v = osr::view(&self.tab_id);
        // Element space, for the same reason as `bounds_for_range`.
        let z = v.zoom_or_one();
        Some(Bounds {
            origin: gpui::point(px(v.origin_x / z), px(v.origin_y / z)),
            size: gpui::size(px(v.width as f32), px(v.height as f32)),
        })
    }

    /// **`true`, and a browser is the case this flag was written for.**
    ///
    /// gpui's doc: printable keys route to the IME before keybinding
    /// matching *when a non-ASCII input source is active*, so a
    /// multi-stroke binding cannot intercept keys the IME should compose.
    /// A page is exactly that: while a Japanese input source is on, `j`
    /// belongs to the composition and not to any chord. It changes nothing
    /// on a Latin layout, where no input source is claiming the keys.
    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }
}

/// CV2's zoom arithmetic. The behaviour it guards lives behind CEF and a
/// live display, so these pin the identities the paint depends on.
#[cfg(test)]
mod card_view_tests {
    use super::card_view;

    /// At and above 100% the buffer exactly fills the space on screen, so
    /// the card is drawn one buffer pixel per screen pixel.
    #[test]
    fn a_magnified_card_is_rendered_at_its_drawn_size() {
        for display in [1.0_f32, 2.0] {
            for zoom in [1.0_f32, 1.4, 2.0] {
                let (box_w, box_h) = (560.0, 460.0);
                let (lw, lh, dev, _) = card_view(box_w, box_h, display, zoom);
                assert!(
                    (lw * dev - box_w * display * zoom).abs() < 0.01,
                    "width: {lw} x {dev} at zoom {zoom} on a {display}x display"
                );
                assert!((lh * dev - box_h * display * zoom).abs() < 0.01, "height at {zoom}");
            }
        }
    }

    /// Below 100% the ask stops moving. That is what keeps Chromium out of
    /// the zoom entirely — and it is not a compromise, it is a buffer with
    /// more pixels than the box it lands in.
    #[test]
    fn a_shrunken_card_keeps_asking_for_full_density() {
        for display in [1.0_f32, 2.0] {
            for zoom in [0.5_f32, 0.7, 0.99] {
                let (_, _, dev, _) = card_view(560.0, 460.0, display, zoom);
                assert_eq!(dev, display, "at zoom {zoom} on a {display}x display");
            }
        }
    }

    /// And the buffer is never SMALLER than the space it has to fill —
    /// which is the whole difference between a miniature and a blur.
    #[test]
    fn the_buffer_is_never_short_of_the_space_it_lands_in() {
        for display in [1.0_f32, 2.0] {
            for zoom in [0.5_f32, 0.8, 1.0, 1.6, 2.0] {
                let (lw, _, dev, _) = card_view(560.0, 460.0, display, zoom);
                assert!(
                    lw * dev >= 560.0 * display * zoom - 0.01,
                    "short at zoom {zoom} on a {display}x display"
                );
            }
        }
    }

    /// **The layout must not move when the zoom does.** This is the whole
    /// operator complaint: a card drawn small used to be told it *was*
    /// small, so the page reflowed into a phone width with desktop type
    /// still in it. The element is measured in the card's own coordinates,
    /// which the zoom does not touch — so the logical size CEF is given is
    /// the box, whatever the plane is doing.
    #[test]
    fn the_page_is_laid_out_at_the_cards_own_size_whatever_the_zoom() {
        for zoom in [0.5_f32, 0.8, 1.0, 2.0] {
            let (lw, lh, _, _) = card_view(560.0, 460.0, 2.0, zoom);
            assert_eq!((lw, lh), (560.0, 460.0), "logical size moved at {zoom}");
        }
    }

    /// Only the DENSITY moves, and only upward.
    #[test]
    fn the_zoom_is_spent_on_the_device_scale_and_nowhere_else() {
        for zoom in [1.0_f32, 1.5, 2.0] {
            let (.., dev, _) = card_view(560.0, 460.0, 2.0, zoom);
            assert!((dev - 2.0 * zoom).abs() < 1e-6, "at {zoom}");
        }
    }

    /// The well, a split pane and a popup overlay are never inside a scaled
    /// subtree, and for them nothing may have changed at all.
    #[test]
    fn an_unzoomed_surface_is_told_exactly_what_it_was_before() {
        let (lw, lh, dev, z) = card_view(1200.0, 800.0, 2.0, 1.0);
        assert_eq!((lw, lh, dev, z), (1200.0, 800.0, 2.0, 1.0));
    }

    /// A zoom of zero would divide a pointer coordinate by zero and put
    /// every click at infinity. A garbage value is 1:1, never a crash.
    #[test]
    fn a_zoom_of_zero_is_refused_rather_than_divided_by() {
        for bad in [0.0_f32, -3.0, f32::NAN, f32::INFINITY] {
            let (lw, _, dev, z) = card_view(560.0, 460.0, 2.0, bad);
            assert_eq!((lw, dev, z), (560.0, 2.0, 1.0), "for {bad}");
        }
    }

    /// Input is the other half, and it is the one thing that still crosses
    /// between the two spaces. The element publishes its origin multiplied
    /// into WINDOW coordinates and `to_page` divides the pointer by the
    /// same zoom, so a point a third of the way across the drawn card is a
    /// third of the way across the page it is a picture of.
    #[test]
    fn a_point_lands_at_the_same_fraction_of_the_page_it_does_of_the_card() {
        let element_origin = 120.0_f32;
        for zoom in [0.5_f32, 1.0, 2.0] {
            let (lw, ..) = card_view(560.0, 460.0, 2.0, zoom);
            let published = element_origin * zoom;
            // A third of the way across the card as DRAWN.
            let at = published + 560.0 * zoom / 3.0;
            let page_x = (at - published) / zoom;
            assert!(
                (page_x - lw / 3.0).abs() < 0.01,
                "zoom {zoom}: {page_x} is not a third of {lw}"
            );
        }
    }
}
