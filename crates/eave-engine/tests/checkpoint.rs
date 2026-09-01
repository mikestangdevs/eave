//! Turn checkpoints (Slice AG1 #11) — the git half.
//!
//! These tests are about the *object store*, not the wire: what survives,
//! what gets written back, and what is deliberately left alone. The
//! engine-level behaviour (when a checkpoint is taken, what the transcript
//! says) is pinned in `wire.rs` and the app harness.

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo() -> PathBuf {
    let dir = scratch();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("base.txt"), "one\ntwo\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    dir
}

fn index_for(name: &str) -> PathBuf {
    scratch().join("sessions").join(name).join("turn.index")
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// **The reason this slice exists.**
///
/// `snapshot_tree` alone leaves a tree nothing points at, and `git.rs`
/// already documents that a `gc --prune=now` collects it — a fair trade
/// for a one-turn measurement basis, and fatal for something an operator
/// restores from later. This test runs exactly that gc and asserts the
/// two behave differently: the bare tree is gone, the checkpoint is not.
///
/// If this ever fails, checkpoints have silently become unrestorable and
/// nothing else in the suite would notice, because every other test makes
/// its checkpoint and uses it in the same breath.
#[tokio::test]
async fn a_checkpoint_survives_the_gc_that_collects_a_bare_snapshot() {
    let repo = repo();
    std::fs::write(repo.join("base.txt"), "changed\n").unwrap();

    let bare = eave_engine::git::snapshot_tree(&repo, &index_for("bare"))
        .await
        .expect("bare capture");
    let kept = eave_engine::git::capture_checkpoint(&repo, &index_for("kept"), "s1")
        .await
        .expect("checkpoint capture");

    // Same content, so git stored ONE tree object. The point is not that
    // the shas differ — it is that only one of them is reachable.
    assert_eq!(bare, kept.tree, "both captures describe the same worktree");
    assert!(eave_engine::git::checkpoint_alive(&repo, &bare).await);

    git(&repo, &["gc", "--prune=now", "-q"]);

    assert!(
        eave_engine::git::checkpoint_alive(&repo, &kept.tree).await,
        "the ref must hold the checkpoint's tree through a gc"
    );
    assert_eq!(
        git_out(&repo, &["rev-parse", &kept.git_ref]),
        kept.commit,
        "the ref must still name the checkpoint commit"
    );

    // And the control: with the ref removed, the very same tree goes.
    git(&repo, &["update-ref", "-d", &kept.git_ref]);
    git(&repo, &["gc", "--prune=now", "-q"]);
    assert!(
        !eave_engine::git::checkpoint_alive(&repo, &bare).await,
        "unreferenced, the identical tree is collected — which is what \
         the turn basis has always been exposed to"
    );
}

/// A restore is a three-way job and each third fails differently if you
/// get it wrong: a modified file must go back, a created file must go
/// away, and a deleted file must come back.
#[tokio::test]
async fn a_restore_puts_back_every_kind_of_change() {
    let repo = repo();
    std::fs::write(repo.join("kept.txt"), "original\n").unwrap();
    std::fs::write(repo.join("doomed.txt"), "delete me later\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "two more"]);

    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");

    // The turn does all three things.
    std::fs::write(repo.join("kept.txt"), "MANGLED BY THE AGENT\n").unwrap();
    std::fs::write(repo.join("invented.txt"), "the agent made this\n").unwrap();
    std::fs::remove_file(repo.join("doomed.txt")).unwrap();

    let report = eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("restore.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");

    assert_eq!(read(&repo.join("kept.txt")), "original\n");
    assert!(!repo.join("invented.txt").exists(), "created file must go");
    assert_eq!(read(&repo.join("doomed.txt")), "delete me later\n");

    let mut files = report.files.clone();
    files.sort();
    assert_eq!(files, vec!["doomed.txt", "invented.txt", "kept.txt"]);
    assert!(
        report.undo.is_some(),
        "a restore that wrote must be undoable"
    );
}

/// **The undo is taken before a byte is written, and it works.**
///
/// This is what licenses the verb at all: the same rule that lets bulk
/// archive ship while bulk delete stays absent.
#[tokio::test]
async fn a_restore_can_itself_be_undone() {
    let repo = repo();
    let before = eave_engine::git::capture_checkpoint(&repo, &index_for("a"), "s1")
        .await
        .expect("capture");

    std::fs::write(repo.join("base.txt"), "work the operator wants back\n").unwrap();
    std::fs::write(repo.join("new.txt"), "and this too\n").unwrap();

    let report = eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("b"),
        &before.tree,
        &scratch().join("p1.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");
    assert_eq!(read(&repo.join("base.txt")), "one\ntwo\n");
    assert!(!repo.join("new.txt").exists());

    // Now walk it back.
    let undo = report.undo.expect("an undo point");
    eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("c"),
        &undo.tree,
        &scratch().join("p2.patch"),
        "s1",
        true,
    )
    .await
    .expect("undo the restore");

    assert_eq!(
        read(&repo.join("base.txt")),
        "work the operator wants back\n",
        "the undo must return the state the restore replaced"
    );
    assert_eq!(read(&repo.join("new.txt")), "and this too\n");
}

/// **A restore must never delete build output.** `snapshot_tree`'s
/// `add -A` honours `.gitignore`, so an ignored path is in neither tree
/// and the patch cannot name it. Pinned because the failure mode —
/// wiping `target/` as part of an "undo" — is the worst thing this slice
/// could possibly do, and it would look like a checkout bug, not a
/// capture bug.
#[tokio::test]
async fn a_restore_leaves_ignored_files_alone() {
    let repo = repo();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-qm", "ignore target"]);
    std::fs::create_dir_all(repo.join("target")).unwrap();
    std::fs::write(repo.join("target/expensive.bin"), "hours of compiling\n").unwrap();

    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");

    std::fs::write(repo.join("base.txt"), "agent edit\n").unwrap();
    std::fs::write(repo.join("target/expensive.bin"), "rebuilt since\n").unwrap();

    let report = eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("r.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");

    assert_eq!(
        read(&repo.join("base.txt")),
        "one\ntwo\n",
        "tracked file reverts"
    );
    assert_eq!(
        read(&repo.join("target/expensive.bin")),
        "rebuilt since\n",
        "an ignored file must survive a restore untouched"
    );
    assert_eq!(report.files, vec!["base.txt"]);
}

/// eave read the working tree to make the checkpoint; it writes the
/// working tree to restore it. What is STAGED is the operator's and was
/// never ours to move — `git apply` runs without `--index` for exactly
/// this reason.
#[tokio::test]
async fn a_restore_never_moves_the_operators_index() {
    let repo = repo();
    std::fs::write(repo.join("staged.txt"), "the operator staged this\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    let staged_before = git_out(&repo, &["diff", "--cached", "--name-only"]);
    assert_eq!(
        staged_before, "staged.txt",
        "fixture must have a staged path"
    );

    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");
    std::fs::write(repo.join("base.txt"), "agent edit\n").unwrap();

    eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("r.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");

    assert_eq!(
        git_out(&repo, &["diff", "--cached", "--name-only"]),
        staged_before,
        "the operator's staged set must be byte-identical after a restore"
    );
}

/// Nothing to do is a SUCCESS with a different sentence — and it must not
/// mint an undo ref for a restore that wrote nothing.
#[tokio::test]
async fn restoring_an_unchanged_tree_writes_nothing_and_parks_nothing() {
    let repo = repo();
    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");

    let report = eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("r.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");

    assert!(report.files.is_empty());
    assert!(
        report.undo.is_none(),
        "nothing was replaced, so nothing is parked"
    );
    assert_eq!(
        git_out(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/eave/session-s1/undo-",
            ]
        ),
        "",
        "no undo ref may exist for a restore that wrote nothing"
    );
}

/// The operator owns their repository. A checkpoint whose objects they
/// removed is a STATED fact at the moment of restore — the same shape
/// `scan_against` uses for a basis that went away — never a crash and
/// never a silent no-op that looks like success.
#[tokio::test]
async fn a_collected_checkpoint_is_a_sentence_not_a_crash() {
    let repo = repo();
    std::fs::write(repo.join("base.txt"), "before\n").unwrap();
    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");

    git(&repo, &["update-ref", "-d", &cp.git_ref]);
    git(&repo, &["gc", "--prune=now", "-q"]);
    std::fs::write(repo.join("base.txt"), "after\n").unwrap();

    let err = eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("r.patch"),
        "s1",
        true,
    )
    .await
    .expect_err("a missing checkpoint cannot restore");

    assert!(
        err.contains("no longer in the repository"),
        "the refusal must say what happened: {err:?}"
    );
    assert_eq!(
        read(&repo.join("base.txt")),
        "after\n",
        "a refused restore must not have touched anything"
    );
}

