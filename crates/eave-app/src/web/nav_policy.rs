//! What a navigation is allowed to do, decided before it starts.
//!
//! `RequestHandler::on_before_browse` is the one place eave sees a
//! navigation *before* Chromium commits to it, and it was empty. Three
//! abilities live here, all of them Brave's, all of them decisions about a
//! URL and nothing else:
//!
//!   * **B7** — `mailto:` / `tel:` and friends are not pages. Without this
//!     they are dead links: Chromium has no handler registered for them
//!     inside an off-screen browser, so the click does nothing at all.
//!   * **E5** — tracking query parameters are stripped from cross-site
//!     navigations.
//!   * the refusal half of B7: a scheme eave does not know is not handed to
//!     the operating system.
//!
//! # Why this file is not behind `--features web`
//! Every function here is a pure function of a URL string. Slice E1
//! established the pattern and the reason: `cargo test` runs with the
//! feature OFF, so anything behind the gate is code the suite never sees.
//! A tracker list and a tokenizer are exactly the kind of thing that must
//! be asserted, and the gated half is only the four lines that call in.
//!
//! # Provenance
//! The rules are **Brave's**, read out of `brave-core` rather than invented:
//! `components/query_filter/browser/utils.cc` for the policy,
//! `components/query_filter/browser/test_support/query_filter_test_helper.cc`
//! for the parameter list, and
//! `browser/net/brave_site_hacks_network_delegate_helper_unittest.cc` for
//! the edge cases — which are ported verbatim into this module's tests,
//! because a tokenizer this fiddly is worth checking against the browser
//! that has already been wrong about it in public.

/// What `on_before_browse` should do with a navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Proceed exactly as asked. The overwhelmingly common answer.
    Allow,
    /// Proceed, but at this URL instead — a tracked link with its trackers
    /// removed. The original navigation is cancelled and this one is asked
    /// for in its place.
    Rewrite(String),
    /// Not a page. Hand it to the operating system.
    External(String),
    /// The same page, over TLS (E2).
    ///
    /// Separate from [`Self::Rewrite`] because the caller has an obligation
    /// a plain rewrite does not carry: it must **remember** the upgrade, so
    /// that a failure can go back. `fallback` is the http URL to return to
    /// — already query-stripped, so falling back does not reintroduce a
    /// tracker that was removed on the way up.
    Upgrade { to: String, fallback: String },
    /// A scheme eave will neither render nor hand to the OS.
    ///
    /// This is the same posture `ui::open_url` already takes for markdown
    /// links, for the same reason: `open(1)` launches whatever has
    /// registered for a scheme, and the string arrives from a web page.
    /// An allow-list is the security boundary; a deny-list would be a
    /// guess about every app installed on the machine.
    Block,
}

/// One navigation, and everything a decision needs to know about it.
///
/// A struct rather than six positional arguments because the list grows:
/// strict mode and a per-site shields-down toggle are both settings eave
/// does not have yet, and both belong here when it does.
#[derive(Debug, Clone, Copy)]
pub struct Nav<'a> {
    pub url: &'a str,
    /// Where the tab is now — the initiator of a top-level navigation.
    pub from: Option<&'a str>,
    pub method: &'a str,
    pub is_redirect: bool,
    pub is_main_frame: bool,
    /// False once this host has already failed over TLS this session, which
    /// is what stops eave upgrading the same host on every navigation and
    /// paying a failed connection each time.
    pub allow_upgrade: bool,
}

/// Schemes eave will hand to the OS.
///
/// **An allow-list on purpose, and a short one.** These are the schemes a
/// person unambiguously means to leave the browser with, and every one of
/// them lands in a first-party Apple app. Chrome's behaviour for an
/// *unknown* scheme is to ask first; eave has nowhere to ask from inside
/// `on_before_browse` yet, so it refuses instead — which is what already
/// happens today, only now it is deliberate and traced. A confirm prompt
/// (reusing `web::dialogs`) is what turns those refusals into Chrome's
/// behaviour, and is the next step rather than this one.
const EXTERNAL_SCHEMES: &[&str] = &[
    "mailto",
    "tel",
    "sms",
    "facetime",
    "facetime-audio",
    "webcal",
];

/// Whether eave will hand this scheme to the operating system.
///
/// Public so the syscall site can ask again — see `ui::open_external`. One
/// list, two checks, and the second one is what makes the first a boundary
/// rather than a suggestion.
pub fn is_external_scheme(scheme: &str) -> bool {
    EXTERNAL_SCHEMES.contains(&scheme)
}

/// Schemes that are pages, and stay in the tab.
const WEB_SCHEMES: &[&str] = &["http", "https", "file", "about", "data", "blob", "chrome"];

