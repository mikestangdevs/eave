//! Real popup windows: the sized `window.open` an OAuth flow uses (C2).
//!
//! # What was broken, and why it was silent
//! `on_before_popup` used to answer every popup one of two ways: turn it
//! into a tab, or refuse it. Both answers **cancel the popup CEF was about
//! to create**, and cancelling is what the page sees as `window.open()`
//! returning `null`. For a `target="_blank"` link nobody notices — a tab
//! is a better answer than a window and no script reads the handle. For
//! the shape every OAuth and payment flow is written in:
//!
//! ```js
//! var w = window.open('', 'auth', 'width=500,height=620');
//! w.location = provider + '?state=' + s;   // TypeError: w is null
//! ```
//!
//! …eave did nothing at all, with no error a person could see and nothing
//! in the trace unless `EAVE_WEB_TRACE` was on. A dead button. That is a
//! hard breakage rather than an annoyance, and it is the last one of its
//! size left in the browser arm.
//!
//! # The fix is to ADOPT the browser rather than cancel it
//! Returning 0 from `on_before_popup` with a windowless `WindowInfo` and a
//! client of our own makes CEF create the popup browser and hand it to
//! this process — same OSR pipeline as a tab, same handlers, and crucially
//! **the same Blink-level relationship to the opener**, so the returned
//! handle is live, `window.opener` works, and `w.location = …` navigates.
//! Nothing else can produce that handle: it exists at the instant
//! `on_before_popup` returns, and asking the engine to mint a tab is a
//! round trip over a socket that finishes long after the script has run.
//! **That is the whole reason a popup is a sheet and not a tab** — not an
//! aesthetic preference about windows.
//!
//! # Where it is drawn
//! A near-full overlay over the opener's page — Arc's peek, not a dialog.
//! eave is one window and a page may not make it grow another (the C3
//! ruling, unchanged) — but a popup is not really a request for a
//! *window*, it is a request for a **surface with a live handle that the
//! page can close**, and an overlay is all of that. It is also what an
//! OAuth popup already behaves like: you finish it or you cancel it, and
//! the opener waits.
//!
//! **A modal sheet was built first and refused on sight**, and the reason
//! is kept because it is the design's whole argument: a dialog-sized card
//! centred on the requested geometry treats a popup as a QUESTION, and a
//! popup is a PAGE — a consent screen or a payment form is a document
//! somebody has to read and work in. See `crate::popup_sheet`, which is
//! the surface; nothing on it states an address, and the size the page
//! asked for is not used at all (see [`page_size`]).
//!
//! # This module is the DECISION half and it is ungated
//! Same bargain as [`super::ask`] and [`super::scroll`]: the geometry, the
//! registry and the split intent are arithmetic and maps, so the
//! feature-off suite drives all of them. The CEF half — creating the
//! browser, closing it — is four call sites in `tabs.rs`.
//!
//! Not to be confused with [`super::osr::popup`], which is Blink's
//! PET_POPUP widget surface: the little rectangle a `<select>` drops down.
//! Same word, different layer.

use std::cell::RefCell;
use std::collections::HashMap;


/// How much of the window stays visible ABOVE the overlay.
///
/// Measured from the window, not the well: the overlay is root-mounted, so
/// this margin runs up through the address row to the window's own top
/// edge. Enough to leave the row it covers legible behind the dim, and no
/// more — a document wants its height.
pub const TOP: f32 = 28.0;

/// …and BELOW it, which is a smaller number than [`TOP`] on purpose.
///
/// The overlay is anchored to the top of the window, where the address
/// row it replaces is, and it runs almost to the bottom edge. Equal
/// margins made it look like it was floating in the middle of the window
/// rather than hanging from the top of it; this was settled by looking at
/// it, like the rest of these numbers.
pub const BOTTOM: f32 = 10.0;

