#![allow(dead_code)]

use async_trait::async_trait;
use code_git::{
    Access, Credentials, FetchPlan, FetchRequest, GitBackend, GitError, GitResult, Object, ObjectId, ReceiveOutcome,
    ReceiveRequest, Ref, RefList, RefStatus, ServerConfig, parse_tag,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct MemRepo {
    pub objects: HashMap<ObjectId, Object>,
    pub refs: BTreeMap<String, ObjectId>,
    pub protect_main: bool,
    pub password: Option<String>,
}

/// An in-memory backend: repos by name, last fetch plan sizes, pushes.
#[derive(Default)]
pub struct Mem {
    pub repos: Mutex<HashMap<String, MemRepo>>,
    pub plans: Mutex<Vec<usize>>,
    /// Each push's options and pusher.
    pub pushes: Mutex<Vec<(Vec<String>, Option<String>)>>,
}

impl Mem {
    pub fn with_repo(name: &str) -> Arc<Self> {
        let m = Arc::new(Mem::default());
        m.add_repo(name);
        m
    }

    pub fn add_repo(&self, name: &str) {
        self.repos.lock().unwrap().insert(name.into(), MemRepo::default());
    }

    pub fn update<R>(&self, name: &str, f: impl FnOnce(&mut MemRepo) -> R) -> R {
        f(self.repos.lock().unwrap().get_mut(name).unwrap())
    }

    pub fn ref_of(&self, repo: &str, name: &str) -> Option<ObjectId> {
        self.update(repo, |r| r.refs.get(name).copied())
    }

    pub fn last_plan(&self) -> usize {
        *self.plans.lock().unwrap().last().unwrap()
    }
}

#[async_trait]
impl GitBackend for Mem {
    async fn authorize(&self, repo: &str, credentials: Option<&Credentials>, _access: Access) -> GitResult<Option<String>> {
        let repos = self.repos.lock().unwrap();
        let r = repos.get(repo).ok_or_else(|| GitError::NotFound(repo.into()))?;
        match (&r.password, credentials) {
            (None, c) => Ok(c.map(|c| c.username.clone())),
            (Some(p), Some(c)) if &c.password == p => Ok(Some(c.username.clone())),
            _ => Err(GitError::Unauthorized),
        }
    }

    async fn refs(&self, repo: &str) -> GitResult<RefList> {
        let repos = self.repos.lock().unwrap();
        let r = repos.get(repo).ok_or_else(|| GitError::NotFound(repo.into()))?;
        let refs = r
            .refs
            .iter()
            .map(|(name, oid)| {
                let peeled = r.objects.get(oid).filter(|o| o.kind == code_git::Kind::Tag).and_then(|o| parse_tag(&o.data).ok()).map(|t| t.0);
                Ref { name: name.clone(), oid: *oid, peeled }
            })
            .collect();
        Ok(RefList { head: Some("refs/heads/main".into()), refs })
    }

    async fn read_object(&self, repo: &str, oid: &ObjectId) -> GitResult<Option<Object>> {
        Ok(self.repos.lock().unwrap().get(repo).and_then(|r| r.objects.get(oid).cloned()))
    }

    async fn plan_fetch(&self, repo: &str, req: &FetchRequest) -> GitResult<FetchPlan> {
        let plan = code_git::walk::plan_fetch(self, repo, req).await?;
        self.plans.lock().unwrap().push(plan.objects.len());
        Ok(plan)
    }

    async fn receive(&self, repo: &str, req: ReceiveRequest) -> GitResult<ReceiveOutcome> {
        self.pushes.lock().unwrap().push((req.options.clone(), req.pusher.clone()));
        let mut repos = self.repos.lock().unwrap();
        let r = repos.get_mut(repo).unwrap();
        for o in req.objects {
            r.objects.insert(o.id, o);
        }
        let mut out = ReceiveOutcome::default();
        let check = |r: &MemRepo, u: &code_git::RefUpdate| -> Result<(), String> {
            if r.protect_main && u.name == "refs/heads/main" {
                return Err("conflict: src/lib.rs changed on main since your base".into());
            }
            let cur = r.refs.get(&u.name).copied().unwrap_or(ObjectId::ZERO);
            if cur != u.old {
                return Err("stale old value".into());
            }
            if !u.is_delete() && !r.objects.contains_key(&u.new) {
                return Err("missing objects".into());
            }
            Ok(())
        };
        let results: Vec<_> = req.updates.iter().map(|u| check(r, u)).collect();
        let all_ok = results.iter().all(Result::is_ok);
        for (u, res) in req.updates.iter().zip(results) {
            let res = if req.atomic && !all_ok && res.is_ok() { Err("atomic push failed".into()) } else { res };
            if res.is_ok() {
                if u.is_delete() {
                    r.refs.remove(&u.name);
                } else {
                    r.refs.insert(u.name.clone(), u.new);
                }
            } else if r.protect_main {
                out.messages.push("roll: your change conflicts with main\n  src/lib.rs".into());
            }
            out.refs.push(RefStatus { name: u.name.clone(), result: res });
        }
        Ok(out)
    }
}

/// Serves `backend` on a free port; returns `http://127.0.0.1:<port>/git`.
pub async fn serve(backend: Arc<Mem>) -> String {
    serve_with(backend, ServerConfig::default()).await
}

pub async fn serve_with(backend: Arc<Mem>, config: ServerConfig) -> String {
    let app = code_git::router(backend, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/git")
}

/// Runs git with an isolated config; returns the output whether or not it succeeded.
pub fn git_raw(home: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "-c", "user.name=Agent", "-c", "user.email=agent@roll.sh", "-c", "advice.detachedHead=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env_remove("GIT_DIR")
        .output()
        .expect("run git")
}

/// Runs git and panics with its output if it fails; returns stdout.
pub fn git(home: &Path, dir: &Path, args: &[&str]) -> String {
    let out = git_raw(home, dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
