//! CEF inside a gpui process: the `NSApp` that satisfies both, and the pump.
//!
//! The parked CEF attempt recorded its blocker as "`GPUIApplication` does not
//! conform to `CefAppProtocol`". It is a real requirement — Chromium's macOS
//! message pump reaches for `-[NSApp isHandlingSendEvent]` — but it is not a
//! wall, and the spike dissolved it in two moves:
//!
//! 1. **The class.** `GPUIApplication` is registered by a `#[ctor]` at load
//!    time, and `+[NSApplication sharedApplication]` builds its instance from
//!    the *receiver's* class when `NSApp` is still nil. So subclass gpui's
//!    class at runtime, add the two methods, and ask the SUBCLASS for
//!    `sharedApplication` before gpui asks. gpui then finds an `NSApp` that is
//!    still a `GPUIApplication` (its `platform` ivar is where it expects) and
//!    Chromium finds one that answers the protocol.
//! 2. **The run loop.** `CefRunMessageLoop` calls `[NSApp run]`, and so does
//!    gpui; only one of them can. `external_message_pump` hands that back to
//!    the host — CEF asks to be pumped and we pump it.
//!
//! # Ordering, which is the whole difficulty
//! [`install_nsapp`] must run before gpui touches `NSApp`, and
//! [`initialize`] must run after `execute_process` has decided this is the
//! browser process. They are two calls rather than one because the window
//! between them is where `main` decides what kind of process it is at all.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use cef::*;
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, msg_send, sel};

// ---------------------------------------------------------------------------
// 1. An NSApp that both gpui and Chromium accept
// ---------------------------------------------------------------------------

static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

extern "C" fn is_handling_send_event(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::SeqCst))
}

extern "C" fn set_handling_send_event(_this: &AnyObject, _sel: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.store(handling.as_bool(), Ordering::SeqCst);
}

extern "C" fn send_event(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
    // Chromium reads this flag to know it is inside -[NSApplication
    // sendEvent:] and must not re-enter its own nested loops. Restore the
    // previous value rather than clearing: sendEvent: nests.
    let previous = HANDLING_SEND_EVENT.swap(true, Ordering::SeqCst);
    unsafe {
        let imp = class!(NSApplication)
            .instance_method(sel!(sendEvent:))
            .unwrap()
            .implementation();
        let imp: extern "C" fn(&AnyObject, Sel, *mut AnyObject) = std::mem::transmute(imp);
        imp(this, sel!(sendEvent:), event);
    }
    HANDLING_SEND_EVENT.store(previous, Ordering::SeqCst);
}

/// Register a `GPUIApplication` subclass that answers `CefAppProtocol`, then
/// make `NSApp` an instance of it.
///
/// Built with the raw objc runtime rather than objc2's `ClassBuilder`,
/// because two objc2 majors are in this graph at once (gpui carries 0.5, cef
/// carries 0.6) and the typed builder's `MethodImplementation` bound will not
/// resolve across them. `objc_allocateClassPair` has no opinion about any of
/// that. This is the rule at every gpui/CEF type seam, not a one-off.
///
/// Failure here is not fatal and must not be: a build that cannot start
/// Chromium should still be eave. It reports and returns.
pub fn install_nsapp() {
    if let Err(e) = try_install_nsapp() {
        eprintln!("eave: no page engine — {e}");
        DISABLED.store(true, Ordering::SeqCst);
    }
}

static DISABLED: AtomicBool = AtomicBool::new(false);

fn try_install_nsapp() -> Result<(), String> {
    let gpui_class = AnyClass::get(c"GPUIApplication")
        .ok_or("GPUIApplication is not registered — is gpui linked in?")?;

    unsafe {
        let cls = objc_allocateClassPair(
            gpui_class as *const AnyClass as *mut _,
            c"EaveCefApplication".as_ptr(),
            0,
        );
        if cls.is_null() {
            return Err("objc_allocateClassPair returned nil".into());
        }
        class_addMethod(
            cls,
            sel!(isHandlingSendEvent),
            is_handling_send_event as *const c_void,
            c"c@:".as_ptr(),
        );
        class_addMethod(
            cls,
            sel!(setHandlingSendEvent:),
            set_handling_send_event as *const c_void,
            c"v@:c".as_ptr(),
        );
        class_addMethod(
            cls,
            sel!(sendEvent:),
            send_event as *const c_void,
            c"v@:@".as_ptr(),
        );

        // Chromium's DCHECK on `conformsToProtocol:` is compiled out of a
        // release framework, but claiming conformance is cheap and makes the
        // class honest for anything that does check.
        for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
            let existing = objc_getProtocol(name.as_ptr());
            let proto = if existing.is_null() {
                let p = objc_allocateProtocol(name.as_ptr());
                if p.is_null() {
                    continue;
                }
                objc_registerProtocol(p);
                p
            } else {
                existing
            };
            class_addProtocol(cls, proto);
        }
        objc_registerClassPair(cls);

        let app: *mut AnyObject = msg_send![cls as *mut AnyObject, sharedApplication];
        if app.is_null() {
            return Err("sharedApplication returned nil".into());
        }
        let gpui_kind: Bool = msg_send![app, isKindOfClass: gpui_class];
        let responds: Bool = msg_send![app, respondsToSelector: sel!(isHandlingSendEvent)];
        if !gpui_kind.as_bool() || !responds.as_bool() {
            return Err("NSApp does not satisfy both gpui and CEF".into());
        }
    }
    Ok(())
}

unsafe extern "C" {
    fn objc_allocateClassPair(
        superclass: *mut c_void,
        name: *const std::os::raw::c_char,
        extra_bytes: usize,
    ) -> *mut c_void;
    fn objc_registerClassPair(cls: *mut c_void);
    fn class_addMethod(
        cls: *mut c_void,
        name: Sel,
        imp: *const c_void,
        types: *const std::os::raw::c_char,
    ) -> Bool;
    fn objc_getProtocol(name: *const std::os::raw::c_char) -> *mut c_void;
    fn objc_allocateProtocol(name: *const std::os::raw::c_char) -> *mut c_void;
    fn objc_registerProtocol(proto: *mut c_void);
    fn class_addProtocol(cls: *mut c_void, proto: *mut c_void) -> Bool;
}

