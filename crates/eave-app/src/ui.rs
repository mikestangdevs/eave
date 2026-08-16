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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    actions, div, list, point, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity,
    FocusHandle, Font, FontStyle, FontWeight, Hsla, KeyBinding, KeyDownEvent, ListAlignment,
    ListOffset, ListState, ScrollWheelEvent, SharedString, StrikethroughStyle, StyledText,
    TextRun, TitlebarOptions, UnderlineStyle, Window, WindowBounds, WindowControlArea,
    WindowOptions,
};
use gpui::Animation;
use gpui::AnimationExt as _;
use gpui_platform::application;
use tokio::sync::mpsc as tokio_mpsc;

use eave_engine::client::{Client, ClientError, EngineSpawn};
use eave_wire::model::{Delivery, Row, ToolStatus, Transcript};
use eave_wire::{AttentionSnapshot, Cursor, SubscriptionItem, WaitReason};

use crate::display::{DisplayList, RowId};
use crate::markdown::{inline_runs, Block, InlineRun};
use crate::theme;

actions!(eave, [Quit]);

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

#[derive(Debug)]
enum FeedEvent {
    Session { id: String },
    Reset,
    Records {
        epoch: u64,
        records: Vec<eave_core::journal::Record>,
    },
}

/// One drained attention burst; last frame wins (the overlay is a current-
/// state broadcast, not a log).
type AttnEvent = AttentionSnapshot;

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
    attn: Mutex<VecDeque<AttnEvent>>,
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
    fn push_attn(&self, ev: AttnEvent) {
        self.queues.attn.lock().unwrap().push_back(ev);
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
                    Some(SubscriptionItem::Attention { attention, .. }) => {
                        plumbing.push_attn(attention);
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
    plumbing.push_feed(FeedEvent::Session {
        id: format!("replay:{}", path.file_name().unwrap_or_default().to_string_lossy()),
    });
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
            match &step.item {
                SubscriptionItem::Reset { .. } => {
                    if iter == 0 {
                        plumbing.push_feed(FeedEvent::Reset);
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
                        epoch: *epoch,
                        records,
                    });
                }
                SubscriptionItem::Attention { attention, .. } => {
                    plumbing.push_attn(attention.clone());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entities (per-concern, spec 03 §View tree)

struct ConnectionState {
    status: ConnEvent,
}

/// Session status badges (spec 03 §View tree: `SessionsState`): the
/// engine's transient attention overlay for the v0 session. The UI derives
/// nothing here — it renders exactly what the engine classified, plus
/// wall-clock elapsed off the snapshot's `since_ms`.
struct SessionsState {
    attention: Option<AttentionSnapshot>,
}

impl SessionsState {
    fn notice_standing(&self) -> bool {
        matches!(&self.attention, Some(a) if !matches!(a, AttentionSnapshot::Running))
    }
}

/// Quiet escalates from a subtle line to a prominent bar at 10 min
/// (build directive 2026-08-16: "subtle at threshold, prominent at 10min,
/// then steady elapsed reporting").
const PROMINENT_AFTER_MS: u64 = 10 * 60 * 1000;

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
}

impl TranscriptState {
    fn new() -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(512.));
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
        }
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
        let ops = self.display.update(&self.transcript);
        if ops.reset {
            self.list.reset(self.display.len());
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
        self.list.reset(self.display.len());
        self.veil = None;
    }
}

struct ComposerState {
    text: String,
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
                thickness: px(1.),
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

struct Shell {
    focus: FocusHandle,
    conn: Entity<ConnectionState>,
    sessions: Entity<SessionsState>,
    transcript: Entity<TranscriptState>,
    composer: Entity<ComposerState>,
    commands: tokio_mpsc::UnboundedSender<Command>,
    perf: Option<PerfProbe>,
}

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

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
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

