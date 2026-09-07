//! Slice C5 — the tab strip's **model** half: B1's `TabsState` flattened
//! into the one line the strip draws.
//!
//! Pure, for the reason `slash` and `omnibox` are: ordering, labels, group
//! runs and the ⌘W/ctrl-tab arithmetic are all decidable without a window,
//! and a rule that can only be exercised by opening one is a rule that
//! ships on argument alone.
//!
//! # One strip, one `active`
//!
//! Slice C1 left a question open and named it: *a second notion of
//! "active"*. eave had two — `TabsState::active`, the browser store's, and
//! the shell's own selected session — and no field to reconcile them.
//!
//! **There is one now, and it is `TabsState::active`.** A session tab is a
//! *view onto* a session, so selecting a session and activating its tab are
//! the same act; the shell's selection is derived from the active tab
//! rather than held beside it. That is what B1's model already said (one
//! strip, one `active`, two kinds of tab) and what C1 could not act on
//! because nothing drew the strip.
//!
//! The rail is unchanged and is still the system of record for sessions:
//! closing a session's tab closes the **view**, never the session. B1's
//! process suite already asserts that end of it.
//!
//! # Groups
//!
//! B1 models ordering as ONE list of [`Slot`]s — groups and loose tabs
//! interleaved — precisely so the strip does not have to invent a rule for
//! how two orderings mix. This flattens that list in place, marking where a
//! group's run starts and ends so the renderer can draw the run as one
//! shape without needing a second pass or a lookahead.

use std::collections::HashMap;

use eave_wire::{LoadState, Slot, TabContent, TabInfo, TabsSnapshot, PRIMARY_WINDOW};

/// Narrow an engine-wide snapshot to one window's strip (Slice W1).
///
/// **This is the whole of what a second window costs the surfaces.** The
/// engine broadcasts every window's tabs on one subscription, because an
/// app with two windows open is still one client; each shell then reads
/// the snapshot through here and gets back exactly the shape a
/// single-window eave always had — its own tabs, its own `order`, its own
/// `active`. Everything downstream of `Shell::strip_snapshot` was written
/// against that shape and none of it had to learn what a window is.
///
/// A snapshot from an engine that predates windows carries no `windows`
/// entry at all. That is not an empty strip: it is one window's worth of
/// tabs with no name on it, so it answers for the primary window and for
/// nothing else — which is what it was.
pub fn strip_for_window(snapshot: &TabsSnapshot, window: &str) -> TabsSnapshot {
    let strip = snapshot.windows.iter().find(|w| w.id == window);
    let (order, active) = match strip {
        Some(w) => (w.order.clone(), w.active.clone()),
        // No entry, and this is the primary window: either the engine is
        // older than windows, or the primary has no tabs and the flat
        // fields answer for whichever window is first. `windows` empty
        // distinguishes the two — a populated `windows` without ours
        // means ours is genuinely empty.
        None if window == PRIMARY_WINDOW && snapshot.windows.is_empty() => {
            (snapshot.order.clone(), snapshot.active.clone())
        }
        None => (Vec::new(), None),
    };
    let mine = |t: &TabInfo| match &t.window {
        Some(w) => w == window,
        None => window == PRIMARY_WINDOW,
    };
    let groups: Vec<_> = snapshot
        .groups
        .iter()
        .filter(|g| {
            order
                .iter()
                .any(|s| matches!(s, Slot::Group { id } if id == &g.id))
        })
        .cloned()
        .collect();
    TabsSnapshot {
        tabs: snapshot.tabs.iter().filter(|t| mine(t)).cloned().collect(),
        group_tabs: snapshot
            .group_tabs
            .iter()
            .filter(|(id, _)| groups.iter().any(|g| &g.id == id))
            .cloned()
            .collect(),
        groups,
        order,
        active,
        // Deliberately dropped: a narrowed snapshot is one window's strip,
        // and leaving every window's list inside it would give a caller a
        // second, wider answer to the question it just narrowed.
        windows: Vec::new(),
        read_only: snapshot.read_only,
    }
}

/// What a session tab needs from the session rail in order to draw.
///
/// Handed in rather than looked up, so this module never learns what a
/// `SessionInfo` is and the fallback for a session the rail has never heard
/// of is a decidable, tested case rather than an `unwrap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChip {
    /// The name the rail draws: the operator's title, the title this
    /// client derived from a transcript it holds (Slice ID2a), or the
    /// rail's placeholder. Never mined from another client's session —
    /// see `snapshot.rs`'s "one rule a client must keep".
    pub label: String,
    /// `label` is the rail's stand-in (`new session`, `untitled`) rather
    /// than a name anybody chose or derived (Slice ID2a). The tab draws it
    /// in the same quieter ink a page tab uses for a stand-in, so a row of
    /// tabs never presents a placeholder at a name's weight.
    pub placeholder: bool,
    /// The rail's own state word (`running`, `idle`, …), for the dot.
    pub state: String,
    /// The provider id, for the mark.
    pub provider: String,
}

/// Which kind of tab this is, with what it takes to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabKind {
    Page,
    /// A view onto a session. `chip` is `None` when the rail has no such
    /// session — see [`ORPHAN_SESSION_LABEL`].
    Session {
        session_id: String,
        chip: Option<SessionChip>,
    },
}

/// What a session tab says when the session it views is not in the rail.
///
/// This is reachable and is not a bug: the rail hides archived sessions
/// unless they are selected or asking something, so a tab can outlive its
/// session's visibility. Drawing nothing would leave an unlabelled tab the
/// operator cannot identify or account for; drawing the id would put a hex
/// string nobody chose on the strip, which the operator ruled against for
/// rail rows (Slice AO) and again for session tabs (Slice AS).
pub const ORPHAN_SESSION_LABEL: &str = "session";

/// What a page tab says before it has been anywhere.
pub const NEW_TAB_LABEL: &str = "New Chat";

