//! The app's copy of what a person has already answered (G1).
//!
//! # Why a mirror at all
//! The engine owns permission answers, because they must survive a restart
//! and this process owns nothing that does. But Chromium asks its question
//! **synchronously, on this thread's own stack** — `on_show_permission_prompt`
//! is a callback with a live continuation in it, and the rule this whole
//! module tree is written around says you answer it before you do anything
//! else. There is no point in that callback where a round trip to another
//! process could be awaited.
//!
//! So the answer has to already be here when the question arrives. The
//! browser task subscribes to [`eave_wire::Request::WatchPermissions`] and
//! drops each snapshot in here; the handler reads it with a lock it holds
//! for the length of a map lookup. **The mirror is a copy. The engine is
//! the record.** Nothing here writes to disk and nothing here is consulted
//! after a restart until the first snapshot lands.
//!
//! # Why `RwLock` and not the `thread_local!` its neighbours use
//! Every other map in `web/` is written and read on the same thread. This
//! one is written by a tokio task and read by the CEF handler on the main
//! thread, so a `thread_local!` would give the reader a permanently empty
//! mirror — which fails in the worst possible direction: it would look
//! exactly like "nothing is remembered", and eave would go back to asking
//! every time with no error anywhere.
//!
//! # Ungated on purpose
//! Same bargain as `policy.rs`: `cargo test` runs with the feature OFF, so
//! anything behind the gate is untested code. The mirror is a lock around
//! a map and the decision is a pure function of it, so both live here and
//! the gated caller in `dialogs.rs` is a handful of lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

pub use eave_wire::{Grant, PermissionDecision, PermissionKind, Permissions, Verdict};

fn mirror() -> &'static RwLock<Permissions> {
    static MIRROR: OnceLock<RwLock<Permissions>> = OnceLock::new();
    MIRROR.get_or_init(|| RwLock::new(Permissions::new()))
}

/// Take a snapshot from the engine. Replacement, not a merge — this is a
/// snapshot channel and the engine's state is the whole truth, including
/// the part where a revoked grant is *absent*.
pub fn install(grants: Vec<Grant>) {
    if let Ok(mut m) = mirror().write() {
        *m = Permissions::from_grants(grants);
    }
    // G28: Chromium keeps its own copy and this snapshot has just made
    // eave's disagree with it. The reconciliation cannot happen here —
    // see `NEEDS_REPLAY`.
    NEEDS_REPLAY.store(true, Ordering::Release);
}

/// Set when a snapshot lands; cleared by whoever replays it into Chromium.
///
/// # Why a flag and not just doing it
/// [`install`] runs on a **tokio task** (`ui.rs`'s `watch_permissions`
/// subscriber). Reconciling with Chromium means
/// `CefRequestContext::SetContentSetting`, which is a browser-process
/// UI-thread call — from anywhere else it is undefined behaviour rather
/// than an error. So the task raises a flag and the main thread's drive
/// pass does the work, the same handoff the preload queue uses.
///
/// An `AtomicBool` and not a channel because the payload is "the mirror
/// changed" and the mirror is already shared; coalescing several snapshots
/// into one replay is correct, not a compromise.
static NEEDS_REPLAY: AtomicBool = AtomicBool::new(false);

/// Whether a snapshot has landed since the last replay. Clears the flag.
///
/// **Main thread only**, and the caller must be able to reach Chromium —
/// clearing this without replaying loses the reconciliation until the next
/// snapshot, which for a browser left open is never.
pub fn take_replay_request() -> bool {
    NEEDS_REPLAY.swap(false, Ordering::AcqRel)
}

