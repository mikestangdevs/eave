//! Slice S2 — the Agents pane: Slice P2's settings surface.
//!
//! P2 made a session carry its own agent — binary, model, flags — and
//! left "a picker" open. The launcher (Q2/CK) became the *per-session*
//! picker; this pane is the other half: **defaults for new sessions**,
//! plus the facts about each provider an operator needs while deciding
//! them. Per-session override stays at session creation, and a running
//! session's agent cannot change (`Engine::reject_config_change`) — the
//! pane says so in as many words ([`DEFAULTS_SCOPE_SENTENCE`]).
//!
//! # The three stance rules
//!
//! 1. **A row exists only for a provider eave can actually drive.** Rows
//!    are read from [`eave_core::provider::ADAPTERS`] — the table the
//!    engine spawns from — so a row cannot claim what the engine would
//!    refuse. `cursor-agent` is catalogued but has no adapter, so it has
//!    no row *here*: the launcher is where "eave names it and cannot
//!    drive it" is already stated, and a defaults row for an agent that
//!    cannot start would be a control wired to nothing. Codex has an
//!    adapter and a deliberately unproven posture; its row states that
//!    refusal and offers no defaults.
//! 2. **Posture is a fact, never a setting.** Slice K's whole finding is
//!    that supplying ANY `--permission-mode` stops the CLI asking, so
//!    there is nothing here to choose: eave spawns with
//!    `--permission-prompt-tool stdio` and no mode, and each adapter's
//!    [`PostureProof`] names where that is verified per spawn. The pane
//!    displays that declaration verbatim — the same registry row the
//!    engine's gate and the launcher tile read — rather than a sentence
//!    of its own that could drift from them.
//! 3. **No OAuth buttons, no API-key storage.** Auth belongs to each
//!    CLI. For Claude Code the pane reads the installed CLI's own answer
//!    (`claude auth status`, JSON) and, when signed out, names the
//!    command to run ([`CLAUDE_SIGN_IN_COMMAND`]); eave stores no
//!    credential from here, ever. For the other providers eave has
//!    verified no auth-status surface, so the pane states nothing about
//!    their auth — never a guess (`capabilities.rs`'s house rule).
//!
//! # Probing is a spawn, so it obeys the spawn rules
//!
//! The status probe runs a vendor CLI, which makes it subject to Slice
//! CK's discipline and not a hole around it: only for a provider
//! [`eave_core::session::posture_is_verified`] accepts, only in an
//! [`eave_core::capabilities::is_armed`] process (so `cargo test` never
//! spawns a vendor CLI because a render happened), fixed `&'static`
//! argv, no turn ever starts, bounded by
//! [`eave_core::capabilities::PROBE_TIMEOUT`] and killed. Every read
//! path answers from memory immediately; the pane re-probes on open and
//! on demand ([`refresh_status`]) so what it shows is the CLI's actual
//! state, freshly read, never a cached guess — and while a probe is out
//! the last answer stays on screen *labelled as being re-checked*
//! ([`StatusView::Probing`]).

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use gpui::{div, prelude::*, px, AnyElement, Context, SharedString};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use bezel::theme::Theme;

use eave_core::capabilities::{is_armed, PROBE_TIMEOUT};
use eave_core::provider::{self, Availability, PostureProof};
use eave_core::session::conflicts_with_eave_flags;

use crate::launcher;
use crate::settings_window::{
    caption, card, input_box, row, section_heading, segmented_with_ids, SettingsWindow,
};
use crate::text_field::TextField;
use crate::theme;

// ---------------------------------------------------------------------------
// Defaults for new sessions

/// What a NEW session starts with when its creator does not say —
/// per provider, because a model alias belongs to one CLI's vocabulary
/// and a flag to one CLI's syntax (the launcher's `pick_provider` clears
/// the model for the same reason).
///
/// Lives inside S1's `Settings` as one serde-defaulted field, so it
/// persists in `<data_dir>/settings.json` with everything else and an
/// old file reads back as "no defaults" — the same compatibility story
/// as P2's journal header: absent means absent, and a struct holding no
/// defaults serializes to nothing at all
/// ([`tests::no_defaults_serializes_to_nothing_at_all`]).
///
/// Both levels carry the same `#[serde(flatten)]` tail `Settings` itself
/// carries, for the same load-bearing reason: several checkouts share
/// one data dir, so keys THIS build does not know — a newer build's
/// agents settings — must survive a rewrite by it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentDefaults {
    /// Keyed by catalog id (`"claude-code"`, …). An entry with nothing
    /// in it is pruned on write rather than serialized as `{}` noise.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderDefaults>,
    /// Keys this build does not know, preserved across a rewrite.
    /// **Keep this field last**; typed fields added later go above it.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// One provider's defaults. Every field optional; absent means "the
/// CLI's own default", exactly as on [`eave_core::session::AgentConfig`]
/// — these are the same two facts, staged one step earlier.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDefaults {
    /// Passed verbatim as `--model` (P2 caveat 5: eave does not validate
    /// it — an unknown model is the CLI's error to state). `None` sends
    /// no `--model` flag at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra CLI flags for new sessions, verbatim and in order. Already
    /// refused at entry if any would undo eave's posture
    /// ([`parse_flags`]); the engine refuses them again at open
    /// (fail-closed twice, on purpose).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Keys this build does not know, preserved across a rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl ProviderDefaults {
    /// Empty means safe to prune: no defaults AND no foreign keys —
    /// pruning an entry that still carries a newer build's keys would
    /// be exactly the silent strip the flatten tail exists to prevent.
    fn is_empty(&self) -> bool {
        self.model.is_none() && self.extra_args.is_empty() && self.extra.is_empty()
    }
}

