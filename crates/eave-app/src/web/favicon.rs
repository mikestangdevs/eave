//! Favicon bytes, fetched through Chromium and painted on the strip.
//!
//! Spec 06-browser D13: the url is a fact about the tab (journalled, on
//! the snapshot); the pixels are a cache. Fetch through the browser that
//! already has the page's cookies — never a second HTTP client. Decode is
//! Chromium's (`CefImage::get_as_png`); gpui then takes a PNG it already
//! knows how to paint.
//!
//! **Do not call `download_image` from inside `OnFaviconURLChange`.**
//! That handler runs in the middle of `CefDoMessageLoopWork`. Starting a
//! URLRequest there is a no-op on this CEF (Alloy + OSR): the callback
//! never fires and the globe stays. [`fetch`] only queues; [`run_queued`]
//! starts the download after the pump returns. Dark-on-transparent glyphs
//! are inverted in [`store`] so they read on the rail's dark tiles. Each
//! slot gets its own device-size PNG: the tab plate is 16 logical (32
//! device on 2×), and the bookmark mark is the same 16 (32 device).
//! Sharing one 48×48 bitmap used to bilinear-filter the plate 48→32.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cef::*;
use gpui::ImageFormat;

use super::tabs::{with_any_browser, with_browser};

/// One download, two paint sizes. gpui `img` treats PNG pixels as 1×
/// logical and Metal bilinear-filters a miss, so the plate and the tile
/// cannot share a bitmap.
///
/// `source_px` is the native side before slot resize. GitHub's tab PNG
/// is 32 — 1:1 on the plate and the tile at 32 device. A later
/// apple-touch (120) must be allowed to replace it.
#[derive(Clone)]
struct CachedIcon {
    tab: Arc<gpui::Image>,
    tile: Arc<gpui::Image>,
    source_px: u32,
}

thread_local! {
    static ICONS: RefCell<HashMap<String, CachedIcon>> = RefCell::new(HashMap::new());
    /// Last URL we asked Chromium to fetch, so a snapshot that restates
    /// the same icon does not start another download.
    static ASKED: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// The same pixels keyed by HOST, for surfaces that have a url but no
    /// browser — the rail's pinned bookmark grid (Slice T1b).
    ///
    /// A bookmark is not a tab, so it can never appear in [`ICONS`]. Keying
    /// by host also means visiting a site pays for its bookmark's tile: one
    /// download serves the tab plate and every bookmark to that host.
    static BY_HOST: RefCell<HashMap<String, CachedIcon>> = RefCell::new(HashMap::new());
    /// `(host, icon_url)` -> the [`LOAD_GEN`] we last asked at. Keyed by
    /// URL, not host: a 32 px `/favicon.ico` must not block the
    /// apple-touch upgrade that makes the tile sharp.
    static ASKED_HOST_URL: RefCell<HashMap<(String, String), u64>> = RefCell::new(HashMap::new());
    /// Bumped every time a page finishes loading.
    ///
    /// **This is the whole fix for "the tiles stayed globes".** The first
    /// version marked a host asked the moment `download_image` was CALLED,
    /// which is not the moment it succeeds. The rail paints on the very
    /// first frame, so the call landed three pumps after `on_after_created`
    /// and before the browser had committed any navigation — Chromium threw
    /// it away without ever invoking the callback, and because the host was
    /// already marked, nothing asked again. `started=true` and a favicon
    /// that never arrives are not a contradiction.
    ///
    /// Keying the mark to a generation gives every icon-less bookmark one
    /// more attempt each time a page load completes, which is exactly when
    /// a browser is known good, and stops when the bytes land.
    static LOAD_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// `(tab_id, icon_url, page_host)` — `tab_id` empty for a bookmark.
    static QUEUE: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
}

/// A page finished loading — some browser is now known to be usable.
pub fn note_page_loaded() {
    LOAD_GEN.with(|g| g.set(g.get() + 1));
}