/// What to do about an ask, without disturbing anybody.
///
/// [`Verdict::Ask`] on any doubt at all — an unremembered capability, an
/// origin with no honest key, or a poisoned lock. Failing towards the
/// prompt is the only safe direction: the cost is one dialog, and the cost
/// of failing the other way is a camera.
/// What eave can actually deliver on, today.
///
/// **06-browser G2.** A permission eave grants and then cannot honour is
/// worse than one it refuses: the page is told it succeeded, so it takes no
/// fallback, shows no in-page banner, and the person is simply never told
/// anything. A refusal is a fact the page can act on; a false grant is not.
///
/// Notifications are the case. **CEF exposes no notification API at all** —
/// the 151.6.0 bindings carry 24 symbols matching "notification" and every
/// one of them is a colour id (`CEF_ColorNotificationBackgroundActive` and
/// friends). There is no handler to implement and no callback to answer.
/// Displaying one would mean shimming `window.Notification` in the renderer
/// and routing it out to `UNUserNotificationCenter`, and eave has no
/// renderer seam to shim through: there is not one `RenderProcessHandler`,
/// `on_process_message_received`, `send_process_message` or
/// `execute_java_script` in the crate, and `web_helper.rs` is 27 lines.
/// That seam is a slice of its own — and the one E8's agent-visible console
/// needs too, which is the argument for building it once, deliberately.
///
/// **This is the ONE place to flip when the seam exists.** Everything else
/// asks this question rather than naming the kind.
pub fn can_deliver(kind: &PermissionKind) -> bool {
    !matches!(kind, PermissionKind::Notifications)
}

/// Every kind eave models, minus `Unknown`.
///
/// Written out rather than derived because `PermissionKind` carries an
/// `Unknown(String)` and so cannot be a `const` slice. **A new variant must
/// be added here**; `notifications_are_the_only_thing_eave_refuses` is what
/// notices when one is not, because a list that silently misses a kind is
/// exactly how a default-block rule stops covering what it claims to.
pub fn all_kinds() -> Vec<PermissionKind> {
    vec![
        PermissionKind::Camera,
        PermissionKind::Microphone,
        PermissionKind::Geolocation,
        PermissionKind::Notifications,
        PermissionKind::Clipboard,
        PermissionKind::MidiSysex,
        PermissionKind::PointerLock,
        PermissionKind::KeyboardLock,
        PermissionKind::WindowManagement,
        PermissionKind::LocalFonts,
        PermissionKind::IdleDetection,
        PermissionKind::StorageAccess,
        PermissionKind::LocalNetwork,
        PermissionKind::VrSession,
        PermissionKind::ArSession,
        PermissionKind::ScreenAudio,
        PermissionKind::ScreenVideo,
    ]
}

/// Compile-time guard on [`all_kinds`]: adding a `PermissionKind` variant
/// breaks this match, which is the reminder to add it to the list above.
///
/// A test that counted `all_kinds().len()` against a literal was the first
/// thing written here and it was worthless — it compared the list to
/// itself, and it passed while the list was missing `ScreenAudio` and
/// `ScreenVideo`. Exhaustiveness is the compiler's job; only the compiler
/// can see a variant nobody wrote down.
#[cfg(test)]
fn _every_variant_is_accounted_for(kind: &PermissionKind) {
    match kind {
        PermissionKind::Camera
        | PermissionKind::Microphone
        | PermissionKind::Geolocation
        | PermissionKind::Notifications
        | PermissionKind::Clipboard
        | PermissionKind::MidiSysex
        | PermissionKind::PointerLock
        | PermissionKind::KeyboardLock
        | PermissionKind::WindowManagement
        | PermissionKind::LocalFonts
        | PermissionKind::IdleDetection
        | PermissionKind::StorageAccess
        | PermissionKind::LocalNetwork
        | PermissionKind::VrSession
        | PermissionKind::ArSession
        | PermissionKind::ScreenAudio
        | PermissionKind::ScreenVideo => {}
        // Never in `all_kinds`: it is not a kind, it is a bit this build
        // has no word for, and it is never persisted either.
        PermissionKind::Unknown(_) => {}
    }
}

/// Everything eave cannot deliver, enumerated.
///
/// The per-origin callers ask `can_deliver` about one kind. Chromium's
/// DEFAULT content setting has no origin to key on, so that caller needs
/// the whole set — and gets it by filtering [`all_kinds`] through the same
/// predicate, so the two cannot disagree.
pub fn undeliverable_kinds() -> Vec<PermissionKind> {
    undeliverable(&all_kinds())
}

