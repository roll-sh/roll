//! roll's orchestrator MCP end to end: fake providers stand in for the agents, and the test
//! drives the real Streamable HTTP server with the token each provider session was given
//! (initialize → tools/list → tools/call), the way Codex or Claude would.

mod common;

use agent_host::provider::fake::{Call, FAIL, FAILED, FakeProvider, FakeSession, Script};
use agent_host::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::*;
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// An agent that only does what the test tells it to.
fn manual(id: &str) -> Arc<FakeProvider> {
    FakeProvider::with(id, Capabilities::default(), Script::Manual)
}

/// A daemon with `config.toml` and a git project.
async fn world(providers: Vec<Arc<FakeProvider>>, config: &str) -> World {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    git_repo(repo.path());
    let host = open(home.path(), providers).await;
    let repo_path = std::fs::canonicalize(repo.path()).unwrap();
    let Reply::Created { id } =
        run(&host, Command::ProjectAdd { path: repo_path.to_string_lossy().into(), name: None, clone: None }).await
    else {
        panic!("project.add failed")
    };
    World { host, home: home.path().to_path_buf(), repo: repo_path, project: ProjectId(id), dirs: vec![home, repo] }
}

/// A thread whose first turn keeps running (its provider is manual).
async fn lead(w: &World, provider: &str, runtime_mode: RuntimeMode) -> ThreadId {
    let reply = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId(provider.into()),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode,
            interaction_mode: InteractionMode::Default,
            worktree: false,
            title: None,
            prompt: Some("lead the work".into()),
            attachments: Vec::new(),
            orchestrator: false,
        })
        .await;
    let Reply::Created { id } = reply else { panic!("thread.create: {reply:?}") };
    let id = ThreadId(id);
    w.wait_thread(&id, |t| t.status == ThreadStatus::Running).await;
    id
}

/// The provider's turn ends with `text` as its answer.
fn answer(session: &FakeSession, text: &str) {
    let id = session.next_item();
    session.emit(ProviderEvent::Item(Item {
        id,
        parent: None,
        status: NodeStatus::Completed,
        body: NodeBody::Message { text: text.into() },
    }));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
}

fn sends(calls: &[Call]) -> Vec<String> {
    calls.iter().filter_map(|c| if let Call::Send(t) = c { Some(t.clone()) } else { None }).collect()
}

/// Lets anything that would wrongly happen (a wake, a queued message) happen before checking.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// An MCP client with the credentials a provider session was given.
#[derive(Clone)]
struct Mcp {
    http: reqwest::Client,
    url: String,
    auth: String,
    ids: Arc<AtomicU64>,
}

impl Mcp {
    async fn of(session: &FakeSession) -> Mcp {
        let server = &session.config.mcp[0];
        assert_eq!(server.name, "roll");
        let auth = server.headers.iter().find(|(k, _)| k == "Authorization").unwrap().1.clone();
        let mcp = Mcp { http: reqwest::Client::new(), url: server.url.clone(), auth, ids: Arc::default() };
        let init = mcp
            .rpc(
                "initialize",
                json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
            )
            .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
        let status = mcp.post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.status();
        assert_eq!(status, reqwest::StatusCode::ACCEPTED);
        mcp
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .header("Authorization", &self.auth)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let response = self.post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["id"], id);
        body
    }

    /// `tools/call`: the structured result and whether it is an error.
    async fn call(&self, tool: &str, arguments: Value) -> (Value, bool) {
        let body = self.rpc("tools/call", json!({ "name": tool, "arguments": arguments })).await;
        let result = &body["result"];
        assert!(result.is_object(), "{body}");
        let structured = result["structuredContent"].clone();
        let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, structured, "the text content carries the same JSON");
        (structured, result["isError"].as_bool().unwrap())
    }

    async fn ok(&self, tool: &str, arguments: Value) -> Value {
        let (value, error) = self.call(tool, arguments).await;
        assert!(!error, "{tool}: {value}");
        value
    }

    /// The error code of a refused call.
    async fn refused(&self, tool: &str, arguments: Value) -> String {
        let (value, error) = self.call(tool, arguments).await;
        assert!(error, "{tool} should be refused: {value}");
        value["error"]["code"].as_str().unwrap().to_string()
    }

    async fn delegate(&self, arguments: Value) -> (String, ThreadId) {
        let v = self.ok("delegate_task", arguments).await;
        (v["task_id"].as_str().unwrap().to_string(), ThreadId(v["child_thread_id"].as_str().unwrap().into()))
    }
}

fn subagent_nodes(s: &Records, thread: &ThreadId) -> Vec<Node> {
    s.nodes(thread).into_iter().filter(|n| matches!(n.body, NodeBody::Subagent { .. })).collect()
}

fn task_node(s: &Records, id: &str) -> Option<Node> {
    match s.live.get(&(EntityKind::Node, id.to_string())) {
        Some(Entity::Node(n)) => Some(n.clone()),
        _ => None,
    }
}

fn node_result(node: &Node) -> Option<String> {
    match &node.body {
        NodeBody::Subagent { result, .. } => result.clone(),
        _ => None,
    }
}

