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
//! never fires and the plate stays on the globe. [`fetch`] only queues;
//! [`run_queued`] starts the download after the pump returns.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cef::*;
use gpui::ImageFormat;

use super::tabs::{with_any_browser, with_browser};

thread_local! {
    static ICONS: RefCell<HashMap<String, Arc<gpui::Image>>> = RefCell::new(HashMap::new());
    /// Last URL we asked Chromium to fetch, so a snapshot that restates
    /// the same icon does not start another download.
    static ASKED: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// The same pixels keyed by HOST, for surfaces that have a url but no
    /// browser — the rail's pinned bookmark grid (Slice T1b).
    ///
    /// A bookmark is not a tab, so it can never appear in [`ICONS`]. Keying
    /// by host also means visiting a site pays for its bookmark's tile: one
    /// download serves the tab plate and every bookmark to that host.
    static BY_HOST: RefCell<HashMap<String, Arc<gpui::Image>>> = RefCell::new(HashMap::new());
    /// Host -> the [`LOAD_GEN`] we last asked at, so a rail that re-renders
    /// sixty times a second does not start sixty downloads — and so a
    /// request Chromium dropped is asked again rather than never.
    static ASKED_HOST: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
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

/// The icon for any url, by host. `None` means "no bytes yet", never
/// "this site has no icon" — same contract as [`get`].
pub fn for_url(url: &str) -> Option<Arc<gpui::Image>> {
    let host = crate::omnibox::engine_host(url);
    if host.is_empty() {
        return None;
    }
    BY_HOST.with(|m| m.borrow().get(&host).cloned())
}

pub fn get(tab_id: &str) -> Option<Arc<gpui::Image>> {
    ICONS.with(|m| m.borrow().get(tab_id).cloned())
}

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
        if let Some(image) = get(tab_id) {
            BY_HOST.with(|m| {
                m.borrow_mut().entry(page_host.clone()).or_insert(image);
            });
        }
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
        if BY_HOST.with(|m| m.borrow().contains_key(page_host)) {
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
        host.download_image(Some(&CefString::from(url)), 0, 0, 0, Some(&mut cb));
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
        ASKED_HOST.with(|m| {
            m.borrow_mut().insert(page_host.to_string(), gen);
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
fn store(tab_id: &str, host: &str, image: Arc<gpui::Image>) {
    if !tab_id.is_empty() {
        ICONS.with(|m| {
            m.borrow_mut().insert(tab_id.to_string(), image.clone());
        });
    }
    if !host.is_empty() {
        BY_HOST.with(|m| {
            m.borrow_mut().insert(host.to_string(), image);
        });
    }
    super::host::note_chrome_change();
}

/// Ask for a BOOKMARK's icon — no tab, no browser of its own.
///
/// Routed through whatever browser happens to be live, per spec 06-browser
/// D13 (never a second HTTP client). Guesses `/favicon.ico`, because the
/// real icon url lives in a `<link rel=icon>` we would have to load the
/// page to read; a tab that later visits the host overwrites this with the
/// icon the page actually declares.
///
/// Queues rather than starting on this stack — same reason as [`fetch`].
pub fn fetch_for_url(url: &str) {
    let host = crate::omnibox::engine_host(url);
    if host.is_empty() || for_url(url).is_some() {
        return;
    }
    let gen = LOAD_GEN.with(|g| g.get());
    if ASKED_HOST.with(|m| m.borrow().get(&host).is_some_and(|asked| *asked == gen)) {
        return;
    }
    let guess = format!("https://{host}/favicon.ico");
    enqueue(String::new(), guess, host);
}

fn png_from_cef(image: &mut Image) -> Option<Vec<u8>> {
    if image.is_empty() != 0 {
        return None;
    }
    for scale in [1.0_f32, 2.0] {
        let mut w = 0i32;
        let mut h = 0i32;
        let Some(png) = image.as_png(scale, 1, Some(&mut w), Some(&mut h)) else {
            continue;
        };
        let n = png.size();
        if n == 0 {
            continue;
        }
        let mut buf = vec![0u8; n];
        let copied = png.data(Some(&mut buf), 0);
        if copied > 0 {
            buf.truncate(copied);
            return Some(buf);
        }
        let ptr = png.raw_data() as *const u8;
        if !ptr.is_null() {
            return Some(unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec());
        }
    }
    None
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
            store(
                &self.tab_id,
                &self.host,
                Arc::new(gpui::Image::from_bytes(ImageFormat::Png, bytes)),
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
    use super::fallback_url;

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
}
