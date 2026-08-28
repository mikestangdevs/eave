//! The append-only event log the browser stores are built on.
//!
//! Same *discipline* as the session journal (`crate::journal`): line 1 is a
//! header, every later line is one `{"seq":N,"ts_ms":M,"event":{…}}` record,
//! writes are append-only, and a partial final line is the expected crash
//! shape and is truncated away on load.
//!
//! It is a **separate implementation on purpose**, and the three places it
//! diverges are the reason:
//!
//! 1. **A seq break is a fact, not a corruption.** The session journal
//!    refuses one because clients resume against `(epoch, seq)` and a gap
//!    there means a client would silently miss records (spec 05 §Cursor).
//!    Nothing resumes against these logs — they are folded whole at load and
//!    never streamed by seq — so a gap has no client-visible meaning and
//!    losing every bookmark over one is a worse outcome than stating it.
//! 2. **Mid-file corruption skips the line instead of refusing the file.**
//!    That is only safe because every browser fold is *total*: an event
//!    naming an id the fold has never seen is a no-op, so dropping an
//!    `Added` cannot make a later `Moved` do something wrong. The count of
//!    skipped lines is returned in [`LoadReport`] and is meant to be said
//!    out loud, never swallowed.
//! 3. **An unknown `format_version` degrades to READ-ONLY, never to empty.**
//!    Refusing to start is not an option for a browser, and starting empty
//!    over a future version's file would destroy it on the first write. The
//!    log reports itself sealed and every append is refused.
//!
//! What does NOT diverge: append-only between compactions, torn-tail
//! truncation, fsync on the header and on [`EventLog::sync`], and the
//! directory fsync that makes a freshly created file's existence durable.
//!
//! # Compaction
//! [`EventLog::compact`] is the one operation in this codebase that rewrites
//! a log. It writes a fresh file beside the old one, fsyncs it, and renames
//! it over the top. It exists for exactly one caller — forgetting browsing
//! history — and [`crate::browser::history`] explains why that is not the
//! same act as rewriting an agent's audit trail.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::journal::now_ms;

pub const LOG_FORMAT_VERSION: u32 = 1;

/// Line 1 of every browser log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogHeader {
    pub format_version: u32,
    /// Which log this is (`tabs`, `bookmarks`, `history`). Written so a
    /// file that ends up under the wrong name is caught by its content
    /// rather than believed because of its path.
    pub kind: String,
    pub created_ts_ms: u64,
}

/// One line after the header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord<E> {
    pub seq: u64,
    pub ts_ms: u64,
    pub event: E,
}

/// What a load found, in facts. Every non-default field here is something
/// the operator is entitled to be told; nothing in this struct is an
/// internal detail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// A partial final line was found and truncated — the ordinary crash
    /// shape, worth stating but not alarming.
    pub torn_tail: bool,
    /// Line numbers that would not parse and were skipped. Non-empty means
    /// events were lost.
    pub skipped_lines: Vec<usize>,
    /// Seq numbers were not contiguous. Records were still applied.
    pub seq_breaks: usize,
    /// The header named a `format_version` this build does not know. The
    /// log is [`EventLog::sealed`] and will refuse every append; the file
    /// on disk is untouched.
    pub unknown_format_version: Option<u32>,
    /// The header was missing or unparseable and the file was renamed out
    /// of the way. A fresh log was started in its place.
    pub quarantined: Option<PathBuf>,
}

impl LoadReport {
    /// True when the load lost something or refused something — the
    /// condition a caller must not pass over in silence.
    pub fn is_clean(&self) -> bool {
        self.skipped_lines.is_empty()
            && self.seq_breaks == 0
            && self.unknown_format_version.is_none()
            && self.quarantined.is_none()
    }

