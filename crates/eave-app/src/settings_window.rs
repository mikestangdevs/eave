//! The Settings window (Slice S1) — frame + the real panes.
//!
//! # The stance, stated once
//! Every row here is wired to something that exists. The layout grammar is
//! borrowed from the reference shots (`frame-refs/radius-settings/`): a
//! toolbar of panes across the top, grouped cards below, label-left
//! value-right rows — and the CONTENTS deliberately are not. The reference
//! has profile rows, an update channel, a "Coming soon" badge; eave has
//! none of those capabilities, so it has none of those rows. A settings
//! row that lies about a capability is worse than no row.
//!
//! # The window
//! A real second window on ⌘, — the Theme Editor's open-or-raise shape
//! (`theme_editor.rs`), which is the pattern already walked in production
//! with CEF pumping beside it. The pump repaints ALL windows on a
//! delivered page frame (`web/host.rs` `refresh_windows`), so this window
//! redraws at page framerate while a page animates; the Theme Editor has
//! lived with that since it landed, and it is a cost, not a defect.
//!
//! ⌘W closes this window: the chord is bound to `eave::CloseTab` globally,
//! and the Shell is the only element that handles it, so in any other
//! window it would otherwise dispatch into silence. The root here handles
//! it as "close the window", which is what ⌘W means when there is no tab.
//!
//! # State
//! Every pane reads and writes `settings.json` through `crate::settings`
//! — one struct, serde-defaulted, live-applied. The one exception is the
//! rail width, whose persistence predates this window (`frame.json`,
//! Slice D3) and stays there; the settings store carries its LIVE value so
//! this pane and the rail's own drag see one number. The module doc of
//! `settings.rs` owns that reasoning.

use gpui::{
    div, point, prelude::*, px, size, App, Bounds, Context, Div, DragMoveEvent, Empty, Entity,
    FocusHandle, Render, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowOptions,
};

use bezel::theme::Theme;
use bezel::ui::widgets::{self, Controls as _};

use crate::settings::{self, AppearanceSetting, SearchEngine};
use crate::text_field::TextField;
use crate::{icons, theme};

// ---------------------------------------------------------------------------
// The macOS System Settings dark palette — literals, not theme tokens.
//
// This window deliberately does NOT wear eave's chrome. The operator's
// ruling (2026-08-25): a settings window should read as macOS system UI,
// like the frame-refs — which are themselves app-drawn mimicry of the
// System Settings look, since eave (like the reference app) renders every
// pixel itself and hosts no AppKit widget anywhere. So the imitation is
// the spec: solid dark chrome, grouped cards on hairlines, a centered
// title, native-shaped segmented controls. Values eyeballed against the
// frame-refs and Apple's dark grouped-list colors; the ACCENT is the
// user's own `AppleAccentColor` (a later ruling, same day) — see
// [`system_accent`] below.
// ---------------------------------------------------------------------------

/// The window and content ground — solid, like System Settings, not
/// eave's frosted glass. The glass ruling governs the product frame; a
/// settings window that must read "system" reads solid.
fn win_bg() -> gpui::Rgba {
    gpui::rgb(0x242426)
}
/// A grouped card's plate.
fn card_bg() -> gpui::Rgba {
    gpui::rgb(0x2e2e31)
}
/// Hairlines: card borders and row separators.
fn hairline() -> gpui::Rgba {
    gpui::rgba(0xffffff17)
}
/// Primary label.
fn label_color() -> gpui::Rgba {
    gpui::rgba(0xfffffff2)
}
/// Secondary text — captions, values, unselected toolbar items.
fn secondary() -> gpui::Rgba {
    gpui::rgb(0x9d9da3)
}
/// A segmented control's container well.
fn seg_well() -> gpui::Rgba {
    gpui::rgba(0xffffff12)
}
/// The selected segment's raised knob.
fn seg_knob() -> gpui::Rgba {
    gpui::rgb(0x636367)
}
/// The selected toolbar item's plate.
fn toolbar_sel() -> gpui::Rgba {
    gpui::rgba(0xffffff14)
}

/// The USER'S macOS accent colour (System Settings → Appearance), not
/// eave's brand accent — the operator's ruling: this window wears the
/// system's colour like a system window would. Read once per process
/// exactly like `reduce_motion` in `ui.rs` (`AppleAccentColor` in the
/// global domain via CFPreferences; cfprefsd caches per process, so a
/// change lands on relaunch — stated, not papered over with a poller).
/// The dark-appearance variants of Apple's palette, since this window is
/// dark. An absent key is the default: systemBlue. ("Multicolour" also
/// reads as absent and so lands on blue — the one wrinkle, noted here.)
fn system_accent() -> gpui::Rgba {
    static ACCENT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    gpui::rgb(*ACCENT.get_or_init(|| accent_rgb(read_apple_accent())))
}

/// `AppleAccentColor`'s integer → Apple's dark-appearance accent, as a
/// pure map so both rungs are testable without touching cfprefsd.
fn accent_rgb(pref: Option<i64>) -> u32 {
    match pref {
        Some(-1) => 0x98989d, // graphite
        Some(0) => 0xff453a,  // red
        Some(1) => 0xff9f0a,  // orange
        Some(2) => 0xffd60a,  // yellow
        Some(3) => 0x32d74b,  // green
        Some(5) => 0xbf5af2,  // purple
        Some(6) => 0xff375f,  // pink
        // 4 is blue; absent is the un-chosen default, also blue.
        _ => 0x0a84ff,
    }
}

