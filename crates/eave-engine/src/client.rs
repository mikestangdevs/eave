//! Headless wire client: connect (spawning the engine if absent), speak
//! the handshake, multiplex unary calls and subscriptions over one socket.
//!
//! This is the layer every surface sits on — the GPUI shell holds no wire
//! state of its own. Connect-first, spawn-on-refused is the UI-supervises-
//! engine contract (spec 01 §Topology); a spawned engine that loses the
//! instance lock exits `ENGINE_BUSY_EXIT` naming the holder, which
//! surfaces here as [`ClientError::Busy`] — renderable, not a crash
//! (spec 02 §Contended start).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedWriteHalf, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};

use eave_core::proc::ProcIdentity;
use eave_core::session::AgentConfig;
use eave_wire::{
    socket_path, BusyReport, Cursor, HelloOk, KillOk, ListOk, OpenOk, Request, RequestEnvelope,
    ResponseBody, ResponseEnvelope, SendOk, SessionInfo, SubscriptionItem, ENGINE_BUSY_EXIT,
    PROTO_VERSION,
};

const SPAWN_CONNECT_WINDOW: Duration = Duration::from_secs(10);
const SPAWN_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Another engine instance holds the data dir's lock and ours exited
    /// inert. `holder` may be `None` ("holder unknown") — still actionable.
    #[error("engine busy: another instance holds the lock{}", match holder { Some(h) => format!(" (pid {})", h.pid), None => " (holder unknown)".to_string() })]
    Busy { holder: Option<ProcIdentity> },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("engine connection lost")]
    Disconnected,
    #[error("engine spawn failed: {0}")]
    Spawn(String),
}

/// How to start an engine when the socket refuses. `None` = never spawn
/// (tests that manage the engine process themselves).
#[derive(Debug, Clone)]
pub struct EngineSpawn {
    pub program: PathBuf,
    pub args: Vec<String>,
}

enum Pending {
    Unary(oneshot::Sender<Result<serde_json::Value, String>>),
    Stream(mpsc::UnboundedSender<SubscriptionItem>),
}

pub struct Client {
    writer: Arc<TokioMutex<OwnedWriteHalf>>,
    pending: Arc<StdMutex<HashMap<u64, Pending>>>,
    next_id: AtomicU64,
    pub engine: ProcIdentity,
}

