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
/// The plate is [`crate::theme::RAIL_MARK`] = 16 logical px. The default
/// backing scale on this machine is 2, so 32 is one device pixel per
/// plate pixel. A 3× display still gets a sharp-enough downsample; asking
/// for 48 would spend the decode on pixels the plate cannot show.
const MAX_SIDE: u32 = 32;

thread_local! {
    static ICONS: RefCell<HashMap<String, Arc<gpui::Image>>> = RefCell::new(HashMap::new());
    /// Last URL we asked Chromium to fetch, so a snapshot that restates
    /// the same icon does not start another download.
    static ASKED: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
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
pub fn fetch(tab_id: &str, url: &str) {
    if url.is_empty() {
        return;
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
        let mut cb = FaviconCallback::new(tab_id.to_string(), url.to_string());
        host.download_image(Some(&CefString::from(url)), 1, MAX_SIDE, 0, Some(&mut cb));
        true
    })
    .unwrap_or(false);
    if started {
        super::host::request_pump();
    }
}

fn store(tab_id: &str, image: Arc<gpui::Image>) {
    ICONS.with(|m| {
        m.borrow_mut().insert(tab_id.to_string(), image);
    });
    super::host::note_chrome_change();
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
                Arc::new(gpui::Image::from_bytes(ImageFormat::Png, bytes)),
            );
        }
    }
}
