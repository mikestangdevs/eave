//! Browser state: tabs, groups, bookmarks, history.
//!
//! **Data only.** Nothing here fetches a page, renders one, or knows which
//! engine does. Every page fact arrives as an event that something else
//! states, which is the point: this is the half of a browser that does not
//! care what draws the page.
//!
//! The spike it was written not to prejudge has since been decided —
//! Chromium via CEF, off-screen, in `eave-app/src/web/` — and this module
//! did not change to accommodate it, which is the strongest evidence the
//! separation was real rather than decorative.
//!
//! # Layout on disk
//! ```text
//! <data_dir>/browser/tabs.jsonl        session restore: tabs, groups, ordering
//! <data_dir>/browser/bookmarks.jsonl   the hierarchy
//! <data_dir>/browser/history.jsonl     the visit log
//! <data_dir>/browser/permissions.jsonl what a person answered, per origin
//! <data_dir>/browser/chromium/         CEF's own cache root — NOT ours, and
//!                                      not a journal; see web/host.rs
//! ```
//!
//! Four files, not one, and the reason is that they have four different
//! lifetimes and four different failure consequences. Tabs churn every few
//! seconds and are worth little individually; bookmarks change rarely and
//! are worth a great deal each; history is the largest by orders of
//! magnitude and is the only one a person ever asks to have destroyed;
//! permissions are the smallest and the most dangerous to get wrong, since
//! a lost row re-asks and a wrong row grants a camera. Sharing one file
//! would make the cheapest writer set the durability policy for the most
//! valuable data in it, and would make forgetting one url rewrite the tab
//! strip.
//!
//! # Relationship to sessions (Slice B1 decision)
//! Sessions are **not** tabs and this slice does not make them tabs. The
//! session rail stays the system of record for agents: `sessions/<uuid>/`,
//! `watch`, `subscribe`, all unchanged. A tab may hold a *reference* to a
//! session ([`tabs::TabContent::Session`]) and a group may state which
//! checkout it is about ([`tabs::TabGroup::workspace`]) — two facts, both
//! inert, neither of which lets one store reach into the other. Closing a
//! tab that references a session does not touch the session, and the
//! process suite asserts exactly that.
//!
//! The reasoning, and where this should go, is in
//! `docs/reports/stage-b1-browser-state.md`.

pub mod bookmarks;
pub mod downloads;
pub mod history;
pub mod import;
pub mod log;
pub mod permissions;
pub mod snapshot;
pub mod tabs;

use std::io;
use std::path::{Path, PathBuf};

use bookmarks::{BookmarkEvent, BookmarkTree};
use history::{History, HistoryEvent};
use log::{EventLog, LoadReport};
use permissions::{PermissionEvent, Permissions};
use tabs::{TabEvent, TabsState};

/// Where the browser's three logs live under a data dir.
pub fn browser_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("browser")
}

/// What opening the browser store found, per log. Every non-clean report
/// here is something the engine states rather than swallows.
#[derive(Debug, Clone, Default)]
pub struct OpenReport {
    pub tabs: LoadReport,
    pub bookmarks: LoadReport,
    pub history: LoadReport,
    pub permissions: LoadReport,
}

impl OpenReport {
    /// One line per log that has something to say, or empty when all three
    /// loaded cleanly.
    pub fn stated(&self) -> Vec<String> {
        [
            ("tabs", &self.tabs),
            ("bookmarks", &self.bookmarks),
            ("history", &self.history),
            ("permissions", &self.permissions),
        ]
        .into_iter()
        .filter_map(|(name, r)| r.stated().map(|s| format!("{name}: {s}")))
        .collect()
    }
}

/// The three logs and the three folds, opened together.
///
/// Writes are **flush-on-append, fsync on the durable moments**, matching
/// the journal's write-behind contract: a tab move that is lost to a power
/// cut costs one strip position, and paying an fsync per keystroke-speed
/// event would cost more than it protects. Bookmarks fsync on every change,
/// because a bookmark is something somebody asked to keep.
pub struct BrowserStore {
    tabs_log: EventLog<TabEvent>,
    bookmarks_log: EventLog<BookmarkEvent>,
    history_log: EventLog<HistoryEvent>,
    permissions_log: EventLog<PermissionEvent>,
    pub tabs: TabsState,
    pub bookmarks: BookmarkTree,
    pub history: History,
    pub permissions: Permissions,
}