/// `CFPreferencesCopyAppValue("AppleAccentColor", any-application)` as an
/// integer. `None` is a machine where the user never chose — the normal
/// state, not an error. Shape lifted from `macos_reduce_motion` (ui.rs).
#[cfg(target_os = "macos")]
fn read_apple_accent() -> Option<i64> {
    use std::ffi::c_void;
    type CFTypeRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFTypeRef;
        fn CFPreferencesCopyAppValue(key: CFTypeRef, application_id: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(number: CFTypeRef, the_type: isize, value_ptr: *mut c_void) -> u8;
        fn CFRelease(cf: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;
    /// `kCFNumberSInt64Type`.
    const SINT64: isize = 4;
    // SAFETY: same contract as `macos_reduce_motion` — every pointer is
    // null-checked or released, none outlives this function.
    unsafe {
        let mk = |s: &str| -> CFTypeRef {
            CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0)
        };
        let key = mk("AppleAccentColor");
        let domain = mk("kCFPreferencesAnyApplication");
        if key.is_null() || domain.is_null() {
            if !key.is_null() {
                CFRelease(key);
            }
            if !domain.is_null() {
                CFRelease(domain);
            }
            return None;
        }
        let value = CFPreferencesCopyAppValue(key, domain);
        CFRelease(key);
        CFRelease(domain);
        if value.is_null() {
            return None;
        }
        let out = if CFGetTypeID(value) == CFNumberGetTypeID() {
            let mut n: i64 = 0;
            if CFNumberGetValue(value, SINT64, &mut n as *mut i64 as *mut c_void) != 0 {
                Some(n)
            } else {
                None
            }
        } else {
            None
        };
        CFRelease(value);
        out
    }
}

#[cfg(not(target_os = "macos"))]
fn read_apple_accent() -> Option<i64> {
    None
}

gpui::actions!(eave_settings, [OpenSettings]);

struct SettingsWindows(Option<WindowHandle<SettingsWindow>>);
impl gpui::Global for SettingsWindows {}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenSettings, cx| {
        open(cx);
    });
}

/// Open the window, or raise it if it is already up. The stale-handle case
/// is `update(..).is_ok()`, not a close hook — gpui has none; a closed
/// window's handle errors and a fresh one is opened over it, exactly as
/// the Theme Editor does. Returns the handle so the harness can drive the
/// window it opened; the action handler drops it.
pub fn open(cx: &mut App) -> Option<WindowHandle<SettingsWindow>> {
    if let Some(handle) = cx.try_global::<SettingsWindows>().and_then(|g| g.0.clone()) {
        if handle
            .update(cx, |_, window, _| {
                window.activate_window();
            })
            .is_ok()
        {
            return Some(handle);
        }
    }
    let bounds = Bounds::centered(None, size(px(715.0), px(640.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(560.0), px(440.0))),
            // Solid, not eave's glass: System Settings is opaque, and the
            // ruling this window answers to is "reads as macOS system UI".
            window_background: WindowBackgroundAppearance::Opaque,
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(theme::TRAFFIC_X), px(theme::TRAFFIC_Y))),
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(SettingsWindow::new);
            // Focus the root so ⌘W has a dispatch path before any field
            // is clicked — a window nothing focuses is a window whose
            // chords go nowhere.
            let focus = view.read(cx).focus.clone();
            window.focus(&focus, cx);
            view
        },
    );
    if let Ok(handle) = opened {
        cx.set_global(SettingsWindows(Some(handle)));
        return Some(handle);
    }
    None
}

/// The panes. Only these — see the module doc's stance paragraph.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    General,
    Tabs,
    Agents,
    Shortcuts,
    Info,
}

/// Toolbar order, icon, label — one table so the toolbar and the panes
/// cannot disagree about what exists.
const PANES: [(Pane, &str, &str); 5] = [
    (Pane::General, icons::SETTINGS_MINIMALISTIC, "General"),
    (Pane::Tabs, icons::WIDGET, "Tabs"),
    (Pane::Agents, icons::CHAT_ROUND_LINE, "Agents"),
    (Pane::Shortcuts, icons::KEYBOARD, "Shortcuts"),
    (Pane::Info, icons::INFO_CIRCLE, "Info"),
];

pub struct SettingsWindow {
    focus: FocusHandle,
    pane: Pane,
    /// The Shortcuts pane's filter line.
    query: Entity<TextField>,
    /// The custom search prefix, alive across pane switches so a
    /// half-typed URL survives a detour to another pane.
    custom_search: Entity<TextField>,
}

