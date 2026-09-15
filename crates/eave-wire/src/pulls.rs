//! Pull requests — the review a branch is in (Slice GH1).
//!
//! A **new file on purpose.** These types could sit in `frames.rs` beside
//! [`SessionInfo`](crate::SessionInfo), and every lane in this project
//! edits `frames.rs`; a slice that can land as an added module instead of
//! an edited one costs the next rebase nothing.
//!
//! Everything here is what a LIST row needs and nothing more. The review
//! body, its comments, its files and its check names belong to a detail
//! surface that does not exist yet, and guessing their shape now would
//! mean carrying fields no reader has.

use serde::{Deserialize, Serialize};

/// Where a review stands, as a person reads it.
///
/// **GitHub answers this as two fields and every surface renders one
/// word.** `state` is `OPEN`/`CLOSED`/`MERGED` and `isDraft` is a
/// separate boolean, so each surface that draws a row would otherwise
/// decide the precedence for itself — and they would not all decide the
/// same way. Draft is folded in here because it can only ever be true of
/// an OPEN review (a merged draft does not exist), so the fold discards
/// nothing and leaves one word to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    Draft,
    Open,
    Merged,
    Closed,
}

impl PullState {
    /// Open work — what a list shows by default and what a stack action
    /// would act on. A draft is open: it is unfinished, not finished.
    pub fn is_open(self) -> bool {
        matches!(self, PullState::Draft | PullState::Open)
    }

    /// The one word a row draws.
    pub fn word(self) -> &'static str {
        match self {
            PullState::Draft => "draft",
            PullState::Open => "open",
            PullState::Merged => "merged",
            PullState::Closed => "closed",
        }
    }
}

/// The rollup of the head commit's checks.
///
/// **[`ChecksState::None`] is not passing.** A head nobody has run
/// anything against and a head that ran everything green are different
/// facts, and collapsing them is how a row draws a tick it did not earn —
/// the same rule [`FilesSnapshot`](crate::FilesSnapshot) follows when it
/// refuses to report a count it could not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Passing,
    Pending,
    Failing,
    /// No check has ever reported against this head.
    None,
}

/// A review's place in a GitHub stack.
///
/// **This is asked, never derived.** GitHub's GraphQL schema carries
/// `PullRequest.stack` and `PullRequest.stackEntry` natively — measured
/// against the live API on 2026-09-14, not read out of a source tree —
/// so a stack eave draws is the stack GitHub says exists. Chaining
/// reviews by matching one's base branch to another's head is a
/// *fallback* for repositories not using the feature, and it lives in
/// the app's grouping pass rather than here, because it is an inference
/// and this struct is a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRef {
    /// GitHub's own number for the stack — `#11703` in the reference
    /// capture, whose members are `#11696` and `#11702`. **Not any
    /// member's number**, and it collides with one often enough that a
    /// surface printing the wrong one looks right.
    pub number: u64,
    /// How many reviews are in the stack, including terminal ones.
    pub size: u32,
    /// 1-based and bottom-first, exactly as `stackEntry.position`
    /// answers: position 1 is the layer that merges into the stack's
    /// base, and the layer you would review first.
    pub position: u32,
}

/// One review, at the level of detail a list row draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: PullState,
    /// The branch the work is on — what a session is matched against.
    pub head: String,
    /// The branch it merges into. In a stack this is the layer below's
    /// `head`, which is what makes the fallback chaining possible.
    pub base: String,
    /// `None` when GitHub reports no author, which happens for a review
    /// opened by a since-deleted account.
    pub author: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// Epoch milliseconds. **Converted at the decode**, because every
    /// age eave draws is drawn by `coarse_age` from epoch ms and a
    /// timestamp that arrives as text makes each surface parse dates.
    pub updated_ms: u64,
    pub url: String,
    pub checks: ChecksState,
    pub stack: Option<StackRef>,
}

/// The repository a checkout belongs to, as the host names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoId {
    /// `github.com`, or an Enterprise host. Carried because `gh` needs
    /// `--hostname` and because an account signed in to one host says
    /// nothing about another.
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// `owner/name`, the form `gh` takes and a person reads.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Why a listing is not a listing.
///
/// Modelled on [`GitState`](crate::GitState): the absence of an answer is
/// itself an answer, and it names which of the several things that must
/// be true was not. A surface that only knows "no reviews" cannot tell an
/// operator to run `gh auth login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullsState {
    Ok,
    /// The GitHub CLI is not on this machine. `searched` is every
    /// directory looked in, so the report can be specific the way
    /// `ProgramResolution::NotInstalled` is.
    CliMissing { searched: Vec<String> },
    /// `gh` is here and holds no credential for this host.
    SignedOut { host: String },
    /// The directory is not in a git checkout at all.
    NotARepo,
    /// eave could not read the directory at all.
    ///
    /// **Split out from [`PullsState::NotARepo`] after the walk**, which
    /// found every repository under `~/Desktop` answering *"not a git
    /// checkout"* from a bundle while the same paths read correctly from
    /// a test. A `git rev-parse` that fails because the process may not
    /// look at the folder is not evidence about what is in the folder,
    /// and reporting it as though it were sends an operator to look for
    /// a problem in their repository that is not there.
    Unreadable {
        path: String,
        /// The operating system's own words.
        why: String,
    },
    /// A checkout with no remote eave could read an owner and name out
    /// of — a local-only repository, or one whose remote is a form this
    /// parser does not know.
    NoRemote,
    /// The host is not one eave can ask. **Named rather than folded into
    /// an error**: GitLab and Bitbucket checkouts are ordinary, and
    /// "eave does not do this yet" is a different sentence from "the
    /// read failed".
    UnsupportedHost { host: String },
    /// `gh` ran and the read did not come back usable.
    Error { message: String },
}