/// One tab, reduced to what the strip paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripTab {
    pub id: String,
    pub kind: TabKind,
    /// The line on the tab.
    pub label: String,
    /// True when `label` is a stand-in rather than something the page or
    /// the operator said. The renderer dims these; a placeholder drawn in
    /// the same ink as a real title is a tab claiming to be called
    /// "New tab".
    pub placeholder: bool,
    pub load: LoadState,
    /// Whether this tab's page is in memory (Slice E3).
    ///
    /// **The renderer recedes these**, on the same principle as
    /// `placeholder` above: a tab whose page eave has thrown away, drawn
    /// identically to one holding a live page, is the strip claiming
    /// something it cannot back up. Always `true` for a session tab — a
    /// session is not a browser and has nothing to evict — so nothing here
    /// makes a session row look unloaded.
    ///
    /// A never-visited tab reads `false` too, and that is deliberate rather
    /// than sloppy: to the person looking at the strip, "eave gave this
    /// tab's page back" and "you have not opened this tab yet" are the same
    /// state — a tab whose page is not loaded and will load when they click
    /// it. One rule covers both and cannot drift out of step with itself.
    pub resident: bool,
    pub active: bool,
    pub pinned: bool,
    /// Where the page says its icon lives. `None` for a session tab, and
    /// for a page that has not reported one yet — the plate then draws
    /// the globe, which is the honest stand-in, not a missing mark.
    pub favicon_url: Option<String>,
    /// The page address, so the plate can look the icon up by host when
    /// the tab-keyed cache is empty (restored tabs journal no favicon
    /// event). `None` for a session tab.
    pub url: Option<String>,
    /// The group this tab is in, and where in its run it sits. `None` for a
    /// loose tab.
    pub run: Option<GroupRun>,
    /// A private tab. The plate draws a hat instead of a site mark so the
    /// strip can be read at a glance.
    pub incognito: bool,
}

/// Where a tab sits inside its group's run, so the renderer can draw the
/// run as one shape in a single pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRun {
    pub group_id: String,
    pub first: bool,
    pub last: bool,
}

/// One item on the strip's single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripItem {
    /// A group's head. Always immediately followed by that group's tabs,
    /// unless the group is collapsed.
    Group {
        id: String,
        title: Option<String>,
        color: Option<String>,
        /// The checkout root this project is bound to, when it is bound to
        /// one. Carried for Slice AG2 #8: it is the key the project's
        /// derived colour comes from, and it is the SAME key the rail
        /// groups its sessions on — so the two surfaces agree about a
        /// project's identity by computing it, not by exchanging it.
        workspace: Option<String>,
        collapsed: bool,
        /// How many tabs it holds — the number a collapsed group shows,
        /// and the reason a collapsed group is not simply absent.
        count: usize,
        /// True when the active tab is inside this group. A collapsed group
        /// holding the active tab has to say so, or activating a tab makes
        /// the strip look like nothing happened.
        holds_active: bool,
    },
    Tab(StripTab),
}

impl StripItem {
    /// The tab id, when this item is a tab. Groups have none, which is what
    /// keeps them out of the ⌘W and ctrl-tab arithmetic.
    pub fn tab_id(&self) -> Option<&str> {
        match self {
            StripItem::Tab(t) => Some(t.id.as_str()),
            StripItem::Group { .. } => None,
        }
    }
}

/// The label a page tab draws, and whether it is a placeholder.
///
/// Title, then the address in typing form, then a stand-in. **Never the raw
/// url**: `https://www.github.com/` in a 148 px tab is mostly scheme, and
/// B1 was careful that a nav entry's title is `None` rather than the url
/// precisely so this fallback is a decision made here rather than a lie
/// made upstream.
fn page_label(tab: &TabInfo) -> (String, bool) {
    // A tab that has been nowhere is a NEW CHAT, whatever CEF committed
    // for it — and CEF titles a blank page "about:blank", so this comes
    // BEFORE the title (CL1, operator: *"lets stop calling it about:blank
    // and just have it be new chat"*). Same predicate the launcher uses.
    if is_blank_page(tab) {
        return (NEW_TAB_LABEL.to_string(), true);
    }
    if let Some(t) = tab.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return (t.to_string(), false);
    }
    if let Some(url) = tab.url.as_deref() {
        let typed = eave_wire::typing_form(url);
        if !typed.is_empty() {
            return (typed, false);
        }
        // `about:blank` with a history behind it has no typing form, and
        // it IS where the tab is.
        return (url.to_string(), false);
    }
    (NEW_TAB_LABEL.to_string(), true)
}

/// The order the strip is DRAWN in: every group first, then every loose
/// tab, each keeping its relative place in `snapshot.order`.
///
/// # Why the strip reorders the engine's list at all
///
/// A group is a **project** (Slice T1): it is bound to a checkout root by
/// [`eave_wire::TabGroup::workspace`], and the sidebar draws projects above
/// loose tabs the way Comet draws Projects above Tabs. The engine keeps one
/// interleaved ordering because that is the operator's ordering and B1 was
/// right not to split it; this is a presentation rule laid over it, and it
/// is applied HERE — in one function both consumers share — rather than in
/// the renderer.
///
/// **That last part is the whole reason this exists.** [`items`] and
/// [`cycle_order`] each walked `snapshot.order` independently. A renderer
/// that sectioned the list on its own would have moved what is drawn
/// without moving what ctrl-tab walks, and the two would have disagreed
/// silently — the invariant
/// `ctrl_tab_walks_the_drawn_order_and_wraps_both_ways` asserts and the one
/// a display-only split would quietly break.
fn drawn_order(snapshot: &TabsSnapshot) -> Vec<&Slot> {
    let (groups, loose): (Vec<&Slot>, Vec<&Slot>) = snapshot
        .order
        .iter()
        .partition(|s| matches!(s, Slot::Group { .. }));
    groups.into_iter().chain(loose).collect()
}

/// How many of `items` belong to the projects section — the leading run of
/// group heads and grouped tabs.
///
/// Handed to the renderer so the two section headers know where to go,
/// rather than the renderer re-deriving the split and getting a different
/// answer from [`drawn_order`].
pub fn project_item_count(items: &[StripItem]) -> usize {
    items
        .iter()
        .take_while(|i| match i {
            StripItem::Group { .. } => true,
            StripItem::Tab(t) => t.run.is_some(),
        })
        .count()
}

/// What a project is called: the last segment of its checkout root.
///
/// `/Users/mike/Desktop/eave/wt-main` reads as `wt-main`, which is the name
/// the operator uses for it out loud. The full path is what the group is
/// BOUND to and stays on `workspace`; a title is what a person reads, and a
/// 46-character absolute path in a 158 px row is not that.
///
/// A root that is all separators (`/`) has no last segment and keeps the
/// path — a project called nothing at all would be worse than a project
/// called `/`.
pub fn project_title(workspace: &str) -> String {
    workspace
        .trim_end_matches('/')
        .rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or(workspace)
        .to_string()
}

/// The group bound to this checkout root, if one already exists.
///
/// The lookup behind Slice T1's auto-create: a session opening in a
/// checkout that has no group is what makes eave mint one. Equality on the
/// stated path, exactly as `shared_with` is derived from equality of
/// [`eave_wire::SessionInfo::checkout_root`] — same fact, same comparison.
pub fn group_for_workspace(snapshot: &TabsSnapshot, workspace: &str) -> Option<String> {
    snapshot
        .groups
        .iter()
        .find(|g| g.workspace.as_deref() == Some(workspace))
        .map(|g| g.id.clone())
}

