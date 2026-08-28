//! Bookmarks: a hierarchy, folded from an append-only log.
//!
//! Every bookmark fact is durable — there is no transient half to this
//! store. A bookmark exists because somebody asked for it to keep existing.
//!
//! # Why a tree of ids and not a nested structure
//! The obvious model is `Folder { children: Vec<Node> }`. It is rejected
//! because a move is then a splice between two owners, and every splice is
//! a chance to drop or duplicate the moved subtree. Here a node names its
//! parent, each parent holds an ordered list of child ids, and a move is
//! two list edits with the node itself untouched. The cycle check
//! ([`BookmarkTree::would_cycle`]) is then a walk up the parent chain
//! rather than a proof about ownership.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::tabs::{normalize_tab_title, normalize_url};

pub type BookmarkId = String;

/// What a node is. The two kinds are deliberately one enum rather than two
/// stores: they share one ordering inside a folder, and two stores would
/// need a rule for how they interleave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BookmarkKind {
    Folder,
    Link { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: BookmarkId,
    /// `None` = a top-level node. There is no synthetic root node: a root
    /// that exists in the data is a root that can be renamed, moved, or
    /// deleted by a caller that did not realise it was special.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<BookmarkId>,
    /// `None` = unnamed. A link with no title renders as its url by
    /// whatever draws it; this store does not substitute one, so that
    /// "named after the page" and "named by the operator" stay
    /// distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub kind: BookmarkKind,
    pub created_ts_ms: u64,
}

impl Bookmark {
    pub fn url(&self) -> Option<&str> {
        match &self.kind {
            BookmarkKind::Link { url } => Some(url.as_str()),
            BookmarkKind::Folder => None,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, BookmarkKind::Folder)
    }
}

/// The durable vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BookmarkEvent {
    Added {
        id: BookmarkId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<BookmarkId>,
        #[serde(default)]
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// `Some` = a link, `None` = a folder. One field rather than a
        /// nested kind enum, because "a bookmark with a url" is what every
        /// caller actually has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        ts_ms: u64,
    },
    /// Remove a node. A folder takes its whole subtree with it — the fold
    /// derives that rather than the log enumerating it, so the log stays
    /// one line per operator action and the semantics live in one place.
    Removed { id: BookmarkId },
    Moved {
        id: BookmarkId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<BookmarkId>,
        #[serde(default)]
        index: usize,
    },
    Renamed {
        id: BookmarkId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Point an existing link somewhere else. Kept apart from
    /// [`BookmarkEvent::Renamed`] because a title and an address are
    /// different things to get wrong, and an edit box that changed both at
    /// once has no way to say which one the operator meant.
    Retargeted { id: BookmarkId, url: String },
}

/// The whole hierarchy. The fold is total: an event naming an unknown id is
/// a no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkTree {
    pub nodes: HashMap<BookmarkId, Bookmark>,
    /// Ordered children per folder id.
    pub children: HashMap<BookmarkId, Vec<BookmarkId>>,
    /// Ordered top-level nodes.
    pub roots: Vec<BookmarkId>,
}

