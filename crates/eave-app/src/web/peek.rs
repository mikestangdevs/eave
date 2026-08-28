//! Bookmark peek — what a bookmarked service would show, before you go.
//!
//! Arc's hover previews for Gmail and Google Calendar, extended with
//! Linear: rest on a bookmark tile and a card shows the inbox, today's
//! events, or your issues, without opening the tab.
//!
//! # Where the data comes from
//!
//! **A page that already has the user's session**, read over E9's CDP
//! channel (`cdp_evaluate`). No OAuth, no API keys, no second HTTP client
//! — the same posture as `favicon` ("fetch through the browser that
//! already has the page's cookies"). Per provider the extractor is a JS
//! expression that resolves to a JSON string:
//!
//! - **Mail** — a same-origin `fetch` of Gmail's atom feed
//!   (`mail.google.com/mail/feed/atom`), which is unread messages with
//!   sender/subject/snippet/date. Structured and a decade stable, unlike
//!   Gmail's DOM.
//! - **Calendar** — the day grid's `[data-eventid]` chips, read through
//!   their accessibility text. DOM, because Google Calendar has no
//!   cookie-authenticated feed.
//! - **Linear** — the app's own GraphQL (`client-api.linear.app`) with
//!   the session credentials, falling back to whatever issue links the
//!   current view shows. UNVERIFIED against a live workspace: Linear may
//!   authenticate that endpoint with a bearer token rather than the
//!   cookie, in which case the fallback is what runs.
//!
//! The page evaluated in is an **open tab** of the provider's host when
//! one exists, else a hidden `peek:` browser created for the purpose (see
//! `tabs::ensure_peek_browser` — the `devtools:` template: a windowless
//! browser whose id the strip has never heard of).
//!
//! # Why the module is ungated
//!
//! Same reason as `notify` and `observe`: `cdp_call`'s feature-off twin
//! issues the call and a test supplies the answer, so the whole lifecycle
//! — request, issue, fold, expire — runs in the default suite. The only
//! gated line is the one that asks CEF for the hidden browser.
//!
//! # Trust
//!
//! Rows come from page JavaScript and are DATA: every string is clamped
//! by chars (the `notify` posture), nothing is interpreted, and a payload
//! that will not parse is a retry, never a panic.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use eave_wire::TabsSnapshot;

use super::cdp;

/// How long a fetched answer is fresh. Within it a hover shows the cache;
/// past it a live source is asked again (the stale rows keep showing
/// while the refetch is in flight, which is what a glanceable card wants).
const STALE_AFTER: Duration = Duration::from_secs(60);

/// How long one `request` keeps the loop interested. The tooltip renders
/// every frame while it is up, so interest is renewed continuously and
/// lapses this soon after the pointer leaves.
const INTEREST_TTL: Duration = Duration::from_secs(3);

/// How long the hidden `peek:` browser outlives the last hover. Long
/// enough that a second glance is instant, short enough that the ~60 MB
/// a resident page costs (E3's number) is given back.
const HIDDEN_KEEP: Duration = Duration::from_secs(600);

/// Quick retry — the extractor said the document is not the provider yet
/// (hidden browser still loading, page mid-navigation) or the send found
/// no browser (`NotSent` during the creation gap).
const RETRY_SOON: Duration = Duration::from_millis(700);

/// How long an installed shim gets to answer before the install is
/// re-issued. Generous, because the shim answers on its own clock and a
/// premature reinstall is five messages of noise (though harmless — the
/// page-side guard makes a double install a no-op).
const INSTALL_RETRY: Duration = Duration::from_secs(4);

/// Slow retry after a real refusal (signed out, extractor threw).
const RETRY_FAIL: Duration = Duration::from_secs(30);

/// How many quick retries before the answer becomes "unavailable".
/// 60 × `RETRY_SOON` ≈ 42 s — past any plausible hidden-browser load.
const ATTEMPT_CAP: u32 = 60;

/// Row budget and per-field clamps, in CHARS (the `notify` rule: bytes
/// panic on a boundary, chars never).
const MAX_ROWS: usize = 6;
const PRIMARY_MAX: usize = 80;
const SECONDARY_MAX: usize = 120;
const DETAIL_MAX: usize = 160;
const META_MAX: usize = 24;

/// The services a bookmark can preview. Matched by HOST, not by url:
/// a bookmark deep into a Gmail label is still mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Mail,
    Calendar,
    Linear,
}

impl Provider {
    /// The provider a host belongs to, or `None` for an ordinary bookmark.
    pub fn for_host(host: &str) -> Option<Provider> {
        match host {
            "mail.google.com" => Some(Provider::Mail),
            "calendar.google.com" => Some(Provider::Calendar),
            "linear.app" => Some(Provider::Linear),
            h if h.ends_with(".linear.app") => Some(Provider::Linear),
            _ => None,
        }
    }

