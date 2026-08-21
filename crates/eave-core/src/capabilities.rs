//! Slice CK — **what the installed CLI says it can run.**
//!
//! # The defect this closes
//!
//! Until this slice every model eave offered was typed by hand. The
//! launcher's `PI_MODELS` named six `provider/id` pairs; `claude-code`
//! named `opus`/`sonnet`/`haiku`; Codex, Cursor and Grok named nothing at
//! all. Nothing ever asked the binary on disk, so the lists could only
//! rot, and they had — measured on this machine 2026-08-21 with the CLIs
//! installed:
//!
//! - `pi --list-models` answered *"No models available. Use /login…"*
//!   while eave's picker offered six Pi models, **every one of them
//!   unusable**.
//! - `grok models` named `grok-4.6` and `grok-4.5`; eave's Grok picker
//!   named none.
//! - `codex app-server`'s `model/list` named five; eave's picker named
//!   none.
//! - Claude Code's own `list_models` control request named
//!   `claude-fable-5[1m]` and `opus[1m]`; eave's picker had never heard of
//!   either.
//!
//! # Asking is a spawn, so it obeys the spawn rules
//!
//! A probe runs a vendor's CLI. That makes it subject to the same
//! discipline as a session and **not** a hole around it:
//!
//! - It runs only for a provider [`crate::session::posture_is_verified`]
//!   already accepts. eave will not start a CLI it has refused to drive
//!   just because the question is a small one.
//! - Its argv is a `&'static` constant per provider. Nothing from a
//!   session, a config file or an operator's `extra_args` reaches it, so
//!   there is no path by which a probe could carry
//!   [`crate::session::conflicts_with_eave_flags`] — and
//!   [`tests::no_probe_undoes_eaves_posture`] holds that shut.
//! - It never sends a user message, so **no turn ever starts and no tool
//!   call can ever be requested.** That, not a flag, is why a probe cannot
//!   write to disk: there is nothing on the other side to write.
//! - It is bounded by [`PROBE_TIMEOUT`] and killed, never waited on.
//!
//! # A probe must never block the launcher
//!
//! comet learned this one expensively. `docs/research/acp.md`
//! §"Managed adapter installs" (2026-08-15) records that a cold `npx`
//! *"could also stall a first chat for minutes"*, and its rule since is
//! that "discovery probes never block on npm … `run()` blocks". eave takes
//! the same rule for the same reason.
//!
//! So every function here answers **from memory, immediately**. [`begin`]
//! starts a thread and returns; [`reported`] reads a map. A launcher frame
//! that draws before any child process has answered draws eave's own list
//! and *says so* — stale-but-instant beats correct-but-frozen, and
//! [`Freshness`] exists so the UI can never show one while implying the
//! other.
//!
//! # Silence is the honest answer
//!
//! [`ModelSource::Silence`] is a first-class row, not a gap. Hermes has no
//! non-interactive model list — `hermes model` is a picker TUI and its
//! `--refresh` re-fetches every provider's live `/v1/models` — so eave
//! does not pretend to have asked. It keeps its static entry and the row
//! says, in one sentence, why nobody could be asked. Same house rule as
//! `events.rs`'s empty `slash_commands` and `provider.rs`'s
//! `ProgramResolution`: never a guess, and never filled in from something
//! that was not the question.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a probe may take before it is killed and treated as silence.
///
/// **15s is the siblings' number, not one chosen here.** Three
/// independent repos on this disk converged on it for exactly this job:
/// comet `crates/harness/src/cursor/mod.rs:139` (`Duration::from_secs(15)`
/// around the model shim), synara
/// `apps/server/src/provider/Layers/GrokAdapter.ts:141`
/// (`GROK_MODEL_DISCOVERY_TIMEOUT_MS = 15_000`) and t3code
/// `apps/server/src/provider/Layers/CursorProvider.ts:62`
/// (`CURSOR_ACP_MODEL_DISCOVERY_TIMEOUT_MS = 15_000`). Nothing here waits
/// on it, so it bounds a background thread's lifetime rather than a frame.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How to ask one CLI what it can run.
pub enum Ask {
    /// Run the CLI with a fixed argv and read what it prints.
    Argv {
        args: &'static [&'static str],
        /// stdout and stderr, joined — several of these CLIs print their
        /// list on one and their refusal on the other.
        read: fn(&str) -> Vec<String>,
    },
    /// Speak the CLI's own newline-framed JSON wire on stdio: write these
    /// lines, then read replies until one of them is the answer.
    Wire {
        args: &'static [&'static str],
        write: &'static [&'static str],
        /// `None` = not the frame we are waiting for; keep reading.
        read: fn(&str) -> Option<Vec<String>>,
    },
}

