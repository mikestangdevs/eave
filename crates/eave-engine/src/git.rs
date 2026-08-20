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
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use eave_wire::{
    ChangedFile, DiffBasis, DiffHunk, DiffKind, DiffLine, DiffOp, FileDiff, FileStatus,
    FilesSnapshot, GitState,
};

/// Bound on every git call. Generous — a cold `status` on a large repo
/// legitimately takes seconds — but hard: past it the child is killed
/// and the snapshot states the timeout.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Untracked files above this size get `None` line counts instead of a
/// read; same for binaries (NUL in the head bytes).
const COUNT_CAP_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// List cap (B2): a repo with 10k changed files must not ship a 10k-row
/// frame on every refresh. The overflow is a stated count
/// (`FilesSnapshot::omitted`), never silent truncation.
pub const MAX_FILES: usize = 1000;
/// Per-scan bound on untracked-file line counting (each count is a file
/// read): past it, untracked counts render `—` — honest unknowns beat a
/// scan that reads a thousand files.
pub const UNTRACKED_COUNT_MAX: usize = 200;

/// Diff caps (B2): bound memory as well as time. Past either, the diff
/// is the head of the real thing and says so (`truncated`); the full
/// diff stays in git where it belongs.
pub const DIFF_MAX_BYTES: usize = 512 * 1024;
pub const DIFF_MAX_LINES: usize = 2000;

/// One full scan of the session cwd: statuses from porcelain v2, line
/// counts from `numstat` against HEAD (or the empty tree in a repo with
/// no commits), untracked files counted directly.
pub async fn scan(cwd: &Path) -> FilesSnapshot {
    scan_with(Path::new("git"), cwd, GIT_TIMEOUT).await
}

