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
    socket_path, DiffBasis, DiffKind, DiffOp, DiffStat, FileScope, FileStatus, FilesSnapshot,
    GitState, ResponseBody, ResponseEnvelope, SessionInfo, SubscriptionItem,
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
    // Slice AA: turn-scoped canaries. Written AFTER each session's send,
    // so they exist only in that session's turn diff — a second, narrower
    // path for one session's file data to reach another's channel.
    const TURN_CANARY_A: &str = "TURN_CANARY_A_a41c.txt";
    const TURN_CANARY_B: &str = "TURN_CANARY_B_e6d8.txt";
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
    w.write_all(b"{\"id\":2,\"method\":\"watch\"}\n")
        .await
        .unwrap();
    w.write_all(b"{\"id\":3,\"method\":\"files\",\"params\":{\"session_id\":\"sA\"}}\n")
        .await
        .unwrap();
    w.write_all(b"{\"id\":4,\"method\":\"files\",\"params\":{\"session_id\":\"sB\"}}\n")
        .await
        .unwrap();
    // ids 7/8 = the same two sessions at turn scope (Slice AA). A second
    // basis is a second engine-side cache and a second scan path, so it
    // is a second way for one session's file data to end up on another
    // session's id — it gets the same raw-byte treatment as the first.
    w.write_all(
        b"{\"id\":7,\"method\":\"files\",\"params\":{\"session_id\":\"sA\",\"scope\":\"latest_turn\"}}\n",
    )
    .await
    .unwrap();
    w.write_all(
        b"{\"id\":8,\"method\":\"files\",\"params\":{\"session_id\":\"sB\",\"scope\":\"latest_turn\"}}\n",
    )
    .await
    .unwrap();

    // Drive both sessions through a real turn while we listen — turn
    // boundaries poke the files poll on top of the open-time scans.
    // `send_detailed`, not `send`: this pair has been an intermittent
    // failure under load for several slices and a bare `assert!` on the
    // bool could only ever report "it was false". The engine now keeps the
    // agent's own words about a refused write; if this fires again, the
    // message names the fault instead of the symptom.
    for sid in ["sA", "sB"] {
        let ok = c.send_detailed(sid, "go").await.expect("send");
        assert!(
            ok.delivered,
            "{sid}: the write never reached the agent — {}",
            ok.reason
                .as_deref()
                .unwrap_or("and the engine gave no reason")
        );
    }
    // Each session's turn canary lands after ITS dispatch, so it belongs
    // to that turn and to no other.
    std::fs::write(repo_a.join(TURN_CANARY_A), "turn a\n").unwrap();
    std::fs::write(repo_b.join(TURN_CANARY_B), "turn b\n").unwrap();

    let mut watch_lines: Vec<String> = Vec::new();
    let mut files_a_lines: Vec<String> = Vec::new();
    let mut files_b_lines: Vec<String> = Vec::new();
    let mut turn_a_lines: Vec<String> = Vec::new();
    let mut turn_b_lines: Vec<String> = Vec::new();
    let mut saw_a = false;
    let mut saw_b = false;
    let mut saw_turn_a = false;
    let mut saw_turn_b = false;
    let end = Instant::now() + Duration::from_secs(30);
    // Positive controls: each canary must appear on its OWN files id.
    while !(saw_a && saw_b && saw_turn_a && saw_turn_b) {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "canaries never arrived (a: {saw_a}, b: {saw_b}, \
             turn a: {saw_turn_a}, turn b: {saw_turn_b})"
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
            7 => {
                if let ResponseBody::Item(SubscriptionItem::Files { session_id, .. }) = &env.body {
                    assert_eq!(session_id, "sA", "files frame tagged wrong: {line}");
                } else {
                    panic!("files id carried a non-files frame: {line}");
                }
                saw_turn_a |= line.contains(TURN_CANARY_A);
                turn_a_lines.push(line);
            }
            8 => {
                if let ResponseBody::Item(SubscriptionItem::Files { session_id, .. }) = &env.body {
                    assert_eq!(session_id, "sB", "files frame tagged wrong: {line}");
                } else {
                    panic!("files id carried a non-files frame: {line}");
                }
                saw_turn_b |= line.contains(TURN_CANARY_B);
                turn_b_lines.push(line);
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
            7 => turn_a_lines.push(line),
            8 => turn_b_lines.push(line),
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
        .chain(turn_a_lines.iter())
        .chain(turn_b_lines.iter())
        .chain(watch_lines.iter())
    {
        assert!(
            !line.contains(CONTENT_A) && !line.contains(CONTENT_B),
            "diff content leaked onto a subscription channel: {line}"
        );
    }

    // The isolation invariant, on raw bytes — both bases.
    for line in files_a_lines.iter().chain(turn_a_lines.iter()) {
        assert!(
            !line.contains(CANARY_B) && !line.contains(TURN_CANARY_B),
            "sB file data leaked onto one of sA's files channels: {line}"
        );
    }
    for line in files_b_lines.iter().chain(turn_b_lines.iter()) {
        assert!(
            !line.contains(CANARY_A) && !line.contains(TURN_CANARY_A),
            "sA file data leaked onto one of sB's files channels: {line}"
        );
    }
    assert!(!watch_lines.is_empty(), "watch stayed silent");
    for line in &watch_lines {
        assert!(
            !line.contains(CANARY_A)
                && !line.contains(CANARY_B)
                && !line.contains(TURN_CANARY_A)
                && !line.contains(TURN_CANARY_B),
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
    assert!(
        !snap.watching,
        "fallback snapshots must state watching=false"
    );
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

/// AG2 #11, and this is the trap the row was built around.
///
/// `refresh_files` used to poke the watch channel only `if wt_changed` —
/// only when a WORKTREE fact moved. This session was never isolated, so
/// that condition is flatly `false` for it forever: the diffstat would be
/// computed correctly, stored correctly, and never pushed, and the rail
/// would show whatever it had at open time while the tree moved under it.
///
/// So: no isolate, no send, no turn boundary anywhere. The `files`
/// subscription is here only to start the FS watcher — nothing reads it.
/// Every assertion is on `watch`, which is the channel the rail's rows
/// actually arrive on.
#[tokio::test]
async fn a_plain_session_pushes_its_diffstat_on_watch_with_no_worktree_and_no_turn() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");

    let mut files = c.files("s1").await.expect("files");
    // Same reason `fs_events_refresh_without_turn_boundaries` waits on
    // this flag: until the watcher is live, a write is only guaranteed to
    // be picked up by a poll this test never triggers.
    let _ = wait_for_snapshot(&mut files, "watcher live", |s| s.watching).await;
    let mut watch = c.watch().await.expect("watch");

    std::fs::write(repo.join("base.txt"), "one\nLIVE\nALSO\ntwo\n").unwrap();
    let s1 = wait_for_watch(&mut watch, "the diffstat arrives", |s| {
        s.session_id == "s1" && s.diff_stat.is_some()
    })
    .await;
    assert_eq!(
        s1.diff_stat,
        Some(DiffStat { adds: 2, dels: 0 }),
        "two lines inserted into a committed file, and nothing removed"
    );
    assert!(
        s1.worktree.is_none(),
        "the point of the test: this session has no worktree fact to move"
    );

    // Both directions. A gate that pushed the rise and not the fall would
    // freeze every row at its last non-zero total, which reads as a stuck
    // number rather than a missing one and is harder to notice.
    std::fs::write(repo.join("base.txt"), "one\ntwo\n").unwrap();
    let back = wait_for_watch(&mut watch, "the diffstat goes back to nothing", |s| {
        s.session_id == "s1" && s.diff_stat.is_none()
    })
    .await;
    assert_eq!(back.diff_stat, None, "clean again, and said so");

    drop(engine);
}

async fn wait_for_watch<F>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    what: &str,
    pred: F,
) -> SessionInfo
where
    F: Fn(&SessionInfo) -> bool,
{
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("watch stream ended waiting for {what}"));
        if let SubscriptionItem::Sessions { sessions } = item {
            if let Some(found) = sessions.into_iter().find(&pred) {
                return found;
            }
        }
    }
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

/// AG2 #11 on a COLD engine — the half a harness cannot reach.
///
/// Every surface test hands the rail a `SessionInfo` with the field already
/// filled, so none of them can see whether anything ever fills it. Walked
/// with a real window, the answer was no: on a fresh engine every idle row
/// was blank and only the session you clicked grew numbers, because the
/// startup sweep scanned a session only if it claimed a WORKTREE (Slice I2)
/// and a plain session was left with `handle.files == None`.
///
/// The assertion is deliberately made through `list`, with **no `files`
/// subscription anywhere** — subscribing is what used to hide this, because
/// `run_files` triggers a scan when the cache is empty. This is the rail's
/// own vantage point: rows it never subscribed to.
#[tokio::test]
async fn a_cold_engine_states_the_diffstat_without_anyone_subscribing() {
    let dir = scratch();
    let repo = repo();
    let engine1 = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    // Committed base plus two new lines: a diff that exists before the
    // engine this test cares about has ever run.
    std::fs::write(repo.join("base.txt"), "one\nLIVE\nALSO\ntwo\n").unwrap();
    drop(c);
    drop(engine1);

    let engine2 = EngineProc::spawn(&dir, "linger");
    let c2 = Client::connect(&dir).await.expect("reconnect");
    let end = Instant::now() + Duration::from_secs(15);
    let stat = loop {
        assert!(
            Instant::now() < end,
            "a cold engine never stated the diffstat for an unsubscribed session"
        );
        let sessions = c2.list().await.expect("list");
        let s1 = sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("s1 listed");
        if let Some(stat) = s1.diff_stat {
            assert!(
                s1.worktree.is_none(),
                "the point of the test: a session I2's eager scan does not cover"
            );
            break stat;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        stat,
        DiffStat { adds: 2, dels: 0 },
        "the cold engine's own scan, not a subscriber's"
    );

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

// ---------------------------------------------------------------------------
// Turn-scoped changes (Slice AA)

/// Wait for the session's current turn to end on the wire. The base is
/// captured at DISPATCH, so nothing here depends on a turn having
/// finished — but the acceptance story does ("turn completes; the user
/// edits out of band"), and a test that skips the wait would be asserting
/// about an ordering it never established.
async fn wait_for_turn_end(rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < end, "turn never completed");
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(SubscriptionItem::Events { records, .. })) => {
                if records.iter().any(|r| {
                    matches!(
                        &r.event,
                        eave_core::journal::JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::TurnCompleted { .. }
                        }
                    )
                }) {
                    return;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("transcript stream ended"),
            Err(_) => {}
        }
    }
}

