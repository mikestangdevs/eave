//! engine-stub: a minimal one-session engine process for acceptance tests
//! that must `kill -9` the *engine* itself (spec 04: "kill -9 the engine
//! mid-turn", "kill the engine mid-journal-write").
//!
//! It does exactly what the real engine will do around one session: acquire
//! the session dir, write the journal header, spawn the agent via the real
//! `ClaudeCodeSession` path, save the meta breadcrumb, optionally send one
//! user message (journaled as `UserSent` at send time), and record events
//! until the terminal outcome. Tests kill it at chosen moments and assert
//! on what survived on disk.
//!
//! Configuration via env: `STUB_DATA_DIR`, `STUB_SESSION_ID`, `STUB_AGENT`
//! (path to the agent binary), `STUB_FAKE_MODE`, `STUB_MESSAGE` (optional).

use eave_core::claude::ClaudeCodeSession;
use eave_core::journal::{now_ms, Header, JournalEvent, JournalWriter, FORMAT_VERSION};
use eave_core::recorder::record_session;
use eave_core::session::{AgentSession, SpawnConfig};
use eave_core::store::{Meta, Store, META_FORMAT_VERSION};
use tokio::sync::mpsc;

fn env_required(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("engine-stub: {key} must be set"))
}

#[tokio::main]
async fn main() {
    // Lock-probe mode: attempt the engine instance lock and report via exit
    // code (0 = acquired, 42 = busy). Used by the two-instances acceptance
    // test, which must probe from a separate process.
    if std::env::var("STUB_TRY_LOCK").is_ok() {
        use eave_core::store::{EngineLock, LockError};
        match EngineLock::acquire(std::path::Path::new(&env_required("STUB_DATA_DIR"))) {
            Ok(_held) => std::process::exit(0),
            Err(LockError::Busy) => std::process::exit(42),
            Err(e) => {
                eprintln!("engine-stub lock probe: {e}");
                std::process::exit(1);
            }
        }
    }

    let data_dir = env_required("STUB_DATA_DIR");
    let session_id = env_required("STUB_SESSION_ID");
    let agent = env_required("STUB_AGENT");
    let fake_mode = std::env::var("STUB_FAKE_MODE").unwrap_or_else(|_| "normal".into());
    let message = std::env::var("STUB_MESSAGE").ok();

    let store = Store::open(&data_dir).expect("open store");
    let dir = store.create_session_dir(&session_id).expect("session dir");
    let cwd = std::env::temp_dir();

    let header = Header {
        format_version: FORMAT_VERSION,
        session_id: session_id.clone(),
        provider: "fake-agent".into(),
        cwd: cwd.display().to_string(),
        created_ts_ms: now_ms(),
    };
    let writer =
        JournalWriter::create(&Store::journal_path(&dir), &header).expect("create journal");

    let mut cfg = SpawnConfig::new(&agent, &cwd, session_id.clone());
    cfg.extra_env.push(("FAKE_MODE".into(), fake_mode));
    let (mut session, events) = ClaudeCodeSession::spawn(&cfg).expect("spawn agent");

    let meta = Meta {
        format_version: META_FORMAT_VERSION,
        provider: "fake-agent".into(),
        cwd: cwd.display().to_string(),
        session_id: session_id.clone(),
        state: "running".into(),
        child: Some(session.identity()),
    };
    meta.save(&dir).expect("save meta");

    let (extra_tx, extra_rx) = mpsc::channel(16);
    if let Some(text) = message {
        extra_tx
            .send(JournalEvent::UserSent { text: text.clone() })
            .await
            .expect("queue UserSent");
        // The fake agent may exit before reading stdin; the send itself is
        // already journaled, so a broken pipe here is not fatal.
        let _ = session.send_user_message(&text).await;
    }
    drop(extra_tx);

    let outcome = record_session(events, extra_rx, writer, dir, meta)
        .await
        .expect("record session");
    println!("engine-stub outcome: {outcome:?}");
}