#[tokio::test]
async fn the_server_speaks_streamable_http_and_tokens_name_one_live_thread() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let thread = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let session = codex.session(0).await;
    let server = &session.config.mcp[0];
    assert_eq!(server.url, w.host.mcp_url());
    let addr: std::net::SocketAddr =
        server.url.strip_prefix("http://").unwrap().strip_suffix("/mcp").unwrap().parse().unwrap();
    assert!(addr.ip().is_loopback() && addr.ip().is_ipv4());
    assert!(server.headers[0].1.starts_with("Bearer rlx_"));

    let mcp = Mcp::of(&session).await;
    let init = mcp.rpc("initialize", json!({ "protocolVersion": "1999-01-01" })).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "an unknown version gets the newest");
    assert_eq!(init["result"]["serverInfo"]["name"], "roll");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(mcp.rpc("ping", json!({})).await["result"], json!({}));
    let tools = mcp.rpc("tools/list", json!({})).await;
    let names: Vec<&str> =
        tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in ["delegate_task", "task_status", "task_wait", "task_cancel", "task_send", "list_tasks"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert_eq!(mcp.rpc("resources/list", json!({})).await["error"]["code"], -32601);
    assert_eq!(mcp.rpc("tools/call", json!({ "name": "rm_rf", "arguments": {} })).await["error"]["code"], -32602);
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "  " })).await, "invalid_request");
    assert_eq!(mcp.refused("task_status", json!({})).await, "invalid_request");

    let caps = mcp.ok("orchestrator_capabilities", json!({})).await;
    assert_eq!(caps["thread_id"], thread.0);
    assert_eq!((caps["depth"].as_u64(), caps["max_depth"].as_u64()), (Some(0), Some(3)));
    assert_eq!(caps["providers"][0]["id"], "codex");
    assert_eq!(caps["providers"][0]["available"], true);

    // A batch is answered as an array, without the notification.
    let batch = mcp
        .post(json!([{ "jsonrpc": "2.0", "id": "a", "method": "ping" }, { "jsonrpc": "2.0", "method": "notifications/x" }]))
        .await;
    let batch: Value = batch.json().await.unwrap();
    assert_eq!(batch, json!([{ "jsonrpc": "2.0", "id": "a", "result": {} }]));

    let http = reqwest::Client::new();
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
    let status = http.post(&server.url).json(&ping).send().await.unwrap().status();
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let status =
        http.post(&server.url).header("Authorization", "Bearer rlx_nope").json(&ping).send().await.unwrap().status();
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let status = http
        .post(&server.url)
        .header("Authorization", &mcp.auth)
        .header("Origin", "https://evil.example")
        .json(&ping)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    let status = http.get(&server.url).header("Authorization", &mcp.auth).send().await.unwrap().status();
    assert_eq!(status, reqwest::StatusCode::METHOD_NOT_ALLOWED);
    let garbage = http.post(&server.url).header("Authorization", &mcp.auth).body("{").send().await.unwrap();
    assert_eq!(garbage.status(), reqwest::StatusCode::BAD_REQUEST);

    // The token dies with its thread.
    w.run(Command::RunInterrupt { thread: thread.clone() }).await;
    w.wait_thread(&thread, |t| t.active_run.is_none()).await;
    assert_eq!(w.run(Command::ThreadDelete { thread }).await, Reply::Ok);
    assert_eq!(mcp.post(ping).await.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_sync_task_returns_the_childs_answer_and_the_records_show_the_subagent() {
    let codex = manual("codex");
    let claude = FakeProvider::new("claude");
    let w = world(vec![codex.clone(), claude.clone()], "").await;
    let parent = lead(&w, "codex", RuntimeMode::AutoEdit).await;
    let mcp = Mcp::of(&codex.session(0).await).await;

    let v = mcp
        .ok("delegate_task", json!({ "task": "Count the files", "provider": "claude", "mode": "sync", "title": "Counter" }))
        .await;
    assert_eq!(v["status"], "completed", "{v}");
    assert_eq!(v["result"], "echo: Count the files");
    assert_eq!(v["provider"], "claude");
    assert_eq!(v["title"], "Counter");
    assert_eq!(v["wait_timed_out"], false);
    assert!(v.get("next").is_none());
    let child = ThreadId(v["child_thread_id"].as_str().unwrap().into());
    let task = v["task_id"].as_str().unwrap().to_string();
    assert!(task.starts_with("nod_"));

    // The child got only the task, its own token, and the parent's checkout and modes.
    let session = claude.session(0).await;
    assert_eq!(sends(&session.calls()), vec!["Count the files"]);
    assert_ne!(session.config.mcp[0].headers, codex.session(0).await.config.mcp[0].headers);
    assert_eq!(session.config.runtime_mode, RuntimeMode::AutoEdit);

    let node = w
        .wait("the task node to complete", |s| task_node(s, &task).filter(|n| n.status == NodeStatus::Completed))
        .await;
    let s = w.state().await;
    let parent_thread = s.thread(&parent).unwrap();
    let parent_run = parent_thread.active_run.clone().unwrap();
    assert_eq!((node.thread.clone(), node.run.clone(), node.parent.clone()), (parent.clone(), parent_run.clone(), None));
    assert!(node.completed_at.is_some());
    // The node names the model the child runs.
    let c_model = s.thread(&child).unwrap().model.clone();
    assert_eq!(node.body, NodeBody::Subagent {
        agent: "claude".into(),
        prompt: "Count the files".into(),
        child: Some(child.clone()),
        result: Some("echo: Count the files".into()),
        progress: None,
        model: c_model,
    });
    let c = s.thread(&child).unwrap();
    assert_eq!(c.lineage, Some(Lineage {
        parent: parent.clone(),
        relation: Relation::Subagent,
        root: parent.clone(),
        from_run: Some(parent_run),
    }));
    assert_eq!(
        (c.title.as_str(), c.cwd.as_str(), c.runtime_mode),
        ("Counter", parent_thread.cwd.as_str(), RuntimeMode::AutoEdit)
    );
    assert_eq!(c.project, w.project);
    assert_eq!(s.runs(&child)[0].prompt, "Count the files");

    // The result went to the waiting call, so it wakes nobody.
    answer(&codex.session(0).await, "done");
    w.wait_thread(&parent, |t| t.active_run.is_none()).await;
    settle().await;
    let t = w.state().await.thread(&parent).cloned().unwrap();
    assert_eq!((t.run_count, t.queued.len()), (1, 0));
    assert_eq!(mcp.ok("task_status", json!({ "task_id": task })).await["status"], "completed");
}

#[tokio::test]
async fn async_results_wake_the_parent_queued_behind_its_turn_or_at_once_when_idle() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let parent = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let lead_session = codex.session(0).await;
    let mcp = Mcp::of(&lead_session).await;

    let v = mcp.ok("delegate_task", json!({ "task": "research the parser", "title": "Research" })).await;
    assert_eq!(v["status"], "running");
    assert_eq!(v["next"], "Completion wakes this thread, so end your turn. Do not wait on it or poll it.");
    let first = v["task_id"].as_str().unwrap().to_string();
    let (second, _) = mcp.delegate(json!({ "task": "check the tests", "title": "Tests" })).await;
    let listed = mcp.ok("list_tasks", json!({})).await;
    let listed: Vec<&str> = listed["tasks"].as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap()).collect();
    assert_eq!(listed, vec![first.as_str(), second.as_str()]);

    // The parent is busy: the result queues behind its turn, and the next one joins it.
    answer(&codex.session(1).await, "the parser is LL(1)");
    let t = w.wait_thread(&parent, |t| t.queued.len() == 1).await;
    assert_eq!(t.queued[0].text, format!("Delegated task {first} (Research) [completed]:\nthe parser is LL(1)"));
    answer(&codex.session(2).await, "all green");
    let t = w.wait_thread(&parent, |t| t.queued.len() == 1 && t.queued[0].text.contains("all green")).await;
    let wake = format!(
        "Delegated task {first} (Research) [completed]:\nthe parser is LL(1)\n\nDelegated task {second} (Tests) [completed]:\nall green"
    );
    assert_eq!(t.queued[0].text, wake);
    let node = w.wait("the node result", |s| task_node(s, &second).filter(|n| node_result(n).is_some())).await;
    assert_eq!(node_result(&node).as_deref(), Some("all green"));

    // The turn ends; the wake runs next, as the parent's own new run.
    answer(&lead_session, "waiting for my tasks");
    let t = w.wait_thread(&parent, |t| t.run_count == 2 && t.queued.is_empty()).await;
    assert_eq!(w.state().await.runs(&parent)[1].prompt, wake);
    let calls = lead_session.wait_calls(|c| sends(c).len() == 2).await;
    assert_eq!(sends(&calls)[1], wake);
    assert!(t.active_run.is_some());

    // An idle parent wakes at once.
    let (third, _) = mcp.delegate(json!({ "task": "write docs", "title": "Docs" })).await;
    answer(&lead_session, "ok");
    w.wait_thread(&parent, |t| t.active_run.is_none() && t.run_count == 2).await;
    answer(&codex.session(3).await, "docs written");
    w.wait_thread(&parent, |t| t.run_count == 3).await;
    assert_eq!(w.state().await.runs(&parent)[2].prompt, format!("Delegated task {third} (Docs) [completed]:\ndocs written"));
}