// ---------------------------------------------------------------------------
// 2. The pump
// ---------------------------------------------------------------------------
//
// `external_message_pump` means CEF never runs its own loop: it asks to be
// pumped, and the host decides when. Two things have to be true of whatever
// does the asking-to-pumping:
//
//   - `do_message_loop_work` must run on the MAIN thread, and
//   - an idle page must cost nothing, because the spike's headline number
//     (1 frame in 9 seconds on an idle page) is a property worth keeping.
//
// This went through `dispatch_after_f` on `_dispatch_main_q` first, which is
// the textbook answer and did not work: CEF asked once, and the callback
// never arrived — `schedule #0` with no matching `pump #0`, and a browser
// that was created and then never loaded anything. Rather than keep guessing
// at libdispatch from FFI, the scheduling goes through gpui's own executor,
// which is already the thing that owns this thread.
//
// A parked `recv()` costs nothing, so the idle property survives.

/// Where CEF's pump requests go, and the receiving end waiting to be picked
/// up by the shell.
///
/// **Both ends are created before `CefInitialize`, and that is load-bearing.**
/// CEF asks for its first pump DURING initialisation, long before the window
/// exists — and it does not ask twice. An earlier version created the channel
/// when the shell started the pump task, so that first request landed on a
/// `None` and was dropped, after which nothing ever pumped again: the browser
/// was created, never finished creating, and the page stayed blank with no
/// error anywhere. The queue exists first so there is nothing to drop.
static PUMP: OnceLock<tokio::sync::mpsc::UnboundedSender<i64>> = OnceLock::new();
static PUMP_RX: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<i64>>> = Mutex::new(None);

/// Create the pump channel. Call before `CefInitialize`.
fn open_pump_channel() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<i64>();
    if PUMP.set(tx).is_ok() {
        *PUMP_RX.lock().unwrap() = Some(rx);
    }
}

/// Ask for a pump. Anything that hands CEF work — creating a browser,
/// loading a url, forwarding a click — calls this so the work is acted on
/// now rather than at the next heartbeat.
pub fn request_pump() {
    if let Some(tx) = PUMP.get() {
        let _ = tx.send(0);
    }
}

/// Pump CEF on this stack.
///
/// The async pump task lives on gpui's executor, which does not run while
/// AppKit is inside a live-resize tracking loop (`NSEventTrackingRunLoopMode`).
/// `PageSurface::paint` therefore calls this after `WasResized` so Chromium
/// can layout on the same frames the window is changing size. Re-entrant
/// calls are a no-op: nesting `CefDoMessageLoopWork` is forbidden, and the
/// in-flight pump already re-posts via [`REQUESTED_DURING_PUMP`].
pub fn pump_now() {
    if !running() {
        return;
    }
    if IN_PUMP.load(Ordering::SeqCst) {
        REQUESTED_DURING_PUMP.store(true, Ordering::SeqCst);
        return;
    }
    pump_cef();
}

