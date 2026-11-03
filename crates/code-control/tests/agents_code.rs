//! roll code agents working in roll code (docs/code/agents.md, "Agents in roll code"), end to
//! end: the real relay, a real `agent-host` daemon (on the fake provider) over the real link,
//! and real roll code repos served over git smart HTTP. Cloning repos as projects, starting a
//! repo as a project, publishing a thread as a change, landing it through the merge queue (a
//! blocked land is an answer), the change's status following onto the thread record, and a
//! thread taking a work item.

mod common;

use agent_host::Host;
use agent_host::config::Config as HostConfig;
use agent_host::provider::Provider;
use agent_host::provider::fake::FakeProvider;
use agent_host::relay::{LinkConfig, LinkEvent};
use agent_proto::{ChangeStatus, Entity, LandingStatus, Project, Reply, Thread};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{App, app_with, git};
use futures::StreamExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const ADMIN: &str = "admin-secret";
const APP: &str = "/v1/code/repos/acme/app";

struct World {
    a: App,
    owner: String,
    machine: String,
    git_url: String,
    clones: tempfile::TempDir,
    _home: tempfile::TempDir,
    _stop: oneshot::Sender<()>,
}

/// acme with an owner token and `acme/app` (a README on main; the merge queue, requiring `ci`),
/// served on a port; a daemon whose clone directory is a temp dir, connected as machine `box`.
async fn world() -> World {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let git_url = format!("{url}/git");
    let gu = git_url.clone();
    let a = app_with(move |c| {
        c.admin_token = Some(ADMIN.to_string());
        c.git_url = gu;
    });
    let router = a.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    a.code.agents.set_follow_every(Duration::from_millis(50));

    let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "me" })), Some(ADMIN)).await;
    let owner = t["token"].as_str().unwrap().to_string();
    let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app" })), Some(&owner)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = a.req("POST", &format!("{APP}/land"), Some(json!({ "message": "init", "files": { "README.md": { "content": "# app\n" } } })), Some(&owner)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = a.req("PATCH", APP, Some(json!({ "land_policy": "queue", "required_checks": ["ci"] })), Some(&owner)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    a.code.start_queues().unwrap();

    let (s, m) = a.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": "box" })), Some(&owner)).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let machine = m["machine"]["id"].as_str().unwrap().to_string();
    let token = m["token"].as_str().unwrap().to_string();

    let home = tempfile::tempdir().unwrap();
    let clones = tempfile::tempdir().unwrap();
    let config = HostConfig { home: home.path().to_path_buf(), clone_dir: Some(clones.path().to_path_buf()), ..HostConfig::default() };
    let providers: Vec<Arc<dyn Provider>> = vec![FakeProvider::new("codex")];
    let host = Host::open(config, providers).await.unwrap();
    let (tx, mut events) = mpsc::unbounded_channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let daemon: Arc<dyn agent_host::Daemon> = host;
    tokio::spawn(agent_host::relay::run(daemon, LinkConfig { api: url, token }, tx, async move {
        let _ = stopped.await;
    }));
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("the daemon connects") {
            Some(LinkEvent::Connected { .. }) => break,
            Some(_) => continue,
            None => panic!("the link stopped"),
        }
    }
    World { a, owner, machine, git_url, clones, _home: home, _stop: stop }
}

