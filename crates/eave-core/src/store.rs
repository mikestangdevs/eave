//! Data-dir layout, engine instance lock, atomic session meta (spec 02
//! §Layout).
//!
//! `<data_dir>/engine.lock` — flock'd; single engine instance per data dir
//! (comet's instance lock; dsh's *lack* of one is a flagged pain point
//! there). A second app instance fails to acquire and must talk to the
//! running engine instead of writing journals itself — that is what keeps
//! two instances against the same repo from corrupting anything.
//!
//! `<data_dir>/sessions/<uuid>/journal.jsonl` — the append-only journal.
//! `<data_dir>/sessions/<uuid>/meta.json` — small, rewritten atomically
//! (temp + fsync + rename): provider, cwd, state, resume cursor, and the
//! (pid, start_time) of a live child — the orphan-detection breadcrumb
//! after an engine crash.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::proc::{self, ProcIdentity};

pub const META_FORMAT_VERSION: u32 = 1;

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("sessions"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join("sessions").join(session_id)
    }

    pub fn create_session_dir(&self, session_id: &str) -> io::Result<PathBuf> {
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// All session ids present on disk (spec 02 §Resume contract: scanned
    /// on engine start for crash-tail repair and orphan checks).
    pub fn session_ids(&self) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join("sessions"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn journal_path(session_dir: &Path) -> PathBuf {
        session_dir.join("journal.jsonl")
    }

    pub fn meta_path(session_dir: &Path) -> PathBuf {
        session_dir.join("meta.json")
    }
}

/// Contended-start contract (spec 02 §Layout): acquisition is fail-fast
/// (`LOCK_NB`) — never blocks, never retries. The loser gets `Busy` naming
/// the holder and must exit inert: no journal, meta, or session writes.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another engine instance holds the flock. `holder` is the winner's
    /// identity as recorded in the lock file (`None` if unreadable — the
    /// content is advisory; liveness comes from the flock itself). The
    /// meta breadcrumb was deliberately not used here: it names the
    /// session *child*, not the engine holding the lock.
    #[error("another engine instance holds the lock for this data dir{}", match holder { Some(h) => format!(" (pid {})", h.pid), None => " (holder unknown)".to_string() })]
    Busy { holder: Option<ProcIdentity> },
    #[error("io error acquiring engine lock: {0}")]
    Io(#[from] io::Error),
}

/// Held for the engine's lifetime; the flock releases on drop/exit —
/// including SIGKILL, which is what makes it safe as a liveness signal.
/// A *stale* lock therefore cannot exist: a dead holder's flock is gone
/// the moment the process is; only its advisory content lingers, which
/// never blocks and is overwritten by the next winner.
#[derive(Debug)]
pub struct EngineLock {
    _file: File,
}

impl EngineLock {
    // clippy::suspicious_open_options wants `.truncate(true)` here. Taking
    // that advice is the bug: a process that merely LOSES the flock race
    // would erase the winner's identity record on its way to finding out it
    // lost, and the loser's whole job is to display that record. The absent
    // O_TRUNC is the feature; `set_len(0)` below runs only on the winning
    // path, after the lock is held.
    #[allow(clippy::suspicious_open_options)]
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        let path = data_dir.join("engine.lock");
        // No O_TRUNC: a losing open must not erase the winner's record.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            // Winner: record our identity for losers to display.
            let me = proc::identity_of(std::process::id() as i32);
            file.set_len(0)?;
            (&file).write_all(&serde_json::to_vec(&me).unwrap_or_default())?;
            let _ = file.sync_all();
            Ok(Self { _file: file })
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                let holder = fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Option<ProcIdentity>>(&b).ok())
                    .flatten();
                Err(LockError::Busy { holder })
            } else {
                Err(LockError::Io(err))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub format_version: u32,
    pub provider: String,
    pub cwd: String,
    /// Provider-native resume cursor (Claude `session_id`).
    pub session_id: String,
    /// Current state name (spec 01 §States; terminal names from
    /// `Outcome::state_name`).
    pub state: String,
    /// Live child, if any — the orphan-detection breadcrumb. Cleared on
    /// terminal outcomes.
    pub child: Option<ProcIdentity>,
    /// The operator's name for this session (Stage AB), already through
    /// [`normalize_title`]. `None` = unnamed, and eave never fills it in.
    ///
    /// It lives HERE and not in the journal header for the same reason
    /// `header.agent` lives there and not here: the header is the
    /// session's immutable identity, written once before anything spawns,
    /// and a name the operator can change at any time is not identity.
    /// `meta.json` is the mutable file, rewritten atomically on every
    /// state change — which is exactly the write pattern a rename needs.
    ///
    /// `#[serde(default)]`, so a meta written before this field existed
    /// reads back as unnamed. `format_version` did not move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The operator filed this session away (Stage AC). One bit, and it
    /// means exactly one thing: **the default rail listing does not draw
    /// it.** Nothing else changes — the session still loads, still
    /// replays, still answers `subscribe`, and can still be sent to.
    ///
    /// It lives here, beside [`title`](Meta::title), for the identical
    /// reason: it is a mutable operator opinion, and the journal is the
    /// session's immutable history. **Archiving must never touch the
    /// journal** — an archive that rewrote history would be a delete
    /// wearing a softer word, and the whole value of the bit is that it
    /// is reversible by flipping it back.
    ///
    /// `#[serde(default)]` + `skip_serializing_if`, so a pre-Stage-AC
    /// meta reads back unarchived and an unarchived session writes no
    /// key at all. `format_version` did not move.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
}

