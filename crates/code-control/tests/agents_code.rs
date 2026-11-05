//! roll code agents working in roll code (docs/code/agents.md, "Agents in roll code"), end to
//! end: the real relay, a real `agent-host` daemon (on the fake provider) over the real link,
//! and real roll code repos served over git smart HTTP. Cloning repos as projects, starting a
//! repo as a project, publishing a thread as a change, landing it through the merge queue (a
//! blocked land is an answer), the change's status following onto the thread record, and a
//! thread taking a work item: its claim kept alive while the thread works, and done when the
//! published change lands.

mod common;

use agent_host::Host;
use agent_host::config::Config as HostConfig;
use agent_host::provider::Provider;
use agent_host::provider::fake::{FakeProvider, Script};
use agent_host::provider::{ProviderEvent, TurnEnd};
use agent_host::relay::{LinkConfig, LinkEvent};
use agent_proto::{Capabilities, ChangeStatus, Entity, LandingStatus, Project, Reply, Settle, SettledBy, Thread, ThreadStatus};
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
    /// Provider `claude`: its turns run until the test ends them.
    manual: Arc<FakeProvider>,
    _home: tempfile::TempDir,
    /// Dropping it stops the daemon's link.
    stop: Option<oneshot::Sender<()>>,
}

/// acme with an owner token and `acme/app` (a README on main; the merge queue, requiring `ci`),
/// served on a port; a daemon whose clone directory is a temp dir, connected as machine `box`,
/// with an echoing `codex` and a `claude` whose turns the test ends.
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
    let manual = FakeProvider::with("claude", Capabilities::default(), Script::Manual);
    let providers: Vec<Arc<dyn Provider>> = vec![FakeProvider::new("codex"), manual.clone()];
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
    World { a, owner, machine, git_url, clones, manual, _home: home, stop: Some(stop) }
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
    // It starts with one commit (a README), so a thread can work in a worktree and publish.
    assert_eq!(std::fs::read_to_string(Path::new(&p.path).join("README.md")).unwrap(), "# fresh\n\nFrom the canvas\n");
    let thread = w.created(json!({ "type": "thread.create", "project": fresh, "provider": "codex", "worktree": true, "title": "First feature" })).await;
    let worktree = w.thread(&thread).await.worktree.expect("a worktree");
    std::fs::write(Path::new(&worktree.path).join("hello.txt"), "hi\n").unwrap();
    assert!(w.publish(&thread, "First feature", false).await.change.is_some());
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
    // The relay tells the daemon, which settles the thread (its project's auto-settle).
    let t = stream.until(&thread, "settled", |t| matches!(t.inbox.settle, Some(Settle::Settled { by: SettledBy::Landed, .. }))).await;
    assert_eq!(t.change.map(|c| c.status), Some(ChangeStatus::Landed));

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

