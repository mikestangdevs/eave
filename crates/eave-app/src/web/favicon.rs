//! Favicon bytes, fetched through Chromium and painted on the strip.
//!
//! Spec 06-browser D13: the url is a fact about the tab (journalled, on
//! the snapshot); the pixels are a cache. Fetch through the browser that
//! already has the page's cookies — never a second HTTP client. Decode is
//! Chromium's (`CefImage::get_as_png`); gpui then takes a PNG it already
//! knows how to paint.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cef::*;
use gpui::ImageFormat;

use super::tabs::with_browser;

/// Longest side, in device pixels, we ask Chromium to decode to.
///
/// The plate is [`crate::theme::RAIL_MARK`] = 16 logical px, and Slice T1b's
/// bookmark tile draws the same bytes at the same 16 px. The default backing
/// scale on this machine is 2, so 32 is one device pixel per surface pixel.
/// A 3x display still gets a sharp-enough downsample; asking for 48 would
/// spend the decode on pixels neither surface can show.
///
/// Briefly 48, when T1b's tile was 60 px with a 24 px mark. The tile shrank;
/// so does this.
const MAX_SIDE: u32 = 32;

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
}

/// Ask Chromium for this tab's icon, if we do not already have it.
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
    // `page_host`, not `host`: the closure below binds `b.host()`, a
    // `BrowserHost`, to that name.
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
    ASKED.with(|m| {
        m.borrow_mut().insert(tab_id.to_string(), url.to_string());
    });
    let started = with_browser(tab_id, |b| {
        let Some(host) = b.host() else {
            return false;
        };
        let mut cb = FaviconCallback::new(tab_id.to_string(), url.to_string(), page_host.clone());
        host.download_image(Some(&CefString::from(url)), 1, MAX_SIDE, 0, Some(&mut cb));
        true
    })
    .unwrap_or(false);
    if started {
        super::host::request_pump();
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
/// Does nothing when no browser is live — the tile keeps its globe and the
/// next call retries, since the host is only marked asked once one starts.
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
    let started = super::tabs::with_any_browser(|b| {
        let Some(browser_host) = b.host() else {
            if super::tabs::trace() {
                eprintln!("[web] bookmark favicon {host}: browser carries no host");
            }
            return false;
        };
        let mut cb = FaviconCallback::new(String::new(), guess.clone(), host.clone());
        browser_host.download_image(
            Some(&CefString::from(guess.as_str())),
            1,
            MAX_SIDE,
            0,
            Some(&mut cb),
        );
        true
    })
    .unwrap_or(false);
    if super::tabs::trace() {
        eprintln!("[web] bookmark favicon {guess} started={started}");
    }
    if started {
        ASKED_HOST.with(|m| {
            m.borrow_mut().insert(host, gen);
        });
        super::host::request_pump();
    }
}

fn png_from_cef(image: &mut Image) -> Option<Vec<u8>> {
    if image.is_empty() == 1 {
        return None;
    }
    let scale = if image.has_representation(2.0) == 1 {
        2.0
    } else {
        1.0
    };
    let mut w = 0i32;
    let mut h = 0i32;
    let png = image.as_png(scale, 1, Some(&mut w), Some(&mut h))?;
    let n = png.size();
    if n == 0 {
        return None;
    }
    let ptr = png.raw_data() as *const u8;
    if ptr.is_null() {
        return None;
    }
    // Copy out: CEF's buffer dies with `png`.
    Some(unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec())
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
            // 200 is a live fetch; 0 is a cache hit. Anything else is a
            // miss and the plate keeps the globe.
            if http_status_code != 200 && http_status_code != 0 {
                if super::tabs::trace() {
                    eprintln!(
                        "[web] favicon {} for {} status={http_status_code}",
                        self.url, self.tab_id
                    );
                }
                return;
            }
            let Some(image) = image else { return };
            let Some(bytes) = png_from_cef(image) else { return };
            if super::tabs::trace() {
                eprintln!(
                    "[web] favicon {} for {} {} bytes",
                    self.url,
                    self.tab_id,
                    bytes.len()
                );
            }
            store(
                &self.tab_id,
                &self.host,
                Arc::new(gpui::Image::from_bytes(ImageFormat::Png, bytes)),
            );
        }
    }
}
