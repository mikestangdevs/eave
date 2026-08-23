//! Slice A acceptance: the `watch` subscription (spec 05 §Watch) — the
//! first cross-session broadcast in the protocol, and therefore the seam
//! where per-session isolation could quietly break.
//!
//! The test that matters here is the leak test: a raw-socket watcher
//! collects every byte the engine sends on the watch id while another
//! client streams canary-marked transcript content — and none of it may
//! appear. The positive controls (the canary IS on the subscribe wire;
//! the watcher DOES see state transitions) keep the assertion non-vacuous.
//!
//! Same harness rules as `wire.rs`: data dirs under `/tmp/eave-wire-tests`
//! (`sun_path` limit), workspace-built bins.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use eave_core::journal::JournalEvent;
use eave_core::proc::{self, ProcIdentity};
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::model::{Row, Transcript};
use eave_wire::{socket_path, ResponseBody, ResponseEnvelope, SessionInfo, SubscriptionItem};

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
    fn spawn(data_dir: &Path, fake_mode: &str) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let child = Command::new(engined_bin())
            .args(["--data-dir", &data_dir.display().to_string()])
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", fake_mode)
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

    fn sigkill(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        let sessions = self.data_dir.join("sessions");
        if let Ok(entries) = std::fs::read_dir(sessions) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(id) = meta.child {
                        kill_if_matches(&id);
                    }
                }
            }
        }
    }
}

fn kill_if_matches(id: &ProcIdentity) {
    if let Some(current) = proc::identity_of(id.pid) {
        if current == *id {
            unsafe {
                libc::killpg(id.pid, libc::SIGKILL);
            }
        }
    }
}

async fn drain_records(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    transcript: &mut Transcript,
    min_records: usize,
    deadline: Duration,
) -> Vec<eave_core::journal::Record> {
    let mut got = Vec::new();
    let end = Instant::now() + deadline;
    while got.len() < min_records {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for records (have {})",
            got.len()
        );
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("subscription item in time")
            .expect("subscription stream ended unexpectedly");
        match item {
            SubscriptionItem::Reset { .. } => transcript.reset(),
            SubscriptionItem::Events { epoch, records, .. } => {
                transcript.apply(epoch, &records).expect("no seq gaps");
                got.extend(records);
            }
            SubscriptionItem::Attention { .. }
            | SubscriptionItem::Sessions { .. }
            | SubscriptionItem::Files { .. } => {}
            // Slice B1: browser frames flow on their own subscription
            // ids and are demultiplexed by the client, so one arriving
            // here would mean the demux itself broke. Say so loudly
            // rather than drop it into the ignore arm above.
            SubscriptionItem::Tabs { .. }
            | SubscriptionItem::Bookmarks { .. }
            | SubscriptionItem::Permissions { .. } => {
                panic!("a browser frame arrived on a session subscription")
            }
        }
    }
    got
}

