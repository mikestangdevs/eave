//! What a page asks a person, what they answered, and the one rule that
//! decides whether the answer is written down.
//!
//! # Why this is its own module, and why it is UNGATED
//! [`Ask`], [`Answer`] and [`Pending`] used to exist **twice** — once in
//! `dialogs.rs` behind `--features web`, and once again in `mod.rs`'s
//! feature-off half as a hand-copied twin. Two definitions of the same
//! vocabulary is the shape that lets them drift, and one of them was
//! already load-bearing for a bug: `06-browser.md` G26 records that the
//! feature-off `pending()` returns `None` unconditionally, so **nothing
//! about the ask surface could be asserted by `cargo test`** and G25's
//! modality fix shipped code-verified only.
//!
//! Slice E1 solved the same problem for find and zoom by putting the
//! *state* outside the gate and leaving only the calls into Chromium
//! inside it. This is that, for asks. The types and the decision rule live
//! here and compile in both feature states; `dialogs.rs` keeps the CEF
//! callbacks and nothing else.
//!
//! # The rule (G27)
//! **A dismissal is not a denial.** See [`decision_of`].

use super::permissions::PermissionKind;

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
    /// HTTP basic/digest auth, or a proxy demanding the same (D7).
    ///
    /// `realm` is the server's own words for what is being protected and is
    /// shown verbatim when it exists — it is the only thing distinguishing
    /// two password prompts from the same host. It is also **attacker
    /// controlled**, which is why it is carried as data and rendered as
    /// text, never interpolated into eave's own sentence.
    Credentials {
        host: String,
        realm: Option<String>,
        /// A proxy asking, not the site. Worth saying out loud: the
        /// password goes to a different party than the address bar names.
        is_proxy: bool,
    },
    /// The TLS certificate did not check out (D8).
    ///
    /// `reason` is Chromium's own error code, named. Carried rather than
    /// phrased for the same reason `Ask::Permission` carries kinds.
    Certificate { url: String, reason: String },
}

impl Ask {
    /// Whether this ask can be refused. An `alert()` cannot — it has one
    /// button — and a surface that drew a Cancel beside it would be
    /// offering a choice the page will never hear about.
    pub fn is_refusable(&self) -> bool {
        !matches!(self, Ask::Alert { .. })
    }

    /// Whether answering this ask writes a standing decision down.
    ///
    /// Only permission asks do. `alert()` and `<input type=file>` are
    /// events, not standing decisions, and a browser that remembered "you
    /// said OK to an alert once" would be remembering nothing.
    pub fn is_a_standing_decision(&self) -> bool {
        matches!(self, Ask::Permission { .. })
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
///
/// **Three states, not two.** The doc comment this replaces read
/// "Cancel / no / deny / stay" on a single variant, and that conflation
/// was the bug: see [`decision_of`].
///
/// `Debug` is hand-written, not derived — see [`Answer::Credentials`].
#[derive(Clone, PartialEq)]
pub enum Answer {
    /// OK / yes / **allow** / leave. An explicit decision.
    Ok,
    /// No / **don't allow** / stay. An explicit decision, and for a
    /// permission a durable one — the person read the prompt and refused.
    Cancel,
    /// **No decision was made.** Escape, or the ask going away with the
    /// browser. The page is told no, because something has to be said and
    /// there is no third answer on the web platform — but nothing is
    /// written down and the next ask is asked again.
    Dismiss,
    /// A `prompt()`'s text.
    Text(String),
    /// Chosen files, as paths.
    Files(Vec<String>),
    /// A username and a password, for [`Ask::Credentials`] (D7).
    ///
    /// **Never logged, never traced, never written to the permission
    /// store.** `Debug` is implemented by hand below so that a stray
    /// `{answer:?}` — and `dialogs::answer` has one — cannot put a
    /// password in a terminal or a crash report.
    Credentials { user: String, password: String },
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Ok => write!(f, "Ok"),
            Answer::Cancel => write!(f, "Cancel"),
            Answer::Dismiss => write!(f, "Dismiss"),
            // The TEXT is elided too. `prompt()` is where a page asks for
            // a passphrase when it has no auth realm to lean on, and the
            // trace line in `dialogs::answer` prints every answer.
            Answer::Text(t) => write!(f, "Text(<{} chars>)", t.chars().count()),
            Answer::Files(p) => write!(f, "Files({} paths)", p.len()),
            Answer::Credentials { user, .. } => {
                write!(
                    f,
                    "Credentials(user {} chars, password elided)",
                    user.chars().count()
                )
            }
        }
    }
}

