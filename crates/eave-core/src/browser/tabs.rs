//! Tabs and tab groups: the durable event vocabulary and the fold.
//!
//! # Where the durability line is drawn
//! The rule this project already runs on is that **state that is durable is
//! journaled and transient state is re-announced** — the same split that
//! keeps `AttentionSnapshot` off the journal (spec 05 §Engine semantics).
//! For a tab it lands here:
//!
//! | fact | durable? | why |
//! | --- | --- | --- |
//! | id, group, index, pinned | yes | a restart has to put the strip back |
//! | committed url + title | yes | that IS the tab |
//! | back/forward stack | yes | Chrome restores it; so do we |
//! | favicon **url** | yes | one short string, and it names the tab |
//! | favicon **bytes** | no | a cache; re-fetched by whichever engine wins |
//! | [`LoadState`] | no | a restored tab is not loading. It is [`LoadState::Unloaded`] |
//! | [`PendingNav`] | no | a restored tab is not mid-navigation either |
//! | scroll offset, form state, JS heap | no | the engine's, not ours |
//!
//! The load state is the one worth being loud about: journaling it would
//! make every restart restore a strip full of tabs claiming to be loading,
//! which is precisely the lie `Attention` is kept out of the journal to
//! avoid. It is re-announced by whichever browser engine is attached, and
//! before anything attaches the honest answer is "not loaded yet".
//!
//! # What this module deliberately does not know
//! It does not know what a page is, how to fetch one, or which engine is
//! going to render it. Every page fact arrives as an event that some engine
//! states ([`TabEvent::Navigated`], [`TabEvent::Titled`],
//! [`TabEvent::FaviconStated`]) — which is the whole point of the slice.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The longest back/forward stack one tab keeps.
///
/// A cap has to exist because the stack is durable and per tab, so an
/// uncapped one grows without bound for the life of a pinned tab. The value
/// is derived from what the stack costs to hold rather than picked to look
/// round: an entry is a url plus a title, and the ones in this project's own
/// browsing are 60–120 bytes of url and 30–80 of title, so ~200 B/entry. At
/// this cap a tab's stack is ~10 KB and a 200-tab restore reads ~2 MB — the
/// same order as a single session journal, which is the budget this
/// codebase already lives inside.
///
/// It is emphatically NOT a wire budget: the stack never rides the tabs
/// snapshot (see [`TabInfo`], which carries two bools instead). If it ever
/// does, this number is far too large and the right fix is to keep it off
/// the wire, not to shrink history the operator can still use.
pub const NAV_STACK_MAX: usize = 50;

/// The longest tab or group title kept, in **characters** — a byte cap
/// would split a multi-byte character, exactly as
/// `store::TITLE_MAX_CHARS` explains for session titles.
///
/// Larger than the session cap (64) because a page title is not something a
/// person typed to fit a rail: it is whatever the page says, and truncating
/// it hard enough to fit a tab would destroy text the omnibox still wants to
/// search. Bounding the wire and bounding the display stay different jobs.
pub const TAB_TITLE_MAX_CHARS: usize = 300;

/// The longest url kept. Data urls and search links genuinely reach the
/// low thousands of characters; the classic 2083-character IE limit is far
/// below what anything modern refuses, so this is set above it and exists
/// only to stop a pathological page from making a tab unbounded.
pub const URL_MAX_CHARS: usize = 8192;

/// Opaque tab identity. Minted by whoever opens the tab (the engine mints
/// uuids); this crate never invents one, exactly as it never invents a
/// session id.
pub type TabId = String;
/// Opaque group identity.
pub type GroupId = String;

/// What a tab is showing.
///
/// **The sessions relationship lives here, and it is a reference, not
/// ownership** (Slice B1 decision). A tab may point at an agent session;
/// the session's durable truth stays in `sessions/<uuid>/` and travels on
/// `watch`/`subscribe` exactly as it did before this slice. Closing such a
/// tab closes a *view*: it does not kill, archive, or otherwise touch the
/// session, and `browser.rs`'s process suite asserts that by name.
///
/// The alternative — making the agent chat a tab outright — is what Radius
/// does, and it is rejected here for one concrete reason: a session
/// outlives every view of it. It keeps running, keeps journaling, and keeps
/// standing on a permission question with nobody looking. A model where
/// closing the last tab is indistinguishable from ending the work has to
/// invent somewhere for that session to go; keeping the rail as the system
/// of record means it never left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabContent {
    /// An ordinary web page.
    #[default]
    Page,
    /// A view onto an agent session. The id is the session's, and this
    /// crate treats it as an opaque string — the browser store never reads
    /// a session's journal and never writes to its directory.
    Session { session_id: String },
}

impl TabContent {
    /// Public because the wire snapshot uses it as a `skip_serializing_if`,
    /// which is what keeps an ordinary page tab from carrying a `content`
    /// key an older client would not know.
    pub fn is_default_content(&self) -> bool {
        matches!(self, TabContent::Page)
    }
}

/// Whether a page is loading right now.
///
/// **Never journaled.** Re-announced by the attached browser engine; a tab
/// read back off disk is [`LoadState::Unloaded`] until something says
/// otherwise, because that is what is true.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoadState {
    /// Restored from disk, or opened but not handed to an engine yet.
    #[default]
    Unloaded,
    Loading,
    Complete,
    /// The engine's own words. Rendered verbatim; eave adds nothing.
    Failed {
        reason: String,
    },
}

impl LoadState {
    /// See [`TabContent::is_default_content`] for why this is public.
    pub fn is_default_state(&self) -> bool {
        matches!(self, LoadState::Unloaded)
    }
}

