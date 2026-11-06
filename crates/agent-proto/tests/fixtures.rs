//! One JSON sample of every wire variant, checked into `fixtures/`. The dashboard parses the
//! same files (`apps/dashboard/src/lib/agents-api.test.ts`), so a change on either side that
//! breaks the other fails a test. `ROLL_FIXTURES=write cargo test -p agent-proto` rewrites them.
//!
//! The `covered_*` matches are exhaustive: a new variant doesn't compile until it has a sample.

mod automation_samples;

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
        settings: ProjectSettings {
            auto_pull: true,
            defaults: ThreadDefaults {
                provider: Some(ProviderId::claude()),
                model: Some("claude-opus-5-5".into()),
                effort: Some("high".into()),
                options: ModelOptions::from([("fastMode".into(), OptionValue::Flag(true))]),
                runtime_mode: Some(RuntimeMode::AutoEdit),
                interaction_mode: Some(InteractionMode::Default),
                worktree: Some(true),
            },
            scripts: vec![
                ProjectScript { id: "setup".into(), name: "Install".into(), command: "pnpm install".into(), icon: ScriptIcon::Configure, on_worktree: true },
                ProjectScript { id: "test".into(), name: "Test".into(), command: "cargo test".into(), icon: ScriptIcon::Test, on_worktree: false },
            ],
            auto_settle: AutoSettle { after_days: None, on_land: false, keep_changes: true },
            cleanup: Cleanup { worktree_after_days: Some(7), keep: CleanupKeep::AnyLocalFiles },
            color: Some(IdentityColor::Teal),
            icon: ProjectIcon::File { path: "public/logo.svg".into() },
        },
        pull: Some(PullResult { at: T, outcome: PullOutcome::Pulled { branch: "main".into(), from: "8ea39e4d".into(), to: "f35f34cf".into(), commits: 3 } }),
        logo: Some(ProjectLogo { path: "public/logo.svg".into(), url: "data:image/svg+xml;base64,PHN2Zy8+".into() }),
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
        options: ModelOptions::from([("fastMode".into(), OptionValue::Flag(true)), ("contextWindow".into(), OptionValue::Choice("1m".into()))]),
        runtime_mode: RuntimeMode::AutoEdit,
        interaction_mode: InteractionMode::Default,
        cwd: "/home/ada/.local/share/roll/agent/worktrees/prj_1234567890abcdef/thr_a1b2c3d4e5f60718".into(),
        worktree: Some(Worktree {
            path: "/home/ada/.local/share/roll/agent/worktrees/prj_1234567890abcdef/thr_a1b2c3d4e5f60718".into(),
            branch: "roll/fix-the-flaky-queue-test".into(),
            base: "f35f34cf".into(),
            removed_at: Some(T),
        }),
        status: ThreadStatus::Waiting,
        lineage: Some(Lineage {
            parent: ThreadId::from_suffix("0000000000000001"),
            relation: Relation::Fork,
            root: ThreadId::from_suffix("0000000000000001"),
            from_run: Some(run()),
        }),
        orchestrator: true,
        active_run: Some(run()),
        queued: vec![
            QueuedMessage { id: "q1".into(), text: "then run clippy".into(), attachments: vec![], at: T, delivers: vec![] },
            QueuedMessage { id: "q2".into(), text: "Delegated task nod_0000000000000002 (Tests) [completed]:\nAll green.".into(), attachments: vec![], at: T, delivers: vec![node(2)] },
        ],
        run_count: 3,
        pending_requests: 1,
        plan_ready: true,
        activity: Some("Waiting to run `cargo test`".into()),
        usage: Usage { input_tokens: 12_000, output_tokens: 3_400, cached_tokens: 9_000, cost_usd: Some(0.42) },
        context: Some(ContextUsage { used_tokens: 124_000, max_tokens: Some(200_000), total_processed_tokens: Some(410_000), cache_ttl_ms: Some(3_600_000), auto_compact_at: Some(117_000) }),
        error: None,
        change: Some(Box::new(ThreadChange {
            repo: "acme/roll".into(),
            number: 42,
            status: ChangeStatus::Checking,
            url: "https://roll.sh/acme/roll/changes/42".into(),
            detail: Some("test (running)".into()),
        })),
        work: Some(Box::new(WorkRef { id: "wi_7Hq2Zp4Ka9Lm3N".into(), repo: "acme/roll".into(), number: 41, title: "Fix the flaky queue test".into() })),
        created_at: T,
        updated_at: T,
        archived_at: None,
        inbox: Box::new(Inbox { visited_at: Some(T - 60_000), ..Inbox::default() }),
    }
}

/// A settled thread, and one snoozed after a snooze woke it once.
fn shelved() -> Vec<Thread> {
    let idle = |n: &str| Thread {
        id: ThreadId::from_suffix(n),
        status: ThreadStatus::Idle,
        lineage: None,
        active_run: None,
        queued: vec![],
        pending_requests: 0,
        plan_ready: false,
        activity: None,
        change: None,
        work: None,
        ..thread()
    };
    vec![
        Thread {
            inbox: Box::new(Inbox {
                settle: Some(Settle::Settled { at: T - 3 * 86_400_000, by: SettledBy::Inactivity }),
                visited_at: Some(T - 4 * 86_400_000),
                ..Inbox::default()
            }),
            ..idle("00000000000000a1")
        },
        Thread {
            inbox: Box::new(Inbox {
                settle: Some(Settle::Active { at: T - 7_200_000 }),
                snooze: Some(Snooze { until: T + 3_600_000, at: T }),
                woke_at: Some(T - 3_600_000),
                visited_at: Some(T),
                manual: true,
            }),
            ..idle("00000000000000a2")
        },
    ]
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
        steered_at: vec![T + 900],
        idle_compaction: false,
        delivers: vec![],
        requested_at: T,
        started_at: Some(T + 20),
        completed_at: None,
        checkpoint: None,
        stat: None,
        usage: Usage::default(),
        context: Some(ContextUsage { used_tokens: 18_500, max_tokens: None, total_processed_tokens: None, ..Default::default() }),
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
        Entity::Thread(shelved()[0].clone()),
        Entity::Thread(shelved()[1].clone()),
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
                progress: Some("Reading src/claims.rs".into()),
                model: Some("claude-haiku-4-5".into()),
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
        Entity::Node(node_rec(
            11,
            None,
            NodeStatus::Completed,
            NodeBody::Compaction { before_tokens: Some(182_000), after_tokens: Some(41_000), error: None },
        )),
        Entity::Node(node_rec(
            12,
            None,
            NodeStatus::Failed,
            NodeBody::Compaction { before_tokens: None, after_tokens: None, error: Some("Not enough messages to compact.".into()) },
        )),
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
            | NodeBody::Compaction { .. }
            | NodeBody::Request { .. }
            | NodeBody::System { .. } => {}
        },
    }
}

