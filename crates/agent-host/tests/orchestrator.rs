//! The orchestrator on fake providers: runs, items, queueing, steering, interrupts, requests,
//! idempotent commands, restart recovery, and the replay/subscribe contract.

mod common;

use agent_host::provider::fake::{Call, FakeProvider, Script};
use agent_host::provider::{Item, ProviderEvent, TurnEnd};
use agent_host::{Daemon, Outbound};
use agent_proto::*;
use common::*;
use std::collections::HashSet;
use std::time::Duration;

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
        body: NodeBody::Subagent { agent: "explorer".into(), prompt: "look around".into(), child: None, result: None, progress: None, model: None },
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

fn queued_texts(t: &Thread) -> Vec<&str> {
    t.queued.iter().map(|m| m.text.as_str()).collect()
}

/// T3's queue controls: edit, reorder, remove, and "Send now" (promote to steer while a run is
/// going; start the next run on a held queue, after which the rest drains).
#[tokio::test]
async fn queued_messages_are_edited_reordered_and_sent_now() {
    let fake = manual("codex", true);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    let mut ids = Vec::new();
    for text in ["two", "three", "four"] {
        let Reply::Created { id } = w.send(&thread, text, SendMode::Queue).await else { panic!() };
        ids.push(id);
    }
    let edit = |message: &str, text: &str| Command::MessageEdit { thread: thread.clone(), message: message.into(), text: text.into(), attachments: None };
    assert_eq!(w.run(edit(&ids[0], "two, edited")).await, Reply::Ok);
    let empty = w.run(edit(&ids[0], "  ")).await;
    assert!(matches!(empty, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })), "{empty:?}");
    let reorder = |message: &str, before: Option<&str>| Command::MessageReorder { thread: thread.clone(), message: message.into(), before: before.map(Into::into) };
    assert_eq!(w.run(reorder(&ids[2], Some(&ids[0]))).await, Reply::Ok);
    let t = w.wait_thread(&thread, |t| t.queued.first().is_some_and(|m| m.text == "four")).await;
    assert_eq!(queued_texts(&t), ["four", "two, edited", "three"]);
    assert_eq!(w.run(reorder(&ids[2], None)).await, Reply::Ok);
    let t = w.wait_thread(&thread, |t| t.queued.last().is_some_and(|m| m.text == "four")).await;
    assert_eq!(queued_texts(&t), ["two, edited", "three", "four"]);
    let unknown = w.run(reorder(&ids[0], Some("msg_nope"))).await;
    assert!(matches!(unknown, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })), "{unknown:?}");

    // Send now, while the run goes: it steers the run and leaves the queue.
    let promote = |message: &str| Command::MessagePromote { thread: thread.clone(), message: message.into() };
    assert_eq!(w.run(promote(&ids[1])).await, Reply::Ok);
    assert_eq!(session.wait_calls(|c| c.len() == 2).await[1], Call::Steer("three".into()));
    let t = w.wait_thread(&thread, |t| t.queued.len() == 2).await;
    assert_eq!(queued_texts(&t), ["two, edited", "four"]);
    assert_eq!(w.state().await.runs(&thread)[0].steers, vec!["three"]);

    // Stopped, the queue is held; Send now starts that message, and the rest drains after it.
    assert_eq!(w.run(Command::RunInterrupt { thread: thread.clone() }).await, Reply::Ok);
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    let Reply::Created { .. } = w.run(promote(&ids[2])).await else { panic!("a run starts") };
    let t = w.wait_thread(&thread, |t| t.run_count == 2).await;
    assert_eq!(queued_texts(&t), ["two, edited"]);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    let t = w.wait_thread(&thread, |t| t.run_count == 3).await;
    assert!(t.queued.is_empty());
    assert_eq!(sends(&session.wait_calls(|c| sends(c).len() == 3).await), ["one", "four", "two, edited"]);
}

