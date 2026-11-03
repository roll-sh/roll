//! The orchestrator: a [`Host`] makes this machine behave like a T3 Code server
//! (docs/code/agents.md). It owns every record, runs the provider sessions, and turns their
//! events into entities.
//!
//! Ported from T3 Code's `apps/server/src/orchestration-v2/` (`Orchestrator.ts`,
//! `ThreadMessageIntake.ts`, `RunExecutionService.ts`, `ProviderEventIngestor.ts`,
//! `RunFinalizationService.ts`, `CommandReceiptStore.ts`), trimmed to agent-proto's model.
//!
//! - Every record lives in the [`Store`]; a change is written (with its `seq`) before it is
//!   broadcast, so a subscriber never sees an upsert that a restart would lose.
//! - Each thread has one runtime ([`Rt`]) behind an async lock: commands and the thread's provider
//!   events take it in turn, so a thread's records change one step at a time. Provider events are
//!   drained into an unbounded queue as they arrive, so a provider waiting on its event channel
//!   can never deadlock a command that is waiting on the provider.
//! - Command ids are idempotency keys (T3's command receipts): the first reply is stored and
//!   returned again for the same id, also after a restart.

mod handoff;
mod queries;
mod runs;
mod tasks;
mod terminals;
mod threads;

pub(crate) use tasks::{Delegation, TaskError};

use crate::Outbound;
use crate::config::Config;
use crate::provider::{Provider, Session};
use crate::store::Store;
use agent_proto::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, broadcast};

/// Command receipts are kept this long (a client retries within seconds; a week is generous).
const RECEIPT_DAYS: u64 = 7;

pub struct Host {
    me: Weak<Host>,
    config: Config,
    store: Store,
    providers: BTreeMap<ProviderId, Arc<dyn Provider>>,
    out: broadcast::Sender<Outbound>,
    info: std::sync::Mutex<MachineInfo>,
    threads: std::sync::Mutex<HashMap<ThreadId, Arc<AsyncMutex<Rt>>>>,
    /// Commands in flight by id, so a retry that races the first call waits for its receipt.
    inflight: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Serializes project add/remove (path uniqueness, cascading deletes).
    projects: AsyncMutex<()>,
    terminals: std::sync::Mutex<HashMap<TerminalId, Arc<terminals::LiveTerminal>>>,
    /// roll's orchestrator MCP server, given to every provider session.
    pub(crate) mcp: crate::mcp::Server,
    /// Delegated tasks in flight (`tasks.rs`).
    tasks: tasks::Tasks,
}

/// Why a command failed; becomes `Reply::Error`.
#[derive(Debug)]
pub(crate) enum Fail {
    NotFound(String),
    Invalid(String),
    Conflict(String),
    Provider(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Fail::Internal(e)
    }
}

pub(crate) type Res<T> = Result<T, Fail>;

impl Fail {
    fn into_reply(self) -> Reply {
        let (code, message) = match self {
            Fail::NotFound(m) => (ErrorCode::NotFound, m),
            Fail::Invalid(m) => (ErrorCode::Invalid, m),
            Fail::Conflict(m) => (ErrorCode::Conflict, m),
            Fail::Provider(m) => (ErrorCode::Provider, m),
            Fail::Internal(e) => {
                tracing::warn!("agent command failed: {e:#}");
                (ErrorCode::Internal, format!("{e:#}"))
            }
        };
        Reply::Error(ReplyError { code, message })
    }
}

