//! Stage B integration tests: journal write path, crash-tail repair, cold
//! resume, instance lock (docs/spec/02-persistence.md, 04-slice.md
//! acceptance; build directive 2026-08-15).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use eave_core::events::SessionEvent;
use eave_core::journal::{
    self, classify_exit, Header, JournalEvent, JournalWriter, Outcome, FORMAT_VERSION,
};
use eave_core::proc;
use eave_core::process::{self, OrphanOutcome};
use eave_core::recorder::{record_session, send_and_journal};
use eave_core::session::{AgentSession, SpawnConfig};
use eave_core::store::{EngineLock, Meta, Store, META_FORMAT_VERSION};
use eave_core::stream::ClaudeStream;
use tokio::sync::mpsc;

mod support;
use support::spawn_agent;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eave-journal-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_header(session_id: &str) -> Header {
    Header {
        format_version: FORMAT_VERSION,
        session_id: session_id.into(),
        provider: "fake-agent".into(),
        cwd: "/tmp".into(),
        created_ts_ms: journal::now_ms(),
        agent: Default::default(),
    }
}

fn test_meta(session_id: &str) -> Meta {
    Meta {
        format_version: META_FORMAT_VERSION,
        provider: "fake-agent".into(),
        cwd: "/tmp".into(),
        session_id: session_id.into(),
        state: "running".into(),
        child: None,
        title: None,
        archived: false,
    }
}

fn provider(event: SessionEvent) -> JournalEvent {
    JournalEvent::Provider { event }
}

/// Spawn an engine-stub process against the fake agent.
fn spawn_stub(root: &Path, session_id: &str, fake_mode: &str, message: Option<&str>) -> Child {
    spawn_stub_env(root, session_id, fake_mode, message, &[])
}

fn spawn_stub_env(
    root: &Path,
    session_id: &str,
    fake_mode: &str,
    message: Option<&str>,
    extra_env: &[(&str, &str)],
) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engine-stub"));
    cmd.env("STUB_DATA_DIR", root)
        .env("STUB_SESSION_ID", session_id)
        .env("STUB_AGENT", env!("CARGO_BIN_EXE_fake-agent"))
        .env("STUB_FAKE_MODE", fake_mode);
    if let Some(m) = message {
        cmd.env("STUB_MESSAGE", m);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn engine-stub")
}

