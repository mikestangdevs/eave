//! Slice AR — the `/` palette's **discovery** half: what slash commands
//! this operator actually has, and what each one does.
//! Slice AR2 — and where that answer really comes from.
//!
//! `Shell::send_current` has always forwarded a `/`-prefixed message
//! verbatim, so the CLI has always resolved these. What was missing was
//! never plumbing — it was that **nothing in eave could name them**. This
//! module is that half, and it is deliberately pure: no gpui, no globals,
//! no `current_dir`, every path handed in. The half with the interesting
//! behaviour is the half that has to be testable without a window
//! (`overlay`'s ruling, taken again here).
//!
//! # Two sources, and only one of them is authoritative
//!
//! There are two functions here and they answer different questions.
//!
//! [`discover`] reads the disk. It is complete over things that have a
//! file, which is exactly the set that has a **description**, and it has
//! never offered a row the CLI did not have.
//!
//! [`offer`] decides what the palette shows, and it takes its NAMES from
//! the CLI's own `system/init` frame instead. That frame is a complete
//! inventory, it arrives before the model is invoked, and it is the
//! definition of what that CLI will accept — so precedence, collisions and
//! every future change to the naming convention are resolved inside the
//! CLI and arrive here already settled, rather than being re-derived in
//! eave and drifting from it. [`discover`]'s output is then matched to
//! those names to supply the descriptions.
//!
//! The frame is why AR's precedence work still matters and is not merely
//! preserved: when two files claim one name, its order is what decides
//! which file's description gets attached to the CLI's single row.
//!
//! # The cwd trap, stated once
//!
//! eave is one binary in two roles: the app spawns `eave --engine` as a
//! child, and **the engine is what holds a session's cwd**. The app's own
//! `std::env::current_dir()` is wherever the operator launched it from —
//! `/` for a bundle opened from Finder — and a previous slice shipped a
//! launcher that printed the app's cwd while opening in the engine's.
//!
//! So [`discover`] takes `session_cwd` as a parameter and this module
//! never asks the process where it is. `session_cwd` is
//! `SessionInfo::cwd`, which the app already has for the rail and which
//! the engine stamped. `home` is passed in for the same reason, and
//! because a test must be able to point it at a fixture.
//! `no_ambient_directory_is_ever_consulted` holds the rule on the source.
//!
//! # Six providers, one file format
//!
//! The operator has four agent CLIs installed and eave names six. They do
//! **not** need four parsers: skills have converged on one shape — a
//! directory holding `SKILL.md` with YAML frontmatter carrying `name` and
//! `description` — and that convergence is not a coincidence. Codex's own
//! vendored catalog names it: *"Agent Skills open standard"*,
//! `agentskills.io`. Grok's `~/.grok/bundled/skills/code-review/SKILL.md`
//! is byte-for-byte the same shape as a Claude skill.
//!
//! So what varies per provider is **the set of roots to scan**, and
//! nothing else. That is [`roots`], and it is the only provider-aware
//! thing in this file.
//!
//! **eave cannot start a non-Claude session today.** `launcher::PROVIDERS`
//! carries six agents and `models` is non-empty for exactly one; the other
//! five render "no adapter yet". So the Codex, Cursor and Grok roots below
//! are real, surveyed information that **nothing can currently exercise**,
//! and [`Evidence`] on each root says exactly how much is known about it.
//! Parameterising now is far cheaper than retrofitting; claiming it works
//! would be a lie.
//!
//! # Precedence
//!
//! Project (`<session_cwd>/.claude/…`) → user (`~/.claude/…`) → plugin,
//! first-wins on a name collision. That is the CLI's documented
//! resolution order and it is the order [`discover`] emits, so a caller
//! that keeps the first of each name gets it for free.
//!
//! # What each source can and cannot see (measured, 2026-08-20)
//!
//! Spawning `claude` with eave's exact headless argv from `/tmp` and
//! reading the first stdout line gives a `system/init` frame announcing
//! **68 slash commands** and 37 skills. Everything reachable from disk —
//! project, user, and the plugin install paths — accounts for **21** of
//! those 68. The other 47 (`context`, `compact`, `dataviz`, `simplify`,
//! `loop`, …) exist **nowhere on the filesystem**: they are bundled inside
//! the `claude` executable. `find ~/.claude
//! /opt/homebrew/lib/node_modules -name dataviz` returns nothing while
//! `/dataviz` is live in the CLI.
//!
//! A filesystem scan therefore cannot be complete, and [`discover`] does
//! not claim to be. It is complete over the things that have a `SKILL.md`
//! or a `.md` to read — which is exactly the set that has a
//! **description**. AR stopped there, on the ground that a row of bare
//! names is not discovery. That is a good objection to the wrong question:
//! it argues for showing those rows WITHOUT a description, not for hiding
//! them, and an operator who types `/` and does not see `/context`
//! concludes eave cannot do it.
//!
//! The frame is also **per session and cwd-sensitive**, which is why it
//! rides the session's own stream: planting one command and one skill
//! under a directory's `.claude/` took the same machine from 68 names to
//! 71 in the same minute.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where an entry came from. Carried so a row can say it — two `/verify`s
/// from different scopes are not the same command, and the palette must
/// not silently pick one and look decisive about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `<session_cwd>/.claude/…` — the SESSION's cwd, never the app's.
    Project,
    /// `~/.claude/…`
    User,
    /// A plugin's install path; the string is the plugin's own name, which
    /// is also the namespace its entries are addressed under.
    Plugin(String),
}

impl Origin {
    /// The one word a row shows for this scope.
    pub fn label(&self) -> &str {
        match self {
            Origin::Project => "project",
            Origin::User => "user",
            Origin::Plugin(name) => name,
        }
    }
}

/// A skill and a command are both reached by typing `/name`, and the CLI
/// puts both in one `slash_commands` list — but they are different things
/// on disk and a palette that conflated them could not explain itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory containing `SKILL.md`.
    Skill,
    /// A `.md` file under `commands/`.
    Command,
}

/// One row's worth of fact: a name, and whatever eave could find out
/// about it.
///
/// **Slice AR2 split this in two, and the split is the slice.** The name
/// and the facts now come from different places and only one of them is
/// authoritative. A name is what the operator types and what the CLI will
/// accept; [`Found`] is what a file on disk says about that name, and
/// there is often no file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashEntry {
    /// What the operator types after the `/`, WITHOUT the slash.
    pub name: String,
    /// What eave READ about this name, or `None` when it read nothing.
    ///
    /// `None` never comes out of [`discover`] — a scan only ever finds a
    /// name by reading the file that carries it. It comes out of [`offer`],
    /// for the names the CLI announced that no file eave scans backs: **46
    /// of the 67 rows** the palette offers on this machine. Those are
    /// bundled inside the `claude` executable, so there is nothing to read
    /// and nothing to say, and the row states its name alone rather than
    /// not existing.
    pub found: Option<Found>,
}

/// What one file on disk declares about a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The frontmatter `description`, whitespace-collapsed and capped.
    /// `None` = the file had no frontmatter or no description key. Kept as
    /// an `Option` rather than an empty string so the row can be honest
    /// about the difference between "does nothing worth saying" and "said
    /// nothing".
    pub description: Option<String>,
    pub origin: Origin,
    pub kind: Kind,
    /// The frontmatter said `disable-model-invocation: true` — the author
    /// declared this skill **operator-invoked only**, so the model will
    /// never reach for it on its own.
    ///
    /// Worth surfacing rather than dropping, because it is precisely the
    /// class a `/` palette exists for: a skill the agent cannot decide to
    /// use is one the operator must be able to find. Two of Grok's 22
    /// bundled skills declare it (`code-review`, `implement`).
    pub operator_only: bool,
}

