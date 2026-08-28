//! Import bookmarks and history from another browser.
//!
//! CEF does not import. Chromium's importer lives in `chrome/browser/`
//! and does not even list Chrome, Brave or Arc as sources. Those three
//! are files on disk in Chromium's own format; Safari is a plist + sqlite.
//! This module reads those files. It does not copy Chromium C++.
//!
//! Detection is "the profile file exists and opens", not "the .app is in
//! /Applications". A leftover profile from an uninstalled browser still
//! imports.

mod arc;
mod chromium;
mod safari;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The four sources the picker lists, in the order the pills draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Browser {
    Brave,
    Chrome,
    Safari,
    Arc,
}

impl Browser {
    pub const ALL: [Browser; 4] = [
        Browser::Brave,
        Browser::Chrome,
        Browser::Safari,
        Browser::Arc,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Browser::Brave => "Brave",
            Browser::Chrome => "Chrome",
            Browser::Safari => "Safari",
            Browser::Arc => "Arc",
        }
    }

    pub fn wrap_title(self) -> String {
        format!("From {}", self.label())
    }
}

/// Whether a source can be imported from, on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Ready,
    Missing,
    /// The file is there and cannot be read (Safari without Full Disk
    /// Access is the usual case).
    Unreadable,
}

/// Where each source's profile lives. Production fills this from `$HOME`;
/// tests pass a scratch tree so they never touch the operator's browsers.
#[derive(Debug, Clone)]
pub struct BrowserRoots {
    pub chrome: PathBuf,
    pub brave: PathBuf,
    pub safari: PathBuf,
    pub arc_sidebar: PathBuf,
    pub arc_chromium: PathBuf,
}

impl BrowserRoots {
    /// `$HOME/Library/...` layout. `None` when HOME is unset.
    pub fn from_home() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let lib = home.join("Library");
        let support = lib.join("Application Support");
        Some(Self {
            chrome: support.join("Google/Chrome/Default"),
            brave: support.join("BraveSoftware/Brave-Browser/Default"),
            safari: lib.join("Safari"),
            arc_sidebar: support.join("Arc/StorableSidebar.json"),
            arc_chromium: support.join("Arc/User Data/Default"),
        })
    }
}

/// One node of an imported bookmark tree. `url` present makes a link,
/// absent makes a folder — the same rule [`super::bookmarks::BookmarkEvent::Added`]
/// uses, so the engine can file these without a second kind tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ImportedNode>,
}

impl ImportedNode {
    pub fn folder(title: impl Into<String>, children: Vec<ImportedNode>) -> Self {
        Self {
            title: Some(title.into()),
            url: None,
            children,
        }
    }

    pub fn link(title: Option<String>, url: String) -> Self {
        Self {
            title,
            url: Some(url),
            children: Vec::new(),
        }
    }

    pub fn is_folder(&self) -> bool {
        self.url.is_none()
    }

    pub fn link_count(&self) -> usize {
        let here = usize::from(self.url.is_some());
        here + self.children.iter().map(Self::link_count).sum::<usize>()
    }
}

/// One url from another browser's history, with the source's visit_count
/// intact so frecency is not flattened to "once".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedVisit {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub visit_count: u32,
    pub last_visit_ms: u64,
}

/// What a successful parse produced, not yet written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedPayload {
    pub source: Option<Browser>,
    pub bookmarks: Vec<ImportedNode>,
    pub history: Vec<ImportedVisit>,
}

impl ImportedPayload {
    pub fn bookmark_count(&self) -> usize {
        self.bookmarks.iter().map(ImportedNode::link_count).sum()
    }

    pub fn history_count(&self) -> usize {
        self.history.len()
    }

    /// Wrap top-level bookmarks in `From {Browser}` so a re-import is
    /// additive and deleting the folder is undo.
    pub fn wrapped(mut self) -> Self {
        let Some(source) = self.source else {
            return self;
        };
        if self.bookmarks.is_empty() {
            return self;
        }
        self.bookmarks = vec![ImportedNode::folder(source.wrap_title(), self.bookmarks)];
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("that browser's profile was not found")]
    Missing,
    #[error("could not read the profile ({0})")]
    Io(#[from] io::Error),
    #[error("could not parse the profile ({0})")]
    Parse(String),
}

impl ImportError {
    pub fn from_io(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::PermissionDenied {
            Self::Io(err)
        } else if err.kind() == io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io(err)
        }
    }
}