/// T3's `/compact`: a run whose provider compacts instead of taking a turn; steering it queues
/// (a compaction can't be steered), and its row gets the context before and after from the
/// thread's reports when the provider doesn't say (Codex).
#[tokio::test]
async fn compact_runs_as_a_compaction_and_steering_it_queues() {
    let caps = Capabilities { steer: true, compact: true, ..Capabilities::default() };
    let fake = FakeProvider::with("codex", caps, Script::Manual);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    session.emit(ProviderEvent::Context(ContextUsage { used_tokens: 180_000, max_tokens: Some(258_000), total_processed_tokens: None, ..Default::default() }));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.context.is_some()).await;

    let Reply::Created { .. } = w.send(&thread, " /compact ", SendMode::Queue).await else { panic!() };
    assert_eq!(session.wait_calls(|c| c.len() == 2).await[1], Call::Compact);
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    // Steering a compaction queues the message instead; Send now refuses and keeps its place.
    let Reply::Created { id } = w.send(&thread, "then this", SendMode::Steer).await else { panic!("queued") };
    let refused = w.run(Command::MessagePromote { thread: thread.clone(), message: id }).await;
    assert!(matches!(refused, Reply::Error(ReplyError { code: ErrorCode::Conflict, .. })), "{refused:?}");
    assert_eq!(w.wait_thread(&thread, |t| t.queued.len() == 1).await.queued[0].text, "then this");

    let item = |status| ProviderEvent::Item(Item { id: "cc".into(), parent: None, status, body: NodeBody::Compaction { before_tokens: None, after_tokens: None, error: None } });
    session.emit(item(NodeStatus::Running));
    session.emit(item(NodeStatus::Completed));
    session.emit(ProviderEvent::Context(ContextUsage { used_tokens: 30_000, max_tokens: Some(258_000), total_processed_tokens: None, ..Default::default() }));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    // The queue drains after it: the steer went in as the next run.
    let t = w.wait_thread(&thread, |t| t.run_count == 3).await;
    assert_eq!(t.context.unwrap().used_tokens, 30_000);
    assert_eq!(sends(&session.wait_calls(|c| sends(c).len() == 2).await), ["one", "then this"]);
    let s = w.state().await;
    let runs = s.runs(&thread);
    assert_eq!((runs[1].prompt.as_str(), runs[1].status), (" /compact ", RunStatus::Completed));
    let compaction = s.nodes(&thread).into_iter().find(|n| matches!(n.body, NodeBody::Compaction { .. })).unwrap();
    assert_eq!(compaction.body, NodeBody::Compaction { before_tokens: Some(180_000), after_tokens: Some(30_000), error: None });
    assert_eq!(compaction.run, runs[1].id);
}

/// T3's idle compaction before the prompt cache expires: with the machine set to
/// `before_cache_expires`, a big conversation whose run reported a cache lifetime compacts by
/// itself two minutes before it runs out, as a run marked `idle_compaction`; once, not again after
/// that compaction. With the default (on return) nothing happens by itself.
#[tokio::test]
async fn an_idle_conversation_compacts_before_its_prompt_cache_expires() {
    let caps = Capabilities { compact: true, ..Capabilities::default() };
    let fake = FakeProvider::with("claude", caps, Script::Manual);
    let w = World::new(vec![fake.clone()]).await;
    // The cache lifetime puts the window's start 1.5 s after the run (it opens two minutes early).
    let context = ContextUsage { used_tokens: 250_000, max_tokens: Some(1_000_000), cache_ttl_ms: Some(120_000 + 1_500), ..Default::default() };

    // On return (the default): the daemon leaves it alone.
    let quiet = w.thread("claude", Some("one"), false).await;
    let session = fake.session(0).await;
    session.emit(ProviderEvent::Context(context));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&quiet, |t| t.status == ThreadStatus::Idle).await;

    let settings = MachineSettings { idle_compaction: Some(IdleCompaction::BeforeCacheExpires), ..MachineSettings::default() };
    assert_eq!(w.run(Command::MachineConfigure { settings: Box::new(settings) }).await, Reply::Ok);
    let thread = w.thread("claude", Some("refactor the store"), false).await;
    let session = fake.session(1).await;
    session.emit(ProviderEvent::Context(context));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    assert_eq!(session.wait_calls(|c| c.contains(&Call::Compact)).await.last(), Some(&Call::Compact));
    let t = w.wait_thread(&thread, |t| t.run_count == 2).await;
    let runs = w.state().await.runs(&thread);
    assert!(runs[1].idle_compaction && !runs[0].idle_compaction);
    assert_eq!(runs[1].prompt, "/compact");
    session.emit(ProviderEvent::Context(ContextUsage { used_tokens: 40_000, ..context }));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 2).await;
    assert_eq!(t.provider, ProviderId::claude());

    // A compaction is never compacted again, and the quiet thread stayed quiet.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    assert_eq!(w.state().await.runs(&thread).len(), 2);
    assert_eq!(w.state().await.runs(&quiet).len(), 1);
    assert!(!fake.session(0).await.calls().contains(&Call::Compact));
}