/// Pruning one session's pins leaves every other session's alone.
#[tokio::test]
async fn pruning_a_sessions_pins_spares_the_others() {
    let repo = repo();
    // Different content per session, so the two cannot collide on one
    // commit-named ref and pass by accident.
    std::fs::write(repo.join("base.txt"), "mine\n").unwrap();
    let mine = eave_engine::git::capture_checkpoint(&repo, &index_for("c"), "mine")
        .await
        .expect("capture");
    std::fs::write(repo.join("base.txt"), "theirs\n").unwrap();
    let theirs = eave_engine::git::capture_checkpoint(&repo, &index_for("c"), "theirs")
        .await
        .expect("capture");

    eave_engine::git::prune_pins(&repo, "mine", eave_engine::git::KIND_TURN, 0).await;

    assert_eq!(
        git_out(&repo, &["rev-parse", "--verify", "--quiet", &mine.git_ref]),
        "",
        "the named session's checkpoints go"
    );
    assert!(
        !git_out(
            &repo,
            &["rev-parse", "--verify", "--quiet", &theirs.git_ref]
        )
        .is_empty(),
        "another session's checkpoints stay"
    );
}

/// **The cap bounds the namespace, and keeps the newest.**
///
/// Nothing else bounds it: the engine has no session-delete path, so a
/// ref written per turn is a ref kept forever, and a checkpoint holds
/// every version of every file it captured.
///
/// Ordering is by the commit's own date, which git records at ONE-SECOND
/// resolution — so this sleeps to make "newest" a real question rather
/// than a coin flip. Within a single second the prune may keep the older
/// of two; turns are seconds apart and the cap only bites at 50, so the
/// worst case is one checkpoint of fifty being the wrong one.
#[tokio::test]
async fn the_cap_bounds_the_pins_and_keeps_the_newest() {
    let repo = repo();
    let mut made = Vec::new();
    for i in 0..3 {
        std::fs::write(repo.join("base.txt"), format!("state {i}\n")).unwrap();
        made.push(
            eave_engine::git::capture_checkpoint(&repo, &index_for("c"), "s1")
                .await
                .expect("capture"),
        );
    }
    // A second boundary, so the last capture is unambiguously newest.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    std::fs::write(repo.join("base.txt"), "the newest state\n").unwrap();
    let newest = eave_engine::git::capture_checkpoint(&repo, &index_for("c"), "s1")
        .await
        .expect("capture");

    eave_engine::git::prune_pins(&repo, "s1", eave_engine::git::KIND_TURN, 1).await;

    let left = git_out(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/eave/session-s1/turn-*",
        ],
    );
    let lines: Vec<&str> = left.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "the cap must bound the set: {left}");
    assert_eq!(
        lines[0], newest.git_ref,
        "the newest pin is the one worth keeping"
    );
    // And the pruned ones really are gone as objects once git sweeps.
    git(&repo, &["reflog", "expire", "--expire=now", "--all"]);
    git(&repo, &["gc", "--prune=now", "-q"]);
    assert!(
        eave_engine::git::checkpoint_alive(&repo, &newest.tree).await,
        "the kept pin survives"
    );
    assert!(
        !eave_engine::git::checkpoint_alive(&repo, &made[0].tree).await,
        "a pruned pin is collectable — which is the point of pruning"
    );
}

