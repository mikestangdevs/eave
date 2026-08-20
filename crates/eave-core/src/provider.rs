//! The provider registry: **one table that turns a provider id into an
//! adapter**, so eave can start an agent other than Claude without the
//! engine's spawn path knowing which one.
//!
//! # What was wrong
//!
//! `AgentSession` (see [`crate::session`]) has been the right abstraction
//! since Stage A — nine methods, one implementor. But nothing dispatched
//! on it. `engine.rs` named `ClaudeCodeSession::spawn` literally, and
//! `posture_is_verified` answered from a hand-maintained array of one
//! string. The seam existed; there was no lookup behind it. Five of six
//! launcher tiles said "no adapter yet" and there was nowhere for an
//! adapter to *be*.
//!
//! # The three tables, and why they are three
//!
//! 1. [`CATALOG`] — **every provider eave names**, with the binary it
//!    would run and where that binary lives on a machine. This is a fact
//!    about the vendor's CLI, not about eave's code, so it exists for
//!    providers eave cannot yet drive. It is what lets the launcher say
//!    *"the CLI is not installed"* about `cursor-agent` — a different
//!    sentence, with a different fix, from *"eave has no adapter"*.
//! 2. [`ADAPTERS`] — **every provider eave can actually drive**, one
//!    entry per implementor of [`AgentAdapter`]. Registration is one line
//!    (see the macro invocation below) and adds no match arm anywhere.
//! 3. [`crate::session::posture_is_verified`] — derived from (2), never
//!    maintained by hand again.
//!
//! A registry entry for an id that is not in the catalog cannot resolve a
//! program, so [`tests::every_registered_adapter_is_in_the_catalog`]
//! refuses one.
//!
//! # Which process does what
//!
//! eave is one binary in two roles. **The ENGINE spawns agents**, so the
//! engine is the only process that calls [`Adapter::spawn`]. The APP
//! reads [`availability`] to draw the launcher strip. Both link this
//! crate, so both answer from this table — the launcher cannot offer a
//! tile the engine would refuse, because they are reading the same rows.
//! The two processes probe the same filesystem (the app spawns the engine
//! as a child, so the engine inherits the app's `PATH`); see the caveat
//! in [`resolve_program`] for when they can still disagree.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::events::SessionEvent;
use crate::session::{AgentSession, SessionError, SpawnConfig};

// ---------------------------------------------------------------------------
// Posture

/// **Every provider's permission posture is different, and none of it is
/// the registry's to invent.** Slice K's proof — that `--permission-prompt-tool
/// stdio` with no `--permission-mode` is the only combination making the
/// Claude CLI *ask* — is Claude-specific and lives in `claude.rs`. It
/// transfers to no other CLI.
///
/// So the registry does not verify posture. It requires each adapter to
/// **declare** one, as a required associated const on [`AgentAdapter`],
/// and it makes that declaration load-bearing: only [`PostureProof::Verified`]
/// makes `posture_is_verified` true, and the engine refuses to spawn
/// anything else. An adapter that has not done the work says so and does
/// not run; there is no third state and no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostureProof {
    /// This adapter proves, **at runtime and on every spawn**, that its
    /// CLI will ask before it writes, and refuses the session when it
    /// cannot.
    ///
    /// `evidence` names where that check lives and what it reads, so the
    /// claim can be audited from this table rather than taken on trust —
    /// e.g. Claude's reads the CLI's own `init.permissionMode` against
    /// `ASKING_MODES`. A string is not a proof; it is a pointer to one,
    /// and the reason it is required is that a reviewer looking at this
    /// registry should never have to guess whether a `Verified` was
    /// earned.
    Verified { evidence: &'static str },
    /// The adapter exists and can spawn, but eave has not established
    /// that the CLI asks before it writes — so eave will not run it.
    /// A session naming it is refused with `reason`, at open and again at
    /// spawn, exactly as an unknown provider is.
    Unproven { reason: &'static str },
}

impl PostureProof {
    pub fn is_verified(&self) -> bool {
        matches!(self, PostureProof::Verified { .. })
    }
}

// ---------------------------------------------------------------------------
// The adapter contract

/// What a provider adapter must implement to be registerable.
///
/// The whole contract is: **be an [`AgentSession`], name your provider id,
/// declare your posture, and spawn yourself**. Everything else the engine
/// does after spawn already goes through the trait.
///
/// Spawning stays on the concrete type — it returns `Self` plus its own
/// event receiver, which no trait object can express — and this trait is
/// the bridge: the [`adapters!`] macro turns the concrete `spawn` into a
/// function pointer that boxes the result, which is what makes the
/// registry a table of data rather than a match.
///
/// Neither const has a default, so **an adapter that has not declared a
/// posture does not compile**. That is the whole of requirement 5: the
/// declaration is not a convention, it is the type system.
pub trait AgentAdapter: AgentSession + Sized + 'static {
    /// The value that appears in `AgentConfig::provider`, in the journal
    /// header, and on the launcher tile. Must match a [`CATALOG`] id.
    const PROVIDER_ID: &'static str;

    /// This adapter's own answer to "does this CLI ask before it writes,
    /// and how do you know". See [`PostureProof`].
    const POSTURE: PostureProof;

    /// Spawn the CLI. The returned receiver's final event must be
    /// `SessionEvent::Exited` — the recorder waits on it.
    fn spawn(cfg: &SpawnConfig) -> Result<(Self, mpsc::Receiver<SessionEvent>), SessionError>;
}

