//! Slice I1 acceptance: N concurrent sessions (the multi-session build).
//!
//! The engine was concurrent by construction before this slice —
//! per-session recorder/monitor/broadcast tasks, cross-contamination
//! proven at N=2 since Stage B. What I1 adds is the `unsubscribe` wire
//! method (a switch must release the deselected session's streams and
//! its FS watcher, not leak them for the connection's life) and proof at
//! N>2 for everything the directive lists: concurrent streaming with
//! switches, background attention on watch, kill isolation, engine
//! restart with N live, and the raw-byte isolation invariant at N.
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
use eave_wire::{socket_path, ResponseEnvelope, SubscriptionItem};

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

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo with one committed file, in its own scratch dir — each
/// session gets its OWN repo (I1 is the different-worktrees case).
fn repo() -> PathBuf {
    let dir = scratch();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("base.txt"), "one\ntwo\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    dir
}

struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path, fake_mode: &str) -> Self {
        Self::spawn_with_env(data_dir, fake_mode, &[])
    }

    fn spawn_with_env(data_dir: &Path, fake_mode: &str, envs: &[(&str, &str)]) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let mut cmd = Command::new(engined_bin());
        cmd.args(["--data-dir", &data_dir.display().to_string()])
            // A test engine must never inherit the DEVELOPER'S checkout as
            // its default cwd: a session opened without one would then be
            // a session on the real repo, and since AG1 #11 that means a
            // checkpoint ref written into the shared git dir on every
            // send. (It also hashed the whole eave work tree for nothing.)
            .current_dir(data_dir)
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", fake_mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn eave-engined");
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