/// Mark the page as busy, so the pump stays fast for a moment.
pub fn note_activity() {
    ACTIVE_UNTIL.store(now_ms() + BUSY_TAIL_MS, Ordering::Relaxed);
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static ACTIVE_UNTIL: AtomicU64 = AtomicU64::new(0);

/// How long after the last frame or input the pump stays at [`BUSY_MS`].
const BUSY_TAIL_MS: u64 = 400;
/// The pump interval while a page is doing something.
const BUSY_MS: u64 = 8;
/// The pump interval while CEF is up but no browser is live.
///
/// **This used to be a park, and the park is what made `kill` hang.** With
/// nothing to pump for, the loop waited on the request channel forever —
/// and Chromium answers a signal by posting a shutdown task onto the UI
/// thread, which only ever runs if something pumps. So SIGTERM on an eave
/// with no page open did nothing at all and the process had to be SIGKILLed
/// (Slice C6). Four wakeups a second is the price of a process that can be
/// asked to stop; it is 60× cheaper than the spike's 4 ms timer, and nothing
/// repaints on it.
const IDLE_MS: u64 = 250;

/// The pump interval while a page is live but quiet. A page that is merely
/// open still needs its timers and its network callbacks run, so this cannot
/// be zero — but it is 12× cheaper than the busy rate, and NOTHING repaints
/// on it: `pump` only refreshes the window when a frame actually arrived.
const QUIET_MS: u64 = 100;

/// Run CEF's pending work on gpui's foreground executor, forever.
///
/// # The rule this obeys
/// **A foreign event loop is lent every borrow you hold.**
/// `CefDoMessageLoopWork` runs arbitrary Chromium tasks, and those tasks call
/// back into this process *synchronously*. So this loop enters CEF with **no
/// gpui borrow held**: pumping and repainting are two separate steps, and only
/// the repaint takes [`gpui::AsyncApp::update`].
///
/// Slice C6 measured what the previous shape cost. It was `cx.update(pump)` —
/// the borrow held across the CEF call — and on a signal Chromium posts
/// `-[NSApplication terminate:]`, AppKit fires `WillTerminate`
/// **synchronously**, and gpui's observer reaches for the app `RefCell` that
/// the pump is still holding. `already borrowed`, then abort because the panic
/// cannot unwind back through the Objective-C frames. **48 runs out of 48.**
/// gpui states the same invariant from the other side: `MacPlatform::quit`
/// defers `terminate:` to a fresh main-queue turn *precisely* so that no
/// borrow is on the stack when the app goes down. Pumping inside `cx.update`
/// handed Chromium a way around that, and [`watch_for_signals`] is how the
/// decision is taken back.
///
/// # Why there is a heartbeat at all
/// `OnScheduleMessagePumpWork` is wired and honoured, and it is **not
/// sufficient on its own** — but not for the reason this comment used to
/// give. It claimed CEF "calls it exactly once, ever. Measured, not
/// inferred", and that was the whole justification for the timer. C6
/// re-measured: a thirty-second run logs `schedule #0` through `#24`, which
/// is the trace's own cap, so the true count is *at least* 25 — see
/// [`TRACE_CAP`], which now says so in the log rather than leaving the next
/// reader to find it here. The once-ever number was an artefact of the bug it
/// was recorded alongside — the `App` object was being dropped after
/// `initialize`, taking the handler with it, which is fixed and documented
/// above.
///
/// What IS true, and what the heartbeat is actually for, is the next
/// paragraph's problem: **CEF suppresses `OnScheduleMessagePumpWork` for the
/// duration of a `CefDoMessageLoopWork` call**, so work queued while we are
/// inside one raises no request. [`pump_cef`] re-posts on the way out, and
/// the heartbeat is the backstop for anything that slips between the two.
///
/// C7 re-measured that too rather than inheriting it. With
/// `EAVE_WEB_NO_HEARTBEAT=1` the loop waits on requests alone, and a run that
/// normally logs 25 pumps and paints twice instead **logs 2–4 pumps and never
/// paints at all**. So the heartbeat stays — and the knob stays with it,
/// deliberately, because the reason this comment was wrong for two slices is
/// that re-checking it required an edit. Two things keep it from being the
/// spike's 4 ms timer wearing a hat: it decays to [`QUIET_MS`] when nothing
/// is happening, and it drops to [`IDLE_MS`] when no browser is live.
///
/// And the property the spike measured is untouched: the pump does not
/// repaint. [`refresh_if_painted`] asks gpui to redraw only when the frame
/// counter moved, so an idle page still costs zero redraws however often it
/// is pumped.
pub fn start_pump(cx: &mut gpui::App) {
    let Some(mut rx) = PUMP_RX.lock().unwrap().take() else {
        return;
    };
    watch_for_signals();
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        // One unconditional pump before waiting on anything: whatever CEF
        // asked for during initialisation is satisfied here.
        pump_cef();
        refresh_if_painted(cx);
        loop {
            let ms = if super::tabs::any_browser() {
                let quiet = now_ms() >= ACTIVE_UNTIL.load(Ordering::Relaxed);
                if quiet {
                    QUIET_MS
                } else {
                    BUSY_MS
                }
            } else {
                IDLE_MS
            };
            // Whichever comes first: the heartbeat, or somebody asking. The
            // request side is what keeps browser creation prompt now that
            // the idle case is a slow tick rather than a park.
            if no_heartbeat() {
                // Measurement mode only (see the heartbeat note above): wait
                // on requests alone, so "is `OnScheduleMessagePumpWork`
                // sufficient by itself?" can be answered by running it rather
                // than argued about.
                if rx.recv().await.is_none() {
                    return;
                }
            } else {
                let timer = cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(ms));
                futures::future::select(std::pin::pin!(timer), std::pin::pin!(rx.recv())).await;
            }
            while rx.try_recv().is_ok() {}

            if quit_requested() {
                if std::env::var("EAVE_WEB_TRACE").is_ok() {
                    eprintln!("[web] quit requested — stopping the page engine");
                }
                // **The page engine is stopped HERE, on the pump's own
                // stack.** Not in the quit observer: that runs inside
                // `[NSApp terminate:]`, and `CefShutdown` called from inside
                // the `WillTerminate` notification blocks forever — measured,
                // with settle periods of 0, 300 and 1000 ms, all three
                // hanging. Here there is no borrow held, no `terminate:`
                // above us and no `CefDoMessageLoopWork` around us, which is
                // the only stack CEF will shut down on.
                stop_cef(true);
                // gpui's quit is DEFERRED to a fresh main-queue turn, and
                // that is exactly what is wanted: `will_terminate` then runs
                // with no borrow held and outside `CefDoMessageLoopWork`, so
                // the quit observer can close browsers and call `CefShutdown`
                // legally. Keep pumping afterwards — the deferred block is
                // main-queue work like any other and still has to be run.
                cx.update(|app| app.quit());
            }
            // Slice E3's eviction clock, read HERE and not on snapshots:
            // the pump ticks whether or not anything changed, which is the
            // property a clock needs. It runs BEFORE `pump_cef` because a
            // close it asks for is work CEF has to be pumped to finish, and
            // that pump is on the next line rather than a tick away.
            //
            // No borrow is held across it, for the same reason nothing else
            // here holds one: it closes browsers, and closing a browser
            // runs `on_before_close` synchronously on this stack.
            super::evict::tick();
            pump_cef();
            refresh_if_painted(cx);
        }
    })
    .detach();
}

/// Signals, answered by eave rather than by Chromium.
///
/// Chromium installs its own `SIGTERM`/`SIGINT`/`SIGHUP` handlers during
/// `CefInitialize` and answers them by posting `-[NSApplication terminate:]`
/// onto the UI thread — which is to say, from inside whatever
/// `CefDoMessageLoopWork` call happens to run it. That is the stack Slice C6
/// found under 48 of 48 aborts.
///
/// Taking the signal ourselves moves the decision somewhere we control. The
/// handler does nothing but set a flag — one atomic store, which is all that
/// is async-signal-safe — and the pump turns that flag into gpui's own
/// deferred quit on its next turn. Installed AFTER `CefInitialize` so that
/// ours is the handler that sticks.
fn watch_for_signals() {
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, on_quit_signal as *const () as libc::sighandler_t);
        }
    }
}

extern "C" fn on_quit_signal(_sig: i32) {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// True exactly once, on the first turn after a quit signal arrives.
fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst) && !QUIT_STARTED.swap(true, Ordering::SeqCst)
}

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static QUIT_STARTED: AtomicBool = AtomicBool::new(false);

/// True while inside `do_message_loop_work`.
///
/// **CEF suppresses `OnScheduleMessagePumpWork` for the duration of a
/// `CefDoMessageLoopWork` call.** Any work Chromium queues while we are
/// inside one therefore produces no request, and a host that only pumps on
/// request stops pumping — permanently, after two pumps. That is not a
/// theoretical hazard: it is what this did first, and the symptom was a
/// browser that was created and then never loaded its page, with no error
/// anywhere. cefclient's external pump solves it the same way, by noticing
/// requests raised during the pump and re-posting once afterwards.
static IN_PUMP: AtomicBool = AtomicBool::new(false);
static REQUESTED_DURING_PUMP: AtomicBool = AtomicBool::new(false);