/// On a provider that can't compact, `/compact` is an ordinary message.
#[tokio::test]
async fn compact_is_a_message_where_the_provider_cant() {
    let fake = manual("acp:grok", false);
    let w = World::new(vec![fake.clone()]).await;
    w.thread("acp:grok", Some("/compact"), false).await;
    assert_eq!(fake.session(0).await.wait_calls(|c| !c.is_empty()).await, [Call::Send("/compact".into())]);
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
        options: Default::default(),
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

/// T3's live configuration for an ACP agent: its probe can't list models without a session, so
/// what a session offered (`ProviderEvent::Catalog`) is what the machine offers from then on,
/// published at once and kept across a restart for the same version.
#[tokio::test]
async fn a_sessions_catalog_fills_in_a_probe_without_models() {
    let fake = FakeProvider::with("acp:fake", Capabilities::default(), Script::Manual);
    fake.unlist_models();
    let w = World::new(vec![fake.clone()]).await;
    let models = |info: &MachineInfo| info.providers.iter().find(|p| p.id.0 == "acp:fake").map(|p| p.models.clone()).unwrap_or_default();
    assert!(models(&w.host.info().await).is_empty());

    let thread = w.thread("acp:fake", Some("hi"), false).await;
    let session = fake.session(0).await;
    let offered = vec![ModelInfo {
        default: true,
        options: vec![ModelOption::reasoning(vec![OptionChoice::new("low", "Low"), OptionChoice::new("high", "High")], Some("high"))],
        ..ModelInfo::new("grok-4.7", "Grok 4.7")
    }];
    session.emit(ProviderEvent::Catalog { models: offered.clone() });
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    // Published by itself: the next hello says them.
    assert_eq!(offered_soon(&w.host, models).await, offered);

    // A restart probes again (no models) and still offers them, for the same version.
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    drop(host);
    let fake2 = FakeProvider::with("acp:fake", Capabilities::default(), Script::Manual);
    fake2.unlist_models();
    let host = open(&home, vec![fake2]).await;
    assert_eq!(offered_soon(&host, models).await, offered);
    drop(dirs);
}

/// What the machine info offers once `pick` finds something in it (at most 10 s).
async fn offered_soon(host: &agent_host::Host, pick: impl Fn(&MachineInfo) -> Vec<ModelInfo>) -> Vec<ModelInfo> {
    for _ in 0..200 {
        let info = host.info().await;
        let found = pick(&info);
        if !found.is_empty() && info.providers.iter().all(|p| !p.checking) {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    pick(&host.info().await)
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
            options: Default::default(),
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

/// T3's `hasActionableProposedPlan`: a run that completes with a plan (Codex's plan item) marks
/// the thread `plan_ready`, so the canvas can say "Plan ready" without loading its runs; the next
/// run clears it, and a run without a plan never sets it.
#[tokio::test]
async fn a_completed_plan_is_ready_until_the_next_run() {
    let fake = FakeProvider::with("codex", Capabilities { plan_mode: true, ..Capabilities::default() }, Script::Manual);
    let w = World::new(vec![fake.clone()]).await;
    let Reply::Created { id } = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId::codex(),
            model: None,
            effort: None,
            options: Default::default(),
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
    session.wait_calls(|c| c.len() == 1).await;
    let plan = |status| ProviderEvent::Item(Item { id: "plan".into(), parent: None, status, body: NodeBody::Plan { text: "# Refactor\n\n1. Split the module".into() } });
    session.emit(plan(NodeStatus::Running));
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert!(!t.plan_ready, "a plan still forming isn't ready");
    session.emit(plan(NodeStatus::Completed));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    assert!(t.plan_ready);

    // The next run (Refine, or Implement) retires it; a run without a plan doesn't bring it back.
    w.run(Command::MessageSend { thread: thread.clone(), text: "shorter".into(), attachments: Vec::new(), mode: SendMode::Queue }).await;
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert!(!t.plan_ready);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 2).await;
    assert!(!t.plan_ready);
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
        options: Default::default(),
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
async fn a_project_carries_its_logo_and_its_icon_setting_picks_another() {
    let (home, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::write(root.join("public/favicon.svg"), "<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
    let host = open(home.path(), vec![FakeProvider::new("codex")]).await;
    let Reply::Created { id } = run(&host, Command::ProjectAdd { path: root.to_string_lossy().into(), name: None, clone: None }).await else {
        panic!("project.add failed")
    };
    let project = ProjectId(id);
    let record = |s: &common::Records| match s.live.get(&(EntityKind::Project, project.0.clone())) {
        Some(Entity::Project(p)) => p.clone(),
        other => panic!("no project record: {other:?}"),
    };
    let logo = record(&state(&host).await).logo.expect("the favicon as the project's logo");
    assert_eq!(logo.path, "public/favicon.svg");
    assert!(logo.url.starts_with("data:image/svg+xml;base64,"));

    // A picture of its own: read, shrunk and sent as a PNG.
    std::fs::create_dir_all(root.join("assets")).unwrap();
    image::RgbaImage::from_pixel(200, 200, image::Rgba([20, 160, 140, 255])).save(root.join("assets/logo.png")).unwrap();
    let settings = |icon| ProjectSettings { color: Some(IdentityColor::Teal), icon, ..ProjectSettings::default() };
    let update = |s| Command::ProjectUpdate { project: project.clone(), name: None, settings: Some(s) };
    assert_eq!(run(&host, update(settings(ProjectIcon::File { path: "./assets/logo.png".into() }))).await, Reply::Ok);
    let p = record(&state(&host).await);
    assert_eq!(p.settings.icon, ProjectIcon::File { path: "assets/logo.png".into() });
    assert_eq!(p.settings.color, Some(IdentityColor::Teal));
    let logo = p.logo.unwrap();
    assert_eq!(logo.path, "assets/logo.png");
    assert!(logo.url.starts_with("data:image/png;base64,") && logo.url.len() <= ProjectLogo::MAX_URL);

    // Back to automatic: the favicon again.
    assert_eq!(run(&host, update(settings(ProjectIcon::Auto))).await, Reply::Ok);
    assert_eq!(record(&state(&host).await).logo.unwrap().path, "public/favicon.svg");

    // Icons that aren't one are refused, and nothing changes.
    for bad in [
        ProjectIcon::File { path: "../outside.png".into() },
        ProjectIcon::File { path: "notes.txt".into() },
        ProjectIcon::Monogram { text: Some("TOO LONG".into()) },
        ProjectIcon::Emoji { emoji: "abc".into() },
    ] {
        let refused = run(&host, update(settings(bad))).await;
        assert!(matches!(refused, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })), "{refused:?}");
    }
    assert_eq!(record(&state(&host).await).settings.icon, ProjectIcon::Auto);
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
        auto_settle: AutoSettle { after_days: Some(7), on_land: false, keep_changes: true },
        cleanup: Cleanup { worktree_after_days: Some(0), keep: CleanupKeep::AnyLocalFiles },
        ..ProjectSettings::default()
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
    let mut effort_option = settings.clone();
    effort_option.defaults.options = ModelOptions::from([("effort".to_string(), OptionValue::Choice("high".into()))]);
    let forever = ProjectSettings { cleanup: Cleanup { worktree_after_days: Some(Cleanup::MAX_DAYS + 1), ..Cleanup::default() }, ..settings.clone() };
    for bad in [twice, effort_option, forever, ProjectSettings { scripts: vec![script("x y", "X", "true", false)], ..settings.clone() }, ProjectSettings {
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


// ── machine settings ──

/// What the daemon's `config.toml` says about Claude in the settings tests: a key, a binary, an
/// argument and a variable.
const CLAUDE_FILE: &str = "[providers.claude]\nbinary = \"/usr/local/bin/claude\"\nargs = [\"--from-file\"]\nenv = { ANTHROPIC_API_KEY = \"sk-ant-file-only\", CLAUDE_CONFIG_DIR = \"/file\" }\n";

fn configure(settings: MachineSettings) -> Command {
    Command::MachineConfigure { settings: Box::new(settings) }
}

#[tokio::test]
async fn machine_settings_are_checked_stored_applied_and_reported_without_credentials() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_repo(repo.path());
    std::fs::write(home.path().join("config.toml"), CLAUDE_FILE).unwrap();
    let (codex, claude) = (FakeProvider::new("codex"), FakeProvider::new("claude"));
    let host = open(home.path(), vec![codex.clone(), claude.clone()]).await;
    let Reply::Created { id } = run(&host, Command::ProjectAdd { path: repo.path().to_string_lossy().into(), name: None, clone: None }).await else {
        panic!("project.add failed")
    };
    let project = ProjectId(id);
    let create = |provider: &str, worktree: bool| Command::ThreadCreate {
        project: project.clone(),
        provider: ProviderId(provider.into()),
        model: Some("big-1".into()),
        effort: None,
        options: Default::default(),
        runtime_mode: RuntimeMode::FullAccess,
        interaction_mode: InteractionMode::Default,
        worktree,
        title: Some("Settings".into()),
        prompt: Some("go".into()),
        attachments: Vec::new(),
        orchestrator: false,
    };
    let ids = |info: &MachineInfo| info.providers.iter().map(|p| p.id.0.clone()).collect::<Vec<_>>();

    // Before anything is set: everything on offer, and config.toml reported by names only.
    let info = host.info().await;
    assert_eq!(ids(&info), ["claude", "codex"]);
    let config = info.settings.clone().expect("the settings are in the info");
    assert_eq!(config.set, MachineSettings::default());
    let file = config.providers.iter().find(|p| p.id == ProviderId::claude()).unwrap();
    assert_eq!((file.binary.as_deref(), file.args, file.env.clone()), (Some("/usr/local/bin/claude"), 1, vec!["ANTHROPIC_API_KEY".to_string(), "CLAUDE_CONFIG_DIR".into()]));
    assert_eq!(config.base.branch_prefix, "roll");
    assert_eq!(config.base.worktree_dir, home.path().join("worktrees").display().to_string());
    assert_eq!(config.config_path, home.path().join("config.toml").display().to_string());
    assert!(!serde_json::to_string(&info).unwrap().contains("sk-ant-file-only"), "a credential never leaves the machine");
    assert_eq!(claude.configured().unwrap().binary, Some("/usr/local/bin/claude".into()));

    // A credential, an address or an unknown provider is refused, and nothing changes.
    let mut leaky = MachineSettings::default();
    leaky.providers.insert(ProviderId::claude(), ProviderSettings { env: [("ANTHROPIC_BASE_URL".to_string(), "https://evil.test".to_string())].into(), ..ProviderSettings::default() });
    let Reply::Error(e) = run(&host, configure(leaky)).await else { panic!("a base URL was accepted") };
    assert_eq!(e.code, ErrorCode::Invalid);
    assert!(e.message.contains("config.toml"), "{}", e.message);
    let mut unknown = MachineSettings::default();
    unknown.providers.insert(ProviderId("acp:nope".into()), ProviderSettings { enabled: false, ..ProviderSettings::default() });
    assert!(matches!(run(&host, configure(unknown)).await, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })));
    assert_eq!(host.info().await.settings.unwrap().set, MachineSettings::default());

    // Codex off; Claude with its own binary, an argument, a variable over the file's and a model.
    let wt = home.path().join("wt");
    let mut settings = MachineSettings {
        worktree_dir: Some(wt.display().to_string()),
        branch_prefix: Some("ada".into()),
        max_depth: Some(1),
        ..MachineSettings::default()
    };
    settings.providers.insert(ProviderId::codex(), ProviderSettings { enabled: false, ..ProviderSettings::default() });
    settings.providers.insert(
        ProviderId::claude(),
        ProviderSettings {
            binary: Some("/opt/claude".into()),
            args: vec!["--verbose".into()],
            env: [("CLAUDE_CONFIG_DIR".to_string(), "/set".to_string())].into(),
            models: vec![
                CustomModel { id: "big-1".into(), name: Some("Big".into()), options: None },
                CustomModel {
                    id: "big-2".into(),
                    name: None,
                    options: Some(vec![ModelOption::Boolean { id: "turbo".into(), label: "Turbo".into(), description: None, default: true, role: None }]),
                },
            ],
            ..ProviderSettings::default()
        },
    );
    let mut events = host.subscribe();
    assert_eq!(run(&host, configure(settings.clone())).await, Reply::Ok);
    // `hello` went out again with them before the reply.
    // (A start-up probe's `hello` may come first, with the old settings.)
    let sent: Vec<MachineInfo> = std::iter::from_fn(|| events.try_recv().ok()).filter_map(|o| if let Outbound::Info(i) = o { Some(i) } else { None }).collect();
    assert!(sent.iter().any(|i| i.settings.as_ref().unwrap().set == settings), "hello again with them: {sent:?}");
    let info = host.info().await;
    assert_eq!(ids(&info), ["claude"], "a provider turned off isn't offered");
    let big = info.providers[0].models.iter().find(|m| m.id == "big-1").expect("the custom model is listed");
    assert!(big.custom && big.name == "Big");
    // Options the owner described are the model's (T3's custom capabilities); none: the provider's.
    let big2 = info.providers[0].models.iter().find(|m| m.id == "big-2").expect("the second custom model");
    assert_eq!(big2.options, vec![ModelOption::Boolean { id: "turbo".into(), label: "Turbo".into(), description: None, default: true, role: None }]);
    assert!(big.options.is_empty());
    let applied = claude.configured().unwrap();
    assert_eq!(applied.binary, Some("/opt/claude".into()));
    assert_eq!(applied.args, vec!["--from-file", "--verbose"]);
    assert!(applied.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/set".into())) && !applied.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/file".into())));
    assert!(applied.env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"), "the file's key stays on the machine and in use");

    // New work: never on Codex; Claude's sessions get the variables, worktrees go where they're told.
    let Reply::Error(e) = run(&host, create("codex", false)).await else { panic!("codex was offered") };
    assert_eq!(e.code, ErrorCode::Provider);
    assert!(e.message.contains("turned off"), "{}", e.message);
    let Reply::Created { id } = run(&host, create("claude", true)).await else { panic!("thread.create on claude") };
    let session = claude.session(0).await;
    assert!(session.config.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/set".into())));
    let thread = state(&host).await.thread(&ThreadId(id)).cloned().unwrap();
    let worktree = thread.worktree.unwrap();
    assert!(worktree.path.starts_with(&wt.display().to_string()), "{}", worktree.path);
    assert!(worktree.branch.starts_with("ada/"), "{}", worktree.branch);

    // Kept across a restart.
    drop(session);
    drop(host);
    let (codex2, claude2) = (FakeProvider::new("codex"), FakeProvider::new("claude"));
    let host = open(home.path(), vec![codex2, claude2.clone()]).await;
    let info = host.info().await;
    assert_eq!(info.settings.as_ref().unwrap().set, settings);
    assert_eq!(ids(&info), ["claude"]);
    assert_eq!(claude2.configured().unwrap().binary, Some("/opt/claude".into()));

    // Turned back on, offered again.
    assert_eq!(run(&host, configure(MachineSettings::default())).await, Reply::Ok);
    assert_eq!(ids(&host.info().await), ["claude", "codex"]);
}

/// The daemon opens without waiting for its providers' probes: each is `checking` until its probe
/// answers (and `hello` goes out again then), a restart starts from the last probe, and
/// `provider.refresh` replies once its probes have answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_status_fills_in_as_probes_answer() {
    let home = tempfile::tempdir().unwrap();
    let codex = FakeProvider::new("codex");
    codex.hold_probes();
    let host = open(home.path(), vec![codex.clone()]).await;
    let p = host.info().await.providers[0].clone();
    assert!(p.checking && !p.installed && p.auth == AuthStatus::Unknown, "nothing known yet: {p:?}");
    let mut events = host.subscribe();
    codex.release_probes();
    let probed = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("hello again").unwrap() {
            Outbound::Info(info) if !info.providers[0].checking => break info.providers[0].clone(),
            _ => {}
        }
    };
    assert!(probed.installed && probed.name == "Fake codex", "{probed:?}");

    // Checking again: the last probe stays on show, and the reply waits for the new one.
    codex.hold_probes();
    let h = host.clone();
    let refresh = tokio::spawn(async move { run(&h, Command::ProviderRefresh {}).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!refresh.is_finished(), "replied before the probe answered");
    let p = host.info().await.providers[0].clone();
    assert!(p.checking && p.installed && p.name == "Fake codex", "{p:?}");
    codex.release_probes();
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), refresh).await.unwrap().unwrap(), Reply::Ok);
    assert!(!host.info().await.providers[0].checking);

    // After a restart, the last probe until the new one answers.
    drop(host);
    let codex = FakeProvider::new("codex");
    codex.hold_probes();
    let host = open(home.path(), vec![codex.clone()]).await;
    let p = host.info().await.providers[0].clone();
    assert!(p.checking && p.installed && p.name == "Fake codex", "{p:?}");
    codex.release_probes();
}