impl BrowserStore {
    /// Open every log under `<data_dir>/browser/`, folding each.
    pub fn open(data_dir: &Path) -> io::Result<(Self, OpenReport)> {
        let dir = browser_dir(data_dir);
        std::fs::create_dir_all(&dir)?;

        let (tabs_log, tab_events, tabs_report) =
            EventLog::<TabEvent>::open(&dir.join("tabs.jsonl"), "tabs")?;
        let (bookmarks_log, bookmark_events, bookmarks_report) =
            EventLog::<BookmarkEvent>::open(&dir.join("bookmarks.jsonl"), "bookmarks")?;
        let (history_log, history_events, history_report) =
            EventLog::<HistoryEvent>::open(&dir.join("history.jsonl"), "history")?;
        let (permissions_log, permission_events, permissions_report) =
            EventLog::<PermissionEvent>::open(&dir.join("permissions.jsonl"), "permissions")?;

        let mut tabs = TabsState::new();
        for e in &tab_events {
            tabs.apply(e);
        }
        // Nothing is loaded on restore, whatever the tabs were doing when
        // the process died. This is the transient half of the durability
        // split being enforced at the one place it could otherwise leak.
        for tab in tabs.tabs.values_mut() {
            tab.load = tabs::LoadState::Unloaded;
        }

        let mut bookmarks = BookmarkTree::new();
        for e in &bookmark_events {
            bookmarks.apply(e);
        }

        let mut history = History::new();
        history.load(history_events);

        let mut permissions = Permissions::new();
        for e in &permission_events {
            permissions.apply(e);
        }

        let mut store = Self {
            tabs_log,
            bookmarks_log,
            history_log,
            permissions_log,
            tabs,
            bookmarks,
            history,
            permissions,
        };
        // Enforce the entry cap at open, where paying for a compaction is
        // invisible, rather than on the visit that crosses it.
        if store.history.over_cap() {
            store.history.prune_to_cap();
            store.history_log.compact(store.history.retained())?;
        }
        let report = OpenReport {
            tabs: tabs_report,
            bookmarks: bookmarks_report,
            history: history_report,
            permissions: permissions_report,
        };
        Ok((store, report))
    }

    /// True when any log is from a format this build must not write.
    pub fn read_only(&self) -> bool {
        self.tabs_log.sealed()
            || self.bookmarks_log.sealed()
            || self.history_log.sealed()
            || self.permissions_log.sealed()
    }

    /// Apply and journal one tab event. Returns whether anything changed;
    /// a no-op is neither written nor broadcast.
    pub fn tab_event(&mut self, event: TabEvent) -> io::Result<bool> {
        // Private tabs are in-memory only. Journaling one would write its
        // url into tabs.jsonl, which is the opposite of private, and
        // restoring it on the next launch would show the operator a tab
        // they closed by quitting.
        let incognito = self.tabs.event_is_incognito(&event);
        if !self.tabs.apply(&event) {
            return Ok(false);
        }
        if incognito {
            return Ok(true);
        }
        self.tabs_log.append(&event)?;
        // Write-behind: the strip is worth a lost move, not an fsync per
        // keystroke.
        self.tabs_log.flush()?;
        Ok(true)
    }

    /// Apply and journal one bookmark event, fsynced. A bookmark is
    /// something somebody asked to keep, which is exactly the case where
    /// the fsync is worth its cost.
    pub fn bookmark_event(&mut self, event: BookmarkEvent) -> io::Result<bool> {
        if !self.bookmarks.apply(&event) {
            return Ok(false);
        }
        self.bookmarks_log.append(&event)?;
        self.bookmarks_log.sync()?;
        Ok(true)
    }

