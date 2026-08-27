//! `window.Notification`, honestly — the display half of slice G2.
//!
//! [`super::permissions`] stopped eave lying: a notification it cannot
//! deliver is refused rather than granted (`0f9fd22`). This module is the
//! other half — actually delivering one — and it exists because **CEF alloy
//! has no notification API at all**. There is a platform notification
//! service behind the browser, it is non-null, and it drops what it is
//! given. Nothing eave can call on the CEF seam displays anything.
//!
//! So eave displays it itself, and the page reaches eave through a CDP
//! binding rather than through Chromium's notification path.
//!
//! # The JavaScript is not the enforcement point, and must never become it
//! The shim is a convenience for well-behaved pages. A hostile one can
//! delete it, keep a reference to the raw binding, and call it with any
//! payload it likes — `Runtime.addBinding` installs a real function on the
//! page's own global and there is no way to hide it. Everything that
//! decides *whether* something is shown, and *who it is attributed to*,
//! therefore lives on this side:
//!
//! - **The origin is never read from the payload.** It comes from eave's
//!   own record of which tab raised the event. A page that puts
//!   `"origin": "bank.example"` in the JSON is ignored, because nothing
//!   here looks.
//! - **The permission check happens in Rust**, on arrival, against the
//!   same store the prompt writes to.
//! - **Every string is clamped** before it can reach a layout pass.
//!
//! Read the payload as what it is: text from a web page.
//!
//! # `onshow` fires when it showed, and not before
//! The defect G2 names is not that notifications were missing — it is that
//! `onshow` **fired anyway**, so no page could tell. The shim therefore
//! does not raise `show` when it posts to the binding. It raises it when
//! eave sends [`Ack::Shown`] back, after the toast is on screen. A
//! notification that is refused gets [`Ack::Blocked`] and an `error` event,
//! which is a thing a page can branch on.

use serde_json::{json, Value};

/// The binding a page calls to reach eave.
///
/// One binding carries every kind of message, tagged inside the payload,
/// because each `Runtime.addBinding` is a separate global on the page and
/// a second one buys nothing but a second name to keep in step.
pub const BINDING: &str = "__eaveNotify";

/// The function eave calls in the page to answer.
pub const ACK: &str = "__eaveNotifyAck";

/// Longest title eave will lay out.
///
/// A cap rather than a scroll: this is a transient surface, and a page that
/// sends 4 MB of title is not trying to tell the user something.
pub const TITLE_MAX: usize = 120;

/// Longest body eave will lay out. See [`TITLE_MAX`].
pub const BODY_MAX: usize = 400;

/// What a page asked eave to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `new Notification(title, options)`.
    Show(Request),
    /// `Notification.requestPermission()`.
    Request {
        /// The page's own id for the call, echoed back in the ack.
        seq: u64,
    },
    /// `notification.close()`.
    Close {
        /// The page's id for the notification being closed.
        seq: u64,
    },
}

/// One notification a page wants shown.
///
/// There is deliberately **no origin field**. The origin is eave's to
/// decide, from the tab, and a struct that could carry the page's opinion
/// of its own identity is a struct someone will eventually read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// The page's id for this notification, echoed in the ack so the shim
    /// can find the object again.
    pub seq: u64,
    /// Already clamped to [`TITLE_MAX`].
    pub title: String,
    /// Already clamped to [`BODY_MAX`].
    pub body: String,
    /// The page's `tag`: a later notification with the same tag replaces
    /// an earlier one rather than stacking. Clamped like the rest.
    pub tag: String,
}

/// What eave tells the page happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// It is on screen. The shim raises `show` **now**, not earlier.
    Shown,
    /// Refused — no permission, or eave declined. The shim raises `error`.
    Blocked,
    /// It has gone, by timeout or by the user. The shim raises `close`.
    Closed,
}

impl Ack {
    /// The word the shim switches on.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Shown => "show",
            Self::Blocked => "error",
            Self::Closed => "close",
        }
    }
}

/// Clamp a page-supplied string to `max` **characters**, not bytes.
///
/// Bytes would split a multi-byte character and produce a panic on the
/// slice — or, worse in a release build, a string that is valid UTF-8 by
/// luck. Emoji in a notification title is not an edge case, it is Tuesday.
fn clamp(value: &str, max: usize) -> String {
    let mut out: String = value.chars().take(max).collect();
    // A page can send newlines; a toast is one line and a title that
    // smuggles a paragraph break is a layout bug the page chose.
    if out.contains(['\n', '\r']) {
        out = out.replace(['\n', '\r'], " ");
    }
    out
}

/// Read one `Runtime.bindingCalled` event.
///
/// Returns `None` for every event that is not ours — including bindings
/// some other consumer installs later, which is why the name is checked
/// rather than assumed. The payload is a **string** in CDP, containing
/// JSON, so this parses twice and either step can fail on a page that
/// calls the raw binding with garbage. Both failures are `None`: a page
/// calling its own global wrongly is not an eave error.
pub fn parse_binding_called(params: &str) -> Option<Message> {
    let event: Value = serde_json::from_str(params).ok()?;
    if event.get("name")?.as_str()? != BINDING {
        return None;
    }
    let payload: Value = serde_json::from_str(event.get("payload")?.as_str()?).ok()?;
    let seq = payload.get("seq").and_then(Value::as_u64).unwrap_or(0);

    match payload.get("kind").and_then(Value::as_str)? {
        "show" => Some(Message::Show(Request {
            seq,
            title: clamp(
                payload.get("title").and_then(Value::as_str).unwrap_or(""),
                TITLE_MAX,
            ),
            body: clamp(
                payload.get("body").and_then(Value::as_str).unwrap_or(""),
                BODY_MAX,
            ),
            tag: clamp(
                payload.get("tag").and_then(Value::as_str).unwrap_or(""),
                TITLE_MAX,
            ),
        })),
        "request" => Some(Message::Request { seq }),
        "close" => Some(Message::Close { seq }),
        _ => None,
    }
}

