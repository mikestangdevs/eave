//! What a person has already answered, per origin — the memory a prompt
//! needs to be worth showing.
//!
//! # Why this is a defect being fixed and not a feature being added
//! D9 built the permission *prompt* and stopped there, so eave asked for
//! the camera every single time a page wanted it. **A prompt with no memory
//! is worse than no prompt**: it trains the operator to click the primary
//! button without reading, which is precisely the reflex every permission
//! dialog exists to prevent. The ask already carried an origin
//! (`web/dialogs.rs`); nothing wrote the answer down.
//!
//! # Why the answer lives here and not in the app
//! Because it has to survive a restart, and the app process does not own
//! anything that does. This is the same line `history.rs` and
//! `bookmarks.rs` sit on: the engine is where a fact goes when losing it
//! would be losing a decision somebody made. The app keeps a **mirror** of
//! this state (`eave-app/src/web/permissions.rs`) because CEF asks its
//! question synchronously, on the app's own stack, and a round trip to
//! another process is not available inside that callback — but the mirror
//! is a copy of this, never the record.
//!
//! # The four rules, and why each one is a rule
//!
//! 1. **A grant is keyed by ORIGIN, not by tab, page or url.** Scheme,
//!    host and non-default port and nothing else — a permission granted at
//!    `https://example.com/settings` is granted to `https://example.com`,
//!    which is how every other browser behaves and is what a person means
//!    when they say "allow this site".
//! 2. **The scheme is part of the key.** `http://example.com` and
//!    `https://example.com` are different origins, so an answer given to
//!    the secure one must never be replayed to a plaintext impostor.
//! 3. **An origin eave cannot name is never remembered.** A `data:` URL, a
//!    sandboxed frame, an empty origin — anything [`normalize_origin`]
//!    rejects prompts every time and stores nothing. There is no key that
//!    would be honest, so there is no row.
//! 4. **An unrecognised capability is never remembered.** CEF hands over a
//!    bitmask; a bit this build has no word for is still a real capability
//!    and it is not eave's to grant silently on a person's behalf next
//!    time. [`PermissionKind::Unknown`] exists so the prompt can say so,
//!    and [`PermissionKind::is_storable`] is what keeps it off disk.
//!
//! # Revoking rewrites rather than appends
//! Like `history.rs` and unlike the other two logs. "I took the camera away
//! from that site" has to still be true after a power cut, and a tombstone
//! appended after the grant would leave the grant itself sitting in the
//! file — which is the wrong answer to "what has this browser been told to
//! remember about me?"

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// One capability a page can ask for.
///
/// A closed vocabulary plus an escape hatch. The closed part is what can be
/// written down; [`PermissionKind::Unknown`] is what lets an ask still be
/// *phrased* when a future Chromium invents a capability this build has
/// never heard of.
///
/// The bit-to-kind mapping is deliberately NOT here — it needs CEF's
/// constants, and this crate must not depend on Chromium. It lives in
/// `eave-app/src/web/dialogs.rs`, which is the one place that already has
/// them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionKind {
    Camera,
    Microphone,
    Geolocation,
    Notifications,
    Clipboard,
    MidiSysex,
    PointerLock,
    KeyboardLock,
    WindowManagement,
    LocalFonts,
    IdleDetection,
    StorageAccess,
    LocalNetwork,
    VrSession,
    ArSession,
    /// Whole-desktop audio, which is not the microphone.
    ScreenAudio,
    /// Screen capture, which is not the camera.
    ScreenVideo,
    /// A bit this build has no word for. Carries the raw mask so the
    /// prompt can at least be specific about its ignorance.
    ///
    /// **Never persisted** — see [`PermissionKind::is_storable`].
    Unknown(u32),
}

impl PermissionKind {
    /// The wire and on-disk spelling. Stable: renaming one of these
    /// strings silently forgets every grant that used it.
    pub fn as_str(&self) -> String {
        match self {
            Self::Camera => "camera".into(),
            Self::Microphone => "microphone".into(),
            Self::Geolocation => "geolocation".into(),
            Self::Notifications => "notifications".into(),
            Self::Clipboard => "clipboard".into(),
            Self::MidiSysex => "midi_sysex".into(),
            Self::PointerLock => "pointer_lock".into(),
            Self::KeyboardLock => "keyboard_lock".into(),
            Self::WindowManagement => "window_management".into(),
            Self::LocalFonts => "local_fonts".into(),
            Self::IdleDetection => "idle_detection".into(),
            Self::StorageAccess => "storage_access".into(),
            Self::LocalNetwork => "local_network".into(),
            Self::VrSession => "vr_session".into(),
            Self::ArSession => "ar_session".into(),
            Self::ScreenAudio => "screen_audio".into(),
            Self::ScreenVideo => "screen_video".into(),
            Self::Unknown(bits) => format!("unknown_{bits:#x}"),
        }
    }

