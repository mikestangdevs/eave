//! GitHub reviews, read through the operator's own `gh` (Slice GH1).
//!
//! **eave holds no GitHub credential and never asks for one.** There is
//! no OAuth app, no token in `settings.json`, no keychain entry of its
//! own: every call here shells out to the GitHub CLI the operator
//! already signed in with, exactly the way the rest of eave drives the
//! six agent CLIs. That is the same choice the reference implementation
//! made (`gh auth login`, and a provider CLI per host), and it is worth
//! naming as a choice rather than an accident — it means eave's blast
//! radius on a compromised machine is what `gh` can already do, and it
//! means a repository the operator can see is a repository eave can see,
//! with no permission dialog of its own.
//!
//! **Stacks are asked for, not derived.** GitHub's GraphQL schema
//! carries `PullRequest.stack` and `PullRequest.stackEntry` natively —
//! introspected against the live API on 2026-09-14, not read out of
//! anyone's source tree — so one query answers a review's whole stack.
//! Chaining by base-to-head is a *fallback* for repositories not using
//! the feature and lives in the app's grouping pass, because it is an
//! inference and this module reports.
//!
//! **The query is a constant and the repository rides as variables.**
//! `gh api graphql --input -` takes a `{query, variables}` body on
//! stdin, so no owner or branch name is ever pasted into a query string.
//! The reference implementation interpolates and defends with a
//! character-class test; taking variables means there is nothing to
//! defend.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eave_core::provider::{resolve_spec, ProgramResolution, ProgramSpec};
use eave_core::time::rfc3339_epoch_ms;
use eave_wire::pulls::{
    ChecksState, PullRequest, PullState, PullsState, RepoId, RepoReviews, ReviewsSnapshot,
    SkippedRoot, StackRef,
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The GitHub CLI, found the way every other CLI here is found — `PATH`
/// first, then the Homebrew and `~/.local/bin` fallbacks that make a
/// Finder-launched app behave like a shell-launched one.
const GH: ProgramSpec = ProgramSpec {
    bin: "gh",
    also_look_in: &[],
};

/// Where `gh` is, when an operator's copy is somewhere eave does not
/// look. Spelled like the provider overrides and for the same reason.
const GH_BIN_OVERRIDE: &str = "EAVE_GH_BIN";

/// Longer than `git::GIT_TIMEOUT` **because this one crosses a
/// network.** Ten seconds is generous for a local `git status` and mean
/// for a GraphQL round trip on hotel wifi; a read that gives up while
/// the answer is in flight reports "the read failed" about a working
/// setup, which is the least useful thing this module could say.
pub const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// The local git calls — naming a remote is filesystem work.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many reviews one listing asks for.
///
/// The reference draws a scrolling list and pages; eave does not page
/// yet, so this is the whole answer and it is stated as such by the
/// surface rather than trailing off silently.
pub const DEFAULT_LIMIT: usize = 50;

/// The only host this slice can answer for.
///
/// GitHub Enterprise checkouts are ordinary and eave refuses them **by
/// name** ([`PullsState::UnsupportedHost`]) rather than reporting an
/// empty list: "eave does not do this yet" and "you have no reviews"
/// are different sentences, and only one of them is true.
const SUPPORTED_HOST: &str = "github.com";

/// Every field a list row draws, as a fragment so a document asking
/// about a dozen repositories says it once.
///
/// `statusCheckRollup` is read off the **last** commit because that is
/// the head the checks belong to; asking the pull request for its checks
/// directly is not a thing the schema offers.
const PULLS_FRAGMENT: &str = r#"fragment Row on PullRequest {
  number title state isDraft url updatedAt
  headRefName baseRefName
  additions deletions changedFiles
  author { login }
  stackEntry { position }
  stack { number size }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}
fragment Open on Repository {
  pullRequests(states: OPEN, first: $n, orderBy: {field: UPDATED_AT, direction: DESC}) {
    nodes { ...Row }
  }
}
fragment Recent on Repository {
  pullRequests(states: [MERGED, CLOSED], first: $m, orderBy: {field: UPDATED_AT, direction: DESC}) {
    nodes { ...Row }
  }
}"#;

/// How many settled reviews a repository's history shows.
///
/// **Separate from the open list on purpose.** The operator asked for
/// *"all repos and their history"*, and one list ordered by update time
/// answers neither question well: on a busy repository the fifty most
/// recently touched reviews are almost all merged, which buries the
/// handful that are actually open. So the query asks twice, in one
/// document, and the surface draws them as two sections. It is also what
/// makes the merged mark reachable at all — the purple glyph was drawn
/// and coloured in the first pass and no query could ever produce it.
pub const RECENT_LIMIT: usize = 15;

/// How many repositories ride in one GraphQL document.
///
/// Small enough that a chunk stays well inside GitHub's query-cost
/// budget and that one unlucky repository's timeout does not take a
/// whole overview down with it; large enough that the common case — an
/// operator with a handful of projects — is a single round trip.
const CHUNK: usize = 8;

/// How many local checkouts eave will resolve out of one scan.
///
/// A folder of folders is what an operator points this at, and pointing
/// it at a home directory should cost a bounded amount of work rather
/// than walking a disk. The cap is stated by the surface when it binds,
/// because a silently truncated list is the same lie as a silently
/// truncated count.
pub const MAX_CHECKOUTS: usize = 128;

/// How deep a scan looks for a checkout under a folder it was given.
///
/// Three levels reaches `~/Desktop/DevProjects/remake-wonderland` and
/// `~/Desktop/eave/wt-gh` — a project folder, a workspace, a worktree —
/// and stops well short of walking a source tree.
const SCAN_DEPTH: usize = 3;

/// Folders a scan never descends into. Dot-directories are skipped
/// wholesale (`.git` included, which is what keeps a checkout from being
/// scanned for checkouts), and these are the two that are neither hidden
/// nor ever a checkout of the operator's own.
const SKIP_DIRS: &[&str] = &["node_modules", "target"];