#[tokio::test]
async fn a_result_read_first_is_taken_out_of_the_queued_wake() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let parent = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let lead_session = codex.session(0).await;
    let mcp = Mcp::of(&lead_session).await;
    let (task, _) = mcp.delegate(json!({ "task": "look around" })).await;
    answer(&codex.session(1).await, "seen it");
    w.wait_thread(&parent, |t| t.queued.len() == 1).await;

    let v = mcp.ok("task_status", json!({ "task_id": task })).await;
    assert_eq!((v["status"].as_str(), v["result"].as_str()), (Some("completed"), Some("seen it")));
    w.wait_thread(&parent, |t| t.queued.is_empty()).await;
    answer(&lead_session, "done");
    w.wait_thread(&parent, |t| t.active_run.is_none()).await;
    settle().await;
    assert_eq!(w.state().await.thread(&parent).unwrap().run_count, 1, "nothing woke the parent");
}

#[tokio::test]
async fn subagents_nest_within_the_depth_limit_and_are_refused_beyond_it() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "[delegation]\nmax_depth = 2\n").await;
    let root = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp_root = Mcp::of(&codex.session(0).await).await;
    let (task_a, a) = mcp_root.delegate(json!({ "task": "plan the work" })).await;
    let mcp_a = Mcp::of(&codex.session(1).await).await;
    let caps = mcp_a.ok("orchestrator_capabilities", json!({})).await;
    assert_eq!((caps["depth"].as_u64(), caps["can_delegate"].as_bool()), (Some(1), Some(true)));
    let (task_b, b) = mcp_a.delegate(json!({ "task": "do one part" })).await;
    let mcp_b = Mcp::of(&codex.session(2).await).await;
    assert_eq!(mcp_b.ok("orchestrator_capabilities", json!({})).await["can_delegate"], false);
    assert_eq!(mcp_b.refused("delegate_task", json!({ "task": "too deep" })).await, "depth_exceeded");

    let s = w.state().await;
    let lb = s.thread(&b).unwrap().lineage.clone().unwrap();
    assert_eq!((lb.parent, lb.root, lb.relation), (a.clone(), root.clone(), Relation::Subagent));
    assert_eq!(lb.from_run, s.thread(&a).unwrap().active_run);
    let b_node = task_node(&s, &task_b).unwrap();
    assert_eq!(b_node.thread, a, "the grandchild's node is in its own parent's run");
    assert_eq!(subagent_nodes(&s, &root).len(), 1);

    // The root sees and controls its subagents' tasks too.
    assert_eq!(mcp_root.ok("task_status", json!({ "task_id": task_b })).await["child_thread_id"], b.0);
    answer(&codex.session(2).await, "part done");
    let v = mcp_a.ok("task_wait", json!({ "task_id": task_b, "timeout_ms": 5000 })).await;
    assert_eq!((v["status"].as_str(), v["result"].as_str()), (Some("completed"), Some("part done")));
    assert_eq!(mcp_root.ok("task_status", json!({ "task_id": task_a })).await["status"], "running");
}

