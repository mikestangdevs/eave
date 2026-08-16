//! Stage C acceptance: warm reconnect, cursor contract, contended start,
//! delivery rendering, repaired-session replay — all against a real engine
//! *process* over the real socket (the UI is a thin projection of exactly
//! this client + `Transcript` layer).
//!
//! Data dirs live under `/tmp/eave-wire-tests/...` — deliberately not the
//! session scratchpad: UNIX `sun_path` is ~104 bytes on macOS and the
//! scratchpad path already exceeds it.
//!
//! Requires workspace-built bins (`cargo test` at the workspace root):
//! `eave-engined` comes via CARGO_BIN_EXE; `fake-agent` is found next to it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eave_core::journal::JournalEvent;
use eave_core::proc::{self, ProcIdentity};
use eave_core::store::Meta;
use eave_engine::client::{Client, ClientError, EngineSpawn};
use eave_wire::model::{Delivery, Row, ToolStatus, Transcript};
use eave_wire::{socket_path, SubscriptionItem};

fn engined_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"))
}

fn fake_agent_bin() -> PathBuf {
    let path = engined_bin()
        .parent()
        .expect("bin dir")
        .join("fake-agent");
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

/// A running engine process. Drop kills it and its agent children so a
/// failing assertion never leaks processes.
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
        // Probe with a real connect: a stale socket *file* from a killed
        // previous engine exists but refuses.
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
        // Reap agent children the engine left behind, guarded by the meta
        // breadcrumb identity (never a bare pid).
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
                // setsid at spawn ⇒ pgid == pid.
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
        assert!(!remaining.is_zero(), "timed out waiting for records (have {})", got.len());
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
        }
    }
    got
}

/// Kill the UI mid-turn; relaunch; the turn is still streaming. Twice in a
/// row, cursor honored, no duplicate events, no gaps (spec 04 acceptance;
/// Stage C directive).
#[tokio::test]
async fn warm_reconnect_streams_across_client_generations() {
    let dir = scratch();
    let mut engine = EngineProc::spawn(&dir, "slow-stream");

    // Generation 1: open, send (spawns the agent), watch the turn stream.
    let c1 = Client::connect(&dir).await.expect("connect gen1");
    let info = c1.open(Some("s1".into()), None).await.expect("open");
    assert_eq!(info.state, "new");
    assert!(c1.send("s1", "go").await.expect("send"));
    let mut t1 = Transcript::new();
    let mut rx1 = c1.subscribe("s1", None).await.expect("subscribe gen1");
    let first = drain_records(&mut rx1, &mut t1, 8, Duration::from_secs(15)).await;
    assert_eq!(first.first().unwrap().seq, 1, "full replay starts at seq 1");
    let cursor1 = t1.cursor.expect("cursor after records");
    // Abrupt death of the UI: the socket just closes. From the engine's
    // side a SIGKILLed client is exactly this.
    drop(rx1);
    drop(c1);

    // The turn must keep streaming with zero clients attached.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Generation 2: resume from cursor — replay continues at seq+1.
    let c2 = Client::connect(&dir).await.expect("connect gen2");
    let mut rx2 = c2.subscribe("s1", Some(cursor1)).await.expect("subscribe gen2");
    let mut t2 = Transcript { cursor: Some(cursor1), ..Default::default() };
    let second = drain_records(&mut rx2, &mut t2, 8, Duration::from_secs(15)).await;
    assert_eq!(
        second.first().unwrap().seq,
        cursor1.seq + 1,
        "cursor honored: no gap, no duplicate"
    );
    // The disconnected window kept streaming: more than one tick arrived
    // in the first batch after resubscribe.
    assert!(
        second.iter().filter(|r| r.seq <= cursor1.seq + 600 / 50).count() > 3,
        "records accumulated while no client was attached"
    );
    let cursor2 = t2.cursor.unwrap();
    drop(rx2);
    drop(c2);

    // Generation 3: again — and this time also prove the no-cursor path
    // (a relaunched UI holds no projection): full replay from seq 1.
    let c3 = Client::connect(&dir).await.expect("connect gen3");
    let mut rx3 = c3.subscribe("s1", Some(cursor2)).await.expect("subscribe gen3");
    let mut t3 = Transcript { cursor: Some(cursor2), ..Default::default() };
    let third = drain_records(&mut rx3, &mut t3, 4, Duration::from_secs(15)).await;
    assert_eq!(third.first().unwrap().seq, cursor2.seq + 1);

    let mut t_full = Transcript::new();
    let mut rx_full = c3.subscribe("s1", None).await.expect("subscribe full");
    let full = drain_records(&mut rx_full, &mut t_full, cursor2.seq as usize, Duration::from_secs(15)).await;
    assert_eq!(full.first().unwrap().seq, 1);
    let seqs: Vec<u64> = full.iter().map(|r| r.seq).collect();
    for w in seqs.windows(2) {
        assert_eq!(w[1], w[0] + 1, "full replay is contiguous, no dups");
    }

    // The running turn was never disturbed by any of this.
    let all = [first, second, third].concat();
    assert!(
        !all.iter().any(|r| matches!(
            r.event,
            JournalEvent::TurnInterrupted { .. } | JournalEvent::Closed { .. }
        )),
        "turn survived client churn"
    );

    engine.sigkill();
}

