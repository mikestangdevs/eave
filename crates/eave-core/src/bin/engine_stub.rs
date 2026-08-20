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
use eave_core::journal::{now_ms, Header, JournalWriter, FORMAT_VERSION};
use eave_core::recorder::{record_session, send_and_journal};
use eave_core::session::{AgentSession, SpawnConfig};
use eave_core::store::{EngineLock, LockError, Meta, Store, META_FORMAT_VERSION};
use tokio::sync::mpsc;

fn env_required(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("engine-stub: {key} must be set"))
}

#[tokio::main]
async fn main() {
    // Lock-probe mode: attempt the engine instance lock and report via exit
    // code (0 = acquired, 42 = busy, naming the holder on stdout). Used by
    // the contended-start acceptance tests, which must probe from a
    // separate process.
    if std::env::var("STUB_TRY_LOCK").is_ok() {
        match EngineLock::acquire(std::path::Path::new(&env_required("STUB_DATA_DIR"))) {
            Ok(_held) => {
                println!("LOCK_ACQUIRED");
                std::process::exit(0);
            }
            Err(LockError::Busy { holder }) => {
                match holder {
                    Some(h) => println!("LOCK_BUSY holder_pid={}", h.pid),
                    None => println!("LOCK_BUSY holder_pid=none"),
                }
                std::process::exit(42);
            }
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
    // A real engine always holds the instance lock; the stub takes it only
    // on request because some tests run several stubs against one store to
    // model multiple sessions under a single engine.
    let _lock = std::env::var("STUB_TAKE_LOCK")
        .ok()
        .map(|_| EngineLock::acquire(store.root()).expect("engine lock"));
    let dir = store.create_session_dir(&session_id).expect("session dir");
    let cwd = std::env::temp_dir();

    let header = Header {
        format_version: FORMAT_VERSION,
        session_id: session_id.clone(),
        provider: "fake-agent".into(),
        cwd: cwd.display().to_string(),
        created_ts_ms: now_ms(),
        agent: Default::default(),
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
        title: None,
    };
    meta.save(&dir).expect("save meta");

    let (extra_tx, extra_rx) = mpsc::channel(16);
    if let Some(text) = message {
        // Journal-then-send; a failed pipe write leaves the message in the
        // journaled-but-undelivered state (spec 02 §Delivery state).
        let _delivered = send_and_journal(&mut session, &extra_tx, &text)
            .await
            .expect("recorder must be alive");
    }
    drop(extra_tx);

    let outcome = record_session(events, extra_rx, writer, dir, meta, None)
        .await
        .expect("record session");
    println!("engine-stub outcome: {outcome:?}");
}