/// Tracking parameters stripped on every host.
///
/// Brave's global rule (`"include": ["*://*/*"]`), verbatim — 65 entries.
/// In Brave this list is delivered by the component updater rather than
/// compiled in, so it moves without a browser release; this is the set as
/// their own test fixture pins it. **Sorted, because [`is_tracker`] binary
/// searches it** — a test asserts the sort rather than trusting the eye.
const GLOBAL_TRACKERS: &[&str] = &[
    "__hsfp",
    "__hssc",
    "__hstc",
    "__s",
    "_bhlid",
    "_branch_match_id",
    "_branch_referrer",
    "_gl",
    "_hsenc",
    "_openstat",
    "at_recipient_id",
    "at_recipient_list",
    "bbeml",
    "bsft_clkid",
    "bsft_uid",
    "dclid",
    "et_rid",
    "fb_action_ids",
    "fb_comment_id",
    "fbclid",
    "gclid",
    "guce_referrer",
    "guce_referrer_sig",
    "hsCtaTracking",
    "irclickid",
    "mc_eid",
    "ml_subscriber",
    "ml_subscriber_hash",
    "msclkid",
    "mtm_cid",
    "oft_c",
    "oft_ck",
    "oft_d",
    "oft_id",
    "oft_ids",
    "oft_k",
    "oft_lk",
    "oft_sk",
    "oly_anon_id",
    "oly_enc_id",
    "pk_cid",
    "rb_clickid",
    "s_cid",
    "sc_customer",
    "sc_eh",
    "sc_uid",
    "sfmc_activityid",
    "sfmc_id",
    "sms_click",
    "sms_source",
    "sms_uph",
    "srsltid",
    "ss_email_id",
    "syclid",
    "ttclid",
    "twclid",
    "unicorn_click_id",
    "vero_conv",
    "vero_id",
    "vgo_ee",
    "wbraid",
    "wickedid",
    "yclid",
    "ymclid",
    "ysclid",
];

/// Parameters stripped only on certain sites.
///
/// **These are not trackers everywhere.** `si` is a share identifier on
/// YouTube and an ordinary parameter on a thousand other sites; stripping
/// it globally would corrupt requests. Brave scopes them by URL pattern and
/// so does this: the host must equal the entry or be a subdomain of it.
const SCOPED_TRACKERS: &[(&[&str], &[&str])] = &[
    (&["instagram.com"], &["igsh", "igshid"]),
    (&["twitter.com", "x.com"], &["ref_src", "ref_url"]),
    (&["youtube.com", "youtu.be"], &["si"]),
];

/// Parameters stripped **unless** the URL looks like the one page that
/// needs them.
///
/// The subtlety worth keeping: `mkt_tok` identifies the recipient of a
/// marketing email, and it is a tracker on the article it links to — but on
/// the *unsubscribe* page it is the only thing saying who to unsubscribe.
/// Strip it there and the unsubscribe link silently stops working, which is
/// a worse outcome than the tracking.
///
/// Brave matches these with RE2; the three patterns in their list are a
/// literal and one alternation, so a matcher over literals covers them
/// exactly. [`spec_matches`] is that matcher.
const CONDITIONAL_TRACKERS: &[(&str, &[&str])] = &[
    ("ck_subscriber_id", &["/unsubscribe"]),
    ("h_sid", &["/email/"]),
    ("h_slt", &["/email/"]),
    // Brave's pattern is `([uU]nsubscribe|emailWebview)`: the alternation
    // is literal, and the character class only exists to accept a
    // capitalised "Unsubscribe" in a path. Both spellings are listed rather
    // than lower-casing the URL, because the rest of a URL is
    // case-sensitive and folding it would change what the other patterns
    // match.
    ("mkt_tok", &["unsubscribe", "Unsubscribe", "emailWebview"]),
];

/// Hosts the query filter never touches.
///
/// `urldefense.com` rewrites a whole URL into its own path, so "strip a
/// parameter" there means corrupting somebody else's encoding.
const EXEMPT_HOSTS: &[&str] = &["urldefense.com"];

/// Hosts and host shapes that are never upgraded to HTTPS.
///
/// **This list is the feature.** An upgrade with no exclusions breaks every
/// host that genuinely has no TLS, and breaks it as a *connection error* —
/// which reads to the operator as "eave is broken", not "that site is
/// http". Each entry below is a class of host where https is not merely
/// unlikely but wrong.
fn upgradable_host(host: &str) -> bool {
    // Loopback. A dev server on http://localhost is the single most common
    // http URL an eave operator will ever type.
    if host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    // Literal addresses never have a certificate that validates.
    if is_ip_literal(host) {
        return false;
    }
    // mDNS and Tor: `.local` is a LAN name, `.onion` is not reachable over
    // TLS in the ordinary way and upgrading it leaks intent.
    if host.ends_with(".local") || host.ends_with(".onion") {
        return false;
    }
    // A single label is an intranet name — `http://wiki`, `http://build`.
    // There is no public CA that will have issued for it.
    if !host.contains('.') {
        return false;
    }
    true
}

/// Whether a host is an IPv4 or IPv6 literal rather than a name.
///
/// The v6 case arrives here already unbracketed, because [`host`] strips
/// the brackets — so "contains a colon" is enough to recognise it.
fn is_ip_literal(host: &str) -> bool {
    if host.contains(':') {
        return true; // v6 literal
    }
    let mut labels = 0;
    for label in host.split('.') {
        if label.is_empty() || !label.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        labels += 1;
    }
    labels == 4
}

/// The port a URL states, or `None` when it states none.
fn port(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?;
    // An IPv6 literal's colons are inside the brackets; the port, if any,
    // is after them.
    let after = match hostport.split_once(']') {
        Some((_, tail)) => tail,
        None => hostport,
    };
    after
        .split_once(':')
        .map(|(_, p)| p)
        .filter(|p| !p.is_empty())
}

