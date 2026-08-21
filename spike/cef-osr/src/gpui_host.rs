//! CEF inside a GPUI process — the thing the previous CEF attempt stopped on.
//!
//! That attempt recorded the blocker as "`GPUIApplication` does not conform to
//! `CefAppProtocol`". It is a real requirement — Chromium's macOS message pump
//! reaches for `-[NSApp isHandlingSendEvent]` — but it is not a wall. The
//! protocol is two methods and a marker, `GPUIApplication` is registered by a
//! `#[ctor]` at load time, and `+[NSApplication sharedApplication]` builds its
//! instance from the *receiver's* class. So: subclass gpui's class at runtime,
//! add the two methods, and instantiate `NSApp` from the subclass before gpui
//! asks for it. gpui then finds an `NSApp` that is still a `GPUIApplication`
//! (so its `platform` ivar is there) and Chromium finds one that answers the
//! protocol.
//!
//! The second half is the run loop. CEF's own `CefRunMessageLoop` calls
//! `[NSApp run]`, and so does gpui — only one of them can. `external_message
//! _pump` hands that back to the host: CEF asks to be pumped, we pump it from
//! the main thread, and gpui keeps the run loop.

use cef::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{class, msg_send, sel};

use core_video::pixel_buffer::CVPixelBuffer;
use gpui::{
    div, prelude::*, px, size, App, Bounds, Context, Corners, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Window, WindowBounds, WindowOptions,
};

/// `EAVE_CEF_GPUI=1` runs the in-process arm instead of the standalone probe.
pub fn gpui_mode() -> bool {
    matches!(std::env::var("EAVE_CEF_GPUI"), Ok(v) if v != "0" && !v.is_empty())
}

// ---------------------------------------------------------------------------
// 1. NSApp that both gpui and Chromium accept
// ---------------------------------------------------------------------------

static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);

extern "C" fn is_handling_send_event(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::SeqCst))
}

extern "C" fn set_handling_send_event(_this: &AnyObject, _sel: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.store(handling.as_bool(), Ordering::SeqCst);
}

extern "C" fn send_event(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
    // Chromium reads this flag to know it is inside -[NSApplication sendEvent:]
    // and must not re-enter its own nested loops. Restore rather than clear:
    // sendEvent: nests.
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
/// make `NSApp` an instance of it. Must run before gpui asks for `NSApp`.
///
/// Built with the raw runtime rather than objc2's `ClassBuilder`: two objc2
/// majors are in this graph at once (gpui carries 0.5, cef carries 0.6) and
/// the typed builder's `MethodImplementation` bound will not resolve across
/// them. `objc_allocateClassPair` has no opinion about any of that.
fn install_cef_capable_nsapp() -> Result<(), String> {
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
            is_handling_send_event as *const std::ffi::c_void,
            c"c@:".as_ptr(),
        );
        class_addMethod(
            cls,
            sel!(setHandlingSendEvent:),
            set_handling_send_event as *const std::ffi::c_void,
            c"v@:c".as_ptr(),
        );
        class_addMethod(
            cls,
            sel!(sendEvent:),
            send_event as *const std::ffi::c_void,
            c"v@:@".as_ptr(),
        );

        // The protocol itself. Chromium's DCHECK on `conformsToProtocol:` is
        // compiled out of a release framework, but claiming conformance is
        // cheap and makes the class honest for anything that does check.
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

        // `+sharedApplication` allocates from the receiver's class when NSApp
        // is still nil — so asking the subclass first decides NSApp's class.
        let app: *mut AnyObject = msg_send![cls as *mut AnyObject, sharedApplication];
        if app.is_null() {
            return Err("sharedApplication returned nil".into());
        }
        let is_nsapp: Bool = msg_send![app, isKindOfClass: class!(NSApplication)];
        let gpui_kind: Bool = msg_send![app, isKindOfClass: gpui_class];
        let responds: Bool = msg_send![app, respondsToSelector: sel!(isHandlingSendEvent)];
        let conforms: Bool = {
            let proto = objc_getProtocol(c"CefAppProtocol".as_ptr()) as *mut AnyObject;
            msg_send![app, conformsToProtocol: proto]
        };
        println!(
            "[gpui-host] NSApp: NSApplication={} GPUIApplication={} responds to \
             isHandlingSendEvent={} conformsToProtocol(CefAppProtocol)={}",
            is_nsapp.as_bool(),
            gpui_kind.as_bool(),
            responds.as_bool(),
            conforms.as_bool()
        );
        if !gpui_kind.as_bool() || !responds.as_bool() {
            return Err("NSApp does not satisfy both gpui and CEF".into());
        }
    }
    Ok(())
}

