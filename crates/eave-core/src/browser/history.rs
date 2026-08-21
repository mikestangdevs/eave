//! Browsing history: the visit log, and an index the omnibox can query
//! inside a frame.
//!
//! # Why this log is allowed to be rewritten
//! Everything else in this codebase appends and never rewrites, because a
//! session journal is an audit trail of what an agent did to somebody's
//! disk and a trail that can be edited is not one. Browsing history is the
//! opposite duty. A person who asks to be forgotten and is given a
//! tombstone reading `{"forgotten":"https://…"}` has not been forgotten;
//! the url is still on the disk, in plain text, under their name. So
//! [`HistoryEvent::Forgotten`] and [`HistoryEvent::Cleared`] trigger a
//! physical compaction ([`super::log::EventLog::compact`]) and the visits
//! are gone. That is the rule bending in the direction it has to bend, and
//! it is the only place in this codebase it bends at all.
//!
//! # The ranking, and why it needs no clock
//! Typeahead wants frecency: visits, discounted by age. Written out,
//!
//! ```text
//! score(now) = visits × 2^( -(now - last) / H )
//! ```
//!
//! Sorting by that would mean re-sorting every entry whenever `now` moved.
//! It does not, because taking log2 of both sides gives
//!
//! ```text
//! log2 score = log2(visits) − now/H + last/H
//! ```
//!
//! and `now/H` is the same subtraction for every entry. It cannot change
//! any pair's order. So the ranking key is
//!
//! ```text
//! rank = log2(visits) + last_visit_days / H
//! ```
//!
//! which contains no clock at all: it is computed once when an entry
//! changes and never goes stale. [`HALF_LIFE_DAYS`] is what calibrates it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::tabs::{normalize_tab_title, normalize_url};

/// How long it takes a visit to lose half its weight, in days.
///
/// Derived from one stated preference rather than picked: **a page visited
/// once today should rank level with a page visited twice a month ago.**
/// Setting `visits × 2^(−age/H)` equal for (1 visit, 0 days) and (2 visits,
/// 30 days) gives `1 = 2 × 2^(−30/H)`, so `2^(−30/H) = 1/2`, so `H = 30`.
///
/// Change the preference and this number follows from it; do not change
/// this number on its own.
pub const HALF_LIFE_DAYS: f64 = 30.0;

const MS_PER_DAY: f64 = 86_400_000.0;

/// The most distinct urls kept.
///
/// Derived from what the in-memory index costs, because that is the real
/// constraint — the log itself is cheap. Per entry: a url (~80 B in this
/// project's own browsing), a title (~60 B), three integers, plus its match
/// keys — [`MAX_KEYS_PER_ENTRY`] at ~20 B of text and 8 B of index each.
/// That is roughly 170 B of entry and 170 B of keys, so ~350 B, and at this
/// cap the whole index is about **35 MB**. That is the budget: an
/// always-resident structure in a desktop app, sized to be unremarkable
/// beside a single Chromium renderer.
///
/// It is also about a decade of ordinary browsing, so the cap is a
/// backstop, not a routine event.
pub const MAX_ENTRIES: usize = 100_000;

/// The most match keys one entry contributes to the index.
///
/// A cap is needed because titles are page-supplied and a title of a
/// thousand words would otherwise put a thousand keys in the index for one
/// url. Twelve is what a real entry uses: one normalised url, one full url,
/// up to three host forms, and the first several title words — past which
/// the extra words are ones nobody types first anyway.
pub const MAX_KEYS_PER_ENTRY: usize = 12;

/// Above this many keys in a prefix's lexicographic range, the query walks
/// entries in rank order instead of walking the range.
///
/// Not a quality tradeoff — **both paths return the same answer** (see
/// [`HistoryIndex::query`]). It is purely which one is cheaper: a rare
/// prefix has a tiny range and is cheapest scanned directly; a common
/// prefix matches so many entries that the rank-ordered walk fills its top
/// K within the first handful. The crossover is set at the point where the
/// range scan's cost (proportional to the range) exceeds the rank walk's
/// expected cost (proportional to entries/matches).
const RANGE_SCAN_CAP: usize = 4_096;

/// How far the rank-ordered walk goes before giving up and doing the exact
/// range scan instead. Only reachable when a common prefix's matches are
/// all clustered at the bottom of the ranking, which is not a shape real
/// history takes — the fallback exists so the answer is never wrong, at the
/// cost of being slower in a case that does not occur.
const RANK_SCAN_CAP: usize = 20_000;

/// How a visit came about. Recorded because "I typed this" and "a page
/// redirected me here" are different evidence about what the person wants,
/// and because a reload is not a new visit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// Typed or chosen in the omnibox.
    Typed,
    /// Followed from a link on a page.
    #[default]
    Link,
    /// The same page again. Does NOT increment the visit count — a page
    /// left open on a refresh loop would otherwise climb to the top of
    /// every typeahead.
    Reload,
    /// Restored by session restore. Also does not count: the person did
    /// not choose it, the restart did.
    Restore,
    /// A redirect, a form post, anything else.
    Other,
}

impl Transition {
    /// Whether this transition means the person went somewhere.
    pub fn counts_as_a_visit(&self) -> bool {
        !matches!(self, Transition::Reload | Transition::Restore)
    }
}

