//! Fold a live session's event stream into its journal and meta (spec 02).
//!
//! Two inputs: the provider event channel from the session, and an
//! out-of-band channel for engine-side records (`UserSent` at send time).
//! Write-behind: appends buffer and reach the OS every
//! `JOURNAL_FLUSH_INTERVAL`; terminal records fsync at append.

use std::io;
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::events::SessionEvent;
use crate::journal::{classify_exit, JournalEvent, JournalWriter, Outcome, Record};
use crate::proc;
use crate::session::{AgentSession, ImageRef};
use crate::store::Meta;
use crate::thresholds::JOURNAL_FLUSH_INTERVAL;

/// Ask the provider to rewind, and journal it **only if it agreed**
/// (Slice AG1 #10).
///
/// **The order is the exact reverse of [`send_and_journal`], and the
/// asymmetry is the point.** A send is journalled FIRST because a send
/// that failed to reach the pipe is still a fact worth keeping — the
/// operator typed it, and spec 02's delivery state exists to show it as
/// undelivered. A rewind is journalled LAST because a rewind that did not
/// happen is not a fact at all: the record makes the projection drop rows,
/// so writing one for a refused rewind would hide turns the agent still
/// remembers, and the transcript would disagree with the conversation
/// permanently and silently.
///
/// So: refused or unsupported leaves the journal untouched.
pub async fn rewind_and_journal<S: AgentSession + ?Sized>(
    session: &mut S,
    extra: &mpsc::Sender<JournalEvent>,
    target: &crate::session::RewindTarget,
) -> io::Result<crate::session::RewindOutcome> {
    let gone = |_| io::Error::new(io::ErrorKind::BrokenPipe, "recorder is gone");
    let outcome = session
        .rewind(target)
        .await
        .map_err(|e| io::Error::other(format!("rewind failed: {e}")))?;
    if let crate::session::RewindOutcome::Rewound { .. } = &outcome {
        // `msg_id` is `Some` on this path or the adapter could not have
        // addressed the rewind at all — Claude's own arm returns `Refused`
        // for `None` before it writes a frame.
        if let Some(id) = &target.msg_id {
            extra
                .send(JournalEvent::Rewound {
                    through_msg_id: id.clone(),
                    dropped: target.turns_dropped,
                })
                .await
                .map_err(gone)?;
        }
    }
    Ok(outcome)
}

/// Journal-then-send: append `UserSent` *before* touching the pipe, and
/// confirm with `UserDelivered` only after the write succeeds. Returns
/// whether the message was delivered. A pipe failure is not an error —
/// the send stays journaled as undelivered, which is exactly the state
/// the UI must distinguish from sent (spec 02 §Delivery state). `Err`
/// only if the recorder is gone (nothing can be journaled at all).
/// Journal the send, attempt it, and journal the delivery.
///
/// `Ok(None)` = delivered. `Ok(Some(reason))` = journaled but NOT delivered,
/// and `reason` is the agent's own words about why.
pub async fn send_and_journal<S: AgentSession + ?Sized>(
    session: &mut S,
    extra: &mpsc::Sender<JournalEvent>,
    text: &str,
    images: &[ImageRef],
) -> io::Result<Option<String>> {
    let gone = |_| io::Error::new(io::ErrorKind::BrokenPipe, "recorder is gone");
    extra
        .send(JournalEvent::UserSent { text: text.into() })
        .await
        .map_err(gone)?;
    match session.send_user_message(text, images).await {
        Ok(()) => {
            extra
                .send(JournalEvent::UserDelivered {})
                .await
                .map_err(gone)?;
            Ok(None)
        }
        // The reason is KEPT. It used to be `Err(_) => Ok(false)`, and that
        // single underscore is why an undelivered send has been
        // undiagnosable: the delivery contract is a bool by design (spec 02
        // §Delivery state), but discarding the cause meant the only
        // observable was "false", with nothing anywhere saying why. A test
        // that fails on it can report the assertion and never the fault,
        // which is how `files_channel_never_crosses_sessions` has been an
        // open flake across several slices — not hard to fix, impossible to
        // even look at.
        //
        // Still `Ok`: a refused write is a fact about the agent, not an
        // error in the engine, and the caller's contract is unchanged.
        //
        // Slice AE: keeping the io error was not enough either. Every
        // failure of this write reads `Broken pipe (os error 32)`, and one
        // string covers two states a reader must not confuse:
        //
        //   - the agent is alive and refused the write — a fact about the
        //     agent, which is what the bool was designed to say;
        //   - the agent was already DEAD when we wrote — a session that
        //     never started, or has since exited, where "the agent refused
        //     your message" is simply untrue.
        //
        // Reproduced deterministically 2026-08-20: an agent that exits at
        // spawn yields exactly `delivered = false` with reason `io error:
        // Broken pipe (os error 32)`, indistinguishable until now from a
        // live agent refusing. Liveness is answered from the OS rather
        // than from a flag a supervisor task still has to set: the (pid,
        // start_time) guard is an immediate fact and cannot race the exit
        // it reports on.
        Err(e) => {
            let id = session.identity();
            if proc::identity_matches(&id) {
                Ok(Some(e.to_string()))
            } else {
                Ok(Some(format!(
                    "the agent process (pid {}) was already gone when the message was \
                     written, so nothing refused it — the session is dead, not busy ({e})",
                    id.pid
                )))
            }
        }
    }
}

