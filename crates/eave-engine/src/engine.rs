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
use eave_core::journal::{
    self, now_ms, Header, JournalEvent, JournalWriter, Record, FORMAT_VERSION,
};
use eave_core::proc::{self, PidState, ProcIdentity};
use eave_core::process;
use eave_core::recorder::{record_session, send_and_journal};
use eave_core::session::{
    conflicts_with_eave_flags, posture_is_verified, AgentConfig, AgentSession, ApprovalDecision,
    SessionWatch, SpawnConfig, POSTURE_VERIFIED_PROVIDERS,
};
use eave_core::store::{EngineLock, LockError, Meta, Store, META_FORMAT_VERSION};
use eave_core::thresholds::AttentionThresholds;
use eave_wire::{
    socket_path, ChangedFile, Cursor, DecideOk, Decision, DiffOk, FileStatus, FilesSnapshot,
    HelloOk, IsolateOk, KillOk, ListOk, OpenOk, RemoveWorktreeOk, Request, RequestEnvelope,
    ResponseBody, ResponseEnvelope, SendOk, SessionInfo, SubscriptionItem, UnsubscribeOk,
    PROTO_VERSION,
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
    /// Where isolation worktrees are created (Slice I2):
    /// `<root>/<repo-name>/<sid8>`. Deliberately NOT the data dir —
    /// worktrees are user-facing working checkouts the operator opens in
    /// an editor (comet's stance, kept). `EAVE_WORKTREES_DIR` overrides
    /// (tests).
    pub worktrees_root: PathBuf,
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
        // Slice K: the posture the fake agent will announce, so the
        // acceptance tests can drive the refusal paths through the real
        // spawn/verify path rather than around it.
        for key in [
            "FAKE_PERMISSION_MODE",
            "FAKE_APPROVAL_TOOL",
            "FAKE_APPROVAL_PATH",
        ] {
            if let Ok(v) = std::env::var(format!("EAVE_{key}")) {
                agent_env.push((key.to_string(), v));
            }
        }
        let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let worktrees_root = std::env::var_os("EAVE_WORKTREES_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"))
                    .join(".eave")
                    .join("worktrees")
            });
        Self {
            data_dir,
            agent_program: agent.into(),
            agent_env,
            provider,
            default_cwd,
            worktrees_root,
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
    /// The live agent leader's pid, set beside `live` and cleared with it
    /// (Stage L). Its only use is sampling resident memory for the
    /// aggregate resource fact — the (pid, start_time) identity that
    /// governs kills and probes lives in `meta.json` and the adapter, and
    /// this must never be used in its place.
    pid: Option<i32>,
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
    /// Canonical checkout root of the session cwd (Slice I2), probed off
    /// the open/load path (git runs as its own task, never inline).
    /// `None` until the probe lands; equality across sessions is the
    /// shared-checkout collision fact.
    checkout: StdMutex<Option<crate::git::CheckoutProbe>>,
    /// Permission questions the live agent is parked on, oldest first, and
    /// the posture it reported at `init` (Slice K). Both are ephemeral
    /// per-process facts — the durable truth is the journal, which carries
    /// every request and every decision — but the rail needs them as a
    /// count and a mode name without subscribing to the transcript.
    approvals: StdMutex<ApprovalState>,
    /// The session's eave-created worktree, if any (Slice I2): identity
    /// from `worktree.json` in the session dir (durable — crash
    /// re-detection starts from it and verifies against `git worktree
    /// list`, never engine memory); presence and dirty count stamped by
    /// scans.
    worktree: StdMutex<Option<WorktreeState>>,
}

/// Live permission facts for one session (Slice K). Identity and counts
/// only — the tool names and paths live in the journal and travel on the
/// session's own subscribe stream, never on watch (spec 05 §Watch).
#[derive(Debug, Default)]
struct ApprovalState {
    /// Open request ids, oldest first.
    pending: Vec<String>,
    /// What the CLI said its permission mode was at `init`. `None` until
    /// a process has announced itself.
    mode: Option<String>,
}

/// Live view of a session's worktree facts (Slice I2).
#[derive(Debug, Clone)]
struct WorktreeState {
    base: PathBuf,
    path: PathBuf,
    branch: String,
    present: bool,
    dirty_files: Option<u64>,
}

/// What `worktree.json` persists: the identity claim only — presence and
/// dirtiness are always re-measured from git, never trusted from disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WorktreeMeta {
    format_version: u32,
    base: PathBuf,
    path: PathBuf,
    branch: String,
}

const WORKTREE_META_VERSION: u32 = 1;

fn worktree_meta_path(session_dir: &Path) -> PathBuf {
    session_dir.join("worktree.json")
}