    fn on_wheel(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // User scroll breaks follow/pin; wheel back to the end restores
        // follow (spec 03 §Scroll). Work is deferred out of the scroll
    	// handler (known ListState borrow papercut, spec 03 §No-fork
        // consequences).
        let up = ev.delta.pixel_delta(window.line_height()).y > px(0.);
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
                div()
                    .text_size(px(theme::TITLE_SIZE))
                    .font_weight(FontWeight::MEDIUM)
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
        div().w_full().flex_none().pt(px(4.)).pb(px(10.)).child(
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
                            .child(label),
                    )
                    .children(session.map(|s| {
                        div().text_color(theme::text_ghost()).child(s)
                    })),
            ),
        )
    }

    /// Loud connection states get a real panel above the composer with the
    /// full actionable message; the footer only carries the short fact.
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
        Some(
            Self::column(
                div()
                    .w_full()
                    .rounded(px(theme::RADIUS_PANEL))
                    .border_1()
                    .border_color(theme::danger_ring())
                    .bg(theme::danger_wash())
                    .shadow(theme::shadow_panel())
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(px(theme::DETAIL_SIZE))
                    .line_height(px(theme::DETAIL_LINE))
                    .text_color(theme::text_secondary())
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::danger())
                            .child("engine unavailable"),
                    )
                    .child(message),
            )
            .pb(px(8.))
            .into_any_element(),
        )
    }

    /// The attention notice (spec 01 §States): `quiet` escalates subtle →
    /// prominent and reports elapsed steadily; `unresponsive` and `failed`
    /// are prominent panels with the evidence/cause and a kill affordance.
    /// Nothing here ever auto-dismisses or auto-kills — notices persist
    /// until the engine reclassifies or the user acts.
    fn attention_notice(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let attention = self.sessions.read(cx).attention.clone()?;
        let now = eave_core::journal::now_ms();
        let panel = |ring: Hsla, wash: Hsla| {
            div()
                .w_full()
                .rounded(px(theme::RADIUS_PANEL))
                .border_1()
                .border_color(ring)
                .bg(wash)
                .shadow(theme::shadow_panel())
                .px(px(12.))
                .py(px(8.))
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
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::MEDIUM
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
                        theme::warning_wash_strong()
                    } else {
                        theme::warning_wash()
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
            } => panel(theme::danger_ring(), theme::danger_wash())
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
            AttentionSnapshot::Failed { since_ms, cause } => {
                panel(theme::danger_ring(), theme::danger_wash())
                    .child(label(
                        theme::danger(),
                        format!("failed {} ago", fmt_elapsed(now.saturating_sub(since_ms))),
                        true,
                    ))
                    .child(cause)
                    .child(self.kill_affordance(cx, "clean up the process when you're ready"))
            }
        };
        Some(Self::column(inner).pb(px(8.)).into_any_element())
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
                    .px(px(10.))
                    .py(px(3.))
                    .rounded(px(theme::RADIUS_BUTTON))
                    .border_1()
                    .border_color(theme::danger_ring())
                    .bg(theme::danger_wash_strong())
                    .text_size(px(theme::DETAIL_SIZE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::danger())
                    .id("attn-kill")
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
    fn render_row(state: &TranscriptState, ix: usize) -> AnyElement {
        let Some(row) = state.display.row(ix).copied() else {
            return div().into_any_element();
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
                    .px(px(12.))
                    .py(px(8.))
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
                                .font_weight(FontWeight::MEDIUM)
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
                    .pt(px(8.))
                    .pb(px(2.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(theme::LABEL_SIZE))
                    .line_height(px(theme::LABEL_LINE))
                    .child(div().size(px(theme::TOOL_DOT)).rounded_full().bg(dot))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_secondary())
                            .child(SharedString::from(name.clone())),
                    )
                    .child(div().text_color(theme::text_tertiary()).child(label))
                    .into_any_element()
            }
            Row::Marker { text } => div()
                .py(px(12.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(10.))
                        .py(px(4.))
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
                    // Placeholder row of an empty live message.
                    return Self::column(
                        div()
                            .pt(px(theme::MSG_TOP_GAP))
                            .text_color(theme::live())
                            .child("▌"),
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
                            .h(px(24.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(theme::border())
                            .text_size(px(theme::MICRO_SIZE))
                            .line_height(px(theme::MICRO_LINE))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text_ghost())
                            .child(SharedString::from(label.to_string())),
                    )
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .font_family(theme::MONO)
                            .text_size(px(theme::CODE_SIZE))
                            .line_height(px(theme::CODE_LINE))
                            .text_color(theme::code_block_text())
                            .child(SharedString::from(code.trim_end().to_string())),
                    )
                    .into_any_element()
            }
            Block::Heading { level, text } => {
                let (size, line, bold) = theme::heading(*level);
                div()
                    .pt_1()
                    .text_size(size)
                    .line_height(line)
                    .text_color(theme::text())
                    .font_weight(if bold {
                        FontWeight::BOLD
                    } else {
                        FontWeight::SEMIBOLD
                    })
                    .child(SharedString::from(text.clone()))
                    .into_any_element()
            }
            Block::Rule => div()
                .my_1()
                .h(px(1.))
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
                            ("veil", ((id.msg as u64) << 32) | (v.generation & 0xffff_ffff)),
                            Animation::new(v.duration),
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

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    c.sort();
                    let pc = |q: f64| c[((c.len() - 1) as f64 * q) as usize];
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

        let transcript_entity = self.transcript.clone();
        let state = self.transcript.read(cx);
        let list_state = state.list.clone();
        // The ↓-bottom affordance needs something to return to: never on an
        // empty log (the Stage E cosmetic bug), only when follow is broken.
        let show_affordance = state.scroll != ScrollMode::Follow && !state.display.is_empty();
        let composer_text = self.composer.read(cx).text.clone();
        let has_draft = !composer_text.is_empty();

        let transcript_el = list(list_state, move |ix, _window, cx| {
            transcript_entity.read_with(cx, |state, _| Shell::render_row(state, ix))
        })
        .size_full();

        let composer = Self::column(
            div()
                .w_full()
                .rounded(px(theme::RADIUS_COMPOSER))
                .border_1()
                .border_color(theme::border_strong())
                .bg(theme::composer())
                .shadow(theme::shadow_panel())
                .p(px(10.))
                .child(
                    div()
                        .min_h(px(24.))
                        .px(px(4.))
                        .pt(px(2.))
                        .text_size(px(theme::BODY_SIZE))
                        .line_height(px(22.))
                        .text_color(theme::text())
                        .child(if has_draft {
                            div().child(format!("{composer_text}▏")).into_any_element()
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
                        .mt(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(theme::MICRO_SIZE))
                                .text_color(theme::text_ghost())
                                .child("return to send"),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .size(px(theme::SEND_BUTTON))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if has_draft {
                                    theme::inverse()
                                } else {
                                    theme::overlay_strong()
                                })
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if has_draft {
                                    theme::on_inverse()
                                } else {
                                    theme::text_ghost()
                                })
                                .id("send")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send_current(cx);
                                }))
                                .child("↑"),
                        ),
                ),
        )
        .flex_none();

        // Opaque window, custom titlebar — locked decision (spec 03
        // §No-fork consequences) plus the Stage F visual pass.
        div()
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
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .child(self.titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .relative()
                    .pt(px(theme::LOG_TOP_PAD))
                    .pb(px(theme::LOG_BOTTOM_PAD))
                    .child(transcript_el)
                    .when(show_affordance, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom_3()
                                .left_0()
                                .right_0()
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .px(px(12.))
                                        .py(px(5.))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(theme::border_strong())
                                        .bg(theme::raised())
                                        .shadow(theme::shadow_panel())
                                        .text_size(px(theme::LABEL_SIZE))
                                        .text_color(theme::text_secondary())
                                        .id("to-bottom")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.transcript.update(cx, |t, cx| {
                                                t.scroll = ScrollMode::Follow;
                                                t.assert_scroll();
                                                t.list.scroll_to_end();
                                                cx.notify();
                                            });
                                        }))
                                        .child("↓ bottom"),
                                ),
                        )
                    }),
            )
            .children(self.conn_panel(cx))
            .children(self.attention_notice(cx))
            .child(composer)
            .child(self.footer(cx))
    }
}

