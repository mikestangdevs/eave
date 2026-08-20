//! The Stage D GPUI shell: virtualized transcript with stable ids and
//! fingerprints, markdown mending, paint-only fade veil, anchor-flip
//! scroll (spec 03 §Rendering, §Scroll). The seam is unchanged from
//! Stage C: the UI holds no authoritative state, only the shared
//! [`Transcript`] projection fed by resubscribe + replay (spec 03
//! §Restart survival).
//!
//! Architecture per spec 03 §Wake & notify, scaled to one session: a
//! client task owns the socket, decodes frames onto **per-concern queues**
//! (connection / feed), and pokes a single **bounded(1) wake channel**; a
//! foreground pump drains every non-empty queue in one pass and notifies
//! only the entities whose queues were non-empty — ≤1 notify per entity
//! per burst. Frame pacing comes from the engine's strict 120 ms delta
//! folding — no UI frame-pacing timer (spec 03 §Streaming commit).

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::Animation;
use gpui::AnimationExt as _;
use gpui::{
    actions, div, list, point, prelude::*, px, size, AnyElement, App, Bounds, ClickEvent, Context,
    Corners, CursorStyle, DispatchPhase, Element, Entity, FocusHandle, Font, FontStyle, FontWeight,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, KeyBinding, KeyDownEvent,
    LayoutId, ListAlignment, ListOffset, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString, StrikethroughStyle,
    StyledText, TextLayout, TextRun, TitlebarOptions, UnderlineStyle, Window,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowHandle, WindowOptions,
};
use gpui_platform::application;
use tokio::sync::mpsc as tokio_mpsc;

use eave_core::session::AgentConfig;
use eave_engine::client::{Client, ClientError, EngineSpawn};
use eave_wire::model::{ApprovalStatus, Delivery, Row, ToolStatus, Transcript};
use eave_wire::{
    AttentionSnapshot, ChangedFile, Cursor, DiffBasis, DiffHunk, DiffKind, DiffLine, DiffOp,
    FileDiff, FileScope, FileStatus, FilesSnapshot, GitState, SessionInfo, SubscriptionItem,
    WaitReason,
};

use crate::display::{DisplayList, RowId};
use crate::edge_fade::edge_faded;
use crate::icons;
use crate::launcher::{self, PROVIDERS};
use crate::loaders;
use crate::markdown::{self, inline_runs, Align, Block, InlineRun, Marker};
use crate::motion::{self, HoverFade as _};
use crate::overlay::{self, MenuItem, Popup};
use crate::theme;

actions!(eave, [Quit, ApproveOldest, DenyOldest]);

/// How much of a tool's input the approval card shows before it states
/// that it is holding back (Slice K). A 4 KB `Write` must not push its own
/// approve button off the screen; a truncated input that does not SAY it
/// is truncated would be the same lie as any other half-stated fact.
const APPROVAL_INPUT_CAP: usize = 1200;

// ---------------------------------------------------------------------------
// Client task ⇄ UI plumbing (unchanged seam from Stage C)

#[derive(Debug, Clone)]
enum ConnEvent {
    Connecting,
    Connected {
        engine_pid: i32,
    },
    Reconnecting {
        attempt: u32,
    },
    /// Contended start: another engine holds the data dir (spec 02
    /// §Contended start). `None` = "holder unknown" — still actionable.
    Busy {
        holder_pid: Option<i32>,
    },
    Failed {
        message: String,
    },
}

/// Feed frames are tagged with the session they belong to: a selection
/// switch drops the old subscription, but frames from it may already sit
/// in the queue — the pump discards anything not matching the transcript's
/// current session (no event bleed between sessions).
#[derive(Debug)]
enum FeedEvent {
    Session {
        id: String,
    },
    /// Nothing is selected and nothing will be opened for you — the
    /// launcher's state (Slice Q2). Distinct from "the selected session's
    /// journal is empty": this says there is no session at all.
    ///
    /// `error` carries the engine's own words when a launch was refused —
    /// the posture gate's message, verbatim. Silence there would be the
    /// worst of both: a button that does nothing and says nothing.
    NoSession {
        error: Option<String>,
    },
    Reset {
        id: String,
    },
    Records {
        id: String,
        epoch: u64,
        records: Vec<eave_core::journal::Record>,
    },
}

/// One drained attention burst; last frame wins (the overlay is a current-
/// state broadcast, not a log). Tagged with its session so a notice from a
/// deselected session can never render over the selected one.
type AttnEvent = (String, AttentionSnapshot);

/// One watch snapshot (spec 05 §Watch): the full session set, folded by
/// replacement — never merged.
type RailEvent = Vec<SessionInfo>;

/// One files snapshot (spec 05 §Files), session-tagged like attention so
/// a deselected session's file list can never render over the selected
/// one. `None` snapshot = the engine refused the subscription (an engine
/// predating the method) — a stated fact, not silence.
type FilesEvent = (String, Option<FilesSnapshot>);

/// One diff reply (B2), tagged with the session AND path it answers so a
/// stale reply — a switch or a re-click raced the request — can never
/// render under the wrong row. `Err` is the engine's stated message
/// (including "unknown method" from an engine predating diffs).
type DiffEvent = (String, String, Result<FileDiff, String>);

/// What one of the rail's row actions had to say, session-tagged. The
/// rail states it on that session's row; `None` clears any standing note
/// (the action landed, and the watch snapshot carries the new facts).
///
/// It began as `WtNote` for Slice I2's isolate/remove pair. Stage AC put
/// rename and archive on the same row, and their refusals have exactly
/// the same shape and the same home — so the type was widened and
/// renamed rather than duplicated. A second per-session note channel is
/// a second thing that can be showing when the first one should be.
#[derive(Debug, Clone)]
enum RowNote {
    /// The engine refused a non-forced worktree delete, restating fresh
    /// counts — the delete affordance is now armed: the next click means
    /// it.
    ConfirmDelete {
        dirty_files: Option<u64>,
        unmerged_commits: Option<u64>,
    },
    /// A stated refusal from any row action, in the ENGINE's own words.
    ///
    /// Rename and archive can both fail — an unreadable `meta.json`
    /// refuses both — and a rename that silently did not stick is the
    /// worst version of this: the operator types a name, presses return,
    /// the field closes, and the row says something else. The refusal is
    /// stated on the row it was aimed at.
    Error(String),
}

type RowNoteEvent = (String, Option<RowNote>);

#[derive(Debug)]
enum Command {
    Send(String),
    Kill,
    /// Projection lost sync (seq gap): drop and fully resubscribe.
    Resubscribe,
    /// Rail row click: subscribe this session instead (cursorless — the
    /// UI holds no projection for it; full replay, never merge).
    Select(String),
    /// Rail "+ new": go to the launcher (Slice Q2). This used to open a
    /// session immediately with the engine's defaults; it now deselects
    /// and lets you choose an agent, which is the whole point of having a
    /// rest state.
    NewSession,
    /// Launcher "start session" (Slice Q2): open with a stated agent and
    /// select it. The engine refuses a provider whose posture it has not
    /// proven, so a wrong `provider` here produces a stated error rather
    /// than an unreviewed child process.
    Launch {
        provider: String,
        /// `None` = pass no `--model` at all, which is not the same as
        /// passing an empty one (`AgentConfig::model`).
        model: Option<String>,
    },
    /// Files-rail basis toggle (Slice AA): re-open the `files`
    /// subscription against the other basis. It is a resubscribe, not a
    /// filter — the engine only computes the turn-scoped list while
    /// somebody is subscribed to it, so "which one is on screen" and
    /// "which one is being computed" are deliberately the same fact.
    FilesScope(FileScope),
    /// Files-rail row click (B2): fetch one file's diff. Unary and
    /// re-issued on every fresh snapshot while the row stays expanded —
    /// the files channel is the invalidation signal.
    Diff(String),
    /// Rail rename (Stage AC): set this session's operator title, or
    /// clear it with `None`.
    ///
    /// The text is sent RAW. Normalization — trim, fold to one line, cap
    /// at `TITLE_MAX_CHARS` — is the engine's, and the reply carries what
    /// actually landed, so there is exactly one implementation of what a
    /// title is. An empty field is `None`: clearing a name is the same
    /// verb as setting one.
    Rename {
        id: String,
        title: Option<String>,
    },
    /// Rail archive click (Stage AC): file a session away, or bring it
    /// back. One bit; the engine never touches the journal for it, so it
    /// is reversible by sending the other value.
    SetArchived {
        id: String,
        archived: bool,
    },
    /// Rail "isolate" click (Slice I2): opt this never-started session
    /// into its own worktree. The watch snapshot carries the result;
    /// refusals surface as a stated note on the row.
    Isolate(String),
    /// Rail "delete worktree" click (Slice I2). `force: false` first —
    /// the engine refuses with fresh counts if anything would be lost,
    /// which arms the affordance; the second click sends `force: true`
    /// (decision 2's two-step).
    RemoveWorktree {
        id: String,
        force: bool,
    },
    /// Answer one open permission question (Slice K). `message` is the
    /// operator's own words on a deny — the provider hands them to the
    /// model verbatim, so "denied, commit first" is a different
    /// instruction from "denied".
    Decide {
        request_id: String,
        allow: bool,
        message: String,
    },
}

#[derive(Default)]
struct Queues {
    conn: Mutex<VecDeque<ConnEvent>>,
    feed: Mutex<VecDeque<FeedEvent>>,
    attn: Mutex<VecDeque<AttnEvent>>,
    rail: Mutex<VecDeque<RailEvent>>,
    files: Mutex<VecDeque<FilesEvent>>,
    diffs: Mutex<VecDeque<DiffEvent>>,
    notes: Mutex<VecDeque<RowNoteEvent>>,
}

#[derive(Clone)]
struct Plumbing {
    queues: Arc<Queues>,
    wake: futures::channel::mpsc::Sender<()>,
}

