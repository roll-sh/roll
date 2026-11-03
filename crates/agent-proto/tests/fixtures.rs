//! One JSON sample of every wire variant, checked into `fixtures/`. The dashboard parses the
//! same files (`apps/dashboard/src/lib/agents-api.test.ts`), so a change on either side that
//! breaks the other fails a test. `ROLL_FIXTURES=write cargo test -p agent-proto` rewrites them.
//!
//! The `covered_*` matches are exhaustive: a new variant doesn't compile until it has a sample.

use agent_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::BTreeMap;

const T: Millis = 1_762_000_000_000;

fn thr() -> ThreadId {
    ThreadId::from_suffix("a1b2c3d4e5f60718")
}
fn run() -> RunId {
    RunId::from_suffix("0011223344556677")
}
fn node(n: u8) -> NodeId {
    NodeId::from_suffix(&format!("{n:016x}"))
}
fn req() -> RequestId {
    RequestId::from_suffix("8899aabbccddeeff")
}
fn prj() -> ProjectId {
    ProjectId::from_suffix("1234567890abcdef")
}
fn mch() -> MachineId {
    MachineId::from_suffix("fedcba0987654321")
}
fn chk() -> CheckpointId {
    CheckpointId::from_suffix("0f0f0f0f0f0f0f0f")
}
fn trm() -> TerminalId {
    TerminalId::from_suffix("7777777777777777")
}

fn project() -> Project {
    Project {
        id: prj(),
        name: "roll".into(),
        path: "/home/ada/roll".into(),
        repo: Some(RepoRef { owner: "acme".into(), repo: "roll".into() }),
        git: Some(GitInfo { branch: Some("main".into()), head: Some("f35f34cf".into()), remote: None, dirty: true }),
        created_at: T,
        updated_at: T,
    }
}

fn thread() -> Thread {
    Thread {
        id: thr(),
        project: prj(),
        title: "Fix the flaky queue test".into(),
        provider: ProviderId::claude(),
        model: Some("claude-opus-5-5".into()),
        effort: Some("high".into()),
        runtime_mode: RuntimeMode::AutoEdit,
        interaction_mode: InteractionMode::Default,
        cwd: "/home/ada/.local/share/roll/agent/worktrees/prj_1234567890abcdef/thr_a1b2c3d4e5f60718".into(),
        worktree: Some(Worktree {
            path: "/home/ada/.local/share/roll/agent/worktrees/prj_1234567890abcdef/thr_a1b2c3d4e5f60718".into(),
            branch: "roll/fix-the-flaky-queue-test".into(),
            base: "f35f34cf".into(),
        }),
        status: ThreadStatus::Waiting,
        lineage: Some(Lineage {
            parent: ThreadId::from_suffix("0000000000000001"),
            relation: Relation::Fork,
            root: ThreadId::from_suffix("0000000000000001"),
            from_run: Some(run()),
        }),
        active_run: Some(run()),
        queued: vec![QueuedMessage { id: "q1".into(), text: "then run clippy".into(), attachments: vec![], at: T }],
        run_count: 3,
        pending_requests: 1,
        activity: Some("Waiting to run `cargo test`".into()),
        usage: Usage { input_tokens: 12_000, output_tokens: 3_400, cached_tokens: 9_000, cost_usd: Some(0.42) },
        error: None,
        created_at: T,
        updated_at: T,
        archived_at: None,
    }
}

fn run_rec() -> Run {
    Run {
        id: run(),
        thread: thr(),
        ordinal: 3,
        provider: ProviderId::claude(),
        model: Some("claude-opus-5-5".into()),
        status: RunStatus::Waiting,
        prompt: "The queue test fails one time in ten. Find out why.".into(),
        attachments: vec![Attachment { name: "trace.png".into(), mime: "image/png".into(), data: "iVBORw0KGgo=".into() }],
        steers: vec!["it only fails on Postgres".into()],
        requested_at: T,
        started_at: Some(T + 20),
        completed_at: None,
        checkpoint: None,
        stat: None,
        usage: Usage::default(),
        error: None,
    }
}