/// How much of the well stays visible to the LEFT and RIGHT.
///
/// Wider than [`TOP`] on purpose, and **every one of these numbers was
/// settled on
/// screen rather than derived**: at one inset all round, the overlay
/// covered the page it came from almost completely and the dim stopped
/// reading as *the page underneath* — it read as a border. A page is also
/// easier to read narrower than the window it is in, which is a text
/// column's measure applied to somebody else's document.
pub const SIDE: f32 = 96.0;

/// The strip the controls stand in, down the right of the card.
///
/// **The card and its controls are centred in the well TOGETHER, as one
/// object** — not the card with its buttons hung off the side of it.
/// Centring the card alone pushes the controls out past the arrangement's
/// own middle and the whole thing reads as hung off-centre. So the well
/// spends half a gutter of padding at each end and the full gutter beside
/// the card, which puts the same margin to the left of the card as to the
/// right of the buttons.
///
/// The controls are **outside the card, not on it**. A page is somebody
/// else's rectangle and eave has no business drawing controls over it — a
/// button floating on the page is a button the page can hide something
/// behind, which is `page_dialog`'s clickjacking note in another shape.
pub const GUTTER: f32 = 38.0;

/// The size to create the popup's browser at, from the well it opens in.
///
/// The layout is what really decides — the sheet publishes its own bounds
/// as the browser's view on first paint, like every other page in eave —
/// so this exists to make the FIRST frame the right shape rather than a
/// default-sized one scaled into the card for a beat.
///
/// # The page's own requested size is not in this, and cannot be
/// **Measured 2026-09-03, twice, on CEF 151.6.0:** a
/// `window.open(u, n, "width=460,height=560")` arrives at
/// `on_before_popup` as `PopupFeatures { width: 0, width_set: 0,
/// height: 0, height_set: 0, is_popup: 1 }`. The struct is populated —
/// `is_popup` is right — but **every geometry field is zero and unset**.
/// CEF kept the fields and stopped filling them, which is BR5's
/// spellcheck shape one surface over.
///
/// A shim was written to recover it (the page knows what it asked for, so
/// the page was asked, over E9's binding — `notify.rs`'s move), measured
/// working end to end, and then **reverted**, because this design does
/// not want the number: the overlay is the well, and a 500×620 card in
/// the middle of a 1600 px window was the thing that made it a bad
/// surface in the first place. It is kept whole as
/// `scratchpad/c2-popup-size-shim.rs.kept` against a design that wants it
/// back. No decoder without a producer, and no producer without a
/// consumer.
pub fn page_size(well_w: f32, well_h: f32) -> (f32, f32) {
    (
        (well_w - SIDE * 2.0 - GUTTER * 2.0).max(1.0),
        (well_h - TOP - BOTTOM).max(1.0),
    )
}

/// A popup that is up, or being opened.
#[derive(Clone, Debug, PartialEq)]
pub struct Popup {
    /// The browser's id, in the `popup:` namespace — see
    /// [`super::peek_id`] for the same move and the same reason: it rides
    /// the per-id OSR pipeline with no strip row of its own.
    pub id: String,
    /// The TAB this sheet belongs to. A popup is drawn over its opener and
    /// dies with it; a popup opened from a popup resolves back to the same
    /// tab (see [`root_opener`]).
    pub opener: String,
    /// Where the popup is now. Empty until it commits — `window.open('')`
    /// starts at `about:blank` and is navigated a frame later by the
    /// script that opened it, which is the whole shape this slice exists
    /// for.
    pub url: String,
}

thread_local! {
    /// Popups by OPENER TAB, one each. `thread_local!` for [`super::ask`]'s
    /// reason: `#[gpui::test]` runs each test on its own thread inside one
    /// process, and a process-wide map would let one test's popup appear in
    /// another's window.
    static OPEN: RefCell<HashMap<String, Popup>> = RefCell::new(HashMap::new());
    static NEXT: RefCell<u64> = const { RefCell::new(0) };
}

/// A fresh popup browser id.
pub fn mint_id() -> String {
    NEXT.with(|n| {
        let mut n = n.borrow_mut();
        *n += 1;
        format!("popup:{n}")
    })
}