    /// The card's header word.
    pub fn title(self) -> &'static str {
        match self {
            Provider::Mail => "Inbox",
            Provider::Calendar => "Today",
            Provider::Linear => "Issues",
        }
    }

    /// The host a source page must be on.
    pub fn source_host(self) -> &'static str {
        match self {
            Provider::Mail => "mail.google.com",
            Provider::Calendar => "calendar.google.com",
            Provider::Linear => "linear.app",
        }
    }

    /// What the hidden browser loads. Mail goes straight to the feed —
    /// (Feature-off there is no browser to point at a url, hence the
    /// `allow`; the deciding sibling `extractor` still compiles and runs
    /// everywhere.)
    /// a page of XML, no Gmail app — because the extractor only needs
    /// the ORIGIN, not the inbox UI. Calendar loads the AGENDA view — a
    /// list that starts TODAY, which is the operator's ruling ("from
    /// today and on, not the whole week"). Linear needs its app.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn hidden_url(self) -> &'static str {
        match self {
            Provider::Mail => "https://mail.google.com/mail/feed/atom",
            Provider::Calendar => "https://calendar.google.com/calendar/u/0/r/agenda",
            Provider::Linear => "https://linear.app/",
        }
    }

    /// The extractor. Resolves to a JSON STRING — the shim forwards it
    /// over the binding verbatim, one string, no object graphs.
    ///
    /// Envelope: `{"v":1,"rows":[{p,s,d,m,ts}]}` on success,
    /// `{"err":"origin","host":…}` while the document is not the
    /// provider yet, `{"err":"signin"}` when the session is absent,
    /// `{"err":"notready"}` while an SPA has not mounted, anything else
    /// being the thrown error's text.
    fn extractor(self) -> &'static str {
        match self {
            Provider::Mail => MAIL_JS,
            Provider::Calendar => CALENDAR_JS,
            Provider::Linear => LINEAR_JS,
        }
    }
}

// Unread mail as rows — sender and content (the walk's second ruling
// reversed its first: "just show emails with from and content"). The
// count still rides along for whoever wants a badge later.
const MAIL_JS: &str = r#"(async () => {
  const read = (x) => {
    const rows = Array.from(x.getElementsByTagName('entry')).slice(0, 6).map((e) => {
      const g = (n) => { const el = e.getElementsByTagName(n)[0]; return el ? el.textContent : ''; };
      let who = '';
      const a = e.getElementsByTagName('author')[0];
      if (a) { const n = a.getElementsByTagName('name')[0]; who = n ? n.textContent : ''; }
      return { p: who, s: g('title'), d: g('summary'), ts: g('modified') || g('issued') };
    });
    let count = null;
    const c = x.getElementsByTagName('fullcount')[0];
    if (c) { const n = parseInt(c.textContent, 10); if (!isNaN(n)) count = n; }
    return { rows, count };
  };
  try {
    if (location.host !== 'mail.google.com') return JSON.stringify({ err: 'origin', host: location.host });
    // The hidden peek browser IS the feed: an XML document that can be
    // read directly, no fetch — which also sidesteps the feed response's
    // own CSP, under which a fetch from this document throws. (That
    // throw surfaced on the first live walk as a card stuck on
    // "Fetching inbox…" with a silent 30 s retry loop behind it.)
    if (location.pathname.indexOf('feed/atom') !== -1) {
      if (document.getElementsByTagName('feed').length) {
        const got = read(document);
        return JSON.stringify({ v: 1, count: got.count, rows: got.rows });
      }
      if (document.readyState !== 'complete') return JSON.stringify({ err: 'notready' });
      return JSON.stringify({ err: 'signin' });
    }
    // An open Gmail tab: same-origin fetch under the app's own CSP.
    const r = await fetch('https://mail.google.com/mail/feed/atom', { credentials: 'include' });
    if (r.status === 401 || r.status === 403) return JSON.stringify({ err: 'signin' });
    if (!r.ok) return JSON.stringify({ err: 'http ' + r.status });
    const x = new DOMParser().parseFromString(await r.text(), 'text/xml');
    if (x.querySelector('parsererror')) return JSON.stringify({ err: 'signin' });
    const got = read(x);
    return JSON.stringify({ v: 1, count: got.count, rows: got.rows });
  } catch (e) { return JSON.stringify({ err: String(e).slice(0, 120) }); }
})()"#;

const CALENDAR_JS: &str = r#"(() => {
  try {
    if (location.host !== 'calendar.google.com') return JSON.stringify({ err: 'origin', host: location.host });
    if (location.pathname.indexOf('/r/agenda') === -1) return JSON.stringify({ err: 'wrongview' });
    // TODAY only (the walk's ruling). The agenda groups days under
    // [data-date] containers; when today's is findable, read inside it,
    // else fall back to the whole list (still ordered from today).
    const d = new Date();
    const key = '' + d.getFullYear() + String(d.getMonth() + 1).padStart(2, '0') + String(d.getDate()).padStart(2, '0');
    const today = document.querySelector('[data-date="' + key + '"]');
    const scope = today || document;
    // "2:30pm" -> minutes since midnight; a bare "2:30" borrows am/pm
    // from its neighbour, the way the labels abbreviate.
    const t2m = (s, ref) => {
      const m = /(\d{1,2})(?::(\d{2}))?\s*(am|pm)?/i.exec(s || '');
      if (!m) return null;
      let h = parseInt(m[1], 10);
      const min = m[2] ? parseInt(m[2], 10) : 0;
      const ap = (m[3] || ref || '').toLowerCase();
      if (h > 23 || min > 59) return null;
      if (ap === 'pm' && h < 12) h += 12;
      if (ap === 'am' && h === 12) h = 0;
      return h * 60 + min;
    };
    const seen = new Set(); const rows = [];
    for (const el of scope.querySelectorAll('[data-eventid], [data-eventchip]')) {
      const id = el.getAttribute('data-eventid') || el.getAttribute('data-eventchip') || '';
      if (!id || seen.has(id)) continue;
      seen.add(id);
      const hidden = el.querySelector('.XuJrye');
      let label = el.getAttribute('aria-label') || (hidden ? hidden.textContent : '') || el.textContent || '';
      label = label.trim().replace(/\s+/g, ' ');
      if (!label) continue;
      const parts = label.split(', ');
      let p = label, m = '';
      if (parts.length > 1 && /\d/.test(parts[0]) && parts[0].length <= 40) { m = parts[0]; p = parts[1]; }
      const row = { p, m };
      const halves = m.split(/–|—|-| to /i);
      if (halves.length >= 2) {
        const endAp = (/(am|pm)/i.exec(halves[1]) || [])[0];
        const he = t2m(halves[1], null);
        const hs = t2m(halves[0], endAp);
        if (hs !== null) row.hs = hs;
        if (he !== null) row.he = he;
      } else {
        const hs = t2m(m, null);
        if (hs !== null) row.hs = hs;
      }
      rows.push(row);
      if (rows.length >= 8) break;
    }
    if (rows.length === 0 && !document.querySelector('[role="main"]')) return JSON.stringify({ err: 'notready' });
    return JSON.stringify({ v: 1, nowm: d.getHours() * 60 + d.getMinutes(), rows });
  } catch (e) { return JSON.stringify({ err: String(e).slice(0, 120) }); }
})()"#;