#[tokio::test]
async fn a_task_can_not_reach_its_parent_its_siblings_or_another_tree() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp_root = Mcp::of(&codex.session(0).await).await;
    let (task_a, _) = mcp_root.delegate(json!({ "task": "a" })).await;
    let (task_s, _) = mcp_root.delegate(json!({ "task": "sibling" })).await;
    let mcp_a = Mcp::of(&codex.session(1).await).await;
    lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp_other = Mcp::of(&codex.session(3).await).await;

    for (mcp, task) in [(&mcp_a, &task_s), (&mcp_a, &task_a), (&mcp_other, &task_a)] {
        assert_eq!(mcp.refused("task_status", json!({ "task_id": task })).await, "task_not_found");
        assert_eq!(mcp.refused("task_wait", json!({ "task_id": task, "timeout_ms": 1 })).await, "task_not_found");
        assert_eq!(mcp.refused("task_cancel", json!({ "task_id": task })).await, "task_not_found");
        assert_eq!(mcp.refused("task_send", json!({ "task_id": task, "message": "stop" })).await, "task_not_found");
    }
    assert_eq!(mcp_a.refused("task_status", json!({ "task_id": "nod_nope" })).await, "task_not_found");
    assert_eq!(mcp_a.ok("list_tasks", json!({})).await["tasks"], json!([]));
    assert_eq!(mcp_other.ok("list_tasks", json!({})).await["tasks"], json!([]));
    assert_eq!(mcp_root.ok("task_status", json!({ "task_id": task_s })).await["status"], "running");
}

#[tokio::test]
async fn cancelling_a_task_stops_everything_under_it_and_wakes_nobody() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let root = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let lead_session = codex.session(0).await;
    let mcp_root = Mcp::of(&lead_session).await;
    let (task_a, a) = mcp_root.delegate(json!({ "task": "a" })).await;
    let mcp_a = Mcp::of(&codex.session(1).await).await;
    let (task_b, b) = mcp_a.delegate(json!({ "task": "b" })).await;
    let mcp_b = Mcp::of(&codex.session(2).await).await;
    let (task_c, c) = mcp_b.delegate(json!({ "task": "c" })).await;

    let v = mcp_root.ok("task_cancel", json!({ "task_id": task_a })).await;
    assert_eq!(v, json!({ "task_id": task_a, "status": "cancel_requested" }));
    for (session, thread) in [(1, &a), (2, &b), (3, &c)] {
        codex.session(session).await.wait_calls(|c| c.contains(&Call::Interrupt)).await;
        w.wait_thread(thread, |t| t.active_run.is_none()).await;
    }
    for task in [&task_a, &task_b, &task_c] {
        let node =
            w.wait("the node to stop", |s| task_node(s, task).filter(|n| n.status == NodeStatus::Interrupted)).await;
        assert!(node.completed_at.is_some());
        assert_eq!(mcp_root.ok("task_status", json!({ "task_id": task })).await["status"], "cancelled");
    }
    assert_eq!(w.state().await.runs(&a)[0].status, RunStatus::Interrupted);
    // A finished task stays as it was.
    assert_eq!(mcp_root.ok("task_cancel", json!({ "task_id": task_a })).await["status"], "cancelled");

    answer(&lead_session, "done");
    w.wait_thread(&root, |t| t.active_run.is_none()).await;
    settle().await;
    let s = w.state().await;
    for thread in [&root, &a, &b] {
        let t = s.thread(thread).unwrap();
        assert_eq!((t.run_count, t.queued.len()), (1, 0), "a cancelled task wakes nobody");
    }
}

#[tokio::test]
async fn interrupting_the_parent_ends_its_sync_wait_and_stops_its_tasks() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let root = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp = Mcp::of(&codex.session(0).await).await;
    let (task_early, early) = mcp.delegate(json!({ "task": "background" })).await;
    let waiting = {
        let mcp = mcp.clone();
        tokio::spawn(async move { mcp.call("delegate_task", json!({ "task": "blocking", "mode": "sync" })).await })
    };
    let blocking = codex.session(2).await;
    blocking.wait_calls(|c| sends(c) == ["blocking"]).await;
    settle().await;
    assert!(!waiting.is_finished());

    assert_eq!(w.run(Command::RunInterrupt { thread: root.clone() }).await, Reply::Ok);
    let (v, error) = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    // Whichever stops first, the turn or its task, ends the wait.
    if error {
        assert_eq!(v["error"]["code"], "parent_not_active");
    } else {
        assert_eq!(v["status"], "cancelled", "{v}");
    }
    blocking.wait_calls(|c| c.contains(&Call::Interrupt)).await;
    codex.session(1).await.wait_calls(|c| c.contains(&Call::Interrupt)).await;
    w.wait_thread(&early, |t| t.active_run.is_none()).await;
    let nodes = w
        .wait("both nodes to stop", |s| {
            let nodes = subagent_nodes(s, &root);
            (nodes.len() == 2 && nodes.iter().all(|n| n.status == NodeStatus::Interrupted)).then_some(nodes)
        })
        .await;
    assert!(nodes.iter().any(|n| n.id.0 == task_early));
    settle().await;
    let t = w.state().await.thread(&root).cloned().unwrap();
    assert_eq!((t.run_count, t.queued.len()), (1, 0));
}