/// `http://…` as `https://…`, if this navigation should be upgraded.
///
/// Pure — the session's memory of what has already failed arrives as
/// [`Nav::allow_upgrade`].
///
/// # Two deliberate refusals
/// **A non-default port is never upgraded.** `http://host:8080` is a dev
/// server or an appliance, and https on the same port is a different
/// service or nothing at all. Chrome's HTTPS-Upgrades declines these too.
///
/// **A redirect target is never upgraded.** A site that deliberately
/// redirects https → http would otherwise be upgraded straight back, and
/// the two would trade the navigation forever. Only a navigation somebody
/// asked for is a candidate.
pub fn upgrade_target(nav: &Nav<'_>) -> Option<String> {
    if !nav.allow_upgrade || !nav.is_main_frame || nav.is_redirect {
        return None;
    }
    // A POST re-issued over a different scheme is a second submission.
    if !nav.method.eq_ignore_ascii_case("GET") {
        return None;
    }
    if scheme(nav.url)?.as_str() != "http" {
        return None;
    }
    if port(nav.url).is_some_and(|p| p != "80") {
        return None;
    }
    if !upgradable_host(&host(nav.url)?) {
        return None;
    }
    Some(format!("https://{}", nav.url.strip_prefix("http://")?))
}

/// The scheme of a URL, lower-cased, or `None` if it has none.
///
/// Deliberately not a URL parser. The only question asked of the scheme is
/// which of three lists it is in, and a parser would bring a dependency and
/// an opinion about the rest of the string that this module does not want —
/// see [`strip_query`] for why the rest of the string is left alone.
pub fn scheme(url: &str) -> Option<String> {
    let end = url.find(':')?;
    let s = &url[..end];
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    // A scheme must start with a letter — otherwise "1:30" is a scheme.
    if !s.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

/// The host of an http(s) URL, lower-cased, without userinfo or port.
pub fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    // Authority ends at the first `/`, `?` or `#`.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // `user:pass@host` — the host is after the LAST `@`, since userinfo may
    // itself contain one.
    let hostport = authority.rsplit('@').next()?;
    let h = match hostport.strip_prefix('[') {
        // IPv6 literal: the colons inside are not a port separator.
        Some(v6) => v6.split(']').next()?,
        None => hostport.split(':').next()?,
    };
    if h.is_empty() {
        return None;
    }
    Some(h.to_ascii_lowercase())
}

/// Whether `host` is `suffix` or a subdomain of it.
fn host_is(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

/// Whether two hosts count as the same site.
///
/// ⚠️ **An approximation, and it errs in the safe direction on purpose.**
/// Brave asks Chromium's registry-controlled-domains, which carries the
/// Public Suffix List and therefore knows that `a.co.uk` and `b.co.uk` are
/// *different* sites. Comparing the last two labels says they are the same.
///
/// The consequence of the error is one-directional: same-site is the
/// **exempt** case, so being too generous strips *less* than Brave does. It
/// can never strip a parameter Brave would have kept, which is the failure
/// that breaks a website. Tightening this needs a PSL, which is a
/// dependency and a data file that has to be updated; the honest state is
/// that eave under-strips on multi-label public suffixes.
fn same_site(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let site = |h: &str| {
        let labels: Vec<&str> = h.split('.').collect();
        if labels.len() <= 2 {
            h.to_string()
        } else {
            labels[labels.len() - 2..].join(".")
        }
    };
    site(a) == site(b)
}

/// Whether a URL contains any of a conditional tracker's literal patterns.
fn spec_matches(spec: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| spec.contains(p))
}

/// Whether `param` is stripped for a URL on `host`.
fn is_tracker(param: &str, host: &str) -> bool {
    if GLOBAL_TRACKERS.binary_search(&param).is_ok() {
        return true;
    }
    SCOPED_TRACKERS
        .iter()
        .any(|(hosts, params)| params.contains(&param) && hosts.iter().any(|h| host_is(host, h)))
}

/// Remove tracking parameters from a query string, or `None` if none were
/// found.
///
/// **Hand-tokenized, and Brave hand-tokenizes it too** — deliberately, for
/// the same reason. Round-tripping through a URL parser re-encodes what it
/// did not change: `?a+b=some%20thing` comes back differently spelled, and
/// a server that signed the query rejects it. Splitting on `&` and putting
/// the untouched tokens back preserves every byte that was not removed.
///
/// The three rules that look like bugs and are not:
///   * a token with no `=` is **kept** — `?fbclid` alone is not `fbclid=…`
///     and carries nothing;
///   * an **empty value** is kept — `?fbclid=` identifies nobody;
///   * an **empty name** is kept — `?=msclkid` names no parameter.
///
/// All three are cases the browsers get wrong; all three are asserted below.
pub fn strip_query(query: &str, spec: &str, host: &str) -> Option<String> {
    let mut kept: Vec<&str> = Vec::new();
    let mut stripped = false;

    for token in query.split('&') {
        let Some((param, value)) = token.split_once('=') else {
            // Not a `name=value` token at all. Back it goes, verbatim.
            kept.push(token);
            continue;
        };
        if param.is_empty() || value.is_empty() {
            kept.push(token);
            continue;
        }
        if is_tracker(param, host) {
            stripped = true;
            continue;
        }
        // Independent of the lists above: strip unless this is the page
        // that needs the parameter.
        if let Some((_, patterns)) = CONDITIONAL_TRACKERS.iter().find(|(p, _)| *p == param) {
            if !spec_matches(spec, patterns) {
                stripped = true;
                continue;
            }
        }
        kept.push(token);
    }

    stripped.then(|| kept.join("&"))
}

