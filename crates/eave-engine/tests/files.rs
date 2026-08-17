//! Slice B1 acceptance: the `files` subscription (spec 05 §Files) — the
//! changed-file overview, and the second place per-session isolation
//! could quietly break (watch was the first).
//!
//! The test that matters is the leak test: raw-socket subscriptions to
//! two sessions' files channels (plus watch) collect every byte the
//! engine emits on those ids while both sessions' repos hold
//! canary-named files — and no canary may ever cross to the wrong id.
//! Positive controls (each canary DOES appear on its own files id) keep
//! the assertion non-vacuous.
//!
//! Same harness rules as `wire.rs`: data dirs under `/tmp/eave-wire-tests`
//! (`sun_path` limit), workspace-built bins.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use eave_core::proc::{self, ProcIdentity};
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::{
    socket_path, FileStatus, FilesSnapshot, GitState, ResponseBody, ResponseEnvelope,
    SubscriptionItem,
};

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

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo with one committed file, in its own scratch dir.
fn repo() -> PathBuf {
    let dir = scratch();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("base.txt"), "one\ntwo\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    dir
}

struct EngineProc {
    child: Child,
    data_dir: PathBuf,
}

impl EngineProc {
    fn spawn(data_dir: &Path, fake_mode: &str) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
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

    fn sigkill(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGKILL);
        }
        let _ = self.child.wait();
        let sessions = self.data_dir.join("sessions");
        if let Ok(entries) = std::fs::read_dir(sessions) {
            for entry in entries.flatten() {
                if let Ok(meta) = Meta::load(&entry.path()) {
                    if let Some(id) = meta.child {
                        kill_if_matches(&id);
                    }
                }
            }
        }
    }
}

fn kill_if_matches(id: &ProcIdentity) {
    if let Some(current) = proc::identity_of(id.pid) {
        if current == *id {
            unsafe {
                libc::killpg(id.pid, libc::SIGKILL);
            }
        }
    }
}

/// Wait for a files frame whose snapshot satisfies the predicate;
/// panics past the deadline. Frames fold by replacement, so skipping
/// non-matching snapshots is the client contract, not a fudge.
async fn wait_for_snapshot(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    what: &str,
    pred: impl Fn(&FilesSnapshot) -> bool,
) -> FilesSnapshot {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("files stream ended unexpectedly");
        if let SubscriptionItem::Files { snapshot, .. } = item {
            if pred(&snapshot) {
                return snapshot;
            }
        }
    }
}

/// THE isolation test (build directive, Slice B1): raw-socket
/// subscriptions collect every byte on two sessions' files ids and the
/// watch id across both sessions' lives — and file data never crosses a
/// session boundary, never appears on watch.
#[tokio::test]
async fn files_channel_never_crosses_sessions() {
    const CANARY_A: &str = "CANARY_A_3f9a.txt";
    const CANARY_B: &str = "CANARY_B_7c2b.txt";
    let dir = scratch();
    let repo_a = repo();
    let repo_b = repo();
    std::fs::write(repo_a.join(CANARY_A), "aaa\n").unwrap();
    std::fs::write(repo_b.join(CANARY_B), "bbb\n").unwrap();
    let engine = EngineProc::spawn(&dir, "linger");

    let c = Client::connect(&dir).await.expect("connect client");
    c.open(Some("sA".into()), Some(repo_a.display().to_string()))
        .await
        .expect("open sA");
    c.open(Some("sB".into()), Some(repo_b.display().to_string()))
        .await
        .expect("open sB");

    // Raw sockets: assert on actual bytes, not on what a typed client
    // deigned to decode. id 2 = watch, id 3 = files sA, id 4 = files sB.
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .expect("connect raw");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    let hello = lines.next_line().await.unwrap().expect("hello reply");
    assert!(hello.contains("\"ok\""), "{hello}");
    w.write_all(b"{\"id\":2,\"method\":\"watch\"}\n").await.unwrap();
    w.write_all(b"{\"id\":3,\"method\":\"files\",\"params\":{\"session_id\":\"sA\"}}\n")
        .await
        .unwrap();
    w.write_all(b"{\"id\":4,\"method\":\"files\",\"params\":{\"session_id\":\"sB\"}}\n")
        .await
        .unwrap();

    // Drive both sessions through a real turn while we listen — turn
    // boundaries poke the files poll on top of the open-time scans.
    assert!(c.send("sA", "go a").await.expect("send sA"));
    assert!(c.send("sB", "go b").await.expect("send sB"));

    let mut watch_lines: Vec<String> = Vec::new();
    let mut files_a_lines: Vec<String> = Vec::new();
    let mut files_b_lines: Vec<String> = Vec::new();
    let mut saw_a = false;
    let mut saw_b = false;
    let end = Instant::now() + Duration::from_secs(20);
    // Positive controls: each canary must appear on its OWN files id.
    while !(saw_a && saw_b) {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "canaries never arrived (a: {saw_a}, b: {saw_b})"
        );
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .expect("frame in time")
            .unwrap()
            .expect("raw stream open");
        let env: ResponseEnvelope = serde_json::from_str(&line).expect("valid frame");
        match env.id {
            2 => {
                if let ResponseBody::Item(SubscriptionItem::Sessions { .. }) = &env.body {
                } else {
                    panic!("watch id carried a non-sessions frame: {line}");
                }
                watch_lines.push(line);
            }
            3 => {
                if let ResponseBody::Item(SubscriptionItem::Files { session_id, .. }) = &env.body {
                    assert_eq!(session_id, "sA", "files frame tagged wrong: {line}");
                } else {
                    panic!("files id carried a non-files frame: {line}");
                }
                saw_a |= line.contains(CANARY_A);
                files_a_lines.push(line);
            }
            4 => {
                if let ResponseBody::Item(SubscriptionItem::Files { session_id, .. }) = &env.body {
                    assert_eq!(session_id, "sB", "files frame tagged wrong: {line}");
                } else {
                    panic!("files id carried a non-files frame: {line}");
                }
                saw_b |= line.contains(CANARY_B);
                files_b_lines.push(line);
            }
            _ => {}
        }
    }

    // The isolation invariant, on raw bytes.
    for line in &files_a_lines {
        assert!(
            !line.contains(CANARY_B),
            "sB file data leaked onto sA's files channel: {line}"
        );
    }
    for line in &files_b_lines {
        assert!(
            !line.contains(CANARY_A),
            "sA file data leaked onto sB's files channel: {line}"
        );
    }
    assert!(!watch_lines.is_empty(), "watch stayed silent");
    for line in &watch_lines {
        assert!(
            !line.contains(CANARY_A) && !line.contains(CANARY_B),
            "file data leaked onto the watch channel: {line}"
        );
        assert!(
            !line.contains("\"snapshot\"") && !line.contains("\"files\""),
            "files payload structure on the watch channel: {line}"
        );
    }

    drop(engine);
}

