//! The eviction decision, as a function of facts (Slice E3).
//!
//! # Why this is a separate file from `evict`
//! Two reasons, and the second is the load-bearing one.
//!
//! 1. Everything in `evict` touches CEF, so it can only say *what happened*
//!    to a browser. What ought to happen to it is a question about a tab,
//!    some timestamps and two thresholds — no Chromium in it anywhere.
//! 2. **`evict` is behind `--features web`, and the default test run is
//!    not.** A policy that only compiled with the feature on would be a
//!    policy the 894-test suite never executed, and "the tests pass" would
//!    have said nothing about the rule that decides when eave throws a
//!    page away. This module is compiled in BOTH feature states and its
//!    tests run in the ordinary `cargo test`.
//!
//! The reasons a tab is kept are an enum rather than a bool for the same
//! reason `LoadState::Failed` carries the engine's words: a trace line that
//! says *why* a tab survived is a debugging instrument, and one that says
//! `false` is not.

use eave_wire::{TabContent, TabInfo};

/// What eave knows about one page tab's idleness. Supplied by `evict` from
/// CEF callbacks; meaningless to anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Idle {
    /// When this tab was last the active tab.
    pub last_active_ms: u64,
    /// When the page last STARTED an audio stream. There is no stopped-at
    /// counterpart — see [`eave_core::thresholds::PAGE_AUDIO_HOLD`].
    pub audio_started_ms: Option<u64>,
    /// Whether the page holds camera or microphone access right now.
    pub capturing: bool,
}

/// The two clocks the decision runs on, already in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub idle_after_ms: u64,
    pub audio_hold_ms: u64,
}

impl Clock {
    /// The clocks as configured, env overrides included.
    pub fn from_thresholds() -> Self {
        Self {
            idle_after_ms: eave_core::thresholds::page_idle_evict_after().as_millis() as u64,
            audio_hold_ms: eave_core::thresholds::page_audio_hold().as_millis() as u64,
        }
    }
}

/// What should happen to one tab's browser, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Take the browser back.
    Evict,
    /// Not a page. Session tabs have no browser to take.
    NotAPage,
    /// The tab the operator is looking at. Never evicted, whatever the
    /// clock says — and the clock should not be able to say anything,
    /// because an active tab's stamp is refreshed on every tick.
    Active,
    /// No browser to take: never visited, or already evicted.
    NotResident,
    /// Pinned (Slice D3's field). The tab most likely to be returned to is
    /// the last one that should cost a reload.
    Pinned,
    /// Looked at recently enough.
    NotIdleYet,
    /// Holding a camera or a microphone — the operator is in a call, and
    /// the tab being unwatched is the normal case rather than a signal.
    Capturing,
    /// Started an audio stream inside the hold window.
    PlayingAudio,
}

impl Verdict {
    pub fn evicts(self) -> bool {
        matches!(self, Verdict::Evict)
    }

    /// A word for the trace line.
    pub fn why(self) -> &'static str {
        match self {
            Verdict::Evict => "evict",
            Verdict::NotAPage => "not a page",
            Verdict::Active => "active",
            Verdict::NotResident => "not resident",
            Verdict::Pinned => "pinned",
            Verdict::NotIdleYet => "not idle yet",
            Verdict::Capturing => "capturing",
            Verdict::PlayingAudio => "playing audio",
        }
    }
}

/// Should this tab give its browser back?
///
/// The order of the tests is not arbitrary: the cheap structural answers
/// come first so that the interesting ones (`NotIdleYet`, `PlayingAudio`)
/// are only ever reported about a tab they could actually apply to. A trace
/// that said "not idle yet" about a session tab would be noise.
pub fn verdict(
    tab: &TabInfo,
    idle: &Idle,
    is_active: bool,
    resident: bool,
    now_ms: u64,
    clock: Clock,
) -> Verdict {
    if tab.content != TabContent::Page {
        return Verdict::NotAPage;
    }
    if is_active {
        return Verdict::Active;
    }
    if !resident {
        return Verdict::NotResident;
    }
    if tab.pinned {
        return Verdict::Pinned;
    }
    // `saturating_sub` and not `-`: the clock is wall time and wall time
    // goes backwards (ntp, sleep, a manual change). A tab must not become
    // instantly evictable because the machine's idea of now moved.
    if now_ms.saturating_sub(idle.last_active_ms) < clock.idle_after_ms {
        return Verdict::NotIdleYet;
    }
    if idle.capturing {
        return Verdict::Capturing;
    }
    if idle
        .audio_started_ms
        .is_some_and(|at| now_ms.saturating_sub(at) < clock.audio_hold_ms)
    {
        return Verdict::PlayingAudio;
    }
    Verdict::Evict
}

