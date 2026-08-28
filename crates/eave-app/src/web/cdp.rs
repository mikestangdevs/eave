//! The Chrome DevTools Protocol, as eave speaks it (Slice E9).
//!
//! # Why this is a separate file from `inspector`
//! The same split as `ask`/`dialogs` and `evict_policy`/`evict`, for the
//! same reason. Everything in `inspector` touches CEF and can only say
//! *what Chromium did*. What eave **sends**, how a call is matched to its
//! answer, and what an answer means are questions about JSON, an integer
//! and two maps — no Chromium in any of it. `inspector` is behind
//! `--features web`; this is not, so the default test run exercises the
//! whole protocol surface rather than skipping it.
//!
//! # What E9 actually is
//! `CefBrowserHost::SendDevToolsMessage` is the full DevTools protocol
//! **in-process**: this channel opens no socket, needs no handshake, and
//! nothing outside the process can reach it. Driving a page is normally
//! either a second browser (Playwright) or an open debugging port, and
//! *this* mechanism needs neither.
//!
//! **But eave does currently open one anyway, and not for this.** Slice
//! G9's docked DevTools sets `remote_debugging_port: 9223`
//! (`host::DEVTOOLS_PORT`) because CEF 151's `ShowDevTools` rejects
//! windowless rendering, so the bundled frontend has to reach its target
//! over a socket. It is set in `CefSettings` at initialise, so it is open
//! for the whole session and every tab **whether or not a dock is open**,
//! and Chromium's DevTools endpoint is unauthenticated to anything that
//! can reach loopback. That is a property of the dock, not of this file —
//! but an earlier draft of this paragraph claimed eave listens on nothing,
//! and it was wrong the moment G9 landed.
//!
//! # The answer is CEF's, verbatim
//! CEF parses the reply envelope before we see it: `on_dev_tools_method_result`
//! arrives already split into an id, a success flag, and either the
//! `"result"` or the `"error"` dictionary. So [`outcome`] **transcribes**
//! rather than re-parses, for the reason `find` states about the match
//! ordinal — a second parser that disagrees with the first disagrees
//! silently, and only on the inputs nobody tested.
//!
//! # Everything here is UI-thread state, and that is not a shortcut
//! CEF calls the observer on the browser-process UI thread, and
//! `SendDevToolsMessage` may only be called there. So both ends of a call
//! are pinned to that thread already, and `thread_local!` is not a
//! simplification — it is the shape the constraint forces.
//!
//! **This matters for the engine.** When an agent drives a page, the ask
//! arrives from another thread, and it must be *posted* to the UI thread
//! rather than reaching in. A mirror of this state readable off-thread
//! would be a mirror that fails the way slice G1's `thread_local!` did —
//! *indistinguishably from the feature simply not working*. There is no
//! such mirror here on purpose.

use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

/// What came back for one CDP call.
///
/// An error is split into its code and message rather than kept as a blob
/// because `-32000 "Cannot find context with specified id"` is the routine
/// failure when a page navigates mid-call, and a caller that wants to retry
/// needs to tell it from a malformed-method error without doing its own
/// JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The `"result"` dictionary, verbatim JSON. `{}` for methods that
    /// return nothing — which is a success, not an absence.
    Ok(String),
    /// The `"error"` dictionary, split.
    Err { code: i64, message: String },
    /// Chromium never took the message: no live browser, no host, or
    /// `SendDevToolsMessage` returned false. Distinct from `Err`, which
    /// means the agent answered and the answer was a refusal.
    NotSent,
    /// The tab went away before the answer arrived. A caller waiting on
    /// this id would otherwise wait for ever.
    Gone,
}

/// An event Chromium raised without being asked — the E8 half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The tab whose browser raised it. Present because one queue serves
    /// every tab; without it a console line has no page.
    pub tab_id: String,
    /// The CDP event name, e.g. `Network.responseReceived`.
    pub method: String,
    /// The `"params"` dictionary, verbatim JSON. `{}` when absent.
    pub params: String,
}

/// How many events are kept before the oldest are dropped.
///
/// A busy page raises thousands: `Network.dataReceived` alone fires per
/// chunk. Unbounded, this queue is a memory leak that only shows up on the
/// pages people actually use.
pub const EVENT_CAP: usize = 512;

