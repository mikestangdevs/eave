//! Stage E acceptance: attention states against a real engine *process*
//! (spec 01 §States, §Attention clocks; build directive 2026-08-16).
//!
//! The classifier runs at scaled-down clocks via the `EAVE_THRESH_*`
//! dev/test overrides — the state machine, monitor, probes and wire frames
//! are the production ones; only the constants shrink.
//!
//! Data dirs live under `/tmp/eave-wire-tests/` (`sun_path` limit — see
//! wire.rs). Requires workspace-built bins.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eave_core::proc;
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::{socket_path, AttentionSnapshot, SubscriptionItem, WaitReason};

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

/// Scaled clocks: quiet 400 ms, tool 2 s, 3 CPU samples × 100 ms.
const FAST: &[(&str, &str)] = &[
    ("EAVE_THRESH_QUIET_MS", "400"),
    ("EAVE_THRESH_TOOL_MS", "2000"),
    ("EAVE_THRESH_CPU_SAMPLES", "3"),
    ("EAVE_THRESH_CPU_SAMPLE_MS", "100"),
];

struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path, fake_mode: &str, thresholds: &[(&str, &str)]) -> Self {
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
        for (k, v) in thresholds {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn eave-engined");
        let me = Self {
            child,
            data_dir: data_dir.to_path_buf(),
        };
        let sock = socket_path(&me.data_dir);
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "engine never opened its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        me
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Ok(entries) = std::fs::read_dir(self.data_dir.join("sessions")) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(id) = meta.child {
                        if proc::identity_of(id.pid) == Some(id) {
                            unsafe {
                                libc::killpg(id.pid, libc::SIGKILL);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Start a session mid-turn against `fake_mode` and return
/// (client, session_id, subscription).
async fn start(
    dir: &Path,
) -> (
    Client,
    String,
    tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
) {
    let client = Client::connect(dir).await.expect("connect");
    let info = client.open(None, None).await.expect("open");
    let rx = client
        .subscribe(&info.session_id, None)
        .await
        .expect("subscribe");
    let delivered = client.send(&info.session_id, "go").await.expect("send");
    assert!(delivered, "fake accepted the message");
    (client, info.session_id, rx)
}

/// Collect frames until `pred` matches an attention snapshot (returning
/// it), or panic at the deadline listing everything seen.
async fn await_attention(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    deadline: Duration,
    what: &str,
    mut pred: impl FnMut(&AttentionSnapshot) -> bool,
) -> AttentionSnapshot {
    let end = Instant::now() + deadline;
    let mut seen: Vec<String> = Vec::new();
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {what}; attention seen: {seen:?}"
        );
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => continue,
            Ok(None) => panic!("stream ended waiting for {what}; seen: {seen:?}"),
            Ok(Some(SubscriptionItem::Attention { attention, .. })) => {
                if pred(&attention) {
                    return attention;
                }
                seen.push(format!("{attention:?}"));
            }
            Ok(Some(_)) => {}
        }
    }
}

/// Drain for `window`, returning every attention snapshot that arrived.
async fn drain_attention(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    window: Duration,
) -> Vec<AttentionSnapshot> {
    let end = Instant::now() + window;
    let mut out = Vec::new();
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return out;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => return out,
            Ok(None) => return out,
            Ok(Some(SubscriptionItem::Attention { attention, .. })) => out.push(attention),
            Ok(Some(_)) => {}
        }
    }
}

fn agent_identity(dir: &Path, session_id: &str) -> eave_core::proc::ProcIdentity {
    Meta::load(&dir.join("sessions").join(session_id))
        .expect("meta")
        .child
        .expect("live child breadcrumb")
}

// ---------------------------------------------------------------------------

