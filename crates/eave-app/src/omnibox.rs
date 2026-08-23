//! Slice C5 — the omnibox's **decision** half: what one line of typed text
//! means, and what Return does with it.
//!
//! Pure. No gpui, no globals, no network, no clock. The rule this file
//! follows is `overlay`'s and `slash`'s: the half with the interesting
//! behaviour is the half that has to be testable without a window, because
//! a surface that can only be exercised by opening a window is a surface
//! whose rules ship on argument alone.
//!
//! # The one question an address bar has to answer
//!
//! A person types one line and means one of two things: *go here* or *find
//! this*. Getting it wrong is not a cosmetic failure in either direction —
//! searching for `localhost:3000` sends a private hostname to a third party,
//! and navigating to `rust trait objects` is a DNS lookup for a sentence.
//!
//! So [`resolve`] is written as a **ladder of stated facts**, cheapest and
//! most certain first, and every rung says what it decides and why. There is
//! no fuzzy scoring and no "did you mean": the rules are legible, and when
//! one of them is wrong it is wrong the same way every time, which is the
//! only kind of wrong a person can learn to work around.
//!
//! # Two refusals, which are the interesting part
//!
//! `javascript:` and `data:` are valid schemes that this deliberately treats
//! as **search text**, exactly as Chrome and Safari do. Pasting a
//! `javascript:` url into an address bar and pressing Return is a real
//! attack — it runs in the origin of whatever page is loaded, so "paste this
//! into the address bar to fix your account" is a self-XSS delivered by
//! social engineering rather than by a bug. `data:` is the same shape with
//! a different payload: a document that is not from anywhere, wearing the
//! address bar's authority.
//!
//! Neither is refused with an error. They fall through to search, which is
//! the honest outcome — the text was not an address anyone should navigate
//! to, so it is text.

use eave_wire::{HistoryMatch, MatchKind};

/// Where a search goes when nobody has said otherwise.
///
/// **Google, and this is the stated fact the slice was asked for.** The
/// choice is not neutral and pretending otherwise would be worse than
/// picking: every character typed here that is not an address is sent to
/// this host. Google is where a person typing a sentence into an address
/// bar expects it to go, and a default that is quietly worse at the one job
/// the row has is a default that gets worked around rather than trusted.
///
/// The cost is real and this does not hide it. The host is **named in the
/// interface**, not only here: the search row reads `Search google.com for
/// "…"`, so it is on screen at the moment it is about to receive what you
/// typed. That is the difference between a default and a hidden default,
/// and it is why the sentence is derived from the url rather than written
/// down beside it.
///
/// The prefix carries `www.`; [`engine_host`] strips it, so the row reads
/// `google.com` while the request goes to the host Google actually serves.
///
/// Overridable with `EAVE_SEARCH_URL`, which is a **prefix**: the query is
/// percent-encoded and appended. `https://duckduckgo.com/?q=` and
/// `https://html.duckduckgo.com/html/?q=` both work — the host that asks
/// for the least back is one env var away. The row renames itself from the
/// override's host, so an operator who changes this sees the change rather
/// than a stale sentence.
pub const DEFAULT_SEARCH: &str = "https://www.google.com/search?q=";

