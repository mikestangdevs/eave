//! A page whose renderer has stopped answering — 06-browser **C6**.
//!
//! # What was missing, and why nothing said so
//! Chromium runs a hang monitor: when a renderer has not processed an
//! input event for at least fifteen seconds it decides the renderer is
//! unresponsive. eave drew nothing for that, ever. A page frozen by a
//! runaway script was indistinguishable from a slow one — no spinner ends,
//! no click lands, the tab looks fine — and there was no way out short of
//! quitting eave, because a hung renderer is also what makes closing its
//! tab take a while.
//!
//! # eave cannot be TOLD this, and the reason is eave's own doing
//! CEF has a callback for it, `OnRenderProcessUnresponsive`, and it is
//! unreachable here. `WebContentsImpl::RendererUnresponsive` opens with
//! `ShouldIgnoreUnresponsiveRenderer()`, whose last line is
//!
//! ```text
//! // Ignore unresponsive renderers if the debugger is attached to them
//! // since the unresponsiveness might be a result of the renderer
//! // sitting on a breakpoint.
//! return DevToolsAgentHost::IsDebuggerAttached(this);
//! ```
//!
//! **eave attaches a DevTools client to every page it opens**, and always
//! has: E9's channel is what carries the notification shim, the audible
//! shim, the selection reads and BR6's scroll restore. So every eave page
//! looks to Chromium like a page with a debugger on it, and every hang
//! report about one is dropped one statement before CEF could see it.
//!
//! That was measured, not reasoned — and confirmed on a control, because
//! a suppression that explains everything is exactly the kind of story
//! that turns out to be wrong. The handler never fires in eave;
//! `IsRenderProcessUnresponsive` flips true in the same run; Chrome shows
//! "Page Unresponsive" for the same page; **and Chrome with DevTools open
//! does not.** See `docs/reports/stage-c6-unresponsive-pages.md`.
//!
//! So the producer is a poll — [`super::tabs::poll_unresponsive`] — and it
//! reads the *same fact* rather than an approximation of it:
//! `is_render_process_unresponsive` returns
//! `RenderWidgetHostImpl::is_unresponsive_`, set on the first line of
//! `RendererIsUnresponsive`, one statement earlier than the hop that gets
//! suppressed. Same threshold, same decision, same instant.
//!
//! # It is a NOTICE, not an ask
//! [`super::dialogs`] keeps one rule: **a held callback is a stopped
//! page** — every entry in its map is a renderer waiting on a person.
//! Nothing here works like that. Chromium is not waiting on the person, it
//! is waiting on the *page*, and it will go on waiting whatever anybody
//! does. So the surface must not demand an answer, and *doing nothing is a
//! legitimate outcome the operator chooses by ignoring it*. Waiting needs
//! no button; it is what happens when you read the notice and go back to
//! what you were doing.
//!
//! # It withdraws itself — unlike the notice it is modelled on
//! `Shell::attention_notice` draws an unresponsive *session* in this same
//! danger idiom, and its rule is that it **never withdraws on its own**.
//! That rule is right there and wrong here, and the axis is evidence.
//!
//! A session is called unresponsive by inference — zero CPU across N
//! samples, no tool running, nothing in flight — so eave is reading tea
//! leaves and must not also decide, on its own authority, that the reading
//! has expired. A page is called unresponsive by **Chromium's own
//! decision**, and the same flag answers the other way the moment the
//! renderer comes back. Leaving the panel up after that would be eave
//! asserting something it has been told is untrue, over a page the
//! operator can already see working.
//!
//! # The clock starts at fifteen seconds, not at zero
//! Chromium's threshold is `input::kHungRendererDelay`, fifteen seconds on
//! desktop, plus a one-second ping before it commits to the answer. So the
//! hang is already that old when the flag flips. Dating it from `now`
//! would understate every hang by the whole of that threshold — the notice
//! would say `0:02` about a page that had been dead for seventeen seconds.
//! [`AFTER_MS`] is the minimum Chromium waits, so backdating by it can only
//! ever *understate*, which is the safe direction for a number somebody is
//! about to act on.
//!
//! # Ungated, for [`super::ask`]'s and [`super::scroll`]'s reason
//! Everything here is a map and a subtraction, so the default suite drives
//! all of it. The CEF half is the poll and the close, both in `tabs.rs`.

use std::cell::RefCell;
use std::collections::HashMap;

/// How long Chromium waits before it decides, in milliseconds.
///
/// Not a threshold of eave's: `input::kHungRendererDelay` is fifteen
/// seconds on desktop, the hang monitor implements it, and eave cannot
/// change it short of the `--disable-hang-monitor` switch it deliberately
/// does not pass. It is here because it is the age of a hang at the
/// instant eave first sees it — see the module doc on why the clock does
/// not start at zero.
pub const AFTER_MS: u64 = 15_000;

thread_local! {
    /// When each hung tab's renderer is believed to have stopped
    /// answering, in wall-clock milliseconds.
    ///
    /// `thread_local!` for [`super::popups`]'s reason: `#[gpui::test]`
    /// runs each test on its own thread inside one process, and a
    /// process-wide map would let one test's hung page appear in
    /// another's window.
    static HUNG: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
}

