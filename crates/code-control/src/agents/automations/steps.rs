//! The built-in steps (docs/code/automations.md, "Steps"). Each acts through roll's API as its
//! automation ([`StepContext::api`]): commands to daemons go through the commands route (which
//! wakes sandboxes, grants git access, applies the agent policy and routes in a cluster), with
//! ids made from the run and step, so an attempt that runs again is answered from the daemon's
//! receipt instead of running twice.

use super::context::{HttpError, StepContext};
use super::registry::{OwnerRef, StepKind, StepOutcome};
use crate::Code;
use crate::review::pattern::Pattern;
use agent_proto::automation::{
    AgentDefaults, AgentMessage, AgentPublish, AgentRespond, AgentScript, AgentSettle, AgentStart, AgentWait, DelayConfig, LogLevel, Notice, NotifyConfig, OnError, ProjectRef,
    RespondRule, Retry, RunState, SettleAction, StepInfo, Target, WaitFor, WebhookCall, step_kinds,
};
use agent_proto::{Command, Entity, Machine, MachineId, Project, ProjectId, RepoRef, Reply, RequestBody, RequestStatus, Response, RunStatus, Settle, Thread, ThreadId, ThreadStatus};
use axum::http::Method;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Register the built-in triggers' catalog entries and steps (idempotent).
pub(crate) fn register_built_ins(code: &Arc<Code>) {
    let r = &code.automations.registry;
    if r.step(step_kinds::AGENT_START).is_some() {
        return;
    }
    r.register_step(Arc::new(StartAgent));
    r.register_step(Arc::new(WaitAgent));
    r.register_step(Arc::new(MessageAgent));
    r.register_step(Arc::new(RespondAgent));
    r.register_step(Arc::new(PublishAgent));
    r.register_step(Arc::new(RunScript));
    r.register_step(Arc::new(SettleAgent));
    r.register_step(Arc::new(CallWebhook));
    r.register_step(Arc::new(Notify));
    r.register_step(Arc::new(Delay));
    super::triggers::register_built_ins(r);
}

// ── helpers ──

fn s(d: &str) -> Value {
    json!({ "type": "string", "description": d })
}

/// A string with a widget for it (docs/code/automations.md, "The catalog"): `template`,
/// `template-long`, `roll-thread`, `roll-model`, `roll-secret`.
fn f(d: &str, format: &str) -> Value {
    json!({ "type": "string", "description": d, "format": format })
}

fn target_schema() -> Value {
    json!({ "format": "roll-target", "oneOf": [
        { "type": "object", "properties": { "type": { "const": "machine" }, "machine": s("A machine's id (mch_…) or name") }, "required": ["type", "machine"] },
        { "type": "object", "properties": { "type": { "const": "sandbox" }, "size": s("A sandbox size (default standard)"), "keep": { "type": "boolean", "description": "Keep the sandbox when the run ends" } }, "required": ["type"] },
        { "type": "object", "properties": { "type": { "const": "any_online" } }, "required": ["type"] },
        { "type": "object", "properties": { "type": { "const": "project_machine" } }, "required": ["type"] },
    ] })
}

fn project_schema() -> Value {
    json!({ "format": "roll-project", "oneOf": [
        { "type": "object", "properties": { "type": { "const": "id" }, "id": s("A project id (prj_…)") }, "required": ["type", "id"] },
        { "type": "object", "properties": { "type": { "const": "repo" }, "repo": s("A roll code repo (repo or owner/repo)"), "path": s("Where to clone it when the machine has none") }, "required": ["type", "repo"] },
    ] })
}

fn thread_schema() -> Value {
    f("A thread id, e.g. {{steps.start.thread}} (default: the run's latest thread)", "roll-thread")
}

fn info(kind: &str, title: &str, description: &str, config: Value, output: &[&str]) -> StepInfo {
    StepInfo { kind: kind.into(), title: title.into(), description: description.into(), config, output: output.iter().map(|o| o.to_string()).collect(), integration: None, needs_setup: false }
}