thread_local! {
    /// CDP ids are per-agent, and eave keeps one counter for the process:
    /// ids need only be unique among *outstanding* calls, and a single
    /// counter makes a stray id traceable to the one place that issues them.
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    /// Issued calls awaiting an answer: id -> (tab, method).
    static PENDING: RefCell<HashMap<u32, (String, String)>> =
        RefCell::new(HashMap::new());
    /// Answers not yet collected.
    static RESULTS: RefCell<HashMap<u32, Outcome>> = RefCell::new(HashMap::new());
    /// Events not yet drained, oldest first.
    static EVENTS: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    /// Events dropped to the cap, ever. **Counted, not silent** — a trace
    /// cap that discards without saying so cost this repo two wrong
    /// conclusions in slice C5, and the fix is that the number survives.
    /// **It is not a backpressure signal on its own**: see
    /// [`dropped_events`].
    static DROPPED: Cell<u64> = const { Cell::new(0) };
}

/// Claim the next call id and record what it is waiting for.
pub fn begin(tab_id: &str, method: &str) -> u32 {
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1).max(1));
        id
    });
    PENDING.with(|p| {
        p.borrow_mut()
            .insert(id, (tab_id.to_string(), method.to_string()));
    });
    id
}

/// The outgoing envelope for a call.
///
/// `params` is **omitted entirely** when it is null or an empty object,
/// rather than sent as `"params": null`. Several domains reject an explicit
/// null where they accept an absent key, and the failure is a `-32602`
/// that names the method rather than the key — which reads as "the method
/// is wrong" and sends you looking in the wrong place.
pub fn envelope(id: u32, method: &str, params: &Value) -> String {
    let bare = match params {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    };
    if bare {
        json!({ "id": id, "method": method }).to_string()
    } else {
        json!({ "id": id, "method": method, "params": params }).to_string()
    }
}

/// Turn CEF's `(success, payload)` pair into an [`Outcome`].
///
/// An error payload that will not parse is kept as its own text rather than
/// discarded: an unrecognised error is still the only account of what went
/// wrong, and `code: 0` with the raw body beats `Err { code: 0, message: "" }`.
pub fn outcome(success: bool, payload: Option<&[u8]>) -> Outcome {
    let text = payload
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    if success {
        // A method that returns nothing succeeds with no payload. That is
        // an empty result, not a missing one.
        return Outcome::Ok(if text.is_empty() {
            "{}".to_string()
        } else {
            text
        });
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Outcome::Err {
            code: v.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(&text)
                .to_string(),
        },
        Err(_) => Outcome::Err {
            code: 0,
            message: text,
        },
    }
}

/// File an answer against its call. Unknown ids are dropped: a result for
/// a call this process did not make belongs to the docked DevTools
/// frontend, which shares the browser and is none of our business.
pub fn complete(id: u32, outcome: Outcome) {
    let known = PENDING.with(|p| p.borrow_mut().remove(&id)).is_some();
    if known {
        RESULTS.with(|r| {
            r.borrow_mut().insert(id, outcome);
        });
    }
}

/// Collect an answer, if it has arrived. Taking it forgets it.
pub fn take(id: u32) -> Option<Outcome> {
    RESULTS.with(|r| r.borrow_mut().remove(&id))
}

/// Whether this id is still waiting. `false` for both "answered" and
/// "never issued", which is why [`take`] is the question worth asking.
pub fn is_pending(id: u32) -> bool {
    PENDING.with(|p| p.borrow().contains_key(&id))
}

/// The method an outstanding call is waiting on.
pub fn pending_method(id: u32) -> Option<String> {
    PENDING.with(|p| p.borrow().get(&id).map(|(_, m)| m.clone()))
}

/// Queue an event, dropping the oldest past [`EVENT_CAP`].
pub fn note_event(tab_id: &str, method: &str, params: &str) {
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        while e.len() >= EVENT_CAP {
            e.pop_front();
            DROPPED.with(|d| d.set(d.get() + 1));
        }
        e.push_back(Event {
            tab_id: tab_id.to_string(),
            method: method.to_string(),
            params: if params.is_empty() {
                "{}".to_string()
            } else {
                params.to_string()
            },
        });
    });
}

/// Take every queued event, oldest first.
pub fn drain_events() -> Vec<Event> {
    EVENTS.with(|e| e.borrow_mut().drain(..).collect())
}

