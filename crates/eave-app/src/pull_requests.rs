//! Grouping reviews into stacks, and stacks into list lines (Slice GH1).
//!
//! Pure, like [`crate::dock`]: every answer here is a function of the
//! rows the engine returned, so the whole of it is testable without a
//! window. The surface renders what this module decides and decides
//! nothing itself.
//!
//! The semantics are taken from the reference implementation
//! (`packages/shared/src/threadPullRequests.ts` and
//! `pullRequestListLines.ts` in `pingdotgg/t3code`, read 2026-09-14),
//! because they are the ones the operator's own reference capture was
//! drawn by. Two of them are not obvious and both are load-bearing:
//!
//! * **A stack reads bottom-first.** Depth 0 is the layer that merges
//!   into the stack's base — the one you would review first — and each
//!   layer above it indents. In the capture, `#11702` (`cli/service-
//!   restart -> main`) sits flush and `#11696` (`docs/standalone-install
//!   -> cli/service-restart`) sits indented beneath it, even though
//!   `#11696` is the *newer* review. Depth is topology, not time.
//! * **A whole stack floats on its newest layer.** A chain sorts by the
//!   most recent update anywhere in it, so a push to the top layer
//!   raises the stack rather than stranding it below unrelated reviews.
//!
//! Only the *fallback* chaining lives here. Native GitHub stacks arrive
//! already grouped and ordered from the host — see
//! [`eave_wire::StackRef`].

use eave_wire::pulls::PullRequest;

/// One drawn row: which review, and where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// Index into the rows this was built from. **An index, not a
    /// clone**: a list redraws every frame and a review carries two
    /// `String`s of prose.
    pub row: usize,
    /// 0 for the layer on the stack's own base; one step per layer above.
    pub depth: usize,
    /// Which chain this line belongs to, so a surface can tell one
    /// stack's lines from the next one's without re-deriving anything.
    pub chain: usize,
    /// Set **only on the bottom layer of a multi-layer chain**, because
    /// that is the row that names the whole stack. Every other layer
    /// draws no badge; a size on each of twelve rows says twelve times
    /// what is true once.
    pub stack: Option<StackBadge>,
    /// How many layers this chain has. `1` is a review standing alone.
    ///
    /// Carried on **every** line, not just the bottom one, because the
    /// surface draws the stack as a TREE: each layer has to know whether
    /// a connector runs up from it, down from it, or both, and a badge
    /// on the bottom row cannot tell the row above anything.
    pub chain_size: usize,
    /// Is this the topmost layer — the one nothing sits on?
    pub last_in_chain: bool,
}

impl Line {
    /// Part of a stack at all?
    pub fn stacked(&self) -> bool {
        self.chain_size > 1
    }

    /// Does a connector run UP from this row to the layer below it?
    pub fn joins_below(&self) -> bool {
        self.stacked() && self.depth > 0
    }

    /// Does a connector run DOWN from this row to the layer above it?
    pub fn joins_above(&self) -> bool {
        self.stacked() && !self.last_in_chain
    }
}

/// The `⚏ 12` on a stack's bottom row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackBadge {
    pub size: usize,
    /// Whether GitHub said so, or eave inferred it from base branches.
    ///
    /// Kept because they are different claims and the weaker one should
    /// be able to say so — an inferred stack is eave reading a shape
    /// into two ordinary reviews, and it can be wrong in a way the
    /// host's own answer cannot.
    pub native: bool,
}

