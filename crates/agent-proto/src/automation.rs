//! roll code automations (docs/code/automations.md): the wire types. No I/O.
//!
//! An automation is a trigger, filters on the event it brings, and steps. Every event a trigger
//! sees goes into the owner's durable inbox first (an [`AutomationEvent`] with an idempotency
//! key); each automation it matches gets one [`AutomationRun`], whose [`StepRun`]s run in order
//! with retries and survive restarts. The relay (`code-control/src/agents/automations/`) keeps
//! all of it; the dashboard mirrors these types in `apps/dashboard/src/lib/automations-api.ts`
//! and parses the same fixtures (`fixtures/automation*.json`).
//!
//! Steps are open: `kind` names a step from the relay's registry (the built-in ones are
//! [`step_kinds`]; integrations add theirs, e.g. `linear.comment`) and `config` is that kind's
//! JSON, described by its JSON Schema in the [`Catalog`]. The built-in kinds' configs are typed
//! here ([`AgentStart`] and the rest) so both sides agree on them.

use crate::{ApprovalAction, Decision, InteractionMode, MachineId, Millis, ModelOptions, ProviderId, RuntimeMode, SendMode, ThreadId, is_false};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub use crate::{AutomationEventId, AutomationId, AutomationRunId};

// ---------------------------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------------------------

/// What the owner writes: everything about an automation that isn't the server's.
/// `POST …/automations` takes one; `PUT …/automations/{id}` replaces it whole.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationSpec {
    /// One line, at most [`limits::NAME`] characters.
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub trigger: Trigger,
    /// All of them must hold for the event to start a run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    /// Run in order, 1 to [`limits::STEPS`].
    pub steps: Vec<Step>,
    /// What `agent.start` steps use where they say nothing.
    #[serde(default, skip_serializing_if = "AgentDefaults::is_empty")]
    pub defaults: AgentDefaults,
    #[serde(default)]
    pub concurrency: Concurrency,
    /// Unset: [`limits::DEFAULT_RATE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Runs act as the agent `agent:<name>` (its narrower rights: docs/code/agents.md) instead of
    /// as the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
    /// The owner's agent tokens may run it by hand (`POST …/run`, the MCP tool).
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_agents: bool,
}

fn yes() -> bool {
    true
}

/// An automation as the relay keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Automation {
    pub id: AutomationId,
    #[serde(flatten)]
    pub spec: AutomationSpec,
    /// Saves so far; `PUT` with an older one is a 409.
    pub revision: u64,
    /// The owner cursor of its last change (the `automation` stream event).
    pub cursor: u64,
    pub created_at: Millis,
    pub updated_at: Millis,
    /// Who created it (the owner's name, `admin`, or `agent:<name>`).
    pub created_by: String,
    /// The names of its secrets (set with `PUT …/secrets/{name}`); their values never leave the
    /// server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Set for a `webhook` trigger: where to send events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookIngress>,
    /// When a `schedule` or `interval` trigger fires next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<RunBrief>,
}

/// An inbound webhook's address. The secret is shown once, when it's made
/// ([`AutomationSaved::webhook_secret`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookIngress {
    pub url: String,
    /// Its last four characters, so the owner can tell which one a sender has.
    pub secret_hint: String,
}

/// `POST …/automations`, `PUT …/automations/{id}` and `POST …/{id}/webhook/secret`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationSaved {
    pub automation: Automation,
    /// A new inbound webhook secret (`rlh_…`): only in the answer that made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
}

/// The newest run, on the automation's record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBrief {
    pub id: AutomationRunId,
    pub status: RunState,
    pub at: Millis,
}

