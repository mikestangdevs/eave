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
use crate::session::AgentSession;
use crate::store::Meta;
use crate::thresholds::JOURNAL_FLUSH_INTERVAL;

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
) -> io::Result<Option<String>> {
    let gone = |_| io::Error::new(io::ErrorKind::BrokenPipe, "recorder is gone");
    extra
        .send(JournalEvent::UserSent { text: text.into() })
        .await
        .map_err(gone)?;
    match session.send_user_message(text).await {
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
                        meta.save_keeping_title(&session_dir)?;
                        return Ok(outcome);
                    }
                    ev => {
                        if let SessionEvent::Init { session_id, .. } = &ev {
                            meta.session_id = session_id.clone();
                            meta.save_keeping_title(&session_dir)?;
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
            ev = extra.recv(), if extra_open => {
                match ev {
                    Some(ev) => {
                        let rec = writer.append(ev)?;
                        tap_send(rec);
                    }
                    None => extra_open = false,
                }
            }
            _ = tick.tick() => {
                writer.flush()?;
            }
        }
    }
}