/// Run the lock probe from a separate process; returns (exit code, stdout).
fn probe_lock(root: &Path) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_engine-stub"))
        .env("STUB_TRY_LOCK", "1")
        .env("STUB_DATA_DIR", root)
        .output()
        .expect("run lock probe");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Poll a file until its contents satisfy `pred` (the journal flushes every
/// ~120 ms, so on-disk state converges quickly).
async fn wait_for_file(path: &Path, pred: impl Fn(&str) -> bool) -> String {
    let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
    loop {
        let contents = std::fs::read_to_string(path).unwrap_or_default();
        if pred(&contents) {
            return contents;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("file {path:?} never satisfied predicate; last contents:\n{contents}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Format round-trips

#[test]
fn journal_round_trips_records() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let header = test_header("s1");
    let mut w = JournalWriter::create(&path, &header).unwrap();
    let written = vec![
        w.append(provider(SessionEvent::Init {
            session_id: "s1".into(),
            permission_mode: Some("default".into()),
            slash_commands: Vec::new(),
        }))
        .unwrap(),
        w.append(JournalEvent::UserSent { text: "hi".into() })
            .unwrap(),
        w.append(JournalEvent::UserDelivered {}).unwrap(),
        w.append(provider(SessionEvent::TextDelta { text: "hel".into() }))
            .unwrap(),
        w.append(provider(SessionEvent::TurnCompleted {
            subtype: "success".into(),
            is_error: false,
        }))
        .unwrap(),
        w.append(JournalEvent::Closed {
            outcome: Outcome::Done,
        })
        .unwrap(),
    ];
    w.sync().unwrap();

    let loaded = journal::load(&path).unwrap();
    assert_eq!(loaded.header, header);
    assert_eq!(loaded.records, written);
    assert!(loaded.is_terminal());
    assert!(
        loaded.repair.already_terminal,
        "load must not have appended"
    );
    assert!(!loaded.torn_tail_repaired);
    assert!(loaded.undelivered_user_sends().is_empty());
}

#[test]
fn unknown_system_subtype_round_trips_verbatim() {
    // Acceptance (build directive): an unknown system subtype must survive
    // parse → journal write → journal read byte-for-byte, so future CLI
    // additions are recorded without a parser change.
    let line = r#"{"type":"system","subtype":"quantum_flux","payload":{"deep":[1,2,{"x":"ünïcode"}]},"session_id":"s"}"#;
    let rate_line = r#"{"type":"rate_limit_event","window":"5h","utilization":0.42}"#;

    let mut stream = ClaudeStream::new();
    let events = stream.parse_line(line);
    assert_eq!(
        events,
        vec![SessionEvent::System {
            subtype: "quantum_flux".into(),
            raw: line.into(),
        }]
    );
    let rate_events = stream.parse_line(rate_line);
    assert_eq!(
        rate_events,
        vec![SessionEvent::Unknown {
            event_type: "rate_limit_event".into(),
            raw: rate_line.into(),
        }]
    );

    let root = scratch();
    let path = root.join("journal.jsonl");
    let mut w = JournalWriter::create(&path, &test_header("s1")).unwrap();
    w.append(provider(events[0].clone())).unwrap();
    w.append(provider(rate_events[0].clone())).unwrap();
    w.sync().unwrap();

    let loaded = journal::load(&path).unwrap();
    match &loaded.records[0].event {
        JournalEvent::Provider {
            event: SessionEvent::System { subtype, raw },
        } => {
            assert_eq!(subtype, "quantum_flux");
            assert_eq!(raw, line, "raw system line must round-trip unchanged");
        }
        other => panic!("expected System record, got {other:?}"),
    }
    match &loaded.records[1].event {
        JournalEvent::Provider {
            event: SessionEvent::Unknown { raw, .. },
        } => assert_eq!(raw, rate_line),
        other => panic!("expected Unknown record, got {other:?}"),
    }
}

#[test]
fn exit_classification_treats_143_as_killed() {
    // The real CLI catches SIGTERM and exits 143 (claude-stream-notes.md).
    assert_eq!(
        classify_exit(Some(143), None, ""),
        Outcome::Killed {
            code: Some(143),
            signal: None
        }
    );
    // Raw signal deaths from our own ladder are kills, not crashes.
    assert!(matches!(
        classify_exit(None, Some(libc::SIGTERM), ""),
        Outcome::Killed { .. }
    ));
    assert!(matches!(
        classify_exit(None, Some(libc::SIGKILL), ""),
        Outcome::Killed { .. }
    ));
    assert_eq!(classify_exit(Some(0), None, ""), Outcome::Done);
    assert_eq!(
        classify_exit(Some(3), None, "disk on fire"),
        Outcome::Crashed {
            code: Some(3),
            signal: None,
            stderr_tail: "disk on fire".into()
        }
    );
}

// ---------------------------------------------------------------------------
// Corruption behavior (spec 02 §Corruption)

#[test]
fn torn_tail_truncates_to_last_valid_record_at_every_byte() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let mut w = JournalWriter::create(&path, &test_header("s1")).unwrap();
    for i in 0..5 {
        w.append(provider(SessionEvent::TextDelta {
            text: format!("chunk-{i}-payload"),
        }))
        .unwrap();
    }
    w.sync().unwrap();
    let original = std::fs::read(&path).unwrap();
    let last_line_len = original
        .split(|b| *b == b'\n')
        .rfind(|s| !s.is_empty())
        .unwrap()
        .len();
    let last_start = original.len() - (last_line_len + 1);

    for cut in last_start..original.len() {
        let case = root.join(format!("torn-{cut}.jsonl"));
        std::fs::write(&case, &original[..cut]).unwrap();
        let loaded =
            journal::load(&case).unwrap_or_else(|e| panic!("cut at byte {cut} must load, got {e}"));
        // A partial final record must never surface; the only complete-parse
        // cut is at original.len()-1 (the record intact minus its newline).
        // Load also applies crash-tail repair: the streaming turn settles
        // as interrupted (+TurnInterrupted, +Closed).
        let surviving = if cut == original.len() - 1 { 5 } else { 4 };
        assert_eq!(loaded.records.len(), surviving + 2, "cut at byte {cut}");
        for rec in &loaded.records[..surviving] {
            assert!(matches!(
                &rec.event,
                JournalEvent::Provider {
                    event: SessionEvent::TextDelta { .. }
                }
            ));
        }
        assert!(matches!(
            &loaded.records[surviving].event,
            JournalEvent::TurnInterrupted { .. }
        ));
        assert!(matches!(
            &loaded.records[surviving + 1].event,
            JournalEvent::Closed {
                outcome: Outcome::Interrupted
            }
        ));
        // Loading repaired the file: a reload is clean and identical.
        let reloaded = journal::load(&case).unwrap();
        assert!(!reloaded.torn_tail_repaired);
        assert!(reloaded.repair.already_terminal);
        assert_eq!(reloaded.records, loaded.records);
    }
}

#[test]
fn midfile_corruption_refuses_and_quarantines() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let mut w = JournalWriter::create(&path, &test_header("s1")).unwrap();
    for i in 0..4 {
        w.append(provider(SessionEvent::TextDelta {
            text: format!("t{i}"),
        }))
        .unwrap();
    }
    w.sync().unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = contents.lines().collect();
    lines[2] = "!!not json!!";
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

    match journal::load(&path) {
        Err(journal::LoadError::Corrupt {
            line_no,
            renamed_to,
            ..
        }) => {
            assert_eq!(line_no, 3);
            assert!(renamed_to.exists(), "quarantine file must exist");
            assert!(!path.exists(), "original must have been renamed away");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn seq_break_is_corruption() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let header = serde_json::to_string(&test_header("s1")).unwrap();
    let rec = |seq: u64| {
        format!(r#"{{"seq":{seq},"ts_ms":1,"event":{{"kind":"user_sent","text":"x"}}}}"#)
    };
    std::fs::write(&path, format!("{header}\n{}\n{}\n", rec(1), rec(3))).unwrap();
    assert!(matches!(
        journal::load(&path),
        Err(journal::LoadError::Corrupt { line_no: 3, .. })
    ));
}

#[test]
fn unknown_format_version_is_refused_loudly_without_rename() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let mut header = serde_json::to_value(test_header("s1")).unwrap();
    header["format_version"] = 99.into();
    std::fs::write(&path, format!("{header}\n")).unwrap();
    assert!(matches!(
        journal::load(&path),
        Err(journal::LoadError::UnknownFormatVersion { found: 99 })
    ));
    assert!(path.exists(), "no best-effort rename for future formats");
}

// ---------------------------------------------------------------------------
// Crash-tail repair (spec 02 §Resume contract step 1)

#[test]
fn crash_tail_repair_settles_open_turn_as_interrupted() {
    let root = scratch();
    let path = root.join("journal.jsonl");
    let mut w = JournalWriter::create(&path, &test_header("s1")).unwrap();
    w.append(provider(SessionEvent::Init {
        session_id: "s1".into(),
        permission_mode: Some("default".into()),
        slash_commands: Vec::new(),
    }))
    .unwrap();
    w.append(JournalEvent::UserSent {
        text: "do the thing".into(),
    })
    .unwrap();
    w.append(provider(SessionEvent::ToolUseStarted {
        id: "toolu_x".into(),
        name: "Bash".into(),
        input: String::new(),
        call: None,
    }))
    .unwrap();
    w.sync().unwrap();

    // Load is the single door and repairs on the way through.
    let loaded = journal::load(&path).unwrap();
    assert!(loaded.is_terminal());
    assert!(!loaded.repair.already_terminal);
    assert_eq!(loaded.repair.tools_interrupted, vec!["toolu_x".to_string()]);
    assert!(loaded.repair.turn_interrupted);

    let tail: Vec<&JournalEvent> = loaded.records.iter().map(|r| &r.event).collect();
    assert!(matches!(
        tail[tail.len() - 3],
        JournalEvent::ToolInterrupted { id } if id == "toolu_x"
    ));
    assert!(matches!(
        tail[tail.len() - 2],
        JournalEvent::TurnInterrupted { .. }
    ));
    assert!(matches!(
        tail[tail.len() - 1],
        JournalEvent::Closed {
            outcome: Outcome::Interrupted
        }
    ));

    // The send was journaled but never confirmed: undelivered backfill.
    let sent_seq = loaded
        .records
        .iter()
        .find_map(|r| match &r.event {
            JournalEvent::UserSent { .. } => Some(r.seq),
            _ => None,
        })
        .unwrap();
    assert_eq!(loaded.undelivered_user_sends(), vec![sent_seq]);

    // Repair is idempotent across loads.
    let again = journal::load(&path).unwrap();
    assert!(again.repair.already_terminal);
    assert_eq!(again.records, loaded.records);
}

// ---------------------------------------------------------------------------
// Live recording (recorder + real spawn path)

#[tokio::test]
async fn exit_143_is_recorded_as_killed_not_crashed() {
    let root = scratch();
    let store = Store::open(&root).unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let dir = store.create_session_dir(&sid).unwrap();
    let jpath = Store::journal_path(&dir);
    let writer = JournalWriter::create(&jpath, &test_header(&sid)).unwrap();

    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        sid.clone(),
    );
    cfg.extra_env.push(("FAKE_MODE".into(), "term-143".into()));
    let (mut session, events) = spawn_agent(&cfg).unwrap();

    let (extra_tx, extra_rx) = mpsc::channel(1);
    drop(extra_tx);
    let rec = tokio::spawn(record_session(
        events,
        extra_rx,
        writer,
        dir.clone(),
        test_meta(&sid),
        None,
    ));

    wait_for_file(&jpath, |c| c.contains("alive and catching TERM")).await;
    session.kill().await.expect("kill must be provable");

    let outcome = tokio::time::timeout(TEST_TIMEOUT, rec)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        Outcome::Killed {
            code: Some(143),
            signal: None
        },
        "exit 143 must classify as killed, not crashed"
    );

    let loaded = journal::load(&jpath).unwrap();
    assert!(loaded.repair.already_terminal);
    assert!(matches!(
        &loaded.records.last().unwrap().event,
        JournalEvent::Closed {
            outcome: Outcome::Killed {
                code: Some(143),
                ..
            }
        }
    ));
    let meta = Meta::load(&dir).unwrap();
    assert_eq!(meta.state, "killed");
    assert!(meta.child.is_none());
}

#[tokio::test]
async fn journaled_but_undelivered_send_is_distinct_from_sent() {
    // The agent closes its stdin (alive, but the pipe is dead): the journal
    // write succeeds, the pipe write fails. On reload the message must be
    // in the undelivered state, never rendered as sent (spec 02 §Delivery
    // state).
    let root = scratch();
    let store = Store::open(&root).unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let dir = store.create_session_dir(&sid).unwrap();
    let jpath = Store::journal_path(&dir);
    let writer = JournalWriter::create(&jpath, &test_header(&sid)).unwrap();

    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        sid.clone(),
    );
    cfg.extra_env
        .push(("FAKE_MODE".into(), "stdin-closed".into()));
    let (mut session, events) = spawn_agent(&cfg).unwrap();
    let (extra_tx, extra_rx) = mpsc::channel(8);
    let rec = tokio::spawn(record_session(
        events,
        extra_rx,
        writer,
        dir.clone(),
        test_meta(&sid),
        None,
    ));
    wait_for_file(&jpath, |c| c.contains("init")).await;

    let refusal = send_and_journal(&mut *session, &extra_tx, "into the void")
        .await
        .unwrap();
    // The reason is now kept, so this asserts the failure AND that the
    // engine can say what it was — an undelivered send with no explanation
    // is what made this class of failure unreadable everywhere else.
    let why = refusal.expect("pipe write to a closed stdin must fail");
    assert!(!why.is_empty(), "a refused write must state a reason");
    // The live half of the distinction `a_send_into_a_dead_agent_says_so`
    // makes: this agent IS alive and IS refusing, so the reason must not
    // blame a dead process. Without this control that test could pass
    // against code that called every failed write a dead session.
    assert!(
        !why.contains("already gone"),
        "the agent is alive and refusing; the reason must not call it gone: {why}"
    );
    drop(extra_tx);
    session.kill().await.expect("kill must be provable");
    rec.await.unwrap().unwrap();

    let loaded = journal::load(&jpath).unwrap();
    let sent_seq = loaded
        .records
        .iter()
        .find_map(|r| match &r.event {
            JournalEvent::UserSent { text } if text == "into the void" => Some(r.seq),
            _ => None,
        })
        .expect("the attempt itself must be journaled");
    assert_eq!(
        loaded.undelivered_user_sends(),
        vec![sent_seq],
        "unconfirmed send in a terminal journal is undelivered"
    );

    // Control: the same flow over a live pipe confirms delivery.
    let sid2 = uuid::Uuid::new_v4().to_string();
    let dir2 = store.create_session_dir(&sid2).unwrap();
    let jpath2 = Store::journal_path(&dir2);
    let writer2 = JournalWriter::create(&jpath2, &test_header(&sid2)).unwrap();
    let mut cfg2 = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        sid2.clone(),
    );
    cfg2.extra_env
        .push(("FAKE_MODE".into(), "tool-cycle".into()));
    let (mut session2, events2) = spawn_agent(&cfg2).unwrap();
    let (extra_tx2, extra_rx2) = mpsc::channel(8);
    let rec2 = tokio::spawn(record_session(
        events2,
        extra_rx2,
        writer2,
        dir2.clone(),
        test_meta(&sid2),
        None,
    ));
    assert!(
        send_and_journal(&mut *session2, &extra_tx2, "hello there")
            .await
            .unwrap()
            .is_none(),
        "the resumed session must accept a send"
    );
    drop(extra_tx2);
    tokio::time::timeout(TEST_TIMEOUT, rec2)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(journal::load(&jpath2)
        .unwrap()
        .undelivered_user_sends()
        .is_empty());
}

