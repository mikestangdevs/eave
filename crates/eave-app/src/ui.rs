//! The Stage C GPUI shell — deliberately ugly (build directive): naive
//! top-to-bottom text, no virtualization, no mending, no veil, no scroll
//! work. Stage D is render quality. What this stage proves is the seam:
//! the UI holds no authoritative state, only the shared [`Transcript`]
//! projection fed by resubscribe + replay (spec 03 §Restart survival).
//!
//! Architecture per spec 03 §Wake & notify, scaled to one session: a
//! client task owns the socket, decodes frames onto **per-concern queues**
//! (connection / feed), and pokes a single **bounded(1) wake channel**; a
//! foreground pump drains every non-empty queue in one pass and notifies
//! only the entities whose queues were non-empty — ≤1 notify per entity
//! per burst.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    actions, div, prelude::*, px, rgb, size, App, Bounds, Context, Entity, FocusHandle,
    KeyBinding, KeyDownEvent, Window, WindowBounds, WindowOptions,
};
use gpui_platform::application;
use tokio::sync::mpsc as tokio_mpsc;

use eave_engine::client::{Client, ClientError, EngineSpawn};
use eave_wire::model::{Delivery, Row, ToolStatus, Transcript};
use eave_wire::{Cursor, SubscriptionItem};

actions!(eave, [Quit]);

// ---------------------------------------------------------------------------
// Client task ⇄ UI plumbing

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

#[derive(Debug)]
enum FeedEvent {
    Session { id: String },
    Reset,
    Records {
        epoch: u64,
        records: Vec<eave_core::journal::Record>,
    },
}

#[derive(Debug)]
enum Command {
    Send(String),
    Kill,
    /// Projection lost sync (seq gap): drop and fully resubscribe.
    Resubscribe,
}

#[derive(Default)]
struct Queues {
    conn: Mutex<VecDeque<ConnEvent>>,
    feed: Mutex<VecDeque<FeedEvent>>,
}

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
}

/// The client task: connect (spawning the engine if absent), pick the v0
/// session, subscribe, forward frames; on disconnect, reconnect with
/// doubling backoff and a 5-attempt breaker (spec 01 §Topology) and
/// resubscribe from the held cursor — replay, never merge.
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
    let mut attempt: u32 = 0;
    let mut cursor: Option<Cursor> = None;
    loop {
        plumbing.push_conn(if attempt == 0 {
            ConnEvent::Connecting
        } else {
            ConnEvent::Reconnecting { attempt }
        });
        let client = match Client::connect_or_spawn(&data_dir, &spawn).await {
            Ok(c) => c,
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

        // v0 slice: exactly one session — adopt the first on disk or open
        // a fresh one (spec 04 §In the slice).
        let session_id = match pick_session(&client).await {
            Ok(id) => id,
            Err(_) => continue,
        };
        plumbing.push_conn(ConnEvent::Connected {
            engine_pid: client.engine.pid,
        });
        plumbing.push_feed(FeedEvent::Session {
            id: session_id.clone(),
        });

        let mut rx = match client.subscribe(&session_id, cursor).await {
            Ok(rx) => rx,
            Err(_) => continue,
        };
        loop {
            tokio::select! {
                item = rx.recv() => match item {
                    Some(SubscriptionItem::Reset { .. }) => {
                        cursor = None;
                        plumbing.push_feed(FeedEvent::Reset);
                    }
                    Some(SubscriptionItem::Events { epoch, records, .. }) => {
                        if let Some(last) = records.last() {
                            cursor = Some(Cursor { epoch, seq: last.seq });
                        }
                        plumbing.push_feed(FeedEvent::Records { epoch, records });
                    }
                    None => break, // connection gone → outer reconnect loop
                },
                cmd = commands.recv() => match cmd {
                    Some(Command::Send(text)) => {
                        // Delivery truth arrives as journal records
                        // (UserSent/UserDelivered); a transport error means
                        // the connection is gone.
                        if client.send(&session_id, &text).await.is_err() {
                            break;
                        }
                    }
                    Some(Command::Kill) => {
                        let _ = client.kill(&session_id).await;
                    }
                    Some(Command::Resubscribe) => {
                        cursor = None;
                        break;
                    }
                    None => return, // UI gone
                },
            }
        }
    }
}

async fn pick_session(client: &Client) -> Result<String, ClientError> {
    let sessions = client.list().await?;
    match sessions.into_iter().next() {
        Some(info) => Ok(info.session_id),
        None => Ok(client.open(None, None).await?.session_id),
    }
}

// ---------------------------------------------------------------------------
// Entities (per-concern, spec 03 §View tree)

struct ConnectionState {
    status: ConnEvent,
}

struct TranscriptState {
    session_id: Option<String>,
    transcript: Transcript,
}

struct ComposerState {
    text: String,
}

// ---------------------------------------------------------------------------
// Shell

struct Shell {
    focus: FocusHandle,
    conn: Entity<ConnectionState>,
    transcript: Entity<TranscriptState>,
    composer: Entity<ComposerState>,
    commands: tokio_mpsc::UnboundedSender<Command>,
}

impl Shell {
    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "enter" => {
                let text = self.composer.update(cx, |c, cx| {
                    let t = c.text.trim().to_string();
                    c.text.clear();
                    cx.notify();
                    t
                });
                if !text.is_empty() {
                    let _ = self.commands.send(Command::Send(text));
                }
            }
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

