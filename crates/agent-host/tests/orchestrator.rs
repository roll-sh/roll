//! The orchestrator on fake providers: runs, items, queueing, steering, interrupts, requests,
//! idempotent commands, restart recovery, and the replay/subscribe contract.

mod common;

use agent_host::provider::fake::{Call, FakeProvider, Script};
use agent_host::provider::{Item, ProviderEvent, TurnEnd};
use agent_host::{Daemon, Outbound};
use agent_proto::*;
use common::*;
use std::collections::HashSet;

fn manual(id: &str, steer: bool) -> std::sync::Arc<FakeProvider> {
    FakeProvider::with(id, Capabilities { steer, ..Capabilities::default() }, Script::Manual)
}

fn sends(calls: &[Call]) -> Vec<String> {
    calls.iter().filter_map(|c| if let Call::Send(t) = c { Some(t.clone()) } else { None }).collect()
}

#[tokio::test]
async fn a_prompt_runs_streams_items_and_completes_with_checkpoints() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake.clone()]).await;
    let mut events = w.host.subscribe();
    let thread = w.thread("codex", Some("Say hello   to\nthe world"), false).await;

    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 1).await;
    assert_eq!(t.title, "Say hello to the world");
    assert_eq!(t.cwd, w.repo.to_string_lossy());
    assert!(t.active_run.is_none() && t.activity.is_none());
    let s = w.state().await;
    let runs = s.runs(&thread);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);
    assert_eq!(runs[0].prompt, "Say hello   to\nthe world");
    assert!(runs[0].usage.output_tokens > 0);
    assert_eq!(t.usage, runs[0].usage);
    let nodes = s.nodes(&thread);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].status, NodeStatus::Completed);
    assert_eq!(nodes[0].body, NodeBody::Message { text: "echo: Say hello   to\nthe world".into() });
    let checkpoints = s.checkpoints(&thread);
    assert_eq!(checkpoints.iter().map(|c| c.ordinal).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(runs[0].checkpoint.as_ref(), Some(&checkpoints[1].id));
    assert_eq!(sh(&w.repo, &["rev-parse", &format!("refs/roll/checkpoints/{thread}/1")]).trim(), checkpoints[1].commit);

    // Deltas went out live, between the node's upserts.
    let mut texts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Outbound::Delta(Delta::Text { node, text, .. }) = event {
            assert_eq!(node, nodes[0].id);
            texts.push(text);
        }
    }
    assert_eq!(texts.concat(), "echo: Say hello   to\nthe world");

    let session = fake.session(0).await;
    assert_eq!(session.config.cwd, w.repo);
    assert_eq!(sends(&session.calls()), vec!["Say hello   to\nthe world"]);
}

#[tokio::test]
async fn items_become_a_node_tree_with_activity() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("go"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    let tool = |id: &str, parent: Option<&str>, status| {
        ProviderEvent::Item(Item {
            id: id.into(),
            parent: parent.map(String::from),
            status,
            body: NodeBody::Tool(ToolCall {
                name: "Bash".into(),
                kind: ToolKind::Command,
                title: "cargo test".into(),
                input: serde_json::json!({"command": "cargo test"}),
                output: None,
                exit_code: None,
                files: Vec::new(),
            }),
        })
    };
    session.emit(ProviderEvent::Item(Item {
        id: "task".into(),
        parent: None,
        status: NodeStatus::Running,
        body: NodeBody::Subagent { agent: "explorer".into(), prompt: "look around".into(), child: None, result: None },
    }));
    session.emit(tool("t1", Some("task"), NodeStatus::Running));
    let t = w.wait_thread(&thread, |t| t.activity.as_deref() == Some("Running cargo test")).await;
    assert_eq!(t.status, ThreadStatus::Running);
    let s = w.state().await;
    let nodes = s.nodes(&thread);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[1].parent.as_ref(), Some(&nodes[0].id));
    assert_eq!((nodes[0].ordinal, nodes[1].ordinal), (1, 2));

    session.emit(ProviderEvent::OutputDelta { item: "t1".into(), text: "ok".into() });
    session.emit(tool("t1", Some("task"), NodeStatus::Completed));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    let nodes = w.state().await.nodes(&thread);
    // The subagent never said it finished: the completion barrier closes it.
    assert!(nodes.iter().all(|n| n.status == NodeStatus::Completed && n.completed_at.is_some()));
}

#[tokio::test]
async fn streaming_nodes_are_upserted_at_most_every_500ms_and_on_completion() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("go"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    let mut events = w.host.subscribe();
    let mut text = String::new();
    for i in 0..50 {
        text.push_str(&format!("w{i} "));
        session.emit(ProviderEvent::Item(Item {
            id: "m".into(),
            parent: None,
            status: NodeStatus::Running,
            body: NodeBody::Message { text: text.clone() },
        }));
    }
    let node = w.wait("the node", |s| s.nodes(&thread).first().cloned()).await;
    // The throttled tail lands within a flush interval.
    w.wait("the latest text", |s| {
        s.nodes(&thread).first().filter(|n| n.body == NodeBody::Message { text: text.clone() }).cloned()
    })
    .await;
    let mut upserts = 0;
    while let Ok(event) = events.try_recv() {
        if let Outbound::Entity(s) = event
            && s.entity.id() == node.id.as_str()
        {
            upserts += 1;
        }
    }
    assert!((1..=3).contains(&upserts), "{upserts} upserts for 50 updates");
}

