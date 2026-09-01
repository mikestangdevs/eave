//! Slice B1 acceptance: browser state, against a REAL engine process.
//!
//! Everything asserted here goes over the socket to a spawned
//! `eave-engined`, because the claims are about what survives a restart and
//! what crosses a wire — neither of which an in-process test can observe.
//! The unit tests in `eave_core::browser` cover the folds; this file covers
//! the engine.
//!
//! Harness rules are `wire.rs`'s: data dirs under `/tmp/eave-wire-tests`
//! (the `sun_path` limit is ~104 bytes), workspace-built bins.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use eave_engine::client::Client;
use eave_wire::{
    socket_path, BookmarksSnapshot, LoadState, PageFact, PermissionDecision, PermissionKind,
    PermissionsSnapshot, Request, ResponseBody, ResponseEnvelope, Setting, Slot, SubscriptionItem,
    TabContent, TabsSnapshot,
};

/// Navigate the way the world works after Slice C1: the operator asks, and
/// a page engine answers.
///
/// B1's `TabNavigate` committed on its own, because nothing was attached to
/// say what landed. It is now a request, so a test that wants a page to
/// have LANDED has to say so — and these two frames are exactly the two the
/// CEF host sends for every real navigation.
///
/// Tests that are about the *seam* send the two halves by hand. This helper
/// is for the ones that are about something else (history, the nav stack)
/// and merely need a page to be there.
async fn land(client: &Client, tab: &str, url: &str) {
    client
        .browser_do(Request::TabNavigate {
            tab_id: tab.to_string(),
            url: url.to_string(),
        })
        .await
        .unwrap();
    client
        .browser_do(Request::TabStated {
            tab_id: tab.to_string(),
            fact: PageFact::Committed {
                url: url.to_string(),
                title: None,
            },
        })
        .await
        .unwrap();
}

fn engined_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"))
}

fn fake_agent_bin() -> PathBuf {
    let path = engined_bin().parent().expect("bin dir").join("fake-agent");
    assert!(
        path.exists(),
        "fake-agent not built at {path:?} — run `cargo test` from the workspace root"
    );
    path
}

fn scratch() -> PathBuf {
    let dir = PathBuf::from("/tmp/eave-wire-tests").join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let child = Command::new(engined_bin())
            .args(["--data-dir", &data_dir.display().to_string()])
            // A test engine must never inherit the DEVELOPER'S checkout as
            // its default cwd: a session opened without one would then be
            // a session on the real repo, and since AG1 #11 that means a
            // checkpoint ref written into the shared git dir on every
            // send. (It also hashed the whole eave work tree for nothing.)
            .current_dir(data_dir)
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", "linger")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn eave-engined");
        let me = Self {
            child,
            data_dir: data_dir.to_path_buf(),
        };
        me.await_socket();
        me
    }

    fn await_socket(&self) {
        let sock = socket_path(&self.data_dir);
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "engine never opened its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Stop the engine the way a crash would, so nothing gets a chance to
    /// tidy up on the way out. A restart test that let the engine flush
    /// gracefully would be testing the wrong thing.
    fn sigkill(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        // The socket file outlives the process; a restart binds a fresh
        // one, so leaving it would let `await_socket` succeed against a
        // dead engine.
        let _ = std::fs::remove_file(socket_path(&self.data_dir));
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

/// The next tab snapshot on a `watch_tabs` stream.
async fn next_tabs(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
) -> TabsSnapshot {
    let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("tabs frame in time")
        .expect("tabs stream open");
    match item {
        SubscriptionItem::Tabs { snapshot } => snapshot,
        other => panic!("expected a tabs frame, got {other:?}"),
    }
}

async fn next_bookmarks(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
) -> BookmarksSnapshot {
    let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("bookmarks frame in time")
        .expect("bookmarks stream open");
    match item {
        SubscriptionItem::Bookmarks { snapshot } => snapshot,
        other => panic!("expected a bookmarks frame, got {other:?}"),
    }
}

async fn next_permissions(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
) -> PermissionsSnapshot {
    let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("permissions frame in time")
        .expect("permissions stream open");
    match item {
        SubscriptionItem::Permissions { snapshot } => snapshot,
        other => panic!("expected a permissions frame, got {other:?}"),
    }
}

/// Read frames until one satisfies `f`, or time out saying what was seen.
/// Snapshots coalesce, so a test must never assume the frame it wants is
/// the very next one — it must assume only that one arrives.
async fn tabs_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    what: &str,
    mut f: impl FnMut(&TabsSnapshot) -> bool,
) -> TabsSnapshot {
    let end = Instant::now() + Duration::from_secs(8);
    let mut last = None;
    while Instant::now() < end {
        let snap = next_tabs(rx).await;
        if f(&snap) {
            return snap;
        }
        last = Some(snap);
    }
    panic!("never saw {what}; last snapshot was {last:#?}");
}

// ---------------------------------------------------------------------------

/// The slice's first claim: tabs and groups survive a restart, and the
/// restart is a `kill -9`, not a shutdown.
///
/// Also the slice's durability LINE, in the same test: the strip comes back
/// with its urls, titles, pin, group and ordering — and every tab reads
/// `Unloaded`, because a tab restored off disk is not loading. Journaling
/// the load state would restore a strip full of tabs claiming to be busy,
/// which is the exact lie `Attention` is kept out of the journal to avoid.
#[tokio::test]
async fn the_strip_survives_a_kill_9_and_comes_back_unloaded() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let group = client
        .browser_create(Request::GroupCreate {
            window: None,
            title: Some("work".into()),
            color: Some("blue".into()),
            workspace: Some("/repo/eave".into()),
            index: None,
        })
        .await
        .unwrap();
    let loose = client
        .tab_open(None, Some("https://example.com/"), true)
        .await
        .unwrap();
    let grouped = client
        .browser_create(Request::TabOpen {
            window: None,
            url: Some("https://docs.rs/serde".into()),
            group: Some(group.clone()),
            index: None,
            content: TabContent::Page,
            opener: None,
            activate: false,
            incognito: false,
        })
        .await
        .unwrap();
    assert!(client
        .browser_do(Request::TabPin {
            tab_id: loose.clone(),
            pinned: true
        })
        .await
        .unwrap());
    // A page engine states a title, and says the page is loading.
    client
        .browser_do(Request::TabStated {
            tab_id: grouped.clone(),
            fact: PageFact::Titled {
                title: Some("serde — Rust".into()),
            },
        })
        .await
        .unwrap();
    client
        .browser_do(Request::TabStated {
            tab_id: grouped.clone(),
            fact: PageFact::Load {
                load: LoadState::Loading,
                can_go_back: None,
                can_go_forward: None,
            },
        })
        .await
        .unwrap();

    let before = {
        let mut rx = client.watch_tabs().await.unwrap();
        tabs_until(&mut rx, "the loading tab", |s| {
            s.tabs.iter().any(|t| t.load == LoadState::Loading)
        })
        .await
    };
    assert_eq!(before.tabs.len(), 2);
    assert_eq!(before.active.as_deref(), Some(loose.as_str()));

    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    let mut rx = client.watch_tabs().await.unwrap();
    let after = next_tabs(&mut rx).await;

    assert_eq!(after.tabs.len(), 2, "both tabs came back");
    let by_id = |id: &str| {
        after
            .tabs
            .iter()
            .find(|t| t.id == id)
            .expect("tab present")
            .clone()
    };
    let l = by_id(&loose);
    let g = by_id(&grouped);
    assert_eq!(l.url.as_deref(), Some("https://example.com/"));
    assert!(l.pinned, "the pin is durable");
    assert_eq!(
        g.group.as_deref(),
        Some(group.as_str()),
        "group membership is durable"
    );
    assert_eq!(
        g.title.as_deref(),
        Some("serde — Rust"),
        "the title is durable"
    );
    assert_eq!(
        after.active.as_deref(),
        Some(loose.as_str()),
        "the active tab is durable"
    );
    assert_eq!(after.groups.len(), 1);
    assert_eq!(after.groups[0].title.as_deref(), Some("work"));
    assert_eq!(
        after.groups[0].workspace.as_deref(),
        Some("/repo/eave"),
        "the group's stated checkout is durable"
    );
    // THE line: transient state did not come back.
    assert!(
        after.tabs.iter().all(|t| t.load == LoadState::Unloaded),
        "a restored tab claimed to be loading — load state must never be journaled: {:#?}",
        after.tabs
    );
    // ...and the ordering came back as ONE list holding both kinds of
    // slot, in the order they were created: the group was made first, so
    // it is first, and the ungrouped tab sits after it. Two orderings —
    // one for groups, one for loose tabs — could not express this.
    assert_eq!(after.order.len(), 2, "{:#?}", after.order);
    assert!(
        matches!(&after.order[0], Slot::Group { id } if id == &group),
        "{:#?}",
        after.order
    );
    assert!(
        matches!(&after.order[1], Slot::Tab { id } if id == &loose),
        "{:#?}",
        after.order
    );
    assert_eq!(after.group_tabs.len(), 1);
    assert_eq!(after.group_tabs[0].1, vec![grouped.clone()]);
}

