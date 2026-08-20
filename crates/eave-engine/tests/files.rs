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
    socket_path, DiffKind, DiffOp, FileStatus, FilesSnapshot, GitState, ResponseBody,
    ResponseEnvelope, SubscriptionItem,
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
        Self::spawn_with_env(data_dir, fake_mode, &[])
    }

    fn spawn_with_env(data_dir: &Path, fake_mode: &str, envs: &[(&str, &str)]) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let mut cmd = Command::new(engined_bin());
        cmd.args(["--data-dir", &data_dir.display().to_string()])
            .env("EAVE_AGENT", fake_agent_bin())
            .env("EAVE_FAKE_MODE", fake_mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn eave-engined");
        let me = Self {
            child,
            data_dir: data_dir.to_path_buf(),
        };
        me.await_socket();
        me
    }

    /// An engine with NO agent override — its default agent is the real
    /// `claude` CLI. Only for tests that never send: opens spawn
    /// nothing, so these run on machines without claude installed.
    fn spawn_real_claude(data_dir: &Path) -> Self {
        let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
        let mut cmd = Command::new(engined_bin());
        cmd.args(["--data-dir", &data_dir.display().to_string()])
            .env_remove("EAVE_AGENT")
            .env_remove("EAVE_FAKE_MODE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        let child = cmd.spawn().expect("spawn eave-engined");
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
    // B2: diff CONTENT canaries — file contents must flow only on a diff
    // reply's own envelope id, never on any subscription channel.
    const CONTENT_A: &str = "CONTENT_CANARY_A_51d0";
    const CONTENT_B: &str = "CONTENT_CANARY_B_92e7";
    let dir = scratch();
    let repo_a = repo();
    let repo_b = repo();
    std::fs::write(repo_a.join(CANARY_A), format!("{CONTENT_A}\n")).unwrap();
    std::fs::write(repo_b.join(CANARY_B), format!("{CONTENT_B}\n")).unwrap();
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

    // B2: request each session's canary diff on this same raw socket —
    // the content must arrive on the requesting envelope id and nowhere
    // else. Keep collecting the subscription ids while we wait.
    w.write_all(
        format!(
            "{{\"id\":5,\"method\":\"diff\",\"params\":{{\"session_id\":\"sA\",\"path\":\"{CANARY_A}\"}}}}\n{{\"id\":6,\"method\":\"diff\",\"params\":{{\"session_id\":\"sB\",\"path\":\"{CANARY_B}\"}}}}\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut diff_a_line: Option<String> = None;
    let mut diff_b_line: Option<String> = None;
    let end = Instant::now() + Duration::from_secs(15);
    while diff_a_line.is_none() || diff_b_line.is_none() {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "diff replies never arrived");
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .expect("frame in time")
            .unwrap()
            .expect("raw stream open");
        let env: ResponseEnvelope = serde_json::from_str(&line).expect("valid frame");
        match env.id {
            2 => watch_lines.push(line),
            3 => files_a_lines.push(line),
            4 => files_b_lines.push(line),
            5 => diff_a_line = Some(line),
            6 => diff_b_line = Some(line),
            _ => {}
        }
    }
    // Positive controls: each diff reply carries its own file's content.
    assert!(
        diff_a_line.as_deref().unwrap().contains(CONTENT_A),
        "sA diff missing its content: {diff_a_line:?}"
    );
    assert!(
        diff_b_line.as_deref().unwrap().contains(CONTENT_B),
        "sB diff missing its content: {diff_b_line:?}"
    );
    // And never each other's.
    assert!(!diff_a_line.as_deref().unwrap().contains(CONTENT_B));
    assert!(!diff_b_line.as_deref().unwrap().contains(CONTENT_A));
    // Diff content never appears on ANY subscription channel.
    for line in files_a_lines
        .iter()
        .chain(files_b_lines.iter())
        .chain(watch_lines.iter())
    {
        assert!(
            !line.contains(CONTENT_A) && !line.contains(CONTENT_B),
            "diff content leaked onto a subscription channel: {line}"
        );
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

/// The polling FALLBACK end to end (B2 keeps B1's contract alive for
/// when watching is unavailable — here forced via `EAVE_NO_FSWATCH=1`):
/// an idle-time modification does NOT refresh anything (no watcher, no
/// turn), and the next turn boundary picks it up.
#[tokio::test]
async fn turn_boundary_poll_is_the_fallback_without_a_watcher() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn_with_env(&dir, "linger", &[("EAVE_NO_FSWATCH", "1")]);
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");

    let mut rx = c.files("s1").await.expect("files");
    let snap = wait_for_snapshot(&mut rx, "open-time snapshot", |_| true).await;
    assert_eq!(snap.state, GitState::Ok);
    assert!(snap.files.is_empty(), "repo starts clean: {:?}", snap.files);
    // Drain the open-time/subscribe-time scan stragglers (both triggers
    // may fire once each) so the quiet-check below sees only what the
    // WRITE causes.
    while tokio::time::timeout(Duration::from_millis(700), rx.recv())
        .await
        .is_ok()
    {}

    // Change the worktree while the session idles: with the watcher
    // disabled and no live turn, nothing may refresh.
    std::fs::write(repo.join("base.txt"), "one\nCHANGED\nthree\n").unwrap();
    let quiet = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
    assert!(
        quiet.is_err(),
        "fallback mode must not refresh on a bare fs write: {quiet:?}"
    );
    assert!(c.send("s1", "go").await.expect("send"));

    let snap = wait_for_snapshot(&mut rx, "post-turn snapshot", |s| !s.files.is_empty()).await;
    assert_eq!(snap.state, GitState::Ok);
    // The mode is a stated fact on the wire: no watcher → the snapshot
    // says so (the rail renders "refreshes at turn end" from this).
    assert!(!snap.watching, "fallback snapshots must state watching=false");
    let f = &snap.files[0];
    assert_eq!(f.path, "base.txt");
    assert_eq!(f.status, FileStatus::Modified);
    assert_eq!((f.adds, f.dels), (Some(2), Some(1)));

    drop(engine);
}

/// B2's live invalidation: with a files subscriber attached, a bare fs
/// write — no turn boundary anywhere near — refreshes the snapshot via
/// the FS watcher.
#[tokio::test]
async fn fs_events_refresh_without_turn_boundaries() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");

    let mut rx = c.files("s1").await.expect("files");
    let _ = wait_for_snapshot(&mut rx, "initial snapshot", |_| true).await;

    // No send, no turn — the write alone must surface.
    std::fs::write(repo.join("base.txt"), "one\nLIVE\ntwo\n").unwrap();
    // The predicate carries `watching` for a reason, and dropping it makes
    // this test flaky under load rather than wrong: `engine.rs` states that
    // the frames between subscribing and the watcher's startup scan may
    // legitimately say `watching = false`, and that "that scan's own frame
    // corrects it". If the write lands before the startup scan — which a
    // loaded machine makes ordinary — then the startup scan's own frame is
    // already non-empty, a bare `!files.is_empty()` selects THAT frame, and
    // the assertion below fails against a flag the engine never promised on
    // it.
    //
    // So the predicate must select the frame that PROVES the claim, not
    // merely one consistent with it. Timing out here still means the real
    // failure — no watcher-driven refresh ever arrived.
    let snap = wait_for_snapshot(&mut rx, "watcher-driven snapshot", |s| {
        !s.files.is_empty() && s.watching
    })
    .await;
    assert_eq!(snap.files[0].path, "base.txt");
    assert_eq!(snap.files[0].status, FileStatus::Modified);

    drop(engine);
}

/// Fixing a dogfood blocker (directive 2026-08-17): the real claude CLI
/// rejects non-UUID `--session-id` values, so a named session opened
/// against it could only ever die at its first spawn. The engine now
/// refuses such an open with the reason — a stated error instead of a
/// doomed session. Opens never spawn the agent, so this runs fine on a
/// machine without claude; overridden agents (every other test here)
/// keep accepting any id.
#[tokio::test]
async fn non_uuid_open_is_refused_for_the_real_claude() {
    let dir = scratch();
    let engine = EngineProc::spawn_real_claude(&dir);
    let c = Client::connect(&dir).await.expect("connect");

    let err = c
        .open(Some("s-alpha".into()), None)
        .await
        .expect_err("non-UUID id must be refused");
    let msg = format!("{err}");
    assert!(msg.contains("not a UUID"), "unhelpful refusal: {msg}");

    // A UUID opens normally, and the refused open created nothing.
    let uuid = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    let info = c
        .open(Some(uuid.into()), None)
        .await
        .expect("uuid open succeeds");
    assert_eq!(info.session_id, uuid);
    let sessions = c.list().await.expect("list");
    assert_eq!(sessions.len(), 1, "{sessions:?}");

    drop(engine);
}

/// The debounce constraint: an agent-style burst of 200 rapid writes
/// produces a bounded number of refreshes (one or a few, never one per
/// write), and the final snapshot covers every file.
#[tokio::test]
async fn burst_of_writes_coalesces_into_bounded_refreshes() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    let mut rx = c.files("s1").await.expect("files");
    let _ = wait_for_snapshot(&mut rx, "initial snapshot", |_| true).await;

    for i in 0..200 {
        std::fs::write(repo.join(format!("burst{i:03}.txt")), "x\n").unwrap();
    }

    // Collect every frame for a fixed window past the burst; the last
    // one must be complete, and the count must be a handful, not 200.
    let mut frames: Vec<FilesSnapshot> = Vec::new();
    let end = Instant::now() + Duration::from_secs(4);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(SubscriptionItem::Files { snapshot, .. })) => frames.push(snapshot),
            Ok(Some(_)) => {}
            Ok(None) => panic!("files stream ended"),
            Err(_) => break,
        }
    }
    assert!(!frames.is_empty(), "burst produced no refresh at all");
    assert!(
        frames.len() <= 6,
        "200 writes produced {} refreshes — debounce is thrashing",
        frames.len()
    );
    let last = frames.last().unwrap();
    assert_eq!(
        last.files.len(),
        200,
        "final snapshot incomplete: {} files",
        last.files.len()
    );

    drop(engine);
}

