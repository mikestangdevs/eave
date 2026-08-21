//! The fallback arm: CEF windowed, as a child `NSView` of the gpui window.
//!
//! The question is narrow. The WKWebView spike's punchout mechanism assumes
//! the page's hosting view can be made a SIBLING of gpui's Metal view, one
//! slot below it, with a runtime `hitTest:` on gpui's view returning `nil`
//! over the page's rect so clicks fall through. Does that survive CEF, whose
//! content is composited by a different process and arrives as a remote
//! `CALayer` rather than as pixels the hosting view drew?
//!
//! What is checked here, in order:
//!   1. CEF accepts gpui's Metal view as `parent_view` and creates a browser.
//!   2. The view CEF hands back (`GetWindowHandle`) is reparentable — we can
//!      lift it out and reinsert it below gpui's view with
//!      `addSubview:positioned:NSWindowBelow`.
//!   3. After the reorder the view hierarchy is what punchout requires:
//!      page and gpui view are siblings of the same content view, page below.
//!   4. gpui's view class accepts a runtime `hitTest:`, so clicks over the
//!      page's rect can be handed to it.
//!
//! The layer-tree question is settled by (2) and (3) together: if CEF's view
//! survives being removed and reinserted at a new z-position and keeps
//! painting, its remote layer is an ordinary sublayer and AppKit's ordering
//! rules apply to it — which is the whole basis of the mechanism.

use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::Encode as _;
use objc2::{msg_send, sel};

const NS_WINDOW_BELOW: isize = -1;

pub fn enabled() -> bool {
    matches!(std::env::var("EAVE_CEF_NATIVE"), Ok(v) if v != "0" && !v.is_empty())
}

/// Describe a view: class name, superview class, and frame.
unsafe fn describe(view: *mut AnyObject) -> String {
    if view.is_null() {
        return "nil".into();
    }
    let cls: *const AnyClass = msg_send![view, class];
    let name = (*cls).name().to_string_lossy().into_owned();
    let superview: *mut AnyObject = msg_send![view, superview];
    let super_name = if superview.is_null() {
        "nil".to_string()
    } else {
        let scls: *const AnyClass = msg_send![superview, class];
        (*scls).name().to_string_lossy().into_owned()
    };
    format!("{name} (superview: {super_name})")
}

static mut GPUI_VIEW: *mut AnyObject = std::ptr::null_mut();

/// The hole, in gpui-view coordinates, published by whatever element owns
/// the web tab. Static because `hitTest:` has nowhere else to read from.
static HOLE: std::sync::Mutex<Option<(f64, f64, f64, f64)>> = std::sync::Mutex::new(None);

pub fn set_hole(x: f64, y: f64, w: f64, h: f64) {
    *HOLE.lock().unwrap() = Some((x, y, w, h));
}

extern "C" fn hit_test(this: *mut AnyObject, _sel: Sel, point: CgPoint) -> *mut AnyObject {
    unsafe {
        let superview: *mut AnyObject = msg_send![this, superview];
        let local: CgPoint = msg_send![this, convertPoint: point, fromView: superview];
        let bounds: CgRect = msg_send![this, bounds];
        let flipped: Bool = msg_send![this, isFlipped];
        let y = if flipped.as_bool() {
            local.y
        } else {
            bounds.size.height - local.y
        };
        if let Some((hx, hy, hw, hh)) = *HOLE.lock().unwrap() {
            if local.x >= hx && local.x < hx + hw && y >= hy && y < hy + hh {
                // Not ours: AppKit continues to the next sibling, which is
                // the page.
                return std::ptr::null_mut();
            }
        }
        let cls: *const AnyClass = msg_send![this, class];
        let superclass = (*cls).superclass().unwrap();
        let imp = superclass.instance_method(sel!(hitTest:)).unwrap().implementation();
        let imp: extern "C" fn(*mut AnyObject, Sel, CgPoint) -> *mut AnyObject =
            std::mem::transmute(imp);
        imp(this, sel!(hitTest:), point)
    }
}

// objc2 needs an Encode impl to type-check a message send, and these are the
// standard CoreGraphics layouts.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CgPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CgSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CgRect {
    pub origin: CgPoint,
    pub size: CgSize,
}