fn transcript_texts(t: &Transcript) -> Vec<String> {
    t.rows
        .iter()
        .filter_map(|r| match r {
            Row::User { text, .. } => Some(text.clone()),
            Row::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Raw-socket helpers (the leak/unsubscribe tests assert on actual bytes)

struct RawConn {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl RawConn {
    async fn connect(dir: &Path) -> Self {
        let stream = tokio::net::UnixStream::connect(socket_path(dir))
            .await
            .expect("connect raw");
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
            .await
            .unwrap();
        let hello = lines.next_line().await.unwrap().expect("hello reply");
        assert!(hello.contains("\"ok\""), "{hello}");
        Self { lines, w }
    }

    async fn send(&mut self, raw: &str) {
        self.w.write_all(raw.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    async fn next(&mut self, what: &str, deadline: Duration) -> String {
        tokio::time::timeout(deadline, self.lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap()
            .unwrap_or_else(|| panic!("connection closed waiting for {what}"))
    }

    /// Collect every line for the window (no assertion on count).
    async fn collect_for(&mut self, window: Duration) -> Vec<String> {
        let mut out = Vec::new();
        let end = Instant::now() + window;
        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return out;
            }
            match tokio::time::timeout(remaining, self.lines.next_line()).await {
                Err(_) => return out,
                Ok(Ok(Some(line))) => out.push(line),
                Ok(_) => return out,
            }
        }
    }
}

fn frame_id(line: &str) -> u64 {
    serde_json::from_str::<ResponseEnvelope>(line)
        .map(|e| e.id)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// I1: N sessions streaming concurrently; switching disturbs nothing

/// Four sessions in four different repos, all mid-turn at once. Visit each
/// one the way the UI does (subscribe → read → unsubscribe → next):
/// every transcript is seq-contiguous, carries only its own canary, and
/// after the tour every background session has provably kept streaming
/// (its log advanced past where we left it) with its turn intact.
#[tokio::test]
async fn n_sessions_stream_concurrently_and_switching_disturbs_nothing() {
    const N: usize = 4;
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");
    let c = Client::connect(&dir).await.expect("connect");

    let canary = |i: usize| format!("CANARY_S{i}_9c4e");
    let mut cwds = Vec::new();
    for i in 0..N {
        let cwd = repo();
        c.open(Some(format!("s{i}")), Some(cwd.display().to_string()))
            .await
            .expect("open");
        assert!(c.send(&format!("s{i}"), &canary(i)).await.expect("send"));
        cwds.push(cwd);
    }

    // First tour: all four are streaming; read each mid-stream.
    let mut seen_seq = [0u64; N];
    // `i` names the SESSION ("s{i}"), not just the slot it indexes,
    // so the range loop is the form that says what this is.
    #[allow(clippy::needless_range_loop)]
    for i in 0..N {
        let sid = format!("s{i}");
        let (sub_id, mut rx) = c.subscribe_with_id(&sid, None).await.expect("subscribe");
        let mut t = Transcript::new();
        let records = drain_records(&mut rx, &mut t, 8, Duration::from_secs(15)).await;
        assert_eq!(records.first().unwrap().seq, 1, "full replay from seq 1");
        let texts = transcript_texts(&t);
        for j in 0..N {
            let present = texts.iter().any(|txt| txt.contains(&canary(j)));
            assert_eq!(
                present,
                i == j,
                "session {sid} transcript vs canary {j}: only its own may appear"
            );
        }
        seen_seq[i] = records.last().unwrap().seq;
        assert!(c.unsubscribe(sub_id).await.expect("unsubscribe"));
    }

    // The tour left every session running in the background: revisit each
    // and its log must have advanced well past where we left it, with the
    // turn still open — switching stopped, paused, and dropped nothing.
    tokio::time::sleep(Duration::from_millis(700)).await;
    // `i` names the SESSION ("s{i}"), not just the slot it indexes,
    // so the range loop is the form that says what this is.
    #[allow(clippy::needless_range_loop)]
    for i in 0..N {
        let sid = format!("s{i}");
        let (sub_id, mut rx) = c.subscribe_with_id(&sid, None).await.expect("resubscribe");
        let mut t = Transcript::new();
        let records = drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        let last = records.last().unwrap().seq;
        assert!(
            last > seen_seq[i] + 4,
            "session {sid} stalled in the background: seq {last} vs {} at switch-away",
            seen_seq[i]
        );
        assert!(
            !records.iter().any(|r| matches!(
                r.event,
                JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. }
            )),
            "session {sid} turn must survive the tour untouched"
        );
        let texts = transcript_texts(&t);
        assert!(
            texts.iter().any(|txt| txt.contains(&canary(i))),
            "session {sid} replay carries its own canary"
        );
        assert!(c.unsubscribe(sub_id).await.expect("unsubscribe"));
    }

    drop(engine);
}

// ---------------------------------------------------------------------------
// I1: unsubscribe semantics (the method this slice adds)

/// After `unsubscribe`, not one more frame flows on the old id — asserted
/// on raw bytes, with new records provably generated after the cut and a
/// fresh subscription seeing them (the engine still streams, just not to
/// the dead id).
#[tokio::test]
async fn unsubscribe_stops_a_record_stream_dead() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    assert!(c.send("s1", "first turn").await.expect("send"));

    let mut raw = RawConn::connect(&dir).await;
    raw.send(r#"{"id":10,"method":"subscribe","params":{"session_id":"s1"}}"#)
        .await;
    // Read until the first turn's completion has replayed on id 10.
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "first turn never replayed");
        let line = raw.next("replay frame", Duration::from_secs(15)).await;
        if frame_id(&line) == 10 && line.contains("turn_completed") {
            break;
        }
    }

    raw.send(r#"{"id":11,"method":"unsubscribe","params":{"id":10}}"#)
        .await;
    let reply = loop {
        let line = raw.next("unsubscribe reply", Duration::from_secs(10)).await;
        if frame_id(&line) == 11 {
            break line;
        }
        // In-flight id-10 frames written before the engine processed the
        // unsubscribe are legitimate; after the reply they are not.
        assert_eq!(frame_id(&line), 10, "unexpected frame: {line}");
    };
    assert!(reply.contains("\"stopped\":true"), "{reply}");

    // Generate a whole new turn's worth of records after the cut.
    assert!(c.send("s1", "second turn").await.expect("send 2"));
    let stray = raw.collect_for(Duration::from_secs(2)).await;
    assert!(
        stray.is_empty(),
        "frames after unsubscribe on a supposedly dead connection: {stray:?}"
    );

    // Positive control: a fresh subscription sees both turns.
    raw.send(r#"{"id":30,"method":"subscribe","params":{"session_id":"s1"}}"#)
        .await;
    let end = Instant::now() + Duration::from_secs(15);
    let mut saw_second = false;
    while !saw_second {
        assert!(
            Instant::now() < end,
            "fresh subscribe never replayed turn 2"
        );
        let line = raw.next("fresh replay", Duration::from_secs(15)).await;
        assert_eq!(frame_id(&line), 30, "only id 30 may flow now: {line}");
        saw_second = line.contains("second turn");
    }

    drop(engine);
}

/// Unsubscribing the files stream releases the session's FS watcher (the
/// subscriber-scoped resource). Observable without reaching into the
/// engine: after the cut, a bare fs write must cause NO rescan (the next
/// subscriber's first frame — the cached snapshot — provably predates the
/// write), and that next subscriber's arrival restarts a watcher whose
/// startup scan then states the new file. A leaked refcount fails both
/// halves: the old watcher would have rescanned, and no fresh startup
/// scan would run.
#[tokio::test]
async fn unsubscribe_releases_the_files_watcher() {
    let dir = scratch();
    let cwd = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    // Open only — no send, so no turn boundaries can poll git and muddy
    // the "who scanned" attribution below.
    c.open(Some("s1".into()), Some(cwd.display().to_string()))
        .await
        .expect("open");

    let mut raw = RawConn::connect(&dir).await;
    raw.send(r#"{"id":20,"method":"files","params":{"session_id":"s1"}}"#)
        .await;
    // Wait for the watcher's startup scan: a frame stating watching=true.
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "watcher never reported active");
        let line = raw.next("files frame", Duration::from_secs(15)).await;
        assert_eq!(frame_id(&line), 20);
        if line.contains("\"watching\":true") {
            break;
        }
    }

    // Prove the live watcher end-to-end first: a marker write must flow
    // all the way to a frame. This also drains the startup-scan pipeline —
    // scans are gated per session, and a scan QUEUED before the cut but
    // executed after the next write would see that write and fake a leak
    // (the race this test originally had).
    std::fs::write(cwd.join("marker.txt"), "seen by the live watcher\n").unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            Instant::now() < end,
            "live watcher never scanned marker.txt"
        );
        let line = raw.next("marker frame", Duration::from_secs(15)).await;
        assert_eq!(frame_id(&line), 20);
        if line.contains("marker.txt") {
            break;
        }
    }

    raw.send(r#"{"id":21,"method":"unsubscribe","params":{"id":20}}"#)
        .await;
    loop {
        let line = raw.next("unsubscribe reply", Duration::from_secs(10)).await;
        if frame_id(&line) == 21 {
            assert!(line.contains("\"stopped\":true"), "{line}");
            break;
        }
        assert_eq!(frame_id(&line), 20, "unexpected frame: {line}");
    }
    // Settle: any scan already queued at the cut runs now, before the
    // write below exists — and the dead id must stay silent throughout.
    let stray = raw.collect_for(Duration::from_secs(2)).await;
    assert!(
        stray.is_empty(),
        "frames after files unsubscribe: {stray:?}"
    );

    // A write the released watcher must NOT see. Wait out the debounce
    // plus margin; nothing may arrive on the dead id either.
    std::fs::write(cwd.join("orphan.txt"), "written after release\n").unwrap();
    let stray = raw.collect_for(Duration::from_millis(1800)).await;
    assert!(
        stray.is_empty(),
        "frames after files unsubscribe: {stray:?}"
    );

    // Next subscriber: its FIRST frame is the cached snapshot, which must
    // predate orphan.txt (no watcher was alive to rescan) — then the
    // restarted watcher's startup scan states the file.
    raw.send(r#"{"id":40,"method":"files","params":{"session_id":"s1"}}"#)
        .await;
    let first = raw.next("cached snapshot", Duration::from_secs(10)).await;
    assert_eq!(frame_id(&first), 40);
    assert!(
        first.contains("marker.txt"),
        "cached snapshot must carry the pre-cut state: {first}"
    );
    assert!(
        !first.contains("orphan.txt"),
        "cached snapshot already lists orphan.txt — the released watcher kept scanning: {first}"
    );
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            Instant::now() < end,
            "no startup scan after resubscribe — the watcher refcount never dropped to zero"
        );
        let line = raw
            .next("startup-scan frame", Duration::from_secs(15))
            .await;
        assert_eq!(frame_id(&line), 40);
        if line.contains("orphan.txt") {
            break;
        }
    }

    drop(engine);
}

// ---------------------------------------------------------------------------
// I1: kill isolation at N

/// Killing one of three mid-turn sessions leaves the other two provably
/// untouched: the subscribed one keeps streaming through the kill, the
/// unsubscribed one is revisited later still mid-turn, and the watch
/// snapshot states exactly one `killed`.
#[tokio::test]
async fn killing_one_session_leaves_the_others_streaming() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");
    let c = Client::connect(&dir).await.expect("connect");
    for i in 0..3 {
        c.open(Some(format!("s{i}")), None).await.expect("open");
        assert!(c.send(&format!("s{i}"), "go").await.expect("send"));
    }

    let (sub_id, mut rx) = c.subscribe_with_id("s1", None).await.expect("subscribe s1");
    let mut t1 = Transcript::new();
    drain_records(&mut rx, &mut t1, 6, Duration::from_secs(15)).await;

    c.kill("s0").await.expect("kill s0");

    // s1 streams straight through the kill — a fresh batch arrives after
    // it, with the turn still open.
    let records = drain_records(&mut rx, &mut t1, 10, Duration::from_secs(15)).await;
    assert!(
        !records.iter().any(|r| matches!(
            r.event,
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. }
        )),
        "s1 must be untouched by s0's kill"
    );
    assert!(c.unsubscribe(sub_id).await.expect("unsubscribe"));

    // s2 (never subscribed while the kill happened) is also mid-turn.
    let mut rx2 = c.subscribe("s2", None).await.expect("subscribe s2");
    let mut t2 = Transcript::new();
    let r2 = drain_records(&mut rx2, &mut t2, 6, Duration::from_secs(15)).await;
    assert!(
        !r2.iter().any(|r| matches!(
            r.event,
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. }
        )),
        "s2 must be untouched by s0's kill"
    );

    // The session set states the facts: s0 killed, s1/s2 live.
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < end, "list never settled after the kill");
        let sessions = c.list().await.expect("list");
        let state = |id: &str| {
            sessions
                .iter()
                .find(|s| s.session_id == id)
                .map(|s| (s.state.clone(), s.live))
                .unwrap()
        };
        if state("s0") == ("killed".to_string(), false) {
            assert!(state("s1").1, "s1 still live");
            assert!(state("s2").1, "s2 still live");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    drop(engine);
}