/// Whether a popup's target is "no address yet".
///
/// **Measured: `window.open('')` reaches `on_before_popup` as
/// `about:blank`, not as an empty string.** The first cut of this slice
/// tested for empty and would have missed the handle-first shape entirely
/// on the one disposition that needs it — a `window.open('')` with no
/// window features at all, which arrives as an ordinary foreground-tab
/// disposition and would have been turned into a tab, cancelling the
/// popup and handing the script `null` all over again.
pub fn is_blank(url: &str) -> bool {
    url.is_empty() || url == "about:blank" || url == "about:blank#blocked"
}

/// Whether an id names a popup browser rather than a tab.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn is_popup_id(id: &str) -> bool {
    id.starts_with("popup:")
}

/// The popup on a tab, if one is up.
pub fn open_for(tab_id: &str) -> Option<Popup> {
    OPEN.with(|o| o.borrow().get(tab_id).cloned())
}

/// The popup with this browser id, if it is still up.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn by_id(id: &str) -> Option<Popup> {
    OPEN.with(|o| o.borrow().values().find(|p| p.id == id).cloned())
}

/// The TAB a new popup belongs to, given whoever opened it.
///
/// A popup that opens a popup — an identity provider handing off to a
/// second one, which is a real OAuth shape — resolves back to the tab that
/// started it all, so the sheet stays where the person is looking instead
/// of stacking modals over each other.
pub fn root_opener(opener: &str) -> String {
    by_id(opener)
        .map(|p| p.opener)
        .unwrap_or_else(|| opener.to_string())
}

/// File a popup as this tab's. Returns whatever it displaced, **whose
/// browser the caller must close** — an orphaned popup browser is an
/// invisible live page, which is worse than either keeping or closing it.
pub fn note_opened(popup: Popup) -> Option<Popup> {
    OPEN.with(|o| o.borrow_mut().insert(popup.opener.clone(), popup))
}

/// Where a popup went, from its own `on_address_change`.
pub fn note_url(id: &str, url: &str) {
    OPEN.with(|o| {
        if let Some(p) = o.borrow_mut().values_mut().find(|p| p.id == id) {
            p.url = url.to_string();
        }
    });
}

/// Take a popup off the screen, by browser id.
pub fn forget(id: &str) -> Option<Popup> {
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let key = o.iter().find(|(_, p)| p.id == id).map(|(k, _)| k.clone())?;
        o.remove(&key)
    })
}

/// Take a tab's popup off the screen. Used when the TAB goes.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn forget_opener(tab_id: &str) -> Option<Popup> {
    OPEN.with(|o| o.borrow_mut().remove(tab_id))
}

/// Whether this popup browser is still wanted — the reaper's question.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn browser_wanted(id: &str) -> bool {
    by_id(id).is_some()
}

// ── "Put it beside the page it came from" ───────────────────────────────
//
// The split control cannot finish its own work. Splitting means pairing
// two TAB IDS, and the tab this one needs does not exist yet: opening it
// is a `PageRequest` to the engine, which mints the id and announces it in
// a later snapshot. So the intent is filed here and the shell settles it
// when the tab appears — the same shape as a pending navigation, one layer
// out.
//
// **The tab is recognised by being NEW, not by being named.** `TabInfo`
// carries no opener on the wire (C3's model is engine-side and unused), so
// the ids present at the moment of the ask are remembered and the match is
// the one that was not there before and is at the address asked for. A
// second tab opened by hand in the same instant is the only thing that
// could confuse it, and the cost of guessing wrong is a split the operator
// did not ask for rather than anything lost.

/// A split waiting for its tab.
#[derive(Clone, Debug, PartialEq)]
struct PendingSplit {
    opener: String,
    url: String,
    known: Vec<String>,
}

thread_local! {
    static SPLIT: RefCell<Option<PendingSplit>> = const { RefCell::new(None) };
}

