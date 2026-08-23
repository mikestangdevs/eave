//! What a page ASKS a person, and the one surface all of those asks share.
//!
//! # Why one module for three handlers
//! `alert()`, `<input type=file>` and `getUserMedia()` look unrelated until
//! you notice they are the same shape: **Chromium stops, hands eave a
//! callback, and will not continue until someone answers.** In Chrome all
//! three are drawn by `chrome/browser/ui/`; CEF has no opinion about any of
//! them, so an embedder that fills none of these slots gets three different
//! flavours of the same silent failure.
//!
//! And they were silent in three DIFFERENT ways, which is why they had to be
//! found by reading rather than by using:
//!
//! - **No `JSDialogHandler`** — CEF's default is to suppress the dialog and
//!   continue as if the person had cancelled. `confirm()` is permanently
//!   `false`, `prompt()` permanently `null`. A page's logic runs down its
//!   "the user said no" branch forever and nothing anywhere says why.
//! - **No `DialogHandler`** — `<input type=file>` opens nothing. Under
//!   off-screen rendering there is no native window for CEF to parent a
//!   picker to, so the click is simply consumed.
//! - **No `PermissionHandler`** — camera, microphone, geolocation,
//!   notifications and clipboard are all auto-denied without asking.
//!
//! # The rule this module keeps
//! **A held callback is a stopped page.** Every entry in [`PENDING`] is a
//! renderer waiting on a person, so the two ways to leak one are both
//! serious: dropping the callback without answering leaves the page stopped
//! forever, and answering twice is a use-after-free in Chromium. Everything
//! here goes through [`answer`], which removes before it replies, so a
//! second answer finds nothing and does nothing.
//!
//! One ask per tab, which is also Chromium's own model — it serialises
//! dialogs per frame and will not raise a second while one is outstanding.
//!
//! # The rule G1 added
//! **A permission ask that has already been answered is not asked again.**
//! D9 built the prompt and stopped there, so eave asked for the camera on
//! every page load — which trains the operator to click the primary button
//! without reading, the exact reflex a permission dialog exists to prevent.
//! [`super::permissions`] holds the answers, mirrored from the engine
//! (which is where they survive a restart), and this module consults it
//! before disturbing anybody and records the answer when it does.
//!
//! Only the two permission paths do this. `alert()` and `<input type=file>`
//! are events, not standing decisions, and a browser that remembered "you
//! said OK to an alert once" would be remembering nothing.

use std::collections::HashMap;

use cef::*;

use super::permissions::{PermissionDecision, PermissionKind, Verdict};

/// What the page is asking for.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// `alert()` — one button, and no answer to give back.
    Alert { message: String },
    /// `confirm()` — yes or no.
    Confirm { message: String },
    /// `prompt()` — text, with a default the page suggested.
    Prompt { message: String, default: String },
    /// `beforeunload` — the page would rather you did not leave.
    ///
    /// Separate from [`Ask::Confirm`] because the buttons are not "yes/no"
    /// and the stakes are not symmetric: the destructive answer is the one
    /// that leaves. `reloading` distinguishes ⌘R from a close, which is the
    /// difference between "Reload?" and "Leave?".
    Leave { message: String, reloading: bool },
    /// Camera, microphone, geolocation, notifications, clipboard…
    ///
    /// **Structured, not phrased.** The words come from
    /// [`PermissionKind::label`] at the point of drawing; carrying the
    /// strings instead would mean the answer had no stable key to be filed
    /// under, which is how D9 ended up unable to remember anything.
    Permission { kinds: Vec<PermissionKind> },
    /// `<input type=file>`, or a download asking where to go.
    Files {
        title: String,
        multiple: bool,
        folder: bool,
        save: bool,
    },
}

impl Ask {
    /// Whether this ask can be refused. An `alert()` cannot — it has one
    /// button — and a surface that drew a Cancel beside it would be
    /// offering a choice the page will never hear about.
    pub fn is_refusable(&self) -> bool {
        !matches!(self, Ask::Alert { .. })
    }
}