/// A live agent behind dynamic dispatch.
///
/// # Why boxed rather than an enum over the four known providers
///
/// The premise that `AgentSession` "is not object-safe because it has
/// `async fn` methods" is **false as the trait stands**: it carries
/// `#[async_trait]`, which rewrites every `async fn` into
/// `fn(..) -> Pin<Box<dyn Future + Send + '_>>` before the compiler ever
/// sees a trait. It is object-safe today —
/// [`tests::the_session_trait_is_object_safe`] is the proof, and it is a
/// compile-time one.
///
/// Given that, the enum loses on the only axis this slice cares about:
///
/// - **It does not remove the match, it relocates it.** An enum must name
///   `crate::codex::CodexSession` and friends, so `eave-core` grows a
///   variant *and* an arm in each of the nine forwarding methods per
///   provider. Four adapter slices landing in parallel would each touch
///   thirty-six lines of one file. That is the conflict the operator
///   asked to be designed out, moved from `engine.rs` to here.
/// - **It closes a set that is not closed.** Six providers today; the
///   catalog is a list eave adds to.
/// - **The cost it saves is already being paid.** `#[async_trait]`
///   allocates a boxed future on *every* call to `send_user_message`,
///   `respond_to_approval` and `kill` — on the concrete type too. Adding
///   a vtable jump to a call that already heap-allocates a future and
///   then performs a `write(2)` is not measurable. The one extra
///   allocation per *session* (not per call, per session) is noise beside
///   the process spawn it accompanies.
///
/// An enum would win if the trait were hot and monomorphizable. It is
/// neither: every method is a field read or an await on I/O, and the
/// call rate is bounded by how fast a human types.
pub type BoxedSession = Box<dyn AgentSession>;

type SpawnFn =
    fn(&SpawnConfig) -> Result<(BoxedSession, mpsc::Receiver<SessionEvent>), SessionError>;

fn spawn_boxed<A: AgentAdapter>(
    cfg: &SpawnConfig,
) -> Result<(BoxedSession, mpsc::Receiver<SessionEvent>), SessionError> {
    A::spawn(cfg).map(|(session, events)| (Box::new(session) as BoxedSession, events))
}

/// One row of [`ADAPTERS`]: a provider id, its declared posture, and the
/// function that starts it. Deliberately data — nothing here branches on
/// which provider it is.
pub struct Adapter {
    pub id: &'static str,
    pub posture: PostureProof,
    spawn: SpawnFn,
}

impl Adapter {
    /// Spawn this provider's CLI. **Called by the engine process only.**
    pub fn spawn(
        &self,
        cfg: &SpawnConfig,
    ) -> Result<(BoxedSession, mpsc::Receiver<SessionEvent>), SessionError> {
        (self.spawn)(cfg)
    }
}