/// The durable vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryEvent {
    Visited {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        ts_ms: u64,
        #[serde(default)]
        transition: Transition,
    },
    /// A title that arrived after its visit committed — the ordinary case,
    /// since a page states its title once it has parsed.
    Titled { url: String, title: String },
    /// Forget one url. Triggers a compaction; see the module docs.
    Forgotten { url: String },
    /// Forget a time range. `since_ms`/`before_ms` are both optional, so
    /// "everything" is `Cleared { since_ms: None, before_ms: None }` and
    /// "the last hour" is a `since_ms`. Triggers a compaction.
    Cleared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_ms: Option<u64>,
    },
}

impl HistoryEvent {
    /// Whether applying this event means the log has to be physically
    /// rewritten rather than merely appended to.
    pub fn requires_compaction(&self) -> bool {
        matches!(
            self,
            HistoryEvent::Forgotten { .. } | HistoryEvent::Cleared { .. }
        )
    }
}

/// One url, everything known about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Visits that counted (see [`Transition::counts_as_a_visit`]). Never
    /// zero for an entry that exists.
    pub visit_count: u32,
    pub first_visit_ms: u64,
    pub last_visit_ms: u64,
}

impl HistoryEntry {
    /// The clock-free ranking key derived in the module docs. Higher is
    /// better.
    pub fn rank(&self) -> f64 {
        let visits = self.visit_count.max(1) as f64;
        visits.log2() + (self.last_visit_ms as f64 / MS_PER_DAY) / HALF_LIFE_DAYS
    }

    /// The page size a bare typeahead asks for, used by this module's own
    /// latency test so the measurement matches what the engine actually
    /// serves. The engine's default lives in `eave_engine::browser`.
    pub fn default_limit() -> usize {
        8
    }

    /// The frecency this entry actually has at a given moment. Not used for
    /// ordering — [`HistoryEntry::rank`] is — but it is the quantity the
    /// ordering is derived from, and a surface that wants to show a
    /// strength can ask for it.
    pub fn frecency_at(&self, now_ms: u64) -> f64 {
        let age_days = (now_ms.saturating_sub(self.last_visit_ms)) as f64 / MS_PER_DAY;
        self.visit_count.max(1) as f64 * 0.5f64.powf(age_days / HALF_LIFE_DAYS)
    }
}

/// Which kind of key a typeahead result matched on. Stated so a surface can
/// render the two differently; also the precedence the query applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The typed text is a prefix of the address. What somebody typing
    /// `git` at the omnibox almost always means.
    Address,
    /// The typed text starts a word in the page title.
    Title,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMatch {
    pub entry: HistoryEntry,
    pub matched: MatchKind,
}

/// Strip a url down to what a person types: no scheme, no `www.`, no
/// trailing slash. `https://www.github.com/` becomes `github.com`.
pub fn typing_form(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let no_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let no_www = no_scheme.strip_prefix("www.").unwrap_or(no_scheme);
    no_www.trim_end_matches('/').to_string()
}

/// The host part of a typing-form url, or the whole thing when there is no
/// path.
fn host_of(typing: &str) -> &str {
    typing.split(['/', '?', '#']).next().unwrap_or(typing)
}

/// Every string that should make this entry match when typed as a prefix,
/// paired with the kind of match it is.
fn match_keys(entry: &HistoryEntry) -> Vec<(String, MatchKind)> {
    let mut keys: Vec<(String, MatchKind)> = Vec::new();
    let typing = typing_form(&entry.url);
    if typing.is_empty() {
        return keys;
    }
    let host = host_of(&typing).to_string();
    keys.push((typing.clone(), MatchKind::Address));
    // The scheme-ful form, so pasting a full url still matches.
    let lower = entry.url.to_lowercase();
    if lower != typing {
        keys.push((lower, MatchKind::Address));
    }
    if host != typing {
        keys.push((host.clone(), MatchKind::Address));
    }
    // `docs.github.com` should also be reachable by typing `github.com`:
    // drop leading labels while more than two remain, so `com` alone never
    // becomes a key.
    let labels: Vec<&str> = host.split('.').collect();
    for start in 1..labels.len().saturating_sub(1) {
        keys.push((labels[start..].join("."), MatchKind::Address));
    }
    if let Some(title) = &entry.title {
        for word in title
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            keys.push((word.to_string(), MatchKind::Title));
        }
    }
    keys.truncate(MAX_KEYS_PER_ENTRY);
    keys.sort();
    keys.dedup();
    keys
}