fn commands() -> Vec<Command> {
    vec![
        Command::ProjectAdd { path: "/home/ada/roll".into(), name: None, clone: Some(RepoRef { owner: "acme".into(), repo: "roll".into() }) },
        Command::ProjectRemove { project: prj() },
        Command::ProjectUpdate {
            project: prj(),
            name: Some("roll".into()),
            settings: Some(ProjectSettings { auto_pull: true, ..ProjectSettings::default() }),
        },
        Command::ProjectUpdate {
            project: prj(),
            name: None,
            settings: Some(ProjectSettings { color: Some(IdentityColor::Rose), icon: ProjectIcon::Emoji { emoji: "🦀".into() }, ..ProjectSettings::default() }),
        },
        Command::ProjectUpdate {
            project: prj(),
            name: None,
            settings: Some(ProjectSettings { icon: ProjectIcon::Monogram { text: Some("RO".into()) }, ..ProjectSettings::default() }),
        },
        Command::ProjectPull { project: prj() },
        Command::ProjectCleanup { project: prj() },
        Command::ThreadCreate {
            project: prj(),
            provider: ProviderId::codex(),
            model: Some("gpt-5.5-codex".into()),
            effort: Some("high".into()),
            options: ModelOptions::from([("serviceTier".into(), OptionValue::Choice("fast".into()))]),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Plan,
            worktree: true,
            title: None,
            prompt: Some("Add a --json flag to roll code status".into()),
            attachments: vec![],
            orchestrator: false,
        },
        Command::ThreadCreate {
            project: prj(),
            provider: ProviderId::claude(),
            model: Some("claude-opus-5-5".into()),
            effort: None,
            options: ModelOptions::new(),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            worktree: false,
            title: None,
            prompt: Some("Add dark mode to the settings page, with tests".into()),
            attachments: vec![],
            orchestrator: true,
        },
        Command::ThreadConfigure {
            thread: thr(),
            title: Some("Renamed".into()),
            provider: Some(ProviderId("acp:gemini".into())),
            model: None,
            effort: None,
            options: Some(ModelOptions::from([("thinking".into(), OptionValue::Flag(false))])),
            runtime_mode: Some(RuntimeMode::Ask),
            interaction_mode: None,
        },
        Command::ThreadFork { thread: thr(), at: Some(run()), worktree: true },
        Command::ThreadArchive { thread: thr() },
        Command::ThreadUnarchive { thread: thr() },
        Command::ThreadDelete { thread: thr() },
        Command::ThreadSettle { thread: thr() },
        Command::ThreadUnsettle { thread: thr() },
        Command::ThreadSnooze { thread: thr(), until: T + 3_600_000 },
        Command::ThreadUnsnooze { thread: thr() },
        Command::ThreadVisit { thread: thr() },
        Command::ThreadAutoSettle { thread: thr(), enabled: false },
        Command::ThreadChangeEnded { thread: thr(), landed: true },
        Command::MessageSend { thread: thr(), text: "also update the docs".into(), attachments: vec![], mode: SendMode::Steer },
        Command::MessageUnqueue { thread: thr(), message: "q1".into() },
        Command::MessageEdit { thread: thr(), message: "q1".into(), text: "and the changelog".into(), attachments: None },
        Command::MessageEdit { thread: thr(), message: "q1".into(), text: "no images".into(), attachments: Some(vec![]) },
        Command::MessageReorder { thread: thr(), message: "q2".into(), before: Some("q1".into()) },
        Command::MessageReorder { thread: thr(), message: "q1".into(), before: None },
        Command::MessagePromote { thread: thr(), message: "q1".into() },
        Command::RunInterrupt { thread: thr() },
        Command::RequestRespond {
            request: req(),
            response: Response::Decision { decision: Decision::Decline, message: Some("use the fake runner".into()) },
        },
        Command::CheckpointRollback { thread: thr(), checkpoint: chk() },
        Command::ThreadPublish { thread: thr(), message: "Fix the flaky queue test".into(), land: true },
        Command::TerminalOpen { project: Some(prj()), thread: None, command: Some("claude /login".into()), script: None, cols: 120, rows: 32 },
        Command::TerminalOpen { project: None, thread: Some(thr()), command: None, script: Some("test".into()), cols: 120, rows: 32 },
        Command::TerminalInput { terminal: trm(), data: "bHMK".into() },
        Command::TerminalResize { terminal: trm(), cols: 100, rows: 30 },
        Command::TerminalClose { terminal: trm() },
        Command::ProviderRefresh {},
        Command::MachineConfigure { settings: Box::new(settings()) },
        Command::EndpointSet { endpoint: endpoint_spec(), key: KeyChange::Set { sealed: sealed() } },
        Command::EndpointSet { endpoint: EndpointSpec { providers: vec![ProviderId::claude()], ..endpoint_spec() }, key: KeyChange::Keep },
        Command::EndpointSet { endpoint: endpoint_spec(), key: KeyChange::Clear },
        Command::EndpointRemove { endpoint: "cliproxy".into() },
        Command::EndpointCheck { endpoint: "cliproxy".into() },
        Command::GatewayConfigure { settings: Box::new(gateway_settings()) },
        Command::GatewayKey { upstream: "anthropic".into(), key: KeyChange::Set { sealed: sealed() } },
        Command::GatewayKey { upstream: "openrouter".into(), key: KeyChange::Clear },
        Command::GatewayClientIssue { name: "Ada's sandbox".into(), machine: Some(mch()), seal_key: "hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo".into() },
        Command::GatewayClientRevoke { client: "gwc_5f1d2c3b4a596877".into() },
        Command::GatewayTest { upstream: None },
        Command::GatewayTest { upstream: Some("anthropic".into()) },
        Command::GatewayRemove {},
        Command::GatewayStatus {},
        Command::GatewayUsage { from: T - 7 * 86_400_000, to: T, by: UsageBy::Machine, bucket: UsageBucket::Day },
        Command::GatewayLog { after: Some(41), before: None, limit: 50, errors: true },
        Command::GatewayLogBody { seq: 42 },
        Command::RegistryInstall { agent: "goose".into() },
        Command::RegistryRemove { agent: "goose".into() },
        Command::DiffGet { thread: thr(), run: Some(run()) },
        Command::FsList { project: None, path: "/home/ada".into() },
        Command::FsRead { project: prj(), path: "README.md".into() },
        Command::FsWrite { thread: thr(), path: "docs/plans/fix-the-queue.md".into(), text: "# Fix the queue\n".into() },
        Command::GitStatus { thread: thr() },
        Command::TerminalScrollback { terminal: trm() },
        Command::RegistryList {},
    ]
}