/// Detect each of the four sources against `roots`.
pub fn detect(roots: &BrowserRoots) -> Vec<(Browser, SourceStatus)> {
    Browser::ALL
        .into_iter()
        .map(|b| (b, detect_one(b, roots)))
        .collect()
}

pub fn detect_from_home() -> Vec<(Browser, SourceStatus)> {
    match BrowserRoots::from_home() {
        Some(roots) => detect(&roots),
        None => Browser::ALL
            .into_iter()
            .map(|b| (b, SourceStatus::Missing))
            .collect(),
    }
}

fn detect_one(browser: Browser, roots: &BrowserRoots) -> SourceStatus {
    match browser {
        Browser::Chrome => chromium_status(&roots.chrome),
        Browser::Brave => chromium_status(&roots.brave),
        Browser::Safari => safari_status(&roots.safari),
        Browser::Arc => {
            if roots.arc_sidebar.is_file() {
                match std::fs::File::open(&roots.arc_sidebar) {
                    Ok(_) => SourceStatus::Ready,
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        SourceStatus::Unreadable
                    }
                    Err(_) => SourceStatus::Unreadable,
                }
            } else {
                SourceStatus::Missing
            }
        }
    }
}

fn chromium_status(profile: &Path) -> SourceStatus {
    let bookmarks = profile.join("Bookmarks");
    let history = profile.join("History");
    if !bookmarks.exists() && !history.exists() {
        return SourceStatus::Missing;
    }
    // A file we cannot even open is unreadable; a missing half is still
    // Ready (step 2 will offer only the half that parsed).
    if bookmarks.exists() {
        if let Err(e) = std::fs::File::open(&bookmarks) {
            if e.kind() == io::ErrorKind::PermissionDenied {
                return SourceStatus::Unreadable;
            }
        }
    }
    SourceStatus::Ready
}

fn safari_status(dir: &Path) -> SourceStatus {
    let bookmarks = dir.join("Bookmarks.plist");
    let history = dir.join("History.db");
    if !bookmarks.exists() && !history.exists() {
        return SourceStatus::Missing;
    }
    if bookmarks.exists() {
        if let Err(e) = std::fs::File::open(&bookmarks) {
            if e.kind() == io::ErrorKind::PermissionDenied {
                return SourceStatus::Unreadable;
            }
        }
    }
    SourceStatus::Ready
}

/// Parse one source. A missing half (no History file, empty bar) is an
/// empty vec, not an error — the other half can still land.
pub fn load(
    browser: Browser,
    roots: &BrowserRoots,
    bookmarks: bool,
    history: bool,
) -> Result<ImportedPayload, ImportError> {
    if detect_one(browser, roots) == SourceStatus::Missing {
        return Err(ImportError::Missing);
    }
    let mut payload = match browser {
        Browser::Chrome => chromium::load(&roots.chrome, bookmarks, history)?,
        Browser::Brave => chromium::load(&roots.brave, bookmarks, history)?,
        Browser::Safari => safari::load(&roots.safari, bookmarks, history)?,
        Browser::Arc => {
            let mut p = ImportedPayload::default();
            if bookmarks {
                p.bookmarks = arc::load_sidebar(&roots.arc_sidebar)?;
            }
            if history {
                p.history = chromium::load_history(&roots.arc_chromium.join("History"))?;
            }
            p
        }
    };
    payload.source = Some(browser);
    payload.bookmarks = prune_empty_folders(payload.bookmarks);
    payload.bookmarks = dedup_links(payload.bookmarks);
    payload.history = dedup_visits(payload.history);
    Ok(payload.wrapped())
}

pub fn load_from_home(
    browser: Browser,
    bookmarks: bool,
    history: bool,
) -> Result<ImportedPayload, ImportError> {
    let roots = BrowserRoots::from_home().ok_or(ImportError::Missing)?;
    load(browser, &roots, bookmarks, history)
}

fn prune_empty_folders(nodes: Vec<ImportedNode>) -> Vec<ImportedNode> {
    nodes
        .into_iter()
        .filter_map(|mut n| {
            n.children = prune_empty_folders(n.children);
            if n.url.is_some() || !n.children.is_empty() {
                Some(n)
            } else {
                None
            }
        })
        .collect()
}

fn dedup_links(nodes: Vec<ImportedNode>) -> Vec<ImportedNode> {
    let mut seen = HashSet::new();
    fn walk(nodes: Vec<ImportedNode>, seen: &mut HashSet<String>) -> Vec<ImportedNode> {
        let mut out = Vec::new();
        for mut n in nodes {
            if let Some(url) = n.url.as_deref() {
                if !seen.insert(url.to_string()) {
                    continue;
                }
            }
            n.children = walk(n.children, seen);
            out.push(n);
        }
        out
    }
    walk(nodes, &mut seen)
}

