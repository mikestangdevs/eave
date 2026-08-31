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

/// A running engine process. Drop kills it and its agent children so a
/// failing assertion never leaks processes.
struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path, fake_mode: &str) -> Self {
        Self::spawn_with_env(data_dir, fake_mode, &[])
    }

    /// Same, plus `EAVE_*` variables the engine forwards to the agent —
    /// how a test makes the fake agent announce an id eave did not choose.
    fn spawn_with_env(data_dir: &Path, fake_mode: &str, env: &[(&str, &str)]) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let mut cmd = Command::new(engined_bin());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd
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
            // Transient overlay — not part of the record stream these
            // helpers assert on (Stage E has its own suite).
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
    let mut rx2 = c2
        .subscribe("s1", Some(cursor1))
        .await
        .expect("subscribe gen2");
    let mut t2 = Transcript::new();
    t2.cursor = Some(cursor1);
    let second = drain_records(&mut rx2, &mut t2, 8, Duration::from_secs(15)).await;
    assert_eq!(
        second.first().unwrap().seq,
        cursor1.seq + 1,
        "cursor honored: no gap, no duplicate"
    );
    // The disconnected window kept streaming: more than one tick arrived
    // in the first batch after resubscribe.
    assert!(
        second
            .iter()
            .filter(|r| r.seq <= cursor1.seq + 600 / 50)
            .count()
            > 3,
        "records accumulated while no client was attached"
    );
    let cursor2 = t2.cursor.unwrap();
    drop(rx2);
    drop(c2);

    // Generation 3: again — and this time also prove the no-cursor path
    // (a relaunched UI holds no projection): full replay from seq 1.
    let c3 = Client::connect(&dir).await.expect("connect gen3");
    let mut rx3 = c3
        .subscribe("s1", Some(cursor2))
        .await
        .expect("subscribe gen3");
    let mut t3 = Transcript::new();
    t3.cursor = Some(cursor2);
    let third = drain_records(&mut rx3, &mut t3, 4, Duration::from_secs(15)).await;
    assert_eq!(third.first().unwrap().seq, cursor2.seq + 1);

    let mut t_full = Transcript::new();
    let mut rx_full = c3.subscribe("s1", None).await.expect("subscribe full");
    let full = drain_records(
        &mut rx_full,
        &mut t_full,
        cursor2.seq as usize,
        Duration::from_secs(15),
    )
    .await;
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
        if t1.rows.iter().any(|r| {
            matches!(
                r,
                Row::Tool {
                    status: ToolStatus::Running,
                    ..
                }
            )
        }) {
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
            let alive = proc::identity_of(id.pid)
                .map(|cur| cur == id)
                .unwrap_or(false);
            if !alive {
                break;
            }
            assert!(Instant::now() < gone_by, "orphaned agent was not reaped");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let mut rx2 = c2
        .subscribe("s1", Some(stale_cursor))
        .await
        .expect("subscribe 2");
    let first = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .expect("frame in time")
        .expect("stream open");
    let SubscriptionItem::Reset { epoch, .. } = &first else {
        panic!("stale epoch must be answered with Reset, got {first:?}");
    };
    assert_ne!(
        *epoch, stale_cursor.epoch,
        "epoch changed across engine restart"
    );

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
    assert!(rx_records > 0);
    assert_eq!(t2.closed.as_deref(), Some("interrupted"));
    assert!(t2.rows.iter().any(|r| matches!(
        r,
        Row::Tool {
            status: ToolStatus::Interrupted,
            ..
        }
    )));
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
        if t.rows.iter().any(|r| {
            matches!(
                r,
                Row::User {
                    delivery: Delivery::Delivered,
                    ..
                }
            )
        }) {
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
        if t.rows.iter().any(|r| {
            matches!(
                r,
                Row::User { text, delivery: Delivery::Pending, .. } if text == "lost words"
            )
        }) {
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
            Row::User { text, delivery: Delivery::Undelivered, .. } if text == "lost words"
        )),
        "undelivered send renders distinct: {:?}",
        t.rows
    );
    assert!(
        !t.rows.iter().any(|r| matches!(
            r,
            Row::User { text, delivery: Delivery::Delivered, .. } if text == "lost words"
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

/// Cold resume names the id the PROVIDER announced, not the id eave
/// proposed (slice CD, 2026-08-20).
///
/// `send_after_terminal_outcome_resumes_same_provider_session` above
/// cannot see this: its fake agent announces the same id eave passed, so
/// resuming under either one looks identical. Here `FAKE_SESSION_ID`
/// makes the agent announce `provider-minted-9` while eave opened the
/// session as `s1` — the shape of every non-Claude provider, all of which
/// mint their own id (Grok and Hermes at `session/new`, Codex at thread
/// creation) and have never heard of eave's uuid.
///
/// Before this slice the engine computed `resume` as a bare `bool` —
/// "has this session ever seen an `Init`?" — and then resumed under
/// `cfg.session_id`. The provider's id was sitting in the journal one
/// field away from the code that needed it. `AgentSession::
/// provider_session_id()` existed on all five adapters, was asserted by
/// their tests, and had no consumer in the engine at all.
#[tokio::test]
async fn cold_resume_carries_the_providers_own_id_not_eaves() {
    let dir = scratch();
    let engine = EngineProc::spawn_with_env(
        &dir,
        "resume-probe",
        &[("EAVE_FAKE_SESSION_ID", "provider-minted-9")],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");

    let mut t = Transcript::new();
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");

    let flags = |t: &Transcript| -> Vec<String> {
        t.rows
            .iter()
            .filter_map(|r| match r {
                Row::Assistant { text, .. } if text.contains("RESUME_FLAG") => Some(text.clone()),
                _ => None,
            })
            .collect()
    };

    // Generation 1: fresh. eave names `s1` on argv; the agent answers with
    // an id of its own, which is what lands in the journal's `Init`.
    assert!(c.send("s1", "one").await.expect("send"));
    let end = Instant::now() + Duration::from_secs(15);
    while t.closed.is_none() {
        assert!(Instant::now() < end, "generation 1 never settled");
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    }
    let g1 = flags(&t);
    assert_eq!(g1.len(), 1, "{g1:?}");
    assert!(g1[0].contains("RESUME_FLAG:false"), "{g1:?}");
    assert!(
        g1[0].contains("ARGV_ID:s1"),
        "a FRESH session is named with eave's own id: {g1:?}"
    );

    // Generation 2: the terminal session gets another send. This must go
    // out as `--resume provider-minted-9`.
    t.closed = None;
    assert!(c.send("s1", "two").await.expect("send 2"));
    let end = Instant::now() + Duration::from_secs(15);
    while t.closed.is_none() {
        assert!(Instant::now() < end, "generation 2 never settled");
        drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
    }
    let g2 = flags(&t);
    assert_eq!(g2.len(), 2, "{g2:?}");
    assert!(g2[1].contains("RESUME_FLAG:true"), "{g2:?}");
    assert!(
        g2[1].contains("ARGV_ID:provider-minted-9"),
        "a resume must carry the id the provider announced, not eave's: {g2:?}"
    );
    assert!(
        !g2[1].contains("ARGV_ID:s1"),
        "resuming under eave's own uuid is the bug this test exists for: {g2:?}"
    );

    drop(engine);
}

/// Strict delta-folding (Stage D; spec 03 §Streaming commit): the wire
/// must carry at most one delta-only Events frame per ~120 ms window,
/// each folding several records — not one frame per record.
///
/// **What this test may not measure.** Its first version timed frame
/// ARRIVALS with `Instant::now()` in the reading loop and required
/// consecutive delta frames to be ≥90 ms apart with nearly every frame
/// folding ≥2 records. Both numbers describe things the engine does not
/// control:
///
/// - An arrival timestamp is when this task was *scheduled to read*, not
///   when the engine sent. On a loaded box the reader stalls, frames
///   queue in the channel, and two are then dequeued microseconds apart:
///   measured "58 ms", "32 ms", engine entirely innocent. 25 failures in
///   48 runs at load ~300 on 2026-08-20, every one of them this.
/// - "nearly every frame folds ≥2" is the fake agent's 50 ms sleep
///   divided by the 120 ms window. When load stretches those sleeps past
///   120 ms, `engine.rs` says the leading edge of a burst after quiet
///   **flashes through immediately** — so one record per frame is the
///   documented, correct answer, and the old assertion called it a bug.
///
/// So both claims are made from the engine's own record timestamps, which
/// no amount of client-side scheduling can move:
///
/// 1. **The budget.** Frames cannot outpace one per window, however fast
///    the deltas arrive. Asserted in aggregate over the measured span —
///    the same 120 ms, not a looser one.
/// 2. **Non-vacuity.** More records than frames, so folding provably
///    happened; without it claim 1 passes on a stream too slow to fold.
#[tokio::test]
async fn deltas_fold_into_120ms_frames() {
    /// The window under test (spec 03 §Streaming commit,
    /// `engine.rs::COALESCE`). Used as the contract, never as a knob.
    const COALESCE_MS: u64 = 120;

    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "slow-stream");

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    assert!(c.send("s1", "go").await.expect("send"));
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");

    // (records, first ts_ms, last ts_ms, all_deltas) — every timestamp
    // here was stamped by the engine when the record was appended.
    let mut t = Transcript::new();
    let mut frames: Vec<(usize, u64, u64, bool)> = Vec::new();
    let end = Instant::now() + Duration::from_secs(8);
    while frames.len() < 12 {
        assert!(Instant::now() < end, "not enough frames: {}", frames.len());
        let item = tokio::time::timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("frame in time")
            .expect("stream open");
        match item {
            SubscriptionItem::Reset { .. } => t.reset(),
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
            SubscriptionItem::Events { epoch, records, .. } => {
                t.apply(epoch, &records).expect("no seq gaps");
                let all_deltas = records.iter().all(|r| {
                    matches!(
                        &r.event,
                        JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::TextDelta { .. }
                        }
                    )
                });
                frames.push((
                    records.len(),
                    records.first().unwrap().ts_ms,
                    records.last().unwrap().ts_ms,
                    all_deltas,
                ));
            }
        }
    }
    drop(rx);

    // Steady state: skip the leading edge (replay + turn start), keep the
    // delta-only frames.
    let steady: Vec<&(usize, u64, u64, bool)> = frames.iter().skip(3).filter(|f| f.3).collect();
    assert!(
        steady.len() >= 6,
        "expected a steady delta stream, got {}",
        steady.len()
    );

    // Claim 1: the budget. `span` runs between engine record stamps, and
    // the emissions that bracket it sit outside it — the window that
    // produced the first frame opened before its first record was
    // stamped, and the last frame was sent after its last record was.
    // That is two windows the span cannot see, so the count is allowed
    // `span / COALESCE + 2`. One frame per record would need far more:
    // at the fake's cadence the same span would carry ~2.4x this many.
    let span_ms = steady.last().unwrap().2 - steady.first().unwrap().1;
    let ceiling = (span_ms / COALESCE_MS) + 2;
    assert!(
        steady.len() as u64 <= ceiling,
        "{} delta frames across {span_ms} ms — more than one per {COALESCE_MS} ms window \
         (ceiling {ceiling}); counts {:?}",
        steady.len(),
        steady.iter().map(|f| f.0).collect::<Vec<_>>()
    );

    // Claim 2: non-vacuity. A stream too slow to fold would satisfy claim
    // 1 without the coalescer existing at all.
    let total: usize = steady.iter().map(|f| f.0).sum();
    assert!(
        total > steady.len(),
        "no frame folded anything, so nothing here tested folding: {:?}",
        steady.iter().map(|f| f.0).collect::<Vec<_>>()
    );

    drop(engine);
}
