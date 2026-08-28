//! The engine's half of the browser state (Slice B1).
//!
//! Owns the three logs, applies every mutation through one lock, and pokes
//! the two snapshot channels. It is deliberately thin: every rule about
//! what a tab is, what a move means, and what forgetting costs lives in
//! `eave_core::browser`, so a second surface folding the same logs reaches
//! the same state without reimplementing any of it.
//!
//! # Why the browser is engine-wide and not per session
//! Sessions each get a handle, a journal, and a broadcast, because sessions
//! are independent and a client subscribes to one. There is one browser.
//! Making it per-session would mean deciding which session's history an
//! omnibox searches, and the answer is "the person's", not "the agent's" —
//! which is the same reason this state is not folded into `SessionInfo`.
//!
//! # Locking
//! One `std::sync::Mutex` around the whole store, held across the log
//! append. The appends are small and the bookmark path fsyncs — the same
//! shape `Engine::rename` already has when it writes `meta.json` inline —
//! so this matches the existing cost model rather than inventing a new one.
//! If a profile ever shows it, the fix is to move the fsync off the
//! reactor, not to shard the lock: the store's whole value is that one
//! writer decides ordering.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::broadcast;

use eave_core::browser::bookmarks::{BookmarkEvent, BookmarkId};
use eave_core::browser::history::{HistoryEvent, Transition};
use eave_core::browser::permissions::{Decision, PermissionEvent, PermissionKind};
use eave_core::browser::snapshot::{BookmarksSnapshot, PermissionsSnapshot, TabsSnapshot};
use eave_core::browser::tabs::{
    normalize_url, GroupId, LoadState, NavStack, PendingNav, Setting, TabContent, TabEvent, TabId,
    PRIMARY_WINDOW,
};
use eave_core::browser::{BrowserStore, OpenReport};
use eave_core::journal::now_ms;
use eave_wire::{HistoryQueryOk, ImportOk, ImportedBookmark, ImportedVisit, PageFact};

/// Snapshot pokes carry no payload — the snapshot is taken at emission —
/// so the channel can be small and lag just coalesces, exactly as
/// `WATCH_CAP` explains for the session watch.
const BROWSER_CAP: usize = 64;

/// How many typeahead rows the engine returns when the client names no
/// limit.
///
/// A default, not a design: the number of rows a drop-down shows is the
/// client's call once there is a client to make it, and every request may
/// override this. Eight is what an omnibox list holds without becoming a
/// thing to scroll.
pub const HISTORY_QUERY_DEFAULT_LIMIT: usize = 8;

/// The most typeahead rows one reply will carry.
///
/// Derived from frame size, like `git::MAX_FILES` is. A match is a url, a
/// title and four integers — ~200 bytes — so this cap puts the reply at
/// ~20 KB, one ndjson line of the same order as a changed-file frame. A
/// client asking for more is not refused; it is answered with this many,
/// because a typeahead nobody can read is not worth a slower keystroke.
pub const HISTORY_QUERY_MAX_LIMIT: usize = 100;

/// Why the tab strip changed — and therefore how urgently it has to be
/// sent.
///
/// B1 sent a snapshot on every poke, which was right when the only thing
/// that could poke was an operator. A page engine changes the arithmetic:
/// one navigation produces `Loading` → `Complete` plus a title and a
/// favicon, and a strip of tabs loading at once produces that many at
/// once. Without a distinction the snapshot rate would follow the *pages*,
/// not the person.
///
/// The split is the same one `run_subscription` already makes between a
/// text delta and everything else: prose can wait 120 ms, an event cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPoke {
    /// Anything durable — opened, closed, moved, committed — plus a
    /// navigation *request*, which is not durable but is an operator
    /// action and must not sit in a buffer behind a spinner.
    Structural,
    /// Load state only. The spinner, and nothing else.
    Load,
}

