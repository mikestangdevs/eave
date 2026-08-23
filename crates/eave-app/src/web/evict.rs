//! When a page tab gives its browser back (Slice E3).
//!
//! # What this slice is answering
//! `tabs::drive` states the memory policy eave had until now: **creation
//! requires activation**, so a tab you never looked at costs nothing. C5
//! wrote that down as "a deliberately conservative floor and not a
//! resident-set policy", named the missing half — *nothing evicts a browser
//! once created* — and parked it. The ceiling that left is "tabs you have
//! actually visited this run", and eave never gave any of it back.
//!
//! **How much that actually was is measured here, and it is not the number
//! C5 recorded.** See `eave_core::thresholds::PAGE_IDLE_EVICT_AFTER` for
//! the table: ten page tabs cost 0.90 GB against a 0.32 GB floor with no
//! page resident, so ~322 MB per browser was very nearly the FLOOR being
//! attributed to the first tab. Eviction reclaims everything above that
//! floor and nothing of the floor itself.
//!
//! This is the missing half. A page tab nobody has looked at for
//! [`eave_core::thresholds::PAGE_IDLE_EVICT_AFTER`] gives its browser back;
//! **the tab itself stays**, with its url, its title and its favicon, and
//! activating it builds a browser again at the same address.
//!
//! # Why that is nearly free, and it is not this slice's doing
//! Because the back/forward stack was never in Chromium. Slice B1 put it in
//! the engine (`browser::tabs::NavStack`, durable, 50 entries, journaled),
//! and back/forward are served by `tab_go` issuing an ordinary pending
//! navigation. So destroying a browser destroys **no history**: the stack
//! lives in a different process and does not care.
//!
//! Restoring is equally free and for a second inherited reason. Re-creating
//! at the tab's current url commits that url, and C5 made a same-url commit
//! a **reload** rather than a push (`NavStack::is_refresh_of_current`) —
//! kept precisely so that refreshing five times did not leave five history
//! entries. The consequence here is one nobody designed for: a restored tab
//! re-commits where it was standing, the stack is not truncated, and even
//! the FORWARD entries survive.
//!
//! **What is genuinely lost is stated rather than papered over**: scroll
//! position, form contents, the JS heap, and any state the page held only in
//! memory. A restored tab is the same address freshly loaded, not the same
//! page put back. That is what every browser's tab discarding does, and it
//! is the honest description of this feature.
//!
//! # Where the clock is read
//! In [`tick`], on the pump, and **not** on snapshots. A snapshot-driven
//! clock would be a clock that stops: leave one tab in front of you for
//! half an hour with nothing else happening and no snapshot arrives, so the
//! tab you are looking at would go stale and evict itself out from under
//! you. The pump ticks whether or not anything changed, which is the
//! property this needs.
//!
//! # The rule this obeys, and it is the same rule
//! **A foreign event loop is lent every borrow you hold** (C7). Closing a
//! browser runs `on_before_close` synchronously, on this stack, and that
//! lands back in [`note_closed`] — which takes this module's own
//! `RefCell`s. So the scan collects victims, **drops every borrow**, and
//! only then closes. C7's discipline is usually quoted about `BROWSERS`;
//! it was never about that map, it is about any borrow held across a call
//! into CEF, and this module is a fresh chance to get it wrong.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use eave_wire::TabsSnapshot;

