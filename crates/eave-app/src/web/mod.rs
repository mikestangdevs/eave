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
//! # The one rule this module is written around
//! **Under an external message pump, CEF runs its handlers SYNCHRONOUSLY on
//! your stack.** Everything you are holding when you call into Chromium is
//! therefore lent to Chromium mid-call. That has bitten this project twice,
//! in two disguises that look nothing alike:
//!
//! - **Every BORROW you hold.** `with_browser` clones the handle and ends
//!   the `RefCell` borrow *before* calling in, because `close_browser`
//!   re-enters `on_before_close`, which borrows the same map. Slice C6
//!   measured that as an `already borrowed` abort on ⌘Q with a page open;
//!   C7 fixed it in the helper rather than at a call site, **48 of 48**.
//! - **Every CONTINUATION you hold.** A callback that hands you a
//!   `Callback` object has **stopped the page** until you answer it — a
//!   `CefJsdialogCallback`, a file, permission or download callback. Park
//!   one across your own bookkeeping and you get a hung renderer rather
//!   than a crash; answer one twice and it is a use-after-free inside
//!   Chromium.
//!
//! In practice: **answer before you file, remove before you reply, and
//! answer on the way out of `on_before_close` rather than dropping.**
//!
//! Stated together because the second was found by a different session, on
//! a different symptom, after the first had already been written down as
//! though it were only about `RefCell`s. It never was. An abort and a hang;
//! one rule. Anything added here that takes a CEF callback inherits it.
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
mod dialogs;
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
#[cfg(feature = "web")]
pub use dialogs::{answer, any_pending, pending, Answer, Ask, Pending};
#[cfg(feature = "web")]
pub use host::{initialize, install_nsapp, request_quit, shutdown, start_pump};
#[cfg(feature = "web")]
pub use page::page_element;
#[cfg(feature = "web")]
pub use tabs::{
    chrome, downloads, drive, install_fact_sink, install_request_sink, web_memory, Download, Fact,
    PageChrome, PageRequest,
};

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

/// What a page asked for. Nothing asks here.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone)]
pub enum PageRequest {
    OpenTab {
        url: String,
        opener: String,
        activate: bool,
    },
}

/// Give the page engine somewhere to ask. Nothing asks here.
#[cfg(not(feature = "web"))]
pub fn install_request_sink(_tx: tokio::sync::mpsc::UnboundedSender<PageRequest>) {}

/// What the page last said about itself — progress, status, cursor,
/// fullscreen. All defaults with no page engine, which is honest: a build
/// without Chromium has no page to have a cursor over.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone, Default)]
pub struct PageChrome {
    pub progress: f32,
    pub status: Option<String>,
    pub fullscreen: bool,
    pub cursor: gpui::CursorStyle,
}

/// What the page last said about itself. `None` — there is no page.
#[cfg(not(feature = "web"))]
pub fn chrome(_tab_id: &str) -> Option<PageChrome> {
    None
}

/// A download in flight.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub id: u32,
    pub url: String,
    pub file_name: String,
    pub path: String,
    pub received: i64,
    pub total: i64,
    pub percent: i32,
    pub done: bool,
    pub cancelled: bool,
}

/// Everything downloading, newest first. Empty — nothing can download
/// without a page engine to download it.
#[cfg(not(feature = "web"))]
pub fn downloads() -> Vec<Download> {
    Vec::new()
}

/// What a page is asking a person. Nothing asks here.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    Alert {
        message: String,
    },
    Confirm {
        message: String,
    },
    Prompt {
        message: String,
        default: String,
    },
    Leave {
        message: String,
        reloading: bool,
    },
    Permission {
        kinds: Vec<String>,
    },
    Files {
        title: String,
        multiple: bool,
        folder: bool,
        save: bool,
    },
}

#[cfg(not(feature = "web"))]
impl Ask {
    pub fn is_refusable(&self) -> bool {
        !matches!(self, Ask::Alert { .. })
    }
}

/// An outstanding ask, and which tab is stopped on it.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub tab_id: String,
    pub origin: String,
    pub ask: Ask,
}

/// What a person answered.
#[cfg(not(feature = "web"))]
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Ok,
    Cancel,
    Text(String),
    Files(Vec<String>),
}

/// What this tab is stopped on. `None` — there is no page to stop.
#[cfg(not(feature = "web"))]
pub fn pending(_tab_id: &str) -> Option<Pending> {
    None
}

/// Whether any tab is stopped on an ask.
#[cfg(not(feature = "web"))]
pub fn any_pending() -> bool {
    false
}

/// Answer a tab's outstanding ask. Nothing is outstanding here.
#[cfg(not(feature = "web"))]
pub fn answer(_tab_id: &str, _answer: Answer) {}

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