/// A work item as the API shows it now.
async fn work_item(w: &World, id: &str) -> Value {
    let (s, v) = w.req("GET", &format!("/v1/code/work/{id}"), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}

/// Waits (`secs` at most) until the work item satisfies `f`.
async fn work_until(w: &World, id: &str, secs: u64, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let v = work_item(w, id).await;
        if f(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_working_threads_claim_stays_alive_and_lapses_when_it_stops() {
    let mut w = world().await;
    w.a.code.agents.set_work_idle(Duration::from_millis(300));
    let project = w.clone_app().await;
    let mut stream = Threads::open(&w).await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "claude", "prompt": "Fix the login bug" })).await;
    stream.until(&thread, "running", |t| t.status == ThreadStatus::Running).await;
    let path = format!("/v1/code/agents/acme/threads/{thread}/work");
    let claim = || json!({ "repo": "acme/app", "lease_s": 2 });

    // Claimed with a 2 s lease while the thread runs: it outlives its lease, renewed.
    let (_, item) = w.req("POST", &format!("{APP}/work"), Some(json!({ "title": "Fix the login bug" }))).await;
    let id = item["id"].as_str().unwrap().to_string();
    let (s, v) = w.req("POST", &path, Some(claim())).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("claimed")), "{v}");
    let first = v["lease_until"].as_i64().unwrap();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let v = work_item(&w, &id).await;
    assert_eq!((v["state"].as_str(), v["claimed_by"].as_str()), (Some("claimed"), Some("agent:box")), "{v}");
    assert!(v["lease_until"].as_i64().unwrap() > first + 1, "renewed: {v}");

    // The run ends and the thread sits idle past the grace: the lease runs out as any other.
    w.manual.session(0).await.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    stream.until(&thread, "idle", |t| t.status == ThreadStatus::Idle).await;
    work_until(&w, &id, 8, "lapsed once idle", |v| v["state"] == "open").await;

    // Claimed again by a working thread whose machine goes offline: it lapses too.
    let (s, v) = w.cmd(json!({ "type": "message.send", "thread": thread, "text": "Once more", "mode": "queue" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    stream.until(&thread, "running again", |t| t.status == ThreadStatus::Running).await;
    let (s, v) = w.req("POST", &path, Some(claim())).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("claimed")), "{v}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(work_item(&w, &id).await["state"], "claimed", "still kept alive");
    drop(w.stop.take());
    for _ in 0..500 {
        if !w.a.code.agents.online(&w.machine) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!w.a.code.agents.online(&w.machine));
    work_until(&w, &id, 8, "lapsed once offline", |v| v["state"] == "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publishing_does_the_threads_work_item() {
    let w = world().await;
    let project = w.clone_app().await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "codex", "worktree": true, "title": "Fix the login bug" })).await;
    let worktree = w.thread(&thread).await.worktree.expect("a worktree");
    let (_, item) = w.req("POST", &format!("{APP}/work"), Some(json!({ "title": "Fix the login bug" }))).await;
    let id = item["id"].as_str().unwrap().to_string();
    let mut stream = Threads::open(&w).await;
    let (s, v) = w.req("POST", &format!("/v1/code/agents/acme/threads/{thread}/work"), Some(json!({ "repo": "acme/app" }))).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("claimed")), "{v}");
    stream.until(&thread, "work", |t| t.work.is_some()).await;

    // Published: the change does the item, which goes to review.
    std::fs::write(Path::new(&worktree.path).join("login.txt"), "fixed\n").unwrap();
    let n = w.publish(&thread, "Fix the login bug", false).await.change.expect("a change");
    let (_, c) = w.req("GET", &format!("{APP}/changes/{n}"), None).await;
    assert_eq!(c["work_item"].as_str(), Some(id.as_str()), "{c}");
    let change = c["id"].as_str().unwrap().to_string();
    let v = work_item(&w, &id).await;
    assert_eq!(v["state"], "review", "{v}");
    assert_eq!(v["changes"], json!([change]), "{v}");

    // The link is roll code's own: another item, or one that doesn't exist, is refused.
    let (_, other) = w.req("POST", &format!("{APP}/work"), Some(json!({ "title": "Something else" }))).await;
    let (s, v) = w.req("PATCH", &format!("/v1/code/changes/{change}"), Some(json!({ "work_item": other["id"] }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = w.req("PATCH", &format!("/v1/code/changes/{change}"), Some(json!({ "work_item": "wi_nope" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = w.req("PATCH", &format!("/v1/code/changes/{change}"), Some(json!({ "work_item": 7 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = w.req("PATCH", &format!("/v1/code/changes/{change}"), Some(json!({ "work_item": id }))).await;
    assert_eq!((s, v["work_item"].as_str()), (StatusCode::OK, Some(id.as_str())), "the same item again: {v}");

    // Landing the change (through the queue, once ci passes) completes the item.
    w.req("POST", &format!("/v1/code/changes/{change}/checks"), Some(json!({ "name": "ci", "status": "success" }))).await;
    stream.until(&thread, "ready", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Ready)).await;
    let landing = w.publish(&thread, "Fix the login bug", true).await.landing.expect("landing");
    assert_eq!(landing.status, LandingStatus::Queued, "{landing:?}");
    stream.until(&thread, "landed", |t| t.change.as_ref().is_some_and(|c| c.status == ChangeStatus::Landed)).await;
    work_until(&w, &id, 10, "done once landed", |v| v["state"] == "done").await;
    let (s, v) = w.req("PATCH", &format!("/v1/code/changes/{change}"), Some(json!({ "work_item": other["id"] }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "a landed change: {v}");
}

impl World {
    /// The project's mirrored record once it satisfies `f` (15 s at most).
    async fn project_until(&self, id: &str, what: &str, f: impl Fn(&Project) -> bool) -> Project {
        for _ in 0..1500 {
            let found = self.entities().await.into_iter().find_map(|e| match e {
                Entity::Project(p) if p.id.0 == id && f(&p) => Some(p),
                _ => None,
            });
            if let Some(p) = found {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what}: not seen within 15 s");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_code_projects_with_auto_pull_follow_their_default_branch() {
    use agent_proto::{PullOutcome, PullSkip};
    let w = world().await;
    w.a.code.agents.set_pull_every(Duration::from_millis(100));
    // A repo that lands straight away (no queue).
    let lib = "/v1/code/repos/acme/lib";
    let (s, v) = w.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "lib" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let land = |file: &str| json!({ "message": format!("Add {file}"), "files": { file: { "content": format!("{file}\n") } } });
    let (s, v) = w.req("POST", &format!("{lib}/land"), Some(land("README.md"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = w.created(json!({ "type": "project.add", "path": "", "clone": { "owner": "acme", "repo": "lib" } })).await;
    let dir = w.clones.path().join("acme/lib");
    let head = |dir: &Path| git(dir, dir, &["rev-parse", "HEAD"]).trim().to_string();

    // Off: a landing leaves the checkout alone.
    let (s, v) = w.req("POST", &format!("{lib}/land"), Some(land("early.txt"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!dir.join("early.txt").exists());

    // Turning it on pulls at once, with the relay's grant.
    let (s, v) = w.cmd(json!({ "type": "project.update", "project": id, "settings": { "auto_pull": true } })).await;
    assert_eq!((s, v["type"].as_str()), (StatusCode::OK, Some("ok")), "{v}");
    w.project_until(&id, "the first pull", |p| matches!(p.pull.as_ref().map(|r| &r.outcome), Some(PullOutcome::Pulled { .. }))).await;
    assert!(dir.join("early.txt").exists());

    // A change landing on main comes down by itself.
    let (s, v) = w.req("POST", &format!("{lib}/land"), Some(land("feature.txt"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let commit = v["git_commit"].as_str().expect("the landing's commit").to_string();
    let p = w.project_until(&id, "the landing pulled", |p| p.git.as_ref().and_then(|g| g.head.as_deref()) == Some(commit.as_str())).await;
    assert!(matches!(p.pull.map(|r| r.outcome), Some(PullOutcome::Pulled { commits: 1, .. })));
    assert_eq!(head(&dir), commit);
    assert_eq!(std::fs::read_to_string(dir.join("feature.txt")).unwrap(), "feature.txt\n");
    let config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
    assert!(!config.contains("rlg_") && !config.contains("Authorization"), "the grant is never written down: {config}");

    // Local changes: the next landing is skipped, and the files stay as they are.
    std::fs::write(dir.join("README.md"), "mine\n").unwrap();
    let (s, v) = w.req("POST", &format!("{lib}/land"), Some(land("later.txt"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let later = v["git_commit"].as_str().expect("the landing's commit").to_string();
    w.project_until(&id, "the skip", |p| matches!(p.pull.as_ref().map(|r| &r.outcome), Some(PullOutcome::Skipped { reason: PullSkip::LocalChanges, .. }))).await;
    assert_eq!(head(&dir), commit);
    assert_eq!(std::fs::read_to_string(dir.join("README.md")).unwrap(), "mine\n");
    assert!(!dir.join("later.txt").exists());

    // Asked by hand once it's clean: the answer is what the pull did (an ask still queued behind
    // the skipped one may have pulled it first).
    git(&dir, &dir, &["checkout", "-q", "--", "README.md"]);
    let (s, v) = w.cmd(json!({ "type": "project.pull", "project": id })).await;
    assert_eq!((s, v["type"].as_str()), (StatusCode::OK, Some("pulled")), "{v}");
    assert!(matches!(v["outcome"]["status"].as_str(), Some("pulled" | "up_to_date")), "{v}");
    assert_eq!(head(&dir), later);
    assert!(dir.join("later.txt").exists());

    // Agents can't change a project's settings or pull it.
    let (s, t) = w.a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent:bot" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{t}");
    let agent = t["token"].as_str().unwrap();
    let path = format!("/v1/code/agents/acme/machines/{}/commands", w.machine);
    for command in [json!({ "type": "project.pull", "project": id }), json!({ "type": "project.update", "project": id, "settings": {} })] {
        let (s, v) = w.a.req("POST", &path, Some(json!({ "id": "agent-1", "command": command })), Some(agent)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    }
}

impl World {
    /// A new repo with no commits (roll's own new-repo path lands a README; this one doesn't).
    async fn empty_repo(&self, name: &str) {
        let (s, v) = self.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name }))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }

    /// `acme/<name>` cloned at `path` (under the clone directory) while it has no commits.
    async fn clone_unborn(&self, name: &str, path: &str) -> (String, Project) {
        let id = self.created(json!({ "type": "project.add", "path": path, "clone": { "owner": "acme", "repo": name } })).await;
        let p = self.project(&id).await;
        assert_eq!(p.git.as_ref().map(|g| g.head.is_none()), Some(true), "{p:?}");
        (id, p)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_publish_of_an_empty_repo_starts_its_default_branch() {
    let w = world().await;
    w.empty_repo("blank").await;
    let (project, p) = w.clone_unborn("blank", "").await;
    let (other, _) = w.clone_unborn("blank", "work/blank-2").await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "codex", "worktree": true, "title": "Start the app" })).await;
    let t = w.thread(&thread).await;
    assert!(t.worktree.is_none(), "no commit to make a worktree from: it works in the folder");
    std::fs::write(Path::new(&p.path).join("app.txt"), "the app\n").unwrap();

    // Publishing it is the repo's first commit, on its default branch: it lands as it's pushed.
    let published = w.publish(&thread, "Start the app", false).await;
    assert_eq!((published.branch.as_str(), published.change), ("main", None));
    let landing = published.landing.expect("landing");
    assert_eq!(landing.status, LandingStatus::Landed, "{landing:?}");
    let (_, repo) = w.req("GET", "/v1/code/repos/acme/blank", None).await;
    assert_eq!(repo["head_git"].as_str(), Some(published.commit.as_str()), "the same commit id: the folder's main is the repo's");
    let (s, b) = w.a.raw("GET", "/v1/code/repos/acme/blank/blob/main/app.txt", None, Some(&w.owner)).await;
    assert_eq!((s, b.as_slice()), (StatusCode::OK, b"the app\n".as_slice()));
    let dir = Path::new(&p.path);
    assert_eq!(git(dir, dir, &["symbolic-ref", "--short", "HEAD"]).trim(), "main");

    // The other clone's thread is too late: the repo has a commit now, and the relay says so (a
    // grant only pushes main while main is absent).
    let late = w.created(json!({ "type": "thread.create", "project": other, "provider": "codex", "worktree": true, "title": "Me too" })).await;
    w.thread(&late).await;
    std::fs::write(w.clones.path().join("work/blank-2/other.txt"), "x\n").unwrap();
    let (s, v) = w.cmd(json!({ "type": "thread.publish", "thread": late, "message": "Me too", "land": false })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap_or("").contains("has commits now"), "{v}");
    let (_, repo) = w.req("GET", "/v1/code/repos/acme/blank", None).await;
    assert_eq!(repo["head_git"].as_str(), Some(published.commit.as_str()));

    // A repo where nothing lands on main directly (it requires reviews) takes its first commit as a
    // change on a branch of its own, which the thread follows; landing it waits for the review.
    w.empty_repo("reviewed").await;
    let (s, v) = w.req("PATCH", "/v1/code/repos/acme/reviewed/review-settings", Some(json!({ "required_approvals": 1 }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (project, p) = w.clone_unborn("reviewed", "").await;
    let thread = w.created(json!({ "type": "thread.create", "project": project, "provider": "codex", "worktree": true, "title": "Start it" })).await;
    w.thread(&thread).await;
    std::fs::write(Path::new(&p.path).join("a.txt"), "a\n").unwrap();
    let mut stream = Threads::open(&w).await;
    let published = w.publish(&thread, "Start it", true).await;
    assert_eq!(published.branch, "roll/first-commit");
    let landing = published.landing.expect("landing");
    assert_eq!(landing.status, LandingStatus::Blocked, "{landing:?}");
    let n = published.change.expect("a change");
    let (_, c) = w.req("GET", &format!("/v1/code/repos/acme/reviewed/changes/{n}"), None).await;
    assert_eq!((c["state"].as_str(), c["branch"].as_str()), (Some("open"), Some("roll/first-commit")), "{c}");
    let t = stream.until(&thread, "linked", |t| t.change.is_some()).await;
    assert_eq!(t.change.map(|c| c.number), Some(n));
    let (_, repo) = w.req("GET", "/v1/code/repos/acme/reviewed", None).await;
    assert!(repo["head_git"].is_null(), "{repo}");
}
