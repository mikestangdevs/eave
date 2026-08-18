//! Slice K acceptance: interactive approvals, end to end through the real
//! spawn → ask → decide → act path (spec 04 §Permissions, spec 01
//! §Attention clocks, spec 05 §Watch).
//!
//! Everything here runs the real engine binary against the fake agent
//! speaking the control-frame shapes captured from claude 2.1.226 on
//! 2026-08-18 (`docs/research/approvals-capture-2026-08-18/`). The four
//! things that must be true, and are each proven below:
//!
//! 1. A question reaches the operator and the answer reaches the agent —
//!    including the operator's own words on a deny.
//! 2. A session parked on a question is never reported as ambiguous and
//!    never offered a kill, no matter how long it waits.
//! 3. A CLI that cannot prove it will ask does not get to run.
//! 4. The watch channel says a session is waiting and how many times, and
//!    never what tool or which path — that is transcript content.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use eave_core::journal::JournalEvent;
use eave_core::proc;
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::model::{ApprovalStatus, Row, Transcript};
use eave_wire::{socket_path, AttentionSnapshot, Decision, SubscriptionItem};

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
        Self::spawn_with(data_dir, fake_mode, &[])
    }

    fn spawn_with(data_dir: &Path, fake_mode: &str, env: &[(&str, &str)]) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let mut cmd = Command::new(engined_bin());
        cmd.args(["--data-dir", &data_dir.display().to_string()])
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", fake_mode)
            // Scaled-down clocks so "past both thresholds" is seconds, not
            // twelve minutes. The classifier itself is the real one.
            .env("EAVE_THRESH_QUIET_MS", "300")
            .env("EAVE_THRESH_TOOL_MS", "600")
            .env("EAVE_THRESH_CPU_SAMPLE_MS", "60")
            .env("EAVE_THRESH_CPU_SAMPLES", "3")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        for (k, v) in env {
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
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Ok(entries) = std::fs::read_dir(self.data_dir.join("sessions")) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(id) = meta.child {
                        if let Some(current) = proc::identity_of(id.pid) {
                            if current == id {
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
}

/// Fold subscription frames until `pred` holds over the transcript, or
/// time out naming what the transcript did contain.
async fn fold_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    t: &mut Transcript,
    what: &str,
    deadline: Duration,
    mut pred: impl FnMut(&Transcript) -> bool,
) {
    let end = Instant::now() + deadline;
    loop {
        if pred(t) {
            return;
        }
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {what}; rows: {:?}",
            t.rows
        );
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("subscription ended");
        match item {
            SubscriptionItem::Reset { .. } => t.reset(),
            SubscriptionItem::Events { epoch, records, .. } => {
                t.apply(epoch, &records).expect("no seq gaps");
            }
            _ => {}
        }
    }
}

fn approval_row(t: &Transcript) -> Option<(String, ApprovalStatus)> {
    t.rows.iter().find_map(|r| match r {
        Row::Approval {
            request_id, status, ..
        } => Some((request_id.clone(), status.clone())),
        _ => None,
    })
}

fn tool_result_text(t: &Transcript) -> Vec<String> {
    t.rows
        .iter()
        .filter_map(|r| match r {
            Row::Assistant { text, .. } | Row::Marker { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The whole point of the slice: the agent asks, the session waits, the
/// operator answers, and the tool runs — with every fact needed to decide
/// present on the question.
#[tokio::test]
async fn an_approval_asks_waits_and_then_acts_on_the_answer() {
    let dir = scratch();
    let _engine = EngineProc::spawn_with(
        &dir,
        "approval-cycle",
        &[("EAVE_FAKE_APPROVAL_PATH", "/tmp/eave-fake/canary-path.txt")],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "write the file").await.expect("send"));

    let mut t = Transcript::new();
    fold_until(&mut rx, &mut t, "the question", Duration::from_secs(15), |t| {
        approval_row(t).is_some()
    })
    .await;

    let (rid, status) = approval_row(&t).unwrap();
    assert_eq!(status, ApprovalStatus::Pending);
    // The facts an operator needs to decide, all on the question.
    match t.rows.iter().find(|r| matches!(r, Row::Approval { .. })) {
        Some(Row::Approval {
            tool,
            path,
            input_json,
            ..
        }) => {
            assert_eq!(tool, "Write");
            assert_eq!(path.as_deref(), Some("/tmp/eave-fake/canary-path.txt"));
            assert!(
                input_json.contains("canary-path.txt"),
                "the full input must be stated: {input_json}"
            );
        }
        other => panic!("expected an approval row, got {other:?}"),
    }

    // Nothing happens until a human says so: the agent is still parked.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(approval_row(&t).unwrap().1, ApprovalStatus::Pending);

    assert!(
        c.decide("s1", &rid, Decision::Allow).await.expect("decide"),
        "an open question must accept its answer"
    );
    fold_until(&mut rx, &mut t, "the tool to run", Duration::from_secs(15), |t| {
        matches!(approval_row(t), Some((_, ApprovalStatus::Approved)))
            && tool_result_text(t).iter().any(|s| s.contains("did the thing"))
    })
    .await;

    // The decision is journaled — the audit trail that makes "eave never
    // auto-approves" checkable rather than merely asserted.
    let decided = t.rows.iter().any(|r| matches!(r, Row::Approval { status: ApprovalStatus::Approved, .. }));
    assert!(decided);
    let journal = std::fs::read_to_string(dir.join("sessions/s1/journal.jsonl")).unwrap();
    assert!(
        journal.contains("\"kind\":\"approval_decided\"") && journal.contains("\"allowed\":true"),
        "the decision must be durable history"
    );
}

/// A deny carries the operator's own words to the model verbatim — the
/// capture-proven difference between "denied" and "denied, commit first".
#[tokio::test]
async fn a_denial_hands_the_operators_words_to_the_agent() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "approval-cycle");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "write the file").await.expect("send"));

    let mut t = Transcript::new();
    fold_until(&mut rx, &mut t, "the question", Duration::from_secs(15), |t| {
        approval_row(t).is_some()
    })
    .await;
    let (rid, _) = approval_row(&t).unwrap();
    c.decide(
        "s1",
        &rid,
        Decision::Deny {
            message: "commit first, then ask again".into(),
        },
    )
    .await
    .expect("decide");

    fold_until(&mut rx, &mut t, "the refusal to land", Duration::from_secs(15), |t| {
        matches!(approval_row(t), Some((_, ApprovalStatus::Denied { .. })))
            && tool_result_text(t).iter().any(|s| s.contains("stood down"))
    })
    .await;
    match approval_row(&t).unwrap().1 {
        ApprovalStatus::Denied { message } => {
            assert_eq!(message, "commit first, then ask again")
        }
        other => panic!("expected a denial, got {other:?}"),
    }
    let journal = std::fs::read_to_string(dir.join("sessions/s1/journal.jsonl")).unwrap();
    assert!(journal.contains("commit first, then ask again"));
}