/// Whether this answer is a standing decision, and which way (G27).
///
/// `None` means **do not write anything down.**
///
/// # The bug this function is
/// `dialogs.rs` used to record every permission answer as
/// `matches!(answer, Answer::Ok)` — so *everything that was not Ok* became
/// a durable **Deny**, and `ui.rs` mapped **escape to `Answer::Cancel`**.
/// One Escape on a prompt and that capability was denied for that origin
/// for ever, in `permissions.jsonl` and in Chromium's own content settings,
/// from a gesture that meant "not now".
///
/// Chrome has three states here and eave had two. Chromium's own model is
/// in `components/permissions/permission_decision_auto_blocker.cc`: a
/// dismissal is recorded but **not** durable, and only after
/// `kDefaultDismissalsBeforeBlock` (3) consecutive dismissals of the same
/// capability by the same origin does it embargo. Replying
/// `PermissionRequestResult::DISMISS` rather than `DENY` is what puts eave
/// on that path instead of writing the block itself.
///
/// `Text` and `Files` are answers to asks that are not standing decisions
/// at all, so they are `None` for the same reason and never reach this
/// with a permission on the other end.
pub fn decision_of(answer: &Answer) -> Option<bool> {
    match answer {
        Answer::Ok => Some(true),
        Answer::Cancel => Some(false),
        Answer::Dismiss
        | Answer::Text(_)
        | Answer::Files(_)
        // Credentials answer an ask that is not a standing decision. eave
        // does not have a password store and this slice does not add one:
        // the answer goes to Chromium and nowhere else.
        | Answer::Credentials { .. } => None,
    }
}

/// Whether the page should be told its ask succeeded.
///
/// Separate from [`decision_of`] because they genuinely differ on
/// [`Answer::Dismiss`]: the page hears "no" (there is nothing else to say
/// to a `confirm()`), and eave writes nothing down.
pub fn page_hears_yes(answer: &Answer) -> bool {
    matches!(
        answer,
        Answer::Ok | Answer::Text(_) | Answer::Files(_) | Answer::Credentials { .. }
    )
}

/// How a password is drawn (D7).
///
/// One bullet per character and nothing else — not the length in words,
/// not a partial reveal, not the first letter. `•` (U+2022) is what every
/// other browser uses, and matching it matters because a field that looks
/// different from a password field is one people type their password into
/// while it is not one.
///
/// A function rather than an inline `repeat` in `ui.rs` so that the rule
/// is stated once and can be asserted without a window: the render calls
/// this and nothing else.
pub fn masked(secret: &str) -> String {
    "•".repeat(secret.chars().count())
}

// ---------------------------------------------------------------------------
// The map (G26)
// ---------------------------------------------------------------------------