/// Build one document for `n` repositories.
///
/// **Nothing from the operator is pasted into it.** The aliases and the
/// variable names are built from an index — `p0`, `$o0`, `$r0` — and
/// every owner and name still rides in the JSON variables, so the only
/// interpolated text in the whole document is a `usize` this function
/// counted itself.
fn batch_query(n: usize) -> String {
    let mut vars = String::from("$n: Int!, $m: Int!");
    let mut body = String::new();
    for i in 0..n {
        vars.push_str(&format!(", $o{i}: String!, $r{i}: String!"));
        body.push_str(&format!(
            "  p{i}: repository(owner: $o{i}, name: $r{i}) {{ ...Open }}\n\
             \x20 q{i}: repository(owner: $o{i}, name: $r{i}) {{ ...Recent }}\n"
        ));
    }
    format!("query Reviews({vars}) {{\n{body}}}\n{PULLS_FRAGMENT}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every open review on every repository under `roots`.
pub async fn reviews(roots: &[String], limit: usize) -> ReviewsSnapshot {
    reviews_with(Path::new("git"), roots, limit).await
}

/// [`reviews`] with the git binary injectable, so a test can hand it a
/// fake remote without a network or a real checkout.
pub async fn reviews_with(git: &Path, roots: &[String], limit: usize) -> ReviewsSnapshot {
    let scan = expand_roots(roots);
    let mut skipped = scan.skipped;

    // Name each checkout's repository **locally**, before anything
    // crosses a network. This is what collapses five worktrees of one
    // repository into one row and one question, rather than asking
    // GitHub the same thing five times and printing the answer five
    // times.
    let named = name_checkouts(git, scan.checkouts).await;

    let mut repos: Vec<RepoReviews> = Vec::new();
    for (root, named) in named {
        match named {
            Err(state) => skipped.push(SkippedRoot { root, state }),
            Ok((id, branch)) => match repos.iter().position(|r| r.repo == id) {
                Some(ix) => {
                    repos[ix].roots.push(root);
                    repos[ix].branches.extend(branch);
                }
                None => repos.push(RepoReviews {
                    repo: id,
                    roots: vec![root],
                    branches: branch.into_iter().collect(),
                    rows: Vec::new(),
                    recent: Vec::new(),
                    // Nothing has been asked yet. Overwritten by the
                    // read below, and never left as `Ok` with no rows —
                    // an unasked repository and one with no open reviews
                    // are different facts.
                    state: PullsState::Error {
                        message: "not read".to_string(),
                    },
                }),
            },
        }
    }

    if !repos.is_empty() {
        match resolve_spec(&GH, GH_BIN_OVERRIDE) {
            ProgramResolution::Found(gh) => read_all(&gh, &mut repos, limit).await,
            ProgramResolution::NotInstalled { searched, .. } => {
                let searched: Vec<String> =
                    searched.iter().map(|p| p.display().to_string()).collect();
                for r in repos.iter_mut() {
                    r.state = PullsState::CliMissing {
                        searched: searched.clone(),
                    };
                }
            }
        }
    }

    // **Most recently touched first.** A repository with a review
    // updated an hour ago is the one the operator is working in; one
    // eave could not read sinks to the bottom, where it is still visible
    // and still says why.
    repos.sort_by(|a, b| {
        let key = |r: &RepoReviews| {
            (
                matches!(r.state, PullsState::Ok),
                r.rows
                    .iter()
                    .chain(r.recent.iter())
                    .map(|p| p.updated_ms)
                    .max()
                    .unwrap_or(0),
            )
        };
        key(b)
            .cmp(&key(a))
            .then_with(|| a.repo.slug().cmp(&b.repo.slug()))
    });
    skipped.sort_by(|a, b| a.root.cmp(&b.root));

    ReviewsSnapshot {
        repos,
        skipped,
        read_ms: now_ms(),
        roots: roots.to_vec(),
        truncated: scan.truncated,
    }
}

/// What a scan found.
pub struct Scan {
    pub checkouts: Vec<String>,
    /// Folders eave was pointed at and could not look inside, with the
    /// operating system's own words.
    pub skipped: Vec<SkippedRoot>,
    /// The scan hit [`MAX_CHECKOUTS`] with folders left to look at.
    pub truncated: bool,
}

/// Turn the folders eave was pointed at into the checkouts inside them.
///
/// A root that is a checkout is itself; a root that is not is **scanned**
/// for the checkouts it holds, which is what lets an operator point eave
/// at the folder their projects live in rather than adding twenty
/// folders one at a time. Both a `.git` directory and a `.git` file (a
/// worktree, which is how every lane in this project works) count.
pub fn expand_roots(roots: &[String]) -> Scan {
    let mut scan = Scan {
        checkouts: Vec::new(),
        skipped: Vec::new(),
        truncated: false,
    };
    for root in roots {
        let path = Path::new(root);
        // **Can this process see the folder at all?** Asked before
        // anything is concluded about what is in it — a refusal to read
        // says nothing about whether a checkout is there, and reporting
        // it as "no repositories" sends an operator looking for a
        // problem in their folders that is not in them.
        if let Err(e) = std::fs::read_dir(path) {
            scan.skipped.push(SkippedRoot {
                root: root.clone(),
                state: PullsState::Unreadable {
                    path: root.clone(),
                    why: e.to_string(),
                },
            });
            continue;
        }
        if is_checkout(path) {
            push_unique(&mut scan.checkouts, root.clone());
            continue;
        }
        scan_into(path, SCAN_DEPTH, &mut scan);
    }
    scan
}

fn is_checkout(path: &Path) -> bool {
    path.join(".git").exists()
}

fn push_unique(out: &mut Vec<String>, v: String) {
    if !out.contains(&v) {
        out.push(v);
    }
}

fn scan_into(dir: &Path, depth: usize, scan: &mut Scan) {
    if depth == 0 {
        return;
    }
    if scan.checkouts.len() >= MAX_CHECKOUTS {
        scan.truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sorted, so a capped scan takes the same checkouts every time
    // rather than whatever order the filesystem answered in.
    let mut kids: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    kids.sort();
    for kid in kids {
        if scan.checkouts.len() >= MAX_CHECKOUTS {
            scan.truncated = true;
            return;
        }
        let name = kid.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }
        if is_checkout(&kid) {
            push_unique(&mut scan.checkouts, kid.display().to_string());
            // A checkout is not scanned for checkouts. A vendored
            // dependency with its own `.git` is not a project of the
            // operator's, and a submodule is the same repository again.
            continue;
        }
        scan_into(&kid, depth - 1, scan);
    }
}

/// How many `git` calls run at once while naming checkouts.
const NAMING_JOBS: usize = 16;

/// Resolve every checkout's repository and branch, a few at a time.
type Named = (String, Result<(RepoId, Option<String>), PullsState>);

async fn name_checkouts(git: &Path, checkouts: Vec<String>) -> Vec<Named> {
    let mut out: Vec<Named> = Vec::with_capacity(checkouts.len());
    for chunk in checkouts.chunks(NAMING_JOBS) {
        let mut set = tokio::task::JoinSet::new();
        for (ix, root) in chunk.iter().enumerate() {
            let git = git.to_path_buf();
            let root = root.clone();
            set.spawn(async move {
                let path = Path::new(&root);
                let named = match name_one(&git, path).await {
                    Ok(v) => Ok(v),
                    Err(state) => Err(state),
                };
                (ix, root, named)
            });
        }
        // Collected then re-sorted: a `JoinSet` answers in completion
        // order, and the list an operator sees must not depend on which
        // `git` finished first.
        let mut done: Vec<(usize, String, Result<(RepoId, Option<String>), PullsState>)> =
            Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(v) = res {
                done.push(v);
            }
        }
        done.sort_by_key(|(ix, _, _)| *ix);
        out.extend(done.into_iter().map(|(_, root, named)| (root, named)));
    }
    out
}

/// Ask GitHub about every named repository, a chunk at a time.
///
/// **A failed batch SPLITS rather than taking its repositories with
/// it.** Measured in the walk: one `HTTP 502` — transient, and not
/// reproducible in three runs afterwards — emptied an overview of seven
/// repositories at once, every row wearing the same refusal. Batching is
/// an optimisation, so its failure mode must not be worse than not
/// batching: a group that fails wholesale is halved and retried, down to
/// single repositories, and only a repository that fails **alone** is
/// reported as failed. A flake costs one extra round trip; a repository
/// too heavy to answer inside a document costs only itself.
async fn read_all(gh: &Path, repos: &mut [RepoReviews], limit: usize) {
    let mut queue: Vec<Vec<usize>> = (0..repos.len())
        .collect::<Vec<_>>()
        .chunks(CHUNK)
        .map(<[usize]>::to_vec)
        .collect();
    // Reverse, so the work comes off the stack in the order the
    // repositories are listed and a partly-drawn overview fills top
    // first.
    queue.reverse();
    let mut diagnosed = false;
    while let Some(group) = queue.pop() {
        let Err(message) = read_group(gh, repos, &group, limit).await else {
            continue;
        };
        // **Diagnose only on failure, and only once.** A `gh auth
        // status` probe before every read would spend a process on a
        // question whose answer is almost always yes, and would still
        // race the read it is meant to explain. Asking after the fact
        // costs nothing in the common case and is strictly more
        // accurate: it explains the failure that actually happened. Once
        // rather than per group, because a signed-out `gh` fails every
        // group and splitting would turn one wrong answer into a storm
        // of them.
        if !diagnosed {
            diagnosed = true;
            let host = repos[group[0]].repo.host.clone();
            let cwd = read_cwd(repos, &group);
            if signed_out(gh, Path::new(&cwd), &host).await {
                for r in repos.iter_mut() {
                    if !matches!(r.state, PullsState::Ok) {
                        r.state = PullsState::SignedOut { host: host.clone() };
                    }
                }
                return;
            }
        }
        if group.len() > 1 {
            let mid = group.len() / 2;
            queue.push(group[mid..].to_vec());
            queue.push(group[..mid].to_vec());
            continue;
        }
        repos[group[0]].state = PullsState::Error { message };
    }
}

/// Somewhere to run `gh` from. Any path inside a checkout will do — the
/// repository rides in the variables — and `gh` only needs a directory
/// that exists.
fn read_cwd(repos: &[RepoReviews], group: &[usize]) -> String {
    repos[group[0]]
        .roots
        .first()
        .cloned()
        .unwrap_or_else(|| ".".to_string())
}

/// One document, covering `group`. `Err` is a failure of the whole
/// document; a repository refused on its own row is an `Ok` that wrote
/// that row's state.
async fn read_group(
    gh: &Path,
    repos: &mut [RepoReviews],
    group: &[usize],
    limit: usize,
) -> Result<(), String> {
    let ids: Vec<RepoId> = group.iter().map(|ix| repos[*ix].repo.clone()).collect();
    // Every repository in one group is on one host by construction:
    // `repo_id` refuses anything but `SUPPORTED_HOST`.
    let host = ids[0].host.clone();
    let mut variables = serde_json::Map::new();
    variables.insert("n".into(), serde_json::json!(limit as i64));
    variables.insert("m".into(), serde_json::json!(RECENT_LIMIT as i64));
    for (i, id) in ids.iter().enumerate() {
        variables.insert(format!("o{i}"), serde_json::json!(id.owner));
        variables.insert(format!("r{i}"), serde_json::json!(id.name));
    }
    let body = serde_json::json!({
        "query": batch_query(ids.len()),
        "variables": variables,
    })
    .to_string();

    // The budget grows with the group: one repository on a slow link and
    // eight repositories on a fast one are not the same read, and a
    // timeout that fits the first reports "the read failed" about the
    // second.
    let timeout = GH_TIMEOUT + Duration::from_secs(2 * ids.len() as u64);
    let cwd = read_cwd(repos, group);
    let raw = run_gh(
        gh,
        Path::new(&cwd),
        &["api", "graphql", "--hostname", &host, "--input", "-"],
        Some(body.as_bytes()),
        timeout,
    )
    .await?;

    let mut answers = decode_batch(&raw, ids.len())?;
    for (slot, ix) in group.iter().enumerate() {
        match answers.remove(&slot) {
            Some(Ok((open, recent))) => {
                repos[*ix].rows = open;
                repos[*ix].recent = recent;
                repos[*ix].state = PullsState::Ok;
            }
            // Named by GitHub as this repository's own problem, so it is
            // this repository's own refusal and no retry will change it.
            Some(Err(message)) => repos[*ix].state = PullsState::Error { message },
            None => {
                repos[*ix].state = PullsState::Error {
                    message: "GitHub answered nothing for this repository".to_string(),
                }
            }
        }
    }
    Ok(())
}

/// One checkout's repository and branch — the fast way, then the sure
/// way.
async fn name_one(git: &Path, root: &Path) -> Result<(RepoId, Option<String>), PullsState> {
    // **Can this process see the folder at all?** Asked before anything
    // is concluded about what is in it: a refusal to read says nothing
    // about whether a checkout is there, and reporting it as "not a git
    // checkout" sends an operator looking for a problem in their
    // repository that is not in it. Measured on this machine
    // 2026-09-14: from an ad-hoc-signed bundle every checkout under
    // `~/Desktop` failed this way, and the same paths answered correctly
    // from a unit test run in a terminal.
    if let Err(e) = std::fs::read_dir(root) {
        return Err(PullsState::Unreadable {
            path: root.display().to_string(),
            why: e.to_string(),
        });
    }
    if let Some((url, branch)) = read_checkout(root) {
        if let Some((host, owner, name)) = parse_remote(&url) {
            if host != SUPPORTED_HOST {
                return Err(PullsState::UnsupportedHost { host });
            }
            return Ok((RepoId { host, owner, name }, branch));
        }
    }
    // Anything the fast path did not recognise is git's to answer, and
    // git's own words are what the refusal carries.
    let id = repo_id_via_git(git, root).await?;
    Ok((id, branch_of(git, root).await))
}

/// Name a checkout's repository and branch **by reading git's own
/// files**, with no subprocess at all.
///
/// **This is the difference between a surface that feels instant and one
/// that does not.** Measured on the operator's machine 2026-09-14:
/// naming ten checkouts cost 34 seconds from the shipped bundle and 4.5
/// seconds from a plain test process running the same code. The gap is
/// not the network and not git — it is *spawning* git. macOS validates a
/// child's code signature against its parent, and from an ad-hoc-signed
/// app that validation is slow enough to dominate; ten checkouts meant
/// thirty `git` launches.
///
/// So the common case launches none. `.git/config` and `.git/HEAD` are
/// plain text in a documented, stable format, and reading the two of
/// them answers both questions this module asks of a checkout. Anything
/// unexpected — a layout this does not recognise, a missing file, a
/// remote it cannot parse — falls through to [`repo_id_via_git`], which
/// is slower and carries git's own diagnosis. **Nothing is decided
/// here that git would decide differently; what changes is only how
/// often git is asked.**
fn read_checkout(root: &Path) -> Option<(String, Option<String>)> {
    let dot = root.join(".git");
    // A worktree's `.git` is a FILE naming the real directory — which is
    // how every lane in this project works, so it is not an edge case.
    let git_dir = if dot.is_dir() {
        dot
    } else {
        let raw = std::fs::read_to_string(&dot).ok()?;
        let named = Path::new(raw.trim().strip_prefix("gitdir:")?.trim());
        if named.is_absolute() {
            named.to_path_buf()
        } else {
            root.join(named)
        }
    };
    // **The config lives in the COMMON directory.** A worktree's own git
    // directory holds its HEAD and no config; `commondir` points at the
    // one that does.
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => {
            let rel = Path::new(rel.trim());
            if rel.is_absolute() {
                rel.to_path_buf()
            } else {
                git_dir.join(rel)
            }
        }
        Err(_) => git_dir.clone(),
    };
    let config = std::fs::read_to_string(common.join("config")).ok()?;
    let url = config_remote_url(&config)?;
    // HEAD is this checkout's own, never the common one — that is the
    // whole point of a worktree.
    let branch = std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|h| {
            h.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        })
        .filter(|b| !b.is_empty());
    Some((url, branch))
}