/// Flatten the snapshot into the strip's single line.
///
/// `sessions` is the rail's own view, keyed by session id. A session tab
/// whose session is missing from it still draws — see
/// [`ORPHAN_SESSION_LABEL`].
pub fn items(
    snapshot: &TabsSnapshot,
    sessions: &HashMap<String, SessionChip>,
    resident: &dyn Fn(&str) -> bool,
) -> Vec<StripItem> {
    let by_id: HashMap<&str, &TabInfo> = snapshot.tabs.iter().map(|t| (t.id.as_str(), t)).collect();
    let group_tabs: HashMap<&str, &Vec<String>> = snapshot
        .group_tabs
        .iter()
        .map(|(g, ids)| (g.as_str(), ids))
        .collect();
    let active = snapshot.active.as_deref();

    let mut out = Vec::with_capacity(snapshot.tabs.len() + snapshot.groups.len());
    for slot in drawn_order(snapshot) {
        match slot {
            Slot::Tab { id } => {
                if let Some(info) = by_id.get(id.as_str()) {
                    out.push(StripItem::Tab(strip_tab(
                        info, active, None, sessions, resident,
                    )));
                }
            }
            Slot::Group { id } => {
                let group = snapshot.groups.iter().find(|g| &g.id == id);
                let ids: &[String] = group_tabs
                    .get(id.as_str())
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let holds_active = active.is_some_and(|a| ids.iter().any(|t| t == a));
                out.push(StripItem::Group {
                    id: id.clone(),
                    title: group.and_then(|g| g.title.clone()),
                    color: group.and_then(|g| g.color.clone()),
                    workspace: group.and_then(|g| g.workspace.clone()),
                    collapsed: group.is_some_and(|g| g.collapsed),
                    count: ids.len(),
                    holds_active,
                });
                if group.is_some_and(|g| g.collapsed) {
                    continue;
                }
                let last_ix = ids.len().saturating_sub(1);
                for (ix, tab_id) in ids.iter().enumerate() {
                    if let Some(info) = by_id.get(tab_id.as_str()) {
                        let run = GroupRun {
                            group_id: id.clone(),
                            first: ix == 0,
                            last: ix == last_ix,
                        };
                        out.push(StripItem::Tab(strip_tab(
                            info,
                            active,
                            Some(run),
                            sessions,
                            resident,
                        )));
                    }
                }
            }
        }
    }
    out
}

fn strip_tab(
    info: &TabInfo,
    active: Option<&str>,
    run: Option<GroupRun>,
    sessions: &HashMap<String, SessionChip>,
    resident: &dyn Fn(&str) -> bool,
) -> StripTab {
    let (kind, label, placeholder) = match &info.content {
        TabContent::Page => {
            let (label, placeholder) = page_label(info);
            (TabKind::Page, label, placeholder)
        }
        TabContent::Session { session_id } => {
            let chip = sessions.get(session_id).cloned();
            let (label, placeholder) = match &chip {
                Some(c) if !c.label.trim().is_empty() => (c.label.clone(), c.placeholder),
                _ => (ORPHAN_SESSION_LABEL.to_string(), true),
            };
            (
                TabKind::Session {
                    session_id: session_id.clone(),
                    chip,
                },
                label,
                placeholder,
            )
        }
    };
    // A session tab has no browser, so it is never "unloaded" — asking the
    // page engine about it would be asking the wrong question and would
    // recede every session row in a build with no Chromium in it.
    let resident = match &info.content {
        TabContent::Page => resident(&info.id),
        TabContent::Session { .. } => true,
    };
    StripTab {
        id: info.id.clone(),
        kind,
        label,
        placeholder,
        resident,
        // A tab with a navigation asked for and unanswered is loading,
        // whatever the last engine said. The engine already sets this on
        // `tab_navigate`, and stating it here too costs nothing and closes
        // the window between the request and the first `Load` fact.
        load: if info.pending.is_some() {
            LoadState::Loading
        } else {
            info.load.clone()
        },
        active: active == Some(info.id.as_str()),
        pinned: info.pinned,
        favicon_url: match &info.content {
            TabContent::Page => info.favicon_url.clone(),
            TabContent::Session { .. } => None,
        },
        url: match &info.content {
            TabContent::Page => info.url.clone(),
            TabContent::Session { .. } => None,
        },
        run,
        incognito: info.incognito,
    }
}

/// Every tab id on the strip, in the order it is drawn — which is the order
/// ctrl-tab walks.
///
/// **A collapsed group's tabs are in it.** They are still open, and a
/// cycling key that skipped them would make them unreachable by keyboard
/// while a collapsed group hid them from the mouse. The renderer expands
/// the group when the cycle lands inside it.
pub fn cycle_order(snapshot: &TabsSnapshot) -> Vec<String> {
    let live: HashMap<&str, ()> = snapshot.tabs.iter().map(|t| (t.id.as_str(), ())).collect();
    let group_tabs: HashMap<&str, &Vec<String>> = snapshot
        .group_tabs
        .iter()
        .map(|(g, ids)| (g.as_str(), ids))
        .collect();
    let mut out = Vec::with_capacity(snapshot.tabs.len());
    for slot in drawn_order(snapshot) {
        match slot {
            Slot::Tab { id } => {
                if live.contains_key(id.as_str()) {
                    out.push(id.clone());
                }
            }
            Slot::Group { id } => {
                if let Some(ids) = group_tabs.get(id.as_str()) {
                    out.extend(
                        ids.iter()
                            .filter(|t| live.contains_key(t.as_str()))
                            .cloned(),
                    );
                }
            }
        }
    }
    out
}

/// Which tab ctrl-tab lands on. `None` when there is nowhere to go.
///
/// Wraps, and wrapping is the point: with two tabs open, ctrl-tab has to
/// come back or the second press does nothing. An active tab that is not on
/// the strip (which the fold makes impossible, but a snapshot in flight can
/// still show) starts from the beginning rather than refusing.
pub fn cycle(snapshot: &TabsSnapshot, forward: bool) -> Option<String> {
    cycle_in(&cycle_order(snapshot), snapshot.active.as_deref(), forward)
}

/// Which tab should become active when `closing` goes away.
///
/// **The tab to its right, then the tab to its left.** That is what every
/// browser does and the reason is that closing several tabs in a row should
/// keep working without moving the pointer: landing on the left neighbour
/// each time walks the selection backwards through tabs the operator has
/// already dealt with.
pub fn successor(snapshot: &TabsSnapshot, closing: &str) -> Option<String> {
    let order = cycle_order(snapshot);
    let ix = order.iter().position(|t| t == closing)?;
    order
        .get(ix + 1)
        .or_else(|| if ix == 0 { None } else { order.get(ix - 1) })
        .cloned()
}