#[tokio::test]
async fn child_approvals_are_requests_on_the_child_and_end_a_sync_wait() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "").await;
    let root = lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp = Mcp::of(&codex.session(0).await).await;
    let waiting = {
        let mcp = mcp.clone();
        tokio::spawn(async move { mcp.ok("delegate_task", json!({ "task": "deploy", "mode": "sync" })).await })
    };
    let child_session = codex.session(1).await;
    child_session.wait_calls(|c| !sends(c).is_empty()).await;
    child_session.emit(ProviderEvent::Request {
        id: "rpc-1".into(),
        item: None,
        body: RequestBody::Approval(Approval {
            action: ApprovalAction::Command,
            title: "Run `make deploy`".into(),
            command: Some("make deploy".into()),
            cwd: None,
            files: Vec::new(),
            reason: None,
            tool: None,
            input: Value::Null,
            decisions: vec![Decision::Accept, Decision::Decline],
        }),
    });
    let v = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    assert_eq!(v["status"], "waiting");
    assert_eq!(v["pending_request"]["kind"], "approval");
    assert_eq!(v["pending_request"]["title"], "Run `make deploy`");
    let task = v["task_id"].as_str().unwrap().to_string();
    let child = ThreadId(v["child_thread_id"].as_str().unwrap().into());
    let request = w.state().await.requests(&child).pop().unwrap();
    assert_eq!(request.id.0, v["pending_request"]["request_id"].as_str().unwrap());
    assert_eq!(request.status, RequestStatus::Pending);
    w.wait("the parent's node to wait", |s| task_node(s, &task).filter(|n| n.status == NodeStatus::Waiting)).await;
    assert!(w.state().await.requests(&root).is_empty(), "the request is the child's, not the parent's");

    // Answered on the canvas; the task goes on and finishes.
    let respond = Command::RequestRespond {
        request: request.id.clone(),
        response: Response::Decision { decision: Decision::Accept, message: None },
    };
    assert_eq!(w.run(respond).await, Reply::Ok);
    w.wait("the node to run again", |s| task_node(s, &task).filter(|n| n.status == NodeStatus::Running)).await;
    let waiting = {
        let mcp = mcp.clone();
        let task = task.clone();
        tokio::spawn(async move { mcp.ok("task_wait", json!({ "task_id": task })).await })
    };
    settle().await;
    answer(&child_session, "deployed");
    let v = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    assert_eq!((v["status"].as_str(), v["result"].as_str()), (Some("completed"), Some("deployed")));

    // A wait that runs out says so and leaves the task alone.
    let v = mcp.ok("task_send", json!({ "task_id": task, "message": "and roll back" })).await;
    assert_eq!(v["status"], "running");
    let v = mcp.ok("task_wait", json!({ "task_id": task, "timeout_ms": 50 })).await;
    assert_eq!((v["status"].as_str(), v["wait_timed_out"].as_bool()), (Some("running"), Some(true)));
}