    /// Apply a batch of bookmark and history events from an import.
    ///
    /// One fsync at the end of the bookmarks, one flush at the end of
    /// history — not one per row. Importing Safari's 5k urls as 5k
    /// [`Self::bookmark_event`] calls would fsync 5k times for no extra
    /// durability the operator can use: the import is one action, and
    /// either the folder landed or it did not.
    pub fn import_batch(
        &mut self,
        bookmarks: &[BookmarkEvent],
        history: &[HistoryEvent],
    ) -> io::Result<(usize, usize)> {
        let mut bm = 0usize;
        for event in bookmarks {
            if self.bookmarks.apply(event) {
                self.bookmarks_log.append(event)?;
                bm += 1;
            }
        }
        if bm > 0 {
            self.bookmarks_log.sync()?;
        }

        let hist = self.history.apply_many(history);
        for event in history {
            // apply_many already folded; we still have to journal the
            // events that actually landed. A no-op (empty url, zero
            // visit_count) was not absorbed, so skip it here too.
            match event {
                HistoryEvent::Imported { url, visit_count, .. } => {
                    if visit_count == &0 || tabs::normalize_url(url).is_none() {
                        continue;
                    }
                }
                HistoryEvent::Visited { url, .. } | HistoryEvent::Titled { url, .. } => {
                    if tabs::normalize_url(url).is_none() {
                        continue;
                    }
                }
                _ => continue,
            }
            self.history_log.append(event)?;
        }
        if hist > 0 {
            self.history_log.flush()?;
            if self.history.over_cap() {
                self.history.prune_to_cap();
                self.history_log.compact(self.history.retained())?;
            }
        }
        Ok((bm, hist))
    }

    /// Apply and journal one history event.
    ///
    /// A forget or a clear rewrites the file instead of appending to it —
    /// see [`history`]'s module docs for why this is the one log allowed to
    /// be rewritten — and is fsynced, because "I deleted that" has to still
    /// be true after a power cut.
    pub fn history_event(&mut self, event: HistoryEvent) -> io::Result<bool> {
        let destructive = event.requires_compaction();
        if !self.history.apply(&event) {
            return Ok(false);
        }
        if destructive {
            self.history_log.compact(self.history.retained())?;
            self.history_log.sync()?;
        } else {
            self.history_log.append(&event)?;
            self.history_log.flush()?;
        }
        if self.history.over_cap() {
            self.history.prune_to_cap();
            self.history_log.compact(self.history.retained())?;
        }
        Ok(true)
    }

    /// Apply and journal one permission event, fsynced both ways.
    ///
    /// A grant is fsynced for the reason a bookmark is: somebody decided
    /// it deliberately and re-deciding it is a nag. A **revoke** is fsynced
    /// and *compacted* for the reason a history forget is: "I took the
    /// camera away from that site" has to still be true after a power cut,
    /// and a tombstone appended after the grant would leave the grant
    /// itself sitting in the file.
    pub fn permission_event(&mut self, event: PermissionEvent) -> io::Result<bool> {
        let destructive = event.requires_compaction();
        if !self.permissions.apply(&event) {
            return Ok(false);
        }
        if destructive {
            self.permissions_log.compact(&self.permissions.retained())?;
        } else {
            self.permissions_log.append(&event)?;
        }
        self.permissions_log.sync()?;
        Ok(true)
    }

