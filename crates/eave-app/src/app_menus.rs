//! The macOS menu bar (Slice Y).
//!
//! **gpui only calls `setMainMenu_` from `App::set_menus`.** Skip it and
//! `NSApp.mainMenu` stays nil for the life of the process: no app menu, no
//! Window menu, and nothing for the auto-hidden system menu bar to reveal
//! when the pointer reaches the top of the screen. eave was in that state —
//! not because a binding was missing, but because nobody had ever handed
//! the platform a menu.
//!
//! Worth stating precisely, because it was misdiagnosed twice: **⌘Q always
//! worked.** `bootstrap` binds it and handles it with `cx.quit()`, and a
//! key binding does not go through the menu bar. What was missing is the
//! bar itself, and with it ⌘W, ⌘M and ⌘H — the three a macOS user reaches
//! for without thinking and which nothing in eave answered.
//!
//! Structure ported from comet's `app_menus.rs` (MIT), which ports zed's.
//! Narrower than either: eave has no appearance-mode picker and no
//! document-style New Window. It does have two second windows opened from
//! this bar: Settings (⌘,, Slice S1) and the Theme Editor (bezel's Create
//! page, ⌘⇧,).

use gpui::{actions, App, Menu, MenuItem, Window};

actions!(
    eave_app,
    [
        About,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow
    ]
);

/// Register the handlers and hand the platform a menu.
///
/// Called from `bootstrap` so the harness boots the same wiring the app
/// does — a menu registered only in `main` is a menu no test ever sees.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Window actions act on the ACTIVE window rather than a captured
    // handle: eave opens one window today, and a captured handle would be
    // the bug waiting for the day it opens two.
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |w| w.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |w| w.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |w| w.remove_window()));
}

/// Hand the platform the bar. **Call this immediately before
/// `cx.activate(true)`, not from [`init`].**
///
/// This is the whole slice's trap and it cost a live debugging round. A
/// `setMainMenu_` issued early in `bootstrap` — before the window exists
/// and before the app activates — does not survive: the process runs with
/// `NSApp.mainMenu` still nil, and every accessibility query answers
/// "Can't get menu bar 1 … Invalid index". Nothing errors and nothing
/// logs; the menu is simply not there.
///
/// comet has the same split for the same reason: `app_menus::init` is
/// handlers only, and `set_menus` sits on the line above `cx.activate`
/// (`comet/crates/ui/src/lib.rs:189-190`).
pub fn install(cx: &mut App) {
    cx.set_menus(menus());
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}

/// The bar itself.
///
/// Plain actions rather than `OsAction`: at eave's gpui pin `OsAction`
/// covers only Cut/Copy/Paste/SelectAll/Undo/Redo — Minimize and Close are
/// not in that enum. comet reaches for `OsAction::Minimize`, which means
/// comet is on a LATER rev of the same fork than eave's `5d1f83d`. So the
/// chords are bound in `bootstrap` alongside ⌘Q instead, which produces
/// the same behaviour and the same menu glyphs, just without the system
/// specialisation.
fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "eave".into(),
            disabled: false,
            items: vec![
                // Disabled rather than absent: an About item that opens
                // nothing is a lie, and an app menu with no About at all
                // reads as unfinished. Stating "there is one, it does
                // nothing yet" is the honest middle.
                MenuItem::action("About eave", About).disabled(true),
                MenuItem::separator(),
                // Where every macOS app keeps it, chord glyph derived from
                // the ⌘, binding in `bootstrap`. Present because eave now
                // HAS the surface — see the negative test below, which
                // guarded this exact row until S1 built the window.
                MenuItem::action("Settings…", crate::settings_window::OpenSettings),
                MenuItem::action("Customize Theme", crate::theme_editor::OpenThemeEditor),
                MenuItem::separator(),
                MenuItem::action("Hide eave", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit eave", crate::ui::Quit),
            ],
        },
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bar names what eave actually has.
    ///
    /// gpui hands `set_menus` straight to `setMainMenu_`, so nothing in a
    /// unit test can observe the result — `NSApp.mainMenu` is platform
    /// state. What IS checkable is the structure handed over, and the thing
    /// worth checking is the omission: a menu naming a Settings window or
    /// an appearance mode would be a menu that lies. Theme Editor is
    /// present because eave now has that surface.
    #[test]
    fn the_bar_names_only_what_eave_has() {
        let m = menus();
        let names: Vec<&str> = m.iter().map(|x| x.name.as_ref()).collect();
        assert_eq!(names, vec!["eave", "Window"]);

        let app_items = m[0].items.len();
        let window_items = m[1].items.len();
        assert!(app_items >= 5, "the app menu lost items: {app_items}");
        assert!(
            window_items >= 3,
            "the window menu lost items: {window_items}"
        );

        let app_names: Vec<String> = m[0]
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            app_names.iter().any(|n| n == "Customize Theme"),
            "the app menu lost Customize Theme: {app_names:?}"
        );
        // Present since S1 built the window; before that, this very test
        // forbade the word. The row and the surface must arrive together,
        // and this assertion plus the negative list below is that contract
        // from both sides.
        assert!(
            app_names.iter().any(|n| n == "Settings…"),
            "the app menu lost Settings: {app_names:?}"
        );

        // Nothing in either menu may claim a surface eave does not have.
        // This is a NEGATIVE test on purpose: the failure it prevents is
        // someone porting another row out of comet's menu, which has three
        // appearance modes and a document-style New Window. "settings" and
        // "preferences" left this list when S1 built the real window.
        for menu in &m {
            for item in &menu.items {
                if let MenuItem::Action { name, .. } = item {
                    let n = name.to_lowercase();
                    for absent in ["appearance", "new window"] {
                        assert!(
                            !n.contains(absent),
                            "the menu offers {name:?}, and eave has no such surface"
                        );
                    }
                }
            }
        }
    }
}
