//! Per-tab page zoom.
//!
//! # The ladder is Chrome's, and the arithmetic is CEF's
//! CEF takes a *zoom level*, not a percentage, and the two are related by
//! `percent = 100 * 1.2^level` — Blink's own constant. So ⌘+ is not "add
//! ten percent": it is a step along a ladder that has been the same in
//! every Chromium browser for fifteen years (67, 75, 80, 90, 100, 110,
//! 125…), and inventing a different one would make eave's 110% a different
//! size from every other browser's 110% on the same page.
//!
//! The ladder is stored as percentages because that is what the operator
//! reads, and converted at the boundary because that is what CEF takes.
//!
//! # Per tab, and why that is not obvious
//! Chromium's own zoom is per *origin* — zoom `example.com` in one tab and
//! every other `example.com` tab follows. That is a preference, and a
//! preference needs somewhere to live and a way to be forgotten; eave has
//! neither yet. Per tab is the honest smaller thing: it says exactly what
//! it does, it dies with the tab, and it does not quietly write a setting
//! the operator cannot find. Origin-scoped zoom is a later slice and it
//! needs the settings surface first.

use cef::{ImplBrowser, ImplBrowserHost};

/// Set this tab's browser to `percent`.
///
/// The ladder, the per-tab bookkeeping and the arithmetic are in
/// `web/mod.rs`; this is the call into Chromium and nothing else.
pub(super) fn apply(tab_id: &str, percent: u32) {
    let level = super::level_for(percent);
    super::tabs::with_browser(tab_id, |b| {
        if let Some(host) = b.host() {
            host.set_zoom_level(level);
        }
    });
    super::host::note_activity();
    super::host::request_pump();
    super::host::note_chrome_change();
}
