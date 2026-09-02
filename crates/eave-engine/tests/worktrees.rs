//! Slice I2 acceptance: same-repo isolation via git worktrees.
//!
//! The decisions under test (directive, 2026-08-17): detection is
//! automatic and STATED (`shared_with` on watch/list), creation is the
//! operator's opt-in (`isolate`); eave NEVER deletes a dirty worktree —
//! removal restates freshly-measured counts and only a forced second
//! call proceeds; crash-orphaned worktrees are re-detected from `git
//! worktree list` against known sessions on restart, never from memory.
//!
//! Same harness rules as `wire.rs`/`multi.rs`: data dirs under
//! `/tmp/eave-wire-tests` (`sun_path` limit), workspace-built bins,
//! `EAVE_WORKTREES_DIR` pointed at per-test scratch.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use eave_core::proc::{self, ProcIdentity};
use eave_core::store::Meta;
use eave_engine::client::Client;
use eave_wire::model::{Row, Transcript};
use eave_wire::{socket_path, SessionInfo, SubscriptionItem};

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

fn git(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A git repo with one committed file — the ONE repo the sessions under
/// test share (I2 is the same-repo case).
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
    fn spawn_with_env(data_dir: &Path, fake_mode: &str, envs: &[(&str, &str)]) -> Self {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("engine-test.log"))
            .unwrap();
        let mut cmd = Command::new(engined_bin());
        cmd.args(["--data-dir", &data_dir.display().to_string()])
            // A test engine must never inherit the DEVELOPER'S checkout as
            // its default cwd: a session opened without one would then be
            // a session on the real repo, and since AG1 #11 that means a
            // checkpoint ref written into the shared git dir on every
            // send. (It also hashed the whole eave work tree for nothing.)
            .current_dir(data_dir)
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

/// Poll `list` until the predicate holds (the checkout probe and scans
/// run off-loop, so sharing/worktree facts land asynchronously).
async fn wait_list<F>(c: &Client, what: &str, pred: F) -> Vec<SessionInfo>
where
    F: Fn(&[SessionInfo]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let sessions = c.list().await.expect("list");
        if pred(&sessions) {
            return sessions;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last list: {sessions:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn by_id<'a>(sessions: &'a [SessionInfo], id: &str) -> &'a SessionInfo {
    sessions
        .iter()
        .find(|s| s.session_id == id)
        .unwrap_or_else(|| panic!("{id} missing from list"))
}

/// Wait until the session's on-disk journal contains `needle` (the tap
/// runs ahead of the 120 ms flush — crash tests must wait for disk).
fn wait_for_journal(data_dir: &Path, sid: &str, needle: &str) {
    let path = data_dir.join("sessions").join(sid).join("journal.jsonl");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::fs::read_to_string(&path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "journal for {sid} never contained {needle:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn drain_records(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    transcript: &mut Transcript,
    min_records: usize,
    deadline: Duration,
) -> Vec<eave_core::journal::Record> {
    let mut got = Vec::new();
    let end = Instant::now() + deadline;
    while got.len() < min_records {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for records (have {})",
            got.len()
        );
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("subscription item in time")
            .expect("subscription stream ended unexpectedly");
        match item {
            SubscriptionItem::Reset { .. } => transcript.reset(),
            SubscriptionItem::Events { epoch, records, .. } => {
                transcript.apply(epoch, &records).expect("no seq gaps");
                got.extend(records);
            }
            SubscriptionItem::Attention { .. }
            | SubscriptionItem::Sessions { .. }
            | SubscriptionItem::Files { .. } => {}
            // Slice B1: browser frames flow on their own subscription
            // ids and are demultiplexed by the client, so one arriving
            // here would mean the demux itself broke. Say so loudly
            // rather than drop it into the ignore arm above.
            SubscriptionItem::Tabs { .. }
            | SubscriptionItem::Bookmarks { .. }
            | SubscriptionItem::Permissions { .. }
            // Slice AG1 #16: a handed-over session's pty rides its own
            // `term_attach` stream, never this one.
            | SubscriptionItem::TermScreen { .. }
            | SubscriptionItem::TermOutput { .. }
            | SubscriptionItem::TermExited { .. } => {
                panic!("a browser frame arrived on a session subscription")
            }
        }
    }
    got
}

fn transcript_texts(t: &Transcript) -> Vec<String> {
    t.rows
        .iter()
        .filter_map(|r| match r {
            Row::User { text, .. } => Some(text.clone()),
            Row::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Decision 1: detection automatic + stated; creation opt-in

/// Two sessions opened in ONE repo: both rows state the sharing, no
/// worktree machinery is invoked (shared-by-choice is legitimate). The
/// operator isolates the second: a worktree appears on the stated scheme
/// (`<root>/<repo>/<sid8>`, branch `eave/<sid8>`), the session's cwd is
/// rewritten to it, and the sharing fact clears on both rows.
#[tokio::test]
async fn sharing_is_stated_and_isolate_is_the_opt_in() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open s1");
    c.open(Some("s2".into()), Some(base.display().to_string()))
        .await
        .expect("open s2");

    // Detection is automatic: the sharing fact lands on both, stated.
    let listed = wait_list(&c, "mutual sharing", |s| {
        s.len() == 2 && s.iter().all(|i| i.shared_with.len() == 1)
    })
    .await;
    assert_eq!(by_id(&listed, "s1").shared_with, vec!["s2".to_string()]);
    assert_eq!(by_id(&listed, "s2").shared_with, vec!["s1".to_string()]);
    // Shared-by-choice invokes NO machinery: nothing under the root.
    assert_eq!(
        std::fs::read_dir(&wtroot).unwrap().count(),
        0,
        "no worktree may exist before the operator opts in"
    );

    // The opt-in.
    let info = c.isolate("s2").await.expect("isolate s2");
    let repo_name = base.file_name().unwrap().to_string_lossy().into_owned();
    let expected_wt = wtroot.join(&repo_name).join("s2");
    assert_eq!(info.cwd, expected_wt.display().to_string());
    let wt = info.worktree.expect("worktree fact");
    assert_eq!(wt.branch, "eave/s2");
    assert!(wt.present);
    assert!(
        expected_wt.join("base.txt").exists(),
        "checkout materialized"
    );

    // git agrees: base + one linked worktree; the sharing fact clears.
    let listed_git = git(&base, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed_git
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        2,
        "{listed_git}"
    );
    wait_list(&c, "sharing cleared", |s| {
        s.iter().all(|i| i.shared_with.is_empty())
    })
    .await;
    assert!(
        dir.join("sessions")
            .join("s2")
            .join("worktree.json")
            .exists(),
        "durable claim written"
    );
}

/// Slice ID2b: the PROJECT key. A session in the base checkout has
/// `project_root == checkout_root` — it is the primary task — and an
/// isolated session, whose `checkout_root` is its worktree directory,
/// still names the base repository as its project. Before this field the
/// rail grouped on `checkout_root` alone and drew one repository as two
/// headers (`<repo>` and `<sid8>`); this is the fact that lets it draw one.
///
/// Both stamps are read off `list`, i.e. the same probe that stamps
/// `checkout_root`, one field over — and on the isolated session it is
/// present on the very reply that announces the worktree, not a poll
/// later.
#[tokio::test]
async fn an_isolated_session_names_the_base_repository_as_its_project() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("p1".into()), Some(base.display().to_string()))
        .await
        .expect("open p1");
    c.open(Some("w1".into()), Some(base.display().to_string()))
        .await
        .expect("open w1");
    let canon_base = std::fs::canonicalize(&base).unwrap();
    let listed = wait_list(&c, "both probed", |s| {
        s.len() == 2 && s.iter().all(|i| i.project_root.is_some())
    })
    .await;
    for id in ["p1", "w1"] {
        let info = by_id(&listed, id);
        assert_eq!(
            info.project_root.as_deref(),
            Some(canon_base.to_str().unwrap()),
            "{id}: the base checkout is its own project"
        );
        assert_eq!(info.project_root, info.checkout_root, "{id}: primary");
    }

    let info = c.isolate("w1").await.expect("isolate w1");
    assert_ne!(
        info.checkout_root, info.project_root,
        "an isolated session's task is its worktree, not the base"
    );
    assert_eq!(
        info.project_root.as_deref(),
        Some(canon_base.to_str().unwrap()),
        "the isolate reply already files the worktree under its repository"
    );
    // And after the off-loop probe re-runs, git agrees with the stamp.
    let listed = wait_list(&c, "worktree probed", |s| {
        by_id(s, "w1").worktree.is_some()
            && by_id(s, "w1").project_root.as_deref() == Some(canon_base.to_str().unwrap())
    })
    .await;
    let w1 = by_id(&listed, "w1");
    assert_eq!(
        w1.checkout_root.as_deref(),
        Some(std::fs::canonicalize(&w1.cwd).unwrap().to_str().unwrap()),
        "the task key is still the worktree directory"
    );
}

/// Slice ID2a: the first title an isolated session gets names its
/// branch, ONCE; a title applied before isolation names it at creation;
/// and a worktree whose claim predates the rule keeps its name whatever
/// the session is called.
///
/// The name comes from the TITLE the operator applied, never from the
/// transcript — `WorktreeInfo::branch` rides `watch`, and this is what
/// keeps `watch_channel_never_carries_transcript_content` true with a
/// branch on the row.
#[tokio::test]
async fn the_operators_title_names_the_branch_once_and_old_claims_keep_theirs() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    for sid in ["s1", "s2", "s3"] {
        c.open(Some(sid.into()), Some(base.display().to_string()))
            .await
            .expect("open");
    }

    // s1: isolated unnamed, then named. The branch follows the name.
    let info = c.isolate("s1").await.expect("isolate s1");
    assert_eq!(info.worktree.as_ref().unwrap().branch, "eave/s1");
    let info = c
        .rename("s1", Some("Diagnose: OMP chat blanking!"))
        .await
        .expect("rename s1");
    let wt = info.worktree.expect("worktree fact");
    assert_eq!(wt.branch, "eave/diagnose-omp-chat-blanking");
    assert!(wt.present);
    // git agrees, and the worktree's HEAD moved with the ref.
    let head = git(Path::new(&info.cwd), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(head.trim(), "eave/diagnose-omp-chat-blanking");
    assert!(
        git(&base, &["branch", "--list", "eave/s1"])
            .trim()
            .is_empty(),
        "the opaque branch is still there"
    );

    // Once: a correction of the title leaves the branch where other tools
    // can find it.
    let info = c
        .rename("s1", Some("blanking, round two"))
        .await
        .expect("rename again");
    assert_eq!(
        info.worktree.unwrap().branch,
        "eave/diagnose-omp-chat-blanking",
        "a second rename moved the branch"
    );
    // ...and clearing the name is not a reason to move it either.
    let info = c.rename("s1", None).await.expect("clear");
    assert_eq!(
        info.worktree.unwrap().branch,
        "eave/diagnose-omp-chat-blanking"
    );

    // s2: named BEFORE isolation opens straight onto the slug — and a
    // slug already taken falls back to `-<sid8>` rather than refusing.
    c.rename("s2", Some("Diagnose OMP chat blanking"))
        .await
        .expect("rename s2");
    let info = c.isolate("s2").await.expect("isolate s2");
    assert_eq!(
        info.worktree.unwrap().branch,
        "eave/s2",
        "the slug is s1's; the opaque name is the honest fallback at creation"
    );
    // A later title on that opaque branch tries the slug, finds it taken,
    // and takes the slug with the session id behind it.
    let info = c
        .rename("s2", Some("Diagnose OMP chat blanking"))
        .await
        .expect("rename s2");
    assert_eq!(
        info.worktree.unwrap().branch,
        "eave/diagnose-omp-chat-blanking-s2"
    );

    // s3: a version-1 claim — a worktree from before the rule — keeps
    // its opaque name whatever the session is called, across a restart.
    c.isolate("s3").await.expect("isolate s3");
    drop(c);
    drop(engine);
    let claim = dir.join("sessions").join("s3").join("worktree.json");
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&claim).unwrap()).unwrap();
    assert_eq!(v["format_version"], 2, "new claims are version 2");
    v["format_version"] = serde_json::json!(1);
    std::fs::write(&claim, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("reconnect");
    let info = c
        .rename("s3", Some("an old worktree"))
        .await
        .expect("rename s3");
    assert_eq!(
        info.worktree.unwrap().branch,
        "eave/s3",
        "an existing worktree's branch was renamed"
    );
    // And s1's moved branch survived the restart as the session's claim.
    let listed = wait_list(&c, "s1 reloaded", |s| {
        s.iter().any(|i| i.session_id == "s1")
    })
    .await;
    assert_eq!(
        by_id(&listed, "s1").worktree.as_ref().unwrap().branch,
        "eave/diagnose-omp-chat-blanking"
    );
}

/// Refusal shapes are stated, never crashes: after the first send the
/// cwd is fixed; a non-git cwd has no worktree to offer; isolation is
/// not repeatable.
#[tokio::test]
async fn isolate_refusals_are_stated() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let plain = scratch();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "tool-cycle",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");

    // (a) already started.
    c.open(Some("sa".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    assert!(c.send("sa", "hello").await.expect("send"));
    // Depending on when the tool-cycle child exits, the stated reason is
    // either "live" or "already started" — both are the same rule (cwd
    // fixed once anything ran), both refuse.
    let err = c.isolate("sa").await.expect_err("must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("already started") || msg.contains("session is live"),
        "{msg}"
    );

    // (b) non-git cwd: stated, shared-only.
    c.open(Some("sb".into()), Some(plain.display().to_string()))
        .await
        .expect("open");
    let err = c.isolate("sb").await.expect_err("must refuse");
    assert!(
        err.to_string().contains("not inside a git repository"),
        "{err}"
    );

    // (c) not repeatable.
    c.open(Some("sc".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    c.isolate("sc").await.expect("first isolate");
    let err = c.isolate("sc").await.expect_err("must refuse");
    assert!(err.to_string().contains("already isolated"), "{err}");
}

// ---------------------------------------------------------------------------
// Isolation actually isolates: one repo, two worktrees, nothing crosses

/// Two sessions isolated out of ONE repo: their streams carry only their
/// own canaries, their `files` channels list only their own worktree's
/// files (the B2 leak discipline extended to same-repo worktrees), and
/// each agent process provably runs IN its own worktree.
#[tokio::test]
async fn two_isolated_sessions_in_one_repo_do_not_cross() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    for sid in ["s1", "s2"] {
        c.open(Some(sid.into()), Some(base.display().to_string()))
            .await
            .expect("open");
        c.isolate(sid).await.expect("isolate");
    }
    let repo_name = base.file_name().unwrap().to_string_lossy().into_owned();
    let wt1 = wtroot.join(&repo_name).join("s1");
    let wt2 = wtroot.join(&repo_name).join("s2");

    // Concurrent writes land in separate worktrees by construction —
    // prove the rails see them separately. Filename canaries, the same
    // leak vector files.rs guards.
    std::fs::write(wt1.join("CANARY_FILE_S1_e7a1.txt"), "w1\n").unwrap();
    std::fs::write(wt2.join("CANARY_FILE_S2_b2c9.txt"), "w2\n").unwrap();
    assert!(c.send("s1", "CANARY_TEXT_S1_44aa").await.expect("send s1"));
    assert!(c.send("s2", "CANARY_TEXT_S2_55bb").await.expect("send s2"));

    // Records: own canary only, full replay each.
    for (sid, own, other) in [
        ("s1", "CANARY_TEXT_S1_44aa", "CANARY_TEXT_S2_55bb"),
        ("s2", "CANARY_TEXT_S2_55bb", "CANARY_TEXT_S1_44aa"),
    ] {
        let mut rx = c.subscribe(sid, None).await.expect("subscribe");
        let mut t = Transcript::new();
        // The recorder's extra (UserSent) and provider channels are
        // unordered (progress.md trap): keep draining until the own
        // canary lands rather than counting records.
        let deadline = Instant::now() + Duration::from_secs(15);
        let texts = loop {
            drain_records(&mut rx, &mut t, 1, Duration::from_secs(15)).await;
            let texts = transcript_texts(&t);
            if texts.iter().any(|x| x.contains(own)) {
                break texts;
            }
            assert!(
                Instant::now() < deadline,
                "{sid} never showed its own canary: {texts:?}"
            );
        };
        assert!(
            !texts.iter().any(|x| x.contains(other)),
            "{sid} leaked a foreign canary: {texts:?}"
        );
    }

    // Files channels on raw bytes: every line that arrives on each
    // subscription is checked against the OTHER worktree's canary.
    let mut raw = RawConn::connect(&dir).await;
    raw.send(r#"{"id":40,"method":"files","params":{"session_id":"s1"}}"#)
        .await;
    raw.send(r#"{"id":41,"method":"files","params":{"session_id":"s2"}}"#)
        .await;
    let lines = raw.collect_for(Duration::from_secs(4)).await;
    let mut saw = (false, false);
    for line in &lines {
        let id = frame_id(line);
        if line.contains("CANARY_FILE_S1_e7a1") {
            assert_eq!(id, 40, "s1's file crossed to id {id}: {line}");
            saw.0 = true;
        }
        if line.contains("CANARY_FILE_S2_b2c9") {
            assert_eq!(id, 41, "s2's file crossed to id {id}: {line}");
            saw.1 = true;
        }
    }
    assert!(
        saw.0 && saw.1,
        "positive controls: both canaries must appear once each on their own id ({lines:?})"
    );

    // Each agent runs IN its worktree: the linger children's cwds.
    for (sid, wt) in [("s1", &wt1), ("s2", &wt2)] {
        let meta = Meta::load(&dir.join("sessions").join(sid)).expect("meta");
        let child = meta.child.expect("live child");
        let out = Command::new("lsof")
            .args(["-a", "-d", "cwd", "-p", &child.pid.to_string(), "-Fn"])
            .output()
            .expect("lsof");
        let cwd_line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.starts_with('n'))
            .map(str::to_string)
            .unwrap_or_default();
        let suffix = format!("{}/{}", repo_name, sid);
        assert!(
            cwd_line.ends_with(&suffix),
            "{sid} agent cwd {cwd_line:?} is not its worktree {wt:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Decision 2 + lifecycle: dirty is protected; clean removal is complete

/// Clean removal leaves no trace: worktree gone, `git worktree list`
/// back to one, the `eave/` branch deleted, the durable claim gone, the
/// rail fact gone. The session itself remains (history is never touched).
#[tokio::test]
async fn clean_removal_leaves_no_trace() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let wt_path = PathBuf::from(&info.cwd);

    let ok = c.remove_worktree("s1", false).await.expect("remove");
    assert!(ok.removed, "clean worktree removes without force: {ok:?}");
    assert_eq!(ok.dirty_files, Some(0));
    assert_eq!(ok.unmerged_commits, Some(0));
    assert!(!wt_path.exists());
    let listed = git(&base, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "{listed}"
    );
    assert_eq!(
        git(&base, &["branch", "--list", "eave/*"]).trim(),
        "",
        "no stale eave/ branch"
    );
    assert!(!dir
        .join("sessions")
        .join("s1")
        .join("worktree.json")
        .exists());
    let sessions = wait_list(&c, "worktree fact cleared", |s| {
        by_id(s, "s1").worktree.is_none()
    })
    .await;
    assert_eq!(sessions.len(), 1, "the session itself remains");
}

/// Dirty removal is a refusal restating fresh counts; only the forced
/// second call proceeds. Eave never deletes dirty work on one click.
#[tokio::test]
async fn dirty_removal_requires_force_and_restates_counts() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let wt_path = PathBuf::from(&info.cwd);
    std::fs::write(wt_path.join("work.txt"), "uncommitted\n").unwrap();
    std::fs::write(wt_path.join("more.txt"), "also\n").unwrap();

    let refusal = c.remove_worktree("s1", false).await.expect("reply");
    assert!(!refusal.removed);
    assert_eq!(refusal.dirty_files, Some(2), "fresh count, restated");
    assert!(wt_path.exists(), "nothing deleted on refusal");

    let forced = c.remove_worktree("s1", true).await.expect("forced");
    assert!(forced.removed);
    assert!(!wt_path.exists());
    assert_eq!(git(&base, &["branch", "--list", "eave/*"]).trim(), "");
}

/// The operator deletes the worktree out from under eave: the fact is
/// stated (`present: false`, files channel errors), nothing is
/// recreated, and removal then completes the bookkeeping (prune + branch).
#[tokio::test]
async fn worktree_deleted_out_from_under_is_stated_then_cleaned() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let wt_path = PathBuf::from(&info.cwd);

    std::fs::remove_dir_all(&wt_path).unwrap();
    // A files subscription triggers the scan that discovers it.
    let _rx = c.files("s1").await.expect("files");
    wait_list(&c, "missing worktree stated", |s| {
        by_id(s, "s1").worktree.as_ref().is_some_and(|w| !w.present)
    })
    .await;
    assert!(!wt_path.exists(), "eave never recreates a deleted worktree");

    let ok = c.remove_worktree("s1", false).await.expect("remove");
    assert!(ok.removed, "nothing left to protect: {ok:?}");
    let listed = git(&base, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "pruned: {listed}"
    );
    assert_eq!(git(&base, &["branch", "--list", "eave/*"]).trim(), "");
}

// ---------------------------------------------------------------------------
// Crash rot: the part the directive says rots

/// Engine SIGKILLed with a dirty worktree session mid-turn. The restart
/// re-detects the worktree from `git worktree list` + the durable claim,
/// states the orphan with its dirty count on list/watch, keeps the
/// rewritten cwd, and deletes NOTHING.
#[tokio::test]
async fn crash_orphan_is_redetected_with_dirty_count() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let wtroot_s = wtroot.display().to_string();
    let mut engine =
        EngineProc::spawn_with_env(&dir, "linger", &[("EAVE_WORKTREES_DIR", &wtroot_s)]);
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let wt_path = PathBuf::from(&info.cwd);
    assert!(c.send("s1", "working").await.expect("send"));
    wait_for_journal(&dir, "s1", "user_delivered");
    // The dirty work a crash must not eat.
    std::fs::write(wt_path.join("uncommitted.txt"), "precious\n").unwrap();

    engine.sigkill();
    drop(c);
    let _engine2 = EngineProc::spawn_with_env(&dir, "linger", &[("EAVE_WORKTREES_DIR", &wtroot_s)]);
    let c2 = Client::connect(&dir).await.expect("reconnect");
    let sessions = wait_list(&c2, "orphan restated with dirty count", |s| {
        by_id(s, "s1")
            .worktree
            .as_ref()
            .is_some_and(|w| w.present && w.dirty_files.is_some_and(|n| n >= 1))
    })
    .await;
    let s1 = by_id(&sessions, "s1");
    assert_eq!(s1.state, "interrupted", "crash repair settled the state");
    assert_eq!(
        s1.cwd,
        wt_path.display().to_string(),
        "cwd rewrite survived"
    );
    assert!(wt_path.join("uncommitted.txt").exists(), "nothing deleted");
    assert_eq!(
        s1.worktree.as_ref().unwrap().branch,
        "eave/s1",
        "identity re-derived, not remembered"
    );
}

/// Stage L: the startup sweep has always found worktrees no session
/// claims and has always left them alone — correctly — but it said so
/// only on engine stderr, where nothing reads it. Now it also writes a
/// file in the data dir, and **removes that file** when the sweep is
/// clean, so the report can never outlive the thing it reports.
#[tokio::test]
async fn an_unclaimed_worktree_is_stated_where_it_can_be_found() {
    let dir = scratch();
    let wtroot = scratch();
    let wtroot_s = wtroot.display().to_string();
    let report = dir.join("unclaimed-worktrees.txt");

    // A directory under the worktrees root that no session will ever own
    // — the shape a crashed `isolate` or a hand-deleted session dir
    // leaves behind.
    let orphan = wtroot.join("somerepo").join("dead0000");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("precious.txt"), "not yours to delete\n").unwrap();

    {
        let _engine =
            EngineProc::spawn_with_env(&dir, "linger", &[("EAVE_WORKTREES_DIR", &wtroot_s)]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !report.exists() {
            assert!(
                Instant::now() < deadline,
                "the sweep never stated its finding"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let body = std::fs::read_to_string(&report).unwrap();
        let canon = std::fs::canonicalize(&orphan).unwrap();
        assert!(
            body.contains(&canon.display().to_string()),
            "the report must name the directory; got:\n{body}"
        );
        assert!(
            body.contains("git worktree prune"),
            "and must say what to do about it, since eave will not: {body}"
        );
        assert!(
            orphan.join("precious.txt").exists(),
            "stating is not touching — the sweep must never delete"
        );
    }

    // Clean the orphan up the way an operator would, restart, and the
    // report must be GONE rather than left standing as a stale list.
    std::fs::remove_dir_all(wtroot.join("somerepo")).unwrap();
    let _engine2 = EngineProc::spawn_with_env(&dir, "linger", &[("EAVE_WORKTREES_DIR", &wtroot_s)]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while report.exists() {
        assert!(
            Instant::now() < deadline,
            "a clean sweep left a stale report behind — it would send \
             someone looking for a directory that no longer exists"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Provider cwd: the agent spawns (and respawns via --resume) in the worktree

/// `cwd-report` states the agent's own cwd as its one turn. Both the
/// fresh spawn and the `--resume` respawn after the segment ends must
/// report the WORKTREE, not the base repo.
#[tokio::test]
async fn agent_spawns_and_resumes_in_the_worktree() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "cwd-report",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let repo_name = base.file_name().unwrap().to_string_lossy().into_owned();
    let suffix = format!("{repo_name}/s1");
    assert!(info.cwd.ends_with(&suffix));

    let cwd_texts = |t: &Transcript| -> Vec<String> {
        transcript_texts(t)
            .into_iter()
            .filter(|x| x.ends_with(&suffix))
            .collect()
    };

    assert!(c.send("s1", "where are you").await.expect("send 1"));
    wait_list(&c, "first segment done", |s| {
        let i = by_id(s, "s1");
        i.state == "done" && !i.live
    })
    .await;
    // Second send on the terminal session: the engine respawns with
    // `--resume` (spec 02) — still in the worktree.
    assert!(c.send("s1", "and now").await.expect("send 2"));
    wait_list(&c, "second segment done", |s| {
        let i = by_id(s, "s1");
        i.state == "done" && !i.live
    })
    .await;

    let mut rx = c.subscribe("s1", None).await.expect("subscribe");
    let mut t = Transcript::new();
    drain_records(&mut rx, &mut t, 8, Duration::from_secs(15)).await;
    let reports = cwd_texts(&t);
    assert_eq!(
        reports.len(),
        2,
        "both segments must report the worktree cwd: {:?}",
        transcript_texts(&t)
    );
}

// ---------------------------------------------------------------------------
// Watcher scope: a linked worktree's git dir lives OUTSIDE the worktree

/// `git add`/`git commit` inside a linked worktree mutate the base
/// repo's private stanza (`.git/worktrees/<name>` — index, HEAD), not
/// the watched cwd. The I2 watcher covers that dir too: a commit must
/// refresh the files rail to clean with NO turn boundary anywhere.
#[tokio::test]
async fn commits_in_the_worktree_refresh_the_rail_without_a_turn() {
    let dir = scratch();
    let wtroot = scratch();
    let base = repo();
    let _engine = EngineProc::spawn_with_env(
        &dir,
        "linger",
        &[("EAVE_WORKTREES_DIR", &wtroot.display().to_string())],
    );
    let c = Client::connect(&dir).await.expect("connect");
    c.open(Some("s1".into()), Some(base.display().to_string()))
        .await
        .expect("open");
    let info = c.isolate("s1").await.expect("isolate");
    let wt_path = PathBuf::from(&info.cwd);

    let mut rx = c.files("s1").await.expect("files");
    // Wait until the watcher is live (stamped per frame) so what follows
    // is proven event-driven, not the open-time poll.
    next_files(&mut rx, "watcher live", |s| s.watching).await;

    std::fs::write(wt_path.join("staged.txt"), "w\n").unwrap();
    next_files(&mut rx, "write seen", |s| {
        s.files.iter().any(|f| f.path == "staged.txt")
    })
    .await;

    // No sends ever happen in this test: there is no turn boundary to
    // poll on. Only the watcher can deliver this.
    git(&wt_path, &["add", "."]);
    git(&wt_path, &["commit", "-qm", "wt commit"]);
    next_files(&mut rx, "commit seen (clean again)", |s| {
        s.state == eave_wire::GitState::Ok && s.files.is_empty()
    })
    .await;
}

async fn next_files<F>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
    what: &str,
    pred: F,
) -> eave_wire::FilesSnapshot
where
    F: Fn(&eave_wire::FilesSnapshot) -> bool,
{
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        let item = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("files stream ended waiting for {what}"));
        if let SubscriptionItem::Files { snapshot, .. } = item {
            if pred(&snapshot) {
                return snapshot;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Raw-socket helpers (the leak assertions run on actual bytes)

struct RawConn {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl RawConn {
    async fn connect(dir: &Path) -> Self {
        let stream = tokio::net::UnixStream::connect(socket_path(dir))
            .await
            .expect("connect raw");
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        w.write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"proto\":1}}\n")
            .await
            .unwrap();
        let hello = lines.next_line().await.unwrap().expect("hello reply");
        assert!(hello.contains("\"ok\""), "{hello}");
        Self { lines, w }
    }

    async fn send(&mut self, raw: &str) {
        self.w.write_all(raw.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    /// Collect every line for the window (no assertion on count).
    async fn collect_for(&mut self, window: Duration) -> Vec<String> {
        let mut out = Vec::new();
        let end = Instant::now() + window;
        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return out;
            }
            match tokio::time::timeout(remaining, self.lines.next_line()).await {
                Err(_) => return out,
                Ok(Ok(Some(line))) => out.push(line),
                Ok(_) => return out,
            }
        }
    }
}

fn frame_id(line: &str) -> u64 {
    serde_json::from_str::<eave_wire::ResponseEnvelope>(line)
        .map(|e| e.id)
        .unwrap_or(0)
}