fn parse<T: serde::de::DeserializeOwned>(config: &Value) -> Result<T, String> {
    let v = if config.is_null() { json!({}) } else { config.clone() };
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// `Err(outcome)` short-circuits a step.
type Step<T> = Result<T, StepOutcome>;

fn fail<T>(m: impl Into<String>) -> Step<T> {
    Err(StepOutcome::Fail(m.into()))
}

fn rendered(ctx: &StepContext, t: &str) -> Step<String> {
    ctx.render(t).map_err(StepOutcome::Fail)
}

fn agents_path(ctx: &StepContext) -> String {
    format!("/v1/code/agents/{}", percent_encoding::utf8_percent_encode(&ctx.owner().name, percent_encoding::NON_ALPHANUMERIC))
}

/// The automation's `agent.start` defaults (from the run's context).
fn defaults(ctx: &StepContext) -> AgentDefaults {
    serde_json::from_value(ctx.context()["automation"]["defaults"].clone()).unwrap_or_default()
}

/// The owner's machines (with whether they're connected).
async fn machines(ctx: &StepContext) -> Step<Vec<Machine>> {
    let a = ctx.api(Method::GET, &format!("{}/machines", agents_path(ctx)), None).await;
    if !a.ok() {
        return Err(if a.transient() { StepOutcome::Retry(a.error()) } else { StepOutcome::Fail(a.error()) });
    }
    serde_json::from_value(a.body).map_err(|e| StepOutcome::Retry(format!("reading the machines: {e}")))
}

/// The owner's projects as the relay mirrors them: (machine, project).
fn projects(code: &Code, owner_id: &str) -> Vec<(String, Project)> {
    let rows: Vec<(String, String)> =
        code.db.sql().query("SELECT machine_id, json FROM agent_entities WHERE owner_id = ? AND kind = 'project'", [owner_id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or_default();
    rows.into_iter()
        .filter_map(|(m, j)| match serde_json::from_str::<Entity>(&j) {
            Ok(Entity::Project(p)) => Some((m, p)),
            _ => None,
        })
        .collect()
}

/// `repo` or `owner/repo`, as the owner's.
fn repo_ref(owner: &OwnerRef, repo: &str) -> RepoRef {
    match repo.split_once('/') {
        Some((o, r)) => RepoRef { owner: o.to_string(), repo: r.to_string() },
        None => RepoRef { owner: owner.name.clone(), repo: repo.to_string() },
    }
}

fn is_repo(p: &Project, r: &RepoRef) -> bool {
    p.repo.as_ref().is_some_and(|x| x.owner.eq_ignore_ascii_case(&r.owner) && x.repo.eq_ignore_ascii_case(&r.repo))
}

/// The thread a step works on: its config's (a template), else the run's latest. (machine, thread)
fn thread_of(ctx: &StepContext, configured: Option<&str>) -> Step<(String, Thread)> {
    let id = match configured.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => rendered(ctx, t)?,
        None => match ctx.context()["run"]["threads"].as_array().and_then(|t| t.last()).and_then(|t| t["thread"].as_str()) {
            Some(t) => t.to_string(),
            None => return fail("no thread: set `thread`, or start one in an earlier step"),
        },
    };
    match crate::agents::store::thread_of(ctx.code.db.sql(), &ctx.owner().id, id.trim()) {
        Ok(Some(t)) => Ok(t),
        Ok(None) => fail(format!("no thread {id}")),
        Err(e) => Err(StepOutcome::Retry(format!("reading the thread: {e}"))),
    }
}

/// A thread's runs, as mirrored, oldest first.
fn runs_of(code: &Code, machine: &str, thread: &str) -> Vec<agent_proto::Run> {
    let rows: Vec<String> = code.db.sql().query("SELECT json FROM agent_entities WHERE machine_id = ? AND kind = 'run' AND thread = ?", [machine, thread], |r| r.get(0)).unwrap_or_default();
    let mut runs: Vec<agent_proto::Run> = rows
        .iter()
        .filter_map(|j| match serde_json::from_str::<Entity>(j) {
            Ok(Entity::Run(r)) => Some(r),
            _ => None,
        })
        .collect();
    runs.sort_by_key(|r| r.ordinal);
    runs
}

/// The agent's last message in a run (bounded).
fn last_message(code: &Code, machine: &str, thread: &str, run: &str) -> Option<String> {
    let rows: Vec<String> = code
        .db
        .sql()
        .query(
            "SELECT json FROM agent_entities WHERE machine_id = ? AND kind = 'node' AND thread = ? AND json LIKE ?",
            [machine, thread, &format!("%\"run\":\"{run}\"%")],
            |r| r.get(0),
        )
        .unwrap_or_default();
    rows.iter()
        .filter_map(|j| match serde_json::from_str::<Entity>(j) {
            Ok(Entity::Node(n)) if n.run.as_str() == run && n.parent.is_none() => match n.body {
                agent_proto::NodeBody::Message { text } => Some((n.ordinal, text)),
                _ => None,
            },
            _ => None,
        })
        .max_by_key(|(o, _)| *o)
        .map(|(_, t)| t.chars().take(8000).collect())
}

// ── agent.start ──

struct StartAgent;

impl StartAgent {
    /// Pick the machine (and make a sandbox): `Ok(None)` when the step must wait for one.
    async fn machine(&self, ctx: &mut StepContext, target: &Target, project: Option<&ProjectRef>) -> Step<Option<String>> {
        if let Some(m) = ctx.state()["machine"].as_str() {
            return Ok(Some(m.to_string()));
        }
        let all = machines(ctx).await?;
        let mirrored = projects(&ctx.code, &ctx.owner().id);
        let chosen = match target {
            Target::Machine { machine } => {
                let want = rendered(ctx, machine)?;
                match all.iter().find(|m| m.id.as_str() == want || m.name == want) {
                    Some(m) => m.id.0.clone(),
                    None => return fail(format!("no machine {want}")),
                }
            }
            Target::ProjectMachine => match project {
                Some(ProjectRef::Id { id }) => {
                    let id = rendered(ctx, id)?;
                    match mirrored.iter().find(|(_, p)| p.id.as_str() == id) {
                        Some((m, _)) => m.clone(),
                        None => return fail(format!("no project {id}")),
                    }
                }
                _ => return fail("project_machine needs a project by id"),
            },
            Target::AnyOnline => {
                let online: Vec<&Machine> = all.iter().filter(|m| m.online).collect();
                let has = |m: &Machine| match project {
                    Some(ProjectRef::Id { id }) => mirrored.iter().any(|(mid, p)| mid == m.id.as_str() && p.id.as_str() == id),
                    Some(ProjectRef::Repo { repo, .. }) => {
                        let r = repo_ref(ctx.owner(), repo);
                        mirrored.iter().any(|(mid, p)| mid == m.id.as_str() && is_repo(p, &r))
                    }
                    None => false,
                };
                match online.iter().find(|m| has(m)) {
                    Some(m) => m.id.0.clone(),
                    // A repo project goes onto any connected machine (cloned there).
                    None => match (project, online.iter().find(|m| m.sandbox.is_none()).or(online.first())) {
                        (Some(ProjectRef::Repo { .. }), Some(m)) => m.id.0.clone(),
                        _ => {
                            ctx.log(LogLevel::Info, "no connected machine has the project yet; waiting for one");
                            return Ok(None);
                        }
                    },
                }
            }
            Target::Sandbox { size, keep } => {
                let name = format!("auto-{}-{}", ctx.run().as_str().trim_start_matches("arn_").chars().take(10).collect::<String>(), ctx.step().id);
                let base = format!("{}/sandboxes", agents_path(ctx));
                let list = ctx.api(Method::GET, &base, None).await;
                let existing = list.body["sandboxes"].as_array().and_then(|l| l.iter().find(|s| s["name"] == name.as_str() && s["status"] != "deleting").cloned());
                let made = match existing {
                    Some(s) => s,
                    None => {
                        let a = ctx.api(Method::POST, &base, Some(json!({ "name": name, "size": size }))).await;
                        if !a.ok() {
                            return Err(if a.transient() { StepOutcome::Retry(a.error()) } else { StepOutcome::Fail(format!("making a sandbox: {}", a.error())) });
                        }
                        ctx.log(LogLevel::Info, format!("made a {size} sandbox, {name}"));
                        a.body["sandbox"].clone()
                    }
                };
                let (Some(sbx), Some(m)) = (made["id"].as_str(), made["machine"].as_str()) else { return fail("the sandbox route answered without an id") };
                ctx.set_state(json!({ "machine": m, "sandbox": sbx, "keep": keep }));
                return Ok(Some(m.to_string()));
            }
        };
        let mut st = ctx.state().clone();
        if !st.is_object() {
            st = json!({});
        }
        st["machine"] = json!(chosen);
        ctx.set_state(st);
        Ok(Some(chosen))
    }
}

#[async_trait::async_trait]
impl StepKind for StartAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_START,
            "Start an agent",
            "Start a thread on a machine, a new sandbox, any connected machine with the project, or the project's machine. Unset fields come from the automation's defaults, then the project's.",
            json!({ "type": "object", "required": ["prompt"], "properties": {
                "target": target_schema(), "project": project_schema(),
                "provider": s("codex, claude or acp:<name>"), "model": f("A model id", "roll-model"), "effort": s("low … max"),
                "runtime_mode": { "enum": ["full_access", "auto_edit", "ask"] }, "interaction_mode": { "enum": ["default", "plan"] },
                "worktree": { "type": "boolean", "description": "Its own git worktree" }, "title": f("The thread's title", "template"), "prompt": f("What the agent is asked", "template-long"),
                "orchestrator": { "type": "boolean", "description": "Plan and delegate to subagents" },
            } }),
            &["machine", "project", "thread", "sandbox"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: AgentStart = parse(config)?;
        if c.prompt.trim().is_empty() {
            return Err("prompt can't be empty".into());
        }
        if let (Some(Target::ProjectMachine), Some(ProjectRef::Repo { .. })) = (&c.target, &c.project) {
            return Err("project_machine needs a project by id".into());
        }
        Ok(())
    }

    fn default_retry(&self) -> Retry {
        Retry { attempts: 5, backoff_s: 15 }
    }

    fn default_timeout(&self) -> Duration {
        Duration::from_secs(30 * 60)
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        match self.start(ctx).await {
            Ok(o) | Err(o) => o,
        }
    }

    async fn cleanup(&self, ctx: &mut StepContext, _status: RunState) {
        let (Some(sbx), keep) = (ctx.state()["sandbox"].as_str().map(str::to_string), ctx.state()["keep"].as_bool().unwrap_or(false)) else { return };
        if keep {
            return;
        }
        let a = ctx.api(Method::DELETE, &format!("{}/sandboxes/{sbx}", agents_path(ctx)), None).await;
        if a.ok() || a.status == axum::http::StatusCode::NOT_FOUND {
            ctx.log(LogLevel::Info, format!("deleted the sandbox {sbx}"));
        } else {
            ctx.log(LogLevel::Warn, format!("deleting the sandbox {sbx}: {}", a.error()));
        }
    }
}

impl StartAgent {
    async fn start(&self, ctx: &mut StepContext) -> Step<StepOutcome> {
        let c: AgentStart = ctx.config().map_err(StepOutcome::Fail)?;
        let d = defaults(ctx);
        let project = c.project.clone().or(d.project.clone());
        let target = c.target.clone().or(d.target.clone()).unwrap_or(match project {
            Some(ProjectRef::Id { .. }) => Target::ProjectMachine,
            _ => Target::AnyOnline,
        });
        if let (Some(m), Some(t)) = (ctx.state()["machine"].as_str().map(str::to_string), ctx.state()["thread"].as_str().map(str::to_string)) {
            // Started on an earlier attempt that couldn't save its outcome.
            ctx.started_thread(&MachineId(m), &ThreadId(t.clone()));
            return Ok(StepOutcome::Done(self.output(ctx, &t)));
        }
        let Some(machine) = self.machine(ctx, &target, project.as_ref()).await? else {
            return Ok(StepOutcome::Wait { after: Duration::from_secs(30) });
        };
        // A new sandbox: wait for its daemon (commands to it wait too, but not this long).
        if matches!(target, Target::Sandbox { .. }) {
            let online = machines(ctx).await?.iter().any(|m| m.id.as_str() == machine && m.online);
            if !online {
                ctx.log(LogLevel::Info, "waiting for the sandbox to connect");
                return Ok(StepOutcome::Wait { after: Duration::from_secs(5) });
            }
        }
        let mirrored = projects(&ctx.code, &ctx.owner().id);
        let p: Project = match &project {
            Some(ProjectRef::Id { id }) => {
                let id = rendered(ctx, id)?;
                match mirrored.iter().find(|(m, p)| m == &machine && p.id.as_str() == id) {
                    Some((_, p)) => p.clone(),
                    None => return fail(format!("the project {id} isn't on that machine")),
                }
            }
            Some(ProjectRef::Repo { repo, path }) => {
                let r = repo_ref(ctx.owner(), &rendered(ctx, repo)?);
                match mirrored.iter().find(|(m, p)| m == &machine && is_repo(p, &r)) {
                    Some((_, p)) => p.clone(),
                    None => {
                        ctx.log(LogLevel::Info, format!("cloning {}/{} onto the machine", r.owner, r.repo));
                        let path = path.as_deref().map(|p| rendered(ctx, p)).transpose()?.unwrap_or_default();
                        let reply = ctx.command(&machine, "clone", Command::ProjectAdd { path, name: None, clone: Some(r.clone()) }).await?;
                        let Reply::Created { id } = reply else { return fail("the machine didn't answer with a project") };
                        // The record follows the reply; wait for it to be mirrored.
                        match projects(&ctx.code, &ctx.owner().id).into_iter().find(|(_, p)| p.id.as_str() == id) {
                            Some((_, p)) => p,
                            None => return Ok(StepOutcome::Wait { after: Duration::from_secs(2) }),
                        }
                    }
                }
            }
            None => return fail("no project: set `project` on the step or in the automation's defaults"),
        };
        let pd = &p.settings.defaults;
        let provider = match c.provider.clone().or(d.provider.clone()).or(pd.provider.clone()) {
            Some(p) => p,
            None => {
                let all = machines(ctx).await?;
                match all.iter().find(|m| m.id.as_str() == machine).and_then(|m| m.info.as_ref()).and_then(|i| i.providers.first()) {
                    Some(p) => p.id.clone(),
                    None => return fail("no provider: set one, or connect a machine with codex or claude"),
                }
            }
        };
        let title = c.title.as_deref().map(|t| rendered(ctx, t)).transpose()?;
        let prompt = rendered(ctx, &c.prompt)?;
        let cmd = Command::ThreadCreate {
            project: ProjectId(p.id.0.clone()),
            provider,
            model: c.model.clone().or(d.model.clone()).or(pd.model.clone()),
            effort: c.effort.clone().or(d.effort.clone()).or(pd.effort.clone()),
            runtime_mode: c.runtime_mode.or(d.runtime_mode).or(pd.runtime_mode).unwrap_or_default(),
            interaction_mode: c.interaction_mode.or(d.interaction_mode).or(pd.interaction_mode).unwrap_or_default(),
            worktree: c.worktree.or(d.worktree).or(pd.worktree).unwrap_or(false),
            title,
            prompt: Some(prompt),
            attachments: vec![],
            orchestrator: c.orchestrator,
        };
        let Reply::Created { id: thread } = ctx.command(&machine, "thread", cmd).await? else { return fail("the machine didn't answer with a thread") };
        ctx.started_thread(&MachineId(machine.clone()), &ThreadId(thread.clone()));
        let mut st = ctx.state().clone();
        st["machine"] = json!(machine);
        st["project"] = json!(p.id);
        st["thread"] = json!(thread);
        ctx.set_state(st);
        ctx.log(LogLevel::Info, format!("started {thread}"));
        Ok(StepOutcome::Done(self.output(ctx, &thread)))
    }

    fn output(&self, ctx: &StepContext, thread: &str) -> Value {
        let st = ctx.state();
        json!({ "machine": st["machine"], "project": st["project"], "thread": thread, "sandbox": st["sandbox"] })
    }
}

// ── agent.wait ──

struct WaitAgent;

/// Where a thread is, for `agent.wait`.
fn wait_outcome(code: &Code, machine: &str, t: &Thread, until: &[WaitFor]) -> Option<(WaitFor, Option<agent_proto::Run>)> {
    let runs = runs_of(code, machine, t.id.as_str());
    let last = runs.last().cloned();
    let busy = t.status.is_busy() || t.active_run.is_some() || !t.queued.is_empty();
    for w in until {
        let hit = match w {
            WaitFor::Settled => matches!(t.inbox.settle, Some(Settle::Settled { .. })),
            WaitFor::Waiting => t.status == ThreadStatus::Waiting,
            WaitFor::Failed => t.status == ThreadStatus::Error || (!busy && last.as_ref().is_some_and(|r| matches!(r.status, RunStatus::Failed | RunStatus::Interrupted))),
            WaitFor::Done => !busy && last.as_ref().is_some_and(|r| r.status == RunStatus::Completed),
        };
        if hit {
            return Some((*w, last));
        }
    }
    None
}

#[async_trait::async_trait]
impl StepKind for WaitAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_WAIT,
            "Wait for the agent",
            "Wait until the thread is done, failed, waiting on you or settled (default done or failed), at most timeout_s (default an hour).",
            json!({ "type": "object", "properties": {
                "thread": thread_schema(),
                "until": { "type": "array", "items": { "enum": ["done", "failed", "waiting", "settled"] } },
                "timeout_s": { "type": "integer", "minimum": 1 },
                "on_timeout": { "enum": ["fail", "continue"] },
            } }),
            &["outcome", "thread", "run", "status", "error", "message"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: AgentWait = parse(config)?;
        if c.timeout_s.is_some_and(|t| t == 0 || t > 7 * 86_400) {
            return Err("timeout_s: 1 s to 7 days".into());
        }
        Ok(())
    }

    fn default_timeout(&self) -> Duration {
        Duration::from_secs(8 * 86_400)
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: AgentWait = match ctx.config() {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(e),
        };
        let (machine, t) = match thread_of(ctx, c.thread.as_deref()) {
            Ok(x) => x,
            Err(o) => return o,
        };
        let until = if c.until.is_empty() { vec![WaitFor::Done, WaitFor::Failed] } else { c.until.clone() };
        let out = |outcome: &str, run: Option<&agent_proto::Run>| {
            let message = run.and_then(|r| last_message(&ctx.code, &machine, t.id.as_str(), r.id.as_str()));
            json!({
                "outcome": outcome, "thread": t.id, "machine": machine, "run": run.map(|r| r.id.clone()), "status": t.status,
                "error": run.and_then(|r| r.error.clone()).or(t.error.clone()), "message": message, "title": t.title,
                "change": t.change,
            })
        };
        if let Some((w, run)) = wait_outcome(&ctx.code, &machine, &t, &until) {
            let name = serde_json::to_value(w).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            return StepOutcome::Done(out(&name, run.as_ref()));
        }
        let timeout = c.timeout_s.or(ctx.step().timeout_s).unwrap_or(3600) as i64 * 1000;
        if code_sql::now_ms() - ctx.started_at() >= timeout {
            return match c.on_timeout {
                OnError::Continue => StepOutcome::Done(out("timeout", runs_of(&ctx.code, &machine, t.id.as_str()).last())),
                OnError::Fail => StepOutcome::Fail(format!("{} didn't get there within {} s", t.id, timeout / 1000)),
            };
        }
        // Its records wake the run sooner (automation_threads); this is the safety net.
        let left = (ctx.started_at() + timeout - code_sql::now_ms()).max(1000) as u64;
        StepOutcome::Wait { after: Duration::from_millis(left.min(30_000)) }
    }
}

