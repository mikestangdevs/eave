//! Slice AW: the Hermes adapter, driven end to end over real ACP.
//!
//! Every test here spawns a real process and speaks real newline JSON-RPC
//! to it — `fake-hermes` implements the ACP server side, plus the
//! `config get` / `approvals test` subcommands the posture preflight shells
//! out to, so the adapter's own code path is what runs.
//!
//! **Nothing here touches the operator's Hermes, auth, or network.** The
//! live-CLI checks live in `hermes_live.rs` behind `#[ignore]`.
//!
//! Spawn through [`spawn_hermes`], not `HermesSession::spawn`: the handle
//! it returns reaps its process group on drop, which is the only reap that
//! survives a panicking test. `support/mod.rs` explains why that matters;
//! this file carries its own wrapper because that module's is typed to
//! `ClaudeCodeSession`.

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use eave_core::events::SessionEvent;
use eave_core::hermes::{preflight, ChildEnv, HermesSession, Preflight, APPROVAL_STALE_AFTER};
use eave_core::proc::{self, ProcIdentity};
use eave_core::process;
use eave_core::session::{AgentSession, ApprovalDecision, SessionError, SpawnConfig};
use eave_core::thresholds::{KILL_GRACE, KILL_VERIFY_WINDOW};
use tokio::sync::mpsc::Receiver;

mod support;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

fn fake_cfg(extra: &[(&str, &str)]) -> SpawnConfig {
    let mut cfg = SpawnConfig::new(
        env!("CARGO_BIN_EXE_fake-hermes"),
        std::env::temp_dir(),
        format!("test-{}", uuid::Uuid::new_v4()),
    );
    for (k, v) in extra {
        cfg.extra_env.push((k.to_string(), v.to_string()));
    }
    cfg
}

/// A Hermes session that proves its process group is gone before the test
/// that owns it finishes — whether it finishes by returning or panicking.
struct ReapedHermes(Option<HermesSession>);

impl Deref for ReapedHermes {
    type Target = HermesSession;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("session taken")
    }
}

impl DerefMut for ReapedHermes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("session taken")
    }
}

impl Drop for ReapedHermes {
    fn drop(&mut self) {
        let Some(session) = self.0.take() else {
            return;
        };
        let id = session.identity();
        if !reap_group_blocking(&id) {
            // Loud, but not a panic: a panicking Drop during an unwind
            // aborts the process and would replace the test's real failure
            // with noise.
            eprintln!(
                "eave test harness: could not prove process group {} gone; pid {} may be \
                 orphaned",
                id.pid, id.pid
            );
        }
    }
}

/// SIGTERM, then SIGKILL, each verified — the blocking twin of
/// `HermesSession::kill`, because `Drop` cannot await. No signal is ever
/// sent to a pid whose identity no longer matches: a dead leader's pgid can
/// be reused, and in a suite that runs ten tests at once "somebody else" is
/// another test's agent.
fn reap_group_blocking(id: &ProcIdentity) -> bool {
    if !proc::identity_matches(id) {
        return true;
    }
    let _ = process::killpg(id.pid, libc::SIGTERM);
    if wait_until_gone(id, KILL_GRACE) {
        return true;
    }
    if proc::identity_matches(id) {
        let _ = process::killpg(id.pid, libc::SIGKILL);
    }
    wait_until_gone(id, KILL_VERIFY_WINDOW)
}

fn wait_until_gone(id: &ProcIdentity, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        // Reap our own zombie: `#[tokio::test]` builds a current-thread
        // runtime and this loop is blocking it, so the supervisor task
        // that would normally `wait()` cannot run. A zombie is still a
        // member of its process group, so without this exit is never
        // provable.
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(id.pid, &mut status, libc::WNOHANG) };
        if !proc::identity_matches(id) && !process::group_alive(id.pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_hermes(cfg: &SpawnConfig) -> Result<(ReapedHermes, Receiver<SessionEvent>), SessionError> {
    support::arm_leak_guard();
    let (session, rx) = HermesSession::spawn(cfg)?;
    Ok((ReapedHermes(Some(session)), rx))
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
        panic!("channel closed before the expected event; saw: {events:?}");
    })
    .await
    .expect("expected event did not arrive in time")
}

fn is_init(e: &SessionEvent) -> bool {
    matches!(e, SessionEvent::Init { .. })
}