fn node_rec(n: u8, parent: Option<NodeId>, status: NodeStatus, body: NodeBody) -> Node {
    Node {
        id: node(n),
        thread: thr(),
        run: run(),
        parent,
        ordinal: n as u32,
        status,
        started_at: T + n as u64,
        completed_at: (status != NodeStatus::Running && status != NodeStatus::Waiting).then_some(T + 100),
        body,
    }
}

fn entities() -> Vec<Entity> {
    vec![
        Entity::Project(project()),
        Entity::Thread(thread()),
        Entity::Run(run_rec()),
        Entity::Node(node_rec(1, None, NodeStatus::Completed, NodeBody::Message { text: "Looking at the test.".into() })),
        Entity::Node(node_rec(2, None, NodeStatus::Completed, NodeBody::Reasoning { text: "The lease expires…".into() })),
        Entity::Node(node_rec(3, None, NodeStatus::Completed, NodeBody::Plan { text: "1. Reproduce\n2. Fix".into() })),
        Entity::Node(node_rec(
            4,
            None,
            NodeStatus::Running,
            NodeBody::Todos {
                items: vec![
                    Todo { text: "Reproduce".into(), status: TodoStatus::Completed },
                    Todo { text: "Fix".into(), status: TodoStatus::InProgress },
                    Todo { text: "Test".into(), status: TodoStatus::Pending },
                ],
            },
        )),
        Entity::Node(node_rec(
            5,
            None,
            NodeStatus::Completed,
            NodeBody::Tool(ToolCall {
                name: "Edit".into(),
                kind: ToolKind::Edit,
                title: "crates/code-control/src/queue.rs".into(),
                input: json!({"file_path": "crates/code-control/src/queue.rs", "old_string": "a", "new_string": "b"}),
                output: None,
                exit_code: None,
                files: vec![FileChange {
                    path: "crates/code-control/src/queue.rs".into(),
                    change: ChangeKind::Modify,
                    diff: Some("@@ -1 +1 @@\n-a\n+b\n".into()),
                }],
            }),
        )),
        Entity::Node(node_rec(
            6,
            None,
            NodeStatus::Running,
            NodeBody::Subagent {
                agent: "Explore".into(),
                prompt: "Find every caller of claim()".into(),
                child: None,
                result: None,
            },
        )),
        Entity::Node(node_rec(
            7,
            Some(node(6)),
            NodeStatus::Completed,
            NodeBody::Tool(ToolCall {
                name: "Bash".into(),
                kind: ToolKind::Command,
                title: "rg claim\\(".into(),
                input: json!({"command": "rg 'claim\\('"}),
                output: Some("src/queue.rs:12: claim(".into()),
                exit_code: Some(0),
                files: vec![],
            }),
        )),
        Entity::Node(node_rec(8, None, NodeStatus::Waiting, NodeBody::Request { request: req() })),
        Entity::Node(node_rec(9, None, NodeStatus::Completed, NodeBody::System { text: "Context compacted".into() })),
        Entity::Request(Request {
            id: req(),
            thread: thr(),
            run: run(),
            node: node(8),
            status: RequestStatus::Pending,
            body: RequestBody::Approval(Approval {
                action: ApprovalAction::Command,
                title: "Run `cargo test -p code-control`".into(),
                command: Some("cargo test -p code-control".into()),
                cwd: Some("/home/ada/roll".into()),
                files: vec![],
                reason: Some("Check the fix".into()),
                tool: Some("Bash".into()),
                input: json!({"command": "cargo test -p code-control"}),
                decisions: vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel],
            }),
            created_at: T,
            resolved_at: None,
            resolved_by: None,
            response: None,
        }),
        Entity::Request(Request {
            id: RequestId::from_suffix("0000000000000002"),
            thread: thr(),
            run: run(),
            node: node(10),
            status: RequestStatus::Resolved,
            body: RequestBody::Questions {
                questions: vec![Question {
                    id: "db".into(),
                    header: "Database".into(),
                    question: "Which database should the test use?".into(),
                    options: vec![
                        QuestionOption { label: "SQLite".into(), description: Some("Fast".into()) },
                        QuestionOption { label: "Postgres".into(), description: None },
                    ],
                    multi: false,
                }],
            },
            created_at: T,
            resolved_at: Some(T + 5),
            resolved_by: Some("ada".into()),
            response: Some(Response::Answers { answers: BTreeMap::from([("db".into(), "Postgres".into())]) }),
        }),
        Entity::Checkpoint(Checkpoint {
            id: chk(),
            thread: thr(),
            run: Some(run()),
            ordinal: 3,
            commit: "9fceb02d0ae598e95dc970b74767f19372d61af8".into(),
            stat: DiffStat { files: 2, additions: 14, deletions: 3 },
            created_at: T,
        }),
        Entity::Terminal(Terminal {
            id: trm(),
            project: Some(prj()),
            thread: None,
            cwd: "/home/ada/roll".into(),
            title: "claude /login".into(),
            status: TerminalStatus::Running,
            exit_code: None,
            cols: 120,
            rows: 32,
            created_at: T,
        }),
        Entity::Removed(Removed { entity: EntityKind::Node, id: node(9).0, thread: Some(thr()) }),
    ]
}

