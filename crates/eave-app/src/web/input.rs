//! Forwarding input to a windowless browser, by hand.
//!
//! This is the half of OSR that has no equivalent in a native view. A
//! `WKWebView` or a windowed CEF browser is an `NSView`: AppKit routes events
//! to it and everything below — hit testing, text selection, drag
//! autoscroll, IME, cursor shapes, the `<select>` dropdown — is the toolkit's
//! problem. In windowless mode all of it is ours, and the surface is
//! `CefBrowserHost::Send*Event`.
//!
//! Two translations are needed, and only one of them is cheap.
//!
//! **Mouse** is nearly free: CEF wants view-relative logical pixels and a
//! modifier bitfield, which is exactly what gpui hands us.
//!
//! **Keyboard is not.** `cef_key_event_t` carries `windows_key_code` — a
//! Windows virtual-key code — plus a native code and a UTF-16 character, and
//! Chromium expects a `RAWKEYDOWN` / `CHAR` / `KEYUP` sequence rather than
//! the single "key down" gpui reports. gpui has *already* collapsed the
//! native event into its own `Keystroke` (a string key name plus modifiers),
//! so the host has to translate back out of that abstraction into a model
//! gpui deliberately hid. What follows covers ASCII, the editing keys and the
//! arrows on a **US layout** — enough for an address bar and a form, and
//! carried verbatim from the spike rather than rebuilt.
//!
//! **What it does not cover, and what that costs.** A real table has to
//! handle non-US layouts (where the character a key produces is not implied
//! by its position), dead keys, the numeric keypad, and IME composition —
//! and IME is not a bigger table but a different mechanism entirely
//! (`ImeSetComposition` / `ImeCommitText`, plus reporting the composition
//! rectangle back so the candidate window lands in the right place). None of
//! that is here. On this build a French keyboard types the wrong letters and
//! Japanese cannot be typed at all. That is the next slice, and it is
//! deliberately not started in this one.

use std::cell::{Cell, RefCell};
use std::path::Path;

use cef::sys::{cef_drag_operations_mask_t, cef_event_flags_t};
use cef::{
    drag_data_create, Browser, CefString, CompositionUnderline, DragData, DragOperationsMask,
    ImplBrowser, ImplBrowserHost, ImplDragData, KeyEvent, KeyEventType, MouseButtonType,
    MouseEvent, Range as CefRange,
};
use gpui::{Keystroke, Modifiers, Pixels, Point, ScrollDelta, ScrollWheelEvent};

/// CEF's modifier bitfield from gpui's modifiers.
pub fn flags(m: &Modifiers) -> u32 {
    let mut f = 0u32;
    if m.shift {
        f |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0;
    }
    if m.control {
        f |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0;
    }
    if m.alt {
        f |= cef_event_flags_t::EVENTFLAG_ALT_DOWN.0;
    }
    if m.platform {
        f |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0;
    }
    f
}

fn event(at: Point<Pixels>, m: &Modifiers) -> MouseEvent {
    MouseEvent {
        x: f32::from(at.x) as i32,
        y: f32::from(at.y) as i32,
        modifiers: flags(m),
    }
}

/// A move with `held` set is a DRAG. CEF has no separate drag event: the
/// held button lives in the same modifier bitfield as shift and command, and
/// omitting it is why a naive port can click and scroll but never select
/// text — Blink sees a hover, not a selection gesture.
pub fn mouse_move(browser: &Browser, at: Point<Pixels>, m: &Modifiers, held: bool, leaving: bool) {
    if let Some(host) = browser.host() {
        let mut ev = event(at, m);
        if held {
            ev.modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0;
        }
        host.send_mouse_move_event(Some(&ev), leaving as i32);
    }
}

pub fn mouse_button(
    browser: &Browser,
    at: Point<Pixels>,
    m: &Modifiers,
    button: MouseButtonType,
    up: bool,
    clicks: i32,
) {
    if let Some(host) = browser.host() {
        host.send_mouse_click_event(Some(&event(at, m)), button, up as i32, clicks);
    }
}