fn is_turn_done(e: &SessionEvent) -> bool {
    matches!(e, SessionEvent::TurnCompleted { .. })
}

// ---------------------------------------------------------------------------
// Handshake and identity

/// The full ACP opening: eave initializes, opens a session, verifies the
/// posture, and states the id Hermes minted.
///
/// The id is asserted to be **Hermes' own**, not the one eave passed in:
/// `session/new` takes a cwd and nothing else, so unlike Claude's
/// `--session-id` there is no caller-chosen cursor and the provider id is
/// discovered after the fact.
#[tokio::test]
async fn a_new_session_discovers_the_id_hermes_minted() {
    let cfg = fake_cfg(&[("FAKE_HERMES_SESSION_ID", "acp-minted-42")]);
    let (session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, is_init).await;

    let SessionEvent::Init {
        session_id,
        permission_mode,
        ..
    } = events.last().unwrap()
    else {
        unreachable!()
    };
    assert_eq!(session_id, "acp-minted-42");
    assert_ne!(
        *session_id, cfg.session_id,
        "ACP has no caller-chosen id; eave must not pretend its own was honoured"
    );
    assert_eq!(permission_mode.as_deref(), Some("default"));
    assert_eq!(
        session.provider_session_id().as_deref(),
        Some("acp-minted-42")
    );
}

/// A resume asks `session/load` for **Hermes'** id — the cursor Hermes
/// minted last time and eave journalled — not for eave's own.
///
/// The test above (`…must not pretend its own was honoured`) already
/// established that ACP has no caller-chosen id. This is the other half of
/// that fact: if eave's id cannot be imposed at `session/new`, it cannot
/// be used at `session/load` either. The cursor and `cfg.session_id` are
/// set to different values so that reading the wrong one fails.
#[tokio::test]
async fn a_resume_loads_the_providers_id_not_eaves() {
    let mut cfg = fake_cfg(&[]);
    let wanted = "acp-minted-42".to_string();
    cfg.session_id = "eave-side-id-that-hermes-never-issued".into();
    cfg.resume = Some(wanted.clone());
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, is_init).await;

    assert!(
        matches!(events.last().unwrap(), SessionEvent::Init { session_id, .. } if *session_id == wanted),
        "resume must carry eave's own cursor; saw {:?}",
        events.last()
    );
}

/// The child is its own process group leader (spec 01 §Spawn), which is
/// what makes a group kill safe.
#[tokio::test]
async fn the_child_leads_its_own_process_group() {
    let cfg = fake_cfg(&[]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;

    // `setsid` makes the child its own group leader, so its pgid IS its
    // pid — and `getpgid` reporting that is what makes a group kill hit
    // the agent's descendants and nothing of eave's.
    let pid = session.identity().pid;
    assert_eq!(
        unsafe { libc::getpgid(pid) },
        pid,
        "a child that shares eave's process group cannot be group-killed safely"
    );
    assert_ne!(
        pid,
        unsafe { libc::getpgid(0) },
        "and it must not be eave's own group"
    );
    let report = session.kill().await.unwrap();
    assert!(!report.already_exited);
}

// ---------------------------------------------------------------------------
// Turns, text and tools

#[tokio::test]
async fn a_turn_streams_text_and_completes() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "normal")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("hi").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TextDelta { text } if text.contains("fake hermes"))),
        "the deltas must reach the stream: {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, SessionEvent::AssistantText { text } if text.contains("fake hermes"))
        ),
        "the accumulated turn text must be snapshotted once: {events:?}"
    );
    assert!(
        matches!(
            events.last().unwrap(),
            SessionEvent::TurnCompleted { subtype, is_error: false } if subtype == "end_turn"
        ),
        "{events:?}"
    );
}

/// A tool call that Hermes starts and never finishes is in flight, and
/// stays there.
///
/// This is a separate test from the round trip below because it is the
/// only place the count can be sampled honestly. The adapter's parser
/// updates its state as it reads each line, which is strictly ahead of the
/// test draining the event channel — so in a mode where the completion
/// follows the start immediately, a "count is 1 here" assertion is a race
/// that passes on timing rather than on behaviour. Sampling only at rest
/// would pass against an implementation that never counted at all, so
/// neither check is dropped; they are split so that both can be true.
#[tokio::test]
async fn a_tool_call_that_never_finishes_stays_in_flight() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "tool-pending")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("build it").await.unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseStarted { .. })
    })
    .await;

    assert_eq!(session.in_flight_tools(), 1);
    // Nothing can resolve it: the fake sends no completion and no turn
    // reply, so this is a fact about the count and not about the clock.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(session.in_flight_tools(), 1);
    session.kill().await.unwrap();
}