/// Where a provider's model list comes from — or why it cannot come from
/// anywhere.
pub enum ModelSource {
    /// The installed CLI can be asked, like this.
    Ask(Ask),
    /// It cannot, and this is the sentence that says why. Shown to the
    /// operator verbatim, so it is written for one.
    Silence(&'static str),
}

/// The claude control request that names the `--model` values this build
/// of Claude Code accepts.
///
/// **Verified live 2026-08-21** against `claude` 2.1.x on this machine
/// over the exact wire `claude.rs` already speaks (`-p --input-format
/// stream-json --output-format stream-json --verbose`): the reply is a
/// `control_response` whose `response.models[]` carries `value` (the
/// alias `--model` takes), `resolvedModel`, `displayName`, `description`
/// and `supportedEffortLevels`. The subtype is also present in the
/// shipped binary's own strings (`subtype:"list_models"`).
///
/// This corrects three siblings at once. waku
/// (`crates/waku-core/src/model_catalog.rs:113`) says Claude Code "does
/// not expose a model inventory command" and keeps a static list; t3code
/// (`ClaudeProvider.ts:329`) and comet (`claude/catalog.rs`) are static
/// too. Only synara reaches it, and only through the Agent SDK's
/// `query.supportedModels()` (`ClaudeAdapter.ts:6289`) rather than the
/// raw wire. eave drives the raw wire, so eave can ask directly.
const CLAUDE_LIST_MODELS: &[&str] = &[
    r#"{"type":"control_request","request_id":"eave-capabilities","request":{"subtype":"list_models"}}"#,
];

/// `initialize` → `initialized` → `model/list` on the Codex app-server.
///
/// **Verified live 2026-08-21** against `codex` 0.148.0: `model/list`
/// answers `{"data":[{"id":…}],"nextCursor":null}` with no auth prompt and
/// no `capabilities.experimentalApi` opt-in. waku spawns the same three
/// frames from Rust (`model_catalog.rs:592-665`); t3code
/// (`CodexProvider.ts:297`) and synara (`codexAppServerManager.ts:2476`)
/// send the same method. comet is the outlier and stayed static, with the
/// live path written down as a seam (`codex/mod.rs:299`).
const CODEX_MODEL_LIST: &[&str] = &[
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"eave","version":"0"}}}"#,
    r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    r#"{"jsonrpc":"2.0","id":2,"method":"model/list","params":{}}"#,
];

/// One row per [`crate::provider::CATALOG`] id, held to that by
/// [`tests::every_catalogued_provider_states_where_its_models_come_from`].
///
/// Each row's evidence level is stated on the parser it names.
pub const MODEL_SOURCES: &[(&str, ModelSource)] = &[
    (
        "claude-code",
        ModelSource::Ask(Ask::Wire {
            // The same flags `claude.rs` opens a session with, minus the
            // session's own: no `--session-id`, no `--resume`, and no
            // `--permission-prompt-tool`, because no turn is started and
            // so nothing can ask for permission to do anything.
            args: &[
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ],
            write: CLAUDE_LIST_MODELS,
            read: claude_list_models_reply,
        }),
    ),
    (
        "codex",
        ModelSource::Ask(Ask::Wire {
            args: &["app-server"],
            write: CODEX_MODEL_LIST,
            read: codex_model_list_reply,
        }),
    ),
    (
        "cursor-agent",
        ModelSource::Ask(Ask::Argv {
            args: &["--list-models"],
            read: cursor_list_models,
        }),
    ),
    (
        "grok",
        ModelSource::Ask(Ask::Argv {
            args: &["models"],
            read: grok_models,
        }),
    ),
    (
        "hermes",
        // Verified 2026-08-21 from the installed CLI's own `--help`:
        // `hermes model` is documented as "Interactively select your
        // inference provider and default model" and its only list-shaped
        // flag is `--refresh`, which "re-fetch[es] every provider's live
        // /v1/models list" — a network walk behind a TUI, with no
        // non-interactive output. The list Hermes would offer rides ACP
        // `session/new` instead (comet reaches it that way,
        // `crates/harness/src/acp/mod.rs:877-928`), and eave's ACP core
        // does not expose a standalone probe today.
        ModelSource::Silence(
            "Hermes has no non-interactive model list: `hermes model` is a picker.",
        ),
    ),
    (
        "pi",
        ModelSource::Ask(Ask::Argv {
            args: &["--list-models"],
            read: pi_list_models,
        }),
    ),
];

/// The row for a provider, or `None` for one eave does not name.
pub fn model_source(provider_id: &str) -> Option<&'static ModelSource> {
    MODEL_SOURCES
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, s)| s)
}