/// Sessions the rail knows about that no tab views yet.
///
/// **This is the reconciliation policy, and it is one-directional.** A tab
/// may reference a session; nothing gives a session a pointer to its tabs.
/// So the shell asks this question of the strip and opens what is missing,
/// and the session store learns nothing — which is the B1 decision held on
/// the one code path that could have quietly broken it.
///
/// Order is the rail's, so tabs appear in the order the rail lists them
/// rather than in whatever order a `HashMap` iterates.
pub fn sessions_without_tabs(snapshot: &TabsSnapshot, rail_order: &[String]) -> Vec<String> {
    let viewed: HashMap<&str, ()> = snapshot
        .tabs
        .iter()
        .filter_map(|t| match &t.content {
            TabContent::Session { session_id } => Some((session_id.as_str(), ())),
            TabContent::Page => None,
        })
        .collect();
    rail_order
        .iter()
        .filter(|s| !viewed.contains_key(s.as_str()))
        .cloned()
        .collect()
}

/// The tab that views a given session, if one does.
pub fn tab_for_session(snapshot: &TabsSnapshot, session_id: &str) -> Option<String> {
    snapshot
        .tabs
        .iter()
        .find(|t| matches!(&t.content, TabContent::Session { session_id: s } if s == session_id))
        .map(|t| t.id.clone())
}

/// The session the active tab views, if it views one.
///
/// **This is the derivation that removes the second notion of active.** The
/// shell's selected session is a function of the strip rather than a second
/// piece of state kept beside it.
pub fn active_session(snapshot: &TabsSnapshot) -> Option<String> {
    let active = snapshot.active.as_deref()?;
    snapshot
        .tabs
        .iter()
        .find(|t| t.id == active)
        .and_then(|t| match &t.content {
            TabContent::Session { session_id } => Some(session_id.clone()),
            TabContent::Page => None,
        })
}

/// Whether the active tab is a page — i.e. whether the well shows a
/// document rather than a transcript.
pub fn active_is_page(snapshot: &TabsSnapshot) -> bool {
    snapshot
        .active
        .as_deref()
        .and_then(|a| snapshot.tabs.iter().find(|t| t.id == a))
        .is_some_and(|t| t.content == TabContent::Page)
}

/// Slice T1 — whether the active tab is a page that has **never been
/// anywhere**.
///
/// ⌘T's tab, before anything is typed into the omnibox. It is a page tab
/// like any other, so `active_is_page` is true of it and the content well
/// belongs to it; what it has no way to say on its own is that there is
/// nothing to draw there *yet* and never was — as opposed to a page that is
/// loading, or one whose browser eave evicted.
///
/// All three conditions are needed and none is redundant. `url` alone is
/// `None` on a tab whose browser was evicted mid-history; `pending` catches
/// the tab that has been asked to go somewhere and not yet arrived, which
/// is emphatically not blank; and `history_len` is what tells a fresh tab
/// from one the operator navigated back off the front of.
///
/// **`about:blank` with one history entry is blank too (Slice NC3).** The
/// live engine never keeps `url: None` on a ⌘T tab: CEF creates the
/// browser at `about:blank`, `on_load_start` reports it as a commit, and
/// the tab arrives with a url and `history_len == 1`. The harness modelled
/// the blank tab as `url: None` and the rule matched only that, so the
/// launcher showed for one snapshot and then gave the well to a white
/// `about:blank` — the operator's *"the screen instantly goes blank"*.
/// A tab navigated back onto `about:blank` has two entries and is not
/// blank; a tab asked to go somewhere is `pending` and is not blank.
pub fn active_is_blank_page(snapshot: &TabsSnapshot) -> bool {
    let Some(active) = snapshot.active.as_deref() else {
        return false;
    };
    snapshot.tabs.iter().any(|t| t.id == active && is_blank_page(t))
}

/// Whether ONE page tab has never been anywhere — the rule above, asked
/// about any tab (CL1 asks it per column; the label asks it per row).
pub fn is_blank_page(t: &TabInfo) -> bool {
    matches!(t.content, TabContent::Page)
        && t.pending.is_none()
        && match t.url.as_deref() {
            None => t.history_len == 0,
            Some("about:blank") => t.history_len <= 1,
            Some(_) => false,
        }
}

/// Where a tab sits: which group holds it, and its index inside that
/// container.
///
/// Slice T1 needs this to put a session tab exactly where the blank tab it
/// replaces was — same project, same row. Reading the position off the
/// snapshot rather than remembering it from when the tab was opened,
/// because the operator may have dragged it since.
pub fn tab_slot(snapshot: &TabsSnapshot, tab_id: &str) -> Option<(Option<String>, usize)> {
    for (group, ids) in &snapshot.group_tabs {
        if let Some(ix) = ids.iter().position(|t| t == tab_id) {
            return Some((Some(group.clone()), ix));
        }
    }
    // Loose: index among the top-level slots, which is what `TabMove` and
    // `TabOpen` mean by an index at the top level.
    snapshot
        .order
        .iter()
        .position(|slot| matches!(slot, Slot::Tab { id } if id == tab_id))
        .map(|ix| (None, ix))
}

/// The checkout root the active tab's project is bound to.
///
/// Slice T1 gave a group a `workspace`; this is the read that finally
/// spends it. A chat launched while a project's tab is active belongs in
/// that project's worktree, so `Command::Launch` carries this as its
/// `cwd` and the engine starts the CLI there.
///
/// `None` for a loose tab, for a group with no workspace, and for no
/// active tab at all — all three mean the same thing to the caller
/// (**let the engine pick**), which is exactly what eave did before this
/// existed.
pub fn active_workspace(snapshot: &TabsSnapshot) -> Option<String> {
    let active = snapshot.active.as_deref()?;
    let (group, _) = tab_slot(snapshot, active)?;
    let group = group?;
    snapshot
        .groups
        .iter()
        .find(|g| g.id == group)
        .and_then(|g| g.workspace.clone())
}

/// Which set of tabs a tab belongs to (Slice ID2b): a task's group, or the
/// window's loose set — the rail's PAGES zone since ID2b′.
///
/// The containment flip is this distinction and nothing more. A page tab
/// in a group belongs to the task that group is bound to; a page tab in no
/// group is the window's Browse set, which needs no store, no event and no
/// wire field, because it is exactly what `strip_for_window` already
/// returns as ungrouped tabs. Incognito tabs are always loose, so Browse
/// is where a private tab lives without ever naming a durable group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Container {
    /// A task's tool set: the group, by id.
    Group(String),
    /// The window's loose tabs.
    Loose,
}

/// The container `tab_id` sits in, or `None` for a tab the strip does not
/// hold.
pub fn container_of(snapshot: &TabsSnapshot, tab_id: &str) -> Option<Container> {
    let (group, _) = tab_slot(snapshot, tab_id)?;
    Some(match group {
        Some(g) => Container::Group(g),
        None => Container::Loose,
    })
}