fn dedup_visits(visits: Vec<ImportedVisit>) -> Vec<ImportedVisit> {
    let mut seen = HashSet::new();
    visits
        .into_iter()
        .filter(|v| seen.insert(v.url.clone()))
        .collect()
}

/// Skip schemes that are not a page. Chromium bookmark files hold
/// `javascript:` and `place:` rows that would file as links and then
/// fail to navigate.
fn usable_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("place:")
        || lower.starts_with("about:")
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Copy a sqlite db (and its `-wal`/`-shm` sidecars) to a temp dir so we
/// can open it while Chrome still holds the original. Chromium's importer
/// does the same copy.
fn copy_sqlite(src: &Path) -> io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "eave-import-{}-{}-{}",
        std::process::id(),
        stamp,
        src.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "db".into())
    ));
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join("db");
    std::fs::copy(src, &dest)?;
    for suffix in ["-wal", "-shm"] {
        let mut side = src.as_os_str().to_os_string();
        side.push(suffix);
        let side = PathBuf::from(side);
        if side.exists() {
            let mut to = dest.as_os_str().to_os_string();
            to.push(suffix);
            let _ = std::fs::copy(&side, PathBuf::from(to));
        }
    }
    Ok(dest)
}

fn cleanup_copied_sqlite(copied: &Path) {
    if let Some(dir) = copied.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Chrome / WebKit time → unix milliseconds.
pub(crate) fn chrome_time_to_unix_ms(micros_since_1601: i64) -> Option<u64> {
    // 11644473600 seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_MICROS: i64 = 11_644_473_600_000_000;
    let unix_micros = micros_since_1601.checked_sub(EPOCH_DIFF_MICROS)?;
    if unix_micros < 0 {
        return None;
    }
    Some((unix_micros / 1_000) as u64)
}

pub(crate) fn apple_time_to_unix_ms(seconds_since_2001: f64) -> Option<u64> {
    // 978307200 seconds between 1970-01-01 and 2001-01-01.
    const EPOCH_DIFF: f64 = 978_307_200.0;
    let unix = seconds_since_2001 + EPOCH_DIFF;
    if unix < 0.0 {
        return None;
    }
    Some((unix * 1_000.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-import-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn roots_at(base: &Path) -> BrowserRoots {
        BrowserRoots {
            chrome: base.join("chrome"),
            brave: base.join("brave"),
            safari: base.join("safari"),
            arc_sidebar: base.join("arc/StorableSidebar.json"),
            arc_chromium: base.join("arc-chromium"),
        }
    }

    #[test]
    fn a_missing_profile_is_missing_not_ready() {
        let dir = scratch();
        let roots = roots_at(&dir);
        for (b, status) in detect(&roots) {
            assert_eq!(status, SourceStatus::Missing, "{b:?}");
        }
    }

    #[test]
    fn chrome_bookmarks_file_makes_chrome_ready() {
        let dir = scratch();
        let roots = roots_at(&dir);
        std::fs::create_dir_all(&roots.chrome).unwrap();
        std::fs::write(roots.chrome.join("Bookmarks"), "{}").unwrap();
        let map: std::collections::HashMap<_, _> = detect(&roots).into_iter().collect();
        assert_eq!(map[&Browser::Chrome], SourceStatus::Ready);
        assert_eq!(map[&Browser::Brave], SourceStatus::Missing);
    }

    #[test]
    fn load_wraps_bookmarks_in_from_browser() {
        let dir = scratch();
        let roots = roots_at(&dir);
        std::fs::create_dir_all(&roots.chrome).unwrap();
        std::fs::write(
            roots.chrome.join("Bookmarks"),
            r#"{
              "roots": {
                "bookmark_bar": {
                  "type": "folder",
                  "children": [
                    {"type": "url", "name": "Rust", "url": "https://www.rust-lang.org/"},
                    {"type": "url", "name": "Rust", "url": "https://www.rust-lang.org/"}
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        let payload = load(Browser::Chrome, &roots, true, false).unwrap();
        assert_eq!(payload.bookmarks.len(), 1);
        assert_eq!(
            payload.bookmarks[0].title.as_deref(),
            Some("From Chrome")
        );
        // Same url twice in the source → once in the wrap.
        assert_eq!(payload.bookmarks[0].children.len(), 1);
        assert_eq!(payload.bookmark_count(), 1);
    }
}