/// An outstanding ask, and which tab is stopped on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub tab_id: String,
    /// Where it came from. **Shown, always** — a dialog with no origin is
    /// how a page impersonates the browser, and it is the whole reason
    /// Chrome puts the origin above the message rather than beside it.
    pub origin: String,
    pub ask: Ask,
}

/// What a person answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// OK / yes / allow / leave.
    Ok,
    /// Cancel / no / deny / stay.
    Cancel,
    /// A `prompt()`'s text.
    Text(String),
    /// Chosen files, as paths.
    Files(Vec<String>),
}

/// The CEF side of an outstanding ask — the thing that has to be called
/// exactly once.
enum Reply {
    Js(JsdialogCallback),
    File(FileDialogCallback),
    Permission(PermissionPromptCallback),
    Media(MediaAccessCallback, u32),
}

thread_local! {
    /// One ask per tab. Main thread only, like every other map in this
    /// module's neighbours.
    static PENDING: std::cell::RefCell<HashMap<String, (Pending, Reply)>> =
        std::cell::RefCell::new(HashMap::new());
}

/// What this tab is stopped on, if anything.
pub fn pending(tab_id: &str) -> Option<Pending> {
    PENDING.with(|p| p.borrow().get(tab_id).map(|(ask, _)| ask.clone()))
}

/// Whether any tab is stopped on an ask.
pub fn any_pending() -> bool {
    PENDING.with(|p| !p.borrow().is_empty())
}

fn raise(tab_id: &str, origin: String, ask: Ask, reply: Reply) {
    if super::tabs::trace() {
        eprintln!("[web] ask from {origin}: {ask:?}");
    }
    let entry = (
        Pending {
            tab_id: tab_id.to_string(),
            origin,
            ask,
        },
        reply,
    );
    // Replacing an outstanding ask would strand its callback and stop that
    // renderer forever. Chromium serialises dialogs per frame so this should
    // not happen — but "should not" is not "cannot", and the honest answer
    // to a second ask is to refuse the FIRST properly and then take it.
    let displaced = PENDING.with(|p| p.borrow_mut().insert(tab_id.to_string(), entry));
    if let Some((old, reply)) = displaced {
        eprintln!(
            "eave: a second ask arrived for {tab_id} — cancelling {:?}",
            old.ask
        );
        reply_with(reply, Answer::Cancel);
    }
    super::tabs::bump_chrome_revision();
}

/// Answer a tab's outstanding ask. Does nothing if there is none, which is
/// what makes a double-answer safe rather than a crash in Chromium.
pub fn answer(tab_id: &str, answer: Answer) {
    let taken = PENDING.with(|p| p.borrow_mut().remove(tab_id));
    let Some((pending, reply)) = taken else {
        return;
    };
    if super::tabs::trace() {
        eprintln!("[web] answered {:?} with {answer:?}", pending.ask);
    }
    // G1: write the answer down before replying, so a page that re-asks
    // the instant it is unblocked finds it already there.
    if let Ask::Permission { kinds } = &pending.ask {
        remember(&pending.origin, kinds, matches!(answer, Answer::Ok));
    }
    reply_with(reply, answer);
    super::tabs::bump_chrome_revision();
    super::host::request_pump();
}

/// Let go of a tab's ask without answering it — only correct when the
/// browser is going away anyway, because the page it would have unblocked
/// is going with it.
pub fn forget(tab_id: &str) {
    let taken = PENDING.with(|p| p.borrow_mut().remove(tab_id));
    if let Some((_, reply)) = taken {
        // Still answered, not dropped. A callback released without a reply
        // is a renderer left stopped, and on a browser that is closing that
        // is the difference between a clean teardown and a hang.
        reply_with(reply, Answer::Cancel);
    }
}