/// The search prefix in force. Read per call rather than cached: this is not
/// hot (it runs once per keystroke at typing speed, against a `Vec` lookup),
/// and a cached environment read is the kind of thing that makes a test
/// depend on which test ran first.
pub fn search_prefix() -> String {
    std::env::var("EAVE_SEARCH_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SEARCH.to_string())
}

/// The host a search prefix points at, for the sentence in the interface.
///
/// The **host**, not a brand name. A lookup table of pretty names would be
/// wrong the moment somebody sets `EAVE_SEARCH_URL` to something not in it,
/// and it would be wrong silently — the row would keep saying "Google"
/// while sending the query somewhere else. The host is derived from the
/// thing that actually receives the text, so it cannot drift from it.
pub fn engine_host(prefix: &str) -> String {
    let after_scheme = prefix
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(prefix);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// What Return does with the line as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Navigate. `url` is what goes on the wire; `typed_scheme` says whether
    /// the operator wrote the scheme or this inferred it, so the interface
    /// can show the address it is actually going to rather than the fragment
    /// that was typed.
    Address { url: String, typed_scheme: bool },
    /// Search. `url` is where it goes; `query` is what was typed; `host` is
    /// who receives it, and is on screen.
    Search {
        url: String,
        query: String,
        host: String,
    },
}

impl Target {
    /// The address this navigates to, either way. The omnibox has exactly
    /// one exit and it is `tab_navigate` — a search is a navigation to a
    /// search engine, not a second kind of act.
    pub fn url(&self) -> &str {
        match self {
            Target::Address { url, .. } => url,
            Target::Search { url, .. } => url,
        }
    }
}

/// Schemes that mean "this is an address, go there".
///
/// `://` is required for the authority-bearing ones so that `localhost:3000`
/// is not read as the scheme `localhost` — which is the classic version of
/// this bug, and the reason a bare `^[a-z][a-z0-9+.-]*:` test is wrong here.
const ADDRESS_SCHEMES: &[&str] = &[
    "http://",
    "https://",
    "file://",
    "ftp://",
    "ws://",
    "wss://",
    "chrome://",
    "devtools://",
];

/// Schemes with no authority that are still addresses.
const OPAQUE_ADDRESS_SCHEMES: &[&str] = &["about:", "view-source:"];

/// Schemes that are addresses to a browser and **text** to this omnibox. See
/// the module docs: this is the self-XSS paste, and Chrome and Safari refuse
/// it the same way.
const REFUSED_SCHEMES: &[&str] = &["javascript:", "data:", "vbscript:"];

/// Decide what one typed line means. `None` when there is nothing to decide.
pub fn resolve(input: &str, prefix: &str) -> Option<Target> {
    let typed = input.trim();
    if typed.is_empty() {
        return None;
    }
    let lower = typed.to_ascii_lowercase();

    // 1. A refused scheme is text. Checked BEFORE the address schemes so
    //    that no ordering accident can make it navigable.
    if REFUSED_SCHEMES.iter().any(|s| lower.starts_with(s)) {
        return Some(search(typed, prefix));
    }

    // 2. A scheme the operator wrote is a decision they already made.
    if ADDRESS_SCHEMES.iter().any(|s| lower.starts_with(s))
        || OPAQUE_ADDRESS_SCHEMES.iter().any(|s| lower.starts_with(s))
    {
        return Some(Target::Address {
            url: typed.to_string(),
            typed_scheme: true,
        });
    }

    // 3. Whitespace inside means a sentence. A url with a real space in it
    //    cannot be typed unencoded anyway, so there is nothing to lose here
    //    and a great deal to gain: `rust trait objects` must never be a DNS
    //    lookup.
    if typed.split_whitespace().count() > 1 {
        return Some(search(typed, prefix));
    }

    // 4. Something host-shaped. This is the only rung that guesses, and it
    //    guesses conservatively — see `host_shape`.
    match host_shape(typed) {
        Some(HostShape::Local) => Some(Target::Address {
            // **http, not https, and only here.** A dev server on localhost
            // or a machine-local IP almost never has a certificate, and
            // defaulting those to https turns `localhost:3000` into a
            // connection error with no explanation. Every other inferred
            // scheme is https: guessing http for the public internet in
            // 2026 would be downgrading somebody's connection on their
            // behalf.
            url: format!("http://{typed}"),
            typed_scheme: false,
        }),
        Some(HostShape::Public) => Some(Target::Address {
            url: format!("https://{typed}"),
            typed_scheme: false,
        }),
        None => Some(search(typed, prefix)),
    }
}

fn search(query: &str, prefix: &str) -> Target {
    Target::Search {
        url: format!("{prefix}{}", percent_encode_query(query)),
        query: query.to_string(),
        host: engine_host(prefix),
    }
}

/// What kind of host a string looks like, when it looks like one at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostShape {
    /// `localhost`, a loopback literal, or a private-range address. Reached
    /// over http.
    Local,
    /// A public name. Reached over https.
    Public,
}

