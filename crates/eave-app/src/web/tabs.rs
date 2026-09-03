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

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Instant;

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

/// Chromium lists icons best-first, but "best" is often a 16×16 ICO or
/// an SVG whose viewBox is 16. CEF's `DownloadImage` has no
/// `preferred_size`, so an SVG rasters at its intrinsic size — usually
/// 16 — and that is the blur. Prefer a sized PNG / apple-touch, then a
/// multi-res ICO, and take SVG last. The plate falls back to the globe
/// if decode fails.
fn pick_favicon_url(urls: &[String]) -> Option<String> {
    urls.iter()
        .filter(|u| !u.is_empty())
        .min_by_key(|u| favicon_url_score(u))
        .cloned()
}

fn favicon_url_score(u: &str) -> (u8, u8) {
    let l = u.to_ascii_lowercase();
    let size = if l.contains("apple-touch")
        || l.contains("fluidicon")
        || l.contains("fluid-icon")
        || l.contains("192x192")
        || l.contains("180x180")
        || l.contains("128x128")
        || l.contains("256x256")
        || l.contains("512x512")
    {
        0
    } else if l.contains("96x96") || l.contains("64x64") || l.contains("48x48") {
        1
    } else if l.contains("32x32") {
        2
    } else if l.contains("16x16") {
        6
    } else {
        3
    };
    let kind = if l.contains(".png") || l.contains(".webp") {
        0
    } else if l.contains(".ico") {
        1
    } else if l.contains(".jpg") || l.contains(".jpeg") {
        3
    } else if l.contains(".svg") {
        4
    } else {
        2
    };
    (size, kind)
}

/// CEF caps a windowless browser at `windowless_frame_rate`, and **its
/// default is 30** — not the display's rate, not the page's. The spike
/// located this by finding a hard 30 Hz ceiling on a page that could
/// produce far more. Set explicitly, so the number is a decision.
const FRAME_RATE: i32 = 60;

/// A peek browser's frame budget. It must stay VISIBLE (a never-shown
/// browser's DevTools agent answers from an empty document — walk
/// three), so the rate is where its cost is bounded instead: nothing
/// composites its frames, they exist only so Chromium treats the page
/// as real.
const PEEK_FRAME_RATE: i32 = 2;

/// The base background a page paints onto, ARGB — **opaque white**.
///
/// `set_as_windowless` documents this in one line and it is easy to read
/// past: *"Transparent painting is enabled by default but can be disabled
/// by setting CefBrowserSettings.background_color to an opaque value."*
/// Default is `0`, whose alpha byte is `0x00`, so every browser eave made
/// was painting **transparent**, and the theme showed through the page.
///
/// The failure is invisible until the app is not grey. A page only paints
/// the boxes it was told to paint: Gmail's header has `background: white`
/// and drew white, its buttons drew blue, its text drew black — and the
/// body, which declares no background at all, drew NOTHING and handed the
/// well's colour to the viewer. Themed orange, that reads as a tinted
/// page; on the greys the theme shipped with, it read as correct. **The
/// theme did not cause this bug, it revealed it.**
///
/// White rather than a theme colour, because the page is not ours to
/// colour — this is the base Chromium itself uses, and it is what the
/// reference browsers show. It is not a floor over dark pages either:
/// blink's `StyleEngine::UpdateColorSchemeBackground` recomputes the base
/// for a document that declares `color-scheme: dark`, so a dark page stays
/// dark. Alpha must be `0x00` or `0xFF`; CEF documents nothing between.
const PAGE_BACKGROUND: u32 = 0xFFFF_FFFF;

#[cfg(test)]
mod favicon_pick {
    use super::pick_favicon_url;

    #[test]
    fn prefers_png_over_ico() {
        let urls = [
            "https://a.test/favicon.ico".into(),
            "https://a.test/icon.png".into(),
        ];
        assert_eq!(
            pick_favicon_url(&urls).as_deref(),
            Some("https://a.test/icon.png")
        );
    }

    #[test]
    fn takes_an_ico_when_that_is_all_there_is() {
        let urls = ["https://a.test/favicon.ico".into()];
        assert_eq!(
            pick_favicon_url(&urls).as_deref(),
            Some("https://a.test/favicon.ico")
        );
    }

    #[test]
    fn prefers_ico_over_svg() {
        let urls = [
            "https://a.test/favicon.svg".into(),
            "https://a.test/favicon.ico".into(),
        ];
        assert_eq!(
            pick_favicon_url(&urls).as_deref(),
            Some("https://a.test/favicon.ico"),
            "CEF rasters SVG at its viewBox, usually 16"
        );
    }

    #[test]
    fn prefers_a_sized_png_over_a_bare_ico() {
        let urls = [
            "https://a.test/favicon.ico".into(),
            "https://a.test/favicon-32x32.png".into(),
        ];
        assert_eq!(
            pick_favicon_url(&urls).as_deref(),
            Some("https://a.test/favicon-32x32.png")
        );
    }
}

thread_local! {
    /// Tabs whose browser has been ASKED for and has not arrived yet.
    ///
    /// `browser_host_create_browser` is async: nothing lands in `BROWSERS`
    /// until `on_after_created`, so `has_browser` answers *false* for the
    /// whole gap. Anything that reconciles more than once inside that gap
    /// and keys off `has_browser` alone will ask for a second browser for
    /// the same tab — two renderers, two sets of helpers, and the second
    /// `insert` silently evicting the first.
    ///
    /// This was survivable while `drive` ran only on snapshots, which are
    /// far rarer than a browser start. Preloading reconciles on the pump,
    /// which is not, so the gap became reachable and had to be closed.
    static CREATING: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());

    /// Live browsers by tab id. Main thread only: every CEF UI callback
    /// lands there under an external pump, and so does gpui's render.
    static BROWSERS: std::cell::RefCell<HashMap<String, Browser>> =
        std::cell::RefCell::new(HashMap::new());

    /// The last navigation sequence acted on, per tab. This is why
    /// `PendingNav` carries a seq at all: without it, asking for the same
    /// address twice would look like no change and the reload would be dead.
    static ACTED: std::cell::RefCell<HashMap<String, u64>> =
        std::cell::RefCell::new(HashMap::new());

    /// Ids whose browser has finished a MAIN-frame load at least once.
    /// See `page_loaded` — the gate that keeps a first CDP message out
    /// of the creation gap.
    static LOADED: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());

    /// Tab ids whose browser was created against the private request
    /// context. The set, not the snapshot, because CEF callbacks cannot
    /// read the engine's model from this stack.
    static INCOGNITO_TABS: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());

    /// The one in-memory Chromium profile all private tabs share, the
    /// way one incognito window shares cookies. Empty `cache_path` is
    /// CEF's incognito: nothing is written to disk. Dropped when the
    /// last private tab's browser closes so the next private session
    /// is a fresh one.
    static INCOGNITO_CTX: std::cell::RefCell<Option<RequestContext>> =
        std::cell::RefCell::new(None);
}

fn tab_is_incognito(tab_id: &str) -> bool {
    INCOGNITO_TABS.with(|s| s.borrow().contains(tab_id))
}

fn incognito_context() -> Option<RequestContext> {
    if !super::host::running() {
        return None;
    }
    INCOGNITO_CTX.with(|slot| {
        if slot.borrow().is_none() {
            let settings = RequestContextSettings {
                cache_path: CefString::from(""),
                persist_session_cookies: 0,
                ..Default::default()
            };
            *slot.borrow_mut() = request_context_create_context(Some(&settings), None);
        }
        slot.borrow().clone()
    })
}

/// Whether any tab has a live browser. The pump parks when this is false.
pub fn any_browser() -> bool {
    BROWSERS.with(|b| !b.borrow().is_empty())
}

/// Whether a tab has a live browser.
pub fn has_browser(tab_id: &str) -> bool {
    BROWSERS.with(|b| b.borrow().contains_key(tab_id))
}

/// A browser for this tab exists or is on its way.
///
/// The question every caller actually means when it asks `has_browser`
/// before deciding whether to create one. See [`CREATING`].
pub fn browser_wanted(tab_id: &str) -> bool {
    has_browser(tab_id) || CREATING.with(|c| c.borrow().contains(tab_id))
}

/// Whether this id's browser has finished a main-frame load at least
/// once since it was (re)created.
pub fn page_loaded(tab_id: &str) -> bool {
    LOADED.with(|l| l.borrow().contains(tab_id))
}

/// Page tabs the well is painting this frame. Not the engine's `active`:
/// a split shows two, and the one that is not `active` still has to stay
/// composited and ineligible for eviction.
pub fn set_shown_pages(ids: impl IntoIterator<Item = impl AsRef<str>>) {
    SHOWN.with(|s| {
        let mut s = s.borrow_mut();
        s.clear();
        s.extend(ids.into_iter().map(|id| id.as_ref().to_string()));
    });
}

/// Whether this page tab is on screen, split pane or not.
pub fn is_on_screen(tab_id: &str) -> bool {
    SHOWN.with(|s| s.borrow().contains(tab_id))
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

/// B4 — stop loading this tab (⎋).
///
/// The abort this produces is folded as a stop rather than a failure; the
/// argument is on [`super::nav_policy::reports_failure`], and without it ⎋
/// would replace a half-drawn page with an error screen reading
/// `ERR_ABORTED`.
///
/// Silent when the tab has no browser. A stop on a tab that is not loading
/// is what Chromium already does with it: nothing.
pub(super) fn stop_load(tab_id: &str) {
    with_browser(tab_id, |b| b.stop_load());
    super::host::request_pump();
    super::host::note_activity();
}

/// B5 — reload this tab ignoring the cache (⇧⌘R).
///
/// **Not the same call as eave's ⌘R**, which goes through the engine's
/// `pending` so the reload is a fact the engine folds and the nav stack
/// stays the index of record (B1). A hard reload is a *cache* decision
/// about the page already there, so it is a browser command like zoom —
/// it re-fetches the current entry rather than pushing a new one, and the
/// engine's stack is unchanged by it and must stay so.
pub(super) fn reload_hard(tab_id: &str) {
    with_browser(tab_id, |b| b.reload_ignore_cache());
    super::host::request_pump();
    super::host::note_activity();
}

/// D10 — tell the page to leave fullscreen.
///
/// The half that keeps the two states from drifting. eave puts the WINDOW
/// into macOS fullscreen because the page asked, but the operator can take
/// the window back out by hand (the green button, ⌃⌘F, a swipe) and
/// Chromium never hears about it — the document would keep believing it is
/// fullscreen, keep its fullscreen stylesheet applied, and its own exit
/// control would toggle against a state nothing else agrees with.
///
/// `will_cause_resize: 1` because leaving fullscreen does change the page's
/// size here: eave's chrome comes back around it.
pub(super) fn exit_fullscreen(tab_id: &str) {
    with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.exit_fullscreen(1);
        }
    });
    super::host::request_pump();
    super::host::note_activity();
}

/// C4 (half) — mute or unmute this tab's audio.
///
/// # Why only half of C4
/// The row this serves cannot say "this tab is making noise", and that is a
/// CEF constraint rather than unfinished work. Learning that a page is
/// audible means accepting its stream through `AudioHandler`, and accepting
/// it routes the audio to eave as PCM *instead of to the speakers* — see
/// `PageAudioHandler::audio_parameters`, which declines for exactly that
/// reason. So eave would silence every page in exchange for knowing which
/// ones were making noise.
///
/// **Muting is unaffected by any of that.** `set_audio_muted` is a
/// `BrowserHost` call that does not go near the capture path, and
/// `is_audio_muted` reads back what eave itself set. The indicator is
/// blocked; the control is not, and shipping the control alone is worth
/// more than shipping neither.
pub(super) fn set_muted(tab_id: &str, muted: bool) {
    with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.set_audio_muted(i32::from(muted));
        }
    });
    super::host::note_activity();
    super::host::note_chrome_change();
}

/// Whether this tab is muted right now.
///
/// `false` when there is no browser. An evicted tab is not playing anything
/// to mute, and Chromium does not carry the flag across the new browser
/// that replaces it — which is [`super::evict`]'s standing warning about
/// per-browser state, and the reason this reads Chromium rather than a map
/// of eave's own that would go stale without saying so.
pub(super) fn is_muted(tab_id: &str) -> bool {
    with_browser(tab_id, |b| {
        b.host().map(|h| h.is_audio_muted() != 0).unwrap_or(false)
    })
    .unwrap_or(false)
}

/// Run `f` against SOME live browser — whichever the map yields first.
///
/// For work that needs Chromium's network stack but belongs to no tab: a
/// bookmark's favicon is the case this was added for. Spec 06-browser D13
/// says fetch through a browser that has the cookies rather than standing
/// up a second HTTP client, and a bookmark has no browser of its own.
///
/// Clones out before calling, exactly as [`with_browser`] does and for the
/// same reason: holding `BROWSERS.borrow()` across a synchronous CEF call
/// is what aborted the app on every quit until Slice C7.
pub fn with_any_browser<R>(f: impl FnOnce(&Browser) -> R) -> Option<R> {
    let browser = BROWSERS.with(|b| b.borrow().values().next().cloned())?;
    Some(f(&browser))
}

// ---------------------------------------------------------------------------
// What a page ASKS for (as against what it reports)
// ---------------------------------------------------------------------------

/// Something a page asked eave to do that only the engine can decide.
///
/// The mirror of [`Fact`], and deliberately a different type. A fact is
/// "this happened, fold it"; a request is "may I" — the engine can refuse,
/// reorder, or answer it differently, and a page that could write tab state
/// directly would be the second source of truth the whole `web` module is
/// built to avoid (module docs).
#[derive(Debug, Clone)]
pub enum PageRequest {
    /// `target="_blank"`, `window.open`, ⌘-click. Carries the tab that
    /// asked, so the engine can record the opener relationship it already
    /// models ([`eave_core::browser::snapshot::TabInfo::opener`]).
    OpenTab {
        url: String,
        opener: String,
        activate: bool,
        incognito: bool,
    },
    /// A navigation eave rewrote before it started — a tracked link with
    /// its trackers removed (`nav_policy::Decision::Rewrite`).
    ///
    /// **Asked for rather than performed**, and that is the borrow rule in
    /// `web/mod.rs` being obeyed rather than a style choice: the decision
    /// is made inside `on_before_browse`, which CEF runs synchronously on
    /// our stack, and calling `load_url` from in there would re-enter
    /// Chromium mid-callback. Cancelling and posting the clean URL back
    /// through the sink takes the same path every other navigation takes.
    Navigate { tab_id: String, url: String },
    /// A `mailto:` / `tel:` link. Not a page — the operating system's.
    ///
    /// Routed out here rather than spawned from the handler so that the one
    /// place eave shells out to `open(1)` stays in `ui.rs`, where the
    /// scheme guard and its test already live.
    External { url: String },
    /// What a person answered about an origin's camera, microphone,
    /// location… (G1).
    ///
    /// The **durable** half of the answer. `dialogs.rs` has already
    /// applied it to the app's mirror by the time this is sent, so the
    /// page is not waiting on it; this is the trip to the engine, which is
    /// the only thing here that survives a restart.
    RememberPermission {
        origin: String,
        kinds: Vec<super::permissions::PermissionKind>,
        decision: super::permissions::PermissionDecision,
    },
    /// Trackpad swipe (or anything else that wants the engine stack, not
    /// Chromium's). Same verb the arrows use.
    Go { tab_id: String, delta: i32 },
    /// A person confirmed an unknown scheme. `open_external` still
    /// refuses those; this path is the one that may call `open(1)`.
    ConfirmedScheme { url: String },
}

static REQUESTS: OnceLock<tokio::sync::mpsc::UnboundedSender<PageRequest>> = OnceLock::new();

/// Give the page engine somewhere to ask. Requests raised before this is
/// called are dropped, for the same reason facts are: there is no engine to
/// ask, and a queue held across a reconnect would open tabs for links
/// clicked before it.
pub fn install_request_sink(tx: tokio::sync::mpsc::UnboundedSender<PageRequest>) {
    let _ = REQUESTS.set(tx);
}

pub(super) fn request(req: PageRequest) {
    if let Some(tx) = REQUESTS.get() {
        let _ = tx.send(req);
    }
}

/// Whether `EAVE_WEB_TRACE` is set, asked once.
///
/// Cached because the callers now include `on_cursor_change`, which fires on
/// every pointer move across a boundary — an env lookup per mouse move is
/// the kind of cost that does not show up in a profile but is still wrong.
pub(super) fn trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("EAVE_WEB_TRACE").is_ok())
}

// ---------------------------------------------------------------------------
// Per-tab chrome state: what the page told us that the shell draws
// ---------------------------------------------------------------------------

