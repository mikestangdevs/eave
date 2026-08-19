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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    actions, div, list, point, prelude::*, px, size, AnyElement, App, Bounds, ClickEvent, Context,
    Corners, Element, Entity, FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, KeyBinding, KeyDownEvent, LayoutId, ListAlignment, ListOffset, ListState,
    MouseDownEvent, Pixels, Point, ScrollHandle, ScrollWheelEvent, SharedString,
    StrikethroughStyle, StyledText,
    TextRun, TitlebarOptions, UnderlineStyle, Window, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowHandle, WindowOptions,
};
use gpui::Animation;
use gpui::AnimationExt as _;
use gpui_platform::application;
use tokio::sync::mpsc as tokio_mpsc;

use eave_engine::client::{Client, ClientError, EngineSpawn};
use eave_wire::model::{ApprovalStatus, Delivery, Row, ToolStatus, Transcript};
use eave_wire::{
    AttentionSnapshot, ChangedFile, Cursor, DiffHunk, DiffKind, DiffLine, DiffOp, FileDiff,
    FileStatus, FilesSnapshot, GitState, SessionInfo, SubscriptionItem, WaitReason,
};

use crate::display::{DisplayList, RowId};
use crate::edge_fade::edge_faded;
use crate::loaders;
use crate::markdown::{inline_runs, Block, InlineRun};
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
    Connected { engine_pid: i32 },
    Reconnecting { attempt: u32 },
    /// Contended start: another engine holds the data dir (spec 02
    /// §Contended start). `None` = "holder unknown" — still actionable.
    Busy { holder_pid: Option<i32> },
    Failed { message: String },
}