/// THE Slice K clock test (directive: "the riskiest hour"). A session held
/// on a question past BOTH thresholds must never be reported as `quiet`
/// and must never become `unresponsive` — the state that offers a kill.
#[tokio::test]
async fn a_session_parked_on_a_question_is_never_reported_quiet_or_unresponsive() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "approval-hang");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "write the file").await.expect("send"));

    // Thresholds are 300 ms / 600 ms here; wait well past both, plus
    // enough CPU sample intervals to harden anything hardenable.
    let end = Instant::now() + Duration::from_secs(6);
    let mut saw_awaiting = false;
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < end {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(SubscriptionItem::Attention { attention, .. })) => {
                seen.push(attention.state_name().to_string());
                match attention {
                    AttentionSnapshot::AwaitingApproval { pending, ref tool, .. } => {
                        assert_eq!(pending, 1);
                        assert_eq!(tool, "Write");
                        saw_awaiting = true;
                    }
                    AttentionSnapshot::Quiet { .. } | AttentionSnapshot::Unresponsive { .. } => {
                        panic!(
                            "a session waiting on the operator was classified {}; \
                             attention seen: {seen:?}",
                            attention.state_name()
                        )
                    }
                    _ => {}
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("subscription ended"),
            Err(_) => {}
        }
    }
    assert!(saw_awaiting, "attention never said it was waiting on me; saw {seen:?}");

    // And the rail sees it without subscribing: state + count only.
    let infos = c.list().await.expect("list");
    let info = infos.iter().find(|i| i.session_id == "s1").unwrap();
    assert_eq!(info.state, "awaiting_approval");
    assert_eq!(info.pending_approvals, 1);
    assert_eq!(info.permission_mode.as_deref(), Some("default"));
}