/// [`scan`] with the git binary and timeout injectable (tests: a fake
/// git that hangs proves the timeout, at test-scale duration).
pub async fn scan_with(git: &Path, cwd: &Path, timeout: Duration) -> FilesSnapshot {
    let status_bytes = match run(
        git,
        cwd,
        timeout,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(state) => return stated_snapshot(state),
    };
    let mut files = parse_status_v2(&status_bytes);
    if files.is_empty() {
        return FilesSnapshot {
            state: GitState::Ok,
            files,
            omitted: 0,
            watching: false,
            basis: DiffBasis::WorkingTree,
        };
    }

    // Line counts for tracked changes: one numstat against HEAD covers
    // staged + unstaged as a single per-file delta. A repo with no
    // commits has no HEAD — diff against its own empty tree instead
    // (hash-object, so SHA-1 and SHA-256 repos both answer).
    let numstat = match run(
        git,
        cwd,
        timeout,
        &["diff", "--numstat", "-z", "-M", "HEAD", "--"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(GitState::Error { .. }) => {
            match run(
                git,
                cwd,
                timeout,
                &["hash-object", "-t", "tree", "/dev/null"],
            )
            .await
            {
                Ok(tree) => {
                    let tree = String::from_utf8_lossy(&tree).trim().to_string();
                    match run(
                        git,
                        cwd,
                        timeout,
                        &["diff", "--numstat", "-z", "-M", &tree, "--"],
                    )
                    .await
                    {
                        Ok(bytes) => bytes,
                        Err(state) => return stated_snapshot(state),
                    }
                }
                Err(state) => return stated_snapshot(state),
            }
        }
        Err(state) => return stated_snapshot(state),
    };
    let counts = parse_numstat(&numstat);
    for f in &mut files {
        if let Some((adds, dels)) = counts.get(&f.path) {
            f.adds = *adds;
            f.dels = *dels;
        }
    }

    // List cap before the untracked reads: the omitted tail must not
    // cost file I/O either.
    let omitted = files.len().saturating_sub(MAX_FILES) as u64;
    files.truncate(MAX_FILES);

    // Untracked files never appear in numstat; count them ourselves,
    // bounded per file (size/binary) and per scan (count cap).
    let mut counted = 0usize;
    for f in &mut files {
        if f.untracked && f.adds.is_none() && f.dels.is_none() {
            if counted >= UNTRACKED_COUNT_MAX {
                continue; // stated `—`, not a thousand file reads
            }
            counted += 1;
            if let Some(lines) = count_lines(&cwd.join(&f.path)).await {
                f.adds = Some(lines);
                f.dels = Some(0);
            }
        }
    }
    FilesSnapshot {
        state: GitState::Ok,
        files,
        omitted,
        watching: false,
        basis: DiffBasis::WorkingTree,
    }
}

/// A snapshot that carries nothing but the reason it carries nothing.
/// Every scan bail-out lands here so "git could not answer" is one shape
/// with one meaning, whichever call failed.
fn stated_snapshot(state: GitState) -> FilesSnapshot {
    FilesSnapshot {
        state,
        files: Vec::new(),
        omitted: 0,
        watching: false,
        basis: DiffBasis::WorkingTree,
    }
}

/// Run one git command; classify failure as a stated fact. The child is
/// killed on timeout (`kill_on_drop`).
async fn run(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    args: &[&str],
) -> Result<Vec<u8>, GitState> {
    run_env(git, cwd, timeout, args, None).await
}

/// [`run`] with one extra environment variable — the seat `GIT_INDEX_FILE`
/// sits in (Slice AA). Kept as a separate parameter rather than a general
/// env map because there is exactly one variable eave ever overrides here
/// and it is the one that decides whether a git call touches the
/// operator's real index.
async fn run_env(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    args: &[&str],
    extra_env: Option<(&str, &Path)>,
) -> Result<Vec<u8>, GitState> {
    let mut cmd = Command::new(git);
    if let Some((k, v)) = extra_env {
        cmd.env(k, v);
    }
    let child = cmd
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
        Err(_elapsed) => return Err(unanswered(args.first().unwrap_or(&""), cwd, timeout)),
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
        let (status, path, orig_path, untracked) = match chunk.as_bytes().first() {
            Some(b'1') => {
                // `1 XY sub mH mI mW hH hI path` — path may hold spaces.
                let mut it = chunk.splitn(9, ' ');
                let xy = it.nth(1).unwrap_or("");
                let path = it.nth(6).unwrap_or("");
                (fold_xy(xy), path.to_string(), None, false)
            }
            Some(b'2') => {
                // `2 XY sub mH mI mW hH hI Xscore path` NUL `origpath`.
                let mut it = chunk.splitn(10, ' ');
                let path = it.nth(9).unwrap_or("");
                // The pre-rename path: the diff pathspec needs both sides
                // (B2), and "renamed from X" is a fact worth stating.
                let orig = chunks.next().filter(|o| !o.is_empty());
                (FileStatus::Renamed, path.to_string(), orig, false)
            }
            Some(b'u') => {
                // `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
                let mut it = chunk.splitn(11, ' ');
                let path = it.nth(10).unwrap_or("");
                (FileStatus::Conflicted, path.to_string(), None, false)
            }
            Some(b'?') => (
                FileStatus::Added,
                chunk[2.min(chunk.len())..].to_string(),
                None,
                true,
            ),
            _ => continue, // `!` ignored entries (not requested), headers
        };
        if !path.is_empty() {
            files.push(ChangedFile {
                path,
                status,
                adds: None,
                dels: None,
                orig_path,
                untracked,
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

// ---------------------------------------------------------------------------
// Turn-scoped changes (Slice AA)
//
// "What did THIS turn change" is a different question from "what is
// uncommitted", and git has no name for the answer — so eave makes one:
// at every turn dispatch it writes the working tree (tracked AND
// untracked-unignored) into the object store as a tree, and the
// turn-scoped scan is that tree against a fresh one.
//
// Two properties are load-bearing and neither is obvious:
//
// 1. **The throwaway index must live OUTSIDE the repository.** `git add
//    -A` under a `GIT_INDEX_FILE` in eave's data dir leaves `git status
//    --porcelain` byte-identical (asserted by test). Put the same file
//    inside the work tree and `add -A` sweeps up the index — and its lock
//    file — into the very tree it is capturing. That was observed on a
//    scratch repo, not reasoned about.
// 2. **Both sides of the diff carry untracked files**, which is why the
//    turn scan needs none of the untracked synthesis the working-tree
//    scan does. A file that was already untracked when the turn started
//    is in the base tree, so a turn that edits it reports MODIFIED.
//    Synthesize instead and the same file reads as entirely new — the
//    turn gets credited with every line the operator wrote before it
//    started.

/// Capture the current working tree — tracked and untracked-unignored —
/// as a tree object, via a throwaway index at `index`.
///
/// `index` is a parameter, not a temp file this function invents, for one
/// reason: the caller is responsible for putting it in eave's data dir.
/// Inside the repo it corrupts the capture (see above); in `/tmp` it is
/// somebody else's to delete. Reusing the same path across calls is
/// deliberate — a fresh index has no stat cache, so every capture would
/// otherwise re-hash the whole work tree. On failure the file is removed
/// so a corrupt index heals itself on the next attempt instead of failing
/// forever.
///
/// The real index is never named and never touched: the only mutation is
/// to `index`, and the operator's staged state is exactly as it was.
pub async fn snapshot_tree(cwd: &Path, index: &Path) -> Result<String, GitState> {
    snapshot_tree_with(Path::new("git"), cwd, GIT_TIMEOUT, index).await
}

/// [`snapshot_tree`] with the git binary and timeout injectable.
pub async fn snapshot_tree_with(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    index: &Path,
) -> Result<String, GitState> {
    if let Some(parent) = index.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return Err(GitState::Error {
                message: format!("turn index dir {}: {e}", parent.display()),
            });
        }
    }
    // No pathspec: `add -A` with one would scope the capture to the cwd,
    // and a session cwd is allowed to be a subdirectory of its checkout.
    // The working-tree scan states root-relative paths, so the turn scan
    // must cover the same tree or the two scopes would disagree about
    // what a path even means.
    let env = Some(("GIT_INDEX_FILE", index));
    if let Err(state) = run_env(git, cwd, timeout, &["add", "-A", "--ignore-errors"], env).await {
        let _ = tokio::fs::remove_file(index).await;
        return Err(state);
    }
    match run_env(git, cwd, timeout, &["write-tree"], env).await {
        Ok(out) => {
            let sha = String::from_utf8_lossy(&out).trim().to_string();
            if sha.is_empty() {
                let _ = tokio::fs::remove_file(index).await;
                return Err(GitState::Error {
                    message: "git write-tree printed nothing".into(),
                });
            }
            Ok(sha)
        }
        Err(state) => {
            let _ = tokio::fs::remove_file(index).await;
            Err(state)
        }
    }
}

/// The turn-scoped scan: `base_tree` against a fresh capture of the
/// working tree.
///
/// `Err(reason)` means the BASIS is unusable — the caller states it as
/// `DiffBasis::Unavailable` rather than as a git failure, because the
/// repository is fine and only the thing we wanted to measure from is
/// gone. `Ok` may still carry a [`GitState::Error`]: that is a scan
/// failure, which is the existing shape.
pub async fn scan_against(
    cwd: &Path,
    base_tree: &str,
    index: &Path,
) -> Result<FilesSnapshot, String> {
    scan_against_with(Path::new("git"), cwd, GIT_TIMEOUT, base_tree, index).await
}

/// [`scan_against`] with the git binary and timeout injectable.
pub async fn scan_against_with(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    base_tree: &str,
    index: &Path,
) -> Result<FilesSnapshot, String> {
    // Ask before diffing, rather than pattern-matching git's stderr
    // afterwards. The base is an UNREACHABLE object — nothing references
    // it — so an agent that runs `git gc --prune=now` mid-turn is
    // entitled to collect it, and eave has to be able to say so in words
    // instead of reporting a git failure the operator cannot act on.
    match run(git, cwd, timeout, &["cat-file", "-t", base_tree]).await {
        Ok(out) if String::from_utf8_lossy(&out).trim() == "tree" => {}
        _ => {
            return Err(format!(
                "the turn's base tree {} is no longer in the object store — a `git gc` \
                 can prune it while the turn runs. The next turn starts a fresh base.",
                short_sha(base_tree)
            ))
        }
    }

    let current = match snapshot_tree_with(git, cwd, timeout, index).await {
        Ok(sha) => sha,
        Err(state) => return Ok(stated_snapshot(state)),
    };

    let names = match run(
        git,
        cwd,
        timeout,
        &[
            "diff",
            "--name-status",
            "-z",
            "-M",
            base_tree,
            &current,
            "--",
        ],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(state) => return Ok(stated_snapshot(state)),
    };
    let mut files = parse_name_status(&names);
    if files.is_empty() {
        return Ok(FilesSnapshot {
            state: GitState::Ok,
            files,
            omitted: 0,
            watching: false,
            basis: DiffBasis::WorkingTree,
        });
    }

    // Both sides are trees, so numstat covers every entry — including the
    // untracked ones the working-tree scan has to count by hand.
    let numstat = match run(
        git,
        cwd,
        timeout,
        &["diff", "--numstat", "-z", "-M", base_tree, &current, "--"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(state) => return Ok(stated_snapshot(state)),
    };
    let counts = parse_numstat(&numstat);
    for f in &mut files {
        if let Some((adds, dels)) = counts.get(&f.path) {
            f.adds = *adds;
            f.dels = *dels;
        }
    }

    // `untracked` describes the WORKING TREE, not this diff — it is what
    // tells the diff request to read the file's own bytes instead of
    // asking `git diff HEAD` about a path git has never heard of. The
    // turn scan changes the basis of the LIST; the per-file diff stays
    // working-tree-scoped, so the flag has to stay working-tree truth.
    if let Ok(bytes) = run(
        git,
        cwd,
        timeout,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await
    {
        let others: std::collections::HashSet<&str> = bytes
            .split(|b| *b == 0)
            .filter(|c| !c.is_empty())
            .filter_map(|c| std::str::from_utf8(c).ok())
            .collect();
        for f in &mut files {
            f.untracked = others.contains(f.path.as_str());
        }
    }

    let omitted = files.len().saturating_sub(MAX_FILES) as u64;
    files.truncate(MAX_FILES);
    Ok(FilesSnapshot {
        state: GitState::Ok,
        files,
        omitted,
        watching: false,
        basis: DiffBasis::WorkingTree,
    })
}

/// Enough of a sha to identify it in a sentence, without pretending the
/// operator is going to type it.
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// `diff --name-status -z`: status and path are SEPARATE NUL-terminated
/// fields (unlike the newline format), and a rename/copy score is
/// followed by two path fields — old, then new.
fn parse_name_status(bytes: &[u8]) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut chunks = bytes
        .split(|b| *b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned());
    while let Some(code) = chunks.next() {
        let letter = code.as_bytes()[0];
        let (status, orig_path) = match letter {
            b'A' => (FileStatus::Added, None),
            b'D' => (FileStatus::Deleted, None),
            b'R' | b'C' => {
                let Some(old) = chunks.next() else { break };
                // A copy's destination is a path that did not exist
                // before, and its source is unchanged and listed nowhere
                // — "added" is the fact, and naming a rename source that
                // is still sitting there would be a lie.
                if letter == b'R' {
                    (FileStatus::Renamed, Some(old))
                } else {
                    (FileStatus::Added, None)
                }
            }
            // `T` is a type change (file ↔ symlink ↔ submodule); `M` is
            // content or mode. Both are "this path is different now".
            b'M' | b'T' => (FileStatus::Modified, None),
            b'U' => (FileStatus::Conflicted, None),
            // `X` is git's own "unknown, probably a bug" marker. Skipping
            // the code alone would desynchronise the path stream, so its
            // path is consumed with it.
            _ => {
                let _ = chunks.next();
                continue;
            }
        };
        let Some(path) = chunks.next() else { break };
        if path.is_empty() {
            continue;
        }
        files.push(ChangedFile {
            path,
            status,
            adds: None,
            dels: None,
            orig_path,
            untracked: false,
        });
    }
    files
}

// ---------------------------------------------------------------------------
// Worktree plumbing (Slice I2)
//
// Same discipline as the scanner: every call is a timeout-bounded child
// process (`run`), never libgit2, never on the event loop. Failures are
// stated strings (git's own first stderr line where git refused) — the
// engine forwards them verbatim as `Err` replies.

/// The canonical checkout root for a cwd: the git worktree toplevel when
/// the cwd is inside one, the canonical cwd itself otherwise. Two
/// sessions with equal roots operate on the SAME files — this is the
/// collision fact (Slice I2 decision 1). Nested repos resolve to the
/// innermost toplevel, which is the honest answer: that is the tree git
/// commands in that cwd act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutProbe {
    pub root: std::path::PathBuf,
    pub is_repo: bool,
}

pub async fn checkout_root(cwd: &Path) -> CheckoutProbe {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match run(
        Path::new("git"),
        cwd,
        GIT_TIMEOUT,
        &["rev-parse", "--show-toplevel"],
    )
    .await
    {
        Ok(out) => {
            let top = String::from_utf8_lossy(&out).trim().to_string();
            if top.is_empty() {
                // Inside a bare repo / .git dir: no worktree. Fall back
                // to the directory itself.
                CheckoutProbe {
                    root: canon(cwd),
                    is_repo: false,
                }
            } else {
                CheckoutProbe {
                    root: canon(Path::new(&top)),
                    is_repo: true,
                }
            }
        }
        Err(_) => CheckoutProbe {
            root: canon(cwd),
            is_repo: false,
        },
    }
}

/// The cwd's resolved git dir (worktree-private for linked worktrees:
/// `<base>/.git/worktrees/<name>`). A linked worktree keeps its HEAD and
/// index THERE — outside the worktree directory — so the session's FS
/// watcher must cover it too or commits/index moves go stale until the
/// next poll.
pub async fn git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let out = run(
        Path::new("git"),
        cwd,
        GIT_TIMEOUT,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .await
    .ok()?;
    let dir = String::from_utf8_lossy(&out).trim().to_string();
    (!dir.is_empty()).then(|| std::path::PathBuf::from(dir))
}

fn stated(err: GitState) -> String {
    err.stated()
}

/// `git worktree add -b <branch> <path>` from the base repo: a fresh
/// checkout of the base's HEAD commit on a new branch. A dirty base is
/// fine and untouched — its uncommitted changes stay where they are; the
/// worktree starts clean at HEAD (stated Slice I2 edge case). Submodules
/// are NOT initialized in the new worktree (git's own behavior, stated).
pub async fn worktree_add(base: &Path, path: &Path, branch: &str) -> Result<(), String> {
    run(
        Path::new("git"),
        base,
        GIT_TIMEOUT,
        &["worktree", "add", "-b", branch, &path.to_string_lossy()],
    )
    .await
    .map(drop)
    .map_err(stated)
}

/// `git worktree remove` — with `--force` only when the operator forced
/// past the engine's own dirty check (decision 2's two-step). Without
/// force, git's dirty refusal is a second, independent guard.
pub async fn worktree_remove(base: &Path, path: &Path, force: bool) -> Result<(), String> {
    let path = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    run(Path::new("git"), base, GIT_TIMEOUT, &args)
        .await
        .map(drop)
        .map_err(stated)
}

pub async fn worktree_prune(base: &Path) -> Result<(), String> {
    run(Path::new("git"), base, GIT_TIMEOUT, &["worktree", "prune"])
        .await
        .map(drop)
        .map_err(stated)
}

/// `git branch -D` — the engine only ever calls this for `eave/`
/// branches it created (the guard lives at the call site).
pub async fn branch_delete(base: &Path, branch: &str) -> Result<(), String> {
    run(
        Path::new("git"),
        base,
        GIT_TIMEOUT,
        &["branch", "-D", "--", branch],
    )
    .await
    .map(drop)
    .map_err(stated)
}

/// Worktree paths registered in the base repo (`git worktree list
/// --porcelain`), main checkout first. This — never engine memory — is
/// the ground truth restart re-detection checks a session's claimed
/// worktree against (Slice I2).
pub async fn worktree_list(base: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let out = run(
        Path::new("git"),
        base,
        GIT_TIMEOUT,
        &["worktree", "list", "--porcelain"],
    )
    .await
    .map_err(stated)?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(|p| std::path::PathBuf::from(p.trim()))
        .collect())
}

/// Commits reachable from `branch` but from no other ref — the work that
/// `branch -D` would orphan. `None` = git could not answer (an unborn
/// branch, a vanished repo): an honest unknown the caller must treat as
/// "possibly some", never as zero.
pub async fn unmerged_count(repo: &Path, branch: &str) -> Option<u64> {
    // `--exclude` globs match the SHORTENED name of the next ref-listing
    // option and apply to that option only; `--all` is unusable here
    // because it also includes every linked worktree's HEAD — which IS
    // the branch being measured. Tags and remotes stay unexcluded on
    // purpose: a commit reachable from either is recoverable, not unique.
    let exclude = format!("--exclude={branch}");
    let out = run(
        Path::new("git"),
        repo,
        GIT_TIMEOUT,
        &[
            "rev-list",
            "--count",
            branch,
            "--not",
            &exclude,
            "--branches",
            "--tags",
            "--remotes",
        ],
    )
    .await
    .ok()?;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Per-file diff (Slice B2)

/// One file's diff, computed on demand — nothing is cached, so nothing
/// can go stale (invalidation is a fresh [`FilesSnapshot`], after which
/// the client re-requests). `file` is the snapshot's entry for the path:
/// its `untracked`/`orig_path` facts pick the honest strategy. Every
/// failure shape lands as a stated [`DiffKind`], never an error path.
pub async fn diff(cwd: &Path, file: &ChangedFile, base: Option<&str>) -> FileDiff {
    diff_with(Path::new("git"), cwd, GIT_TIMEOUT, file, base).await
}

/// [`diff`] with the git binary and timeout injectable (tests).
pub async fn diff_with(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    file: &ChangedFile,
    base: Option<&str>,
) -> FileDiff {
    let kind = if file.untracked {
        // `git diff HEAD` cannot see an untracked file; its honest diff
        // is its own content, all additions — read directly, with the
        // same binary/size discipline as the tracked path.
        untracked_kind(&cwd.join(&file.path)).await
    } else {
        tracked_kind(git, cwd, timeout, file, base).await
    };
    FileDiff {
        path: file.path.clone(),
        kind,
    }
}

/// Tracked change: one `git diff` against HEAD — worktree + index as a
/// single delta, the same basis as the snapshot's numstat — with the
/// empty-tree fallback for a repo with no commits. Renames put both
/// sides in the pathspec so `-M` can pair them.
/// `base` is the tree to diff against: `None` is `HEAD` (the working-tree
/// scope), `Some(sha)` a turn basis. A turn-scoped LIST with a HEAD-relative
/// PANE is the bug this parameter exists to prevent — the row says "this
/// turn touched it" and the diff shows everything since the last commit.
async fn tracked_kind(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    file: &ChangedFile,
    base: Option<&str>,
) -> DiffKind {
    let mut paths: Vec<&str> = vec![&file.path];
    if let Some(orig) = &file.orig_path {
        paths.push(orig);
    }
    let base_args = |base: &str| {
        let mut a: Vec<String> = vec!["diff".into(), "-M".into(), base.into(), "--".into()];
        a.extend(paths.iter().map(|p| p.to_string()));
        a
    };
    let head = base.unwrap_or("HEAD");
    let out = match run_capped(git, cwd, timeout, &base_args(head), DIFF_MAX_BYTES).await {
        Ok(out) => out,
        Err(GitState::Error { .. }) => {
            // No HEAD yet? Diff against the repo's own empty tree
            // (hash-object, so SHA-1 and SHA-256 repos both answer).
            let tree = match run(
                git,
                cwd,
                timeout,
                &["hash-object", "-t", "tree", "/dev/null"],
            )
            .await
            {
                Ok(t) => String::from_utf8_lossy(&t).trim().to_string(),
                Err(state) => return state_to_kind(state),
            };
            match run_capped(git, cwd, timeout, &base_args(&tree), DIFF_MAX_BYTES).await {
                Ok(out) => out,
                Err(state) => return state_to_kind(state),
            }
        }
        Err(state) => return state_to_kind(state),
    };
    parse_diff(&out.bytes, out.truncated)
}

fn state_to_kind(state: GitState) -> DiffKind {
    DiffKind::Error {
        message: state.stated(),
    }
}

struct CappedOutput {
    bytes: Vec<u8>,
    /// Stdout exceeded the cap; the child was killed and `bytes` is the
    /// head of the real output.
    truncated: bool,
}

/// Run one git command reading stdout only up to `cap` bytes — a huge
/// diff must bound memory as well as time. Past the cap the child is
/// killed and the head is returned marked truncated; exit status only
/// The user directories macOS puts behind a Files-and-Folders grant.
///
/// Pure path logic, `home` passed in, so it is testable without touching a
/// real `$HOME` — and so the table is readable as a list rather than buried
/// in string surgery.
///
/// This list is deliberately short. It is the three directories a person
/// actually keeps repositories in plus iCloud Drive, not an attempt to
/// mirror TCC. A wrong `Some` here puts a misleading sentence in front of
/// somebody, which is the exact failure this whole change exists to fix, so
/// the bar for adding a row is that repositories really live there.
fn gated_root(cwd: &Path, home: &Path) -> Option<String> {
    const GATED: [&str; 4] = [
        "Desktop",
        "Documents",
        "Downloads",
        "Library/Mobile Documents",
    ];
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    for name in GATED {
        let root = home.join(name);
        if cwd.starts_with(&root) {
            return Some(if name == "Library/Mobile Documents" {
                "iCloud Drive".to_string()
            } else {
                format!("~/{name}")
            });
        }
    }
    None
}

/// [`gated_root`] against the real home directory, and only on the platform
/// where the claim is true. On any other OS these directories carry no such
/// gate, and saying they do would be inventing a cause.
fn permission_gate(cwd: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        gated_root(cwd, &home)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cwd;
        None
    }
}

/// The state a git call that never came back should report.
///
/// Never [`GitState::Error`]: the process was alive when the budget ran
/// out, so git is not known to have failed at anything.
fn unanswered(what: &str, cwd: &Path, timeout: Duration) -> GitState {
    GitState::Unanswered {
        what: what.to_string(),
        secs: timeout.as_secs_f32(),
        gate: permission_gate(cwd),
    }
}

/// matters for complete reads.
async fn run_capped(
    git: &Path,
    cwd: &Path,
    timeout: Duration,
    args: &[String],
    cap: usize,
) -> Result<CappedOutput, GitState> {
    let mut child = match Command::new(git)
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(GitState::Error {
                message: format!("git failed to start: {e}"),
            })
        }
    };
    let timed_out = || unanswered(args.first().map(String::as_str).unwrap_or(""), cwd, timeout);
    let mut stdout = child.stdout.take().expect("stdout piped");
    let deadline = tokio::time::Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = match tokio::time::timeout_at(deadline, stdout.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                return Err(GitState::Error {
                    message: format!("git diff read: {e}"),
                })
            }
            Err(_elapsed) => return Err(timed_out()),
        };
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > cap {
            // Enough: kill the child (drop does) and state the cut.
            drop(child);
            bytes.truncate(cap);
            return Ok(CappedOutput {
                bytes,
                truncated: true,
            });
        }
    }
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(GitState::Error {
                message: format!("git diff wait: {e}"),
            })
        }
        Err(_elapsed) => return Err(timed_out()),
    };
    if status.success() {
        return Ok(CappedOutput {
            bytes,
            truncated: false,
        });
    }
    let mut stderr_bytes = Vec::new();
    if let Some(mut se) = child.stderr.take() {
        let _ = tokio::time::timeout_at(deadline, se.read_to_end(&mut stderr_bytes)).await;
    }
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if stderr.contains("not a git repository") {
        return Err(GitState::NotARepo);
    }
    Err(GitState::Error {
        message: format!(
            "git {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            stderr.lines().next().unwrap_or("(no stderr)")
        ),
    })
}