/// `origin`'s url out of a git config, or the first remote's.
///
/// The same precedence [`repo_id_via_git`] uses, for the same reason: a
/// fork's `upstream` is the repository its reviews live on as often as
/// not, so taking the first listed is a guess and is only reached when
/// there is no better answer.
fn config_remote_url(config: &str) -> Option<String> {
    let mut current: Option<String> = None;
    let mut first: Option<String> = None;
    let mut origin: Option<String> = None;
    for line in config.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            let head = rest.split(']').next().unwrap_or("");
            current = head
                .strip_prefix("remote")
                .map(|n| n.trim().trim_matches('"').to_string());
            continue;
        }
        let Some(name) = current.as_deref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "url" {
            continue;
        }
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        if name == "origin" && origin.is_none() {
            origin = Some(value.clone());
        }
        if first.is_none() {
            first = Some(value);
        }
    }
    origin.or(first)
}

/// The repository `cwd` belongs to, from its own remote.
async fn repo_id_via_git(git: &Path, cwd: &Path) -> Result<RepoId, PullsState> {
    // **Can this process see the folder at all?** Asked before git is,
    // because a `rev-parse` that fails for want of permission says
    // nothing whatever about what is in the directory — and answering
    // `NotARepo` there sends an operator hunting a problem in their
    // repository that is not in it. Measured on this machine
    // 2026-09-14: from an ad-hoc-signed bundle every checkout under
    // `~/Desktop` failed, and the same paths answered correctly from a
    // unit test run in a terminal.
    if let Err(e) = std::fs::read_dir(cwd) {
        return Err(PullsState::Unreadable {
            path: cwd.display().to_string(),
            why: e.to_string(),
        });
    }
    let inside = run_git(git, cwd, &["rev-parse", "--is-inside-work-tree"], GIT_TIMEOUT).await;
    match inside {
        Ok(out) if String::from_utf8_lossy(&out).trim() == "true" => {}
        // git ran and said no.
        Ok(_) => return Err(PullsState::NotARepo),
        // git could not answer. Its own sentence, because the difference
        // between "not a repository" and "dubious ownership" and "git is
        // not installed" is the whole of what the operator needs.
        Err(why) if !why.is_empty() => {
            return Err(PullsState::Unreadable {
                path: cwd.display().to_string(),
                why,
            })
        }
        Err(_) => return Err(PullsState::NotARepo),
    }
    // `origin` first, then whatever remote exists. A checkout with two
    // remotes and no `origin` is unusual and a fork's `upstream` is the
    // repository its reviews live on as often as not — taking the first
    // listed is a guess, so it is only reached when there is no better
    // answer.
    let url = match run_git(git, cwd, &["remote", "get-url", "origin"], GIT_TIMEOUT).await {
        Ok(out) => String::from_utf8_lossy(&out).trim().to_string(),
        Err(_) => {
            let names = run_git(git, cwd, &["remote"], GIT_TIMEOUT)
                .await
                .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                .unwrap_or_default();
            let first = names.lines().next().unwrap_or_default().trim().to_string();
            if first.is_empty() {
                return Err(PullsState::NoRemote);
            }
            run_git(git, cwd, &["remote", "get-url", &first], GIT_TIMEOUT)
                .await
                .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                .map_err(|_| PullsState::NoRemote)?
        }
    };

    let (host, owner, name) = parse_remote(&url).ok_or(PullsState::NoRemote)?;
    if host != SUPPORTED_HOST {
        return Err(PullsState::UnsupportedHost { host });
    }
    Ok(RepoId { host, owner, name })
}