/// Decide whether a schemeless string is a host, and which kind.
///
/// **Deliberately strict.** Every false positive here is a search silently
/// turned into a failed navigation, and the person cannot tell why — a
/// browser that answers `foo.bar` with a DNS error when they meant to search
/// for it is worse than one that searches for `example.com` once. So the
/// last label has to be alphabetic and at least two characters, which is
/// what every real TLD is and what almost no sentence fragment is.
fn host_shape(s: &str) -> Option<HostShape> {
    // Split the authority off the path/query/fragment. `example.com/a?b#c`
    // is a host with a path; `?b` alone is not a host at all.
    let authority = s.split(['/', '?', '#']).next().unwrap_or(s);
    if authority.is_empty() {
        return None;
    }
    // Credentials are not something to guess a scheme from. `user@host` in
    // an omnibox is far more often an email address somebody is looking up
    // than a url they mean to visit, and an email address would otherwise
    // sail through every test below.
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    // A colon with a non-numeric tail is not a port, so this was never an
    // authority.
    if let Some(p) = port {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    if host.is_empty() {
        return None;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" {
        return Some(HostShape::Local);
    }
    if let Some(shape) = ipv4_shape(&lower) {
        return Some(shape);
    }
    let labels: Vec<&str> = lower.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    if labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    if !labels
        .iter()
        .all(|l| l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        return None;
    }
    let tld = labels[labels.len() - 1];
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(HostShape::Public)
}

/// An IPv4 literal, and whether it is one this machine should reach over
/// http. `1.2.3` is not one — which is what keeps a version number from
/// being navigated to.
fn ipv4_shape(host: &str) -> Option<HostShape> {
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 {
        return None;
    }
    let mut parsed = [0u8; 4];
    for (i, o) in octets.iter().enumerate() {
        if o.is_empty() || o.len() > 3 || !o.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        parsed[i] = o.parse::<u8>().ok()?;
    }
    // Loopback and the three private ranges. Everything else is a public
    // address and gets https like any other public host.
    let local = parsed[0] == 127
        || parsed[0] == 10
        || (parsed[0] == 192 && parsed[1] == 168)
        || (parsed[0] == 172 && (16..32).contains(&parsed[1]));
    Some(if local {
        HostShape::Local
    } else {
        HostShape::Public
    })
}

/// Percent-encode a query for a `?q=` slot.
///
/// Hand-written rather than a dependency, because the whole of it is the
/// unreserved set from RFC 3986 plus the `+`-for-space convention that every
/// search engine expects in a query string. Adding a crate to the graph for
/// twenty lines would be the more expensive choice, and this one is
/// exercised by name.
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------

/// What one row of the dropdown is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// Row zero: what Return does right now, before any history is
    /// consulted. Always present while there is text, and always first.
    Go,
    /// Row zero, when the line is a search rather than an address.
    SearchWith,
    /// Somewhere this operator has been. `MatchKind` says whether the typed
    /// text matched the address or the title, which is the difference
    /// between "you were going here" and "you were looking for this".
    Visited(MatchKind),
}

/// One row, already reduced to the strings a renderer paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// The line a person reads first.
    pub primary: String,
    /// The line under it, when there is a second fact worth stating.
    pub secondary: Option<String>,
    /// Where choosing this row goes. Every row has one; there is no row that
    /// does nothing.
    pub url: String,
}

