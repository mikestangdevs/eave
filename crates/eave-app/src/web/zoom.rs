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
//! # Per tab — and the justification below it was FALSE (measured
//! 2026-08-23)
//! This used to say: Chromium's zoom is per origin, that is a preference, a
//! preference needs somewhere to live and a way to be forgotten, *eave has
//! neither yet*, and per tab is the honest smaller thing because it "does
//! not quietly write a setting the operator cannot find".
//!
//! **Every ⌘+ writes exactly that setting.** `set_zoom_level` below reaches
//! `ZoomController` in `ZOOM_MODE_DEFAULT`, whose default branch is
//! `zoom_map->SetZoomLevelForHost(host, level)` — per host and durable. It
//! is on this machine's disk right now, in
//! `<data_dir>/browser/chromium/Default/Preferences`:
//!
//! ```text
//! partition.per_host_zoom_levels.x."www.google.com".zoom_level
//!     = -0.5778829311823857
//! ```
//!
//! That number is `level_for(90)` to the last digit — one notch down eave's
//! own ladder, converted by the function twenty lines up, stored by
//! Chromium under a host key. So the place a preference could live already
//! exists and eave is already writing to it; what is missing is the way to
//! *forget* it and any surface that shows it.
//!
//! # What that makes this module
//! Not "the smaller honest thing" but **the second of two sources of
//! truth.** Chromium remembers the origin across restarts; `ZOOMS` in
//! `web/mod.rs` is keyed by `tab_id` and starts empty every launch. The
//! divergence they predict: open a fresh tab on an origin zoomed earlier —
//! **the page renders zoomed and eave's chrome says 100%.** Unverified in
//! the running app; it is one launch to check and it is the first thing
//! the G5 slice should do.
//!
//! Origin-scoped zoom is therefore not "a later slice that needs a settings
//! surface first". It is a reconciliation with a store that is already
//! there. See `06-browser.md` G5.

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