/// One pump, with **no gpui borrow held**. See [`start_pump`] for why that
/// is the entire point of this function existing separately from
/// [`refresh_if_painted`].
fn pump_cef() {
    if traced() {
        let n = PUMPS.fetch_add(1, Ordering::Relaxed);
        trace_capped("pump", n, format_args!(""));
    }
    IN_PUMP.store(true, Ordering::SeqCst);
    REQUESTED_DURING_PUMP.store(false, Ordering::SeqCst);
    do_message_loop_work();
    IN_PUMP.store(false, Ordering::SeqCst);
    // Favicon downloads queued from OnFaviconURLChange start HERE, after
    // the pump, not inside the display handler. See `favicon.rs`.
    super::favicon::run_queued();
    // DevTools docks whose target lookup came back on a worker thread.
    // Same reason as the favicons: it creates a browser, which does not
    // belong on a worker thread or inside a CEF callback.
    super::tabs::run_queued_docks();
    // The notification and audible shims read their CDP events here too,
    // not only on paint — see `web::tick_shims`.
    super::tick_shims();
    // Widevine's first update check, once Chromium has registered its
    // components — which is AFTER `CefInitialize` returns (0 registered
    // at that instant, measured 2026-09-03). See `cdm.rs`.
    super::cdm::poll();
    // Anything Chromium queued while we were inside raised no request, so
    // ask for one more pass. This is what keeps the chain alive; without it
    // the loop runs on the heartbeat alone with work outstanding.
    if REQUESTED_DURING_PUMP.swap(false, Ordering::SeqCst) {
        if let Some(tx) = PUMP.get() {
            let _ = tx.send(0);
        }
    }
}

/// Ask gpui to redraw — and **only** if a frame actually arrived.
///
/// **Render on delivered frames, never on vsync.** The spike measured an idle
/// page at 1 frame in 9 seconds; a tab that repainted the window at 60 Hz
/// regardless would throw that away and burn a GPU on a page that is not
/// moving.
///
/// This is the one part of the pump that takes gpui's borrow, and it takes it
/// **after** `CefDoMessageLoopWork` has returned. Do not fold it back into the
/// pump: that shape is what Slice C6 removed.
fn refresh_if_painted(cx: &mut gpui::AsyncApp) {
    // D11's dwell completes here, not on vsync: a pointer sitting on a
    // link delivers no frame, and promoting first means the bubble
    // appearing is a chrome revision this same check can see.
    super::tabs::promote_status();
    // Two reasons to redraw, one gate. A frame arriving is the obvious
    // one; the chrome changing is the one added with the display handler,
    // because a cursor crossing a link boundary or a status bubble
    // appearing is a real change that may deliver no frame at all. Both
    // counters move on an EVENT and not on a clock, so the "never on
    // vsync" property is unchanged.
    //
    // The two counters are PACKED, not mixed: frames in the low 32 bits,
    // chrome revisions in the high 32. A hash of the pair could collide and
    // silently drop a redraw; a pack cannot until one of them passes 2^32,
    // which at 60 Hz is eight centuries of frames.
    //
    // Slice R1 merged E1's `CHROME_DIRTY` in ALONGSIDE the pack rather than
    // choosing between them. They are not redundant: the pack covers chrome
    // eave learned about FROM Chromium (a link hover, a status bubble), and
    // the flag covers chrome eave decided on ITSELF (a find tally moving
    // "3 of 17" to "4 of 17", a context menu opening) — which bumps no CEF
    // revision and would otherwise wait for an unrelated frame.
    let chrome = CHROME_DIRTY.swap(false, Ordering::Relaxed);
    let seen =
        (super::tabs::chrome_revision() << 32) | (super::osr::frames_delivered() & 0xffff_ffff);
    let last = LAST_REFRESHED.load(Ordering::Relaxed);
    if seen == last && !chrome {
        return;
    }
    // E1: only a real FRAME is activity. A chrome change is not, or an open
    // find bar would hold the pump at `BUSY_MS` for as long as it is up.
    let painted = (seen & 0xffff_ffff) != (last & 0xffff_ffff);
    LAST_REFRESHED.store(seen, Ordering::Relaxed);
    if painted {
        note_activity();
    }
    cx.update(|app| app.refresh_windows());
}

/// How many `pump`/`schedule` lines the trace prints before it goes quiet.
///
/// The cap exists because both counters run at the heartbeat's rate and an
/// uncapped trace is a log nobody reads. **What it must never be is silent**,
/// which is what it was, and that silence has now produced two wrong
/// conclusions from the same log:
///
///   * C6's predecessor read `schedule #0` with no more after it and wrote
///     down "CEF calls `OnScheduleMessagePumpWork` exactly once, ever.
///     Measured, not inferred" — the justification for the heartbeat, from a
///     number the cap had truncated.
///   * 2026-08-21, reading a blank-page capture: `pump #0 … pump #24` was
///     read as "the pump starves at 25 in 14 s" and blamed on C7's interval
///     logic. The true count that run was in the hundreds; the page was blank
///     for another reason entirely, and the pump was never involved.
///
/// So the cap announces itself once, and [`shutdown`] prints the totals the
/// counters actually hold. A truncated log that says it is truncated is a
/// measurement; one that does not is a trap that reads as evidence.
const TRACE_CAP: u64 = 25;

/// One capped trace line. `extra` is appended verbatim after the counter.
fn trace_capped(what: &str, n: u64, extra: std::fmt::Arguments<'_>) {
    if n < TRACE_CAP {
        eprintln!("[web] {what} #{n}{extra}");
    } else if n == TRACE_CAP {
        eprintln!(
            "[web] {what}: {TRACE_CAP} lines is the trace cap — no more will be \
             printed. The counter keeps running; the total is in the shutdown \
             line. DO NOT read the last number above as a count."
        );
    }
}

/// Something eave draws ABOUT the page changed, and no page frame is coming
/// to carry it onto the screen.
///
/// The counterpart to the frame counter — see [`refresh_if_painted`]. Cheap
/// and idempotent, so the callers do not have to know whether a redraw is
/// already pending.
pub(super) fn note_chrome_change() {
    CHROME_DIRTY.store(true, Ordering::Relaxed);
    request_pump();
}

static CHROME_DIRTY: AtomicBool = AtomicBool::new(false);

/// `EAVE_WEB_TRACE`, read once. The pump asks on every turn and the shutdown
/// loop runs it a thousand times, so an `env::var` per call is a real cost.
fn no_heartbeat() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| std::env::var("EAVE_WEB_NO_HEARTBEAT").is_ok())
}

pub(super) fn traced() -> bool {
    static TRACED: OnceLock<bool> = OnceLock::new();
    *TRACED.get_or_init(|| std::env::var("EAVE_WEB_TRACE").is_ok())
}

static LAST_REFRESHED: AtomicU64 = AtomicU64::new(0);
static PUMPS: AtomicU64 = AtomicU64::new(0);
static SCHEDULES: AtomicU64 = AtomicU64::new(0);

