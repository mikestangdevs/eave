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
//!   notifications and clipboard were all auto-denied without asking.
//!   (That handler exists now. The line is kept because it is what this
//!   module was built to fix — but note that it stopped being true, and
//!   notifications spent a while grantable-and-undeliverable as a result.
//!   See `permissions::can_deliver` and 06-browser G2.)
//!
//! # The rule this module keeps
//! **A held callback is a stopped page.** Every entry in [`REPLIES`] is a
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

/// The vocabulary lives in [`super::ask`], **outside the feature gate**,
/// so `cargo test` can assert on it without Chromium in the binary. This
/// module is the CEF half only. See that module for why (06-browser G26).
pub use super::ask::{decision_of, page_hears_yes, Answer, Ask, Pending};

/// The CEF side of an outstanding ask — the thing that has to be called
/// exactly once.
enum Reply {
    Js(JsdialogCallback),
    File(FileDialogCallback),
    Permission(PermissionPromptCallback),
    Media(MediaAccessCallback, u32),
    /// D7. `cont(user, password)` or `cancel()`, exactly once.
    Auth(AuthCallback),
    /// D8. `cont()` proceeds past a bad certificate; `cancel()` blocks the
    /// load. **`cancel` is the safe direction and every path that is not an
    /// explicit "proceed" takes it** — see `reply_with`.
    Cert(Callback),
}

thread_local! {
    /// The CEF half of every outstanding ask — the thing that must be
    /// called exactly once.
    ///
    /// **The ASK itself is not in here** (G26): it lives in
    /// `super::ask::PENDING`, outside the feature gate, so the suite can
    /// render and drive the dialog. This map holds only what cannot leave
    /// the gate — a live Chromium continuation. The two are kept in step
    /// by `raise` and `answer`, which are the only writers.
    ///
    /// Main thread only, like every other map in this module's neighbours.
    static REPLIES: std::cell::RefCell<HashMap<String, Reply>> =
        std::cell::RefCell::new(HashMap::new());
}

fn raise(tab_id: &str, origin: String, ask: Ask, reply: Reply) {
    if super::tabs::trace() {
        eprintln!("[web] ask from {origin}: {ask:?}");
    }
    // Replacing an outstanding ask would strand its callback and stop that
    // renderer forever. Chromium serialises dialogs per frame so this should
    // not happen — but "should not" is not "cannot", and the honest answer
    // to a second ask is to refuse the FIRST properly and then take it.
    //
    // The reply goes in FIRST and the displaced one comes back out of the
    // same map, so the two maps cannot end up holding different tabs' idea
    // of what is outstanding.
    let displaced_reply = REPLIES.with(|r| r.borrow_mut().insert(tab_id.to_string(), reply));
    let displaced = super::ask::note_raised(Pending {
        tab_id: tab_id.to_string(),
        origin,
        ask,
    });
    if let Some(old) = displaced {
        eprintln!(
            "eave: a second ask arrived for {tab_id} — dismissing {:?}",
            old.ask
        );
    }
    if let Some(reply) = displaced_reply {
        // A dismissal, not a refusal: nobody was asked, so nobody
        // answered, and G27's rule is that only a person's answer is
        // written down.
        reply_with(reply, Answer::Dismiss);
    }
    super::tabs::bump_chrome_revision();
}

