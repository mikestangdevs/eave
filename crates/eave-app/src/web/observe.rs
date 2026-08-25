//! What a page did, kept where an agent can read it — slice E8.
//!
//! E9 made a page **drivable**; this makes it **legible**. The spec is
//! explicit about who the consumer is — *the agent reads requests,
//! responses, console* — so this is a store an agent queries, not a panel.
//! Nothing here draws anything, which is also why it can land while four
//! sessions are inside `ui.rs`.
//!
//! # The mechanism was already finished and producing into a bin
//! `inspector`'s observer has received every CDP event since `7f2120b`, and
//! [`super::cdp::note_event`] has been queueing them behind a counted 512
//! cap the whole time. **`drain_events()` had zero callers.** Worse,
//! `notify` sends `Runtime.enable` to install its shim, so
//! `Runtime.consoleAPICalled` has been firing into that queue and being
//! evicted unread. E8 is a consumer, not a mechanism — the spec said so and
//! it was right.
//!
//! # Two consumers, one queue, and neither may eat the other
//! `notify` takes only `Runtime.bindingCalled`. This takes the console and
//! network families. Both go through
//! [`super::cdp::take_events_matching`], which leaves what it does not
//! want — a plain `drain_events()` from either would silently starve the
//! other, with no error and no log line.
//!
//! # A page is not the only thing writing here, and the cap is shared
//! The docked DevTools frontend attaches to the same agent and enables
//! domains for **itself**, so its traffic lands in the same queue. That is
//! `cdp::dropped_events`' documented hazard and it applies to this
//! consumer directly: on a busy page with Inspect open, this store can miss
//! lines because someone opened a panel. Per-tab caps here bound the
//! *store*; they do nothing about the *queue* upstream. The fix, when
//! somebody has measured it, is the interest registry named in
//! `take_events_matching`.
//!
//! # Everything a page says is a page's words
//! A console line is authored by the page, exactly like a notification
//! title. Nothing here is trusted, nothing is interpreted as an
//! instruction, and every string is clamped before it is stored. The tab a
//! line is attributed to comes from the event's own tab, which eave
//! assigned — never from the payload.

use serde_json::Value;

/// Console lines kept per tab.
///
/// A loop logging every frame is normal, not pathological, so this is a
/// ring and not a limit — the newest lines are the ones an agent asked
/// about.
pub const CONSOLE_CAP: usize = 200;

/// Requests kept per tab. A single page load is routinely 100+.
pub const NETWORK_CAP: usize = 300;

/// Longest single console line kept. A page can `console.log` a megabyte.
pub const TEXT_MAX: usize = 2_000;

/// Longest URL kept. Data URLs are unbounded by nature.
pub const URL_MAX: usize = 500;

/// One line a page printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    /// `log`, `warn`, `error`, `info`, `debug` — or whatever the page used.
    pub level: String,
    /// The message, already flattened and clamped.
    pub text: String,
    /// Where it came from, when Chromium says. Empty when it does not.
    pub source: String,
}

/// One request a page made, folded from up to three CDP events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// Chromium's id, which is how the three events find each other.
    pub request_id: String,
    pub method: String,
    pub url: String,
    /// `None` until the response arrives — and for ever if it never does,
    /// which is the honest record of a request that hung.
    pub status: Option<u16>,
    pub mime: String,
    /// Encoded bytes, once Chromium reports the transfer finished.
    pub bytes: Option<u64>,
    /// Chromium's own words when it failed. Empty when it did not.
    pub failed: String,
}