/// The things a page states continuously that belong to the CHROME, not to
/// the tab model.
///
/// Deliberately app-local rather than `PageFact`s on the wire. The engine
/// journals what it is told and broadcasts it to every client; a cursor that
/// changes on every pointer move, or a progress value that changes sixty
/// times a second, would be a fold and a broadcast per frame for state that
/// is meaningless to any client that is not the one holding the mouse. The
/// rule this draws: **a fact crosses the wire when a second client would
/// need it, or when it must survive a restart.** A cursor is neither.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageChrome {
    /// 0.0 to 1.0 across a load.
    pub progress: f32,
    /// The link under the pointer, once the dwell has elapsed.
    /// Chromium's raw report is held until [`super::STATUS_SHOW_DELAY`].
    pub status: Option<String>,
    /// The `title=` of whatever is under the pointer.
    pub tooltip: Option<String>,
    /// The page has asked to be fullscreen.
    pub fullscreen: bool,
    /// What the pointer should look like over this page.
    pub cursor: gpui::CursorStyle,
}

thread_local! {
    /// Chrome state by tab. Main thread only, like `BROWSERS`: every writer
    /// is a CEF UI callback and every reader is a gpui render.
    static CHROME: std::cell::RefCell<HashMap<String, PageChrome>> =
        std::cell::RefCell::new(HashMap::new());

    /// Whether each tab is the one being looked at, as last told to CEF.
    /// See [`set_visibility`] — this exists so an unchanged visibility is
    /// not re-sent on every snapshot fold.
    static VISIBLE: std::cell::RefCell<HashMap<String, bool>> =
        std::cell::RefCell::new(HashMap::new());

    /// Page tabs the shell is currently painting, which may be more than
    /// the engine's `active` when two tabs share the well. Drive and
    /// eviction both read this so a split pane does not go hidden or get
    /// discarded for not being `active`.
    static SHOWN: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());

    /// The last url REPORTED per tab, so `on_address_change` can tell a
    /// real move from the echo of a load it already reported. See
    /// `on_address_change` for why the dedup is not left to the engine.
    static LAST_URL: std::cell::RefCell<HashMap<String, String>> =
        std::cell::RefCell::new(HashMap::new());

    /// D11's dwell: Chromium's latest link-under-pointer, and whether the
    /// bubble has already popped. `PageChrome.status` is the SHOWN value,
    /// so a hover that has not yet waited is not a chrome change.
    static STATUS_HOVER: std::cell::RefCell<HashMap<String, StatusHover>> =
        std::cell::RefCell::new(HashMap::new());
}

/// One tab's pending (or shown) link hover. See [`note_status_at`].
struct StatusHover {
    text: Option<String>,
    since: Instant,
    shown: bool,
}

/// Bumped whenever [`PageChrome`] actually CHANGES for any tab.
///
/// The pump redraws on delivered frames and never on vsync — the property
/// the spike measured an idle page at 1 frame in 9 seconds to protect. But
/// a cursor crossing a link boundary is a real change that may deliver no
/// frame at all, so the gate needs a second reason rather than a lower bar.
/// This is that reason: a counter that moves on change and not on callback,
/// so a page reporting the same cursor sixty times a second still costs
/// zero redraws.
static CHROME_REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times the chrome has changed. Read by the pump's redraw gate.
pub fn chrome_revision() -> u64 {
    CHROME_REV.load(std::sync::atomic::Ordering::Relaxed)
}

/// The back/forward arrows as **Chromium** sees them, for Slice E2's
/// `PageFact::Load`.
///
/// `None` when there is no browser on the callback — the engine then falls
/// back to its own nav stack, which is the D1 behaviour and the right
/// default. It is not `Some(false)`: "I could not ask" and "there is no
/// history" are different answers, and collapsing them is how the arrows
/// got hardcoded to false in the first place (`strip.rs:419`).
fn nav_flags(browser: Option<&mut Browser>) -> (Option<bool>, Option<bool>) {
    match browser {
        Some(b) => (Some(b.can_go_back() == 1), Some(b.can_go_forward() == 1)),
        None => (None, None),
    }
}

/// Move the redraw gate for a chrome change that is not a [`PageChrome`]
/// field — a download's progress, say. Same counter, same contract: it
/// moves on a CHANGE, never on a callback.
pub(super) fn bump_chrome_revision() {
    CHROME_REV.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn set_chrome(tab_id: &str, f: impl FnOnce(&mut PageChrome)) {
    let changed = CHROME.with(|c| {
        let mut map = c.borrow_mut();
        let entry = map.entry(tab_id.to_string()).or_default();
        let before = entry.clone();
        f(entry);
        *entry != before
    });
    if changed {
        CHROME_REV.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What the page last said about itself. `None` for a tab with no browser.
pub fn chrome(tab_id: &str) -> Option<PageChrome> {
    CHROME.with(|c| c.borrow().get(tab_id).cloned())
}

fn forget_chrome(tab_id: &str) {
    CHROME.with(|c| c.borrow_mut().remove(tab_id));
    LAST_URL.with(|u| u.borrow_mut().remove(tab_id));
    STATUS_HOVER.with(|h| {
        h.borrow_mut().remove(tab_id);
    });
    // Or a rebuilt browser for a reused tab id would inherit a visibility
    // that was never sent to it, and a restored background tab would paint
    // at full rate because this said it had already been hidden.
    VISIBLE.with(|v| v.borrow_mut().remove(tab_id));
}

/// Chromium just said what is under the pointer. Hide is immediate;
/// show waits [`super::STATUS_SHOW_DELAY`].
fn note_status(tab_id: &str, text: Option<String>) {
    note_status_at(tab_id, text, Instant::now());
}

fn note_status_at(tab_id: &str, text: Option<String>, now: Instant) {
    let mut apply: Option<Option<String>> = None;
    STATUS_HOVER.with(|h| {
        let mut map = h.borrow_mut();
        match &text {
            None => {
                map.remove(tab_id);
                apply = Some(None);
            }
            Some(next) => {
                let hover = map.entry(tab_id.to_string()).or_insert(StatusHover {
                    text: None,
                    since: now,
                    shown: false,
                });
                if hover.text.as_ref() == Some(next) {
                    return;
                }
                hover.text = Some(next.clone());
                if hover.shown {
                    apply = Some(Some(next.clone()));
                } else {
                    hover.since = now;
                }
            }
        }
    });
    if let Some(status) = apply {
        set_chrome(tab_id, |c| c.status = status);
    }
}

/// Commit any hover that has now waited its delay. The pump calls this
/// so the bubble appearing is a chrome revision on that same refresh —
/// a pointer sitting on a link delivers no frame, and without this the
/// dwell would only be noticed on the next incidental paint.
pub(super) fn promote_status() {
    promote_status_at(Instant::now());
}

fn promote_status_at(now: Instant) {
    STATUS_HOVER.with(|h| {
        for (id, hover) in h.borrow_mut().iter_mut() {
            if hover.shown {
                continue;
            }
            let Some(text) = hover.text.clone() else {
                continue;
            };
            if super::status_bubble_due(hover.since, now) {
                hover.shown = true;
                set_chrome(id, |c| c.status = Some(text));
            }
        }
    });
}

/// Record `url` as this tab's address, and say whether it CHANGED.
///
/// Both callers matter: `on_load_start` calls it to seed the value (and
/// ignores the answer, because a load is a report either way), and
/// `on_address_change` calls it to decide whether it has anything to say.
fn note_url(tab_id: &str, url: &str) -> bool {
    LAST_URL.with(|u| {
        let mut map = u.borrow_mut();
        match map.get(tab_id) {
            Some(prev) if prev == url => false,
            _ => {
                map.insert(tab_id.to_string(), url.to_string());
                true
            }
        }
    })
}

/// The address this tab is on, as last reported.
///
/// The initiator of a top-level navigation, for
/// [`super::nav_policy::decide`] — `None` on a tab's very first load, which
/// the policy treats as cross-site.
fn last_url(tab_id: &str) -> Option<String> {
    LAST_URL.with(|u| u.borrow().get(tab_id).cloned())
}

/// Chromium's cursor, as gpui's.
///
/// Not exhaustive and deliberately so: gpui has no equivalent for the
/// panning cursors or the eight-way resize set, and inventing one by
/// picking a near-miss would be worse than the arrow — a wrong cursor
/// states a wrong affordance. Everything unmapped falls to
/// [`gpui::CursorStyle::Arrow`], which is what it would have been anyway.
fn cursor_style(type_: CursorType) -> gpui::CursorStyle {
    use gpui::CursorStyle as G;
    match type_ {
        CursorType::HAND => G::PointingHand,
        CursorType::IBEAM => G::IBeam,
        CursorType::VERTICALTEXT => G::IBeam,
        CursorType::CROSS => G::Crosshair,
        CursorType::WAIT | CursorType::PROGRESS => G::OperationNotAllowed,
        CursorType::MOVE => G::ClosedHand,
        CursorType::COPY => G::DragCopy,
        CursorType::ALIAS => G::DragLink,
        CursorType::NOTALLOWED | CursorType::NODROP => G::OperationNotAllowed,
        CursorType::CONTEXTMENU => G::ContextualMenu,
        CursorType::EASTWESTRESIZE | CursorType::COLUMNRESIZE => G::ResizeLeftRight,
        CursorType::NORTHSOUTHRESIZE | CursorType::ROWRESIZE => G::ResizeUpDown,
        CursorType::EASTRESIZE => G::ResizeRight,
        CursorType::WESTRESIZE => G::ResizeLeft,
        CursorType::NORTHRESIZE => G::ResizeUp,
        CursorType::SOUTHRESIZE => G::ResizeDown,
        _ => G::Arrow,
    }
}

/// Ask a tab's browser to close. **The only place in eave that calls
/// `close_browser` on a page.**
///
/// Three callers now — the departed-tab reaper, [`close_all`] on the way to
/// `CefShutdown`, and Slice E3's eviction clock — and they were three
/// open-coded copies of the same six lines before this existed. That is
/// worth one function for the reason C7 gives about [`with_browser`]: a
/// discipline that has to be re-applied at each call site is a discipline
/// that will be missed at the next one, and the C7 audit found the missed
/// one by reading nine call sites rather than one.
///
/// FORCED, and the argument is the reaper's: the unforced form runs the
/// page's `beforeunload`, and eave has no dialog surface to ask the question
/// on — so an unforced close would be refusable by a page with nowhere for
/// the refusal to be seen, and the caller would re-issue it forever. For a
/// departed tab the decision has already been made. For an eviction it is
/// eave's own decision about eave's own memory, which is not a question to
/// put to the page either. A `beforeunload` a person can actually answer
/// needs the dialog surface first, and that is the same missing surface
/// `<select>` needs.
///
/// Closing is ASYNCHRONOUS even when forced. This only asks; the entry
/// disappears in `on_before_close`, on a later pump.
pub fn close_browser_for(tab_id: &str) {
    // **C7's rule, asserted rather than remembered.** Every close path in
    // eave funnels through here, so this is the one place that can check
    // the invariant for all of them: no cell in the `web` module may be
    // borrowed when Chromium is about to run our handlers on this stack.
    // A `debug_assert` because it costs nothing in a release build and
    // because the failure it replaces — `already borrowed`, unwinding
    // through Objective-C frames, aborting the process — is one nobody can
    // debug from the crash report (C6 spent a slice on it).
    debug_assert!(
        BROWSERS.with(|b| b.try_borrow_mut().is_ok()),
        "close_browser_for({tab_id}) with BROWSERS borrowed — see `with_browser`"
    );
    debug_assert!(
        super::evict::borrows_clear(),
        "close_browser_for({tab_id}) with an eviction cell borrowed — see `evict::tick`"
    );
    LOADED.with(|l| {
        l.borrow_mut().remove(tab_id);
    });
    // The dock browser dies with its tab (a close or an eviction alike):
    // it has no snapshot row, so nothing else would close it, and a
    // frontend over a dead target is noise. One level of recursion,
    // guarded by the prefix check.
    if super::devtools_parent(tab_id).is_none() {
        let dt = super::devtools_id(tab_id);
        if has_browser(&dt) {
            close_browser_for(&dt);
        }
    }
    // Through `with_browser`, so the borrow is dropped before CEF is
    // entered. See its doc comment for what holding it costs.
    with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.close_browser(1);
        }
    });
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
    // G28: the permission mirror is filled by a tokio task, and reaching
    // Chromium's content settings is a UI-thread call. This is the first
    // main-thread pass after a snapshot lands, so it is where the two
    // stores are reconciled. Cheap when nothing changed — one relaxed
    // swap of an `AtomicBool`.
    if super::permissions::take_replay_request() {
        super::dialogs::replay_into_chromium();
    }
    // The eviction clock reads the strip between snapshots — which tab is
    // active, which tabs are pages, which are pinned — so it is handed the
    // strip here, at the one place that knows a fold has finished.
    super::evict::note_snapshot(snapshot);
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
    // **Creation still requires activation**, and it is half of the memory
    // policy. A tab that has never been looked at has no browser and costs
    // nothing. (The other half of this sentence used to read "a live
    // browser is ~322 MB of helper processes, so opening twenty tabs must
    // not cost 6 GB". E3 measured that and it is wrong in an interesting
    // way: ~322 MB is almost exactly eave's floor with NO page resident,
    // and the marginal cost of a tab is far smaller. See
    // `thresholds::PAGE_IDLE_EVICT_AFTER` for the table. The rule survives
    // its bad justification — not creating a browser nobody asked to look
    // at is still right — but the arithmetic behind it does not.) This is a
    // deliberately
    // conservative floor, and it is no longer the whole policy: Slice E3
    // added the other half in `evict`, so a page tab nobody has looked at
    // for `thresholds::PAGE_IDLE_EVICT_AFTER` gives its browser back and
    // the ceiling stops being "tabs you have actually visited this run".
    // The tradeoff C5 named is real and is paid rather than avoided — a
    // discarded tab loses scroll, form state and JS heap — and the reason
    // it is affordable is that the back/forward stack was never in
    // Chromium to begin with (B1 put it in the engine). See `evict`.
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
        let shown = is_active || is_on_screen(&tab.id);
        if !has_browser(&tab.id) && !shown {
            // Not shown and not yet warmed. Its `pending` is still on the
            // tab and will be issued as the start url when a browser
            // arrives — either from the preload pass below, or on
            // activation if the cap never reached this tab. A split pane
            // counts as shown even when it is not `active`.
            continue;
        }
        drive_tab(tab);
        set_visibility(&tab.id, shown);
    }

    // A tab that left the strip takes its browser with it. Nothing did this
    // before C5 because nothing could close a tab — `EAVE_WEB_AT_BOOT` opens
    // one and never removes it — and ⌘W is what makes it reachable. Without
    // this a closed tab keeps its share of the helper processes alive, keeps
    // loading, and keeps reporting facts about a tab the engine has already
    // forgotten (which the engine discards, so the leak is silent).
    //
    // Driven off the snapshot rather than off the close, for the same reason
    // everything else here is: the engine owns what a tab IS, and the
    // snapshot is how that arrives. A close handler would have to be told,
    // and would be wrong the moment a tab is closed by anything but this
    // window.
    // ── WARMING, WHICH IS THE OTHER HALF OF THE MEMORY POLICY ──
    //
    // Until this existed, `shown` above was the ONLY thing that could
    // create a browser, so every tab restored at launch booted Chromium the
    // first time it was opened or dragged into a split, and the well showed
    // `PAGE_STARTING` over an empty pane while it came up. Creation is
    // async (`on_after_created`), so that gap is unavoidable once you are
    // already waiting for it — the fix is to not be waiting for it.
    //
    // The rule lives in `evict_policy::preload_queue` because it is a
    // question about tabs and timestamps with no Chromium in it, and
    // because `evict_policy` compiles in BOTH feature states while this
    // module does not. A preload rule the default test run never executed
    // would be a rule nobody checks.
    //
    // **One per pass, deliberately.** Eight `create_browser` calls in a
    // single turn asks for eight sets of helper processes at once, on the
    // pump, during launch — the moment eave can least afford it. The queue
    // is re-derived next tick and `request_redrive` makes sure there is one.
    let warm = super::evict_policy::preload_queue(
        &snapshot.tabs,
        super::evict::residency,
        eave_core::thresholds::page_preload_max(),
    );
    if let Some(id) = warm.first() {
        if let Some(tab) = snapshot.tabs.iter().find(|t| &t.id == id) {
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] warming {} ({} queued)", tab.id, warm.len());
            }
            drive_tab(tab);
            // **Visibility is deliberately NOT set here.** `set_visibility`
            // routes through `with_browser`, which does nothing while the
            // browser is still being created — but it records the value
            // first, so a `false` written now makes the real call later look
            // unchanged and `was_hidden(1)` is never sent. A warmed tab
            // would then render at full rate, unwatched, for as long as it
            // lived. The loop above hides it on the next pass, once the
            // browser exists and the call can actually land.
        }
        if warm.len() > 1 {
            super::evict::request_redrive();
        }
        super::host::request_pump();
    }

    // A tab can also depart while its browser is still being created, and
    // that one is not in `BROWSERS` to be reaped. Left marked, it would be
    // permanently unwarmable — `drive_tab` would return early forever — so
    // the mark is pruned against the strip rather than against the map.
    CREATING.with(|c| {
        c.borrow_mut().retain(|id| {
            // A peek browser has no strip row at any point in its life —
            // pruning it against the strip would mark it unwanted the
            // same pass it was asked for, and `ensure_peek_browser` would
            // ask again forever. Its owner is the peek store.
            if super::is_peek_id(id) {
                return super::peek::browser_wanted(id);
            }
            let key = super::devtools_parent(id).unwrap_or(id);
            snapshot.tabs.iter().any(|t| t.id == key)
        })
    });

    let live: Vec<String> = BROWSERS.with(|b| b.borrow().keys().cloned().collect());
    for id in live {
        // A devtools browser has no snapshot row of its own — its life is
        // its PARENT's. While the parent tab stands, keep it and drive its
        // visibility (nothing else calls `set_visibility` for it, and a
        // browser never told `was_hidden` renders at full rate forever);
        // when the parent departs the strip, fall through to the close.
        if let Some(parent) = super::devtools_parent(&id) {
            if snapshot.tabs.iter().any(|t| t.id == parent) {
                set_visibility(&id, is_on_screen(&id));
                continue;
            }
        } else if super::is_peek_id(&id) {
            // A peek browser stays VISIBLE — `was_hidden(1)` on a
            // browser that was never shown left its DevTools agent
            // answering from an empty document forever (walk three's
            // `at=""` loop; the load-order gate did not change it, this
            // did). The rendering it buys is bounded by
            // `PEEK_FRAME_RATE` at creation, not by this loop.
            if super::peek::browser_wanted(&id) {
                set_visibility(&id, true);
                continue;
            }
        } else if snapshot.tabs.iter().any(|t| t.id == id) {
            continue;
        }
        if std::env::var("EAVE_WEB_TRACE").is_ok() {
            eprintln!("[web] closing browser for departed tab {id}");
        }
        close_browser_for(&id);
        // The tab is gone from the model, so what this module knew about
        // its idleness is not a fact about anything any more.
        super::evict::forget(&id);
        super::host::request_pump();
    }
}

