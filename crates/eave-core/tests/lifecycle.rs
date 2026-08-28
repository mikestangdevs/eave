//! Stage A integration tests: spawn, stream-JSON parse, process-group
//! identity, verified kill (docs/spec/01-lifecycle.md, 04-slice.md).
//!
//! All tests drive the real `ClaudeCodeSession` spawn path against the
//! fake-agent harness; behavior is selected per-spawn via `extra_env`.
//!
//! Spawn through [`support::spawn_agent`], not `ClaudeCodeSession::spawn`:
//! the handle it returns reaps its process group on drop, which is the
//! only reap that survives a panicking test. `support/mod.rs` says why
//! that matters and what fails the run when someone forgets.

use std::time::Duration;

use eave_core::claude::ClaudeCodeSession;
use eave_core::events::SessionEvent;
use eave_core::proc;
use eave_core::process;
use eave_core::session::{AgentSession, SpawnConfig};
use tokio::sync::mpsc::Receiver;

mod support;
use support::spawn_agent;

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
    let (session, mut rx) = spawn_agent(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    // The fake honors argv --session-id like the real CLI does (verified
    // live 2026-08-15): the caller-chosen id round-trips through Init.
    assert!(matches!(
        &events[0],
        SessionEvent::Init { session_id, .. } if *session_id == cfg.session_id
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
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::TurnCompleted {
            is_error: false,
            ..
        }
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { code: Some(0), .. }
    ));
}

#[tokio::test]
async fn malformed_lines_surface_without_breaking_the_parser() {
    let cfg = fake_cfg("malformed", &[]);
    let (_session, mut rx) = spawn_agent(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    let malformed = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Malformed { .. }))
        .count();
    assert!(
        malformed >= 2,
        "expected ≥2 malformed events, saw {malformed}"
    );
    // Parsing continued past the garbage: later valid events still arrived.
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::AssistantText { text } if text.contains("still alive"))
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnCompleted { .. })));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { code: Some(0), .. }
    ));
}

/// Stage L, reconciling the Slice K caveat: `in_flight_tools()` counts
/// tool calls and `pending_approvals()` counts open questions, and the
/// two are different facts that must never be summed.
///
/// This test pins the coupling that makes them *look* redundant. On the
/// real wire (captures C/D/E, `docs/research/approvals-capture-2026-08-18/`)
/// a `can_use_tool` request always follows that tool's own `tool_use`
/// block, so a parked session reports one of each — for the SAME tool.
/// That is the CLI's ordering, not eave's guarantee: if it ever changes,
/// this test says so, and anything that had quietly started treating
/// either number as "is this session busy?" needs revisiting.
#[tokio::test]
async fn an_open_question_and_an_in_flight_tool_are_separate_counts() {
    let cfg = fake_cfg("approval-hang", &[]);
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;
    session.send_user_message("go", &[]).await.unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;

    assert_eq!(
        session.pending_approvals(),
        1,
        "the session is parked on exactly one question"
    );
    assert_eq!(
        session.in_flight_tools(),
        1,
        "and the tool that question is about is still outstanding — the \
         CLI announces the tool before it asks (captures C/D/E). If this \
         ever reads 0, the counts have come apart and nothing may assume \
         a parked session shows a tool in flight"
    );

    // Reap. `approval-hang` parks forever by design, and nothing in this
    // crate kills a child on drop — every other test here either runs the
    // agent to exit or kills it explicitly. This one asserted and returned,
    // which orphaned exactly one hung fake-agent per `cargo test` run,
    // reparented to init, in a codebase whose entire subject is
    // child-process lifecycle. Kill is the file's convention and it is
    // provable, so assert that too rather than firing and hoping.
    //
    // Slice AE: this line is still right and it was never enough. It runs
    // only when everything above it succeeded, so every panic above —
    // including the `.expect()` inside `recv_until`'s 30 s timeout — walked
    // straight past it. All 34 orphans found on this machine on 2026-08-20
    // carried `FAKE_MODE=approval-hang`: this test, on a run that failed or
    // was interrupted. The reap that covers those paths is
    // `support::ReapedSession`'s `Drop`, which the handle now carries.
    let report = session.kill().await.expect("kill must be provable");
    assert!(!process::group_alive(session.identity().pid));
    assert!(
        report.stragglers_killed.is_empty(),
        "the parked agent should have no descendants to strand"
    );
}

#[tokio::test]
async fn tool_use_tracking_counts_in_flight_tools() {
    let cfg = fake_cfg("tool-cycle", &[]);
    let (session, mut rx) = spawn_agent(&cfg).unwrap();

    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseStarted { .. })
    })
    .await;
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
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let report = session.kill().await.expect("kill must be provable");
    assert!(!report.escalated, "SIGTERM should have sufficed");
    assert!(!report.already_exited);
    assert!(report.stragglers_killed.is_empty());
    assert!(!process::group_alive(session.identity().pid));

    let events = collect_until_exit(&mut rx).await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited {
            signal: Some(libc::SIGTERM),
            ..
        }
    ));
}