thread_local! {
    static CONSOLE: std::cell::RefCell<
        std::collections::HashMap<String, std::collections::VecDeque<ConsoleLine>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static NETWORK: std::cell::RefCell<
        std::collections::HashMap<String, std::collections::VecDeque<Exchange>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    /// Tabs whose domains are on. Keyed by tab, not document: `Network.enable`
    /// is a property of the agent's session, and re-sending it every frame
    /// would be three CDP calls per frame per tab.
    static WATCHED: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Is this an event this consumer wants?
///
/// Named rather than inlined so the *other* consumer's predicate and this
/// one can be read side by side — the bug that matters is an event both
/// claim, or one neither does.
pub fn wanted(method: &str) -> bool {
    matches!(
        method,
        "Runtime.consoleAPICalled"
            | "Runtime.exceptionThrown"
            | "Log.entryAdded"
            | "Network.requestWillBeSent"
            | "Network.responseReceived"
            | "Network.loadingFinished"
            | "Network.loadingFailed"
    )
}

fn clamp(value: &str, max: usize) -> String {
    // Chars, not bytes: slicing a multi-byte character panics, and a page
    // logging emoji is not an edge case.
    value.chars().take(max).collect()
}

/// One turn: enable what is not yet enabled, then fold what has arrived.
pub fn drive(snapshot: &eave_wire::TabsSnapshot) {
    watch_new_tabs(snapshot);
    for event in super::cdp::take_events_matching(|e| wanted(&e.method)) {
        let Ok(params) = serde_json::from_str::<Value>(&event.params) else {
            continue;
        };
        fold(&event.tab_id, &event.method, &params);
    }
}

/// Turn the reporting domains on for any page tab that does not have them.
///
/// **Nothing is reported until this runs**, and the silence is
/// indistinguishable from a page doing nothing — which is exactly how this
/// row sat "most of the way there" with `cdp_watch` called only from a test.
fn watch_new_tabs(snapshot: &eave_wire::TabsSnapshot) {
    for tab in &snapshot.tabs {
        if tab.content != eave_wire::TabContent::Page {
            continue;
        }
        let id = tab.id.to_string();
        if WATCHED.with(|w| w.borrow().contains(&id)) {
            continue;
        }
        WATCHED.with(|w| {
            w.borrow_mut().insert(id.clone());
        });
        super::cdp_watch(&id);
    }
}

fn fold(tab_id: &str, method: &str, params: &Value) {
    match method {
        "Runtime.consoleAPICalled" => {
            let level = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("log")
                .to_string();
            push_line(
                tab_id,
                ConsoleLine {
                    level,
                    text: clamp(&flatten_args(params.get("args")), TEXT_MAX),
                    source: String::new(),
                },
            );
        }
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails");
            // `exception.description` carries the stack; `text` alone is
            // usually the bare word "Uncaught", which tells an agent nothing.
            let text = details
                .and_then(|d| d.get("exception"))
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.and_then(|d| d.get("text")).and_then(Value::as_str))
                .unwrap_or("");
            push_line(
                tab_id,
                ConsoleLine {
                    level: "error".to_string(),
                    text: clamp(text, TEXT_MAX),
                    source: details
                        .and_then(|d| d.get("url"))
                        .and_then(Value::as_str)
                        .map(|u| clamp(u, URL_MAX))
                        .unwrap_or_default(),
                },
            );
        }
        "Log.entryAdded" => {
            let Some(entry) = params.get("entry") else {
                return;
            };
            push_line(
                tab_id,
                ConsoleLine {
                    level: entry
                        .get("level")
                        .and_then(Value::as_str)
                        .unwrap_or("info")
                        .to_string(),
                    text: clamp(
                        entry.get("text").and_then(Value::as_str).unwrap_or(""),
                        TEXT_MAX,
                    ),
                    source: entry
                        .get("url")
                        .and_then(Value::as_str)
                        .map(|u| clamp(u, URL_MAX))
                        .unwrap_or_default(),
                },
            );
        }
        "Network.requestWillBeSent" => {
            let Some(id) = params.get("requestId").and_then(Value::as_str) else {
                return;
            };
            let request = params.get("request");
            push_exchange(
                tab_id,
                Exchange {
                    request_id: id.to_string(),
                    method: request
                        .and_then(|r| r.get("method"))
                        .and_then(Value::as_str)
                        .unwrap_or("GET")
                        .to_string(),
                    url: clamp(
                        request
                            .and_then(|r| r.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        URL_MAX,
                    ),
                    status: None,
                    mime: String::new(),
                    bytes: None,
                    failed: String::new(),
                },
            );
        }
        "Network.responseReceived" => {
            let response = params.get("response");
            update(tab_id, params, |e| {
                e.status = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_u64)
                    .map(|s| s as u16);
                e.mime = response
                    .and_then(|r| r.get("mimeType"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            });
        }
        "Network.loadingFinished" => {
            let bytes = params.get("encodedDataLength").and_then(Value::as_u64);
            update(tab_id, params, |e| e.bytes = bytes);
        }
        "Network.loadingFailed" => {
            let text = params
                .get("errorText")
                .and_then(Value::as_str)
                .unwrap_or("failed")
                .to_string();
            update(tab_id, params, |e| e.failed = text.clone());
        }
        _ => {}
    }
}

/// Flatten `Runtime.consoleAPICalled`'s argument list into one line.
///
/// `console.log("a", 1, {x:2})` arrives as three remote objects. A value is
/// used when Chromium sent one; otherwise its `description`, which is what
/// carries objects and functions. **An argument with neither is rendered as
/// its type in angle brackets rather than dropped** — a log line that
/// silently loses a term reads as the page having printed something else.
fn flatten_args(args: Option<&Value>) -> String {
    let Some(args) = args.and_then(Value::as_array) else {
        return String::new();
    };
    args.iter()
        .map(|arg| match arg.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => arg
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "<{}>",
                        arg.get("type").and_then(Value::as_str).unwrap_or("value")
                    )
                }),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_line(tab_id: &str, line: ConsoleLine) {
    CONSOLE.with(|c| {
        let mut c = c.borrow_mut();
        let lines = c.entry(tab_id.to_string()).or_default();
        while lines.len() >= CONSOLE_CAP {
            lines.pop_front();
        }
        lines.push_back(line);
    });
}

