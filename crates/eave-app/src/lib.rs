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

/// Composer attachments: local paths staged onto a send, mentioned as
/// `@path` when the draft goes out. Public for the same reason `slash` is
/// — pure logic over paths, no gpui, so the headless suite can prove the
/// cap, the mention form and the compose rule without a window.
pub mod attach;

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

/// The omnibox's decision half (Slice C5): what one typed line means —
/// address or search — and the rows the dropdown offers. **Public, like
/// `slash` and for the same reason**: it is pure logic with no gpui in it,
/// so the url-versus-search ladder can be exercised without a window. The
/// half that draws lives in `ui`.
pub mod omnibox;

/// The tab strip's model half (Slice C5): B1's `TabsState` flattened into
/// the one line the strip draws, with page tabs and session tabs in it.
/// Public for the same reason `omnibox` is — the ordering, the labels and
/// the group runs are decidable without a window, and that is where the
/// behaviour worth asserting lives.
pub mod strip;

/// The bookmarks bar's model half (Slice T1): B1's flat, depth-first
/// `BookmarksSnapshot` read as a bar of top-level chips and the runs of
/// children each folder opens. Public for the same reason `strip` is.
pub mod bookmarks;

/// The macOS menu bar (Slice Y). Private: it is wiring, not part of the
/// headless surface the acceptance runs use.
mod app_menus;
pub mod display;
pub mod fixture;
pub mod markdown;
pub mod theme;

// Slice BZ: bezel's tokens, built from ours. The only place in the tree
// allowed to write a bezel token — see the module doc for why that matters.
pub mod bezel_theme;
/// bezel's `TextField`, vendored — the editing engine eave never had. See the
/// module doc for the provenance and the enumerated delta.
pub mod text_field;
/// Activity / tool-call / diff presentation ported from bezel's gallery.
mod bezel_activity;
mod bezel_diff;
mod bezel_tools;
/// The theme editor (bezel's Create page). A window, opened from the menu.
pub mod theme_editor;

pub mod ui;

/// Slice C1: the page engine (Chromium/CEF, off-screen). Behind
/// `--features web`; a set of honest no-ops without it.
pub mod web;
