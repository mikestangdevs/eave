//! Settings — one struct, one file, every pane writes through it (Slice S1).
//!
//! # The shape
//! `<data_dir>/settings.json` deserialises into [`Settings`], whole-struct
//! `#[serde(default)]` so a file written by any older build parses forever —
//! a missing field is the default, never an error. The last field is a
//! `#[serde(flatten)]` map, and it is load-bearing rather than decorative:
//! several checkouts share one data dir (the rule `frame.json` already
//! obeys, `ui.rs` §frame prefs), so keys THIS build does not know must
//! survive a rewrite by it. A typed struct without the flatten would
//! silently strip a newer build's settings on every save.
//!
//! # Two doors, one truth
//! The current settings live in a process-wide mutex, mirrored into a gpui
//! global ([`SettingsStore`]) for anything that renders:
//!
//! - **With a `cx`** (panes, menus): read `cx.global::<SettingsStore>()`,
//!   write through [`update`] — the only writer. It persists, applies the
//!   live side effects, and notifies `observe_global` watchers.
//! - **Without a `cx`** (the omnibox's pure resolution ladder, CEF
//!   callbacks in `web/tabs.rs`): read [`snapshot`] or one of the typed
//!   accessors. These are reads only; nothing without a `cx` may write,
//!   because a write that cannot notify windows is a write the interface
//!   silently disagrees with.
//!
//! # What is deliberately NOT in the struct
//! The rail width. It already persists to `frame.json` (Slice D3) with a
//! merged read-modify-write that other checkouts' builds also perform;
//! moving it here would fork one value into two files that disagree after
//! the first foreign write. The settings window edits it through the same
//! `save_rail_w` path the drag uses, and [`SettingsStore::rail_w`] carries
//! the LIVE value so the pane and the frame stay in step — see `ui.rs`.
//!
//! # Live-apply
//! No restart rows. Each field's route to taking effect immediately:
//! - `appearance` — resolves to the one theme eave has (dark); the field
//!   persists the operator's intent for the day a light theme exists.
//! - `search` — `omnibox::search_prefix()` re-reads per call already
//!   (deliberately uncached); it now falls back to this store instead of
//!   straight to the Google constant.
//! - `evict_after_min` — mirrored into
//!   `eave_core::thresholds::set_page_evict_override`; the eviction clock
//!   (`web/evict.rs`) rebuilds its `Clock` from thresholds every tick, so
//!   the next tick sees the new value. `None` is the off position:
//!   mirrored as `u64::MAX` ms, an idle age no tab reaches.
//! - `switch_to_background_tabs` — read at each decision site
//!   (`tab_disposition`, the open-in-new-tab menu rows).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

/// The operator's appearance intent. eave is dark-only today —
/// `theme.rs` deliberately never ported `Appearance::Light`, and
/// `bezel_theme.rs:141` pins bezel to `Dark` — so both variants resolve to
/// the same paint. The variant that is missing is the point: a `Light`
/// option that painted nothing different would be a row that lies, so it
/// does not exist until the theme does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceSetting {
    /// Follow macOS — which, with one theme, is dark either way. Kept as
    /// the default so that when a light theme lands, operators who never
    /// opened Settings get the system behaviour without a migration.
    #[default]
    System,
    /// Dark, explicitly.
    Dark,
}

/// Where a search leaves the omnibox. The custom variant carries a
/// **prefix** in `EAVE_SEARCH_URL`'s format — the percent-encoded query is
/// appended — because that env var is the contract this setting grew out
/// of, and one format means one encoder ([`crate::omnibox::search`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchEngine {
    #[default]
    Google,
    DuckDuckGo,
    Custom {
        prefix: String,
    },
}

/// DuckDuckGo's query prefix. The plain endpoint, not `html.duckduckgo.com`
/// — the omnibox docs mention both; the JS one is what a browser sends an
/// interactive user to.
pub const DUCKDUCKGO_SEARCH: &str = "https://duckduckgo.com/?q=";

impl SearchEngine {
    /// The prefix this engine sends queries to. A custom prefix that is
    /// empty after trimming falls back to the default engine rather than
    /// producing `""` + query — same guard `search_prefix` applies to the
    /// env var, for the same reason: a blank is a mistake, not a choice.
    pub fn prefix(&self) -> String {
        match self {
            SearchEngine::Google => crate::omnibox::DEFAULT_SEARCH.to_string(),
            SearchEngine::DuckDuckGo => DUCKDUCKGO_SEARCH.to_string(),
            SearchEngine::Custom { prefix } => {
                let p = prefix.trim();
                if p.is_empty() {
                    crate::omnibox::DEFAULT_SEARCH.to_string()
                } else {
                    p.to_string()
                }
            }
        }
    }
}

