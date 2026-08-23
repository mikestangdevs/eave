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

/// What a navigation is allowed to do, decided before it starts (B7, E5).
///
/// **Ungated on purpose** — every function in it is a pure function of a
/// URL string, and `cargo test` runs with the feature OFF. Same bargain as
/// the shared half further down this file. The gated caller is four lines
/// in `tabs.rs`'s `on_before_browse`.
pub mod policy;

/// What a person has already answered, per origin (G1).
///
/// **Ungated for the same reason as [`policy`]** — it is a lock around a
/// map and a pure function of it, and the suite runs with the feature off.
/// The gated caller is a handful of lines in `dialogs.rs`.
pub mod permissions;

/// `com.apple.quarantine` on a finished download (G4).
///
/// **Ungated**, and it is the one module here that is ungated for a
/// stronger reason than testability: nothing in it is Chromium's. It is
/// two `libc` calls and a string format, so the suite can assert against a
/// real file on a real disk rather than against a mock of the platform.
pub mod quarantine;

#[cfg(feature = "web")]
mod dialogs;
#[cfg(feature = "web")]
mod find;
#[cfg(feature = "web")]
mod host;
#[cfg(feature = "web")]
mod input;
#[cfg(feature = "web")]
mod menu;
#[cfg(feature = "web")]
mod osr;
#[cfg(feature = "web")]
mod page;
#[cfg(feature = "web")]
mod tabs;
#[cfg(feature = "web")]
mod zoom;

#[cfg(feature = "web")]
#[cfg(feature = "web")]
pub use dialogs::{answer, any_pending, pending, Answer, Ask, Pending};
#[cfg(feature = "web")]
pub use host::{initialize, install_nsapp, request_quit, shutdown, start_pump};
#[cfg(feature = "web")]
pub use page::page_element;
#[cfg(feature = "web")]
pub use tabs::{
    chrome, copy_selection, downloads, drive, install_fact_sink, install_request_sink, web_memory,
    Download, Fact, PageChrome, PageRequest,
};

// ---------------------------------------------------------------------------
// What the two halves share (Slice E1)
// ---------------------------------------------------------------------------
//
// These types and this arithmetic are in the ungated half **so that they are
// compiled and tested by a build without Chromium in it**. `cargo test`
// runs the suite with the feature off — that is the whole reason the feature
// exists — so anything behind the gate is code the suite never sees. A zoom
// ladder and a find tally are pure functions of numbers and strings; putting
// them where only a 343 MB download can reach them would mean shipping them
// unasserted.

/// What the find bar states about one tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Find {
    /// What was asked for. Empty means the bar is open and nothing has been
    /// typed, which is a different state from closed.
    pub query: String,
    /// CEF's `active_match_ordinal`, 1-based. Zero when there is no active
    /// match.
    pub active: i32,
    /// CEF's total.
    pub count: i32,
    /// Whether the last result for this query carried `final_update`. False
    /// means the count is still rising.
    pub settled: bool,
}

impl Find {
    /// The fact, as the bar says it — or `None` while there is nothing
    /// honest to say.
    ///
    /// Three states, deliberately not two. An empty query has no answer; an
    /// **unsettled** zero is *not yet* rather than *none*, because CEF
    /// reports a long page progressively and only the last result carries
    /// `final_update`; a settled zero is the real absence and is worth
    /// stating plainly. A bar that renders the middle case as "no matches"
    /// flashes a wrong answer on every keystroke over a long document.
    pub fn tally(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        if self.count > 0 {
            return Some(format!("{} of {}", self.active.max(1), self.count));
        }
        if self.settled {
            return Some("no matches".to_string());
        }
        None
    }
}

/// What Chromium says was under the pointer when the right button went
/// down, in the terms eave draws a menu from.
///
/// Everything here is a fact about a DOM this process cannot see, which is
/// why it is carried across rather than re-derived. `x`/`y` have already
/// been converted to **window** space by the handler that built this.
#[derive(Debug, Clone, PartialEq)]
pub struct PageMenu {
    pub tab_id: String,
    pub x: f32,
    pub y: f32,
    /// The link under the pointer, if the pointer was over one.
    pub link_url: Option<String>,
    /// The selected text, if any. Carried whole; the row that shows it
    /// truncates for display and searches for all of it.
    pub selection: Option<String>,
    /// Whether the click landed in an editable field.
    pub editable: bool,
}