/// Chromium `ui::kScrollbarPixelsPerCocoaTick` — pixels per notchy-wheel
/// tick on macOS (`ui/events/cocoa/cocoa_event_utils.h`). Arc/Chrome
/// convert `-[NSEvent deltaY]` with this; gpui reports those ticks as
/// `ScrollDelta::Lines`. We used to multiply by the window line-height
/// (~21) instead, so a mouse wheel was about half of Arc.
const COCOA_TICK_PX: f32 = 40.0;

thread_local! {
    /// Sub-pixel remainder of the last wheel. CEF's `SendMouseWheelEvent`
    /// takes `int`; a trackpad tick is often 0.3–0.9 px, which truncated
    /// to 0 and was dropped (`deltaX == 0 && deltaY == 0` is a no-op in
    /// CEF). Gentle scrolls felt sticky and a flick lost the small events
    /// in the gesture.
    static WHEEL_FRAC: Cell<(f32, f32)> = const { Cell::new((0.0, 0.0)) };
}

fn take_pixels(dx: f32, dy: f32) -> (i32, i32) {
    WHEEL_FRAC.with(|cell| {
        let (mut rx, mut ry) = cell.get();
        if dx != 0.0 && rx != 0.0 && dx.signum() != rx.signum() {
            rx = 0.0;
        }
        if dy != 0.0 && ry != 0.0 && dy.signum() != ry.signum() {
            ry = 0.0;
        }
        let x = rx + dx;
        let y = ry + dy;
        let ix = x.trunc();
        let iy = y.trunc();
        cell.set((x - ix, y - iy));
        (ix as i32, iy as i32)
    })
}

/// Pixel deltas for `SendMouseWheelEvent`, plus whether they are
/// trackpad-precise (so we can set `EVENTFLAG_PRECISION_SCROLLING_DELTA`).
///
/// Trackpad / Magic Mouse: gpui already reports pixels — NSEvent
/// `scrollingDelta`, which Chromium treats as
/// `kCGScrollWheelEventPointDelta`. Notchy wheel: gpui reports lines;
/// Chrome multiplies by [`COCOA_TICK_PX`], not font size.
pub fn wheel_delta(ev: &ScrollWheelEvent) -> (i32, i32, bool) {
    match ev.delta {
        ScrollDelta::Pixels(p) => {
            let (dx, dy) = take_pixels(f32::from(p.x), f32::from(p.y));
            (dx, dy, true)
        }
        ScrollDelta::Lines(l) => {
            let (dx, dy) = take_pixels(l.x * COCOA_TICK_PX, l.y * COCOA_TICK_PX);
            (dx, dy, false)
        }
    }
}

pub fn wheel(
    browser: &Browser,
    at: Point<Pixels>,
    m: &Modifiers,
    dx: i32,
    dy: i32,
    precise: bool,
) {
    if dx == 0 && dy == 0 {
        return;
    }
    if let Some(host) = browser.host() {
        let mut ev = event(at, m);
        if precise {
            // Tells Chromium the deltas are pixels, not WHEEL_DELTA ticks.
            // CEF's Mac translator already hardcodes `kScrollByPrecisePixel`,
            // but Aura (and anything that reads the modifier) does not.
            ev.modifiers |= cef_event_flags_t::EVENTFLAG_PRECISION_SCROLLING_DELTA.0;
        }
        host.send_mouse_wheel_event(Some(&ev), dx, dy);
    }
}

pub fn focus(browser: &Browser, focused: bool) {
    if let Some(host) = browser.host() {
        host.set_focus(focused as i32);
    }
}