/// Engine dies mid-turn and restarts: epoch changes, old cursor is refused
/// with a Reset, and the replayed (repaired) session renders interrupted —
/// not a phantom completion.
#[tokio::test]
async fn engine_restart_resets_epoch_and_replays_repair() {
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "tool-hang");

    let c1 = Client::connect(&dir).await.expect("connect");
    c1.open(Some("s1".into()), None).await.expect("open");
    assert!(c1.send("s1", "dig in").await.expect("send"));
    let mut t1 = Transcript::new();
    let mut rx1 = c1.subscribe("s1", None).await.expect("subscribe");
    // Wait until the tool is provably outstanding on the wire.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "tool never started");
        drain_records(&mut rx1, &mut t1, 1, Duration::from_secs(15)).await;
        if t1
            .rows
            .iter()
            .any(|r| matches!(r, Row::Tool { status: ToolStatus::Running, .. }))
        {
            break;
        }
    }
    let stale_cursor = t1.cursor.unwrap();
    let agent = Meta::load(&dir.join("sessions/s1")).expect("meta").child;
    drop(rx1);
    drop(c1);

    // The wire tap runs ahead of the 120 ms write-behind flush; the repair
    // we assert on derives from what reached *disk*. Wait for it.
    let jpath = dir.join("sessions/s1/journal.jsonl");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let bytes = std::fs::read_to_string(&jpath).unwrap_or_default();
        if bytes.contains("tool_use_started") && bytes.contains("user_delivered") {
            break;
        }
        assert!(Instant::now() < end, "journal never flushed the tool start");
        std::thread::sleep(Duration::from_millis(20));
    }

    engine1.sigkill();
    // Skip EngineProc's Drop cleanup: the orphaned agent must still be
    // there for engine 2's startup scan to reap — that is the thing under
    // test. (A fallback reap runs after the asserts.)
    std::mem::forget(engine1);

    // Engine 2 adopts the dir: reaps the orphan, load repairs the journal.
    let engine2 = EngineProc::spawn(&dir, "tool-hang");
    let c2 = Client::connect(&dir).await.expect("connect 2");

    // The orphaned agent from engine 1 is gone (identity-guarded check).
    if let Some(id) = agent {
        let gone_by = Instant::now() + Duration::from_secs(10);
        loop {
            let alive = proc::identity_of(id.pid).map(|cur| cur == id).unwrap_or(false);
            if !alive {
                break;
            }
            assert!(Instant::now() < gone_by, "orphaned agent was not reaped");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let mut rx2 = c2.subscribe("s1", Some(stale_cursor)).await.expect("subscribe 2");
    let first = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .expect("frame in time")
        .expect("stream open");
    let SubscriptionItem::Reset { epoch, .. } = &first else {
        panic!("stale epoch must be answered with Reset, got {first:?}");
    };
    assert_ne!(*epoch, stale_cursor.epoch, "epoch changed across engine restart");

    let mut t2 = Transcript::new();
    let mut rx_records = 0usize;
    let end = Instant::now() + Duration::from_secs(10);
    while t2.closed.is_none() {
        assert!(Instant::now() < end, "never saw terminal record in replay");
        match tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .expect("frame")
            .expect("stream open")
        {
            SubscriptionItem::Reset { .. } => t2.reset(),
            SubscriptionItem::Events { epoch, records, .. } => {
                rx_records += records.len();
                t2.apply(epoch, &records).unwrap();
            }
        }
    }
    assert!(rx_records > 0);
    assert_eq!(t2.closed.as_deref(), Some("interrupted"));
    assert!(t2.rows.iter().any(
        |r| matches!(r, Row::Tool { status: ToolStatus::Interrupted, .. })
    ));
    assert!(t2
        .rows
        .iter()
        .any(|r| matches!(r, Row::Marker { text } if text.contains("turn interrupted"))));
    assert!(
        !t2.rows
            .iter()
            .any(|r| matches!(r, Row::Marker { text } if text.contains("closed: done"))),
        "no phantom completion"
    );

    // Fallback: if engine 2 failed to reap (assert above already caught
    // it), don't leak the hung agent past the test.
    if let Some(id) = agent {
        kill_if_matches(&id);
    }
    drop(engine2);
}

/// Second app instance while one holds the lock: Busy names the holder,
/// nothing crashes, the first instance's state is untouched.
#[tokio::test]
async fn contended_start_surfaces_busy_with_holder() {
    let dir = scratch();
    let engine1 = EngineProc::spawn(&dir, "tool-cycle");
    let c1 = Client::connect(&dir).await.expect("connect");
    c1.open(Some("s1".into()), None).await.expect("open");

    // Force the would-spawn path: hide the socket so connect refuses while
    // the flock stays held by engine 1.
    let sock = socket_path(&dir);
    let hidden = dir.join("engine.sock.hidden");
    std::fs::rename(&sock, &hidden).unwrap();

    let spawn = EngineSpawn {
        program: engined_bin(),
        args: vec!["--data-dir".into(), dir.display().to_string()],
    };
    let err = match Client::connect_or_spawn(&dir, &spawn).await {
        Ok(_) => panic!("second engine must lose the lock"),
        Err(e) => e,
    };
    match err {
        ClientError::Busy { holder } => {
            let holder = holder.expect("holder identity recorded in engine.lock");
            assert_eq!(holder.pid, engine1.pid(), "busy names the running engine");
        }
        other => panic!("expected Busy, got {other:?}"),
    }

    // First instance provably undisturbed: still serving on the same
    // socket inode, session intact.
    std::fs::rename(&hidden, &sock).unwrap();
    let c2 = Client::connect(&dir).await.expect("engine 1 still serving");
    let sessions = c2.list().await.expect("list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "s1");

    drop(engine1);
}

/// An undelivered message is distinct from a sent one all the way through
/// the wire and the shared projection (spec 02 §Delivery state).
#[tokio::test]
async fn undelivered_send_renders_distinct_from_sent() {
    // Delivered control: a normal agent confirms delivery.
    let dir_ok = scratch();
    let engine_ok = EngineProc::spawn(&dir_ok, "tool-cycle");
    let c = Client::connect(&dir_ok).await.expect("connect");
    c.open(Some("ok".into()), None).await.expect("open");
    assert!(c.send("ok", "hello").await.expect("send"), "delivered");
    let mut t = Transcript::new();
    let mut rx = c.subscribe("ok", None).await.expect("subscribe");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end);
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        if t.rows
            .iter()
            .any(|r| matches!(r, Row::User { delivery: Delivery::Delivered, .. }))
        {
            break;
        }
    }
    drop(engine_ok);

    // Undelivered: agent alive but its stdin is closed — EPIPE at the
    // engine, journaled-but-undelivered on the wire. The first (warmup)
    // send races the fake's `close(0)` and may land in the pipe buffer;
    // once `init` is on the wire the close provably happened, so the next
    // send must fail.
    let dir_bad = scratch();
    let engine_bad = EngineProc::spawn(&dir_bad, "stdin-closed");
    let c = Client::connect(&dir_bad).await.expect("connect");
    c.open(Some("bad".into()), None).await.expect("open");
    let _ = c.send("bad", "warmup").await.expect("warmup send");
    let mut t = Transcript::new();
    let mut rx = c.subscribe("bad", None).await.expect("subscribe");
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end);
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        if t.rows
            .iter()
            .any(|r| matches!(r, Row::Marker { text } if text.contains("session started")))
        {
            break;
        }
    }
    assert!(
        !c.send("bad", "lost words").await.expect("send"),
        "pipe write must fail after the agent closed its stdin"
    );
    // Live session, unconfirmed: renders Pending (not sent).
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end);
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
        if t.rows.iter().any(|r| matches!(
            r,
            Row::User { text, delivery: Delivery::Pending } if text == "lost words"
        )) {
            break;
        }
    }
    // Kill settles it: Closed arrives, the send is provably undelivered.
    c.kill("bad").await.expect("kill");
    let end = Instant::now() + Duration::from_secs(15);
    while t.closed.is_none() {
        assert!(Instant::now() < end, "never saw Closed after kill");
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    }
    assert_eq!(t.closed.as_deref(), Some("killed"));
    assert!(
        t.rows.iter().any(|r| matches!(
            r,
            Row::User { text, delivery: Delivery::Undelivered } if text == "lost words"
        )),
        "undelivered send renders distinct: {:?}",
        t.rows
    );
    assert!(
        !t.rows.iter().any(|r| matches!(
            r,
            Row::User { text, delivery: Delivery::Delivered } if text == "lost words"
        )),
        "the failed send must never render as sent"
    );
    drop(engine_bad);
}