/// **The decision this slice had to make, asserted.**
///
/// A tab may VIEW a session. Closing that tab closes the view and nothing
/// else: the session is still there, still listed, still its own durable
/// thing. If this ever fails, sessions have been collapsed into tabs and
/// the rail has stopped being the system of record.
#[tokio::test]
async fn closing_a_tab_that_views_a_session_does_not_touch_the_session() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let session = client
        .open(None, Some(dir.display().to_string()))
        .await
        .unwrap();
    let sid = session.session_id.clone();
    let tab = client.tab_open_session(&sid, None, None).await.unwrap();

    let mut rx = client.watch_tabs().await.unwrap();
    let snap = next_tabs(&mut rx).await;
    let viewing = snap
        .tabs
        .iter()
        .find(|t| t.id == tab)
        .expect("the session tab");
    assert_eq!(
        viewing.content,
        TabContent::Session {
            session_id: sid.clone()
        },
        "the tab names the session"
    );
    assert_eq!(viewing.url, None, "a session tab is not at a url");

    assert!(client
        .browser_do(Request::TabClose { tab_id: tab })
        .await
        .unwrap());
    let after = tabs_until(&mut rx, "the tab gone", |s| s.tabs.is_empty()).await;
    assert!(after.tabs.is_empty());

    // The session is untouched: same id, same state, still listed.
    let sessions = client.list().await.unwrap();
    let still = sessions
        .iter()
        .find(|s| s.session_id == sid)
        .expect("closing a tab ENDED a session — sessions are not tabs");
    assert_eq!(still.state, session.state);
    assert!(!still.archived, "closing a tab archived a session");
    // And its own directory is still on disk.
    assert!(dir
        .join("sessions")
        .join(&sid)
        .join("journal.jsonl")
        .exists());
}

/// Closing a whole GROUP takes its tabs with it — and still does not touch
/// any session those tabs viewed. The stronger form of the test above,
/// because a group close is the one operation that removes tabs the caller
/// did not name.
#[tokio::test]
async fn closing_a_group_takes_its_tabs_and_leaves_its_sessions() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let session = client
        .open(None, Some(dir.display().to_string()))
        .await
        .unwrap();
    let group = client
        .browser_create(Request::GroupCreate {
            window: None,
            title: Some("eave".into()),
            color: None,
            workspace: None,
            index: None,
        })
        .await
        .unwrap();
    client
        .tab_open_session(&session.session_id, Some(&group), None)
        .await
        .unwrap();
    client
        .browser_create(Request::TabOpen {
            window: None,
            url: Some("https://example.com/".into()),
            group: Some(group.clone()),
            index: None,
            content: TabContent::Page,
            opener: None,
            activate: false,
            incognito: false,
        })
        .await
        .unwrap();

    let mut rx = client.watch_tabs().await.unwrap();
    tabs_until(&mut rx, "two grouped tabs", |s| s.tabs.len() == 2).await;

    assert!(client
        .browser_do(Request::GroupClose {
            group_id: group.clone()
        })
        .await
        .unwrap());
    let after = tabs_until(&mut rx, "the group gone", |s| s.groups.is_empty()).await;
    assert!(after.tabs.is_empty(), "the group's tabs went with it");

    assert!(
        client
            .list()
            .await
            .unwrap()
            .iter()
            .any(|s| s.session_id == session.session_id),
        "closing a group ended a session"
    );
}