/// The icon for any url, by host, sized for the bookmark tile.
/// `None` means "no bytes yet", never "this site has no icon" — same
/// contract as [`get`].
pub fn for_url(url: &str) -> Option<Arc<gpui::Image>> {
    host_icon(url).map(|c| c.tile)
}

/// The same host cache, sized for the tab plate. A restored tab has no
/// tab-id entry until Chromium restates the icon; the host paid for both
/// sizes when the page (or a bookmark) last fetched.
pub fn tab_for_url(url: &str) -> Option<Arc<gpui::Image>> {
    host_icon(url).map(|c| c.tab)
}

fn host_icon(url: &str) -> Option<CachedIcon> {
    let host = crate::omnibox::engine_host(url);
    if host.is_empty() {
        return None;
    }
    BY_HOST.with(|m| m.borrow().get(&host).cloned())
}

pub fn get(tab_id: &str) -> Option<Arc<gpui::Image>> {
    ICONS.with(|m| m.borrow().get(tab_id).map(|c| c.tab.clone()))
}

fn cached(tab_id: &str) -> Option<CachedIcon> {
    ICONS.with(|m| m.borrow().get(tab_id).cloned())
}

fn host_source_px(host: &str) -> u32 {
    BY_HOST.with(|m| m.borrow().get(host).map(|c| c.source_px).unwrap_or(0))
}

/// A source at least this large Lanczos-down to the tile without
/// inventing pixels. 32 (GitHub's tab PNG / ICO) is below it — that is
/// the pixelated bookmark when the tile was 48 device; apple-touch
/// still upgrades it.
const TILE_SOURCE_MIN: u32 = 96;

/// Forget a tab that has actually closed. An eviction keeps the pixels:
/// the tab is still on the strip and the icon is still true of it.
pub fn forget(tab_id: &str) {
    ICONS.with(|m| {
        m.borrow_mut().remove(tab_id);
    });
    ASKED.with(|m| {
        m.borrow_mut().remove(tab_id);
    });
    QUEUE.with(|q| {
        q.borrow_mut().retain(|(id, _, _)| id != tab_id);
    });
}

/// Queue a fetch. Safe to call from a CEF handler; the download starts
/// on [`run_queued`], after the pump.
///
/// `page_url` is what the icon is keyed by in [`BY_HOST`] — the PAGE's
/// host, not the icon's. They differ whenever a site serves its mark off a
/// CDN, and it is the page's host a bookmark will look up.
pub fn fetch(tab_id: &str, url: &str, page_url: &str) {
    if url.is_empty() {
        return;
    }
    // Both call sites reach here for the same tab: the CEF handler the
    // moment a page declares its icon, and the snapshot pass afterwards.
    // Whichever runs first wins the download, so the host must be filed
    // from the pixels we already hold rather than from this call's fetch —
    // otherwise the second call skips and the host key is never written,
    // which is exactly the bookmark tile staying a globe forever.
    let page_host = crate::omnibox::engine_host(page_url);
    if !page_host.is_empty() {
        if let Some(icon) = cached(tab_id) {
            BY_HOST.with(|m| {
                m.borrow_mut().entry(page_host.clone()).or_insert(icon);
            });
        }
        // Tab PNG is often 32 (GitHub). Fine 1:1 on today's 32-device
        // tile; apple-touch still upgrades it, and Calendar's origin
        // 404s so that host goes through known_icon_url instead.
        maybe_enqueue_upgrade(&page_host);
    }
    let skip = ASKED.with(|m| m.borrow().get(tab_id).map(|u| u == url).unwrap_or(false))
        && get(tab_id).is_some();
    if skip {
        return;
    }
    enqueue(tab_id.to_string(), url.to_string(), page_host);
}

/// `{scheme}://{host}/favicon.ico` for an ordinary page url, so a site
/// that never raises `OnFaviconURLChange` still gets an icon.
pub fn fallback_url(page_url: &str) -> Option<String> {
    let scheme = if let Some(rest) = page_url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = page_url.strip_prefix("http://") {
        ("http", rest)
    } else {
        return None;
    };
    let host = scheme.1.split('/').next().filter(|h| !h.is_empty())?;
    if host.contains('@') {
        return None;
    }
    Some(format!("{}://{host}/favicon.ico", scheme.0))
}