/// The engine's browser state.
pub struct Browser {
    store: StdMutex<BrowserStore>,
    tabs_tx: broadcast::Sender<TabPoke>,
    bookmarks_tx: broadcast::Sender<()>,
    permissions_tx: broadcast::Sender<()>,
    /// Distinguishes two requests for the same address. See [`PendingNav`].
    nav_seq: AtomicU64,
    /// What opening the logs found. Held so it can be stated more than
    /// once — at startup on stderr, and to anything that asks later.
    report: OpenReport,
}

impl Browser {
    /// Open the three logs under `<data_dir>/browser/`.
    pub fn open(data_dir: &Path) -> std::io::Result<Arc<Self>> {
        let (store, report) = BrowserStore::open(data_dir)?;
        let (tabs_tx, _) = broadcast::channel(BROWSER_CAP);
        let (bookmarks_tx, _) = broadcast::channel(BROWSER_CAP);
        let (permissions_tx, _) = broadcast::channel(BROWSER_CAP);
        Ok(Arc::new(Self {
            store: StdMutex::new(store),
            tabs_tx,
            bookmarks_tx,
            permissions_tx,
            nav_seq: AtomicU64::new(0),
            report,
        }))
    }

    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    pub fn subscribe_tabs(&self) -> broadcast::Receiver<TabPoke> {
        self.tabs_tx.subscribe()
    }

    /// Announce a tab-strip change. Returns whether anyone was listening,
    /// which nothing acts on — a strip nobody watches still changed.
    fn poke_tabs(&self, kind: TabPoke) -> bool {
        self.tabs_tx.send(kind).is_ok()
    }

    pub fn subscribe_bookmarks(&self) -> broadcast::Receiver<()> {
        self.bookmarks_tx.subscribe()
    }

    pub fn tabs_snapshot(&self) -> TabsSnapshot {
        let s = self.store.lock().unwrap();
        s.tabs.snapshot(s.read_only())
    }

    pub fn subscribe_permissions(&self) -> broadcast::Receiver<()> {
        self.permissions_tx.subscribe()
    }

    pub fn bookmarks_snapshot(&self) -> BookmarksSnapshot {
        let s = self.store.lock().unwrap();
        s.bookmarks.snapshot(s.read_only())
    }

    pub fn permissions_snapshot(&self) -> PermissionsSnapshot {
        let s = self.store.lock().unwrap();
        s.permissions.snapshot(s.read_only())
    }

    /// Apply one tab event, poke the strip if it changed.
    fn tab(&self, event: TabEvent) -> Result<bool, String> {
        let changed = self
            .store
            .lock()
            .unwrap()
            .tab_event(event)
            .map_err(|e| format!("write tabs log: {e}"))?;
        if changed {
            // Everything that reaches the log is durable, and therefore
            // structural by definition.
            self.poke_tabs(TabPoke::Structural);
        }
        Ok(changed)
    }

    fn bookmark(&self, event: BookmarkEvent) -> Result<bool, String> {
        let changed = self
            .store
            .lock()
            .unwrap()
            .bookmark_event(event)
            .map_err(|e| format!("write bookmarks log: {e}"))?;
        if changed {
            let _ = self.bookmarks_tx.send(());
        }
        Ok(changed)
    }

    fn permission(&self, event: PermissionEvent) -> Result<bool, String> {
        let changed = self
            .store
            .lock()
            .unwrap()
            .permission_event(event)
            .map_err(|e| format!("write permissions log: {e}"))?;
        if changed {
            let _ = self.permissions_tx.send(());
        }
        Ok(changed)
    }

    fn history(&self, event: HistoryEvent) -> Result<bool, String> {
        self.store
            .lock()
            .unwrap()
            .history_event(event)
            .map_err(|e| format!("write history log: {e}"))
    }

    // -----------------------------------------------------------------
    // Tabs

