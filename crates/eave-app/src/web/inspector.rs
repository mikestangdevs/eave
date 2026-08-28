//! The CDP channel itself: send a message, hear the answers (Slice E9).
//!
//! The Chromium half of [`super::cdp`], and nothing else. Everything about
//! *what* is sent and what an answer means lives there, ungated and tested;
//! this file is the two CEF calls and the observer they need.
//!
//! # The registration is the subscription
//! `AddDevToolsMessageObserver` hands back a `Registration` whose **drop
//! unregisters the observer**. Nothing else references it, so a version of
//! this file that ignored the return value would compile, run, send
//! perfectly good messages, and hear nothing back — the failure being
//! silence rather than an error. [`REGISTRATIONS`] exists to hold them, and
//! that is its whole job.
//!
//! # One observer per tab, attached on first use
//! The agent attaches when the first message is sent while detached, so
//! registering ahead of time buys nothing and registering per call would
//! stack duplicate observers on one browser. First send wins; the
//! registration then lives as long as the tab.
//!
//! # eave is not the only client on this browser
//! A docked DevTools frontend (slice G9) talks to the same agent with its
//! own ids. Its results reach this observer too, which is why
//! [`super::cdp::complete`] drops answers to ids it never issued instead of
//! trusting that every id belongs to us.

// The `wrap_dev_tools_message_observer!` macro names types that never
// appear in this source — same reason `find.rs` and `tabs.rs` carry a glob.
use cef::*;

use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Live observer registrations, by tab. **Dropping one unregisters the
    /// observer**, so this map is the subscription and not a cache of it.
    static REGISTRATIONS: RefCell<HashMap<String, Registration>> =
        RefCell::new(HashMap::new());
    /// Call ids issued by the `EAVE_CDP_EVAL` probe, so their answers can
    /// be printed instead of left for a consumer that does not exist yet.
    static PROBES: RefCell<std::collections::HashSet<u32>> =
        RefCell::new(std::collections::HashSet::new());
}

/// `EAVE_CDP_EVAL=<expression>` — the instrument that makes E9 observable.
///
/// Runs the expression in every page as it finishes loading and prints the
/// answer to stderr. It exists because a channel that is only ever exercised
/// by a test suite is a channel nobody has watched work: G9 sat "built and
/// green" for a day before the operator opened Inspect and settled it in a
/// second. Read once — an env var that changed mid-session would make two
/// runs of the same build disagree.
///
/// # It prints to stderr, and the documented launch path THROWS STDERR AWAY
/// eave is launched with `open -n`, and a LaunchServices-launched app has no
/// terminal attached. Without `--stderr` this probe runs, works, and shows
/// you nothing — which reads exactly like the channel being broken. Both
/// flags are needed, and the env one is `--env` rather than a shell prefix:
///
/// ```text
/// open -n --env EAVE_CDP_EVAL=document.title \
///         --stderr /tmp/eave.log <path>/eave.app
/// ```
///
/// (A neighbouring doc claimed `open -n` passes no environment at all. It
/// was never measured and is false; the missing terminal was the real
/// cause. One symptom, wrong cause — worth knowing, because the same
/// mistake would make this probe look dead.)
fn probe_expression() -> Option<&'static str> {
    static EXPR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    EXPR.get_or_init(|| std::env::var("EAVE_CDP_EVAL").ok())
        .as_deref()
}

/// Fire the probe for a page that has just finished loading.
pub(super) fn probe_on_load(tab_id: &str) {
    let Some(expression) = probe_expression() else {
        return;
    };
    // The docked DevTools frontend is a page too, and it loads like one.
    // Driving it would evaluate against DevTools' own DOM and report that
    // as if it were the page underneath — a wrong answer that looks
    // entirely plausible.
    if super::devtools_parent(tab_id).is_some() {
        return;
    }
    let (method, params) = super::cdp::evaluate(expression);
    let id = send(tab_id, method, &params);
    PROBES.with(|p| {
        p.borrow_mut().insert(id);
    });
}