/// Whether the list a caller is about to show came from the CLI or from
/// eave, and — when it came from eave — why.
///
/// The whole point of the enum is that a UI **cannot** render a list
/// without having been handed the answer to "who said so".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Nobody has asked, and nobody is going to: [`ModelSource::Silence`].
    /// The `&str` is that row's sentence.
    Unaskable(&'static str),
    /// Nobody has asked yet — the app has not armed probes, or the frame
    /// asking is the first one.
    Unasked,
    /// **eave will not spawn this provider, so it will not ask it either.**
    /// Codex is here today: its `model/list` answers instantly (verified
    /// live 2026-08-21), and eave still does not send it, because
    /// `codex.rs` declares its posture `Unproven` and a probe that ran
    /// anyway would be a way to start a refused CLI with a friendly name
    /// on it. The day that posture is proven this row lights up with no
    /// further code.
    Refused,
    /// A probe is out. eave's own list is on screen meanwhile.
    Asking,
    /// The CLI answered and named these. An empty list is still an answer:
    /// it means *this operator has no models on this CLI*, which is a
    /// different fact from eave not knowing.
    Reported { named: usize },
    /// The CLI was asked and did not answer — it failed to spawn, timed
    /// out, or printed nothing eave could read.
    Silent,
}

impl Freshness {
    /// Did the list on screen come from the installed CLI?
    pub fn is_from_the_cli(&self) -> bool {
        matches!(self, Freshness::Reported { .. })
    }
}

enum Probed {
    InFlight,
    Reported(&'static [&'static str]),
    Silent,
}

static PROBES: OnceLock<Mutex<HashMap<&'static str, Probed>>> = OnceLock::new();

fn probes() -> &'static Mutex<HashMap<&'static str, Probed>> {
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Set once by the app's `run`, exactly like `ui.rs`'s `DATA_DIR`.
///
/// **A probe is a spawn, and a test suite must never spawn a vendor CLI
/// because a render happened.** Nothing here starts a process until the
/// process that owns a real window says it may; the test harness never
/// calls `run`, so `model_choices` is inert under `cargo test` and every
/// test below drives the cache through [`record`] instead.
static ARMED: OnceLock<()> = OnceLock::new();

/// Allow probes to spawn. Idempotent; called once from the app's `run`.
pub fn arm() {
    let _ = ARMED.set(());
}

/// Is spawning allowed in this process?
pub fn is_armed() -> bool {
    ARMED.get().is_some()
}

/// What the CLI reported, if it has reported. **Never blocks and never
/// spawns** — this is a map lookup.
pub fn reported(provider_id: &str) -> Option<&'static [&'static str]> {
    match probes().lock().ok()?.get(provider_id) {
        Some(Probed::Reported(m)) => Some(m),
        _ => None,
    }
}

/// Who said what is on screen. See [`Freshness`].
pub fn freshness(provider_id: &str) -> Freshness {
    if let Some(ModelSource::Silence(why)) = model_source(provider_id) {
        return Freshness::Unaskable(why);
    }
    let Ok(map) = probes().lock() else {
        return Freshness::Unasked;
    };
    match map.get(provider_id) {
        Some(Probed::Reported(m)) => Freshness::Reported { named: m.len() },
        Some(Probed::InFlight) => Freshness::Asking,
        Some(Probed::Silent) => Freshness::Silent,
        None if !crate::session::posture_is_verified(provider_id) => Freshness::Refused,
        None => Freshness::Unasked,
    }
}

/// Record an answer against a provider.
///
/// The probe thread's own writer, and the seam every test below uses in
/// its place. Ids are leaked deliberately: a provider is probed at most
/// once per process, the cache outlives every caller anyway, and leaking
/// is what lets a picker hand out `&'static str` without the app's session
/// state growing an owned `String` it would then have to thread through
/// `AgentConfig`.
pub fn record(provider_id: &'static str, models: Vec<String>) {
    let leaked: &'static [&'static str] = Vec::leak(
        models
            .into_iter()
            .map(|m| &*Box::leak(m.into_boxed_str()))
            .collect::<Vec<&'static str>>(),
    );
    if let Ok(mut map) = probes().lock() {
        map.insert(provider_id, Probed::Reported(leaked));
    }
}

/// Drop everything learned, so the next [`begin`] asks again.
///
/// Mirrors `provider::forget_resolutions`, and exists for the same reason:
/// a process-lifetime memo that no test can reset is a test-order
/// dependency waiting to happen.
pub fn forget() {
    if let Ok(mut map) = probes().lock() {
        map.clear();
    }
}
/// May eave spawn this provider's CLI to ask it a question?
///
/// **Extracted as a predicate so it can be tested.** It used to be an
/// inline `if` inside [`begin`], and [`begin`] early-returns unless the
/// process is armed — which only `ui::run` does — so no test could reach
/// the gate at all. Mutation-checked 2026-08-21: deleting the inline
/// version broke **zero** tests. The neighbouring test
/// `a_provider_eave_would_not_spawn_is_not_probed_either` looked like
/// cover and was not; it exercises [`freshness`], the *display* path.
///
/// A gate on a security boundary that no test can reach is a gate that
/// will be deleted by someone tidying up.
fn may_probe(id: &str) -> bool {
    crate::session::posture_is_verified(id)
}

