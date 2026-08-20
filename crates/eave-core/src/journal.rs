//! Append-only JSONL session journal (spec 02).
//!
//! # File format
//! Line 1 is a [`Header`]; every subsequent line is one [`Record`]
//! (`{"seq":N,"ts_ms":M,"event":{...}}`) with `seq` strictly increasing
//! from 1. Append-only: repairs are appends, history is never rewritten in
//! place.
//!
//! # Durability contract
//! - A record is exactly one `\n`-terminated line written by a single
//!   writer. Because the file is never rewritten in place, the only torn
//!   state a crash can produce is a **partial final line**.
//! - fsync points: the header at creation, and every terminal record
//!   (`TurnCompleted`, `Closed`) at append. Everything else is buffered in
//!   memory and written to the OS on the ~120 ms coalesce tick
//!   (`thresholds::JOURNAL_FLUSH_INTERVAL`) — dsh's write-behind + fsync'd
//!   terminal appends (research: deepseek-harness.md §3).
//! - On load: an unparseable **final** line is the torn-write signature —
//!   the file is truncated to the last valid record and loading proceeds
//!   (comet-style torn-line tolerance, research: comet.md §3). An
//!   unparseable line **before** the final line, or a `seq` break, is
//!   mid-file corruption: refuse, rename to `journal.corrupt-<ts>`, surface
//!   the error. Never partial-load silently (spec 02 §Corruption).
//! - Unknown `format_version`: refuse loudly, file left in place.
//!
//! The journal persists every provider line the parser does not model —
//! all `system` subtypes and unknown top-level types — verbatim via the
//! `raw` field on those events, so recording never requires a parser change
//! (build directive 2026-08-15).

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session::AgentConfig;
use crate::events::SessionEvent;

pub const FORMAT_VERSION: u32 = 1;

/// Line 1 of every journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub format_version: u32,
    /// The provider-native resume cursor — caller-chosen at spawn, so it
    /// exists before the first turn does (spec 02 §Resume contract).
    pub session_id: String,
    pub provider: String,
    pub cwd: String,
    pub created_ts_ms: u64,
    /// What agent this session runs (Slice P2) — chosen at open, written
    /// here once, and read back on every load, which is what makes it
    /// survive a restart and govern a `--resume` rather than the engine's
    /// current environment doing so.
    ///
    /// It lives in the HEADER, not in `meta.json`, because it is a fact
    /// about the session's identity that must not drift: the journal is
    /// append-only and the header is written before anything spawns, so
    /// the config a turn ran under is recoverable from history. `meta` is
    /// the mutable file and gets rewritten on every state change.
    ///
    /// **`format_version` did NOT move for this.** `load` refuses any
    /// version but its own, so a bump would make every existing journal
    /// unreadable; the field is `#[serde(default)]` instead, so an old
    /// header reads as `AgentConfig::default()` — "use the engine's
    /// defaults", which is exactly what those sessions did.
    #[serde(default, skip_serializing_if = "is_default_agent")]
    pub agent: AgentConfig,
}

fn is_default_agent(a: &AgentConfig) -> bool {
    a == &AgentConfig::default()
}

/// One journal line after the header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts_ms: u64,
    pub event: JournalEvent,
}

/// The journal's event vocabulary: everything the parser produced, plus the
/// engine-side records the provider stream cannot carry (the user's own
/// sends, and the synthetic closers crash-tail repair appends — spec 02
/// §Resume contract step 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEvent {
    /// A parsed provider event, verbatim from the stream parser.
    Provider { event: SessionEvent },
    /// A user message at the moment the engine *attempted* to send it —
    /// journaled before the pipe write, so an interrupted turn still shows
    /// what was asked even if the CLI died first. Delivery is a separate
    /// fact: without a matching `UserDelivered`, this record is
    /// journaled-but-undelivered (spec 02 §Delivery state).
    UserSent { text: String },
    /// Confirmation that the pipe write for the most recent unconfirmed
    /// `UserSent` reached the agent's stdin (order-paired; sends are
    /// sequential in v0). Absent confirmation in a terminal journal =
    /// undelivered — the UI must not render the message as sent.
    UserDelivered {},
    /// The operator's answer to one `ApprovalRequested` (Slice K), written
    /// at the moment it provably reached the agent. This is the audit
    /// trail that makes "eave never auto-approves" checkable after the
    /// fact rather than merely asserted: every allowed tool call in a
    /// journal has a human decision recorded against it, by request id.
    /// `message` is the operator's own words on a deny — the provider
    /// hands them to the model verbatim, so they are part of the history.
    ApprovalDecided {
        request_id: String,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Synthetic closer: a tool that was outstanding when the session ended
    /// without a terminal event. Outcome unknown — never invented.
    ToolInterrupted { id: String },
    /// Synthetic closer: the open turn ended without a `result`.
    TurnInterrupted { reason: String },
    /// Terminal record for one **process segment**. A journal that was
    /// continued via `--resume` (spec 02 §Resume contract) holds one
    /// segment per CLI process, each ending in `Closed`; the journal as a
    /// whole is terminal iff its *last* record is `Closed`. A `Closed`
    /// finalizes everything in flight in its segment: unconfirmed sends
    /// become undelivered, outstanding tools can never resolve.
    Closed { outcome: Outcome },
}

