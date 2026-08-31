//! Stage AU integration tests: the Codex adapter against a scriptable
//! `codex app-server` stand-in.
//!
//! Every test drives the real `CodexSession::spawn` path — argv, process
//! group, env sanitising and all — against `fake-codex`, whose frame
//! shapes come from a live capture of the real binary (0.148.0) and from
//! that binary's own generated JSON Schema. Behaviour is selected
//! per-spawn via `extra_env`, the same way `lifecycle.rs` drives
//! `fake-agent`.
//!
//! **Nothing here touches the operator's Codex auth or the network.** The
//! one test that runs the real CLI is `#[ignore]`d and says so.
//!
//! Spawn through [`spawn_codex`], never `CodexSession::spawn` directly:
//! the guard it returns reaps the process group on drop, which is the only
//! reap that survives a panicking test (`support/mod.rs` says why).

use std::time::Duration;

use eave_core::codex::CodexSession;
use eave_core::events::SessionEvent;
use eave_core::proc;
use eave_core::session::{AgentSession, ApprovalDecision, SessionError, SpawnConfig};
use tokio::sync::mpsc::Receiver;

mod support;
use support::{adopt, Reaped};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn fake_cfg(mode: &str, extra: &[(&str, &str)]) -> SpawnConfig {
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-codex"),
        std::env::temp_dir(),
        format!("test-{}", uuid::Uuid::new_v4()),
    );
    cfg.extra_env.push(("FAKE_CODEX_MODE".into(), mode.into()));
    for (k, v) in extra {
        cfg.extra_env.push((k.to_string(), v.to_string()));
    }
    cfg
}

fn spawn_codex(
    cfg: &SpawnConfig,
) -> Result<(CodexSession, Receiver<SessionEvent>, Reaped), SessionError> {
    let (session, rx) = CodexSession::spawn(cfg)?;
    let guard = adopt(session.identity());
    Ok((session, rx, guard))
}

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