/// Ask this provider's CLI what it can run, **without waiting for the
/// answer**. Returns immediately, every time.
///
/// Idempotent: the second call while a probe is out, and every call after
/// one has landed, does nothing. Silently does nothing at all when:
///
/// - the process is not [`arm`]ed (the test harness, and any tool that
///   only wants to read the catalog);
/// - eave would refuse to spawn this provider anyway
///   ([`crate::session::posture_is_verified`]) — a probe is not a way
///   around that gate;
/// - the row is [`ModelSource::Silence`];
/// - the binary is not installed.
pub fn begin(provider_id: &str) {
    if !is_armed() {
        return;
    }
    let Some((id, ModelSource::Ask(ask))) = MODEL_SOURCES.iter().find(|(id, _)| *id == provider_id)
    else {
        return;
    };
    // The gate, not a bypass of it. See the module doc.
    if !may_probe(id) {
        return;
    }
    let Some(entry) = crate::provider::catalog(id) else {
        return;
    };
    let Some(program) = crate::provider::resolve_program(entry)
        .path()
        .map(|p| p.to_path_buf())
    else {
        return;
    };
    {
        let Ok(mut map) = probes().lock() else {
            return;
        };
        if map.contains_key(id) {
            return;
        }
        map.insert(id, Probed::InFlight);
    }
    let ask: &'static Ask = ask;
    std::thread::spawn(move || {
        let answer = run_probe(&program, ask);
        if let Ok(mut map) = probes().lock() {
            map.insert(
                id,
                match answer {
                    Some(models) => Probed::Reported(Vec::leak(
                        models
                            .into_iter()
                            .map(|m| &*Box::leak(m.into_boxed_str()))
                            .collect::<Vec<&'static str>>(),
                    )),
                    None => Probed::Silent,
                },
            );
        }
    });
}

