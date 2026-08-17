//! Engine-side git polling for the changed-file overview (Slice B,
//! spec 05 §Files).
//!
//! Truth source is `git status` / `git diff` run in the session cwd —
//! never tool records, which describe intent, not disk. Every invocation
//! is an async child process bounded by [`GIT_TIMEOUT`], so a
//! pathological repo can slow its own snapshot but never wedge the
//! engine's event loop or any session (build-directive constraint).
//! `GIT_OPTIONAL_LOCKS=0` keeps the poll read-only in practice: status
//! must not contend for the index lock with an agent that is itself
//! running git in the same repo.
//!
//! A non-git cwd and a failed/timed-out invocation are stated facts
//! ([`GitState::NotARepo`] / [`GitState::Error`]), never crashes.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use eave_wire::{ChangedFile, FileStatus, FilesSnapshot, GitState};

/// Bound on every git call. Generous — a cold `status` on a large repo
/// legitimately takes seconds — but hard: past it the child is killed
/// and the snapshot states the timeout.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Untracked files above this size get `None` line counts instead of a
/// read; same for binaries (NUL in the head bytes).
const COUNT_CAP_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// One full scan of the session cwd: statuses from porcelain v2, line
/// counts from `numstat` against HEAD (or the empty tree in a repo with
/// no commits), untracked files counted directly.
pub async fn scan(cwd: &Path) -> FilesSnapshot {
    scan_with(Path::new("git"), cwd, GIT_TIMEOUT).await
}

/// [`scan`] with the git binary and timeout injectable (tests: a fake
/// git that hangs proves the timeout, at test-scale duration).
pub async fn scan_with(git: &Path, cwd: &Path, timeout: Duration) -> FilesSnapshot {
    let status_bytes = match run(git, cwd, timeout, &["status", "--porcelain=v2", "-z", "--untracked-files=all"]).await {
        Ok(bytes) => bytes,
        Err(state) => return FilesSnapshot { state, files: Vec::new() },
    };
    let mut files = parse_status_v2(&status_bytes);
    if files.is_empty() {
        return FilesSnapshot { state: GitState::Ok, files };
    }

    // Line counts for tracked changes: one numstat against HEAD covers
    // staged + unstaged as a single per-file delta. A repo with no
    // commits has no HEAD — diff against its own empty tree instead
    // (hash-object, so SHA-1 and SHA-256 repos both answer).
    let numstat = match run(git, cwd, timeout, &["diff", "--numstat", "-z", "-M", "HEAD", "--"]).await {
        Ok(bytes) => bytes,
        Err(GitState::Error { .. }) => {
            match run(git, cwd, timeout, &["hash-object", "-t", "tree", "/dev/null"]).await {
                Ok(tree) => {
                    let tree = String::from_utf8_lossy(&tree).trim().to_string();
                    match run(git, cwd, timeout, &["diff", "--numstat", "-z", "-M", &tree, "--"]).await {
                        Ok(bytes) => bytes,
                        Err(state) => return FilesSnapshot { state, files: Vec::new() },
                    }
                }
                Err(state) => return FilesSnapshot { state, files: Vec::new() },
            }
        }
        Err(state) => return FilesSnapshot { state, files: Vec::new() },
    };
    let counts = parse_numstat(&numstat);
    for f in &mut files {
        if let Some((adds, dels)) = counts.get(&f.path) {
            f.adds = *adds;
            f.dels = *dels;
        }
    }

    // Untracked files never appear in numstat; count them ourselves,
    // bounded and binary-guarded.
    for f in &mut files {
        if f.status == FileStatus::Added && f.adds.is_none() && f.dels.is_none() {
            if let Some(lines) = count_lines(&cwd.join(&f.path)).await {
                f.adds = Some(lines);
                f.dels = Some(0);
            }
        }
    }
    FilesSnapshot { state: GitState::Ok, files }
}