fn paths(snap: &FilesSnapshot) -> Vec<String> {
    let mut v: Vec<String> = snap.files.iter().map(|f| f.path.clone()).collect();
    v.sort();
    v
}

/// The PANE answers the same question the LIST does.
///
/// Slice AA scoped the file list to the turn and left the diff under it
/// HEAD-relative. The row was right and the hunks were wrong: click a file
/// in "this turn" and you saw every change since the last commit, under a
/// heading saying otherwise. That is worse than showing nothing, because
/// it looks like an answer.
#[tokio::test]
async fn a_turn_scoped_diff_shows_only_the_turn() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    let mut events = c.subscribe("s1", None).await.expect("subscribe");

    // One line edited BEFORE the turn, one DURING it, in the same file.
    // That is the whole test: a HEAD-relative diff shows both, a
    // turn-relative diff shows only the second.
    std::fs::write(repo.join("base.txt"), "one\nBEFORE THE TURN\nthree\n").unwrap();
    assert!(c.send("s1", "turn one").await.expect("send"));
    std::fs::write(
        repo.join("base.txt"),
        "one\nBEFORE THE TURN\nDURING THE TURN\n",
    )
    .unwrap();
    wait_for_turn_end(&mut events).await;

    let working = c.diff("s1", "base.txt").await.expect("working diff");
    let turn = c
        .diff_scoped("s1", "base.txt", FileScope::LatestTurn)
        .await
        .expect("turn diff");

    // CHANGED lines only. `git diff` prints context too, so an unchanged
    // "BEFORE THE TURN" appears in both diffs as a `Ctx` line — asserting
    // on the raw text would fail against a perfectly correct turn diff.
    // What separates the two bases is which lines are ADDED or REMOVED.
    let changed = |d: &eave_wire::FileDiff| match &d.kind {
        DiffKind::Text { hunks, .. } => hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| !matches!(l.op, DiffOp::Ctx))
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected a text diff, got {other:?}"),
    };
    let w = changed(&working);
    let t = changed(&turn);

    assert!(
        w.contains("BEFORE THE TURN") && w.contains("DURING THE TURN"),
        "the working-tree diff must show both edits:\n{w}"
    );
    assert!(
        t.contains("DURING THE TURN"),
        "the turn diff lost the turn's own edit:\n{t}"
    );
    assert!(
        !t.contains("BEFORE THE TURN"),
        "the turn diff is still HEAD-relative — it shows work the turn did \
         not do:\n{t}"
    );

    drop(engine);
}

