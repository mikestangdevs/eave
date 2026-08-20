//! Slice AR — the `/` palette's **discovery** half: what slash commands
//! this operator actually has, and what each one does.
//!
//! `Shell::send_current` has always forwarded a `/`-prefixed message
//! verbatim, so the CLI has always resolved these. What was missing was
//! never plumbing — it was that **nothing in eave could name them**. This
//! module is that half, and it is deliberately pure: no gpui, no globals,
//! no `current_dir`, every path handed in. The half with the interesting
//! behaviour is the half that has to be testable without a window
//! (`overlay`'s ruling, taken again here).
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
//! # What this can and cannot see (measured, 2026-08-20)
//!
//! On this machine the CLI reports **38 skills and 71 slash commands** in
//! its `system/init` frame. Everything reachable from disk — project,
//! user, and the plugin install paths — accounts for **18** of those 38.
//! The other 20 (`dataviz`, `simplify`, `code-review`, `loop`, …) exist
//! **nowhere on the filesystem**: they are bundled inside the `claude`
//! executable. `find ~/.claude /opt/homebrew/lib/node_modules -name
//! dataviz` returns nothing while `/dataviz` is live in the CLI.
//!
//! A filesystem scan therefore cannot be complete, and this one does not
//! claim to be. It is complete over the things that have a `SKILL.md` or
//! a `.md` to read — which is exactly the set that has a **description**,
//! and a row with no description is not discovery. See the report.

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

/// One row's worth of fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashEntry {
    /// What the operator types after the `/`, WITHOUT the slash.
    pub name: String,
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
                description: front.description,
                origin: origin.clone(),
                kind: Kind::Skill,
                operator_only: front.operator_only,
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
            description: front.description,
            origin: origin.clone(),
            kind: Kind::Command,
            operator_only: front.operator_only,
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

    fn found(td: &TempTree) -> Vec<SlashEntry> {
        discover(CLAUDE, &td.path().join("proj"), &td.path().join("home"))
    }

    #[test]
    fn a_project_skill_and_a_project_command_are_both_found_with_their_descriptions() {
        let td = fixture();
        let all = found(&td);
        let skill = all
            .iter()
            .find(|e| e.name == "projskill" && e.origin == Origin::Project)
            .expect("project skill");
        assert_eq!(skill.kind, Kind::Skill);
        assert_eq!(skill.description.as_deref(), Some("a project skill"));

        let cmd = all.iter().find(|e| e.name == "projcmd").expect("command");
        assert_eq!(cmd.kind, Kind::Command);
        assert_eq!(cmd.origin, Origin::Project);
        assert_eq!(cmd.description.as_deref(), Some("a project command"));
    }

    /// Verified against the real CLI (2.1.226): a probe command at
    /// `.claude/commands/sub/nested.md` came back on stdout as
    /// `<command-name>/sub:nested</command-name>` and ran.
    #[test]
    fn a_command_in_a_subdirectory_namespaces_with_a_colon() {
        let td = fixture();
        let all = found(&td);
        let n = all.iter().find(|e| e.name == "sub:nested").expect("nested");
        assert_eq!(n.description.as_deref(), Some("a nested command"));
    }

    /// The one that would have been guessed wrong. A skill directory named
    /// `dirnamexyz` whose frontmatter said `name: frontmatternameabc` was
    /// reported by the CLI's own `system/init` frame as `dirnamexyz`.
    #[test]
    fn a_skill_is_named_by_its_directory_not_by_its_frontmatter() {
        let td = fixture();
        let all = found(&td);
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
        let all = found(&td);
        let p = all
            .iter()
            .find(|e| e.name == "pack:packed")
            .unwrap_or_else(|| {
                panic!(
                    "expected pack:packed; got {:?}",
                    all.iter().map(|e| &e.name).collect::<Vec<_>>()
                )
            });
        assert_eq!(p.origin, Origin::Plugin("pack".into()));
        assert_eq!(p.description.as_deref(), Some("a grouped plugin skill"));
        assert!(
            !all.iter().any(|e| e.name.contains("group")),
            "the grouping directory must not appear in any name"
        );
    }

    #[test]
    fn project_outranks_user_for_the_same_name() {
        let td = fixture();
        let all = found(&td);
        let picked = filter(&all, "projskill");
        assert_eq!(
            picked.len(),
            1,
            "one /projskill reaches the CLI, so one row"
        );
        assert_eq!(picked[0].origin, Origin::Project);
        assert_eq!(picked[0].description.as_deref(), Some("a project skill"));
    }

    #[test]
    fn a_folded_block_description_is_read_and_collapsed_to_one_line() {
        let td = fixture();
        let all = found(&td);
        let u = all.iter().find(|e| e.name == "userskill").expect("user");
        assert_eq!(
            u.description.as_deref(),
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
        let all = found(&td);
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
        let all = found(&td);
        let hits = filter(&all, "packed");
        assert_eq!(
            hits.first().map(|e| e.name.as_str()),
            Some("pack:packed"),
            "typing the leaf must reach the namespaced entry"
        );
    }

    #[test]
    fn a_prefix_match_outranks_a_match_in_the_middle() {
        let entries = vec![
            SlashEntry {
                name: "unprojected".into(),
                description: None,
                origin: Origin::User,
                kind: Kind::Command,
                operator_only: false,
            },
            SlashEntry {
                name: "proj".into(),
                description: None,
                origin: Origin::User,
                kind: Kind::Command,
                operator_only: false,
            },
        ];
        let hits = filter(&entries, "proj");
        assert_eq!(hits[0].name, "proj");
        assert_eq!(hits[1].name, "unprojected");
    }

    #[test]
    fn an_empty_query_offers_everything_in_precedence_order() {
        let td = fixture();
        let all = found(&td);
        let hits = filter(&all, "");
        assert_eq!(hits.len(), all.len() - 1, "one duplicate name is merged");
        assert_eq!(hits[0].origin, Origin::Project);
    }

    #[test]
    fn insertion_puts_the_slash_back() {
        let e = SlashEntry {
            name: "craft-skills:seams".into(),
            description: None,
            origin: Origin::Plugin("craft-skills".into()),
            kind: Kind::Skill,
            operator_only: false,
        };
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
        let d = all[0].description.as_ref().unwrap();
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
        assert!(all[0].description.is_some());
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
        assert_eq!(all[0].description, None);
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
        assert_eq!(all[0].description, None);
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
            all[0].operator_only,
            "the author declared this operator-invoked only, which is \
             exactly the class a / palette exists for"
        );
        assert!(all[0].description.is_some());
    }

    /// A skill that says nothing about model invocation has not declared
    /// `false` — but it renders the same, and does not earn a third state.
    #[test]
    fn a_skill_that_does_not_mention_model_invocation_is_not_operator_only() {
        let td = fixture();
        let all = found(&td);
        assert!(all.iter().all(|e| !e.operator_only));
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
    /// `EAVE_SLASH_DUMP_CWD=<dir> cargo test -p eave-app --lib
    /// dump_real_inventory -- --ignored --nocapture`
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
                e.kind,
                e.origin.label(),
                e.description.as_deref().unwrap_or("(no description)")
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