/// How the session ended. `Interrupted` is the crash-tail repair verdict:
/// the journal simply stopped — ambiguity settles as interrupted, never as
/// completion (spec 02 §Resume contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Killed {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Crashed {
        code: Option<i32>,
        signal: Option<i32>,
        stderr_tail: String,
    },
    Interrupted,
}

impl Outcome {
    /// The terminal state name recorded in `meta.json` (spec 01 §States).
    pub fn state_name(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Killed { .. } => "killed",
            Outcome::Crashed { .. } => "failed",
            Outcome::Interrupted => "interrupted",
        }
    }
}

/// Classify exit facts into a terminal outcome. The Claude CLI catches
/// SIGTERM and exits with code 143 rather than dying by signal (research:
/// claude-stream-notes.md §Kill/exit facts) — 128+SIGTERM is a normal kill
/// outcome, not a crash. Raw TERM/KILL signal deaths are our own kill
/// ladder's doing and classify the same way.
pub fn classify_exit(code: Option<i32>, signal: Option<i32>, stderr_tail: &str) -> Outcome {
    match (code, signal) {
        (Some(0), _) => Outcome::Done,
        (Some(143), _) => Outcome::Killed { code, signal },
        (_, Some(sig)) if sig == libc::SIGTERM || sig == libc::SIGKILL => {
            Outcome::Killed { code, signal }
        }
        _ => Outcome::Crashed {
            code,
            signal,
            stderr_tail: stderr_tail.to_string(),
        },
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Write path

pub struct JournalWriter {
    file: File,
    buf: Vec<u8>,
    next_seq: u64,
}

impl JournalWriter {
    /// Create a fresh journal: header written and fsynced before any event
    /// (spec 02 §Durability). The containing directory is fsynced too so the
    /// file's existence survives a crash.
    pub fn create(path: &Path, header: &Header) -> io::Result<Self> {
        let mut file = OpenOptions::new().create_new(true).append(true).open(path)?;
        let mut line = serde_json::to_vec(header).map_err(to_io)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_all()?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(Self {
            file,
            buf: Vec::new(),
            next_seq: 1,
        })
    }

    /// Reopen an existing journal for repair appends, continuing at
    /// `next_seq` (from [`LoadedJournal::next_seq`]).
    pub fn append_existing(path: &Path, next_seq: u64) -> io::Result<Self> {
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self {
            file,
            buf: Vec::new(),
            next_seq,
        })
    }

    /// Buffer one record (write-behind — reaches the OS on `flush`).
    /// Returns the record as written, seq and timestamp stamped.
    pub fn append(&mut self, event: JournalEvent) -> io::Result<Record> {
        let record = Record {
            seq: self.next_seq,
            ts_ms: now_ms(),
            event,
        };
        let mut line = serde_json::to_vec(&record).map_err(to_io)?;
        line.push(b'\n');
        self.buf.extend_from_slice(&line);
        self.next_seq += 1;
        Ok(record)
    }

    /// Write buffered records to the OS (no fsync).
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.file.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Flush and fsync — used for the terminal records (spec 02 §Durability).
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        self.file.sync_all()
    }
}