/// The visit log folded into one entry per url, plus the two sorted key
/// arrays the omnibox searches.
///
/// Rebuilt wholesale by [`HistoryIndex::rebuild`] rather than maintained
/// incrementally. That is a deliberate trade: a visit is recorded far less
/// often than a keystroke is typed, and an index that is rebuilt has no way
/// to drift from the entries it indexes.
#[derive(Debug, Clone, Default)]
pub struct HistoryIndex {
    /// Sorted by [`HistoryEntry::rank`], best first. The position in this
    /// vector IS the rank, which is what makes the rank-ordered walk a
    /// straight iteration.
    entries: Vec<HistoryEntry>,
    by_url: HashMap<String, usize>,
    /// `(key, entry index)`, sorted lexicographically by key.
    address_keys: Vec<(Box<str>, u32)>,
    title_keys: Vec<(Box<str>, u32)>,
    /// Per entry, its keys, so the rank-ordered walk can test an entry
    /// without searching the sorted arrays. Flat storage with offsets:
    /// `entry_keys[key_at[i]..key_at[i + 1]]` are entry `i`'s.
    entry_address_keys: Vec<Box<str>>,
    entry_address_at: Vec<u32>,
    entry_title_keys: Vec<Box<str>>,
    entry_title_at: Vec<u32>,
    /// How many entries [`HistoryIndex::rebuild`] dropped to hold
    /// [`MAX_ENTRIES`].
    ///
    /// It has to be recorded here because `rebuild` truncates: after it
    /// runs, `entries.len()` is at most the cap whether or not the cap
    /// bit, so asking the index how big it is can never reveal that it
    /// was too big. Reading the truncated length was the bug — it made
    /// `over_cap` permanently false, so the log grew without limit while
    /// the code that was supposed to bound it ran and did nothing.
    dropped_by_cap: usize,
}

impl HistoryIndex {
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, url: &str) -> Option<&HistoryEntry> {
        self.by_url.get(url).map(|i| &self.entries[*i])
    }

    /// How many entries the last rebuild dropped to hold [`MAX_ENTRIES`].
    /// Non-zero means the retained visits on disk still hold urls this
    /// index no longer offers, and the log wants compacting.
    pub fn dropped_by_cap(&self) -> usize {
        self.dropped_by_cap
    }

    /// Build from a set of entries. Sorts by rank, then indexes.
    pub fn rebuild(&mut self, mut entries: Vec<HistoryEntry>) {
        // Descending rank; ties broken on last visit then url so the order
        // is total and a test can assert on it.
        entries.sort_by(|a, b| {
            b.rank()
                .total_cmp(&a.rank())
                .then(b.last_visit_ms.cmp(&a.last_visit_ms))
                .then(a.url.cmp(&b.url))
        });
        self.dropped_by_cap = entries.len().saturating_sub(MAX_ENTRIES);
        entries.truncate(MAX_ENTRIES);

        self.by_url = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.url.clone(), i))
            .collect();
        self.address_keys.clear();
        self.title_keys.clear();
        self.entry_address_keys.clear();
        self.entry_title_keys.clear();
        self.entry_address_at = Vec::with_capacity(entries.len() + 1);
        self.entry_title_at = Vec::with_capacity(entries.len() + 1);

        for (i, entry) in entries.iter().enumerate() {
            self.entry_address_at
                .push(self.entry_address_keys.len() as u32);
            self.entry_title_at.push(self.entry_title_keys.len() as u32);
            for (key, kind) in match_keys(entry) {
                let boxed: Box<str> = key.into_boxed_str();
                match kind {
                    MatchKind::Address => {
                        self.address_keys.push((boxed.clone(), i as u32));
                        self.entry_address_keys.push(boxed);
                    }
                    MatchKind::Title => {
                        self.title_keys.push((boxed.clone(), i as u32));
                        self.entry_title_keys.push(boxed);
                    }
                }
            }
        }
        self.entry_address_at
            .push(self.entry_address_keys.len() as u32);
        self.entry_title_at.push(self.entry_title_keys.len() as u32);
        self.address_keys.sort();
        self.title_keys.sort();
        self.entries = entries;
    }

    /// Top `limit` matches for what has been typed so far.
    ///
    /// Addresses first, then titles for whatever room is left — typing
    /// `git` means `github.com` far more often than it means an article
    /// with "git" in the headline, and a typeahead that cannot commit to
    /// that ordering makes the operator read every row.
    ///
    /// Empty input returns nothing. It is not an error and it is not
    /// "everything": an omnibox with nothing typed has nothing to suggest
    /// from history, and returning the whole log would be the most
    /// expensive possible answer to the least informative question.
    pub fn query(&self, typed: &str, limit: usize) -> Vec<HistoryMatch> {
        let needle = typed.trim().to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        // Typing a full url must match the entry that stored the short
        // form, so search on the typed text AND its typing form.
        let short = typing_form(&needle);
        let mut out: Vec<HistoryMatch> = Vec::new();
        let mut taken: Vec<bool> = vec![false; self.entries.len()];

        for (keys, per_entry, offsets, kind) in [
            (
                &self.address_keys,
                &self.entry_address_keys,
                &self.entry_address_at,
                MatchKind::Address,
            ),
            (
                &self.title_keys,
                &self.entry_title_keys,
                &self.entry_title_at,
                MatchKind::Title,
            ),
        ] {
            if out.len() >= limit {
                break;
            }
            let mut hits: Vec<u32> = Vec::new();
            for probe in [needle.as_str(), short.as_str()] {
                if probe.is_empty() {
                    continue;
                }
                self.collect(keys, per_entry, offsets, probe, limit, &mut hits);
                if probe == short {
                    break;
                }
                if short == needle {
                    break;
                }
            }
            hits.sort_unstable();
            hits.dedup();
            // `hits` are entry indices, and entry indices are rank order,
            // so sorting them ascending IS sorting by rank.
            for i in hits {
                if out.len() >= limit {
                    break;
                }
                if taken[i as usize] {
                    continue;
                }
                taken[i as usize] = true;
                out.push(HistoryMatch {
                    entry: self.entries[i as usize].clone(),
                    matched: kind,
                });
            }
        }
        out
    }

    /// Entry indices whose keys start with `prefix`, best-ranked first,
    /// at most `limit` of them appended to `into`.
    ///
    /// Two strategies, **both exact**; which one runs is only a question of
    /// which is cheaper. See [`RANGE_SCAN_CAP`].
    fn collect(
        &self,
        keys: &[(Box<str>, u32)],
        per_entry: &[Box<str>],
        offsets: &[u32],
        prefix: &str,
        limit: usize,
        into: &mut Vec<u32>,
    ) {
        let lo = keys.partition_point(|(k, _)| k.as_ref() < prefix);
        let hi = lo + keys[lo..].partition_point(|(k, _)| k.starts_with(prefix));
        let range = hi - lo;
        if range == 0 {
            return;
        }
        // The exact answer by range: every entry index in the prefix
        // range, deduped, best-ranked first (entry index IS rank order).
        let exact = || -> Vec<u32> {
            let mut found: Vec<u32> = keys[lo..hi].iter().map(|(_, i)| *i).collect();
            found.sort_unstable();
            found.dedup();
            found.truncate(limit);
            found
        };
        if range <= RANGE_SCAN_CAP {
            into.extend(exact());
            return;
        }
        // Common prefix: walk entries best-first and stop as soon as the
        // top `limit` are in hand.
        //
        // Accumulated locally, never straight into `into`: the fallback
        // below has to discard a partial walk, and `into` may already hold
        // a previous probe's hits that must survive.
        let mut walked: Vec<u32> = Vec::with_capacity(limit);
        let entry_count = offsets.len().saturating_sub(1);
        for i in 0..entry_count.min(RANK_SCAN_CAP) {
            let from = offsets[i] as usize;
            let to = offsets[i + 1] as usize;
            if per_entry[from..to].iter().any(|k| k.starts_with(prefix)) {
                walked.push(i as u32);
                if walked.len() == limit {
                    into.extend(walked);
                    return;
                }
            }
        }
        if entry_count > RANK_SCAN_CAP {
            // The walk ran out of budget without filling the page. Pay for
            // the exact answer rather than return a short one.
            into.extend(exact());
        } else {
            // The walk saw every entry there is, so it IS the exact answer.
            into.extend(walked);
        }
    }
}

