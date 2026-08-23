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

use eave_wire::{LoadState, Slot, TabContent, TabInfo, TabsSnapshot};

/// What a session tab needs from the session rail in order to draw.
///
/// Handed in rather than looked up, so this module never learns what a
/// `SessionInfo` is and the fallback for a session the rail has never heard
/// of is a decidable, tested case rather than an `unwrap`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChip {
    /// The name the rail draws. Never mined from a transcript — see
    /// `snapshot.rs`'s "one rule a client must keep".
    pub label: String,
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
pub const NEW_TAB_LABEL: &str = "New tab";

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
    pub active: bool,
    pub pinned: bool,
    /// The group this tab is in, and where in its run it sits. `None` for a
    /// loose tab.
    pub run: Option<GroupRun>,
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
    if let Some(t) = tab.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return (t.to_string(), false);
    }
    if let Some(url) = tab.url.as_deref() {
        let typed = eave_wire::typing_form(url);
        if !typed.is_empty() {
            return (typed, false);
        }
        // `about:blank` has no typing form, and it IS where the tab is.
        return (url.to_string(), false);
    }
    (NEW_TAB_LABEL.to_string(), true)
}

/// Flatten the snapshot into the strip's single line.
///
/// `sessions` is the rail's own view, keyed by session id. A session tab
/// whose session is missing from it still draws — see
/// [`ORPHAN_SESSION_LABEL`].
pub fn items(snapshot: &TabsSnapshot, sessions: &HashMap<String, SessionChip>) -> Vec<StripItem> {
    let by_id: HashMap<&str, &TabInfo> = snapshot.tabs.iter().map(|t| (t.id.as_str(), t)).collect();
    let group_tabs: HashMap<&str, &Vec<String>> = snapshot
        .group_tabs
        .iter()
        .map(|(g, ids)| (g.as_str(), ids))
        .collect();
    let active = snapshot.active.as_deref();

    let mut out = Vec::with_capacity(snapshot.tabs.len() + snapshot.groups.len());
    for slot in &snapshot.order {
        match slot {
            Slot::Tab { id } => {
                if let Some(info) = by_id.get(id.as_str()) {
                    out.push(StripItem::Tab(strip_tab(info, active, None, sessions)));
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
                        out.push(StripItem::Tab(strip_tab(info, active, Some(run), sessions)));
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
) -> StripTab {
    let (kind, label, placeholder) = match &info.content {
        TabContent::Page => {
            let (label, placeholder) = page_label(info);
            (TabKind::Page, label, placeholder)
        }
        TabContent::Session { session_id } => {
            let chip = sessions.get(session_id).cloned();
            let (label, placeholder) = match &chip {
                Some(c) if !c.label.trim().is_empty() => (c.label.clone(), false),
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
    StripTab {
        id: info.id.clone(),
        kind,
        label,
        placeholder,
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
        run,
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
    for slot in &snapshot.order {
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
    let order = cycle_order(snapshot);
    if order.is_empty() {
        return None;
    }
    let at = snapshot
        .active
        .as_deref()
        .and_then(|a| order.iter().position(|t| t == a));
    let next = match at {
        Some(ix) if forward => (ix + 1) % order.len(),
        Some(ix) => (ix + order.len() - 1) % order.len(),
        None => 0,
    };
    Some(order[next].clone())
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
                page("d", Some("about:blank"), None),
            ],
            Some("a"),
        );
        let items = items(&s, &HashMap::new());
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
                ("about:blank", false),
            ]
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
        let items = items(&s, &chips(&[("sess-1", "refactor the fold")]));
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

    /// A session tab can outlive its session's place in the rail — the rail
    /// hides archived sessions. It still draws, and it does not draw a hex
    /// string nobody chose.
    #[test]
    fn a_session_tab_whose_session_the_rail_does_not_list_still_draws() {
        let s = snap(vec![session_tab("t", "sess-gone")], Some("t"));
        let items = items(&s, &HashMap::new());
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
        match &items(&s, &HashMap::new())[0] {
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
                last_active: None,
                created_ts_ms: 0,
            }],
            order: vec![
                Slot::Tab { id: "loose".into() },
                Slot::Group { id: "grp".into() },
            ],
            group_tabs: vec![("grp".into(), vec!["g1".into(), "g2".into(), "g3".into()])],
            active: Some("g2".into()),
            read_only: false,
        }
    }

    /// B1's one ordering, flattened in place, with the run marked so the
    /// renderer can draw it as one shape in a single pass.
    #[test]
    fn a_group_flattens_in_place_and_its_run_is_marked_at_both_ends() {
        let items = items(&grouped(), &HashMap::new());
        assert_eq!(items.len(), 5, "{items:?}");
        assert!(matches!(&items[0], StripItem::Tab(t) if t.id == "loose" && t.run.is_none()));
        match &items[1] {
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
        let runs: Vec<(bool, bool)> = items[2..]
            .iter()
            .filter_map(|i| match i {
                StripItem::Tab(t) => t.run.as_ref().map(|r| (r.first, r.last)),
                _ => None,
            })
            .collect();
        assert_eq!(runs, vec![(true, false), (false, false), (false, true)]);
    }

    /// A collapsed group hides its tabs from the strip but not from the
    /// keyboard, and it has to say that the active tab is inside it —
    /// otherwise activating a tab looks like nothing happened.
    #[test]
    fn a_collapsed_group_hides_its_tabs_but_still_counts_and_still_cycles() {
        let mut s = grouped();
        s.groups[0].collapsed = true;
        let items = items(&s, &HashMap::new());
        assert_eq!(items.len(), 2, "collapsed tabs were drawn: {items:?}");
        assert!(matches!(
            &items[1],
            StripItem::Group {
                count: 3,
                holds_active: true,
                collapsed: true,
                ..
            }
        ));
        assert_eq!(
            cycle_order(&s),
            vec!["loose", "g1", "g2", "g3"],
            "a collapsed group made its tabs unreachable by keyboard"
        );
    }

    /// Wrapping is the point: with two tabs open, a cycle that did not wrap
    /// would make the second press do nothing.
    #[test]
    fn ctrl_tab_walks_the_drawn_order_and_wraps_both_ways() {
        let s = grouped();
        assert_eq!(cycle_order(&s), vec!["loose", "g1", "g2", "g3"]);
        assert_eq!(cycle(&s, true).as_deref(), Some("g3"));
        assert_eq!(cycle(&s, false).as_deref(), Some("g1"));

        let mut last = s.clone();
        last.active = Some("g3".into());
        assert_eq!(cycle(&last, true).as_deref(), Some("loose"));
        let mut first = s.clone();
        first.active = Some("loose".into());
        assert_eq!(cycle(&first, false).as_deref(), Some("g3"));

        assert_eq!(cycle(&snap(vec![], None), true), None);
        // Nothing active: start at the beginning rather than refuse.
        let mut none = s.clone();
        none.active = None;
        assert_eq!(cycle(&none, true).as_deref(), Some("loose"));
    }

    /// Right, then left. Landing on the left neighbour each time walks the
    /// selection backwards through tabs already dealt with.
    #[test]
    fn closing_a_tab_lands_on_its_right_neighbour_then_its_left() {
        let s = grouped();
        assert_eq!(successor(&s, "loose").as_deref(), Some("g1"));
        assert_eq!(successor(&s, "g2").as_deref(), Some("g3"));
        assert_eq!(
            successor(&s, "g3").as_deref(),
            Some("g2"),
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
}