/// `EAVE_WEB_PROBE_CLICK=x,y` — click once INSIDE the page, from in here,
/// as soon as it has loaded.
///
/// # Why this exists rather than a synthetic OS click
/// The capture rig drives eave the way a person does: a Quartz mouse event
/// at screen coordinates. That stopped working on this machine — a control
/// run clicked eave's OWN reload button and `on_load_start` did not move,
/// so clicks were reaching no window at all, and any conclusion drawn from
/// a shot taken that way would have been about the machine rather than
/// about eave. (`scripts/dev.sh` running from another checkout kills every
/// eave on the machine on each rebuild and `open -n`s its own, taking
/// frontmost with it.)
///
/// A probe that posts the click through CEF needs no window to be key, no
/// process to be frontmost, and nothing else on the machine to hold still.
/// It proves strictly less than a real click — it does not exercise
/// gpui's hit-testing or the window-to-page coordinate conversion, both of
/// which `page.rs` owns — and it proves exactly the thing a `<select>`
/// needs proven: that Chromium opens a popup and that eave receives,
/// blits and places it.
///
/// Coordinates are PAGE space, which for a probe page is the only space
/// that is knowable without measuring the frame first.
///
/// Same family as `EAVE_WEB_AT_BOOT` and `EAVE_WEB_TRACE`: instrumentation
/// that only exists when asked for, and costs one `env::var` per load when
/// it is not.
/// Fired from the paint path, not from `on_load_end`.
///
/// The first version fired on load-end and produced nothing: the trace shows
/// `probe click` landing BEFORE `accel_paint #0`, so the click was hit-tested
/// against a document that had loaded but not yet laid out, and Chromium had
/// no `<select>` box under the point. Waiting for real pixels is the whole
/// correction — a page that has painted is a page with boxes in it.
pub(super) fn probe_click(tab_id: &str) {
    let Ok(spec) = std::env::var("EAVE_WEB_PROBE_CLICK") else {
        return;
    };
    // `x,y` is a whole click; `x,y,hold` presses and does not release.
    //
    // Both are needed and neither is the obvious default for the other. A
    // `<button onclick=…>` fires on the UP, so a held press does nothing at
    // all; a `<select>` opens on the DOWN and the matching up closes it
    // again, so a whole click leaves nothing on screen to photograph. The
    // full click is the default because it is what a person does.
    let mut parts = spec.split(',');
    let (Some(x), Some(y)) = (parts.next(), parts.next()) else {
        eprintln!("eave: EAVE_WEB_PROBE_CLICK wants `x,y` or `x,y,hold`, got {spec:?}");
        return;
    };
    let hold = parts.next().map(str::trim) == Some("hold");
    let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>()) else {
        eprintln!("eave: EAVE_WEB_PROBE_CLICK wants two numbers, got {spec:?}");
        return;
    };
    let at = gpui::point(gpui::px(x), gpui::px(y));
    let m = gpui::Modifiers::default();
    eprintln!("[web] probe click at {x},{y} in {tab_id} (hold={hold})");
    with_browser(tab_id, |b| {
        // Focus first, for the same reason `page.rs` does it on a real
        // click: a page that has never been focused receives no input.
        super::input::focus(b, true);
        super::input::mouse_button(b, at, &m, MouseButtonType::LEFT, false, 1);
        if !hold {
            super::input::mouse_button(b, at, &m, MouseButtonType::LEFT, true, 1);
        }
    });
    super::host::note_activity();
    super::host::request_pump();
}

/// Tell a browser whether anybody is looking at it.
///
/// **Nothing called `was_hidden` before this** — confirmed independently by
/// Slice E1 on `a4b3983` — so every page tab behind another tab went on
/// compositing at 60 Hz into a buffer nothing painted. Twenty background
/// tabs were twenty renderers doing full work for no viewer.
///
/// # This is not eviction, and it does not contradict C5
/// C5's rule stands: **a page tab that already has a browser keeps loading,
/// whatever is in front of it.** `was_hidden` does not touch loading,
/// script, timers or network — it is Chromium's own "this view is not on
/// screen", the same signal a background tab in Chrome gets. The page still
/// finishes its load behind a session tab and still reports the facts that
/// drive the strip's spinner; it just stops producing frames nobody is
/// going to draw. Slice E3's eviction is the next step up the same ladder
/// and answers a different question (memory, not GPU).
///
/// # Why it is idempotent here rather than tracked
/// `drive` runs on every snapshot, so this would otherwise re-issue the
/// same visibility on every fold. CEF treats a repeat as a no-op, but the
/// call still crosses into Chromium, so the last value is remembered per
/// tab and only a CHANGE is sent — the same shape as `set_view`'s
/// size check, and for the same reason.
fn set_visibility(tab_id: &str, visible: bool) {
    let changed =
        VISIBLE.with(|v| v.borrow_mut().insert(tab_id.to_string(), visible) != Some(visible));
    if !changed {
        return;
    }
    if trace() {
        eprintln!("[web] was_hidden({}) for {tab_id}", !visible);
    }
    with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.was_hidden(if visible { 0 } else { 1 });
        }
    });
}

/// Reconcile one page tab. Split out of [`drive`] so the loop above reads as
/// the policy it is, and so the per-tab work is the same whether the tab is
/// the one being looked at or one loading behind a session.
fn drive_tab(tab: &eave_wire::TabInfo) {
    if !has_browser(&tab.id) {
        // Asked for already and still coming. Asking twice would make two.
        if CREATING.with(|c| c.borrow().contains(&tab.id)) {
            return;
        }
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
        // Seed the address dedup from the tab's COMMITTED url, not from
        // whatever the browser reports first.
        //
        // Slice E3 makes a browser's lifetime shorter than its tab's: an
        // idle page tab is evicted and rebuilt at the same url under the
        // same tab id. `on_before_close` clears this tab's chrome state on
        // the way out, so without seeding here the rebuilt browser's first
        // `on_address_change` would look like a move to a url the tab is
        // already standing on — and the engine would fold it as a reload
        // and write a `Transition::Reload` visit for a page nobody
        // reloaded. Seeding from the tab makes the dedup a property of the
        // TAB rather than of the browser, which is also what makes it
        // survive a restart.
        if let Some(url) = tab.url.as_deref() {
            note_url(&tab.id, url);
        }
        create(&tab.id, start, tab.incognito);
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
            let issued = with_browser(&tab.id, |b| {
                // A walk must use Chromium's stack. `load_url` of the
                // walked address would push a fresh CEF entry, and the
                // next `can_go_back` would disagree with the engine
                // stack the arrows read. Fall back to a load only when
                // Chromium has nowhere to walk — a restored tab whose
                // CEF history is empty.
                if let Some(delta) = pending.delta {
                    if delta < 0 && b.can_go_back() != 0 {
                        b.go_back();
                        return true;
                    }
                    if delta > 0 && b.can_go_forward() != 0 {
                        b.go_forward();
                        return true;
                    }
                }
                match b.main_frame() {
                    Some(frame) => {
                        frame.load_url(Some(&CefString::from(pending.url.as_str())));
                        true
                    }
                    None => false,
                }
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
    if let Some(url) = tab.favicon_url.as_deref() {
        super::favicon::fetch(&tab.id, url, tab.url.as_deref().unwrap_or_default());
    } else if let Some(page) = tab.url.as_deref() {
        if let Some(ico) = super::favicon::fallback_url(page) {
            super::favicon::fetch(&tab.id, &ico, page);
        }
    }
}

/// Create the hidden browser a bookmark peek reads through — the
/// `devtools:` move (a windowless browser whose id the strip has never
/// heard of), owned by the peek store instead of a parent tab.
///
/// Same `create` as a page tab, so it shares the default request context
/// — the cookies that make the peek show YOUR inbox — and the same
/// `CREATING` guard, so asking on every retry tick makes one browser,
/// not one per tick. Visibility is deliberately not touched here: the
/// browser does not exist yet, and `set_visibility` records before it
/// applies (the preload arm's trap). `drive`'s reaper hides it on the
/// first pass after `on_after_created`.
pub fn ensure_peek_browser(id: &str, url: &str) {
    if !super::host::running() {
        return;
    }
    if browser_wanted(id) {
        return;
    }
    create(id, Some(url), false);
    super::host::request_pump();
    super::host::note_activity();
}

/// Point a hidden peek browser back at its url — the parked-browser
/// case: a calendar that redirected away from the agenda view, a feed
/// that landed on a sign-in page the operator has since completed. The
/// peek store bounds how often this is asked for; a no-op while the
/// browser is still being created (`with_browser` finds nothing), which
/// the store's retry clock absorbs.
pub fn peek_navigate(id: &str, url: &str) {
    if !super::host::running() {
        return;
    }
    let landed = with_browser(id, |b| match b.main_frame() {
        Some(frame) => {
            frame.load_url(Some(&CefString::from(url)));
            true
        }
        None => false,
    });
    if std::env::var("EAVE_WEB_TRACE").is_ok() {
        eprintln!("[web] peek renavigate {id} -> {url} landed={landed:?}");
    }
    super::host::request_pump();
}

/// Create a windowless browser for a tab.
fn create(tab_id: &str, url: Option<&str>, incognito: bool) {
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
        windowless_frame_rate: if super::is_peek_id(tab_id) {
            PEEK_FRAME_RATE
        } else {
            FRAME_RATE
        },
        background_color: PAGE_BACKGROUND,
        ..Default::default()
    };

    if std::env::var("EAVE_WEB_TRACE").is_ok() {
        eprintln!("[web] creating browser for {tab_id} at {url:?}");
    }
    CREATING.with(|c| c.borrow_mut().insert(tab_id.to_string()));
    // A recreated browser has not loaded anything yet, whatever its
    // predecessor did.
    LOADED.with(|l| {
        l.borrow_mut().remove(tab_id);
    });
    if incognito {
        INCOGNITO_TABS.with(|s| {
            s.borrow_mut().insert(tab_id.to_string());
        });
    }
    let mut client = PageClient::new(tab_id.to_string());
    let start = CefString::from(url.unwrap_or("about:blank"));
    let mut ctx = if incognito {
        incognito_context()
    } else {
        None
    };
    browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&start),
        Some(&settings),
        None,
        ctx.as_mut(),
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

/// Cut the page's current selection — slice A4.
///
/// Only does anything in an editable field, and **Blink decides that**,
/// not eave: `Frame::cut` on a read-only selection is a no-op rather than
/// an error, which is the behaviour a chord wants. The alternative would
/// be for the chrome to guess editability from the last
/// `ContextMenuParams`, which is stale the moment the pointer moves.
pub fn cut_selection(tab_id: &str) {
    with_browser(tab_id, |b| {
        if let Some(frame) = b.main_frame() {
            frame.cut();
        }
    });
    super::host::request_pump();
}

/// Paste the system clipboard into the page — slice A4.
///
/// **`paste` and not `paste_and_match_style`.** Chrome's plain ⌘V keeps
/// the source's formatting and puts the stripped version on ⌥⇧⌘V; matching
/// style here would silently discard formatting on every paste into a rich
/// editor, which is a data loss the operator did not ask for and cannot
/// undo from the chrome.
///
/// Nothing is read from the clipboard by eave on this path. Blink takes it
/// directly, so an image or HTML on the pasteboard arrives as an image or
/// HTML — unlike the composer's ⌘V, which has to interpret the pasteboard
/// itself because a gpui text field can only be handed a string.
pub fn paste_into_page(tab_id: &str) {
    with_browser(tab_id, |b| {
        if let Some(frame) = b.main_frame() {
            frame.paste();
        }
    });
    super::host::request_pump();
}

/// Select the whole document, or the whole field if one has focus — A4.
///
/// Routed through the MAIN frame for `copy_selection`'s reason: which
/// element "all" means is Blink's question, and it resolves it from the
/// focused frame downward.
pub fn select_all(tab_id: &str) {
    with_browser(tab_id, |b| {
        if let Some(frame) = b.main_frame() {
            frame.select_all();
        }
    });
    super::host::request_pump();
}

/// Open DevTools for this tab's page, docked — the menu's "Inspect" row.
///
/// # Why this does NOT call `show_dev_tools`
/// It did, twice, and both shapes were wrong. A windowed `WindowInfo`
/// opens a separate native window (the operator rejected it). A windowless
/// one is REJECTED by CEF 151 itself — `chrome_browser_delegate.cc`,
/// branch 7922: *"Windowless rendering is not supported for this DevTools
/// window"*, after which it builds a Chrome-style native window regardless
/// (`CHECK(platform_delegate->IsChromeStyle())`). The association API
/// cannot produce a docked frontend in this CEF, full stop.
///
/// # What it does instead
/// The frontend is just a web app, bundled inside the Chromium framework
/// we already ship, that speaks the DevTools protocol over a websocket.
/// eave runs the protocol endpoint on localhost ([`host::DEVTOOLS_PORT`]),
/// asks it for this tab's target, and loads the frontend URL into an
/// ordinary windowless browser keyed `devtools:<tab>` — the identical OSR
/// pipeline the page rides, for the price of the id. Chrome's own dock is
/// this exact architecture plus window chrome eave doesn't have.
///
/// The inspect point is accepted but UNUSED for now: aiming the frontend
/// at a node needs the CDP channel (E9's surface). The row still opens the
/// dock; it just doesn't pre-select the element under the pointer.
pub fn show_devtools(tab_id: &str, _x: i32, _y: i32) {
    // **CEF must be running, and this guard is load-bearing in TESTS.**
    // Everything below ends in `CefString::from`, and a `CefString` built
    // in a process that never ran `CefInitialize` is a SIGSEGV that takes
    // the whole lib-test binary with it — no assertion, no test name.
    // Before the endpoint existed the early returns hid that; now the
    // lookup SUCCEEDS from a test process, because the developer's own
    // eave is usually running and its debugging port answers anyone on
    // loopback. The suite reached a live browser and then died on the
    // next line. (It is also a fair demonstration of what that port is.)
    if !super::host::running() {
        return;
    }
    // The frontend reaches its target over Chromium's debugging port, and
    // the port is off unless the operator asked for it. Refuse HERE, with
    // the other guards, and above all **before any browser is created**:
    // once the OSR surface exists it has to be reaped, and the reaper maps
    // a devtools id to its PARENT's snapshot row — a dock that never loads
    // is exactly the shape that gets missed there.
    if !super::host::devtools_port_enabled() {
        eprintln!(
            "[web] DevTools dock refused: the debugging port is off. \
             Relaunch with EAVE_DEVTOOLS_PORT=1."
        );
        return;
    }
    // No docking DevTools onto DevTools. Nothing in the product asks for
    // it now that the frontend keeps its own menus, but the id would be
    // `devtools:devtools:…` and the second frontend would inspect the
    // first, which is a state worth refusing rather than explaining.
    if super::devtools_parent(tab_id).is_some() {
        return;
    }
    let dt_id = super::devtools_id(tab_id);
    if has_browser(&dt_id) || CREATING.with(|c| c.borrow().contains(&dt_id)) {
        // Already docked, or on its way — a second Inspect is "make sure
        // it is open", nothing more.
        return;
    }
    let page_url = with_browser(tab_id, |b| {
        b.main_frame()
            .map(|f| CefString::from(&f.url()).to_string())
    })
    .flatten()
    .filter(|u| !u.is_empty());

    // **The lookup runs on ANOTHER THREAD, and that is the whole reason
    // the dock stayed black.** Chromium answers `/json` by posting to the
    // UI thread (`devtools_http_handler.cc`: `GetUIThreadTaskRunner()
    // ->PostTask(&OnJsonRequest)`), and this function IS the UI thread.
    // Asking here blocked the only thread that could answer, the read
    // timed out, and `show_devtools` gave up without a word. From a
    // separate process the identical call always succeeded, which is what
    // made it look like the endpoint was fine.
    //
    // The answer comes back through `run_queued_docks`, on the UI thread,
    // just after a pump — the same shape `favicon::run_queued` uses for
    // work that must not happen inside a CEF callback.
    CREATING.with(|c| c.borrow_mut().insert(dt_id.clone()));
    std::thread::spawn(move || {
        let found = devtools_frontend_url(page_url.as_deref());
        if let Ok(mut q) = PENDING_DOCKS.lock() {
            q.push((dt_id, found));
        }
        super::host::request_pump();
    });
}

/// Docks whose target lookup has come back, waiting for the UI thread.
///
/// `None` is a failed lookup, carried rather than dropped so the pane can
/// be taken down instead of standing empty over a browser that is never
/// coming.
static PENDING_DOCKS: std::sync::Mutex<Vec<(String, Option<String>)>> =
    std::sync::Mutex::new(Vec::new());

/// Create the browsers whose frontend URLs arrived since the last pump.
///
/// **UI thread, after the pump**, beside `favicon::run_queued` and for the
/// same reason: this creates browsers, and browser creation is not
/// something to do from a worker thread or from inside a CEF callback.
pub(super) fn run_queued_docks() {
    // Closes first, and BEFORE the early return below — the dock queue is
    // empty on almost every pump, so draining closes after it would mean
    // the frontend's ✕ only worked while another dock was being opened.
    let closing = match PENDING_DOCK_CLOSES.lock() {
        Ok(mut q) if !q.is_empty() => std::mem::take(&mut *q),
        _ => Vec::new(),
    };
    for dt_id in closing {
        if let Some(parent) = super::devtools_parent(&dt_id) {
            // The same call the page-menu row and ⌘⌥I make. The close path
            // itself stays untouched.
            super::close_devtools(parent);
        }
        // No page frame is coming to carry the pane's disappearance onto
        // the screen — the shell has to be told the chrome changed, the
        // same reason the context menu says so. See `note_chrome_change`.
        super::host::note_chrome_change();
    }

    let queued = match PENDING_DOCKS.lock() {
        Ok(mut q) if !q.is_empty() => std::mem::take(&mut *q),
        _ => return,
    };
    for (dt_id, frontend) in queued {
        let Some(frontend) = frontend else {
            // The lookup failed. Let go of the id so a later Inspect can
            // try again, and close the dock so no empty pane is left.
            CREATING.with(|c| {
                c.borrow_mut().remove(&dt_id);
            });
            super::devtools_note_closed(&dt_id);
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] devtools: no frontend url for {dt_id}");
            }
            continue;
        };
        if has_browser(&dt_id) {
            CREATING.with(|c| {
                c.borrow_mut().remove(&dt_id);
            });
            continue;
        }
        // Closed while the lookup was in flight. Creating it now would
        // reopen a dock the operator just shut — Close looking like it
        // did nothing, until they did it a second time.
        let still_wanted = super::devtools_parent(&dt_id).is_some_and(super::devtools_open);
        if !still_wanted {
            CREATING.with(|c| {
                c.borrow_mut().remove(&dt_id);
            });
            continue;
        }
        if std::env::var("EAVE_WEB_TRACE").is_ok() {
            eprintln!("[web] devtools: docking {dt_id} at {frontend}");
        }
        // From here it is `create` verbatim, for the devtools id.
        let mut window_info = WindowInfo::default().set_as_windowless(std::ptr::null_mut());
        window_info.shared_texture_enabled = 1;
        let v = super::osr::view(&dt_id);
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
        let mut client = PageClient::new(dt_id.clone());
        let start = CefString::from(frontend.as_str());
        browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&start),
            Some(&settings),
            None,
            None,
        );
        super::host::request_pump();
    }
}

