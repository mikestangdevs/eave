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
    /// A batch of records, contiguous by seq. Batching is opportunistic
    /// coalescing — a batch boundary carries no meaning.
    Events {
        session_id: String,
        epoch: u64,
        records: Vec<Record>,
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