fn to_io(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

// ---------------------------------------------------------------------------
// Load path

#[derive(Debug)]
pub struct LoadedJournal {
    pub header: Header,
    pub records: Vec<Record>,
    /// A partial final line was found and truncated away.
    pub torn_tail_repaired: bool,
    /// What crash-tail repair did during this load (`already_terminal` when
    /// the journal needed nothing).
    pub repair: RepairOutcome,
}

impl LoadedJournal {
    pub fn next_seq(&self) -> u64 {
        self.records.last().map(|r| r.seq + 1).unwrap_or(1)
    }

    /// Terminal = the last record is `Closed`. Always true after `load`,
    /// which applies crash-tail repair itself.
    pub fn is_terminal(&self) -> bool {
        is_terminal(&self.records)
    }

    /// Seqs of `UserSent` records with no `UserDelivered` confirmation.
    /// `load` always returns a terminal journal, so unconfirmed means
    /// undelivered — the UI must render these as not-sent (spec 02
    /// §Delivery state). Pairing never crosses a `Closed` segment boundary:
    /// a later `--resume` segment's confirmation cannot apply to a send
    /// whose process is already gone.
    pub fn undelivered_user_sends(&self) -> Vec<u64> {
        let mut pending: Vec<u64> = Vec::new();
        let mut undelivered: Vec<u64> = Vec::new();
        for rec in &self.records {
            match &rec.event {
                JournalEvent::UserSent { .. } => pending.push(rec.seq),
                JournalEvent::UserDelivered {} => {
                    if !pending.is_empty() {
                        pending.remove(0);
                    }
                }
                JournalEvent::Closed { .. } => undelivered.append(&mut pending),
                _ => {}
            }
        }
        undelivered.append(&mut pending);
        undelivered
    }
}

fn is_terminal(records: &[Record]) -> bool {
    matches!(
        records.last(),
        Some(Record {
            event: JournalEvent::Closed { .. },
            ..
        })
    )
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("journal has no readable header; quarantined at {renamed_to:?}")]
    MissingHeader { renamed_to: PathBuf },
    #[error("unknown journal format_version {found} (this build reads {FORMAT_VERSION}); refusing to parse")]
    UnknownFormatVersion { found: u64 },
    #[error("journal corrupt at line {line_no} ({detail}); quarantined at {renamed_to:?}")]
    Corrupt {
        line_no: usize,
        detail: String,
        renamed_to: PathBuf,
    },
}

/// **The single read path** (spec 02 §Resume contract; build directive
/// 2026-08-16): parse, apply torn-tail repair, apply crash-tail repair,
/// and return header + events + repair outcome together. There is no
/// header-only read — Stage C takes the `--resume` cursor from
/// `LoadedJournal::header`, so one parser owns one set of corruption
/// semantics. Corruption before the tail refuses the whole file — never a
/// silent partial load (spec 02 §Corruption).
pub fn load(path: &Path) -> Result<LoadedJournal, LoadError> {
    let bytes = fs::read(path)?;

    // Segment into lines, keeping byte offsets for truncation.
    let mut segments: Vec<(usize, &[u8], bool)> = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            segments.push((start, &bytes[start..i], true));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        segments.push((start, &bytes[start..], false));
    }

    let Some(&(_, header_bytes, header_terminated)) = segments.first() else {
        return Err(LoadError::MissingHeader {
            renamed_to: quarantine(path),
        });
    };
    let header: Header = match serde_json::from_slice(header_bytes) {
        Ok(h) => h,
        Err(_) => {
            // Check the version field alone before quarantining: an intact
            // header from a future format must refuse loudly, not rename.
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(header_bytes) {
                if let Some(found) = v.get("format_version").and_then(|v| v.as_u64()) {
                    if found != u64::from(FORMAT_VERSION) {
                        return Err(LoadError::UnknownFormatVersion { found });
                    }
                }
            }
            return Err(LoadError::MissingHeader {
                renamed_to: quarantine(path),
            });
        }
    };
    if header.format_version != FORMAT_VERSION {
        return Err(LoadError::UnknownFormatVersion {
            found: u64::from(header.format_version),
        });
    }

    let mut records: Vec<Record> = Vec::new();
    let mut torn_tail_repaired = false;
    let mut last_ok_unterminated = !header_terminated && segments.len() == 1;
    for (idx, &(offset, seg, terminated)) in segments.iter().enumerate().skip(1) {
        let is_last = idx == segments.len() - 1;
        let line_no = idx + 1;
        match serde_json::from_slice::<Record>(seg) {
            Ok(rec) => {
                let expected = records.last().map(|r| r.seq + 1).unwrap_or(1);
                if rec.seq != expected {
                    return Err(LoadError::Corrupt {
                        line_no,
                        detail: format!("seq {} where {} expected", rec.seq, expected),
                        renamed_to: quarantine(path),
                    });
                }
                records.push(rec);
                last_ok_unterminated = !terminated;
            }
            Err(e) if is_last => {
                // Torn write: truncate to the end of the last valid record
                // and proceed (spec 02 §Corruption — expected crash shape).
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(offset as u64)?;
                f.sync_all()?;
                let _ = e;
                torn_tail_repaired = true;
                last_ok_unterminated = false;
            }
            Err(e) => {
                return Err(LoadError::Corrupt {
                    line_no,
                    detail: e.to_string(),
                    renamed_to: quarantine(path),
                });
            }
        }
    }

    // A valid final line that lost only its newline: complete the line so
    // future appends start fresh.
    if last_ok_unterminated {
        let mut f = OpenOptions::new().append(true).open(path)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }

    let repair = crash_tail_repair(path, &mut records)?;
    Ok(LoadedJournal {
        header,
        records,
        torn_tail_repaired,
        repair,
    })
}