/// A CLIProxyAPI stand-in: `/v1/models` lists two models for the right bearer key, 401 otherwise,
/// and remembers every `Authorization` it was sent.
async fn mock_cliproxy(key: &'static str) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move |headers: HeaderMap| {
            let log = log.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                log.lock().unwrap().push(auth.clone());
                if auth == format!("Bearer {key}") {
                    axum::Json(serde_json::json!({ "object": "list", "data": [{ "id": "gemini-2.5-pro", "owned_by": "google" }, { "id": "gpt-5.5" }] })).into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn endpoint_set(spec: &EndpointSpec, key: KeyChange) -> Command {
    Command::EndpointSet { endpoint: spec.clone(), key }
}

fn invalid(reply: Reply) -> String {
    match reply {
        Reply::Error(ReplyError { code: ErrorCode::Invalid, message }) => message,
        other => panic!("expected invalid, got {other:?}"),
    }
}

/// Endpoints (docs/code/agents.md, "Endpoints"): a key sealed to this daemon opens only here and
/// only for its endpoint, reaches only the providers that use it (never `hello`), lists the
/// endpoint's models under its name, survives a restart in a private `agent.db`, and goes away
/// with the endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoint_keys_are_sealed_to_the_daemon_and_reach_only_their_providers() {
    const KEY: &str = "sk-roll-test-0123456789";
    let (url, seen) = mock_cliproxy(KEY).await;
    let home = tempfile::tempdir().unwrap();
    let (codex, claude) = (FakeProvider::new("codex"), FakeProvider::new("claude"));
    let host = open(home.path(), vec![codex.clone(), claude.clone()]).await;
    let seal_key = host.info().await.seal_key.expect("hello carries the seal key");
    let spec = EndpointSpec {
        id: "cliproxy".into(),
        name: "CLIProxyAPI".into(),
        kind: EndpointKind::Cliproxy,
        base_url: url.clone(),
        providers: vec![ProviderId::claude()],
        url_env: None,
        key_env: None,
        key_ref: None,
        fallback: false,
    };
    let sealed = |endpoint: &str, key: &str| KeyChange::Set { sealed: agent_host::endpoints::seal(&seal_key, endpoint, key).unwrap() };

    // A box for another endpoint, or sealed to another daemon, doesn't open; nothing is kept.
    assert!(invalid(run(&host, endpoint_set(&spec, sealed("other", KEY))).await).contains("sealed to this machine"));
    let stranger = agent_host::endpoints::SealKey::generate().public();
    let foreign = KeyChange::Set { sealed: agent_host::endpoints::seal(&stranger, "cliproxy", KEY).unwrap() };
    assert!(invalid(run(&host, endpoint_set(&spec, foreign)).await).contains("sealed to this machine"));
    // A provider that isn't here is refused (one that can't speak the endpoint's API too: the
    // adapters' own tests).
    let elsewhere = EndpointSpec { providers: vec![ProviderId("acp:nope".into())], ..spec.clone() };
    assert!(invalid(run(&host, endpoint_set(&elsewhere, KeyChange::Keep)).await).contains("no provider"));
    assert!(host.info().await.settings.unwrap().endpoints.is_empty());

    let mut events = host.subscribe();
    assert_eq!(run(&host, endpoint_set(&spec, sealed("cliproxy", KEY))).await, Reply::Ok);
    let env = claude.configured().unwrap().env;
    assert!(env.contains(&("ANTHROPIC_BASE_URL".into(), url.clone())), "{env:?}");
    assert!(env.contains(&("ANTHROPIC_AUTH_TOKEN".into(), KEY.into())));
    assert!(!format!("{:?}", codex.configured().unwrap()).contains(KEY), "only the providers that use it");

    // The models come from the endpoint, with the key, and are offered under its name.
    let info = loop {
        let info = host.info().await;
        let listed = info.settings.as_ref().unwrap().endpoints.first().is_some_and(|e| e.checked_at.is_some());
        if listed {
            break info;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(seen.lock().unwrap().last().map(String::as_str), Some(format!("Bearer {KEY}").as_str()));
    let ep = &info.settings.as_ref().unwrap().endpoints[0];
    assert_eq!((ep.key, ep.models.len(), ep.error.as_deref()), (KeyState::Sealed, 2, None));
    let claude_info = info.providers.iter().find(|p| p.id == ProviderId::claude()).unwrap();
    assert_eq!((claude_info.auth, claude_info.detail.as_deref()), (AuthStatus::Ready, Some("via CLIProxyAPI")));
    let gemini = claude_info.models.iter().find(|m| m.id == "gemini-2.5-pro").expect("the endpoint's model");
    assert_eq!(gemini.via.as_deref(), Some("CLIProxyAPI"));
    assert!(!info.providers.iter().find(|p| p.id == ProviderId::codex()).unwrap().models.iter().any(|m| m.via.is_some()));

    // `hello` never carries the key, nor does anything the daemon sends.
    assert!(!serde_json::to_string(&info).unwrap().contains(KEY));
    while let Ok(out) = events.try_recv() {
        let text = match out {
            Outbound::Info(i) => serde_json::to_string(&i).unwrap(),
            Outbound::Entity(e) => serde_json::to_string(&e).unwrap(),
            Outbound::Delta(d) => serde_json::to_string(&d).unwrap(),
        };
        assert!(!text.contains(KEY), "sent the key: {text}");
    }

    // A check answers now; a wrong key says so without echoing either key.
    let Reply::EndpointChecked(check) = run(&host, Command::EndpointCheck { endpoint: "cliproxy".into() }).await else { panic!("no check") };
    assert!(check.ok && check.models == 2 && check.error.is_none(), "{check:?}");
    assert_eq!(run(&host, endpoint_set(&spec, sealed("cliproxy", "sk-wrong"))).await, Reply::Ok);
    let Reply::EndpointChecked(check) = run(&host, Command::EndpointCheck { endpoint: "cliproxy".into() }).await else { panic!("no check") };
    assert!(!check.ok && check.error.as_deref().is_some_and(|e| e.contains("401") && !e.contains("sk-")), "{check:?}");
    assert_eq!(run(&host, endpoint_set(&spec, sealed("cliproxy", KEY))).await, Reply::Ok);

    // Kept across a restart, in a file only its user reads.
    drop(host);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join("agent.db")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let (codex, claude) = (FakeProvider::new("codex"), FakeProvider::new("claude"));
    let host = open(home.path(), vec![codex.clone(), claude.clone()]).await;
    assert_eq!(host.info().await.seal_key.as_deref(), Some(seal_key.as_str()), "the key pair is made once");
    assert!(claude.configured().unwrap().env.contains(&("ANTHROPIC_AUTH_TOKEN".into(), KEY.into())));

    // Changing it without a key keeps the key; Codex taken on gets it too.
    let both = EndpointSpec { providers: vec![ProviderId::claude(), ProviderId::codex()], ..spec.clone() };
    assert_eq!(run(&host, endpoint_set(&both, KeyChange::Keep)).await, Reply::Ok);
    assert!(codex.configured().unwrap().env.contains(&("OPENAI_API_KEY".into(), KEY.into())));
    // A second endpoint takes Codex over: a provider uses one endpoint.
    let gw = EndpointSpec { id: "gw".into(), name: "Gateway".into(), kind: EndpointKind::Openai, base_url: "http://127.0.0.1:9/v1".into(), providers: vec![ProviderId::codex()], ..spec.clone() };
    assert_eq!(run(&host, endpoint_set(&gw, KeyChange::Keep)).await, Reply::Ok);
    let endpoints = host.info().await.settings.unwrap().endpoints;
    assert_eq!(endpoints.iter().find(|e| e.spec.id == "cliproxy").unwrap().spec.providers, vec![ProviderId::claude()]);
    assert!(!format!("{:?}", codex.configured().unwrap()).contains(KEY));
    // Cleared, then removed: the provider goes back to its own login.
    assert_eq!(run(&host, endpoint_set(&spec, KeyChange::Clear)).await, Reply::Ok);
    assert!(!claude.configured().unwrap().env.iter().any(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN"));
    assert_eq!(host.info().await.settings.unwrap().endpoints.iter().find(|e| e.spec.id == "cliproxy").unwrap().key, KeyState::None);
    assert_eq!(run(&host, Command::EndpointRemove { endpoint: "cliproxy".into() }).await, Reply::Ok);
    assert!(!claude.configured().unwrap().env.iter().any(|(k, _)| k.starts_with("ANTHROPIC")));
    assert!(matches!(run(&host, Command::EndpointRemove { endpoint: "cliproxy".into() }).await, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
}

/// Usage limits a session reports (Codex's `account/rateLimits/updated`, Claude's
/// `rate_limit_event`) go on the provider's info, during a turn and between turns, merged by window
/// (T3's `applyUsageLimitsUpdate`), and a probe that finds none keeps them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_usage_limits_go_on_the_providers_info() {
    let fake = manual("codex", true);
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    let window = |id: &str, kind, used| UsageWindow { resets_at: Some(9_000), ..UsageWindow::new(id, kind, id, used) };
    session.emit(ProviderEvent::UsageLimits(UsageLimits::new(1, vec![window("primary", UsageWindowKind::Session, 40.0)])));
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    // Between turns: another window, and the first one moves on (its reset time kept).
    session.emit(ProviderEvent::UsageLimits(UsageLimits::new(
        2,
        vec![UsageWindow::new("primary", UsageWindowKind::Session, "primary", 55.0), window("secondary", UsageWindowKind::Weekly, 10.0)],
    )));
    let limits = |info: &MachineInfo| info.providers[0].usage_limits.clone();
    let mut got = None;
    for _ in 0..100 {
        got = limits(&w.host.info().await).filter(|l| l.windows.len() == 2);
        if got.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = got.expect("both windows on the provider's info");
    let shown: Vec<_> = got.windows.iter().map(|w| (w.id.as_str(), w.used_percent, w.resets_at)).collect();
    assert_eq!(shown, [("primary", 55.0, Some(9_000)), ("secondary", 10.0, Some(9_000))]);

    // Checking again (the fake's probe reports no limits) keeps them.
    assert_eq!(run(&w.host, Command::ProviderRefresh {}).await, Reply::Ok);
    assert_eq!(limits(&w.host.info().await).map(|l| l.windows.len()), Some(2));
}
