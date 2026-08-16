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
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedWriteHalf, UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};

use eave_core::claude::ClaudeCodeSession;
use eave_core::journal::{self, now_ms, Header, JournalEvent, JournalWriter, Record, FORMAT_VERSION};
use eave_core::proc::{self, ProcIdentity};
use eave_core::process;
use eave_core::recorder::{record_session, send_and_journal};
use eave_core::session::{AgentSession, SpawnConfig};
use eave_core::store::{EngineLock, LockError, Meta, Store, META_FORMAT_VERSION};
use eave_wire::{
    socket_path, Cursor, HelloOk, KillOk, ListOk, OpenOk, Request, RequestEnvelope, ResponseBody,
    ResponseEnvelope, SendOk, SessionInfo, SubscriptionItem, PROTO_VERSION,
};

const BROADCAST_CAP: usize = 4096;

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
}

struct Live {
    session: ClaudeCodeSession,
    extra_tx: mpsc::Sender<JournalEvent>,
}

pub struct SessionHandle {
    session_id: String,
    dir: PathBuf,
    shared: StdMutex<Shared>,
    bcast: broadcast::Sender<Record>,
    /// Serializes spawn/send/kill — sends stay sequential, which the
    /// order-paired delivery state depends on (spec 02 §Delivery state).
    live: TokioMutex<Option<Live>>,
}

fn new_epoch() -> u64 {
    (now_ms() << 16) | u64::from(std::process::id() as u16)
}

impl SessionHandle {
    fn info(&self) -> SessionInfo {
        let s = self.shared.lock().unwrap();
        SessionInfo {
            session_id: self.session_id.clone(),
            provider: s.header.provider.clone(),
            cwd: s.header.cwd.clone(),
            created_ts_ms: s.header.created_ts_ms,
            state: s.state.clone(),
            live: s.live,
            epoch: s.epoch,
            last_seq: s.records.last().map(|r| r.seq).unwrap_or(0),
        }
    }

    /// Snapshot for a new subscription: what to replay, plus the live
    /// receiver registered *under the same lock* so nothing can fall
    /// between replay and follow (no gap, no dup — the receiver may still
    /// see records already in the snapshot; the caller filters by seq).
    fn snapshot(
        &self,
        cursor: Option<Cursor>,
    ) -> (broadcast::Receiver<Record>, Vec<SubscriptionItem>, u64) {
        let s = self.shared.lock().unwrap();
        let rx = self.bcast.subscribe();
        let last_seq = s.records.last().map(|r| r.seq).unwrap_or(0);
        match cursor {
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
                (rx, items, last_seq.max(c.seq))
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
                (rx, items, last_seq)
            }
        }
    }

    fn epoch(&self) -> u64 {
        self.shared.lock().unwrap().epoch
    }
}

// ---------------------------------------------------------------------------
// Engine

pub struct Engine {
    cfg: EngineConfig,
    store: Store,
    identity: ProcIdentity,
    sessions: StdMutex<HashMap<String, Arc<SessionHandle>>>,
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
            Arc::new(SessionHandle {
                session_id: id.clone(),
                dir,
                shared: StdMutex::new(Shared {
                    header,
                    epoch: new_epoch(),
                    records: Vec::new(),
                    state: "new".into(),
                    live: false,
                }),
                bcast,
                live: TokioMutex::new(None),
            })
        };
        sessions.insert(id, Arc::clone(&handle));
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
        Ok(Arc::new(SessionHandle {
            session_id: id.to_string(),
            dir,
            shared: StdMutex::new(Shared {
                header: loaded.header.clone(),
                epoch: new_epoch(),
                records: loaded.records,
                state,
                live: false,
            }),
            bcast,
            live: TokioMutex::new(None),
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

        let (extra_tx, extra_rx) = mpsc::channel(16);
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<Record>();

        // Tap → in-memory log + subscriber broadcast. Send errors just mean
        // zero subscribers.
        let tap_handle = Arc::clone(handle);
        tokio::spawn(async move {
            while let Some(rec) = tap_rx.recv().await {
                tap_handle.shared.lock().unwrap().records.push(rec.clone());
                let _ = tap_handle.bcast.send(rec);
            }
        });

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
            }
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
    let engine = Arc::new(Engine {
        cfg,
        store,
        identity,
        sessions: StdMutex::new(HashMap::new()),
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
                // Unparseable request: best-effort error with id 0.
                let _ = write_frame(
                    &w,
                    &ResponseEnvelope {
                        id: 0,
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
    loop {
        match rx.recv().await {
            Ok(rec) => {
                if rec.seq <= last_sent {
                    continue; // already covered by the replay snapshot
                }
                // Opportunistic coalescing: drain whatever else is ready
                // into one Events frame.
                let mut records = vec![rec];
                while let Ok(more) = rx.try_recv() {
                    if more.seq > records.last().unwrap().seq {
                        records.push(more);
                    }
                }
                last_sent = records.last().unwrap().seq;
                let item = SubscriptionItem::Events {
                    session_id: handle.session_id.clone(),
                    epoch: handle.epoch(),
                    records,
                };
                if send_item(&w, id, item).await.is_err() {
                    return;
                }
            }
            // Fell behind the broadcast ring: heal with a fresh full replay
            // (spec 03 §Streaming commit — a fresh Reset heals).
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let (nrx, items, nlast) = handle.snapshot(None);
                rx = nrx;
                last_sent = nlast;
                for item in items {
                    if send_item(&w, id, item).await.is_err() {
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
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