    /// The sentence to put in front of a person, or `None` when there is
    /// nothing to say. A torn tail alone says nothing: it is the normal
    /// consequence of a crash and no event was lost by it.
    pub fn stated(&self) -> Option<String> {
        if self.is_clean() {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(v) = self.unknown_format_version {
            parts.push(format!(
                "this file is format version {v} and this build reads \
                 {LOG_FORMAT_VERSION} — it has been left alone and nothing \
                 will be written to it"
            ));
        }
        if let Some(p) = &self.quarantined {
            parts.push(format!("the previous file was moved to {}", p.display()));
        }
        if !self.skipped_lines.is_empty() {
            parts.push(format!(
                "{} unreadable line(s) were skipped",
                self.skipped_lines.len()
            ));
        }
        if self.seq_breaks > 0 {
            parts.push(format!(
                "{} gap(s) in the record numbering",
                self.seq_breaks
            ));
        }
        Some(parts.join("; "))
    }
}

/// An append-only log of `E`, opened once and held for the process's life.
pub struct EventLog<E> {
    path: PathBuf,
    kind: String,
    file: Option<File>,
    buf: Vec<u8>,
    next_seq: u64,
    /// Set when the on-disk file is from a format this build does not
    /// know. Every append is refused; the file is never touched.
    sealed: bool,
    _marker: PhantomData<fn() -> E>,
}

impl<E: Serialize + DeserializeOwned> EventLog<E> {
    /// Open (creating if absent) and read every record back.
    ///
    /// Returns the events in order plus the [`LoadReport`]. The caller folds
    /// the events and is expected to state the report.
    pub fn open(path: &Path, kind: &str) -> io::Result<(Self, Vec<E>, LoadReport)> {
        let mut report = LoadReport::default();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        if !path.exists() {
            let log = Self::create(path, kind)?;
            return Ok((log, Vec::new(), report));
        }

        let bytes = fs::read(path)?;
        // Segment into lines, keeping byte offsets so a torn tail can be
        // truncated at the end of the last whole record.
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

        let header: Option<LogHeader> = segments
            .first()
            .and_then(|(_, seg, _)| serde_json::from_slice(seg).ok());
        let Some(header) = header else {
            // No readable header: this is not one of our logs (or its
            // first line was destroyed). Move it aside — never parse a
            // file we cannot identify — and start fresh.
            let dest = quarantine(path);
            report.quarantined = Some(dest);
            let log = Self::create(path, kind)?;
            return Ok((log, Vec::new(), report));
        };
        if header.format_version != LOG_FORMAT_VERSION {
            // Read-only degradation. Deliberately NOT a quarantine: the
            // file is a newer build's and is still that build's to read.
            report.unknown_format_version = Some(header.format_version);
            return Ok((
                Self {
                    path: path.to_path_buf(),
                    kind: kind.to_string(),
                    file: None,
                    buf: Vec::new(),
                    next_seq: 1,
                    sealed: true,
                    _marker: PhantomData,
                },
                Vec::new(),
                report,
            ));
        }

        let mut events = Vec::new();
        let mut last_seq = 0u64;
        let mut truncate_to: Option<u64> = None;
        for (idx, &(offset, seg, terminated)) in segments.iter().enumerate().skip(1) {
            let is_last = idx == segments.len() - 1;
            match serde_json::from_slice::<LogRecord<E>>(seg) {
                Ok(rec) => {
                    if rec.seq != last_seq + 1 {
                        report.seq_breaks += 1;
                    }
                    last_seq = rec.seq;
                    events.push(rec.event);
                    if is_last && !terminated {
                        // A whole record that merely lost its newline.
                        // Completing the line keeps the next append from
                        // concatenating onto it.
                        let mut f = OpenOptions::new().append(true).open(path)?;
                        f.write_all(b"\n")?;
                        f.sync_all()?;
                    }
                }
                Err(_) if is_last && !terminated => {
                    // The expected crash shape: a partial final line.
                    truncate_to = Some(offset as u64);
                    report.torn_tail = true;
                }
                Err(_) => report.skipped_lines.push(idx + 1),
            }
        }
        if let Some(len) = truncate_to {
            let f = OpenOptions::new().write(true).open(path)?;
            f.set_len(len)?;
            f.sync_all()?;
        }

        let file = OpenOptions::new().append(true).open(path)?;
        Ok((
            Self {
                path: path.to_path_buf(),
                kind: kind.to_string(),
                file: Some(file),
                buf: Vec::new(),
                next_seq: last_seq + 1,
                sealed: false,
                _marker: PhantomData,
            },
            events,
            report,
        ))
    }

