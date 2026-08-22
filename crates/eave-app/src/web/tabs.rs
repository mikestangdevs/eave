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

/// Whether any tab has a live browser. The pump parks when this is false.
pub fn any_browser() -> bool {
    BROWSERS.with(|b| !b.borrow().is_empty())
}

/// Whether a tab has a live browser.
pub fn has_browser(tab_id: &str) -> bool {
    BROWSERS.with(|b| b.borrow().contains_key(tab_id))
}

/// Do something with a tab's browser, if it has one.
///
/// **The borrow ends BEFORE `f` runs, and that is the whole point of the
/// clone.** `f` is routinely a call into CEF — `close_browser`, `load_url`,
/// `send_key_event` — and under an external message pump CEF runs its
/// handlers *synchronously*, on this stack. `close_browser` calls
/// `on_before_close`, which takes `BROWSERS.borrow_mut()`. The obvious
/// one-liner (`b.borrow().get(tab_id).map(f)`) keeps the shared borrow alive
/// across `f` and therefore aborts the app on `already borrowed` every time
/// a browser is closed: Slice C6 measured ⌘Q doing it on `main` and C5's
/// departed-tab reaper doing it on ⌘W.
///
/// This is the general rule the whole `web` module is written to now: **a
/// foreign event loop is lent every borrow you hold.** Nine call sites route
/// through here, so the discipline belongs in the helper — fixing it at a
/// call site fixes that call site and nothing else.
///
/// `Browser` is a refcounted CEF handle, so `cloned()` is a retain and not a
/// copy of the browser.
pub fn with_browser<R>(tab_id: &str, f: impl FnOnce(&Browser) -> R) -> Option<R> {
    let browser = BROWSERS.with(|b| b.borrow().get(tab_id).cloned())?;
    Some(f(&browser))
}

/// Reconcile the page engine against the tab strip.
///
/// Called on every snapshot, and cheap when nothing changed — which is the
/// common case, because the strip's 120 ms fold means this is not called at
/// spinner rate.
pub fn drive(snapshot: &TabsSnapshot) {
    if std::env::var("EAVE_WEB_TRACE").is_ok() {
        eprintln!(
            "[web] drive running={} tabs={} active={:?} pending={:?}",
            super::host::running(),
            snapshot.tabs.len(),
            snapshot.active,
            snapshot.tabs.first().and_then(|t| t.pending.clone()),
        );
    }
    if !super::host::running() {
        return;
    }
    // ── THE ANSWER TO "WHAT HAPPENS BEHIND A SESSION TAB?" (Slice C5) ──
    //
    // C1 drove exactly one tab — the active one, when it was a page — and
    // left the real question open: *what happens when the active tab is a
    // session and a page is loading behind it?* Under C1's rule the answer
    // was silent and wrong. Switching to a session tab made this function
    // return before touching anything, so a page mid-load simply stopped
    // being driven: its pending navigation was never issued, and the strip
    // would have shown a spinner over a tab nobody was loading.
    //
    // **The decision: a page tab that already has a browser keeps loading,
    // whatever is in front of it.** A background load is the tab's own
    // business; being looked at is not a precondition for finishing. That
    // is what every browser does, and it is the only answer under which the
    // load state in the strip is a fact rather than a decoration.
    //
    // **Creation still requires activation**, and that is the whole of the
    // memory policy. A live browser is ~322 MB of helper processes, so
    // opening twenty tabs must not cost 6 GB — a tab that has never been
    // looked at has no browser and costs nothing. This is a deliberately
    // conservative floor and not a resident-set policy: nothing here evicts
    // a browser once created, so the ceiling is "tabs you have actually
    // visited this run". Eviction is a policy question with its own tradeoffs
    // (a discarded tab loses scroll, form state and JS heap) and it deserves
    // its own slice; see the report.
    //
    // There is therefore ONE notion of active in eave, and it is
    // `TabsState::active`. It decides which tab's frames are painted and
    // which tab may create a browser. It does not decide which tabs are
    // allowed to make progress.
    let active = snapshot.active.as_deref();
    for tab in snapshot
        .tabs
        .iter()
        .filter(|t| t.content == TabContent::Page)
    {
        let is_active = Some(tab.id.as_str()) == active;
        if !has_browser(&tab.id) && !is_active {
            // Never looked at. No browser, no cost, and nothing to drive —
            // its `pending` is still on the tab and will be issued as the
            // start url the moment it is activated.
            continue;
        }
        drive_tab(tab);
    }

    // A tab that left the strip takes its browser with it. Nothing did this
    // before C5 because nothing could close a tab — `EAVE_WEB_AT_BOOT` opens
    // one and never removes it — and ⌘W is what makes it reachable. Without
    // this a closed tab keeps ~322 MB of helper processes alive, keeps
    // loading, and keeps reporting facts about a tab the engine has already
    // forgotten (which the engine discards, so the leak is silent).
    //
    // Driven off the snapshot rather than off the close, for the same reason
    // everything else here is: the engine owns what a tab IS, and the
    // snapshot is how that arrives. A close handler would have to be told,
    // and would be wrong the moment a tab is closed by anything but this
    // window.
    let live: Vec<String> = BROWSERS.with(|b| b.borrow().keys().cloned().collect());
    for id in live {
        if snapshot.tabs.iter().any(|t| t.id == id) {
            continue;
        }
        if std::env::var("EAVE_WEB_TRACE").is_ok() {
            eprintln!("[web] closing browser for departed tab {id}");
        }
        with_browser(&id, |b| {
            if let Some(host) = b.host() {
                // FORCED. The unforced form runs the page's
                // `beforeunload`, and eave has no dialog surface to ask
                // the question on — so an unforced close would be
                // refusable by a page with nowhere for the refusal to be
                // seen, and this loop would re-issue it on every snapshot
                // forever. The tab is already gone from the model; the
                // decision has been made. A `beforeunload` that a person
                // can actually answer needs the dialog surface first, and
                // that is the same missing surface `<select>` needs.
                host.close_browser(1);
            }
        });
        super::host::request_pump();
    }
}