fn covered_command(c: &Command) {
    match c {
        Command::ProjectAdd { .. }
        | Command::ProjectRemove { .. }
        | Command::ProjectUpdate { .. }
        | Command::ProjectPull { .. }
        | Command::ProjectCleanup { .. }
        | Command::ThreadCreate { .. }
        | Command::ThreadConfigure { .. }
        | Command::ThreadFork { .. }
        | Command::ThreadArchive { .. }
        | Command::ThreadUnarchive { .. }
        | Command::ThreadDelete { .. }
        | Command::ThreadSettle { .. }
        | Command::ThreadUnsettle { .. }
        | Command::ThreadSnooze { .. }
        | Command::ThreadUnsnooze { .. }
        | Command::ThreadVisit { .. }
        | Command::ThreadAutoSettle { .. }
        | Command::ThreadChangeEnded { .. }
        | Command::MessageSend { .. }
        | Command::MessageUnqueue { .. }
        | Command::MessageEdit { .. }
        | Command::MessageReorder { .. }
        | Command::MessagePromote { .. }
        | Command::RunInterrupt { .. }
        | Command::RequestRespond { .. }
        | Command::CheckpointRollback { .. }
        | Command::ThreadPublish { .. }
        | Command::TerminalOpen { .. }
        | Command::TerminalInput { .. }
        | Command::TerminalResize { .. }
        | Command::TerminalClose { .. }
        | Command::ProviderRefresh {}
        | Command::MachineConfigure { .. }
        | Command::EndpointSet { .. }
        | Command::EndpointRemove { .. }
        | Command::EndpointCheck { .. }
        | Command::GatewayConfigure { .. }
        | Command::GatewayKey { .. }
        | Command::GatewayClientIssue { .. }
        | Command::GatewayClientRevoke { .. }
        | Command::GatewayTest { .. }
        | Command::GatewayRemove {}
        | Command::GatewayStatus {}
        | Command::GatewayUsage { .. }
        | Command::GatewayLog { .. }
        | Command::GatewayLogBody { .. }
        | Command::RegistryInstall { .. }
        | Command::RegistryRemove { .. }
        | Command::RegistryList {}
        | Command::DiffGet { .. }
        | Command::FsList { .. }
        | Command::FsRead { .. }
        | Command::FsWrite { .. }
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
            landing: Some(Landing {
                status: LandingStatus::Blocked,
                message: "change #42 can't land yet".into(),
                blockers: vec!["needs an approving review".into()],
            }),
        }),
        Reply::Pulled(PullResult { at: T, outcome: PullOutcome::UpToDate { branch: "main".into() } }),
        Reply::Pulled(PullResult {
            at: T,
            outcome: PullOutcome::Skipped { reason: PullSkip::LocalChanges, message: "the checkout has uncommitted changes".into() },
        }),
        Reply::Pulled(PullResult { at: T, outcome: PullOutcome::Failed { message: "fetching origin failed: could not resolve host".into() } }),
        Reply::Written { path: "docs/plans/fix-the-queue.md".into() },
        Reply::Cleaned(CleanupReport {
            removed: vec![thr()],
            kept: [
                KeptReason::LocalChanges,
                KeptReason::Busy,
                KeptReason::Terminal,
                KeptReason::OtherBranch,
                KeptReason::Shared,
                KeptReason::NotManaged,
                KeptReason::Failed,
            ]
            .into_iter()
            .map(|reason| CleanupKept { thread: ThreadId::from_suffix("0000000000000001"), reason })
            .collect(),
        }),
        Reply::Scrollback { data: "JCBscwo=".into() },
        Reply::EndpointChecked(EndpointCheck { ok: true, models: 42, ms: 180, error: None }),
        Reply::EndpointChecked(EndpointCheck { ok: false, models: 0, ms: 10_000, error: Some("the endpoint answered 401 Unauthorized".into()) }),
        Reply::GatewayClient(IssuedClient { client: gateway_client(), sealed: sealed() }),
        Reply::GatewayTested {
            checks: vec![
                UpstreamCheck { upstream: "anthropic".into(), ok: true, ms: 210, models: 9, error: None },
                UpstreamCheck { upstream: "openrouter".into(), ok: false, ms: 80, models: 0, error: Some("OpenRouter answered 401 Unauthorized".into()) },
            ],
        },
        Reply::GatewayStatus(GatewayStatus {
            run: GatewayRun::Running { addresses: vec![addr([127, 0, 0, 1], 8790), addr([100, 64, 0, 10], 8790)], since: T },
            upstreams: vec![
                UpstreamHealth {
                    upstream: "anthropic".into(),
                    circuit: CircuitState::Closed,
                    failures: 0,
                    open_until: None,
                    cooling_until: None,
                    last_ok_at: Some(T),
                    last_error: None,
                    last_error_at: None,
                    requests_hour: 312,
                    p50_ms: Some(640),
                },
                UpstreamHealth {
                    upstream: "bedrock".into(),
                    circuit: CircuitState::Open,
                    failures: 5,
                    open_until: Some(T + 30_000),
                    cooling_until: Some(T + 20_000),
                    last_ok_at: None,
                    last_error: Some("Amazon Bedrock answered 503 Service Unavailable".into()),
                    last_error_at: Some(T),
                    requests_hour: 4,
                    p50_ms: None,
                },
            ],
            clients_seen: vec![ClientSeen { client: "gwc_5f1d2c3b4a596877".into(), at: T }],
            models: vec!["claude-sonnet-4-5".into(), "fast".into(), "gpt-5.5".into()],
        }),
        Reply::GatewayUsage(UsageReport {
            by: UsageBy::Model,
            bucket: UsageBucket::Day,
            from: T - 86_400_000,
            to: T,
            rows: vec![UsageRow {
                at: T - 86_400_000,
                key: "claude-sonnet-4-5".into(),
                requests: 1204,
                errors: 3,
                input_tokens: 8_400_000,
                output_tokens: 610_000,
                cache_read_tokens: 52_000_000,
                cache_write_tokens: 1_900_000,
                cost_micros: 55_485_000,
            }],
            truncated: false,
        }),
        Reply::GatewayLog { entries: vec![log_entry()] },
        Reply::GatewayLogBody(LogBody { seq: 42, request: "{\"model\":\"fast\"}".into(), response: "event: message_start\n".into(), truncated: true }),
        Reply::Registry(RegistryList {
            agents: vec![
                RegistryAgent {
                    id: "goose".into(),
                    name: "goose".into(),
                    version: "1.57.0".into(),
                    description: "A local, extensible, open source AI agent that automates engineering tasks".into(),
                    website: None,
                    repository: Some("https://github.com/block/goose".into()),
                    icon: Some("https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg".into()),
                    kinds: vec![RegistryKind::Binary],
                    installable: true,
                    integrity: RegistryIntegrity::Sha256,
                    installed: Some("1.57.0".into()),
                    offered: Some(ProviderId("acp:goose".into())),
                },
                RegistryAgent {
                    id: "fast-agent".into(),
                    name: "fast-agent".into(),
                    version: "0.10.1".into(),
                    description: String::new(),
                    website: Some("https://fast-agent.ai".into()),
                    repository: None,
                    icon: None,
                    kinds: vec![RegistryKind::Uvx],
                    installable: false,
                    integrity: RegistryIntegrity::Registry,
                    installed: None,
                    offered: None,
                },
                RegistryAgent {
                    id: "junie".into(),
                    name: "Junie".into(),
                    version: "3959.5.0".into(),
                    description: "JetBrains' coding agent".into(),
                    website: None,
                    repository: None,
                    icon: None,
                    kinds: vec![RegistryKind::Binary, RegistryKind::Npx],
                    installable: true,
                    integrity: RegistryIntegrity::None,
                    installed: None,
                    offered: None,
                },
            ],
            fetched_at: T,
            error: None,
        }),
        Reply::Registry(RegistryList { agents: vec![], fetched_at: 0, error: Some("couldn't fetch the ACP registry: timed out".into()) }),
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
        | Reply::Pulled(_)
        | Reply::Written { .. }
        | Reply::Cleaned(_)
        | Reply::Scrollback { .. }
        | Reply::EndpointChecked(_)
        | Reply::GatewayClient(_)
        | Reply::GatewayTested { .. }
        | Reply::GatewayStatus(_)
        | Reply::GatewayUsage(_)
        | Reply::GatewayLog { .. }
        | Reply::GatewayLogBody(_)
        | Reply::Registry(_)
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

