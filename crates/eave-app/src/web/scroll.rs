//! Where the page was scrolled, and putting it back when eave takes the
//! browser away — 06-browser A7, and the half of C7 that bites daily.
//!
//! # The gap this closes, in the words of the module that opened it
//! `evict`'s own doc states what a discarded tab loses: *"scroll position,
//! form contents, the JS heap … A restored tab is the same address freshly
//! loaded, not the same page put back. That is what every browser's tab
//! discarding does, and it is the honest description of this feature."*
//! It was honest and it was also the thing people notice: you come back to
//! a long article you were halfway down and you are at the top of it. This
//! puts the scroll back. It does **not** put back form contents or the JS
//! heap, and that sentence stays true of both.
//!
//! # The invariant: eave restores only what eave took away
//! **A restore is armed at eviction and nowhere else.** Not on every load,
//! not on reloads, not on back and forward. Chromium owns the scroll
//! position of an ordinary navigation and does it properly, from the
//! navigation entry's page state — B1 walks its history with `go_back`
//! precisely so that keeps working. A second opinion applied on top would
//! fight it, and would lose in the one case it could not see: the same url
//! visited twice at two different scroll positions, where this module
//! remembers one offset and Chromium remembers both.
//!
//! So the rule is narrow enough to be obviously right: eave destroyed a
//! browser to reclaim memory, eave re-creates it at the same address, and
//! eave is responsible for making that look like nothing happened. Every
//! other navigation is Chromium's.
//!
//! # Keyed by the url it was taken at, and consumed once
//! An armed restore names the address it belongs to. If the operator
//! activates an evicted tab and the tab has since been pointed somewhere
//! else, the url does not match and nothing is put back — an offset from a
//! different page is not a partial answer, it is a wrong one. And `take`
//! removes it, so a reload a minute later does not jump the page again.
//!
//! # Why the script retries, and why it stops
//! `window.scrollTo` at load-end can land short: `load` fires with the
//! document parsed, but a long page is still growing as images arrive and
//! the scroll clamps to whatever height exists at that instant. So the
//! injected script re-asks over a bounded number of animation frames and
//! **stops as soon as it arrives**, which is the same shape as the probe
//! click waiting for real pixels rather than for `on_load_end` — a page
//! that has painted is a page with boxes in it. It is bounded rather than
//! conditional because the honest failure is a page that genuinely got
//! shorter, and retrying that for ever is a page that can never be
//! scrolled by hand.
//!
//! # The units, and why eave does no arithmetic on them
//! **Chromium reports the offset in PHYSICAL pixels and `window.scrollTo`
//! takes CSS pixels**, which the walk found the hard way: a page sitting at
//! `window.scrollY == 1500` was recorded as `3000` and restored twice as
//! far down as it had been. The tests could not have seen it — they hand a
//! number in and read the same number out, which is exactly right for what
//! they pin and blind to what the number *means*.
//!
//! The conversion is done **in the injected script, by the renderer**, and
//! not here. The factor is not the display's scale alone: it is the
//! display scale times the page's zoom, and `window.devicePixelRatio` is
//! that product, maintained by the only process that knows both. Dividing
//! by `osr::view().scale` on this side would be right at 100% zoom and
//! wrong at every other setting, in a way no test and no unzoomed walk
//! would ever show. So the numbers cross unconverted and the page divides.
//!
//! # No CEF in here
//! Only the two call sites touch Chromium (`osr` reports the offset,
//! `tabs` injects at load-end, `evict` arms). This module is arithmetic
//! and a map, so it is ungated and the default suite drives all of it —
//! the same split `cdp` and `inspector` already use.

use std::cell::RefCell;
use std::collections::HashMap;

/// How many animation frames the restore script will re-ask for.
///
/// Ten is about 160 ms at 60 Hz: long enough for the images above the fold
/// on a slow page to arrive and give the document its height, short enough
/// that a page which really is shorter now stops fighting the operator
/// almost immediately.
const RESTORE_FRAMES: u32 = 10;

/// Below this many pixels, an armed restore is not worth arming.
///
/// A tab sitting at the top is the overwhelming majority of tabs, and the
/// top is where a freshly loaded page already is — so arming there would
/// inject a script into most revivals to accomplish nothing. One pixel is
/// the threshold rather than zero because Chromium reports fractional
/// offsets and a rounding artefact is not a scroll position.
const WORTH_RESTORING: f64 = 1.0;