impl SlashEntry {
    /// The text a send would carry — the slash included. The palette
    /// inserts exactly this, which is why it is derived here and not
    /// formatted at the call site.
    pub fn insertion(&self) -> String {
        format!("/{}", self.name)
    }

    /// What this command says it does, or `None` — either because no file
    /// declared one or because eave never found a file at all. Both render
    /// the same way (no second line) and neither is invented.
    pub fn description(&self) -> Option<&str> {
        self.found.as_ref()?.description.as_deref()
    }

    /// Skill or command, or `None` when eave has only the name.
    pub fn kind(&self) -> Option<Kind> {
        self.found.as_ref().map(|f| f.kind)
    }

    /// Which scope the description came from, or `None` when eave has only
    /// the name.
    pub fn origin(&self) -> Option<&Origin> {
        self.found.as_ref().map(|f| &f.origin)
    }

    /// A file declared this operator-invoked only. `false` also covers "no
    /// file said anything", which is the same rendering and the same
    /// non-claim.
    pub fn operator_only(&self) -> bool {
        self.found.as_ref().is_some_and(|f| f.operator_only)
    }
}

/// How much description a row keeps.
///
/// Not a display metric — the row truncates with an ellipsis in the
/// layout, which is CSS's job and does it at the real pixel width. This
/// is a *storage* bound, and it exists because the descriptions on this
/// machine are not short: `~/.claude/skills/framer/SKILL.md` carries a
/// **974-character** folded scalar, and the palette holds one
/// `SharedString` per row for as long as the app runs. 160 is roughly two
/// rendered lines at `DETAIL_SIZE` in a composer-width row, so nothing
/// that could ever be on screen is lost to it.
const DESCRIPTION_CAP: usize = 160;

/// How much is actually KNOWN about a root, kept in the table so no reader
/// mistakes a surveyed path for a verified one.
///
/// This project's rule is that a guess must never be presented as a fact.
/// Five of the six providers cannot be started by eave at all, so for most
/// of this table there is no CLI to check against — and saying so in the
/// type is cheaper than saying it in a comment nobody reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// Scanned on this machine **and** cross-checked against that CLI's
    /// own reported inventory: every entry this root produced is a command
    /// the CLI actually has. Only Claude's roots can be this, because only
    /// Claude reports an inventory eave can read.
    CrossChecked,
    /// Seen populated here and parsed cleanly, but never cross-checked —
    /// because eave cannot start that provider, so there is no inventory
    /// to check against. `~/.grok/bundled/skills` is the only one:
    /// 23 directories, 22 of them real skills.
    Populated,
    /// Never seen populated. Scanned anyway, because an absent directory
    /// costs nothing to scan and omitting a real one costs a missing row.
    /// **Not a fact.**
    Unverified,
}

/// One directory to scan, and what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    /// Which shape to read it as. A `skills/` root holds directories; a
    /// `commands/` root holds `.md` files.
    pub layout: Layout,
    pub origin: Origin,
    /// Prefix every entry under this root with `<namespace>:`. `Some` only
    /// for plugin roots.
    pub namespace: Option<String>,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `<root>/<name>/SKILL.md`, at any depth.
    Skills,
    /// `<root>/<name>.md`, subdirectories namespacing with a colon.
    Commands,
}

/// Every directory `provider` keeps operator-invocable things in, in
/// resolution order.
///
/// **This table is the slice's surveyed knowledge, and it is pinned by
/// `the_roots_table_is_what_was_actually_surveyed`.** Each row was checked
/// against this machine on 2026-08-20; the [`Evidence`] column is what
/// separates the two Claude roots that were cross-checked against the real
/// CLI from the Grok root that was only read, from the paths that were
/// merely believed.
///
/// Deliberately NOT in this table, with reasons:
///
///   - `~/.claude/plugins/marketplaces/` — a catalog of 30+ plugins the
///     operator never installed. Offering them offers rows the CLI lacks.
///   - `~/.codex/vendor_imports/skills/` — the same mistake wearing
///     another vendor's clothes. It is a clone of
///     `github.com/openai/skills` (confirmed: `git remote -v`), it holds
///     its skills under `skills/.curated/`, and its own README says
///     curated skills must be installed with `$skill-installer` before
///     Codex has them. The sibling `.system/` directory, which that README
///     says IS auto-installed, **does not exist here**. So Codex has zero
///     installed skills on this machine, and `~/.codex/skills` — which
///     exists and is empty — is the correct and only root.
///   - `~/.codex/prompts/`, `~/.cursor/commands/`, `~/.grok/commands/` —
///     these do not exist. If any of those providers has a commands
///     convention, it was not found, and inventing a path is how a palette
///     starts offering things that are not there.
///
/// `hermes` and `pi` get an empty list: eave names them, nothing about
/// their layout was surveyed, and an empty list is the honest answer.
pub fn roots(provider: &str, session_cwd: &Path, home: &Path) -> Vec<Root> {
    let root = |path: PathBuf, layout, origin, evidence| Root {
        path,
        layout,
        origin,
        namespace: None,
        evidence,
    };
    match provider {
        "claude-code" => {
            let mut v = vec![
                root(
                    session_cwd.join(".claude/skills"),
                    Layout::Skills,
                    Origin::Project,
                    Evidence::CrossChecked,
                ),
                root(
                    session_cwd.join(".claude/commands"),
                    Layout::Commands,
                    Origin::Project,
                    Evidence::CrossChecked,
                ),
                root(
                    home.join(".claude/skills"),
                    Layout::Skills,
                    Origin::User,
                    Evidence::CrossChecked,
                ),
                // The directory does not exist on this machine, so this
                // exact root produced nothing to check. Its SHAPE is the
                // same `.claude/commands` the project root uses, and that
                // one was cross-checked — but a shape argument is not a
                // sighting, so it is marked for what it is.
                root(
                    home.join(".claude/commands"),
                    Layout::Commands,
                    Origin::User,
                    Evidence::Unverified,
                ),
            ];
            for (name, install) in installed_plugins(home) {
                for (sub, layout) in [("skills", Layout::Skills), ("commands", Layout::Commands)] {
                    v.push(Root {
                        path: install.join(sub),
                        layout,
                        origin: Origin::Plugin(name.clone()),
                        namespace: Some(name.clone()),
                        evidence: Evidence::CrossChecked,
                    });
                }
            }
            v
        }
        // **`ls` says this directory is empty and it is not.** Codex keeps
        // its six auto-installed skills in a HIDDEN subdirectory,
        // `~/.codex/skills/.system/`, which is exactly what the vendored
        // catalog's README describes ("Skills in `.system` are
        // automatically installed"). A survey with `ls -1` reports zero
        // entries; `ls -a` and this scanner both find
        // `imagegen`, `openai-docs`, `plugin-creator`, `review-agent`,
        // `skill-creator`, `skill-installer`.
        //
        // That is why [`scan_skills`] recurses into dot-directories rather
        // than skipping them, and it is the reason the recursion is
        // load-bearing rather than a convenience for `craft-skills`'
        // grouping folders.
        "codex" => vec![root(
            home.join(".codex/skills"),
            Layout::Skills,
            Origin::User,
            Evidence::Populated,
        )],
        // `~/.cursor/agents` exists and is EMPTY, and there is no
        // `~/.cursor/commands`. Whether `agents/` even holds `SKILL.md`
        // directories is unknown — it has never been seen with anything in
        // it. Scanned because the failure mode of being wrong is an empty
        // list, which is what not scanning gives anyway.
        "cursor-agent" => vec![root(
            home.join(".cursor/agents"),
            Layout::Skills,
            Origin::User,
            Evidence::Unverified,
        )],
        "grok" => vec![
            root(
                home.join(".grok/skills"),
                Layout::Skills,
                Origin::User,
                Evidence::Unverified,
            ),
            // 23 directories, 22 real skills (`shared/` is support code
            // with no `SKILL.md`, and the scanner drops it for exactly
            // that reason). Read and parsed; never cross-checked, because
            // eave cannot start Grok.
            root(
                home.join(".grok/bundled/skills"),
                Layout::Skills,
                Origin::User,
                Evidence::Populated,
            ),
        ],
        // Named by the launcher, never surveyed. An empty list is the
        // honest answer; a guessed path is not.
        _ => Vec::new(),
    }
}