impl SettingsWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            TextField::new(cx)
                .with_placeholder("Search shortcuts…")
                .with_debug_name("settings-shortcut-query")
        });
        let custom_search = cx.new(|cx| {
            let mut f = TextField::new(cx)
                .with_placeholder("https://example.com/search?q=")
                .with_debug_name("settings-custom-search");
            if let SearchEngine::Custom { prefix } = settings::snapshot().search {
                f.set_content(prefix, cx);
            }
            f
        });
        // The filter re-renders on every edit; the custom prefix WRITES on
        // every edit. A per-keystroke write of a ~200-byte file through
        // temp+rename is the same cost the theme editor pays per slider
        // move, and applying on blur would mean a prefix that looks set
        // and is not — the quiet failure this window exists to remove.
        cx.observe(&query, |_, _, cx| cx.notify()).detach();
        cx.observe(&custom_search, |_, field, cx| {
            let typed = field.read(cx).content().to_string();
            if matches!(settings::snapshot().search, SearchEngine::Custom { ref prefix } if *prefix != typed)
            {
                settings::update(cx, |s| s.search = SearchEngine::Custom { prefix: typed });
            }
            cx.notify();
        })
        .detach();
        Self {
            focus: cx.focus_handle(),
            pane: Pane::General,
            query,
            custom_search,
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = Theme::of(cx).clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(win_bg())
            .text_color(label_color())
            .font_family(theme.font_sans.clone())
            .track_focus(&self.focus)
            // ⌘W: see the module doc. The Shell is the only other handler
            // of this action, and it does not propagate, so this cannot
            // steal a tab-close from the main window.
            .on_action(cx.listener(|_, _: &crate::ui::CloseTab, window, _| {
                window.remove_window();
            }))
            .child(
                // A centered semibold title, the reference's shape — not
                // the muted left-parked label the Theme Editor wears.
                div()
                    .flex_none()
                    .h(px(theme::TITLEBAR_H))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(label_color())
                            .child("Settings"),
                    ),
            )
            .child(self.toolbar(&theme, cx))
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(24.0))
                    .pt(px(16.0))
                    .pb(px(32.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .children(match self.pane {
                        Pane::General => self.general_pane(&theme, cx),
                        Pane::Tabs => self.tabs_pane(&theme, cx),
                        Pane::Agents => crate::settings_agents::render(&theme, cx),
                        Pane::Shortcuts => self.shortcuts_pane(&theme, cx),
                        Pane::Info => info_pane(&theme),
                    }),
            )
    }
}

// ---------------------------------------------------------------------------
// The frame's parts
// ---------------------------------------------------------------------------

impl SettingsWindow {
    /// The pane toolbar: icon over label, the active pane on a quiet
    /// plate. The reference grammar, minus its panes eave has no backing
    /// features for.
    fn toolbar(&self, _theme: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .flex_none()
            .px(px(16.0))
            .pb(px(8.0))
            .border_b_1()
            .border_color(hairline())
            .flex()
            .flex_row()
            .justify_center()
            .gap(px(2.0))
            .children(PANES.iter().map(|(pane, glyph, label)| {
                let selected = self.pane == *pane;
                let pane = *pane;
                // The reference's selection: accent icon AND label on a
                // quiet plate; everything unselected is the secondary
                // gray. The accent is the USER'S system accent, not the
                // brand's — see `system_accent`.
                let color: gpui::Hsla = if selected {
                    system_accent().into()
                } else {
                    secondary().into()
                };
                div()
                    .id(SharedString::from(format!("settings-pane-{label}")))
                    .debug_selector(|| format!("settings-pane-{label}"))
                    .cursor_pointer()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.0))
                    .px(px(13.0))
                    .py(px(5.0))
                    .rounded(px(8.0))
                    .when(selected, |d| d.bg(toolbar_sel()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pane = pane;
                        cx.notify();
                    }))
                    .child(icons::icon(glyph, 19.0, color))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(color)
                            .child(SharedString::from(*label)),
                    )
            }))
    }
}

/// A group's title over its card.
pub(crate) fn section_heading(_theme: &Theme, label: &'static str) -> Div {
    div()
        .pt(px(6.0))
        .px(px(2.0))
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(secondary())
        .child(label)
}

/// A grouped card: rows stacked with hairlines between them — the macOS
/// grouped-list plate, hairline-bordered, radius 10.
pub(crate) fn card(_theme: &Theme, rows: Vec<gpui::AnyElement>) -> Div {
    let last = rows.len().saturating_sub(1);
    div()
        .rounded(px(10.0))
        .bg(card_bg())
        .border_1()
        .border_color(hairline())
        .flex()
        .flex_col()
        .children(rows.into_iter().enumerate().map(move |(i, row)| {
            div()
                .px(px(16.0))
                .py(px(11.0))
                .when(i != last, |d| d.border_b_1().border_color(hairline()))
                .child(row)
                .into_any_element()
        }))
}

/// Label left, control right — the row grammar every pane speaks.
pub(crate) fn row(_theme: &Theme, label: impl Into<SharedString>, control: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(label_color())
                .child(label.into()),
        )
        .child(div().ml_auto().child(control))
        .into_any_element()
}

/// One quiet line under a control — the honest footnote, never a promise.
pub(crate) fn caption(_theme: &Theme, text: impl Into<SharedString>) -> gpui::AnyElement {
    div()
        .text_size(px(11.5))
        .text_color(secondary())
        .child(text.into())
        .into_any_element()
}