wrap_browser_process_handler! {
    struct EaveBrowserProcessHandler {}

    impl BrowserProcessHandler {
        /// CEF asking to be pumped.
        ///
        /// This replaces the spike's 4 ms timer, which existed so a probe
        /// could prove liveness without also arguing about efficiency. A
        /// fixed timer wakes the process 250 times a second forever; this
        /// wakes it when Chromium has something to do, which on an idle page
        /// is almost never — the property worth keeping.
        ///
        /// May be called from any thread, which is the whole reason this is
        /// a channel send and not the work itself.
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            // Counted unconditionally, not just under trace: this counter is
            // the predicate `stop_cef` shuts down on. See there.
            let n = SCHEDULES.fetch_add(1, Ordering::Relaxed);
            if traced() {
                trace_capped(
                    "schedule",
                    n,
                    format_args!(
                        " delay={delay_ms} in_pump={}",
                        IN_PUMP.load(Ordering::SeqCst)
                    ),
                );
            }
            if IN_PUMP.load(Ordering::SeqCst) {
                // Suppressed by CEF's own contract; `pump` re-posts on the
                // way out.
                REQUESTED_DURING_PUMP.store(true, Ordering::SeqCst);
                return;
            }
            if let Some(tx) = PUMP.get() {
                let _ = tx.send(delay_ms);
            }
        }
    }
}

