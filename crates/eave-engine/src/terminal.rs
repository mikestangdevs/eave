//! Hosting a provider's own TUI in a pty (Slice AG1 #16).
//!
//! # Why the engine owns this and not the app
//!
//! Every other process eave creates is spawned here, and the reasons are
//! the same ones: the engine is where a pid's identity is recorded, where
//! the verified kill ladder lives (`eave_core::process`), and where an
//! orphan left by a crashed app is reaped on the next start. An app-side
//! pty would have none of that, and a TUI is exactly the process most
//! likely to be left behind — it exits when the operator says so, which
//! may be never.
//!
//! It also buys the property that makes this better than opening
//! Terminal.app beside eave: **the TUI outlives the window**. The app can
//! close, crash or be relaunched and the conversation is still there,
//! because the pty is held by the engine and re-attaching replays it.
//!
//! # What eave is NOT doing while a session is handed over
//!
//! Everything. A TUI speaks to a human over a byte stream; there are no
//! frames to decode, so for the length of a handover eave sees no turns,
//! no tool calls, no diffs and no approvals. That is not a gap to be
//! papered over later — it is what handing over MEANS, and the surface
//! says so rather than leaving a stale `running` on the rail. The journal
//! records the handover and the return so the transcript can mark the
//! stretch it did not witness.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

/// How much pty output the engine keeps so a re-attach can repaint.
///
/// A vt stream is only meaningful from a point the emulator can
/// synchronise on, so this is a REPLAY log, not a scrollback: an attaching
/// client feeds it to its emulator from the start and arrives at the
/// screen the TUI last drew. 256 KiB is far more than a full-screen repaint
/// (an 80×24 alt-screen frame with colour is a few KiB) and small enough
/// that the memory cost of a handed-over session is not worth a setting.
const REPLAY_CAP: usize = 256 * 1024;

/// Fan-out depth to attached clients. One client is the normal case; the
/// bound exists so a client that stops reading is dropped rather than
/// growing the engine's memory without limit.
const OUTPUT_LAG: usize = 1024;

/// One byte more than a chunk: the reader hands whole reads to the
/// broadcast, and this is the buffer it reads into.
const READ_CHUNK: usize = 16 * 1024;

