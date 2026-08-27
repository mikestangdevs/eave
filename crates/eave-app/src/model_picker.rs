//! MonoCode's model picker, as data.
//!
//! The popup itself lives in `ui.rs` (it is a projection of launcher +
//! session state). This file is the part that can be named without a
//! `Shell`: tabs, search, favorites, and the catalog rows those produce.
//!
//! eave still passes `--model` verbatim (P2 caveat 5, Slice CK). A row's
//! **title** is the name a person reads; [`Entry::model`] is the flag.

use std::path::Path;

use crate::launcher::{self, PROVIDERS};

/// MonoCode `MENU_WIDTH`.
pub const WIDTH: f32 = 300.0;
/// MonoCode popup height (`Math.min(340, …)`).
pub const HEIGHT: f32 = 340.0;
/// MonoCode selected-tab underline (`h-0.5` → 2 px).
pub const TAB_UNDERLINE: f32 = 2.0;
/// ⌘1…⌘9 pick the first this many visible rows.
pub const SHORTCUTS: usize = 9;

const STORE_NAME: &str = "model-picker.json";

/// Favorites first, then one tab per named provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Favorites,
    Provider(usize),
}

impl Default for Tab {
    fn default() -> Self {
        Self::Favorites
    }
}

impl Tab {
    pub fn key(self) -> String {
        match self {
            Self::Favorites => "favorites".into(),
            Self::Provider(ix) => PROVIDERS
                .get(ix)
                .map(|p| p.id.to_string())
                .unwrap_or_else(|| "favorites".into()),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        if raw == "favorites" {
            return Self::Favorites;
        }
        PROVIDERS
            .iter()
            .position(|p| p.id == raw)
            .map(Self::Provider)
            .unwrap_or(Self::Favorites)
    }
}

/// One row the picker can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub provider_ix: usize,
    pub model: &'static str,
}

impl Entry {
    pub fn id(self) -> String {
        let p = &PROVIDERS[self.provider_ix];
        format!("{}:{}", p.id, self.model)
    }

    pub fn provider(self) -> &'static launcher::Provider {
        &PROVIDERS[self.provider_ix]
    }

    pub fn enabled(self) -> bool {
        launcher::eave_can_drive(self.provider())
    }

    /// What the row's title says. A person-facing name; [`Self::model`] is
    /// still the `--model` value.
    pub fn title(self) -> String {
        display_name(self.provider().id, self.model)
    }

    /// `Claude Code · claude-code` — MonoCode's `HARNESS_TITLE · HARNESS_LABEL`.
    pub fn subtitle(self) -> String {
        let p = self.provider();
        format!("{} · {}", p.name, p.id)
    }
}

/// Person-facing name for a `--model` value.
///
/// Known ids are named the way MonoCode's catalogs name them (native id
/// in, display name out). Anything the CLI reports that is not in the
/// table is title-cased rather than shown as a slug.
pub fn display_name(provider_id: &str, native: &str) -> String {
    if native.is_empty() {
        return String::new();
    }
    if let Some(name) = named(provider_id, native) {
        return name.to_string();
    }
    let (base, bracket) = split_effort(native);
    if let Some(name) = named(provider_id, base) {
        return match bracket {
            Some(b) => format!("{name} ({b})"),
            None => name.to_string(),
        };
    }
    prettify(native)
}