impl Plumbing {
    fn push_conn(&self, ev: ConnEvent) {
        self.queues.conn.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_feed(&self, ev: FeedEvent) {
        self.queues.feed.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_attn(&self, ev: AttnEvent) {
        self.queues.attn.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_rail(&self, ev: RailEvent) {
        self.queues.rail.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_files(&self, ev: FilesEvent) {
        self.queues.files.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_diff(&self, ev: DiffEvent) {
        self.queues.diffs.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
    fn push_row_note(&self, ev: RowNoteEvent) {
        self.queues.notes.lock().unwrap().push_back(ev);
        let _ = self.wake.clone().try_send(());
    }
}

/// The client task: connect (spawning the engine if absent), watch the
/// session set, subscribe the selected session, forward frames. On
/// disconnect, reconnect with doubling backoff and a 5-attempt breaker
/// (spec 01 §Topology) and resubscribe from the held cursor — replay,
/// never merge. Selection lives here as "which session am I subscribed
/// to"; the UI only ever requests a switch (`Command::Select`) and renders
/// what the feed reports back — the rail's highlight is a projection of
/// the actual subscription, not a UI-held selection.
async fn feed_loop(
    data_dir: PathBuf,
    plumbing: Plumbing,
    mut commands: tokio_mpsc::UnboundedReceiver<Command>,
) {
    let spawn = EngineSpawn {
        program: std::env::current_exe().expect("current exe"),
        args: vec![
            "--engine".into(),
            "--data-dir".into(),
            data_dir.display().to_string(),
        ],
    };
    // `EAVE_NO_SPAWN=1` (dev-loop): never auto-spawn an engine, only
    // connect. Closes the spawn race against a hand-launched engine with
    // custom env: if that engine dies, the app's respawn (using the
    // APP's env — agent defaults to real claude) would win the lock race
    // inside its 250 ms backoff and quietly change what agent sessions
    // spawn. With the gate set, the human relaunching their engine
    // always wins.
    let no_spawn = std::env::var("EAVE_NO_SPAWN")
        .map(|v| v == "1")
        .unwrap_or(false);
    let mut attempt: u32 = 0;
    let mut cursor: Option<Cursor> = None;
    let mut selected: Option<String> = None;
    // Slice Q2. Set by `+ new`, cleared by anything that picks a session.
    // Without it, the "degrade to the most recent session" rule below
    // would bounce you straight back out of the launcher — asking for a
    // new session and being handed the old one is the bug this flag
    // exists to prevent, not a corner case.
    let mut at_launcher = false;
    // Slice AA: the basis the files rail is asking for. Loop-level, above
    // both labels, because it is an operator preference — it must survive
    // a session switch and a reconnect, not be reset by them.
    let mut files_scope = FileScope::WorkingTree;
    'conn: loop {
        plumbing.push_conn(if attempt == 0 {
            ConnEvent::Connecting
        } else {
            ConnEvent::Reconnecting { attempt }
        });
        let connected = if no_spawn {
            Client::connect(&data_dir).await
        } else {
            Client::connect_or_spawn(&data_dir, &spawn).await
        };
        let client = match connected {
            // Arc: diff requests run as their own tasks off this loop (a
            // slow repo's diff must not stall sends/kills or the feed).
            Ok(c) => Arc::new(c),
            Err(ClientError::Busy { holder }) => {
                plumbing.push_conn(ConnEvent::Busy {
                    holder_pid: holder.map(|h| h.pid),
                });
                // Keep probing: the other instance may quit.
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            Err(e) => {
                attempt += 1;
                if attempt >= 5 {
                    plumbing.push_conn(ConnEvent::Failed {
                        message: format!("engine unreachable after 5 attempts: {e}"),
                    });
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                continue;
            }
        };
        attempt = 0;
        let mut watch_rx = match client.watch().await {
            Ok(rx) => rx,
            Err(_) => continue 'conn,
        };
        plumbing.push_conn(ConnEvent::Connected {
            engine_pid: client.engine.pid,
        });

        'session: loop {
            // Validate the held selection against the engine's set; a
            // vanished session (fresh data dir, changed disk) degrades to
            // the most recently ACTIVE session (Stage AB — it used to be
            // the most recently created, which after a restart is rarely
            // the one you were working in). Sessions that never ran fall
            // back to creation order, so the old behaviour is the floor
            // rather than a special case.
            let sessions = match client.list().await {
                Ok(s) => s,
                Err(_) => continue 'conn,
            };
            // Slice Q2 REMOVED the last arm of this: an empty engine used
            // to be met with `client.open(None, None)` right here, which
            // is why eave had no rest state to design. A session is now
            // opened only by `Command::Launch` — i.e. only because someone
            // asked for one, under a stated agent.
            let session_id = if at_launcher {
                None
            } else {
                selected
                    .clone()
                    .filter(|id| sessions.iter().any(|s| &s.session_id == id))
                    .or_else(|| {
                        sessions
                            .iter()
                            .max_by_key(|s| activity_key(s))
                            .map(|s| s.session_id.clone())
                    })
            };
            let Some(session_id) = session_id else {
                // The launcher. No subscription exists, so this is a much
                // smaller select! than the one below — but the rail's
                // watch stream is a 'conn-level thing and keeps flowing,
                // which is what lets the session list stay live behind a
                // launcher that is showing because you pressed `+ new`.
                selected = None;
                cursor = None;
                plumbing.push_feed(FeedEvent::NoSession { error: None });
                loop {
                    tokio::select! {
                        item = watch_rx.recv() => match item {
                            Some(SubscriptionItem::Sessions { sessions }) => {
                                plumbing.push_rail(sessions);
                            }
                            Some(_) => {}
                            None => continue 'conn,
                        },
                        cmd = commands.recv() => match cmd {
                            Some(Command::Launch { provider, model }) => {
                                let agent = AgentConfig {
                                    provider: Some(provider),
                                    model,
                                    ..AgentConfig::default()
                                };
                                match client.open_with_agent(None, None, Some(agent)).await {
                                    Ok(info) => {
                                        at_launcher = false;
                                        selected = Some(info.session_id);
                                        continue 'session;
                                    }
                                    // A refusal here is the engine's
                                    // posture gate doing its job, and the
                                    // connection is fine — state its words
                                    // on the launcher rather than tearing
                                    // the connection down.
                                    Err(ClientError::Engine(msg)) => {
                                        plumbing.push_feed(FeedEvent::NoSession {
                                            error: Some(msg),
                                        });
                                    }
                                    Err(_) => continue 'conn,
                                }
                            }
                            Some(Command::Select(id)) => {
                                at_launcher = false;
                                selected = Some(id);
                                continue 'session;
                            }
                            // Already here.
                            Some(Command::NewSession) => {}
                            // No subscription to re-open here, but the
                            // preference must still be the one the next
                            // session subscribes with — dropping it would
                            // make the rail's label and the engine's
                            // basis disagree the moment a session opens.
                            Some(Command::FilesScope(scope)) => files_scope = scope,
                            Some(Command::Isolate(id)) => {
                                let _ = client.isolate(&id).await;
                            }
                            Some(Command::RemoveWorktree { id, force }) => {
                                let _ = client.remove_worktree(&id, force).await;
                            }
                            // Stage AC. Serviced HERE as well as in the
                            // session loop below, because the rail is on
                            // screen at the launcher too — every row is
                            // visible and none of them is selected. An
                            // arm that existed in only one of these two
                            // loops would be an affordance that silently
                            // does nothing depending on which pane is up.
                            Some(Command::Rename { id, title }) => {
                                let note = match client.rename(&id, title.as_deref()).await {
                                    Ok(_) => None,
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((id, note));
                            }
                            Some(Command::SetArchived { id, archived }) => {
                                let note = match client.set_archived(&id, archived).await {
                                    Ok(_) => None,
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((id, note));
                            }
                            // Everything below needs a selected session
                            // and there is not one: dropped, never queued.
                            // A send that arrives at the launcher has no
                            // session to be addressed to, and holding it
                            // to replay into whatever opens next would be
                            // worse than losing it.
                            //
                            // **Listed rather than caught by `Some(_)`,
                            // deliberately (Stage AC).** The session loop
                            // below matches every variant explicitly, so
                            // the compiler catches a command nobody
                            // services there; this arm used to opt this
                            // loop out of that check, which means a new
                            // rail affordance could silently do nothing
                            // whenever the launcher is up — an affordance
                            // that exists and is never called being the
                            // exact defect this slice was written to
                            // close. Naming them costs six lines and
                            // makes the next one a compile error.
                            Some(Command::Send(_))
                            | Some(Command::Kill)
                            | Some(Command::Resubscribe)
                            | Some(Command::Diff(_))
                            | Some(Command::Decide { .. }) => {}
                            None => return, // UI gone
                        },
                    }
                }
            };
            // The cursor belongs to one session's log: a switch always
            // subscribes cursorless (full replay — the UI holds no
            // projection of the new session).
            if selected.as_deref() != Some(session_id.as_str()) {
                cursor = None;
            }
            selected = Some(session_id.clone());
            plumbing.push_feed(FeedEvent::Session {
                id: session_id.clone(),
            });

            let (sub_id, mut rx) = match client.subscribe_with_id(&session_id, cursor).await {
                Ok(pair) => pair,
                Err(_) => continue 'conn,
            };
            // The selected session's changed-file overview rides its own
            // per-session subscription (spec 05 §Files); a switch drops
            // it with the loop. `None` after open = transport gone.
            let (mut files_id, mut files_rx) =
                match client.files_with_id_scoped(&session_id, files_scope).await {
                    Ok((id, rx)) => (id, Some(rx)),
                    Err(_) => continue 'conn,
                };
            // Why the loop breaks with a reason instead of continuing its
            // labels directly: a switch must first unsubscribe the old
            // streams (Slice I1) — without that the engine keeps streaming
            // the deselected session and holds its FS watcher for the
            // connection's whole life. Connection loss skips the cleanup
            // (the dying connection is the cleanup).
            enum StreamEnd {
                Switch,
                Conn,
                Quit,
            }
            let end = loop {
                tokio::select! {
                    item = async {
                        match files_rx.as_mut() {
                            Some(frx) => frx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => match item {
                        Some(SubscriptionItem::Files { session_id: sid, snapshot }) => {
                            plumbing.push_files((sid, Some(snapshot)));
                        }
                        Some(_) => {} // files carries nothing else
                        None => {
                            // The engine refused the method (it predates
                            // files) or closed the stream alone: state the
                            // fact and stop polling the dead receiver.
                            plumbing.push_files((session_id.clone(), None));
                            files_rx = None;
                        }
                    },
                    item = watch_rx.recv() => match item {
                        Some(SubscriptionItem::Sessions { sessions }) => {
                            plumbing.push_rail(sessions);
                        }
                        Some(_) => {} // watch carries nothing else
                        None => break StreamEnd::Conn,
                    },
                    item = rx.recv() => match item {
                        Some(SubscriptionItem::Reset { session_id: sid, .. }) => {
                            cursor = None;
                            plumbing.push_feed(FeedEvent::Reset { id: sid });
                        }
                        Some(SubscriptionItem::Events { session_id: sid, epoch, records }) => {
                            if let Some(last) = records.last() {
                                cursor = Some(Cursor { epoch, seq: last.seq });
                            }
                            plumbing.push_feed(FeedEvent::Records { id: sid, epoch, records });
                        }
                        Some(SubscriptionItem::Attention { session_id: sid, attention }) => {
                            plumbing.push_attn((sid, attention));
                        }
                        // Never on this id: sessions ride watch, files
                        // ride the files subscription.
                        Some(SubscriptionItem::Sessions { .. })
                        | Some(SubscriptionItem::Files { .. }) => {}
                        None => break StreamEnd::Conn, // connection gone → reconnect
                    },
                    cmd = commands.recv() => match cmd {
                        Some(Command::Send(text)) => {
                            // Delivery truth arrives as journal records
                            // (UserSent/UserDelivered); a transport error means
                            // the connection is gone.
                            if client.send(&session_id, &text).await.is_err() {
                                break StreamEnd::Conn;
                            }
                        }
                        Some(Command::Kill) => {
                            let _ = client.kill(&session_id).await;
                        }
                        Some(Command::Diff(path)) => {
                            // Off-loop: the reply lands on the diffs
                            // queue tagged (session, path); the pump
                            // discards it if the slot moved on.
                            let client = Arc::clone(&client);
                            let sid = session_id.clone();
                            let plumbing = plumbing.clone();
                            // The basis the LIST is showing. Clicking a row
                            // in the "this turn" list and getting a
                            // HEAD-relative diff was the shipped bug: the
                            // right file, the wrong hunks, under a heading
                            // that says otherwise.
                            let scope = files_scope;
                            tokio::spawn(async move {
                                let res = client
                                    .diff_scoped(&sid, &path, scope)
                                    .await
                                    .map_err(|e| e.to_string());
                                plumbing.push_diff((sid, path, res));
                            });
                        }
                        Some(Command::Isolate(sid)) => {
                            // Off-loop like diffs; the watch snapshot
                            // carries success, so only refusals need a
                            // session-tagged note.
                            let client = Arc::clone(&client);
                            let plumbing = plumbing.clone();
                            tokio::spawn(async move {
                                let note = match client.isolate(&sid).await {
                                    Ok(_) => None,
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((sid, note));
                            });
                        }
                        // Stage AC. Off-loop like isolate: the watch
                        // snapshot carries success (the label changes, or
                        // the row leaves the list), so only a refusal
                        // needs a session-tagged note. A rename that did
                        // not stick has to SAY so — the field closes
                        // either way, and a row quietly showing the old
                        // name is the worst reading of that.
                        Some(Command::Rename { id, title }) => {
                            let client = Arc::clone(&client);
                            let plumbing = plumbing.clone();
                            tokio::spawn(async move {
                                let note = match client.rename(&id, title.as_deref()).await {
                                    Ok(_) => None,
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((id, note));
                            });
                        }
                        Some(Command::SetArchived { id, archived }) => {
                            let client = Arc::clone(&client);
                            let plumbing = plumbing.clone();
                            tokio::spawn(async move {
                                let note = match client.set_archived(&id, archived).await {
                                    Ok(_) => None,
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((id, note));
                            });
                        }
                        Some(Command::RemoveWorktree { id, force }) => {
                            let client = Arc::clone(&client);
                            let plumbing = plumbing.clone();
                            tokio::spawn(async move {
                                let note = match client.remove_worktree(&id, force).await {
                                    Ok(ok) if ok.removed => None,
                                    Ok(ok) => Some(RowNote::ConfirmDelete {
                                        dirty_files: ok.dirty_files,
                                        unmerged_commits: ok.unmerged_commits,
                                    }),
                                    Err(e) => Some(RowNote::Error(e.to_string())),
                                };
                                plumbing.push_row_note((id, note));
                            });
                        }
                        Some(Command::Decide { request_id, allow, message }) => {
                            // Off-loop: answering must never block the
                            // stream that carries the next question.
                            let client = Arc::clone(&client);
                            let sid = session_id.clone();
                            tokio::spawn(async move {
                                let decision = if allow {
                                    eave_wire::Decision::Allow
                                } else {
                                    eave_wire::Decision::Deny { message }
                                };
                                let _ = client.decide(&sid, &request_id, decision).await;
                            });
                        }
                        Some(Command::FilesScope(scope)) => {
                            // Handled in place rather than through
                            // StreamEnd::Switch: a basis toggle has
                            // nothing to do with the transcript, and
                            // tearing that subscription down would make
                            // the whole conversation re-replay to answer
                            // a question about files.
                            if scope != files_scope {
                                files_scope = scope;
                                let _ = client.unsubscribe(files_id).await;
                                match client
                                    .files_with_id_scoped(&session_id, files_scope)
                                    .await
                                {
                                    Ok((id, rx)) => {
                                        files_id = id;
                                        files_rx = Some(rx);
                                    }
                                    Err(_) => break StreamEnd::Conn,
                                }
                            }
                        }
                        Some(Command::Resubscribe) => {
                            cursor = None;
                            break StreamEnd::Switch;
                        }
                        Some(Command::Select(id)) => {
                            if selected.as_deref() != Some(id.as_str()) {
                                selected = Some(id);
                                cursor = None;
                                break StreamEnd::Switch;
                            }
                        }
                        // Slice Q2: `+ new` no longer opens anything. It
                        // drops the selection and lands on the launcher,
                        // where the agent gets chosen before a process
                        // exists.
                        Some(Command::NewSession) => {
                            at_launcher = true;
                            cursor = None;
                            break StreamEnd::Switch;
                        }
                        // DROPPED — and Slice AI checked, because this arm
                        // is where `EAVE_LAUNCH_AT_BOOT` went to die.
                        //
                        // **Do not service it here.** The launcher is on
                        // screen only while nothing is selected, so the
                        // only `Launch` that can reach this arm is the
                        // SECOND press of the start button: the first one
                        // opened a session and brought the loop back here,
                        // and the second is still queued behind it.
                        // Opening on it would make a double-click on
                        // `Start` two sessions.
                        //
                        // What was broken was the other direction — a
                        // caller that wants a session while one is selected
                        // has to ask for the launcher first, and the boot
                        // hook did not. It does now
                        // (`Shell::launch_at_boot`); `+ new` is the way in,
                        // and it is the arm directly above.
                        Some(Command::Launch { .. }) => {}
                        None => break StreamEnd::Quit, // UI gone
                    },
                }
            };
            match end {
                StreamEnd::Switch => {
                    // Release the deselected session's streams engine-side
                    // (Slice I1): stops the old subscription's socket
                    // traffic and drops the files stream's FS-watcher
                    // refcount — "subscriber-scoped watcher" means the
                    // SELECTED session's, not every session ever visited.
                    // Errors are ignored: an older engine refuses the
                    // method (connection close still cleans up), a dead
                    // connection is discovered by the next subscribe.
                    let _ = client.unsubscribe(sub_id).await;
                    let _ = client.unsubscribe(files_id).await;
                    continue 'session;
                }
                StreamEnd::Conn => continue 'conn,
                StreamEnd::Quit => return,
            }
        }
    }
}

/// Replay feed: drive the UI from a captured wire fixture instead of a
/// live engine (`EAVE_REPLAY=<path>`, `EAVE_REPLAY_SPEED=<factor>`). The
/// Stage D acceptance runs against real pacing and real markdown.
fn replay_loop(path: PathBuf, speed: f64, plumbing: Plumbing) {
    let steps = match crate::fixture::load(&path) {
        Ok(s) if !s.is_empty() => s,
        _ => {
            plumbing.push_conn(ConnEvent::Failed {
                message: format!("fixture {} unreadable or empty", path.display()),
            });
            return;
        }
    };
    plumbing.push_conn(ConnEvent::Connected { engine_pid: 0 });
    let sid = format!(
        "replay:{}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    plumbing.push_feed(FeedEvent::Session { id: sid.clone() });
    // A synthetic rail snapshot so harness runs (replay assert, captures)
    // exercise the real constrained-width layout with the rail present.
    plumbing.push_rail(vec![SessionInfo {
        session_id: sid.clone(),
        // Unnamed on purpose: the replay rig should measure the row shape
        // an ordinary session actually has, which is the fallback label.
        title: None,
        archived: false,
        provider: "replay".into(),
        model: None,
        cwd: path.display().to_string(),
        created_ts_ms: 0,
        last_activity_ms: None,
        state: "running".into(),
        live: true,
        epoch: 0,
        last_seq: 0,
        checkout_root: None,
        shared_with: Vec::new(),
        worktree: None,
        pending_approvals: 0,
        permission_mode: Some("default".into()),
        // A real `claude` sits in this range; the replay rig is the only
        // place the aggregate line renders through a real compositor, so
        // it renders with a realistic number rather than none.
        rss_bytes: Some(268_435_456),
    }]);
    // Same for the files rail (Slice B): the reading column must be
    // constrained on BOTH sides during asserts and captures.
    plumbing.push_files((sid.clone(), Some(synthetic_files_snapshot())));
    // `EAVE_REPLAY_LOOP=n` replays the capture n times back to back with
    // seq/ts rebased — a 9-minute fixture ×4 is the "30+ min of frames"
    // acceptance stream (each pass appends like a resumed segment).
    let loops: u64 = std::env::var("EAVE_REPLAY_LOOP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let t0 = steps[0].ts_ms;
    let span = steps.last().unwrap().ts_ms - t0;
    let max_seq = steps
        .iter()
        .filter_map(|s| match &s.item {
            SubscriptionItem::Events { records, .. } => records.last().map(|r| r.seq),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let started = Instant::now();
    for iter in 0..loops {
        for step in &steps {
            let ts = step.ts_ms - t0 + iter * (span + 1000);
            let due = Duration::from_millis((ts as f64 / speed) as u64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
            // Frames are re-tagged with the replay session id: the pump
            // discards frames whose session doesn't match the current one,
            // and a fixture's captured ids are not this session.
            match &step.item {
                SubscriptionItem::Reset { .. } => {
                    if iter == 0 {
                        plumbing.push_feed(FeedEvent::Reset { id: sid.clone() });
                    }
                }
                SubscriptionItem::Events { epoch, records, .. } => {
                    let records = records
                        .iter()
                        .map(|r| {
                            let mut r = r.clone();
                            r.seq += iter * max_seq;
                            r
                        })
                        .collect();
                    plumbing.push_feed(FeedEvent::Records {
                        id: sid.clone(),
                        epoch: *epoch,
                        records,
                    });
                }
                SubscriptionItem::Attention { attention, .. } => {
                    plumbing.push_attn((sid.clone(), attention.clone()));
                }
                SubscriptionItem::Sessions { .. } | SubscriptionItem::Files { .. } => {}
            }
        }
    }
}

/// The harness's files snapshot (replay + perf): plausible rows covering
/// every status silhouette, so asserts and captures exercise the rail
/// the shell actually ships.
fn synthetic_files_snapshot() -> FilesSnapshot {
    FilesSnapshot {
        state: GitState::Ok,
        files: vec![
            ChangedFile {
                path: "crates/eave-app/src/ui.rs".into(),
                status: FileStatus::Modified,
                adds: Some(124),
                dels: Some(38),
                orig_path: None,
                untracked: false,
            },
            ChangedFile {
                path: "crates/eave-engine/src/git.rs".into(),
                status: FileStatus::Added,
                adds: Some(310),
                dels: Some(0),
                orig_path: None,
                untracked: true,
            },
            ChangedFile {
                path: "docs/notes/scratch.md".into(),
                status: FileStatus::Deleted,
                adds: Some(0),
                dels: Some(57),
                orig_path: None,
                untracked: false,
            },
            ChangedFile {
                path: "assets/icon.png".into(),
                status: FileStatus::Modified,
                adds: None,
                dels: None,
                orig_path: None,
                untracked: false,
            },
        ],
        omitted: 0,
        // The harness plays a live-looking rail; the mode line should
        // state the live mode too.
        watching: true,
        basis: DiffBasis::WorkingTree,
    }
}

/// A [`git::DIFF_MAX_LINES`]-scale text diff for the perf harness: the
/// long-diff render path is new in B2 and must be measured at its cap,
/// not at a friendly size. Runs alternate ctx/del/add so the wash
/// grouping is exercised, not short-circuited.
fn synthetic_long_diff() -> FileDiff {
    let hunks = (0..40)
        .map(|h| DiffHunk {
            header: format!(
                "@@ -{},50 +{},50 @@ fn section_{h}()",
                h * 60 + 1,
                h * 60 + 1
            ),
            lines: (0..50)
                .map(|i| DiffLine {
                    op: match i % 5 {
                        0 | 1 => DiffOp::Ctx,
                        2 => DiffOp::Del,
                        _ => DiffOp::Add,
                    },
                    text: format!("    let value_{h}_{i} = compute(input[{i}], flags | MASK_{h});"),
                })
                .collect(),
        })
        .collect();
    FileDiff {
        path: "crates/eave-app/src/ui.rs".into(),
        kind: DiffKind::Text {
            hunks,
            truncated: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Entities (per-concern, spec 03 §View tree)

struct ConnectionState {
    status: ConnEvent,
}

/// The session rail's state (spec 03 §View tree: `SessionsState`), all of
/// it ephemeral projection: the last watch snapshot (rebuilt from the
/// engine on every reconnect — nothing persisted app-side), the transient
/// attention overlay for the *selected* session (tagged with its session
/// id so a stale notice can't outlive a switch), and the rail's collapsed
/// flag (pure view preference). The selected session itself is NOT held
/// here — it is `TranscriptState::session_id`, i.e. whatever the feed
/// task actually subscribed; the rail highlights that fact.
struct SessionsState {
    /// The rail's rows, **in the order they are drawn** (Stage AB). Facts
    /// still fold by replacement from every watch snapshot; only the order
    /// is the app's own — see [`fold_rail`].
    sessions: Vec<SessionInfo>,
    collapsed: bool,
    /// Whether the rail is currently drawing archived rows (Stage AC).
    ///
    /// A pure view preference, like `collapsed`, and deliberately NOT
    /// persisted: it is a "let me look at the filing cabinet" mode, not a
    /// setting, and an eave that came back tomorrow still showing every
    /// archived session would have made the archive bit pointless.
    show_archived: bool,
    attention: Option<(String, AttentionSnapshot)>,
    /// Standing worktree-action notes per session (Slice I2): a stated
    /// refusal, or an armed delete confirmation restating the engine's
    /// fresh counts. Cleared by a settled action; folded by replacement.
    row_notes: std::collections::HashMap<String, RowNote>,
}

impl SessionsState {
    fn notice_standing(&self) -> bool {
        matches!(&self.attention, Some((_, a)) if !matches!(a, AttentionSnapshot::Running))
    }
}

/// The files rail's state (Slice B, spec 05 §Files) — pure projection of
/// the engine's git polling, session-tagged so a snapshot can never
/// outlive a switch. `snapshot` holds the LAST files event: `None` =
/// nothing received yet; `Some((sid, None))` = the engine refused the
/// subscription (stated fact); `Some((sid, Some(snap)))` = data.
struct FilesState {
    snapshot: Option<FilesEvent>,
    collapsed: bool,
    /// Which basis the rail is asking for (Slice AA). This is the
    /// REQUEST, not the answer — what actually arrived is stated by the
    /// snapshot's own `basis`, and the two disagree for the moment
    /// between a toggle and its first frame. The toggle clears
    /// `snapshot`, so that moment renders as the skeleton rather than as
    /// the other basis's list wearing this basis's label.
    scope: FileScope,
    /// The one expanded diff (B2), session-tagged like everything else
    /// in this rail: a switch clears it, a snapshot that no longer lists
    /// its file closes it, and a fresh snapshot re-requests it (the
    /// files channel is the invalidation signal — the diff itself is
    /// never cached engine-side).
    diff: Option<DiffSlot>,
}

/// How long the files rail waits for an answer before it states that
/// nothing has given one (Slice AH — the eternal skeleton).
///
/// **Derived from the engine's own ceiling, not chosen.** `refresh_files`
/// shells out to git and every call is bounded by
/// [`eave_engine::git::GIT_TIMEOUT`], so a healthy engine whose first git
/// call times out legitimately owes its first frame that long. Two of
/// them covers one fully timed-out call PLUS the whole rest of a normal
/// scan — measured at 0.04–0.07 s wall on this repo (N4 §2.3) — which is
/// three orders of magnitude of slack on the part that is not the
/// timeout. The multiplier lives here rather than in a comment so that
/// raising the engine's timeout cannot silently make the rail impatient.
///
/// **The ceiling is what being wrong costs.** While the rail waits it
/// holds a pulse lease, and a lease at 30 fps is +8.7 points of a core
/// (measured in N2, quoted in N3 §3). Before this slice that cost ran for
/// the life of the process; this bounds it.
///
/// It bounds the ANIMATION and the WORDS, never the subscription. Nothing
/// is unsubscribed, nothing is killed, no session is touched (progress.md
/// §Known failures: no timer ever kills), and a snapshot arriving at 21 s
/// renders exactly as one arriving at 2 s does.
const FILES_PATIENCE: Duration = Duration::from_secs(2 * eave_engine::git::GIT_TIMEOUT.as_secs());

/// What the files rail knows about the selected session's file list, on
/// this frame (Slice AH).
///
/// The gate and the render read the SAME answer — [`Shell::files_wait`] —
/// because the defect this closes was exactly the two disagreeing: the
/// render drew "a list is coming" forever while the gate believed it,
/// forever, and paid 30 fps for the belief.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilesWait {
    /// Nothing to wait for: an answer for the selected session is in hand,
    /// or nothing is selected.
    Answered,
    /// Waiting, and still inside [`FILES_PATIENCE`]. The skeleton is the
    /// honest picture here — a list really is on its way.
    Waiting,
    /// The wait is over without an answer, and these are the words for
    /// why. An eternal skeleton is an error state that lost its error;
    /// this is the error, found again.
    Silent(String),
}

/// The connection facts that mean no file list is coming, in the
/// connection's own words (Slice AH).
///
/// Read rather than timed, and preferred over [`FILES_PATIENCE`] wherever
/// it answers: a timeout invents a fact where this states one. `None`
/// means the connection is live or still coming up, so the only evidence
/// available is elapsed time.
///
/// `Reconnecting` is deliberately NOT here. It is bounded — five attempts
/// with a doubling 250 ms backoff, ~7.75 s in total — and it ends in
/// `Failed`, which is. Calling a 250 ms blip "no file list" would be the
/// opposite defect: an error state that never had an error.
fn conn_silence(status: &ConnEvent) -> Option<String> {
    match status {
        ConnEvent::Failed { message } => Some(format!("no file list — {message}")),
        ConnEvent::Busy { holder_pid } => Some(match holder_pid {
            Some(pid) => format!("no file list — another eave (pid {pid}) holds the engine"),
            None => "no file list — another eave holds the engine".into(),
        }),
        ConnEvent::Connecting | ConnEvent::Connected { .. } | ConnEvent::Reconnecting { .. } => {
            None
        }
    }
}

struct DiffSlot {
    session: String,
    path: String,
    fetch: DiffFetch,
}

#[derive(Clone)]
enum DiffFetch {
    Loading,
    /// Arc: the pane clones out of the entity every render; a 2000-line
    /// diff must not be a per-frame deep copy.
    Ready(Arc<FileDiff>),
    Failed(String),
}

/// The glyph for a file's git status (Slice P3).
///
/// This mapping replaces `FileStatus::glyph()`'s one-letter vocabulary at
/// the render site only — the wire type keeps its letters, because they
/// are git's own names for these states and the engine has no business
/// knowing what the shell draws.
///
/// **The set is deliberately primitive.** These render into a 12 px box:
/// anything with interior detail (a page outline, a pencil) is mush at
/// that size, so each status gets the simplest mark that cannot be
/// confused with the others at a glance — and `Modified` gets a RING
/// rather than a disc because eave already spends filled discs on session
/// and tool state.
fn status_icon(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => icons::STATUS_ADDED,
        FileStatus::Modified => icons::STATUS_MODIFIED,
        FileStatus::Deleted => icons::STATUS_DELETED,
        FileStatus::Renamed => icons::STATUS_RENAMED,
        FileStatus::Conflicted => icons::STATUS_CONFLICTED,
    }
}

/// Right-aligned count labels for one changed file; `None` when the
/// counts are unknowable (binary / oversized) — rendered as one ghost
/// dash instead of fake zeros.
fn count_labels(adds: Option<u64>, dels: Option<u64>) -> Option<(String, String)> {
    match (adds, dels) {
        (None, None) => None,
        (a, d) => Some((
            format!("+{}", a.unwrap_or(0)),
            format!("−{}", d.unwrap_or(0)),
        )),
    }
}

/// The inline diff pane (B2), rendered under its expanded row. Every
/// shape is a stated fact — binary, symlink, submodule, rename, error,
/// truncation all render as words; only add/remove lines get pigment
/// (the washes), because add/remove IS state and nothing else here is.
fn diff_pane(f: &ChangedFile, fetch: &DiffFetch, phase: Option<f32>) -> AnyElement {
    let fact = |text: String| {
        div()
            .text_size(px(theme::DETAIL_SIZE))
            .line_height(px(theme::DETAIL_LINE))
            .text_color(theme::text_tertiary())
            .child(SharedString::from(text))
            .into_any_element()
    };
    let mut children: Vec<AnyElement> = Vec::new();
    if let Some(orig) = &f.orig_path {
        children.push(fact(format!("renamed from {orig}")));
    }
    match fetch {
        // Slice N3: the diff is on its way. Skeleton lines in the pane's
        // own line rhythm, so the arriving hunks land where the skeleton
        // already was.
        DiffFetch::Loading => children.push(
            loaders::skeleton(theme::SKELETON_ROWS, phase)
                .debug_selector(|| "diff-skeleton".into())
                .into_any_element(),
        ),
        DiffFetch::Failed(message) => children.push(fact(format!("diff unavailable: {message}"))),
        DiffFetch::Ready(diff) => match &diff.kind {
            DiffKind::Binary => children.push(fact("binary file — no text diff".into())),
            DiffKind::Symlink { target } => children.push(fact(format!("symlink → {target}"))),
            DiffKind::Submodule { detail } => children.push(fact(format!("submodule {detail}"))),
            // The engine stated this sentence in full (`GitState::stated`).
            // The `git failed: ` that used to be prepended here was the
            // second frame in the pair that produced
            // `git failed: git status timed out after 10s`.
            DiffKind::Error { message } => children.push(fact(message.clone())),
            DiffKind::Text { hunks, truncated } => {
                if hunks.is_empty() && !*truncated {
                    children.push(fact("no line changes".into()));
                }
                // Render cap: the pane sits in a non-virtualized scroll
                // container, so every line here is laid out every frame
                // — past the cap the remainder is a stated count. (The
                // engine's own 2000-line cap still bounds the data;
                // `truncated` states that separately.)
                let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
                // The cap is stated IN the pane, up front (directive
                // 2026-08-17) — a reader mid-pane must know they are
                // looking at a head, not the diff.
                if total > theme::DIFF_PANE_MAX_LINES {
                    children.push(fact(format!(
                        "showing the first {} of {} lines",
                        theme::DIFF_PANE_MAX_LINES,
                        total
                    )));
                }
                let mut budget = theme::DIFF_PANE_MAX_LINES;
                'hunks: for hunk in hunks {
                    children.push(
                        div()
                            .font_family(theme::MONO)
                            .text_size(px(theme::MICRO_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .truncate()
                            .child(SharedString::from(hunk.header.clone()))
                            .into_any_element(),
                    );
                    // Consecutive same-op lines render as ONE element (a
                    // multi-line text under one wash): element count
                    // scales with runs, not lines.
                    for (op, text, count) in diff_runs(&hunk.lines, budget) {
                        budget -= count;
                        let styled = div()
                            .px(px(theme::SPACE_1))
                            .font_family(theme::MONO)
                            .text_size(px(theme::DIFF_SIZE))
                            .line_height(px(theme::DIFF_LINE))
                            .child(SharedString::from(text));
                        children.push(
                            match op {
                                DiffOp::Add => styled
                                    .bg(theme::diff_add_bg())
                                    .text_color(theme::text_secondary()),
                                DiffOp::Del => styled
                                    .bg(theme::diff_del_bg())
                                    .text_color(theme::text_secondary()),
                                DiffOp::Ctx => styled.text_color(theme::text_tertiary()),
                            }
                            .into_any_element(),
                        );
                        if budget == 0 {
                            break 'hunks;
                        }
                    }
                }
                let shown = total.min(theme::DIFF_PANE_MAX_LINES);
                if shown < total {
                    children.push(fact(format!(
                        "… {} more lines — ⌘-click opens the file",
                        total - shown
                    )));
                }
                if *truncated {
                    children.push(fact(
                        "diff truncated by the engine — the full diff stays in git".into(),
                    ));
                }
            }
        },
    }
    // Indented under its row, scoped by a hairline edge (structure, not
    // decoration); no plate — the diff sits on the same bare frost as
    // the rows around it.
    div()
        .ml(px(theme::FILE_STATUS_W))
        .pl(px(theme::SPACE_2))
        .py(px(theme::SPACE_1))
        .border_l_1()
        .border_color(theme::border())
        .flex()
        .flex_col()
        .children(children)
        .into_any_element()
}

/// Group consecutive same-op diff lines into one multi-line string per
/// run, taking at most `budget` lines (the render cap): element count
/// scales with runs, line count is bounded, and each run reports how
/// many lines it holds so the caller can spend the budget.
fn diff_runs(lines: &[DiffLine], budget: usize) -> Vec<(DiffOp, String, usize)> {
    let mut runs: Vec<(DiffOp, String, usize)> = Vec::new();
    for l in lines.iter().take(budget) {
        match runs.last_mut() {
            Some((op, buf, count)) if *op == l.op => {
                buf.push('\n');
                buf.push_str(&l.text);
                *count += 1;
            }
            _ => runs.push((l.op, l.text.clone(), 1)),
        }
    }
    runs
}

/// Quiet escalates from a subtle line to a prominent bar at 10 min
/// (build directive 2026-08-16: "subtle at threshold, prominent at 10min,
/// then steady elapsed reporting").
const PROMINENT_AFTER_MS: u64 = 10 * 60 * 1000;

/// Self-drawn provider identity (Slice A directive): one uniform,
/// monochrome initials chip + a plain text label, generated from the
/// provider string — deliberately never a third-party mark, not even
/// where a provider publishes assets. `claude-code` → ("CC", "claude
/// code"); a one-word provider takes its first two letters.
fn provider_identity(provider: &str) -> (String, String) {
    let segs: Vec<&str> = provider
        .split(['-', '_', ' '])
        .filter(|s| !s.is_empty())
        .collect();
    let glyph: String = match segs.as_slice() {
        [] => "?".to_string(),
        [one] => one.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    let label = if segs.is_empty() {
        provider.to_string()
    } else {
        segs.join(" ")
    };
    (glyph.to_uppercase(), label)
}

/// State pigment per the stance: color only when state deserves attention.
/// `running` is live activity; quiet/unresponsive/failed are the
/// engine-classified notices; every settled or not-yet-started state is
/// neutral.
fn state_color(state: &str) -> Hsla {
    match state {
        "running" => theme::live(),
        // Slice K: the session is parked on the operator. Attention-worthy
        // — it is the one state where the thing being waited on is you —
        // but not a fault, so warning and never danger.
        "quiet" | "awaiting_approval" => theme::warning(),
        "unresponsive" | "failed" => theme::danger(),
        _ => theme::text_tertiary(),
    }
}

/// Attention badge: only the states the operator may need to act on get a
/// word in the rail; everything else is just the neutral dot. The word is
/// the state name except for `awaiting_approval`, which says what it means
/// for the person reading it (Slice K) — a background session parked on a
/// question has to read as "you are the blocker" without being opened.
fn state_badge(state: &str) -> Option<&'static str> {
    match state {
        "quiet" => Some("quiet"),
        "unresponsive" => Some("unresponsive"),
        "failed" => Some("failed"),
        "awaiting_approval" => Some("waiting on you"),
        _ => None,
    }
}

/// Take the operator's deny reason out of the composer and clear it
/// (Slice K). eave has exactly one text field, so the composer IS the
/// reason field — the card says so in as many words. Clearing it is the
/// point: the words were spent on the agent, and leaving them behind
/// would let the next Enter send the same sentence again as a message.
fn take_deny_reason(composer: Option<&Entity<ComposerState>>, cx: &mut App) -> String {
    let Some(composer) = composer else {
        return String::new();
    };
    let text = composer.read(cx).text.trim().to_string();
    if !text.is_empty() {
        composer.update(cx, |c, cx| {
            c.text.clear();
            cx.notify();
        });
    }
    text
}

/// The permission posture as a rail fact (Slice K amendment 3). A session
/// running in a mode that never asks must not look like one that does —
/// this is the line that makes the difference visible without opening it.
fn posture_line(mode: &str) -> String {
    match mode {
        "default" | "manual" => format!("asks before it writes · {mode}"),
        other => format!("NEVER ASKS · {other}"),
    }
}

/// Above this share of the machine's physical memory held by agent
/// processes, the aggregate fact turns from a statement into a warning
/// (Stage L). Not a limit and not enforced — eave still starts whatever
/// the operator starts. Half was chosen because that is roughly where a
/// working Mac starts trading the rest of its memory away, and the number
/// on screen is the operator's evidence either way.
const AGENT_MEMORY_WARN_FRACTION: f64 = 0.5;

/// The aggregate resource fact (Stage L): what every live agent costs
/// right now, together, against the machine it runs on.
///
/// This exists because per-session honesty was not enough. eave states a
/// failure per session accurately, but at N=10 (measured, Slice I1) the
/// ceiling is not eave — engine ~8 MB — it is N × a real `claude` at
/// 200–310 MB, and nothing anywhere said so; the machine simply started
/// paging. One line, always the sum of what is actually resident.
///
/// Returns `None` when nothing live reports memory: no live session, an
/// engine predating `rss_bytes`, or a probe that failed. Absence is
/// stated by rendering nothing — a "0.0 GB" would be a lie about a
/// running agent.
///
/// Both limits of the underlying number are real and are named where a
/// reader can act on them (`eave_core::proc::resident_bytes`): leaders
/// only, so tool children are uncounted, and resident sizes double-count
/// shared pages, so the sum is an upper bound. Provider rate contention —
/// the other thing that bites at high N — is not measurable here at all.
fn resource_line(
    sessions: &[eave_wire::SessionInfo],
    physical: Option<u64>,
) -> Option<(String, bool)> {
    let reporting: Vec<u64> = sessions
        .iter()
        .filter(|s| s.live)
        .filter_map(|s| s.rss_bytes)
        .collect();
    if reporting.is_empty() {
        return None;
    }
    let total: u64 = reporting.iter().sum();
    let n = reporting.len();
    let noun = if n == 1 { "agent" } else { "agents" };
    match physical {
        Some(phys) if phys > 0 => {
            let frac = total as f64 / phys as f64;
            Some((
                format!("{n} {noun} · {} of {}", gib(total), gib(phys)),
                frac >= AGENT_MEMORY_WARN_FRACTION,
            ))
        }
        // No denominator rather than an invented one: the sum alone is
        // still a true fact, and a fabricated machine size would not be.
        _ => Some((format!("{n} {noun} · {} resident", gib(total)), false)),
    }
}

/// The machine's physical memory, read once. It does not change while
/// the app runs, and the fact renders on every rail paint.
fn physical_memory_cached() -> Option<u64> {
    static PHYS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *PHYS.get_or_init(eave_core::proc::physical_memory)
}

/// Bytes as gibibytes at one decimal — the unit the operator's own
/// Activity Monitor uses, so the two numbers can be compared directly.
fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// The shared-checkout fact (Slice I2): who else works in these files,
/// by 8-char session prefix — identity, never content.
fn shares_line(shared_with: &[String]) -> String {
    let names: Vec<String> = shared_with
        .iter()
        .map(|id| id.chars().take(8).collect())
        .collect();
    format!("shares files with {}", names.join(", "))
}

/// The worktree fact line (Slice I2). Every shape is a stated fact:
/// clean, a dirty count, an honest unknown (`dirty —`, a failed scan),
/// or the directory being gone. The branch is deliberately NOT repeated
/// here — it is the predictable `eave/<sid8>` and the cwd line above
/// already ends in `<sid8>`; at rail width the dirty count must never
/// lose to truncation (it is the fact decision 2 protects).
fn worktree_line(w: &eave_wire::WorktreeInfo) -> String {
    if !w.present {
        return "worktree missing".to_string();
    }
    match w.dirty_files {
        Some(0) => "worktree · clean".to_string(),
        Some(n) => format!("worktree · {n} dirty"),
        None => "worktree · dirty —".to_string(),
    }
}

/// Where a row sorts: when this session's state last moved, or when it was
/// created if it never has (Stage AB).
fn activity_key(s: &SessionInfo) -> u64 {
    s.last_activity_ms.unwrap_or(s.created_ts_ms)
}

/// Does the rail draw this row? (Stage AC.)
///
/// Archiving hides a session from the DEFAULT listing and does nothing
/// else — the session is still there, still on `watch`, still one bit
/// away from coming back. The filter lives here, in the client, because
/// an engine that omitted archived sessions from its own listing would
/// leave the bit unreachable from the surface that set it.
///
/// The two exceptions are the whole design, and both exist to stop the
/// filter from swallowing something:
///
/// 1. **The selected session always has a row.** Archive the session you
///    are reading and the transcript would otherwise be sitting there
///    with no row to unarchive, no row to switch away from, and nothing
///    in the rail admitting it exists.
/// 2. **A session parked on a permission question always has a row.**
///    eave never auto-approves, never auto-denies and never times out
///    into either (Slice K), so the ONLY way out of `awaiting_approval`
///    is somebody clicking. Hiding that row would strand the agent for
///    the life of the process behind an affordance nobody can reach. The
///    row states why it is there — see [`archived_line`].
///
///    Tested on the COUNT **or** the state name, not on either alone.
///    They come from two different structures engine-side — the count
///    from `SessionHandle::approvals`, the name from the attention
///    monitor's own pending list — and this predicate has an asymmetric
///    cost: showing a row that did not need showing is a cosmetic miss,
///    hiding one that did strands an agent forever. The disjunction takes
///    the cheap error.
///
/// Note what is deliberately NOT an exception: `running`, `quiet`,
/// `failed`, `unresponsive`. None of them is blocked on the operator, and
/// exempting them would mean a session could never be filed away for the
/// exact reason people file sessions away — it finished, or it failed.
fn rail_shows(info: &SessionInfo, selected: Option<&str>, show_archived: bool) -> bool {
    !info.archived
        || show_archived
        || selected == Some(info.session_id.as_str())
        || info.pending_approvals > 0
        || info.state == "awaiting_approval"
}

/// The word an archived row wears (Stage AC), or `None` for a row that is
/// not archived.
///
/// An archived row is only ever on screen for a reason, and when that
/// reason is "it is asking you something" the row says so. Otherwise a
/// session the operator filed away would reappear in a list they believe
/// is filtered, with nothing on it explaining why — which reads as the
/// filter being broken rather than as the filter doing its one job.
fn archived_line(info: &SessionInfo, show_archived: bool) -> Option<String> {
    if !info.archived {
        return None;
    }
    // The same disjunction [`rail_shows`] uses, and it has to be the same
    // one: a row let through by the state name and explained by neither
    // is a row with no account of why it is there.
    let asking = info.pending_approvals > 0 || info.state == "awaiting_approval";
    Some(if !show_archived && asking {
        "archived · still asking".to_string()
    } else {
        "archived".to_string()
    })
}

/// One affordance on a rail row: a hit target, an icon, and nothing else.
///
/// The size is stated here rather than in `theme` on purpose, and it is the
/// one thing in this slice I would move later: `theme.rs` is being reworked
/// by a concurrent slice and a new constant there would have been a
/// guaranteed conflict for a value only this call site reads.
///
/// 24 px, not the 16 px of the glyph. A 16 px hit target is below every
/// platform's pointer guidance and these sit inside a 224 px row where a
/// miss means selecting the session instead of archiving it — the two
/// outcomes are not close enough for a tight target to be acceptable.
const ROW_ACTION_HIT: f32 = 24.0;

/// The glyph inside that target. Smaller than `ICON_SM`, and the gap
/// between the two numbers is the point: these sit in the row's corner
/// beside text, where a 16 px mark reads as a button and a 13 px one reads
/// as a hint. The TARGET does not shrink with it — a 13 px hit box would be
/// well under every platform's pointer guidance, and a miss here selects
/// the session instead of acting on it.
const ROW_ACTION_GLYPH: f32 = 13.0;

fn row_action(key: String, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    let selector = key.clone();
    let glyph_selector = format!("{key}-glyph");
    div()
        .id(SharedString::from(key.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .w(px(ROW_ACTION_HIT))
        .h(px(ROW_ACTION_HIT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_BUTTON))
        // Its own wash, keyed to itself: the strip is already riding the
        // ROW's hover for opacity, and a control that cannot say which of
        // five is under the pointer is worse than five labelled boxes.
        .hover_fade(key, theme::wash_rest(), theme::overlay())
        .cursor_pointer()
        .child(icons::tagged_icon(
            glyph,
            ROW_ACTION_GLYPH,
            theme::text_secondary(),
            glyph_selector,
        ))
}

/// The rail's archived toggle, stated as a fact plus the action (Stage AC).
///
/// The count is of archived sessions, not of hidden rows, and the wording
/// is a state rather than an instruction because those two numbers differ:
/// a selected or still-asking archived row is on screen while the bit is
/// off. "3 archived · show" is true either way; "show 3 archived" would
/// promise to reveal three rows and reveal two.
fn archived_toggle_label(archived: usize, showing: bool) -> String {
    let action = if showing { "hide" } else { "show" };
    format!("{archived} archived · {action}")
}

/// What the rail says when it holds rows and draws none of them
/// (Stage AC).
///
/// "no sessions" over a rail whose sessions are all filed away is a lie
/// of exactly the shape this codebase keeps finding: a state that lost
/// the reason it is in. The count is the reason, and the toggle that
/// undoes it is the line directly above.
fn rail_empty_note(total: usize, archived: usize) -> String {
    if total == 0 {
        "no sessions".to_string()
    } else if archived >= total {
        format!("every session is archived ({archived})")
    } else {
        // Not reachable through the rail's own filter today — the only
        // thing that hides a row is the archive bit — but stated rather
        // than unwrapped, because an empty list with no reason on it is
        // the failure this whole function exists to prevent.
        format!("nothing to show — {archived} of {total} archived")
    }
}

/// Fold a watch snapshot into the rail's rows, keeping the ORDER of rows
/// that were already there (Stage AB).
///
/// Facts still fold by replacement — every surviving row takes the
/// snapshot's `SessionInfo` wholesale, so nothing is merged and nothing
/// goes stale. What is preserved is position, which is not an engine fact
/// at all: `watch` sends a set, and where a row sits in that set is the
/// app's decision.
///
/// **Activity decides position exactly once, when a row arrives.** After
/// that the row stays put for as long as the session exists. Re-sorting
/// live is the bug comet shipped: it ordered the rail by attention, so
/// merely opening a session changed that session's state, which re-sorted
/// the rail, which slid the row out from under the pointer that had just
/// clicked it. A list you cannot click is worse than a list in a stale
/// order — and the order is not even stale, because the thing an operator
/// tracks a session by is where they last saw it.
fn fold_rail(rows: &mut Vec<SessionInfo>, snapshot: Vec<SessionInfo>) {
    let mut incoming: std::collections::HashMap<String, SessionInfo> = snapshot
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();
    // Surviving rows, in the order they already had, refreshed. A row
    // whose session is gone from the snapshot is simply dropped.
    let kept: Vec<SessionInfo> = rows
        .drain(..)
        .filter_map(|old| incoming.remove(&old.session_id))
        .collect();
    // Everything still in `incoming` has no row yet.
    let mut arrivals: Vec<SessionInfo> = incoming.into_values().collect();
    arrivals.sort_by(|a, b| {
        activity_key(b)
            .cmp(&activity_key(a))
            // Two sessions opened in the same millisecond must still land
            // in a defined order, or the rail flickers between snapshots.
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    arrivals.extend(kept);
    *rows = arrivals;
}

/// The row's primary label: the operator's title when there is one, the
/// provider label otherwise (Stage AB).
///
/// A fallback rather than a second line, because a session nobody has named
/// must render no title at all — an empty line with a title's weight is a
/// blank the operator reads as a bug. The provider identity is not lost
/// when a title takes the slot: the chip beside it is drawn from the same
/// `provider_identity` call.
fn row_label(info: &SessionInfo, provider_label: &str) -> String {
    info.title
        .clone()
        .unwrap_or_else(|| provider_label.to_string())
}

/// The rail's meta line: which session, and where it works (Stage AB).
///
/// The 8-char id is here because it is the ONLY thing that tells two
/// untitled sessions in one repo apart without an operator naming them —
/// and because it is pure identity, the one class of fact `watch` has
/// always been allowed to carry. It is dropped when the cwd already ends
/// in it, which is every isolated session (`isolate` puts the sid8 at the
/// end of the worktree path), where repeating it would say the same thing
/// twice on one line.
fn rail_meta_line(info: &SessionInfo, home: Option<&str>) -> String {
    let cwd = short_cwd(&info.cwd, home);
    let sid8: String = info.session_id.chars().take(8).collect();
    if sid8.is_empty() || cwd.ends_with(&sid8) {
        return cwd;
    }
    format!("{sid8} · {cwd}")
}

/// Home-relative cwd for the rail's meta line.
fn short_cwd(cwd: &str, home: Option<&str>) -> String {
    match home {
        Some(h) if !h.is_empty() && cwd.starts_with(h) => format!("~{}", &cwd[h.len()..]),
        _ => cwd.to_string(),
    }
}

/// Bound one block of tool payload for display, stating the remainder.
///
/// Same rule as [`fence_view`] and the diff pane: a row inside a
/// virtualized list may not have a height the agent chooses.
fn tool_body_view(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_string();
    }
    let mut out = text.lines().take(max).collect::<Vec<_>>().join("\n");
    out.push_str(&format!("\n… {} more lines", total - max));
    out
}

/// What a fence DRAWS, which is not always what it holds.
///
/// A fence is one row in a virtualized list, so its height is a promise the
/// list makes. An agent that emits a 4,000-line file would otherwise make
/// one row 80,000 px tall. Past [`theme::FENCE_MAX_LINES`] the view stops
/// and states the remainder — and states that copy is unaffected, because
/// the honest failure mode of a cap is someone believing it truncated their
/// data.
fn fence_view(code: &str) -> String {
    let total = code.lines().count();
    if total <= theme::FENCE_MAX_LINES {
        return code.to_string();
    }
    let mut out = code
        .lines()
        .take(theme::FENCE_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(&format!(
        "\n… {} more lines — copy takes all {total}",
        total - theme::FENCE_MAX_LINES
    ));
    out
}

/// Open a path with the system handler (macOS `open`). Both call sites —
/// the worktree "open" affordance and the diff pane's ⌘-click — route
/// through here so the Stage J harness can assert the *intent* without
/// launching Finder on the machine running the tests.
fn open_in_system(path: &std::path::Path) {
    #[cfg(test)]
    {
        OPENED.with(|o| o.borrow_mut().push(path.to_path_buf()));
    }
    #[cfg(not(test))]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
}

#[cfg(test)]
thread_local! {
    /// Every path the UI asked the system to open, in order (test-only).
    static OPENED: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Every URL the UI asked the system to open (test-only).
    static OPENED_URLS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Hand a markdown link to the default browser — **if it is a web link**.
///
/// The scheme check is repeated here even though `markdown::inline_runs`
/// only ever attaches an href that already passed it. `open(1)` launches
/// whatever has a registered handler, and the string arrives from an
/// agent's output: the guard belongs at the syscall, not only at the
/// parser, so that a future caller cannot route around it. A refused
/// target opens nothing and is not recorded as opened.
fn open_url(raw: &str) {
    let Some(url) = markdown::web_url(raw) else {
        return;
    };
    #[cfg(test)]
    {
        OPENED_URLS.with(|o| o.borrow_mut().push(url.clone()));
    }
    #[cfg(not(test))]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
}

fn fmt_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Anchor-flip scroll (spec 03 §Scroll): bottom-follow at rest; own send
/// flips to a top-anchored view pinning the sent message; user scroll
/// breaks follow; the scroll-to-bottom affordance restores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollMode {
    Follow,
    /// Pin the given transcript row's first display row to the viewport
    /// top, re-asserted on every update.
    Pinned {
        msg: u32,
    },
    /// Waiting for our just-sent message's row to appear in the feed.
    PinOnNextUserRow,
    Free,
}

/// Paint-only fade veil (spec 03 §Rendering): the freshest appended text
/// of the live message fades in; layout is unaffected (text enters layout
/// immediately at full size — only paint alpha animates).
struct Veil {
    msg: u32,
    /// Message-text byte offset where the freshest burst begins.
    from_byte: usize,
    /// Restarts the animation element; bumped per growth burst.
    generation: u64,
    duration: Duration,
}

/// Replay acceptance instrumentation (`EAVE_REPLAY_ASSERT=1`): after the
/// fixture's second user send, scroll up and freeze; any later movement of
/// the logical scroll anchor while frames stream/settle below is a scroll
/// jump — the acceptance requires zero.
struct ReplayAssert {
    frozen: Option<ListOffset>,
    jumps: usize,
}

struct TranscriptState {
    session_id: Option<String>,
    /// Slice Q2: the engine's own words when a launch was refused. Lives
    /// here rather than on `Shell` because it is cleared by the same feed
    /// event that clears the session — the two facts arrive together and
    /// must never disagree.
    launch_error: Option<String>,
    transcript: Transcript,
    display: DisplayList,
    list: ListState,
    scroll: ScrollMode,
    veil: Option<Veil>,
    /// EMA of delta-burst intervals, clamping the veil duration to
    /// 120–400 ms (spec 03 §Rendering).
    ema_ms: f32,
    last_growth: Option<Instant>,
    assert_replay: Option<ReplayAssert>,
    /// Slice K: the approval card's buttons live inside a virtualized row
    /// built by a static fn with no `Context`, so they emit through a
    /// clone of the shell's own command channel. Deliberately the SAME
    /// channel the keyboard path uses — the two routes are one behavior,
    /// which is what makes the harness's keyboard test meaningful.
    commands: Option<tokio_mpsc::UnboundedSender<Command>>,
    /// The composer, read at deny time for the operator's reason and
    /// cleared once it has been spent (see [`Shell::deny_reason`]).
    composer: Option<Entity<ComposerState>>,
    /// A weak handle back to the shell, for the same reason `composer` is
    /// here (Slice K): the rows are built by STATIC fns with no `Context`,
    /// and the fence copy button has to state that it copied. Weak so the
    /// shell owning this state does not own a cycle back to itself.
    shell: Option<gpui::WeakEntity<Shell>>,
    /// Tool rows the reader has opened, by tool id.
    ///
    /// Deliberately NOT cleared by `reset()`: a resubscribe replays the
    /// same conversation, and a reader who opened a card to study it
    /// should not have it shut by the engine reconnecting. It IS dropped
    /// on a real session switch, with everything else that belongs to the
    /// session that left.
    open_tools: std::collections::HashSet<String>,
    /// Slice N3 — the live-turn clock behind the working trailer.
    ///
    /// A signed BALANCE, never a bool. The recorder's two input channels
    /// are unordered relative to each other, so a fast CLI's
    /// `TurnCompleted` can be journaled BEFORE the `UserDelivered` that
    /// opened its turn (progress.md trap; `attention::Classifier`'s
    /// `turn_balance` is the engine-side twin of this field, and a bool
    /// there was a ~40% flake). Floored at −1 so the pair nets to closed
    /// in either arrival order — the failure a bool buys here is a
    /// phantom turn whose trailer never goes away.
    turn_balance: i32,
    /// When the operator asked, in journal ms: the `UserSent` timestamp.
    /// From the journal rather than from when this process happened to
    /// attach, so a session opened mid-turn reports the turn's real age
    /// and not the age of the subscription.
    turn_started_ms: Option<u64>,
}

impl TranscriptState {
    fn new() -> Self {
        // Count 1, not 0: the trailing under-glass spacer exists from
        // birth so every later count is uniformly display len + 1.
        let list = ListState::new(1, ListAlignment::Bottom, px(512.));
        list.set_follow_mode(gpui::FollowMode::Tail);
        Self {
            session_id: None,
            launch_error: None,
            transcript: Transcript::new(),
            display: DisplayList::new(),
            list,
            scroll: ScrollMode::Follow,
            veil: None,
            ema_ms: 200.0,
            last_growth: None,
            assert_replay: None,
            commands: None,
            composer: None,
            shell: None,
            open_tools: std::collections::HashSet::new(),
            turn_balance: 0,
            turn_started_ms: None,
        }
    }

    /// Is a turn in flight right now? (Slice N3.) The engine's own
    /// `SessionInfo.state` cannot answer this: the real CLI **lingers**
    /// after `turn_completed` waiting for more stdin, so it still reads
    /// `running` between turns (progress.md). The turn boundary on the
    /// wire is `TurnCompleted`, which is what the balance counts.
    fn in_turn(&self) -> bool {
        self.turn_balance > 0
    }

    fn live_len(&self) -> Option<(u32, usize)> {
        self.transcript
            .rows
            .iter()
            .enumerate()
            .rev()
            .find_map(|(ix, r)| match r {
                Row::Assistant { text, live: true } => Some((ix as u32, text.len())),
                _ => None,
            })
    }

    /// Fold one record batch and mirror the change into the `ListState`
    /// with minimal ops (splice for structure, remeasure for in-place
    /// growth/settle — spec 03 §Rendering).
    fn apply_records(
        &mut self,
        epoch: u64,
        records: &[eave_core::journal::Record],
    ) -> Result<(), ()> {
        let before_live = self.live_len();
        let user_rows_before = self.transcript.rows.len();
        if self.transcript.apply(epoch, records).is_err() {
            return Err(());
        }
        // Slice N3: the live-turn clock, folded from the same batch. After
        // the apply, so a batch rejected for a sequence gap never moves it.
        for rec in records {
            match &rec.event {
                eave_core::journal::JournalEvent::UserSent { .. } => {
                    self.turn_started_ms = Some(rec.ts_ms);
                }
                eave_core::journal::JournalEvent::UserDelivered {} => {
                    self.turn_balance = (self.turn_balance + 1).min(8);
                }
                // A segment ended: whatever was open died with the process.
                eave_core::journal::JournalEvent::Closed { .. } => {
                    self.turn_balance = 0;
                }
                eave_core::journal::JournalEvent::Provider {
                    event: eave_core::events::SessionEvent::TurnCompleted { .. },
                } => {
                    self.turn_balance = (self.turn_balance - 1).max(-1);
                }
                _ => {}
            }
        }
        let ops = self.display.update(&self.transcript);
        // List item count is always display rows + 1: the trailing
        // under-glass spacer (Stage H). Content splices carry indices
        // < len, so an append at old_len inserts BEFORE the spacer and
        // the spacer stays terminal without bookkeeping.
        if ops.reset {
            self.list.reset(self.display.len() + 1);
        } else {
            if let Some(range) = &ops.remeasure {
                self.list.remeasure_items(range.clone());
            }
            if let Some((range, count)) = &ops.splice {
                self.list.splice(range.clone(), *count);
            }
        }

        // Veil bookkeeping: how much did the live message grow?
        let after_live = self.live_len();
        match (before_live, after_live) {
            (before, Some((msg, new_len))) => {
                let old_len = match before {
                    Some((m, l)) if m == msg => l,
                    _ => 0,
                };
                if new_len > old_len {
                    let now = Instant::now();
                    if let Some(prev) = self.last_growth {
                        let dt = now.duration_since(prev).as_secs_f32() * 1000.0;
                        self.ema_ms = 0.8 * self.ema_ms + 0.2 * dt;
                    }
                    self.last_growth = Some(now);
                    let duration = Duration::from_millis(self.ema_ms.clamp(120.0, 400.0) as u64);
                    let generation = self.veil.as_ref().map(|v| v.generation + 1).unwrap_or(1);
                    self.veil = Some(Veil {
                        msg,
                        from_byte: old_len,
                        generation,
                        duration,
                    });
                }
            }
            // Live message settled: text is authoritative, veil is done.
            (_, None) => self.veil = None,
        }

        // Anchor flip: our sent message appeared — pin it to the top
        // (spec 03 §Scroll).
        if self.scroll == ScrollMode::PinOnNextUserRow {
            let new_user = self.transcript.rows[user_rows_before.min(self.transcript.rows.len())..]
                .iter()
                .position(|r| matches!(r, Row::User { .. }))
                .map(|off| (user_rows_before + off) as u32);
            if let Some(msg) = new_user {
                self.scroll = ScrollMode::Pinned { msg };
            }
        }
        self.assert_scroll();

        if let Some(mut ra) = self.assert_replay.take() {
            match ra.frozen {
                None => {
                    let users = self
                        .transcript
                        .rows
                        .iter()
                        .filter(|r| matches!(r, Row::User { .. }))
                        .count();
                    if users >= 2 {
                        // A user scrolled up mid-stream: freeze here.
                        self.scroll = ScrollMode::Free;
                        self.assert_scroll();
                        self.list.scroll_by(px(-600.));
                        ra.frozen = Some(self.list.logical_scroll_top());
                    }
                }
                Some(f) => {
                    let cur = self.list.logical_scroll_top();
                    if cur.item_ix != f.item_ix || cur.offset_in_item != f.offset_in_item {
                        println!(
                            "REPLAY_JUMP from=({},{:?}) to=({},{:?}) rows={}",
                            f.item_ix,
                            f.offset_in_item,
                            cur.item_ix,
                            cur.offset_in_item,
                            self.display.len()
                        );
                        ra.jumps += 1;
                        ra.frozen = Some(cur);
                    }
                }
            }
            if self.transcript.closed.is_some() {
                let jumps = ra.jumps;
                println!(
                    "REPLAY_ASSERT rows={} jumps={} {}",
                    self.display.len(),
                    jumps,
                    if jumps == 0 { "PASS" } else { "FAIL" }
                );
                std::process::exit(if jumps == 0 { 0 } else { 1 });
            }
            self.assert_replay = Some(ra);
        }
        Ok(())
    }

    /// Re-assert the scroll intent after content changed (spec 03: the
    /// pin is "re-asserted each layout"; follow-tail is the list's own
    /// mode).
    /// Re-measure the display row carrying `tool_id`.
    ///
    /// Opening a card changes its HEIGHT, and `ListState` caches heights.
    /// The display fold's own remeasure path cannot help here: it keys off
    /// the row's content fingerprint, and open-ness is view state that
    /// deliberately does not live on the `Row` — a fold over journal
    /// records has no business knowing what the reader has expanded. So
    /// the toggle re-measures the one row it changed, which is the same
    /// remeasure-never-splice rule the fold follows (`display::diff`),
    /// applied from the other side.
    fn remeasure_tool(&mut self, tool_id: &str) {
        let msg = self
            .transcript
            .rows
            .iter()
            .position(|r| matches!(r, Row::Tool { id, .. } if id == tool_id));
        let Some(msg) = msg else { return };
        if let Some(ix) = self.display.first_row_of_msg(msg as u32) {
            self.list.remeasure_items(ix..ix + 1);
        }
    }

    fn assert_scroll(&mut self) {
        match self.scroll {
            ScrollMode::Follow => {
                self.list.set_follow_mode(gpui::FollowMode::Tail);
            }
            ScrollMode::Pinned { msg } => {
                self.list.set_follow_mode(gpui::FollowMode::Normal);
                if let Some(item_ix) = self.display.first_row_of_msg(msg) {
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item: px(0.),
                    });
                }
            }
            ScrollMode::PinOnNextUserRow | ScrollMode::Free => {
                self.list.set_follow_mode(gpui::FollowMode::Normal);
            }
        }
    }

    fn reset(&mut self) {
        self.transcript.reset();
        self.display.update(&self.transcript);
        self.list.reset(self.display.len() + 1);
        self.veil = None;
        // Slice N3: the turn clock is a projection like everything else
        // here — a replay re-derives it, and a switch must not carry the
        // old session's open turn into the new one's trailer.
        self.turn_balance = 0;
        self.turn_started_ms = None;
    }
}

struct ComposerState {
    /// The live buffer: always the draft belonging to `key`.
    text: String,
    /// Which session `text` belongs to; `""` is the launcher, which has no
    /// composer mounted and therefore never accumulates one.
    key: String,
    /// Every OTHER session's unsent words, parked while it is not selected.
    /// The entry for `key` lives in `text`, never here — one draft has one
    /// home, so there is no pair to keep in step.
    drafts: HashMap<String, String>,
    /// When the caret last went solid (inventory §1.20). Every keystroke
    /// resets it, which is what keeps a typing burst from blinking: the
    /// cadence is solid through its first half-period, so the caret only
    /// starts flashing once the typist has stopped. `None` until the first
    /// keystroke — a caret nobody has typed at has nothing to blink about.
    caret_anchor: Option<Instant>,
}

impl ComposerState {
    /// Move the composer to another session, carrying each draft with the
    /// session it was typed into.
    ///
    /// Before Slice S there was one buffer for every session, so a sentence
    /// typed into A and left there was still in the field when B was
    /// selected — and the next Return sent A's words to B. The buffer is
    /// per-session now, and the switch is the only thing that moves it.
    ///
    /// An empty draft is removed rather than parked: absence and emptiness
    /// are the same fact here, and storing both invites them to disagree.
    fn switch_to(&mut self, key: String) {
        if key == self.key {
            return;
        }
        let outgoing = std::mem::take(&mut self.text);
        if outgoing.is_empty() {
            self.drafts.remove(&self.key);
        } else {
            self.drafts.insert(std::mem::take(&mut self.key), outgoing);
        }
        self.text = self.drafts.remove(&key).unwrap_or_default();
        self.key = key;
    }
}

// ---------------------------------------------------------------------------
// Veiled text element: paint-only fade of the fresh tail

/// One markdown block as styled runs, with the freshest `veil_from_run`
/// suffix painted at animated alpha. Layout sees the full text at full
/// size from the first frame — opacity only, zero translate (spec 03).
#[derive(Clone, IntoElement)]
struct VeiledBlock {
    text: SharedString,
    runs: Vec<InlineRun>,
    /// Visible-char count that is already settled (veil starts after it).
    settled_chars: usize,
    alpha: f32,
    color: Hsla,
    code_color: Hsla,
}

impl VeiledBlock {
    /// A block with no veil: every character settled, full alpha. The
    /// constructor every non-streaming inline surface uses — headings,
    /// blockquotes, list items, table cells — so that all of them get
    /// links, inline code and emphasis from one implementation.
    fn plain(runs: Vec<InlineRun>, color: Hsla) -> Self {
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        let settled_chars = text.chars().count();
        Self {
            text: SharedString::from(text),
            runs,
            settled_chars,
            alpha: 1.0,
            color,
            code_color: theme::code_text(),
        }
    }

    /// Byte ranges of the flat text that carry an href, in order.
    fn link_ranges(&self) -> Vec<(std::ops::Range<usize>, SharedString)> {
        let mut out = Vec::new();
        let mut at = 0usize;
        for run in &self.runs {
            let end = at + run.text.len();
            if let Some(href) = &run.href {
                out.push((at..end, SharedString::from(href.clone())));
            }
            at = end;
        }
        out
    }

    fn text_runs(&self, base_font: &Font) -> Vec<TextRun> {
        let mut out = Vec::with_capacity(self.runs.len() + 1);
        let mut seen = 0usize;
        for run in &self.runs {
            let mut font = base_font.clone();
            if run.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if run.style.italic {
                font.style = FontStyle::Italic;
            }
            if run.style.code {
                font.family = theme::MONO.into();
            }
            let color = if run.style.code {
                self.code_color
            } else {
                self.color
            };
            let underline = run.style.link.then(|| UnderlineStyle {
                thickness: px(theme::HAIRLINE),
                color: None,
                wavy: false,
            });
            let strikethrough = run.style.strike.then(|| StrikethroughStyle {
                thickness: px(theme::HAIRLINE),
                color: None,
            });
            let len = run.text.len();
            let chars = run.text.chars().count();
            let make = |len: usize, alpha: f32| TextRun {
                len,
                font: font.clone(),
                color: color.opacity(alpha * color.a),
                background_color: None,
                underline,
                strikethrough,
            };
            if seen + chars <= self.settled_chars {
                out.push(make(len, 1.0));
            } else if seen >= self.settled_chars {
                out.push(make(len, self.alpha));
            } else {
                // The veil boundary falls inside this run: split it.
                let split_chars = self.settled_chars - seen;
                let split_bytes: usize = run
                    .text
                    .chars()
                    .take(split_chars)
                    .map(|c| c.len_utf8())
                    .sum();
                out.push(make(split_bytes, 1.0));
                out.push(make(len - split_bytes, self.alpha));
            }
            seen += chars;
        }
        out.retain(|r| r.len > 0);
        out
    }
}

impl RenderOnce for VeiledBlock {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let base_font = window.text_style().font();
        let runs = self.text_runs(&base_font);
        let links = self.link_ranges();
        let styled = StyledText::new(self.text).with_runs(runs);
        if links.is_empty() {
            // The common case pays nothing: no hitbox, no listeners, the
            // same element the transcript has always drawn.
            styled.into_any_element()
        } else {
            LinkText::new(styled, links).into_any_element()
        }
    }
}

// ---------------------------------------------------------------------------
// Clickable links inside a run of styled text

/// [`StyledText`] plus hit-testing for the sub-ranges that are links.
///
/// gpui ships `InteractiveText`, and it is deliberately not used here. It
/// carries a tooltip builder, a hover listener and cross-frame element
/// state for the mouse-down index — machinery that needs an `ElementId`
/// per block and a `window.refresh()` between the down and the up. What a
/// transcript link needs is one hitbox and one mouse-up, and the veil's
/// animated element is rebuilt from scratch every frame, so state that has
/// to survive a frame is state that would have to be threaded through the
/// display tree for nothing.
///
/// The mapping from pixels to a link is `TextLayout::index_for_position`,
/// the same call `InteractiveText` makes, which is why a link that WRAPS
/// across two lines is still one link: the hit test runs against the
/// laid-out glyph, not against a rectangle somebody guessed.
struct LinkText {
    styled: StyledText,
    layout: TextLayout,
    links: Vec<(std::ops::Range<usize>, SharedString)>,
}

impl LinkText {
    fn new(styled: StyledText, links: Vec<(std::ops::Range<usize>, SharedString)>) -> Self {
        // The layout is an `Rc` shared with the element, so this clone
        // reads what the real layout pass wrote.
        let layout = styled.layout().clone();
        Self {
            styled,
            layout,
            links,
        }
    }

    fn href_at(
        links: &[(std::ops::Range<usize>, SharedString)],
        ix: usize,
    ) -> Option<&SharedString> {
        links.iter().find(|(r, _)| r.contains(&ix)).map(|(_, h)| h)
    }
}

impl IntoElement for LinkText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for LinkText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        Element::request_layout(&mut self.styled, None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        Element::prepaint(
            &mut self.styled,
            None,
            inspector_id,
            bounds,
            state,
            window,
            cx,
        );
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        Element::paint(
            &mut self.styled,
            None,
            inspector_id,
            bounds,
            request,
            &mut (),
            window,
            cx,
        );
        let layout = self.layout.clone();
        let links = std::mem::take(&mut self.links);
        let over_link_now = layout
            .index_for_position(window.mouse_position())
            .ok()
            .is_some_and(|ix| Self::href_at(&links, ix).is_some());
        if over_link_now {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        {
            let (layout, links, hitbox) = (layout.clone(), links.clone(), hitbox.clone());
            window.on_mouse_event(move |ev: &MouseUpEvent, phase, window: &mut Window, _cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                // `Ok` only: `index_for_position` answers `Err(nearest)` for
                // a point that is inside the block but past the end of a
                // line, and treating that as a hit would open a link from
                // the empty margin beside it.
                if let Ok(ix) = layout.index_for_position(ev.position) {
                    if let Some(href) = Self::href_at(&links, ix) {
                        open_url(href);
                    }
                }
            });
        }
        {
            // The cursor is decided at PAINT time, and a settled window
            // paints on nothing (N4's idle floor) — so without this the
            // pointer could sit on a link showing an arrow forever. One
            // frame is bought only when the answer actually changes, which
            // is at a link's edge, not per glyph.
            let hitbox = hitbox.clone();
            window.on_mouse_event(
                move |ev: &MouseMoveEvent, phase, window: &mut Window, _cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    let now = layout
                        .index_for_position(ev.position)
                        .ok()
                        .is_some_and(|ix| Self::href_at(&links, ix).is_some());
                    if now != over_link_now {
                        window.refresh();
                    }
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Perf probe (spec 04 §Acceptance: 10k messages, no frame over 16 ms)

struct PerfProbe {
    frame_times: Vec<Duration>,
    /// Main-thread CPU consumed per frame (layout, shaping, paint record).
    /// Wall deltas on a loaded machine measure the scheduler, not us;
    /// vsync idle burns no CPU, so this is the render cost itself.
    cpu_times: Vec<Duration>,
    last_cpu: Option<Duration>,
    last_frame: Option<Instant>,
    /// Top-of-viewport row this frame; advanced by `PERF_ROWS_PER_FRAME`.
    /// Item-based (not pixel `scroll_by`): unmeasured rows are zero-height
    /// in the seek, so pixel scrolling would teleport past them unmeasured
    /// — the opposite of what the acceptance measures.
    item_ix: usize,
    done: bool,
}

/// ~8 rows/frame ≈ a 19k px/s flick at these row heights — top to bottom
/// of 10k rows in ~20 s, every row measured on the way.
const PERF_ROWS_PER_FRAME: usize = 8;

fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

// ---------------------------------------------------------------------------
// Shell

/// Slice P1's demo state, and the whole of the overlay layer's presence
/// in the product: nothing else in eave opens a popup.
///
/// It hangs off the shell rather than living in the overlay module because
/// that is what a real consumer will do — a popup belongs to whatever owns
/// the affordance that opened it, and building the demo any other way
/// would have proved the machinery works in a shape no consumer can copy.
/// Inert unless [`overlay::demo_enabled`]; see there for the gate.
#[derive(Default)]
struct DemoOverlay {
    /// `T` is the anchor point the menu was opened at, in window
    /// coordinates — the smallest thing a popup can need to remember, and
    /// enough to prove the parameter is carried through the exit.
    menu: Popup<Point<Pixels>>,
    /// The last row chosen. Nothing reads it but the tests; it exists so
    /// that "the row fired" is an observable fact rather than an absence.
    chosen: Option<SharedString>,
}

struct Shell {
    focus: FocusHandle,
    conn: Entity<ConnectionState>,
    sessions: Entity<SessionsState>,
    files: Entity<FilesState>,
    transcript: Entity<TranscriptState>,
    composer: Entity<ComposerState>,
    commands: tokio_mpsc::UnboundedSender<Command>,
    perf: Option<PerfProbe>,
    /// Transient ⌘L confirmation shown in the footer ("logged 14:52:03"
    /// or the write error), cleared after a few seconds. The generation
    /// counter keeps a stale clear-timer from eating a newer note.
    friction: Option<(u64, String)>,
    friction_gen: u64,
    /// Whether the friction log's path has been stated in the footer yet
    /// this run (Stage L).
    friction_path_stated: bool,
    /// The rails' scroll offsets (Slice M). Held only so the scroll-edge
    /// fades can read overflow at PAINT time — nothing else reads them,
    /// and no scroll POSITION is authoritative here (spec 03: the UI
    /// holds no authoritative state).
    rail_scroll: ScrollHandle,
    files_scroll: ScrollHandle,
    /// Slice P1 — see [`DemoOverlay`]. No product consumer.
    demo: DemoOverlay,
    /// The rail's inline rename field (Stage AC), when one is open.
    /// `None` = nobody is naming anything. Held on the shell for the same
    /// reason `launcher` is: it is a choice in progress, not a fact about
    /// a session, so it must survive the feed events that rebuild the
    /// rail underneath it.
    rename: Option<RenameEdit>,
    /// Slice Q2 — the launcher's own state. Held on the shell rather than
    /// in `TranscriptState` because it is a *choice in progress*, not a
    /// fact about a session: it survives the feed events that clear the
    /// transcript, so pressing `+ new` twice does not reset the agent you
    /// were half-way through picking.
    launcher: LauncherUi,
    /// Slice AH — since when the files rail has had no answer, and for
    /// which session. `None` = it is not waiting.
    ///
    /// A `Cell` on the shell rather than a field on `FilesState`, for two
    /// reasons that are the same reason. It is not a fact about the
    /// session — it is this window's record of its own patience, so it has
    /// no business in a projection of engine state. And the gate that
    /// reads it ([`Shell::work_in_flight`]) takes `&self` and runs inside
    /// the render pass: an entity write there would notify, and a notify
    /// per frame is a second way to build the exact defect this closes.
    ///
    /// The clock is the executor's ([`gpui::BackgroundExecutor::now`]),
    /// never `Instant::now`, so the deterministic scheduler can advance it
    /// — the same choice `motion::pulse_now` makes and for the same
    /// reason.
    files_wait_since: Cell<Option<(String, Instant)>>,
}

/// One open rail rename (Stage AC).
///
/// `rename` has been complete on the wire since Stage AB — method,
/// engine, client, persistence, tests — with no caller. This is the
/// caller.
///
/// The buffer is NOT the composer. Sharing eave's one text field would
/// mean the rename box either eats a session's unsent draft or shows it,
/// and Slice T went to some trouble to make a draft belong to exactly one
/// session. A name being typed belongs to a row.
struct RenameEdit {
    /// The row being named. The field renders in that row's label slot
    /// and nowhere else, so a rail that re-folds underneath it cannot
    /// move the box onto a different session.
    session_id: String,
    /// What has been typed. Sent raw — the engine normalizes and the
    /// reply is what landed (`Command::Rename`).
    text: String,
}

/// Slice Q2. The provider strip's selection and the model menu.
struct LauncherUi {
    /// Index into [`launcher::PROVIDERS`].
    provider: usize,
    /// The model menu — P1's `Popup`, anchored at the picker button. `T`
    /// is the anchor point in window coordinates.
    models: Popup<Point<Pixels>>,
    /// The chosen `--model`. **`None` means pass no flag at all**, which
    /// is `launcher::MODEL_DEFAULT` and is not the same as an empty one.
    model: Option<&'static str>,
}

impl Default for LauncherUi {
    fn default() -> Self {
        Self {
            provider: launcher::default_provider(),
            models: Popup::default(),
            model: None,
        }
    }
}

/// The data dir this process is running against, recorded by [`run`] so
/// code below the feed loop can resolve paths against it (Stage L).
/// Unset under the test harness, which never calls `run`.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Data dir per spec 02 §Layout — the default when neither `--data-dir`
/// nor `EAVE_DATA_DIR` is given. Lives here rather than in `main.rs` so
/// the friction log can fall back to the same place without a second copy
/// of the layout rule.
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME is set");
    PathBuf::from(home).join("Library/Application Support/eave")
}

/// Where ⌘L appends its stamp (Stage L — this used to hardcode one
/// person's desk, `~/Desktop/eave/docs/dogfood-log.md`, which existed on
/// exactly one machine and silently failed to write on any other):
///
/// 1. `EAVE_DOGFOOD_LOG` if set — the dev-loop and test override, and now
///    also how anyone keeps writing into a checkout's own docs.
/// 2. `<data_dir>/dogfood-log.md` — beside the journals the notes are
///    about, so the log follows `--data-dir` the way everything else does.
///
/// A missing file is created; a missing directory is a stated write error
/// in the footer.
fn friction_log_path() -> PathBuf {
    resolve_friction_log(
        std::env::var("EAVE_DOGFOOD_LOG").ok(),
        DATA_DIR.get().cloned(),
    )
}

/// The resolution rule itself, taking its inputs rather than reading
/// them, so it is testable without touching process-wide env or the
/// once-set data dir.
fn resolve_friction_log(override_path: Option<String>, data_dir: Option<PathBuf>) -> PathBuf {
    match override_path {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => data_dir
            .unwrap_or_else(default_data_dir)
            .join("dogfood-log.md"),
    }
}

/// A path with `$HOME` collapsed to `~`, for stating a location in
/// chrome without spending half the footer on it.
fn tilde(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

/// Local wall-clock stamp for the friction log (the log is read by a
/// human at this desk — UTC would be friction in the friction log).
fn local_timestamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// How long the ⌘L footer confirmation stands.
const FRICTION_NOTE_TTL: Duration = Duration::from_secs(4);

impl Shell {
    /// Send the composer draft (enter key and send button share this).
    fn send_current(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.update(cx, |c, cx| {
            let t = c.text.trim().to_string();
            c.text.clear();
            cx.notify();
            t
        });
        if !text.is_empty() {
            // Anchor flip on own send (spec 03 §Scroll): pin once the row
            // lands in the feed.
            self.transcript.update(cx, |t, _| {
                t.scroll = ScrollMode::PinOnNextUserRow;
            });
            let _ = self.commands.send(Command::Send(text));
        }
    }

    /// ⌘L: append one timestamped context line — selected session id and
    /// its reported state — to the dogfood friction log, and confirm (or
    /// state the failure) in the footer. Deliberately nothing more: the
    /// keystroke's job is to make the moment findable; the note is
    /// written in the log file afterwards.
    fn friction_log(&mut self, cx: &mut Context<Self>) {
        let stamp = local_timestamp();
        let session = self.transcript.read(cx).session_id.clone();
        let context = match &session {
            None => "no session".to_string(),
            Some(id) => {
                let state = self
                    .sessions
                    .read(cx)
                    .sessions
                    .iter()
                    .find(|s| &s.session_id == id)
                    .map(|s| s.state.clone());
                let short = &id[..id.len().min(8)];
                match state {
                    Some(st) => format!("{short} {st}"),
                    None => short.to_string(),
                }
            }
        };
        let path = friction_log_path();
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "- {stamp} · {context} · ")
            });
        let note = match written {
            // The destination is stated on the first append of the
            // session, once: the path moved with the data dir in Stage L,
            // and a friction log whose location is a guess is friction.
            Ok(()) if !std::mem::replace(&mut self.friction_path_stated, true) => {
                format!("logged {} → {}", &stamp[11..], tilde(&path))
            }
            Ok(()) => format!("logged {}", &stamp[11..]),
            Err(e) => format!("log failed: {e}"),
        };
        self.transient_note(note, cx);
    }

    /// State one fact in the footer for [`FRICTION_NOTE_TTL`], then take it
    /// back.
    ///
    /// Extracted from `friction_log`, which was its only writer until the
    /// fence copy button needed the same shape. The generation counter is
    /// the load-bearing part: two notes in quick succession each arm their
    /// own timer, and without it the FIRST timer would clear the SECOND
    /// note early — the note would vanish while it was still true.
    fn transient_note(&mut self, note: String, cx: &mut Context<Self>) {
        self.friction_gen += 1;
        let gen = self.friction_gen;
        self.friction = Some((gen, note));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FRICTION_NOTE_TTL).await;
            let _ = this.update(cx, |shell, cx| {
                if shell.friction.as_ref().is_some_and(|(g, _)| *g == gen) {
                    shell.friction = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The request id of the oldest open permission question, if any
    /// (Slice K). Oldest, not newest: questions are answered in the order
    /// the agent asked them, which is the order the cards read in.
    fn oldest_pending_approval(&self, cx: &Context<Self>) -> Option<String> {
        self.transcript
            .read(cx)
            .transcript
            .pending_approvals()
            .find_map(|row| match row {
                Row::Approval { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
    }

    /// Answer the oldest open question from the keyboard. Same channel,
    /// same command as the buttons — one behavior with two doors.
    fn decide_oldest(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(request_id) = self.oldest_pending_approval(cx) else {
            return;
        };
        let message = if allow {
            String::new()
        } else {
            let composer = self.composer.clone();
            take_deny_reason(Some(&composer), cx)
        };
        let _ = self.commands.send(Command::Decide {
            request_id,
            allow,
            message,
        });
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // Slice P1: an open overlay owns the keyboard, and it owns ALL of
        // it — escape dismisses, the arrows move the highlight, return
        // chooses, and every other key is swallowed rather than falling
        // through to the composer. A menu you can type past is not a menu,
        // and the alternative (letting unhandled keys through) means a
        // stray character lands in a draft the user cannot see behind the
        // surface. Both dismissals stay reachable, so this traps nobody.
        //
        // Ahead of the caret-anchor reset below on purpose: navigating a
        // menu is not "the typist is still in the composer", and re-anchoring
        // the blink from here would make an arrow key look like a keystroke.
        if self.launcher.models.is_open() {
            match ks.key.as_str() {
                "escape" => self.close_model_menu(cx),
                "down" => {
                    self.launcher.models.highlight_next();
                    cx.notify();
                }
                "up" => {
                    self.launcher.models.highlight_prev();
                    cx.notify();
                }
                "enter" => match self.launcher.models.highlighted() {
                    Some(ix) => self.choose_model(ix, cx),
                    None => self.close_model_menu(cx),
                },
                _ => {}
            }
            return;
        }
        if self.demo.menu.is_open() {
            match ks.key.as_str() {
                "escape" => self.close_demo_menu(cx),
                "down" => {
                    self.demo.menu.highlight_next();
                    cx.notify();
                }
                "up" => {
                    self.demo.menu.highlight_prev();
                    cx.notify();
                }
                "enter" => match self.demo.menu.highlighted() {
                    Some(ix) => self.choose_demo(ix, cx),
                    // Return with nothing highlighted dismisses rather
                    // than firing the first row: the user has not chosen.
                    None => self.close_demo_menu(cx),
                },
                _ => {}
            }
            return;
        }
        // Stage AC: an open rename field owns the keyboard, exactly the
        // way P1's menus above do, and for the same reason — a field you
        // can type past is not a field, and every key that fell through
        // here would land in a composer draft the operator cannot see
        // behind the row they are naming.
        //
        // Ahead of the no-session return below, because the rail is on
        // screen at the launcher too: a session can be named while
        // nothing is selected, and the early return would swallow every
        // keystroke aimed at the field.
        //
        // Ahead of the caret-anchor reset for the same reason as the
        // overlay gates: nobody is typing in the composer.
        if self.rename.is_some() {
            match ks.key.as_str() {
                "escape" => self.cancel_rename(cx),
                "enter" => self.commit_rename(cx),
                "backspace" => {
                    if let Some(edit) = self.rename.as_mut() {
                        edit.text.pop();
                    }
                    cx.notify();
                }
                // Paste, on the same chord the composer uses. A title is
                // the one field somebody is likely to paste a branch name
                // into, and the engine's cap is what makes that safe.
                "v" if ks.modifiers.platform => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        if let Some(edit) = self.rename.as_mut() {
                            edit.text.push_str(&text);
                        }
                        cx.notify();
                    }
                }
                _ => {
                    if let Some(ch) = ks.key_char.clone() {
                        if !ks.modifiers.platform && !ks.modifiers.control {
                            if let Some(edit) = self.rename.as_mut() {
                                edit.text.push_str(&ch);
                            }
                            cx.notify();
                        }
                    }
                }
            }
            return;
        }
        // Slice Q2 unmounted the composer at the launcher (`show_launcher`
        // below) but left this handler open, so keystrokes still landed in
        // the buffer behind it — invisible, because the element is not
        // rendered — and Return then dispatched a `Send` the rest loop
        // drops, there being no session to address it to. The words
        // surfaced later in whatever session opened first. Q2's own comment
        // at the mount site says an input that silently eats what you type
        // is the one thing this must not be; that is the half this closes.
        //
        // Ahead of the caret-anchor reset for the same reason as the
        // overlay gates: nobody is typing in a composer that is not there.
        if self.transcript.read(cx).session_id.is_none() {
            // ⌘L survives. The friction log stamps the MOMENT, not a
            // session, and it already states "no session" when there is
            // none — the launcher is exactly where a dogfooder needs it.
            if ks.key == "l" && ks.modifiers.platform {
                self.friction_log(cx);
            }
            return;
        }
        // §1.20: ANY key resets the blink anchor, not just the ones that
        // change the text. Backspacing to empty, a rejected ⌘-chord, an
        // arrow key — they are all "the typist is still here", and a caret
        // that blinked through them would be flashing at someone who is
        // actively using the composer.
        let struck = cx.background_executor().now();
        self.composer
            .update(cx, |c, _| c.caret_anchor = Some(struck));
        match ks.key.as_str() {
            "enter" => self.send_current(cx),
            "backspace" => {
                self.composer.update(cx, |c, cx| {
                    c.text.pop();
                    cx.notify();
                });
            }
            // ctrl-k: kill the live session (dev affordance; real kill UI
            // is later stage work).
            "k" if ks.modifiers.control => {
                let _ = self.commands.send(Command::Kill);
            }
            // cmd-l: friction log (dogfood prep directive 2026-08-17) —
            // append a context stamp to the dogfood log; the note itself
            // is written there afterwards, out of band.
            "l" if ks.modifiers.platform => self.friction_log(cx),
            // cmd-v: paste (Stage D build directive; IME stays deferred).
            "v" if ks.modifiers.platform => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.composer.update(cx, |c, cx| {
                        c.text.push_str(&text);
                        cx.notify();
                    });
                }
            }
            _ => {
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ks.modifiers.platform && !ks.modifiers.control {
                        let ch = ch.clone();
                        self.composer.update(cx, |c, cx| {
                            c.text.push_str(&ch);
                            cx.notify();
                        });
                    }
                }
            }
        }
    }

    /// Open the rail's inline rename field on one row (Stage AC), seeded
    /// with whatever that session is called now.
    ///
    /// Seeded rather than blank because the common edit is a correction,
    /// and a field that cleared the name you are trying to fix makes
    /// "leave it alone" the same gesture as "delete it".
    fn begin_rename(&mut self, id: String, cx: &mut Context<Self>) {
        let text = self
            .sessions
            .read(cx)
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.title.clone())
            .unwrap_or_default();
        self.rename = Some(RenameEdit {
            session_id: id,
            text,
        });
        cx.notify();
    }

    /// Close the field, sending nothing. Escape, and every path that takes
    /// the row out from under it.
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            cx.notify();
        }
    }

    /// Send what was typed and close the field.
    ///
    /// The raw text goes to the engine — normalization is `store::
    /// normalize_title`'s and the reply is what landed, so there is one
    /// implementation of what a title is rather than two that drift. An
    /// empty field clears the name: that is the same verb, which is what
    /// stops "set" and "clear" from ever disagreeing.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.rename.take() else {
            return;
        };
        let title = (!edit.text.trim().is_empty()).then_some(edit.text);
        let _ = self.commands.send(Command::Rename {
            id: edit.session_id,
            title,
        });
        cx.notify();
    }

    /// Is the composer caret drawn this frame? (inventory §1.20 — 500 ms
    /// half-period, solid through the first half so a typing burst never
    /// blinks.)
    ///
    /// **Only a draft blinks, and that is a divergence worth reading.** A
    /// blink is the one tween in this slice that never settles, so its
    /// consumer holds a pulse lease for as long as it is on screen — and
    /// eave's caret is on screen always, which would mean a window that
    /// redraws 30 times a second forever. That directly contradicts the
    /// invariant N1 was built around and asserts twice: *a window with
    /// nothing mid-flight schedules nothing at all.* Gating on a draft keeps
    /// the invariant reachable — an idle eave still schedules nothing — and
    /// keeps the blink where it means something, marking the insertion point
    /// in real text rather than decorating a placeholder. Measured cost of
    /// the ungated version is in `docs/reports/stage-n2-hover.md` §5; if the
    /// user wants it always, delete the `has_draft` early return and nothing
    /// else changes.
    fn caret_lit(&self, cx: &mut Context<Self>) -> bool {
        let (has_draft, anchor) = {
            let c = self.composer.read(cx);
            (!c.text.is_empty(), c.caret_anchor)
        };
        if !has_draft {
            return true;
        }
        // The lease is the whole reason this call is `&mut`: it is what
        // keeps the clock ticking, and dropping it is what parks the clock.
        let view = cx.entity_id();
        let (Some(now), Some(anchor)) = (motion::pulse_now(view, cx), anchor) else {
            // Reduced motion, or a draft that arrived without a keystroke
            // (a paste through the fixture path). Solid.
            return true;
        };
        motion::caret_visible(now.saturating_duration_since(anchor))
    }

    fn on_wheel(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // User scroll breaks follow/pin; wheel back to the end restores
        // follow (spec 03 §Scroll). Work is deferred out of the scroll
        // handler (known ListState borrow papercut, spec 03 §No-fork
        // consequences).
        let up = ev.delta.pixel_delta(window.line_height()).y > px(0.);
        if std::env::var("EAVE_DEBUG_SCROLL").is_ok() {
            eprintln!(
                "WHEEL delta={:?} up={up}",
                ev.delta.pixel_delta(window.line_height()).y
            );
        }
        self.transcript.update(cx, |t, cx| {
            // Nothing to scroll on an empty log: never leave Follow, so the
            // ↓-bottom affordance can't appear over blank space (the Stage E
            // cosmetic bug).
            if t.display.is_empty() {
                return;
            }
            if up {
                if t.scroll != ScrollMode::Free {
                    t.scroll = ScrollMode::Free;
                    t.assert_scroll();
                    cx.notify();
                }
            } else if t.scroll == ScrollMode::Free && t.list.is_scrolled_to_end().unwrap_or(false) {
                t.scroll = ScrollMode::Follow;
                t.assert_scroll();
                cx.notify();
            }
        });
    }

    /// Wrap content in the reading column: full-width gutter, centered cap
    /// (every surface in the shell shares this rhythm).
    fn column(inner: impl IntoElement) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(theme::GUTTER))
            .child(
                div()
                    .w_full()
                    .max_w(px(theme::CONTENT_MAX_W))
                    .min_w(px(0.))
                    .child(inner),
            )
    }

    /// Custom titlebar: native drag/double-click via the window-control
    /// area, traffic lights inset by the platform options.
    fn titlebar(&self, cx: &Context<Self>) -> gpui::Div {
        let session = self.transcript.read(cx).session_id.clone();
        let short = session
            .as_deref()
            .map(|s| s.chars().take(8).collect::<String>());
        // No seam below: the titlebar sits on the same continuous glass
        // ground as the rails (comet's shell) — the content card carries
        // its own outline.
        div()
            .w_full()
            .h(px(theme::TITLEBAR_H))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .window_control_area(WindowControlArea::Drag)
            .child(div().w(px(theme::TRAFFIC_CLEARANCE)).h_full().flex_none())
            .child(
                // Rail collapse toggle. Slice P3 replaced the hand-drawn
                // pictogram (a bordered rect with a hairline `div` at the
                // third) with the drawn glyph it was imitating. Same mark,
                // to within half a pixel — see `theme::ICON_TOGGLE` for why
                // the BOX is larger — and one element instead of three.
                div()
                    .id("rail-toggle")
                    .debug_selector(|| "rail-toggle".into())
                    .p(px(theme::SPACE_1))
                    .mr_2()
                    .rounded(px(theme::RADIUS_BUTTON))
                    .hover_fade("rail-toggle", theme::wash_rest(), theme::overlay())
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sessions.update(cx, |s, cx| {
                            s.collapsed = !s.collapsed;
                            cx.notify();
                        });
                    }))
                    .child(icons::tagged_icon(
                        icons::SIDEBAR_MINIMALISTIC_LEFT,
                        theme::ICON_TOGGLE,
                        // The pigment the hand-drawn version painted its
                        // border with. Named here, not inherited — gpui
                        // does not cascade colour into an SVG.
                        theme::text_tertiary(),
                        "rail-toggle-glyph",
                    )),
            )
            .child(
                div()
                    .text_size(px(theme::TITLE_SIZE))
                    .font_weight(theme::WEIGHT_LABEL)
                    .text_color(theme::text_secondary())
                    .child("eave"),
            )
            .children(short.map(|s| {
                div()
                    .ml_2()
                    .text_size(px(theme::SMALL_SIZE))
                    .text_color(theme::text_tertiary())
                    .child(s)
            }))
            .child(div().flex_1())
            .child(
                // Files-rail collapse toggle: the same glyph mirrored
                // (divider on the right third). Slice P3, as above.
                div()
                    .id("files-toggle")
                    .debug_selector(|| "files-toggle".into())
                    .p(px(theme::SPACE_1))
                    .mr_2()
                    .rounded(px(theme::RADIUS_BUTTON))
                    .hover_fade("files-toggle", theme::wash_rest(), theme::overlay())
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.files.update(cx, |f, cx| {
                            f.collapsed = !f.collapsed;
                            cx.notify();
                        });
                    }))
                    .child(icons::tagged_icon(
                        icons::SIDEBAR_MINIMALISTIC,
                        theme::ICON_TOGGLE,
                        theme::text_tertiary(),
                        "files-toggle-glyph",
                    )),
            )
    }

    /// The session rail (Slice A): rows on the glass ground, an instrument list of
    /// every session the engine holds — recency-ordered (newest first),
    /// one row per session: state dot, self-drawn provider identity,
    /// attention badge only when a state deserves it, cwd as quiet meta.
    /// The highlight is a projection of the feed task's actual
    /// subscription (`TranscriptState::session_id`); clicking only
    /// *requests* a switch.
    fn rail(&self, cx: &Context<Self>, phase: Option<f32>) -> AnyElement {
        // Already in draw order: `fold_rail` decided it when each row
        // arrived, and deliberately does not re-decide it here (Stage AB).
        let all = self.sessions.read(cx).sessions.clone();
        let selected = self.transcript.read(cx).session_id.clone();
        let row_notes = self.sessions.read(cx).row_notes.clone();
        // Stage AC. The filter runs HERE, over the folded rows, and the
        // rows themselves are untouched — so the count below is a real
        // count and un-archiving needs nothing to come back from.
        let show_archived = self.sessions.read(cx).show_archived;
        let total = all.len();
        let archived_count = all.iter().filter(|s| s.archived).count();
        let editing = self
            .rename
            .as_ref()
            .map(|e| (e.session_id.clone(), e.text.clone()));
        let home = std::env::var("HOME").ok();
        // Stage L's aggregate ceiling, computed over EVERY session and not
        // over the filtered list. An archived agent still holding 2 GB is
        // precisely the cost the operator has stopped thinking about, and
        // dropping it out of the total the moment the row is hidden would
        // make the ceiling quietly optimistic — the one thing a ceiling
        // may not be.
        let resource = resource_line(&all, physical_memory_cached());
        let sessions: Vec<SessionInfo> = all
            .into_iter()
            .filter(|s| rail_shows(s, selected.as_deref(), show_archived))
            .collect();
        // The meta line aligns under the provider label: dot + gap +
        // chip + gap.
        let meta_indent = theme::STATE_DOT + theme::SPACE_2 + theme::PROVIDER_CHIP + theme::SPACE_2;
        let rows: Vec<AnyElement> = sessions
            .into_iter()
            .map(|info| {
                let is_sel = selected.as_deref() == Some(info.session_id.as_str());
                let (glyph, provider_label) = provider_identity(&info.provider);
                let label = row_label(&info, &provider_label);
                let dot = state_color(&info.state);
                // Slice K: the badge counts the open questions when there
                // is more than one — "waiting on you (3)" is a different
                // fact from "waiting on you".
                let badge = state_badge(&info.state).map(|word| {
                    if info.state == "awaiting_approval" && info.pending_approvals > 1 {
                        format!("{word} ({})", info.pending_approvals)
                    } else {
                        word.to_string()
                    }
                });
                let posture = info.permission_mode.as_deref().map(posture_line);
                let posture_asks =
                    matches!(info.permission_mode.as_deref(), Some("default") | Some("manual"));
                let sid = info.session_id.clone();
                // Slice I2 facts, stated as quiet meta lines. Sharing:
                // sessions working in the same checkout (decision 1 —
                // detection automatic, stated, never acted on).
                let shares =
                    (!info.shared_with.is_empty()).then(|| shares_line(&info.shared_with));
                // Isolation is offered only while it is still possible:
                // nothing ever spawned, and a collision actually stands.
                let can_isolate = info.state == "new"
                    && !info.live
                    && info.worktree.is_none()
                    && !info.shared_with.is_empty();
                let wt_note = row_notes.get(&info.session_id).cloned();
                let armed = matches!(wt_note, Some(RowNote::ConfirmDelete { .. }));
                let wt = info.worktree.clone();
                let wt_line = wt.as_ref().map(worktree_line);
                // Orphan affordances (decision 2): open in editor, or
                // delete — never while a process is live in it.
                let wt_present = wt.as_ref().map(|w| w.present).unwrap_or(false);
                // The worktree directory IS the session cwd (isolate
                // rewrote it) — that is what "open" opens.
                let wt_path = info.cwd.clone();
                let show_wt_buttons = wt.is_some() && !info.live;
                let note_line = wt_note.as_ref().map(|n| match n {
                    RowNote::ConfirmDelete {
                        dirty_files,
                        unmerged_commits,
                    } => {
                        let fmt = |v: &Option<u64>| {
                            v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
                        };
                        (
                            format!(
                                "{} uncommitted files, {} unmerged commits — delete again to mean it",
                                fmt(dirty_files),
                                fmt(unmerged_commits)
                            ),
                            theme::warning(),
                        )
                    }
                    RowNote::Error(msg) => (msg.clone(), theme::danger()),
                });
                let sid_iso = info.session_id.clone();
                let sid_del = info.session_id.clone();
                let sid_ren = info.session_id.clone();
                let sid_arc = info.session_id.clone();
                // Stage AC: the row's own facts about being filed away.
                let archived_word = archived_line(&info, show_archived);
                let archive_to = !info.archived;
                // The rename field replaces the label IN PLACE, on this
                // row only. A rail that re-folds while the field is open
                // therefore cannot slide the box onto another session.
                let editing_here = editing
                    .as_ref()
                    .filter(|(id, _)| id == &info.session_id)
                    .map(|(_, text)| text.clone());
                div()
                    .id(SharedString::from(format!("rail-{}", info.session_id)))
                    .debug_selector(|| format!("rail-{}", info.session_id))
                    // The positioning context for Stage AK's action strip.
                    // In-flow children are unaffected, so this changes no
                    // measurement — which the row-height test relies on.
                    .relative()
                    .px(px(theme::RAIL_CARD_BLEED))
                    .py(px(theme::SPACE_2))
                    .rounded(px(theme::RADIUS_BUTTON))
                    // A newly-arrived row fades in (§1.4). Opacity only, and
                    // opacity never participates in layout — the rail's
                    // structure is untouched.
                    .opacity(motion::entrance_t(
                        &format!("in:rail-{}", info.session_id),
                        motion::FADE_QUICK,
                    ))
                    // The selected row keeps its standing fill and takes no
                    // hover step (Slice M): stated as rest == hovered rather
                    // than by dropping the fade, so a selection flip under a
                    // stationary pointer cannot strand a half-faded entry.
                    .hover_fade(
                        format!("rail-{}", info.session_id),
                        if is_sel { theme::overlay_strong() } else { theme::wash_rest() },
                        if is_sel { theme::overlay_strong() } else { theme::overlay() },
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Stage AC: selecting anything closes an open
                        // rename field, discarding it. The field lives in
                        // one row's label slot; leaving it open while the
                        // selection moves would strand a keyboard grab on
                        // a row the operator has walked away from.
                        this.cancel_rename(cx);
                        let _ = this.commands.send(Command::Select(sid.clone()));
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(theme::SPACE_1))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                // Slice N3: a running session's dot works;
                                // every other state keeps the solid dot it
                                // has always had. `quiet`/`failed`/`done`
                                // are states, not activity — a pulsing
                                // danger dot would say "still going" over
                                // a session that is provably dead.
                                loaders::work_dot(
                                    theme::STATE_DOT,
                                    dot,
                                    phase.filter(|_| info.state == "running"),
                                )
                                .flex_none(),
                            )
                            .child(
                                div()
                                    .size(px(theme::PROVIDER_CHIP))
                                    .flex_none()
                                    .rounded(px(theme::RADIUS_CHIP))
                                    .border_1()
                                    .border_color(theme::border_strong())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_family(theme::MONO)
                                    .text_size(px(theme::MICRO_SIZE))
                                    .text_color(theme::text_secondary())
                                    .child(SharedString::from(glyph)),
                            )
                            .child(match editing_here {
                                // Stage AC: the rename field, in the
                                // label's own slot. Same box, same
                                // metrics — see the transparent border on
                                // the label below for why the row does not
                                // wobble when this appears.
                                Some(text) => div()
                                    .id(SharedString::from(format!(
                                        "rename-{}",
                                        info.session_id
                                    )))
                                    .debug_selector(|| format!("rename-{}", info.session_id))
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .border_1()
                                    .border_color(theme::border_strong())
                                    .rounded(px(theme::RADIUS_BUTTON))
                                    .text_size(px(theme::DETAIL_SIZE))
                                    .line_height(px(theme::DETAIL_LINE))
                                    .font_weight(theme::WEIGHT_LABEL)
                                    .text_color(theme::text())
                                    // Clicking inside the field is not a
                                    // click on the row: without this, a
                                    // pointer landing in the box you are
                                    // typing in re-selects the session and
                                    // the field survives only by accident.
                                    .on_click(|_, _, cx| cx.stop_propagation())
                                    // An I-beam, not a hand. The field is
                                    // the one thing in eave's rail that is
                                    // typed into rather than pressed, and
                                    // `every_clickable_element_says_it_is_clickable`
                                    // counts both — its property is that a
                                    // handler always states a cursor, not
                                    // that every cursor is a pointer.
                                    .cursor_text()
                                    // A SOLID caret, deliberately. The
                                    // composer's blink holds a pulse lease
                                    // for as long as it is on screen
                                    // (`caret_lit`), and this field is one
                                    // more surface that would then be
                                    // pinned at the frame rate. N4's idle
                                    // floor is worth more than a blink on
                                    // a box that is open for four seconds.
                                    .child(SharedString::from(format!("{text}▏")))
                                    .into_any_element(),
                                // Stage AB: `min_w(0)` is what makes
                                // `truncate` bite inside a flex row — a
                                // flex item's default minimum is its
                                // content, so without it a long title
                                // grows the row and pushes the badge off
                                // the end instead of clipping itself.
                                None => div()
                                    .debug_selector(|| format!("label-{}", info.session_id))
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    // Stage AC: a border nobody can see,
                                    // carrying the exact height the rename
                                    // field's visible one costs. Opening
                                    // the field must not move the rows
                                    // under it — a list that shifts while
                                    // you are typing into it is the comet
                                    // bug this rail already refuses once,
                                    // in `fold_rail`.
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .rounded(px(theme::RADIUS_BUTTON))
                                    .text_size(px(theme::DETAIL_SIZE))
                                    .line_height(px(theme::DETAIL_LINE))
                                    .font_weight(theme::WEIGHT_LABEL)
                                    .text_color(if is_sel {
                                        theme::text()
                                    } else {
                                        theme::text_secondary()
                                    })
                                    .child(SharedString::from(label))
                                    .into_any_element(),
                            })
                            .children(badge.map(|word| {
                                div()
                                    .debug_selector(|| format!("badge-{}", info.session_id))
                                    .flex_none()
                                    .text_size(px(theme::SMALL_SIZE))
                                    .line_height(px(theme::SMALL_LINE))
                                    .font_weight(theme::WEIGHT_LABEL)
                                    .text_color(dot)
                                    .child(SharedString::from(word))
                            })),
                    )
                    .child(
                        div()
                            .debug_selector(|| format!("cwd-{}", info.session_id))
                            .pl(px(meta_indent))
                            .truncate()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(rail_meta_line(&info, home.as_deref()))),
                    )
                    // Stage AC: an archived row that is on screen says so,
                    // and when it is on screen because it is BLOCKED on
                    // the operator it says that too. A filtered list that
                    // shows a row it claims to be hiding, with nothing on
                    // the row explaining why, reads as a broken filter.
                    .children(archived_word.map(|line| {
                        div()
                            .debug_selector(|| format!("archived-{}", info.session_id))
                            .pl(px(meta_indent))
                            .truncate()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(line))
                    }))
                    // Slice K amendment 3: the posture the CLI reported at
                    // init, stated as a rail fact. Full width and wrapping
                    // for the same reason as the shares line — a posture
                    // that clips to "NEVER AS…" is worse than none.
                    .children(posture.map(|line| {
                        div()
                            .debug_selector(|| format!("posture-{}", info.session_id))
                            .pl(px(meta_indent))
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_weight(if posture_asks {
                                theme::WEIGHT_LABEL
                            } else {
                                theme::WEIGHT_STRONG
                            })
                            .text_color(if posture_asks {
                                theme::text_tertiary()
                            } else {
                                theme::danger()
                            })
                            .child(SharedString::from(line))
                    }))
                    // Slice I2: shared-checkout fact, full row width — at
                    // rail width a fact must never lose to a button. It
                    // WRAPS rather than clips: the list grows with the
                    // number of sharers (two 8-char ids already sit 3 px
                    // from the edge), and a half-stated "shares files
                    // with bbbb2222, cc…" names the wrong set. Found by
                    // the Stage J harness, not by eye.
                    .children(shares.map(|line| {
                        div()
                            .debug_selector(|| format!("shares-{}", info.session_id))
                            .pl(px(meta_indent))
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(line))
                    }))
                    // Slice I2: worktree fact line, also full width.
                    .children(wt_line.map(|line| {
                        div()
                            .debug_selector(|| format!("wt-line-{}", info.session_id))
                            .pl(px(meta_indent))
                            .truncate()
                            .font_family(theme::MONO)
                            .text_size(px(theme::MICRO_SIZE))
                            .text_color(if wt_present {
                                theme::text_tertiary()
                            } else {
                                theme::warning()
                            })
                            .child(SharedString::from(line))
                    }))
                    // The affordance row sits on its own line beneath the
                    // facts it acts on.
                    //
                    // Stage AC: rename and archive appear on the SELECTED
                    // row only. The rail is 248 px wide and its rows are
                    // already three or four lines deep — a permanent
                    // button strip on every one of them would cost more
                    // sessions on screen than the buttons are worth, and
                    // hover-revealed children need per-row state the
                    // rail deliberately does not keep. The selected row is
                    // the row you are managing.
                    // Stage AK — the affordances are ICONS, revealed on
                    // hover, floating over the row's right edge.
                    //
                    // They were five bordered text buttons on a line of
                    // their own, and that line cost every row ~20 px whether
                    // or not anyone wanted to manage that session. Stage AC
                    // said so in its own comment and accepted it, because
                    // "hover-revealed children need per-row state the rail
                    // deliberately does not keep".
                    //
                    // They do not. `motion::hover_t` is keyed, public, and
                    // the row ALREADY registers `rail-{sid}` for its own
                    // wash — so the row's hover progress is a value to read,
                    // not state to invent. Opacity rides the existing
                    // listener's frame lease, so the idle floor is untouched
                    // and no new lease exists to leak.
                    //
                    // `.absolute()` is the other half: an overlay strip
                    // costs NO height, so removing the old line makes every
                    // row shorter and the wrap that R1's fixed-height bug
                    // taught us to fear cannot happen — there is nothing
                    // left to wrap.
                    //
                    // Mounted only once hover has begun rather than always
                    // at opacity 0. An invisible control that still takes
                    // clicks is a trap, and the row's right edge is
                    // somewhere a person may reasonably click to SELECT.
                    // `hover_t` is exactly 0 when the pointer is elsewhere,
                    // so this gate costs one frame at the start of a fade
                    // and removes the misfire entirely.
                    .when(can_isolate || show_wt_buttons || is_sel, |d| {
                        let reveal = motion::hover_t(&format!("rail-{}", info.session_id));
                        d.when(reveal > 0.0, |d| {
                            d.child(
                                div()
                                    .absolute()
                                    // The row's top-right CORNER, not its
                                    // vertical middle: centred, the strip
                                    // floats beside the id/path line and
                                    // reads as part of the row's content.
                                    // Pinned to the corner it reads as
                                    // chrome, which is what it is.
                                    .top(px(theme::SPACE_1))
                                    .right(px(theme::SPACE_1))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(theme::SPACE_1))
                                    .opacity(reveal)
                                    .when(is_sel, |d| {
                                        d.child(
                                            row_action(
                                                format!("ren-{}", info.session_id),
                                                icons::PEN,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.begin_rename(sid_ren.clone(), cx);
                                            })),
                                        )
                                        .child(
                                            row_action(
                                                format!("arc-{}", info.session_id),
                                                if archive_to {
                                                    icons::ARCHIVE_MINIMALISTIC
                                                } else {
                                                    icons::ARCHIVE_UP_MINIMALISTIC
                                                },
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                // Deliberately NOT a two-step
                                                // like `delete`. Nothing is
                                                // destroyed — the same button,
                                                // one click, brings it back —
                                                // so a confirmation here would
                                                // be ceremony with no fact
                                                // behind it.
                                                let _ = this.commands.send(Command::SetArchived {
                                                    id: sid_arc.clone(),
                                                    archived: archive_to,
                                                });
                                            })),
                                        )
                                    })
                                    .when(can_isolate, |d| {
                                        d.child(
                                            row_action(
                                                format!("iso-{}", info.session_id),
                                                icons::GIT_BRANCH,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                // An affordance acts on the
                                                // row; it does not select it
                                                // (amendment 2 — selection is
                                                // the row's own click).
                                                cx.stop_propagation();
                                                let _ = this
                                                    .commands
                                                    .send(Command::Isolate(sid_iso.clone()));
                                            })),
                                        )
                                    })
                                    .when(show_wt_buttons && wt_present, |d| {
                                        d.child(
                                            row_action(
                                                format!("wto-{}", info.session_id),
                                                icons::FOLDER,
                                            )
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                open_in_system(std::path::Path::new(&wt_path));
                                            }),
                                        )
                                    })
                                    .when(show_wt_buttons, |d| {
                                        d.child(
                                            row_action(
                                                format!("wtd-{}", info.session_id),
                                                icons::TRASH_BIN_MINIMALISTIC,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                let _ = this.commands.send(
                                                    Command::RemoveWorktree {
                                                        id: sid_del.clone(),
                                                        force: armed,
                                                    },
                                                );
                                            })),
                                        )
                                    }),
                            )
                        })
                    })
                    // A standing worktree note: the armed confirmation's
                    // restated counts, or the engine's stated refusal.
                    .children(note_line.map(|(line, color)| {
                        div()
                            .debug_selector(|| format!("wt-note-{}", info.session_id))
                            .pl(px(meta_indent))
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(color)
                            .child(SharedString::from(line))
                    }))
                    .into_any_element()
            })
            .collect();
        // Slice M (§3.1): an empty rail rendered literally nothing, so a
        // first run and a rail that failed to load looked identical. One
        // clause, the fact only — the action is the `+ new` button eight
        // pixels above it, and restating a visible button is noise.
        let rows: Vec<AnyElement> = if rows.is_empty() {
            vec![div()
                .px(px(theme::RAIL_GUTTER))
                .py(px(theme::SPACE_1))
                .child(
                    div()
                        .debug_selector(|| "rail-empty".into())
                        .text_size(px(theme::SMALL_SIZE))
                        .line_height(px(theme::SMALL_LINE))
                        .text_color(theme::text_tertiary())
                        // Stage AC: "no sessions" over a rail whose
                        // sessions are all filed away is an empty state
                        // that lost its reason. `rail_empty_note` keeps
                        // the reason on screen, and the toggle that undoes
                        // it is the line directly above.
                        .child(SharedString::from(rail_empty_note(total, archived_count))),
                )
                .into_any_element()]
        } else {
            rows
        };
        // No surface of its own: rail rows sit directly on the glass
        // ground (comet's shell model, 2026-08-17). The content well
        // beside it is the differentiated card; painting the rail would
        // put a plate where the frost should read through.
        div()
            .w(px(theme::RAIL_W))
            .h_full()
            .flex_none()
            .pl(px(theme::PLATE_MARGIN))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(theme::RAIL_GUTTER))
                    .pt(px(theme::SPACE_3))
                    .pb(px(theme::SPACE_2))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_weight(theme::WEIGHT_LABEL)
                            .debug_selector(|| "rail-title".into())
                            .text_color(theme::text_tertiary())
                            .child("sessions"),
                    )
                    .child(
                        div()
                            .id("new-session")
                            .debug_selector(|| "new-session".into())
                            .px(px(theme::SPACE_2))
                            .py(px(theme::SPACE_1))
                            .rounded(px(theme::RADIUS_BUTTON))
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_secondary())
                            .hover_fade("new-session", theme::wash_rest(), theme::overlay())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, _| this.new_session()))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(theme::SPACE_1))
                            .child(icons::tagged_icon(
                                icons::PEN_NEW_SQUARE,
                                theme::ICON_SM,
                                theme::text_secondary(),
                                "new-session-glyph",
                            ))
                            .child("new"),
                    ),
            )
            // Stage AC: the archived shelf, on its own line rather than
            // squeezed into the header beside `new`. At 248 px the header
            // has ~212 px of room and "sessions" plus `new` already take
            // most of it — a third control up there would be a contest
            // between a fact and a button, which is the contest a fact
            // loses. Present only when there is something filed away, so a
            // rail nobody has archived from looks exactly as it did.
            .children((archived_count > 0).then(|| {
                div()
                    .flex_none()
                    .px(px(theme::RAIL_GUTTER))
                    .pb(px(theme::SPACE_2))
                    .child(
                        div()
                            .id("rail-archived")
                            .debug_selector(|| "rail-archived".into())
                            .px(px(theme::SPACE_2))
                            .py(px(theme::SPACE_1))
                            .rounded(px(theme::RADIUS_BUTTON))
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .hover_fade("rail-archived", theme::wash_rest(), theme::overlay())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sessions.update(cx, |state, cx| {
                                    state.show_archived = !state.show_archived;
                                    cx.notify();
                                });
                            }))
                            .child(SharedString::from(archived_toggle_label(
                                archived_count,
                                show_archived,
                            ))),
                    )
            }))
            // Slice M: rows dissolve at the scroller's edges instead of
            // ending on a hard cut. The rail paints no surface of its
            // own, so there is nothing to cut against BUT the desktop
            // showing through the glass — which is exactly the case a
            // painted gradient cannot handle and `EdgeFade` can. Gated on
            // overflow (the rail has no resting top pad), so a rail whose
            // sessions all fit looks exactly as it did before.
            .child(
                edge_faded(
                    theme::RAIL_FADE_BAND,
                    true,
                    true,
                    div()
                        .id("rail-rows")
                        .debug_selector(|| "rail-rows".into())
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.rail_scroll)
                        .pr(px(theme::RAIL_CARD_BLEED))
                        .pb(px(theme::SPACE_2))
                        .flex()
                        .flex_col()
                        .gap(px(theme::SPACE_1))
                        .children(rows),
                )
                .fade_overflow_y(&self.rail_scroll),
            )
            // Stage L: the aggregate resource fact, under the list it
            // aggregates and outside the scroller — a ceiling the
            // operator has to scroll to find is a ceiling they meet by
            // surprise. Absent entirely when nothing live reports memory.
            .children(resource.map(|(line, over)| {
                div()
                    .flex_none()
                    .px(px(theme::RAIL_GUTTER))
                    .pb(px(theme::SPACE_3))
                    .pt(px(theme::SPACE_1))
                    .child(
                        div()
                            .debug_selector(|| "rail-resource".into())
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_weight(if over {
                                theme::WEIGHT_LABEL
                            } else {
                                theme::WEIGHT_BODY
                            })
                            .text_color(if over {
                                theme::warning()
                            } else {
                                theme::text_tertiary()
                            })
                            .child(SharedString::from(line)),
                    )
            }))
            .into_any_element()
    }

    /// The files rail (Slice B1): rows on the glass ground, right of the
    /// content card —
    /// the selected session's changed files as git states them. Every
    /// body is a stated fact: not-a-repo, a git failure, and a clean repo
    /// all render as plain words, never as an error or an empty void.
    /// Session-tagged like the attention overlay: data from a deselected
    /// session never renders.
    fn files_rail(&self, cx: &Context<Self>, phase: Option<f32>) -> AnyElement {
        let selected = self.transcript.read(cx).session_id.clone();
        let held = self.files.read(cx).snapshot.clone();
        // Some(None) = engine refused; Some(Some) = snapshot; None = no
        // data for the selected session yet.
        let current: Option<Option<FilesSnapshot>> = match (held, selected.clone()) {
            (Some((sid, snap)), Some(sel)) if sid == sel => Some(snap),
            _ => None,
        };
        // The expanded diff, only if it belongs to the selected session
        // (a deselected session's diff never renders — same tag rule as
        // the snapshot itself).
        let diff_slot: Option<(String, DiffFetch)> = {
            let st = self.files.read(cx);
            st.diff.as_ref().and_then(|s| {
                (selected.as_deref() == Some(s.session.as_str()))
                    .then(|| (s.path.clone(), s.fetch.clone()))
            })
        };
        let fact = |text: &str| {
            div()
                .px(px(theme::SPACE_2))
                .text_size(px(theme::DETAIL_SIZE))
                .line_height(px(theme::DETAIL_LINE))
                .text_color(theme::text_tertiary())
                .child(SharedString::from(text.to_string()))
                .into_any_element()
        };
        let (count_label, body): (Option<String>, AnyElement) = match &current {
            // Slice N3: the snapshot has not arrived yet. The single
            // character `…` was the same picture whether it was seconds
            // away or never coming; skeleton rows at least say a LIST is
            // what is coming, in the rhythm it will arrive in.
            //
            // Slice AH: and now they stop saying it when it stops being
            // true. `files_wait` is the same answer the pulse gate reads,
            // so the skeleton is on screen exactly while a lease is held
            // and never one frame longer.
            None => match self.files_wait(cx) {
                FilesWait::Silent(reason) => (None, self.files_silence(&reason, cx)),
                FilesWait::Answered | FilesWait::Waiting => (
                    None,
                    div()
                        .px(px(theme::SPACE_2))
                        .child(
                            loaders::skeleton(theme::SKELETON_ROWS, phase)
                                .debug_selector(|| "files-skeleton".into()),
                        )
                        .into_any_element(),
                ),
            },
            Some(None) => (None, fact("file status unavailable — engine predates it")),
            // Slice AA: a basis we cannot measure from outranks anything
            // the scan says, because the scan says nothing. Rendering
            // this as `GitState::Ok` with an empty list would put "this
            // turn changed nothing" on screen when the truth is "there is
            // no turn to compare against" — the one confusion this whole
            // slice exists to prevent. No count either: zero would be a
            // claim.
            Some(Some(FilesSnapshot {
                basis: DiffBasis::Unavailable { reason },
                ..
            })) => (None, fact(reason)),
            Some(Some(snap)) => match &snap.state {
                GitState::NotARepo => (None, fact("not a git repository")),
                // One arm for every state that has something to say, so a
                // new `GitState` cannot reach the rail with the app
                // inventing wording for it — which is how this surface
                // ended up double-framing a timeout as a git failure.
                state @ (GitState::Error { .. } | GitState::Unanswered { .. }) => {
                    let said = state.stated();
                    let remedy = state.remedy();
                    (
                        None,
                        div()
                            .px(px(theme::SPACE_2))
                            .flex()
                            .flex_col()
                            .gap(px(theme::SPACE_1))
                            .text_size(px(theme::DETAIL_SIZE))
                            .line_height(px(theme::DETAIL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(said))
                            .children(remedy.map(|r| {
                                div()
                                    .debug_selector(|| "git-remedy".into())
                                    .child(SharedString::from(r))
                            }))
                            .into_any_element(),
                    )
                }
                // An empty list under a basis we DO have is a real zero,
                // and it says which zero it is: nothing uncommitted at
                // all, or a turn that has not written anything yet.
                GitState::Ok if snap.files.is_empty() => (
                    Some("0".into()),
                    fact(match &snap.basis {
                        DiffBasis::Turn { .. } => "this turn has changed nothing yet",
                        _ => "no changes",
                    }),
                ),
                GitState::Ok => {
                    let mut rows: Vec<AnyElement> = Vec::new();
                    for f in &snap.files {
                        let expanded = matches!(&diff_slot, Some((p, _)) if p.as_str() == f.path);
                        rows.push(self.file_row(f, expanded, cx));
                        if expanded {
                            if let Some((_, fetch)) = &diff_slot {
                                rows.push(diff_pane(f, fetch, phase));
                            }
                        }
                    }
                    // The list cap is a stated fact, never silence.
                    if snap.omitted > 0 {
                        rows.push(fact(&format!("… and {} more files", snap.omitted)));
                    }
                    let label = if snap.omitted > 0 {
                        format!(
                            "{} of {}",
                            snap.files.len(),
                            snap.files.len() as u64 + snap.omitted
                        )
                    } else {
                        snap.files.len().to_string()
                    };
                    (
                        Some(label),
                        div().flex().flex_col().children(rows).into_any_element(),
                    )
                }
            },
        };
        // The refresh mode is a stated fact (directive 2026-08-17: the
        // watcher degrades silently — a rail that looks live while
        // polling would lie by omission). Only stated when there is a
        // scanned repo to keep fresh; quiet ghost text, not an alarm.
        // Slice AA: silent while the basis is unavailable — the body is
        // already explaining why there is nothing, and "watching file
        // events" underneath it would describe machinery, not the answer.
        let scope = self.files.read(cx).scope;
        let mode: Option<&'static str> = match &current {
            // An engine predating Slice AA ignores the `scope` field and
            // streams the working tree regardless — serde drops unknown
            // params rather than refusing them. The toggle would then read
            // as a lie, so the disagreement is stated: the snapshot's own
            // basis is the authority on what arrived, never the request.
            Some(Some(snap))
                if scope == FileScope::LatestTurn
                    && matches!(snap.basis, DiffBasis::WorkingTree) =>
            {
                Some("engine predates turn scope — showing the whole working tree")
            }
            Some(Some(snap))
                if matches!(snap.state, GitState::Ok)
                    && !matches!(snap.basis, DiffBasis::Unavailable { .. }) =>
            {
                Some(if snap.watching {
                    "watching file events"
                } else {
                    "refreshes at turn end"
                })
            }
            _ => None,
        };
        // Rows on the bare glass ground, mirroring rail() — no plate.
        div()
            .w(px(theme::FILES_RAIL_W))
            .h_full()
            .flex_none()
            .pr(px(theme::PLATE_MARGIN))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(theme::RAIL_GUTTER))
                    .pt(px(theme::SPACE_3))
                    .pb(px(theme::SPACE_2))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_weight(theme::WEIGHT_LABEL)
                            .debug_selector(|| "files-title".into())
                            .text_color(theme::text_tertiary())
                            .child("changes"),
                    )
                    .children(count_label.map(|n| {
                        div()
                            .debug_selector(|| "files-count".into())
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_family(theme::MONO)
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(n.to_string()))
                    })),
            )
            // Slice AA: the basis toggle. Its own row rather than a third
            // item in the header — the labels are words, not glyphs, and
            // squeezing them beside the title and the count is what
            // produced the overlapping menu rows in R1.
            .child(
                div()
                    .flex_none()
                    .px(px(theme::RAIL_GUTTER))
                    .pb(px(theme::SPACE_2))
                    .flex()
                    .flex_row()
                    .gap(px(theme::SPACE_1))
                    .child(self.scope_segment(FileScope::WorkingTree, "all", scope, cx))
                    .child(self.scope_segment(FileScope::LatestTurn, "this turn", scope, cx)),
            )
            // Slice M: same edge fade as the session rail, same reason.
            .child(
                edge_faded(
                    theme::RAIL_FADE_BAND,
                    true,
                    true,
                    div()
                        .id("files-rows")
                        .debug_selector(|| "files-rows".into())
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.files_scroll)
                        .pr(px(theme::RAIL_CARD_BLEED))
                        .pb(px(theme::SPACE_2))
                        .child(body),
                )
                .fade_overflow_y(&self.files_scroll),
            )
            .children(mode.map(|text| {
                div()
                    .flex_none()
                    .px(px(theme::RAIL_GUTTER))
                    .pb(px(theme::SPACE_2))
                    .child(
                        div()
                            .debug_selector(|| "files-mode".into())
                            .text_size(px(theme::DETAIL_SIZE))
                            .line_height(px(theme::DETAIL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(text),
                    )
            }))
            .into_any_element()
    }

    /// The files rail when nothing answered (Slice AH): the error the
    /// eternal skeleton had lost, plus an affordance where one can
    /// actually do something.
    ///
    /// Written in the rail's own `fact` voice — quiet tertiary words on
    /// the bare ground, the same as "not a git repository" and "no
    /// changes" — because it is the same kind of statement and a louder
    /// one would read as an alarm about the engine that this window is not
    /// in a position to raise.
    ///
    /// **`try again` appears only while the connection is live.** It sends
    /// `Command::Resubscribe`, which the feed loop services inside its
    /// session loop; after the 5-attempt breaker that loop has returned,
    /// so the button would be an affordance that silently does nothing —
    /// the exact defect Stage AC widened the launcher's command match to
    /// prevent. A stated fact with no button beats a button that lies.
    fn files_silence(&self, reason: &str, cx: &Context<Self>) -> AnyElement {
        let live = matches!(self.conn.read(cx).status, ConnEvent::Connected { .. });
        div()
            .px(px(theme::SPACE_2))
            .flex()
            .flex_col()
            .gap(px(theme::SPACE_1))
            .child(
                div()
                    .debug_selector(|| "files-silence".into())
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .text_color(theme::text_tertiary())
                    .child(SharedString::from(reason.to_string())),
            )
            .children(live.then(|| {
                div()
                    .id("files-retry")
                    .debug_selector(|| "files-retry".into())
                    .w_full()
                    .px(px(theme::SPACE_1))
                    .py(px(theme::SPACE_1))
                    .rounded(px(theme::RADIUS_CHIP))
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    // `text()`, not `text_secondary()`, and the reason is
                    // this session's other slice: a quiet tier UNDER a
                    // wash is the defect Slice AG exists to fix
                    // (`text_secondary` over `overlay` is 3.81:1, and
                    // this element's rest state fades up to exactly that
                    // wash on hover). The full tier is 7.00:1 over the
                    // same ground, and it is also the right weight for
                    // the one thing on this surface you can act on —
                    // `scope_segment` reaches for the same tier for the
                    // same reason.
                    .text_color(theme::text())
                    .hover_fade("files-retry", theme::wash_rest(), theme::overlay())
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Restart the patience with the request, or the
                        // rail would re-state the old silence on the very
                        // next frame and the button would read as broken.
                        this.files_wait_since.set(None);
                        let _ = this.commands.send(Command::Resubscribe);
                        cx.notify();
                    }))
                    .child("try again")
            }))
            .into_any_element()
    }

    /// One changed file: status glyph in a fixed slot, path, +/− counts.
    /// Counts wear the existing state pigments (adds success, removals
    /// danger). B2: click toggles the file's inline diff; ⌘-click opens
    /// the file in the system editor (nothing to open for a deleted
    /// file — the click is ignored, not an error).
    /// The path renders as truncating-directory + always-visible
    /// basename: the filename is the identity and must survive any
    /// width; the directory is context and may lose its tail.
    fn file_row(&self, f: &ChangedFile, expanded: bool, cx: &Context<Self>) -> AnyElement {
        let counts = count_labels(f.adds, f.dels);
        let (dir, base) = match f.path.rfind('/') {
            Some(ix) => (f.path[..=ix].to_string(), f.path[ix + 1..].to_string()),
            None => (String::new(), f.path.clone()),
        };
        let path = f.path.clone();
        let status = f.status;
        div()
            .id(SharedString::from(format!("file-{}", f.path)))
            .debug_selector(|| format!("file-{}", f.path))
            .px(px(theme::RAIL_CARD_BLEED))
            .py(px(theme::SPACE_1))
            .rounded(px(theme::RADIUS_BUTTON))
            // §1.4: a file that has just appeared in the rail fades in.
            // The path is the key, so a snapshot replaced by an FS event
            // does not re-fade rows that were already there.
            .opacity(motion::entrance_t(
                &format!("in:file-{}", f.path),
                motion::FADE_QUICK,
            ))
            // An expanded row already rests at the wash, so its hover step
            // is the same colour — rest == hovered, no flicker on expand.
            .hover_fade(
                format!("file-{}", f.path),
                if expanded {
                    theme::overlay()
                } else {
                    theme::wash_rest()
                },
                theme::overlay(),
            )
            .cursor_pointer()
            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                if ev.modifiers().platform {
                    if status != FileStatus::Deleted {
                        if let Some(cwd) = this.selected_cwd(cx) {
                            open_in_system(&cwd.join(&path));
                        }
                    }
                    return;
                }
                this.toggle_diff(path.clone(), cx);
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                // Slice P3: the slot keeps its width so paths stay aligned
                // exactly where the letter left them; only its contents
                // changed. See `status_icon` for what the letters became.
                div()
                    .w(px(theme::FILE_STATUS_W))
                    .h(px(theme::SMALL_LINE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icons::tagged_icon(
                        status_icon(f.status),
                        theme::ICON_STATUS,
                        theme::text_tertiary(),
                        format!("file-status-{}", f.path),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .when(!dir.is_empty(), |d| {
                        d.child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(theme::text_tertiary())
                                .child(SharedString::from(dir)),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::text_secondary())
                            .child(SharedString::from(base)),
                    ),
            )
            .child(match counts {
                Some((adds, dels)) => div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .font_family(theme::MONO)
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .child(
                        div()
                            .text_color(theme::success())
                            .child(SharedString::from(adds)),
                    )
                    .child(
                        div()
                            .text_color(theme::danger())
                            .child(SharedString::from(dels)),
                    )
                    .into_any_element(),
                // Unknowable (binary / oversized): one honest dash.
                None => div()
                    .flex_none()
                    .font_family(theme::MONO)
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .text_color(theme::text_tertiary())
                    .child("—")
                    .into_any_element(),
            })
            .into_any_element()
    }

    /// Toggle one file's inline diff (B2). Opening requests the content
    /// (unary; the reply folds in via the diffs queue); the request is
    /// re-issued by the pump on every fresh snapshot while the row stays
    /// expanded, so the pane tracks the worktree without any engine-side
    /// diff cache.
    fn toggle_diff(&self, path: String, cx: &mut Context<Self>) {
        let Some(sid) = self.transcript.read(cx).session_id.clone() else {
            return;
        };
        let same = matches!(
            self.files.read(cx).diff.as_ref(),
            Some(slot) if slot.session == sid && slot.path == path
        );
        self.files.update(cx, |state, cx| {
            state.diff = if same {
                None
            } else {
                Some(DiffSlot {
                    session: sid,
                    path: path.clone(),
                    fetch: DiffFetch::Loading,
                })
            };
            cx.notify();
        });
        if !same {
            let _ = self.commands.send(Command::Diff(path));
        }
    }

    /// Switch the files rail's basis (Slice AA).
    ///
    /// The held snapshot is dropped, not kept as a placeholder: it
    /// describes the OTHER basis, and leaving it on screen under the new
    /// label would be the one lie this slice is about. The skeleton says
    /// "a list is coming" and is the honest picture for the round trip.
    /// The expanded diff goes with it — a row that may not exist in the
    /// new list cannot keep a pane open under it.
    fn set_files_scope(&self, scope: FileScope, cx: &mut Context<Self>) {
        if self.files.read(cx).scope == scope {
            return;
        }
        self.files.update(cx, |state, cx| {
            state.scope = scope;
            state.snapshot = None;
            state.diff = None;
            cx.notify();
        });
        // Slice AH: a toggle is a fresh request, so it gets fresh patience.
        // Without this a rail that had already given up on one basis would
        // state that silence over the other one before it had been asked.
        self.files_wait_since.set(None);
        let _ = self.commands.send(Command::FilesScope(scope));
    }

    /// One segment of the files rail's basis toggle (Slice AA).
    fn scope_segment(
        &self,
        scope: FileScope,
        label: &'static str,
        selected: FileScope,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_sel = scope == selected;
        let name = match scope {
            FileScope::WorkingTree => "working-tree",
            FileScope::LatestTurn => "latest-turn",
        };
        div()
            .id(SharedString::from(format!("files-scope-{name}")))
            .debug_selector(move || format!("files-scope-{name}"))
            .flex_1()
            .min_w(px(0.))
            .px(px(theme::SPACE_1))
            .py(px(theme::SPACE_1))
            .rounded(px(theme::RADIUS_CHIP))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(theme::SMALL_SIZE))
            .line_height(px(theme::SMALL_LINE))
            // One wash, two roles: the selected segment RESTS at
            // `overlay()`, the other rests invisible and fades up to it.
            // So hovering an unselected segment previews exactly what
            // selecting it will look like, and selection needs no second
            // pigment of its own.
            .hover_fade(
                format!("files-scope-{name}"),
                if is_sel {
                    theme::overlay()
                } else {
                    theme::wash_rest()
                },
                theme::overlay(),
            )
            .when(is_sel, |d| {
                d.font_weight(theme::WEIGHT_LABEL).text_color(theme::text())
            })
            .when(!is_sel, |d| d.text_color(theme::text_tertiary()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.set_files_scope(scope, cx)))
            .child(label)
            .into_any_element()
    }

    /// The selected session's cwd, for resolving a repo-relative path to
    /// something the system `open` can take.
    fn selected_cwd(&self, cx: &Context<Self>) -> Option<PathBuf> {
        let sid = self.transcript.read(cx).session_id.clone()?;
        self.sessions
            .read(cx)
            .sessions
            .iter()
            .find(|s| s.session_id == sid)
            .map(|s| PathBuf::from(&s.cwd))
    }

    /// Footer status bar: connection state as a dot + plain fact, session
    /// on the right. Rare loud states (busy/failed) additionally get a
    /// panel from [`Self::conn_panel`].
    fn footer(&self, cx: &Context<Self>) -> gpui::Div {
        let (dot, label): (Hsla, String) = match &self.conn.read(cx).status {
            ConnEvent::Connecting => (theme::warning(), "connecting to engine…".into()),
            ConnEvent::Connected { engine_pid } => {
                (theme::success(), format!("engine {engine_pid}"))
            }
            ConnEvent::Reconnecting { attempt } => (
                theme::warning(),
                format!("reconnecting (attempt {attempt})…"),
            ),
            ConnEvent::Busy { .. } => (theme::danger(), "engine busy".into()),
            ConnEvent::Failed { .. } => (theme::danger(), "engine unreachable".into()),
        };
        let session = self.transcript.read(cx).session_id.clone();
        div()
            .w_full()
            .flex_none()
            .border_t_1()
            .border_color(theme::border())
            .pt(px(theme::SPACE_1))
            .pb(px(theme::SPACE_3))
            .child(Self::column(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .text_color(theme::text_tertiary())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(theme::STATE_DOT)).rounded_full().bg(dot))
                            .child(label)
                            .children(self.friction.as_ref().map(|(_, note)| {
                                div()
                                    .text_color(theme::text_tertiary())
                                    .child(SharedString::from(note.clone()))
                            })),
                    )
                    .children(session.map(|s| div().text_color(theme::text_tertiary()).child(s))),
            ))
    }

    /// Loud connection states get a real panel above the composer with the
    /// full actionable message; the footer only carries the short fact.
    ///
    /// It mounts and unmounts on a state change, which is exactly the shape
    /// gpui's element-id-keyed animation clock replays on — so its §1.3
    /// entrance is hand-driven from a keyed store (`motion::entrance_t`).
    fn conn_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let message = match &self.conn.read(cx).status {
            ConnEvent::Busy { holder_pid } => match holder_pid {
                Some(pid) => format!(
                    "another eave instance holds this data dir (pid {pid}) — quit it or use a different --data-dir"
                ),
                None => "another eave instance holds this data dir (holder unknown) — quit it or use a different --data-dir".into(),
            },
            ConnEvent::Failed { message } => message.clone(),
            _ => return None,
        };
        let t = motion::entrance_t("in:conn-panel", motion::FADE_IN);
        Some(
            Self::column(frost(
                theme::RADIUS_PANEL,
                div()
                    .w_full()
                    .rounded(px(theme::RADIUS_PANEL))
                    .border_1()
                    .border_color(theme::danger_ring())
                    .bg(theme::danger_plate_glass())
                    .shadow(theme::shadow_panel())
                    .px(px(theme::SPACE_3))
                    .py(px(theme::SPACE_2))
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .text_color(theme::text_secondary())
                    .child(
                        div()
                            .font_weight(theme::WEIGHT_LABEL)
                            .text_color(theme::danger())
                            .child("engine unavailable"),
                    )
                    .child(message),
            ))
            .pb(px(theme::SPACE_2))
            .opacity(t)
            // §1.3's rise, as a RELATIVE inset: taffy resolves relative
            // insets after layout, so this slides the panel the way a CSS
            // transform would and the composer below it never moves.
            .relative()
            .top(px(motion::ENTRANCE_RISE * (1.0 - t)))
            .into_any_element(),
        )
    }

    /// What the files rail knows about the selected session's list, and
    /// therefore both whether it may animate and what it must say
    /// (Slice AH).
    ///
    /// **The defect this closes.** The gate used to ask "is there a
    /// snapshot for the selected session?" and answer "no" forever. A
    /// skeleton that never resolves looks like a skeleton, which looks
    /// like loading, which looks fine — so nobody notices by looking, and
    /// the tell is CPU: the rail held a pulse lease for the life of the
    /// process. An eternal skeleton is an error state that lost its error.
    ///
    /// **Two kinds of evidence, and the stronger one first.** If the
    /// connection itself says no answer is coming, that is a fact and it
    /// is stated at once with no clock involved ([`conn_silence`]). Only
    /// when the connection is live — so the only evidence available is
    /// elapsed time — does [`FILES_PATIENCE`] apply, and what it then
    /// states is what this window actually knows ("nothing answered in
    /// 20s"), never a diagnosis of the engine it cannot make.
    ///
    /// **Idempotent.** Both the gate and the render call it on the same
    /// frame; the second call reads the wait the first one started, so the
    /// picture and the timer can never disagree about whether a list is on
    /// its way. That disagreement was the whole bug.
    ///
    /// A COLLAPSED rail calls it from neither — `work_in_flight`
    /// short-circuits on `collapsed` and `files_rail` is not built — so a
    /// rail nobody can see costs nothing and starts its patience fresh
    /// when it is opened. That is the right reading: the wait is about a
    /// picture, and a hidden picture is not waiting for anything.
    fn files_wait(&self, cx: &Context<Self>) -> FilesWait {
        let Some(selected) = self.transcript.read(cx).session_id.clone() else {
            self.files_wait_since.set(None);
            return FilesWait::Answered;
        };
        // `Some((sid, None))` counts as arrived: "the engine refused this
        // method" is an answer. Only an absent tag, or one belonging to a
        // session that is no longer selected, is a wait.
        if matches!(&self.files.read(cx).snapshot, Some((sid, _)) if sid == &selected) {
            self.files_wait_since.set(None);
            return FilesWait::Answered;
        }
        if let Some(reason) = conn_silence(&self.conn.read(cx).status) {
            self.files_wait_since.set(None);
            return FilesWait::Silent(reason);
        }
        let now = cx.background_executor().now();
        let since = match self.files_wait_since.take() {
            // A wait belongs to one session: switching restarts it, which
            // is why the tag is stored beside the instant.
            Some((sid, started)) if sid == selected => started,
            _ => now,
        };
        self.files_wait_since.set(Some((selected, since)));
        if now.saturating_duration_since(since) >= FILES_PATIENCE {
            FilesWait::Silent(format!(
                "no file list — nothing answered in {}s",
                FILES_PATIENCE.as_secs()
            ))
        } else {
            FilesWait::Waiting
        }
    }

    /// Is anything in this window mid-flight this frame? (Slice N3.)
    ///
    /// The ONE place that decides whether the pulse clock gets a lease,
    /// and therefore whether the window schedules any work at all. Every
    /// arm names the render site it answers for, because a gate that
    /// drifts from its sites fails in one of two directions: under-eager
    /// freezes an indicator mid-wave, over-eager leaves a 30 fps timer
    /// running over a still picture. Over-eager is the safe side and this
    /// is deliberately a superset — a site that draws nothing costs a
    /// timer, a site that draws a frozen loader is a bug report.
    fn work_in_flight(&self, cx: &Context<Self>) -> bool {
        // The rail's state dots.
        if self
            .sessions
            .read(cx)
            .sessions
            .iter()
            .any(|s| s.state == "running")
        {
            return true;
        }
        let files = self.files.read(cx);
        // The files rail: a selected session whose snapshot has not
        // arrived (the skeleton rows).
        //
        // **Slice AH.** This arm used to be "selected, and no snapshot",
        // full stop — an unbounded predicate, so a subscription nobody
        // ever answered held the lease for the life of the process and
        // eave sat at 30 fps drawing a picture that said a list was
        // coming. The bound is in `files_wait`, which is also what the
        // rail renders from, so the gate and the picture can no longer
        // disagree about whether anything is on its way.
        if !files.collapsed && self.files_wait(cx) == FilesWait::Waiting {
            return true;
        }
        // The diff pane's own load.
        if matches!(&files.diff, Some(d) if matches!(d.fetch, DiffFetch::Loading)) {
            return true;
        }
        let state = self.transcript.read(cx);
        // The working trailer.
        if state.in_turn() {
            return true;
        }
        // A running tool row, or a live message with nothing in it yet.
        // Reverse order: both live at the tail of the log, so the scan
        // that finds something exits at once and only a settled log is
        // walked in full.
        state.transcript.rows.iter().rev().any(|r| match r {
            Row::Tool { status, .. } => matches!(status, ToolStatus::Running),
            Row::Assistant { live, .. } => *live,
            _ => false,
        })
    }

    /// The working trailer (Slice N3, inventory §3.6): what a healthy
    /// running turn looks like.
    ///
    /// Until this slice eave had **no in-flow indicator for one at all**.
    /// A turn that was working and a turn that was wedged rendered
    /// identically, and an elapsed time only appeared once the engine had
    /// already escalated the session to `quiet` — which is to say, only
    /// after something had gone wrong.
    ///
    /// It sits in the floating chrome stack, in the same slot the
    /// attention notice uses, and only when that notice is absent. Two
    /// consequences, both intended: the operator reads one sentence in
    /// one place and one voice ("working for 12s" → "quiet for 30s"),
    /// and — because the stack is a SIBLING of the virtualized list,
    /// anchored to the bottom of the well — the trailer can never
    /// participate in a row's cached height or move the composer.
    ///
    /// No flavour word. comet rotates a 20-word vocabulary here every 7 s
    /// (Thinking, Pondering, Scheming…); eave's register is instrument,
    /// so the trailer states the fact and the elapsed time and nothing
    /// else (report §7.1 on the two voices).
    fn work_trailer(&self, phase: Option<f32>, cx: &Context<Self>) -> Option<AnyElement> {
        let state = self.transcript.read(cx);
        if !state.in_turn() {
            return None;
        }
        // No `UserSent` seen means no honest clock to report; the
        // indicator alone still states that something is running. Never
        // invent a start time from when this process attached.
        let elapsed = state
            .turn_started_ms
            .map(|started| fmt_elapsed(eave_core::journal::now_ms().saturating_sub(started)));
        Some(
            Self::column(
                div()
                    .debug_selector(|| "work-trailer".into())
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pb(px(theme::SPACE_2))
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .text_color(theme::text_tertiary())
                    .child(loaders::work_bar(theme::live(), phase))
                    .child(SharedString::from(match elapsed {
                        Some(e) => format!("working for {e}"),
                        None => "working".to_string(),
                    })),
            )
            .into_any_element(),
        )
    }

    /// The attention notice (spec 01 §States): `quiet` escalates subtle →
    /// prominent and reports elapsed steadily; `unresponsive` and `failed`
    /// are prominent panels with the evidence/cause and a kill affordance.
    /// Nothing here ever auto-dismisses or auto-kills — notices persist
    /// until the engine reclassifies or the user acts.
    ///
    /// Like [`conn_panel`](Self::conn_panel) it carries a hand-driven §1.3
    /// entrance. The key is the SESSION's, not a constant: a notice that
    /// moves from one session to another is a different notice arriving,
    /// and it should enter rather than cross-fade in place.
    fn attention_notice(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (attn_session, attention) = self.sessions.read(cx).attention.clone()?;
        // A notice belongs to one session; never render it over another
        // (a switch mid-notice must not carry the panel across).
        if self.transcript.read(cx).session_id.as_deref() != Some(attn_session.as_str()) {
            return None;
        }
        // Is the question the notice is about already on screen, with its
        // own buttons? (Slice K — see the `AwaitingApproval` arm below.)
        let question_on_screen = self
            .transcript
            .read(cx)
            .transcript
            .pending_approvals()
            .next()
            .is_some();
        let now = eave_core::journal::now_ms();
        let panel = |ring: Hsla, wash: Hsla| {
            div()
                .w_full()
                .rounded(px(theme::RADIUS_PANEL))
                .border_1()
                .border_color(ring)
                .bg(wash)
                .shadow(theme::shadow_panel())
                .px(px(theme::SPACE_3))
                .py(px(theme::SPACE_2))
                .flex()
                .flex_col()
                .gap_1()
                .text_size(px(theme::DETAIL_SIZE))
                .line_height(px(theme::DETAIL_LINE))
                .text_color(theme::text_secondary())
        };
        let label = |dot: Hsla, text: String, strong: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().size(px(theme::STATE_DOT)).rounded_full().bg(dot))
                .child(
                    div()
                        .font_weight(if strong {
                            theme::WEIGHT_STRONG
                        } else {
                            theme::WEIGHT_LABEL
                        })
                        .text_color(dot)
                        .child(text),
                )
        };
        let inner: gpui::Div = match attention {
            AttentionSnapshot::Running => return None,
            AttentionSnapshot::Quiet { since_ms, reason } => {
                let held = now.saturating_sub(since_ms);
                let what = match &reason {
                    WaitReason::Tool { name, since_ms } => format!(
                        "waiting on tool {name} for {}",
                        fmt_elapsed(now.saturating_sub(*since_ms))
                    ),
                    WaitReason::Api { since_ms } => format!(
                        "awaiting API response for {}",
                        fmt_elapsed(now.saturating_sub(*since_ms))
                    ),
                    WaitReason::Idle => "no output mid-turn, nothing in flight".to_string(),
                };
                let prominent = held >= PROMINENT_AFTER_MS;
                panel(
                    theme::warning_ring(),
                    if prominent {
                        theme::warning_plate_strong_glass()
                    } else {
                        theme::warning_plate_glass()
                    },
                )
                .child(label(
                    theme::warning(),
                    format!("quiet for {}", fmt_elapsed(held)),
                    prominent,
                ))
                .child(what)
            }
            AttentionSnapshot::Unresponsive {
                since_ms,
                zero_samples,
                sampled_ms,
            } => panel(theme::danger_ring(), theme::danger_plate_glass())
                .child(label(
                    theme::danger(),
                    format!(
                        "unresponsive for {}",
                        fmt_elapsed(now.saturating_sub(since_ms))
                    ),
                    true,
                ))
                .child(format!(
                    "evidence: zero CPU across {zero_samples} consecutive samples \
                     ({} sampled) · no tool running · no API request in flight",
                    fmt_elapsed(sampled_ms)
                ))
                .child(self.kill_affordance(cx, "it will never be killed automatically")),
            // Slice K: when the question itself is on screen, the card
            // says everything this panel would — and says it better,
            // because it carries the buttons. The panel is floating
            // chrome above the composer, so rendering both put a notice
            // ON TOP of the approve/deny it was pointing at (found by eye
            // in the real renderer, 2026-08-18). The notice therefore
            // only stands in when the question is NOT on screen: the
            // projection was reset, or the attention frame outran the
            // records that carry the question.
            AttentionSnapshot::AwaitingApproval { .. } if question_on_screen => return None,
            AttentionSnapshot::AwaitingApproval {
                since_ms,
                pending,
                tool,
            } => panel(theme::warning_ring(), theme::warning_plate_strong_glass())
                .child(label(
                    theme::warning(),
                    format!(
                        "waiting on you for {}",
                        fmt_elapsed(now.saturating_sub(since_ms))
                    ),
                    true,
                ))
                .child(if pending > 1 {
                    format!(
                        "{pending} permission requests are open; the oldest is {tool}. \
                         The session waits until you answer — nothing times out."
                    )
                } else {
                    format!(
                        "{tool} is waiting for your approval below. The session waits \
                         until you answer — nothing times out."
                    )
                }),
            AttentionSnapshot::Failed { since_ms, cause } => {
                panel(theme::danger_ring(), theme::danger_plate_glass())
                    .child(label(
                        theme::danger(),
                        format!("failed {} ago", fmt_elapsed(now.saturating_sub(since_ms))),
                        true,
                    ))
                    .child(cause)
                    .child(self.kill_affordance(cx, "clean up the process when you're ready"))
            }
        };
        let t = motion::entrance_t(&format!("in:notice-{attn_session}"), motion::FADE_IN);
        Some(
            Self::column(frost(theme::RADIUS_PANEL, inner))
                .pb(px(theme::SPACE_2))
                .opacity(t)
                .relative()
                .top(px(motion::ENTRANCE_RISE * (1.0 - t)))
                .into_any_element(),
        )
    }

    fn kill_affordance(&self, cx: &Context<Self>, note: &str) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .pt_1()
            .child(
                div()
                    .px(px(theme::SPACE_3))
                    .py(px(theme::SPACE_1))
                    .rounded(px(theme::RADIUS_BUTTON))
                    .border_1()
                    .border_color(theme::danger_ring())
                    .text_size(px(theme::DETAIL_SIZE))
                    .font_weight(theme::WEIGHT_LABEL)
                    .text_color(theme::danger())
                    .id("attn-kill")
                    .debug_selector(|| "attn-kill".into())
                    // Slice M (§2.2). Its resting fill is already a
                    // danger wash, so the hover step is more of the same
                    // pigment rather than the neutral overlay every
                    // unfilled affordance uses. Both ends are opaque
                    // pigment here, so this is the one site where the
                    // blend is an ordinary sRGB interpolation.
                    .hover_fade(
                        "attn-kill",
                        theme::danger_wash_strong(),
                        theme::danger_wash_hover(),
                    )
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, _| {
                        let _ = this.commands.send(Command::Kill);
                    }))
                    .child("kill session"),
            )
            .child(
                div()
                    .text_size(px(theme::SMALL_SIZE))
                    .text_color(theme::text_tertiary())
                    .child(SharedString::from(note.to_string())),
            )
    }

    /// Build one virtualized row. Called only for visible + overdraw rows.
    /// Every row wraps its content in the shared reading column; vertical
    /// rhythm lives on the row (turn gap before a directive, block gap
    /// after prose) so virtualization owns all spacing.
    fn render_row(state: &TranscriptState, ix: usize, phase: Option<f32>) -> AnyElement {
        let Some(row) = state.display.row(ix).copied() else {
            // The one index past the content is the under-glass spacer
            // (Stage H): it holds Follow's newest row above the floating
            // chrome while the list's viewport extends beneath it.
            // Constant height, position-independent — the anchor
            // invariant below applies to it too.
            return div().h(px(theme::COMPOSER_CLEARANCE)).into_any_element();
        };
        let id = row.id;
        let Some(trow) = state.transcript.rows.get(id.msg as usize) else {
            return div().into_any_element();
        };
        // NOTE: row height must depend only on the row's identity and
        // content — never its position (first/last). A position-dependent
        // pad changes a row's height without a fingerprint change when a
        // splice appends behind it, leaving a stale cached measure that
        // moves the scroll anchor (caught by the replay assert). The log's
        // top/bottom breathing room lives on the list container instead.
        let first = ix == 0;
        let inner: AnyElement = match trow {
            // A user directive: the log's asymmetry made visible. Left-
            // aligned raised block — an event in the run, not a chat
            // bubble; the only raised full blocks in the stream are the
            // operator's instructions.
            Row::User { text, delivery } => {
                let block = div()
                    .max_w(px(theme::DIRECTIVE_MAX_W))
                    .min_w(px(0.))
                    .rounded(px(theme::RADIUS_DIRECTIVE))
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::raised())
                    .px(px(theme::SPACE_3))
                    .py(px(theme::SPACE_2))
                    .text_size(px(theme::USER_SIZE))
                    .line_height(px(theme::USER_LINE))
                    .text_color(theme::text())
                    .when(matches!(delivery, Delivery::Pending), |d| d.opacity(0.65))
                    .child(SharedString::from(text.clone()));
                let wrap = div()
                    .when(!first, |d| d.pt(px(theme::TURN_GAP)))
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_1()
                    .child(block);
                match delivery {
                    // The acceptance-critical distinction: journaled but
                    // never reached the agent (spec 02 §Delivery state).
                    Delivery::Undelivered => wrap
                        .child(
                            div()
                                .text_size(px(theme::SMALL_SIZE))
                                .line_height(px(theme::SMALL_LINE))
                                .font_weight(theme::WEIGHT_LABEL)
                                .text_color(theme::danger())
                                .child("not delivered — the agent never saw this message"),
                        )
                        .into_any_element(),
                    _ => wrap.into_any_element(),
                }
            }
            Row::Tool {
                id: tool_id,
                name,
                status,
                input,
                result,
                is_error,
            } => {
                // Slice Z: a tool with anything to show can be opened, and
                // a FAILED one shows its reason without being opened —
                // Slice V's rule stands, this only adds the other half.
                let has_body = !input.trim().is_empty() || !result.trim().is_empty();
                let open = state.open_tools.contains(tool_id);
                let (label, dot) = match status {
                    ToolStatus::Running => ("running…", theme::live()),
                    ToolStatus::Finished if *is_error => ("failed", theme::danger()),
                    ToolStatus::Finished => ("done", theme::text_tertiary()),
                    ToolStatus::Interrupted => ("interrupted", theme::danger()),
                };
                // What the tool was ASKED to do. Until this slice a row was
                // the tool's name and a status word, so an agent running
                // `cargo test --workspace` rendered "Bash  done" — the one
                // fact an operator is watching for was the one eave threw
                // away (`stream.rs` kept the id and dropped the input).
                let detail = eave_wire::model::tool_detail(name, input);
                // A failed tool states why. A successful one does not: its
                // output is usually long, usually uninteresting, and always
                // a row height inside a virtualized list.
                let failure = (*is_error && !open && !result.trim().is_empty()).then(|| {
                    let total = result.lines().count();
                    let mut text = result
                        .lines()
                        .take(theme::TOOL_ERROR_MAX_LINES)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if total > theme::TOOL_ERROR_MAX_LINES {
                        // State the remainder rather than trailing off, the
                        // way the diff pane states its own cap.
                        text.push_str(&format!(
                            "\n… {} more lines",
                            total - theme::TOOL_ERROR_MAX_LINES
                        ));
                    }
                    text
                });
                let head = div()
                    .id(SharedString::from(format!("tool-{tool_id}")))
                    .when(has_body, |d| {
                        let tid = tool_id.clone();
                        let shell = state.shell.clone();
                        d.cursor_pointer()
                            .debug_selector(|| "tool-head".into())
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                if let Some(shell) = shell.as_ref().and_then(|s| s.upgrade()) {
                                    let tid = tid.clone();
                                    shell.update(cx, |shell, cx| {
                                        shell.transcript.update(cx, |t, cx| {
                                            if !t.open_tools.remove(&tid) {
                                                t.open_tools.insert(tid.clone());
                                            }
                                            t.remeasure_tool(&tid);
                                            cx.notify();
                                        });
                                    });
                                }
                            })
                    })
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(loaders::work_dot(
                        theme::TOOL_DOT,
                        dot,
                        // Only while it is actually running: `done` and
                        // `interrupted` are settled facts.
                        phase.filter(|_| matches!(status, ToolStatus::Running)),
                    ))
                    .child(
                        div()
                            .font_family(theme::MONO)
                            .text_size(px(theme::TOOL_NAME_SIZE))
                            .line_height(px(theme::TOOL_NAME_LINE))
                            .font_weight(theme::WEIGHT_LABEL)
                            .text_color(theme::text_secondary())
                            .child(SharedString::from(name.clone())),
                    )
                    .children(detail.map(|d| {
                        div()
                            .debug_selector(|| "tool-detail".into())
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .font_family(theme::MONO)
                            .text_size(px(theme::TOOL_NAME_SIZE))
                            .line_height(px(theme::TOOL_NAME_LINE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(d))
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(if *is_error {
                                theme::danger()
                            } else {
                                theme::text_tertiary()
                            })
                            .child(label),
                    );
                div()
                    .debug_selector(|| "tool-row".into())
                    .pt(px(theme::SPACE_2))
                    .pb(px(theme::SPACE_1))
                    .pl(px(theme::TOOL_INDENT))
                    .flex()
                    .flex_col()
                    .gap(px(theme::SPACE_1))
                    .child(head)
                    .children(open.then(|| {
                        // Opened: what it was asked, and what came back.
                        // Both verbatim, both bounded, each labelled —
                        // an unlabelled pair of blobs would make the
                        // reader guess which is which.
                        let mut parts: Vec<(&str, String)> = Vec::new();
                        if !input.trim().is_empty() {
                            parts
                                .push(("input", tool_body_view(input, theme::TOOL_BODY_MAX_LINES)));
                        }
                        if !result.trim().is_empty() {
                            parts.push((
                                if *is_error { "error" } else { "result" },
                                tool_body_view(result, theme::TOOL_BODY_MAX_LINES),
                            ));
                        }
                        div()
                            .debug_selector(|| "tool-body".into())
                            .ml(px(theme::TOOL_INDENT))
                            .flex()
                            .flex_col()
                            .gap(px(theme::SPACE_1))
                            .children(parts.into_iter().map(|(label, body)| {
                                div()
                                    .rounded(px(theme::RADIUS_CODE))
                                    .border_1()
                                    .border_color(theme::border())
                                    .bg(theme::inset())
                                    .px(px(theme::SPACE_2))
                                    .py(px(theme::SPACE_1))
                                    .flex()
                                    .flex_col()
                                    .gap(px(theme::SPACE_1))
                                    .child(
                                        div()
                                            .text_size(px(theme::MICRO_SIZE))
                                            .line_height(px(theme::MICRO_LINE))
                                            .font_weight(theme::WEIGHT_LABEL)
                                            .text_color(theme::text_tertiary())
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .font_family(theme::MONO)
                                            .text_size(px(theme::DETAIL_SIZE))
                                            .line_height(px(theme::SMALL_LINE))
                                            .text_color(theme::text_secondary())
                                            .child(SharedString::from(body)),
                                    )
                            }))
                    }))
                    .children(failure.map(|f| {
                        div()
                            .ml(px(theme::TOOL_INDENT))
                            .rounded(px(theme::RADIUS_CODE))
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::inset())
                            .px(px(theme::SPACE_2))
                            .py(px(theme::SPACE_1))
                            .font_family(theme::MONO)
                            .text_size(px(theme::DETAIL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_secondary())
                            .child(SharedString::from(f))
                    }))
                    .into_any_element()
            }
            // Slice K: the question itself. Every fact an operator needs
            // to decide is on the card — tool, what it does, which path,
            // and the full input — because approving without knowing what
            // you approved is the failure this slice exists to prevent.
            Row::Approval {
                request_id,
                tool,
                description,
                path,
                input_json,
                status,
            } => Self::render_approval(
                state,
                request_id,
                tool,
                description,
                path,
                input_json,
                status,
            ),
            Row::Marker { text } => div()
                .py(px(theme::SPACE_3))
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(theme::SPACE_3))
                        .py(px(theme::SPACE_1))
                        .rounded_full()
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::overlay())
                        .text_size(px(theme::SMALL_SIZE))
                        .line_height(px(theme::SMALL_LINE))
                        .text_color(theme::text_tertiary())
                        .child(SharedString::from(text.clone())),
                )
                .into_any_element(),
            Row::Assistant {
                text: msg_text,
                live,
            } => {
                let Some(block) = state.display.block(id) else {
                    // Placeholder row of an empty live message. Slice N3
                    // replaced the static `▌` here: a caret that never
                    // moves is the same picture whether the agent is
                    // mid-thought or wedged. The slot is pinned to ONE
                    // body line so the row's cached height cannot depend
                    // on the phase — the indicator only ever changes
                    // opacity inside it.
                    return Self::column(
                        div()
                            .pt(px(theme::MSG_TOP_GAP))
                            .h(px(theme::BODY_LINE))
                            .flex()
                            .items_center()
                            .child(loaders::work_bar(theme::live(), phase)),
                    )
                    .into_any_element();
                };
                let el = Self::render_block(state, id, block, msg_text, *live);
                div()
                    .when(id.block == 0, |d| d.pt(px(theme::MSG_TOP_GAP)))
                    .pb(px(theme::BLOCK_GAP))
                    .child(el)
                    .into_any_element()
            }
        };
        Self::column(inner).into_any_element()
    }

    /// One permission question, pending or settled (Slice K).
    ///
    /// Deliberately NOT a `cx.listener` closure per button: this is a
    /// static row builder like every other row, so the click has to reach
    /// the shell through the same window-level dispatch the rest of the
    /// transcript uses. The buttons emit into the shared command channel
    /// directly — the same channel the keyboard path uses, so both routes
    /// are provably one behavior.
    fn render_approval(
        state: &TranscriptState,
        request_id: &str,
        tool: &str,
        description: &str,
        path: &Option<String>,
        input_json: &str,
        status: &ApprovalStatus,
    ) -> AnyElement {
        let pending = matches!(status, ApprovalStatus::Pending);
        let rid = request_id.to_string();
        let (ring, plate) = if pending {
            (theme::warning_ring(), theme::warning_plate_strong_glass())
        } else {
            (theme::border(), theme::overlay())
        };
        let line = |color: Hsla, text: String| {
            div()
                .text_size(px(theme::SMALL_SIZE))
                .line_height(px(theme::SMALL_LINE))
                .text_color(color)
                .child(SharedString::from(text))
        };
        let mono = |text: String| {
            div()
                .font_family(theme::MONO)
                .text_size(px(theme::CODE_SIZE))
                .line_height(px(theme::CODE_LINE))
                .text_color(theme::code_block_text())
                .child(SharedString::from(text))
        };
        let mut card = div()
            .my(px(theme::SPACE_2))
            .rounded(px(theme::RADIUS_PANEL))
            .border_1()
            .border_color(ring)
            .bg(plate)
            .px(px(theme::SPACE_3))
            .py(px(theme::SPACE_2))
            .flex()
            .flex_col()
            .gap_1()
            .debug_selector({
                let rid = rid.clone();
                move || format!("approval-{rid}")
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(theme::DETAIL_SIZE))
                            .font_weight(theme::WEIGHT_STRONG)
                            .text_color(if pending {
                                theme::warning()
                            } else {
                                theme::text_secondary()
                            })
                            .child(SharedString::from(if pending {
                                format!("{tool} — needs your approval")
                            } else {
                                tool.to_string()
                            })),
                    )
                    .child(
                        div()
                            .debug_selector({
                                let rid = rid.clone();
                                move || format!("approval-status-{rid}")
                            })
                            .text_size(px(theme::SMALL_SIZE))
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(match status {
                                ApprovalStatus::Pending => "waiting".to_string(),
                                ApprovalStatus::Approved => "approved".to_string(),
                                ApprovalStatus::Denied { message } => {
                                    if message.is_empty() {
                                        "denied".to_string()
                                    } else {
                                        format!("denied: {message}")
                                    }
                                }
                                ApprovalStatus::Withdrawn { reason } => {
                                    format!("withdrawn: {reason}")
                                }
                            })),
                    ),
            );
        if !description.is_empty() {
            card = card.child(line(theme::text(), description.to_string()));
        }
        if let Some(p) = path {
            card = card.child(mono(p.clone()).debug_selector({
                let rid = rid.clone();
                move || format!("approval-path-{rid}")
            }));
        }
        // The full input, stated. Truncation is stated too — a silently
        // shortened input is the same lie as a silently shortened fact
        // anywhere else in this app.
        let (shown, omitted) = if input_json.chars().count() > APPROVAL_INPUT_CAP {
            let head: String = input_json.chars().take(APPROVAL_INPUT_CAP).collect();
            let rest = input_json.chars().count() - APPROVAL_INPUT_CAP;
            (head, rest)
        } else {
            (input_json.to_string(), 0)
        };
        card = card.child(
            div()
                .mt_1()
                .rounded(px(theme::RADIUS_CODE))
                .border_1()
                .border_color(theme::border())
                .bg(theme::inset())
                .px(px(theme::SPACE_2))
                .py(px(theme::SPACE_1))
                .debug_selector({
                    let rid = rid.clone();
                    move || format!("approval-input-{rid}")
                })
                .child(mono(shown)),
        );
        if omitted > 0 {
            card = card.child(line(
                theme::text_tertiary(),
                format!("… and {omitted} more characters of input"),
            ));
        }
        if pending {
            card = card
                .child(
                    div()
                        .pt_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(Self::decision_button(
                            state,
                            &rid,
                            true,
                            "approve",
                            theme::live(),
                        ))
                        .child(Self::decision_button(
                            state,
                            &rid,
                            false,
                            "deny",
                            theme::danger(),
                        )),
                )
                .child(line(
                    theme::text_tertiary(),
                    "⇧⌘A approve · ⇧⌘D deny · whatever is in the composer is sent as the \
                     deny reason"
                        .to_string(),
                ));
        }
        Self::column(card).into_any_element()
    }

    fn decision_button(
        state: &TranscriptState,
        rid: &str,
        allow: bool,
        word: &str,
        color: Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        let rid = rid.to_string();
        let id: SharedString = format!("{}-{rid}", if allow { "approve" } else { "deny" }).into();
        let tx = state.commands.clone();
        let composer = state.composer.clone();
        div()
            .px(px(theme::SPACE_3))
            .py(px(theme::SPACE_1))
            .rounded(px(theme::RADIUS_BUTTON))
            .border_1()
            .border_color(color)
            .text_size(px(theme::DETAIL_SIZE))
            .font_weight(theme::WEIGHT_LABEL)
            .text_color(color)
            .child(SharedString::from(word.to_string()))
            .id(id.clone())
            .debug_selector({
                let rid = rid.clone();
                move || format!("{}-{rid}", if allow { "approve" } else { "deny" })
            })
            // Slice M (§2.2). The neutral wash, not the button's own
            // pigment: approve and deny are the two most consequential
            // clicks in the app, and hover must say "the pointer is
            // here" without ever reading as "this one is chosen".
            //
            // The only two hover fades in the app that live INSIDE the
            // virtualized list, so their reads land in prepaint rather
            // than in the root render — which is the whole reason the
            // store's staleness rule carries a frame of grace
            // (`motion::Fades::begin`).
            .hover_fade(id.to_string(), theme::wash_rest(), theme::overlay())
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let message = if allow {
                    String::new()
                } else {
                    take_deny_reason(composer.as_ref(), cx)
                };
                if let Some(tx) = &tx {
                    let _ = tx.send(Command::Decide {
                        request_id: rid.clone(),
                        allow,
                        message,
                    });
                }
            })
    }

    /// A quiet trailer under a capped block, stating what it is holding
    /// back. Every cap in this app states its own remainder; this is the
    /// list's and the table's version of the fence's line.
    fn capped_note(text: String) -> gpui::Div {
        div()
            .pt(px(theme::SPACE_1))
            .text_size(px(theme::SMALL_SIZE))
            .line_height(px(theme::SMALL_LINE))
            .text_color(theme::text_tertiary())
            .child(SharedString::from(text))
            .debug_selector(|| "md-capped".into())
    }

    /// A bullet, an ordinal, or a checkbox — whatever this item's gutter
    /// holds. Never a fixed-height row: the gutter is a fixed WIDTH beside
    /// a column that wraps, which is the shape `overlay::menu_row` got
    /// wrong (a fixed-height row whose label wrapped drew over its
    /// neighbour, Slice R1).
    fn list_marker(marker: Marker, depth: usize) -> AnyElement {
        match marker {
            Marker::Task(done) => div()
                .w(px(theme::CHECKBOX))
                .h(px(theme::CHECKBOX))
                // Centred on the FIRST line of the item, not on the item:
                // a three-line item must not float its box to the middle.
                .mt(px((theme::BODY_LINE - theme::CHECKBOX) / 2.0))
                .flex_shrink_0()
                .rounded(px(theme::RADIUS_CHECKBOX))
                .border_1()
                .border_color(if done {
                    theme::success()
                } else {
                    theme::border_strong()
                })
                .when(done, |d| d.bg(theme::success()))
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .when(done, |d| {
                    d.child(
                        div()
                            .text_size(px(theme::MICRO_SIZE))
                            .line_height(px(theme::CHECKBOX))
                            .font_weight(theme::WEIGHT_STRONG)
                            .text_color(theme::on_inverse())
                            .child("✓"),
                    )
                })
                .debug_selector(move || format!("md-task-{}", if done { "done" } else { "open" }))
                .into_any_element(),
            other => {
                let glyph = match other {
                    // The bullet says which level it is on, so a nested
                    // list reads as nested even where the indent is small.
                    Marker::Bullet => match depth % 3 {
                        0 => "•".to_string(),
                        1 => "◦".to_string(),
                        _ => "▪".to_string(),
                    },
                    Marker::Number(n) => format!("{n}."),
                    Marker::Task(_) => unreachable!("handled above"),
                };
                div()
                    .text_size(px(theme::BODY_SIZE))
                    .line_height(px(theme::BODY_LINE))
                    .text_color(theme::text_tertiary())
                    .child(SharedString::from(glyph))
                    .into_any_element()
            }
        }
    }

    /// A list block: markers in their own gutter, nesting as indentation,
    /// task boxes drawn rather than spelled `[ ]`.
    fn render_list(text: &str) -> AnyElement {
        let (items, more) = markdown::list_view(text, theme::LIST_MAX_ITEMS);
        if items.is_empty() {
            // Nothing parsed as an item (a list block that is only lazy
            // continuation). State the source rather than nothing.
            return div()
                .text_size(px(theme::BODY_SIZE))
                .line_height(px(theme::BODY_LINE))
                .text_color(theme::text())
                .child(VeiledBlock::plain(inline_runs(text), theme::text()))
                .into_any_element();
        }
        let mut col = div().flex().flex_col().debug_selector(|| "md-list".into());
        for (ix, item) in items.iter().enumerate() {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .pl(px(item.depth as f32 * theme::LIST_INDENT))
                    .child(
                        div()
                            .w(px(theme::LIST_GUTTER_W))
                            .flex_shrink_0()
                            .flex()
                            // An ordinal is read from its RIGHT edge — the
                            // periods of "9." and "10." line up or the
                            // column looks broken. A bullet and a checkbox
                            // are read from their centres, and a capture of
                            // a mixed list showed the two disagreeing by
                            // eight pixels when everything was right-aligned.
                            .map(|d| match item.marker {
                                Marker::Number(_) => d.justify_end(),
                                _ => d.justify_center(),
                            })
                            .child(Self::list_marker(item.marker, item.depth)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .pl(px(theme::LIST_MARKER_GAP))
                            .text_size(px(theme::BODY_SIZE))
                            .line_height(px(theme::BODY_LINE))
                            .text_color(theme::text())
                            .child(VeiledBlock::plain(inline_runs(&item.text), theme::text()))
                            .debug_selector(move || format!("md-item-{ix}")),
                    ),
            );
        }
        if more > 0 {
            col = col.child(Self::capped_note(format!("… {more} more items")));
        }
        col.into_any_element()
    }

    /// A pipe table, as columns.
    ///
    /// **The width decision, stated: it WRAPS.** The table never scrolls
    /// and never widens the transcript. Columns take a zero flex basis and
    /// a grow proportional to their longest cell, so the row is exactly the
    /// content column wide whatever the table holds, and cell text wraps
    /// inside its own column. `overflow_hidden` is a backstop rather than
    /// the mechanism — verified by capture, gpui breaks even a single
    /// unbreakable 120-character token at the column edge rather than
    /// letting it overhang, so the clip is what catches a case that has not
    /// been produced yet, not the everyday behaviour.
    ///
    /// The two alternatives were rejected for one reason each: a
    /// horizontally scrolling row hides data behind a gesture with no
    /// affordance, inside a list that already owns the vertical one; and
    /// letting the row grow makes the WHOLE transcript scroll sideways, so
    /// every paragraph in the session pays for one table.
    fn render_table(text: &str) -> AnyElement {
        let t = markdown::table_view(text, theme::TABLE_MAX_ROWS, theme::TABLE_CELL_MAX_CHARS);
        if t.cols() == 0 {
            return div().into_any_element();
        }
        let cell =
            |ix: usize, body: &str, align: Align, strong: bool, color: Hsla, selector: String| {
                div()
                    .flex_basis(px(0.))
                    .flex_grow(t.weights.get(ix).copied().unwrap_or(1.0))
                    .min_w(px(0.))
                    .px(px(theme::TABLE_CELL_PAD_X))
                    .py(px(theme::TABLE_CELL_PAD_Y))
                    .text_size(px(theme::BODY_SIZE))
                    .line_height(px(theme::BODY_LINE))
                    .text_color(color)
                    .when(strong, |d| d.font_weight(theme::WEIGHT_STRONG))
                    .map(|d| match align {
                        Align::Left => d.text_left(),
                        Align::Center => d.text_center(),
                        Align::Right => d.text_right(),
                    })
                    .child(VeiledBlock::plain(inline_runs(body), color))
                    .debug_selector(move || selector)
            };

        let mut table = div()
            .my_1()
            .w_full()
            .rounded(px(theme::RADIUS_CODE))
            .border_1()
            .border_color(theme::border())
            .overflow_hidden()
            .debug_selector(|| "md-table".into());

        if !t.head.is_empty() {
            let mut head = div()
                .flex()
                .flex_row()
                .items_stretch()
                .bg(theme::overlay())
                .border_b_1()
                .border_color(theme::border_strong());
            for (c, body) in t.head.iter().enumerate() {
                head = head.child(cell(
                    c,
                    body,
                    t.align[c],
                    true,
                    theme::text(),
                    format!("md-head-{c}"),
                ));
            }
            table = table.child(head);
        }
        for (r, row) in t.rows.iter().enumerate() {
            let mut line = div()
                .flex()
                .flex_row()
                .items_stretch()
                .when(r > 0 || !t.head.is_empty(), |d| {
                    d.border_t_1().border_color(theme::border())
                });
            for (c, body) in row.iter().enumerate() {
                // Body cells carry the DATA, so they get the primary tier,
                // not the quiet one: a table is not a caption. (The quiet
                // tier would also have been the wrong call on the header's
                // wash — `text_secondary` over `overlay()` is 3.81:1.)
                line = line.child(cell(
                    c,
                    body,
                    t.align[c],
                    false,
                    theme::text(),
                    format!("md-cell-{r}-{c}"),
                ));
            }
            table = table.child(line);
        }
        if t.more_rows > 0 {
            table = table.child(
                div()
                    .border_t_1()
                    .border_color(theme::border())
                    .px(px(theme::TABLE_CELL_PAD_X))
                    .pb(px(theme::TABLE_CELL_PAD_Y))
                    .child(Self::capped_note(format!("… {} more rows", t.more_rows))),
            );
        }
        table.into_any_element()
    }

    fn render_block(
        state: &TranscriptState,
        id: RowId,
        block: &Block,
        msg_text: &str,
        live: bool,
    ) -> AnyElement {
        let is_tail = state
            .display
            .block(RowId {
                msg: id.msg,
                block: id.block + 1,
            })
            .is_none();
        match block {
            // Mending for fences is structural: `split_blocks` yields the
            // open fence's code, and the row renders as a normal, closed-
            // looking code box; the synthetic closer exists only in this
            // display tree, canonical text untouched (spec 03 §Rendering).
            Block::Fence { lang, code, .. } => {
                let label = if lang.is_empty() { "code" } else { lang };
                div()
                    .my_1()
                    .rounded(px(theme::RADIUS_CODE))
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::inset())
                    .overflow_hidden()
                    .child(
                        div()
                            .h(px(theme::CODE_STRIP_H))
                            .px(px(theme::SPACE_3))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(theme::border())
                            .text_size(px(theme::MICRO_SIZE))
                            .line_height(px(theme::MICRO_LINE))
                            .font_weight(theme::WEIGHT_LABEL)
                            .text_color(theme::text_tertiary())
                            .child(SharedString::from(label.to_string()))
                            // The strip's remaining width, so the button
                            // sits at the far edge without a spacer div.
                            .child(div().flex_1())
                            .child({
                                let whole = code.trim_end().to_string();
                                let lines = whole.lines().count();
                                let shell = state.shell.clone();
                                div()
                                    .id(("fence-copy", id.block))
                                    .debug_selector(|| "fence-copy".into())
                                    .cursor_pointer()
                                    .px(px(theme::SPACE_1))
                                    .text_color(theme::text_tertiary())
                                    .child(icons::icon(
                                        icons::COPY,
                                        theme::ICON_SM,
                                        theme::text_tertiary(),
                                    ))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            whole.clone(),
                                        ));
                                        // The WHOLE fence, not the drawn
                                        // part: a cap is a reading limit,
                                        // and a copy that silently gave you
                                        // the first 200 lines of a 500-line
                                        // block would be the worst kind of
                                        // quiet failure.
                                        if let Some(shell) =
                                            shell.as_ref().and_then(|s| s.upgrade())
                                        {
                                            shell.update(cx, |shell, cx| {
                                                shell.transient_note(
                                                    format!(
                                                        "copied {lines} line{}",
                                                        if lines == 1 { "" } else { "s" }
                                                    ),
                                                    cx,
                                                );
                                            });
                                        }
                                    })
                            }),
                    )
                    .child({
                        let drawn = fence_view(code.trim_end());
                        div()
                            .px(px(theme::SPACE_3))
                            .py(px(theme::SPACE_2))
                            .font_family(theme::MONO)
                            .text_size(px(theme::CODE_SIZE))
                            .line_height(px(theme::CODE_LINE))
                            .text_color(theme::code_block_text())
                            .child(SharedString::from(drawn))
                    })
                    .into_any_element()
            }
            // A heading is inline markdown too. Until Slice AD it was the
            // one block rendered as a bare string, so `## The **why**`
            // printed its own asterisks at 20 px.
            Block::Heading { level, text } => {
                let (size, line, weight) = theme::heading(*level);
                div()
                    .pt_1()
                    .text_size(size)
                    .line_height(line)
                    .text_color(theme::text())
                    .font_weight(weight)
                    .child(VeiledBlock::plain(inline_runs(text), theme::text()))
                    .into_any_element()
            }
            // A blockquote: a rule down the left, quieter ink. Parsed for
            // the first time in Slice AD — before it, `> note` was a
            // paragraph that began with a greater-than sign.
            Block::Quote { text } => div()
                .my_1()
                .flex()
                .flex_row()
                .child(
                    div()
                        .w(px(theme::QUOTE_BAR_W))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme::border_strong()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .pl(px(theme::SPACE_3))
                        .text_size(px(theme::BODY_SIZE))
                        .line_height(px(theme::BODY_LINE))
                        .text_color(theme::text_secondary())
                        .child(VeiledBlock::plain(
                            inline_runs(text),
                            theme::text_secondary(),
                        )),
                )
                .debug_selector(|| "md-quote".into())
                .into_any_element(),
            Block::List { text } => Self::render_list(text),
            Block::Table { text } => Self::render_table(text),
            Block::Rule => div()
                .my_1()
                .h(px(theme::HAIRLINE))
                .bg(theme::border())
                .into_any_element(),
            Block::Paragraph { text } => {
                let runs = inline_runs(text);
                let flat: String = runs.iter().map(|r| r.text.as_str()).collect();
                let total_chars = flat.chars().count();
                // Paint-only veil on the growing tail block of the live
                // message (spec 03 §Rendering).
                let veil = state
                    .veil
                    .as_ref()
                    .filter(|v| live && is_tail && v.msg == id.msg && v.from_byte < msg_text.len());
                let block_el = VeiledBlock {
                    text: SharedString::from(flat),
                    runs,
                    settled_chars: match veil {
                        Some(v) => {
                            let fresh_bytes = msg_text.len() - v.from_byte;
                            // The tail block ends where the message ends;
                            // map message-level growth onto the block's
                            // visible chars (marker chars are consumed by
                            // the inline parser; the boundary is a fade
                            // edge, not a semantic one).
                            total_chars.saturating_sub(fresh_bytes)
                        }
                        None => total_chars,
                    },
                    alpha: 1.0,
                    color: theme::text(),
                    code_color: theme::code_text(),
                };
                let el: AnyElement = match veil {
                    Some(v) => block_el
                        .with_animation(
                            // The id carries a generation counter for a
                            // reason: `with_animation`'s clock is keyed by
                            // the element-id PATH and replays from t=0 on
                            // remount, so a veil whose id did not change
                            // per burst would restart on every re-render.
                            (
                                "veil",
                                ((id.msg as u64) << 32) | (v.generation & 0xffff_ffff),
                            ),
                            // Slice M (§1.21): the curve, not the
                            // multi-chunk rewrite. `Animation::new`
                            // defaults to LINEAR, which put the fade's
                            // slowest-reading stretch at the end, where
                            // the text is already legible; an ease-out
                            // moves it to where it is not noticed.
                            //
                            // This element is also the whole of eave's
                            // reduced-motion story (§1.22):
                            // `with_animation` honours `App::reduce_motion`
                            // itself — a oneshot renders its END state and
                            // schedules no frames, i.e. alpha 1.0, i.e.
                            // text that simply appears. The flag it reads
                            // is set in `bootstrap`.
                            Animation::new(v.duration).with_easing(theme::veil_ease),
                            |mut el, t| {
                                el.alpha = t;
                                el
                            },
                        )
                        .into_any_element(),
                    None => block_el.into_any_element(),
                };
                div()
                    .child(el)
                    .debug_selector(|| "md-para".into())
                    .into_any_element()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slice P1's demo trigger — the overlay layer's only caller.
//
// The layer has no product consumer by directive, so this is what proves
// it: a button that is not rendered at all unless `EAVE_OVERLAY_DEMO` is
// set, opening a three-row menu that exercises every branch of the
// primitive (a plain row, a row with trailing state, a disabled row). The
// tests turn the same gate on directly.

impl Shell {
    /// The demo menu's rows. Rebuilt per render rather than stored: a menu
    /// states facts that were true when it was drawn, and a consumer that
    /// cached them would show a stale list after the state moved under it.
    fn demo_items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Reveal in Finder"),
            MenuItem::new("Copy session id").trailing("⌘C"),
            // The disabled row is not decoration: it is the only way to
            // prove the highlight skips it and that it takes no click.
            MenuItem::new("Delete worktree").disabled(),
        ]
    }

    fn open_demo_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let enabled = Self::demo_items().iter().map(|i| i.enabled).collect();
        self.demo.menu.open(at, enabled, now);
        cx.notify();
    }

    /// Dismiss: start the exit, and schedule the reap that will drop it.
    ///
    /// The reap is scheduled here, by the caller, rather than inside
    /// `begin_close` — a state machine that spawns tasks is a state machine
    /// that cannot be unit-tested without an executor, and `Popup` is
    /// deliberately pure. `begin_close` answering `false` under reduced
    /// motion is what keeps a timer from being spawned for an exit that
    /// never plays.
    fn close_demo_menu(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        if self.demo.menu.begin_close(now, cx.reduce_motion()) {
            overlay::reap(cx, |shell, cx| {
                let now = cx.background_executor().now();
                shell.demo.menu.finish_close(now);
            });
        }
        cx.notify();
    }

    fn choose_demo(&mut self, ix: usize, cx: &mut Context<Self>) {
        // Re-checked here as well as in the row: a disabled row drops its
        // click handler, but the KEYBOARD path reaches this by index, and
        // an index is only as safe as whoever computed it.
        if let Some(item) = Self::demo_items().get(ix).filter(|i| i.enabled) {
            self.demo.chosen = Some(item.label.clone());
        }
        self.close_demo_menu(cx);
    }

    /// The trigger. **Absolutely positioned**, so that even with the gate
    /// on it cannot move a single pixel of the real shell — which is what
    /// lets the other 244 tests run with it compiled in.
    fn demo_trigger(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("overlay-demo")
            .debug_selector(|| "overlay-demo".into())
            .absolute()
            .top(px(theme::TITLEBAR_H))
            .left(px(theme::PLATE_MARGIN))
            .px(px(theme::SPACE_2))
            .py(px(theme::SPACE_1))
            .rounded(px(theme::RADIUS_BUTTON))
            .border_1()
            .border_color(theme::border_strong())
            .text_size(px(theme::LABEL_SIZE))
            .line_height(px(theme::LABEL_LINE))
            .text_color(theme::text_secondary())
            .hover_fade("overlay-demo", theme::wash_rest(), theme::overlay())
            .cursor_pointer()
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                // Anchored at the pointer, one grid unit below it — the
                // context-menu idiom, and the one anchor that needs no
                // element bounds. A menu hung off a button's own box wants
                // `anchored()`'s local position mode instead; both are one
                // call, and which is right is the consumer's decision.
                let at = window.mouse_position();
                this.open_demo_menu(point(at.x, at.y + px(theme::MENU_ANCHOR_GAP)), cx);
            }))
            .child("overlay demo")
    }

    fn demo_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let anchor = *self.demo.menu.data()?;
        let now = cx.background_executor().now();
        let reduced = cx.reduce_motion();
        let live = self.demo.menu.is_open();
        let highlighted = self.demo.menu.highlighted();
        let rows = Self::demo_items()
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                overlay::menu_row(
                    ix,
                    item,
                    highlighted == Some(ix),
                    live,
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_demo(ix, cx)),
                )
            })
            .collect();
        overlay::menu_layer(
            &self.demo.menu,
            anchor,
            now,
            reduced,
            rows,
            cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_demo_menu(cx)),
        )
    }
}