/// A question the process can no longer answer must not sit on screen as
/// if it could. Killing the session settles it, and a late decision is a
/// stated race rather than a lie.
#[tokio::test]
async fn a_question_left_open_settles_when_the_process_goes() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "approval-hang");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "write the file").await.expect("send"));

    let mut t = Transcript::new();
    fold_until(&mut rx, &mut t, "the question", Duration::from_secs(15), |t| {
        approval_row(t).is_some()
    })
    .await;
    let (rid, _) = approval_row(&t).unwrap();

    c.kill("s1").await.expect("kill");
    fold_until(&mut rx, &mut t, "the question to settle", Duration::from_secs(15), |t| {
        matches!(approval_row(t), Some((_, ApprovalStatus::Withdrawn { .. })))
    })
    .await;

    // The late click: a stated fact, never an error and never journaled
    // as if it had landed.
    let err = c.decide("s1", &rid, Decision::Allow).await;
    match err {
        Ok(answered) => assert!(!answered, "a dead session cannot have been answered"),
        Err(_) => {} // "session is not live" — equally stated
    }
    let journal = std::fs::read_to_string(dir.join("sessions/s1/journal.jsonl")).unwrap();
    assert!(
        !journal.contains("approval_decided"),
        "no decision ever reached the agent, so none may be recorded"
    );
}

/// Amendment 1: a CLI announcing a posture that never asks does not get to
/// run, and the transcript says why.
#[tokio::test]
async fn a_posture_that_never_asks_refuses_to_run() {
    for mode in ["acceptEdits", "bypassPermissions", "none"] {
        let dir = scratch();
        let _engine = EngineProc::spawn_with(
            &dir,
            "hang-silent",
            &[("EAVE_FAKE_PERMISSION_MODE", mode)],
        );
        let c = Client::connect(&dir).await.expect("connect");
        c.open(Some("s1".into()), None).await.expect("open");
        let mut rx = c.subscribe("s1", None).await.expect("subscribe");
        assert!(c.send("s1", "go").await.expect("send"));

        let mut t = Transcript::new();
        fold_until(
            &mut rx,
            &mut t,
            "the stated refusal",
            Duration::from_secs(15),
            |t| {
                tool_result_text(t)
                    .iter()
                    .any(|s| s.contains("refused to run in permission mode"))
            },
        )
        .await;
        // Refused loudly AND stopped: never left alive in a posture
        // nobody reviewed.
        fold_until(&mut rx, &mut t, "the session to end", Duration::from_secs(20), |t| {
            t.closed.is_some()
        })
        .await;
        let said = tool_result_text(&t);
        assert!(
            said.iter().any(|s| s.contains(mode) || s.contains("unstated")),
            "the refusal must name the posture it refused; said {said:?}"
        );
    }
}

/// Amendment 1, the flag-rejected case: if the undocumented flag ever
/// disappears, eave says so instead of quietly running unreviewed.
#[tokio::test]
async fn a_cli_that_rejects_the_permission_flag_states_why() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "reject-permission-flag");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    let _ = c.send("s1", "go").await;

    let mut t = Transcript::new();
    fold_until(&mut rx, &mut t, "the stated refusal", Duration::from_secs(20), |t| {
        tool_result_text(t)
            .iter()
            .any(|s| s.contains("rejected --permission-prompt-tool"))
    })
    .await;
    assert!(tool_result_text(&t)
        .iter()
        .any(|s| s.contains("without review")));
}