thread_local! {
    /// What we know about each page tab's idleness. Keyed by tab id and
    /// outliving the browser deliberately: "this tab was evicted" is a fact
    /// about the TAB, and the tab is still there.
    static SEEN: RefCell<HashMap<String, Seen>> = RefCell::new(HashMap::new());

    /// The last strip we were handed. [`tick`] needs to know which tab is
    /// active and which tabs are pages, and it runs between snapshots.
    static LAST: RefCell<Option<TabsSnapshot>> = const { RefCell::new(None) };

    /// A browser eave closed has finished closing, and the strip should be
    /// reconciled again. See [`note_closed`] for the race this closes.
    static REDRIVE: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    /// When this tab was last the active tab. Refreshed every tick while it
    /// IS active, so the tab in front of the operator never ages.
    last_active_ms: u64,
    /// When the page last STARTED an audio stream. There is no stopped-at
    /// counterpart and that is not an oversight — see
    /// [`eave_core::thresholds::PAGE_AUDIO_HOLD`].
    audio_started_ms: Option<u64>,
    /// Whether the page currently holds camera or microphone access. Unlike
    /// audio playback this one has both edges, so it is exact.
    capturing: bool,
    /// Whether eave took this tab's browser back. Distinct from "has no
    /// browser": a tab you never opened has no browser either, and the two
    /// are different facts even though they look alike on screen.
    evicted: bool,
    /// The last verdict traced for this tab, so the trace can report
    /// CHANGES rather than re-printing the same answer ten times a second.
    /// A log that repeats is a log nobody reads.
    last_verdict: Option<super::policy::Verdict>,
}

impl Seen {
    /// The facts the decision runs on. `Seen` also carries `evicted`, which
    /// is a fact about what eave DID rather than about the tab's idleness,
    /// and the policy has no business reading it.
    fn idle(&self) -> super::policy::Idle {
        super::policy::Idle {
            last_active_ms: self.last_active_ms,
            audio_started_ms: self.audio_started_ms,
            capturing: self.capturing,
        }
    }

    fn new(now: u64) -> Self {
        Self {
            last_active_ms: now,
            audio_started_ms: None,
            capturing: false,
            evicted: false,
            last_verdict: None,
        }
    }
}

// ---------------------------------------------------------------------------
// What the rest of the page engine tells us
// ---------------------------------------------------------------------------

/// The strip, as of the last reconciliation.
pub fn note_snapshot(snapshot: &TabsSnapshot) {
    LAST.with(|l| *l.borrow_mut() = Some(snapshot.clone()));
}

/// A browser exists for this tab again. Clears the evicted mark: whatever
/// this tab is now, it is resident.
pub fn note_created(tab_id: &str) {
    let now = super::host::now_ms();
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let seen = s
            .entry(tab_id.to_string())
            .or_insert_with(|| Seen::new(now));
        seen.evicted = false;
        seen.last_active_ms = now;
        // A fresh browser has played nothing and holds no devices. Carrying
        // the old browser's audio hold across would keep a tab that once
        // beeped exempt for an hour after every restore, compounding.
        seen.audio_started_ms = None;
        seen.capturing = false;
    });
}

/// A browser has finished closing.
///
/// # The race this closes
/// `close_browser` is asynchronous even when forced: it asks, and
/// `on_before_close` removes the map entry on a later pump. In between,
/// `has_browser` still answers true. So an operator who clicks an
/// evicted-this-instant tab inside that window gets a snapshot whose
/// `drive_tab` sees a live browser, does nothing — and then the close
/// completes and the tab has no browser and no pending reconciliation. The
/// tab would sit blank until something unrelated moved the strip.
///
/// Setting a flag here rather than re-driving here is the C7 rule again:
/// this runs INSIDE CEF's synchronous callback, and creating a browser from
/// inside `on_before_close` is exactly the sort of re-entrancy that abort
/// was made of. [`tick`] acts on it one pump later, on a clean stack.
pub fn note_closed(tab_id: &str) {
    let was_eviction = SEEN.with(|s| s.borrow().get(tab_id).is_some_and(|seen| seen.evicted));
    if was_eviction {
        REDRIVE.with(|r| r.set(true));
    }
}

/// A page began playing audio. Called from the audio handler, which
/// deliberately declines to capture the stream.
pub fn note_audio_started(tab_id: &str) {
    let now = super::host::now_ms();
    SEEN.with(|s| {
        s.borrow_mut()
            .entry(tab_id.to_string())
            .or_insert_with(|| Seen::new(now))
            .audio_started_ms = Some(now);
    });
}

/// A page took or released camera/microphone access.
pub fn note_media_access(tab_id: &str, capturing: bool) {
    let now = super::host::now_ms();
    SEEN.with(|s| {
        s.borrow_mut()
            .entry(tab_id.to_string())
            .or_insert_with(|| Seen::new(now))
            .capturing = capturing;
    });
}