/// A tool call round trip: started with the ACP kind as its name and the
/// title preserved, resolved with Hermes' own verdict, and nothing left in
/// flight afterwards.
#[tokio::test]
async fn a_tool_call_round_trip_reports_both_ends() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "tools")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("build it").await.unwrap();

    let started = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseStarted { .. })
    })
    .await;
    assert!(
        matches!(
            started.last().unwrap(),
            SessionEvent::ToolUseStarted { id, name, input, .. }
                if id == "call-1" && name == "execute" && input.contains("cargo test")
        ),
        "the ACP kind is the name and the title survives in the input: {:?}",
        started.last()
    );
    // Hermes sends no `rawInput` for `terminal`, so the typed call falls
    // through to its own one-line summary — VERBATIM. §3.3's rule that
    // eave never invents a tool NAME by splitting that string stands: this
    // does not split it. A row that says `execute` and nothing else is the
    // readability regression that rule accepted; this pays it back without
    // guessing anything.
    assert!(
        matches!(
            started.last().unwrap(),
            SessionEvent::ToolUseStarted { call: Some(eave_core::tool::ToolCall::Run { command }), .. }
                if command == "terminal: cargo test"
        ),
        "expected Hermes' own summary, uncut: {:?}",
        started.last()
    );
    let done = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ToolUseFinished { .. })
    })
    .await;
    assert!(
        matches!(
            done.last().unwrap(),
            SessionEvent::ToolUseFinished { id, result, is_error: false }
                if id == "call-1" && result.contains("42 passed")
        ),
        "{:?}",
        done.last()
    );
    recv_until(&mut rx, is_turn_done).await;
    assert_eq!(session.in_flight_tools(), 0);
}

/// A tool Hermes starts and never reports on does not stay in flight
/// forever. The turn ending IS the fact, and it is stated as one rather
/// than left as a phantom the attention clocks would read as work.
#[tokio::test]
async fn a_tool_left_open_at_turn_end_is_closed_with_the_reason() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "orphan-tool")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("go").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ToolUseFinished { id, result, is_error: true }
                if id == "call-orphan" && result.contains("turn ended")
        )),
        "an unresolved tool must be closed with a stated reason: {events:?}"
    );
    assert_eq!(session.in_flight_tools(), 0);
}

/// A line that is not JSON, and one that is JSON but not a JSON-RPC
/// message, are both surfaced and neither is fatal.
#[tokio::test]
async fn malformed_input_is_surfaced_and_the_session_keeps_going() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "malformed")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("go").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;

    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::Malformed { .. }))
            .count(),
        2,
        "both the unparseable line and the untagged one: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TextDelta { text } if text.contains("still alive"))),
        "the session must survive both: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Approvals — the reason this adapter speaks ACP

/// The whole permission round trip: Hermes asks, eave surfaces a question
/// a human could actually answer, the decision reaches Hermes, and Hermes
/// says which option it received.
///
/// The assertion is on what the AGENT reports it got, not on what eave
/// wrote — a test that only checked eave's own frame would pass against an
/// adapter whose reply Hermes ignored.
#[tokio::test]
async fn an_allow_reaches_hermes_and_hermes_says_which_option_it_got() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "approval")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("clean up").await.unwrap();

    let asked = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested {
        request_id,
        tool,
        display_name,
        description,
        input_json,
        suggestions_json,
        path,
    } = asked.last().unwrap()
    else {
        unreachable!()
    };
    assert_eq!(tool, "execute");
    assert!(display_name.contains("rm -rf ./build"), "{display_name}");
    assert_eq!(description, "Run a dangerous command");
    assert!(
        input_json.contains("rm -rf ./build"),
        "the operator must see what they are approving: {input_json}"
    );
    assert!(
        suggestions_json
            .as_deref()
            .is_some_and(|s| s.contains("allow_always")),
        "Hermes' own grant offers are journaled as a fact: {suggestions_json:?}"
    );
    assert_eq!(*path, None, "a shell command names no path");
    assert_eq!(session.pending_approvals(), 1);

    assert!(session
        .respond_to_approval(request_id, &ApprovalDecision::Allow)
        .await
        .unwrap());
    assert_eq!(session.pending_approvals(), 0);

    let events = recv_until(&mut rx, is_turn_done).await;
    assert!(
        events.iter().any(
            |e| matches!(e, SessionEvent::TextDelta { text } if text == "decision=allow_once")
        ),
        "Hermes must report the option eave selected: {events:?}"
    );
}