/// `delivered = false` covers two different worlds and used to say the
/// same six words about both.
///
/// A live agent refusing a write is a fact about the agent — the bool
/// exists for that (spec 02 §Delivery state). An agent that was already
/// dead when we wrote is not refusing anything; it is a session that
/// never started or has since exited, and "the agent did not accept your
/// message" points the reader at the wrong process entirely. Both
/// produced `io error: Broken pipe (os error 32)` and nothing else.
///
/// This is the dead half. `journaled_but_undelivered_send_is_distinct_from_sent`
/// is the live half, and it asserts the reason does NOT say "gone" —
/// together they are what makes either assertion mean anything.
#[tokio::test]
async fn a_send_into_a_dead_agent_says_so() {
    let sid = uuid::Uuid::new_v4().to_string();
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        sid.clone(),
    );
    cfg.extra_env.push(("FAKE_MODE".into(), "crash".into()));
    let (mut session, mut events) = spawn_agent(&cfg).unwrap();

    // Wait for the exit rather than sleeping toward it: `Exited` is always
    // the final event, so past it the process is provably gone and the
    // write below cannot race it.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while let Some(ev) = events.recv().await {
            if matches!(ev, SessionEvent::Exited { .. }) {
                return;
            }
        }
        panic!("the agent never reported an exit");
    })
    .await
    .expect("agent did not exit in time");
    assert!(
        !proc::identity_matches(&session.identity()),
        "the specimen must really be gone before the write"
    );

    // The receiver is held, not dropped: a closed recorder is a different
    // failure (`Err`), and this test is about the `Ok(Some(reason))` path.
    let (extra_tx, _extra_rx) = mpsc::channel(8);
    let why = send_and_journal(&mut *session, &extra_tx, "into a dead agent")
        .await
        .expect("a dead agent is not a recorder failure")
        .expect("a write into a dead agent must not report delivery");

    assert!(
        why.contains("already gone"),
        "the reason must name the dead process, not just the errno: {why}"
    );
    assert!(
        why.contains(&session.identity().pid.to_string()),
        "the reason must name WHICH process: {why}"
    );
}