static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(&self.owner)).await
    }

    /// A command to the daemon, by the owner.
    async fn cmd(&self, command: Value) -> (StatusCode, Value) {
        let id = format!("c{}", IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
        self.req("POST", &format!("/v1/code/agents/acme/machines/{}/commands", self.machine), Some(json!({ "id": id, "command": command }))).await
    }

    async fn created(&self, command: Value) -> String {
        let (s, v) = self.cmd(command).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["type"], "created", "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    async fn entities(&self) -> Vec<Entity> {
        let (s, v) = self.req("GET", "/v1/code/agents/acme/state", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let state: agent_proto::State = serde_json::from_value(v).unwrap();
        state.entities.into_iter().map(|m| m.entity).collect()
    }

    /// A record the relay mirrored (5 s at most).
    async fn project(&self, id: &str) -> Project {
        for _ in 0..500 {
            if let Some(p) = self.entities().await.into_iter().find_map(|e| match e {
                Entity::Project(p) if p.id.0 == id => Some(p),
                _ => None,
            }) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("project {id} never reached the relay");
    }

    async fn thread(&self, id: &str) -> Thread {
        for _ in 0..500 {
            if let Some(t) = self.entities().await.into_iter().find_map(|e| match e {
                Entity::Thread(t) if t.id.0 == id => Some(t),
                _ => None,
            }) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("thread {id} never reached the relay");
    }

    async fn clone_app(&self) -> String {
        self.created(json!({ "type": "project.add", "path": "", "clone": { "owner": "acme", "repo": "app" } })).await
    }

    async fn publish(&self, thread: &str, message: &str, land: bool) -> agent_proto::Published {
        let (s, v) = self.cmd(json!({ "type": "thread.publish", "thread": thread, "message": message, "land": land })).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        match serde_json::from_value(v.clone()).unwrap() {
            Reply::Published(p) => p,
            other => panic!("expected published, got {other:?}"),
        }
    }
}

fn origin(dir: &Path) -> String {
    git(dir, dir, &["remote", "get-url", "origin"]).trim().to_string()
}

/// The owner's agents event stream, read for thread records.
struct Threads {
    body: axum::body::BodyDataStream,
    buf: String,
}

impl Threads {
    async fn open(w: &World) -> Threads {
        let req = Request::get("/v1/code/agents/acme/events").header("authorization", format!("Bearer {}", w.owner)).body(Body::empty()).unwrap();
        let resp = w.a.send(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        Threads { body: resp.into_body().into_data_stream(), buf: String::new() }
    }

    /// The next record of `id` that satisfies `f` (15 s at most): what a canvas sees live.
    async fn until(&mut self, id: &str, what: &str, f: impl Fn(&Thread) -> bool) -> Thread {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            while let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let Some(data) = block.lines().find_map(|l| l.strip_prefix("data:")) else { continue };
                if let Ok(agent_proto::StreamEvent::Entity(m)) = serde_json::from_str(data.trim())
                    && let Entity::Thread(t) = m.entity
                    && t.id.0 == id
                    && f(&t)
                {
                    return t;
                }
            }
            let chunk = tokio::time::timeout_at(deadline, self.body.next()).await.unwrap_or_else(|_| panic!("{what}: not seen within 15 s")).expect("stream open").unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_clone_onto_machines_and_new_repos_start_as_projects() {
    let w = world().await;
    let id = w.clone_app().await;
    let dir = w.clones.path().join("acme/app");
    assert_eq!(std::fs::read_to_string(dir.join("README.md")).unwrap(), "# app\n");
    assert_eq!(origin(&dir), format!("{}/acme/app.git", w.git_url), "the remote has no credentials in it");
    let config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
    assert!(!config.contains("rlg_") && !config.contains("Authorization"), "the grant is never written down: {config}");
    let p = w.project(&id).await;
    assert_eq!(p.repo, Some(agent_proto::RepoRef { owner: "acme".into(), repo: "app".into() }));
    assert_eq!(PathBuf::from(&p.path), std::fs::canonicalize(&dir).unwrap());
    // Cloning it again finds the checkout already there: the same project.
    assert_eq!(w.clone_app().await, id);
    // A relative path goes under the clone directory.
    let other = w.created(json!({ "type": "project.add", "path": "work/app-2", "clone": { "owner": "acme", "repo": "app" } })).await;
    assert_ne!(other, id);
    assert!(w.clones.path().join("work/app-2/README.md").exists());
    let (s, v) = w.cmd(json!({ "type": "project.add", "path": "../out", "clone": { "owner": "acme", "repo": "app" } })).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")), "{v}");
    // Only the owner's repos, and only ones that exist.
    let (s, v) = w.cmd(json!({ "type": "project.add", "path": "", "clone": { "owner": "acme", "repo": "nope" } })).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // A new repo, cloned as a project in one step.
    let path = format!("/v1/code/agents/acme/machines/{}/projects", w.machine);
    let body = json!({ "id": "start-fresh", "repo": { "name": "fresh", "visibility": "private", "description": "From the canvas" } });
    let (s, v) = w.req("POST", &path, Some(body.clone())).await;
    assert_eq!((s, v["type"].as_str()), (StatusCode::OK, Some("created")), "{v}");
    let fresh = v["id"].as_str().unwrap().to_string();
    let (s, repo) = w.req("GET", "/v1/code/repos/acme/fresh", None).await;
    assert_eq!((s, repo["visibility"].as_str(), repo["description"].as_str()), (StatusCode::OK, Some("private"), Some("From the canvas")));
    let p = w.project(&fresh).await;
    assert_eq!(p.repo, Some(agent_proto::RepoRef { owner: "acme".into(), repo: "fresh".into() }));
    assert_eq!(origin(Path::new(&p.path)), format!("{}/acme/fresh.git", w.git_url));
    // A retry (the repo exists now) answers the same.
    let (s, again) = w.req("POST", &path, Some(body)).await;
    assert_eq!((s, again["id"].as_str()), (StatusCode::OK, Some(fresh.as_str())), "{again}");
    let (s, v) = w.req("POST", &path, Some(json!({ "id": "x", "repo": { "name": "Bad Name!" } }))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn published_threads_become_changes_that_land_through_the_queue() {
    let w = world().await;
    let project = w.clone_app().await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "codex", "worktree": true, "title": "Add the feature" })).await;
    let t = w.thread(&thread).await;
    let worktree = t.worktree.clone().expect("a worktree");
    let mut stream = Threads::open(&w).await;

    // The agent's work, published: the branch becomes a change whose head is the commit.
    std::fs::write(Path::new(&worktree.path).join("feature.txt"), "done\n").unwrap();
    let p = w.publish(&thread, "Add the feature", false).await;
    assert_eq!(p.branch, worktree.branch);
    let n = p.change.expect("the branch's change");
    assert_eq!(p.url.as_deref(), Some(format!("https://roll.sh/acme/app/changes/{n}").as_str()));
    assert!(p.landing.is_none());
    let (_, c) = w.req("GET", &format!("{APP}/changes/{n}"), None).await;
    assert_eq!((c["branch"].as_str(), c["state"].as_str()), (Some(worktree.branch.as_str()), Some("open")), "{c}");
    let (_, cs) = w.req("GET", &format!("{APP}/changesets/{}", c["head"].as_str().unwrap()), None).await;
    assert_eq!(cs["git_commit"].as_str(), Some(p.commit.as_str()));
    let change = c["id"].as_str().unwrap().to_string();

    // The thread shows its change, and follows it: required checks not run yet.
    let t = stream.until(&thread, "blocked", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Blocked)).await;
    let tc = t.change.unwrap();
    assert_eq!((tc.repo.as_str(), tc.number), ("acme/app", n));
    assert!(tc.detail.as_deref().unwrap_or("").contains("ci"), "{tc:?}");

    // More work revises the same change.
    std::fs::write(Path::new(&worktree.path).join("more.txt"), "more\n").unwrap();
    let again = w.publish(&thread, "And more", false).await;
    assert_eq!(again.change, Some(n));
    let (_, c) = w.req("GET", &format!("{APP}/changes/{n}"), None).await;
    assert_eq!(c["revision"], 2, "{c}");

    // CI reports; the thread follows.
    let check = |status: &'static str| json!({ "name": "ci", "status": status });
    w.req("POST", &format!("/v1/code/changes/{change}/checks"), Some(check("running"))).await;
    stream.until(&thread, "checking", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Checking && c.detail.as_deref() == Some("ci"))).await;
    // Landing while ci runs is blocked: an answer, not an error.
    let blocked = w.publish(&thread, "And more", true).await;
    let landing = blocked.landing.expect("landing");
    assert_eq!(landing.status, LandingStatus::Blocked, "{landing:?}");
    assert!(landing.blockers.iter().any(|b| b.contains("ci")), "{landing:?}");
    w.req("POST", &format!("/v1/code/changes/{change}/checks"), Some(check("failure"))).await;
    stream.until(&thread, "failing", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Failing)).await;
    w.req("POST", &format!("/v1/code/changes/{change}/checks"), Some(check("success"))).await;
    stream.until(&thread, "ready", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Ready)).await;

    // Through the merge queue onto main.
    let queued = w.publish(&thread, "And more", true).await;
    let landing = queued.landing.expect("landing");
    assert!(matches!(landing.status, LandingStatus::Queued), "{landing:?}");
    let t = stream.until(&thread, "landed", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Landed)).await;
    assert_eq!(t.change.unwrap().number, n);
    let (s, b) = w.a.raw("GET", &format!("{APP}/blob/main/feature.txt"), None, Some(&w.owner)).await;
    assert_eq!((s, b.as_slice()), (StatusCode::OK, b"done\n".as_slice()));
    let (_, c) = w.req("GET", &format!("{APP}/changes/{n}"), None).await;
    assert_eq!(c["state"], "landed");

    // The daemon's own updates of the thread keep what roll code knows about it.
    let (s, v) = w.cmd(json!({ "type": "thread.configure", "thread": thread, "title": "Renamed" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let t = stream.until(&thread, "renamed", |t| t.title == "Renamed").await;
    assert_eq!(t.change.map(|c| c.status), Some(ChangeStatus::Landed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threads_take_work_items() {
    let w = world().await;
    let project = w.clone_app().await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "codex" })).await;
    w.thread(&thread).await;
    let (s, item) = w.req("POST", &format!("{APP}/work"), Some(json!({ "title": "Fix the login bug", "priority": 0 }))).await;
    assert_eq!(s, StatusCode::CREATED, "{item}");
    let mut stream = Threads::open(&w).await;
    let path = format!("/v1/code/agents/acme/threads/{thread}/work");

    // Claimed for the thread: the next open item of the repo.
    let (s, v) = w.req("POST", &path, Some(json!({ "repo": "acme/app" }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["id"].as_str(), v["state"].as_str(), v["claimed_by"].as_str()), (item["id"].as_str(), Some("claimed"), Some("agent:box")));
    let t = stream.until(&thread, "work", |t| t.work.is_some()).await;
    let work = t.work.unwrap();
    assert_eq!((work.id.as_str(), work.repo.as_str(), work.number, work.title.as_str()), (item["id"].as_str().unwrap(), "acme/app", item["number"].as_u64().unwrap(), "Fix the login bug"));
    // Nothing else to claim.
    assert_eq!(w.req("POST", &path, Some(json!({ "repo": "acme/app" }))).await.0, StatusCode::NO_CONTENT);
    // Let go, then take a given item.
    assert_eq!(w.req("DELETE", &path, None).await.0, StatusCode::NO_CONTENT);
    stream.until(&thread, "no work", |t| t.work.is_none()).await;
    let (s, v) = w.req("POST", &path, Some(json!({ "work": format!("acme/app#{}", item["number"]) }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    stream.until(&thread, "work again", |t| t.work.is_some()).await;
    assert_eq!(w.req("POST", &path, Some(json!({}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(w.req("POST", "/v1/code/agents/acme/threads/thr_nope/work", Some(json!({ "repo": "acme/app" }))).await.0, StatusCode::NOT_FOUND);
    // A thread record the daemon sends again keeps its work item.
    let (s, v) = w.cmd(json!({ "type": "thread.configure", "thread": thread, "title": "Login" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let t = stream.until(&thread, "renamed", |t| t.title == "Login").await;
    assert!(t.work.is_some());
}
