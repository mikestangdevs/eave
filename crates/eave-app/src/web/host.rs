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

/// Mark the page as busy, so the pump stays fast for a moment.
pub fn note_activity() {
    ACTIVE_UNTIL.store(now_ms() + BUSY_TAIL_MS, Ordering::Relaxed);
}

fn now_ms() -> u64 {
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
/// The pump interval while a page is live but quiet. A page that is merely
/// open still needs its timers and its network callbacks run, so this cannot
/// be zero — but it is 12× cheaper than the busy rate, and NOTHING repaints
/// on it: `pump` only refreshes the window when a frame actually arrived.
const QUIET_MS: u64 = 100;

/// Run CEF's pending work on gpui's foreground executor, forever.
///
/// # Why there is a heartbeat at all
/// The intent was to replace the spike's fixed 4 ms timer with pure
/// `OnScheduleMessagePumpWork`, and that is wired and honoured. It is not
/// sufficient: **CEF calls it exactly once, ever.** Measured, not inferred —
/// with every schedule and every pump logged, a full run from launch to a
/// loaded page produces `schedule #0` and nothing more, and a host that pumps
/// only on request stops after two pumps with the browser created and its
/// page never loaded.
///
/// So requests are honoured *and* a heartbeat runs underneath them, which is
/// what cefclient's own external pump does for the same reason. Two things
/// keep this from being the 4 ms timer wearing a hat:
///
///   - it only runs while a browser is live, and parks completely otherwise;
///   - it decays to [`QUIET_MS`] when nothing is happening.
///
/// And the property the spike actually measured is untouched: the pump does
/// not repaint. [`pump`] refreshes the window only when the frame counter
/// moved, so an idle page still costs zero redraws however often it is
/// pumped.
pub fn start_pump(cx: &mut gpui::App) {
    let Some(mut rx) = PUMP_RX.lock().unwrap().take() else {
        return;
    };
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        // One unconditional pump before waiting on anything: whatever CEF
        // asked for during initialisation is satisfied here.
        cx.update(pump);
        loop {
            if super::tabs::any_browser() {
                let quiet = now_ms() >= ACTIVE_UNTIL.load(Ordering::Relaxed);
                let ms = if quiet { QUIET_MS } else { BUSY_MS };
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(ms))
                    .await;
                while rx.try_recv().is_ok() {}
            } else {
                // Nothing to pump for. Park until something asks — a browser
                // being created, or CEF's one request.
                if rx.recv().await.is_none() {
                    return;
                }
                while rx.try_recv().is_ok() {}
            }
            cx.update(pump);
        }
    })
    .detach();
}

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

/// One pump. Runs CEF's pending work, then — and only then — asks gpui to
/// redraw if a frame actually arrived.
///
/// Takes `&mut App` rather than reaching for an `AsyncApp` it stored
/// earlier, and that is not a tidiness point: this runs INSIDE a
/// `cx.update`, which already holds gpui's app borrow, so calling
/// `AsyncApp::refresh` here takes the same `RefCell` twice and aborts the
/// process on `already borrowed`. The handle we already have is the only
/// one that is safe to use.
fn pump(app: &mut gpui::App) {
    if std::env::var("EAVE_WEB_TRACE").is_ok() {
        let n = PUMPS.fetch_add(1, Ordering::Relaxed);
        if n < 25 {
            eprintln!("[web] pump #{n}");
        }
    }
    IN_PUMP.store(true, Ordering::SeqCst);
    REQUESTED_DURING_PUMP.store(false, Ordering::SeqCst);
    do_message_loop_work();
    IN_PUMP.store(false, Ordering::SeqCst);
    // Anything Chromium queued while we were inside raised no request, so
    // ask for one more pass. This is what keeps the chain alive; without it
    // the loop parks forever with work outstanding.
    if REQUESTED_DURING_PUMP.swap(false, Ordering::SeqCst) {
        if let Some(tx) = PUMP.get() {
            let _ = tx.send(0);
        }
    }
    // **Render on delivered frames, never on vsync.** The spike measured an
    // idle page at 1 frame in 9 seconds; a tab that repainted the window at
    // 60 Hz regardless would throw that away and burn a GPU on a page that
    // is not moving. `refresh` is called only when the frame counter moved.
    let seen = super::osr::frames_delivered();
    if seen != LAST_REFRESHED.load(Ordering::Relaxed) {
        LAST_REFRESHED.store(seen, Ordering::Relaxed);
        // A page that is painting is a page that is doing something.
        note_activity();
        app.refresh_windows();
    }
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
            if std::env::var("EAVE_WEB_TRACE").is_ok() {
                let n = SCHEDULES.fetch_add(1, Ordering::Relaxed);
                if n < 25 {
                    eprintln!("[web] schedule #{n} delay={delay_ms} in_pump={}",
                        IN_PUMP.load(Ordering::SeqCst));
                }
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
             inside its .app (scripts/package-web.sh assembles one)"
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
pub fn initialize() -> bool {
    if DISABLED.load(Ordering::SeqCst) || !load_framework() {
        return false;
    }
    open_pump_channel();
    let args = args::Args::new();
    let mut app = EaveCefApp::new();
    let settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        // gpui owns [NSApp run]; CEF gets pumped instead of running its own.
        external_message_pump: 1,
        ..Default::default()
    };
    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if !ok {
        eprintln!("eave: CefInitialize failed — continuing without a page engine");
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

/// Stop CEF.
///
/// Deliberately does nothing while a windowless browser is still live: the
/// spike found `shutdown()` blocks there, and a quit that hangs is worse
/// than a process that exits with Chromium still mapped. `tabs::close_all`
/// runs first; this is the second half.
pub fn shutdown() {
    if RUNNING.swap(false, Ordering::SeqCst) {
        super::tabs::close_all();
        cef::shutdown();
    }
}
