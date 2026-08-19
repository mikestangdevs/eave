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

/// The icon pipeline (Slice P3): the embedded glyph set and the
/// `AssetSource` that serves it. Private like the other render-path
/// modules — the headless acceptance runs never draw.
mod icons;

/// The overlay layer (Slice P1): the anchored popover surface, the
/// `Popup` state machine and the menu-row primitive. Private for the same
/// reason as `loaders` and `motion` — it renders, and it has no product
/// consumer yet at all.
mod overlay;

pub mod display;
pub mod fixture;
pub mod markdown;
pub mod theme;
pub mod ui;