/// hang-silent: quiet (reason: genuinely idle mid-turn) at the threshold,
/// then — zero CPU sustained across N samples with nothing in flight —
/// hardens to unresponsive with the evidence. Never auto-killed.
#[tokio::test]
async fn hang_silent_goes_quiet_idle_then_hardens_unresponsive() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "hang-silent", FAST);
    let (client, sid, mut rx) = start(&dir).await;

    let quiet = await_attention(&mut rx, Duration::from_secs(10), "quiet", |a| {
        matches!(a, AttentionSnapshot::Quiet { .. })
    })
    .await;
    match &quiet {
        AttentionSnapshot::Quiet {
            reason: WaitReason::Idle,
            ..
        } => {}
        other => panic!("wrong wait reason for silent hang: {other:?}"),
    }

    let unresp = await_attention(&mut rx, Duration::from_secs(10), "unresponsive", |a| {
        matches!(a, AttentionSnapshot::Unresponsive { .. })
    })
    .await;
    match unresp {
        AttentionSnapshot::Unresponsive {
            zero_samples,
            sampled_ms,
            ..
        } => {
            assert!(zero_samples >= 3, "evidence: {zero_samples} samples");
            assert!(sampled_ms >= 300, "evidence: {sampled_ms} ms sampled");
        }
        _ => unreachable!(),
    }

    // Never auto-killed: the process is still alive, and list() reports
    // the overlay state.
    let id = agent_identity(&dir, &sid);
    assert!(proc::identity_matches(&id), "agent must still be alive");
    let info = client.list().await.expect("list").remove(0);
    assert_eq!(info.state, "unresponsive");
    assert!(info.live);
}

/// hang-busy: CPU active without output is `quiet`, and stays quiet — the
/// zero-CPU evidence can never accumulate.
#[tokio::test]
async fn hang_busy_stays_quiet_never_unresponsive() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "hang-busy", FAST);
    let (client, _sid, mut rx) = start(&dir).await;

    await_attention(&mut rx, Duration::from_secs(10), "quiet", |a| {
        matches!(a, AttentionSnapshot::Quiet { .. })
    })
    .await;
    // 1.5 s ≥ 4× the full unresponsive window (3 samples × 100 ms): if the
    // busy loop were ever going to harden, it would have.
    let later = drain_attention(&mut rx, Duration::from_millis(1500)).await;
    assert!(
        !later
            .iter()
            .any(|a| matches!(a, AttentionSnapshot::Unresponsive { .. })),
        "busy hang must never classify unresponsive: {later:?}"
    );
    let info = client.list().await.expect("list").remove(0);
    assert_eq!(info.state, "quiet");
}

/// THE regression that matters most (build directive): after
/// turn_completed the CLI lingers on stdin at zero CPU — that idleness is
/// normal and classifies as nothing at all.
#[tokio::test]
async fn idle_between_turns_after_turn_completed_is_no_state() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "linger", FAST);
    let (client, _sid, mut rx) = start(&dir).await;

    // Wait for the turn to complete on the wire.
    let end = Instant::now() + Duration::from_secs(10);
    let mut done = false;
    while !done {
        assert!(Instant::now() < end, "turn never completed");
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(SubscriptionItem::Events { records, .. })) => {
                done = records.iter().any(|r| {
                    matches!(
                        &r.event,
                        eave_core::journal::JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::TurnCompleted { .. }
                        }
                    )
                });
            }
            Ok(Some(_)) => {}
            _ => {}
        }
    }

    // 2 s of between-turns silence = 5× the quiet threshold and 6× the
    // unresponsive window at these clocks. Nothing may classify.
    let later = drain_attention(&mut rx, Duration::from_secs(2)).await;
    assert!(
        !later
            .iter()
            .any(|a| !matches!(a, AttentionSnapshot::Running)),
        "idle between turns must not classify: {later:?}"
    );
    let info = client.list().await.expect("list").remove(0);
    assert_eq!(info.state, "running", "lingering CLI stays plain running");
}

/// A long legitimate tool call: far past QUIET_AFTER nothing fires (the
/// quiet clock does not run while a tool is outstanding); at
/// TOOL_EXEC_AFTER the notice names the tool.
#[tokio::test]
async fn outstanding_tool_suppresses_quiet_then_names_the_tool() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "tool-hang", FAST);
    let (_client, _sid, mut rx) = start(&dir).await;

    // 1.2 s = 3× the quiet threshold, well under the 2 s tool threshold —
    // the scaled shape of "an 8-minute tool call does not trip quiet".
    let early = drain_attention(&mut rx, Duration::from_millis(1200)).await;
    assert!(
        !early
            .iter()
            .any(|a| !matches!(a, AttentionSnapshot::Running)),
        "tool under its threshold must not classify: {early:?}"
    );

    let quiet = await_attention(&mut rx, Duration::from_secs(10), "quiet/tool", |a| {
        matches!(a, AttentionSnapshot::Quiet { .. })
    })
    .await;
    match quiet {
        AttentionSnapshot::Quiet {
            reason: WaitReason::Tool { name, .. },
            ..
        } => assert_eq!(name, "Bash", "notice names the outstanding tool"),
        other => panic!("expected tool wait reason: {other:?}"),
    }
}

