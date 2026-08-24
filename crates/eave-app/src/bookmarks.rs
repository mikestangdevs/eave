//! Slice T1 — the bookmarks bar's **model** half.
//!
//! Pure, for the reason `strip` and `omnibox` are: which nodes are on the
//! bar, what each one is called, and which children a folder opens are all
//! decidable without a window, and a rule that can only be exercised by
//! opening one is a rule that ships on argument alone.
//!
//! # The tree arrives flat, and that is deliberate
//!
//! [`eave_wire::BookmarksSnapshot`] is a depth-first list of
//! [`eave_wire::BookmarkNode`]s carrying a `depth`, not a nested structure.
//! B1's reason is on the wire type: JSON object key order is undefined, so
//! putting the store's maps on the wire would make the tree's order depend
//! on hashing. Depth-first IS the drawing order, so this module reads runs
//! out of that list rather than walking pointers.

use eave_wire::{BookmarkKind, BookmarksSnapshot};

/// One bookmark, reduced to what the bar paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarItem {
    pub id: String,
    /// The line on the chip.
    pub label: String,
    /// `None` on a folder.
    pub url: Option<String>,
    /// How many children a folder holds. Zero on a folder is a fact — an
    /// empty folder — and the renderer must not draw it as a link.
    pub children: usize,
}

impl BarItem {
    pub fn is_folder(&self) -> bool {
        self.url.is_none()
    }
}

/// What a bookmark with no name of its own is called.
///
/// The host, not the whole url: `https://github.com/eave/pulls?tab=x` in a
/// chip is mostly punctuation, and the store deliberately leaves `title`
/// `None` rather than substituting one so that "named after the page" and
/// "named by the operator" stay distinguishable (`bookmarks.rs`'s own
/// note). Choosing the stand-in is therefore this module's job.
///
/// A url with no host at all keeps the url — a chip labelled with the empty
/// string is a bookmark nobody can identify or remove.
pub fn fallback_label(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");
    if host.is_empty() {
        url.to_string()
    } else {
        host.to_string()
    }
}

fn item(node: &eave_wire::BookmarkNode) -> BarItem {
    let url = match &node.bookmark.kind {
        BookmarkKind::Link { url } => Some(url.clone()),
        BookmarkKind::Folder => None,
    };
    let label = match (&node.bookmark.title, &url) {
        (Some(t), _) if !t.trim().is_empty() => t.clone(),
        (_, Some(url)) => fallback_label(url),
        // A folder the operator never named. Not "folder" — that is the
        // same class of stand-in as the hex session id the operator ruled
        // against twice, and unlike a link there is no address to fall back
        // on. An empty name is at least visibly unfinished.
        _ => String::new(),
    };
    BarItem {
        id: node.bookmark.id.clone(),
        label,
        url,
        children: node.children,
    }
}

/// What sits on the bar: the top-level nodes, in order.
pub fn bar(snapshot: &BookmarksSnapshot) -> Vec<BarItem> {
    snapshot
        .nodes
        .iter()
        .filter(|n| n.depth == 0)
        .map(item)
        .collect()
}

/// The top-level LINKS — what the rail's pinned grid paints.
///
/// Slice T1b moved bookmarks off the page and into the rail, where the two
/// kinds stop being interchangeable: a link becomes an icon tile with no
/// label, a folder becomes a titled row under the projects. Splitting them
/// here rather than filtering at each call site keeps the two renderers
/// from disagreeing about which is which.
pub fn pinned(snapshot: &BookmarksSnapshot) -> Vec<BarItem> {
    bar(snapshot)
        .into_iter()
        .filter(|i| !i.is_folder())
        .collect()
}

/// The top-level FOLDERS — the titled rows that sit under the projects.
pub fn folders(snapshot: &BookmarksSnapshot) -> Vec<BarItem> {
    bar(snapshot)
        .into_iter()
        .filter(BarItem::is_folder)
        .collect()
}

/// What a folder opens: its DIRECT children, in order.
///
/// Direct, not its whole subtree — a depth-first list makes a grandchild
/// look exactly like a child to anything that only tests "comes after the
/// folder". The run ends at the first node whose depth returns to the
/// folder's own, which is what makes this one pass rather than two.
pub fn children_of(snapshot: &BookmarksSnapshot, folder_id: &str) -> Vec<BarItem> {
    let Some(start) = snapshot
        .nodes
        .iter()
        .position(|n| n.bookmark.id == folder_id)
    else {
        return Vec::new();
    };
    let depth = snapshot.nodes[start].depth;
    snapshot.nodes[start + 1..]
        .iter()
        .take_while(|n| n.depth > depth)
        .filter(|n| n.depth == depth + 1)
        .map(item)
        .collect()
}

