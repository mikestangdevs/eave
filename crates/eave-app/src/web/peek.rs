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

use eave_wire::{TabContent, TabsSnapshot};

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

    fn matches_host(self, host: &str) -> bool {
        match self {
            Provider::Linear => host == "linear.app" || host.ends_with(".linear.app"),
            _ => host == self.source_host(),
        }
    }

    /// What the hidden browser loads. Mail goes straight to the feed —
    /// (Feature-off there is no browser to point at a url, hence the
    /// `allow`; the deciding sibling `extractor` still compiles and runs
    /// everywhere.)
    /// a page of XML, no Gmail app — because the extractor only needs
    /// the ORIGIN, not the inbox UI. Calendar and Linear need their apps.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn hidden_url(self) -> &'static str {
        match self {
            Provider::Mail => "https://mail.google.com/mail/feed/atom",
            Provider::Calendar => "https://calendar.google.com/",
            Provider::Linear => "https://linear.app/",
        }
    }

    /// The extractor. Resolves to a JSON STRING (not an object) so the
    /// answer crosses CDP as one `result.value` — `returnByValue` hands
    /// back a string verbatim where an object graph invites surprises.
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

const MAIL_JS: &str = r#"(async () => {
  try {
    if (location.host !== 'mail.google.com') return JSON.stringify({ err: 'origin', host: location.host });
    const r = await fetch('https://mail.google.com/mail/feed/atom', { credentials: 'include' });
    if (r.status === 401 || r.status === 403) return JSON.stringify({ err: 'signin' });
    if (!r.ok) return JSON.stringify({ err: 'http ' + r.status });
    const x = new DOMParser().parseFromString(await r.text(), 'text/xml');
    if (x.querySelector('parsererror')) return JSON.stringify({ err: 'signin' });
    const rows = Array.from(x.getElementsByTagName('entry')).slice(0, 6).map((e) => {
      const g = (n) => { const el = e.getElementsByTagName(n)[0]; return el ? el.textContent : ''; };
      let who = '';
      const a = e.getElementsByTagName('author')[0];
      if (a) { const n = a.getElementsByTagName('name')[0]; who = n ? n.textContent : ''; }
      return { p: who, s: g('title'), d: g('summary'), ts: g('modified') || g('issued') };
    });
    return JSON.stringify({ v: 1, rows });
  } catch (e) { return JSON.stringify({ err: String(e).slice(0, 120) }); }
})()"#;

const CALENDAR_JS: &str = r#"(() => {
  try {
    if (location.host !== 'calendar.google.com') return JSON.stringify({ err: 'origin', host: location.host });
    const seen = new Set(); const rows = [];
    for (const el of document.querySelectorAll('[data-eventid]')) {
      const id = el.getAttribute('data-eventid') || '';
      if (!id || seen.has(id)) continue;
      seen.add(id);
      const hidden = el.querySelector('.XuJrye');
      let label = el.getAttribute('aria-label') || (hidden ? hidden.textContent : '') || el.textContent || '';
      label = label.trim().replace(/\s+/g, ' ');
      if (!label) continue;
      const parts = label.split(', ');
      let p = label, m = '';
      if (parts.length > 1 && /\d/.test(parts[0]) && parts[0].length <= 40) { m = parts[0]; p = parts[1]; }
      rows.push({ p, m });
      if (rows.length >= 8) break;
    }
    if (rows.length === 0 && !document.querySelector('[role="main"]')) return JSON.stringify({ err: 'notready' });
    return JSON.stringify({ v: 1, rows });
  } catch (e) { return JSON.stringify({ err: String(e).slice(0, 120) }); }
})()"#;

