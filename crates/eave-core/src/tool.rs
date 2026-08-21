//! What a tool call actually *is*, decoded once at the adapter and carried
//! for the life of the record.
//!
//! # The defect this exists to close
//!
//! Every provider hands eave a structured object — `{"command": "cargo
//! test"}`, `{"path": "src/main.rs"}` — and until this module the adapters
//! flattened it to a `String` at ingest and threw the structure away. It
//! could never be widened again: a `Row` downstream held bytes, not a
//! command, so the only way to say "this call read *this* file" was to
//! re-guess the shape from the tool's name. `eave_wire::model::tool_detail`
//! was exactly that guess, keyed on Claude's tool names, and it therefore
//! answered `None` for **every other provider eave ships** — a Codex
//! `commandExecution`, a Hermes `execute`, a Pi `bash` and a Grok `write`
//! all rendered as a bare tool name and a status word.
//!
//! # The rule that shapes this enum
//!
//! **Typing ADDS a lens; it never replaces the fact of record.**
//! [`crate::events::SessionEvent::ToolUseStarted`] still carries the
//! provider's own `name` and its `input` verbatim (bounded by
//! [`crate::events::TOOL_PAYLOAD_CAP`]). This enum is the *rendered*
//! projection of that input, so:
//!
//! - **Every field here is something the UI puts on screen.** No field
//!   exists "for later". A tool row is one line; this is that line's
//!   argument.
//! - **Nothing bulky is duplicated.** A `Write`'s content and an `Edit`'s
//!   old/new strings are deliberately absent — they are already on `input`,
//!   and the journal **replays in full on cold start** (spec 02), so
//!   copying an 8 KiB payload into a second field on the same record would
//!   double what startup reads to buy nothing.
//! - **An unrecognised tool is [`ToolCall::Unknown`], never a guess.** The
//!   provider's name survives; so does its input, on the event.
//!
//! # Relationship to comet
//!
//! comet solved the same problem first (`comet/crates/proto/src/agent.rs:149`,
//! a 12-variant `ToolCall`), and the decision to type at the adapter with an
//! `Unknown{name, input}` escape hatch is taken from it wholesale. The
//! *fields* are not: comet's calls travel over a network into a shared CRDT
//! document, so it carries write content and MCP inputs and then strips them
//! at the doc boundary (`comet/crates/doc/src/parts.rs:485`
//! `sanitize_tool_call`). eave's journal never leaves the host, so eave
//! needs no sanitiser — and therefore no fields to sanitise.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::cap_payload;

/// A tool call reduced to the one fact a transcript row states about it.
///
/// Serialised with an inner `"tool"` tag rather than `"kind"`: a journal
/// line already spends `"kind"` on the event type, and two different
/// `kind`s in one object is a line nobody can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolCall {
    /// A shell command. Claude `Bash`, Codex `commandExecution`, Pi `bash`,
    /// ACP `execute`.
    Run { command: String },
    /// Read a file's contents.
    Read { path: String },
    /// Create or overwrite a file whole.
    Write { path: String },
    /// Change part of a file in place.
    Edit { path: String },
    /// A change eave cannot reduce to one written or edited file — a
    /// delete, a move, or several files in one call. `None` when the
    /// provider named no single path, which is the honest answer for a
    /// multi-file patch.
    Patch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// List a directory. Pi has an `ls` tool; comet's enum has no analogue
    /// because none of its agents ship one.
    List { path: String },
    /// Search file *contents* for a pattern.
    Grep { pattern: String },
    /// Match file *names* against a glob. Distinct from [`ToolCall::Grep`]
    /// because they are different questions, and the row says which was
    /// asked.
    Glob { pattern: String },
    /// Retrieve one URL.
    Fetch { url: String },
    /// Search the web.
    WebSearch { query: String },
    /// The agent restated its plan.
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    /// The agent spawned a subagent, and this is what it asked it to do.
    ///
    /// eave has no subagent UI, so this is a *label*, not a binding: unlike
    /// comet — whose `ToolCall::is_subagent_spawn` gates which chip may be
    /// stamped with a child-session ref, and which exists because a driver
    /// keying bug once turned an ordinary chip into a spawn chip that
    /// opened an empty panel (`comet/crates/proto/src/agent.rs:203`, dated
    /// 2026-08-20) — nothing downstream of here keys off this variant.
    Task { description: String },
    /// A tool this build does not recognise. Carries the provider's own
    /// name so a reader holding only the typed call still knows what ran;
    /// the input is **not** repeated here because
    /// [`crate::events::SessionEvent::ToolUseStarted::input`] on the very
    /// same record already has it verbatim.
    Unknown { name: String },
}

