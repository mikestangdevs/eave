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

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_after_f(
        when: u64,
        queue: *mut c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_time(base: u64, delta: i64) -> u64;
}

const DISPATCH_TIME_NOW: u64 = 0;

fn main_queue() -> *mut c_void {
    (&raw const _dispatch_main_q) as *mut c_void
}

/// One pump. Runs CEF's pending work, then — and only then — asks gpui to
/// redraw if a frame actually arrived.
extern "C" fn pump(_: *mut c_void) {
    do_message_loop_work();
    // **Render on delivered frames, never on vsync.** The spike measured an
    // idle page at 1 frame in 9 seconds; a tab that repainted the window at
    // 60 Hz regardless would throw that away and burn a GPU for a page that
    // is not moving. `refresh` is called only when the frame counter moved.
    let seen = super::osr::frames_delivered();
    if seen != LAST_REFRESHED.load(Ordering::Relaxed) {
        LAST_REFRESHED.store(seen, Ordering::Relaxed);
        REFRESH.with(|r| {
            if let Some(cx) = r.borrow().as_ref() {
                cx.refresh();
            }
        });
    }
}

static LAST_REFRESHED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The handle a CEF callback uses to wake gpui. Main thread only, which
    /// is where every one of these callbacks lands under an external pump.
    static REFRESH: RefCell<Option<gpui::AsyncApp>> = const { RefCell::new(None) };
}

/// Hand the page engine a way to wake the window. Called once, from the
/// shell, after the window exists.
pub fn set_repaint_handle(cx: gpui::AsyncApp) {
    REFRESH.with(|r| *r.borrow_mut() = Some(cx));
}

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
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            let delay_ns = delay_ms.max(0).saturating_mul(1_000_000);
            unsafe {
                dispatch_after_f(
                    dispatch_time(DISPATCH_TIME_NOW, delay_ns),
                    main_queue(),
                    std::ptr::null_mut(),
                    pump,
                );
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

/// Start CEF in this (browser) process. Returns whether a page engine is
/// now running.
pub fn initialize() -> bool {
    if DISABLED.load(Ordering::SeqCst) {
        return false;
    }
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