// ── agent.message ──

struct MessageAgent;

#[async_trait::async_trait]
impl StepKind for MessageAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_MESSAGE,
            "Message the agent",
            "Send a message to the thread: a new run when it's idle; queued after the active run, or steering it.",
            json!({ "type": "object", "required": ["text"], "properties": { "thread": thread_schema(), "text": f("The message", "template-long"), "mode": { "enum": ["queue", "steer"] } } }),
            &["thread", "run"],
        )
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: AgentMessage = ctx.config().map_err(StepOutcome::Fail)?;
            let (machine, t) = thread_of(ctx, c.thread.as_deref())?;
            let text = rendered(ctx, &c.text)?;
            let reply = ctx.command(&machine, "message", Command::MessageSend { thread: t.id.clone(), text, attachments: vec![], mode: c.mode }).await?;
            let run = match reply {
                Reply::Created { id } => Some(id),
                _ => None,
            };
            Ok(StepOutcome::Done(json!({ "thread": t.id, "run": run })))
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── agent.respond ──

struct RespondAgent;

fn glob(p: &str, s: &str) -> bool {
    Pattern::new(p).is_some_and(|p| p.matches(s))
}

/// The first rule that decides an approval.
fn decide(rules: &[RespondRule], a: &agent_proto::Approval) -> Option<agent_proto::Decision> {
    rules
        .iter()
        .find(|r| {
            r.action.is_none_or(|x| x == a.action)
                && r.command.as_deref().is_none_or(|g| a.command.as_deref().is_some_and(|c| glob(g, c)))
                && r.tool.as_deref().is_none_or(|g| a.tool.as_deref().is_some_and(|t| glob(g, t)))
                && r.files.as_deref().is_none_or(|g| !a.files.is_empty() && a.files.iter().all(|f| glob(g, &f.path)))
        })
        .map(|r| r.decision)
}