/// What starts runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Only by hand: the dashboard's button, `roll agent automation run`, the MCP tool.
    Manual,
    /// A cron expression (5 fields, or 6 with seconds) in an IANA time zone.
    Schedule {
        cron: String,
        #[serde(default = "utc")]
        timezone: String,
    },
    /// Every so many seconds (at least [`limits::MIN_INTERVAL_S`]).
    Interval { every_s: u64 },
    /// `POST` to the automation's own URL, signed with its secret ([`WebhookIngress`]).
    Webhook,
    /// roll code events of the owner's repos (the webhook events: `landed`, `change`, `review`,
    /// `check`, `queue`, `work`, `preview`, `push`, `thread`); empty: all of them. `repos` are
    /// names (`repo` or `owner/repo`); empty: every repo of the owner.
    Code {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        repos: Vec<String>,
    },
    /// The owner's agents: threads finishing, failing, waiting, settling, requests opening.
    Agent { events: Vec<AgentEvent> },
    /// An integration's events (`linear`, `github`, `slack`, …: whatever the relay's registry
    /// has), with that integration's config (its JSON Schema is in the [`Catalog`]).
    Integration {
        source: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<String>,
        #[serde(default, skip_serializing_if = "Value::is_null", deserialize_with = "config_fields")]
        config: Value,
    },
}

fn utc() -> String {
    "UTC".into()
}

impl Trigger {
    /// The event source it listens to (the inbox's `source`).
    pub fn source(&self) -> &str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Schedule { .. } => "schedule",
            Trigger::Interval { .. } => "interval",
            Trigger::Webhook => "webhook",
            Trigger::Code { .. } => "code",
            Trigger::Agent { .. } => "agent",
            Trigger::Integration { source, .. } => source,
        }
    }
}

/// What the owner's agents do that a trigger can listen for. `thread.*` events carry the thread
/// (and its run); `request.opened` the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A thread was started.
    #[serde(rename = "thread.created")]
    ThreadCreated,
    /// A run completed.
    #[serde(rename = "thread.done")]
    ThreadDone,
    /// A run failed.
    #[serde(rename = "thread.failed")]
    ThreadFailed,
    /// A thread started waiting on its user (an approval or a question).
    #[serde(rename = "thread.waiting")]
    ThreadWaiting,
    /// A thread was settled (by hand, inactivity, its change landing or closing).
    #[serde(rename = "thread.settled")]
    ThreadSettled,
    /// An approval or a question was opened.
    #[serde(rename = "request.opened")]
    RequestOpened,
}

impl AgentEvent {
    pub const ALL: [AgentEvent; 6] =
        [AgentEvent::ThreadCreated, AgentEvent::ThreadDone, AgentEvent::ThreadFailed, AgentEvent::ThreadWaiting, AgentEvent::ThreadSettled, AgentEvent::RequestOpened];

    /// The event's `kind` in the inbox.
    pub fn kind(self) -> &'static str {
        match self {
            AgentEvent::ThreadCreated => "thread.created",
            AgentEvent::ThreadDone => "thread.done",
            AgentEvent::ThreadFailed => "thread.failed",
            AgentEvent::ThreadWaiting => "thread.waiting",
            AgentEvent::ThreadSettled => "thread.settled",
            AgentEvent::RequestOpened => "request.opened",
        }
    }
}

/// A condition on the run's context (`event.…`, `steps.<id>.…`, `run.…`): `field` is a dotted
/// path, as in templates without the braces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: FilterOp,
    /// What `op` compares with (unused by `exists`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub value: Value,
    /// The opposite.
    #[serde(default, skip_serializing_if = "is_false")]
    pub not: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    /// The same JSON value (a number and its string are equal: `"3"` = `3`).
    Equals,
    /// A string holds `value` (case-insensitive), or a list has it.
    Contains,
    /// Equals one of `value` (a list).
    AnyOf,
    /// A list of labels (strings, or objects with a `name`) has `value` (case-insensitive).
    HasLabel,
    /// A string matches the glob `value` (roll code's glob rules: `*` within a path segment,
    /// `?`, `**` only as a whole segment, never recursive backtracking).
    Glob,
    /// The field is there and not null.
    Exists,
    /// A number greater than `value`.
    Gt,
    /// A number less than `value`.
    Lt,
}

