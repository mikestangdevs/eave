//! macOS Notification Center — the OS banner, for the whole app.
//!
//! **This was `web/notify.rs`'s private `macos` module and it is ungated
//! now.** It moved because a page calling `Notification()` was never the
//! only thing eave has to tell an operator who is looking somewhere else:
//! a session finishing is the other one, and it is the one that matters
//! when the window is behind your editor. Nothing here knows what a page
//! is; it takes a subtitle, a title, a body and an id.
//!
//! It is gated on `target_os` ALONE. The `web` feature exists to keep a
//! 343 MB Chromium download out of `cargo test`'s path (see `lib.rs`);
//! the objc2 bindings are small, pure Rust and download nothing, so
//! gating notifications behind Chromium was an accident of where the code
//! was first written rather than a decision.
//!
//! Tahoe 26 dropped `NSUserNotification` for real: `deliverNotification:`
//! returns, the class still exists, and usernoted never records the app
//! (measured 2026-08-29 — zero `ncprefs` hits, no banner, no list row).
//! The live API is `UNUserNotificationCenter`. That one needs three things
//! this module cannot fake:
//!
//! 1. A **bundle signature** with a bound Info.plist (`codesign -s -` is
//!    enough; the linker's `adhoc,linker-signed` identifier is a random
//!    cargo hash and `Info.plist=not bound`). Developer ID is not required.
//! 2. Launch through LaunchServices (`open` / Dock / double-click), from a
//!    location macOS treats as an app — `~/Applications` works, `/tmp`
//!    returns `UNErrorDomain` code 1 *Notifications are not allowed for
//!    this application* without a prompt.
//! 3. A delegate that answers `willPresentNotification:` with Banner|List,
//!    or the banner is swallowed while eave is frontmost.
//!
//! Authorization is asked for on the first post and **never waited for**:
//! every post goes to `addNotificationRequest` at once. Measured
//! 2026-08-29: Tahoe answers the ask with `UNErrorDomain` code 1 /
//! `authorizationStatus` Denied for an unsigned bundle even with Allow on
//! in System Settings, and the add still completes with `err=nil`; a queue
//! held until `granted=true` would swallow every post on such a machine.
//! (GPUI posts the same way.)
//!
//! **A failure here is never allowed to withhold an in-app signal.** The
//! page toast and the rail's own `finished` mark are the honest displays;
//! this is additional.

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::{Mutex, OnceLock};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, ProtocolObject};
    use objc2::{define_class, msg_send, AnyThread};
    use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationSound,
        UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    /// One notification's payload, as it goes to Notification Center.
    struct Payload {
        origin: String,
        title: String,
        body: String,
        id: String,
    }

    /// Where the one authorization ask stands. `Granted` and `Denied` are
    /// both terminal for the process — the difference is recorded for the
    /// trace, and because a later reader will want to know which it was.
    enum Auth {
        Idle,
        Asking,
        Granted,
        Denied,
    }

    /// `Retained<UNUserNotificationCenter>` is neither Send nor Sync in objc2
    /// 0.6. The objects are process-singletons used from the main thread and
    /// from Apple's authorization callback; the wrapper is the same leak the
    /// NSUserNotification delegate used.
    struct SendCenter(Retained<UNUserNotificationCenter>);
    unsafe impl Send for SendCenter {}
    unsafe impl Sync for SendCenter {}

    struct SendDelegate(#[allow(dead_code)] Retained<EaveNcDelegate>);
    unsafe impl Send for SendDelegate {}
    unsafe impl Sync for SendDelegate {}

    struct State {
        /// Strong ref: the center's `delegate` property is weak.
        _delegate: SendDelegate,
        center: SendCenter,
        auth: Mutex<Auth>,
    }

    fn traced() -> bool {
        std::env::var_os("EAVE_WEB_TRACE").is_some() || std::env::var_os("EAVE_TRACE").is_some()
    }

    fn state() -> Option<&'static State> {
        static STATE: OnceLock<State> = OnceLock::new();
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return None;
        }
        Some(STATE.get_or_init(|| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = EaveNcDelegate::new();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            State {
                _delegate: SendDelegate(delegate),
                center: SendCenter(center),
                auth: Mutex::new(Auth::Idle),
            }
        }))
    }

    pub(super) fn post(origin: &str, title: &str, body: &str, id: String) {
        let Some(bundle) = NSBundle::mainBundle().bundleIdentifier() else {
            if traced() {
                eprintln!("[nc] skip: no bundle id");
            }
            return;
        };
        if traced() {
            eprintln!("[nc] post bundle={bundle} id={id}");
        }
        let Some(state) = state() else {
            return;
        };
        let payload = Payload {
            origin: origin.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            id,
        };
        // Ask once so a notDetermined app gets the system prompt. Do not
        // wait for the answer — see the module doc for the measurement
        // that rules a queue out.
        {
            let mut auth = state.auth.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*auth, Auth::Idle) {
                *auth = Auth::Asking;
                drop(auth);
                request_authorization(state);
            }
        }
        deliver(&state.center.0, &payload);
    }

    fn request_authorization(state: &'static State) {
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                eprintln!(
                    "[nc] UN auth error domain={} code={} {}",
                    error.domain(),
                    error.code(),
                    error.localizedDescription()
                );
            }
            if traced() {
                eprintln!("[nc] UN auth granted={}", granted.as_bool());
            }
            let mut auth = state.auth.lock().unwrap_or_else(|e| e.into_inner());
            *auth = if granted.as_bool() {
                Auth::Granted
            } else if error.is_null() {
                // The person clicked Don't Allow. Do not ask again this
                // process. A later launch re-reads the system's answer.
                Auth::Denied
            } else {
                // System refusal (unsigned /tmp copy, LaunchServices
                // pointing at a sibling bundle, …) is not a user denial.
                // Stay Idle so a later post can retry after the identity
                // is right.
                Auth::Idle
            };
        });
        state
            .center
            .0
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &completion,
            );
    }

    fn deliver(center: &UNUserNotificationCenter, pending: &Payload) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&pending.title));
        if !pending.body.is_empty() {
            content.setBody(&NSString::from_str(&pending.body));
        }
        if !pending.origin.is_empty() {
            content.setSubtitle(&NSString::from_str(&pending.origin));
        }
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&pending.id),
            &content,
            None,
        );
        let id = pending.id.clone();
        let completion = RcBlock::new(move |error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                eprintln!(
                    "[nc] UN add error domain={} code={} {} id={id}",
                    error.domain(),
                    error.code(),
                    error.localizedDescription()
                );
            } else if traced() {
                eprintln!("[nc] UN delivered id={id}");
            }
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[ivars = ()]
        struct EaveNcDelegate;

        unsafe impl NSObjectProtocol for EaveNcDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for EaveNcDelegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                completion_handler.call((
                    UNNotificationPresentationOptions::Banner
                        | UNNotificationPresentationOptions::List
                        | UNNotificationPresentationOptions::Sound,
                ));
            }
        }
    );

    impl EaveNcDelegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }
}

/// Put one notification on the system's banner list.
///
/// `subtitle` is the third line — the origin for a page, the project or
/// provider for a session. `id` replaces a notification already on screen
/// with the same string, which is how a tag works and how a session that
/// finishes twice does not stack.
///
/// Silently does nothing off macOS, and on a macOS build whose bundle has
/// no identifier (a loose `cargo run` binary) — both are "there is no
/// Notification Center to reach", not errors to report.
#[cfg(target_os = "macos")]
pub(crate) fn post(subtitle: &str, title: &str, body: &str, id: String) {
    mac::post(subtitle, title, body, id);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn post(_subtitle: &str, _title: &str, _body: &str, _id: String) {}