/// Back and forward are durable, and a navigation from mid-stack truncates
/// the forward entries — the standard rule, and the reason a back-then-click
/// loses the forward arrow.
///
/// The stack itself is NOT on the strip snapshot: the snapshot carries two
/// booleans and a count, and the entries come from `tab_nav`. That
/// separation is asserted here too, because it is the thing that keeps one
/// keystroke from re-broadcasting every open tab's whole history.
#[tokio::test]
async fn back_and_forward_survive_a_restart_and_stay_off_the_snapshot() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client
        .tab_open(None, Some("https://a.example/"), true)
        .await
        .unwrap();
    for url in ["https://b.example/", "https://c.example/"] {
        land(&client, &tab, url).await;
    }
    // Two steps back, then a new navigation: c must be gone.
    assert!(client
        .browser_do(Request::TabGo {
            tab_id: tab.clone(),
            delta: -1
        })
        .await
        .unwrap());
    let nav = client.tab_nav(&tab).await.unwrap();
    assert_eq!(nav.current_entry().unwrap().url, "https://b.example/");
    assert!(nav.can_go_forward());

    land(&client, &tab, "https://d.example/").await;
    let nav = client.tab_nav(&tab).await.unwrap();
    assert_eq!(
        nav.entries
            .iter()
            .map(|e| e.url.as_str())
            .collect::<Vec<_>>(),
        vec![
            "https://a.example/",
            "https://b.example/",
            "https://d.example/"
        ],
        "navigating from mid-stack must drop the forward entries"
    );
    assert!(!nav.can_go_forward());

    // A move past the end is clamped and reported as no change, not an
    // error — the operator may have raced a navigation.
    assert!(
        !client
            .browser_do(Request::TabGo {
                tab_id: tab.clone(),
                delta: 5
            })
            .await
            .unwrap(),
        "a clamped traversal must report changed:false, not fail"
    );

    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let nav = client.tab_nav(&tab).await.unwrap();
    assert_eq!(nav.entries.len(), 3, "the stack survived the restart");
    assert_eq!(nav.current, 2);
    assert!(nav.can_go_back());

    let mut rx = client.watch_tabs().await.unwrap();
    let snap = next_tabs(&mut rx).await;
    let t = &snap.tabs[0];
    assert!(t.can_go_back, "the strip states the back arrow");
    assert!(!t.can_go_forward);
    assert_eq!(t.history_len, 3, "the strip states the count");
    // The entries themselves must not be on the wire here.
    let json = serde_json::to_string(&snap).unwrap();
    assert!(
        !json.contains("a.example"),
        "the back/forward stack rode the strip snapshot: {json}"
    );
}

/// Bookmarks: add, rename, move, remove — hierarchical, durable, and a
/// folder takes its subtree with it.
#[tokio::test]
async fn bookmarks_are_a_hierarchy_that_survives_a_restart() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let folder = client
        .browser_create(Request::BookmarkAdd {
            parent: None,
            index: None,
            title: Some("rust".into()),
            url: None,
        })
        .await
        .unwrap();
    let link = client
        .browser_create(Request::BookmarkAdd {
            parent: Some(folder.clone()),
            index: None,
            title: Some("serde".into()),
            url: Some("https://serde.rs/".into()),
        })
        .await
        .unwrap();
    let loose = client
        .browser_create(Request::BookmarkAdd {
            parent: None,
            index: None,
            title: None,
            url: Some("https://example.com/".into()),
        })
        .await
        .unwrap();

    assert!(client
        .browser_do(Request::BookmarkRename {
            bookmark_id: link.clone(),
            title: Some("  Serde\ndocs  ".into())
        })
        .await
        .unwrap());
    // Renaming to the name it already has is not a change and must not
    // re-broadcast the tree.
    assert!(
        !client
            .browser_do(Request::BookmarkRename {
                bookmark_id: link.clone(),
                title: Some("Serde docs".into())
            })
            .await
            .unwrap(),
        "a no-op rename reported changed:true"
    );

    // A folder cannot be moved inside itself, and the refusal says so.
    let err = client
        .browser_do(Request::BookmarkMove {
            bookmark_id: folder.clone(),
            parent: Some(folder.clone()),
            index: 0,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("inside itself"), "{err}");

    // Move the loose link into the folder.
    assert!(client
        .browser_do(Request::BookmarkMove {
            bookmark_id: loose.clone(),
            parent: Some(folder.clone()),
            index: 0
        })
        .await
        .unwrap());

    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    let mut rx = client.watch_bookmarks().await.unwrap();
    let snap = next_bookmarks(&mut rx).await;

    assert_eq!(snap.nodes.len(), 3);
    assert_eq!(snap.nodes[0].bookmark.id, folder, "the folder is first");
    assert_eq!(snap.nodes[0].depth, 0);
    assert_eq!(snap.nodes[0].children, 2);
    assert_eq!(
        snap.nodes[1].bookmark.id, loose,
        "moved to index 0 inside it"
    );
    assert_eq!(snap.nodes[1].depth, 1);
    assert_eq!(snap.nodes[2].bookmark.id, link);
    assert_eq!(
        snap.nodes[2].bookmark.title.as_deref(),
        Some("Serde docs"),
        "the rename was normalized to one line and is durable"
    );

    // Removing the folder takes the subtree.
    assert!(client
        .browser_do(Request::BookmarkRemove {
            bookmark_id: folder
        })
        .await
        .unwrap());
    let mut rx = client.watch_bookmarks().await.unwrap();
    let snap = next_bookmarks(&mut rx).await;
    assert!(snap.nodes.is_empty(), "the subtree went with the folder");
}