/// One line of an agent's plan. `done` is the provider's own completion
/// flag, never inferred from the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

/// The separator eave uses between two facts on one line (`3/7 · fix the
/// parser`), matching the rail and the composer.
const JOIN: &str = " · ";

impl ToolCall {
    /// The single line a transcript row states next to the tool's name, or
    /// `None` when there is nothing to say beyond the name itself.
    ///
    /// One line, always: a heredoc or a multi-line command states its first
    /// line and says it was cut, rather than making one row as tall as a
    /// script.
    pub fn detail(&self) -> Option<String> {
        match self {
            ToolCall::Run { command } => first_line(command),
            ToolCall::Read { path }
            | ToolCall::Write { path }
            | ToolCall::Edit { path }
            | ToolCall::List { path } => first_line(path),
            ToolCall::Patch { path } => path.as_deref().and_then(first_line),
            ToolCall::Grep { pattern } | ToolCall::Glob { pattern } => first_line(pattern),
            ToolCall::Fetch { url } => first_line(url),
            ToolCall::WebSearch { query } => first_line(query),
            ToolCall::Task { description } => first_line(description),
            ToolCall::Todo { items } => {
                if items.is_empty() {
                    return None;
                }
                let done = items.iter().filter(|i| i.done).count();
                let counts = format!("{done}/{}", items.len());
                // The first thing still outstanding is what the agent is
                // about to do, which is the fact worth a row.
                match items
                    .iter()
                    .find(|i| !i.done)
                    .and_then(|i| first_line(&i.text))
                {
                    Some(next) => Some(format!("{counts}{JOIN}{next}")),
                    None => Some(counts),
                }
            }
            // The name column already carries this verbatim; repeating it
            // in the detail column would be one fact rendered twice.
            ToolCall::Unknown { .. } => None,
        }
    }
}

/// A string reduced to its first non-empty line, marked when it was cut.
fn first_line(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    let first = lines.next()?.trim_end();
    if first.is_empty() {
        return None;
    }
    Some(if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    })
}

// ---------------------------------------------------------------------------
// Extraction helpers, shared by every adapter's decoder.

/// A JSON string field, bounded by the same cap the raw payload is bounded
/// by.
///
/// The cap matters here and not only on `input`: a decoder reads the
/// provider's object *before* it is serialised and capped, so an agent that
/// writes a 200 KB heredoc would otherwise put all 200 KB into
/// `Run{command}` — on a record whose whole point is to be small enough to
/// replay a thousand times at cold start.
pub(crate) fn text(v: &Value, key: &str) -> Option<String> {
    let s = v.get(key)?.as_str()?;
    if s.is_empty() {
        None
    } else {
        Some(cap_payload(s))
    }
}

/// The first of several spellings a provider might use for one field.
///
/// A key set rather than a per-provider branch: ACP agents disagree on
/// `path` vs `file_path` vs `filePath` within the same protocol, and a new
/// agent using any of them needs no code change.
pub(crate) fn text_any(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| text(v, k))
}

// ---------------------------------------------------------------------------
// ACP, shared by the Grok and Hermes adapters.

/// ACP's `locations: [{path, line?}]`, which is where an ACP agent puts the
/// file it is about to touch when it sends no `rawInput` at all.
fn first_location(update: &Value) -> Option<String> {
    text(update.get("locations")?.as_array()?.first()?, "path")
}

/// Generic ACP `title`s that are labels, not arguments.
///
/// An ACP agent may send `title` before it has any arguments to show, and
/// the label is a display string for a *kind* — putting "Read File" where a
/// path belongs is worse than putting nothing.
fn is_placeholder_title(title: &str) -> bool {
    matches!(
        title.trim(),
        "Terminal"
            | "Read File"
            | "Edit File"
            | "Delete File"
            | "Write File"
            | "Web Search"
            | "Web Fetch"
            | "Find"
            | "grep"
    )
}