/// Everything this operator can reach, in resolution order.
///
/// `session_cwd` is the SESSION's working directory (`SessionInfo::cwd`),
/// never the app process's — see the module docs. `home` is the
/// operator's home directory.
///
/// Emitted project-first, then user, then plugins; within a scope, skills
/// before commands. Duplicate names are **kept**, not merged, and left in
/// precedence order — deduplication is the caller's choice, and a caller
/// that wants the CLI's answer takes the first of each name.
///
/// Never fails. A missing `.claude` is the common case (this machine has
/// no `~/.claude/commands` at all) and an unreadable one is not worth an
/// error path in a menu: the honest rendering of "could not read" and of
/// "nothing there" is the same empty list.
pub fn discover(provider: &str, session_cwd: &Path, home: &Path) -> Vec<SlashEntry> {
    let mut out = Vec::new();
    for root in roots(provider, session_cwd, home) {
        let ns = root.namespace.as_deref();
        match root.layout {
            Layout::Skills => scan_skills(&root.path, &root.origin, ns, &mut out),
            Layout::Commands => scan_commands(&root.path, "", &root.origin, ns, &mut out),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Slice AR2 — the CLI's own inventory, described by the scan

/// Names the palette refuses to offer even though the CLI announces them.
///
/// **Explicit, tiny, and every entry states its measurement.** The rule
/// this list exists to obey is that a carve-out must be stated: the
/// palette's whole claim is "this is what the CLI will accept", so every
/// subtraction from that has to be defensible in one sentence with a
/// measurement behind it, and a name nobody has measured stays on the
/// menu.
///
/// - **`clear`** — verified inert in eave's `-p` mode. Sent through eave's
///   exact argv on 2026-08-20 (and independently by Slice AR two days
///   earlier), the CLI replies with `{"type":"result","subtype":"success",
///   "is_error":false,"result":""}` and **no assistant turn at all**. The
///   second measurement is worse than the first: it also emits
///   `{"type":"conversation_reset","new_conversation_id":…}`, a top-level
///   type `stream.rs` has no arm for and surfaces as `Unknown`. So `/clear`
///   in eave does not merely do nothing visible — it silently forks the
///   CLI's conversation out from under a journal that keeps appending to
///   the old one. A palette row for it would offer a way to desynchronise
///   the session.
///
/// Not on this list, deliberately: `/compact`, `/init`, `/reload-skills`
/// and the rest of the destructive-sounding built-ins. They have not been
/// measured, several cannot be measured without side effects, and guessing
/// at them would put invented policy in a list whose entire value is that
/// it contains none. `/context` and `/model` were measured and **work** —
/// `/context` returned a 3129-character report through eave's own argv.
const NEVER_OFFERED: &[&str] = &["clear"];

/// What the palette offers: the CLI's own names, described by the scan.
///
/// **`inventory` is the source of truth for WHICH rows exist.** It is the
/// `slash_commands` array out of the agent's `system/init` frame, which the
/// CLI writes before the model is invoked and which is therefore, by
/// definition, the set of names that CLI will accept. Precedence,
/// collisions and every future change to the naming convention are settled
/// inside the CLI and arrive here already resolved, instead of being
/// re-derived in eave and drifting.
///
/// **`scanned` is the source of truth for WHAT A ROW SAYS.** The frame
/// carries names and nothing else, so a description has to come from the
/// file, matched by name. `scanned` arrives in [`discover`]'s precedence
/// order (project → user → plugin) and the FIRST match wins — which is
/// exactly what that ordering was built for: when two files claim one
/// name, the CLI resolves the one eave describes.
///
/// Order is the frame's own, unedited. It is not alphabetical; it is
/// registration order, and it puts the operator's own skills ahead of the
/// bundled built-ins, which is a better default than anything eave would
/// invent. It was byte-identical across four separate spawns from three
/// different directories on 2026-08-20, so rows do not move between
/// launches.
///
/// Duplicate names in `inventory` cannot happen — it is the CLI's resolved
/// list — but a duplicate would produce a duplicate row rather than being
/// silently dropped, because inventing a dedup rule for a case the CLI says
/// is impossible is how eave would end up hiding a real row.
pub fn offer(inventory: &[String], scanned: &[SlashEntry]) -> Vec<SlashEntry> {
    let mut described: std::collections::HashMap<&str, &Found> =
        std::collections::HashMap::with_capacity(scanned.len());
    for e in scanned {
        if let Some(found) = &e.found {
            described.entry(e.name.as_str()).or_insert(found);
        }
    }
    inventory
        .iter()
        .filter(|name| !NEVER_OFFERED.contains(&name.as_str()))
        .map(|name| SlashEntry {
            name: name.clone(),
            found: described.get(name.as_str()).map(|f| (*f).clone()),
        })
        .collect()
}

/// Skills: a directory containing `SKILL.md`, at any depth.
///
/// **The depth is not part of the name and the frontmatter `name` is not
/// the name either — the containing DIRECTORY is.** Both halves of that
/// are measured, not assumed:
///
///   - `craft-skills` lays its skills out as
///     `skills/craft/delete-this/SKILL.md`, and the CLI addresses it
///     `/craft-skills:delete-this`. The grouping directory `craft`
///     appears nowhere in the command.
///   - a probe skill in `skills/dirnamexyz/` whose frontmatter said
///     `name: frontmatternameabc` came back in the CLI's own
///     `slash_commands` as **`dirnamexyz`**. The frontmatter name is
///     ignored for addressing.
///
/// So: recurse to find `SKILL.md`, name the entry after its immediate
/// parent directory, and read the frontmatter for the description only.
/// How deep the skill walk goes below a root.
///
/// The deepest real layout measured is **two**:
/// `~/.codex/skills/.system/imagegen/` and
/// `<plugin>/skills/craft/delete-this/`. Four is that plus slack for a
/// vendor who nests one level further, and it is a *bound* rather than a
/// tuning knob — its job is to stop a walk, not to admit one. Without it a
/// checkout that happened to sit inside a skills root would be walked in
/// full.
const MAX_SKILL_DEPTH: usize = 4;

/// Directories a skill never lives in, skipped by name.
///
/// Not cosmetic: dot-directories are walked on purpose (Codex's entire
/// inventory is inside one), so the walk cannot simply skip anything
/// hidden — and `~/.codex/vendor_imports/skills` really does contain a
/// `.git`. These two names are where a recursive walk goes quadratic.
const NEVER_A_SKILL: &[&str] = &[".git", "node_modules"];

fn scan_skills(dir: &Path, origin: &Origin, namespace: Option<&str>, out: &mut Vec<SlashEntry>) {
    scan_skills_depth(dir, origin, namespace, 0, out);
}

fn scan_skills_depth(
    dir: &Path,
    origin: &Origin,
    namespace: Option<&str>,
    depth: usize,
    out: &mut Vec<SlashEntry>,
) {
    if depth > MAX_SKILL_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sorted, because `read_dir` order is the filesystem's and a menu
    // whose rows move between launches is not a menu. BTreeMap rather
    // than a sort afterwards so the recursion is ordered too.
    let mut dirs = BTreeMap::new();
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(name) = e.file_name().to_str() {
                if !NEVER_A_SKILL.contains(&name) {
                    dirs.insert(name.to_string(), e.path());
                }
            }
        }
    }
    for (name, path) in dirs {
        let skill_md = path.join("SKILL.md");
        if skill_md.is_file() {
            let front = Frontmatter::read(&skill_md);
            out.push(SlashEntry {
                name: namespaced(namespace, &name),
                found: Some(Found {
                    description: front.description,
                    origin: origin.clone(),
                    kind: Kind::Skill,
                    operator_only: front.operator_only,
                }),
            });
        } else {
            // A grouping directory — `skills/craft/` for a plugin,
            // `skills/.system/` for Codex. Not a skill itself, and its own
            // name never reaches a command.
            scan_skills_depth(&path, origin, namespace, depth + 1, out);
        }
    }
}

/// Commands: a `.md` file whose basename is the command.
///
/// A subdirectory namespaces with a colon — `commands/sub/nested.md` is
/// `/sub:nested`, verified against the real CLI. That composes with a
/// plugin's own namespace the same way, so a plugin's `sub/nested.md`
/// would be `/plugin:sub:nested`; nothing on this machine has one, and it
/// is the only part of the naming here that is inferred rather than
/// measured.
fn scan_commands(
    dir: &Path,
    prefix: &str,
    origin: &Origin,
    namespace: Option<&str>,
    out: &mut Vec<SlashEntry>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = BTreeMap::new();
    let mut subdirs = BTreeMap::new();
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                subdirs.insert(name, e.path());
            }
            Ok(t) if t.is_file() => {
                if let Some(stem) = name.strip_suffix(".md") {
                    files.insert(stem.to_string(), e.path());
                }
            }
            _ => {}
        }
    }
    for (stem, path) in files {
        let front = Frontmatter::read(&path);
        out.push(SlashEntry {
            name: namespaced(namespace, &format!("{prefix}{stem}")),
            found: Some(Found {
                description: front.description,
                origin: origin.clone(),
                kind: Kind::Command,
                operator_only: front.operator_only,
            }),
        });
    }
    for (name, path) in subdirs {
        scan_commands(&path, &format!("{prefix}{name}:"), origin, namespace, out);
    }
}