/// THE isolation test (build directive, Slice A): a raw-socket watch
/// client sees every byte the engine emits on the watch id across a
/// session's whole life — spawn, canary-marked streaming, kill — and no
/// transcript content may ever appear. Watch frames are `sessions`
/// snapshots and nothing else.
#[tokio::test]
async fn watch_channel_never_carries_transcript_content() {
    const CANARY: &str = "LEAK_CANARY_7f3a";
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");

    // Raw watch socket: speak the protocol by hand so the assertion is on
    // the actual bytes, not on what a typed client deigned to decode.
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .expect("connect raw");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    let hello = lines.next_line().await.unwrap().expect("hello reply");
    assert!(hello.contains("\"ok\""), "{hello}");
    w.write_all(b"{\"id\":2,\"method\":\"watch\"}\n")
        .await
        .unwrap();

    // A second, normal client drives a session through its whole life
    // with the canary in the user text (a journal record's content).
    let c = Client::connect(&dir).await.expect("connect client");
    c.open(Some("s1".into()), None).await.expect("open");
    assert!(c
        .send("s1", &format!("{CANARY} do the thing"))
        .await
        .expect("send"));

    // Positive control 1: the canary provably crosses the wire on a
    // *subscribe* — so its absence on watch is isolation, not vacuity.
    let mut t = Transcript::new();
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "canary never arrived on subscribe");
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        if t.rows
            .iter()
            .any(|r| matches!(r, Row::User { text, .. } if text.contains(CANARY)))
        {
            break;
        }
    }
    // Let the turn stream a moment, then settle the session terminally.
    tokio::time::sleep(Duration::from_millis(400)).await;
    c.kill("s1").await.expect("kill");

    // Collect raw watch frames until the watcher has seen the terminal
    // state — the full life cycle happened while we were listening.
    let mut watch_lines: Vec<String> = Vec::new();
    let end = Instant::now() + Duration::from_secs(15);
    let mut saw_live = false;
    let mut saw_terminal = false;
    while !saw_terminal {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "watch never reported the kill");
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .expect("watch frame in time")
            .unwrap()
            .expect("watch stream open");
        let env: ResponseEnvelope = serde_json::from_str(&line).expect("valid frame");
        if env.id != 2 {
            continue; // not a watch frame (there are none, but be strict)
        }
        if let ResponseBody::Item(SubscriptionItem::Sessions { sessions }) = &env.body {
            if let Some(s1) = sessions.iter().find(|s| s.session_id == "s1") {
                saw_live |= s1.live;
                saw_terminal |= !s1.live && s1.state == "killed";
            }
        } else {
            panic!("watch id carried a non-sessions frame: {line}");
        }
        watch_lines.push(line);
    }

    // Positive control 2: the watcher did observe state moving.
    assert!(saw_live, "watch never reported the session live");
    assert!(!watch_lines.is_empty());

    // The isolation invariant, on raw bytes: no transcript content, no
    // record structure, ever, on the watch id.
    for line in &watch_lines {
        assert!(
            !line.contains(CANARY),
            "transcript content leaked onto the watch channel: {line}"
        );
        assert!(
            !line.contains("\"records\"") && !line.contains("\"events\""),
            "record payload structure on the watch channel: {line}"
        );
    }

    drop(engine);
}

/// Stage AB, and the reason the rail's title is not the one the build
/// directive asked for.
///
/// The obvious title is the session's first prompt, capped. It cannot
/// exist: the canary above plants its marker at the HEAD of that prompt,
/// so a leading-words title does not merely bend the isolation invariant
/// (spec 05 §Watch), it fails `watch_channel_never_carries_transcript_content`
/// by name. The feature was degraded to an operator-set name instead, and
/// this test is what stops it being quietly un-degraded: it asserts the
/// engine looks at the journal for a title exactly never.
///
/// Positive control is the same one the canary uses — the words provably
/// cross `subscribe` — so "no title" here is a rule being kept, not a
/// session that never had any content to mine.
#[tokio::test]
async fn the_engine_never_mines_a_title_out_of_the_transcript() {
    const CANARY: &str = "TITLE_CANARY_91b4";
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect client");
    c.open(Some("s1".into()), None).await.expect("open");
    // Delivery is deliberately not asserted here: `UserSent` is journaled
    // BEFORE the pipe write (spec 02 §Delivery state), so the record this
    // test is about exists either way — and the delivered flag is a known
    // flaky signal on a loaded machine.
    c.send("s1", &format!("{CANARY} rename the rail rows"))
        .await
        .expect("send");

    // Positive control: the prompt IS on this session's own stream.
    let mut t = Transcript::new();
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "canary never arrived on subscribe");
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        if t.rows
            .iter()
            .any(|r| matches!(r, Row::User { text, .. } if text.contains(CANARY)))
        {
            break;
        }
    }

    // The session set says nothing about it — not the first word, not a
    // capped prefix, nothing.
    let mut watch = c.watch().await.expect("watch");
    let end = Instant::now() + Duration::from_secs(15);
    let mut checked = 0usize;
    while Instant::now() < end && checked < 3 {
        let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(5), watch.recv()).await
        else {
            break;
        };
        let SubscriptionItem::Sessions { sessions } = item else {
            panic!("watch carried a non-sessions frame");
        };
        let s1 = sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("s1 in snapshot");
        assert_eq!(
            s1.title, None,
            "the engine derived a title from the transcript — that is the \
             one thing this field may never be"
        );
        // Belt and braces at the byte level: no fragment of the prompt,
        // however short, may have reached the snapshot by any route.
        let json = serde_json::to_string(&sessions).unwrap();
        assert!(!json.contains(CANARY), "{json}");
        assert!(!json.contains("rename the rail"), "{json}");
        checked += 1;
    }
    assert!(checked > 0, "watch produced no snapshot to check");

    drop(engine);
}