/// `None` = the CLI did not answer. `Some(vec![])` = it answered "none",
/// which is a different fact and is kept as one.
fn run_probe(program: &std::path::Path, ask: &'static Ask) -> Option<Vec<String>> {
    match ask {
        Ask::Argv { args, read } => {
            let out = Command::new(program)
                .args(*args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .ok()
                .and_then(wait_bounded)?;
            Some(read(&out))
        }
        Ask::Wire { args, write, read } => {
            let mut child = Command::new(program)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            {
                let stdin = child.stdin.as_mut()?;
                for line in *write {
                    if writeln!(stdin, "{line}").is_err() {
                        break;
                    }
                }
                let _ = stdin.flush();
            }
            let stdout = child.stdout.take()?;
            let deadline = Instant::now() + PROBE_TIMEOUT;
            let answer = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(models) = read(&line) {
                        return Some(models);
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                None
            });
            // The reader thread is detached rather than joined: a CLI that
            // never answers must not hold this thread, and killing the
            // child is what closes its stdout and lets the reader retire.
            let landed = loop {
                if answer.is_finished() {
                    break answer.join().ok().flatten();
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(POLL);
            };
            let _ = child.kill();
            let _ = child.wait();
            landed
        }
    }
}

/// How often a probe checks whether its child has finished. Small enough
/// that a fast CLI is not held (grok answers in ~2.6s, codex in well under
/// one), large enough that a stalled one costs no measurable CPU.
const POLL: Duration = Duration::from_millis(25);

/// Wait for a child, killing it at [`PROBE_TIMEOUT`]. Returns stdout and
/// stderr joined — `pi` prints its refusal on stdout and `cursor-agent`
/// prints its own on stderr, and the parsers reject both.
fn wait_bounded(mut child: Child) -> Option<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
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

/// Drop CSI/OSC escape sequences. Three of these CLIs colour their lists
/// (`cursor-agent` uses chalk cyan/green/dim, `grok` its own palette) and
/// a colour code is not part of a model id.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `grok models`.
///
/// **Verified live 2026-08-21** against grok 1.0.5 on this machine, which
/// printed:
///
/// ```text
/// Default model: grok-4.6
///
/// Available models:
///   * grok-4.6 (default)
///   - grok-4.5
/// ```
///
/// waku parses the identical shape
/// (`crates/waku-core/src/model_catalog.rs:412-451`), which is the second
/// witness that the `*`/`-` marking is the format and not this machine's
/// accident.
fn grok_models(out: &str) -> Vec<String> {
    let cleaned = strip_ansi(out);
    let mut listing = false;
    let mut ids = Vec::new();
    for line in cleaned.lines() {
        let line = line.trim();
        if line == "Available models:" {
            listing = true;
            continue;
        }
        if !listing {
            continue;
        }
        let Some(entry) = line.strip_prefix('*').or_else(|| line.strip_prefix('-')) else {
            continue;
        };
        let id = entry
            .trim()
            .strip_suffix("(default)")
            .unwrap_or(entry.trim())
            .trim();
        if !id.is_empty() && !id.chars().any(char::is_whitespace) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// `pi --list-models`.
///
/// **Verified 2026-08-21 from the installed package's own shipped
/// source**, not from running it: pi 0.84.2's
/// `dist/cli/list-models.js.map` carries `src/cli/list-models.ts`, which
/// prints a `padEnd` table joined by two spaces under the header
/// `provider  model  context  max-out  thinking  images`, and prints
/// `formatNoModelsAvailableMessage()` — no table at all — when the
/// operator has authenticated no provider.
///
/// eave's `--model` takes `provider/id` (`pi.rs::MODELS`), so the two
/// leading columns are rejoined with a slash. **Running it live on this
/// machine took the empty branch**, which is the whole finding: the six
/// ids the launcher hardcoded are not available to this operator.
fn pi_list_models(out: &str) -> Vec<String> {
    let cleaned = strip_ansi(out);
    let mut listing = false;
    let mut ids = Vec::new();
    for line in cleaned.lines() {
        let mut cols = line.split_whitespace();
        let (Some(a), Some(b)) = (cols.next(), cols.next()) else {
            continue;
        };
        if !listing {
            // The header is the only line that reads exactly
            // "provider model …", and a model id column cannot collide
            // with it because pi ids are never the literal "model".
            listing = a == "provider" && b == "model";
            continue;
        }
        ids.push(format!("{a}/{b}"));
    }
    ids
}

/// `cursor-agent --list-models`.
///
/// **Verified 2026-08-21 from the installed bundle**
/// (`~/.local/share/cursor-agent/versions/2026.08.11-e8db854/5408.index.js`),
/// whose printer emits a dim `Available models` header, a blank line, then
/// one row per model shaped `<displayModelId>` optionally followed by
/// ` - <displayName>` and optionally ` (current, default)`, then a
/// `Tip: use --model <id> …` footer — and
/// `"No models available for this account."` when there are none.
///
/// Running it live on this machine printed `Error: Authentication
/// required.` instead, so the *format* is verified from the artefact while
/// a *populated* run is not. Every non-conforming line is dropped, which
/// is why that error yields an empty answer rather than a fake model.
///
/// synara's CLI parser splits on the same `" - "`
/// (`apps/server/src/provider/acp/CursorAcpSupport.ts:368-402`); waku's
/// (`model_catalog.rs:198-246`) expects `*`/`-` bullets, which this
/// version does not print — a reminder that the sibling is evidence about
/// its own pinned version, not about the binary on this disk.
fn cursor_list_models(out: &str) -> Vec<String> {
    let cleaned = strip_ansi(out);
    let mut ids = Vec::new();
    for line in cleaned.lines() {
        let line = line.trim();
        if line.is_empty()
            || line == "Available models"
            || line.starts_with("Tip:")
            || line.starts_with("Error:")
            || line.starts_with("No models available")
        {
            continue;
        }
        let head = line.split(" - ").next().unwrap_or(line).trim();
        // A trailing "(current)" / "(default)" / "(current, default)".
        let head = match head.split_once(" (") {
            Some((id, rest)) if rest.ends_with(')') => id,
            _ => head,
        };
        let head = head.trim();
        if head.is_empty() || head.split_whitespace().count() != 1 {
            continue;
        }
        ids.push(head.to_string());
    }
    ids
}

/// One line of Claude Code's stream-json output; `Some` for the
/// `control_response` that answers `list_models`.
///
/// `value` is taken and `supportedEffortLevels` is deliberately dropped —
/// see [`tests::a_probe_never_crosses_a_model_with_its_effort_ladder`] and
/// the note there about comet's reversal.
fn claude_list_models_reply(line: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let response = v.get("response")?;
    if response.get("subtype").and_then(Value::as_str) != Some("success") {
        return None;
    }
    Some(
        response
            .get("response")?
            .get("models")?
            .as_array()?
            .iter()
            .filter_map(|m| m.get("value").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
    )
}

/// One line of the Codex app-server's output; `Some` for the reply to the
/// `model/list` request [`CODEX_MODEL_LIST`] sends as id 2.
///
/// Only the first page is read. `nextCursor` was `null` on this machine
/// with five models, and a picker that pages is a picker that waits —
/// which is the thing this module exists not to do.
fn codex_model_list_reply(line: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("id").and_then(Value::as_u64) != Some(2) {
        return None;
    }
    Some(
        v.get("result")?
            .get("data")?
            .as_array()?
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is process-global, so the tests that clear it take turns.
    /// Rust runs a suite in parallel by default and two of these would
    /// otherwise race through each other's `forget()`.
    static CACHE: Mutex<()> = Mutex::new(());

    /// Every provider eave names states where its models come from, one
    /// way or the other. The point of the table is that there is no third
    /// option — a provider cannot silently have no answer.
    #[test]
    fn every_catalogued_provider_states_where_its_models_come_from() {
        for e in crate::provider::CATALOG {
            assert!(
                model_source(e.id).is_some(),
                "{} is catalogued but names no model source — add an Ask or a Silence",
                e.id,
            );
        }
        for (id, _) in MODEL_SOURCES {
            assert!(
                crate::provider::catalog(id).is_some(),
                "{id} names a model source but is not a provider eave catalogues",
            );
        }
        assert_eq!(MODEL_SOURCES.len(), crate::provider::CATALOG.len());
    }

    /// A `Silence` row must say why, in a sentence an operator can read.
    /// "Silence is the honest answer, not a placeholder" only holds if the
    /// silence is explained.
    #[test]
    fn silence_names_its_reason() {
        for (id, source) in MODEL_SOURCES {
            if let ModelSource::Silence(why) = source {
                assert!(
                    why.len() > 20 && why.ends_with('.'),
                    "{id}'s silence is not a sentence: {why:?}",
                );
            }
        }
    }

    /// **A probe is a spawn, and it may not be a hole around the posture
    /// gate.** No probe argv may carry a flag
    /// `session::conflicts_with_eave_flags` refuses from an operator's own
    /// `extra_args`, nor any of the vendor bypasses eave never sends.
    #[test]
    fn no_probe_undoes_eaves_posture() {
        // The bypass flags across the six CLIs, by their own `--help`.
        const NEVER: &[&str] = &[
            "--yolo",
            "--always-approve",
            "--force",
            "--trust",
            "--approve-mcps",
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--approve",
            "-a",
            "--full-auto",
            "--sandbox",
            "--add-dir",
        ];
        for (id, source) in MODEL_SOURCES {
            let ModelSource::Ask(ask) = source else {
                continue;
            };
            let args = match ask {
                Ask::Argv { args, .. } => *args,
                Ask::Wire { args, .. } => *args,
            };
            for arg in args {
                assert!(
                    !crate::session::conflicts_with_eave_flags(arg),
                    "{id}'s probe passes {arg}, which eave refuses from a session's own args",
                );
                assert!(
                    !NEVER.contains(arg),
                    "{id}'s probe passes {arg}, a permission bypass eave never sends",
                );
            }
            // And nothing a probe writes may be a user turn: the reason a
            // probe cannot cause a tool call is that no turn ever starts.
            if let Ask::Wire { write, .. } = ask {
                for line in *write {
                    let v: Value = serde_json::from_str(line)
                        .unwrap_or_else(|e| panic!("{id}'s probe writes non-JSON: {e}"));
                    assert_ne!(
                        v.get("type").and_then(Value::as_str),
                        Some("user"),
                        "{id}'s probe sends a user message, which starts a turn",
                    );
                    assert_ne!(
                        v.get("method").and_then(Value::as_str),
                        Some("session/prompt"),
                        "{id}'s probe prompts, which starts a turn",
                    );
                }
            }
        }
    }

    /// Grok's real output, captured 2026-08-21 from `grok models` on this
    /// machine.
    const GROK_OUT: &str = "You are logged in with grok.com.\n\nDefault model: grok-4.6\n\n\
                            Available models:\n  * grok-4.6 (default)\n  - grok-4.5\n";

    #[test]
    fn grok_names_the_models_grok_named() {
        assert_eq!(grok_models(GROK_OUT), vec!["grok-4.6", "grok-4.5"]);
    }

    #[test]
    fn grok_prose_before_the_listing_is_not_a_model() {
        // Everything above "Available models:" is a sentence, including a
        // line that happens to name the default model.
        assert!(grok_models("Default model: grok-4.6\n").is_empty());
        assert!(grok_models("").is_empty());
    }

    /// Pi's table, in the layout `src/cli/list-models.ts` builds.
    const PI_OUT: &str = "provider   model             context  max-out  thinking  images\n\
                          anthropic  claude-opus-4-5   200K     64K      yes       yes\n\
                          openai     gpt-5.3-codex     400K     128K     yes       yes\n";

    #[test]
    fn pi_carries_the_provider_in_the_model_id() {
        // `--model` takes `provider/id`; a bare id would silently fall
        // back to pi's default provider (google).
        assert_eq!(
            pi_list_models(PI_OUT),
            vec!["anthropic/claude-opus-4-5", "openai/gpt-5.3-codex"],
        );
    }

    #[test]
    fn pi_with_no_credentials_names_nothing() {
        // The live answer on this machine, verbatim.
        let out = "No models available. Use /login to log into a provider via OAuth or API key. \
                   See:\n  /opt/homebrew/lib/node_modules/x/docs/providers.md\n";
        assert!(
            pi_list_models(out).is_empty(),
            "pi's refusal must not be read as a model",
        );
    }

    /// Cursor's printer, in the shape `5408.index.js` emits it.
    const CURSOR_OUT: &str = "Available models\n\n\
                              gpt-5.2 - GPT-5.2 (current, default)\n\
                              claude-opus-4-8 - Claude Opus 4.8\n\
                              auto\n\n\
                              Tip: use --model <id> (or /model <id> in interactive mode) to switch.\n";

    #[test]
    fn cursor_takes_the_id_and_leaves_the_display_name() {
        assert_eq!(
            cursor_list_models(CURSOR_OUT),
            vec!["gpt-5.2", "claude-opus-4-8", "auto"],
        );
    }

    #[test]
    fn cursor_without_auth_names_nothing() {
        // The live answer on this machine, verbatim.
        let out =
            "Error: Authentication required. Run 'agent login', pass --api-key/--auth-token, \
                   or set CURSOR_API_KEY/CURSOR_AUTH_TOKEN.\n";
        assert!(cursor_list_models(out).is_empty());
    }

    /// Claude's real reply, trimmed to the fields eave reads. Captured
    /// 2026-08-21 from `claude` 2.1.x over the wire `claude.rs` speaks.
    const CLAUDE_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"eave-capabilities","response":{"models":[{"value":"default","resolvedModel":"claude-opus-5[1m]","displayName":"Default (recommended)","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"]},{"value":"opus[1m]","resolvedModel":"claude-opus-5[1m]","displayName":"Opus (1M context)","supportedEffortLevels":["low","medium","high"]},{"value":"claude-fable-5[1m]","resolvedModel":"claude-fable-5","displayName":"Fable"},{"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet"},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku"}]}}}"#;

    #[test]
    fn claude_names_the_models_claude_named() {
        assert_eq!(
            claude_list_models_reply(CLAUDE_REPLY),
            Some(vec![
                "default".to_string(),
                "opus[1m]".to_string(),
                "claude-fable-5[1m]".to_string(),
                "sonnet".to_string(),
                "haiku".to_string(),
            ]),
        );
    }

    /// **comet's reversal, kept.** `docs/research/acp.md:48-70` records
    /// that comet inverted its preference order on 2026-08-09 because the
    /// original one "put one row per model × effort in the picker":
    /// codex-acp enumerated `gpt-5.6-sol[low]`…`[ultra]` on the legacy
    /// surface while its `configOptions` carried base ids with effort as a
    /// separate axis. Zed deleted its model API outright over the same
    /// problem (`zed@c413552859`).
    ///
    /// eave reads flags and native wires rather than ACP, so the *order*
    /// does not transfer — but the rule underneath it does, and both
    /// surfaces eave reads carry the same hazard: Claude's reply has
    /// `supportedEffortLevels` beside every `value`, and Codex's has
    /// `supportedReasoningEfforts` beside every `id`. Crossing them would
    /// turn five Codex models into thirty rows. eave takes the base id and
    /// nothing else.
    #[test]
    fn a_probe_never_crosses_a_model_with_its_effort_ladder() {
        let claude = claude_list_models_reply(CLAUDE_REPLY).expect("claude reply parses");
        assert_eq!(claude.len(), 5, "one row per model, not per model × effort");
        for m in &claude {
            assert!(!m.contains("[low]"), "{m} carries an effort level");
        }
        let codex = codex_model_list_reply(CODEX_REPLY).expect("codex reply parses");
        assert_eq!(codex.len(), 2);
        for m in &codex {
            assert!(!m.contains("[low]"), "{m} carries an effort level");
        }
    }

    /// Codex's real reply, trimmed. Captured 2026-08-21 from
    /// `codex app-server` 0.148.0.
    const CODEX_REPLY: &str = r#"{"id":2,"result":{"data":[{"id":"gpt-5.6-sol","model":"gpt-5.6-sol","displayName":"GPT-5.6-Sol","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"max"}],"isDefault":true},{"id":"gpt-5.5","model":"gpt-5.5","displayName":"GPT-5.5","supportedReasoningEfforts":[{"reasoningEffort":"low"}],"isDefault":false}],"nextCursor":null}}"#;

    #[test]
    fn codex_answers_only_on_its_own_request_id() {
        assert_eq!(
            codex_model_list_reply(CODEX_REPLY),
            Some(vec!["gpt-5.6-sol".to_string(), "gpt-5.5".to_string()]),
        );
        // The `initialize` reply rides the same stream and must not be
        // mistaken for the answer.
        assert_eq!(
            codex_model_list_reply(r#"{"id":1,"result":{"codexHome":"/x"}}"#),
            None,
        );
        assert_eq!(
            codex_model_list_reply(r#"{"method":"remoteControl/status/changed","params":{}}"#),
            None,
        );
    }

    #[test]
    fn a_frame_that_is_not_the_answer_is_not_an_answer() {
        for line in [
            "",
            "not json",
            r#"{"type":"system","subtype":"init","session_id":"x"}"#,
            r#"{"type":"control_response","response":{"subtype":"error","error":"nope"}}"#,
        ] {
            assert_eq!(claude_list_models_reply(line), None, "{line}");
        }
    }

    #[test]
    fn colour_codes_are_not_part_of_a_model_id() {
        let painted =
            "Available models\n\n\u{1b}[36mgpt-5.2\u{1b}[39m - \u{1b}[2mGPT-5.2\u{1b}[22m\n";
        assert_eq!(cursor_list_models(painted), vec!["gpt-5.2"]);
    }

    /// The cache is the whole non-blocking story: what a frame reads, it
    /// reads from here, and an unasked provider reads as unasked rather
    /// than as "no models".
    #[test]
    fn an_unasked_provider_is_not_a_provider_with_no_models() {
        let _turn = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        forget();
        // Claude, because it is the one provider whose posture is proven —
        // an unproven one reads `Refused` and is a different fact, see
        // `a_provider_eave_would_not_spawn_is_not_probed_either`.
        assert_eq!(reported("claude-code"), None);
        assert_eq!(freshness("claude-code"), Freshness::Unasked);
        record("claude-code", vec!["sonnet".into()]);
        assert_eq!(reported("claude-code"), Some(&["sonnet"][..]));
        assert_eq!(freshness("claude-code"), Freshness::Reported { named: 1 });
        assert!(freshness("claude-code").is_from_the_cli());
        forget();
    }

    /// A CLI that answers "none" has answered. That is not the same state
    /// as a CLI that was never asked, and the picker says something
    /// different about each.
    #[test]
    fn naming_no_models_is_still_an_answer() {
        let _turn = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        forget();
        record("pi", vec![]);
        assert_eq!(freshness("pi"), Freshness::Reported { named: 0 });
        assert!(freshness("pi").is_from_the_cli());
        assert_eq!(reported("pi"), Some(&[][..]));
        forget();
    }

    /// **A probe is not a way around the posture gate.** Codex answers
    /// `model/list` in well under a second — verified live 2026-08-21
    /// against `codex` 0.148.0 — and eave still does not ask, because
    /// `codex.rs` declares `PostureProof::Unproven` and a discovery spawn
    /// of a CLI eave refuses to run for a session is the same spawn with a
    /// friendlier name. The picker says so rather than looking pending.
    #[test]
    fn a_provider_eave_would_not_spawn_is_not_probed_either() {
        let _turn = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        forget();
        for e in crate::provider::CATALOG {
            if crate::session::posture_is_verified(e.id) {
                continue;
            }
            if matches!(model_source(e.id), Some(ModelSource::Silence(_))) {
                continue;
            }
            assert_eq!(
                freshness(e.id),
                Freshness::Refused,
                "{} has no proven posture, so it must read as refused rather than pending",
                e.id,
            );
        }
        forget();
    }

    /// Hermes reads as unaskable at every moment, cache or no cache —
    /// there is nothing to wait for and the UI must not imply there is.
    #[test]
    fn an_unaskable_provider_never_reads_as_pending() {
        let _turn = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        forget();
        assert!(matches!(freshness("hermes"), Freshness::Unaskable(_)));
        assert!(!freshness("hermes").is_from_the_cli());
    }

    /// **The gate is not optional.** Nothing spawns in a process that has
    /// not been armed, which is why the suite above can call `begin`
    /// freely without a vendor CLI ever starting.
    #[test]
    fn an_unarmed_process_never_spawns_a_probe() {
        let _turn = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !is_armed(),
            "the test harness must never arm probes — it does not call run()",
        );
        forget();
        for e in crate::provider::CATALOG {
            begin(e.id);
        }
        for e in crate::provider::CATALOG {
            assert!(
                !matches!(freshness(e.id), Freshness::Asking),
                "{} started a probe in an unarmed process",
                e.id,
            );
        }
    }

    /// Live, opt-in, and `#[ignore]`d exactly like the other `live_*`
    /// tests: it needs the operator's real CLIs, real auth and a network.
    /// Run with `cargo test -p eave-core live_probe -- --ignored`.
    #[test]
    #[ignore = "spawns the operator's installed CLIs"]
    fn live_probe_reads_what_the_installed_clis_report() {
        arm();
        forget();
        for e in crate::provider::CATALOG {
            begin(e.id);
        }
        std::thread::sleep(PROBE_TIMEOUT + Duration::from_secs(1));
        for e in crate::provider::CATALOG {
            eprintln!("{}: {:?} {:?}", e.id, freshness(e.id), reported(e.id));
        }
    }

    /// **The gate itself, not the picker that reports it.**
    ///
    /// The test above asserts an unproven provider *reads* as refused.
    /// This asserts eave will not *spawn* it — a different claim, and the
    /// one that matters, since the first stays true even if the spawn
    /// gate is deleted. Verified by mutation: removing the gate leaves
    /// this failing and every other test in the module green.
    ///
    /// Written against the predicate rather than by arming the process,
    /// because a test that proves the gate by spawning the CLI when the
    /// gate is broken is a test that runs a refused CLI on regression.
    #[test]
    fn eave_will_not_spawn_a_cli_it_refuses_to_drive() {
        let mut checked = 0;
        for e in crate::provider::CATALOG {
            if crate::session::posture_is_verified(e.id) {
                assert!(may_probe(e.id), "{} is drivable, so it is askable", e.id);
            } else {
                assert!(
                    !may_probe(e.id),
                    "{} has no proven posture: a discovery spawn is that same \
                     spawn with a friendlier name",
                    e.id,
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no unproven provider in the catalog — this test would pass vacuously"
        );
    }
}