unsafe impl objc2::Encode for CgPoint {
    const ENCODING: objc2::Encoding =
        objc2::Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl objc2::Encode for CgSize {
    const ENCODING: objc2::Encoding =
        objc2::Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl objc2::Encode for CgRect {
    const ENCODING: objc2::Encoding =
        objc2::Encoding::Struct("CGRect", &[CgPoint::ENCODING, CgSize::ENCODING]);
}

unsafe extern "C" {
    fn class_addMethod(
        cls: *mut std::ffi::c_void,
        name: Sel,
        imp: *const std::ffi::c_void,
        types: *const std::os::raw::c_char,
    ) -> Bool;
}

/// Add `hitTest:` to gpui's view class at runtime. gpui's Metal view does not
/// define one, so this adds rather than swizzles and the super call reaches
/// `NSView`'s.
unsafe fn install_hit_test(view: *mut AnyObject) -> bool {
    let cls: *const AnyClass = msg_send![view, class];
    let already: Bool = msg_send![view, respondsToSelector: sel!(hitTest:)];
    let _ = already; // NSView always responds; what matters is OUR imp landing.
    class_addMethod(
        cls as *mut std::ffi::c_void,
        sel!(hitTest:),
        hit_test as *const std::ffi::c_void,
        c"@@:{CGPoint=dd}".as_ptr(),
    )
    .as_bool()
}

/// Run the four checks and print a verdict.
pub fn probe(cef_view: *mut AnyObject, gpui_view: *mut AnyObject) {
    unsafe {
        GPUI_VIEW = gpui_view;
        println!("[native] CEF view   : {}", describe(cef_view));
        println!("[native] gpui view  : {}", describe(gpui_view));

        let content: *mut AnyObject = msg_send![gpui_view, superview];
        if content.is_null() {
            println!("[native] VERDICT: gpui's view has no superview — punchout needs one");
            return;
        }
        println!("[native] content    : {}", describe(content));

        // 2 + 3: lift the page out and reinsert it below gpui's view.
        let _: () = msg_send![cef_view, removeFromSuperview];
        let _: () = msg_send![content, addSubview: cef_view,
            positioned: NS_WINDOW_BELOW, relativeTo: gpui_view];

        let subviews: *mut AnyObject = msg_send![content, subviews];
        let count: usize = msg_send![subviews, count];
        let mut order = Vec::new();
        for i in 0..count {
            let v: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
            let cls: *const AnyClass = msg_send![v, class];
            order.push((*cls).name().to_string_lossy().into_owned());
        }
        println!("[native] sibling order (back to front): {order:?}");

        let cef_super: *mut AnyObject = msg_send![cef_view, superview];
        let siblings = cef_super == content;
        let page_index = order.len().checked_sub(1).map(|_| {
            order
                .iter()
                .position(|n| n.contains("Wry") || n.contains("Chrome") || n.contains("CEF") || n.contains("BrowserView") || n.contains("Content"))
        });
        let _ = page_index;

        // 4: the hit-test hook.
        let hooked = install_hit_test(gpui_view);

        println!(
            "[native] VERDICT: reparented={} siblings-of-content={} hitTest-installed={}",
            !cef_super.is_null(),
            siblings,
            hooked
        );
    }
}


// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// Create a WINDOWED CEF browser parented into gpui's own `NSView`, then run
/// the punchout checks against it.
pub fn create_windowed_browser(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        println!("[native] gpui Window has no raw window handle");
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        println!("[native] raw window handle is not AppKit");
        return;
    };
    let gpui_view = appkit.ns_view.as_ptr() as *mut AnyObject;

    let url = std::env::var("EAVE_CEF_NATIVE_URL")
        .unwrap_or_else(|_| "https://example.com/".to_string());
    let url = cef::CefString::from(url.as_str());

    let window_info = cef::WindowInfo::default().set_as_child(
        gpui_view as *mut std::ffi::c_void,
        &cef::Rect { x: 0, y: 0, width: 900, height: 600 },
    );
    let settings = cef::BrowserSettings::default();
    let mut client = crate::shared::simple_handler::SimpleHandlerClient::new(
        crate::shared::simple_handler::SimpleHandler::new(true),
    );

    let browser = cef::browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&url),
        Some(&settings),
        None,
        None,
    );
    let Some(browser) = browser else {
        println!("[native] VERDICT: CEF refused to create a windowed browser in gpui's view");
        return;
    };
    use cef::{ImplBrowser, ImplBrowserHost};
    let Some(host) = browser.host() else { return };
    let cef_view = host.window_handle() as *mut AnyObject;
    set_hole(0.0, 0.0, 900.0, 600.0);
    probe(cef_view, gpui_view);
}
