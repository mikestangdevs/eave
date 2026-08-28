//! What the page has selected, as the chrome knows it (06-browser A4).
//!
//! # Why the chrome needs to know at all
//! Chromium owns the selection; it lives in the renderer, in a document
//! this process cannot read. But eave's chrome has to answer questions
//! about it without asking: is ⌘C worth anything right now, is there
//! something to search for, should an Edit menu's Copy be live. A menu
//! built at right-click time can read [`super::menu`]'s params and does —
//! `ContextMenuParams::selection_text` is authoritative at that instant.
//! **Nothing else has an instant.** A chord is pressed with no menu open,
//! and there is no callback to ask.
//!
//! So `RenderHandler::on_text_selection_changed` is the standing answer,
//! and this is where it is kept.
//!
//! # Why this file has no CEF in it
//! Same reason as [`super::ime`] and `nav_policy`: what is here is a map
//! and the rules for updating it, so the suite that runs by default
//! exercises all of it. The gated half is one callback in [`super::osr`].
//!
//! # What it is NOT
//! It is not the selection's *content* for any purpose but display.
//! Copying does not read this — [`super::tabs::copy_selection`] calls
//! `Frame::copy` and lets Blink decide what that means, because the
//! selection can span nodes, shadow roots and iframes that a plain string
//! does not describe. This is what the chrome shows and reasons about;
//! Blink is what acts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

/// One tab's selection, as last reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    /// The selected text, flattened. Empty is not stored — see [`note`].
    pub text: String,
    /// The range Chromium reported, in **UTF-16 units**, when it offered
    /// one. Kept for the same reason [`super::ime`] counts in UTF-16: it
    /// is what the platform's text APIs count in.
    pub range: Option<Range<usize>>,
}

thread_local! {
    static SELECTIONS: RefCell<HashMap<String, Selection>> = RefCell::new(HashMap::new());
}

/// Chromium has reported this tab's selection.
///
/// **An empty or absent text CLEARS the entry, and that is the whole
/// contract.** `on_text_selection_changed` fires on deselection with an
/// empty string rather than going silent, so a version that only ever
/// inserted would leave `Copy` live on a page with nothing selected — and
/// worse, would keep the *previous* selection's text, so "Search for
/// …" would offer words the operator had already clicked away from.
pub fn note(tab_id: &str, text: Option<&str>, range: Option<Range<usize>>) {
    let text = text.unwrap_or("");
    SELECTIONS.with(|s| {
        let mut map = s.borrow_mut();
        if text.is_empty() {
            map.remove(tab_id);
            return;
        }
        map.insert(
            tab_id.to_string(),
            Selection { text: text.to_string(), range },
        );
    });
}

/// The selected text, if this tab has any.
pub fn text_for(tab_id: &str) -> Option<String> {
    SELECTIONS.with(|s| s.borrow().get(tab_id).map(|k| k.text.clone()))
}

/// **Is there anything for ⌘C to do?** The question the key path asks.
pub fn has_selection(tab_id: &str) -> bool {
    SELECTIONS.with(|s| s.borrow().contains_key(tab_id))
}

/// A tab is gone. Without this a reused id inherits a stale selection and
/// offers to search for words from a page that is no longer open — the
/// same shape as [`super::ime::forget_tab`].
pub fn forget_tab(tab_id: &str) {
    SELECTIONS.with(|s| {
        s.borrow_mut().remove(tab_id);
    });
}

/// A navigation replaced the document, so nothing is selected any more.
///
/// **Chromium does not always say so.** A same-document navigation can
/// leave the old selection reported; a cross-document one fires the
/// callback, but not before the chrome has already drawn a frame of the
/// new page. Clearing on navigation makes the answer never *stale in the
/// dangerous direction* — offering nothing when there is something is a
/// missing affordance, offering the previous page's words is a wrong one.
pub fn note_navigated(tab_id: &str) {
    forget_tab(tab_id);
}

#[cfg(test)]
pub fn set_for_test(tab_id: &str, text: &str) {
    note(tab_id, Some(text), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_with_nothing_selected_offers_nothing() {
        forget_tab("t1");
        assert!(!has_selection("t1"));
        assert_eq!(text_for("t1"), None);
    }

    #[test]
    fn a_reported_selection_is_readable_back() {
        forget_tab("t2");
        note("t2", Some("hello world"), Some(0..11));
        assert!(has_selection("t2"));
        assert_eq!(text_for("t2").as_deref(), Some("hello world"));
    }

    #[test]
    fn an_empty_report_clears_rather_than_storing_an_empty_string() {
        forget_tab("t3");
        note("t3", Some("something"), None);
        // The positive control: without this the assertion below also
        // passes on a tab that never had a selection at all.
        assert!(has_selection("t3"));
        note("t3", Some(""), None);
        assert!(!has_selection("t3"));
        assert_eq!(text_for("t3"), None);
    }

    #[test]
    fn an_absent_report_clears_too() {
        forget_tab("t4");
        note("t4", Some("something"), None);
        assert!(has_selection("t4"));
        note("t4", None, None);
        assert!(!has_selection("t4"));
    }

    #[test]
    fn a_new_selection_replaces_the_old_one_rather_than_accumulating() {
        forget_tab("t5");
        note("t5", Some("first"), None);
        note("t5", Some("second"), None);
        assert_eq!(text_for("t5").as_deref(), Some("second"));
    }

    #[test]
    fn tabs_do_not_see_each_others_selections() {
        forget_tab("t6");
        forget_tab("t7");
        note("t6", Some("mine"), None);
        assert_eq!(text_for("t6").as_deref(), Some("mine"));
        assert!(!has_selection("t7"));
    }

    #[test]
    fn forgetting_a_tab_forgets_its_selection() {
        note("t8", Some("gone soon"), None);
        assert!(has_selection("t8"));
        forget_tab("t8");
        assert!(!has_selection("t8"));
    }

    #[test]
    fn navigating_clears_the_previous_documents_selection() {
        forget_tab("t9");
        note("t9", Some("old page words"), None);
        assert!(has_selection("t9"));
        note_navigated("t9");
        assert!(
            !has_selection("t9"),
            "a stale selection would offer to search for the previous page's text"
        );
    }

    #[test]
    fn whitespace_is_a_real_selection() {
        // Deliberately NOT trimmed: selecting a run of spaces in a code
        // block is a real thing to copy, and a `trim().is_empty()` guard
        // would silently refuse it.
        forget_tab("t10");
        note("t10", Some("   "), None);
        assert!(has_selection("t10"));
        assert_eq!(text_for("t10").as_deref(), Some("   "));
    }

    #[test]
    fn the_range_is_kept_when_offered_and_absent_when_not() {
        forget_tab("t11");
        note("t11", Some("abc"), Some(2..5));
        SELECTIONS.with(|s| {
            assert_eq!(s.borrow().get("t11").unwrap().range, Some(2..5));
        });
        note("t11", Some("abc"), None);
        SELECTIONS.with(|s| {
            assert_eq!(s.borrow().get("t11").unwrap().range, None);
        });
    }
}