/// File the intent: `url` has just been asked for, to be put beside
/// `opener`. `known` is every tab id that exists right now.
pub fn request_split(opener: &str, url: &str, known: Vec<String>) {
    SPLIT.with(|s| {
        *s.borrow_mut() = Some(PendingSplit {
            opener: opener.to_string(),
            url: url.to_string(),
            known,
        });
    });
}

/// The pair to split, once the tab has arrived. `tabs` is every tab as
/// `(id, url, pending url)` — a tab that has been asked to navigate but
/// has not committed yet answers on the second, which is the ordinary
/// case here because the ask and the snapshot race.
///
/// Consumed on success. **Also consumed when the opener is gone**: a split
/// with one live end is not a split, and an intent that can never be
/// satisfied would otherwise match the next tab opened at that address,
/// minutes later, for no reason anybody could see.
pub fn split_ready(tabs: &[(String, Option<String>, Option<String>)]) -> Option<(String, String)> {
    let pending = SPLIT.with(|s| s.borrow().clone())?;
    if !tabs.iter().any(|(id, _, _)| id == &pending.opener) {
        SPLIT.with(|s| *s.borrow_mut() = None);
        return None;
    }
    let found = tabs.iter().find(|(id, url, going)| {
        !pending.known.contains(id)
            && (url.as_deref() == Some(pending.url.as_str())
                || going.as_deref() == Some(pending.url.as_str()))
    })?;
    SPLIT.with(|s| *s.borrow_mut() = None);
    Some((pending.opener.clone(), found.0.clone()))
}