/// Send one CDP call to a tab's page and return the id it will answer on.
///
/// Never blocks: the answer arrives on a later pump, through the observer.
/// A tab with no live browser is not an error here — it answers
/// [`super::cdp::Outcome::NotSent`] immediately, so a caller polling
/// [`super::cdp::take`] terminates either way rather than waiting on a page
/// that was evicted between the decision to call and the call.
pub(super) fn send(tab_id: &str, method: &str, params: &Value) -> u32 {
    let id = super::cdp::begin(tab_id, method);
    let body = super::cdp::envelope(id, method, params);

    // Register BEFORE sending. The agent attaches on the first message
    // while detached, and an observer added after that first send misses
    // its answer — a failure that only ever shows on the first call of a
    // session, which is the one a person is most likely to be watching.
    ensure_observer(tab_id);

    let sent = super::tabs::with_browser(tab_id, |b| {
        b.host()
            .map(|h| h.send_dev_tools_message(Some(body.as_bytes())) != 0)
            .unwrap_or(false)
    })
    .unwrap_or(false);

    if super::host::traced() {
        eprintln!("[web] cdp #{id} {method} -> {tab_id} sent={sent}");
    }
    if !sent {
        super::cdp::complete(id, super::cdp::Outcome::NotSent);
    }

    super::host::note_activity();
    super::host::request_pump();
    id
}

/// A tab is going away: drop its observer registration.
///
/// Only the Chromium half. Answering the calls the tab owed is
/// [`super::cdp::forget_tab`]'s job and happens in `forget_page`, which is
/// ungated — a tab closing must settle its pending calls in **both** feature
/// states, and a cleanup that only ran with `--features web` would be a
/// cleanup the default suite could never observe.
///
/// Called before the cdp half, so no further callback can arrive for a tab
/// whose pending calls are about to be resolved.
pub(super) fn forget_registration(tab_id: &str) {
    REGISTRATIONS.with(|r| {
        r.borrow_mut().remove(tab_id);
    });
}

/// Attach an observer to this tab's browser, once.
fn ensure_observer(tab_id: &str) {
    if REGISTRATIONS.with(|r| r.borrow().contains_key(tab_id)) {
        return;
    }
    // Compute the registration entirely outside the map's borrow: this is
    // a synchronous CEF call, and CEF runs handlers on our stack under an
    // external pump. Holding `REGISTRATIONS.borrow_mut()` across it is the
    // `already borrowed` abort the whole module is written to avoid.
    let registration = super::tabs::with_browser(tab_id, |b| {
        b.host().and_then(|h| {
            let mut observer = TabObserver::new(tab_id.to_string());
            h.add_dev_tools_message_observer(Some(&mut observer))
        })
    })
    .flatten();

    if let Some(registration) = registration {
        REGISTRATIONS.with(|r| {
            r.borrow_mut().insert(tab_id.to_string(), registration);
        });
    } else if super::host::traced() {
        eprintln!("[web] cdp observer refused for {tab_id}");
    }
}

wrap_dev_tools_message_observer! {
    pub struct TabObserver {
        tab_id: String,
    }

    impl DevToolsMessageObserver {
        /// `on_dev_tools_message` is deliberately NOT implemented.
        ///
        /// It is the raw catch-all, and returning true from it suppresses
        /// the two typed callbacks below. Leaving it at its default (false,
        /// "not handled") is what lets CEF split the envelope for us —
        /// which it has already done correctly, in C++, before we see it.
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            super::cdp::complete(
                message_id as u32,
                super::cdp::outcome(success == 1, result),
            );
            // The probe consumes its own answer: nothing else is reading
            // results yet, and an uncollected one would sit in the map for
            // the life of the tab.
            let id = message_id as u32;
            if PROBES.with(|p| p.borrow_mut().remove(&id)) {
                match super::cdp::take(id) {
                    Some(outcome) => eprintln!("[web] EAVE_CDP_EVAL #{id} -> {outcome:?}"),
                    None => eprintln!("[web] EAVE_CDP_EVAL #{id} -> answered, but not ours"),
                }
            }
            // The answer changed no page pixel, so nothing else would ask
            // for a redraw — the same reason `find` marks a chrome change
            // when only the tally moved.
            super::host::note_chrome_change();
        }

        fn on_dev_tools_event(
            &self,
            _browser: Option<&mut Browser>,
            method: Option<&CefString>,
            params: Option<&[u8]>,
        ) {
            let method = method.map(|m| m.to_string()).unwrap_or_default();
            let params = params
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default();
            super::cdp::note_event(&self.tab_id, &method, &params);
        }
    }
}