fn load_worktree_meta(session_dir: &Path) -> Option<WorktreeMeta> {
    let bytes = std::fs::read(worktree_meta_path(session_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_worktree_meta(session_dir: &Path, meta: &WorktreeMeta) -> io::Result<()> {
    let tmp = session_dir.join("worktree.json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(meta).expect("worktree meta serializes"),
    )?;
    std::fs::rename(&tmp, worktree_meta_path(session_dir))
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
            // Slice P2. A configured model is a fact ABOUT the session, in
            // the same class as `provider` and `cwd` — not transcript
            // content — so it is allowed on the watch channel by the
            // isolation invariant (spec 05 §Watch). `None` = the CLI's own
            // default, which eave neither knows nor invents.
            model: s.header.agent.model.clone(),
            cwd: s.header.cwd.clone(),
            created_ts_ms: s.header.created_ts_ms,
            state,
            live: s.live,
            epoch: s.epoch,
            last_seq: s.records.last().map(|r| r.seq).unwrap_or(0),
            checkout_root: self
                .checkout
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.root.display().to_string()),
            // Cross-session by definition; the engine's list() fills it.
            shared_with: Vec::new(),
            pending_approvals: {
                let a = self.approvals.lock().unwrap();
                a.pending.len() as u64
            },
            permission_mode: self.approvals.lock().unwrap().mode.clone(),
            // Stage L: sampled here rather than cached, because `info()` is
            // built only for `list` and `watch` snapshots — operator-paced,
            // not record-paced — and one `proc_pidinfo` per live session
            // per snapshot buys a number that is never stale. A cached
            // value would freeze on an idle session, which is exactly the
            // session whose cost the operator has stopped thinking about.
            rss_bytes: s.pid.and_then(proc::resident_bytes),
            worktree: self
                .worktree
                .lock()
                .unwrap()
                .as_ref()
                .map(|w| eave_wire::WorktreeInfo {
                    base: w.base.display().to_string(),
                    branch: w.branch.clone(),
                    present: w.present,
                    dirty_files: w.dirty_files,
                }),
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
                let tail: Vec<Record> = s
                    .records
                    .iter()
                    .filter(|r| r.seq > c.seq)
                    .cloned()
                    .collect();
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
    *ON.get_or_init(|| {
        std::env::var("EAVE_NO_FSWATCH")
            .map(|v| v != "1")
            .unwrap_or(true)
    })
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
    /// Resolve a session's agent config against the engine's defaults and
    /// **refuse anything whose permission posture is not proven** (Slice P2).
    ///
    /// This is the gate the whole slice exists to place. Per-session config
    /// means a caller can name any provider and any binary; Slice K's proof
    /// that the agent asks before it writes is Claude-specific and lives in
    /// `claude.rs`. Spawning an unverified provider would reintroduce the v0
    /// auto-accept blocker by the side door — a session writing to disk with
    /// nothing able to review it — so an unrecognised provider is refused
    /// with the reason, never spawned on the assumption it behaves like
    /// Claude. Fail-closed, exactly like `ASKING_MODES`.
    /// A session's agent config is fixed at open. Re-opening with a
    /// DIFFERENT one is refused rather than silently ignored: the caller
    /// asked for a model and would otherwise get another, which is the kind
    /// of quiet substitution this codebase states instead of doing.
    /// Re-opening with the same config, or with none, is fine.
    fn reject_config_change(have: &AgentConfig, want: Option<&AgentConfig>) -> Result<(), String> {
        match want {
            Some(w) if w != have => Err(format!(
                "session already exists with a different agent config (has {have:?}, asked for \
                 {w:?}) — a session's agent is fixed when it is opened, because its journal and \
                 its provider-side resume cursor were created under it"
            )),
            _ => Ok(()),
        }
    }

    fn resolve_agent(&self, want: &AgentConfig) -> Result<AgentConfig, String> {
        // The gate applies to a provider named IN CONFIG — the surface this
        // slice opens. A session that names none inherits the engine's own
        // `EAVE_AGENT`, which is the operator's deliberate launch-time
        // choice and predates this slice (I1 caveat 9); gating that too
        // would refuse every test harness and would be a different slice's
        // decision to make.
        //
        // Neither path skips Slice K's REAL proof, which is per-spawn and at
        // runtime: the CLI's own `init.permissionMode` is checked against
        // `ASKING_MODES` on every single spawn, and a session that cannot
        // prove it asks is refused there regardless of how it was
        // configured. This static check is the earlier, coarser one — it
        // refuses a provider eave has no adapter for at all, before a
        // process exists.
        let Some(provider) = want.provider.clone() else {
            return Ok(AgentConfig {
                provider: None,
                program: want.program.clone(),
                model: want.model.clone(),
                extra_args: want.extra_args.clone(),
            });
        };
        if !posture_is_verified(&provider) {
            return Err(format!(
                "provider {provider:?} has no verified permission posture — eave only knows how \
                 to make {:?} ask before it writes, and refuses to run an agent whose writes it \
                 cannot review. Slice K established the proof and it is provider-specific.",
                POSTURE_VERIFIED_PROVIDERS.join(", ")
            ));
        }
        if let Some(bad) = want
            .extra_args
            .iter()
            .find(|a| conflicts_with_eave_flags(a))
        {
            return Err(format!(
                "agent flag {bad:?} may not be set per session — it would override the permission \
                 posture or the resume cursor that eave depends on"
            ));
        }
        Ok(AgentConfig {
            provider: Some(provider),
            program: want.program.clone(),
            model: want.model.clone(),
            extra_args: want.extra_args.clone(),
        })
    }

    fn open_or_load(
        self: &Arc<Self>,
        session_id: Option<String>,
        cwd: Option<String>,
        agent: Option<AgentConfig>,
    ) -> Result<Arc<SessionHandle>, String> {
        let id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Validate BEFORE touching the registry or the disk: a refused
        // config must leave no session behind.
        let agent = agent.map(|a| self.resolve_agent(&a)).transpose()?;
        // Hold the registry lock across the whole open: two concurrent
        // opens of one id must never build two handles over one journal.
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(h) = sessions.get(&id) {
            Self::reject_config_change(&h.shared.lock().unwrap().header.agent, agent.as_ref())?;
            return Ok(Arc::clone(h));
        }
        let dir = self.store.session_dir(&id);
        let journal_path = Store::journal_path(&dir);
        let handle = if journal_path.exists() {
            let h = self.load_from_disk(&id)?;
            // The journal's own config wins — that is what "survives a
            // restart" means. A caller asking for a different one is told.
            Self::reject_config_change(&h.shared.lock().unwrap().header.agent, agent.as_ref())?;
            h
        } else {
            // The real claude CLI rejects `--session-id` values that are
            // not UUIDs, so a named id against it dies at first spawn —
            // refuse at open with the reason instead of creating a
            // session that can only ever fail. Overridden agents
            // (fake-agent, other CLIs) accept any id; generated ids are
            // always UUIDs and pass. Existing journals are exempt: their
            // history is real and loadable regardless.
            if self.cfg.agent_program == Path::new("claude") && uuid::Uuid::parse_str(&id).is_err()
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
                // The resolved config's provider, so `header.provider` and
                // `header.agent.provider` can never disagree about what
                // this session runs.
                provider: agent
                    .as_ref()
                    .and_then(|a| a.provider.clone())
                    .unwrap_or_else(|| self.cfg.provider.clone()),
                cwd: cwd.unwrap_or_else(|| self.cfg.default_cwd.display().to_string()),
                created_ts_ms: now_ms(),
                // Slice P2: written once, here, before anything spawns.
                agent: agent.clone().unwrap_or_default(),
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
                    pid: None,
                    attention: None,
                }),
                bcast,
                live: TokioMutex::new(None),
                watch_tx: self.watch_tx.clone(),
                files: StdMutex::new(None),
                files_tx,
                files_gate: TokioMutex::new(()),
                fs_watch: StdMutex::new(FsWatchCtl::default()),
                checkout: StdMutex::new(None),
                approvals: StdMutex::new(ApprovalState::default()),
                worktree: StdMutex::new(None),
            })
        };
        sessions.insert(id, Arc::clone(&handle));
        drop(sessions);
        // The session set changed — watchers re-snapshot.
        handle.poke_watch();
        // Session open is a files poll boundary (spec 05 §Files).
        tokio::spawn(refresh_files(Arc::clone(&handle)));
        // Checkout root probe (Slice I2): the shared-cwd collision fact,
        // computed off-loop like everything git.
        tokio::spawn(probe_checkout(Arc::clone(&handle)));
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
        let last_mode = last_permission_mode(&loaded.records);
        let (bcast, _) = broadcast::channel(BROADCAST_CAP);
        let (files_tx, _) = broadcast::channel(FILES_CAP);
        // The worktree identity claim loads from disk; presence and
        // dirtiness stay unknown until verified against git
        // (`verify_worktree` — never from memory, Slice I2).
        let worktree = load_worktree_meta(&dir).map(|m| WorktreeState {
            base: m.base,
            path: m.path,
            branch: m.branch,
            present: false,
            dirty_files: None,
        });
        Ok(Arc::new(SessionHandle {
            session_id: id.to_string(),
            dir,
            shared: StdMutex::new(Shared {
                header: loaded.header.clone(),
                epoch: new_epoch(),
                records: loaded.records,
                state,
                live: false,
                pid: None,
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
            checkout: StdMutex::new(None),
            // The last posture this session ran under is history: read it
            // back off the log so a cold rail states it before any respawn.
            approvals: StdMutex::new(ApprovalState {
                pending: Vec::new(),
                mode: last_mode,
            }),
            worktree: StdMutex::new(worktree),
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
        // Slice P2: the SESSION's config decides what spawns, not the
        // engine's environment — which is what makes it survive a restart
        // and govern a `--resume`. The engine's values fill only the gaps.
        // Re-checked here rather than trusted from the header: a journal is
        // a file on disk and could have been written by another build, and
        // this is the last point before a process is created.
        let agent = &header.agent;
        // Re-checked at the last point before a process exists, and only for
        // a provider the SESSION named: a journal is a file on disk and
        // could have been written by another build. An unconfigured session
        // runs the engine's default, exactly as it did before this slice.
        if let Some(provider) = agent.provider.as_deref() {
            if !posture_is_verified(provider) {
                return Err(format!(
                    "refusing to spawn session {} : provider {provider:?} has no verified \
                     permission posture (Slice K's proof is Claude-specific)",
                    handle.session_id
                ));
            }
        }
        let program: PathBuf = agent
            .program
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.agent_program.clone());
        let mut cfg = SpawnConfig::new(
            program,
            PathBuf::from(&header.cwd),
            handle.session_id.clone(),
        );
        cfg.resume = resume;
        cfg.extra_env = self.cfg.agent_env.clone();
        cfg.model = agent.model.clone();
        cfg.extra_args = agent
            .extra_args
            .iter()
            .filter(|a| !conflicts_with_eave_flags(a))
            .cloned()
            .collect();
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
        meta.save(&handle.dir)
            .map_err(|e| format!("save meta: {e}"))?;

        {
            let mut s = handle.shared.lock().unwrap();
            s.live = true;
            s.pid = Some(session.identity().pid);
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
                // Slice K: the rail's approval facts, kept from the same
                // records the transcript folds — one truth, two readers.
                // A change here is a state change a watcher can see, so it
                // pokes watch (a count, never a tool name or a path).
                if track_approvals(&tap_handle, &rec) {
                    tap_handle.poke_watch();
                }
                // Amendment 1: a CLI that cannot prove it will ask does not
                // get to keep running. Killed here rather than left alive
                // in a posture nobody reviewed — the transcript already
                // carries the stated reason.
                if let JournalEvent::Provider {
                    event: eave_core::events::SessionEvent::PostureRefused { .. },
                } = &rec.event
                {
                    tokio::spawn(refuse_posture(Arc::clone(&tap_handle)));
                }
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
                s.pid = None;
                s.state = state;
                // The overlay is per-process; the journal's terminal truth
                // governs now. Broadcast the clear so a live notice drops.
                s.attention = None;
            }
            // The process that asked the questions is gone; nothing can be
            // answered now (Slice K). The mode it ran under stays — that is
            // history the rail keeps stating.
            done_handle.approvals.lock().unwrap().pending.clear();
            let _ = done_handle
                .bcast
                .send(Fanout::Attention(AttentionSnapshot::Running));
            done_handle.poke_watch();
            *done_handle.live.lock().await = None;
        });

        Ok(Live { session, extra_tx })
    }

    /// Answer one open permission question (Slice K). Serialized on the
    /// same `live` lock as sends and kills, so a decision can never
    /// interleave with a user message mid-line on the child's stdin.
    ///
    /// Order is deliberate: the decision is written to the pipe FIRST and
    /// journaled only once that write succeeded. The record means "this
    /// answer reached the agent" — which is the fact an audit needs — so a
    /// failed write must leave no record claiming otherwise, and leaves
    /// the question open for the operator to try again.
    async fn decide(
        self: &Arc<Self>,
        id: &str,
        request_id: &str,
        decision: Decision,
    ) -> Result<DecideOk, String> {
        let handle = self.session(id)?;
        let mut live = handle.live.lock().await;
        let l = live
            .as_mut()
            .ok_or("session is not live — nothing is waiting on an answer")?;
        let core = match &decision {
            Decision::Allow => ApprovalDecision::Allow,
            Decision::Deny { message } => ApprovalDecision::Deny {
                message: if message.trim().is_empty() {
                    "The operator denied this action in eave.".to_string()
                } else {
                    message.clone()
                },
            },
        };
        let answered = l
            .session
            .respond_to_approval(request_id, &core)
            .await
            .map_err(|e| format!("could not answer the agent: {e}"))?;
        if answered {
            let (allowed, message) = match &core {
                ApprovalDecision::Allow => (true, None),
                ApprovalDecision::Deny { message } => (false, Some(message.clone())),
            };
            l.extra_tx
                .send(JournalEvent::ApprovalDecided {
                    request_id: request_id.to_string(),
                    allowed,
                    message,
                })
                .await
                .map_err(|_| "recorder gone".to_string())?;
        }
        Ok(DecideOk { answered })
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

    /// Slice I2: move a never-started session into its own worktree.
    /// Isolation is the operator's opt-in (decision 1) and is only legal
    /// while nothing has ever spawned — the provider CLI keys its resume
    /// history off the cwd, so after the first send the cwd is fixed.
    async fn isolate(self: &Arc<Self>, id: &str) -> Result<SessionInfo, String> {
        let handle = self.session(id)?;
        // Hold the live lock for the whole rewrite: `send` serializes on
        // it, so no spawn can race the cwd change.
        let live = handle.live.lock().await;
        let attached = live
            .as_ref()
            .map(|l| !l.extra_tx.is_closed())
            .unwrap_or(false);
        if attached {
            return Err("session is live — isolation is chosen before the first send".into());
        }
        {
            let s = handle.shared.lock().unwrap();
            if !s.records.is_empty() || s.state != "new" {
                return Err(
                    "session already started — its cwd is fixed after the first send".into(),
                );
            }
        }
        if handle.worktree.lock().unwrap().is_some() {
            return Err("session is already isolated in a worktree".into());
        }
        let cwd = PathBuf::from(handle.shared.lock().unwrap().header.cwd.clone());
        let probe = crate::git::checkout_root(&cwd).await;
        if !probe.is_repo {
            return Err(format!(
                "{} is not inside a git repository — no worktree is possible; sessions can only share it",
                cwd.display()
            ));
        }
        let base = probe.root;
        // The stated naming scheme: dir `<root>/<repo>/<sid8>`, branch
        // `eave/<sid8>` — sid8 = the session id's first 8 alphanumerics.
        // Predictable, session-owned, and safe to delete on clean removal.
        let sid8: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect::<String>()
            .to_lowercase();
        if sid8.is_empty() {
            return Err("session id yields no worktree name".into());
        }
        let repo_name = base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        let wt_path = self.cfg.worktrees_root.join(&repo_name).join(&sid8);
        if wt_path.exists() {
            return Err(format!(
                "worktree path already exists: {}",
                wt_path.display()
            ));
        }
        if let Some(parent) = wt_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create worktrees dir: {e}"))?;
        }
        let branch = format!("eave/{sid8}");
        crate::git::worktree_add(&base, &wt_path, &branch)
            .await
            .map_err(|e| format!("git worktree add refused: {e}"))?;
        // Rewrite the header-only journal with the new cwd: fresh journal
        // at a temp path, rename over — atomic, header fsync'd by
        // `create`. Legal precisely because no records exist (checked
        // above, under the live lock).
        let mut header = handle.shared.lock().unwrap().header.clone();
        header.cwd = wt_path.display().to_string();
        let journal_path = Store::journal_path(&handle.dir);
        let tmp = handle.dir.join("journal.jsonl.tmp");
        let _ = std::fs::remove_file(&tmp);
        let rewrite = JournalWriter::create(&tmp, &header)
            .map(drop)
            .and_then(|()| std::fs::rename(&tmp, &journal_path));
        if let Err(e) = rewrite {
            // The worktree exists but the session still points at the old
            // cwd — state exactly that; nothing is silently half-done.
            return Err(format!(
                "worktree created at {} but the session cwd rewrite failed: {e}",
                wt_path.display()
            ));
        }
        if let Ok(mut meta) = Meta::load(&handle.dir) {
            meta.cwd = header.cwd.clone();
            let _ = meta.save(&handle.dir);
        }
        save_worktree_meta(
            &handle.dir,
            &WorktreeMeta {
                format_version: WORKTREE_META_VERSION,
                base: base.clone(),
                path: wt_path.clone(),
                branch: branch.clone(),
            },
        )
        .map_err(|e| format!("worktree created but worktree.json save failed: {e}"))?;
        handle.shared.lock().unwrap().header = header;
        *handle.worktree.lock().unwrap() = Some(WorktreeState {
            base,
            path: wt_path.clone(),
            branch,
            present: true,
            // git just created it — clean by construction.
            dirty_files: Some(0),
        });
        *handle.checkout.lock().unwrap() = Some(crate::git::CheckoutProbe {
            root: std::fs::canonicalize(&wt_path).unwrap_or(wt_path),
            is_repo: true,
        });
        drop(live);
        handle.poke_watch();
        tokio::spawn(refresh_files(Arc::clone(&handle)));
        self.info_with_sharing(id)
    }

    /// Slice I2: delete a session's eave worktree. Decision 2 verbatim:
    /// eave NEVER deletes dirty work unforced — the counts are measured
    /// freshly here, restated in the refusal, and only a `force` call
    /// (the operator's second, meant click) proceeds past them.
    async fn remove_worktree(
        self: &Arc<Self>,
        id: &str,
        force: bool,
    ) -> Result<RemoveWorktreeOk, String> {
        let handle = self.session(id)?;
        let live = handle.live.lock().await;
        let attached = live
            .as_ref()
            .map(|l| !l.extra_tx.is_closed())
            .unwrap_or(false);
        if attached {
            return Err("session is live — kill it before deleting its worktree".into());
        }
        let Some(wt) = handle.worktree.lock().unwrap().clone() else {
            return Err("session has no eave-created worktree".into());
        };
        let exists = wt.path.exists();
        let dirty_files = if exists {
            let snap = crate::git::scan(&wt.path).await;
            match snap.state {
                eave_wire::GitState::Ok => Some(snap.files.len() as u64 + snap.omitted),
                // Unknown is not clean: refuses below unless forced.
                _ => None,
            }
        } else {
            Some(0) // the directory is gone; no files left to protect
        };
        let unmerged_commits = crate::git::unmerged_count(&wt.base, &wt.branch).await;
        let protectable =
            dirty_files.is_none_or(|n| n > 0) || unmerged_commits.is_none_or(|n| n > 0);
        if protectable && !force {
            // A refusal carrying its facts, not an error. Stamp the fresh
            // count so the rail states what the reply states.
            {
                let mut slot = handle.worktree.lock().unwrap();
                if let Some(w) = slot.as_mut() {
                    w.dirty_files = dirty_files;
                    w.present = exists;
                }
            }
            drop(live);
            handle.poke_watch();
            return Ok(RemoveWorktreeOk {
                removed: false,
                dirty_files,
                unmerged_commits,
            });
        }
        if exists {
            crate::git::worktree_remove(&wt.base, &wt.path, force)
                .await
                .map_err(|e| format!("git worktree remove refused: {e}"))?;
        }
        // Tidy is best-effort past the removal itself: prune the stale
        // registration; delete the branch ONLY if it is eave's own
        // naming — never a branch the operator checked out themselves.
        let _ = crate::git::worktree_prune(&wt.base).await;
        if wt.branch.starts_with("eave/") {
            let _ = crate::git::branch_delete(&wt.base, &wt.branch).await;
        }
        let _ = std::fs::remove_file(worktree_meta_path(&handle.dir));
        *handle.worktree.lock().unwrap() = None;
        drop(live);
        handle.poke_watch();
        tokio::spawn(refresh_files(Arc::clone(&handle)));
        Ok(RemoveWorktreeOk {
            removed: true,
            dirty_files,
            unmerged_commits,
        })
    }

    fn list(&self) -> ListOk {
        let sessions = self.sessions.lock().unwrap();
        let mut infos: Vec<SessionInfo> = sessions.values().map(|h| h.info()).collect();
        drop(sessions);
        infos.sort_by_key(|a| a.created_ts_ms);
        fill_sharing(&mut infos);
        ListOk { sessions: infos }
    }

    /// One session's info with the cross-session sharing fact filled —
    /// what unary replies about a single session return.
    fn info_with_sharing(&self, id: &str) -> Result<SessionInfo, String> {
        self.list()
            .sessions
            .into_iter()
            .find(|s| s.session_id == id)
            .ok_or_else(|| format!("unknown session {id}"))
    }
}

