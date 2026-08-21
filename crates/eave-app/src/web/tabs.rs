//! One browser per web tab, driven by the engine's tab model.
//!
//! # The direction of control
//! The engine owns what a tab IS; this owns what a page DOES. They meet at
//! two places and nowhere else:
//!
//! - **Down:** [`eave_wire::TabInfo::pending`] — a navigation the operator
//!   asked for. This reads it and loads it. It is not an instruction from
//!   the engine so much as a fact about the tab that only a renderer can
//!   act on.
//! - **Up:** [`Fact`] — what a page reported, on its way to a `tab_stated`
//!   frame. Nothing here writes tab state; it says what happened and the
//!   engine decides what that means.
//!
//! Keeping the two vocabularies apart is the whole reason Slice B1 built
//! `tab_navigate` and `tab_stated` as different frames, and the reason C1
//! could attach a page engine without the engine learning what Chromium is.

use std::collections::HashMap;
use std::sync::OnceLock;

use cef::*;
use eave_wire::{LoadState, PageFact, TabContent, TabsSnapshot};

/// Something a page reported, on its way to a `tab_stated` frame.
pub type Fact = (String, PageFact);

/// Where reports go. Set once, by the shell, when it has a connection to
/// report them over.
///
/// A channel rather than a drained queue so that nothing polls: a page that
/// reports a title twenty minutes from now should wake the sender then, and
/// an idle page should wake nothing at all. That property — an idle tab
/// costing nothing — is the one the spike measured (1 frame in 9 seconds)
/// and the one easiest to lose by accident.
static SINK: OnceLock<tokio::sync::mpsc::UnboundedSender<Fact>> = OnceLock::new();

/// Give the page engine somewhere to report. Facts raised before this is
/// called are dropped, which is correct: there is no connection to send
/// them over, and a queue held across one would be replaying stale page
/// state into a fresh engine.
pub fn install_fact_sink(tx: tokio::sync::mpsc::UnboundedSender<Fact>) {
    let _ = SINK.set(tx);
}

fn report(tab_id: &str, fact: PageFact) {
    if let Some(tx) = SINK.get() {
        let _ = tx.send((tab_id.to_string(), fact));
    }
}

/// CEF caps a windowless browser at `windowless_frame_rate`, and **its
/// default is 30** — not the display's rate, not the page's. The spike
/// located this by finding a hard 30 Hz ceiling on a page that could
/// produce far more. Set explicitly, so the number is a decision.
const FRAME_RATE: i32 = 60;

thread_local! {
    /// Live browsers by tab id. Main thread only: every CEF UI callback
    /// lands there under an external pump, and so does gpui's render.
    static BROWSERS: std::cell::RefCell<HashMap<String, Browser>> =
        std::cell::RefCell::new(HashMap::new());

    /// The last navigation sequence acted on, per tab. This is why
    /// `PendingNav` carries a seq at all: without it, asking for the same
    /// address twice would look like no change and the reload would be dead.
    static ACTED: std::cell::RefCell<HashMap<String, u64>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Whether a tab has a live browser.
pub fn has_browser(tab_id: &str) -> bool {
    BROWSERS.with(|b| b.borrow().contains_key(tab_id))
}

/// Do something with a tab's browser, if it has one.
pub fn with_browser<R>(tab_id: &str, f: impl FnOnce(&Browser) -> R) -> Option<R> {
    BROWSERS.with(|b| b.borrow().get(tab_id).map(f))
}

/// Reconcile the page engine against the tab strip.
///
/// Called on every snapshot, and cheap when nothing changed — which is the
/// common case, because the strip's 120 ms fold means this is not called at
/// spinner rate.
pub fn drive(snapshot: &TabsSnapshot) {
    if !super::host::running() {
        return;
    }
    // Slice C1 renders ONE tab: the active one, when it is a page. The model
    // may hold any number, and the reconciliation below is written per tab so
    // that lifting this restriction is a matter of choosing more ids, not of
    // restructuring. Every additional live browser is ~490 MB, so which tabs
    // stay resident is a policy question and deserves its own slice.
    let active = snapshot.active.as_deref();
    let Some(tab) = snapshot
        .tabs
        .iter()
        .find(|t| Some(t.id.as_str()) == active && t.content == TabContent::Page)
    else {
        return;
    };

    if !has_browser(&tab.id) {
        create(&tab.id, tab.url.as_deref());
    }

    // A navigation the operator asked for and nothing has answered.
    if let Some(pending) = tab.pending.as_ref() {
        let already = ACTED.with(|a| a.borrow().get(&tab.id).copied());
        if already != Some(pending.seq) {
            ACTED.with(|a| {
                a.borrow_mut().insert(tab.id.clone(), pending.seq);
            });
            with_browser(&tab.id, |b| {
                if let Some(frame) = b.main_frame() {
                    frame.load_url(Some(&CefString::from(pending.url.as_str())));
                }
            });
        }
    }
}

/// Create a windowless browser for a tab.
fn create(tab_id: &str, url: Option<&str>) {
    // `set_as_windowless` also forces Alloy runtime style — OSR and the
    // Chrome runtime style are mutually exclusive, which is a constraint
    // worth knowing before anything is built on Chrome-style UI.
    let mut window_info = WindowInfo::default().set_as_windowless(std::ptr::null_mut());
    window_info.shared_texture_enabled = 1;
    let v = super::osr::view(tab_id);
    window_info.bounds = Rect {
        x: 0,
        y: 0,
        width: v.width,
        height: v.height,
    };

    let settings = BrowserSettings {
        windowless_frame_rate: FRAME_RATE,
        ..Default::default()
    };

    let mut client = PageClient::new(tab_id.to_string());
    let start = CefString::from(url.unwrap_or("about:blank"));
    browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&start),
        Some(&settings),
        None,
        None,
    );
}