/// The frontend URL for a tab's target, from the local protocol endpoint.
///
/// Matching is by the tab's CURRENT URL against the target list, falling
/// back to the first page target when nothing matches (one tab open is the
/// common case). Exact target identity needs the CDP channel (E9) —
/// `/json/list` does not know eave's tab ids.
fn devtools_frontend_url(page_url: Option<&str>) -> Option<String> {
    let body = devtools_targets_json()?;
    let targets: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let list = targets.as_array()?;
    let candidates: Vec<&serde_json::Value> = list
        .iter()
        .filter(|t| {
            t["type"].as_str() == Some("page")
                && t["url"]
                    .as_str()
                    .is_some_and(|u| !u.starts_with("devtools://"))
        })
        .collect();
    let pick = page_url
        .and_then(|u| {
            candidates
                .iter()
                .find(|t| t["url"].as_str() == Some(u))
                .copied()
        })
        .or_else(|| candidates.first().copied())?;
    // **Built from `webSocketDebuggerUrl`, NOT from `devtoolsFrontendUrl`,
    // and that is the whole of why the dock painted black.** The listing's
    // own frontend url points at Google's REMOTE copy
    // (`https://chrome-devtools-frontend.appspot.com/serve_rev/@<rev>/…`)
    // with `?ws=127.0.0.1:…` attached — and an `https` page may not open an
    // insecure websocket, so the frontend loaded and then could never reach
    // the target. Nothing painted, no error anywhere eave could see it.
    //
    // The same endpoint serves the BUNDLED frontend — the copy inside the
    // framework we already ship, version-matched to this Chromium — over
    // plain http, where a `ws://` to the same host is ordinary. That is
    // also the one that must be used on principle: a browser's devtools
    // should not be a fetch from a third-party host.
    let ws = pick["webSocketDebuggerUrl"].as_str()?.strip_prefix("ws://")?;
    // **`devtools_app.html` WITH `can_dock=true`.** Both halves were
    // wrong the other way first, and the second one twice.
    //
    // `inspector.html` is Chrome's FALLBACK entrypoint
    // (`devtools_window.cc`: `kFallbackFrontendURL`); `devtools_app.html`
    // is `kDefaultFrontendURL`, the one every docked window actually gets.
    //
    // **`can_dock` is not optional, because DevTools' own close button is
    // gated on it.** Read from the frontend this build serves
    // (`ui/legacy/legacy.js`, `DockController`):
    //
    //   if (!canDock) { this.#dockSide = "undocked";
    //                   this.closeButton.setVisible(false); return; }
    //   ...
    //   this.closeButton.setVisible(this.#dockSide !== "undocked");
    //
    // So the real ✕ exists only with `can_dock` AND an actual dock side.
    // There is no configuration with the button and without docking, and
    // no button eave appends by hand is ever in the right place — the
    // button is a registered toolbar ITEM, positioned by DevTools.
    //
    // The cost, which an earlier pass measured and blamed correctly:
    // `can_dock` loads `AdvancedApp`, whose root split reserves a pane for
    // the INSPECTED PAGE and asks the embedder to size it through
    // `setInspectedPageBounds` — a literal no-op on the stub host every
    // hosted frontend gets. Nothing sized it, so it stood empty: the 150 px
    // grey band.
    //
    // eave now fills that pane, which is what makes the flag affordable:
    // the frontend browser covers the whole well and eave paints its page
    // into the rect the placeholder reports. See `DOCK_SHIM_JS`.
    Some(format!(
        "http://127.0.0.1:{}/devtools/devtools_app.html?ws={}&can_dock=true",
        super::host::DEVTOOLS_PORT,
        ws
    ))
}

/// The one setting the docked frontend needs, set from inside its own page.
///
/// `screencast-enabled` defaults to **true**, and it is what draws the
/// page a second time as a picture in a phone-shaped frame beside the
/// panels. It lives in `localStorage` on the frontend's own origin, so
/// there is no way to seed it from out here: the page sets it and reloads
/// **once per profile**, after which the value persists and this is a
/// no-op. `getItem` immediately after `setItem` is what makes it exactly
/// once rather than a reload loop.
/// The sentinel the injected close prints. Matched exactly, and only on a
/// `devtools:` browser, so a page that logs the same string cannot shut
/// anyone's panel.
pub(super) const DOCK_CLOSE_SENTINEL: &str = "EAVE_DOCK_CLOSE";

/// The rect DevTools reserves for the inspected page, in CSS pixels of
/// the frontend's own viewport. Because the frontend browser covers the
/// whole well, those are the well's logical pixels, which is what makes
/// the mapping in `ui.rs` a straight copy rather than a conversion.
pub(super) const DOCK_BOUNDS_SENTINEL: &str = "EAVE_DOCK_BOUNDS";

/// Two shims, injected into the frontend on every load of a `devtools:`
/// browser.
///
/// **1. `screencast-enabled=false`.** Without `can_dock` the frontend runs
/// the screencast app, whose pane collapses only once this is off. It
/// lives in `localStorage` on the frontend's own origin, so nothing
/// outside the page can seed it: the page sets it and reloads once per
/// profile.
///
/// **2. The close button, appended to the frontend's own toolbar.**
///
/// The operator asked for Chrome's placement — in the panel, in the row
/// with the gear and the kebab — and eave cannot get there any other way.
/// Chrome's own ✕ exists only under `can_dock`, which is ruled out (it
/// makes the frontend hold a permanently empty pane waiting for an
/// embedder confirmation a hosted frontend can never send; dock report
/// §4). An eave-drawn ✕ floated over that corner would cover the kebab,
/// and an eave-drawn strip above the panel — the first thing tried — is
/// what the operator rejected as "outside of the panel".
///
/// So the button is a child of `.tabbed-pane-header`, appended after the
/// frontend's own controls. It inherits the toolbar's colour and metrics,
/// which is why it tracks the DevTools theme for free and why there is no
/// geometry for eave to get wrong. **This replaces the reserved
/// `paddingRight` strip**, which existed only to keep an eave overlay off
/// the gear and would now be a gap.
///
/// **This is a second load-bearing shim into a versioned Chromium bundle,
/// and that is a real cost, taken deliberately.** The dead-end report
/// argued against exactly this. What changes the answer is that the
/// operator has now twice asked for the in-panel placement, and the
/// alternatives that avoid injection cannot produce it. The dependency is
/// one selector — `.tabbed-pane-header`, which the padding shim already
/// relied on — and it degrades visibly rather than silently: if the
/// selector stops matching after a Chromium bump, the retry loop gives up
/// and the panel simply has no ✕, leaving ⌘⌥I and the page-menu row. It
/// cannot break Inspect, and it cannot close a dock nobody asked to close.
///
/// The `MutationObserver` is not belt-and-braces: the frontend rebuilds
/// that header when panels change, and a button appended once would
/// quietly vanish the first time the operator clicked Network.
/// **The toolbar is inside a SHADOW ROOT, which is why this walks.**
/// `TabbedPane` is constructed `{useShadowDom: !0}`, so
/// `document.querySelector('.tabbed-pane-header')` returns `null` no
/// matter how long it is retried. The padding shim that shipped in
/// `67515c6` used exactly that selector from `document` and has therefore
/// **never done anything** — a silent no-op nobody could see, since its
/// only effect would have been a gap. Verified against the frontend this
/// build actually serves (`GET /devtools/ui/legacy/legacy.js`): the class
/// is present, the header is `display:flex; flex:0 0 27px`, and the
/// gear/kebab arrive through a `<slot name="right">`, so a child appended
/// to the header lands after them — which is the placement asked for.
///
/// `__EAVE_SENTINEL__` is substituted rather than `format!`-interpolated
/// so the JavaScript can be read without every brace doubled.
const DOCK_SHIM_JS: &str = r#"
(function(){
  try{
    // Seeded once, then never fought again: if the operator later docks
    // left or bottom, `eave-dock-seeded` stops us forcing it back.
    var need=false;
    if(localStorage.getItem('screencast-enabled')!=='false'){
      localStorage.setItem('screencast-enabled','false'); need=true;
    }
    if(!localStorage.getItem('eave-dock-seeded')){
      localStorage.setItem('currentDockState','"right"');
      localStorage.setItem('eave-dock-seeded','1');
      need=true;
    }
    if(need){ location.reload(); return; }

    var deep=function(sel,root,depth){
      root=root||document; depth=depth||0;
      if(depth>12) return null;
      var el=root.querySelector(sel);
      if(el) return el;
      var all=root.querySelectorAll('*');
      for(var i=0;i<all.length;i++){
        var sr=all[i].shadowRoot;
        if(sr){ var r=deep(sel,sr,depth+1); if(r) return r; }
      }
      return null;
    };

    // The placeholder is the hole DevTools reserves for the inspected
    // page. `AdvancedApp` is on globalThis, which is the cheap route;
    // `.split-widget-main` is the fallback if that ever stops being
    // exported. NOTE the host object itself is NOT reachable from page
    // script, so overriding `setInspectedPageBounds` is not an option —
    // we read the rect off the element instead.
    var cachedPh=null;
    var placeholderEl=function(){
      if(cachedPh&&cachedPh.isConnected) return cachedPh;
      try{
        var A=globalThis.Emulation&&globalThis.Emulation.AdvancedApp;
        if(A&&A.instance){
          var app=A.instance();
          if(app&&app.inspectedPagePlaceholder&&app.inspectedPagePlaceholder.element){
            cachedPh=app.inspectedPagePlaceholder.element;
            return cachedPh;
          }
        }
      }catch(e){}
      cachedPh=deep('.split-widget-main');
      return cachedPh;
    };

    var lastB='';
    var report=function(){
      var el=placeholderEl();
      if(!el) return;
      var r=el.getBoundingClientRect();
      if(!r.width||!r.height) return;
      var m=Math.round(r.left)+','+Math.round(r.top)+','+
            Math.round(r.width)+','+Math.round(r.height);
      if(m===lastB) return;
      lastB=m;
      console.log('__EAVE_BOUNDS__ '+m);
    };

    // DevTools' OWN close button. Its Click handler is bound to the host
    // stub at construction, so the method cannot be overridden after the
    // fact — a DOM listener beside it is the version-stable way in. The
    // bound no-op still runs and does nothing, which is what it did
    // before.
    var closeEl=null;
    var wireClose=function(){
      if(closeEl&&closeEl.isConnected) return true;
      var b=deep('.close-devtools');
      if(!b) return false;
      closeEl=b;
      if(!b.getAttribute('data-eave-wired')){
        b.setAttribute('data-eave-wired','1');
        b.addEventListener('click',function(){ console.log('__EAVE_CLOSE__'); },true);
      }
      return true;
    };

    // Device mode arrives with can_dock, but eave does not apply the
    // emulation parameters to the page, so the frame would lie. Hidden
    // until it is real.
    var dmDone=false;
    var hideDeviceMode=function(){
      if(dmDone) return true;
      var d=deep('[jslog*="toggle-device-mode"]')||deep('[aria-label*="device toolbar"]');
      if(!d||!d.style) return false;
      d.style.display='none';
      dmDone=true;
      return true;
    };

    var tick=function(){ wireClose(); hideDeviceMode(); report(); };
    tick();
    setInterval(tick,250);
    if(window.ResizeObserver){
      try{
        var ro=new ResizeObserver(report);
        var el=placeholderEl();
        if(el) ro.observe(el);
      }catch(e){}
    }
    window.addEventListener('resize',report);
  }catch(e){}
})();
"#;

fn devtools_settle_frontend(frame: &mut Frame) {
    let shim = DOCK_SHIM_JS
        .replace("__EAVE_CLOSE__", DOCK_CLOSE_SENTINEL)
        .replace("__EAVE_BOUNDS__", DOCK_BOUNDS_SENTINEL);
    frame.execute_java_script(Some(&CefString::from(shim.as_str())), None, 0);
}