/// THE acceptance story (Slice AA): three files are dirty and only one of
/// them belongs to the current turn.
///
/// The working tree cannot tell them apart — that is the whole problem —
/// so the turn scope is measured from a tree captured at dispatch. Note
/// what the working-tree list is here: it is not wrong, it is answering a
/// different question, and both answers ship.
#[tokio::test]
async fn latest_turn_lists_only_what_the_current_turn_changed() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    let mut events = c.subscribe("s1", None).await.expect("subscribe");

    // Dirt that predates everything: the operator's own uncommitted work.
    std::fs::write(repo.join("base.txt"), "one\nEDITED BEFORE ANY TURN\n").unwrap();

    // Turn 1 — its base is captured before the agent could touch anything.
    assert!(c.send("s1", "turn one").await.expect("send 1"));
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    wait_for_turn_end(&mut events).await;

    // Between turns, the operator edits something else entirely.
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();

    // Turn 2. Its base now contains base.txt's edit, a.rs and b.rs — all
    // of which are somebody else's work as far as this turn is concerned.
    assert!(c.send("s1", "turn two").await.expect("send 2"));
    std::fs::write(repo.join("a.rs"), "fn a() { /* turn two */ }\n").unwrap();

    let (turn_id, mut turn_rx) = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn");
    // The predicate must select the frame that PROVES the claim: a
    // turn-based snapshot that has already seen a change. An empty one is
    // consistent with the claim (the write may not have landed yet) and
    // proves nothing.
    let turn = wait_for_snapshot(&mut turn_rx, "turn-scoped snapshot", |s| {
        matches!(s.basis, DiffBasis::Turn { .. }) && !s.files.is_empty()
    })
    .await;
    assert_eq!(
        paths(&turn),
        vec!["a.rs".to_string()],
        "the turn scope must list this turn's file and nothing else"
    );
    assert_eq!(turn.files[0].status, FileStatus::Modified);

    let mut all_rx = c.files("s1").await.expect("files working_tree");
    let all = wait_for_snapshot(&mut all_rx, "working-tree snapshot", |s| s.files.len() >= 3).await;
    assert_eq!(
        paths(&all),
        vec![
            "a.rs".to_string(),
            "b.rs".to_string(),
            "base.txt".to_string()
        ],
        "the working tree still answers its own question in full"
    );
    assert_eq!(
        all.basis,
        DiffBasis::WorkingTree,
        "a working-tree snapshot states its basis"
    );

    let _ = c.unsubscribe(turn_id).await;
    drop(engine);
}

