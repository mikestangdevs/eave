//! Stage A integration tests: spawn, stream-JSON parse, process-group
//! identity, verified kill (docs/spec/01-lifecycle.md, 04-slice.md).
//!
//! All tests drive the real `ClaudeCodeSession` spawn path against the
//! fake-agent harness; behavior is selected per-spawn via `extra_env`.

use std::time::Duration;

use eave_core::claude::ClaudeCodeSession;
use eave_core::events::SessionEvent;
use eave_core::proc;
use eave_core::process;
use eave_core::session::{AgentSession, SpawnConfig};
use tokio::sync::mpsc::Receiver;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn fake_cfg(mode: &str, extra: &[(&str, &str)]) -> SpawnConfig {
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-agent"),
        std::env::temp_dir(),
        format!("test-{}", uuid::Uuid::new_v4()),
    );
    cfg.extra_env.push(("FAKE_MODE".into(), mode.into()));
    for (k, v) in extra {
        cfg.extra_env.push((k.to_string(), v.to_string()));
    }
    cfg
}

/// Drain the event channel until `Exited` (always the final event) arrives.
async fn collect_until_exit(rx: &mut Receiver<SessionEvent>) -> Vec<SessionEvent> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let done = matches!(event, SessionEvent::Exited { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("event stream did not reach Exited in time")
}

/// Wait for a specific event, returning everything seen up to and including it.
async fn recv_until(
    rx: &mut Receiver<SessionEvent>,
    mut pred: impl FnMut(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let hit = pred(&event);
            events.push(event);
            if hit {
                return events;
            }
        }
        panic!("channel closed before expected event; saw: {events:?}");
    })
    .await
    .expect("expected event did not arrive in time")
}

#[tokio::test]
async fn normal_turn_streams_events_and_exits_cleanly() {
    let cfg = fake_cfg("normal", &[]);
    let (session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    // The fake honors argv --session-id like the real CLI does (verified
    // live 2026-08-15): the caller-chosen id round-trips through Init.
    assert!(matches!(
        &events[0],
        SessionEvent::Init { session_id } if *session_id == cfg.session_id
    ));
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some(cfg.session_id.as_str())
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::TextDelta { text } if text.contains("hello"))));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantText { text } if text.contains("hello"))));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnCompleted { is_error: false, .. })));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { code: Some(0), .. }
    ));
}

#[tokio::test]
async fn malformed_lines_surface_without_breaking_the_parser() {
    let cfg = fake_cfg("malformed", &[]);
    let (_session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    let malformed = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Malformed { .. }))
        .count();
    assert!(malformed >= 2, "expected ≥2 malformed events, saw {malformed}");
    // Parsing continued past the garbage: later valid events still arrived.
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantText { text } if text.contains("still alive"))));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnCompleted { .. })));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { code: Some(0), .. }
    ));
}

#[tokio::test]
async fn tool_use_tracking_counts_in_flight_tools() {
    let cfg = fake_cfg("tool-cycle", &[]);
    let (session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();

    recv_until(&mut rx, |e| matches!(e, SessionEvent::ToolUseStarted { .. })).await;
    assert_eq!(session.in_flight_tools(), 1);

    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseFinished { .. })
    })
    .await;
    assert_eq!(session.in_flight_tools(), 0);
    // The tool_use id arrives twice (content_block_start + the complete
    // assistant snapshot); the first window consumed the single Started
    // event, so a duplicate would show up here.
    let duplicate_started = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::ToolUseStarted { .. }))
        .count();
    assert_eq!(duplicate_started, 0);

    collect_until_exit(&mut rx).await;
}

#[tokio::test]
async fn graceful_kill_is_verified_without_escalation() {
    let cfg = fake_cfg("hang-silent", &[]);
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let report = session.kill().await.expect("kill must be provable");
    assert!(!report.escalated, "SIGTERM should have sufficed");
    assert!(!report.already_exited);
    assert!(report.stragglers_killed.is_empty());
    assert!(!process::group_alive(session.identity().pid));

    let events = collect_until_exit(&mut rx).await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { signal: Some(libc::SIGTERM), .. }
    ));
}

