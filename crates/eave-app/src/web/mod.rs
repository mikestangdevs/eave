//! The page engine: Chromium/CEF rendering off-screen into gpui's scene.
//!
//! # Why off-screen and not a native view
//! Settled by `docs/research/cef-spike.md`, and the finding reversed the
//! going-in assumption. A native `WKWebView`/CEF view is a sibling of gpui's
//! Metal layer, so chrome can only cross it by punching a hole — and a hole
//! is an ABSENCE of gpui pixels, not a copy of the page's, so `BackdropBlur`
//! could never see the page. Frosted glass over a web page was impossible by
//! construction.
//!
//! Off-screen rendering removes the problem rather than solving it. CEF hands
//! over a finished frame; we copy it into a `CVPixelBuffer` we own and paint
//! it with `Window::paint_surface`, which is an ordinary scene primitive. It
//! sorts with quads and text, it is clipped by the content mask, and anything
//! drawn after it — `BackdropBlur` included — sees its pixels. The whole
//! distance between gpui's video-shaped surface primitive and a browser frame
//! was 91 lines of fork (`8521fc1`).
//!
//! # Where the seams are
//! **CEF lives in the app process, not the engine.** The engine is headless
//! and must stay that way; it owns the tab *model* and nothing about pixels.
//! So this module is a client of the engine like any other: it reads
//! [`eave_wire::TabInfo::pending`] to learn where to go, and reports what
//! happened back through `tab_stated` frames. Nothing here writes tab state
//! directly, and nothing in the engine knows Chromium exists.
//!
//! # Feature gate
//! Everything real is behind `--features web`. With the feature off this
//! module is a set of no-ops with the same signatures, so the shell has one
//! code path and the test suite never needs a 323 MB framework. The no-ops
//! are honest ones — [`available`] returns false and [`Page::frame`] yields
//! nothing — rather than panics, because "eave built without a browser" is a
//! real configuration and not an error.

#[cfg(feature = "web")]
mod host;
#[cfg(feature = "web")]
mod input;
#[cfg(feature = "web")]
mod osr;
#[cfg(feature = "web")]
mod page;
#[cfg(feature = "web")]
mod tabs;

#[cfg(feature = "web")]
pub use host::{initialize, install_nsapp, set_repaint_handle, shutdown};
#[cfg(feature = "web")]
pub use page::page_element;
#[cfg(feature = "web")]
pub use tabs::{drive, install_fact_sink, web_memory, Fact};

// ---------------------------------------------------------------------------
// The feature-off half
// ---------------------------------------------------------------------------

/// Whether this build can render pages at all.
///
/// A build fact, not a runtime one: with the feature off there is no
/// Chromium in the binary and no amount of configuration adds one. The
/// shell asks so it can render a tab honestly rather than showing an
/// address bar over a void.
pub const fn available() -> bool {
    cfg!(feature = "web")
}

/// What a page reported, on its way to a `tab_stated` frame.
#[cfg(not(feature = "web"))]
pub type Fact = (String, eave_wire::PageFact);

/// Install an `NSApp` that satisfies both gpui and Chromium.
///
/// **Must run before gpui asks for `NSApp`**, which is why it is separate
/// from [`initialize`] — see `host.rs` for the ordering and why it cannot be
/// folded in.
#[cfg(not(feature = "web"))]
pub fn install_nsapp() {}

/// Start CEF. Returns whether a page engine is now running.
#[cfg(not(feature = "web"))]
pub fn initialize() -> bool {
    false
}

/// Stop CEF.
#[cfg(not(feature = "web"))]
pub fn shutdown() {}

/// Hand the page engine a way to wake the window when a frame arrives.
#[cfg(not(feature = "web"))]
pub fn set_repaint_handle(_cx: gpui::AsyncApp) {}

/// Reconcile the browser against the tab strip: open what is missing, and
/// send whatever tab is asking to navigate where it asked to go.
#[cfg(not(feature = "web"))]
pub fn drive(_snapshot: &eave_wire::TabsSnapshot) {}

/// Give the page engine somewhere to report. Nothing reports here.
#[cfg(not(feature = "web"))]
pub fn install_fact_sink(_tx: tokio::sync::mpsc::UnboundedSender<Fact>) {}

/// Resident bytes across the CEF process tree, and how many processes that
/// is. `None` when nothing is running or the probe failed — the same
/// absence-is-stated rule the agent memory line already keeps.
#[cfg(not(feature = "web"))]
pub fn web_memory() -> Option<(usize, u64)> {
    None
}

/// The page, as an element. `None` when this tab has no live browser —
/// which is every tab in a build without the feature.
#[cfg(not(feature = "web"))]
pub fn page_element(_tab_id: &str, _cx: &mut gpui::App) -> Option<gpui::AnyElement> {
    None
}