/// A segmented choice, drawn as macOS draws one: a recessed well, the
/// chosen segment a raised gray knob with white text. Bezel's toggle
/// group (chips in a row) is what made the first cut read as app UI; the
/// ids and debug selectors are unchanged, so the harness cannot tell.
fn segmented<T: PartialEq + Copy + 'static>(
    theme: &Theme,
    current: T,
    items: &[(T, &'static str)],
    on_pick: impl Fn(T, &mut App) + Clone + 'static,
    cx: &mut Context<SettingsWindow>,
) -> gpui::AnyElement {
    segmented_with_ids(theme, "settings-seg", current, items, on_pick, cx)
}

/// The same control with the id/selector prefix chosen by the caller —
/// for a pane drawing several groups that share labels (S2's Agents
/// pane repeats a model id once per provider). `debug_bounds` is one
/// map keyed by selector, last-painted wins, so a fixed prefix would
/// let a harness click land on the wrong provider's segment.
pub(crate) fn segmented_with_ids<T: PartialEq + Copy + 'static>(
    _theme: &Theme,
    id_prefix: &str,
    current: T,
    items: &[(T, &'static str)],
    on_pick: impl Fn(T, &mut App) + Clone + 'static,
    cx: &mut Context<SettingsWindow>,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .p(px(2.0))
        .rounded(px(7.0))
        .bg(seg_well())
        .children(items.iter().map(|(value, label)| {
            let value = *value;
            let selected = current == value;
            let on_pick = on_pick.clone();
            let selector = format!("{id_prefix}-{label}");
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector)
                .cursor_pointer()
                .px(px(11.0))
                .py(px(3.0))
                .rounded(px(5.0))
                .text_size(px(12.0))
                .when(selected, |d| {
                    d.bg(seg_knob())
                        .text_color(label_color())
                        .font_weight(gpui::FontWeight::MEDIUM)
                })
                .when(!selected, |d| d.text_color(gpui::rgba(0xebebf5b8)))
                .on_click(cx.listener(move |_, _, _, cx| {
                    on_pick(value, cx);
                    cx.notify();
                }))
                .child(SharedString::from(*label))
        }))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

impl SettingsWindow {
    fn general_pane(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let s = settings::snapshot();
        let mut out: Vec<gpui::AnyElement> = Vec::new();

        // Appearance. System follows macOS; Light and Dark pin. Customize
        // Theme edits the active appearance's own brand.
        out.push(section_heading(theme, "Appearance").into_any_element());
        out.push(
            card(
                theme,
                vec![
                    row(
                        theme,
                        "Appearance",
                        segmented(
                            theme,
                            s.appearance,
                            &[
                                (AppearanceSetting::System, "System"),
                                (AppearanceSetting::Light, "Light"),
                                (AppearanceSetting::Dark, "Dark"),
                            ],
                            |v, cx| settings::update(cx, |s| s.appearance = v),
                            cx,
                        ),
                    ),
                    caption(
                        theme,
                        "System follows macOS. Light and Dark share colors; frost is per appearance.",
                    ),
                ],
            )
            .into_any_element(),
        );

        out.push(section_heading(theme, "Import").into_any_element());
        out.push(
            card(
                theme,
                vec![
                    row(
                        theme,
                        "Import from another browser",
                        div()
                            .id("settings-import")
                            .debug_selector(|| "settings-import".into())
                            .cursor_pointer()
                            .text_size(px(13.0))
                            .text_color(system_accent())
                            .child("Import…")
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(
                                    Box::new(crate::import_dialog::OpenImportDialog),
                                    cx,
                                );
                            }))
                            .into_any_element(),
                    ),
                    caption(
                        theme,
                        "Bring bookmarks and history from Brave, Chrome, Safari, or Arc.",
                    ),
                ],
            )
            .into_any_element(),
        );

        // Search engine. The sentence under it is derived from the prefix
        // that will actually receive the query (`engine_host`), so the row
        // cannot drift from the wire — and when the EAVE_SEARCH_URL env
        // override is set, the sentence says that instead of lying about
        // the picker being in charge.
        let env_override = std::env::var("EAVE_SEARCH_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let engine = match s.search {
            SearchEngine::Google => 0usize,
            SearchEngine::DuckDuckGo => 1,
            SearchEngine::Custom { .. } => 2,
        };
        let mut search_rows = vec![row(
            theme,
            "Search engine",
            segmented(
                theme,
                engine,
                &[(0usize, "Google"), (1, "DuckDuckGo"), (2, "Custom")],
                {
                    let custom = self.custom_search.clone();
                    move |v, cx| {
                        let prefix = custom.read(cx).content().to_string();
                        settings::update(cx, move |s| {
                            s.search = match v {
                                0 => SearchEngine::Google,
                                1 => SearchEngine::DuckDuckGo,
                                _ => SearchEngine::Custom { prefix },
                            }
                        })
                    }
                },
                cx,
            ),
        )];
        if engine == 2 {
            search_rows.push(row(
                theme,
                "Custom prefix",
                input_box(theme, self.custom_search.clone()),
            ));
            if matches!(&s.search, SearchEngine::Custom { prefix } if prefix.trim().is_empty()) {
                search_rows.push(caption(
                    theme,
                    "Empty — searches fall back to Google until a prefix is set.",
                ));
            }
        }
        search_rows.push(caption(
            theme,
            match &env_override {
                Some(prefix) => SharedString::from(format!(
                    "Overridden for this run: EAVE_SEARCH_URL sends searches to {}.",
                    crate::omnibox::engine_host(prefix)
                )),
                None => SharedString::from(format!(
                    "Searches go to {} — the query is appended to the prefix.",
                    crate::omnibox::engine_host(&s.search.prefix())
                )),
            },
        ));
        out.push(section_heading(theme, "Search").into_any_element());
        out.push(card(theme, search_rows).into_any_element());

        // Rail rest width — D3's persistence, surfaced. The slider writes
        // the same `frame.json` key the rail's own drag writes, through
        // the same clamp; the frame follows live via the settings store.
        let rail_w = cx.global::<settings::SettingsStore>().rail_w;
        let frac = (rail_w - theme::RAIL_W_MIN) / (theme::RAIL_W_MAX - theme::RAIL_W_MIN);
        out.push(section_heading(theme, "Frame").into_any_element());
        out.push(
            card(
                theme,
                vec![
                    row(
                        theme,
                        "Rail rest width",
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div().w(px(180.0)).child(
                                    theme
                                        .slider(frac.clamp(0.0, 1.0))
                                        .id("settings-rail-w")
                                        .cursor_pointer()
                                        .on_drag(RailKnobDrag, |_, _, _, cx| cx.new(|_| Empty))
                                        .on_drag_move(cx.listener(
                                            |_, ev: &DragMoveEvent<RailKnobDrag>, _, cx| {
                                                let f = widgets::axis_fraction(
                                                    ev.event.position,
                                                    ev.bounds,
                                                    gpui::Axis::Horizontal,
                                                    0.0,
                                                );
                                                settings::set_rail_w(
                                                    cx,
                                                    theme::RAIL_W_MIN
                                                        + f * (theme::RAIL_W_MAX
                                                            - theme::RAIL_W_MIN),
                                                );
                                                cx.notify();
                                            },
                                        )),
                                ),
                            )
                            .child(
                                div()
                                    .w(px(52.0))
                                    .text_size(px(12.5))
                                    .text_color(secondary())
                                    .child(SharedString::from(format!("{rail_w:.0} px"))),
                            )
                            .into_any_element(),
                    ),
                    caption(
                        theme,
                        "The width the rail rests at; dragging its edge writes the same value.",
                    ),
                ],
            )
            .into_any_element(),
        );
        out
    }
}