/// The kinds in `kinds` that eave cannot honour, in order.
///
/// Returned rather than a bool because the callers want to *say* which one
/// stopped them — a trace line naming `Notifications` is the difference
/// between "the handler is broken" and "eave refused on purpose".
pub fn undeliverable(kinds: &[PermissionKind]) -> Vec<PermissionKind> {
    kinds.iter().filter(|k| !can_deliver(k)).cloned().collect()
}

pub fn verdict(origin: &str, kinds: &[PermissionKind]) -> Verdict {
    match mirror().read() {
        Ok(m) => m.verdict(origin, kinds),
        Err(_) => Verdict::Ask,
    }
}

/// Apply an answer locally, at the moment it is given.
///
/// The engine is told too, and its snapshot will arrive and replace this —
/// so why write it here at all? Because the round trip is not instant and
/// a page that re-asks inside it would be asked again, which is the exact
/// defect G1 exists to remove. If the engine refuses the write (a sealed
/// log), the next snapshot overwrites this and the prompt comes back,
/// which is the honest outcome rather than a silent divergence.
pub fn note_answer(origin: &str, kinds: &[PermissionKind], decision: PermissionDecision) {
    let Ok(mut m) = mirror().write() else { return };
    for kind in kinds {
        m.apply(&eave_core::browser::permissions::PermissionEvent::Decided {
            origin: origin.to_string(),
            permission: kind.clone(),
            decision,
            ts_ms: 0,
        });
    }
}

/// Everything the mirror holds, origin-ordered. For a surface that lists
/// what this browser has been told to remember.
pub fn grants() -> Vec<Grant> {
    mirror().read().map(|m| m.grants()).unwrap_or_default()
}

/// How many answers are held for one url's origin.
///
/// Takes a **url**, not an origin, because every caller has one: the tab
/// knows where it is, not what it is filed under. Normalizing here rather
/// than at each call site is what stops two of them disagreeing about what
/// "this site" means.
pub fn count_for(url: &str) -> usize {
    let Some(origin) = eave_wire::normalize_origin(url) else {
        return 0;
    };
    mirror()
        .read()
        .map(|m| m.grants().iter().filter(|g| g.origin == origin).count())
        .unwrap_or(0)
}

/// Drop every answer for one url's origin, locally.
///
/// The optimistic half of a revoke, and the same bargain [`note_answer`]
/// makes: the engine is told too and its snapshot replaces this. Doing it
/// here as well is what makes the menu row disappear on the click rather
/// than a round trip later.
pub fn forget_origin(url: &str) -> Option<String> {
    let origin = eave_wire::normalize_origin(url)?;
    // Taken BEFORE the clear, because Chromium's copy has to be cleared
    // per capability and after this there is nothing left to enumerate.
    let held: Vec<PermissionKind> = mirror()
        .read()
        .map(|m| {
            m.grants()
                .into_iter()
                .filter(|g| g.origin == origin)
                .map(|g| g.permission)
                .collect()
        })
        .unwrap_or_default();
    if let Ok(mut m) = mirror().write() {
        m.apply(
            &eave_core::browser::permissions::PermissionEvent::Forgotten {
                origin: origin.clone(),
                permission: None,
            },
        );
    }
    // **Chromium keeps its own durable copy, and it is the one that decides
    // whether eave is ever asked again.** `cb.cont(ACCEPT)` reaches
    // `delegate_->Accept(std::monostate{})` — CEF does not expose
    // `AcceptThisTime`, so it is always the durable branch — and
    // `PermissionContextBase::RequestPermission` then returns early on a
    // stored GRANTED/DENIED without ever reaching the prompt. Clearing only
    // eave's two stores left the site holding the permission and the prompt
    // never coming back: a revoke that did nothing and said nothing.
    super::cef_clear_permissions(&origin, &held);
    Some(origin)
}