fn namespaced(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{ns}:{name}"),
        None => name.to_string(),
    }
}

/// The installed plugins, as `(name, install_path)` pairs, sorted by name.
///
/// Read from `~/.claude/plugins/installed_plugins.json`, whose keys are
/// `plugin@marketplace` and whose values carry the `installPath`. The
/// plugin's NAME — the part before the `@` — is the namespace its entries
/// are addressed under (`craft-skills@craft-skills` →
/// `/craft-skills:seams`).
///
/// **Deliberately not `~/.claude/plugins/marketplaces/`.** That directory
/// is a *catalog* — on this machine it holds 30+ plugins the operator has
/// never installed — and offering them would put rows in the palette that
/// the CLI does not have.
///
/// Hand-parsed rather than given a serde type, because the shape is
/// somebody else's and it has already been through a `"version": 2`. A
/// structural parse that skips what it does not recognise degrades to
/// "fewer rows"; a typed one degrades to "no rows at all" the first time
/// the file grows a key.
fn installed_plugins(home: &Path) -> Vec<(String, PathBuf)> {
    let path = home.join(".claude/plugins/installed_plugins.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(plugins) = v.get("plugins").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut out = BTreeMap::new();
    for (key, installs) in plugins {
        let name = key.split('@').next().unwrap_or(key).to_string();
        // The value is an array of installs (a plugin can be installed at
        // more than one scope). Take the first that names a path — the
        // CLI's own `init` frame reports exactly one path per plugin.
        let Some(path) = installs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.get("installPath").and_then(|p| p.as_str()))
            .next()
        else {
            continue;
        };
        out.insert(name, PathBuf::from(path));
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Frontmatter

/// The `description` from a markdown file's YAML frontmatter, collapsed to
/// one line and capped at [`DESCRIPTION_CAP`].
///
/// A YAML *subset*, not a parser: top-level `key: value`, plus folded
/// (`>`) and literal (`|`) block scalars, which is what the files on this
/// machine actually use — `framer`'s description is a `>` block spanning
/// four lines. Anything richer is not worth a dependency for one string,
/// and the failure mode of not understanding a construct is `None`, which
/// the row already knows how to render.
/// What one `SKILL.md` (or command `.md`) declares about itself.
///
/// One struct because it is **one read**: the file is opened once and both
/// fields come off the same string. Two `fs::read_to_string` calls per
/// entry would double the syscalls on a scan that already walks a plugin
/// tree, for no gain.
struct Frontmatter {
    description: Option<String>,
    operator_only: bool,
}

impl Frontmatter {
    fn read(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self {
                description: None,
                operator_only: false,
            };
        };
        let description = frontmatter_value(&text, "description")
            .map(|raw| raw.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|c| !c.is_empty())
            .map(|c| cap(&c, DESCRIPTION_CAP));
        // A file that does not mention the key has declared nothing, which
        // is not the same fact as declaring `false` — but it renders
        // identically and does not earn a third state.
        let operator_only = frontmatter_value(&text, "disable-model-invocation")
            .is_some_and(|v| v.trim() == "true");
        Self {
            description,
            operator_only,
        }
    }
}

/// Cap at `limit` characters, at a word boundary, with an ellipsis.
///
/// Counts CHARS, not bytes, and slices on a char boundary — these strings
/// are full of em dashes and curly quotes, and a byte slice through one
/// panics.
fn cap(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let cut: String = s.chars().take(limit).collect();
    let head = match cut.rfind(' ') {
        // Only break at a space if it leaves most of the budget spent;
        // otherwise a single long token would collapse the row to almost
        // nothing.
        Some(ix) if ix >= limit / 2 => &cut[..ix],
        _ => cut.as_str(),
    };
    format!("{}…", head.trim_end_matches([' ', ',', ';', ':', '-']))
}

/// Pull one top-level key out of a `---`-delimited frontmatter block.
fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---\n").or_else(|| {
        // Tolerate a BOM or CRLF, which a file edited on another machine
        // will have and which would otherwise silently produce `None`.
        text.trim_start_matches('\u{feff}').strip_prefix("---\r\n")
    })?;
    let end = body
        .lines()
        .position(|l| l.trim_end() == "---" || l.trim_end() == "...")?;
    let mut lines = body.lines().take(end);
    while let Some(line) = lines.next() {
        // Top-level keys only: an indented `description:` belongs to some
        // nested mapping and is not this file's description.
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let value = rest.trim();
        // A block scalar (`>`, `|`, with any chomping/indent indicator) or
        // a bare `key:` — the value is the indented lines that follow.
        if value.is_empty() || value.starts_with('>') || value.starts_with('|') {
            let block: Vec<&str> = lines
                .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
                .map(str::trim)
                .collect();
            return Some(block.join(" "));
        }
        return Some(unquote(value).to_string());
    }
    None
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

// ---------------------------------------------------------------------------
// The query