impl AgentDefaults {
    /// No defaults and no foreign keys — the state `Settings` skips on
    /// write, so a file that never set a default is byte-identical to a
    /// pre-S2 one.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.extra.is_empty()
    }

    /// The model a new session of this provider starts with, unless its
    /// creator picks otherwise. **Consumed by pre-selecting the
    /// launcher's picker, never by filling in behind it** — a default
    /// applied invisibly at launch would resurrect itself after an
    /// operator explicitly chose "CLI default", and a default you cannot
    /// see is one you cannot override.
    pub fn model_for(&self, provider_id: &str) -> Option<&str> {
        self.providers.get(provider_id)?.model.as_deref()
    }

    /// The flags a new session of this provider is launched with. These
    /// have no per-session picker (yet), so unlike the model they ARE
    /// applied at launch — the pane is the one place they are stated.
    pub fn extra_args_for(&self, provider_id: &str) -> &[String] {
        self.providers
            .get(provider_id)
            .map(|d| d.extra_args.as_slice())
            .unwrap_or(&[])
    }

    pub fn set_model(&mut self, provider_id: &str, model: Option<String>) {
        self.providers
            .entry(provider_id.to_string())
            .or_default()
            .model = model;
        self.prune(provider_id);
    }

    pub fn set_extra_args(&mut self, provider_id: &str, extra_args: Vec<String>) {
        self.providers
            .entry(provider_id.to_string())
            .or_default()
            .extra_args = extra_args;
        self.prune(provider_id);
    }

    /// Clearing the last field of an entry removes the entry, so a file
    /// whose defaults were all taken back is byte-identical to one that
    /// never had any.
    fn prune(&mut self, provider_id: &str) {
        if self
            .providers
            .get(provider_id)
            .is_some_and(ProviderDefaults::is_empty)
        {
            self.providers.remove(provider_id);
        }
    }
}

/// The scope statement the pane renders, verbatim. A defaults surface
/// that does not say when defaults apply invites the operator to expect
/// a running session to change under it.
pub const DEFAULTS_SCOPE_SENTENCE: &str = "Defaults apply to new sessions only. Each session \
     still picks its own agent at creation, and a running session's agent cannot change.";

/// Intern a model id as `&'static str`, so a stored default can sit in
/// `LauncherUi::model` (which is `Option<&'static str>` because every
/// other id it holds comes from a `const` table or the capabilities
/// cache). Leaking is the same deliberate choice `capabilities::record`
/// makes and for the same reason — these are a handful of short ids
/// with process lifetime — and the set keeps re-saving the same model
/// from leaking it again.
pub fn intern_model(model: &str) -> &'static str {
    static INTERNED: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let mut set = INTERNED
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = set.iter().find(|s| **s == model) {
        return hit;
    }
    let leaked: &'static str = Box::leak(model.to_string().into_boxed_str());
    set.push(leaked);
    leaked
}

// ---------------------------------------------------------------------------
// Flags: entry-time refusal

/// Parse the pane's flags field into the argv-shaped list P2 carries.
///
/// **Whitespace split, no shell quoting** — the engine passes
/// `extra_args` verbatim to `exec`, not through a shell, so quoting
/// would be a syntax this field pretends to have and does not. A flag
/// value with a space in it cannot be expressed here; that is stated
/// rather than half-supported.
///
/// A flag that would undo the permission posture is refused at entry
/// with the reason, mirroring the engine's own per-session refusal
/// (`engine.rs`: "agent flag … may not be set per session — it would
/// override the permission posture") so the two surfaces tell one
/// story. The engine still refuses it again at open — this check is a
/// courtesy, not the gate.
pub fn parse_flags(input: &str) -> Result<Vec<String>, String> {
    let args: Vec<String> = input.split_whitespace().map(str::to_string).collect();
    if let Some(bad) = args.iter().find(|a| conflicts_with_eave_flags(a)) {
        return Err(format!(
            "flag {bad:?} may not be a default — it would override the permission posture eave \
             verifies on every spawn"
        ));
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// The rows

/// One provider's row in the pane: identity from the launcher's strip
/// (same name, same order), capability facts from the registry the
/// engine spawns from. Both sources already hold each other equal in
/// their own tests; this struct adds no third opinion.
pub struct AgentRow {
    pub id: &'static str,
    pub name: &'static str,
    pub availability: Availability,
    pub posture: PostureProof,
}

impl AgentRow {
    /// Are defaults offered for this row? Only for a provider the
    /// engine would actually spawn — a default model for an agent eave
    /// refuses to run is a control wired to nothing.
    pub fn defaults_editable(&self) -> bool {
        self.posture.is_verified()
    }
}

/// The pane's provider list: **exactly the registered adapters**, in
/// strip order. Catalogued-but-adapterless providers (`cursor-agent`)
/// are deliberately absent — see stance rule 1 in the module doc.
pub fn agent_rows() -> Vec<AgentRow> {
    launcher::PROVIDERS
        .iter()
        .filter_map(|p| {
            provider::adapter(p.id).map(|a| AgentRow {
                id: p.id,
                name: p.name,
                availability: provider::availability(p.id),
                posture: a.posture,
            })
        })
        .collect()
}

/// The posture fact for one row, from the adapter's own declaration —
/// the same [`PostureProof`] the engine's gate reads, so the pane
/// cannot state a posture the engine would not enforce.
///
/// The headline for a verified posture is the rail's own opening words
/// (`ui.rs::posture_line`: "asks before it writes"), so the pane and
/// the rail describe the same fact in the same voice.
pub fn posture_fact(provider_id: &str, posture: &PostureProof) -> String {
    match posture {
        PostureProof::Verified { evidence } => {
            format!("asks before it writes — verified on every spawn ({evidence})")
        }
        PostureProof::Unproven { reason } => {
            format!("eave will not run {provider_id} unsupervised: {reason}")
        }
    }
}

// ---------------------------------------------------------------------------
// CLI status: installed, which version, signed in as whom

/// What the installed CLI itself answered, when it answered. Either
/// half can be individually absent — a CLI whose `--version` printed
/// something unreadable can still state its auth, and vice versa —
/// and absent is rendered as "could not read", never filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliStatus {
    /// First token of `--version`, e.g. `"2.1.226"`.
    pub version: Option<String>,
    /// Parsed from `auth status`. `None` = the CLI did not answer in a
    /// shape eave could read.
    pub auth: Option<AuthStatus>,
}

/// The CLI's own answer to "who am I signed in as". Fields are the
/// CLI's words verbatim (`authMethod`, `subscriptionType`) — eave does
/// not translate a vendor's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    SignedIn {
        method: Option<String>,
        subscription: Option<String>,
    },
    SignedOut,
}

/// What the pane may render about a provider's CLI right now. Every
/// state says who said so: `Reported` is the CLI's answer, `Probing`
/// is last answer plus the admission it is being re-checked, `Silent`
/// is "asked, no usable reply", and `NotProbed` is "nobody asked yet".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusView {
    NotProbed,
    Probing { last: Option<CliStatus> },
    Reported(CliStatus),
    Silent,
}

