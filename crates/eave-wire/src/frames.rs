//! Wire protocol between the engine and any client (spec 05-wire.md).
//!
//! Transport: UNIX domain socket at `<data_dir>/engine.sock`, ndjson — one
//! JSON object per `\n`-terminated line (spec 01 §Topology). Envelopes are
//! deliberately loose (`{id, method, params}` / `{id, ok|err|item}`) while
//! one binary ships both ends; the frame vocabulary is surface-agnostic so
//! a non-terminal client can exist later without protocol changes.
//!
//! # Handshake
//! The client's first request MUST be `hello` carrying its protocol
//! version. The engine answers `ok` with its own version and (pid,
//! start_time) identity. The client disconnects on mismatch — the engine
//! does not attempt cross-version service.
//!
//! # The (epoch, seq) cursor contract
//! An **epoch** identifies one contiguous, immutable view of a session's
//! record log held by a running engine. `seq` is the journal record seq
//! (spec 02). Within one epoch, event frames are strictly ordered by seq
//! with no gaps and no repeats — a client that last saw `seq` may resume
//! with `Cursor { epoch, seq }` and the engine replays from `seq + 1`.
//!
//! The epoch changes whenever the engine (re)opens the session from disk:
//! a `journal::load` may truncate a torn tail and append repair closers, so
//! seqs observed against the old epoch are not comparable to the new log.
//! On epoch mismatch — or no cursor at all — the engine sends `Reset` and
//! replays from seq 1 (spec 02 §Client resume). A client holding no durable
//! projection (the v0 GPUI shell after relaunch) MUST subscribe with no
//! cursor: the cursor only skips replay, it never recovers projection state
//! the client no longer has. UI is pure projection — resubscribe + replay,
//! never merge (locked decision).

use serde::{Deserialize, Serialize};

use eave_core::journal::{Header, Record};
use eave_core::proc::ProcIdentity;
use eave_core::session::AgentConfig;

pub use eave_core::attention::{AttentionSnapshot, WaitReason};
pub use eave_core::browser::bookmarks::{Bookmark, BookmarkId, BookmarkKind};
pub use eave_core::browser::history::{
    typing_form, HistoryEntry, HistoryMatch, MatchKind, Transition,
};
pub use eave_core::browser::permissions::{
    normalize_origin, Decision as PermissionDecision, Grant, PermissionKind, Permissions, Verdict,
};
pub use eave_core::browser::snapshot::{
    BookmarkNode, BookmarksSnapshot, PermissionsSnapshot, TabInfo, TabsSnapshot, WindowStrip,
};
pub use eave_core::browser::tabs::{
    GroupId, LoadState, NavEntry, NavStack, PendingNav, Setting, Slot, TabContent, TabGroup, TabId,
    WindowId, PRIMARY_WINDOW,
};

pub const PROTO_VERSION: u32 = 1;