// ---------------------------------------------------------------------------
// I1: engine restart with N live sessions

/// SIGKILL the engine with four sessions mid-turn: the restarted engine
/// reaps every orphaned agent (before it opens its socket), and all four
/// sessions load with the honest repair verdict — interrupted, not live,
/// not lost.
#[tokio::test]
async fn engine_restart_with_n_live_sessions_recovers_all_honestly() {
    const N: usize = 4;
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "slow-stream");
    let c = Client::connect(&dir).await.expect("connect");
    for i in 0..N {
        c.open(Some(format!("s{i}")), None).await.expect("open");
        assert!(c.send(&format!("s{i}"), "go").await.expect("send"));
    }
    // The journal is write-behind: wait until every session's stream has
    // demonstrably reached disk, or repair has nothing to conclude from.
    let end = Instant::now() + Duration::from_secs(15);
    for i in 0..N {
        let jpath = dir
            .join("sessions")
            .join(format!("s{i}"))
            .join("journal.jsonl");
        loop {
            let text = std::fs::read_to_string(&jpath).unwrap_or_default();
            if text.contains("user_delivered") && text.contains("tick") {
                break;
            }
            assert!(Instant::now() < end, "s{i} journal never reached disk");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Capture the live agents' identities, then kill the engine dead.
    let identities: Vec<ProcIdentity> = (0..N)
        .map(|i| {
            Meta::load(&dir.join("sessions").join(format!("s{i}")))
                .expect("meta")
                .child
                .expect("live child breadcrumb")
        })
        .collect();
    engine1.sigkill();

    // Restart. The startup scan (reap + load) completes before the socket
    // opens, so once we can connect the orphans are already dealt with.
    let engine2 = EngineProc::spawn(&dir, "slow-stream");
    for id in &identities {
        assert!(
            proc::identity_of(id.pid)
                .map(|now| now != *id)
                .unwrap_or(true),
            "orphaned agent pid {} survived the restart reap",
            id.pid
        );
    }
    let c2 = Client::connect(&dir).await.expect("connect 2");
    let sessions = c2.list().await.expect("list");
    assert_eq!(sessions.len(), N);
    for s in &sessions {
        assert!(
            !s.live,
            "{} must not report live after restart",
            s.session_id
        );
        assert_eq!(
            s.state, "interrupted",
            "{} was killed mid-turn with the engine — repair must conclude interrupted",
            s.session_id
        );
    }

    drop(engine2);
}

// ---------------------------------------------------------------------------
// I1: background attention reaches watch without a subscription

/// A background session (never subscribed) goes quiet and then hardens to
/// unresponsive — and the watch snapshot states it, while the client is
/// subscribed to a different session the whole time. The rail's badge for
/// a non-selected row is a projection of exactly this.
#[tokio::test]
async fn background_attention_reaches_watch_while_subscribed_elsewhere() {
    let dir = scratch();
    let engine = EngineProc::spawn_with_env(
        &dir,
        "hang-silent",
        &[
            ("EAVE_THRESH_QUIET_MS", "500"),
            ("EAVE_THRESH_CPU_SAMPLES", "3"),
            ("EAVE_THRESH_CPU_SAMPLE_MS", "150"),
        ],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("bg".into()), None).await.expect("open bg");
    c.open(Some("fg".into()), None).await.expect("open fg");
    assert!(c.send("bg", "go hang").await.expect("send bg"));

    // The foreground subscription is to fg — bg is never subscribed.
    let _fg_rx = c.subscribe("fg", None).await.expect("subscribe fg");
    let mut watch = c.watch().await.expect("watch");

    let mut saw_quiet = false;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "watch never reported the background session unresponsive (saw quiet: {saw_quiet})"
        );
        let item = tokio::time::timeout(remaining, watch.recv())
            .await
            .expect("watch frame in time")
            .expect("watch stream open");
        let SubscriptionItem::Sessions { sessions } = item else {
            continue;
        };
        let bg = sessions.iter().find(|s| s.session_id == "bg").unwrap();
        saw_quiet |= bg.state == "quiet";
        if bg.state == "unresponsive" {
            break;
        }
    }
    assert!(
        saw_quiet,
        "quiet precedes unresponsive on the watch channel"
    );

    drop(engine);
}