/// Split a URL into everything before the query, the query, and the
/// fragment — without decoding any of it.
fn parts(url: &str) -> (&str, Option<&str>, &str) {
    let (before_frag, frag) = match url.find('#') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    match before_frag.find('?') {
        Some(i) => (&before_frag[..i], Some(&before_frag[i + 1..]), frag),
        None => (before_frag, None, frag),
    }
}

/// What to do with a navigation.
///
/// The order matters and is not arbitrary: scheme first (a `mailto:` is not
/// a page and nothing else applies to it), then the query filter, then the
/// upgrade. Stripping before upgrading means the fallback URL is already
/// clean, so a site that fails over TLS does not get its trackers back on
/// the way down.
pub fn decide(nav: Nav<'_>) -> Decision {
    let Some(scheme) = scheme(nav.url) else {
        // No scheme at all. Not this module's problem to fix.
        return Decision::Allow;
    };
    if EXTERNAL_SCHEMES.contains(&scheme.as_str()) {
        return Decision::External(nav.url.to_string());
    }
    if !WEB_SCHEMES.contains(&scheme.as_str()) {
        return Decision::Block;
    }

    let stripped = stripped_url(&nav);
    let http_url = stripped.as_deref().unwrap_or(nav.url);

    // E2. Asked of the stripped URL, so `fallback` is the clean one.
    let upgraded = {
        let on_clean = Nav {
            url: http_url,
            ..nav
        };
        upgrade_target(&on_clean)
    };
    if let Some(to) = upgraded {
        return Decision::Upgrade {
            to,
            fallback: http_url.to_string(),
        };
    }
    match stripped {
        Some(clean) => Decision::Rewrite(clean),
        None => Decision::Allow,
    }
}

/// The URL with its tracking parameters removed, or `None` if it had none.
///
/// Every gate here is Brave's, and each one exists because removing it
/// breaks a real site rather than because it is tidy.
fn stripped_url(nav: &Nav<'_>) -> Option<String> {
    // Only GET. A POST carries its payload separately and rewriting the
    // URL would re-issue it as a different request.
    if !nav.method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let (head, query, frag) = parts(nav.url);
    let query = query?;
    if query.is_empty() {
        return None;
    }
    let host = host(nav.url)?;
    if EXEMPT_HOSTS.iter().any(|h| host_is(&host, h)) {
        return None;
    }
    // Same-site navigation: the parameters are the site's own business.
    // This is the gate that keeps analytics-driven site features working,
    // and dropping it is how a query filter earns a reputation for breaking
    // logins. A redirect within a site is the same case — the site's own
    // plumbing.
    if let Some(from) = nav.from {
        if let Some(from_host) = self::host(from) {
            if same_site(&host, &from_host) {
                return None;
            }
        }
    }
    let _ = nav.is_redirect;

    let clean = strip_query(query, nav.url, &host)?;
    // Brave's last gate: only rewrite if the query actually got shorter.
    if clean.len() >= query.len() {
        return None;
    }
    Some(if clean.is_empty() {
        format!("{head}{frag}")
    } else {
        format!("{head}?{clean}{frag}")
    })
}

// ---------------------------------------------------------------------------
// The session's memory (E2)
// ---------------------------------------------------------------------------
//
// Two pieces of state, both here rather than behind the feature gate, for
// the reason the rest of this file is here: the suite runs with the feature
// OFF, and a fallback that has never been asserted is a fallback nobody
// should trust.

thread_local! {
    /// Hosts that have already failed over TLS in this session.
    ///
    /// Without this, every navigation to an http-only host pays a failed
    /// TLS connection first — so the site works, but every page load on it
    /// is slower than it was before eave "improved" it.
    static DOWNGRADED: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());

    /// The upgrade a tab is currently attempting: the https URL in flight,
    /// and the http URL to go back to.
    ///
    /// One slot per tab, not a queue. A second navigation replaces the
    /// first, because the first is no longer happening.
    static IN_FLIGHT: std::cell::RefCell<std::collections::HashMap<String, (String, String)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Whether this host may still be upgraded.
pub fn may_upgrade(host: &str) -> bool {
    DOWNGRADED.with(|d| !d.borrow().contains(host))
}

/// Remember that this host does not serve TLS, so eave stops trying.
pub fn note_downgrade(host: &str) {
    DOWNGRADED.with(|d| {
        d.borrow_mut().insert(host.to_string());
    });
}

/// Record an upgrade this tab is attempting.
pub fn note_upgrade(tab_id: &str, to: &str, fallback: &str) {
    IN_FLIGHT.with(|f| {
        f.borrow_mut()
            .insert(tab_id.to_string(), (to.to_string(), fallback.to_string()));
    });
}

/// Why a load ended without a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The navigation was cancelled — by eave, or by the operator pressing
    /// stop. **Never a reason to fall back**, and this is the trap the whole
    /// enum exists to avoid: eave performs an upgrade *by cancelling* the
    /// http navigation, so `ERR_ABORTED` arrives on every single upgrade. A
    /// fallback that fired on it would undo every upgrade it ever made,
    /// instantly and invisibly.
    Aborted,
    /// The connection failed, the name did not resolve, the certificate was
    /// rejected. The site is not reachable this way.
    Unreachable,
}