#[async_trait::async_trait]
impl StepKind for RespondAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_RESPOND,
            "Answer the agent",
            "Answer the thread's pending approvals by rules (the first that matches decides, else `otherwise`) and its questions (by id or header, or their first option). What no rule decides is left for you.",
            json!({ "type": "object", "properties": {
                "thread": thread_schema(),
                "rules": { "type": "array", "items": { "type": "object", "required": ["decision"], "properties": {
                    "action": { "enum": ["command", "file_change", "permission", "tool", "plan"] }, "command": s("A glob on the command"),
                    "files": s("A glob every changed file matches"), "tool": s("A glob on the tool"),
                    "decision": { "enum": ["accept", "accept_for_session", "decline", "cancel"] } } } },
                "otherwise": { "enum": ["accept", "accept_for_session", "decline", "cancel"] },
                "answers": { "type": "object", "additionalProperties": { "type": "string" } },
                "first_option": { "type": "boolean" }, "message": f("A note to the agent with a decline", "template"),
            } }),
            &["answered", "left"],
        )
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: AgentRespond = ctx.config().map_err(StepOutcome::Fail)?;
            let (machine, t) = thread_of(ctx, c.thread.as_deref())?;
            let rows: Vec<String> = ctx
                .code
                .db
                .sql()
                .query("SELECT json FROM agent_entities WHERE machine_id = ? AND kind = 'request' AND thread = ?", [machine.as_str(), t.id.as_str()], |r| r.get(0))
                .map_err(|e| StepOutcome::Retry(e.to_string()))?;
            let pending = rows.iter().filter_map(|j| match serde_json::from_str::<Entity>(j) {
                Ok(Entity::Request(r)) if r.status == RequestStatus::Pending => Some(r),
                _ => None,
            });
            let (mut answered, mut left) = (Vec::new(), Vec::new());
            for r in pending.collect::<Vec<_>>() {
                let response = match &r.body {
                    RequestBody::Approval(a) => decide(&c.rules, a).or(c.otherwise).filter(|d| a.decisions.contains(d)).map(|decision| Response::Decision {
                        decision,
                        message: matches!(decision, agent_proto::Decision::Decline | agent_proto::Decision::Cancel).then(|| c.message.clone()).flatten(),
                    }),
                    RequestBody::Questions { questions } => {
                        let mut answers = BTreeMap::new();
                        for q in questions {
                            let given = c.answers.get(&q.id).or_else(|| c.answers.get(&q.header));
                            let a = match given {
                                Some(t) => Some(rendered(ctx, t)?),
                                None if c.first_option => q.options.first().map(|o| o.label.clone()),
                                None => None,
                            };
                            if let Some(a) = a {
                                answers.insert(q.id.clone(), a);
                            }
                        }
                        (answers.len() == questions.len()).then_some(Response::Answers { answers })
                    }
                };
                match response {
                    Some(response) => {
                        ctx.command(&machine, &format!("respond_{}", r.id.as_str().trim_start_matches("req_")), Command::RequestRespond { request: r.id.clone(), response }).await?;
                        answered.push(r.id.0.clone());
                    }
                    None => left.push(r.id.0.clone()),
                }
            }
            ctx.log(LogLevel::Info, format!("answered {}, left {}", answered.len(), left.len()));
            Ok(StepOutcome::Done(json!({ "answered": answered, "left": left })))
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── agent.publish ──