/// Slice Q2 — the launcher.
impl Shell {
    fn provider(&self) -> &'static launcher::Provider {
        &PROVIDERS[self.launcher.provider]
    }

    /// The model menu's rows, rebuilt per render for the same reason the
    /// demo's are: a menu states what was true when it was drawn.
    ///
    /// The current choice carries a trailing check rather than a highlight
    /// — highlight is the KEYBOARD's cursor and means "this is where you
    /// are", which is a different fact from "this is what is set".
    fn model_items(&self) -> Vec<MenuItem> {
        let current = self.launcher.model;
        self.provider()
            .models
            .iter()
            .map(|m| {
                let is_current = match current {
                    Some(c) => c == *m,
                    None => *m == launcher::MODEL_DEFAULT,
                };
                let row = MenuItem::new(*m);
                if is_current {
                    row.trailing("✓")
                } else {
                    row
                }
            })
            .collect()
    }

    fn open_model_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let enabled = self.model_items().iter().map(|i| i.enabled).collect();
        self.launcher.models.open(at, enabled, now);
        cx.notify();
    }

    /// Dismiss: start the exit, schedule the reap. Same division as the
    /// demo's — `Popup` stays pure, the caller owns the timer, and
    /// `begin_close` answering `false` under reduced motion is what keeps
    /// a timer from being spawned for an exit that never plays.
    fn close_model_menu(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        if self.launcher.models.begin_close(now, cx.reduce_motion()) {
            overlay::reap(cx, |shell, cx| {
                let now = cx.background_executor().now();
                shell.launcher.models.finish_close(now);
            });
        }
        cx.notify();
    }

    fn choose_model(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(m) = self.provider().models.get(ix) {
            // The first entry means "send no --model flag", which is a
            // different thing from sending the string "CLI default".
            self.launcher.model = if *m == launcher::MODEL_DEFAULT {
                None
            } else {
                Some(*m)
            };
        }
        self.close_model_menu(cx);
    }

    /// Pick a provider tile.
    ///
    /// **Clears the model.** A model alias belongs to one CLI's vocabulary
    /// — carrying `opus` across to another provider would send a flag that
    /// vendor has never heard of, and eave does not validate models
    /// (P2 caveat 5), so the CLI would be the one to complain.
    fn pick_provider(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.launcher.provider != ix {
            self.launcher.provider = ix;
            self.launcher.model = None;
        }
        cx.notify();
    }

    fn launch(&mut self, cx: &mut Context<Self>) {
        let p = self.provider();
        let _ = self.commands.send(Command::Launch {
            provider: p.id.to_string(),
            model: self.launcher.model.map(str::to_string),
        });
        cx.notify();
    }

    /// `+ new`: **deselect, and land on the launcher** (Slice Q2 — it no
    /// longer opens anything).
    ///
    /// A named method rather than a closure body because the rail's button
    /// is no longer its only caller: [`Shell::launch_at_boot`] takes the
    /// same route, and "the same route" has to be literally the same call
    /// or it is a claim nobody checks.
    ///
    /// Nothing here touches UI state, deliberately. Whether the launcher is
    /// up is the connection loop's `at_launcher`, projected back as
    /// `FeedEvent::NoSession` — a shell that raised the launcher on its own
    /// would be holding state the engine has not agreed to, which is the
    /// one thing the projection rule forbids.
    fn new_session(&self) {
        let _ = self.commands.send(Command::NewSession);
    }

    /// What `EAVE_LAUNCH_AT_BOOT=1` presses, **in the order a person
    /// presses it: `+ new` first, then start.**
    ///
    /// SLICE AI, and the `+ new` is the whole slice. The hook used to send
    /// `Command::Launch` alone, and `Launch` is serviced by exactly ONE of
    /// the connection loop's two branches — the rest loop, which only runs
    /// while no session is selected. The session branch drops it (see
    /// there; the drop is right). So the moment the data dir had any
    /// history the hook boot-selected that session, sent a `Launch` nobody
    /// was listening for, and **silently did nothing** — from Q2 onwards,
    /// through two independent rediscoveries.
    /// `scripts/q2-launch-proof.sh` never caught it because every case it
    /// ran started from an empty engine.
    ///
    /// `+ new` is not a shortcut around the UI; it is the UI. The commands
    /// channel preserves order, so `NewSession` puts the loop back in the
    /// rest branch before `Launch` is read out behind it. Against an EMPTY
    /// engine the extra `NewSession` lands on the rest loop's "already
    /// here" arm and costs nothing — which is why the proof's `launched`
    /// case still wants exactly one session rather than two.
    fn launch_at_boot(&mut self, cx: &mut Context<Self>) {
        self.new_session();
        self.launch(cx);
    }

    /// The model menu's overlay layer. Separate from `demo_layer` and
    /// mounted beside it: two popups, two independent state machines, and
    /// no shared "which menu is open" global to get out of step.
    fn launcher_menu_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let anchor = *self.launcher.models.data()?;
        let now = cx.background_executor().now();
        let reduced = cx.reduce_motion();
        let live = self.launcher.models.is_open();
        let highlighted = self.launcher.models.highlighted();
        let rows = self
            .model_items()
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                overlay::menu_row(
                    ix,
                    item,
                    highlighted == Some(ix),
                    live,
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_model(ix, cx)),
                )
            })
            .collect();
        overlay::menu_layer(
            &self.launcher.models,
            anchor,
            now,
            reduced,
            rows,
            cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_model_menu(cx)),
        )
    }

    /// The launcher itself: what eave looks like when nothing is running.
    ///
    /// Positioned exactly where Slice M's "no session selected" ghost text
    /// was — an absolute overlay above the composer's clearance, outside
    /// the edge-fade scope and outside the virtualized list, so it can
    /// never participate in a row height.
    fn launcher_view(&self, error: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let cwd = std::env::current_dir()
            .map(|p| tilde(&p))
            .unwrap_or_else(|_| "the current directory".into());
        let selected = self.launcher.provider;
        let p = self.provider();
        let model = self.launcher.model.unwrap_or(launcher::MODEL_DEFAULT);
        // The entrance rides an INNER div: this box is absolute, and
        // `relative()` on it would destroy that positioning outright (the
        // scar Slice M's note left).
        let t = motion::entrance_t("in:launcher", motion::FADE_IN);

        div()
            .debug_selector(|| "launcher".into())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            // Not `COMPOSER_CLEARANCE` — the composer is not mounted while
            // this is up (see the render chain), so reserving its height
            // would push the launcher off centre by half a composer.
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .opacity(t)
                    .relative()
                    .top(px(motion::ENTRANCE_RISE * (1.0 - t)))
                    .w_full()
                    .max_w(px(theme::LAUNCHER_MAX_W))
                    .px(px(theme::GUTTER))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(theme::LAUNCHER_GAP))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(theme::SPACE_1))
                            .child(
                                div()
                                    .debug_selector(|| "launcher-title".into())
                                    .text_size(px(theme::LAUNCHER_TITLE_SIZE))
                                    .line_height(px(theme::LAUNCHER_TITLE_LINE))
                                    .font_weight(theme::WEIGHT_STRONG)
                                    .text_color(theme::text())
                                    .child("Start a session"),
                            )
                            .child(
                                // Where it will run, stated before it runs
                                // rather than discovered afterwards. The
                                // cwd is the single most consequential
                                // fact about a coding agent's session.
                                div()
                                    .debug_selector(|| "launcher-cwd".into())
                                    .text_size(px(theme::SMALL_SIZE))
                                    .line_height(px(theme::SMALL_LINE))
                                    .text_color(theme::text_tertiary())
                                    .child(format!("in {cwd}")),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "provider-strip".into())
                            // Capped at three tiles wide so six providers
                            // wrap 3+3 rather than however many happen to
                            // fit. Six across does not fit the transcript
                            // column at eave's default window size with
                            // both rails open, and the first build's 4+2
                            // read as an accident — which is what an
                            // uncapped `flex_wrap` always eventually does.
                            .max_w(px(theme::STRIP_MAX_W))
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .justify_center()
                            .gap(px(theme::TILE_GAP))
                            .children(PROVIDERS.iter().enumerate().map(|(ix, provider)| {
                                launcher::tile(
                                    ix,
                                    provider,
                                    ix == selected,
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.pick_provider(ix, cx)
                                    }),
                                )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(theme::SPACE_2))
                            // The model picker. A button that opens P1's
                            // menu — the launcher is the overlay layer's
                            // first product consumer.
                            .child(
                                div()
                                    .id("launcher-model")
                                    .debug_selector(|| "launcher-model".into())
                                    .px(px(theme::SPACE_3))
                                    .py(px(theme::SPACE_1))
                                    .rounded(px(theme::RADIUS_BUTTON))
                                    .border_1()
                                    .border_color(theme::border())
                                    .text_size(px(theme::LABEL_SIZE))
                                    .line_height(px(theme::LABEL_LINE))
                                    .text_color(theme::text_secondary())
                                    .hover_fade(
                                        "launcher-model",
                                        theme::wash_rest(),
                                        theme::overlay(),
                                    )
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        let at = window.mouse_position();
                                        this.open_model_menu(
                                            point(at.x, at.y + px(theme::MENU_ANCHOR_GAP)),
                                            cx,
                                        );
                                    }))
                                    .child(format!("{model}  ⌄")),
                            )
                            .child(
                                div()
                                    .id("launcher-start")
                                    .debug_selector(|| "launcher-start".into())
                                    .px(px(theme::SPACE_4))
                                    .py(px(theme::SPACE_1))
                                    .rounded(px(theme::RADIUS_BUTTON))
                                    .bg(theme::inverse())
                                    .text_size(px(theme::LABEL_SIZE))
                                    .line_height(px(theme::LABEL_LINE))
                                    .font_weight(theme::WEIGHT_LABEL)
                                    .text_color(theme::on_inverse())
                                    .cursor_pointer()
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.launch(cx)),
                                    )
                                    .child(format!("Start {}", p.name)),
                            ),
                    )
                    // The engine's own words on a refusal. Not a toast and
                    // not a log line: the screen that asked is the screen
                    // that answers.
                    .children(error.map(|e| {
                        div()
                            .debug_selector(|| "launcher-error".into())
                            .max_w(px(theme::LAUNCHER_MAX_W))
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::danger())
                            .child(e.to_string())
                    })),
            )
            .into_any_element()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Slice N2: open the frame FIRST, before anything below reads a
        // hover wash or an entrance. `begin_frame` is where stale entries
        // are pruned, and a read that beat the prune would see a dead
        // element's entry and skip its own entrance (motion.rs).
        //
        // Time comes from the executor, never `Instant::now()` — N1's rule,
        // and what lets the harness drive every tween off the fake clock.
        motion::begin_frame(cx.background_executor().now(), cx.reduce_motion());

        // Perf probe: measure inter-frame time while force-scrolling the
        // 10k transcript top to bottom (spec 04 §Acceptance).
        if let Some(perf) = &mut self.perf {
            if !perf.done {
                let now = Instant::now();
                if let Some(last) = perf.last_frame.replace(now) {
                    perf.frame_times.push(now.duration_since(last));
                }
                let cpu = thread_cpu_time();
                if let Some(last) = perf.last_cpu.replace(cpu) {
                    perf.cpu_times.push(cpu.saturating_sub(last));
                }
                let state = self.transcript.read(cx);
                perf.item_ix += PERF_ROWS_PER_FRAME;
                let last_ix = state.display.len().saturating_sub(1);
                state.list.scroll_to(ListOffset {
                    item_ix: perf.item_ix.min(last_ix),
                    offset_in_item: px(0.),
                });
                if perf.item_ix >= last_ix && perf.frame_times.len() > 60 {
                    perf.done = true;
                    // Skip startup (window creation, first measures); the
                    // acceptance is about steady-state scrolling. Inter-
                    // frame deltas on a vsync-locked display sit at the
                    // refresh period when healthy — a frame that blew the
                    // budget shows up as a missed vsync (≥2 periods).
                    let mut t: Vec<Duration> = perf.frame_times.iter().skip(10).copied().collect();
                    t.sort();
                    let p = |q: f64| t[((t.len() - 1) as f64 * q) as usize];
                    let missed = t
                        .iter()
                        .filter(|d| **d > Duration::from_micros(25_000))
                        .count();
                    let mut c: Vec<Duration> = perf.cpu_times.iter().skip(10).copied().collect();
                    // Where did the worst CPU frame land? Deterministic
                    // position across runs = content-driven (e.g. first-use
                    // font shaping); wandering position = load noise.
                    let max_at = perf
                        .cpu_times
                        .iter()
                        .enumerate()
                        .skip(10)
                        .max_by_key(|(_, d)| **d)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    c.sort();
                    let pc = |q: f64| c[((c.len() - 1) as f64 * q) as usize];
                    println!(
                        "PERF cpu_max_at_frame={max_at} (~row {})",
                        max_at * PERF_ROWS_PER_FRAME
                    );
                    println!(
                        "PERF frames={} rows={} wall[p50={:?} p95={:?} p99={:?} max={:?} missed_vsync={}] cpu[p50={:?} p95={:?} p99={:?} max={:?}]",
                        t.len(),
                        last_ix + 1,
                        p(0.5),
                        p(0.95),
                        p(0.99),
                        t.last().unwrap(),
                        missed,
                        pc(0.5),
                        pc(0.95),
                        pc(0.99),
                        c.last().unwrap(),
                    );
                    cx.quit();
                } else {
                    let entity = self.transcript.clone();
                    window.on_next_frame(move |_, cx| {
                        entity.update(cx, |_, cx| cx.notify());
                    });
                }
            }
        }

        // Before the immutable reads below: this one takes a pulse lease.
        let caret_lit = self.caret_lit(cx);
        // Slice N3: ONE ask for the whole window, here in the root
        // render pass. The shell decides once whether anything is
        // mid-flight, takes one lease on the pulse clock for it, and
        // hands the same phase to every indicator — which is what keeps
        // them phase-locked (N1 §1), and what keeps a window with nothing
        // in flight from constructing the clock at all.
        let working = self.work_in_flight(cx);
        let view = cx.entity_id();
        let pulse = loaders::pulse(working, view, cx);

        let transcript_entity = self.transcript.clone();
        let state = self.transcript.read(cx);
        let list_state = state.list.clone();
        // The ↓-bottom affordance needs something to return to: never on an
        // empty log (the Stage E cosmetic bug), only when follow is broken.
        let show_affordance = state.scroll != ScrollMode::Follow && !state.display.is_empty();
        let rail_collapsed = self.sessions.read(cx).collapsed;
        let files_collapsed = self.files.read(cx).collapsed;
        let composer_text = self.composer.read(cx).text.clone();
        let has_draft = !composer_text.is_empty();
        let pending_here = self
            .transcript
            .read(cx)
            .transcript
            .pending_approvals()
            .count();
        // Slice M (§3.2): what an empty log actually means. Three cases,
        // and only two of them are facts this shell can back.
        //   - nothing subscribed -> "no session selected"
        //   - subscribed, and the engine's own count says the journal is
        //     empty (last_seq 0) -> "nothing sent yet"
        //   - subscribed, journal NOT empty, rows not here yet -> the
        //     replay is still in flight. That is a LOADING state, and
        //     Slice M builds no loaders; "nothing sent yet" over a session
        //     with 200 messages would be a lie for the length of a round
        //     trip, so this says nothing at all, exactly as before.
        // Slice Q2 took the third case away from this note and gave it to
        // the launcher: "no session selected" was a sentence where a
        // screen belonged. The other two are unchanged, and the one that
        // says nothing still says nothing — a replay in flight over a
        // 200-message journal must not be captioned "nothing sent yet" for
        // the length of a round trip.
        let show_launcher = state.session_id.is_none();
        let launch_error = state.launch_error.clone();
        let empty_note: Option<&'static str> = if !state.display.is_empty() {
            None
        } else if let Some(sid) = state.session_id.as_deref() {
            self.sessions
                .read(cx)
                .sessions
                .iter()
                .find(|s| s.session_id == sid)
                .filter(|s| s.last_seq == 0)
                .map(|_| "nothing sent yet")
        } else {
            None
        };

        let transcript_el = list(list_state, move |ix, _window, cx| {
            transcript_entity.read_with(cx, |state, _| Shell::render_row(state, ix, pulse))
        })
        .size_full();

        let composer = Self::column(frost(
            theme::RADIUS_COMPOSER,
            div()
                .w_full()
                .rounded(px(theme::RADIUS_COMPOSER))
                .border_1()
                .border_color(theme::border_strong())
                .bg(theme::chrome_glass())
                .shadow(theme::shadow_panel())
                .p(px(theme::SPACE_3))
                .child(
                    div()
                        .min_h(px(theme::COMPOSER_MIN_H))
                        .px(px(theme::SPACE_1))
                        .pt(px(theme::SPACE_HALF))
                        .text_size(px(theme::BODY_SIZE))
                        .line_height(px(theme::COMPOSER_LINE))
                        .text_color(theme::text())
                        .child(if has_draft {
                            // The blink is a COLOUR-ONLY run split, never a
                            // glyph swap and never a sibling element. The
                            // caret has to stay inside the text so it wraps
                            // with it, and cosmic-text's `Attrs::compatible`
                            // ignores colour — so the two runs shape as one
                            // contiguous string and the layout is byte-
                            // identical whether the caret is lit or not.
                            // (Same property the streaming veil relies on;
                            // `VeiledBlock::text_runs`.)
                            let caret = "▏";
                            let font = gpui::font(theme::SANS);
                            let run = |len: usize, color: Hsla| TextRun {
                                len,
                                font: font.clone(),
                                color,
                                background_color: None,
                                underline: None::<UnderlineStyle>,
                                strikethrough: None::<StrikethroughStyle>,
                            };
                            let runs = vec![
                                run(composer_text.len(), theme::text()),
                                run(
                                    caret.len(),
                                    theme::text().opacity(if caret_lit { 1.0 } else { 0.0 }),
                                ),
                            ];
                            div()
                                .debug_selector(|| "composer-text".into())
                                .child(
                                    StyledText::new(format!("{composer_text}{caret}"))
                                        .with_runs(runs),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .flex()
                                .flex_row()
                                .child(div().text_color(theme::text()).child("▏"))
                                .child(
                                    div()
                                        .text_color(theme::text_tertiary())
                                        .child("Message eave…"),
                                )
                                .into_any_element()
                        }),
                )
                .child(
                    div()
                        .mt(px(theme::SPACE_2))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .debug_selector(|| "composer-hint".into())
                                .text_size(px(theme::MICRO_SIZE))
                                .text_color(if pending_here > 0 {
                                    theme::warning()
                                } else {
                                    theme::text_tertiary()
                                })
                                // Slice K, capture N (2026-08-18): the CLI
                                // swallows a user message sent while a
                                // permission is pending — no echo, no ack,
                                // nothing until the decision lands. Under
                                // eave's order-paired delivery contract
                                // that would show as a send stuck pending
                                // with no stated reason, so the composer
                                // says the reason.
                                .child(SharedString::from(if pending_here > 0 {
                                    "return to send — queued behind the approval above until \
                                     you answer"
                                        .to_string()
                                } else {
                                    "return to send".to_string()
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .size(px(theme::SEND_BUTTON))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                // Slice P3: no text_size/font_weight/text_color
                                // — the child is a glyph, an SVG takes its size
                                // from its own box, and gpui does NOT cascade
                                // text colour into one (see `icons::icon`). The
                                // arm colour is handed to the glyph directly.
                                .id("send")
                                .debug_selector(|| "send".into())
                                // Slice M (§2.2). A filled control REPLACES its bg
                                // rather than compositing a wash over it, so both
                                // states have to name a hovered fill.
                                //
                                // Arming the button (first character typed) swaps
                                // BOTH ends of the fade under a pointer that may
                                // be sitting on it. That is a step, not a tween —
                                // the fade interpolates between whatever the two
                                // ends are this frame, so the armed colour arrives
                                // at once and only the hover progress is smooth.
                                .hover_fade(
                                    "send",
                                    if has_draft {
                                        theme::inverse()
                                    } else {
                                        theme::overlay_strong()
                                    },
                                    if has_draft {
                                        theme::inverse_hover()
                                    } else {
                                        theme::overlay_strongest()
                                    },
                                )
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send_current(cx);
                                }))
                                .child(icons::tagged_icon(
                                    icons::ARROW_UP,
                                    theme::ICON_SM,
                                    if has_draft {
                                        theme::on_inverse()
                                    } else {
                                        theme::text_tertiary()
                                    },
                                    "send-glyph",
                                )),
                        ),
                ),
        ))
        .flex_none();

        // Slice N3: the healthy-turn half of the attention slot. The
        // notice owns the slot whenever it has anything to say, so the
        // two are mutually exclusive by construction and never stack.
        let notice = self.attention_notice(cx);
        let trailer = if notice.is_none() {
            self.work_trailer(pulse, cx)
        } else {
            None
        };

        // Slice P1: built before the chain rather than inside a `.when`,
        // so neither closure has to capture `self` and `cx` alongside the
        // element being built. Both are `None` in the shipped app.
        let demo_trigger = overlay::demo_enabled().then(|| self.demo_trigger(cx));
        let demo_layer = self.demo_layer(cx);
        // Slice Q2, built here for the same reason: the launcher and its
        // model menu both need `self` and `cx`, and the element chain
        // below has neither to spare.
        let launcher_el = show_launcher.then(|| self.launcher_view(launch_error.as_deref(), cx));
        let launcher_menu = self.launcher_menu_layer(cx);

        // Opaque window by default, custom titlebar — locked decision
        // (spec 03 §No-fork consequences) plus the Stage F visual pass.
        // This root bg is the glass ground.
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::canvas())
            .text_color(theme::text())
            .font_family(theme::SANS)
            .text_size(px(theme::BODY_SIZE))
            .line_height(px(theme::BODY_LINE))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            // Slice K: answering is a real action with a real binding, not
            // a character the composer happens not to want.
            .on_action(cx.listener(|this, _: &ApproveOldest, _, cx| this.decide_oldest(true, cx)))
            .on_action(cx.listener(|this, _: &DenyOldest, _, cx| this.decide_oldest(false, cx)))
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .child(self.titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .when(!rail_collapsed, |d| d.child(self.rail(cx, pulse)))
                    .child(
                        // The content well: an inset rounded card floating
                        // over the glass ground (comet's shell model) — the
                        // rails sit on the bare frost around it, the card's
                        // black wash keeps a solid floor under the reading
                        // column.
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .mx(px(theme::PLATE_MARGIN))
                            .mb(px(theme::PLATE_MARGIN))
                            .rounded(px(theme::RADIUS_PANEL))
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::content_plate())
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                // Stage H glass: the list's viewport runs
                                // the full column height and the composer
                                // stack FLOATS over it (content passes
                                // beneath the frost when scrolled; the
                                // trailing spacer keeps Follow's newest
                                // row above the glass).
                                div()
                                    .flex_1()
                                    .min_h(px(0.))
                                    .relative()
                                    // Slice M: the log dissolves into the
                                    // well's edges instead of ending on a
                                    // hard cut. LOG_TOP_PAD moved INSIDE
                                    // the faded wrapper on purpose — it
                                    // is the resting pad the top band is
                                    // sized to (TRANSCRIPT_FADE_BAND_TOP),
                                    // so scroll-top stays fully opaque and
                                    // only content actually scrolling away
                                    // fades. Not gated on scroll state:
                                    // comet gated theirs and got a report
                                    // of the top staying unfaded for one
                                    // frame on every session switch. The
                                    // floating chrome below is a SIBLING,
                                    // outside the scope — the pill and the
                                    // composer must not fade along with
                                    // the content they occlude.
                                    .child(
                                        edge_faded(
                                            theme::TRANSCRIPT_FADE_BAND_TOP,
                                            true,
                                            true,
                                            div()
                                                .size_full()
                                                .pt(px(theme::LOG_TOP_PAD))
                                                .child(transcript_el),
                                        )
                                        .band_top(theme::TRANSCRIPT_FADE_BAND_TOP)
                                        .band_bottom(theme::TRANSCRIPT_FADE_BAND_BOTTOM),
                                    )
                                    // Slice M (§3.2): an overlay, so it is
                                    // outside both the fade scope and the
                                    // virtualized list and can never touch
                                    // a row height. Held above the
                                    // composer's clearance so it is never
                                    // read through the frost.
                                    .children(empty_note.map(|note| {
                                        // §1.3. The entrance rides an INNER
                                        // div: the note's own box is
                                        // absolute (it is an overlay pinned
                                        // above the composer's clearance)
                                        // and `relative()` would destroy
                                        // that positioning outright.
                                        let t =
                                            motion::entrance_t("in:empty-note", motion::FADE_IN);
                                        div()
                                            .debug_selector(|| "transcript-empty".into())
                                            .absolute()
                                            .top_0()
                                            .left_0()
                                            .right_0()
                                            .bottom(px(theme::COMPOSER_CLEARANCE))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(theme::SMALL_SIZE))
                                            .line_height(px(theme::SMALL_LINE))
                                            .text_color(theme::text_tertiary())
                                            .child(
                                                div()
                                                    .opacity(t)
                                                    .relative()
                                                    .top(px(motion::ENTRANCE_RISE * (1.0 - t)))
                                                    .child(note),
                                            )
                                    }))
                                    // Slice Q2 — the launcher, in exactly
                                    // the slot the note it replaced used:
                                    // an overlay outside both the fade
                                    // scope and the virtualized list, so
                                    // it can never touch a row height.
                                    .children(launcher_el)
                                    .when(show_affordance, |d| {
                                        // §1.6, 180 ms. comet approximates a
                                        // `scale 0.96→1` with fade + a 2 px
                                        // rise because gpui divs have no
                                        // scale transform at the pinned rev.
                                        // The rise rides the ABSOLUTE inset
                                        // here rather than a relative one —
                                        // an absolutely positioned box moves
                                        // nothing else by definition.
                                        let t =
                                            motion::entrance_t("in:to-bottom", motion::DIALOG_IN);
                                        d.child(
                                            div()
                                                .absolute()
                                                .opacity(t)
                                                .bottom(px(theme::COMPOSER_CLEARANCE
                                                    + theme::SPACE_3
                                                    - motion::DIALOG_RISE * (1.0 - t)))
                                                .left_0()
                                                .right_0()
                                                .flex()
                                                .justify_center()
                                                .child(frost(
                                                    theme::RADIUS_PILL,
                                                    div()
                                                        .px(px(theme::SPACE_3))
                                                        .py(px(theme::SPACE_1))
                                                        .rounded_full()
                                                        .border_1()
                                                        .border_color(theme::border_strong())
                                                        .shadow(theme::shadow_panel())
                                                        .text_size(px(theme::LABEL_SIZE))
                                                        .text_color(theme::text_secondary())
                                                        .id("to-bottom")
                                                        .debug_selector(|| "to-bottom".into())
                                                        // Slice M (§2.2). Same alpha,
                                                        // lighter tint — the pill's job
                                                        // is to occlude the log behind
                                                        // it, and a thinner hover would
                                                        // undo that.
                                                        .hover_fade(
                                                            "to-bottom",
                                                            theme::chrome_glass(),
                                                            theme::chrome_glass_hover(),
                                                        )
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.transcript.update(cx, |t, cx| {
                                                                t.scroll = ScrollMode::Follow;
                                                                t.assert_scroll();
                                                                t.list.scroll_to_end();
                                                                cx.notify();
                                                            });
                                                        }))
                                                        .flex()
                                                        .flex_row()
                                                        .items_center()
                                                        .gap(px(theme::SPACE_1))
                                                        .child(icons::tagged_icon(
                                                            icons::ARROW_DOWN,
                                                            theme::ICON_SM,
                                                            theme::text_secondary(),
                                                            "to-bottom-glyph",
                                                        ))
                                                        .child("bottom"),
                                                )),
                                        )
                                    })
                                    .child(
                                        div()
                                            .absolute()
                                            .bottom_0()
                                            .left_0()
                                            .right_0()
                                            .pb(px(theme::SPACE_3))
                                            .flex()
                                            .flex_col()
                                            .children(self.conn_panel(cx))
                                            .children(notice)
                                            .children(trailer)
                                            // Slice Q2: no session, no
                                            // composer. A draft typed at
                                            // the launcher has nothing to
                                            // be addressed to — the rest
                                            // loop drops a `Send` because
                                            // holding it to replay into
                                            // whatever opens next would be
                                            // worse — so an input that
                                            // silently eats what you type
                                            // is the one thing this must
                                            // not be.
                                            .children((!show_launcher).then_some(composer)),
                                    ),
                            )
                            .child(self.footer(cx)),
                    )
                    .when(!files_collapsed, |d| d.child(self.files_rail(cx, pulse))),
            )
            // Slice P1: the overlay layer mounts at the ROOT and nowhere
            // else — `deferred` fixes paint order, but layout stays where
            // it is written, and the transcript is a virtualized list that
            // caches row heights. See `overlay::menu_layer`.
            .children(demo_trigger)
            .children(demo_layer)
            // After `demo_layer` so that if both are ever mounted at once
            // the launcher's menu is on top — the launcher is the product
            // surface and the demo is a test fixture.
            .children(launcher_menu);

        // Slice N1: the motion tail. Anything hand-driven that is mid-flight
        // claims this frame during the pass above; the root asks once, here,
        // and schedules exactly one more frame. Nothing mid-flight -> nothing
        // scheduled, which is the entire point of the shape (motion.rs).
        // Reduced motion never schedules: gpui honours the flag inside
        // `with_animation`, but `request_animation_frame` is the raw call
        // underneath it and honours nothing on its own — gpui's own doc
        // comment says to check the flag here.
        //
        // Slice N2 is the first thing to claim it, and it claims from HERE
        // rather than from the thirteen elements themselves: a hover read
        // inside the virtualized list happens in prepaint, after this tail
        // has already run, so an element that raised its own claim would
        // miss its frame (N1's `mark_active` doc comment). One ask, from
        // the root render, over the whole store.
        if motion::fades_active() {
            motion::mark_active();
        }
        if motion::take_active() && !cx.reduce_motion() {
            window.request_animation_frame();
        }
        root
    }
}