fn covered_entity(e: &Entity) {
    match e {
        Entity::Project(_)
        | Entity::Thread(_)
        | Entity::Run(_)
        | Entity::Request(_)
        | Entity::Checkpoint(_)
        | Entity::Terminal(_)
        | Entity::Removed(_) => {}
        Entity::Node(n) => match n.body {
            NodeBody::Message { .. }
            | NodeBody::Reasoning { .. }
            | NodeBody::Plan { .. }
            | NodeBody::Todos { .. }
            | NodeBody::Tool(_)
            | NodeBody::Subagent { .. }
            | NodeBody::Request { .. }
            | NodeBody::System { .. } => {}
        },
    }
}

fn commands() -> Vec<Command> {
    vec![
        Command::ProjectAdd { path: "/home/ada/roll".into(), name: None, clone: Some(RepoRef { owner: "acme".into(), repo: "roll".into() }) },
        Command::ProjectRemove { project: prj() },
        Command::ThreadCreate {
            project: prj(),
            provider: ProviderId::codex(),
            model: Some("gpt-5.5-codex".into()),
            effort: Some("high".into()),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Plan,
            worktree: true,
            title: None,
            prompt: Some("Add a --json flag to roll code status".into()),
            attachments: vec![],
        },
        Command::ThreadConfigure {
            thread: thr(),
            title: Some("Renamed".into()),
            provider: Some(ProviderId("acp:gemini".into())),
            model: None,
            effort: None,
            runtime_mode: Some(RuntimeMode::Ask),
            interaction_mode: None,
        },
        Command::ThreadFork { thread: thr(), at: Some(run()), worktree: true },
        Command::ThreadArchive { thread: thr() },
        Command::ThreadUnarchive { thread: thr() },
        Command::ThreadDelete { thread: thr() },
        Command::MessageSend { thread: thr(), text: "also update the docs".into(), attachments: vec![], mode: SendMode::Steer },
        Command::MessageUnqueue { thread: thr(), message: "q1".into() },
        Command::RunInterrupt { thread: thr() },
        Command::RequestRespond {
            request: req(),
            response: Response::Decision { decision: Decision::Decline, message: Some("use the fake runner".into()) },
        },
        Command::CheckpointRollback { thread: thr(), checkpoint: chk() },
        Command::ThreadPublish { thread: thr(), message: "Fix the flaky queue test".into(), land: true },
        Command::TerminalOpen { project: Some(prj()), thread: None, command: Some("claude /login".into()), cols: 120, rows: 32 },
        Command::TerminalInput { terminal: trm(), data: "bHMK".into() },
        Command::TerminalResize { terminal: trm(), cols: 100, rows: 30 },
        Command::TerminalClose { terminal: trm() },
        Command::ProviderRefresh {},
        Command::DiffGet { thread: thr(), run: Some(run()) },
        Command::FsList { project: None, path: "/home/ada".into() },
        Command::FsRead { project: prj(), path: "README.md".into() },
        Command::GitStatus { thread: thr() },
        Command::TerminalScrollback { terminal: trm() },
    ]
}

