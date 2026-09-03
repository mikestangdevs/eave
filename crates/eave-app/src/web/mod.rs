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
//! The general form, which is what to carry to anything new here: **do not
//! hold anything of your own — a borrow, a lock, a continuation — across a
//! call into a foreign event loop that will call you back on this stack.**
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
///
/// # It was called `policy`, and so was something else
/// E3 landed a `web/policy.rs` deciding **eviction** on the same day this
/// one landed deciding **navigation**, on two branches, and they collided
/// as an add/add when the branches met. Both are real, both are ungated
/// for the same testability reason, and neither is "the" policy — so
/// neither keeps the bare name. See [`evict_policy`].
pub mod nav_policy;

/// What a person has already answered, per origin (G1).
///
/// **Ungated for the same reason as [`nav_policy`]** — it is a lock around a
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

/// Turn a dark-on-transparent favicon into a light mark for the rail.
/// Ungated so the decision can be asserted without Chromium.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod favicon_adapt;

/// The ask VOCABULARY and the rule that decides what an answer means,
/// compiled in both feature states so the default test run exercises them.
/// `dialogs` — the CEF half — is gated; this is not. See its module docs
/// for why (06-browser G26).
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod ask;
/// The DevTools protocol eave speaks: what goes on the wire, how a call is
/// matched to its answer, and what an answer means. Compiled in BOTH
/// feature states — `inspector`, the CEF half, is gated; this is not, so
/// the default test run exercises the protocol rather than skipping it.
/// See its module docs (06-browser E9).
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub mod cdp;
/// IME composition state (06-browser A3). Ungated for `cdp`'s reason: what
/// is in it is ranges and rectangles, and the suite that runs by default
/// has to be able to exercise it.
pub mod ime;
/// What the page has selected, as the chrome knows it (06-browser A4).
/// Ungated for `ime`'s reason — it is a map and the rules for updating it.
pub mod selection;
/// Where the page is scrolled, and putting it back after an eviction
/// (06-browser A7, and half of C7). Ungated for `selection`'s reason — it
/// is two maps, a threshold and one generated line of JavaScript.
pub mod scroll;
/// Drag & drop as the chrome and the page share it (06-browser A5).
/// Ungated for `selection`'s reason — it is a map and the rules for
/// updating it. The four `drag_target_*` calls and `start_dragging` are
/// gated; this is not. Feature-off, only [`drag::forget_tab`] is reached
/// from `forget_page`; the rest is for tests and the gated callers.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub mod drag;
#[cfg(feature = "web")]
mod dialogs;
#[cfg(feature = "web")]
mod evict;
/// The eviction DECISION, compiled in both feature states so the default
/// test run exercises it. See its module docs.
///
/// With the feature OFF nothing calls it — `evict` is the only consumer and
/// `evict` is gated — so its methods are dead in that build and only its
/// tests run. The allow is scoped to exactly that state rather than blanket,
/// so genuinely dead code in the `web` build is still caught.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod evict_policy;
#[cfg(feature = "web")]
mod favicon;
#[cfg(feature = "web")]
mod find;
#[cfg(feature = "web")]
mod cdm;
#[cfg(feature = "web")]
mod host;
#[cfg(feature = "web")]
mod input;
#[cfg(feature = "web")]
mod inspector;
#[cfg(feature = "web")]
mod menu;
/// What a page did — console and network — kept where an agent can read
/// it (E8). Compiled in BOTH feature states: it is a fold over CDP events
/// into bounded per-tab stores, and every decision in it is testable
/// without a Chromium. See its module docs.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod observe;
/// `window.Notification`, reimplemented over a CDP binding because CEF
/// alloy has no notification API — the display half of G2. Compiled in
/// BOTH feature states: everything that decides whether a notification is
/// shown, and who it is attributed to, lives here rather than in the
/// injected JavaScript, so the default test run exercises the decisions.
/// See its module docs.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
mod notify;

/// C4's indicator — which tabs are making a noise, learned over CDP.
///
/// Ungated for `notify`'s reason: the parse and the shim source are pure,
/// `cdp_call` has a feature-off twin, so the whole loop runs in the default
/// suite and only the Chromium at the far end is missing.
mod audible;

/// Bookmark peek — hover previews for bookmarked services (Gmail,
/// Google Calendar, Linear), fetched over the CDP channel through a page
/// that already has the operator's session. Ungated for `notify`'s
/// reason: the providers, the fold and the cache are pure, `cdp_call`
/// has a feature-off twin, and only the hidden-browser line is gated.
/// `pub(crate)` unlike its siblings because the renderer matches on
/// [`peek::Status`] — a facade that hid the enum would just re-state it.
pub(crate) mod peek;
#[cfg(feature = "web")]
mod osr;
#[cfg(feature = "web")]
mod page;
#[cfg(feature = "web")]
mod pdf;
#[cfg(feature = "web")]
mod tabs;
#[cfg(feature = "web")]
mod zoom;

pub use ask::{any_pending, certificate_explanation, masked, pending, Answer, Ask, Pending};
#[cfg(feature = "web")]
pub use dialogs::answer;
#[cfg(feature = "web")]
pub use host::{initialize, install_nsapp, request_quit, shutdown, start_pump};
#[cfg(feature = "web")]
pub use pdf::scheme_app;
#[cfg(feature = "web")]
pub use page::{page_element, page_focus_handle, set_keyboard_page, set_live_divider};
#[cfg(feature = "web")]
pub use tabs::{
    chrome, downloads, drive, install_fact_sink, install_request_sink, is_on_screen,
    print_to_pdf, set_shown_pages, web_memory, Download, Fact, PageChrome, PageRequest,
};

/// Clear Chromium's own copy of an origin's permission answers (G1).
///
/// **Not a tidiness call — a correctness one.** For every capability that
/// arrives through `on_show_permission_prompt`, Chromium stores its own
/// durable ALLOW/BLOCK and short-circuits later requests before the
/// embedder is asked. Without this, eave's revoke cleared eave's stores and
/// the site kept the permission. See `dialogs::sync_to_chromium`.
#[cfg(feature = "web")]
pub(crate) fn cef_clear_permissions(origin: &str, kinds: &[permissions::PermissionKind]) {
    dialogs::clear_in_chromium(origin, kinds);
}

/// Nothing to clear: there is no Chromium here.
#[cfg(not(feature = "web"))]
pub(crate) fn cef_clear_permissions(_origin: &str, _kinds: &[permissions::PermissionKind]) {}

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
    /// The click, still in PAGE space — what `x`/`y` were before the
    /// window-space conversion. Inspect needs it: `show_dev_tools`'s
    /// `inspect_element_at` is a question about the page, and the element
    /// under the pointer is found by Blink, not by eave's window.
    pub page_x: i32,
    pub page_y: i32,
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

