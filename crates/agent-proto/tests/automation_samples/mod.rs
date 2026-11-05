//! Samples of the automation wire types, for `fixtures.rs` (stream events) and `automations.rs`
//! (their own fixture files).
#![allow(dead_code)]

use agent_proto::automation::*;
use agent_proto::{ApprovalAction, Decision, InteractionMode, MachineId, Millis, ProviderId, RuntimeMode, SendMode, ThreadId};
use serde_json::json;
use std::collections::BTreeMap;

pub const T: Millis = 1_762_000_000_000;

pub fn aut(n: u8) -> AutomationId {
    AutomationId::from_suffix(&format!("{n:016x}"))
}
pub fn arn() -> AutomationRunId {
    AutomationRunId::from_suffix("0a0b0c0d0e0f1011")
}
pub fn aev() -> AutomationEventId {
    AutomationEventId::from_suffix("1213141516171819")
}

fn step(id: &str, kind: &str, config: serde_json::Value) -> Step {
    Step { id: id.into(), name: String::new(), kind: kind.into(), config, when: vec![], on_error: OnError::Fail, retry: None, timeout_s: None }
}

fn cfg<T: serde::Serialize>(t: T) -> serde_json::Value {
    serde_json::to_value(t).unwrap()
}

/// Every built-in step kind, with its typed config.
pub fn every_step() -> Vec<Step> {
    vec![
        Step {
            name: "Start the fix".into(),
            when: vec![Filter { field: "event.data.change.draft".into(), op: FilterOp::Equals, value: json!(false), not: false }],
            retry: Some(Retry { attempts: 3, backoff_s: 30 }),
            timeout_s: Some(600),
            ..step(
                "start",
                step_kinds::AGENT_START,
                cfg(AgentStart {
                    target: Some(Target::Sandbox { size: "large".into(), keep: true }),
                    project: Some(ProjectRef::Repo { repo: "acme/app".into(), path: Some("app".into()) }),
                    provider: Some(ProviderId::claude()),
                    model: Some("claude-opus-5-5".into()),
                    effort: Some("high".into()),
                    options: Default::default(),
                    runtime_mode: Some(RuntimeMode::AutoEdit),
                    interaction_mode: Some(InteractionMode::Plan),
                    worktree: Some(true),
                    title: Some("Fix {{event.data.change.title}}".into()),
                    prompt: "The checks of {{event.data.change.url}} fail. Fix them.".into(),
                    orchestrator: true,
                }),
            )
        },
        step(
            "wait",
            step_kinds::AGENT_WAIT,
            cfg(AgentWait { thread: Some("{{steps.start.thread}}".into()), until: vec![WaitFor::Done, WaitFor::Failed, WaitFor::Waiting, WaitFor::Settled], timeout_s: Some(3600), on_timeout: OnError::Continue }),
        ),
        step("nudge", step_kinds::AGENT_MESSAGE, cfg(AgentMessage { thread: None, text: "Run the tests again.".into(), mode: SendMode::Steer })),
        step(
            "approve",
            step_kinds::AGENT_RESPOND,
            cfg(AgentRespond {
                thread: None,
                rules: vec![
                    RespondRule { action: Some(ApprovalAction::Command), command: Some("cargo *".into()), files: None, tool: None, decision: Decision::Accept },
                    RespondRule { action: Some(ApprovalAction::FileChange), command: None, files: Some("docs/**".into()), tool: Some("Edit".into()), decision: Decision::AcceptForSession },
                ],
                otherwise: Some(Decision::Decline),
                answers: BTreeMap::from([("scope".into(), "{{event.data.input.scope}}".into())]),
                first_option: true,
                message: Some("Automations only run cargo.".into()),
            }),
        ),
        Step { on_error: OnError::Continue, ..step("publish", step_kinds::AGENT_PUBLISH, cfg(AgentPublish { thread: None, message: Some("Fix checks".into()), land: true })) },
        step(
            "test",
            step_kinds::AGENT_SCRIPT,
            cfg(AgentScript { script: "test".into(), thread: None, project: Some(ProjectRef::Id { id: "prj_1234567890abcdef".into() }), target: Some(Target::ProjectMachine) }),
        ),
        step("settle", step_kinds::AGENT_SETTLE, cfg(AgentSettle { thread: None, action: SettleAction::Archive })),
        step(
            "tell",
            step_kinds::WEBHOOK,
            cfg(WebhookCall {
                url: "https://hooks.example.com/roll".into(),
                method: "POST".into(),
                headers: BTreeMap::from([("authorization".into(), "Bearer {{secrets.EXAMPLE_TOKEN}}".into())]),
                body: json!({ "text": "{{event.data.change.title}} is fixed" }),
                secret: Some("SIGNING".into()),
                ok_statuses: vec![409],
            }),
        ),
        step(
            "toast",
            step_kinds::NOTIFY,
            cfg(NotifyConfig { title: "Fixed {{event.data.change.title}}".into(), body: "{{steps.publish.url}}".into(), level: NoticeLevel::Success, link: Some("{{steps.publish.url}}".into()) }),
        ),
        step("later", step_kinds::DELAY, cfg(DelayConfig { seconds: 300 })),
        step("comment", "linear.comment", json!({ "issue": "{{event.data.issue.id}}", "body": "On it" })),
    ]
}

