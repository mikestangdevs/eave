//! Slice AV integration tests: the Grok adapter's real spawn path, ACP
//! handshake, posture gate, approval round trip, and verified kill.
//!
//! Every test here drives `GrokSession::spawn` against the fake-agent
//! harness in its `acp-*` modes — a real process, real pipes, real
//! JSON-RPC — so what is exercised is the adapter, not a parser in
//! isolation (`grok.rs`'s own unit tests cover the wire shapes, against
//! frames captured from the real CLI).
//!
//! Spawn through [`support::spawn_grok_agent`], not `GrokSession::spawn`:
//! the handle it returns reaps its process group on drop, which is the
//! only reap that survives a panicking test. `support/mod.rs` says why
//! that matters.
//!
//! Nothing here touches the operator's Grok auth or the network. The
//! live-CLI checks that DO are in `grok_live.rs`, all `#[ignore]`d.

use std::time::Duration;

use eave_core::events::SessionEvent;
use eave_core::session::{AgentSession, ApprovalDecision, SpawnConfig};
use tokio::sync::mpsc::Receiver;

mod support;
use support::spawn_grok_agent;

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

/// Drain events until `pred` matches, returning everything seen. Fails the
/// test rather than hanging if the stream ends or the window expires.
async fn drain_until(
    rx: &mut Receiver<SessionEvent>,
    pred: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let done = pred(&event) || matches!(event, SessionEvent::Exited { .. });
            seen.push(event);
            if done {
                return seen;
            }
        }
        seen
    })
    .await
    .expect("event stream did not reach the expected event in time")
}

fn text_of(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The happy path: the handshake completes, the session names itself, its
/// posture is announced as one that asks, and a reply streams back.
#[tokio::test]
async fn a_grok_session_announces_itself_and_streams_a_reply() {
    let cfg = fake_cfg("acp-asks", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");

    session.send_user_message("ping").await.expect("send");
    let events = drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;

    let init = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Init {
                session_id,
                permission_mode,
                ..
            } => Some((session_id.clone(), permission_mode.clone())),
            _ => None,
        })
        .expect("the session must announce itself");
    // The id is the one GROK issued, not the one eave was configured with:
    // ACP has no way to propose an id at session/new.
    assert_eq!(init.0, "01a0fake-0000-7000-8000-000000000001");
    assert_ne!(init.0, cfg.session_id);
    assert_eq!(init.1.as_deref(), Some("default"));
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some("01a0fake-0000-7000-8000-000000000001")
    );

    assert!(text_of(&events).contains("heard: ping"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::PostureRefused { .. })),
        "a session that asks must not be refused: {events:?}"
    );
    session.kill().await.expect("kill");
}

/// The gate that makes this adapter shippable. An operator whose
/// `~/.grok/config.toml` says `permission_mode = "always-approve"` gets a
/// session that never asks, and eave must refuse it rather than supervise
/// writes it can never review.
#[tokio::test]
async fn a_session_that_auto_approves_is_refused_with_a_reason() {
    let cfg = fake_cfg("acp-yolo", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");

    session.send_user_message("go").await.expect("send");
    let events = drain_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;

    let refusal = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::PostureRefused { mode, reason } => Some((mode.clone(), reason.clone())),
            _ => None,
        })
        .expect("an auto-approving session must be refused");
    assert_eq!(refusal.0, "always-approve");
    assert!(
        refusal.1.contains("config.toml"),
        "the refusal must tell the operator how to fix it: {}",
        refusal.1
    );
    session.kill().await.expect("kill");
}

/// Fail-closed. A session that runs a tool without ever having said
/// whether it asks is refused at that tool — silence is not consent.
#[tokio::test]
async fn a_tool_that_runs_without_a_stated_posture_is_refused() {
    let cfg = fake_cfg("acp-no-posture", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");

    session.send_user_message("go").await.expect("send");
    let events = drain_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::PostureRefused { mode, .. } if mode == "unstated"
        )),
        "an unproven posture must be refused: {events:?}"
    );
    session.kill().await.expect("kill");
}