fn push_exchange(tab_id: &str, exchange: Exchange) {
    NETWORK.with(|n| {
        let mut n = n.borrow_mut();
        let list = n.entry(tab_id.to_string()).or_default();
        while list.len() >= NETWORK_CAP {
            list.pop_front();
        }
        list.push_back(exchange);
    });
}

/// Apply a later event to the request it belongs to.
///
/// **A miss is normal and is not an error.** `responseReceived` for a
/// request whose `requestWillBeSent` was evicted by [`NETWORK_CAP`], or that
/// was in flight before the domain was enabled, has nothing to attach to.
/// Inventing a half-empty `Exchange` from a response alone would put rows
/// in the store with no URL and no method, which reads as corruption.
fn update(tab_id: &str, params: &Value, apply: impl FnOnce(&mut Exchange)) {
    let Some(id) = params.get("requestId").and_then(Value::as_str) else {
        return;
    };
    NETWORK.with(|n| {
        let mut n = n.borrow_mut();
        let Some(list) = n.get_mut(tab_id) else {
            return;
        };
        if let Some(e) = list.iter_mut().rev().find(|e| e.request_id == id) {
            apply(e);
        }
    });
}

/// What this tab printed, oldest first.
pub fn console(tab_id: &str) -> Vec<ConsoleLine> {
    CONSOLE.with(|c| {
        c.borrow()
            .get(tab_id)
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    })
}

/// What this tab fetched, oldest first.
pub fn exchanges(tab_id: &str) -> Vec<Exchange> {
    NETWORK.with(|n| {
        n.borrow()
            .get(tab_id)
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    })
}