/// The eviction default, in minutes, derived from the constant it
/// surfaces rather than written down twice —
/// [`eave_core::thresholds::PAGE_IDLE_EVICT_AFTER`] is the decision
/// (20 minutes, and its doc comment owns the reasoning); this is that
/// decision showing through.
fn default_evict_min() -> Option<u32> {
    Some((eave_core::thresholds::PAGE_IDLE_EVICT_AFTER.as_secs() / 60) as u32)
}

/// Everything `settings.json` holds. One struct; every pane reads and
/// writes through it. See the module docs for the two access doors and
/// the flatten field's job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: AppearanceSetting,
    pub search: SearchEngine,
    /// Minutes a page tab may sit un-looked-at before its browser is
    /// given back (Slice E3, surfaced). `None` is off: every visited
    /// tab's browser stays resident for the run.
    #[serde(default = "default_evict_min")]
    pub evict_after_min: Option<u32>,
    /// When a link opens a background tab (⌘-click, middle-click,
    /// `target="_blank"`, the open-in-new-tab menu rows): `false` keeps
    /// you where you are — the gesture's meaning, and the default —
    /// `true` switches to the new tab immediately.
    pub switch_to_background_tabs: bool,
    /// Defaults for NEW sessions — per-provider model and extra flags
    /// (Slice S2). The Agents pane owns the type and is its only writer;
    /// see `settings_agents.rs`. Skipped when empty so a file that never
    /// set a default is byte-identical to a pre-S2 one.
    #[serde(skip_serializing_if = "crate::settings_agents::AgentDefaults::is_empty")]
    pub agents: crate::settings_agents::AgentDefaults,
    /// Keys this build does not know, preserved across a rewrite.
    /// **Keep this field last**; typed fields added later go above it.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: AppearanceSetting::default(),
            search: SearchEngine::default(),
            evict_after_min: default_evict_min(),
            switch_to_background_tabs: false,
            agents: Default::default(),
            extra: serde_json::Map::new(),
        }
    }
}

static PATH: OnceLock<PathBuf> = OnceLock::new();