/// Start every queued download. Call after `CefDoMessageLoopWork` so we
/// are not inside a display-handler stack.
pub fn run_queued() {
    let jobs = QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for (tab_id, url, page_host) in jobs {
        start(&tab_id, &url, &page_host);
    }
}

fn enqueue(tab_id: String, url: String, page_host: String) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if !q.iter().any(|(id, u, _)| id == &tab_id && u == &url) {
            q.push((tab_id, url, page_host));
        }
    });
    super::host::request_pump();
}

fn start(tab_id: &str, url: &str, page_host: &str) {
    if !tab_id.is_empty()
        && get(tab_id).is_some()
        && ASKED.with(|m| m.borrow().get(tab_id).map(|u| u == url).unwrap_or(false))
    {
        return;
    }
    if tab_id.is_empty() && !page_host.is_empty() {
        if host_source_px(page_host) >= TILE_SOURCE_MIN {
            return;
        }
    }

    let download = |b: &Browser| {
        let Some(host) = b.host() else {
            return false;
        };
        let mut cb =
            FaviconCallback::new(tab_id.to_string(), url.to_string(), page_host.to_string());
        IN_FLIGHT.with(|v| {
            v.borrow_mut().push(cb.clone());
            if v.borrow().len() > 32 {
                v.borrow_mut().remove(0);
            }
        });
        // `is_favicon=0`: the dedicated favicon path on this CEF does not
        // complete under OSR. An ordinary image download still uses the
        // browser's request context (cookies, cache, proxy).
        // `max_image_size` 256: keep a large source (apple-touch, SVG
        // that actually rasters big, 128 PNG) and Lanczos *down* to the
        // slot. 0 can mean "default small" on some CEF builds.
        host.download_image(Some(&CefString::from(url)), 0, 256, 0, Some(&mut cb));
        true
    };

    let started = if tab_id.is_empty() {
        with_any_browser(download).unwrap_or(false)
    } else {
        let own = with_browser(tab_id, download).unwrap_or(false);
        if own {
            true
        } else {
            with_any_browser(download).unwrap_or(false)
        }
    };
    if !started {
        return;
    }
    if !tab_id.is_empty() {
        ASKED.with(|m| {
            m.borrow_mut().insert(tab_id.to_string(), url.to_string());
        });
    } else {
        let gen = LOAD_GEN.with(|g| g.get());
        ASKED_HOST_URL.with(|m| {
            m.borrow_mut()
                .insert((page_host.to_string(), url.to_string()), gen);
        });
    }
    super::host::request_pump();
    if super::tabs::trace() {
        eprintln!("[web] favicon start {url} for {tab_id}");
    }
}

/// File the pixels under both keys.
///
/// `tab_id` is empty for a bookmark fetch, which belongs to no tab; `host`
/// is empty for a url we could not read a host out of. Either may be
/// skipped, and skipping both is a download nobody can ever see — which is
/// why the caller checks before starting one.
fn store(tab_id: &str, host: &str, icon: CachedIcon) {
    if !tab_id.is_empty() {
        ICONS.with(|m| {
            let mut m = m.borrow_mut();
            let better = m
                .get(tab_id)
                .is_none_or(|old| icon.source_px >= old.source_px);
            if better {
                m.insert(tab_id.to_string(), icon.clone());
            }
        });
    }
    if !host.is_empty() {
        BY_HOST.with(|m| {
            let mut m = m.borrow_mut();
            let better = m
                .get(host)
                .is_none_or(|old| icon.source_px >= old.source_px);
            if better {
                m.insert(host.to_string(), icon);
            }
        });
    }
    super::host::note_chrome_change();
}

