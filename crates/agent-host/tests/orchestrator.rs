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
    };
    let (a, b) = tokio::join!(
        w.host.run("same".into(), "owner".into(), create.clone()),
        w.host.run("same".into(), "owner".into(), create.clone())
    );
    assert_eq!(a, b);
    let c = w.host.run("same".into(), "owner".into(), create).await;
    assert_eq!(a, c);
    let threads = w.state().await.live.keys().filter(|(k, _)| *k == EntityKind::Thread).count();
    assert_eq!(threads, 1);
    assert_eq!(fake.sessions().len(), 1);
    assert_eq!(sends(&fake.session(0).await.calls()).len(), 1);
    // Errors are receipts too.
    let missing = Command::ProjectRemove { project: ProjectId("prj_missing".into()) };
    let first = w.host.run("err".into(), "owner".into(), missing.clone()).await;
    assert!(matches!(first, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
    assert_eq!(w.host.run("err".into(), "owner".into(), missing).await, first);
}

#[tokio::test]
async fn a_restart_interrupts_runs_cancels_requests_and_resumes_the_native_thread() {
    let fake = manual("codex", false);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("long job"), false).await;
    let session = fake.session(0).await;
    session.wait_calls(|c| c.len() == 1).await;
    session.emit(ProviderEvent::Request {
        id: "r".into(),
        item: None,
        body: RequestBody::Questions { questions: Vec::new() },
    });
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Waiting).await;
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
    assert_eq!(s.requests(&thread)[0].status, RequestStatus::Cancelled);
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