#[tokio::test]
async fn messages_queue_behind_a_running_run_and_can_be_unqueued() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    let Reply::Created { id: two } = w.send(&thread, "two", SendMode::Queue).await else { panic!() };
    let Reply::Created { .. } = w.send(&thread, "three", SendMode::Queue).await else { panic!() };
    let t = w.wait_thread(&thread, |t| t.queued.len() == 2).await;
    assert_eq!(t.queued[0].text, "two");
    assert_eq!(w.run(Command::MessageUnqueue { thread: thread.clone(), message: two }).await, Reply::Ok);
    let unknown = w.run(Command::MessageUnqueue { thread: thread.clone(), message: "msg_nope".into() }).await;
    assert!(matches!(unknown, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));

    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    let t = w.wait_thread(&thread, |t| t.run_count == 2 && t.status == ThreadStatus::Running).await;
    assert!(t.queued.is_empty());
    assert_eq!(sends(&session.wait_calls(|c| sends(c).len() == 2).await), vec!["one", "three"]);
    let runs = w.state().await.runs(&thread);
    assert_eq!(runs[0].status, RunStatus::Completed);
    assert_eq!(runs[1].prompt, "three");
}

#[tokio::test]
async fn steering_goes_into_the_turn_when_the_provider_can() {
    let fake = manual("codex", true);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    assert_eq!(w.send(&thread, "also this", SendMode::Steer).await, Reply::Ok);
    assert_eq!(session.wait_calls(|c| c.len() == 2).await, vec![Call::Send("one".into()), Call::Steer("also this".into())]);
    let runs = w.wait("the steer", |s| Some(s.runs(&thread)).filter(|r| r[0].steers.len() == 1)).await;
    assert_eq!(runs[0].steers, vec!["also this"]);
    assert_eq!(runs.len(), 1);
}

#[tokio::test]
async fn steering_without_native_support_interrupts_and_restarts_the_same_run() {
    let fake = manual("claude", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("claude", Some("one"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    assert_eq!(w.send(&thread, "change of plan", SendMode::Steer).await, Reply::Ok);
    let calls = session.wait_calls(|c| sends(c).len() == 2).await;
    assert_eq!(calls, vec![Call::Send("one".into()), Call::Interrupt, Call::Send("change of plan".into())]);
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert_eq!(t.run_count, 1);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    let runs = w.state().await.runs(&thread);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);
    assert_eq!(runs[0].steers, vec!["change of plan"]);
}

#[tokio::test]
async fn interrupting_stops_the_run_and_holds_the_queue() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    w.send(&thread, "later", SendMode::Queue).await;
    assert_eq!(w.run(Command::RunInterrupt { thread: thread.clone() }).await, Reply::Ok);
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    assert_eq!(t.queued.len(), 1, "an interrupt holds the queue");
    let runs = w.state().await.runs(&thread);
    assert_eq!(runs[0].status, RunStatus::Interrupted);
    assert_eq!(sends(&session.calls()), vec!["one"]);
    // Interrupting an idle thread is fine.
    assert_eq!(w.run(Command::RunInterrupt { thread: thread.clone() }).await, Reply::Ok);
}

#[tokio::test]
async fn approvals_wait_and_are_answered_once() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("deploy"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Item(Item {
        id: "cmd".into(),
        parent: None,
        status: NodeStatus::Running,
        body: NodeBody::Tool(ToolCall {
            name: "commandExecution".into(),
            kind: ToolKind::Command,
            title: "rm -rf build".into(),
            input: serde_json::Value::Null,
            output: None,
            exit_code: None,
            files: Vec::new(),
        }),
    }));
    session.emit(ProviderEvent::Request {
        id: "rpc-7".into(),
        item: Some("cmd".into()),
        body: RequestBody::Approval(Approval {
            action: ApprovalAction::Command,
            title: "Run `rm -rf build`".into(),
            command: Some("rm -rf build".into()),
            cwd: None,
            files: Vec::new(),
            reason: None,
            tool: None,
            input: serde_json::Value::Null,
            decisions: vec![Decision::Accept, Decision::Decline],
        }),
    });
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    assert_eq!(t.pending_requests, 1);
    assert_eq!(t.activity.as_deref(), Some("Waiting for approval: Run `rm -rf build`"));
    let s = w.state().await;
    let request = s.requests(&thread).pop().unwrap();
    assert_eq!(request.status, RequestStatus::Pending);
    assert_eq!(s.runs(&thread)[0].status, RunStatus::Waiting);
    let node = s.nodes(&thread).into_iter().find(|n| n.id == request.node).unwrap();
    assert_eq!((node.status, node.body.clone()), (NodeStatus::Waiting, NodeBody::Request { request: request.id.clone() }));

    let answer = |decision| Command::RequestRespond {
        request: request.id.clone(),
        response: Response::Decision { decision, message: None },
    };
    let refused = w.run(answer(Decision::AcceptForSession)).await;
    assert!(matches!(refused, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })), "{refused:?}");
    assert_eq!(w.run(answer(Decision::Accept)).await, Reply::Ok);
    let calls = session.wait_calls(|c| c.len() == 2).await;
    assert_eq!(calls[1], Call::Respond {
        request: "rpc-7".into(),
        response: Response::Decision { decision: Decision::Accept, message: None }
    });
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert_eq!(t.pending_requests, 0);
    let request = w.state().await.requests(&thread).pop().unwrap();
    assert_eq!(request.status, RequestStatus::Resolved);
    assert_eq!(request.resolved_by.as_deref(), Some("owner"));
    let again = w.run(answer(Decision::Accept)).await;
    assert!(matches!(again, Reply::Error(ReplyError { code: ErrorCode::Conflict, .. })), "{again:?}");
}