const LINEAR_JS: &str = r#"(async () => {
  const dbg = [];
  try {
    if (location.host !== 'linear.app' && !location.host.endsWith('.linear.app')) return JSON.stringify({ err: 'origin', host: location.host });
    try {
      const q = '{ viewer { assignedIssues(first: 8, orderBy: updatedAt) { nodes { identifier title url state { name } } } } }';
      const r = await fetch('https://client-api.linear.app/graphql', { method: 'POST', credentials: 'include', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ query: q }) });
      dbg.push('gql:' + r.status);
      if (r.ok) {
        const j = await r.json();
        const ns = j && j.data && j.data.viewer && j.data.viewer.assignedIssues ? j.data.viewer.assignedIssues.nodes : null;
        if (Array.isArray(ns) && ns.length) return JSON.stringify({ v: 1, dbg: dbg.join(' '), rows: ns.map((n) => ({ p: n.title || '', m: (n.identifier || '') + (n.state && n.state.name ? ' - ' + n.state.name : ''), u: n.url || '' })) });
        dbg.push('gql-nodes:' + (Array.isArray(ns) ? ns.length : 'none'));
      }
    } catch (e) { dbg.push('gql-threw:' + String(e).slice(0, 40)); }
    const rows = []; const seen = new Set();
    // Navigation wearing an issue's clothes — the chip scan on a real
    // workspace surfaced view labels, and the walk asked them off ("no
    // org or all issues text").
    const noise = /^(all issues|issues|inbox|my issues|active|backlog|triage|views|projects|cycles)$/i;
    const push = (p, m, u) => {
      p = (p || '').trim().replace(/\s+/g, ' ');
      if (!p || seen.has(p) || noise.test(p)) return;
      seen.add(p);
      rows.push({ p: p.slice(0, 160), m: m || '', u: u || '' });
    };
    // 1. Issue links, on views that render them as anchors.
    for (const el of document.querySelectorAll('a[href*="/issue/"]')) {
      const href = el.getAttribute('href') || '';
      let u = '';
      try { u = new URL(href, location.origin).href; } catch (ignored) {}
      push(el.getAttribute('aria-label') || el.textContent, (href.split('/issue/')[1] || '').split('/')[0], u);
      if (rows.length >= 8) break;
    }
    dbg.push('a:' + rows.length);
    // 2. The ID chip ("ENG-123") anywhere, its row being the nearest
    //    ancestor with more text than the chip itself.
    if (rows.length === 0) {
      const idish = /^[A-Z][A-Z0-9]{1,6}-\d+$/;
      let scanned = 0;
      for (const el of document.querySelectorAll('span, div')) {
        if (++scanned > 4000) break;
        const t = (el.textContent || '').trim();
        if (!idish.test(t)) continue;
        let row = el;
        for (let i = 0; i < 5 && row.parentElement; i++) {
          row = row.parentElement;
          if ((row.textContent || '').trim().length > t.length + 8) break;
        }
        const label = (row.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 160);
        push(label.replace(t, '').trim() || label, t, '');
        if (rows.length >= 8) break;
      }
      dbg.push('chip:' + rows.length);
    }
    if (rows.length === 0 && !document.querySelector('main, #root')) return JSON.stringify({ err: 'notready' });
    return JSON.stringify({ v: 1, dbg: dbg.join(' '), rows });
  } catch (e) { return JSON.stringify({ err: String(e).slice(0, 120) }); }
})()"#;

/// One line of a peek card. Already clamped; render verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Mail: the sender. Calendar and Linear: the title.
    pub primary: String,
    /// Mail: the subject. Empty elsewhere.
    pub secondary: String,
    /// Mail: the snippet. Empty elsewhere.
    pub detail: String,
    /// Mail: the age ("2h"). Calendar: the time range. Linear: id + state.
    pub meta: String,
    /// Where clicking the row goes — the card is an ACTIONABLE flow (the
    /// operator's ruling), so a row that knows its address carries it.
    /// `None` falls back to the bookmark's own url. http(s) only: the
    /// value came from page JS, and an untrusted `javascript:` here would
    /// be a script gadget wearing a link's clothes.
    pub url: Option<String>,
    /// Calendar: when the event starts/ends, minutes since local
    /// midnight — what places it on the hour grid. `None` renders as a
    /// plain row (an all-day event, or a time the parse did not read).
    pub start_min: Option<u32>,
    pub end_min: Option<u32>,
}

/// What the card should say right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No answer yet, and one is being worked on.
    Loading,
    /// `rows` is the answer.
    Ready,
    /// The answer is "nothing there" — an empty inbox is a fact, not a
    /// failure, and must not render as one.
    Empty,
    /// The provider wants a sign-in this profile does not have.
    SignedOut,
    /// Gave up: attempts exhausted or the extractor kept failing.
    Unavailable,
}

/// A render-ready snapshot for one provider. Cheap to build per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub title: &'static str,
    pub source_host: &'static str,
    pub status: Status,
    pub rows: Vec<Row>,
    /// Mail's unread count, riding along for whoever wants a badge.
    pub count: Option<u64>,
    /// Calendar: the local time of the answer, minutes since midnight —
    /// the hour grid's "now". From the PAGE's clock, because this
    /// process has no timezone database and the page does.
    pub now_min: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    SignedOut,
    Unavailable,
}

