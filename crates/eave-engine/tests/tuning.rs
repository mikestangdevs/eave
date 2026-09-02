//! Slice TN1 — a session's effort level / fast lane, end to end over the
//! socket: refused with the CLI's own list, durable across an engine
//! restart, and applied to an argv-only agent by a restart at the next
//! send rather than mid-turn.
//!
//! The fake agent stands in for Claude Code here (the engine's default
//! provider), whose adapter declares both knobs `NextSpawn`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eave_core::store::Meta;
use eave_core::tuning::Tuning;
use eave_engine::client::Client;
use eave_wire::{socket_path, TakesHold};

fn engined_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"))
}

fn fake_agent_bin() -> PathBuf {
    let path = engined_bin().parent().expect("bin dir").join("fake-agent");
    assert!(path.exists(), "fake-agent not built at {path:?}");
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
            .current_dir(data_dir)
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
        let sock = socket_path(&me.data_dir);
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "engine never opened its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        me
    }

    fn sigkill(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        self.sigkill();
        if let Ok(entries) = std::fs::read_dir(self.data_dir.join("sessions")) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(child) = meta.child {
                        unsafe {
                            libc::kill(child.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
}

fn session_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("sessions").join(id)
}

async fn wait_live(c: &Client, id: &str, want: bool) {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "session never became live={want}");
        let listed = c.list().await.expect("list");
        let s = listed.iter().find(|s| s.session_id == id).unwrap();
        if s.live == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_turn_settled(c: &Client, id: &str, after_seq: u64) {
    // The linger fake answers each line with a whole turn; the journal's
    // `last_seq` moves past the send and the turn's close. Waiting for
    // TWO records past the send is the least this needs; the `send`
    // path then re-reads the journal itself.
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "turn never settled");
        let listed = c.list().await.expect("list");
        let s = listed.iter().find(|s| s.session_id == id).unwrap();
        if s.last_seq >= after_seq + 3 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_tuning_is_a_durable_operator_fact_and_an_unknown_level_is_refused_with_the_list() {
    let dir = scratch();
    let mut engine1 = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");

    let ok = c
        .tune(
            "s1",
            &Tuning {
                effort: Some("xhigh".into()),
                fast: true,
            },
        )
        .await
        .expect("tune");
    assert_eq!(ok.info.effort.as_deref(), Some("xhigh"));
    assert!(ok.info.fast);
    assert_eq!(
        ok.takes_hold,
        TakesHold::NextSpawn,
        "nothing is running, so the next start carries it"
    );

    // Refused with the CLI's own words, not eave's.
    let err = c
        .tune(
            "s1",
            &Tuning {
                effort: Some("ultra".into()),
                fast: false,
            },
        )
        .await
        .expect_err("ultra is not a Claude level");
    let msg = err.to_string();
    assert!(
        msg.contains("\"ultra\"") && msg.contains("low, medium, high, xhigh, max"),
        "{msg}"
    );
    // ... and the refusal changed nothing.
    let listed = c.list().await.expect("list");
    let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
    assert_eq!(s1.effort.as_deref(), Some("xhigh"));
    assert!(s1.fast);

    // On disk beside title/archived, and read back by a fresh engine.
    let meta = Meta::load(&session_dir(&dir, "s1")).expect("meta");
    assert_eq!(meta.tuning.effort.as_deref(), Some("xhigh"));
    engine1.sigkill();
    let _engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("connect 2");
    let listed = c2.list().await.expect("list 2");
    let s1 = listed.iter().find(|s| s.session_id == "s1").unwrap();
    assert_eq!(
        s1.effort.as_deref(),
        Some("xhigh"),
        "tuning did not survive a restart"
    );
    assert!(s1.fast);

    // Back to the provider's default states nothing on the wire.
    let ok = c2.tune("s1", &Tuning::default()).await.expect("clear");
    assert_eq!(ok.info.effort, None);
    assert!(!ok.info.fast);
}

#[tokio::test]
async fn an_argv_only_agent_is_restarted_at_the_next_send_never_mid_turn() {
    let dir = scratch();
    let _engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), None).await.expect("open s1");

    c.send("s1", "go").await.expect("send");
    wait_live(&c, "s1", true).await;
    let first = Meta::load(&session_dir(&dir, "s1"))
        .expect("meta")
        .child
        .expect("a live child")
        .pid;
    let seq0 = c
        .list()
        .await
        .unwrap()
        .iter()
        .find(|s| s.session_id == "s1")
        .unwrap()
        .last_seq;
    wait_turn_settled(&c, "s1", seq0.saturating_sub(3)).await;

    // Live, argv-only: the change is stated as taking hold at the next
    // send, and the process is NOT touched now.
    let ok = c
        .tune(
            "s1",
            &Tuning {
                effort: Some("high".into()),
                fast: false,
            },
        )
        .await
        .expect("tune");
    assert_eq!(ok.takes_hold, TakesHold::NextSend);
    let still = Meta::load(&session_dir(&dir, "s1"))
        .unwrap()
        .child
        .unwrap()
        .pid;
    assert_eq!(still, first, "a tune must not restart the agent by itself");

    // The next send finds no turn open and restarts under the new argv:
    // a different pid, and the session is live again.
    c.send("s1", "again").await.expect("send again");
    let end = Instant::now() + Duration::from_secs(15);
    let second = loop {
        assert!(Instant::now() < end, "the agent was never restarted");
        if let Ok(m) = Meta::load(&session_dir(&dir, "s1")) {
            if let Some(child) = m.child {
                if child.pid != first {
                    break child.pid;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_ne!(second, first);
    wait_live(&c, "s1", true).await;

    // Unchanged tuning: a further send does NOT restart it.
    c.send("s1", "and again").await.expect("send 3");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let third = Meta::load(&session_dir(&dir, "s1"))
        .unwrap()
        .child
        .unwrap()
        .pid;
    assert_eq!(
        third, second,
        "a send with nothing changed restarted the agent"
    );
}