/// Stage AB: a title the OPERATOR set is a session fact, so it does what
/// every other session fact does — rides `watch`, survives the meta
/// rewrites a turn performs, and survives an engine restart.
///
/// The turn is the part worth having a test for. `meta.json` is rewritten
/// wholesale at spawn and again at `init`, from a snapshot taken before
/// the rename; without `Meta::save_keeping_operator_fields` the name vanishes the
/// first time you talk to the session you just named.
#[tokio::test]
async fn an_operator_title_is_a_durable_session_fact() {
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");

    // Normalization is the engine's, and the reply states what landed.
    let info = c
        .rename("s1", Some("  the   rail\nslice  "))
        .await
        .expect("rename");
    assert_eq!(info.title.as_deref(), Some("the rail slice"));

    // The cap is the engine's too — a paste does not become a session fact.
    let long = "x".repeat(500);
    let info = c.rename("s1", Some(&long)).await.expect("rename long");
    assert_eq!(
        info.title.as_deref().map(|t| t.chars().count()),
        Some(eave_core::store::TITLE_MAX_CHARS)
    );
    let info = c
        .rename("s1", Some("the rail slice"))
        .await
        .expect("rename back");
    assert_eq!(info.title.as_deref(), Some("the rail slice"));

    // A turn rewrites meta twice. The name must still be there after it.
    // What matters is that a spawn happened, not that the byte landed.
    c.send("s1", "go").await.expect("send");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "session never went live");
        let listed = c.list().await.expect("list");
        let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!(
            s1.title.as_deref(),
            Some("the rail slice"),
            "a turn erased the name the operator gave the session"
        );
        if s1.live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    c.kill("s1").await.expect("kill");
    engine1.sigkill();

    // Durable: a fresh engine reads it back off disk.
    let engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("connect 2");
    let mut watch = c2.watch().await.expect("watch");
    let item = tokio::time::timeout(Duration::from_secs(10), watch.recv())
        .await
        .expect("snapshot in time")
        .expect("watch open");
    let SubscriptionItem::Sessions { sessions } = item else {
        panic!("watch must open with a snapshot");
    };
    let s1 = sessions.iter().find(|s| s.session_id == "s1").unwrap();
    assert_eq!(s1.title.as_deref(), Some("the rail slice"));

    // And clearing is the same method, not a second one.
    let info = c2.rename("s1", None).await.expect("clear");
    assert_eq!(info.title, None);
    // So is a title made only of whitespace: that is not a title.
    let info = c2.rename("s1", Some("   ")).await.expect("blank");
    assert_eq!(info.title, None);

    drop(engine2);
}