/// `(provider, native) → name`. An empty provider matches any tab — used
/// for ids that mean the same thing on every CLI (`grok-4.6`).
///
/// Claude / Cursor names from MonoCode `claudeCatalog` / `MODELS`.
/// Grok names from `grok models` (2026-08-21). Pi from `eave_core::pi::MODELS`.
const NAMES: &[(&str, &str, &str)] = &[
    ("claude-code", "claude-sonnet-5", "Claude Sonnet 5"),
    ("claude-code", "claude-opus-5", "Claude Opus 5"),
    ("claude-code", "claude-fable-5", "Claude Fable 5"),
    ("claude-code", "claude-opus-4-8", "Claude Opus 4.8"),
    ("claude-code", "claude-opus-4-7", "Claude Opus 4.7"),
    ("claude-code", "claude-opus-4-6", "Claude Opus 4.6"),
    ("claude-code", "claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-code", "claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-code", "claude-haiku-4-5", "Claude Haiku 4.5"),
    ("claude-code", "opus", "Opus"),
    ("claude-code", "sonnet", "Sonnet"),
    ("claude-code", "haiku", "Haiku"),
    ("cursor-agent", "composer-2.5", "Composer 2.5"),
    ("cursor-agent", "gpt-5.4", "GPT-5.4"),
    ("cursor-agent", "claude-sonnet-4-6", "Sonnet 4.6"),
    ("cursor-agent", "grok-4.6", "Cursor Grok 4.6"),
    ("codex", "gpt-5.6-luna", "GPT-5.6 Luna"),
    ("codex", "gpt-5.6-terra", "GPT-5.6 Terra"),
    ("codex", "gpt-5.4", "GPT-5.4"),
    ("codex", "gpt-5.3-codex", "GPT-5.3 Codex"),
    ("", "grok-4.6", "Grok 4.6"),
    ("", "grok-4.5", "Grok 4.5"),
    ("", "anthropic/claude-opus-4-5", "Claude Opus 4.5"),
    ("", "anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5"),
    ("", "anthropic/claude-haiku-4-5", "Claude Haiku 4.5"),
    ("", "openai/gpt-5.3-codex", "GPT-5.3 Codex"),
    ("", "google/gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
    ("", "xai/grok-4.6", "Grok 4.6"),
];

fn named(provider_id: &str, native: &str) -> Option<&'static str> {
    let mut generic = None;
    for (p, n, name) in NAMES {
        if *n != native {
            continue;
        }
        if *p == provider_id {
            return Some(*name);
        }
        if p.is_empty() {
            generic = Some(*name);
        }
    }
    generic
}

fn split_effort(native: &str) -> (&str, Option<&str>) {
    match native.split_once('[') {
        Some((base, rest)) => (base, Some(rest.trim_end_matches(']'))),
        None => (native, None),
    }
}

fn prettify(native: &str) -> String {
    let slug = native.rsplit('/').next().unwrap_or(native);
    let (base, bracket) = split_effort(slug);
    if let Some(name) = named("", base) {
        return match bracket {
            Some(b) => format!("{name} ({b})"),
            None => name.to_string(),
        };
    }
    let tokens: Vec<&str> = base.split('-').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if i + 1 < tokens.len()
            && tokens[i].chars().all(|c| c.is_ascii_digit())
            && tokens[i + 1].chars().all(|c| c.is_ascii_digit())
        {
            out.push(format!("{}.{}", tokens[i], tokens[i + 1]));
            i += 2;
            continue;
        }
        out.push(word_case(tokens[i]));
        i += 1;
    }
    let mut s = if tokens.first().copied() == Some("gpt") {
        match out.as_slice() {
            [_, rest @ ..] => format!("GPT-{}", rest.join("-")),
            _ => out.join(" "),
        }
    } else {
        out.join(" ")
    };
    if let Some(b) = bracket {
        s.push_str(" (");
        s.push_str(b);
        s.push(')');
    }
    s
}

fn word_case(w: &str) -> String {
    if w.eq_ignore_ascii_case("gpt") {
        return "GPT".into();
    }
    if w.eq_ignore_ascii_case("glm") {
        return "GLM".into();
    }
    if w.chars().any(|c| c.is_ascii_digit()) {
        return w.to_string();
    }
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Live picker fields that survive a close. Favorites and the last tab
/// also round-trip through [`STORE_NAME`] when a data dir is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub tab: Tab,
    pub query: String,
    pub active: usize,
    pub favorites: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tab: Tab::Favorites,
            query: String::new(),
            active: 0,
            favorites: Vec::new(),
        }
    }
}

impl State {
    pub fn clamp_active(&mut self, n: usize) {
        if n == 0 {
            self.active = 0;
        } else if self.active >= n {
            self.active = n - 1;
        }
    }

