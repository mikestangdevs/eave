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

    /// Save, keeping whatever [`title`](Meta::title) is on disk (Stage AB).
    ///
    /// `meta.json` has two writers with different concerns: the state
    /// machine (state, child, resume cursor) rewrites it on every
    /// transition, and a rename writes exactly one field. Because every
    /// save rewrites the whole file, either writer erases the other's work
    /// unless one rule holds — **`rename` is the only writer of `title`,
    /// and every other writer preserves it.** This is that rule.
    ///
    /// Without it the bug is quiet and specific: name a session, send it a
    /// message, and the spawn's meta write drops the name you just gave it.
    pub fn save_keeping_title(&mut self, session_dir: &Path) -> io::Result<()> {
        if let Ok(on_disk) = Self::load(session_dir) {
            self.title = on_disk.title;
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
    #[test]
    fn meta_without_a_title_still_loads_and_stays_silent() {
        let line = r#"{"format_version":1,"provider":"claude-code","cwd":"/tmp",
            "session_id":"s1","state":"new","child":null}"#;
        let meta: Meta = serde_json::from_str(line).unwrap();
        assert_eq!(meta.title, None);
        let back = serde_json::to_string(&meta).unwrap();
        assert!(!back.contains("title"), "{back}");
    }
}