/// The text of every assistant message in a run, joined.
fn said(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantText { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn refusal(events: &[SessionEvent]) -> (String, String) {
    events
        .iter()
        .find_map(|e| match e {
            SessionEvent::PostureRefused { mode, reason } => Some((mode.clone(), reason.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no PostureRefused among {events:?}"))
}

// ---------------------------------------------------------------------------
// The handshake and the posture it asks for
// ---------------------------------------------------------------------------

/// The three fields that decide whether eave sees writes at all are
/// spelled out in the literal, here, rather than read back from the
/// adapter's constants — a test that imports `WANT_SANDBOX_ARG` would pass
/// just as happily if someone changed it to `danger-full-access`.
///
/// `read-only` is the load-bearing one and the least obvious: under
/// `workspace-write` the agent writes inside the cwd with no approval
/// request, so eave would supervise a session whose writes it never saw.
#[tokio::test]
async fn the_handshake_asks_for_the_strict_posture() {
    let cfg = fake_cfg("handshake-report", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;
    let report = said(&events);

    assert!(report.starts_with("HANDSHAKE:thread/start:"), "{report}");
    assert!(
        report.contains(r#""approvalPolicy":"untrusted""#),
        "eave must ask for the policy under which anything untrusted escalates: {report}"
    );
    assert!(
        report.contains(r#""approvalsReviewer":"user""#),
        "approvals must be reviewed by the operator, not by a subagent: {report}"
    );
    assert!(
        report.contains(r#""sandbox":"read-only""#),
        "any wider sandbox writes without asking, which eave would never see: {report}"
    );
    assert!(
        report.contains(&format!(r#""cwd":"{}""#, cfg.cwd.to_string_lossy())),
        "the thread runs where the session runs: {report}"
    );
    // Absent model = no `model` key at all, which is not the same as an
    // empty one.
    assert!(!report.contains("\"model\""), "{report}");
}

#[tokio::test]
async fn a_configured_model_reaches_the_thread_not_the_command_line() {
    let mut cfg = fake_cfg("handshake-report", &[]);
    cfg.model = Some("gpt-5.6-terra".into());
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let report = said(&collect_until_exit(&mut rx).await);
    assert!(report.contains(r#""model":"gpt-5.6-terra""#), "{report}");
}

/// `codex app-server` takes no model and no cwd on the command line, so
/// the argv is fixed: the subcommand and the transport, then whatever the
/// session's own config appended.
#[tokio::test]
async fn argv_names_the_app_server_and_its_transport() {
    let cfg = fake_cfg("argv-report", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let report = said(&collect_until_exit(&mut rx).await);
    assert_eq!(report, "ARGV:app-server --stdio", "{report}");
}

#[tokio::test]
async fn no_eave_vars_reach_the_child() {
    // Set on THIS process, so the child would inherit it if the adapter
    // did not strip it.
    std::env::set_var("EAVE_CODEX_LEAK_PROBE", "1");
    let cfg = fake_cfg("env-report", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let report = said(&collect_until_exit(&mut rx).await);
    std::env::remove_var("EAVE_CODEX_LEAK_PROBE");
    assert_eq!(report, "ENV:no EAVE_ vars", "{report}");
}

// ---------------------------------------------------------------------------
// Session identity: discovered, not supplied
// ---------------------------------------------------------------------------

/// Codex's `thread/start` has no parameter for a caller-chosen id — the
/// server names the thread and tells you afterwards. So unlike Claude,
/// where eave's own session id IS the provider's, the two differ here and
/// `provider_session_id()` is the only place the Codex id exists.
#[tokio::test]
async fn a_fresh_thread_id_is_discovered_rather_than_supplied() {
    let cfg = fake_cfg(
        "normal",
        &[("FAKE_CODEX_THREAD", "01a020c6-1111-7000-8000-server1d")],
    );
    let (session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let SessionEvent::Init { session_id, .. } = &events[0] else {
        panic!("first event was not Init: {events:?}");
    };
    assert_eq!(session_id, "01a020c6-1111-7000-8000-server1d");
    assert_ne!(
        session_id, &cfg.session_id,
        "eave's session id is not the provider's, and pretending otherwise \
         would give the wrong cursor to a resume"
    );
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some("01a020c6-1111-7000-8000-server1d")
    );
}

/// A resume names the thread on `thread/resume`, and the resumed thread
/// keeps that id.
///
/// This test used to carry the comment "this is the one path where
/// `SpawnConfig::session_id` must already BE the Codex thread id" — a
/// requirement stated in a comment that nothing in the engine ever
/// satisfied, because the engine passes eave's own uuid. The requirement
/// now lives in the type (`resume: Option<String>`) instead, and the two
/// ids are set to different values so that reading the wrong one fails.
#[tokio::test]
async fn a_resume_names_the_thread_it_is_continuing() {
    let mut cfg = fake_cfg("handshake-report", &[]);
    cfg.session_id = "eave-side-id-that-codex-never-issued".into();
    cfg.resume = Some("01a020c6-2222-7000-8000-resumeme".into());
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;
    let report = said(&events);

    assert!(report.starts_with("HANDSHAKE:thread/resume:"), "{report}");
    assert!(
        report.contains(r#""threadId":"01a020c6-2222-7000-8000-resumeme""#),
        "{report}"
    );
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some("01a020c6-2222-7000-8000-resumeme"),
        "a resumed thread keeps the id it was resumed by"
    );
}

// ---------------------------------------------------------------------------
// Posture verification — the gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_normal_turn_streams_text_and_exits_cleanly() {
    let cfg = fake_cfg("normal", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("say hello", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(
        matches!(
            &events[0],
            SessionEvent::Init { permission_mode: Some(m), .. } if m == "untrusted/user/readOnly"
        ),
        "{events:?}"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::UserEcho { text, .. } if text == "say hello")));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::TextDelta { text } if text == "hello")));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantText { text } if text == "hello")));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::TurnCompleted { subtype, is_error: false } if subtype == "completed"
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { code: Some(0), .. }
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::PostureRefused { .. })),
        "a compliant posture must not be refused: {events:?}"
    );
}

/// The refusal is driven by what the SERVER reports, not by what eave
/// asked for. A server that answers a strict request with a permissive
/// posture is the whole reason the echo is read at all.
#[tokio::test]
async fn a_server_that_reports_a_permissive_posture_is_refused() {
    let cfg = fake_cfg("posture-lie", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;

    // The Init is emitted anyway: what the CLI said it was doing is worth
    // journaling especially when the answer is that eave will not run it.
    assert!(
        matches!(
            &events[0],
            SessionEvent::Init { permission_mode: Some(m), .. } if m == "never/auto_review/dangerFullAccess"
        ),
        "{events:?}"
    );
    let (mode, reason) = refusal(&events);
    assert_eq!(mode, "never/auto_review/dangerFullAccess");
    assert!(reason.contains("approval policy"), "{reason}");
}

/// The subtle one, and the reason `sandbox` is checked at all: policy and
/// reviewer are both exactly what eave asked for, and the session still
/// must not run, because under `workspaceWrite` a write inside the cwd
/// produces no approval request.
#[tokio::test]
async fn a_wider_sandbox_is_refused_even_with_the_right_policy_and_reviewer() {
    let cfg = fake_cfg("posture-sandbox", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (mode, reason) = refusal(&events);
    assert_eq!(mode, "untrusted/user/workspaceWrite");
    assert!(reason.contains("sandbox"), "{reason}");
    assert!(
        reason.contains("without an approval request"),
        "the reason must say what eave would stop seeing: {reason}"
    );
}

#[tokio::test]
async fn an_automatic_reviewer_is_refused_because_the_operator_would_not_be_asked() {
    let cfg = fake_cfg("posture-reviewer", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (mode, reason) = refusal(&events);
    assert_eq!(mode, "untrusted/auto_review/readOnly");
    assert!(reason.contains("automatic reviewer"), "{reason}");
}

/// Fail-closed: a server that states no posture at all is refused, not
/// assumed compliant. "Does this posture review writes?" has no safe
/// default.
#[tokio::test]
async fn a_server_that_states_no_posture_is_refused() {
    let cfg = fake_cfg("posture-silent", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (mode, _) = refusal(&events);
    assert_eq!(mode, "unstated/unstated/unstated");
}

#[tokio::test]
async fn an_initialize_the_server_refuses_is_a_posture_refusal() {
    let cfg = fake_cfg("init-reject", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (mode, reason) = refusal(&events);
    assert_eq!(mode, "unknown");
    assert!(reason.contains("initialize"), "{reason}");
}

/// The shape a missing resume takes. Verified live against the real
/// binary: `thread/resume` for an unknown id answers
/// `-32600 no rollout found for thread id …` rather than silently
/// starting a fresh thread — so eave must never treat a failed resume as
/// a new session.
#[tokio::test]
async fn a_thread_that_cannot_be_opened_is_a_posture_refusal() {
    let mut cfg = fake_cfg("thread-error", &[]);
    cfg.resume = Some("01a020c6-3333-7000-8000-goneaway".into());
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (_, reason) = refusal(&events);
    assert!(reason.contains("no rollout found"), "{reason}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Init { .. })),
        "a thread that never opened must not report an Init: {events:?}"
    );
}

#[tokio::test]
async fn a_thread_the_server_did_not_name_is_a_posture_refusal() {
    let cfg = fake_cfg("unnamed-thread", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    let (mode, _) = refusal(&events);
    assert_eq!(mode, "unstated");
}

/// A flag that would put the server in an unreviewable posture is refused
/// before a process exists at all — the engine's own filter names Claude's
/// flags and would let every one of these through.
#[test]
fn a_posture_undoing_flag_is_refused_before_a_process_exists() {
    for bad in [
        "--dangerously-bypass-approvals-and-sandbox",
        "--approve-for-me",
        "-s",
        "danger-full-access",
        "-c",
    ] {
        let mut cfg = fake_cfg("normal", &[]);
        cfg.extra_args = match bad {
            "danger-full-access" => vec!["--sandbox".into(), bad.into()],
            "-c" => vec!["-c".into(), "approval_policy=never".into()],
            _ => vec![bad.into()],
        };
        let err = CodexSession::spawn(&cfg)
            .err()
            .unwrap_or_else(|| panic!("{bad} was accepted"));
        assert!(
            format!("{err}").contains("undo the permission posture"),
            "{bad}: {err}"
        );
    }
}

#[tokio::test]
async fn an_ordinary_flag_still_reaches_the_child() {
    let mut cfg = fake_cfg("argv-report", &[]);
    cfg.extra_args = vec!["--strict-config".into()];
    let (session, _rx) = CodexSession::spawn(&cfg).expect("harmless flag must be allowed");
    let _guard = adopt(session.identity());
}

// ---------------------------------------------------------------------------
// Tool accounting
// ---------------------------------------------------------------------------

/// `in_flight_tools` counts tool calls and only tool calls. The fake emits
/// a `reasoning` item alongside the command: a real item, not a tool, and
/// the count must not move for it — that distinction is the difference
/// between "the agent is working" and "the agent is thinking".
#[tokio::test]
async fn tool_calls_count_in_and_back_out_and_reasoning_does_not() {
    let cfg = fake_cfg("tool-cycle", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();

    let mut events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseStarted { .. })
    })
    .await;
    assert_eq!(
        session.in_flight_tools(),
        1,
        "the command is outstanding: {events:?}"
    );
    events.extend(collect_until_exit(&mut rx).await);
    assert_eq!(session.in_flight_tools(), 0, "{events:?}");

    let starts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolUseStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec!["commandExecution"],
        "the reasoning item is not a tool call: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ToolUseStarted { input, .. }
                if input.contains("echo hello") && input.contains("/tmp")
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ToolUseFinished { id, result, is_error: false,
                diff: None,
            }
                if id == "c1" && result.contains("exit 0") && result.contains("hello")
        )),
        "{events:?}"
    );
    // It was still recorded, just not as a tool: a journal that drops an
    // item because eave has no row for it loses history.
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::System { subtype, .. } if subtype == "item/started/reasoning"
        )),
        "{events:?}"
    );
}

/// A tool that never resolves keeps the count at one, indefinitely.
///
/// This is the input the attention clocks branch on (spec 01: while
/// in-flight > 0, `QUIET_AFTER` does not run and `TOOL_EXEC_AFTER`
/// applies instead), so a count that decayed on its own would silently
/// change which clock a stuck session is measured by.
#[tokio::test]
async fn a_tool_that_never_resolves_stays_in_flight() {
    let cfg = fake_cfg("tool-hang", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseStarted { .. })
    })
    .await;

    assert_eq!(session.in_flight_tools(), 1);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        session.in_flight_tools(),
        1,
        "nothing but a resolve may lower this count"
    );
    session.kill().await.expect("kill must be provable");
}

/// `is_error` is the provider's own status, not an inference from the
/// output text.
#[tokio::test]
async fn a_failed_tool_reports_the_providers_own_status() {
    let cfg = fake_cfg("tool-fail", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::ToolUseFinished { is_error: true, result, .. } if result.contains("exit 2")
    )), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::TurnCompleted { subtype, is_error: true } if subtype == "failed"
        )),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approval_parks_the_session_and_an_allow_reaches_the_wire() {
    let cfg = fake_cfg("approval-cycle", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();

    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested {
        request_id,
        tool,
        display_name,
        description,
        path,
        input_json,
        suggestions_json,
    } = events.last().unwrap().clone()
    else {
        unreachable!()
    };
    assert_eq!(tool, "commandExecution");
    assert_eq!(display_name, "Run command");
    assert_eq!(description, "command is not on the trusted list");
    assert_eq!(path.as_deref(), Some("/tmp"));
    assert!(input_json.contains("echo hello"), "{input_json}");
    assert!(
        suggestions_json.unwrap().contains("acceptForSession"),
        "the provider's own routes out of being asked are journaled verbatim"
    );
    assert_eq!(session.pending_approvals(), 1);

    assert!(session
        .respond_to_approval(&request_id, &ApprovalDecision::Allow)
        .await
        .unwrap());
    assert_eq!(session.pending_approvals(), 0);

    let rest = collect_until_exit(&mut rx).await;
    assert!(
        said(&rest).contains("DECISION:accept"),
        "the decision must reach the server with the id it asked with: {rest:?}"
    );
}

/// Deny sends `decline`, not `cancel`: "no, not that" is not "stop
/// everything", and Codex's `cancel` interrupts the whole turn.
///
/// It also proves the fidelity gap this adapter cannot close — the
/// operator's message is NOT on the wire, because Codex's approval
/// decision is a closed enum with nowhere to put text.
#[tokio::test]
async fn a_denied_command_is_declined_and_the_operators_words_do_not_travel() {
    let cfg = fake_cfg("approval-cycle", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested { request_id, .. } = events.last().unwrap().clone() else {
        unreachable!()
    };

    session
        .respond_to_approval(
            &request_id,
            &ApprovalDecision::Deny {
                message: "commit first".into(),
            },
        )
        .await
        .unwrap();

    let rest = collect_until_exit(&mut rx).await;
    let heard = said(&rest);
    assert!(heard.contains("DECISION:decline"), "{heard}");
    assert!(
        !heard.contains("commit first"),
        "if this ever passes, Codex grew a channel for the deny message and \
         the adapter should start using it: {heard}"
    );
}

/// A decision for a question that is not open must never reach the pipe:
/// the server would ignore it and eave would journal it as if it had
/// landed.
#[tokio::test]
async fn a_stale_or_unknown_decision_never_reaches_the_pipe() {
    let cfg = fake_cfg("approval-hang", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested { request_id, .. } = events.last().unwrap().clone() else {
        unreachable!()
    };

    assert!(
        !session
            .respond_to_approval("never-asked", &ApprovalDecision::Allow)
            .await
            .unwrap(),
        "a request id nobody asked with is a race, not an error"
    );
    assert!(session
        .respond_to_approval(&request_id, &ApprovalDecision::Allow)
        .await
        .unwrap());
    assert!(
        !session
            .respond_to_approval(&request_id, &ApprovalDecision::Allow)
            .await
            .unwrap(),
        "answering twice must not double-send"
    );
    session.kill().await.unwrap();
}

#[tokio::test]
async fn a_withdrawn_request_stops_being_pending() {
    let cfg = fake_cfg("approval-withdraw", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("run it", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::ApprovalRequested { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::ApprovalWithdrawn { .. })));
    assert_eq!(
        session.pending_approvals(),
        0,
        "an unanswerable question left on screen is a lie"
    );
}

/// A file-change approval names no path in its own params — the paths are
/// in the item that started earlier. Correlating the two is the whole
/// point: approving without knowing what you approved is the failure this
/// event exists to prevent.
#[tokio::test]
async fn a_file_change_approval_names_the_paths_its_request_did_not() {
    let cfg = fake_cfg("file-approval", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("write it", &[]).await.unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested {
        tool,
        display_name,
        description,
        path,
        input_json,
        ..
    } = events.last().unwrap().clone()
    else {
        unreachable!()
    };
    assert_eq!(tool, "fileChange");
    assert_eq!(display_name, "Edit files");
    assert_eq!(path.as_deref(), Some("/tmp/eave-fake/note.txt"));
    assert!(description.contains("other.txt"), "{description}");
    assert!(input_json.contains("+hi"), "{input_json}");
    session.kill().await.unwrap();
}

/// A question eave cannot present is answered immediately with a stated
/// refusal. Silence would park the session on something nobody can see.
#[tokio::test]
async fn a_dialog_eave_cannot_present_is_refused_on_the_wire() {
    let cfg = fake_cfg("dialog-request", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("ask me", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::DialogRefused { dialog, .. } if dialog == "item/tool/requestUserInput"
        )),
        "{events:?}"
    );
    let heard = said(&events);
    assert!(heard.starts_with("DIALOG:-32603:"), "{heard}");
    assert!(heard.contains("Do not retry"), "{heard}");
}

/// A server request eave does not implement is answered `method not
/// found`, not with a dialog refusal: a clock read is not a question for a
/// human.
#[tokio::test]
async fn an_unimplemented_server_request_is_answered_method_not_found() {
    let cfg = fake_cfg("infra-request", &[]);
    let (_session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::System { subtype, .. } if subtype == "currentTime/read"
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::DialogRefused { .. })),
        "infrastructure is not a dialog: {events:?}"
    );
    assert!(said(&events).contains("INFRA:-32601"), "{events:?}");
}

// ---------------------------------------------------------------------------
// Robustness and kill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_lines_surface_without_breaking_the_parser() {
    let cfg = fake_cfg("malformed", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    session.send_user_message("go", &[]).await.unwrap();
    let events = collect_until_exit(&mut rx).await;

    assert!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::Malformed { .. }))
            .count()
            >= 2,
        "{events:?}"
    );
    // The parser kept going: the good frames on either side still arrived.
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::Init { .. })));
    assert_eq!(said(&events), "hello");
}