fn reply_with(reply: Reply, answer: Answer) {
    let ok = matches!(answer, Answer::Ok | Answer::Text(_) | Answer::Files(_));
    match reply {
        Reply::Js(cb) => {
            let text = match &answer {
                Answer::Text(t) => Some(CefString::from(t.as_str())),
                _ => None,
            };
            cb.cont(ok as i32, text.as_ref());
        }
        Reply::File(cb) => match answer {
            Answer::Files(paths) if !paths.is_empty() => {
                let mut list = CefStringList::new();
                for p in &paths {
                    list.append(p);
                }
                cb.cont(Some(&mut list));
            }
            // An empty selection is a cancel, not a selection of nothing.
            _ => cb.cancel(),
        },
        Reply::Permission(cb) => cb.cont(if ok {
            PermissionRequestResult::ACCEPT
        } else {
            PermissionRequestResult::DENY
        }),
        Reply::Media(cb, requested) => {
            if ok {
                // Grant exactly what was asked for and nothing more. CEF
                // takes a bitmask, so it is possible to hand back a
                // permission the page never requested; a person answering
                // "allow" to a microphone prompt has not agreed to a camera.
                cb.cont(requested);
            } else {
                cb.cancel();
            }
        }
    }
}

/// The permission bits, as capabilities eave has words for.
///
/// The mapping lives here and not in `eave-core` for one reason: it needs
/// CEF's constants, and the crate that stores the answer must not depend on
/// Chromium. The *words* live over there, on [`PermissionKind::label`], so
/// that the string a person reads and the key their answer is filed under
/// can never drift apart.
fn permission_kinds(bits: u32) -> Vec<PermissionKind> {
    let known: &[(u32, PermissionKind)] = &[
        (
            PermissionRequestTypes::CAMERA_STREAM.get_raw(),
            PermissionKind::Camera,
        ),
        (
            PermissionRequestTypes::MIC_STREAM.get_raw(),
            PermissionKind::Microphone,
        ),
        (
            PermissionRequestTypes::GEOLOCATION.get_raw(),
            PermissionKind::Geolocation,
        ),
        (
            PermissionRequestTypes::NOTIFICATIONS.get_raw(),
            PermissionKind::Notifications,
        ),
        (
            PermissionRequestTypes::CLIPBOARD.get_raw(),
            PermissionKind::Clipboard,
        ),
        (
            PermissionRequestTypes::MIDI_SYSEX.get_raw(),
            PermissionKind::MidiSysex,
        ),
        (
            PermissionRequestTypes::POINTER_LOCK.get_raw(),
            PermissionKind::PointerLock,
        ),
        (
            PermissionRequestTypes::KEYBOARD_LOCK.get_raw(),
            PermissionKind::KeyboardLock,
        ),
        (
            PermissionRequestTypes::WINDOW_MANAGEMENT.get_raw(),
            PermissionKind::WindowManagement,
        ),
        (
            PermissionRequestTypes::LOCAL_FONTS.get_raw(),
            PermissionKind::LocalFonts,
        ),
        (
            PermissionRequestTypes::IDLE_DETECTION.get_raw(),
            PermissionKind::IdleDetection,
        ),
        (
            PermissionRequestTypes::STORAGE_ACCESS.get_raw(),
            PermissionKind::StorageAccess,
        ),
        (
            PermissionRequestTypes::LOCAL_NETWORK.get_raw(),
            PermissionKind::LocalNetwork,
        ),
        (
            PermissionRequestTypes::VR_SESSION.get_raw(),
            PermissionKind::VrSession,
        ),
        (
            PermissionRequestTypes::AR_SESSION.get_raw(),
            PermissionKind::ArSession,
        ),
    ];
    let mut kinds: Vec<PermissionKind> = known
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, kind)| kind.clone())
        .collect();
    if kinds.is_empty() {
        // An unrecognised permission is still a permission, and denying it
        // silently because eave has no word for it would be the same
        // do-nothing failure this handler exists to remove. It is asked
        // about every time and never remembered — `is_storable` is false —
        // because remembering "allow" for something nobody could read is
        // not consent.
        kinds.push(PermissionKind::Unknown(bits));
    }
    kinds
}

