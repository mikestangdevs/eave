//! Stage B integration tests: journal write path, crash-tail repair, cold
//! resume, instance lock (docs/spec/02-persistence.md, 04-slice.md
//! acceptance; build directive 2026-08-15).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use eave_core::claude::ClaudeCodeSession;
use eave_core::events::SessionEvent;
use eave_core::journal::{
    self, classify_exit, Header, JournalEvent, JournalWriter, Outcome, FORMAT_VERSION,
};
use eave_core::proc;
use eave_core::process::{self, OrphanOutcome};
use eave_core::recorder::record_session;
use eave_core::session::{AgentSession, SpawnConfig};
use eave_core::store::{EngineLock, Meta, Store, META_FORMAT_VERSION};
use eave_core::stream::ClaudeStream;
use tokio::sync::mpsc;

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
    }
}

fn provider(event: SessionEvent) -> JournalEvent {
    JournalEvent::Provider { event }
}

/// Spawn an engine-stub process against the fake agent.
fn spawn_stub(root: &Path, session_id: &str, fake_mode: &str, message: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engine-stub"));
    cmd.env("STUB_DATA_DIR", root)
        .env("STUB_SESSION_ID", session_id)
        .env("STUB_AGENT", env!("CARGO_BIN_EXE_fake-agent"))
        .env("STUB_FAKE_MODE", fake_mode);
    if let Some(m) = message {
        cmd.env("STUB_MESSAGE", m);
    }
    cmd.spawn().expect("spawn engine-stub")
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
        }))
        .unwrap(),
        w.append(JournalEvent::UserSent { text: "hi".into() }).unwrap(),
        w.append(provider(SessionEvent::TextDelta {
            text: "hel".into(),
        }))
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
    assert!(!loaded.torn_tail_repaired);
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
        .filter(|s| !s.is_empty())
        .last()
        .unwrap()
        .len();
    let last_start = original.len() - (last_line_len + 1);

    for cut in last_start..original.len() {
        let case = root.join(format!("torn-{cut}.jsonl"));
        std::fs::write(&case, &original[..cut]).unwrap();
        let loaded = journal::load(&case)
            .unwrap_or_else(|e| panic!("cut at byte {cut} must load, got {e}"));
        // A partial final record must never surface; the only complete-parse
        // cut is at original.len()-1 (the record intact minus its newline).
        let expect = if cut == original.len() - 1 { 5 } else { 4 };
        assert_eq!(loaded.records.len(), expect, "cut at byte {cut}");
        // Loading repaired the file: a reload is clean and identical.
        let reloaded = journal::load(&case).unwrap();
        assert!(!reloaded.torn_tail_repaired);
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
    }))
    .unwrap();
    w.append(JournalEvent::UserSent {
        text: "do the thing".into(),
    })
    .unwrap();
    w.append(provider(SessionEvent::ToolUseStarted {
        id: "toolu_x".into(),
        name: "Bash".into(),
    }))
    .unwrap();
    w.sync().unwrap();

    let mut loaded = journal::load(&path).unwrap();
    assert!(!loaded.is_terminal());
    let repair = journal::repair(&path, &mut loaded).unwrap();
    assert!(!repair.already_terminal);
    assert_eq!(repair.tools_interrupted, vec!["toolu_x".to_string()]);
    assert!(repair.turn_interrupted);

    let reloaded = journal::load(&path).unwrap();
    assert!(reloaded.is_terminal());
    let tail: Vec<&JournalEvent> = reloaded.records.iter().map(|r| &r.event).collect();
    assert!(matches!(
        tail[tail.len() - 3],
        JournalEvent::ToolInterrupted { id } if id == "toolu_x"
    ));
    assert!(matches!(tail[tail.len() - 2], JournalEvent::TurnInterrupted { .. }));
    assert!(matches!(
        tail[tail.len() - 1],
        JournalEvent::Closed { outcome: Outcome::Interrupted }
    ));

    // Repair is idempotent.
    let mut again = journal::load(&path).unwrap();
    assert!(journal::repair(&path, &mut again).unwrap().already_terminal);
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

    let mut cfg = SpawnConfig::new(env!("CARGO_BIN_EXE_fake-agent"), std::env::temp_dir(), sid.clone());
    cfg.extra_env.push(("FAKE_MODE".into(), "term-143".into()));
    let (mut session, events) = ClaudeCodeSession::spawn(&cfg).unwrap();

    let (extra_tx, extra_rx) = mpsc::channel(1);
    drop(extra_tx);
    let rec = tokio::spawn(record_session(events, extra_rx, writer, dir.clone(), test_meta(&sid)));

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
    assert!(matches!(
        &loaded.records.last().unwrap().event,
        JournalEvent::Closed { outcome: Outcome::Killed { code: Some(143), .. } }
    ));
    let meta = Meta::load(&dir).unwrap();
    assert_eq!(meta.state, "killed");
    assert!(meta.child.is_none());
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

    // Wait until the outstanding tool is on disk — we are provably mid-turn.
    wait_for_file(&jpath, |c| c.contains("tool_use_started")).await;
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

    // Transcript intact; repair settles the turn as interrupted.
    let mut loaded = journal::load(&jpath).unwrap();
    assert_eq!(loaded.header.session_id, sid);
    assert!(loaded.records.iter().any(|r| matches!(
        &r.event,
        JournalEvent::UserSent { text } if text == "please hang in a tool"
    )));
    assert!(!loaded.is_terminal());
    let repair = journal::repair(&jpath, &mut loaded).unwrap();
    assert_eq!(repair.tools_interrupted, vec!["toolu_hang".to_string()]);
    assert!(repair.turn_interrupted);
    assert!(journal::load(&jpath).unwrap().is_terminal());

    // Continuation: next message goes to a fresh process continuing the
    // same provider session via --resume <session_id>.
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        loaded.header.session_id.clone(),
    );
    cfg.resume = true;
    cfg.extra_env.push(("FAKE_MODE".into(), "resume-probe".into()));
    let (_session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
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
    // valid event and no partial record surfaces.
    let mut loaded = journal::load(&jpath).unwrap();
    assert!(
        !loaded.records.is_empty(),
        "flood ran long enough to journal events"
    );
    assert!(!loaded.is_terminal());
    journal::repair(&jpath, &mut loaded).unwrap();
    let reloaded = journal::load(&jpath).unwrap();
    assert!(reloaded.is_terminal());
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
    let try_lock = |root: &Path| {
        Command::new(env!("CARGO_BIN_EXE_engine-stub"))
            .env("STUB_TRY_LOCK", "1")
            .env("STUB_DATA_DIR", root)
            .status()
            .expect("run lock probe")
            .code()
    };
    let root = scratch();
    let held = EngineLock::acquire(&root).expect("first acquire");
    assert_eq!(
        try_lock(&root),
        Some(42),
        "a second instance must see Busy while the lock is held"
    );
    drop(held);
    assert_eq!(
        try_lock(&root),
        Some(0),
        "the lock must be free once the holder releases it"
    );
}