/// A navigation the operator asked for that has not landed yet.
///
/// Transient, and journaled for exactly as long as [`LoadState`] is —
/// which is not at all. A tab restored from disk is not mid-navigation,
/// and a strip that came back claiming to be loading three addresses
/// nobody asked for again would be the same lie.
///
/// # Why there is a sequence number
/// A bare `Option<String>` cannot tell "go to example.com" from "go to
/// example.com **again**", and the second one is what a reload is. The
/// page engine watches this field; without `seq` it would see no change
/// and do nothing, and the reload button would be dead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingNav {
    /// Normalized, as [`normalize_url`] leaves it.
    pub url: String,
    /// Increases per request, engine-wide. Only ever compared for
    /// inequality against the last one a renderer acted on.
    pub seq: u64,
    /// A back/forward walk the page engine should issue as Chromium
    /// `go_back` / `go_forward`, not as `load_url`.
    ///
    /// Absent means "load `url`". Present is Slice E2: `TabGo` already
    /// moved the engine stack, and a `load_url` of the walked address
    /// would push a fresh Chromium entry — so the next CEF
    /// `can_go_back` would disagree with the stack the arrows read.
    /// `None` from an older engine is a plain navigation, which is
    /// what those engines actually produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i32>,
}

/// One entry in a tab's back/forward stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavEntry {
    pub url: String,
    /// `None` = the page never stated one. Never the url standing in for a
    /// title — an omnibox that cannot tell those apart ranks on a lie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub ts_ms: u64,
}

/// A tab's back/forward stack and where in it the tab is standing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavStack {
    pub entries: Vec<NavEntry>,
    /// Index into `entries` of the entry currently shown. Meaningless when
    /// `entries` is empty; kept at 0 there.
    #[serde(default)]
    pub current: usize,
}

impl NavStack {
    pub fn current_entry(&self) -> Option<&NavEntry> {
        self.entries.get(self.current)
    }

    pub fn can_go_back(&self) -> bool {
        self.current > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.current + 1 < self.entries.len()
    }

    /// Whether a commit at `url` would land on the entry the tab is already
    /// standing on — i.e. whether it is a **reload** rather than a move.
    ///
    /// Public because the caller has to decide the history
    /// [`super::history::Transition`] BEFORE applying the event, and this is
    /// the one question that decision turns on. Asking after the fold has
    /// run is too late: the fold has already made the two cases look
    /// identical.
    pub fn is_refresh_of_current(&self, url: &str) -> bool {
        self.current_entry().is_some_and(|e| e.url == url)
    }

    /// Commit a navigation. Everything forward of the current entry is
    /// dropped — the standard browser rule, and the reason a back-then-click
    /// loses the forward arrow.
    ///
    /// Returns `true` when this was a **refresh of the current entry**
    /// rather than a move, which is what lets the fold keep the title and
    /// the favicon it would otherwise clear.
    ///
    /// # Why a same-url commit does not push
    /// Committing the address the tab is already showing is a reload, and a
    /// reload does not add a history entry in any browser. Before Slice C5
    /// it pushed one here, so refreshing a page five times left five
    /// identical entries and the back button needed five presses to leave
    /// the page you were on. It also truncated the forward stack, so going
    /// back and then reloading destroyed the forward arrow.
    ///
    /// The forward entries are deliberately kept: at `[A, B]` standing on
    /// `A`, reloading `A` is a reload of `A` and not a new navigation, so
    /// `B` is still ahead of you. That is the difference between this and
    /// the truncating path below, and it is the whole reason `Traversed`
    /// was already a separate variant.
    fn commit(&mut self, url: String, title: Option<String>, ts_ms: u64) -> bool {
        if self.is_refresh_of_current(&url) {
            let e = &mut self.entries[self.current];
            e.ts_ms = ts_ms;
            // A commit carries no title (the engine states one later, from
            // `on_title_change`), so `None` here means "not stated yet" and
            // NOT "this page has no title". Overwriting with it would blank
            // the entry a person walks back to.
            if title.is_some() {
                e.title = title;
            }
            return true;
        }
        if !self.entries.is_empty() {
            self.entries.truncate(self.current + 1);
        }
        self.entries.push(NavEntry { url, title, ts_ms });
        // Drop from the OLD end when the cap is hit: the far past is what a
        // person is least likely to walk back to, and dropping the near past
        // would make the back button skip.
        if self.entries.len() > NAV_STACK_MAX {
            let over = self.entries.len() - NAV_STACK_MAX;
            self.entries.drain(0..over);
        }
        self.current = self.entries.len() - 1;
        false
    }
}

/// One tab's durable state, plus the transient facts an engine announces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    /// `None` = ungrouped. Ungrouped tabs and groups share one ordering
    /// (see [`TabsState::order`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "TabContent::is_default_content")]
    pub content: TabContent,
    pub nav: NavStack,
    /// The tab's own title, when the page stated one. Falls back to the
    /// current nav entry's title, then to nothing — never to the url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Where the favicon lives, as the page stated it. The bytes are a
    /// cache and are not this crate's business.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
    /// Transient. Not journaled; see the module docs.
    #[serde(default, skip_serializing_if = "LoadState::is_default_state")]
    pub load: LoadState,
    /// A navigation asked for and not yet landed. Transient, not
    /// journaled. Set by [`crate::browser::tabs`]'s caller when the
    /// operator asks; cleared by the commit that answers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingNav>,
    /// Last page-engine report of the back arrow. Transient, like
    /// [`Self::load`]: never journaled, cleared on a walk or a commit so
    /// the nav stack is what a snapshot reads until the next `Load` fact.
    ///
    /// `None` = nobody has stated it this run, and the snapshot uses
    /// [`NavStack::can_go_back`]. A `Some` overlays that, which is how CEF
    /// history flags travel through `tab_stated` onto the same
    /// `TabInfo::can_go_back` the pane header and the chords already read.
    #[serde(skip)]
    pub page_can_go_back: Option<bool>,
    /// Symmetric with [`Self::page_can_go_back`].
    #[serde(skip)]
    pub page_can_go_forward: Option<bool>,
    /// The tab this one was opened from, when it was opened from one.
    /// Stated so a client can place a child tab beside its parent; nothing
    /// in this crate acts on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener: Option<TabId>,
    pub created_ts_ms: u64,
}

impl Tab {
    /// The url the tab is showing, or `None` before its first navigation
    /// commits. A tab opened at `about:blank` has a url; a tab opened with
    /// nothing does not, and the two must not render the same.
    pub fn url(&self) -> Option<&str> {
        self.nav.current_entry().map(|e| e.url.as_str())
    }