/// A deny is delivered as Hermes' own `deny` option, not as an ACP
/// "cancelled" outcome. The two both read as a refusal on Hermes' side,
/// but only one of them means the operator answered.
#[tokio::test]
async fn a_deny_is_delivered_as_a_chosen_option() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "approval")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("clean up").await.unwrap();
    let asked = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested { request_id, .. } = asked.last().unwrap() else {
        unreachable!()
    };

    assert!(session
        .respond_to_approval(
            request_id,
            &ApprovalDecision::Deny {
                message: "commit first".into(),
            },
        )
        .await
        .unwrap());

    let events = recv_until(&mut rx, is_turn_done).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TextDelta { text } if text == "decision=deny")),
        "{events:?}"
    );
}

/// Answering a question that is not open is a race, stated as `Ok(false)`
/// — never an error, and never written to the pipe, because Hermes would
/// ignore it while eave journaled it as if it had landed.
#[tokio::test]
async fn a_decision_for_a_question_that_is_not_open_is_a_race_not_an_error() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "approval")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("clean up").await.unwrap();
    let asked = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested { request_id, .. } = asked.last().unwrap() else {
        unreachable!()
    };

    assert!(session
        .respond_to_approval(request_id, &ApprovalDecision::Allow)
        .await
        .unwrap());
    assert!(
        !session
            .respond_to_approval(request_id, &ApprovalDecision::Allow)
            .await
            .unwrap(),
        "the second answer to the same question must not reach the pipe"
    );
    assert!(!session
        .respond_to_approval("never-asked", &ApprovalDecision::Allow)
        .await
        .unwrap());
}

/// When Hermes offers an option set this adapter cannot answer from, eave
/// refuses to guess. Picking a plausible-looking id would be answering a
/// question eave did not understand, on the operator's behalf.
#[tokio::test]
async fn an_unanswerable_option_set_is_refused_rather_than_guessed() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "approval-odd-options")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("go").await.unwrap();
    let asked = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::ApprovalRequested { .. })
    })
    .await;
    let SessionEvent::ApprovalRequested { request_id, .. } = asked.last().unwrap() else {
        unreachable!()
    };

    let err = session
        .respond_to_approval(request_id, &ApprovalDecision::Allow)
        .await
        .expect_err("eave must not invent an option id");
    assert!(
        err.to_string().contains("without guessing"),
        "the refusal must say why: {err}"
    );
    assert_eq!(
        session.pending_approvals(),
        1,
        "an undelivered decision leaves the question open, not dead"
    );
}

/// eave declined every ACP client capability at `initialize`, so a request
/// for one is answered immediately with a stated refusal — silence would
/// park the turn on a request nobody can see.
#[tokio::test]
async fn a_client_method_eave_does_not_serve_is_refused_immediately() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "client-request")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("read a file").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::DialogRefused { dialog, .. } if dialog == "fs/read_text_file"
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TextDelta { text } if text == "asked anyway")),
        "the turn must continue rather than park: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Posture

/// A session mode that auto-allows edits is refused at the first frame,
/// and the refusal names the mode.
#[tokio::test]
async fn a_mode_that_never_asks_is_refused() {
    for mode in ["accept_edits", "dont_ask"] {
        let cfg = fake_cfg(&[("FAKE_HERMES_SESSION_MODE", mode)]);
        let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
        let events = recv_until(&mut rx, |e| {
            matches!(e, SessionEvent::PostureRefused { .. })
        })
        .await;
        assert!(
            matches!(
                events.last().unwrap(),
                SessionEvent::PostureRefused { mode: m, reason } if m == mode && reason.contains("auto-allows")
            ),
            "{:?}",
            events.last()
        );
    }
}