#[derive(Debug, Default)]
struct Entry {
    rows: Vec<Row>,
    count: Option<u64>,
    now_min: Option<u32>,
    fetched: Option<Instant>,
    fail: Option<Fail>,
    attempts: u32,
    next_try: Option<Instant>,
    interest_until: Option<Instant>,
    /// Set only by ACTIVE interest (a tooltip actually up) — the tile's
    /// per-frame passive request must never be the thing that boots a
    /// hidden Chromium for a bookmark nobody hovered.
    hover_until: Option<Instant>,
    /// While set, `tabs::drive`'s reaper keeps the hidden browser.
    hidden_until: Option<Instant>,
    /// The extractor said the hidden browser is on the wrong VIEW (a
    /// calendar parked on the week grid when the card promises today).
    /// Consumed by `step`, which re-navigates the browser — bounded,
    /// because a page that keeps answering "wrong view" after being
    /// pointed at the right one is a loop, not a plan.
    renav_wanted: bool,
    renavs: u32,
    renav_last: Option<Instant>,
}

thread_local! {
    /// UI-thread only, like everything in `cdp` and for the same reason:
    /// the sends and the answers both live on the browser-process UI
    /// thread, and an off-thread mirror fails as silence.
    static STORE: RefCell<HashMap<Provider, Entry>> = RefCell::new(HashMap::new());

    /// Clicks a peek card took, waiting for a Shell to act on them. A
    /// queue because the card is a tooltip VIEW with no path to the
    /// Shell that owns tabs — the ask-surface move: a click is DATA, and
    /// the render pass drains it where the strip is writable.
    static CLICKS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// A peek card row was clicked: remember `(source_host, url)` for the
/// Shell. Clamped — both strings originate in page JS.
pub fn note_click(source_host: &str, url: &str) {
    if url.len() > 600 || source_host.len() > 100 {
        return;
    }
    CLICKS.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() < 8 {
            c.push((source_host.to_string(), url.to_string()));
        }
    });
}

/// The clicks since the last drain, oldest first.
pub fn take_clicks() -> Vec<(String, String)> {
    CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Mark interest in a host's peek. `active` means a card is actually up
/// (or imminent) — it is what may create the hidden browser. Idempotent
/// and cheap, so render may call it every frame.
pub fn request(host: &str, active: bool, now: Instant) {
    let Some(provider) = Provider::for_host(host) else {
        return;
    };
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(provider).or_default();
        e.interest_until = Some(now + INTEREST_TTL);
        if active {
            e.hover_until = Some(now + INTEREST_TTL);
            // A fresh hover un-sticks an old failure: the situation it
            // reported (signed out, never loaded) may have been fixed
            // since, and the operator asking again is the signal. The
            // hidden browser gets pointed back at its url too — a feed
            // browser parked on the sign-in page it redirected to stays
            // parked forever otherwise, and signing in cannot reach it.
            if e.fail.is_some() && e.next_try.is_none_or(|t| now >= t) {
                e.attempts = 0;
                e.renav_wanted = true;
            }
        }
    });
}

/// The render-ready state of a host's peek, or `None` for a host no
/// provider claims.
pub fn view(host: &str, now: Instant) -> Option<View> {
    let provider = Provider::for_host(host)?;
    let _ = now;
    STORE.with(|s| {
        let s = s.borrow();
        let e = s.get(&provider);
        let (status, rows, count, now_min) = match e {
            Some(e) if e.fetched.is_some() => {
                if e.rows.is_empty() {
                    (Status::Empty, Vec::new(), e.count, e.now_min)
                } else {
                    (Status::Ready, e.rows.clone(), e.count, e.now_min)
                }
            }
            Some(e) => match e.fail {
                Some(Fail::SignedOut) => (Status::SignedOut, Vec::new(), None, None),
                Some(Fail::Unavailable) => (Status::Unavailable, Vec::new(), None, None),
                None => (Status::Loading, Vec::new(), None, None),
            },
            None => (Status::Loading, Vec::new(), None, None),
        };
        Some(View {
            title: provider.title(),
            source_host: provider.source_host(),
            status,
            rows,
            count,
            now_min,
        })
    })
}

/// The binding a shimmed page answers on. Every take of the event queue
/// filters by THIS name — `cdp`'s rule: `take_events_matching` REMOVES
/// what it matches, so an unnamed filter eats other consumers' events.
pub const BINDING: &str = "__eavePeek";

/// The page half: runs the provider's extractor now and every 30 s, and
/// forwards each answer over the binding tagged with the provider host.
///
/// This is `notify`'s transport, adopted after `Runtime.evaluate` was
/// walked into the ground: on a browser the operator never sees, every
/// evaluate answered from an empty document — through four rounds of
/// fixes (attach order, visibility, a load gate, `Runtime.enable`) the
/// `at=""` never moved, while a NEW-DOCUMENT script provably runs in the
/// real page on every load, because `notify` and `audible` have shipped
/// on exactly that. A binding answer carries its own origin story; an
/// evaluate trusts the agent to pick the context, and the agent picked
/// wrong somewhere nobody can see.
fn shim_source(provider: Provider) -> String {
    let key = match provider {
        Provider::Mail => "mail",
        Provider::Calendar => "calendar",
        Provider::Linear => "linear",
    };
    format!(
        r#"(() => {{
  if (globalThis.__eavePeekOn_{key}) return; globalThis.__eavePeekOn_{key} = 1;
  const run = async () => {{
    try {{
      const out = await ({extractor});
      if (typeof __eavePeek === 'function' && typeof out === 'string') {{
        __eavePeek(JSON.stringify({{ src: '{host}', env: out }}));
      }}
    }} catch (e) {{}}
  }};
  // Self-rescue: when this shim finds itself in the VOID context (the
  // empty document `Runtime.evaluate` was measured landing in, `at=""`),
  // it steers the top frame — its own window when it IS the top — to
  // the provider: a location assignment is a navigation, cross-origin
  // ones included, and the fresh document then runs the NEW-DOCUMENT
  // copy of this shim in the real page. It still reports the sighting,
  // so the trace and the renavigation clock keep seeing the truth.
  if (String(location.host) === '') {{
    if (typeof __eavePeek === 'function') {{
      __eavePeek(JSON.stringify({{ src: '{host}', env: '{{"err":"origin","host":""}}' }}));
    }}
    try {{ (top || window).location.href = '{hidden}'; }} catch (e) {{}}
    return;
  }}
  run();
  setInterval(run, 30000);
}})()"#,
        extractor = provider.extractor(),
        host = provider.source_host(),
        hidden = provider.hidden_url(),
    )
}