/// The zoom ladder, in percent. Chrome's, and `100` must be on it.
///
/// CEF takes a zoom *level*, not a percentage, related by
/// `percent = 100 * 1.2^level` — Blink's own constant. So ⌘+ is a step
/// along a ladder that has been the same in every Chromium browser for
/// fifteen years, and inventing a different one would make eave's 110% a
/// different size from every other browser's 110% on the same page.
pub const LADDER: [u32; 15] = [
    25, 33, 50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300,
];

/// Where `100` sits on [`LADDER`], found rather than written twice.
pub fn rest_index() -> usize {
    LADDER.iter().position(|p| *p == 100).unwrap_or(0)
}

/// CEF's zoom level for a percentage.
pub fn level_for(percent: u32) -> f64 {
    (percent as f64 / 100.0).ln() / 1.2f64.ln()
}

/// Step `delta` rungs from `current`. **Clamped at both ends**, so holding
/// ⌘+ stops at the top rather than wrapping round to 25%.
pub fn step_index(current: usize, delta: i32) -> usize {
    (current as i32 + delta).clamp(0, LADDER.len() as i32 - 1) as usize
}

// ---------------------------------------------------------------------------
// The state, and why it is on this side of the gate
// ---------------------------------------------------------------------------
//
// **State here; calls into Chromium behind the gate.** Which tab's find bar
// is up, what was typed, what the count came back as, what rung the zoom is
// on, whether a menu has been asked for — none of that is a Chromium fact.
// It is eave's bookkeeping about a page, and keeping it here buys two
// things that are worth more than the tidiness of one module per feature:
//
//   1. the shell reads and writes it in **both** feature states, so
//      `ui.rs` has no `#[cfg]` in it and the render pass has one shape; and
//   2. the suite — which runs with the feature OFF — can drive the whole
//      surface end to end. Without this the find bar, the zoom fact and
//      every row of the page menu would be verifiable only by screenshot,
//      and a screenshot is one configuration on one machine on one day.
//
// What is genuinely Chromium's stays behind the gate and is reached through
// the four shims at the bottom of this section: ask CEF to search, tell it
// to stop, set a zoom level, copy a selection. In a build without the
// feature those are the no-ops they honestly are.

thread_local! {
    /// Per tab, because find is per page.
    static FINDS: std::cell::RefCell<std::collections::HashMap<String, Find>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// Which tabs have the bar up. Separate from [`FINDS`] because "open
    /// with nothing typed" and "closed" are different states, and one
    /// `Option<Find>` cannot hold both without an empty-string convention
    /// that something will eventually read wrong.
    static FIND_OPEN: std::cell::RefCell<std::collections::HashMap<String, bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// Index into [`LADDER`], per tab. **Absent means "never zoomed"**,
    /// which renders the same as 100% but is not the same state: the pane
    /// header states zoom only when it is not the default, and that is the
    /// difference between a fact and a decoration.
    static ZOOMS: std::cell::RefCell<std::collections::HashMap<String, usize>> =
        std::cell::RefCell::new(std::collections::HashMap::new());

    /// The menu Chromium has asked for and the shell has not yet taken.
    ///
    /// One slot, not a queue. A second right-click before the first menu is
    /// drawn should open the second menu, not both — and a queue here would
    /// mean a stale menu opening at a stale point one frame later.
    static MENU: std::cell::RefCell<Option<PageMenu>> = const {
        std::cell::RefCell::new(None)
    };
}

pub fn find_is_open(tab_id: &str) -> bool {
    FIND_OPEN.with(|o| o.borrow().get(tab_id).copied().unwrap_or(false))
}

pub fn find_state(tab_id: &str) -> Find {
    FINDS.with(|f| f.borrow().get(tab_id).cloned().unwrap_or_default())
}

/// Open the bar. **A no-op on a tab that already has it up**, so ⌘F on an
/// open bar does not throw the query away — which is what every browser on
/// this platform does, and since this shell has one focus handle and the
/// bar is already the thing consuming keys, "leave it alone" *is*
/// re-focusing.
pub fn find_open(tab_id: &str) {
    FIND_OPEN.with(|o| {
        o.borrow_mut().insert(tab_id.to_string(), true);
    });
}