/// A mode this build has never heard of is refused too. Fail-closed by
/// construction: "does this posture review writes?" has no safe default,
/// and a wrong guess is unreviewed writes on someone's machine.
#[tokio::test]
async fn an_unknown_mode_is_refused_rather_than_assumed_safe() {
    let cfg = fake_cfg(&[("FAKE_HERMES_SESSION_MODE", "turbo_2027")]);
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::PostureRefused { mode, .. } if mode == "turbo_2027"
    ));
}

/// A session that states no mode at all is refused, and the refusal says
/// that the absence — not a bad value — is the problem.
#[tokio::test]
async fn a_session_that_states_no_mode_is_refused() {
    let cfg = fake_cfg(&[("FAKE_HERMES_SESSION_MODE", "unstated")]);
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::PostureRefused { mode, .. } if mode == "unstated"
    ));
}

/// A refused session cannot be prompted, and the error carries the reason
/// rather than a bare protocol failure.
///
/// The engine already kills a session on `PostureRefused`; this is the
/// second lock on the same door, so a refused session that somehow kept
/// running still could not write.
#[tokio::test]
async fn a_refused_session_will_not_accept_a_prompt() {
    let cfg = fake_cfg(&[("FAKE_HERMES_SESSION_MODE", "dont_ask")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;

    let err = session
        .send_user_message("write a file")
        .await
        .expect_err("a refused session must not be promptable");
    assert!(
        err.to_string().contains("auto-allows"),
        "the refusal reason must survive to the caller: {err}"
    );
}

/// A Hermes that dies before it can shake hands fails the next send
/// instead of hanging on it.
///
/// The `watch` that carries handshake state is owned by the session, so a
/// dead child never closes it: without an explicit terminal state here,
/// `send_user_message` would wait for the lifetime of the process. A send
/// that blocks forever is worse than one that fails.
#[tokio::test]
async fn a_hermes_that_dies_before_the_handshake_fails_the_next_send() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "die-at-start")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, |e| matches!(e, SessionEvent::Exited { .. })).await;

    let err = tokio::time::timeout(TEST_TIMEOUT, session.send_user_message("hello"))
        .await
        .expect("send must not hang on a session that will never open")
        .expect_err("and it must not silently succeed either");
    assert!(
        err.to_string().contains("ended before it could open"),
        "the failure must say what happened: {err}"
    );
}

/// A posture that changes under eave's feet mid-session is caught. eave
/// never sends `session/set_mode`, so this should be unreachable — which
/// is exactly why it is guarded rather than trusted.
#[tokio::test]
async fn a_mode_change_mid_session_is_refused() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "mode-flip")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("go").await.unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::PostureRefused { mode, .. } if mode == "dont_ask"
    ));
}

/// The command half of the posture proof: when Hermes' own evaluator says
/// it would run a known-dangerous command without asking, eave refuses the
/// session and names the verdict it got.
///
/// This is the ONLY check that can see the bypasses that live in the
/// operator's `config.yaml` and environment (`approvals.mode: off`,
/// `command_allowlist`, `HERMES_YOLO_MODE`), because none of them appear
/// on the wire.
#[tokio::test]
async fn a_bypassed_command_gate_refuses_the_session() {
    let cfg = fake_cfg(&[("FAKE_HERMES_VERDICT", "allow")]);
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(
        matches!(
            events.last().unwrap(),
            SessionEvent::PostureRefused { mode, reason }
                if mode == "default" && reason.contains("without asking") && reason.contains("bypass")
        ),
        "a wire-clean session with a bypassed command gate must still be refused: {:?}",
        events.last()
    );
}

/// An evaluator that answers something other than a verdict is not a pass.
/// A posture that cannot be checked is not a posture eave runs under.
#[tokio::test]
async fn an_unreadable_preflight_answer_refuses_the_session() {
    let cfg = fake_cfg(&[("FAKE_HERMES_VERDICT", "no-json")]);
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(
        matches!(
            events.last().unwrap(),
            SessionEvent::PostureRefused { reason, .. } if reason.contains("no verdict")
        ),
        "{:?}",
        events.last()
    );
}