// ---------------------------------------------------------------------------
// kill -9 the engine (spec 04 acceptance)

#[tokio::test]
async fn kill9_engine_mid_turn_repairs_and_resumes_same_session() {
    let root = scratch();
    let sid = uuid::Uuid::new_v4().to_string();
    let mut stub = spawn_stub(&root, &sid, "tool-hang", Some("please hang in a tool"));
    let dir = root.join("sessions").join(&sid);
    let jpath = dir.join("journal.jsonl");

    // Wait until the outstanding tool AND the send records are on disk —
    // provably mid-turn, with nothing left in the write-behind buffer
    // (the agent hangs, so no further records will ever arrive).
    wait_for_file(&jpath, |c| {
        c.contains("tool_use_started") && c.contains("user_sent") && c.contains("user_delivered")
    })
    .await;
    unsafe {
        libc::kill(stub.id() as i32, libc::SIGKILL);
    }
    stub.wait().unwrap();

    // The agent survived the engine's death; the meta breadcrumb finds it,
    // v0 policy kills it and verifies (spec 02 §Resume contract step 2).
    let meta = Meta::load(&dir).unwrap();
    let child = meta.child.expect("meta must carry the orphan breadcrumb");
    assert!(
        proc::identity_matches(&child),
        "fake-agent should have outlived the engine"
    );
    let reaped = process::reap_orphan(&child).await;
    assert!(
        matches!(reaped, OrphanOutcome::Reaped { .. }),
        "orphan reap must be proven, got {reaped:?}"
    );
    assert!(!proc::identity_matches(&child));

    // Transcript intact; the load itself settles the turn as interrupted.
    let loaded = journal::load(&jpath).unwrap();
    assert_eq!(loaded.header.session_id, sid);
    assert!(loaded.records.iter().any(|r| matches!(
        &r.event,
        JournalEvent::UserSent { text } if text == "please hang in a tool"
    )));
    assert!(!loaded.repair.already_terminal);
    assert_eq!(
        loaded.repair.tools_interrupted,
        vec!["toolu_hang".to_string()]
    );
    assert!(loaded.repair.turn_interrupted);
    assert!(loaded.is_terminal());
    // The message reached the agent's stdin before the engine died.
    assert!(loaded.undelivered_user_sends().is_empty());

    // Continuation: next message goes to a fresh process continuing the
    // same provider session via --resume <session_id>.
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        loaded.header.session_id.clone(),
    );
    cfg.resume = true;
    cfg.extra_env
        .push(("FAKE_MODE".into(), "resume-probe".into()));
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    // resume-probe answers the continuation message, like the real CLI —
    // cold resume is always user-initiated (spec 02: no auto-redispatch).
    session
        .send_user_message("continue")
        .await
        .expect("send continuation message");
    let probe = tokio::time::timeout(TEST_TIMEOUT, async {
        while let Some(ev) = rx.recv().await {
            if let SessionEvent::AssistantText { text } = ev {
                return text;
            }
        }
        panic!("resume-probe emitted no assistant text");
    })
    .await
    .unwrap();
    assert!(
        probe.contains("RESUME_FLAG:true"),
        "continuation must use --resume: {probe}"
    );
    assert!(
        probe.contains(&format!("ID:{sid}")),
        "continuation must carry the same session id: {probe}"
    );
}