/// Build the registry from a list of adapter types — **one line each**.
///
/// A macro rather than a `const fn` generic so each row is a plain struct
/// literal reading the type's own associated consts; there is no place for
/// a row to disagree with the adapter it names.
macro_rules! adapters {
    ($($adapter:ty),* $(,)?) => {
        /// Every provider eave can drive, in no particular order.
        ///
        /// **To register an adapter, add one line to the
        /// `adapters! { … }` invocation in this file.** Nothing else in
        /// the engine, the wire or the app needs to change: the launcher
        /// tile lights up, `posture_is_verified` starts answering true,
        /// and the spawn path finds you.
        pub const ADAPTERS: &[Adapter] = &[$(Adapter {
            id: <$adapter as AgentAdapter>::PROVIDER_ID,
            posture: <$adapter as AgentAdapter>::POSTURE,
            spawn: spawn_boxed::<$adapter>,
        }),*];
    };
}

adapters! {
    // Claude Code — the reference adapter. Slice K's posture proof.
    crate::claude::ClaudeCodeSession,

    // The four below are being written in parallel (Slices AT/codex,
    // AT/grok, AT/hermes, AT/pi). Each lands as
    // `crates/eave-core/src/<id>.rs`, adds its `pub mod` to `lib.rs`, and
    // uncomments its own line here. Nothing else.

    // crate::codex::CodexSession,

    // crate::grok::GrokSession,

    // crate::hermes::HermesSession,

    // crate::pi::PiSession,
}

/// The adapter for a provider id, or `None` if eave cannot drive it.
///
/// A linear scan over a handful of rows — a map would cost more than it
/// saves at six entries, and this is called once per spawn and once per
/// launcher tile.
pub fn adapter(provider_id: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.id == provider_id)
}

// ---------------------------------------------------------------------------
// The catalog: which binary, and where it lives

/// Where a provider's CLI is found on a machine.
///
/// `bin` is the name as it would appear on `PATH`. `also_look_in` is for
/// installers that put their binary somewhere `PATH` does not
/// necessarily reach — it is searched **after** `PATH`, so an operator's
/// own copy always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramSpec {
    pub bin: &'static str,
    pub also_look_in: &'static [&'static str],
}

/// One provider eave names.
pub struct CatalogEntry {
    pub id: &'static str,
    pub program: ProgramSpec,
}

/// Bin directories searched after `PATH`, for every provider.
///
/// **This list is not a nicety, it is what makes the packaged app work.**
/// `launchctl getenv PATH` is empty on macOS, so an eave launched from
/// Finder or `/Applications` inherits launchd's default
/// `/usr/bin:/bin:/usr/sbin:/sbin` — and *none* of the five installed
/// agent CLIs on this machine live there. A `PATH`-only resolver would
/// report every provider as not installed in exactly the configuration a
/// user runs eave in.
///
/// Each entry is a directory a CLI on this machine actually resolved
/// from, or the documented install target of one that did:
/// `/opt/homebrew/bin` (claude, codex, pi — Homebrew on Apple silicon),
/// `~/.local/bin` (hermes), `/usr/local/bin` (Homebrew on Intel),
/// `~/.bun/bin` and `~/.cargo/bin` (the two package managers whose
/// shims land outside both).
const COMMON_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "~/.local/bin",
    "~/.bun/bin",
    "~/.cargo/bin",
];

/// Every provider eave names — the launcher strip's population, and the
/// only ids `AgentConfig::provider` may carry.
///
/// Adding an id here does **not** make it startable; that takes an
/// [`AgentAdapter`]. The two are separate on purpose: a named provider
/// with no adapter is a promise eave is visibly not keeping, which the
/// launcher states, and a promise stated is better than a tile hidden.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "claude-code",
        program: ProgramSpec {
            bin: "claude",
            also_look_in: &[],
        },
    },
    CatalogEntry {
        id: "codex",
        program: ProgramSpec {
            bin: "codex",
            also_look_in: &[],
        },
    },
    CatalogEntry {
        id: "cursor-agent",
        program: ProgramSpec {
            bin: "cursor-agent",
            // No bespoke path, and that is the verified answer rather
            // than an omission: cursor-agent installs a symlink in
            // `~/.local/bin` (→ `~/.local/share/cursor-agent/versions/…`),
            // which `COMMON_BIN_DIRS` already covers. An earlier draft
            // guessed `~/.cursor/bin` while nothing was installed to
            // check against; the guess was wrong and is gone.
            also_look_in: &[],
        },
    },
    CatalogEntry {
        id: "grok",
        program: ProgramSpec {
            bin: "grok",
            // grok installs to its own home rather than a shared bin dir
            // (verified: `~/.grok/bin/grok` → `../downloads/grok-1.0.5-macos-aarch64`).
            also_look_in: &["~/.grok/bin"],
        },
    },
    CatalogEntry {
        id: "hermes",
        program: ProgramSpec {
            bin: "hermes",
            also_look_in: &[],
        },
    },
    CatalogEntry {
        id: "pi",
        program: ProgramSpec {
            bin: "pi",
            also_look_in: &[],
        },
    },
];