/// One turn of the peek loop: fold the answers that arrived over the
/// binding, then (re)install shims where interest is live. Returns
/// whether work is pending for a live interest — the caller uses it to
/// keep frames coming so the card repaints when an answer lands.
pub fn drive(snapshot: &TabsSnapshot, now: Instant) -> bool {
    let _ = snapshot;
    let answers = cdp::take_events_matching(|ev| {
        cdp::binding_name(&ev.params).as_deref() == Some(BINDING)
    });
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        for ev in answers {
            fold_answer(&mut s, &ev.params, now);
        }
        let mut busy = false;
        for (provider, e) in s.iter_mut() {
            step(*provider, e, now);
            let interested = e.interest_until.is_some_and(|t| now < t);
            if interested && e.next_try.is_some_and(|t| now < t) {
                busy = true;
            }
        }
        busy
    })
}

/// One `Runtime.bindingCalled` from a shimmed page, into its provider's
/// entry. The payload names its provider (`src`) because one binding
/// serves three shims and the event does not say which page raised it.
fn fold_answer(map: &mut HashMap<Provider, Entry>, params: &str, now: Instant) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(params) else {
        return;
    };
    let Some(payload) = v.get("payload").and_then(|p| p.as_str()) else {
        return;
    };
    let Ok(outer) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    let Some(provider) = outer
        .get("src")
        .and_then(|s| s.as_str())
        .and_then(Provider::for_host)
    else {
        return;
    };
    let Some(env) = outer.get("env").and_then(|e| e.as_str()) else {
        return;
    };
    let e = map.entry(provider).or_default();
    fold_envelope(provider, e, env, now);
}

fn step(provider: Provider, e: &mut Entry, now: Instant) {
    let interested = e.interest_until.is_some_and(|t| now < t);
    if !interested {
        return;
    }
    // Only a HOVER may boot the hidden browser; once it exists, passive
    // interest may keep reading through it. Every provider reads its own
    // browser and never a tab — three walks, three reasons: the Gmail
    // app's CSP refuses the feed fetch, the operator's calendar view
    // answers a different question, and a background tab was never shown
    // so nothing sane can be evaluated in it (the `at=""` finding).
    let hover = e.hover_until.is_some_and(|t| now < t);
    let hidden_live = e.hidden_until.is_some_and(|t| now < t);
    if !hover && !hidden_live {
        return;
    }
    let id = super::peek_id(provider.source_host());
    if hover {
        e.hidden_until = Some(now + HIDDEN_KEEP);
        #[cfg(feature = "web")]
        super::tabs::ensure_peek_browser(&id, provider.hidden_url());
    }
    // The renavigation runs AHEAD of the retry clock, deliberately: a
    // page full of blank iframes reports the void once per document, and
    // on walk four that chatter reset `next_try` fast enough that a
    // renav behind the clock never came due. Rate-limited on its own.
    if e.renav_wanted
        && e.renavs < 6
        && e.renav_last.is_none_or(|t| now.duration_since(t) > Duration::from_secs(2))
    {
        e.renav_wanted = false;
        e.renavs += 1;
        e.renav_last = Some(now);
        // The nonce makes it a HARD navigation beyond argument — a
        // load_url of the address the browser already shows was
        // observed doing nothing at all.
        #[cfg(feature = "web")]
        {
            let sep = if provider.hidden_url().contains('?') {
                '&'
            } else {
                '?'
            };
            let url = format!("{}{}eave_peek={}", provider.hidden_url(), sep, e.renavs);
            super::tabs::peek_navigate(&id, &url);
        }
    }
    let fresh = e
        .fetched
        .is_some_and(|t| now.duration_since(t) < STALE_AFTER);
    if fresh {
        return;
    }
    if e.next_try.is_some_and(|t| now < t) {
        return;
    }
    if e.attempts >= ATTEMPT_CAP {
        e.fail = Some(Fail::Unavailable);
        return;
    }
    // Not before the document is there: install-then-navigate is the
    // covered path (the new-document script runs on the next load), but
    // an install into the creation gap is five messages nothing hears.
    if !super::page_has_loaded(&id) {
        e.next_try = Some(now + RETRY_SOON);
        return;
    }

    // (Re)install the shim — notify's exact five calls, in its exact
    // order, each silent when wrong (its module docs carry the table).
    // The evaluate runs the shim in the CURRENT document; the
    // new-document script covers every later one; re-issuing on the
    // retry clock is the recovery for a browser that was reaped and
    // recreated (the page guard makes a re-install a no-op elsewhere).
    e.attempts += 1;
    e.next_try = Some(now + INSTALL_RETRY);
    let src = shim_source(provider);
    let _ = super::cdp_call(&id, "Runtime.enable", &serde_json::Value::Null);
    let _ = super::cdp_call(&id, "Page.enable", &serde_json::Value::Null);
    let _ = super::cdp_call(
        &id,
        "Runtime.addBinding",
        &serde_json::json!({ "name": BINDING }),
    );
    let _ = super::cdp_call(
        &id,
        "Page.addScriptToEvaluateOnNewDocument",
        &serde_json::json!({ "source": src }),
    );
    let _ = super::cdp_call(
        &id,
        "Runtime.evaluate",
        &serde_json::json!({ "expression": src }),
    );
}