#[tokio::test]
async fn kill9_during_journal_writes_loads_to_last_valid_event() {
    let root = scratch();
    let sid = uuid::Uuid::new_v4().to_string();
    let mut stub = spawn_stub(&root, &sid, "flood", None);
    let dir = root.join("sessions").join(&sid);
    let jpath = dir.join("journal.jsonl");

    // Kill while the journal is growing fast — writes are in flight.
    wait_for_file(&jpath, |c| c.len() > 16_384).await;
    unsafe {
        libc::kill(stub.id() as i32, libc::SIGKILL);
    }
    stub.wait().unwrap();

    // The flood agent dies on EPIPE once the stub is gone, or gets reaped
    // here; either way exit must be proven before we call this clean.
    if let Some(child) = Meta::load(&dir).unwrap().child {
        let outcome = process::reap_orphan(&child).await;
        assert!(!matches!(outcome, OrphanOutcome::Unproven), "{outcome:?}");
    }

    // Whatever instant the kill landed at, the journal loads to the last
    // valid event and no partial record surfaces; the load settles it.
    let loaded = journal::load(&jpath).unwrap();
    assert!(
        !loaded.records.is_empty(),
        "flood ran long enough to journal events"
    );
    assert!(!loaded.repair.already_terminal);
    assert!(loaded.is_terminal());
    let reloaded = journal::load(&jpath).unwrap();
    assert!(reloaded.repair.already_terminal);
    assert!(!reloaded.torn_tail_repaired);
}