/// Close every browser. Runs before `CefShutdown`, which blocks forever if
/// a windowless browser is still live.
pub fn close_all() {
    let ids: Vec<String> = BROWSERS.with(|b| b.borrow().keys().cloned().collect());
    for id in ids {
        with_browser(&id, |b| {
            if let Some(host) = b.host() {
                host.close_browser(1);
            }
        });
    }
    BROWSERS.with(|b| b.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// Resident memory
// ---------------------------------------------------------------------------

/// Resident bytes across the CEF process tree, and how many processes.
///
/// **What this counts and does not**: the helper processes CEF spawned as
/// children of this one — renderer, GPU, network, and the rest. It does NOT
/// count this process, because this process is also eave: gpui, the rail,
/// the transcript. Attributing all of it to "web" would inflate the number
/// with the app itself, and there is no honest way to split one address
/// space. So the figure is the page's own processes, and it is an
/// UNDERCOUNT by whatever Chromium holds in ours.
///
/// The same two limits the agent line already states apply here too:
/// resident sizes double-count shared pages, so a sum across processes is
/// an upper bound per process and a lower bound overall.
pub fn web_memory() -> Option<(usize, u64)> {
    if !super::host::running() {
        return None;
    }
    let children = eave_core::proc::child_pids(std::process::id())?;
    let mut n = 0;
    let mut total = 0u64;
    for pid in children {
        if let Some(bytes) = eave_core::proc::resident_bytes(pid) {
            n += 1;
            total += bytes;
        }
    }
    (n > 0).then_some((n, total))
}

// ---------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------

wrap_life_span_handler! {
    struct PageLifeSpanHandler {
        tab_id: String,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser.cloned() else { return };
            if let Some(host) = browser.host() {
                // Explicit, not inherited. CEF's default here is 30.
                host.set_windowless_frame_rate(FRAME_RATE);
            }
            BROWSERS.with(|b| b.borrow_mut().insert(self.tab_id.clone(), browser));
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            BROWSERS.with(|b| { b.borrow_mut().remove(&self.tab_id); });
            super::osr::forget(&self.tab_id);
        }
    }
}

wrap_load_handler! {
    struct PageLoadHandler {
        tab_id: String,
    }

    impl LoadHandler {
        /// The commit. This is the url that ACTUALLY landed, after
        /// redirects — which is why a navigation request and a navigation
        /// commit are different frames and carry different urls.
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let Some(frame) = frame else { return };
            // Sub-frames load constantly and are not the tab's address. An
            // iframe's url in the omnibox is the classic version of this bug.
            if frame.is_main() != 1 {
                return;
            }
            let url = CefString::from(&frame.url()).to_string();
            if url.is_empty() {
                return;
            }
            report(&self.tab_id, PageFact::Committed { url, title: None });
            report(&self.tab_id, PageFact::Load { load: LoadState::Loading });
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            if frame.map(|f| f.is_main()) != Some(1) {
                return;
            }
            report(&self.tab_id, PageFact::Load { load: LoadState::Complete });
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _error_code: Errorcode,
            error_text: Option<&CefString>,
            _failed_url: Option<&CefString>,
        ) {
            if frame.map(|f| f.is_main()) != Some(1) {
                return;
            }
            // The engine's own words, rendered verbatim — `LoadState::Failed`
            // exists to carry them rather than to be re-worded by eave.
            let reason = error_text.map(|t| t.to_string()).unwrap_or_default();
            report(&self.tab_id, PageFact::Load { load: LoadState::Failed { reason } });
        }
    }
}

wrap_display_handler! {
    struct PageDisplayHandler {
        tab_id: String,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(|t| t.to_string()).filter(|t| !t.is_empty());
            report(&self.tab_id, PageFact::Titled { title });
        }
    }
}

wrap_client! {
    pub struct PageClient {
        tab_id: String,
    }

    impl Client {
        /// **Returning a RenderHandler at all is what puts CEF into the
        /// windowless path.** The `WindowInfo` flag alone is not enough —
        /// CEF asserts the client supplies one.
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(super::osr::PageRenderHandler::new(self.tab_id.clone()))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(PageLifeSpanHandler::new(self.tab_id.clone()))
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(PageLoadHandler::new(self.tab_id.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(PageDisplayHandler::new(self.tab_id.clone()))
        }
    }
}