// ---------------------------------------------------------------------------
// Entry

pub fn run(data_dir: PathBuf) -> ! {
    let replay: Option<(PathBuf, f64)> = std::env::var("EAVE_REPLAY").ok().map(|p| {
        let speed = std::env::var("EAVE_REPLAY_SPEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);
        (PathBuf::from(p), speed)
    });
    let perf_10k = std::env::var("EAVE_PERF").map(|v| v == "10k").unwrap_or(false);

    application().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let queues = Arc::new(Queues::default());
        let (wake_tx, mut wake_rx) = futures::channel::mpsc::channel::<()>(1);
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

        let conn = cx.new(|_| ConnectionState {
            status: ConnEvent::Connecting,
        });
        let sessions = cx.new(|_| SessionsState { attention: None });
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
            cx.spawn(async move |cx| loop {
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
                if sessions.read_with(cx, |s, _| s.notice_standing()) {
                    sessions.update(cx, |_, cx| cx.notify());
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
                    if let Some(last) = attn_events.into_iter().last() {
                        let _ = sessions.update(cx, |state, cx| {
                            state.attention = Some(last);
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
                                    FeedEvent::Reset => state.reset(),
                                    FeedEvent::Records { epoch, records } => {
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
                    }
                }
            })
            .detach();
        }

        let bounds = Bounds::centered(None, size(px(theme::WINDOW_W), px(theme::WINDOW_H)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(theme::WINDOW_MIN_W), px(theme::WINDOW_MIN_H))),
                titlebar: Some(TitlebarOptions {
                    title: Some("eave".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(theme::TRAFFIC_X), px(theme::TRAFFIC_Y))),
                }),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(|cx| {
                    let composer = cx.new(|_| ComposerState { text: String::new() });
                    cx.observe(&conn, |_, _, cx| cx.notify()).detach();
                    cx.observe(&sessions, |_, _, cx| cx.notify()).detach();
                    cx.observe(&transcript, |_, _, cx| cx.notify()).detach();
                    cx.observe(&composer, |_, _, cx| cx.notify()).detach();
                    Shell {
                        focus: cx.focus_handle(),
                        conn: conn.clone(),
                        sessions: sessions.clone(),
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
    state.list.reset(state.display.len());
    state.scroll = ScrollMode::Free;
    state.assert_scroll();
    state.list.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
    });
}