const LINEAR_JS: &str = r#"(async () => {
  try {
    if (location.host !== 'linear.app' && !location.host.endsWith('.linear.app')) return JSON.stringify({ err: 'origin', host: location.host });
    try {
      const q = '{ viewer { assignedIssues(first: 8, orderBy: updatedAt) { nodes { identifier title state { name } } } } }';
      const r = await fetch('https://client-api.linear.app/graphql', { method: 'POST', credentials: 'include', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ query: q }) });
      if (r.ok) {
        const j = await r.json();
        const ns = j && j.data && j.data.viewer && j.data.viewer.assignedIssues ? j.data.viewer.assignedIssues.nodes : null;
        if (Array.isArray(ns)) return JSON.stringify({ v: 1, rows: ns.map((n) => ({ p: n.title || '', m: (n.identifier || '') + (n.state && n.state.name ? ' - ' + n.state.name : '') })) });
      }
    } catch (ignored) {}
    const rows = [];
    for (const el of document.querySelectorAll('a[href*="/issue/"]')) {
      const t = (el.getAttribute('aria-label') || el.textContent || '').trim().replace(/\s+/g, ' ');
      const href = el.getAttribute('href') || '';
      const m = (href.split('/issue/')[1] || '').split('/')[0];
      if (t) rows.push({ p: t.slice(0, 160), m });
      if (rows.length >= 8) break;
    }
    if (rows.length === 0 && !document.querySelector('main, #root')) return JSON.stringify({ err: 'notready' });
    return JSON.stringify({ v: 1, rows });
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    SignedOut,
    Unavailable,
}

#[derive(Debug, Default)]
struct Entry {
    rows: Vec<Row>,
    fetched: Option<Instant>,
    fail: Option<Fail>,
    inflight: Option<u32>,
    attempts: u32,
    next_try: Option<Instant>,
    interest_until: Option<Instant>,
    /// Set only by ACTIVE interest (a tooltip actually up) — the tile's
    /// per-frame passive request must never be the thing that boots a
    /// hidden Chromium for a bookmark nobody hovered.
    hover_until: Option<Instant>,
    /// While set, `tabs::drive`'s reaper keeps the hidden browser.
    hidden_until: Option<Instant>,
}

thread_local! {
    /// UI-thread only, like everything in `cdp` and for the same reason:
    /// the sends and the answers both live on the browser-process UI
    /// thread, and an off-thread mirror fails as silence.
    static STORE: RefCell<HashMap<Provider, Entry>> = RefCell::new(HashMap::new());
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
            // since, and the operator asking again is the signal.
            if e.fail.is_some() && e.next_try.is_none_or(|t| now >= t) {
                e.attempts = 0;
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
        let (status, rows) = match e {
            Some(e) if e.fetched.is_some() => {
                if e.rows.is_empty() {
                    (Status::Empty, Vec::new())
                } else {
                    (Status::Ready, e.rows.clone())
                }
            }
            Some(e) => match e.fail {
                Some(Fail::SignedOut) => (Status::SignedOut, Vec::new()),
                Some(Fail::Unavailable) => (Status::Unavailable, Vec::new()),
                None => (Status::Loading, Vec::new()),
            },
            None => (Status::Loading, Vec::new()),
        };
        Some(View {
            title: provider.title(),
            source_host: provider.source_host(),
            status,
            rows,
        })
    })
}

/// One turn of the peek loop: collect answers, issue the next fetch.
/// Returns whether work is still in flight for a live interest — the
/// caller uses it to keep frames coming so the card repaints when the
/// answer lands (a tooltip renders only when its window does).
pub fn drive(snapshot: &TabsSnapshot, now: Instant) -> bool {
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let mut busy = false;
        for (provider, e) in s.iter_mut() {
            step(*provider, e, snapshot, now);
            let interested = e.interest_until.is_some_and(|t| now < t);
            if interested && (e.inflight.is_some() || e.next_try.is_some_and(|t| now < t)) {
                busy = true;
            }
        }
        busy
    })
}