#[tokio::test]
async fn a_withdrawn_request_is_cancelled() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("ask me"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Request {
        id: "q1".into(),
        item: None,
        body: RequestBody::Questions {
            questions: vec![Question {
                id: "color".into(),
                header: "Color".into(),
                question: "Which?".into(),
                options: Vec::new(),
                multi: false,
            }],
        },
    });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    session.emit(ProviderEvent::RequestCancelled { id: "q1".into() });
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert_eq!(t.pending_requests, 0);
    assert_eq!(w.state().await.requests(&thread)[0].status, RequestStatus::Cancelled);
}

#[tokio::test]
async fn command_ids_are_idempotent() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake.clone()]).await;
    let create = Command::ThreadCreate {
        project: w.project.clone(),
        provider: ProviderId::codex(),
        model: None,
        effort: None,
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        worktree: false,
        title: Some("Once".into()),
        prompt: Some("hi".into()),
        attachments: Vec::new(),
        orchestrator: false,
    };
    let (a, b) = tokio::join!(
        w.host.run("same".into(), "owner".into(), create.clone()),
        w.host.run("same".into(), "owner".into(), create.clone())
    );
    assert_eq!(a, b);
    let c = w.host.run("same".into(), "owner".into(), create.clone()).await;
    assert_eq!(a, c);
    let threads = w.state().await.live.keys().filter(|(k, _)| *k == EntityKind::Thread).count();
    assert_eq!(threads, 1);
    assert_eq!(fake.sessions().len(), 1);
    assert_eq!(sends(&fake.session(0).await.calls()).len(), 1);
    // Ids are per sender: another client's "same" is its own command.
    let other = w.host.run("same".into(), "agent:bot".into(), create.clone()).await;
    assert!(matches!(other, Reply::Created { .. }) && other != a, "{other:?}");
    assert_eq!(w.host.run("same".into(), "agent:bot".into(), create.clone()).await, other);
    let threads = w.state().await.live.keys().filter(|(k, _)| *k == EntityKind::Thread).count();
    assert_eq!(threads, 2);

    // A command the daemon was running when it stopped (started, never finished) doesn't run
    // again: its id answers `conflict`, for good.
    let store = agent_host::store::Store::open(&w.home.join("agent.db")).unwrap();
    assert_eq!(store.begin_command("owner", "cut", 1).unwrap(), agent_host::store::Receipt::New);
    let cut = w.host.run("cut".into(), "owner".into(), create.clone()).await;
    assert!(matches!(&cut, Reply::Error(ReplyError { code: ErrorCode::Conflict, message }) if message.contains("stopped")), "{cut:?}");
    assert_eq!(w.host.run("cut".into(), "owner".into(), create).await, cut);
    let threads = w.state().await.live.keys().filter(|(k, _)| *k == EntityKind::Thread).count();
    assert_eq!(threads, 2);
    // Errors are receipts too.
    let missing = Command::ProjectRemove { project: ProjectId("prj_missing".into()) };
    let first = w.host.run("err".into(), "owner".into(), missing.clone()).await;
    assert!(matches!(first, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
    assert_eq!(w.host.run("err".into(), "owner".into(), missing).await, first);
}

#[tokio::test]
async fn a_restart_interrupts_running_runs_and_resumes_the_native_thread() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("long job"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    // Events apply in order: once this node shows, the native thread id is stored.
    session.emit(ProviderEvent::Item(Item { id: "m".into(), parent: None, status: NodeStatus::Running, body: NodeBody::Message { text: "working".into() } }));
    w.wait("the node", |s| s.nodes(&thread).first().cloned()).await;
    let receipt = w.host.run("before".into(), "owner".into(), Command::ProviderRefresh {}).await;
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    drop(host);

    let fake2 = manual("codex", false);
    let host = open(&home, vec![fake2.clone()]).await;
    let s = state(&host).await;
    let t = s.thread(&thread).unwrap();
    assert_eq!((t.status, t.active_run.clone(), t.pending_requests), (ThreadStatus::Idle, None, 0));
    assert_eq!(s.runs(&thread)[0].status, RunStatus::Interrupted);
    assert_eq!(host.run("before".into(), "owner".into(), Command::ProviderRefresh {}).await, receipt);

    let reply = run(&host, Command::MessageSend {
        thread: thread.clone(),
        text: "carry on".into(),
        attachments: Vec::new(),
        mode: SendMode::Queue,
    })
    .await;
    assert!(matches!(reply, Reply::Created { .. }), "{reply:?}");
    let resumed = fake2.session(0).await;
    assert_eq!(resumed.config.resume.as_deref(), Some(session.native()));
    // The resumed conversation already holds run 1: nothing is handed over.
    assert_eq!(sends(&resumed.wait_calls(|c| c.len() == 1).await), vec!["carry on"]);
    drop(dirs);
}

fn approval(command: &str) -> RequestBody {
    RequestBody::Approval(Approval {
        action: ApprovalAction::Command,
        title: format!("Run `{command}`"),
        command: Some(command.into()),
        cwd: None,
        files: Vec::new(),
        reason: None,
        tool: None,
        input: serde_json::Value::Null,
        decisions: vec![Decision::Accept, Decision::Decline],
    })
}

/// A run waiting on an approval when the daemon stops (a sandbox going to sleep) stays waiting:
/// the stop parks it and closes the session, the restart keeps its request pending and its
/// queued message, and answering it resumes the provider's conversation with the decision, as a
/// new turn in the same run. The resumed turn's ask for the same command is answered at once,
/// and the queue drains after the run completes.
#[tokio::test]
async fn a_request_pending_when_the_daemon_stops_is_answered_after_a_restart() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("clean up"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    let tool = ToolCall {
        name: "commandExecution".into(),
        kind: ToolKind::Command,
        title: "rm -rf build".into(),
        input: serde_json::Value::Null,
        output: None,
        exit_code: None,
        files: Vec::new(),
    };
    session.emit(ProviderEvent::Item(Item { id: "cmd".into(), parent: None, status: NodeStatus::Running, body: NodeBody::Tool(tool) }));
    session.emit(ProviderEvent::Request { id: "rpc-7".into(), item: Some("cmd".into()), body: approval("rm -rf build") });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    assert!(matches!(w.send(&thread, "then the docs", SendMode::Queue).await, Reply::Created { .. }));

    // SIGTERM: the waiting run is parked as it is, the session closed.
    w.host.shutdown().await;
    assert!(session.calls().contains(&Call::Close));
    let check = |s: &Records| {
        let t = s.thread(&thread).unwrap();
        assert_eq!((t.status, t.pending_requests, t.queued.len()), (ThreadStatus::Waiting, 1, 1), "{t:?}");
        let run = &s.runs(&thread)[0];
        assert_eq!(run.status, RunStatus::Waiting);
        assert_eq!(t.active_run.as_ref(), Some(&run.id));
        let request = s.requests(&thread).pop().unwrap();
        assert_eq!(request.status, RequestStatus::Pending);
        let nodes = s.nodes(&thread);
        let waiting = nodes.iter().find(|n| n.id == request.node).unwrap();
        assert_eq!(waiting.status, NodeStatus::Waiting);
        assert!(nodes.iter().filter(|n| n.id != request.node).all(|n| n.status == NodeStatus::Interrupted), "{nodes:?}");
        request
    };
    let request = check(&w.state().await);
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    drop(host);

    // The next start: still waiting, still answerable.
    let fake2 = manual("codex", false);
    let host = open(&home, vec![fake2.clone()]).await;
    assert_eq!(check(&state(&host).await).id, request.id);
    let answer = Command::RequestRespond { request: request.id.clone(), response: Response::Decision { decision: Decision::Accept, message: None } };
    assert_eq!(run(&host, answer.clone()).await, Reply::Ok);
    let resumed = fake2.session(0).await;
    assert_eq!(resumed.config.resume.as_deref(), Some(session.native()));
    let said = sends(&resumed.wait_calls(|c| !sends(c).is_empty()).await);
    assert_eq!(said.len(), 1);
    assert!(said[0].contains("I approved it") && said[0].contains("rm -rf build") && !said[0].contains("clean up"), "the conversation has the prompt: {said:?}");
    let s = state(&host).await;
    let t = s.thread(&thread).unwrap();
    let runs = s.runs(&thread);
    assert_eq!((t.status, t.pending_requests, runs.len(), runs[0].status), (ThreadStatus::Running, 0, 1, RunStatus::Running));
    let answered = s.requests(&thread).into_iter().find(|r| r.id == request.id).unwrap();
    assert_eq!((answered.status, answered.resolved_by.as_deref()), (RequestStatus::Resolved, Some("owner")));
    assert!(s.nodes(&thread).iter().any(|n| matches!(&n.body, NodeBody::System { text } if text.contains("machine stopped"))), "the transcript says so");
    let again = run(&host, answer).await;
    assert!(matches!(again, Reply::Error(ReplyError { code: ErrorCode::Conflict, .. })), "{again:?}");

    // The agent asks to run it again: already approved.
    resumed.emit(ProviderEvent::Request { id: "rpc-9".into(), item: None, body: approval("rm -rf build") });
    resumed.wait_calls(|c| c.contains(&Call::Respond { request: "rpc-9".into(), response: Response::Decision { decision: Decision::Accept, message: None } })).await;
    let s = state(&host).await;
    assert!(s.requests(&thread).iter().all(|r| r.status == RequestStatus::Resolved && r.resolved_by.as_deref() == Some("owner")));
    // Something else still asks.
    resumed.emit(ProviderEvent::Request { id: "rpc-10".into(), item: None, body: approval("rm -rf dist") });
    wait(&host, |s| s.thread(&thread).filter(|t| t.status == ThreadStatus::Waiting).cloned(), "a new ask").await;
    resumed.emit(ProviderEvent::RequestCancelled { id: "rpc-10".into() });

    // The run completes; the queued message runs next.
    resumed.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    resumed.wait_calls(|c| sends(c).last().map(String::as_str) == Some("then the docs")).await;
    let runs = wait(&host, |s| Some(s.runs(&thread)).filter(|r| r.len() == 2), "the queued run").await;
    assert_eq!((runs[0].status, runs[0].ordinal), (RunStatus::Completed, 1));
    drop(dirs);
}

/// Questions pending at a restart are answered as a message on the resumed conversation (T3's
/// questions after a restart), a conversation that can't be resumed starts fresh with the run's
/// prompt in front, and a `cancel` decision just ends the parked run.
#[tokio::test]
async fn parked_questions_are_answered_as_a_message_and_cancel_ends_the_run() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("pick a color"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    let question = Question { id: "color".into(), header: "Color".into(), question: "Which color?".into(), options: Vec::new(), multi: false };
    session.emit(ProviderEvent::Request { id: "q1".into(), item: None, body: RequestBody::Questions { questions: vec![question] } });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    let other = w.thread("codex", Some("deploy"), false).await;
    let session2 = fake.session(1).await;
    session2.wait_calls(|c| c.len() == 1).await;
    let mut deploy = approval("deploy");
    if let RequestBody::Approval(a) = &mut deploy {
        a.decisions.push(Decision::Cancel);
    }
    session2.emit(ProviderEvent::Request { id: "a1".into(), item: None, body: deploy });
    w.wait_thread(&other, |t| t.status == ThreadStatus::Waiting).await;
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    // A crash this time (no shutdown): the same.
    drop(host);

    let fake2 = manual("codex", false);
    // The provider lost the conversation: the session starts fresh.
    fake2.fail_next_start("no such conversation");
    let host = open(&home, vec![fake2.clone()]).await;
    let s = state(&host).await;
    let q = s.requests(&thread).pop().unwrap();
    assert_eq!(q.status, RequestStatus::Pending);
    let answers = [("color".to_string(), "Blue".to_string())].into();
    assert_eq!(run(&host, Command::RequestRespond { request: q.id.clone(), response: Response::Answers { answers } }).await, Reply::Ok);
    let fresh = fake2.session(0).await;
    assert_eq!(fresh.config.resume, None);
    let said = sends(&fresh.wait_calls(|c| !sends(c).is_empty()).await);
    assert!(said[0].contains("pick a color") && said[0].contains("Which color?") && said[0].contains("Blue"), "{said:?}");

    // Cancel: the run ends, nothing is sent.
    let a = state(&host).await.requests(&other).pop().unwrap();
    let cancel = Command::RequestRespond { request: a.id, response: Response::Decision { decision: Decision::Cancel, message: None } };
    assert_eq!(run(&host, cancel).await, Reply::Ok);
    let s = state(&host).await;
    let t = s.thread(&other).unwrap();
    assert_eq!((t.status, t.active_run.clone()), (ThreadStatus::Idle, None));
    assert_eq!(s.runs(&other)[0].status, RunStatus::Interrupted);
    assert_eq!(s.requests(&other)[0].status, RequestStatus::Resolved);
    assert_eq!(fake2.sessions().len(), 1, "no session for a cancelled run");
    drop(dirs);
}

/// Interrupting a parked run ends it; the restart's queue stays held, like any interrupt.
#[tokio::test]
async fn a_parked_run_can_be_interrupted() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("wait for me"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Request { id: "a".into(), item: None, body: approval("make") });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    w.host.shutdown().await;
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    drop(host);
    let host = open(&home, vec![manual("codex", false)]).await;
    assert_eq!(run(&host, Command::RunInterrupt { thread: thread.clone() }).await, Reply::Ok);
    let s = state(&host).await;
    let t = s.thread(&thread).unwrap();
    assert_eq!((t.status, t.active_run.clone(), t.pending_requests), (ThreadStatus::Idle, None, 0));
    assert_eq!(s.runs(&thread)[0].status, RunStatus::Interrupted);
    assert_eq!(s.requests(&thread)[0].status, RequestStatus::Cancelled);
    drop(dirs);
}