/// Close the bar and tell CEF to stop looking.
///
/// `clear_selection` is true on escape: dismissing a find leaves the page
/// where it scrolled to but takes the highlight away, which is what makes
/// escape read as an undo of the *search* rather than of the *scroll*.
pub fn find_close(tab_id: &str, clear_selection: bool) {
    FIND_OPEN.with(|o| {
        o.borrow_mut().insert(tab_id.to_string(), false);
    });
    FINDS.with(|f| {
        f.borrow_mut().remove(tab_id);
    });
    cef_stop_find(tab_id, clear_selection);
}

/// Ask for a query, or step through the one already asked for.
///
/// **`find_next` is the whole difference between typing and cycling.** With
/// it false CEF restarts the search from the top and re-counts; with it
/// true it advances from the active match without re-counting. Sending
/// `false` on every ⏎ is how a find bar ends up permanently saying
/// "1 of 17".
pub fn find_search(tab_id: &str, query: &str, forward: bool, find_next: bool) {
    if query.is_empty() {
        // Nothing to look for. Stop rather than search for the empty
        // string, which CEF answers by clearing the count in a way that
        // reads as "no matches" for text nobody asked about.
        FINDS.with(|f| {
            f.borrow_mut().insert(tab_id.to_string(), Find::default());
        });
        cef_stop_find(tab_id, true);
        return;
    }
    FINDS.with(|f| {
        let mut f = f.borrow_mut();
        let entry = f.entry(tab_id.to_string()).or_default();
        if entry.query != query {
            // **A new query invalidates the old count outright.** Keeping
            // it for the length of one round trip is how a bar shows the
            // previous word's tally underneath the current word's text.
            *entry = Find {
                query: query.to_string(),
                ..Default::default()
            };
        }
    });
    cef_find(tab_id, query, forward, find_next);
}

/// A result came back from Chromium. Called by the gated find handler.
// **Dead in a build without the browser, and that is the honest state.**
// These four are the gated half's way back into the shared state: the find
// handler reports a count, `on_after_created` re-asserts a zoom, the
// context-menu handler publishes a request, and `on_before_close` forgets a
// page. With the feature off there is no handler to call any of them.
// Named rather than blanket-allowed (N1's rule), and per-function rather
// than on the module, so a genuinely unused fifth one still shows up.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn note_find_result(tab_id: &str, count: i32, active: i32, settled: bool) {
    FINDS.with(|f| {
        let mut f = f.borrow_mut();
        let entry = f.entry(tab_id.to_string()).or_default();
        entry.count = count;
        entry.active = active;
        entry.settled = settled;
    });
}

/// The zoom this tab is at, in percent — or `None` when it has never been
/// zoomed.
///
/// `None` rather than `Some(100)` on purpose. A tab zoomed to 150 and put
/// back by ⌘0 is at the default again; [`zoom_reset`] clears the entry
/// rather than storing 100, so "never touched" and "put back" are one
/// state and read as one.
pub fn zoom_percent(tab_id: &str) -> Option<u32> {
    ZOOMS.with(|z| z.borrow().get(tab_id).map(|ix| LADDER[*ix]))
}

pub fn zoom_step(tab_id: &str, delta: i32) {
    let current = ZOOMS
        .with(|z| z.borrow().get(tab_id).copied())
        .unwrap_or_else(rest_index);
    let next = step_index(current, delta);
    if next == rest_index() {
        zoom_reset(tab_id);
        return;
    }
    ZOOMS.with(|z| {
        z.borrow_mut().insert(tab_id.to_string(), next);
    });
    cef_set_zoom(tab_id, LADDER[next]);
}

/// ⌘0. Back to the default, and back to saying nothing about it.
pub fn zoom_reset(tab_id: &str) {
    ZOOMS.with(|z| {
        z.borrow_mut().remove(tab_id);
    });
    cef_set_zoom(tab_id, 100);
}

/// Re-assert this tab's zoom on a browser that has just been created.
///
/// **Zoom belongs to the tab, not to the browser.** A tab whose browser was
/// closed and re-made must come back at the size it was left at, or the
/// zoom silently belonged to the browser all along.
///
/// That was a hypothetical when it was written and is not one now: Slice E3
/// evicts idle page browsers, so **a browser's lifetime is shorter than its
/// tab's and a restored tab is a brand-new browser at the same url**. Keyed
/// by tab id, re-applied here.
///
/// ⚠️ **Open, and it belongs to E3's side of the seam:** [`forget_page`]
/// drops the zoom, and it runs from `on_before_close` — which is reached by
/// a tab being CLOSED and by a browser being EVICTED alike. An evicted tab
/// arguably keeps its zoom; a closed one certainly does not. Only the
/// eviction path knows which it is.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn zoom_reapply(tab_id: &str) {
    if let Some(p) = zoom_percent(tab_id) {
        cef_set_zoom(tab_id, p);
    }
}

