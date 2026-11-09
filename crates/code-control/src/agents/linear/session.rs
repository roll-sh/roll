//! `linear.agent_session`: a roll thread's work, live in Linear's agent session
//! (https://linear.app/developers/agent-interaction).
//!
//! The step follows one thread (the run's `agent.start`, or the one already working on the issue)
//! and, each time the run wakes (any record of the thread, a reply in Linear):
//!
//! 1. **Takes the session** for its run ([`store::claim`]): one run shows a session at a time. A
//!    run that finds another one still showing it hands over (`outcome: "handed"`): that one
//!    gets the reply.
//! 2. **Passes replies on** (`linear_prompts`, recorded by the webhook in order): `stop` interrupts
//!    the thread; a reply to an elicitation answers the request it asked about (`request.respond`:
//!    approve/decline, an option, or the text); anything else steers the running agent, or starts
//!    its next run. Each is marked passed on after its command went out (commands are idempotent
//!    by ids made from the run, the step and the prompt's seq).
//! 3. **Shows the thread's records** past its cursor in the relay's mirror: tool calls as actions
//!    (ephemeral while they run), reasoning and interim messages as thoughts, its todo list as the
//!    session's plan, approvals and questions as elicitations, and the final message (with the
//!    change it was published as) as the response, or an error.
//! 4. **Ends** when the thread is idle after the run it was waiting for, by giving the session up
//!    with a compare-and-swap that fails if a reply came in that wasn't passed on (then it goes
//!    on). Later replies start a new run of the automation (the event's `attached` is false).

use super::client::{LinearError, uuid_for};