/// gpui `Keystroke` -> Windows virtual-key code, plus the character Chromium
/// should insert (if any).
///
/// Returns `None` for anything this probe does not claim to handle, so the
/// gap is visible in the log rather than silently mistyped.
fn windows_key_code(ks: &Keystroke) -> Option<(i32, Option<u16>)> {
    let key = ks.key.as_str();
    // Single printable characters: for ASCII letters and digits the virtual
    // key code IS the uppercase ASCII value, which is the one piece of this
    // that is genuinely simple.
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            let vk = c.to_ascii_uppercase() as i32;
            let typed = ks
                .key_char
                .as_ref()
                .and_then(|s| s.encode_utf16().next())
                .or(Some(c as u16));
            return Some((vk, typed));
        }
        if let Some(vk) = punctuation_vk(c) {
            let typed = ks
                .key_char
                .as_ref()
                .and_then(|s| s.encode_utf16().next())
                .or(Some(c as u16));
            return Some((vk, typed));
        }
    }
    let vk = match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "escape" => 0x1B,
        "space" => 0x20,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "delete" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        _ => return None,
    };
    let typed = match key {
        "space" => Some(' ' as u16),
        "enter" => Some('\r' as u16),
        "tab" => Some('\t' as u16),
        _ => None,
    };
    Some((vk, typed))
}

/// gpui key name -> macOS virtual key code.
///
/// This table is the finding, not an implementation detail. `cef_key_event_t`
/// carries BOTH a Windows virtual-key code and a *native* one, and on macOS
/// Chromium rebuilds the keystroke from the native code — so a correct
/// `windows_key_code` with `native_key_code: 0` produces a page that sees
/// every key as `KeyA`. gpui has already thrown the native code away by the
/// time it hands over a `Keystroke`, so the host has to reconstruct it from
/// gpui's key NAME: a second table, maintained against a layout gpui does not
/// expose. It is correct only for a US layout, which is the point — Chromium
/// carries a real one, and it is not fifty lines.
fn mac_key_code(key: &str) -> Option<i32> {
    const LETTERS: [i32; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [i32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return Some(LETTERS[(c as u8 - b'a') as usize]);
        }
        if c.is_ascii_uppercase() {
            return Some(LETTERS[(c as u8 - b'A') as usize]);
        }
        if c.is_ascii_digit() {
            return Some(DIGITS[(c as u8 - b'0') as usize]);
        }
        return Some(match c {
            '-' => 27,
            '=' => 24,
            '[' => 33,
            ']' => 30,
            '\\' => 42,
            ';' => 41,
            '\'' => 39,
            ',' => 43,
            '.' => 47,
            '/' => 44,
            '`' => 50,
            _ => return None,
        });
    }
    Some(match key {
        "space" => 49,
        "enter" => 36,
        "tab" => 48,
        "backspace" => 51,
        "escape" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        "delete" => 117,
        _ => return None,
    })
}

fn punctuation_vk(c: char) -> Option<i32> {
    Some(match c {
        ';' | ':' => 0xBA,
        '=' | '+' => 0xBB,
        ',' | '<' => 0xBC,
        '-' | '_' => 0xBD,
        '.' | '>' => 0xBE,
        '/' | '?' => 0xBF,
        '`' | '~' => 0xC0,
        '[' | '{' => 0xDB,
        '\\' | '|' => 0xDC,
        ']' | '}' => 0xDD,
        '\'' | '"' => 0xDE,
        _ => return None,
    })
}