impl Client {
    /// Connect to a running engine; fail if none is serving.
    pub async fn connect(data_dir: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path(data_dir)).await?;
        Self::handshake(stream).await
    }

    /// Connect-first, spawn-on-refused (spec 01 §Topology). The spawned
    /// engine is its own process group and outlives us by design.
    pub async fn connect_or_spawn(
        data_dir: &Path,
        spawn: &EngineSpawn,
    ) -> Result<Self, ClientError> {
        if let Ok(stream) = UnixStream::connect(socket_path(data_dir)).await {
            return Self::handshake(stream).await;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("engine.log"))
            .map_err(|e| ClientError::Spawn(format!("open engine.log: {e}")))?;
        let mut child = {
            use std::os::unix::process::CommandExt;
            std::process::Command::new(&spawn.program)
                .args(&spawn.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(log)
                .process_group(0)
                .spawn()
                .map_err(|e| ClientError::Spawn(format!("spawn engine: {e}")))?
        };
        let deadline = std::time::Instant::now() + SPAWN_CONNECT_WINDOW;
        loop {
            if let Ok(stream) = UnixStream::connect(socket_path(data_dir)).await {
                // The engine is detached on purpose; we never wait on it.
                drop(child.stdout.take());
                return Self::handshake(stream).await;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.code() == Some(ENGINE_BUSY_EXIT) {
                        // The engine lost the instance lock and reported the
                        // holder on stdout before exiting inert.
                        let holder = child
                            .stdout
                            .take()
                            .and_then(|mut out| {
                                use std::io::Read;
                                let mut s = String::new();
                                out.read_to_string(&mut s).ok()?;
                                serde_json::from_str::<BusyReport>(s.lines().next()?).ok()
                            })
                            .and_then(|r| r.busy.holder);
                        return Err(ClientError::Busy { holder });
                    }
                    return Err(ClientError::Spawn(format!(
                        "engine exited during startup: {status}"
                    )));
                }
                Ok(None) => {}
                Err(e) => return Err(ClientError::Spawn(format!("waitpid: {e}"))),
            }
            if std::time::Instant::now() > deadline {
                return Err(ClientError::Spawn("engine never opened its socket".into()));
            }
            tokio::time::sleep(SPAWN_POLL).await;
        }
    }

    async fn handshake(stream: UnixStream) -> Result<Self, ClientError> {
        let (r, w) = stream.into_split();
        let pending: Arc<StdMutex<HashMap<u64, Pending>>> = Arc::default();
        let client = Self {
            writer: Arc::new(TokioMutex::new(w)),
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
            engine: ProcIdentity {
                pid: 0,
                start_tvsec: 0,
                start_tvusec: 0,
            },
        };

        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let env: ResponseEnvelope = match serde_json::from_str(&line) {
                    Ok(env) => env,
                    Err(_) => continue,
                };
                let mut pending = pending.lock().unwrap();
                match env.body {
                    ResponseBody::Ok(v) => {
                        if let Some(Pending::Unary(tx)) = pending.remove(&env.id) {
                            let _ = tx.send(Ok(v));
                        }
                    }
                    ResponseBody::Err { message } => match pending.remove(&env.id) {
                        Some(Pending::Unary(tx)) => {
                            let _ = tx.send(Err(message));
                        }
                        // An error on a subscription id ends that stream
                        // (dropping the sender surfaces as a closed
                        // receiver) — e.g. an older engine rejecting a
                        // method it doesn't know. The caller degrades;
                        // it must never wait forever on a stream the
                        // engine refused.
                        Some(Pending::Stream(_)) | None => {}
                    },
                    ResponseBody::Item(item) => {
                        if let Some(Pending::Stream(tx)) = pending.get(&env.id) {
                            if tx.send(item).is_err() {
                                pending.remove(&env.id);
                            }
                        }
                    }
                }
            }
            // Connection gone: wake every waiter (dropped senders read as
            // Disconnected / ended streams on the other side).
            pending.lock().unwrap().clear();
        });

        let hello: HelloOk = client
            .request(Request::Hello {
                proto: PROTO_VERSION,
            })
            .await?;
        if hello.proto != PROTO_VERSION {
            return Err(ClientError::Protocol(format!(
                "engine speaks proto {}, we speak {PROTO_VERSION}",
                hello.proto
            )));
        }
        Ok(Self {
            engine: hello.engine,
            ..client
        })
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        request: Request,
    ) -> Result<T, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, Pending::Unary(tx));
        self.write(&RequestEnvelope { id, request }).await?;
        let value = rx
            .await
            .map_err(|_| ClientError::Disconnected)?
            .map_err(ClientError::Engine)?;
        serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    async fn write(&self, env: &RequestEnvelope) -> Result<(), ClientError> {
        let mut line = serde_json::to_vec(env).expect("request envelopes serialize");
        line.push(b'\n');
        let mut w = self.writer.lock().await;
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>, ClientError> {
        Ok(self.request::<ListOk>(Request::List).await?.sessions)
    }

    /// Open with the engine's default agent. Unchanged signature on
    /// purpose: every existing caller (including the app) keeps working,
    /// and per-session config is opt-in through [`open_with_agent`].
    pub async fn open(
        &self,
        session_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<SessionInfo, ClientError> {
        self.open_with_agent(session_id, cwd, None).await
    }

    /// Open with a stated agent config (Slice P2) — which binary, which
    /// model, which extra flags, carried by the session rather than the
    /// engine.
    ///
    /// The engine refuses a provider whose permission posture it has not
    /// proven, and refuses re-opening an existing session under a different
    /// config; both come back as `ClientError` with the reason, because a
    /// silently substituted model is worse than a failed open.
    pub async fn open_with_agent(
        &self,
        session_id: Option<String>,
        cwd: Option<String>,
        agent: Option<AgentConfig>,
    ) -> Result<SessionInfo, ClientError> {
        Ok(self
            .request::<OpenOk>(Request::Open {
                session_id,
                cwd,
                agent,
            })
            .await?
            .info)
    }

    /// Sequential sends only — the delivery state is order-paired
    /// (spec 02 §Delivery state). Await each send before the next.
    pub async fn send(&self, session_id: &str, text: &str) -> Result<bool, ClientError> {
        Ok(self.send_detailed(session_id, text).await?.delivered)
    }

    /// [`send`](Self::send) keeping the engine's reason for a refusal.
    ///
    /// Exists so a FAILING test can say why. `send`'s bool is the delivery
    /// contract and stays the common path; an assertion on it alone reports
    /// that something did not arrive and nothing about what happened, which
    /// is precisely how an undelivered send stayed unexplained across
    /// several slices.
    pub async fn send_detailed(&self, session_id: &str, text: &str) -> Result<SendOk, ClientError> {
        self.request::<SendOk>(Request::Send {
            session_id: session_id.into(),
            text: text.into(),
        })
        .await
    }

    pub async fn kill(&self, session_id: &str) -> Result<KillOk, ClientError> {
        self.request::<KillOk>(Request::Kill {
            session_id: session_id.into(),
        })
        .await
    }

    /// Answer one open permission question (Slice K). `Ok(false)` = the
    /// question was no longer open — already answered, or withdrawn by the
    /// provider under the operator's click. A race, not an error.
    pub async fn decide(
        &self,
        session_id: &str,
        request_id: &str,
        decision: eave_wire::Decision,
    ) -> Result<bool, ClientError> {
        Ok(self
            .request::<eave_wire::DecideOk>(Request::Decide {
                session_id: session_id.into(),
                request_id: request_id.into(),
                decision,
            })
            .await?
            .answered)
    }

    /// Watch the session set (spec 05 §Watch): yields `Sessions` snapshot
    /// frames — one immediately, one on every session-set/state change.
    /// Fold by replacement. Carries no transcript content, ever.
    pub async fn watch(&self) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, Pending::Stream(tx));
        self.write(&RequestEnvelope {
            id,
            request: Request::Watch,
        })
        .await?;
        Ok(rx)
    }

    /// Observe one session's changed-file overview (spec 05 §Files):
    /// yields `Files` snapshot frames — the current one immediately, a
    /// fresh one after every engine-side git poll. Fold by replacement.
    /// The stream ends if the engine refuses the method (older engine)
    /// or the connection dies — render "unavailable", don't wait.
    pub async fn files(
        &self,
        session_id: &str,
    ) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        Ok(self
            .files_with_id_scoped(session_id, eave_wire::FileScope::default())
            .await?
            .1)
    }

    /// Like [`files`](Self::files), but also returns the stream's request
    /// id — the handle [`unsubscribe`](Self::unsubscribe) takes. A caller
    /// that switches sessions must unsubscribe the old id, or the engine
    /// keeps the stream (and its FS watcher) alive for the connection's
    /// whole life (Slice I1).
    pub async fn files_with_id(
        &self,
        session_id: &str,
    ) -> Result<(u64, mpsc::UnboundedReceiver<SubscriptionItem>), ClientError> {
        self.files_with_id_scoped(session_id, eave_wire::FileScope::default())
            .await
    }

    /// [`files_with_id`](Self::files_with_id) against a chosen basis
    /// (Slice AA): the whole working tree, or only what the current turn
    /// changed. The scope is fixed for the subscription's life — changing
    /// basis means `unsubscribe` and ask again, which is also what keeps
    /// the engine from paying for a turn scan nobody is reading.
    ///
    /// An engine predating the field ignores it and streams the working
    /// tree; the snapshot's own `basis` says which one arrived, so a
    /// client never has to assume it got what it asked for.
    pub async fn files_with_id_scoped(
        &self,
        session_id: &str,
        scope: eave_wire::FileScope,
    ) -> Result<(u64, mpsc::UnboundedReceiver<SubscriptionItem>), ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, Pending::Stream(tx));
        self.write(&RequestEnvelope {
            id,
            request: Request::Files {
                session_id: session_id.into(),
                scope,
            },
        })
        .await?;
        Ok((id, rx))
    }

    /// Cancel a streaming request by its id (Slice I1): the engine stops
    /// the task and releases what it held; this side drops the pending
    /// entry so stray in-flight frames on the id fall on the floor.
    /// Returns the engine's `stopped` fact. Idempotent both sides; an
    /// older engine errors on the method — surfaced, callers ignore.
    pub async fn unsubscribe(&self, id: u64) -> Result<bool, ClientError> {
        self.pending.lock().unwrap().remove(&id);
        Ok(self
            .request::<eave_wire::UnsubscribeOk>(Request::Unsubscribe { id })
            .await?
            .stopped)
    }

    /// One file's diff content (B2), computed by the engine at request
    /// time — unary. Nothing is cached anywhere: a fresh snapshot on the
    /// `files` subscription is the signal to re-request. An older engine
    /// errors on this id (the B1 id-echo fix) — surface the message,
    /// don't wait.
    pub async fn diff(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<eave_wire::FileDiff, ClientError> {
        self.diff_scoped(session_id, path, eave_wire::FileScope::WorkingTree)
            .await
    }

    /// [`diff`](Self::diff) against a stated basis — the scope the file list
    /// was showing when the row was clicked.
    pub async fn diff_scoped(
        &self,
        session_id: &str,
        path: &str,
        scope: eave_wire::FileScope,
    ) -> Result<eave_wire::FileDiff, ClientError> {
        Ok(self
            .request::<eave_wire::DiffOk>(Request::Diff {
                session_id: session_id.into(),
                path: path.into(),
                scope,
            })
            .await?
            .diff)
    }

    /// Move a never-started session into its own worktree (Slice I2) —
    /// the operator's opt-in isolation. The engine refuses (stated
    /// reasons) once anything has spawned, while live, and for non-git
    /// cwds; an older engine errors on the method.
    pub async fn isolate(&self, session_id: &str) -> Result<SessionInfo, ClientError> {
        Ok(self
            .request::<eave_wire::IsolateOk>(Request::Isolate {
                session_id: session_id.into(),
            })
            .await?
            .info)
    }

    /// Name a session, or clear its name with `None` (Stage AB).
    ///
    /// Returns the session as the engine now holds it, so the caller reads
    /// back the NORMALIZED title — trimmed, folded to one line, capped —
    /// rather than assuming the string it sent is the one that landed.
    pub async fn rename(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<SessionInfo, ClientError> {
        Ok(self
            .request::<eave_wire::RenameOk>(Request::Rename {
                session_id: session_id.into(),
                title: title.map(str::to_string),
            })
            .await?
            .info)
    }

    /// File a session away, or bring it back (Stage AC).
    ///
    /// One bit in `meta.json`; the journal is never touched, so this is
    /// reversible by calling it again with the other value. Returns the
    /// session as the engine now holds it — the caller reads the bit back
    /// rather than assuming its own write landed.
    ///
    /// **Archived sessions keep arriving on `list` and `watch`.** Hiding
    /// them is the caller's decision, and it has to be, or the bit would
    /// be unreachable from the surface that set it.
    pub async fn set_archived(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<SessionInfo, ClientError> {
        Ok(self
            .request::<eave_wire::ArchiveOk>(Request::Archive {
                session_id: session_id.into(),
                archived,
            })
            .await?
            .info)
    }

    /// Delete a session's eave worktree (Slice I2). `removed: false` is
    /// the engine's refusal restating freshly-measured dirty facts —
    /// re-send with `force: true` only when the operator confirmed
    /// against them (decision 2's two-step).
    pub async fn remove_worktree(
        &self,
        session_id: &str,
        force: bool,
    ) -> Result<eave_wire::RemoveWorktreeOk, ClientError> {
        self.request::<eave_wire::RemoveWorktreeOk>(Request::RemoveWorktree {
            session_id: session_id.into(),
            force,
        })
        .await
    }

    /// Subscribe per the cursor contract (spec 05-wire.md). The stream ends
    /// when the connection dies; resubscribe on a fresh client with the
    /// last cursor (or `None` if you hold no projection).
    pub async fn subscribe(
        &self,
        session_id: &str,
        cursor: Option<Cursor>,
    ) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        Ok(self.subscribe_with_id(session_id, cursor).await?.1)
    }

    // -----------------------------------------------------------------
    // Browser state (Slice B1). Thin pass-throughs: every rule lives in
    // `eave_core::browser` and every decision in the engine, so a second
    // client can be written without re-deriving any of it.

    /// Watch the whole tab strip: `Tabs` snapshot frames, one immediately
    /// and one on every change. Fold by replacement.
    pub async fn watch_tabs(
        &self,
    ) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        self.stream(Request::WatchTabs).await
    }

    /// Watch the bookmark hierarchy. Same contract.
    pub async fn watch_bookmarks(
        &self,
    ) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        self.stream(Request::WatchBookmarks).await
    }

    /// Open a tab; returns the id the engine minted.
    pub async fn tab_open(&self, url: Option<&str>, activate: bool) -> Result<String, ClientError> {
        Ok(self
            .request::<eave_wire::CreatedOk>(Request::TabOpen {
                url: url.map(str::to_string),
                group: None,
                index: None,
                content: eave_wire::TabContent::Page,
                opener: None,
                activate,
            })
            .await?
            .id)
    }

    /// Open a tab that VIEWS a session. The session is named, not owned:
    /// closing this tab does not touch it.
    pub async fn tab_open_session(
        &self,
        session_id: &str,
        group: Option<&str>,
    ) -> Result<String, ClientError> {
        Ok(self
            .request::<eave_wire::CreatedOk>(Request::TabOpen {
                url: None,
                group: group.map(str::to_string),
                index: None,
                content: eave_wire::TabContent::Session {
                    session_id: session_id.into(),
                },
                opener: None,
                activate: false,
            })
            .await?
            .id)
    }

    /// Any browser mutation that answers `changed`.
    pub async fn browser_do(&self, request: Request) -> Result<bool, ClientError> {
        Ok(self.request::<eave_wire::ChangedOk>(request).await?.changed)
    }

    /// Any browser creation that answers with a minted id.
    pub async fn browser_create(&self, request: Request) -> Result<String, ClientError> {
        Ok(self.request::<eave_wire::CreatedOk>(request).await?.id)
    }

    /// One tab's whole back/forward stack — deliberately not on the strip
    /// snapshot.
    pub async fn tab_nav(&self, tab_id: &str) -> Result<eave_wire::NavStack, ClientError> {
        Ok(self
            .request::<eave_wire::TabNavOk>(Request::TabNav {
                tab_id: tab_id.into(),
            })
            .await?
            .nav)
    }

    /// Ask history what to suggest for what has been typed.
    pub async fn history_query(
        &self,
        typed: &str,
        limit: Option<usize>,
    ) -> Result<eave_wire::HistoryQueryOk, ClientError> {
        self.request::<eave_wire::HistoryQueryOk>(Request::HistoryQuery {
            typed: typed.into(),
            limit,
        })
        .await
    }

    /// Register a streaming request and hand back its receiver. The shape
    /// every subscription here shares, factored out so a new one cannot
    /// get the registration order wrong (the pending entry must exist
    /// before the write, or a fast engine's first frame is dropped).
    async fn stream(
        &self,
        request: Request,
    ) -> Result<mpsc::UnboundedReceiver<SubscriptionItem>, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, Pending::Stream(tx));
        self.write(&RequestEnvelope { id, request }).await?;
        Ok(rx)
    }

    /// Like [`subscribe`](Self::subscribe), but also returns the stream's
    /// request id for [`unsubscribe`](Self::unsubscribe) — see
    /// [`files_with_id`](Self::files_with_id) for why switchers need it.
    pub async fn subscribe_with_id(
        &self,
        session_id: &str,
        cursor: Option<Cursor>,
    ) -> Result<(u64, mpsc::UnboundedReceiver<SubscriptionItem>), ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, Pending::Stream(tx));
        self.write(&RequestEnvelope {
            id,
            request: Request::Subscribe {
                session_id: session_id.into(),
                cursor,
            },
        })
        .await?;
        Ok((id, rx))
    }
}