/// The catalog row for an id, or `None` for a provider eave does not name.
pub fn catalog(provider_id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == provider_id)
}

/// The environment variable that overrides where a provider's binary is
/// found: `EAVE_PROVIDER_BIN_CLAUDE_CODE`, `EAVE_PROVIDER_BIN_GROK`, …
///
/// An operator whose CLI is somewhere eave does not look has a way to say
/// so that does not require a code change, and the not-installed message
/// names this variable rather than leaving them stuck.
pub fn program_override_var(provider_id: &str) -> String {
    format!(
        "EAVE_PROVIDER_BIN_{}",
        provider_id.to_ascii_uppercase().replace('-', "_")
    )
}

/// Where a provider's CLI is, or where eave looked and did not find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramResolution {
    Found(PathBuf),
    /// Not on this machine. `searched` is every path that was stat'd, in
    /// order, so the message can say where eave looked instead of just
    /// that it failed.
    NotInstalled {
        bin: &'static str,
        searched: Vec<PathBuf>,
    },
}

impl ProgramResolution {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ProgramResolution::Found(p) => Some(p),
            ProgramResolution::NotInstalled { .. } => None,
        }
    }

    /// The binary's own name, whether or not it was found. The engine
    /// uses it to answer "is this session going to run the real `claude`
    /// CLI", which decides whether a non-UUID session id is legal.
    pub fn program_name(&self) -> Option<&str> {
        match self {
            ProgramResolution::Found(p) => p.file_name().and_then(|n| n.to_str()),
            ProgramResolution::NotInstalled { bin, .. } => Some(bin),
        }
    }

    /// One sentence an operator can act on: what is missing, where eave
    /// looked, and how to point it somewhere else. `None` for `Found`.
    pub fn why_not(&self, provider_id: &str) -> Option<String> {
        match self {
            ProgramResolution::Found(_) => None,
            ProgramResolution::NotInstalled { bin, searched } => {
                Some(not_installed_sentence(bin, provider_id, searched.len()))
            }
        }
    }
}

/// The one sentence eave says about a CLI it cannot find, written once
/// so the engine's refusal and the launcher's caption cannot drift into
/// telling an operator two different stories about the same absence.
fn not_installed_sentence(bin: &str, provider_id: &str, places_searched: usize) -> String {
    format!(
        "the {bin:?} CLI is not installed on this machine — eave looked in {places_searched} \
         place(s) including PATH; install it, or set {} to point at it",
        program_override_var(provider_id),
    )
}

fn expand_home(dir: &str) -> Option<PathBuf> {
    match dir.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(rest)),
        None => Some(PathBuf::from(dir)),
    }
}

fn is_executable_file(p: &Path) -> bool {
    // `metadata` follows symlinks, which is what we want: every CLI on
    // this machine is a symlink into a Cask or a node_modules tree.
    match std::fs::metadata(p) {
        Ok(m) => {
            m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
        }
        Err(_) => false,
    }
}