    pub fn step_active(&mut self, n: usize, delta: isize) {
        if n == 0 {
            self.active = 0;
            return;
        }
        let cur = self.active as isize;
        self.active = (cur + delta).rem_euclid(n as isize) as usize;
    }

    pub fn toggle_favorite(&mut self, id: String) {
        if let Some(i) = self.favorites.iter().position(|x| x == &id) {
            self.favorites.remove(i);
        } else {
            self.favorites.push(id);
        }
    }

    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.iter().any(|x| x == id)
    }
}

/// Disk shape. Missing file → empty favorites, no remembered tab.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Persist {
    pub favorites: Vec<String>,
    pub tab: Option<Tab>,
}

pub fn store_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(STORE_NAME)
}

pub fn load(path: &Path) -> Persist {
    let Ok(bytes) = std::fs::read(path) else {
        return Persist::default();
    };
    parse_persist(&bytes)
}

pub fn save(path: &Path, persist: &Persist) {
    let body = serde_json::json!({
        "favorites": persist.favorites,
        "tab": persist.tab.map(Tab::key),
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, body.to_string());
}

fn parse_persist(bytes: &[u8]) -> Persist {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Persist::default();
    };
    let favorites = v
        .get("favorites")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let tab = v
        .get("tab")
        .and_then(|x| x.as_str())
        .map(Tab::from_key);
    Persist { favorites, tab }
}

/// Tabs the left/right keys walk: favorites, then every provider eave
/// can drive. Unavailable providers still *draw* in the bar (dimmed);
/// they are not in this list, matching MonoCode's `modelPickerTabs`.
pub fn step_tabs(available: impl Fn(usize) -> bool) -> Vec<Tab> {
    std::iter::once(Tab::Favorites)
        .chain(
            PROVIDERS
                .iter()
                .enumerate()
                .filter(|(ix, _)| available(*ix))
                .map(|(ix, _)| Tab::Provider(ix)),
        )
        .collect()
}

pub fn step_tab(tab: Tab, delta: isize, available: impl Fn(usize) -> bool) -> Tab {
    let tabs = step_tabs(available);
    if tabs.is_empty() {
        return tab;
    }
    let from = tabs.iter().position(|t| *t == tab).unwrap_or(0);
    let n = tabs.len() as isize;
    tabs[(from as isize + delta).rem_euclid(n) as usize]
}

pub fn available_ix(ix: usize) -> bool {
    launcher::eave_can_drive(&PROVIDERS[ix])
}

/// Every model this provider's picker would list — native `--model`
/// values only. Empty when the CLI has none or takes no `--model`.
pub fn entries_for_provider(ix: usize) -> Vec<Entry> {
    let p = &PROVIDERS[ix];
    launcher::model_choices(p)
        .into_iter()
        .filter(|m| !m.is_empty())
        .map(|model| Entry {
            provider_ix: ix,
            model,
        })
        .collect()
}

fn all_entries() -> Vec<Entry> {
    PROVIDERS
        .iter()
        .enumerate()
        .flat_map(|(ix, _)| entries_for_provider(ix))
        .collect()
}

pub fn find(id: &str) -> Option<Entry> {
    all_entries().into_iter().find(|e| e.id() == id)
}

/// Rows for `tab`, then the search needle if any.
pub fn visible(tab: Tab, query: &str, favorites: &[String]) -> Vec<Entry> {
    let pool = match tab {
        Tab::Favorites => favorites.iter().filter_map(|id| find(id)).collect(),
        Tab::Provider(ix) => entries_for_provider(ix),
    };
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return pool;
    }
    pool.into_iter()
        .filter(|e| matches_query(e, &needle))
        .collect()
}

fn matches_query(entry: &Entry, needle: &str) -> bool {
    let p = entry.provider();
    let hay = format!(
        "{} {} {} {}",
        entry.title(),
        entry.model,
        p.name,
        p.id
    )
    .to_lowercase();
    hay.contains(needle)
}

