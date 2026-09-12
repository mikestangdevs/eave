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
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    /// One notification's payload, as it goes to Notification Center.
    struct Payload {
        origin: String,
        title: String,
        body: String,
        id: String,
        sound: super::Sound,
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

    /// The identifier of a banner the operator clicked, waiting to be
    /// acted on by the app's own loop.
    ///
    /// One slot and not a queue: two clicks before the app has looked mean
    /// the operator asked for the second one. A queue would take them to
    /// the first and then move the selection under them.
    static ACTIVATED: Mutex<Option<String>> = Mutex::new(None);

    /// How this module asks the app to come and look. Installed once at
    /// startup; absent in any process that never wires one (the harness,
    /// a bare `cargo run`), where the slot simply fills and is never read.
    static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    pub(super) fn set_waker(f: Box<dyn Fn() + Send + Sync>) {
        let _ = WAKE.set(f);
    }

    pub(super) fn take_activation() -> Option<String> {
        ACTIVATED.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn note_activation(id: String) {
        if traced() {
            eprintln!("[nc] activated id={id}");
        }
        // **And to `EAVE_NC_TRACE`'s FILE.** A click on a banner is the
        // one event in this module that cannot be provoked from a test or
        // a socket — it takes a human hand or a synthetic one — so when it
        // does not work there is nothing to look at. stderr is not that
        // thing here: NT1 measured `open --stderr` capturing NOTHING from
        // a bundle, and a 0-byte log reads exactly like a callback that
        // never fired.
        if let Some(p) = std::env::var_os("EAVE_NC_TRACE") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                let _ = writeln!(f, "activated id={id}");
            }
        }
        *ACTIVATED.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        if let Some(wake) = WAKE.get() {
            wake();
        }
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

    pub(super) fn post(origin: &str, title: &str, body: &str, id: String, sound: super::Sound) {
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
            sound,
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
        // **An empty sound name is NOT silence** — it is the name that
        // resolves to the system default, which is the trap dray records
        // beside this same decision. Both arms here name a sound on
        // purpose; neither is "no sound", because this channel only fires
        // when the operator is in another application and a banner they
        // cannot hear is a banner they will not get.
        let sound = match pending.sound {
            super::Sound::Default => UNNotificationSound::defaultSound(),
            // A question holds the session open until it is answered, so
            // it is allowed to be more insistent than news that keeps.
            // `soundNamed:` takes a system sound by name; an unknown name
            // falls back to the default rather than failing, which is the
            // right failure for something this cosmetic.
            super::Sound::Ask => UNNotificationSound::soundNamed(&NSString::from_str("Ping")),
        };
        content.setSound(Some(&sound));
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

            /// The operator clicked the banner (Slice NT4b).
            ///
            /// **Without this arm the feature LOOKS like it works.** macOS
            /// raises the app on a banner click by itself, so eave came
            /// forward and sat on whatever session was already selected —
            /// which is indistinguishable from "it took me there" when the
            /// right session happens to be open, and silently wrong the
            /// rest of the time. Dray records exactly this shape, and the
            /// lesson it draws is the one worth keeping: **raising the
            /// window and selecting the session are separate things and
            /// both are needed.** macOS does the first; this does the
            /// second.
            ///
            /// Nothing is selected on THIS stack. The identifier is put in
            /// a slot and the app is woken — the same rule
            /// `evict::note_closed` follows for the same reason, that a
            /// foreign callback is not a place to reach into the UI from.
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion_handler: &block2::DynBlock<dyn Fn()>,
            ) {
                let id = unsafe { response.notification() }.request().identifier();
                note_activation(id.to_string());
                // Called unconditionally and last. A delegate that returns
                // without calling this leaks the system's completion block
                // and macOS logs a violation.
                completion_handler.call(());
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

/// How loud one banner is allowed to be (Slice NT4).
///
/// Two values and no `Silent`, deliberately. This channel fires only when
/// the operator is in another application — that is the whole gate on it —
/// so a banner with no sound is one they may never notice, which makes it
/// indistinguishable from the feature not working. The choice here is
/// between two kinds of news, not between speaking and not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sound {
    /// A finish: news that keeps.
    Default,
    /// A question: the session is STOPPED until it is answered.
    Ask,
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
pub(crate) fn post(subtitle: &str, title: &str, body: &str, id: String, sound: Sound) {
    mac::post(subtitle, title, body, id, sound);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn post(_subtitle: &str, _title: &str, _body: &str, _id: String, _sound: Sound) {}

/// Install the callback this module uses to ask the app to come and look
/// after a banner is clicked (Slice NT4b). Called once at startup.
#[cfg(target_os = "macos")]
pub(crate) fn set_waker(f: Box<dyn Fn() + Send + Sync>) {
    mac::set_waker(f);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_waker(_f: Box<dyn Fn() + Send + Sync>) {}

/// The identifier of a banner the operator clicked since this was last
/// asked, if any. Consumed — asking twice gives it to the first caller.
#[cfg(target_os = "macos")]
pub(crate) fn take_activation() -> Option<String> {
    mac::take_activation()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn take_activation() -> Option<String> {
    None
}

/// Put a count on the Dock icon, or clear it (Slice NT5).
///
/// **The Dock badge is a promise about what opening the window shows**, so
/// what it counts is not a free choice: it is the number of rail rows
/// wearing a mark, and it is computed by the same predicate the rail draws
/// with. A badge that counted anything else would send the operator to a
/// window with nothing in it, which teaches them to stop trusting the
/// number — and a number nobody trusts is worse than no number.
///
/// **It needs no clearing path of its own**, which is the test of whether
/// the predicate was chosen correctly. An unread finish clears when the
/// row is viewed because `rail_seen` moves; a question clears when it is
/// answered because the session leaves `awaiting_approval`. Nothing here
/// has to remember to turn the badge off.
///
/// `0` sends `nil`, which is what REMOVES a badge — an empty string is not
/// the same thing and leaves an empty red dot on the icon.
///
/// Raw `msg_send!` rather than a new `objc2-app-kit` dependency: this is
/// two messages to an object every Cocoa app already has, and the crate
/// would be carried by every build to reach them.
#[cfg(target_os = "macos")]
pub(crate) fn set_dock_badge(count: usize) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    // No bundle, no Dock tile worth writing to — the same gate `post` uses
    // and for the same reason.
    if objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .is_none()
    {
        return;
    }
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let tile: *mut AnyObject = msg_send![app, dockTile];
        if tile.is_null() {
            return;
        }
        if count == 0 {
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![tile, setBadgeLabel: nil];
        } else {
            let label: Retained<NSString> = NSString::from_str(&count.to_string());
            let _: () = msg_send![tile, setBadgeLabel: &*label];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_dock_badge(_count: usize) {}
