//! Live-CLI checks for the Grok adapter. **Every test here is `#[ignore]`d**
//! because each one spawns the real `grok` binary, which needs the
//! operator's xAI credentials and network access. Nothing in the default
//! suite depends on them.
//!
//! Run them deliberately:
//!
//! ```text
//! cargo test -p eave-core --test grok_live -- --ignored --nocapture
//! ```
//!
//! The binary is `~/.grok/bin/grok` unless `EAVE_GROK_BIN` says otherwise.
//!
//! # These tests read the machine's real posture, and that is the point
//!
//! [`real_grok_states_a_posture_that_eave_can_gate_on`] asserts only that
//! the session states a posture eave recognises — not which one — because
//! the answer depends on the operator's `~/.grok/config.toml`. On the
//! machine this slice was built on, `[ui] permission_mode =
//! "always-approve"` is set, so the honest live result there is
//! `PostureRefused`, and the test says so out loud rather than failing.
//!
//! To see the asking path live, point `GROK_HOME` at a directory whose
//! `config.toml` does NOT set `permission_mode` (copy `auth.json` and
//! `agent_id` into it), then run
//! [`real_grok_asks_before_it_writes`].

use std::path::PathBuf;
use std::time::Duration;

use eave_core::events::SessionEvent;
use eave_core::session::{AgentSession, ApprovalDecision, SpawnConfig};
use tokio::sync::mpsc::Receiver;

mod support;
use support::spawn_grok_agent;

/// Live turns hit a frontier model over the network; the fake-agent
/// timeout is far too tight for that.
const LIVE_TIMEOUT: Duration = Duration::from_secs(180);

fn grok_bin() -> PathBuf {
    match std::env::var("EAVE_GROK_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(std::env::var("HOME").expect("HOME"))
            .join(".grok")
            .join("bin")
            .join("grok"),
    }
}

fn live_cfg(cwd: &std::path::Path) -> SpawnConfig {
    SpawnConfig::new(grok_bin(), cwd, format!("live-{}", uuid::Uuid::new_v4()))
}

async fn drain_until(
    rx: &mut Receiver<SessionEvent>,
    pred: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    tokio::time::timeout(LIVE_TIMEOUT, async {
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
    .expect("live grok did not reach the expected event in time")
}

/// The gate, against the real CLI: a live session announces itself with a
/// posture eave recognises, and refuses itself when that posture is
/// auto-approve.
#[tokio::test]
#[ignore = "spawns the real grok CLI; needs xAI auth and network"]
async fn real_grok_states_a_posture_that_eave_can_gate_on() {
    let dir = tempdir();
    let cfg = live_cfg(&dir);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn real grok");

    session
        .send_user_message("Reply with just the word ok. Do not use any tools.", &[])
        .await
        .expect("send");
    let events = drain_until(&mut rx, |e| matches!(e, SessionEvent::Init { .. })).await;

    let mode = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Init {
                permission_mode, ..
            } => permission_mode.clone(),
            _ => None,
        })
        .expect("a live session must state a posture");
    println!("live grok posture: {mode}");
    assert!(
        mode == "default" || mode == "always-approve",
        "eave must recognise the posture it gates on, got {mode:?}"
    );
    if mode == "always-approve" {
        let refused = drain_until(&mut rx, |e| {
            matches!(e, SessionEvent::PostureRefused { .. })
        })
        .await;
        assert!(
            refused
                .iter()
                .any(|e| matches!(e, SessionEvent::PostureRefused { .. })),
            "an auto-approving live session must be refused"
        );
        println!("this machine's ~/.grok/config.toml disables asking; session refused (correct)");
    }
    session.kill().await.expect("kill");
}

/// The whole product promise, live: Grok asks eave before it writes, and a
/// denial provably stops the write.
///
/// Requires a Grok config that does NOT set `permission_mode`. See the
/// module docs for the `GROK_HOME` recipe; without it, on a machine
/// configured for always-approve, this test states that and stops.
#[tokio::test]
#[ignore = "spawns the real grok CLI; needs xAI auth, network, and an asking config"]
async fn real_grok_asks_before_it_writes() {
    let dir = tempdir();
    let cfg = live_cfg(&dir);
    let (mut session, mut rx) = spawn_grok_agent(&cfg).expect("spawn real grok");

    let target = dir.join("live-denied.txt");
    session
        .send_user_message(
            &format!(
                "Create a file at {} containing the word nope. Then stop.",
                target.display()
            ),
            &[],
        )
        .await
        .expect("send");

    let events = drain_until(&mut rx, |e| {
        matches!(
            e,
            SessionEvent::ApprovalRequested { .. } | SessionEvent::PostureRefused { .. }
        )
    })
    .await;

    if events
        .iter()
        .any(|e| matches!(e, SessionEvent::PostureRefused { .. }))
    {
        println!(
            "SKIPPED: this machine's grok config auto-approves, so it never asked. \
             Point GROK_HOME at a config without `permission_mode` to exercise this path."
        );
        session.kill().await.expect("kill");
        return;
    }

    let (request_id, path) = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ApprovalRequested {
                request_id, path, ..
            } => Some((request_id.clone(), path.clone())),
            _ => None,
        })
        .expect("grok must ask before writing");
    println!("live grok asked to write {path:?}");
    assert_eq!(
        path.as_deref(),
        Some(target.to_str().unwrap()),
        "the question must name the file it would actually touch"
    );

    assert!(session
        .respond_to_approval(
            &request_id,
            &ApprovalDecision::Deny {
                message: "no, leave the filesystem alone".into(),
            },
        )
        .await
        .expect("respond"));

    drain_until(&mut rx, |e| matches!(e, SessionEvent::TurnCompleted { .. })).await;
    // The proof that matters: a denial is not advisory.
    assert!(
        !target.exists(),
        "a denied write must not have happened: {} exists",
        target.display()
    );
    session.kill().await.expect("kill");
}

/// A scratch directory that is not the repo — a live agent is about to be
/// pointed at it.
fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eave-grok-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}