use super::steps::{self, post_activity};
use super::store::{self, Claim, PendingAsk};
use crate::Code;
use crate::agents::automations::{OwnerRef, props, StepContext, StepKind, StepOutcome};
use agent_proto::automation::{LogLevel, Retry, RunState, StepInfo};
use agent_proto::{Command, Decision, Entity, Node, NodeBody, NodeStatus, Request, RequestBody, RequestStatus, Response, SendMode, Thread, ThreadStatus, TodoStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Weak;
use std::time::Duration;

/// Records read per wake (then it wakes again soon for the rest).
const PAGE: i64 = 200;
/// Node ids remembered as shown.
const SHOWN: usize = 400;
/// How long it waits between looks when nothing wakes it.
const IDLE_WAIT: Duration = Duration::from_secs(60);

pub struct AgentSession {
    pub code: Weak<Code>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The agent session (default: the event's).
    #[serde(default)]
    session: Option<String>,
    /// The thread (default: an earlier step's `thread`, else the issue's thread in the event).
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    machine: Option<String>,
    /// Tool calls as actions.
    #[serde(default = "yes")]
    actions: bool,
    /// Reasoning and interim messages as thoughts.
    #[serde(default = "yes")]
    thoughts: bool,
    /// The agent's todo list as the session's plan.
    #[serde(default = "yes")]
    plan: bool,
    /// The final message as the session's response (Linear posts it as roll's comment).
    #[serde(default = "yes")]
    respond: bool,
}

fn yes() -> bool {
    true
}

/// What the step keeps between wakes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    claimed: bool,
    #[serde(default)]
    linked: bool,
    /// The mirror's cursor of the newest record shown.
    #[serde(default)]
    cursor: i64,
    /// Nodes shown (and how: `running` or `done`).
    #[serde(default)]
    shown: Vec<(String, String)>,
    /// A finished message held back: the run's last one is the response, the others thoughts.
    #[serde(default)]
    held: Option<(String, String)>,
    /// The plan last sent (its hash).
    #[serde(default)]
    plan: Option<String>,
    /// The request an elicitation asks about.
    #[serde(default)]
    asked: Option<String>,
    /// The thread must have at least this many runs before it can be done.
    #[serde(default)]
    expect_runs: u32,
    /// The run already answered with a response or an error.
    #[serde(default)]
    responded: Option<String>,
    #[serde(default)]
    stopped: bool,
    /// A new session's question was passed on to a thread that was already going.
    #[serde(default)]
    opened: bool,
}

impl State {
    fn shown(&self, node: &str, how: &str) -> bool {
        self.shown.iter().any(|(n, h)| n == node && (h == how || h == "done"))
    }

    fn show(&mut self, node: &str, how: &str) {
        self.shown.retain(|(n, _)| n != node);
        self.shown.push((node.to_string(), how.to_string()));
        if self.shown.len() > SHOWN {
            self.shown.remove(0);
        }
    }
}

/// The thread a step follows: from the config, an earlier step's output, or the event.
fn thread_of(c: &Config, ctx: &StepContext) -> Option<(Option<String>, String, bool)> {
    if let Some(t) = c.thread.clone().filter(|t| !t.trim().is_empty()) {
        return Some((c.machine.clone().filter(|m| !m.is_empty()), t, true));
    }
    let steps = &ctx.context()["steps"];
    if let Some(o) = steps.as_object() {
        for v in o.values() {
            if let Some(t) = v["thread"].as_str().filter(|t| !t.is_empty()) {
                return Some((v["machine"].as_str().map(str::to_string), t.to_string(), true));
            }
        }
    }
    let t = &ctx.context()["event"]["data"]["thread"];
    t["thread"].as_str().map(|th| (t["machine"].as_str().map(str::to_string), th.to_string(), false))
}

/// The answer a reply in Linear gives a request.
pub fn answer(req: &Request, body: &str, value: Option<&str>) -> Response {
    let said = value.unwrap_or(body).trim();
    match &req.body {
        RequestBody::Approval(a) => {
            let first = said.lines().next().unwrap_or_default().trim().trim_end_matches(['.', '!']).to_ascii_lowercase();
            let may = |d: Decision| a.decisions.is_empty() || a.decisions.contains(&d);
            let decision = match first.as_str() {
                "always" | "approve always" | "approve for session" | "accept for session" if may(Decision::AcceptForSession) => Some(Decision::AcceptForSession),
                "approve" | "approved" | "accept" | "yes" | "y" | "ok" | "okay" | "lgtm" | "go" | "go ahead" | "sure" | "allow" | "do it" => Some(Decision::Accept),
                "decline" | "declined" | "deny" | "no" | "n" | "reject" | "don't" | "dont" => Some(Decision::Decline),
                "cancel" | "stop" | "abort" if may(Decision::Cancel) => Some(Decision::Cancel),
                _ => None,
            };
            match decision {
                Some(d) => {
                    let rest = said.lines().skip(1).collect::<Vec<_>>().join("\n");
                    Response::Decision { decision: d, message: Some(rest.trim().to_string()).filter(|m| !m.is_empty() && d != Decision::Accept) }
                }
                // Anything else is a decline that tells the agent what to do instead.
                None => Response::Decision { decision: Decision::Decline, message: Some(said.to_string()).filter(|m| !m.is_empty()) },
            }
        }
        RequestBody::Questions { questions } => {
            let mut answers = BTreeMap::new();
            let pick = |q: &agent_proto::Question, text: &str| -> String {
                let t = text.trim();
                if let Ok(n) = t.parse::<usize>()
                    && n >= 1
                    && n <= q.options.len()
                {
                    return q.options[n - 1].label.clone();
                }
                q.options.iter().find(|o| o.label.eq_ignore_ascii_case(t)).map(|o| o.label.clone()).unwrap_or_else(|| t.to_string())
            };
            if questions.len() == 1 {
                answers.insert(questions[0].id.clone(), pick(&questions[0], said));
            } else {
                // "Header: answer" lines, or numbered lines, in order; else the whole reply.
                let lines: Vec<&str> = said.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                for (i, q) in questions.iter().enumerate() {
                    let by_header = lines.iter().find_map(|l| {
                        let (h, a) = l.split_once(':')?;
                        let h = h.trim().trim_start_matches(['-', '*', ' ']);
                        (h.eq_ignore_ascii_case(&q.header) || h.eq_ignore_ascii_case(&q.id)).then(|| a.trim().to_string())
                    });
                    let by_number = lines.iter().find_map(|l| l.strip_prefix(&format!("{}.", i + 1)).or_else(|| l.strip_prefix(&format!("{})", i + 1))).map(|a| a.trim().to_string()));
                    let a = by_header.or(by_number).unwrap_or_else(|| said.to_string());
                    answers.insert(q.id.clone(), pick(q, &a));
                }
            }
            Response::Answers { answers }
        }
    }
}

/// An elicitation for a request: its body and, for a choice, Linear's `select` options.
pub fn elicitation(req: &Request) -> Value {
    match &req.body {
        RequestBody::Approval(a) => {
            let mut body = format!("**roll asks to {}**", lower_first(&a.title));
            if let Some(c) = &a.command {
                body.push_str(&format!("\n\n```\n{}\n```", c.chars().take(2000).collect::<String>()));
            }
            if !a.files.is_empty() {
                let files: Vec<String> = a.files.iter().take(20).map(|f| format!("- `{}`", f.path)).collect();
                body.push_str(&format!("\n\n{}", files.join("\n")));
            }
            if let Some(r) = &a.reason {
                body.push_str(&format!("\n\n{r}"));
            }
            body.push_str("\n\nReply **approve** or **decline** (anything else declines, and tells the agent what to do instead).");
            let mut options = vec![json!({ "label": "Approve", "value": "approve" })];
            if a.decisions.contains(&Decision::AcceptForSession) {
                options.push(json!({ "label": "Approve for this session", "value": "always" }));
            }
            options.push(json!({ "label": "Decline", "value": "decline" }));
            json!({ "type": "elicitation", "body": body, "signal": "select", "signal_metadata": { "options": options } })
        }
        RequestBody::Questions { questions } => {
            let mut body = String::new();
            for (i, q) in questions.iter().enumerate() {
                if questions.len() > 1 {
                    body.push_str(&format!("{}. **{}**: {}\n", i + 1, q.header, q.question));
                } else {
                    body.push_str(&format!("**{}**\n", q.question));
                }
                for (j, o) in q.options.iter().enumerate() {
                    let d = o.description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default();
                    body.push_str(&format!("   {}. {}{d}\n", j + 1, o.label));
                }
            }
            if questions.len() > 1 {
                body.push_str("\nReply with a line per question (`Header: answer`).");
            }
            let mut v = json!({ "type": "elicitation", "body": body.trim_end() });
            if questions.len() == 1 && !questions[0].options.is_empty() && !questions[0].multi {
                v["signal"] = json!("select");
                v["signal_metadata"] = json!({ "options": questions[0].options.iter().map(|o| json!({ "label": o.label, "value": o.label })).collect::<Vec<_>>() });
            }
            v
        }
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// An action for a tool call.
fn action(t: &agent_proto::ToolCall, done: bool) -> Value {
    use agent_proto::ToolKind::*;
    let verb = match (t.kind, done) {
        (Command, false) => "Running",
        (Command, true) => "Ran",
        (Edit, false) => "Editing",
        (Edit, true) => "Edited",
        (Read, false) => "Reading",
        (Read, true) => "Read",
        (Search, false) => "Searching",
        (Search, true) => "Searched",
        (Web, false) => "Fetching",
        (Web, true) => "Fetched",
        (Mcp | Other, false) => "Using",
        (Mcp | Other, true) => "Used",
    };
    let parameter = if t.title.is_empty() { t.name.clone() } else { t.title.clone() };
    let mut v = json!({ "type": "action", "action": verb, "parameter": parameter.chars().take(300).collect::<String>() });
    if done {
        let result = match (t.exit_code, t.files.len()) {
            (Some(0), _) => Some("exit 0".to_string()),
            (Some(c), _) => Some(format!("exit {c}")),
            (None, n) if n > 0 => Some(format!("{n} file{}", if n == 1 { "" } else { "s" })),
            _ => None,
        };
        if let Some(r) = result {
            v["result"] = json!(r);
        }
    }
    v
}

/// The relay's records of a thread changed after `cursor`, oldest first.
fn records_after(code: &Code, owner_id: &str, thread: &str, cursor: i64) -> code_sql::Result<Vec<(i64, Entity)>> {
    let rows: Vec<(i64, String)> = code.db.sql().query(
        "SELECT cursor, json FROM agent_entities WHERE owner_id = ? AND thread = ? AND cursor > ? ORDER BY cursor LIMIT ?",
        code_sql::params![owner_id, thread, cursor, PAGE],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(rows.into_iter().filter_map(|(c, j)| serde_json::from_str(&j).ok().map(|e| (c, e))).collect())
}

impl AgentSession {
    /// Send a daemon command as the automation, once per `part`.
    async fn command(ctx: &StepContext, machine: &str, part: &str, command: Command) -> Result<(), StepOutcome> {
        ctx.command(machine, part, command).await.map(|_| ())
    }
}

/// An activity's outcome: a transient failure waits (and tries the same activity again), a
/// permanent one is logged and skipped (a lost line in Linear isn't worth the run).
enum Posted {
    Ok,
    Later(String),
}

async fn post(ctx: &mut StepContext, session: &str, content: Value, key: &str, ephemeral: bool) -> Posted {
    let code = ctx.code.clone();
    let id = uuid_for(&format!("activity:{}:{key}", ctx.idempotency_key()));
    match post_activity(&code, &ctx.owner().id.clone(), session, content, Some(&id), ephemeral).await {
        Ok(_) => Posted::Ok,
        Err(e) if e.transient() => Posted::Later(e.to_string()),
        Err(e @ (LinearError::NotConnected | LinearError::Reauth(_) | LinearError::Server(_))) => Posted::Later(e.to_string()),
        Err(e) => {
            ctx.log(LogLevel::Warn, format!("Linear refused an activity: {e}"));
            Posted::Ok
        }
    }
}

macro_rules! post_or_wait {
    ($ctx:expr, $state:expr, $session:expr, $content:expr, $key:expr, $eph:expr) => {
        if let Posted::Later(why) = post($ctx, $session, $content, $key, $eph).await {
            $ctx.log(LogLevel::Warn, format!("Linear: {why}; trying again"));
            $ctx.set_state(serde_json::to_value(&$state).unwrap_or_default());
            return StepOutcome::Wait { after: Duration::from_secs(30) };
        }
    };
}

#[async_trait::async_trait]
impl StepKind for AgentSession {
    fn info(&self) -> StepInfo {
        StepInfo {
            kind: "linear.agent_session".into(),
            title: "Show the agent in Linear".into(),
            description: "Stream a roll thread's progress to the Linear agent session as activities, ask its questions and approvals there, and pass the replies on (they answer, steer or stop it). Done when the thread is idle after its run (its last message is the response).".into(),
            config: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": props!({
                    "session": { "type": "string", "title": "Session", "description": "Agent session id; default: the event's", "format": "template" },
                    "thread": { "type": "string", "title": "Thread", "description": "Default: an earlier step's, else the issue's", "format": "roll-thread" },
                    "machine": { "type": "string", "title": "Machine", "description": "Where a new thread starts (default: the project's)", "format": "roll-machine" },
                    "actions": { "type": "boolean", "description": "Tool calls as actions (default on)" },
                    "thoughts": { "type": "boolean", "description": "Reasoning and interim messages as thoughts (default on)" },
                    "plan": { "type": "boolean", "description": "The agent's todos as the session's plan (default on)" },
                    "respond": { "type": "boolean", "description": "The final message as the response (default on)" },
                }),
            }),
            output: ["outcome", "thread", "machine", "session", "message", "change_url"].map(String::from).to_vec(),
            integration: Some(super::SOURCE.into()),
            needs_setup: false,
        }
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        serde_json::from_value::<Config>(if config.is_null() { json!({}) } else { config.clone() }).map(|_| ()).map_err(|e| e.to_string())
    }

    fn default_retry(&self) -> Retry {
        Retry { attempts: 6, backoff_s: 15 }
    }

    fn default_timeout(&self) -> Duration {
        // An agent that waits on someone's answer in Linear may take a while.
        Duration::from_secs(24 * 3600)
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        self.code.upgrade().is_some_and(|c| !steps::connected(&c, owner))
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let raw = if ctx.step().config.is_null() { json!({}) } else { ctx.step().config.clone() };
        let c: Config = match ctx.render_json(&raw).and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(format!("linear.agent_session config: {e}")),
        };
        let Some(session) = c.session.clone().filter(|s| !s.is_empty()).or_else(|| ctx.context()["event"]["data"]["session"]["id"].as_str().map(str::to_string)) else {
            return StepOutcome::Fail("no agent session: set `session` (the event has none)".into());
        };
        let Some((machine, thread, fresh)) = thread_of(&c, ctx) else {
            return StepOutcome::Fail("no thread to show: start one in an earlier step (agent.start) or set `thread`".into());
        };
        let code = ctx.code.clone();
        let owner = ctx.owner().clone();
        let db = code.db.sql();
        let (machine, t) = match crate::agents::store::thread_of(db, &owner.id, &thread) {
            Ok(Some((m, t))) => (machine.unwrap_or(m), Some(t)),
            Ok(None) => match machine {
                // Not mirrored yet (it was just created): wait for it.
                Some(m) => (m, None),
                None => return StepOutcome::Wait { after: Duration::from_secs(5) },
            },
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let mut st: State = serde_json::from_value(ctx.state().clone()).unwrap_or_default();
        let run = ctx.run().as_str().to_string();

        // 1. The session is this run's to show (a run that still holds it is woken: it takes the
        // replies).
        if !st.claimed {
            match store::claim(db, &session, &run, &machine, &thread, fresh, &|c, r| crate::agents::automations::registry::wake_run(c, r)) {
                Ok(Claim::Taken) => {
                    st.claimed = true;
                    // A thread this run started is done after its first run; one that was going
                    // already, after a run the replies start (or the one going now).
                    st.expect_runs = if fresh { 1 } else { t.as_ref().map(|t| t.run_count).unwrap_or(0).max(1) };
                    // The thread's records wake the run (its own agent.start's do already).
                    ctx.follow_thread(&agent_proto::MachineId(machine.clone()), &agent_proto::ThreadId(thread.clone()));
                }
                Ok(Claim::Held(other)) => {
                    ctx.log(LogLevel::Info, format!("run {other} shows this session; the reply goes to it"));
                    return StepOutcome::Done(json!({ "outcome": "handed", "session": session, "thread": thread, "machine": machine, "run": other }));
                }
                Ok(Claim::Unknown) => return StepOutcome::Fail(format!("roll has no Linear agent session {session} (sessions come from Linear's webhooks)")),
                Err(e) => return StepOutcome::Retry(e.to_string()),
            }
        }
        if !st.linked {
            let url = format!("{}/dashboard/agents?owner={}&focus={}", code.config.web_url.trim_end_matches('/'), owner.name, thread);
            let link = code
                .linear_graphql(&owner.id, "mutation RollSessionUrl($id: String!, $input: AgentSessionUpdateInput!) { agentSessionUpdate(id: $id, input: $input) { success } }", json!({ "id": session, "input": { "addedExternalUrls": [{ "label": "roll thread", "url": url }] } }))
                .await;
            match link {
                Ok(_) => st.linked = true,
                Err(e) if e.transient() => {
                    ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
                    return StepOutcome::Wait { after: Duration::from_secs(30) };
                }
                Err(e) => {
                    ctx.log(LogLevel::Warn, format!("linking the thread on the session: {e}"));
                    st.linked = true;
                }
            }
        }

        // A new session on an issue whose thread goes on: what was asked goes to that thread.
        if !fresh && !st.opened && ctx.context()["event"]["kind"].as_str() == Some("agent_session.created") {
            let asked = ctx.context()["event"]["data"]["prompt"].as_str().unwrap_or_default().trim().to_string();
            if !asked.is_empty() {
                let who = ctx.context()["event"]["data"]["user"]["name"].as_str().unwrap_or("someone").to_string();
                let busy = t.as_ref().is_some_and(|t| t.status.is_busy() || t.active_run.is_some());
                if !busy {
                    st.expect_runs = st.expect_runs.max(t.as_ref().map(|t| t.run_count + 1).unwrap_or(1));
                }
                let cmd = Command::MessageSend { thread: agent_proto::ThreadId(thread.clone()), text: format!("{who} (in Linear): {asked}"), attachments: vec![], mode: if busy { SendMode::Steer } else { SendMode::Queue } };
                if let Err(o) = Self::command(ctx, &machine, "opened", cmd).await {
                    ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
                    return o;
                }
            }
            st.opened = true;
        }

        // 2. Replies.
        let s = match store::session_db(db, &session) {
            Ok(Some(s)) => s,
            Ok(None) => return StepOutcome::Fail("the session's record is gone".into()),
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let prompts = match store::prompts_after(db, &session, s.consumed_seq) {
            Ok(p) => p,
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let mut pending = s.pending.clone();
        for p in prompts {
            let thread_id = agent_proto::ThreadId(thread.clone());
            let ask = pending.as_ref().and_then(|a| match crate::agents::store::mirrored(db, &machine, agent_proto::EntityKind::Request, &a.request) {
                Ok(Some(Entity::Request(r))) if r.status == RequestStatus::Pending => Some(r),
                _ => None,
            });
            let sent = if p.signal.as_deref() == Some("stop") {
                st.stopped = true;
                Self::command(ctx, &machine, &format!("stop_{}", p.seq), Command::RunInterrupt { thread: thread_id }).await
            } else if let Some(r) = ask {
                let response = answer(&r, &p.body, p.signal_value.as_deref());
                ctx.log(LogLevel::Info, format!("{} answered {} in Linear", p.user_name, r.id.0));
                let sent = Self::command(ctx, &machine, &format!("answer_{}", p.seq), Command::RequestRespond { request: r.id.clone(), response }).await;
                if sent.is_ok() {
                    pending = None;
                    st.asked = None;
                    if let Err(e) = store::set_pending(db, &session, None) {
                        return StepOutcome::Retry(e.to_string());
                    }
                }
                sent
            } else {
                let busy = t.as_ref().is_some_and(|t| t.status.is_busy() || t.active_run.is_some());
                if !busy {
                    st.expect_runs = st.expect_runs.max(t.as_ref().map(|t| t.run_count + 1).unwrap_or(1));
                }
                st.responded = None;
                let text = format!("{} (in Linear): {}", p.user_name, p.body);
                Self::command(ctx, &machine, &format!("prompt_{}", p.seq), Command::MessageSend { thread: thread_id, text, attachments: vec![], mode: if busy { SendMode::Steer } else { SendMode::Queue } }).await
            };
            if let Err(o) = sent {
                ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
                return o;
            }
            if let Err(e) = store::consumed(db, &session, &run, p.seq) {
                return StepOutcome::Retry(e.to_string());
            }
        }

        // 3. The thread's records.
        let records = match records_after(&code, &owner.id, &thread, st.cursor) {
            Ok(r) => r,
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let more = records.len() as i64 == PAGE;
        let newest_running = records.iter().rev().find_map(|(_, e)| match e {
            Entity::Node(n) if n.status == NodeStatus::Running && matches!(n.body, NodeBody::Tool(_)) => Some(n.id.0.clone()),
            _ => None,
        });
        for (cursor, e) in records {
            match e {
                Entity::Node(n) => {
                    if let Some(o) = self.show_node(ctx, &mut st, &session, &c, &n, newest_running.as_deref()).await {
                        return o;
                    }
                }
                Entity::Request(r) => {
                    if r.status == RequestStatus::Pending && st.asked.as_deref() != Some(r.id.0.as_str()) && pending.as_ref().is_none_or(|p| p.request != r.id.0) {
                        let ask = PendingAsk {
                            request: r.id.0.clone(),
                            kind: if matches!(r.body, RequestBody::Approval(_)) { "approval".into() } else { "questions".into() },
                            questions: match &r.body {
                                RequestBody::Questions { questions } => questions.iter().map(|q| q.id.clone()).collect(),
                                _ => vec![],
                            },
                        };
                        // The pending ask is stored first: a reply that comes before the
                        // activity is acknowledged still finds it.
                        if let Err(e) = store::set_pending(db, &session, Some(&ask)) {
                            return StepOutcome::Retry(e.to_string());
                        }
                        pending = Some(ask);
                        post_or_wait!(ctx, st, &session, elicitation(&r), &format!("ask_{}", r.id.0), false);
                        st.asked = Some(r.id.0.clone());
                    } else if r.status != RequestStatus::Pending && pending.as_ref().is_some_and(|p| p.request == r.id.0) {
                        // Answered in roll (or cancelled): the question in Linear is moot.
                        pending = None;
                        st.asked = None;
                        if let Err(e) = store::set_pending(db, &session, None) {
                            return StepOutcome::Retry(e.to_string());
                        }
                        if r.status == RequestStatus::Resolved && c.thoughts {
                            let by = r.resolved_by.clone().unwrap_or_else(|| "someone".into());
                            post_or_wait!(ctx, st, &session, json!({ "type": "thought", "body": format!("Answered in roll by {by}.") }), &format!("answered_{}", r.id.0), true);
                        }
                    }
                }
                _ => {}
            }
            st.cursor = cursor;
        }
        if more {
            ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
            return StepOutcome::Wait { after: Duration::from_millis(200) };
        }

        // 4. Done when the thread is idle after the run it was waiting for.
        let t = match crate::agents::store::thread_of(db, &owner.id, &thread) {
            Ok(t) => t.map(|(_, t)| t),
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let Some(t) = t else {
            ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
            return StepOutcome::Wait { after: Duration::from_secs(5) };
        };
        let idle = !t.status.is_busy() && t.active_run.is_none() && t.queued.is_empty();
        if !idle || t.run_count < st.expect_runs {
            ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
            return StepOutcome::Wait { after: IDLE_WAIT };
        }
        let last_run = last_run(&code, &owner.id, &machine, &t);
        let run_key = last_run.as_ref().map(|r| r.id.0.clone()).unwrap_or_else(|| format!("{}", t.run_count));
        let failed = t.status == ThreadStatus::Error || last_run.as_ref().is_some_and(|r| matches!(r.status, agent_proto::RunStatus::Failed));
        let message = st.held.take().map(|(_, m)| m).unwrap_or_default();
        let change = t.change.as_ref().map(|ch| ch.url.clone());
        if st.responded.as_deref() != Some(run_key.as_str()) {
            let content = if st.stopped {
                json!({ "type": "response", "body": "Stopped." })
            } else if failed {
                let why = t.error.clone().or_else(|| last_run.as_ref().and_then(|r| r.error.clone())).unwrap_or_else(|| "the agent's run failed".into());
                json!({ "type": "error", "body": format!("The agent stopped with an error: {why}") })
            } else {
                let mut body = if message.trim().is_empty() { "Done.".to_string() } else { message.clone() };
                if let Some(u) = &change {
                    body.push_str(&format!("\n\n[The change in roll]({u})"));
                }
                json!({ "type": "response", "body": body })
            };
            if c.respond || failed || st.stopped {
                post_or_wait!(ctx, st, &session, content, &format!("end_{run_key}"), false);
            }
            st.responded = Some(run_key);
        }
        match store::release(db, &session, &run) {
            Ok(true) => {}
            Ok(false) => {
                // A reply came in meanwhile: pass it on first.
                ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
                return StepOutcome::Wait { after: Duration::from_millis(100) };
            }
            Err(e) => return StepOutcome::Retry(e.to_string()),
        }
        let outcome = if st.stopped {
            "stopped"
        } else if failed {
            "failed"
        } else {
            "done"
        };
        ctx.set_state(serde_json::to_value(&st).unwrap_or_default());
        StepOutcome::Done(json!({
            "outcome": outcome,
            "session": session,
            "thread": thread,
            "machine": machine,
            "message": message.chars().take(8000).collect::<String>(),
            "change_url": change,
        }))
    }

    async fn cleanup(&self, ctx: &mut StepContext, status: RunState) {
        let Some(session) = ctx.context()["event"]["data"]["session"]["id"].as_str().map(str::to_string).or_else(|| ctx.step().config["session"].as_str().map(str::to_string)) else { return };
        let code = ctx.code.clone();
        let run = ctx.run().as_str().to_string();
        if let Ok(true) = store::abandon(code.db.sql(), &session, &run)
            && matches!(status, RunState::Cancelled | RunState::Failed)
        {
            let body = if status == RunState::Cancelled { "roll's automation run was cancelled." } else { "roll's automation run failed; see its run in roll." };
            let _ = post(ctx, &session, json!({ "type": "error", "body": body }), "cleanup", false).await;
        }
    }
}

impl AgentSession {
    /// Show one node (once per state); `Some` when the step has to wait.
    async fn show_node(&self, ctx: &mut StepContext, st: &mut State, session: &str, c: &Config, n: &Node, newest_running: Option<&str>) -> Option<StepOutcome> {
        let id = n.id.0.as_str();
        let done = !matches!(n.status, NodeStatus::Running | NodeStatus::Waiting);
        macro_rules! send {
            ($content:expr, $key:expr, $eph:expr) => {
                if let Posted::Later(why) = post(ctx, session, $content, $key, $eph).await {
                    ctx.log(LogLevel::Warn, format!("Linear: {why}; trying again"));
                    ctx.set_state(serde_json::to_value(&*st).unwrap_or_default());
                    return Some(StepOutcome::Wait { after: Duration::from_secs(30) });
                }
            };
        }
        // A message held back is an interim one once anything comes after it.
        if let Some((held_id, text)) = st.held.clone()
            && held_id != id
        {
            st.held = None;
            if c.thoughts {
                send!(json!({ "type": "thought", "body": text }), &format!("msg_{held_id}"), false);
            }
        }
        match &n.body {
            NodeBody::Tool(t) if c.actions => {
                if done && !st.shown(id, "done") {
                    send!(action(t, true), &format!("tool_{id}"), false);
                    st.show(id, "done");
                } else if !done && !st.shown(id, "running") && newest_running == Some(id) {
                    send!(action(t, false), &format!("tool_{id}_running"), true);
                    st.show(id, "running");
                }
            }
            NodeBody::Reasoning { text } if c.thoughts && done && !st.shown(id, "done") && !text.trim().is_empty() => {
                send!(json!({ "type": "thought", "body": text }), &format!("think_{id}"), false);
                st.show(id, "done");
            }
            NodeBody::Message { text } if done && !st.shown(id, "done") && !text.trim().is_empty() => {
                st.held = Some((id.to_string(), text.clone()));
                st.show(id, "done");
            }
            NodeBody::Todos { items } if c.plan => {
                let plan: Vec<Value> = items
                    .iter()
                    .map(|t| json!({ "content": t.text, "status": match t.status { TodoStatus::Pending => "pending", TodoStatus::InProgress => "inProgress", TodoStatus::Completed => "completed" } }))
                    .collect();
                let hash = crate::hash_token(&serde_json::to_string(&plan).unwrap_or_default());
                if st.plan.as_deref() != Some(hash.as_str()) {
                    let code = ctx.code.clone();
                    let r = code
                        .linear_graphql(&ctx.owner().id.clone(), "mutation RollSessionPlan($id: String!, $input: AgentSessionUpdateInput!) { agentSessionUpdate(id: $id, input: $input) { success } }", json!({ "id": session, "input": { "plan": plan } }))
                        .await;
                    match r {
                        Err(e) if e.transient() => {
                            ctx.set_state(serde_json::to_value(&*st).unwrap_or_default());
                            return Some(StepOutcome::Wait { after: Duration::from_secs(30) });
                        }
                        Err(e) => ctx.log(LogLevel::Warn, format!("Linear refused the plan: {e}")),
                        Ok(_) => {}
                    }
                    st.plan = Some(hash);
                }
            }
            NodeBody::Subagent { agent, prompt, .. } if c.actions && !st.shown(id, if done { "done" } else { "running" }) => {
                let v = json!({ "type": "action", "action": if done { "Delegated" } else { "Delegating" }, "parameter": format!("{agent}: {}", prompt.chars().take(200).collect::<String>()) });
                send!(v, &format!("sub_{id}_{done}"), !done);
                st.show(id, if done { "done" } else { "running" });
            }
            _ => {}
        }
        None
    }
}

/// The thread's newest run, from the mirror.
fn last_run(code: &Code, owner_id: &str, machine: &str, t: &Thread) -> Option<agent_proto::Run> {
    let rows: Vec<String> = code
        .db
        .sql()
        .query(
            "SELECT json FROM agent_entities WHERE owner_id = ? AND machine_id = ? AND thread = ? AND kind = 'run' ORDER BY cursor DESC LIMIT 20",
            code_sql::params![owner_id, machine, t.id.0.as_str()],
            |r| r.get(0),
        )
        .ok()?;
    rows.into_iter()
        .filter_map(|j| match serde_json::from_str(&j) {
            Ok(Entity::Run(r)) => Some(r),
            _ => None,
        })
        .max_by_key(|r| r.ordinal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_proto::{Approval, ApprovalAction, NodeId, Question, QuestionOption, RequestId, RunId, ThreadId};

    fn request(body: RequestBody) -> Request {
        Request {
            id: RequestId("req_1".into()),
            thread: ThreadId("thr_1".into()),
            run: RunId("run_1".into()),
            node: NodeId("nod_1".into()),
            status: RequestStatus::Pending,
            body,
            created_at: 0,
            resolved_at: None,
            resolved_by: None,
            response: None,
        }
    }

    fn approval() -> Request {
        request(RequestBody::Approval(Approval {
            action: ApprovalAction::Command,
            title: "Run `cargo test`".into(),
            command: Some("cargo test".into()),
            cwd: None,
            files: vec![],
            reason: None,
            tool: None,
            input: Value::Null,
            decisions: vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel],
        }))
    }

    fn decision(r: Response) -> (Decision, Option<String>) {
        match r {
            Response::Decision { decision, message } => (decision, message),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn replies_to_approvals_decide_them() {
        let a = approval();
        assert_eq!(decision(answer(&a, "Approve", None)), (Decision::Accept, None));
        assert_eq!(decision(answer(&a, "yes!", None)), (Decision::Accept, None));
        assert_eq!(decision(answer(&a, "LGTM", None)), (Decision::Accept, None));
        assert_eq!(decision(answer(&a, "whatever", Some("always"))), (Decision::AcceptForSession, None));
        assert_eq!(decision(answer(&a, "no\nuse nextest instead", None)), (Decision::Decline, Some("use nextest instead".into())));
        assert_eq!(decision(answer(&a, "stop", None)), (Decision::Cancel, None));
        // Anything else declines, and tells the agent what to do instead.
        assert_eq!(decision(answer(&a, "run only the unit tests", None)), (Decision::Decline, Some("run only the unit tests".into())));
    }

    #[test]
    fn replies_to_questions_answer_them() {
        let q = |id: &str, header: &str, options: &[&str]| Question {
            id: id.into(),
            header: header.into(),
            question: format!("{header}?"),
            options: options.iter().map(|o| QuestionOption { label: o.to_string(), description: None }).collect(),
            multi: false,
        };
        let one = request(RequestBody::Questions { questions: vec![q("db", "Database", &["Postgres", "SQLite"])] });
        let answers = |r: Response| match r {
            Response::Answers { answers } => answers,
            other => panic!("{other:?}"),
        };
        assert_eq!(answers(answer(&one, "sqlite", None))["db"], "SQLite");
        assert_eq!(answers(answer(&one, "1", None))["db"], "Postgres");
        assert_eq!(answers(answer(&one, "MySQL please", None))["db"], "MySQL please");
        let two = request(RequestBody::Questions { questions: vec![q("db", "Database", &["Postgres", "SQLite"]), q("name", "Name", &[])] });
        let a = answers(answer(&two, "Name: billing\nDatabase: postgres", None));
        assert_eq!(a["db"], "Postgres");
        assert_eq!(a["name"], "billing");
        let a = answers(answer(&two, "1. 2\n2. ledger", None));
        assert_eq!(a["db"], "SQLite");
        assert_eq!(a["name"], "ledger");
    }
}