/// Stage AB: `last_activity_ms` is stamped by state changes and by nothing
/// else — which is what keeps it inside the isolation invariant instead of
/// beside it.
///
/// A timestamp that advanced per record would let any watcher read the
/// *rate* of any session's transcript off the watch channel: not content
/// sent, but content inferred, which the invariant is equally about. So
/// the second half of this test is the load-bearing half — the stamp holds
/// perfectly still across a stretch of heavy streaming, because the state
/// name held still.
#[tokio::test]
async fn activity_is_stamped_by_state_changes_and_nothing_else() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");

    // Nothing has happened, so nothing is claimed. Not a zero — a zero
    // would sort every never-started session to 1970.
    let listed = c.list().await.expect("list");
    let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
    assert_eq!(s1.last_activity_ms, None);
    let created = s1.created_ts_ms;

    // new → running is a state change, and it stamps.
    c.send("s1", "go").await.expect("send");
    let end = Instant::now() + Duration::from_secs(15);
    let stamped = loop {
        assert!(Instant::now() < end, "session never went live");
        let listed = c.list().await.expect("list");
        let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
        if let Some(ms) = s1.last_activity_ms {
            assert!(ms >= created, "{ms} < {created}");
            break ms;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    // Now the session streams hard with its state name unchanged. The
    // stamp must not move a millisecond.
    let before = c.list().await.expect("list");
    let state_before = before
        .iter()
        .find(|s| s.session_id == "s1")
        .unwrap()
        .state
        .clone();
    tokio::time::sleep(Duration::from_millis(900)).await;
    let after = c.list().await.expect("list");
    let s1 = after.iter().find(|s| s.session_id == "s1").unwrap();
    if s1.state == state_before {
        assert_eq!(
            s1.last_activity_ms,
            Some(stamped),
            "the activity stamp moved while the state did not — that makes \
             the watch channel a report of transcript throughput"
        );
    }

    // A terminal transition IS a state change, so it does stamp.
    c.kill("s1").await.expect("kill");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "kill never settled");
        let listed = c.list().await.expect("list");
        let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
        if !s1.live && s1.last_activity_ms > Some(stamped) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    drop(engine);
}

/// Selection switch mid-stream (Slice A): drop the s1 subscription while
/// its turn is still streaming, subscribe s2 cursorless — s2 replays from
/// seq 1 behind a Reset, contains only s2 content, and s1's turn is
/// provably undisturbed.
#[tokio::test]
async fn switching_subscriptions_swaps_transcripts_cleanly() {
    const CANARY1: &str = "S1_CANARY_a1";
    const CANARY2: &str = "S2_CANARY_b2";
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");
    assert!(c.send("s1", CANARY1).await.expect("send s1"));
    let mut t1 = Transcript::new();
    let mut rx1 = c.subscribe("s1", None).await.expect("subscribe s1");
    drain_records(&mut rx1, &mut t1, 6, Duration::from_secs(15)).await;
    assert!(
        t1.rows
            .iter()
            .any(|r| matches!(r, Row::User { text, .. } if text.contains(CANARY1))),
        "s1 canary in s1 transcript"
    );

    // Second session starts streaming too.
    c.open(Some("s2".into()), None).await.expect("open s2");
    assert!(c.send("s2", CANARY2).await.expect("send s2"));

    // The switch: the UI drops its old subscription and subscribes the
    // new session with no cursor (it holds no projection for it).
    drop(rx1);
    let mut rx2 = c.subscribe("s2", None).await.expect("subscribe s2");
    let first = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .expect("frame in time")
        .expect("stream open");
    match &first {
        SubscriptionItem::Reset { session_id, .. } => assert_eq!(session_id, "s2"),
        other => panic!("cursorless subscribe must open with Reset, got {other:?}"),
    }
    let mut t2 = Transcript::new();
    let records = drain_records(&mut rx2, &mut t2, 6, Duration::from_secs(15)).await;
    assert_eq!(records.first().unwrap().seq, 1, "full replay from seq 1");
    let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
    for w in seqs.windows(2) {
        assert_eq!(w[1], w[0] + 1, "no gaps, no duplicates across the switch");
    }
    let t2_text: Vec<String> = t2
        .rows
        .iter()
        .filter_map(|r| match r {
            Row::User { text, .. } => Some(text.clone()),
            Row::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        t2_text.iter().any(|t| t.contains(CANARY2)),
        "s2 content present after switch"
    );
    assert!(
        !t2_text.iter().any(|t| t.contains(CANARY1)),
        "no bleed: s1 content must not appear in the s2 stream"
    );

    // s1 was never disturbed by the switch: resubscribing replays a
    // still-open, still-streaming turn.
    let mut t1b = Transcript::new();
    let mut rx1b = c.subscribe("s1", None).await.expect("resubscribe s1");
    let r1 = drain_records(&mut rx1b, &mut t1b, 6, Duration::from_secs(15)).await;
    assert!(
        !r1.iter().any(|r| matches!(
            r.event,
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. }
        )),
        "s1 turn survived the switch untouched"
    );

    drop(engine);
}

