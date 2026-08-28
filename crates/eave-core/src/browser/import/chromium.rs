//! Chromium-profile bookmarks JSON + History sqlite.
//!
//! Shared by Chrome, Brave, and Arc's `User Data/Default` history. Arc's
//! sidebar is a different file and lives in `arc.rs`.

use std::path::Path;

use rusqlite::Connection;
use serde::Deserialize;

use super::{
    chrome_time_to_unix_ms, cleanup_copied_sqlite, copy_sqlite, usable_url, ImportError,
    ImportedNode, ImportedPayload, ImportedVisit,
};

#[derive(Debug, Deserialize)]
struct BookmarksFile {
    roots: Roots,
}

#[derive(Debug, Deserialize)]
struct Roots {
    #[serde(default)]
    bookmark_bar: Option<RawNode>,
    #[serde(default)]
    other: Option<RawNode>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    children: Vec<RawNode>,
}

pub fn load(profile: &Path, bookmarks: bool, history: bool) -> Result<ImportedPayload, ImportError> {
    let mut payload = ImportedPayload::default();
    if bookmarks {
        payload.bookmarks = load_bookmarks(&profile.join("Bookmarks"))?;
    }
    if history {
        payload.history = load_history(&profile.join("History"))?;
    }
    Ok(payload)
}

pub fn load_bookmarks(path: &Path) -> Result<Vec<ImportedNode>, ImportError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path).map_err(ImportError::from_io)?;
    let file: BookmarksFile =
        serde_json::from_str(&raw).map_err(|e| ImportError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(bar) = file.roots.bookmark_bar {
        out.extend(convert_children(bar.children));
    }
    if let Some(other) = file.roots.other {
        let kids = convert_children(other.children);
        if !kids.is_empty() {
            let title = other
                .name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Other Bookmarks".into());
            out.push(ImportedNode::folder(title, kids));
        }
    }
    Ok(out)
}

fn convert_children(nodes: Vec<RawNode>) -> Vec<ImportedNode> {
    nodes.into_iter().filter_map(convert_node).collect()
}

fn convert_node(node: RawNode) -> Option<ImportedNode> {
    let kind = node.kind.as_deref().unwrap_or("");
    if kind == "url" || node.url.is_some() {
        let url = usable_url(node.url.as_deref().unwrap_or(""))?;
        let title = node.name.filter(|s| !s.trim().is_empty());
        return Some(ImportedNode::link(title, url));
    }
    let children = convert_children(node.children);
    if children.is_empty() {
        return None;
    }
    let title = node.name.filter(|s| !s.trim().is_empty());
    Some(ImportedNode {
        title,
        url: None,
        children,
    })
}

pub fn load_history(path: &Path) -> Result<Vec<ImportedVisit>, ImportError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let copied = copy_sqlite(path).map_err(ImportError::from_io)?;
    let result = read_history(&copied);
    cleanup_copied_sqlite(&copied);
    result
}

fn read_history(path: &Path) -> Result<Vec<ImportedVisit>, ImportError> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| ImportError::Parse(e.to_string()))?;
    let mut stmt = conn
        .prepare(
            "SELECT url, title, visit_count, last_visit_time, hidden
             FROM urls",
        )
        .map_err(|e| ImportError::Parse(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        })
        .map_err(|e| ImportError::Parse(e.to_string()))?;

    let mut out = Vec::new();
    for row in rows {
        let (url, title, visit_count, last_visit, hidden) =
            row.map_err(|e| ImportError::Parse(e.to_string()))?;
        if hidden != 0 {
            continue;
        }
        let Some(url) = usable_url(&url) else {
            continue;
        };
        let Some(last_visit_ms) = chrome_time_to_unix_ms(last_visit) else {
            continue;
        };
        let visit_count = visit_count.max(1) as u32;
        let title = title.and_then(|t| {
            let t = t.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        });
        out.push(ImportedVisit {
            url,
            title,
            visit_count,
            last_visit_ms,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn scratch() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-cr-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chromium_bookmarks_bar_and_other_map() {
        let json = r#"{
          "roots": {
            "bookmark_bar": {
              "type": "folder",
              "name": "Bookmarks Bar",
              "children": [
                {"type": "url", "name": "Rust", "url": "https://www.rust-lang.org/"},
                {"type": "folder", "name": "Work", "children": [
                  {"type": "url", "name": "Docs", "url": "https://doc.rust-lang.org/"}
                ]}
              ]
            },
            "other": {
              "type": "folder",
              "name": "Other Bookmarks",
              "children": [
                {"type": "url", "name": "Hidden", "url": "https://other.example/"}
              ]
            },
            "synced": {
              "type": "folder",
              "name": "Mobile Bookmarks",
              "children": [
                {"type": "url", "name": "Phone", "url": "https://mobile.example/"}
              ]
            }
          }
        }"#;
        let dir = scratch();
        let path = dir.join("Bookmarks");
        std::fs::write(&path, json).unwrap();
        let nodes = load_bookmarks(&path).unwrap();
        assert_eq!(nodes.len(), 3, "{nodes:?}");
        assert_eq!(nodes[0].title.as_deref(), Some("Rust"));
        assert_eq!(nodes[0].url.as_deref(), Some("https://www.rust-lang.org/"));
        assert_eq!(nodes[1].title.as_deref(), Some("Work"));
        assert_eq!(nodes[1].children.len(), 1);
        assert_eq!(nodes[2].title.as_deref(), Some("Other Bookmarks"));
        assert_eq!(nodes[2].children[0].url.as_deref(), Some("https://other.example/"));
        let raw = format!("{nodes:?}");
        assert!(
            !raw.contains("mobile.example"),
            "synced/mobile bookmarks leaked: {raw}"
        );
    }

    #[test]
    fn javascript_bookmarks_are_dropped() {
        let json = r#"{
          "roots": {
            "bookmark_bar": {
              "type": "folder",
              "children": [
                {"type": "url", "name": "js", "url": "javascript:void(0)"},
                {"type": "url", "name": "ok", "url": "https://ok.example/"}
              ]
            }
          }
        }"#;
        let dir = scratch();
        let path = dir.join("Bookmarks");
        std::fs::write(&path, json).unwrap();
        let nodes = load_bookmarks(&path).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].url.as_deref(), Some("https://ok.example/"));
    }

    #[test]
    fn chromium_history_sqlite_maps_visit_count() {
        let dir = scratch();
        let path = dir.join("History");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE urls (
                id INTEGER PRIMARY KEY,
                url TEXT,
                title TEXT,
                visit_count INTEGER,
                typed_count INTEGER,
                last_visit_time INTEGER,
                hidden INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        // 13300000000000000 µs since 1601 ≈ 2022-ish unix.
        conn.execute(
            "INSERT INTO urls (url, title, visit_count, last_visit_time, hidden)
             VALUES (?1, ?2, 7, 13300000000000000, 0)",
            rusqlite::params!["https://hist.example/", "Hist",],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO urls (url, title, visit_count, last_visit_time, hidden)
             VALUES (?1, ?2, 1, 13300000000000000, 1)",
            rusqlite::params!["https://hidden.example/", "Hidden"],
        )
        .unwrap();
        drop(conn);

        let visits = load_history(&path).unwrap();
        assert_eq!(visits.len(), 1, "{visits:?}");
        assert_eq!(visits[0].url, "https://hist.example/");
        assert_eq!(visits[0].visit_count, 7);
        assert_eq!(visits[0].title.as_deref(), Some("Hist"));
        assert!(visits[0].last_visit_ms > 1_600_000_000_000);
    }
}