fn media_kinds(bits: u32) -> Vec<PermissionKind> {
    let known: &[(u32, PermissionKind)] = &[
        (
            MediaAccessPermissionTypes::DEVICE_AUDIO_CAPTURE.get_raw(),
            PermissionKind::Microphone,
        ),
        (
            MediaAccessPermissionTypes::DEVICE_VIDEO_CAPTURE.get_raw(),
            PermissionKind::Camera,
        ),
        (
            MediaAccessPermissionTypes::DESKTOP_AUDIO_CAPTURE.get_raw(),
            PermissionKind::ScreenAudio,
        ),
        (
            MediaAccessPermissionTypes::DESKTOP_VIDEO_CAPTURE.get_raw(),
            PermissionKind::ScreenVideo,
        ),
    ];
    let mut kinds: Vec<PermissionKind> = known
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, kind)| kind.clone())
        .collect();
    if kinds.is_empty() {
        kinds.push(PermissionKind::Unknown(bits));
    }
    kinds
}

/// Write an answer down: locally first, so the very next ask finds it, and
/// then to the engine, which is where it survives a restart.
///
/// **Nothing is sent for an origin or a capability that could not be filed
/// honestly.** The engine's fold would refuse them anyway (rules 3 and 4 in
/// `eave_core::browser::permissions`), so sending would only put a line on
/// a socket for the far end to drop.
fn remember(origin: &str, kinds: &[PermissionKind], allowed: bool) {
    let decision = if allowed {
        PermissionDecision::Allow
    } else {
        PermissionDecision::Deny
    };
    let storable: Vec<PermissionKind> = kinds.iter().filter(|k| k.is_storable()).cloned().collect();
    if storable.is_empty() || eave_wire::normalize_origin(origin).is_none() {
        return;
    }
    super::permissions::note_answer(origin, &storable, decision);
    // Chromium has already stored its own copy for the kinds it decides
    // (that is the whole point of `sync_to_chromium`'s doc comment), but
    // stating it explicitly costs one call and removes the case where the
    // two disagree because eave's answer took a path Chromium did not see.
    sync_to_chromium(
        origin,
        &storable,
        if allowed {
            ContentSettingValues::ALLOW
        } else {
            ContentSettingValues::BLOCK
        },
    );
    super::tabs::request(super::PageRequest::RememberPermission {
        origin: origin.to_string(),
        kinds: storable,
        decision,
    });
}

/// Answer an ask from what is already known, or `false` if nobody has
/// answered this before and a person must be disturbed.
///
/// Called from inside the CEF handler and answering the continuation
/// **synchronously**, which CEF documents as allowed for both permission
/// paths — and which is the only option anyway, because the alternative is
/// parking a stopped renderer while nothing is pending.
/// The `ContentSettingsType` Chromium files this capability under, for the
/// kinds Chromium itself decides.
///
/// `None` means **eave is the only store for this one**, and there are two
/// such cases:
///
/// - **camera and microphone.** They arrive down
///   `on_request_media_access_permission`, and CEF answers that from
///   `media_access_query.cc`, which references `HostContentSettingsMap`
///   nowhere at all — `CheckMediaAccessPermission` returns true with the
///   comment "Always allowed here. RequestMediaAccessPermission will be
///   called." Writing a content setting for these would write a fact that
///   nothing ever reads.
/// - **an unrecognised capability**, which is never stored anywhere.
fn content_type(kind: &PermissionKind) -> Option<ContentSettingTypes> {
    Some(match kind {
        PermissionKind::Geolocation => ContentSettingTypes::GEOLOCATION,
        PermissionKind::Notifications => ContentSettingTypes::NOTIFICATIONS,
        PermissionKind::Clipboard => ContentSettingTypes::CLIPBOARD_READ_WRITE,
        PermissionKind::MidiSysex => ContentSettingTypes::MIDI_SYSEX,
        PermissionKind::PointerLock => ContentSettingTypes::POINTER_LOCK,
        PermissionKind::WindowManagement => ContentSettingTypes::WINDOW_MANAGEMENT,
        PermissionKind::LocalFonts => ContentSettingTypes::LOCAL_FONTS,
        PermissionKind::IdleDetection => ContentSettingTypes::IDLE_DETECTION,
        PermissionKind::StorageAccess => ContentSettingTypes::STORAGE_ACCESS,
        PermissionKind::VrSession => ContentSettingTypes::VR,
        PermissionKind::ArSession => ContentSettingTypes::AR,
        // Camera, mic, screen capture, keyboard lock and the unknown bit:
        // eave's store is the only one that decides these.
        _ => return None,
    })
}