fn step(provider: Provider, e: &mut Entry, snapshot: &TabsSnapshot, now: Instant) {
    // Collect first, so an answer that has landed is folded before any
    // decision about issuing another.
    if let Some(id) = e.inflight {
        if let Some(outcome) = cdp::take(id) {
            e.inflight = None;
            fold(provider, e, outcome, now);
        } else if !cdp::is_pending(id) {
            // Neither pending nor answered: the id was resolved and taken
            // by nobody, or forgotten. Let the retry clock decide.
            e.inflight = None;
            e.next_try = Some(now + RETRY_SOON);
        }
    }

    let interested = e.interest_until.is_some_and(|t| now < t);
    if !interested || e.inflight.is_some() {
        return;
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

    // A source page: an open, non-incognito page tab on the provider's
    // host. Incognito is excluded on purpose — its jar is not the
    // profile the bookmark peek claims to speak for.
    let source = snapshot
        .tabs
        .iter()
        .filter(|t| t.content == TabContent::Page && !t.incognito)
        .find(|t| {
            t.url
                .as_deref()
                .is_some_and(|u| provider.matches_host(&crate::omnibox::engine_host(u)))
        })
        .map(|t| t.id.clone());

    let target = match source {
        Some(id) => id,
        None => {
            // No open tab. Only a HOVER may boot the hidden browser.
            if !e.hover_until.is_some_and(|t| now < t) {
                return;
            }
            let id = super::peek_id(provider.source_host());
            e.hidden_until = Some(now + HIDDEN_KEEP);
            #[cfg(feature = "web")]
            super::tabs::ensure_peek_browser(&id, provider.hidden_url());
            id
        }
    };

    e.attempts += 1;
    e.next_try = Some(now + RETRY_SOON);
    e.inflight = Some(super::cdp_evaluate(&target, provider.extractor()));
}

/// Fold one CDP answer into the entry.
fn fold(provider: Provider, e: &mut Entry, outcome: cdp::Outcome, now: Instant) {
    let _ = provider;
    match outcome {
        cdp::Outcome::Ok(result) => match evaluate_value(&result) {
            Some(value) => fold_envelope(e, &value, now),
            // An exception or a non-string: a page mid-navigation does
            // this. Retry on the quick clock.
            None => e.next_try = Some(now + RETRY_SOON),
        },
        // `-32000 cannot find context` is a navigation race; every other
        // refusal is equally transient from here. Quick clock, capped by
        // ATTEMPT_CAP like the rest.
        cdp::Outcome::Err { .. } => e.next_try = Some(now + RETRY_SOON),
        // The browser is not there yet (hidden one still creating) or is
        // not coming (feature off). The cap turns the latter into
        // Unavailable rather than a spin.
        cdp::Outcome::NotSent => e.next_try = Some(now + RETRY_SOON),
        cdp::Outcome::Gone => e.next_try = Some(now + RETRY_SOON),
    }
}

/// `Runtime.evaluate`'s `result.value`, when it is the string the
/// extractor promised. `None` covers exceptions, handles and undefined.
fn evaluate_value(result: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(result).ok()?;
    if v.get("exceptionDetails").is_some() {
        return None;
    }
    v.get("result")?
        .get("value")?
        .as_str()
        .map(|s| s.to_string())
}

/// The extractor's own envelope.
fn fold_envelope(e: &mut Entry, value: &str, now: Instant) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(value) else {
        e.next_try = Some(now + RETRY_SOON);
        return;
    };
    if let Some(err) = v.get("err").and_then(|e| e.as_str()) {
        match err {
            // The document is not the provider yet. If it settled on a
            // Google sign-in, that IS the answer.
            "origin" => {
                let host = v.get("host").and_then(|h| h.as_str()).unwrap_or("");
                if host.starts_with("accounts.google.com") {
                    e.fail = Some(Fail::SignedOut);
                    e.next_try = Some(now + RETRY_FAIL);
                } else {
                    e.next_try = Some(now + RETRY_SOON);
                }
            }
            "notready" => e.next_try = Some(now + RETRY_SOON),
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
                    Some(Row {
                        primary,
                        secondary: clamp(&get("s"), SECONDARY_MAX),
                        detail: clamp(&get("d"), DETAIL_MAX),
                        meta,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    e.rows = rows;
    e.fetched = Some(now);
    e.fail = None;
    e.attempts = 0;
    e.next_try = None;
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

/// Test seam: the outstanding CDP id for a provider, so a test can
/// answer it with `cdp::complete` the way Chromium would.
#[cfg(test)]
pub fn inflight_for_test(provider: Provider) -> Option<u32> {
    STORE.with(|s| s.borrow().get(&provider).and_then(|e| e.inflight))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_tab(id: &str, url: &str) -> eave_wire::TabInfo {
        eave_wire::TabInfo {
            id: id.to_string(),
            group: None,
            pinned: false,
            content: TabContent::Page,
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

    fn ok_envelope(env: &str) -> cdp::Outcome {
        cdp::Outcome::Ok(
            serde_json::json!({ "result": { "type": "string", "value": env } }).to_string(),
        )
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
    fn an_open_source_tab_is_evaluated_in_not_the_hidden_browser() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/mail/u/0/")]);
        drive(&snap, now);
        let calls = crate::web::take_cdp_calls_for_test();
        let eval = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.evaluate")
            .expect("an evaluate was issued");
        assert_eq!(eval.0, "t1", "the open Gmail tab is the source");
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
                .all(|(_, m, _)| m != "Runtime.evaluate"),
            "passive interest issued a fetch with no source page"
        );
        // A hover may.
        request("mail.google.com", true, now);
        drive(&snapshot(vec![]), now);
        let calls = crate::web::take_cdp_calls_for_test();
        let eval = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.evaluate")
            .expect("hover issued the fetch");
        assert_eq!(eval.0, "peek:mail.google.com");
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
        let id = inflight_for_test(Provider::Mail).expect("issued");
        let long = "x".repeat(500);
        cdp::complete(
            id,
            ok_envelope(
                &serde_json::json!({ "v": 1, "rows": [
                    { "p": "Ada", "s": "Hello", "d": long, "ts": "" },
                    { "p": "", "s": "dropped — no primary", "d": "", "ts": "" },
                ] })
                .to_string(),
            ),
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
        let id = inflight_for_test(Provider::Calendar).expect("issued");
        cdp::complete(id, ok_envelope(r#"{ "v": 1, "rows": [] }"#));
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
        let id = inflight_for_test(Provider::Mail).expect("issued");
        cdp::complete(id, ok_envelope(r#"{ "err": "signin" }"#));
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
        let id = inflight_for_test(Provider::Mail).expect("issued against the peek browser");
        cdp::complete(
            id,
            ok_envelope(r#"{ "err": "origin", "host": "accounts.google.com" }"#),
        );
        drive(&snapshot(vec![]), now);
        assert_eq!(
            view("mail.google.com", now).unwrap().status,
            Status::SignedOut
        );
    }

    #[test]
    fn an_incognito_source_is_never_used() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let mut tab = page_tab("t1", "https://mail.google.com/");
        tab.incognito = true;
        drive(&snapshot(vec![tab]), now);
        let calls = crate::web::take_cdp_calls_for_test();
        let eval = calls
            .iter()
            .find(|(_, m, _)| m == "Runtime.evaluate")
            .expect("still fetches — through the peek browser");
        assert_eq!(
            eval.0, "peek:mail.google.com",
            "the incognito tab's jar is not the profile's"
        );
    }

    #[test]
    fn interest_lapsing_stops_the_busy_signal() {
        reset_for_test();
        let now = Instant::now();
        request("mail.google.com", true, now);
        let snap = snapshot(vec![page_tab("t1", "https://mail.google.com/")]);
        assert!(drive(&snap, now), "inflight with live interest is busy");
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
    fn clamps_count_chars_not_bytes() {
        // A multi-byte boundary must not panic and must count as chars.
        let s = "é".repeat(10);
        assert_eq!(clamp(&s, 4).chars().count(), 4);
    }
}