/// One repository's reviews, and what eave could not answer.
///
/// **Keyed by the repository, not by a checkout.** eave has five
/// worktrees of its own repository on this disk and the operator has
/// forty of another; a list with one row per *folder* would ask GitHub
/// the same question five times and print the same answer five times. So
/// the checkouts collapse into [`RepoReviews::roots`] and the repository
/// is the row — which is also why [`RepoReviews::repo`] is not an
/// `Option`: a row that names no repository is not a repository, it is a
/// [`SkippedRoot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoReviews {
    pub repo: RepoId,
    /// Every checkout of this repository eave was asked about. More than
    /// one is ordinary — a worktree per lane is how this project works —
    /// and the first is the one the read went through.
    pub roots: Vec<String>,
    /// The branch each of those checkouts is on. Detached heads are
    /// skipped rather than blanked, because a blank would match a review
    /// whose head branch failed to decode.
    ///
    /// **This is the session link.** eave already knows which checkout a
    /// session works in; matching that checkout's branch against a
    /// review's [`PullRequest::head`] is what lets a row say *this one is
    /// yours* without an operator linking anything by hand. The reference
    /// implementation links manually (a command-palette action, a
    /// right-click, an agent tool) because its threads are not branch-
    /// scoped the way eave's are — here the fact is already on hand, so
    /// asking for it would be asking for something eave knows.
    pub branches: Vec<String>,
    /// The OPEN reviews, newest first.
    pub rows: Vec<PullRequest>,
    /// The most recently settled reviews — merged and closed.
    ///
    /// **A second list, not more rows.** The operator asked for *"all
    /// repos and their history"*, and one list ordered by update time
    /// answers neither question well: on a busy repository the fifty
    /// most recently touched reviews are almost all merged, which buries
    /// the handful that are open. So the engine asks twice in one
    /// document and the surface draws two sections.
    ///
    /// It is also what makes the merged mark REACHABLE. The purple glyph
    /// was drawn, coloured and tested in the first pass, and the query
    /// asked `states: OPEN` — a decoder with no producer, which is the
    /// shape this project refuses everywhere else and shipped here.
    #[serde(default)]
    pub recent: Vec<PullRequest>,
    /// Never `Ok` with no read behind it: an unasked repository and a
    /// repository with no open reviews are different facts.
    pub state: PullsState,
}

impl RepoReviews {
    /// How many open reviews, or `None` when the read did not land.
    pub fn open_count(&self) -> Option<usize> {
        matches!(self.state, PullsState::Ok).then(|| self.rows.len())
    }

    /// Does any checkout of this repository sit on `branch`?
    pub fn on_branch(&self, branch: &str) -> bool {
        self.branches.iter().any(|b| b == branch)
    }
}

/// A folder eave looked at and got no repository out of.
///
/// **Counted, not listed.** Measured on the operator's own disk
/// 2026-09-14: pointing a scan at `~/Desktop` found 64 folders holding a
/// `.git`, of which **forty-four were orphaned worktree stubs and gutted
/// git directories** — a list that drew one row per folder would be nine
/// parts wreckage to one part repository. So the overview collapses
/// these into a count it can expand, which is the rail's own
/// `showing N of M · K hidden` rule: never silently dropped, never
/// allowed to bury the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRoot {
    pub root: String,
    pub state: PullsState,
}

impl SkippedRoot {
    /// The folder's own name — what an operator recognises it by.
    pub fn name(&self) -> &str {
        self.root
            .rsplit('/')
            .find(|p| !p.is_empty())
            .unwrap_or(&self.root)
    }
}

/// Every repository eave was asked about, in one answer.
///
/// **One read, not one per repository.** The engine resolves each
/// checkout's remote locally, collapses the duplicates, and asks GitHub
/// for all of them in a single aliased GraphQL document — so an overview
/// of a dozen repositories costs one network round trip rather than
/// twelve, and a repository this account cannot see is refused on its
/// own row instead of failing the list. Measured end to end on the
/// operator's disk: three roots in 1.0 s, a whole `~/Desktop` scan of 64
/// checkouts in 1.1 s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewsSnapshot {
    pub repos: Vec<RepoReviews>,
    /// Folders that held no repository, and why. See [`SkippedRoot`].
    pub skipped: Vec<SkippedRoot>,
    /// When this read completed, epoch ms — the `read just now` line.
    pub read_ms: u64,
    /// The folders eave was pointed at, before any scan expanded them.
    ///
    /// **The overview states its own scope.** A list headed *all
    /// repositories* that is really "the folders eave happens to know"
    /// is the empty-list lie one level up: the operator cannot tell a
    /// repository they have none of from a repository eave never looked
    /// for.
    pub roots: Vec<String>,
    /// The scan stopped at its cap before it ran out of folders.
    /// **Stated by the surface when true**, because a silently truncated
    /// list is the same lie as a silently truncated count.
    #[serde(default)]
    pub truncated: bool,
}

impl ReviewsSnapshot {
    /// The entry for a checkout, if this snapshot covers one.
    pub fn for_root(&self, root: &str) -> Option<&RepoReviews> {
        self.repos.iter().find(|r| r.roots.iter().any(|x| x == root))
    }

    /// Why a checkout has no entry, if eave looked at it and skipped it.
    pub fn skip_for_root(&self, root: &str) -> Option<&SkippedRoot> {
        self.skipped.iter().find(|s| s.root == root)
    }
}