/// The whole point of the slice: eave is asked, the operator decides, and
/// the decision provably reaches the agent.
#[tokio::test]
async fn an_allow_reaches_the_agent_as_the_once_option() {
    let cfg = fake_cfg("acp-approval", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");
    session.send_user_message("write it").await.expect("send");

    let events = drain_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let request_id = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the agent must ask");
    assert_eq!(session.pending_approvals(), 1);

    assert!(session
        .respond_to_approval(&request_id, &ApprovalDecision::Allow)
        .await
        .expect("respond"));
    assert_eq!(session.pending_approvals(), 0);

    let after = drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;
    // The agent reports the option id it actually received — proof the
    // answer travelled, and proof it was the single-use one rather than
    // the session-wide grant eave never offers.
    assert!(
        text_of(&after).contains("DECISION:allow-once"),
        "expected the agent to report allow-once, got {after:?}"
    );
    session.kill().await.expect("kill");
}

/// A denial stops the tool AND carries the operator's sentence. ACP has no
/// field for that sentence, and a denied turn ends immediately, so the
/// adapter sends it as the following prompt — Grok's own label for the
/// option is "No, and tell Grok what to do differently".
#[tokio::test]
async fn a_denial_stops_the_tool_and_its_message_follows_as_a_prompt() {
    let cfg = fake_cfg("acp-approval", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");
    session.send_user_message("write it").await.expect("send");

    let events = drain_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let request_id = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the agent must ask");

    assert!(session
        .respond_to_approval(
            &request_id,
            &ApprovalDecision::Deny {
                message: "denied — commit first".into(),
            },
        )
        .await
        .expect("respond"));

    let after = drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;
    assert!(
        text_of(&after).contains("DECISION:reject-once"),
        "the denial must reach the agent: {after:?}"
    );
    // …and the sentence behind it must reach the model too, or "denied —
    // commit first" is no different from a bare "denied".
    let followup = drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;
    assert!(
        text_of(&followup).contains("FOLLOWUP:denied — commit first"),
        "the deny message must reach the model: {followup:?}"
    );
    session.kill().await.expect("kill");
}

/// A question answered twice is a race — the operator clicked as the
/// provider withdrew it, or two views raced. It is a fact, not an error,
/// and the second answer must never reach the pipe: ACP would reject a
/// duplicate response id and eave would have journaled a decision that
/// never landed.
#[tokio::test]
async fn answering_the_same_question_twice_is_a_race_not_an_error() {
    let cfg = fake_cfg("acp-approval", &[]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");
    session.send_user_message("write it").await.expect("send");

    let events = drain_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let request_id = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ApprovalRequested { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .expect("the agent must ask");

    assert!(session
        .respond_to_approval(&request_id, &ApprovalDecision::Allow)
        .await
        .expect("first answer lands"));
    assert!(
        !session
            .respond_to_approval(&request_id, &ApprovalDecision::Allow)
            .await
            .expect("a stale answer is not an error"),
        "the second answer must be reported as not-open"
    );
    // An id nobody ever asked about is the same kind of non-answer.
    assert!(!session
        .respond_to_approval("call-never-existed", &ApprovalDecision::Allow)
        .await
        .expect("unknown id is not an error"));
    session.kill().await.expect("kill");
}

/// Resume goes out as `session/load` naming the id eave was given, and the
/// session reports that id as its own.
#[tokio::test]
async fn a_resumed_session_reports_the_id_it_was_asked_to_load() {
    let mut cfg = fake_cfg("acp-asks", &[]);
    cfg.session_id = "01a020c7-d363-7a50-9f08-37e0cc12db3a".into();
    cfg.resume = true;
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");

    session
        .send_user_message("still there?")
        .await
        .expect("send");
    drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some("01a020c7-d363-7a50-9f08-37e0cc12db3a"),
        "a loaded session keeps the id it was loaded from"
    );
    session.kill().await.expect("kill");
}

/// The kill contract, on the Grok spawn path. The adapter passes
/// `--no-leader` precisely so the work stays in this group; a group that
/// shrugs off SIGTERM is escalated and verified empty.
#[tokio::test]
async fn a_stubborn_grok_group_is_escalated_and_leaves_no_survivors() {
    let cfg = fake_cfg("hang-ignore-term", &[("FAKE_SPAWN_CHILD", "1")]);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn");
    // Wait for the child's first line before killing. The harness installs
    // its SIGTERM handler and spawns the stubborn grandchild BEFORE it
    // writes anything, so one line proves the group has two
    // SIGTERM-ignoring members — killing sooner would race that setup and
    // measure nothing. (This mode speaks Claude's stream-JSON, so to the
    // Grok parser that first line is a `Malformed`, which is exactly the
    // synchronisation point the Claude test gets from `Init`.)
    drain_until(&mut rx, |_| true).await;
    let pgid = session.identity().pid;

    let report = session.kill().await.expect("kill must be provable");
    assert!(report.escalated, "SIGTERM was ignored, so it must escalate");
    assert!(!report.already_exited);
    assert!(
        !eave_core::process::group_alive(pgid),
        "the whole process group must be gone"
    );
    // Independent verification: no process anywhere still claims this pgid.
    let survivors = eave_core::process::scan_group_pids(pgid).await.unwrap();
    assert!(survivors.is_empty(), "surviving pids: {survivors:?}");
}

/// A session killed before it ever finished its handshake still dies, and
/// a caller waiting on that handshake is told rather than left hanging.
#[tokio::test]
async fn a_send_on_a_dead_session_reports_instead_of_hanging() {
    let cfg = fake_cfg("hang-ignore-term", &[]);
    let (mut session, _rx) = spawn_grok_agent(&cfg).expect("spawn");
    session.kill().await.expect("kill");

    let err = tokio::time::timeout(TEST_TIMEOUT, session.send_user_message("anyone there?"))
        .await
        .expect("send must not hang once the agent is gone");
    assert!(
        err.is_err(),
        "sending into a dead session must fail loudly, got {err:?}"
    );
}
