//! Browser state: tabs, groups, bookmarks, history.
//!
//! **Data only.** Nothing here fetches a page, renders one, or knows which
//! engine will. Every page fact arrives as an event that something else
//! states, which is the point: this is the half of a browser that does not
//! care whether Chromium, WebKit, or a wry webview wins the spike.
//!
//! # Layout on disk
//! ```text
//! <data_dir>/browser/tabs.jsonl        session restore: tabs, groups, ordering
//! <data_dir>/browser/bookmarks.jsonl   the hierarchy
//! <data_dir>/browser/history.jsonl     the visit log
//! ```
//!
//! Three files, not one, and the reason is that they have three different
//! lifetimes and three different failure consequences. Tabs churn every few
//! seconds and are worth little individually; bookmarks change rarely and
//! are worth a great deal each; history is the largest by orders of
//! magnitude and is the only one a person ever asks to have destroyed.
//! Sharing one file would make the cheapest writer set the durability
//! policy for the most valuable data in it, and would make forgetting one
//! url rewrite the tab strip.
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
pub mod history;
pub mod log;
pub mod snapshot;
pub mod tabs;

use std::io;
use std::path::{Path, PathBuf};

use bookmarks::{BookmarkEvent, BookmarkTree};
use history::{History, HistoryEvent};
use log::{EventLog, LoadReport};
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
}

impl OpenReport {
    /// One line per log that has something to say, or empty when all three
    /// loaded cleanly.
    pub fn stated(&self) -> Vec<String> {
        [
            ("tabs", &self.tabs),
            ("bookmarks", &self.bookmarks),
            ("history", &self.history),
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
    pub tabs: TabsState,
    pub bookmarks: BookmarkTree,
    pub history: History,
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

        let mut store = Self {
            tabs_log,
            bookmarks_log,
            history_log,
            tabs,
            bookmarks,
            history,
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
        };
        Ok((store, report))
    }

    /// True when any log is from a format this build must not write.
    pub fn read_only(&self) -> bool {
        self.tabs_log.sealed() || self.bookmarks_log.sealed() || self.history_log.sealed()
    }

    /// Apply and journal one tab event. Returns whether anything changed;
    /// a no-op is neither written nor broadcast.
    pub fn tab_event(&mut self, event: TabEvent) -> io::Result<bool> {
        if !self.tabs.apply(&event) {
            return Ok(false);
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

    /// Push every buffered write to the OS and fsync. Called at shutdown
    /// and wherever the engine wants a known-durable point.
    pub fn sync(&mut self) -> io::Result<()> {
        self.tabs_log.sync()?;
        self.bookmarks_log.sync()?;
        self.history_log.sync()
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
                id: id.into(),
                group: None,
                index: usize::MAX,
                content: TabContent::Page,
                opener: None,
                url: Some(url.into()),
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

    /// The three logs are three files with three lifetimes. Forgetting a
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
}