/// Subscription resume position: the last (epoch, seq) the client has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub epoch: u64,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Client → engine

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Hello {
        proto: u32,
    },
    /// All sessions the engine knows (on disk or live).
    List,
    /// Create a session (fresh id if `None`) or load an existing one.
    /// Creating writes the journal header immediately; nothing is spawned
    /// until the first `Send`.
    Open {
        session_id: Option<String>,
        cwd: Option<String>,
        /// What agent this session should run (Slice P2). `None` = the
        /// engine's defaults, which is what every caller predating this
        /// field sends and why it is `#[serde(default)]`: an older client's
        /// `open` frame still parses, and behaves exactly as it did.
        ///
        /// Fixed at open. Re-opening an existing session with a DIFFERENT
        /// config is an error, not a silent substitution — the journal and
        /// the provider-side resume cursor were created under the original.
        /// A provider with no verified permission posture is refused here
        /// (Slice K's proof is Claude-specific); see
        /// `eave_core::session::posture_is_verified`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentConfig>,
    },
    /// Stream a session's record log. Replays per the cursor contract, then
    /// follows live. Items keep flowing on this request's id until the
    /// connection closes.
    Subscribe {
        session_id: String,
        cursor: Option<Cursor>,
    },
    /// Send one user message. Spawns the agent CLI first if the session is
    /// not live (`--resume` when a provider ran before, `--session-id`
    /// otherwise — spec 02 §Resume contract). Sends are sequential: the
    /// engine fully journals and attempts delivery of one send before the
    /// next — the order-paired delivery state depends on this (spec 02
    /// §Delivery state; do NOT pipeline).
    Send {
        session_id: String,
        text: String,
        /// Absolute paths of images to encode as native content blocks
        /// at send time. Empty (and omitted on the wire) for text-only
        /// sends. Bytes never travel this socket — the engine reads the
        /// file. `#[serde(default)]` so a client predating the field
        /// still parses as a text send.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    /// Kill the session's live process (verified kill ladder, spec 01).
    Kill {
        session_id: String,
    },
    /// Stop the in-flight turn without killing the process (composer Stop).
    /// Claude writes a `control_request` interrupt; other adapters no-op.
    Interrupt {
        session_id: String,
    },
    /// Observe the session SET (spec 05 §Watch): items flow on this
    /// request's id as `Sessions` snapshots — one immediately, one on
    /// every session-set / state / attention change. This is the only
    /// cross-session broadcast in the protocol and it is content-free by
    /// construction: its sole payload type is [`SessionInfo`]. A client
    /// must `Subscribe` to a specific session to receive records.
    Watch,
    /// Observe ONE session's changed-file overview (spec 05 §Files):
    /// items flow on this request's id as `Files` snapshots — the current
    /// one immediately (triggering a scan if none exists yet), a fresh
    /// one after every engine-side git poll. Strictly per-session: file
    /// data for session A never flows on any other session's files id,
    /// and never on watch (isolation invariant, enforced by test).
    Files {
        session_id: String,
        /// Which basis the snapshot is measured against (Slice AA).
        /// `#[serde(default)]` = `WorkingTree`, so every client predating
        /// this field asks for exactly what it always got.
        ///
        /// Scope is fixed for the life of the subscription: the engine
        /// only pays for a turn-scoped scan while somebody is subscribed
        /// at that scope (it costs a full working-tree hash pass), so a
        /// client that wants the other scope unsubscribes and asks again.
        #[serde(default, skip_serializing_if = "FileScope::is_default")]
        scope: FileScope,
    },
    /// One file's diff content (Slice B2), computed by the engine from
    /// `git diff` in the session cwd at request time — unary, not a
    /// subscription. There is deliberately no engine-held diff state to
    /// go stale: invalidation rides the `files` channel (a fresh
    /// [`FilesSnapshot`] means "re-request any diff you are showing").
    /// Every failure shape is a stated fact inside [`FileDiff`], never a
    /// hang — and every underlying git call is timeout-bounded.
    Diff {
        session_id: String,
        path: String,
        /// Which basis to diff against, matching the `files` list the row
        /// was clicked in.
        ///
        /// Without this the pane silently answered a different question
        /// than the list above it: toggle to "this turn", click a file,
        /// and the diff was still working-tree — HEAD-relative — so a file
        /// the turn touched showed every change since the last commit.
        /// `#[serde(default)]`, so a pre-AA client's frame is unchanged.
        #[serde(default, skip_serializing_if = "FileScope::is_default")]
        scope: FileScope,
    },
    /// Cancel a streaming request (`subscribe` / `watch` / `files`) by its
    /// envelope id, on this connection (Slice I1). The engine stops the
    /// stream's task and releases any subscriber-scoped resource it held
    /// (a `files` stream's FS-watcher refcount). Idempotent: an unknown or
    /// already-finished id still replies ok — the caller may race a
    /// disconnect or a stream the engine ended on its own. Without this, a
    /// client that switches sessions leaks the old stream for the
    /// connection's whole life; connection close remains the backstop.
    Unsubscribe {
        id: u64,
    },
    /// Move a session into its own git worktree (Slice I2). Allowed only
    /// while nothing has ever spawned (state `new`, no records) — after
    /// the first send the provider has seen the cwd and it is fixed for
    /// the session's life (`--resume` finds its history by cwd). The
    /// engine creates `<worktrees_root>/<repo>/<sid8>` on a fresh
    /// `eave/<sid8>` branch off the base repo's HEAD and rewrites the
    /// session's cwd to it. Isolation is the operator's choice, never
    /// automatic (Slice I2 decision 1): detection of a shared checkout is
    /// stated on `watch`; this method is the opt-in.
    Isolate {
        session_id: String,
    },
    /// Delete a session's eave-created worktree (Slice I2). The engine
    /// re-checks state freshly at call time: a LIVE session refuses (kill
    /// first); uncommitted files or commits unreachable from any other
    /// ref refuse unless `force` — the reply restates the counts so the
    /// operator confirms against facts, not a stale rail row (decision 2:
    /// eave never deletes dirty work without the operator meaning it).
    /// Clean removal also prunes and deletes the `eave/` branch.
    RemoveWorktree {
        session_id: String,
        force: bool,
    },
    /// Set (or clear, with `None`) a session's operator title (Stage AB).
    /// Normalized and length-capped engine-side
    /// (`eave_core::store::normalize_title`) and persisted in `meta.json`,
    /// so it survives a restart.
    ///
    /// **Operator-authored, never engine-derived.** The rail needs to say
    /// which session a row is, and the obvious source — the session's
    /// first `UserSent` — is transcript content, which may not cross
    /// `watch` (spec 05 §Watch). So the title arrives the only way a
    /// session fact legitimately can: someone typed it *as* the label.
    /// The engine never reads a journal to fill it; see
    /// `watch.rs::the_engine_never_mines_a_title_out_of_the_transcript`.
    Rename {
        session_id: String,
        /// `None` clears the title back to unstated. So does any string
        /// that normalizes to empty — a title of spaces is not a title.
        title: Option<String>,
    },
    /// File a session away, or bring it back (Stage AC).
    ///
    /// **One bit, and the journal is never touched.** Archiving means
    /// exactly one thing — the default rail listing does not draw this
    /// session — and it is stored beside the title in `meta.json`, so it
    /// survives a restart and can be flipped back. Nothing else changes:
    /// an archived session still loads, still replays, still answers
    /// `subscribe`, and can still be sent to. eave deletes nothing here;
    /// destruction is `remove_worktree`'s business and its own two-step.
    ///
    /// Setting and clearing are the same verb for the same reason
    /// [`Request::Rename`] takes an `Option`: two methods for one bit is
    /// two things that can disagree about which is authoritative.
    ///
    /// Allowed in every state, including while live. There is nothing to
    /// serialize against, and the engine deliberately does not refuse a
    /// running session — the operator may legitimately file away a
    /// session they have stopped watching. What must never happen is a
    /// session parked on a permission question disappearing where nobody
    /// can answer it, and that is a LISTING rule, kept by the client that
    /// draws the list, not by refusing the bit.
    Archive {
        session_id: String,
        /// `true` files it away; `false` brings it back.
        archived: bool,
    },
    /// Answer one open permission question (Slice K). The agent is parked
    /// until this arrives and there is no other way out — eave never
    /// auto-approves, never auto-denies, and never times out into either.
    /// Reply [`DecideOk`]; an id that is no longer open is a stated fact
    /// (`answered: false`), not an error: the provider may have withdrawn
    /// the question under the operator's click.
    Decide {
        session_id: String,
        request_id: String,
        #[serde(flatten)]
        decision: Decision,
    },

    // -----------------------------------------------------------------
    // Browser state (Slice B1). Data only — none of these render
    // anything, and none of them know which page engine is attached.
    //
    // Every variant below is either an OPERATOR intent (open this, move
    // that) or an ENGINE fact (this navigation committed, this page
    // finished loading). Keeping the two vocabularies in one enum but
    // distinct in name is deliberate: `tab_navigate` is a request that
    // may not land, `tab_stated` is a report of what did.
    /// Observe the whole tab strip (Slice B1): items flow on this
    /// request's id as [`SubscriptionItem::Tabs`] snapshots — one
    /// immediately, one on every change. Fold by REPLACEMENT.
    ///
    /// Engine-wide, like `watch`, and carrying no session's transcript:
    /// a tab that views a session names it by id and nothing else.
    WatchTabs,
    /// Observe the bookmark hierarchy. Same contract as
    /// [`Request::WatchTabs`].
    WatchBookmarks,
    /// Observe every permission answer (G1). Same contract as
    /// [`Request::WatchTabs`].
    ///
    /// A subscription rather than a query, and that is forced rather than
    /// chosen: the page engine is asked its question **synchronously, on
    /// the app's own stack**, by Chromium — there is no point in that
    /// callback where a round trip to another process could be awaited.
    /// So the app holds a mirror of this snapshot and reads it locally.
    /// The mirror is a copy; the engine is the record.
    WatchPermissions,
    /// One tab's full back/forward stack — unary, deliberately not on the
    /// snapshot. See `eave_core::browser::snapshot`'s module docs.
    TabNav {
        tab_id: TabId,
    },
    /// Ask history what to suggest for what has been typed so far.
    /// Unary and stateless: the omnibox asks again on every keystroke,
    /// so there is nothing to hold and nothing to invalidate.
    HistoryQuery {
        typed: String,
        /// `#[serde(default)]` so a client that sends none gets the
        /// engine's own page size rather than zero results.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },

    /// Open a tab. The engine mints the id and returns it.
    TabOpen {
        /// Which window's strip it lands in (Slice W1). `None` is the
        /// primary window, so a client that has never heard of windows
        /// sends no key and gets the one strip it has always had.
        ///
        /// **The client names this, not the engine.** A window is the
        /// client's surface — see [`eave_wire::WindowId`] — and ⌘N has
        /// to be able to put a tab in a window that exists on screen
        /// before any reply has come back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupId>,
        /// Where in its container it lands; past the end appends, and
        /// `None` appends. Not a bare `usize` defaulting to 0, because
        /// that would silently make every new tab open first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// What the tab shows. Defaults to an ordinary page, so a client
        /// that has never heard of session tabs sends no key at all.
        #[serde(default, skip_serializing_if = "TabContent::is_default_content")]
        content: TabContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opener: Option<TabId>,
        /// Make it the active tab. Defaults to false so the field's
        /// absence means the least surprising thing — a background tab
        /// does not steal focus.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        activate: bool,
        /// A private tab. Not journaled, not history, Chromium in-memory
        /// storage. Skipped when false so an older engine still parses.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        incognito: bool,
    },
    /// Close a tab. **A tab that views a session does not end that
    /// session** — see [`TabContent::Session`]. Closing a view is not
    /// closing the work.
    TabClose {
        tab_id: TabId,
    },
    TabActivate {
        tab_id: TabId,
    },
    /// Move a tab. `group: None` means the top level, which is why this
    /// is a plain `Option` and not a [`Setting`]: a move always names its
    /// destination, so there is no "leave it alone" to express.
    TabMove {
        tab_id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupId>,
        #[serde(default)]
        index: usize,
    },
    TabPin {
        tab_id: TabId,
        pinned: bool,
    },
    /// Walk the back/forward stack. `-1` is back, `+1` forward; a move
    /// past either end is clamped and lands as a no-op rather than an
    /// error, because the operator may have raced a navigation.
    TabGo {
        tab_id: TabId,
        delta: i32,
    },
    /// Ask a tab to navigate. The engine records the intent as a
    /// committed navigation in this slice, because no page engine is
    /// attached yet to say otherwise — the caveat is stated in
    /// `docs/reports/stage-b1-browser-state.md`.
    TabNavigate {
        tab_id: TabId,
        url: String,
    },
    /// **The page-engine seam.** Whatever renders pages states what it
    /// observed here, and this is the only door those facts come through.
    /// It is one method rather than five so that adding a fact later does
    /// not add a method — [`PageFact`] carries its own tag.
    TabStated {
        tab_id: TabId,
        #[serde(flatten)]
        fact: PageFact,
    },

    GroupCreate {
        /// Which window's strip the group sits in (Slice W1). `None` is
        /// the primary window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        /// The checkout this group is about, when it is about one. Stated
        /// only; nothing in the engine acts on it (Slice B1 decision).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    /// Change a group's operator-set fields. Each is a [`Setting`] inside
    /// an `Option`, because three states are needed and two would not do:
    /// absent means leave alone, `{"set":"clear"}` means clear it, and
    /// `{"set":"value",…}` sets it.
    GroupUpdate {
        group_id: GroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<Setting<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collapsed: Option<bool>,
    },
    GroupMove {
        group_id: GroupId,
        #[serde(default)]
        index: usize,
    },
    /// Close a group and every tab in it. Tabs in the group that view
    /// sessions still do not end those sessions.
    GroupClose {
        group_id: GroupId,
    },

    /// Add a bookmark. `url` present makes a link, absent makes a folder
    /// — one field rather than a kind tag, because "a bookmark with a
    /// url" is what every caller actually has in hand.
    BookmarkAdd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<BookmarkId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Remove a bookmark. A folder takes its subtree with it.
    BookmarkRemove {
        bookmark_id: BookmarkId,
    },
    BookmarkMove {
        bookmark_id: BookmarkId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<BookmarkId>,
        #[serde(default)]
        index: usize,
    },
    BookmarkRename {
        bookmark_id: BookmarkId,
        /// `None` clears the name, exactly as [`Request::Rename`] does
        /// for a session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Point an existing link somewhere else. Separate from a rename
    /// because a title and an address are different things to get wrong.
    BookmarkRetarget {
        bookmark_id: BookmarkId,
        url: String,
    },

    /// File a parsed import from another browser. The client parses (the
    /// engine does not need `$HOME`, and a test can send a fixture tree);
    /// the engine applies in one batch and answers [`ImportOk`].
    BrowserImport {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bookmarks: Vec<ImportedBookmark>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        history: Vec<ImportedVisit>,
    },

    /// Forget one url. The visits are physically removed from the log,
    /// not tombstoned — see `eave_core::browser::history`.
    HistoryForget {
        url: String,
    },
    /// Forget a time range. Both bounds absent means everything.
    HistoryClear {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_ms: Option<u64>,
    },

    /// Write down what a person answered about one origin (G1).
    ///
    /// `permissions` is the whole ask, not one bit of it: a grouped
    /// camera-and-microphone prompt takes one answer, so both are recorded
    /// with it. The engine refuses to store an origin it cannot name and a
    /// capability it has no word for — see
    /// `eave_core::browser::permissions`.
    PermissionSet {
        origin: String,
        permissions: Vec<PermissionKind>,
        decision: PermissionDecision,
    },
    /// Take an answer back. `permission: None` forgets everything this
    /// origin was ever answered. The grant is physically removed from the
    /// log, not tombstoned — same rule as [`Request::HistoryForget`].
    PermissionForget {
        origin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission: Option<PermissionKind>,
    },
    /// Forget every origin's answers.
    PermissionClear,
}

/// Something a page engine observed and is reporting.
///
/// These are facts, not requests. `Committed` in particular is the url
/// that actually landed — after redirects — which is why a navigation
/// request and a navigation commit are different frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum PageFact {
    /// A navigation committed. Pushes a back/forward entry.
    Committed {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// The page stated its title (usually after the commit, once it has
    /// parsed). `None` clears it back to unstated.
    Titled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Where the favicon lives. The bytes are the engine's cache, not
    /// eave's business.
    Favicon {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        favicon_url: Option<String>,
    },
    /// Loading state. **Never journaled** — a restored tab is
    /// [`LoadState::Unloaded`], because that is what it is.
    ///
    /// Slice E2: the page engine reports the back/forward arrows on every
    /// load-state change, on this same fact, so they travel through
    /// `tab_stated` onto the snapshot the pane header and the chords read.
    /// Absent (an older engine, a test that is not about history) leaves
    /// the snapshot on the nav stack — the D1 default, not a hardcoded
    /// false.
    Load {
        #[serde(flatten)]
        load: LoadState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        can_go_back: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        can_go_forward: Option<bool>,
    },
}

/// The two answers, and only two (Slice K). `Deny` carries the operator's
/// own words, which the provider hands to the model verbatim as the tool
/// result — "denied, commit first" is a different instruction from
/// "denied" (capture D, 2026-08-18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny {
        #[serde(default)]
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Engine → client

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBody {
    /// Unary success. Payload shape depends on the request (`HelloOk`,
    /// `ListOk`, `OpenOk`, `SendOk`, `KillOk`) — loose on purpose.
    Ok(serde_json::Value),
    Err {
        message: String,
    },
    /// One subscription frame; carries the `Subscribe` request's id.
    Item(SubscriptionItem),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubscriptionItem {
    /// Drop everything you hold for this session and expect replay from
    /// seq 1. Sent when no/mismatched cursor was presented, and again
    /// mid-stream if the engine's broadcast lagged past the subscriber.
    Reset {
        session_id: String,
        epoch: u64,
        header: Header,
    },
    /// A batch of records, contiguous by seq. Batching is the engine's
    /// strict 120 ms delta folding (spec 05 §Engine semantics) — pacing
    /// only; a batch boundary carries no meaning.
    Events {
        session_id: String,
        epoch: u64,
        records: Vec<Record>,
    },
    /// Transient attention overlay (spec 01 §States): the engine's live
    /// classification of `running`/`quiet`/`unresponsive`/`failed` with
    /// its evidence. Deliberately NOT a journal record — attention is
    /// derived, reversible surface state, not history; it carries no seq
    /// and never advances the cursor. Emitted on every transition, again
    /// on evidence updates, and once after replay on subscribe; `Running`
    /// means "clear any notice".
    Attention {
        session_id: String,
        attention: AttentionSnapshot,
    },
    /// Watch frame (spec 05 §Watch): a full session-set snapshot taken at
    /// emission time. Clients fold by REPLACEMENT, never merge — there
    /// are no deltas, so there are no ordering hazards. Isolation
    /// invariant (enforced by test): this is the only frame a `watch`
    /// subscription ever carries, and [`SessionInfo`] holds session
    /// identity and state ONLY — never journal records, never provider
    /// output, never transcript content of any session.
    Sessions { sessions: Vec<SessionInfo> },
    /// Files frame (spec 05 §Files): one session's full changed-file
    /// snapshot, taken by the engine's git poll at emission time. Fold by
    /// REPLACEMENT — no deltas, nothing to merge or go stale. Flows only
    /// on the `files` subscription for exactly this session.
    Files {
        session_id: String,
        snapshot: FilesSnapshot,
    },
    /// Tab-strip frame (Slice B1): the whole strip, taken at emission
    /// time. Fold by REPLACEMENT, like every other snapshot here.
    ///
    /// Engine-wide. It carries tab ids, urls, page titles and — for a tab
    /// that views a session — that session's id. It carries **no
    /// session's transcript**, which is the same rule `watch` keeps and
    /// for the same reason: this is a broadcast every client sees.
    Tabs { snapshot: TabsSnapshot },
    /// Bookmark-hierarchy frame (Slice B1). Same contract.
    Bookmarks { snapshot: BookmarksSnapshot },
    /// Permission-answer frame (G1). Same contract.
    Permissions { snapshot: PermissionsSnapshot },
}

/// Unary reply to [`Request::Decide`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecideOk {
    /// The decision reached the agent. `false` = the question was no
    /// longer open (already answered, or withdrawn under the click).
    pub answered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloOk {
    pub proto: u32,
    pub engine: ProcIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// The operator's own name for this session (Stage AB). `None` = never
    /// named, which is the honest default and renders as no line at all —
    /// eave does not invent a title.
    ///
    /// **Read this before making it automatic.** The natural implementation
    /// is to derive it from the session's first `UserSent`, capped. That is
    /// forbidden here, and the reason is the isolation invariant (spec 05
    /// §Watch): `watch` is the ONE cross-session broadcast, and it may
    /// carry facts ABOUT a session and never a byte of any session's
    /// transcript. A prompt's first words are the transcript. The raw-byte
    /// canary in `eave-engine/tests/watch.rs` proves the rule holds, and it
    /// plants its marker at the head of the first prompt — so a
    /// leading-words title does not merely violate the invariant in
    /// principle, it fails that test by name.
    ///
    /// The distinction this field keeps is the same one
    /// [`SessionInfo::pending_approvals`] keeps: a COUNT of open questions
    /// may cross, the tool and path may not. Here, a label the operator
    /// chose may cross, the words the agent was given may not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The operator filed this session away (Stage AC). A fact ABOUT the
    /// session — an opinion the operator typed, in the same class as
    /// `title` — so the isolation invariant (§Watch) lets it cross: it
    /// says nothing about what any agent was asked to do.
    ///
    /// **The engine reports archived sessions like any other.** Filtering
    /// is the client's job, and that is deliberate: a listing that simply
    /// omitted them would leave the bit unreachable from the surface that
    /// set it, so un-archiving would need a second listing mode to exist
    /// at all. It also keeps the failure mode benign — a client that has
    /// never heard of this field renders exactly the list it always did.
    ///
    /// `#[serde(default)]` + `skip_serializing_if`, so an engine predating
    /// the field puts no key on the wire and a client predating it ignores
    /// one that is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    pub provider: String,
    /// The model this session is configured to run (Slice P2), verbatim as
    /// it will be passed to the CLI. `None` = the provider's own default,
    /// or a session recorded by an engine predating per-session config —
    /// never a guessed name.
    ///
    /// A fact ABOUT the session, like `provider` and `cwd`, so the watch
    /// channel may carry it: the isolation invariant bars transcript
    /// CONTENT, and which model was configured is not content. That is what
    /// lets a rail state every session's model without subscribing to any
    /// of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub cwd: String,
    pub created_ts_ms: u64,
    /// When this session's reported [`state`](SessionInfo::state) last
    /// moved (Stage AB), for the rail's ordering. `None` = it has not moved
    /// since the engine loaded the session — sort on `created_ts_ms`
    /// instead, never on a fabricated zero, which would sort every
    /// never-started session to 1970.
    ///
    /// **Stamped only where `state`/`live` change, and nowhere else.** That
    /// is what keeps it inside the isolation invariant rather than beside
    /// it: a timestamp that advanced per journal record would turn `watch`
    /// into a side channel reporting the *rate* of a session's transcript
    /// to every client — content inferred rather than sent. Stamping
    /// exactly the transitions `state` already announces adds no
    /// observation a watcher did not already have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// Current state name (spec 01 §States; "new" before any spawn).
    pub state: String,
    /// A live agent process is attached right now.
    pub live: bool,
    pub epoch: u64,
    pub last_seq: u64,
    /// Canonical checkout root for the session cwd (Slice I2): the git
    /// worktree toplevel when the cwd is inside one, the canonical cwd
    /// itself otherwise. `None` = not determined yet (the probe runs off
    /// the open/load path) or an engine predating I2. Two sessions with
    /// the same root work in the SAME files — the collision fact
    /// `shared_with` is derived from equality of this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_root: Option<String>,
    /// Other sessions whose checkout root equals this one's (Slice I2),
    /// stamped by the engine at snapshot time. Identity-only, like every
    /// watch fact: session ids, no content. Empty = sole occupant (or
    /// root not yet known). Sharing is legitimate and merely stated
    /// (decision 1); isolation is the operator's opt-in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
    /// Present when the session's cwd is an eave-created worktree
    /// (Slice I2). Facts only — the rail states them; nothing acts on
    /// them automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    /// Resident memory held by this session's agent process right now, in
    /// bytes (Stage L). `None` when nothing is live, when the probe
    /// failed, or from an engine predating this field — never a zero,
    /// which would read as "this agent costs nothing".
    ///
    /// A quantity, like `pending_approvals` is a count: the watch
    /// isolation invariant (§Watch) bars transcript CONTENT, and how much
    /// memory a process holds is a fact about the machine, not about what
    /// the operator asked the agent to do. It is what lets the rail state
    /// an aggregate ceiling for every session at once without subscribing
    /// to any of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    /// Open permission questions right now (Slice K). A COUNT, deliberately
    /// — the watch channel may say that a session is waiting on the
    /// operator and how many times, and may never say which tool or which
    /// path. Those are transcript content and travel only on that
    /// session's own `subscribe` stream (isolation invariant, §Watch).
    /// This is what makes a BACKGROUND session's pending approval visible
    /// in the rail without switching to it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pending_approvals: u64,
    /// The permission posture the agent CLI reported at `init` (Slice K):
    /// the mode it says it is actually running in, not the one eave asked
    /// for. `None` before the first spawn, or for a session recorded by an
    /// engine predating Slice K. The rail states it, because a session in
    /// a mode that never asks must not look like one that does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// A session's eave-created worktree, as last observed (Slice I2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// The base repo's worktree root the isolation was created from.
    pub base: String,
    /// The dedicated branch (`eave/<sid8>` — sid8 = the first 8 chars of
    /// the session id; predictable, session-owned, safe to delete on
    /// clean removal).
    pub branch: String,
    /// The worktree directory existed at the last check. `false` is the
    /// stated "deleted out from under the session" fact — eave never
    /// recreates it.
    pub present: bool,
    /// Changed-file count from the engine's last git scan of the
    /// worktree (uncommitted work — the thing decision 2 protects).
    /// `None` = not scanned yet, an honest unknown, never a fake zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_files: Option<u64>,
}

/// One session's changed-file overview (Slice B, spec 05 §Files).
///
/// Truth source is engine-side `git status` / `git diff` in the session
/// cwd — NEVER derived from tool records, which describe intent, not
/// disk. Ephemeral engine state, never journaled: git itself is the
/// store of record and survives restarts for free. A non-git cwd or a
/// failed git invocation is a stated fact (`state`), not an error path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesSnapshot {
    pub state: GitState,
    /// Empty when `state` is not `Ok` — and also when the repo is clean.
    pub files: Vec<ChangedFile>,
    /// Files beyond the engine's list cap (a 10k-changed-file repo must
    /// not ship a 10k-row frame on every refresh). Zero = the list is
    /// complete; non-zero is a stated fact the rail must render ("… and
    /// N more"), never silent truncation.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
    /// True when an FS watcher covered the session cwd at send time —
    /// the list refreshes live as files change. False = the polling
    /// fallback: it refreshes at turn boundaries and may be stale during
    /// a long turn. Stamped per frame (not per scan) so a watcher that
    /// breaks mid-flight re-reports truthfully on the next snapshot; a
    /// B1 engine's frames default to false, which is accurate — B1 only
    /// polled. The rail states this mode (a list that looks live but
    /// isn't would lie by omission).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watching: bool,
    /// What this list is measured AGAINST (Slice AA). The default —
    /// and every engine predating the field — is [`DiffBasis::WorkingTree`],
    /// which is exactly what those engines produced.
    ///
    /// The whole point of the field is that "this turn changed nothing"
    /// and "there is no turn to measure against" are different answers
    /// and must not share a rendering. A turn-scoped list whose basis is
    /// gone carries [`DiffBasis::Unavailable`] with git's or the engine's
    /// own words, and `files` is empty because there is nothing to say —
    /// never because nothing changed.
    #[serde(default, skip_serializing_if = "DiffBasis::is_default")]
    pub basis: DiffBasis,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Which basis a `files` subscription asks the engine to measure against
/// (Slice AA, spec 05 §Files).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileScope {
    /// Everything uncommitted: the working tree against `HEAD`. The
    /// original Slice B behaviour and still the default.
    #[default]
    WorkingTree,
    /// Only what changed since the most recent turn was dispatched —
    /// "what did THIS turn do", separated from whatever was already
    /// dirty and from whatever the operator edited between turns.
    LatestTurn,
}

impl FileScope {
    fn is_default(&self) -> bool {
        matches!(self, FileScope::WorkingTree)
    }
}

/// What a [`FilesSnapshot`] is measured against — a stated fact on every
/// frame, because the two ways a turn-scoped list can be empty mean
/// opposite things.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffBasis {
    /// `HEAD` plus the index — everything uncommitted.
    #[default]
    WorkingTree,
    /// The tree captured when the current turn was dispatched. `at` is
    /// the capture time (ms since epoch), so a surface can say WHICH turn
    /// it is showing rather than just "the last one".
    Turn { at: u64 },
    /// A turn basis was asked for and there is none to answer with. The
    /// reason is the engine's or git's own words and is meant to be
    /// rendered verbatim: no turn has been dispatched since this engine
    /// started (a restart drops it — it is memory, not history), the
    /// capture failed, or the base tree was pruned out of the object
    /// store by a `git gc` while the turn ran.
    Unavailable { reason: String },
}

impl DiffBasis {
    fn is_default(&self) -> bool {
        matches!(self, DiffBasis::WorkingTree)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitState {
    /// The cwd is a work tree and the scan succeeded (`files` may still
    /// be empty: a clean repo is a fact, not an absence).
    Ok,
    /// The session cwd is not inside a git work tree. A fact to render,
    /// not a failure.
    NotARepo,
    /// Git ran, answered, and said no — a bad revision, a corrupt index, a
    /// vanished cwd. The message is git's own first stderr line and is
    /// shown verbatim; the previous snapshot is gone.
    Error { message: String },
    /// **Git did not answer.** The process was still running when the
    /// budget expired, so nothing is known about the repo — and, crucially,
    /// nothing is known to be *wrong with git*. Kept apart from
    /// [`GitState::Error`] because collapsing the two is what produced
    /// `git failed: git status timed out after 10s` on a machine where
    /// `git status` takes 36 ms.
    ///
    /// `gate` is `Some(dir)` when the cwd sits inside a directory macOS
    /// puts behind a Files-and-Folders grant. A *pending* grant blocks the
    /// syscall rather than refusing it, so a timeout there is not a
    /// symptom of anything being broken: it is the OS waiting for the
    /// person to answer a dialog. A *refused* grant returns `EPERM`
    /// immediately and arrives as [`GitState::Error`] instead, which is
    /// why the hint belongs on this variant and only on this one.
    Unanswered {
        /// The subcommand that did not come back, e.g. `status`.
        what: String,
        secs: f32,
        gate: Option<String>,
    },
}

impl GitState {
    /// The sentence to put in front of a person.
    ///
    /// It lives here, once, because the defect this replaces was **two call
    /// sites each adding their own frame**: the engine wrote
    /// `git status timed out after 10s` and the app wrapped it in
    /// `git failed: …`, producing a sentence that blamed git twice on a
    /// machine where `git status` took 36 ms. Neither half was willing to
    /// own the wording, so both invented some.
    ///
    /// Callers render this verbatim and add nothing.
    pub fn stated(&self) -> String {
        match self {
            GitState::Ok => "git gave no answer".into(),
            GitState::NotARepo => "not a git repository".into(),
            GitState::Error { message } => format!("git failed: {message}"),
            GitState::Unanswered {
                what,
                secs,
                gate: Some(dir),
            } => format!("git {what} did not answer in {secs:.0}s — {dir} is a folder macOS gates"),
            GitState::Unanswered {
                what,
                secs,
                gate: None,
            } => format!("git {what} did not answer in {secs:.0}s"),
        }
    }

    /// What the person can DO about it, when there is anything.
    ///
    /// Separate from [`GitState::stated`] on this project's standing
    /// pattern — state the fact, then offer the affordance — and phrased as
    /// a condition rather than a diagnosis, because a timeout inside a
    /// gated folder is *evidence* of a pending grant and not proof of one.
    /// A git call can be slow in `~/Desktop` for ordinary reasons, and a
    /// message that asserted the cause would be making the same mistake in
    /// the other direction.
    pub fn remedy(&self) -> Option<String> {
        match self {
            GitState::Unanswered { gate: Some(_), .. } => Some(
                "If eave has not been granted access, approve the prompt — or add it \
                 under System Settings › Privacy & Security › Files and Folders."
                    .into(),
            ),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Repo-relative path (the post-rename path for renames).
    pub path: String,
    pub status: FileStatus,
    /// Added/removed line counts from `git diff --numstat` (untracked
    /// files are counted by the engine). `None` = unknowable: binary
    /// file, or too large to count.
    pub adds: Option<u64>,
    pub dels: Option<u64>,
    /// For renames: the pre-rename path (B2 — the diff needs both sides
    /// in its pathspec, and "renamed from X" is a fact worth stating).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    /// The porcelain entry described an untracked file (B2: its "diff"
    /// is the file's own content — `git diff HEAD` cannot see it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untracked: bool,
}

/// Worktree-level status, folded from git's two-letter XY codes: the
/// rail states what happened to the file, not the index/worktree split
/// (staging is explicitly out of scope for Slice B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// New relative to HEAD — includes untracked files.
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Unmerged (conflict) entry.
    Conflicted,
}

impl FileStatus {
    /// One-letter glyph for compact rendering.
    pub fn glyph(&self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Conflicted => "U",
        }
    }
}

/// Unary reply to [`Request::Diff`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffOk {
    pub diff: FileDiff,
}

/// One file's diff content (Slice B2), as git states it at request time.
/// Ephemeral by construction: computed on demand, held nowhere — a fresh
/// [`FilesSnapshot`] on the `files` channel is the invalidation signal.
/// Every shape a diff can take is a stated fact, including the ones that
/// have no text to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub kind: DiffKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffKind {
    /// Unified-diff hunks. Empty `hunks` is itself a fact ("no line
    /// changes" — a pure rename or a mode-only change).
    Text {
        hunks: Vec<DiffHunk>,
        /// The engine hit its byte or line cap: these hunks are the head
        /// of a larger diff. The rail states it; the full diff stays in
        /// git where it belongs.
        truncated: bool,
    },
    /// Binary content differs; there is no text diff to show.
    Binary,
    /// A submodule pointer moved (and/or its worktree is dirty). The
    /// detail is the fact to render, e.g. `abc1234 → def5678 (dirty)`.
    Submodule { detail: String },
    /// An untracked symlink: its "content" is where it points.
    Symlink { target: String },
    /// Git could not answer (timeout, vanished file, unreadable repo).
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// The `@@ -a,b +c,d @@ context` line, verbatim.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    /// Line content without the leading `+`/`-`/` ` marker — the op
    /// carries that; the rail renders it as a wash, not a glyph.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Add,
    Del,
    Ctx,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListOk {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOk {
    pub info: SessionInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendOk {
    /// Why the write did not reach the agent, when `delivered` is false.
    ///
    /// `#[serde(default)]` so a pre-existing engine's reply still parses.
    /// This exists because "delivered: false" with no cause is a dead end:
    /// the reason was discarded at the source for the life of the project
    /// and no test that hit it could report anything but the assertion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The pipe write reached the agent's stdin. `false` means the message
    /// is journaled-but-undelivered — the `UserSent` record will never gain
    /// a `UserDelivered` pair, and no surface may render it as sent
    /// (spec 02 §Delivery state).
    pub delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillOk {
    pub escalated: bool,
    pub already_exited: bool,
}

/// Reply to [`Request::Interrupt`]. Empty on purpose: the interrupt is
/// fire-and-forget on the CLI's stdin; there is no "it stopped" ack on
/// the wire besides the turn settling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InterruptOk {}

/// Reply to [`Request::Unsubscribe`] (Slice I1). `stopped` is a fact, not
/// a success bit: `false` means the id named no live stream on this
/// connection (already ended, never existed, or a raced disconnect) —
/// equally fine from the caller's side, stated rather than silent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsubscribeOk {
    pub stopped: bool,
}

/// Reply to [`Request::Isolate`] (Slice I2): the session as it now
/// stands, cwd rewritten to the fresh worktree. Failure shapes (already
/// spawned, live, non-git cwd, `git worktree add` refused) are `Err`
/// replies carrying git's or the engine's stated reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IsolateOk {
    pub info: SessionInfo,
}

/// Reply to [`Request::Rename`] (Stage AB): the session as it now stands,
/// so the caller reads back the NORMALIZED title rather than the string it
/// sent. Those differ whenever the engine trimmed, folded a newline, or hit
/// the length cap — and a caller that assumed its own string landed would
/// render a title the engine does not hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenameOk {
    pub info: SessionInfo,
}

/// Reply to [`Request::Archive`] (Stage AC). Carries the whole
/// [`SessionInfo`] for the same reason [`RenameOk`] does: the caller reads
/// the bit back off the session as the engine now holds it, rather than
/// assuming the value it sent is the one that landed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveOk {
    pub info: SessionInfo,
}

/// Reply to [`Request::RemoveWorktree`] (Slice I2). `removed: false` is
/// a refusal with its facts: the counts the operator must confirm
/// against (`force: true` re-sends meaning it). The counts are measured
/// freshly at call time — never the rail's possibly-stale numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoveWorktreeOk {
    pub removed: bool,
    /// Uncommitted changed files in the worktree at check time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_files: Option<u64>,
    /// Commits on the `eave/` branch unreachable from any other ref —
    /// removal deletes the branch, so these would be lost too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmerged_commits: Option<u64>,
}

/// What a spawned engine process reports on stdout before exiting 42 when
/// it lost the instance lock: the holder from `engine.lock`, so the UI can
/// render Busy with a pid even though it never got a socket to talk to
/// (spec 02 §Contended start).
/// Reply to [`Request::TabOpen`] and [`Request::GroupCreate`]: the id the
/// engine minted. The caller reads its own new object's identity back
/// rather than guessing it, exactly as [`RenameOk`] returns the normalized
/// title instead of letting the caller assume its string landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedOk {
    pub id: String,
}