/// Answer a tab's outstanding ask. Does nothing if there is none, which is
/// what makes a double-answer safe rather than a crash in Chromium.
pub fn answer(tab_id: &str, answer: Answer) {
    let taken = super::ask::take(tab_id);
    let reply = REPLIES.with(|r| r.borrow_mut().remove(tab_id));
    let Some(pending) = taken else {
        return;
    };
    // **An ask with no Chromium continuation is not a bug.** G2's
    // notification prompt is raised by eave itself (`notify::handle_events`)
    // because the shim intercepts `requestPermission()` before Chromium ever
    // sees it — so there is a person to answer and a decision to store, but
    // nothing to call back into. Bailing here, as this did, took the prompt
    // off the screen and wrote nothing down, which made the permission
    // unanswerable and the capability unreachable by any path a person has.
    let Some(reply) = reply else {
        if let Ask::Permission { kinds } = &pending.ask {
            if let Some(allowed) = decision_of(&answer) {
                remember(&pending.origin, kinds, allowed);
            }
        }
        if let Ask::OpenScheme { url, .. } = &pending.ask {
            if matches!(answer, Answer::Ok) {
                super::tabs::request(super::PageRequest::ConfirmedScheme { url: url.clone() });
            }
        }
        if super::tabs::trace() {
            eprintln!("[web] answered {:?} with {answer:?} — eave's own ask", pending.ask);
        }
        super::host::note_chrome_change();
        return;
    };
    if super::tabs::trace() {
        eprintln!("[web] answered {:?} with {answer:?}", pending.ask);
    }
    // G1: write the answer down before replying, so a page that re-asks
    // the instant it is unblocked finds it already there.
    //
    // **G27: only if it IS an answer.** This used to be
    // `matches!(answer, Answer::Ok)`, which filed every dismissal as a
    // durable deny — see `ask::decision_of` for the whole shape of it.
    if let Ask::Permission { kinds } = &pending.ask {
        if let Some(allowed) = decision_of(&answer) {
            remember(&pending.origin, kinds, allowed);
        } else if super::tabs::trace() {
            eprintln!(
                "[web] dismissed {kinds:?} from {} — nothing written down",
                pending.origin
            );
        }
    }
    reply_with(reply, answer);
    super::tabs::bump_chrome_revision();
    super::host::request_pump();
}

/// Let go of a tab's ask without answering it — only correct when the
/// browser is going away anyway, because the page it would have unblocked
/// is going with it.
pub fn forget(tab_id: &str) {
    super::ask::take(tab_id);
    let taken = REPLIES.with(|r| r.borrow_mut().remove(tab_id));
    if let Some(reply) = taken {
        // Still answered, not dropped. A callback released without a reply
        // is a renderer left stopped, and on a browser that is closing that
        // is the difference between a clean teardown and a hang.
        //
        // `Dismiss` rather than `Cancel` (G27): closing a tab with a
        // camera prompt up is not the operator refusing the camera.
        reply_with(reply, Answer::Dismiss);
    }
}

fn reply_with(reply: Reply, answer: Answer) {
    let ok = page_hears_yes(&answer);
    // A dismissal is told to Chromium AS a dismissal, not as a refusal.
    // Chromium's `PermissionDecisionAutoBlocker` counts dismissals and
    // embargoes an origin only after three of them
    // (`kDefaultDismissalsBeforeBlock`), which is the behaviour eave wants
    // and cannot get by writing the block itself.
    let dismissed = matches!(answer, Answer::Dismiss);
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
        } else if dismissed {
            PermissionRequestResult::DISMISS
        } else {
            PermissionRequestResult::DENY
        }),
        Reply::Auth(cb) => match answer {
            Answer::Credentials { user, password } => {
                cb.cont(
                    Some(&CefString::from(user.as_str())),
                    Some(&CefString::from(password.as_str())),
                );
            }
            // Anything else — Cancel, Dismiss, a stray Ok with no fields —
            // is "no credentials", and Chromium turns that into the 401 the
            // server already sent.
            _ => cb.cancel(),
        },
        Reply::Cert(cb) => {
            if ok {
                cb.cont();
            } else {
                cb.cancel();
            }
        }
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
    // G2: `can_deliver` is a fact about eave, not an answer from a person,
    // so it must never be written down as one. Storing the refusal would
    // leave a Deny nobody chose sitting in `permissions.jsonl`, outliving
    // the missing capability and quietly answering for the person on the
    // day it arrives.
    let storable: Vec<PermissionKind> = kinds
        .iter()
        .filter(|k| k.is_storable() && super::permissions::can_deliver(k))
        .cloned()
        .collect();
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