/// History records what was visited, answers a typeahead, and forgets
/// PHYSICALLY: after a forget, the url is not in the file on disk.
///
/// The last assertion is the one that matters. A tombstone would leave the
/// url sitting in plain text under somebody's name, and "I deleted that"
/// would be false.
#[tokio::test]
async fn history_answers_typeahead_and_forgets_from_the_file_itself() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client
        .tab_open(None, Some("https://github.com/eave"), true)
        .await
        .unwrap();
    client
        .browser_do(Request::TabStated {
            tab_id: tab.clone(),
            fact: PageFact::Titled {
                title: Some("eave on GitHub".into()),
            },
        })
        .await
        .unwrap();
    for url in [
        "https://gitlab.com/x",
        "https://docs.rs/serde",
        "https://secret.example/very-private-page",
    ] {
        land(&client, &tab, url).await;
    }

    // Typing an address prefix.
    let hits = client.history_query("git", None).await.unwrap();
    let urls: Vec<&str> = hits.matches.iter().map(|m| m.entry.url.as_str()).collect();
    assert!(urls.contains(&"https://github.com/eave"), "{urls:?}");
    assert!(urls.contains(&"https://gitlab.com/x"), "{urls:?}");
    assert!(!urls.contains(&"https://docs.rs/serde"), "{urls:?}");
    assert_eq!(hits.total_entries, 4);

    // A full url with its scheme still matches the entry stored short.
    let hits = client.history_query("https://docs.rs", None).await.unwrap();
    assert_eq!(hits.matches.len(), 1);
    assert_eq!(hits.matches[0].entry.url, "https://docs.rs/serde");

    // A title word.
    let hits = client.history_query("eave", None).await.unwrap();
    assert!(hits
        .matches
        .iter()
        .any(|m| m.entry.url == "https://github.com/eave"));

    // Nothing typed suggests nothing — not everything.
    assert!(client
        .history_query("   ", None)
        .await
        .unwrap()
        .matches
        .is_empty());

    // Now forget one url.
    assert!(client
        .browser_do(Request::HistoryForget {
            url: "https://secret.example/very-private-page".into()
        })
        .await
        .unwrap());
    let hits = client.history_query("secret", None).await.unwrap();
    assert!(
        hits.matches.is_empty(),
        "forgotten url still answered a query"
    );

    // THE assertion: it is gone from the bytes on disk, before AND after a
    // restart. Read the file directly — asking the engine would only prove
    // the engine's memory forgot.
    let path = dir.join("browser").join("history.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains("very-private-page"),
        "a forgotten url is still in {}: {raw}",
        path.display()
    );
    assert!(
        raw.contains("docs.rs"),
        "the forget took everything with it"
    );

    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    assert!(client
        .history_query("secret", None)
        .await
        .unwrap()
        .matches
        .is_empty());
    assert_eq!(
        client
            .history_query("git", None)
            .await
            .unwrap()
            .matches
            .len(),
        2,
        "the rest of history survived the restart"
    );
}

/// A range clear forgets a window and keeps the rest — and the visits are
/// gone from the file, not tombstoned.
#[tokio::test]
async fn clearing_a_range_forgets_that_window_only() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client
        .tab_open(None, Some("https://alpha.example/"), true)
        .await
        .unwrap();
    // Everything so far is "old". Take the boundary from the engine's own
    // clock rather than the test's: the engine stamps the visits, so a
    // boundary the test invented could fall on the wrong side of them.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let boundary = eave_core::journal::now_ms();
    tokio::time::sleep(Duration::from_millis(30)).await;
    land(&client, &tab, "https://beta.example/").await;

    assert!(client
        .browser_do(Request::HistoryClear {
            since_ms: Some(boundary),
            before_ms: None
        })
        .await
        .unwrap());

    // Queried by prefix, one url at a time: the index matches PREFIXES of
    // an address, so "example" would match neither of these hosts. That is
    // a stated limitation of the index, not an accident — see the slice
    // report.
    let kept = client.history_query("alpha", Some(50)).await.unwrap();
    assert_eq!(
        kept.matches
            .iter()
            .map(|m| m.entry.url.as_str())
            .collect::<Vec<_>>(),
        vec!["https://alpha.example/"],
        "the clear took a visit from before its window"
    );
    let gone = client.history_query("beta", Some(50)).await.unwrap();
    assert!(
        gone.matches.is_empty(),
        "the clear missed its own window: {gone:?}"
    );
    let raw = std::fs::read_to_string(dir.join("browser").join("history.jsonl")).unwrap();
    assert!(!raw.contains("beta.example"), "{raw}");
    assert!(raw.contains("alpha.example"), "{raw}");
}

/// The strip snapshot is a snapshot: full state, fold by replacement, one
/// on subscribe and one after every change. Also: a change that changes
/// nothing is not broadcast.
#[tokio::test]
async fn the_strip_is_a_snapshot_and_a_no_op_is_not_one() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let mut rx = client.watch_tabs().await.unwrap();
    let first = next_tabs(&mut rx).await;
    assert!(first.tabs.is_empty(), "a fresh engine has no tabs");

    let tab = client
        .tab_open(None, Some("https://example.com/"), true)
        .await
        .unwrap();
    tabs_until(&mut rx, "the new tab", |s| s.tabs.len() == 1).await;

    // Pin it, then pin it again. The second is a no-op: it must report
    // changed:false AND must not produce a frame.
    assert!(client
        .browser_do(Request::TabPin {
            tab_id: tab.clone(),
            pinned: true
        })
        .await
        .unwrap());
    tabs_until(&mut rx, "the pin", |s| s.tabs[0].pinned).await;
    assert!(
        !client
            .browser_do(Request::TabPin {
                tab_id: tab.clone(),
                pinned: true
            })
            .await
            .unwrap(),
        "re-pinning an already-pinned tab reported a change"
    );
    // Give a frame time to arrive if one were coming. Nothing should.
    assert!(
        tokio::time::timeout(Duration::from_millis(400), rx.recv())
            .await
            .is_err(),
        "a no-op re-broadcast the whole strip"
    );

    // An unknown tab is a no-op too, not an error: the operator may have
    // clicked a row that closed under them.
    assert!(!client
        .browser_do(Request::TabPin {
            tab_id: "no-such-tab".into(),
            pinned: true
        })
        .await
        .unwrap());
}

