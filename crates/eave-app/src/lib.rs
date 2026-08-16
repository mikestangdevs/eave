//! eave-app library surface: the pure display pipeline (markdown blocks,
//! display tree, fixture replay) is exposed here so the Stage D acceptance
//! runs headless against the captured real-agent fixture; `ui` is the GPUI
//! shell consuming it.

pub mod display;
pub mod fixture;
pub mod markdown;
pub mod theme;
pub mod ui;