// ---------------------------------------------------------------------------
// Two instances, same repo (spec 04 acceptance)

#[test]
fn engine_lock_is_exclusive_per_data_dir() {
    // The contract is cross-*process* exclusion — a second app instance's
    // engine must be refused (spec 02 §Layout). Same-process double-open
    // flock semantics are quirky on macOS and not part of the contract, so
    // the probe runs as a separate process (engine-stub STUB_TRY_LOCK).
    let root = scratch();
    let held = EngineLock::acquire(&root).expect("first acquire");
    let (code, stdout) = probe_lock(&root);
    assert_eq!(code, Some(42), "a second instance must see Busy while held");
    // Busy names the holder — this test process wrote its identity.
    assert!(
        stdout.contains(&format!("holder_pid={}", std::process::id())),
        "loser must learn the holder pid: {stdout}"
    );
    drop(held);
    let (code, _) = probe_lock(&root);
    assert_eq!(code, Some(0), "the lock must be free once released");
}

#[tokio::test]
async fn contended_start_fails_fast_and_leaves_the_winner_undisturbed() {
    // A real engine process holds the lock mid-turn; a second engine's
    // start must fail fast, name the holder, and be provably inert
    // (spec 02 §Layout, contended-start contract).
    let root = scratch();
    let sid = uuid::Uuid::new_v4().to_string();
    let mut winner = spawn_stub_env(
        &root,
        &sid,
        "tool-hang",
        Some("hold the fort"),
        &[("STUB_TAKE_LOCK", "1")],
    );
    let dir = root.join("sessions").join(&sid);
    let jpath = dir.join("journal.jsonl");
    // All records the hanging agent will ever produce, fully flushed —
    // so any later change to the file would be the loser's doing.
    wait_for_file(&jpath, |c| {
        c.contains("tool_use_started") && c.contains("user_sent") && c.contains("user_delivered")
    })
    .await;

    let journal_before = std::fs::read(&jpath).unwrap();
    let meta_before = std::fs::read(dir.join("meta.json")).unwrap();

    let (code, stdout) = probe_lock(&root);
    assert_eq!(code, Some(42), "loser must fail fast, not block or retry");
    assert!(
        stdout.contains(&format!("holder_pid={}", winner.id())),
        "loser must name the winning engine: {stdout}"
    );

    // Winner undisturbed: still alive, lock still held, session state
    // byte-identical.
    assert_eq!(unsafe { libc::kill(winner.id() as i32, 0) }, 0);
    assert_eq!(probe_lock(&root).0, Some(42), "lock must still be held");
    assert_eq!(std::fs::read(&jpath).unwrap(), journal_before);
    assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), meta_before);

    // Cleanup: kill the winner, reap its orphaned agent.
    unsafe {
        libc::kill(winner.id() as i32, libc::SIGKILL);
    }
    winner.wait().unwrap();
    if let Some(child) = Meta::load(&dir).unwrap().child {
        let outcome = process::reap_orphan(&child).await;
        assert!(!matches!(outcome, OrphanOutcome::Unproven), "{outcome:?}");
    }
}

#[test]
fn stale_lock_content_from_a_dead_holder_never_blocks() {
    // flock cannot go stale — the kernel releases it on holder death,
    // SIGKILL included. What a dead holder leaves behind is only advisory
    // *content*, which must neither block nor confuse the next winner
    // (spec 02 §Layout).
    let root = scratch();
    std::fs::write(
        root.join("engine.lock"),
        r#"{"pid":999999,"start_tvsec":1,"start_tvusec":1}"#,
    )
    .unwrap();
    let held = EngineLock::acquire(&root).expect("dead holder's content must not block");
    // The winner rewrote the holder record with its own identity.
    let content = std::fs::read(root.join("engine.lock")).unwrap();
    let holder: Option<proc::ProcIdentity> = serde_json::from_slice(&content).unwrap();
    assert_eq!(holder.unwrap().pid, std::process::id() as i32);
    drop(held);

    // Garbage content is equally harmless.
    std::fs::write(root.join("engine.lock"), "!!garbage!!").unwrap();
    EngineLock::acquire(&root).expect("garbage content must not block");
}