fn covered_command(c: &Command) {
    match c {
        Command::ProjectAdd { .. }
        | Command::ProjectRemove { .. }
        | Command::ThreadCreate { .. }
        | Command::ThreadConfigure { .. }
        | Command::ThreadFork { .. }
        | Command::ThreadArchive { .. }
        | Command::ThreadUnarchive { .. }
        | Command::ThreadDelete { .. }
        | Command::MessageSend { .. }
        | Command::MessageUnqueue { .. }
        | Command::RunInterrupt { .. }
        | Command::RequestRespond { .. }
        | Command::CheckpointRollback { .. }
        | Command::ThreadPublish { .. }
        | Command::TerminalOpen { .. }
        | Command::TerminalInput { .. }
        | Command::TerminalResize { .. }
        | Command::TerminalClose { .. }
        | Command::ProviderRefresh {}
        | Command::DiffGet { .. }
        | Command::FsList { .. }
        | Command::FsRead { .. }
        | Command::GitStatus { .. }
        | Command::TerminalScrollback { .. } => {}
    }
}

fn replies() -> Vec<Reply> {
    let change = FileChange { path: "README.md".into(), change: ChangeKind::Add, diff: Some("@@ -0,0 +1 @@\n+hi\n".into()) };
    vec![
        Reply::Ok,
        Reply::Created { id: thr().0 },
        Reply::Diff(Diff { stat: DiffStat { files: 1, additions: 1, deletions: 0 }, files: vec![change.clone()], truncated: false }),
        Reply::Entries { entries: vec![FsEntry { name: "src".into(), dir: true, size: 0 }, FsEntry { name: "Cargo.toml".into(), dir: false, size: 812 }] },
        Reply::File(FileContent { path: "README.md".into(), text: Some("hi\n".into()), size: 3 }),
        Reply::GitStatus(GitStatus { branch: Some("roll/fix".into()), ahead: 2, behind: 0, files: vec![change] }),
        Reply::Published(Published {
            branch: "roll/fix".into(),
            commit: "9fceb02d".into(),
            change: Some(42),
            url: Some("https://roll.sh/acme/roll/changes/42".into()),
        }),
        Reply::Scrollback { data: "JCBscwo=".into() },
        Reply::Error(ReplyError { code: ErrorCode::MachineOffline, message: "the machine isn't connected".into() }),
    ]
}

fn covered_reply(r: &Reply) {
    match r {
        Reply::Ok
        | Reply::Created { .. }
        | Reply::Diff(_)
        | Reply::Entries { .. }
        | Reply::File(_)
        | Reply::GitStatus(_)
        | Reply::Published(_)
        | Reply::Scrollback { .. }
        | Reply::Error(_) => {}
    }
}

fn deltas() -> Vec<Delta> {
    vec![
        Delta::Text { thread: thr(), node: node(1), text: "Looking".into() },
        Delta::Output { thread: thr(), node: node(7), text: "test result: ok\n".into() },
        Delta::Terminal { terminal: trm(), data: "G1szMm0=".into() },
    ]
}

fn covered_delta(d: &Delta) {
    match d {
        Delta::Text { .. } | Delta::Output { .. } | Delta::Terminal { .. } => {}
    }
}