// ---------------------------------------------------------------------------
// Frost — the fork's BackdropBlur behind floating chrome (Stage H).
// The child paints inside ONE scene layer with the blur painted first:
// the single layer makes the blur/content order structural. (With
// bounds-tree ordering a repaint elsewhere can reassign the card's quads
// below the blur and wash them away — comet hit exactly this; their
// frost element documents it.)

struct Frost {
    corner_radius: f32,
    child: AnyElement,
}

/// Blur the scene behind `child` (radius [`theme::BLUR_CHROME`]), then
/// paint it. `corner_radius` must match the child card's rounding.
fn frost(corner_radius: f32, child: impl IntoElement) -> Frost {
    Frost {
        corner_radius,
        child: child.into_any_element(),
    }
}

impl Element for Frost {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_layer(bounds, |window| {
            window.paint_backdrop_blur(
                bounds,
                Corners::all(px(self.corner_radius)),
                px(theme::BLUR_CHROME),
            );
            self.child.paint(window, cx);
        });
    }
}

impl IntoElement for Frost {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

// ---------------------------------------------------------------------------
// Entry

pub fn run(data_dir: PathBuf) -> ! {
    // Recorded before anything renders: the friction log resolves against
    // it (Stage L), and it must not differ from the dir the feed loop uses.
    let _ = DATA_DIR.set(data_dir.clone());
    let replay: Option<(PathBuf, f64)> = std::env::var("EAVE_REPLAY").ok().map(|p| {
        let speed = std::env::var("EAVE_REPLAY_SPEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);
        (PathBuf::from(p), speed)
    });
    let perf_10k = std::env::var("EAVE_PERF")
        .map(|v| v == "10k")
        .unwrap_or(false);

    // Slice P3: the one place an `Application` is built, so the one place
    // the icon set can be registered. `bootstrap` cannot do it — assets are
    // an `Application`-level concern and gpui exposes no `App` setter — which
    // is why the harness renders every glyph as empty coverage (icons.rs).
    application()
        .with_assets(icons::Icons)
        .run(move |cx: &mut App| {
            let queues = Arc::new(Queues::default());
            let (wake_tx, wake_rx) = futures::channel::mpsc::channel::<()>(1);
            let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<Command>();

            // Feed source: live engine, or fixture replay (Stage D acceptance
            // harness) — the pump and entities cannot tell the difference.
            {
                let plumbing = Plumbing {
                    queues: Arc::clone(&queues),
                    wake: wake_tx,
                };
                match replay.clone() {
                    Some((path, speed)) => {
                        std::thread::spawn(move || replay_loop(path, speed, plumbing));
                        drop(cmd_rx);
                    }
                    None if perf_10k => drop(cmd_rx), // no feed at all
                    None => {
                        let data_dir = data_dir.clone();
                        std::thread::spawn(move || {
                            tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("tokio runtime")
                                .block_on(feed_loop(data_dir, plumbing, cmd_rx));
                        });
                    }
                }
            }

            bootstrap(
                cx,
                Feed {
                    queues,
                    wake_rx,
                    cmd_tx,
                },
            );
        });
    std::process::exit(0);
}

/// One feed's worth of plumbing, handed to [`bootstrap`]: the shared
/// queues, the wake receiver the pump blocks on, and the command sender
/// the Shell emits intent through. `run()` wires the far ends into a
/// live engine or a fixture replay; the stage-J harness holds them
/// directly — the pump and entities cannot tell the difference.
struct Feed {
    queues: Arc<Queues>,
    wake_rx: futures::channel::mpsc::Receiver<()>,
    cmd_tx: tokio_mpsc::UnboundedSender<Command>,
}

/// Whether the operator has asked for less motion (Slice M, §1.22).
///
/// `EAVE_REDUCE_MOTION` wins when set (`1`/`true` on, anything else off).
/// It exists because the OS setting is not something a test — or a
/// verification pass — can toggle, and a preference with no way to
/// exercise its other branch is a preference nobody has ever seen work.
///
/// Otherwise macOS's own accessibility preference (System Settings →
/// Accessibility → Display → Reduce motion), which lives in the
/// `com.apple.universalaccess` domain. gpui reads this NOWHERE: at our
/// pin, `App::reduce_motion` defaults to false and the only production
/// caller of `set_reduce_motion` in the whole tree is Zed setting it from
/// its own settings file. So honouring the system preference means eave
/// reading it, or it is not honoured at all.
///
/// Read ONCE, at boot: `cfprefsd` caches per process, so a change made
/// while eave is running takes effect on relaunch. Stated rather than
/// papered over with a polling timer — a polling timer is exactly the
/// per-frame machinery this slice does not build.
fn reduce_motion_preference() -> bool {
    if let Some(forced) =
        reduce_motion_override(std::env::var("EAVE_REDUCE_MOTION").ok().as_deref())
    {
        return forced;
    }
    #[cfg(target_os = "macos")]
    {
        macos_reduce_motion().unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// The env half of [`reduce_motion_preference`], split out as a pure
/// function so both branches are tested without touching process env
/// (same reason `resolve_friction_log` is pure).
fn reduce_motion_override(var: Option<&str>) -> Option<bool> {
    var.map(|v| matches!(v, "1" | "true"))
}

/// `CFPreferencesCopyAppValue("reduceMotion", "com.apple.universalaccess")`.
///
/// `None` means the key is absent or is not a boolean — which is the
/// normal state of a machine where the operator has never touched the
/// setting, not an error worth reporting.
#[cfg(target_os = "macos")]
fn macos_reduce_motion() -> Option<bool> {
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFTypeRef;
        fn CFPreferencesCopyAppValue(key: CFTypeRef, application_id: CFTypeRef) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;
        fn CFRelease(cf: CFTypeRef);
    }

    /// `kCFStringEncodingUTF8`.
    const UTF8: u32 = 0x0800_0100;

    // SAFETY: every pointer below is either null-checked before use or
    // came from a Create/Copy call whose ownership we then release. No
    // pointer outlives this function.
    unsafe {
        let mk = |s: &str| -> CFTypeRef {
            CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0)
        };
        let key = mk("reduceMotion");
        let domain = mk("com.apple.universalaccess");
        if key.is_null() || domain.is_null() {
            if !key.is_null() {
                CFRelease(key);
            }
            if !domain.is_null() {
                CFRelease(domain);
            }
            return None;
        }
        let value = CFPreferencesCopyAppValue(key, domain);
        CFRelease(key);
        CFRelease(domain);
        if value.is_null() {
            return None;
        }
        let out =
            (CFGetTypeID(value) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value) != 0);
        CFRelease(value);
        out
    }
}

/// Was `EAVE_LAUNCH_AT_BOOT=1` asked for? (Q2's rig hook — see the call
/// site in [`bootstrap`], and [`Shell::launch_at_boot`] for what it does.)
///
/// Split out from the hook so the environment read is the ONLY untested
/// part of it. It must stay a bare read: the harness boots this same
/// `bootstrap`, and an env var is process-wide, so a test that set it would
/// press start in every other test running beside it.
fn launch_at_boot_requested() -> bool {
    std::env::var("EAVE_LAUNCH_AT_BOOT").is_ok_and(|v| v == "1")
}

/// Everything downstream of the feed — entities, elapsed ticker, pump,
/// window, Shell — shared verbatim by `run()` and the harness tests:
/// production and tests walk one spine (stage J).
fn bootstrap(cx: &mut App, feed: Feed) -> WindowHandle<Shell> {
    let Feed {
        queues,
        mut wake_rx,
        cmd_tx,
    } = feed;
    let perf_10k = std::env::var("EAVE_PERF")
        .map(|v| v == "10k")
        .unwrap_or(false);

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        // Slice K: answering is a first-class act, so it gets real
        // bindings rather than living only inside the key handler.
        KeyBinding::new("cmd-shift-a", ApproveOldest, None),
        KeyBinding::new("cmd-shift-d", DenyOldest, None),
        // Slice Y: the three a macOS user reaches for without thinking.
        // Bound here rather than expressed as `OsAction`s because eave's
        // gpui pin has no Minimize/Close variants (see `app_menus`).
        KeyBinding::new("cmd-w", crate::app_menus::CloseWindow, None),
        KeyBinding::new("cmd-m", crate::app_menus::Minimize, None),
        KeyBinding::new("cmd-h", crate::app_menus::Hide, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    // The menu bar. Registered HERE rather than in `main` so the harness
    // boots the same wiring the app does — and note this is not what makes
    // ⌘Q work (the binding above already did, and always has). It is what
    // makes the bar exist at all: gpui calls `setMainMenu_` only from
    // `set_menus`, so without this `NSApp.mainMenu` is nil and the
    // auto-hidden system bar has nothing to reveal.
    crate::app_menus::init(cx); // handlers only — the bar goes in last

    // Slice M (§1.22): honoured for the one animation eave has. gpui
    // applies this inside `with_animation` itself — a oneshot renders its
    // end state and schedules no frames — so the veil becomes "the text
    // is simply there", which is the correct reduced form of a fade.
    // Set here rather than in `run` so the harness boots the same wiring
    // production does (Stage J).
    cx.set_reduce_motion(reduce_motion_preference());

    let conn = cx.new(|_| ConnectionState {
        status: ConnEvent::Connecting,
    });
    let sessions = cx.new(|_| SessionsState {
        sessions: Vec::new(),
        collapsed: false,
        // Stage AC: eave opens on the working set, always. The toggle is
        // a mode you enter and leave, never a state you boot into.
        show_archived: false,
        attention: None,
        row_notes: std::collections::HashMap::new(),
    });
    let files = cx.new(|_| FilesState {
        // Perf runs have no feed: seed a synthetic snapshot so the
        // measured layout includes a populated files rail — and (B2)
        // a cap-size expanded diff, so the long-diff render path is
        // measured at its worst, not at a friendly size.
        snapshot: perf_10k.then(|| ("perf:10k".to_string(), Some(synthetic_files_snapshot()))),
        collapsed: false,
        scope: FileScope::WorkingTree,
        diff: perf_10k.then(|| DiffSlot {
            session: "perf:10k".into(),
            path: "crates/eave-app/src/ui.rs".into(),
            fetch: DiffFetch::Ready(Arc::new(synthetic_long_diff())),
        }),
    });
    let replay_assert = std::env::var("EAVE_REPLAY_ASSERT").is_ok();
    let transcript = cx.new(|_| {
        let mut t = TranscriptState::new();
        if perf_10k {
            perf_fill_10k(&mut t);
        }
        if replay_assert {
            t.assert_replay = Some(ReplayAssert {
                frozen: None,
                jumps: 0,
            });
        }
        t
    });

    // Steady elapsed reporting (build directive): while a notice
    // stands, re-render the badge once a second so its elapsed text
    // moves. Costs one entity read per second when idle.
    {
        let sessions = sessions.clone();
        let transcript = transcript.clone();
        cx.spawn(async move |cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if sessions.read_with(cx, |s, _| s.notice_standing()) {
                sessions.update(cx, |_, cx| cx.notify());
            }
            // Slice N3: the working trailer's elapsed has to move too,
            // and it must NOT lean on the pulse clock for it. Under
            // reduced motion no lease is ever taken and no pulse timer
            // exists, so a trailer driven only by the loader would freeze
            // its clock for exactly the people who asked for less motion.
            // This second is the trailer's only guaranteed heartbeat.
            if transcript.read_with(cx, |t, _| t.in_turn()) {
                transcript.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    }

    // The pump (spec 03 §Wake & notify): one wake per burst, drain
    // every non-empty queue, ≤1 notify per entity per burst.
    {
        let queues = Arc::clone(&queues);
        let conn = conn.clone();
        let sessions = sessions.clone();
        let files = files.clone();
        let transcript = transcript.clone();
        let cmd_tx = cmd_tx.clone();
        cx.spawn(async move |cx| {
            while wake_rx.next().await.is_some() {
                let conn_events: Vec<ConnEvent> = queues.conn.lock().unwrap().drain(..).collect();
                if let Some(last) = conn_events.into_iter().last() {
                    conn.update(cx, |state, cx| {
                        state.status = last;
                        cx.notify();
                    });
                }
                // Attention is current-state, not a log: the last
                // frame of the burst wins.
                let attn_events: Vec<AttnEvent> = queues.attn.lock().unwrap().drain(..).collect();
                let rail_events: Vec<RailEvent> = queues.rail.lock().unwrap().drain(..).collect();
                // Files snapshots fold by replacement — last of the
                // burst wins; the render filters by session tag.
                let files_events: Vec<FilesEvent> =
                    queues.files.lock().unwrap().drain(..).collect();
                let diff_events: Vec<DiffEvent> = queues.diffs.lock().unwrap().drain(..).collect();
                if !files_events.is_empty() || !diff_events.is_empty() {
                    let cmd_tx = cmd_tx.clone();
                    files.update(cx, |state, cx| {
                        if let Some((sid, snap)) = files_events.into_iter().last() {
                            // A fresh snapshot is the diff's
                            // invalidation signal (B2): re-request
                            // the open pane, or close it when its
                            // file is no longer listed — "not
                            // changed anymore" is stated by the
                            // row's disappearance.
                            if let Some(slot) = &state.diff {
                                if slot.session == sid {
                                    let listed = snap.as_ref().is_some_and(|s| {
                                        s.state == GitState::Ok
                                            && s.files.iter().any(|f| f.path == slot.path)
                                    });
                                    if listed {
                                        let _ = cmd_tx.send(Command::Diff(slot.path.clone()));
                                    } else {
                                        state.diff = None;
                                    }
                                }
                            }
                            state.snapshot = Some((sid, snap));
                        }
                        // Diff replies fold only into the slot they
                        // were requested for — a reply that raced a
                        // switch or a re-click is discarded.
                        for (sid, path, res) in diff_events {
                            if let Some(slot) = &mut state.diff {
                                if slot.session == sid && slot.path == path {
                                    slot.fetch = match res {
                                        Ok(d) => DiffFetch::Ready(Arc::new(d)),
                                        Err(m) => DiffFetch::Failed(m),
                                    };
                                }
                            }
                        }
                        cx.notify();
                    });
                }
                let last_attn = attn_events.into_iter().last();
                let last_rail = rail_events.into_iter().last();
                let note_events: Vec<RowNoteEvent> =
                    queues.notes.lock().unwrap().drain(..).collect();
                if last_attn.is_some() || last_rail.is_some() || !note_events.is_empty() {
                    sessions.update(cx, |state, cx| {
                        if let Some(tagged) = last_attn {
                            state.attention = Some(tagged);
                        }
                        // Watch snapshots fold by replacement — the
                        // engine's set is the truth, never merged. Row
                        // ORDER is the one thing carried across (Stage AB):
                        // it is not an engine fact, and re-deciding it on
                        // every state change moves rows under the pointer.
                        if let Some(snapshot) = last_rail {
                            fold_rail(&mut state.sessions, snapshot);
                        }
                        // Worktree notes fold by replacement per
                        // session; a settled action clears its note.
                        for (sid, note) in note_events {
                            match note {
                                Some(n) => {
                                    state.row_notes.insert(sid, n);
                                }
                                None => {
                                    state.row_notes.remove(&sid);
                                }
                            }
                        }
                        cx.notify();
                    });
                }
                let feed_events: Vec<FeedEvent> = queues.feed.lock().unwrap().drain(..).collect();
                if !feed_events.is_empty() {
                    let cmd_tx = cmd_tx.clone();
                    let mut switched = false;
                    transcript.update(cx, |state, cx| {
                        for ev in feed_events {
                            match ev {
                                FeedEvent::Session { id } => {
                                    if state.session_id.as_deref() != Some(id.as_str()) {
                                        switched = true;
                                    }
                                    state.session_id = Some(id)
                                }
                                // Slice Q2. `reset()` as well as clearing
                                // the id: the launcher renders over the
                                // transcript column, and rows left behind
                                // it would be a dead session's content
                                // sitting under a live launcher.
                                FeedEvent::NoSession { error } => {
                                    if state.session_id.is_some() {
                                        switched = true;
                                    }
                                    state.session_id = None;
                                    state.reset();
                                    state.launch_error = error;
                                }
                                // Frames tagged with another session are
                                // stragglers from a dropped subscription
                                // — discard, never apply (no bleed).
                                FeedEvent::Reset { id } => {
                                    if state.session_id.as_deref() == Some(id.as_str()) {
                                        state.reset();
                                    }
                                }
                                FeedEvent::Records { id, epoch, records } => {
                                    if state.session_id.as_deref() != Some(id.as_str()) {
                                        continue;
                                    }
                                    if state.apply_records(epoch, &records).is_err() {
                                        // Out-of-sequence: drop local
                                        // state, resubscribe; a fresh
                                        // Reset heals (spec 03
                                        // §Streaming commit).
                                        state.reset();
                                        let _ = cmd_tx.send(Command::Resubscribe);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    });
                    // A real selection switch closes the diff pane:
                    // it belongs to the session that just left.
                    if switched {
                        files.update(cx, |state, cx| {
                            if state.diff.take().is_some() {
                                cx.notify();
                            }
                        });
                        // ...and moves the composer's draft with it. This is
                        // the ONLY writer of the composer's key: the swap
                        // rides the same fact the rest of the switch does,
                        // so a draft cannot end up parked under a session
                        // the shell is no longer showing.
                        //
                        // The scroll mode rides it too. `reset()` clears the
                        // transcript, the display, the list, the veil and the
                        // turn clock and did NOT clear this, so a session you
                        // switched into inherited the last one's mode: `Free`
                        // (no follow while it streams, and a ↓-bottom pill
                        // over a transcript already at its bottom) or a
                        // `Pinned { msg }` whose id then resolved against a
                        // transcript that never held it. Stage G caveat 2
                        // recorded that as "not code; watched" — it is code.
                        //
                        // Deliberately NOT inside `reset()`: two of its three
                        // callers are same-session replays (`FeedEvent::Reset`
                        // and the out-of-sequence heal), and forcing Follow
                        // there would yank a reader who had scrolled up back
                        // to the tail every time the engine resubscribed. A
                        // switch is the only moment the mode is stale.
                        let moved = transcript.update(cx, |state, _| {
                            state.scroll = ScrollMode::Follow;
                            state.assert_scroll();
                            // ...and the open tool cards go with it. They
                            // are keyed by tool id, and a tool id from the
                            // session that left can collide with one in the
                            // session arriving.
                            state.open_tools.clear();
                            let key = state.session_id.clone().unwrap_or_default();
                            state.composer.clone().map(|c| (c, key))
                        });
                        if let Some((composer, key)) = moved {
                            composer.update(cx, |c, cx| {
                                c.switch_to(key);
                                cx.notify();
                            });
                        }
                    }
                }
            }
        })
        .detach();
    }

    let bounds = Bounds::centered(None, size(px(theme::WINDOW_W), px(theme::WINDOW_H)), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(theme::WINDOW_MIN_W), px(theme::WINDOW_MIN_H))),
                // Glass is the only look (ruled 2026-08-17, opaque path deleted
                // 2026-08-19). theme.rs owns the tints this blur sits under.
                window_background: WindowBackgroundAppearance::Blurred,
                titlebar: Some(TitlebarOptions {
                    title: Some("eave".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(theme::TRAFFIC_X), px(theme::TRAFFIC_Y))),
                }),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(|cx| {
                    let composer = cx.new(|_| ComposerState {
                        text: String::new(),
                        key: String::new(),
                        drafts: HashMap::new(),
                        caret_anchor: None,
                    });
                    // Slice K: the approval card's buttons are built by a
                    // static row fn with no `Context`, so give the transcript
                    // state the two handles they need — the shell's own
                    // command channel and the composer they read the deny
                    // reason from.
                    let weak_shell = cx.weak_entity();
                    transcript.update(cx, |t, _| {
                        t.commands = Some(cmd_tx.clone());
                        t.composer = Some(composer.clone());
                        t.shell = Some(weak_shell);
                    });
                    cx.observe(&conn, |_, _, cx| cx.notify()).detach();
                    cx.observe(&sessions, |_, _, cx| cx.notify()).detach();
                    cx.observe(&files, |_, _, cx| cx.notify()).detach();
                    cx.observe(&transcript, |_, _, cx| cx.notify()).detach();
                    cx.observe(&composer, |_, _, cx| cx.notify()).detach();
                    Shell {
                        focus: cx.focus_handle(),
                        demo: DemoOverlay::default(),
                        rename: None,
                        launcher: LauncherUi::default(),
                        files_wait_since: Cell::new(None),
                        conn: conn.clone(),
                        sessions: sessions.clone(),
                        files: files.clone(),
                        transcript: transcript.clone(),
                        composer,
                        commands: cmd_tx.clone(),
                        perf: perf_10k.then(|| PerfProbe {
                            frame_times: Vec::new(),
                            cpu_times: Vec::new(),
                            last_cpu: None,
                            last_frame: None,
                            item_ix: 0,
                            done: false,
                        }),
                        friction: None,
                        friction_gen: 0,
                        friction_path_stated: false,
                        rail_scroll: ScrollHandle::new(),
                        files_scroll: ScrollHandle::new(),
                    }
                });
                let handle = shell.read(cx).focus.clone();
                window.focus(&handle, cx);
                // Slice P1: `EAVE_OVERLAY_DEMO=open` opens the menu on the
                // first frame, so the surface can be looked at rather than
                // only asserted. A fixed anchor rather than the pointer's,
                // because there is no pointer yet.
                if overlay::demo_opens_at_boot() {
                    shell.update(cx, |s, cx| s.open_demo_menu(point(px(240.), px(180.)), cx));
                }
                // Slice Q2: `EAVE_LAUNCH_AT_BOOT=1` presses the launcher's own
                // start button on the first frame.
                //
                // This exists because the launcher's HALF that a capture can
                // prove is the half that does nothing: a shot shows eave
                // resting instead of auto-opening, and cannot show a click
                // producing a session. The harness cannot cover it either —
                // it replaces the connection loop with plumbing, so
                // `Command::Launch` reaching a real `open_with_agent` is
                // exercised by no test in the suite. Same shape and same
                // reason as P1's `EAVE_OVERLAY_DEMO`.
                //
                // Slice AI: what it presses lives in `Shell::launch_at_boot`,
                // so the sequence is testable by the harness even though the
                // environment read is not.
                if launch_at_boot_requested() {
                    shell.update(cx, |s, cx| s.launch_at_boot(cx));
                }
                shell
            },
        )
        .expect("open window");
    // The bar, last: a `setMainMenu_` issued before the window exists and
    // before activation does not stick (see `app_menus::install`).
    crate::app_menus::install(cx);
    cx.activate(true);
    handle
}

/// Fill the transcript with 10k rows of real fixture-derived markdown and
/// park the scroll at the very top (spec 04 §Acceptance: scroll top to
/// bottom under budget).
fn perf_fill_10k(state: &mut TranscriptState) {
    let steps = crate::fixture::load(&crate::fixture::default_path()).unwrap_or_default();
    let mut base = Transcript::new();
    for step in &steps {
        if let SubscriptionItem::Events { epoch, records, .. } = &step.item {
            let _ = base.apply(*epoch, records);
        }
    }
    if base.rows.is_empty() {
        base.push_row(Row::Assistant {
            text: "fixture missing — synthetic filler\n\n```rust\nfn x() {}\n```".into(),
            live: false,
        });
    }
    while state.transcript.rows.len() < 10_000 {
        for row in &base.rows {
            state.transcript.push_row(row.clone());
        }
    }
    while state.transcript.rows.len() > 10_000 {
        state.transcript.rows.pop();
        state.transcript.revs.pop();
    }
    state.session_id = Some("perf:10k".into());
    state.display.update(&state.transcript);
    state.list.reset(state.display.len() + 1);
    state.scroll = ScrollMode::Free;
    state.assert_scroll();
    state.list.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
    });
}

#[cfg(test)]
#[path = "ui_harness.rs"]
mod harness;

#[cfg(test)]
mod tests {
    // -----------------------------------------------------------------
    // Slice AH — the eternal skeleton

    /// **Where the rail's patience comes from, and what it may not do.**
    ///
    /// A number that only has to make one test pass can be anything. This
    /// one is pinned from both sides against constants it does not own:
    ///
    ///   - **Below** by the engine's own ceiling. `refresh_files` shells
    ///     out to git and every call is bounded by `GIT_TIMEOUT`, so a
    ///     healthy engine whose first git call times out owes its first
    ///     frame that long. A rail that gave up inside that window would
    ///     call a live scan a failure — the opposite defect, and a louder
    ///     one, because it puts an error on screen where none happened.
    ///   - **Above** by what being wrong costs. The rail holds a pulse
    ///     lease while it waits, and a lease at 30 fps is +8.7 points of a
    ///     core (measured in N2). A minute of that on a window where
    ///     nothing is happening is not a loading state, it is the bug.
    ///
    /// The 30 s ceiling is a literal rather than a multiple of
    /// `GIT_TIMEOUT`: deriving BOTH bounds from the same constant would
    /// let the pair slide together and assert nothing.
    #[test]
    fn the_rails_patience_clears_the_engines_git_ceiling_and_stops_inside_a_minute() {
        let git = eave_engine::git::GIT_TIMEOUT;
        assert!(
            FILES_PATIENCE >= git + git / 2,
            "the files rail gives up in {:?}, but one of the engine's own \
             git calls may legitimately take {git:?} — the rail would put \
             `no file list` on screen over a scan that is still running",
            FILES_PATIENCE
        );
        assert!(
            FILES_PATIENCE <= Duration::from_secs(30),
            "the rail waits {:?} before it says anything, and it holds a \
             30 fps pulse lease the whole time",
            FILES_PATIENCE
        );
    }

    /// The connection facts the rail is allowed to read off instead of
    /// timing, and — the half that matters — the ones it is not.
    ///
    /// `Failed` must carry the ENGINE'S OWN WORDS through. The failure
    /// this catches is a rail that swallows "engine unreachable after 5
    /// attempts: …" and prints a house-style shrug, which is the same
    /// defect as the eternal skeleton one layer up: an error state that
    /// lost its error.
    #[test]
    fn only_a_settled_connection_answers_for_the_files_rail() {
        let failed = conn_silence(&ConnEvent::Failed {
            message: "engine unreachable after 5 attempts: no such file".into(),
        })
        .expect("a failed connection is a stated fact, not a wait");
        assert!(
            failed.contains("engine unreachable after 5 attempts: no such file"),
            "the rail dropped the engine's own words: {failed}"
        );
        assert!(
            conn_silence(&ConnEvent::Busy {
                holder_pid: Some(991)
            })
            .is_some_and(|w| w.contains("991")),
            "a contended start names the holder or it is not actionable"
        );
        assert!(
            conn_silence(&ConnEvent::Busy { holder_pid: None }).is_some(),
            "an unknown holder is still a reason no list is coming"
        );

        // The live ones. `Reconnecting` is bounded — five attempts with a
        // doubling 250 ms backoff — and ends in `Failed`, so calling a
        // 250 ms blip `no file list` would be an error state that never
        // had an error.
        for live in [
            ConnEvent::Connecting,
            ConnEvent::Connected { engine_pid: 7 },
            ConnEvent::Reconnecting { attempt: 4 },
        ] {
            assert_eq!(
                conn_silence(&live),
                None,
                "{live:?} is not a reason to stop expecting a file list"
            );
        }
    }

    /// A fence under the cap is drawn verbatim; over it, the view stops and
    /// says so — including that copy is unaffected, because the failure this
    /// notice prevents is someone believing eave truncated their data.
    #[test]
    fn a_long_fence_states_what_it_is_not_drawing() {
        let short = "let x = 1;\nlet y = 2;";
        assert_eq!(fence_view(short), short, "under the cap, verbatim");

        let long: String = (0..theme::FENCE_MAX_LINES + 40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = fence_view(&long);
        assert_eq!(
            view.lines().count(),
            theme::FENCE_MAX_LINES + 1,
            "the drawn fence is the cap plus its own one-line notice"
        );
        assert!(view.contains("40 more lines"), "the remainder is stated");
        assert!(
            view.contains(&format!("copy takes all {}", theme::FENCE_MAX_LINES + 40)),
            "a cap that does not say copy is unaffected reads as data loss"
        );
        assert!(
            view.starts_with("line 0"),
            "the cap drops the TAIL, not the head"
        );
    }

    use super::*;

    /// Slice M (§1.21). The veil's curve is an ease-OUT: it must reach
    /// the endpoints exactly (a fade that never quite finishes leaves
    /// permanently dimmed text) and be ahead of linear everywhere in
    /// between, which is the whole difference from the default it
    /// replaced.
    #[test]
    fn the_veil_curve_is_an_ease_out_that_still_lands_on_its_endpoints() {
        assert_eq!(theme::veil_ease(0.0), 0.0);
        assert_eq!(theme::veil_ease(1.0), 1.0);
        let mut prev = 0.0;
        for i in 1..=99 {
            let t = i as f32 / 100.0;
            let v = theme::veil_ease(t);
            assert!(v > prev, "must be monotonic at t={t}");
            assert!(v > t, "must lead linear at t={t} (got {v})");
            assert!(v <= 1.0);
            prev = v;
        }
    }

    /// Slice M (§1.22). The override is what makes reduced motion
    /// verifiable at all — the OS setting is not something a test run can
    /// toggle. Anything that is not an explicit yes is a no, so a stray
    /// `EAVE_REDUCE_MOTION=maybe` cannot silently disable the one
    /// animation in the app.
    #[test]
    fn the_reduced_motion_override_only_answers_when_it_was_asked() {
        assert_eq!(reduce_motion_override(None), None);
        assert_eq!(reduce_motion_override(Some("1")), Some(true));
        assert_eq!(reduce_motion_override(Some("true")), Some(true));
        assert_eq!(reduce_motion_override(Some("0")), Some(false));
        assert_eq!(reduce_motion_override(Some("")), Some(false));
        assert_eq!(reduce_motion_override(Some("maybe")), Some(false));
    }

    /// The CoreFoundation read is the only unsafe block in the app, and
    /// the failure it could have is not a wrong answer but an abort. This
    /// exercises it against the running machine's real preference domain:
    /// absent (`None`) is the normal state of a Mac whose owner has never
    /// opened that settings pane, and is not a failure.
    #[cfg(target_os = "macos")]
    #[test]
    fn reading_the_system_reduced_motion_preference_does_not_abort() {
        let seen = macos_reduce_motion();
        assert!(matches!(seen, None | Some(true) | Some(false)));
    }

    #[test]
    fn provider_identity_is_uniform_and_never_a_mark() {
        assert_eq!(
            provider_identity("claude-code"),
            ("CC".to_string(), "claude code".to_string())
        );
        assert_eq!(provider_identity("codex"), ("CO".into(), "codex".into()));
        assert_eq!(provider_identity("cursor"), ("CU".into(), "cursor".into()));
        assert_eq!(
            provider_identity("fake-agent"),
            ("FA".into(), "fake agent".into())
        );
        assert_eq!(provider_identity(""), ("?".into(), "".into()));
    }

    #[test]
    fn state_badges_only_for_attention_worthy_states() {
        for s in ["quiet", "unresponsive", "failed"] {
            assert_eq!(state_badge(s), Some(s));
        }
        for s in ["new", "running", "done", "killed", "interrupted"] {
            assert_eq!(state_badge(s), None, "{s} must not badge");
        }
    }

    #[test]
    fn count_labels_never_fake_a_number() {
        assert_eq!(
            count_labels(Some(12), Some(4)),
            Some(("+12".into(), "−4".into()))
        );
        assert_eq!(
            count_labels(Some(0), Some(57)),
            Some(("+0".into(), "−57".into()))
        );
        // Binary / oversized: unknowable, not zero.
        assert_eq!(count_labels(None, None), None);
    }

    #[test]
    fn worktree_facts_state_every_shape() {
        use eave_wire::WorktreeInfo;
        let mut w = WorktreeInfo {
            base: "/repo".into(),
            branch: "eave/abcd1234".into(),
            present: true,
            dirty_files: Some(0),
        };
        assert_eq!(worktree_line(&w), "worktree · clean");
        w.dirty_files = Some(3);
        assert_eq!(worktree_line(&w), "worktree · 3 dirty");
        // A failed scan is an honest unknown, never a fake zero.
        w.dirty_files = None;
        assert_eq!(worktree_line(&w), "worktree · dirty —");
        w.present = false;
        assert_eq!(worktree_line(&w), "worktree missing");

        assert_eq!(
            shares_line(&["0a1b2c3d-4444-5555".into(), "ffff0000-1".into()]),
            "shares files with 0a1b2c3d, ffff0000"
        );
    }

    #[test]
    fn short_cwd_is_home_relative() {
        assert_eq!(short_cwd("/Users/x/proj", Some("/Users/x")), "~/proj");
        assert_eq!(short_cwd("/opt/proj", Some("/Users/x")), "/opt/proj");
        assert_eq!(short_cwd("/opt/proj", None), "/opt/proj");
    }

    fn info(id: &str, created: u64, activity: Option<u64>) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            title: None,
            archived: false,
            provider: "claude-code".into(),
            model: None,
            cwd: "/tmp/repo".into(),
            created_ts_ms: created,
            last_activity_ms: activity,
            state: "idle".into(),
            live: false,
            epoch: 1,
            last_seq: 0,
            checkout_root: None,
            shared_with: Vec::new(),
            worktree: None,
            pending_approvals: 0,
            permission_mode: None,
            rss_bytes: None,
        }
    }

    /// Stage AB. Activity decides where a row LANDS; it never decides
    /// again. Both halves are asserted here because either alone is the
    /// wrong product: sort-always is comet's moving-target rail, and
    /// sort-never means the session you were last working in can be
    /// anywhere after a restart.
    #[test]
    fn activity_places_a_row_once_and_then_leaves_it_alone() {
        let mut rows = Vec::new();

        // Cold start: everything is an arrival, so everything sorts.
        // `b` was created first but acted most recently — activity wins,
        // which is the whole reason the field exists.
        fold_rail(
            &mut rows,
            vec![
                info("a", 100, None),
                info("b", 10, Some(900)),
                info("c", 300, None),
            ],
        );
        let ids: Vec<&str> = rows.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);

        // The same sessions again, with `a` now the most recently active
        // by a wide margin. Nothing moves.
        fold_rail(
            &mut rows,
            vec![
                info("a", 100, Some(99_999)),
                info("b", 10, Some(900)),
                info("c", 300, None),
            ],
        );
        let ids: Vec<&str> = rows.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["b", "c", "a"],
            "a state change re-sorted the rail — that is the bug that moves \
             a row out from under a click"
        );
        // ...but the FACTS did fold by replacement.
        assert_eq!(rows[2].last_activity_ms, Some(99_999));