// ---------------------------------------------------------------------------
// The other half of the same question (the preload rule)
// ---------------------------------------------------------------------------

/// Where one page tab's browser stands right now.
///
/// `resident` and `evicted` are **not** opposites, and the distinction is
/// the whole reason preloading and eviction can coexist. A tab that has
/// never been opened is not resident and not evicted — warm it. A tab eave
/// took the browser back from is not resident *and* evicted — leave it
/// alone, or the next tick hands back exactly what the last one reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Residency {
    /// This tab has a browser.
    pub resident: bool,
    /// eave took this tab's browser back for idleness.
    pub evicted: bool,
    /// When this tab was last the active tab.
    pub last_active_ms: u64,
}

/// Which tabs should be given a browser *before* anyone asks for one, most
/// recently used first.
///
/// # The rule
/// A page tab is warmed when it is not resident, has not been evicted, and
/// there is room under `cap`. Those are the only three reasons a tab is
/// missing from this list, and they are worth knowing apart when someone
/// reports "it still booted": it was already warm, eave had deliberately
/// reclaimed it, or tabs used more recently spent the budget.
///
/// # Why the cap counts residents rather than warmings
/// `cap` is a ceiling on *resident page browsers*, not a number of tabs to
/// warm per pass. Counting only new warmings would let eight already-open
/// tabs sit beside eight freshly warmed ones.
///
/// # Why the order needs a tiebreak
/// At launch every tab is first seen in the same instant, so
/// `last_active_ms` is identical across the strip and sorting on it alone
/// would leave the choice to whatever order the map happened to yield.
/// Strip position breaks the tie, which makes the answer both deterministic
/// and defensible: leftmost first when nothing else distinguishes them.
pub fn preload_queue(
    tabs: &[TabInfo],
    residency: impl Fn(&str) -> Residency,
    cap: usize,
) -> Vec<String> {
    let pages: Vec<(usize, &TabInfo, Residency)> = tabs
        .iter()
        .enumerate()
        .filter(|(_, t)| t.content == TabContent::Page)
        .map(|(i, t)| (i, t, residency(&t.id)))
        .collect();

    let budget = cap.saturating_sub(pages.iter().filter(|(_, _, r)| r.resident).count());
    if budget == 0 {
        return Vec::new();
    }

    let mut candidates: Vec<(usize, &TabInfo, Residency)> = pages
        .into_iter()
        .filter(|(_, _, r)| !r.resident && !r.evicted)
        .collect();
    candidates.sort_by(|a, b| {
        b.2.last_active_ms
            .cmp(&a.2.last_active_ms)
            .then(a.0.cmp(&b.0))
    });

    candidates
        .into_iter()
        .take(budget)
        .map(|(_, t, _)| t.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60 * 1000;

    fn clock() -> Clock {
        Clock {
            idle_after_ms: 20 * MINUTE,
            audio_hold_ms: 60 * MINUTE,
        }
    }

    fn page(id: &str) -> TabInfo {
        TabInfo {
            id: id.to_string(),
            group: None,
            pinned: false,
            content: TabContent::Page,
            url: Some("https://example.com/".into()),
            title: Some("Example".into()),
            favicon_url: None,
            load: Default::default(),
            pending: None,
            can_go_back: false,
            can_go_forward: false,
            history_len: 1,
            opener: None,
            created_ts_ms: 0,
        }
    }

    /// Looked at once, then left alone past the threshold.
    fn idle_since(ms: u64) -> Idle {
        Idle {
            last_active_ms: ms,
            ..Default::default()
        }
    }

    #[test]
    fn a_page_left_alone_past_the_threshold_gives_its_browser_back() {
        let now = 100 * MINUTE;
        let v = verdict(
            &page("t"),
            &idle_since(now - 21 * MINUTE),
            false,
            true,
            now,
            clock(),
        );
        assert_eq!(v, Verdict::Evict);
    }

    /// The boundary, stated: strictly less than the threshold is kept, so
    /// the threshold itself evicts. One test rather than a comment because
    /// which side of the boundary the constant sits on is exactly the kind
    /// of thing a later refactor flips silently.
    #[test]
    fn the_threshold_itself_evicts_and_a_millisecond_under_it_does_not() {
        let now = 100 * MINUTE;
        let at = verdict(
            &page("t"),
            &idle_since(now - 20 * MINUTE),
            false,
            true,
            now,
            clock(),
        );
        let under = verdict(
            &page("t"),
            &idle_since(now - 20 * MINUTE + 1),
            false,
            true,
            now,
            clock(),
        );
        assert_eq!(at, Verdict::Evict);
        assert_eq!(under, Verdict::NotIdleYet);
    }

    /// The tab in front of the operator is never a candidate, and this is
    /// asserted with an idle stamp far past the threshold on purpose: the
    /// active check must not depend on the stamp being refreshed.
    #[test]
    fn the_tab_being_looked_at_is_never_evicted_however_stale_its_stamp() {
        let now = 100 * MINUTE;
        let v = verdict(&page("t"), &idle_since(0), true, true, now, clock());
        assert_eq!(v, Verdict::Active);
    }

    #[test]
    fn a_pinned_tab_is_exempt() {
        let now = 100 * MINUTE;
        let mut tab = page("t");
        tab.pinned = true;
        let v = verdict(&tab, &idle_since(0), false, true, now, clock());
        assert_eq!(v, Verdict::Pinned);
    }

    /// A session tab has no browser and no business in this loop.
    #[test]
    fn a_session_tab_is_not_a_candidate() {
        let now = 100 * MINUTE;
        let mut tab = page("t");
        tab.content = TabContent::Session {
            session_id: "s".into(),
        };
        let v = verdict(&tab, &idle_since(0), false, true, now, clock());
        assert_eq!(v, Verdict::NotAPage);
    }

    /// Evicting twice would ask CEF to close a browser that is already
    /// closing, and the second close would find nothing.
    #[test]
    fn a_tab_with_no_browser_is_not_evicted_again() {
        let now = 100 * MINUTE;
        let v = verdict(&page("t"), &idle_since(0), false, false, now, clock());
        assert_eq!(v, Verdict::NotResident);
    }

    #[test]
    fn a_tab_holding_a_camera_or_microphone_is_exempt() {
        let now = 100 * MINUTE;
        let idle = Idle {
            last_active_ms: 0,
            capturing: true,
            ..Default::default()
        };
        assert_eq!(
            verdict(&page("t"), &idle, false, true, now, clock()),
            Verdict::Capturing
        );
    }

    /// Audio inside the hold window keeps the tab.
    #[test]
    fn a_tab_that_started_audio_recently_is_exempt() {
        let now = 100 * MINUTE;
        let idle = Idle {
            last_active_ms: 0,
            audio_started_ms: Some(now - 30 * MINUTE),
            ..Default::default()
        };
        assert_eq!(
            verdict(&page("t"), &idle, false, true, now, clock()),
            Verdict::PlayingAudio
        );
    }

    /// **The stated failure mode, asserted rather than described.** CEF
    /// gives no stop edge for an audio stream we decline to capture, so the
    /// hold expires on a clock rather than on the audio ending — and a
    /// single continuous stream longer than the hold IS evicted, mid-play.
    /// If this test ever starts failing because someone made the hold
    /// unbounded, the memory ceiling has quietly come back.
    #[test]
    fn audio_older_than_the_hold_no_longer_protects_the_tab() {
        let now = 100 * MINUTE;
        let idle = Idle {
            last_active_ms: 0,
            audio_started_ms: Some(now - 61 * MINUTE),
            ..Default::default()
        };
        assert_eq!(
            verdict(&page("t"), &idle, false, true, now, clock()),
            Verdict::Evict
        );
    }

    /// Wall time goes backwards — ntp, sleep, someone changing the clock.
    /// A tab must not become instantly evictable because `now` moved behind
    /// the stamp; `saturating_sub` is what makes that true and this is the
    /// test that says so.
    #[test]
    fn a_clock_that_moved_backwards_does_not_evict_everything() {
        let now = 10 * MINUTE;
        let idle = idle_since(90 * MINUTE);
        assert_eq!(
            verdict(&page("t"), &idle, false, true, now, clock()),
            Verdict::NotIdleYet
        );
    }

    /// Ten tabs, one active, all idle: nine go and the tenth stays. This is
    /// the slice's headline claim in the smallest form that can be checked
    /// without Chromium.
    #[test]
    fn ten_idle_tabs_give_back_nine_browsers() {
        let now = 100 * MINUTE;
        let tabs: Vec<TabInfo> = (0..10).map(|i| page(&format!("t{i}"))).collect();
        let evicted = tabs
            .iter()
            .filter(|t| verdict(t, &idle_since(0), t.id == "t0", true, now, clock()).evicts())
            .count();
        assert_eq!(evicted, 9);
    }
    // ── preload ──────────────────────────────────────────────────────────

    fn session(id: &str) -> TabInfo {
        TabInfo {
            content: TabContent::Session {
                session_id: "s".into(),
            },
            ..page(id)
        }
    }

    /// Cold, never-opened page tabs are exactly what the old rule left to
    /// boot on first sight. They are the reason this function exists.
    fn cold() -> Residency {
        Residency::default()
    }

    #[test]
    fn a_never_opened_page_tab_is_warmed_before_anyone_asks() {
        let tabs = vec![page("a"), page("b")];
        assert_eq!(preload_queue(&tabs, |_| cold(), 8), vec!["a", "b"]);
    }

    /// The defect this whole change is about: opening a tab, or dragging one
    /// into a split, must not be the thing that starts Chromium.
    #[test]
    fn nothing_is_left_cold_when_the_strip_fits_under_the_cap() {
        let tabs: Vec<TabInfo> = (0..8).map(|i| page(&format!("t{i}"))).collect();
        assert_eq!(preload_queue(&tabs, |_| cold(), 8).len(), 8);
    }

    #[test]
    fn session_tabs_are_not_warmed_and_do_not_spend_the_budget() {
        let tabs = vec![session("s0"), session("s1"), page("p")];
        assert_eq!(preload_queue(&tabs, |_| cold(), 2), vec!["p"]);
    }

    /// The interlock with E3. Handing a browser straight back to a tab eave
    /// just reclaimed would make eviction a no-op with extra steps, and the
    /// memory would never actually be returned.
    #[test]
    fn an_evicted_tab_is_left_alone_rather_than_immediately_rewarmed() {
        let tabs = vec![page("kept"), page("reclaimed")];
        let q = preload_queue(
            &tabs,
            |id| Residency {
                evicted: id == "reclaimed",
                ..cold()
            },
            8,
        );
        assert_eq!(q, vec!["kept"]);
    }

    #[test]
    fn the_cap_counts_browsers_already_resident_not_just_new_ones() {
        let tabs: Vec<TabInfo> = (0..6).map(|i| page(&format!("t{i}"))).collect();
        // Three are already up, so a cap of four leaves room for exactly one.
        let q = preload_queue(
            &tabs,
            |id| Residency {
                resident: matches!(id, "t0" | "t1" | "t2"),
                ..cold()
            },
            4,
        );
        assert_eq!(q, vec!["t3"]);
    }

    #[test]
    fn a_full_cap_warms_nothing_further() {
        let tabs: Vec<TabInfo> = (0..10).map(|i| page(&format!("t{i}"))).collect();
        let q = preload_queue(
            &tabs,
            |id| Residency {
                resident: id < "t8",
                ..cold()
            },
            8,
        );
        assert!(q.is_empty(), "expected no warmings, got {q:?}");
    }

    /// Most recently used first, because the budget is spent on the tabs
    /// most likely to be reached for next.
    #[test]
    fn the_budget_goes_to_the_most_recently_used_tabs() {
        let tabs = vec![page("old"), page("newest"), page("middle")];
        let q = preload_queue(
            &tabs,
            |id| Residency {
                last_active_ms: match id {
                    "newest" => 300,
                    "middle" => 200,
                    _ => 100,
                },
                ..cold()
            },
            2,
        );
        assert_eq!(q, vec!["newest", "middle"]);
    }

    /// At launch every tab is first seen in the same instant, so the stamps
    /// tie and strip order has to decide. Without the tiebreak this test is
    /// flaky rather than wrong, which is the worse failure.
    #[test]
    fn tabs_seen_in_the_same_instant_fall_back_to_strip_order() {
        let tabs: Vec<TabInfo> = (0..5).map(|i| page(&format!("t{i}"))).collect();
        let q = preload_queue(
            &tabs,
            |_| Residency {
                last_active_ms: 1_000,
                ..cold()
            },
            3,
        );
        assert_eq!(q, vec!["t0", "t1", "t2"]);
    }

    /// `EAVE_THRESH_PAGE_PRELOAD_MAX=0` has to restore the old behaviour
    /// exactly, so the change is reversible at runtime without a build.
    #[test]
    fn a_cap_of_zero_restores_create_on_activation() {
        let tabs: Vec<TabInfo> = (0..4).map(|i| page(&format!("t{i}"))).collect();
        assert!(preload_queue(&tabs, |_| cold(), 0).is_empty());
    }
}