/// A tab is gone: drop what it said, and forget that it was watched.
///
/// The watched-set entry matters as much as the data — a tab id that is
/// reused would otherwise never have its domains enabled again, and the
/// symptom is a page that reports nothing with no error anywhere.
pub fn forget_tab(tab_id: &str) {
    CONSOLE.with(|c| {
        c.borrow_mut().remove(tab_id);
    });
    NETWORK.with(|n| {
        n.borrow_mut().remove(tab_id);
    });
    WATCHED.with(|w| {
        w.borrow_mut().remove(tab_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reset() {
        CONSOLE.with(|c| c.borrow_mut().clear());
        NETWORK.with(|n| n.borrow_mut().clear());
        WATCHED.with(|w| w.borrow_mut().clear());
        let _ = super::super::cdp::drain_events();
        let _ = super::super::take_cdp_calls_for_test();
    }

    fn snapshot(tab: &str) -> eave_wire::TabsSnapshot {
        eave_wire::TabsSnapshot {
            tabs: vec![eave_wire::TabInfo {
                id: tab.into(),
                group: None,
                pinned: false,
                content: eave_wire::TabContent::Page,
                url: Some("https://example.com/".to_string()),
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

    fn raise(tab: &str, method: &str, params: Value) {
        super::super::cdp::note_event(tab, method, &params.to_string());
    }

    #[test]
    fn a_page_that_logs_is_readable_afterwards() {
        reset();
        let snap = snapshot("o1");
        drive(&snap);
        raise(
            "o1",
            "Runtime.consoleAPICalled",
            json!({"type": "warn", "args": [{"type": "string", "value": "careful"}]}),
        );
        drive(&snap);
        let lines = console("o1");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, "warn");
        assert_eq!(lines[0].text, "careful");
    }

    /// `console.log("a", 1, {x:2})` is three remote objects, not a string.
    #[test]
    fn every_argument_survives_into_the_line() {
        reset();
        let snap = snapshot("o2");
        drive(&snap);
        raise(
            "o2",
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [
                {"type": "string", "value": "count"},
                {"type": "number", "value": 3},
                {"type": "object", "description": "Object {x: 2}"},
                {"type": "function"}
            ]}),
        );
        drive(&snap);
        let text = &console("o2")[0].text;
        assert!(text.contains("count"), "{text}");
        assert!(text.contains('3'), "{text}");
        assert!(text.contains("Object"), "{text}");
        // An argument with neither value nor description is rendered as its
        // type, never dropped: a line that silently loses a term reads as
        // the page having printed something else.
        assert!(text.contains("<function>"), "{text}");
    }

    /// The three network events are one request, folded by `requestId`.
    #[test]
    fn a_request_and_its_response_are_one_row() {
        reset();
        let snap = snapshot("o3");
        drive(&snap);
        raise(
            "o3",
            "Network.requestWillBeSent",
            json!({"requestId": "R1", "request": {"url": "https://example.com/a.js", "method": "GET"}}),
        );
        raise(
            "o3",
            "Network.responseReceived",
            json!({"requestId": "R1", "response": {"status": 200, "mimeType": "text/javascript"}}),
        );
        raise(
            "o3",
            "Network.loadingFinished",
            json!({"requestId": "R1", "encodedDataLength": 4096}),
        );
        drive(&snap);
        let rows = exchanges("o3");
        assert_eq!(rows.len(), 1, "three events must fold into one row");
        assert_eq!(rows[0].url, "https://example.com/a.js");
        assert_eq!(rows[0].status, Some(200));
        assert_eq!(rows[0].bytes, Some(4096));
        assert!(rows[0].failed.is_empty());
    }

    #[test]
    fn a_request_that_never_answered_says_so_by_staying_empty() {
        reset();
        let snap = snapshot("o4");
        drive(&snap);
        raise(
            "o4",
            "Network.requestWillBeSent",
            json!({"requestId": "R1", "request": {"url": "https://slow.example/", "method": "GET"}}),
        );
        drive(&snap);
        let rows = exchanges("o4");
        assert_eq!(
            rows[0].status, None,
            "a hung request must not look like a 0"
        );
        assert_eq!(rows[0].bytes, None);
    }

    #[test]
    fn a_failure_keeps_chromiums_own_words() {
        reset();
        let snap = snapshot("o5");
        drive(&snap);
        raise(
            "o5",
            "Network.requestWillBeSent",
            json!({"requestId": "R1", "request": {"url": "https://x.example/", "method": "GET"}}),
        );
        raise(
            "o5",
            "Network.loadingFailed",
            json!({"requestId": "R1", "errorText": "net::ERR_NAME_NOT_RESOLVED"}),
        );
        drive(&snap);
        assert_eq!(exchanges("o5")[0].failed, "net::ERR_NAME_NOT_RESOLVED");
    }

    /// A response whose request was never seen has nothing to attach to.
    #[test]
    fn a_response_with_no_request_does_not_invent_a_row() {
        reset();
        let snap = snapshot("o6");
        drive(&snap);
        raise(
            "o6",
            "Network.responseReceived",
            json!({"requestId": "GHOST", "response": {"status": 200}}),
        );
        drive(&snap);
        assert!(
            exchanges("o6").is_empty(),
            "a row with no URL and no method reads as corruption, not as data"
        );
    }

    #[test]
    fn an_uncaught_error_carries_its_stack_not_the_word_uncaught() {
        reset();
        let snap = snapshot("o7");
        drive(&snap);
        raise(
            "o7",
            "Runtime.exceptionThrown",
            json!({"exceptionDetails": {
                "text": "Uncaught",
                "url": "https://example.com/a.js",
                "exception": {"description": "TypeError: x is not a function\n    at f (a.js:3)"}
            }}),
        );
        drive(&snap);
        let line = &console("o7")[0];
        assert_eq!(line.level, "error");
        assert!(line.text.contains("TypeError"), "{}", line.text);
        assert_eq!(line.source, "https://example.com/a.js");
    }

    /// The two consumers share one queue and must not eat each other.
    #[test]
    fn the_notification_binding_is_not_taken_by_this_consumer() {
        assert!(!wanted("Runtime.bindingCalled"));
        // ...and nothing this consumer wants is a binding call.
        for m in [
            "Runtime.consoleAPICalled",
            "Log.entryAdded",
            "Network.requestWillBeSent",
        ] {
            assert!(wanted(m));
        }
    }

    #[test]
    fn another_consumers_events_are_left_in_the_queue() {
        reset();
        let snap = snapshot("o8");
        drive(&snap);
        raise(
            "o8",
            "Runtime.bindingCalled",
            json!({"name": "__eaveNotify"}),
        );
        raise(
            "o8",
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [{"value": "mine"}]}),
        );
        drive(&snap);
        assert_eq!(console("o8").len(), 1);
        let left = super::super::cdp::drain_events();
        assert_eq!(left.len(), 1, "the binding call must still be queued");
        assert_eq!(left[0].method, "Runtime.bindingCalled");
    }

    #[test]
    fn the_domains_are_enabled_once_per_tab_not_once_per_frame() {
        reset();
        let snap = snapshot("o9");
        drive(&snap);
        let first = super::super::take_cdp_calls_for_test();
        assert!(
            first.iter().any(|(_, m, _)| m == "Network.enable"),
            "nothing enabled the domains; a page would report silence: {first:?}"
        );
        drive(&snap);
        drive(&snap);
        assert!(
            super::super::take_cdp_calls_for_test().is_empty(),
            "later frames must not re-enable"
        );
    }

    #[test]
    fn a_busy_page_keeps_the_newest_lines_rather_than_the_first() {
        reset();
        let snap = snapshot("o10");
        drive(&snap);
        for i in 0..(CONSOLE_CAP + 10) {
            raise(
                "o10",
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"value": format!("line {i}")}]}),
            );
        }
        drive(&snap);
        let lines = console("o10");
        assert_eq!(lines.len(), CONSOLE_CAP);
        assert!(
            lines
                .last()
                .unwrap()
                .text
                .contains(&format!("line {}", CONSOLE_CAP + 9)),
            "the ring must keep the newest: {:?}",
            lines.last()
        );
    }

    #[test]
    fn a_page_cannot_put_a_megabyte_in_one_line() {
        reset();
        let snap = snapshot("o11");
        drive(&snap);
        raise(
            "o11",
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [{"value": "\u{1f600}".repeat(TEXT_MAX * 2)}]}),
        );
        drive(&snap);
        // Chars, not bytes: a byte slice would split the emoji and panic.
        assert_eq!(console("o11")[0].text.chars().count(), TEXT_MAX);
    }

    #[test]
    fn a_tab_that_goes_away_is_forgotten_including_that_it_was_watched() {
        reset();
        let snap = snapshot("o12");
        drive(&snap);
        raise(
            "o12",
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [{"value": "hi"}]}),
        );
        drive(&snap);
        assert_eq!(console("o12").len(), 1);
        let _ = super::super::take_cdp_calls_for_test();

        forget_tab("o12");
        assert!(console("o12").is_empty());
        // The watched-set entry is the subtle half: a reused id would never
        // be enabled again and would report nothing, with no error.
        drive(&snap);
        assert!(
            super::super::take_cdp_calls_for_test()
                .iter()
                .any(|(_, m, _)| m == "Network.enable"),
            "a tab seen again must have its domains enabled again"
        );
    }
}