/// The branch `cwd` is on, or `None` on a detached head.
///
/// This is what lets a row say *this is the review for the session you
/// are in*. It is read here rather than added to `git::scan`'s status
/// call because that call runs on a watch and this one runs when an
/// operator asks — putting a field on the hot snapshot to serve a cold
/// reader is how a scan grows a cost nobody asked for.
async fn branch_of(git: &Path, cwd: &Path) -> Option<String> {
    let out = run_git(git, cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"], GIT_TIMEOUT)
        .await
        .ok()?;
    let branch = String::from_utf8_lossy(&out).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// Whether `gh` holds no credential for this host.
async fn signed_out(gh: &Path, cwd: &Path, host: &str) -> bool {
    run_gh(
        gh,
        cwd,
        &["auth", "status", "--hostname", host],
        None,
        GH_TIMEOUT,
    )
    .await
    .is_err()
}

/// `(host, owner, name)` from a git remote URL.
///
/// Handles the four forms a GitHub remote actually takes: scp-style
/// `git@host:owner/name.git`, `ssh://git@host/owner/name.git`,
/// `https://host/owner/name.git`, and either with a `user@` or a port.
pub fn parse_remote(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    let rest = if let Some(i) = url.find("://") {
        &url[i + 3..]
    } else if let Some((left, right)) = url.split_once(':') {
        // scp-style `git@github.com:owner/name.git`. A Windows drive
        // letter or a `file:` path has no second slash-separated part
        // and falls out below.
        let host = left.rsplit('@').next()?;
        return finish(host, right);
    } else {
        url
    };

    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit('@').next()?;
    // A port is not part of the host GitHub is addressed by.
    let host = host.split(':').next()?;
    finish(host, path)
}

fn finish(host: &str, path: &str) -> Option<(String, String, String)> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    // `owner/name/extra` is not a repository path; refusing is better
    // than silently naming `owner/name` out of something else.
    if parts.next().is_some() || host.is_empty() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner, name))
}

// ---- the wire shape GitHub answers with -------------------------------

#[derive(Deserialize)]
struct GqlResponse {
    /// Keyed by the aliases [`batch_query`] built — `p0`, `p1`, … — and
    /// a repository the token cannot see comes back as an explicit
    /// `null` beside an error naming it.
    #[serde(default)]
    data: Option<std::collections::HashMap<String, Option<GqlRepo>>>,
    #[serde(default)]
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(default)]
    message: String,
    /// Which alias the error is about. GitHub names the field that
    /// failed, which is what lets one repository be refused on its own
    /// row while the rest of the document answers.
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

impl GqlError {
    /// The alias this error names, if it names one.
    fn alias(&self) -> Option<&str> {
        self.path.first().and_then(|v| v.as_str())
    }
}

#[derive(Deserialize)]
struct GqlRepo {
    #[serde(rename = "pullRequests")]
    pull_requests: GqlNodes,
}

#[derive(Deserialize)]
struct GqlNodes {
    #[serde(default)]
    nodes: Vec<GqlPull>,
}

#[derive(Deserialize)]
struct GqlPull {
    number: u64,
    title: String,
    state: String,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
    url: String,
    #[serde(rename = "updatedAt", default)]
    updated_at: String,
    #[serde(rename = "headRefName", default)]
    head_ref_name: String,
    #[serde(rename = "baseRefName", default)]
    base_ref_name: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(rename = "changedFiles", default)]
    changed_files: u64,
    #[serde(default)]
    author: Option<GqlActor>,
    #[serde(rename = "stackEntry", default)]
    stack_entry: Option<GqlStackEntry>,
    #[serde(default)]
    stack: Option<GqlStack>,
    #[serde(default)]
    commits: Option<GqlNodesCommits>,
}