/// A file that was ALREADY untracked when the turn started is modified
/// during it. It must read MODIFIED with the turn's line counts, not
/// as an all-added file with the operator's lines credited to the agent.
///
/// This is the property that makes the base a TREE rather than `HEAD`:
/// both sides of the turn diff carry untracked content, so nothing has to
/// be synthesized and nothing is misattributed.
#[tokio::test]
async fn a_file_untracked_before_the_turn_reads_as_modified() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");

    // Three lines the operator wrote, never committed, never staged.
    std::fs::write(repo.join("notes.txt"), "mine one\nmine two\nmine three\n").unwrap();
    assert!(c.send("s1", "go").await.expect("send"));
    // The turn appends one line to it.
    std::fs::write(
        repo.join("notes.txt"),
        "mine one\nmine two\nmine three\nagent line\n",
    )
    .unwrap();

    let (turn_id, mut turn_rx) = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn");
    let turn = wait_for_snapshot(&mut turn_rx, "turn-scoped snapshot", |s| {
        matches!(s.basis, DiffBasis::Turn { .. }) && !s.files.is_empty()
    })
    .await;
    assert_eq!(paths(&turn), vec!["notes.txt".to_string()]);
    let f = &turn.files[0];
    assert_eq!(
        f.status,
        FileStatus::Modified,
        "already-untracked at turn start = modified by the turn, not added"
    );
    assert_eq!(
        (f.adds, f.dels),
        (Some(1), Some(0)),
        "only the turn's own line counts"
    );
    // The flag still describes the WORKING TREE, because it is what picks
    // the diff strategy for the file (`git diff HEAD` cannot see it).
    assert!(f.untracked, "the file is still untracked on disk");

    // And the working-tree scope keeps answering its own question: the
    // whole file is new relative to HEAD.
    let mut all_rx = c.files("s1").await.expect("files working_tree");
    let all = wait_for_snapshot(&mut all_rx, "working-tree snapshot", |s| {
        !s.files.is_empty()
    })
    .await;
    let f = all.files.iter().find(|f| f.path == "notes.txt").unwrap();
    assert_eq!(f.status, FileStatus::Added);
    assert_eq!((f.adds, f.dels), (Some(4), Some(0)));

    let _ = c.unsubscribe(turn_id).await;
    drop(engine);
}