/// Docks whose frontend asked to be closed, drained on the UI thread by
/// [`run_queued_docks`].
///
/// **Queued rather than closed on the spot, deliberately.** The request
/// arrives inside `on_console_message` — a CEF callback, on Chromium's own
/// stack, for the very browser that would be destroyed. Closing a browser
/// from inside one of its own callbacks is the shape this process has
/// already been bitten by, and the close/resize dead end is explicit that
/// the close path is not to be improvised on. This puts the close exactly
/// where the *open* already happens.
static PENDING_DOCK_CLOSES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn queue_dock_close(dt_id: &str) {
    if let Ok(mut q) = PENDING_DOCK_CLOSES.lock() {
        if !q.iter().any(|x| x == dt_id) {
            q.push(dt_id.to_string());
        }
    }
    super::host::request_pump();
}

/// `GET /json/list` from the local endpoint. Blocking but bounded — it is
/// localhost, 300 ms to connect and 700 ms to read, and it runs once per
/// Inspect click, not per frame.
fn devtools_targets_json() -> Option<String> {
    devtools_targets_json_at(std::net::SocketAddr::from((
        [127, 0, 0, 1],
        super::host::DEVTOOLS_PORT,
    )))
}

fn devtools_targets_json_at(addr: std::net::SocketAddr) -> Option<String> {
    use std::io::{Read, Write};
    let mut s =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_millis(700)))
        .ok()?;
    // **The `Host` header must carry the PORT**, because Chromium builds
    // the URLs it hands back from it: with `Host: 127.0.0.1` every
    // `webSocketDebuggerUrl` came back as `ws://127.0.0.1/devtools/…`,
    // pointing the frontend at port 80. Measured both ways against the
    // live endpoint before this line was written.
    s.write_all(
        format!("GET /json/list HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .ok()?;

    // **Read to `Content-Length`, never to EOF.** The endpoint ignores
    // `Connection: close` and holds the socket open, so `read_to_end`
    // ends in `Err(TimedOut)` — and `.ok()?` on that discards a
    // COMPLETE response already sitting in the buffer. That is what made
    // the dock silently never open: the target lookup failed every time,
    // `show_devtools` gave up, and no browser was ever created.
    //
    // `curl` could not show it, because curl stops at `Content-Length`
    // like this now does. The bug only existed in the hand-rolled read.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if let Some(head_end) = find(&raw, b"\r\n\r\n") {
                    match content_length(&raw[..head_end]) {
                        // Whole body in hand — stop rather than wait out
                        // the read timeout on a socket that stays open.
                        Some(len) if raw.len() - (head_end + 4) >= len => break,
                        Some(_) => {}
                        // No length header: fall back to waiting for the
                        // close or the timeout, and keep what arrives.
                        None => {}
                    }
                }
            }
            // A timeout here is an answer, not a failure: whatever
            // arrived is what there is.
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    text.split_once("\r\n\r\n").map(|(_, body)| body.to_string())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Close a tab's DevTools browser, if it has one. The state half lives in
/// `super::close_devtools`; the map entry disappears in the devtools
/// browser's own `on_before_close`, like any browser's. Also drops an
/// in-flight lookup so a Close that beats the worker cannot reopen the
/// dock when the URL arrives.
pub fn close_devtools(tab_id: &str) {
    let dt = super::devtools_id(tab_id);
    CREATING.with(|c| {
        c.borrow_mut().remove(&dt);
    });
    if let Ok(mut q) = PENDING_DOCKS.lock() {
        q.retain(|(id, _)| id != &dt);
    }
    if has_browser(&dt) {
        close_browser_for(&dt);
    }
    super::host::request_pump();
}

/// Close every browser. Runs before `CefShutdown`, which blocks forever if
/// a windowless browser is still live.
pub fn close_all() {
    let ids: Vec<String> = BROWSERS.with(|b| b.borrow().keys().cloned().collect());
    for id in ids {
        close_browser_for(&id);
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
/// (That ~322 MB was one tab's total. E3 measured the same set with ten
/// tabs and with none — 0.90 GB and 0.32 GB — so most of it is a floor
/// that does not multiply. The undercount this paragraph is about is
/// unaffected either way; only the sense of scale changes.)
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
        /// `target="_blank"`, `window.open`, and middle-click.
        ///
        /// **Returning 1 cancels Chromium's popup and is the only correct
        /// answer here.** Letting CEF proceed asks it to create a browser
        /// with a `WindowInfo` we did not fill in — under off-screen
        /// rendering that is a browser with no render handler and no
        /// window, i.e. a page that exists and can never be seen. Before
        /// this, every `target="_blank"` link in eave was a dead link: the
        /// click was consumed, nothing opened, and nothing said why.
        ///
        /// So the popup is refused and re-asked as an eave tab. That is a
        /// REQUEST and not a report — `PageRequest` rather than `PageFact`
        /// — because opening a tab is the engine's decision and the page
        /// engine is a client like any other (module docs).
        ///
        /// The dispositions are not all the same thing. A background tab
        /// (⌘-click) must not steal focus; `SAVE_TO_DISK` is a download and
        /// not a tab at all, and is left to the download handler rather
        /// than being turned into a tab that immediately closes itself.
        #[allow(clippy::too_many_arguments)]
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(|u| u.to_string()).unwrap_or_default();
            if trace() {
                eprintln!("[web] on_before_popup {target_disposition:?} url={url:?}");
            }
            if url.is_empty() {
                // `window.open()` with no url — a page about to write into
                // the new window through the returned handle. eave has
                // nowhere to put that handle, so refusing is the honest
                // answer; opening a blank tab that never receives its
                // content would be worse.
                return 1;
            }
            let Some((activate, incognito)) =
                tab_disposition(target_disposition, tab_is_incognito(&self.tab_id))
            else {
                // Not a tab. Cancel the WINDOW — there is nowhere to put
                // one — and leave whatever else is coming to see it.
                return 1;
            };
            request(PageRequest::OpenTab {
                url,
                opener: self.tab_id.clone(),
                activate,
                incognito,
            });
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if trace() {
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
            CREATING.with(|c| c.borrow_mut().remove(&self.tab_id));
            let evicted = BROWSERS.with(|b| b.borrow_mut().insert(self.tab_id.clone(), browser));
            drop(evicted);
            // G5: nothing to re-assert here any more. E1 pushed eave's
            // remembered zoom onto each new browser because zoom was
            // thought to belong to the tab; Chromium has been storing it
            // per HOST and restoring it itself all along, so pushing a
            // tab-keyed value in would OVERRIDE the host's own. The
            // reconciliation is now a read, and it happens once the
            // navigation has committed — see `on_load_end`.
            // This tab is resident again: start its idle clock and clear
            // any evicted mark. Outside the borrow above, deliberately.
            super::evict::note_created(&self.tab_id);
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            // Removed, then dropped outside the borrow — see `with_browser`.
            let gone = BROWSERS.with(|b| b.borrow_mut().remove(&self.tab_id));
            drop(gone);
            INCOGNITO_TABS.with(|s| {
                s.borrow_mut().remove(&self.tab_id);
            });
            if INCOGNITO_TABS.with(|s| s.borrow().is_empty()) {
                let ctx = INCOGNITO_CTX.with(|c| c.borrow_mut().take());
                drop(ctx);
            }
            super::osr::forget(&self.tab_id);
            super::page::forget_resize_state(&self.tab_id);
            forget_chrome(&self.tab_id);
            // A held dialog callback is a stopped renderer. This browser is
            // going away and so is the page that would have been unblocked,
            // but the callback is still answered rather than dropped —
            // releasing one unanswered is the difference between a clean
            // teardown and a hang.
            super::dialogs::forget(&self.tab_id);
            super::nav_policy::forget(&self.tab_id);
            // Slice E1. A find that outlived its page would state a count
            // about a document that is gone; the zoom is dropped for the
            // same reason `close_browser` is reached at all, which is that
            // the tab itself has left the strip.
            super::forget_page(&self.tab_id);
            // A no-op for page browsers. For a devtools browser — closed
            // by the frontend's own ✕, its parent's close, or an eviction
            // — this clears the dock state, so the pane does not stand
            // empty over a browser that no longer exists.
            super::devtools_note_closed(&self.tab_id);
            // If this close was an eviction, the strip may want the tab
            // back already. Sets a flag; nothing is created on this stack.
            super::evict::note_closed(&self.tab_id);
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
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let (can_go_back, can_go_forward) = nav_flags(browser);
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
            // Seed the address dedup. The answer is ignored on purpose: a
            // load is worth reporting whether or not the address moved
            // (a reload does not move it), and it is `on_address_change`
            // that has to be quiet about the echo.
            note_url(&self.tab_id, &url);
            // A4: a new main-frame document has nothing selected in it.
            // Chromium fires `on_text_selection_changed` on a cross-document
            // navigation, but not before the chrome has already drawn a
            // frame of the new page — and on some same-document navigations
            // not at all. Clearing here makes the answer only ever stale in
            // the safe direction: offering nothing when there is something
            // is a missing affordance; offering the PREVIOUS page's words
            // is a wrong one.
            super::selection::note_navigated(&self.tab_id);
            report(&self.tab_id, PageFact::Committed { url, title: None });
            report(
                &self.tab_id,
                PageFact::Load {
                    load: LoadState::Loading,
                    can_go_back,
                    can_go_forward,
                },
            );
        }

        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            let (can_go_back, can_go_forward) = nav_flags(browser);
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_load_end status={_http_status_code}");
            }
            let Some(frame) = frame else { return };
            if frame.is_main() != 1 {
                return;
            }
            // The docked DevTools frontend, settling its own settings. A
            // no-op for every ordinary page, and for the frontend after
            // the first time. See `devtools_settle_frontend`.
            if super::devtools_parent(&self.tab_id).is_some() {
                devtools_settle_frontend(frame);
            }
            set_chrome(&self.tab_id, |c| c.progress = 1.0);
            // G5: the commit is the moment Chromium has applied this
            // host's stored zoom, so it is the moment eave can state it.
            // Earlier — in `on_after_created` — the level is still the
            // default and the header would say 100% over a zoomed page,
            // which is the bug this replaced.
            super::zoom_sync(&self.tab_id);
            // The load is a fact other modules key on: peek holds its
            // first CDP message until this, because a message sent in
            // the CREATION GAP attaches the DevTools agent to the
            // initial empty document — measured: `EAVE_CDP_EVAL` read
            // `about:blank` off a browser whose feed had already
            // committed with status 200.
            LOADED.with(|l| {
                l.borrow_mut().insert(self.tab_id.clone());
            });
            // E9's instrument. `EAVE_CDP_EVAL=<expr>` drives this page once
            // per load and prints its own answer; a no-op with the var
            // unset, which is every ordinary run.
            super::inspector::probe_on_load(&self.tab_id);
            // A loaded page means some browser is usable, which is what a
            // bookmark's favicon fetch needs and could not assume at boot.
            super::favicon::note_page_loaded();
            report(
                &self.tab_id,
                PageFact::Load {
                    load: LoadState::Complete,
                    can_go_back,
                    can_go_forward,
                },
            );
        }

        fn on_load_error(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] on_load_error {:?}", error_text.map(|t| t.to_string()));
            }
            if frame.map(|f| f.is_main()) != Some(1) {
                return;
            }
            // E2's fallback. An upgrade eave made that could not connect
            // goes back to http rather than showing the operator a
            // connection error for a site that was working before.
            //
            // `ERR_ABORTED` is excluded inside `nav_policy::fallback`, and it
            // has to be: an upgrade is performed BY CANCELLING the http
            // navigation, so an abort arrives on every upgrade eave makes.
            let why = if error_code == Errorcode::ABORTED {
                super::nav_policy::Failure::Aborted
            } else {
                super::nav_policy::Failure::Unreachable
            };
            let failed = failed_url.map(|u| u.to_string()).unwrap_or_default();
            if let Some(http_url) = super::nav_policy::fallback(&self.tab_id, &failed, why) {
                if trace() {
                    eprintln!("[web] policy: fell back {failed} -> {http_url}");
                }
                request(super::PageRequest::Navigate {
                    tab_id: self.tab_id.clone(),
                    url: http_url,
                });
                // Not reported as a failure: the tab is not failing, it is
                // taking the other route. Reporting `LoadState::Failed`
                // here would flash an error state that the very next
                // navigation replaces.
                return;
            }
            // An abort is not a failure, and there was no upgrade to fall
            // back to — so the page that is already on screen is the right
            // one. `nav_policy::reports_failure` carries the argument; it
            // is the same one the early return above makes for the upgrade
            // branch, which is the only branch that used to get it.
            if !super::nav_policy::reports_failure(why) {
                // Progress still has to land, or the address bar's loading
                // wash never clears: `on_load_end` does not follow a load
                // that was cancelled before it committed.
                set_chrome(&self.tab_id, |c| c.progress = 1.0);
                let (can_go_back, can_go_forward) = nav_flags(browser);
                report(
                    &self.tab_id,
                    PageFact::Load {
                        load: LoadState::Complete,
                        can_go_back,
                        can_go_forward,
                    },
                );
                return;
            }
            // The engine's own words, rendered verbatim — `LoadState::Failed`
            // exists to carry them rather than to be re-worded by eave.
            let reason = error_text.map(|t| t.to_string()).unwrap_or_default();
            let (can_go_back, can_go_forward) = nav_flags(browser);
            report(
                &self.tab_id,
                PageFact::Load {
                    load: LoadState::Failed { reason },
                    can_go_back,
                    can_go_forward,
                },
            );
        }
    }
}

/// A download in flight, as the shell needs to see it.
///
/// App-local for the same reason [`PageChrome`] is: a byte count that
/// changes many times a second is not something to journal or broadcast.
/// What SHOULD outlive the app — "this file was downloaded from this url" —
/// is a `downloads.rs` in `eave-core` beside `history.rs`, and that is a
/// slice of its own (spec 06-browser D4). This is the half that makes a
/// download link do something instead of nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub id: u32,
    pub url: String,
    pub file_name: String,
    /// Where it is being written. Empty until Chromium settles the path.
    pub path: String,
    pub received: i64,
    pub total: i64,
    pub percent: i32,
    pub done: bool,
    pub cancelled: bool,
}

thread_local! {
    /// Downloads by CEF's own id, newest state wins.
    static DOWNLOADS: std::cell::RefCell<HashMap<u32, Download>> =
        std::cell::RefCell::new(HashMap::new());
    /// The live callback for each in-flight download — **the only handle
    /// that can stop one**.
    ///
    /// CEF hands a fresh `DownloadItemCallback` to every
    /// `on_download_updated`, and it is valid for that call only unless it
    /// is retained. Dropping it is what made cancel unbuildable before:
    /// the item id was known, and nothing in the process could act on it.
    /// Same shape as `dialogs`' held ask callbacks — an ask is data, and
    /// the reply is a CEF callback someone has to be holding.
    static CANCELS: std::cell::RefCell<HashMap<u32, DownloadItemCallback>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Every download this run knows about, newest first.
pub fn downloads() -> Vec<Download> {
    let mut all: Vec<Download> = DOWNLOADS.with(|d| d.borrow().values().cloned().collect());
    all.sort_by_key(|d| std::cmp::Reverse(d.id));
    all
}

/// Stop a download in flight.
///
/// A no-op for an id that has finished or was never live — a person can
/// click cancel on a row that completed while the menu was open, and that
/// is not an error.
pub fn cancel_download(id: u32) {
    // Cloned out before the call, never called under the borrow: CEF runs
    // handlers on our stack under an external pump, and `on_download_updated`
    // fires from inside `cancel()` wanting this same map.
    let callback = CANCELS.with(|c| c.borrow().get(&id).cloned());
    let Some(callback) = callback else { return };
    if trace() {
        eprintln!("[web] download id={id} cancel requested");
    }
    callback.cancel();
    super::host::request_pump();
}

thread_local! {
    static DOWNLOAD_LOG: std::cell::RefCell<Option<eave_core::browser::log::EventLog<eave_core::browser::downloads::DownloadEvent>>> =
        const { std::cell::RefCell::new(None) };
}

/// Load finished downloads from `<data_dir>/browser/downloads.jsonl`.
pub fn restore_downloads(data_dir: &std::path::Path) {
    use eave_core::browser::downloads::Downloads;
    use eave_core::browser::log::EventLog;
    let path = data_dir.join("browser").join("downloads.jsonl");
    let (log, events, report) = match EventLog::open(&path, "downloads") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("eave: downloads log: {e}");
            return;
        }
    };
    if let Some(line) = report.stated() {
        eprintln!("eave: downloads: {line}");
    }
    let mut fold = Downloads::new();
    for e in &events {
        fold.apply(e);
    }
    DOWNLOADS.with(|d| {
        let mut map = d.borrow_mut();
        for (i, item) in fold.items().iter().enumerate() {
            let id = 1_000_000 + i as u32;
            map.entry(id).or_insert(Download {
                id,
                url: item.url.clone(),
                file_name: item.file_name.clone(),
                path: item.path.clone(),
                received: item.bytes,
                total: item.bytes,
                percent: 100,
                done: true,
                cancelled: false,
            });
        }
    });
    DOWNLOAD_LOG.with(|l| *l.borrow_mut() = Some(log));
}

