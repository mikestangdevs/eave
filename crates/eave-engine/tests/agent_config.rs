//! Slice P2 acceptance: per-session agent config, end to end through the
//! real open → persist → spawn path (spec 05 §Open, spec 02 §Resume
//! contract).
//!
//! Everything here runs the real engine binary. The spawn assertions read
//! the argv the CHILD actually received, via the fake agent's
//! `argv-report` mode, rather than inspecting the `SpawnConfig` the engine
//! built — that would test the engine against itself and would pass even
//! if the flags never reached a process.
//!
//! Four things must be true, and each is proven below:
//!
//! 1. A session's configured model reaches its agent process.
//! 2. That config outlives the engine, and governs the `--resume` spawn —
//!    which is the whole point of persisting it rather than reading env.
//! 3. A provider whose permission posture eave has not proven never spawns,
//!    and a flag that would undo the posture is refused. This is Slice K's
//!    blocker, guarded against the side door per-session config opens.
//! 4. A session's agent is fixed at open: re-opening under a different one
//!    is refused, never silently substituted.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eave_core::proc;
use eave_core::session::AgentConfig;
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::model::{Row, Transcript};
use eave_wire::{socket_path, SubscriptionItem};

fn engined_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"))
}

fn fake_agent_bin() -> PathBuf {
    let path = engined_bin().parent().expect("bin dir").join("fake-agent");
    assert!(
        path.exists(),
        "fake-agent not built at {path:?} — run `cargo test` from the workspace root"
    );
    path
}

fn scratch() -> PathBuf {
    let dir = PathBuf::from("/tmp/eave-wire-tests").join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path, fake_mode: &str) -> Self {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("engine-test.log"))
            .unwrap();
        let child = Command::new(engined_bin())
            .args(["--data-dir", &data_dir.display().to_string()])
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", fake_mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn eave-engined");
        let me = Self {
            child,
            data_dir: data_dir.to_path_buf(),
        };
        me.await_socket();
        me
    }

    fn await_socket(&self) {
        let sock = socket_path(&self.data_dir);
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "engine never opened its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Ok(entries) = std::fs::read_dir(self.data_dir.join("sessions")) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(id) = meta.child {
                        if let Some(current) = proc::identity_of(id.pid) {
                            if current == id {
                                unsafe {
                                    libc::killpg(id.pid, libc::SIGKILL);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Drive one turn and return what the agent said — which under
/// `argv-report` is its own argv.
///
/// Subscribes from the session's CURRENT end rather than from the start.
/// A `None` cursor replays the whole journal, so on the second engine of
/// the restart test the first turn's argv line arrives before the new
/// one's and the helper answers about the wrong spawn — which is exactly
/// how this was caught.
async fn argv_seen(c: &Client, session_id: &str) -> String {
    let here = c
        .list()
        .await
        .expect("list")
        .into_iter()
        .find(|s| s.session_id == session_id)
        .expect("session in list");
    let mut rx = c
        .subscribe(
            session_id,
            Some(eave_wire::Cursor {
                epoch: here.epoch,
                seq: here.last_seq,
            }),
        )
        .await
        .expect("subscribe");
    c.send(session_id, "go").await.expect("send");
    let mut t = Transcript::default();
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let said: Vec<String> = t
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        if let Some(line) = said
            .iter()
            .find(|l| l.contains("--session-id") || l.contains("--resume"))
        {
            return line.clone();
        }
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "no argv line; rows: {:?}", t.rows);
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out")
            .expect("subscription ended");
        match item {
            SubscriptionItem::Reset { .. } => t.reset(),
            SubscriptionItem::Events { epoch, records, .. } => {
                t.apply(epoch, &records).expect("no seq gaps");
            }
            _ => {}
        }
    }
}

fn claude(model: &str) -> AgentConfig {
    AgentConfig {
        provider: Some("claude-code".into()),
        program: None,
        model: Some(model.into()),
        extra_args: Vec::new(),
    }
}

/// 1. The configured model reaches the process. Asserted on the child's own
///    argv, so nothing between the config and `execve` can be silently
///    dropping it.
#[tokio::test]
async fn a_sessions_configured_model_reaches_its_agent_process() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    let info = c
        .open_with_agent(Some("s1".into()), None, Some(claude("opus-4-2")))
        .await
        .expect("open");
    assert_eq!(
        info.model.as_deref(),
        Some("opus-4-2"),
        "SessionInfo must state the configured model as a fact"
    );

    let argv = argv_seen(&c, "s1").await;
    assert!(
        argv.contains("--model opus-4-2"),
        "the session's model never reached the agent; argv was: {argv}"
    );
    assert!(
        argv.contains("--permission-prompt-tool stdio"),
        "per-session config must not displace the posture flag; argv: {argv}"
    );
}

/// A session with no config still spawns exactly as it did before — the
/// engine's default fills every gap and no `--model` is invented.
#[tokio::test]
async fn a_session_without_config_spawns_exactly_as_before() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    let info = c.open(Some("s1".into()), None).await.expect("open");
    assert_eq!(
        info.model, None,
        "no model configured means no model stated"
    );

    let argv = argv_seen(&c, "s1").await;
    assert!(
        !argv.contains("--model"),
        "eave invented a --model for a session that configured none: {argv}"
    );
}