/// `{origin}/apple-touch-icon.png` is the size Arc uses for a pin.
/// GitHub's is 120×120; `/favicon.ico` is 16+32 and looks like Lego on
/// a small tile. Skip when we already have ≥ [`TILE_SOURCE_MIN`].
/// Hosts whose origin 404s both guesses (Calendar) go through
/// [`known_icon_url`] instead of burning two downloads on 404s.
fn maybe_enqueue_upgrade(host: &str) {
    if host.is_empty() || host_source_px(host) >= TILE_SOURCE_MIN {
        return;
    }
    if let Some(url) = known_icon_url(host) {
        enqueue_host_url(host, url);
        return;
    }
    enqueue_host_url(host, format!("https://{host}/apple-touch-icon.png"));
}

fn enqueue_host_url(host: &str, url: String) {
    if host.is_empty() || host_source_px(host) >= TILE_SOURCE_MIN {
        return;
    }
    let gen = LOAD_GEN.with(|g| g.get());
    let asked = ASKED_HOST_URL.with(|m| {
        m.borrow()
            .get(&(host.to_string(), url.clone()))
            .is_some_and(|asked| *asked == gen)
    });
    if asked {
        return;
    }
    enqueue(String::new(), url, host.to_string());
}

/// A mark Chromium will not find at `{origin}/favicon.ico`.
///
/// Measured 2026-08-28: `calendar.google.com/favicon.ico` and
/// `…/apple-touch-icon.png` both 302 to `/calendar/…` and 404. The
/// icon the Calendar page itself paints is the dated gstatic PNG
/// (`calendar_{mday}_2x.png`, no leading zero — `calendar_01_2x.png`
/// 404s). Fetch still goes through Chromium (`download_image`), never
/// a second HTTP client.
fn known_icon_url(host: &str) -> Option<String> {
    known_icon_url_on(host, local_mday())
}

fn known_icon_url_on(host: &str, day: u32) -> Option<String> {
    match host {
        "calendar.google.com" => {
            let day = day.clamp(1, 31);
            Some(format!(
                "https://ssl.gstatic.com/calendar/images/dynamiclogo_2020q4/calendar_{day}_2x.png"
            ))
        }
        _ => None,
    }
}

fn local_mday() -> u32 {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        if t < 0 {
            return 1;
        }
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 1;
        }
        tm.tm_mday.clamp(1, 31) as u32
    }
}

/// Ask for a BOOKMARK's icon — no tab, no browser of its own.
///
/// Routed through whatever browser happens to be live, per spec 06-browser
/// D13 (never a second HTTP client). Guesses apple-touch first (a 32 px
/// ICO is what made GitHub look pixelated), then `/favicon.ico` so a
/// site with no touch icon still gets a mark. Hosts in [`known_icon_url`]
/// skip the origin guesses — those 404.
///
/// Queues rather than starting on this stack — same reason as [`fetch`].
pub fn fetch_for_url(url: &str) {
    let host = crate::omnibox::engine_host(url);
    if host.is_empty() {
        return;
    }
    maybe_enqueue_upgrade(&host);
    if host_source_px(&host) > 0 || known_icon_url(&host).is_some() {
        return;
    }
    enqueue_host_url(&host, format!("https://{host}/favicon.ico"));
}

fn png_from_cef(image: &mut Image) -> Option<Vec<u8>> {
    if image.is_empty() != 0 {
        return None;
    }
    // Ask for the *native* representation with the most pixels, not
    // `as_png(3.0)` — closest-match at 3× can bilinear-upsample a 16×16
    // to 48, and that blurry PNG is what we then painted 1:1.
    let mut best_scale = 1.0_f32;
    let mut best_area = 0i32;
    let mut found = false;
    for want in [3.0_f32, 2.0, 1.5, 1.0] {
        let mut actual = 0.0_f32;
        let mut w = 0i32;
        let mut h = 0i32;
        if image.representation_info(want, Some(&mut actual), Some(&mut w), Some(&mut h)) == 0 {
            continue;
        }
        let area = w.saturating_mul(h);
        if area > best_area {
            best_area = area;
            best_scale = if actual > 0.0 { actual } else { want };
            found = true;
        }
    }
    let scales: &[f32] = if found {
        &[best_scale]
    } else {
        &[3.0, 2.0, 1.0]
    };
    let mut best: Option<(i32, Vec<u8>)> = None;
    for scale in scales {
        let mut w = 0i32;
        let mut h = 0i32;
        let Some(png) = image.as_png(*scale, 1, Some(&mut w), Some(&mut h)) else {
            continue;
        };
        let n = png.size();
        if n == 0 {
            continue;
        }
        let mut buf = vec![0u8; n];
        let copied = png.data(Some(&mut buf), 0);
        let bytes = if copied > 0 {
            buf.truncate(copied);
            buf
        } else {
            let ptr = png.raw_data() as *const u8;
            if ptr.is_null() {
                continue;
            }
            unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec()
        };
        let area = w.saturating_mul(h);
        if best.as_ref().is_none_or(|(a, _)| area > *a) {
            best = Some((area, bytes));
        }
    }
    best.map(|(_, b)| b)
}