/// Chromium says this tab's renderer has stopped answering.
///
/// Returns whether this is the *first* sight of this hang. The poll asks
/// on every pump and a hang stays true across all of them, so the start
/// time is written once and every later report leaves it alone — a notice
/// whose elapsed restarted on each look would report a page that had been
/// dead for five minutes as a fraction of a second gone.
pub fn note_unresponsive(tab_id: &str, now_ms: u64) -> bool {
    HUNG.with(|h| {
        let mut h = h.borrow_mut();
        if h.contains_key(tab_id) {
            return false;
        }
        h.insert(tab_id.to_string(), now_ms.saturating_sub(AFTER_MS));
        true
    })
}

/// Chromium says this tab's renderer is answering again.
///
/// Returns whether anything was actually cleared, so the caller can leave
/// the paint alone when nothing changed — which is every pump for every
/// healthy page, and therefore nearly all of them.
pub fn note_responsive(tab_id: &str) -> bool {
    HUNG.with(|h| h.borrow_mut().remove(tab_id).is_some())
}

/// The tab is going away, or its renderer already has.
pub fn forget(tab_id: &str) {
    HUNG.with(|h| {
        h.borrow_mut().remove(tab_id);
    });
}

/// How long this tab has been unresponsive, or `None` if it is not.
///
/// Saturating, so a clock that goes backwards between the two readings
/// reports zero rather than an enormous number — the wall clock can be
/// stepped by the system at any moment and this is the only arithmetic
/// here that a person reads.
pub fn hung_for(tab_id: &str, now_ms: u64) -> Option<u64> {
    HUNG.with(|h| {
        h.borrow()
            .get(tab_id)
            .map(|since| now_ms.saturating_sub(*since))
    })
}

/// Is any page hung at all?
///
/// The elapsed on the notice has to move, and a hung page is the one thing
/// on screen that cannot move it: no script runs, no frame arrives, no CEF
/// callback fires, so nothing in the ordinary paint path is coming. The
/// shell's one-second heartbeat is the only clock it has, and this is what
/// arms it — cheap enough to ask every second, and false every second that
/// nothing is wrong.
pub fn any() -> bool {
    HUNG.with(|h| !h.borrow().is_empty())
}

/// What the notice says under its heading.
///
/// Chromium's threshold stated as a fact about Chromium, because that is
/// whose measurement it is — the same reason `attention_notice` prefixes
/// the session case with `evidence:` rather than asserting the session is
/// dead.
pub const EVIDENCE: &str = "no answer from this page for 15 seconds or more · a script \
                            running away with it cannot be stopped from outside";

/// The note UNDER the button — `kill_affordance` puts its note beside
/// one, and that works there because it is four words; this is a sentence
/// and in a row it ran out past the panel's own edge.
///
/// It says what is NOT happening, because the sentence a person needs
/// before they walk away from a machine is that nothing is being torn down
/// behind their back — and it says what the button costs, because
/// "reload" on a page somebody has typed into is not free.
pub const NOTHING_AUTOMATIC: &str =
    "nothing reloads on its own · reloading discards what the page was holding";

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole of the backdating rule, and the reason it exists.
    #[test]
    fn a_hang_is_already_chromiums_threshold_old_when_we_hear_about_it() {
        forget("t1");
        assert!(note_unresponsive("t1", 100_000));
        assert_eq!(
            hung_for("t1", 100_000),
            Some(AFTER_MS),
            "a hang reported at 100s started at 85s, not at 100s"
        );
        assert_eq!(hung_for("t1", 105_000), Some(AFTER_MS + 5_000));
    }

    #[test]
    fn the_same_hang_reported_again_does_not_restart_its_clock() {
        forget("t2");
        assert!(note_unresponsive("t2", 100_000));
        assert!(
            !note_unresponsive("t2", 160_000),
            "a repeat is the same hang continuing, not a new one"
        );
        assert_eq!(
            hung_for("t2", 160_000),
            Some(AFTER_MS + 60_000),
            "the elapsed must count from the FIRST report"
        );
    }

    #[test]
    fn a_page_that_recovers_stops_being_hung_and_says_it_changed() {
        forget("t3");
        note_unresponsive("t3", 1_000);
        assert!(note_responsive("t3"), "a cleared hang is a change to paint");
        assert_eq!(hung_for("t3", 9_000), None);
    }

    /// The poll asks about every browser on every pump, so the healthy
    /// answer arrives constantly for tabs that were never filed. The
    /// honest reply is that nothing happened — and the caller leans on it,
    /// because a repaint per pump per page is the cost of getting this
    /// wrong.
    #[test]
    fn recovering_a_page_that_was_never_hung_changes_nothing() {
        forget("t4");
        assert!(!note_responsive("t4"));
    }

    #[test]
    fn a_closed_tab_is_not_still_hung() {
        forget("t5");
        note_unresponsive("t5", 1_000);
        forget("t5");
        assert_eq!(hung_for("t5", 2_000), None);
    }

    #[test]
    fn one_tab_hanging_says_nothing_about_another() {
        forget("a");
        forget("b");
        note_unresponsive("a", 50_000);
        assert_eq!(hung_for("b", 50_000), None);
        assert!(hung_for("a", 50_000).is_some());
    }

    #[test]
    fn nothing_hung_arms_no_heartbeat() {
        forget("h1");
        assert!(!any(), "a tab was left hung by another test in this thread");
        note_unresponsive("h1", 1_000);
        assert!(any());
        note_responsive("h1");
        assert!(!any(), "the heartbeat must stand down when the last hang clears");
    }

    /// A wall clock that steps backwards must not produce a headline
    /// number in the billions.
    #[test]
    fn a_clock_that_goes_backwards_reports_zero() {
        forget("t6");
        note_unresponsive("t6", 100_000);
        assert_eq!(hung_for("t6", 0), Some(0));
    }
}