/// `EAVE_WEB_TRACE=1`, the same lamp the rest of `web` reads. The first
/// live walk spent its mail card on "Fetching inbox…" for minutes while
/// the extractor failed every 30 s IN SILENCE — a fold that says nothing
/// is a fold nobody can diagnose.
fn trace() -> bool {
    std::env::var("EAVE_WEB_TRACE").is_ok()
}

/// The extractor's own envelope.
fn fold_envelope(provider: Provider, e: &mut Entry, value: &str, now: Instant) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(value) else {
        e.next_try = Some(now + RETRY_SOON);
        return;
    };
    if let Some(err) = v.get("err").and_then(|e| e.as_str()) {
        if trace() {
            let at = v.get("host").and_then(|h| h.as_str()).unwrap_or("");
            eprintln!("[web] peek {} err={err:?} at={at:?}", provider.source_host());
        }
        match err {
            // The document is not the provider yet. If it settled on a
            // Google sign-in, that IS the answer.
            "origin" => {
                let host = v.get("host").and_then(|h| h.as_str()).unwrap_or("");
                if host.starts_with("accounts.google.com") {
                    e.fail = Some(Fail::SignedOut);
                    e.next_try = Some(now + RETRY_FAIL);
                } else if host.is_empty() {
                    // A VOID sighting — an empty document (the initial
                    // one, or a blank iframe running the new-document
                    // shim). Re-navigate NOW, from the diagnosis itself:
                    // walk four put the renav behind the step's gates
                    // and watched it never come due. Rate-limited and
                    // bounded here; the retry clock is left alone (blank
                    // iframes report this once per document each).
                    if e.renavs < 6
                        && e
                            .renav_last
                            .is_none_or(|t| now.duration_since(t) > Duration::from_secs(2))
                    {
                        e.renavs += 1;
                        e.renav_last = Some(now);
                        #[cfg(feature = "web")]
                        {
                            let base = provider.hidden_url();
                            let sep = if base.contains('?') { '&' } else { '?' };
                            let url = format!("{base}{sep}eave_peek={}", e.renavs);
                            super::tabs::peek_navigate(
                                &super::peek_id(provider.source_host()),
                                &url,
                            );
                        }
                    }
                } else {
                    e.next_try = Some(now + RETRY_SOON);
                }
            }
            "notready" => e.next_try = Some(now + RETRY_SOON),
            "wrongview" => {
                e.renav_wanted = true;
                e.next_try = Some(now + RETRY_SOON);
            }
            "signin" => {
                e.fail = Some(Fail::SignedOut);
                e.next_try = Some(now + RETRY_FAIL);
            }
            _ => {
                e.next_try = Some(now + RETRY_FAIL);
                if e.attempts >= ATTEMPT_CAP {
                    e.fail = Some(Fail::Unavailable);
                }
            }
        }
        return;
    }
    let rows = v
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|rs| {
            rs.iter()
                .take(MAX_ROWS)
                .filter_map(|r| {
                    let get = |k: &str| {
                        r.get(k)
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    let primary = clamp(&get("p"), PRIMARY_MAX);
                    if primary.is_empty() {
                        return None;
                    }
                    let ts = get("ts");
                    let meta = if ts.is_empty() {
                        clamp(&get("m"), META_MAX)
                    } else {
                        age_label(&ts)
                    };
                    let minute = |k: &str| {
                        r.get(k)
                            .and_then(|n| n.as_u64())
                            .and_then(|n| u32::try_from(n).ok())
                            .filter(|n| *n < 24 * 60)
                    };
                    Some(Row {
                        primary,
                        secondary: clamp(&get("s"), SECONDARY_MAX),
                        detail: clamp(&get("d"), DETAIL_MAX),
                        meta,
                        url: safe_url(&get("u")),
                        start_min: minute("hs"),
                        end_min: minute("he"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if trace() {
        let dbg = v.get("dbg").and_then(|d| d.as_str()).unwrap_or("");
        eprintln!(
            "[web] peek {} rows={} count={:?} dbg={dbg:?}",
            provider.source_host(),
            rows.len(),
            v.get("count").and_then(|c| c.as_u64()),
        );
    }
    e.rows = rows;
    e.count = v.get("count").and_then(|c| c.as_u64());
    e.now_min = v
        .get("nowm")
        .and_then(|n| n.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n < 24 * 60);
    e.fetched = Some(now);
    e.fail = None;
    e.attempts = 0;
    e.renavs = 0;
    e.next_try = None;
}

/// A row url the card may follow: http(s) only, sane length. The value
/// came from page JavaScript — a `javascript:` or `data:` here is a
/// script gadget, and refusing it costs a fallback to the bookmark url.
fn safe_url(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.len() > 600 {
        return None;
    }
    (t.starts_with("https://") || t.starts_with("http://")).then(|| t.to_string())
}

/// Whether `tabs::drive`'s reaper should keep a `peek:` browser alive.
/// `false` the moment the keep-window lapses, and the reaper closes it
/// the way it closes a departed tab's. (The caller is gated with the
/// reaper; the rule is not, so the default suite asserts it.)
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub fn browser_wanted(id: &str) -> bool {
    let Some(host) = super::peek_host(id) else {
        return false;
    };
    let Some(provider) = Provider::for_host(host) else {
        return false;
    };
    let now = Instant::now();
    STORE.with(|s| {
        s.borrow()
            .get(&provider)
            .is_some_and(|e| e.hidden_until.is_some_and(|t| now < t))
    })
}

/// Clamp by CHARS. Bytes panic on a boundary; chars never do.
fn clamp(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect()
}

/// "2h" from an RFC 3339 timestamp, against the wall clock. Hand-rolled
/// because eave-app carries no date crate and the feed's format is fixed;
/// anything unparseable is an empty label, never an error.
fn age_label(ts: &str) -> String {
    let Some(then) = rfc3339_epoch(ts) else {
        return String::new();
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dt = now - then;
    if dt < 0 {
        return String::new();
    }
    if dt < 60 {
        "now".to_string()
    } else if dt < 3600 {
        format!("{}m", dt / 60)
    } else if dt < 86_400 {
        format!("{}h", dt / 3600)
    } else {
        format!("{}d", dt / 86_400)
    }
}

/// Seconds since the epoch for `2026-08-27T09:15:22Z` and friends
/// (fractional seconds and `±hh:mm` offsets included). `None` for
/// anything that does not look like that.
fn rfc3339_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' && b[10] != b't' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Skip fractional seconds, then read the offset.
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    let offset = match b.get(i) {
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let oh: i64 = s.get(i + 1..i + 3)?.parse().ok()?;
            let om: i64 = s.get(i + 4..i + 6)?.parse().ok()?;
            let mag = oh * 3600 + om * 60;
            if *sign == b'+' {
                mag
            } else {
                -mag
            }
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date — Howard
/// Hinnant's `days_from_civil`, transcribed.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
pub fn reset_for_test() {
    STORE.with(|s| s.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_tab(id: &str, url: &str) -> eave_wire::TabInfo {
        eave_wire::TabInfo {
            id: id.to_string(),
            group: None,
            pinned: false,
            content: eave_wire::TabContent::Page,
            url: Some(url.to_string()),
            title: None,
            favicon_url: None,
            load: Default::default(),
            pending: None,
            can_go_back: false,
            can_go_forward: false,
            history_len: 0,
            opener: None,
            incognito: false,
            window: None,
            created_ts_ms: 0,
        }
    }

    fn snapshot(tabs: Vec<eave_wire::TabInfo>) -> TabsSnapshot {
        TabsSnapshot {
            tabs,
            ..Default::default()
        }
    }

    /// Raise the event a shimmed page would: the binding call carrying
    /// the provider-tagged envelope. This is the whole transport, so the
    /// tests drive the real lifecycle end to end.
    fn answer(src: &str, env: &str) {
        let payload = serde_json::json!({ "src": src, "env": env }).to_string();
        let params = serde_json::json!({ "name": BINDING, "payload": payload }).to_string();
        cdp::note_event(&format!("peek:{src}"), "Runtime.bindingCalled", &params);
    }

    #[test]
    fn hosts_map_to_their_providers_and_nothing_else() {
        assert_eq!(Provider::for_host("mail.google.com"), Some(Provider::Mail));
        assert_eq!(
            Provider::for_host("calendar.google.com"),
            Some(Provider::Calendar)
        );
        assert_eq!(Provider::for_host("linear.app"), Some(Provider::Linear));
        assert_eq!(
            Provider::for_host("acme.linear.app"),
            Some(Provider::Linear)
        );
        assert_eq!(Provider::for_host("github.com"), None);
        // A host that merely CONTAINS a provider's name is not it.
        assert_eq!(Provider::for_host("mail.google.com.evil.test"), None);
        assert_eq!(Provider::for_host("notlinear.app"), None);
    }

    #[test]
    fn every_provider_reads_through_its_peek_browser_never_a_tab() {
        // Three walks, three reasons, one rule: the Gmail app's CSP
        // refuses the feed fetch, the operator's calendar view answers
        // a different question, and a background tab was never shown so
        // its agent cannot be evaluated in (the `at=""` finding).
        reset_for_test();
        let now = Instant::now();
        request("linear.app", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://linear.app/acme/inbox")]);
        drive(&snap, now);
        let calls = crate::web::take_cdp_calls_for_test();
        let install = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.addBinding")
            .expect("a shim install was issued");
        assert_eq!(
            install.0, "peek:linear.app",
            "the open tab must be left out of it"
        );
    }

    #[test]
    fn mail_reads_through_the_hidden_feed_even_with_a_gmail_tab_open() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/mail/u/0/")]);
        drive(&snap, now);
        let calls = crate::web::take_cdp_calls_for_test();
        let install = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.addBinding")
            .expect("a shim install was issued");
        assert_eq!(
            install.0, "peek:mail.google.com",
            "the Gmail app tab's CSP refuses the feed fetch; the hidden feed document is the source"
        );
    }

    #[test]
    fn no_source_tab_falls_back_to_the_peek_browser_only_on_hover() {
        reset_for_test();
        let now = Instant::now();
        // Passive interest (the tile rendering) must NOT boot a browser.
        request("mail.google.com", false, now);
        drive(&snapshot(vec![]), now);
        assert!(
            crate::web::take_cdp_calls_for_test()
                .iter()
                .all(|(_, m, _)| m != "Runtime.addBinding"),
            "passive interest installed a shim with no source page"
        );
        // A hover may.
        request("mail.google.com", true, now);
        drive(&snapshot(vec![]), now);
        let calls = crate::web::take_cdp_calls_for_test();
        let install = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.addBinding")
            .expect("hover installed the shim");
        assert_eq!(install.0, "peek:mail.google.com");
        assert!(browser_wanted("peek:mail.google.com"));
        assert!(!browser_wanted("peek:github.com"));
    }

    #[test]
    fn an_answer_folds_into_ready_rows_and_clamps() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/")]);
        drive(&snap, now);
        let long = "x".repeat(500);
        answer(
            "mail.google.com",
            &serde_json::json!({ "v": 1, "rows": [
                { "p": "Ada", "s": "Hello", "d": long, "ts": "" },
                { "p": "", "s": "dropped — no primary", "d": "", "ts": "" },
            ] })
            .to_string(),
        );
        drive(&snap, now);
        let v = view("mail.google.com", now).expect("a provider host has a view");
        assert_eq!(v.status, Status::Ready);
        assert_eq!(v.rows.len(), 1, "a row with no primary is dropped");
        assert_eq!(v.rows[0].primary, "Ada");
        assert_eq!(v.rows[0].detail.chars().count(), DETAIL_MAX);
    }

    #[test]
    fn empty_rows_read_as_empty_not_as_failure() {
        reset_for_test();
        let now = Instant::now();
        request("calendar.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://calendar.google.com/")]);
        drive(&snap, now);
        answer("calendar.google.com", r#"{ "v": 1, "rows": [] }"#);
        drive(&snap, now);
        assert_eq!(
            view("calendar.google.com", now).unwrap().status,
            Status::Empty
        );
    }

    #[test]
    fn a_signin_answer_reads_as_signed_out() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/")]);
        drive(&snap, now);
        answer("mail.google.com", r#"{ "err": "signin" }"#);
        drive(&snap, now);
        assert_eq!(
            view("mail.google.com", now).unwrap().status,
            Status::SignedOut
        );
    }

    #[test]
    fn an_origin_answer_that_settled_on_google_signin_is_signed_out() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        drive(&snapshot(vec![]), now);
        answer(
            "mail.google.com",
            r#"{ "err": "origin", "host": "accounts.google.com" }"#,
        );
        drive(&snapshot(vec![]), now);
        assert_eq!(
            view("mail.google.com", now).unwrap().status,
            Status::SignedOut
        );
    }

    #[test]
    fn interest_lapsing_stops_the_busy_signal() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/")]);
        assert!(drive(&snap, now), "pending work with live interest is busy");
        let later = now + INTEREST_TTL + Duration::from_secs(1);
        assert!(
            !drive(&snap, later),
            "lapsed interest must not keep pumping frames"
        );
    }

    #[test]
    fn rfc3339_arithmetic_is_right() {
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_epoch("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(rfc3339_epoch("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        // Fractional seconds and offsets.
        assert_eq!(rfc3339_epoch("1970-01-01T00:00:00.500Z"), Some(0));
        assert_eq!(rfc3339_epoch("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(rfc3339_epoch("1969-12-31T23:00:00-01:00"), Some(0));
        assert_eq!(rfc3339_epoch("not a date"), None);
        assert_eq!(rfc3339_epoch("1970-13-01T00:00:00Z"), None);
    }

    #[test]
    fn mail_is_its_rows_and_the_count_rides_along() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/")]);
        drive(&snap, now);
        answer(
            "mail.google.com",
            r#"{ "v": 1, "count": 128, "rows": [{ "p": "Ada", "s": "Hi" }] }"#,
        );
        drive(&snap, now);
        let v = view("mail.google.com", now).unwrap();
        assert_eq!(v.status, Status::Ready);
        assert_eq!(v.count, Some(128));
        // No rows is "nothing new", whatever the count claims — the
        // card shows MAIL, and there is none to show.
        reset_for_test();
        request("mail.google.com", true, now);
        drive(&snap, now);
        answer("mail.google.com", r#"{ "v": 1, "count": 0, "rows": [] }"#);
        drive(&snap, now);
        assert_eq!(view("mail.google.com", now).unwrap().status, Status::Empty);
    }

    #[test]
    fn calendar_reads_through_the_hidden_browser_even_with_a_tab_open() {
        reset_for_test();
        let now = Instant::now();
        request("calendar.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://calendar.google.com/")]);
        drive(&snap, now);
        let calls = crate::web::take_cdp_calls_for_test();
        let install = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.addBinding")
            .expect("issued");
        assert_eq!(
            install.0, "peek:calendar.google.com",
            "the operator's own calendar view answers a different question"
        );
    }

    #[test]
    fn a_row_url_from_the_page_is_https_or_nothing() {
        reset_for_test();
        let now = Instant::now();
        request("linear.app", true, now);
        drive(&snapshot(vec![]), now);
        answer(
            "linear.app",
            r#"{ "v": 1, "rows": [
                { "p": "real", "u": "https://linear.app/x/issue/E-1" },
                { "p": "gadget", "u": "javascript:alert(1)" }
            ] }"#,
        );
        drive(&snapshot(vec![]), now);
        let v = view("linear.app", now).unwrap();
        assert_eq!(
            v.rows[0].url.as_deref(),
            Some("https://linear.app/x/issue/E-1")
        );
        assert_eq!(v.rows[1].url, None, "a javascript: url is refused");
    }

    #[test]
    fn card_clicks_queue_and_drain_once() {
        note_click("mail.google.com", "https://mail.google.com/mail/u/0/#inbox");
        let clicks = take_clicks();
        assert_eq!(
            clicks,
            vec![(
                "mail.google.com".to_string(),
                "https://mail.google.com/mail/u/0/#inbox".to_string()
            )]
        );
        assert!(take_clicks().is_empty(), "taking forgets");
    }

    #[test]
    fn clamps_count_chars_not_bytes() {
        // A multi-byte boundary must not panic and must count as chars.
        let s = "é".repeat(10);
        assert_eq!(clamp(&s, 4).chars().count(), 4);
    }
}