/// 2. The whole reason the config is persisted rather than read from env:
///    it outlives the engine and governs the RESUME spawn.
#[tokio::test]
async fn the_config_survives_a_restart_and_governs_the_resume_spawn() {
    let dir = scratch();
    {
        let _engine = EngineProc::spawn(&dir, "argv-report");
        let c = Client::connect(&dir).await.expect("connect");
        c.open_with_agent(Some("s1".into()), None, Some(claude("haiku-9")))
            .await
            .expect("open");
        let first = argv_seen(&c, "s1").await;
        assert!(
            first.contains("--session-id"),
            "first spawn names the id: {first}"
        );
        assert!(first.contains("--model haiku-9"), "first spawn: {first}");
    }
    // Engine gone. A new one has no memory of the config except the journal.
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    let info = c.open(Some("s1".into()), None).await.expect("reopen");
    assert_eq!(
        info.model.as_deref(),
        Some("haiku-9"),
        "the model did not survive the restart"
    );

    let again = argv_seen(&c, "s1").await;
    assert!(
        again.contains("--resume"),
        "a session that ran before must resume, not re-name itself: {again}"
    );
    assert!(
        again.contains("--model haiku-9"),
        "the resumed spawn lost the session's model: {again}"
    );
}

/// 3. Slice K's blocker, guarded at the door per-session config opens. An
///    unverified provider is refused with the reason, and — the part that
///    matters — NOTHING is created for it.
#[tokio::test]
async fn an_unverified_provider_is_refused_and_leaves_no_session_behind() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    let err = c
        .open_with_agent(
            Some("s1".into()),
            None,
            Some(AgentConfig {
                provider: Some("codex".into()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("an unverified provider must not open");
    let msg = format!("{err}");
    assert!(
        msg.contains("codex") && msg.contains("posture"),
        "the refusal must name the provider and the reason: {msg}"
    );
    assert!(
        !dir.join("sessions").join("s1").exists(),
        "a refused open left a session directory behind"
    );
    assert!(
        c.list().await.expect("list").is_empty(),
        "a refused open left a session in the registry"
    );
}

/// The other half of the same door: a flag that would undo the posture is
/// refused even though the provider is the verified one.
#[tokio::test]
async fn a_flag_that_would_undo_the_permission_posture_is_refused() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    for bad in [
        vec!["--permission-mode".to_string(), "acceptEdits".to_string()],
        vec!["--permission-mode=acceptEdits".to_string()],
        vec!["--permission-prompt-tool".to_string(), "none".to_string()],
    ] {
        let err = c
            .open_with_agent(
                Some(format!("s-{}", bad.len())),
                None,
                Some(AgentConfig {
                    provider: Some("claude-code".into()),
                    extra_args: bad.clone(),
                    ..Default::default()
                }),
            )
            .await
            .expect_err("a posture-defeating flag must not open");
        assert!(
            format!("{err}").contains("permission"),
            "refusal must name the reason for {bad:?}"
        );
    }
}

/// 4. A session's agent is fixed at open. Asking for a different one is an
///    error, not a quiet substitution — a caller who picks a model and
///    silently gets another is the failure this refuses to have.
#[tokio::test]
async fn reopening_under_a_different_config_is_refused_not_substituted() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    c.open_with_agent(Some("s1".into()), None, Some(claude("opus-4-2")))
        .await
        .expect("open");

    let err = c
        .open_with_agent(Some("s1".into()), None, Some(claude("haiku-9")))
        .await
        .expect_err("a different config must be refused");
    assert!(
        format!("{err}").contains("different agent config"),
        "refusal must say what happened: {err}"
    );

    // Re-opening with the SAME config, or with none at all, is fine.
    c.open_with_agent(Some("s1".into()), None, Some(claude("opus-4-2")))
        .await
        .expect("same config must be accepted");
    let info = c.open(Some("s1".into()), None).await.expect("no config");
    assert_eq!(info.model.as_deref(), Some("opus-4-2"), "config unchanged");
}

/// The isolation invariant is unchanged (spec 05 §Watch): the watch channel
/// may carry the model as a SessionInfo fact — it is a fact about the
/// session, in the same class as provider and cwd — and carries no
/// transcript content with it.
#[tokio::test]
async fn the_watch_channel_states_the_model_and_still_no_content() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "argv-report");
    let c = Client::connect(&dir).await.expect("connect");
    c.open_with_agent(Some("s1".into()), None, Some(claude("opus-4-2")))
        .await
        .expect("open");

    let sessions = c.list().await.expect("list");
    let s = sessions.iter().find(|s| s.session_id == "s1").expect("s1");
    assert_eq!(s.model.as_deref(), Some("opus-4-2"));
    assert_eq!(s.provider, "claude-code");
    // The raw frame carries the model and nothing resembling a transcript.
    let raw = serde_json::to_string(&sessions).expect("serialize");
    assert!(raw.contains("opus-4-2"), "model must be on the wire: {raw}");
    for leak in ["\"text\"", "\"content\"", "\"tool_use\"", "\"records\""] {
        assert!(
            !raw.contains(leak),
            "watch payload carried {leak}, which is transcript content: {raw}"
        );
    }
}