/// Whether this failure is one the operator should be shown an error for.
///
/// # `ERR_ABORTED` must never reach `LoadState::Failed`
/// An abort is not a site that would not load — it is a load that eave, the
/// page, or the operator *stopped on purpose*, and there are three ordinary
/// ways to get one:
///
/// - **B4.** The operator pressed ⎋ while the page was loading.
/// - **A superseded navigation.** A link clicked while the previous page is
///   still loading aborts the first. This is the common one, and it needs no
///   new feature to reach — it has been reachable since B1.
/// - **An HTTPS upgrade.** E2 performs an upgrade *by cancelling* the http
///   navigation, so an abort arrives on every upgrade eave makes.
///
/// In all three the right screen is the one already there, and Chromium
/// agrees: `ERR_ABORTED` is the one net error Chrome deliberately does not
/// render an error page for.
///
/// # This is half a rule the file already had, applied in only one place
/// [`fallback`] returns `None` for `Aborted` and `on_load_error`'s comment
/// spells the reason out — "the tab is not failing, it is taking the other
/// route… reporting `LoadState::Failed` here would flash an error state that
/// the very next navigation replaces." That early return only guards the
/// *upgrade* branch. An abort with no upgrade outstanding fell straight
/// through it and was reported as a failure with the reason `ERR_ABORTED`,
/// which is the same flash the comment refuses, arrived at by the other
/// path. B4 is what made it worth fixing rather than what introduced it.
pub fn reports_failure(why: Failure) -> bool {
    why != Failure::Aborted
}

/// A load failed. If it was an upgrade eave made, returns the http URL to
/// go back to — and remembers not to upgrade that host again.
///
/// # An HTTP error status is not a failure
/// A 404 or a 500 over https is a **successful connection** to a site that
/// has TLS, and Brave's own test pins this: `broken2.com` serves a 404 over
/// https and the expected result is that the page stays on https. CEF makes
/// this easy to get right — `on_load_error` does not fire for an HTTP error
/// status, only for a load that produced no response at all.
pub fn fallback(tab_id: &str, failed_url: &str, why: Failure) -> Option<String> {
    if why == Failure::Aborted {
        return None;
    }
    let (in_flight, http_url) = IN_FLIGHT.with(|f| f.borrow().get(tab_id).cloned())?;
    if in_flight != failed_url {
        // A different navigation failed. The upgrade, if any, is still
        // outstanding and is not answered by this.
        return None;
    }
    // Taken, not read: if the http attempt fails too, there is no record
    // left and nothing to loop on.
    IN_FLIGHT.with(|f| {
        f.borrow_mut().remove(tab_id);
    });
    if let Some(h) = host(failed_url) {
        note_downgrade(&h);
    }
    Some(http_url)
}