fn settings() -> MachineSettings {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(ProviderId::codex(), ProviderSettings { enabled: false, ..ProviderSettings::default() });
    providers.insert(
        ProviderId::claude(),
        ProviderSettings {
            enabled: true,
            binary: Some("/opt/claude/bin/claude".into()),
            args: vec!["--verbose".into()],
            env: [("CLAUDE_CONFIG_DIR".to_string(), "/Users/ada/.claude-work".to_string())].into(),
            models: vec![
                CustomModel { id: "claude-opus-5-5[1m]".into(), name: Some("Opus 5.5 (1M)".into()), options: None },
                CustomModel {
                    id: "claude-opus-5-5-preview".into(),
                    name: None,
                    options: Some(vec![ModelOption::reasoning(vec![OptionChoice::new("low", "Low"), OptionChoice::new("high", "High")], Some("high"))]),
                },
            ],
            auto_compact_window: Some(300_000),
            idle_compaction: Some(IdleCompaction::BeforeCacheExpires),
        },
    );
    MachineSettings {
        providers,
        defaults: ThreadDefaults { provider: Some(ProviderId::claude()), runtime_mode: Some(RuntimeMode::AutoEdit), worktree: Some(true), ..ThreadDefaults::default() },
        worktree_dir: Some("~/worktrees".into()),
        branch_prefix: Some("ada".into()),
        clone_dir: None,
        pull_every: Some(300),
        max_depth: Some(2),
        max_children: None,
        git_name: Some("Ada Lovelace".into()),
        git_email: Some(concat!("ada", "@", "example.com").into()),
        idle_compaction: Some(IdleCompaction::Off),
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
                    name: "Claude Opus 5.5".into(),
                    short_name: Some("Opus 5.5".into()),
                    default: true,
                    options: vec![
                        ModelOption::reasoning(
                            vec![
                                OptionChoice::new("low", "Low"),
                                OptionChoice::new("medium", "Medium"),
                                OptionChoice::new("high", "High"),
                                OptionChoice::new("ultracode", "Ultracode").described("xhigh effort plus multi-agent workflow orchestration"),
                            ],
                            Some("medium"),
                        ),
                        ModelOption::Boolean { id: "fastMode".into(), label: "Fast Mode".into(), description: None, default: false, role: Some(OptionRole::Speed) },
                    ],
                    legacy: false,
                    images: true,
                    custom: false,
                    via: None,
                    badge: Some(ModelBadge::New),
                }, ModelInfo {
                    id: "claude-sonnet-4-6".into(),
                    name: "Claude Sonnet 4.6".into(),
                    short_name: None,
                    default: false,
                    options: vec![
                        ModelOption::Select {
                            id: "contextWindow".into(),
                            label: "Context Window".into(),
                            description: None,
                            choices: vec![OptionChoice::new("200k", "200k"), OptionChoice::new("1m", "1M")],
                            default: Some("200k".into()),
                            role: Some(OptionRole::ContextWindow),
                        },
                        ModelOption::Boolean { id: "thinking".into(), label: "Thinking".into(), description: Some("Thinks before answering".into()), default: true, role: None },
                    ],
                    legacy: true,
                    images: false,
                    custom: false,
                    via: None,
                    badge: None,
                }, ModelInfo { custom: true, ..ModelInfo::new("claude-opus-5-5-20261101", "Opus 5.5 (pinned)") },
                ModelInfo { via: Some("CLIProxyAPI".into()), ..ModelInfo::new("gemini-2.5-pro", "gemini-2.5-pro") }],
                update_required: vec![UpdateRequired { id: "claude-haiku-5-5".into(), name: "Claude Haiku 5.5".into(), min_version: "2.1.293".into() }],
                capabilities: Capabilities { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true, compact: true },
                checking: false,
                usage_limits: None,
            },
            ProviderInfo {
                id: ProviderId::codex(),
                name: "Codex".into(),
                installed: true,
                version: Some("0.164.0".into()),
                auth: AuthStatus::Missing,
                detail: Some("Run `codex login`".into()),
                models: vec![],
                update_required: vec![],
                capabilities: Capabilities { steer: true, fork: true, rollback: true, plan_mode: true, images: true, subagents: true, mode_switch: false, compact: true },
                checking: false,
                usage_limits: Some(UsageLimits {
                    checked_at: 1_780_000_000_000,
                    windows: vec![
                        UsageWindow {
                            resets_at: Some(1_780_010_000_000),
                            window_minutes: Some(300),
                            ..UsageWindow::new("primary", UsageWindowKind::Session, "Session", 42.5)
                        },
                        UsageWindow { window_minutes: Some(10080), ..UsageWindow::new("secondary", UsageWindowKind::Weekly, "Weekly", 100.0) },
                    ],
                    plan: Some("plus".into()),
                }),
            },
            // Still being probed, with no earlier probe to show.
            ProviderInfo {
                id: ProviderId("acp:grok".into()),
                name: "Grok Build".into(),
                installed: false,
                version: None,
                auth: AuthStatus::Unknown,
                detail: None,
                models: vec![],
                update_required: vec![],
                capabilities: Capabilities { steer: false, fork: false, rollback: false, plan_mode: true, images: true, subagents: false, mode_switch: true, compact: false },
                checking: true,
                usage_limits: None,
            },
        ],
        settings: Some(Box::new(MachineConfig {
            set: settings(),
            base: SettingsBase {
                worktree_dir: "/Users/ada/.local/share/roll/agent/worktrees".into(),
                branch_prefix: "roll".into(),
                clone_dir: Some("/Users/ada/roll".into()),
                pull_every: 60,
                max_depth: 3,
                max_children: 6,
                git_name: Some("Ada".into()),
                git_email: None,
            },
            providers: vec![
                ProviderBase { id: ProviderId::claude(), name: "Claude Code".into(), binary: None, args: 0, env: vec!["ANTHROPIC_API_KEY".into()] },
                ProviderBase { id: ProviderId::codex(), name: "Codex".into(), binary: Some("/opt/homebrew/bin/codex".into()), args: 1, env: vec![] },
            ],
            config_path: "/Users/ada/.local/share/roll/agent/config.toml".into(),
            endpoints: vec![
                EndpointInfo {
                    spec: endpoint_spec(),
                    key: KeyState::Sealed,
                    models: vec![EndpointModel { id: "gemini-2.5-pro".into(), name: None }, EndpointModel { id: "gpt-5.5".into(), name: Some("GPT-5.5".into()) }],
                    checked_at: Some(T),
                    error: None,
                },
                EndpointInfo {
                    spec: EndpointSpec {
                        id: "gemini".into(),
                        name: "Gemini key".into(),
                        kind: EndpointKind::Custom,
                        base_url: "https://generativelanguage.googleapis.com".into(),
                        providers: vec![ProviderId("acp:gemini".into())],
                        url_env: None,
                        key_env: Some("GEMINI_API_KEY".into()),
                        key_ref: Some("op://Dev/Gemini/credential".into()),
                        fallback: false,
                    },
                    key: KeyState::Reference,
                    models: vec![],
                    checked_at: None,
                    error: Some("1Password: the op CLI isn't signed in".into()),
                },
                EndpointInfo {
                    spec: EndpointSpec {
                        id: gateway_limits::ENDPOINT_ID.into(),
                        name: "roll gateway on ai-server".into(),
                        kind: EndpointKind::Gateway,
                        base_url: "http://ai-server.tail1234.ts.net:8790".into(),
                        providers: vec![ProviderId::claude(), ProviderId::codex()],
                        url_env: None,
                        key_env: None,
                        key_ref: None,
                        fallback: true,
                    },
                    key: KeyState::Sealed,
                    models: vec![EndpointModel { id: "fast".into(), name: None }],
                    checked_at: Some(T),
                    error: None,
                },
            ],
            gateway: Some(Box::new(GatewayInfo {
                settings: gateway_settings(),
                keys: vec![
                    UpstreamKey { upstream: "anthropic".into(), key: KeyState::Sealed },
                    UpstreamKey { upstream: "bedrock".into(), key: KeyState::Reference },
                    UpstreamKey { upstream: "local".into(), key: KeyState::None },
                ],
                clients: vec![gateway_client()],
                run: GatewayRun::Failed { error: format!("{} is in use", addr([100, 64, 0, 10], 8790)), addresses: vec![addr([127, 0, 0, 1], 8790)] },
            })),
            registry: Some(vec![RegistryInstalled {
                id: "goose".into(),
                version: "1.57.0".into(),
                provider: ProviderId("acp:goose".into()),
                kind: RegistryKind::Binary,
                installed_at: T,
            }]),
        })),
        seal_key: Some("hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo".into()),
    }
}