/// Put Chromium's copy of an origin's answers in step with eave's.
///
/// # Why this exists, and why leaving it out was a DEFECT
/// For every capability that comes down `on_show_permission_prompt`,
/// **Chromium is the one that decides whether eave is asked at all.**
/// `cb.cont(ACCEPT)` reaches `delegate_->Accept(std::monostate{})` — CEF
/// deliberately does not expose `AcceptThisTime`, so it is always the
/// durable branch — and `PermissionContextBase::RequestPermission` then
/// short-circuits any later request:
///
/// ```text
/// // Check the content setting to see if the user has already made a
/// // decision, or if the origin is under embargo. If so, respect that.
/// if (!status_ignorable && (result.status == GRANTED ||
///                           result.status == DENIED)) { … return; }
/// ```
///
/// `status_ignorable` is false for any request that is not
/// `IsEmbeddedPermissionElementInitiated()`, which an ordinary
/// `Notification.requestPermission()` is not. So without this call, eave's
/// "Reset permissions for this site" cleared eave's two stores, Chromium
/// kept its own ALLOW, **the prompt never came back and the site kept the
/// permission.** The revoke did nothing and said nothing.
///
/// `verdict()`'s deliberate fail-towards-`Ask` cannot save that, because
/// the prompt never arrives to be failed towards.
///
/// Verified against the pinned trees — Chromium 151.0.7922.174 and CEF
/// branch `cef-151`, the pair the lockfile's `cef 151.6.0+151.3.18` names.
fn sync_to_chromium(origin: &str, kinds: &[PermissionKind], value: ContentSettingValues) {
    let Some(context) = request_context_get_global_context() else {
        return;
    };
    // eave passes `None` for the request context when it creates a browser,
    // so the global context IS the one every page uses.
    let url = CefString::from(origin);
    for kind in kinds {
        let Some(ty) = content_type(kind) else {
            continue;
        };
        // Requesting and top-level url are both the origin: eave has no
        // per-embedder permission model, and answering "allow example.com"
        // means allowing it, not allowing it only inside itself.
        context.set_content_setting(Some(&url), Some(&url), ty, value);
        if super::tabs::trace() {
            eprintln!("[web] chromium content setting {kind:?} for {origin} := {value:?}");
        }
    }
}

/// Take back Chromium's copy for one origin. Called by the revoke path.
pub fn clear_in_chromium(origin: &str, kinds: &[PermissionKind]) {
    sync_to_chromium(origin, kinds, ContentSettingValues::DEFAULT);
}

fn trace_remembered(origin: &str, kinds: &[PermissionKind], allowed: bool) {
    // Worth a line of its own: a prompt that does NOT appear is invisible
    // by construction, so without this the difference between "remembered"
    // and "the handler is broken" cannot be seen from outside.
    if super::tabs::trace() {
        let verdict = if allowed { "allow" } else { "deny" };
        eprintln!("[web] permission {kinds:?} from {origin}: {verdict} from memory");
    }
}

fn answered_already(origin: &str, kinds: &[PermissionKind]) -> Option<bool> {
    match super::permissions::verdict(origin, kinds) {
        Verdict::Ask => None,
        Verdict::Allow => Some(true),
        Verdict::Deny => Some(false),
    }
}

