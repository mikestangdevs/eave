//! C4's other half: which tabs are making a noise.
//!
//! # Why this is not `AudioHandler`
//! CEF hands the embedder exactly one audio seam, and it is the wrong shape.
//! `CefAudioHandler::GetAudioParameters` returning true means "route this
//! page's audio to me as PCM" — **instead of** to the speakers. So the one
//! call that would tell eave a tab is audible is the same call that silences
//! it, and `PageAudioHandler` in `tabs.rs` declines for that reason. The
//! stop edge is unavailable for the same reason: `OnAudioStreamStopped`
//! fires only for a capture that was accepted.
//!
//! An earlier note in this slice concluded from that that the indicator was
//! impossible. **That was wrong, and it was wrong in the way this codebase
//! keeps being wrong**: it read one API, found the door shut, and reported
//! the room empty. The question is not "does `AudioHandler` expose this", it
//! is "can eave learn it at all" — and eave has a whole second channel into
//! every page.
//!
//! # The seam is CDP, exactly as it was for notifications
//! `web/notify.rs` had the same shape of problem — CEF alloy has no
//! notification API — and solved it by shimming the Web API itself over
//! E9's channel. This does the same thing one level down: a script on every
//! document watches the two ways a page can make sound and reports when the
//! answer changes.
//!
//! # What it covers, and what it does not
//! **Covered:** `<video>` and `<audio>` in any frame the shim reaches
//! (YouTube, Netflix, Spotify's player, a podcast page), and Web Audio
//! source nodes — oscillators and buffer sources — which is how games,
//! meeting apps and synths make sound.
//!
//! **Not covered**, and stated here rather than discovered later:
//!
//! - **Cross-origin iframes.** The shim installs into the frames CDP gives
//!   it; a video in a cross-origin embed can play unheard by this.
//! - **Audio from a worker**, and any path that reaches the output without
//!   going through a media element or a source node's `start()`.
//! - **A page that lies.** The binding is a real global (see
//!   `cdp::add_binding`), so a page can call it with anything. The worst it
//!   buys is a wrong speaker icon on its own tab, which is why nothing but
//!   the icon is derived from this.
//!
//! Chrome's own indicator reads `WebContents::IsCurrentlyAudible()`, at the
//! audio-output level, and cannot be wrong. This can be. It is the best
//! answer available through CEF, and the gap is the honest cost.

use serde_json::Value;

/// The page-side global this shim posts through.
///
/// Distinct from `notify`'s: each `Runtime.addBinding` is its own global,
/// and two consumers sharing one name would interleave payloads.
pub const BINDING: &str = "__eave_audible";

/// Everything a tab needs before it can report audibility.
///
/// The order is `notify::install_calls`' order and for the same reasons —
/// `Runtime.enable` before the binding or `bindingCalled` never arrives,
/// `Page.enable` before the new-document script will take, and an
/// `evaluate` of the same source at the end for the document that is
/// **already open**. A shim installed only for the next navigation is a
/// shim that never runs on the page a person is looking at.
pub fn install_calls() -> Vec<(&'static str, Value)> {
    let source = shim_source();
    vec![
        ("Runtime.enable", serde_json::json!({})),
        ("Page.enable", serde_json::json!({})),
        super::cdp::add_binding(BINDING),
        super::cdp::add_script_on_new_document(&source),
        super::cdp::evaluate(&source),
    ]
}