pub fn spec(name: &str, trigger: Trigger) -> AutomationSpec {
    AutomationSpec {
        name: name.into(),
        description: String::new(),
        enabled: true,
        trigger,
        filters: vec![],
        steps: vec![step("toast", step_kinds::NOTIFY, cfg(NotifyConfig { title: "Hello".into(), body: String::new(), level: NoticeLevel::Info, link: None }))],
        defaults: AgentDefaults::default(),
        concurrency: Concurrency::Parallel,
        rate_limit: None,
        run_as: None,
        allow_agents: false,
    }
}

fn automation(n: u8, spec: AutomationSpec) -> Automation {
    Automation {
        id: aut(n),
        spec,
        revision: 1,
        cursor: 20 + n as u64,
        created_at: T,
        updated_at: T,
        created_by: "acme".into(),
        secrets: vec![],
        webhook: None,
        next_at: None,
        last_run: None,
    }
}

/// Every trigger, filter op, concurrency policy, target and project ref.
pub fn automations() -> Vec<Automation> {
    let filters = vec![
        Filter { field: "event.data.repo".into(), op: FilterOp::Equals, value: json!("acme/app"), not: false },
        Filter { field: "event.data.change.title".into(), op: FilterOp::Contains, value: json!("flaky"), not: true },
        Filter { field: "event.data.kind".into(), op: FilterOp::AnyOf, value: json!(["landed", "queue"]), not: false },
        Filter { field: "event.data.change.labels".into(), op: FilterOp::HasLabel, value: json!("bug"), not: false },
        Filter { field: "event.data.files".into(), op: FilterOp::Glob, value: json!("src/**/*.rs"), not: false },
        Filter { field: "event.data.change.url".into(), op: FilterOp::Exists, value: json!(null), not: false },
        Filter { field: "event.data.change.number".into(), op: FilterOp::Gt, value: json!(10), not: false },
        Filter { field: "event.data.change.number".into(), op: FilterOp::Lt, value: json!(1000), not: false },
    ];
    vec![
        automation(
            1,
            AutomationSpec {
                description: "When the checks of a change fail, start an agent in a sandbox to fix them.".into(),
                filters,
                steps: every_step(),
                defaults: AgentDefaults {
                    target: Some(Target::AnyOnline),
                    project: Some(ProjectRef::Id { id: "prj_1234567890abcdef".into() }),
                    provider: Some(ProviderId::codex()),
                    model: Some("gpt-5.5".into()),
                    effort: Some("medium".into()),
                    options: Default::default(),
                    runtime_mode: Some(RuntimeMode::FullAccess),
                    interaction_mode: Some(InteractionMode::Default),
                    worktree: Some(true),
                },
                concurrency: Concurrency::CancelPrevious,
                rate_limit: Some(RateLimit { max: 10, per_s: 3600 }),
                run_as: Some("fixer".into()),
                allow_agents: true,
                ..spec("Fix failing checks", Trigger::Code { events: vec!["check".into(), "change".into()], repos: vec!["acme/app".into()] })
            },
        ),
        Automation {
            next_at: Some(T + 3_600_000),
            last_run: Some(RunBrief { id: arn(), status: RunState::Succeeded, at: T }),
            secrets: vec!["EXAMPLE_TOKEN".into(), "SIGNING".into()],
            ..automation(2, AutomationSpec { concurrency: Concurrency::Skip, ..spec("Nightly triage", Trigger::Schedule { cron: "0 3 * * 1-5".into(), timezone: "Europe/Budapest".into() }) })
        },
        automation(3, AutomationSpec { concurrency: Concurrency::Queue, ..spec("Every hour", Trigger::Interval { every_s: 3600 }) }),
        Automation {
            webhook: Some(WebhookIngress { url: "https://api.roll.sh/v1/code/agents/acme/automations/aut_0000000000000004/webhook".into(), secret_hint: "9f3a".into() }),
            ..automation(4, AutomationSpec { enabled: false, ..spec("From CI", Trigger::Webhook) })
        },
        automation(5, spec("By hand", Trigger::Manual)),
        automation(6, spec("Agents", Trigger::Agent { events: AgentEvent::ALL.to_vec() })),
        automation(
            7,
            spec("Linear issues", Trigger::Integration { source: "linear".into(), events: vec!["issue.created".into()], config: json!({ "team": "ENG" }) }),
        ),
        automation(8, spec("Every machine target", Trigger::Code { events: vec![], repos: vec![] })),
    ]
    .into_iter()
    .map(|mut a| {
        if a.id == aut(8) {
            a.spec.defaults.target = Some(Target::Machine { machine: "ada-mbp".into() });
        }
        a
    })
    .collect()
}

