//! What the browser stores put on the wire.
//!
//! Snapshots, not deltas: a client folds by REPLACEMENT, exactly as it does
//! for `Sessions` and `Files` (spec 05 §Watch, §Files). There are no deltas,
//! so there are no ordering hazards and nothing to go stale.
//!
//! # What is deliberately NOT here
//! **A tab's back/forward stack.** It is durable, it is up to
//! [`super::tabs::NAV_STACK_MAX`] entries, and the snapshot is re-sent in
//! full on every change to any tab. Carrying stacks would make one
//! keystroke in one tab re-broadcast ~10 KB × every open tab. What a tab
//! strip actually needs from the stack is two booleans — whether the back
//! and forward arrows are live — so that is what crosses, and the stack
//! itself is fetched per tab, on demand, by `tab_nav`.
//!
//! This is the same count-versus-content line `SessionInfo::pending_approvals`
//! draws, applied to size rather than to secrecy.
//!
//! # The one rule a client must keep
//! A tab that is a view onto a session ([`super::tabs::TabContent::Session`])
//! must take its title from the operator or from the session's own
//! `SessionInfo::title` — **never from the session's transcript.** The tabs
//! channel is engine-wide, like `watch`, so mining a title out of a
//! prompt's first words would put transcript content on a cross-session
//! broadcast, which is the exact thing spec 05 §Watch forbids. The engine
//! never sets such a title itself, and `browser.rs`'s raw-byte canary
//! proves no prompt text reaches this channel.

use serde::{Deserialize, Serialize};

use super::bookmarks::{Bookmark, BookmarkTree};
use super::permissions::{Grant, Permissions};
use super::tabs::{
    GroupId, LoadState, PendingNav, Slot, TabContent, TabGroup, TabId, TabsState, WindowId,
    PRIMARY_WINDOW,
};

/// One tab, as the strip sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "TabContent::is_default_content")]
    pub content: TabContent,
    /// `None` before the tab's first navigation commits. A tab showing
    /// `about:blank` has a url; a tab showing nothing does not, and the two
    /// must not render alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The page's title, or the current history entry's. Never the url
    /// standing in for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
    /// Transient, re-announced by whichever engine is attached — never
    /// journaled. A tab restored from disk reads
    /// [`LoadState::Unloaded`], which is what is true of it.
    #[serde(default, skip_serializing_if = "LoadState::is_default_state")]
    pub load: LoadState,
    /// A navigation asked for and not yet landed.
    ///
    /// This crosses the wire because **whatever renders pages is a client**
    /// — in eave it lives in the app process, not the engine — so this is
    /// the only way it can be told to go somewhere. It reads the request,
    /// loads it, and answers with [`crate::browser::tabs::TabEvent::Navigated`]
    /// via a `tab_stated` frame, which is what clears this field.
    ///
    /// Transient, exactly like `load`: never journaled, so a restored tab
    /// carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingNav>,
    /// The back and forward arrows, as two facts instead of a stack. See
    /// the module docs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_go_back: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_go_forward: bool,
    /// How many entries are in the back/forward stack. A count may cross;
    /// the entries themselves may not.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub history_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener: Option<TabId>,
    /// A private tab. Skipped when false so an older client still parses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incognito: bool,
    /// Which window's strip holds it (Slice W1).
    ///
    /// `None` means the primary window, so a client that has never heard
    /// of windows reads every tab as its own — which is what it was
    /// before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowId>,
    pub created_ts_ms: u64,
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// The whole strip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabsSnapshot {
    /// Every tab, in strip order — groups expanded in place.
    pub tabs: Vec<TabInfo>,
    /// Every group. Order matches the group slots in `order`.
    pub groups: Vec<TabGroup>,
    /// The top-level ordering: groups and ungrouped tabs interleaved.
    pub order: Vec<Slot>,
    /// Tab ids per group, in order.
    pub group_tabs: Vec<(GroupId, Vec<TabId>)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<TabId>,
    /// Every window's strip, in the order the windows were opened
    /// (Slice W1).
    ///
    /// **The snapshot carries every window and a client picks out its
    /// own.** One `watch_tabs` subscription serves a whole client, and an
    /// app with two windows on screen is still one client — so filtering
    /// at the engine would mean either a subscription per window or a
    /// window id on a channel that has never had one. Filtering at the
    /// surface costs a retain over a list the client is about to draw
    /// anyway.
    ///
    /// `tabs` above is every window's tabs; `order` and `active` are the
    /// FIRST window's, which is what a client that reads neither this
    /// field nor [`TabInfo::window`] sees — the single strip it had
    /// before windows existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<WindowStrip>,
    /// A log on disk is from a newer format and nothing will be written.
    /// A stated fact so a surface can say so instead of quietly losing
    /// every change the operator makes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// One window's share of a [`TabsSnapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowStrip {
    pub id: WindowId,
    /// Top-level ordering for this window: groups and ungrouped tabs.
    pub order: Vec<Slot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<TabId>,
}