/// Resolve a provider's binary on **this** machine, uncached.
///
/// Order: the `EAVE_PROVIDER_BIN_*` override **alone, if it is set**;
/// otherwise every directory on `PATH`, then the provider's own
/// `also_look_in`, then [`COMMON_BIN_DIRS`]. `PATH` before the fallbacks
/// so an operator's own copy wins over a Homebrew one.
///
/// **Caveat — the app and the engine can disagree.** Normally they cannot:
/// the app spawns `eave --engine` as a child, so the engine inherits the
/// app's environment and resolves identically. An engine started
/// separately (a shell, a launch agent) with a different `PATH` can
/// resolve differently from the app drawing the tiles, and the engine is
/// the one that is right, because the engine is the process that spawns.
/// That is why a launch eave believed would work still gets a stated
/// refusal from the engine rather than an assumption from the app.
pub fn resolve_program_uncached(entry: &CatalogEntry) -> ProgramResolution {
    let mut searched = Vec::new();

    // An override is AUTHORITATIVE, not a first guess. If an operator
    // says the binary is at a path and it is not there, eave reports that
    // path as missing rather than quietly running a different copy it
    // found on `PATH` — the quiet substitution this codebase refuses
    // everywhere else (`reject_config_change` is the same principle).
    if let Some(p) = std::env::var_os(program_override_var(entry.id)).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if is_executable_file(&p) {
            ProgramResolution::Found(p)
        } else {
            ProgramResolution::NotInstalled {
                bin: entry.program.bin,
                searched: vec![p],
            }
        };
    }

    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    let extra = entry
        .program
        .also_look_in
        .iter()
        .chain(COMMON_BIN_DIRS)
        .filter_map(|d| expand_home(d));

    for dir in path_dirs.into_iter().chain(extra) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(entry.program.bin);
        if searched.contains(&candidate) {
            continue;
        }
        if is_executable_file(&candidate) {
            return ProgramResolution::Found(candidate);
        }
        searched.push(candidate);
    }

    ProgramResolution::NotInstalled {
        bin: entry.program.bin,
        searched,
    }
}

/// How long a resolution is reused before the filesystem is asked again.
///
/// The launcher draws six tiles and asks each one whether its CLI exists;
/// gpui redraws on every frame, so an uncached probe would run the whole
/// sweep on every frame. **Measured on this machine** (29 `PATH` entries
/// × 6 binaries, so ~170 `stat` calls): one full sweep costs **483 µs
/// cold, 62 µs warm**, against **1.4 µs** through the memo. At 60 fps the
/// uncached sweep is ~0.4% of a core spent re-asking a question whose
/// answer only changes when somebody runs an installer.
///
/// **One second is a policy choice, not a measurement**, and it is
/// bounded on both sides by one: at 62 µs a sweep it costs ~0.006% of a
/// core, and it is short enough that installing a CLI lights its tile
/// before an operator would think to restart eave. It is not
/// correctness-critical in either direction — the engine resolves
/// uncached at spawn, so a stale `Ready` produces a stated refusal, never
/// a bad spawn.
const RESOLUTION_TTL: Duration = Duration::from_secs(1);

struct Memo {
    at: Instant,
    resolutions: Vec<(&'static str, ProgramResolution)>,
}

static MEMO: Mutex<Option<Memo>> = Mutex::new(None);

/// Resolve a provider's binary, reusing a resolution up to
/// [`RESOLUTION_TTL`] old. Use this on a render path; use
/// [`resolve_program_uncached`] when about to spawn.
pub fn resolve_program(entry: &'static CatalogEntry) -> ProgramResolution {
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = memo
        .as_ref()
        .is_some_and(|m| m.at.elapsed() < RESOLUTION_TTL);
    if !fresh {
        *memo = Some(Memo {
            at: Instant::now(),
            resolutions: CATALOG
                .iter()
                .map(|e| (e.id, resolve_program_uncached(e)))
                .collect(),
        });
    }
    memo.as_ref()
        .and_then(|m| m.resolutions.iter().find(|(id, _)| *id == entry.id))
        .map(|(_, r)| r.clone())
        // An entry outside the catalog cannot be memoized; answer directly
        // rather than lie about it.
        .unwrap_or_else(|| resolve_program_uncached(entry))
}

/// Drop the memo. For tests that change `PATH` or an override variable
/// and need the next resolution to see it.
pub fn forget_resolutions() {
    *MEMO.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

// ---------------------------------------------------------------------------
// Availability: the one question the launcher asks

/// Can eave start this provider here, and if not, **which** not?
///
/// The distinction is the point. "eave has no adapter for Cursor" and
/// "the Cursor CLI is not on this machine" have different fixes — one is
/// eave's work, one is a `brew install` — and a UI that renders both as a
/// grey tile saying "no adapter yet" tells the operator to wait for a
/// release they do not need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// An adapter, a verified posture, and the CLI is at `program`.
    Ready { program: PathBuf },
    /// An adapter with a verified posture, but the CLI is not installed.
    NotInstalled {
        bin: &'static str,
        /// Every path eave stat'd, for the message.
        searched: Vec<PathBuf>,
    },
    /// An adapter exists and declares its own posture unproven, so eave
    /// refuses to run it (see [`PostureProof`]).
    PostureUnproven { reason: &'static str },
    /// eave names this provider and has no adapter for it yet.
    NoAdapter,
    /// eave does not name this provider at all — a config or a journal
    /// naming it was written by something else.
    Unknown,
}

impl Availability {
    /// Is this provider startable right now?
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready { .. })
    }