fn persist_finished(d: &Download) {
    use eave_core::browser::downloads::DownloadEvent;
    if d.path.is_empty() {
        return;
    }
    DOWNLOAD_LOG.with(|l| {
        if let Some(log) = l.borrow_mut().as_mut() {
            let _ = log.append(&DownloadEvent::Finished {
                url: d.url.clone(),
                file_name: d.file_name.clone(),
                path: d.path.clone(),
                bytes: d.received,
                // When it finished — the panel's "when". Was a literal 0,
                // so every restored row read as the epoch (walked
                // 2026-09-03).
                ts_ms: super::host::now_ms(),
            });
            let _ = log.flush();
        }
    });
}

wrap_pdf_print_callback! {
    struct SavePdfCallback {
        path: String,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, path: Option<&CefString>, ok: ::std::os::raw::c_int) {
            if super::host::traced() {
                let p = path.map(|p| p.to_string()).unwrap_or_else(|| self.path.clone());
                eprintln!("[web] print_to_pdf ok={ok} path={p}");
            }
        }
    }
}

/// Save the current page as a PDF in `~/Downloads`. No preview: Alloy
/// does not have print preview.
pub fn print_to_pdf(tab_id: &str) {
    let path = with_browser(tab_id, |b| {
        let Some(host) = b.host() else { return None };
        let url = last_url(tab_id);
        let name = url
            .as_deref()
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|u| u.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or("page");
        let stem = name.trim_end_matches(".pdf");
        let dest = download_dir().join(format!("{stem}.pdf"));
        let dest_s = dest.to_string_lossy().into_owned();
        let settings = PdfPrintSettings {
            print_background: 1,
            scale: 1.0,
            paper_width: 8.5,
            paper_height: 11.0,
            ..PdfPrintSettings::default()
        };
        let mut cb = SavePdfCallback::new(dest_s.clone());
        host.print_to_pdf(
            Some(&CefString::from(dest_s.as_str())),
            Some(&settings),
            Some(&mut cb),
        );
        Some(dest_s)
    })
    .flatten();
    if trace() {
        match path {
            Some(p) => eprintln!("[web] print_to_pdf asked -> {p}"),
            None => eprintln!("[web] print_to_pdf: no browser"),
        }
    }
    super::host::request_pump();
}

/// Where downloads go.
///
/// `~/Downloads` when it exists, the home directory when it does not, and
/// the temp dir if even that is unreadable. **Chromium is told the
/// directory and left to pick the final name**, which is what makes
/// `report.pdf` land beside an existing `report.pdf` as `report (1).pdf`
/// rather than silently replacing someone's file. Writing our own
/// uniquifier here would be re-implementing that badly.
fn download_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let downloads = std::path::Path::new(&home).join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return std::path::PathBuf::from(home);
    }
    std::env::temp_dir()
}

wrap_download_handler! {
    struct PageDownloadHandler {
        tab_id: String,
    }

    impl DownloadHandler {
        /// Whether a download may begin at all.
        ///
        /// Always yes. The place to refuse a download is a policy eave does
        /// not have yet (spec 06-browser E1); refusing here silently would
        /// be the same do-nothing behaviour this handler exists to remove.
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> i32 {
            1
        }

        /// A download started. **Returning without calling `cont` cancels
        /// it**, which is exactly what used to happen: with no download
        /// handler at all, clicking a download link did nothing, silently,
        /// with no error anywhere. That is the single worst kind of missing
        /// feature — indistinguishable from a broken page.
        ///
        /// `show_dialog = 0`: eave has no file-save dialog surface yet
        /// (spec 06-browser D6), and asking CEF to show one it cannot draw
        /// off-screen would hang the download on a prompt nobody can see.
        /// Downloading straight to `~/Downloads` is what every browser does
        /// by default anyway; the "ask where to save" setting is the
        /// feature that needs the surface, not this.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> i32 {
            let name = suggested_name
                .map(|n| n.to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "download".to_string());
            let target = download_dir().join(&name);
            let url = download_item
                .as_ref()
                .map(|d| CefString::from(&d.url()).to_string())
                .unwrap_or_default();
            let id = download_item.as_ref().map(|d| d.id()).unwrap_or(0);
            if trace() {
                eprintln!("[web] download starting id={id} name={name:?} -> {}", target.display());
            }
            DOWNLOADS.with(|d| {
                d.borrow_mut().insert(
                    id,
                    Download {
                        id,
                        url,
                        file_name: name,
                        path: target.to_string_lossy().to_string(),
                        received: 0,
                        total: 0,
                        percent: 0,
                        done: false,
                        cancelled: false,
                    },
                )
            });
            // CEF's contract (walked 2026-09-03, `download_manager_delegate_impl.cc:446`
            // logs an ERROR otherwise): return true when the callback was
            // executed here, false when it is held for later.
            match callback {
                Some(callback) => {
                    callback.cont(Some(&CefString::from(&*target.to_string_lossy())), 0);
                    1
                }
                None => 0,
            }
        }

        /// Progress, completion and cancellation.
        ///
        /// Called often — several times a second on a live transfer — so
        /// this only bumps the redraw gate when something a person could
        /// SEE actually changed. A byte count that moved but left the
        /// percentage alone is not a repaint.
        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            _callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let id = item.id();
            // Retain the callback while the transfer is live. Cloning a CEF
            // handle is a retain, not a copy of the download.
            if let Some(callback) = _callback {
                CANCELS.with(|c| {
                    c.borrow_mut().insert(id, callback.clone());
                });
            }
            let path = CefString::from(&item.full_path()).to_string();
            // `suggested_file_name` is only filled on the item CEF hands to
            // `on_before_download`; on updates it is empty, and the journal
            // wrote `"file_name":""` for every finished download (walked
            // 2026-09-03). The name is what `on_before_download` stored, or
            // the path's own last segment.
            let file_name = {
                let suggested = CefString::from(&item.suggested_file_name()).to_string();
                if !suggested.is_empty() {
                    suggested
                } else {
                    DOWNLOADS
                        .with(|d| d.borrow().get(&id).map(|p| p.file_name.clone()))
                        .filter(|n| !n.is_empty())
                        .or_else(|| {
                            std::path::Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                        })
                        .unwrap_or_default()
                }
            };
            let next = Download {
                id,
                url: CefString::from(&item.url()).to_string(),
                file_name,
                path,
                received: item.received_bytes(),
                total: item.total_bytes(),
                percent: item.percent_complete(),
                done: item.is_complete() == 1,
                cancelled: item.is_canceled() == 1,
            };
            // G4: Gatekeeper reacts to one extended attribute and nothing
            // else, and without it eave hands over an unsigned binary with
            // LESS friction than every other browser on the machine.
            //
            // **As soon as the file exists, not when it finishes.** Marking
            // at completion leaves a window in which a fully-written
            // executable sits in ~/Downloads unquarantined; Chromium marks
            // the partial file for exactly this reason. `mark` is
            // XATTR_CREATE, so it returns true once per path and can never
            // overwrite or clear a quarantine — calling it on every update
            // is one refused syscall, which is nothing beside the bytes
            // being written.
            if !next.path.is_empty()
                && super::quarantine::mark(std::path::Path::new(&next.path))
                && trace()
            {
                eprintln!("[web] download id={id} quarantined at {}", next.path);
            }
            let changed = DOWNLOADS.with(|d| {
                let mut map = d.borrow_mut();
                match map.get(&id) {
                    Some(prev) if prev == &next => false,
                    _ => {
                        map.insert(id, next.clone());
                        true
                    }
                }
            });
            if !changed {
                return;
            }
            if trace() && (next.done || next.cancelled || next.percent % 25 == 0) {
                eprintln!(
                    "[web] download id={id} {}% {}/{} done={} cancelled={}",
                    next.percent, next.received, next.total, next.done, next.cancelled
                );
            }
            // Finished or stopped: the callback can do nothing more, and
            // holding it would keep a CEF object alive for the life of the
            // run for every file a person ever downloaded.
            if next.done || next.cancelled {
                CANCELS.with(|c| {
                    c.borrow_mut().remove(&id);
                });
            }
            if next.done && !next.cancelled {
                persist_finished(&next);
            }
            // A download is chrome, not page pixels: it changes something
            // eave draws with no page frame to carry the change, which is
            // exactly what the chrome revision counter is for.
            bump_chrome_revision();
        }
    }
}

/// What a [`WindowOpenDisposition`] means for a browser whose windows are
/// tabs.
///
/// `Some(activate)` = open a page tab, focused or not. `None` = **not ours**
/// — leave it to whatever else is going to see it.
///
/// One function, two callers: `on_before_popup` (a `target="_blank"`, a
/// `window.open`) and `on_open_urlfrom_tab` (a middle-click, a ⌘-click).
/// Chromium routes those down different paths and hands eave the same
/// vocabulary at the end of each, so the risk worth removing is the two
/// sites disagreeing about what `NEW_BACKGROUND_TAB` means.
/// `Some((activate, incognito))` = open a page tab. `None` = not ours.
///
/// A link opened from an already-private tab stays private: putting it
/// in the ordinary profile would be answering a request for privacy
/// with a silent no, which is the defect this used to have for
/// `OFF_THE_RECORD`.
fn tab_disposition(
    disposition: WindowOpenDisposition,
    opener_incognito: bool,
) -> Option<(bool, bool)> {
    match disposition {
        // ⌘-click, middle-click, `target="_blank" rel=noopener` from a
        // background context. Not activated by default — that is the whole
        // point of the gesture: keep this, I am still reading. Slice S1
        // makes the exception a choice rather than a hardcode: the Tabs
        // pane's "switch to it immediately" flips this arm and only this
        // arm — a FOREGROUND disposition was already an ask to switch.
        WindowOpenDisposition::NEW_BACKGROUND_TAB => Some((
            crate::settings::switch_to_background_tabs(),
            opener_incognito,
        )),
        WindowOpenDisposition::NEW_FOREGROUND_TAB => Some((true, opener_incognito)),
        // eave is one window and a page is a tab in it (the C3 ruling), so
        // "new window" and "new popup" are tabs too. A page cannot make
        // eave grow a second window by asking.
        WindowOpenDisposition::NEW_WINDOW | WindowOpenDisposition::NEW_POPUP => {
            Some((true, opener_incognito))
        }
        // A download wearing a navigation's clothes. Not a tab — the
        // download handler is what should see it, and turning it into a
        // tab would open a page that closes itself.
        WindowOpenDisposition::SAVE_TO_DISK => None,
        WindowOpenDisposition::OFF_THE_RECORD => Some((true, true)),
        // An ordinary navigation, or Chromium telling us to do nothing.
        _ => None,
    }
}

wrap_request_handler! {
    struct PageRequestHandler {
        tab_id: String,
    }

    impl RequestHandler {
        /// A link the operator asked to open *somewhere else* (G3).
        ///
        /// **This is where a middle-click lands**, and until it was filled
        /// a middle-click on a page did nothing at all. CEF's own words for
        /// this callback name the case: navigation "that might open in a
        /// special way (e.g. links clicked via middle-click or
        /// ctrl + left-click)". It is a different path from
        /// `on_before_popup` — that one is `window.open` and
        /// `target="_blank"` — and an embedder that fills only the popup
        /// half gets a browser where `target="_blank"` works and the
        /// muscle-memory gesture silently does not.
        ///
        /// Returning 1 means **eave handled it**, which cancels the
        /// navigation CEF was about to attempt. That is right: eave has no
        /// second window to open one in, and the tab it does open is asked
        /// for through the sink rather than issued here — the borrow rule
        /// in the module docs.
        ///
        /// Returning 0 hands it back, which is what a disposition eave has
        /// no tab for wants: a download, or a request for a private context
        /// eave does not have.
        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = target_url.map(|u| u.to_string()).unwrap_or_default();
            if trace() {
                eprintln!(
                    "[web] on_open_urlfrom_tab {target_disposition:?} gesture={user_gesture} \
                     url={url:?}"
                );
            }
            if url.is_empty() {
                return 0;
            }
            let Some((activate, incognito)) =
                tab_disposition(target_disposition, tab_is_incognito(&self.tab_id))
            else {
                return 0;
            };
            request(PageRequest::OpenTab {
                url,
                opener: self.tab_id.clone(),
                activate,
                incognito,
            });
            1
        }

        /// A site asked for a TLS CLIENT certificate — and answering is
        /// not optional, because the fall-through is a SEGFAULT.
        ///
        /// With no handler here, CEF falls back to
        /// `ChromeContentBrowserClient::SelectClientCertificate` — the
        /// Chrome certificate PICKER, which under alloy/OSR has none of
        /// its tab UI and dies dereferencing null. That is how signing in
        /// to Google took the whole app down: `accounts.google.com` is
        /// one of the few sites that sends a TLS CertificateRequest, its
        /// `signin/challenge/*` pages tripped it on load, and the tab
        /// journal restored the page and crash-looped. Measured 4×
        /// 2026-08-27 (byte-identical frames); reproduced headlessly with
        /// an auto-filled identifier page; CEF #3972 is the same shape.
        /// The first TWO attributions — WebAuthn's missing entitlement,
        /// then `navigator.credentials` — were wrong: a password form and
        /// the credentials API both load clean; the TLS handshake is the
        /// trigger. See docs/reports/bookmark-peek.md §walk.
        ///
        /// Returning 1 without calling `Select` continues WITHOUT a
        /// client certificate — the callback's destructor does exactly
        /// that — which is what a consumer browser with no cert to offer
        /// wants, and what Chrome itself does when you have none
        /// installed. A cert-picker UI (enterprise) would be its own
        /// slice; today the honest answer is "none".
        fn on_select_client_certificate(
            &self,
            _browser: Option<&mut Browser>,
            _is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            _port: ::std::os::raw::c_int,
            _certificates: Option<&[Option<X509Certificate>]>,
            _callback: Option<&mut SelectClientCertificateCallback>,
        ) -> ::std::os::raw::c_int {
            if trace() {
                let host = host.map(|h| h.to_string()).unwrap_or_default();
                eprintln!(
                    "[web] client certificate requested by {host} — continuing without one"
                );
            }
            1
        }

        /// Decide what a navigation is allowed to do, before it starts.
        ///
        /// **This method was empty, and an empty `on_before_browse` is not
        /// "allow everything" — it is three abilities the browser does not
        /// have.** A `mailto:` link inside an off-screen browser resolves
        /// to no handler and does nothing at all; a tracked link carries
        /// its trackers; an unknown scheme is whatever Chromium decides.
        ///
        /// The decision itself is in [`super::policy`], ungated and
        /// tested against Brave's own cases. All that happens here is
        /// turning an answer into a return code.
        ///
        /// # Returning 1 means "cancel", and cancelling is how a rewrite
        /// works
        /// CEF gives no way to *edit* the URL from this callback, so a
        /// rewrite is a cancel plus a fresh navigation. The fresh one is
        /// asked for through the sink rather than issued here — see
        /// [`super::PageRequest::Navigate`] for why re-entering Chromium
        /// from inside its own callback is the thing this codebase has
        /// already been bitten by twice.
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            // Named `incoming`, not `request`: this module has a free
            // `request()` that posts to the sink, and a parameter of that
            // name silently shadows it.
            incoming: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(req) = incoming else {
                return 0;
            };
            let is_main_frame = frame.map(|f| f.is_main()) == Some(1);
            // `url()` hands back a userfree string, which has no `Display`
            // of its own — the utf16 view does.
            let url = CefStringUtf16::from(&req.url()).to_string();
            let method = CefStringUtf16::from(&req.method()).to_string();
            // Where the tab is now IS the initiator of a top-level
            // navigation, and `LAST_URL` already tracks it per tab for the
            // address dedup.
            let from = last_url(&self.tab_id);
            // E2 consults what this session has already learned: a host
            // that failed TLS once is not tried again.
            let allow_upgrade = super::nav_policy::host(&url)
                .map(|h| super::nav_policy::may_upgrade(&h))
                .unwrap_or(false);
            let decision = super::nav_policy::decide(super::nav_policy::Nav {
                url: &url,
                from: from.as_deref(),
                method: &method,
                is_redirect: is_redirect != 0,
                is_main_frame,
                allow_upgrade,
            });
            match decision {
                super::nav_policy::Decision::Allow => 0,
                super::nav_policy::Decision::Rewrite(clean) => {
                    if trace() {
                        eprintln!("[web] policy: rewrote {url} -> {clean}");
                    }
                    request(super::PageRequest::Navigate {
                        tab_id: self.tab_id.clone(),
                        url: clean,
                    });
                    1
                }
                super::nav_policy::Decision::Upgrade { to, fallback } => {
                    if trace() {
                        eprintln!("[web] policy: upgraded {url} -> {to}");
                    }
                    // Recorded BEFORE the navigation is asked for. The
                    // failure that needs this record can arrive as soon as
                    // the navigation starts.
                    super::nav_policy::note_upgrade(&self.tab_id, &to, &fallback);
                    request(super::PageRequest::Navigate {
                        tab_id: self.tab_id.clone(),
                        url: to,
                    });
                    1
                }
                super::nav_policy::Decision::External(url) => {
                    if trace() {
                        eprintln!("[web] policy: external {url}");
                    }
                    request(super::PageRequest::External { url });
                    1
                }
                super::nav_policy::Decision::Block => {
                    if trace() {
                        eprintln!("[web] policy: blocked {url}");
                    }
                    1
                }
                super::nav_policy::Decision::AskExternal(url) => {
                    if trace() {
                        eprintln!("[web] policy: ask to open {url}");
                    }
                    let scheme = super::nav_policy::scheme(&url).unwrap_or_default();
                    super::ask::note_raised(super::ask::Pending {
                        tab_id: self.tab_id.clone(),
                        origin: from.clone().unwrap_or_else(|| url.clone()),
                        ask: super::ask::Ask::OpenScheme { scheme, url },
                    });
                    bump_chrome_revision();
                    1
                }
            }
        }

        /// The renderer for this tab died.
        ///
        /// **This is the worst failure mode eave had, because it produced no
        /// failure.** The last frame stays in `PAGES` and keeps being
        /// painted, so a crashed tab is a photograph of the page it used to
        /// be: it looks alive, scrolls nothing, and answers no click. The
        /// operator's only evidence is that the web stopped working.
        ///
        /// Reported as `LoadState::Failed`, reusing the surface
        /// `on_load_error` already feeds rather than inventing a state the
        /// strip would have to learn. A dead renderer IS a failed load —
        /// the distinction the operator cares about is *why*, and that
        /// rides in the reason string.
        ///
        /// # Why the frame is dropped
        /// `osr::forget` here is the point of the whole handler. Leaving the
        /// last frame up is what makes a crash invisible; dropping it is
        /// what makes the tab go blank and the failure state show. It also
        /// releases the destination buffer of a browser that will never
        /// paint again.
        ///
        /// Recovery is not automatic and deliberately so: a page that
        /// crashed once will usually crash again on reload, and a tab that
        /// silently reloaded itself into a loop would be worse than one
        /// that says what happened. `drive` will rebuild the browser when
        /// the operator navigates or reloads.
        /// **D7 — HTTP basic/digest auth, and proxy auth.**
        ///
        /// Without this a page behind a 401 is a blank tab: CEF's default
        /// is to decline, Chromium turns that into the server's own 401
        /// body (usually empty), and there is nowhere to type a password.
        ///
        /// # The return value is the whole contract
        /// `1` means **eave has taken the callback and will answer it**,
        /// and Chromium will wait indefinitely for that. `0` means it did
        /// not, and Chromium cancels. So the only unsafe thing here is
        /// returning 1 down a path that drops the callback — which is why
        /// the callback is cloned into `PENDING` before anything can fail,
        /// and why a missing callback returns 0 rather than pretending.
        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            realm: Option<&CefString>,
            _scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else { return 0 };
            let host = host.map(|h| h.to_string()).unwrap_or_default();
            if host.is_empty() {
                // A password prompt that cannot say who is asking is a
                // phishing surface, not a feature. Decline instead.
                return 0;
            }
            let origin = origin_url.map(|o| o.to_string()).unwrap_or_default();
            if trace() {
                eprintln!(
                    "[web] auth challenge for {} from {host} (proxy={})",
                    self.tab_id,
                    is_proxy != 0
                );
            }
            let raised = super::dialogs::raise_auth(
                &self.tab_id,
                &origin,
                &host,
                port,
                realm.map(|r| r.to_string()),
                is_proxy != 0,
                callback,
            );
            raised as ::std::os::raw::c_int
        }

        /// **D8 — a TLS certificate that did not check out.**
        ///
        /// Same contract as `auth_credentials`: `1` keeps the callback and
        /// the load waits; `0` lets Chromium cancel. The difference is
        /// which way the silence falls — declining here is *safe*, so a
        /// path that cannot raise the interstitial returns 0 and the load
        /// simply fails, exactly as it does today.
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            cert_error: Errorcode,
            request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else { return 0 };
            let url = request_url.map(|u| u.to_string()).unwrap_or_default();
            if url.is_empty() {
                return 0;
            }
            let reason = cert_error_reason(cert_error);
            if trace() {
                eprintln!("[web] certificate error for {}: {reason} ({url})", self.tab_id);
            }
            super::dialogs::raise_cert(&self.tab_id, &url, reason, callback);
            1
        }

        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: i32,
            error_string: Option<&CefString>,
        ) {
            // Chromium's own words when it has any; its own words are what
            // `LoadState::Failed` exists to carry verbatim.
            let detail = error_string
                .map(|e| e.to_string())
                .filter(|e| !e.is_empty());
            let reason = match detail {
                Some(detail) => format!("{} ({detail})", termination_reason(status)),
                None if error_code != 0 => {
                    format!("{} (code {error_code})", termination_reason(status))
                }
                None => termination_reason(status).to_string(),
            };
            if trace() {
                eprintln!("[web] render process terminated for {}: {reason}", self.tab_id);
            }
            // Drop the stale frame FIRST, so nothing can repaint the corpse
            // between the report and the next render.
            super::osr::forget(&self.tab_id);
            set_chrome(&self.tab_id, |c| {
                c.progress = 0.;
                // The renderer that would have sent the clearing empty
                // string is the one that just died, so nothing else will
                // take this down.
                c.tooltip = None;
                c.fullscreen = false;
            });
            note_status(&self.tab_id, None);
            report(
                &self.tab_id,
                PageFact::Load {
                    load: LoadState::Failed { reason },
                    // A dead renderer cannot be asked about its history, and
                    // `None` is what says so — the engine keeps its own nav
                    // stack, which is exactly the state that survives this.
                    can_go_back: None,
                    can_go_forward: None,
                },
            );
        }
    }
}