/// The command the pane names when Claude Code is signed out. Verified
/// against the installed CLI 2026-08-25: `claude auth` is the command
/// group (`login` / `logout` / `status`); a bare `claude login` is not
/// a subcommand.
pub const CLAUDE_SIGN_IN_COMMAND: &str = "claude auth login";

/// The auth line for Claude Code's row. Stated, never guessed: an
/// unreadable answer says so and names the command an operator can run
/// to see for themselves.
pub fn auth_sentence(auth: Option<&AuthStatus>) -> String {
    match auth {
        Some(AuthStatus::SignedIn {
            method: Some(m),
            subscription: Some(s),
        }) => format!("signed in via {m} ({s})"),
        Some(AuthStatus::SignedIn {
            method: Some(m), ..
        }) => format!("signed in via {m}"),
        Some(AuthStatus::SignedIn { .. }) => "signed in".to_string(),
        Some(AuthStatus::SignedOut) => format!(
            "not signed in — run `{CLAUDE_SIGN_IN_COMMAND}` in a terminal. eave stores no \
             credentials; auth belongs to the CLI."
        ),
        None => "could not read auth status — run `claude auth status` in a terminal to see \
                 the CLI's own answer."
            .to_string(),
    }
}

/// How each provider's CLI is asked about itself. **One row: Claude
/// Code.** The other adapters have no verified status surface — grok,
/// hermes and pi state auth only inside interactive flows eave has not
/// captured — so eave asks them nothing and states nothing
/// ([`status_askable`] is how the pane knows to stay silent).
struct StatusAsk {
    version_args: &'static [&'static str],
    auth_args: &'static [&'static str],
}

/// `claude --version` → `2.1.226 (Claude Code)`; `claude auth status` →
/// a JSON object with `loggedIn` / `authMethod` / `subscriptionType`.
/// Both verified live 2026-08-25 against claude 2.1.226 on this
/// machine, each answering in under a second. Neither starts a turn;
/// neither carries a flag [`conflicts_with_eave_flags`] refuses
/// ([`tests::no_status_probe_undoes_eaves_posture`]).
const STATUS_ASKS: &[(&str, StatusAsk)] = &[(
    "claude-code",
    StatusAsk {
        version_args: &["--version"],
        auth_args: &["auth", "status"],
    },
)];

/// Does this provider have a status surface eave has verified? The
/// pane renders no auth line at all for one that does not.
pub fn status_askable(provider_id: &str) -> bool {
    STATUS_ASKS.iter().any(|(id, _)| *id == provider_id)
}

enum Entry {
    InFlight {
        last: Option<CliStatus>,
    },
    /// `status: None` = the CLI was asked and nothing usable came back.
    /// `at` is when the answer landed, for the staleness re-ask and the
    /// "checked Ns ago" caption.
    Landed {
        status: Option<CliStatus>,
        at: Instant,
    },
}

static STATUS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();

fn status_map() -> &'static Mutex<HashMap<String, Entry>> {
    STATUS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What the pane may say about this provider's CLI. **Never blocks and
/// never spawns** — a map lookup, exactly like `capabilities::reported`.
pub fn status(provider_id: &str) -> StatusView {
    let Ok(map) = status_map().lock() else {
        return StatusView::NotProbed;
    };
    match map.get(provider_id) {
        None => StatusView::NotProbed,
        Some(Entry::InFlight { last }) => StatusView::Probing { last: last.clone() },
        Some(Entry::Landed {
            status: Some(s), ..
        }) => StatusView::Reported(s.clone()),
        Some(Entry::Landed { status: None, .. }) => StatusView::Silent,
    }
}

/// How long ago the current answer landed. `None` while nothing has.
pub fn status_age(provider_id: &str) -> Option<std::time::Duration> {
    match status_map().lock().ok()?.get(provider_id)? {
        Entry::Landed { at, .. } => Some(at.elapsed()),
        Entry::InFlight { .. } => None,
    }
}

/// How old a landed answer may get while the pane is on screen before it
/// is re-asked on its own. **A dial position, not a derivation**: long
/// enough that the two ~1 s child processes are a negligible duty cycle
/// with the pane parked open, short enough that a `claude auth login`
/// finished in a terminal shows up here without anyone clicking. The
/// pane's check-again affordance and a window reopen both force a fresh
/// ask, so nothing rides on this number being right.
const STATUS_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Re-ask if the landed answer is older than [`STATUS_RECHECK`]. Called
/// from the pane's render, so it only runs while the facts are actually
/// on screen; a no-op in every other state.
pub fn refresh_if_stale(provider_id: &str) {
    if status_age(provider_id).is_some_and(|age| age > STATUS_RECHECK) {
        refresh_status(provider_id);
    }
}

/// Ask this provider's CLI about itself if nobody has yet, without
/// waiting for the answer. Idempotent per process; the pane calls it
/// from its render the way the launcher calls `capabilities::begin`.
pub fn begin_status(provider_id: &str) {
    let already = status_map()
        .lock()
        .map(|m| m.contains_key(provider_id))
        .unwrap_or(true);
    if !already {
        force_probe(provider_id);
    }
}

/// Ask again, keeping the last answer on screen labelled as being
/// re-checked. The pane calls this when it opens — "read the CLI's
/// actual state" means the state *now*, not the state at first render —
/// and from its refresh affordance. A no-op while a probe is out.
pub fn refresh_status(provider_id: &str) {
    let stale = {
        let Ok(map) = status_map().lock() else {
            return;
        };
        match map.get(provider_id) {
            Some(Entry::InFlight { .. }) => return,
            Some(Entry::Landed { status, .. }) => status.clone(),
            None => None,
        }
    };
    force_probe_keeping(provider_id, stale);
}

fn force_probe(provider_id: &str) {
    force_probe_keeping(provider_id, None);
}