/// Group rows into chains of row indices, each **bottom-first**.
///
/// Native stacks win outright; whatever is left is chained by matching
/// one review's base branch to another's head. All rows come from one
/// repository (the snapshot is per-repository), so a branch name is a
/// key on its own here — the reference has to qualify it by host and
/// repository because a thread may link reviews across several.
pub fn chains(rows: &[PullRequest]) -> Vec<Chain> {
    let mut out: Vec<Chain> = Vec::new();
    let mut placed = vec![false; rows.len()];

    // --- native, straight from the host ---
    let mut seen_stacks: Vec<u64> = Vec::new();
    for r in rows.iter() {
        let Some(s) = &r.stack else { continue };
        if seen_stacks.contains(&s.number) {
            continue;
        }
        seen_stacks.push(s.number);
        let mut members: Vec<usize> = (0..rows.len())
            .filter(|&i| rows[i].stack.as_ref().is_some_and(|m| m.number == s.number))
            .collect();
        members.sort_by_key(|&i| rows[i].stack.as_ref().map(|m| m.position).unwrap_or(0));
        for &i in &members {
            placed[i] = true;
        }
        out.push(Chain {
            layers: members,
            native: true,
        });
    }

    // --- the rest, chained by base -> head ---
    let rest: Vec<usize> = (0..rows.len()).filter(|&i| !placed[i]).collect();

    // A head branch used by two reviews identifies no parent, so it
    // identifies none at all. Guessing one would draw a layer under a
    // review it has nothing to do with.
    let mut by_head: Vec<(&str, Option<usize>)> = Vec::new();
    for &i in &rest {
        let head = rows[i].head.as_str();
        match by_head.iter_mut().find(|(h, _)| *h == head) {
            Some(slot) => slot.1 = None,
            None => by_head.push((head, Some(i))),
        }
    }
    let parent_of = |i: usize| -> Option<usize> {
        by_head
            .iter()
            .find(|(h, _)| *h == rows[i].base.as_str())
            .and_then(|(_, p)| *p)
            .filter(|&p| p != i)
    };

    let has_child: Vec<bool> = {
        let mut flags = vec![false; rows.len()];
        for &i in &rest {
            if let Some(p) = parent_of(i) {
                flags[p] = true;
            }
        }
        flags
    };

    // Walk down from each top — a review nothing builds on — collecting
    // its base chain, then reverse so the layers read bottom-first.
    for &top in &rest {
        if has_child[top] {
            continue;
        }
        let mut layers = Vec::new();
        let mut cursor = Some(top);
        while let Some(i) = cursor {
            if placed[i] {
                break;
            }
            placed[i] = true;
            layers.push(i);
            cursor = parent_of(i);
        }
        if !layers.is_empty() {
            layers.reverse();
            out.push(Chain {
                layers,
                native: false,
            });
        }
    }

    // **A cycle has no top.** `a` based on `b` and `b` based on `a` is
    // not a stack and cannot be ordered into one; the reviews still have
    // to be visible, so each stands alone rather than being drawn in an
    // order eave invented. (The `placed` check in the walk above is what
    // keeps this from looping forever.)
    for &i in &rest {
        if !placed[i] {
            placed[i] = true;
            out.push(Chain {
                layers: vec![i],
                native: false,
            });
        }
    }

    out
}

/// A group of reviews that stack on one another, bottom-first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub layers: Vec<usize>,
    pub native: bool,
}

/// Flatten chains into the drawn order: newest chain first, and each
/// chain's layers bottom-first beneath it.
pub fn lines(rows: &[PullRequest]) -> Vec<Line> {
    let mut chains = chains(rows);
    // A chain is as recent as its most recent layer.
    chains.sort_by(|a, b| {
        let newest = |c: &Chain| {
            c.layers
                .iter()
                .map(|&i| rows[i].updated_ms)
                .max()
                .unwrap_or(0)
        };
        newest(b)
            .cmp(&newest(a))
            // A stable tiebreak so two chains updated in the same
            // millisecond do not swap places between paints.
            .then_with(|| {
                let first = |c: &Chain| c.layers.first().map(|&i| rows[i].number).unwrap_or(0);
                first(b).cmp(&first(a))
            })
    });

    let mut out = Vec::new();
    for (chain_ix, chain) in chains.iter().enumerate() {
        let size = chain.layers.len();
        for (depth, &row) in chain.layers.iter().enumerate() {
            out.push(Line {
                row,
                depth,
                chain: chain_ix,
                stack: (depth == 0 && size > 1).then_some(StackBadge {
                    size,
                    native: chain.native,
                }),
                chain_size: size,
                last_in_chain: depth + 1 == size,
            });
        }
    }
    out
}

