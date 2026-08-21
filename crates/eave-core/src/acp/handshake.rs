//! The gate between "the child is up" and "there is a session to talk to".
//!
//! ACP has no session id until the agent answers `session/new`, and a
//! `session/prompt` without one is a protocol error — so every caller that
//! sends a prompt has to WAIT for the handshake rather than race it. Both
//! eave ACP adapters had their own copy of that wait, with their own copy
//! of the two rules that make it safe:
//!
//! 1. **Unbounded.** A handshake that never completes is a hung agent, and
//!    eave does not time out into a decision — the attention clocks
//!    classify it and the operator decides (spec 01).
//! 2. **Settled at EOF.** The watch sender outlives the reader, so a dead
//!    child would never close the channel on its own and the wait would
//!    hang for the lifetime of the process. The reader settles the gate as
//!    a stated failure on its way out instead.
//!
//! Rule 2 is eave's answer to the third of the ordering hazards comet
//! records at `comet/docs/research/acp.md:108` — "EOF right after a final
//! response reads as a clean finish … not a crash". comet needs a 50ms
//! turn-future grace there because its responses arrive on a different
//! path from its notifications; eave reads one ordered stream, so the
//! reader simply knows it is finished and says so.

use tokio::sync::watch;

/// Where a session is between `initialize` and a usable session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// Still shaking hands.
    Pending,
    /// Open, and every posture proof this agent requires has passed. The
    /// id it carries is the PROVIDER's own — the resume cursor.
    Ready(String),
    /// There will never be a session id, and this is why.
    Refused(String),
}

/// Why a wait for the session id ended without one.
#[derive(Debug, Clone)]
pub enum HandshakeError {
    /// The handshake settled as a failure, with the reason it stated.
    /// Report THAT, not a generic broken pipe: "grok could not resume
    /// session X" is the sentence the operator needs.
    Refused(String),
    /// The session itself is gone.
    Gone,
}

/// The publisher side and the waiter side of one session's handshake.
pub struct HandshakeGate {
    tx: watch::Sender<Handshake>,
    rx: watch::Receiver<Handshake>,
}

impl Default for HandshakeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl HandshakeGate {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(Handshake::Pending);
        Self { tx, rx }
    }

    pub fn is_pending(&self) -> bool {
        self.state() == Handshake::Pending
    }

    /// The settled state right now, without waiting.
    pub fn state(&self) -> Handshake {
        self.rx.borrow().clone()
    }

    /// The session is open and proven. Idempotent by construction: a
    /// second `session/new` reply cannot demote a refusal.
    pub fn ready(&self, session_id: String) {
        let _ = self.tx.send(Handshake::Ready(session_id));
    }

    /// There will be no session, and this is the sentence that says why.
    pub fn refuse(&self, reason: String) {
        let _ = self.tx.send(Handshake::Refused(reason));
    }

    /// The reader's last act: a handshake still pending when the stream
    /// ends can never complete, so state that instead of leaving every
    /// future caller blocked forever.
    pub fn settle_if_pending(&self, reason: &str) {
        if self.is_pending() {
            self.refuse(reason.to_string());
        }
    }

    /// Wait for the session id. See the module docs for why this has no
    /// deadline.
    pub async fn session_id(&self) -> Result<String, HandshakeError> {
        let mut rx = self.rx.clone();
        loop {
            match rx.borrow_and_update().clone() {
                Handshake::Ready(id) => return Ok(id),
                Handshake::Refused(reason) => return Err(HandshakeError::Refused(reason)),
                Handshake::Pending => {}
            }
            if rx.changed().await.is_err() {
                return Err(HandshakeError::Gone);
            }
        }
    }
}