fn info() -> MachineInfo {
    MachineInfo {
        hostname: "ada-mbp".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        version: "0.1.0".into(),
        home: "/Users/ada".into(),
        providers: vec![
            ProviderInfo {
                id: ProviderId::claude(),
                name: "Claude Code".into(),
                installed: true,
                version: Some("2.1.310".into()),
                auth: AuthStatus::Ready,
                detail: None,
                models: vec![ModelInfo {
                    id: "claude-opus-5-5".into(),
                    name: "Opus 5.5".into(),
                    default: true,
                    efforts: vec!["low".into(), "medium".into(), "high".into(), "max".into()],
                }],
                capabilities: Capabilities { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true },
            },
            ProviderInfo {
                id: ProviderId::codex(),
                name: "Codex".into(),
                installed: true,
                version: Some("0.164.0".into()),
                auth: AuthStatus::Missing,
                detail: Some("Run `codex login`".into()),
                models: vec![],
                capabilities: Capabilities { steer: true, fork: true, rollback: true, plan_mode: true, images: true, subagents: true, mode_switch: false },
            },
        ],
    }
}

fn daemon_frames() -> Vec<DaemonFrame> {
    vec![
        DaemonFrame::Hello { protocol: PROTOCOL, info: info() },
        DaemonFrame::Entities { items: vec![Stamped { seq: 41, entity: Entity::Project(project()) }] },
        DaemonFrame::Delta { delta: deltas().remove(0) },
        DaemonFrame::Reply { id: "cmd_1".into(), result: Reply::Created { id: thr().0 } },
        DaemonFrame::Pong,
    ]
}

fn covered_daemon(f: &DaemonFrame) {
    match f {
        DaemonFrame::Hello { .. } | DaemonFrame::Entities { .. } | DaemonFrame::Delta { .. } | DaemonFrame::Reply { .. } | DaemonFrame::Pong => {}
    }
}

fn relay_frames() -> Vec<RelayFrame> {
    vec![
        RelayFrame::Welcome { machine: mch(), owner: "acme".into(), resume_from: 40 },
        RelayFrame::Ack { seq: 41 },
        RelayFrame::Command { id: "cmd_1".into(), by: "acme".into(), command: Command::RunInterrupt { thread: thr() } },
        RelayFrame::Ping,
    ]
}

fn covered_relay(f: &RelayFrame) {
    match f {
        RelayFrame::Welcome { .. } | RelayFrame::Ack { .. } | RelayFrame::Command { .. } | RelayFrame::Ping => {}
    }
}

fn machine() -> Machine {
    Machine {
        id: mch(),
        name: "ada-mbp".into(),
        online: true,
        info: Some(info()),
        sandbox: Some(SandboxInfo { backend: "agent37".into(), size: "2cpu-4gb".into(), status: SandboxStatus::Running, monthly_cents: 2000 }),
        created_at: T,
        last_seen: Some(T),
    }
}

fn stream() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Entity(Box::new(Mirrored { cursor: 7, machine: mch(), entity: Entity::Thread(thread()) })),
        StreamEvent::Machine { cursor: 8, machine: machine() },
        StreamEvent::MachineRemoved { cursor: 9, machine: mch() },
        StreamEvent::Delta { machine: mch(), delta: deltas().remove(1) },
    ]
}

fn covered_stream(e: &StreamEvent) {
    match e {
        StreamEvent::Entity(_) | StreamEvent::Machine { .. } | StreamEvent::MachineRemoved { .. } | StreamEvent::Delta { .. } => {}
    }
}

fn login_polls() -> Vec<LoginPoll> {
    vec![
        LoginPoll::Pending,
        LoginPoll::Approved { token: "rlm_0123456789abcdef".into(), machine: mch(), owner: "acme".into() },
        LoginPoll::Expired,
        LoginPoll::Denied,
    ]
}