/// Feed frames are tagged with the session they belong to: a selection
/// switch drops the old subscription, but frames from it may already sit
/// in the queue — the pump discards anything not matching the transcript's
/// current session (no event bleed between sessions).
#[derive(Debug)]
enum FeedEvent {
    Session { id: String },
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

/// One worktree-action outcome (Slice I2), session-tagged. The rail
/// states it on the session's row; `Settled` clears any standing note
/// (an isolate landed, a removal completed — the watch snapshot carries
/// the new facts).
#[derive(Debug, Clone)]
enum WtNote {
    /// The engine refused a non-forced delete, restating fresh counts —
    /// the delete affordance is now armed: the next click means it.
    ConfirmDelete {
        dirty_files: Option<u64>,
        unmerged_commits: Option<u64>,
    },
    /// A stated refusal/error from isolate or remove (engine's words).
    Error(String),
}

type WtEvent = (String, Option<WtNote>);

#[derive(Debug)]
enum Command {
    Send(String),
    Kill,
    /// Projection lost sync (seq gap): drop and fully resubscribe.
    Resubscribe,
    /// Rail row click: subscribe this session instead (cursorless — the
    /// UI holds no projection for it; full replay, never merge).
    Select(String),
    /// Rail "+ new": open a fresh session and select it.
    NewSession,
    /// Files-rail row click (B2): fetch one file's diff. Unary and
    /// re-issued on every fresh snapshot while the row stays expanded —
    /// the files channel is the invalidation signal.
    Diff(String),
    /// Rail "isolate" click (Slice I2): opt this never-started session
    /// into its own worktree. The watch snapshot carries the result;
    /// refusals surface as a stated note on the row.
    Isolate(String),
    /// Rail "delete worktree" click (Slice I2). `force: false` first —
    /// the engine refuses with fresh counts if anything would be lost,
    /// which arms the affordance; the second click sends `force: true`
    /// (decision 2's two-step).
    RemoveWorktree { id: String, force: bool },
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
    wt: Mutex<VecDeque<WtEvent>>,
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
    fn push_wt(&self, ev: WtEvent) {
        self.queues.wt.lock().unwrap().push_back(ev);
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
    let no_spawn = std::env::var("EAVE_NO_SPAWN").map(|v| v == "1").unwrap_or(false);
    let mut attempt: u32 = 0;
    let mut cursor: Option<Cursor> = None;
    let mut selected: Option<String> = None;
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
            // the most recent session, or a fresh one on an empty engine.
            let sessions = match client.list().await {
                Ok(s) => s,
                Err(_) => continue 'conn,
            };
            let session_id = match selected
                .clone()
                .filter(|id| sessions.iter().any(|s| &s.session_id == id))
            {
                Some(id) => id,
                None => match sessions.iter().max_by_key(|s| s.created_ts_ms) {
                    Some(s) => s.session_id.clone(),
                    None => match client.open(None, None).await {
                        Ok(info) => info.session_id,
                        Err(_) => continue 'conn,
                    },
                },
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
            let (files_id, mut files_rx) = match client.files_with_id(&session_id).await {
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
                            tokio::spawn(async move {
                                let res = client
                                    .diff(&sid, &path)
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
                                    Err(e) => Some(WtNote::Error(e.to_string())),
                                };
                                plumbing.push_wt((sid, note));
                            });
                        }
                        Some(Command::RemoveWorktree { id, force }) => {
                            let client = Arc::clone(&client);
                            let plumbing = plumbing.clone();
                            tokio::spawn(async move {
                                let note = match client.remove_worktree(&id, force).await {
                                    Ok(ok) if ok.removed => None,
                                    Ok(ok) => Some(WtNote::ConfirmDelete {
                                        dirty_files: ok.dirty_files,
                                        unmerged_commits: ok.unmerged_commits,
                                    }),
                                    Err(e) => Some(WtNote::Error(e.to_string())),
                                };
                                plumbing.push_wt((id, note));
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
                        Some(Command::NewSession) => match client.open(None, None).await {
                            Ok(info) => {
                                selected = Some(info.session_id);
                                cursor = None;
                                break StreamEnd::Switch;
                            }
                            Err(_) => break StreamEnd::Conn,
                        },
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
    let sid = format!("replay:{}", path.file_name().unwrap_or_default().to_string_lossy());
    plumbing.push_feed(FeedEvent::Session { id: sid.clone() });
    // A synthetic rail snapshot so harness runs (replay assert, captures)
    // exercise the real constrained-width layout with the rail present.
    plumbing.push_rail(vec![SessionInfo {
        session_id: sid.clone(),
        provider: "replay".into(),
        cwd: path.display().to_string(),
        created_ts_ms: 0,
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
    }
}

/// A [`git::DIFF_MAX_LINES`]-scale text diff for the perf harness: the
/// long-diff render path is new in B2 and must be measured at its cap,
/// not at a friendly size. Runs alternate ctx/del/add so the wash
/// grouping is exercised, not short-circuited.
fn synthetic_long_diff() -> FileDiff {
    let hunks = (0..40)
        .map(|h| DiffHunk {
            header: format!("@@ -{},50 +{},50 @@ fn section_{h}()", h * 60 + 1, h * 60 + 1),
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
    sessions: Vec<SessionInfo>,
    collapsed: bool,
    attention: Option<(String, AttentionSnapshot)>,
    /// Standing worktree-action notes per session (Slice I2): a stated
    /// refusal, or an armed delete confirmation restating the engine's
    /// fresh counts. Cleared by a settled action; folded by replacement.
    wt_notes: std::collections::HashMap<String, WtNote>,
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
    /// The one expanded diff (B2), session-tagged like everything else
    /// in this rail: a switch clears it, a snapshot that no longer lists
    /// its file closes it, and a fresh snapshot re-requests it (the
    /// files channel is the invalidation signal — the diff itself is
    /// never cached engine-side).
    diff: Option<DiffSlot>,
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
            .text_color(theme::text_ghost())
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
        DiffFetch::Loading => {
            children.push(
                loaders::skeleton(theme::SKELETON_ROWS, phase)
                    .debug_selector(|| "diff-skeleton".into())
                    .into_any_element(),
            )
        }
        DiffFetch::Failed(message) => children.push(fact(format!("diff unavailable: {message}"))),
        DiffFetch::Ready(diff) => match &diff.kind {
            DiffKind::Binary => children.push(fact("binary file — no text diff".into())),
            DiffKind::Symlink { target } => children.push(fact(format!("symlink → {target}"))),
            DiffKind::Submodule { detail } => children.push(fact(format!("submodule {detail}"))),
            DiffKind::Error { message } => children.push(fact(format!("git failed: {message}"))),
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
                            .text_color(theme::text_ghost())
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
                                DiffOp::Ctx => styled.text_color(theme::text_ghost()),
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
        _ => theme::text_ghost(),
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
fn resource_line(sessions: &[eave_wire::SessionInfo], physical: Option<u64>) -> Option<(String, bool)> {
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

/// Home-relative cwd for the rail's meta line.
fn short_cwd(cwd: &str, home: Option<&str>) -> String {
    match home {
        Some(h) if !h.is_empty() && cwd.starts_with(h) => format!("~{}", &cwd[h.len()..]),
        _ => cwd.to_string(),
    }
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
        std::cell::RefCell::new(Vec::new());
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
    Pinned { msg: u32 },
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
        self.transcript.rows.iter().enumerate().rev().find_map(|(ix, r)| match r {
            Row::Assistant { text, live: true } => Some((ix as u32, text.len())),
            _ => None,
        })
    }

    /// Fold one record batch and mirror the change into the `ListState`
    /// with minimal ops (splice for structure, remeasure for in-place
    /// growth/settle — spec 03 §Rendering).
    fn apply_records(&mut self, epoch: u64, records: &[eave_core::journal::Record]) -> Result<(), ()> {
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
                    let duration =
                        Duration::from_millis(self.ema_ms.clamp(120.0, 400.0) as u64);
                    let generation =
                        self.veil.as_ref().map(|v| v.generation + 1).unwrap_or(1);
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
    text: String,
    /// When the caret last went solid (inventory §1.20). Every keystroke
    /// resets it, which is what keeps a typing burst from blinking: the
    /// cadence is solid through its first half-period, so the caret only
    /// starts flashing once the typist has stopped. `None` until the first
    /// keystroke — a caret nobody has typed at has nothing to blink about.
    caret_anchor: Option<Instant>,
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
            let color = if run.style.code { self.code_color } else { self.color };
            let underline = run.style.link.then(|| UnderlineStyle {
                thickness: px(theme::HAIRLINE),
                color: None,
                wavy: false,
            });
            let len = run.text.len();
            let chars = run.text.chars().count();
            let make = |len: usize, alpha: f32| TextRun {
                len,
                font: font.clone(),
                color: color.opacity(alpha * color.a),
                background_color: None,
                underline: underline.clone(),
                strikethrough: None::<StrikethroughStyle>,
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
        StyledText::new(self.text).with_runs(runs)
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
        // §1.20: ANY key resets the blink anchor, not just the ones that
        // change the text. Backspacing to empty, a rejected ⌘-chord, an
        // arrow key — they are all "the typist is still here", and a caret
        // that blinked through them would be flashing at someone who is
        // actively using the composer.
        let struck = cx.background_executor().now();
        self.composer.update(cx, |c, _| c.caret_anchor = Some(struck));
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
            } else if t.scroll == ScrollMode::Free
                && t.list.is_scrolled_to_end().unwrap_or(false)
            {
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
                // Rail collapse toggle: a self-drawn sidebar pictogram
                // (bordered rect + one vertical hairline), monochrome.
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
                    .child(
                        div()
                            .w(px(theme::RAIL_TOGGLE_W))
                            .h(px(theme::RAIL_TOGGLE_H))
                            .rounded(px(theme::RADIUS_GLYPH))
                            .border_1()
                            .border_color(theme::text_tertiary())
                            .child(
                                div()
                                    .w(px(theme::HAIRLINE))
                                    .h_full()
                                    .ml(px(theme::RAIL_TOGGLE_W / 3.0))
                                    .bg(theme::text_tertiary()),
                            ),
                    ),
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
                    .text_color(theme::text_ghost())
                    .child(s)
            }))
            .child(div().flex_1())
            .child(
                // Files-rail collapse toggle: the sidebar pictogram
                // mirrored (hairline on the right third).
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
                    .child(
                        div()
                            .w(px(theme::RAIL_TOGGLE_W))
                            .h(px(theme::RAIL_TOGGLE_H))
                            .rounded(px(theme::RADIUS_GLYPH))
                            .border_1()
                            .border_color(theme::text_tertiary())
                            .child(
                                div()
                                    .w(px(theme::HAIRLINE))
                                    .h_full()
                                    .ml(px(theme::RAIL_TOGGLE_W * 2.0 / 3.0))
                                    .bg(theme::text_tertiary()),
                            ),
                    ),
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
        let mut sessions = self.sessions.read(cx).sessions.clone();
        sessions.sort_by(|a, b| b.created_ts_ms.cmp(&a.created_ts_ms));
        let selected = self.transcript.read(cx).session_id.clone();
        let wt_notes = self.sessions.read(cx).wt_notes.clone();
        let home = std::env::var("HOME").ok();
        let resource = resource_line(&sessions, physical_memory_cached());
        // The meta line aligns under the provider label: dot + gap +
        // chip + gap.
        let meta_indent = theme::STATE_DOT + theme::SPACE_2 + theme::PROVIDER_CHIP + theme::SPACE_2;
        let rows: Vec<AnyElement> = sessions
            .into_iter()
            .map(|info| {
                let is_sel = selected.as_deref() == Some(info.session_id.as_str());
                let (glyph, label) = provider_identity(&info.provider);
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
                let wt_note = wt_notes.get(&info.session_id).cloned();
                let armed = matches!(wt_note, Some(WtNote::ConfirmDelete { .. }));
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
                    WtNote::ConfirmDelete {
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
                    WtNote::Error(msg) => (msg.clone(), theme::danger()),
                });
                let sid_iso = info.session_id.clone();
                let sid_del = info.session_id.clone();
                div()
                    .id(SharedString::from(format!("rail-{}", info.session_id)))
                    .debug_selector(|| format!("rail-{}", info.session_id))
                    .px(px(theme::SPACE_2))
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
                    .on_click(cx.listener(move |this, _, _, _| {
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
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(px(theme::DETAIL_SIZE))
                                    .line_height(px(theme::DETAIL_LINE))
                                    .font_weight(theme::WEIGHT_LABEL)
                                    .text_color(if is_sel {
                                        theme::text()
                                    } else {
                                        theme::text_secondary()
                                    })
                                    .child(SharedString::from(label)),
                            )
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
                            .text_color(theme::text_ghost())
                            .child(SharedString::from(short_cwd(&info.cwd, home.as_deref()))),
                    )
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
                                theme::text_ghost()
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
                            .text_color(theme::text_ghost())
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
                                theme::text_ghost()
                            } else {
                                theme::warning()
                            })
                            .child(SharedString::from(line))
                    }))
                    // The affordance row (isolate / open / delete) sits on
                    // its own line beneath the facts it acts on.
                    .when(can_isolate || show_wt_buttons, |d| {
                        d.child(
                            div()
                                .pl(px(meta_indent))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(theme::SPACE_2))
                                .when(can_isolate, |d| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "iso-{}",
                                                info.session_id
                                            )))
                                            .debug_selector(|| format!("iso-{}", info.session_id))
                                            .flex_none()
                                            .px(px(theme::SPACE_2))
                                            .rounded(px(theme::RADIUS_BUTTON))
                                            .border_1()
                                            .border_color(theme::border())
                                            .text_size(px(theme::SMALL_SIZE))
                                            .line_height(px(theme::SMALL_LINE))
                                            .text_color(theme::text_secondary())
                                            .hover_fade(
                                                format!("iso-{}", info.session_id),
                                                theme::wash_rest(),
                                                theme::overlay(),
                                            )
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                // An affordance acts on the
                                                // row; it does not select it
                                                // (amendment 2 — selection is
                                                // the row's own click).
                                                cx.stop_propagation();
                                                let _ = this
                                                    .commands
                                                    .send(Command::Isolate(sid_iso.clone()));
                                            }))
                                            .child("isolate"),
                                    )
                                })
                                .when(show_wt_buttons && wt_present, |d| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "wto-{}",
                                                info.session_id
                                            )))
                                            .debug_selector(|| format!("wto-{}", info.session_id))
                                            .flex_none()
                                            .px(px(theme::SPACE_2))
                                            .rounded(px(theme::RADIUS_BUTTON))
                                            .border_1()
                                            .border_color(theme::border())
                                            .text_size(px(theme::SMALL_SIZE))
                                            .line_height(px(theme::SMALL_LINE))
                                            .text_color(theme::text_secondary())
                                            .hover_fade(
                                                format!("wto-{}", info.session_id),
                                                theme::wash_rest(),
                                                theme::overlay(),
                                            )
                                            .cursor_pointer()
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                open_in_system(std::path::Path::new(&wt_path));
                                            })
                                            .child("open"),
                                    )
                                })
                                .when(show_wt_buttons, |d| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "wtd-{}",
                                                info.session_id
                                            )))
                                            .debug_selector(|| format!("wtd-{}", info.session_id))
                                            .flex_none()
                                            .px(px(theme::SPACE_2))
                                            .rounded(px(theme::RADIUS_BUTTON))
                                            .border_1()
                                            .border_color(if armed {
                                                theme::danger()
                                            } else {
                                                theme::border()
                                            })
                                            .text_size(px(theme::SMALL_SIZE))
                                            .line_height(px(theme::SMALL_LINE))
                                            .text_color(if armed {
                                                theme::danger()
                                            } else {
                                                theme::text_secondary()
                                            })
                                            .hover_fade(
                                                format!("wtd-{}", info.session_id),
                                                theme::wash_rest(),
                                                theme::overlay(),
                                            )
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                let _ = this.commands.send(
                                                    Command::RemoveWorktree {
                                                        id: sid_del.clone(),
                                                        force: armed,
                                                    },
                                                );
                                            }))
                                            .child("delete"),
                                    )
                                }),
                        )
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
                .debug_selector(|| "rail-empty".into())
                .px(px(theme::SPACE_2))
                .py(px(theme::SPACE_1))
                .text_size(px(theme::SMALL_SIZE))
                .line_height(px(theme::SMALL_LINE))
                .text_color(theme::text_ghost())
                .child("no sessions")
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
                    .px(px(theme::SPACE_3))
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
                            .on_click(cx.listener(|this, _, _, _| {
                                let _ = this.commands.send(Command::NewSession);
                            }))
                            .child("+ new"),
                    ),
            )
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
                        .px(px(theme::SPACE_2))
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
                    .debug_selector(|| "rail-resource".into())
                    .flex_none()
                    .px(px(theme::SPACE_3))
                    .pb(px(theme::SPACE_3))
                    .pt(px(theme::SPACE_1))
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .font_weight(if over { theme::WEIGHT_LABEL } else { theme::WEIGHT_BODY })
                    .text_color(if over { theme::warning() } else { theme::text_ghost() })
                    .child(SharedString::from(line))
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
                .text_color(theme::text_ghost())
                .child(SharedString::from(text.to_string()))
                .into_any_element()
        };
        let (count_label, body): (Option<String>, AnyElement) = match &current {
            // Slice N3: the snapshot has not arrived yet. The single
            // character `…` was the same picture whether it was seconds
            // away or never coming; skeleton rows at least say a LIST is
            // what is coming, in the rhythm it will arrive in.
            None => (
                None,
                div()
                    .px(px(theme::SPACE_2))
                    .child(
                        loaders::skeleton(theme::SKELETON_ROWS, phase)
                            .debug_selector(|| "files-skeleton".into()),
                    )
                    .into_any_element(),
            ),
            Some(None) => (None, fact("file status unavailable — engine predates it")),
            Some(Some(snap)) => match &snap.state {
                GitState::NotARepo => (None, fact("not a git repository")),
                GitState::Error { message } => (
                    None,
                    div()
                        .px(px(theme::SPACE_2))
                        .text_size(px(theme::DETAIL_SIZE))
                        .line_height(px(theme::DETAIL_LINE))
                        .text_color(theme::text_tertiary())
                        .child(SharedString::from(format!("git failed: {message}")))
                        .into_any_element(),
                ),
                GitState::Ok if snap.files.is_empty() => (Some("0".into()), fact("no changes")),
                GitState::Ok => {
                    let mut rows: Vec<AnyElement> = Vec::new();
                    for f in &snap.files {
                        let expanded =
                            matches!(&diff_slot, Some((p, _)) if p.as_str() == f.path);
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
                        format!("{} of {}", snap.files.len(), snap.files.len() as u64 + snap.omitted)
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
        let mode: Option<&'static str> = match &current {
            Some(Some(snap)) if matches!(snap.state, GitState::Ok) => Some(if snap.watching {
                "watching file events"
            } else {
                "refreshes at turn end"
            }),
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
                    .px(px(theme::SPACE_3))
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
                            .text_color(theme::text_tertiary())
                            .child("changes"),
                    )
                    .children(count_label.map(|n| {
                        div()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .font_family(theme::MONO)
                            .text_color(theme::text_ghost())
                            .child(SharedString::from(n.to_string()))
                    })),
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
                        .px(px(theme::SPACE_2))
                        .pb(px(theme::SPACE_2))
                        .child(body),
                )
                .fade_overflow_y(&self.files_scroll),
            )
            .children(mode.map(|text| {
                div()
                    .debug_selector(|| "files-mode".into())
                    .flex_none()
                    .px(px(theme::SPACE_3))
                    .pb(px(theme::SPACE_2))
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .text_color(theme::text_ghost())
                    .child(text)
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
            .px(px(theme::SPACE_2))
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
                if expanded { theme::overlay() } else { theme::wash_rest() },
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
                div()
                    .w(px(theme::FILE_STATUS_W))
                    .flex_none()
                    .font_family(theme::MONO)
                    .text_size(px(theme::MICRO_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .text_color(theme::text_tertiary())
                    .child(f.status.glyph()),
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
                                .text_color(theme::text_ghost())
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
                    .child(div().text_color(theme::success()).child(SharedString::from(adds)))
                    .child(div().text_color(theme::danger()).child(SharedString::from(dels)))
                    .into_any_element(),
                // Unknowable (binary / oversized): one honest dash.
                None => div()
                    .flex_none()
                    .font_family(theme::MONO)
                    .text_size(px(theme::SMALL_SIZE))
                    .line_height(px(theme::SMALL_LINE))
                    .text_color(theme::text_ghost())
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
            ConnEvent::Reconnecting { attempt } => {
                (theme::warning(), format!("reconnecting (attempt {attempt})…"))
            }
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
            .child(
            Self::column(
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
                            .child(
                                div()
                                    .size(px(theme::STATE_DOT))
                                    .rounded_full()
                                    .bg(dot),
                            )
                            .child(label)
                            .children(self.friction.as_ref().map(|(_, note)| {
                                div()
                                    .text_color(theme::text_ghost())
                                    .child(SharedString::from(note.clone()))
                            })),
                    )
                    .children(session.map(|s| {
                        div().text_color(theme::text_ghost()).child(s)
                    })),
            ),
        )
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
        let selected = self.transcript.read(cx).session_id.clone();
        let files = self.files.read(cx);
        // The files rail: a selected session whose snapshot has not
        // arrived (the skeleton rows).
        if !files.collapsed && selected.is_some() {
            let arrived = matches!(&files.snapshot, Some((sid, _)) if Some(sid.as_str()) == selected.as_deref());
            if !arrived {
                return true;
            }
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
                    format!("unresponsive for {}", fmt_elapsed(now.saturating_sub(since_ms))),
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
            Row::Tool { id: _, name, status } => {
                let (label, dot) = match status {
                    ToolStatus::Running => ("running…", theme::live()),
                    ToolStatus::Finished => ("done", theme::text_ghost()),
                    ToolStatus::Interrupted => ("interrupted", theme::danger()),
                };
                div()
                    .pt(px(theme::SPACE_2))
                    .pb(px(theme::SPACE_1))
                    .pl(px(theme::TOOL_INDENT))
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
                    .child(
                        div()
                            .text_size(px(theme::SMALL_SIZE))
                            .line_height(px(theme::SMALL_LINE))
                            .text_color(theme::text_tertiary())
                            .child(label),
                    )
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
                state, request_id, tool, description, path, input_json, status,
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
            Row::Assistant { text: msg_text, live } => {
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
                                format!("{tool}")
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
            card = card.child(
                mono(p.clone()).debug_selector({
                    let rid = rid.clone();
                    move || format!("approval-path-{rid}")
                }),
            );
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
                        .child(Self::decision_button(state, &rid, true, "approve", theme::live()))
                        .child(Self::decision_button(state, &rid, false, "deny", theme::danger())),
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

    fn render_block(
        state: &TranscriptState,
        id: RowId,
        block: &Block,
        msg_text: &str,
        live: bool,
    ) -> AnyElement {
        let is_tail = state
            .display
            .block(RowId { msg: id.msg, block: id.block + 1 })
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
                            .text_color(theme::text_ghost())
                            .child(SharedString::from(label.to_string())),
                    )
                    .child(
                        div()
                            .px(px(theme::SPACE_3))
                            .py(px(theme::SPACE_2))
                            .font_family(theme::MONO)
                            .text_size(px(theme::CODE_SIZE))
                            .line_height(px(theme::CODE_LINE))
                            .text_color(theme::code_block_text())
                            .child(SharedString::from(code.trim_end().to_string())),
                    )
                    .into_any_element()
            }
            Block::Heading { level, text } => {
                let (size, line, weight) = theme::heading(*level);
                div()
                    .pt_1()
                    .text_size(size)
                    .line_height(line)
                    .text_color(theme::text())
                    .font_weight(weight)
                    .child(SharedString::from(text.clone()))
                    .into_any_element()
            }
            Block::Rule => div()
                .my_1()
                .h(px(theme::HAIRLINE))
                .bg(theme::border())
                .into_any_element(),
            Block::List { text } | Block::Table { text } | Block::Paragraph { text } => {
                let runs = inline_runs(text);
                let flat: String = runs.iter().map(|r| r.text.as_str()).collect();
                let total_chars = flat.chars().count();
                // Paint-only veil on the growing tail block of the live
                // message (spec 03 §Rendering).
                let veil = state.veil.as_ref().filter(|v| {
                    live && is_tail && v.msg == id.msg && v.from_byte < msg_text.len()
                });
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
                            ("veil", ((id.msg as u64) << 32) | (v.generation & 0xffff_ffff)),
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
                div().child(el).into_any_element()
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
                    let mut t: Vec<Duration> =
                        perf.frame_times.iter().skip(10).copied().collect();
                    t.sort();
                    let p = |q: f64| t[((t.len() - 1) as f64 * q) as usize];
                    let missed = t
                        .iter()
                        .filter(|d| **d > Duration::from_micros(25_000))
                        .count();
                    let mut c: Vec<Duration> =
                        perf.cpu_times.iter().skip(10).copied().collect();
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
                    println!("PERF cpu_max_at_frame={max_at} (~row {})", max_at * PERF_ROWS_PER_FRAME);
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
            Some("no session selected")
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
                                        .text_color(theme::text_ghost())
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
                                    theme::text_ghost()
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
                                .text_size(px(theme::SEND_GLYPH_SIZE))
                                .font_weight(theme::WEIGHT_STRONG)
                                .text_color(if has_draft {
                                    theme::on_inverse()
                                } else {
                                    theme::text_ghost()
                                })
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
                                    if has_draft { theme::inverse() } else { theme::overlay_strong() },
                                    if has_draft { theme::inverse_hover() } else { theme::overlay_strongest() },
                                )
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send_current(cx);
                                }))
                                .child("↑"),
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
            .on_action(cx.listener(|this, _: &ApproveOldest, _, cx| {
                this.decide_oldest(true, cx)
            }))
            .on_action(cx.listener(|this, _: &DenyOldest, _, cx| {
                this.decide_oldest(false, cx)
            }))
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
                                        let t = motion::entrance_t(
                                            "in:empty-note",
                                            motion::FADE_IN,
                                        );
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
                                            .text_color(theme::text_ghost())
                                            .child(
                                                div()
                                                    .opacity(t)
                                                    .relative()
                                                    .top(px(motion::ENTRANCE_RISE * (1.0 - t)))
                                                    .child(note),
                                            )
                                    }))
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
                                                        .child("↓ bottom"),
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
                                            .child(composer),
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
            .children(demo_layer);

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
    let perf_10k = std::env::var("EAVE_PERF").map(|v| v == "10k").unwrap_or(false);

    application().run(move |cx: &mut App| {
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

        bootstrap(cx, Feed { queues, wake_rx, cmd_tx });
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
    if let Some(forced) = reduce_motion_override(std::env::var("EAVE_REDUCE_MOTION").ok().as_deref())
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
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                UTF8,
                0,
            )
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
        let out = (CFGetTypeID(value) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value) != 0);
        CFRelease(value);
        out
    }
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
    let perf_10k = std::env::var("EAVE_PERF").map(|v| v == "10k").unwrap_or(false);

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        // Slice K: answering is a first-class act, so it gets real
        // bindings rather than living only inside the key handler.
        KeyBinding::new("cmd-shift-a", ApproveOldest, None),
        KeyBinding::new("cmd-shift-d", DenyOldest, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());

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
        attention: None,
        wt_notes: std::collections::HashMap::new(),
    });
    let files = cx.new(|_| FilesState {
        // Perf runs have no feed: seed a synthetic snapshot so the
        // measured layout includes a populated files rail — and (B2)
        // a cap-size expanded diff, so the long-diff render path is
        // measured at its worst, not at a friendly size.
        snapshot: perf_10k
            .then(|| ("perf:10k".to_string(), Some(synthetic_files_snapshot()))),
        collapsed: false,
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
            cx.background_executor()
                .timer(Duration::from_secs(1))
                .await;
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
                let conn_events: Vec<ConnEvent> =
                    queues.conn.lock().unwrap().drain(..).collect();
                if let Some(last) = conn_events.into_iter().last() {
                    let _ = conn.update(cx, |state, cx| {
                        state.status = last;
                        cx.notify();
                    });
                }
                // Attention is current-state, not a log: the last
                // frame of the burst wins.
                let attn_events: Vec<AttnEvent> =
                    queues.attn.lock().unwrap().drain(..).collect();
                let rail_events: Vec<RailEvent> =
                    queues.rail.lock().unwrap().drain(..).collect();
                // Files snapshots fold by replacement — last of the
                // burst wins; the render filters by session tag.
                let files_events: Vec<FilesEvent> =
                    queues.files.lock().unwrap().drain(..).collect();
                let diff_events: Vec<DiffEvent> =
                    queues.diffs.lock().unwrap().drain(..).collect();
                if !files_events.is_empty() || !diff_events.is_empty() {
                    let cmd_tx = cmd_tx.clone();
                    let _ = files.update(cx, |state, cx| {
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
                                        let _ =
                                            cmd_tx.send(Command::Diff(slot.path.clone()));
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
                let wt_events: Vec<WtEvent> =
                    queues.wt.lock().unwrap().drain(..).collect();
                if last_attn.is_some() || last_rail.is_some() || !wt_events.is_empty() {
                    let _ = sessions.update(cx, |state, cx| {
                        if let Some(tagged) = last_attn {
                            state.attention = Some(tagged);
                        }
                        // Watch snapshots fold by replacement — the
                        // engine's set is the truth, never merged.
                        if let Some(snapshot) = last_rail {
                            state.sessions = snapshot;
                        }
                        // Worktree notes fold by replacement per
                        // session; a settled action clears its note.
                        for (sid, note) in wt_events {
                            match note {
                                Some(n) => {
                                    state.wt_notes.insert(sid, n);
                                }
                                None => {
                                    state.wt_notes.remove(&sid);
                                }
                            }
                        }
                        cx.notify();
                    });
                }
                let feed_events: Vec<FeedEvent> =
                    queues.feed.lock().unwrap().drain(..).collect();
                if !feed_events.is_empty() {
                    let cmd_tx = cmd_tx.clone();
                    let mut switched = false;
                    let _ = transcript.update(cx, |state, cx| {
                        for ev in feed_events {
                            match ev {
                                FeedEvent::Session { id } => {
                                    if state.session_id.as_deref() != Some(id.as_str()) {
                                        switched = true;
                                    }
                                    state.session_id = Some(id)
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
                        let _ = files.update(cx, |state, cx| {
                            if state.diff.take().is_some() {
                                cx.notify();
                            }
                        });
                    }
                }
            }
        })
        .detach();
    }

    let bounds = Bounds::centered(None, size(px(theme::WINDOW_W), px(theme::WINDOW_H)), cx);
    let handle = cx.open_window(
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
                    caret_anchor: None,
                });
                // Slice K: the approval card's buttons are built by a
                // static row fn with no `Context`, so give the transcript
                // state the two handles they need — the shell's own
                // command channel and the composer they read the deny
                // reason from.
                transcript.update(cx, |t, _| {
                    t.commands = Some(cmd_tx.clone());
                    t.composer = Some(composer.clone());
                });
                cx.observe(&conn, |_, _, cx| cx.notify()).detach();
                cx.observe(&sessions, |_, _, cx| cx.notify()).detach();
                cx.observe(&files, |_, _, cx| cx.notify()).detach();
                cx.observe(&transcript, |_, _, cx| cx.notify()).detach();
                cx.observe(&composer, |_, _, cx| cx.notify()).detach();
                Shell {
                    focus: cx.focus_handle(),
                    demo: DemoOverlay::default(),
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
                shell.update(cx, |s, cx| {
                    s.open_demo_menu(point(px(240.), px(180.)), cx)
                });
            }
            shell
        },
    )
    .expect("open window");
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
}