wrap_app! {
    struct EaveCefApp {}

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(EaveBrowserProcessHandler::new())
        }

        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            let Some(registrar) = registrar else { return };
            registrar.add_custom_scheme(
                Some(&CefString::from("eave")),
                super::pdf::scheme_options(),
            );
        }

        /// One switch, and the docked DevTools does not connect without it.
        ///
        /// Chromium refuses a websocket to the debugging port that arrives
        /// with an `Origin` header, unless that origin is named here — the
        /// mitigation for a web page reaching the port via DNS rebinding.
        /// eave's frontend is a real page served from
        /// `http://127.0.0.1:<port>`, so it sends exactly such a header and
        /// is refused: the panel draws, then "Debugging connection was
        /// closed — WebSocket disconnected". Chromium says so on stderr,
        /// naming the flag; nothing surfaces in the UI.
        ///
        /// **The value is that ONE origin, never `*`.** `*` would admit any
        /// page in any browser on this machine that can guess the port.
        /// This admits the frontend eave itself serves, which is the only
        /// thing at that origin.
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // Browser process only — it is the one that hosts the endpoint.
            // Renderers and helpers get an empty process type too, so check
            // for emptiness rather than for a name.
            let is_browser = process_type
                .map(|t| t.to_string().is_empty())
                .unwrap_or(true);
            let Some(command_line) = command_line else { return };
            if !is_browser {
                return;
            }
            command_line.append_switch_with_value(
                Some(&CefString::from("remote-allow-origins")),
                Some(&CefString::from(
                    format!("http://127.0.0.1:{DEVTOOLS_PORT}").as_str(),
                )),
            );
            // ── The keychain prompt, and the switch that skips it ──
            //
            // Chromium encrypts cookies with a key it keeps in the login
            // keychain under "Chromium Safe Storage", and the keychain
            // item's ACL names the app by its code signature. An ad-hoc
            // signature is a per-build cdhash, so EVERY rebuilt bundle is a
            // new app to the keychain, and the first page load raises the
            // system's "eave wants to use your confidential information"
            // sheet. Until it is answered the network context has no key
            // and no navigation starts — the pane stays blank with the URL
            // in the address field (2026-09-03, measured: renderer up, no
            // request at the server, `process_requirement.cc` -67030 in
            // chrome_debug.log). Synthetic clicks do not reach the sheet.
            //
            // `EAVE_WEB_MOCK_KEYCHAIN=1` passes Chromium's own
            // `--use-mock-keychain` (what cefclient and Chromium's tests
            // use): a fixed key, cookies at rest are NOT protected by the
            // keychain. It exists for unattended walks on throwaway data
            // dirs, is off by default, and is not a product setting — a
            // signed release has one stable identity and is asked once.
            if std::env::var_os("EAVE_WEB_MOCK_KEYCHAIN").is_some() {
                command_line.append_switch(Some(&CefString::from("use-mock-keychain")));
            }
            // ── The Google sign-in crash, and what it actually was ──
            //
            // Loading any accounts.google.com `signin/challenge/*` page
            // took the whole app down with SIGSEGV, ~4 s after load, no
            // interaction — and crash-LOOPED, because the tab journal
            // restores the challenge page as the active tab. Four
            // byte-identical crashes on 2026-08-27; reproduced headlessly.
            //
            // The symbolicated frame (dSYM for this exact pin) is
            // `ReadAnythingSoftNavigationObserver::OnSoftNavigation` —
            // Chrome's READING MODE, watching page-load metrics for SOFT
            // navigations. Gaia's sign-in is an SPA: identifier →
            // challenge/pwd (or /pk) is a soft navigation, the observer
            // fires, and `tabs::TabInterface::GetFromContents` dies on a
            // WebContents that has no Chrome tab model — which is every
            // WebContents under alloy/OSR. Its own `if (!tab)` guard sits
            // one line too late. Gated on `kImmersiveReadAnything`,
            // ENABLED_BY_DEFAULT at this pin; disabled → the observer
            // returns before touching anything.
            //
            // Two wrong fixes are recorded here so nobody re-walks them:
            // WebAuthn (`--disable-features=WebAuthentication` did
            // nothing; the FIDO "Touch ID unavailable" stderr line was a
            // coincident symptom) and the client-certificate handler
            // (`on_select_client_certificate` never fired). The crash
            // reproduces with WebAuthn fully off, on the PASSWORD page.
            // See docs/reports/bookmark-peek.md.
            //
            // MERGE with CEF's own value — an `append_switch_with_value`
            // on a key CEF also sets REPLACES it (measured: eave's first
            // attempt wiped `AutofillActorMode,GlicActorUi,…` off the
            // helper command lines), and CEF's own disabled features are
            // load-bearing for other alloy holes.
            let disable_features = CefString::from("disable-features");
            let existing =
                CefString::from(&command_line.switch_value(Some(&disable_features)))
                    .to_string();
            let merged = if existing.is_empty() {
                "ImmersiveReadAnything".to_string()
            } else {
                format!("{existing},ImmersiveReadAnything")
            };
            command_line.append_switch_with_value(
                Some(&disable_features),
                Some(&CefString::from(merged.as_str())),
            );
            // WebAuthn's API is removed from the renderer separately
            // (`WebAuth` is a BLINK runtime feature, not a base::Feature).
            // Not the crash — but the platform authenticator cannot work
            // here: it needs the `.org.chromium.Chromium.webauthn`
            // keychain-access-group entitlement, which an unsigned bundle
            // cannot carry, so Chromium logs "Touch ID authenticator
            // unavailable" and the passkey prompt can only fail. With the
            // API absent, sites fall back to passwords, which work — a
            // present `PublicKeyCredential` that always fails is worse
            // than an absent one, because it is what sites branch on.
            //
            // **`EAVE_WEB_ENABLE_WEBAUTHN=1` puts the API back, for
            // measuring and nothing else.** The note this replaces said
            // "revisit when bundles are signed" and gave nobody a way to
            // revisit: answering it meant editing this line and building
            // 379 MB, so for a fortnight the passkey row read
            // "signing-gated" on an argument rather than a reading. The
            // switch is off by default, is never set by the product, and
            // exists so that the day there is a Developer ID certificate
            // the question takes one launch instead of one build. What it
            // cannot do is make passkeys work: CEF's public API has no
            // WebAuthn surface at all, so an embedder cannot hand
            // Chromium the keychain access group the mac authenticator is
            // configured from — signing is necessary and, on this
            // evidence, not sufficient. See the BR5 report.
            if std::env::var_os("EAVE_WEB_ENABLE_WEBAUTHN").is_none() {
                command_line.append_switch_with_value(
                    Some(&CefString::from("disable-blink-features")),
                    Some(&CefString::from("WebAuth")),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Start and stop
// ---------------------------------------------------------------------------

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether CEF is up.
pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// The one way to reach Chromium's global request context.
///
/// # This exists because remembering a guard is not a strategy
/// **Reaching a CEF global before `CefInitialize` is not "absent", it is
/// UNSAFE.** `request_context_get_global_context` dereferences a global
/// the framework has not built, so it is a SIGSEGV rather than a `None` —
/// signal 11, no assertion, no failing test name, the whole lib-test
/// binary gone. It has now bitten this codebase in **three** places:
///
/// 1. `dialogs::sync_to_chromium`, the first time the `--features web`
///    suite ran — reached through `permissions::forget_origin`.
/// 2. `permissions::forget_origin` → `cef_clear_permissions` again, in a
///    later slice, before that guard covered it.
/// 3. `browser_settings`, from a test that only wanted to read two fields
///    — a `CefString` is allocated by the framework too, so merely
///    *building* a `Settings` is enough.
///
/// Each was fixed by remembering to write `if !running() { return }` at
/// the call site, which is a strategy that works until somebody forgets.
/// So the check moves *inside*: this returns `None` before initialisation
/// and is the only place `request_context_get_global_context` is named.
///
/// It does not help with (3) — nothing can, short of not touching a
/// `CefString` from a test. That is why `USER_AGENT` is a constant.
pub(super) fn global_request_context() -> Option<RequestContext> {
    if !running() {
        return None;
    }
    request_context_get_global_context()
}

/// Load the Chromium framework, which is not linked — it is `dlopen`ed out
/// of the bundle at runtime.
///
/// Every CEF entry point is a null function pointer until this runs, so
/// skipping it does not fail, it **segfaults inside `cef::initialize`** with
/// a null-address `EXC_BAD_ACCESS` and no message. Costly to diagnose from
/// the symptom, trivial once named.
///
/// The loader unloads on `Drop`, and unloading Chromium out from under a
/// live browser is not a thing to do, so it is leaked deliberately: its
/// lifetime is the process's.
fn load_framework() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let loader = library_loader::LibraryLoader::new(&exe, false);
    if !loader.load() {
        eprintln!(
            "eave: no Chromium framework beside the binary — eave must run from \
             inside its .app (scripts/bundle-app.sh --features web assembles one)"
        );
        return false;
    }
    std::mem::forget(loader);
    // Pins the API version this build negotiated against. Must precede any
    // other CEF call.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    true
}

/// Start CEF in this (browser) process. Returns whether a page engine is
/// now running.
pub fn initialize(data_dir: &std::path::Path) -> bool {
    if DISABLED.load(Ordering::SeqCst) || !load_framework() {
        return false;
    }
    open_pump_channel();
    let args = args::Args::new();
    let mut app = EaveCefApp::new();

    // WHERE CHROMIUM KEEPS ITS STATE. Unset, CEF picks a process-global
    // temporary directory, warns about it on every single launch, and — the
    // part that actually matters — gives two eave instances the SAME one.
    // Chromium takes an exclusive lock on its cache root, so the second
    // instance does not get a degraded browser, it gets no browser at all.
    // eave already has a per-instance data dir (that is what `--data-dir`
    // and `EAVE_DATA_DIR` are for, and what the test harness leans on to run
    // instances side by side), so pointing CEF at a subdirectory of it makes
    // the browser follow the same rule as the rest of eave's state.
    //
    // `browser/chromium` rather than `browser/`, because `browser/` is
    // already where the tab and history journals live and Chromium treats
    // its cache root as territory it owns.
    let cache = data_dir.join("browser").join("chromium");
    let _ = std::fs::create_dir_all(&cache);
    let settings = browser_settings(&cache);
    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if !ok {
        eprintln!("eave: CefInitialize failed — continuing without a page engine");
    } else {
        super::pdf::register();
        super::tabs::restore_downloads(data_dir);
        super::cdm::arm();
    }
    // **The app object must outlive `initialize`.** It is what vends the
    // browser-process handler, and that handler is the only thing CEF calls
    // to ask for a pump. Letting the local drop at the end of this function
    // took the handler with it: CEF asked for exactly one pump — the one
    // raised during initialisation, while the object was still alive — and
    // then never asked again. Everything downstream looked like a pump bug
    // for an hour. Its lifetime is the process's, so it is leaked and says
    // so.
    std::mem::forget(app);
    RUNNING.store(ok, Ordering::SeqCst);
    ok
}

/// **The User-Agent decision (06-browser G6), as a constant.**
///
/// `None` means "leave Chromium's alone", and that is the decision.
///
/// # Why not just set it
/// - `user_agent_product` is the tempting field and it is a trap. CEF
///   documents it as "the product portion of the default User-Agent
///   string", which means `eave/0.1.0` **replaces** `Chrome/151.0.0.0`
///   rather than appending to it. A large amount of the web gates on that
///   token, so the field that reads as "identify ourselves politely" is
///   really "stop being Chrome".
/// - `user_agent` replaces the whole string, which hands eave ownership of
///   a hard-coded Chromium version number. A UA claiming `Chrome/151` on a
///   build linking Chromium 153 is worse than no decision, because it is a
///   decision that rots silently.
///
/// **Brave — the reference implementation sitting in this workspace —
/// does exactly this.** Its only User-Agent patch is
/// `chromium_src/components/embedder_support/user_agent_utils.cc`, and
/// that file rewrites the **Client Hints brand list** (hard-coding the
/// brand, forcing `major.0.0.0`, emptying `BuildModelInfo`) while leaving
/// the legacy `User-Agent` header exactly as Chromium built it. The
/// reasoning is fingerprinting: a browser with its own UA string is a
/// browser whose users can be picked out by it.
///
/// So the place to revisit this is Client Hints, not here. Changing either
/// constant fails `the_user_agent_decision_is_still_chromiums`, which is
/// the point — the test exists to make somebody read the paragraph above.
const USER_AGENT: Option<&str> = None;

/// The product token half of [`USER_AGENT`]. Same decision, same reason.
const USER_AGENT_PRODUCT: Option<&str> = None;

/// The DevTools protocol port, localhost-only (Chromium binds 127.0.0.1
/// for remote debugging unless told otherwise). 9223 rather than the
/// conventional 9222 so a developer's ordinary Chrome debugging session
/// on this machine cannot collide with eave's.
///
/// **Only reached when the operator asks for it** — see
/// [`devtools_port_enabled`].
pub(crate) const DEVTOOLS_PORT: u16 = 9223;

/// Whether this session was launched with the debugging port enabled.
///
/// # Why this is off by default
/// `remote_debugging_port` is a `CefSettings` field applied at
/// `CefInitialize`, so enabling it opens the endpoint **for the whole
/// session and every tab, from launch, whether or not a dock is ever
/// opened**. Inspect is not the exposure; running eave was. Chromium's
/// DevTools endpoint is unauthenticated to anything that can reach
/// loopback: `/json/list` hands out target ids and a target is full CDP
/// over every origin the browser holds — cookies, storage and script
/// execution included.
///
/// **This is not theoretical, and it was demonstrated here by accident.**
/// `tabs::show_devtools` carries the account: the lib-test binary, run
/// from a checkout, reached a *different* eave's live browser because that
/// process's debugging port answered a stranger on loopback. A test
/// process drove another instance's browser without asking anyone. That is
/// the whole threat model, performed in-house, before anyone was looking
/// for it.
///
/// No shipping Chromium browser has this on by default. Chrome and Arc
/// require `--remote-debugging-port`, and **Brave does not patch remote
/// debugging at all** (zero files in `brave-core` mention it, so it
/// inherits Chromium's default). Their own docked DevTools uses no socket
/// whatsoever — `DevToolsAgentHost::AttachClient`, an in-process binding.
/// Read once: a variable that changed mid-session would make two halves of
/// one run disagree about whether a port is open.
pub(crate) fn devtools_port_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        super::devtools_port_requested(std::env::var("EAVE_DEVTOOLS_PORT").ok().as_deref())
    })
}