/// Where the unclaimed-worktree sweep leaves its findings (Stage L).
fn unclaimed_report_path(data_dir: &Path) -> PathBuf {
    data_dir.join("unclaimed-worktrees.txt")
}

/// State the sweep's result somewhere the operator can actually reach it.
///
/// The sweep has run since Slice I2 and reported only to engine stderr,
/// which no operator ever sees — so directories eave itself created, and
/// then lost track of, accumulated in total silence. They are never
/// deleted (decision 2: the operator owns destruction), which is exactly
/// why they have to be *findable*.
///
/// The file is rewritten from scratch on every engine start and **deleted
/// when the sweep finds nothing**: a stale list of paths that have since
/// been cleaned up would be worse than no list, because it would send
/// someone looking for directories that are already gone. This is not a
/// UI surface and does not pretend to be one — the rail has no
/// engine-wide fact channel, and inventing one for this would mean a
/// second cross-session broadcast beside `watch`.
fn write_unclaimed_report(path: &Path, root: &Path, found: &[PathBuf]) {
    if found.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut body = String::from(
        "# Unclaimed eave worktrees\n\n         Directories under the worktrees root that no session in this data\n         dir claims — a crashed `isolate`, or a session dir removed by hand.\n         eave has NOT touched them and will not: it never deletes work it\n         cannot prove is owned and clean. Inspect them, and remove them\n         yourself when you are sure (`git worktree remove`, or `rm -rf` plus\n         `git worktree prune` in the base repo).\n\n         Rewritten on every engine start; deleted when the sweep comes back\n         empty, so this file is never a stale list.\n\n",
    );
    body.push_str(&format!("worktrees root: {}\n\n", root.display()));
    for p in found {
        body.push_str(&format!("{}\n", p.display()));
    }
    if let Err(e) = std::fs::write(path, body) {
        eprintln!("engine: cannot write {}: {e}", path.display());
    }
}