    /// The inverse of [`PermissionKind::as_str`]. Total: a spelling this
    /// build does not know reads back as an [`PermissionKind::Unknown`]
    /// with no bits, which is unstorable and therefore inert.
    pub fn parse(s: &str) -> Self {
        match s {
            "camera" => Self::Camera,
            "microphone" => Self::Microphone,
            "geolocation" => Self::Geolocation,
            "notifications" => Self::Notifications,
            "clipboard" => Self::Clipboard,
            "midi_sysex" => Self::MidiSysex,
            "pointer_lock" => Self::PointerLock,
            "keyboard_lock" => Self::KeyboardLock,
            "window_management" => Self::WindowManagement,
            "local_fonts" => Self::LocalFonts,
            "idle_detection" => Self::IdleDetection,
            "storage_access" => Self::StorageAccess,
            "local_network" => Self::LocalNetwork,
            "vr_session" => Self::VrSession,
            "ar_session" => Self::ArSession,
            "screen_audio" => Self::ScreenAudio,
            "screen_video" => Self::ScreenVideo,
            other => Self::Unknown(
                other
                    .strip_prefix("unknown_0x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .unwrap_or(0),
            ),
        }
    }

    /// The words a person reads before deciding.
    ///
    /// Named individually rather than printed as a bitmask because this
    /// string *is* the question. "camera and microphone" is a question;
    /// `0x480` is not.
    pub fn label(&self) -> String {
        match self {
            Self::Camera => "camera".into(),
            Self::Microphone => "microphone".into(),
            Self::Geolocation => "your location".into(),
            Self::Notifications => "notifications".into(),
            Self::Clipboard => "the clipboard".into(),
            Self::MidiSysex => "MIDI devices".into(),
            Self::PointerLock => "the pointer".into(),
            Self::KeyboardLock => "the keyboard".into(),
            Self::WindowManagement => "your windows".into(),
            Self::LocalFonts => "your fonts".into(),
            Self::IdleDetection => "whether you are idle".into(),
            Self::StorageAccess => "cross-site storage".into(),
            Self::LocalNetwork => "your local network".into(),
            Self::VrSession => "virtual reality".into(),
            Self::ArSession => "augmented reality".into(),
            Self::ScreenAudio => "system audio".into(),
            Self::ScreenVideo => "your screen".into(),
            Self::Unknown(bits) => format!("an unrecognised capability ({bits:#x})"),
        }
    }

    /// Whether an answer about this may be written down.
    ///
    /// False for exactly one variant, and rule 4 of this module is the
    /// whole reason it exists: remembering "allow" for a capability eave
    /// cannot name would be granting, forever and silently, something
    /// nobody was ever able to read.
    pub fn is_storable(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl Serialize for PermissionKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for PermissionKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = PermissionKind;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a permission name")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
                Ok(PermissionKind::parse(s))
            }
        }
        d.deserialize_str(V)
    }
}

/// What was answered. Two values, and **both are remembered** — Chrome,
/// Firefox and Safari all persist a refusal, and a "no" that has to be
/// repeated on every page load is the same nag the "yes" case fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    pub fn allowed(self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// The durable vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionEvent {
    /// Somebody answered.
    Decided {
        origin: String,
        permission: PermissionKind,
        decision: Decision,
        ts_ms: u64,
    },
    /// Take an answer back. `permission: None` forgets everything this
    /// origin was ever answered.
    Forgotten {
        origin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission: Option<PermissionKind>,
    },
    /// Forget every origin.
    Cleared,
}

impl PermissionEvent {
    /// Whether applying this must rewrite the log rather than extend it.
    /// Both ways of forgetting must actually remove the grant from disk —
    /// see the module docs.
    pub fn requires_compaction(&self) -> bool {
        matches!(self, Self::Forgotten { .. } | Self::Cleared)
    }
}

/// One answer, flattened for the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Grant {
    pub origin: String,
    pub permission: PermissionKind,
    pub decision: Decision,
    pub ts_ms: u64,
}