/// Take the menu request, if there is one. Called from the shell's render
/// pass; leaves the slot empty.
pub fn take_menu_request() -> Option<PageMenu> {
    MENU.with(|m| m.borrow_mut().take())
}

/// Chromium asked for a menu. Called by the gated context-menu handler —
/// and by the harness, which is the point of it living here.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn request_menu(menu: PageMenu) {
    MENU.with(|m| {
        let replaced = m.borrow_mut().replace(menu);
        drop(replaced);
    });
}

/// A page went away. Everything eave remembers about it goes with it.
///
/// A find that outlived its page would state a count about a document that
/// is gone, and a zoom held for a departed tab is a leak that shows up as a
/// surprise the next time the id is reused.
///
/// ⚠️ **This is right for a CLOSE and wrong for an EVICTION, and on this
/// branch it cannot tell them apart.** It runs from `on_before_close`,
/// which Slice E3's eviction also reaches — and an evicted tab still
/// exists, so its zoom and its query are still state about a tab the
/// operator has not closed. E3 publishes
/// `web::page_was_evicted(tab_id)`, true during `on_before_close` for an
/// eviction and false for a real close, precisely so this can branch. When
/// the two branches meet, the first line here becomes an early return on
/// that predicate.
///
/// One judgement to make at the same time, and it is not E3's to make: the
/// **open bit** should not survive an eviction even if the query does.
/// Restoring it would put a find bar back over a page that is mid-reload
/// with nothing found yet, stating "no matches" about a document that has
/// not arrived. Keep the query, drop the bit.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn forget_page(tab_id: &str) {
    FINDS.with(|f| {
        f.borrow_mut().remove(tab_id);
    });
    FIND_OPEN.with(|o| {
        o.borrow_mut().remove(tab_id);
    });
    ZOOMS.with(|z| {
        z.borrow_mut().remove(tab_id);
    });
    MENU.with(|m| {
        let mut m = m.borrow_mut();
        if m.as_ref().is_some_and(|r| r.tab_id == tab_id) {
            *m = None;
        }
    });
}

// ── The four shims: everything above this line is eave's, everything
// below it is Chromium's ────────────────────────────────────────────────

#[cfg(feature = "web")]
fn cef_find(tab_id: &str, query: &str, forward: bool, find_next: bool) {
    find::drive(tab_id, query, forward, find_next);
}
#[cfg(not(feature = "web"))]
fn cef_find(_tab_id: &str, _query: &str, _forward: bool, _find_next: bool) {}

#[cfg(feature = "web")]
fn cef_stop_find(tab_id: &str, clear_selection: bool) {
    find::stop(tab_id, clear_selection);
}
#[cfg(not(feature = "web"))]
fn cef_stop_find(_tab_id: &str, _clear_selection: bool) {}

#[cfg(feature = "web")]
fn cef_set_zoom(tab_id: &str, percent: u32) {
    zoom::apply(tab_id, percent);
}
#[cfg(not(feature = "web"))]
fn cef_set_zoom(_tab_id: &str, _percent: u32) {}

// ---------------------------------------------------------------------------
// The feature-off half
// ---------------------------------------------------------------------------

/// Whether this build can render pages at all.
///
/// A build fact, not a runtime one: with the feature off there is no
/// Chromium in the binary and no amount of configuration adds one. The
/// shell asks so it can render a tab honestly rather than showing an
/// address bar over a void.
pub fn available() -> bool {
    if cfg!(feature = "web") {
        return true;
    }
    PRETEND.with(|p| p.get())
}