// `wrap_download_image_callback!` expands to trait names that do not
// appear in this source, same as the other CEF handler wrappers.
wrap_download_image_callback! {
    pub struct FaviconCallback {
        tab_id: String,
        url: String,
        host: String,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            _image_url: Option<&CefString>,
            http_status_code: i32,
            image: Option<&mut Image>,
        ) {
            // 200 live, 0 cache, 304 not-modified. Anything 4xx/5xx keeps
            // the globe.
            let ok = http_status_code == 0
                || http_status_code == 304
                || (200..300).contains(&http_status_code);
            if super::tabs::trace() {
                eprintln!(
                    "[web] favicon {} for {} status={http_status_code}",
                    self.url, self.tab_id
                );
            }
            if !ok {
                return;
            }
            let Some(image) = image else { return };
            let Some(bytes) = png_from_cef(image) else { return };
            let pngs = super::favicon_adapt::for_dark_rail(&bytes);
            store(
                &self.tab_id,
                &self.host,
                CachedIcon {
                    tab: Arc::new(gpui::Image::from_bytes(ImageFormat::Png, pngs.tab)),
                    tile: Arc::new(gpui::Image::from_bytes(ImageFormat::Png, pngs.tile)),
                    source_px: pngs.source_px,
                },
            );
        }
    }
}

thread_local! {
    /// Rust-side retains so the callback cannot drop before Chromium
    /// calls it. The binding AddRefs when passing into `download_image`,
    /// then Drop Releases; without this extra hold the last ref can die
    /// on the same stack the request was started.
    static IN_FLIGHT: RefCell<Vec<DownloadImageCallback>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::{fallback_url, known_icon_url_on, local_mday};

    #[test]
    fn fallback_url_is_origin_favicon() {
        assert_eq!(
            fallback_url("https://github.com/zed-industries/zed").as_deref(),
            Some("https://github.com/favicon.ico")
        );
        assert_eq!(
            fallback_url("http://example.com:8080/a").as_deref(),
            Some("http://example.com:8080/favicon.ico")
        );
        assert_eq!(fallback_url("about:blank"), None);
        assert_eq!(fallback_url("https://user:pass@evil.test/"), None);
    }

    #[test]
    fn calendar_uses_the_dated_gstatic_mark() {
        assert_eq!(
            known_icon_url_on("calendar.google.com", 27).as_deref(),
            Some(
                "https://ssl.gstatic.com/calendar/images/dynamiclogo_2020q4/calendar_27_2x.png"
            )
        );
        // `calendar_01_2x.png` 404s; the CDN path has no leading zero.
        assert_eq!(
            known_icon_url_on("calendar.google.com", 1).as_deref(),
            Some(
                "https://ssl.gstatic.com/calendar/images/dynamiclogo_2020q4/calendar_1_2x.png"
            )
        );
        assert!(
            known_icon_url_on("github.com", 27).is_none(),
            "a host that serves /favicon.ico must not take a special URL"
        );
        let day = local_mday();
        assert!(
            (1..=31).contains(&day),
            "local day of month was {day}"
        );
    }
}