/// Every answer this browser has been given, folded.
///
/// `BTreeMap` and not `HashMap` on purpose: [`Permissions::grants`] is sent
/// on the wire and rewritten to disk, and a map that reorders itself
/// between two identical states would make every snapshot look like a
/// change and every compaction rewrite the file differently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    by_origin: BTreeMap<String, BTreeMap<PermissionKind, (Decision, u64)>>,
}

/// What to do about an ask, before anybody is disturbed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No stored answer covers this. Show the prompt.
    Ask,
    /// Every capability in the ask is allowed. Grant without asking.
    Allow,
    /// Every capability is answered and at least one is refused.
    Deny,
}

impl Permissions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event. Returns whether anything changed — a no-op is
    /// neither written nor broadcast, exactly as in the other three stores.
    pub fn apply(&mut self, event: &PermissionEvent) -> bool {
        match event {
            PermissionEvent::Decided {
                origin,
                permission,
                decision,
                ts_ms,
            } => {
                // Rules 3 and 4, enforced at the fold rather than at the
                // caller — so a hand-edited file cannot inject a grant for
                // an origin or a capability eave would refuse to mint.
                if !permission.is_storable() {
                    return false;
                }
                let Some(origin) = normalize_origin(origin) else {
                    return false;
                };
                let slot = self.by_origin.entry(origin).or_default();
                if slot.get(permission).map(|(d, _)| *d) == Some(*decision) {
                    // Re-answering the same way is not a change. Without
                    // this every page load that re-asks would append a
                    // line and re-broadcast the whole snapshot.
                    return false;
                }
                slot.insert(permission.clone(), (*decision, *ts_ms));
                true
            }
            PermissionEvent::Forgotten { origin, permission } => {
                let Some(origin) = normalize_origin(origin) else {
                    return false;
                };
                let Some(slot) = self.by_origin.get_mut(&origin) else {
                    return false;
                };
                let changed = match permission {
                    Some(p) => slot.remove(p).is_some(),
                    None => !std::mem::take(slot).is_empty(),
                };
                if slot.is_empty() {
                    self.by_origin.remove(&origin);
                }
                changed
            }
            PermissionEvent::Cleared => !std::mem::take(&mut self.by_origin).is_empty(),
        }
    }

    /// What was answered for one origin and one capability.
    pub fn get(&self, origin: &str, permission: &PermissionKind) -> Option<Decision> {
        let origin = normalize_origin(origin)?;
        self.by_origin
            .get(&origin)?
            .get(permission)
            .map(|(d, _)| *d)
    }

    /// What to do about an ask covering `kinds` from `origin`.
    ///
    /// **All or nothing, and it prompts on any doubt.** An ask that names
    /// three capabilities of which two are remembered is still shown,
    /// because CEF's prompt path takes one answer for the whole request
    /// and silently granting the remembered two while denying the third
    /// would be answering a question nobody was asked. The same reasoning
    /// makes an empty ask an [`Verdict::Ask`] rather than a grant.
    pub fn verdict(&self, origin: &str, kinds: &[PermissionKind]) -> Verdict {
        if kinds.is_empty() {
            return Verdict::Ask;
        }
        let Some(origin) = normalize_origin(origin) else {
            return Verdict::Ask;
        };
        let Some(slot) = self.by_origin.get(&origin) else {
            return Verdict::Ask;
        };
        let mut all_allowed = true;
        for kind in kinds {
            match slot.get(kind) {
                None => return Verdict::Ask,
                Some((Decision::Deny, _)) => all_allowed = false,
                Some((Decision::Allow, _)) => {}
            }
        }
        if all_allowed {
            Verdict::Allow
        } else {
            Verdict::Deny
        }
    }

    /// Every answer, origin-ordered then capability-ordered. What crosses
    /// the wire, and what a compaction writes back.
    pub fn grants(&self) -> Vec<Grant> {
        self.by_origin
            .iter()
            .flat_map(|(origin, slot)| {
                slot.iter()
                    .map(move |(permission, (decision, ts_ms))| Grant {
                        origin: origin.clone(),
                        permission: permission.clone(),
                        decision: *decision,
                        ts_ms: *ts_ms,
                    })
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.by_origin.is_empty()
    }

    /// How many answers are held, across every origin.
    pub fn len(&self) -> usize {
        self.by_origin.values().map(BTreeMap::len).sum()
    }

    /// The log that would reproduce this state — one `Decided` per grant,
    /// with every forget already applied rather than recorded.
    pub fn retained(&self) -> Vec<PermissionEvent> {
        self.grants()
            .into_iter()
            .map(|g| PermissionEvent::Decided {
                origin: g.origin,
                permission: g.permission,
                decision: g.decision,
                ts_ms: g.ts_ms,
            })
            .collect()
    }

    /// Rebuild from what came over the wire.
    pub fn from_grants(grants: impl IntoIterator<Item = Grant>) -> Self {
        let mut me = Self::new();
        for g in grants {
            me.apply(&PermissionEvent::Decided {
                origin: g.origin,
                permission: g.permission,
                decision: g.decision,
                ts_ms: g.ts_ms,
            });
        }
        me
    }
}

/// The key an answer is filed under, or `None` when there is not an honest
/// one.
///
/// Scheme, host, and the port only when it is not the scheme's default —
/// the WHATWG serialization of an origin, which is what "this site" means
/// everywhere else. Everything after the authority is dropped: a
/// permission is not per-page.
///
/// Returns `None` for anything that cannot be named that way, and that is
/// the security-relevant half of this function rather than a tidiness one:
///
/// - **an empty origin** — a sandboxed frame, or CEF declining to say;
/// - **`data:`, `blob:`, `about:`, `javascript:`** and every other scheme
///   that is not `http`, `https` or `file`, because their "origin" is
///   either opaque or the content itself;
/// - **userinfo** (`https://user:pw@host`) — not rejected but *stripped*,
///   because a key with a password in it is a secret written to a file
///   that exists to be read;
/// - **a host that is empty after all that.**
///
/// `file://` normalizes to one origin for every local file, which is a
/// real decision and a coarse one: it matches what Chromium does by
/// default, and the alternative — a grant per path — makes a permission
/// store that grows without bound and still surprises people.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "file" => return Some("file://".to_string()),
        "http" | "https" => {}
        _ => return None,
    }

    // Authority only: everything from the first path, query or fragment
    // separator is not part of an origin.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Userinfo is dropped, not kept — see the doc comment.
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }

    // An IPv6 literal carries colons of its own, so the port separator is
    // the one AFTER the closing bracket and nowhere else.
    let (host, port) = match authority.rfind(']') {
        Some(close) => match authority[close + 1..].strip_prefix(':') {
            Some(p) => (&authority[..=close], Some(p)),
            None => (authority, None),
        },
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() || host == "[]" {
        return None;
    }

    let default_port = if scheme == "https" { "443" } else { "80" };
    match port {
        // A port that is not a number is not a port, and guessing which
        // origin was meant is how two different sites end up sharing one
        // row.
        Some(p) if p.parse::<u16>().is_err() => None,
        Some(p) if p != default_port => Some(format!("{scheme}://{host}:{p}")),
        _ => Some(format!("{scheme}://{host}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(origin: &str, permission: PermissionKind, decision: Decision) -> PermissionEvent {
        PermissionEvent::Decided {
            origin: origin.into(),
            permission,
            decision,
            ts_ms: 1,
        }
    }

    /// The defect this module exists to fix, stated as a test: an answer
    /// given once covers the next ask.
    #[test]
    fn an_answer_covers_the_next_ask() {
        let mut p = Permissions::new();
        assert_eq!(
            p.verdict("https://example.com", &[PermissionKind::Camera]),
            Verdict::Ask,
            "nothing is remembered yet, so it must ask"
        );
        assert!(p.apply(&decided(
            "https://example.com/settings?x=1",
            PermissionKind::Camera,
            Decision::Allow
        )));
        assert_eq!(
            p.verdict("https://example.com/other/page", &[PermissionKind::Camera]),
            Verdict::Allow,
            "a permission is per origin, not per page"
        );
    }

    /// A refusal is remembered too. Anything else is the same nag wearing
    /// the opposite answer.
    #[test]
    fn a_refusal_is_remembered_as_firmly_as_a_grant() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://ads.test",
            PermissionKind::Notifications,
            Decision::Deny,
        ));
        assert_eq!(
            p.verdict("https://ads.test", &[PermissionKind::Notifications]),
            Verdict::Deny
        );
    }

    /// Rule 2. An answer given to the secure origin must not be replayed
    /// to a plaintext one wearing the same host.
    #[test]
    fn a_grant_does_not_cross_the_scheme() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://bank.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        assert_eq!(
            p.verdict("http://bank.test", &[PermissionKind::Camera]),
            Verdict::Ask
        );
    }

    /// A port is part of the key, and the DEFAULT port is not — so
    /// `https://x` and `https://x:443` are one site and `https://x:8443`
    /// is another.
    #[test]
    fn the_default_port_is_the_same_origin_and_any_other_is_not() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://x.test:443",
            PermissionKind::Geolocation,
            Decision::Allow,
        ));
        assert_eq!(
            p.verdict("https://x.test", &[PermissionKind::Geolocation]),
            Verdict::Allow
        );
        assert_eq!(
            p.verdict("https://x.test:8443", &[PermissionKind::Geolocation]),
            Verdict::Ask
        );
    }

    /// Rule 3, at the fold: an origin with no honest key is never written
    /// down, so it is asked about every time.
    #[test]
    fn an_unnameable_origin_is_never_remembered() {
        let mut p = Permissions::new();
        for origin in [
            "",
            "data:text/html,<b>hi",
            "about:blank",
            "javascript:0",
            "https://",
            "https://user@",
        ] {
            assert!(
                !p.apply(&decided(origin, PermissionKind::Camera, Decision::Allow)),
                "{origin} was stored"
            );
            assert_eq!(
                p.verdict(origin, &[PermissionKind::Camera]),
                Verdict::Ask,
                "{origin} answered without asking"
            );
        }
        assert!(p.is_empty());
    }

    /// Rule 4. eave will not silently re-grant something it has no word
    /// for, so the bit never reaches disk and the ask always happens.
    #[test]
    fn an_unrecognised_capability_is_never_remembered() {
        let mut p = Permissions::new();
        let odd = PermissionKind::Unknown(1 << 30);
        assert!(!odd.is_storable());
        assert!(!p.apply(&decided("https://x.test", odd.clone(), Decision::Allow)));
        assert_eq!(p.verdict("https://x.test", &[odd]), Verdict::Ask);
    }

    /// The whole ask, or none of it. Two remembered capabilities plus one
    /// new one is still a question.
    #[test]
    fn a_partly_remembered_ask_is_still_asked() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        assert_eq!(
            p.verdict(
                "https://x.test",
                &[PermissionKind::Camera, PermissionKind::Microphone]
            ),
            Verdict::Ask
        );
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Microphone,
            Decision::Allow,
        ));
        assert_eq!(
            p.verdict(
                "https://x.test",
                &[PermissionKind::Camera, PermissionKind::Microphone]
            ),
            Verdict::Allow
        );
    }

    /// One refusal in a grouped ask refuses the group. Granting the rest
    /// would be answering a question nobody was shown.
    #[test]
    fn one_refusal_refuses_the_whole_group() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Microphone,
            Decision::Deny,
        ));
        assert_eq!(
            p.verdict(
                "https://x.test",
                &[PermissionKind::Camera, PermissionKind::Microphone]
            ),
            Verdict::Deny
        );
    }

    /// An ask that names nothing is not a grant of everything.
    #[test]
    fn an_empty_ask_is_asked() {
        let p = Permissions::new();
        assert_eq!(p.verdict("https://x.test", &[]), Verdict::Ask);
    }

    /// Answering the same way twice is not a change, so it neither writes
    /// a line nor re-broadcasts a snapshot. Changing the answer IS.
    #[test]
    fn re_answering_the_same_way_changes_nothing() {
        let mut p = Permissions::new();
        assert!(p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Allow
        )));
        assert!(!p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Allow
        )));
        assert!(p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Deny
        )));
        assert_eq!(
            p.verdict("https://x.test", &[PermissionKind::Camera]),
            Verdict::Deny
        );
    }

    /// Forgetting one capability leaves the others, and forgetting the
    /// last one takes the origin's row away rather than leaving an empty
    /// shell in the snapshot.
    #[test]
    fn forgetting_one_capability_leaves_the_rest() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        p.apply(&decided(
            "https://x.test",
            PermissionKind::Microphone,
            Decision::Allow,
        ));
        assert!(p.apply(&PermissionEvent::Forgotten {
            origin: "https://x.test".into(),
            permission: Some(PermissionKind::Camera),
        }));
        assert_eq!(
            p.verdict("https://x.test", &[PermissionKind::Camera]),
            Verdict::Ask
        );
        assert_eq!(
            p.verdict("https://x.test", &[PermissionKind::Microphone]),
            Verdict::Allow
        );
        assert!(p.apply(&PermissionEvent::Forgotten {
            origin: "https://x.test".into(),
            permission: None,
        }));
        assert!(p.is_empty(), "an emptied origin left a row behind");
    }

    /// Forgetting something that was never answered is a no-op, not an
    /// error and not a write.
    #[test]
    fn forgetting_nothing_changes_nothing() {
        let mut p = Permissions::new();
        assert!(!p.apply(&PermissionEvent::Forgotten {
            origin: "https://x.test".into(),
            permission: None,
        }));
        assert!(!p.apply(&PermissionEvent::Cleared));
    }

    /// Both ways of forgetting rewrite rather than append, which is what
    /// makes the grant actually leave the disk.
    #[test]
    fn every_forget_requires_a_rewrite() {
        assert!(PermissionEvent::Cleared.requires_compaction());
        assert!(PermissionEvent::Forgotten {
            origin: "https://x.test".into(),
            permission: None,
        }
        .requires_compaction());
        assert!(
            !decided("https://x.test", PermissionKind::Camera, Decision::Allow)
                .requires_compaction()
        );
    }

    /// A compaction has to reproduce the state it compacted, or a revoke
    /// silently takes unrelated grants with it.
    #[test]
    fn the_retained_log_reproduces_the_state() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://a.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        p.apply(&decided(
            "https://b.test",
            PermissionKind::Notifications,
            Decision::Deny,
        ));
        p.apply(&decided(
            "https://a.test",
            PermissionKind::Microphone,
            Decision::Allow,
        ));
        p.apply(&PermissionEvent::Forgotten {
            origin: "https://a.test".into(),
            permission: Some(PermissionKind::Microphone),
        });

        let mut rebuilt = Permissions::new();
        for e in p.retained() {
            rebuilt.apply(&e);
        }
        assert_eq!(rebuilt, p);
        assert_eq!(p.len(), 2);
    }

    /// The wire round-trip, which is the app's mirror in miniature.
    #[test]
    fn the_wire_round_trip_preserves_every_answer() {
        let mut p = Permissions::new();
        p.apply(&decided(
            "https://a.test",
            PermissionKind::Camera,
            Decision::Allow,
        ));
        p.apply(&decided(
            "https://b.test:8443",
            PermissionKind::Geolocation,
            Decision::Deny,
        ));
        assert_eq!(Permissions::from_grants(p.grants()), p);
    }

    /// Every spelling survives a trip through the log, because a renamed
    /// one is a forgotten grant.
    #[test]
    fn every_permission_name_round_trips() {
        let all = [
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
            PermissionKind::Unknown(0x4000_0000),
        ];
        for k in &all {
            assert_eq!(&PermissionKind::parse(&k.as_str()), k, "{k:?}");
            let json = serde_json::to_string(k).unwrap();
            assert_eq!(&serde_json::from_str::<PermissionKind>(&json).unwrap(), k);
            assert!(!k.label().is_empty());
        }
        // A name from a build that is not this one reads back inert
        // rather than as a wrong capability.
        assert_eq!(
            PermissionKind::parse("brain_interface"),
            PermissionKind::Unknown(0)
        );
        assert!(!PermissionKind::parse("brain_interface").is_storable());
    }

    /// The origin key, one case per rule in the doc comment.
    #[test]
    fn an_origin_is_scheme_host_and_a_non_default_port() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://Example.COM/a/b?c#d", Some("https://example.com")),
            ("http://example.com:80", Some("http://example.com")),
            ("http://example.com:8080", Some("http://example.com:8080")),
            ("https://example.com:443/", Some("https://example.com")),
            // Userinfo is stripped, never keyed on.
            ("https://user:pw@example.com/x", Some("https://example.com")),
            // IPv6: the port separator is the colon after the bracket.
            ("http://[::1]:8000/", Some("http://[::1]:8000")),
            ("http://[::1]/", Some("http://[::1]")),
            ("http://[::1]:80/", Some("http://[::1]")),
            ("file:///Users/x/page.html", Some("file://")),
            ("  https://example.com  ", Some("https://example.com")),
            ("", None),
            ("example.com", None),
            ("about:blank", None),
            ("data:text/html,x", None),
            ("blob:https://example.com/uuid", None),
            ("chrome://settings", None),
            ("https://", None),
            ("http://example.com:notaport", None),
        ];
        for (raw, want) in cases {
            assert_eq!(
                normalize_origin(raw).as_deref(),
                *want,
                "normalize_origin({raw:?})"
            );
        }
    }
}
