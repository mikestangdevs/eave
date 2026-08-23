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
//! # The one rule this whole module is written to
//! **Under an external message pump, CEF runs its handlers SYNCHRONOUSLY on
//! your stack.** Everything below follows from that single fact, and it has
//! now been found twice, from two directions, as two different symptoms:
//!
//! - **Every borrow you hold is lent to Chromium mid-call.** Slice C7:
//!   `with_browser` held `BROWSERS.borrow()` across a `close_browser` whose
//!   `on_before_close` takes `borrow_mut()` — `already borrowed`, unwinding
//!   through Objective-C frames, abort. 48 runs of 48.
//! - **Every continuation you hold is lent to Chromium too.** A CEF callback
//!   that hands you a `Callback` object has STOPPED the page until you answer
//!   it, so parking one across your own bookkeeping is a hung renderer rather
//!   than a crash. Hence: answer before you file, remove before you reply,
//!   and answer on the way out of `on_before_close` rather than dropping.
//!
//! Stated together because the second was found by a different session, on a
//! different symptom, after the first had been written down as though it were
//! only about `RefCell`s. It never was. The general form is: **do not hold
//! anything of your own — a borrow, a lock, a continuation — across a call
//! into a foreign event loop that will call you back on this stack.**
//!
//! # Diagnostics
//! `EAVE_WEB_TRACE=1` narrates the page engine's lifecycle on stderr: every
//! reconciliation, every browser created, the load callbacks, the pump, and
//! the first frames. It is here because three of this slice's four real
//! defects were invisible without it — a pump that stopped after two calls,
//! a navigation dropped into a browser that did not exist yet, and an app
//! object whose drop took CEF's only callback with it. None of them
//! produced an error; all of them produced a blank page.
//!
//! # Feature gate
//! Everything real is behind `--features web`. With the feature off this
//! module is a set of no-ops with the same signatures, so the shell has one
//! code path and the test suite never needs a 323 MB framework. The no-ops
//! are honest ones — [`available`] returns false and [`Page::frame`] yields
//! nothing — rather than panics, because "eave built without a browser" is a
//! real configuration and not an error.

#[cfg(feature = "web")]
mod evict;
#[cfg(feature = "web")]
mod host;
#[cfg(feature = "web")]
mod input;
#[cfg(feature = "web")]
mod osr;
#[cfg(feature = "web")]
mod page;
/// The eviction DECISION, compiled in both feature states so the default
/// test run exercises it. See its module docs.
///
/// With the feature OFF nothing calls it — `evict` is the only consumer and
/// `evict` is gated — so its methods are dead in that build and only its
/// tests run. The allow is scoped to exactly that state rather than blanket,
/// so genuinely dead code in the `web` build is still caught.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod policy;
#[cfg(feature = "web")]
mod tabs;

#[cfg(feature = "web")]
pub use host::{initialize, install_nsapp, request_quit, shutdown, start_pump};
#[cfg(feature = "web")]
pub use page::page_element;
#[cfg(feature = "web")]
pub use tabs::{drive, install_fact_sink, web_memory, Fact};

/// Whether this tab's page is in memory right now — i.e. whether there is a
/// live browser behind it.
///
/// **The hook Slice E3 leaves for the strip and the rail.** An evicted tab
/// and a never-visited tab are the same thing to the person looking at the
/// strip — a tab whose page is not loaded and will load when they click it
/// — so this is the broad fact, and it is the one worth drawing. Dim the
/// favicon on `!page_is_resident`.
#[cfg(feature = "web")]
pub fn page_is_resident(tab_id: &str) -> bool {
    tabs::has_browser(tab_id)
}

/// Whether eave took this tab's browser back to reclaim memory (Slice E3).
///
/// The narrow fact, kept separate because the two are genuinely different
/// events even where they look alike: this one is eave acting on the
/// operator's memory rather than the operator never having opened the tab.
#[cfg(feature = "web")]
pub fn page_was_evicted(tab_id: &str) -> bool {
    evict::was_evicted(tab_id)
}

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
pub fn initialize(_data_dir: &std::path::Path) -> bool {
    false
}

/// Stop CEF.
#[cfg(not(feature = "web"))]
pub fn shutdown() {}

/// Ask eave to quit the orderly way. No page engine here, so the caller
/// should quit normally — which is what `false` says.
#[cfg(not(feature = "web"))]
pub fn request_quit() -> bool {
    false
}

/// Start pumping CEF on gpui's executor. Nothing to pump here.
#[cfg(not(feature = "web"))]
pub fn start_pump(_cx: &mut gpui::App) {}

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

/// Whether this tab's page is in memory. Never, in a build with no
/// Chromium in it — which is honest rather than convenient: the strip's
/// "not loaded" treatment is then true of every page tab, because it is.
#[cfg(not(feature = "web"))]
pub fn page_is_resident(_tab_id: &str) -> bool {
    false
}

/// Whether eave evicted this tab's browser. Nothing to evict here.
#[cfg(not(feature = "web"))]
pub fn page_was_evicted(_tab_id: &str) -> bool {
    false
}