/// The call that answers one notification.
///
/// Goes out as a `Runtime.evaluate` rather than anything cleverer because
/// the page half is a plain function on the page's own global: there is no
/// reverse binding in CDP, and an evaluate is the reverse channel.
pub fn ack_call(seq: u64, ack: Ack) -> (&'static str, Value) {
    super::cdp::evaluate(&format!(
        "globalThis.{ACK}&&globalThis.{ACK}({seq},{verb})",
        ACK = ACK,
        seq = seq,
        verb = json!(ack.verb()),
    ))
}

/// Tell the page what its permission state is now.
///
/// Sent on install and again whenever the answer changes, so
/// `Notification.permission` is a value the page can read synchronously —
/// which is what the API promises and what feature-detection reads.
pub fn permission_call(state: &str) -> (&'static str, Value) {
    super::cdp::evaluate(&format!(
        "globalThis.{ACK}&&globalThis.{ACK}(0,\"permission\",{state})",
        ACK = ACK,
        state = json!(state),
    ))
}

/// Everything that must be sent to a tab before a page can notify.
///
/// Order matters and is not obvious:
///
/// 1. `Runtime.enable` — without it `Runtime.bindingCalled` never arrives,
///    and the failure is silence rather than an error.
/// 2. `Page.enable` — required before
///    `addScriptToEvaluateOnNewDocument` will take.
/// 3. `Runtime.addBinding` — install the function.
/// 4. `Page.addScriptToEvaluateOnNewDocument` — the shim, for **future**
///    documents.
/// 5. `Runtime.evaluate` of the same source — for the document that is
///    **already loaded**. Step 4 does nothing for it, and a shim installed
///    only for the next navigation is a shim that never runs on a page a
///    person is looking at.
///
/// Steps 4 and 5 carry the same source, which is why the shim guards
/// against installing itself twice.
pub fn install_calls() -> Vec<(&'static str, Value)> {
    let source = shim_source();
    vec![
        ("Runtime.enable", json!({})),
        ("Page.enable", json!({})),
        super::cdp::add_binding(BINDING),
        super::cdp::add_script_on_new_document(&source),
        super::cdp::evaluate(&source),
    ]
}

/// The page-side shim: `window.Notification`, reimplemented over [`BINDING`].
///
/// Deliberately an ordinary function on the global rather than anything
/// hidden. It cannot be hidden — see the module docs — so pretending
/// otherwise would only mislead whoever reads this next.
pub fn shim_source() -> String {
    // `{{` / `}}` are format escapes; this is a `format!` so the two names
    // stay in one place rather than being spelled again in JS.
    format!(
        r#"(function(){{
  if (globalThis.{ACK}) return;
  var pending = new Map();
  var permission = "default";
  var asks = new Map();
  var next = 1;
  function post(msg) {{
    var fn = globalThis.{BINDING};
    if (typeof fn !== "function") return false;
    try {{ fn(JSON.stringify(msg)); return true; }} catch (e) {{ return false; }}
  }}
  function N(title, options) {{
    if (!(this instanceof N)) throw new TypeError("Notification is a constructor");
    options = options || {{}};
    this.title = String(title);
    this.body = String(options.body == null ? "" : options.body);
    this.tag = String(options.tag == null ? "" : options.tag);
    this.onshow = null; this.onclose = null; this.onerror = null; this.onclick = null;
    var seq = next++;
    this._seq = seq;
    pending.set(seq, this);
    var self = this;
    if (!post({{kind: "show", seq: seq, title: this.title, body: this.body, tag: this.tag}})) {{
      // No binding: nothing displayed. Report the failure rather than a
      // show — the whole point of this shim.
      Promise.resolve().then(function(){{ deliver(seq, "error"); }});
    }}
  }}
  N.prototype.close = function(){{ post({{kind: "close", seq: this._seq}}); }};
  N.prototype.addEventListener = function(type, fn){{
    (this["_l_" + type] = this["_l_" + type] || []).push(fn);
  }};
  N.prototype.removeEventListener = function(type, fn){{
    var l = this["_l_" + type]; if (!l) return;
    var i = l.indexOf(fn); if (i >= 0) l.splice(i, 1);
  }};
  Object.defineProperty(N, "permission", {{get: function(){{ return permission; }}}});
  N.requestPermission = function(cb){{
    var seq = next++;
    var p = new Promise(function(resolve){{ asks.set(seq, resolve); }});
    if (!post({{kind: "request", seq: seq}})) {{
      asks.delete(seq);
      // A page that cannot reach eave is denied, not left hanging: the
      // API has no third answer and a pending promise stalls the caller.
      p = Promise.resolve("denied");
    }}
    if (typeof cb === "function") p.then(cb);
    return p;
  }};
  N.maxActions = 0;
  function deliver(seq, verb) {{
    var n = pending.get(seq);
    if (!n) return;
    if (verb === "close" || verb === "error") pending.delete(seq);
    var ev = {{type: verb, target: n, currentTarget: n}};
    var h = n["on" + verb];
    if (typeof h === "function") {{ try {{ h.call(n, ev); }} catch (e) {{}} }}
    var l = n["_l_" + verb] || [];
    for (var i = 0; i < l.length; i++) {{ try {{ l[i].call(n, ev); }} catch (e) {{}} }}
  }}
  globalThis.{ACK} = function(seq, verb, arg) {{
    if (verb === "permission") {{
      permission = String(arg);
      asks.forEach(function(resolve){{ resolve(permission); }});
      asks.clear();
      return;
    }}
    deliver(seq, verb);
  }};
  globalThis.Notification = N;
}})()"#,
        ACK = ACK,
        BINDING = BINDING,
    )
}