fn addr(ip: [u8; 4], port: u16) -> String {
    std::net::SocketAddr::from((ip, port)).to_string()
}

fn gateway_settings() -> GatewaySettings {
    let up = |id: &str, kind: UpstreamKind| UpstreamSpec {
        id: id.into(),
        name: kind.label().into(),
        kind,
        base_url: None,
        region: None,
        project: None,
        models: vec![],
        responses: false,
        headers: vec![],
        allow_private: false,
        key_ref: None,
        disabled: false,
    };
    GatewaySettings {
        enabled: true,
        port: 8790,
        listen: vec![std::net::Ipv4Addr::new(100, 64, 0, 10).to_string()],
        upstreams: vec![
            up("anthropic", UpstreamKind::Anthropic),
            UpstreamSpec {
                region: Some("us-east-1".into()),
                models: vec!["anthropic.claude-*".into()],
                key_ref: Some("op://Dev/Bedrock/credential".into()),
                ..up("bedrock", UpstreamKind::Bedrock)
            },
            UpstreamSpec { region: Some("us-east5".into()), project: Some("acme-ai".into()), disabled: true, ..up("vertex", UpstreamKind::Vertex) },
            UpstreamSpec { headers: vec![("http-referer".into(), "https://roll.sh".into())], ..up("openrouter", UpstreamKind::Openrouter) },
            UpstreamSpec { base_url: Some("https://acme.openai.azure.com".into()), ..up("azure", UpstreamKind::AzureOpenai) },
            UpstreamSpec { base_url: Some("https://gw.example.com".into()), ..up("compat", UpstreamKind::AnthropicCompatible) },
            UpstreamSpec { base_url: Some("https://api.openai.com/v1".into()), ..up("openai", UpstreamKind::Openai) },
            UpstreamSpec {
                base_url: Some(format!("http://{}/v1", addr([127, 0, 0, 1], 11434))),
                models: vec!["qwen3-coder*".into()],
                responses: true,
                allow_private: true,
                ..up("local", UpstreamKind::OpenaiCompatible)
            },
        ],
        routes: vec![RouteSpec {
            model: "fast".into(),
            targets: vec![
                RouteTarget { upstream: "anthropic".into(), model: Some("claude-haiku-4-5".into()) },
                RouteTarget { upstream: "bedrock".into(), model: Some("anthropic.claude-haiku-4-5".into()) },
            ],
        }],
        prices: vec![ModelPrice { model: "qwen3-coder*".into(), input: 0, output: 0, cache_read: 0, cache_write: 0 }],
        log_bodies: true,
        affinity: false,
        retention_days: 90,
    }
}