        // A genuinely new session is an arrival, so it does place itself,
        // and a session that is gone from the snapshot loses its row.
        fold_rail(
            &mut rows,
            vec![
                info("a", 100, Some(99_999)),
                info("c", 300, None),
                info("d", 400, None),
            ],
        );
        let ids: Vec<&str> = rows.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["d", "c", "a"], "arrivals place, survivors hold");
    }

    /// Two sessions opened inside the same millisecond still need a
    /// defined order, or the rail reshuffles itself between snapshots.
    #[test]
    fn a_tie_in_activity_is_broken_by_identity() {
        let mut rows = Vec::new();
        fold_rail(
            &mut rows,
            vec![info("bbbb", 5, None), info("aaaa", 5, None)],
        );
        let ids: Vec<&str> = rows.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["aaaa", "bbbb"]);
    }

    /// Stage AB: the row's primary label. A named session says its name; an
    /// unnamed one says its provider — never an empty string, which is what
    /// would draw the blank line this slice is not allowed to produce.
    #[test]
    fn an_unnamed_row_falls_back_to_the_provider_rather_than_to_nothing() {
        let mut s = info("aaaa1111", 1, None);
        assert_eq!(row_label(&s, "claude code"), "claude code");
        s.title = Some("review the parser".into());
        assert_eq!(row_label(&s, "claude code"), "review the parser");
    }

    /// The meta line is what tells two untitled sessions in one repo apart,
    /// so the identity has to be on it — except where it would be said
    /// twice.
    #[test]
    fn the_meta_line_names_the_session_unless_the_path_already_does() {
        let mut s = info("aaaa1111", 1, None);
        s.cwd = "/Users/x/proj".into();
        assert_eq!(rail_meta_line(&s, Some("/Users/x")), "aaaa1111 · ~/proj");

        let mut other = s.clone();
        other.session_id = "bbbb2222".into();
        assert_ne!(
            rail_meta_line(&s, Some("/Users/x")),
            rail_meta_line(&other, Some("/Users/x")),
            "same repo, same provider — the meta line is the only thing \
             left to tell them apart"
        );

        // An isolated session's cwd already ends in its own sid8.
        s.cwd = "/Users/x/wt/proj/aaaa1111".into();
        assert_eq!(rail_meta_line(&s, Some("/Users/x")), "~/wt/proj/aaaa1111");
    }

    // -----------------------------------------------------------------
    // Stage AC — the archive filter.

    /// The plain rule and its two deliberate exceptions.
    #[test]
    fn the_filter_hides_an_archived_row_and_never_hides_a_blocked_one() {
        let plain = info("aaaa1111", 1, None);
        assert!(
            rail_shows(&plain, None, false),
            "an unarchived session is not affected by the filter"
        );

        let mut filed = plain.clone();
        filed.archived = true;
        assert!(!rail_shows(&filed, None, false), "the bit did nothing");
        assert!(
            rail_shows(&filed, None, true),
            "the toggle did not bring it back"
        );
        assert!(
            rail_shows(&filed, Some("aaaa1111"), false),
            "the session on screen lost its row — nothing left to \
             unarchive it with"
        );
        assert!(
            !rail_shows(&filed, Some("bbbb2222"), false),
            "some OTHER session being selected unhid it"
        );

        // eave never auto-answers a permission question (Slice K), so the
        // only way out of `awaiting_approval` is a click. A filter that
        // hid this row would park the agent for the life of the process.
        let mut asking = filed.clone();
        asking.state = "awaiting_approval".into();
        asking.pending_approvals = 1;
        assert!(rail_shows(&asking, None, false));
        // Either half alone is enough. The count and the state name are
        // maintained by two different engine-side structures, and the
        // error this predicate must not make is hiding a row that needed
        // showing — so it takes the union rather than trusting them to
        // agree on every snapshot.
        let mut count_only = filed.clone();
        count_only.pending_approvals = 1;
        assert!(rail_shows(&count_only, None, false), "the count alone");
        let mut name_only = filed.clone();
        name_only.state = "awaiting_approval".into();
        assert!(rail_shows(&name_only, None, false), "the state name alone");

        // And the states that are NOT exceptions, one at a time — these
        // are the reasons people archive, so exempting them would make the
        // bit useless exactly when it is wanted.
        for state in ["running", "quiet", "failed", "unresponsive", "done"] {
            let mut s = filed.clone();
            s.state = state.into();
            assert!(
                !rail_shows(&s, None, false),
                "an archived `{state}` session refused to stay filed away"
            );
        }
    }

    /// A row the filter let through says why it is there.
    #[test]
    fn a_visible_archived_row_states_that_it_is_archived() {
        let mut s = info("aaaa1111", 1, None);
        assert_eq!(
            archived_line(&s, false),
            None,
            "an ordinary row says nothing"
        );

        s.archived = true;
        assert_eq!(archived_line(&s, false).as_deref(), Some("archived"));
        assert_eq!(archived_line(&s, true).as_deref(), Some("archived"));

        // On screen because it is blocked on the operator, while the
        // toggle says archived rows are hidden: the row has to account
        // for its own presence or the filter reads as broken.
        s.pending_approvals = 2;
        assert_eq!(
            archived_line(&s, false).as_deref(),
            Some("archived · still asking")
        );
        // The state name alone gets the row on screen (see `rail_shows`),
        // so it has to account for itself too.
        s.pending_approvals = 0;
        s.state = "awaiting_approval".into();
        assert_eq!(
            archived_line(&s, false).as_deref(),
            Some("archived · still asking")
        );
        // With the shelf open every archived row is on screen, so there is
        // nothing to account for.
        assert_eq!(archived_line(&s, true).as_deref(), Some("archived"));
    }

    /// The toggle states a fact and an action, and its count is of
    /// archived SESSIONS rather than of hidden rows — those two numbers
    /// differ whenever an exception is on screen.
    #[test]
    fn the_archived_toggle_reads_the_same_way_in_both_directions() {
        assert_eq!(archived_toggle_label(3, false), "3 archived · show");
        assert_eq!(archived_toggle_label(3, true), "3 archived · hide");
        assert_eq!(archived_toggle_label(1, false), "1 archived · show");
    }

    /// An empty rail must never lose the reason it is empty. "no sessions"
    /// over a rail whose sessions are all filed away is exactly the
    /// "error state that lost its error" this codebase keeps finding.
    #[test]
    fn an_empty_rail_says_why_it_is_empty() {
        assert_eq!(rail_empty_note(0, 0), "no sessions");
        assert_eq!(
            rail_empty_note(4, 4),
            "every session is archived (4)",
            "a filtered-to-nothing rail claimed there were no sessions"
        );
        // Not reachable through today's filter, and stated anyway.
        assert_eq!(rail_empty_note(4, 1), "nothing to show — 1 of 4 archived");
    }
}