/// An API request in flight: quiet names "awaiting API", and zero CPU must
/// NOT harden it — waiting on the network burns no CPU by design.
#[tokio::test]
async fn requesting_hang_names_awaiting_api_and_never_hardens() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "requesting-hang", FAST);
    let (_client, _sid, mut rx) = start(&dir).await;

    let quiet = await_attention(&mut rx, Duration::from_secs(10), "quiet/api", |a| {
        matches!(a, AttentionSnapshot::Quiet { .. })
    })
    .await;
    assert!(
        matches!(
            quiet,
            AttentionSnapshot::Quiet {
                reason: WaitReason::Api { .. },
                ..
            }
        ),
        "expected awaiting-API reason: {quiet:?}"
    );
    let later = drain_attention(&mut rx, Duration::from_millis(1500)).await;
    assert!(
        !later
            .iter()
            .any(|a| matches!(a, AttentionSnapshot::Unresponsive { .. })),
        "awaiting-API must never harden to unresponsive: {later:?}"
    );
}

/// Provable death without process exit: the CLI closes stdout mid-turn and
/// stays alive → `failed` with the concrete cause, on the probe cadence
/// (no attention timer involved — the quiet clock here is 8 s, the verdict
/// arrives in ~2 probe ticks), and never auto-killed.
#[tokio::test]
async fn stdout_close_mid_turn_is_failed_with_cause() {
    let dir = scratch();
    let slow: &[(&str, &str)] = &[
        ("EAVE_THRESH_QUIET_MS", "8000"),
        ("EAVE_THRESH_TOOL_MS", "600000"),
        ("EAVE_THRESH_CPU_SAMPLES", "6"),
        ("EAVE_THRESH_CPU_SAMPLE_MS", "5000"),
    ];
    let _engine = EngineProc::spawn(&dir, "stdout-close", slow);
    let (client, sid, mut rx) = start(&dir).await;

    let t0 = Instant::now();
    let failed = await_attention(&mut rx, Duration::from_secs(10), "failed", |a| {
        matches!(a, AttentionSnapshot::Failed { .. })
    })
    .await;
    let waited = t0.elapsed();
    match &failed {
        AttentionSnapshot::Failed { cause, .. } => {
            assert!(
                cause.contains("stdout closed mid-turn"),
                "concrete cause recorded: {cause}"
            );
        }
        _ => unreachable!(),
    }
    assert!(
        waited < Duration::from_secs(5),
        "failed must come from the probe, not a threshold: {waited:?}"
    );

    // Never auto-killed, even provably dead: kill stays the user's call.
    let id = agent_identity(&dir, &sid);
    assert!(proc::identity_matches(&id), "process left alive");
    let info = client.list().await.expect("list").remove(0);
    assert_eq!(info.state, "failed");

    // The user's kill still works and yields the journal's terminal truth.
    client.kill(&sid).await.expect("kill");
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < end, "kill never settled");
        let info = client.list().await.expect("list").remove(0);
        if !info.live {
            assert_eq!(info.state, "killed");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A subscriber that arrives while a notice is standing sees it
/// immediately (the snapshot rides along after replay) — the relaunch-
/// while-quiet shape.
#[tokio::test]
async fn late_subscriber_sees_standing_attention() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "hang-silent", FAST);
    let (client, sid, mut rx) = start(&dir).await;
    await_attention(&mut rx, Duration::from_secs(10), "quiet", |a| {
        matches!(a, AttentionSnapshot::Quiet { .. })
    })
    .await;

    let client2 = Client::connect(&dir).await.expect("second client");
    let mut rx2 = client2.subscribe(&sid, None).await.expect("subscribe");
    // No new transition needed: the standing overlay arrives with replay.
    await_attention(&mut rx2, Duration::from_secs(2), "standing notice", |a| {
        !matches!(a, AttentionSnapshot::Running)
    })
    .await;
    drop(client);
}