thread_local! {
    /// One outstanding ask per tab.
    ///
    /// # Why the MAP is out here and the CALLBACKS are not
    /// `06-browser.md` G26: the whole ask surface used to live behind
    /// `--features web`, so `pending()` returned `None` unconditionally in the
    /// build the suite runs and **no test could put a dialog on screen.**
    /// G25's modality fix — the missing `.occlude()` that let clicks fall
    /// through a permission prompt into the live page — shipped code-verified
    /// only, and D7's password masking and D8's deliberately-swapped buttons
    /// would have shipped the same way.
    ///
    /// The split is along the seam that already existed: an ask is **data**
    /// and a reply is a **CEF callback**. The data lives here and compiles in
    /// both states; `dialogs::REPLIES` holds the callbacks and stays gated. So
    /// the harness can raise an ask, render it, click it and assert on it,
    /// while nothing about Chromium's lifetime rules moved.
    ///
    /// One ask per tab, which is Chromium's own model — it serialises dialogs
    /// per frame and will not raise a second while one is outstanding.
    ///
    /// `thread_local!` and not a `static`: `#[gpui::test]` runs each test on
    /// its own thread inside one process, so a process-wide map would let one
    /// test's dialog appear in another's window. (This is the opposite of
    /// `permissions.rs`, whose mirror is written by a tokio task and therefore
    /// cannot be thread-local — the two modules differ because their writers
    /// do.)
    static PENDING: std::cell::RefCell<std::collections::HashMap<String, Pending>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// What this tab is stopped on, if anything.
pub fn pending(tab_id: &str) -> Option<Pending> {
    PENDING.with(|p| p.borrow().get(tab_id).cloned())
}

/// Whether any tab is stopped on an ask.
pub fn any_pending() -> bool {
    PENDING.with(|p| !p.borrow().is_empty())
}

/// Record a raised ask. Returns whatever it displaced, which the caller
/// must answer — a stranded callback is a renderer stopped forever.
pub fn note_raised(ask: Pending) -> Option<Pending> {
    PENDING.with(|p| p.borrow_mut().insert(ask.tab_id.clone(), ask))
}

/// Take a tab's ask off the screen. `None` when there was none, which is
/// what makes a double-answer a no-op rather than a crash in Chromium.
pub fn take(tab_id: &str) -> Option<Pending> {
    PENDING.with(|p| p.borrow_mut().remove(tab_id))
}

/// Every certificate failure eave names (D8).
///
/// **One list, two consumers.** `tabs::cert_error_reason` maps CEF's
/// `Errorcode` onto exactly these strings and
/// [`certificate_explanation`] turns exactly these strings into words —
/// so a code added to one side and forgotten on the other is a test
/// failure rather than a silent fallback sentence in front of a person
/// deciding whether they are being attacked.
pub const CERTIFICATE_REASONS: &[&str] = &[
    "ERR_CERT_COMMON_NAME_INVALID",
    "ERR_CERT_DATE_INVALID",
    "ERR_CERT_AUTHORITY_INVALID",
    "ERR_CERT_CONTAINS_ERRORS",
    "ERR_CERT_NO_REVOCATION_MECHANISM",
    "ERR_CERT_UNABLE_TO_CHECK_REVOCATION",
    "ERR_CERT_REVOKED",
    "ERR_CERT_INVALID",
    "ERR_CERT_WEAK_SIGNATURE_ALGORITHM",
    "ERR_CERT_NON_UNIQUE_NAME",
    "ERR_CERT_WEAK_KEY",
    "ERR_CERT_NAME_CONSTRAINT_VIOLATION",
    "ERR_CERT_VALIDITY_TOO_LONG",
    "ERR_CERTIFICATE_TRANSPARENCY_REQUIRED",
    "ERR_CERT_KNOWN_INTERCEPTION_BLOCKED",
    "ERR_CERT_SELF_SIGNED_LOCAL_NETWORK",
];

/// What a certificate failure means, in words, for the interstitial (D8).
///
/// Keyed by Chromium's `ERR_` name so the two halves cannot drift: the CEF
/// side (`tabs::cert_error_reason`) produces exactly these strings and
/// nothing else. Ungated so `cargo test` covers it.
///
/// # Why the wording matters more than usual
/// Every one of these is a sentence the operator will decide on. Chrome's
/// own research is that people click through interstitials they do not
/// understand, so each says **what is wrong** rather than what failed —
/// "this certificate was issued for a different site" beats "common name
/// invalid" for someone deciding whether they are being attacked.
///
/// The fallback is deliberately the *suspicious* reading, not a shrug: an
/// unrecognised code is still a failed check.
pub fn certificate_explanation(reason: &str) -> &'static str {
    match reason {
        "ERR_CERT_COMMON_NAME_INVALID" => {
            "This certificate was issued for a different site, so it does not \
             prove you are talking to this one."
        }
        "ERR_CERT_DATE_INVALID" => {
            "This certificate has expired, or is not valid yet. That can be a \
             misconfigured server — or a clock that is wrong."
        }
        "ERR_CERT_AUTHORITY_INVALID" | "ERR_CERT_SELF_SIGNED_LOCAL_NETWORK" => {
            "Nobody eave trusts vouched for this certificate. Self-signed \
             certificates look exactly like an interception."
        }
        "ERR_CERT_REVOKED" => {
            "This certificate was revoked by whoever issued it. That usually \
             means its private key was compromised."
        }
        "ERR_CERT_KNOWN_INTERCEPTION_BLOCKED" => {
            "This connection is being intercepted by software known to read \
             encrypted traffic."
        }
        "ERR_CERT_WEAK_SIGNATURE_ALGORITHM" | "ERR_CERT_WEAK_KEY" => {
            "This certificate uses cryptography that is no longer strong \
             enough to rely on."
        }
        "ERR_CERTIFICATE_TRANSPARENCY_REQUIRED" => {
            "This certificate was not published where it could be publicly \
             audited, which is required for a site like this one."
        }
        "ERR_CERT_UNABLE_TO_CHECK_REVOCATION" | "ERR_CERT_NO_REVOCATION_MECHANISM" => {
            "eave could not check whether this certificate has been revoked."
        }
        "ERR_CERT_VALIDITY_TOO_LONG" => {
            "This certificate is valid for longer than the rules allow, which \
             is why it is not trusted."
        }
        "ERR_CERT_NAME_CONSTRAINT_VIOLATION" => {
            "Whoever issued this certificate was not permitted to issue it for \
             this site."
        }
        "ERR_CERT_NON_UNIQUE_NAME" => {
            "This name is not unique on the internet, so a certificate cannot \
             prove anything about it."
        }
        _ => "eave could not verify this site's certificate.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The G27 defect, as an assertion: escape used to arrive here as
    /// `Cancel` and be filed as a permanent deny.
    #[test]
    fn a_dismissal_is_not_written_down() {
        assert_eq!(decision_of(&Answer::Dismiss), None);
    }

    /// …and the thing it must not break: an explicit refusal IS durable.
    /// A browser that forgot "don't allow" would re-ask for the camera on
    /// every load, which is the G1 defect this slice must not reintroduce.
    #[test]
    fn dont_allow_is_still_a_durable_deny() {
        assert_eq!(decision_of(&Answer::Cancel), Some(false));
    }

    #[test]
    fn allow_is_a_durable_grant() {
        assert_eq!(decision_of(&Answer::Ok), Some(true));
    }

    /// Text and files answer asks that have nothing standing to record.
    #[test]
    fn an_answer_that_is_data_records_no_decision() {
        assert_eq!(decision_of(&Answer::Text("hello".into())), None);
        assert_eq!(decision_of(&Answer::Files(vec!["/tmp/x".into()])), None);
    }

    /// The page cannot tell a dismissal from a refusal, and must not be
    /// able to — there is no third answer to `confirm()`.
    #[test]
    fn the_page_hears_no_either_way() {
        assert!(!page_hears_yes(&Answer::Dismiss));
        assert!(!page_hears_yes(&Answer::Cancel));
        assert!(page_hears_yes(&Answer::Ok));
    }

    /// An empty file selection is a cancel, not a selection of nothing —
    /// but it is still not a decision to remember.
    #[test]
    fn an_empty_file_answer_is_data_not_a_decision() {
        assert_eq!(decision_of(&Answer::Files(Vec::new())), None);
        assert!(page_hears_yes(&Answer::Files(Vec::new())));
    }

    #[test]
    fn only_a_permission_ask_stands() {
        assert!(Ask::Permission {
            kinds: vec![PermissionKind::Camera]
        }
        .is_a_standing_decision());
        assert!(!Ask::Alert {
            message: "x".into()
        }
        .is_a_standing_decision());
        assert!(!Ask::Files {
            title: "x".into(),
            multiple: false,
            folder: false,
            save: false,
        }
        .is_a_standing_decision());
    }

    /// **A password must not be printable.** `dialogs::answer` traces
    /// every answer with `{answer:?}`, and `EAVE_WEB_TRACE` output goes to
    /// stderr, into terminals and into crash reports. A derived `Debug`
    /// would put the password in all three.
    #[test]
    fn a_password_never_survives_a_debug_format() {
        let a = Answer::Credentials {
            user: "ada".into(),
            password: "hunter2-correct-horse".into(),
        };
        let printed = format!("{a:?}");
        assert!(
            !printed.contains("hunter2"),
            "the password reached a Debug string: {printed}"
        );
        assert!(printed.contains("elided"), "{printed}");
    }

    /// The same rule for `prompt()`, which is where a page asks for a
    /// passphrase when it has no auth realm to lean on.
    #[test]
    fn prompt_text_never_survives_a_debug_format_either() {
        let printed = format!("{:?}", Answer::Text("my-recovery-phrase".into()));
        assert!(!printed.contains("recovery"), "{printed}");
        assert!(printed.contains("18 chars"), "{printed}");
    }

    /// The user name is a hint the operator may need in a trace ("which
    /// account did I try?"), so its length is printed and its value is not.
    #[test]
    fn a_debug_answer_still_says_what_kind_it_was() {
        assert_eq!(format!("{:?}", Answer::Dismiss), "Dismiss");
        assert!(format!("{:?}", Answer::Files(vec!["/a".into(), "/b".into()])).contains("2 paths"));
    }

    /// Credentials are an answer, not a standing decision: eave has no
    /// password store and this slice does not add one.
    #[test]
    fn credentials_are_not_written_down() {
        assert_eq!(
            decision_of(&Answer::Credentials {
                user: "ada".into(),
                password: "x".into()
            }),
            None
        );
    }

    /// …but the page (and Chromium) must hear yes, or the request is
    /// cancelled and the operator typed a password for nothing.
    #[test]
    fn credentials_are_heard_as_a_yes() {
        assert!(page_hears_yes(&Answer::Credentials {
            user: "ada".into(),
            password: "x".into()
        }));
    }

    /// Neither new ask is a standing decision — an auth realm is a
    /// challenge, and a bad certificate is a fact about one navigation.
    #[test]
    fn neither_auth_nor_a_certificate_is_remembered() {
        assert!(!Ask::Credentials {
            host: "x.test".into(),
            realm: None,
            is_proxy: false,
        }
        .is_a_standing_decision());
        assert!(!Ask::Certificate {
            url: "https://x.test/".into(),
            reason: "ERR_CERT_AUTHORITY_INVALID".into(),
        }
        .is_a_standing_decision());
    }

    /// Two of the sixteen deliberately share the generic sentence
    /// (`ERR_CERT_CONTAINS_ERRORS`, `ERR_CERT_INVALID` — Chromium's own
    /// catch-alls). Every OTHER named code must say something specific,
    /// or the mapping has a hole that reads as working.
    #[test]
    fn every_named_certificate_failure_is_explained() {
        let generic = certificate_explanation("ERR_SOMETHING_UNKNOWN");
        let vague = ["ERR_CERT_CONTAINS_ERRORS", "ERR_CERT_INVALID"];
        for reason in CERTIFICATE_REASONS {
            let words = certificate_explanation(reason);
            if vague.contains(reason) {
                assert_eq!(words, generic, "{reason} was given specific words");
                continue;
            }
            assert_ne!(
                words, generic,
                "{reason} fell through to the generic sentence"
            );
        }
    }

    /// An unknown code must still read as a failed check, never as a
    /// shrug — this is the string a person decides on.
    #[test]
    fn an_unrecognised_certificate_failure_still_reads_as_a_failure() {
        let words = certificate_explanation("ERR_CERT_SOMETHING_NEW_IN_CHROMIUM_152");
        assert!(words.contains("could not verify"), "{words}");
    }

    /// The explanations are shown to a person, so none may be empty and
    /// none may leak the raw code as its own explanation.
    #[test]
    fn no_certificate_explanation_is_empty_or_just_the_code() {
        for reason in CERTIFICATE_REASONS {
            let words = certificate_explanation(reason);
            assert!(!words.trim().is_empty(), "{reason} explained by nothing");
            assert!(
                !words.contains("ERR_"),
                "{reason} explained by a code: {words}"
            );
        }
    }

    /// The mask must not leak the password, in any part of it.
    #[test]
    fn a_mask_is_bullets_and_only_bullets() {
        let secret = "hunter2";
        let drawn = masked(secret);
        assert_eq!(drawn, "•••••••");
        assert!(!drawn.contains('h'), "{drawn}");
        assert!(drawn.chars().all(|c| c == '•'), "{drawn}");
    }

    /// Counted in CHARACTERS, not bytes — a password with an é in it must
    /// not draw an extra bullet and give away where it is.
    #[test]
    fn a_mask_counts_characters_not_bytes() {
        assert_eq!(masked("café").chars().count(), 4);
        assert_eq!(masked("日本語").chars().count(), 3);
    }

    /// Nothing typed draws nothing. An empty field that showed one bullet
    /// would be telling the operator they typed when they did not.
    #[test]
    fn an_empty_password_draws_nothing() {
        assert_eq!(masked(""), "");
    }

    /// The map is per tab, and taking one leaves the other alone —
    /// otherwise answering a dialog in one tab clears another's.
    #[test]
    fn asks_are_kept_and_taken_per_tab() {
        let mk = |tab: &str| Pending {
            tab_id: tab.into(),
            origin: "https://x.test/".into(),
            ask: Ask::Alert {
                message: "hi".into(),
            },
        };
        assert!(note_raised(mk("t-a")).is_none());
        assert!(note_raised(mk("t-b")).is_none());
        assert!(any_pending());
        assert!(take("t-a").is_some());
        assert!(pending("t-a").is_none());
        assert!(
            pending("t-b").is_some(),
            "answering one tab cleared another"
        );
        assert!(take("t-b").is_some());
        assert!(take("t-b").is_none(), "a second take found something");
    }

    /// A second ask for one tab DISPLACES the first and hands it back, so
    /// the caller can answer the stranded callback. Dropping it silently
    /// is a renderer stopped forever.
    #[test]
    fn a_second_ask_hands_back_the_one_it_replaced() {
        let mk = |msg: &str| Pending {
            tab_id: "t-displace".into(),
            origin: "https://x.test/".into(),
            ask: Ask::Alert {
                message: msg.into(),
            },
        };
        assert!(note_raised(mk("first")).is_none());
        let displaced = note_raised(mk("second")).expect("the first ask vanished");
        assert_eq!(
            displaced.ask,
            Ask::Alert {
                message: "first".into()
            }
        );
        assert!(take("t-displace").is_some());
    }

    /// An `alert()` has one button, so a Cancel beside it would offer a
    /// choice the page never hears about.
    #[test]
    fn an_alert_cannot_be_refused_and_everything_else_can() {
        assert!(!Ask::Alert {
            message: "x".into()
        }
        .is_refusable());
        assert!(Ask::Confirm {
            message: "x".into()
        }
        .is_refusable());
        assert!(Ask::Leave {
            message: "x".into(),
            reloading: false
        }
        .is_refusable());
    }
}
