//! Arc `StorableSidebar.json`.
//!
//! This is not Chromium's Bookmarks JSON. Items are interleaved
//! `id`-string then object. Spaces name a pinned container; unpinned
//! ("today") tabs are a session, not bookmarks, and are skipped.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::{usable_url, ImportError, ImportedNode};

pub fn load_sidebar(path: &Path) -> Result<Vec<ImportedNode>, ImportError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path).map_err(ImportError::from_io)?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| ImportError::Parse(e.to_string()))?;
    Ok(from_value(&v))
}

fn from_value(root: &Value) -> Vec<ImportedNode> {
    let containers = root
        .pointer("/sidebar/containers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // containers[0] is `global`; the sidebar lives in the first container
    // that has `items` + `spaces`.
    let Some(container) = containers.iter().find(|c| c.get("items").is_some()) else {
        return Vec::new();
    };
    let items = interleaved_objects(container.get("items"));
    let spaces = interleaved_objects(container.get("spaces"));
    let by_id: HashMap<String, &Value> = items
        .iter()
        .filter_map(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), *item))
        })
        .collect();

    let mut out = Vec::new();
    for space in spaces {
        let title = space
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Space")
            .to_string();
        let Some(pinned_id) = pinned_container_id(space) else {
            continue;
        };
        let children = match by_id.get(&pinned_id) {
            Some(node) => collect_children(node, &by_id),
            None => Vec::new(),
        };
        if children.is_empty() {
            continue;
        }
        out.push(ImportedNode::folder(title, children));
    }
    out
}

/// `containerIDs` is `["pinned", <id>, "unpinned", <id>]`.
fn pinned_container_id(space: &Value) -> Option<String> {
    let ids = space.get("containerIDs")?.as_array()?;
    for (i, v) in ids.iter().enumerate() {
        if v.as_str() == Some("pinned") {
            if let Some(id) = ids.get(i + 1).and_then(Value::as_str) {
                if id != "unpinned" && !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn interleaved_objects(v: Option<&Value>) -> Vec<&Value> {
    let Some(Value::Array(items)) = v else {
        return Vec::new();
    };
    items.iter().filter(|i| i.is_object()).collect()
}

fn collect_children(node: &Value, by_id: &HashMap<String, &Value>) -> Vec<ImportedNode> {
    let Some(ids) = node.get("childrenIds").and_then(Value::as_array) else {
        return Vec::new();
    };
    ids.iter()
        .filter_map(|id| id.as_str())
        .filter_map(|id| by_id.get(id).copied())
        .filter_map(|child| convert(child, by_id))
        .collect()
}

fn convert(node: &Value, by_id: &HashMap<String, &Value>) -> Option<ImportedNode> {
    let data = node.get("data")?;
    if let Some(tab) = data.get("tab") {
        let url = usable_url(tab.get("savedURL").and_then(Value::as_str).unwrap_or(""))?;
        let title = node
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                tab.get("savedTitle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .map(|s| s.to_string());
        return Some(ImportedNode::link(title, url));
    }
    if data.get("list").is_some() {
        let children = collect_children(node, by_id);
        if children.is_empty() {
            return None;
        }
        let title = node
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Folder")
            .to_string();
        return Some(ImportedNode::folder(title, children));
    }
    if data.get("splitView").is_some() || data.get("itemContainer").is_some() {
        // Not a bookmark. Flatten children into the parent.
        let children = collect_children(node, by_id);
        if children.len() == 1 {
            return children.into_iter().next();
        }
        if children.is_empty() {
            return None;
        }
        let title = node
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Split")
            .to_string();
        return Some(ImportedNode::folder(title, children));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-arc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn arc_sidebar_keeps_pinned_and_lists_and_drops_unpinned() {
        let json = r#"{
          "sidebar": {
            "containers": [
              {"global": {}},
              {
                "spaces": [
                  "space-1",
                  {
                    "id": "space-1",
                    "title": "Main",
                    "containerIDs": ["pinned", "pinned-1", "unpinned", "unpinned-1"]
                  }
                ],
                "items": [
                  "pinned-1",
                  {
                    "id": "pinned-1",
                    "title": null,
                    "childrenIds": ["list-1", "tab-pinned", "split-1"],
                    "data": {"itemContainer": {"containerType": {"spaceItems": {"_0": "space-1"}}}}
                  },
                  "unpinned-1",
                  {
                    "id": "unpinned-1",
                    "title": null,
                    "childrenIds": ["tab-today"],
                    "data": {"itemContainer": {}}
                  },
                  "list-1",
                  {
                    "id": "list-1",
                    "title": "UI comp",
                    "childrenIds": ["tab-in-list"],
                    "data": {"list": {}}
                  },
                  "tab-in-list",
                  {
                    "id": "tab-in-list",
                    "title": "Dribbble",
                    "childrenIds": [],
                    "data": {"tab": {"savedURL": "https://dribbble.com/", "savedTitle": "Dribbble"}}
                  },
                  "tab-pinned",
                  {
                    "id": "tab-pinned",
                    "title": null,
                    "childrenIds": [],
                    "data": {"tab": {"savedURL": "https://pinned.example/", "savedTitle": "Pinned"}}
                  },
                  "tab-today",
                  {
                    "id": "tab-today",
                    "title": "Today",
                    "childrenIds": [],
                    "data": {"tab": {"savedURL": "https://today.example/", "savedTitle": "Today"}}
                  },
                  "split-1",
                  {
                    "id": "split-1",
                    "title": null,
                    "childrenIds": ["tab-split"],
                    "data": {"splitView": {"layoutOrientation": "horizontal"}}
                  },
                  "tab-split",
                  {
                    "id": "tab-split",
                    "title": "Split",
                    "childrenIds": [],
                    "data": {"tab": {"savedURL": "https://split.example/"}}
                  }
                ]
              }
            ]
          }
        }"#;
        let dir = scratch();
        let path = dir.join("StorableSidebar.json");
        std::fs::write(&path, json).unwrap();
        let nodes = load_sidebar(&path).unwrap();
        assert_eq!(nodes.len(), 1, "{nodes:?}");
        assert_eq!(nodes[0].title.as_deref(), Some("Main"));
        let raw = format!("{nodes:?}");
        assert!(
            !raw.contains("today.example"),
            "unpinned today-tab leaked into bookmarks: {raw}"
        );
        assert!(raw.contains("dribbble.com"), "{raw}");
        assert!(raw.contains("pinned.example"), "{raw}");
        assert!(raw.contains("split.example"), "{raw}");
        let list = nodes[0]
            .children
            .iter()
            .find(|n| n.title.as_deref() == Some("UI comp"))
            .expect("list folder missing");
        assert_eq!(list.children[0].url.as_deref(), Some("https://dribbble.com/"));
    }
}