/// A server (or a proxy) is demanding a username and a password (D7).
///
/// Returns whether the ask was raised. `false` means the callback was NOT
/// taken and the caller must return 0 so Chromium cancels the request
/// itself — a held callback nobody can answer is a stopped load forever.
pub(super) fn raise_auth(
    tab_id: &str,
    origin_url: &str,
    host: &str,
    port: i32,
    realm: Option<String>,
    is_proxy: bool,
    callback: &AuthCallback,
) -> bool {
    // The host as the operator has to judge it. A port is part of the
    // identity of a server and omitting it is how two different services on
    // one machine become indistinguishable in a password prompt.
    let where_from = match port {
        0 | 80 | 443 => host.to_string(),
        p => format!("{host}:{p}"),
    };
    // An empty realm is not a realm. Servers send `realm=""` routinely and
    // rendering it would draw an empty quoted string under the host.
    let realm = realm.filter(|r| !r.trim().is_empty());
    raise(
        tab_id,
        // For a proxy the origin is NOT where the password goes, so the
        // dialog states the proxy as its subject and keeps the page's
        // origin only as the context line the surface always shows.
        origin_url.to_string(),
        Ask::Credentials {
            host: where_from,
            realm,
            is_proxy,
        },
        Reply::Auth(callback.clone()),
    );
    true
}