/// The child is `setsid`'d into its own group, and the kill proves exit
/// rather than reporting it on a delivered signal.
#[tokio::test]
async fn a_hung_server_is_killed_with_proof_of_exit() {
    let cfg = fake_cfg("hang-silent", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let id = session.identity();
    assert!(proc::identity_matches(&id));
    let report = session.kill().await.expect("kill must be provable");
    assert!(!report.already_exited);
    assert!(!proc::identity_matches(&id), "the leader is gone");

    let events = collect_until_exit(&mut rx).await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { .. }
    ));
}

/// A server that dies before opening a thread never stated a posture, and
/// a turn sent into it fails saying so rather than hanging.
#[tokio::test]
async fn a_server_that_dies_before_opening_a_thread_says_why() {
    let cfg = fake_cfg("init-reject", &[]);
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    session.kill().await.unwrap();

    let err = session
        .send_user_message("anyone there?", &[])
        .await
        .expect_err("there is no thread to send a turn to");
    assert!(
        format!("{err}").contains("exited before it opened a thread"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Against the real binary
// ---------------------------------------------------------------------------

/// The handshake against the REAL `codex app-server`, proving the posture
/// echo comes back exactly as this adapter requires.
///
/// Ignored by default: it needs `codex` on PATH. It does **not** need
/// Codex auth or the network — `initialize` and `thread/start` are
/// answered locally, and no turn is ever started. Run it with:
///
/// ```text
/// EAVE_CODEX_BIN=/opt/homebrew/bin/codex \
///   cargo test -p eave-core --test codex -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs the real codex CLI on this machine; set EAVE_CODEX_BIN"]
async fn the_real_app_server_reports_the_posture_eave_requires() {
    let bin = std::env::var("EAVE_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let cfg = SpawnConfig::new(
        bin,
        std::env::temp_dir(),
        format!("test-{}", uuid::Uuid::new_v4()),
    );
    let (mut session, mut rx, _guard) = spawn_codex(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let SessionEvent::Init {
        session_id,
        permission_mode,
        ..
    } = events.last().unwrap().clone()
    else {
        unreachable!()
    };
    assert_eq!(
        permission_mode.as_deref(),
        Some("untrusted/user/readOnly"),
        "the real app-server must echo back the strict posture"
    );
    assert!(!session_id.is_empty());
    assert!(!events
        .iter()
        .any(|e| matches!(e, SessionEvent::PostureRefused { .. })));
    session.kill().await.unwrap();
}