/// The orchestrator's private per-thread state, kept in `thread_state` (never mirrored).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Private {
    /// The provider's own conversation id, per provider id.
    #[serde(default)]
    native: BTreeMap<String, String>,
    /// The newest run ordinal each provider's native conversation contains. A provider missing
    /// here starts fresh and gets the whole thread (and `preamble`) handed over.
    #[serde(default)]
    seen: BTreeMap<String, u32>,
    /// How many native turns each run (by ordinal) took: one, plus one per steering restart.
    #[serde(default)]
    turns: BTreeMap<u32, u32>,
    /// Where each run (by ordinal) ended in its provider's native conversation (`TurnRef`): a
    /// fork at that run starts there.
    #[serde(default)]
    refs: BTreeMap<u32, NativeRef>,
    /// Per provider whose rollbacks resume at a turn ([`RollbackStyle::ResumeAt`]): the turn of
    /// its native conversation that the thread goes on from after a rollback (T3's
    /// `nativeConversationHeadRef`). Every session resumes there until a turn goes past it.
    ///
    /// [`RollbackStyle::ResumeAt`]: crate::provider::RollbackStyle::ResumeAt
    #[serde(default)]
    heads: BTreeMap<String, NativeRef>,
    /// How a fork gets its parent's conversation; its first session settles it.
    #[serde(default)]
    transfer: Option<Transfer>,
    /// Context inherited from a parent thread (a fork's summary of its parent).
    #[serde(default)]
    preamble: Option<String>,
    /// The title is a placeholder: the first message names the thread.
    #[serde(default)]
    auto_title: bool,
    /// The commit of the newest live checkpoint: the base of the next run's diff.
    #[serde(default)]
    head: Option<String>,
    /// Each checkpoint's base commit (the checkpoint it is diffed against).
    #[serde(default)]
    from: BTreeMap<String, String>,
}

/// A turn in a provider's native conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeRef {
    /// The conversation (`NativeThread`).
    thread: String,
    /// The turn's ref (`TurnRef`).
    turn: String,
}

/// A fork's context transfer from its parent (T3's `ContextTransfer` of type `fork`): pending
/// until the fork's first session, then resolved natively or portably.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub(crate) enum Transfer {
    /// Fork the parent's native conversation with `provider` at the end of its run `upto`, if
    /// the fork's first session is on that provider and the point is reachable.
    Pending { provider: String, upto: u32 },
    /// The fork's conversation with `provider` is a native copy of the parent's
    /// (`resolved_native`), made at turn `from` of the parent's conversation when the fork point
    /// was one (T3 keeps the copy's head message id). Rolling the fork back to before its own
    /// first run copies the parent's conversation there again.
    Native {
        #[serde(default)]
        provider: String,
        #[serde(default)]
        from: Option<NativeRef>,
    },
    /// The fork's provider starts fresh with the summary in `preamble` (`resolved_portable`).
    Portable,
}

/// A thread's live state.
pub(crate) struct Rt {
    thread: Thread,
    private: Private,
    /// The checkout is a git work tree (checkpoints and diffs work).
    git: bool,
    live: Option<Live>,
    active: Option<runs::Active>,
    /// Bumped for every session, so events from a replaced session are ignored.
    generation: u64,
    /// The thread was deleted; whoever still holds this runtime must stop.
    gone: bool,
}

/// A running provider session and what it was started with.
pub(crate) struct Live {
    provider: ProviderId,
    session: Arc<dyn Session>,
    generation: u64,
    capabilities: Capabilities,
    model: Option<String>,
    effort: Option<String>,
    runtime: RuntimeMode,
    interaction: InteractionMode,
}