pub fn saved() -> Vec<AutomationSaved> {
    let a = automations();
    vec![AutomationSaved { automation: a[3].clone(), webhook_secret: Some("rlh_0123456789abcdef0123456789abcdef".into()) }, AutomationSaved { automation: a[0].clone(), webhook_secret: None }]
}

pub fn event() -> AutomationEvent {
    AutomationEvent {
        id: aev(),
        source: "code".into(),
        kind: "check".into(),
        key: "code:ob_0123456789abcd".into(),
        at: T,
        data: json!({ "repo": "acme/app", "change": { "number": 12, "title": "Fix the queue", "url": "https://roll.sh/acme/app/changes/12" } }),
        automation: None,
    }
}

fn step_run(id: &str, kind: &str, status: StepState) -> StepRun {
    StepRun { id: id.into(), kind: kind.into(), status, attempt: 1, started_at: Some(T), finished_at: None, next_at: None, output: json!(null), error: None, log: vec![] }
}

/// Every run and step state.
pub fn run(status: RunState) -> AutomationRun {
    AutomationRun {
        id: arn(),
        automation: aut(1),
        name: "Fix failing checks".into(),
        status,
        event: EventBrief { id: aev(), source: "code".into(), kind: "check".into(), summary: "acme/app#12: check test failed".into(), at: T },
        steps: vec![
            StepRun {
                finished_at: Some(T + 4000),
                output: json!({ "machine": "mch_fedcba0987654321", "project": "prj_1234567890abcdef", "thread": "thr_a1b2c3d4e5f60718", "sandbox": "sbx_1" }),
                log: vec![
                    LogLine { at: T + 1000, level: LogLevel::Info, text: "Creating a large sandbox".into() },
                    LogLine { at: T + 3000, level: LogLevel::Warn, text: "The sandbox took 2 s to connect".into() },
                ],
                ..step_run("start", step_kinds::AGENT_START, StepState::Succeeded)
            },
            StepRun { next_at: Some(T + 60_000), ..step_run("wait", step_kinds::AGENT_WAIT, StepState::Waiting) },
            StepRun {
                attempt: 2,
                next_at: Some(T + 90_000),
                error: Some("hooks.example.com answered 503".into()),
                log: vec![LogLine { at: T + 5000, level: LogLevel::Error, text: "503 Service Unavailable".into() }],
                ..step_run("tell", step_kinds::WEBHOOK, StepState::Retrying)
            },
            step_run("nudge", step_kinds::AGENT_MESSAGE, StepState::Running),
            StepRun { attempt: 0, started_at: None, ..step_run("toast", step_kinds::NOTIFY, StepState::Pending) },
            StepRun { error: Some("the thread isn't there".into()), ..step_run("publish", step_kinds::AGENT_PUBLISH, StepState::Failed) },
            StepRun { attempt: 0, ..step_run("test", step_kinds::AGENT_SCRIPT, StepState::Skipped) },
            StepRun { attempt: 0, ..step_run("settle", step_kinds::AGENT_SETTLE, StepState::Cancelled) },
        ],
        threads: vec![RunThread { machine: MachineId::from_suffix("fedcba0987654321"), thread: ThreadId::from_suffix("a1b2c3d4e5f60718"), step: "start".into() }],
        cursor: 30,
        created_at: T,
        started_at: Some(T),
        finished_at: status.is_final().then_some(T + 120_000),
        reason: matches!(status, RunState::Failed | RunState::Skipped | RunState::Cancelled).then(|| "another run was going".to_string()),
    }
}

