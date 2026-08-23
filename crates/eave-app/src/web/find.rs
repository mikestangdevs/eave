//! Find in page: CEF counts and highlights, eave states the number.
//!
//! # Why this is thin, and what that means
//! `CefBrowserHost::Find` is a complete find implementation — it walks the
//! DOM, it paints the yellow-and-orange match highlighting, it scrolls the
//! active match into view, and it survives navigation. None of that is
//! eave's to build, and building any of it would be building a second,
//! worse one over the top of a page whose pixels we do not own.
//!
//! What eave owns is exactly two things: **the query**, and **the number**.
//! The query is text the operator typed into a surface Chromium cannot see;
//! the number arrives asynchronously in [`ImplFindHandler::on_find_result`]
//! and is the only fact the find bar states.
//!
//! # The number is CEF's, verbatim
//! `active_match_ordinal` is 1-based and `count` is the total, so "3 of 17"
//! is a transcription and not a calculation. It matters that it is: a find
//! bar that computes its own ordinal from its own idea of which match is
//! current will disagree with the highlighted one the moment a page
//! reflows, and the disagreement is silent.
//!
//! # `final_update` is why [`Find::settled`] exists
//! CEF reports progressively — a long page raises several results for one
//! query, with a rising count, and only the last has `final_update` set.
//! So "0 matches" and "0 matches *so far*" are different facts arriving on
//! the same callback, and a bar that renders the first as "no matches"
//! flashes a wrong answer on every keystroke over a long document. The flag
//! is carried so the shell can decline to say anything it does not yet
//! know.

// `wrap_find_handler!` expands to `WrapFindHandler`/`ImplFindHandler`
// and names `FindHandler` as its return type, none of which appear in
// the source here — so the glob is what the macro requires, not
// shorthand. Same reason `tabs.rs` and `osr.rs` carry one.
use cef::*;

/// Ask Chromium to search. The query and the count are eave's bookkeeping
/// and live in `web/mod.rs`; this is the call.
pub(super) fn drive(tab_id: &str, query: &str, forward: bool, find_next: bool) {
    super::tabs::with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.find(
                Some(&CefString::from(query)),
                forward as i32,
                // Case-insensitive, like every find bar on this platform.
                // A case toggle is a control this bar does not have yet,
                // and defaulting to sensitive would make it silently miss.
                0,
                find_next as i32,
            );
        }
    });
    super::host::note_activity();
    super::host::request_pump();
}

pub(super) fn stop(tab_id: &str, clear_selection: bool) {
    super::tabs::with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.stop_finding(clear_selection as i32);
        }
    });
    super::host::request_pump();
}

wrap_find_handler! {
    pub struct PageFindHandler {
        tab_id: String,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: i32,
            count: i32,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: i32,
            final_update: i32,
        ) {
            if super::host::traced() {
                eprintln!(
                    "[web] find result {}/{} final={} for {}",
                    active_match_ordinal, count, final_update, self.tab_id
                );
            }
            super::note_find_result(
                &self.tab_id,
                count,
                active_match_ordinal,
                final_update == 1,
            );
            // The number changed and no page frame necessarily did — a
            // result that only moves the tally would otherwise sit unseen
            // until something else asked for a redraw.
            super::host::note_chrome_change();
        }
    }
}