#[tokio::test]
async fn stubborn_group_is_escalated_and_leaves_no_survivors() {
    let cfg = fake_cfg("hang-ignore-term", &[("FAKE_SPAWN_CHILD", "1")]);
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    // Init is emitted after the grandchild is spawned, so the group has two
    // SIGTERM-ignoring members by the time we kill.
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;
    let pgid = session.identity().pid;

    let report = session.kill().await.expect("kill must be provable");
    assert!(
        report.escalated,
        "SIGTERM was ignored; SIGKILL was required"
    );

    // Independent verification: no process anywhere still claims this pgid.
    assert!(!process::group_alive(pgid));
    let survivors = process::scan_group_pids(pgid).await.unwrap();
    assert!(survivors.is_empty(), "surviving pids: {survivors:?}");

    collect_until_exit(&mut rx).await;
}

#[tokio::test]
async fn crash_surfaces_exit_code_and_stderr_tail() {
    let cfg = fake_cfg("crash", &[]);
    let (_session, mut rx) = spawn_agent(&cfg).unwrap();
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
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    recv_until(
        &mut rx,
        |e| matches!(e, SessionEvent::AssistantText { text } if text.contains("alive before term")),
    )
    .await;

    // On SIGTERM the fake prints protocol lines, then exits. Those lines are
    // stale pipe output (spec 01 §Kill) and must never surface as events.
    session.kill().await.expect("kill must be provable");
    let events = collect_until_exit(&mut rx).await;
    let leaked = events.iter().any(|e| {
        matches!(e, SessionEvent::AssistantText { text } | SessionEvent::TextDelta { text }
            if text.contains("POST_TERM_MARKER"))
    });
    assert!(
        !leaked,
        "stale post-kill protocol lines surfaced: {events:?}"
    );
}

#[tokio::test]
async fn identity_guard_stops_matching_after_exit() {
    let cfg = fake_cfg("hang-silent", &[]);
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let id = session.identity();
    assert!(
        proc::identity_matches(&id),
        "live child must match its identity"
    );

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
    let (_session, mut rx) = spawn_agent(&cfg).unwrap();
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

/// The env var that arms `deliberately_leaks_an_agent`. Without it that
/// fixture does nothing, so `cargo test -- --ignored` (the documented way
/// to run the real-CLI smoke tests below) cannot strand an agent.
const LEAK_FIXTURE_ENV: &str = "EAVE_TEST_LEAK_FIXTURE";

/// The reap must survive the path a failing test actually takes.
///
/// A test that panics never reaches its own `kill()` call, and
/// `ClaudeCodeSession` does not kill on drop — that is a product
/// guarantee (a session outlives the UI), not an oversight. So the
/// harness has to do it, during the unwind, or a failing run leaves an
/// agent behind. `approval-hang` is the specimen because it is the mode
/// that parks forever and the mode every orphan on this machine was
/// running.
#[tokio::test]
async fn a_panicking_test_still_reaps_its_agent() {
    let cfg = fake_cfg("approval-hang", &[]);
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;
    // Drive it all the way to the parked question, like the test whose
    // orphans were found in the field. Only past that point does the fake
    // stop touching its pipes, which is what let 34 of them survive their
    // parents indefinitely — one that is still blocked on its first
    // stdin read dies on its own when the run ends, and would prove less.
    session.send_user_message("go", &[]).await.unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let id = session.identity();
    assert!(
        proc::identity_matches(&id),
        "the specimen must be alive before it can be proven reaped"
    );

    // Unwind through the handle exactly as a failed assertion would.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _holds_the_agent = session;
        panic!("a test failing while its agent is parked");
    }));
    assert!(unwound.is_err(), "the panic must unwind, not be swallowed");

    assert!(
        !proc::identity_matches(&id),
        "the agent outlived a panicking test — pid {} is an orphan",
        id.pid
    );
    assert!(
        !process::group_alive(id.pid),
        "the agent's process group outlived a panicking test"
    );
}