// ---------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------

wrap_jsdialog_handler! {
    pub struct PageJsDialogHandler {
        tab_id: String,
    }

    impl JsdialogHandler {
        /// `alert()`, `confirm()`, `prompt()`.
        ///
        /// **Returning 1 means "eave will answer this"** and is a promise:
        /// the callback must be called exactly once, eventually, or the
        /// renderer stays stopped. Returning 0 would hand it back to CEF,
        /// which under off-screen rendering has nowhere to draw it.
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut i32>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let message = message_text.map(|m| m.to_string()).unwrap_or_default();
            let ask = match dialog_type {
                JsdialogType::CONFIRM => Ask::Confirm { message },
                JsdialogType::PROMPT => Ask::Prompt {
                    message,
                    default: default_prompt_text.map(|d| d.to_string()).unwrap_or_default(),
                },
                _ => Ask::Alert { message },
            };
            raise(
                &self.tab_id,
                origin_url.map(|o| o.to_string()).unwrap_or_default(),
                ask,
                Reply::Js(callback.clone()),
            );
            1
        }

        /// `beforeunload` — "changes you made may not be saved".
        ///
        /// This is the one C5 could not answer and worked around: its
        /// departed-tab reaper calls `close_browser(1)` — FORCED — precisely
        /// because there was no surface to ask the question on, and an
        /// unforced close would be refusable by a page with nowhere for the
        /// refusal to be seen. That workaround can be revisited now.
        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            is_reload: i32,
            callback: Option<&mut JsdialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            raise(
                &self.tab_id,
                String::new(),
                Ask::Leave {
                    message: message_text.map(|m| m.to_string()).unwrap_or_default(),
                    reloading: is_reload != 0,
                },
                Reply::Js(callback.clone()),
            );
            1
        }

        /// Chromium is discarding its dialog state — a navigation, usually.
        /// Anything outstanding will never be answered by the page, so it
        /// must not sit on screen waiting for a person either.
        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            PENDING.with(|p| {
                p.borrow_mut().remove(&self.tab_id);
            });
            super::tabs::bump_chrome_revision();
        }
    }
}

wrap_permission_handler! {
    pub struct PagePermissionHandler {
        tab_id: String,
    }

    impl PermissionHandler {
        /// Camera and microphone, which come down their own path rather
        /// than through the general prompt.
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            let kinds = media_kinds(requested_permissions);
            // G1. Grant exactly what was asked for and nothing more, for
            // the same reason `reply_with` does: CEF takes a bitmask, and
            // a remembered "allow" for the microphone is not consent to
            // the camera.
            if let Some(allowed) = answered_already(&origin, &kinds) {
                trace_remembered(&origin, &kinds, allowed);
                if allowed {
                    callback.cont(requested_permissions);
                } else {
                    callback.cancel();
                }
                return 1;
            }
            raise(
                &self.tab_id,
                origin,
                Ask::Permission { kinds },
                Reply::Media(callback.clone(), requested_permissions),
            );
            1
        }

        /// Geolocation, notifications, clipboard, MIDI, and the rest.
        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let origin = requesting_origin.map(|o| o.to_string()).unwrap_or_default();
            let kinds = permission_kinds(requested_permissions);
            if let Some(allowed) = answered_already(&origin, &kinds) {
                trace_remembered(&origin, &kinds, allowed);
                callback.cont(if allowed {
                    PermissionRequestResult::ACCEPT
                } else {
                    PermissionRequestResult::DENY
                });
                return 1;
            }
            raise(
                &self.tab_id,
                origin,
                Ask::Permission { kinds },
                Reply::Permission(callback.clone()),
            );
            1
        }

        /// Chromium withdrew the prompt — the page navigated away, or it
        /// answered itself. Take it off the screen; the callback is already
        /// spent, so this must NOT go through `answer`.
        fn on_dismiss_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _result: PermissionRequestResult,
        ) {
            PENDING.with(|p| {
                p.borrow_mut().remove(&self.tab_id);
            });
            super::tabs::bump_chrome_revision();
        }
    }
}