/// The palette's open/closed predicate AND its filter, in one function.
///
/// `Some(query)` = the palette is open on `query`; `None` = it is not
/// open. There is no separate "is the palette showing" bit anywhere,
/// deliberately: **the palette is a projection of the composer's text**,
/// not a surface with a life of its own. A second bit would be a second
/// source of truth for one fact, and the two would disagree the first
/// time a draft was restored by a session switch.
///
/// Closes on whitespace, because a space means the operator has finished
/// choosing and started typing the command's argument — `/eaveprobecmd
/// some arg` is a real thing the CLI accepts, and a menu still covering
/// the composer at that point is in the way.
pub fn palette_query(composer_text: &str) -> Option<&str> {
    let rest = composer_text.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// The rows for `query`, best match first, deduplicated by name.
///
/// Three ranks, and they are about where the typed letters LAND, not about
/// how many matched:
///
///   1. the name starts with the query — `/fra` → `framer`
///   2. the part after the last `:` starts with it — `/del` →
///      `craft-skills:delete-this`, which is the one that makes a
///      namespaced plugin skill reachable without typing its plugin
///   3. the query appears anywhere in the name
///
/// Ties keep discovery order, which is precedence order, so a project
/// entry outranks a user one at equal quality.
///
/// Dedup is first-wins on the name for the same reason: the CLI resolves
/// one `/verify`, so the palette shows one.
pub fn filter<'a>(entries: &'a [SlashEntry], query: &str) -> Vec<&'a SlashEntry> {
    let q = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut ranked: Vec<(u8, usize, &SlashEntry)> = Vec::new();
    for (ix, e) in entries.iter().enumerate() {
        if !seen.insert(&e.name) {
            continue;
        }
        let lower = e.name.to_lowercase();
        let leaf = lower.rsplit(':').next().unwrap_or(&lower);
        let rank = if q.is_empty() || lower.starts_with(&q) {
            0
        } else if leaf.starts_with(&q) {
            1
        } else if lower.contains(&q) {
            2
        } else {
            continue;
        };
        ranked.push((rank, ix, e));
    }
    ranked.sort_by_key(|(rank, ix, _)| (*rank, *ix));
    ranked.into_iter().map(|(_, _, e)| e).collect()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The one provider eave can actually start.
    const CLAUDE: &str = "claude-code";

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A scratch tree that removes itself.
    ///
    /// Hand-rolled on `std::env::temp_dir()` rather than pulling in
    /// `tempfile`, which is the idiom `eave-core::store` and
    /// `eave-engine::git` already use in their own tests. The counter is
    /// per-process and the pid is in the name, so two tests on two threads
    /// (which is how `#[gpui::test]` runs) cannot collide.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("eave-slash-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fixture laid out the way this machine actually is — including the
    /// two shapes that were measured and would otherwise have been
    /// guessed wrong: a plugin whose skills sit under a GROUPING directory,
    /// and a skill whose frontmatter `name` disagrees with its directory.
    fn fixture() -> TempTree {
        let td = TempTree::new("fixture");
        let root = td.path();
        let (proj, home) = (root.join("proj"), root.join("home"));

        write(
            &proj.join(".claude/skills/projskill/SKILL.md"),
            "---\nname: projskill\ndescription: a project skill\n---\nbody\n",
        );
        write(
            &proj.join(".claude/commands/projcmd.md"),
            "---\ndescription: a project command\n---\nbody\n",
        );
        write(
            &proj.join(".claude/commands/sub/nested.md"),
            "---\ndescription: a nested command\n---\nbody\n",
        );

        write(
            &home.join(".claude/skills/userskill/SKILL.md"),
            "---\nname: userskill\ndescription: >\n  a user skill whose description\n  is a folded block\n---\nbody\n",
        );
        // The measured case: directory name wins over frontmatter name.
        write(
            &home.join(".claude/skills/dirnamewins/SKILL.md"),
            "---\nname: notthisone\ndescription: named by its directory\n---\nbody\n",
        );
        // Precedence: same name as the project's, and must lose.
        write(
            &home.join(".claude/skills/projskill/SKILL.md"),
            "---\ndescription: the USER copy, which must lose\n---\nbody\n",
        );

        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &format!(
                r#"{{"version":2,"plugins":{{"pack@market":[{{"scope":"user","installPath":"{}"}}]}}}}"#,
                home.join("plug").display()
            ),
        );
        // The measured layout: skills/<group>/<name>/SKILL.md, and the
        // group is NOT in the command name.
        write(
            &home.join("plug/skills/group/packed/SKILL.md"),
            "---\ndescription: a grouped plugin skill\n---\nbody\n",
        );
        td
    }

    fn scanned(td: &TempTree) -> Vec<SlashEntry> {
        discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"))
    }

    /// A row whose only fact is its name — the shape [`offer`] produces
    /// for a command bundled inside the agent's own binary.
    fn named(name: &str) -> SlashEntry {
        SlashEntry {
            name: name.into(),
            found: None,
        }
    }

    #[test]
    fn a_project_skill_and_a_project_command_are_both_found_with_their_descriptions() {
        let td = fixture();
        let all = scanned(&td);
        let skill = all
            .iter()
            .find(|e| e.name == "projskill" && e.origin() == Some(&Origin::Project))
            .expect("project skill");
        assert_eq!(skill.kind(), Some(Kind::Skill));
        assert_eq!(skill.description(), Some("a project skill"));

        let cmd = all.iter().find(|e| e.name == "projcmd").expect("command");
        assert_eq!(cmd.kind(), Some(Kind::Command));
        assert_eq!(cmd.origin(), Some(&Origin::Project));
        assert_eq!(cmd.description(), Some("a project command"));
    }

    /// Verified against the real CLI (2.1.226): a probe command at
    /// `.claude/commands/sub/nested.md` came back on stdout as
    /// `<command-name>/sub:nested</command-name>` and ran.
    #[test]
    fn a_command_in_a_subdirectory_namespaces_with_a_colon() {
        let td = fixture();
        let all = scanned(&td);
        let n = all.iter().find(|e| e.name == "sub:nested").expect("nested");
        assert_eq!(n.description(), Some("a nested command"));
    }

    /// The one that would have been guessed wrong. A skill directory named
    /// `dirnamexyz` whose frontmatter said `name: frontmatternameabc` was
    /// reported by the CLI's own `system/init` frame as `dirnamexyz`.
    #[test]
    fn a_skill_is_named_by_its_directory_not_by_its_frontmatter() {
        let td = fixture();
        let all = scanned(&td);
        assert!(
            all.iter().any(|e| e.name == "dirnamewins"),
            "the directory name is the command; got {:?}",
            all.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(
            !all.iter().any(|e| e.name == "notthisone"),
            "the frontmatter name must not become a command"
        );
    }

    /// `craft-skills` really is laid out `skills/craft/delete-this/` and
    /// the CLI really does call it `/craft-skills:delete-this` — the
    /// grouping directory is not in the name.
    #[test]
    fn a_plugin_skill_takes_the_plugins_namespace_and_not_its_grouping_directory() {
        let td = fixture();
        let all = scanned(&td);
        let p = all
            .iter()
            .find(|e| e.name == "pack:packed")
            .unwrap_or_else(|| {
                panic!(
                    "expected pack:packed; got {:?}",
                    all.iter().map(|e| &e.name).collect::<Vec<_>>()
                )
            });
        assert_eq!(p.origin(), Some(&Origin::Plugin("pack".into())));
        assert_eq!(p.description(), Some("a grouped plugin skill"));
        assert!(
            !all.iter().any(|e| e.name.contains("group")),
            "the grouping directory must not appear in any name"
        );
    }

    #[test]
    fn project_outranks_user_for_the_same_name() {
        let td = fixture();
        let all = scanned(&td);
        let picked = filter(&all, "projskill");
        assert_eq!(
            picked.len(),
            1,
            "one /projskill reaches the CLI, so one row"
        );
        assert_eq!(picked[0].origin(), Some(&Origin::Project));
        assert_eq!(picked[0].description(), Some("a project skill"));
    }

    #[test]
    fn a_folded_block_description_is_read_and_collapsed_to_one_line() {
        let td = fixture();
        let all = scanned(&td);
        let u = all.iter().find(|e| e.name == "userskill").expect("user");
        assert_eq!(
            u.description(),
            Some("a user skill whose description is a folded block")
        );
    }

    #[test]
    fn a_missing_claude_directory_is_an_empty_list_and_not_a_panic() {
        let td = TempTree::new("case");
        assert!(discover(CLAUDE, &td.path().join("nope"), &td.path().join("alsonope")).is_empty());
    }

    /// `~/.claude/plugins/marketplaces/` is a CATALOG, not an inventory —
    /// it holds 30+ plugins this operator has never installed. A palette
    /// that offered them would offer rows the CLI does not have.
    #[test]
    fn a_plugin_that_is_only_in_a_marketplace_is_not_offered() {
        let td = fixture();
        let home = td.path().join("home");
        write(
            &home
                .join(".claude/plugins/marketplaces/mkt/plugins/uninstalled/skills/ghost/SKILL.md"),
            "---\ndescription: never installed\n---\nbody\n",
        );
        let all = scanned(&td);
        assert!(
            !all.iter().any(|e| e.name.contains("ghost")),
            "an uninstalled marketplace plugin must not reach the palette"
        );
    }

    #[test]
    fn the_palette_opens_only_on_a_leading_slash_and_closes_on_a_space() {
        assert_eq!(palette_query("/fra"), Some("fra"));
        assert_eq!(palette_query("/"), Some(""));
        assert_eq!(palette_query("hello"), None);
        assert_eq!(palette_query(" /fra"), None);
        assert_eq!(palette_query("what is /fra"), None);
        // The argument case: the operator has chosen and is typing an arg.
        assert_eq!(palette_query("/eaveprobecmd some arg"), None);
        assert_eq!(palette_query("/fra\n"), None);
    }

    #[test]
    fn the_leaf_of_a_namespaced_name_is_reachable_without_typing_the_plugin() {
        let td = fixture();
        let all = scanned(&td);
        let hits = filter(&all, "packed");
        assert_eq!(
            hits.first().map(|e| e.name.as_str()),
            Some("pack:packed"),
            "typing the leaf must reach the namespaced entry"
        );
    }

    #[test]
    fn a_prefix_match_outranks_a_match_in_the_middle() {
        let entries = vec![named("unprojected"), named("proj")];
        let hits = filter(&entries, "proj");
        assert_eq!(hits[0].name, "proj");
        assert_eq!(hits[1].name, "unprojected");
    }

    #[test]
    fn an_empty_query_offers_everything_in_precedence_order() {
        let td = fixture();
        let all = scanned(&td);
        let hits = filter(&all, "");
        assert_eq!(hits.len(), all.len() - 1, "one duplicate name is merged");
        assert_eq!(hits[0].origin(), Some(&Origin::Project));
    }

    #[test]
    fn insertion_puts_the_slash_back() {
        let e = named("craft-skills:seams");
        assert_eq!(e.insertion(), "/craft-skills:seams");
    }

    /// A description is capped for STORAGE — the row truncates in layout,
    /// and this only keeps a 974-character folded scalar (which
    /// `~/.claude/skills/framer/SKILL.md` really has) out of a
    /// `SharedString` that lives as long as the app.
    #[test]
    fn a_very_long_description_is_capped_at_a_word_boundary() {
        let td = TempTree::new("case");
        let long = "word ".repeat(400);
        write(
            &td.path().join("proj/.claude/skills/verbose/SKILL.md"),
            &format!("---\ndescription: {long}\n---\nbody\n"),
        );
        let all = discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"));
        let d = all[0].description().unwrap();
        assert!(
            d.chars().count() <= 161,
            "capped at 160 chars plus the ellipsis; got {}",
            d.chars().count()
        );
        assert!(d.ends_with('…'), "a cut description says it was cut: {d:?}");
        assert!(d.starts_with("word word"));
    }

    /// Multibyte, because every description on this machine is full of em
    /// dashes and curly quotes and a BYTE slice through one panics.
    #[test]
    fn capping_a_multibyte_description_does_not_panic() {
        let td = TempTree::new("case");
        write(
            &td.path().join("proj/.claude/skills/dashes/SKILL.md"),
            &format!("---\ndescription: {}\n---\n", "—curly’quote—".repeat(40)),
        );
        let all = discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"));
        assert!(all[0].description().is_some());
    }

    #[test]
    fn a_file_with_no_frontmatter_yields_a_row_with_no_description() {
        let td = TempTree::new("case");
        write(
            &td.path().join("proj/.claude/commands/bare.md"),
            "just a prompt body, no frontmatter at all\n",
        );
        let all = discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "bare");
        assert_eq!(all[0].description(), None);
    }

    /// An indented `description:` belongs to some nested mapping — a
    /// tool's, an example's — and is not the file's own.
    #[test]
    fn an_indented_description_is_not_mistaken_for_the_files_own() {
        let td = TempTree::new("case");
        write(
            &td.path().join("proj/.claude/commands/nested.md"),
            "---\nname: nested\ntools:\n  description: not this one\n---\nbody\n",
        );
        let all = discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"));
        assert_eq!(all[0].description(), None);
    }

    /// Codex's entire inventory lives in a HIDDEN directory
    /// (`~/.codex/skills/.system/`), so the walk must enter dot-dirs — and
    /// having entered them it must still refuse `.git`, which really does
    /// sit inside one of the directories the survey looked at.
    #[test]
    fn the_walk_enters_hidden_directories_but_never_a_git_checkout() {
        let td = TempTree::new("hidden");
        let home = td.path().join("home");
        write(
            &home.join(".codex/skills/.system/imagegen/SKILL.md"),
            "---\ndescription: hidden but installed\n---\nbody\n",
        );
        write(
            &home.join(".codex/skills/.git/objects/decoy/SKILL.md"),
            "---\ndescription: must never be offered\n---\nbody\n",
        );
        let all = discover("codex", &td.path().join("proj"), &home);
        assert_eq!(
            all.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["imagegen"],
            "the .system grouping directory is entered and does not name \
             the command; .git is refused"
        );
    }

    /// The bound is a stop, not a tuning knob — the deepest real layout
    /// measured is two (`skills/.system/imagegen`,
    /// `skills/craft/delete-this`).
    #[test]
    fn the_skill_walk_stops_at_a_bounded_depth() {
        let td = TempTree::new("deep");
        let home = td.path().join("home");
        let mut p = home.join(".codex/skills");
        for _ in 0..8 {
            p = p.join("d");
        }
        write(
            &p.join("burrowed/SKILL.md"),
            "---\ndescription: too deep\n---\n",
        );
        assert!(discover("codex", &td.path().join("proj"), &home).is_empty());
    }

    /// **The surveyed roots table, pinned.**
    ///
    /// eave cannot start Codex, Cursor or Grok — `launcher::PROVIDERS`
    /// gives five of six providers an empty `models` list and the launcher
    /// says "no adapter yet". So nothing in the app can exercise these
    /// paths, and without this test the survey behind them would rot
    /// silently until the day an adapter lands.
    ///
    /// It asserts the SHAPE of the knowledge, not a directory listing:
    /// which roots exist per provider, in which order, and — the part that
    /// matters — that nothing claims more evidence than was actually
    /// gathered. Only Claude may claim `CrossChecked`, because Claude is
    /// the only CLI that reports an inventory eave was able to diff
    /// against.
    #[test]
    fn the_roots_table_is_what_was_actually_surveyed() {
        let (cwd, home) = (Path::new("/w"), Path::new("/h"));
        let names = |p: &str| -> Vec<String> {
            roots(p, cwd, home)
                .iter()
                .map(|r| r.path.display().to_string())
                .collect()
        };

        assert_eq!(
            names("claude-code"),
            vec![
                "/w/.claude/skills",
                "/w/.claude/commands",
                "/h/.claude/skills",
                "/h/.claude/commands",
            ],
            "project before user, skills before commands"
        );
        // Codex keeps a clone of github.com/openai/skills beside this, and
        // it is a CATALOG: its own README says curated skills must be
        // installed before Codex has them, and the `.system` directory it
        // says IS auto-installed does not exist here.
        assert_eq!(names("codex"), vec!["/h/.codex/skills"]);
        assert_eq!(
            roots("codex", cwd, home)[0].evidence,
            Evidence::Populated,
            "`ls -1` calls this empty; it holds six skills in a HIDDEN \
             .system/ subdirectory, which is why the walk enters \
             dot-directories"
        );
        assert!(
            !names("codex").iter().any(|p| p.contains("vendor_imports")),
            "the vendored openai/skills catalog must never be scanned — \
             it is the marketplace mistake wearing another vendor's clothes"
        );
        assert_eq!(names("cursor-agent"), vec!["/h/.cursor/agents"]);
        assert_eq!(
            names("grok"),
            vec!["/h/.grok/skills", "/h/.grok/bundled/skills"]
        );
        // Named by the launcher, never surveyed. An empty list is honest;
        // a guessed path is not.
        assert!(names("hermes").is_empty());
        assert!(names("pi").is_empty());
        assert!(names("not-a-provider").is_empty());

        for p in ["codex", "cursor-agent", "grok", "hermes", "pi"] {
            for r in roots(p, cwd, home) {
                assert_ne!(
                    r.evidence,
                    Evidence::CrossChecked,
                    "{p}'s {:?} cannot be cross-checked: eave cannot start \
                     {p}, so there is no inventory to check it against",
                    r.path
                );
            }
        }
        assert!(
            roots("grok", cwd, home)
                .iter()
                .any(|r| r.evidence == Evidence::Populated),
            "~/.grok/bundled/skills was read: 23 directories, 22 skills"
        );
    }

    /// The format really is one format. A Grok skill — `name`,
    /// `description`, `disable-model-invocation` — parses with the same
    /// reader as a Claude one, because both are the `agentskills.io`
    /// shape.
    #[test]
    fn a_grok_skill_parses_with_the_same_reader_as_a_claude_one() {
        let td = TempTree::new("grok");
        let home = td.path().join("home");
        // Copied from ~/.grok/bundled/skills/code-review/SKILL.md.
        write(
            &home.join(".grok/bundled/skills/code-review/SKILL.md"),
            "---\nname: code-review\ndescription: Run an extremely strict \
             maintainability review for abstraction quality.\n\
             disable-model-invocation: true\n---\nbody\n",
        );
        // `shared/` is support code with no SKILL.md, and must not become
        // a row.
        write(
            &home.join(".grok/bundled/skills/shared/personas/notes.md"),
            "x",
        );

        let all = discover("grok", &td.path().join("proj"), &home);
        assert_eq!(
            all.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["code-review"],
            "a directory with no SKILL.md anywhere under it is not a skill"
        );
        assert!(
            all[0].operator_only(),
            "the author declared this operator-invoked only, which is \
             exactly the class a / palette exists for"
        );
        assert!(all[0].description().is_some());
    }

    /// A skill that says nothing about model invocation has not declared
    /// `false` — but it renders the same, and does not earn a third state.
    #[test]
    fn a_skill_that_does_not_mention_model_invocation_is_not_operator_only() {
        let td = fixture();
        let all = scanned(&td);
        assert!(all.iter().all(|e| !e.operator_only()));
    }

    // -- Slice AR2: the CLI's own inventory --------------------------

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// **The slice.** The palette lists what the FRAME lists — including
    /// the names no file on disk backs — and the scan only describes them.
    ///
    /// The mix below is the shape of a real frame: a handful of things the
    /// operator has files for, and rather more that are compiled into the
    /// agent's own binary. On this machine that ratio is 21 to 46.
    #[test]
    fn the_palette_lists_what_the_cli_announces_and_the_scan_only_describes_it() {
        let td = fixture();
        let scan = scanned(&td);
        let inventory = names(&[
            "projskill",
            "context",
            "projcmd",
            "compact",
            "pack:packed",
            "dataviz",
        ]);
        let offered = offer(&inventory, &scan);

        assert_eq!(
            offered.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [
                "projskill",
                "context",
                "projcmd",
                "compact",
                "pack:packed",
                "dataviz"
            ],
            "every announced name is offered, in the frame's own order"
        );

        // A name with a file behind it keeps everything the scan read.
        assert_eq!(offered[0].description(), Some("a project skill"));
        assert_eq!(offered[0].kind(), Some(Kind::Skill));
        assert_eq!(offered[0].origin(), Some(&Origin::Project));
        assert_eq!(offered[4].description(), Some("a grouped plugin skill"));

        // A name with none states its name and claims nothing else. This
        // row is the whole point of AR2 and it is deliberately empty: eave
        // does not know what `/context` does and will not invent a line.
        assert_eq!(offered[1].name, "context");
        assert_eq!(offered[1].description(), None);
        assert_eq!(offered[1].kind(), None);
        assert_eq!(offered[1].origin(), None);
        assert!(!offered[1].operator_only());
        assert_eq!(
            offered[1].insertion(),
            "/context",
            "a row with no description is still a row that can be sent"
        );
    }

    /// A file on disk the CLI does not announce is **not** offered.
    ///
    /// This is the half that makes the frame the source of truth rather
    /// than a supplement, and it is the half that can cost a row: a skill
    /// added since the session last started is on disk and not in the
    /// frame, so it will not appear until that session runs again. The
    /// trade is deliberate. The frame is the definition of what the CLI
    /// will accept; a union with the scan would put eave back in the
    /// business of predicting that, which is the business AR2 exists to
    /// get it out of.
    #[test]
    fn a_file_the_cli_does_not_announce_is_not_offered() {
        let td = fixture();
        let scan = scanned(&td);
        assert!(
            scan.iter().any(|e| e.name == "projskill"),
            "the fixture's project skill is on disk"
        );
        let offered = offer(&names(&["projcmd"]), &scan);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].name, "projcmd");
    }

    /// **Slice AR's precedence work is what attributes a description to
    /// the right file**, and that is why it survives AR2 rather than being
    /// replaced by it.
    ///
    /// The CLI resolves one `/projskill` and announces it once. Two files
    /// claim that name — the project's and the user's — and only the
    /// project's is the one the CLI will run, so only the project's
    /// description may appear under it. Take the last match instead and
    /// the palette describes a file the agent will never execute.
    #[test]
    fn precedence_decides_which_file_describes_one_announced_name() {
        let td = fixture();
        let scan = scanned(&td);
        assert_eq!(
            scan.iter().filter(|e| e.name == "projskill").count(),
            2,
            "the fixture plants the collision this rule exists for"
        );
        let offered = offer(&names(&["projskill"]), &scan);
        assert_eq!(offered.len(), 1, "one announced name is one row");
        assert_eq!(offered[0].description(), Some("a project skill"));
        assert_eq!(offered[0].origin(), Some(&Origin::Project));
    }

    /// `/clear` is announced by the CLI, is accepted by the CLI, and does
    /// nothing eave can survive — so it is the one name subtracted from
    /// the frame's answer.
    ///
    /// Measured through eave's exact argv on 2026-08-20: the reply is a
    /// `result` whose text is the empty string, with no assistant turn,
    /// preceded by a `conversation_reset` frame that hands the CLI a new
    /// conversation id while eave's journal keeps appending to the old
    /// one. Offering it would offer a way to silently desynchronise a
    /// session.
    #[test]
    fn clear_is_never_offered_however_the_cli_announces_it() {
        let offered = offer(&names(&["context", "clear", "model"]), &[]);
        assert_eq!(
            offered.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["context", "model"],
            "the deny-list removes exactly its own entry and nothing beside it"
        );
    }

    /// **Every carve-out is stated, and the statement is machine-checked.**
    ///
    /// The palette's claim is "this is what your CLI will accept". Each
    /// subtraction from that weakens the claim, so each one has to carry a
    /// measurement in prose next to it — the same discipline
    /// `the_roots_table_is_what_was_actually_surveyed` applies to the
    /// roots. A future slice that quietly adds a name here fails this.
    ///
    /// The second half is the guard against creep: `/context` and `/model`
    /// were both measured working in `-p` mode (a 3129-character context
    /// report came back through eave's own argv), so neither may be
    /// filtered on a hunch that built-ins are risky.
    #[test]
    fn every_denied_name_states_its_evidence_and_the_measured_ones_stay() {
        let src = include_str!("slash.rs");
        for name in NEVER_OFFERED {
            // Assembled rather than written whole, so this test cannot
            // satisfy itself with its own source.
            let claim = format!("**`{name}`** —");
            assert!(
                src.lines()
                    .filter(|l| l.trim_start().starts_with("///"))
                    .any(|l| l.contains(&claim)),
                "/{name} is refused with no stated reason; a carve-out from \
                 the CLI's own inventory must say what was measured"
            );
        }
        for verified in ["context", "model"] {
            assert!(
                !NEVER_OFFERED.contains(&verified),
                "/{verified} was measured WORKING in headless mode and must \
                 not be filtered"
            );
        }
    }

    /// An announced name that no file backs is not a hole in the row — it
    /// is a row with one fact in it, and the renderer needs to be able to
    /// tell that apart from a file that simply had no `description:` key.
    ///
    /// Both render without a second line; only one of them may claim a
    /// kind or a scope.
    #[test]
    fn a_file_without_a_description_is_not_the_same_as_no_file() {
        let td = TempTree::new("nodesc");
        write(
            &td.path().join("proj/.claude/commands/quiet.md"),
            "just a body, no frontmatter at all\n",
        );
        let scan = discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"));
        let offered = offer(&names(&["quiet", "compact"]), &scan);

        assert_eq!(offered[0].description(), None);
        assert_eq!(
            offered[0].kind(),
            Some(Kind::Command),
            "a file that said nothing was still READ, and eave knows what it is"
        );
        assert_eq!(offered[1].description(), None);
        assert_eq!(
            offered[1].kind(),
            None,
            "a name with no file behind it must not borrow the other's kind"
        );
    }

    /// An INSTRUMENT, not an assertion: dump what [`discover`] finds for a
    /// real directory, so it can be diffed against the CLI's own
    /// `slash_commands` list.
    ///
    /// Ignored because its answer depends on the machine it runs on. The
    /// other side of the diff is obtained by spawning the CLI exactly the
    /// way `eave-core::claude` does, reading the first `system/init` line
    /// off stdout, and killing the process group before the model is asked
    /// anything — which costs nothing and is how every "the CLI addresses
    /// it THIS way" claim in this module was settled.
    ///
    /// Slice AR2 gave it the OTHER side of that diff: point
    /// `EAVE_SLASH_DUMP_INIT` at a file holding one captured `system/init`
    /// line and it runs the real [`offer`] against it and reports the
    /// split — how many rows the CLI announces, how many the scan can
    /// describe, and how many the scan finds that the CLI does not have
    /// (which must be zero).
    ///
    /// `EAVE_SLASH_DUMP_CWD=<dir> EAVE_SLASH_DUMP_INIT=<init.json> cargo
    /// test -p eave-app --lib dump_real_inventory -- --ignored --nocapture`
    #[test]
    #[ignore = "machine-dependent instrument; prints, asserts nothing"]
    fn dump_real_inventory() {
        let cwd = std::env::var("EAVE_SLASH_DUMP_CWD").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        let provider =
            std::env::var("EAVE_SLASH_DUMP_PROVIDER").unwrap_or_else(|_| "claude-code".into());
        for r in roots(&provider, Path::new(&cwd), Path::new(&home)) {
            println!("root {:?} {:?} {}", r.evidence, r.layout, r.path.display());
        }
        let all = discover(&provider, Path::new(&cwd), Path::new(&home));
        println!(
            "discovered {} entries for {provider} under {cwd} / {home}",
            all.len()
        );
        for e in &all {
            println!(
                "  /{}  [{:?} {}]  {}",
                e.name,
                e.kind(),
                e.origin().map_or("(no file)", Origin::label),
                e.description().unwrap_or("(no description)")
            );
        }

        let Ok(init_path) = std::env::var("EAVE_SLASH_DUMP_INIT") else {
            println!("(set EAVE_SLASH_DUMP_INIT=<file with one system/init line> for the diff)");
            return;
        };
        let text = std::fs::read_to_string(&init_path).expect("read the captured init line");
        let line = text
            .lines()
            .find(|l| l.contains("\"subtype\":\"init\""))
            .expect("a system/init line in that file");
        let v: serde_json::Value = serde_json::from_str(line).expect("parse the init line");
        let inventory: Vec<String> = v["slash_commands"]
            .as_array()
            .expect("slash_commands array")
            .iter()
            .filter_map(|c| c.as_str())
            .map(str::to_string)
            .collect();
        let offered = offer(&inventory, &all);
        let described = offered.iter().filter(|e| e.found.is_some()).count();
        let names: std::collections::HashSet<&str> = inventory.iter().map(String::as_str).collect();
        let false_offers: Vec<&str> = all
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| !names.contains(n))
            .collect();
        println!(
            "the CLI announces {}; the palette offers {} ({} denied); \
             the scan describes {} of them and finds {} the CLI does not have: {:?}",
            inventory.len(),
            offered.len(),
            inventory.len() - offered.len(),
            described,
            false_offers.len(),
            false_offers,
        );
        for e in &offered {
            println!(
                "  /{}  {}",
                e.name,
                e.description().unwrap_or("(name only — no file backs it)")
            );
        }
    }

    /// **The cwd trap.** eave is one binary in two roles and the ENGINE is
    /// what holds a session's cwd; the app's `current_dir()` is wherever
    /// the bundle was launched from. A previous slice shipped a launcher
    /// that printed one while opening in the other. This module must
    /// therefore never ask the process where it is — every path arrives as
    /// a parameter.
    ///
    /// Whitespace-insensitive on purpose: a source-scanning test that
    /// embeds indentation breaks the moment a concurrent slice re-nests a
    /// block.
    #[test]
    fn no_ambient_directory_is_ever_consulted() {
        let src = include_str!("slash.rs");
        // Each needle is ASSEMBLED rather than written whole, so the
        // guard does not match its own source and report itself. (It did,
        // on the first run — 3 hits against 2 lines of prose.)
        let needles = [
            format!("current_{}()", "dir"),
            format!("env::var(\"{}\")", "PWD"),
            format!("home_{}()", "dir"),
        ];
        // The doc comments talk ABOUT `current_dir`, so look for the CALL.
        for forbidden in &needles {
            let hits = src.matches(forbidden.as_str()).count();
            let in_prose = src
                .lines()
                .filter(|l| l.trim_start().starts_with("//"))
                .filter(|l| l.contains(forbidden.as_str()))
                .count();
            assert_eq!(
                hits, in_prose,
                "{forbidden} appears in code, not just in prose — discovery \
                 must be keyed to the SESSION's cwd, which arrives as a \
                 parameter, never to the app process's own directory"
            );
        }
    }
}