/// A pty running one provider's TUI.
pub struct Pty {
    /// The size the pty currently believes it is. Held so an attaching
    /// client can be told what to size its emulator to before the first
    /// byte arrives, rather than guessing and repainting.
    size: Mutex<(u16, u16)>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    out: broadcast::Sender<Arc<Vec<u8>>>,
    replay: Arc<Mutex<Vec<u8>>>,
    /// The child's process GROUP, which is what a pty child is: `openpty`
    /// + `spawn_command` puts it in its own session with the slave as its
    /// controlling terminal. Signalling the group rather than the pid is
    /// the same rule the agent spawn path follows — a TUI that shelled out
    /// has children, and killing only the leader orphans them.
    pgid: Option<i32>,
    alive: Arc<AtomicBool>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Pty {
    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Vec<u8>>> {
        self.out.subscribe()
    }

    /// Everything written since the pty opened, capped at [`REPLAY_CAP`].
    ///
    /// **Take this BEFORE subscribing** and the attach duplicates a chunk;
    /// take it after and it misses one. `TerminalHost::attach` takes both
    /// under the same lock for that reason.
    pub fn replay(&self) -> Vec<u8> {
        self.replay.lock().unwrap().clone()
    }

    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    /// Tell the kernel the window changed, which is what makes the TUI
    /// redraw at the new size. A pty whose size nobody sets stays 80×24 and
    /// the TUI draws into a box that does not match the window.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        {
            let mut s = self.size.lock().unwrap();
            match resize_to(*s, cols, rows) {
                Some(next) => *s = next,
                None => return Ok(()),
            }
        }
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(std::io::Error::other)
    }

    /// Ask the TUI to exit, then make sure it did.
    ///
    /// SIGHUP first, because that is what a closing terminal window sends
    /// and every one of these CLIs already handles it — they save the
    /// conversation on the way out, which a SIGKILL would skip. The
    /// escalation exists for a child that ignores it.
    pub async fn close(&self) {
        if let Some(pgid) = self.pgid {
            let _ = eave_core::process::killpg(pgid, libc::SIGHUP);
            if !eave_core::process::await_group_empty(pgid, eave_core::thresholds::KILL_GRACE).await
            {
                let _ = eave_core::process::killpg(pgid, libc::SIGKILL);
                eave_core::process::await_group_empty(
                    pgid,
                    eave_core::thresholds::KILL_VERIFY_WINDOW,
                )
                .await;
            }
        }
        let _ = self.child.lock().unwrap().kill();
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// Every pty the engine is holding, keyed by eave session id.
#[derive(Default)]
pub struct TerminalHost {
    ptys: Mutex<HashMap<String, Arc<Pty>>>,
    /// Sessions whose handover is being set up right now.
    ///
    /// **A pty cannot exist for the whole of `term_open`.** The agent has
    /// to be settled first — one process per conversation — and a verified
    /// kill takes as long as it takes. In that window the session is
    /// handed over and has no pty, which is indistinguishable from a pty
    /// that died, so `reap_dead_terminals` would clear the claim it was
    /// just given. It DID, on the first live walk: the reaper ran, the
    /// rail said `killed`, and the terminal was on screen at the time.
    opening: Mutex<std::collections::HashSet<String>>,
}

/// What an attaching client needs to start painting.
pub struct Attachment {
    pub cols: u16,
    pub rows: u16,
    pub replay: Vec<u8>,
    pub output: broadcast::Receiver<Arc<Vec<u8>>>,
}

impl TerminalHost {
    pub fn get(&self, session_id: &str) -> Option<Arc<Pty>> {
        self.ptys.lock().unwrap().get(session_id).cloned()
    }

    /// Session ids the engine holds a live pty for **or is opening one
    /// for**. Both count as handed over; see [`TerminalHost::opening`].
    pub fn open_sessions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .ptys
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.is_alive())
            .map(|(id, _)| id.clone())
            .collect();
        ids.extend(self.opening.lock().unwrap().iter().cloned());
        ids
    }

    /// Claim a session as being handed over before the pty exists.
    pub fn begin_opening(&self, session_id: &str) {
        self.opening.lock().unwrap().insert(session_id.to_string());
    }

    /// Release the claim, whether the open succeeded or failed. A claim
    /// left behind is a session that reports `terminal` forever.
    pub fn end_opening(&self, session_id: &str) {
        self.opening.lock().unwrap().remove(session_id);
    }

    /// The replay and the live stream, taken together.
    ///
    /// Both come off one lock acquisition. Taken separately, whatever the
    /// pty wrote between them is either duplicated or lost, and a vt stream
    /// with a hole in it does not resynchronise — the screen stays wrong
    /// until the TUI happens to repaint.
    pub fn attach(&self, session_id: &str) -> Option<Attachment> {
        let ptys = self.ptys.lock().unwrap();
        let pty = ptys.get(session_id)?;
        let replay_guard = pty.replay.lock().unwrap();
        let output = pty.out.subscribe();
        let replay = replay_guard.clone();
        drop(replay_guard);
        let (cols, rows) = pty.size();
        Some(Attachment {
            cols,
            rows,
            replay,
            output,
        })
    }

    /// Start a TUI. `argv` is everything after the program, as built by
    /// the adapter's own `tui_args` — this function does not know which
    /// provider it is running and must not learn.
    pub fn open(
        &self,
        session_id: &str,
        program: &std::path::Path,
        argv: &[String],
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Pty>, String> {
        if let Some(existing) = self.get(session_id) {
            if existing.is_alive() {
                return Ok(existing);
            }
        }
        let cols = cols.max(20);
        let rows = rows.max(4);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("could not open a pty: {e}"))?;

        let mut cmd = CommandBuilder::new(program);
        for a in argv {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        // Spec 01 §Spawn: a child never inherits eave's own variables. The
        // agent spawn path sanitizes for the same reason — an `EAVE_*` left
        // in the environment is read by eave's own test doubles and by any
        // nested eave the operator starts from inside the TUI.
        for (k, _) in std::env::vars() {
            if k.starts_with("EAVE_") {
                cmd.env_remove(&k);
            }
        }
        // A TUI asks the terminal what it can do and draws to match. eave's
        // emulator is a vt/xterm one with 24-bit colour, so saying so is a
        // statement of fact; leaving TERM unset makes curses-based CLIs
        // refuse to start at all ("terminal not fully functional").
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("could not start the TUI: {e}"))?;
        // The slave fd must be dropped in the parent or the reader never
        // sees EOF when the child exits — the parent would be holding the
        // last write end open and the read blocks forever.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("could not read the pty: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("could not write the pty: {e}"))?;
        let pgid = pair.master.process_group_leader();

        let (out, _) = broadcast::channel(OUTPUT_LAG);
        let replay = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let pty = Arc::new(Pty {
            size: Mutex::new((cols, rows)),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            out: out.clone(),
            replay: Arc::clone(&replay),
            pgid,
            alive: Arc::clone(&alive),
            child: Mutex::new(child),
        });

        // A dedicated thread, not the blocking pool: this read is parked
        // for as long as the operator is thinking, which is exactly the
        // shape that starves a shared pool.
        std::thread::Builder::new()
            .name(format!("eave-pty-{session_id}"))
            .spawn(move || pump(reader, out, replay, alive))
            .map_err(|e| format!("could not start the pty reader: {e}"))?;

        self.ptys
            .lock()
            .unwrap()
            .insert(session_id.to_string(), Arc::clone(&pty));
        Ok(pty)
    }

    /// Close one session's TUI and forget it.
    pub async fn close(&self, session_id: &str) -> bool {
        let pty = self.ptys.lock().unwrap().remove(session_id);
        match pty {
            Some(p) => {
                p.close().await;
                true
            }
            None => false,
        }
    }

    /// Close every TUI. Called on engine shutdown — a pty the engine
    /// forgets is a process nothing will ever reap.
    pub async fn close_all(&self) {
        let all: Vec<Arc<Pty>> = self.ptys.lock().unwrap().drain().map(|(_, p)| p).collect();
        for p in all {
            p.close().await;
        }
    }
}