/// Send the RAWKEYDOWN / CHAR / KEYUP sequence Chromium expects for one
/// gpui key press. gpui reports a single "key down"; a browser wants three
/// events, and omitting the CHAR is exactly why naive OSR ports have keys
/// that move the caret but never insert text.
pub fn key(browser: &Browser, ks: &Keystroke) -> bool {
    let Some(host) = browser.host() else {
        return false;
    };
    let Some((vk, typed)) = windows_key_code(ks) else {
        println!("[input] UNTRANSLATED key {:?} — dropped", ks.key);
        return false;
    };
    let modifiers = flags(&ks.modifiers);

    let base = KeyEvent {
        size: std::mem::size_of::<cef::sys::_cef_key_event_t>(),
        type_: KeyEventType::RAWKEYDOWN,
        modifiers,
        windows_key_code: vk,
        native_key_code: mac_key_code(ks.key.as_str()).unwrap_or(0),
        is_system_key: 0,
        character: typed.unwrap_or(0),
        unmodified_character: typed.unwrap_or(0),
        focus_on_editable_field: 0,
    };
    host.send_key_event(Some(&base));

    // **No `CHAR` event. Text arrives through the IME instead — A3.**
    //
    // This is forced, not preferred. `page.rs` registers a
    // `gpui::InputHandler` on the focused page, and gpui delivers ordinary
    // printable keys to a registered handler as text: that is how eave's
    // own composer works, where `TextField` binds no key handler for
    // characters at all and every one arrives through
    // `replace_text_in_range`. So with the handler live, a `CHAR` here
    // would insert every letter a SECOND time. The two paths cannot both
    // carry text, and the IME path is the one that has to win —
    //
    //   * it is the only one that can compose, so CJK works at all; and
    //   * the character comes from AppKit's own interpretation of the key
    //     rather than from `windows_key_code`'s US-layout table, so a
    //     French or German keyboard finally types its own letters. That
    //     was the *other* half of A3 and it is fixed by the same change.
    //
    // `RAWKEYDOWN` and `KEYUP` still go, so pages still get `keydown` and
    // `keyup` — shortcuts, games and key handlers are untouched.
    //
    // **`keypress` still fires, and an earlier version of this comment
    // said it would not.** That prediction was about a first cut which
    // routed every character through `ImeCommitText`; committed IME text
    // does not raise `keypress`. But that cut inserted nothing at all (see
    // [`ime_commit`]), and the fix — [`char_text`], which sends a real
    // `CHAR` — brings `keypress` back with it. **Measured on a live page,
    // not reasoned:** `keydown`, `keypress` and `beforeinput` all fire.
    // The claimed regression does not exist; it is recorded here only
    // because it was written down as fact before it was checked.
    let _ = typed;

    let mut up = base.clone();
    up.type_ = KeyEventType::KEYUP;
    host.send_key_event(Some(&up));
    true
}

// ---------------------------------------------------------------------------
// IME — 06-browser A3
// ---------------------------------------------------------------------------

/// Set the provisional (marked) text — `ImeSetComposition`.
///
/// This is the "not yet committed" stage: `か` on screen, underlined, while
/// the candidate list is open. Chromium shows it in the focused field and
/// replaces it on every call, so eave sends the WHOLE composition each
/// time rather than a delta.
///
/// **`replacement_range: None` means "replace the current composition"**,
/// which is what macOS's own `setMarkedText:` semantics already give us:
/// AppKit hands the full marked string every time. Passing a range here
/// would mean tracking Chromium's document offsets, which eave does not
/// have and does not need.
///
/// One underline over the whole run, `CEF_CUS_SOLID`. Chromium draws it;
/// eave does not paint composing text itself and must not, because the
/// text is inside the page's own field.
pub fn ime_set_composition(browser: &Browser, text: &str, selection: std::ops::Range<usize>) {
    let Some(host) = browser.host() else { return };
    let units = text.encode_utf16().count() as u32;
    let underline = CompositionUnderline {
        size: std::mem::size_of::<cef::sys::_cef_composition_underline_t>(),
        range: CefRange { from: 0, to: units },
        color: 0,
        background_color: 0,
        thick: 0,
        style: cef::sys::cef_composition_underline_style_t::CEF_CUS_SOLID.into(),
    };
    let sel = CefRange {
        from: selection.start as u32,
        to: selection.end as u32,
    };
    host.ime_set_composition(
        Some(&CefString::from(text)),
        Some(&[underline]),
        None,
        Some(&sel),
    );
}