/// Run one git command; classify failure as a stated fact. The child is
/// killed on timeout (`kill_on_drop`).
async fn run(git: &Path, cwd: &Path, timeout: Duration, args: &[&str]) -> Result<Vec<u8>, GitState> {
    let child = Command::new(git)
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            return Err(GitState::Error {
                message: format!("git failed to start: {e}"),
            })
        }
    };
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(GitState::Error {
                message: format!("git {}: {e}", args.first().unwrap_or(&"")),
            })
        }
        Err(_elapsed) => {
            return Err(GitState::Error {
                message: format!(
                    "git {} timed out after {}s",
                    args.first().unwrap_or(&""),
                    timeout.as_secs_f32()
                ),
            })
        }
    };
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not a git repository") {
        return Err(GitState::NotARepo);
    }
    Err(GitState::Error {
        message: format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.lines().next().unwrap_or("(no stderr)")
        ),
    })
}

/// Porcelain v2 `-z`: NUL-separated entries; a rename (`2 …`) entry is
/// followed by one extra NUL-separated token (the original path).
fn parse_status_v2(bytes: &[u8]) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut chunks = bytes
        .split(|b| *b == 0)
        .map(|c| String::from_utf8_lossy(c).into_owned());
    while let Some(chunk) = chunks.next() {
        if chunk.is_empty() {
            continue;
        }
        let (status, path) = match chunk.as_bytes().first() {
            Some(b'1') => {
                // `1 XY sub mH mI mW hH hI path` — path may hold spaces.
                let mut it = chunk.splitn(9, ' ');
                let xy = it.nth(1).unwrap_or("");
                let path = it.nth(6).unwrap_or("");
                (fold_xy(xy), path.to_string())
            }
            Some(b'2') => {
                // `2 XY sub mH mI mW hH hI Xscore path` NUL `origpath`.
                let mut it = chunk.splitn(10, ' ');
                let path = it.nth(9).unwrap_or("");
                let _orig = chunks.next(); // consumed; B1 shows the new path
                (FileStatus::Renamed, path.to_string())
            }
            Some(b'u') => {
                // `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
                let mut it = chunk.splitn(11, ' ');
                let path = it.nth(10).unwrap_or("");
                (FileStatus::Conflicted, path.to_string())
            }
            Some(b'?') => (FileStatus::Added, chunk[2.min(chunk.len())..].to_string()),
            _ => continue, // `!` ignored entries (not requested), headers
        };
        if !path.is_empty() {
            files.push(ChangedFile {
                path,
                status,
                adds: None,
                dels: None,
            });
        }
    }
    files
}

/// The worktree-level fold of git's XY pair: what happened to the file,
/// not the index/worktree split (staging is out of scope for Slice B).
fn fold_xy(xy: &str) -> FileStatus {
    if xy.contains('D') {
        FileStatus::Deleted
    } else if xy.contains('A') {
        FileStatus::Added
    } else {
        FileStatus::Modified
    }
}