/// Why a renderer died, in words an operator can act on.
///
/// Chromium's status codes are not self-explanatory and two of them mean
/// very different things to the person reading them: `PROCESS_OOM` is "this
/// page wanted more memory than it could have" and is about the page, while
/// `LAUNCH_FAILED` is about eave's own install and is not the page's fault
/// at all. Rendering both as "the page crashed" would send someone looking
/// in the wrong place.
fn termination_reason(status: TerminationStatus) -> &'static str {
    match status {
        TerminationStatus::PROCESS_CRASHED => "the page's renderer crashed",
        TerminationStatus::PROCESS_OOM => "the page ran out of memory",
        TerminationStatus::PROCESS_WAS_KILLED => "the page's renderer was killed",
        TerminationStatus::LAUNCH_FAILED => "the page's renderer could not start",
        TerminationStatus::INTEGRITY_FAILURE => "the page's renderer failed an integrity check",
        _ => "the page's renderer stopped",
    }
}

/// Chromium's own name for a certificate failure (D8).
///
/// **The `ERR_` name, not a sentence.** It is what every other browser
/// prints, what a search finds, and what a server operator needs to fix
/// their configuration — `ask::certificate_explanation` turns it into
/// words for the person reading the interstitial, and that mapping lives
/// outside the feature gate so the suite can check it.
///
/// The catch-all keeps the shape rather than inventing a name: an
/// unrecognised code is still a certificate error and still must not be
/// waved through.
fn cert_error_reason(code: Errorcode) -> &'static str {
    match code {
        Errorcode::CERT_COMMON_NAME_INVALID => "ERR_CERT_COMMON_NAME_INVALID",
        Errorcode::CERT_DATE_INVALID => "ERR_CERT_DATE_INVALID",
        Errorcode::CERT_AUTHORITY_INVALID => "ERR_CERT_AUTHORITY_INVALID",
        Errorcode::CERT_CONTAINS_ERRORS => "ERR_CERT_CONTAINS_ERRORS",
        Errorcode::CERT_NO_REVOCATION_MECHANISM => "ERR_CERT_NO_REVOCATION_MECHANISM",
        Errorcode::CERT_UNABLE_TO_CHECK_REVOCATION => "ERR_CERT_UNABLE_TO_CHECK_REVOCATION",
        Errorcode::CERT_REVOKED => "ERR_CERT_REVOKED",
        Errorcode::CERT_INVALID => "ERR_CERT_INVALID",
        Errorcode::CERT_WEAK_SIGNATURE_ALGORITHM => "ERR_CERT_WEAK_SIGNATURE_ALGORITHM",
        Errorcode::CERT_NON_UNIQUE_NAME => "ERR_CERT_NON_UNIQUE_NAME",
        Errorcode::CERT_WEAK_KEY => "ERR_CERT_WEAK_KEY",
        Errorcode::CERT_NAME_CONSTRAINT_VIOLATION => "ERR_CERT_NAME_CONSTRAINT_VIOLATION",
        Errorcode::CERT_VALIDITY_TOO_LONG => "ERR_CERT_VALIDITY_TOO_LONG",
        Errorcode::CERTIFICATE_TRANSPARENCY_REQUIRED => "ERR_CERTIFICATE_TRANSPARENCY_REQUIRED",
        Errorcode::CERT_KNOWN_INTERCEPTION_BLOCKED => "ERR_CERT_KNOWN_INTERCEPTION_BLOCKED",
        Errorcode::CERT_SELF_SIGNED_LOCAL_NETWORK => "ERR_CERT_SELF_SIGNED_LOCAL_NETWORK",
        _ => "ERR_CERT_INVALID",
    }
}

wrap_display_handler! {
    struct PageDisplayHandler {
        tab_id: String,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if trace() {
                eprintln!("[web] on_title_change {:?}", title.map(|t| t.to_string()));
            }
            let title = title.map(|t| t.to_string()).filter(|t| !t.is_empty());
            report(&self.tab_id, PageFact::Titled { title });
        }

        /// The address changed **without a load** — `history.pushState`,
        /// `replaceState`, a fragment jump.
        ///
        /// `on_load_start` cannot see any of those: no document is fetched,
        /// so no load starts. On a single-page app that is most of the
        /// navigation that ever happens, and without this the omnibox shows
        /// the address you arrived at half an hour ago while the page has
        /// moved on — the classic embedded-browser tell.
        ///
        /// # Why the dedup
        /// This ALSO fires for every ordinary navigation, right after
        /// `on_load_start` has already reported the same url. The engine's
        /// [`eave_core::browser::tabs::NavStack::commit`] treats a same-url
        /// commit as a refresh and does not push a second entry, so the
        /// stack would stay correct — but the fold still writes a
        /// `HistoryEvent::Visited` with `Transition::Reload` each time, and
        /// a reload entry per navigation is exactly the typeahead inflation
        /// Slice C5 removed. Dedup here, where the previous url is known,
        /// costs a `HashMap` lookup and keeps that fix intact.
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            // Sub-frames change address constantly and are not the tab's
            // address — the same rule `on_load_start` keeps, and the
            // classic version of this bug is an iframe's url in the
            // omnibox.
            if frame.map(|f| f.is_main()) != Some(1) {
                return;
            }
            let Some(url) = url.map(|u| u.to_string()).filter(|u| !u.is_empty()) else {
                return;
            };
            if trace() {
                eprintln!("[web] on_address_change {url:?}");
            }
            if !note_url(&self.tab_id, &url) {
                return;
            }
            report(&self.tab_id, PageFact::Committed { url, title: None });
        }

        /// Where the page says its icon lives, and the fetch that follows.
        ///
        /// The URL is a fact about the tab (journalled). The pixels are a
        /// cache in `favicon.rs`, fetched through this browser so cookies
        /// and the proxy are Chromium's. The strip paints those bytes; the
        /// globe is only "we do not have them yet".
        fn on_favicon_urlchange(
            &self,
            _browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            // `CefStringList` only reads by value and we hold a borrow.
            // The clone is shallow — a `Borrowed` collection, which `Drop`
            // does not free — so this reads the list without taking
            // ownership of Chromium's.
            let urls: Vec<String> = icon_urls
                .map(|l| l.clone())
                .map(|l| l.into_iter().filter(|u| !u.is_empty()).collect())
                .unwrap_or_default();
            // Chromium lists "best first", but "best" is often a 16×16
            // ICO. The plate paints through gpui's `img`, which prefers a
            // PNG/SVG/WEBP it can decode without a codec surprise. Fall
            // back to whatever Chromium offered rather than drawing
            // nothing.
            let picked = pick_favicon_url(&urls);
            if trace() {
                eprintln!("[web] on_favicon_urlchange {picked:?} from {urls:?}");
            }
            report(
                &self.tab_id,
                PageFact::Favicon {
                    favicon_url: picked.clone(),
                },
            );
            if let Some(url) = picked {
                let page = LAST_URL.with(|u| u.borrow().get(&self.tab_id).cloned());
                super::favicon::fetch(&self.tab_id, &url, page.as_deref().unwrap_or_default());
            }
        }

        /// 0.0 to 1.0 across a load.
        ///
        /// Local to the app and not a `PageFact`: `LoadState` is a fold the
        /// engine journals decisions against, and a value that changes
        /// sixty times a second does not belong in it. The strip reads this
        /// directly through [`progress`].
        fn on_loading_progress_change(&self, _browser: Option<&mut Browser>, progress: f64) {
            set_chrome(&self.tab_id, |c| c.progress = progress as f32);
        }

        /// The link under the pointer — the bubble in the corner.
        ///
        /// Show waits [`super::STATUS_SHOW_DELAY`]; an empty string (the
        /// pointer left the link) clears immediately. The delay lives in
        /// [`note_status`], not here, so a test can drive the clock
        /// without a CEF callback.
        fn on_status_message(&self, _browser: Option<&mut Browser>, value: Option<&CefString>) {
            let text = value.map(|v| v.to_string()).filter(|v| !v.is_empty());
            note_status(&self.tab_id, text);
        }

        /// D12 — the `title=` under the pointer.
        ///
        /// **Returns 1: handled.** CEF's contract is that 0 lets the
        /// browser display its own tooltip, and a windowless browser has
        /// no window to display one in — so 0 here would be eave declining
        /// to draw a tooltip in favour of nobody drawing one. This is the
        /// same shape as every other OSR callback: the default is not
        /// "Chromium does it", it is "it does not happen".
        ///
        /// CEF sends an empty string on leaving an element, which clears
        /// the field rather than leaving the last tooltip stuck to the
        /// pointer.
        fn on_tooltip(
            &self,
            _browser: Option<&mut Browser>,
            text: Option<&mut CefString>,
        ) -> ::std::os::raw::c_int {
            let next = text
                .map(|t| t.to_string())
                .filter(|t| !t.is_empty());
            set_chrome(&self.tab_id, |c| c.tooltip = next.clone());
            1
        }

        /// The page asked for fullscreen (a video, usually).
        ///
        /// Recorded, not acted on: what fullscreen MEANS in a browser that
        /// is also an IDE is a frame decision, not a renderer one (spec
        /// 06-browser D10). Recording it is what lets the shell answer
        /// later without the page having to ask twice.
        fn on_fullscreen_mode_change(&self, _browser: Option<&mut Browser>, fullscreen: i32) {
            if trace() {
                eprintln!("[web] on_fullscreen_mode_change {fullscreen}");
            }
            set_chrome(&self.tab_id, |c| c.fullscreen = fullscreen != 0);
        }

        /// The pointer, over whatever is under it.
        ///
        /// A native browser gets this from the OS view; an off-screen one
        /// gets it here and nowhere else, so without this the cursor is an
        /// arrow over every link, every text field and every column-resize
        /// handle on the web. It is the single loudest "this is a picture
        /// of a website" tell, and it is one callback.
        ///
        /// Returning 0 lets CEF keep its own bookkeeping; eave sets the
        /// real cursor from gpui, where the page element is
        /// ([`super::page::page_element`]).
        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> i32 {
            set_chrome(&self.tab_id, |c| c.cursor = cursor_style(type_));
            0
        }

        /// `console.log` from the page.
        ///
        /// Returning 0 keeps Chromium's own default handling. This is
        /// wired now because it is the cheap half of spec 06-browser E8 —
        /// an agent that can read the page's console — and because a
        /// renderer that is throwing on every frame is otherwise silent.
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: i32,
        ) -> i32 {
            // The frontend's own close, asking to be shut. Two conditions,
            // both required: the exact sentinel, and a `devtools:` browser
            // — so an ordinary page that happens to log the same string
            // cannot close anybody's panel. Answering 1 keeps it out of
            // Chromium's log, since it is a message to eave and not a
            // console line anyone wants to read.
            if let Some(parent) = super::devtools_parent(&self.tab_id) {
                let msg = message.map(|m| m.to_string()).unwrap_or_default();
                if msg == DOCK_CLOSE_SENTINEL {
                    queue_dock_close(&self.tab_id);
                    return 1;
                }
                if let Some(rest) = msg.strip_prefix(DOCK_BOUNDS_SENTINEL) {
                    // The frontend telling eave where the page goes. eave
                    // is the embedder for this one call, which is the
                    // whole of the docking contract it can honour.
                    if let Some(rect) = super::parse_dock_bounds(rest.trim()) {
                        if super::set_dock_page_rect(parent, rect) {
                            // Only the page MOVED — no page frame is
                            // coming to carry that onto the screen.
                            super::host::note_chrome_change();
                        }
                    }
                    return 1;
                }
            }
            if trace() {
                eprintln!(
                    "[web] console {:?} {}:{} {}",
                    level,
                    source.map(|s| s.to_string()).unwrap_or_default(),
                    line,
                    message.map(|m| m.to_string()).unwrap_or_default(),
                );
            }
            0
        }

        /// Camera and microphone, which is the ONE media signal CEF gives
        /// eave with both edges (Slice E3). A tab holding a device is in a
        /// call, and a tab in a call must not be evicted for idleness —
        /// nobody looks at the tab they are talking through.
        ///
        /// This is device CAPTURE, not playback; the two are different
        /// questions and only this one has an honest answer. See
        /// `PageAudioHandler` for the other.
        fn on_media_access_change(
            &self,
            _browser: Option<&mut Browser>,
            has_video_access: ::std::os::raw::c_int,
            has_audio_access: ::std::os::raw::c_int,
        ) {
            let capturing = has_video_access != 0 || has_audio_access != 0;
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] media access {} capturing={capturing}", self.tab_id);
            }
            super::evict::note_media_access(&self.tab_id, capturing);
        }
    }
}