fn gateway_client() -> GatewayClient {
    GatewayClient { id: "gwc_5f1d2c3b4a596877".into(), name: "Ada's sandbox".into(), machine: Some(mch()), created_at: T }
}

fn log_entry() -> LogEntry {
    LogEntry {
        seq: 42,
        at: T,
        client: Some("gwc_5f1d2c3b4a596877".into()),
        machine: Some(mch()),
        project: Some(prj().0),
        thread: Some(thr().0),
        format: WireFormat::Responses,
        model: "fast".into(),
        stream: true,
        status: 200,
        ms: 4210,
        first_byte_ms: Some(690),
        attempts: vec![
            Attempt { upstream: "anthropic".into(), model: "claude-haiku-4-5".into(), status: 529, ms: 120, error: Some("overloaded".into()) },
            Attempt { upstream: "bedrock".into(), model: "anthropic.claude-haiku-4-5".into(), status: 200, ms: 4090, error: None },
        ],
        input_tokens: 12_000,
        output_tokens: 800,
        cache_read_tokens: 40_000,
        cache_write_tokens: 0,
        cost_micros: 19_200,
        error: None,
        has_body: true,
    }
}

fn gateway_plans() -> Vec<GatewayPlan> {
    vec![GatewayPlan {
        host: MachineId::from_suffix("00000000000000aa"),
        urls: vec![
            GatewayUrl { label: "Tailscale".into(), url: "http://ai-server.tail1234.ts.net:8790".into() },
            GatewayUrl { label: "Docker sandboxes".into(), url: "http://172.30.0.1:8790".into() },
        ],
        members: vec![
            GatewayMember { machine: mch(), url: "http://ai-server.tail1234.ts.net:8790".into(), providers: vec![], client: Some("gwc_5f1d2c3b4a596877".into()), joined_at: T, error: None },
            GatewayMember {
                machine: MachineId::from_suffix("00000000000000bb"),
                url: "http://172.30.0.1:8790".into(),
                providers: vec![ProviderId::claude()],
                client: None,
                joined_at: T,
                error: Some("the machine isn't connected".into()),
            },
        ],
        new_sandboxes: true,
        fallback: true,
        acknowledged_at: T,
        updated_at: T,
    }]
}