/// One step: a kind from the registry, its config, and how it's run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Unique in the automation: `[a-z0-9_]`, 1 to 32 characters. Later steps read its output as
    /// `steps.<id>.…`.
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// `agent.start`, `webhook`, `linear.comment`, …
    pub kind: String,
    /// The kind's config. Strings in it are templates (`{{event.…}}`). A field that is `null` is
    /// unset (see [`config_fields`]).
    #[serde(default, skip_serializing_if = "Value::is_null", deserialize_with = "config_fields")]
    pub config: Value,
    /// Run only when all of these hold (on the event and the steps before); otherwise skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<Filter>,
    #[serde(default, skip_serializing_if = "OnError::is_fail")]
    pub on_error: OnError,
    /// Unset: the kind's own (most retry transient failures 3 times).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    /// The longest the step may take, waits included (unset: the kind's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

/// A config (a step's, an integration trigger's) as parsed: its fields that are `null` are unset,
/// so they're dropped, and every kind reads `{"headers": null}` as `{}`. The builder used to send
/// `null` for a field nobody filled in (templates still may), and a kind's `#[serde(default)]`
/// map, list or flag refused it ("invalid type: null, expected a map"). Only the config's own
/// fields: a `null` deeper in is the user's data (a webhook's JSON body).
fn config_fields<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    let mut v = Value::deserialize(d)?;
    if let Value::Object(m) = &mut v {
        m.retain(|_, f| !f.is_null());
    }
    Ok(v)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// The run fails.
    #[default]
    Fail,
    /// The run goes on; later steps see `steps.<id>.error`.
    Continue,
}

impl OnError {
    pub fn is_fail(&self) -> bool {
        *self == OnError::Fail
    }
}

/// Attempts after a transient failure, with exponential backoff from `backoff_s` (×2 each time,
/// at most an hour).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retry {
    /// Attempts in all, 1 to [`limits::ATTEMPTS`].
    pub attempts: u32,
    pub backoff_s: u64,
}

/// What a new run does while another run of the same automation is going.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// Run alongside it (at most [`limits::PARALLEL`] at once; past that they queue).
    #[default]
    Parallel,
    /// Don't run (the run is recorded as `skipped`).
    Skip,
    /// Wait for it to finish.
    Queue,
    /// Cancel it, then run.
    CancelPrevious,
}

/// At most `max` runs start per `per_s` seconds; past it new runs are `skipped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub max: u32,
    pub per_s: u64,
}

/// `agent.start`'s defaults for the whole automation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The model's other options (`Thread::options`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ModelOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_mode: Option<RuntimeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_mode: Option<InteractionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
}