    /// Push every buffered write to the OS and fsync. Called at shutdown
    /// and wherever the engine wants a known-durable point.
    pub fn sync(&mut self) -> io::Result<()> {
        self.tabs_log.sync()?;
        self.bookmarks_log.sync()?;
        self.history_log.sync()?;
        self.permissions_log.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabs::{LoadState, TabContent, TabEvent};

    fn scratch() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-bstore-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn open_tab(store: &mut BrowserStore, id: &str, url: &str) {
        store
            .tab_event(TabEvent::Opened {
                window: tabs::PRIMARY_WINDOW.to_string(),
                id: id.into(),
                group: None,
                index: usize::MAX,
                content: TabContent::Page,
                opener: None,
                url: Some(url.into()),
                incognito: false,
                ts_ms: 1,
            })
            .unwrap();
    }

    /// The durability line, enforced at the one place it could leak: a tab
    /// read back off disk is `Unloaded`, whatever it was doing when the
    /// process died.
    #[test]
    fn a_reopened_store_reports_every_tab_unloaded() {
        let dir = scratch();
        {
            let (mut store, report) = BrowserStore::open(&dir).unwrap();
            assert!(
                report.stated().is_empty(),
                "a fresh store had something to say"
            );
            open_tab(&mut store, "t", "https://x/");
            // In-memory only — the one mutation that bypasses the log.
            store.tabs.tabs.get_mut("t").unwrap().load = LoadState::Loading;
            store.sync().unwrap();
        }
        let (store, _) = BrowserStore::open(&dir).unwrap();
        assert_eq!(store.tabs.tabs["t"].load, LoadState::Unloaded);
        assert_eq!(store.tabs.tabs["t"].url(), Some("https://x/"));
    }

    /// One sealed log makes the WHOLE store read-only, and the snapshot
    /// says so. Reporting per-log would let a surface offer a bookmark
    /// button that silently does nothing.
    #[test]
    fn one_sealed_log_makes_the_whole_store_state_itself_read_only() {
        let dir = scratch();
        {
            let (mut store, _) = BrowserStore::open(&dir).unwrap();
            open_tab(&mut store, "t", "https://x/");
            store.sync().unwrap();
        }
        // A future build wrote the bookmarks file.
        let bm = browser_dir(&dir).join("bookmarks.jsonl");
        std::fs::write(
            &bm,
            "{\"format_version\":99,\"kind\":\"bookmarks\",\"created_ts_ms\":0}\n",
        )
        .unwrap();

        let (mut store, report) = BrowserStore::open(&dir).unwrap();
        assert!(store.read_only());
        let said = report.stated();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("bookmarks:"), "{said:?}");
        assert!(said[0].contains("format version 99"), "{said:?}");
        assert!(store.tabs.snapshot(store.read_only()).read_only);
        assert!(
            store.bookmarks.snapshot(store.read_only()).read_only,
            "the bookmark surface must know it cannot write"
        );
        // The tabs log is fine, so tab writes still land — but a bookmark
        // write is refused rather than silently dropped.
        assert!(store
            .tab_event(TabEvent::Pinned {
                id: "t".into(),
                pinned: true
            })
            .unwrap());
        assert!(store
            .bookmark_event(bookmarks::BookmarkEvent::Added {
                id: "b".into(),
                parent: None,
                index: 0,
                title: Some("x".into()),
                url: Some("https://x/".into()),
                ts_ms: 0,
            })
            .is_err());
        assert_eq!(
            std::fs::read_to_string(&bm).unwrap().lines().count(),
            1,
            "the future build's file was written to"
        );
    }

    /// The four logs are four files with four lifetimes. Forgetting a
    /// url rewrites history and must not touch the tab strip.
    #[test]
    fn forgetting_a_url_does_not_rewrite_the_tab_strip() {
        let dir = scratch();
        let (mut store, _) = BrowserStore::open(&dir).unwrap();
        open_tab(&mut store, "t", "https://keep.test/");
        store
            .history_event(history::HistoryEvent::Visited {
                url: "https://secret.test/".into(),
                title: None,
                ts_ms: 1,
                transition: history::Transition::Typed,
            })
            .unwrap();
        store.sync().unwrap();
        let tabs_before = std::fs::read_to_string(browser_dir(&dir).join("tabs.jsonl")).unwrap();

        assert!(store
            .history_event(history::HistoryEvent::Forgotten {
                url: "https://secret.test/".into()
            })
            .unwrap());
        let tabs_after = std::fs::read_to_string(browser_dir(&dir).join("tabs.jsonl")).unwrap();
        assert_eq!(tabs_before, tabs_after, "a forget rewrote the tab strip");
        let hist = std::fs::read_to_string(browser_dir(&dir).join("history.jsonl")).unwrap();
        assert!(!hist.contains("secret.test"), "{hist}");
        // The tab is still at the url it was at — a tab's own address is
        // not history, and forgetting history does not close tabs.
        assert_eq!(store.tabs.tabs["t"].url(), Some("https://keep.test/"));
    }

    /// G1, at the store: the whole point is that the answer is still there
    /// after the process that took it has gone.
    #[test]
    fn a_permission_answered_once_is_still_answered_after_a_restart() {
        use permissions::{Decision, PermissionEvent, PermissionKind, Verdict};
        let dir = scratch();
        {
            let (mut store, _) = BrowserStore::open(&dir).unwrap();
            assert!(store
                .permission_event(PermissionEvent::Decided {
                    origin: "https://cam.test/room/7".into(),
                    permission: PermissionKind::Camera,
                    decision: Decision::Allow,
                    ts_ms: 9,
                })
                .unwrap());
            store.sync().unwrap();
        }
        let (mut store, report) = BrowserStore::open(&dir).unwrap();
        assert!(report.stated().is_empty(), "{:?}", report.stated());
        assert_eq!(
            store
                .permissions
                .verdict("https://cam.test", &[PermissionKind::Camera]),
            Verdict::Allow
        );

        // And a revoke actually leaves the disk, rather than being
        // tombstoned after a grant that stays readable.
        let file = browser_dir(&dir).join("permissions.jsonl");
        assert!(std::fs::read_to_string(&file).unwrap().contains("cam.test"));
        assert!(store
            .permission_event(PermissionEvent::Forgotten {
                origin: "https://cam.test".into(),
                permission: None,
            })
            .unwrap());
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(!after.contains("cam.test"), "{after}");
        assert!(store.permissions.is_empty());
    }