/// Commit a COMPOSITION into the page — `ImeCommitText`.
///
/// **Only valid while a composition is active, and that is measured, not
/// assumed.** The first cut of A3 routed every character through here on
/// the reasoning that one insertion path is better than two. It compiled,
/// the handler registered, `replace_text_in_range` fired 72 times with the
/// right text — and **not one character reached the page**. Nothing
/// errored; `value` simply stayed empty and no `beforeinput` was raised.
///
/// `ImeCommitText` completes an *existing* composition. With none open
/// Chromium has nothing to complete, and the call is a silent no-op. That
/// is why CEF's own OSR sample still sends `CHAR` for ordinary typing and
/// reserves this for the IME — see [`char_text`], which is the other half.
///
/// `relative_cursor_pos` is `0` — the caret lands after the inserted text,
/// which is what every text field does and what AppKit assumes.
pub fn ime_commit(browser: &Browser, text: &str) {
    let Some(host) = browser.host() else { return };
    host.ime_commit_text(Some(&CefString::from(text)), None, 0);
}

/// Insert text that is NOT completing a composition — one `CHAR` per
/// UTF-16 unit.
///
/// The ordinary-typing half of A3, and the one that actually puts letters
/// on the page — **confirmed live**: a page field takes ASCII, shifted
/// characters, digits and punctuation, and raises `keydown`, `keypress`
/// and `beforeinput` for each.
///
/// It is an improvement on the `CHAR` this replaced:
/// the character comes from **AppKit's own interpretation of the key**,
/// delivered through `NSTextInputClient`, rather than from
/// `windows_key_code`'s US-layout table — so a French or German keyboard
/// finally types its own letters, and a dead key (`⌥e` `e` → `é`) arrives
/// as the composed character instead of two wrong ones.
///
/// Per UTF-16 unit because that is what `cef_key_event_t::character` is.
/// A character outside the BMP arrives as a surrogate pair and Chromium
/// reassembles it; splitting it here is correct rather than lossy.
pub fn char_text(browser: &Browser, text: &str) {
    let Some(host) = browser.host() else { return };
    for unit in text.encode_utf16() {
        let ev = KeyEvent {
            size: std::mem::size_of::<cef::sys::_cef_key_event_t>(),
            type_: KeyEventType::CHAR,
            modifiers: 0,
            // For a CHAR event Chromium reads `character`; the key code
            // carries the same value, which is what CEF's own mac client
            // does and what Blink expects for a synthesised character.
            windows_key_code: unit as i32,
            native_key_code: 0,
            is_system_key: 0,
            character: unit,
            unmodified_character: unit,
            focus_on_editable_field: 1,
        };
        host.send_key_event(Some(&ev));
    }
}

/// Throw the composition away — `ImeCancelComposition`.
///
/// The provisional text disappears and nothing is inserted. This is
/// Escape, and it is also what a click elsewhere means.
pub fn ime_cancel(browser: &Browser) {
    if let Some(host) = browser.host() {
        host.ime_cancel_composition();
    }
}

/// Accept whatever is composing as-is — `ImeFinishComposingText`.
///
/// Distinct from [`ime_cancel`] in the direction that matters: this keeps
/// the text. Focus moving away mid-composition means "I meant that", the
/// way every native field behaves.
pub fn ime_finish(browser: &Browser) {
    if let Some(host) = browser.host() {
        host.ime_finish_composing_text(1);
    }
}

// ---------------------------------------------------------------------------
// Drag & drop — 06-browser A5
// ---------------------------------------------------------------------------

fn ops_every() -> DragOperationsMask {
    DragOperationsMask::from(cef_drag_operations_mask_t::DRAG_OPERATION_EVERY)
}

fn ops_none() -> DragOperationsMask {
    DragOperationsMask::from(cef_drag_operations_mask_t::DRAG_OPERATION_NONE)
}

fn ops_copy() -> DragOperationsMask {
    DragOperationsMask::from(cef_drag_operations_mask_t::DRAG_OPERATION_COPY)
}

thread_local! {
    /// The `DragData` `start_dragging` cloned. Lives until [`clear_out`] —
    /// `drag_target_drag_enter` on another page needs the original, and
    /// rebuilding from [`super::drag::OutKind`] would drop image bytes and
    /// fragment HTML we did not project.
    static OUT_DATA: RefCell<Option<DragData>> = const { RefCell::new(None) };
}