// ---------------------------------------------------------------------------
// I1: the isolation invariant at N, on raw bytes

/// Three sessions stream concurrently, each with a text canary in its
/// records and a canary-named file in its own repo. One raw connection
/// watches the session set and subscribes to s1's records and files —
/// and across every byte it receives: no canary of any kind on watch, no
/// s0/s2 canary (text or filename) anywhere at all. Positive controls
/// keep it non-vacuous.
/// Stage L: the aggregate resource fact is only worth rendering if the
/// engine actually measures something. `rss_bytes` must be a real
/// resident size for a live agent and **absent** the moment nothing is
/// attached — a stale or zeroed number would let the rail claim a fleet
/// costs nothing while the machine pages.
#[tokio::test]
async fn a_live_agent_reports_what_it_costs_and_a_dead_one_reports_nothing() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");

    // Before any spawn there is no process, so there is no number.
    let before = c.list().await.expect("list");
    assert_eq!(
        before[0].rss_bytes, None,
        "a session that never spawned holds no memory and must say so by \
         absence, not by zero"
    );

    assert!(c.send("s1", "hello").await.expect("send"));
    // Polled to a real size rather than asserted on the first reading:
    // this is a LIVE sample, and a process caught microseconds after
    // exec legitimately holds almost nothing (32 KB, observed) before its
    // pages fault in. What must hold is that the number is a real
    // resident set that grows into one, not a placeholder.
    let end = Instant::now() + Duration::from_secs(15);
    let mut first: Option<u64> = None;
    let rss = loop {
        assert!(
            Instant::now() < end,
            "live agent never reported a real size (first reading: {first:?})"
        );
        if let Some(rss) = c.list().await.expect("list")[0].rss_bytes {
            first.get_or_insert(rss);
            if rss > 500_000 {
                break rss;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        rss > 500_000,
        "a settled process holds real memory; got {rss} bytes, which means \
         the probe is reporting something other than a resident set"
    );

    c.kill("s1").await.expect("kill");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "rss never cleared after the kill");
        let info = c.list().await.expect("list").remove(0);
        if !info.live {
            assert_eq!(
                info.rss_bytes, None,
                "the process is gone — the fact must vanish with it, or the \
                 rail keeps charging the operator for a dead agent"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(engine);
}

#[tokio::test]
async fn no_content_crosses_sessions_on_any_channel_at_n() {
    const TEXT: [&str; 3] = ["TXT_CANARY_s0_e1", "TXT_CANARY_s1_e2", "TXT_CANARY_s2_e3"];
    const FILE: [&str; 3] = [
        "FILE_CANARY_s0_f1.txt",
        "FILE_CANARY_s1_f2.txt",
        "FILE_CANARY_s2_f3.txt",
    ];
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");
    let c = Client::connect(&dir).await.expect("connect");
    for i in 0..3 {
        let cwd = repo();
        // The canary file exists before open, so the open scan lists it.
        std::fs::write(cwd.join(FILE[i]), "canary\n").unwrap();
        c.open(Some(format!("s{i}")), Some(cwd.display().to_string()))
            .await
            .expect("open");
        assert!(c.send(&format!("s{i}"), TEXT[i]).await.expect("send"));
    }

    let mut raw = RawConn::connect(&dir).await;
    raw.send(r#"{"id":2,"method":"watch"}"#).await;
    raw.send(r#"{"id":3,"method":"subscribe","params":{"session_id":"s1"}}"#)
        .await;
    raw.send(r#"{"id":4,"method":"files","params":{"session_id":"s1"}}"#)
        .await;

    // Collect everything while all three stream.
    let lines = raw.collect_for(Duration::from_secs(4)).await;
    assert!(!lines.is_empty(), "raw connection saw nothing at all");

    let mut own_text = false;
    let mut own_file = false;
    for line in &lines {
        let id = frame_id(line);
        if id == 2 {
            for c in TEXT.iter().chain(FILE.iter()) {
                assert!(!line.contains(c), "canary on the watch channel: {line}");
            }
            assert!(
                !line.contains("\"records\""),
                "record structure on the watch channel: {line}"
            );
        }
        // Nothing from s0 or s2 may reach this connection on ANY id: it
        // subscribed only to s1.
        for i in [0usize, 2] {
            assert!(
                !line.contains(TEXT[i]) && !line.contains(FILE[i]),
                "session s{i} content crossed to a connection that never subscribed it: {line}"
            );
        }
        own_text |= id == 3 && line.contains(TEXT[1]);
        own_file |= id == 4 && line.contains(FILE[1]);
    }
    assert!(
        own_text,
        "positive control: s1's text canary on its subscribe id"
    );
    assert!(
        own_file,
        "positive control: s1's file canary on its files id"
    );

    drop(engine);
}
