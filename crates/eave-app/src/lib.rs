//! eave-app library surface: the pure display pipeline (markdown blocks,
//! display tree, fixture replay) is exposed here so the Stage D acceptance
//! runs headless against the captured real-agent fixture; `ui` is the GPUI
//! shell consuming it.

/// The fork's `EdgeFade` scope, wrapped as an element (Slice M). Private:
/// it is a render-path detail of `ui`, not part of the headless surface
/// the acceptance runs use.
mod edge_fade;

/// The frame drive (Slice N1): the shared pulse clock, the motion catalog
/// and the render tail. Private for the same reason `edge_fade` is — it is
/// render-path machinery, not part of the headless surface the acceptance
/// runs use.
mod motion;

/// The loaders (Slice N3): the working indicator and the skeletons — the
/// first consumer of `motion`. Private for the same reason: it renders,
/// and the headless acceptance runs do not.
mod loaders;

/// The icon pipeline (Slice P3) and the icon family (Slice Q1): the
/// embedded glyph set, the `AssetSource` that serves it, and the three
/// drawing classes that keep a bought family, five in-house glyphs and six
/// provider marks reading as one set. Private like the other render-path
/// modules — the headless acceptance runs never draw.
mod icons;

/// The launcher (Slice Q2): eave's rest state — the provider strip, the
/// table of agents eave knows how to name, and the gate-backed answer to
/// which of them can actually be started. Private for the same reason as
/// its neighbours: it renders.
mod launcher;

/// The overlay layer (Slice P1): the anchored popover surface, the
/// `Popup` state machine and the menu-row primitive. Private for the same
/// reason as `loaders` and `motion` — it renders, and it has no product
/// consumer yet at all.
mod overlay;

/// The `/` palette's discovery half (Slice AR): which of the operator's
/// Claude Code skills and slash commands exist, and what each one does.
/// **Public, unlike its render-path neighbours** — it is pure logic over
/// paths handed in, with no gpui in it, so it is exactly the kind of thing
/// the headless runs can exercise.
pub mod slash;

/// The macOS menu bar (Slice Y). Private: it is wiring, not part of the
/// headless surface the acceptance runs use.
mod app_menus;
pub mod display;
pub mod fixture;
pub mod markdown;
pub mod theme;

pub mod ui;

/// Slice C1: the page engine (Chromium/CEF, off-screen). Behind
/// `--features web`; a set of honest no-ops without it.
pub mod web;