wrap_dialog_handler! {
    pub struct PageFileDialogHandler {
        tab_id: String,
    }

    impl DialogHandler {
        /// `<input type=file>`, and "save as".
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let title = title
                .map(|t| t.to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "Choose a file".to_string());
            raise(
                &self.tab_id,
                String::new(),
                Ask::Files {
                    title,
                    multiple: mode == FileDialogMode::OPEN_MULTIPLE,
                    folder: mode == FileDialogMode::OPEN_FOLDER,
                    save: mode == FileDialogMode::SAVE,
                },
                Reply::File(callback.clone()),
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alert_has_nothing_to_refuse() {
        assert!(!Ask::Alert {
            message: "hi".into()
        }
        .is_refusable());
        assert!(Ask::Confirm {
            message: "hi".into()
        }
        .is_refusable());
        assert!(Ask::Prompt {
            message: "hi".into(),
            default: String::new()
        }
        .is_refusable());
    }

    /// The names are what the operator reads before deciding, so a
    /// combination has to read as a sentence and not as a bitmask.
    #[test]
    fn permissions_are_named_not_numbered() {
        let both = PermissionRequestTypes::CAMERA_STREAM.get_raw()
            | PermissionRequestTypes::MIC_STREAM.get_raw();
        let kinds = permission_kinds(both);
        assert!(kinds.contains(&PermissionKind::Camera), "{kinds:?}");
        assert!(kinds.contains(&PermissionKind::Microphone), "{kinds:?}");
        assert_eq!(kinds.len(), 2);
        let names: Vec<String> = kinds.iter().map(|k| k.label()).collect();
        assert_eq!(names, vec!["camera".to_string(), "microphone".to_string()]);
    }

    /// An unknown bit must still produce a question. Denying it silently
    /// because eave has no word for it is the do-nothing failure this
    /// module exists to remove — and G1 must not turn it into a silent
    /// PERMANENT grant either, so it is unstorable.
    #[test]
    fn an_unknown_permission_still_asks_and_is_never_remembered() {
        let kinds = permission_kinds(1 << 30);
        assert_eq!(kinds, vec![PermissionKind::Unknown(1 << 30)]);
        assert!(kinds[0].label().contains("unrecognised"), "{kinds:?}");
        assert!(!kinds[0].is_storable());
        assert!(answered_already("https://x.test", &kinds).is_none());
    }

    #[test]
    fn media_bits_are_named_too() {
        let kinds = media_kinds(MediaAccessPermissionTypes::DEVICE_VIDEO_CAPTURE.get_raw());
        assert_eq!(kinds, vec![PermissionKind::Camera]);
        assert_eq!(kinds[0].label(), "camera");
    }

    /// The screen is not the camera and system audio is not the
    /// microphone. They are separate keys, so allowing the camera must
    /// never silently allow a screen capture.
    #[test]
    fn the_screen_and_the_camera_are_different_permissions() {
        let screen = media_kinds(MediaAccessPermissionTypes::DESKTOP_VIDEO_CAPTURE.get_raw());
        assert_eq!(screen, vec![PermissionKind::ScreenVideo]);
        assert_ne!(screen[0], PermissionKind::Camera);
        assert_eq!(
            media_kinds(MediaAccessPermissionTypes::DESKTOP_AUDIO_CAPTURE.get_raw()),
            vec![PermissionKind::ScreenAudio]
        );
    }

    #[test]
    fn nothing_is_pending_on_a_quiet_tab() {
        assert!(pending("dlg-quiet").is_none());
    }

    /// Answering a tab with no ask must be a no-op and not a panic — the
    /// shell can race a dismissal against a click.
    #[test]
    fn answering_nothing_does_nothing() {
        answer("dlg-nothing", Answer::Ok);
        assert!(pending("dlg-nothing").is_none());
    }
}