/// The gates, in the same order `capabilities::begin` applies them: an
/// unarmed process never spawns; a provider with no ask row is never
/// asked; a provider eave refuses to drive is not probed either (a
/// status spawn of a refused CLI is that spawn with a friendlier name);
/// an absent binary cannot be run. Failing any gate leaves the map
/// untouched, so the view stays `NotProbed` rather than pretending.
fn force_probe_keeping(provider_id: &str, last: Option<CliStatus>) {
    if !is_armed() {
        return;
    }
    let Some((id, ask)) = STATUS_ASKS.iter().find(|(id, _)| *id == provider_id) else {
        return;
    };
    if !eave_core::session::posture_is_verified(id) {
        return;
    }
    let Some(entry) = provider::catalog(id) else {
        return;
    };
    let Some(program) = provider::resolve_program(entry).path().map(Path::to_path_buf) else {
        return;
    };
    let id = id.to_string();
    if let Ok(mut map) = status_map().lock() {
        map.insert(id.clone(), Entry::InFlight { last });
    }
    std::thread::spawn(move || {
        let landed = run_status_probe(&program, ask);
        if let Ok(mut map) = status_map().lock() {
            map.insert(
                id,
                Entry::Landed {
                    status: landed,
                    at: Instant::now(),
                },
            );
        }
    });
}

/// Record an answer, or (`None`) a silence, against a provider. The
/// probe thread's writer and the seam every test drives instead of a
/// spawn — same shape as `capabilities::record`.
pub fn record_status(provider_id: &str, status: Option<CliStatus>) {
    if let Ok(mut map) = status_map().lock() {
        map.insert(
            provider_id.to_string(),
            Entry::Landed {
                status,
                at: Instant::now(),
            },
        );
    }
}

/// Drop everything learned, so the next `begin_status` asks again.
pub fn forget_status() {
    if let Ok(mut map) = status_map().lock() {
        map.clear();
    }
}

/// Serialise tests that touch the status cache — it is process-global
/// and `cargo test` shares one process across threads. Same rule and
/// same shape as `capabilities::probe_test_lock`. **Test-only.**
#[doc(hidden)]
pub fn status_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static TEST_TURN: Mutex<()> = Mutex::new(());
    TEST_TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn run_status_probe(program: &PathBuf, ask: &StatusAsk) -> Option<CliStatus> {
    let version = run_bounded(program, ask.version_args)
        .as_deref()
        .and_then(parse_version);
    let auth = run_bounded(program, ask.auth_args)
        .as_deref()
        .and_then(parse_auth_status);
    if version.is_none() && auth.is_none() {
        return None;
    }
    Some(CliStatus { version, auth })
}

/// Run one bounded CLI invocation and return stdout + stderr joined —
/// the same bounded wait `capabilities::wait_bounded` uses, restated
/// here because that helper is private to its crate. The exit status is
/// deliberately ignored: a CLI may well exit non-zero while printing
/// exactly the answer (a signed-out `auth status` is still an answer).
fn run_bounded(program: &Path, args: &[&str]) -> Option<String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    wait_bounded(child)
}

fn wait_bounded(mut child: Child) -> Option<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        out.push('\n');
        let _ = s.read_to_string(&mut out);
    }
    Some(out)
}

/// First token of the first non-empty line, if it looks like a version.
/// `claude --version` prints `2.1.226 (Claude Code)`; the parenthetical
/// is the CLI naming itself and is not part of the version.
pub fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let token = line.trim().split_whitespace().next()?;
    let looks_versionish = token.starts_with(|c: char| c.is_ascii_digit())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    looks_versionish.then(|| token.to_string())
}