#[tokio::test]
async fn a_provider_that_wont_start_fails_the_run() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", None, false).await;
    fake.fail_next_start("codex is not logged in");
    let reply = w.send(&thread, "hi", SendMode::Queue).await;
    assert!(matches!(&reply, Reply::Error(ReplyError { code: ErrorCode::Provider, message }) if message.contains("logged in")), "{reply:?}");
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Error).await;
    assert!(t.error.as_deref().unwrap().contains("logged in"));
    assert_eq!(w.state().await.runs(&thread)[0].status, RunStatus::Failed);
    // The next message starts a session and works.
    assert!(matches!(w.send(&thread, "again", SendMode::Queue).await, Reply::Created { .. }));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 2).await;
}

#[tokio::test]
async fn a_session_that_exits_mid_run_fails_it() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("go"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Exited { error: Some("segfault".into()) });
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Error).await;
    assert_eq!(t.error.as_deref(), Some("segfault"));
    // A new session is started (resuming) for the next message.
    w.send(&thread, "again", SendMode::Queue).await;
    let next = fake.session(1).await;
    assert_eq!(next.config.resume.as_deref(), Some(session.native()));
}

#[tokio::test]
async fn replay_and_subscribe_agree_and_never_repeat_a_record() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake.clone()]).await;
    let mut events = w.host.subscribe();
    let thread = w.thread("codex", Some("one"), false).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    w.send(&thread, "two", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 2).await;

    let mut live = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Outbound::Entity(s) = event {
            live.push(*s);
        }
    }
    assert!(live.windows(2).all(|w| w[0].seq < w[1].seq), "broadcasts are in seq order");
    let replay = w.host.since(0, usize::MAX).await.unwrap();
    assert!(replay.windows(2).all(|w| w[0].seq < w[1].seq));
    let keys: HashSet<_> = replay.iter().map(|s| (s.entity.kind(), s.entity.id().to_string())).collect();
    assert_eq!(keys.len(), replay.len(), "each record once");
    // The replay is exactly the newest broadcast of every record (the project came earlier).
    for stamped in replay.iter().filter(|s| s.entity.kind() != EntityKind::Project) {
        let newest = live.iter().rev().find(|l| l.entity.kind() == stamped.entity.kind() && l.entity.id() == stamped.entity.id());
        assert_eq!(newest, Some(stamped));
    }
    // Paging resumes after the last seq it saw.
    let first = w.host.since(0, 3).await.unwrap();
    let rest = w.host.since(first.last().unwrap().seq, usize::MAX).await.unwrap();
    assert_eq!(first.len() + rest.len(), replay.len());
}