/// The diff request end to end: every requested shape comes back as a
/// stated fact — text hunks for a modification, all-adds for untracked,
/// binary stated, and a path the engine has no entry for still answers.
#[tokio::test]
async fn diff_over_the_wire_states_every_shape() {
    let dir = scratch();
    let repo = repo();
    std::fs::write(repo.join("base.txt"), "one\nEDIT\ntwo\n").unwrap();
    std::fs::write(repo.join("new.txt"), "n1\nn2\n").unwrap();
    std::fs::write(repo.join("blob.bin"), [0u8, 1, 0]).unwrap();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    // Prime the snapshot so diffs see untracked/orig facts.
    let mut rx = c.files("s1").await.expect("files");
    let _ = wait_for_snapshot(&mut rx, "snapshot", |s| s.files.len() == 3).await;

    let d = c.diff("s1", "base.txt").await.expect("diff base");
    match d.kind {
        DiffKind::Text { hunks, truncated } => {
            assert!(!truncated);
            assert!(hunks[0]
                .lines
                .iter()
                .any(|l| l.op == DiffOp::Add && l.text == "EDIT"));
        }
        other => panic!("expected text diff, got {other:?}"),
    }

    let d = c.diff("s1", "new.txt").await.expect("diff new");
    match d.kind {
        DiffKind::Text { hunks, .. } => {
            assert!(hunks[0].lines.iter().all(|l| l.op == DiffOp::Add));
        }
        other => panic!("expected all-added diff, got {other:?}"),
    }

    let d = c.diff("s1", "blob.bin").await.expect("diff bin");
    assert_eq!(d.kind, DiffKind::Binary);

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