/// The url "open in a tab" would open, or `None` when there is nothing to
/// promote yet.
///
/// **A promotion is a NAVIGATION, not a hand-off.** The popup's browser
/// cannot move into a tab: its id is bound into the CEF client at
/// creation, and the tab's own browser is created by the engine's
/// reconcile long after. So this opens the popup's *current address* in a
/// real tab and closes the sheet, which is right for "this login is too
/// small to use" and wrong for a flow mid-POST. `None` — and a disabled
/// control — for a popup that has not committed anywhere, because opening
/// a tab on `about:blank` is a promotion to nothing.
pub fn promote_url(popup: &Popup) -> Option<String> {
    super::nav_policy::host(&popup.url).map(|_| popup.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn popup(url: &str) -> Popup {
        Popup {
            id: "popup:1".into(),
            opener: "t1".into(),
            url: url.into(),
        }
    }






    #[test]
    fn the_handle_first_shape_is_recognised_in_chromiums_own_spelling() {
        // `window.open('')` arrives as `about:blank`. Testing for an empty
        // string missed it, which would have left the whole OAuth shape
        // broken on the disposition that needs this most.
        assert!(is_blank(""));
        assert!(is_blank("about:blank"));
        assert!(!is_blank("https://accounts.google.com/"));
    }




    #[test]
    fn a_popup_with_nowhere_to_go_cannot_be_promoted() {
        assert_eq!(promote_url(&popup("")), None);
        assert_eq!(promote_url(&popup("about:blank")), None);
        assert_eq!(
            promote_url(&popup("https://example.com/a")).as_deref(),
            Some("https://example.com/a")
        );
    }

    #[test]
    fn a_popups_popup_stays_on_the_tab_the_person_is_looking_at() {
        let p = popup("");
        note_opened(p.clone());
        assert_eq!(root_opener("popup:1"), "t1");
        // Anything that is not a live popup id is its own root, which is
        // the ordinary case: a tab opened it.
        assert_eq!(root_opener("t9"), "t9");
        forget("popup:1");
    }

    #[test]
    fn one_sheet_per_tab_and_the_displaced_one_is_handed_back() {
        let first = popup("https://a.example/");
        note_opened(first.clone());
        let mut second = popup("https://b.example/");
        second.id = "popup:2".into();
        let displaced = note_opened(second);
        assert_eq!(displaced.map(|p| p.id), Some("popup:1".to_string()));
        assert_eq!(open_for("t1").map(|p| p.id), Some("popup:2".to_string()));
        assert!(!browser_wanted("popup:1"), "the displaced browser is unwanted");
        forget("popup:2");
    }

    #[test]
    fn a_closed_popup_is_wanted_by_nobody() {
        note_opened(popup(""));
        assert!(browser_wanted("popup:1"));
        assert_eq!(forget("popup:1").map(|p| p.opener), Some("t1".to_string()));
        assert!(!browser_wanted("popup:1"));
        assert_eq!(open_for("t1"), None);
        // Twice is a no-op, not a panic: the sheet's ✕ and the page's own
        // `window.close()` can both arrive for one popup.
        assert_eq!(forget("popup:1"), None);
    }

    /// Where a popup went is not drawn anywhere — the overlay is all page
    /// — but it is what the expand and split controls open, so it has to
    /// be right.
    #[test]
    fn where_a_popup_went_is_what_the_controls_would_open() {
        note_opened(popup(""));
        assert_eq!(promote_url(&open_for("t1").unwrap()), None);
        note_url("popup:1", "https://accounts.google.com/signin");
        assert_eq!(
            promote_url(&open_for("t1").unwrap()).as_deref(),
            Some("https://accounts.google.com/signin")
        );
        // An address for a popup that is gone is dropped, not resurrected.
        forget("popup:1");
        note_url("popup:1", "https://elsewhere.example/");
        assert_eq!(open_for("t1"), None);
    }








    #[test]
    fn a_split_waits_for_the_tab_the_engine_has_not_minted_yet() {
        let here = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        request_split("t1", "https://accounts.test/", here(&["t1", "t2"]));
        // The tab is not there yet: the ask stands rather than matching
        // the opener or some other tab that happens to be at that url.
        let before = vec![
            ("t1".to_string(), Some("https://opener.test/".into()), None),
            ("t2".to_string(), Some("https://accounts.test/".into()), None),
        ];
        assert_eq!(split_ready(&before), None);
        // It arrives, still on its pending navigation — which is the
        // ordinary case, since the snapshot beats the commit.
        let mut after = before.clone();
        after.push((
            "t3".to_string(),
            None,
            Some("https://accounts.test/".into()),
        ));
        assert_eq!(
            split_ready(&after),
            Some(("t1".to_string(), "t3".to_string()))
        );
        // Consumed: a second snapshot must not split it again.
        assert_eq!(split_ready(&after), None);
    }

    #[test]
    fn a_split_whose_opener_has_gone_is_dropped_rather_than_left_armed() {
        request_split("t1", "https://accounts.test/", vec!["t1".to_string()]);
        let without_opener = vec![(
            "t9".to_string(),
            Some("https://accounts.test/".into()),
            None,
        )];
        assert_eq!(split_ready(&without_opener), None);
        // …and it is gone, so a later tab at that address is not grabbed.
        let with_both = vec![
            ("t1".to_string(), Some("https://opener.test/".into()), None),
            (
                "t9".to_string(),
                Some("https://accounts.test/".into()),
                None,
            ),
        ];
        assert_eq!(split_ready(&with_both), None);
    }

    #[test]
    fn the_overlay_takes_the_well_and_leaves_the_gutter() {
        let (w, h) = page_size(1600.0, 900.0);
        // Half a gutter of padding at each end plus the full gutter the
        // controls stand in — the arrangement is centred as one object.
        assert_eq!(w, 1600.0 - SIDE * 2.0 - GUTTER * 2.0);
        assert_eq!(h, 900.0 - TOP - BOTTOM);
        // A well smaller than the chrome around the page still names a
        // size a browser can be created at: zero is not one, and CEF
        // treats it as "pick something for me".
        let (w, h) = page_size(10.0, 10.0);
        assert!(w >= 1.0 && h >= 1.0, "degenerate well gave {w}x{h}");
    }

    #[test]
    fn ids_are_namespaced_so_the_reaper_can_tell_them_from_tabs() {
        let a = mint_id();
        let b = mint_id();
        assert_ne!(a, b);
        assert!(is_popup_id(&a));
        assert!(!is_popup_id("t1"));
        assert!(!is_popup_id("peek:github.com"));
    }
}