/// Groups: three states for a field, not two. Absent leaves it alone,
/// `clear` clears it, `value` sets it — which is why `Setting` exists.
#[tokio::test]
async fn a_group_update_can_leave_alone_set_and_clear() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let group = client
        .browser_create(Request::GroupCreate {
            window: None,
            title: Some("one".into()),
            color: Some("blue".into()),
            workspace: Some("/repo".into()),
            index: None,
        })
        .await
        .unwrap();
    let mut rx = client.watch_tabs().await.unwrap();
    tabs_until(&mut rx, "the group", |s| s.groups.len() == 1).await;

    // Set the title, leave the colour alone, clear the workspace.
    assert!(client
        .browser_do(Request::GroupUpdate {
            group_id: group.clone(),
            title: Some(Setting::Value {
                value: "two".into()
            }),
            color: None,
            workspace: Some(Setting::Clear),
            collapsed: Some(true),
        })
        .await
        .unwrap());
    let snap = tabs_until(&mut rx, "the updated group", |s| {
        s.groups[0].title.as_deref() == Some("two")
    })
    .await;
    let g = &snap.groups[0];
    assert_eq!(
        g.color.as_deref(),
        Some("blue"),
        "an absent field was not left alone"
    );
    assert_eq!(g.workspace, None, "clear did not clear");
    assert!(g.collapsed);
}

/// The isolation canary, in the shape `watch.rs` established: a raw socket
/// collects every byte the engine writes on a `watch_tabs` id while a
/// session streams canary-marked transcript content. None of it may appear.
///
/// `watch_tabs` is engine-wide, exactly like `watch`, so it inherits the
/// same rule (spec 05 §Watch): a cross-session broadcast may carry facts
/// ABOUT things and never a byte of any session's transcript. The obvious
/// way to break it is to name a session tab after the first words of its
/// prompt.
#[tokio::test]
async fn the_tabs_channel_never_carries_transcript_content() {
    const CANARY: &str = "ZZCANARYZZ-tabs-must-never-see-this";

    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);

    // Raw socket: hello, then watch_tabs, then collect every byte.
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .unwrap();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    lines.next_line().await.unwrap().expect("hello reply");
    w.write_all(b"{\"id\":2,\"method\":\"watch_tabs\"}\n")
        .await
        .unwrap();

    let collected = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&collected);
    let reader = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            sink.lock().unwrap().push_str(&line);
            sink.lock().unwrap().push('\n');
        }
    });

    // Meanwhile: a session gets a canary-marked prompt, and a tab views it.
    let client = Client::connect(&dir).await.unwrap();
    let session = client
        .open(None, Some(dir.display().to_string()))
        .await
        .unwrap();
    let sid = session.session_id.clone();
    client.tab_open_session(&sid, None, None).await.unwrap();
    client
        .send(&sid, &format!("{CANARY} please do nothing"))
        .await
        .unwrap();
    // Let the turn stream and the strip re-broadcast several times.
    for _ in 0..3 {
        client
            .tab_open(None, Some("https://example.com/"), false)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    let seen = collected.lock().unwrap().clone();
    reader.abort();

    // Positive control: the canary IS in the session's own transcript, so
    // a passing test is not passing because nothing happened.
    let mut sub = client.subscribe(&sid, None).await.unwrap();
    let mut transcript_bytes = String::new();
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end && !transcript_bytes.contains(CANARY) {
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
            transcript_bytes.push_str(&serde_json::to_string(&item).unwrap());
        }
    }
    assert!(
        transcript_bytes.contains(CANARY),
        "the canary never reached the subscribe stream — this test would pass vacuously"
    );

    // Positive control 2: the tabs stream WAS producing frames.
    assert!(
        seen.contains("\"type\":\"tabs\""),
        "no tabs frames were collected at all: {seen}"
    );
    assert!(
        seen.contains(&sid),
        "the session tab should name its session by id"
    );

    // THE assertion.
    assert!(
        !seen.contains(CANARY),
        "transcript content crossed the tabs channel: {seen}"
    );
}

/// A newer client's browser method must not hang an older engine's caller,
/// and an older client's frames must still parse. The serde-default half is
/// unit-tested in `frames.rs`; this is the process-level half — an unknown
/// method gets an error ON ITS OWN ID.
#[tokio::test]
async fn an_unknown_browser_method_errors_on_its_own_id() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .unwrap();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    lines.next_line().await.unwrap().unwrap();
    w.write_all(b"{\"id\":77,\"method\":\"tab_teleport\",\"params\":{}}\n")
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().expect("a reply");
    let env: ResponseEnvelope = serde_json::from_str(&line).unwrap();
    assert_eq!(env.id, 77, "the error must carry the caller's own id");
    assert!(matches!(env.body, ResponseBody::Err { .. }));
}

/// **The seam B1 left open, now closed (Slice C1).**
///
/// B1's `tab_navigate` committed on its own, and said in its own report that
/// this had to change before a page engine attached. It has attached. If
/// the request still committed, every navigation would be recorded TWICE —
/// once when asked and once when the page reported what landed — which is
/// two nav entries, two history visits, and a back button that needs
/// pressing twice to go one page back.
///
/// So: asking does not commit. It records a request that whatever renders
/// pages can see, and the commit is what moves the tab.
#[tokio::test]
async fn a_navigation_is_a_request_until_a_page_says_otherwise() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client.tab_open(None, None, true).await.unwrap();
    let mut rx = client.watch_tabs().await.unwrap();
    tabs_until(&mut rx, "the new tab", |s| s.tabs.len() == 1).await;

    // Ask.
    assert!(client
        .browser_do(Request::TabNavigate {
            tab_id: tab.clone(),
            url: "https://asked.example/".into(),
        })
        .await
        .unwrap());

    let asked = tabs_until(&mut rx, "the request", |s| s.tabs[0].pending.is_some()).await;
    let info = &asked.tabs[0];
    assert_eq!(
        info.pending.as_ref().unwrap().url,
        "https://asked.example/",
        "the request must reach whatever renders pages — it is a client"
    );
    assert_eq!(
        info.load,
        LoadState::Loading,
        "asking is itself a load state; the tab should say so before any engine answers"
    );
    assert_eq!(
        info.url, None,
        "a tab must not claim to be showing a page nobody has fetched"
    );
    assert_eq!(info.history_len, 0, "the request pushed a nav entry");

    // The request is NOT durable. Nothing about it may reach the log.
    let raw = std::fs::read_to_string(dir.join("browser").join("tabs.jsonl")).unwrap();
    assert!(
        !raw.contains("asked.example"),
        "a navigation request was journaled; a restart would replay it: {raw}"
    );

    // Now a page engine answers — and answers with a DIFFERENT url, because
    // that is what a redirect is.
    assert!(client
        .browser_do(Request::TabStated {
            tab_id: tab.clone(),
            fact: PageFact::Committed {
                url: "https://landed.example/".into(),
                title: Some("Landed".into()),
            },
        })
        .await
        .unwrap());

    let landed = tabs_until(&mut rx, "the commit", |s| s.tabs[0].url.is_some()).await;
    let info = &landed.tabs[0];
    assert_eq!(info.url.as_deref(), Some("https://landed.example/"));
    assert_eq!(info.title.as_deref(), Some("Landed"));
    assert_eq!(
        info.history_len, 1,
        "exactly one entry: the ask and the commit are one navigation, not two"
    );
    assert!(
        info.pending.is_none(),
        "a commit answers the request even when it lands elsewhere — leaving it \
         set makes a renderer that watches it navigate again, which is a loop"
    );

    // And history recorded ONE visit, at the url that actually landed.
    let hits = client.history_query("landed", Some(50)).await.unwrap();
    assert_eq!(hits.matches.len(), 1, "{hits:?}");
    assert_eq!(
        hits.total_entries, 1,
        "the ask and the commit both journaled"
    );
    let asked_for = client
        .history_query("asked.example", Some(50))
        .await
        .unwrap();
    assert!(
        asked_for.matches.is_empty(),
        "history recorded an address that was never reached: {asked_for:?}"
    );
}