/// An ACP `title` used as an argument, when it is one.
///
/// **This is the agent's own one-line summary, verbatim — never a parse of
/// it.** Hermes sends no `rawInput` for `terminal`, `read_file`,
/// `write_file` or `patch` and folds the tool name into a human title
/// (`"terminal: cargo test"`); the Hermes slice already decided that
/// splitting that title to recover a tool *name* silently invents one
/// (docs/reports/stage-aw-hermes-adapter.md §3.3, §7.2) and that decision
/// stands. Showing the whole title as the row's detail is the other thing:
/// it is what Hermes itself says the call is, uncut and unparsed, and it
/// beats a row that says `execute` and nothing else.
fn title_arg(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() || is_placeholder_title(t) {
        None
    } else {
        Some(cap_payload(t))
    }
}

/// Decode an ACP `tool_call` update (`kind` + `rawInput` + `locations` +
/// `title`) into a [`ToolCall`].
///
/// `kind` is ACP's enumerated protocol value (`read|edit|delete|move|
/// search|execute|think|fetch|switch_mode|other`), so it drives the match;
/// everything else is a best-effort argument lookup, **most structured
/// first** — `rawInput`, then `locations`, then the agent's own `title`.
///
/// `kind` is optional in the protocol and defaults to `other`, which is
/// not a technicality: Grok omits it on every `tool_call` update, and its
/// adapter therefore decodes by Grok's own tool name and only falls
/// through to here. Shared by both ACP adapters because the protocol is
/// the same one; the provider-specific knowledge (Grok's
/// `_meta["x.ai/tool"]`, Hermes' title convention) stays in the adapters.
pub fn from_acp_update(update: &Value) -> ToolCall {
    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other");
    let raw = update.get("rawInput").filter(|v| !v.is_null());
    let title = update.get("title").and_then(Value::as_str).unwrap_or("");
    // Every ACP agent eave has captured spells the file field differently:
    // Grok sends `file_path` (capture: docs/reports/stage-av-grok-adapter.md
    // line 180), the ACP examples say `path`, camelCase shows up too.
    const PATH_KEYS: [&str; 3] = ["path", "file_path", "filePath"];
    let path = || {
        raw.and_then(|r| text_any(r, &PATH_KEYS))
            .or_else(|| first_location(update))
    };
    match kind {
        "execute" => ToolCall::Run {
            command: raw
                .and_then(|r| text_any(r, &["command", "cmd"]))
                .or_else(|| title_arg(title))
                .unwrap_or_default(),
        },
        "read" => match path().or_else(|| title_arg(title)) {
            Some(path) => ToolCall::Read { path },
            None => ToolCall::Unknown { name: kind.into() },
        },
        // ACP folds create, modify, delete and move into `edit`-family
        // kinds and says which only through the payload. A `rawInput` that
        // carries whole content is a write; one that does not is an edit;
        // a delete or move is neither, so it is a patch against the path.
        "edit" => match path().or_else(|| title_arg(title)) {
            Some(path) if raw.is_some_and(|r| r.get("content").is_some()) => {
                ToolCall::Write { path }
            }
            Some(path) => ToolCall::Edit { path },
            None => ToolCall::Patch { path: None },
        },
        "delete" | "move" => ToolCall::Patch {
            path: path().or_else(|| title_arg(title)),
        },
        "search" => match raw.and_then(|r| text_any(r, &["pattern", "query", "regex"])) {
            Some(pattern) => ToolCall::Grep { pattern },
            None => match raw.and_then(|r| text_any(r, &["globPattern", "glob"])) {
                Some(pattern) => ToolCall::Glob { pattern },
                None => match title_arg(title) {
                    Some(pattern) => ToolCall::Grep { pattern },
                    None => ToolCall::Unknown { name: kind.into() },
                },
            },
        },
        "fetch" => match raw.and_then(|r| text_any(r, &["url", "uri"])) {
            Some(url) => ToolCall::Fetch { url },
            None => match raw
                .and_then(|r| text_any(r, &["query", "searchTerm"]))
                .or_else(|| title_arg(title))
            {
                Some(query) => ToolCall::WebSearch { query },
                None => ToolCall::Unknown { name: kind.into() },
            },
        },
        // `think`, `switch_mode`, `other`, and any kind a later ACP
        // version adds. Named by `kind` rather than by `title`: the kind is
        // an enumerated protocol value and the title is prose.
        _ => ToolCall::Unknown {
            name: kind.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The whole point of the slice, stated as one assertion per shape a
    /// row has to read: a run says its command, a read says its path.
    #[test]
    fn a_typed_call_states_its_one_argument() {
        assert_eq!(
            ToolCall::Run {
                command: "cargo test --workspace".into()
            }
            .detail()
            .as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(
            ToolCall::Read {
                path: "/src/main.rs".into()
            }
            .detail()
            .as_deref(),
            Some("/src/main.rs")
        );
        assert_eq!(
            ToolCall::Unknown {
                name: "SomethingNew".into()
            }
            .detail(),
            None,
            "the name column already says this; a second copy is not a detail"
        );
    }

    /// A multi-line command states its first line and says it was cut —
    /// one row must never become as tall as a heredoc.
    #[test]
    fn a_multi_line_argument_is_cut_and_says_so() {
        let d = ToolCall::Run {
            command: "cat <<EOF\nline two\nEOF".into(),
        }
        .detail()
        .unwrap();
        assert_eq!(d, "cat <<EOF …");
        assert!(!d.contains("line two"));
    }

    /// A plan row says how far along it is and what is next.
    #[test]
    fn a_plan_states_its_progress_and_the_next_step() {
        let items = vec![
            TodoItem {
                text: "read the brief".into(),
                done: true,
            },
            TodoItem {
                text: "fix the parser".into(),
                done: false,
            },
            TodoItem {
                text: "ship".into(),
                done: false,
            },
        ];
        assert_eq!(
            ToolCall::Todo { items }.detail().as_deref(),
            Some("1/3 · fix the parser")
        );
        assert_eq!(ToolCall::Todo { items: vec![] }.detail(), None);
    }

    /// An argument longer than the payload cap is bounded here too. The
    /// decoder reads the provider's object *before* it is serialised and
    /// capped, so without this a 200 KB heredoc would ride the record.
    #[test]
    fn an_extracted_argument_is_bounded_by_the_payload_cap() {
        let huge: String = std::iter::repeat_n('x', crate::events::TOOL_PAYLOAD_CAP * 3).collect();
        let got = text(&json!({ "command": huge }), "command").unwrap();
        assert!(got.len() < huge.len());
        assert!(got.contains("bytes elided"));
    }

    /// ACP's structured fields beat its display string, and the display
    /// string beats nothing at all.
    #[test]
    fn acp_prefers_raw_input_then_locations_then_title() {
        assert_eq!(
            from_acp_update(&json!({
                "kind": "execute", "title": "terminal: cargo test",
                "rawInput": {"command": "cargo test --workspace"}
            })),
            ToolCall::Run {
                command: "cargo test --workspace".into()
            }
        );
        assert_eq!(
            from_acp_update(&json!({
                "kind": "read", "title": "Read File",
                "locations": [{"path": "/src/lib.rs"}]
            })),
            ToolCall::Read {
                path: "/src/lib.rs".into()
            },
            "a placeholder title must never stand in for a path"
        );
        // Hermes' real shape: no rawInput, no locations, a human title.
        assert_eq!(
            from_acp_update(&json!({"kind": "execute", "title": "terminal: cargo test"})),
            ToolCall::Run {
                command: "terminal: cargo test".into()
            },
            "the agent's own summary, verbatim — better than a bare kind"
        );
    }

    /// An ACP `edit` that carries whole content is a write, one that does
    /// not is an edit, and a delete is neither.
    #[test]
    fn acp_edit_kinds_separate_write_from_edit_from_patch() {
        assert_eq!(
            from_acp_update(&json!({
                "kind": "edit",
                "rawInput": {"variant": "Write", "file_path": "/tmp/perm.txt", "content": "asked\n"}
            })),
            ToolCall::Write {
                path: "/tmp/perm.txt".into()
            }
        );
        assert_eq!(
            from_acp_update(&json!({
                "kind": "edit",
                "rawInput": {"file_path": "/tmp/perm.txt", "old_string": "a", "new_string": "b"}
            })),
            ToolCall::Edit {
                path: "/tmp/perm.txt".into()
            }
        );
        assert_eq!(
            from_acp_update(&json!({"kind": "delete", "locations": [{"path": "/tmp/gone"}]})),
            ToolCall::Patch {
                path: Some("/tmp/gone".into())
            }
        );
    }

    /// An ACP kind this build has never heard of is named, never guessed.
    #[test]
    fn an_unheard_of_acp_kind_is_named_not_guessed() {
        assert_eq!(
            from_acp_update(&json!({"kind": "switch_mode", "title": "Switch to plan mode"})),
            ToolCall::Unknown {
                name: "switch_mode".into()
            }
        );
    }
}