fn quarantine(path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = path.with_file_name(format!("journal.corrupt-{ts}"));
    let _ = fs::rename(path, &dest);
    dest
}

// ---------------------------------------------------------------------------
// Crash-tail repair

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    /// The journal was already terminal; nothing was appended.
    pub already_terminal: bool,
    /// Tools outstanding at the point of death, closed with unknown outcome.
    pub tools_interrupted: Vec<String>,
    /// An open turn was settled as interrupted.
    pub turn_interrupted: bool,
}

/// Append synthetic closers to a non-terminal journal: unknown-outcome
/// closers for in-flight tools, an interrupted turn end if a turn was open,
/// and a `Closed { Interrupted }` terminal record. Ambiguity settles as
/// interrupted — never invented completion (spec 02 §Resume contract).
/// Private on purpose: `load` is the only door, so no caller can see an
/// unrepaired journal.
fn crash_tail_repair(path: &Path, records: &mut Vec<Record>) -> io::Result<RepairOutcome> {
    // A header-only journal is a session that was opened but never ran —
    // nothing was in flight, so there is nothing to settle as interrupted
    // (Stage C opens the journal at `Open`, before any spawn).
    if records.is_empty() || is_terminal(records) {
        return Ok(RepairOutcome {
            already_terminal: true,
            tools_interrupted: Vec::new(),
            turn_interrupted: false,
        });
    }

    // Derive in-flight tools and turn-openness purely from the log —
    // log-derived state is the whole point of the format (spec 02 §Choice).
    // Any assistant activity (deltas, snapshots) means a turn was open.
    let mut in_flight: BTreeSet<String> = BTreeSet::new();
    let mut turn_open = false;
    for rec in records.iter() {
        match &rec.event {
            JournalEvent::Provider { event } => match event {
                SessionEvent::ToolUseStarted { id, .. } => {
                    in_flight.insert(id.clone());
                    turn_open = true;
                }
                SessionEvent::ToolUseFinished { id, .. } => {
                    in_flight.remove(id);
                }
                SessionEvent::UserEcho { .. }
                | SessionEvent::TextDelta { .. }
                | SessionEvent::AssistantText { .. }
                // A permission question only exists mid-turn (Slice K).
                | SessionEvent::ApprovalRequested { .. } => turn_open = true,
                SessionEvent::TurnCompleted { .. } => turn_open = false,
                _ => {}
            },
            JournalEvent::UserSent { .. } | JournalEvent::UserDelivered {} => turn_open = true,
            // A pending approval needs no synthetic closer: the `Closed`
            // record below settles it in the shared fold (spec 05
            // §Engine semantics), exactly as it settles a pending send.
            JournalEvent::ApprovalDecided { .. } => {}
            JournalEvent::ToolInterrupted { id } => {
                in_flight.remove(id);
            }
            JournalEvent::TurnInterrupted { .. } => turn_open = false,
            // A segment end settles everything in flight — a tool from a
            // previous process segment can never resolve, so repair of a
            // later segment must not close it again.
            JournalEvent::Closed { .. } => {
                in_flight.clear();
                turn_open = false;
            }
        }
    }

    let next_seq = records.last().map(|r| r.seq + 1).unwrap_or(1);
    let mut writer = JournalWriter::append_existing(path, next_seq)?;
    let tools_interrupted: Vec<String> = in_flight.into_iter().collect();
    for id in &tools_interrupted {
        let rec = writer.append(JournalEvent::ToolInterrupted { id: id.clone() })?;
        records.push(rec);
    }
    if turn_open {
        let rec = writer.append(JournalEvent::TurnInterrupted {
            reason: "session ended without a terminal event".into(),
        })?;
        records.push(rec);
    }
    let rec = writer.append(JournalEvent::Closed {
        outcome: Outcome::Interrupted,
    })?;
    records.push(rec);
    writer.sync()?;

    Ok(RepairOutcome {
        already_terminal: false,
        tools_interrupted,
        turn_interrupted: turn_open,
    })
}
