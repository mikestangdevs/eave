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
    /// A scheme eave will neither render nor hand to the OS.
    ///
    /// This is the same posture `ui::open_url` already takes for markdown
    /// links, for the same reason: `open(1)` launches whatever has
    /// registered for a scheme, and the string arrives from a web page.
    /// An allow-list is the security boundary; a deny-list would be a
    /// guess about every app installed on the machine.
    Block,
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
/// `from` is the URL the tab is currently on — the initiator, for a
/// top-level navigation. `None` means eave does not know, which happens for
/// the first load of a tab; an unknown initiator is treated as cross-site,
/// which is Brave's behaviour and the stripping one.
pub fn decide(url: &str, from: Option<&str>, method: &str, is_redirect: bool) -> Decision {
    let Some(scheme) = scheme(url) else {
        // No scheme at all. Not this module's problem to fix.
        return Decision::Allow;
    };
    if EXTERNAL_SCHEMES.contains(&scheme.as_str()) {
        return Decision::External(url.to_string());
    }
    if !WEB_SCHEMES.contains(&scheme.as_str()) {
        return Decision::Block;
    }

    // ── From here down it is only the query filter ────────────────────
    //
    // Every gate below is Brave's, and each one exists because removing it
    // breaks a real site rather than because it is tidy.

    // Only GET. A POST carries its payload separately and rewriting the
    // URL would re-issue it as a different request.
    if !method.eq_ignore_ascii_case("GET") {
        return Decision::Allow;
    }
    let (head, Some(query), frag) = parts(url) else {
        return Decision::Allow;
    };
    if query.is_empty() {
        return Decision::Allow;
    }
    let Some(host) = host(url) else {
        return Decision::Allow;
    };
    if EXEMPT_HOSTS.iter().any(|h| host_is(&host, h)) {
        return Decision::Allow;
    }
    // Same-site navigation: the parameters are the site's own business.
    // This is the gate that keeps analytics-driven site features working,
    // and dropping it is how a query filter earns a reputation for breaking
    // logins.
    if let Some(from) = from {
        if let Some(from_host) = self::host(from) {
            if same_site(&host, &from_host) {
                return Decision::Allow;
            }
        }
    }
    // A redirect eave did not cause is still a navigation somebody asked
    // for; a redirect within a site is the site's own plumbing.
    if is_redirect {
        if let Some(from) = from {
            if let Some(from_host) = self::host(from) {
                if same_site(&host, &from_host) {
                    return Decision::Allow;
                }
            }
        }
    }

    let Some(clean) = strip_query(query, url, &host) else {
        return Decision::Allow;
    };
    // Brave's last gate: only rewrite if the query actually got shorter.
    if clean.len() >= query.len() {
        return Decision::Allow;
    }
    let rebuilt = if clean.is_empty() {
        format!("{head}{frag}")
    } else {
        format!("{head}?{clean}{frag}")
    };
    Decision::Rewrite(rebuilt)
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

    /// Cross-site, so the filter is actually reached.
    const FROM: Option<&str> = Some("https://example.net/");

    fn filtered(url: &str) -> Decision {
        decide(url, FROM, "GET", false)
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
            decide("mailto:a@b.test", FROM, "GET", false),
            Decision::External("mailto:a@b.test".into())
        );
        assert_eq!(
            decide("tel:+15551234", FROM, "GET", false),
            Decision::External("tel:+15551234".into())
        );
    }

    /// The refusal is the half that matters for safety: `open(1)` launches
    /// whatever registered for a scheme, and this string came from a page.
    #[test]
    fn an_unknown_scheme_is_refused_rather_than_handed_to_the_os() {
        assert_eq!(
            decide("zoommtg://zoom.us/join", FROM, "GET", false),
            Decision::Block
        );
        assert_eq!(
            decide("javascript:alert(1)", FROM, "GET", false),
            Decision::Block
        );
    }

    #[test]
    fn ordinary_pages_are_allowed() {
        assert_eq!(
            decide("https://a.test/", FROM, "GET", false),
            Decision::Allow
        );
        assert_eq!(decide("about:blank", FROM, "GET", false), Decision::Allow);
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
                decide("https://example.com/?fbclid=1", Some(from), "GET", false),
                Decision::Allow,
                "stripped a same-site navigation from {from}"
            );
        }
    }

    #[test]
    fn only_get_is_filtered() {
        assert_eq!(
            decide("https://example.com/?fbclid=1", FROM, "POST", false),
            Decision::Allow
        );
    }

    #[test]
    fn a_same_site_redirect_is_exempt_and_a_cross_site_one_is_not() {
        assert_eq!(
            decide(
                "https://example.com/?fbclid=1",
                Some("https://sub.example.com/"),
                "GET",
                true
            ),
            Decision::Allow
        );
        assert_eq!(
            decide(
                "https://example.com/?fbclid=1",
                Some("https://other.test/"),
                "GET",
                true
            ),
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
            decide("https://example.com/?fbclid=1", None, "GET", false),
            Decision::Rewrite("https://example.com/".into())
        );
    }

    /// The documented approximation, asserted so it is a decision and not a
    /// surprise: without a public-suffix list these read as one site, so
    /// eave under-strips here. Brave, which has the PSL, would strip.
    #[test]
    fn multi_label_public_suffixes_are_treated_as_one_site_and_under_strip() {
        assert_eq!(
            decide(
                "https://a.co.uk/?fbclid=1",
                Some("https://b.co.uk/"),
                "GET",
                false
            ),
            Decision::Allow,
            "documented under-strip: `same_site` has no public-suffix list"
        );
    }
}
