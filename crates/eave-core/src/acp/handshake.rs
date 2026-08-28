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

// ---------------------------------------------------------------------------
// What the agent said it could do

/// The agent's own answer to "which session methods do you implement?",
/// read from the `initialize` reply and written to the journal.
///
/// # Why this exists
///
/// Until slice CL, **both ACP adapters threw the entire `initialize`
/// result away.** They checked it for an `error` and otherwise emitted
/// nothing — the single richest frame of the session, discarded without a
/// record. That is the same defect slice CI had just fixed one commit
/// earlier on the notification channel (`handle_notification` ended
/// `_ => Vec::new()`, silently dropping 202 of grok's 204 vendor
/// methods), and [`crate::events::SessionEvent::Unknown`] already states
/// the house rule it violated: surfaced, never fatal.
///
/// The cost was concrete. Slice CL had to answer "does the wire actually
/// support `session/list`, `fork`, `resume`, `close`, per agent?" and
/// eave's own journal could not tell it, so the question was settled by
/// four live probes against the installed binaries. Every future slice
/// asking the same question would have had to repeat them.
///
/// # Capabilities are load-bearing, not decorative — verified 2026-08-21
///
/// The advertisement is TRUTHFUL on both agents, and the two agents do
/// not agree:
///
/// | method | grok 1.0.5 | Hermes 0.20.4 |
/// | --- | --- | --- |
/// | `session/load` (`loadSession`) | yes | yes |
/// | `session/list` | yes | yes |
/// | `session/resume` | yes | yes |
/// | `session/close` | **yes** | **no** |
/// | `session/fork` | **no** | **yes** |
///
/// Asking grok to `session/fork` answers `-32601 Method not found`, so an
/// unadvertised method is not a feature that can be shipped — it is a
/// broken call on half of eave's ACP agents. `close`, `fork` and `resume`
/// are additionally marked **UNSTABLE** in the ACP schema itself ("not
/// part of the spec yet, and may be removed or changed at any point" —
/// `acp/schema.py` `SessionCapabilities`, the package Hermes vendors).
///
/// # What is recorded, and what is deliberately not
///
/// A small projection eave builds, **never the agent's bytes**. Grok's
/// `initialize` result is ~4 KB of model catalogs, MCP configuration and
/// `currentWorkingDirectory`, and the frames around it carry operator
/// home paths and — observed in this very capture — a bearer token out of
/// the operator's own MCP configuration. Slice CI had to filter exactly
/// that out of its fixture. Recording the result verbatim would put it in
/// the journal permanently, so this records the capability FACTS and
/// nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Advertisement {
    /// The ACP version the agent answered with, if it stated one.
    pub protocol_version: Option<i64>,
    /// `name/version` from `agentInfo`, bounded like any other
    /// wire-sourced string eave copies into its record.
    pub agent: Option<String>,
    /// `agentCapabilities.loadSession`. Absent reads as `false`: eave does
    /// not guess a capability it was not told about.
    pub load_session: bool,
    /// The keys of `agentCapabilities.sessionCapabilities`, sorted.
    ///
    /// Sorted because JSON object order is not guaranteed and this string
    /// goes into a permanent record that tests and operators compare.
    pub session_caps: Vec<String>,
}