/// The turn basis is live memory, and an engine restart drops it. That is
/// a SENTENCE, not an error and not an empty list — the difference
/// between "this turn changed nothing" and "there is no turn to measure
/// against" is the entire point of the feature.
///
/// The working-tree scope is untouched by the same restart: git is its
/// store of record and rebuilds it for free.
#[tokio::test]
async fn a_restart_states_that_the_turn_basis_is_gone() {
    let dir = scratch();
    let repo = repo();
    let mut engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    assert!(c.send("s1", "go").await.expect("send"));
    std::fs::write(repo.join("mid-turn.txt"), "written during the turn\n").unwrap();

    // Prove the basis existed before the kill — otherwise "unavailable"
    // afterwards would be indistinguishable from a feature that never
    // worked.
    let mut turn_rx = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn")
        .1;
    let before = wait_for_snapshot(&mut turn_rx, "pre-kill turn snapshot", |s| {
        matches!(s.basis, DiffBasis::Turn { .. }) && !s.files.is_empty()
    })
    .await;
    assert_eq!(paths(&before), vec!["mid-turn.txt".to_string()]);
    drop(c);

    engine.sigkill();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("reconnect");
    c.open(Some("s1".into()), None).await.expect("reopen");

    let (turn_id, mut turn_rx) = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn");
    let after = wait_for_snapshot(&mut turn_rx, "post-restart turn snapshot", |s| {
        matches!(s.basis, DiffBasis::Unavailable { .. })
    })
    .await;
    let DiffBasis::Unavailable { reason } = &after.basis else {
        unreachable!("selected by the predicate")
    };
    assert!(
        reason.contains("engine"),
        "the reason must name what was lost and why: {reason:?}"
    );
    assert!(
        after.files.is_empty(),
        "an unavailable basis carries words, never a list"
    );
    assert_eq!(
        after.state,
        GitState::Ok,
        "the repository is fine — only the basis is gone"
    );

    // The working tree is completely unaffected by the restart.
    let mut all_rx = c.files("s1").await.expect("files working_tree");
    let all = wait_for_snapshot(&mut all_rx, "post-restart working tree", |s| {
        !s.files.is_empty()
    })
    .await;
    assert_eq!(paths(&all), vec!["mid-turn.txt".to_string()]);

    let _ = c.unsubscribe(turn_id).await;
    drop(engine);
}