    fn create(path: &Path, kind: &str) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .append(false)
            .open(path)?;
        let header = LogHeader {
            format_version: LOG_FORMAT_VERSION,
            kind: kind.to_string(),
            created_ts_ms: now_ms(),
        };
        let mut line = serde_json::to_vec(&header).map_err(invalid)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_all()?;
        // The file's own existence has to survive a crash too, or the next
        // start reads an absent log as "no bookmarks" (journal.rs does the
        // same for the same reason).
        if let Some(dir) = path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        drop(file);
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            kind: kind.to_string(),
            file: Some(file),
            buf: Vec::new(),
            next_seq: 1,
            sealed: false,
            _marker: PhantomData,
        })
    }

    /// A log opened from a future format version. Every append is refused
    /// and the file is never written.
    pub fn sealed(&self) -> bool {
        self.sealed
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Buffer one event. Reaches the OS on [`EventLog::flush`].
    pub fn append(&mut self, event: &E) -> io::Result<()> {
        if self.sealed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} log at {} is from a newer format version and is not \
                     ours to write",
                    self.kind,
                    self.path.display()
                ),
            ));
        }
        let record = LogRecord {
            seq: self.next_seq,
            ts_ms: now_ms(),
            event,
        };
        let mut line = serde_json::to_vec(&record).map_err(invalid)?;
        line.push(b'\n');
        self.buf.extend_from_slice(&line);
        self.next_seq += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }

    /// Flush and fsync. Used where losing the last event would be a
    /// visible lie rather than a small loss — see each store's own rule.
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        match self.file.as_mut() {
            Some(f) => f.sync_all(),
            None => Ok(()),
        }
    }

    /// Rewrite the log so it contains exactly `events`, then fsync and
    /// rename it into place.
    ///
    /// **The only rewrite in this codebase, and it is not the session
    /// journal's rule bending.** That rule protects an audit trail of what
    /// an agent did to somebody's disk. This one serves the opposite duty:
    /// a person asked to be forgotten, and a log that kept a tombstone
    /// naming the url would not have forgotten anything. Deletion here has
    /// to be physical or it is theatre.
    pub fn compact(&mut self, events: &[E]) -> io::Result<()> {
        if self.sealed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} log is sealed; refusing to compact", self.kind),
            ));
        }
        self.flush()?;
        let tmp = self.path.with_extension("jsonl.compacting");
        {
            let mut f = File::create(&tmp)?;
            let header = LogHeader {
                format_version: LOG_FORMAT_VERSION,
                kind: self.kind.clone(),
                created_ts_ms: now_ms(),
            };
            let mut out = serde_json::to_vec(&header).map_err(invalid)?;
            out.push(b'\n');
            for (i, event) in events.iter().enumerate() {
                let record = LogRecord {
                    seq: i as u64 + 1,
                    ts_ms: now_ms(),
                    event,
                };
                out.extend_from_slice(&serde_json::to_vec(&record).map_err(invalid)?);
                out.push(b'\n');
            }
            f.write_all(&out)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        self.next_seq = events.len() as u64 + 1;
        Ok(())
    }
}

fn quarantine(path: &Path) -> PathBuf {
    let ts = now_ms() / 1000;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "log".into());
    let dest = path.with_file_name(format!("{name}.corrupt-{ts}"));
    let _ = fs::rename(path, &dest);
    dest
}

