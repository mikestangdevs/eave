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

pub mod display;
pub mod fixture;
pub mod markdown;
pub mod theme;
pub mod ui;
