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
use crate::journal::{classify_exit, JournalEvent, JournalWriter, Outcome};
use crate::session::AgentSession;
use crate::store::Meta;
use crate::thresholds::JOURNAL_FLUSH_INTERVAL;

/// Journal-then-send: append `UserSent` *before* touching the pipe, and
/// confirm with `UserDelivered` only after the write succeeds. Returns
/// whether the message was delivered. A pipe failure is not an error —
/// the send stays journaled as undelivered, which is exactly the state
/// the UI must distinguish from sent (spec 02 §Delivery state). `Err`
/// only if the recorder is gone (nothing can be journaled at all).
pub async fn send_and_journal<S: AgentSession + ?Sized>(
    session: &mut S,
    extra: &mpsc::Sender<JournalEvent>,
    text: &str,
) -> io::Result<bool> {
    let gone = |_| io::Error::new(io::ErrorKind::BrokenPipe, "recorder is gone");
    extra
        .send(JournalEvent::UserSent { text: text.into() })
        .await
        .map_err(gone)?;
    match session.send_user_message(text).await {
        Ok(()) => {
            extra.send(JournalEvent::UserDelivered {}).await.map_err(gone)?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Run until the session's `Exited` event, which is classified into the
/// journal's terminal `Closed` record (exit 143 = killed, not crashed —
/// research: claude-stream-notes.md). Returns the outcome.
pub async fn record_session(
    mut events: mpsc::Receiver<SessionEvent>,
    mut extra: mpsc::Receiver<JournalEvent>,
    mut writer: JournalWriter,
    session_dir: PathBuf,
    mut meta: Meta,
) -> io::Result<Outcome> {
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
                        writer.append(JournalEvent::Closed { outcome: outcome.clone() })?;
                        writer.sync()?;
                        meta.state = outcome.state_name().into();
                        meta.child = None;
                        meta.save(&session_dir)?;
                        return Ok(outcome);
                    }
                    ev => {
                        if let SessionEvent::Init { session_id } = &ev {
                            meta.session_id = session_id.clone();
                            meta.save(&session_dir)?;
                        }
                        let terminal = matches!(ev, SessionEvent::TurnCompleted { .. });
                        writer.append(JournalEvent::Provider { event: ev })?;
                        if terminal {
                            writer.sync()?;
                        }
                    }
                }
            }
            ev = extra.recv(), if extra_open => {
                match ev {
                    Some(ev) => { writer.append(ev)?; }
                    None => extra_open = false,
                }
            }
            _ = tick.tick() => {
                writer.flush()?;
            }
        }
    }
}