/// **A `git gc --prune=now` no longer collects the turn basis** — and it
/// used to, which is what this test was originally written to accept.
///
/// The basis is written through a throwaway index, so it was unreachable
/// by construction and an agent running housekeeping mid-turn could take
/// it out from under us. Slice AG1 #11 pins the same tree under
/// `refs/eave/` because a checkpoint has to survive far longer than one
/// turn; the diff basis inherits that protection for free.
///
/// Kept as a live-engine test rather than folded into `checkpoint.rs`:
/// the unit tests there prove `capture_checkpoint` pins a tree, and this
/// proves the SEND PATH actually calls it. Those are different claims,
/// and only the second one fails if the wiring is dropped.
#[tokio::test]
async fn a_gc_no_longer_collects_the_turn_basis() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    // Dirty first, so the captured tree is not simply HEAD's own tree —
    // that one is reachable anyway and would prove nothing.
    std::fs::write(repo.join("pre.txt"), "dirty before the turn\n").unwrap();
    assert!(c.send("s1", "go").await.expect("send"));
    std::fs::write(repo.join("during.txt"), "during the turn\n").unwrap();

    let (turn_id, mut turn_rx) = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn");
    let before = wait_for_snapshot(&mut turn_rx, "pre-gc turn snapshot", |s| {
        matches!(s.basis, DiffBasis::Turn { .. }) && !s.files.is_empty()
    })
    .await;
    assert_eq!(paths(&before), vec!["during.txt".to_string()]);

    // What an agent running housekeeping does to us.
    git(&repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&repo, &["gc", "--prune=now", "--quiet"]);
    std::fs::write(repo.join("during.txt"), "during the turn, again\n").unwrap();

    let after = wait_for_snapshot(&mut turn_rx, "post-gc turn snapshot", |s| {
        !s.files.is_empty()
    })
    .await;
    assert!(
        matches!(after.basis, DiffBasis::Turn { .. }),
        "the basis must survive a gc now that it is pinned: {:?}",
        after.basis
    );
    assert_eq!(paths(&after), vec!["during.txt".to_string()]);

    let _ = c.unsubscribe(turn_id).await;
    drop(engine);
}