/// A tab went away.
pub fn forget(tab_id: &str) {
    IN_FLIGHT.with(|f| {
        f.borrow_mut().remove(tab_id);
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The URL cases are ported from
// `brave-core/browser/net/brave_site_hacks_network_delegate_helper_unittest.cc`
// — both the `QueryStringUntouched` list and the `QueryStringFiltered`
// pairs, with their expected outputs. Porting the assertions rather than
// re-deriving them is the point: these are the cases a hand-written
// tokenizer gets wrong, and they come with an oracle.

#[cfg(test)]
mod tests {
    use super::*;

    /// B4: an abort is never an error page, and an unreachable host always
    /// is.
    ///
    /// The second assertion is the one that keeps this honest — a rule that
    /// answered `false` for everything would satisfy the first half and
    /// silence every real load failure in the product.
    #[test]
    fn a_stop_is_not_an_error_and_an_unreachable_host_still_is() {
        assert!(!reports_failure(Failure::Aborted));
        assert!(reports_failure(Failure::Unreachable));
    }

    /// The abort rule and the FALLBACK rule are separate, and an abort has
    /// to satisfy both.
    ///
    /// They come from opposite directions and it would be easy to collapse
    /// them: `fallback` refuses an abort so an upgrade is never undone,
    /// while `reports_failure` refuses one so a stop never draws an error.
    /// An abort with an upgrade outstanding must still be silent — that is
    /// the case where BOTH rules fire on one event, and it is E2's every
    /// upgrade.
    #[test]
    fn an_abort_mid_upgrade_neither_falls_back_nor_reports() {
        note_upgrade("tab-b4", "https://up.test/", "http://up.test/");
        assert_eq!(
            fallback("tab-b4", "https://up.test/", Failure::Aborted),
            None,
            "an abort must never trigger the fallback that undoes the upgrade"
        );
        assert!(!reports_failure(Failure::Aborted));
        // The upgrade is still outstanding — the abort answered nothing, so
        // a REAL failure on the same url can still fall back. Without this
        // the abort would have consumed the record and the fallback would
        // be dead for the case it exists for.
        assert_eq!(
            fallback("tab-b4", "https://up.test/", Failure::Unreachable),
            Some("http://up.test/".to_string())
        );
        forget("tab-b4");
    }

    /// Cross-site, so the filter is actually reached.
    const FROM: Option<&str> = Some("https://example.net/");

    /// An ordinary top-level GET, upgrades allowed.
    fn nav(url: &str) -> Nav<'_> {
        Nav {
            url,
            from: FROM,
            method: "GET",
            is_redirect: false,
            is_main_frame: true,
            allow_upgrade: true,
        }
    }

    /// The query filter in isolation.
    ///
    /// `allow_upgrade: false` on purpose — several of Brave's cases are
    /// `http://` URLs, and with E2 in play they would come back as
    /// [`Decision::Upgrade`] and prove nothing about the filter. The two
    /// features are tested separately because they are separate decisions;
    /// `an_http_url_is_stripped_and_upgraded_in_one_step` is where they meet.
    fn filtered(url: &str) -> Decision {
        decide(Nav {
            allow_upgrade: false,
            ..nav(url)
        })
    }

    #[test]
    fn the_global_list_is_sorted_because_the_lookup_binary_searches_it() {
        let mut sorted = GLOBAL_TRACKERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(GLOBAL_TRACKERS, sorted.as_slice());
    }

    #[test]
    fn schemes_are_recognised_and_a_time_is_not_one() {
        assert_eq!(scheme("https://a.test/"), Some("https".into()));
        assert_eq!(scheme("MAILTO:a@b.test"), Some("mailto".into()));
        assert_eq!(scheme("1:30"), None, "a clock is not a scheme");
        assert_eq!(scheme("/path/only"), None);
    }

    #[test]
    fn hosts_survive_userinfo_ports_and_v6() {
        assert_eq!(host("https://example.com/p?q"), Some("example.com".into()));
        assert_eq!(host("http://u:p@example.com/x"), Some("example.com".into()));
        assert_eq!(
            host("https://example.com:8443/"),
            Some("example.com".into())
        );
        assert_eq!(host("https://u@h@example.com/"), Some("example.com".into()));
        assert_eq!(host("https://[::1]:8080/"), Some("::1".into()));
    }

    // ── B7: schemes ───────────────────────────────────────────────────

    #[test]
    fn mail_and_phone_links_leave_the_browser() {
        assert_eq!(
            decide(nav("mailto:a@b.test")),
            Decision::External("mailto:a@b.test".into())
        );
        assert_eq!(
            decide(nav("tel:+15551234")),
            Decision::External("tel:+15551234".into())
        );
    }

    /// The refusal is the half that matters for safety: `open(1)` launches
    /// whatever registered for a scheme, and this string came from a page.
    #[test]
    fn an_unknown_scheme_is_refused_rather_than_handed_to_the_os() {
        assert_eq!(decide(nav("zoommtg://zoom.us/join")), Decision::Block);
        assert_eq!(decide(nav("javascript:alert(1)")), Decision::Block);
    }

    #[test]
    fn ordinary_pages_are_allowed() {
        assert_eq!(decide(nav("https://a.test/")), Decision::Allow);
        assert_eq!(decide(nav("about:blank")), Decision::Allow);
    }

    // ── E5: Brave's `QueryStringUntouched`, ported ────────────────────

    #[test]
    fn brave_untouched_cases_are_untouched() {
        for url in [
            "https://example.com/",
            "https://example.com/?",
            "https://example.com/?+%20",
            "https://user:pass@example.com/path/file.html?foo=1#fragment",
            "http://user:pass@example.com/path/file.html?foo=1&bar=2#fragment",
            "https://example.com/?file=https%3A%2F%2Fexample.com%2Ftest.pdf",
            "https://example.com/?title=1+2&caption=1%202",
            "https://example.com/?foo=1&&bar=2#fragment",
            "https://example.com/?foo&bar=&#fragment",
            "https://example.com/?foo=1&fbcid=no&gcid=no&mc_cid=no&bar=&#frag",
            "https://example.com/?fbclid=&gclid&=mc_eid&msclkid=",
            "https://example.com/?value=fbclid=1&not-gclid=2&foo+mc_eid=3",
            "https://example.com/?+fbclid=1",
            "https://example.com/?%20fbclid=1",
            "https://example.com/#fbclid=1",
            "https://example.com/?__ss=1234-abcd",
            "https://example.com/?mkt_tok=123&mkt_unsubscribe=1",
            "https://example.com/?mkt_unsubscribe=1&fake_param=abc&mkt_tok=123",
            "https://example.com/Unsubscribe.html?fake_param=abc&mkt_tok=123",
        ] {
            assert_eq!(
                filtered(url),
                Decision::Allow,
                "should not have touched {url}"
            );
        }
    }

    // ── E5: Brave's `QueryStringFiltered`, ported with its outputs ────

    #[test]
    fn brave_filtered_cases_produce_braves_urls() {
        for (before, after) in [
            ("https://example.com/?fbclid=1234", "https://example.com/"),
            ("https://example.com/?fbclid=1234&", "https://example.com/"),
            ("https://example.com/?&fbclid=1234", "https://example.com/"),
            ("https://example.com/?gclid=1234", "https://example.com/"),
            (
                "https://example.com/?fbclid=0&gclid=1&msclkid=a&mc_eid=a1",
                "https://example.com/",
            ),
            (
                "https://example.com/?fbclid=&foo=1&bar=2&gclid=abc",
                "https://example.com/?fbclid=&foo=1&bar=2",
            ),
            (
                "https://example.com/?fbclid=&foo=1&gclid=1234&bar=2",
                "https://example.com/?fbclid=&foo=1&bar=2",
            ),
            (
                "http://u:p@example.com/path/file.html?foo=1&fbclid=abcd#fragment",
                "http://u:p@example.com/path/file.html?foo=1#fragment",
            ),
            ("https://example.com/?__s=1234-abcd", "https://example.com/"),
            ("https://example.com/?fbclid==1", "https://example.com/"),
            ("https://example.com/?fbclid==", "https://example.com/"),
            (
                "https://example.com/?fbclid&foo&&gclid=2&bar=&%20",
                "https://example.com/?fbclid&foo&&bar=&%20",
            ),
            (
                "https://example.com/?fbclid=1&1==2&=msclkid&foo=bar&&a=b=c&",
                "https://example.com/?1==2&=msclkid&foo=bar&&a=b=c&",
            ),
            (
                "https://example.com/?fbclid=1&=2&?foo=yes&bar=2+",
                "https://example.com/?=2&?foo=yes&bar=2+",
            ),
            (
                "https://example.com/?fbclid=1&a+b+c=some%20thing&1%202=3+4",
                "https://example.com/?a+b+c=some%20thing&1%202=3+4",
            ),
            (
                "https://example.com/?mkt_tok=123&foo=bar",
                "https://example.com/?foo=bar",
            ),
        ] {
            assert_eq!(
                filtered(before),
                Decision::Rewrite(after.to_string()),
                "wrong rewrite for {before}"
            );
        }
    }

    /// The scoped rules, both directions. `si` is the one that shows why
    /// scoping is not fussiness: stripped on YouTube, ordinary everywhere.
    #[test]
    fn scoped_trackers_are_stripped_only_on_their_own_sites() {
        assert_eq!(
            filtered("https://example.com/?igshid=1234"),
            Decision::Allow
        );
        assert_eq!(
            filtered("https://www.instagram.com/?igshid=1234"),
            Decision::Rewrite("https://www.instagram.com/".into())
        );
        assert_eq!(
            filtered("https://brave.com/?ref_src=example.com"),
            Decision::Allow
        );
        assert_eq!(
            filtered("https://twitter.com/?ref_src=example.com"),
            Decision::Rewrite("https://twitter.com/".into())
        );
        assert_eq!(
            filtered("https://x.com/?ref_src=example.com"),
            Decision::Rewrite("https://x.com/".into())
        );
        assert_eq!(
            filtered("https://example.com/?si=abc"),
            Decision::Allow,
            "`si` is an ordinary parameter off YouTube"
        );
        assert_eq!(
            filtered("https://youtu.be/xyz?si=abc"),
            Decision::Rewrite("https://youtu.be/xyz".into())
        );
    }

    /// The unsubscribe case. Stripping `mkt_tok` on the article is the
    /// point; stripping it on the unsubscribe page breaks unsubscribing.
    #[test]
    fn a_conditional_tracker_survives_on_the_page_that_needs_it() {
        assert_eq!(
            filtered("https://example.com/?mkt_tok=123&foo=bar"),
            Decision::Rewrite("https://example.com/?foo=bar".into())
        );
        assert_eq!(
            filtered("https://example.com/index.php/email/emailWebview?mkt_tok=1234&foo=bar"),
            Decision::Allow,
            "the email webview needs its token"
        );
        assert_eq!(
            filtered("https://example.com/unsubscribe?mkt_tok=1234"),
            Decision::Allow,
            "stripping this is how an unsubscribe link stops working"
        );
    }

    // ── E5: the gates ─────────────────────────────────────────────────

    #[test]
    fn a_same_site_navigation_keeps_its_parameters() {
        for from in [
            "https://example.com/path",     // same origin
            "https://sub.example.com/path", // same site
        ] {
            assert_eq!(
                decide(Nav {
                    from: Some(from),
                    ..nav("https://example.com/?fbclid=1")
                }),
                Decision::Allow,
                "stripped a same-site navigation from {from}"
            );
        }
    }

    #[test]
    fn only_get_is_filtered() {
        assert_eq!(
            decide(Nav {
                method: "POST",
                ..nav("https://example.com/?fbclid=1")
            }),
            Decision::Allow
        );
    }

    #[test]
    fn a_same_site_redirect_is_exempt_and_a_cross_site_one_is_not() {
        assert_eq!(
            decide(Nav {
                from: Some("https://sub.example.com/"),
                is_redirect: true,
                ..nav("https://example.com/?fbclid=1")
            }),
            Decision::Allow
        );
        assert_eq!(
            decide(Nav {
                from: Some("https://other.test/"),
                is_redirect: true,
                ..nav("https://example.com/?fbclid=1")
            }),
            Decision::Rewrite("https://example.com/".into())
        );
    }

    #[test]
    fn an_exempt_host_is_left_entirely_alone() {
        assert_eq!(
            filtered("https://urldefense.com/v3/__https://x.test/?fbclid=1__;!!abc$"),
            Decision::Allow
        );
    }

    /// An unknown initiator is treated as cross-site — the stripping
    /// answer — which is what a tab's first load looks like.
    #[test]
    fn an_unknown_initiator_still_strips() {
        assert_eq!(
            decide(Nav {
                from: None,
                ..nav("https://example.com/?fbclid=1")
            }),
            Decision::Rewrite("https://example.com/".into())
        );
    }

    // ── E2: the upgrade ───────────────────────────────────────────────

    #[test]
    fn an_ordinary_http_page_is_upgraded() {
        assert_eq!(
            decide(nav("http://example.com/page")),
            Decision::Upgrade {
                to: "https://example.com/page".into(),
                fallback: "http://example.com/page".into(),
            }
        );
    }

    #[test]
    fn https_is_left_alone() {
        assert_eq!(decide(nav("https://example.com/page")), Decision::Allow);
    }

    /// **The list that keeps this feature from being a bug.** Every one of
    /// these is a host where https is not merely unlikely but wrong, and
    /// upgrading it fails as a connection error — which reads as "eave is
    /// broken" rather than "that site is http".
    #[test]
    fn the_hosts_that_have_no_tls_are_never_upgraded() {
        for url in [
            "http://localhost/",
            "http://localhost:80/",
            "http://app.localhost/",
            "http://127.0.0.1/",
            "http://192.168.1.10/",
            "http://[::1]/",
            "http://printer.local/",
            "http://abcdefgh.onion/",
            "http://wiki/",
        ] {
            assert_eq!(decide(nav(url)), Decision::Allow, "upgraded {url}");
        }
    }

    /// A dev server. https on the same port is a different service or
    /// nothing at all.
    #[test]
    fn a_non_default_port_is_never_upgraded() {
        assert_eq!(decide(nav("http://example.com:8080/")), Decision::Allow);
        assert_eq!(
            decide(nav("http://example.com:80/")),
            Decision::Upgrade {
                to: "https://example.com:80/".into(),
                fallback: "http://example.com:80/".into(),
            },
            "port 80 is the default, and stated or not it is the same URL"
        );
    }

    /// A site that deliberately redirects https → http would otherwise be
    /// upgraded straight back, and the two would trade the navigation
    /// forever.
    #[test]
    fn a_redirect_target_is_never_upgraded() {
        assert_eq!(
            decide(Nav {
                is_redirect: true,
                ..nav("http://example.com/")
            }),
            Decision::Allow
        );
    }

    #[test]
    fn a_post_is_never_upgraded_because_that_is_a_second_submission() {
        assert_eq!(
            decide(Nav {
                method: "POST",
                ..nav("http://example.com/")
            }),
            Decision::Allow
        );
    }

    #[test]
    fn a_subframe_is_not_a_navigation_this_decides() {
        assert_eq!(
            decide(Nav {
                is_main_frame: false,
                ..nav("http://example.com/")
            }),
            Decision::Allow
        );
    }

    /// Stripping happens BEFORE upgrading, so the URL eave falls back to is
    /// the clean one. Falling back must not hand the trackers back.
    #[test]
    fn an_http_url_is_stripped_and_upgraded_in_one_step() {
        assert_eq!(
            decide(nav("http://example.com/?fbclid=1&foo=2")),
            Decision::Upgrade {
                to: "https://example.com/?foo=2".into(),
                fallback: "http://example.com/?foo=2".into(),
            }
        );
    }

    // ── E2: the fallback ──────────────────────────────────────────────

    /// **The trap this whole feature turns on.** eave performs an upgrade
    /// BY CANCELLING the http navigation, so `ERR_ABORTED` arrives on every
    /// single upgrade. A fallback that fired on it would undo every upgrade
    /// it ever made, instantly and invisibly — the feature would appear to
    /// do nothing at all.
    #[test]
    fn an_abort_never_falls_back() {
        note_upgrade("t-abort", "https://a.test/", "http://a.test/");
        assert_eq!(
            fallback("t-abort", "https://a.test/", Failure::Aborted),
            None
        );
        assert!(
            may_upgrade("a.test"),
            "an abort must not blacklist the host either"
        );
    }

    #[test]
    fn an_unreachable_upgrade_goes_back_to_http_and_stops_retrying() {
        note_upgrade("t-fail", "https://b.test/", "http://b.test/");
        assert!(may_upgrade("b.test"));
        assert_eq!(
            fallback("t-fail", "https://b.test/", Failure::Unreachable),
            Some("http://b.test/".to_string())
        );
        assert!(
            !may_upgrade("b.test"),
            "a host that has failed TLS once must not be retried on every load"
        );
        assert_eq!(
            decide(Nav {
                allow_upgrade: may_upgrade("b.test"),
                ..nav("http://b.test/")
            }),
            Decision::Allow,
            "and the decision must actually consult that memory"
        );
    }

    /// If the http attempt fails too there is no record left, so nothing
    /// loops.
    #[test]
    fn a_fallback_is_taken_once() {
        note_upgrade("t-once", "https://c.test/", "http://c.test/");
        assert!(fallback("t-once", "https://c.test/", Failure::Unreachable).is_some());
        assert_eq!(
            fallback("t-once", "http://c.test/", Failure::Unreachable),
            None,
            "the http failure must find no record to fall back to"
        );
    }

    /// A failure on some other URL is not an answer to the outstanding
    /// upgrade — a subresource or a stale navigation must not consume it.
    #[test]
    fn a_different_urls_failure_does_not_consume_the_upgrade() {
        note_upgrade("t-other", "https://d.test/", "http://d.test/");
        assert_eq!(
            fallback("t-other", "https://elsewhere.test/", Failure::Unreachable),
            None
        );
        assert_eq!(
            fallback("t-other", "https://d.test/", Failure::Unreachable),
            Some("http://d.test/".to_string()),
            "the real failure still answers it"
        );
    }

    #[test]
    fn closing_a_tab_forgets_its_upgrade() {
        note_upgrade("t-gone", "https://e.test/", "http://e.test/");
        forget("t-gone");
        assert_eq!(
            fallback("t-gone", "https://e.test/", Failure::Unreachable),
            None
        );
    }

    /// The documented approximation, asserted so it is a decision and not a
    /// surprise: without a public-suffix list these read as one site, so
    /// eave under-strips here. Brave, which has the PSL, would strip.
    #[test]
    fn multi_label_public_suffixes_are_treated_as_one_site_and_under_strip() {
        assert_eq!(
            decide(Nav {
                from: Some("https://b.co.uk/"),
                ..nav("https://a.co.uk/?fbclid=1")
            }),
            Decision::Allow,
            "documented under-strip: `same_site` has no public-suffix list"
        );
    }
}