fn gateway_joins() -> Vec<GatewayJoin> {
    vec![
        GatewayJoin { machine: mch(), url: None, providers: vec![] },
        GatewayJoin { machine: mch(), url: Some("http://172.30.0.1:8790".into()), providers: vec![ProviderId::claude()] },
    ]
}

fn endpoint_spec() -> EndpointSpec {
    EndpointSpec {
        id: "cliproxy".into(),
        name: "CLIProxyAPI".into(),
        kind: EndpointKind::Cliproxy,
        base_url: "http://100.64.0.7:8317".into(),
        providers: vec![ProviderId::claude(), ProviderId::codex()],
        url_env: None,
        key_env: None,
        key_ref: None,
        fallback: false,
    }
}

fn sealed() -> SealedSecret {
    SealedSecret {
        v: SEAL_VERSION,
        epk: "3p7bfXt9wbTTW2HC7OQ1Nz-DQ8hbeGdNrfx-FG-IK08".into(),
        nonce: "AAECAwQFBgcICQoL".into(),
        ct: "bm90IGEgcmVhbCBrZXksIGp1c3QgYSBzaGFwZQ".into(),
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
        RelayFrame::Notice { message: "roll didn't keep 3 of this machine's records".into() },
        RelayFrame::Command { id: "cmd_1".into(), by: "acme".into(), command: Box::new(Command::RunInterrupt { thread: thr() }), grant: None },
        RelayFrame::Command {
            id: "cmd_2".into(),
            by: "agent:bot".into(),
            command: Box::new(Command::ThreadPublish { thread: thr(), message: "Fix the flaky queue test".into(), land: true }),
            grant: Some(Box::new(CodeGrant {
                repo: RepoRef { owner: "acme".into(), repo: "roll".into() },
                url: "https://roll.sh/acme/roll.git".into(),
                token: "rlg_0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                write: true,
                branch: Some("roll/fix-the-flaky-queue-test".into()),
                expires_at: T + 600_000,
            })),
        },
        RelayFrame::Ping,
    ]
}

fn covered_relay(f: &RelayFrame) {
    match f {
        RelayFrame::Welcome { .. } | RelayFrame::Ack { .. } | RelayFrame::Notice { .. } | RelayFrame::Command { .. } | RelayFrame::Ping => {}
    }
}

fn machine() -> Machine {
    Machine {
        id: mch(),
        name: "ada-mbp".into(),
        online: true,
        info: Some(info()),
        sandbox: Some(SandboxInfo { backend: "agent37".into(), size: "2cpu-4gb".into(), status: SandboxStatus::Running, hourly_cents: 3, stopped_reason: None }),
        appearance: MachineAppearance { color: Some(IdentityColor::Violet), kind: Some(MachineKind::Cloud) },
        created_at: T,
        last_seen: Some(T),
    }
}

/// A sandbox asleep (stopped while idle), and one on its way up again.
fn asleep() -> Vec<Machine> {
    let sandbox = |status, stopped_reason| Some(SandboxInfo { backend: "agent37".into(), size: "standard".into(), status, hourly_cents: 3, stopped_reason });
    vec![
        Machine { online: false, sandbox: sandbox(SandboxStatus::Stopped, Some(StopReason::Idle)), ..machine() },
        Machine { online: false, sandbox: sandbox(SandboxStatus::Stopped, Some(StopReason::Owner)), ..machine() },
        Machine { online: false, sandbox: sandbox(SandboxStatus::Starting, None), ..machine() },
    ]
}

fn stream() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Entity(Box::new(Mirrored { cursor: 7, machine: mch(), entity: Entity::Thread(thread()) })),
        StreamEvent::Machine { cursor: 8, machine: machine() },
        StreamEvent::MachineRemoved { cursor: 9, machine: mch() },
        StreamEvent::Delta { machine: mch(), delta: deltas().remove(1) },
        StreamEvent::Canvas { cursor: 10, revision: 3, updated_at: 1_762_000_000_000 },
        StreamEvent::Settings { cursor: 11, revision: 2, updated_at: 1_762_000_000_000 },
        StreamEvent::Automation { cursor: 12, automation: Box::new(automation_samples::automations().remove(1)) },
        StreamEvent::AutomationRemoved { cursor: 13, automation: automation_samples::aut(3) },
        StreamEvent::AutomationRun { cursor: 14, run: Box::new(automation_samples::run(automation::RunState::Waiting)) },
        StreamEvent::Notice { notice: automation_samples::notice() },
    ]
}