/// Stamp `shared_with` (Slice I2): sessions whose canonical checkout
/// roots are equal work in the SAME files. Sessions with an unknown root
/// (probe not landed / non-canonicalizable) share with nobody — stated
/// as absence, never guessed.
/// The permission posture of the most recent process segment in a log
/// (Slice K). History, not a guess: it is whatever the CLI itself stated
/// at `init` the last time this session ran.
fn last_permission_mode(records: &[Record]) -> Option<String> {
    records.iter().rev().find_map(|r| match &r.event {
        JournalEvent::Provider {
            event:
                eave_core::events::SessionEvent::Init {
                    permission_mode, ..
                },
        } => permission_mode.clone(),
        _ => None,
    })
}

fn fill_sharing(infos: &mut [SessionInfo]) {
    let mut by_root: HashMap<String, Vec<String>> = HashMap::new();
    for s in infos.iter() {
        if let Some(root) = &s.checkout_root {
            by_root
                .entry(root.clone())
                .or_default()
                .push(s.session_id.clone());
        }
    }
    for s in infos.iter_mut() {
        let Some(root) = &s.checkout_root else {
            continue;
        };
        let Some(ids) = by_root.get(root) else {
            continue;
        };
        s.shared_with = ids
            .iter()
            .filter(|id| **id != s.session_id)
            .cloned()
            .collect();
    }
}