struct PublishAgent;

#[async_trait::async_trait]
impl StepKind for PublishAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_PUBLISH,
            "Publish the agent's work",
            "Commit the thread's worktree on its branch and push it: a roll code change (land sends it to landing: the merge queue on queue repos, else the landing rules).",
            json!({ "type": "object", "properties": { "thread": thread_schema(), "message": f("The commit message (default: the thread's title)", "template"), "land": { "type": "boolean" } } }),
            &["branch", "commit", "change", "url", "landing"],
        )
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: AgentPublish = ctx.config().map_err(StepOutcome::Fail)?;
            let (machine, t) = thread_of(ctx, c.thread.as_deref())?;
            let message = match c.message.as_deref() {
                Some(m) => rendered(ctx, m)?,
                None => t.title.clone(),
            };
            match ctx.command(&machine, "publish", Command::ThreadPublish { thread: t.id.clone(), message, land: c.land }).await? {
                Reply::Published(p) => Ok(StepOutcome::Done(serde_json::to_value(p).unwrap_or_default())),
                _ => fail("the machine didn't answer with what it published"),
            }
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── agent.script ──

struct RunScript;

#[async_trait::async_trait]
impl StepKind for RunScript {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_SCRIPT,
            "Run a project script",
            "Run one of the project's scripts in a terminal: in the thread's worktree when thread is set, else in the project's checkout.",
            json!({ "type": "object", "required": ["script"], "properties": { "script": s("The script's id"), "thread": thread_schema(), "project": project_schema(), "target": target_schema() } }),
            &["terminal", "machine"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: AgentScript = parse(config)?;
        if matches!(c.target, Some(Target::Sandbox { .. })) {
            return Err("a script runs where the project is: a machine, any connected one, or the project's".into());
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: AgentScript = ctx.config().map_err(StepOutcome::Fail)?;
            let script = rendered(ctx, &c.script)?;
            let (machine, project, thread) = if c.thread.is_some() || c.project.is_none() {
                let (m, t) = thread_of(ctx, c.thread.as_deref())?;
                (m, None, Some(t.id))
            } else {
                let d = defaults(ctx);
                let project = c.project.clone().or(d.project);
                let target = c.target.clone().unwrap_or(Target::AnyOnline);
                let Some(m) = StartAgent.machine(ctx, &target, project.as_ref()).await? else { return Ok(StepOutcome::Wait { after: Duration::from_secs(30) }) };
                let mirrored = projects(&ctx.code, &ctx.owner().id);
                let p = match &project {
                    Some(ProjectRef::Id { id }) => mirrored.iter().find(|(mm, p)| mm == &m && p.id.as_str() == id).map(|(_, p)| p.id.clone()),
                    Some(ProjectRef::Repo { repo, .. }) => {
                        let r = repo_ref(ctx.owner(), repo);
                        mirrored.iter().find(|(mm, p)| mm == &m && is_repo(p, &r)).map(|(_, p)| p.id.clone())
                    }
                    None => None,
                };
                let Some(p) = p else { return fail("the project isn't on that machine") };
                (m, Some(p), None)
            };
            let cmd = Command::TerminalOpen { project, thread, command: None, script: Some(script), cols: 120, rows: 32 };
            match ctx.command(&machine, "script", cmd).await? {
                Reply::Created { id } => Ok(StepOutcome::Done(json!({ "terminal": id, "machine": machine }))),
                _ => fail("the machine didn't answer with a terminal"),
            }
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── agent.settle ──

struct SettleAgent;

#[async_trait::async_trait]
impl StepKind for SettleAgent {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::AGENT_SETTLE,
            "Settle or archive",
            "Settle, archive or un-settle the thread.",
            json!({ "type": "object", "properties": { "thread": thread_schema(), "action": { "enum": ["settle", "archive", "unsettle"] } } }),
            &["thread", "action"],
        )
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: AgentSettle = ctx.config().map_err(StepOutcome::Fail)?;
            let (machine, t) = thread_of(ctx, c.thread.as_deref())?;
            let thread = t.id.clone();
            let cmd = match c.action {
                SettleAction::Settle => Command::ThreadSettle { thread },
                SettleAction::Archive => Command::ThreadArchive { thread },
                SettleAction::Unsettle => Command::ThreadUnsettle { thread },
            };
            ctx.command(&machine, "settle", cmd).await?;
            Ok(StepOutcome::Done(json!({ "thread": t.id, "action": c.action })))
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── webhook ──

struct CallWebhook;

#[async_trait::async_trait]
impl StepKind for CallWebhook {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::WEBHOOK,
            "Call a webhook",
            "An HTTP request to a public address (no redirects, private addresses refused, the answer read up to 1 MiB). With secret, the body is signed like roll's webhooks (X-Roll-Signature-256). Header values may use {{secrets.<name>}}.",
            json!({ "type": "object", "required": ["url"], "properties": {
                "url": f("Where to send it", "template"), "method": { "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                "headers": { "type": "object", "additionalProperties": { "type": "string", "format": "template" } },
                "body": { "description": "A template, or JSON whose strings are templates (default: the event)", "format": "template-long" },
                "secret": f("One of the automation's secrets to sign with", "roll-secret"),
                "ok_statuses": { "type": "array", "items": { "type": "integer" } },
            } }),
            &["status", "headers", "body"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: WebhookCall = parse(config)?;
        if !matches!(c.method.to_ascii_uppercase().as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err("method: GET, POST, PUT, PATCH or DELETE".into());
        }
        if !c.url.contains("{{") {
            crate::review::apps::check_url(&c.url).map_err(|e| e.to_string().replace("webhook_url", "url"))?;
        }
        if c.headers.len() > 32 || c.headers.keys().any(|k| k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return Err("headers: at most 32, named [A-Za-z0-9-]".into());
        }
        Ok(())
    }

    fn secrets_at(&self, path: &str) -> bool {
        path.starts_with("config.headers.")
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: WebhookCall = ctx.config().map_err(StepOutcome::Fail)?;
            let url = rendered(ctx, &c.url)?;
            let body = match &c.body {
                Value::Null => serde_json::to_vec(&ctx.context()["event"]).unwrap_or_default(),
                Value::String(t) => rendered(ctx, t)?.into_bytes(),
                v => serde_json::to_vec(&ctx.render_json(v).map_err(StepOutcome::Fail)?).unwrap_or_default(),
            };
            let mut headers: Vec<(String, String)> = Vec::new();
            for (k, v) in &c.headers {
                headers.push((k.clone(), ctx.render_secret(v).map_err(StepOutcome::Fail)?.expose().to_string()));
            }
            if !c.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) && !c.body.is_string() {
                headers.push(("content-type".into(), "application/json".into()));
            }
            let key = ctx.idempotency_key();
            headers.push(("x-roll-delivery".into(), key.clone()));
            headers.push(("idempotency-key".into(), key));
            if let Some(name) = &c.secret {
                let Some(secret) = ctx.secret(name) else { return fail(format!("the automation has no secret {name} (or ROLL_SECRET_KEY changed)")) };
                let ts = code_sql::now_ms() / 1000;
                headers.push(("x-roll-timestamp".into(), ts.to_string()));
                headers.push(("x-roll-signature-256".into(), crate::review::apps::signature(secret.expose(), ts, &body)));
            }
            let method = c.method.to_ascii_uppercase();
            let send_body = (method != "GET").then_some(body);
            let a = match ctx.http(&method, &url, &headers, send_body).await {
                Ok(a) => a,
                Err(HttpError::Refused(m)) => return fail(m),
                Err(HttpError::Failed(m)) => return Err(StepOutcome::Retry(m)),
            };
            let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            ctx.log(LogLevel::Info, format!("{method} {host} answered {}", a.status));
            let out = json!({ "status": a.status, "headers": a.headers.iter().cloned().collect::<BTreeMap<_, _>>(), "body": a.body_value(), "truncated": a.truncated });
            if (200..300).contains(&a.status) || c.ok_statuses.contains(&a.status) {
                Ok(StepOutcome::Done(out))
            } else if a.status >= 500 || a.status == 429 || a.status == 408 {
                Err(StepOutcome::Retry(format!("{host} answered {}", a.status)))
            } else {
                fail(format!("{host} answered {}", a.status))
            }
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── notify ──

struct Notify;

#[async_trait::async_trait]
impl StepKind for Notify {
    fn info(&self) -> StepInfo {
        info(
            step_kinds::NOTIFY,
            "Notify",
            "A notice in the dashboard (a toast on every open device).",
            json!({ "type": "object", "required": ["title"], "properties": { "title": f("The notice", "template"), "body": f("More about it", "template-long"), "level": { "enum": ["info", "success", "warning", "error"] }, "link": f("Where it links", "template") } }),
            &["id"],
        )
    }

    fn default_retry(&self) -> Retry {
        Retry { attempts: 1, backoff_s: 1 }
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let go = async {
            let c: NotifyConfig = ctx.config().map_err(StepOutcome::Fail)?;
            let cut = |s: String, n: usize| s.chars().take(n).collect::<String>();
            let notice = Notice {
                id: format!("ntc_{}", ctx.idempotency_key().replace('.', "_")),
                title: cut(rendered(ctx, &c.title)?, 200),
                body: cut(rendered(ctx, &c.body)?, 2000),
                level: c.level,
                link: c.link.as_deref().map(|l| rendered(ctx, l)).transpose()?.filter(|l| l.starts_with("https://") || l.starts_with("http://") || l.starts_with('/')),
                automation: Some(ctx.automation().clone()),
                run: Some(ctx.run().clone()),
                at: code_sql::now_ms().max(0) as u64,
            };
            let id = notice.id.clone();
            let _ = ctx.code.agents.hub(&ctx.owner().id).events.send(Arc::new(agent_proto::StreamEvent::Notice { notice }));
            Ok(StepOutcome::Done(json!({ "id": id })))
        };
        match go.await {
            Ok(o) | Err(o) => o,
        }
    }
}

// ── delay ──

struct Delay;

#[async_trait::async_trait]
impl StepKind for Delay {
    fn info(&self) -> StepInfo {
        info(step_kinds::DELAY, "Wait", "Wait this long (a restart doesn't start it over).", json!({ "type": "object", "required": ["seconds"], "properties": { "seconds": { "type": "integer", "minimum": 1, "maximum": 604800 } } }), &[])
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: DelayConfig = parse(config)?;
        if c.seconds == 0 || c.seconds > 7 * 86_400 {
            return Err("seconds: 1 to 604800".into());
        }
        Ok(())
    }

    fn default_timeout(&self) -> Duration {
        Duration::from_secs(8 * 86_400)
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: DelayConfig = match ctx.config() {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(e),
        };
        let until = ctx.started_at() + c.seconds as i64 * 1000;
        let now = code_sql::now_ms();
        if now >= until {
            return StepOutcome::Done(json!({}));
        }
        StepOutcome::Wait { after: Duration::from_millis((until - now) as u64) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_proto::{ApprovalAction, Decision, FileChange};

    fn approval(action: ApprovalAction, command: Option<&str>, files: &[&str]) -> agent_proto::Approval {
        agent_proto::Approval {
            action,
            title: "x".into(),
            command: command.map(str::to_string),
            cwd: None,
            files: files.iter().map(|p| FileChange { path: p.to_string(), change: agent_proto::ChangeKind::Modify, diff: None }).collect(),
            reason: None,
            tool: None,
            input: Value::Null,
            decisions: vec![Decision::Accept, Decision::Decline],
        }
    }

    #[test]
    fn rules_decide_in_order() {
        let rules = vec![
            RespondRule { action: Some(ApprovalAction::Command), command: Some("cargo *".into()), files: None, tool: None, decision: Decision::Accept },
            RespondRule { action: Some(ApprovalAction::FileChange), command: None, files: Some("docs/**".into()), tool: None, decision: Decision::Accept },
            RespondRule { action: Some(ApprovalAction::Command), command: None, files: None, tool: None, decision: Decision::Decline },
        ];
        assert_eq!(decide(&rules, &approval(ApprovalAction::Command, Some("cargo test -p x"), &[])), Some(Decision::Accept));
        assert_eq!(decide(&rules, &approval(ApprovalAction::Command, Some("rm -rf /"), &[])), Some(Decision::Decline));
        assert_eq!(decide(&rules, &approval(ApprovalAction::FileChange, None, &["docs/a.md", "docs/b/c.md"])), Some(Decision::Accept));
        assert_eq!(decide(&rules, &approval(ApprovalAction::FileChange, None, &["docs/a.md", "src/lib.rs"])), None);
    }
}
