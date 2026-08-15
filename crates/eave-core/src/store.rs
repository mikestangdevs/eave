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

use crate::proc::ProcIdentity;

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

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another engine instance holds the lock for this data dir")]
    Busy,
    #[error("io error acquiring engine lock: {0}")]
    Io(#[from] io::Error),
}

/// Held for the engine's lifetime; the flock releases on drop/exit —
/// including SIGKILL, which is what makes it safe as a liveness signal.
#[derive(Debug)]
pub struct EngineLock {
    _file: File,
}

impl EngineLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(data_dir.join("engine.lock"))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Self { _file: file })
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Err(LockError::Busy)
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

    pub fn load(session_dir: &Path) -> io::Result<Self> {
        let bytes = fs::read(Store::meta_path(session_dir))?;
        serde_json::from_slice(&bytes).map_err(invalid)
    }
}

fn invalid(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}