impl AgentDefaults {
    pub fn is_empty(&self) -> bool {
        *self == AgentDefaults::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The built-in steps' configs
// ---------------------------------------------------------------------------------------------

/// The built-in step kinds. Integrations register more (`<integration>.<action>`).
pub mod step_kinds {
    pub const AGENT_START: &str = "agent.start";
    pub const AGENT_WAIT: &str = "agent.wait";
    pub const AGENT_MESSAGE: &str = "agent.message";
    pub const AGENT_RESPOND: &str = "agent.respond";
    pub const AGENT_PUBLISH: &str = "agent.publish";
    pub const AGENT_SCRIPT: &str = "agent.script";
    pub const AGENT_SETTLE: &str = "agent.settle";
    pub const WEBHOOK: &str = "webhook";
    pub const NOTIFY: &str = "notify";
    pub const DELAY: &str = "delay";
    pub const ALL: [&str; 10] = [AGENT_START, AGENT_WAIT, AGENT_MESSAGE, AGENT_RESPOND, AGENT_PUBLISH, AGENT_SCRIPT, AGENT_SETTLE, WEBHOOK, NOTIFY, DELAY];
}

/// Where an agent runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Target {
    /// This machine (its id `mch_…` or its name).
    Machine { machine: String },
    /// A new sandbox of this size (`standard`, `large`, …), deleted when the run ends unless
    /// `keep`.
    Sandbox {
        #[serde(default = "standard")]
        size: String,
        #[serde(default, skip_serializing_if = "is_false")]
        keep: bool,
    },
    /// Any connected machine that has the project (a repo project: cloned onto one if none has).
    AnyOnline,
    /// The machine the project is on (a project by id).
    ProjectMachine,
}

fn standard() -> String {
    "standard".into()
}

/// Which project an agent works in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectRef {
    /// A project by its id (`prj_…`).
    Id { id: String },
    /// The project that is a checkout of this roll code repo (`repo` or `owner/repo`) on the
    /// target machine, cloned there (into `path`, default the clone dir) when it has none.
    Repo {
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

/// `agent.start`: start a thread. Unset fields come from the automation's [`AgentDefaults`],
/// then the project's own defaults; `prompt` and `title` are templates. Output: `machine`,
/// `project`, `thread` (and `sandbox` for a new sandbox).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentStart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The model's other options (`Thread::options`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ModelOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_mode: Option<RuntimeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_mode: Option<InteractionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub prompt: String,
    /// Start an orchestrator: the prompt is a goal it plans and delegates.
    #[serde(default, skip_serializing_if = "is_false")]
    pub orchestrator: bool,
}

/// What `agent.wait` waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    /// Its last run completed and nothing is queued.
    Done,
    /// Its last run failed (or its session died).
    Failed,
    /// It waits on its user.
    Waiting,
    /// It was settled.
    Settled,
}

/// `agent.wait`: wait until the thread does one of `until` (default done or failed), at most
/// `timeout_s` (default an hour; past it the step fails, or succeeds with `outcome: "timeout"`
/// when `on_timeout` is `continue`). Output: `outcome` (`done`, `failed`, `waiting`, `settled`,
/// `timeout`), `thread`, `run`, `status`, `error`, `message` (the agent's last text, bounded).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentWait {
    /// A thread id (a template, e.g. `{{steps.start.thread}}`); unset: the run's latest thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<WaitFor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    #[serde(default, skip_serializing_if = "OnError::is_fail")]
    pub on_timeout: OnError,
}

/// `agent.message`: send `text` (a template) to the thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub text: String,
    #[serde(default)]
    pub mode: SendMode,
}

/// `agent.respond`: answer the thread's pending requests by policy. Approvals take the first
/// rule that matches them, else `otherwise` (unset: left for the owner). Questions take
/// `answers` (by question id or header; templates), else their first option when
/// `first_option`, else are left. Output: `answered` and `left` (request ids).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRespond {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RespondRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otherwise: Option<Decision>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub answers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub first_option: bool,
    /// A note to the agent with a decline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An approval policy: every condition that's set must hold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RespondRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ApprovalAction>,
    /// A glob on the command (`cargo *`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A glob every changed file must match (`docs/**`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<String>,
    /// A glob on the tool's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub decision: Decision,
}

/// `agent.publish`: commit the thread's work on its branch and push it (a change in a roll code
/// repo); `land` sends it to landing (the merge queue on a `queue` repo). Output: `branch`,
/// `commit`, `change`, `url`, `landing`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentPublish {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The commit message (a template; default: the thread's title).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub land: bool,
}

/// `agent.script`: run one of the project's scripts (by id) in a terminal: in the thread's
/// worktree when `thread` is set, else in the project's checkout on the target machine. Output:
/// `terminal`, `machine`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentScript {
    pub script: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettleAction {
    #[default]
    Settle,
    Archive,
    Unsettle,
}

/// `agent.settle`: settle, archive or un-settle the thread.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentSettle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default)]
    pub action: SettleAction,
}