#[test]
fn concurrent_sessions_in_one_store_do_not_cross_contaminate() {
    let root = scratch();
    let sid_a = uuid::Uuid::new_v4().to_string();
    let sid_b = uuid::Uuid::new_v4().to_string();
    let mut a = spawn_stub(&root, &sid_a, "tool-cycle", Some("message for A"));
    let mut b = spawn_stub(&root, &sid_b, "tool-cycle", Some("message for B"));
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    for (sid, own, other) in [
        (&sid_a, "message for A", "message for B"),
        (&sid_b, "message for B", "message for A"),
    ] {
        let dir = root.join("sessions").join(sid);
        let loaded = journal::load(&dir.join("journal.jsonl")).unwrap();
        assert_eq!(&loaded.header.session_id, sid);
        assert!(loaded.is_terminal());
        assert!(matches!(
            &loaded.records.last().unwrap().event,
            JournalEvent::Closed { outcome: Outcome::Done }
        ));
        let text = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert!(text.contains(own));
        assert!(
            !text.contains(other),
            "session {sid} journal contains the other session's message"
        );
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
    let (mut session, events) = ClaudeCodeSession::spawn(&cfg).unwrap();
    let (extra_tx, extra_rx) = mpsc::channel(4);
    let rec = tokio::spawn(record_session(events, extra_rx, writer, dir.clone(), test_meta(&sid)));

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
    assert!(matches!(outcome, Outcome::Killed { code: Some(143), .. }));
    assert!(journal::load(&jpath).unwrap().is_terminal());

    // Cold resume from the journal's cursor: fresh process, same session.
    let loaded = journal::load(&jpath).unwrap();
    let mut cfg2 = SpawnConfig::new("claude", std::env::temp_dir(), loaded.header.session_id);
    cfg2.resume = true;
    let (mut session2, mut rx2) = ClaudeCodeSession::spawn(&cfg2).unwrap();
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