/// Amendment 2: a dialog eave cannot present is answered with a stated
/// refusal, not silence — and the transcript records that it happened.
#[tokio::test]
async fn a_dialog_request_gets_a_stated_refusal_not_silence() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "dialog-request");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "ask me something").await.expect("send"));

    let mut t = Transcript::new();
    fold_until(
        &mut rx,
        &mut t,
        "the agent to report the refusal it got",
        Duration::from_secs(15),
        |t| {
            tool_result_text(t)
                .iter()
                .any(|s| s.contains("DIALOG_ANSWER:"))
        },
    )
    .await;
    let said = tool_result_text(&t);
    assert!(
        said.iter().any(|s| s.contains("cannot present")),
        "the model must be told plainly; got {said:?}"
    );
    assert!(
        said.iter()
            .any(|s| s.contains("eave cannot present request_user_dialog dialogs")),
        "and the operator must see it in the transcript; got {said:?}"
    );
}

/// THE isolation test for Slice K, at the raw-byte level: a watcher may
/// learn that a session is waiting on the operator and how many times, and
/// may never learn which tool or which path. Those are transcript content
/// and travel only on that session's own subscribe stream.
#[tokio::test]
async fn the_watch_channel_never_carries_a_tool_name_or_a_path() {
    const CANARY_PATH: &str = "/tmp/eave-fake/LEAK_CANARY_PATH_9c2e.txt";
    const CANARY_TOOL: &str = "LeakCanaryTool";
    let dir = scratch();
    let _engine = EngineProc::spawn_with(
        &dir,
        "approval-hang",
        &[
            ("EAVE_FAKE_APPROVAL_PATH", CANARY_PATH),
            ("EAVE_FAKE_APPROVAL_TOOL", CANARY_TOOL),
        ],
    );

    // Raw watch socket: assert on the actual bytes, not on what a typed
    // client deigned to decode.
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .expect("connect raw");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    lines.next_line().await.unwrap().expect("hello reply");
    w.write_all(b"{\"id\":2,\"method\":\"watch\"}\n").await.unwrap();

    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open");
    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    assert!(c.send("s1", "write the file").await.expect("send"));

    // Positive control: the canaries provably cross the wire on the
    // session's OWN stream, so their absence on watch is isolation and
    // not vacuity.
    let mut t = Transcript::new();
    fold_until(&mut rx, &mut t, "the question", Duration::from_secs(15), |t| {
        matches!(approval_row(t), Some((_, ApprovalStatus::Pending)))
    })
    .await;
    match t.rows.iter().find(|r| matches!(r, Row::Approval { .. })) {
        Some(Row::Approval { tool, path, .. }) => {
            assert_eq!(tool, CANARY_TOOL);
            assert_eq!(path.as_deref(), Some(CANARY_PATH));
        }
        other => panic!("no approval row: {other:?}"),
    }

    // Collect every watch byte for a while, then assert on all of it.
    let mut watch_bytes = String::new();
    let mut saw_state = false;
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        match tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                assert!(
                    line.contains("\"sessions\""),
                    "watch carried a frame that is not a sessions snapshot: {line}"
                );
                if line.contains("awaiting_approval") {
                    saw_state = true;
                    assert!(
                        line.contains("\"pending_approvals\":1"),
                        "the count is a permitted fact and must be there: {line}"
                    );
                }
                watch_bytes.push_str(&line);
                watch_bytes.push('\n');
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("watch read error: {e}"),
            Err(_) => {}
        }
    }

    assert!(
        saw_state,
        "watch never reported the parked state, so the leak assertion is vacuous: {watch_bytes}"
    );
    assert!(
        !watch_bytes.contains(CANARY_TOOL),
        "watch leaked the tool name:\n{watch_bytes}"
    );
    assert!(
        !watch_bytes.contains("LEAK_CANARY_PATH"),
        "watch leaked the path:\n{watch_bytes}"
    );
    assert!(
        !watch_bytes.contains("can_use_tool") && !watch_bytes.contains("approval_requested"),
        "watch leaked the request itself:\n{watch_bytes}"
    );
    // Sanity: JournalEvent is the type that must never appear here.
    let _ = std::mem::size_of::<JournalEvent>();
}