unsafe extern "C" {
    fn objc_allocateClassPair(
        superclass: *mut std::ffi::c_void,
        name: *const std::os::raw::c_char,
        extra_bytes: usize,
    ) -> *mut std::ffi::c_void;
    fn objc_registerClassPair(cls: *mut std::ffi::c_void);
    fn class_addMethod(
        cls: *mut std::ffi::c_void,
        name: Sel,
        imp: *const std::ffi::c_void,
        types: *const std::os::raw::c_char,
    ) -> Bool;
    fn objc_getProtocol(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn objc_allocateProtocol(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn objc_registerProtocol(proto: *mut std::ffi::c_void);
    fn class_addProtocol(cls: *mut std::ffi::c_void, proto: *mut std::ffi::c_void) -> Bool;
}

// ---------------------------------------------------------------------------
// 2. The page, as a scene primitive
// ---------------------------------------------------------------------------

thread_local! {
    /// The latest completed frame, as an IOSurface-backed BGRA CVPixelBuffer
    /// that WE own — CEF's own surface may not outlive its callback.
    pub static PAGE: RefCell<Option<CVPixelBuffer>> = const { RefCell::new(None) };
}

pub static FRAMES_IN_GPUI: AtomicU64 = AtomicU64::new(0);

pub fn publish(buffer: CVPixelBuffer) {
    PAGE.with(|p| *p.borrow_mut() = Some(buffer));
    FRAMES_IN_GPUI.fetch_add(1, Ordering::Relaxed);
}

struct PageSurface;

impl IntoElement for PageSurface {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PageSurface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        PAGE.with(|p| {
            if let Some(buffer) = p.borrow().as_ref() {
                window.paint_surface(bounds, buffer.clone());
            }
        });
    }
}

struct Shell;

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Repaint every frame: the page is being pushed in from CEF, and gpui
        // has no idea anything changed.
        cx.notify();

        let chrome = |title: &'static str| {
            crate::gpui_host::Frost {
                radius: px(14.),
                child: div()
                    .w(px(320.))
                    .h(px(200.))
                    .bg(gpui::hsla(0., 0., 0., 0.25))
                    .rounded(px(14.))
                    .border_1()
                    .border_color(gpui::hsla(0., 0., 1., 0.14))
                    .p(px(16.))
                    .text_color(gpui::hsla(0., 0., 1., 0.92))
                    .child(title)
                    .child(
                        div()
                            .mt(px(8.))
                            .text_color(gpui::hsla(0., 0., 1., 0.65))
                            .child(format!(
                                "{} frames from Chromium",
                                FRAMES_IN_GPUI.load(Ordering::Relaxed)
                            )),
                    )
                    .into_any_element(),
            }
            .into_any_element()
        };

        div()
            .size_full()
            .relative()
            .bg(gpui::hsla(0., 0., 0.06, 1.0))
            .child(div().absolute().inset_0().child(PageSurface))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(chrome("Chrome over a live Chromium page")),
            )
    }
}

pub struct Frost {
    pub child: gpui::AnyElement,
    pub radius: Pixels,
}

impl IntoElement for Frost {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Frost {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_layer(bounds, |window| {
            window.paint_backdrop_blur(bounds, Corners::all(self.radius), px(18.));
            self.child.paint(window, cx);
        });
    }
}

// ---------------------------------------------------------------------------
// 3. Bring-up
// ---------------------------------------------------------------------------

pub fn run(main_args: &MainArgs, cmd_line: &CommandLine, sandbox_info: *mut u8) {
    let switch = CefString::from("type");
    let is_browser_process = cmd_line.has_switch(Some(&switch)) != 1;
    let ret = execute_process(Some(main_args), None, sandbox_info);
    if !is_browser_process {
        assert!(ret >= 0, "cannot execute non-browser process");
        return;
    }
    assert_eq!(ret, -1, "cannot execute browser process");

    if let Err(e) = install_cef_capable_nsapp() {
        eprintln!("[gpui-host] FAILED: {e}");
        std::process::exit(2);
    }

    let mut app = crate::shared::simple_app::SimpleApp::new();
    let settings = Settings {
        no_sandbox: !cfg!(feature = "sandbox") as _,
        windowless_rendering_enabled: 1,
        // gpui owns [NSApp run]; CEF gets pumped instead of running its own.
        external_message_pump: 1,
        ..Default::default()
    };
    assert_eq!(
        initialize(Some(main_args), Some(&settings), Some(&mut app), sandbox_info),
        1,
        "CefInitialize failed inside a gpui process"
    );
    println!("[gpui-host] CefInitialize returned 1 inside a gpui process");

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Shell),
        )
        .unwrap();
        cx.activate(true);

        // Pump CEF from gpui's own loop. A timer rather than
        // OnScheduleMessagePumpWork: for a probe, "is it alive" beats "is it
        // efficient", and the cost of the polite version is a note, not a risk.
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(4))
                    .await;
                let _ = cx.update(|_| do_message_loop_work());
            }
        })
        .detach();
    });

    shutdown();
}