#[derive(Deserialize)]
struct GqlActor {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct GqlStackEntry {
    #[serde(default)]
    position: u32,
}

#[derive(Deserialize)]
struct GqlStack {
    #[serde(default)]
    number: u64,
    #[serde(default)]
    size: u32,
}

#[derive(Deserialize)]
struct GqlNodesCommits {
    #[serde(default)]
    nodes: Vec<GqlCommitNode>,
}

#[derive(Deserialize)]
struct GqlCommitNode {
    #[serde(default)]
    commit: Option<GqlCommit>,
}

#[derive(Deserialize)]
struct GqlCommit {
    #[serde(rename = "statusCheckRollup", default)]
    rollup: Option<GqlRollup>,
}

#[derive(Deserialize)]
struct GqlRollup {
    #[serde(default)]
    state: String,
}

/// Turn one batched GraphQL response into rows, per repository.
///
/// **A partial answer is answered partially, and never truncated
/// silently.** GraphQL returns 200 with `data` half-filled beside an
/// `errors` array; the batch is what makes that ordinary rather than
/// exceptional, because one repository the token cannot see should not
/// take the other seven down. So an error that *names* an alias becomes
/// that repository's own refusal, an error that names none applies to
/// every repository in the document even where rows came back beside it,
/// and a repository with data and no error gets its rows. What is never
/// done is dropping a repository out of the list — the truncated-sum lie `FilesSnapshot::diff_stat`
/// refuses, one surface over.
type Answers = std::collections::HashMap<usize, Result<(Vec<PullRequest>, Vec<PullRequest>), String>>;

pub fn decode_batch(raw: &[u8], n: usize) -> Result<Answers, String> {
    let parsed: GqlResponse =
        serde_json::from_slice(raw).map_err(|e| format!("unreadable GitHub response: {e}"))?;

    let errors = parsed.errors.unwrap_or_default();
    let general = errors
        .iter()
        .filter(|e| e.alias().is_none())
        .map(|e| e.message.trim())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join("; ");

    let Some(mut data) = parsed.data else {
        // No `data` key at all: the document itself was refused, and
        // that is about every repository in it.
        return Err(if general.is_empty() {
            "GitHub reported an error with no message".to_string()
        } else {
            general
        });
    };

    let mut out: Answers = std::collections::HashMap::new();
    for i in 0..n {
        let open_alias = format!("p{i}");
        let recent_alias = format!("q{i}");
        let named = errors
            .iter()
            .filter(|e| {
                e.alias() == Some(open_alias.as_str()) || e.alias() == Some(recent_alias.as_str())
            })
            .map(|e| e.message.trim())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        let entry = if !named.is_empty() {
            Err(named)
        } else if !general.is_empty() {
            // **An error that names no field is about the whole
            // document, data or no data.** GitHub answers 200 with real
            // rows beside `Something went wrong while fetching stack`,
            // and those rows are a listing that silently lies about
            // being complete — a stack that failed to load draws as a
            // pile of unrelated reviews.
            Err(general.clone())
        } else {
            match data.remove(&open_alias).flatten() {
                Some(repo) => Ok((
                    repo.pull_requests.nodes.into_iter().map(row).collect(),
                    // **A missing history is not a failed read.** The
                    // open list is the answer to the question the
                    // operator asked; a repository whose settled reviews
                    // did not come back still has its open ones, and
                    // refusing the row over the second half would trade
                    // the useful answer for the decorative one.
                    data.remove(&recent_alias)
                        .flatten()
                        .map(|r| r.pull_requests.nodes.into_iter().map(row).collect())
                        .unwrap_or_default(),
                )),
                None => Err("GitHub returned no repository".to_string()),
            }
        };
        out.insert(i, entry);
    }
    Ok(out)
}

fn row(p: GqlPull) -> PullRequest {
    let checks = p
        .commits
        .and_then(|c| c.nodes.into_iter().next())
        .and_then(|n| n.commit)
        .and_then(|c| c.rollup)
        .map(|r| checks_state(&r.state))
        .unwrap_or(ChecksState::None);
    let stack = match (p.stack, p.stack_entry) {
        (Some(s), Some(e)) => Some(StackRef {
            number: s.number,
            size: s.size,
            position: e.position,
        }),
        // A stack without a position (or the reverse) cannot say where
        // this review sits in it, and a layer drawn at the wrong depth
        // is worse than a row drawn flat.
        _ => None,
    };
    PullRequest {
        number: p.number,
        title: p.title,
        state: pull_state(&p.state, p.is_draft),
        head: p.head_ref_name,
        base: p.base_ref_name,
        author: p.author.map(|a| a.login).filter(|l| !l.is_empty()),
        additions: p.additions,
        deletions: p.deletions,
        changed_files: p.changed_files,
        updated_ms: rfc3339_epoch_ms(&p.updated_at).unwrap_or(0),
        url: p.url,
        checks,
        stack,
    }
}

fn pull_state(state: &str, is_draft: bool) -> PullState {
    match state {
        "MERGED" => PullState::Merged,
        "CLOSED" => PullState::Closed,
        // Draft is only ever a fact about an open review.
        _ if is_draft => PullState::Draft,
        _ => PullState::Open,
    }
}

fn checks_state(rollup: &str) -> ChecksState {
    match rollup {
        "SUCCESS" => ChecksState::Passing,
        "FAILURE" | "ERROR" => ChecksState::Failing,
        // `EXPECTED` is a check GitHub has been told to wait for and
        // has not seen — pending, not absent.
        "PENDING" | "EXPECTED" => ChecksState::Pending,
        _ => ChecksState::None,
    }
}

// ---- processes --------------------------------------------------------

/// **Keeps git's own words.** The first cut mapped every failure to
/// `()`, which is what let a permissions refusal be reported as "not a
/// git checkout" — the error was in hand and thrown away one line before
/// the wrong conclusion was drawn from its absence.
async fn run_git(git: &Path, cwd: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    run(git, cwd, args, None, timeout, &[]).await
}

async fn run_gh(
    gh: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    run(
        gh,
        cwd,
        args,
        stdin,
        timeout,
        &[
            // Never stop for a human: this runs behind a UI with no
            // terminal attached, and a prompt would read as a hang.
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("NO_COLOR", "1"),
            ("CLICOLOR", "0"),
        ],
    )
    .await
}

async fn run(
    bin: &Path,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    timeout: Duration,
    env: &[(&str, &str)],
) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(bin);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return Err(format!(
                "{} failed to start: {e}",
                bin.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            ))
        }
    };
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A write failure here is the child having already exited, which
        // the wait below reports with its own stderr — a better message
        // than "broken pipe".
        let _ = pipe.write_all(bytes).await;
        drop(pipe);
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("read failed: {e}")),
        Err(_) => return Err(format!("timed out after {}s", timeout.as_secs())),
    };
    if out.status.success() {
        return Ok(out.stdout);
    }
    // **The CLI's own words.** eave renders a provider's refusal verbatim
    // everywhere else (TN2's Codex and Pi refusals); a GitHub outage, a
    // rate limit and a repository the signed-in account cannot see are
    // three different sentences and `gh` writes all three.
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("exited with status {}", out.status)
    } else {
        stderr
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_of_github_remote_names_one_repository() {
        let want = Some((
            "github.com".to_string(),
            "mikestangdevs".to_string(),
            "eave".to_string(),
        ));
        for url in [
            "git@github.com:mikestangdevs/eave.git",
            "git@github.com:mikestangdevs/eave",
            "ssh://git@github.com/mikestangdevs/eave.git",
            "https://github.com/mikestangdevs/eave.git",
            "https://github.com/mikestangdevs/eave",
            "https://someone@github.com/mikestangdevs/eave.git",
            "https://github.com/mikestangdevs/eave/",
            "git://github.com/mikestangdevs/eave.git",
            // Case belongs to the host, not to the path: GitHub owners
            // and repository names are case-preserving.
            "https://GitHub.com/mikestangdevs/eave.git",
        ] {
            assert_eq!(parse_remote(url), want, "{url}");
        }
    }

    #[test]
    fn a_remote_that_names_no_repository_is_refused() {
        // Each of these would produce a plausible wrong answer under a
        // looser parser, which is the failure that matters: a wrong
        // owner reads as "you have no reviews".
        for url in [
            "",
            "   ",
            "https://github.com/mikestangdevs",
            "https://github.com/",
            "/srv/git/local.git",
            "https://github.com/a/b/c",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn a_self_hosted_remote_keeps_its_own_host() {
        assert_eq!(
            parse_remote("git@ghe.example.com:team/thing.git"),
            Some((
                "ghe.example.com".to_string(),
                "team".to_string(),
                "thing".to_string()
            ))
        );
    }

    /// The captured shape of the real thing: one row of the reference
    /// screenshot's own stack, as `gh api graphql` answered it.
    const CAPTURE: &str = r#"{"data":{"p0":{"pullRequests":{"nodes":[
      {"number":11702,"title":"feat(cli): add t3 service restart",
       "state":"OPEN","isDraft":false,
       "url":"https://github.com/pingdotgg/t3code/pull/11702",
       "updatedAt":"2026-09-14T01:46:40Z",
       "headRefName":"cli/service-restart","baseRefName":"main",
       "additions":430,"deletions":61,"changedFiles":9,
       "author":{"login":"juliusmarminge"},
       "stackEntry":{"position":1},"stack":{"number":11703,"size":2},
       "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
      {"number":11696,"title":"docs: make the standalone installer primary",
       "state":"OPEN","isDraft":true,
       "url":"https://github.com/pingdotgg/t3code/pull/11696",
       "updatedAt":"2026-09-14T01:40:00Z",
       "headRefName":"docs/standalone-install","baseRefName":"cli/service-restart",
       "additions":118,"deletions":126,"changedFiles":4,
       "author":null,"stackEntry":{"position":2},"stack":{"number":11703,"size":2},
       "commits":{"nodes":[]}}
    ]}}}}"#;

    #[test]
    fn a_capture_decodes_into_rows() {
        let mut answers = decode_batch(CAPTURE.as_bytes(), 1).expect("decodes");
        let (rows, _recent) = answers.remove(&0).expect("slot 0").expect("rows");
        assert_eq!(rows.len(), 2);

        let top = &rows[0];
        assert_eq!(top.number, 11702);
        assert_eq!(top.state, PullState::Open);
        assert_eq!(top.head, "cli/service-restart");
        assert_eq!(top.base, "main");
        assert_eq!((top.additions, top.deletions, top.changed_files), (430, 61, 9));
        assert_eq!(top.author.as_deref(), Some("juliusmarminge"));
        assert_eq!(top.checks, ChecksState::Passing);
        assert_eq!(top.updated_ms, 1_789_350_400_000); // 2026-09-14T01:46:40Z
        assert_eq!(
            top.stack,
            Some(StackRef {
                number: 11703,
                size: 2,
                position: 1
            })
        );

        let above = &rows[1];
        // `isDraft` folds into the one word a row draws.
        assert_eq!(above.state, PullState::Draft);
        assert!(above.state.is_open(), "a draft is unfinished, not finished");
        // A deleted account leaves no login, and that is not an empty
        // string pretending to be a name.
        assert_eq!(above.author, None);
        // No commit node at all is "nothing has reported", never a tick.
        assert_eq!(above.checks, ChecksState::None);
        // The stack's own number is not either member's number.
        assert_eq!(above.stack.as_ref().map(|s| s.number), Some(11703));
    }

    #[test]
    fn a_graphql_error_is_not_an_empty_list() {
        // 200 OK with a populated `errors` array is how GitHub reports a
        // repository the signed-in account cannot see. Reading the empty
        // `nodes` beside it would draw "no open reviews" over a
        // permissions problem.
        let raw = br#"{"data":{"p0":null},
            "errors":[{"message":"Could not resolve to a Repository with the name 'x/y'."}]}"#;
        let mut answers = decode_batch(raw, 1).expect("the document itself parsed");
        let err = answers.remove(&0).expect("slot 0").expect_err("must not be rows");
        assert!(err.contains("Could not resolve"), "{err}");
    }

    /// **The reason the read is batched at all.** The operator has one
    /// repository on an account `gh` is not signed in as, and seven it
    /// can see; refusing the whole overview because of the one would
    /// make the feature useless on this machine.
    #[test]
    fn one_repository_can_be_refused_without_taking_the_others_down() {
        let raw = br#"{"data":{
            "p0":{"pullRequests":{"nodes":[
              {"number":1,"title":"t","state":"OPEN","isDraft":false,"url":"u",
               "updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"main",
               "additions":1,"deletions":0,"changedFiles":1}]}},
            "p1":null},
            "errors":[{"message":"Could not resolve to a Repository with the name 'x/y'.",
                       "path":["p1"]}]}"#;
        let mut answers = decode_batch(raw, 2).expect("the document itself parsed");
        let (ok, _) = answers.remove(&0).expect("slot 0").expect("rows");
        assert_eq!(ok.len(), 1, "the readable repository lost its rows");
        let err = answers.remove(&1).expect("slot 1").expect_err("must not be rows");
        assert!(err.contains("Could not resolve"), "{err}");
    }

    #[test]
    fn a_partial_answer_is_refused_rather_than_truncated() {
        // `data` carries real rows AND `errors` names what was dropped.
        // Returning the rows would be a listing that silently lies about
        // being complete.
        let raw = br#"{"data":{"p0":{"pullRequests":{"nodes":[
            {"number":1,"title":"t","state":"OPEN","isDraft":false,"url":"u",
             "updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"b",
             "additions":1,"deletions":0,"changedFiles":1,"author":{"login":"a"}}]}}},
            "errors":[{"message":"Something went wrong while fetching stack"}]}"#;
        let mut answers = decode_batch(raw, 1).expect("the document itself parsed");
        assert!(
            answers.remove(&0).expect("slot 0").is_err(),
            "rows beside an unnamed error are a listing that lies about being complete"
        );
    }

    #[test]
    fn missing_optional_fields_do_not_fail_the_row() {
        // Every optional the query asks for, absent at once — the shape
        // an older GitHub Enterprise without stacks would answer with.
        let raw = br#"{"data":{"p0":{"pullRequests":{"nodes":[
            {"number":7,"title":"bare","state":"OPEN","isDraft":false,"url":"u",
             "updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"main",
             "additions":0,"deletions":0,"changedFiles":0}]}}}}"#;
        let mut answers = decode_batch(raw, 1).expect("decodes");
        let (rows, _) = answers.remove(&0).expect("slot 0").expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stack, None);
        assert_eq!(rows[0].checks, ChecksState::None);
        assert_eq!(rows[0].author, None);
    }

    /// **The common case launches no `git` at all**, and this is the
    /// reader that makes that true. Both layouts this project actually
    /// has: an ordinary checkout, and a worktree whose `.git` is a file
    /// and whose config lives in the main checkout's directory.
    #[test]
    fn a_checkout_names_itself_without_running_git() {
        let tmp = std::env::temp_dir().join(format!("eave-gh-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        // An ordinary checkout.
        let main = tmp.join("main");
        std::fs::create_dir_all(main.join(".git")).expect("mkdir");
        std::fs::write(
            main.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = https://github.com/o/r.git\n\tfetch = +refs/heads/*\n",
        )
        .expect("write");
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/trunk\n").expect("write");

        let (url, branch) = read_checkout(&main).expect("the checkout named nothing");
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(branch.as_deref(), Some("trunk"));

        // A worktree: `.git` is a FILE, its own HEAD, and the config one
        // directory up through `commondir`.
        let wt_git = main.join(".git/worktrees/lane");
        std::fs::create_dir_all(&wt_git).expect("mkdir");
        std::fs::write(wt_git.join("commondir"), "../..\n").expect("write");
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/slice-x\n").expect("write");
        let wt = tmp.join("lane");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_git.display()),
        )
        .expect("write");

        let (url, branch) = read_checkout(&wt).expect("the worktree named nothing");
        assert_eq!(url, "https://github.com/o/r.git", "a worktree's config is the main one's");
        assert_eq!(
            branch.as_deref(),
            Some("slice-x"),
            "a worktree's HEAD is its OWN, which is the whole point of one"
        );

        // A detached head is no branch, never an empty one — an empty
        // branch would match a review whose head failed to decode.
        std::fs::write(wt_git.join("HEAD"), "0123456789abcdef0123456789abcdef01234567\n")
            .expect("write");
        assert_eq!(read_checkout(&wt).expect("still a checkout").1, None);

        // Nothing it does not recognise is guessed at: an unparseable
        // layout answers None and the caller asks git.
        std::fs::write(main.join(".git/config"), "[core]\n\tbare = false\n").expect("write");
        assert!(
            read_checkout(&main).is_none(),
            "a config with no remote was answered anyway"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `origin` wins; failing that the first remote listed, which is the
    /// same precedence the `git` path uses and for the same reason.
    #[test]
    fn origin_wins_and_a_lone_remote_is_taken() {
        let both = "[remote \"upstream\"]\n\turl = git@github.com:up/stream.git\n\
                    [remote \"origin\"]\n\turl = https://github.com/o/r.git\n";
        assert_eq!(
            config_remote_url(both).as_deref(),
            Some("https://github.com/o/r.git")
        );
        let lone = "[remote \"fork\"]\n\turl = git@github.com:f/k.git\n";
        assert_eq!(
            config_remote_url(lone).as_deref(),
            Some("git@github.com:f/k.git")
        );
        // A `url` outside any remote section is not a remote.
        assert_eq!(config_remote_url("[core]\n\turl = nope\n"), None);
        assert_eq!(config_remote_url(""), None);
    }

    /// Two fake binaries, so the recovery path can be driven with no
    /// network and no GitHub: a `git` that names a repository for any
    /// directory, and a `gh` that refuses any document naming more than
    /// one repository.
    ///
    /// **This is the walk's defect.** One transient `HTTP 502` — not
    /// reproducible in three runs afterwards — emptied an overview of
    /// seven repositories at once, every row wearing the same refusal.
    /// Batching is an optimisation and its failure mode must not be
    /// worse than not batching.
    #[test]
    fn a_batch_that_fails_splits_instead_of_taking_its_repositories_with_it() {
        let _held = gh_bin_gate();

        let tmp = std::env::temp_dir().join(format!("eave-gh-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let roots: Vec<String> = (0..3)
            .map(|i| {
                let d = tmp.join(format!("r{i}"));
                std::fs::create_dir_all(d.join(".git")).expect("mkdir");
                d.display().to_string()
            })
            .collect();

        let git = write_script(
            &tmp,
            "git",
            r#"
case "$*" in
  *"--is-inside-work-tree"*) echo true ;;
  *"remote get-url origin"*) echo "https://github.com/o/$(basename "$PWD").git" ;;
  *"symbolic-ref"*) echo main ;;
  *) exit 1 ;;
esac
"#,
        );
        let gh = write_script(
            &tmp,
            "gh",
            r#"
case "$*" in
  *"auth status"*) exit 0 ;;
esac
body=$(cat)
n=$(printf '%s' "$body" | grep -o 'repository(owner' | wc -l | tr -d ' ')
# Two fields per repository: more than two means more than one repo.
if [ "$n" -gt 2 ]; then
  echo "gh: HTTP 502" >&2
  exit 1
fi
printf '{"data":{"p0":{"pullRequests":{"nodes":[{"number":1,"title":"t","state":"OPEN","isDraft":false,"url":"u","updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"main","additions":1,"deletions":0,"changedFiles":1}]}},"q0":{"pullRequests":{"nodes":[]}}}}'
"#,
        );

        let prev = std::env::var(GH_BIN_OVERRIDE).ok();
        std::env::set_var(GH_BIN_OVERRIDE, &gh);
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let snap = rt.block_on(reviews_with(&git, &roots, 10));
        match prev {
            Some(v) => std::env::set_var(GH_BIN_OVERRIDE, v),
            None => std::env::remove_var(GH_BIN_OVERRIDE),
        }

        assert_eq!(snap.repos.len(), 3, "{:?}", snap.skipped);
        for r in &snap.repos {
            assert_eq!(
                r.state,
                PullsState::Ok,
                "{} kept the batch's failure after the split",
                r.repo.slug()
            );
            assert_eq!(r.rows.len(), 1, "{}", r.repo.slug());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A repository that fails **alone** is the one that failed, and it
    /// says so on its own row while the rest of the overview stands.
    #[test]
    fn a_repository_that_fails_alone_is_the_only_one_that_says_so() {
        let _held = gh_bin_gate();

        let tmp = std::env::temp_dir().join(format!("eave-gh-alone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let roots: Vec<String> = ["good", "bad"]
            .iter()
            .map(|n| {
                let d = tmp.join(n);
                std::fs::create_dir_all(d.join(".git")).expect("mkdir");
                d.display().to_string()
            })
            .collect();

        let git = write_script(
            &tmp,
            "git",
            r#"
case "$*" in
  *"--is-inside-work-tree"*) echo true ;;
  *"remote get-url origin"*) echo "https://github.com/o/$(basename "$PWD").git" ;;
  *"symbolic-ref"*) echo main ;;
  *) exit 1 ;;
esac
"#,
        );
        // Refuses any document mentioning `bad`, whatever its size — so
        // the split runs all the way down and one row is left holding
        // the failure.
        let gh = write_script(
            &tmp,
            "gh",
            r#"
case "$*" in
  *"auth status"*) exit 0 ;;