fn store() -> MutexGuard<'static, Settings> {
    static STORE: OnceLock<Mutex<Settings>> = OnceLock::new();
    STORE
        .get_or_init(|| Mutex::new(Settings::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Where settings are remembered across launches. Called from `ui::run`
/// before the window opens, like `theme::set_brand_path`; the harness
/// never sets it, so tests read defaults and writes go nowhere.
pub fn set_settings_path(path: PathBuf) {
    let _ = PATH.set(path);
    let Some(path) = PATH.get() else { return };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return; // first launch — defaults
    };
    match serde_json::from_str::<Settings>(&raw) {
        Ok(s) => *store() = s,
        // A corrupt preference file may not stop the window opening —
        // the same rule every loader in `ui.rs` follows. Stated on
        // stderr rather than swallowed.
        Err(e) => eprintln!(
            "eave: {} is not valid settings JSON ({e}); using defaults",
            path.display()
        ),
    }
}

/// The current settings, cloned. The no-`cx` read door.
pub fn snapshot() -> Settings {
    store().clone()
}

/// Serialise tests that MUTATE the process-global settings store —
/// `cargo test` runs a crate's tests in parallel threads of one process,
/// so two tests writing settings race and the loser asserts against the
/// other's state. Same rule and shape as `capabilities::probe_test_lock`
/// (which is lock-ordered FIRST in any test taking both). **Test-only.**
#[doc(hidden)]
pub fn settings_test_lock() -> MutexGuard<'static, ()> {
    static TEST_TURN: Mutex<()> = Mutex::new(());
    TEST_TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// [`Settings::switch_to_background_tabs`], for the decision sites in
/// `web/tabs.rs` and the open-in-new-tab menu rows.
pub fn switch_to_background_tabs() -> bool {
    store().switch_to_background_tabs
}

/// The gpui mirror: what panes read and `observe_global` watches.
///
/// `rail_w` rides along even though it is not a [`Settings`] field —
/// `frame.json` owns its persistence (see the module docs) — because the
/// settings pane and the rail's own drag must see one live value.
pub struct SettingsStore {
    pub settings: Settings,
    pub rail_w: f32,
}

impl gpui::Global for SettingsStore {}

/// Install the global and apply the on-boot side effects. Called from
/// `bootstrap` so the harness boots the same wiring production does.
pub fn init(cx: &mut gpui::App) {
    let settings = snapshot();
    apply_side_effects(&settings);
    cx.set_global(SettingsStore {
        settings,
        rail_w: crate::ui::load_rail_w(),
    });
}

/// Change settings. The only writer: mutates, persists, applies the live
/// side effects, and repaints every window. A closure that ends up
/// changing nothing writes nothing.
pub fn update(cx: &mut gpui::App, f: impl FnOnce(&mut Settings)) {
    let mut next = snapshot();
    f(&mut next);
    if next == *store() {
        return;
    }
    *store() = next.clone();
    persist(&next);
    apply_side_effects(&next);
    cx.global_mut::<SettingsStore>().settings = next;
    cx.refresh_windows();
}

/// Move the live rail width (from the settings pane or the rail's own
/// drag mouse-up) and persist it where D3 already keeps it. Clamping is
/// `ui::clamp_rail_w`, the same function the drag runs per move.
pub fn set_rail_w(cx: &mut gpui::App, w: f32) {
    let w = crate::ui::clamp_rail_w(w);
    let g = cx.global_mut::<SettingsStore>();
    if (g.rail_w - w).abs() <= f32::EPSILON {
        return;
    }
    g.rail_w = w;
    crate::ui::save_rail_w(w);
    cx.refresh_windows();
}

/// The settings that act outside gpui, pushed to where they act.
/// `evict_after_min` becomes a thresholds override because the eviction
/// clock lives on the CEF pump and reads thresholds, not this module.
fn apply_side_effects(s: &Settings) {
    eave_core::thresholds::set_page_evict_override(Some(match s.evict_after_min {
        Some(min) => u64::from(min).saturating_mul(60_000),
        // Off: an idle age no tab ever reaches. Stated as MAX rather than
        // as a disable flag so the policy function needs no second code
        // path — `now - last_active < MAX` is simply always true.
        None => u64::MAX,
    }));
}

/// Write the whole file. Temp-then-rename like `Meta::save`
/// (`eave-core/src/store.rs`): settings.json has one writer, so unlike
/// `frame.json` there is no foreign key to merge around — the flatten
/// field carries unknown keys instead.
fn persist(s: &Settings) {
    let Some(path) = PATH.get() else { return };
    let text = match serde_json::to_string_pretty(s) {
        Ok(t) => t + "\n",
        Err(e) => {
            eprintln!("eave: cannot encode settings: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        eprintln!("eave: cannot save settings to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty object — the file every pre-S1 build would offer if it had
    /// written one — parses to exactly the defaults. This is the
    /// parse-forever contract.
    #[test]
    fn an_empty_file_is_the_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
    }

    /// A file naming only one field keeps the others at default —
    /// serde-defaulted per field, not per file.
    #[test]
    fn a_partial_file_fills_in() {
        let s: Settings = serde_json::from_str(r#"{"search":"duck_duck_go"}"#).unwrap();
        assert_eq!(s.search, SearchEngine::DuckDuckGo);
        assert_eq!(s.evict_after_min, default_evict_min());
        assert_eq!(s.appearance, AppearanceSetting::System);
    }

    /// Keys this build does not know survive a decode→encode round trip.
    /// The failure this prevents: a newer build's settings silently
    /// stripped every time an older build saves.
    #[test]
    fn unknown_keys_survive_a_rewrite() {
        // The fixture key was "agents" until Slice S2 made that a typed
        // field; a fixture must name a key that will never become one.
        let raw = r#"{"appearance":"dark","zz_unknown_future":{"default_model":"opus"}}"#;
        let s: Settings = serde_json::from_str(raw).unwrap();
        let out = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["zz_unknown_future"]["default_model"], "opus");
        assert_eq!(v["appearance"], "dark");
    }

    /// `null` is the off position and is not the same as absent: absent
    /// means "never chosen" (the 20-minute default), null means "chosen
    /// off". Both must round-trip.
    #[test]
    fn evict_off_is_null_and_absent_is_default() {
        let off: Settings = serde_json::from_str(r#"{"evict_after_min":null}"#).unwrap();
        assert_eq!(off.evict_after_min, None);
        let out = serde_json::to_string(&off).unwrap();
        let again: Settings = serde_json::from_str(&out).unwrap();
        assert_eq!(again.evict_after_min, None);

        let absent: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.evict_after_min, Some(20));
    }

    /// The default is derived from E3's constant, not written down twice.
    #[test]
    fn the_evict_default_is_e3s_constant() {
        assert_eq!(
            u64::from(default_evict_min().unwrap()) * 60,
            eave_core::thresholds::PAGE_IDLE_EVICT_AFTER.as_secs()
        );
    }

    /// A custom prefix that is blank after trimming is a mistake, not a
    /// choice — it falls back to the default engine instead of sending
    /// queries to `""`.
    #[test]
    fn a_blank_custom_prefix_falls_back() {
        let e = SearchEngine::Custom {
            prefix: "   ".into(),
        };
        assert_eq!(e.prefix(), crate::omnibox::DEFAULT_SEARCH);
        let e = SearchEngine::Custom {
            prefix: "https://kagi.com/search?q=".into(),
        };
        assert_eq!(e.prefix(), "https://kagi.com/search?q=");
    }
}