    /// The best title available, without inventing one.
    pub fn display_title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .or_else(|| self.nav.current_entry().and_then(|e| e.title.as_deref()))
    }
}

/// A tab group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: GroupId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A colour name chosen by the operator, stored verbatim and
    /// interpreted by whatever draws it. Deliberately not an enum: the
    /// palette is a design decision and this crate is not where design
    /// decisions get frozen into the file format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
    /// The checkout root this group is *about*, when it is about one.
    ///
    /// This is the stated half of the sessions relationship (Slice B1
    /// decision): Radius groups tabs per worktree and that instinct is
    /// right, so the binding is recordable. **Nothing acts on it** — the
    /// engine never moves a tab because of it, never creates a group for a
    /// worktree, and never opens a session from one. It is a fact in the
    /// same class as `SessionInfo::checkout_root`, which is stated and
    /// likewise acted on by nobody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The tab this group was last showing, so re-entering a group lands
    /// where the operator left it. Derived from activations, never set
    /// directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<TabId>,
    pub created_ts_ms: u64,
}

/// One slot in the strip's ordering: a group, or a tab that is in none.
///
/// Ordering is ONE list, not a list of groups plus a list of loose tabs,
/// because the strip draws one line and a model with two orderings has to
/// invent a rule for how they interleave — which is how a tab dragged out
/// of a group ends up somewhere nobody asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Slot {
    Tab { id: TabId },
    Group { id: GroupId },
}

/// The durable vocabulary. One variant per fact that survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabEvent {
    Opened {
        id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupId>,
        /// Where in its container the tab lands. Past the end appends.
        #[serde(default)]
        index: usize,
        #[serde(default, skip_serializing_if = "TabContent::is_default_content")]
        content: TabContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opener: Option<TabId>,
        /// The url it opens at, when it opens at one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        ts_ms: u64,
    },
    /// A navigation COMMITTED. Not a request to navigate — this is the
    /// engine stating what the tab is now showing, which is why it carries
    /// the url that actually landed rather than the one that was asked for.
    Navigated {
        id: TabId,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        ts_ms: u64,
    },
    /// Moved within the existing stack — the back and forward buttons.
    /// Separate from [`TabEvent::Navigated`] because it must NOT truncate
    /// the forward entries.
    Traversed {
        id: TabId,
        /// Signed offset from the current entry: -1 is back, +1 forward.
        /// Clamped to the stack; an impossible move is a no-op, never an
        /// error, because the operator may have raced a navigation.
        delta: i32,
    },
    Titled {
        id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    FaviconStated {
        id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        favicon_url: Option<String>,
    },
    Pinned {
        id: TabId,
        pinned: bool,
    },
    Moved {
        id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupId>,
        #[serde(default)]
        index: usize,
    },
    Closed {
        id: TabId,
    },
    /// The active tab changed. One event, two derived facts: the window's
    /// active tab, and the containing group's `last_active`.
    Activated {
        id: TabId,
    },
    GroupCreated {
        id: GroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default)]
        index: usize,
        ts_ms: u64,
    },
    /// Change one or more of a group's operator-set fields. `None` means
    /// "leave alone"; clearing a field is [`Clear::Yes`] on it. Two states
    /// need three values, which is why this is not a bare `Option`.
    GroupUpdated {
        id: GroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collapsed: Option<bool>,
    },
    GroupMoved {
        id: GroupId,
        #[serde(default)]
        index: usize,
    },
    /// Close a group. Its tabs go with it — that is what closing a group
    /// means, and a variant that silently orphaned them would leave tabs
    /// nothing draws.
    GroupClosed {
        id: GroupId,
    },
}

/// "Set it to this" versus "clear it", distinguished from "don't touch it"
/// by the enclosing `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "set", rename_all = "snake_case")]
pub enum Setting<T> {
    Value { value: T },
    Clear,
}

impl<T> Setting<T> {
    fn into_option(self) -> Option<T> {
        match self {
            Setting::Value { value } => Some(value),
            Setting::Clear => None,
        }
    }
}

/// Trim an operator- or page-supplied string to the one shape we store:
/// single line, trimmed, capped in characters. `None` out means the string
/// was not a title at all.
pub fn normalize_tab_title(raw: &str) -> Option<String> {
    let folded = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return None;
    }
    Some(folded.chars().take(TAB_TITLE_MAX_CHARS).collect())
}

/// Cap a url without splitting a character. A url is NOT whitespace-folded:
/// whitespace inside one is significant and folding it would produce a
/// different address.
pub fn normalize_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(URL_MAX_CHARS).collect())
}

/// Everything the tab strip is, folded from the log.
///
/// The fold is **total**: every event applies to every state. An event
/// naming an unknown id is a no-op, which is what lets
/// [`super::log::EventLog`] skip an unreadable line instead of refusing the
/// whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabsState {
    pub tabs: HashMap<TabId, Tab>,
    pub groups: HashMap<GroupId, TabGroup>,
    /// Top-level ordering: groups and ungrouped tabs, interleaved.
    pub order: Vec<Slot>,
    /// Tab ordering inside each group.
    pub group_order: HashMap<GroupId, Vec<TabId>>,
    /// The window's active tab. `None` when there are no tabs — never a
    /// dangling id, which the fold guarantees by clearing it on close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<TabId>,
}