/// The rows whose head branch is one this operator has checked out.
///
/// **This is the whole session link.** eave knows which checkouts a
/// session works in and what branch each is on, so the review for the
/// work you are looking at identifies itself. The reference
/// implementation has an entire linking surface for this — a command,
/// a right-click, a dialog, an agent tool — because its threads are not
/// branch-scoped the way eave's sessions are.
///
/// **Branches, plural, because checkouts are.** A repository with a
/// worktree per lane has one branch out per worktree — this project has
/// six of its own — and marking only the first would pick whichever
/// checkout the scan happened to reach first. An empty branch name never
/// matches: a head that failed to decode is empty too, and a blank
/// matching a blank marks an arbitrary row.
pub fn mine(rows: &[PullRequest], branches: &[String]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| !r.head.is_empty() && branches.iter().any(|b| *b == r.head))
        .map(|(ix, _)| ix)
        .collect()
}

/// What the sidebar is narrowed to (Slice GH1).
///
/// **It matches only what the row SHOWS.** That is AG2 #10's rule for
/// the rail's filter and it is here for the same reason: a filter that
/// matches a field the operator cannot see answers with rows that look
/// unrelated, and one that misses a word they CAN see reads as broken.
/// So a review is matched on its title, its number and its head branch —
/// the three things its two lines draw — and a repository on its slug.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Folded to lowercase once, here, rather than per row.
    pub needle: String,
    /// Only reviews on a branch this operator has checked out.
    pub mine_only: bool,
}

impl Filter {
    pub fn new(needle: &str, mine_only: bool) -> Self {
        Filter {
            needle: needle.trim().to_lowercase(),
            mine_only,
        }
    }

    /// Is anything narrowed at all?
    pub fn is_open(&self) -> bool {
        self.needle.is_empty() && !self.mine_only
    }

    /// Does this review survive?
    pub fn keeps(&self, pr: &PullRequest, branches: &[String]) -> bool {
        if self.mine_only && !branches.iter().any(|b| *b == pr.head) {
            return false;
        }
        if self.needle.is_empty() {
            return true;
        }
        pr.title.to_lowercase().contains(&self.needle)
            || pr.head.to_lowercase().contains(&self.needle)
            || format!("#{}", pr.number).contains(&self.needle)
    }

    /// Which of `rows` survive, as indices into `rows`.
    pub fn rows(&self, rows: &[PullRequest], branches: &[String]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| self.keeps(r, branches))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// Does this repository survive?
    ///
    /// **A repository whose NAME matches is kept whole**, reviews and
    /// all: an operator typing `eave` is asking for that repository, not
    /// for the reviews whose titles happen to say `eave`. Otherwise it
    /// is kept only if something inside it survived — which is what
    /// keeps a filtered list from being a page of empty headings.
    pub fn keeps_repo(&self, slug: &str, rows: &[PullRequest], branches: &[String]) -> bool {
        if self.is_open() {
            return true;
        }
        if !self.needle.is_empty() && slug.to_lowercase().contains(&self.needle) {
            return true;
        }
        rows.iter().any(|r| self.keeps(r, branches))
    }

