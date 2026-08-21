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
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::broadcast;

use eave_core::browser::bookmarks::{BookmarkEvent, BookmarkId};
use eave_core::browser::history::{HistoryEvent, Transition};
use eave_core::browser::snapshot::{BookmarksSnapshot, TabsSnapshot};
use eave_core::browser::tabs::{GroupId, LoadState, NavStack, Setting, TabContent, TabEvent, TabId};
use eave_core::browser::{BrowserStore, OpenReport};
use eave_core::journal::now_ms;
use eave_wire::{HistoryQueryOk, PageFact};

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

/// The engine's browser state.
pub struct Browser {
    store: StdMutex<BrowserStore>,
    tabs_tx: broadcast::Sender<()>,
    bookmarks_tx: broadcast::Sender<()>,
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
        Ok(Arc::new(Self {
            store: StdMutex::new(store),
            tabs_tx,
            bookmarks_tx,
            report,
        }))
    }

    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    pub fn subscribe_tabs(&self) -> broadcast::Receiver<()> {
        self.tabs_tx.subscribe()
    }

    pub fn subscribe_bookmarks(&self) -> broadcast::Receiver<()> {
        self.bookmarks_tx.subscribe()
    }

    pub fn tabs_snapshot(&self) -> TabsSnapshot {
        let s = self.store.lock().unwrap();
        s.tabs.snapshot(s.read_only())
    }

    pub fn bookmarks_snapshot(&self) -> BookmarksSnapshot {
        let s = self.store.lock().unwrap();
        s.bookmarks.snapshot(s.read_only())
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
            let _ = self.tabs_tx.send(());
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
        url: Option<String>,
        group: Option<GroupId>,
        index: Option<usize>,
        content: TabContent,
        opener: Option<TabId>,
        activate: bool,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let ts_ms = now_ms();
        self.tab(TabEvent::Opened {
            id: id.clone(),
            group,
            index: index.unwrap_or(usize::MAX),
            content,
            opener,
            url: url.clone(),
            ts_ms,
        })?;
        // Opening at a url is a visit. Recorded here rather than left to
        // the page engine because a tab restored into an address the
        // operator typed IS a visit, whoever renders it — and history that
        // depended on which engine won would be the exact coupling this
        // slice exists to avoid.
        if let Some(u) = url {
            self.history(HistoryEvent::Visited {
                url: u,
                title: None,
                ts_ms,
                transition: Transition::Typed,
            })?;
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
        if changed {
            // Walking the stack is a visit of its own kind: the page is
            // seen again, but the person did not choose the address, so it
            // must not inflate the typeahead ranking.
            let (url, title) = {
                let s = self.store.lock().unwrap();
                match s.tabs.tabs.get(&tab_id).and_then(|t| t.nav.current_entry()) {
                    Some(e) => (e.url.clone(), e.title.clone()),
                    None => return Ok(changed),
                }
            };
            self.history(HistoryEvent::Visited {
                url,
                title,
                ts_ms: now_ms(),
                transition: Transition::Restore,
            })?;
        }
        Ok(changed)
    }

    /// A navigation the operator asked for.
    ///
    /// **In this slice it commits immediately**, because no page engine is
    /// attached to report what actually landed. Once one is, this becomes
    /// a request and the commit arrives as
    /// [`PageFact::Committed`] — the seam is already in place, and the
    /// caveat is stated in the slice report rather than left to be
    /// discovered.
    pub fn tab_navigate(&self, tab_id: TabId, url: String) -> Result<bool, String> {
        let ts_ms = now_ms();
        let changed = self.tab(TabEvent::Navigated {
            id: tab_id,
            url: url.clone(),
            title: None,
            ts_ms,
        })?;
        if changed {
            self.history(HistoryEvent::Visited {
                url,
                title: None,
                ts_ms,
                transition: Transition::Typed,
            })?;
        }
        Ok(changed)
    }

    /// A page engine reporting what it observed.
    pub fn tab_stated(&self, tab_id: TabId, fact: PageFact) -> Result<bool, String> {
        match fact {
            PageFact::Committed { url, title } => {
                let ts_ms = now_ms();
                let changed = self.tab(TabEvent::Navigated {
                    id: tab_id,
                    url: url.clone(),
                    title: title.clone(),
                    ts_ms,
                })?;
                if changed {
                    self.history(HistoryEvent::Visited {
                        url,
                        title,
                        ts_ms,
                        transition: Transition::Link,
                    })?;
                }
                Ok(changed)
            }
            PageFact::Titled { title } => {
                let url = self
                    .store
                    .lock()
                    .unwrap()
                    .tabs
                    .tabs
                    .get(&tab_id)
                    .and_then(|t| t.url().map(str::to_string));
                let changed = self.tab(TabEvent::Titled {
                    id: tab_id,
                    title: title.clone(),
                })?;
                // A title that arrives after the commit is the ordinary
                // case, so history learns it too — otherwise every entry
                // in the omnibox would be searchable by url alone.
                if let (Some(url), Some(title)) = (url, title) {
                    self.history(HistoryEvent::Titled { url, title })?;
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
            PageFact::Load { load } => Ok(self.set_load(tab_id, load)),
        }
    }

    /// Set a tab's load state without journaling it. The one mutation that
    /// bypasses the log, and the reason is the whole durability split: see
    /// `eave_core::browser::tabs`'s module docs.
    fn set_load(&self, tab_id: TabId, load: LoadState) -> bool {
        let changed = {
            let mut s = self.store.lock().unwrap();
            match s.tabs.tabs.get_mut(&tab_id) {
                Some(t) if t.load != load => {
                    t.load = load;
                    true
                }
                _ => false,
            }
        };
        if changed {
            let _ = self.tabs_tx.send(());
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
        title: Option<String>,
        color: Option<String>,
        workspace: Option<String>,
        index: Option<usize>,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.tab(TabEvent::GroupCreated {
            id: id.clone(),
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
        self.tab(TabEvent::GroupMoved { id: group_id, index })
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

    // There is deliberately no `sync()` here. The engine's accept loop
    // runs until a signal kills it (spec 01 §Crash & engine shutdown),
    // so a shutdown flush would have no caller. Nothing is lost by that:
    // tab writes are write-behind to the OS, which survives the engine
    // dying by any means, and the two things worth more than that — a
    // bookmark, and a forget — fsync on every write.
}