/// The other half, still true and still needed: the operator owns their
/// repository. With eave's refs removed, the base really is collectable —
/// and eave states that in words instead of reporting a git failure, or,
/// worse, an empty list that reads as "this turn changed nothing".
#[tokio::test]
async fn a_base_the_operator_unpinned_is_a_stated_fact() {
    let dir = scratch();
    let repo = repo();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(repo.display().to_string()))
        .await
        .expect("open");
    // The base has to be UNREACHABLE for a gc to collect it, and a
    // capture of a pristine checkout is byte-identical to HEAD's own tree
    // — reachable, and therefore safe. Dirtying the tree first is what
    // makes the captured tree a genuine orphan, which is the state this
    // whole risk is about.
    std::fs::write(repo.join("pre.txt"), "dirty before the turn\n").unwrap();
    assert!(c.send("s1", "go").await.expect("send"));
    std::fs::write(repo.join("during.txt"), "during the turn\n").unwrap();

    let (turn_id, mut turn_rx) = c
        .files_with_id_scoped("s1", FileScope::LatestTurn)
        .await
        .expect("files latest_turn");
    let before = wait_for_snapshot(&mut turn_rx, "pre-gc turn snapshot", |s| {
        matches!(s.basis, DiffBasis::Turn { .. }) && !s.files.is_empty()
    })
    .await;
    assert_eq!(paths(&before), vec!["during.txt".to_string()]);

    // The operator (or a script of theirs) sweeps eave's refs, and THEN
    // housekeeping runs. Only now is the base a genuine orphan.
    let refs = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/eave/"])
        .current_dir(&repo)
        .output()
        .expect("list eave refs");
    let listed = String::from_utf8_lossy(&refs.stdout);
    assert!(
        listed.contains("refs/eave/"),
        "the send path must have pinned a checkpoint to unpin: {listed:?}"
    );
    for name in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
        git(&repo, &["update-ref", "-d", name]);
    }
    git(&repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&repo, &["gc", "--prune=now", "--quiet"]);
    // Nudge a rescan (the base is only consulted when one runs).
    std::fs::write(repo.join("during.txt"), "during the turn, again\n").unwrap();

    let after = wait_for_snapshot(&mut turn_rx, "post-gc turn snapshot", |s| {
        matches!(s.basis, DiffBasis::Unavailable { .. })
    })
    .await;
    let DiffBasis::Unavailable { reason } = &after.basis else {
        unreachable!("selected by the predicate")
    };
    assert!(
        reason.contains("gc") && reason.contains("checkpoint ref"),
        "the reason must name BOTH things that had to happen: {reason:?}"
    );
    assert!(after.files.is_empty());
    assert_eq!(after.state, GitState::Ok);

    let _ = c.unsubscribe(turn_id).await;
    drop(engine);
}