thread_local! {
    /// Where each tab is scrolled right now, as Chromium last said.
    static AT: RefCell<HashMap<String, (f64, f64)>> = RefCell::new(HashMap::new());
    /// What to put back when an evicted tab's browser returns.
    static PENDING: RefCell<HashMap<String, Pending>> = RefCell::new(HashMap::new());
}

/// An armed restore: an offset, and the address it belongs to.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    url: String,
    x: f64,
    y: f64,
}

/// Chromium moved this tab's scroll offset.
///
/// **Nothing asks for a frame here.** A scroll has already repainted the
/// page by definition, unlike the selection and the context menu, which
/// change chrome-visible state on a still page and must say so.
pub fn note(tab_id: &str, x: f64, y: f64) {
    AT.with(|a| {
        a.borrow_mut().insert(tab_id.to_string(), (x, y));
    });
}

/// eave is about to take this tab's browser away at `url`. Arm the restore.
///
/// Called from the eviction scan and from nowhere else — see the module
/// note on why this is not hooked into the common close path. A tab at the
/// top of its page arms nothing.
pub fn note_evicted(tab_id: &str, url: Option<&str>) {
    let Some(url) = url else {
        // No address means nothing to match a revival against, so an
        // armed restore could only ever fire on the wrong page.
        return;
    };
    let at = AT.with(|a| a.borrow().get(tab_id).copied());
    let Some((x, y)) = at else { return };
    if x.abs() < WORTH_RESTORING && y.abs() < WORTH_RESTORING {
        return;
    }
    PENDING.with(|p| {
        p.borrow_mut().insert(
            tab_id.to_string(),
            Pending {
                url: url.to_string(),
                x,
                y,
            },
        );
    });
}

/// A load finished on `url`. If eave owes this tab a scroll position for
/// exactly that address, hand it over — once.
pub fn take(tab_id: &str, url: &str) -> Option<(f64, f64)> {
    PENDING.with(|p| {
        let mut map = p.borrow_mut();
        match map.get(tab_id) {
            Some(pending) if pending.url == url => {
                let (x, y) = (pending.x, pending.y);
                map.remove(tab_id);
                Some((x, y))
            }
            // A different address: the tab was pointed somewhere else
            // while it was away. Drop the debt rather than carrying it —
            // it can no longer come true, and a stale entry would fire on
            // a later navigation back to the old url having missed every
            // scroll in between.
            Some(_) => {
                map.remove(tab_id);
                None
            }
            None => None,
        }
    })
}

/// The tab is gone. Forget both halves.
pub fn forget(tab_id: &str) {
    AT.with(|a| {
        a.borrow_mut().remove(tab_id);
    });
    PENDING.with(|p| {
        p.borrow_mut().remove(tab_id);
    });
}