/// The visit log, folded. Holds the events too, because forgetting means
/// rewriting the file and the file's contents are exactly these events
/// minus the forgotten ones.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Every retained visit, in the order recorded. This IS what the log
    /// file contains after a compaction.
    visits: Vec<HistoryEvent>,
    index: HistoryIndex,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn index(&self) -> &HistoryIndex {
        &self.index
    }

    /// The events that belong in the file. After a compaction this is what
    /// gets written; between compactions it is what has been appended.
    pub fn retained(&self) -> &[HistoryEvent] {
        &self.visits
    }

    /// Fold a whole log at load. One rebuild at the end rather than one per
    /// event.
    pub fn load(&mut self, events: Vec<HistoryEvent>) {
        for event in events {
            self.absorb(&event);
        }
        self.reindex();
    }

    /// Apply one event. Returns whether anything changed.
    pub fn apply(&mut self, event: &HistoryEvent) -> bool {
        let changed = self.absorb(event);
        if changed {
            self.reindex();
        }
        changed
    }

    /// Fold without reindexing.
    fn absorb(&mut self, event: &HistoryEvent) -> bool {
        match event {
            HistoryEvent::Visited { url, .. } | HistoryEvent::Titled { url, .. } => {
                if normalize_url(url).is_none() {
                    return false;
                }
                self.visits.push(event.clone());
                true
            }
            HistoryEvent::Forgotten { url } => {
                let before = self.visits.len();
                self.visits.retain(|v| match v {
                    HistoryEvent::Visited { url: u, .. } | HistoryEvent::Titled { url: u, .. } => {
                        u != url
                    }
                    _ => true,
                });
                before != self.visits.len()
            }
            HistoryEvent::Cleared {
                since_ms,
                before_ms,
            } => {
                let before = self.visits.len();
                // A range clear drops visits by time. A `Titled` carries no
                // time of its own, so it goes when its url has no visits
                // left — done in a second pass below.
                self.visits.retain(|v| match v {
                    HistoryEvent::Visited { ts_ms, .. } => {
                        let after_start = since_ms.is_none_or(|s| *ts_ms >= s);
                        let before_end = before_ms.is_none_or(|b| *ts_ms < b);
                        !(after_start && before_end)
                    }
                    _ => true,
                });
                let live: std::collections::HashSet<&str> = self
                    .visits
                    .iter()
                    .filter_map(|v| match v {
                        HistoryEvent::Visited { url, .. } => Some(url.as_str()),
                        _ => None,
                    })
                    .collect();
                let keep: Vec<bool> = self
                    .visits
                    .iter()
                    .map(|v| match v {
                        HistoryEvent::Titled { url, .. } => live.contains(url.as_str()),
                        _ => true,
                    })
                    .collect();
                let mut it = keep.into_iter();
                self.visits.retain(|_| it.next().unwrap_or(true));
                before != self.visits.len()
            }
        }
    }

    /// Rebuild the entry set and the index from the retained visits.
    fn reindex(&mut self) {
        let mut by_url: HashMap<&str, HistoryEntry> = HashMap::new();
        for v in &self.visits {
            match v {
                HistoryEvent::Visited {
                    url,
                    title,
                    ts_ms,
                    transition,
                } => {
                    let title = title.as_deref().and_then(normalize_tab_title);
                    let e = by_url.entry(url.as_str()).or_insert_with(|| HistoryEntry {
                        url: url.clone(),
                        title: None,
                        visit_count: 0,
                        first_visit_ms: *ts_ms,
                        last_visit_ms: *ts_ms,
                    });
                    if transition.counts_as_a_visit() {
                        e.visit_count += 1;
                    }
                    e.first_visit_ms = e.first_visit_ms.min(*ts_ms);
                    e.last_visit_ms = e.last_visit_ms.max(*ts_ms);
                    // A later visit's title wins: pages get retitled, and
                    // the current title is the one somebody would search.
                    if title.is_some() {
                        e.title = title;
                    }
                }
                HistoryEvent::Titled { url, title } => {
                    if let Some(e) = by_url.get_mut(url.as_str()) {
                        e.title = normalize_tab_title(title);
                    }
                }
                _ => {}
            }
        }
        let entries: Vec<HistoryEntry> = by_url
            .into_values()
            // A url whose every visit was a reload or a restore has a
            // count of zero. It was still visited, so it stays — with the
            // floor of one that `rank` already applies.
            .map(|mut e| {
                e.visit_count = e.visit_count.max(1);
                e
            })
            .collect();
        self.index.rebuild(entries);
    }

    /// True when the retained visits hold more distinct urls than the cap
    /// allows, so the log should be compacted down to what the index
    /// actually offers.
    ///
    /// Asks the INDEX what it had to drop, not how big it is: `rebuild`
    /// truncates, so its length is at most the cap either way.
    pub fn over_cap(&self) -> bool {
        self.index.dropped_by_cap() > 0
    }

    /// Drop the worst-ranked entries until the cap holds, and return
    /// whether anything was dropped. The caller compacts the file
    /// afterwards — [`History::retained`] is then what belongs in it.
    pub fn prune_to_cap(&mut self) -> bool {
        if !self.over_cap() {
            return false;
        }
        let keep: std::collections::HashSet<String> = self
            .index
            .entries()
            .iter()
            .take(MAX_ENTRIES)
            .map(|e| e.url.clone())
            .collect();
        self.visits.retain(|v| match v {
            HistoryEvent::Visited { url, .. } | HistoryEvent::Titled { url, .. } => {
                keep.contains(url)
            }
            _ => true,
        });
        self.reindex();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn visit(url: &str, title: Option<&str>, ts_ms: u64) -> HistoryEvent {
        HistoryEvent::Visited {
            url: url.into(),
            title: title.map(str::to_string),
            ts_ms,
            transition: Transition::Link,
        }
    }

    fn history_of(events: Vec<HistoryEvent>) -> History {
        let mut h = History::new();
        h.load(events);
        h
    }

    /// The calibration point [`HALF_LIFE_DAYS`] is DERIVED from: one visit
    /// today is worth two visits a month ago. If this fails, either the
    /// constant moved without its reason moving, or the reason moved
    /// without the constant.
    #[test]
    fn the_half_life_is_the_number_its_stated_preference_implies() {
        let now = 100 * DAY;
        let today = HistoryEntry {
            url: "https://today/".into(),
            title: None,
            visit_count: 1,
            first_visit_ms: now,
            last_visit_ms: now,
        };
        let a_month_ago_twice = HistoryEntry {
            url: "https://old/".into(),
            title: None,
            visit_count: 2,
            first_visit_ms: now - 30 * DAY,
            last_visit_ms: now - 30 * DAY,
        };
        let a = today.frecency_at(now);
        let b = a_month_ago_twice.frecency_at(now);
        assert!(
            (a - b).abs() < 1e-9,
            "one visit today ({a}) should tie two visits 30 days ago ({b}) — \
             HALF_LIFE_DAYS is {HALF_LIFE_DAYS} and the preference it was \
             derived from no longer holds"
        );
    }

    /// The whole reason `rank` exists: it orders entries the same way
    /// frecency does, at every moment, without containing a clock. If this
    /// fails, the index would have to be re-sorted on every query.
    #[test]
    fn the_rank_key_orders_exactly_as_frecency_does_at_any_moment() {
        let base = 400 * DAY;
        let entries: Vec<HistoryEntry> = [
            ("https://a/", 1u32, base),
            ("https://b/", 9, base - 45 * DAY),
            ("https://c/", 3, base - 5 * DAY),
            ("https://d/", 50, base - 200 * DAY),
            ("https://e/", 2, base - DAY),
        ]
        .into_iter()
        .map(|(url, visits, last)| HistoryEntry {
            url: url.into(),
            title: None,
            visit_count: visits,
            first_visit_ms: last,
            last_visit_ms: last,
        })
        .collect();

        let by_rank = {
            let mut v = entries.clone();
            v.sort_by(|x, y| y.rank().total_cmp(&x.rank()));
            v.iter().map(|e| e.url.clone()).collect::<Vec<_>>()
        };
        // The same ordering must fall out of real frecency at every `now`
        // we try — including ones long after the last visit.
        for now in [base, base + DAY, base + 365 * DAY, base + 3650 * DAY] {
            let mut v = entries.clone();
            v.sort_by(|x, y| y.frecency_at(now).total_cmp(&x.frecency_at(now)));
            assert_eq!(
                v.iter().map(|e| e.url.clone()).collect::<Vec<_>>(),
                by_rank,
                "rank and frecency disagreed at now={now}"
            );
        }
    }

    /// A reload is not a new visit. A page left open on a refresh loop
    /// would otherwise climb to the top of every typeahead.
    #[test]
    fn a_reload_or_a_restore_does_not_count_as_a_visit() {
        let h = history_of(vec![
            visit("https://x/", None, DAY),
            HistoryEvent::Visited {
                url: "https://x/".into(),
                title: None,
                ts_ms: 2 * DAY,
                transition: Transition::Reload,
            },
            HistoryEvent::Visited {
                url: "https://x/".into(),
                title: None,
                ts_ms: 3 * DAY,
                transition: Transition::Restore,
            },
        ]);
        let e = h.index().get("https://x/").unwrap();
        assert_eq!(e.visit_count, 1, "a reload inflated the visit count");
        assert_eq!(
            e.last_visit_ms,
            3 * DAY,
            "a reload should still move the recency — the page WAS seen"
        );
    }

    /// A url whose every visit was a reload still exists, with the floor of
    /// one that `rank` applies. It was visited; the count is just not
    /// evidence of choosing it.
    #[test]
    fn a_url_visited_only_by_reload_still_exists() {
        let h = history_of(vec![HistoryEvent::Visited {
            url: "https://x/".into(),
            title: None,
            ts_ms: DAY,
            transition: Transition::Reload,
        }]);
        assert_eq!(h.index().get("https://x/").unwrap().visit_count, 1);
    }

    /// Titles arrive after their visit, and history has to learn them or
    /// every omnibox row would be searchable by url alone.
    #[test]
    fn a_title_learned_after_the_visit_becomes_searchable() {
        let h = history_of(vec![
            visit("https://x/", None, DAY),
            HistoryEvent::Titled {
                url: "https://x/".into(),
                title: "The Rust Programming Language".into(),
            },
        ]);
        let hits = h.index().query("rust", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched, MatchKind::Title);
    }

    /// Addresses outrank titles. Typing `git` means `github.com` far more
    /// often than it means an article with "git" in the headline.
    #[test]
    fn an_address_match_comes_before_a_title_match() {
        let h = history_of(vec![
            // The title match is deliberately given far more visits, so a
            // ranking that ignored the kind would put it first.
            HistoryEvent::Visited {
                url: "https://example.com/post".into(),
                title: Some("Git is hard".into()),
                ts_ms: 10 * DAY,
                transition: Transition::Link,
            },
            visit("https://example.com/post", Some("Git is hard"), 11 * DAY),
            visit("https://example.com/post", Some("Git is hard"), 12 * DAY),
            visit("https://github.com/", None, DAY),
        ]);
        let hits = h.index().query("git", 5);
        assert_eq!(hits[0].entry.url, "https://github.com/");
        assert_eq!(hits[0].matched, MatchKind::Address);
        assert_eq!(hits[1].matched, MatchKind::Title);
    }

    /// A url is matched the way a person types it: no scheme, no `www.`,
    /// and a sub-domained host is also reachable by its parent domain.
    #[test]
    fn a_url_matches_the_way_a_person_types_it() {
        assert_eq!(typing_form("https://www.GitHub.com/a/"), "github.com/a");
        assert_eq!(typing_form("http://x.test"), "x.test");
        // `www.` is stripped, so the stored url is reachable without it.
        let one = history_of(vec![visit("https://www.github.com/anthropics", None, DAY)]);
        assert_eq!(
            one.index().query("github", 5).len(),
            1,
            "www. was not stripped"
        );
        // ...and typing the full url, scheme and all, still matches.
        assert_eq!(one.index().query("https://www.github.com/anth", 5).len(), 1);

        let h = history_of(vec![
            visit("https://www.github.com/anthropics", None, DAY),
            visit("https://docs.github.com/", None, DAY),
        ]);
        let idx = h.index();
        // A sub-domained host is reachable by its own name...
        assert_eq!(idx.query("docs.github", 5).len(), 1);
        // ...and by its parent domain, which also still finds the bare one.
        assert_eq!(
            idx.query("github.com", 5).len(),
            2,
            "a sub-domained host must be reachable by its parent domain"
        );
        // A bare TLD is never a key, or every url would match "com".
        assert!(idx.query("com", 5).is_empty());
    }

    /// An empty omnibox suggests nothing. Not everything — which would be
    /// the most expensive possible answer to the least informative
    /// question.
    #[test]
    fn nothing_typed_suggests_nothing() {
        let h = history_of(vec![visit("https://x/", None, DAY)]);
        assert!(h.index().query("", 5).is_empty());
        assert!(h.index().query("   ", 5).is_empty());
        assert!(h.index().query("x", 0).is_empty());
    }

    /// **The claim that licenses the query's two strategies.** Above
    /// [`RANGE_SCAN_CAP`] the query walks entries in rank order instead of
    /// walking the key range; the two must return the SAME answer, or the
    /// threshold is a silent quality cliff rather than a cost optimisation.
    ///
    /// Built so the common prefix genuinely blows past the cap: 3000 urls
    /// sharing one, versus a rare prefix matching a handful.
    #[test]
    fn both_query_strategies_return_the_same_answer() {
        let mut events = Vec::new();
        for i in 0..3000u64 {
            // Same prefix, wildly varying rank, so the top of the ranking
            // is scattered through the key range rather than at its head.
            let visits = (i % 17) + 1;
            for v in 0..visits {
                events.push(visit(
                    &format!("https://common{i:05}.test/"),
                    None,
                    (i * 7 + v) % (500 * DAY / DAY) * DAY + DAY,
                ));
            }
        }
        events.push(visit("https://rare-one.test/", None, 400 * DAY));
        let h = history_of(events);
        let idx = h.index();

        // Brute force: every entry whose keys start with the prefix, in
        // rank order, which is entry-index order.
        let brute = |prefix: &str, limit: usize| -> Vec<String> {
            idx.entries()
                .iter()
                .filter(|e| {
                    match_keys(e)
                        .iter()
                        .any(|(k, kind)| *kind == MatchKind::Address && k.starts_with(prefix))
                })
                .take(limit)
                .map(|e| e.url.clone())
                .collect()
        };

        for (prefix, limit) in [
            ("common", 8),
            ("common", 25),
            ("common0001", 8),
            ("rare", 8),
        ] {
            let got: Vec<String> = idx
                .query(prefix, limit)
                .into_iter()
                .map(|m| m.entry.url)
                .collect();
            assert_eq!(
                got,
                brute(prefix, limit),
                "the two strategies disagree for prefix {prefix:?} at limit {limit}"
            );
        }
    }

    /// Typing something whose scheme-stripped form differs from what was
    /// typed searches BOTH, and neither probe may discard the other's
    /// hits.
    ///
    /// The bug this pins: the exact-answer fallback inside `collect`
    /// cleared the shared output buffer, so a second probe taking that
    /// path would throw away everything the first probe had found. Only
    /// reachable past `RANK_SCAN_CAP`, which is exactly the kind of path
    /// that ships broken.
    #[test]
    fn searching_both_forms_of_what_was_typed_keeps_both_sets_of_hits() {
        let h = history_of(vec![
            visit("https://docs.rs/serde", None, 5 * DAY),
            visit("https://http-example.test/", None, 4 * DAY),
        ]);
        // "https://docs.rs" typed in full: the needle has a scheme, the
        // typing form does not, so both probes run.
        let hits = h.index().query("https://docs.rs", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.url, "https://docs.rs/serde");
        // And a needle whose typing form is identical still works.
        assert_eq!(h.index().query("docs.rs", 5).len(), 1);
    }

    /// The query must answer well inside a frame.
    ///
    /// The budget is derived, not chosen: the omnibox has to redraw within
    /// one 60 Hz frame (16.7 ms) and the render is the part that must not
    /// be squeezed, so the engine-side answer gets at most a third of it —
    /// **5 ms**. This runs UNOPTIMISED (eave-core is not in the workspace's
    /// `opt-level = 2` list), which is the pessimistic case: the release
    /// number is the one that ships and it is far below this.
    ///
    /// 20,000 urls, not [`MAX_ENTRIES`], because building 100k entries in a
    /// debug build costs more wall-clock than the assertion is worth. The
    /// shape of the cost is what is being asserted — a binary search plus a
    /// bounded scan — and it does not change with N. That is a stated
    /// limit of this test, and the slice report repeats it.
    #[test]
    fn a_typeahead_query_answers_well_inside_a_frame() {
        const N: u64 = 20_000;
        const BUDGET: std::time::Duration = std::time::Duration::from_millis(5);

        let mut events = Vec::with_capacity(N as usize);
        for i in 0..N {
            events.push(visit(
                &format!("https://site{i:05}.example/page/{i}"),
                Some(&format!("Site {i} — a page about things")),
                (i % 400) * DAY + DAY,
            ));
        }
        let h = history_of(events);
        let idx = h.index();
        assert_eq!(idx.len(), N as usize);

        // Every shape of query: a one-character prefix (the widest range),
        // a whole-word title term, a narrowing address, and a miss.
        let probes = [
            "s",
            "si",
            "site0",
            "site01234",
            "https://site00042",
            "page",
            "zzzz",
        ];
        let mut worst = std::time::Duration::ZERO;
        let mut worst_probe = "";
        for probe in probes {
            // Three runs; the worst is the one reported, not the best.
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let hits = idx.query(probe, HistoryEntry::default_limit());
                let took = t.elapsed();
                std::hint::black_box(&hits);
                if took > worst {
                    worst = took;
                    worst_probe = probe;
                }
            }
        }
        assert!(
            worst < BUDGET,
            "typeahead over {N} urls took {worst:?} on {worst_probe:?}, past the \
             {BUDGET:?} budget (a third of a 60 Hz frame, unoptimised)"
        );
        // Print it so the number is visible in `cargo test -- --nocapture`
        // rather than only asserted.
        println!("typeahead worst over {N} urls: {worst:?} on {worst_probe:?}");
    }

    /// Forgetting is physical: the retained set — which IS what the file
    /// gets rewritten to — no longer mentions the url anywhere.
    #[test]
    fn forgetting_a_url_removes_every_trace_of_it_from_the_retained_set() {
        let mut h = history_of(vec![
            visit("https://keep/", None, DAY),
            visit("https://secret/", Some("private"), DAY),
            HistoryEvent::Titled {
                url: "https://secret/".into(),
                title: "still private".into(),
            },
        ]);
        assert!(h.apply(&HistoryEvent::Forgotten {
            url: "https://secret/".into()
        }));
        let raw = serde_json::to_string(h.retained()).unwrap();
        assert!(!raw.contains("secret"), "a tombstone kept the url: {raw}");
        assert!(
            !raw.contains("private"),
            "the title survived the forget: {raw}"
        );
        assert!(raw.contains("keep"));
        assert!(h.index().get("https://secret/").is_none());
        // Forgetting something that was never there is a no-op, not an
        // error, and must not trigger a rewrite.
        assert!(!h.apply(&HistoryEvent::Forgotten {
            url: "https://never/".into()
        }));
    }

    /// A range clear takes its window and nothing else — and takes the
    /// orphaned titles with it, since a title whose visits are gone is a
    /// url sitting in the file under a different key.
    #[test]
    fn a_range_clear_takes_its_window_and_the_titles_it_orphans() {
        let mut h = history_of(vec![
            visit("https://old/", Some("old page"), DAY),
            visit("https://new/", None, 10 * DAY),
            HistoryEvent::Titled {
                url: "https://old/".into(),
                title: "old page retitled".into(),
            },
        ]);
        assert!(h.apply(&HistoryEvent::Cleared {
            since_ms: None,
            before_ms: Some(5 * DAY),
        }));
        let raw = serde_json::to_string(h.retained()).unwrap();
        assert!(
            !raw.contains("old"),
            "the cleared window left traces: {raw}"
        );
        assert!(raw.contains("new"));
        assert_eq!(h.index().len(), 1);

        // Both bounds absent means everything.
        assert!(h.apply(&HistoryEvent::Cleared {
            since_ms: None,
            before_ms: None,
        }));
        assert!(h.index().is_empty());
        assert!(h.retained().is_empty());
    }

    /// Only the destructive events rewrite the file. If this ever says
    /// otherwise, every visit would pay for a full rewrite.
    #[test]
    fn only_forgetting_requires_a_rewrite() {
        assert!(!visit("https://x/", None, 0).requires_compaction());
        assert!(!HistoryEvent::Titled {
            url: "https://x/".into(),
            title: "t".into()
        }
        .requires_compaction());
        assert!(HistoryEvent::Forgotten {
            url: "https://x/".into()
        }
        .requires_compaction());
        assert!(HistoryEvent::Cleared {
            since_ms: None,
            before_ms: None
        }
        .requires_compaction());
    }

    /// Serde defaults: an event from a build predating `transition` reads
    /// as an ordinary link visit, which is what those visits were.
    #[test]
    fn an_older_builds_visit_still_parses() {
        let old = r#"{"kind":"visited","url":"https://x/","ts_ms":5}"#;
        let e: HistoryEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            e,
            HistoryEvent::Visited {
                url: "https://x/".into(),
                title: None,
                ts_ms: 5,
                transition: Transition::Link,
            }
        );
        // ...and it counts, which a default of `Reload` would have
        // silently prevented for every pre-existing visit.
        assert!(Transition::default().counts_as_a_visit());
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// The entry cap is enforced, and it keeps the BEST-ranked entries —
    /// dropping the ones a typeahead was least likely to offer.
    ///
    /// Run at real scale ([`MAX_ENTRIES`] + 1) rather than against a
    /// scaled-down constant, because a cap tested at a size it will never
    /// see is testing a different code path's arithmetic.
    #[test]
    fn the_entry_cap_drops_the_worst_ranked_and_keeps_the_rest() {
        let day = 86_400_000u64;
        let mut events = Vec::with_capacity(MAX_ENTRIES + 1);
        // Rank rises with `i`, so entry 0 is the worst and must be the one
        // dropped when one entry too many exists.
        for i in 0..=MAX_ENTRIES {
            events.push(HistoryEvent::Visited {
                url: format!("https://site{i:07}.test/"),
                title: None,
                ts_ms: (i as u64) * day / 100 + day,
                transition: Transition::Link,
            });
        }
        let mut h = History::new();
        h.load(events);
        assert_eq!(h.index().len(), MAX_ENTRIES, "the index truncated at build");

        // `over_cap` reads the index, which `rebuild` already truncated —
        // so the retained EVENTS are what still needs pruning. Prove the
        // dropped url is gone from the retained set, which is what gets
        // written back to the file.
        h.prune_to_cap();
        let retained_urls: std::collections::HashSet<&str> = h
            .retained()
            .iter()
            .filter_map(|e| match e {
                HistoryEvent::Visited { url, .. } => Some(url.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            retained_urls.len() <= MAX_ENTRIES,
            "retained {} urls, cap is {MAX_ENTRIES}",
            retained_urls.len()
        );
        assert!(
            !retained_urls.contains("https://site0000000.test/"),
            "the worst-ranked entry survived the cap"
        );
        assert!(
            retained_urls.contains(format!("https://site{MAX_ENTRIES:07}.test/").as_str()),
            "the best-ranked entry was dropped"
        );
    }
}
