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
//! arrows — enough to prove a form works. Chromium's own macOS translation
//! table is roughly 300 lines and handles layouts, dead keys and the numeric
//! keypad; that is the real bill, and it is not paid here.

use cef::sys::cef_event_flags_t;
use cef::{Browser, ImplBrowser, ImplBrowserHost, KeyEvent, KeyEventType, MouseButtonType, MouseEvent};
use gpui::{Keystroke, Modifiers, Pixels, Point};

/// CEF's modifier bitfield from gpui's modifiers.
pub fn flags(m: &Modifiers) -> u32 {
    let mut f = 0u32;
    if m.shift {
        f |= cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0 as u32;
    }
    if m.control {
        f |= cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0 as u32;
    }
    if m.alt {
        f |= cef_event_flags_t::EVENTFLAG_ALT_DOWN.0 as u32;
    }
    if m.platform {
        f |= cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0 as u32;
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
            ev.modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0 as u32;
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

pub fn wheel(browser: &Browser, at: Point<Pixels>, m: &Modifiers, dx: i32, dy: i32) {
    if let Some(host) = browser.host() {
        host.send_mouse_wheel_event(Some(&event(at, m)), dx, dy);
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
                .or_else(|| Some(c as u16));
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

    if let Some(ch) = typed {
        // No CHAR event when a command modifier is held: that is a shortcut,
        // not typing, and inserting the character would be wrong.
        if !ks.modifiers.platform && !ks.modifiers.control {
            let mut ev = base.clone();
            ev.type_ = KeyEventType::CHAR;
            ev.character = ch;
            ev.unmodified_character = ch;
            host.send_key_event(Some(&ev));
        }
    }

    let mut up = base.clone();
    up.type_ = KeyEventType::KEYUP;
    host.send_key_event(Some(&up));
    true
}

pub const fn left() -> MouseButtonType {
    MouseButtonType::LEFT
}
