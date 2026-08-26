//! Slice S2 gate: **defaults round-trip through the real settings.json.**
//!
//! An integration test binary on purpose — `settings::set_settings_path`
//! is a once-per-process latch (the in-crate harness deliberately never
//! sets it so unit tests read defaults and write nowhere), so the only
//! place a real file round trip can run without leaking a path into
//! every other test is a process of its own.

use gpui::TestAppContext;

use eave_app::settings::{self, Settings};
use eave_app::settings_agents::{self, AgentDefaults};

/// Set a default model and default flags the way the pane does, then
/// read the FILE back — not the in-process store — and extract the
/// defaults through the same decoder the pane uses. What is asserted is
/// the whole gate: the bytes S1's temp+rename persist wrote parse back
/// to exactly what was set, and clearing everything returns the file to
/// carrying no `agents` key at all.
#[gpui::test]
fn defaults_round_trip_through_the_settings_file(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("eave-s2-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("settings.json");
    let _ = std::fs::remove_file(&path);
    settings::set_settings_path(path.clone());

    cx.update(|cx| {
        settings::init(cx);
        settings_agents::write_defaults(cx, |d| {
            d.set_model("claude-code", Some("sonnet".into()));
            d.set_extra_args("claude-code", vec!["--verbose".into()]);
        });
    });

    let raw = std::fs::read_to_string(&path).expect("settings.json was written");
    let reread: Settings = serde_json::from_str(&raw).expect("the file parses");
    let defaults = settings_agents::defaults_in(&reread);
    assert_eq!(defaults.model_for("claude-code"), Some("sonnet"));
    assert_eq!(defaults.extra_args_for("claude-code"), ["--verbose"]);

    // And the other settings survived the write untouched — the pane
    // writes THROUGH Settings, not over it.
    assert_eq!(reread.switch_to_background_tabs, false);

    // Taking both defaults back removes the key from the file entirely.
    cx.update(|cx| {
        settings_agents::write_defaults(cx, |d| {
            d.set_model("claude-code", None);
            d.set_extra_args("claude-code", vec![]);
        });
    });
    let raw = std::fs::read_to_string(&path).expect("settings.json still exists");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parses");
    assert!(
        v.get(settings_agents::SETTINGS_KEY).is_none(),
        "cleared defaults must leave no residue: {raw}"
    );
    let reread: Settings = serde_json::from_str(&raw).expect("parses as Settings");
    assert_eq!(settings_agents::defaults_in(&reread), AgentDefaults::default());

    let _ = std::fs::remove_dir_all(&dir);
}