/// The page-side watcher.
///
/// Three things are worth knowing about this script:
///
/// 1. **Media events are caught in the CAPTURE phase.** `play`, `pause`,
///    `playing` and `ended` do not bubble, so a listener on `document`
///    only ever sees them by capturing on the way down. A bubble-phase
///    listener here would compile, run, and never fire once.
/// 2. **Web Audio is counted at `start()`/`ended`, not at context state.**
///    An `AudioContext` in state `running` with nothing connected is
///    silent, and treating `running` as audible puts a speaker on every
///    page that merely constructed one. `AudioScheduledSourceNode.start`
///    covers oscillators and buffer sources, which is where the sound
///    actually begins.
/// 3. **It reports on CHANGE**, not on every event. A playing video fires
///    `timeupdate` several times a second; posting each one would be a
///    binding call per frame for a value that moves twice a session.
pub fn shim_source() -> String {
    format!(
        r#"(function(){{
  if (globalThis.{BINDING}__installed) return;
  globalThis.{BINDING}__installed = true;
  var sources = 0;
  var last = null;
  function mediaAudible() {{
    var all = document.querySelectorAll("video,audio");
    for (var i = 0; i < all.length; i++) {{
      var m = all[i];
      if (!m.paused && !m.ended && !m.muted && m.volume > 0) return true;
    }}
    return false;
  }}
  function report() {{
    var now = mediaAudible() || sources > 0;
    if (now === last) return;
    last = now;
    var fn = globalThis.{BINDING};
    if (typeof fn !== "function") return;
    try {{ fn(JSON.stringify({{audible: now}})); }} catch (e) {{}}
  }}
  // Capture phase: these events do not bubble.
  ["play","playing","pause","ended","volumechange","emptied","abort"]
    .forEach(function(t){{ document.addEventListener(t, report, true); }});
  try {{
    var proto = globalThis.AudioScheduledSourceNode &&
                globalThis.AudioScheduledSourceNode.prototype;
    if (proto && proto.start && !proto.{BINDING}__patched) {{
      var start = proto.start, stop = proto.stop;
      var settle = function(node) {{
        if (node.{BINDING}__done) return;
        node.{BINDING}__done = true;
        sources = Math.max(0, sources - 1);
        report();
      }};
      proto.start = function(){{
        var r = start.apply(this, arguments);
        if (!this.{BINDING}__live) {{
          this.{BINDING}__live = true;
          sources++;
          var self = this;
          this.addEventListener("ended", function(){{ settle(self); }});
          report();
        }}
        return r;
      }};
      proto.stop = function(){{
        var r = stop.apply(this, arguments);
        // `ended` may never fire for a node stopped before it started, so
        // settle here as well; `__done` keeps the count from going twice.
        var self = this;
        if (self.{BINDING}__live) settle(self);
        return r;
      }};
      proto.{BINDING}__patched = true;
    }}
  }} catch (e) {{}}
  // The document may already be playing when this installs — an
  // `evaluate` into a loaded page is exactly that case.
  report();
}})();"#
    )
}

/// Read one audibility report. `None` for anything that is not ours.
///
/// Pure and ungated so the default suite runs it: what the page said is a
/// decision about a JSON string, and none of it needs Chromium. The payload
/// is a **string** containing JSON, so this parses twice and either step can
/// fail on a page that calls the raw global with garbage — both failures are
/// `None`, because a page misusing its own global is not an eave error.
pub fn parse_audible(params: &str) -> Option<bool> {
    if super::cdp::binding_name(params).as_deref() != Some(BINDING) {
        return None;
    }
    let event: Value = serde_json::from_str(params).ok()?;
    let payload: Value = serde_json::from_str(event.get("payload")?.as_str()?).ok()?;
    payload.get("audible")?.as_bool()
}