/// Probe the session cwd's canonical checkout root (Slice I2) and stamp
/// it; watchers re-snapshot when the fact lands or changes.
/// Fold one record into the session's live approval facts (Slice K).
/// Returns whether anything a watcher can see changed.
fn track_approvals(handle: &Arc<SessionHandle>, rec: &Record) -> bool {
    use eave_core::events::SessionEvent as E;
    let mut a = handle.approvals.lock().unwrap();
    match &rec.event {
        JournalEvent::Provider { event } => match event {
            E::Init {
                permission_mode, ..
            } => {
                let changed = a.mode != *permission_mode;
                a.mode = permission_mode.clone();
                changed
            }
            E::ApprovalRequested { request_id, .. } => {
                if a.pending.iter().any(|id| id == request_id) {
                    return false;
                }
                a.pending.push(request_id.clone());
                true
            }
            E::ApprovalWithdrawn { request_id, .. } => {
                let before = a.pending.len();
                a.pending.retain(|id| id != request_id);
                a.pending.len() != before
            }
            // A turn cannot end with a question still open; if the
            // provider says it did, believe the provider.
            E::TurnCompleted { .. } => {
                let had = !a.pending.is_empty();
                a.pending.clear();
                had
            }
            _ => false,
        },
        JournalEvent::ApprovalDecided { request_id, .. } => {
            let before = a.pending.len();
            a.pending.retain(|id| id != request_id);
            a.pending.len() != before
        }
        JournalEvent::Closed { .. } => {
            let had = !a.pending.is_empty();
            a.pending.clear();
            had
        }
        _ => false,
    }
}