/// A typed address stays typed through the commit.
///
/// Not cosmetic: `Transition` is what ranks the omnibox, and `Typed` is
/// weighted above `Link` precisely because choosing an address is a
/// stronger signal than following one. Since every commit now arrives
/// through the same door a link click does, the provenance has to be
/// carried across the request rather than inferred at the far end —
/// otherwise the typeahead gets worse the more the address bar is used.
#[tokio::test]
async fn a_typed_address_is_still_typed_when_the_page_reports_back() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client.tab_open(None, None, true).await.unwrap();

    // Asked for: this is a typed visit.
    land(&client, &tab, "https://typed.example/").await;
    // Not asked for: the page navigated itself, so this is a link.
    client
        .browser_do(Request::TabStated {
            tab_id: tab.clone(),
            fact: PageFact::Committed {
                url: "https://followed.example/".into(),
                title: None,
            },
        })
        .await
        .unwrap();

    let raw = std::fs::read_to_string(dir.join("browser").join("history.jsonl")).unwrap();
    let typed_line = raw
        .lines()
        .find(|l| l.contains("typed.example"))
        .expect("no visit for the typed address");
    let followed_line = raw
        .lines()
        .find(|l| l.contains("followed.example"))
        .expect("no visit for the followed link");
    assert!(
        typed_line.contains("\"typed\""),
        "a typed address was recorded as something else: {typed_line}"
    );
    assert!(
        followed_line.contains("\"link\""),
        "a link was recorded as typed: {followed_line}"
    );
}

/// **Slice C5: a refresh is a reload, not a typed visit.**
///
/// Every commit arrives through the same door, and a reload arrives with a
/// `pending` request set — the reload button and the omnibox both go
/// through `tab_navigate`. So "answered a request" was true for a refresh
/// too, and every refresh was recorded `Typed`.
///
/// That is precisely the inflation `Transition::Reload` exists to prevent:
/// `counts_as_a_visit()` is false for it, so a page left open on a refresh
/// loop must not climb the typeahead. Before this fix it climbed once per
/// refresh, and the omnibox got worse the more a page was reloaded — which
/// is the opposite of what an address bar is for.
#[tokio::test]
async fn a_refresh_is_recorded_as_a_reload_and_does_not_inflate_the_typeahead() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client.tab_open(None, None, true).await.unwrap();
    // One typed visit, then the same address five more times: a refresh
    // loop, asked for exactly the way the omnibox asks.
    land(&client, &tab, "https://refreshed.example/").await;
    // A rival that was genuinely visited twice. It must outrank the page
    // that was reloaded six times.
    land(&client, &tab, "https://rival.example/").await;
    land(&client, &tab, "https://refreshed.example/").await;
    for _ in 0..5 {
        land(&client, &tab, "https://refreshed.example/").await;
    }

    let raw = std::fs::read_to_string(dir.join("browser").join("history.jsonl")).unwrap();
    let reloads = raw
        .lines()
        .filter(|l| l.contains("refreshed.example") && l.contains("\"reload\""))
        .count();
    let typed = raw
        .lines()
        .filter(|l| l.contains("refreshed.example") && l.contains("\"typed\""))
        .count();
    // Two genuine arrivals — the first, and the return from `rival` — and
    // five refreshes. Coming BACK to a page you left is a real visit; the
    // tab was showing something else, so the commit moves it. Only a
    // commit at the address the tab is already on is a reload, which is
    // why `is_refresh_of_current` reads the current entry and not the
    // stack.
    assert_eq!(
        typed, 2,
        "a genuine arrival was demoted to a reload:\n{raw}"
    );
    assert_eq!(reloads, 5, "refreshes were not recorded as reloads:\n{raw}");

    // The count, which is the fact the transition exists to protect. Seven
    // arrivals at that address; two of them were somebody going there.
    let hits = client
        .history_query("refreshed.example", Some(50))
        .await
        .unwrap();
    let refreshed = hits
        .matches
        .iter()
        .find(|m| m.entry.url == "https://refreshed.example/")
        .unwrap_or_else(|| panic!("no entry for the refreshed page: {hits:?}"));
    assert_eq!(
        refreshed.entry.visit_count, 2,
        "a refresh counted as a visit: {:?}",
        refreshed.entry
    );

    // And the tab did not accumulate a stack of identical entries. Three:
    // the page, the rival, the page again. Eight commits landed; before
    // this fix there were eight entries and the back button needed six
    // presses to leave a page the operator had refreshed five times.
    let nav = client.tab_nav(&tab).await.unwrap();
    assert_eq!(
        nav.entries.len(),
        3,
        "reloads pushed duplicate nav entries: {:?}",
        nav.entries.iter().map(|e| &e.url).collect::<Vec<_>>()
    );
}