/// Fold raw `git diff` output into hunks, bounded by [`DIFF_MAX_LINES`].
/// Special shapes surface as their own kinds: `Binary files … differ`,
/// and `Subproject commit` pairs (a submodule pointer move / dirty
/// worktree). Conflicted files arrive in combined (`diff --cc`) format;
/// first-byte classification renders them approximately — every line is
/// still shown, ops may fold the two parents together.
fn parse_diff(bytes: &[u8], byte_truncated: bool) -> DiffKind {
    let text = String::from_utf8_lossy(bytes);
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut line_count = 0usize;
    let mut truncated = byte_truncated;
    let mut sub_old: Option<String> = None;
    let mut sub_new: Option<String> = None;
    'lines: for line in text.lines() {
        if line.starts_with("Binary files ") && line.ends_with(" differ") {
            return DiffKind::Binary;
        }
        if let Some(sha) = line.strip_prefix("-Subproject commit ") {
            sub_old = Some(sha.trim().to_string());
            continue;
        }
        if let Some(sha) = line.strip_prefix("+Subproject commit ") {
            sub_new = Some(sha.trim().to_string());
            continue;
        }
        if line.starts_with("@@") {
            if line_count >= DIFF_MAX_LINES {
                truncated = true;
                break 'lines;
            }
            hunks.push(DiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        // File-header lines (diff --git, index, ---/+++, mode) precede
        // the first hunk; skip anything not inside one.
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        let (op, content) = match line.as_bytes().first() {
            Some(b'+') => (DiffOp::Add, &line[1..]),
            Some(b'-') => (DiffOp::Del, &line[1..]),
            // `\ No newline at end of file` — a git artifact, not content.
            Some(b'\\') => continue,
            Some(b' ') => (DiffOp::Ctx, &line[1..]),
            _ => (DiffOp::Ctx, line),
        };
        if line_count >= DIFF_MAX_LINES {
            truncated = true;
            break 'lines;
        }
        line_count += 1;
        hunk.lines.push(DiffLine {
            op,
            text: content.to_string(),
        });
    }
    if byte_truncated {
        // The cap may have cut mid-line; drop the possibly-partial tail.
        if let Some(h) = hunks.last_mut() {
            h.lines.pop();
        }
    }
    if sub_old.is_some() || sub_new.is_some() {
        return DiffKind::Submodule {
            detail: submodule_detail(sub_old, sub_new),
        };
    }
    DiffKind::Text { hunks, truncated }
}

/// `abc1234 → def5678`, `(dirty)` when the worktree is; a dirty-only
/// submodule (pointer unmoved) states that instead of a fake move.
fn submodule_detail(old: Option<String>, new: Option<String>) -> String {
    let short = |s: &str| s.chars().take(7).collect::<String>();
    let (new_sha, dirty) = match new {
        Some(n) => match n.strip_suffix("-dirty") {
            Some(base) => (Some(base.to_string()), true),
            None => (Some(n), false),
        },
        None => (None, false),
    };
    match (old, new_sha) {
        (Some(o), Some(n)) if o == n => format!("dirty worktree at {}", short(&o)),
        (Some(o), Some(n)) => format!(
            "{} → {}{}",
            short(&o),
            short(&n),
            if dirty { " (dirty)" } else { "" }
        ),
        (Some(o), None) => format!("removed at {}", short(&o)),
        (None, Some(n)) => format!(
            "added at {}{}",
            short(&n),
            if dirty { " (dirty)" } else { "" }
        ),
        (None, None) => "changed".to_string(),
    }
}

/// An untracked file's diff kind: all-added lines synthesized from its
/// own content, with symlinks and binaries stated as what they are.
async fn untracked_kind(path: &Path) -> DiffKind {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) => {
            return DiffKind::Error {
                message: format!("file unreadable (changed underneath?): {e}"),
            }
        }
    };
    if meta.file_type().is_symlink() {
        return match tokio::fs::read_link(path).await {
            Ok(target) => DiffKind::Symlink {
                target: target.display().to_string(),
            },
            Err(e) => DiffKind::Error {
                message: format!("symlink unreadable: {e}"),
            },
        };
    }
    if !meta.is_file() {
        return DiffKind::Error {
            message: "not a regular file".into(),
        };
    }
    // Capped read: a multi-GB untracked file must cost at most the cap.
    let mut bytes = Vec::new();
    let truncated_bytes = match tokio::fs::File::open(path).await {
        Ok(f) => {
            let mut take = f.take(DIFF_MAX_BYTES as u64 + 1);
            if let Err(e) = take.read_to_end(&mut bytes).await {
                return DiffKind::Error {
                    message: format!("file unreadable: {e}"),
                };
            }
            let over = bytes.len() > DIFF_MAX_BYTES;
            bytes.truncate(DIFF_MAX_BYTES);
            over
        }
        Err(e) => {
            return DiffKind::Error {
                message: format!("file unreadable: {e}"),
            }
        }
    };
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return DiffKind::Binary;
    }
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut truncated = truncated_bytes;
    for l in String::from_utf8_lossy(&bytes).lines() {
        if lines.len() >= DIFF_MAX_LINES {
            truncated = true;
            break;
        }
        lines.push(DiffLine {
            op: DiffOp::Add,
            text: l.to_string(),
        });
    }
    if truncated_bytes {
        lines.pop(); // the cap may have cut mid-line
    }
    let total = lines.len();
    if total == 0 && !truncated {
        return DiffKind::Text {
            hunks: Vec::new(),
            truncated: false,
        };
    }
    DiffKind::Text {
        hunks: vec![DiffHunk {
            header: format!("@@ -0,0 +1,{total} @@"),
            lines,
        }],
        truncated,
    }
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

    // -- Slice B2: per-file diffs ------------------------------------------

    /// Scan then diff one path through the same route the engine uses:
    /// the snapshot's entry (untracked/orig_path facts included) feeds
    /// the diff strategy.
    async fn scan_and_diff(dir: &Path, path: &str) -> FileDiff {
        let snap = scan(dir).await;
        let f = by_path(&snap, path).clone();
        diff(dir, &f, None).await
    }

    fn text_hunks(kind: &DiffKind) -> &Vec<DiffHunk> {
        match kind {
            DiffKind::Text {
                hunks,
                truncated: false,
            } => hunks,
            other => panic!("expected untruncated text diff, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn modified_file_diff_has_hunks_without_marker_prefixes() {
        let dir = repo_with_commit();
        std::fs::write(dir.join("a.txt"), "one\nTWO!\nthree\n").unwrap();
        let d = scan_and_diff(&dir, "a.txt").await;
        let hunks = text_hunks(&d.kind);
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].header.starts_with("@@"), "{}", hunks[0].header);
        let ops: Vec<(DiffOp, &str)> = hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.text.as_str()))
            .collect();
        assert_eq!(
            ops,
            vec![
                (DiffOp::Ctx, "one"),
                (DiffOp::Del, "two"),
                (DiffOp::Add, "TWO!"),
                (DiffOp::Ctx, "three"),
            ]
        );
    }

    #[tokio::test]
    async fn untracked_file_diff_is_all_added() {
        let dir = repo_with_commit();
        std::fs::write(dir.join("fresh.txt"), "alpha\nbeta\n").unwrap();
        let d = scan_and_diff(&dir, "fresh.txt").await;
        let hunks = text_hunks(&d.kind);
        assert_eq!(hunks.len(), 1);
        let ops: Vec<(DiffOp, &str)> = hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.text.as_str()))
            .collect();
        assert_eq!(ops, vec![(DiffOp::Add, "alpha"), (DiffOp::Add, "beta")]);
    }

    #[tokio::test]
    async fn untracked_binary_is_stated() {
        let dir = repo_with_commit();
        std::fs::write(dir.join("blob.bin"), [1u8, 0, 2, 0]).unwrap();
        let d = scan_and_diff(&dir, "blob.bin").await;
        assert_eq!(d.kind, DiffKind::Binary);
    }

    #[tokio::test]
    async fn tracked_binary_change_is_stated() {
        let dir = repo_with_commit();
        std::fs::write(dir.join("blob.bin"), [1u8, 0, 2, 0]).unwrap();
        git(&dir, &["add", "blob.bin"]);
        git(&dir, &["commit", "-qm", "bin"]);
        std::fs::write(dir.join("blob.bin"), [9u8, 0, 9, 9, 0]).unwrap();
        let d = scan_and_diff(&dir, "blob.bin").await;
        assert_eq!(d.kind, DiffKind::Binary);
    }

    #[tokio::test]
    async fn untracked_symlink_states_its_target() {
        let dir = repo_with_commit();
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
        let d = scan_and_diff(&dir, "link").await;
        assert_eq!(
            d.kind,
            DiffKind::Symlink {
                target: "a.txt".into()
            }
        );
    }

    #[tokio::test]
    async fn deleted_file_diff_is_all_removed() {
        let dir = repo_with_commit();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        let d = scan_and_diff(&dir, "gone.txt").await;
        let hunks = text_hunks(&d.kind);
        let ops: Vec<(DiffOp, &str)> = hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.text.as_str()))
            .collect();
        assert_eq!(ops, vec![(DiffOp::Del, "x"), (DiffOp::Del, "y")]);
    }

    #[tokio::test]
    async fn pure_rename_captures_orig_and_has_no_line_changes() {
        let dir = repo_with_commit();
        git(&dir, &["mv", "a.txt", "b.txt"]);
        let snap = scan(&dir).await;
        let f = by_path(&snap, "b.txt").clone();
        assert_eq!(f.orig_path.as_deref(), Some("a.txt"));
        let d = diff(&dir, &f, None).await;
        let hunks = text_hunks(&d.kind);
        assert!(hunks.is_empty(), "pure rename has no hunks: {hunks:?}");
    }

    #[tokio::test]
    async fn rename_with_edit_shows_the_edit() {
        let dir = repo_with_commit();
        git(&dir, &["mv", "a.txt", "b.txt"]);
        std::fs::write(dir.join("b.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let d = scan_and_diff(&dir, "b.txt").await;
        let hunks = text_hunks(&d.kind);
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert!(hunks[0]
            .lines
            .iter()
            .any(|l| l.op == DiffOp::Add && l.text == "four"));
    }

    #[tokio::test]
    async fn submodule_pointer_move_is_stated() {
        let inner = repo_with_commit();
        let dir = repo_with_commit();
        let out = StdCommand::new("git")
            .args([
                "-c",
                "protocol.file.allow=always",
                "-c",
                "user.email=t@e",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "submodule",
                "add",
                &inner.display().to_string(),
                "sub",
            ])
            .current_dir(&dir)
            .output()
            .expect("submodule add");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        git(&dir, &["commit", "-qm", "add sub"]);
        // Move the submodule's checked-out commit: the outer repo now
        // sees a pointer change.
        let sub = dir.join("sub");
        std::fs::write(sub.join("more.txt"), "z\n").unwrap();
        git(&sub, &["add", "."]);
        git(&sub, &["commit", "-qm", "advance"]);
        let d = scan_and_diff(&dir, "sub").await;
        match d.kind {
            DiffKind::Submodule { detail } => {
                assert!(detail.contains('→'), "{detail}");
            }
            other => panic!("expected submodule fact, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_commit_repo_diffs_against_the_empty_tree() {
        let dir = scratch();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("staged.txt"), "a\nb\n").unwrap();
        git(&dir, &["add", "."]);
        let d = scan_and_diff(&dir, "staged.txt").await;
        let hunks = text_hunks(&d.kind);
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].lines.iter().all(|l| l.op == DiffOp::Add));
    }

    #[tokio::test]
    async fn conflicted_file_still_renders_as_text() {
        let dir = repo_with_commit();
        git(&dir, &["checkout", "-qb", "side"]);
        std::fs::write(dir.join("a.txt"), "one\nSIDE\nthree\n").unwrap();
        git(&dir, &["commit", "-aqm", "side"]);
        git(&dir, &["checkout", "-q", "-"]);
        std::fs::write(dir.join("a.txt"), "one\nMAIN\nthree\n").unwrap();
        git(&dir, &["commit", "-aqm", "main"]);
        let _ = StdCommand::new("git")
            .args(["merge", "side"])
            .current_dir(&dir)
            .output()
            .expect("merge attempt"); // fails: conflict — that's the point
        let snap = scan(&dir).await;
        let f = by_path(&snap, "a.txt").clone();
        assert_eq!(f.status, FileStatus::Conflicted);
        let d = diff(&dir, &f, None).await;
        match d.kind {
            DiffKind::Text { hunks, .. } => {
                assert!(!hunks.is_empty(), "conflict diff should show hunks")
            }
            other => panic!("expected text (combined) diff, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn huge_diff_truncates_at_the_line_cap() {
        let dir = repo_with_commit();
        let body: String = (0..(DIFF_MAX_LINES + 500))
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(dir.join("big.txt"), body).unwrap();
        let d = scan_and_diff(&dir, "big.txt").await;
        match d.kind {
            DiffKind::Text { hunks, truncated } => {
                assert!(truncated, "line cap must state truncation");
                let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
                assert!(total <= DIFF_MAX_LINES, "{total}");
                assert!(total >= DIFF_MAX_LINES - 1, "{total}");
            }
            other => panic!("expected truncated text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn huge_lines_truncate_at_the_byte_cap() {
        let dir = repo_with_commit();
        // ~100 lines × 10 KB ≈ 1 MB — few lines, many bytes: the byte
        // cap (run_capped's bounded stdout read) must fire before the
        // line cap ever would. Tracked, so the diff comes from git.
        std::fs::write(dir.join("wide.txt"), "small\n").unwrap();
        git(&dir, &["add", "wide.txt"]);
        git(&dir, &["commit", "-qm", "wide"]);
        let long = "x".repeat(10 * 1024);
        let body: String = (0..100).map(|i| format!("{i}{long}\n")).collect();
        std::fs::write(dir.join("wide.txt"), body).unwrap();
        let d = scan_and_diff(&dir, "wide.txt").await;
        match d.kind {
            DiffKind::Text { truncated, .. } => {
                assert!(truncated, "byte cap must state truncation")
            }
            other => panic!("expected truncated text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_cap_states_the_omitted_count() {
        let dir = repo_with_commit();
        for i in 0..(MAX_FILES + 150) {
            std::fs::write(dir.join(format!("f{i:05}.txt")), "x\n").unwrap();
        }
        let snap = scan(&dir).await;
        assert_eq!(snap.state, GitState::Ok);
        assert_eq!(snap.files.len(), MAX_FILES);
        assert_eq!(snap.omitted, 150);
        // Untracked counting is itself capped: the head got counts, the
        // tail states `—` instead of costing hundreds more reads.
        assert_eq!(snap.files[0].adds, Some(1));
        assert_eq!(snap.files[UNTRACKED_COUNT_MAX + 10].adds, None);
    }

    // -- Slice I2: worktree plumbing ---------------------------------------

    #[tokio::test]
    async fn checkout_root_resolves_toplevel_and_non_git() {
        let dir = repo_with_commit();
        let sub = dir.join("nested/deeper");
        std::fs::create_dir_all(&sub).unwrap();
        let probe = checkout_root(&sub).await;
        assert!(probe.is_repo);
        assert_eq!(probe.root, std::fs::canonicalize(&dir).unwrap());

        let plain = scratch();
        let probe = checkout_root(&plain).await;
        assert!(!probe.is_repo);
        assert_eq!(probe.root, std::fs::canonicalize(&plain).unwrap());
    }

    #[tokio::test]
    async fn worktree_add_from_dirty_base_leaves_base_untouched() {
        let base = repo_with_commit();
        // Base is dirty at add time — the stated edge case: the worktree
        // starts clean at HEAD; the base keeps its uncommitted change.
        std::fs::write(base.join("a.txt"), "dirty edit\n").unwrap();
        let wt = scratch().join("wt");
        worktree_add(&base, &wt, "eave/test1234").await.unwrap();

        let wt_probe = checkout_root(&wt).await;
        assert!(wt_probe.is_repo);
        assert_eq!(wt_probe.root, std::fs::canonicalize(&wt).unwrap());

        let wt_snap = scan(&wt).await;
        assert_eq!(wt_snap.state, GitState::Ok);
        assert!(
            wt_snap.files.is_empty(),
            "worktree starts clean: {:?}",
            wt_snap.files
        );

        let base_snap = scan(&base).await;
        assert_eq!(base_snap.files.len(), 1, "base keeps its dirty edit");

        // The worktree's git dir is the base's private stanza — the part
        // the FS watcher must additionally cover.
        let gd = git_dir(&wt).await.unwrap();
        assert!(
            gd.starts_with(std::fs::canonicalize(&base).unwrap().join(".git/worktrees"))
                || gd.starts_with(base.join(".git/worktrees")),
            "{}",
            gd.display()
        );

        let listed = worktree_list(&base).await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
    }

    #[tokio::test]
    async fn worktree_add_refusals_are_stated() {
        let base = repo_with_commit();
        let wt = scratch().join("wt");
        worktree_add(&base, &wt, "eave/dup").await.unwrap();
        // Same branch again: git refuses with its own message.
        let err = worktree_add(&base, &scratch().join("wt2"), "eave/dup")
            .await
            .unwrap_err();
        assert!(err.contains("dup") || err.contains("already"), "{err}");
        // Non-git base: stated, not a crash.
        let err = worktree_add(&scratch(), &scratch().join("wt3"), "eave/x")
            .await
            .unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");
    }

    #[tokio::test]
    async fn dirty_worktree_remove_needs_force_and_branch_cleanup_completes() {
        let base = repo_with_commit();
        let wt = scratch().join("wt");
        worktree_add(&base, &wt, "eave/abcd1234").await.unwrap();
        std::fs::write(wt.join("work.txt"), "uncommitted\n").unwrap();

        // git's own dirty guard — the second, independent layer under the
        // engine's check.
        let err = worktree_remove(&base, &wt, false).await.unwrap_err();
        assert!(
            err.contains("dirty") || err.contains("--force") || err.contains("contains modified"),
            "{err}"
        );
        assert!(wt.exists());

        worktree_remove(&base, &wt, true).await.unwrap();
        assert!(!wt.exists());
        worktree_prune(&base).await.unwrap();
        branch_delete(&base, "eave/abcd1234").await.unwrap();
        assert_eq!(worktree_list(&base).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rm_rfed_worktree_prunes_clean() {
        let base = repo_with_commit();
        let wt = scratch().join("wt");
        worktree_add(&base, &wt, "eave/gone0000").await.unwrap();
        // The operator deleted the directory out from under git.
        std::fs::remove_dir_all(&wt).unwrap();
        worktree_prune(&base).await.unwrap();
        branch_delete(&base, "eave/gone0000").await.unwrap();
        assert_eq!(worktree_list(&base).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unmerged_count_sees_worktree_only_commits() {
        let base = repo_with_commit();
        let wt = scratch().join("wt");
        worktree_add(&base, &wt, "eave/cnt00000").await.unwrap();
        assert_eq!(unmerged_count(&base, "eave/cnt00000").await, Some(0));
        std::fs::write(wt.join("new.txt"), "z\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "wt work"]);
        assert_eq!(unmerged_count(&base, "eave/cnt00000").await, Some(1));
        // Merge it into the base's branch: no longer unique, safe to drop.
        git(&base, &["merge", "-q", "eave/cnt00000"]);
        assert_eq!(unmerged_count(&base, "eave/cnt00000").await, Some(0));
    }

    /// The diff path honors the same hang discipline as the scan.
    #[tokio::test]
    async fn hanging_git_diff_is_killed_and_stated() {
        let dir = scratch();
        let fake = dir.join("slow-git");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = ChangedFile {
            path: "a.txt".into(),
            status: FileStatus::Modified,
            adds: None,
            dels: None,
            orig_path: None,
            untracked: false,
        };
        let started = std::time::Instant::now();
        let d = diff_with(&fake, &dir, Duration::from_millis(200), &file, None).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        // The diff pane collapses every state into `DiffKind::Error`, so
        // the wording is all it has. Assert the negative as well: this is
        // the surface that used to print `git failed: … timed out`, and a
        // regression here is a sentence, not a type error.
        match d.kind {
            DiffKind::Error { message } => {
                assert!(message.contains("did not answer"), "{message}");
                assert!(
                    !message.contains("git failed"),
                    "git did not fail, it did not answer: {message}"
                );
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
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
        // `Unanswered`, NOT `Error`: the fake git was alive and sleeping
        // when the budget expired, so nothing failed. Distinguishing those
        // two is the whole point of the variant.
        assert!(!snap.state.stated().contains("git failed"));
        match snap.state {
            GitState::Unanswered { what, secs, gate } => {
                assert_eq!(what, "status");
                assert!(secs > 0.0, "the budget is reported, got {secs}");
                assert_eq!(gate, None, "a scratch dir is not a gated folder");
            }
            other => panic!("a hanging git reported {other:?} instead of Unanswered"),
        }
    }

    // --- Slice AJ — which folders macOS gates ------------------------------

    /// The table is a whitelist and it behaves like one on both sides.
    ///
    /// A wrong `Some` here is the worse error: it puts "macOS gates this
    /// folder" in front of somebody whose problem is something else, which
    /// is the same class of mistake as the `git failed` message this whole
    /// change exists to remove.
    #[test]
    fn only_the_gated_folders_are_called_gated() {
        let home = std::path::Path::new("/Users/somebody");
        for (path, want) in [
            ("/Users/somebody/Desktop/eave", Some("~/Desktop")),
            ("/Users/somebody/Documents/x/y", Some("~/Documents")),
            ("/Users/somebody/Downloads", Some("~/Downloads")),
            (
                "/Users/somebody/Library/Mobile Documents/com~apple~CloudDocs/r",
                Some("iCloud Drive"),
            ),
            ("/Users/somebody/code/eave", None),
            ("/Users/somebody", None),
            ("/tmp/eave", None),
            ("/Users/somebody/Library/Caches", None),
        ] {
            assert_eq!(
                super::gated_root(std::path::Path::new(path), home).as_deref(),
                want,
                "{path}"
            );
        }
    }

    /// A sibling whose name merely STARTS with a gated one is not gated.
    ///
    /// `starts_with` on `Path` compares whole components, so this holds —
    /// but it holds by a property of the type rather than by anything
    /// visible at the call site, and the naive string version of this
    /// function would answer `Some` for all three.
    #[test]
    fn a_name_that_merely_starts_with_a_gated_one_is_not_gated() {
        let home = std::path::Path::new("/Users/somebody");
        for path in [
            "/Users/somebody/Desktops/eave",
            "/Users/somebody/Documents-old/eave",
            "/Users/somebody/DownloadsArchive",
        ] {
            assert_eq!(
                super::gated_root(std::path::Path::new(path), home),
                None,
                "{path}"
            );
        }
    }

    /// A timeout reports `Unanswered`, never `Error`. This is the engine
    /// half of `a_timeout_never_says_git_failed`: that test proves the
    /// SENTENCE is right, this one proves the engine hands over the state
    /// that produces it.
    #[tokio::test]
    async fn a_git_call_that_never_returns_is_unanswered_not_an_error() {
        let dir = std::env::temp_dir().join("aj-unanswered");
        std::fs::create_dir_all(&dir).expect("scratch");
        // `sleep` stands in for a git that is blocked rather than broken —
        // a real pending permission grant blocks the syscall, so the
        // process is alive and healthy when the budget expires, which is
        // precisely the state that used to be reported as a git failure.
        let err = super::run(
            std::path::Path::new("/bin/sleep"),
            &dir,
            std::time::Duration::from_millis(120),
            &["30"],
        )
        .await
        .expect_err("must not succeed inside the budget");
        match err {
            eave_wire::GitState::Unanswered { what, secs, .. } => {
                assert_eq!(what, "30", "the subcommand is carried through");
                assert!(secs > 0.0, "the budget is reported, got {secs}");
            }
            other => panic!("a blocked call reported {other:?} instead of Unanswered"),
        }
    }
}