/// Take only the events a consumer asked for, leaving the rest queued.
///
/// [`drain_events`] is the take-everything case and stays the default; this
/// exists because **there is more than one consumer**. E8 wants console and
/// network; G2's notifications want `Runtime.bindingCalled` and nothing
/// else. With a single drain, whichever ran first would silently eat the
/// other's traffic — a defect with no error and no log line, visible only
/// as a feature that works until someone opens the other one.
///
/// A predicate rather than a list of method prefixes, so neither consumer
/// has to commit to a taxonomy of CDP events before anyone knows what the
/// second one needs.
///
/// # Known next move: this makes starvation possible, and the cap is silent
/// One capped queue plus selective takes means a category **nobody** reads
/// can fill all [`EVENT_CAP`] slots and evict a category someone is reading
/// — the docked frontend's self-enabled `Network.*` being the obvious
/// source, since it produces into this queue without any consumer asking.
/// So a notification can go missing because a person opened Inspect. The
/// fix, when it is needed, is an interest registry: [`note_event`] keeps
/// only what some consumer has registered for, which also restores
/// [`dropped_events`] to meaning what its name says. Deliberately not built
/// yet — it is the right answer to a problem nobody has measured.
pub fn take_events_matching(want: impl Fn(&Event) -> bool) -> Vec<Event> {
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(e.len());
        for event in e.drain(..) {
            if want(&event) {
                taken.push(event);
            } else {
                kept.push_back(event);
            }
        }
        *e = kept;
        taken
    })
}

/// How many events have been dropped to the cap. Never resets.
///
/// # This is NOT a backpressure signal on its own, and an earlier version
/// # of this comment said it was
/// It was written as "a consumer that sees this climb knows it is reading
/// too slowly". That is wrong, because **eave is not the only producer into
/// this queue.** The docked DevTools frontend (G9) attaches to the same
/// agent on the same browser, and the domains it enables *for itself* raise
/// events that reach [`super::inspector`]'s observer like any other. So
/// opening Inspect on a busy page fills all [`EVENT_CAP`] slots with
/// traffic nobody asked for and this number climbs — from a **human
/// action**, with no consumer involved at all.
///
/// The cap is working when that happens; it is not a leak. But a reader who
/// takes a rising count as "my consumer is too slow" will go looking in the
/// wrong process. Quote it with the condition attached: *was a dock open?*
pub fn dropped_events() -> u64 {
    DROPPED.with(Cell::get)
}

/// A tab's browser is gone. Answer everything it owed so nothing waits for
/// an agent that no longer exists.
pub fn forget_tab(tab_id: &str) {
    let orphaned: Vec<u32> = PENDING.with(|p| {
        p.borrow()
            .iter()
            .filter(|(_, (t, _))| t == tab_id)
            .map(|(id, _)| *id)
            .collect()
    });
    for id in orphaned {
        PENDING.with(|p| p.borrow_mut().remove(&id));
        RESULTS.with(|r| {
            r.borrow_mut().insert(id, Outcome::Gone);
        });
    }
    EVENTS.with(|e| e.borrow_mut().retain(|ev| ev.tab_id != tab_id));
}

// ---------------------------------------------------------------------------
// The four calls §4 E names, plus the enables E8 needs
// ---------------------------------------------------------------------------

/// Run JavaScript in the page and get its **value**.
///
/// `returnByValue` is the load-bearing argument. Without it `Runtime.evaluate`
/// answers with a remote object *handle* — `{"type":"object","objectId":"..."}`
/// — and an agent that reads `result.value` gets nothing back with no error
/// anywhere, because the call succeeded. `awaitPromise` is here for the same
/// class of reason: a page's own helpers are usually async, and without it the
/// answer is a pending promise handle.
pub fn evaluate(expression: &str) -> (&'static str, Value) {
    (
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        }),
    )
}

/// A click, as the two input events a click actually is.
///
/// One `mousePressed` is not a click and pages that listen for `click`
/// will not fire. `buttons` is the *held* mask and differs between the two
/// halves — 1 while down, 0 after release; sending 1 on the release leaves
/// pages that track drag state believing the button is still held.
pub fn click_events(x: f64, y: f64) -> [(&'static str, Value); 2] {
    [
        (
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": x, "y": y,
                "button": "left", "buttons": 1, "clickCount": 1,
            }),
        ),
        (
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased", "x": x, "y": y,
                "button": "left", "buttons": 0, "clickCount": 1,
            }),
        ),
    ]
}

/// Type text into whatever holds focus.
///
/// `Input.insertText` and not a key-event sequence: it is one call for any
/// length of text, it carries characters no keycode names, and it is what
/// an agent filling a field means. It does **not** raise `keydown`, so a
/// page that submits on Enter needs a real key event as well — that is a
/// different call and deliberately not hidden inside this one.
pub fn insert_text(text: &str) -> (&'static str, Value) {
    ("Input.insertText", json!({ "text": text }))
}