    /// Three or four words for a launcher tile. Present tense, about this
    /// machine.
    pub fn tile_status(&self) -> &'static str {
        match self {
            Availability::Ready { .. } => "ready",
            Availability::NotInstalled { .. } => "not installed",
            Availability::PostureUnproven { .. } => "posture unproven",
            Availability::NoAdapter => "no adapter yet",
            Availability::Unknown => "unknown agent",
        }
    }

    /// The whole sentence, for a refusal or a tooltip. `None` when there
    /// is nothing to explain.
    pub fn why_not(&self, provider_id: &str) -> Option<String> {
        match self {
            Availability::Ready { .. } => None,
            Availability::NotInstalled { bin, searched } => {
                Some(not_installed_sentence(bin, provider_id, searched.len()))
            }
            Availability::PostureUnproven { reason } => Some(format!(
                "eave will not run {provider_id} unsupervised: {reason}"
            )),
            Availability::NoAdapter => Some(format!("eave has no adapter for {provider_id} yet")),
            Availability::Unknown => Some(format!("eave does not know a provider {provider_id:?}")),
        }
    }
}

/// What eave can do about this provider on this machine, cached per
/// [`RESOLUTION_TTL`]. **Called by the app to draw a tile**; the engine
/// answers the same question uncached at spawn.
pub fn availability(provider_id: &str) -> Availability {
    let Some(entry) = catalog(provider_id) else {
        return Availability::Unknown;
    };
    let Some(adapter) = adapter(provider_id) else {
        return Availability::NoAdapter;
    };
    if let PostureProof::Unproven { reason } = adapter.posture {
        return Availability::PostureUnproven { reason };
    }
    match resolve_program(entry) {
        ProgramResolution::Found(program) => Availability::Ready { program },
        ProgramResolution::NotInstalled { bin, searched } => {
            Availability::NotInstalled { bin, searched }
        }
    }
}