#[tokio::test]
async fn limits_providers_models_modes_worktrees_and_follow_ups() {
    let codex = manual("codex");
    let w = world(vec![codex.clone()], "[delegation]\nmax_children = 2\n").await;
    let root = lead(&w, "codex", RuntimeMode::AutoEdit).await;
    let lead_session = codex.session(0).await;
    let mcp = Mcp::of(&lead_session).await;

    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "provider": "gemini" })).await, "provider_unavailable");
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "model": "gpt-0" })).await, "model_unavailable");
    assert_eq!(
        mcp.refused("delegate_task", json!({ "task": "x", "runtime_mode": "full_access" })).await,
        "runtime_mode_escalation_denied"
    );
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "mode": "later" })).await, "invalid_request");
    // Options the model doesn't take, or with values it doesn't offer, are refused.
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "model": "fake-1", "options": { "fastMode": "yes" } })).await, "option_unavailable");
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "model": "fake-2", "options": { "fastMode": true } })).await, "option_unavailable");
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "x", "options": { "effort": "high" } })).await, "invalid_request");
    // The capabilities list each model's options.
    let caps = mcp.ok("orchestrator_capabilities", json!({})).await;
    let fake1 = &caps["providers"][0]["models"][0];
    assert_eq!((fake1["efforts"].clone(), fake1["options"][1]["id"].clone()), (json!(["low", "high"]), json!("fastMode")));

    let (_, asked) = mcp
        .delegate(json!({ "task": "carefully", "runtime_mode": "ask", "interaction_mode": "plan", "model": "fake-1", "options": { "fastMode": true } }))
        .await;
    let (task, isolated) = mcp.delegate(json!({ "task": "fix the parser", "worktree": true })).await;
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "third" })).await, "too_many_tasks");

    let s = w.state().await;
    let a = s.thread(&asked).unwrap();
    assert_eq!(
        (a.runtime_mode, a.interaction_mode, a.model.as_deref()),
        (RuntimeMode::Ask, InteractionMode::Plan, Some("fake-1"))
    );
    assert_eq!(a.options, ModelOptions::from([("fastMode".to_string(), OptionValue::Flag(true))]));
    let i = s.thread(&isolated).unwrap();
    assert_eq!(i.runtime_mode, RuntimeMode::AutoEdit, "inherited");
    let wt = i.worktree.clone().unwrap();
    assert_eq!(wt.branch, "roll/fix-the-parser");
    assert_eq!(i.cwd, wt.path);
    assert_eq!(wt.base, sh(&w.repo, &["rev-parse", "HEAD"]).trim());
    let child_session = codex.session(2).await;
    assert_eq!(child_session.config.cwd, std::path::Path::new(&wt.path));

    // A finished task frees a slot; a follow-up runs in the same child and reports again.
    answer(&child_session, "fixed");
    mcp.ok("task_wait", json!({ "task_id": task, "timeout_ms": 5000 })).await;
    let (third_task, _) = mcp.delegate(json!({ "task": "third" })).await;
    assert_eq!(mcp.ok("task_cancel", json!({ "task_id": third_task })).await["status"], "cancel_requested");

    let v = mcp.ok("task_send", json!({ "task_id": task, "message": "now add a test" })).await;
    assert_eq!(v["status"], "running");
    w.wait("the node to run again", |s| {
        task_node(s, &task).filter(|n| n.status == NodeStatus::Running && n.completed_at.is_none())
    })
    .await;
    assert_eq!(sends(&child_session.calls()), vec!["fix the parser", "now add a test"], "same conversation");
    answer(&lead_session, "over to the subagents");
    w.wait_thread(&root, |t| t.active_run.is_none()).await;
    answer(&child_session, "test added");
    w.wait_thread(&root, |t| t.run_count == 2).await;
    assert!(w.state().await.runs(&root)[1].prompt.ends_with("[completed]:\ntest added"));
    let node = task_node(&w.state().await, &task).unwrap();
    assert_eq!((node.status, node_result(&node).as_deref()), (NodeStatus::Completed, Some("test added")));

    // Delegating needs a running turn.
    answer(&lead_session, "thanks");
    w.wait_thread(&root, |t| t.active_run.is_none()).await;
    assert_eq!(mcp.refused("delegate_task", json!({ "task": "late" })).await, "parent_not_active");
}

/// "Orchestrate" on the canvas: the thread is an orchestrator, its sessions are told to plan and
/// delegate (and every session is told how delegation works), the run records only the user's
/// goal, its tasks are plain subagents told to do the work themselves, and a fork goes on
/// orchestrating.
#[tokio::test]
async fn an_orchestrator_is_told_to_plan_and_delegate_and_its_tasks_to_do_the_work() {
    let codex = manual("codex");
    let claude = FakeProvider::new("claude");
    let w = world(vec![codex.clone(), claude.clone()], "").await;
    let goal = "Add dark mode to the settings page, with tests";
    let reply = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId::codex(),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            worktree: false,
            title: None,
            prompt: Some(goal.into()),
            attachments: Vec::new(),
            orchestrator: true,
        })
        .await;
    let Reply::Created { id } = reply else { panic!("thread.create: {reply:?}") };
    let lead = ThreadId(id);
    let t = w.wait_thread(&lead, |t| t.status == ThreadStatus::Running).await;
    assert!(t.orchestrator);
    assert_eq!(w.state().await.runs(&lead)[0].prompt, goal, "the run keeps what the user sent");

    let session = codex.session(0).await;
    assert_eq!(sends(&session.calls()), vec![goal]);
    let told = session.config.instructions.clone().unwrap();
    assert!(told.contains("## roll orchestration") && told.contains("## You are the orchestrator"), "{told}");
    assert!(!told.contains("## You are a subagent"));

    // It plans, then hands out the parts: each an ordinary subagent in its own worktree.
    let mcp = Mcp::of(&session).await;
    mcp.ok("orchestrator_capabilities", json!({})).await;
    let (_, tokens) = mcp.delegate(json!({ "task": "Theme tokens for dark mode", "provider": "claude", "worktree": true, "title": "Tokens" })).await;
    let (_, tests) = mcp.delegate(json!({ "task": "Tests for the theme switch", "provider": "claude", "worktree": true, "title": "Tests" })).await;
    let s = w.state().await;
    for child in [&tokens, &tests] {
        let c = s.thread(child).unwrap();
        assert!(!c.orchestrator, "a task is never an orchestrator");
        assert_eq!(c.lineage.as_ref().map(|l| (&l.parent, l.relation)), Some((&lead, Relation::Subagent)));
        assert!(c.worktree.is_some());
    }
    let child = claude.session(0).await.config.instructions.clone().unwrap();
    assert!(child.contains("## roll orchestration") && child.contains("## You are a subagent"), "{child}");
    assert!(!child.contains("## You are the orchestrator"));

    // A thread the user starts as usual only learns how delegation works.
    answer(&codex.session(0).await, "planned");
    let plain = w.thread("claude", Some("hi"), false).await;
    w.wait_thread(&plain, |t| t.run_count == 1 && t.active_run.is_none()).await;
    let sessions = claude.sessions();
    let mine = sessions.iter().find(|s| sends(&s.calls()) == ["hi"]).expect("the plain thread's session");
    let told = mine.config.instructions.clone().unwrap();
    assert!(told.contains("## roll orchestration") && !told.contains("## You are"), "{told}");

    // The tasks' results wake the orchestrator once its planning turn is over (together, or one
    // after the other as they finish).
    let (mut seen, mut woke) = (1, String::new());
    while !(woke.contains("(Tokens) [completed]") && woke.contains("(Tests) [completed]")) {
        let t = w.wait_thread(&lead, |t| t.run_count > seen && t.active_run.is_some()).await;
        for run in &w.state().await.runs(&lead)[seen as usize..t.run_count as usize] {
            woke.push_str(&run.prompt);
        }
        seen = t.run_count;
        answer(&codex.session(0).await, "checked");
    }
    w.wait_thread(&lead, |t| t.active_run.is_none()).await;

    // Forking the orchestrator forks an orchestrator.
    let Reply::Created { id } = w.run(Command::ThreadFork { thread: lead.clone(), at: None, worktree: false }).await else {
        panic!("thread.fork")
    };
    assert!(w.state().await.thread(&ThreadId(id)).unwrap().orchestrator);
}

