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

pub use eave_core::attention::{AttentionSnapshot, WaitReason};

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
    Hello { proto: u32 },
    /// All sessions the engine knows (on disk or live).
    List,
    /// Create a session (fresh id if `None`) or load an existing one.
    /// Creating writes the journal header immediately; nothing is spawned
    /// until the first `Send`.
    Open {
        session_id: Option<String>,
        cwd: Option<String>,
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
    Send { session_id: String, text: String },
    /// Kill the session's live process (verified kill ladder, spec 01).
    Kill { session_id: String },
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
    Files { session_id: String },
    /// One file's diff content (Slice B2), computed by the engine from
    /// `git diff` in the session cwd at request time — unary, not a
    /// subscription. There is deliberately no engine-held diff state to
    /// go stale: invalidation rides the `files` channel (a fresh
    /// [`FilesSnapshot`] means "re-request any diff you are showing").
    /// Every failure shape is a stated fact inside [`FileDiff`], never a
    /// hang — and every underlying git call is timeout-bounded.
    Diff { session_id: String, path: String },
    /// Cancel a streaming request (`subscribe` / `watch` / `files`) by its
    /// envelope id, on this connection (Slice I1). The engine stops the
    /// stream's task and releases any subscriber-scoped resource it held
    /// (a `files` stream's FS-watcher refcount). Idempotent: an unknown or
    /// already-finished id still replies ok — the caller may race a
    /// disconnect or a stream the engine ended on its own. Without this, a
    /// client that switches sessions leaks the old stream for the
    /// connection's whole life; connection close remains the backstop.
    Unsubscribe { id: u64 },
    /// Move a session into its own git worktree (Slice I2). Allowed only
    /// while nothing has ever spawned (state `new`, no records) — after
    /// the first send the provider has seen the cwd and it is fixed for
    /// the session's life (`--resume` finds its history by cwd). The
    /// engine creates `<worktrees_root>/<repo>/<sid8>` on a fresh
    /// `eave/<sid8>` branch off the base repo's HEAD and rewrites the
    /// session's cwd to it. Isolation is the operator's choice, never
    /// automatic (Slice I2 decision 1): detection of a shared checkout is
    /// stated on `watch`; this method is the opt-in.
    Isolate { session_id: String },
    /// Delete a session's eave-created worktree (Slice I2). The engine
    /// re-checks state freshly at call time: a LIVE session refuses (kill
    /// first); uncommitted files or commits unreachable from any other
    /// ref refuse unless `force` — the reply restates the counts so the
    /// operator confirms against facts, not a stale rail row (decision 2:
    /// eave never deletes dirty work without the operator meaning it).
    /// Clean removal also prunes and deletes the `eave/` branch.
    RemoveWorktree { session_id: String, force: bool },
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
    Err { message: String },
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloOk {
    pub proto: u32,
    pub engine: ProcIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub provider: String,
    pub cwd: String,
    pub created_ts_ms: u64,
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
}

fn is_zero(n: &u64) -> bool {
    *n == 0
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
    /// Git could not answer (timeout, missing binary, vanished cwd, …).
    /// The message is shown verbatim; the previous snapshot is gone.
    Error { message: String },
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
                    provider: "claude-code".into(),
                    cwd: "/tmp".into(),
                    created_ts_ms: 1,
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
                }],
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        assert!(line.contains("\"sessions\""), "{line}");
        let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn files_frames_round_trip() {
        let req = RequestEnvelope {
            id: 5,
            request: Request::Files {
                session_id: "s1".into(),
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"method\":\"files\""), "{line}");
        let back: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

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
                    },
                }),
            };
            let line = serde_json::to_string(&resp).unwrap();
            assert!(line.contains("\"snapshot\""), "{line}");
            let back: ResponseEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(back, resp);
        }
    }

    #[test]
    fn diff_frames_round_trip() {
        let req = RequestEnvelope {
            id: 6,
            request: Request::Diff {
                session_id: "s1".into(),
                path: "src/lib.rs".into(),
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
                        DiffLine { op: DiffOp::Ctx, text: "one".into() },
                        DiffLine { op: DiffOp::Del, text: "two".into() },
                        DiffLine { op: DiffOp::Add, text: "TWO".into() },
                    ],
                }],
                truncated: true,
            },
            DiffKind::Binary,
            DiffKind::Submodule { detail: "abc1234 → def5678 (dirty)".into() },
            DiffKind::Symlink { target: "../elsewhere".into() },
            DiffKind::Error { message: "git diff timed out".into() },
        ] {
            let ok = DiffOk {
                diff: FileDiff { path: "src/lib.rs".into(), kind },
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
}