/// `preflight` reports the backend it evaluated against, because an
/// isolating terminal backend makes the runtime skip every command guard —
/// a session that looks armed on the wire and is not.
#[tokio::test]
async fn the_preflight_evaluates_against_the_configured_terminal_backend() {
    let program = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-hermes"));
    let cwd = std::env::temp_dir();

    // Backend unset: the probe still runs and the gate is armed.
    let armed = ChildEnv::for_session(&[("FAKE_HERMES_VERDICT".into(), "ask-approval".into())]);
    assert_eq!(preflight(&program, &cwd, &armed).await, Preflight::Asks);

    // Backend named, and the evaluator answers `allow` for it — the shape
    // a container backend produces, where the runtime skips every guard.
    let containerised = ChildEnv::for_session(&[
        ("FAKE_HERMES_ENV_TYPE".into(), "docker".into()),
        ("FAKE_HERMES_VERDICT".into(), "allow".into()),
    ]);
    let Preflight::Refused(reason) = preflight(&program, &cwd, &containerised).await else {
        panic!("an evaluator answering `allow` must refuse");
    };
    assert!(
        reason.contains("docker"),
        "the refusal must name the backend it evaluated: {reason}"
    );
}

/// The probe runs in the SAME environment as the session it vouches for.
///
/// This is not a detail. `HERMES_HOME` alone points the two at different
/// `config.yaml` files, and a probe reading the wrong one answers honestly
/// about a machine nobody is running on. The check is that a variable
/// supplied only through `SpawnConfig::extra_env` reaches the probe
/// subprocess — which it can only do if both sides share one overlay.
#[tokio::test]
async fn the_probe_and_the_session_see_the_same_environment() {
    let cfg = fake_cfg(&[("FAKE_HERMES_VERDICT", "hardline-deny")]);
    let (_session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(e, SessionEvent::PostureRefused { .. })
    })
    .await;
    assert!(
        matches!(
            events.last().unwrap(),
            SessionEvent::PostureRefused { reason, .. } if reason.contains("hardline-deny")
        ),
        "a verdict reachable only through extra_env must have reached the probe: {:?}",
        events.last()
    );
}

/// eave's withdrawal window rounds DOWN from Hermes' own 60s timeout.
///
/// Asserted against the cited upstream number rather than against eave's
/// own margin constant, because a test that reads the constant it is
/// testing would pass no matter what the margin became — including a
/// margin that rounded the wrong way and let eave journal a decision
/// Hermes had already discarded.
#[test]
fn the_withdrawal_window_closes_before_hermes_stops_waiting() {
    let hermes_timeout = Duration::from_secs(60);
    assert!(
        APPROVAL_STALE_AFTER < hermes_timeout,
        "eave must give up before Hermes does, or it will journal decisions that never landed"
    );
    assert!(
        APPROVAL_STALE_AFTER > hermes_timeout / 2,
        "and not so early that an operator loses answers they were entitled to give"
    );
}

// ---------------------------------------------------------------------------
// Kill

/// A kill proves exit rather than reporting it on hope, and `Exited` is
/// the final event on the channel.
#[tokio::test]
async fn kill_proves_exit_and_exited_is_the_last_event() {
    let cfg = fake_cfg(&[]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;

    let id = session.identity();
    let report = session.kill().await.unwrap();
    assert!(!report.already_exited);
    assert!(!proc::identity_matches(&id));
    assert!(!process::group_alive(id.pid));

    let events = recv_until(&mut rx, |e| matches!(e, SessionEvent::Exited { .. })).await;
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Exited { .. }
    ));
    assert!(
        rx.recv().await.is_none(),
        "Exited is always the final event"
    );
}

// ---------------------------------------------------------------------------
// Live CLI
//
// These drive the operator's REAL `hermes` binary. They are `#[ignore]`d
// because a suite that needs a specific vendor CLI installed is a suite
// that fails on every machine that does not have one.
//
//     cargo test -p eave-core --test hermes_lifecycle -- --ignored \
//         --test-threads=1
//
// Neither one sends a prompt, so neither needs the operator's credentials
// or the network: `initialize` and `session/new` are answered locally, and
// `approvals test` is a dry-run evaluator that by its own `--help` never
// executes the command, prompts anyone, or persists anything.

fn real_hermes() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    std::path::PathBuf::from(home).join(".local/bin/hermes")
}