/// **The 120 ms fold (Slice C1).** Load state is announced by pages, not by
/// people: one navigation is `Loading` then `Complete`, and a window of
/// tabs loading at once multiplies that. A snapshot is the WHOLE strip, so
/// paying one per spinner tick is the expensive way to redraw nothing.
///
/// Two properties, both borrowed from the record fold and both load-bearing:
/// a sustained storm of load changes is throttled, and anything structural
/// cuts the line.
#[tokio::test]
async fn load_state_folds_but_a_structural_change_cuts_the_line() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client.tab_open(None, None, true).await.unwrap();
    let mut rx = client.watch_tabs().await.unwrap();
    tabs_until(&mut rx, "the new tab", |s| s.tabs.len() == 1).await;

    // A storm of load-state changes, sustained past several windows. Every
    // one of these is a real change, so B1 would have sent a snapshot for
    // each.
    let storm = 40;
    let started = Instant::now();
    for i in 0..storm {
        client
            .browser_do(Request::TabStated {
                tab_id: tab.clone(),
                fact: PageFact::Load {
                    load: if i % 2 == 0 {
                        LoadState::Loading
                    } else {
                        LoadState::Complete
                    },
                    can_go_back: None,
                    can_go_forward: None,
                },
            })
            .await
            .unwrap();
    }
    let elapsed = started.elapsed();

    // Drain what actually arrived, allowing one more window to land.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut frames = 0;
    while tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .is_ok()
    {
        frames += 1;
    }

    // The bound the fold promises: at most one frame per window, plus a
    // leading edge and a trailing flush. Derived from the elapsed time
    // rather than a fixed number, because the storm's duration is what
    // decides how many windows it spans.
    let windows = (elapsed.as_millis() / 120) as usize + 3;
    assert!(
        frames <= windows,
        "{frames} snapshots for {storm} load changes over {elapsed:?} — \
         the fold is not folding (bound was {windows})"
    );

    // Now the other half: a structural change must NOT wait for a window.
    // Pin the tab and require the frame promptly — well inside the 120 ms
    // the load path would have cost.
    let pinned_at = Instant::now();
    assert!(client
        .browser_do(Request::TabPin {
            tab_id: tab.clone(),
            pinned: true
        })
        .await
        .unwrap());
    tabs_until(&mut rx, "the pin", |s| s.tabs[0].pinned).await;
    assert!(
        pinned_at.elapsed() < Duration::from_millis(120),
        "an operator action waited behind the spinner fold ({:?})",
        pinned_at.elapsed()
    );
}

/// Slice E2 — `can_go_back` / `can_go_forward` are engine facts, not a
/// hardcoded false. After two commits and a walk they match the stack;
/// a Load fact that carries the two bools overlays them onto the same
/// snapshot the pane header and the chords read.
#[tokio::test]
async fn load_facts_carry_history_flags_onto_the_snapshot() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let tab = client.tab_open(None, None, true).await.unwrap();
    land(&client, &tab, "https://a.example/").await;
    land(&client, &tab, "https://b.example/").await;

    let mut rx = client.watch_tabs().await.unwrap();
    let snap = tabs_until(&mut rx, "two commits", |s| {
        s.tabs.iter().any(|t| t.history_len == 2 && t.can_go_back)
    })
    .await;
    let t = snap.tabs.iter().find(|t| t.id == tab).unwrap();
    assert!(t.can_go_back, "two commits left the back arrow dead");
    assert!(!t.can_go_forward, "two commits invented a forward arrow");

    assert!(client
        .browser_do(Request::TabGo {
            tab_id: tab.clone(),
            delta: -1
        })
        .await
        .unwrap());
    let walked = tabs_until(&mut rx, "the walk", |s| {
        s.tabs.iter().any(|t| t.id == tab && t.can_go_forward)
    })
    .await;
    let t = walked.tabs.iter().find(|t| t.id == tab).unwrap();
    assert_eq!(t.url.as_deref(), Some("https://a.example/"));
    assert!(
        !t.can_go_back && t.can_go_forward,
        "the walk did not restate the stack on the snapshot: back={} fwd={}",
        t.can_go_back,
        t.can_go_forward
    );

    // Overlay: the page engine's flags survive onto the snapshot even
    // when they differ from the stack. The header and the chords read
    // this field, not a second copy.
    assert!(client
        .browser_do(Request::TabStated {
            tab_id: tab.clone(),
            fact: PageFact::Load {
                load: LoadState::Complete,
                can_go_back: Some(true),
                can_go_forward: Some(false),
            },
        })
        .await
        .unwrap());
    let overlay = tabs_until(&mut rx, "the overlay", |s| {
        s.tabs
            .iter()
            .any(|t| t.id == tab && t.can_go_back && !t.can_go_forward)
    })
    .await;
    let t = overlay.tabs.iter().find(|t| t.id == tab).unwrap();
    assert!(
        t.can_go_back && !t.can_go_forward,
        "Load history flags did not land on the snapshot the chords read: \
         back={} fwd={}",
        t.can_go_back,
        t.can_go_forward
    );
}