#[tokio::test]
async fn stubborn_group_is_escalated_and_leaves_no_survivors() {
    let cfg = fake_cfg("hang-ignore-term", &[("FAKE_SPAWN_CHILD", "1")]);
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    // Init is emitted after the grandchild is spawned, so the group has two
    // SIGTERM-ignoring members by the time we kill.
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;
    let pgid = session.identity().pid;

    let report = session.kill().await.expect("kill must be provable");
    assert!(report.escalated, "SIGTERM was ignored; SIGKILL was required");

    // Independent verification: no process anywhere still claims this pgid.
    assert!(!process::group_alive(pgid));
    let survivors = process::scan_group_pids(pgid).await.unwrap();
    assert!(survivors.is_empty(), "surviving pids: {survivors:?}");

    collect_until_exit(&mut rx).await;
}

#[tokio::test]
async fn crash_surfaces_exit_code_and_stderr_tail() {
    let cfg = fake_cfg("crash", &[]);
    let (_session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    match events.last().unwrap() {
        SessionEvent::Exited {
            code: Some(3),
            stderr_tail,
            ..
        } => assert!(
            stderr_tail.contains("synthetic failure"),
            "stderr tail missing: {stderr_tail:?}"
        ),
        other => panic!("expected Exited(code=3), got {other:?}"),
    }
}

#[tokio::test]
async fn protocol_lines_after_kill_initiation_are_dropped() {
    let cfg = fake_cfg("chatty-term", &[]);
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::AssistantText { text } if text.contains("alive before term"))
    })
    .await;

    // On SIGTERM the fake prints protocol lines, then exits. Those lines are
    // stale pipe output (spec 01 §Kill) and must never surface as events.
    session.kill().await.expect("kill must be provable");
    let events = collect_until_exit(&mut rx).await;
    let leaked = events.iter().any(|e| {
        matches!(e, SessionEvent::AssistantText { text } | SessionEvent::TextDelta { text }
            if text.contains("POST_TERM_MARKER"))
    });
    assert!(!leaked, "stale post-kill protocol lines surfaced: {events:?}");
}

#[tokio::test]
async fn identity_guard_stops_matching_after_exit() {
    let cfg = fake_cfg("hang-silent", &[]);
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let id = session.identity();
    assert!(proc::identity_matches(&id), "live child must match its identity");

    // kill() returns only after the child is reaped, so the pid is free (or
    // reusable) — the guard must refuse it from now on.
    session.kill().await.unwrap();
    assert!(
        !proc::identity_matches(&id),
        "identity must not match once the recorded process is gone"
    );
    collect_until_exit(&mut rx).await;
}

#[tokio::test]
async fn inherited_eave_env_is_stripped_from_the_child() {
    // Edition 2021: set_var is safe; the var name is unique to this test.
    std::env::set_var("EAVE_LEAK_CHECK", "1");
    let cfg = fake_cfg("env-report", &[]);
    let (_session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;
    std::env::remove_var("EAVE_LEAK_CHECK");

    let report = events.iter().find_map(|e| match e {
        SessionEvent::AssistantText { text } => Some(text.clone()),
        _ => None,
    });
    let report = report.expect("env-report emitted no assistant text");
    assert!(
        !report.contains("EAVE_LEAK_CHECK"),
        "EAVE_* env leaked into the child: {report}"
    );
}

/// Smoke test against the real `claude` CLI. Requires the CLI on PATH and
/// network/credentials, so it is ignored by default:
/// `cargo test -- --ignored claude_real_smoke`
#[tokio::test]
#[ignore]
async fn claude_real_smoke() {
    let cfg = SpawnConfig::new(
        "claude",
        std::env::temp_dir(),
        uuid::Uuid::new_v4().to_string(),
    );
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    session
        .send_user_message("Reply with exactly the word: pineapple")
        .await
        .unwrap();

    let events = tokio::time::timeout(Duration::from_secs(120), async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let done = matches!(event, SessionEvent::TurnCompleted { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("no result from real claude in time");

    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::Init { .. })));
    assert!(session.provider_session_id().is_some());
    session.kill().await.unwrap();
}