#[test]
fn concurrent_sessions_in_one_store_do_not_cross_contaminate() {
    // N=6 since Slice I1 (was 2): the multi-session build leans on
    // concurrent journal writes staying clean well past a pair.
    const N: usize = 6;
    let root = scratch();
    let sids: Vec<String> = (0..N).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let messages: Vec<String> = (0..N)
        .map(|i| format!("message for session #{i}"))
        .collect();
    let mut children: Vec<_> = sids
        .iter()
        .zip(&messages)
        .map(|(sid, msg)| spawn_stub(&root, sid, "tool-cycle", Some(msg)))
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }

    for (i, sid) in sids.iter().enumerate() {
        let dir = root.join("sessions").join(sid);
        let loaded = journal::load(&dir.join("journal.jsonl")).unwrap();
        assert_eq!(&loaded.header.session_id, sid);
        assert!(loaded.is_terminal());
        assert!(matches!(
            &loaded.records.last().unwrap().event,
            JournalEvent::Closed {
                outcome: Outcome::Done
            }
        ));
        let text = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert!(text.contains(&messages[i]));
        for (j, other) in messages.iter().enumerate() {
            assert!(
                j == i || !text.contains(other),
                "session {sid} journal contains session #{j}'s message"
            );
        }
        assert!(loaded.undelivered_user_sends().is_empty(), "delivered send");
        let meta = Meta::load(&dir).unwrap();
        assert_eq!(&meta.session_id, sid);
        assert_eq!(meta.state, "done");
        assert!(meta.child.is_none(), "no stale pid breadcrumb");
    }
}

// ---------------------------------------------------------------------------
// Real CLI (ignored by default; requires `claude` + credentials):
// `cargo test --test journal -- --ignored real_claude`

