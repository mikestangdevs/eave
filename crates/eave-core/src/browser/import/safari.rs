//! Safari `Bookmarks.plist` + `History.db`.

use std::path::Path;

use plist::Value;
use rusqlite::Connection;

use super::{
    apple_time_to_unix_ms, cleanup_copied_sqlite, copy_sqlite, usable_url, ImportError,
    ImportedNode, ImportedPayload, ImportedVisit,
};

pub fn load(dir: &Path, bookmarks: bool, history: bool) -> Result<ImportedPayload, ImportError> {
    let mut payload = ImportedPayload::default();
    if bookmarks {
        payload.bookmarks = load_bookmarks(&dir.join("Bookmarks.plist"))?;
    }
    if history {
        payload.history = load_history(&dir.join("History.db"))?;
    }
    Ok(payload)
}

pub fn load_bookmarks(path: &Path) -> Result<Vec<ImportedNode>, ImportError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value = Value::from_file(path).map_err(|e| {
        let io_like = io_kind(&e);
        if io_like == Some(std::io::ErrorKind::PermissionDenied) {
            ImportError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                e.to_string(),
            ))
        } else if io_like == Some(std::io::ErrorKind::NotFound) {
            ImportError::Missing
        } else {
            ImportError::Parse(e.to_string())
        }
    })?;
    Ok(walk_root(&value))
}

fn io_kind(err: &plist::Error) -> Option<std::io::ErrorKind> {
    // plist::Error doesn't expose io::ErrorKind directly on every version;
    // the Display is enough to tell PermissionDenied apart from parse.
    let s = err.to_string().to_ascii_lowercase();
    if s.contains("permission") {
        Some(std::io::ErrorKind::PermissionDenied)
    } else if s.contains("not found") || s.contains("no such file") {
        Some(std::io::ErrorKind::NotFound)
    } else {
        None
    }
}

fn walk_root(value: &Value) -> Vec<ImportedNode> {
    let Some(children) = dict_array(value, "Children") else {
        return Vec::new();
    };
    let mut bar = Vec::new();
    let mut menu = Vec::new();
    let mut reading = Vec::new();
    let mut loose = Vec::new();
    for child in children {
        let title = dict_string(child, "Title").unwrap_or("");
        let kind = dict_string(child, "WebBookmarkType").unwrap_or("");
        if kind == "WebBookmarkTypeProxy" {
            continue;
        }
        match title {
            "BookmarksBar" => bar.extend(walk_children(child)),
            "BookmarksMenu" => menu.extend(walk_children(child)),
            "com.apple.ReadingList" => reading.extend(walk_children(child)),
            _ => {
                if let Some(n) = convert(child) {
                    loose.push(n);
                }
            }
        }
    }
    let mut out = bar;
    if !menu.is_empty() {
        out.push(ImportedNode::folder("Bookmarks Menu", menu));
    }
    if !reading.is_empty() {
        out.push(ImportedNode::folder("Reading List", reading));
    }
    out.extend(loose);
    out
}

fn walk_children(node: &Value) -> Vec<ImportedNode> {
    dict_array(node, "Children")
        .unwrap_or(&[])
        .iter()
        .filter_map(convert)
        .collect()
}

fn convert(node: &Value) -> Option<ImportedNode> {
    let kind = dict_string(node, "WebBookmarkType").unwrap_or("");
    if kind == "WebBookmarkTypeProxy" {
        return None;
    }
    if kind == "WebBookmarkTypeLeaf" || dict_string(node, "URLString").is_some() {
        let url = usable_url(dict_string(node, "URLString").unwrap_or(""))?;
        let title = dict_dict(node, "URIDictionary")
            .and_then(|d| dict_string(d, "title"))
            .or_else(|| dict_string(node, "Title"))
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        return Some(ImportedNode::link(title, url));
    }
    let children = walk_children(node);
    if children.is_empty() {
        return None;
    }
    let title = dict_string(node, "Title")
        .filter(|s| !s.trim().is_empty() && *s != "BookmarksBar")
        .map(|s| s.to_string());
    Some(ImportedNode {
        title,
        url: None,
        children,
    })
}

fn dict_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Dictionary(d) => d.get(key).and_then(Value::as_string),
        _ => None,
    }
}

fn dict_array<'a>(value: &'a Value, key: &str) -> Option<&'a [Value]> {
    match value {
        Value::Dictionary(d) => d.get(key).and_then(Value::as_array).map(|a| a.as_slice()),
        _ => None,
    }
}