/// The longest operator title eave will hold, in **characters** — a cap in
/// bytes would split a multi-byte character and store invalid text.
///
/// Why a cap exists at all: the title is a session fact, and session facts
/// ride `watch`, which re-broadcasts EVERY session to EVERY client on every
/// state change. An uncapped title makes one paste into a rename box a
/// permanent multiplier on that broadcast.
///
/// Why this value and not a tighter one: it is deliberately PAST what the
/// rail can draw. Bounding the wire and bounding the display are different
/// jobs — a cap tight enough to double as the visual limit would silently
/// destroy the operator's text, where truncation only hides it. That
/// relationship is asserted rather than assumed:
/// `ui_harness::a_long_title_clips_itself_before_it_crowds_the_badge`
/// measures a title at this cap against the label slot's real width and
/// requires the cap to exceed it.
pub const TITLE_MAX_CHARS: usize = 64;

/// Bring an operator-supplied title to the one shape eave stores: single
/// line, trimmed, capped. `None` out means "not a title" — which is what an
/// empty or whitespace-only string is, and is how a rename clears one.
///
/// The whitespace fold is not cosmetic. The rail draws the title on one
/// line, so an embedded newline would render as a space anyway while the
/// stored string kept a break the operator could not see — the sort of
/// invisible difference that makes two identical-looking titles compare
/// unequal.
pub fn normalize_title(raw: &str) -> Option<String> {
    let folded = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return None;
    }
    Some(folded.chars().take(TITLE_MAX_CHARS).collect())
}

/// Cap on the slug a title becomes in a branch name (Slice ID2a). Forty
/// characters is what `eave/<slug>` needs to fit the rail's branch line
/// beside its `repo/` prefix at the widths the rail actually opens at; a
/// longer one would truncate mid-word on the very row that names it.
pub const BRANCH_SLUG_MAX: usize = 40;