/// How many history suggestions are offered under the `Go` row.
///
/// Six. The dropdown floats over a page, and a list that covers the document
/// is a menu that has stopped being a suggestion. Chrome shows eight in a
/// window that is not also showing what you were reading.
pub const MAX_HISTORY_ROWS: usize = 6;

/// Build the dropdown for one typed line.
///
/// Row zero is always what Return does, so **the operator can always see
/// what they are about to commit to** — the single most important property
/// of an address bar, and the one that makes the url-versus-search decision
/// auditable rather than mysterious. History follows, best-ranked first,
/// with anything that duplicates row zero removed: a row that does the same
/// thing as the row above it is a place to lose a click.
pub fn rows(input: &str, matches: &[HistoryMatch], prefix: &str) -> Vec<Row> {
    let Some(target) = resolve(input, prefix) else {
        return Vec::new();
    };
    let mut rows = Vec::with_capacity(1 + matches.len().min(MAX_HISTORY_ROWS));
    match &target {
        Target::Address { url, typed_scheme } => rows.push(Row {
            kind: RowKind::Go,
            primary: url.clone(),
            // Say so when the scheme was inferred. The operator typed
            // `example.com` and is about to go to `https://example.com` —
            // that is a fact about their navigation, not a detail.
            secondary: (!typed_scheme).then(|| "https added".to_string()),
            url: url.clone(),
        }),
        Target::Search { url, query, host } => rows.push(Row {
            kind: RowKind::SearchWith,
            primary: query.clone(),
            secondary: Some(format!("search {host}")),
            url: url.clone(),
        }),
    }
    let first = rows[0].url.clone();
    for m in matches.iter() {
        if rows.len() > MAX_HISTORY_ROWS {
            break;
        }
        if m.entry.url == first {
            continue;
        }
        rows.push(Row {
            kind: RowKind::Visited(m.matched),
            primary: m
                .entry
                .title
                .clone()
                .unwrap_or_else(|| eave_wire::typing_form(&m.entry.url)),
            // The address under the title, always — a suggestion whose
            // address is hidden is a suggestion you cannot check. When the
            // entry has no title the address IS the primary line, so
            // repeating it below would be noise.
            secondary: m
                .entry
                .title
                .as_ref()
                .map(|_| eave_wire::typing_form(&m.entry.url)),
            url: m.entry.url.clone(),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_wire::HistoryEntry;

    fn t(input: &str) -> Target {
        resolve(input, DEFAULT_SEARCH).expect("something typed")
    }

    fn addr(input: &str) -> String {
        match t(input) {
            Target::Address { url, .. } => url,
            other => panic!("{input:?} was read as a search: {other:?}"),
        }
    }

    fn query(input: &str) -> String {
        match t(input) {
            Target::Search { query, .. } => query,
            other => panic!("{input:?} was read as an address: {other:?}"),
        }
    }

    #[test]
    fn nothing_typed_decides_nothing() {
        assert!(resolve("", DEFAULT_SEARCH).is_none());
        assert!(resolve("   ", DEFAULT_SEARCH).is_none());
    }

    /// A scheme the operator wrote is a decision already made, and is passed
    /// through byte for byte.
    #[test]
    fn a_written_scheme_is_taken_at_its_word() {
        for url in [
            "https://example.com/a?b=c#d",
            "http://example.com",
            "file:///Users/x/page.html",
            "about:blank",
            "view-source:https://example.com",
            "ws://localhost:9222",
        ] {
            assert_eq!(addr(url), url, "{url} was not passed through");
        }
        assert!(matches!(
            t("https://example.com"),
            Target::Address {
                typed_scheme: true,
                ..
            }
        ));
    }

    /// **The self-XSS paste.** `javascript:` and `data:` are valid schemes
    /// and are deliberately not navigable from here. Chrome and Safari both
    /// refuse them, because "paste this in the address bar" is a real attack
    /// delivered by social engineering rather than by a bug.
    #[test]
    fn javascript_and_data_urls_are_text_not_addresses() {
        for hostile in [
            "javascript:alert(document.cookie)",
            "JavaScript:fetch('//evil')",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
        ] {
            let target = resolve(hostile, DEFAULT_SEARCH).unwrap();
            match target {
                Target::Search { query, .. } => assert_eq!(query, hostile),
                Target::Address { url, .. } => {
                    panic!("a {hostile:?} paste would have been navigated to: {url}")
                }
            }
        }
    }

    /// The rung that has to guess. Every one of these is a real thing people
    /// type, and the split between them is the whole feature.
    #[test]
    fn host_shaped_text_is_an_address_and_a_sentence_is_not() {
        assert_eq!(addr("example.com"), "https://example.com");
        assert_eq!(addr("docs.github.com/en"), "https://docs.github.com/en");
        assert_eq!(addr("example.com:8443/x"), "https://example.com:8443/x");
        assert_eq!(addr("my-host.co.uk"), "https://my-host.co.uk");

        assert_eq!(query("rust trait objects"), "rust trait objects");
        assert_eq!(query("eave"), "eave");
        // A version number is not a host: `3` is not a TLD.
        assert_eq!(query("1.2.3"), "1.2.3");
        // A sentence fragment with a dot and no real TLD.
        assert_eq!(query("e.g"), "e.g");
        // A trailing dot leaves an empty label.
        assert_eq!(query("example."), "example.");
        // An email address is looked up far more often than it is visited.
        assert_eq!(
            query("mike@summitpoint-collective.com"),
            "mike@summitpoint-collective.com"
        );
        // A colon with a non-numeric tail was never an authority.
        assert_eq!(query("note:remember"), "note:remember");
    }

    /// **http for the machine, https for the world.** A dev server has no
    /// certificate, and https on `localhost:3000` is a connection error with
    /// no explanation. Everywhere else, guessing http would be downgrading
    /// somebody's connection on their behalf.
    #[test]
    fn only_local_addresses_are_reached_over_http() {
        assert_eq!(addr("localhost"), "http://localhost");
        assert_eq!(addr("localhost:3000"), "http://localhost:3000");
        assert_eq!(addr("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(addr("192.168.1.10"), "http://192.168.1.10");
        assert_eq!(addr("10.0.0.1"), "http://10.0.0.1");
        assert_eq!(addr("172.16.0.1"), "http://172.16.0.1");

        // Public addresses are not local, whatever shape they are.
        assert_eq!(addr("8.8.8.8"), "https://8.8.8.8");
        assert_eq!(addr("172.32.0.1"), "https://172.32.0.1");
        assert_eq!(addr("example.com"), "https://example.com");
    }

    /// An octet out of range is not an address at all — `999.1.1.1` is text.
    #[test]
    fn an_impossible_octet_is_not_an_address() {
        assert_eq!(query("999.1.1.1"), "999.1.1.1");
        assert_eq!(query("1.2.3.4.5"), "1.2.3.4.5");
    }

    #[test]
    fn a_search_is_encoded_and_names_who_receives_it() {
        match t("rust trait objects") {
            Target::Search { url, host, .. } => {
                assert_eq!(url, "https://www.google.com/search?q=rust+trait+objects");
                assert_eq!(host, "google.com");
            }
            other => panic!("{other:?}"),
        }
        match resolve("a&b=c/d?e", DEFAULT_SEARCH).unwrap() {
            Target::Search { url, .. } => {
                assert_eq!(url, "https://www.google.com/search?q=a%26b%3Dc%2Fd%3Fe")
            }
            other => panic!("{other:?}"),
        }
    }

    /// Non-ASCII goes through as UTF-8 bytes, one `%XX` each.
    #[test]
    fn a_query_encodes_utf8_by_the_byte() {
        match resolve("café", DEFAULT_SEARCH).unwrap() {
            Target::Search { url, .. } => {
                assert_eq!(url, "https://www.google.com/search?q=caf%C3%A9")
            }
            other => panic!("{other:?}"),
        }
    }

    /// The override renames the row. A default that changed where the text
    /// went while still saying "google.com" would be worse than having no
    /// sentence at all.
    ///
    /// The override under test is the engine that used to be the default,
    /// which is the case that has to keep working: DuckDuckGo is one env
    /// var away, and the row has to say so.
    #[test]
    fn an_overridden_engine_is_named_from_the_url_it_points_at() {
        let prefix = "https://duckduckgo.com/?q=";
        assert_eq!(engine_host(prefix), "duckduckgo.com");
        match resolve("hello", prefix).unwrap() {
            Target::Search { url, host, .. } => {
                assert_eq!(url, "https://duckduckgo.com/?q=hello");
                assert_eq!(host, "duckduckgo.com");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            engine_host("https://html.duckduckgo.com/html/?q="),
            "html.duckduckgo.com"
        );
    }

    fn entry(url: &str, title: Option<&str>) -> HistoryMatch {
        HistoryMatch {
            entry: HistoryEntry {
                url: url.into(),
                title: title.map(str::to_string),
                visit_count: 3,
                first_visit_ms: 0,
                last_visit_ms: 0,
            },
            matched: MatchKind::Address,
        }
    }

    /// Row zero is always what Return does. It is what makes the
    /// url-versus-search decision auditable instead of mysterious.
    #[test]
    fn row_zero_is_always_what_return_does() {
        let r = rows("example.com", &[], DEFAULT_SEARCH);
        assert_eq!(r[0].kind, RowKind::Go);
        assert_eq!(r[0].url, "https://example.com");
        assert_eq!(r[0].secondary.as_deref(), Some("https added"));

        let r = rows("https://example.com", &[], DEFAULT_SEARCH);
        assert_eq!(
            r[0].secondary, None,
            "a scheme the operator typed is not news"
        );

        let r = rows("rust traits", &[], DEFAULT_SEARCH);
        assert_eq!(r[0].kind, RowKind::SearchWith);
        assert_eq!(r[0].secondary.as_deref(), Some("search google.com"));
    }

    /// A suggestion that does exactly what the row above it does is a place
    /// to lose a click.
    #[test]
    fn a_history_row_that_duplicates_row_zero_is_dropped() {
        let hits = [
            entry("https://example.com", Some("Example")),
            entry("https://example.com/deep", Some("Deep")),
        ];
        let r = rows("example.com", &hits, DEFAULT_SEARCH);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].url, "https://example.com");
        assert_eq!(r[1].url, "https://example.com/deep");
    }

    /// The dropdown floats over a document. A list long enough to cover it
    /// has stopped being a suggestion.
    #[test]
    fn the_dropdown_is_capped_and_the_cap_counts_row_zero() {
        let hits: Vec<HistoryMatch> = (0..30)
            .map(|i| entry(&format!("https://n{i}.test/"), None))
            .collect();
        let r = rows("n", &hits, DEFAULT_SEARCH);
        assert_eq!(r.len(), MAX_HISTORY_ROWS + 1);
        assert!(r.iter().all(|r| !r.url.is_empty()), "a row went nowhere");
    }

    /// An entry with no title reads as its address, and does not then repeat
    /// the address underneath itself.
    #[test]
    fn an_untitled_entry_reads_as_its_address_once() {
        let hits = [entry("https://plain.test/path", None)];
        let r = rows("plain", &hits, DEFAULT_SEARCH);
        assert_eq!(r[1].primary, "plain.test/path");
        assert_eq!(r[1].secondary, None);

        let hits = [entry("https://titled.test/path", Some("Titled"))];
        let r = rows("titled", &hits, DEFAULT_SEARCH);
        assert_eq!(r[1].primary, "Titled");
        assert_eq!(r[1].secondary.as_deref(), Some("titled.test/path"));
    }
}