/// Non-git cwd and vanished cwd both render as stated facts — the engine
/// stays up, the session stays usable.
#[tokio::test]
async fn non_git_and_broken_cwds_are_stated_facts() {
    let dir = scratch();
    let plain_dir = scratch(); // exists, not a repo
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");

    c.open(Some("plain".into()), Some(plain_dir.display().to_string()))
        .await
        .expect("open plain");
    let mut rx = c.files("plain").await.expect("files plain");
    let snap = wait_for_snapshot(&mut rx, "not-a-repo snapshot", |_| true).await;
    assert_eq!(snap.state, GitState::NotARepo);
    assert!(snap.files.is_empty());

    c.open(
        Some("broken".into()),
        Some("/tmp/eave-wire-tests/vanished-cwd-x".into()),
    )
    .await
    .expect("open broken");
    let mut rx = c.files("broken").await.expect("files broken");
    let snap = wait_for_snapshot(&mut rx, "broken-cwd snapshot", |_| true).await;
    assert!(
        matches!(snap.state, GitState::Error { .. }),
        "{:?}",
        snap.state
    );

    // The engine survived both and still answers.
    assert_eq!(c.list().await.expect("list").len(), 2);
    drop(engine);
}

/// The B1 poll contract end to end: session open scans (clean repo → Ok
/// and empty), and the next turn boundary picks up a modification made
/// while the session idled — no FS watcher involved.
#[tokio::test]
async fn turn_boundary_poll_picks_up_changes() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");

    let mut rx = c.files("s1").await.expect("files");
    let snap = wait_for_snapshot(&mut rx, "open-time snapshot", |_| true).await;
    assert_eq!(snap.state, GitState::Ok);
    assert!(snap.files.is_empty(), "repo starts clean: {:?}", snap.files);

    // Change the worktree while the session idles: with no live turn,
    // nothing polls — B1 is turn-boundary polling by design.
    std::fs::write(repo.join("base.txt"), "one\nCHANGED\nthree\n").unwrap();
    assert!(c.send("s1", "go").await.expect("send"));

    let snap = wait_for_snapshot(&mut rx, "post-turn snapshot", |s| !s.files.is_empty()).await;
    assert_eq!(snap.state, GitState::Ok);
    let f = &snap.files[0];
    assert_eq!(f.path, "base.txt");
    assert_eq!(f.status, FileStatus::Modified);
    assert_eq!((f.adds, f.dels), (Some(2), Some(1)));

    drop(engine);
}

/// Engine restart: file state is ephemeral and never journaled, so a
/// fresh engine rebuilds it from git on the first subscription — nothing
/// to repair, nothing persisted.
#[tokio::test]
async fn file_state_rebuilds_from_git_after_engine_restart() {
    let dir = scratch();
    let repo = repo();
    std::fs::write(repo.join("extra.txt"), "x\n").unwrap();
    let mut engine1 = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    let mut rx = c.files("s1").await.expect("files");
    let snap = wait_for_snapshot(&mut rx, "first snapshot", |s| !s.files.is_empty()).await;
    assert_eq!(snap.files[0].path, "extra.txt");

    engine1.sigkill();
    let engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("reconnect");
    let mut rx2 = c2.files("s1").await.expect("files after restart");
    let snap = wait_for_snapshot(&mut rx2, "rebuilt snapshot", |s| !s.files.is_empty()).await;
    assert_eq!(snap.state, GitState::Ok);
    assert_eq!(snap.files[0].path, "extra.txt");
    assert_eq!(snap.files[0].status, FileStatus::Added);

    drop(engine2);
}

/// The stale-engine fix (2026-08-17 trap): a request this engine cannot
/// parse — an unknown method from a newer client — errors on the REAL
/// envelope id, so the caller's pending call resolves instead of hanging
/// forever against a reply with id 0.
#[tokio::test]
async fn unparseable_requests_error_on_their_own_id() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");
    let stream = tokio::net::UnixStream::connect(socket_path(&dir))
        .await
        .expect("connect raw");
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
        .await
        .unwrap();
    let _ = lines.next_line().await.unwrap().expect("hello reply");
    w.write_all(b"{\"id\":9,\"method\":\"frobnicate\",\"params\":{}}\n")
        .await
        .unwrap();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("reply in time")
        .unwrap()
        .expect("stream open");
    let env: ResponseEnvelope = serde_json::from_str(&line).expect("valid frame");
    assert_eq!(env.id, 9, "error must echo the envelope id: {line}");
    assert!(
        matches!(env.body, ResponseBody::Err { .. }),
        "unknown method must error: {line}"
    );
    drop(engine);
}
