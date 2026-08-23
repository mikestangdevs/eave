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
}

/// What to do about an ask, without disturbing anybody.
///
/// [`Verdict::Ask`] on any doubt at all — an unremembered capability, an
/// origin with no honest key, or a poisoned lock. Failing towards the
/// prompt is the only safe direction: the cost is one dialog, and the cost
/// of failing the other way is a camera.
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
}