/// G1's whole claim, against a real engine: an answer given once is still
/// given after the process that took it has been `kill -9`ed.
///
/// The defect this replaces is worse than a missing feature. D9 built the
/// prompt and stored nothing, so eave asked for the camera on every page
/// load — which teaches the operator to click the primary button without
/// reading, the exact reflex a permission dialog exists to prevent.
#[tokio::test]
async fn a_permission_answer_survives_a_restart() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    // One grouped ask, answered once: both capabilities are recorded.
    assert!(client
        .browser_do(Request::PermissionSet {
            origin: "https://meet.example/room/7?x=1".into(),
            permissions: vec![PermissionKind::Camera, PermissionKind::Microphone],
            decision: PermissionDecision::Allow,
        })
        .await
        .unwrap());
    // A refusal is remembered as firmly as a grant — otherwise "don't
    // allow" is the one answer that still nags.
    assert!(client
        .browser_do(Request::PermissionSet {
            origin: "https://ads.example".into(),
            permissions: vec![PermissionKind::Notifications],
            decision: PermissionDecision::Deny,
        })
        .await
        .unwrap());
    // Re-answering the same way is not a change, so it neither appends a
    // line nor re-broadcasts. A page that re-asks on every load would
    // otherwise grow the file forever.
    assert!(
        !client
            .browser_do(Request::PermissionSet {
                origin: "https://meet.example/other".into(),
                permissions: vec![PermissionKind::Camera],
                decision: PermissionDecision::Allow,
            })
            .await
            .unwrap(),
        "a no-op answer reported changed:true"
    );

    // Rules 3 and 4: an origin with no honest key, and a capability eave
    // has no word for, are refused by the store rather than filed under a
    // guess. Both must go on being asked about.
    assert!(!client
        .browser_do(Request::PermissionSet {
            origin: "data:text/html,<b>hi".into(),
            permissions: vec![PermissionKind::Camera],
            decision: PermissionDecision::Allow,
        })
        .await
        .unwrap());
    assert!(!client
        .browser_do(Request::PermissionSet {
            origin: "https://odd.example".into(),
            permissions: vec![PermissionKind::Unknown(1 << 30)],
            decision: PermissionDecision::Allow,
        })
        .await
        .unwrap());

    drop(client);
    engine.sigkill();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    let mut rx = client.watch_permissions().await.unwrap();
    let snap = next_permissions(&mut rx).await;

    assert_eq!(snap.grants.len(), 3, "{:#?}", snap.grants);
    assert!(!snap.read_only);
    // Origin-ordered then capability-ordered, because the whole snapshot
    // is re-sent on every change and a set that reshuffled itself would
    // make every send look like a change.
    assert_eq!(snap.grants[0].origin, "https://ads.example");
    assert_eq!(snap.grants[0].permission, PermissionKind::Notifications);
    assert_eq!(snap.grants[0].decision, PermissionDecision::Deny);
    // The page-and-query part of the url is not part of the key: a
    // permission is per origin, which is what a person means by "this
    // site".
    assert_eq!(snap.grants[1].origin, "https://meet.example");
    assert_eq!(snap.grants[1].permission, PermissionKind::Camera);
    assert_eq!(snap.grants[2].origin, "https://meet.example");
    assert_eq!(snap.grants[2].permission, PermissionKind::Microphone);

    // A revoke takes the grant off the DISK, not just out of memory. A
    // tombstone would leave "this browser may use your camera" sitting in
    // plain text under somebody's name, and "I turned that off" would be
    // false — the same claim `HistoryForget` makes and for the same
    // reason.
    assert!(client
        .browser_do(Request::PermissionForget {
            origin: "https://meet.example".into(),
            permission: Some(PermissionKind::Camera),
        })
        .await
        .unwrap());
    let path = dir.join("browser").join("permissions.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains("\"camera\""),
        "a revoked grant is still in {}: {raw}",
        path.display()
    );
    assert!(
        raw.contains("microphone"),
        "the revoke took an unrelated grant with it: {raw}"
    );

    // Forgetting an origin outright, then everything.
    assert!(client
        .browser_do(Request::PermissionForget {
            origin: "https://meet.example".into(),
            permission: None,
        })
        .await
        .unwrap());
    assert!(client.browser_do(Request::PermissionClear).await.unwrap());
    assert!(
        !client.browser_do(Request::PermissionClear).await.unwrap(),
        "clearing an empty store reported changed:true"
    );

    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    let mut rx = client.watch_permissions().await.unwrap();
    assert!(
        next_permissions(&mut rx).await.grants.is_empty(),
        "a cleared store came back with answers in it"
    );
}

/// **Slice W1 acceptance: ⌘N, over the wire and across a restart.**
///
/// Three claims, and the third is the one that needed a real engine.
///
/// 1. Two windows are two strips: each carries its own tabs and its own
///    active tab, and neither can see the other's.
/// 2. A tab a PAGE opens lands beside the page that opened it. The client
///    names no window for a `window.open` — it has an opener and the fold
///    knows which window that is, so resolving it anywhere else would be
///    a second, staler copy of a fact the engine already holds.
/// 3. **Windows are durable.** A `kill -9` and the second window's tabs
///    come back in the second window, not folded into the first. Windows
///    have no event of their own — a window is its tabs — so this is also
///    the assertion that the `window` on each `Opened` is really being
///    journaled and really being read back.
#[tokio::test]
async fn two_windows_are_two_strips_and_both_survive_a_kill_9() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();

    let first = client
        .tab_open(None, Some("https://first.example/"), true)
        .await
        .unwrap();
    let second = client
        .tab_open(Some("w-2"), Some("https://second.example/"), true)
        .await
        .unwrap();
    // A page in the SECOND window opening a tab: no window named, an
    // opener that is in w-2.
    let child = client
        .browser_create(Request::TabOpen {
            window: None,
            url: None,
            group: None,
            index: None,
            content: TabContent::Page,
            opener: Some(second.clone()),
            activate: false,
            incognito: false,
        })
        .await
        .unwrap();

    let mut rx = client.watch_tabs().await.unwrap();
    let snap = next_tabs(&mut rx).await;
    let window_of = |snap: &TabsSnapshot, id: &str| {
        snap.tabs
            .iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("no tab {id}"))
            .window
            .clone()
    };
    assert_eq!(
        window_of(&snap, &first),
        None,
        "the primary window is unnamed on the wire"
    );
    assert_eq!(window_of(&snap, &second).as_deref(), Some("w-2"));
    assert_eq!(
        window_of(&snap, &child).as_deref(),
        Some("w-2"),
        "a tab a page opened did not land beside the page that opened it"
    );

    let strip = |snap: &TabsSnapshot, id: &str| {
        snap.windows
            .iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("no window {id}"))
            .clone()
    };
    assert_eq!(strip(&snap, "w-2").active.as_deref(), Some(second.as_str()));
    assert_eq!(snap.windows.len(), 2, "two windows, got {:?}", snap.windows);
    // The flat fields still answer for a client that has never heard of
    // windows: the first window's strip, not every window's.
    assert_eq!(snap.active.as_deref(), Some(first.as_str()));
    assert_eq!(
        snap.order.len(),
        1,
        "the flat order carried more than one window"
    );

    drop(rx);
    drop(client);
    engine.sigkill();
    let _engine = EngineProc::spawn(&dir);
    let client = Client::connect(&dir).await.unwrap();
    let mut rx = client.watch_tabs().await.unwrap();
    let after = next_tabs(&mut rx).await;

    assert_eq!(after.tabs.len(), 3, "a tab was lost across the restart");
    assert_eq!(window_of(&after, &first), None);
    assert_eq!(window_of(&after, &second).as_deref(), Some("w-2"));
    assert_eq!(
        window_of(&after, &child).as_deref(),
        Some("w-2"),
        "the second window's tabs folded into the first on restore"
    );
    assert_eq!(after.windows.len(), 2);
}