pub fn runs() -> Vec<RunDetail> {
    [RunState::Queued, RunState::Running, RunState::Waiting, RunState::Succeeded, RunState::Failed, RunState::Cancelled, RunState::Skipped]
        .into_iter()
        .map(|s| RunDetail { run: run(s), event: event() })
        .chain(std::iter::once(RunDetail {
            run: AutomationRun { automation: aut(5), ..run(RunState::Succeeded) },
            event: AutomationEvent {
                source: "manual".into(),
                kind: "run".into(),
                key: "manual:key-1".into(),
                data: json!({ "by": "acme", "input": { "scope": "api" } }),
                automation: Some(aut(5)),
                ..event()
            },
        }))
        .collect()
}

pub fn notice() -> Notice {
    Notice {
        id: "ntc_0123456789abcd".into(),
        title: "Fixed Fix the queue".into(),
        body: "https://roll.sh/acme/app/changes/12".into(),
        level: NoticeLevel::Success,
        link: Some("https://roll.sh/acme/app/changes/12".into()),
        automation: Some(aut(1)),
        run: Some(arn()),
        at: T,
    }
}

pub fn notices() -> Vec<Notice> {
    let plain = |level| Notice { id: "ntc_1".into(), title: "Hi".into(), body: String::new(), level, link: None, automation: None, run: None, at: T };
    vec![notice(), plain(NoticeLevel::Info), plain(NoticeLevel::Warning), plain(NoticeLevel::Error)]
}

pub fn catalog() -> Catalog {
    Catalog {
        triggers: vec![
            TriggerInfo { source: "manual".into(), title: "By hand".into(), description: "Run it from the dashboard, the CLI or MCP.".into(), events: vec![EventInfo { kind: "run".into(), title: "Run".into(), fields: vec!["input".into(), "by".into()], sample: json!({ "by": "acme", "input": {} }) }], config: json!(null), needs_setup: false },
            TriggerInfo {
                source: "linear".into(),
                title: "Linear".into(),
                description: "Issues and comments in Linear.".into(),
                events: vec![EventInfo { kind: "issue.created".into(), title: "Issue created".into(), fields: vec!["issue.title".into(), "issue.labels".into()], sample: json!(null) }],
                config: json!({ "type": "object", "properties": { "team": { "type": "string" } } }),
                needs_setup: true,
            },
        ],
        steps: vec![
            StepInfo { kind: step_kinds::NOTIFY.into(), title: "Notify".into(), description: "A notice in the dashboard.".into(), config: json!({ "type": "object", "required": ["title"] }), output: vec!["id".into()], integration: None, needs_setup: false },
            StepInfo { kind: "linear.comment".into(), title: "Comment on the issue".into(), description: "A comment in Linear.".into(), config: json!({ "type": "object" }), output: vec!["comment".into()], integration: Some("linear".into()), needs_setup: true },
        ],
    }
}

pub fn tests() -> Vec<TestRequest> {
    vec![TestRequest { automation: automations().remove(0).spec, event: TestEvent { source: "code".into(), kind: "check".into(), data: json!({ "repo": "acme/app" }) } }]
}

pub fn test_results() -> Vec<TestResult> {
    vec![TestResult {
        matched: false,
        trigger: true,
        filters: vec![true, false],
        steps: vec![
            TestStep { id: "start".into(), runs: true, config: json!({ "prompt": "Fix acme/app" }), error: None },
            TestStep { id: "comment".into(), runs: false, config: json!(null), error: Some("no step kind linear.comment".into()) },
        ],
    }]
}

pub fn run_now() -> Vec<RunNow> {
    vec![RunNow { input: json!({ "scope": "api" }), key: Some("key-1".into()) }, RunNow::default()]
}