/// The real `hermes acp` completes eave's handshake and reports the
/// asking posture — the claim the whole adapter rests on.
#[tokio::test]
#[ignore = "drives the operator's real hermes binary"]
async fn live_hermes_acp_opens_a_session_in_the_asking_mode() {
    let mut cfg = SpawnConfig::new(
        real_hermes(),
        std::env::temp_dir(),
        format!("live-{}", uuid::Uuid::new_v4()),
    );
    cfg.resume = None;
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    let events = recv_until(&mut rx, |e| {
        matches!(
            e,
            SessionEvent::Init { .. } | SessionEvent::PostureRefused { .. }
        )
    })
    .await;

    match events.last().unwrap() {
        SessionEvent::Init {
            session_id,
            permission_mode,
            ..
        } => {
            assert_eq!(
                permission_mode.as_deref(),
                Some("default"),
                "Hermes' own \"Ask before edits\" mode"
            );
            assert!(!session_id.is_empty());
        }
        SessionEvent::PostureRefused { reason, .. } => panic!(
            "this machine's Hermes would not ask before it acts: {reason}\n\
             That is a real finding about the machine, not a broken test."
        ),
        _ => unreachable!(),
    }
    session.kill().await.unwrap();
}

/// The real `hermes approvals test` still answers `ask-approval` for both
/// canaries, which is the command half of the posture proof.
#[tokio::test]
#[ignore = "drives the operator's real hermes binary"]
async fn live_hermes_still_asks_before_a_dangerous_command() {
    let verdict = preflight(
        &real_hermes(),
        &std::env::temp_dir(),
        &ChildEnv::for_session(&[]),
    )
    .await;
    assert_eq!(
        verdict,
        Preflight::Asks,
        "this machine's Hermes has an approval bypass active"
    );
}

// ---------------------------------------------------------------------------
// Token accounting (Slice CC)

/// **Slice CC.** Hermes states its two token facts on two different
/// channels, and eave keeps them apart all the way to the event channel:
/// the `usage_update` gauge is a level with no bill on it, and the settled
/// prompt response is a bill with no level on it.
///
/// The fixture's numbers are its own; its shapes are the ACP schema's
/// (`acp/schema.py`, `_UsageUpdate` and `Usage` — the package installed
/// alongside the operator's real Hermes). Nothing here reaches Hermes,
/// the network, or any credential.
#[tokio::test]
async fn the_two_token_facts_arrive_separately_and_stay_separate() {
    use eave_core::events::{ContextWindow, TurnTokens};

    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "usage")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("count").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;

    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::TokenUsage { turn, context } => Some((*turn, *context)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2, "one gauge, one bill: {events:?}");

    // The gauge, mid-turn: a level and nothing else.
    assert_eq!(
        usage[0],
        (
            None,
            Some(ContextWindow {
                used_tokens: 21_884,
                limit_tokens: Some(200_000),
            })
        ),
        "a usage_update states occupancy and must never be read as a bill"
    );

    // The bill, on the settled response: tokens and nothing else.
    assert_eq!(
        usage[1],
        (
            TurnTokens::stated(Some(21_884), Some(455), Some(19_000), Some(0)),
            None
        ),
        "the prompt response states the turn and must never be read as occupancy"
    );

    // And it lands before the bookend, so a reader folding events in order
    // has the turn's cost by the time the turn ends.
    let bill_ix = events
        .iter()
        .rposition(|e| matches!(e, SessionEvent::TokenUsage { turn: Some(_), .. }))
        .unwrap();
    let done_ix = events.iter().rposition(is_turn_done).unwrap();
    assert!(bill_ix < done_ix, "{events:?}");
}

/// A turn that says nothing about tokens emits no token record at all.
/// A zeroed one would read as a free turn, which is a claim about the run
/// rather than about the report.
#[tokio::test]
async fn a_turn_with_no_usage_on_the_wire_reports_no_tokens() {
    let cfg = fake_cfg(&[("FAKE_HERMES_MODE", "normal")]);
    let (mut session, mut rx) = spawn_hermes(&cfg).unwrap();
    recv_until(&mut rx, is_init).await;
    session.send_user_message("hi").await.unwrap();
    let events = recv_until(&mut rx, is_turn_done).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TokenUsage { .. })),
        "silence is the honest answer: {events:?}"
    );
}