pub fn hold_out(data: DragData) {
    OUT_DATA.with(|s| *s.borrow_mut() = Some(data));
}

fn clear_out() {
    OUT_DATA.with(|s| *s.borrow_mut() = None);
}

fn with_out_data(f: impl FnOnce(&mut DragData)) {
    OUT_DATA.with(|s| {
        if let Some(data) = s.borrow_mut().as_mut() {
            f(data);
        }
    });
}

/// OS files have entered this page. Builds a `DragData` from the paths
/// gpui already collected; Chromium does the rest (an `<input type=file>`
/// drop zone, a rich editor, whatever is under the pointer).
pub fn files_enter(
    browser: &Browser,
    at: Point<Pixels>,
    m: &Modifiers,
    paths: &[impl AsRef<Path>],
) {
    let Some(host) = browser.host() else { return };
    let Some(mut data) = drag_data_create() else {
        return;
    };
    for p in paths {
        let s = p.as_ref().to_string_lossy();
        data.add_file(Some(&CefString::from(s.as_ref())), None);
    }
    host.drag_target_drag_enter(Some(&mut data), Some(&event(at, m)), ops_every());
}

pub fn files_over(browser: &Browser, at: Point<Pixels>, m: &Modifiers) {
    if let Some(host) = browser.host() {
        host.drag_target_drag_over(Some(&event(at, m)), ops_every());
    }
}

pub fn files_leave(browser: &Browser) {
    if let Some(host) = browser.host() {
        host.drag_target_drag_leave();
    }
}

pub fn files_drop(browser: &Browser, at: Point<Pixels>, m: &Modifiers) {
    if let Some(host) = browser.host() {
        host.drag_target_drop(Some(&event(at, m)));
    }
}

/// An out-drag (from `start_dragging`) has entered this page — maybe the
/// source, maybe another pane.
pub fn out_enter(browser: &Browser, at: Point<Pixels>, m: &Modifiers) {
    let Some(host) = browser.host() else { return };
    with_out_data(|data| {
        host.drag_target_drag_enter(Some(data), Some(&event(at, m)), ops_every());
    });
}

pub fn out_over(browser: &Browser, at: Point<Pixels>, m: &Modifiers) {
    if let Some(host) = browser.host() {
        host.drag_target_drag_over(Some(&event(at, m)), ops_every());
    }
}

pub fn out_leave(browser: &Browser) {
    if let Some(host) = browser.host() {
        host.drag_target_drag_leave();
    }
}

pub fn out_drop(browser: &Browser, at: Point<Pixels>, m: &Modifiers) {
    if let Some(host) = browser.host() {
        host.drag_target_drop(Some(&event(at, m)));
    }
}

/// The out-drag landed. `op` is COPY: we are not moving a file off disk
/// when a page link is dropped on another page.
pub fn out_end(browser: &Browser, at: Point<Pixels>) {
    if let Some(host) = browser.host() {
        let p = event(at, &Modifiers::default());
        host.drag_source_ended_at(p.x, p.y, ops_copy());
    }
    clear_out();
}

/// The out-drag was cancelled (mouse-up on chrome, tab closed, pointer
/// left the window). `drag_source_system_drag_ended` is the "not dropped
/// on us" close; `ended_at` with NONE is the same hang if we skip it.
pub fn out_cancel(browser: &Browser) {
    if let Some(host) = browser.host() {
        host.drag_source_ended_at(0, 0, ops_none());
        host.drag_source_system_drag_ended();
    }
    clear_out();
}

/// A tab is closing while it is the source of an out-drag. Tell Chromium
/// before the browser is gone; [`super::drag::forget_tab`] then forgets
/// the chrome half.
pub fn abort_out_for(tab_id: &str) {
    if super::drag::out_source().as_deref() != Some(tab_id) {
        return;
    }
    super::tabs::with_browser(tab_id, |b| out_cancel(b));
    clear_out();
}
