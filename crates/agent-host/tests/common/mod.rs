//! A daemon on fake providers in a temp home, and ways to wait for its records.

#![allow(dead_code)]

use agent_host::config::Config;
use agent_host::provider::Provider;
use agent_host::provider::fake::FakeProvider;
use agent_host::{Daemon, Host};
use agent_proto::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct World {
    pub host: Arc<Host>,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub project: ProjectId,
    pub dirs: Vec<tempfile::TempDir>,
}

static IDS: AtomicU64 = AtomicU64::new(0);

pub fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Process::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// A git repository with one commit.
pub fn git_repo(dir: &Path) {
    sh(dir, &["init", "-q", "-b", "main"]);
    sh(dir, &["config", "user.name", "Test"]);
    sh(dir, &["config", "user.email", "test@example.test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    sh(dir, &["add", "."]);
    sh(dir, &["commit", "-qm", "init"]);
}

pub async fn open(home: &Path, providers: Vec<Arc<FakeProvider>>) -> Arc<Host> {
    let config = Config::load_from(home.to_path_buf()).unwrap();
    let providers: Vec<Arc<dyn Provider>> = providers.into_iter().map(|p| p as Arc<dyn Provider>).collect();
    Host::open(config, providers).await.unwrap()
}

impl World {
    /// A daemon with a git project.
    pub async fn new(providers: Vec<Arc<FakeProvider>>) -> World {
        World::with_repo(providers, true).await
    }

    pub async fn with_repo(providers: Vec<Arc<FakeProvider>>, git: bool) -> World {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        if git {
            git_repo(repo.path());
        }
        let host = open(home.path(), providers).await;
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        let Reply::Created { id } = run(&host, Command::ProjectAdd {
            path: repo_path.to_string_lossy().into(),
            name: None,
            clone: None,
        })
        .await
        else {
            panic!("project.add failed")
        };
        World {
            host,
            home: home.path().to_path_buf(),
            repo: repo_path,
            project: ProjectId(id),
            dirs: vec![home, repo],
        }
    }

    pub async fn run(&self, command: Command) -> Reply {
        run(&self.host, command).await
    }

    /// `thread.create` on the world's project; returns the thread id.
    pub async fn thread(&self, provider: &str, prompt: Option<&str>, worktree: bool) -> ThreadId {
        let reply = self
            .run(Command::ThreadCreate {
                project: self.project.clone(),
                provider: ProviderId(provider.into()),
                model: None,
                effort: None,
                runtime_mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Default,
                worktree,
                title: None,
                prompt: prompt.map(String::from),
                attachments: Vec::new(),
                orchestrator: false,
            })
            .await;
        let Reply::Created { id } = reply else { panic!("thread.create: {reply:?}") };
        ThreadId(id)
    }

    pub async fn send(&self, thread: &ThreadId, text: &str, mode: SendMode) -> Reply {
        self.run(Command::MessageSend { thread: thread.clone(), text: text.into(), attachments: Vec::new(), mode }).await
    }

    pub async fn state(&self) -> Records {
        state(&self.host).await
    }

    /// Waits up to 5 s for the thread to satisfy `pred`.
    pub async fn wait_thread(&self, id: &ThreadId, pred: impl Fn(&Thread) -> bool) -> Thread {
        wait(&self.host, |s| s.thread(id).filter(|t| pred(t)).cloned(), "the thread").await
    }

    pub async fn wait<T>(&self, what: &str, f: impl Fn(&Records) -> Option<T>) -> T {
        wait(&self.host, f, what).await
    }
}

pub async fn run(host: &Arc<Host>, command: Command) -> Reply {
    let id = format!("cmd-{}", IDS.fetch_add(1, Ordering::SeqCst));
    host.run(id, "owner".into(), command).await
}

/// Every live record, by `(kind, id)`, and the raw replay.
pub struct Records {
    pub replay: Vec<Stamped>,
    pub live: HashMap<(EntityKind, String), Entity>,
}

impl Records {
    pub fn thread(&self, id: &ThreadId) -> Option<&Thread> {
        match self.live.get(&(EntityKind::Thread, id.0.clone())) {
            Some(Entity::Thread(t)) => Some(t),
            _ => None,
        }
    }

    pub fn runs(&self, thread: &ThreadId) -> Vec<Run> {
        let mut runs: Vec<Run> = self
            .live
            .values()
            .filter_map(|e| match e {
                Entity::Run(r) if &r.thread == thread => Some(r.clone()),
                _ => None,
            })
            .collect();
        runs.sort_by_key(|r| r.ordinal);
        runs
    }

    pub fn nodes(&self, thread: &ThreadId) -> Vec<Node> {
        let mut nodes: Vec<Node> = self
            .live
            .values()
            .filter_map(|e| match e {
                Entity::Node(n) if &n.thread == thread => Some(n.clone()),
                _ => None,
            })
            .collect();
        nodes.sort_by_key(|n| (n.run.0.clone(), n.ordinal));
        nodes
    }

    pub fn requests(&self, thread: &ThreadId) -> Vec<Request> {
        self.live
            .values()
            .filter_map(|e| match e {
                Entity::Request(r) if &r.thread == thread => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn checkpoints(&self, thread: &ThreadId) -> Vec<Checkpoint> {
        let mut cps: Vec<Checkpoint> = self
            .live
            .values()
            .filter_map(|e| match e {
                Entity::Checkpoint(c) if &c.thread == thread => Some(c.clone()),
                _ => None,
            })
            .collect();
        cps.sort_by_key(|c| c.ordinal);
        cps
    }
}

pub async fn state(host: &Arc<Host>) -> Records {
    let replay = host.since(0, usize::MAX).await.unwrap();
    let live = replay
        .iter()
        .filter(|s| !matches!(s.entity, Entity::Removed(_)))
        .map(|s| ((s.entity.kind(), s.entity.id().to_string()), s.entity.clone()))
        .collect();
    Records { replay, live }
}

pub async fn wait<T>(host: &Arc<Host>, f: impl Fn(&Records) -> Option<T>, what: &str) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f(&state(host).await) {
            return v;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