impl TabsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event in. Returns whether anything changed, so a caller can
    /// skip journaling and broadcasting a no-op — a click that changes
    /// nothing must not re-broadcast every tab to every client.
    pub fn apply(&mut self, event: &TabEvent) -> bool {
        match event {
            TabEvent::Opened {
                id,
                group,
                index,
                content,
                opener,
                url,
                ts_ms,
            } => {
                if self.tabs.contains_key(id) {
                    return false;
                }
                let mut tab = Tab {
                    id: id.clone(),
                    group: None,
                    pinned: false,
                    content: content.clone(),
                    nav: NavStack::default(),
                    title: None,
                    favicon_url: None,
                    load: LoadState::Unloaded,
                    // A tab opened AT a url has already committed it below;
                    // it is not waiting on anything. A tab opened at nothing
                    // is not waiting either.
                    pending: None,
                    page_can_go_back: None,
                    page_can_go_forward: None,
                    opener: opener.clone(),
                    created_ts_ms: *ts_ms,
                };
                if let Some(u) = url.as_deref().and_then(normalize_url) {
                    tab.nav.commit(u, None, *ts_ms);
                }
                self.tabs.insert(id.clone(), tab);
                // A group that does not exist would put the tab in a
                // container the top-level ordering has no slot for, so
                // nothing would ever draw it. Land it at the top level
                // instead — the same rule `Moved` keeps, and for the same
                // reason: a tab somebody opened must be reachable.
                let group = group.as_ref().filter(|g| self.groups.contains_key(*g));
                self.place(id, group.cloned(), *index);
                true
            }
            TabEvent::Navigated {
                id,
                url,
                title,
                ts_ms,
            } => {
                let Some(u) = normalize_url(url) else {
                    return false;
                };
                let title = title.as_deref().and_then(normalize_tab_title);
                let Some(tab) = self.tabs.get_mut(id) else {
                    return false;
                };
                let refreshed = tab.nav.commit(u, title.clone(), *ts_ms);
                // A navigation replaces the tab's title: the old page's
                // title on the new page is the classic stale-tab bug.
                //
                // A RELOAD is the one case where that reasoning does not
                // apply — it is the same page, so its title and its favicon
                // are still its own. Clearing them anyway made every
                // refresh blank the tab's label until the page re-announced
                // it, which is invisible without a tab strip and obvious
                // with one (Slice C5).
                if !refreshed {
                    tab.title = title;
                    tab.favicon_url = None;
                } else if title.is_some() {
                    tab.title = title;
                }
                // A commit answers whatever was asked for, INCLUDING when
                // the url differs — a redirect landing somewhere else is
                // still that request landing. Leaving it set would make a
                // renderer that watches this field navigate again, which
                // is a loop, not a retry.
                tab.pending = None;
                true
            }
            TabEvent::Traversed { id, delta } => {
                let Some(tab) = self.tabs.get_mut(id) else {
                    return false;
                };
                if tab.nav.entries.is_empty() {
                    return false;
                }
                let last = tab.nav.entries.len() as i64 - 1;
                let want = (tab.nav.current as i64 + *delta as i64).clamp(0, last) as usize;
                if want == tab.nav.current {
                    return false;
                }
                tab.nav.current = want;
                // The tab now shows a different entry, so its title and
                // favicon are that entry's, not the one we came from.
                tab.title = tab.nav.current_entry().and_then(|e| e.title.clone());
                tab.favicon_url = None;
                true
            }
            TabEvent::Titled { id, title } => {
                let title = title.as_deref().and_then(normalize_tab_title);
                let Some(tab) = self.tabs.get_mut(id) else {
                    return false;
                };
                if tab.title == title {
                    return false;
                }
                tab.title = title.clone();
                // The nav entry learns it too, so walking back to this
                // page later shows the title rather than a bare url.
                if let Some(e) = tab.nav.entries.get_mut(tab.nav.current) {
                    e.title = title;
                }
                true
            }
            TabEvent::FaviconStated { id, favicon_url } => {
                let want = favicon_url.as_deref().and_then(normalize_url);
                let Some(tab) = self.tabs.get_mut(id) else {
                    return false;
                };
                if tab.favicon_url == want {
                    return false;
                }
                tab.favicon_url = want;
                true
            }
            TabEvent::Pinned { id, pinned } => {
                let Some(tab) = self.tabs.get_mut(id) else {
                    return false;
                };
                if tab.pinned == *pinned {
                    return false;
                }
                tab.pinned = *pinned;
                true
            }
            TabEvent::Moved { id, group, index } => {
                if !self.tabs.contains_key(id) {
                    return false;
                }
                // A move into a group that does not exist would put the tab
                // somewhere nothing draws. Refuse the group, keep the tab.
                let group = group.as_ref().filter(|g| self.groups.contains_key(*g));
                self.detach(id);
                self.place(id, group.cloned(), *index);
                true
            }
            TabEvent::Closed { id } => {
                if !self.tabs.contains_key(id) {
                    return false;
                }
                // Detach FIRST. `detach` asks the tab which group it is
                // in, so removing the tab first makes it look ungrouped
                // and leaves a dead id in `group_order` — which the strip
                // snapshot then puts on the wire.
                self.detach(id);
                self.tabs.remove(id);
                if self.active.as_deref() == Some(id.as_str()) {
                    self.active = None;
                }
                for g in self.groups.values_mut() {
                    if g.last_active.as_deref() == Some(id.as_str()) {
                        g.last_active = None;
                    }
                }
                true
            }
            TabEvent::Activated { id } => {
                let Some(tab) = self.tabs.get(id) else {
                    return false;
                };
                let group = tab.group.clone();
                let already = self.active.as_deref() == Some(id.as_str());
                self.active = Some(id.clone());
                let mut changed = !already;
                if let Some(g) = group.and_then(|g| self.groups.get_mut(&g)) {
                    if g.last_active.as_deref() != Some(id.as_str()) {
                        g.last_active = Some(id.clone());
                        changed = true;
                    }
                }
                changed
            }
            TabEvent::GroupCreated {
                id,
                title,
                color,
                workspace,
                index,
                ts_ms,
            } => {
                if self.groups.contains_key(id) {
                    return false;
                }
                self.groups.insert(
                    id.clone(),
                    TabGroup {
                        id: id.clone(),
                        title: title.as_deref().and_then(normalize_tab_title),
                        color: color.clone(),
                        collapsed: false,
                        workspace: workspace.clone(),
                        last_active: None,
                        created_ts_ms: *ts_ms,
                    },
                );
                self.group_order.insert(id.clone(), Vec::new());
                let at = (*index).min(self.order.len());
                self.order.insert(at, Slot::Group { id: id.clone() });
                true
            }
            TabEvent::GroupUpdated {
                id,
                title,
                color,
                workspace,
                collapsed,
            } => {
                let Some(g) = self.groups.get_mut(id) else {
                    return false;
                };
                let before = g.clone();
                if let Some(s) = title.clone() {
                    g.title = s.into_option().as_deref().and_then(normalize_tab_title);
                }
                if let Some(s) = color.clone() {
                    g.color = s.into_option();
                }
                if let Some(s) = workspace.clone() {
                    g.workspace = s.into_option();
                }
                if let Some(c) = collapsed {
                    g.collapsed = *c;
                }
                *g != before
            }
            TabEvent::GroupMoved { id, index } => {
                if !self.groups.contains_key(id) {
                    return false;
                }
                let Some(at) = self
                    .order
                    .iter()
                    .position(|s| matches!(s, Slot::Group { id: g } if g == id))
                else {
                    return false;
                };
                let slot = self.order.remove(at);
                let to = (*index).min(self.order.len());
                if to == at {
                    self.order.insert(at, slot);
                    return false;
                }
                self.order.insert(to, slot);
                true
            }
            TabEvent::GroupClosed { id } => {
                if self.groups.remove(id).is_none() {
                    return false;
                }
                for tab_id in self.group_order.remove(id).unwrap_or_default() {
                    self.tabs.remove(&tab_id);
                    if self.active.as_deref() == Some(tab_id.as_str()) {
                        self.active = None;
                    }
                }
                self.order
                    .retain(|s| !matches!(s, Slot::Group { id: g } if g == id));
                true
            }
        }
    }

    /// Remove a tab from whatever ordering currently holds it.
    fn detach(&mut self, id: &TabId) {
        let old_group = self.tabs.get(id).and_then(|t| t.group.clone());
        match old_group {
            Some(g) => {
                if let Some(list) = self.group_order.get_mut(&g) {
                    list.retain(|t| t != id);
                }
            }
            None => self
                .order
                .retain(|s| !matches!(s, Slot::Tab { id: t } if t == id)),
        }
    }

    /// Put a tab into a container at an index, past-the-end meaning append.
    fn place(&mut self, id: &TabId, group: Option<GroupId>, index: usize) {
        match &group {
            Some(g) => {
                let list = self.group_order.entry(g.clone()).or_default();
                let at = index.min(list.len());
                list.insert(at, id.clone());
            }
            None => {
                let at = index.min(self.order.len());
                self.order.insert(at, Slot::Tab { id: id.clone() });
            }
        }
        if let Some(tab) = self.tabs.get_mut(id) {
            tab.group = group;
        }
    }

    /// Every tab in strip order: groups expanded in place, ungrouped tabs
    /// where they sit. The one ordering any surface should read.
    pub fn ordered_tabs(&self) -> Vec<&Tab> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for slot in &self.order {
            match slot {
                Slot::Tab { id } => {
                    if let Some(t) = self.tabs.get(id) {
                        out.push(t);
                    }
                }
                Slot::Group { id } => {
                    for tab_id in self.group_order.get(id).into_iter().flatten() {
                        if let Some(t) = self.tabs.get(tab_id) {
                            out.push(t);
                        }
                    }
                }
            }
        }
        out
    }

    /// The tabs pointing at a given session, if any. The join in the one
    /// direction that is cheap; the reverse (a session naming its tabs) is
    /// deliberately absent, because it would make the session store depend
    /// on the browser store.
    pub fn tabs_for_session(&self, session_id: &str) -> Vec<&Tab> {
        let mut out: Vec<&Tab> = self
            .tabs
            .values()
            .filter(
                |t| matches!(&t.content, TabContent::Session { session_id: s } if s == session_id),
            )
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(state: &mut TabsState, id: &str, url: Option<&str>) {
        state.apply(&TabEvent::Opened {
            id: id.into(),
            group: None,
            index: usize::MAX,
            content: TabContent::Page,
            opener: None,
            url: url.map(str::to_string),
            ts_ms: 1,
        });
    }

    fn nav(state: &mut TabsState, id: &str, url: &str) {
        state.apply(&TabEvent::Navigated {
            id: id.into(),
            url: url.into(),
            title: None,
            ts_ms: 2,
        });
    }

    /// Slice C5. Reloading is committing the address the tab is already
    /// showing, and it must not grow the back stack. Before this, a page
    /// refreshed five times needed the back button pressed five times to
    /// leave it — every press landing on the same url.
    #[test]
    fn a_reload_refreshes_the_current_entry_instead_of_pushing_a_second_one() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a.test/"));
        nav(&mut s, "t", "https://b.test/");
        assert_eq!(s.tabs["t"].nav.entries.len(), 2);

        // The same address again, three times.
        for _ in 0..3 {
            nav(&mut s, "t", "https://b.test/");
        }
        let nav_stack = &s.tabs["t"].nav;
        assert_eq!(
            nav_stack.entries.len(),
            2,
            "a reload pushed a duplicate entry: {:?}",
            nav_stack.entries.iter().map(|e| &e.url).collect::<Vec<_>>()
        );
        assert_eq!(nav_stack.current, 1);
        // One press of back still leaves the page.
        assert!(nav_stack.can_go_back());
        s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -1,
        });
        assert_eq!(s.tabs["t"].url(), Some("https://a.test/"));
    }

    /// The half of the reload rule that a truncating commit would break.
    /// At `[A, B]` standing on `A`, reloading `A` is a reload of `A` — `B`
    /// is still ahead. A commit that truncated here would destroy the
    /// forward arrow every time somebody went back and hit refresh.
    #[test]
    fn reloading_after_going_back_keeps_the_forward_entries() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a.test/"));
        nav(&mut s, "t", "https://b.test/");
        s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -1,
        });
        assert_eq!(s.tabs["t"].url(), Some("https://a.test/"));
        assert!(s.tabs["t"].nav.can_go_forward());

        nav(&mut s, "t", "https://a.test/");
        assert!(
            s.tabs["t"].nav.can_go_forward(),
            "a reload truncated the forward stack"
        );
        assert_eq!(s.tabs["t"].nav.entries.len(), 2);
        // And a real navigation still truncates, which is the rule this
        // one is carved out of.
        nav(&mut s, "t", "https://c.test/");
        assert!(!s.tabs["t"].nav.can_go_forward());
    }

    /// A reload is the same page, so its title and favicon survive it. Any
    /// other navigation clears both — the stale-tab bug. Invisible before
    /// there was a tab strip to blank; the reason this is fixed in C5.
    #[test]
    fn a_reload_keeps_the_title_and_favicon_a_navigation_clears() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a.test/"));
        s.apply(&TabEvent::Titled {
            id: "t".into(),
            title: Some("Alpha".into()),
        });
        s.apply(&TabEvent::FaviconStated {
            id: "t".into(),
            favicon_url: Some("https://a.test/icon.png".into()),
        });

        // A commit carries no title: the engine states one later. That
        // `None` means "not stated yet", not "this page has no title".
        nav(&mut s, "t", "https://a.test/");
        assert_eq!(s.tabs["t"].display_title(), Some("Alpha"));
        assert_eq!(
            s.tabs["t"].favicon_url.as_deref(),
            Some("https://a.test/icon.png")
        );

        nav(&mut s, "t", "https://b.test/");
        assert_eq!(s.tabs["t"].display_title(), None, "a stale title survived");
        assert_eq!(s.tabs["t"].favicon_url, None);
    }

    /// The question the engine has to ask BEFORE it folds, because folding
    /// makes the two cases identical.
    #[test]
    fn is_refresh_of_current_reads_the_current_entry_and_not_the_stack() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a.test/"));
        nav(&mut s, "t", "https://b.test/");
        let nav_stack = &s.tabs["t"].nav;
        assert!(nav_stack.is_refresh_of_current("https://b.test/"));
        // `a.test` IS in the stack, and reloading is not what going back is.
        assert!(!nav_stack.is_refresh_of_current("https://a.test/"));
        assert!(!nav_stack.is_refresh_of_current("https://c.test/"));
        // A tab that has never navigated is not refreshing anything.
        let mut fresh = TabsState::new();
        open(&mut fresh, "u", None);
        assert!(!fresh.tabs["u"].nav.is_refresh_of_current("https://a.test/"));
    }

    /// The rule that makes the whole log skippable: an event naming an id
    /// the fold has never seen changes nothing and does not panic. Without
    /// this, `log.rs` could not skip an unreadable line.
    #[test]
    fn every_event_naming_an_unknown_id_is_a_no_op() {
        let mut s = TabsState::new();
        let events = [
            TabEvent::Navigated {
                id: "ghost".into(),
                url: "https://x/".into(),
                title: None,
                ts_ms: 0,
            },
            TabEvent::Traversed {
                id: "ghost".into(),
                delta: -1,
            },
            TabEvent::Titled {
                id: "ghost".into(),
                title: Some("t".into()),
            },
            TabEvent::FaviconStated {
                id: "ghost".into(),
                favicon_url: None,
            },
            TabEvent::Pinned {
                id: "ghost".into(),
                pinned: true,
            },
            TabEvent::Moved {
                id: "ghost".into(),
                group: Some("nogroup".into()),
                index: 0,
            },
            TabEvent::Closed { id: "ghost".into() },
            TabEvent::Activated { id: "ghost".into() },
            TabEvent::GroupUpdated {
                id: "nogroup".into(),
                title: None,
                color: None,
                workspace: None,
                collapsed: Some(true),
            },
            TabEvent::GroupMoved {
                id: "nogroup".into(),
                index: 0,
            },
            TabEvent::GroupClosed {
                id: "nogroup".into(),
            },
        ];
        for e in &events {
            assert!(!s.apply(e), "{e:?} claimed to change an empty state");
        }
        assert_eq!(s, TabsState::new());
    }

    /// Navigating from mid-stack drops the forward entries — the standard
    /// browser rule, and the reason a back-then-click loses the forward
    /// arrow.
    #[test]
    fn navigating_from_mid_stack_drops_the_forward_entries() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a/"));
        nav(&mut s, "t", "https://b/");
        nav(&mut s, "t", "https://c/");
        assert!(s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -2
        }));
        let tab = &s.tabs["t"];
        assert_eq!(tab.url(), Some("https://a/"));
        assert!(tab.nav.can_go_forward());

        nav(&mut s, "t", "https://d/");
        let urls: Vec<&str> = s.tabs["t"]
            .nav
            .entries
            .iter()
            .map(|e| e.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://a/", "https://d/"]);
        assert!(!s.tabs["t"].nav.can_go_forward());
    }

    /// A traversal past either end clamps and reports no change. It is not
    /// an error: the operator may have raced a navigation, and an error
    /// there would put a dialog in front of a mis-timed click.
    #[test]
    fn a_traversal_past_the_end_clamps_and_reports_no_change() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a/"));
        nav(&mut s, "t", "https://b/");
        assert!(!s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: 3
        }));
        assert!(s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -9
        }));
        assert_eq!(s.tabs["t"].nav.current, 0);
        assert!(!s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -1
        }));
    }

    /// The stack is capped, and it drops from the OLD end. Dropping from
    /// the near end would make the back button skip pages the operator
    /// just came from.
    #[test]
    fn the_nav_stack_caps_by_dropping_the_far_past() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://p0/"));
        for i in 1..=NAV_STACK_MAX + 10 {
            nav(&mut s, "t", &format!("https://p{i}/"));
        }
        let stack = &s.tabs["t"].nav;
        assert_eq!(stack.entries.len(), NAV_STACK_MAX);
        assert_eq!(
            stack.entries.first().unwrap().url,
            format!("https://p{}/", NAV_STACK_MAX + 10 - (NAV_STACK_MAX - 1))
        );
        assert_eq!(
            stack.current_entry().unwrap().url,
            format!("https://p{}/", NAV_STACK_MAX + 10),
            "the tab must still be showing the page it navigated to"
        );
        assert_eq!(stack.current, NAV_STACK_MAX - 1);
    }

    /// A navigation replaces the title. Keeping the old page's title on the
    /// new page is the classic stale-tab bug.
    #[test]
    fn a_navigation_clears_the_previous_pages_title_and_favicon() {
        let mut s = TabsState::new();
        open(&mut s, "t", Some("https://a/"));
        s.apply(&TabEvent::Titled {
            id: "t".into(),
            title: Some("Page A".into()),
        });
        s.apply(&TabEvent::FaviconStated {
            id: "t".into(),
            favicon_url: Some("https://a/f.ico".into()),
        });
        nav(&mut s, "t", "https://b/");
        assert_eq!(s.tabs["t"].display_title(), None);
        assert_eq!(s.tabs["t"].favicon_url, None);
        // Walking back finds the title the entry learned.
        s.apply(&TabEvent::Traversed {
            id: "t".into(),
            delta: -1,
        });
        assert_eq!(s.tabs["t"].display_title(), Some("Page A"));
    }

    /// Ordering is ONE list. A tab dragged out of a group lands in the
    /// top-level ordering at the index asked for, and is no longer in the
    /// group's list — never in both, never in neither.
    #[test]
    fn a_tab_moved_out_of_a_group_is_in_exactly_one_ordering() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::GroupCreated {
            id: "g".into(),
            title: None,
            color: None,
            workspace: None,
            index: usize::MAX,
            ts_ms: 0,
        });
        open(&mut s, "loose", Some("https://loose/"));
        s.apply(&TabEvent::Moved {
            id: "loose".into(),
            group: Some("g".into()),
            index: 0,
        });
        assert_eq!(s.group_order["g"], vec!["loose".to_string()]);
        assert!(
            !s.order
                .iter()
                .any(|x| matches!(x, Slot::Tab { id } if id == "loose")),
            "the tab is in the group AND at the top level"
        );

        s.apply(&TabEvent::Moved {
            id: "loose".into(),
            group: None,
            index: 0,
        });
        assert!(s.group_order["g"].is_empty());
        assert!(matches!(&s.order[0], Slot::Tab { id } if id == "loose"));
        assert_eq!(s.tabs["loose"].group, None);
    }

    /// A move into a group that does not exist keeps the tab: it lands at
    /// the top level, where something can draw it. Silently accepting the
    /// group id would put the tab in a list nothing renders.
    #[test]
    fn a_move_into_a_missing_group_keeps_the_tab_visible() {
        let mut s = TabsState::new();
        open(&mut s, "t", None);
        s.apply(&TabEvent::Moved {
            id: "t".into(),
            group: Some("nope".into()),
            index: 0,
        });
        assert_eq!(s.tabs["t"].group, None);
        assert_eq!(s.ordered_tabs().len(), 1);
    }

    /// Closing a group takes its tabs. Orphaning them would leave tabs no
    /// ordering contains.
    #[test]
    fn closing_a_group_removes_its_tabs_and_clears_the_active_id() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::GroupCreated {
            id: "g".into(),
            title: None,
            color: None,
            workspace: None,
            index: usize::MAX,
            ts_ms: 0,
        });
        s.apply(&TabEvent::Opened {
            id: "t".into(),
            group: Some("g".into()),
            index: usize::MAX,
            content: TabContent::Page,
            opener: None,
            url: None,
            ts_ms: 0,
        });
        s.apply(&TabEvent::Activated { id: "t".into() });
        assert_eq!(s.active.as_deref(), Some("t"));

        s.apply(&TabEvent::GroupClosed { id: "g".into() });
        assert!(s.tabs.is_empty());
        assert!(s.groups.is_empty());
        assert!(s.order.is_empty());
        assert_eq!(
            s.active, None,
            "active pointed at a tab that no longer exists"
        );
        assert!(s.ordered_tabs().is_empty());
    }

    /// Closing a GROUPED tab must leave nothing behind in the group's
    /// ordering.
    ///
    /// The bug this pins: `Closed` removed the tab before detaching it, so
    /// `detach` asked a tab that no longer existed which group it was in,
    /// got `None`, and only cleaned the top-level ordering. The dead id
    /// stayed in `group_order` — and `group_tabs` puts that straight on
    /// the wire, so a client would have drawn a tab that is not there.
    #[test]
    fn closing_a_grouped_tab_leaves_nothing_behind_in_the_group() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::GroupCreated {
            id: "g".into(),
            title: None,
            color: None,
            workspace: None,
            index: usize::MAX,
            ts_ms: 0,
        });
        for id in ["a", "b"] {
            s.apply(&TabEvent::Opened {
                id: id.into(),
                group: Some("g".into()),
                index: usize::MAX,
                content: TabContent::Page,
                opener: None,
                url: None,
                ts_ms: 0,
            });
        }
        assert!(s.apply(&TabEvent::Closed { id: "a".into() }));
        assert_eq!(
            s.group_order["g"],
            vec!["b".to_string()],
            "a closed tab's id was left in its group's ordering"
        );
        assert_eq!(s.ordered_tabs().len(), 1);
        assert_eq!(s.snapshot(false).group_tabs[0].1, vec!["b".to_string()]);
    }

    /// A tab OPENED into a group that does not exist must still be
    /// reachable.
    ///
    /// The bug this pins: `Opened` took the group id on trust, so the tab
    /// went into a `group_order` entry the top-level ordering had no slot
    /// for — it existed in `tabs`, and nothing would ever draw it.
    /// `Moved` had the check; `Opened` did not.
    #[test]
    fn a_tab_opened_into_a_missing_group_is_still_reachable() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::Opened {
            id: "t".into(),
            group: Some("nope".into()),
            index: usize::MAX,
            content: TabContent::Page,
            opener: None,
            url: Some("https://x/".into()),
            ts_ms: 0,
        });
        assert_eq!(s.tabs["t"].group, None);
        assert_eq!(
            s.ordered_tabs().len(),
            1,
            "the tab exists but no ordering contains it"
        );
        assert_eq!(s.snapshot(false).tabs.len(), 1);
    }

    /// One activation, two derived facts: the window's active tab, and the
    /// containing group's memory of where the operator was.
    #[test]
    fn activating_a_tab_also_remembers_it_for_its_group() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::GroupCreated {
            id: "g".into(),
            title: None,
            color: None,
            workspace: None,
            index: usize::MAX,
            ts_ms: 0,
        });
        for id in ["a", "b"] {
            s.apply(&TabEvent::Opened {
                id: id.into(),
                group: Some("g".into()),
                index: usize::MAX,
                content: TabContent::Page,
                opener: None,
                url: None,
                ts_ms: 0,
            });
        }
        s.apply(&TabEvent::Activated { id: "b".into() });
        assert_eq!(s.active.as_deref(), Some("b"));
        assert_eq!(s.groups["g"].last_active.as_deref(), Some("b"));
        // Leaving the group and coming back must not lose that memory.
        open(&mut s, "elsewhere", None);
        s.apply(&TabEvent::Activated {
            id: "elsewhere".into(),
        });
        assert_eq!(s.groups["g"].last_active.as_deref(), Some("b"));
        // Closing the remembered tab clears the memory rather than leaving
        // a dangling id.
        s.apply(&TabEvent::Closed { id: "b".into() });
        assert_eq!(s.groups["g"].last_active, None);
    }

    /// A session tab NAMES a session. It holds no url of its own, and the
    /// join runs only in this direction.
    #[test]
    fn a_session_tab_names_its_session_and_nothing_more() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::Opened {
            id: "t".into(),
            group: None,
            index: usize::MAX,
            content: TabContent::Session {
                session_id: "sess-1".into(),
            },
            opener: None,
            url: None,
            ts_ms: 0,
        });
        assert_eq!(s.tabs_for_session("sess-1").len(), 1);
        assert!(s.tabs_for_session("sess-2").is_empty());
        assert_eq!(s.tabs["t"].url(), None);
        // Closing it removes the TAB. There is nothing here that could
        // reach the session even if it wanted to.
        s.apply(&TabEvent::Closed { id: "t".into() });
        assert!(s.tabs_for_session("sess-1").is_empty());
    }

    /// Titles are folded to one line and capped in CHARACTERS. A byte cap
    /// would split a multi-byte character and store text that is not text
    /// — the same claim `store::normalize_title` makes for session names.
    #[test]
    fn a_title_is_one_line_and_the_cap_never_splits_a_character() {
        assert_eq!(
            normalize_tab_title("  A page\n title  "),
            Some("A page title".into())
        );
        assert_eq!(normalize_tab_title("   \n\t "), None);
        // Written as an escape, not a literal: a precomposed é and an e
        // plus combining accent look identical in a source file.
        let wide = "\u{e9}".repeat(TAB_TITLE_MAX_CHARS * 2);
        let capped = normalize_tab_title(&wide).unwrap();
        assert_eq!(capped.chars().count(), TAB_TITLE_MAX_CHARS);
        assert_eq!(capped.len(), TAB_TITLE_MAX_CHARS * 2, "two bytes per char");
    }

    /// A url is trimmed but never whitespace-FOLDED: whitespace inside a
    /// url is significant, and folding it would produce a different
    /// address.
    #[test]
    fn a_url_is_trimmed_but_never_folded() {
        assert_eq!(
            normalize_url("  https://x/a%20b  "),
            Some("https://x/a%20b".into())
        );
        assert_eq!(normalize_url("   "), None);
        let long = format!("https://x/{}", "a".repeat(URL_MAX_CHARS * 2));
        assert_eq!(normalize_url(&long).unwrap().chars().count(), URL_MAX_CHARS);
    }

    /// The three states a group field needs. Two would not do: "leave it
    /// alone" and "clear it" are different instructions.
    #[test]
    fn a_group_update_distinguishes_absent_from_clear() {
        let mut s = TabsState::new();
        s.apply(&TabEvent::GroupCreated {
            id: "g".into(),
            title: Some("one".into()),
            color: Some("blue".into()),
            workspace: Some("/repo".into()),
            index: usize::MAX,
            ts_ms: 0,
        });
        assert!(s.apply(&TabEvent::GroupUpdated {
            id: "g".into(),
            title: Some(Setting::Value {
                value: "two".into()
            }),
            color: None,
            workspace: Some(Setting::Clear),
            collapsed: None,
        }));
        let g = &s.groups["g"];
        assert_eq!(g.title.as_deref(), Some("two"));
        assert_eq!(
            g.color.as_deref(),
            Some("blue"),
            "an absent field was touched"
        );
        assert_eq!(g.workspace, None);
        assert!(!g.collapsed);
        // An update that changes nothing reports nothing.
        assert!(!s.apply(&TabEvent::GroupUpdated {
            id: "g".into(),
            title: Some(Setting::Value {
                value: "two".into()
            }),
            color: None,
            workspace: None,
            collapsed: Some(false),
        }));
    }

    /// Serde defaults, from this end: a tab and a group written by a build
    /// that predates a field read back as the harmless value, and the
    /// ordinary case writes no key at all.
    #[test]
    fn an_older_builds_event_still_parses_and_the_plain_case_is_silent() {
        // An `Opened` from before content/opener/url existed.
        let old = r#"{"kind":"opened","id":"t","ts_ms":7}"#;
        let e: TabEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            e,
            TabEvent::Opened {
                id: "t".into(),
                group: None,
                index: 0,
                content: TabContent::Page,
                opener: None,
                url: None,
                ts_ms: 7,
            }
        );
        let back = serde_json::to_string(&e).unwrap();
        for key in ["content", "opener", "url", "group"] {
            assert!(!back.contains(key), "a plain tab wrote a {key} key: {back}");
        }
        // A tab with nothing unusual about it serializes without the
        // transient or optional keys either.
        let t = Tab {
            id: "t".into(),
            created_ts_ms: 1,
            ..Tab::default()
        };
        let back = serde_json::to_string(&t).unwrap();
        for key in ["pinned", "content", "load", "favicon_url", "opener"] {
            assert!(!back.contains(key), "a plain tab wrote a {key} key: {back}");
        }
    }
}