pub(crate) fn now() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A random id suffix: 16 lowercase hex digits.
pub(crate) fn suffix() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl Host {
    /// Opens the store in `config.home`, recovers from a previous process (running runs become
    /// `interrupted`, pending requests `cancelled`, terminals `exited`) and gathers machine info.
    pub async fn open(config: Config, providers: Vec<Arc<dyn Provider>>) -> anyhow::Result<Arc<Host>> {
        std::fs::create_dir_all(&config.home)?;
        let store = Store::open(&config.db_path())?;
        store.prune_receipts(now().saturating_sub(RECEIPT_DAYS * 86_400_000))?;
        let providers: BTreeMap<_, _> = providers.into_iter().map(|p| (p.id(), p)).collect();
        let info = machine_info(&providers).await;
        let (out, _) = broadcast::channel(8192);
        let (mcp, listener) = crate::mcp::Server::bind().await?;
        let (tasks, task_work) = tasks::Tasks::new();
        let host = Arc::new_cyclic(|me| Host {
            me: me.clone(),
            config,
            store,
            providers,
            out,
            info: std::sync::Mutex::new(info),
            threads: std::sync::Mutex::new(HashMap::new()),
            inflight: std::sync::Mutex::new(HashMap::new()),
            projects: AsyncMutex::new(()),
            terminals: std::sync::Mutex::new(HashMap::new()),
            mcp,
            tasks,
        });
        host.recover()?;
        host.mcp.start(Arc::downgrade(&host), listener);
        tasks::spawn_worker(Arc::downgrade(&host), task_work);
        Ok(host)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Where roll's orchestrator MCP server listens (`http://<loopback>:<port>/mcp`).
    pub fn mcp_url(&self) -> String {
        self.mcp.url()
    }

    fn recover(&self) -> anyhow::Result<()> {
        let at = now();
        let mut changed = Vec::new();
        for entity in self.store.list(EntityKind::Run)? {
            if let Entity::Run(mut run) = entity
                && !run.status.is_terminal()
            {
                run.status = RunStatus::Interrupted;
                run.completed_at = Some(at);
                run.error.get_or_insert_with(|| "the daemon restarted".into());
                changed.push(Entity::Run(run));
            }
        }
        for entity in self.store.list(EntityKind::Node)? {
            if let Entity::Node(mut node) = entity
                && matches!(node.status, NodeStatus::Running | NodeStatus::Waiting)
            {
                node.status = NodeStatus::Interrupted;
                node.completed_at = Some(at);
                changed.push(Entity::Node(node));
            }
        }
        for entity in self.store.list(EntityKind::Request)? {
            if let Entity::Request(mut request) = entity
                && request.status == RequestStatus::Pending
            {
                request.status = RequestStatus::Cancelled;
                request.resolved_at = Some(at);
                changed.push(Entity::Request(request));
            }
        }
        for entity in self.store.list(EntityKind::Thread)? {
            if let Entity::Thread(mut thread) = entity
                && (thread.status.is_busy() || thread.active_run.is_some() || thread.pending_requests > 0)
            {
                thread.status = ThreadStatus::Idle;
                thread.active_run = None;
                thread.pending_requests = 0;
                thread.activity = None;
                thread.updated_at = at;
                changed.push(Entity::Thread(thread));
            }
        }
        for entity in self.store.list(EntityKind::Terminal)? {
            if let Entity::Terminal(mut terminal) = entity
                && terminal.status == TerminalStatus::Running
            {
                terminal.status = TerminalStatus::Exited;
                changed.push(Entity::Terminal(terminal));
            }
        }
        self.store.put(changed, |_| {})?;
        Ok(())
    }

    /// Writes records and broadcasts them once they are durable.
    pub(crate) fn put(&self, entities: Vec<Entity>) -> anyhow::Result<()> {
        self.store.put(entities, |stamped| {
            for s in stamped {
                let _ = self.out.send(Outbound::Entity(Box::new(s.clone())));
            }
            self.tasks.observe(stamped);
        })?;
        Ok(())
    }

    pub(crate) fn delta(&self, delta: Delta) {
        let _ = self.out.send(Outbound::Delta(delta));
    }

    pub(crate) fn provider(&self, id: &ProviderId) -> Res<Arc<dyn Provider>> {
        self.providers.get(id).cloned().ok_or_else(|| Fail::Invalid(format!("no provider {} on this machine", id.0)))
    }

    pub(crate) fn project(&self, id: &ProjectId) -> Res<Project> {
        match self.store.get(EntityKind::Project, id.as_str())? {
            Some(Entity::Project(p)) => Ok(p),
            _ => Err(Fail::NotFound(format!("no project {id}"))),
        }
    }

    pub(crate) fn stored_thread(&self, id: &ThreadId) -> Res<Thread> {
        match self.store.get(EntityKind::Thread, id.as_str())? {
            Some(Entity::Thread(t)) => Ok(t),
            _ => Err(Fail::NotFound(format!("no thread {id}"))),
        }
    }

    pub(crate) fn runs_of(&self, thread: &ThreadId) -> anyhow::Result<Vec<Run>> {
        let mut runs: Vec<Run> = self
            .store
            .of_thread(thread, EntityKind::Run)?
            .into_iter()
            .filter_map(|e| if let Entity::Run(r) = e { Some(r) } else { None })
            .collect();
        runs.sort_by_key(|r| r.ordinal);
        Ok(runs)
    }

    pub(crate) fn nodes_of(&self, thread: &ThreadId) -> anyhow::Result<Vec<Node>> {
        Ok(self
            .store
            .of_thread(thread, EntityKind::Node)?
            .into_iter()
            .filter_map(|e| if let Entity::Node(n) = e { Some(n) } else { None })
            .collect())
    }

    pub(crate) fn checkpoints_of(&self, thread: &ThreadId) -> anyhow::Result<Vec<Checkpoint>> {
        let mut checkpoints: Vec<Checkpoint> = self
            .store
            .of_thread(thread, EntityKind::Checkpoint)?
            .into_iter()
            .filter_map(|e| if let Entity::Checkpoint(c) = e { Some(c) } else { None })
            .collect();
        checkpoints.sort_by_key(|c| c.ordinal);
        Ok(checkpoints)
    }

    /// The thread's runtime, loaded on first use. Lock it and check `gone`.
    pub(crate) async fn rt(&self, id: &ThreadId) -> Res<Arc<AsyncMutex<Rt>>> {
        if let Some(rt) = self.threads.lock().unwrap().get(id) {
            return Ok(rt.clone());
        }
        let thread = self.stored_thread(id)?;
        let private: Private = self.store.thread_state(id)?.unwrap_or_default();
        let git = crate::git::root(Path::new(&thread.cwd)).await.is_some();
        let rt = Rt { thread, private, git, live: None, active: None, generation: 0, gone: false };
        let mut threads = self.threads.lock().unwrap();
        Ok(threads.entry(id.clone()).or_insert_with(|| Arc::new(AsyncMutex::new(rt))).clone())
    }

    /// Locks a thread's runtime, refusing a deleted thread.
    pub(crate) async fn lock(&self, id: &ThreadId) -> Res<tokio::sync::OwnedMutexGuard<Rt>> {
        let rt = self.rt(id).await?.lock_owned().await;
        if rt.gone {
            return Err(Fail::NotFound(format!("no thread {id}")));
        }
        Ok(rt)
    }

    pub(crate) fn save_private(&self, rt: &Rt) -> anyhow::Result<()> {
        self.store.put_thread_state(&rt.thread.id, &rt.private)
    }

    async fn execute(&self, by: &str, command: Command, grant: Option<CodeGrant>) -> Res<Reply> {
        match command {
            Command::ProjectAdd { path, name, clone } => self.project_add(path, name, clone, grant).await,
            Command::ProjectRemove { project } => self.project_remove(&project).await,
            Command::ThreadCreate {
                project,
                provider,
                model,
                effort,
                runtime_mode,
                interaction_mode,
                worktree,
                title,
                prompt,
                attachments,
            } => {
                let spec = threads::NewThread {
                    project,
                    provider,
                    model,
                    effort,
                    runtime_mode,
                    interaction_mode,
                    worktree,
                    title,
                };
                self.thread_create(spec, prompt, attachments).await
            }
            Command::ThreadConfigure { thread, title, provider, model, effort, runtime_mode, interaction_mode } => {
                self.thread_configure(&thread, title, provider, model, effort, runtime_mode, interaction_mode).await
            }
            Command::ThreadFork { thread, at, worktree } => self.thread_fork(&thread, at, worktree).await,
            Command::ThreadArchive { thread } => self.thread_archive(&thread, true).await,
            Command::ThreadUnarchive { thread } => self.thread_archive(&thread, false).await,
            Command::ThreadDelete { thread } => {
                self.stop_tasks(&thread).await;
                self.thread_delete(&thread).await.map(|()| Reply::Ok)
            }
            Command::MessageSend { thread, text, attachments, mode } => {
                self.message_send(&thread, text, attachments, mode).await
            }
            Command::MessageUnqueue { thread, message } => self.message_unqueue(&thread, &message).await,
            Command::RunInterrupt { thread } => {
                // Like T3's Stop: the thread's delegated tasks stop with it.
                let reply = self.run_interrupt(&thread).await;
                self.stop_tasks(&thread).await;
                reply
            }
            Command::RequestRespond { request, response } => self.request_respond(by, &request, response).await,
            Command::CheckpointRollback { thread, checkpoint } => self.checkpoint_rollback(&thread, &checkpoint).await,
            Command::ThreadPublish { thread, message, land } => self.thread_publish(&thread, &message, land, grant).await,
            Command::TerminalOpen { project, thread, command, cols, rows } => {
                self.terminal_open(project, thread, command, cols, rows).await
            }
            Command::TerminalInput { terminal, data } => self.terminal_input(&terminal, &data).await,
            Command::TerminalResize { terminal, cols, rows } => self.terminal_resize(&terminal, cols, rows).await,
            Command::TerminalClose { terminal } => self.terminal_close(&terminal).await,
            Command::TerminalScrollback { terminal } => self.terminal_scrollback(&terminal).await,
            Command::ProviderRefresh {} => {
                let info = machine_info(&self.providers).await;
                *self.info.lock().unwrap() = info.clone();
                let _ = self.out.send(Outbound::Info(info));
                Ok(Reply::Ok)
            }
            Command::DiffGet { thread, run } => self.diff_get(&thread, run.as_ref()).await,
            Command::FsList { project, path } => self.fs_list(project.as_ref(), &path).await,
            Command::FsRead { project, path } => self.fs_read(&project, &path).await,
            Command::GitStatus { thread } => self.git_status(&thread).await,
        }
    }

    /// Adds a directory as a project; with `clone`, clones the roll code repo into it first
    /// (with the relay's `grant`), and the project records which repo it is.
    async fn project_add(&self, path: String, name: Option<String>, clone: Option<RepoRef>, grant: Option<CodeGrant>) -> Res<Reply> {
        let path = match &clone {
            Some(repo) => self.clone_target(&path, repo)?,
            None => crate::config::expand_tilde(path.trim()),
        };
        if !path.is_absolute() {
            return Err(Fail::Invalid("the path must be absolute".into()));
        }
        // Held across the clone too, so two adds of one directory can't race.
        let _guard = self.projects.lock().await;
        if let Some(repo) = &clone {
            let grant = grant
                .filter(|g| &g.repo == repo)
                .ok_or_else(|| Fail::Invalid(format!("cloning {}/{} needs access granted by the relay", repo.owner, repo.repo)))?;
            crate::git::clone_into(&grant, &path).await.map_err(|e| Fail::Invalid(format!("{e:#}")))?;
        }
        let path = tokio::fs::canonicalize(&path)
            .await
            .map_err(|e| Fail::Invalid(format!("{}: {e}", path.display())))?;
        if !tokio::fs::metadata(&path).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Err(Fail::Invalid(format!("{} is not a directory", path.display())));
        }
        let path_str = path.to_str().ok_or_else(|| Fail::Invalid("the path is not UTF-8".into()))?.to_string();
        for entity in self.store.list(EntityKind::Project)? {
            if let Entity::Project(mut p) = entity
                && p.path == path_str
            {
                // Cloned again where a plain project already was: it is that repo now.
                if clone.is_some() && p.repo != clone {
                    p.repo = clone;
                    p.git = crate::git::info(&path).await;
                    p.updated_at = now();
                    let id = p.id.0.clone();
                    self.put(vec![Entity::Project(p)])?;
                    return Ok(Reply::Created { id });
                }
                return Ok(Reply::Created { id: p.id.0 });
            }
        }
        let at = now();
        let name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| path_str.clone());
        let project = Project {
            id: ProjectId::from_suffix(&suffix()),
            name,
            path: path_str,
            repo: clone,
            git: crate::git::info(&path).await,
            created_at: at,
            updated_at: at,
        };
        let id = project.id.0.clone();
        self.put(vec![Entity::Project(project)])?;
        Ok(Reply::Created { id })
    }

    /// Where a roll code repo is cloned: an absolute `path` as given; otherwise under the clone
    /// directory, at `<owner>/<repo>` (an empty `path`) or the relative `path`.
    fn clone_target(&self, path: &str, repo: &RepoRef) -> Res<PathBuf> {
        let given = crate::config::expand_tilde(path.trim());
        if given.is_absolute() {
            return Ok(given);
        }
        let one_part = |s: &str| {
            let mut parts = Path::new(s).components();
            matches!((parts.next(), parts.next()), (Some(std::path::Component::Normal(_)), None))
        };
        if !one_part(&repo.owner) || !one_part(&repo.repo) {
            return Err(Fail::Invalid(format!("{}/{} isn't a repository name", repo.owner, repo.repo)));
        }
        if given.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(Fail::Invalid("a relative path stays inside the clone directory".into()));
        }
        let root = self.config.clone_dir().ok_or_else(|| Fail::Invalid("no home directory to clone into: give an absolute path".into()))?;
        Ok(if path.trim().is_empty() { root.join(&repo.owner).join(&repo.repo) } else { root.join(given) })
    }

    async fn project_remove(&self, id: &ProjectId) -> Res<Reply> {
        let _guard = self.projects.lock().await;
        let project = self.project(id)?;
        for entity in self.store.list(EntityKind::Thread)? {
            if let Entity::Thread(t) = entity
                && t.project == project.id
            {
                self.thread_delete(&t.id).await?;
            }
        }
        let _ = std::fs::remove_dir(self.config.worktrees().join(project.id.as_str()));
        let mut tombstones = self.close_terminals(|t| t.project.as_ref() == Some(&project.id))?;
        tombstones.push(Entity::Removed(Removed { entity: EntityKind::Project, id: project.id.0, thread: None }));
        self.put(tombstones)?;
        Ok(Reply::Ok)
    }

    fn gate(&self, id: &str) -> Arc<AsyncMutex<()>> {
        self.inflight.lock().unwrap().entry(id.to_string()).or_default().clone()
    }

    fn ungate(&self, id: &str, gate: &Arc<AsyncMutex<()>>) {
        let mut inflight = self.inflight.lock().unwrap();
        // Only the last holder (the map plus this one) removes it.
        if Arc::strong_count(gate) <= 2 {
            inflight.remove(id);
        }
    }
}