/// The script that puts the page back, bounded and self-cancelling.
///
/// Pure, so the shape of it is a test rather than something only a walk
/// can see. Written as one expression with no globals left behind: it is
/// injected into a page eave does not own, and a page that later finds an
/// `eaveRestore` on its window would be entitled to be surprised.
pub fn restore_script(x: f64, y: f64) -> String {
    // `{:?}` on an f64 round-trips and never emits an exponent for the
    // magnitudes a scroll offset has, where `{}` can print `1e7`. That is
    // still valid JS, but a literal that changes shape with its value is a
    // literal nobody can eyeball in a trace.
    //
    // `r` is read ONCE and reused across the retries, so a restore cannot
    // land at two different depths if the operator zooms mid-flight — it
    // finishes the job it started and the next scroll is theirs.
    format!(
        "(function(){{var r=window.devicePixelRatio||1,x={x:?}/r,y={y:?}/r,n=0;\
         function go(){{window.scrollTo(x,y);\
         if(++n<{RESTORE_FRAMES}&&(Math.abs(window.scrollY-y)>1||Math.abs(window.scrollX-x)>1))\
         requestAnimationFrame(go);}}\
         go();}})();"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test owns the maps for its own tab ids, so the process-global
    /// thread-locals need no lock — but a test that reused an id would
    /// read the previous one's state. Ids are unique per test on purpose.
    fn arm(tab: &str, url: &str, x: f64, y: f64) {
        note(tab, x, y);
        note_evicted(tab, Some(url));
    }

    #[test]
    fn an_offset_comes_back_only_at_the_address_it_was_taken_at() {
        arm("t-addr", "https://example.com/long", 0.0, 900.0);
        assert_eq!(
            take("t-addr", "https://example.com/other"),
            None,
            "an offset from one page was offered to another"
        );
        assert_eq!(
            take("t-addr", "https://example.com/long"),
            None,
            "the mismatch should have dropped the debt, not held it for later"
        );
    }

    #[test]
    fn the_right_address_gets_its_offset_back() {
        arm("t-match", "https://example.com/long", 12.0, 900.0);
        assert_eq!(take("t-match", "https://example.com/long"), Some((12.0, 900.0)));
    }

    /// **Consumed once.** Without this a reload of a revived tab would
    /// jump the page out from under whoever was already reading it.
    #[test]
    fn a_restore_fires_once_and_not_on_the_next_load() {
        arm("t-once", "https://example.com/a", 0.0, 400.0);
        assert_eq!(take("t-once", "https://example.com/a"), Some((0.0, 400.0)));
        assert_eq!(
            take("t-once", "https://example.com/a"),
            None,
            "the same restore fired twice"
        );
    }

    /// The top of a page is where a fresh load already is, so arming there
    /// would inject a script into most revivals to accomplish nothing.
    #[test]
    fn a_tab_at_the_top_arms_nothing() {
        arm("t-top", "https://example.com/a", 0.0, 0.0);
        assert_eq!(take("t-top", "https://example.com/a"), None);
    }

    #[test]
    fn a_tab_with_no_address_arms_nothing() {
        note("t-noaddr", 0.0, 900.0);
        note_evicted("t-noaddr", None);
        assert_eq!(take("t-noaddr", ""), None);
    }

    #[test]
    fn a_closed_tab_forgets_where_it_was() {
        arm("t-gone", "https://example.com/a", 0.0, 900.0);
        forget("t-gone");
        assert_eq!(take("t-gone", "https://example.com/a"), None);
        // And the live offset is gone too, so a tab id Chromium reuses
        // cannot inherit the last tab's position.
        note_evicted("t-gone", Some("https://example.com/a"));
        assert_eq!(take("t-gone", "https://example.com/a"), None);
    }

    /// The two traps in the script, pinned: it must name **both** axes (a
    /// horizontally scrolled page is rare and is not nothing), and it must
    /// stop itself, or a page that got shorter would re-ask for ever and
    /// could never be scrolled by hand.
    #[test]
    fn the_restore_script_names_both_axes_and_bounds_its_own_retry() {
        let js = restore_script(12.0, 900.0);
        assert!(js.contains("x=12.0/r") && js.contains("y=900.0/r"), "{js}");
        assert!(js.contains("window.scrollTo(x,y)"), "{js}");
        assert!(
            js.contains(&format!("n<{RESTORE_FRAMES}")),
            "the retry is unbounded: {js}"
        );
        assert!(
            js.contains("window.scrollX"),
            "only the vertical axis is restored: {js}"
        );
    }

    /// A scroll offset is a plain decimal at every magnitude a page has.
    ///
    /// Asserted on the LITERAL and not on the whole script, which is the
    /// first version of this test and was worthless: `requestAnimationFrame`
    /// contains an `e`, so `!js.contains('e')` fails on every input and
    /// says nothing about the number.
    #[test]
    fn a_long_page_does_not_get_an_exponent_in_its_literal() {
        let js = restore_script(0.0, 12_000_000.0);
        assert!(
            js.contains("y=12000000.0/r"),
            "the offset did not survive as a plain decimal: {js}"
        );
    }

    /// **The unit conversion is the page's, and it is the whole reason the
    /// first walk restored a page to twice its depth.** Chromium reports
    /// the offset in physical pixels; `window.scrollTo` takes CSS pixels;
    /// the ratio between them is the display scale times the page zoom,
    /// and `window.devicePixelRatio` is that product. Dividing on eave's
    /// side by the display scale alone would be right at 100% zoom and
    /// silently wrong at every other setting.
    #[test]
    fn the_page_converts_the_units_because_only_the_page_knows_the_zoom() {
        let js = restore_script(0.0, 3000.0);
        assert!(
            js.contains("window.devicePixelRatio"),
            "the offset was handed over unconverted: {js}"
        );
        assert!(
            js.contains("var r=window.devicePixelRatio||1"),
            "the ratio must be read once and reused, or a mid-flight zoom \
             lands the restore at two depths: {js}"
        );
    }
}