thread_local! {
    /// Slice E1's test override for [`available`].
    ///
    /// Thread-local and not a `static`, for the reason `overlay`'s demo
    /// gate gives: `#[gpui::test]` runs each test on its own thread inside
    /// one process, so a process-wide flag would let one test's override
    /// decide another test's render.
    ///
    /// **Why an override is needed at all.** Everything Slice E1 added is
    /// gated on this — ⌘F, ⌘+, the page menu — because a find bar over a
    /// build with no Chromium in it would be a surface that can never
    /// count. But the suite runs with the feature OFF, so without a way to
    /// say "pretend", the whole slice's shell behaviour would be
    /// unassertable and would ship on a screenshot alone. The state these
    /// surfaces read is real in both builds (see below); only the calls
    /// INTO Chromium are gated, and those are what the screenshots are
    /// for.
    static PRETEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub fn pretend_available_for_test() {
    PRETEND.with(|p| p.set(true));
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
    Navigate {
        tab_id: String,
        url: String,
    },
    External {
        url: String,
    },
    RememberPermission {
        origin: String,
        kinds: Vec<permissions::PermissionKind>,
        decision: permissions::PermissionDecision,
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
        kinds: Vec<permissions::PermissionKind>,
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

#[cfg(not(feature = "web"))]
pub fn copy_selection(_tab_id: &str) {}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder has to contain the rest point, or ⌘0 lands on 25%.
    #[test]
    fn the_zoom_ladder_has_a_hundred_on_it() {
        assert!(LADDER.contains(&100), "{LADDER:?}");
        assert_eq!(LADDER[rest_index()], 100);
    }

    /// Increasing, or `step(+1)` sometimes shrinks the page.
    #[test]
    fn the_zoom_ladder_only_goes_up() {
        for pair in LADDER.windows(2) {
            assert!(pair[0] < pair[1], "{LADDER:?} is not sorted at {pair:?}");
        }
    }

    /// 100% must be level 0 **exactly**. A non-zero level at rest is a page
    /// very slightly the wrong size on every tab nobody has touched.
    #[test]
    fn a_hundred_percent_is_zoom_level_zero() {
        assert!(level_for(100).abs() < 1e-12, "{}", level_for(100));
    }

    /// Every rung round-trips through Blink's constant to within a percent,
    /// which is what makes eave's 150% the same size as every other
    /// Chromium browser's 150% on the same page.
    #[test]
    fn every_zoom_rung_round_trips_through_blinks_constant() {
        for p in LADDER {
            let back = 100.0 * 1.2f64.powf(level_for(p));
            assert!(
                (back - p as f64).abs() < 1.0,
                "{p}% -> level {} -> {back}%",
                level_for(p)
            );
        }
    }

    /// Both ends stop rather than wrap. Holding ⌘- must not arrive back at
    /// 300%.
    #[test]
    fn the_zoom_ladder_clamps_at_both_ends() {
        assert_eq!(step_index(0, -1), 0);
        assert_eq!(step_index(0, -99), 0);
        let top = LADDER.len() - 1;
        assert_eq!(step_index(top, 1), top);
        assert_eq!(step_index(top, 99), top);
    }

    /// An empty query has no answer, and saying "no matches" for text
    /// nobody typed is stating a fact about nothing.
    #[test]
    fn an_empty_find_query_states_nothing() {
        assert_eq!(Find::default().tally(), None);
    }

    /// **The middle state is the one worth having.** A zero count that CEF
    /// has not finished counting is *not yet*, not *none* — and rendering
    /// it as "no matches" flashes a wrong answer on every keystroke over a
    /// long page.
    #[test]
    fn an_unsettled_zero_says_nothing_and_a_settled_zero_says_none() {
        let searching = Find {
            query: "eave".into(),
            active: 0,
            count: 0,
            settled: false,
        };
        assert_eq!(searching.tally(), None);
        let done = Find {
            settled: true,
            ..searching
        };
        assert_eq!(done.tally().as_deref(), Some("no matches"));
    }

    /// The number is CEF's, transcribed. `3 of 17`, not a calculation.
    #[test]
    fn a_find_tally_is_the_ordinal_and_the_total() {
        let f = Find {
            query: "eave".into(),
            active: 3,
            count: 17,
            settled: true,
        };
        assert_eq!(f.tally().as_deref(), Some("3 of 17"));
    }

    /// CEF can report a positive count before it has an active match. The
    /// bar must not say "0 of 17", which reads as a broken cursor.
    #[test]
    fn a_find_tally_never_says_zero_of_anything() {
        let f = Find {
            query: "eave".into(),
            active: 0,
            count: 17,
            settled: false,
        };
        assert_eq!(f.tally().as_deref(), Some("1 of 17"));
    }
}
