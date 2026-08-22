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

use std::collections::HashMap;

use cef::*;

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
    Permission { kinds: Vec<String> },
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

/// The permission bits, in words a person can answer.
///
/// Named individually rather than printed as a bitmask because this string
/// is what the operator reads before deciding. "camera and microphone" is a
/// question; `0x480` is not.
fn permission_names(bits: u32) -> Vec<String> {
    let known: &[(u32, &str)] = &[
        (PermissionRequestTypes::CAMERA_STREAM.get_raw(), "camera"),
        (PermissionRequestTypes::MIC_STREAM.get_raw(), "microphone"),
        (
            PermissionRequestTypes::GEOLOCATION.get_raw(),
            "your location",
        ),
        (
            PermissionRequestTypes::NOTIFICATIONS.get_raw(),
            "notifications",
        ),
        (PermissionRequestTypes::CLIPBOARD.get_raw(), "the clipboard"),
        (PermissionRequestTypes::MIDI_SYSEX.get_raw(), "MIDI devices"),
        (
            PermissionRequestTypes::POINTER_LOCK.get_raw(),
            "the pointer",
        ),
        (
            PermissionRequestTypes::KEYBOARD_LOCK.get_raw(),
            "the keyboard",
        ),
        (
            PermissionRequestTypes::WINDOW_MANAGEMENT.get_raw(),
            "your windows",
        ),
        (PermissionRequestTypes::LOCAL_FONTS.get_raw(), "your fonts"),
        (
            PermissionRequestTypes::IDLE_DETECTION.get_raw(),
            "whether you are idle",
        ),
        (
            PermissionRequestTypes::STORAGE_ACCESS.get_raw(),
            "cross-site storage",
        ),
        (
            PermissionRequestTypes::LOCAL_NETWORK.get_raw(),
            "your local network",
        ),
        (
            PermissionRequestTypes::VR_SESSION.get_raw(),
            "virtual reality",
        ),
        (
            PermissionRequestTypes::AR_SESSION.get_raw(),
            "augmented reality",
        ),
    ];
    let mut named: Vec<String> = known
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    if named.is_empty() {
        // An unrecognised permission is still a permission, and denying it
        // silently because eave has no word for it would be the same
        // do-nothing failure this handler exists to remove.
        named.push(format!("an unrecognised capability (0x{bits:x})"));
    }
    named
}

fn media_names(bits: u32) -> Vec<String> {
    let known: &[(u32, &str)] = &[
        (
            MediaAccessPermissionTypes::DEVICE_AUDIO_CAPTURE.get_raw(),
            "microphone",
        ),
        (
            MediaAccessPermissionTypes::DEVICE_VIDEO_CAPTURE.get_raw(),
            "camera",
        ),
        (
            MediaAccessPermissionTypes::DESKTOP_AUDIO_CAPTURE.get_raw(),
            "system audio",
        ),
        (
            MediaAccessPermissionTypes::DESKTOP_VIDEO_CAPTURE.get_raw(),
            "your screen",
        ),
    ];
    let mut named: Vec<String> = known
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect();
    if named.is_empty() {
        named.push(format!("an unrecognised device (0x{bits:x})"));
    }
    named
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
            raise(
                &self.tab_id,
                requesting_origin.map(|o| o.to_string()).unwrap_or_default(),
                Ask::Permission { kinds: media_names(requested_permissions) },
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
            raise(
                &self.tab_id,
                requesting_origin.map(|o| o.to_string()).unwrap_or_default(),
                Ask::Permission { kinds: permission_names(requested_permissions) },
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
        let names = permission_names(both);
        assert!(names.contains(&"camera".to_string()), "{names:?}");
        assert!(names.contains(&"microphone".to_string()), "{names:?}");
        assert_eq!(names.len(), 2);
    }

    /// An unknown bit must still produce a question. Denying it silently
    /// because eave has no word for it is the do-nothing failure this
    /// module exists to remove.
    #[test]
    fn an_unknown_permission_still_asks() {
        let names = permission_names(1 << 30);
        assert_eq!(names.len(), 1);
        assert!(names[0].contains("unrecognised"), "{names:?}");
    }

    #[test]
    fn media_bits_are_named_too() {
        let names = media_names(MediaAccessPermissionTypes::DEVICE_VIDEO_CAPTURE.get_raw());
        assert_eq!(names, vec!["camera".to_string()]);
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