/// Engine restart with a watcher: the watch stream dies with the socket;
/// a fresh watch against the restarted engine rebuilds the full session
/// set from disk (list is repair-backed), including both sessions.
#[tokio::test]
async fn watch_snapshot_rebuilds_after_engine_restart() {
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");
    c.open(Some("s2".into()), None).await.expect("open s2");
    assert!(c.send("s1", "go").await.expect("send"));

    let mut watch = c.watch().await.expect("watch");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "watch never showed both sessions");
        let item = tokio::time::timeout(Duration::from_secs(15), watch.recv())
            .await
            .expect("watch frame")
            .expect("watch open");
        if let SubscriptionItem::Sessions { sessions } = item {
            let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
            if ids.contains(&"s1") && ids.contains(&"s2") {
                break;
            }
        }
    }

    engine1.sigkill();
    // The watch stream ends with the connection — that is the client's
    // signal to reconnect and re-watch (rebuild, never merge).
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if watch.recv().await.is_none() {
                break;
            }
        }
    })
    .await;
    assert!(gone.is_ok(), "watch stream must end when the engine dies");

    let engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("connect 2");
    let mut watch2 = c2.watch().await.expect("watch 2");
    let item = tokio::time::timeout(Duration::from_secs(10), watch2.recv())
        .await
        .expect("snapshot in time")
        .expect("watch open");
    let SubscriptionItem::Sessions { sessions } = item else {
        panic!("watch must open with a snapshot, got {item:?}");
    };
    let by_id = |id: &str| -> &SessionInfo {
        sessions
            .iter()
            .find(|s| s.session_id == id)
            .unwrap_or_else(|| panic!("{id} missing from rebuilt snapshot"))
    };
    // Both sessions rebuilt from disk; nothing is live after the restart,
    // and states come from the repair-backed load.
    assert!(!by_id("s1").live);
    assert!(!by_id("s2").live);
    assert_eq!(
        by_id("s2").state,
        "new",
        "never-spawned session loads as new"
    );

    drop(engine2);
}

/// Stage AC: **the journal is never touched.** That is the entire
/// contract of the archive bit, and it is asserted the only way worth
/// asserting it — on the bytes of `journal.jsonl`, before and after, with
/// a real transcript in the file so there is something to lose.
///
/// The rest of the slice depends on this being true: the whole reason
/// archive is safe to offer as a one-click action is that it is
/// reversible, and it is only reversible because nothing was destroyed.
/// An archive that rewrote history would be a delete wearing a softer
/// word.
#[tokio::test]
async fn archiving_never_touches_the_journal() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");
    c.send("s1", "write something down").await.expect("send");

    // Wait for a journal with real content in it — archiving an empty file
    // proves nothing.
    let journal = dir.join("sessions").join("s1").join("journal.jsonl");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "the journal never grew a transcript");
        let bytes = std::fs::read(&journal).unwrap_or_default();
        if String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
            >= 3
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.kill("s1").await.expect("kill");
    // Settle: a live process can still be appending closers.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = std::fs::read(&journal).expect("read journal");
    assert!(
        before.len() > 100,
        "only {} bytes of journal — nothing here is at risk",
        before.len()
    );

    let info = c.set_archived("s1", true).await.expect("archive");
    assert!(info.archived, "the engine did not report the bit back");
    assert_eq!(
        std::fs::read(&journal).expect("read journal"),
        before,
        "archiving rewrote the journal — the one thing it may never do"
    );

    let info = c.set_archived("s1", false).await.expect("unarchive");
    assert!(!info.archived, "the same method has to flip it back");
    assert_eq!(
        std::fs::read(&journal).expect("read journal"),
        before,
        "un-archiving rewrote the journal"
    );

    // And the transcript still replays, byte for byte, to a subscriber —
    // "the file is unchanged" and "the session still works" are different
    // claims and the second is the one an operator cares about.
    c.set_archived("s1", true).await.expect("archive again");
    let mut t = Transcript::new();
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    assert!(
        t.rows
            .iter()
            .any(|r| matches!(r, Row::User { text, .. } if text.contains("write something down"))),
        "an archived session stopped replaying its own transcript: {:?}",
        t.rows
    );

    drop(engine);
}