/// `claude auth status` prints one JSON object (verified live
/// 2026-08-25, claude 2.1.226: `loggedIn`, `authMethod`, `apiProvider`,
/// `email`, `orgId`, `orgName`, `subscriptionType`). eave reads the
/// three fields it renders and drops the rest — the email and org are
/// the operator's business, not the pane's.
///
/// **The signed-out shape is untested against reality**: producing it
/// would mean signing this machine's operator out. eave reads
/// `loggedIn: false` as signed out and anything unparseable as "could
/// not read", which fails toward saying less, not more.
pub fn parse_auth_status(out: &str) -> Option<AuthStatus> {
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    match v.get("loggedIn").and_then(Value::as_bool)? {
        false => Some(AuthStatus::SignedOut),
        true => Some(AuthStatus::SignedIn {
            method: v
                .get("authMethod")
                .and_then(Value::as_str)
                .map(str::to_string),
            subscription: v
                .get("subscriptionType")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
    }
}

// ---------------------------------------------------------------------------
// Where defaults live in settings.json

/// The `Settings` field these serialize under — named here because two
/// tests assert the FILE carries no such key once every default is
/// taken back ([`AgentDefaults::is_empty`] + `skip_serializing_if` on
/// the field is what makes that true).
///
/// This began life as an `extra`-flatten key while S1's `Settings` was
/// still landing (their suggestion); the typed field writes the same
/// bytes under the same name, so files from that interim read back
/// unchanged.
pub const SETTINGS_KEY: &str = "agents";

/// The defaults carried by one `Settings` value. The seam the round-trip
/// test reads a re-parsed settings.json through, so the test and the
/// pane decode the same way by construction.
pub fn defaults_in(s: &crate::settings::Settings) -> AgentDefaults {
    s.agents.clone()
}

/// The defaults as persisted right now.
pub fn current_defaults() -> AgentDefaults {
    defaults_in(&crate::settings::snapshot())
}

/// Change the defaults, through `settings::update` — the one writer.
/// `pub` for the round-trip test; the pane itself is the real caller.
pub fn write_defaults(cx: &mut gpui::App, f: impl FnOnce(&mut AgentDefaults)) {
    let mut d = current_defaults();
    f(&mut d);
    crate::settings::update(cx, move |s| s.agents = d);
}

/// The stored default model for the launcher to PRE-SELECT (visible,
/// overridable — never filled in behind the picker; see
/// [`AgentDefaults::model_for`]). Interned because `LauncherUi::model`
/// is `Option<&'static str>`.
pub fn preselected_model(provider_id: &str) -> Option<&'static str> {
    let d = current_defaults();
    d.model_for(provider_id).map(intern_model)
}

// ---------------------------------------------------------------------------
// The pane

/// Per-pane UI state that must outlive a render: the flags fields (one
/// `TextField` entity per editable provider) and the current entry-time
/// refusal per field. A global of this pane's own, because
/// `SettingsWindow`'s struct is S1's (declared file ownership).
#[derive(Default)]
pub struct AgentsPaneState {
    flags: HashMap<&'static str, gpui::Entity<TextField>>,
    flag_errors: HashMap<&'static str, String>,
}

impl gpui::Global for AgentsPaneState {}

/// The Agents pane. Called from `SettingsWindow`'s pane match; returns
/// headings and cards for the scroll body, in the window's grammar.
pub fn render(theme: &Theme, cx: &mut Context<SettingsWindow>) -> Vec<AnyElement> {
    if cx.try_global::<AgentsPaneState>().is_none() {
        cx.set_global(AgentsPaneState::default());
    }
    let rows = agent_rows();
    ensure_flags_fields(&rows, cx);

    let mut out: Vec<AnyElement> = Vec::new();
    out.push(section_heading(theme, "Agents").into_any_element());
    out.push(
        card(theme, vec![caption(theme, DEFAULTS_SCOPE_SENTENCE)]).into_any_element(),
    );
    for agent in rows {
        out.push(provider_heading(theme, &agent));
        out.push(provider_card(theme, &agent, cx));
    }
    out
}

/// The section heading for one provider — its name, `section_heading`'s
/// styling. A separate fn because the heading takes `&'static str` and a
/// provider's name already is one.
fn provider_heading(theme: &Theme, agent: &AgentRow) -> AnyElement {
    section_heading(theme, agent.name).into_any_element()
}

fn provider_card(theme: &Theme, agent: &AgentRow, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let id = agent.id;
    let mut rows: Vec<AnyElement> = Vec::new();

    // Defaults lead — they are what an operator comes here to set; the
    // facts below support the choice. (Also load-bearing for the
    // harness: the scroll body culls what sits below the fold, and a
    // control the tests cannot reach is a control that can quietly
    // break.)
    if agent.defaults_editable() {
        rows.extend(defaults_rows(theme, id, cx));
    }

    // The posture fact — the registry's own declaration, verbatim.
    rows.push(caption(
        theme,
        SharedString::from(posture_fact(id, &agent.posture)),
    ));

    // The machine fact: can this start here, and if not, which not.
    rows.push(row(
        theme,
        "Status",
        muted_value(theme, agent.availability.tile_status()),
    ));
    if let Some(why) = agent.availability.why_not(id) {
        rows.push(caption(theme, SharedString::from(why)));
    }

    // The CLI's own state, live — only where a status surface exists and
    // the binary does (an absent CLI's story is the status row above).
    if status_askable(id) && agent.availability.is_ready() {
        begin_status(id);
        refresh_if_stale(id);
        rows.extend(cli_status_rows(theme, id, &agent.availability, cx));
    }

    card(theme, rows).into_any_element()
}

/// The version and account rows, from the probe cache, plus the age and
/// the check-again affordance. While a probe is out the previous answer
/// stays up, labelled; a repaint is scheduled so the landing shows
/// without user input (the probe thread has no `cx` to notify with).
fn cli_status_rows(
    theme: &Theme,
    id: &'static str,
    availability: &Availability,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let mut rows: Vec<AnyElement> = Vec::new();
    let view = status(id);
    if matches!(view, StatusView::Probing { .. }) {
        schedule_repaint(cx);
    }
    let (cli_line, auth_line, footnote) = match &view {
        StatusView::NotProbed => (
            // Only reachable where probes cannot arm (the harness): the
            // gate is stated rather than dressed up as an answer.
            "not checked — probes are off in this process".to_string(),
            None,
            None,
        ),
        StatusView::Probing { last: None } => ("checking…".to_string(), None, None),
        StatusView::Probing { last: Some(s) } => (
            version_line(s, availability) + " · re-checking…",
            Some(auth_sentence(s.auth.as_ref())),
            None,
        ),
        StatusView::Reported(s) => (
            version_line(s, availability),
            Some(auth_sentence(s.auth.as_ref())),
            Some(checked_ago(status_age(id))),
        ),
        StatusView::Silent => (
            "the CLI did not answer `--version` / `auth status`".to_string(),
            None,
            Some(checked_ago(status_age(id))),
        ),
    };
    rows.push(row(
        theme,
        "CLI",
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(muted_mono(theme, cli_line))
            .child(check_again_chip(theme, id, cx))
            .into_any_element(),
    ));
    if let Some(auth) = auth_line {
        rows.push(row(theme, "Account", muted_value(theme, auth)));
    }
    if let Some(note) = footnote {
        rows.push(caption(theme, SharedString::from(note)));
    }
    rows
}

fn version_line(s: &CliStatus, availability: &Availability) -> String {
    let version = s.version.as_deref().unwrap_or("version unreadable");
    match availability {
        Availability::Ready { program } => format!("{version} · {}", program.display()),
        _ => version.to_string(),
    }
}

fn checked_ago(age: Option<std::time::Duration>) -> String {
    match age.map(|a| a.as_secs()) {
        Some(0..=4) => "checked just now".to_string(),
        Some(s @ 5..=119) => format!("checked {s}s ago"),
        Some(s) => format!("checked {}m ago", s / 60),
        None => "checking…".to_string(),
    }
}

fn check_again_chip(
    theme: &Theme,
    id: &'static str,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("agents-recheck-{id}")))
        .debug_selector(|| format!("agents-recheck-{id}"))
        .cursor_pointer()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(theme::RADIUS_CHIP))
        .border_1()
        .border_color(theme.border)
        .text_size(px(11.0))
        .text_color(theme.text_muted)
        .child("check again")
        .on_click(cx.listener(move |_, _, _, cx| {
            refresh_status(id);
            cx.notify();
        }))
        .into_any_element()
}

/// While a probe is out, poke the window again shortly so its landing
/// paints. Self-limiting: each repaint schedules the next only while the
/// state is still `Probing`, and probes are bounded.
fn schedule_repaint(cx: &mut Context<SettingsWindow>) {
    cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(300))
            .await;
        let _ = this.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