/// The settings eave starts Chromium with.
///
/// # This function cannot be called from a test, and that is not a choice
/// Building a [`Settings`] populates `CefString` fields, and a `CefString`
/// is allocated by the Chromium framework. In a process that never ran
/// `CefInitialize` — which is every test in this crate, including the
/// `--features web` ones — that is **a SIGSEGV, not a `None`**: no
/// assertion, no failing test name, just signal 11 taking the whole lib
/// test binary with it. `sync_to_chromium` in `dialogs.rs` carries the
/// same warning for `request_context_get_global_context`; this is the
/// second place it bites, and the reason the G6 guard below asserts on the
/// CONSTANTS rather than on the struct they feed.
fn browser_settings(cache: &std::path::Path) -> Settings {
    let mut settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        // gpui owns [NSApp run]; CEF gets pumped instead of running its own.
        external_message_pump: 1,
        root_cache_path: cache.to_string_lossy().as_ref().into(),
        // The DevTools protocol endpoint, bound to localhost by Chromium,
        // and **off unless the operator asked for it**.
        //
        // It is what makes a DOCKED frontend possible at all: CEF 151's
        // ShowDevTools rejects windowless rendering outright, so the
        // bundled frontend is loaded into an eave-owned OSR browser and
        // connected to the tab's target over this port instead. See
        // `tabs::show_devtools`. That cost is real — with the port off the
        // dock cannot work, and `web::inspect_element` says so rather than
        // leaving a pane over a frontend that will never connect.
        //
        // Only 1024–65535 enables it; 0 is the default and is off. (The
        // ephemeral-port behaviour CEF documents for 0 applies to the
        // command-line switch, not to this field.)
        remote_debugging_port: if devtools_port_enabled() {
            DEVTOOLS_PORT as i32
        } else {
            0
        },
        ..Default::default()
    };
    if let Some(ua) = USER_AGENT {
        settings.user_agent = ua.into();
    }
    if let Some(product) = USER_AGENT_PRODUCT {
        settings.user_agent_product = product.into();
    }
    settings
}

/// Stop CEF.
///
/// Deliberately does nothing while a windowless browser is still live: the
/// spike found `shutdown()` blocks there, and a quit that hangs is worse
/// than a process that exits with Chromium still mapped. `tabs::close_all`
/// runs first; this is the second half.
pub fn shutdown() {
    stop_cef(false);
}