    /// Open a tab and return its minted id.
    ///
    /// The id is the engine's to mint for the same reason a session's is:
    /// two clients opening a tab at the same instant must not be able to
    /// choose the same one.
    #[allow(clippy::too_many_arguments)]
    pub fn tab_open(
        &self,
        window: Option<String>,
        url: Option<String>,
        group: Option<GroupId>,
        index: Option<usize>,
        content: TabContent,
        opener: Option<TabId>,
        activate: bool,
        incognito: bool,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let ts_ms = now_ms();
        // Slice W1 — where an unnamed tab lands, in the order the answer
        // is least likely to be wrong:
        //
        // 1. The window the client named.
        // 2. **The opener's window.** A page calling `window.open` or a
        //    `target=_blank` click has one true answer — beside the page
        //    that opened it — and it is a fact the request already
        //    carries. Resolving it here rather than making the client
        //    send a window id keeps the client from holding a second,
        //    staler copy of something the fold already knows.
        // 3. The primary window, which is what every tab meant before
        //    windows existed.
        let window = window
            .or_else(|| {
                opener
                    .as_ref()
                    .and_then(|o| self.store.lock().unwrap().tabs.window_of(o).cloned())
            })
            .unwrap_or_else(|| PRIMARY_WINDOW.to_string());
        self.tab(TabEvent::Opened {
            id: id.clone(),
            window,
            group,
            index: index.unwrap_or(usize::MAX),
            content,
            opener,
            url: url.clone(),
            incognito,
            ts_ms,
        })?;
        // Opening at a url is a visit. Recorded here rather than left to
        // the page engine because a tab restored into an address the
        // operator typed IS a visit, whoever renders it — and history that
        // depended on which engine won would be the exact coupling this
        // slice exists to avoid. A private tab is the exception: writing
        // the url into history.jsonl is the opposite of private.
        if let Some(u) = url {
            if !incognito {
                self.history(HistoryEvent::Visited {
                    url: u,
                    title: None,
                    ts_ms,
                    transition: Transition::Typed,
                })?;
            }
        }
        if activate {
            self.tab(TabEvent::Activated { id: id.clone() })?;
        }
        Ok(id)
    }

    pub fn tab_close(&self, tab_id: TabId) -> Result<bool, String> {
        self.tab(TabEvent::Closed { id: tab_id })
    }

    pub fn tab_activate(&self, tab_id: TabId) -> Result<bool, String> {
        self.tab(TabEvent::Activated { id: tab_id })
    }

    pub fn tab_move(
        &self,
        tab_id: TabId,
        group: Option<GroupId>,
        index: usize,
    ) -> Result<bool, String> {
        self.tab(TabEvent::Moved {
            id: tab_id,
            group,
            index,
        })
    }

    pub fn tab_pin(&self, tab_id: TabId, pinned: bool) -> Result<bool, String> {
        self.tab(TabEvent::Pinned { id: tab_id, pinned })
    }