    /// Is this repository's name what matched, rather than its contents?
    pub fn matched_name(&self, slug: &str) -> bool {
        !self.needle.is_empty() && slug.to_lowercase().contains(&self.needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::pulls::{ChecksState, PullState, StackRef};

    fn pr(number: u64, head: &str, base: &str, updated_ms: u64) -> PullRequest {
        PullRequest {
            number,
            title: format!("pr {number}"),
            state: PullState::Open,
            head: head.to_string(),
            base: base.to_string(),
            author: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            updated_ms,
            url: String::new(),
            checks: ChecksState::None,
            stack: None,
        }
    }

    fn with_stack(mut p: PullRequest, number: u64, size: u32, position: u32) -> PullRequest {
        p.stack = Some(StackRef {
            number,
            size,
            position,
        });
        p
    }

    fn drawn(rows: &[PullRequest]) -> Vec<(u64, usize, Option<usize>)> {
        lines(rows)
            .into_iter()
            .map(|l| (rows[l.row].number, l.depth, l.stack.map(|s| s.size)))
            .collect()
    }

    #[test]
    fn a_native_stack_reads_bottom_first_whatever_order_it_arrives_in() {
        // The reference capture, and the rows arrive newest-first — so
        // the higher layer comes back from GitHub BEFORE the one it
        // sits on. Position is what orders a stack, never arrival.
        let rows = vec![
            with_stack(pr(11696, "docs/standalone", "cli/restart", 200), 11703, 2, 2),
            with_stack(pr(11702, "cli/restart", "main", 100), 11703, 2, 1),
        ];
        assert_eq!(
            drawn(&rows),
            vec![(11702, 0, Some(2)), (11696, 1, None)],
            "bottom layer flush and badged; the layer above indents and does not"
        );
    }

    #[test]
    fn a_stack_floats_on_its_newest_layer() {
        // #11702's stack was touched most recently at its TOP layer. It
        // must still outrank a lone newer-than-its-bottom review.
        let rows = vec![
            with_stack(pr(11696, "docs/standalone", "cli/restart", 900), 11703, 2, 2),
            with_stack(pr(11702, "cli/restart", "main", 100), 11703, 2, 1),
            pr(500, "solo", "main", 400),
        ];
        assert_eq!(
            drawn(&rows),
            vec![(11702, 0, Some(2)), (11696, 1, None), (500, 0, None)]
        );
    }

    #[test]
    fn reviews_with_no_native_stack_chain_by_branch() {
        // Exactly the shape of the capture's `sea/*` run, which GitHub
        // does report natively — this is the same topology arriving from
        // a repository that does not use the feature.
        let rows = vec![
            pr(3, "c", "b", 300),
            pr(1, "a", "main", 100),
            pr(2, "b", "a", 200),
        ];
        assert_eq!(
            drawn(&rows),
            vec![(1, 0, Some(3)), (2, 1, None), (3, 2, None)]
        );
        assert!(!chains(&rows)[0].native, "inferred, and says so");
    }

    #[test]
    fn a_reused_head_branch_identifies_no_parent() {
        // Two reviews on one head name: a parser that took the first
        // would draw #3 under whichever of them it happened to see
        // first, and the row would look perfectly reasonable.
        let rows = vec![
            pr(1, "shared", "main", 100),
            pr(2, "shared", "main", 200),
            pr(3, "child", "shared", 300),
        ];
        let drawn = drawn(&rows);
        assert_eq!(drawn.len(), 3);
        assert!(
            drawn.iter().all(|(_, depth, _)| *depth == 0),
            "ambiguous parentage draws flat, not guessed: {drawn:?}"
        );
    }

    #[test]
    fn a_cycle_stays_visible_and_does_not_hang() {
        // `a` based on `b` based on `a`. Neither is a top, so the walk
        // never starts; without the final sweep both reviews would
        // vanish from a list that claims to be complete.
        let rows = vec![pr(1, "a", "b", 100), pr(2, "b", "a", 200)];
        let drawn = drawn(&rows);
        assert_eq!(drawn.len(), 2);
        assert!(drawn.iter().all(|(_, depth, _)| *depth == 0));
    }

    #[test]
    fn a_native_stack_wins_over_the_branch_it_could_also_be_chained_by() {
        // Both signals present and agreeing in topology. The host's
        // answer is the one used, and the badge says native.
        let rows = vec![
            with_stack(pr(1, "a", "main", 100), 99, 2, 1),
            with_stack(pr(2, "b", "a", 200), 99, 2, 2),
        ];
        let chains = chains(&rows);
        assert_eq!(chains.len(), 1);
        assert!(chains[0].native);
        assert_eq!(chains[0].layers, vec![0, 1]);
    }

    #[test]
    fn one_review_is_a_chain_with_no_badge() {
        let rows = vec![pr(1, "a", "main", 100)];
        assert_eq!(drawn(&rows), vec![(1, 0, None)]);
    }

    #[test]
    fn every_row_is_drawn_exactly_once() {
        // The property that matters most: a listing may not lose a
        // review to a grouping pass. Mixed native, chained and orphaned.
        let rows = vec![
            with_stack(pr(1, "a", "main", 100), 50, 2, 1),
            with_stack(pr(2, "b", "a", 500), 50, 2, 2),
            pr(3, "c", "main", 300),
            pr(4, "d", "c", 400),
            pr(5, "e", "f", 600),
            pr(6, "f", "e", 700),
        ];
        let mut seen: Vec<usize> = lines(&rows).into_iter().map(|l| l.row).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..rows.len()).collect::<Vec<_>>());
    }