/// The tab ids one container holds, in the order they are drawn — a
/// task's docked pages, or PAGES.
///
/// A group's tabs are its own `group_tabs` run; the loose set is every
/// top-level `Slot::Tab` in `order`, which keeps the engine's ordering
/// exactly as `cycle_order` does for the whole strip.
pub fn container_tabs(snapshot: &TabsSnapshot, container: &Container) -> Vec<String> {
    let live: HashMap<&str, ()> = snapshot.tabs.iter().map(|t| (t.id.as_str(), ())).collect();
    match container {
        Container::Group(id) => snapshot
            .group_tabs
            .iter()
            .find(|(g, _)| g == id)
            .map(|(_, ids)| {
                ids.iter()
                    .filter(|t| live.contains_key(t.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        Container::Loose => snapshot
            .order
            .iter()
            .filter_map(|s| match s {
                Slot::Tab { id } if live.contains_key(id.as_str()) => Some(id.clone()),
                _ => None,
            })
            .collect(),
    }
}

/// The window's loose PAGE tabs — the rows of the rail's PAGES zone
/// (ID2b′). A loose session tab (one the reconcile has not filed yet) is
/// not a page and is not one of them.
pub fn loose_pages(snapshot: &TabsSnapshot) -> Vec<String> {
    container_tabs(snapshot, &Container::Loose)
        .into_iter()
        .filter(|id| {
            snapshot
                .tabs
                .iter()
                .any(|t| &t.id == id && t.content == TabContent::Page)
        })
        .collect()
}

/// Which of `order` a cycling key lands on, from `active` (ID2b′).
///
/// The rule [`cycle`] has always had, over an order the caller supplies —
/// the sidebar's, written by the paint (`Shell::spine_order`). Wraps, and
/// wrapping is the point: with two tabs open, ctrl-tab has to come back
/// or the second press does nothing. An active tab not in the order
/// starts from the beginning rather than refusing.
pub fn cycle_in(order: &[String], active: Option<&str>, forward: bool) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let at = active.and_then(|a| order.iter().position(|t| t == a));
    let next = match at {
        Some(ix) if forward => (ix + 1) % order.len(),
        Some(ix) => (ix + order.len() - 1) % order.len(),
        None => 0,
    };
    Some(order[next].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::{PendingNav, TabGroup};

    fn page(id: &str, url: Option<&str>, title: Option<&str>) -> TabInfo {
        TabInfo {
            id: id.into(),
            group: None,
            pinned: false,
            content: TabContent::Page,
            url: url.map(str::to_string),
            title: title.map(str::to_string),
            favicon_url: None,
            load: LoadState::Unloaded,
            pending: None,
            can_go_back: false,
            can_go_forward: false,
            history_len: 0,
            opener: None,
            incognito: false,
            window: None,
            created_ts_ms: 0,
        }
    }

    fn session_tab(id: &str, session_id: &str) -> TabInfo {
        TabInfo {
            content: TabContent::Session {
                session_id: session_id.into(),
            },
            ..page(id, None, None)
        }
    }

    fn snap(tabs: Vec<TabInfo>, active: Option<&str>) -> TabsSnapshot {
        TabsSnapshot {
            order: tabs
                .iter()
                .map(|t| Slot::Tab { id: t.id.clone() })
                .collect(),
            tabs,
            groups: Vec::new(),
            group_tabs: Vec::new(),
            active: active.map(str::to_string),
            windows: Vec::new(),
            read_only: false,
        }
    }

    fn chips(pairs: &[(&str, &str)]) -> HashMap<String, SessionChip> {
        pairs
            .iter()
            .map(|(id, label)| {
                (
                    id.to_string(),
                    SessionChip {
                        label: label.to_string(),
                        placeholder: false,
                        state: "idle".into(),
                        provider: "claude".into(),
                    },
                )
            })
            .collect()
    }

    /// Title, then the address in typing form, then a stand-in — and the
    /// stand-in says it is one. A tab drawing "New tab" in the same ink as
    /// a real title is a tab claiming to be called that.
    #[test]
    fn a_page_tab_prefers_a_title_then_the_address_then_says_it_has_neither() {
        let s = snap(
            vec![
                page("a", Some("https://www.github.com/eave"), Some("eave")),
                page("b", Some("https://www.github.com/eave"), None),
                page("c", None, None),
                // CEF titles a blank page "about:blank"; still a New Chat.
                page("d", Some("about:blank"), Some("about:blank")),
                // Navigated BACK onto about:blank: two entries, not blank.
                {
                    let mut e = page("e", Some("about:blank"), None);
                    e.history_len = 2;
                    e
                },
            ],
            Some("a"),
        );
        let items = items(&s, &HashMap::new(), &|_| true);
        let labels: Vec<(&str, bool)> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => Some((t.label.as_str(), t.placeholder)),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                ("eave", false),
                ("github.com/eave", false),
                (NEW_TAB_LABEL, true),
                // CEF's committed about:blank with one entry is still a
                // tab that has been nowhere: a New Chat (CL1's ruling).
                (NEW_TAB_LABEL, true),
                ("about:blank", false),
            ]
        );
    }

    #[test]
    fn a_page_tab_carries_its_favicon_url_and_a_session_tab_does_not() {
        let mut p = page("p", Some("https://a.test/"), Some("A"));
        p.favicon_url = Some("https://a.test/favicon.png".into());
        let s = snap(vec![p, session_tab("s", "sess")], Some("p"));
        let items = items(&s, &chips(&[("sess", "work")]), &|_| true);
        let urls: Vec<Option<&str>> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => Some(t.favicon_url.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            vec![Some("https://a.test/favicon.png"), None],
            "session tabs must not inherit a page's favicon"
        );
        let pages: Vec<Option<&str>> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => Some(t.url.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            pages,
            vec![Some("https://a.test/"), None],
            "session tabs must not inherit a page's address"
        );
    }

    /// One strip, two kinds of tab — which is the whole shape B1 argued for
    /// and C1 could not draw.
    #[test]
    fn page_tabs_and_session_tabs_share_one_line_in_one_order() {
        let s = snap(
            vec![
                page("p1", Some("https://a.test/"), Some("Alpha")),
                session_tab("s1", "sess-1"),
                page("p2", Some("https://b.test/"), Some("Beta")),
            ],
            Some("s1"),
        );
        let items = items(&s, &chips(&[("sess-1", "refactor the fold")]), &|_| true);
        assert_eq!(items.len(), 3);
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => Some(t.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["Alpha", "refactor the fold", "Beta"]);
        match &items[1] {
            StripItem::Tab(t) => {
                assert!(t.active, "the session tab is the active one");
                assert!(matches!(&t.kind, TabKind::Session { session_id, chip }
                    if session_id == "sess-1" && chip.is_some()));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Slice ID2a: the rail's stand-in for an unnamed session reaches the
    /// tab flagged as one, so the tab draws it in a stand-in's ink — the
    /// same rule a page tab's placeholder already follows.
    #[test]
    fn a_session_tab_wears_the_rails_placeholder_as_a_placeholder() {
        let s = snap(vec![session_tab("t", "sess-new")], Some("t"));
        let mut chips = chips(&[("sess-new", "new session")]);
        chips.get_mut("sess-new").unwrap().placeholder = true;
        let items = items(&s, &chips, &|_| true);
        match &items[0] {
            StripItem::Tab(t) => {
                assert_eq!(t.label, "new session");
                assert!(t.placeholder, "the rail's stand-in drew at a name's weight");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A session tab can outlive its session's place in the rail — the rail
    /// hides archived sessions. It still draws, and it does not draw a hex
    /// string nobody chose.
    #[test]
    fn a_session_tab_whose_session_the_rail_does_not_list_still_draws() {
        let s = snap(vec![session_tab("t", "sess-gone")], Some("t"));
        let items = items(&s, &HashMap::new(), &|_| true);
        match &items[0] {
            StripItem::Tab(t) => {
                assert_eq!(t.label, ORPHAN_SESSION_LABEL);
                assert!(t.placeholder);
                assert!(
                    !t.label.contains("sess-gone"),
                    "the strip drew a session id at the operator"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A navigation asked for and unanswered IS loading. Without this the
    /// tab shows nothing between the keystroke and the page engine's first
    /// `Load` fact, which is exactly the window a person is watching.
    #[test]
    fn a_pending_navigation_reads_as_loading_before_any_engine_answers() {
        let mut t = page("a", Some("https://a.test/"), Some("Alpha"));
        t.pending = Some(PendingNav {
            url: "https://b.test/".into(),
            seq: 1,
            delta: None,
        });
        t.load = LoadState::Complete;
        let s = snap(vec![t], Some("a"));
        match &items(&s, &HashMap::new(), &|_| true)[0] {
            StripItem::Tab(t) => assert_eq!(t.load, LoadState::Loading),
            other => panic!("{other:?}"),
        }
    }

    fn grouped() -> TabsSnapshot {
        let tabs = vec![
            page("loose", Some("https://loose.test/"), None),
            page("g1", Some("https://g1.test/"), None),
            page("g2", Some("https://g2.test/"), None),
            page("g3", Some("https://g3.test/"), None),
        ];
        TabsSnapshot {
            tabs,
            groups: vec![TabGroup {
                id: "grp".into(),
                title: Some("eave".into()),
                color: Some("violet".into()),
                collapsed: false,
                workspace: None,
                base: None,
                last_active: None,
                created_ts_ms: 0,
            }],
            order: vec![
                Slot::Tab { id: "loose".into() },
                Slot::Group { id: "grp".into() },
            ],
            group_tabs: vec![("grp".into(), vec!["g1".into(), "g2".into(), "g3".into()])],
            active: Some("g2".into()),
            windows: Vec::new(),
            read_only: false,
        }
    }

    /// B1's one ordering, with the run marked so the renderer can draw it
    /// as one shape in a single pass — and the group hoisted above the
    /// loose tabs, which is Slice T1's one presentation rule.
    ///
    /// This test used to be called `a_group_flattens_in_place_…` and
    /// asserted that `loose` came first because the engine's `order` put it
    /// there. In place is exactly what a group no longer is: a group is a
    /// **project**, projects are a section above loose tabs, and the
    /// hoisting happens in [`drawn_order`] so that what is drawn and what
    /// ctrl-tab walks cannot come apart.
    #[test]
    fn a_group_is_drawn_above_the_loose_tabs_with_its_run_marked_at_both_ends() {
        let items = items(&grouped(), &HashMap::new(), &|_| true);
        assert_eq!(items.len(), 5, "{items:?}");
        assert!(
            matches!(&items[4], StripItem::Tab(t) if t.id == "loose" && t.run.is_none()),
            "the loose tab belongs under the projects, not above them: {items:?}"
        );
        match &items[0] {
            StripItem::Group {
                id,
                title,
                count,
                holds_active,
                collapsed,
                ..
            } => {
                assert_eq!(id, "grp");
                assert_eq!(title.as_deref(), Some("eave"));
                assert_eq!(*count, 3);
                assert!(*holds_active);
                assert!(!collapsed);
            }
            other => panic!("{other:?}"),
        }
        let runs: Vec<(bool, bool)> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => t.run.as_ref().map(|r| (r.first, r.last)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, vec![(true, false), (false, false), (false, true)]);
        assert_eq!(
            project_item_count(&items),
            4,
            "the head and its three tabs are the projects section: {items:?}"
        );
    }

    /// A project reads as its folder, not as its path.
    #[test]
    fn a_project_is_named_after_the_last_segment_of_its_checkout() {
        assert_eq!(project_title("/Users/mike/Desktop/eave/wt-main"), "wt-main");
        assert_eq!(
            project_title("/Users/mike/Desktop/eave/wt-main/"),
            "wt-main"
        );
        assert_eq!(project_title("eave"), "eave");
        // Nothing to name it after. Keep the path rather than draw a blank.
        assert_eq!(project_title("/"), "/");
        assert_eq!(project_title(""), "");
    }

    /// Slice T1 — a group is a **project** and projects are drawn above
    /// loose tabs, whatever order the engine keeps them in.
    ///
    /// The engine's `order` is the operator's ordering and B1 was right to
    /// keep it interleaved; this asserts the presentation rule laid over
    /// it, including the part that is easy to lose — that two groups keep
    /// their order *relative to each other*, and two loose tabs keep
    /// theirs. A partition that sorted rather than stably partitioned
    /// would pass a one-group fixture and fail here.
    #[test]
    fn projects_are_drawn_above_loose_tabs_and_keep_their_relative_order() {
        let mut s = grouped();
        s.tabs
            .push(page("loose2", Some("https://loose2.test/"), None));
        s.tabs.push(page("h1", Some("https://h1.test/"), None));
        s.groups.push(TabGroup {
            id: "grp2".into(),
            title: Some("summitpoint".into()),
            color: None,
            collapsed: false,
            workspace: None,
            base: None,
            last_active: None,
            created_ts_ms: 0,
        });
        // Interleaved on the wire: loose, grp, loose2, grp2.
        s.order = vec![
            Slot::Tab { id: "loose".into() },
            Slot::Group { id: "grp".into() },
            Slot::Tab {
                id: "loose2".into(),
            },
            Slot::Group { id: "grp2".into() },
        ];
        s.group_tabs.push(("grp2".into(), vec!["h1".into()]));

        let items = items(&s, &HashMap::new(), &|_| true);
        let drawn: Vec<&str> = items
            .iter()
            .map(|i| match i {
                StripItem::Group { id, .. } => id.as_str(),
                StripItem::Tab(t) => t.id.as_str(),
            })
            .collect();
        assert_eq!(
            drawn,
            vec!["grp", "g1", "g2", "g3", "grp2", "h1", "loose", "loose2"],
            "projects first, then loose tabs, each in its own relative order"
        );
        assert_eq!(project_item_count(&items), 6);
    }

    /// The lookup Slice T1's auto-create turns on: a checkout root either
    /// already has a group or it does not.
    ///
    /// A group with no workspace must not answer for one — it is a project
    /// the operator made by hand, and matching it to a checkout it was
    /// never bound to would file somebody's sessions under a stranger.
    #[test]
    fn a_checkout_root_finds_its_group_and_an_unbound_group_answers_for_nothing() {
        let mut s = grouped();
        assert_eq!(
            group_for_workspace(&s, "/Users/x/eave"),
            None,
            "an unbound group answered for a checkout it was never given"
        );
        s.groups[0].workspace = Some("/Users/x/eave".into());
        assert_eq!(
            group_for_workspace(&s, "/Users/x/eave").as_deref(),
            Some("grp")
        );
        assert_eq!(group_for_workspace(&s, "/Users/x/other"), None);
    }

    /// Slice AG2 #8 — the project row carries the checkout it is bound to,
    /// because that is the key its colour comes from and the same key the
    /// rail groups sessions on. Dropping it here would not break a layout;
    /// it would quietly give the project one colour on the strip and a
    /// different one in the rail.
    #[test]
    fn a_project_row_carries_the_checkout_it_is_bound_to() {
        let mut s = grouped();
        s.groups[0].workspace = Some("/w/eave".into());
        let items = items(&s, &HashMap::new(), &|_| true);
        match &items[0] {
            StripItem::Group { workspace, .. } => assert_eq!(
                workspace.as_deref(),
                Some("/w/eave"),
                "the strip forgot which checkout the project is about"
            ),
            other => panic!("expected a project row: {other:?}"),
        }
    }

    /// A collapsed group hides its tabs from the strip but not from the
    /// keyboard, and it has to say that the active tab is inside it —
    /// otherwise activating a tab looks like nothing happened.
    #[test]
    fn a_collapsed_group_hides_its_tabs_but_still_counts_and_still_cycles() {
        let mut s = grouped();
        s.groups[0].collapsed = true;
        let items = items(&s, &HashMap::new(), &|_| true);
        assert_eq!(items.len(), 2, "collapsed tabs were drawn: {items:?}");
        assert!(matches!(
            &items[0],
            StripItem::Group {
                count: 3,
                holds_active: true,
                collapsed: true,
                ..
            }
        ));
        assert!(matches!(&items[1], StripItem::Tab(t) if t.id == "loose"));
        assert_eq!(
            project_item_count(&items),
            1,
            "a collapsed project is still the projects section, just an empty-looking one"
        );
        assert_eq!(
            cycle_order(&s),
            vec!["g1", "g2", "g3", "loose"],
            "a collapsed group made its tabs unreachable by keyboard"
        );
    }

    /// Wrapping is the point: with two tabs open, a cycle that did not wrap
    /// would make the second press do nothing.
    #[test]
    fn ctrl_tab_walks_the_drawn_order_and_wraps_both_ways() {
        let s = grouped();
        assert_eq!(cycle_order(&s), vec!["g1", "g2", "g3", "loose"]);
        assert_eq!(cycle(&s, true).as_deref(), Some("g3"));
        assert_eq!(cycle(&s, false).as_deref(), Some("g1"));

        let mut last = s.clone();
        last.active = Some("loose".into());
        assert_eq!(cycle(&last, true).as_deref(), Some("g1"));
        let mut first = s.clone();
        first.active = Some("g1".into());
        assert_eq!(cycle(&first, false).as_deref(), Some("loose"));

        assert_eq!(cycle(&snap(vec![], None), true), None);
        // Nothing active: start at the beginning rather than refuse.
        let mut none = s.clone();
        none.active = None;
        assert_eq!(cycle(&none, true).as_deref(), Some("g1"));

        // The invariant this whole hoist exists to keep: what ctrl-tab
        // walks IS what is drawn, in that order. Asserted by construction
        // rather than by two lists that happen to agree today.
        let drawn: Vec<String> = items(&s, &HashMap::new(), &|_| true)
            .iter()
            .filter_map(|i| i.tab_id().map(str::to_string))
            .collect();
        assert_eq!(drawn, cycle_order(&s));
    }

    /// Right, then left. Landing on the left neighbour each time walks the
    /// selection backwards through tabs already dealt with.
    #[test]
    fn closing_a_tab_lands_on_its_right_neighbour_then_its_left() {
        let s = grouped();
        assert_eq!(successor(&s, "g1").as_deref(), Some("g2"));
        assert_eq!(successor(&s, "g2").as_deref(), Some("g3"));
        assert_eq!(
            successor(&s, "g3").as_deref(),
            Some("loose"),
            "a project's last tab falls out to the loose tab below it"
        );
        assert_eq!(
            successor(&s, "loose").as_deref(),
            Some("g3"),
            "the last tab has no right neighbour"
        );
        assert_eq!(
            successor(&snap(vec![page("only", None, None)], Some("only")), "only"),
            None
        );
        assert_eq!(successor(&s, "ghost"), None);
    }

    /// The reconciliation, and the direction it runs in. The browser store
    /// learns about sessions; the session store learns nothing.
    #[test]
    fn sessions_without_tabs_is_asked_of_the_strip_in_the_rails_order() {
        let s = snap(
            vec![page("p", None, None), session_tab("t", "sess-2")],
            Some("p"),
        );
        let rail = vec!["sess-1".to_string(), "sess-2".into(), "sess-3".into()];
        assert_eq!(sessions_without_tabs(&s, &rail), vec!["sess-1", "sess-3"]);
        assert_eq!(tab_for_session(&s, "sess-2").as_deref(), Some("t"));
        assert_eq!(tab_for_session(&s, "sess-1"), None);
    }

    /// The derivation that removes the second notion of active: the shell's
    /// selected session is a function of the strip, not a second piece of
    /// state kept beside it.
    #[test]
    fn the_selected_session_is_derived_from_the_active_tab() {
        let tabs = vec![
            page("p", Some("https://a.test/"), None),
            session_tab("t", "sess-1"),
        ];
        let on_session = snap(tabs.clone(), Some("t"));
        assert_eq!(active_session(&on_session).as_deref(), Some("sess-1"));
        assert!(!active_is_page(&on_session));

        let on_page = snap(tabs, Some("p"));
        assert_eq!(
            active_session(&on_page),
            None,
            "a page tab is not a view onto a session"
        );
        assert!(active_is_page(&on_page));

        assert_eq!(active_session(&snap(vec![], None)), None);
        assert!(!active_is_page(&snap(vec![], None)));
    }

    /// Slice E3. The strip carries residency as a fact about the tab, and a
    /// SESSION tab is always resident — it has no browser to lose. Without
    /// that arm, a build with no page engine would report every session row
    /// unloaded, and the rail would recede itself.
    #[test]
    fn residency_is_carried_for_pages_and_never_asked_of_a_session() {
        let s = snap(
            vec![
                page("p-live", Some("https://example.com/"), Some("live")),
                page("p-evicted", Some("https://docs.rs/gpui"), Some("evicted")),
                session_tab("sess-1", "sess-1"),
            ],
            Some("p-live"),
        );
        let chips = chips(&[("sess-1", "refactor the fold")]);
        // Only `p-live` holds a page. The closure is asked about page tabs
        // and must never be consulted about the session one — if it were,
        // this answer would recede it.
        let items = items(&s, &chips, &|id| id == "p-live");
        let by_id: Vec<(String, bool)> = items
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => Some((t.id.clone(), t.resident)),
                _ => None,
            })
            .collect();
        assert_eq!(
            by_id,
            vec![
                ("p-live".to_string(), true),
                ("p-evicted".to_string(), false),
                ("sess-1".to_string(), true),
            ]
        );
    }

    // ── Slice W1: narrowing a whole-app snapshot to one window ────────

    fn windowed(tab: &str, window: &str) -> TabInfo {
        TabInfo {
            window: Some(window.into()),
            ..page(tab, None, None)
        }
    }

    /// A shell sees its own strip and nothing else — the whole of what a
    /// second window costs the surfaces downstream.
    #[test]
    fn a_window_reads_its_own_tabs_and_no_others() {
        let snapshot = TabsSnapshot {
            tabs: vec![page("a", None, None), windowed("b", "w-2")],
            groups: Vec::new(),
            group_tabs: Vec::new(),
            order: vec![Slot::Tab { id: "a".into() }],
            active: Some("a".into()),
            windows: vec![
                eave_wire::WindowStrip {
                    id: eave_wire::PRIMARY_WINDOW.into(),
                    order: vec![Slot::Tab { id: "a".into() }],
                    active: Some("a".into()),
                },
                eave_wire::WindowStrip {
                    id: "w-2".into(),
                    order: vec![Slot::Tab { id: "b".into() }],
                    active: Some("b".into()),
                },
            ],
            read_only: false,
        };

        let primary = strip_for_window(&snapshot, eave_wire::PRIMARY_WINDOW);
        assert_eq!(
            primary
                .tabs
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(primary.active.as_deref(), Some("a"));

        let second = strip_for_window(&snapshot, "w-2");
        assert_eq!(
            second
                .tabs
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(second.active.as_deref(), Some("b"));

        // A narrowed strip carries no window list: one answer to "which
        // tabs", not two of different widths.
        assert!(primary.windows.is_empty() && second.windows.is_empty());
    }

    /// An engine that predates windows sends no `windows` list. That is
    /// not an empty strip — it is one window's tabs with no name on them,
    /// and it has to answer for the primary window or the app boots
    /// showing nothing.
    #[test]
    fn a_snapshot_from_before_windows_is_the_primary_windows_strip() {
        let snapshot = snap(vec![page("a", None, None)], Some("a"));
        assert!(snapshot.windows.is_empty(), "the fixture named a window");

        let primary = strip_for_window(&snapshot, eave_wire::PRIMARY_WINDOW);
        assert_eq!(
            primary
                .tabs
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(primary.active.as_deref(), Some("a"));

        // ...and only for the primary. A second window reading the same
        // nameless snapshot must get nothing, or every window would draw
        // the same tabs.
        let second = strip_for_window(&snapshot, "w-2");
        assert!(second.tabs.is_empty() && second.active.is_none());
    }

    // ── Slice ID2b: containers ────────────────────────────────────────

    /// A tab is in its task's group or in Browse, and the two sets are
    /// the two things the tool strip can show.
    #[test]
    fn a_tab_is_in_its_group_or_in_the_windows_loose_set() {
        let mut s = grouped();
        s.tabs.push(page("loose2", Some("https://loose2.test/"), None));
        s.order.push(Slot::Tab { id: "loose2".into() });
        assert_eq!(
            container_of(&s, "g2"),
            Some(Container::Group("grp".into()))
        );
        assert_eq!(container_of(&s, "loose"), Some(Container::Loose));
        assert_eq!(container_of(&s, "ghost"), None);
        assert_eq!(
            container_tabs(&s, &Container::Group("grp".into())),
            vec!["g1", "g2", "g3"]
        );
        assert_eq!(
            container_tabs(&s, &Container::Loose),
            vec!["loose", "loose2"],
            "Browse is the loose tabs in the engine's own order"
        );
        assert_eq!(loose_pages(&s), vec!["loose", "loose2"]);
        // A loose SESSION tab is not browsing.
        s.tabs.push(session_tab("t", "sess"));
        s.order.push(Slot::Tab { id: "t".into() });
        assert_eq!(loose_pages(&s), vec!["loose", "loose2"]);
        assert_eq!(
            container_tabs(&s, &Container::Loose),
            vec!["loose", "loose2", "t"]
        );
    }


    /// A group is drawn by the window whose strip holds its slot, and its
    /// tab list travels with it — narrowing must not leave a group in one
    /// window quoting tab ids that another window owns.
    #[test]
    fn a_group_narrows_with_the_window_that_holds_it() {
        let mut snapshot = grouped();
        snapshot.windows = vec![
            eave_wire::WindowStrip {
                id: eave_wire::PRIMARY_WINDOW.into(),
                order: vec![Slot::Tab { id: "loose".into() }],
                active: Some("loose".into()),
            },
            eave_wire::WindowStrip {
                id: "w-2".into(),
                order: vec![Slot::Group { id: "grp".into() }],
                active: Some("g2".into()),
            },
        ];
        for t in &mut snapshot.tabs {
            if t.id != "loose" {
                t.window = Some("w-2".into());
            }
        }

        let primary = strip_for_window(&snapshot, eave_wire::PRIMARY_WINDOW);
        assert!(
            primary.groups.is_empty(),
            "the group leaked into the primary window"
        );
        assert!(primary.group_tabs.is_empty());

        let second = strip_for_window(&snapshot, "w-2");
        assert_eq!(second.groups.len(), 1);
        assert_eq!(second.group_tabs.len(), 1);
        assert_eq!(second.tabs.len(), 3);
    }
}