/// Delegating while the daemon is still probing its providers waits for the probe rather than
/// calling the provider unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_waits_for_a_provider_still_being_checked() {
    use agent_host::Daemon;
    let codex = manual("codex");
    codex.hold_probes();
    let w = world(vec![codex.clone()], "").await;
    assert!(w.host.info().await.providers[0].checking);
    lead(&w, "codex", RuntimeMode::FullAccess).await;
    let mcp = Mcp::of(&codex.session(0).await).await;
    let caps = tokio::spawn(async move { mcp.ok("orchestrator_capabilities", json!({})).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!caps.is_finished(), "answered before the probe did");
    codex.release_probes();
    let caps = tokio::time::timeout(Duration::from_secs(10), caps).await.expect("answered once probed").unwrap();
    assert_eq!(caps["providers"][0]["available"], true);
}

/// An orchestrator on the fake orchestrator model (`Script::Orchestrate`, `orchestrate` under
/// `ROLL_AGENT_FAKE=1 roll agent up`), which calls the MCP server itself.
fn orchestrate() -> Arc<FakeProvider> {
    FakeProvider::with("orchestrate", Capabilities::default(), Script::Orchestrate)
}

/// "Orchestrate" with `goal` on `provider`.
async fn orchestrator(w: &World, provider: &str, goal: &str) -> ThreadId {
    let reply = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId(provider.into()),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: RuntimeMode::AutoEdit,
            interaction_mode: InteractionMode::Default,
            worktree: false,
            title: None,
            prompt: Some(goal.into()),
            attachments: Vec::new(),
            orchestrator: true,
        })
        .await;
    let Reply::Created { id } = reply else { panic!("thread.create: {reply:?}") };
    ThreadId(id)
}

/// A thread's finished messages, oldest run first.
fn messages(s: &Records, thread: &ThreadId) -> Vec<String> {
    let runs: Vec<RunId> = s.runs(thread).into_iter().map(|r| r.id).collect();
    let mut nodes = s.nodes(thread);
    nodes.sort_by_key(|n| (runs.iter().position(|r| r == &n.run), n.ordinal));
    nodes
        .into_iter()
        .filter_map(|n| match n.body {
            NodeBody::Message { text } if n.status == NodeStatus::Completed => Some(text),
            _ => None,
        })
        .collect()
}

/// A thread's MCP tool calls, in order: (tool, status).
fn tool_calls(s: &Records, thread: &ThreadId) -> Vec<(String, NodeStatus)> {
    s.nodes(thread)
        .into_iter()
        .filter_map(|n| match n.body {
            NodeBody::Tool(t) if t.kind == ToolKind::Mcp => Some((t.title, n.status)),
            _ => None,
        })
        .collect()
}

/// The subagent threads under `parent`, by title.
fn children(s: &Records, parent: &ThreadId) -> Vec<Thread> {
    let mut children: Vec<Thread> = s
        .live
        .values()
        .filter_map(|e| match e {
            Entity::Thread(t) if t.lineage.as_ref().is_some_and(|l| &l.parent == parent && l.relation == Relation::Subagent) => {
                Some(t.clone())
            }
            _ => None,
        })
        .collect();
    children.sort_by(|a, b| a.title.cmp(&b.title));
    children
}

fn task_of(s: &Records, parent: &ThreadId, child: &ThreadId) -> Node {
    subagent_nodes(s, parent)
        .into_iter()
        .find(|n| matches!(&n.body, NodeBody::Subagent { child: Some(c), .. } if c == child))
        .expect("the child's task node")
}