    #[test]
    fn the_checkout_branch_names_its_own_review() {
        let rows = vec![pr(1, "a", "main", 100), pr(2, "feature/x", "main", 200)];
        let b = |s: &str| vec![s.to_string()];
        assert_eq!(mine(&rows, &b("feature/x")), vec![1]);
        assert!(
            mine(&rows, &b("main")).is_empty(),
            "the base is not a review"
        );
        // A detached head, and an empty branch, are both "no answer" —
        // never row 0.
        assert!(mine(&rows, &[]).is_empty());
        assert!(mine(&rows, &b("")).is_empty());
    }

    /// **A worktree per lane is how this project works**, so a
    /// repository has several branches out at once and each of them can
    /// have a review.
    #[test]
    fn every_branch_this_operator_has_out_marks_its_own_review() {
        let rows = vec![
            pr(1, "a", "main", 100),
            pr(2, "feature/x", "main", 200),
            pr(3, "feature/y", "main", 300),
        ];
        let branches = vec!["feature/y".to_string(), "a".to_string()];
        assert_eq!(mine(&rows, &branches), vec![0, 2]);
    }

    /// A head that failed to decode is an empty string, and so is a
    /// branch eave could not read — matching them to each other marks an
    /// arbitrary row as the operator's own.
    #[test]
    fn an_empty_head_is_never_anybody_s_branch() {
        let rows = vec![pr(1, "", "main", 100)];
        assert!(mine(&rows, &["".to_string()]).is_empty());
    }
    fn branches() -> Vec<String> {
        vec!["feature/x".to_string()]
    }

    #[test]
    fn a_filter_matches_what_the_row_draws_and_nothing_else() {
        let mut rows = vec![pr(12, "feature/x", "main", 100), pr(34, "other", "main", 200)];
        rows[0].title = "Fix the parser".into();
        rows[1].title = "Add a widget".into();

        // The title, the branch, and the number — the three things the
        // two lines show.
        assert_eq!(Filter::new("parser", false).rows(&rows, &branches()), vec![0]);
        assert_eq!(Filter::new("feature/", false).rows(&rows, &branches()), vec![0]);
        assert_eq!(Filter::new("#34", false).rows(&rows, &branches()), vec![1]);
        // Case does not matter, and neither does surrounding space.
        assert_eq!(Filter::new("  WIDGET ", false).rows(&rows, &branches()), vec![1]);
        // The BASE branch is not drawn on a row, so it is not matched —
        // otherwise every review in a repository answers to `main`.
        assert!(Filter::new("main", false).rows(&rows, &branches()).is_empty());
    }

    #[test]
    fn mine_only_keeps_the_branches_this_operator_has_out() {
        let rows = vec![pr(12, "feature/x", "main", 100), pr(34, "other", "main", 200)];
        assert_eq!(Filter::new("", true).rows(&rows, &branches()), vec![0]);
        // With nothing checked out it keeps nothing, rather than
        // everything — "mine" with no branches is an empty answer, not a
        // disabled filter.
        assert!(Filter::new("", true).rows(&rows, &[]).is_empty());
    }

    /// **A repository whose name matches is kept whole.** An operator
    /// typing `eave` is asking for that repository, not for the reviews
    /// whose titles happen to say `eave`.
    #[test]
    fn a_repository_named_by_the_filter_keeps_all_its_reviews() {
        let rows = vec![pr(12, "a", "main", 100), pr(34, "b", "main", 200)];
        let f = Filter::new("eave", false);
        assert!(f.keeps_repo("mikestangdevs/eave", &rows, &[]));
        assert!(f.matched_name("mikestangdevs/eave"));
        // And one whose name does not match survives only on its
        // contents, so a filtered list is never a page of empty
        // headings.
        assert!(!f.keeps_repo("other/thing", &rows, &[]));
        assert!(!f.matched_name("other/thing"));
    }

    #[test]
    fn an_empty_filter_narrows_nothing() {
        let rows = vec![pr(12, "a", "main", 100)];
        let f = Filter::new("   ", false);
        assert!(f.is_open());
        assert_eq!(f.rows(&rows, &[]), vec![0]);
        assert!(f.keeps_repo("any/thing", &[], &[]));
    }
}