/// The default-model chips and the default-flags field for one editable
/// provider.
fn defaults_rows(
    theme: &Theme,
    id: &'static str,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let mut rows: Vec<AnyElement> = Vec::new();
    let Some(provider) = launcher::PROVIDERS.iter().find(|p| p.id == id) else {
        return rows;
    };
    let defaults = current_defaults();
    let current = defaults.model_for(id).map(str::to_string);

    let choices = launcher::model_choices(provider);
    // `None` is "send no --model at all" (`MODEL_DEFAULT`'s meaning);
    // the interned current keeps `T: Copy` for S1's segmented control,
    // which draws this exactly as the window's other choices are drawn.
    let items: Vec<(Option<&'static str>, &'static str)> = choices
        .iter()
        .map(|m| ((*m != launcher::MODEL_DEFAULT).then_some(*m), *m))
        .collect();
    let current_static: Option<&'static str> = current.as_deref().map(intern_model);
    rows.push(row(
        theme,
        "Default model",
        segmented_with_ids(
            theme,
            &format!("agents-model-{id}"),
            current_static,
            &items,
            move |v: Option<&'static str>, cx| {
                write_defaults(cx, |d| d.set_model(id, v.map(str::to_string)))
            },
            cx,
        ),
    ));
    // A stored default the picker does not offer (hand-edited file, or
    // the CLI's list changed): stated, still honoured verbatim at spawn.
    if let Some(c) = &current {
        if !choices.contains(&c.as_str()) {
            rows.push(caption(
                theme,
                SharedString::from(format!(
                    "Current default {c:?} is not in this list — it is still passed verbatim; \
                     an unknown model is the CLI's error to state."
                )),
            ));
        }
    }
    if let Some(note) = launcher::model_source_note(provider) {
        rows.push(caption(theme, SharedString::from(note)));
    }
    rows.push(caption(
        theme,
        "The launcher opens on this choice; each session can still pick otherwise.",
    ));

    if let Some(field) = cx.global::<AgentsPaneState>().flags.get(id).cloned() {
        rows.push(row(theme, "Default flags", input_box(theme, field)));
    }
    if let Some(err) = cx.global::<AgentsPaneState>().flag_errors.get(id).cloned() {
        rows.push(caption(theme, SharedString::from(format!(
            "{err} — the last accepted flags still stand."
        ))));
    } else {
        let args = defaults.extra_args_for(id);
        rows.push(caption(
            theme,
            match args.is_empty() {
                true => SharedString::from(
                    "Appended to new sessions of this agent, verbatim, after eave's own flags. \
                     Split on whitespace — no shell quoting.",
                ),
                false => SharedString::from(format!(
                    "New sessions launch with: {}",
                    args.join(" ")
                )),
            },
        ));
    }
    rows
}

/// Make sure every editable provider has its flags `TextField`, created
/// once and observed so an edit commits (or is refused) per keystroke —
/// the same write-on-edit contract S1's custom search prefix keeps, and
/// for the same reason: a field that looks set and is not is the quiet
/// failure the window exists to remove.
fn ensure_flags_fields(rows: &[AgentRow], cx: &mut Context<SettingsWindow>) {
    for agent in rows.iter().filter(|r| r.defaults_editable()) {
        let id = agent.id;
        if cx.global::<AgentsPaneState>().flags.contains_key(id) {
            continue;
        }
        let field = cx.new(|cx| {
            let mut f = TextField::new(cx)
                .with_placeholder("--flag value")
                .with_debug_name(format!("agents-flags-{id}"));
            let stored = current_defaults().extra_args_for(id).join(" ");
            if !stored.is_empty() {
                f.set_content(stored, cx);
            }
            f
        });
        cx.observe(&field, move |_, field, cx| {
            let typed = field.read(cx).content().to_string();
            commit_flags(id, &typed, cx);
            cx.notify();
        })
        .detach();
        cx.global_mut::<AgentsPaneState>().flags.insert(id, field);
    }
}

/// The flags field for one provider, if the pane has rendered it — the
/// harness's way to drive a real edit through the real observer without
/// reaching into this module's state. **Test seam.**
#[doc(hidden)]
pub fn flags_field(cx: &gpui::App, id: &str) -> Option<gpui::Entity<TextField>> {
    cx.try_global::<AgentsPaneState>()?.flags.get(id).cloned()
}

/// Commit one flags edit: parse, refuse posture-defeating flags at entry
/// (the refusal is rendered, and the last accepted flags stand), write
/// only on change.
fn commit_flags(id: &'static str, typed: &str, cx: &mut gpui::App) {
    match parse_flags(typed) {
        Ok(args) => {
            cx.global_mut::<AgentsPaneState>().flag_errors.remove(id);
            if current_defaults().extra_args_for(id) != args.as_slice() {
                write_defaults(cx, |d| d.set_extra_args(id, args));
            }
        }
        Err(e) => {
            cx.global_mut::<AgentsPaneState>().flag_errors.insert(id, e);
        }
    }
}

fn muted_value(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(12.5))
        .text_color(theme.text_muted)
        .child(text.into())
        .into_any_element()
}

