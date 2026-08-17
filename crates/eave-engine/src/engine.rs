//! The engine role: owns sessions, serves the wire protocol over a UNIX
//! socket in the data dir (spec 01 §Topology, spec 05-wire.md).
//!
//! One engine instance per data dir, enforced by the flock'd `engine.lock`
//! (spec 02 §Contended start): `serve` fails fast with the holder's
//! identity and the caller must exit inert. The engine never watches a
//! parent pid and does not die with the UI — that is the entire point of
//! the child-process topology (research: waku.md §1, rejected pattern).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedWriteHalf, UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};

use eave_core::attention::{probe_failure, AttentionSnapshot, Classifier, ProbeFacts};
use eave_core::claude::ClaudeCodeSession;
use eave_core::journal::{self, now_ms, Header, JournalEvent, JournalWriter, Record, FORMAT_VERSION};
use eave_core::proc::{self, PidState, ProcIdentity};
use eave_core::process;
use eave_core::recorder::{record_session, send_and_journal};
use eave_core::session::{AgentSession, SessionWatch, SpawnConfig};
use eave_core::store::{EngineLock, LockError, Meta, Store, META_FORMAT_VERSION};
use eave_core::thresholds::AttentionThresholds;
use eave_wire::{
    socket_path, ChangedFile, Cursor, DiffOk, FileStatus, FilesSnapshot, HelloOk, KillOk, ListOk,
    OpenOk, Request, RequestEnvelope, ResponseBody, ResponseEnvelope, SendOk, SessionInfo,
    SubscriptionItem, PROTO_VERSION,
};

const BROADCAST_CAP: usize = 4096;
/// Watch pokes carry no payload (snapshots are taken at emission), so the
/// channel can be tiny; lag just coalesces into the next snapshot.
const WATCH_CAP: usize = 64;
/// Files pokes are payload-free for the same reason (spec 05 §Files).
const FILES_CAP: usize = 16;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    /// Agent CLI binary; `claude` unless overridden (tests: fake-agent).
    pub agent_program: PathBuf,
    /// Extra env for spawned agents (tests: `FAKE_MODE`).
    pub agent_env: Vec<(String, String)>,
    pub provider: String,
    /// cwd recorded for sessions opened without one.
    pub default_cwd: PathBuf,
}