/// The capture is invisible to the operator's own git. `git add -A` into
/// a throwaway `GIT_INDEX_FILE` in eave's data dir leaves `git status
/// --porcelain` BYTE-identical — including the staged/unstaged split,
/// which is the part a careless implementation destroys.
///
/// Not a hypothetical: put the same index file inside the work tree and
/// `add -A` sweeps up the index and its lock into the tree it is
/// capturing, and status grows two entries. That is why the path is a
/// parameter and why this test pins the property rather than the code.
#[tokio::test]
async fn snapshot_tree_never_touches_the_real_index() {
    let repo = repo();
    // Deliberately mixed: one staged change, one unstaged change, one
    // untracked file. A capture that wrote the real index would flatten
    // all three into "staged".
    std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("base.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();

    let porcelain = |cwd: &Path| -> Vec<u8> {
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .expect("git status")
            .stdout
    };
    let before = porcelain(&repo);
    assert!(
        !before.is_empty(),
        "the fixture must be dirty to prove this"
    );

    // The index lives in eave's data dir — never /tmp, never the repo.
    let data_dir = scratch();
    let index = data_dir.join("sessions").join("s1").join("turn-base.index");
    let tree = eave_engine::git::snapshot_tree(&repo, &index)
        .await
        .expect("capture the working tree");

    let after = porcelain(&repo);
    assert_eq!(
        String::from_utf8_lossy(&before),
        String::from_utf8_lossy(&after),
        "snapshot_tree must leave `git status --porcelain` byte-identical"
    );
    assert!(
        index.starts_with(&data_dir),
        "the throwaway index must live in eave's data dir: {index:?}"
    );

    // Positive control: the tree really did capture the untracked file,
    // so "status unchanged" is not just "nothing happened".
    let listing = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", &tree])
        .current_dir(&repo)
        .output()
        .expect("git ls-tree");
    let listing = String::from_utf8_lossy(&listing.stdout);
    for expected in ["base.txt", "staged.txt", "untracked.txt"] {
        assert!(
            listing.lines().any(|l| l == expected),
            "capture missed {expected}: {listing:?}"
        );
    }
    // And nothing of eave's own scratch leaked into it.
    assert!(
        !listing.contains("turn-base.index"),
        "the throwaway index leaked into the captured tree: {listing:?}"
    );
}

/// **Slice AH — the engine's half of the eternal skeleton.**
///
/// The UI's rail draws skeleton rows until a `files` frame arrives, and
/// before this slice that wait was unbounded: an unanswered subscription
/// held a 30 fps pulse lease for the life of the process. The right place
/// to fix that is wherever the answer can be made to exist, so the first
/// question was whether the engine can legitimately stay silent — a
/// timeout in the UI invents a fact, an answer from the engine states
/// one.
///
/// This is that question asked directly, across the cross product of the
/// two scopes and the three cwds a session can have. `non_git_and_broken_
/// cwds_are_stated_facts` covers the working-tree scope's half; the
/// turn scope's half was never asked, and it is the interesting one,
/// because the turn cache starts EMPTY (`files_turn: None`) and is filled
/// only while somebody is subscribed at that scope. A subscriber that
/// arrived before the first turn, on a session that has never run, is
/// exactly the client the rail opens on boot.
///
/// Every cell must produce a frame. If one ever does not, the fix belongs
/// here and not in the shell.
#[tokio::test]
async fn every_files_subscription_answers_at_both_scopes_and_every_cwd() {
    let dir = scratch();
    let engine = EngineProc::spawn(&dir, "linger");
    let c = Client::connect(&dir).await.expect("connect");

    let a_repo = repo();
    let not_a_repo = scratch();
    let cases: [(&str, String); 3] = [
        ("repo", a_repo.display().to_string()),
        ("plain", not_a_repo.display().to_string()),
        // Never created. The engine has to answer about a directory that
        // is not there, which is where a naive scan would simply hang or
        // panic instead of speaking.
        ("gone", "/tmp/eave-wire-tests/ah-vanished-cwd".into()),
    ];

    for (id, cwd) in &cases {
        c.open(Some((*id).into()), Some(cwd.clone()))
            .await
            .unwrap_or_else(|e| panic!("open {id}: {e}"));
        for scope in [FileScope::WorkingTree, FileScope::LatestTurn] {
            let (_sub, mut rx) = c
                .files_with_id_scoped(id, scope)
                .await
                .unwrap_or_else(|e| panic!("files {id} {scope:?}: {e}"));
            // `wait_for_snapshot` panics past its own deadline, which is
            // the assertion: no frame is the failure.
            let snap = wait_for_snapshot(
                &mut rx,
                &format!("first frame for {id} at {scope:?}"),
                |_| true,
            )
            .await;
            // ...and the frame has to SAY something. An empty list under a
            // basis nobody could measure would render as "no changes",
            // which is the confusion Slice AA exists to prevent — so a
            // silent zero counts as no answer for this test's purposes.
            let stated = match (&snap.state, &snap.basis) {
                (GitState::NotARepo, _) => true,
                (GitState::Error { message }, _) => !message.is_empty(),
                // Slice AJ's variant, reached here by INTEGRATION rather
                // than by either branch: AH wrote this match and AJ added
                // the variant, so neither suite could have caught it alone.
                // "git did not answer" is itself an answer, and `stated()`
                // is non-empty by construction — which is exactly why the
                // wording moved into the wire type.
                (GitState::Unanswered { .. }, _) => !snap.state.stated().is_empty(),
                (_, DiffBasis::Unavailable { reason }) => !reason.is_empty(),
                (GitState::Ok, _) => true,
            };
            assert!(
                stated,
                "{id} at {scope:?} answered with a snapshot that states \
                 nothing: {snap:?}"
            );
        }
    }

    // The turn scope on a session that has never run is the boot case, and
    // its answer is words rather than an empty list.
    let (_sub, mut rx) = c
        .files_with_id_scoped("repo", FileScope::LatestTurn)
        .await
        .expect("files repo latest-turn");
    let snap = wait_for_snapshot(&mut rx, "turn basis before any turn", |_| true).await;
    assert!(
        matches!(&snap.basis, DiffBasis::Unavailable { reason } if reason.contains("turn")),
        "a turn scope with no turn behind it must say so, not report an \
         empty change list: {:?}",
        snap.basis
    );

    assert_eq!(
        c.list().await.expect("list").len(),
        3,
        "the engine survived"
    );
    drop(engine);
}