    pub fn tab_go(&self, tab_id: TabId, delta: i32) -> Result<bool, String> {
        let changed = self.tab(TabEvent::Traversed {
            id: tab_id.clone(),
            delta,
        })?;
        if !changed {
            return Ok(false);
        }
        // Walking the stack is a visit of its own kind: the page is
        // seen again, but the person did not choose the address, so it
        // must not inflate the typeahead ranking.
        let (url, title) = {
            let s = self.store.lock().unwrap();
            match s.tabs.tabs.get(&tab_id).and_then(|t| t.nav.current_entry()) {
                Some(e) => (e.url.clone(), e.title.clone()),
                None => return Ok(true),
            }
        };
        // Drive the page engine the same way `tab_navigate` does: a
        // pending the renderer watches. `delta` tells it to walk
        // Chromium's stack rather than `load_url` the walked address —
        // a load would push, and the next `on_load_start` `Committed`
        // would then look like a new navigation. After this traverse,
        // that commit is a refresh of the current entry
        // (`is_refresh_of_current`), so it does not push even if the
        // renderer falls back to a load (a restored tab whose Chromium
        // stack is empty).
        let seq = self.nav_seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut s = self.store.lock().unwrap();
            if let Some(t) = s.tabs.tabs.get_mut(&tab_id) {
                t.pending = Some(PendingNav {
                    url: url.clone(),
                    seq,
                    delta: Some(delta),
                });
                t.load = LoadState::Loading;
                // Until the page engine restates the arrows, the
                // snapshot falls back to the stack this traverse just
                // moved — so ⌘[ is live on the next frame, not after
                // Chromium catches up.
                t.page_can_go_back = None;
                t.page_can_go_forward = None;
            }
        }
        let _ = self.poke_tabs(TabPoke::Structural);
        self.history(HistoryEvent::Visited {
            url,
            title,
            ts_ms: now_ms(),
            transition: Transition::Restore,
        })?;
        Ok(true)
    }

    /// A navigation the operator asked for. **A request, not a commit.**
    ///
    /// B1 committed here, because nothing was attached to say what
    /// actually landed. Slice C1 attaches a page engine, and if this still
    /// committed then every navigation would be recorded twice — once on
    /// the ask and once on [`PageFact::Committed`] — which is two nav
    /// entries, two history visits, and a back button that needs pressing
    /// twice to go anywhere.
    ///
    /// So this now records only the intent, transiently. Whatever renders
    /// pages watches [`PendingNav`], loads it, and reports back through
    /// [`Self::tab_stated`]. **With nothing attached, nothing happens** —
    /// which is the honest answer to "go to this address" from a program
    /// that cannot fetch pages, and is what the load state already said.
    pub fn tab_navigate(&self, tab_id: TabId, url: String) -> Result<bool, String> {
        let Some(url) = normalize_url(&url) else {
            return Err("not a url".into());
        };
        // `fetch_add` rather than a lock: the seq only ever has to differ
        // from the previous one, so its exact value carries no meaning and
        // nothing observes it in order.
        let seq = self.nav_seq.fetch_add(1, Ordering::Relaxed);
        let changed = {
            let mut s = self.store.lock().unwrap();
            match s.tabs.tabs.get_mut(&tab_id) {
                Some(t) => {
                    t.pending = Some(PendingNav {
                        url,
                        seq,
                        delta: None,
                    });
                    // Asking is itself a load state: the omnibox commits,
                    // and the tab should say so before any engine answers.
                    t.load = LoadState::Loading;
                    true
                }
                None => return Err(format!("no such tab: {tab_id}")),
            }
        };
        if changed {
            let _ = self.poke_tabs(TabPoke::Structural);
        }
        Ok(changed)
    }

    /// A page engine reporting what it observed.
    pub fn tab_stated(&self, tab_id: TabId, fact: PageFact) -> Result<bool, String> {
        match fact {
            PageFact::Committed { url, title } => {
                let ts_ms = now_ms();
                // Read the request BEFORE applying the commit, because the
                // fold clears it. A commit that answers a request the
                // operator typed is a TYPED visit even though it arrives
                // by the same door as a link click — and the difference is
                // not cosmetic: `Transition` is what ranks the omnibox, so
                // recording every typed address as a link would make the
                // typeahead worse the more the address bar is used.
                //
                // The THIRD case is a reload, and it has to be read here for
                // the same reason: the fold makes a refresh and a fresh
                // navigation indistinguishable once it has run. See
                // `NavStack::is_refresh_of_current`.
                let (answered_a_request, is_reload) = {
                    let s = self.store.lock().unwrap();
                    match s.tabs.tabs.get(&tab_id) {
                        Some(t) => (t.pending.is_some(), t.nav.is_refresh_of_current(&url)),
                        None => (false, false),
                    }
                };
                let changed = self.tab(TabEvent::Navigated {
                    id: tab_id.clone(),
                    url: url.clone(),
                    title: title.clone(),
                    ts_ms,
                })?;
                if changed {
                    // The stack just moved (or refreshed). Drop any CEF
                    // overlay so the snapshot reads that stack until the
                    // matching Load fact restates the arrows.
                    let incognito = {
                        let mut s = self.store.lock().unwrap();
                        if let Some(t) = s.tabs.tabs.get_mut(&tab_id) {
                            t.page_can_go_back = None;
                            t.page_can_go_forward = None;
                            t.incognito
                        } else {
                            false
                        }
                    };
                    if incognito {
                        return Ok(true);
                    }
                    self.history(HistoryEvent::Visited {
                        url,
                        title,
                        ts_ms,
                        // Reload is checked FIRST, because a refresh
                        // typically also answers a request — the omnibox
                        // and the reload button both go through
                        // `tab_navigate` — and calling that Typed is
                        // exactly the inflation `Transition::Reload` exists
                        // to prevent. Before Slice C5 every refresh
                        // counted as a typed visit, so a page left open on
                        // a refresh loop climbed to the top of the
                        // typeahead: the omnibox's ranking got worse the
                        // more the page was reloaded.
                        transition: if is_reload {
                            Transition::Reload
                        } else if answered_a_request {
                            Transition::Typed
                        } else {
                            Transition::Link
                        },
                    })?;
                }
                Ok(changed)
            }
            PageFact::Titled { title } => {
                let (url, incognito) = {
                    let s = self.store.lock().unwrap();
                    match s.tabs.tabs.get(&tab_id) {
                        Some(t) => (t.url().map(str::to_string), t.incognito),
                        None => (None, false),
                    }
                };
                let changed = self.tab(TabEvent::Titled {
                    id: tab_id,
                    title: title.clone(),
                })?;
                // A title that arrives after the commit is the ordinary
                // case, so history learns it too — otherwise every entry
                // in the omnibox would be searchable by url alone. A
                // private tab's title stays out of history for the same
                // reason its url does.
                if !incognito {
                    if let (Some(url), Some(title)) = (url, title) {
                        self.history(HistoryEvent::Titled { url, title })?;
                    }
                }
                Ok(changed)
            }
            PageFact::Favicon { favicon_url } => self.tab(TabEvent::FaviconStated {
                id: tab_id,
                favicon_url,
            }),
            // Transient: applied to the in-memory tab and broadcast, and
            // deliberately NOT journaled. A restart must not restore a
            // strip of tabs claiming to be loading.
            PageFact::Load {
                load,
                can_go_back,
                can_go_forward,
            } => Ok(self.set_load(tab_id, load, can_go_back, can_go_forward)),
        }
    }

    /// Set a tab's load state without journaling it. The one mutation that
    /// bypasses the log, and the reason is the whole durability split: see
    /// `eave_core::browser::tabs`'s module docs.
    ///
    /// Slice E2: optional history flags ride the same fact. Absent leaves
    /// the snapshot on the nav stack (D1). Present overlays
    /// `TabInfo::can_go_back` / `can_go_forward` so the pane header and
    /// the chords read what the page engine just reported.
    fn set_load(
        &self,
        tab_id: TabId,
        load: LoadState,
        can_go_back: Option<bool>,
        can_go_forward: Option<bool>,
    ) -> bool {
        let changed = {
            let mut s = self.store.lock().unwrap();
            match s.tabs.tabs.get_mut(&tab_id) {
                Some(t) => {
                    let mut changed = t.load != load;
                    if changed {
                        t.load = load;
                    }
                    if can_go_back.is_some() && t.page_can_go_back != can_go_back {
                        t.page_can_go_back = can_go_back;
                        changed = true;
                    }
                    if can_go_forward.is_some() && t.page_can_go_forward != can_go_forward {
                        t.page_can_go_forward = can_go_forward;
                        changed = true;
                    }
                    changed
                }
                None => false,
            }
        };
        if changed {
            self.poke_tabs(TabPoke::Load);
        }
        changed
    }

    /// One tab's whole back/forward stack.
    pub fn tab_nav(&self, tab_id: &str) -> Result<NavStack, String> {
        self.store
            .lock()
            .unwrap()
            .tabs
            .tabs
            .get(tab_id)
            .map(|t| t.nav.clone())
            .ok_or_else(|| format!("unknown tab {tab_id}"))
    }

    // -----------------------------------------------------------------
    // Groups

    pub fn group_create(
        &self,
        window: Option<String>,
        title: Option<String>,
        color: Option<String>,
        workspace: Option<String>,
        index: Option<usize>,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.tab(TabEvent::GroupCreated {
            id: id.clone(),
            window: window.unwrap_or_else(|| PRIMARY_WINDOW.to_string()),
            title,
            color,
            workspace,
            index: index.unwrap_or(usize::MAX),
            ts_ms: now_ms(),
        })?;
        Ok(id)
    }

    pub fn group_update(
        &self,
        group_id: GroupId,
        title: Option<Setting<String>>,
        color: Option<Setting<String>>,
        workspace: Option<Setting<String>>,
        collapsed: Option<bool>,
    ) -> Result<bool, String> {
        self.tab(TabEvent::GroupUpdated {
            id: group_id,
            title,
            color,
            workspace,
            collapsed,
        })
    }

    pub fn group_move(&self, group_id: GroupId, index: usize) -> Result<bool, String> {
        self.tab(TabEvent::GroupMoved {
            id: group_id,
            index,
        })
    }

    pub fn group_close(&self, group_id: GroupId) -> Result<bool, String> {
        self.tab(TabEvent::GroupClosed { id: group_id })
    }

    // -----------------------------------------------------------------
    // Bookmarks

    pub fn bookmark_add(
        &self,
        parent: Option<BookmarkId>,
        index: Option<usize>,
        title: Option<String>,
        url: Option<String>,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.bookmark(BookmarkEvent::Added {
            id: id.clone(),
            parent,
            index: index.unwrap_or(usize::MAX),
            title,
            url,
            ts_ms: now_ms(),
        })?;
        Ok(id)
    }

    pub fn bookmark_remove(&self, bookmark_id: BookmarkId) -> Result<bool, String> {
        self.bookmark(BookmarkEvent::Removed { id: bookmark_id })
    }

    pub fn bookmark_move(
        &self,
        bookmark_id: BookmarkId,
        parent: Option<BookmarkId>,
        index: usize,
    ) -> Result<bool, String> {
        // A move that would make a node its own ancestor is refused with
        // its reason rather than silently ignored: the caller dragged
        // something somewhere, and a drop that does nothing and says
        // nothing looks like a bug in the drag.
        {
            let s = self.store.lock().unwrap();
            if s.bookmarks.would_cycle(&bookmark_id, parent.as_ref()) {
                return Err(format!(
                    "cannot move {bookmark_id} there: a folder cannot be put \
                     inside itself, and a bookmark cannot be put inside a link"
                ));
            }
        }
        self.bookmark(BookmarkEvent::Moved {
            id: bookmark_id,
            parent,
            index,
        })
    }

    pub fn bookmark_rename(
        &self,
        bookmark_id: BookmarkId,
        title: Option<String>,
    ) -> Result<bool, String> {
        self.bookmark(BookmarkEvent::Renamed {
            id: bookmark_id,
            title,
        })
    }

    pub fn bookmark_retarget(&self, bookmark_id: BookmarkId, url: String) -> Result<bool, String> {
        self.bookmark(BookmarkEvent::Retargeted {
            id: bookmark_id,
            url,
        })
    }

    // -----------------------------------------------------------------
    // History

    /// Answer a typeahead. Times itself, because this path has a frame
    /// budget and a number nobody measures is a number nobody keeps.
    pub fn history_query(&self, typed: &str, limit: Option<usize>) -> HistoryQueryOk {
        let limit = limit
            .unwrap_or(HISTORY_QUERY_DEFAULT_LIMIT)
            .min(HISTORY_QUERY_MAX_LIMIT);
        let started = Instant::now();
        let s = self.store.lock().unwrap();
        let index = s.history.index();
        let matches = index.query(typed, limit);
        let total_entries = index.len();
        drop(s);
        HistoryQueryOk {
            matches,
            total_entries,
            took_us: started.elapsed().as_micros() as u64,
        }
    }

    pub fn history_forget(&self, url: String) -> Result<bool, String> {
        self.history(HistoryEvent::Forgotten { url })
    }

    pub fn history_clear(
        &self,
        since_ms: Option<u64>,
        before_ms: Option<u64>,
    ) -> Result<bool, String> {
        self.history(HistoryEvent::Cleared {
            since_ms,
            before_ms,
        })
    }

    /// File a parsed import in one batch. One bookmarks snapshot at the
    /// end, not one per node — the tree is one action.
    pub fn import(
        &self,
        bookmarks: Vec<ImportedBookmark>,
        history: Vec<ImportedVisit>,
    ) -> Result<ImportOk, String> {
        let ts_ms = now_ms();
        let mut bookmark_events = Vec::new();
        emit_bookmarks(&bookmarks, None, &mut bookmark_events, ts_ms);
        let history_events: Vec<HistoryEvent> = history
            .into_iter()
            .map(|v| HistoryEvent::Imported {
                url: v.url,
                title: v.title,
                visit_count: v.visit_count,
                last_visit_ms: v.last_visit_ms,
            })
            .collect();
        let (bm, hist) = self
            .store
            .lock()
            .unwrap()
            .import_batch(&bookmark_events, &history_events)
            .map_err(|e| format!("write import: {e}"))?;
        if bm > 0 {
            let _ = self.bookmarks_tx.send(());
        }
        Ok(ImportOk {
            bookmarks: bm as u32,
            history: hist as u32,
        })
    }

    // -----------------------------------------------------------------
    // Permissions (G1)

    /// Record one answer, which may cover several capabilities: a grouped
    /// camera-and-microphone prompt takes one answer and both are written.
    ///
    /// **Returns changed, not written.** An answer identical to the stored
    /// one is neither logged nor broadcast, which is what keeps a page
    /// that re-asks on every load from appending a line every time.
    pub fn permission_set(
        &self,
        origin: String,
        permissions: Vec<PermissionKind>,
        decision: Decision,
    ) -> Result<bool, String> {
        let ts_ms = now_ms();
        let mut changed = false;
        for permission in permissions {
            // Unstorable capabilities and unnameable origins are refused
            // by the fold, not here — one place decides, and it is the one
            // a hand-edited file also goes through.
            changed |= self.permission(PermissionEvent::Decided {
                origin: origin.clone(),
                permission,
                decision,
                ts_ms,
            })?;
        }
        Ok(changed)
    }

    pub fn permission_forget(
        &self,
        origin: String,
        permission: Option<PermissionKind>,
    ) -> Result<bool, String> {
        self.permission(PermissionEvent::Forgotten { origin, permission })
    }

    pub fn permission_clear(&self) -> Result<bool, String> {
        self.permission(PermissionEvent::Cleared)
    }

    // There is deliberately no `sync()` here. The engine's accept loop
    // runs until a signal kills it (spec 01 §Crash & engine shutdown),
    // so a shutdown flush would have no caller. Nothing is lost by that:
    // tab writes are write-behind to the OS, which survives the engine
    // dying by any means, and the two things worth more than that — a
    // bookmark, and a forget — fsync on every write.
}

fn emit_bookmarks(
    nodes: &[ImportedBookmark],
    parent: Option<String>,
    events: &mut Vec<BookmarkEvent>,
    ts_ms: u64,
) {
    for (index, node) in nodes.iter().enumerate() {
        let id = uuid::Uuid::new_v4().to_string();
        events.push(BookmarkEvent::Added {
            id: id.clone(),
            parent: parent.clone(),
            index,
            title: node.title.clone(),
            url: node.url.clone(),
            ts_ms,
        });
        if !node.children.is_empty() {
            emit_bookmarks(&node.children, Some(id), events, ts_ms);
        }
    }
}