/// `numstat -z`: `adds\tdels\tpath` NUL — except renames, where the path
/// field is empty and two more NUL tokens follow (pre, post). `-` counts
/// mean binary → `None`.
fn parse_numstat(bytes: &[u8]) -> HashMap<String, (Option<u64>, Option<u64>)> {
    let mut map = HashMap::new();
    let mut chunks = bytes
        .split(|b| *b == 0)
        .map(|c| String::from_utf8_lossy(c).into_owned());
    while let Some(chunk) = chunks.next() {
        if chunk.is_empty() {
            continue;
        }
        let mut it = chunk.splitn(3, '\t');
        let (Some(adds), Some(dels), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let adds = adds.parse::<u64>().ok();
        let dels = dels.parse::<u64>().ok();
        let path = if path.is_empty() {
            let _pre = chunks.next();
            match chunks.next() {
                Some(post) => post,
                None => continue,
            }
        } else {
            path.to_string()
        };
        map.insert(path, (adds, dels));
    }
    map
}

/// Count an untracked file's lines, or `None` when the honest answer is
/// "unknowable at reasonable cost": missing (raced a delete), over the
/// size cap, or binary.
async fn count_lines(path: &Path) -> Option<u64> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > COUNT_CAP_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return None;
    }
    if bytes.is_empty() {
        return Some(0);
    }
    let mut lines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    if *bytes.last().unwrap() != b'\n' {
        lines += 1; // final line without a trailing newline
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    fn scratch() -> PathBuf {
        let dir = PathBuf::from("/tmp/eave-git-tests").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
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

    fn repo_with_commit() -> PathBuf {
        let dir = scratch();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "x\ny\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "base"]);
        dir
    }

    fn by_path<'a>(snap: &'a FilesSnapshot, path: &str) -> &'a ChangedFile {
        snap.files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} missing from {:?}", snap.files))
    }

    #[tokio::test]
    async fn non_git_cwd_is_a_stated_fact() {
        let dir = scratch();
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::NotARepo);
        assert!(snap.files.is_empty());
    }

    #[tokio::test]
    async fn missing_cwd_is_a_stated_error() {
        let snap = scan(Path::new("/tmp/eave-git-tests/definitely-not-here")).await;
        assert!(
            matches!(snap.state, GitState::Error { .. }),
            "{:?}",
            snap.state
        );
    }

    #[tokio::test]
    async fn clean_repo_is_ok_and_empty() {
        let dir = repo_with_commit();
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::Ok);
        assert!(snap.files.is_empty(), "{:?}", snap.files);
    }

    #[tokio::test]
    async fn statuses_and_counts_for_modified_untracked_deleted() {
        let dir = repo_with_commit();
        // modify: -1 line, +2 lines
        std::fs::write(dir.join("a.txt"), "one\nTWO!\n2.5\nthree\n").unwrap();
        // delete a tracked file
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        // untracked, no trailing newline on the last line
        std::fs::write(dir.join("new file.txt"), "alpha\nbeta").unwrap();
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::Ok);
        assert_eq!(snap.files.len(), 3, "{:?}", snap.files);

        let m = by_path(&snap, "a.txt");
        assert_eq!(m.status, FileStatus::Modified);
        assert_eq!((m.adds, m.dels), (Some(2), Some(1)));

        let d = by_path(&snap, "gone.txt");
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!((d.adds, d.dels), (Some(0), Some(2)));

        let u = by_path(&snap, "new file.txt");
        assert_eq!(u.status, FileStatus::Added);
        assert_eq!((u.adds, u.dels), (Some(2), Some(0)));
    }

    #[tokio::test]
    async fn staged_rename_reports_the_new_path() {
        let dir = repo_with_commit();
        git(&dir, &["mv", "a.txt", "b.txt"]);
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::Ok);
        let r = by_path(&snap, "b.txt");
        assert_eq!(r.status, FileStatus::Renamed);
        // pure rename: no content delta
        assert_eq!((r.adds, r.dels), (Some(0), Some(0)));
    }

    #[tokio::test]
    async fn repo_without_commits_counts_against_the_empty_tree() {
        let dir = scratch();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("fresh.txt"), "a\nb\nc\n").unwrap();
        git(&dir, &["add", "."]);
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::Ok);
        let f = by_path(&snap, "fresh.txt");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.adds, f.dels), (Some(3), Some(0)));
    }

    #[tokio::test]
    async fn binary_untracked_file_has_unknown_counts() {
        let dir = repo_with_commit();
        std::fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
        let snap = scan(&dir).await;
        let b = by_path(&snap, "blob.bin");
        assert_eq!(b.status, FileStatus::Added);
        assert_eq!((b.adds, b.dels), (None, None));
    }

    /// The timeout constraint, proven at test scale: a git that hangs is
    /// killed and the snapshot states the timeout as a fact.
    #[tokio::test]
    async fn hanging_git_is_killed_and_stated() {
        let dir = scratch();
        let fake = dir.join("slow-git");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let snap = scan_with(&fake, &dir, Duration::from_millis(200)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        match snap.state {
            GitState::Error { message } => {
                assert!(message.contains("timed out"), "{message}")
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }
}