thread_local! {
    /// Which tabs reported themselves audible, and where the shim stands.
    ///
    /// Main-thread only, like every other map in `web/`: every writer is a
    /// CDP event handled on the UI thread and every reader is a gpui
    /// render. This is NOT the `RwLock` shape `permissions.rs` needed —
    /// that one is written by a tokio task.
    static AUDIBLE: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// Tab id → the url the shim was installed at.
    static INSTALLED: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Whether this tab is making a noise, as far as the page has said.
pub fn is_audible(tab_id: &str) -> bool {
    AUDIBLE.with(|a| a.borrow().contains(tab_id))
}

/// One pass: install where needed, then fold what has arrived.
pub fn drive(snapshot: &eave_wire::TabsSnapshot) {
    install_where_needed(snapshot);
    handle_events();
}

fn install_where_needed(snapshot: &eave_wire::TabsSnapshot) {
    for tab in &snapshot.tabs {
        if tab.content != eave_wire::TabContent::Page {
            continue;
        }
        let Some(url) = tab.url.as_deref() else {
            continue;
        };
        // Same gate as `notify`: a url eave cannot name an origin for is
        // `about:blank` or a `data:` — nothing to watch and nothing to
        // install into.
        if super::notify::origin_of(url).is_none() {
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
        // A new document is silent until it says otherwise. Without this a
        // tab that navigates away from a playing video keeps its speaker
        // for ever: the old document's `pause` never fires, because the old
        // document is gone.
        AUDIBLE.with(|a| {
            a.borrow_mut().remove(&id);
        });
        for (method, params) in install_calls() {
            super::cdp_call(&id, method, &params);
        }
    }
}

/// Fold every audibility report that has arrived since the last pass.
///
/// Takes only events naming [`BINDING`] — see `cdp::binding_name` for why
/// the filter has to be that specific.
fn handle_events() {
    let events = super::cdp::take_events_matching(|e| {
        e.method == "Runtime.bindingCalled"
            && super::cdp::binding_name(&e.params).as_deref() == Some(BINDING)
    });
    for event in events {
        let Some(audible) = parse_audible(&event.params) else {
            continue;
        };
        AUDIBLE.with(|a| {
            let mut a = a.borrow_mut();
            if audible {
                a.insert(event.tab_id.clone());
            } else {
                a.remove(&event.tab_id);
            }
        });
        // The speaker appears and disappears between CEF frames, and a
        // static page paints twice and then stops — so without this the
        // icon would wait for whatever redrew next. Gated because `host` is:
        // feature-off there is no pump to nudge and no Chromium to nudge it
        // for, while everything above this line still runs and is asserted.
        #[cfg(feature = "web")]
        super::host::note_chrome_change();
    }
}

/// Put a tab into the audible set directly, for the harness.
///
/// The real path is a `Runtime.bindingCalled` from a document, which needs
/// a Chromium no test process has. Both are the same one-line write, and
/// [`parse_audible`] covers the half this skips.
#[cfg(test)]
pub fn set_for_test(tab_id: &str, audible: bool) {
    AUDIBLE.with(|a| {
        let mut a = a.borrow_mut();
        if audible {
            a.insert(tab_id.to_string());
        } else {
            a.remove(tab_id);
        }
    });
}

/// A tab went away.
pub fn forget_tab(tab_id: &str) {
    AUDIBLE.with(|a| {
        a.borrow_mut().remove(tab_id);
    });
    INSTALLED.with(|i| {
        i.borrow_mut().remove(tab_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(payload: &str, name: &str) -> String {
        serde_json::json!({ "name": name, "payload": payload }).to_string()
    }

    /// The parse takes its own binding's reports and nothing else.
    ///
    /// The rejection half is the one that matters: `notify` posts through a
    /// different global into the SAME event queue, and a parse that read any
    /// `bindingCalled` would turn a notification into an audibility report.
    #[test]
    fn only_this_bindings_reports_are_read() {
        assert_eq!(
            parse_audible(&event(r#"{"audible":true}"#, BINDING)),
            Some(true)
        );
        assert_eq!(
            parse_audible(&event(r#"{"audible":false}"#, BINDING)),
            Some(false)
        );
        // notify's binding, with a payload that would otherwise parse.
        assert_eq!(
            parse_audible(&event(r#"{"audible":true}"#, super::super::notify::BINDING)),
            None,
            "another consumer's binding was read as an audibility report"
        );
        // …and the two names really are different, or the assertion above
        // holds for a reason that has nothing to do with the check.
        assert_ne!(BINDING, super::super::notify::BINDING);
    }

    /// A page calling its own global with rubbish is not an eave error.
    #[test]
    fn garbage_from_a_page_is_refused_rather_than_believed() {
        for bad in [
            "",
            "not json",
            r#"{"name":"__eave_audible"}"#,
            &event("not json", BINDING),
            &event(r#"{"audible":"yes"}"#, BINDING),
            &event(r#"{"other":1}"#, BINDING),
        ] {
            assert_eq!(parse_audible(bad), None, "believed {bad:?}");
        }
    }

    /// The shim has to survive being run twice — it is sent as BOTH a
    /// new-document script and an `evaluate` for the page already open, so
    /// the second document of every session runs it twice.
    #[test]
    fn the_shim_guards_against_installing_itself_twice() {
        let source = shim_source();
        assert!(source.contains(&format!("{BINDING}__installed")));
        // Capture phase, because these events do not bubble. Asserted
        // because `true` here is one character and its absence is silent.
        assert!(
            source.contains("addEventListener(t, report, true)"),
            "media listeners must capture — play/pause do not bubble"
        );
        // And the context-state shortcut is NOT taken: a running
        // AudioContext with nothing connected is silent.
        assert!(
            !source.contains("state === \"running\""),
            "audibility must not be inferred from AudioContext state"
        );
    }

    /// `Runtime.enable` before the binding, `Page.enable` before the
    /// new-document script — both orderings are silent when wrong.
    #[test]
    fn install_enables_the_domains_before_it_needs_them() {
        let calls = install_calls();
        let at = |m: &str| calls.iter().position(|(k, _)| *k == m).expect(m);
        assert!(at("Runtime.enable") < at("Runtime.addBinding"));
        assert!(at("Page.enable") < at("Page.addScriptToEvaluateOnNewDocument"));
        // The already-open document is covered too, and it must come after
        // the binding exists or the first report has nowhere to go.
        assert!(at("Runtime.addBinding") < at("Runtime.evaluate"));
    }
}