/// Every link in the tree, depth-first. Folders are skipped.
///
/// The omnibox reads this list: a page you pinned is a page you meant to
/// keep, whether it sits on the grid or inside a folder, and typing its
/// host should land there before a search or a visit.
pub fn all_links(snapshot: &BookmarksSnapshot) -> Vec<BarItem> {
    snapshot
        .nodes
        .iter()
        .filter_map(|n| match &n.bookmark.kind {
            BookmarkKind::Link { url } if !url.is_empty() => Some(item(n)),
            _ => None,
        })
        .collect()
}

/// Every link url in a folder's subtree, in tree order.
///
/// Direct and nested. "Copy All Links" is the whole contents, not only
/// the first rank — a folder of folders would otherwise copy nothing.
pub fn links_in(snapshot: &BookmarksSnapshot, folder_id: &str) -> Vec<String> {
    let Some(start) = snapshot
        .nodes
        .iter()
        .position(|n| n.bookmark.id == folder_id)
    else {
        return Vec::new();
    };
    let depth = snapshot.nodes[start].depth;
    snapshot.nodes[start + 1..]
        .iter()
        .take_while(|n| n.depth > depth)
        .filter_map(|n| match &n.bookmark.kind {
            BookmarkKind::Link { url } if !url.is_empty() => Some(url.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::{Bookmark, BookmarkNode};

    fn node(id: &str, depth: usize, title: Option<&str>, url: Option<&str>) -> BookmarkNode {
        BookmarkNode {
            bookmark: Bookmark {
                id: id.into(),
                parent: None,
                title: title.map(str::to_string),
                kind: match url {
                    Some(u) => BookmarkKind::Link { url: u.into() },
                    None => BookmarkKind::Folder,
                },
                created_ts_ms: 0,
            },
            depth,
            children: 0,
        }
    }

    fn tree() -> BookmarksSnapshot {
        let mut dev = node("dev", 0, Some("dev"), None);
        dev.children = 2;
        BookmarksSnapshot {
            nodes: vec![
                node("gh", 0, None, Some("https://www.github.com/eave/pulls")),
                dev,
                node("rs", 1, None, Some("https://docs.rs/")),
                node("inner", 1, Some("inner"), None),
                node("deep", 2, None, Some("https://deep.test/")),
                node("hn", 0, Some("news"), Some("https://news.ycombinator.com/")),
            ],
            ..Default::default()
        }
    }

    /// The bar is the top level and nothing else — a folder's contents are
    /// not on it, however far down the flat list they sit.
    #[test]
    fn the_bar_is_the_top_level_in_order() {
        let ids: Vec<String> = bar(&tree()).into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["gh", "dev", "hn"]);
    }

    /// A folder opens its DIRECT children. `deep` is inside `inner` which
    /// is inside `dev`, and a depth-first list makes it look like `dev`'s
    /// too if you only test "comes after".
    #[test]
    fn a_folder_opens_its_children_and_not_its_grandchildren() {
        let s = tree();
        let ids: Vec<String> = children_of(&s, "dev").into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["rs", "inner"]);
        assert_eq!(
            children_of(&s, "inner")
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            vec!["deep"]
        );
        // The run stops at the folder's own depth, so a later top-level
        // node is not swept in.
        assert!(children_of(&s, "hn").is_empty());
        assert!(children_of(&s, "nobody").is_empty());
    }

    #[test]
    fn copy_all_links_walks_the_whole_subtree() {
        let s = tree();
        assert_eq!(
            links_in(&s, "dev"),
            vec!["https://docs.rs/", "https://deep.test/"]
        );
        assert!(links_in(&s, "hn").is_empty());
        assert!(links_in(&s, "nobody").is_empty());
    }

    /// Nested pins are still pins. The omnibox would miss a folder's
    /// contents if this only returned the grid.
    #[test]
    fn all_links_walks_the_whole_tree() {
        let urls: Vec<String> = all_links(&tree())
            .into_iter()
            .filter_map(|i| i.url)
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://www.github.com/eave/pulls",
                "https://docs.rs/",
                "https://deep.test/",
                "https://news.ycombinator.com/",
            ]
        );
    }

    /// A link with no name of its own reads as its host, and an operator's
    /// name always wins.
    #[test]
    fn an_unnamed_link_reads_as_its_host() {
        let items = bar(&tree());
        assert_eq!(items[0].label, "github.com", "www. and the path survived");
        assert_eq!(items[2].label, "news", "the operator's own name lost");
        assert_eq!(fallback_label("https://docs.rs/"), "docs.rs");
        assert_eq!(fallback_label("about:blank"), "about:blank");
        // Nothing to name it after: keep the url rather than draw a chip
        // with no label at all.
        assert_eq!(fallback_label("https:///"), "https:///");
    }

    /// A folder is told from a link by having no url, and an EMPTY folder
    /// is still a folder.
    #[test]
    fn an_empty_folder_is_a_folder_and_not_a_link() {
        let empty = BookmarksSnapshot {
            nodes: vec![node("f", 0, Some("empty"), None)],
            ..Default::default()
        };
        let items = bar(&empty);
        assert!(items[0].is_folder());
        assert_eq!(items[0].children, 0);
        assert!(children_of(&empty, "f").is_empty());
    }
}