fn covered_poll(p: &LoginPoll) {
    match p {
        LoginPoll::Pending | LoginPoll::Approved { .. } | LoginPoll::Expired | LoginPoll::Denied => {}
    }
}

/// Serialize, round-trip, and compare with (or write) the checked-in fixture.
fn check<V: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, values: Vec<V>) {
    for v in &values {
        let s = serde_json::to_string(v).unwrap();
        let back: V = serde_json::from_str(&s).unwrap_or_else(|e| panic!("{name}: {s} doesn't parse back: {e}"));
        assert_eq!(&back, v, "{name}: round trip changed {s}");
    }
    let text = serde_json::to_string_pretty(&values).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(format!("{name}.json"));
    if std::env::var("ROLL_FIXTURES").as_deref() == Ok("write") {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == text,
        "fixtures/{name}.json is stale: run `ROLL_FIXTURES=write cargo test -p agent-proto` and update \
         apps/dashboard/src/lib/agents-api.ts to match"
    );
}

#[test]
fn fixtures() {
    let e = entities();
    e.iter().for_each(covered_entity);
    check("entities", e);
    let c = commands();
    c.iter().for_each(covered_command);
    check("commands", c);
    let r = replies();
    r.iter().for_each(covered_reply);
    check("replies", r);
    let d = deltas();
    d.iter().for_each(covered_delta);
    check("deltas", d);
    let f = daemon_frames();
    f.iter().for_each(covered_daemon);
    check("daemon_frames", f);
    let f = relay_frames();
    f.iter().for_each(covered_relay);
    check("relay_frames", f);
    let s = stream();
    s.iter().for_each(covered_stream);
    check("stream", s);
    let p = login_polls();
    p.iter().for_each(covered_poll);
    check("login_polls", p);
    check("state", vec![State { cursor: 9, machines: vec![machine()], entities: vec![Mirrored { cursor: 7, machine: mch(), entity: Entity::Project(project()) }] }]);
    check("command_requests", vec![CommandRequest { id: "cmd_1".into(), command: Command::RunInterrupt { thread: thr() } }]);
    check(
        "device_logins",
        vec![DeviceLogin {
            device_code: "dc_0123456789abcdef0123456789abcdef".into(),
            user_code: "WXYZ-2345".into(),
            verification_uri: "https://roll.sh/dashboard/agents/connect?code=WXYZ-2345".into(),
            interval: 2,
            expires_in: 600,
        }],
    );
}

#[test]
fn every_variant_of_every_tagged_enum_is_sampled() {
    // A variant listed in a covered_* match but never sampled still slips through; count tags.
    let tags = |v: serde_json::Value, key: &str| -> std::collections::BTreeSet<String> {
        v.as_array().unwrap().iter().map(|x| x[key].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(tags(serde_json::to_value(commands()).unwrap(), "type").len(), 24, "commands");
    assert_eq!(tags(serde_json::to_value(replies()).unwrap(), "type").len(), 9, "replies");
    assert_eq!(tags(serde_json::to_value(entities()).unwrap(), "kind").len(), 8, "entities");
    let bodies: std::collections::BTreeSet<String> = entities()
        .iter()
        .filter_map(|e| if let Entity::Node(n) = e { Some(serde_json::to_value(&n.body).unwrap()["type"].as_str().unwrap().to_string()) } else { None })
        .collect();
    assert_eq!(bodies.len(), 8, "node bodies");
}

#[test]
fn scopes_and_threads() {
    for e in entities() {
        let detail = matches!(e.kind(), EntityKind::Node | EntityKind::Checkpoint);
        assert_eq!(e.scope() == Scope::Detail, detail, "{:?}", e.kind());
        if !matches!(e, Entity::Project(_) | Entity::Terminal(_)) {
            assert_eq!(e.thread(), Some(&thr()), "{:?}", e.kind());
        }
    }
}