/// The rail slider's drag id — its own payload type for the same reason
/// the theme editor's knobs carry ids: gpui delivers a drag-move to every
/// listener of the payload TYPE.
#[derive(Clone)]
struct RailKnobDrag;

/// A text field in the card grammar: a recessed plate, the field inside.
pub(crate) fn input_box(_theme: &Theme, field: Entity<TextField>) -> gpui::AnyElement {
    div()
        .w(px(280.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .bg(gpui::rgba(0xffffff0f))
        .border_1()
        .border_color(hairline())
        .child(field)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

/// The eviction choices. 20 minutes is E3's decided default
/// (`thresholds::PAGE_IDLE_EVICT_AFTER` owns the reasoning); 5 and 60
/// bracket it an octave either side — positions on a dial, not derived
/// values, and the dial's midpoint is the derived one.
const EVICT_CHOICES: [(Option<u32>, &str); 4] = [
    (Some(5), "5 min"),
    (Some(20), "20 min"),
    (Some(60), "1 hour"),
    (None, "Off"),
];

impl SettingsWindow {
    fn tabs_pane(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let s = settings::snapshot();
        let mut out: Vec<gpui::AnyElement> = Vec::new();

        out.push(section_heading(theme, "Memory").into_any_element());
        let mut evict_rows = vec![row(
            theme,
            "Return an idle tab's memory after",
            segmented(
                theme,
                s.evict_after_min,
                &EVICT_CHOICES,
                |v, cx| settings::update(cx, |s| s.evict_after_min = v),
                cx,
            ),
        )];
        // A hand-edited settings.json can hold a value the dial doesn't
        // offer. Stated rather than silently rounded to a segment.
        if let Some(min) = s.evict_after_min {
            if !EVICT_CHOICES.iter().any(|(v, _)| *v == Some(min)) {
                evict_rows.push(caption(
                    theme,
                    SharedString::from(format!(
                        "Currently {min} min, set by hand in settings.json — picking a segment replaces it."
                    )),
                ));
            }
        }
        evict_rows.push(caption(
            theme,
            match s.evict_after_min {
                // The cost of Off, with its conditions attached (E3's
                // measurement: trivial local pages, this machine).
                None => {
                    "Off keeps every visited tab's browser resident — ~60 MB per tab \
                     above Chromium's 0.32 GB floor, measured on trivial pages."
                }
                _ => {
                    "An evicted tab keeps its place, url and history; returning reloads \
                     the page — scroll and form state are not kept."
                }
            },
        ));
        out.push(card(theme, evict_rows).into_any_element());

        out.push(section_heading(theme, "New tabs").into_any_element());
        out.push(
            card(
                theme,
                vec![
                    row(
                        theme,
                        "A link that opens a new tab",
                        segmented(
                            theme,
                            s.switch_to_background_tabs,
                            &[(false, "Stays in the background"), (true, "Is switched to")],
                            |v, cx| settings::update(cx, |s| s.switch_to_background_tabs = v),
                            cx,
                        ),
                    ),
                    caption(
                        theme,
                        "⌘-click, middle-click, target=\"_blank\", and the Open in New Tab menu rows.",
                    ),
                ],
            )
            .into_any_element(),
        );
        out
    }
}

// ---------------------------------------------------------------------------
// Shortcuts — a viewer of every chord that exists. Read-only v1.
// ---------------------------------------------------------------------------

pub(crate) struct ShortcutRow {
    pub label: String,
    pub chords: Vec<String>,
}

pub(crate) struct ShortcutGroup {
    pub name: &'static str,
    pub rows: Vec<ShortcutRow>,
}

/// Group order — presentation, not discovery. Discovery is the live
/// keymap: an action bound tomorrow lands in its namespace's group (or
/// "Other") without this file changing, so the viewer cannot silently
/// omit a chord that exists.
const GROUP_ORDER: [&str; 9] = [
    "Application",
    "Tabs",
    "Navigation",
    "Page",
    "Sessions",
    "Composer",
    "Slash palette",
    "Text editing",
    "Other",
];

/// Chords that exist but are not `KeyBinding`s — each one is a raw
/// `on_key` arm in `ui.rs`, cited per row. The keymap cannot know them,
/// and a viewer of "every chord that exists" without them would be lying
/// by omission.
const SUPPLEMENT: [(&str, &str, &str); 4] = [
    // `ui.rs` composer / omnibox / find-bar / rename arms all read the
    // clipboard directly; ⌘V is deliberately not an action there.
    ("Composer", "Paste", "⌘V"),
    // The composer's kill arm — a dev affordance, present, so listed.
    ("Composer", "Kill the active session", "⌃K"),
    // The find bar's enter/shift-enter arms.
    ("Page", "Find: next / previous match", "↩ · ⇧↩"),
    // Every overlay, menu and dialog dismisses on escape.
    ("Application", "Dismiss a menu, overlay or dialog", "⎋"),
];

/// What one action is called on a row, and where it sits. `None` falls
/// back to the de-camel-cased action name in a namespace-derived group —
/// unknown actions APPEAR rather than vanish.
fn describe(action: &str) -> Option<(&'static str, &'static str)> {
    Some(match action {
        "eave::Quit" => ("Application", "Quit eave"),
        "eave::FrictionLog" => ("Application", "Append to the friction log"),
        "eave_app::Minimize" => ("Application", "Minimize the window"),
        "eave_app::Hide" => ("Application", "Hide eave"),
        "eave_settings::OpenSettings" => ("Application", "Settings"),
        "eave_theme::OpenThemeEditor" => ("Application", "Customize Theme"),
        "eave_import::OpenImportDialog" => ("Application", "Import from another browser"),
        "eave::NewWindow" => ("Application", "New window"),
        "eave::NewTab" => ("Tabs", "New tab"),
        "eave::NewIncognitoTab" => ("Tabs", "New incognito tab"),
        "eave::CloseTab" => ("Tabs", "Close the tab — the window when none is open"),
        "eave::NextTab" => ("Tabs", "Next tab"),
        "eave::PrevTab" => ("Tabs", "Previous tab"),
        "eave::ActivateLastTab" => ("Tabs", "Switch to the last tab"),
        "eave::ReopenClosedTab" => ("Tabs", "Reopen the closed tab"),
        "eave::FocusOmnibox" => ("Navigation", "Open the omnibox"),
        "eave::GoBack" => ("Navigation", "Back"),
        "eave::GoForward" => ("Navigation", "Forward"),
        "eave::ReloadTab" => ("Navigation", "Reload the page"),
        "eave::FindInPage" => ("Page", "Find in page"),
        "eave::ZoomIn" => ("Page", "Zoom in"),
        "eave::ZoomOut" => ("Page", "Zoom out"),
        "eave::ZoomReset" => ("Page", "Reset zoom"),
        "eave::ToggleDevtools" => ("Page", "Toggle DevTools"),
        "eave::SelectPrevSession" => ("Sessions", "Previous session in the rail"),
        "eave::SelectNextSession" => ("Sessions", "Next session in the rail"),
        "eave::ApproveOldest" => ("Sessions", "Approve the oldest ask"),
        "eave::DenyOldest" => ("Sessions", "Deny the oldest ask"),
        "eave::SendMessage" => ("Composer", "Send"),
        "eave::SlashNext" => ("Slash palette", "Next suggestion"),
        "eave::SlashPrev" => ("Slash palette", "Previous suggestion"),
        "eave::SlashAccept" => ("Slash palette", "Accept"),
        "eave::SlashDismiss" => ("Slash palette", "Dismiss"),
        _ => return None,
    })
}

/// The fallback group for an action `describe` doesn't know, from its
/// namespace.
fn fallback_group(action: &str) -> &'static str {
    match action.split("::").next() {
        Some("text_field") => "Text editing",
        _ => "Other",
    }
}

/// `SlashDismiss` → "Slash dismiss": the fallback label for an unknown
/// action, so a chord bound after this file was written still reads as
/// words rather than vanishing or shouting CamelCase.
fn decamel(name: &str) -> String {
    let bare = name.rsplit("::").next().unwrap_or(name);
    let mut out = String::with_capacity(bare.len() + 4);
    for (i, ch) in bare.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// One keystroke as macOS glyphs: ⌃⌥⇧⌘ then the key.
pub(crate) fn chord_glyphs(modifiers: &gpui::Modifiers, key: &str) -> String {
    let mut out = String::new();
    if modifiers.control {
        out.push('⌃');
    }
    if modifiers.alt {
        out.push('⌥');
    }
    if modifiers.shift {
        out.push('⇧');
    }
    if modifiers.platform {
        out.push('⌘');
    }
    out.push_str(match key {
        "enter" => "↩",
        "escape" => "⎋",
        "backspace" => "⌫",
        "delete" => "⌦",
        "tab" => "⇥",
        "space" => "Space",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "home" => "↖",
        "end" => "↘",
        "pageup" => "⇞",
        "pagedown" => "⇟",
        k if k.chars().count() == 1 => return out + &k.to_uppercase(),
        k => k,
    });
    out
}

/// Every chord that exists, grouped: the LIVE keymap first — so this pane
/// reads what is actually bound, and cannot drift from `bind_keys` — then
/// the few `on_key` chords the keymap cannot see ([`SUPPLEMENT`]).
pub(crate) fn shortcut_groups(cx: &App, query: &str) -> Vec<ShortcutGroup> {
    use std::collections::HashMap;

    // (group, label) → chords, insertion-ordered within a group.
    let mut rows: HashMap<&'static str, Vec<ShortcutRow>> = HashMap::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let mut tab_digits: Vec<String> = Vec::new();

    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    for binding in keymap.bindings() {
        let action = binding.action().name();
        let chord = binding
            .keystrokes()
            .iter()
            .map(|ks| chord_glyphs(ks.modifiers(), ks.key()))
            .collect::<Vec<_>>()
            .join(" ");
        if !seen.insert((action.to_string(), chord.clone())) {
            continue;
        }
        // ⌘1–⌘8 are eight actions and one idea; collapsed to one row
        // below rather than eight rows of the same sentence.
        if action.starts_with("eave::ActivateTab") {
            tab_digits.push(chord);
            continue;
        }
        let (group, label) = match describe(action) {
            Some((g, l)) => (g, l.to_string()),
            None => (fallback_group(action), decamel(action)),
        };
        let group_rows = rows.entry(group).or_default();
        match group_rows.iter_mut().find(|r| r.label == label) {
            Some(r) => r.chords.push(chord),
            None => group_rows.push(ShortcutRow {
                label,
                chords: vec![chord],
            }),
        }
    }
    if !tab_digits.is_empty() {
        let row = ShortcutRow {
            label: format!("Switch to tab 1–{}", tab_digits.len()),
            chords: vec![format!(
                "{} – {}",
                tab_digits.first().cloned().unwrap_or_default(),
                tab_digits.last().cloned().unwrap_or_default()
            )],
        };
        rows.entry("Tabs").or_default().insert(0, row);
    }
    for (group, label, chord) in SUPPLEMENT {
        rows.entry(group).or_default().push(ShortcutRow {
            label: label.to_string(),
            chords: vec![chord.to_string()],
        });
    }

    let needle = query.trim().to_lowercase();
    GROUP_ORDER
        .iter()
        .filter_map(|name| {
            let mut group_rows = rows.remove(name)?;
            if !needle.is_empty() {
                group_rows.retain(|r| {
                    r.label.to_lowercase().contains(&needle)
                        || r.chords.iter().any(|c| c.to_lowercase().contains(&needle))
                        || name.to_lowercase().contains(&needle)
                });
            }
            (!group_rows.is_empty()).then_some(ShortcutGroup {
                name,
                rows: group_rows,
            })
        })
        .collect()
}

impl SettingsWindow {
    fn shortcuts_pane(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let query = self.query.read(cx).content().to_string();
        let groups = shortcut_groups(cx, &query);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        out.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icons::icon(icons::MAGNIFER, 14.0, secondary().into()))
                .child(
                    div()
                        .flex_1()
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .bg(gpui::rgba(0xffffff0f))
                        .border_1()
                        .border_color(hairline())
                        .child(self.query.clone()),
                )
                .into_any_element(),
        );
        if groups.is_empty() {
            out.push(caption(theme, "No shortcut matches."));
            return out;
        }
        for group in groups {
            out.push(section_heading(theme, group.name).into_any_element());
            out.push(
                card(
                    theme,
                    group
                        .rows
                        .into_iter()
                        .map(|r| {
                            row(
                                theme,
                                r.label,
                                // System font for the chords, as the
                                // reference shows them — mono was the
                                // "app UI" tell here.
                                div()
                                    .text_size(px(13.0))
                                    .text_color(secondary())
                                    .child(SharedString::from(r.chords.join(" · ")))
                                    .into_any_element(),
                            )
                        })
                        .collect(),
                )
                .into_any_element(),
            );
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Info — facts only. No update checker: there is no update channel.
// ---------------------------------------------------------------------------

fn info_pane(theme: &Theme) -> Vec<gpui::AnyElement> {
    let mut rows = vec![
        info_row(theme, "Version", env!("CARGO_PKG_VERSION").to_string()),
        info_row(
            theme,
            "Build",
            match option_env!("EAVE_BUILD_SHA") {
                // Stamped by bundle-app.sh into the Mach-O, so it survives
                // this machine's restamp workflow. It names HEAD at build
                // time — `git status` hangs on this checkout, so whether
                // the tree was dirty is deliberately not claimed.
                Some(sha) => format!("{sha} (HEAD at build)"),
                None => "unstamped — a plain cargo build".to_string(),
            },
        ),
        info_row(
            theme,
            "Platform",
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        info_row(
            theme,
            "gpui fork",
            match option_env!("EAVE_GPUI_REV") {
                Some(rev) => format!("mikestangdevs/zed @ {}", &rev[..rev.len().min(9)]),
                None => "unresolved from Cargo.lock".to_string(),
            },
        ),
    ];
    rows.extend(cef_rows(theme));
    vec![
        section_heading(theme, "eave").into_any_element(),
        card(theme, rows).into_any_element(),
    ]
}

/// CEF and Chromium, in the builds that contain them. In a build without
/// `--features web` there is no browser in the binary, so there are no
/// rows — an Info pane naming a Chromium that is not there would be the
/// exact lie this window bans.
#[cfg(feature = "web")]
fn cef_rows(theme: &Theme) -> Vec<gpui::AnyElement> {
    // The sys constants are the CEF headers this binary was compiled
    // against; scripts/check-cef-pin.sh holds the on-disk framework to the
    // same pin, which is what makes "built against" also "running".
    vec![
        info_row(
            theme,
            "CEF",
            format!(
                "{}.{}.{}",
                cef::sys::CEF_VERSION_MAJOR,
                cef::sys::CEF_VERSION_MINOR,
                cef::sys::CEF_VERSION_PATCH
            ),
        ),
        info_row(
            theme,
            "Chromium",
            format!(
                "{}.{}.{}.{}",
                cef::sys::CHROME_VERSION_MAJOR,
                cef::sys::CHROME_VERSION_MINOR,
                cef::sys::CHROME_VERSION_BUILD,
                cef::sys::CHROME_VERSION_PATCH
            ),
        ),
    ]
}

#[cfg(not(feature = "web"))]
fn cef_rows(_theme: &Theme) -> Vec<gpui::AnyElement> {
    Vec::new()
}

fn info_row(theme: &Theme, label: &'static str, value: String) -> gpui::AnyElement {
    row(
        theme,
        label,
        div()
            .text_size(px(13.0))
            .text_color(secondary())
            .child(SharedString::from(value))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The glyph former: modifier order is ⌃⌥⇧⌘ (macOS's), named keys
    /// become their symbols, letters uppercase.
    #[test]
    fn chords_read_as_macos_glyphs() {
        let m = |control, alt, shift, platform| gpui::Modifiers {
            control,
            alt,
            shift,
            platform,
            function: false,
        };
        assert_eq!(chord_glyphs(&m(false, false, false, true), "t"), "⌘T");
        assert_eq!(chord_glyphs(&m(false, false, true, true), "t"), "⇧⌘T");
        assert_eq!(chord_glyphs(&m(true, false, false, false), "tab"), "⌃⇥");
        assert_eq!(chord_glyphs(&m(false, true, false, true), "i"), "⌥⌘I");
        assert_eq!(chord_glyphs(&m(false, false, false, true), ","), "⌘,");
        assert_eq!(chord_glyphs(&m(false, false, false, false), "escape"), "⎋");
    }

    /// Unknown actions appear as readable words in a namespace-derived
    /// group — the fallback that keeps tomorrow's chord from vanishing.
    #[test]
    fn unknown_actions_fall_back_readably() {
        assert_eq!(decamel("eave::SomeFutureAct"), "Some future act");
        assert_eq!(fallback_group("text_field::MoveLeft"), "Text editing");
        assert_eq!(fallback_group("someplace::Thing"), "Other");
    }

    /// The keymap is the source: bind two chords, and the viewer shows
    /// exactly those (plus the documented on-key supplement), grouped and
    /// searchable. What is asserted NOT to happen matters as much: a
    /// query that misses everything yields no groups, not empty shells.
    #[gpui::test]
    fn the_viewer_reads_the_live_keymap(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-t", crate::ui::NewTab, None),
                gpui::KeyBinding::new("cmd-n", crate::ui::NewWindow, None),
                gpui::KeyBinding::new("cmd-,", OpenSettings, None),
            ]);
            let groups = shortcut_groups(cx, "");
            let tabs = groups.iter().find(|g| g.name == "Tabs").expect("Tabs");
            assert!(tabs.rows.iter().any(|r| r.label == "New tab"
                && r.chords == vec!["⌘T".to_string()]));
            let app = groups
                .iter()
                .find(|g| g.name == "Application")
                .expect("Application");
            assert!(app
                .rows
                .iter()
                .any(|r| r.label == "Settings" && r.chords == vec!["⌘,".to_string()]));
            // Slice W1. ⌘N reads as words rather than as the decamelled
            // "New window" the fallback would give it — which is the
            // difference between a chord this pane KNOWS and one it
            // merely failed to hide.
            assert!(
                app.rows
                    .iter()
                    .any(|r| r.label == "New window" && r.chords == vec!["⌘N".to_string()]),
                "the shortcuts pane lost ⌘N: {:?}",
                app.rows.iter().map(|r| &r.label).collect::<Vec<_>>()
            );

            // Search narrows to matching rows and drops empty groups.
            let hits = shortcut_groups(cx, "new tab");
            assert!(hits.iter().all(|g| !g.rows.is_empty()));
            assert!(hits.iter().any(|g| g.name == "Tabs"));
            let none = shortcut_groups(cx, "zzzz-no-such-chord");
            assert!(none.is_empty());
        });
    }

    /// Nothing in the pane's fixed copy mentions rebinding — read-only v1
    /// is silent about what it does not do, per the directive.
    #[test]
    fn the_viewer_says_nothing_about_rebinding() {
        for (_, label, _) in SUPPLEMENT {
            assert!(!label.to_lowercase().contains("rebind"));
        }
        for name in GROUP_ORDER {
            assert!(!name.to_lowercase().contains("rebind"));
        }
    }
}