wrap_audio_handler! {
    struct PageAudioHandler {
        tab_id: String,
    }

    impl AudioHandler {
        /// **eave declines the capture, and the return value is the whole
        /// point of this handler** (Slice E3).
        ///
        /// CEF's contract on `get_audio_parameters` is "return true to
        /// proceed with audio stream capture, or false to cancel it", and
        /// proceeding means the page's audio is routed to this handler as
        /// PCM *instead of* to the speakers. eave has nothing to play PCM
        /// with, so proceeding would silence every page in exchange for
        /// knowing that it was making noise. Returning 0 leaves playback
        /// exactly as it was.
        ///
        /// What that costs is the stop edge. `on_audio_stream_started` and
        /// `on_audio_stream_stopped` only fire for a capture we accepted,
        /// so the ONLY thing eave ever learns is that a stream began —
        /// which is why the eviction clock treats audio as a hold with a
        /// timeout rather than as a state. Both failure modes are named on
        /// `thresholds::PAGE_AUDIO_HOLD`.
        ///
        /// If CEF ever grows an `IsAudible` on the browser host, this
        /// handler and that constant should both go.
        fn audio_parameters(
            &self,
            _browser: Option<&mut Browser>,
            _params: Option<&mut AudioParameters>,
        ) -> ::std::os::raw::c_int {
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                eprintln!("[web] audio stream starting on {}", self.tab_id);
            }
            super::evict::note_audio_started(&self.tab_id);
            // 0 = do not capture. Playback is untouched.
            0
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

        /// `alert()` / `confirm()` / `prompt()` / `beforeunload`. Without
        /// this arm CEF suppresses the dialog and continues as if the
        /// person had cancelled — `confirm()` is permanently false and
        /// nothing says why.
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(super::dialogs::PageJsDialogHandler::new(self.tab_id.clone()))
        }

        /// Camera, microphone, geolocation, notifications, clipboard.
        /// Without this arm they are all auto-denied without asking.
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(super::dialogs::PagePermissionHandler::new(self.tab_id.clone()))
        }

        /// `<input type=file>`. Without this arm the click is consumed and
        /// no picker opens — there is no native window to parent one to
        /// under off-screen rendering.
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(super::dialogs::PageFileDialogHandler::new(self.tab_id.clone()))
        }

        /// Downloads. Without this arm a download link is a dead link
        /// that reports nothing — CEF cancels a download whose handler
        /// never calls `cont`, and with no handler at all there is nothing
        /// to call it.
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(PageDownloadHandler::new(self.tab_id.clone()))
        }

        /// Wired for `on_render_process_terminated` alone so far. The rest
        /// of this handler is the largest single gap left in eave's
        /// browser: HTTP auth (`get_auth_credentials`), certificate errors
        /// (`on_certificate_error`) and resource control all live here.
        /// See `docs/spec/06-browser.md` D7, D8, E1.
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(PageRequestHandler::new(self.tab_id.clone()))
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

        /// Present only so that eave is TOLD about audio (Slice E3). It
        /// declines every capture it is offered — see `PageAudioHandler`.
        ///
        /// Slice R1: this arm was left inside a TEST FUNCTION by the E3
        /// merge and compiled nowhere, which is why `note_audio_started`
        /// read as dead code. `cargo check` does not build `#[cfg(test)]`
        /// under a feature flag; `--all-targets` does, and that is the only
        /// reason it was caught.
        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(PageAudioHandler::new(self.tab_id.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Only the parts that are eave's own reasoning: the address dedup, the
// change detection behind the redraw gate, and the cursor mapping. Nothing
// here starts CEF — these run under `cargo test --features web` without a
// framework, which is the same bargain `mod.rs` makes for the whole module.

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter is process-wide (the pump reads it from gpui's executor),
    /// so the tests that assert on it take a turn rather than racing.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// **The defect that made the DevTools dock paint black for three
    /// builds**, as a test: Chromium's protocol endpoint ignores
    /// `Connection: close` and holds the socket open after a complete
    /// response. Reading to EOF therefore ends in `Err(TimedOut)`, and
    /// treating that as failure throws away a response already in hand —
    /// so the target lookup returned nothing, `show_devtools` gave up,
    /// and no browser was ever created.
    ///
    /// The server here answers and then deliberately does NOT close,
    /// which is the whole point: against a server that closes politely,
    /// the broken version passed.
    #[test]
    fn a_complete_response_is_read_even_when_the_server_never_closes() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"[{"type":"page","url":"https://example.com/"}]"#;
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length:{}\r\nContent-Type:application/json\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            // Held open, exactly like Chromium's endpoint. Outlives the
            // read; the socket dies with the test process.
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        assert_eq!(
            devtools_targets_json_at(addr).as_deref(),
            Some(body),
            "the body was discarded because the server kept the socket open"
        );
    }

    /// **The two halves of D8's naming, pinned to each other.** This side
    /// maps `Errorcode` onto a string; `ask::certificate_explanation` maps
    /// that string onto words. Nothing in the type system connects them,
    /// so a code named here and absent from `CERTIFICATE_REASONS` would
    /// silently get the generic sentence in front of a person deciding
    /// whether they are being attacked.
    #[test]
    fn every_certificate_code_eave_names_is_one_the_explanations_know() {
        let codes = [
            Errorcode::CERT_COMMON_NAME_INVALID,
            Errorcode::CERT_DATE_INVALID,
            Errorcode::CERT_AUTHORITY_INVALID,
            Errorcode::CERT_CONTAINS_ERRORS,
            Errorcode::CERT_NO_REVOCATION_MECHANISM,
            Errorcode::CERT_UNABLE_TO_CHECK_REVOCATION,
            Errorcode::CERT_REVOKED,
            Errorcode::CERT_INVALID,
            Errorcode::CERT_WEAK_SIGNATURE_ALGORITHM,
            Errorcode::CERT_NON_UNIQUE_NAME,
            Errorcode::CERT_WEAK_KEY,
            Errorcode::CERT_NAME_CONSTRAINT_VIOLATION,
            Errorcode::CERT_VALIDITY_TOO_LONG,
            Errorcode::CERTIFICATE_TRANSPARENCY_REQUIRED,
            Errorcode::CERT_KNOWN_INTERCEPTION_BLOCKED,
            Errorcode::CERT_SELF_SIGNED_LOCAL_NETWORK,
        ];
        for code in codes {
            let named = cert_error_reason(code);
            assert!(
                super::super::ask::CERTIFICATE_REASONS.contains(&named),
                "{named} is produced by cert_error_reason and unknown to the explanations"
            );
        }
        // …and the list has nothing in it the mapping cannot produce.
        let produced: Vec<&str> = codes.iter().map(|c| cert_error_reason(*c)).collect();
        for reason in super::super::ask::CERTIFICATE_REASONS {
            assert!(
                produced.contains(reason),
                "{reason} is explained but no Errorcode maps to it"
            );
        }
    }

    /// An unrecognised code must not become "not a certificate error".
    /// It keeps the shape, and the interstitial still refuses by default.
    #[test]
    fn an_unknown_certificate_code_is_still_a_certificate_error() {
        let named = cert_error_reason(Errorcode::FAILED);
        assert!(named.starts_with("ERR_CERT"), "{named}");
        assert!(super::super::ask::CERTIFICATE_REASONS.contains(&named));
    }

    #[test]
    fn the_first_url_for_a_tab_is_a_change() {
        assert!(note_url("t1", "https://a.test/"));
    }

    #[test]
    fn the_same_url_twice_is_not_a_change() {
        assert!(note_url("t2", "https://a.test/"));
        assert!(!note_url("t2", "https://a.test/"));
    }

    /// The case the dedup exists for: `on_load_start` seeds the url and
    /// `on_address_change` immediately echoes it. The echo must be silent,
    /// or every navigation writes a spurious `Transition::Reload` into
    /// history and inflates the typeahead — the inflation Slice C5 removed.
    #[test]
    fn a_pushstate_after_a_load_is_a_change_but_the_echo_is_not() {
        assert!(note_url("t3", "https://a.test/"));
        assert!(!note_url("t3", "https://a.test/"), "the echo of the load");
        assert!(note_url("t3", "https://a.test/next"), "a real pushState");
    }

    #[test]
    fn tabs_do_not_share_an_address() {
        assert!(note_url("t4a", "https://a.test/"));
        assert!(
            note_url("t4b", "https://a.test/"),
            "a second tab at the same url is its own first navigation"
        );
    }

    #[test]
    fn closing_a_tab_forgets_its_address() {
        assert!(note_url("t5", "https://a.test/"));
        forget_chrome("t5");
        assert!(
            note_url("t5", "https://a.test/"),
            "a reused tab id must not inherit the old tab's address"
        );
    }

    #[test]
    fn a_link_hover_does_not_draw_until_it_has_dwelled() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = Instant::now();
        note_status_at("status-a", Some("https://a.test/".into()), t0);
        assert_eq!(chrome("status-a").and_then(|c| c.status), None);
        promote_status_at(t0 + super::super::STATUS_SHOW_DELAY - std::time::Duration::from_millis(1));
        assert_eq!(chrome("status-a").and_then(|c| c.status), None);
        promote_status_at(t0 + super::super::STATUS_SHOW_DELAY);
        assert_eq!(
            chrome("status-a").and_then(|c| c.status).as_deref(),
            Some("https://a.test/")
        );
        forget_chrome("status-a");
    }

    #[test]
    fn leaving_a_link_hides_the_bubble_immediately() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = Instant::now();
        note_status_at("status-b", Some("https://a.test/".into()), t0);
        promote_status_at(t0 + super::super::STATUS_SHOW_DELAY);
        note_status_at("status-b", None, t0 + super::super::STATUS_SHOW_DELAY);
        assert_eq!(chrome("status-b").and_then(|c| c.status), None);
        forget_chrome("status-b");
    }

    #[test]
    fn a_shown_bubble_updates_without_waiting_again() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = Instant::now();
        note_status_at("status-c", Some("https://a.test/".into()), t0);
        promote_status_at(t0 + super::super::STATUS_SHOW_DELAY);
        note_status_at(
            "status-c",
            Some("https://b.test/".into()),
            t0 + super::super::STATUS_SHOW_DELAY,
        );
        assert_eq!(
            chrome("status-c").and_then(|c| c.status).as_deref(),
            Some("https://b.test/")
        );
        forget_chrome("status-c");
    }

    #[test]
    fn moving_to_another_link_before_the_delay_restarts_it() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = Instant::now();
        let half = super::super::STATUS_SHOW_DELAY / 2;
        note_status_at("status-d", Some("https://a.test/".into()), t0);
        note_status_at("status-d", Some("https://b.test/".into()), t0 + half);
        promote_status_at(t0 + super::super::STATUS_SHOW_DELAY);
        assert_eq!(
            chrome("status-d").and_then(|c| c.status),
            None,
            "still only half a dwell on the second link"
        );
        promote_status_at(t0 + half + super::super::STATUS_SHOW_DELAY);
        assert_eq!(
            chrome("status-d").and_then(|c| c.status).as_deref(),
            Some("https://b.test/")
        );
        forget_chrome("status-d");
    }

    #[test]
    fn a_changed_cursor_moves_the_redraw_gate() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = chrome_revision();
        set_chrome("t6", |c| c.cursor = gpui::CursorStyle::PointingHand);
        assert!(chrome_revision() > before);
    }

    /// The whole point of the change detection. Chromium reports the cursor
    /// on every pointer move within one element, not only on the crossing,
    /// so a gate that moved on each callback would ask gpui to redraw the
    /// window at pointer rate over a page that is not painting — throwing
    /// away the idle-page property the spike measured at 1 frame in 9
    /// seconds.
    #[test]
    fn repeating_the_same_cursor_does_not() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_chrome("t7", |c| c.cursor = gpui::CursorStyle::IBeam);
        let settled = chrome_revision();
        for _ in 0..64 {
            set_chrome("t7", |c| c.cursor = gpui::CursorStyle::IBeam);
        }
        assert_eq!(
            chrome_revision(),
            settled,
            "64 identical reports, 0 redraws"
        );
    }

    #[test]
    fn chrome_is_absent_until_the_page_states_something() {
        assert!(chrome("t8-never-touched").is_none());
        set_chrome("t8-never-touched", |c| c.progress = 0.5);
        assert_eq!(chrome("t8-never-touched").map(|c| c.progress), Some(0.5));
    }

    /// The two that must not read alike. `PROCESS_OOM` is about the page;
    /// `LAUNCH_FAILED` is about eave's own install. Collapsing them to "the
    /// page crashed" sends the reader looking in the wrong place.
    #[test]
    fn a_dead_renderer_says_which_kind_of_dead() {
        assert_eq!(
            termination_reason(TerminationStatus::PROCESS_OOM),
            "the page ran out of memory"
        );
        assert_eq!(
            termination_reason(TerminationStatus::LAUNCH_FAILED),
            "the page's renderer could not start"
        );
        assert_ne!(
            termination_reason(TerminationStatus::PROCESS_OOM),
            termination_reason(TerminationStatus::LAUNCH_FAILED)
        );
    }

    /// An unrecognised status still says something true rather than
    /// nothing — a blank reason renders as a failed tab with no
    /// explanation, which is the state this handler exists to remove.
    #[test]
    fn an_unknown_termination_still_explains_itself() {
        let said = termination_reason(TerminationStatus::ABNORMAL_TERMINATION);
        assert!(!said.is_empty());
        assert!(said.contains("renderer"), "got {said:?}");
    }

    /// Downloads land in `~/Downloads` when it exists. The fallbacks are
    /// the point: a machine without one must still download somewhere
    /// rather than silently refuse, which is the do-nothing behaviour the
    /// whole handler exists to remove.
    #[test]
    fn downloads_have_somewhere_to_go() {
        let dir = download_dir();
        assert!(dir.is_absolute(), "got {dir:?}");
        assert!(dir.is_dir(), "not a directory: {dir:?}");
    }

    #[test]
    fn nothing_is_downloading_until_something_downloads() {
        assert!(downloads()
            .iter()
            .all(|d| !d.file_name.is_empty() || d.id == 0));
    }

    /// The redraw gate must move for a download too — it changes something
    /// eave draws with no page frame to carry the change. Same counter,
    /// same contract as a cursor.
    #[test]
    fn a_download_update_moves_the_redraw_gate() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = chrome_revision();
        bump_chrome_revision();
        assert!(chrome_revision() > before);
    }

    #[test]
    fn the_cursors_that_matter_are_mapped() {
        use gpui::CursorStyle as G;
        assert_eq!(cursor_style(CursorType::HAND), G::PointingHand, "links");
        assert_eq!(cursor_style(CursorType::IBEAM), G::IBeam, "text");
        assert_eq!(cursor_style(CursorType::EASTWESTRESIZE), G::ResizeLeftRight);
        assert_eq!(cursor_style(CursorType::NOTALLOWED), G::OperationNotAllowed);
    }

    /// An unmapped cursor falls to the arrow rather than to a near-miss.
    /// A wrong cursor states a wrong affordance, which is worse than a
    /// plain one — see `cursor_style`.
    #[test]
    fn an_unmapped_cursor_is_an_arrow_and_not_a_guess() {
        assert_eq!(
            cursor_style(CursorType::NORTHEASTPANNING),
            gpui::CursorStyle::Arrow
        );
    }
}