/// `webhook`: an HTTP request to a public address (no redirects, private addresses refused,
/// the answer read up to 1 MiB). With `secret` (one of the automation's secrets, by name) the
/// body is signed like roll's own webhooks (`X-Roll-Signature-256` over `{timestamp}.{body}`).
/// `url`, header values and `body` are templates; header values may also use
/// `{{secrets.<name>}}`. Output: `status`, `headers`, `body` (JSON when it parses, else text).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WebhookCall {
    pub url: String,
    #[serde(default = "post")]
    pub method: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A template (sent as is), or JSON whose strings are templates. Unset: the event as JSON.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Statuses that count as success besides 2xx.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ok_statuses: Vec<u16>,
}

fn post() -> String {
    "POST".into()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// `notify`: a notice on the owner's event stream (the dashboard toasts it). `title`, `body`
/// and `link` are templates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotifyConfig {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default)]
    pub level: NoticeLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// `delay`: wait this long (durably: a restart doesn't start it over).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelayConfig {
    pub seconds: u64,
}

// ---------------------------------------------------------------------------------------------
// Events and runs
// ---------------------------------------------------------------------------------------------

/// One event in the owner's inbox: recorded once per `key` before anything runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationEvent {
    pub id: AutomationEventId,
    /// `manual`, `schedule`, `interval`, `webhook`, `code`, `agent`, or an integration.
    pub source: String,
    /// `run`, `tick`, `landed`, `thread.done`, `issue.created`, …
    pub kind: String,
    /// The idempotency key (`<source>:<the event's own id>`): the same key is the same event.
    pub key: String,
    pub at: Millis,
    /// What happened, as the source describes it (`event.data.…` in templates).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    /// Set for events that are for one automation (manual, schedule, interval, webhook).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationId>,
}

/// One line about an event, on its runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBrief {
    pub id: AutomationEventId,
    pub source: String,
    pub kind: String,
    /// For people ("acme/app#12 landed", "Run by ada").
    pub summary: String,
    pub at: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Waiting for its turn (`queue` concurrency, `parallel`'s cap).
    Queued,
    Running,
    /// Parked on a step: an agent working, a retry's backoff, a delay.
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    /// Not run: concurrency `skip`, the rate limit, or the automation was disabled.
    Skipped,
}

impl RunState {
    pub fn is_final(self) -> bool {
        matches!(self, RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::Skipped)
    }
}

/// One run of an automation, for one event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationRun {
    pub id: AutomationRunId,
    pub automation: AutomationId,
    /// The automation's name when the run was made.
    pub name: String,
    pub status: RunState,
    pub event: EventBrief,
    pub steps: Vec<StepRun>,
    /// Threads its steps started, for the canvas (tagged with the automation).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<RunThread>,
    /// The owner cursor of its last change (the `automation_run` stream event).
    pub cursor: u64,
    pub created_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Millis>,
    /// Why it was skipped, cancelled or failed: one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When it was last retried (a failed or cancelled run going on from the step that stopped
    /// it): what it takes counts from here, and the attempt that stopped stays in its steps' logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retried_at: Option<Millis>,
}

/// A thread a run started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunThread {
    pub machine: MachineId,
    pub thread: ThreadId,
    /// The step that started it.
    pub step: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    /// Parked until `next_at` (an agent working, a delay) or an event wakes it.
    Waiting,
    /// Failed for now; tries again at `next_at`.
    Retrying,
    Succeeded,
    Failed,
    /// Its `when` didn't hold, or the run ended first.
    Skipped,
    Cancelled,
}

impl StepState {
    pub fn is_final(self) -> bool {
        matches!(self, StepState::Succeeded | StepState::Failed | StepState::Skipped | StepState::Cancelled)
    }
}

/// One step of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    /// The step's id in the automation.
    pub id: String,
    pub kind: String,
    pub status: StepState,
    /// Attempts so far.
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Millis>,
    /// When it's looked at again (waiting, retrying).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Millis>,
    /// What it produced (`steps.<id>.…`), at most [`limits::OUTPUT`] bytes of JSON.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The newest [`limits::LOG_LINES`] lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<LogLine>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    #[default]
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub at: Millis,
    #[serde(default)]
    pub level: LogLevel,
    pub text: String,
}