/// One node of a [`Request::BrowserImport`] tree. `url` present makes a
/// link, absent makes a folder — the same rule as [`Request::BookmarkAdd`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedBookmark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ImportedBookmark>,
}

/// One url of a [`Request::BrowserImport`] history batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedVisit {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub visit_count: u32,
    pub last_visit_ms: u64,
}

/// Reply to [`Request::BrowserImport`]: how many nodes / urls landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOk {
    pub bookmarks: u32,
    pub history: u32,
}

/// Reply to every browser mutation that does not create something.
///
/// `changed: false` is a FACT, not a failure: pinning an already-pinned
/// tab, moving a tab to where it already is, or renaming a bookmark to the
/// name it has all land here. The engine neither journals nor broadcasts a
/// no-op — a click that changed nothing must not re-send the whole strip to
/// every client — and the caller is entitled to know that is what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedOk {
    pub changed: bool,
}

/// Reply to [`Request::TabNav`]: one tab's whole back/forward stack, which
/// is deliberately absent from the strip snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabNavOk {
    pub tab_id: TabId,
    #[serde(flatten)]
    pub nav: NavStack,
}

/// Reply to [`Request::HistoryQuery`]: what to suggest, best first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryQueryOk {
    pub matches: Vec<HistoryMatch>,
    /// How many distinct urls history holds. A count may cross a channel
    /// the urls themselves do not — and an omnibox showing nothing wants
    /// to distinguish "no history" from "no match".
    #[serde(default, skip_serializing_if = "is_zero_usize_wire")]
    pub total_entries: usize,
    /// How long the engine took to answer, in microseconds. Stated rather
    /// than assumed: this path has a frame budget, and a surface that can
    /// see the number can prove it is being met on the operator's own
    /// history rather than on a benchmark's.
    #[serde(default, skip_serializing_if = "is_zero_u64_wire")]
    pub took_us: u64,
}