fn muted_mono(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(12.0))
        .font_family(theme.font_mono.clone())
        .text_color(theme.text_muted)
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- defaults ----------------------------------------------------------

    /// The whole compatibility story, same as P2's journal header: no
    /// defaults means no bytes, so S1's `Settings` serializes exactly as
    /// it did before this field existed, and taking every default back
    /// returns the file to that state.
    #[test]
    fn no_defaults_serializes_to_nothing_at_all() {
        let empty = AgentDefaults::default();
        assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");

        let mut d = AgentDefaults::default();
        d.set_model("claude-code", Some("sonnet".into()));
        d.set_extra_args("claude-code", vec!["--verbose".into()]);
        assert_ne!(serde_json::to_string(&d).unwrap(), "{}");

        d.set_model("claude-code", None);
        d.set_extra_args("claude-code", vec![]);
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            "{}",
            "an entry whose defaults were all taken back must not survive as noise"
        );
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let mut d = AgentDefaults::default();
        d.set_model("claude-code", Some("claude-fable-5[1m]".into()));
        d.set_extra_args("claude-code", vec!["--effort".into(), "high".into()]);
        d.set_model("grok", Some("grok-4.6".into()));
        let json = serde_json::to_string(&d).unwrap();
        let back: AgentDefaults = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.model_for("claude-code"), Some("claude-fable-5[1m]"));
        assert_eq!(back.extra_args_for("claude-code"), ["--effort", "high"]);
        assert_eq!(back.model_for("grok"), Some("grok-4.6"));
        assert_eq!(back.model_for("pi"), None);
        assert!(back.extra_args_for("pi").is_empty());
    }

    /// A pre-S2 settings file — or one written by a build that never
    /// heard of agents — reads as "no defaults", not as an error.
    #[test]
    fn an_absent_field_reads_as_no_defaults() {
        let d: AgentDefaults = serde_json::from_str("{}").unwrap();
        assert_eq!(d, AgentDefaults::default());
    }

    /// The data-dir-sharing contract, held at every level: keys a NEWER
    /// build wrote under `agents` survive this build's rewrite (the
    /// flatten tails), and pruning never eats an entry that still
    /// carries a foreign key.
    #[test]
    fn a_newer_builds_agents_keys_survive_this_builds_rewrite() {
        let raw = r#"{"providers":{"claude-code":{"model":"opus","future_key":true}},"future_top":1}"#;
        let d: AgentDefaults = serde_json::from_str(raw).unwrap();
        assert_eq!(d.model_for("claude-code"), Some("opus"));
        let out = serde_json::to_string(&d).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["providers"]["claude-code"]["future_key"], true);
        assert_eq!(v["future_top"], 1);

        let mut d = d;
        d.set_model("claude-code", None);
        let v: Value = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(
            v["providers"]["claude-code"]["future_key"], true,
            "clearing this build's defaults must not prune away a newer build's keys"
        );
    }

    /// P2 caveat 5 carried forward: the model is verbatim, not
    /// validated. A default the CLI has never heard of is the CLI's
    /// error to state at spawn, not eave's to second-guess at entry.
    #[test]
    fn a_model_default_is_verbatim_not_validated() {
        let mut d = AgentDefaults::default();
        d.set_model("claude-code", Some("a-model-no-cli-has".into()));
        assert_eq!(d.model_for("claude-code"), Some("a-model-no-cli-has"));
    }

    // -- flags -------------------------------------------------------------

    /// The four flags the engine refuses per session are refused here at
    /// entry, each with a message that names the flag — mirroring the
    /// engine's own words so the two surfaces tell one story.
    #[test]
    fn a_default_never_carries_a_posture_defeating_flag() {
        for bad in [
            "--permission-mode",
            "--permission-mode=acceptEdits",
            "--permission-prompt-tool",
            "--session-id",
            "--resume",
        ] {
            let err = parse_flags(&format!("--verbose {bad}"))
                .expect_err("a posture-defeating flag must be refused");
            let head = bad.split('=').next().unwrap();
            assert!(err.contains(head), "the refusal must name the flag: {err}");
            assert!(
                err.contains("permission posture"),
                "the refusal must say why: {err}"
            );
        }
    }

    #[test]
    fn ordinary_flags_parse_in_order() {
        assert_eq!(
            parse_flags("  --verbose   --effort high ").unwrap(),
            vec!["--verbose", "--effort", "high"],
        );
        assert_eq!(parse_flags("").unwrap(), Vec::<String>::new());
    }

    /// Interning the same id twice hands back the same pointer — the
    /// leak is per distinct id, not per save.
    #[test]
    fn interning_a_model_twice_leaks_it_once() {
        let a = intern_model("s2-test-model");
        let b = intern_model("s2-test-model");
        assert!(std::ptr::eq(a, b));
        assert_eq!(a, "s2-test-model");
    }

    // -- rows --------------------------------------------------------------

    /// Stance rule 1, held: the pane's rows are exactly the registered
    /// adapters — every row can be spawned or carries the registry's own
    /// refusal, and `cursor-agent` (catalogued, no adapter) has no row.
    #[test]
    fn the_rows_are_exactly_the_registered_adapters() {
        let rows = agent_rows();
        assert_eq!(rows.len(), provider::ADAPTERS.len());
        for a in provider::ADAPTERS {
            assert!(
                rows.iter().any(|r| r.id == a.id),
                "{} is an adapter the engine would spawn but the pane does not list it",
                a.id
            );
        }
        assert!(
            !rows.iter().any(|r| r.id == "cursor-agent"),
            "cursor-agent has no adapter: a defaults row for it would be a control wired to \
             nothing (the launcher already states the absence)"
        );
    }

    #[test]
    fn the_reference_adapter_leads_and_is_editable() {
        let rows = agent_rows();
        assert_eq!(rows[0].id, "claude-code");
        assert!(rows[0].defaults_editable());
        assert!(status_askable("claude-code"));
    }

    /// Defaults are offered exactly where the engine would spawn — the
    /// same predicate (`posture_is_verified`) the launcher's click and
    /// the engine's gate use. Codex is today's negative case: adapter
    /// present, posture deliberately unproven, so no defaults.
    #[test]
    fn defaults_are_editable_exactly_where_the_engine_would_spawn() {
        for row in agent_rows() {
            assert_eq!(
                row.defaults_editable(),
                eave_core::session::posture_is_verified(row.id),
                "{}: the pane and the engine's gate disagree",
                row.id
            );
        }
        let codex = agent_rows().into_iter().find(|r| r.id == "codex");
        if let Some(codex) = codex {
            assert!(
                !codex.defaults_editable(),
                "codex's posture is unproven on purpose (Slice AU); a defaults row would \
                 promise a spawn the engine refuses"
            );
        }
    }

    /// Every row states something true about itself: ready rows have no
    /// excuse to give, non-ready rows give exactly one, and the posture
    /// fact is the registry's own declaration, not this pane's opinion.
    #[test]
    fn every_row_states_its_facts_from_the_registry() {
        for row in agent_rows() {
            match row.availability.is_ready() {
                true => assert!(row.availability.why_not(row.id).is_none()),
                false => assert!(row.availability.why_not(row.id).is_some()),
            }
            let fact = posture_fact(row.id, &row.posture);
            match row.posture {
                PostureProof::Verified { evidence } => {
                    assert!(fact.starts_with("asks before it writes"));
                    assert!(
                        fact.contains(evidence),
                        "{}: the pane must carry the registry's evidence verbatim",
                        row.id
                    );
                }
                PostureProof::Unproven { reason } => {
                    assert!(!fact.starts_with("asks before it writes"));
                    assert!(fact.contains(reason));
                    assert!(fact.contains(row.id));
                }
            }
        }
    }

    /// The gate the directive names: the pane's posture display and the
    /// rail's state the same fact in the same voice. Both open with the
    /// same words for an asking posture, and the rail's never-asks arm
    /// shares no prefix with them — so the two surfaces cannot drift
    /// into describing one session differently.
    #[test]
    fn the_pane_and_the_rail_state_the_same_posture() {
        let posture = &provider::adapter("claude-code")
            .expect("the reference adapter is registered")
            .posture;
        let fact = posture_fact("claude-code", posture);
        assert!(fact.starts_with("asks before it writes"), "{fact}");
        for mode in eave_core::claude::ASKING_MODES {
            assert!(
                crate::ui::posture_line(mode).starts_with("asks before it writes"),
                "the rail describes asking mode {mode:?} differently"
            );
        }
        assert!(
            crate::ui::posture_line("acceptEdits").starts_with("NEVER ASKS"),
            "a never-asks mode must not wear the asking words"
        );
    }

    /// The scope statement says when defaults apply, in words that
    /// cannot be read as "this changes running sessions".
    #[test]
    fn the_pane_says_defaults_are_for_new_sessions_only() {
        assert!(DEFAULTS_SCOPE_SENTENCE.contains("new sessions only"));
        assert!(DEFAULTS_SCOPE_SENTENCE.contains("creation"));
        assert!(DEFAULTS_SCOPE_SENTENCE.contains("cannot change"));
    }

    // -- status ------------------------------------------------------------

    /// The version line claude 2.1.226 actually prints, and the shapes
    /// that must not be mistaken for one.
    #[test]
    fn the_version_is_the_token_not_the_sentence() {
        assert_eq!(
            parse_version("2.1.226 (Claude Code)\n"),
            Some("2.1.226".to_string())
        );
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("error: unknown flag --version\n"), None);
        assert_eq!(parse_version("v2 is coming soon"), None);
    }

    /// The auth reply's real shape (claude 2.1.226, captured 2026-08-25;
    /// identity fields dummied — eave does not read them, so the
    /// substitution weakens nothing).
    #[test]
    fn a_signed_in_reply_names_method_and_subscription() {
        let reply = r#"{
  "loggedIn": true,
  "authMethod": "claude.ai",
  "apiProvider": "firstParty",
  "email": "operator@example.com",
  "orgId": "00000000-0000-0000-0000-000000000000",
  "orgName": "operator@example.com's Organization",
  "subscriptionType": "max"
}"#;
        assert_eq!(
            parse_auth_status(reply),
            Some(AuthStatus::SignedIn {
                method: Some("claude.ai".into()),
                subscription: Some("max".into()),
            })
        );
        let sentence = auth_sentence(parse_auth_status(reply).as_ref());
        assert!(sentence.contains("claude.ai") && sentence.contains("max"));
    }

    /// Signed out is read from `loggedIn: false` (shape untested against
    /// reality — see `parse_auth_status`), and the sentence names the
    /// exact command to run and states that eave stores nothing.
    #[test]
    fn signed_out_names_the_command_and_stores_nothing() {
        assert_eq!(
            parse_auth_status(r#"{"loggedIn": false}"#),
            Some(AuthStatus::SignedOut)
        );
        let sentence = auth_sentence(Some(&AuthStatus::SignedOut));
        assert!(sentence.contains(CLAUDE_SIGN_IN_COMMAND));
        assert!(sentence.contains("stores no credentials"));
    }

    /// An unreadable answer is stated as unreadable — never guessed
    /// toward signed-in OR signed-out — and hands the operator the
    /// command that would show them the CLI's own words.
    #[test]
    fn an_unreadable_answer_is_said_not_guessed() {
        assert_eq!(parse_auth_status("not json at all"), None);
        assert_eq!(parse_auth_status(""), None);
        assert_eq!(parse_auth_status(r#"{"somethingElse": true}"#), None);
        let sentence = auth_sentence(None);
        assert!(sentence.contains("could not read"));
        assert!(sentence.contains("claude auth status"));
    }

    /// Neither status argv carries a flag the engine refuses from a
    /// session's own args, and neither is a shape that starts a turn —
    /// the same holds `capabilities::no_probe_undoes_eaves_posture`
    /// applies to the model probes.
    #[test]
    fn no_status_probe_undoes_eaves_posture() {
        for (id, ask) in STATUS_ASKS {
            for arg in ask.version_args.iter().chain(ask.auth_args.iter()) {
                assert!(
                    !conflicts_with_eave_flags(arg),
                    "{id}'s status probe passes {arg}"
                );
            }
            assert!(
                eave_core::session::posture_is_verified(id),
                "{id} has a status ask but no proven posture — a status spawn of a refused CLI \
                 is that spawn with a friendlier name"
            );
        }
    }

    /// The cache is honest about who has been asked: unprobed is not
    /// silent, silent is not signed-out, and a recorded answer reads
    /// back as the CLI's.
    #[test]
    fn the_status_cache_distinguishes_unasked_asked_and_answered() {
        let _turn = status_test_lock();
        forget_status();
        assert_eq!(status("claude-code"), StatusView::NotProbed);

        record_status("claude-code", None);
        assert_eq!(
            status("claude-code"),
            StatusView::Silent,
            "asked-and-no-answer is a different fact from never-asked"
        );

        let s = CliStatus {
            version: Some("2.1.226".into()),
            auth: Some(AuthStatus::SignedIn {
                method: Some("claude.ai".into()),
                subscription: Some("max".into()),
            }),
        };
        record_status("claude-code", Some(s.clone()));
        assert_eq!(status("claude-code"), StatusView::Reported(s));
        forget_status();
        assert_eq!(status("claude-code"), StatusView::NotProbed);
    }

    /// The arming gate, from this side of the crate boundary: the test
    /// harness never arms probes, so `begin_status` in this process must
    /// leave the map untouched — no spawn, no `InFlight`, nothing.
    #[test]
    fn an_unarmed_process_never_spawns_a_status_probe() {
        let _turn = status_test_lock();
        assert!(
            !is_armed(),
            "the test harness must never arm probes — it does not call run()"
        );
        forget_status();
        begin_status("claude-code");
        refresh_status("claude-code");
        assert_eq!(
            status("claude-code"),
            StatusView::NotProbed,
            "an unarmed process started a probe"
        );
    }

    /// A provider with no verified status surface is asked nothing and
    /// the pane knows to render nothing — never a guess.
    #[test]
    fn a_provider_with_no_status_surface_is_not_asked() {
        let _turn = status_test_lock();
        forget_status();
        for row in agent_rows() {
            if row.id == "claude-code" {
                continue;
            }
            assert!(
                !status_askable(row.id),
                "{}: a status ask exists that this suite does not know about — extend these \
                 tests with its captured shapes",
                row.id
            );
            begin_status(row.id);
            assert_eq!(status(row.id), StatusView::NotProbed);
        }
    }
}