/// Every provider whose posture an adapter has declared verified — the
/// list the engine refuses against, derived rather than maintained.
pub fn posture_verified_providers() -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .filter(|a| a.posture.is_verified())
        .map(|a| a.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dispatch decision, asserted by the compiler: `AgentSession` is
    /// object-safe, so the registry can hold `Box<dyn AgentSession>` and
    /// no enum over the providers is needed. If somebody adds a generic
    /// method or a `Self`-returning one to the trait, this stops
    /// compiling — which is the moment to revisit [`BoxedSession`]'s
    /// argument, not a moment to reach for `#[allow]`.
    #[test]
    fn the_session_trait_is_object_safe() {
        fn takes_a_trait_object(_: &dyn AgentSession) {}
        let _ = takes_a_trait_object;
    }

    #[test]
    fn every_registered_adapter_is_in_the_catalog() {
        for a in ADAPTERS {
            assert!(
                catalog(a.id).is_some(),
                "{} is registered as an adapter but names no catalog entry, so eave cannot find \
                 its binary",
                a.id
            );
        }
    }

    #[test]
    fn catalog_ids_and_binaries_are_distinct() {
        for (i, e) in CATALOG.iter().enumerate() {
            assert!(!e.id.is_empty(), "a catalog entry has no id");
            assert!(!e.program.bin.is_empty(), "{} names no binary", e.id);
            assert!(
                CATALOG.iter().skip(i + 1).all(|o| o.id != e.id),
                "{} appears twice in the catalog",
                e.id
            );
            assert!(
                CATALOG
                    .iter()
                    .skip(i + 1)
                    .all(|o| o.program.bin != e.program.bin),
                "{} shares a binary name with another provider, so a resolution would be \
                 ambiguous",
                e.id
            );
        }
    }

    #[test]
    fn adapters_are_registered_once_each() {
        for (i, a) in ADAPTERS.iter().enumerate() {
            assert!(
                ADAPTERS.iter().skip(i + 1).all(|o| o.id != a.id),
                "{} is registered twice — `adapter()` would silently pick the first",
                a.id
            );
        }
    }

    /// A `Verified` claim must point at where the proof lives. The string
    /// is not the proof; a `Verified` with nothing behind it is the shape
    /// of an adapter that skipped the work, and this is the cheapest
    /// place to refuse it.
    #[test]
    fn a_verified_posture_names_its_evidence() {
        for a in ADAPTERS {
            match a.posture {
                PostureProof::Verified { evidence } => assert!(
                    evidence.len() > 20,
                    "{}: a verified posture must name what is checked and where; got {evidence:?}",
                    a.id
                ),
                PostureProof::Unproven { reason } => assert!(
                    !reason.is_empty(),
                    "{}: an unproven posture must say why",
                    a.id
                ),
            }
        }
    }

    /// The derived list replaces the hand-kept array that used to live in
    /// `session.rs`. Asserted against the registry's own rows rather than
    /// against a literal, so it cannot be satisfied by editing a constant.
    #[test]
    fn the_verified_list_is_derived_from_the_registry() {
        let derived = posture_verified_providers();
        for a in ADAPTERS {
            assert_eq!(
                derived.contains(&a.id),
                a.posture.is_verified(),
                "{} is on the verified list iff it declares a verified posture",
                a.id
            );
        }
        assert_eq!(
            derived.len(),
            ADAPTERS.iter().filter(|a| a.posture.is_verified()).count()
        );
    }

    /// Every named provider answers, and answers a *reason*. The one
    /// thing this must never do is panic or hang on a provider with no
    /// adapter and no binary — `cursor-agent` is both.
    #[test]
    fn every_named_provider_answers_with_a_reason() {
        for e in CATALOG {
            let a = availability(e.id);
            match a.is_ready() {
                true => assert!(a.why_not(e.id).is_none(), "{}: ready with a reason", e.id),
                false => {
                    let why = a.why_not(e.id).expect("a refusal must state itself");
                    assert!(
                        why.contains(e.id) || why.contains(e.program.bin),
                        "{}: the reason names neither the provider nor its binary: {why}",
                        e.id
                    );
                }
            }
        }
    }

    /// **Every catalogued binary resolves to a total, stated answer** —
    /// a path or an absence that names where eave looked. Never a panic,
    /// never an error, never a blocking call.
    ///
    /// Written to be true on any machine: `cursor-agent` is the one that
    /// is absent here and drives the second arm, but the assertion does
    /// not depend on that staying so.
    #[test]
    fn every_catalogued_binary_resolves_or_states_its_absence() {
        for e in CATALOG {
            match resolve_program_uncached(e) {
                ProgramResolution::Found(p) => assert!(
                    p.is_absolute(),
                    "{}: resolved to {p:?}, which the engine would hand to execve unresolved",
                    e.id
                ),
                ProgramResolution::NotInstalled { bin, searched } => {
                    assert_eq!(bin, e.program.bin);
                    assert!(
                        !searched.is_empty(),
                        "{}: absent, and eave will not say where it looked",
                        e.id
                    );
                }
            }
        }
    }

    #[test]
    fn a_provider_eave_does_not_name_is_unknown_not_missing() {
        let a = availability("nothing-eave-has-ever-heard-of");
        assert_eq!(a, Availability::Unknown);
        assert!(!a.is_ready());
    }

    /// **eave's own gap is reported before the machine's**, and the order
    /// matters: a provider with no adapter says so whether or not its CLI
    /// is installed, because installing it would not help.
    ///
    /// Asserted over every catalogued provider eave cannot drive rather
    /// than over one name — `cursor-agent` was the example when this was
    /// written and was uninstalled at the time; it acquired a CLI three
    /// hours later, which is exactly the drift a named example invites.
    #[test]
    fn a_provider_with_no_adapter_says_so_before_anything_about_the_machine() {
        let mut checked = 0;
        for e in CATALOG {
            if adapter(e.id).is_some() {
                continue;
            }
            checked += 1;
            let a = availability(e.id);
            assert_eq!(
                a,
                Availability::NoAdapter,
                "{}: eave has no adapter for it, so nothing about this machine should decide \
                 what the tile says",
                e.id
            );
            assert_eq!(a.tile_status(), "no adapter yet");
            assert!(a.why_not(e.id).is_some());
        }
        assert!(
            checked > 0,
            "every catalogued provider has an adapter — delete this test, it proves nothing"
        );
    }

    /// The not-installed path itself, driven through a real catalog entry
    /// whose binary cannot exist. Proves the resolver returns a stated
    /// absence rather than erroring, panicking, or blocking.
    #[test]
    fn a_binary_that_is_nowhere_resolves_to_not_installed() {
        let entry = CatalogEntry {
            id: "not-a-real-provider",
            program: ProgramSpec {
                bin: "eave-test-binary-that-does-not-exist",
                also_look_in: &["~/.nowhere/bin"],
            },
        };
        match resolve_program_uncached(&entry) {
            ProgramResolution::NotInstalled { bin, searched } => {
                assert_eq!(bin, "eave-test-binary-that-does-not-exist");
                assert!(
                    !searched.is_empty(),
                    "the resolver must report where it looked"
                );
                let why = ProgramResolution::NotInstalled {
                    bin,
                    searched: searched.clone(),
                }
                .why_not(entry.id)
                .expect("not-installed states itself");
                assert!(
                    why.contains("EAVE_PROVIDER_BIN_NOT_A_REAL_PROVIDER"),
                    "the message must name the escape hatch: {why}"
                );
            }
            ProgramResolution::Found(p) => panic!("resolved a nonexistent binary to {p:?}"),
        }
    }

    /// The override wins over every search path, and a directory is never
    /// mistaken for a program.
    #[test]
    fn the_override_variable_points_the_resolver_at_a_binary() {
        let entry = CatalogEntry {
            id: "override-probe",
            program: ProgramSpec {
                bin: "eave-test-binary-that-does-not-exist",
                also_look_in: &[],
            },
        };
        let var = program_override_var(entry.id);
        assert_eq!(var, "EAVE_PROVIDER_BIN_OVERRIDE_PROBE");

        // `/bin/sh` exists and is executable on every machine this runs
        // on; `/bin` is a directory and must not resolve.
        std::env::set_var(&var, "/bin/sh");
        assert_eq!(
            resolve_program_uncached(&entry),
            ProgramResolution::Found(PathBuf::from("/bin/sh"))
        );
        std::env::set_var(&var, "/bin");
        assert!(
            matches!(
                resolve_program_uncached(&entry),
                ProgramResolution::NotInstalled { .. }
            ),
            "a directory resolved as a program"
        );

        // An override that names nothing is a stated absence, never a
        // fallback to something else that happens to be on PATH — proved
        // against a binary that certainly IS on PATH.
        std::env::remove_var(&var);
        let real = CatalogEntry {
            id: "override-probe",
            program: ProgramSpec {
                bin: "sh",
                also_look_in: &[],
            },
        };
        assert!(
            matches!(resolve_program_uncached(&real), ProgramResolution::Found(_)),
            "sh is on PATH; the fixture is wrong"
        );
        std::env::set_var(&var, "/definitely/not/here/sh");
        match resolve_program_uncached(&real) {
            ProgramResolution::NotInstalled { searched, .. } => assert_eq!(
                searched,
                vec![PathBuf::from("/definitely/not/here/sh")],
                "an override must report the path it was given, and look nowhere else"
            ),
            ProgramResolution::Found(p) => {
                panic!("an override eave could not honour silently ran {p:?} instead")
            }
        }
        std::env::remove_var(&var);
    }

    #[test]
    fn the_override_variable_name_is_derived_from_the_id() {
        assert_eq!(
            program_override_var("claude-code"),
            "EAVE_PROVIDER_BIN_CLAUDE_CODE"
        );
        assert_eq!(program_override_var("pi"), "EAVE_PROVIDER_BIN_PI");
        for e in CATALOG {
            let v = program_override_var(e.id);
            assert!(
                v.starts_with("EAVE_PROVIDER_BIN_") && !v.contains('-'),
                "{}: {v} is not a legal environment variable name",
                e.id
            );
        }
    }

    /// The memo must not outlive a change to where eave looks — a cached
    /// "not installed" that survives an install would be a tile that
    /// never lights up.
    #[test]
    fn a_forgotten_resolution_is_taken_again() {
        let entry = catalog("claude-code").expect("claude-code is catalogued");
        let first = resolve_program(entry);
        forget_resolutions();
        let second = resolve_program(entry);
        assert_eq!(
            first, second,
            "the same filesystem answered two different things"
        );
    }
}