/// Run until the session's `Exited` event, which is classified into the
/// journal's terminal `Closed` record (exit 143 = killed, not crashed —
/// research: claude-stream-notes.md). Returns the outcome.
///
/// `tap`, when present, receives every record as it is appended (seq and
/// timestamp stamped) — the engine's fan-out to wire subscribers. The tap
/// sees records before the write-behind flush reaches disk; that is sound
/// because a crash that loses the tail also changes the epoch on reload,
/// which forces subscribers through a `Reset` (spec 05-wire.md).
pub async fn record_session(
    mut events: mpsc::Receiver<SessionEvent>,
    mut extra: mpsc::Receiver<JournalEvent>,
    mut writer: JournalWriter,
    session_dir: PathBuf,
    mut meta: Meta,
    tap: Option<mpsc::UnboundedSender<Record>>,
) -> io::Result<Outcome> {
    let tap_send = |rec: Record| {
        if let Some(tap) = &tap {
            let _ = tap.send(rec);
        }
    };
    let mut tick = tokio::time::interval(JOURNAL_FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut extra_open = true;

    loop {
        tokio::select! {
            // **Biased, and `extra` goes first.** These two channels carry
            // one causal order — eave sends, then the CLI answers — and an
            // unbiased `select!` picks a ready branch at RANDOM, so a fast
            // CLI's whole reply could be journalled ahead of the
            // `UserSent` that caused it. Measured 2026-08-29: a journal
            // reading init / text_delta / assistant_text / turn_completed
            // / user_sent / user_delivered, about one run in four.
            //
            // `UserSent` is enqueued here before the prompt is written to
            // the child's stdin, so draining `extra` first makes that
            // ordering hold rather than leaving it to chance.
            // `UserDelivered` is enqueued after the write and may still
            // land behind the reply — that case is fine and is what the
            // `prompted` balance in `turn.rs` was built for.
            biased;
            ev = extra.recv(), if extra_open => {
                match ev {
                    Some(ev) => {
                        let rec = writer.append(ev)?;
                        tap_send(rec);
                    }
                    None => extra_open = false,
                }
            }
            ev = events.recv() => {
                let Some(ev) = ev else {
                    // `Exited` is contractually the final event; a bare
                    // close means our own process is going down. Flush what
                    // we have — the next load's crash-tail repair settles it.
                    writer.sync()?;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "event stream ended without Exited",
                    ));
                };
                match ev {
                    SessionEvent::Exited { code, signal, stderr_tail } => {
                        let outcome = classify_exit(code, signal, &stderr_tail);
                        let rec = writer.append(JournalEvent::Closed { outcome: outcome.clone() })?;
                        writer.sync()?;
                        tap_send(rec);
                        meta.state = outcome.state_name().into();
                        meta.child = None;
                        // Stage AB: this task holds a `Meta` snapshot taken
                        // at spawn, so a rename that landed mid-turn is not
                        // in it. Preserve the on-disk title rather than
                        // writing back a stale one.
                        meta.save_keeping_operator_fields(&session_dir)?;
                        return Ok(outcome);
                    }
                    ev => {
                        if let SessionEvent::Init { session_id, .. } = &ev {
                            meta.session_id = session_id.clone();
                            meta.save_keeping_operator_fields(&session_dir)?;
                        }
                        let terminal = matches!(ev, SessionEvent::TurnCompleted { .. });
                        let rec = writer.append(JournalEvent::Provider { event: ev })?;
                        if terminal {
                            writer.sync()?;
                        }
                        tap_send(rec);
                    }
                }
            }
            _ = tick.tick() => {
                writer.flush()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Header;
    use crate::session::{RewindOutcome, SessionError};
    use crate::store::{Meta, META_FORMAT_VERSION};

    /// A session that answers a rewind however the test says, and records
    /// whether it was asked at all.
    struct Rewinder(RewindOutcome);

    #[async_trait::async_trait]
    impl AgentSession for Rewinder {
        fn identity(&self) -> crate::proc::ProcIdentity {
            crate::proc::ProcIdentity {
                pid: 1,
                start_tvsec: 0,
                start_tvusec: 0,
            }
        }
        fn provider_session_id(&self) -> Option<String> {
            None
        }
        fn in_flight_tools(&self) -> usize {
            0
        }
        fn last_activity(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
        fn watch(&self) -> crate::session::SessionWatch {
            crate::session::SessionWatch::new(
                self.identity(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
        }
        async fn send_user_message(&mut self, _: &str, _: &[ImageRef]) -> Result<(), SessionError> {
            Ok(())
        }
        fn pending_approvals(&self) -> usize {
            0
        }
        async fn respond_to_approval(
            &mut self,
            _: &str,
            _: &crate::session::ApprovalDecision,
        ) -> Result<bool, SessionError> {
            Ok(true)
        }
        async fn kill(&mut self) -> Result<crate::session::KillReport, SessionError> {
            unreachable!("not killed in these tests")
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn rewind(
            &mut self,
            _: &crate::session::RewindTarget,
        ) -> Result<RewindOutcome, SessionError> {
            Ok(self.0.clone())
        }
    }

    fn a_target() -> crate::session::RewindTarget {
        crate::session::RewindTarget {
            msg_id: Some("u-1".into()),
            last_seen_msg_id: Some("u-2".into()),
            turns_dropped: 2,
        }
    }

    /// **The invariant that keeps a transcript honest.** The record is what
    /// makes the projection drop rows, so writing one for a rewind that did
    /// not happen would hide turns the agent still remembers — permanently,
    /// on every later replay, with nothing on screen to say so.
    ///
    /// This is the exact reverse of `send_and_journal`'s order, and the
    /// test asserts the reversal rather than the mechanism: ask first,
    /// journal only on yes.
    #[tokio::test]
    async fn a_refused_rewind_is_never_journalled() {
        for outcome in [
            RewindOutcome::Refused {
                reason: "turn running".into(),
            },
            RewindOutcome::Unsupported,
        ] {
            let (tx, mut rx) = mpsc::channel(4);
            let mut session = Rewinder(outcome.clone());
            let got = rewind_and_journal(&mut session, &tx, &a_target())
                .await
                .expect("the ask itself succeeded");
            assert_eq!(got, outcome);
            assert!(
                rx.try_recv().is_err(),
                "a rewind that did not happen must leave the journal alone"
            );
        }
    }

    /// And on yes, the record names the SAME message the provider was
    /// asked about — the projection folds by that id, so a different one
    /// would cut the transcript somewhere the conversation was not cut.
    #[tokio::test]
    async fn an_agreed_rewind_is_journalled_against_the_id_it_asked_about() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut session = Rewinder(RewindOutcome::Rewound {
            prefill: Some("first ask".into()),
        });
        rewind_and_journal(&mut session, &tx, &a_target())
            .await
            .expect("rewound");
        let rec = rx.try_recv().expect("one record");
        assert!(
            matches!(&rec, JournalEvent::Rewound { through_msg_id, dropped }
                if through_msg_id == "u-1" && *dropped == 2),
            "{rec:?}"
        );
        assert!(rx.try_recv().is_err(), "exactly one record");
    }

    /// **The two channels carry one causal order, and `select!` must not
    /// pick between them at random.**
    ///
    /// eave enqueues `UserSent` on `extra` BEFORE the prompt is written to
    /// the child's stdin, so a reply can only exist because that record
    /// already exists. An unbiased `tokio::select!` polls a ready branch at
    /// random, and measured on 2026-08-29 that produced journals reading
    /// `init / text_delta / assistant_text / turn_completed / user_sent /
    /// user_delivered` — the whole reply ahead of the prompt that caused
    /// it — in about one run in four.
    ///
    /// That ordering is not cosmetic. `TurnLedger::agent_opener` uses
    /// `sent_unconfirmed` to refuse to mint an agent turn out of an
    /// ordinary reply; with `UserSent` not yet folded the guard reads 0,
    /// the reply mints a phantom `Unannounced` turn, and its
    /// `TurnCompleted` then closes THAT instead of pre-cancelling the
    /// prompted balance. The late `UserDelivered` opens a turn nothing will
    /// ever close, and the session reports `quiet` and then `unresponsive`
    /// while it sits perfectly healthy. The acceptance test that caught it
    /// is `eave-engine/tests/attention.rs::
    /// idle_between_turns_after_turn_completed_is_no_state`, which its own
    /// docstring calls "THE regression that matters most".
    ///
    /// **Not deterministic in the failing direction, and saying so
    /// matters.** Both channels are filled before the recorder runs, so
    /// both branches are ready on the first poll — the case `biased;`
    /// decides and an unbiased `select!` decides by coin flip. Measured
    /// with `biased;` removed: 5 failures in 12 runs. That is a better
    /// detector than the acceptance test's one-in-four AND it names the
    /// cause, but a single green run of this is not proof the bias is
    /// still there. Read the `biased;` if you need to know.
    #[tokio::test]
    async fn the_journal_keeps_eaves_own_record_ahead_of_the_reply_it_caused() {
        let dir = std::env::temp_dir().join(format!("eave-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("journal.jsonl");
        let writer = JournalWriter::create(
            &path,
            &Header {
                format_version: 1,
                session_id: "s1".into(),
                provider: "claude-code".into(),
                cwd: "/tmp".into(),
                created_ts_ms: 1,
                agent: Default::default(),
            },
        )
        .expect("writer");
        let meta = Meta {
            format_version: META_FORMAT_VERSION,
            provider: "claude-code".into(),
            cwd: "/tmp".into(),
            session_id: "s1".into(),
            state: "new".into(),
            child: None,
            title: None,
            archived: false,
        };
        meta.save(&dir).expect("meta");

        let (ev_tx, ev_rx) = mpsc::channel(16);
        let (extra_tx, extra_rx) = mpsc::channel(16);
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();

        // Both ready before the loop runs: eave's own record, and a reply
        // that could only have been provoked by it.
        extra_tx
            .send(JournalEvent::UserSent { text: "go".into() })
            .await
            .expect("user_sent");
        ev_tx
            .send(SessionEvent::AssistantText {
                text: "done".into(),
            })
            .await
            .expect("reply");
        ev_tx
            .send(SessionEvent::TurnCompleted {
                subtype: "success".into(),
                is_error: false,
            })
            .await
            .expect("result");
        ev_tx
            .send(SessionEvent::Exited {
                code: Some(0),
                signal: None,
                stderr_tail: String::new(),
            })
            .await
            .expect("exit");

        let task = tokio::spawn(record_session(
            ev_rx,
            extra_rx,
            writer,
            dir.clone(),
            meta,
            Some(tap_tx),
        ));
        let _ = task.await.expect("join");

        let mut order: Vec<String> = Vec::new();
        while let Ok(rec) = tap_rx.try_recv() {
            order.push(match rec.event {
                JournalEvent::UserSent { .. } => "user_sent".into(),
                JournalEvent::Provider { event } => format!("{event:?}")
                    .split_whitespace()
                    .next()
                    .unwrap_or("provider")
                    .to_string(),
                other => format!("{other:?}"),
            });
        }
        let first = order.first().map(String::as_str);
        assert_eq!(
            first,
            Some("user_sent"),
            "THE REPLY WAS JOURNALLED AHEAD OF THE PROMPT THAT CAUSED IT: \
             {order:?}"
        );
    }
}