/// How long a toast stays up before eave takes it down.
///
/// Chrome's is about this. The number is a compromise nobody loves: long
/// enough to read a two-line body, short enough that a page cannot park
/// something over the top-right corner of the window indefinitely.
pub const TOAST_MS: u64 = 6_000;

/// How many toasts stack before the oldest is pushed off.
///
/// A cap and not a scroll, for the same reason as [`TITLE_MAX`]: this is a
/// surface a **page** decides to put things on, and the only defence
/// against a loop calling `new Notification()` is that the surface has a
/// fixed size.
pub const TOAST_MAX: usize = 3;

/// A notification currently on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    /// The page's id for it, so an ack can be sent when it goes.
    pub seq: u64,
    /// The tab that raised it. An ack goes back to this tab and no other.
    pub tab_id: String,
    /// **Who is shown as the sender.** Derived from eave's own tab model,
    /// never from the payload — see the module docs.
    pub origin: String,
    pub title: String,
    pub body: String,
    /// The page's `tag`. A second notification with the same tag from the
    /// same origin replaces the first rather than stacking, which is what
    /// the web API promises and what makes a progress notification usable.
    pub tag: String,
    /// When it went up, for [`TOAST_MS`].
    pub shown: std::time::Instant,
}

thread_local! {
    static LIVE: std::cell::RefCell<Vec<Live>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Tabs whose page has the shim. Cleared for a tab when it navigates,
    /// because a new document has no shim until the new-document script
    /// runs — and re-running install on every frame would send four CDP
    /// calls per frame per tab.
    static INSTALLED: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Pages waiting on a `requestPermission()` prompt: tab -> (seq, origin).
    ///
    /// The page's promise is open for as long as this entry lives, which is
    /// why the answer path must reach every exit — allowed, denied, and
    /// dismissed. A tab that closes mid-prompt drops the entry with
    /// `forget_tab`; there is no page left to hear an answer.
    static ASKING: std::cell::RefCell<std::collections::HashMap<String, (u64, String)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// What is on screen now, oldest first.
pub fn live() -> Vec<Live> {
    LIVE.with(|l| l.borrow().clone())
}

/// Take one down because a person clicked it away.
pub fn dismiss(seq: u64) {
    let closed = LIVE.with(|l| {
        let mut l = l.borrow_mut();
        let at = l.iter().position(|n| n.seq == seq)?;
        Some(l.remove(at))
    });
    if let Some(n) = closed {
        let (method, params) = ack_call(n.seq, Ack::Closed);
        super::cdp_call(&n.tab_id, method, &params);
    }
}

/// Forget everything a tab was showing, without acking.
///
/// For a tab that is going away: there is no page left to hear a `close`,
/// and `cdp_call` on a dead browser would answer `NotSent` on a later pump
/// for nothing.
pub fn forget_tab(tab_id: &str) {
    ASKING.with(|a| {
        a.borrow_mut().remove(tab_id);
    });
    LIVE.with(|l| l.borrow_mut().retain(|n| n.tab_id != tab_id));
    INSTALLED.with(|i| {
        i.borrow_mut().remove(tab_id);
    });
}

/// The origin eave will attribute a notification to, from a page URL.
///
/// Scheme and host, nothing else — a path is not identity and a long one
/// pushes the part that matters off the end of the card. `None` for a URL
/// with no host, which includes `about:blank` and `file:` — neither has an
/// origin a person could act on, so neither may notify.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// One turn of the notification loop, called from the render pass.
///
/// Does three things, in an order that matters: install the shim on pages
/// that need it, answer whatever pages have asked for since the last turn,
/// then retire anything that has been up long enough.
///
/// `now` is passed rather than read so a test can move time without
/// sleeping.
pub fn drive(snapshot: &eave_wire::TabsSnapshot, now: std::time::Instant) {
    install_where_needed(snapshot);
    handle_events(snapshot);
    answer_asks();
    expire(now);
}

/// Tell any page whose permission prompt has been answered.
///
/// **Without this the capability is unreachable.** The shim replaces
/// `window.Notification`, so `requestPermission()` never reaches Chromium
/// and Chromium never raises its own prompt — and eave's notification
/// content setting is forced to BLOCK besides. So eave has to ask, and eave
/// has to answer. A version of this slice without it grants nothing, ever,
/// by any path a person has: every notification is refused for want of a
/// permission there was no way to give.
///
/// Polled rather than pushed because the answer arrives on the UI's
/// timeline, through the same `ask` surface every other dialog uses, and a
/// callback from there into here would be a second way to answer a prompt.
fn answer_asks() {
    let done: Vec<(String, u64, String)> = ASKING.with(|a| {
        a.borrow()
            .iter()
            .filter(|(tab, _)| super::ask::pending(tab).is_none())
            .map(|(tab, (seq, origin))| (tab.clone(), *seq, origin.clone()))
            .collect()
    });
    for (tab, seq, origin) in done {
        ASKING.with(|a| {
            a.borrow_mut().remove(&tab);
        });
        let _ = seq;
        // A dismissal leaves the store on `Ask`, so this answers
        // `"default"` — which is exactly what the web API means by a
        // prompt the person walked away from, and is a state a page can
        // ask again from.
        let (method, params) = permission_call(permission_state(&origin));
        super::cdp_call(&tab, method, &params);
    }
}

/// Give the shim to any page tab that does not have it at its current URL.
///
/// Keyed by **URL, not tab**: a tab that navigates is a new document, and
/// the new-document script covers it — but only if `Page.enable` and the
/// binding were installed on that browser, which they were. Re-installing
/// on navigation is therefore belt-and-braces, and cheap, and it is what
/// makes the first page of a session work as well as the second.
fn install_where_needed(snapshot: &eave_wire::TabsSnapshot) {
    for tab in &snapshot.tabs {
        if tab.content != eave_wire::TabContent::Page {
            continue;
        }
        let Some(url) = tab.url.as_deref() else {
            continue;
        };
        if origin_of(url).is_none() {
            continue;
        }
        let id = tab.id.to_string();
        let fresh = INSTALLED.with(|i| i.borrow().get(&id).map(String::as_str) != Some(url));
        if !fresh {
            continue;
        }
        INSTALLED.with(|i| {
            i.borrow_mut().insert(id.clone(), url.to_string());
        });
        for (method, params) in install_calls() {
            super::cdp_call(&id, method, &params);
        }
    }
}

/// Answer every `Runtime.bindingCalled` that has arrived since last turn.
///
/// Takes only its own events — E8 wants the console and network traffic in
/// the same queue, and a plain drain would eat it. See
/// [`super::cdp::take_events_matching`].
fn handle_events(snapshot: &eave_wire::TabsSnapshot) {
    // Named, not just typed. `take_events_matching` REMOVES what it
    // matches, so taking every `bindingCalled` and dropping the ones that do
    // not parse would swallow `audible`'s events on the way past — and the
    // symptom would be a speaker icon that never appears, with nothing in
    // this module to suggest why. `parse_binding_called` checks the name
    // too; that check protects the payload, this one protects the queue.
    let events = super::cdp::take_events_matching(|e| {
        e.method == "Runtime.bindingCalled"
            && super::cdp::binding_name(&e.params).as_deref() == Some(BINDING)
    });
    for event in events {
        let Some(message) = parse_binding_called(&event.params) else {
            continue;
        };
        // The origin is looked up HERE, from eave's tab model, and the
        // payload is never consulted for it. A tab eave cannot find an
        // origin for cannot notify — see the module docs.
        let origin = snapshot
            .tabs
            .iter()
            .find(|t| t.id.as_str() == event.tab_id)
            .and_then(|t| t.url.as_deref())
            .and_then(origin_of);
        match message {
            Message::Request { seq } => {
                let Some(origin) = origin.as_deref() else {
                    // No origin eave can name — nothing to remember an
                    // answer against, and nothing honest to put on a
                    // prompt's first line.
                    //
                    // **`"default"`, not `"denied"`.** This is usually
                    // TRANSIENT: a page can call `requestPermission()`
                    // before its tab's URL has reached the snapshot, and
                    // answering `denied` there is a refusal the person
                    // never made, which the page then caches and acts on
                    // for the rest of its life. Observed live — the test
                    // page read `denied` while eave's own prompt for the
                    // same origin was still on screen behind it.
                    // `"default"` is the one answer that is true in both
                    // the transient case and the permanent one, and it is
                    // the only one a page may ask again from.
                    let (method, params) = permission_call("default");
                    super::cdp_call(&event.tab_id, method, &params);
                    continue;
                };
                match permission_state(origin) {
                    // Already answered, either way: the page hears it now.
                    // G1's rule — a stored answer is not re-asked.
                    state @ ("granted" | "denied") => {
                        let (method, params) = permission_call(state);
                        super::cdp_call(&event.tab_id, method, &params);
                    }
                    // Unanswered: raise eave's own prompt. The reply goes
                    // out from `answer_asks` once a person has decided.
                    _ => {
                        super::ask::note_raised(super::ask::Pending {
                            tab_id: event.tab_id.clone(),
                            origin: origin.to_string(),
                            ask: super::ask::Ask::Permission {
                                kinds: vec![
                                    eave_core::browser::permissions::PermissionKind::Notifications,
                                ],
                            },
                        });
                        ASKING.with(|a| {
                            a.borrow_mut()
                                .insert(event.tab_id.clone(), (seq, origin.to_string()));
                        });
                    }
                }
            }
            Message::Close { seq } => {
                LIVE.with(|l| l.borrow_mut().retain(|n| n.seq != seq));
            }
            Message::Show(request) => {
                let allowed = origin
                    .as_deref()
                    .is_some_and(|o| permission_state(o) == "granted");
                if !allowed {
                    let (method, params) = ack_call(request.seq, Ack::Blocked);
                    super::cdp_call(&event.tab_id, method, &params);
                    continue;
                }
                let origin = origin.unwrap_or_default();
                show(&event.tab_id, &origin, request);
            }
        }
    }
}

/// Put one on screen, replacing a same-tag notification from the same tab.
fn show(tab_id: &str, origin: &str, request: Request) {
    let seq = request.seq;
    LIVE.with(|l| {
        let mut l = l.borrow_mut();
        if !request.tag.is_empty() {
            l.retain(|n| !(n.tab_id == tab_id && n.tag == request.tag));
        }
        l.push(Live {
            seq,
            tab_id: tab_id.to_string(),
            origin: origin.to_string(),
            title: request.title,
            body: request.body,
            tag: request.tag,
            shown: std::time::Instant::now(),
        });
        while l.len() > TOAST_MAX {
            l.remove(0);
        }
    });
    // Only now, and only because it is on screen. This line is the slice.
    let (method, params) = ack_call(seq, Ack::Shown);
    super::cdp_call(tab_id, method, &params);
}

/// Retire anything that has been up for [`TOAST_MS`].
fn expire(now: std::time::Instant) {
    let gone: Vec<Live> = LIVE.with(|l| {
        let mut l = l.borrow_mut();
        let (old, keep): (Vec<Live>, Vec<Live>) = l
            .drain(..)
            .partition(|n| now.saturating_duration_since(n.shown).as_millis() as u64 >= TOAST_MS);
        *l = keep;
        old
    });
    for n in gone {
        let (method, params) = ack_call(n.seq, Ack::Closed);
        super::cdp_call(&n.tab_id, method, &params);
    }
}

/// What `Notification.permission` should read for an origin.
///
/// The three words are the web API's, not eave's, so the shim can hand
/// them straight to a page.
pub fn permission_state(origin: &str) -> &'static str {
    use eave_core::browser::permissions::Verdict;
    match super::permissions::verdict(
        origin,
        &[eave_core::browser::permissions::PermissionKind::Notifications],
    ) {
        Verdict::Allow => "granted",
        Verdict::Deny => "denied",
        Verdict::Ask => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape CDP actually delivers: `payload` is a STRING holding JSON,
    /// not a nested object. Getting this wrong parses to `None` forever.
    fn event(payload: &str) -> String {
        json!({
            "name": BINDING,
            "payload": payload,
            "executionContextId": 3,
        })
        .to_string()
    }

    fn show(payload: &str) -> Request {
        match parse_binding_called(&event(payload)) {
            Some(Message::Show(r)) => r,
            other => panic!("expected a show, got {other:?}"),
        }
    }

    // ---- the loop, end to end, with no browser anywhere ----
    //
    // `cdp_call` has a feature-off twin that issues the call and records
    // it, so install → decide → ack → expire all run here. What the `web`
    // build adds is a Chromium on the other end, not different logic.

    /// Thread-locals outlive a test on the same thread, so every loop test
    /// starts from nothing. **Origins are per-test on purpose**: the
    /// permission mirror is process-global, not thread-local, and two tests
    /// sharing an origin would hand each other grants.
    fn reset() {
        LIVE.with(|l| l.borrow_mut().clear());
        INSTALLED.with(|i| i.borrow_mut().clear());
        let _ = super::super::cdp::drain_events();
        let _ = super::super::take_cdp_calls_for_test();
    }

    fn snapshot(tab: &str, url: &str) -> eave_wire::TabsSnapshot {
        eave_wire::TabsSnapshot {
            tabs: vec![eave_wire::TabInfo {
                id: tab.into(),
                group: None,
                pinned: false,
                content: eave_wire::TabContent::Page,
                url: Some(url.to_string()),
                title: None,
                favicon_url: None,
                load: eave_wire::LoadState::Complete,
                pending: None,
                can_go_back: false,
                can_go_forward: false,
                history_len: 0,
                opener: None,
                created_ts_ms: 0,
            }],
            ..Default::default()
        }
    }

    fn raise(tab: &str, payload: &str) {
        super::super::cdp::note_event(tab, "Runtime.bindingCalled", &event(payload));
    }

    fn grant(origin: &str) {
        super::super::permissions::note_answer(
            origin,
            &[eave_core::browser::permissions::PermissionKind::Notifications],
            super::super::permissions::PermissionDecision::Allow,
        );
    }

    fn calls() -> Vec<(String, String, String)> {
        super::super::take_cdp_calls_for_test()
    }

    #[test]
    fn a_granted_notification_goes_up_and_only_then_is_acked() {
        reset();
        let origin = "https://granted.example";
        grant(origin);
        let snap = snapshot("t1", "https://granted.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        let _install = calls();

        raise(
            "t1",
            r#"{"kind":"show","seq":5,"title":"Build done","body":"3 tests"}"#,
        );
        drive(&snap, now);

        let up = live();
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].title, "Build done");
        assert_eq!(up[0].origin, origin, "the origin is eave's, from the tab");

        let acks: Vec<String> = calls().into_iter().map(|(_, _, p)| p).collect();
        assert!(
            acks.iter().any(|p| p.contains("(5,") && p.contains("show")),
            "expected a show ack, got {acks:?}"
        );
    }

    /// The G2 defect, as a test of the whole loop rather than of the shim.
    #[test]
    fn an_ungranted_notification_is_blocked_and_never_goes_up() {
        reset();
        let snap = snapshot("t2", "https://ungranted.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        let _ = calls();

        raise("t2", r#"{"kind":"show","seq":1,"title":"trust me"}"#);
        drive(&snap, now);

        assert!(live().is_empty(), "nothing may be shown without a grant");
        let sent = calls();
        assert!(
            sent.iter().any(|(_, _, p)| p.contains("error")),
            "the page must be told it was blocked, not left waiting: {sent:?}"
        );
    }

    /// **The gap that made the whole capability unreachable.**
    ///
    /// The shim replaces `window.Notification`, so `requestPermission()`
    /// never reaches Chromium and Chromium never prompts — and eave forces
    /// its notification content setting to BLOCK besides. If eave does not
    /// raise its own prompt, there is NO path by which a person can grant,
    /// so every notification is refused for want of a permission nobody
    /// could give. Every other test in this file passed with that hole in
    /// it, because they all seed the grant directly.
    #[test]
    fn asking_for_permission_raises_eaves_own_prompt() {
        reset();
        super::super::ask::take("t-ask");
        let snap = snapshot("t-ask", "https://unanswered.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        raise("t-ask", r#"{"kind":"request","seq":1}"#);
        drive(&snap, now);

        let pending = super::super::ask::pending("t-ask").expect(
            "requestPermission must put a prompt on screen; without one the \
             permission can never be granted by any path a person has",
        );
        assert_eq!(pending.origin, "https://unanswered.example");
        assert!(
            matches!(&pending.ask, super::super::ask::Ask::Permission { kinds }
                if kinds == &[eave_core::browser::permissions::PermissionKind::Notifications]),
            "{:?}",
            pending.ask
        );
        super::super::ask::take("t-ask");
    }

    /// **Found by launching, not by testing.** A page that asks before its
    /// tab's URL reaches the snapshot used to hear `denied` — a refusal
    /// nobody made, which the page caches and acts on for ever. Seen live:
    /// the test page read `permission: denied` while eave's own prompt for
    /// that very origin was still on screen behind it.
    #[test]
    fn an_origin_eave_cannot_name_yet_is_default_not_denied() {
        reset();
        super::super::ask::take("t-noorigin");
        // A page tab with no URL yet — exactly the boot-race shape.
        let mut snap = snapshot("t-noorigin", "https://x.example/");
        snap.tabs[0].url = None;
        let now = std::time::Instant::now();
        drive(&snap, now);
        let _ = calls();
        raise("t-noorigin", r#"{"kind":"request","seq":1}"#);
        drive(&snap, now);

        let sent = calls();
        assert!(
            sent.iter().any(|(_, _, p)| p.contains("default")),
            "a transient miss must be re-askable, not a refusal: {sent:?}"
        );
        assert!(
            !sent.iter().any(|(_, _, p)| p.contains("denied")),
            "eave must not report a refusal no person made: {sent:?}"
        );
    }

    /// An answered origin is not asked again — G1's rule, through this path.
    #[test]
    fn an_origin_that_already_answered_is_not_prompted_again() {
        reset();
        super::super::ask::take("t-ask2");
        let origin = "https://already.example";
        grant(origin);
        let snap = snapshot("t-ask2", "https://already.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        let _ = calls();
        raise("t-ask2", r#"{"kind":"request","seq":1}"#);
        drive(&snap, now);

        assert!(
            super::super::ask::pending("t-ask2").is_none(),
            "a stored answer must not raise a second prompt"
        );
        assert!(
            calls().iter().any(|(_, _, p)| p.contains("granted")),
            "the page must be told the answer it already has"
        );
    }

    /// Once the prompt is gone the page is told, whichever way it went.
    #[test]
    fn the_page_hears_the_answer_once_the_prompt_closes() {
        reset();
        super::super::ask::take("t-ask3");
        let snap = snapshot("t-ask3", "https://waiting.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        raise("t-ask3", r#"{"kind":"request","seq":4}"#);
        drive(&snap, now);
        let _ = calls();
        assert!(super::super::ask::pending("t-ask3").is_some());

        // A person answers: the ask leaves the screen and the store gains
        // the decision. That is what `dialogs::answer` does for a prompt
        // eave raised itself.
        super::super::ask::take("t-ask3");
        grant("https://waiting.example");
        drive(&snap, now);

        assert!(
            calls().iter().any(|(_, _, p)| p.contains("granted")),
            "a promise left open is a page stalled for ever"
        );
    }

    /// The security property, exercised through the real path.
    #[test]
    fn a_page_cannot_forge_the_origin_it_notifies_as() {
        reset();
        grant("https://real.example");
        let snap = snapshot("t3", "https://real.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        let _ = calls();

        raise(
            "t3",
            r#"{"kind":"show","seq":1,"title":"Your bank","origin":"https://bank.example"}"#,
        );
        drive(&snap, now);

        let up = live();
        assert_eq!(up.len(), 1);
        assert_eq!(
            up[0].origin, "https://real.example",
            "the payload's origin must never reach the card"
        );
    }

    #[test]
    fn a_page_with_no_origin_eave_can_name_cannot_notify() {
        reset();
        // `about:blank`, `file:` and a data URL have no origin a person
        // could act on, so the card would have nothing true to put on its
        // first line.
        for url in ["about:blank", "file:///tmp/x.html", "data:text/html,hi"] {
            assert_eq!(origin_of(url), None, "{url}");
        }
        assert_eq!(
            origin_of("https://example.com/a/b?c#d").as_deref(),
            Some("https://example.com"),
            "scheme and host only — a path is not identity"
        );
    }

    #[test]
    fn a_second_notification_with_the_same_tag_replaces_the_first() {
        reset();
        grant("https://tagged.example");
        let snap = snapshot("t4", "https://tagged.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);

        raise(
            "t4",
            r#"{"kind":"show","seq":1,"title":"1 of 3","tag":"progress"}"#,
        );
        raise(
            "t4",
            r#"{"kind":"show","seq":2,"title":"2 of 3","tag":"progress"}"#,
        );
        drive(&snap, now);

        let up = live();
        assert_eq!(up.len(), 1, "a tagged notification replaces, not stacks");
        assert_eq!(up[0].title, "2 of 3");
    }

    #[test]
    fn a_page_cannot_park_notifications_on_the_corner_for_ever() {
        reset();
        grant("https://loud.example");
        let snap = snapshot("t5", "https://loud.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        for seq in 1..=10 {
            raise(
                "t5",
                &format!(r#"{{"kind":"show","seq":{seq},"title":"n{seq}"}}"#),
            );
        }
        drive(&snap, now);
        assert_eq!(live().len(), TOAST_MAX, "the stack is capped");
    }

    #[test]
    fn a_toast_comes_down_by_itself_and_the_page_is_told() {
        reset();
        grant("https://timed.example");
        let snap = snapshot("t6", "https://timed.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        raise("t6", r#"{"kind":"show","seq":3,"title":"hi"}"#);
        drive(&snap, now);
        assert_eq!(live().len(), 1);
        let _ = calls();

        let later = now + std::time::Duration::from_millis(TOAST_MS + 1);
        drive(&snap, later);
        assert!(live().is_empty(), "it should have expired");
        assert!(
            calls().iter().any(|(_, _, p)| p.contains("close")),
            "a page that is waiting on onclose must hear it"
        );
    }

    #[test]
    fn the_shim_is_installed_once_per_document_not_once_per_frame() {
        reset();
        let snap = snapshot("t7", "https://once.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        let first = calls().len();
        assert!(first >= install_calls().len(), "the first frame installs");
        drive(&snap, now);
        drive(&snap, now);
        assert_eq!(calls().len(), 0, "later frames must not re-send install");

        // ...but a new document does need it again.
        let moved = snapshot("t7", "https://once.example/other");
        drive(&moved, now);
        assert!(!calls().is_empty(), "navigation reinstalls");
    }

    #[test]
    fn a_tab_that_goes_away_takes_its_toasts_with_it() {
        reset();
        grant("https://closing.example");
        let snap = snapshot("t8", "https://closing.example/app");
        let now = std::time::Instant::now();
        drive(&snap, now);
        raise("t8", r#"{"kind":"show","seq":1,"title":"hi"}"#);
        drive(&snap, now);
        assert_eq!(live().len(), 1);
        let _ = calls();

        forget_tab("t8");
        assert!(live().is_empty());
        assert!(
            calls().is_empty(),
            "no ack to a browser that is gone — it would answer NotSent on a \
             later pump for nothing"
        );
    }

    #[test]
    fn a_page_asking_to_show_is_understood() {
        let r = show(r#"{"kind":"show","seq":7,"title":"Build done","body":"3 tests"}"#);
        assert_eq!(r.seq, 7);
        assert_eq!(r.title, "Build done");
        assert_eq!(r.body, "3 tests");
    }

    #[test]
    fn the_other_two_kinds_are_understood_too() {
        assert_eq!(
            parse_binding_called(&event(r#"{"kind":"request","seq":2}"#)),
            Some(Message::Request { seq: 2 })
        );
        assert_eq!(
            parse_binding_called(&event(r#"{"kind":"close","seq":9}"#)),
            Some(Message::Close { seq: 9 })
        );
    }

    #[test]
    fn a_binding_that_is_not_ours_is_not_ours() {
        // Another consumer's binding on the same page. Answering it would
        // be reading someone else's mail, and the name check is the only
        // thing standing between us and that.
        let other = json!({"name": "__someoneElse", "payload": r#"{"kind":"show"}"#}).to_string();
        assert_eq!(parse_binding_called(&other), None);
    }

    #[test]
    fn a_page_calling_the_raw_binding_with_garbage_is_not_an_eave_error() {
        // Every one of these is reachable from the page's own console.
        for payload in ["", "not json", "[]", "null", r#"{"kind":"invent"}"#, "{}"] {
            assert_eq!(
                parse_binding_called(&event(payload)),
                None,
                "payload {payload:?}"
            );
        }
        assert_eq!(parse_binding_called("not json at all"), None);
        assert_eq!(parse_binding_called("{}"), None);
    }

    #[test]
    fn a_missing_title_is_empty_rather_than_a_refusal() {
        // `new Notification()` with no arguments is a page bug, not an
        // attack, and it should reach the same refusal path as anything
        // else rather than vanishing before eave sees it.
        let r = show(r#"{"kind":"show","seq":1}"#);
        assert_eq!(r.title, "");
        assert_eq!(r.body, "");
    }

    #[test]
    fn strings_are_clamped_in_characters_so_a_split_char_cannot_panic() {
        let payload = json!({
            "kind": "show",
            "seq": 1,
            "title": "\u{1f600}".repeat(TITLE_MAX * 2),
            "body": "x".repeat(BODY_MAX * 10),
        })
        .to_string();
        let r = show(&payload);
        assert_eq!(r.title.chars().count(), TITLE_MAX);
        assert_eq!(r.body.chars().count(), BODY_MAX);
    }

    #[test]
    fn a_title_cannot_smuggle_a_line_break_into_the_layout() {
        let r = show(r#"{"kind":"show","seq":1,"title":"one\ntwo\r\nthree"}"#);
        assert!(
            !r.title.contains('\n') && !r.title.contains('\r'),
            "{:?}",
            r.title
        );
    }

    /// The load-bearing security property of the whole slice.
    #[test]
    fn the_payload_cannot_say_who_it_is_from() {
        let r = show(r#"{"kind":"show","seq":1,"title":"t","origin":"https://bank.example"}"#);
        // Not "the origin was ignored" — there is nowhere for it to land.
        // If a field is ever added to `Request` for it, this stops compiling
        // rather than silently starting to trust the page.
        let Request {
            seq,
            title,
            body,
            tag,
        } = r;
        let _ = (seq, title, body, tag);
    }

    #[test]
    fn install_enables_the_domains_before_it_needs_them() {
        let calls = install_calls();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| *m).collect();
        let at = |m: &str| methods.iter().position(|x| *x == m).expect(m);
        // Runtime.enable before addBinding: the binding installs either way
        // and the events simply never arrive.
        assert!(at("Runtime.enable") < at("Runtime.addBinding"));
        // Page.enable before the new-document script, or it does not take.
        assert!(at("Page.enable") < at("Page.addScriptToEvaluateOnNewDocument"));
    }

    #[test]
    fn install_covers_the_page_that_is_already_open() {
        // addScriptToEvaluateOnNewDocument does nothing for the current
        // document, so a shim installed only that way works on the SECOND
        // page a person visits and there is no error in between.
        let calls = install_calls();
        let evaluated = calls
            .iter()
            .find(|(m, _)| *m == "Runtime.evaluate")
            .expect("the current document needs the shim too");
        let source = evaluated.1["expression"].as_str().unwrap();
        let installed = calls
            .iter()
            .find(|(m, _)| *m == "Page.addScriptToEvaluateOnNewDocument")
            .unwrap();
        assert_eq!(source, installed.1["source"].as_str().unwrap());
    }

    #[test]
    fn the_shim_survives_being_installed_twice() {
        // Both install paths carry the same source and both can run on one
        // document, so re-entry must be a no-op rather than a second
        // Notification class with an empty pending map.
        assert!(shim_source().contains(&format!("if (globalThis.{ACK}) return;")));
    }

    #[test]
    fn an_ack_names_a_verb_the_shim_switches_on() {
        for (ack, verb) in [
            (Ack::Shown, "show"),
            (Ack::Blocked, "error"),
            (Ack::Closed, "close"),
        ] {
            assert_eq!(ack.verb(), verb);
            let (method, params) = ack_call(4, ack);
            assert_eq!(method, "Runtime.evaluate");
            let expression = params["expression"].as_str().unwrap();
            assert!(
                expression.contains(&format!("({},\"{}\")", 4, verb)),
                "{expression}"
            );
            // The shim may not be installed yet on a page that navigated
            // mid-notification; an unguarded call is a page-visible
            // TypeError in someone else's console.
            assert!(
                expression.starts_with(&format!("globalThis.{ACK}&&")),
                "{expression}"
            );
        }
    }

    #[test]
    fn the_shim_does_not_raise_show_when_it_posts() {
        // G2 in one assertion: the defect was `onshow` firing for a
        // notification that never displayed. The only place the shim may
        // deliver "show" is from the ack handler.
        let source = shim_source();
        let posted = source.find("kind: \"show\"").expect("the post");
        let before = &source[..posted];
        assert!(
            !before.contains("deliver(seq, \"show\")"),
            "the shim must not report a show it has not been told about"
        );
    }

    /// Hand the shim to a real JavaScript engine.
    ///
    /// Everything else in this module tests the Rust side, and the tests
    /// that reach into [`shim_source`] do it by **searching the text** —
    /// which cannot tell a working shim from a syntactically broken one.
    /// The failure that would matter most here is a shim that never parses,
    /// and its symptom is silence in a page nobody is watching.
    ///
    /// So the checks that need an engine live outside cargo, in
    /// `scripts/check-notify-shim.mjs`, and this writes the input for them:
    ///
    /// ```text
    /// EAVE_DUMP_SHIM=/tmp/shim.js cargo test -p eave-app web::notify
    /// node scripts/check-notify-shim.mjs /tmp/shim.js
    /// ```
    ///
    /// A no-op without the variable, so the suite does not grow a
    /// dependency on node being installed.
    #[test]
    fn the_shim_can_be_handed_to_a_real_engine() {
        if let Ok(path) = std::env::var("EAVE_DUMP_SHIM") {
            std::fs::write(path, shim_source()).expect("dump the shim");
        }
    }

    #[test]
    fn a_page_that_cannot_reach_eave_is_denied_rather_than_left_hanging() {
        // requestPermission() returns a Promise the page awaits. With no
        // binding there is no answer coming, and a pending promise stalls
        // whatever the page was going to do next.
        assert!(shim_source().contains("Promise.resolve(\"denied\")"));
    }
}