esac
body=$(cat)
if printf '%s' "$body" | grep -q '"bad"'; then
  echo "gh: HTTP 502" >&2
  exit 1
fi
printf '{"data":{"p0":{"pullRequests":{"nodes":[]}},"q0":{"pullRequests":{"nodes":[]}}}}'
"#,
        );

        let prev = std::env::var(GH_BIN_OVERRIDE).ok();
        std::env::set_var(GH_BIN_OVERRIDE, &gh);
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let snap = rt.block_on(reviews_with(&git, &roots, 10));
        match prev {
            Some(v) => std::env::set_var(GH_BIN_OVERRIDE, v),
            None => std::env::remove_var(GH_BIN_OVERRIDE),
        }

        let good = snap
            .repos
            .iter()
            .find(|r| r.repo.name == "good")
            .expect("the readable repository vanished");
        assert_eq!(good.state, PullsState::Ok, "one bad repository took a good one down");
        let bad = snap
            .repos
            .iter()
            .find(|r| r.repo.name == "bad")
            .expect("the failing repository vanished");
        assert!(
            matches!(&bad.state, PullsState::Error { message } if message.contains("502")),
            "{:?}",
            bad.state
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// **`EAVE_GH_BIN` is process-global and this suite runs its tests on
    /// THREADS of one process**, so the tests that set it take turns.
    ///
    /// Written after a per-test `static GATE` inside each test body gave
    /// each of them its own mutex and no mutual exclusion at all — which
    /// failed exactly once and passed on the re-run, the shape of a
    /// flake nobody would have chased.
    fn gh_bin_gate() -> std::sync::MutexGuard<'static, ()> {
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        GATE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A tiny executable shell script, for the two tests above.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// A real read against a real checkout — `#[ignore]`d because it
    /// needs `gh`, a network and a GitHub checkout, none of which a
    /// suite may assume. Point it somewhere with
    /// `EAVE_GH_LIVE_REPO=/path/to/checkout,/path/to/folder cargo test -p
    /// eave-engine --lib github::tests::a_live_read -- --ignored
    /// --nocapture`. Commas separate roots; a root may be a checkout or
    /// a folder holding checkouts.
    ///
    /// It exists because every other test here decodes a capture, and a
    /// capture cannot tell you that the query still parses, that the
    /// `stack` field is still in the schema, or that `gh` still takes a
    /// body on stdin.
    #[test]
    #[ignore]
    fn a_live_read_answers_from_a_real_checkout() {
        let roots: Vec<String> = std::env::var("EAVE_GH_LIVE_REPO")
            .unwrap_or_else(|_| ".".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let started = std::time::Instant::now();
        let snap = rt.block_on(reviews(&roots, 10));
        println!("took   : {:?}", started.elapsed());
        println!("roots  : {:?}  truncated={}", snap.roots, snap.truncated);
        println!("skipped: {} folder(s) held no repository", snap.skipped.len());
        for bad in snap.skipped.iter().take(3) {
            println!("   {} — {:?}", bad.name(), bad.state);
        }
        for repo in &snap.repos {
            println!(
                "repo   : {:<32} {:?}  {} checkout(s) on {:?}",
                repo.repo.slug(),
                repo.state,
                repo.roots.len(),
                repo.branches
            );
            for r in repo.rows.iter().chain(repo.recent.iter()) {
                let stack = r
                    .stack
                    .as_ref()
                    .map(|s| format!("stack#{} {}/{}", s.number, s.position, s.size))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "  #{:<6} {:<7} {:<18} {:?} +{}/-{}  {} -> {}  {}",
                    r.number,
                    r.state.word(),
                    stack,
                    r.checks,
                    r.additions,
                    r.deletions,
                    r.head,
                    r.base,
                    r.title.chars().take(40).collect::<String>()
                );
            }
        }
        assert!(
            snap.repos.iter().any(|r| matches!(r.state, PullsState::Ok)),
            "live read did not succeed for any repository"
        );
    }

    /// The document is built from an index and nothing else — the one
    /// place a batched query could grow an interpolation.
    #[test]
    fn a_batched_document_names_one_field_per_repository() {
        let q = batch_query(3);
        for i in 0..3 {
            for alias in ["p", "q"] {
                assert!(
                    q.contains(&format!("{alias}{i}: repository(owner: $o{i}, name: $r{i})")),
                    "slot {alias}{i} missing from:\n{q}"
                );
            }
        }
        assert!(!q.contains("p3:"), "a fourth slot appeared");
        assert!(q.contains("fragment Row on PullRequest"), "no row fragment");
        assert!(q.contains("fragment Open on Repository"), "no open fragment");
        assert!(q.contains("fragment Recent on Repository"), "no history fragment");
        // Every owner and name rides in variables. A document holding a
        // literal quote around a name is a document that interpolated.
        assert!(
            q.matches('"').count() == 0,
            "the document quotes something: {q}"
        );
    }

    /// A folder of folders is what an operator points this at.
    #[test]
    fn a_scan_finds_checkouts_under_a_folder_and_does_not_enter_them() {
        let tmp = std::env::temp_dir().join(format!("eave-gh-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // A checkout, a checkout one level deeper, a worktree whose
        // `.git` is a FILE, a vendored checkout inside a checkout, and a
        // folder that is neither.
        for d in [
            "top/.git",
            "nest/deep/.git",
            "nest/deep/vendor/dep/.git",
            "plain/src",
            "node_modules/pkg/.git",
        ] {
            std::fs::create_dir_all(tmp.join(d)).expect("mkdir");
        }
        std::fs::create_dir_all(tmp.join("tree")).expect("mkdir");
        std::fs::write(tmp.join("tree/.git"), "gitdir: /elsewhere").expect("write");

        let scan = expand_roots(&[tmp.display().to_string()]);
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
        assert!(!scan.truncated);
        let names: Vec<String> = scan
            .checkouts
            .iter()
            .map(|p| p.trim_start_matches(&tmp.display().to_string()).to_string())
            .collect();
        assert!(names.contains(&"/top".to_string()), "{names:?}");
        assert!(names.contains(&"/nest/deep".to_string()), "{names:?}");
        // A `.git` FILE is a worktree, which is how every lane in this
        // project works — it is a checkout.
        assert!(names.contains(&"/tree".to_string()), "{names:?}");
        // A checkout is not scanned for checkouts, and `node_modules` is
        // never scanned at all.
        assert!(
            !names.iter().any(|n| n.contains("vendor")),
            "a scan descended into a checkout: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "{names:?}"
        );
        // A root that IS a checkout is itself, not its children.
        let one = expand_roots(&[tmp.join("top").display().to_string()]);
        assert_eq!(one.checkouts.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A folder this process may not read is said so, by name.
    #[test]
    fn a_folder_that_cannot_be_read_is_named_rather_than_counted_as_empty() {
        let scan = expand_roots(&["/definitely/not/here/at/all".to_string()]);
        assert!(scan.checkouts.is_empty());
        assert_eq!(
            scan.skipped.len(),
            1,
            "the folder vanished instead of saying why"
        );
        assert_eq!(scan.skipped[0].root, "/definitely/not/here/at/all");
        assert!(
            matches!(scan.skipped[0].state, PullsState::Unreadable { .. }),
            "{:?}",
            scan.skipped[0].state
        );
    }

    #[test]
    fn check_rollups_map_to_four_answers_and_no_others() {
        assert_eq!(checks_state("SUCCESS"), ChecksState::Passing);
        assert_eq!(checks_state("FAILURE"), ChecksState::Failing);
        assert_eq!(checks_state("ERROR"), ChecksState::Failing);
        assert_eq!(checks_state("PENDING"), ChecksState::Pending);
        assert_eq!(checks_state("EXPECTED"), ChecksState::Pending);
        // A word this decode has never seen must not become a tick.
        assert_eq!(checks_state("SOMETHING_NEW"), ChecksState::None);
    }
}