/// A binary blob must survive a round trip — `git diff` emits no usable
/// hunk for one without `--binary`, and `git apply` would then refuse the
/// patch outright.
#[tokio::test]
async fn a_restore_carries_binary_files() {
    let repo = repo();
    let bytes: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    std::fs::write(repo.join("blob.bin"), &bytes).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "blob"]);

    let cp = eave_engine::git::capture_checkpoint(&repo, &index_for("cap"), "s1")
        .await
        .expect("capture");

    std::fs::write(repo.join("blob.bin"), b"clobbered").unwrap();
    eave_engine::git::restore_checkpoint(
        &repo,
        &index_for("res"),
        &cp.tree,
        &scratch().join("r.patch"),
        "s1",
        true,
    )
    .await
    .expect("restore");

    assert_eq!(
        std::fs::read(repo.join("blob.bin")).unwrap(),
        bytes,
        "a binary file must come back byte-for-byte"
    );
}

// ---------------------------------------------------------------------
// The live engine: does the send path actually take a checkpoint, and
// does a restore reach the operator's files?
//
// The tests above prove the git primitives are correct. These prove they
// are CALLED — a distinction that matters, because every one of the
// primitive tests would still pass with the engine wiring deleted.
// ---------------------------------------------------------------------

mod live {
    use super::*;
    use eave_core::journal::JournalEvent;
    use eave_engine::client::Client;
    use eave_wire::SubscriptionItem;
    use std::process::Child;
    use std::time::{Duration, Instant};