/// `GET …/automations/runs/{run}`: the run and the event it ran for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunDetail {
    pub run: AutomationRun,
    pub event: AutomationEvent,
}

/// `POST …/automations/{id}/run`: run it now. `input` is `event.data.input`; `key` makes the
/// request idempotent (the same key is the same run).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunNow {
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input: Value,
    /// The event's data as its trigger would send it (an object): a run by hand of an automation
    /// a code, agent or integration event starts, its steps reading `event.data.title` and the
    /// like. Its fields go into `event.data` beside `by` and `input`, which roll sets.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A `notify` step's notice, on the owner's stream (no cursor) and kept for later, the newest
/// 200 per owner (`GET …/automations/notices`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default)]
    pub level: NoticeLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<AutomationRunId>,
    pub at: Millis,
}

// ---------------------------------------------------------------------------------------------
// The catalog and dry runs
// ---------------------------------------------------------------------------------------------

/// `GET …/automations/catalog`: every trigger source and step kind the relay has (built in and
/// registered by integrations), for the builder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub triggers: Vec<TriggerInfo>,
    pub steps: Vec<StepInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerInfo {
    /// The [`Trigger`]'s `type`, or an integration's `source`.
    pub source: String,
    pub title: String,
    pub description: String,
    /// The event kinds it brings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventInfo>,
    /// JSON Schema of an integration trigger's `config` (null for the built-in ones).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
    /// An integration that needs connecting first and isn't (the builder says so).
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_setup: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventInfo {
    pub kind: String,
    pub title: String,
    /// Fields of `event.data` worth filtering on, as dotted paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// A realistic `event.data`, for dry runs ("Test") and the builder's autocomplete.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub sample: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepInfo {
    pub kind: String,
    pub title: String,
    pub description: String,
    /// JSON Schema of its `config`.
    pub config: Value,
    /// The fields of its output (`steps.<id>.…`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
    /// The integration it belongs to (`linear`, …); unset: built in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_setup: bool,
}

/// `POST …/automations/test`: what an automation would do with an event, without doing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestRequest {
    pub automation: AutomationSpec,
    pub event: TestEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestEvent {
    pub source: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    /// The trigger takes this event and every filter holds.
    pub matched: bool,
    pub trigger: bool,
    /// Each filter's verdict, in order.
    pub filters: Vec<bool>,
    /// Each step's config with its templates filled in from the event (later steps' references
    /// to earlier outputs are empty), and whether its `when` holds.
    pub steps: Vec<TestStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestStep {
    pub id: String,
    pub runs: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
    /// A config the kind refuses, or a template that doesn't parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bounds the relay keeps to.
pub mod limits {
    /// Automations per owner.
    pub const AUTOMATIONS: usize = 200;
    pub const STEPS: usize = 32;
    pub const FILTERS: usize = 32;
    pub const NAME: usize = 100;
    pub const DESCRIPTION: usize = 2000;
    /// Characters of one template, and of what one renders to.
    pub const TEMPLATE: usize = 64 << 10;
    /// Bytes of JSON one step's output keeps.
    pub const OUTPUT: usize = 64 << 10;
    /// Bytes of JSON an event's data keeps.
    pub const EVENT: usize = 256 << 10;
    /// Log lines a step keeps (the newest), and characters of one.
    pub const LOG_LINES: usize = 50;
    pub const LOG_LINE: usize = 2000;
    pub const ATTEMPTS: u32 = 10;
    /// Runs of one automation going at once under `parallel`.
    pub const PARALLEL: usize = 20;
    /// The rate limit when an automation sets none: 100 runs an hour.
    pub const DEFAULT_RATE: super::RateLimit = super::RateLimit { max: 100, per_s: 3600 };
    pub const MIN_INTERVAL_S: u64 = 60;
    /// Secrets per automation, and bytes of one.
    pub const SECRETS: usize = 20;
    pub const SECRET: usize = 8 << 10;
}