/// Whether a resize request is worth forwarding to the kernel, and to
/// what.
///
/// Two requests are dropped, for different reasons. **A zero dimension is
/// not a small window, it is a window that has not been laid out yet** —
/// gpui reports one on the first frame and while a pane is collapsed — and
/// telling a TUI its screen has no room makes it clear the screen. **An
/// unchanged size** is dropped because forwarding it still raises SIGWINCH,
/// and a TUI repaints on every one; a paint loop that ends in a resize
/// would then never settle.
fn resize_to(current: (u16, u16), cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 || current == (cols, rows) {
        return None;
    }
    Some((cols, rows))
}

/// Read the pty until it ends, fanning bytes out and keeping the replay.
fn pump(
    mut reader: Box<dyn Read + Send>,
    out: broadcast::Sender<Arc<Vec<u8>>>,
    replay: Arc<Mutex<Vec<u8>>>,
    alive: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            // EOF: the child closed its end, which is how a TUI exit
            // arrives here.
            Ok(0) => break,
            Ok(n) => {
                let chunk = Arc::new(buf[..n].to_vec());
                {
                    let mut log = replay.lock().unwrap();
                    log.extend_from_slice(&chunk);
                    trim_replay(&mut log);
                }
                // Err means nobody is attached. That is the normal state of
                // a handed-over session whose window is closed, and it must
                // not stop the pump — the replay log is still growing and
                // the TUI is still running.
                let _ = out.send(chunk);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    alive.store(false, Ordering::SeqCst);
}

/// Hold the replay to [`REPLAY_CAP`], cutting only where an emulator can
/// pick the stream back up.
///
/// A vt stream cut at an arbitrary byte can land in the middle of an escape
/// sequence, and the emulator then reads the tail of that sequence as text
/// — the screen opens with garbage that never clears. Cutting AT an `ESC`
/// cannot do that: the sequence that follows is whole, and at worst the
/// replay begins mid-screen. Losing a little more than asked is the cheap
/// error here; the expensive one is starting inside a sequence.
fn trim_replay(log: &mut Vec<u8>) {
    if log.len() <= REPLAY_CAP {
        return;
    }
    let cut = log.len() - REPLAY_CAP;
    let at = log[cut..]
        .iter()
        .position(|b| *b == 0x1b)
        .map(|p| cut + p)
        // No escape in the whole tail means it is plain text, which is
        // safe to cut anywhere.
        .unwrap_or(cut);
    log.drain(..at);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_replay_is_left_alone() {
        let mut log = b"hello".to_vec();
        trim_replay(&mut log);
        assert_eq!(log, b"hello");
    }

    /// The property the comment on `trim_replay` claims: a trimmed replay
    /// never begins in the middle of an escape sequence.
    #[test]
    fn a_trimmed_replay_begins_at_an_escape_and_not_inside_one() {
        let mut log = vec![b'x'; REPLAY_CAP];
        log.extend_from_slice(b"\x1b[31mred\x1b[0m");
        trim_replay(&mut log);
        assert!(log.len() <= REPLAY_CAP);
        assert_eq!(
            log[0], 0x1b,
            "a replay that starts mid-sequence paints the rest of that \
             sequence as text and never recovers"
        );
    }

    /// Plain text has no boundary to find, and refusing to trim would let
    /// the log grow without limit for a CLI that emits no escapes.
    #[test]
    fn a_replay_with_no_escapes_is_still_bounded() {
        let mut log = vec![b'x'; REPLAY_CAP * 2];
        trim_replay(&mut log);
        assert_eq!(log.len(), REPLAY_CAP);
    }

    /// A window that has not been laid out yet reports a zero, and
    /// forwarding one tells the TUI its screen has no room — which it
    /// answers by clearing it.
    #[test]
    fn a_zero_dimension_is_never_forwarded() {
        assert_eq!(resize_to((80, 24), 0, 24), None);
        assert_eq!(resize_to((80, 24), 100, 0), None);
        assert_eq!(resize_to((80, 24), 0, 0), None);
    }

    /// Every forwarded resize raises SIGWINCH and every TUI repaints on
    /// one. A paint that ends in a resize would then never settle, so a
    /// resize to the size the pty already is must not reach the kernel.
    #[test]
    fn a_resize_to_the_same_size_is_not_a_resize() {
        assert_eq!(resize_to((80, 24), 80, 24), None);
        assert_eq!(resize_to((80, 24), 80, 25), Some((80, 25)));
        assert_eq!(resize_to((80, 24), 81, 24), Some((81, 24)));
    }
}