/// Stage AC: the bit is a durable session fact, it survives the meta
/// rewrites a turn performs, and **the engine keeps reporting archived
/// sessions on `list` and `watch`**.
///
/// That last claim is the one that makes the feature reversible from a
/// UI. An engine that filtered its own listing would leave the bit
/// unreachable from the surface that set it — you could archive a session
/// and then have no way to ask for it back.
#[tokio::test]
async fn an_archived_session_is_durable_and_still_listed() {
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");
    c.open(Some("s2".into()), None).await.expect("open s2");

    let listed = c.list().await.expect("list");
    assert!(
        listed.iter().all(|s| !s.archived),
        "a fresh session arrives archived"
    );

    c.set_archived("s1", true).await.expect("archive");
    let listed = c.list().await.expect("list");
    assert_eq!(
        listed.len(),
        2,
        "the engine filtered its own listing — the bit is now unreachable \
         from the client that set it: {listed:?}"
    );
    let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
    let s2 = listed.iter().find(|s| s.session_id == "s2").unwrap();
    assert!(s1.archived);
    assert!(!s2.archived, "archiving s1 archived its neighbour");

    // Naming and filing are independent fields: setting one must not move
    // the other, in either direction.
    c.rename("s1", Some("the landfill")).await.expect("rename");
    let s1 = c
        .list()
        .await
        .expect("list")
        .into_iter()
        .find(|s| s.session_id == "s1")
        .unwrap();
    assert_eq!(s1.title.as_deref(), Some("the landfill"));
    assert!(s1.archived, "a rename un-archived the session");

    // A turn rewrites meta twice, from a snapshot that knows about
    // neither field. Both have to still be there afterwards.
    c.send("s1", "go").await.expect("send");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "session never went live");
        let listed = c.list().await.expect("list");
        let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
        assert!(
            s1.archived,
            "a turn un-archived the session the operator filed away"
        );
        assert_eq!(s1.title.as_deref(), Some("the landfill"));
        if s1.live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    c.kill("s1").await.expect("kill");
    engine1.sigkill();

    // Durable: a fresh engine reads it back off disk, and reports it.
    let engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("connect 2");
    let mut watch = c2.watch().await.expect("watch");
    let item = tokio::time::timeout(Duration::from_secs(10), watch.recv())
        .await
        .expect("snapshot in time")
        .expect("watch open");
    let SubscriptionItem::Sessions { sessions } = item else {
        panic!("watch must open with a snapshot");
    };
    let s1 = sessions.iter().find(|s| s.session_id == "s1").unwrap();
    assert!(s1.archived, "the bit did not survive a restart");
    assert_eq!(s1.title.as_deref(), Some("the landfill"));
    assert!(
        sessions.iter().any(|s| s.session_id == "s2" && !s.archived),
        "the unarchived neighbour vanished across the restart"
    );

    // Reversible after a restart too, which is the case that actually
    // matters: the operator comes back tomorrow and wants it back.
    let info = c2.set_archived("s1", false).await.expect("unarchive");
    assert!(!info.archived);
    assert!(
        !Meta::load(&dir.join("sessions").join("s1"))
            .expect("meta")
            .archived,
        "the reply said unarchived and the disk disagreed"
    );

    drop(engine2);
}