fn is_zero_usize_wire(n: &usize) -> bool {
    *n == 0
}

fn is_zero_u64_wire(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusyReport {
    pub busy: BusyHolder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusyHolder {
    pub holder: Option<ProcIdentity>,
}

/// Exit code of an engine process that lost the instance lock.
pub const ENGINE_BUSY_EXIT: i32 = 42;

/// Socket path for a data dir. UNIX `sun_path` is ~104 bytes on macOS —
/// callers with deep data dirs will get a bind/connect error, not silent
/// truncation; tests use short dirs for this reason.
pub fn socket_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("engine.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_round_trip() {
        let req = RequestEnvelope {
            id: 7,
            request: Request::Subscribe {
                session_id: "s1".into(),
                cursor: Some(Cursor { epoch: 9, seq: 41 }),
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"subscribe\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        let resp = ResponseEnvelope {
            id: 7,
            body: ResponseBody::Item(SubscriptionItem::Events {
                session_id: "s1".into(),
                epoch: 9,
                records: vec![],
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        assert!(line.contains("\"item\""), "{line}");
        let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn a_text_only_send_omits_images_and_an_old_frame_still_parses() {
        let req = Request::Send {
            session_id: "s1".into(),
            text: "go".into(),
            images: vec![],
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(
            !line.contains("images"),
            "empty images must skip_serializing: {line}"
        );
        let old = r#"{"method":"send","params":{"session_id":"s1","text":"go"}}"#;
        let back: Request = serde_json::from_str(old).unwrap();
        assert_eq!(back, req);

        let with = Request::Send {
            session_id: "s1".into(),
            text: "look".into(),
            images: vec!["/tmp/shot.png".into()],
        };
        let line = serde_json::to_string(&with).unwrap();
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, with);
    }

    #[test]
    fn unsubscribe_round_trips_with_distinct_ids() {
        // The envelope id and the TARGET id are different fields on the
        // same line — the wire shape must keep them apart.
        let req = RequestEnvelope {
            id: 11,
            request: Request::Unsubscribe { id: 10 },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"unsubscribe\""), "{line}");
        assert!(line.contains("\"params\":{\"id\":10}"), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        let ok = UnsubscribeOk { stopped: true };
        let back: UnsubscribeOk =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(back, ok);
    }

    #[test]
    fn watch_frames_round_trip() {
        let req = RequestEnvelope {
            id: 3,
            request: Request::Watch,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"watch\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.request, Request::Watch);

        let resp = ResponseEnvelope {
            id: 3,
            body: ResponseBody::Item(SubscriptionItem::Sessions {
                sessions: vec![SessionInfo {
                    session_id: "s1".into(),
                    title: Some("ship the rail".into()),
                    archived: false,
                    provider: "claude-code".into(),
                    model: None,
                    cwd: "/tmp".into(),
                    created_ts_ms: 1,
                    last_activity_ms: Some(1_700),
                    state: "running".into(),
                    live: true,
                    epoch: 9,
                    last_seq: 41,
                    checkout_root: Some("/tmp".into()),
                    shared_with: vec!["s2".into()],
                    worktree: Some(WorktreeInfo {
                        base: "/repo".into(),
                        branch: "eave/s1prefix".into(),
                        present: true,
                        dirty_files: Some(3),
                    }),
                    pending_approvals: 2,
                    permission_mode: Some("default".into()),
                    rss_bytes: Some(268_435_456),
                }],
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        assert!(line.contains("\"sessions\""), "{line}");
        let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
    }

    /// Stage AC: one method, one bit, both directions — and a frame that
    /// an engine predating the field parses unchanged.
    #[test]
    fn archive_round_trips_in_both_directions() {
        for archived in [true, false] {
            let req = RequestEnvelope {
                id: 12,
                request: Request::Archive {
                    session_id: "s1".into(),
                    archived,
                },
            };
            let line = serde_json::to_string(&req).unwrap();
            assert!(line.contains("\"method\":\"archive\""), "{line}");
            assert!(
                line.contains(&format!("\"archived\":{archived}")),
                "the bit has to be ON the frame in both directions: {line}"
            );
            let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, req);
        }
    }

    /// A `SessionInfo` from an engine that has never heard of the archive
    /// bit must read back VISIBLE. The other default hides every session
    /// an older engine reports, which is the worst possible failure for a
    /// field whose whole meaning is "do not draw this".
    #[test]
    fn a_session_from_an_older_engine_is_never_archived() {
        let line = r#"{"session_id":"s1","provider":"claude-code","cwd":"/tmp",
            "created_ts_ms":1,"state":"idle","live":false,"epoch":1,"last_seq":0}"#;
        let info: SessionInfo = serde_json::from_str(line).unwrap();
        assert!(!info.archived);
        // And an unarchived session puts no key on the wire at all, so a
        // client predating the field sees the bytes it always saw.
        let back = serde_json::to_string(&info).unwrap();
        assert!(!back.contains("archived"), "{back}");
    }

    #[test]
    fn files_frames_round_trip() {
        let req = RequestEnvelope {
            id: 5,
            request: Request::Files {
                session_id: "s1".into(),
                scope: FileScope::WorkingTree,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"files\""), "{line}");
        // The default basis is not written: an engine predating Slice AA
        // must see the exact frame it always saw.
        assert!(!line.contains("scope"), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        let turn = RequestEnvelope {
            id: 5,
            request: Request::Files {
                session_id: "s1".into(),
                scope: FileScope::LatestTurn,
            },
        };
        let line = serde_json::to_string(&turn).unwrap();
        assert!(line.contains("\"scope\":\"latest_turn\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, turn);

        for state in [
            GitState::Ok,
            GitState::NotARepo,
            GitState::Error {
                message: "git status timed out".into(),
            },
        ] {
            let resp = ResponseEnvelope {
                id: 5,
                body: ResponseBody::Item(SubscriptionItem::Files {
                    session_id: "s1".into(),
                    snapshot: FilesSnapshot {
                        state,
                        files: vec![ChangedFile {
                            path: "src/lib.rs".into(),
                            status: FileStatus::Modified,
                            adds: Some(12),
                            dels: None,
                            orig_path: None,
                            untracked: false,
                        }],
                        omitted: 0,
                        watching: true,
                        basis: DiffBasis::WorkingTree,
                    },
                }),
            };
            let line = serde_json::to_string(&resp).unwrap();
            assert!(line.contains("\"snapshot\""), "{line}");
            let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, resp);
        }
    }

    /// Slice AA: every basis a snapshot can state survives the wire, and
    /// the two empty-list cases stay distinguishable after a round trip —
    /// which is the only reason the field exists.
    #[test]
    fn a_files_snapshot_states_its_basis() {
        for basis in [
            DiffBasis::WorkingTree,
            DiffBasis::Turn { at: 1_755_000_000 },
            DiffBasis::Unavailable {
                reason: "no turn has started since this engine did".into(),
            },
        ] {
            let snap = FilesSnapshot {
                state: GitState::Ok,
                files: Vec::new(),
                omitted: 0,
                watching: true,
                basis: basis.clone(),
            };
            let line = serde_json::to_string(&snap).unwrap();
            // The default stays off the wire; anything else is stated.
            assert_eq!(
                line.contains("basis"),
                !matches!(basis, DiffBasis::WorkingTree),
                "{line}"
            );
            let back: FilesSnapshot = serde_json::from_str(&line).unwrap();
            assert_eq!(back, snap);
        }
    }

    #[test]
    fn diff_frames_round_trip() {
        let req = RequestEnvelope {
            id: 6,
            request: Request::Diff {
                session_id: "s1".into(),
                path: "src/lib.rs".into(),
                scope: FileScope::WorkingTree,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"diff\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        for kind in [
            DiffKind::Text {
                hunks: vec![DiffHunk {
                    header: "@@ -1,2 +1,3 @@".into(),
                    lines: vec![
                        DiffLine {
                            op: DiffOp::Ctx,
                            text: "one".into(),
                        },
                        DiffLine {
                            op: DiffOp::Del,
                            text: "two".into(),
                        },
                        DiffLine {
                            op: DiffOp::Add,
                            text: "TWO".into(),
                        },
                    ],
                }],
                truncated: true,
            },
            DiffKind::Binary,
            DiffKind::Submodule {
                detail: "abc1234 → def5678 (dirty)".into(),
            },
            DiffKind::Symlink {
                target: "../elsewhere".into(),
            },
            DiffKind::Error {
                message: "git diff timed out".into(),
            },
        ] {
            let ok = DiffOk {
                diff: FileDiff {
                    path: "src/lib.rs".into(),
                    kind,
                },
            };
            let v = serde_json::to_value(&ok).unwrap();
            let back: DiffOk = serde_json::from_value(v).unwrap();
            assert_eq!(back, ok);
        }
    }

    /// Wire-compat: a B1 engine's snapshot (no `omitted`, no `orig_path`,
    /// no `untracked`) still parses — the new fields default.
    #[test]
    fn b1_files_frames_still_parse() {
        let line = r#"{"state":{"kind":"ok"},"files":[{"path":"a.txt","status":"modified","adds":1,"dels":2}]}"#;
        let snap: FilesSnapshot = serde_json::from_str(line).unwrap();
        assert_eq!(snap.omitted, 0);
        assert_eq!(snap.files[0].orig_path, None);
        assert!(!snap.files[0].untracked);
        // Absent `watching` = the polling fallback — exactly what a B1
        // engine's list is.
        assert!(!snap.watching);
        // Absent `basis` = the working tree, which is the only thing such
        // an engine could ever have measured.
        assert_eq!(snap.basis, DiffBasis::WorkingTree);
    }

    /// Stage AB. Two shapes, one test: naming a session and un-naming it
    /// are the same method, which is what stops "clear the title" from
    /// becoming a second verb that can disagree with the first.
    #[test]
    fn rename_round_trips_in_both_directions() {
        for title in [Some("ship the rail".to_string()), None] {
            let req = RequestEnvelope {
                id: 21,
                request: Request::Rename {
                    session_id: "s1".into(),
                    title: title.clone(),
                },
            };
            let line = serde_json::to_string(&req).unwrap();
            assert!(line.contains("\"method\":\"rename\""), "{line}");
            let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, req);
        }
    }

    /// Wire-compat: a pre-Stage-AB engine's `SessionInfo` still parses —
    /// unnamed, and with no activity stamp. Both degrade to the honest
    /// reading (nobody named it; sort it by creation), which is exactly
    /// what such an engine means.
    #[test]
    fn pre_stage_ab_session_info_still_parses() {
        let line = r#"{"session_id":"s1","provider":"claude-code","cwd":"/tmp",
            "created_ts_ms":7,"state":"new","live":false,"epoch":1,"last_seq":0}"#;
        let info: SessionInfo = serde_json::from_str(line).unwrap();
        assert_eq!(info.title, None);
        assert_eq!(info.last_activity_ms, None);
        // And the absence is absence on the way out too: an unnamed
        // session must not put an empty string on the wire for a rail to
        // render as a blank line.
        let back = serde_json::to_string(&info).unwrap();
        assert!(!back.contains("title"), "{back}");
        assert!(!back.contains("last_activity_ms"), "{back}");
    }

    #[test]
    fn worktree_methods_round_trip() {
        let req = RequestEnvelope {
            id: 12,
            request: Request::Isolate {
                session_id: "s1".into(),
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"isolate\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        let req = RequestEnvelope {
            id: 13,
            request: Request::RemoveWorktree {
                session_id: "s1".into(),
                force: false,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"remove_worktree\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        // A refusal restates its facts; a clean removal omits them.
        for ok in [
            RemoveWorktreeOk {
                removed: false,
                dirty_files: Some(3),
                unmerged_commits: Some(1),
            },
            RemoveWorktreeOk {
                removed: true,
                dirty_files: None,
                unmerged_commits: None,
            },
        ] {
            let back: RemoveWorktreeOk =
                serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
            assert_eq!(back, ok);
        }
    }

    /// Wire-compat: a pre-I2 engine's SessionInfo (no checkout/worktree
    /// facts) still parses — root unknown, sharing unstated, no worktree.
    /// That degradation is accurate: such an engine detects nothing.
    #[test]
    fn pre_i2_session_info_still_parses() {
        let line = r#"{"session_id":"s1","provider":"claude-code","cwd":"/tmp","created_ts_ms":1,"state":"new","live":false,"epoch":9,"last_seq":0}"#;
        let info: SessionInfo = serde_json::from_str(line).unwrap();
        assert_eq!(info.checkout_root, None);
        assert!(info.shared_with.is_empty());
        assert_eq!(info.worktree, None);
    }

    // -----------------------------------------------------------------
    // Slice B1 — the compatibility contract for browser state.
    //
    // The rule these keep is the one Slice P2 established for agent
    // config, applied to a much bigger surface: **an engine or client that
    // predates a field must still parse the frame**, and the ordinary case
    // must put no key on the wire at all. Every assertion below is one
    // half of that, written against literal JSON rather than a round trip,
    // because a round trip only proves this build agrees with itself.

    #[test]
    fn browser_methods_round_trip() {
        for request in [
            Request::WatchTabs,
            Request::WatchBookmarks,
            Request::TabNav {
                tab_id: "t1".into(),
            },
            Request::HistoryQuery {
                typed: "gith".into(),
                limit: Some(8),
            },
            Request::TabOpen {
                window: None,
                url: Some("https://x/".into()),
                group: Some("g1".into()),
                index: Some(2),
                content: TabContent::Session {
                    session_id: "s1".into(),
                },
                opener: Some("t0".into()),
                activate: true,
                incognito: false,
            },
            Request::TabGo {
                tab_id: "t1".into(),
                delta: -1,
            },
            Request::TabStated {
                tab_id: "t1".into(),
                fact: PageFact::Committed {
                    url: "https://x/".into(),
                    title: Some("X".into()),
                },
            },
            Request::TabStated {
                tab_id: "t1".into(),
                fact: PageFact::Load {
                    load: LoadState::Failed {
                        reason: "net::ERR_NAME_NOT_RESOLVED".into(),
                    },
                    can_go_back: None,
                    can_go_forward: None,
                },
            },
            Request::GroupUpdate {
                group_id: "g1".into(),
                title: Some(Setting::Value {
                    value: "work".into(),
                }),
                color: None,
                workspace: Some(Setting::Clear),
                collapsed: Some(true),
            },
            Request::BookmarkAdd {
                parent: None,
                index: None,
                title: Some("f".into()),
                url: None,
            },
            Request::BookmarkRetarget {
                bookmark_id: "b1".into(),
                url: "https://y/".into(),
            },
            Request::BrowserImport {
                bookmarks: vec![ImportedBookmark {
                    title: Some("From Arc".into()),
                    url: None,
                    children: vec![ImportedBookmark {
                        title: Some("Rust".into()),
                        url: Some("https://www.rust-lang.org/".into()),
                        children: Vec::new(),
                    }],
                }],
                history: vec![ImportedVisit {
                    url: "https://hist.example/".into(),
                    title: Some("Hist".into()),
                    visit_count: 4,
                    last_visit_ms: 1,
                }],
            },
            Request::HistoryClear {
                since_ms: Some(1),
                before_ms: None,
            },
            Request::WatchPermissions,
            Request::PermissionSet {
                origin: "https://x.test".into(),
                permissions: vec![PermissionKind::Camera, PermissionKind::Microphone],
                decision: PermissionDecision::Allow,
            },
            Request::PermissionForget {
                origin: "https://x.test".into(),
                permission: Some(PermissionKind::Notifications),
            },
            Request::PermissionForget {
                origin: "https://x.test".into(),
                permission: None,
            },
            Request::PermissionClear,
        ] {
            let env = RequestEnvelope { id: 3, request };
            let line = serde_json::to_string(&env).unwrap();
            let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, env, "{line}");
        }
    }

    /// The plain cases write no keys. A tab opened at a url, with no group,
    /// no opener and no index, must produce a frame an engine predating
    /// every one of those fields would accept unchanged.
    #[test]
    fn an_ordinary_browser_frame_carries_no_optional_keys() {
        let line = serde_json::to_string(&Request::TabOpen {
            window: None,
            url: Some("https://x/".into()),
            group: None,
            index: None,
            content: TabContent::Page,
            opener: None,
            activate: false,
            incognito: false,
        })
        .unwrap();
        for key in ["group", "index", "content", "opener", "activate", "incognito"] {
            assert!(!line.contains(key), "a plain tab_open wrote {key}: {line}");
        }

        let snap = TabsSnapshot::default();
        let line = serde_json::to_string(&snap).unwrap();
        assert!(!line.contains("active"), "{line}");
        assert!(!line.contains("read_only"), "{line}");
    }

    /// An older ENGINE's frames must parse here: every field this build
    /// added since is absent, and each has to land on the value those
    /// engines actually produced.
    #[test]
    fn an_older_engines_browser_frames_still_parse() {
        // A tabs snapshot with only the fields that must always exist.
        let raw = r#"{"tabs":[{"id":"t1","created_ts_ms":5}],
                      "groups":[],"order":[],"group_tabs":[]}"#;
        let snap: TabsSnapshot = serde_json::from_str(raw).unwrap();
        let t = &snap.tabs[0];
        assert_eq!(t.url, None, "an absent url must not become an empty string");
        assert_eq!(
            t.load,
            LoadState::Unloaded,
            "an absent load state must read as unloaded — any other default \
             makes an old engine's tabs claim to be busy"
        );
        assert!(!t.pinned);
        assert!(!t.can_go_back && !t.can_go_forward);
        assert_eq!(t.history_len, 0);

        // An older page engine's load fact has no history flags. Absent
        // is `None`, not `Some(false)` — collapsing those is how the
        // arrows got hardcoded.
        let fact: PageFact = serde_json::from_str(r#"{"fact":"load","state":"complete"}"#).unwrap();
        assert_eq!(
            fact,
            PageFact::Load {
                load: LoadState::Complete,
                can_go_back: None,
                can_go_forward: None,
            }
        );
        let fact = PageFact::Load {
            load: LoadState::Complete,
            can_go_back: Some(true),
            can_go_forward: Some(false),
        };
        let line = serde_json::to_string(&fact).unwrap();
        assert!(
            line.contains("can_go_back") && line.contains("can_go_forward"),
            "a load fact that stated history dropped the keys: {line}"
        );
        assert_eq!(t.content, TabContent::Page, "an absent content is a page");
        assert!(!snap.read_only, "an absent read_only must not lock the UI");

        // A bookmark node from before `children` was counted.
        let raw = r#"{"id":"b1","kind":"folder","created_ts_ms":1,"depth":0}"#;
        let node: BookmarkNode = serde_json::from_str(raw).unwrap();
        assert_eq!(node.children, 0);
        assert_eq!(node.bookmark.title, None);
        assert!(node.bookmark.is_folder());

        // A history reply from before it timed itself.
        let raw = r#"{"matches":[]}"#;
        let ok: HistoryQueryOk = serde_json::from_str(raw).unwrap();
        assert_eq!(ok.total_entries, 0);
        assert_eq!(ok.took_us, 0);
    }

    /// An older CLIENT's frames must parse at the engine: a `tab_open` with
    /// nothing but a url, a `group_update` naming one field, a `tab_move`
    /// with no group.
    #[test]
    fn an_older_clients_browser_frames_still_parse() {
        let raw = r#"{"id":1,"method":"tab_open","params":{"url":"https://x/"}}"#;
        let env: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(
            env.request,
            Request::TabOpen {
                window: None,
                url: Some("https://x/".into()),
                group: None,
                index: None,
                content: TabContent::Page,
                opener: None,
                activate: false,
                incognito: false,
            }
        );

        let raw = r#"{"id":2,"method":"group_update","params":{"group_id":"g","collapsed":true}}"#;
        let env: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(
            env.request,
            Request::GroupUpdate {
                group_id: "g".into(),
                title: None,
                color: None,
                workspace: None,
                collapsed: Some(true),
            },
            "an absent field must mean LEAVE ALONE, never clear"
        );

        let raw = r#"{"id":3,"method":"tab_move","params":{"tab_id":"t"}}"#;
        let env: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(
            env.request,
            Request::TabMove {
                tab_id: "t".into(),
                group: None,
                index: 0,
            }
        );

        let raw = r#"{"id":4,"method":"history_query","params":{"typed":"g"}}"#;
        let env: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(
            env.request,
            Request::HistoryQuery {
                typed: "g".into(),
                limit: None,
            },
            "an absent limit must mean the engine's default, not zero results"
        );
    }

    /// `Setting` is three states, and the wire has to show all three
    /// distinctly. Two would collapse "leave it alone" into "clear it",
    /// which is how a rename box wipes a colour nobody touched.
    #[test]
    fn a_setting_distinguishes_absent_from_clear_from_value() {
        let clear = serde_json::to_string(&Setting::<String>::Clear).unwrap();
        assert_eq!(clear, r#"{"set":"clear"}"#);
        let value = serde_json::to_string(&Setting::Value {
            value: "blue".to_string(),
        })
        .unwrap();
        assert!(value.contains(r#""set":"value""#), "{value}");
        assert_ne!(clear, value);
        let back: Setting<String> = serde_json::from_str(&clear).unwrap();
        assert_eq!(back, Setting::Clear);
    }

    /// The browser subscription frames are tagged like every other one, so
    /// a client demultiplexing on `type` keeps working.
    #[test]
    fn browser_subscription_frames_round_trip() {
        for item in [
            SubscriptionItem::Tabs {
                snapshot: TabsSnapshot::default(),
            },
            SubscriptionItem::Permissions {
                snapshot: PermissionsSnapshot {
                    grants: vec![Grant {
                        origin: "https://x.test".into(),
                        permission: PermissionKind::Camera,
                        decision: PermissionDecision::Allow,
                        ts_ms: 7,
                    }],
                    read_only: false,
                },
            },
            SubscriptionItem::Bookmarks {
                snapshot: BookmarksSnapshot::default(),
            },
        ] {
            let env = ResponseEnvelope {
                id: 4,
                body: ResponseBody::Item(item.clone()),
            };
            let line = serde_json::to_string(&env).unwrap();
            let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, env, "{line}");
        }
        let line = serde_json::to_string(&SubscriptionItem::Tabs {
            snapshot: TabsSnapshot::default(),
        })
        .unwrap();
        assert!(line.contains(r#""type":"tabs""#), "{line}");
    }

    /// `changed: false` is a fact on the OK path, not an error. A caller
    /// that treated it as a failure would show a dialog for a click that
    /// did exactly what was asked.
    #[test]
    fn a_no_op_browser_reply_is_a_fact_not_a_failure() {
        let body = ResponseBody::Ok(serde_json::to_value(ChangedOk { changed: false }).unwrap());
        let line = serde_json::to_string(&body).unwrap();
        assert_eq!(line, r#"{"ok":{"changed":false}}"#);
        let back: ResponseBody = serde_json::from_str(&line).unwrap();
        match back {
            ResponseBody::Ok(v) => {
                let ok: ChangedOk = serde_json::from_value(v).unwrap();
                assert!(!ok.changed);
            }
            other => panic!("a no-op landed on the error path: {other:?}"),
        }
    }

    #[test]
    fn unit_method_has_no_params() {
        let req = RequestEnvelope {
            id: 1,
            request: Request::List,
        };
        let line = serde_json::to_string(&req).unwrap();
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.request, Request::List);
    }

    // -----------------------------------------------------------------
    // Slice P2 — the compatibility contract for per-session agent config

    /// An `open` frame from a client that predates per-session config must
    /// still parse. This is the whole reason the field is
    /// `#[serde(default)]` rather than a proto bump: a running app and a
    /// newer engine (or the reverse) meet across this frame.
    #[test]
    fn an_open_frame_without_agent_config_still_parses() {
        // `#[serde(tag = "method", content = "params")]` — the shape a
        // real pre-P2 client puts on the socket.
        let old = r#"{"method":"open","params":{"session_id":"s1","cwd":null}}"#;
        let req: Request = serde_json::from_str(old).expect("old open frame must parse");
        match req {
            Request::Open {
                session_id,
                cwd,
                agent,
            } => {
                assert_eq!(session_id.as_deref(), Some("s1"));
                assert_eq!(cwd, None);
                assert_eq!(agent, None, "absent config must mean the engine's default");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    /// And the reverse direction: a frame carrying no config must SERIALIZE
    /// exactly as it did before, so an older engine sees a byte-identical
    /// request rather than an unknown key.
    #[test]
    fn an_open_without_config_serializes_with_no_agent_key() {
        let req = Request::Open {
            session_id: Some("s1".into()),
            cwd: None,
            agent: None,
        };
        let raw = serde_json::to_string(&req).expect("serialize");
        assert!(
            !raw.contains("agent"),
            "an unconfigured open must not mention agent: {raw}"
        );
    }

    /// A `SessionInfo` from an engine predating the field parses, and reads
    /// as "no model configured" — never as a guessed default.
    #[test]
    fn a_session_info_without_a_model_parses_as_unconfigured() {
        let old = r#"{
            "session_id":"s1","provider":"claude-code","cwd":"/tmp",
            "created_ts_ms":1,"state":"new","live":false,"epoch":7,"last_seq":0
        }"#;
        let info: SessionInfo = serde_json::from_str(old).expect("old SessionInfo must parse");
        assert_eq!(info.model, None);
        assert_eq!(info.provider, "claude-code");
    }

    /// The config a session states is carried whole across the wire — the
    /// fields exist so a picker can read back exactly what it set.
    #[test]
    fn an_agent_config_round_trips_across_the_wire() {
        let req = Request::Open {
            session_id: Some("s1".into()),
            cwd: None,
            agent: Some(AgentConfig {
                provider: Some("claude-code".into()),
                program: None,
                model: Some("opus-4-2".into()),
                extra_args: vec!["--foo".into()],
            }),
        };
        let raw = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&raw).expect("round trip");
        assert_eq!(back, req);
    }

    // --- Slice AJ — a timeout is not a failure -----------------------------

    /// The exact sentence a packaged eave put in front of a person on
    /// 2026-08-20, on a machine where `git status` took 36 ms.
    ///
    /// Pinned as a literal rather than assembled from the parts, because
    /// the whole defect was two layers each assembling a frame: a test that
    /// built the expectation the same way the code does would have passed
    /// throughout.
    #[test]
    fn a_timeout_never_says_git_failed() {
        let unanswered = GitState::Unanswered {
            what: "status".into(),
            secs: 10.0,
            gate: Some("~/Desktop".into()),
        };
        let said = unanswered.stated();
        assert_ne!(said, "git failed: git status timed out after 10s");
        assert!(
            !said.contains("git failed"),
            "git did not fail — it did not answer: {said:?}"
        );
        assert_eq!(
            said,
            "git status did not answer in 10s — ~/Desktop is a folder macOS gates"
        );
    }

    /// Only git actually failing says git failed.
    #[test]
    fn a_real_git_error_still_says_git_failed() {
        let err = GitState::Error {
            message: "bad revision 'nope'".into(),
        };
        assert_eq!(err.stated(), "git failed: bad revision 'nope'");
        assert_eq!(err.remedy(), None, "there is nothing a person can do here");
    }

    /// The remedy is offered exactly when there is one, and it does not
    /// assert a cause it cannot know: a git call can be slow in a gated
    /// folder for ordinary reasons.
    #[test]
    fn the_remedy_is_conditional_and_only_where_it_applies() {
        let gated = GitState::Unanswered {
            what: "status".into(),
            secs: 10.0,
            gate: Some("~/Desktop".into()),
        };
        let plain = GitState::Unanswered {
            what: "status".into(),
            secs: 10.0,
            gate: None,
        };
        let r = gated.remedy().expect("a gated folder has a remedy");
        assert!(
            r.starts_with("If eave has not been granted access"),
            "{r:?}"
        );
        assert!(r.contains("System Settings"), "{r:?}");
        assert_eq!(plain.remedy(), None);
        assert_eq!(
            plain.stated(),
            "git status did not answer in 10s",
            "no gate, no speculation about one"
        );
    }

    /// The new variant survives the wire. An engine that reports
    /// `Unanswered` to an app that cannot parse it is worse than the bug
    /// this replaces.
    #[test]
    fn unanswered_round_trips() {
        for gate in [None, Some("iCloud Drive".to_string())] {
            let st = GitState::Unanswered {
                what: "diff".into(),
                secs: 2.5,
                gate,
            };
            let raw = serde_json::to_string(&st).expect("serialize");
            let back: GitState = serde_json::from_str(&raw).expect("round trip");
            assert_eq!(back, st);
        }
    }
}
