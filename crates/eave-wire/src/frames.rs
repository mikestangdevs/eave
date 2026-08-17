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
                        }],
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