    struct EngineProc {
        child: Child,
        data_dir: PathBuf,
    }

    impl EngineProc {
        fn spawn(data_dir: &Path) -> Self {
            let engined = PathBuf::from(env!("CARGO_BIN_EXE_eave-engined"));
            let fake = engined.parent().expect("bin dir").join("fake-agent");
            assert!(
                fake.exists(),
                "fake-agent not built at {fake:?} — run `cargo test` from the workspace root"
            );
            let log = std::fs::File::create(data_dir.join("engine-test.log")).unwrap();
            let child = Command::new(&engined)
                .args(["--data-dir", &data_dir.display().to_string()])
                // Never the developer's checkout — see the note in the
                // other test engines.
                .current_dir(data_dir)
                .env("EAVE_AGENT", &fake)
                .env("EAVE_FAKE_MODE", "linger")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log)
                .spawn()
                .expect("spawn eave-engined");
            let me = Self {
                child,
                data_dir: data_dir.to_path_buf(),
            };
            // The engine opens its socket asynchronously; connecting
            // before it does is a NotFound that reads like a bug in the
            // thing under test.
            let sock = eave_wire::socket_path(&me.data_dir);
            let deadline = Instant::now() + Duration::from_secs(20);
            while std::os::unix::net::UnixStream::connect(&sock).is_err() {
                assert!(Instant::now() < deadline, "engine never opened its socket");
                std::thread::sleep(Duration::from_millis(20));
            }
            me
        }
    }

    impl Drop for EngineProc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = &self.data_dir;
        }
    }

    /// Collect records until `pred` is satisfied, or give up in words.
    async fn records_until(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SubscriptionItem>,
        what: &str,
        mut pred: impl FnMut(&[eave_core::journal::Record]) -> bool,
    ) -> Vec<eave_core::journal::Record> {
        let end = Instant::now() + Duration::from_secs(20);
        let mut all: Vec<eave_core::journal::Record> = Vec::new();
        loop {
            if pred(&all) {
                return all;
            }
            let remaining = end.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {what}: {all:#?}"
            );
            let item = tokio::time::timeout(remaining, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .expect("record stream ended");
            match item {
                SubscriptionItem::Events { records, .. } => all.extend(records),
                SubscriptionItem::Reset { .. } => all.clear(),
                _ => {}
            }
        }
    }

    fn checkpoint_seq(records: &[eave_core::journal::Record]) -> Option<u64> {
        records
            .iter()
            .find(|r| matches!(r.event, JournalEvent::Checkpoint { .. }))
            .map(|r| r.seq)
    }

    /// **The wiring test.** A send takes a checkpoint, and the checkpoint
    /// record lands immediately BEFORE the `user_sent` it belongs to —
    /// which is the whole basis on which the projection attaches one to
    /// the other. Matched positionally here, exactly as the fold does it.
    #[tokio::test]
    async fn a_send_checkpoints_the_worktree_before_the_prompt() {
        let dir = scratch();
        let repo = repo();
        std::fs::write(repo.join("base.txt"), "dirty before the turn\n").unwrap();
        let engine = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        c.open(Some("s1".into()), Some(repo.display().to_string()))
            .await
            .expect("open");
        let mut rx = c.subscribe("s1", None).await.expect("subscribe");
        assert!(c.send("s1", "go").await.expect("send"));

        let records = records_until(&mut rx, "a checkpoint and a user_sent", |rs| {
            checkpoint_seq(rs).is_some()
                && rs
                    .iter()
                    .any(|r| matches!(r.event, JournalEvent::UserSent { .. }))
        })
        .await;

        let cp_ix = records
            .iter()
            .position(|r| matches!(r.event, JournalEvent::Checkpoint { .. }))
            .expect("a checkpoint record");
        let sent_ix = records
            .iter()
            .position(|r| matches!(r.event, JournalEvent::UserSent { .. }))
            .expect("a user_sent record");
        assert_eq!(
            cp_ix + 1,
            sent_ix,
            "the checkpoint must be the record immediately before its turn"
        );

        let JournalEvent::Checkpoint { tree, commit } = &records[cp_ix].event else {
            unreachable!()
        };
        assert!(
            eave_engine::git::checkpoint_alive(&repo, tree).await,
            "the checkpoint's tree must be in the repo"
        );
        assert_eq!(
            git_out(
                &repo,
                &[
                    "rev-parse",
                    &eave_engine::git::pin_ref("s1", "turn", commit)
                ]
            ),
            *commit,
            "and it must be pinned under this session's refs"
        );
        drop(engine);
    }

    /// **The round trip.** The agent mangles the tree, the operator
    /// restores it, and the files come back — driven through the wire,
    /// not through the git module.
    #[tokio::test]
    async fn a_restore_reaches_the_operators_files() {
        let dir = scratch();
        let repo = repo();
        let engine = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        c.open(Some("s1".into()), Some(repo.display().to_string()))
            .await
            .expect("open");
        let mut rx = c.subscribe("s1", None).await.expect("subscribe");
        assert!(c.send("s1", "go").await.expect("send"));
        let records = records_until(&mut rx, "the turn's checkpoint", |rs| {
            checkpoint_seq(rs).is_some()
        })
        .await;
        let seq = checkpoint_seq(&records).expect("a checkpoint");

        // The turn does damage.
        std::fs::write(repo.join("base.txt"), "MANGLED\n").unwrap();
        std::fs::write(repo.join("junk.txt"), "the agent invented this\n").unwrap();

        let ok = c.restore("s1", seq, true).await.expect("restore call");
        assert!(ok.restored, "restore refused: {:?}", ok.reason);
        let mut files = ok.files.clone();
        files.sort();
        assert_eq!(files, vec!["base.txt", "junk.txt"]);

        assert_eq!(read(&repo.join("base.txt")), "one\ntwo\n");
        assert!(!repo.join("junk.txt").exists());

        // And it is written down. A file change eave made with no record
        // of making it is the thing the append rule exists to prevent.
        let after = records_until(&mut rx, "the restored record", |rs| {
            rs.iter()
                .any(|r| matches!(r.event, JournalEvent::Restored { .. }))
        })
        .await;
        let rec = after
            .iter()
            .find_map(|r| match &r.event {
                JournalEvent::Restored {
                    to_seq,
                    files,
                    undo_tree,
                } => Some((*to_seq, files.clone(), undo_tree.clone())),
                _ => None,
            })
            .expect("a restored record");
        assert_eq!(rec.0, seq, "the record names the checkpoint restored to");
        assert_eq!(rec.1.len(), 2);
        assert!(
            eave_engine::git::checkpoint_alive(&repo, &rec.2).await,
            "the undo tree must still be in the repo — it is the only way back"
        );
        drop(engine);
    }

    /// **The unconfirmed call is a question, not an action.** It states
    /// the count and writes nothing — and the count it states is measured
    /// by the same walk the real restore uses, so the number the operator
    /// confirms is the number that changes.
    #[tokio::test]
    async fn an_unconfirmed_restore_states_the_count_and_writes_nothing() {
        let dir = scratch();
        let repo = repo();
        let engine = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        c.open(Some("s1".into()), Some(repo.display().to_string()))
            .await
            .expect("open");
        let mut rx = c.subscribe("s1", None).await.expect("subscribe");
        assert!(c.send("s1", "go").await.expect("send"));
        let records = records_until(&mut rx, "the turn's checkpoint", |rs| {
            checkpoint_seq(rs).is_some()
        })
        .await;
        let seq = checkpoint_seq(&records).expect("a checkpoint");

        std::fs::write(repo.join("base.txt"), "MANGLED\n").unwrap();
        std::fs::write(repo.join("junk.txt"), "invented\n").unwrap();

        let asked = c.restore("s1", seq, false).await.expect("dry run");
        assert!(!asked.restored, "an unconfirmed restore must not restore");
        assert_eq!(asked.files.len(), 2);
        assert_eq!(asked.reason.as_deref(), Some("undo 2 files?"));
        assert_eq!(
            read(&repo.join("base.txt")),
            "MANGLED\n",
            "the dry run must not have written"
        );
        assert!(repo.join("junk.txt").exists());
        // And no undo ref was minted for a question.
        assert_eq!(
            git_out(
                &repo,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/eave/session-s1/undo-",
                ]
            ),
            ""
        );

        // The same count, now acted on.
        let done = c.restore("s1", seq, true).await.expect("restore");
        assert!(done.restored);
        assert_eq!(done.files.len(), asked.files.len());
        assert_eq!(read(&repo.join("base.txt")), "one\ntwo\n");
        drop(engine);
    }

    /// A seq that names no checkpoint is answered in words. `Ok` with a
    /// reason, never `Err`: the socket worked, the request was refused.
    #[tokio::test]
    async fn restoring_a_seq_that_is_not_a_checkpoint_is_refused_in_words() {
        let dir = scratch();
        let repo = repo();
        let engine = EngineProc::spawn(&dir);
        let c = Client::connect(&dir).await.expect("connect");
        c.open(Some("s1".into()), Some(repo.display().to_string()))
            .await
            .expect("open");
        std::fs::write(repo.join("base.txt"), "untouched\n").unwrap();

        let ok = c.restore("s1", 9999, true).await.expect("restore call");
        assert!(!ok.restored);
        assert!(
            ok.reason
                .as_deref()
                .unwrap_or_default()
                .contains("no checkpoint"),
            "the refusal must say what was wrong: {:?}",
            ok.reason
        );
        assert_eq!(
            read(&repo.join("base.txt")),
            "untouched\n",
            "a refused restore touches nothing"
        );
        drop(engine);
    }
}