/// Amendment 1's enforcement: end a session whose CLI announced a posture
/// eave will not supervise. The kill ladder is the normal one — proof of
/// exit, never a signal fired and hoped over (spec 01 §Kill).
async fn refuse_posture(handle: Arc<SessionHandle>) {
    let mut live = handle.live.lock().await;
    if let Some(l) = live.as_mut() {
        let _ = l.session.kill().await;
    }
}

async fn probe_checkout(handle: Arc<SessionHandle>) {
    let cwd = PathBuf::from(handle.shared.lock().unwrap().header.cwd.clone());
    let probe = crate::git::checkout_root(&cwd).await;
    let changed = {
        let mut slot = handle.checkout.lock().unwrap();
        let changed = slot.as_ref() != Some(&probe);
        *slot = Some(probe);
        changed
    };
    if changed {
        handle.poke_watch();
    }
}

/// Verify a session's claimed worktree against git (Slice I2 restart
/// re-detection): ground truth is `git worktree list` in the base repo —
/// never engine memory, never the json alone. Present worktrees get an
/// eager scan so an orphan's dirty count reaches the rail without anyone
/// subscribing; absent ones are stated (`present: false`), never pruned
/// automatically — the operator's delete affordance owns destruction.
async fn verify_worktree(handle: Arc<SessionHandle>) {
    let Some(wt) = handle.worktree.lock().unwrap().clone() else {
        return;
    };
    let listed = crate::git::worktree_list(&wt.base)
        .await
        .unwrap_or_default();
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canon(&wt.path);
    let present = wt.path.exists() && listed.iter().any(|p| canon(p) == target);
    {
        let mut slot = handle.worktree.lock().unwrap();
        if let Some(w) = slot.as_mut() {
            w.present = present;
        }
    }
    handle.poke_watch();
    if present {
        refresh_files(Arc::clone(&handle)).await;
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

async fn serve_locked(
    cfg: EngineConfig,
    store: Store,
    _lock: EngineLock,
) -> Result<(), ServeError> {
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
                engine
                    .sessions
                    .lock()
                    .unwrap()
                    .insert(id, Arc::clone(&handle));
                // Slice I2: checkout-root probe for the sharing fact, and
                // worktree re-detection for sessions that claim one —
                // verified against `git worktree list`, never memory, and
                // eagerly scanned so a crash orphan's dirty count reaches
                // the rail without a subscription. Off-loop, per session.
                tokio::spawn(probe_checkout(Arc::clone(&handle)));
                if handle.worktree.lock().unwrap().is_some() {
                    tokio::spawn(verify_worktree(handle));
                }
            }
            Err(e) => eprintln!("engine: skipping session {id}: {e}"),
        }
    }

    // Slice I2: sweep the worktrees root for directories no known session
    // claims (a crashed isolate, a hand-deleted session dir). Stated in
    // the log and left alone — eave never deletes what it cannot prove is
    // an owned, clean worktree; the operator owns destruction.
    {
        let claimed: Vec<PathBuf> = engine
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|h| h.worktree.lock().unwrap().as_ref().map(|w| w.path.clone()))
            .collect();
        let root = engine.cfg.worktrees_root.clone();
        let report = unclaimed_report_path(&engine.cfg.data_dir);
        tokio::spawn(async move {
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let claimed: Vec<PathBuf> = claimed.iter().map(|p| canon(p)).collect();
            let mut found: Vec<PathBuf> = Vec::new();
            if let Ok(repos) = std::fs::read_dir(&root) {
                for repo in repos.flatten().filter(|e| e.path().is_dir()) {
                    let Ok(trees) = std::fs::read_dir(repo.path()) else {
                        continue;
                    };
                    for tree in trees.flatten().filter(|e| e.path().is_dir()) {
                        let path = canon(&tree.path());
                        if !claimed.contains(&path) {
                            eprintln!(
                                "engine: unclaimed worktree {} — no session owns it; left alone",
                                path.display()
                            );
                            found.push(path);
                        }
                    }
                }
            }
            write_unclaimed_report(&report, &root, &found);
        });
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
    // Streaming tasks keyed by their request envelope id, so `unsubscribe`
    // can name and stop one (Slice I1). Aborting a task drops its future
    // at the current await point — RAII guards inside (the files stream's
    // `FilesSubGuard`) run their drops, releasing subscriber-scoped
    // resources. Connection close aborts whatever remains.
    let mut subs: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();

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
            Request::Open {
                session_id,
                cwd,
                agent,
            } => match engine.open_or_load(session_id, cwd, agent) {
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
                    subs.insert(id, tokio::spawn(run_subscription(handle, cursor, w, id)));
                    continue; // items flow on this id; no unary reply
                }
                Err(message) => ResponseBody::Err { message },
            },
            Request::Watch => {
                let w = Arc::clone(&w);
                subs.insert(id, tokio::spawn(run_watch(Arc::clone(&engine), w, id)));
                continue; // snapshots flow on this id; no unary reply
            }
            Request::Files { session_id } => match engine.session(&session_id) {
                Ok(handle) => {
                    let w = Arc::clone(&w);
                    subs.insert(id, tokio::spawn(run_files(handle, w, id)));
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
                    subs.insert(id, tokio::spawn(run_diff(handle, w, id, path)));
                    continue;
                }
                Err(message) => ResponseBody::Err { message },
            },
            Request::Isolate { session_id } => {
                // Off the connection loop (git work), but deliberately NOT
                // in `subs`: this call mutates session state (worktree +
                // cwd rewrite) and must run to completion even if the
                // client disconnects mid-way — an aborted half-isolate
                // would leave an unclaimed worktree behind.
                let w = Arc::clone(&w);
                let engine = Arc::clone(&engine);
                tokio::spawn(async move {
                    let body = match engine.isolate(&session_id).await {
                        Ok(info) => ok_value(&IsolateOk { info }),
                        Err(message) => ResponseBody::Err { message },
                    };
                    let _ = write_frame(&w, &ResponseEnvelope { id, body }).await;
                });
                continue;
            }
            Request::RemoveWorktree { session_id, force } => {
                // Same discipline as isolate: run to completion, never
                // abort a removal half-way through its git sequence.
                let w = Arc::clone(&w);
                let engine = Arc::clone(&engine);
                tokio::spawn(async move {
                    let body = match engine.remove_worktree(&session_id, force).await {
                        Ok(ok) => ok_value(&ok),
                        Err(message) => ResponseBody::Err { message },
                    };
                    let _ = write_frame(&w, &ResponseEnvelope { id, body }).await;
                });
                continue;
            }
            Request::Decide {
                session_id,
                request_id,
                decision,
            } => match engine.decide(&session_id, &request_id, decision).await {
                Ok(ok) => ok_value(&ok),
                Err(message) => ResponseBody::Err { message },
            },
            Request::Unsubscribe { id: target } => {
                // Stop the named stream and release what it held (the
                // files stream's watcher refcount drops via guard drop).
                // Idempotent by design — see the wire doc.
                let stopped = match subs.remove(&target) {
                    Some(task) => {
                        // Hold the write lock across the abort: a stream
                        // mid-frame holds it too, so waiting here means the
                        // abort can never land inside a `write_all` and
                        // tear a frame on the shared socket.
                        let gate = w.lock().await;
                        let live = !task.is_finished();
                        task.abort();
                        drop(gate);
                        live
                    }
                    None => false,
                };
                ok_value(&UnsubscribeOk { stopped })
            }
        };
        if write_frame(&w, &ResponseEnvelope { id, body })
            .await
            .is_err()
        {
            break;
        }
    }

    // Client gone (clean close or SIGKILL — indistinguishable and treated
    // identically): stop its subscriptions. Sessions are untouched; the
    // engine owns them (spec 01 §Crash & engine shutdown: UI quit is a
    // no-op for sessions).
    for s in subs.into_values() {
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
    // Slice I2: for worktree sessions the scan doubles as the dirty-count
    // and presence stamp — the facts the rail states on the session row
    // (and the crash-orphan statement after a restart's eager scan).
    let wt_changed = {
        let mut slot = handle.worktree.lock().unwrap();
        match slot.as_mut() {
            Some(w) => {
                let dirty = match snapshot.state {
                    eave_wire::GitState::Ok => Some(snapshot.files.len() as u64 + snapshot.omitted),
                    // Failed scan = unknown, never a fake zero.
                    _ => None,
                };
                let present = w.path.exists();
                let changed = w.dirty_files != dirty || w.present != present;
                w.dirty_files = dirty;
                w.present = present;
                changed
            }
            None => false,
        }
    };
    *handle.files.lock().unwrap() = Some(snapshot);
    let _ = handle.files_tx.send(());
    if wt_changed {
        handle.poke_watch();
    }
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
    let mut watcher =
        match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let _ = tx.send(res.map(|_| ()).map_err(|_| ()));
        }) {
            Ok(w) => w,
            Err(_) => return, // polling fallback stands
        };
    if watcher
        .watch(&cwd, notify::RecursiveMode::Recursive)
        .is_err()
    {
        return; // non-existent / unwatchable cwd: polling fallback stands
    }
    // Slice I2: a LINKED worktree keeps its HEAD and index in the base
    // repo's private stanza (`<base>/.git/worktrees/<name>`) — OUTSIDE
    // the cwd being watched, so commits and index moves there would
    // never fire an event and the rail would go stale until a poll. Ask
    // git where the real git dir is and cover it too when it lies
    // outside the cwd. Best-effort: failure just means the poll fallback
    // truth-level B2 already had.
    if let Some(gd) = crate::git::git_dir(&cwd).await {
        if !gd.starts_with(&cwd) {
            let _ = watcher.watch(&gd, notify::RecursiveMode::Recursive);
        }
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