/// Close every browser and, when it is safe to, shut CEF down.
///
/// `from_pump` is the whole safety argument. `CefShutdown` is only called on
/// the pump's stack — no gpui borrow held, no `[NSApp terminate:]` above us,
/// no `CefDoMessageLoopWork` around us. Reached any other way (a system
/// logout, an `NSApplication terminate:` eave did not initiate) the browsers
/// are still closed and the handles still released, but CEF is left mapped
/// and the process exits anyway, because **a quit that hangs is worse than a
/// process that exits with Chromium still mapped** — this module's rule since
/// C1, and now with a measurement behind it.
fn stop_cef(from_pump: bool) {
    if !RUNNING.swap(false, Ordering::SeqCst) {
        return;
    }
    let trace = std::env::var("EAVE_WEB_TRACE").is_ok();
    if trace {
        eprintln!("[web] shutdown: closing browsers (from_pump={from_pump})");
    }
    super::tabs::close_all();

    // Closing is work CEF has to be PUMPED to do. `on_before_close` is what
    // removes a browser from the map, so an empty map is one readiness
    // signal — but not a sufficient one: the handler fires SYNCHRONOUSLY
    // inside `close_browser`, so the map is empty before Chromium has torn
    // anything down. Hence pumping for a settle period as well.
    // **Pump generously, then shut down behind a watchdog.**
    //
    // `CefShutdown` does not pump, and it deadlocks — not delays, deadlocks —
    // if browser teardown still has UI-thread work outstanding. Two things
    // were tried before this and both are recorded because they look right:
    //
    //   * A wall-clock settle. 300 ms hung 4 runs in 6; 1000 ms hung 4 in 6 at
    //     one pump per 10 ms and 0 in 6 at one per 1 ms. The clock was never
    //     the variable — the number of pumps was.
    //   * A floor of 1000 pumps on top of the drain below. That made it
    //     STRICTLY WORSE: `CefShutdown` returned in 5 of 10 runs on the drain
    //     alone and in **0 of ~90** with the floor, while adding a second to
    //     every quit. Pumping through a teardown appears to generate more of
    //     it. The floor was removed on that measurement; it is recorded here
    //     because "pump harder" is the obvious next idea and it is wrong.
    //
    // So: pump until Chromium stops asking, capped by a deadline, and let the
    // watchdog below bound what is left. `CefShutdown` still fails to return
    // about half the time — that is a real, open defect, not a solved one.
    let mut quiet = 0u32;
    let mut pumps = 0u32;
    let mut waited = 0;
    while waited < SHUTDOWN_WAIT_MS && quiet < SHUTDOWN_QUIET_ROUNDS {
        let before = SCHEDULES.load(Ordering::Relaxed);
        pump_cef();
        pumps += 1;
        if SCHEDULES.load(Ordering::Relaxed) == before && !super::tabs::any_browser() {
            quiet += 1;
        } else {
            quiet = 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(SHUTDOWN_STEP_MS));
        waited += SHUTDOWN_STEP_MS;
    }
    if trace {
        eprintln!("[web] shutdown: {pumps} pumps in {waited} ms, quiet={quiet}");
        // The numbers the capped lines above are a prefix of. These are what
        // "did the pump run?" is actually answered from.
        eprintln!(
            "[web] totals: {} pumps, {} schedules, {} frames delivered",
            PUMPS.load(Ordering::Relaxed),
            SCHEDULES.load(Ordering::Relaxed),
            super::osr::frames_delivered(),
        );
    }
    super::tabs::forget_all();

    if !from_pump {
        eprintln!(
            "eave: exiting without CefShutdown — the quit did not come through \
             the pump, and CefShutdown blocks if called from inside terminate:"
        );
        return;
    }
    if trace {
        eprintln!("[web] shutdown: calling CefShutdown");
    }
    // **A watchdog, because `CefShutdown` cannot be time-bounded from the
    // thread that calls it.** The pumping above makes a deadlock unlikely; it
    // cannot make it impossible, and this module's standing rule is that a
    // quit which hangs is worse than a process that exits with Chromium still
    // mapped. So the deadline is enforced from outside: `_exit` rather than
    // `exit`, because we are racing a thread that holds Chromium's locks and
    // running atexit handlers against it is how a clean exit becomes a crash.
    let returned = std::sync::Arc::new(AtomicBool::new(false));
    {
        let returned = returned.clone();
        std::thread::spawn(move || {
            let mut left = SHUTDOWN_DEADLINE_MS;
            while left > 0 {
                if returned.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                left = left.saturating_sub(20);
            }
            eprintln!(
                "eave: CefShutdown did not return in {SHUTDOWN_DEADLINE_MS} ms — \
                 exiting with Chromium still mapped"
            );
            unsafe { libc::_exit(0) };
        });
    }
    cef::shutdown();
    returned.store(true, Ordering::SeqCst);
    if trace {
        eprintln!("[web] CefShutdown returned — page engine is down");
    }
}

/// Ask eave to quit, the orderly way: the page engine is stopped on the
/// pump's stack and only then does gpui terminate.
///
/// Returns whether it took ownership of the quit. `false` means there is no
/// page engine running and the caller should just quit normally.
pub fn request_quit() -> bool {
    if !running() {
        return false;
    }
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
    request_pump();
    true
}

/// How long [`shutdown`] pumps waiting for the browsers to report closed
/// before giving up and exiting without `CefShutdown`.
const SHUTDOWN_WAIT_MS: u64 = 3000;
/// How long each pump-and-check step waits. One millisecond: the point is to
/// pump often, not to wait.
const SHUTDOWN_STEP_MS: u64 = 1;
/// How many consecutive pumps must raise no schedule request before the
/// queue counts as drained.
const SHUTDOWN_QUIET_ROUNDS: u32 = 50;
/// How long `CefShutdown` gets before the watchdog exits the process anyway.
const SHUTDOWN_DEADLINE_MS: u64 = 1500;

#[cfg(test)]
mod tests {
    use super::*;

    /// **G6, as a guard rather than a comment.**
    ///
    /// Both User-Agent knobs must stay `None`. Filling either is the
    /// change that looks like an improvement and is not — read
    /// [`USER_AGENT`] before making this test pass.
    ///
    /// Asserted on the constants and **not** on `browser_settings`,
    /// because calling that from a test segfaults: see its doc comment.
    #[test]
    fn the_user_agent_decision_is_still_chromiums() {
        assert!(
            USER_AGENT.is_none(),
            "eave took ownership of the whole User-Agent string, and now owns \
             a Chromium version number that nothing updates"
        );
        assert!(
            USER_AGENT_PRODUCT.is_none(),
            "user_agent_product REPLACES the Chrome/<version> token rather than \
             appending to it — see the comment on USER_AGENT"
        );
    }
}