/// Reconcile one page tab. Split out of [`drive`] so the loop above reads as
/// the policy it is, and so the per-tab work is the same whether the tab is
/// the one being looked at or one loading behind a session.
fn drive_tab(tab: &eave_wire::TabInfo) {
    if !has_browser(&tab.id) {
        // The pending request is the start url, not a load to issue after
        // creation. `browser_host_create_browser` is ASYNC — the browser
        // does not exist until `on_after_created` — so creating and then
        // immediately calling `load_url` finds nothing to call it on, and
        // the request is lost with no error anywhere. (It cost a launch to
        // find: the tab opened, the strip showed it, and the page stayed
        // blank because the load had been dropped into a `None`.)
        let start = tab
            .pending
            .as_ref()
            .map(|p| p.url.as_str())
            .or(tab.url.as_deref());
        create(&tab.id, start);
        // Creating a browser is work CEF has to be pumped to finish.
        super::host::request_pump();
        super::host::note_activity();
        if let Some(pending) = tab.pending.as_ref() {
            // Marked as acted because the browser IS starting there.
            ACTED.with(|a| {
                a.borrow_mut().insert(tab.id.clone(), pending.seq);
            });
        }
        return;
    }

    // A navigation the operator asked for and nothing has answered.
    if let Some(pending) = tab.pending.as_ref() {
        let already = ACTED.with(|a| a.borrow().get(&tab.id).copied());
        if already != Some(pending.seq) {
            // Recorded only if it actually issued. Marking first and
            // failing second is what loses a navigation permanently: the
            // request would never be retried, because nothing about the
            // tab changes to prompt one.
            let issued = with_browser(&tab.id, |b| match b.main_frame() {
                Some(frame) => {
                    frame.load_url(Some(&CefString::from(pending.url.as_str())));
                    true
                }
                None => false,
            });
            if issued == Some(true) {
                ACTED.with(|a| {
                    a.borrow_mut().insert(tab.id.clone(), pending.seq);
                });
                super::host::request_pump();
                super::host::note_activity();
            }
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

    if std::env::var("EAVE_WEB_TRACE").is_ok() {
        eprintln!("[web] creating browser for {tab_id} at {url:?}");
    }
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

/// Copy the page's current selection to the system clipboard.
///
/// Slice E1. `Frame::copy` and not gpui's clipboard: the selection lives in
/// a document this process cannot read, so the only thing that knows what
/// "copy" means here is Blink. Routed through the MAIN frame, because a
/// selection inside an iframe is still the page's selection and CEF resolves
/// it from there.
pub fn copy_selection(tab_id: &str) {
    with_browser(tab_id, |b| {
        if let Some(frame) = b.main_frame() {
            frame.copy();
        }
    });
    super::host::request_pump();
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
    // **Deliberately does NOT clear the map.** Closing is asynchronous even
    // when forced: `close_browser(1)` only asks, and `on_before_close` — which
    // is what removes the entry — runs on a later pump. Clearing here would
    // make `any_browser()` report "all closed" the instant we asked, which is
    // precisely the lie that made `CefShutdown` hang: the caller believed the
    // browsers were gone and called it while Chromium still had one live.
    // An empty map is the readiness signal; see `host::shutdown`.
}

/// Drop every browser handle we still hold, with **no borrow held** — each
/// drop is a CEF release, and a release is a call into CEF like any other.
///
/// The last-resort partner to [`close_all`], for the case where a browser
/// never reported itself closed.
pub fn forget_all() {
    let taken = BROWSERS.with(|b| std::mem::take(&mut *b.borrow_mut()));
    drop(taken);
}

// ---------------------------------------------------------------------------
// Resident memory
// ---------------------------------------------------------------------------

/// Resident bytes across the CEF process tree, and how many processes.
///
/// **What this counts and does not**: the helper processes CEF spawned —
/// renderer, GPU, network, and the rest.
///
/// It does NOT count this process, because this process is also eave: gpui,
/// the rail, the transcript. Attributing all of it to "web" would bill the
/// app to the browser, and there is no honest way to split one address
/// space. So the figure is the page's own processes, and it is an
/// **undercount** by whatever Chromium holds in ours — measured at ~141 MB
/// for the combined app-and-browser process against ~322 MB of helpers.
///
/// It also does not count **eave's own engine**, which is a child of this
/// process too. Filtering on "is a child" alone billed the agent engine to
/// the browser and made the rail say six processes where five were the
/// page's — caught by taking a census rather than trusting the line. The
/// executable path is what tells them apart.
///
/// The same limit the agent line states applies here: resident sizes
/// double-count shared pages, so a sum across processes is an upper bound.
pub fn web_memory() -> Option<(usize, u64)> {
    if !super::host::running() {
        return None;
    }
    let children = eave_core::proc::child_pids(std::process::id())?;
    let mut n = 0;
    let mut total = 0u64;
    for pid in children {
        // CEF's helpers are the nested `<name> Helper*.app` bundles the
        // packaging step creates; nothing else in eave's tree is one.
        let is_helper = eave_core::proc::exe_path(pid).is_some_and(|p| p.contains("Helper"));
        if !is_helper {
            continue;
        }
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] browser created for {}", self.tab_id);
            }
            let Some(browser) = browser.cloned() else { return };
            if let Some(host) = browser.host() {
                // Explicit, not inherited. CEF's default here is 30.
                host.set_windowless_frame_rate(FRAME_RATE);
            }
            // `insert` returns whatever it evicted; dropping that here
            // rather than inside the closure keeps the CEF release off a
            // stack that holds the borrow.
            let evicted = BROWSERS.with(|b| b.borrow_mut().insert(self.tab_id.clone(), browser));
            drop(evicted);
            // Slice E1: zoom belongs to the TAB, not to the browser. A tab
            // whose browser was closed and re-made must come back at the
            // size it was left at — after the insert, because `reapply`
            // goes through `with_browser`.
            super::zoom_reapply(&self.tab_id);
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            // Removed, then dropped outside the borrow — see `with_browser`.
            let gone = BROWSERS.with(|b| b.borrow_mut().remove(&self.tab_id));
            drop(gone);
            super::osr::forget(&self.tab_id);
            // Slice E1. A find that outlived its page would state a count
            // about a document that is gone; the zoom is dropped for the
            // same reason `close_browser` is reached at all, which is that
            // the tab itself has left the strip.
            super::forget_page(&self.tab_id);
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_load_start is_main={:?} url={:?}",
                    frame.as_ref().map(|f| f.is_main()),
                    frame.as_ref().map(|f| CefString::from(&f.url()).to_string()));
            }
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_load_end status={_http_status_code}");
            }
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_load_error {:?}", error_text.map(|t| t.to_string()));
            }
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_title_change {:?}", title.map(|t| t.to_string()));
            }
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

        /// Slice E1. **In windowless mode this is not a customisation hook,
        /// it is the whole menu**: CEF has no window to put one in, so a
        /// client without this has a right-click that does nothing.
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(super::menu::PageContextMenuHandler::new(self.tab_id.clone()))
        }

        /// Slice E1. The find COUNT, which is the only thing about find
        /// that is eave's to state — the walking and the highlighting are
        /// Chromium's.
        fn find_handler(&self) -> Option<FindHandler> {
            Some(super::find::PageFindHandler::new(self.tab_id.clone()))
        }
    }
}