#[tokio::test]
async fn accepting_a_plan_leaves_plan_mode() {
    let fake = FakeProvider::with("claude", Capabilities { mode_switch: true, plan_mode: true, ..Capabilities::default() }, Script::Manual);
    let w = World::new(vec![fake.clone()]).await;
    let Reply::Created { id } = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId::claude(),
            model: None,
            effort: None,
            runtime_mode: RuntimeMode::AutoEdit,
            interaction_mode: InteractionMode::Plan,
            worktree: false,
            title: None,
            prompt: Some("plan the refactor".into()),
            attachments: Vec::new(),
            orchestrator: false,
        })
        .await
    else {
        panic!("thread.create failed")
    };
    let thread = ThreadId(id);
    let session = fake.session(0).await;
    assert_eq!(session.config.interaction_mode, InteractionMode::Plan);
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Request {
        id: "plan-1".into(),
        item: None,
        body: RequestBody::Approval(Approval {
            action: ApprovalAction::Plan,
            title: "Carry out the plan".into(),
            command: None,
            cwd: None,
            files: Vec::new(),
            reason: None,
            tool: Some("ExitPlanMode".into()),
            input: serde_json::Value::Null,
            decisions: vec![Decision::Accept, Decision::Decline],
        }),
    });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
    let request = w.state().await.requests(&thread).pop().unwrap();
    let accept = Command::RequestRespond { request: request.id, response: Response::Decision { decision: Decision::Accept, message: None } };
    assert_eq!(w.run(accept).await, Reply::Ok);
    let t = w.wait_thread(&thread, |t| t.interaction_mode == InteractionMode::Default).await;
    assert_eq!(t.runtime_mode, RuntimeMode::AutoEdit);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;

    // The next run reuses the session as is: it already left plan mode on its own.
    w.run(Command::MessageSend { thread: thread.clone(), text: "go".into(), attachments: Vec::new(), mode: SendMode::Queue }).await;
    let calls = session.wait_calls(|c| sends(c).len() == 2).await;
    assert!(!calls.iter().any(|c| matches!(c, Call::SetModes(..))), "{calls:?}");
    assert_eq!(fake.sessions().len(), 1);
}