/// Forget everything the mirror holds. Only for tests — the engine has no
/// idea this happened, so calling it anywhere else creates precisely the
/// divergence this module's docs say cannot exist.
#[cfg(test)]
fn reset() {
    if let Ok(mut m) = mirror().write() {
        *m = Permissions::new();
    }
    NEEDS_REPLAY.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These share one process-wide mirror, so they share one lock. Two
    /// tests installing different snapshots at once would each see the
    /// other's, which is a flake that only appears under `--test-threads`
    /// above one — the worst kind to debug.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = GATE.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        g
    }

    fn grant(origin: &str, permission: PermissionKind, decision: PermissionDecision) -> Grant {
        Grant {
            origin: origin.into(),
            permission,
            decision,
            ts_ms: 1,
        }
    }

    /// An empty mirror asks. This is the state at launch, before the first
    /// snapshot has arrived, and it must not read as "everything denied"
    /// or as "everything allowed".
    #[test]
    fn an_empty_mirror_asks() {
        let _g = serial();
        assert_eq!(
            verdict("https://x.test", &[PermissionKind::Camera]),
            Verdict::Ask
        );
    }

    /// A snapshot from the engine is what the handler then reads.
    #[test]
    fn an_installed_snapshot_answers_the_next_ask() {
        let _g = serial();
        install(vec![grant(
            "https://x.test",
            PermissionKind::Camera,
            PermissionDecision::Allow,
        )]);
        assert_eq!(
            verdict("https://x.test/page", &[PermissionKind::Camera]),
            Verdict::Allow
        );
        assert_eq!(
            verdict("https://other.test", &[PermissionKind::Camera]),
            Verdict::Ask
        );
    }

    /// Replacement, not merge. A revoked grant is absent from the next
    /// snapshot, and a mirror that merged would keep granting it forever.
    #[test]
    fn a_later_snapshot_replaces_rather_than_merges() {
        let _g = serial();
        install(vec![grant(
            "https://x.test",
            PermissionKind::Camera,
            PermissionDecision::Allow,
        )]);
        install(vec![]);
        assert_eq!(
            verdict("https://x.test", &[PermissionKind::Camera]),
            Verdict::Ask,
            "a revoked grant survived the snapshot that revoked it"
        );
    }

    /// The optimistic write: the answer covers the very next ask, without
    /// waiting for the engine's snapshot to come back.
    #[test]
    fn an_answer_covers_the_next_ask_before_the_engine_replies() {
        let _g = serial();
        note_answer(
            "https://x.test",
            &[PermissionKind::Camera, PermissionKind::Microphone],
            PermissionDecision::Allow,
        );
        assert_eq!(
            verdict(
                "https://x.test",
                &[PermissionKind::Camera, PermissionKind::Microphone]
            ),
            Verdict::Allow
        );
    }

    /// A refusal is remembered by the mirror too — otherwise "don't allow"
    /// is the one answer that still nags.
    #[test]
    fn a_refusal_is_mirrored_as_firmly_as_a_grant() {
        let _g = serial();
        note_answer(
            "https://ads.test",
            &[PermissionKind::Notifications],
            PermissionDecision::Deny,
        );
        assert_eq!(
            verdict("https://ads.test", &[PermissionKind::Notifications]),
            Verdict::Deny
        );
    }

    /// The menu row's two questions, asked of a URL rather than of an
    /// origin: does this site have answers, and can they be taken back.
    #[test]
    fn a_urls_answers_are_counted_and_revoked_by_origin() {
        let _g = serial();
        note_answer(
            "https://meet.test",
            &[PermissionKind::Camera, PermissionKind::Microphone],
            PermissionDecision::Allow,
        );
        assert_eq!(count_for("https://meet.test/room/7?x=1"), 2);
        assert_eq!(count_for("https://other.test/"), 0);
        // Not a url eave can file anything under, so there is nothing to
        // offer to reset.
        assert_eq!(count_for("about:blank"), 0);
        assert_eq!(forget_origin("about:blank"), None);

        assert_eq!(
            forget_origin("https://meet.test/room/7"),
            Some("https://meet.test".to_string())
        );
        assert_eq!(count_for("https://meet.test/"), 0);
        assert_eq!(
            verdict("https://meet.test", &[PermissionKind::Camera]),
            Verdict::Ask,
            "a reset site must be asked again"
        );
    }

    /// The two rules the fold enforces are still enforced through the
    /// mirror, because the mirror IS the fold: an origin with no honest
    /// key and a capability with no name are asked about every time.
    #[test]
    fn the_mirror_keeps_the_stores_two_refusals() {
        let _g = serial();
        note_answer(
            "data:text/html,x",
            &[PermissionKind::Camera],
            PermissionDecision::Allow,
        );
        assert_eq!(
            verdict("data:text/html,x", &[PermissionKind::Camera]),
            Verdict::Ask
        );
        let odd = [PermissionKind::Unknown(1 << 29)];
        note_answer("https://x.test", &odd, PermissionDecision::Allow);
        assert_eq!(verdict("https://x.test", &odd), Verdict::Ask);
        assert!(grants().is_empty());
    }
    /// G28: a snapshot landing is what makes eave's store and Chromium's
    /// disagree, so it is what has to raise the flag. Nothing else does —
    /// an answer given in this process already synced on its way out.
    #[test]
    fn a_snapshot_asks_for_a_replay() {
        let _g = serial();
        assert!(!take_replay_request(), "reset should leave it clear");
        install(vec![grant(
            "https://a.test",
            PermissionKind::Notifications,
            PermissionDecision::Allow,
        )]);
        assert!(take_replay_request());
    }

    /// Taking it clears it. A replay that ran must not run again on the
    /// next drive pass — that is a `SetContentSetting` per frame.
    #[test]
    fn taking_the_replay_request_clears_it() {
        let _g = serial();
        install(Vec::new());
        assert!(take_replay_request());
        assert!(!take_replay_request());
    }

    /// Several snapshots before anyone looks are ONE replay. The payload
    /// is the mirror, not the delta, so coalescing is the correct answer
    /// rather than a dropped update.
    #[test]
    fn snapshots_arriving_in_a_burst_ask_once() {
        let _g = serial();
        for _ in 0..5 {
            install(vec![grant(
                "https://b.test",
                PermissionKind::Geolocation,
                PermissionDecision::Deny,
            )]);
        }
        assert!(take_replay_request());
        assert!(!take_replay_request());
    }

    // ---- G2: what eave can actually deliver on ----

    /// The whole point of the row, stated as an assertion: eave must not be
    /// able to say yes to a notification while nothing can display one.
    #[test]
    fn notifications_are_not_deliverable() {
        assert!(!can_deliver(&PermissionKind::Notifications));
    }

    /// ...and the refusal is exactly one kind wide. A capability rule that
    /// quietly took the rest of the list with it would be a much larger
    /// change wearing G2's name.
    #[test]
    fn notifications_are_the_only_thing_eave_refuses() {
        assert_eq!(
            undeliverable_kinds(),
            vec![PermissionKind::Notifications],
            "the capability refusal grew or shrank; if a kind was added to \
             PermissionKind it also needs adding to all_kinds()"
        );
        // That the enumeration is the WHOLE enum is checked by the
        // compiler, in `_every_variant_is_accounted_for` — not here. A
        // length assertion against a literal compares the list to itself
        // and passes while it is incomplete, which is exactly what it did.
        assert!(
            all_kinds().iter().all(|k| k.is_storable()),
            "all_kinds picked up Unknown, which is never a real kind"
        );
    }

    /// `undeliverable` names what stopped it rather than answering yes/no,
    /// because a trace line that says `Notifications` is the difference
    /// between "eave refused on purpose" and "the handler is broken".
    #[test]
    fn undeliverable_names_the_kind_that_stopped_it() {
        assert_eq!(
            undeliverable(&[PermissionKind::Geolocation, PermissionKind::Notifications]),
            vec![PermissionKind::Notifications]
        );
        assert!(undeliverable(&[PermissionKind::Camera]).is_empty());
        assert!(undeliverable(&[]).is_empty());
    }
}