/// The branch-name form of an operator's title (Slice ID2a): an isolated
/// session's branch is `eave/<sid8>` until the operator names the session,
/// and `eave/<slug of the name>` after.
///
/// Runs of alphanumerics, lowercased and joined by single dashes; every
/// other character is a separator. Cut to [`BRANCH_SLUG_MAX`] at a dash
/// where there is one. `None` when nothing survives — a title of
/// punctuation has no branch in it, and the caller keeps the opaque name
/// rather than minting `eave/`.
///
/// Derived from the TITLE the operator applied, never from a transcript:
/// the branch name rides `watch` (`WorktreeInfo::branch`) and so keeps the
/// same authorship line `SessionInfo::title` keeps.
pub fn branch_slug(title: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(c.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > BRANCH_SLUG_MAX {
        let head: String = out.chars().take(BRANCH_SLUG_MAX).collect();
        out = match head.rfind('-') {
            Some(at) if at > 0 => head[..at].to_string(),
            _ => head,
        };
    }
    Some(out)
}

impl Meta {
    /// Atomic rewrite: temp file, fsync, rename (spec 02 §Layout).
    pub fn save(&self, session_dir: &Path) -> io::Result<()> {
        let tmp = session_dir.join("meta.json.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self).map_err(invalid)?)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, Store::meta_path(session_dir))
    }

    /// Save, keeping whatever the OPERATOR set is on disk — the
    /// [`title`](Meta::title) (Stage AB) and the
    /// [`archived`](Meta::archived) bit (Stage AC).
    ///
    /// `meta.json` has two classes of writer with different concerns: the
    /// state machine (state, child, resume cursor) rewrites it on every
    /// transition from a snapshot taken at spawn, and the operator methods
    /// each write exactly one field. Because every save rewrites the whole
    /// file, either class erases the other's work unless one rule holds —
    /// **each operator field has exactly one writer, and every other
    /// writer preserves it.** This is that rule.
    ///
    /// Without it the bug is quiet and specific: name a session, send it a
    /// message, and the spawn's meta write drops the name you just gave
    /// it. Stage AC widened the rule rather than adding a second one,
    /// because the second one is the one somebody forgets to call.
    pub fn save_keeping_operator_fields(&mut self, session_dir: &Path) -> io::Result<()> {
        if let Ok(on_disk) = Self::load(session_dir) {
            self.title = on_disk.title;
            self.archived = on_disk.archived;
        }
        self.save(session_dir)
    }

    pub fn load(session_dir: &Path) -> io::Result<Self> {
        let bytes = fs::read(Store::meta_path(session_dir))?;
        serde_json::from_slice(&bytes).map_err(invalid)
    }
}

fn invalid(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape a rename box can produce, and the one shape eave keeps.
    #[test]
    fn a_title_is_one_trimmed_line_or_nothing_at_all() {
        assert_eq!(
            normalize_title("  ship the rail  "),
            Some("ship the rail".into())
        );
        assert_eq!(
            normalize_title("ship\nthe\trail"),
            Some("ship the rail".into()),
            "a title renders on one line; it must be stored as one"
        );
        // Blank input is not an empty title — it is no title. Anything
        // else would put an empty string on the wire and an empty line in
        // the rail.
        assert_eq!(normalize_title(""), None);
        assert_eq!(normalize_title("   \n\t "), None);
    }

    /// Slice ID2a: the branch a named session's worktree moves to.
    #[test]
    fn a_title_slugs_to_a_branch_or_to_nothing() {
        assert_eq!(
            branch_slug("Diagnose OMP chat blanking").as_deref(),
            Some("diagnose-omp-chat-blanking")
        );
        // Punctuation is a separator, never a character; runs of it are
        // one dash; the ends carry none.
        assert_eq!(
            branch_slug("  fix: the `parser` (again!) ").as_deref(),
            Some("fix-the-parser-again")
        );
        assert_eq!(branch_slug("Überprüfen"), Some("überprüfen".into()));
        // Nothing alphanumeric is no slug — the caller keeps `eave/<sid8>`
        // rather than minting a branch called `eave/`.
        assert_eq!(branch_slug("--- !!! ---"), None);
        assert_eq!(branch_slug(""), None);
    }

    #[test]
    fn a_long_title_slugs_to_the_cap_at_a_dash() {
        let long = "update the acp decoder so that it handles the plan session update";
        let slug = branch_slug(long).unwrap();
        assert!(slug.chars().count() <= BRANCH_SLUG_MAX, "{slug}");
        assert!(!slug.ends_with('-'), "{slug}");
        assert!(
            long.replace(' ', "-").starts_with(&slug)
                && long.replace(' ', "-")[slug.len()..].starts_with('-'),
            "cut mid-word: {slug}"
        );
        // One word past the cap has no dash to back off to: cut hard.
        let word = "a".repeat(BRANCH_SLUG_MAX * 2);
        assert_eq!(branch_slug(&word).unwrap().len(), BRANCH_SLUG_MAX);
    }

    /// The cap counts characters, not bytes. A byte cap here would cut a
    /// multi-byte character in half and store text that is not text.
    #[test]
    fn the_title_cap_never_splits_a_character() {
        // Written as an escape, not as a literal: a precomposed é and an
        // e plus combining accent look identical in a source file and
        // would make this test measure something else.
        let wide = "\u{e9}".repeat(TITLE_MAX_CHARS * 2);
        let capped = normalize_title(&wide).unwrap();
        assert_eq!(capped.chars().count(), TITLE_MAX_CHARS);
        assert_eq!(capped.len(), TITLE_MAX_CHARS * 2, "two bytes per char");
        assert!(capped.chars().all(|c| c == '\u{e9}'));
        // Exactly at the cap is untouched — an off-by-one here would trim
        // a legal title.
        let exact = "a".repeat(TITLE_MAX_CHARS);
        assert_eq!(normalize_title(&exact), Some(exact));
    }

    /// Stage AB: a meta written before titles existed reads back unnamed,
    /// and an unnamed session writes no `title` key at all.
    ///
    /// Stage AC extends the same claim to `archived`: an engine predating
    /// the bit wrote no key, and reading one back as `true` by accident
    /// would hide a session the operator never filed away.
    #[test]
    fn meta_without_a_title_still_loads_and_stays_silent() {
        let line = r#"{"format_version":1,"provider":"claude-code","cwd":"/tmp",
            "session_id":"s1","state":"new","child":null}"#;
        let meta: Meta = serde_json::from_str(line).unwrap();
        assert_eq!(meta.title, None);
        assert!(
            !meta.archived,
            "a meta predating the archive bit must read back VISIBLE — the \
             other default silently hides every session an old engine wrote"
        );
        let back = serde_json::to_string(&meta).unwrap();
        assert!(!back.contains("title"), "{back}");
        assert!(!back.contains("archived"), "{back}");
    }

    /// A private scratch directory. `eave-core` carries no `tempfile`
    /// dependency, and a unit test is not the place to add one — pid plus
    /// a counter is unique within a run and across concurrent runs.
    fn scratch() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-store-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn meta_for(dir: &Path) -> Meta {
        let m = Meta {
            format_version: META_FORMAT_VERSION,
            provider: "claude-code".into(),
            cwd: "/tmp".into(),
            session_id: "s1".into(),
            state: "new".into(),
            child: None,
            title: None,
            archived: false,
        };
        m.save(dir).expect("save");
        m
    }

    /// Stage AC's half of the single-writer rule, in the shape the bug
    /// actually takes: the state machine rebuilds a whole `Meta` from a
    /// spawn-time snapshot, so both operator fields are `None`/`false` in
    /// the struct it is about to write.
    ///
    /// Both fields must survive that write. A version of this that kept
    /// only the title would archive-then-send and quietly un-archive.
    #[test]
    fn a_state_write_keeps_both_of_the_operators_fields() {
        let dir = scratch();
        let dir = dir.as_path();
        let mut meta = meta_for(dir);

        // The operator's two writes, each touching exactly one field.
        meta.title = normalize_title("chase the flake");
        meta.save(dir).expect("rename");
        let mut meta = Meta::load(dir).expect("load");
        meta.archived = true;
        meta.save(dir).expect("archive");

        // Now the state machine, from its own snapshot — which knows
        // nothing about either.
        let mut spawn_snapshot = Meta {
            format_version: META_FORMAT_VERSION,
            provider: "claude-code".into(),
            cwd: "/tmp".into(),
            session_id: "s1".into(),
            state: "running".into(),
            child: None,
            title: None,
            archived: false,
        };
        spawn_snapshot
            .save_keeping_operator_fields(dir)
            .expect("state write");

        let after = Meta::load(dir).expect("reload");
        assert_eq!(after.state, "running", "the state write did not land");
        assert_eq!(
            after.title.as_deref(),
            Some("chase the flake"),
            "a turn erased the name the operator gave the session"
        );
        assert!(
            after.archived,
            "a turn un-archived the session — the archive bit has one \
             writer and the state machine is not it"
        );
    }

    /// The bit is durable across a reload, and reversible: flipping it
    /// back is the same field, not a second one that can disagree.
    #[test]
    fn archiving_is_one_bit_and_it_flips_both_ways() {
        let dir = scratch();
        let dir = dir.as_path();
        let mut meta = meta_for(dir);
        assert!(!meta.archived, "a fresh session is not archived");

        meta.archived = true;
        meta.save(dir).expect("archive");
        assert!(Meta::load(dir).expect("load").archived);

        meta.archived = false;
        meta.save(dir).expect("unarchive");
        assert!(!Meta::load(dir).expect("load").archived);
        let raw = std::fs::read_to_string(Store::meta_path(dir)).expect("read");
        assert!(
            !raw.contains("archived"),
            "an unarchived session left the key behind: {raw}"
        );
    }
}