/// One bookmark in display order, with the shape a tree view needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkNode {
    #[serde(flatten)]
    pub bookmark: Bookmark,
    /// Nesting depth; 0 for a top-level node.
    pub depth: usize,
    /// Direct children. Zero on a folder is a fact — an empty folder — and
    /// must not render as a link.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub children: usize,
}

/// The whole hierarchy, flattened into display order.
///
/// A flat ordered list rather than the nested maps the store holds, for one
/// reason: JSON object key order is not defined, so putting the store's
/// `HashMap`s on the wire would make the tree's order depend on hashing.
/// Depth-first order is the order it is drawn in, and it is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarksSnapshot {
    pub nodes: Vec<BookmarkNode>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// Every permission answer, flat and ordered (G1).
///
/// Flat rows rather than a nested map for two reasons that pull the same
/// way: it is what a permissions list renders from, and it is what the
/// app's mirror folds back into a [`Permissions`] with no special cases.
/// Ordered by origin then capability, because the whole snapshot is
/// re-sent on every change and a set that reshuffled itself would make
/// every send look like a change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsSnapshot {
    pub grants: Vec<Grant>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

impl Permissions {
    pub fn snapshot(&self, read_only: bool) -> PermissionsSnapshot {
        PermissionsSnapshot {
            grants: self.grants(),
            read_only,
        }
    }
}

impl TabsState {
    /// Take a snapshot at this instant.
    pub fn snapshot(&self, read_only: bool) -> TabsSnapshot {
        let tabs = self
            .ordered_tabs()
            .into_iter()
            .map(|t| TabInfo {
                id: t.id.clone(),
                group: t.group.clone(),
                pinned: t.pinned,
                content: t.content.clone(),
                url: t.url().map(str::to_string),
                title: t.display_title().map(str::to_string),
                favicon_url: t.favicon_url.clone(),
                load: t.load.clone(),
                pending: t.pending.clone(),
                can_go_back: t.page_can_go_back.unwrap_or_else(|| t.nav.can_go_back()),
                can_go_forward: t
                    .page_can_go_forward
                    .unwrap_or_else(|| t.nav.can_go_forward()),
                history_len: t.nav.entries.len(),
                opener: t.opener.clone(),
                incognito: t.incognito,
                // Elided for the primary window so the wire shape of a
                // one-window client is byte-for-byte what it was.
                window: (t.window != PRIMARY_WINDOW).then(|| t.window.clone()),
                created_ts_ms: t.created_ts_ms,
            })
            .collect();
        let groups: Vec<TabGroup> = self
            .windows
            .iter()
            .flat_map(|w| w.order.iter())
            .filter_map(|s| match s {
                Slot::Group { id } => self.groups.get(id).cloned(),
                Slot::Tab { .. } => None,
            })
            .collect();
        let group_tabs = groups
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    self.group_order.get(&g.id).cloned().unwrap_or_default(),
                )
            })
            .collect();
        let windows: Vec<WindowStrip> = self
            .windows
            .iter()
            .map(|w| WindowStrip {
                id: w.id.clone(),
                order: w.order.clone(),
                active: w.active.clone(),
            })
            .collect();
        // The first window's, for a client that reads neither `windows`
        // nor `TabInfo::window` — see the field docs. `first`, not the
        // primary by name: after the primary's last tab closes it is gone
        // from the fold, and answering an old client with an empty strip
        // while other windows hold tabs would be worse than answering it
        // with whichever strip exists.
        let first = self.windows.first();
        TabsSnapshot {
            tabs,
            groups,
            order: first.map(|w| w.order.clone()).unwrap_or_default(),
            group_tabs,
            active: first.and_then(|w| w.active.clone()),
            windows,
            read_only,
        }
    }
}

impl BookmarkTree {
    pub fn snapshot(&self, read_only: bool) -> BookmarksSnapshot {
        let nodes = self
            .walk()
            .into_iter()
            .map(|(depth, b)| BookmarkNode {
                bookmark: b.clone(),
                depth,
                children: self.children.get(&b.id).map(Vec::len).unwrap_or(0),
            })
            .collect();
        BookmarksSnapshot { nodes, read_only }
    }
}
