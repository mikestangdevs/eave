//! Hand-run verification against the real `claude` CLI (not part of the
//! automated suite — run with `cargo run --example manual_smoke`).
//!
//! Phase 1: spawn a session under a caller-chosen `--session-id`, plant a
//! codeword, watch the parsed event stream live, then kill with
//! proof-of-exit and check the identity guard.
//!
//! Phase 2: spawn a *new* process with `--resume <same id>` and ask for the
//! codeword back — proving the caller-chosen id is a working cold-resume
//! cursor (load-bearing for Stage B, spec 02 §Resume contract).

use std::time::Instant;

use eave_core::claude::ClaudeCodeSession;
use eave_core::events::SessionEvent;
use eave_core::proc;
use eave_core::process;
use eave_core::session::{AgentSession, SpawnConfig};

const CODEWORD: &str = "BLUEHERON42";

fn show(t0: Instant, event: &SessionEvent) {
    println!("[{:7.3}s] {:?}", t0.elapsed().as_secs_f64(), event);
}

#[tokio::main]
async fn main() {
    let session_id = uuid::Uuid::new_v4().to_string();
    let cwd = std::env::temp_dir();
    println!(
        "chosen session id: {session_id}\n=== phase 1: plant codeword, watch stream, kill ==="
    );

    let cfg = SpawnConfig::new("claude", &cwd, session_id.clone());
    let (mut session, mut rx) = ClaudeCodeSession::spawn(&cfg).expect("spawn");
    let t0 = Instant::now();
    println!("spawned: identity {:?}", session.identity());

    session
        .send_user_message(&format!(
            "Remember the codeword: {CODEWORD}. Reply with exactly: noted"
        ))
        .await
        .expect("send");
    println!("[{:7.3}s] >> user message sent", t0.elapsed().as_secs_f64());

    while let Some(event) = rx.recv().await {
        show(t0, &event);
        if matches!(event, SessionEvent::TurnCompleted { .. }) {
            break;
        }
    }
    println!(
        "provider session id per stream: {:?} (matches chosen: {})",
        session.provider_session_id(),
        session.provider_session_id().as_deref() == Some(session_id.as_str())
    );

    let id = session.identity();
    println!(
        "identity_matches before kill: {}",
        proc::identity_matches(&id)
    );
    let report = session.kill().await.expect("kill must be provable");
    println!("kill report: {report:?}");
    println!("group_alive after kill: {}", process::group_alive(id.pid));
    println!(
        "identity_matches after kill: {}",
        proc::identity_matches(&id)
    );
    while let Some(event) = rx.recv().await {
        show(t0, &event);
    }

    println!("\n=== phase 2: cold resume via --resume {session_id} ===");
    let mut cfg2 = SpawnConfig::new("claude", &cwd, session_id.clone());
    cfg2.resume = Some(session_id.clone());
    let (mut session2, mut rx2) = ClaudeCodeSession::spawn(&cfg2).expect("spawn resume");
    let t1 = Instant::now();
    session2
        .send_user_message("What is the codeword? Reply with the codeword only.")
        .await
        .expect("send");

    let mut recalled = false;
    while let Some(event) = rx2.recv().await {
        show(t1, &event);
        if let SessionEvent::AssistantText { text } | SessionEvent::TextDelta { text } = &event {
            if text.contains(CODEWORD) {
                recalled = true;
            }
        }
        if matches!(event, SessionEvent::TurnCompleted { .. }) {
            break;
        }
    }
    let report2 = session2.kill().await.expect("kill 2 must be provable");
    println!("kill report 2: {report2:?}");

    println!(
        "\nRESULT: cold resume {} — codeword {}",
        if recalled { "WORKS" } else { "FAILED" },
        if recalled { "recalled" } else { "not recalled" },
    );
    if !recalled {
        std::process::exit(1);
    }
}