fn covered_stream(e: &StreamEvent) {
    match e {
        StreamEvent::Entity(_) | StreamEvent::Machine { .. } | StreamEvent::MachineRemoved { .. } | StreamEvent::Delta { .. } | StreamEvent::Canvas { .. } | StreamEvent::Settings { .. } => {}
        StreamEvent::Automation { .. } | StreamEvent::AutomationRemoved { .. } | StreamEvent::AutomationRun { .. } | StreamEvent::Notice { .. } => {}
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
    let machines = std::iter::once(machine()).chain(asleep()).collect();
    check("state", vec![State { cursor: 9, machines, entities: vec![Mirrored { cursor: 7, machine: mch(), entity: Entity::Project(project()) }] }]);
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
    check(
        "pending_logins",
        vec![
            PendingLogin { user_code: "WXYZ-2345".into(), name: "ada-mbp".into(), hostname: Some("ada-mbp.local".into()), os: Some("macos".into()), created_at: T, expires_at: T + 900_000 },
            PendingLogin { user_code: "BCDF-GHJK".into(), name: "machine".into(), hostname: None, os: None, created_at: T, expires_at: T + 900_000 },
        ],
    );
    check("machine_logins", vec![MachineLogin { machine: machine(), owner: "acme".into() }]);
    check(
        "endpoint_profiles",
        vec![
            EndpointProfile { spec: endpoint_spec(), sandbox_default: true, updated_at: T },
            EndpointProfile {
                spec: EndpointSpec { id: "gw".into(), name: "Team gateway".into(), kind: EndpointKind::Openai, base_url: "https://gw.example.com/v1".into(), providers: vec![ProviderId::codex()], url_env: None, key_env: None, key_ref: None, fallback: false },
                sandbox_default: false,
                updated_at: T,
            },
        ],
    );
    check("gateway_plans", gateway_plans());
    check("gateway_joins", gateway_joins());
    check(
        "shares",
        vec![Share {
            id: "shr_0123456789abcdef01234567".into(),
            owner: "acme".into(),
            title: "Fix the flaky queue test".into(),
            thread: thr(),
            subtree: true,
            machine: SharedMachine { name: "ada-mbp".into(), os: Some("macos".into()) },
            created_at: T,
            entities: entities().into_iter().filter(|e| !matches!(e, Entity::Terminal(_) | Entity::Removed(_))).collect(),
            trimmed: false,
        }],
    );
    check(
        "share_summaries",
        vec![ShareSummary {
            id: "shr_0123456789abcdef01234567".into(),
            title: "Fix the flaky queue test".into(),
            thread: thr(),
            subtree: false,
            url: "https://roll.sh/agents/s/shr_0123456789abcdef01234567".into(),
            threads: 1,
            runs: 2,
            bytes: 18_432,
            created_at: T,
            created_by: "acme".into(),
        }],
    );
}

#[test]
fn every_variant_of_every_tagged_enum_is_sampled() {
    // A variant listed in a covered_* match but never sampled still slips through; count tags.
    let tags = |v: serde_json::Value, key: &str| -> std::collections::BTreeSet<String> {
        v.as_array().unwrap().iter().map(|x| x[key].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(tags(serde_json::to_value(commands()).unwrap(), "type").len(), 55, "commands");
    assert_eq!(tags(serde_json::to_value(replies()).unwrap(), "type").len(), 20, "replies");
    assert_eq!(tags(serde_json::to_value(entities()).unwrap(), "kind").len(), 8, "entities");
    let bodies: std::collections::BTreeSet<String> = entities()
        .iter()
        .filter_map(|e| if let Entity::Node(n) = e { Some(serde_json::to_value(&n.body).unwrap()["type"].as_str().unwrap().to_string()) } else { None })
        .collect();
    assert_eq!(bodies.len(), 9, "node bodies");
}

#[test]
fn scopes_and_threads() {
    for e in entities() {
        let detail = matches!(e.kind(), EntityKind::Node | EntityKind::Checkpoint);
        assert_eq!(e.scope() == Scope::Detail, detail, "{:?}", e.kind());
        match &e {
            Entity::Project(_) | Entity::Terminal(_) => {}
            Entity::Thread(t) => assert_eq!(e.thread(), Some(&t.id)),
            _ => assert_eq!(e.thread(), Some(&thr()), "{:?}", e.kind()),
        }
    }
}

#[test]
fn records_from_before_settling_parse() {
    let mut thread = serde_json::to_value(thread()).unwrap();
    thread.as_object_mut().unwrap().remove("inbox");
    assert_eq!(*serde_json::from_value::<Thread>(thread).unwrap().inbox, Inbox::default());
    // Projects from before keep T3's defaults: three days, and on landing.
    let settings: ProjectSettings = serde_json::from_value(json!({ "auto_pull": true })).unwrap();
    assert_eq!(settings.auto_settle, AutoSettle { after_days: Some(3), on_land: true, keep_changes: false });
    let never: AutoSettle = serde_json::from_value(json!({ "after_days": null })).unwrap();
    assert_eq!(never, AutoSettle { after_days: None, on_land: true, keep_changes: false });
    assert_eq!(serde_json::to_value(never).unwrap(), json!({ "after_days": null, "on_land": true }), "never stays never");
    assert_eq!(serde_json::to_value(ProjectSettings::default()).unwrap()["auto_settle"], json!({ "after_days": 3, "on_land": true }));
    // Cleanup is off until a project turns it on, as in T3, and leaves no trace while off.
    assert_eq!(settings.cleanup, Cleanup { worktree_after_days: None, keep: CleanupKeep::UncommittedChanges });
    assert!(serde_json::to_value(ProjectSettings::default()).unwrap().get("cleanup").is_none());
    let worktree: Worktree = serde_json::from_value(json!({ "path": "/w", "branch": "roll/x", "base": "abc" })).unwrap();
    assert_eq!(worktree.removed_at, None);
}

#[test]
fn records_from_before_context_usage_parse() {
    let mut thread = serde_json::to_value(thread()).unwrap();
    thread.as_object_mut().unwrap().remove("context");
    assert_eq!(serde_json::from_value::<Thread>(thread).unwrap().context, None);
    let mut run = serde_json::to_value(run_rec()).unwrap();
    run.as_object_mut().unwrap().remove("context");
    assert_eq!(serde_json::from_value::<Run>(run).unwrap().context, None);
}

#[test]
fn context_percent_needs_a_window() {
    let c = |used, max| ContextUsage { used_tokens: used, max_tokens: max, total_processed_tokens: None, ..Default::default() };
    assert_eq!(c(50_000, Some(200_000)).used_percent(), Some(25.0));
    assert_eq!(c(250_000, Some(200_000)).used_percent(), Some(100.0));
    assert_eq!(c(50_000, None).used_percent(), None);
    assert_eq!(c(50_000, Some(0)).used_percent(), None);
}