    /// A permission write must not rewrite anything else, for the same
    /// reason a forget must not rewrite the strip: four lifetimes, four
    /// files.
    #[test]
    fn answering_a_permission_does_not_rewrite_the_tab_strip() {
        use permissions::{Decision, PermissionEvent, PermissionKind};
        let dir = scratch();
        let (mut store, _) = BrowserStore::open(&dir).unwrap();
        open_tab(&mut store, "t", "https://keep.test/");
        store.sync().unwrap();
        let before = std::fs::read_to_string(browser_dir(&dir).join("tabs.jsonl")).unwrap();
        store
            .permission_event(PermissionEvent::Decided {
                origin: "https://keep.test".into(),
                permission: PermissionKind::Notifications,
                decision: Decision::Deny,
                ts_ms: 3,
            })
            .unwrap();
        let after = std::fs::read_to_string(browser_dir(&dir).join("tabs.jsonl")).unwrap();
        assert_eq!(before, after, "a permission answer rewrote the tab strip");
    }

    #[test]
    fn an_incognito_tab_is_never_written_to_the_log() {
        let dir = scratch();
        let (mut store, _) = BrowserStore::open(&dir).unwrap();
        assert!(store
            .tab_event(TabEvent::Opened {
                window: tabs::PRIMARY_WINDOW.to_string(),
                id: "secret".into(),
                group: None,
                index: usize::MAX,
                content: TabContent::Page,
                opener: None,
                url: Some("https://private.example/".into()),
                incognito: true,
                ts_ms: 1,
            })
            .unwrap());
        store.sync().unwrap();
        let tabs = std::fs::read_to_string(browser_dir(&dir).join("tabs.jsonl")).unwrap();
        assert!(
            !tabs.contains("private.example"),
            "an incognito url landed in the tab log: {tabs}"
        );
        assert!(
            !tabs.contains("secret"),
            "an incognito tab id landed in the tab log: {tabs}"
        );
        assert!(store.tabs.tabs["secret"].incognito);
        assert_eq!(
            store.tabs.tabs["secret"].url(),
            Some("https://private.example/")
        );

        // A restart must not restore it. The threat this is immune to is
        // "we skipped the append but the fold still wrote it on compact".
        drop(store);
        let (store, _) = BrowserStore::open(&dir).unwrap();
        assert!(
            !store.tabs.tabs.contains_key("secret"),
            "an incognito tab survived a restart"
        );
    }

    #[test]
    fn import_batch_files_a_tree_and_history_in_one_write() {
        let dir = scratch();
        let (mut store, _) = BrowserStore::open(&dir).unwrap();
        let bm = vec![
            bookmarks::BookmarkEvent::Added {
                id: "wrap".into(),
                parent: None,
                index: 0,
                title: Some("From Chrome".into()),
                url: None,
                ts_ms: 1,
            },
            bookmarks::BookmarkEvent::Added {
                id: "link".into(),
                parent: Some("wrap".into()),
                index: 0,
                title: Some("Rust".into()),
                url: Some("https://www.rust-lang.org/".into()),
                ts_ms: 1,
            },
        ];
        let hist = vec![history::HistoryEvent::Imported {
            url: "https://hist.example/".into(),
            title: Some("Hist".into()),
            visit_count: 4,
            last_visit_ms: 9,
        }];
        let (n_bm, n_hist) = store.import_batch(&bm, &hist).unwrap();
        assert_eq!(n_bm, 2);
        assert_eq!(n_hist, 1);
        assert_eq!(store.bookmarks.len(), 2);
        assert_eq!(store.bookmarks.roots, vec!["wrap".to_string()]);
        let e = store
            .history
            .index()
            .entries()
            .iter()
            .find(|e| e.url == "https://hist.example/")
            .expect("imported history missing");
        assert_eq!(e.visit_count, 4);
    }
}