pub fn empty_label(tab: Tab, query: &str) -> &'static str {
    if !query.trim().is_empty() {
        return "No matching models";
    }
    match tab {
        Tab::Favorites => "No favorite models",
        Tab::Provider(ix) if !available_ix(ix) => "This agent is not available",
        Tab::Provider(_) => "No matching models",
    }
}

/// ⌘N for visible index `ix` (0-based), when that row can be picked.
pub fn shortcut(ix: usize, enabled: bool) -> Option<String> {
    if enabled && ix < SHORTCUTS {
        Some(format!("⌘{}", ix + 1))
    } else {
        None
    }
}

/// Open on the remembered tab, else the session/launcher's current
/// provider. First-run empty favorites is a blank list; opening on the
/// agent you are already talking to is the useful empty.
pub fn opening_tab(saved: Option<Tab>, current_provider_ix: usize) -> Tab {
    saved.unwrap_or(Tab::Provider(current_provider_ix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> usize {
        PROVIDERS
            .iter()
            .position(|p| p.id == "claude-code")
            .expect("claude-code is catalogued")
    }

    fn named_provider(id: &str) -> usize {
        PROVIDERS
            .iter()
            .position(|p| p.id == id)
            .unwrap_or_else(|| panic!("{id} is catalogued"))
    }

    #[test]
    fn claude_tab_lists_native_ids() {
        let rows = entries_for_provider(claude());
        let names: Vec<&str> = rows.iter().map(|e| e.model).collect();
        assert_eq!(names[0], "claude-sonnet-5", "{names:?}");
        assert!(!names.iter().any(|n| *n == "CLI default"), "{names:?}");
        assert!(names.contains(&"claude-sonnet-5"), "{names:?}");
        assert!(names.contains(&"claude-opus-5"), "{names:?}");
        assert!(names.contains(&"claude-fable-5"), "{names:?}");
        assert!(!names.contains(&"opus"), "{names:?}");
    }

    #[test]
    fn grok_lists_what_the_cli_named_and_hermes_lists_nothing() {
        let _turn = eave_core::capabilities::probe_test_lock();
        eave_core::capabilities::forget();
        let grok = entries_for_provider(named_provider("grok"));
        let names: Vec<&str> = grok.iter().map(|e| e.model).collect();
        assert_eq!(names, vec!["grok-4.6", "grok-4.5"], "{names:?}");
        assert!(entries_for_provider(named_provider("hermes")).is_empty());
        assert!(entries_for_provider(named_provider("codex")).is_empty());
        assert!(entries_for_provider(named_provider("cursor-agent")).is_empty());
    }

    #[test]
    fn search_opus_hits_pretty_name_and_native_id() {
        let rows = visible(Tab::Provider(claude()), "opus 5", &[]);
        assert!(
            rows.iter().any(|e| e.model == "claude-opus-5"),
            "{rows:?}"
        );
    }

    #[test]
    fn search_matches_provider_name_too() {
        let rows = visible(Tab::Provider(claude()), "claude", &[]);
        assert!(rows.len() >= 2, "{rows:?}");
    }

    #[test]
    fn titles_are_person_facing() {
        let e = Entry {
            provider_ix: claude(),
            model: "claude-sonnet-5",
        };
        assert_eq!(e.title(), "Claude Sonnet 5");
        assert_eq!(
            display_name("claude-code", "opus"),
            "Opus"
        );
        assert_eq!(
            display_name("claude-code", "claude-fable-5[1m]"),
            "Claude Fable 5 (1m)"
        );
        assert_eq!(
            display_name("cursor-agent", "grok-4.6"),
            "Cursor Grok 4.6"
        );
        assert_eq!(display_name("grok", "grok-4.6"), "Grok 4.6");
        assert_eq!(
            display_name("pi", "anthropic/claude-opus-4-5"),
            "Claude Opus 4.5"
        );
    }

    #[test]
    fn favorites_keep_saved_order_and_drop_unknowns() {
        let id = Entry {
            provider_ix: claude(),
            model: "claude-opus-5",
        }
        .id();
        let rows = visible(
            Tab::Favorites,
            "",
            &[id.clone(), "no-such-provider:ghost".into(), id.clone()],
        );
        // find() is by id; the duplicate is two copies of the same entry
        // if the saved list repeats. MonoCode maps 1:1, so a repeat stays.
        assert!(rows.iter().all(|e| e.model == "claude-opus-5"));
        assert!(!rows.is_empty());
        assert!(visible(Tab::Favorites, "", &["ghost:x".into()]).is_empty());
    }

    #[test]
    fn favorite_toggle_adds_then_removes() {
        let mut s = State::default();
        s.toggle_favorite("claude-code:claude-opus-5".into());
        assert!(s.is_favorite("claude-code:claude-opus-5"));
        s.toggle_favorite("claude-code:claude-opus-5".into());
        assert!(!s.is_favorite("claude-code:claude-opus-5"));
    }

    #[test]
    fn tab_key_round_trips_provider_ids() {
        let ix = claude();
        assert_eq!(Tab::from_key("favorites"), Tab::Favorites);
        assert_eq!(Tab::from_key("claude-code"), Tab::Provider(ix));
        assert_eq!(Tab::from_key("not-a-provider"), Tab::Favorites);
        assert_eq!(Tab::Provider(ix).key(), "claude-code");
    }

    #[test]
    fn step_wraps_across_favorites_and_live_providers() {
        let live = |ix: usize| PROVIDERS[ix].id == "claude-code";
        let tabs = step_tabs(live);
        assert_eq!(tabs[0], Tab::Favorites);
        assert_eq!(tabs[1], Tab::Provider(claude()));
        assert_eq!(
            step_tab(Tab::Favorites, 1, live),
            Tab::Provider(claude())
        );
        assert_eq!(step_tab(Tab::Provider(claude()), 1, live), Tab::Favorites);
        assert_eq!(
            step_tab(Tab::Favorites, -1, live),
            Tab::Provider(claude())
        );
    }

    #[test]
    fn opening_tab_prefers_the_saved_one() {
        assert_eq!(
            opening_tab(Some(Tab::Favorites), claude()),
            Tab::Favorites
        );
        assert_eq!(
            opening_tab(None, claude()),
            Tab::Provider(claude())
        );
    }

    #[test]
    fn shortcut_is_cmd_n_for_the_first_nine_live_rows() {
        assert_eq!(shortcut(0, true).as_deref(), Some("⌘1"));
        assert_eq!(shortcut(8, true).as_deref(), Some("⌘9"));
        assert_eq!(shortcut(9, true), None);
        assert_eq!(shortcut(0, false), None);
    }

    #[test]
    fn persist_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "eave-model-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = store_path(&dir);
        save(
            &path,
            &Persist {
                favorites: vec!["claude-code:claude-opus-5".into()],
                tab: Some(Tab::Favorites),
            },
        );
        let loaded = load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.favorites, vec!["claude-code:claude-opus-5"]);
        assert_eq!(loaded.tab, Some(Tab::Favorites));
    }

    #[test]
    fn persist_missing_file_is_empty() {
        let p = load(Path::new("/no/such/eave-model-picker.json"));
        assert!(p.favorites.is_empty());
        assert_eq!(p.tab, None);
    }

    #[test]
    fn entry_id_splits_on_the_first_colon() {
        let e = Entry {
            provider_ix: claude(),
            model: "claude-opus-5",
        };
        assert_eq!(e.id(), "claude-code:claude-opus-5");
        assert_eq!(e.subtitle(), "Claude Code · claude-code");
        assert_eq!(e.title(), "Claude Opus 5");
    }

    #[test]
    fn empty_copy_names_the_cause() {
        assert_eq!(empty_label(Tab::Favorites, ""), "No favorite models");
        assert_eq!(empty_label(Tab::Favorites, "xyz"), "No matching models");
        assert_eq!(
            empty_label(Tab::Provider(claude()), "xyz"),
            "No matching models"
        );
    }
}