/// The TLS certificate for this navigation did not check out (D8).
///
/// # Why this is raised at all, when not raising it is also safe
/// CEF's default for an unhandled `on_certificate_error` is to cancel the
/// request. That is the safe outcome and it is also a **silent** one: the
/// operator gets a failed load with no way to tell a bad certificate from
/// a dead host. The interstitial exists to say which.
pub(super) fn raise_cert(tab_id: &str, url: &str, reason: &str, callback: &Callback) {
    raise(
        tab_id,
        url.to_string(),
        Ask::Certificate {
            url: url.to_string(),
            reason: reason.to_string(),
        },
        Reply::Cert(callback.clone()),
    );
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
    // The guard lives INSIDE the accessor now, because three separate
    // slices have each been bitten by forgetting it here — see
    // `host::global_request_context`, which is the only place
    // `request_context_get_global_context` is named.
    let Some(context) = super::host::global_request_context() else {
        return;
    };
    // eave passes `None` for the request context when it creates a browser,
    // so the global context IS the one every page uses.
    let url = CefString::from(origin);
    for kind in kinds {
        let Some(ty) = content_type(kind) else {
            continue;
        };
        // G2: every write to Chromium goes through this loop, so forcing
        // the value here is what makes "Chromium never delivers a
        // notification" true of all paths rather than of the ones I
        // remembered to edit.
        //
        // The forcing SURVIVED the capability being switched on. eave now
        // shows notifications itself (`super::notify`), so a person can
        // grant one and eave's own store says Allow — but Chromium's copy
        // of that answer stays BLOCK, because Chromium's notification path
        // is the half that lies: its platform service is present, non-null,
        // and drops what it is handed. Granting it here would restore the
        // exact defect G2 exists to fix, behind a working feature.
        let value = if super::permissions::eave_delivers(kind) {
            ContentSettingValues::BLOCK
        } else {
            value
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

/// Push everything eave remembers into Chromium's content settings (G28).
///
/// # The defect this closes
/// [`sync_to_chromium`] ran on **answer** and on **revoke**, so the two
/// stores agreed only for the session in which the answer was given.
/// Measured on 2026-08-23: with a durable `allow` for
/// `http://127.0.0.1:8765/` in `permissions.jsonl`, a fresh launch reported
/// `Notification.permission === 'default'` and
/// `profile.content_settings.exceptions.notifications` was `{}`.
///
/// `requestPermission()` still resolved correctly, because that path
/// consults [`super::permissions`] first — which is exactly why this hid.
/// **The capability worked and the introspection lied**, and a page that
/// *reads* the state rather than calling the request API — the common
/// pattern for notifications — saw a permission it had been granted as
/// unset, for ever.
///
/// # What it does NOT do
/// This writes; it does not reconcile. An ALLOW that exists only in
/// Chromium — the shape G1b fixed for the revoke path — is not visible
/// from here, because the CEF seam offers `set_content_setting` and
/// `get_content_setting` but **no way to enumerate what an origin already
/// has**, and eave does not know which origins to ask about. Revocation
/// clears Chromium at the moment it happens ([`clear_in_chromium`]), so
/// the remaining hole is a store edited behind eave's back.
///
/// Camera and microphone are absent by construction: [`content_type`]
/// returns `None` for them and eave's store is deliberately the only one.
pub fn replay_into_chromium() {
    if !super::host::running() {
        return;
    }
    let grants = super::permissions::grants();
    let mut written = 0usize;
    for grant in &grants {
        if !grant.permission.is_storable() {
            continue;
        }
        if content_type(&grant.permission).is_none() {
            continue;
        }
        let value = match grant.decision {
            PermissionDecision::Allow => ContentSettingValues::ALLOW,
            PermissionDecision::Deny => ContentSettingValues::BLOCK,
        };
        sync_to_chromium(
            &grant.origin,
            std::slice::from_ref(&grant.permission),
            value,
        );
        written += 1;
    }
    block_undeliverable_by_default();
    if super::tabs::trace() {
        eprintln!(
            "[web] replayed {written} of {} grants into chromium",
            grants.len()
        );
    }
}

/// Make the refusal Chromium's own default, so the page never asks.
///
/// **This is what actually turns `Notification.permission` into
/// `'denied'`.** Refusing in `on_show_permission_prompt` stops eave saying
/// yes, but a page that reads `Notification.permission` without calling
/// `requestPermission()` never goes through that handler at all, and a page
/// that does call it gets a prompt eave then refuses — a prompt for
/// something that could never have worked. Setting the default means
/// Chromium answers both itself, with no prompt and no round trip.
///
/// `set_content_setting` with **both urls absent** sets the DEFAULT value
/// for the type — that is the documented contract of
/// `cef_request_context_t::set_content_setting`. Read, not measured: it is
/// the one claim in this file I have not put in front of a running browser,
/// so it is deliberately the *belt* and not the *braces*. If the contract
/// is not what I read, the per-prompt refusal above still holds and eave
/// still never grants — the page would simply see `'default'` instead of
/// `'denied'` and be told no when it asks. Settle it with a live probe
/// (06-browser G2) and delete this paragraph.
fn block_undeliverable_by_default() {
    let Some(context) = super::host::global_request_context() else {
        return;
    };
    for kind in super::permissions::eave_delivered_kinds() {
        let Some(ty) = content_type(&kind) else {
            continue;
        };
        context.set_content_setting(None, None, ty, ContentSettingValues::BLOCK);
        if super::tabs::trace() {
            eprintln!("[web] chromium DEFAULT {kind:?} := BLOCK — eave delivers it itself");
        }
    }
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
            super::ask::take(&self.tab_id);
            REPLIES.with(|r| {
                r.borrow_mut().remove(&self.tab_id);
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
            // G2: refuse what eave cannot honour, BEFORE the store is
            // consulted and before anyone is asked.
            //
            // Answered first because a stored Allow from before this rule
            // existed would otherwise be replayed straight back into a
            // false grant — the person really did click Allow once, and the
            // grant is still on disk. It is not overwritten (see `remember`,
            // which will not write one either way): it is simply not acted
            // on while the capability is missing, so it becomes true again
            // by itself on the day the renderer seam lands.
            //
            // The whole request is denied, not just the undeliverable part,
            // because one `PermissionPromptCallback` answers the whole
            // prompt — there is no partial ACCEPT. In practice this costs
            // nothing: `Notification.requestPermission()` asks for
            // notifications alone, and the default BLOCK written by
            // `replay_into_chromium` means Chromium mostly answers these
            // itself and never reaches this handler at all.
            let refused = super::permissions::undeliverable(&kinds);
            if !refused.is_empty() {
                if super::tabs::trace() {
                    eprintln!(
                        "[web] refused {refused:?} from {origin} — eave cannot \
                         deliver it, so granting it would be a lie (G2)"
                    );
                }
                callback.cont(PermissionRequestResult::DENY);
                return 1;
            }
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
            super::ask::take(&self.tab_id);
            REPLIES.with(|r| {
                r.borrow_mut().remove(&self.tab_id);
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
    // `dialogs` re-exports the ask VOCABULARY from `ask` (line 57) but not
    // the store, because nothing in the CEF half reads it — the callbacks
    // live here, the map lives there. These tests do read it, and they are
    // the only thing in this file that does, so the import is theirs.
    //
    // Nothing caught this: the feature-off suite never compiles this module
    // and `cargo check --features web` does not build `#[cfg(test)]` code,
    // which is the same blind spot Slice R1 recorded. It has been broken
    // since 4260180 — the commit whose report claimed a green
    // `--features web` run.
    use crate::web::pending;

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