#[async_trait]
impl crate::Daemon for Host {
    async fn info(&self) -> MachineInfo {
        self.info.lock().unwrap().clone()
    }

    async fn since(&self, after: u64, limit: usize) -> anyhow::Result<Vec<Stamped>> {
        self.store.since(after, limit)
    }

    fn subscribe(&self) -> broadcast::Receiver<Outbound> {
        self.out.subscribe()
    }

    async fn run(&self, id: String, by: String, command: Command) -> Reply {
        self.run_granted(id, by, command, None).await
    }

    async fn run_granted(&self, id: String, by: String, command: Command, grant: Option<CodeGrant>) -> Reply {
        if command.is_query() {
            return self.execute(&by, command, grant).await.unwrap_or_else(Fail::into_reply);
        }
        let gate = self.gate(&id);
        let reply = {
            let _held = gate.lock().await;
            match self.store.receipt(&id) {
                Ok(Some(reply)) => reply,
                Ok(None) => {
                    let reply = self.execute(&by, command, grant).await.unwrap_or_else(Fail::into_reply);
                    if let Err(e) = self.store.put_receipt(&id, &reply, now()) {
                        tracing::warn!("storing the receipt of command {id}: {e:#}");
                    }
                    reply
                }
                Err(e) => Fail::Internal(e).into_reply(),
            }
        };
        self.ungate(&id, &gate);
        reply
    }
}

async fn machine_info(providers: &BTreeMap<ProviderId, Arc<dyn Provider>>) -> MachineInfo {
    let mut infos = Vec::with_capacity(providers.len());
    for provider in providers.values() {
        infos.push(provider.info().await);
    }
    MachineInfo {
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        home: crate::config::user_home().unwrap_or_else(|| PathBuf::from("/")).to_string_lossy().into_owned(),
        providers: infos,
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within it or
    // truncates, and we stop at the first NUL either way.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    if ok && end > 0 { String::from_utf8_lossy(&buf[..end]).into_owned() } else { "localhost".into() }
}

/// A thread title from a prompt (T3's `deriveThreadTitleSeed`: whitespace collapsed, 50
/// characters then `...`).
pub(crate) fn title_from(text: &str, attachments: &[Attachment]) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !collapsed.is_empty() {
        let chars: Vec<char> = collapsed.chars().collect();
        return if chars.len() <= 50 { collapsed } else { format!("{}...", chars[..50].iter().collect::<String>()) };
    }
    match attachments.len() {
        0 => "New thread".into(),
        1 => "Attached image".into(),
        _ => "Attached images".into(),
    }
}

/// How long a streaming node may go without a durable upsert.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(500);