fn dict_dict<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Dictionary(d) => d.get(key),
        _ => None,
    }
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
            "SELECT i.url,
                    (SELECT v.title FROM history_visits v
                      WHERE v.history_item = i.id AND v.title IS NOT NULL
                      ORDER BY v.visit_time DESC LIMIT 1),
                    COUNT(v.id),
                    MAX(v.visit_time)
             FROM history_items i
             JOIN history_visits v ON v.history_item = i.id
             WHERE i.url IS NOT NULL AND i.url != ''
             GROUP BY i.id",
        )
        .map_err(|e| ImportError::Parse(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })
        .map_err(|e| ImportError::Parse(e.to_string()))?;

    let mut out = Vec::new();
    for row in rows {
        let (url, title, visit_count, visit_time) =
            row.map_err(|e| ImportError::Parse(e.to_string()))?;
        let Some(url) = usable_url(&url) else {
            continue;
        };
        let Some(last_visit_ms) = apple_time_to_unix_ms(visit_time) else {
            continue;
        };
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
            visit_count: visit_count.max(1) as u32,
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
        let dir = std::env::temp_dir().join(format!("eave-sf-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn safari_plist_maps_bar_reading_list_and_skips_history_proxy() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Children</key>
  <array>
    <dict>
      <key>Title</key><string>History</string>
      <key>WebBookmarkType</key><string>WebBookmarkTypeProxy</string>
    </dict>
    <dict>
      <key>Title</key><string>BookmarksBar</string>
      <key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
      <key>Children</key>
      <array>
        <dict>
          <key>WebBookmarkType</key><string>WebBookmarkTypeLeaf</string>
          <key>URLString</key><string>https://bar.example/</string>
          <key>URIDictionary</key>
          <dict><key>title</key><string>Bar</string></dict>
        </dict>
      </array>
    </dict>
    <dict>
      <key>Title</key><string>BookmarksMenu</string>
      <key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
      <key>Children</key><array/>
    </dict>
    <dict>
      <key>Title</key><string>com.apple.ReadingList</string>
      <key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
      <key>Children</key>
      <array>
        <dict>
          <key>WebBookmarkType</key><string>WebBookmarkTypeLeaf</string>
          <key>URLString</key><string>https://read.example/</string>
          <key>URIDictionary</key>
          <dict><key>title</key><string>Read later</string></dict>
        </dict>
      </array>
    </dict>
  </array>
  <key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
</dict>
</plist>"#;
        let dir = scratch();
        let path = dir.join("Bookmarks.plist");
        std::fs::write(&path, xml).unwrap();
        let nodes = load_bookmarks(&path).unwrap();
        let raw = format!("{nodes:?}");
        assert!(
            !raw.contains("History") || nodes.iter().all(|n| n.title.as_deref() != Some("History")),
            "History proxy leaked: {raw}"
        );
        assert_eq!(nodes[0].url.as_deref(), Some("https://bar.example/"));
        let reading = nodes
            .iter()
            .find(|n| n.title.as_deref() == Some("Reading List"))
            .expect("Reading List folder missing");
        assert_eq!(
            reading.children[0].url.as_deref(),
            Some("https://read.example/")
        );
        assert!(
            nodes.iter().all(|n| n.title.as_deref() != Some("Bookmarks Menu")),
            "empty Bookmarks Menu should have been dropped: {raw}"
        );
    }

    #[test]
    fn safari_history_db_maps_counts() {
        let dir = scratch();
        let path = dir.join("History.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE history_items (
                id INTEGER PRIMARY KEY, url TEXT, visit_count INTEGER
             );
             CREATE TABLE history_visits (
                id INTEGER PRIMARY KEY,
                history_item INTEGER,
                visit_time REAL,
                title TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_items (id, url, visit_count) VALUES (1, ?1, 3)",
            rusqlite::params!["https://safari.example/"],
        )
        .unwrap();
        // Apple absolute ≈ 2024.
        conn.execute(
            "INSERT INTO history_visits (history_item, visit_time, title)
             VALUES (1, 750000000.0, 'Safari page')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_visits (history_item, visit_time, title)
             VALUES (1, 750000100.0, 'Safari page')",
            [],
        )
        .unwrap();
        drop(conn);

        let visits = load_history(&path).unwrap();
        assert_eq!(visits.len(), 1, "{visits:?}");
        assert_eq!(visits[0].url, "https://safari.example/");
        assert_eq!(visits[0].visit_count, 2);
        assert_eq!(visits[0].title.as_deref(), Some("Safari page"));
        assert!(visits[0].last_visit_ms > 1_700_000_000_000);
    }
}