impl BookmarkTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Would making `id` a child of `parent` create a cycle — or make a
    /// node its own ancestor? Also true when `parent` is not a folder,
    /// because a link cannot hold children.
    pub fn would_cycle(&self, id: &BookmarkId, parent: Option<&BookmarkId>) -> bool {
        let Some(parent) = parent else {
            return false;
        };
        if parent == id {
            return true;
        }
        match self.nodes.get(parent) {
            Some(p) if !p.is_folder() => return true,
            None => return true,
            _ => {}
        }
        // Walk up from the proposed parent; meeting `id` means `id` is an
        // ancestor of its own new parent.
        let mut cur = Some(parent.clone());
        let mut guard = 0usize;
        while let Some(c) = cur {
            if &c == id {
                return true;
            }
            // A malformed log could in principle hold a pre-existing cycle;
            // bound the walk rather than hang. The bound is the node count,
            // past which any chain must have repeated a node.
            guard += 1;
            if guard > self.nodes.len() + 1 {
                return true;
            }
            cur = self.nodes.get(&c).and_then(|n| n.parent.clone());
        }
        false
    }

    pub fn apply(&mut self, event: &BookmarkEvent) -> bool {
        match event {
            BookmarkEvent::Added {
                id,
                parent,
                index,
                title,
                url,
                ts_ms,
            } => {
                if self.nodes.contains_key(id) {
                    return false;
                }
                // A parent that is missing, or is a link, would hide the
                // new node where nothing can draw it. Land it at the top
                // level instead of dropping it — a bookmark the operator
                // asked for must exist somewhere findable.
                let parent = parent
                    .as_ref()
                    .filter(|p| self.nodes.get(*p).is_some_and(|n| n.is_folder()))
                    .cloned();
                let kind = match url.as_deref().and_then(normalize_url) {
                    Some(u) => BookmarkKind::Link { url: u },
                    None => BookmarkKind::Folder,
                };
                self.nodes.insert(
                    id.clone(),
                    Bookmark {
                        id: id.clone(),
                        parent: parent.clone(),
                        title: title.as_deref().and_then(normalize_tab_title),
                        kind,
                        created_ts_ms: *ts_ms,
                    },
                );
                self.children.entry(id.clone()).or_default();
                self.insert_into(id, parent, *index);
                true
            }
            BookmarkEvent::Removed { id } => {
                if !self.nodes.contains_key(id) {
                    return false;
                }
                let parent = self.nodes.get(id).and_then(|n| n.parent.clone());
                self.unlink(id, parent);
                // Depth-first removal of the subtree.
                let mut stack = vec![id.clone()];
                while let Some(node) = stack.pop() {
                    if let Some(kids) = self.children.remove(&node) {
                        stack.extend(kids);
                    }
                    self.nodes.remove(&node);
                }
                true
            }
            BookmarkEvent::Moved { id, parent, index } => {
                if !self.nodes.contains_key(id) {
                    return false;
                }
                if self.would_cycle(id, parent.as_ref()) {
                    return false;
                }
                let old_parent = self.nodes.get(id).and_then(|n| n.parent.clone());
                let same_container = old_parent.as_ref() == parent.as_ref();
                let before = self.position_of(id);
                self.unlink(id, old_parent);
                self.insert_into(id, parent.clone(), *index);
                if let Some(n) = self.nodes.get_mut(id) {
                    n.parent = parent.clone();
                }
                // A move that landed the node exactly where it started is
                // not a change, and re-broadcasting the whole tree for it
                // would be noise.
                !(same_container && before == self.position_of(id))
            }
            BookmarkEvent::Renamed { id, title } => {
                let title = title.as_deref().and_then(normalize_tab_title);
                let Some(n) = self.nodes.get_mut(id) else {
                    return false;
                };
                if n.title == title {
                    return false;
                }
                n.title = title;
                true
            }
            BookmarkEvent::Retargeted { id, url } => {
                let Some(u) = normalize_url(url) else {
                    return false;
                };
                let Some(n) = self.nodes.get_mut(id) else {
                    return false;
                };
                // Retargeting a folder would turn it into a link and orphan
                // its children. Refuse; the caller meant a different node.
                if n.is_folder() {
                    return false;
                }
                if n.url() == Some(u.as_str()) {
                    return false;
                }
                n.kind = BookmarkKind::Link { url: u };
                true
            }
        }
    }

    fn position_of(&self, id: &BookmarkId) -> Option<usize> {
        let parent = self.nodes.get(id).and_then(|n| n.parent.clone());
        let list = match &parent {
            Some(p) => self.children.get(p)?,
            None => &self.roots,
        };
        list.iter().position(|c| c == id)
    }

    fn unlink(&mut self, id: &BookmarkId, parent: Option<BookmarkId>) {
        match parent {
            Some(p) => {
                if let Some(list) = self.children.get_mut(&p) {
                    list.retain(|c| c != id);
                }
            }
            None => self.roots.retain(|c| c != id),
        }
    }

    fn insert_into(&mut self, id: &BookmarkId, parent: Option<BookmarkId>, index: usize) {
        let list = match &parent {
            Some(p) => self.children.entry(p.clone()).or_default(),
            None => &mut self.roots,
        };
        let at = index.min(list.len());
        list.insert(at, id.clone());
    }

    /// Depth-first walk in display order, yielding each node with its depth.
    pub fn walk(&self) -> Vec<(usize, &Bookmark)> {
        let mut out = Vec::with_capacity(self.nodes.len());
        // (id, depth), pushed so the first root is visited first.
        let mut stack: Vec<(BookmarkId, usize)> =
            self.roots.iter().rev().map(|r| (r.clone(), 0)).collect();
        // The node count bounds the walk: a cycle cannot make it run
        // forever, it can only make it stop early with fewer nodes than
        // exist — which is visible, unlike a hang.
        while let Some((id, depth)) = stack.pop() {
            if out.len() > self.nodes.len() {
                break;
            }
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            out.push((depth, node));
            if let Some(kids) = self.children.get(&id) {
                for k in kids.iter().rev() {
                    stack.push((k.clone(), depth + 1));
                }
            }
        }
        out
    }

    /// Every link whose url matches, at any depth. What "is this page
    /// bookmarked?" asks.
    pub fn links_to(&self, url: &str) -> Vec<&Bookmark> {
        let mut out: Vec<&Bookmark> = self
            .nodes
            .values()
            .filter(|n| n.url() == Some(url))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(t: &mut BookmarkTree, id: &str, parent: Option<&str>, url: Option<&str>) -> bool {
        t.apply(&BookmarkEvent::Added {
            id: id.into(),
            parent: parent.map(str::to_string),
            index: usize::MAX,
            title: Some(id.to_string()),
            url: url.map(str::to_string),
            ts_ms: 0,
        })
    }

    /// The same totality rule the tab fold keeps, and for the same reason:
    /// it is what lets `log.rs` skip an unreadable line.
    #[test]
    fn every_event_naming_an_unknown_id_is_a_no_op() {
        let mut t = BookmarkTree::new();
        for e in [
            BookmarkEvent::Removed { id: "ghost".into() },
            BookmarkEvent::Moved {
                id: "ghost".into(),
                parent: None,
                index: 0,
            },
            BookmarkEvent::Renamed {
                id: "ghost".into(),
                title: Some("x".into()),
            },
            BookmarkEvent::Retargeted {
                id: "ghost".into(),
                url: "https://x/".into(),
            },
        ] {
            assert!(!t.apply(&e), "{e:?} claimed to change an empty tree");
        }
        assert!(t.is_empty());
    }

    #[test]
    fn a_folder_takes_its_whole_subtree_when_removed() {
        let mut t = BookmarkTree::new();
        add(&mut t, "root", None, None);
        add(&mut t, "mid", Some("root"), None);
        add(&mut t, "leaf", Some("mid"), Some("https://x/"));
        add(&mut t, "sibling", None, Some("https://y/"));
        assert_eq!(t.len(), 4);

        assert!(t.apply(&BookmarkEvent::Removed { id: "root".into() }));
        assert_eq!(t.len(), 1, "the subtree survived its folder");
        assert_eq!(t.roots, vec!["sibling".to_string()]);
        // No dangling child lists left behind.
        assert!(!t.children.contains_key("mid"));
    }

    /// A folder cannot be put inside itself, or inside its own descendant,
    /// and a bookmark cannot be put inside a link.
    #[test]
    fn a_move_that_would_make_a_cycle_is_refused() {
        let mut t = BookmarkTree::new();
        add(&mut t, "a", None, None);
        add(&mut t, "b", Some("a"), None);
        add(&mut t, "link", None, Some("https://x/"));

        assert!(t.would_cycle(&"a".to_string(), Some(&"a".to_string())));
        assert!(t.would_cycle(&"a".to_string(), Some(&"b".to_string())));
        assert!(t.would_cycle(&"b".to_string(), Some(&"link".to_string())));
        assert!(!t.would_cycle(&"b".to_string(), Some(&"a".to_string())));
        assert!(!t.would_cycle(&"b".to_string(), None));

        assert!(!t.apply(&BookmarkEvent::Moved {
            id: "a".into(),
            parent: Some("b".into()),
            index: 0
        }));
        assert_eq!(t.nodes["a"].parent, None, "the cycle move landed anyway");
        assert_eq!(t.children["a"], vec!["b".to_string()]);
    }

    /// A move that lands a node exactly where it already was is not a
    /// change — re-broadcasting the whole tree for it would be noise.
    #[test]
    fn a_move_to_where_it_already_is_reports_no_change() {
        let mut t = BookmarkTree::new();
        add(&mut t, "a", None, Some("https://a/"));
        add(&mut t, "b", None, Some("https://b/"));
        assert!(!t.apply(&BookmarkEvent::Moved {
            id: "a".into(),
            parent: None,
            index: 0
        }));
        assert!(t.apply(&BookmarkEvent::Moved {
            id: "a".into(),
            parent: None,
            index: 1
        }));
        assert_eq!(t.roots, vec!["b".to_string(), "a".to_string()]);
    }

    /// Retargeting a folder would turn it into a link and orphan its
    /// children. Refused.
    #[test]
    fn a_folder_cannot_be_retargeted_into_a_link() {
        let mut t = BookmarkTree::new();
        add(&mut t, "f", None, None);
        add(&mut t, "kid", Some("f"), Some("https://x/"));
        assert!(!t.apply(&BookmarkEvent::Retargeted {
            id: "f".into(),
            url: "https://elsewhere/".into()
        }));
        assert!(t.nodes["f"].is_folder());
        assert_eq!(t.children["f"], vec!["kid".to_string()]);
    }

    /// A node added under a missing parent, or under a LINK, still exists —
    /// at the top level, where something can draw it. A bookmark somebody
    /// asked for must not land where nothing renders it.
    #[test]
    fn a_bookmark_added_under_an_impossible_parent_lands_at_the_top() {
        let mut t = BookmarkTree::new();
        add(&mut t, "link", None, Some("https://x/"));
        add(&mut t, "orphan", Some("nope"), Some("https://y/"));
        add(&mut t, "under-a-link", Some("link"), Some("https://z/"));
        assert_eq!(t.nodes["orphan"].parent, None);
        assert_eq!(t.nodes["under-a-link"].parent, None);
        assert_eq!(t.walk().len(), 3, "a bookmark went somewhere nothing draws");
    }

    /// The walk is depth-first in display order, which is what makes the
    /// wire snapshot deterministic.
    #[test]
    fn the_walk_is_depth_first_in_display_order() {
        let mut t = BookmarkTree::new();
        add(&mut t, "f1", None, None);
        add(&mut t, "f1a", Some("f1"), Some("https://a/"));
        add(&mut t, "f1b", Some("f1"), Some("https://b/"));
        add(&mut t, "top", None, Some("https://t/"));
        let order: Vec<(usize, &str)> = t
            .walk()
            .into_iter()
            .map(|(d, b)| (d, b.id.as_str()))
            .collect();
        assert_eq!(order, vec![(0, "f1"), (1, "f1a"), (1, "f1b"), (0, "top")]);
    }

    /// "Is this page bookmarked?" — the query a star button asks.
    #[test]
    fn links_to_finds_every_bookmark_of_a_url_at_any_depth() {
        let mut t = BookmarkTree::new();
        add(&mut t, "f", None, None);
        add(&mut t, "deep", Some("f"), Some("https://x/"));
        add(&mut t, "shallow", None, Some("https://x/"));
        add(&mut t, "other", None, Some("https://y/"));
        let hits: Vec<&str> = t
            .links_to("https://x/")
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(hits, vec!["deep", "shallow"]);
        assert!(t.links_to("https://nope/").is_empty());
    }

    /// Serde defaults: an event from a build predating a field parses, and
    /// the plain case writes no key.
    #[test]
    fn an_older_builds_event_still_parses() {
        let old = r#"{"kind":"added","id":"b","ts_ms":3}"#;
        let e: BookmarkEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            e,
            BookmarkEvent::Added {
                id: "b".into(),
                parent: None,
                index: 0,
                title: None,
                url: None,
                ts_ms: 3,
            },
            "a bookmark with no url is a FOLDER, which is what an old \
             event with no url meant"
        );
        let back = serde_json::to_string(&e).unwrap();
        for key in ["parent", "title", "url"] {
            assert!(!back.contains(key), "{back}");
        }
    }
}