/// The guard itself, exercised end to end: a run that leaks a child must
/// FAIL, not pass quietly and leave the machine dirty.
///
/// This is the test that generalises. `ReapedSession` fixes the call
/// sites that exist; this one fails the next call site that forgets,
/// including one written by someone who never read this file. It re-runs
/// this very binary against a fixture that leaks on purpose and requires
/// a non-zero exit and a message that names the fault.
#[test]
fn a_leaked_agent_fails_the_run() {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = std::process::Command::new(&exe)
        .args([
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "deliberately_leaks_an_agent",
        ])
        .env(LEAK_FIXTURE_ENV, "1")
        .output()
        .expect("re-run this test binary");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    // Reap first, assert second: an assertion that fires before the kill
    // would make the test that guards against orphans leave one.
    // libtest prints the fixture's line inline after the test name, so
    // match the marker anywhere rather than at the start of a line.
    let leaked_pid: i32 = stdout
        .split("LEAKED_PID=")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("fixture never reported a pid\nstdout: {stdout}"));
    let was_alive = proc::identity_of(leaked_pid).is_some();
    unsafe {
        libc::killpg(leaked_pid, libc::SIGKILL);
    }

    assert!(
        was_alive,
        "the fixture's agent was already gone, so this proves nothing about \
         the guard — pid {leaked_pid}"
    );
    assert!(
        !out.status.success(),
        "a run that leaked a child reported success\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("LEAKED CHILD PROCESS"),
        "the run failed but never said why\nstderr: {stderr}"
    );
}

/// Not a test — the fixture `a_leaked_agent_fails_the_run` drives in a
/// child process. It spawns an agent that parks forever and returns
/// without reaping it: exactly the shape that put 34 processes on this
/// machine. It deliberately calls `ClaudeCodeSession::spawn` rather than
/// `support::spawn_agent`, because what is being tested is the guard that
/// catches a call site which never used the helper.
#[tokio::test]
#[ignore = "fixture for a_leaked_agent_fails_the_run; leaks a process on purpose"]
async fn deliberately_leaks_an_agent() {
    if std::env::var(LEAK_FIXTURE_ENV).is_err() {
        return;
    }
    support::arm_leak_guard();
    let cfg = fake_cfg("approval-hang", &[]);
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;
    // Park it on the question first: past that point the fake touches
    // neither pipe again, so it survives this process rather than dying
    // of EPIPE when the run ends. A specimen that cleans itself up would
    // make the guard look stronger than it is.
    session.send_user_message("go", &[]).await.unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    println!("LEAKED_PID={}", session.identity().pid);
    std::mem::forget(session);
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
    let (mut session, mut rx) = spawn_agent(&cfg).unwrap();
    session
        .send_user_message("Reply with exactly the word: pineapple", &[])
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

/// Slice P2, directive item 4: a session whose config was set at open must
/// still `--resume` against the REAL claude CLI — the one thing the fake
/// agent cannot prove, because it accepts whatever argv it is handed.
///
/// Two spawns over one provider session id: the first names it
/// (`--session-id`) with an explicit `--model`, the second resumes it
/// (`--resume`) with the same model, and the second must see the first
/// turn's content. If `--model` and `--resume` were mutually exclusive in
/// the CLI, or if the model changed the resume cursor's meaning, this is
/// where it would show.
///
/// `cargo test -p eave-core --test lifecycle -- --ignored real_claude_resume_with_model`
#[tokio::test]
#[ignore]
async fn real_claude_resume_with_model() {
    let id = uuid::Uuid::new_v4().to_string();
    let model = std::env::var("EAVE_TEST_MODEL").unwrap_or_else(|_| "haiku".into());

    let mut first = SpawnConfig::new("claude", std::env::temp_dir(), id.clone());
    first.model = Some(model.clone());
    let (mut session, mut rx) = spawn_agent(&first).expect("spawn 1");
    session
        .send_user_message("Remember the word pineapple. Reply with just: ok", &[])
        .await
        .expect("send 1");
    let init_mode = wait_turn(&mut rx).await;
    assert!(
        init_mode.is_some(),
        "real claude never stated a permission mode on spawn 1 — posture unverifiable"
    );
    drop(session);

    // Second spawn: same id, same model, resuming.
    let mut second = SpawnConfig::new("claude", std::env::temp_dir(), id.clone());
    second.model = Some(model.clone());
    second.resume = true;
    let (mut session, mut rx) = spawn_agent(&second).expect("spawn 2 (--resume)");
    session
        .send_user_message(
            "What word did I ask you to remember? Reply with just that word.",
            &[],
        )
        .await
        .expect("send 2");

    let mut said = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = rx.recv().await {
            if let SessionEvent::AssistantText { text, .. } = &event {
                said.push_str(text);
            }
            if matches!(event, SessionEvent::TurnCompleted { .. }) {
                break;
            }
        }
    })
    .await;
    assert!(
        said.to_lowercase().contains("pineapple"),
        "--resume with an explicit --model lost the session's history; it said: {said:?}"
    );
}

/// Drain one turn and report the permission mode the CLI announced.
async fn wait_turn(rx: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> Option<String> {
    let mut mode = None;
    let _ = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = rx.recv().await {
            if let SessionEvent::Init {
                permission_mode, ..
            } = &event
            {
                mode = permission_mode.clone();
            }
            if matches!(event, SessionEvent::TurnCompleted { .. }) {
                break;
            }
        }
    })
    .await;
    mode
}