fn invalid(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Ev {
        n: u32,
    }

    /// A private scratch directory. `eave-core` carries no `tempfile`
    /// dependency and a unit test is not the place to add one — pid plus a
    /// counter is unique within a run and across concurrent runs, which is
    /// exactly what `store.rs`'s own tests do.
    fn scratch() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("eave-blog-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_three(path: &Path) {
        let (mut log, events, report) = EventLog::<Ev>::open(path, "test").unwrap();
        assert!(events.is_empty());
        assert!(report.is_clean());
        for n in 1..=3 {
            log.append(&Ev { n }).unwrap();
        }
        log.sync().unwrap();
    }

    #[test]
    fn a_fresh_log_round_trips_every_event() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        let (_log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events, vec![Ev { n: 1 }, Ev { n: 2 }, Ev { n: 3 }]);
        assert!(report.is_clean());
        assert_eq!(report.stated(), None, "a clean load has nothing to say");
    }

    /// The expected crash shape: a partial final line. It is truncated
    /// away, the load succeeds, and the next append starts on a fresh
    /// line rather than concatenating onto the ruin.
    #[test]
    fn a_torn_final_line_is_truncated_and_appending_continues() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        // Simulate the tear: append half a record with no newline.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{\"seq\":4,\"ts_ms\":0,\"eve").unwrap();
        }
        let (mut log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events.len(), 3, "the whole records survived");
        assert!(report.torn_tail);
        assert!(
            report.is_clean(),
            "a torn tail is the normal crash shape and loses nothing — it \
             must not read as data loss"
        );
        log.append(&Ev { n: 4 }).unwrap();
        log.sync().unwrap();
        let (_log, events, _) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events.len(), 4, "the append landed on its own line");
    }

    /// A whole record that merely lost its newline is NOT a tear. It is
    /// kept, and the line is completed so the next append does not run
    /// into it.
    #[test]
    fn a_valid_last_line_missing_only_its_newline_is_kept() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        let (mut log, _, _) = EventLog::<Ev>::open(&path, "test").unwrap();
        log.append(&Ev { n: 1 }).unwrap();
        log.sync().unwrap();
        // Strip the trailing newline.
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(&path, raw.trim_end_matches('\n')).unwrap();

        let (mut log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events, vec![Ev { n: 1 }], "a whole record was thrown away");
        assert!(!report.torn_tail);
        log.append(&Ev { n: 2 }).unwrap();
        log.sync().unwrap();
        let (_log, events, _) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events, vec![Ev { n: 1 }, Ev { n: 2 }]);
    }

    /// Where this log deliberately parts company with the session journal:
    /// a bad line in the MIDDLE is skipped and counted, not a refusal of
    /// the whole file. Losing every bookmark over one bad line is the worse
    /// outcome, and it is only safe because every browser fold is total.
    #[test]
    fn a_bad_line_in_the_middle_is_skipped_and_counted_not_fatal() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        let mut lines: Vec<String> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[2] = "{ this is not json".into();
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let (_log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(
            events,
            vec![Ev { n: 1 }, Ev { n: 3 }],
            "the good lines survived"
        );
        assert_eq!(report.skipped_lines, vec![3]);
        assert!(!report.is_clean());
        let said = report.stated().expect("a skipped line must be stated");
        assert!(said.contains("1 unreadable line"), "{said}");
    }

    /// A seq gap is a fact here, not corruption: nothing resumes against
    /// these logs by seq, so a gap has no client-visible meaning.
    #[test]
    fn a_seq_gap_is_stated_and_the_records_still_apply() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(&path, raw.replace("\"seq\":2", "\"seq\":9")).unwrap();

        let (_log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events.len(), 3, "records were dropped over a numbering gap");
        assert!(report.seq_breaks > 0);
        assert!(report.stated().unwrap().contains("gap"));
    }

    /// A file from a newer build degrades to READ-ONLY. It is not
    /// quarantined and it is not started over: either would destroy a
    /// future version's data on first write.
    #[test]
    fn a_future_format_version_seals_the_log_and_leaves_the_file_alone() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        let raw = fs::read_to_string(&path).unwrap();
        let bumped = raw.replacen(
            &format!("\"format_version\":{LOG_FORMAT_VERSION}"),
            "\"format_version\":99",
            1,
        );
        fs::write(&path, &bumped).unwrap();

        let (mut log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert!(log.sealed());
        assert!(
            events.is_empty(),
            "a sealed log must not hand back a partial read"
        );
        assert_eq!(report.unknown_format_version, Some(99));
        assert!(
            report.quarantined.is_none(),
            "a future file must not be renamed"
        );
        assert!(
            log.append(&Ev { n: 4 }).is_err(),
            "a sealed log accepted a write"
        );
        assert!(
            log.compact(&[]).is_err(),
            "a sealed log accepted a compaction"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            bumped,
            "the future build's file was modified"
        );
    }

    /// A file whose header cannot be read is not one of ours. It is moved
    /// aside — never parsed on the assumption it might be — and a fresh log
    /// starts in its place, with both facts stated.
    #[test]
    fn an_unreadable_header_quarantines_and_starts_fresh() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        fs::write(&path, "not a header at all\n{\"seq\":1}\n").unwrap();

        let (mut log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert!(events.is_empty());
        let quarantined = report.quarantined.clone().expect("must be renamed aside");
        assert!(
            quarantined.exists(),
            "the original was destroyed, not moved"
        );
        assert!(report.stated().unwrap().contains("moved to"));
        // The fresh log works.
        log.append(&Ev { n: 1 }).unwrap();
        log.sync().unwrap();
        let (_log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events, vec![Ev { n: 1 }]);
        assert!(report.is_clean());
    }

    /// Compaction rewrites the file to exactly what it is given, renumbered
    /// from 1, and appending afterwards continues from there.
    #[test]
    fn compaction_replaces_the_file_and_appends_continue_after_it() {
        let dir = scratch();
        let path = dir.join("test.jsonl");
        write_three(&path);
        let (mut log, _, _) = EventLog::<Ev>::open(&path, "test").unwrap();
        log.compact(&[Ev { n: 3 }]).unwrap();
        log.append(&Ev { n: 4 }).unwrap();
        log.sync().unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"n\":1"),
            "compaction left the dropped event behind: {raw}"
        );
        let (_log, events, report) = EventLog::<Ev>::open(&path, "test").unwrap();
        assert_eq!(events, vec![Ev { n: 3 }, Ev { n: 4 }]);
        assert!(
            report.is_clean(),
            "compaction produced a file that does not load cleanly: {report:?}"
        );
        // No leftover temp file beside it.
        assert!(!path.with_extension("jsonl.compacting").exists());
    }
}
