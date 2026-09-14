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
    ChecksState, PullRequest, PullState, PullsSnapshot, PullsState, RepoId, StackRef,
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

/// One query, every field a list row draws.
///
/// `statusCheckRollup` is read off the **last** commit because that is
/// the head the checks belong to; asking the pull request for its checks
/// directly is not a thing the schema offers.
const PULLS_QUERY: &str = r#"query Pulls($owner: String!, $name: String!, $n: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $n, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title state isDraft url updatedAt
        headRefName baseRefName
        additions deletions changedFiles
        author { login }
        stackEntry { position }
        stack { number size }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}"#;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every open review on the repository `cwd` belongs to.
pub async fn pulls(cwd: &Path, limit: usize) -> PullsSnapshot {
    pulls_with(Path::new("git"), cwd, limit).await
}

/// [`pulls`] with the git binary injectable, so a test can hand it a
/// fake remote without a network or a real checkout.
pub async fn pulls_with(git: &Path, cwd: &Path, limit: usize) -> PullsSnapshot {
    let repo = match repo_id(git, cwd).await {
        Ok(r) => r,
        Err(state) => return PullsSnapshot::stated(state, now_ms()),
    };
    let branch = branch_of(git, cwd).await;

    let gh = match resolve_spec(&GH, GH_BIN_OVERRIDE) {
        ProgramResolution::Found(p) => p,
        ProgramResolution::NotInstalled { searched, .. } => {
            return PullsSnapshot::stated(
                PullsState::CliMissing {
                    searched: searched.iter().map(|p| p.display().to_string()).collect(),
                },
                now_ms(),
            )
        }
    };

    let body = serde_json::json!({
        "query": PULLS_QUERY,
        "variables": { "owner": repo.owner, "name": repo.name, "n": limit as i64 },
    })
    .to_string();

    let raw = match run_gh(
        &gh,
        cwd,
        &["api", "graphql", "--hostname", &repo.host, "--input", "-"],
        Some(body.as_bytes()),
        GH_TIMEOUT,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(message) => {
            // **Diagnose only on failure.** A `gh auth status` probe
            // before every read would spend a process on a question
            // whose answer is almost always yes, and would still race
            // the read it is meant to explain. Asking after the fact
            // costs nothing in the common case and is strictly more
            // accurate: it explains the failure that actually happened.
            let state = if signed_out(&gh, cwd, &repo.host).await {
                PullsState::SignedOut {
                    host: repo.host.clone(),
                }
            } else {
                PullsState::Error { message }
            };
            let mut snap = PullsSnapshot::stated(state, now_ms());
            snap.repo = Some(repo);
            snap.checkout_branch = branch;
            return snap;
        }
    };

    match decode_pulls(&raw) {
        Ok(rows) => PullsSnapshot {
            repo: Some(repo),
            rows,
            state: PullsState::Ok,
            read_ms: now_ms(),
            checkout_branch: branch,
        },
        Err(message) => {
            let mut snap = PullsSnapshot::stated(PullsState::Error { message }, now_ms());
            snap.repo = Some(repo);
            snap.checkout_branch = branch;
            snap
        }
    }
}

/// The repository `cwd` belongs to, from its own remote.
async fn repo_id(git: &Path, cwd: &Path) -> Result<RepoId, PullsState> {
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
    #[serde(default)]
    data: Option<GqlData>,
    #[serde(default)]
    errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct GqlData {
    #[serde(default)]
    repository: Option<GqlRepo>,
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

/// Turn one GraphQL response into rows.
pub fn decode_pulls(raw: &[u8]) -> Result<Vec<PullRequest>, String> {
    let parsed: GqlResponse =
        serde_json::from_slice(raw).map_err(|e| format!("unreadable GitHub response: {e}"))?;

    // **A partial answer is still an error here.** GraphQL returns 200
    // with `data` half-filled beside an `errors` array, and a listing
    // that silently drops the reviews it could not read is the
    // truncated-sum lie `FilesSnapshot::diff_stat` refuses.
    if let Some(errors) = parsed.errors.filter(|e| !e.is_empty()) {
        let joined = errors
            .iter()
            .map(|e| e.message.trim())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if joined.is_empty() {
            "GitHub reported an error with no message".to_string()
        } else {
            joined
        });
    }

    let repo = parsed
        .data
        .and_then(|d| d.repository)
        .ok_or_else(|| "GitHub returned no repository".to_string())?;

    Ok(repo.pull_requests.nodes.into_iter().map(row).collect())
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
    const CAPTURE: &str = r#"{"data":{"repository":{"pullRequests":{"nodes":[
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
        let rows = decode_pulls(CAPTURE.as_bytes()).expect("decodes");
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
        let raw = br#"{"data":{"repository":null},
            "errors":[{"message":"Could not resolve to a Repository with the name 'x/y'."}]}"#;
        let err = decode_pulls(raw).expect_err("must not decode to rows");
        assert!(err.contains("Could not resolve"), "{err}");
    }

    #[test]
    fn a_partial_answer_is_refused_rather_than_truncated() {
        // `data` carries real rows AND `errors` names what was dropped.
        // Returning the rows would be a listing that silently lies about
        // being complete.
        let raw = br#"{"data":{"repository":{"pullRequests":{"nodes":[
            {"number":1,"title":"t","state":"OPEN","isDraft":false,"url":"u",
             "updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"b",
             "additions":1,"deletions":0,"changedFiles":1,"author":{"login":"a"}}]}}},
            "errors":[{"message":"Something went wrong while fetching stack"}]}"#;
        assert!(decode_pulls(raw).is_err());
    }

    #[test]
    fn missing_optional_fields_do_not_fail_the_row() {
        // Every optional the query asks for, absent at once — the shape
        // an older GitHub Enterprise without stacks would answer with.
        let raw = br#"{"data":{"repository":{"pullRequests":{"nodes":[
            {"number":7,"title":"bare","state":"OPEN","isDraft":false,"url":"u",
             "updatedAt":"2026-01-01T00:00:00Z","headRefName":"h","baseRefName":"main",
             "additions":0,"deletions":0,"changedFiles":0}]}}}}"#;
        let rows = decode_pulls(raw).expect("decodes");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stack, None);
        assert_eq!(rows[0].checks, ChecksState::None);
        assert_eq!(rows[0].author, None);
    }

    /// A real read against a real checkout — `#[ignore]`d because it
    /// needs `gh`, a network and a GitHub checkout, none of which a
    /// suite may assume. Point it somewhere with
    /// `EAVE_GH_LIVE_REPO=/path/to/checkout cargo test -p eave-engine
    /// --lib github::tests::a_live_read -- --ignored --nocapture`.
    ///
    /// It exists because every other test here decodes a capture, and a
    /// capture cannot tell you that the query still parses, that the
    /// `stack` field is still in the schema, or that `gh` still takes a
    /// body on stdin.
    #[test]
    #[ignore]
    fn a_live_read_answers_from_a_real_checkout() {
        let dir = std::env::var("EAVE_GH_LIVE_REPO").unwrap_or_else(|_| ".".to_string());
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let snap = rt.block_on(pulls(Path::new(&dir), 10));
        println!("repo   : {:?}", snap.repo.as_ref().map(|r| r.slug()));
        println!("branch : {:?}", snap.checkout_branch);
        println!("state  : {:?}", snap.state);
        for r in &snap.rows {
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
        assert_eq!(snap.state, PullsState::Ok, "live read did not succeed");
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