/// A tab left the strip. Its idleness is no longer a fact about anything.
pub fn forget(tab_id: &str) {
    SEEN.with(|s| {
        s.borrow_mut().remove(tab_id);
    });
}

// ---------------------------------------------------------------------------
// What the shell asks us
// ---------------------------------------------------------------------------

/// Whether none of this module's cells are currently borrowed.
///
/// **The C7 rule, made checkable.** C7's finding was that a foreign event
/// loop is lent every borrow you hold, and its fix was a discipline: drop
/// the borrow before calling into CEF. A discipline is only as good as the
/// next person's memory of it, and this module added two more cells for the
/// next person to forget about. `close_browser_for` asserts on this, so a
/// close reached with a borrow open fails loudly in every debug and test
/// build instead of aborting in front of an operator months later.
pub(super) fn borrows_clear() -> bool {
    SEEN.with(|s| s.try_borrow_mut().is_ok()) && LAST.with(|l| l.try_borrow_mut().is_ok())
}

/// Whether eave took this tab's browser back to reclaim memory.
///
/// The narrow fact. For the strip, `tabs::has_browser` — "is this page
/// resident at all" — is usually the one worth drawing, because a
/// never-visited tab and an evicted tab are the same thing to the person
/// looking at them: a tab whose page will load when they click it.
pub fn was_evicted(tab_id: &str) -> bool {
    SEEN.with(|s| s.borrow().get(tab_id).is_some_and(|seen| seen.evicted))
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// One turn of the eviction clock. Called from the pump, on the main
/// thread, with no borrow held.
pub fn tick() {
    if !super::host::running() {
        return;
    }

    // A close we asked for has completed. Reconcile again, in case the
    // operator has gone back to that tab in the meantime. Outside every
    // borrow, deliberately: `drive` creates browsers.
    if REDRIVE.with(|r| r.replace(false)) {
        let snapshot = LAST.with(|l| l.borrow().clone());
        if let Some(snapshot) = snapshot {
            super::tabs::drive(&snapshot);
        }
    }

    let now = super::host::now_ms();
    let clock = super::policy::Clock::from_thresholds();

    // The scan holds borrows; the closing does not. See the module docs —
    // this is C7's rule applied to this module's own cells, and getting it
    // wrong here would abort exactly the way `BROWSERS` did.
    let victims = LAST.with(|l| {
        let guard = l.borrow();
        let Some(snapshot) = guard.as_ref() else {
            return Vec::new();
        };
        let active = snapshot.active.as_deref();
        SEEN.with(|s| {
            let mut s = s.borrow_mut();
            let mut victims: Vec<String> = Vec::new();
            for tab in snapshot.tabs.iter() {
                let is_active = Some(tab.id.as_str()) == active;
                let seen = s.entry(tab.id.clone()).or_insert_with(|| Seen::new(now));
                // The tab being looked at never ages, and this is the whole
                // reason the clock is read on the pump rather than on
                // snapshots: a stamp that only moved when the strip changed
                // would go stale under a tab sitting in front of someone.
                if is_active {
                    seen.last_active_ms = now;
                }
                let verdict = super::policy::verdict(
                    tab,
                    &seen.idle(),
                    is_active,
                    super::tabs::has_browser(&tab.id),
                    now,
                    clock,
                );
                // Why a tab SURVIVED is the interesting half — an
                // eviction announces itself by the memory going away, and a
                // tab that stubbornly will not evict is the thing somebody
                // will be trying to explain. Printed on change only.
                if traced() && seen.last_verdict != Some(verdict) {
                    eprintln!("[web] tab {} · {}", tab.id, verdict.why());
                }
                seen.last_verdict = Some(verdict);
                if verdict.evicts() {
                    seen.evicted = true;
                    victims.push(tab.id.clone());
                }
            }
            victims
        })
    });

    for id in victims {
        if traced() {
            eprintln!("[web] evicting idle tab {id}");
        }
        super::tabs::close_browser_for(&id);
    }
}

fn traced() -> bool {
    std::env::var("EAVE_WEB_TRACE").is_ok()
}