// ── project settings and auto-pull (docs/code/agents.md, "Project settings") ──

/// A bare remote whose `main` someone else lands work on (a merged PR, as the remote sees it),
/// and a daemon whose project is a clone of it.
struct Remote {
    host: std::sync::Arc<agent_host::Host>,
    project: ProjectId,
    /// The project's checkout.
    dir: std::path::PathBuf,
    /// Another clone, where merges come from.
    other: std::path::PathBuf,
    bare: std::path::PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

impl Remote {
    /// `pull_every`: the daemon's `[projects] pull_every` (0: no polling).
    async fn new(pull_every: u64, providers: Vec<std::sync::Arc<FakeProvider>>) -> Remote {
        let (home, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let (bare, seed, dir, other) = (root_path.join("remote.git"), root_path.join("seed"), root_path.join("checkout"), root_path.join("other"));
        for d in [&bare, &seed] {
            std::fs::create_dir(d).unwrap();
        }
        sh(&bare, &["init", "-q", "--bare", "-b", "main"]);
        git_repo(&seed);
        sh(&seed, &["push", "-q", bare.to_str().unwrap(), "main"]);
        for d in [&dir, &other] {
            sh(&root_path, &["clone", "-q", bare.to_str().unwrap(), d.to_str().unwrap()]);
            sh(d, &["config", "user.name", "Test"]);
            sh(d, &["config", "user.email", "[EMAIL]"]);
        }
        std::fs::write(home.path().join("config.toml"), format!("[projects]\npull_every = {pull_every}\n")).unwrap();
        let host = open(home.path(), providers).await;
        let Reply::Created { id } = run(&host, Command::ProjectAdd { path: dir.to_string_lossy().into(), name: None, clone: None }).await else {
            panic!("project.add failed")
        };
        Remote { host, project: ProjectId(id), dir, other, bare, _dirs: vec![home, root] }
    }

    /// Someone lands a commit adding `file` on the remote's main; returns the new head.
    fn land(&self, file: &str) -> String {
        std::fs::write(self.other.join(file), format!("{file}\n")).unwrap();
        sh(&self.other, &["add", "."]);
        sh(&self.other, &["commit", "-qm", &format!("Add {file}")]);
        sh(&self.other, &["push", "-q", "origin", "main"]);
        sh(&self.bare, &["rev-parse", "main"]).trim().to_string()
    }

    fn head(&self) -> String {
        sh(&self.dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    async fn record(&self) -> Project {
        match state(&self.host).await.live.get(&(EntityKind::Project, self.project.0.clone())) {
            Some(Entity::Project(p)) => p.clone(),
            other => panic!("no project record: {other:?}"),
        }
    }

    async fn settings(&self, settings: ProjectSettings) -> Reply {
        run(&self.host, Command::ProjectUpdate { project: self.project.clone(), name: None, settings: Some(settings) }).await
    }

    async fn pull(&self) -> PullOutcome {
        match run(&self.host, Command::ProjectPull { project: self.project.clone() }).await {
            Reply::Pulled(r) => r.outcome,
            other => panic!("project.pull: {other:?}"),
        }
    }

    /// Waits (5 s at most) for a record of the project whose pull result satisfies `f`, as it
    /// goes by: a later poll may replace it at once.
    async fn until(&self, events: &mut tokio::sync::broadcast::Receiver<Outbound>, what: &str, f: impl Fn(&PullOutcome) -> bool) -> PullOutcome {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ev = tokio::time::timeout_at(deadline, events.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Ok(Outbound::Entity(s)) = ev
                && let Entity::Project(Project { id, pull: Some(r), .. }) = &s.entity
                && *id == self.project
                && f(&r.outcome)
            {
                return r.outcome.clone();
            }
        }
    }
}

fn auto_pull() -> ProjectSettings {
    ProjectSettings { auto_pull: true, ..ProjectSettings::default() }
}

#[tokio::test]
async fn auto_pull_fast_forwards_the_default_branch_when_a_merge_lands() {
    let r = Remote::new(1, vec![FakeProvider::new("codex")]).await;
    let before = r.head();
    // Off by default: the checkout stays where it is.
    r.land("one.txt");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(r.head(), before);
    assert!(r.record().await.pull.is_none());

    // Turning it on pulls at once.
    let mut events = r.host.subscribe();
    assert_eq!(r.settings(auto_pull()).await, Reply::Ok);
    assert!(r.record().await.settings.auto_pull, "the reply comes after the settings are stored");
    let pulled = r.until(&mut events, "the first pull", |o| matches!(o, PullOutcome::Pulled { .. })).await;
    let PullOutcome::Pulled { branch, from, to, commits } = pulled else { unreachable!() };
    assert_eq!((branch.as_str(), from.as_str(), commits), ("main", before.as_str(), 1));
    assert_eq!(to, r.head());
    assert_eq!(std::fs::read_to_string(r.dir.join("one.txt")).unwrap(), "one.txt\n");

    // The next merge comes in on its own, and the project's git info follows.
    let head = r.land("two.txt");
    r.until(&mut events, "the second pull", |o| matches!(o, PullOutcome::Pulled { to, .. } if *to == head)).await;
    assert_eq!(r.head(), head);
    assert_eq!(r.record().await.git.and_then(|g| g.head), Some(head));
    assert!(sh(&r.dir, &["status", "--porcelain"]).is_empty());

    // Nothing new: up to date, said once.
    r.until(&mut events, "up to date", |o| matches!(o, PullOutcome::UpToDate { .. })).await;
    let seq = r.host.last_seq().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(r.host.last_seq().await.unwrap(), seq, "a project that stays up to date sends no records");

    // Off again: merges wait.
    assert_eq!(r.settings(ProjectSettings::default()).await, Reply::Ok);
    let head = r.head();
    r.land("three.txt");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(r.head(), head);
}

#[tokio::test]
async fn a_pull_touches_nothing_unless_it_is_a_clean_fast_forward() {
    let r = Remote::new(0, vec![FakeProvider::new("codex")]).await;
    let untouched = |r: &Remote, head: &str, index: &[u8], what: &str| {
        assert_eq!(r.head(), head, "{what}: HEAD moved");
        assert_eq!(std::fs::read(r.dir.join(".git/index")).unwrap(), index, "{what}: the index changed");
    };
    let index = || std::fs::read(r.dir.join(".git/index")).unwrap();

    // Nothing new.
    assert_eq!(r.pull().await, PullOutcome::UpToDate { branch: "main".into() });

    // Uncommitted changes, tracked or not: skipped, and the files stay as they are.
    r.land("a.txt");
    let head = r.head();
    std::fs::write(r.dir.join("README.md"), "mine\n").unwrap();
    let i = index();
    let skipped = r.pull().await;
    assert!(matches!(skipped, PullOutcome::Skipped { reason: PullSkip::LocalChanges, .. }), "{skipped:?}");
    untouched(&r, &head, &i, "a modified file");
    assert_eq!(std::fs::read_to_string(r.dir.join("README.md")).unwrap(), "mine\n");
    assert!(!r.dir.join("a.txt").exists());
    // The project says so.
    assert_eq!(r.record().await.pull.map(|p| p.outcome), Some(skipped));
    sh(&r.dir, &["checkout", "-q", "--", "README.md"]);
    std::fs::write(r.dir.join("notes.txt"), "scratch\n").unwrap();
    let i = index();
    assert!(matches!(r.pull().await, PullOutcome::Skipped { reason: PullSkip::LocalChanges, .. }));
    untouched(&r, &head, &i, "an untracked file");
    std::fs::remove_file(r.dir.join("notes.txt")).unwrap();

    // Clean: fast-forwarded.
    assert!(matches!(r.pull().await, PullOutcome::Pulled { commits: 1, .. }));
    assert!(r.dir.join("a.txt").exists());

    // A local commit the remote doesn't have: diverged, nothing moves.
    std::fs::write(r.dir.join("local.txt"), "local\n").unwrap();
    sh(&r.dir, &["add", "."]);
    sh(&r.dir, &["commit", "-qm", "local"]);
    r.land("b.txt");
    let (head, i) = (r.head(), index());
    assert!(matches!(r.pull().await, PullOutcome::Skipped { reason: PullSkip::Diverged, .. }));
    untouched(&r, &head, &i, "diverged");
    sh(&r.dir, &["reset", "-q", "--hard", "origin/main"]);

    // Another branch checked out, a detached HEAD, a branch without upstream: skipped.
    sh(&r.dir, &["checkout", "-q", "-b", "feature", "--track", "origin/main"]);
    r.land("c.txt");
    let (head, i) = (r.head(), index());
    let other = r.pull().await;
    assert!(matches!(&other, PullOutcome::Skipped { reason: PullSkip::OtherBranch, message } if message.contains("feature")), "{other:?}");
    untouched(&r, &head, &i, "another branch");
    sh(&r.dir, &["checkout", "-q", "--detach"]);
    assert!(matches!(r.pull().await, PullOutcome::Skipped { reason: PullSkip::Detached, .. }));
    sh(&r.dir, &["checkout", "-q", "main"]);
    sh(&r.dir, &["branch", "-q", "--unset-upstream"]);
    let (head, i) = (r.head(), index());
    assert!(matches!(r.pull().await, PullOutcome::Skipped { reason: PullSkip::NoUpstream, .. }));
    untouched(&r, &head, &i, "no upstream");
    sh(&r.dir, &["branch", "-q", "--set-upstream-to", "origin/main"]);

    // A remote that can't be reached: failed, nothing touched.
    sh(&r.dir, &["remote", "set-url", "origin", r.bare.join("gone").to_str().unwrap()]);
    let (head, i) = (r.head(), index());
    assert!(matches!(r.pull().await, PullOutcome::Failed { .. }));
    untouched(&r, &head, &i, "an unreachable remote");
    sh(&r.dir, &["remote", "set-url", "origin", r.bare.to_str().unwrap()]);
    assert!(matches!(r.pull().await, PullOutcome::Pulled { commits: 1, .. }));
}

#[tokio::test]
async fn a_checkout_an_agent_works_in_is_never_pulled_under_it() {
    let fake = manual("claude", false);
    let r = Remote::new(0, vec![fake.clone()]).await;
    let thread = match run(&r.host, Command::ThreadCreate {
        project: r.project.clone(),
        provider: ProviderId::claude(),
        model: None,
        effort: None,
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        worktree: false,
        title: None,
        prompt: Some("work".into()),
        attachments: Vec::new(),
        orchestrator: false,
    })
    .await
    {
        Reply::Created { id } => ThreadId(id),
        other => panic!("{other:?}"),
    };
    let session = fake.session(0).await;
    session.wait_calls(|c| !c.is_empty()).await;
    r.land("a.txt");
    let head = r.head();
    assert!(matches!(r.pull().await, PullOutcome::Skipped { reason: PullSkip::Busy, .. }));
    assert_eq!(r.head(), head);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    wait(&r.host, |s| s.thread(&thread).filter(|t| t.status == ThreadStatus::Idle).cloned(), "the run to end").await;
    assert!(matches!(r.pull().await, PullOutcome::Pulled { .. }));
}

#[tokio::test]
async fn project_settings_are_checked_and_the_setup_script_runs_in_new_worktrees() {
    let w = World::new(vec![FakeProvider::new("codex")]).await;
    let script = |id: &str, name: &str, command: &str, on_worktree: bool| ProjectScript {
        id: id.into(),
        name: name.into(),
        command: command.into(),
        icon: ScriptIcon::Play,
        on_worktree,
    };
    let settings = ProjectSettings {
        auto_pull: false,
        defaults: ThreadDefaults { provider: Some(ProviderId::codex()), worktree: Some(true), ..ThreadDefaults::default() },
        scripts: vec![
            script("setup", "Install", "printf '%s|%s' \"$ROLL_PROJECT_ROOT\" \"$ROLL_WORKTREE_PATH\" > setup.out", true),
            script("lint", "Lint", "printf '%s' \"$ROLL_PROJECT_ROOT\" > lint.out", false),
        ],
        auto_settle: AutoSettle { after_days: Some(7), on_land: false },
    };
    let update = |name: Option<&str>, settings: Option<ProjectSettings>| Command::ProjectUpdate {
        project: w.project.clone(),
        name: name.map(String::from),
        settings,
    };
    assert_eq!(w.run(update(Some("  App  "), Some(settings.clone()))).await, Reply::Ok);
    let project = w.wait("the settings", |s| match s.live.get(&(EntityKind::Project, w.project.0.clone())) {
        Some(Entity::Project(p)) if p.name == "App" => Some(p.clone()),
        _ => None,
    }).await;
    assert_eq!(project.settings, settings);

    // Bad settings are refused, and nothing changes.
    let mut twice = settings.clone();
    twice.scripts.push(script("lint", "Again", "true", false));
    for bad in [twice, ProjectSettings { scripts: vec![script("x y", "X", "true", false)], ..settings.clone() }, ProjectSettings {
        scripts: vec![script("empty", "Empty", "  ", false)],
        ..settings.clone()
    }] {
        let refused = w.run(update(None, Some(bad))).await;
        assert!(matches!(refused, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })), "{refused:?}");
    }
    assert!(matches!(w.run(update(Some(" "), None)).await, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })));
    assert!(matches!(
        w.run(Command::ProjectUpdate { project: ProjectId("prj_nope".into()), name: None, settings: None }).await,
        Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })
    ));

    // A new worktree gets the setup script in a terminal, told where it runs.
    let thread = w.thread("codex", None, true).await;
    let worktree = w.state().await.thread(&thread).unwrap().worktree.clone().unwrap();
    let terminal = w.wait("the setup terminal", |s| {
        s.live.values().find_map(|e| match e {
            Entity::Terminal(t) if t.thread.as_ref() == Some(&thread) => Some(t.clone()),
            _ => None,
        })
    }).await;
    assert_eq!((terminal.title.as_str(), terminal.cwd.as_str()), ("Install", worktree.path.as_str()));
    let out = std::path::Path::new(&worktree.path).join("setup.out");
    let expected = format!("{}|{}", w.repo.display(), worktree.path);
    w.wait("the setup script's output", |_| std::fs::read_to_string(&out).ok().filter(|s| *s == expected)).await;
    // A thread in the checkout doesn't.
    let local = w.thread("codex", None, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!w.state().await.live.values().any(|e| matches!(e, Entity::Terminal(t) if t.thread.as_ref() == Some(&local))));

    // Any script runs by id in a terminal.
    let open = |script: &str| Command::TerminalOpen { project: Some(w.project.clone()), thread: None, command: None, script: Some(script.into()), cols: 80, rows: 24 };
    assert!(matches!(w.run(open("lint")).await, Reply::Created { .. }));
    let lint = w.repo.join("lint.out");
    w.wait("the lint script's output", |_| std::fs::read_to_string(&lint).ok().filter(|s| *s == w.repo.to_string_lossy())).await;
    assert!(matches!(w.run(open("nope")).await, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
    let both = Command::TerminalOpen { project: Some(w.project.clone()), thread: None, command: Some("ls".into()), script: Some("lint".into()), cols: 80, rows: 24 };
    assert!(matches!(w.run(both).await, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })));
}