/// Capture the page as a PNG. The result carries `"data"`, base64.
pub fn screenshot() -> (&'static str, Value) {
    ("Page.captureScreenshot", json!({ "format": "png" }))
}

/// The domains E8 needs before Chromium will say anything unprompted.
///
/// Events do not flow until the domain is enabled — a silence that reads
/// exactly like a page doing nothing. `Network` gives requests and
/// responses, `Log` gives browser-side messages, `Runtime` gives
/// `console.*` via `Runtime.consoleAPICalled`.
pub fn event_enables() -> [(&'static str, Value); 3] {
    [
        ("Network.enable", json!({})),
        ("Log.enable", json!({})),
        ("Runtime.enable", json!({})),
    ]
}

/// Install a function on the page's global that calls back into eave.
///
/// The page calls `<name>(someString)` and Chromium raises
/// `Runtime.bindingCalled` with `{"name":..,"payload":..}`. It is the only
/// page-to-embedder channel CDP has, and it is **one-way**: the answer, if
/// there is one, goes back as a separate [`evaluate`].
///
/// Requires `Runtime.enable` first — without it the call succeeds, the
/// binding is installed, the page can call it, and no event ever arrives.
///
/// # This is a real global and it cannot be hidden
/// Any script on the page can see it, call it, and lie in the payload.
/// Whatever the binding guards must be guarded on the Rust side.
pub fn add_binding(name: &str) -> (&'static str, Value) {
    ("Runtime.addBinding", json!({ "name": name }))
}

/// Which binding a `Runtime.bindingCalled` event names.
///
/// # Why this is not just each consumer's own business
/// [`take_events_matching`] **removes** what it matches. A consumer that
/// takes every `Runtime.bindingCalled` and then discards the payloads it
/// cannot parse does not merely ignore another consumer's events — it eats
/// them, and the second consumer sees silence with nothing to grep for.
/// There are two bindings now (`notify`'s and `audible`'s), so every filter
/// on this event has to name the binding it wants, and this is the one place
/// that knows how to read it.
pub fn binding_name(params: &str) -> Option<String> {
    let event: Value = serde_json::from_str(params).ok()?;
    Some(event.get("name")?.as_str()?.to_string())
}

/// Run `source` at the start of every document loaded from now on.
///
/// For **future** documents only — it does nothing to the one already
/// loaded, which is a distinction with no error attached to it: install a
/// shim this way and it works perfectly on the second page a person visits.
/// Pair it with an [`evaluate`] of the same source for the current
/// document, and write the source so running twice is harmless.
///
/// Requires `Page.enable` first.
pub fn add_script_on_new_document(source: &str) -> (&'static str, Value) {
    (
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": source }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        PENDING.with(|p| p.borrow_mut().clear());
        RESULTS.with(|r| r.borrow_mut().clear());
        EVENTS.with(|e| e.borrow_mut().clear());
    }

    #[test]
    fn params_are_omitted_rather_than_sent_as_null() {
        let e = envelope(7, "Page.reload", &Value::Null);
        assert!(!e.contains("params"), "null params must not reach the wire: {e}");
        let e = envelope(7, "Page.reload", &json!({}));
        assert!(!e.contains("params"), "empty params must not reach the wire: {e}");
    }

    #[test]
    fn a_call_with_params_carries_them() {
        let e = envelope(3, "Runtime.evaluate", &json!({"expression": "1+1"}));
        let v: Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["method"], "Runtime.evaluate");
        assert_eq!(v["params"]["expression"], "1+1");
    }

    #[test]
    fn ids_are_unique_and_never_zero() {
        reset();
        let a = begin("tab", "Page.enable");
        let b = begin("tab", "Page.enable");
        assert_ne!(a, b);
        assert!(a > 0 && b > 0);
    }

    #[test]
    fn a_method_that_returns_nothing_still_succeeded() {
        assert_eq!(outcome(true, None), Outcome::Ok("{}".to_string()));
        assert_eq!(outcome(true, Some(b"")), Outcome::Ok("{}".to_string()));
    }

    #[test]
    fn an_error_is_split_into_code_and_message() {
        let o = outcome(
            false,
            Some(br#"{"code":-32000,"message":"Cannot find context"}"#),
        );
        assert_eq!(
            o,
            Outcome::Err {
                code: -32000,
                message: "Cannot find context".to_string()
            }
        );
    }

    #[test]
    fn an_unparseable_error_keeps_its_own_words() {
        let o = outcome(false, Some(b"upstream died"));
        assert_eq!(
            o,
            Outcome::Err {
                code: 0,
                message: "upstream died".to_string()
            }
        );
    }

    #[test]
    fn an_answer_reaches_the_call_that_asked() {
        reset();
        let id = begin("tab-a", "Runtime.evaluate");
        assert!(is_pending(id));
        assert_eq!(pending_method(id).as_deref(), Some("Runtime.evaluate"));
        complete(id, Outcome::Ok(r#"{"result":{"value":2}}"#.to_string()));
        assert!(!is_pending(id));
        assert_eq!(
            take(id),
            Some(Outcome::Ok(r#"{"result":{"value":2}}"#.to_string()))
        );
        assert_eq!(take(id), None, "an answer is collected once");
    }

    #[test]
    fn an_answer_to_a_call_we_never_made_is_not_kept() {
        reset();
        // The docked DevTools frontend shares the browser and issues its
        // own ids. Its results must not accumulate in eave's map.
        complete(4242, Outcome::Ok("{}".to_string()));
        assert_eq!(take(4242), None);
    }

    #[test]
    fn a_closed_tab_answers_everything_it_owed() {
        reset();
        let mine = begin("tab-a", "Page.captureScreenshot");
        let other = begin("tab-b", "Page.captureScreenshot");
        forget_tab("tab-a");
        assert_eq!(take(mine), Some(Outcome::Gone));
        assert!(is_pending(other), "another tab's call is untouched");
    }

    #[test]
    fn a_closed_tab_takes_its_events_with_it() {
        reset();
        note_event("tab-a", "Network.responseReceived", r#"{"n":1}"#);
        note_event("tab-b", "Network.responseReceived", r#"{"n":2}"#);
        forget_tab("tab-a");
        let left = drain_events();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].tab_id, "tab-b");
    }

    #[test]
    fn events_drain_oldest_first() {
        reset();
        note_event("t", "A", "{}");
        note_event("t", "B", "{}");
        let got: Vec<String> = drain_events().into_iter().map(|e| e.method).collect();
        assert_eq!(got, vec!["A".to_string(), "B".to_string()]);
        assert!(drain_events().is_empty(), "draining empties the queue");
    }

    #[test]
    fn the_event_cap_drops_the_oldest_and_says_how_many() {
        reset();
        let before = dropped_events();
        for i in 0..(EVENT_CAP + 3) {
            note_event("t", &format!("E{i}"), "{}");
        }
        let held = drain_events();
        assert_eq!(held.len(), EVENT_CAP);
        assert_eq!(
            held[0].method, "E3",
            "the three oldest were the ones dropped"
        );
        assert_eq!(
            dropped_events() - before,
            3,
            "the cap is counted, not silent"
        );
    }

    #[test]
    fn an_absent_params_dictionary_becomes_an_empty_one() {
        reset();
        note_event("t", "Page.loadEventFired", "");
        assert_eq!(drain_events()[0].params, "{}");
    }

    #[test]
    fn evaluate_asks_for_a_value_and_not_a_handle() {
        let (method, params) = evaluate("document.title");
        assert_eq!(method, "Runtime.evaluate");
        assert_eq!(params["returnByValue"], true);
        assert_eq!(params["awaitPromise"], true);
    }

    #[test]
    fn a_click_is_a_press_and_a_release_with_different_held_masks() {
        let [down, up] = click_events(12.0, 34.0);
        assert_eq!(down.1["type"], "mousePressed");
        assert_eq!(up.1["type"], "mouseReleased");
        assert_eq!(down.1["buttons"], 1);
        assert_eq!(up.1["buttons"], 0, "the button is not still held after a click");
        for (_, p) in [&down, &up] {
            assert_eq!(p["x"], 12.0);
            assert_eq!(p["clickCount"], 1);
        }
    }

    #[test]
    fn the_event_enables_cover_network_log_and_console() {
        let names: Vec<&str> = event_enables().iter().map(|(m, _)| *m).collect();
        assert!(names.contains(&"Network.enable"));
        assert!(names.contains(&"Log.enable"));
        // console.* arrives as Runtime.consoleAPICalled, so Runtime is the
        // domain that carries the console — not Console, which is legacy.
        assert!(names.contains(&"Runtime.enable"));
    }
}