#[tokio::test]
#[ignore]
async fn real_claude_journal_and_cold_resume() {
    const CODEWORD: &str = "MOSSYFJORD77";
    let root = scratch();
    let store = Store::open(&root).unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let dir = store.create_session_dir(&sid).unwrap();
    let jpath = Store::journal_path(&dir);
    let writer = JournalWriter::create(&jpath, &test_header(&sid)).unwrap();

    let cfg = SpawnConfig::new("claude", std::env::temp_dir(), sid.clone());
    let (mut session, events) = spawn_agent(&cfg).unwrap();
    let (extra_tx, extra_rx) = mpsc::channel(4);
    let rec = tokio::spawn(record_session(
        events,
        extra_rx,
        writer,
        dir.clone(),
        test_meta(&sid),
        None,
    ));

    let msg = format!("Remember the codeword: {CODEWORD}. Reply with exactly: noted");
    extra_tx
        .send(JournalEvent::UserSent { text: msg.clone() })
        .await
        .unwrap();
    session.send_user_message(&msg).await.unwrap();
    drop(extra_tx);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let contents = std::fs::read_to_string(&jpath).unwrap_or_default();
        if contents.contains("turn_completed") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no turn completion");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    session.kill().await.unwrap();
    let outcome = rec.await.unwrap().unwrap();
    assert!(matches!(
        outcome,
        Outcome::Killed {
            code: Some(143),
            ..
        }
    ));
    assert!(journal::load(&jpath).unwrap().is_terminal());

    // Cold resume from the journal's cursor: fresh process, same session.
    let loaded = journal::load(&jpath).unwrap();
    let mut cfg2 = SpawnConfig::new("claude", std::env::temp_dir(), loaded.header.session_id);
    cfg2.resume = true;
    let (mut session2, mut rx2) = spawn_agent(&cfg2).unwrap();
    session2
        .send_user_message("What is the codeword? Reply with the codeword only.")
        .await
        .unwrap();
    let recalled = tokio::time::timeout(Duration::from_secs(120), async {
        let mut recalled = false;
        while let Some(ev) = rx2.recv().await {
            match &ev {
                SessionEvent::AssistantText { text } | SessionEvent::TextDelta { text } => {
                    if text.contains(CODEWORD) {
                        recalled = true;
                    }
                }
                SessionEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        recalled
    })
    .await
    .unwrap();
    session2.kill().await.unwrap();
    assert!(recalled, "resumed session failed to recall the codeword");
}

// ---------------------------------------------------------------------------
// Slice P2 — the persistence half of per-session agent config

/// A journal written before per-session config existed must still load,
/// and must read as "the engine's defaults" — which is exactly what those
/// sessions ran under.
///
/// This is why `FORMAT_VERSION` did NOT move for the new header field:
/// `load` refuses any version but its own, so a bump would have made every
/// existing journal on every dogfooding machine unreadable. The field is
/// `#[serde(default)]` instead, and this test is what pins that choice.
#[test]
fn a_header_written_before_agent_config_still_loads() {
    let dir = scratch();
    let path = dir.join("journal.jsonl");
    // Byte-for-byte the header shape a pre-P2 build wrote.
    let old = r#"{"format_version":1,"session_id":"s1","provider":"claude-code","cwd":"/tmp","created_ts_ms":7}"#;
    std::fs::write(&path, format!("{old}\n")).unwrap();

    let loaded = eave_core::journal::load(&path).expect("an old journal must still load");
    assert_eq!(loaded.header.session_id, "s1");
    assert_eq!(loaded.header.provider, "claude-code");
    assert_eq!(
        loaded.header.agent,
        eave_core::session::AgentConfig::default(),
        "an absent config must read as the engine's defaults"
    );
}

/// And a session that configured nothing must write a header that an OLDER
/// build could still read — no new key at all, thanks to
/// `skip_serializing_if`.
#[test]
fn an_unconfigured_header_writes_no_agent_key() {
    let header = Header {
        format_version: FORMAT_VERSION,
        session_id: "s1".into(),
        provider: "claude-code".into(),
        cwd: "/tmp".into(),
        created_ts_ms: 7,
        agent: Default::default(),
    };
    let raw = serde_json::to_string(&header).expect("serialize");
    assert!(
        !raw.contains("agent"),
        "an unconfigured session must write the old header shape: {raw}"
    );
}

/// A configured session's header round-trips through the file, which is
/// what makes the config survive a restart at all.
#[test]
fn a_configured_header_round_trips_through_the_journal() {
    let dir = scratch();
    let path = dir.join("journal.jsonl");
    let agent = eave_core::session::AgentConfig {
        provider: Some("claude-code".into()),
        program: None,
        model: Some("opus-4-2".into()),
        extra_args: vec!["--foo".into()],
    };
    let header = Header {
        format_version: FORMAT_VERSION,
        session_id: "s1".into(),
        provider: "claude-code".into(),
        cwd: "/tmp".into(),
        created_ts_ms: 7,
        agent: agent.clone(),
    };
    JournalWriter::create(&path, &header).expect("create");
    let loaded = eave_core::journal::load(&path).expect("load");
    assert_eq!(loaded.header.agent, agent);
}

/// **Slice AR2.** The CLI's own command inventory survives the journal,
/// which is the whole reason it needs no channel of its own: a session that
/// has ever run replays its inventory on every later open, with nothing
/// live and nothing spawned.
///
/// The compatibility half is the point of `#[serde(default)]`: every
/// journal written before this field existed carries no `slash_commands`
/// key, and those records must still load. They do, as an empty list —
/// which is exactly what those runs knew.
#[test]
fn an_init_carries_its_command_inventory_through_the_journal_and_an_old_one_still_loads() {
    let dir = scratch();
    let path = dir.join("journal.jsonl");
    let mut w = JournalWriter::create(&path, &test_header("s1")).unwrap();
    w.append(provider(SessionEvent::Init {
        session_id: "s1".into(),
        permission_mode: Some("default".into()),
        slash_commands: vec!["context".into(), "craft-skills:seams".into()],
    }))
    .unwrap();
    w.append(JournalEvent::Closed {
        outcome: Outcome::Done,
    })
    .unwrap();
    w.sync().unwrap();

    let loaded = eave_core::journal::load(&path).expect("load");
    match &loaded.records[0].event {
        JournalEvent::Provider {
            event: SessionEvent::Init { slash_commands, .. },
        } => assert_eq!(slash_commands, &["context", "craft-skills:seams"]),
        other => panic!("got {other:?}"),
    }

    // ...and the shape a pre-AR2 build wrote is what a session that
    // announced nothing writes TODAY: `skip_serializing_if` keeps the key
    // off the line entirely, so an older build reading a newer journal
    // sees exactly the bytes it has always seen.
    let quiet = serde_json::to_string(&eave_core::journal::Record {
        seq: 1,
        ts_ms: 1,
        event: provider(SessionEvent::Init {
            session_id: "s1".into(),
            permission_mode: Some("default".into()),
            slash_commands: Vec::new(),
        }),
    })
    .unwrap();
    assert!(
        !quiet.contains("slash_commands"),
        "an init that announced nothing must write the old record shape: {quiet}"
    );

    // The other direction: that line, loaded by THIS build.
    let old_path = dir.join("old.jsonl");
    std::fs::write(
        &old_path,
        format!(
            "{}\n{quiet}\n{}\n",
            serde_json::to_string(&test_header("s1")).unwrap(),
            serde_json::to_string(&eave_core::journal::Record {
                seq: 2,
                ts_ms: 2,
                event: JournalEvent::Closed {
                    outcome: Outcome::Done
                },
            })
            .unwrap(),
        ),
    )
    .unwrap();
    let old = eave_core::journal::load(&old_path).expect("an old journal must still load");
    match &old.records[0].event {
        JournalEvent::Provider {
            event:
                SessionEvent::Init {
                    slash_commands,
                    permission_mode,
                    ..
                },
        } => {
            assert!(
                slash_commands.is_empty(),
                "a run that announced nothing must read as nothing, never as a guess"
            );
            assert_eq!(permission_mode.as_deref(), Some("default"));
        }
        other => panic!("got {other:?}"),
    }
}