/// Continuation across engine restart uses `--resume` with the same
/// provider session id (spec 02 §Resume contract) — the wire-level cold
/// resume: terminal session + new send = new CLI process on the same id.
#[tokio::test]
async fn send_after_terminal_outcome_resumes_same_provider_session() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "resume-probe");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");

    // First send: fresh session (--session-id); resume-probe reports and
    // exits, leaving the journal terminal.
    assert!(c.send("s1", "one").await.expect("send"));
    let mut t = Transcript::new();
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    let end = Instant::now() + Duration::from_secs(15);
    while t.closed.is_none() {
        assert!(Instant::now() < end);
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    }
    let flags: Vec<String> = t
        .rows
        .iter()
        .filter_map(|r| match r {
            Row::Assistant { text, .. } if text.contains("RESUME_FLAG") => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(flags.len(), 1);
    assert!(flags[0].contains("RESUME_FLAG:false"), "{flags:?}");
    assert!(flags[0].contains("ID:s1"), "{flags:?}");

    // Second send on the now-terminal session: same id, --resume.
    t.closed = None;
    assert!(c.send("s1", "two").await.expect("send 2"));
    let end = Instant::now() + Duration::from_secs(15);
    while t.closed.is_none() {
        assert!(Instant::now() < end);
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    }
    let flags: Vec<String> = t
        .rows
        .iter()
        .filter_map(|r| match r {
            Row::Assistant { text, .. } if text.contains("RESUME_FLAG") => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(flags.len(), 2, "{flags:?}");
    assert!(flags[1].contains("RESUME_FLAG:true"), "{flags:?}");
    assert!(flags[1].contains("ID:s1"), "{flags:?}");

    drop(engine);
}