/// The ladder rung nearest a CEF zoom level — the inverse of [`level_for`].
///
/// # Why it snaps instead of computing (G5)
/// `percent = 100 * 1.2^level` is exact in principle and not in `f64`.
/// Measured on this tree, three rungs do not survive the round trip:
///
/// ```text
///  33% -> 32.99999999999999289   <- `as u32` gives 32
/// 110% -> 110.00000000000001421
/// 175% -> 174.99999999999997158
/// ```
///
/// A truncating conversion therefore reports the 33% rung as **32%**,
/// which is not on [`LADDER`] and which no ⌘+ could step off. Rounding
/// would rescue those three, but only by luck, and nothing checks the luck.
/// So the arithmetic finds the neighbourhood and the LADDER decides the
/// answer — which also means a
/// level Chromium restored from an earlier release, or one a page set with
/// a pinch, is reported as the nearest rung eave can actually step from,
/// rather than as a percentage no ⌘+ can move off.
///
/// Level 0 is exactly 100% and must stay exactly 100%, or a page at rest
/// would draw a zoom badge.
pub fn percent_for(level: f64) -> u32 {
    let exact = 100.0 * 1.2f64.powf(level);
    *LADDER
        .iter()
        .min_by(|a, b| {
            let da = (**a as f64 - exact).abs();
            let db = (**b as f64 - exact).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&100)
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
    ///
    /// **This is the second of two sources of truth, and the other one is
    /// durable.** Chromium stores zoom per *host* and writes it to
    /// `Preferences` on every ⌘+ — measured, see `web/zoom.rs`. This map is
    /// keyed by tab and starts empty each launch, so a fresh tab on a
    /// previously-zoomed origin should render zoomed while this says 100%.
    /// G5 is the reconciliation.
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

/// Put this tab's displayed zoom in step with what Chromium actually has
/// (G5).
///
/// # This replaced `zoom_reapply`, and the direction reversed
/// The old function asserted eave's remembered zoom ONTO a newly created
/// browser, because "zoom belongs to the tab, not to the browser". The
/// probe in `docs/reports/stage-g-probes.md` §2 showed why that was the
/// wrong way round: **`set_zoom_level` reaches `ZoomController` in
/// `ZOOM_MODE_DEFAULT`, whose default branch is
/// `SetZoomLevelForHost` — per host and durable.** Chromium has been
/// storing zoom in `Preferences` on every ⌘+ all along, keyed by host, and
/// restores it by itself on the next navigation. eave's map was the second
/// copy, and the one that was wrong: at launch it is empty, so a page came
/// back zoomed with the header reading nothing.
///
/// So the map is now a **cache of Chromium's answer**, not a store, and
/// this is where it is refreshed. Called after a navigation commits, which
/// is also the only correct moment: zoom is per HOST, so it can change
/// without the tab doing anything except going somewhere else.
///
/// # The behaviour this decides, stated because it is a change
/// Zoom is now **per origin, shared across tabs** — two tabs on the same
/// site are at the same zoom, and a new tab on a site you zoomed last week
/// opens zoomed. That is what Chrome, Edge, Brave and Safari all do, and
/// after this probe it is the only self-consistent option: Chromium writes
/// that store whether or not eave reads it, so the alternative was not
/// "per tab" but "per tab, plus a divergent per-host store on disk".
///
/// The `⚠️` that used to hang on `zoom_reapply` — *does an evicted tab keep
/// its zoom?* — is answered rather than moved: it comes back at its host's
/// zoom, the same as any other tab on that host, and [`forget_page`]
/// dropping the entry is now dropping a cache line rather than a fact.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn zoom_sync(tab_id: &str) {
    let Some(level) = cef_zoom_level(tab_id) else {
        return;
    };
    let percent = percent_for(level);
    ZOOMS.with(|z| {
        let mut z = z.borrow_mut();
        if percent == LADDER[rest_index()] {
            // At rest. Absent rather than `Some(100)`, so "never zoomed"
            // and "put back" stay one state — see [`zoom_percent`].
            z.remove(tab_id);
        } else if let Some(ix) = LADDER.iter().position(|p| *p == percent) {
            z.insert(tab_id.to_string(), ix);
        }
    });
}

/// Take the menu request, if there is one. Called from the shell's render
/// pass; leaves the slot empty.
pub fn take_menu_request() -> Option<PageMenu> {
    MENU.with(|m| m.borrow_mut().take())
}

/// The decoded favicon for a page tab, if Chromium has handed us one.
///
/// The snapshot carries the URL; the pixels live here. A globe on the
/// strip is "we do not have bytes yet", not "this page has no icon".
pub fn favicon_image(tab_id: &str) -> Option<std::sync::Arc<gpui::Image>> {
    #[cfg(test)]
    {
        if let Some(image) = TEST_FAVICONS.with(|m| m.borrow().get(tab_id).cloned()) {
            return Some(image);
        }
    }
    cef_favicon(tab_id)
}

/// The favicon for any url, by host — for surfaces that have an address
/// but no tab. The rail's pinned bookmark grid and the links inside an
/// open folder are the callers. Sized for the 16 logical bookmark mark
/// (32 device on 2×).
pub fn favicon_for_url(url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    #[cfg(test)]
    {
        let host = crate::omnibox::engine_host(url);
        if let Some(image) = TEST_HOST_FAVICONS.with(|m| m.borrow().get(&host).cloned()) {
            return Some(image);
        }
    }
    cef_favicon_for_url(url)
}

/// Host-cache pixels sized for the 16 logical tab plate. Restored tabs
/// have no tab-id entry; they still share the host's 32-device raster.
pub fn favicon_tab_for_url(url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    #[cfg(test)]
    {
        let host = crate::omnibox::engine_host(url);
        if let Some(image) = TEST_HOST_FAVICONS.with(|m| m.borrow().get(&host).cloned()) {
            return Some(image);
        }
    }
    cef_favicon_tab_for_url(url)
}

/// Ask for a bookmark's icon. A no-op without `web`, and idempotent per
/// host, so a render pass may call it on every tile every frame.
pub fn request_favicon(url: &str) {
    #[cfg(feature = "web")]
    favicon::fetch_for_url(url);
    #[cfg(not(feature = "web"))]
    let _ = url;
}

#[cfg(feature = "web")]
fn cef_favicon_for_url(url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    favicon::for_url(url)
}

#[cfg(not(feature = "web"))]
fn cef_favicon_for_url(_url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    None
}

#[cfg(feature = "web")]
fn cef_favicon_tab_for_url(url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    favicon::tab_for_url(url)
}

#[cfg(not(feature = "web"))]
fn cef_favicon_tab_for_url(_url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    None
}

#[cfg(feature = "web")]
fn cef_favicon(tab_id: &str) -> Option<std::sync::Arc<gpui::Image>> {
    favicon::get(tab_id)
}

#[cfg(not(feature = "web"))]
fn cef_favicon(_tab_id: &str) -> Option<std::sync::Arc<gpui::Image>> {
    None
}

#[cfg(test)]
thread_local! {
    static TEST_FAVICONS: std::cell::RefCell<
        std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
thread_local! {
    static TEST_HOST_FAVICONS: std::cell::RefCell<
        std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Test seam: give a HOST bytes, so the rail's grid can be driven without
/// a browser. Keyed the way [`favicon_for_url`] looks up.
#[cfg(test)]
pub fn set_host_favicon_for_test(host: &str, image: Option<std::sync::Arc<gpui::Image>>) {
    TEST_HOST_FAVICONS.with(|m| match image {
        Some(image) => {
            m.borrow_mut().insert(host.to_string(), image);
        }
        None => {
            m.borrow_mut().remove(host);
        }
    });
}

/// Test seam: put bytes on a tab the harness never created a browser for.
#[cfg(test)]
pub fn set_favicon_for_test(tab_id: &str, image: Option<std::sync::Arc<gpui::Image>>) {
    TEST_FAVICONS.with(|m| match image {
        Some(image) => {
            m.borrow_mut().insert(tab_id.to_string(), image);
        }
        None => {
            m.borrow_mut().remove(tab_id);
        }
    });
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
    // Whatever this tab had on screen goes with it, unacked: there is no
    // page left to hear a `close`, and an ack to a dead browser answers
    // `NotSent` on a later pump for nothing. See `notify::forget_tab`.
    notify::forget_tab(tab_id);
    // And its speaker. Both halves matter: a stale `AUDIBLE` entry would
    // put an icon on a reused tab id that is silent, and a stale `INSTALLED`
    // entry would convince the installer the shim was already there — so
    // the reused tab would report nothing, for ever, with no error anywhere.
    // That second failure is `observe::forget_tab`'s exactly.
    audible::forget_tab(tab_id);
    // Its console and network record goes too — including the fact that it
    // was watched, or a reused id would never have its domains enabled
    // again and would report nothing, with no error anywhere.
    observe::forget_tab(tab_id);
    // Its composition too. A reused tab id that inherited a live marked
    // range would have every keystroke swallowed by the IME guard, with
    // nothing in any log to say why.
    ime::forget_tab(tab_id);
    // And what it had selected, or a reused id offers to search for words
    // from a page that is no longer open (A4).
    selection::forget_tab(tab_id);
    // An in-flight out-drag from this tab is the hang path: returning 1
    // from `start_dragging` and never ending it leaves the renderer
    // waiting. The gated half still has to tell Chromium; this forgets
    // the chrome's half so a reused id does not inherit a live drag.
    #[cfg(feature = "web")]
    input::abort_out_for(tab_id);
    drag::forget_tab(tab_id);
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
    // An eviction keeps the pixels: the tab is still on the strip.
    #[cfg(feature = "web")]
    if !page_was_evicted(tab_id) {
        favicon::forget(tab_id);
    }
    // Slice E9. A CDP call whose tab has gone is a caller waiting on an
    // answer that can never arrive, so `forget_tab` answers them `Gone`
    // rather than dropping them. Ungated on purpose: a tab closes in both
    // feature states, and a cleanup only the `web` build ran would be one
    // the default suite could never see fail.
    #[cfg(feature = "web")]
    inspector::forget_registration(tab_id);
    cdp::forget_tab(tab_id);
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

#[cfg(feature = "web")]
fn cef_stop_load(tab_id: &str) {
    tabs::stop_load(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_stop_load(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_reload_hard(tab_id: &str) {
    tabs::reload_hard(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_reload_hard(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_copy_selection(tab_id: &str) {
    tabs::copy_selection(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_copy_selection(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_cut_selection(tab_id: &str) {
    tabs::cut_selection(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_cut_selection(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_paste_into_page(tab_id: &str) {
    tabs::paste_into_page(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_paste_into_page(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_select_all(tab_id: &str) {
    tabs::select_all(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_select_all(_tab_id: &str) {}

#[cfg(feature = "web")]
fn cef_set_muted(tab_id: &str, muted: bool) {
    tabs::set_muted(tab_id, muted);
}
#[cfg(not(feature = "web"))]
fn cef_set_muted(_tab_id: &str, _muted: bool) {}

#[cfg(feature = "web")]
fn cef_is_muted(tab_id: &str) -> bool {
    tabs::is_muted(tab_id)
}
#[cfg(all(not(feature = "web"), not(test)))]
fn cef_is_muted(_tab_id: &str) -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// Mute, as the feature-OFF suite sees it.
    ///
    /// The real answer comes from `is_audio_muted` on a browser that does
    /// not exist in a test process, so without this the speaker control is
    /// unreachable by the harness in either direction — it could never be
    /// shown and never be clicked. That is G26's shape exactly, and the
    /// cost of not having a seam is that the only proof the control works
    /// is a person clicking it.
    static TEST_MUTED: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Pretend a tab is audible, for the harness.
#[cfg(test)]
pub fn set_audible_for_test(tab_id: &str, audible: bool) {
    audible::set_for_test(tab_id, audible);
}

#[cfg(feature = "web")]
fn cef_exit_fullscreen(tab_id: &str) {
    tabs::exit_fullscreen(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_exit_fullscreen(_tab_id: &str) {}

/// D10 — whether this tab's page has asked to be fullscreen.
///
/// Reads the chrome fold rather than Chromium, because Chromium has no
/// getter for it: `on_fullscreen_mode_change` is the only report, so eave's
/// record of the last one IS the state.
pub fn page_fullscreen(tab_id: &str) -> bool {
    chrome(tab_id).is_some_and(|c| c.fullscreen)
}

/// D10 — ask the page to leave fullscreen (`tabs::exit_fullscreen`).
pub fn exit_page_fullscreen(tab_id: &str) {
    cef_exit_fullscreen(tab_id);
}

/// C4 (half) — mute or unmute a tab.
///
/// The matching *indicator* is not available: knowing a page is audible
/// means taking its audio away from the speakers. `tabs::set_muted` carries
/// the account.
pub fn set_tab_muted(tab_id: &str, muted: bool) {
    #[cfg(test)]
    TEST_MUTED.with(|m| {
        let mut m = m.borrow_mut();
        if muted {
            m.insert(tab_id.to_string());
        } else {
            m.remove(tab_id);
        }
    });
    cef_set_muted(tab_id, muted);
}

/// Whether a tab is muted. `false` when there is no browser to ask.
pub fn tab_muted(tab_id: &str) -> bool {
    #[cfg(test)]
    {
        return TEST_MUTED.with(|m| m.borrow().contains(tab_id));
    }
    #[cfg(not(test))]
    cef_is_muted(tab_id)
}

/// B4 — ⎋ during a load: stop where it got to.
///
/// Chromium keeps whatever has already been parsed and painted, which is
/// the point of the key — a stop is not a failure and not a blank page.
pub fn stop_load(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("stop".to_string(), tab_id.to_string())));
    cef_stop_load(tab_id);
}

/// B5 — ⇧⌘R: reload this tab without using the cache.
///
/// The plain ⌘R above this is the engine's reload and stays that way; this
/// one is a browser command about the entry already loaded (see
/// `tabs::reload_hard`).
pub fn reload_hard(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("hard-reload".to_string(), tab_id.to_string())));
    cef_reload_hard(tab_id);
}

/// A4 — ⌘C. Copy the page's selection.
///
/// **Blink does this, not eave.** On macOS Chromium's editing commands
/// arrive through the responder chain (`copy:`), and an OSR host has no
/// responder chain — which is exactly why the chord did nothing before
/// this slice even though the key event reached the page. `Frame::copy`
/// is the substitute for the missing chain.
///
/// The page still receives the keystroke as well, so a site with its own
/// ⌘C handler still runs it. That is Chrome's behaviour too. The one
/// difference eave cannot yet match: a page that calls `preventDefault()`
/// on the keydown suppresses Chrome's built-in copy, and here it does not,
/// because CEF's `send_key_event` reports nothing back about it.
pub fn copy_selection(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("copy".to_string(), tab_id.to_string())));
    cef_copy_selection(tab_id);
}

/// A4 — ⌘X. Cut the page's selection, where the selection is editable.
pub fn cut_selection(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("cut".to_string(), tab_id.to_string())));
    cef_cut_selection(tab_id);
}

/// A4 — ⌘V into a page, which is NOT the composer's ⌘V.
///
/// The composer's reader interprets the pasteboard itself (an image is
/// written to disk and staged as an attachment) because a gpui text field
/// can only be handed a string. Blink needs no such help and must not get
/// it: `Frame::paste` takes the pasteboard directly, so HTML stays HTML.
/// The two never both run — `on_key`'s page gate returns before the
/// composer arm, which is what keeps one chord from having two readers.
pub fn paste_into_page(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("paste".to_string(), tab_id.to_string())));
    cef_paste_into_page(tab_id);
}

/// A4 — ⌘A. Select the whole document, or the focused field.
pub fn select_all(tab_id: &str) {
    #[cfg(test)]
    TEST_NAV_CALLS.with(|n| n.borrow_mut().push(("select-all".to_string(), tab_id.to_string())));
    cef_select_all(tab_id);
}

/// What this tab has selected, for the chrome to reason about (A4).
pub fn page_selection(tab_id: &str) -> Option<String> {
    selection::text_for(tab_id)
}

/// Is there anything for ⌘C to act on? (A4)
pub fn has_page_selection(tab_id: &str) -> bool {
    selection::has_selection(tab_id)
}

#[cfg(test)]
thread_local! {
    /// B4/B5 reaching Chromium, recorded so the feature-OFF suite can see
    /// them.
    ///
    /// Without this the only thing testable about ⇧⌘R is that it compiles:
    /// `cef_reload_hard`'s feature-OFF twin is an empty function, so a
    /// binding that never fired and a binding that fired correctly are the
    /// same observation. That is G26's shape — an ask nothing could
    /// assert — and it is worth a seam rather than a live walk every time
    /// the chord is touched.
    static TEST_NAV_CALLS: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Take what the page-command path sent to Chromium since the last call —
/// B4/B5's stop and hard reload, and A4's copy/cut/paste/select-all.
///
/// It exists because the feature-OFF twins are empty functions, so a chord
/// that never fired and one that fired correctly are otherwise the same
/// observation. That is G26's shape, and it is what localised B4's
/// unreachable escape arm in two runs.
#[cfg(test)]
pub fn take_nav_calls_for_test() -> Vec<(String, String)> {
    TEST_NAV_CALLS.with(|n| std::mem::take(&mut *n.borrow_mut()))
}

/// The devtools browser's id for a tab: a NAMESPACE, not a tab id.
///
/// The docked DevTools frontend is a second windowless browser riding the
/// same per-id OSR pipeline as the page (`BROWSERS`, `osr::PAGES`,
/// `page_element` all key by bare string and consult the strip nowhere).
/// The prefix is what keeps the two worlds apart: real tab ids are bare
/// UUIDs, so `devtools:` collides with nothing — and `drive`'s reaper and
/// the shown-pages feed recognise it explicitly.
pub fn devtools_id(tab_id: &str) -> String {
    format!("devtools:{tab_id}")
}

/// The tab a devtools id belongs to, or `None` for a real tab id.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn devtools_parent(id: &str) -> Option<&str> {
    id.strip_prefix("devtools:")
}

/// The hidden peek browser's id for a provider host — the same NAMESPACE
/// move as [`devtools_id`], for the same reason: it rides the per-id OSR
/// pipeline without a strip row, and the prefix is what keeps `drive`'s
/// reaper from reading it as a departed tab.
pub fn peek_id(host: &str) -> String {
    format!("peek:{host}")
}

/// Whether an id names a hidden peek browser.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn is_peek_id(id: &str) -> bool {
    id.starts_with("peek:")
}

/// The provider host a peek id was made for, or `None` for anything else.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn peek_host(id: &str) -> Option<&str> {
    id.strip_prefix("peek:")
}

thread_local! {
    /// Tabs whose DevTools dock is open, by PARENT tab id. Ungated — the
    /// dock's open/closed state is view state the feature-OFF suite must
    /// see, exactly like [`ask`]'s vocabulary. The browser behind it is
    /// gated; this bit is not.
    static DEVTOOLS_OPEN: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Whether this tab's DevTools dock is open.
pub fn devtools_open(tab_id: &str) -> bool {
    DEVTOOLS_OPEN.with(|d| d.borrow().contains(tab_id))
}

/// Close a tab's DevTools dock: state first (the pane is gone on this
/// render), then Chromium's half.
pub fn close_devtools(tab_id: &str) {
    DEVTOOLS_OPEN.with(|d| {
        d.borrow_mut().remove(tab_id);
    });
    // A rect outlives its frontend otherwise, and the NEXT dock would
    // open with the page already placed where the last one had it — before
    // this frontend has said anything.
    clear_dock_page_rect(tab_id);
    cef_close_devtools(tab_id);
}

/// The devtools BROWSER closed — the frontend's own ✕, the parent tab's
/// close, or an eviction. Clears the dock state so the pane does not
/// stand empty over a browser that no longer exists.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn devtools_note_closed(id: &str) {
    if let Some(parent) = devtools_parent(id) {
        let parent = parent.to_string();
        DEVTOOLS_OPEN.with(|d| {
            d.borrow_mut().remove(&parent);
        });
        clear_dock_page_rect(&parent);
        // The browser closed itself (or finished a Close we asked for).
        // Nothing else would redraw: a dying frontend delivers no frame.
        #[cfg(feature = "web")]
        host::note_chrome_change();
    }
}

#[cfg(feature = "web")]
fn cef_close_devtools(tab_id: &str) {
    tabs::close_devtools(tab_id);
}
#[cfg(not(feature = "web"))]
fn cef_close_devtools(_tab_id: &str) {}

/// Open DevTools for this tab's page, inspecting the element at the given
/// PAGE-space point — the menu's "Inspect" row.
///
/// The shim shape (state out here, Chromium behind `cef_*`) is what lets
/// the feature-OFF suite see the dispatch happen at all. Marks the dock
/// open before the CEF half runs: the pane should stand on the very next
/// render, not when Chromium gets around to `on_after_created`.
pub fn inspect_element(tab_id: &str, x: i32, y: i32) {
    #[cfg(test)]
    TEST_INSPECTS.with(|i| *i.borrow_mut() = Some((tab_id.to_string(), x, y)));
    // The dock rides Chromium's debugging port, and the port is off unless
    // the operator asked for it. Refuse BEFORE marking the dock open: the
    // ⌘⌥I toggle and the "Close DevTools" row both key off `devtools_open`,
    // so setting the flag and then failing would leave the menu offering to
    // close a dock that was never opened.
    if !devtools_available() {
        eprintln!(
            "[web] Inspect needs Chromium's DevTools port, which is off by \
             default. Relaunch with EAVE_DEVTOOLS_PORT=1 to enable it."
        );
        return;
    }
    DEVTOOLS_OPEN.with(|d| {
        d.borrow_mut().insert(tab_id.to_string());
    });
    cef_inspect_element(tab_id, x, y);
}

/// Whether the DevTools dock can work at all this session.
///
/// Feature-OFF answers **true**: there is no Chromium and no port, but the
/// dock's open/closed state is view state the default suite must still
/// exercise — the same reason `DEVTOOLS_OPEN` itself is ungated. What is
/// gated is whether a real port was opened.
pub fn devtools_available() -> bool {
    // In a test process the port policy is not the thing under test: the
    // harness asserts dock VIEW state, and `devtools_port_enabled` reads a
    // process-wide `OnceLock` that no test can vary. So tests default to
    // available and flip it explicitly with
    // [`set_devtools_available_for_test`] when the refusal is the subject.
    // The policy itself is tested purely, in `devtools_port_requested`.
    #[cfg(test)]
    {
        TEST_DOCK_AVAILABLE.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        #[cfg(feature = "web")]
        {
            host::devtools_port_enabled()
        }
        #[cfg(not(feature = "web"))]
        {
            true
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_DOCK_AVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Test seam: pretend the debugging port is on or off.
#[cfg(test)]
pub fn set_devtools_available_for_test(available: bool) {
    TEST_DOCK_AVAILABLE.with(|d| d.set(available));
}

/// Whether an operator's environment asks for Chromium's debugging port.
///
/// The decision, kept pure and ungated so the default suite runs it —
/// `nav_policy`/`evict_policy`'s pattern. Unset is off; so is an empty
/// value and a literal `0`, because `EAVE_DEVTOOLS_PORT=0` plainly reads
/// as "no" and a bare `is_ok()` would read it as yes.
///
/// Its only caller is `host::devtools_port_enabled`, which is gated, so
/// feature-OFF nothing reaches it and only its tests run. The allow is
/// scoped to exactly that state rather than blanket — `evict_policy`'s
/// reasoning, and for the same reason: genuinely dead code in the `web`
/// build should still be caught.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) fn devtools_port_requested(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "0")
}

#[cfg(feature = "web")]
fn cef_inspect_element(tab_id: &str, x: i32, y: i32) {
    tabs::show_devtools(tab_id, x, y);
}
#[cfg(not(feature = "web"))]
fn cef_inspect_element(_tab_id: &str, _x: i32, _y: i32) {}

#[cfg(test)]
thread_local! {
    static TEST_INSPECTS: std::cell::RefCell<Option<(String, i32, i32)>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: the last `inspect_element` call, taken.
#[cfg(test)]
pub fn take_inspect_for_test() -> Option<(String, i32, i32)> {
    TEST_INSPECTS.with(|i| i.borrow_mut().take())
}

// ---------------------------------------------------------------------------
// E9 — the agent-drivable page
// ---------------------------------------------------------------------------

/// Send one CDP call to a tab's page; returns the id its answer will carry.
///
/// The send side of E9 in one function. Collect the answer with
/// [`cdp::take`], which yields `None` until it lands — nothing here blocks,
/// because the answer arrives on a later pump and blocking the UI thread
/// waiting for Chromium is the deadlock this module is shaped to avoid.
pub fn cdp_call(tab_id: &str, method: &str, params: &serde_json::Value) -> u32 {
    #[cfg(test)]
    TEST_CDP.with(|c| {
        c.borrow_mut()
            .push((tab_id.to_string(), method.to_string(), params.to_string()))
    });
    cef_cdp_send(tab_id, method, params)
}

#[cfg(feature = "web")]
fn cef_cdp_send(tab_id: &str, method: &str, params: &serde_json::Value) -> u32 {
    inspector::send(tab_id, method, params)
}
#[cfg(not(feature = "web"))]
fn cef_cdp_send(tab_id: &str, method: &str, _params: &serde_json::Value) -> u32 {
    // No Chromium, so nothing will answer on its own. The call is still
    // ISSUED rather than refused, so the harness drives the real lifecycle
    // — issue, answer, collect — against the same state the `web` build
    // uses. A test supplies the answer with `cdp::complete`.
    cdp::begin(tab_id, method)
}

/// Run JavaScript in a tab and get its value back. See [`cdp::evaluate`]
/// for why the value and not a handle.
pub fn cdp_evaluate(tab_id: &str, expression: &str) -> u32 {
    let (method, params) = cdp::evaluate(expression);
    cdp_call(tab_id, method, &params)
}

/// Click a point in page space. Two ids, because a click is two events and
/// the release is the half a page's `click` listener waits for.
pub fn cdp_click(tab_id: &str, x: f64, y: f64) -> [u32; 2] {
    let [down, up] = cdp::click_events(x, y);
    [
        cdp_call(tab_id, down.0, &down.1),
        cdp_call(tab_id, up.0, &up.1),
    ]
}

/// Type text into whatever the page has focused.
pub fn cdp_type(tab_id: &str, text: &str) -> u32 {
    let (method, params) = cdp::insert_text(text);
    cdp_call(tab_id, method, &params)
}

/// Capture the page as a PNG; the answer carries `"data"`, base64.
pub fn cdp_screenshot(tab_id: &str) -> u32 {
    let (method, params) = cdp::screenshot();
    cdp_call(tab_id, method, &params)
}

// ---------------------------------------------------------------------------
// E8 — what a page did, where an agent can read it
// ---------------------------------------------------------------------------

/// One turn of the observation loop: enable the reporting domains on tabs
/// that lack them, then fold whatever has arrived into the per-tab stores.
///
/// Driven from the render pass with the other per-frame reconciliations.
/// **Ungated**, like the notification loop and for the same reason:
/// `cdp_call`'s feature-off twin issues the calls and a test supplies the
/// events, so the whole fold runs in the default suite.
pub fn drive_observation(snapshot: &eave_wire::TabsSnapshot) {
    observe::drive(snapshot);
}

/// What this tab printed, oldest first.
pub fn tab_console(tab_id: &str) -> Vec<observe::ConsoleLine> {
    observe::console(tab_id)
}

/// What this tab fetched, oldest first.
pub fn tab_exchanges(tab_id: &str) -> Vec<observe::Exchange> {
    observe::exchanges(tab_id)
}

// ---------------------------------------------------------------------------
// G2 — notifications eave actually delivers
// ---------------------------------------------------------------------------

/// One turn of the notification loop.
///
/// Called from the render pass, beside [`set_shown_pages`], because that
/// is where the tab snapshot is readable and where every other
/// once-per-frame reconciliation in this file already happens.
///
/// **Ungated on purpose.** `cdp_call` has a feature-off twin that issues
/// the call and lets a test answer it, so the whole loop — install, decide,
/// ack, expire — runs and is asserted in the default suite. The only thing
/// the `web` build adds is a Chromium on the other end.
pub fn drive_notifications(snapshot: &eave_wire::TabsSnapshot) {
    notify::drive(snapshot, std::time::Instant::now());
    // C4's indicator rides the same pass. Both are CDP shims over the same
    // event queue, and both have to run before the queue's cap drops what
    // they came for — a second timer would only add a way for one to starve
    // the other.
    audible::drive(snapshot);
}

/// The same turn, from the **pump**, between paints.
///
/// The render pass is the wrong clock on its own: gpui paints on input and
/// on animation, and a page that calls `requestPermission()` or
/// `new Notification()` while the operator's hands are still is an event
/// nobody reads. Measured 2026-09-03 — a page asked 600 ms after load and
/// its promise hung until a synthetic click made a frame; the toast timer
/// has the same shape (a toast outlived `TOAST_MS` until something else
/// repainted). E3's eviction clock learnt this first and reads the pump;
/// this reads the snapshot E3 cached for the same reason.
///
/// A change to what is on screen asks for a frame through the chrome-dirty
/// flag the page pipeline already uses, so the pump does not paint by
/// itself and an idle window stays idle (N4).
#[cfg(feature = "web")]
pub(super) fn tick_shims() {
    let Some(snapshot) = evict::last_snapshot() else {
        return;
    };
    let before = notify::live();
    drive_notifications(&snapshot);
    if notify::live() != before {
        host::note_chrome_change();
    }
}

/// C4 — whether this tab is making a noise.
///
/// **Not the same certainty as Chrome's indicator**, and the difference is
/// worth carrying to the call site: this is what the page's own document
/// reported over CDP, not what the audio output is doing. See
/// `web/audible.rs` for what that covers and what it misses.
pub fn tab_audible(tab_id: &str) -> bool {
    audible::is_audible(tab_id)
}

/// The binding name a page calls to reach eave. Re-exported because
/// `notify` is private and the UI harness has to raise the same event
/// Chromium would — naming the string twice is how the two drift.
pub use notify::BINDING as NOTIFY_BINDING;

/// What is on screen right now, oldest first. The renderer's whole view.
pub fn notifications() -> Vec<notify::Live> {
    notify::live()
}

/// A person clicked one away.
pub fn dismiss_notification(seq: u64) {
    notify::dismiss(seq);
}

/// Turn on the domains that make a tab report what it is doing — the E8
/// half. Until this runs, Chromium says nothing unprompted, and the
/// silence is indistinguishable from a page doing nothing.
pub fn cdp_watch(tab_id: &str) -> Vec<u32> {
    cdp::event_enables()
        .iter()
        .map(|(method, params)| cdp_call(tab_id, method, params))
        .collect()
}

// ---------------------------------------------------------------------------
// Bookmark peek — hover previews for bookmarked services
// ---------------------------------------------------------------------------

/// The render-ready peek for a bookmark's url, or `None` for a url no
/// provider claims. Cheap: one host parse and one map read.
pub fn peek_view_for_url(url: &str) -> Option<peek::View> {
    let host = crate::omnibox::engine_host(url);
    peek::view(&host, std::time::Instant::now())
}

/// Mark interest in a bookmark's peek. `active` means a card is up (a
/// hover) and may boot the hidden source browser; passive interest only
/// refreshes through pages that already exist. Idempotent per frame.
pub fn request_peek(url: &str, active: bool) {
    let host = crate::omnibox::engine_host(url);
    peek::request(&host, active, std::time::Instant::now());
}

/// Whether an id's browser has finished a main-frame load — the gate
/// that keeps peek's FIRST CDP message out of the creation gap, where
/// it would attach the DevTools agent to the initial empty document
/// (measured: the probe read `about:blank` off a browser whose page had
/// committed). Feature-off there is no gap to fall into.
pub fn page_has_loaded(id: &str) -> bool {
    #[cfg(feature = "web")]
    {
        tabs::page_loaded(id)
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = id;
        true
    }
}

/// One turn of the peek loop, beside the frame's other reconciliations.
/// Returns whether an answer is still being worked on for a live hover —
/// the caller keeps frames coming while it is, so the card repaints when
/// the answer lands.
pub fn drive_peek(snapshot: &eave_wire::TabsSnapshot) -> bool {
    peek::drive(snapshot, std::time::Instant::now())
}

#[cfg(test)]
thread_local! {
    static TEST_CDP: std::cell::RefCell<Vec<(String, String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Test seam: every `cdp_call` since the last take, as
/// `(tab_id, method, params-json)`.
#[cfg(test)]
pub fn take_cdp_calls_for_test() -> Vec<(String, String, String)> {
    TEST_CDP.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// What Chromium currently has this tab's page at, as a zoom LEVEL.
///
/// The read half of G5. There was never one before, which is how eave came
/// to state a zoom it had not checked.
#[cfg(feature = "web")]
fn cef_zoom_level(tab_id: &str) -> Option<f64> {
    zoom::level_now(tab_id)
}
#[cfg(not(feature = "web"))]
fn cef_zoom_level(_tab_id: &str) -> Option<f64> {
    None
}


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
        incognito: bool,
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
    Go { tab_id: String, delta: i32 },
    ConfirmedScheme { url: String },
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
    pub tooltip: Option<String>,
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

/// No Chromium, no PDF.
#[cfg(not(feature = "web"))]
pub fn print_to_pdf(_tab_id: &str) {}

// ---------------------------------------------------------------------------
// D11 — the link under the pointer
// ---------------------------------------------------------------------------

/// The longest status eave will hand the text system, in characters.
///
/// The bubble is capped at a fraction of the pane and ellipsises visually
/// anyway, so this is not the layout guard — it is the guard on shaping a
/// megabyte of `href` sixty times a second while the pointer moves.
const STATUS_MAX: usize = 512;

/// How long the pointer must rest on a link before the status bubble
/// draws. Chromium reports the URL the instant the cursor crosses into
/// an `<a>`, and drawing then makes the corner flash on the way past.
/// Hide is still immediate — a bubble that outlives the link it describes
/// is describing the wrong thing.
pub(crate) const STATUS_SHOW_DELAY: std::time::Duration = std::time::Duration::from_millis(800);

/// Whether a hover that started at `since` may draw at `now`.
///
/// Ungated and pure, same bargain as [`status_text`]: the delay is a
/// decision about two instants, and none of it needs Chromium. The pane
/// paints `chrome().status` only after this has gone true; it does not
/// decide the clock.
pub(crate) fn status_bubble_due(since: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(since) >= STATUS_SHOW_DELAY
}

/// What the status bubble says, or `None` for no bubble.
///
/// Ungated and pure, the [`download_rows`] shape: what the bubble SAYS is a
/// decision about a string, and none of it needs Chromium. The pane renders
/// this; it does not decide it. Without that split the feature-OFF suite
/// could not see this function at all — `chrome()`'s twin returns `None`, so
/// nothing about the bubble would be assertable (G26's lesson, applied
/// before it becomes a finding rather than after).
///
/// **The string is the page's, not eave's.** `on_status_message` carries
/// whatever the document put in `href`, which makes this attacker-controlled
/// text drawn on eave's own chrome, and two classes of character have to go:
///
/// - **Line breaks, tabs and the Unicode line/paragraph separators.** A
///   one-line bubble handed a `\n` is no longer one line, and U+2028/U+2029
///   are *not* `char::is_control()` — Rust's is the `Cc` category, and those
///   two are `Zl`/`Zp`. Testing `is_control()` alone reads as complete and
///   is not.
/// - **The bidi overrides** (U+202A–U+202E, U+2066–U+2069, plus the two
///   marks). They let a page draw its url right-to-left, so
///   `evil.com/moc.knab` renders as `bank.com/...` — the bubble exists to
///   answer "where does this go", and these are exactly the characters that
///   make it lie.
///
/// Truncation takes the TAIL and never the head, which is the same argument:
/// the scheme and host are what a person checks a status bubble for, so a
/// middle-elision that keeps a pretty path suffix would drop the only part
/// that carries a decision.
pub fn status_text(raw: &str) -> Option<String> {
    sanitize_page_text(raw)
}

/// D12 — what a `title=` attribute is allowed to draw.
///
/// The same treatment as [`status_text`] and for the same reason: a tooltip
/// is page-authored text on eave's chrome, so the newline and the bidi
/// override are the same threats here that they are there.
///
/// **Browsers do render multi-line tooltips and this one does not.** That is
/// a deliberate narrowing rather than an oversight — a `title` is not a
/// layout surface, and one collapsed line cannot be used to paint over the
/// window. If a real multi-line tooltip is ever wanted it needs a height cap
/// and a line cap decided together, which is a different function from this
/// one.
pub fn tooltip_text(raw: &str) -> Option<String> {
    sanitize_page_text(raw)
}

fn sanitize_page_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(STATUS_MAX));
    // Starts true so a leading run of space collapses to nothing at all.
    let mut last_was_space = true;
    for ch in raw.chars() {
        if matches!(ch,
            '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if ch.is_control() || ch.is_whitespace() || ch == '\u{2028}' || ch == '\u{2029}' {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        out.push(ch);
        last_was_space = false;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > STATUS_MAX {
        out = out.chars().take(STATUS_MAX).collect::<String>() + "…";
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// D4 — what the downloads panel says, and what a row does
// ---------------------------------------------------------------------------

/// What clicking a download row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadAction {
    /// Stop a transfer that is still running.
    Cancel(u32),
    /// Show the finished file in Finder. Carries the path, because the row
    /// is rendered from a snapshot and the map may have moved on by the
    /// time someone clicks.
    Reveal(String),
    /// Nothing to do: a cancelled transfer, a finished one whose path
    /// Chromium never settled, or the empty-state row.
    Nothing,
}

/// One row of the downloads panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow {
    pub label: String,
    pub action: DownloadAction,
    pub enabled: bool,
}

/// Turn the live download list into rows.
///
/// Ungated and pure, so the default feature-OFF suite runs it: what a row
/// SAYS is a decision about two integers and two bools, and none of it
/// needs Chromium. The panel then renders these; it does not decide them.
///
/// **`percent` is `-1` when Chromium does not know the total size** — the
/// header says so at `cef_download_item_capi.h:102`, "the rough percent
/// complete or -1 if the receive total size is unknown". A chunked response
/// with no `Content-Length` is the ordinary case, not a rare one, and a row
/// rendered straight from that number reads `-1%`.
pub fn download_rows(all: &[Download]) -> Vec<DownloadRow> {
    if all.is_empty() {
        return vec![DownloadRow {
            label: "No downloads".to_string(),
            action: DownloadAction::Nothing,
            enabled: false,
        }];
    }
    all.iter()
        .map(|d| {
            if d.cancelled {
                DownloadRow {
                    label: format!("{} — cancelled", d.file_name),
                    action: DownloadAction::Nothing,
                    enabled: false,
                }
            } else if d.done {
                // A finished download with no settled path cannot be
                // revealed. Chromium leaves `full_path` empty until it
                // commits the name, so this is reachable rather than
                // defensive.
                let revealable = !d.path.is_empty();
                DownloadRow {
                    label: d.file_name.clone(),
                    action: if revealable {
                        DownloadAction::Reveal(d.path.clone())
                    } else {
                        DownloadAction::Nothing
                    },
                    enabled: revealable,
                }
            } else {
                DownloadRow {
                    label: if d.percent < 0 {
                        format!("{} — downloading", d.file_name)
                    } else {
                        format!("{} — {}%", d.file_name, d.percent)
                    },
                    action: DownloadAction::Cancel(d.id),
                    enabled: true,
                }
            }
        })
        .collect()
}

/// The download list the panel is looking at.
///
/// **One reader, so the control that appears and the rows inside it can
/// never disagree** about whether this session has downloaded anything.
/// Two call sites reading two different things is how the tab rows and the
/// bookmark grid came to draw different favicons for one page.
///
/// Ungated. With `--features web` the list is Chromium's, from `tabs`;
/// without it there is nothing to download and the list is empty — which
/// would leave the whole panel unreachable in the DEFAULT feature state,
/// the one the suite runs. The `#[cfg(test)]` override is what lets the
/// harness drive it there, and it is the same seam shape `favicon_image`
/// already uses for the same reason.
pub fn download_list() -> Vec<Download> {
    #[cfg(test)]
    {
        let injected = TEST_DOWNLOADS.with(|d| d.borrow().clone());
        if !injected.is_empty() {
            return injected;
        }
    }
    downloads()
}

#[cfg(test)]
thread_local! {
    static TEST_DOWNLOADS: std::cell::RefCell<Vec<Download>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Test seam: hand the panel a list without a browser to produce one.
///
/// An empty list clears the override rather than pinning the panel to
/// empty — [`download_list`] falls through to the real reader, so a test
/// that never calls this sees exactly what the product sees.
#[cfg(test)]
pub fn set_downloads_for_test(list: Vec<Download>) {
    TEST_DOWNLOADS.with(|d| *d.borrow_mut() = list);
}

/// Stop a download in flight.
///
/// One ungated function rather than a gated pair, so the panel's click
/// path is the same code in both feature states and the default suite can
/// see that a row actually fired.
pub fn cancel_download(id: u32) {
    #[cfg(test)]
    {
        TEST_CANCELLED.with(|c| c.borrow_mut().push(id));
    }
    #[cfg(feature = "web")]
    tabs::cancel_download(id);
    #[cfg(not(feature = "web"))]
    {
        let _ = id;
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CANCELLED: std::cell::RefCell<Vec<u32>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TEST_REVEALED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Test seam: every id [`cancel_download`] was asked to stop since the
/// last take.
#[cfg(test)]
pub fn take_cancelled_downloads_for_test() -> Vec<u32> {
    TEST_CANCELLED.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Test seam: every path [`reveal_download`] was asked to show.
#[cfg(test)]
pub fn take_revealed_downloads_for_test() -> Vec<String> {
    TEST_REVEALED.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// Show a finished download in Finder.
///
/// **Reveal, not open, and that is a decision rather than a shortcut.**
/// Opening runs the file. Chromium quarantines every download as soon as
/// the file exists (see `quarantine::mark`, and 06-browser G4) precisely so
/// that Gatekeeper gets a say before anything executes — and a browser that
/// launches its own downloads on a single click hands that away. Revealing
/// puts the file in front of the person with the OS still in the loop.
///
/// Ungated: there is no Chromium in it. `open -R` is the documented way to
/// select a file in Finder, and a path that no longer exists makes it fail
/// quietly, which is the right outcome for a row someone clicked late.
pub fn reveal_download(path: &str) {
    if path.is_empty() {
        return;
    }
    // Under test this RECORDS instead of spawning. Not squeamishness: a
    // suite that reveals is a suite that opens Finder windows on the
    // machine running it, and `cargo test` must not reach outside the
    // process to be watched.
    #[cfg(test)]
    {
        TEST_REVEALED.with(|r| r.borrow_mut().push(path.to_string()));
    }
    #[cfg(not(test))]
    {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
}

/// Answer a tab's outstanding ask.
///
/// **06-browser G26 is closed here.** This used to be an empty function
/// beside a `pending()` that returned `None` unconditionally, so the whole
/// ask surface was invisible to `cargo test` and G25's modality fix
/// shipped code-verified only. The map now lives in `ask.rs` outside the
/// gate, so this really does take the ask off the screen and the harness
/// can drive the dialog end to end.
///
/// What is missing in this build is only what was always missing: there is
/// no Chromium continuation to reply to, because there is no page.
#[cfg(not(feature = "web"))]
pub fn answer(tab_id: &str, _answer: Answer) {
    ask::take(tab_id);
}

/// Raise an ask against a tab, for tests.
///
/// **Compiled in both feature states**, because the harness runs in both
/// and the map it writes to is ungated. What it deliberately does not do
/// is register a `Reply` — there is no Chromium continuation behind a
/// test's ask, and `dialogs::answer` handles that by removing the ask and
/// replying to nothing, which is exactly what should happen.
#[cfg(test)]
pub fn raise_ask_for_test(tab_id: &str, origin: &str, what: Ask) {
    ask::note_raised(Pending {
        tab_id: tab_id.to_string(),
        origin: origin.to_string(),
        ask: what,
    });
}

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

/// No page engine, so there is no handle to focus.
#[cfg(not(feature = "web"))]
pub fn page_focus_handle(
    _tab_id: &str,
    _cx: &mut gpui::App,
) -> Option<gpui::FocusHandle> {
    None
}

/// No page engine, so Chromium has nothing to focus.
#[cfg(not(feature = "web"))]
pub fn set_keyboard_page(_tab_id: Option<&str>) {}

/// Where the DevTools frontend says the inspected page belongs, in CSS
/// pixels of the frontend's viewport — which are the well's logical
/// pixels, because the frontend browser covers the whole well.
///
/// **This is eave being the embedder.** Chrome's DevTools window is
/// driven by C++ that answers `setInspectedPageBounds`; a frontend served
/// over http gets `InspectorFrontendHostStub`, where that method is
/// literally `setInspectedPageBounds(r){}`. The rect therefore comes back
/// the only way it can — measured off the placeholder element and printed
/// on the console, since the host object is not reachable from page
/// script. See `tabs::DOCK_SHIM_JS`.
///
/// Ungated and parsed here rather than in `tabs.rs` so the default suite
/// holds the decision: a malformed rect must be REFUSED, not clamped into
/// something plausible, because a plausible-but-wrong rect puts the page
/// somewhere quietly incorrect and the failure is only visible.
pub fn parse_dock_bounds(s: &str) -> Option<(f32, f32, f32, f32)> {
    let mut it = s.split(',');
    let mut next = || it.next()?.trim().parse::<f32>().ok();
    let (x, y, w, h) = (next()?, next()?, next()?, next()?);
    if it.next().is_some() {
        return None;
    }
    // A zero or negative extent is not a placement, and neither is a
    // non-finite one. Refusing leaves the last good rect standing.
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return None;
    }
    if w <= 0.0 || h <= 0.0 || x < 0.0 || y < 0.0 {
        return None;
    }
    Some((x, y, w, h))
}

thread_local! {
    static DOCK_PAGE_RECTS: std::cell::RefCell<
        std::collections::HashMap<String, (f32, f32, f32, f32)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Record where a tab's page should sit inside its dock. Returns whether
/// this actually moved anything, so a caller can avoid asking for a
/// repaint on every unchanged report.
pub fn set_dock_page_rect(tab_id: &str, rect: (f32, f32, f32, f32)) -> bool {
    DOCK_PAGE_RECTS.with(|m| {
        let mut m = m.borrow_mut();
        if m.get(tab_id) == Some(&rect) {
            return false;
        }
        m.insert(tab_id.to_string(), rect);
        true
    })
}

/// `None` until the frontend has reported — which is what the shell's
/// fallback keys off, so a dock whose frontend never reports stays on the
/// old side-by-side layout instead of losing the page entirely.
pub fn dock_page_rect(tab_id: &str) -> Option<(f32, f32, f32, f32)> {
    DOCK_PAGE_RECTS.with(|m| m.borrow().get(tab_id).copied())
}

/// Dropped when the dock closes: a stale rect would place the page inside
/// a frontend that is no longer there the next time one opens.
pub fn clear_dock_page_rect(tab_id: &str) {
    DOCK_PAGE_RECTS.with(|m| {
        m.borrow_mut().remove(tab_id);
    });
}

/// No page engine, so there is no stale frame to stretch — but the
/// SHELL's half of the rule is still worth pinning here, which is why
/// this stub records rather than discarding.
///
/// The rule is "the flag is on exactly while a divider is being dragged",
/// and the failure that matters is not turning it *on* — it is failing to
/// turn it back off, which leaves every page painting stretched for ever
/// and looks like a rendering bug with no obvious cause. That is a shell
/// bug, not a Chromium one, so the feature-off suite can hold it.
#[cfg(not(feature = "web"))]
pub fn set_live_divider(on: bool) {
    LIVE_DIVIDER_FOR_TEST.with(|c| c.set(on));
}

#[cfg(not(feature = "web"))]
thread_local! {
    static LIVE_DIVIDER_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The last value the shell published. See [`set_live_divider`].
///
/// Answered from whichever half is compiled, so the shell's rule is held
/// by BOTH suites: feature-off this is the stub's own record, and with the
/// feature on it is the flag `page::paint` actually consults. A test that
/// only existed in one state would leave the state that ships untested.
#[cfg(all(test, not(feature = "web")))]
pub fn live_divider_for_test() -> bool {
    LIVE_DIVIDER_FOR_TEST.with(|c| c.get())
}

#[cfg(all(test, feature = "web"))]
pub fn live_divider_for_test() -> bool {
    page::live_divider_for_test()
}

/// No page engine, so nothing is on screen as a page.
#[cfg(not(feature = "web"))]
pub fn set_shown_pages(_ids: impl IntoIterator<Item = impl AsRef<str>>) {}

#[cfg(not(feature = "web"))]
pub fn is_on_screen(_tab_id: &str) -> bool {
    false
}


// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;

    /// D11: the bubble draws a string the PAGE chose, so the things it
    /// refuses to draw are the feature.
    ///
    /// Each case asserts the threat is real in the same breath as the
    /// immunity — a bidi case that contained no bidi character, or a
    /// truncation case under the cap, would pass while asserting nothing.
    #[test]
    fn a_status_bubble_never_draws_what_a_page_asked_it_to() {
        assert_eq!(status_text(""), None);
        assert_eq!(status_text("   \t \n "), None);
        assert_eq!(
            status_text("  https://example.com/a  "),
            Some("https://example.com/a".to_string())
        );

        // One line, whatever the page sent. U+2028 and U+2029 are the two
        // that `is_control()` does NOT catch, which is why they are here
        // beside `\n` rather than trusted to it.
        let multi = "https://a.test/x\ny\u{2028}z\u{2029}w\tv";
        assert!(multi.chars().any(|c| c == '\u{2028}'));
        let one = status_text(multi).expect("a url with breaks still has text");
        assert_eq!(one, "https://a.test/x y z w v");
        assert!(!one.chars().any(|c| c.is_control()
            || c == '\u{2028}'
            || c == '\u{2029}'));

        // The bidi overrides go, and the visible text survives them.
        let spoof = "https://evil.test/\u{202E}moc.knab\u{202C}";
        assert!(spoof.chars().any(|c| ('\u{202A}'..='\u{202E}').contains(&c)));
        let clean = status_text(spoof).expect("stripping overrides leaves a url");
        assert!(!clean.chars().any(|c| ('\u{202A}'..='\u{202E}').contains(&c)));
        assert!(clean.starts_with("https://evil.test/"));

        // Truncation keeps the HEAD — the part that carries the decision.
        let long = format!("https://example.com/{}", "p".repeat(STATUS_MAX * 2));
        assert!(long.chars().count() > STATUS_MAX);
        let cut = status_text(&long).expect("a long url still has text");
        assert_eq!(cut.chars().count(), STATUS_MAX + 1, "cap plus the ellipsis");
        assert!(cut.starts_with("https://example.com/"));
        assert!(cut.ends_with('…'));

        // A url exactly at the cap is NOT truncated — the boundary, because
        // an off-by-one here would ellipsise a url that fits.
        let exact: String = std::iter::repeat('a').take(STATUS_MAX).collect();
        assert_eq!(status_text(&exact), Some(exact));
    }

    /// D11's delay is the popup, not the hide. The 799/800 pair is the
    /// boundary: an off-by-one here would either flash a brush-past or
    /// make the operator wait a frame longer than they asked.
    #[test]
    fn a_status_bubble_waits_eight_tenths_before_it_pops() {
        use std::time::{Duration, Instant};
        assert_eq!(STATUS_SHOW_DELAY, Duration::from_millis(800));
        let t0 = Instant::now();
        assert!(!status_bubble_due(t0, t0));
        assert!(!status_bubble_due(t0, t0 + Duration::from_millis(799)));
        assert!(status_bubble_due(t0, t0 + Duration::from_millis(800)));
        assert!(status_bubble_due(t0, t0 + Duration::from_millis(801)));
    }

    /// D12 gets D11's treatment, and this pins that rather than trusting
    /// the two functions to stay one line apart.
    ///
    /// A `title=` is as page-authored as an `href`, so a future edit that
    /// gave the tooltip its own laxer path would reintroduce the newline
    /// and the bidi override on the surface that draws nearest the pointer.
    #[test]
    fn a_tooltip_is_sanitised_exactly_as_a_status_is() {
        for raw in [
            "",
            "  ",
            "plain title",
            "two\nlines",
            "https://evil.test/\u{202E}moc.knab",
            "tab\tseparated",
        ] {
            assert_eq!(
                tooltip_text(raw),
                status_text(raw),
                "tooltip and status diverged on {raw:?}"
            );
        }
        // …and the shared treatment is not the identity, or the loop above
        // would hold for two functions that both did nothing.
        assert_eq!(tooltip_text("two\nlines"), Some("two lines".to_string()));
    }

    /// G5: every rung has to survive the round trip through CEF's float,
    /// because that float is what comes back out of Chromium and it is what
    /// the pane header is now derived from.
    #[test]
    fn every_rung_survives_the_trip_through_a_zoom_level() {
        for percent in LADDER {
            assert_eq!(
                percent_for(level_for(percent)),
                percent,
                "{percent}% did not come back"
            );
        }
    }

    /// The rung that proves the snap is load-bearing, measured on this
    /// tree: **33% does not survive the round trip.**
    ///
    /// ```text
    ///  33% -> 32.99999999999999289   <- `as u32` gives 32
    /// 110% -> 110.00000000000001421
    /// 175% -> 174.99999999999997158
    /// ```
    ///
    /// So a conversion that truncates reports the 33% rung as 32%, which is
    /// not on [`LADDER`] and which no ⌘+ could step off. `percent_for`
    /// snaps instead, and this is the case that would catch a rewrite back
    /// to arithmetic.
    #[test]
    fn the_thirty_three_rung_does_not_round_trip_exactly() {
        let level = level_for(33);
        let exact = 100.0 * 1.2f64.powf(level);
        assert!(exact < 33.0, "{exact} — the premise changed");
        assert_eq!(exact as u32, 32, "truncation is what snapping avoids");
        assert_eq!(percent_for(level), 33);
    }

    /// Chromium's own stored level for one notch down, taken off this
    /// machine's `Preferences` after a real ⌘-. Unlike the rung above this
    /// one *is* exact — which is the point: the ladder decides, so it does
    /// not matter which rungs happen to be lucky.
    #[test]
    fn chromiums_own_stored_level_reads_as_ninety() {
        assert_eq!(percent_for(-0.5778829311823857), 90);
    }

    /// Rest is exactly rest. A page nobody has zoomed must not draw a badge,
    /// and `zoom_sync` decides that by comparing against this.
    #[test]
    fn level_zero_is_exactly_a_hundred() {
        assert_eq!(percent_for(0.0), 100);
    }

    /// A level eave never set — a pinch, or an older release's ladder —
    /// still has to name a rung ⌘+ can step from, rather than a percentage
    /// that no keystroke can move off.
    #[test]
    fn an_off_ladder_level_snaps_to_a_reachable_rung() {
        let odd = level_for(137);
        let snapped = percent_for(odd);
        assert!(LADDER.contains(&snapped), "{snapped} is not on the ladder");
    }

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

    // -----------------------------------------------------------------
    // E9 — the agent-drivable page, through the public surface
    // -----------------------------------------------------------------

    /// The whole lifecycle: a call is issued, an answer is filed against
    /// it, and it is collected once.
    ///
    /// Issued with [`cdp::begin`] rather than [`cdp_call`] on purpose. The
    /// two builds legitimately differ at the send — with `web` on and no
    /// browser, the call is answered `NotSent` before this test could look
    /// at it — and that divergence is asserted directly by the pair of
    /// tests below rather than smuggled into this one.
    #[test]
    fn a_cdp_call_is_issued_answered_and_collected_once() {
        let id = cdp::begin("tab-e9", "Page.reload");
        assert!(cdp::is_pending(id), "an issued call is outstanding");
        assert_eq!(cdp::take(id), None, "no answer has arrived yet");

        cdp::complete(id, cdp::Outcome::Ok(r#"{"ok":true}"#.into()));
        assert!(!cdp::is_pending(id), "an answered call is no longer waiting");
        assert_eq!(
            cdp::take(id),
            Some(cdp::Outcome::Ok(r#"{"ok":true}"#.into()))
        );
        assert_eq!(cdp::take(id), None, "an answer is collected once");
    }

    /// Closing a tab must ANSWER its outstanding calls, not drop them: a
    /// caller polling for an answer that can never arrive is a hang, and
    /// this is exactly when it would happen.
    #[test]
    fn closing_a_tab_answers_the_calls_it_owed() {
        let mine = cdp::begin("tab-closing", "Page.captureScreenshot");
        let other = cdp::begin("tab-staying", "Page.captureScreenshot");
        forget_page("tab-closing");
        assert_eq!(cdp::take(mine), Some(cdp::Outcome::Gone));
        assert!(
            cdp::is_pending(other),
            "another tab's call is not collateral"
        );
    }

    /// **The two builds diverge here, deliberately.** With no Chromium
    /// there is nothing to answer, so the call stays open and the harness
    /// supplies the answer — which is what lets the feature-OFF suite drive
    /// the real lifecycle instead of a mock of it.
    #[cfg(not(feature = "web"))]
    #[test]
    fn with_no_chromium_a_call_stays_open_for_the_harness_to_answer() {
        let id = cdp_call("tab-e9", "Page.reload", &serde_json::json!({}));
        assert!(cdp::is_pending(id));
        assert_eq!(cdp::take(id), None);
    }

    /// The other half of that divergence, and the property that matters in
    /// the shipping build: a call to a tab with **no live browser** is
    /// answered `NotSent` at once rather than left outstanding. A caller
    /// polling [`cdp::take`] therefore terminates whether or not the page
    /// was evicted between deciding to call and calling.
    #[cfg(feature = "web")]
    #[test]
    fn with_no_browser_a_call_answers_not_sent_rather_than_hanging() {
        let id = cdp_call("tab-with-no-browser", "Page.reload", &serde_json::json!({}));
        assert!(!cdp::is_pending(id), "it did not stay outstanding");
        assert_eq!(cdp::take(id), Some(cdp::Outcome::NotSent));
    }

    /// `cdp_evaluate` must ask for a value. Without `returnByValue` the
    /// call SUCCEEDS and hands back an object handle, so an agent reading
    /// `result.value` gets nothing with no error anywhere.
    #[test]
    fn evaluating_asks_chromium_for_a_value() {
        let _ = take_cdp_calls_for_test();
        cdp_evaluate("tab-e9", "document.title");
        let calls = take_cdp_calls_for_test();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "Runtime.evaluate");
        let params: serde_json::Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(params["returnByValue"], true);
        assert_eq!(params["expression"], "document.title");
    }

    /// A click is two calls. One `mousePressed` is not a click, and a page
    /// listening for `click` never fires on it.
    #[test]
    fn a_click_reaches_the_page_as_two_calls() {
        let _ = take_cdp_calls_for_test();
        let ids = cdp_click("tab-e9", 4.0, 9.0);
        assert_ne!(ids[0], ids[1], "each half has its own id to answer on");
        let calls = take_cdp_calls_for_test();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.1 == "Input.dispatchMouseEvent"));
    }

    /// Typing is one call whatever the length, and it carries the text
    /// verbatim — including characters no keycode names.
    #[test]
    fn typing_is_one_call_that_carries_the_text() {
        let _ = take_cdp_calls_for_test();
        cdp_type("tab-e9", "héllo ⌘");
        let calls = take_cdp_calls_for_test();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "Input.insertText");
        let params: serde_json::Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(params["text"], "héllo ⌘");
    }

    /// Nothing is reported until the domains are enabled, so `cdp_watch`
    /// is what makes E8 a feed rather than a silence.
    #[test]
    fn watching_a_tab_enables_the_three_reporting_domains() {
        let _ = take_cdp_calls_for_test();
        let ids = cdp_watch("tab-e9");
        assert_eq!(ids.len(), 3);
        let methods: Vec<String> = take_cdp_calls_for_test()
            .into_iter()
            .map(|c| c.1)
            .collect();
        assert!(methods.contains(&"Network.enable".to_string()));
        assert!(methods.contains(&"Log.enable".to_string()));
        assert!(methods.contains(&"Runtime.enable".to_string()));
    }

    // -----------------------------------------------------------------
    // D4 — the downloads panel's rows
    // -----------------------------------------------------------------

    fn dl(name: &str, percent: i32, done: bool, cancelled: bool, path: &str) -> Download {
        Download {
            id: 7,
            url: "https://example.com/f".into(),
            file_name: name.into(),
            path: path.into(),
            received: 0,
            total: 0,
            percent,
            done,
            cancelled,
        }
    }

    /// An empty panel still says something. A menu that renders zero rows
    /// reads as broken rather than as empty.
    #[test]
    fn an_empty_panel_says_so_in_one_disabled_row() {
        let rows = download_rows(&[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "No downloads");
        assert!(!rows[0].enabled);
        assert_eq!(rows[0].action, DownloadAction::Nothing);
    }

    /// A live transfer shows its percentage and offers to stop.
    #[test]
    fn a_running_download_shows_its_percentage_and_cancels() {
        let rows = download_rows(&[dl("report.pdf", 42, false, false, "/tmp/report.pdf")]);
        assert_eq!(rows[0].label, "report.pdf — 42%");
        assert_eq!(rows[0].action, DownloadAction::Cancel(7));
        assert!(rows[0].enabled);
    }

    /// **The trap.** Chromium reports `-1` when it does not know the total
    /// size — a chunked response with no `Content-Length`, which is
    /// ordinary rather than rare. Rendered straight, that row reads `-1%`.
    #[test]
    fn an_unknown_total_never_renders_as_minus_one_percent() {
        let rows = download_rows(&[dl("stream.bin", -1, false, false, "/tmp/stream.bin")]);
        assert!(
            !rows[0].label.contains("-1"),
            "unknown size leaked a sentinel: {}",
            rows[0].label
        );
        assert_eq!(rows[0].label, "stream.bin — downloading");
        assert_eq!(rows[0].action, DownloadAction::Cancel(7));
    }

    /// A finished download drops the progress text and offers Finder.
    #[test]
    fn a_finished_download_reveals_rather_than_reporting_a_percentage() {
        let rows = download_rows(&[dl("notes.md", 100, true, false, "/tmp/notes.md")]);
        assert_eq!(rows[0].label, "notes.md");
        assert_eq!(
            rows[0].action,
            DownloadAction::Reveal("/tmp/notes.md".to_string())
        );
    }

    /// Chromium leaves `full_path` empty until it commits the name, so a
    /// finished row can have nowhere to point. Offering Finder anyway would
    /// be a click that does nothing.
    #[test]
    fn a_finished_download_with_no_path_offers_nothing() {
        let rows = download_rows(&[dl("notes.md", 100, true, false, "")]);
        assert_eq!(rows[0].action, DownloadAction::Nothing);
        assert!(!rows[0].enabled);
    }

    /// A cancelled transfer stays on the list and says why, rather than
    /// vanishing — a row that disappears reads as a bug in the panel.
    #[test]
    fn a_cancelled_download_says_so_and_does_nothing() {
        let rows = download_rows(&[dl("big.iso", 12, false, true, "/tmp/big.iso")]);
        assert_eq!(rows[0].label, "big.iso — cancelled");
        assert_eq!(rows[0].action, DownloadAction::Nothing);
        assert!(!rows[0].enabled);
    }

    /// The order it is given is the order it renders. `downloads()` sorts
    /// newest-first; re-sorting here would be a second opinion about
    /// ordering that could disagree with it.
    #[test]
    fn rows_keep_the_order_they_were_given() {
        let rows = download_rows(&[
            dl("first.txt", 10, false, false, "/tmp/a"),
            dl("second.txt", 20, false, false, "/tmp/b"),
        ]);
        assert!(rows[0].label.starts_with("first.txt"));
        assert!(rows[1].label.starts_with("second.txt"));
    }

    // -----------------------------------------------------------------
    // The DevTools debugging port is opt-in
    // -----------------------------------------------------------------

    /// Unset means off. This is the whole point of the gate: no shipping
    /// Chromium browser opens a debugging port by default, and eave did.
    #[test]
    fn an_absent_variable_does_not_open_the_port() {
        assert!(!devtools_port_requested(None));
    }

    /// `EAVE_DEVTOOLS_PORT=0` plainly reads as "no". A bare `is_ok()` on
    /// the variable would read it as yes, which is the trap this replaces.
    #[test]
    fn a_zero_or_empty_value_does_not_open_the_port() {
        assert!(!devtools_port_requested(Some("0")));
        assert!(!devtools_port_requested(Some("")));
    }

    /// **The gate's actual effect**, and the failure it prevents: with the
    /// port off, Inspect must refuse AND leave the dock flag clear. A
    /// refusal that set the flag would leave ⌘⌥I and the menu offering to
    /// close a dock that was never opened — and a pane standing over a
    /// frontend that can never connect, which is the defect `59b5af2`
    /// removed.
    #[test]
    fn with_the_port_off_inspect_refuses_and_the_dock_stays_shut() {
        let tab = "tab-port-off";
        set_devtools_available_for_test(false);
        inspect_element(tab, 3, 4);
        assert!(!devtools_open(tab), "no dock, and no flag claiming one");
        set_devtools_available_for_test(true);
        inspect_element(tab, 3, 4);
        assert!(devtools_open(tab), "and it opens once the port is on");
        close_devtools(tab);
    }

    /// Anything an operator would plausibly type to mean yes.
    #[test]
    fn an_asked_for_port_is_opened() {
        for value in ["1", "true", "yes", "9223"] {
            assert!(
                devtools_port_requested(Some(value)),
                "{value} should enable the port"
            );
        }
    }

    /// Refusing Inspect must NOT mark the dock open. ⌘⌥I and the "Close
    /// DevTools" row both key off `devtools_open`, so a refusal that set
    /// the flag would leave the menu offering to close a dock that was
    /// never opened. Feature-OFF the dock is always available, so this
    /// asserts the ordinary path leaves the flag consistent with what the
    /// menu will read.
    #[test]
    fn the_dock_flag_matches_what_inspect_actually_did() {
        let tab = "tab-dock-flag";
        assert!(!devtools_open(tab), "not open before");
        inspect_element(tab, 1, 2);
        assert_eq!(
            devtools_open(tab),
            devtools_available(),
            "the flag says open exactly when the dock could be opened"
        );
        close_devtools(tab);
        assert!(!devtools_open(tab), "and closing clears it");
    }

    /// Every call goes to the tab it names — one queue serves them all, so
    /// a screenshot must not answer another tab's page.
    #[test]
    fn a_call_is_addressed_to_the_tab_it_names() {
        let _ = take_cdp_calls_for_test();
        cdp_screenshot("tab-alpha");
        cdp_screenshot("tab-beta");
        let tabs: Vec<String> = take_cdp_calls_for_test()
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(tabs, vec!["tab-alpha".to_string(), "tab-beta".to_string()]);
    }
}