/// Waits (up to 10 s) until the orchestrator is idle with its last message ending in `line`.
async fn summed_up(w: &World, lead: &ThreadId, line: &str) -> Records {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = w.state().await;
        let idle = s.thread(lead).is_some_and(|t| t.active_run.is_none() && t.run_count > 0);
        if idle && messages(&s, lead).last().is_some_and(|m| m.ends_with(line)) {
            return s;
        }
        if tokio::time::Instant::now() > deadline {
            let runs: Vec<_> = s.runs(lead).into_iter().map(|r| (r.status, r.prompt, r.error)).collect();
            let kids: Vec<_> = children(&s, lead).into_iter().map(|t| (t.title, t.status, t.run_count)).collect();
            let nodes: Vec<_> = s.nodes(lead).into_iter().map(|n| (n.status, n.body)).collect();
            panic!("the orchestrator never said {line:?}:\nruns {runs:#?}\nchildren {kids:?}\nnodes {nodes:#?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The whole loop as a real orchestrator model drives it, through the HTTP server:
/// capabilities, two async tasks that become subagent threads under the orchestrator, their
/// `subagent` nodes following them to completed, the results waking the orchestrator (in one
/// wake or two), and its summary once both are in.
#[tokio::test]
async fn the_fake_orchestrator_delegates_through_the_server_and_sums_up_when_every_part_is_back() {
    let lead_provider = orchestrate();
    let w = world(vec![lead_provider.clone()], "").await;
    let lead = orchestrator(&w, "orchestrate", "Ship dark mode").await;
    let s = summed_up(&w, &lead, "All 2 parts done.").await;

    let runs = s.runs(&lead);
    assert_eq!(runs[0].prompt, "Ship dark mode");
    assert!(runs.len() >= 2 && runs.iter().all(|r| r.status == RunStatus::Completed), "{runs:?}");
    assert!(runs[1..].iter().all(|r| r.prompt.starts_with("Delegated task ")), "{runs:?}");
    let woke = runs[1..].iter().map(|r| r.prompt.as_str()).collect::<Vec<_>>().join("\n\n");
    let mcp = |tool: &str, status| (tool.to_string(), status);
    assert_eq!(tool_calls(&s, &lead), [
        mcp("orchestrator_capabilities", NodeStatus::Completed),
        mcp("delegate_task", NodeStatus::Completed),
        mcp("delegate_task", NodeStatus::Completed)
    ]);

    let kids = children(&s, &lead);
    assert_eq!(kids.len(), 2, "{kids:?}");
    assert_eq!(subagent_nodes(&s, &lead).len(), 2);
    for (i, kid) in kids.iter().enumerate() {
        let (n, task) = (i + 1, format!("Part {} of 2: Ship dark mode", i + 1));
        assert_eq!((kid.title.clone(), kid.provider.0.as_str(), kid.orchestrator), (format!("Part {n}"), "orchestrate", false));
        assert_eq!(kid.lineage.as_ref().unwrap().from_run.as_ref(), Some(&runs[0].id));
        let run = &s.runs(&kid.id)[0];
        assert_eq!((run.prompt.as_str(), run.status), (task.as_str(), RunStatus::Completed));
        let node = task_of(&s, &lead, &kid.id);
        assert_eq!((node.status, &node.run), (NodeStatus::Completed, &runs[0].id));
        assert_eq!(node_result(&node), Some(format!("Done: {task}")));
        assert!(woke.contains(&format!("Delegated task {} (Part {n}) [completed]:\nDone: {task}", node.id)), "{woke}");
        // A subagent does the work and delegates nothing.
        assert!(subagent_nodes(&s, &kid.id).is_empty() && tool_calls(&s, &kid.id).is_empty());
    }
    let told: Vec<String> = lead_provider.sessions().iter().filter_map(|x| x.config.instructions.clone()).collect();
    assert_eq!(told.iter().filter(|t| t.contains("## You are the orchestrator")).count(), 1);
    assert_eq!(told.iter().filter(|t| t.contains("## You are a subagent")).count(), 2);
}

/// A subagent whose run fails: its node fails with the run's error as the result, the wake says
/// `[failed]`, and the orchestrator still finishes, counting it.
#[tokio::test]
async fn a_failing_part_reports_failed_to_the_orchestrator() {
    let w = world(vec![orchestrate()], "").await;
    let lead = orchestrator(&w, "orchestrate", &format!("Ship dark mode {FAIL}")).await;
    let s = summed_up(&w, &lead, "1 of 2 failed.").await;
    let kids = children(&s, &lead);
    assert_eq!(kids.len(), 2);
    assert_eq!((s.runs(&kids[0].id)[0].status, s.runs(&kids[1].id)[0].status), (RunStatus::Completed, RunStatus::Failed));
    assert_eq!(task_of(&s, &lead, &kids[0].id).status, NodeStatus::Completed);
    let failed = task_of(&s, &lead, &kids[1].id);
    assert_eq!((failed.status, node_result(&failed).as_deref()), (NodeStatus::Failed, Some(FAILED)));
    assert!(failed.completed_at.is_some());
    let woke: String = s.runs(&lead)[1..].iter().map(|r| r.prompt.clone()).collect();
    assert!(woke.contains(&format!("Delegated task {} (Part 2) [failed]:\n{FAILED}", failed.id)), "{woke}");
    assert!(s.runs(&lead).iter().all(|r| r.status == RunStatus::Completed));
}

/// Parts for a provider that is logged out, or not on the machine at all, are refused with
/// `provider_unavailable`: no thread starts, nothing wakes the orchestrator, and its turn ends.
#[tokio::test]
async fn parts_for_an_unavailable_provider_are_refused_and_the_orchestrator_still_ends_its_turn() {
    let lead_provider = orchestrate();
    let echo = FakeProvider::new("echo");
    echo.set_auth(AuthStatus::Missing);
    let w = world(vec![lead_provider.clone(), echo], "").await;
    for provider in ["echo", "gemini"] {
        lead_provider.delegate_to(provider);
        let lead = orchestrator(&w, "orchestrate", "Ship dark mode").await;
        let s = summed_up(&w, &lead, "No part could be delegated.").await;
        let said = messages(&s, &lead).pop().unwrap();
        assert_eq!(said.matches("wasn't delegated: provider_unavailable: ").count(), 2, "{said}");
        assert!(said.contains(&format!("provider {provider} isn't installed and logged in")), "{said}");
        let refused = ("delegate_task".to_string(), NodeStatus::Failed);
        assert_eq!(tool_calls(&s, &lead)[1..], [refused.clone(), refused]);
        assert!(children(&s, &lead).is_empty() && subagent_nodes(&s, &lead).is_empty());
        settle().await;
        let s = w.state().await;
        assert_eq!(s.thread(&lead).unwrap().run_count, 1, "nothing woke it");
        assert_eq!(s.runs(&lead)[0].status, RunStatus::Completed);
    }
}