/// Read the advertisement out of an `initialize` result.
///
/// Tolerant by construction, like every other reader here: a missing or
/// malformed field is recorded as absent rather than failing the
/// handshake. eave declines to ACT on these capabilities today (slice CL
/// shipped no lifecycle method), so a misread must never be able to cost
/// a session.
pub fn advertisement(result: Option<&serde_json::Value>) -> Advertisement {
    let Some(result) = result else {
        return Advertisement::default();
    };
    let caps = result.get("agentCapabilities");
    let agent = result.get("agentInfo").and_then(|i| {
        let name = i.get("name").and_then(serde_json::Value::as_str)?;
        let label = match i.get("version").and_then(serde_json::Value::as_str) {
            Some(v) => format!("{name}/{v}"),
            None => name.to_string(),
        };
        // `chars`, not bytes: truncating mid-codepoint would replace a
        // diagnosable label with a panic.
        Some(
            label
                .chars()
                .take(crate::acp::wire::MALFORMED_LINE_CAP)
                .collect::<String>(),
        )
    });
    let mut session_caps: Vec<String> = caps
        .and_then(|c| c.get("sessionCapabilities"))
        .and_then(serde_json::Value::as_object)
        .map(|o| {
            o.keys()
                // `_meta` is ACP's extensibility escape hatch, not a
                // capability. Recording it as one would claim the agent
                // supports a `session/_meta`.
                .filter(|k| *k != "_meta")
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    session_caps.sort();
    Advertisement {
        protocol_version: result
            .get("protocolVersion")
            .and_then(serde_json::Value::as_i64),
        agent,
        load_session: caps
            .and_then(|c| c.get("loadSession"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        session_caps,
    }
}

impl Advertisement {
    /// The journal record of this handshake.
    ///
    /// A [`crate::events::SessionEvent::System`] rather than a new variant,
    /// deliberately: eave replays its journal in full on cold start
    /// (spec 02), and `System` already exists precisely so "future CLI
    /// additions are recorded without a parser change". Adding a variant
    /// would put a record in the journal that every older reader chokes
    /// on; this one they already understand. `acp_replay_absorbed`, from
    /// this same module, set the precedent.
    pub fn record(&self) -> crate::events::SessionEvent {
        let mut raw = serde_json::Map::new();
        if let Some(v) = self.protocol_version {
            raw.insert("protocolVersion".into(), v.into());
        }
        if let Some(a) = &self.agent {
            raw.insert("agent".into(), a.clone().into());
        }
        raw.insert("loadSession".into(), self.load_session.into());
        raw.insert(
            "sessionCapabilities".into(),
            serde_json::Value::from(self.session_caps.clone()),
        );
        crate::events::SessionEvent::System {
            subtype: "acp_capabilities".into(),
            raw: serde_json::Value::Object(raw).to_string(),
        }
    }
}

#[cfg(test)]
mod advertisement_tests {
    use super::*;
    use crate::events::SessionEvent;
    use serde_json::json;

    /// Grok 1.0.5's `agentCapabilities`, copied from a live
    /// `grok agent --no-leader stdio` handshake on 2026-08-21. The model
    /// catalog, MCP config and `currentWorkingDirectory` that came with it
    /// are omitted deliberately — see [`Advertisement`] on why none of
    /// that reaches the journal.
    fn grok_1_0_5() -> serde_json::Value {
        json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
                "mcpCapabilities": { "http": true, "sse": true },
                "sessionCapabilities": { "list": {}, "resume": {}, "close": {} },
                "auth": {},
            },
        })
    }

    /// Hermes 0.20.4's, from `hermes acp` on the same day.
    fn hermes_0_20_4() -> serde_json::Value {
        json!({
            "protocolVersion": 1,
            "agentInfo": { "name": "hermes-agent", "version": "0.20.4" },
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": true },
                "sessionCapabilities": { "fork": {}, "list": {}, "resume": {} },
            },
        })
    }

    /// The finding slice CL exists to record: eave's two ACP agents do NOT
    /// advertise the same session lifecycle, so there is no lifecycle
    /// method beyond `load` that both would answer.
    ///
    /// Grok has `close` and lacks `fork`; Hermes has `fork` and lacks
    /// `close`. Asking grok to fork answers `-32601 Method not found`
    /// (verified live), so shipping either as a shared-core feature would
    /// ship a broken call on one of the two.
    #[test]
    fn the_two_acp_agents_disagree_about_which_session_methods_exist() {
        let grok = advertisement(Some(&grok_1_0_5()));
        let hermes = advertisement(Some(&hermes_0_20_4()));

        assert_eq!(grok.session_caps, ["close", "list", "resume"]);
        assert_eq!(hermes.session_caps, ["fork", "list", "resume"]);

        assert!(
            !grok.session_caps.iter().any(|c| c == "fork"),
            "grok answered -32601 to session/fork on 2026-08-21; if it now \
             advertises fork, re-probe before trusting this"
        );
        assert!(
            !hermes.session_caps.iter().any(|c| c == "close"),
            "Hermes' acp_adapter/server.py has no close_session handler at all"
        );
        assert!(grok.load_session && hermes.load_session);
    }

    #[test]
    fn the_agent_label_and_protocol_version_are_read() {
        let a = advertisement(Some(&hermes_0_20_4()));
        assert_eq!(a.agent.as_deref(), Some("hermes-agent/0.20.4"));
        assert_eq!(a.protocol_version, Some(1));
        // Grok states no `agentInfo`, and an absent label is recorded as
        // absent rather than invented.
        assert_eq!(advertisement(Some(&grok_1_0_5())).agent, None);
    }

    /// Fail-closed: a capability eave was not told about is not one it has.
    #[test]
    fn an_unstated_capability_is_false_not_assumed() {
        for result in [json!({}), json!({ "agentCapabilities": {} }), json!(null)] {
            let a = advertisement(Some(&result));
            assert!(!a.load_session, "loadSession must not default true");
            assert!(a.session_caps.is_empty());
        }
        assert_eq!(advertisement(None), Advertisement::default());
    }

    /// `_meta` is ACP's extensibility escape hatch. Counting it would put
    /// a `session/_meta` in the record that no agent implements.
    #[test]
    fn the_meta_escape_hatch_is_not_a_capability() {
        let a = advertisement(Some(&json!({
            "agentCapabilities": {
                "sessionCapabilities": { "_meta": { "x": 1 }, "list": {} },
            },
        })));
        assert_eq!(a.session_caps, ["list"]);
    }

    /// The record is a projection eave builds, never the agent's bytes.
    /// Grok's real `initialize` result carries `currentWorkingDirectory`
    /// and its neighbouring frames carried a bearer token out of the
    /// operator's MCP config; slice CI had to filter exactly that out of
    /// its fixture. Nothing the agent supplies beyond the capability facts
    /// may reach the journal.
    #[test]
    fn the_journal_record_carries_no_operator_paths_or_secrets() {
        let mut result = grok_1_0_5();
        result["_meta"] = json!({
            "currentWorkingDirectory": "/Users/someone/secret-project",
            "token": "Bearer phx_notarealtoken",
        });
        let SessionEvent::System { subtype, raw } = advertisement(Some(&result)).record() else {
            panic!("the advertisement must record as a System event");
        };
        assert_eq!(subtype, "acp_capabilities");
        assert!(!raw.contains("/Users/"), "operator path leaked: {raw}");
        assert!(!raw.contains("phx_"), "credential leaked: {raw}");
        assert!(raw.contains(r#""sessionCapabilities":["close","list","resume"]"#));
        assert!(raw.contains(r#""loadSession":true"#));
    }

    /// A wire-sourced string copied into a permanent record is bounded,
    /// for the same reason `wire::malformed` bounds a bad line.
    #[test]
    fn a_hostile_agent_label_cannot_flood_the_journal() {
        let a = advertisement(Some(&json!({
            "agentInfo": { "name": "x".repeat(100_000), "version": "1" },
        })));
        let label = a.agent.expect("a long label is still a label");
        assert_eq!(label.chars().count(), crate::acp::wire::MALFORMED_LINE_CAP);
    }
}