impl EngineConfig {
    /// Configuration from the environment: `EAVE_AGENT` overrides the agent
    /// binary, `EAVE_FAKE_MODE` becomes the fake agent's `FAKE_MODE`. The
    /// engine reads `EAVE_*` itself; spawned agents never see them — the
    /// adapter strips the namespace from child env (spec 01 §Spawn).
    pub fn from_env(data_dir: PathBuf) -> Self {
        let agent = std::env::var("EAVE_AGENT").unwrap_or_else(|_| "claude".into());
        let provider = if agent == "claude" {
            "claude-code".to_string()
        } else {
            PathBuf::from(&agent)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into())
        };
        let mut agent_env = Vec::new();
        if let Ok(mode) = std::env::var("EAVE_FAKE_MODE") {
            agent_env.push(("FAKE_MODE".to_string(), mode));
        }
        let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self {
            data_dir,
            agent_program: agent.into(),
            agent_env,
            provider,
            default_cwd,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("engine io error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Session handle

struct Shared {
    header: Header,
    /// One contiguous immutable log view per (engine run × session load) —
    /// the cursor contract's epoch (spec 05-wire.md).
    epoch: u64,
    records: Vec<Record>,
    state: String,
    live: bool,
    /// The live attention overlay (spec 01), maintained by the monitor
    /// task; `None` when no process is attached. Transient — never
    /// journaled.
    attention: Option<AttentionSnapshot>,
}

/// What the per-session broadcast fans out: seq'd journal records, plus
/// transient attention transitions that ride the same channel so a
/// subscriber sees them in one ordered stream.
#[derive(Debug, Clone)]
enum Fanout {
    Record(Record),
    Attention(AttentionSnapshot),
}

struct Live {
    session: ClaudeCodeSession,
    extra_tx: mpsc::Sender<JournalEvent>,
}

pub struct SessionHandle {
    session_id: String,
    dir: PathBuf,
    shared: StdMutex<Shared>,
    bcast: broadcast::Sender<Fanout>,
    /// Serializes spawn/send/kill — sends stay sequential, which the
    /// order-paired delivery state depends on (spec 02 §Delivery state).
    live: TokioMutex<Option<Live>>,
    /// Pokes the engine-wide watch channel (spec 05 §Watch) on state /
    /// attention changes. Deliberately NEVER poked per record — record
    /// flow must be invisible on watch (isolation invariant).
    watch_tx: broadcast::Sender<()>,
    /// Changed-file overview (spec 05 §Files): latest git poll of the
    /// session cwd. Ephemeral engine state, never journaled — git is the
    /// store of record and survives restarts for free. `None` until the
    /// first scan (the first `files` subscription triggers one).
    files: StdMutex<Option<FilesSnapshot>>,
    /// Pokes this session's `files` subscribers; payload-free like watch
    /// (every emission is a fresh read of `files`).
    files_tx: broadcast::Sender<()>,
    /// Serializes git scans so overlapping triggers can't interleave.
    files_gate: TokioMutex<()>,
    /// FS-event invalidation (B2): subscriber refcount plus the stop
    /// handle of the watcher task serving them. The watcher runs only
    /// while a `files` subscription is live; `active` gates the
    /// turn-boundary fallback poll — polling resumes the moment watching
    /// is unavailable (disabled, failed to start, or broken mid-flight).
    fs_watch: StdMutex<FsWatchCtl>,
}

#[derive(Default)]
struct FsWatchCtl {
    subscribers: usize,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    active: Arc<std::sync::atomic::AtomicBool>,
}

fn new_epoch() -> u64 {
    (now_ms() << 16) | u64::from(std::process::id() as u16)
}

impl SessionHandle {
    fn info(&self) -> SessionInfo {
        let s = self.shared.lock().unwrap();
        // While live, the attention overlay is the reported state
        // (spec 01 §States: quiet/unresponsive/failed are what the badge
        // shows); terminal names still come from the journal's truth.
        let state = match (&s.attention, s.live) {
            (Some(a), true) if !matches!(a, AttentionSnapshot::Running) => {
                a.state_name().to_string()
            }
            _ => s.state.clone(),
        };
        SessionInfo {
            session_id: self.session_id.clone(),
            provider: s.header.provider.clone(),
            cwd: s.header.cwd.clone(),
            created_ts_ms: s.header.created_ts_ms,
            state,
            live: s.live,
            epoch: s.epoch,
            last_seq: s.records.last().map(|r| r.seq).unwrap_or(0),
        }
    }

    /// Snapshot for a new subscription: what to replay, plus the live
    /// receiver registered *under the same lock* so nothing can fall
    /// between replay and follow (no gap, no dup — the receiver may still
    /// see records already in the snapshot; the caller filters by seq).
    /// The current attention overlay rides along so a fresh subscriber
    /// renders an in-progress notice without waiting for a transition.
    fn snapshot(
        &self,
        cursor: Option<Cursor>,
    ) -> (broadcast::Receiver<Fanout>, Vec<SubscriptionItem>, u64) {
        let s = self.shared.lock().unwrap();
        let rx = self.bcast.subscribe();
        let last_seq = s.records.last().map(|r| r.seq).unwrap_or(0);
        // Always authoritative: `Running` when no overlay stands, so a
        // resubscribing client sheds any notice it held from before.
        let attn_item = Some(SubscriptionItem::Attention {
            session_id: self.session_id.clone(),
            attention: s.attention.clone().unwrap_or(AttentionSnapshot::Running),
        });
        let (mut items, floor) = match cursor {
            Some(c) if c.epoch == s.epoch && c.seq <= last_seq => {
                let tail: Vec<Record> =
                    s.records.iter().filter(|r| r.seq > c.seq).cloned().collect();
                let mut items = Vec::new();
                if !tail.is_empty() {
                    items.push(SubscriptionItem::Events {
                        session_id: self.session_id.clone(),
                        epoch: s.epoch,
                        records: tail,
                    });
                }
                (items, last_seq.max(c.seq))
            }
            _ => {
                let mut items = vec![SubscriptionItem::Reset {
                    session_id: self.session_id.clone(),
                    epoch: s.epoch,
                    header: s.header.clone(),
                }];
                if !s.records.is_empty() {
                    items.push(SubscriptionItem::Events {
                        session_id: self.session_id.clone(),
                        epoch: s.epoch,
                        records: s.records.clone(),
                    });
                }
                (items, last_seq)
            }
        };
        items.extend(attn_item);
        (rx, items, floor)
    }

    fn epoch(&self) -> u64 {
        self.shared.lock().unwrap().epoch
    }

    /// Publish an attention transition: overlay for late subscribers +
    /// broadcast for live ones. Attention is part of the reported state
    /// name, so watchers are poked too.
    fn set_attention(&self, snap: AttentionSnapshot) {
        self.shared.lock().unwrap().attention = Some(snap.clone());
        let _ = self.bcast.send(Fanout::Attention(snap));
        self.poke_watch();
    }

    /// Something a watcher can see changed (session appeared, process
    /// attached/detached, state or attention moved). Payload-free: every
    /// watch emission is a fresh consistent snapshot, so there is nothing
    /// to get stale or out of order.
    fn poke_watch(&self) {
        let _ = self.watch_tx.send(());
    }

    /// An FS watcher currently covers this session's cwd — the
    /// turn-boundary poll stands down while this is true (B2: events
    /// replace polling; polling is the fallback, not a duplicate).
    fn watcher_active(&self) -> bool {
        self.fs_watch
            .lock()
            .unwrap()
            .active
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// `EAVE_NO_FSWATCH=1` forces the B1 polling fallback (dev/test only —
/// it is also what the engine degrades to when the watch backend fails).
fn fs_watch_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("EAVE_NO_FSWATCH").map(|v| v != "1").unwrap_or(true))
}

/// RAII refcount of one `files` subscription: the first subscriber
/// starts the session's FS watcher, the last one's drop stops it (the
/// subscription tasks are aborted on disconnect — drop still runs).
struct FilesSubGuard {
    handle: Arc<SessionHandle>,
}

fn files_subscriber(handle: &Arc<SessionHandle>) -> FilesSubGuard {
    let mut ctl = handle.fs_watch.lock().unwrap();
    ctl.subscribers += 1;
    if ctl.subscribers == 1 && fs_watch_enabled() {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ctl.stop = Some(stop_tx);
        ctl.active = Arc::clone(&active);
        tokio::spawn(watch_session_files(Arc::clone(handle), active, stop_rx));
    }
    FilesSubGuard {
        handle: Arc::clone(handle),
    }
}

impl Drop for FilesSubGuard {
    fn drop(&mut self) {
        let mut ctl = self.handle.fs_watch.lock().unwrap();
        ctl.subscribers = ctl.subscribers.saturating_sub(1);
        if ctl.subscribers == 0 {
            if let Some(stop) = ctl.stop.take() {
                let _ = stop.send(());
            }
            ctl.active
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Engine

pub struct Engine {
    cfg: EngineConfig,
    store: Store,
    identity: ProcIdentity,
    sessions: StdMutex<HashMap<String, Arc<SessionHandle>>>,
    /// Engine-wide watch poke channel (spec 05 §Watch). Held here so it
    /// outlives every session; handles carry clones.
    watch_tx: broadcast::Sender<()>,
}

impl Engine {
    fn open_or_load(
        self: &Arc<Self>,
        session_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<Arc<SessionHandle>, String> {
        let id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Hold the registry lock across the whole open: two concurrent
        // opens of one id must never build two handles over one journal.
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(h) = sessions.get(&id) {
            return Ok(Arc::clone(h));
        }
        let dir = self.store.session_dir(&id);
        let journal_path = Store::journal_path(&dir);
        let handle = if journal_path.exists() {
            self.load_from_disk(&id)?
        } else {
            // The real claude CLI rejects `--session-id` values that are
            // not UUIDs, so a named id against it dies at first spawn —
            // refuse at open with the reason instead of creating a
            // session that can only ever fail. Overridden agents
            // (fake-agent, other CLIs) accept any id; generated ids are
            // always UUIDs and pass. Existing journals are exempt: their
            // history is real and loadable regardless.
            if self.cfg.agent_program == Path::new("claude")
                && uuid::Uuid::parse_str(&id).is_err()
            {
                return Err(format!(
                    "session id {id:?} is not a UUID — the claude CLI rejects non-UUID session ids"
                ));
            }
            // Fresh session: journal header written now, durable before
            // anything else happens; nothing spawns until the first send.
            let dir = self
                .store
                .create_session_dir(&id)
                .map_err(|e| format!("create session dir: {e}"))?;
            let header = Header {
                format_version: FORMAT_VERSION,
                session_id: id.clone(),
                provider: self.cfg.provider.clone(),
                cwd: cwd.unwrap_or_else(|| self.cfg.default_cwd.display().to_string()),
                created_ts_ms: now_ms(),
            };
            JournalWriter::create(&journal_path, &header)
                .map_err(|e| format!("create journal: {e}"))?;
            let meta = Meta {
                format_version: META_FORMAT_VERSION,
                provider: header.provider.clone(),
                cwd: header.cwd.clone(),
                session_id: id.clone(),
                state: "new".into(),
                child: None,
            };
            meta.save(&dir).map_err(|e| format!("save meta: {e}"))?;
            let (bcast, _) = broadcast::channel(BROADCAST_CAP);
            let (files_tx, _) = broadcast::channel(FILES_CAP);
            Arc::new(SessionHandle {
                session_id: id.clone(),
                dir,
                shared: StdMutex::new(Shared {
                    header,
                    epoch: new_epoch(),
                    records: Vec::new(),
                    state: "new".into(),
                    live: false,
                    attention: None,
                }),
                bcast,
                live: TokioMutex::new(None),
                watch_tx: self.watch_tx.clone(),
                files: StdMutex::new(None),
                files_tx,
                files_gate: TokioMutex::new(()),
                fs_watch: StdMutex::new(FsWatchCtl::default()),
            })
        };
        sessions.insert(id, Arc::clone(&handle));
        drop(sessions);
        // The session set changed — watchers re-snapshot.
        handle.poke_watch();
        // Session open is a files poll boundary (spec 05 §Files).
        tokio::spawn(refresh_files(Arc::clone(&handle)));
        Ok(handle)
    }

    /// The single load path in action: `journal::load` parses, repairs the
    /// torn tail, and settles a non-terminal end as interrupted before we
    /// ever see the records (spec 02 §Single load path). The epoch is fresh
    /// — this on-disk history may differ from anything a client saw before.
    fn load_from_disk(&self, id: &str) -> Result<Arc<SessionHandle>, String> {
        let dir = self.store.session_dir(id);
        let loaded =
            journal::load(&Store::journal_path(&dir)).map_err(|e| format!("load journal: {e}"))?;
        let state = loaded
            .records
            .iter()
            .rev()
            .find_map(|r| match &r.event {
                JournalEvent::Closed { outcome } => Some(outcome.state_name().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "new".into());
        // Keep meta in step with what repair concluded.
        if let Ok(mut meta) = Meta::load(&dir) {
            if meta.state != state || meta.child.is_some() {
                meta.state = state.clone();
                meta.child = None;
                let _ = meta.save(&dir);
            }
        }
        let (bcast, _) = broadcast::channel(BROADCAST_CAP);
        let (files_tx, _) = broadcast::channel(FILES_CAP);
        Ok(Arc::new(SessionHandle {
            session_id: id.to_string(),
            dir,
            shared: StdMutex::new(Shared {
                header: loaded.header.clone(),
                epoch: new_epoch(),
                records: loaded.records,
                state,
                live: false,
                attention: None,
            }),
            bcast,
            live: TokioMutex::new(None),
            watch_tx: self.watch_tx.clone(),
            // No eager scan here: this path also runs for every on-disk
            // session at engine startup, and file state rebuilds lazily
            // from git on the first `files` subscription instead.
            files: StdMutex::new(None),
            files_tx,
            files_gate: TokioMutex::new(()),
            fs_watch: StdMutex::new(FsWatchCtl::default()),
        }))
    }

    fn session(&self, id: &str) -> Result<Arc<SessionHandle>, String> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("unknown session {id}"))
    }

    /// Send one user message, spawning the agent first if the session is
    /// not live. Returns delivery per spec 02 §Delivery state.
    async fn send(self: &Arc<Self>, id: &str, text: &str) -> Result<bool, String> {
        let handle = self.session(id)?;
        let mut live = handle.live.lock().await;
        // A returned recorder drops its `extra` receiver, so a closed
        // channel is proof the attached process segment is over even if the
        // completion task hasn't cleared the slot yet — respawn (with
        // `--resume`) rather than journal into a dead segment.
        let dead = live
            .as_ref()
            .map(|l| l.extra_tx.is_closed())
            .unwrap_or(true);
        if dead {
            *live = Some(self.spawn_live(&handle).await?);
        }
        let l = live.as_mut().unwrap();
        send_and_journal(&mut l.session, &l.extra_tx, text)
            .await
            .map_err(|e| format!("recorder gone: {e}"))
    }

    async fn spawn_live(self: &Arc<Self>, handle: &Arc<SessionHandle>) -> Result<Live, String> {
        let (header, next_seq, resume) = {
            let s = handle.shared.lock().unwrap();
            let resume = s.records.iter().any(|r| {
                matches!(
                    &r.event,
                    JournalEvent::Provider {
                        event: eave_core::events::SessionEvent::Init { .. }
                    }
                )
            });
            (
                s.header.clone(),
                s.records.last().map(|r| r.seq + 1).unwrap_or(1),
                resume,
            )
        };
        let journal_path = Store::journal_path(&handle.dir);
        let writer = JournalWriter::append_existing(&journal_path, next_seq)
            .map_err(|e| format!("open journal for append: {e}"))?;

        // A provider ran before ⇒ continue it (`--resume`); otherwise name
        // the fresh id (`--session-id`) — spec 02 §Resume contract.
        let mut cfg = SpawnConfig::new(
            &self.cfg.agent_program,
            PathBuf::from(&header.cwd),
            handle.session_id.clone(),
        );
        cfg.resume = resume;
        cfg.extra_env = self.cfg.agent_env.clone();
        let (session, events) =
            ClaudeCodeSession::spawn(&cfg).map_err(|e| format!("spawn agent: {e}"))?;

        let meta = Meta {
            format_version: META_FORMAT_VERSION,
            provider: header.provider.clone(),
            cwd: header.cwd.clone(),
            session_id: handle.session_id.clone(),
            state: "running".into(),
            child: Some(session.identity()),
        };
        meta.save(&handle.dir).map_err(|e| format!("save meta: {e}"))?;

        {
            let mut s = handle.shared.lock().unwrap();
            s.live = true;
            s.state = "running".into();
        }
        handle.poke_watch();

        let (extra_tx, extra_rx) = mpsc::channel(16);
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<Record>();

        // Tap → in-memory log + subscriber broadcast. Send errors just mean
        // zero subscribers.
        let tap_handle = Arc::clone(handle);
        tokio::spawn(async move {
            while let Some(rec) = tap_rx.recv().await {
                // Turn boundaries are the files poll FALLBACK (B2): the
                // FS watcher owns invalidation while it runs; the poll
                // stands in whenever watching is unavailable. The scan
                // runs as its own task, never in the record path.
                let turn_boundary = matches!(
                    &rec.event,
                    JournalEvent::Closed { .. }
                        | JournalEvent::Provider {
                            event: eave_core::events::SessionEvent::TurnCompleted { .. }
                        }
                );
                tap_handle.shared.lock().unwrap().records.push(rec.clone());
                let _ = tap_handle.bcast.send(Fanout::Record(rec));
                if turn_boundary && !tap_handle.watcher_active() {
                    tokio::spawn(refresh_files(Arc::clone(&tap_handle)));
                }
            }
        });

        // Attention monitor (spec 01 §Attention clocks): subscribe to the
        // fan-out BEFORE the recorder starts so no record is missed, and
        // publish a fresh Running so any stale notice from a prior segment
        // provably clears.
        handle.set_attention(AttentionSnapshot::Running);
        let monitor_rx = handle.bcast.subscribe();
        tokio::spawn(monitor_session(
            Arc::clone(handle),
            session.watch(),
            std::process::id() as i32,
            monitor_rx,
        ));

        let rec_task = tokio::spawn(record_session(
            events,
            extra_rx,
            writer,
            handle.dir.clone(),
            meta,
            Some(tap_tx),
        ));

        // Completion: settle handle state once the recorder is done.
        let done_handle = Arc::clone(handle);
        tokio::spawn(async move {
            let outcome = rec_task.await;
            let state = match outcome {
                Ok(Ok(outcome)) => outcome.state_name().to_string(),
                // Recorder failure (io error / stream ended bare): the next
                // load's crash-tail repair owns the truth; reflect it.
                _ => "interrupted".to_string(),
            };
            {
                let mut s = done_handle.shared.lock().unwrap();
                s.live = false;
                s.state = state;
                // The overlay is per-process; the journal's terminal truth
                // governs now. Broadcast the clear so a live notice drops.
                s.attention = None;
            }
            let _ = done_handle.bcast.send(Fanout::Attention(AttentionSnapshot::Running));
            done_handle.poke_watch();
            *done_handle.live.lock().await = None;
        });

        Ok(Live { session, extra_tx })
    }

    async fn kill(self: &Arc<Self>, id: &str) -> Result<KillOk, String> {
        let handle = self.session(id)?;
        let mut live = handle.live.lock().await;
        let l = live.as_mut().ok_or("session is not live")?;
        let report = l
            .session
            .kill()
            .await
            .map_err(|e| format!("kill failed: {e}"))?;
        Ok(KillOk {
            escalated: report.escalated,
            already_exited: report.already_exited,
        })
    }

    fn list(&self) -> ListOk {
        let sessions = self.sessions.lock().unwrap();
        let mut infos: Vec<SessionInfo> = sessions.values().map(|h| h.info()).collect();
        infos.sort_by(|a, b| a.created_ts_ms.cmp(&b.created_ts_ms));
        ListOk { sessions: infos }
    }
}

/// The engine role's whole process life: serve forever, or report a lost
/// lock contest per the contended-start contract — a `BusyReport` line on
/// stdout (the one channel a spawning client can read before any socket
/// exists) and exit `ENGINE_BUSY_EXIT`, touching nothing (spec 02
/// §Contended start). Shared by `eave --engine` and `eave-engined`.
pub fn run_blocking(data_dir: PathBuf) -> ! {
    let cfg = EngineConfig::from_env(data_dir);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(serve(cfg)) {
        Ok(()) => std::process::exit(0),
        Err(ServeError::Lock(LockError::Busy { holder })) => {
            let report = eave_wire::BusyReport {
                busy: eave_wire::BusyHolder { holder },
            };
            println!(
                "{}",
                serde_json::to_string(&report).expect("report serializes")
            );
            std::process::exit(eave_wire::ENGINE_BUSY_EXIT);
        }
        Err(e) => {
            eprintln!("eave engine: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Serving

/// Run the engine: acquire the instance lock (fail-fast — `Busy` names the
/// holder and the caller must exit inert, spec 02 §Contended start), repair
/// and adopt what's on disk, then serve the socket forever.
pub async fn serve(cfg: EngineConfig) -> Result<(), ServeError> {
    let store = Store::open(&cfg.data_dir)?;
    let lock = EngineLock::acquire(store.root())?;
    serve_locked(cfg, store, lock).await
}

async fn serve_locked(cfg: EngineConfig, store: Store, _lock: EngineLock) -> Result<(), ServeError> {
    let identity = proc::identity_of(std::process::id() as i32).unwrap_or(ProcIdentity {
        pid: std::process::id() as i32,
        start_tvsec: 0,
        start_tvusec: 0,
    });
    let (watch_tx, _) = broadcast::channel(WATCH_CAP);
    let engine = Arc::new(Engine {
        cfg,
        store,
        identity,
        sessions: StdMutex::new(HashMap::new()),
        watch_tx,
    });

    // Startup scan (spec 02 §Resume contract): reap orphans from the meta
    // breadcrumb, then load — which is also repair. A session that refuses
    // to load (corrupt) is skipped and logged; it must not take the engine
    // down with it.
    for id in engine.store.session_ids()? {
        let dir = engine.store.session_dir(&id);
        if let Ok(meta) = Meta::load(&dir) {
            if let Some(child) = meta.child {
                let outcome = process::reap_orphan(&child).await;
                eprintln!("engine: orphan check for session {id}: {outcome:?}");
            }
        }
        match engine.load_from_disk(&id) {
            Ok(handle) => {
                engine.sessions.lock().unwrap().insert(id, handle);
            }
            Err(e) => eprintln!("engine: skipping session {id}: {e}"),
        }
    }

    // The lock proves no other engine lives; any socket file is stale.
    let sock = socket_path(&engine.cfg.data_dir);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("engine: listening on {}", sock.display());

    loop {
        let (stream, _) = listener.accept().await?;
        let engine = Arc::clone(&engine);
        tokio::spawn(handle_conn(engine, stream));
    }
}

async fn write_frame(
    w: &Arc<TokioMutex<OwnedWriteHalf>>,
    env: &ResponseEnvelope,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(env).expect("response envelopes serialize");
    line.push(b'\n');
    let mut w = w.lock().await;
    w.write_all(&line).await?;
    w.flush().await
}

fn ok_value<T: serde::Serialize>(v: &T) -> ResponseBody {
    ResponseBody::Ok(serde_json::to_value(v).expect("payloads serialize"))
}

async fn handle_conn(engine: Arc<Engine>, stream: UnixStream) {
    let (r, w) = stream.into_split();
    let w = Arc::new(TokioMutex::new(w));
    let mut lines = BufReader::new(r).lines();
    let mut subs: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while let Ok(Some(line)) = lines.next_line().await {
        let env: RequestEnvelope = match serde_json::from_str(&line) {
            Ok(env) => env,
            Err(e) => {
                // Echo the envelope's id even when the body is
                // unparseable — a newer client calling a method this
                // engine predates must get an error IT can correlate,
                // not a reply with id 0 that matches nothing and leaves
                // its call pending forever (the 2026-08-17 stale-engine
                // hang; the fix progress.md designated).
                let id = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
                    .unwrap_or(0);
                let _ = write_frame(
                    &w,
                    &ResponseEnvelope {
                        id,
                        body: ResponseBody::Err {
                            message: format!("bad request: {e}"),
                        },
                    },
                )
                .await;
                continue;
            }
        };
        let id = env.id;
        let body = match env.request {
            Request::Hello { proto: _ } => ok_value(&HelloOk {
                proto: PROTO_VERSION,
                engine: engine.identity,
            }),
            Request::List => ok_value(&engine.list()),
            Request::Open { session_id, cwd } => match engine.open_or_load(session_id, cwd) {
                Ok(h) => ok_value(&OpenOk { info: h.info() }),
                Err(message) => ResponseBody::Err { message },
            },
            Request::Send { session_id, text } => match engine.send(&session_id, &text).await {
                Ok(delivered) => ok_value(&SendOk { delivered }),
                Err(message) => ResponseBody::Err { message },
            },
            Request::Kill { session_id } => match engine.kill(&session_id).await {
                Ok(ok) => ok_value(&ok),
                Err(message) => ResponseBody::Err { message },
            },
            Request::Subscribe { session_id, cursor } => match engine.session(&session_id) {
                Ok(handle) => {
                    let w = Arc::clone(&w);
                    subs.push(tokio::spawn(run_subscription(handle, cursor, w, id)));
                    continue; // items flow on this id; no unary reply
                }
                Err(message) => ResponseBody::Err { message },
            },
            Request::Watch => {
                let w = Arc::clone(&w);
                subs.push(tokio::spawn(run_watch(Arc::clone(&engine), w, id)));
                continue; // snapshots flow on this id; no unary reply
            }
            Request::Files { session_id } => match engine.session(&session_id) {
                Ok(handle) => {
                    let w = Arc::clone(&w);
                    subs.push(tokio::spawn(run_files(handle, w, id)));
                    continue; // snapshots flow on this id; no unary reply
                }
                Err(message) => ResponseBody::Err { message },
            },
            Request::Diff { session_id, path } => match engine.session(&session_id) {
                Ok(handle) => {
                    // Unary, but computed off the connection loop: a slow
                    // repo delays only its own reply — sends and kills on
                    // this connection keep flowing while git works.
                    let w = Arc::clone(&w);
                    subs.push(tokio::spawn(run_diff(handle, w, id, path)));
                    continue;
                }
                Err(message) => ResponseBody::Err { message },
            },
        };
        if write_frame(&w, &ResponseEnvelope { id, body }).await.is_err() {
            break;
        }
    }

    // Client gone (clean close or SIGKILL — indistinguishable and treated
    // identically): stop its subscriptions. Sessions are untouched; the
    // engine owns them (spec 01 §Crash & engine shutdown: UI quit is a
    // no-op for sessions).
    for s in subs {
        s.abort();
    }
}

// ---------------------------------------------------------------------------
// Attention monitor (spec 01 §Attention clocks, §States)

/// One task per live process segment: folds the record fan-out into the
/// pure [`Classifier`], probes OS facts for provable death, samples CPU as
/// the `unresponsive` tiebreaker, and publishes every transition. Exits
/// when the segment closes. Never touches the journal and never kills —
/// classification only.
async fn monitor_session(
    handle: Arc<SessionHandle>,
    watch: SessionWatch,
    engine_pid: i32,
    mut rx: broadcast::Receiver<Fanout>,
) {
    let th = AttentionThresholds::from_env();
    let mut cls = Classifier::new(th, now_ms());
    let mut tick = tokio::time::interval(Duration::from_millis(th.tick_ms()));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cpu_tick =
        tokio::time::interval(Duration::from_millis(th.cpu_sample_interval_ms.max(1)));
    cpu_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_cpu: Option<u64> = None;
    // A normal exit races the OS facts (stdout EOF and pid teardown land
    // moments before the exit waiter fires), so `failed` requires the same
    // verdict on two consecutive ticks with the exit flag still unset.
    let mut pending_cause: Option<String> = None;

    loop {
        tokio::select! {
            f = rx.recv() => match f {
                Ok(Fanout::Record(rec)) => {
                    let closed = matches!(rec.event, JournalEvent::Closed { .. });
                    if let Some(snap) = cls.on_record(&rec, now_ms()) {
                        handle.set_attention(snap);
                    }
                    if closed {
                        return; // completion task owns the clear
                    }
                }
                Ok(Fanout::Attention(_)) => {} // our own echo
                // Lagged is harmless here: classification is heuristics
                // over a live stream, not a projection — keep going.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                // Provable death first — immediate, no timer (spec 01).
                let facts = probe_facts(&watch, engine_pid, cls.in_turn());
                match probe_failure(&facts) {
                    Some(cause) if pending_cause.as_deref() == Some(cause.as_str()) => {
                        if let Some(snap) = cls.fail(cause, now_ms()) {
                            handle.set_attention(snap);
                        }
                    }
                    other => pending_cause = other,
                }
                if let Some(snap) = cls.on_tick(now_ms()) {
                    handle.set_attention(snap);
                }
            }
            _ = cpu_tick.tick() => {
                if cls.wants_cpu_sampling() {
                    match proc::cpu_time(watch.identity.pid) {
                        Some(cpu) => {
                            if let Some(prev) = last_cpu.replace(cpu) {
                                if let Some(snap) = cls.on_cpu_sample(cpu == prev, now_ms()) {
                                    handle.set_attention(snap);
                                }
                            }
                            // First reading is the baseline, not a sample.
                        }
                        None => last_cpu = None, // pid gone: the probe owns it
                    }
                } else {
                    last_cpu = None;
                }
            }
        }
    }
}

/// Assemble one tick's OS facts (spec 01 `failed` inputs) from the pid
/// probe + the adapter's flags. Once the exit waiter has facts, the normal
/// `Exited → Closed` path owns everything — report nothing.
fn probe_facts(watch: &SessionWatch, engine_pid: i32, in_turn: bool) -> ProbeFacts {
    let exited = watch.exited();
    let (vanished, zombie, parent_ok) = if exited {
        (false, false, true)
    } else {
        match proc::pid_state(watch.identity.pid) {
            PidState::Gone => (true, false, true),
            PidState::Zombie => (false, true, true),
            PidState::Uninspectable => (false, false, true),
            PidState::Live(info) => {
                let identity_ok = info.start_tvsec == watch.identity.start_tvsec
                    && info.start_tvusec == watch.identity.start_tvusec;
                (!identity_ok, info.is_zombie, info.ppid == engine_pid)
            }
        }
    };
    ProbeFacts {
        exited,
        vanished,
        zombie,
        parent_ok,
        stdout_eof: watch.stdout_eof(),
        in_turn,
    }
}

async fn run_subscription(
    handle: Arc<SessionHandle>,
    cursor: Option<Cursor>,
    w: Arc<TokioMutex<OwnedWriteHalf>>,
    id: u64,
) {
    let (mut rx, items, mut last_sent) = handle.snapshot(cursor);
    for item in items {
        if send_item(&w, id, item).await.is_err() {
            return;
        }
    }
    // Strict delta-folding at the 120 ms coalesce (spec 03 §Streaming
    // commit): text deltas are held and folded into one Events frame per
    // window — at most one delta frame per 120 ms sustained. The leading
    // edge of a burst after quiet flashes through immediately (the window
    // is measured from the last emit, not the first pending record), and
    // any non-delta record (tool events, snapshots, turn ends, Closed)
    // flushes the batch at once — only prose is latency-tolerant.
    const COALESCE: Duration = Duration::from_millis(120);
    let mut last_emit = Instant::now() - COALESCE;
    let mut lagged = false;
    let mut closed = false;
    loop {
        if lagged {
            // Fell behind the broadcast ring: heal with a fresh full
            // replay (spec 03 §Streaming commit — a fresh Reset heals).
            lagged = false;
            let (nrx, items, nlast) = handle.snapshot(None);
            rx = nrx;
            last_sent = nlast;
            for item in items {
                if send_item(&w, id, item).await.is_err() {
                    return;
                }
            }
            continue;
        }
        if closed {
            return;
        }
        let rec = match rx.recv().await {
            Ok(Fanout::Record(rec)) => rec,
            // Attention frames are transient state, not sequenced records:
            // pass through immediately, outside the fold (spec 05).
            Ok(Fanout::Attention(attention)) => {
                let item = SubscriptionItem::Attention {
                    session_id: handle.session_id.clone(),
                    attention,
                };
                if send_item(&w, id, item).await.is_err() {
                    return;
                }
                continue;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                lagged = true;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if rec.seq <= last_sent {
            continue; // already covered by the replay snapshot
        }
        let mut flush = !is_delta(&rec);
        let mut records = vec![rec];
        let mut attn_after: Vec<AttentionSnapshot> = Vec::new();
        while !flush {
            let since = last_emit.elapsed();
            if since >= COALESCE {
                break;
            }
            match tokio::time::timeout(COALESCE - since, rx.recv()).await {
                Err(_window_elapsed) => break,
                Ok(Ok(Fanout::Record(more))) => {
                    if more.seq > records.last().unwrap().seq {
                        flush = !is_delta(&more);
                        records.push(more);
                    }
                }
                // An attention transition mid-window flushes: the notice
                // must not sit behind buffered prose.
                Ok(Ok(Fanout::Attention(attention))) => {
                    attn_after.push(attention);
                    flush = true;
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    lagged = true;
                    break;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    closed = true;
                    break;
                }
            }
        }
        last_sent = records.last().unwrap().seq;
        last_emit = Instant::now();
        let item = SubscriptionItem::Events {
            session_id: handle.session_id.clone(),
            epoch: handle.epoch(),
            records,
        };
        if send_item(&w, id, item).await.is_err() {
            return;
        }
        for attention in attn_after {
            let item = SubscriptionItem::Attention {
                session_id: handle.session_id.clone(),
                attention,
            };
            if send_item(&w, id, item).await.is_err() {
                return;
            }
        }
    }
}

/// The watch subscription (spec 05 §Watch): send one full session-set
/// snapshot now, then a fresh one after every poke. Every emission is a
/// consistent read taken at emission time, so replacement-folding on the
/// client can never go stale or reorder; a lagged poke channel just means
/// the next snapshot covers more changes at once. This task never touches
/// a session's record log — the only thing it can serialize is
/// `SessionInfo` (isolation invariant, enforced by test).
async fn run_watch(engine: Arc<Engine>, w: Arc<TokioMutex<OwnedWriteHalf>>, id: u64) {
    let mut rx = engine.watch_tx.subscribe();
    loop {
        let item = SubscriptionItem::Sessions {
            sessions: engine.list().sessions,
        };
        if send_item(&w, id, item).await.is_err() {
            return;
        }
        match rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                // Coalesce a burst of pokes into one snapshot.
                while rx.try_recv().is_ok() {}
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// One git poll of the session cwd (spec 05 §Files): serialized per
/// session by the gate, stored as the current snapshot, then this
/// session's files subscribers are poked. Always its own task — git runs
/// as an async child process with every call timeout-bounded (`git.rs`),
/// so a pathological repo slows its own snapshot but never the event
/// loop; sessions keep streaming while git runs (build-directive
/// constraint). Never journaled: git is the store of record.
async fn refresh_files(handle: Arc<SessionHandle>) {
    let _gate = handle.files_gate.lock().await;
    let cwd = PathBuf::from(handle.shared.lock().unwrap().header.cwd.clone());
    let snapshot = crate::git::scan(&cwd).await;
    *handle.files.lock().unwrap() = Some(snapshot);
    let _ = handle.files_tx.send(());
}

/// The files subscription (spec 05 §Files): send the current snapshot
/// now — triggering the first scan if none exists yet, which is also how
/// a restarted engine lazily rebuilds file state from git — then a fresh
/// read after every poke. Strictly per-session by construction: this
/// task holds one session's handle and can serialize nothing but that
/// session's [`FilesSnapshot`]; the raw-byte leak test in
/// `tests/files.rs` enforces it end-to-end.
async fn run_files(handle: Arc<SessionHandle>, w: Arc<TokioMutex<OwnedWriteHalf>>, id: u64) {
    // B2: the first live subscriber starts the session's FS watcher; the
    // last one's drop (including task abort on disconnect) stops it.
    let _guard = files_subscriber(&handle);
    let mut rx = handle.files_tx.subscribe();
    let current = handle.files.lock().unwrap().clone();
    match current {
        Some(mut snapshot) => {
            // The refresh mode is stamped per FRAME, not per scan: it
            // reports the mode at send time, so a watcher that starts or
            // breaks between scans re-states the truth on the very next
            // snapshot. (This first frame may still say `false` for the
            // moment between subscribing and the watcher's startup scan
            // — that scan's own frame corrects it.)
            snapshot.watching = handle.watcher_active();
            let item = SubscriptionItem::Files {
                session_id: handle.session_id.clone(),
                snapshot,
            };
            if send_item(&w, id, item).await.is_err() {
                return;
            }
        }
        None => {
            tokio::spawn(refresh_files(Arc::clone(&handle)));
        }
    }
    loop {
        match rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                // Coalesce a burst of pokes into one snapshot read.
                while rx.try_recv().is_ok() {}
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
        let Some(mut snapshot) = handle.files.lock().unwrap().clone() else {
            continue;
        };
        snapshot.watching = handle.watcher_active();
        let item = SubscriptionItem::Files {
            session_id: handle.session_id.clone(),
            snapshot,
        };
        if send_item(&w, id, item).await.is_err() {
            return;
        }
    }
}

/// One unary diff computation (B2): the snapshot's entry for the path
/// supplies the untracked/orig_path facts that pick the diff strategy; a
/// path the cached snapshot no longer lists gets a plain tracked diff
/// (racing a fresh scan is fine — a new snapshot on the files channel
/// tells the client to re-request anyway). Strictly per-session by
/// construction, like `run_files`: this task holds one session's handle
/// and answers on the requesting connection's own envelope id.
async fn run_diff(
    handle: Arc<SessionHandle>,
    w: Arc<TokioMutex<OwnedWriteHalf>>,
    id: u64,
    path: String,
) {
    let cwd = PathBuf::from(handle.shared.lock().unwrap().header.cwd.clone());
    let entry = handle
        .files
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|s| s.files.iter().find(|f| f.path == path).cloned())
        .unwrap_or(ChangedFile {
            path,
            status: FileStatus::Modified,
            adds: None,
            dels: None,
            orig_path: None,
            untracked: false,
        });
    let diff = crate::git::diff(&cwd, &entry).await;
    let _ = write_frame(
        &w,
        &ResponseEnvelope {
            id,
            body: ok_value(&DiffOk { diff }),
        },
    )
    .await;
}

/// Debounce for FS-event invalidation (B2): refresh after a quiet gap,
/// but never lag a busy burst longer than the max — an agent writing 200
/// files in a burst produces one refresh, and a continuous writer
/// produces about one per second, not one per write.
const FS_QUIET_MS: u64 = 200;
const FS_MAX_LAG_MS: u64 = 1000;

/// The session's FS watcher (B2): recursive watch of the session cwd,
/// alive only while `files` subscribers exist. Every failure degrades to
/// the B1 polling fallback (`active` stays/returns false, so turn
/// boundaries poll again) — a broken watch backend must never mean
/// silently-stale file state. The `.git` dir is deliberately included:
/// commits and index changes move `git status` output, and our own scans
/// are read-only (`GIT_OPTIONAL_LOCKS=0`), so there is no self-trigger
/// loop.
async fn watch_session_files(
    handle: Arc<SessionHandle>,
    active: Arc<std::sync::atomic::AtomicBool>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    use notify::Watcher;
    use std::sync::atomic::Ordering;
    let cwd = PathBuf::from(handle.shared.lock().unwrap().header.cwd.clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), ()>>();
    let mut watcher = match notify::recommended_watcher(
        move |res: Result<notify::Event, notify::Error>| {
            let _ = tx.send(res.map(|_| ()).map_err(|_| ()));
        },
    ) {
        Ok(w) => w,
        Err(_) => return, // polling fallback stands
    };
    if watcher
        .watch(&cwd, notify::RecursiveMode::Recursive)
        .is_err()
    {
        return; // non-existent / unwatchable cwd: polling fallback stands
    }
    active.store(true, Ordering::Relaxed);
    // A subscriber just arrived: heal any staleness from the unwatched
    // gap immediately.
    refresh_files(Arc::clone(&handle)).await;
    loop {
        tokio::select! {
            _ = &mut stop => break,
            ev = rx.recv() => match ev {
                None => break,
                Some(Err(())) => {
                    // Watch backend broke mid-flight: refresh once so
                    // nothing is lost, then drop to the polling fallback.
                    active.store(false, Ordering::Relaxed);
                    refresh_files(Arc::clone(&handle)).await;
                    return;
                }
                Some(Ok(())) => {
                    let burst_start = tokio::time::Instant::now();
                    loop {
                        tokio::select! {
                            _ = &mut stop => {
                                active.store(false, Ordering::Relaxed);
                                return;
                            }
                            _ = tokio::time::sleep(Duration::from_millis(FS_QUIET_MS)) => break,
                            more = rx.recv() => {
                                if more.is_none()
                                    || burst_start.elapsed()
                                        >= Duration::from_millis(FS_MAX_LAG_MS)
                                {
                                    break;
                                }
                            }
                        }
                    }
                    // Events landing while this scan runs queue behind it
                    // and trigger a trailing refresh — the final state
                    // always gets scanned.
                    refresh_files(Arc::clone(&handle)).await;
                }
            }
        }
    }
    active.store(false, Ordering::Relaxed);
}

/// Only text deltas are folded; everything else flushes the window.
fn is_delta(rec: &Record) -> bool {
    matches!(
        &rec.event,
        eave_core::journal::JournalEvent::Provider {
            event: eave_core::events::SessionEvent::TextDelta { .. }
        }
    )
}

async fn send_item(
    w: &Arc<TokioMutex<OwnedWriteHalf>>,
    id: u64,
    item: SubscriptionItem,
) -> io::Result<()> {
    write_frame(
        w,
        &ResponseEnvelope {
            id,
            body: ResponseBody::Item(item),
        },
    )
    .await
}