    fn banner(&self, cx: &Context<Self>) -> gpui::Div {
        let (text, color): (String, u32) = match &self.conn.read(cx).status {
            ConnEvent::Connecting => ("connecting to engine…".into(), 0x8a8a5c),
            ConnEvent::Connected { engine_pid } => {
                (format!("engine pid {engine_pid}"), 0x4a6b4a)
            }
            ConnEvent::Reconnecting { attempt } => {
                (format!("reconnecting (attempt {attempt})…"), 0x8a8a5c)
            }
            ConnEvent::Busy { holder_pid } => (
                match holder_pid {
                    Some(pid) => format!(
                        "engine busy: another eave instance holds this data dir (pid {pid}) — quit it or use a different --data-dir"
                    ),
                    None => "engine busy: another eave instance holds this data dir (holder unknown) — quit it or use a different --data-dir".into(),
                },
                0x8a3a3a,
            ),
            ConnEvent::Failed { message } => (message.clone(), 0x8a3a3a),
        };
        div()
            .w_full()
            .px_2()
            .py_1()
            .bg(rgb(color))
            .text_color(rgb(0xffffff))
            .child(text)
    }

    fn row_el(row: &Row) -> gpui::Div {
        match row {
            Row::User { text, delivery } => {
                let line = div()
                    .text_color(rgb(0x9ecbff))
                    .child(format!("you: {text}"));
                match delivery {
                    Delivery::Delivered => line,
                    Delivery::Pending => line.child(
                        div().text_color(rgb(0x888888)).child("   (sending…)"),
                    ),
                    // The acceptance-critical distinction: journaled but
                    // never reached the agent (spec 02 §Delivery state).
                    Delivery::Undelivered => line.child(
                        div()
                            .text_color(rgb(0xff6b6b))
                            .child("   ✗ NOT DELIVERED — the agent never saw this message"),
                    ),
                }
            }
            Row::Assistant { text, live } => div().text_color(rgb(0xdddddd)).child(format!(
                "{}{}",
                text,
                if *live { " ▌" } else { "" }
            )),
            Row::Tool { id, name, status } => {
                let (label, color) = match status {
                    ToolStatus::Running => ("running…", 0xc9a86a),
                    ToolStatus::Finished => ("done", 0x7aa87a),
                    ToolStatus::Interrupted => ("interrupted", 0xff6b6b),
                };
                div()
                    .text_color(rgb(color))
                    .child(format!("[tool {name} {id}: {label}]"))
            }
            Row::Marker { text } => div()
                .text_color(rgb(0x777777))
                .child(format!("— {text} —")),
        }
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transcript = self.transcript.read(cx);
        let rows: Vec<gpui::Div> = transcript.transcript.rows.iter().map(Self::row_el).collect();
        let session_line = match &transcript.session_id {
            Some(id) => format!("session {id}"),
            None => "no session".to_string(),
        };
        let composer_text = self.composer.read(cx).text.clone();

        // Opaque window, plain rendering — locked decision (spec 03
        // §No-fork consequences).
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1b1b1b))
            .text_color(rgb(0xdddddd))
            .font_family("Menlo")
            .text_sm()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(self.banner(cx))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(rgb(0x666666))
                    .child(session_line),
            )
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .children(rows),
            )
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .bg(rgb(0x252525))
                    .text_color(rgb(0xffffff))
                    .child(format!("> {composer_text}▏")),
            )
    }
}

// ---------------------------------------------------------------------------
// Entry

pub fn run(data_dir: PathBuf) -> ! {
    application().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let queues = Arc::new(Queues::default());
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::channel::<()>(1);
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel::<Command>();

        // The client task lives on its own tokio runtime thread; GPUI's
        // executor never blocks on the wire.
        {
            let data_dir = data_dir.clone();
            let plumbing = Plumbing {
                queues: Arc::clone(&queues),
                wake: wake_tx,
            };
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime")
                    .block_on(feed_loop(data_dir, plumbing, cmd_rx));
            });
        }

        let conn = cx.new(|_| ConnectionState {
            status: ConnEvent::Connecting,
        });
        let transcript = cx.new(|_| TranscriptState {
            session_id: None,
            transcript: Transcript::new(),
        });

        // The pump (spec 03 §Wake & notify): one wake per burst, drain
        // every non-empty queue, ≤1 notify per entity per burst.
        {
            let queues = Arc::clone(&queues);
            let conn = conn.clone();
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
                    let feed_events: Vec<FeedEvent> =
                        queues.feed.lock().unwrap().drain(..).collect();
                    if !feed_events.is_empty() {
                        let cmd_tx = cmd_tx.clone();
                        let _ = transcript.update(cx, |state, cx| {
                            for ev in feed_events {
                                match ev {
                                    FeedEvent::Session { id } => state.session_id = Some(id),
                                    FeedEvent::Reset => state.transcript.reset(),
                                    FeedEvent::Records { epoch, records } => {
                                        if state.transcript.apply(epoch, &records).is_err() {
                                            // Out-of-sequence: drop local
                                            // state, resubscribe; a fresh
                                            // Reset heals (spec 03
                                            // §Streaming commit).
                                            state.transcript.reset();
                                            let _ = cmd_tx.send(Command::Resubscribe);
                                        }
                                    }
                                }
                            }
                            cx.notify();
                        });
                    }
                }
            })
            .detach();
        }

        let bounds = Bounds::centered(None, size(px(760.), px(560.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(|cx| {
                    let composer = cx.new(|_| ComposerState { text: String::new() });
                    cx.observe(&conn, |_, _, cx| cx.notify()).detach();
                    cx.observe(&transcript, |_, _, cx| cx.notify()).detach();
                    cx.observe(&composer, |_, _, cx| cx.notify()).detach();
                    Shell {
                        focus: cx.focus_handle(),
                        conn: conn.clone(),
                        transcript: transcript.clone(),
                        composer,
                        commands: cmd_tx.clone(),
                    }
                });
                let handle = shell.read(cx).focus.clone();
                window.focus(&handle, cx);
                shell
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    std::process::exit(0);
}
