//! The trigger and step registry (docs/code/automations.md, "Integrations"). Integrations
//! (Linear, Slack, GitHub, …) plug in here, without the engine knowing them:
//!
//! - a [`TriggerSource`] describes the events it brings (for the builder's catalog), checks a
//!   trigger's config when an automation is saved, and says whether an event matches a trigger's
//!   config. Its events go into the owner's durable inbox through [`Code::automation_event`] (or
//!   [`record_event`] inside the integration's own write), each with its own id as the
//!   idempotency key (`<source>:<id>`): the same event recorded twice runs nothing twice.
//! - a [`StepKind`] is an action: a JSON config (its JSON Schema in the catalog, checked at save
//!   time by [`StepKind::check`]) and an async [`StepKind::run`] against a [`StepContext`], which
//!   gives it the run's context and templates, the automation's sealed secrets, an SSRF-guarded
//!   HTTP client, the API as the automation, durable scratch state and a log. A run attempt may
//!   be repeated (a node dies mid-step, a retry): make it idempotent with
//!   [`StepContext::idempotency_key`] (daemon commands, `Idempotency-Key` headers, …).
//!
//! Register at startup: `code.automations.registry.register_step(Arc::new(MyStep))`.

use super::context::StepContext;
use crate::{Code, Result};
use agent_proto::automation::{AutomationEvent, AutomationEventId, AutomationId, Retry, RunState, StepInfo, TriggerInfo};
use code_sql::Conn;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// `json!({ "a": …, "b": … })` for a config schema's `properties`, keeping the order they're
/// written in: each field's schema gets `"x-order": <n>`. serde_json's maps are sorted, so without
/// it the builder showed fields alphabetically (Slack's Channel before its Workspace).
macro_rules! props {
    ({ $($body:tt)* }) => {{
        let mut keys: Vec<&'static str> = Vec::new();
        $crate::agents::automations::props!(@key keys; $($body)*);
        $crate::agents::automations::registry::in_order(serde_json::json!({ $($body)* }), &keys)
    }};
    (@key $k:ident;) => {};
    (@key $k:ident; $key:literal : $($rest:tt)*) => {
        $k.push($key);
        $crate::agents::automations::props!(@value $k; $($rest)*);
    };
    (@value $k:ident;) => {};
    (@value $k:ident; , $($rest:tt)*) => { $crate::agents::automations::props!(@key $k; $($rest)*); };
    (@value $k:ident; $t:tt $($rest:tt)*) => { $crate::agents::automations::props!(@value $k; $($rest)*); };
}
pub(crate) use props;

/// Number `properties`' fields in `keys`' order (see [`props!`]).
pub(crate) fn in_order(mut properties: Value, keys: &[&str]) -> Value {
    if let Value::Object(m) = &mut properties {
        for (i, k) in keys.iter().enumerate() {
            if let Some(Value::Object(field)) = m.get_mut(*k) {
                field.insert("x-order".into(), i.into());
            }
        }
    }
    properties
}

/// The owner an automation belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRef {
    pub id: String,
    pub name: String,
}

/// A source of events: built in (`manual`, `schedule`, `code`, `agent`, …) or an integration's.
pub trait TriggerSource: Send + Sync + 'static {
    /// Its `source` (a [`Trigger::Integration`](agent_proto::automation::Trigger)'s `source`),
    /// title, event kinds and the JSON Schema of its config.
    fn info(&self) -> TriggerInfo;

    /// When an automation is saved: whether `events` and `config` are right for `owner` (the
    /// integration is connected, the team exists, …). The message is the 400's.
    fn check(&self, _owner: &OwnerRef, _events: &[String], _config: &Value) -> std::result::Result<(), String> {
        Ok(())
    }

    /// Whether an event of this source is for a trigger with this config (beyond its event
    /// kinds, which the engine checks). The automation's filters come after.
    fn matches(&self, _config: &Value, _event: &AutomationEvent) -> bool {
        true
    }

    /// The owner hasn't set the integration up (the builder says so).
    fn needs_setup(&self, _owner: &OwnerRef) -> bool {
        false
    }
}

/// What one attempt of a step did.
#[derive(Clone, Debug, PartialEq)]
pub enum StepOutcome {
    /// It's done: its output (`steps.<id>.…`), at most [`limits::OUTPUT`](agent_proto::automation::limits::OUTPUT)
    /// bytes of JSON.
    Done(Value),
    /// Not yet (an agent still working, a delay): run it again after `after`, or sooner when an
    /// event about a thread the run started comes in. Keep what's needed in
    /// [`StepContext::set_state`].
    Wait { after: Duration },
    /// A transient failure: tried again with the step's backoff while attempts are left.
    Retry(String),
    /// A permanent failure: the step fails (and the run, unless the step continues on error).
    Fail(String),
}

/// An action a run can take.
#[async_trait::async_trait]
pub trait StepKind: Send + Sync + 'static {
    /// Its `kind` (`agent.start`, `linear.comment`), title, description, the JSON Schema of its
    /// config, and its output's fields.
    fn info(&self) -> StepInfo;

    /// When an automation is saved: whether the config is right. Its templates have already
    /// been parsed.
    fn check(&self, _owner: &OwnerRef, _config: &Value) -> std::result::Result<(), String> {
        Ok(())
    }

    /// Whether `{{secrets.<name>}}` may be used at this place of its config (`config.headers.x`).
    /// Nowhere, unless a kind says so: a secret mustn't end up in a prompt, a body or a log.
    fn secrets_at(&self, _path: &str) -> bool {
        false
    }

    /// Attempts and backoff when the step doesn't say.
    fn default_retry(&self) -> Retry {
        Retry { attempts: 3, backoff_s: 10 }
    }

    /// The longest it may take, waits included, when the step doesn't say.
    fn default_timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    /// One attempt.
    async fn run(&self, ctx: &mut StepContext) -> StepOutcome;

    /// The run ended (`status`) after this step ran: undo what only the run needed (a sandbox it
    /// made). Called once per run and step that has state, best effort.
    async fn cleanup(&self, _ctx: &mut StepContext, _status: RunState) {}

    fn needs_setup(&self, _owner: &OwnerRef) -> bool {
        false
    }
}

/// Every trigger source and step kind this server has.
#[derive(Default)]
pub struct Registry {
    triggers: RwLock<BTreeMap<String, Arc<dyn TriggerSource>>>,
    steps: RwLock<BTreeMap<String, Arc<dyn StepKind>>>,
}

impl Registry {
    /// Add (or replace) a trigger source, by its `source`.
    pub fn register_trigger(&self, t: Arc<dyn TriggerSource>) {
        self.triggers.write().insert(t.info().source, t);
    }

    /// Add (or replace) a step kind, by its `kind`.
    pub fn register_step(&self, s: Arc<dyn StepKind>) {
        self.steps.write().insert(s.info().kind, s);
    }

    pub fn trigger(&self, source: &str) -> Option<Arc<dyn TriggerSource>> {
        self.triggers.read().get(source).cloned()
    }

    pub fn step(&self, kind: &str) -> Option<Arc<dyn StepKind>> {
        self.steps.read().get(kind).cloned()
    }

    /// The catalog, as `owner` sees it (what needs setting up).
    pub fn catalog(&self, owner: &OwnerRef) -> agent_proto::automation::Catalog {
        let triggers = self.triggers.read().values().map(|t| TriggerInfo { needs_setup: t.needs_setup(owner), ..t.info() }).collect();
        let steps = self.steps.read().values().map(|s| StepInfo { needs_setup: s.needs_setup(owner), ..s.info() }).collect();
        agent_proto::automation::Catalog { triggers, steps }
    }
}

/// An event for the inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    /// `linear`, `code`, `agent`, …
    pub source: String,
    /// `issue.created`, `landed`, `thread.done`, …
    pub kind: String,
    /// The source's own id for the event (a delivery id, a log position): its key in the inbox
    /// is `<source>:<id>`, and the same key is recorded once.
    pub id: String,
    /// What happened (`event.data.…`), at most [`limits::EVENT`](agent_proto::automation::limits::EVENT)
    /// bytes of JSON.
    pub data: Value,
    /// For people: one line ("ENG-12 created").
    pub summary: String,
    /// For one automation only (its manual runs, schedule, webhook).
    pub automation: Option<AutomationId>,
}

/// Record `e` in `owner_id`'s inbox inside a caller's write (so the event is in the same
/// transaction as what caused it). `None` if its key was recorded before. Nothing runs until the
/// transaction commits and the engine is woken ([`Code::wake_automations`]).
pub fn record_event(c: &Conn<'_>, owner_id: &str, e: &NewEvent) -> code_sql::Result<Option<AutomationEventId>> {
    super::store::insert_event(c, owner_id, e)
}

/// Have a run looked at now (inside an integration's write: a reply that the run's waiting step
/// handles). Call [`Code::wake_automations`] after the commit. Whether the run is going.
pub fn wake_run(c: &Conn<'_>, run_id: &str) -> code_sql::Result<bool> {
    let n = c.execute(
        "UPDATE automation_runs SET next_at = ? WHERE id = ? AND status IN ('queued', 'running', 'waiting') AND status <> 'queued'",
        code_sql::params![code_sql::now_ms(), run_id],
    )?;
    Ok(n > 0)
}

/// Whether an enabled automation of the owner listens for `kind` of `source` (inside a write).
pub fn listens(c: &Conn<'_>, owner_id: &str, source: &str, kind: &str) -> code_sql::Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM automation_listens WHERE owner_id = ? AND source = ? AND (kind = ? OR kind = '*'))",
        code_sql::params![owner_id, source, kind],
        |r| r.get::<_, bool>(0),
    )
}

impl Code {
    /// Create an automation for an owner server-side (an integration's built-in one), with the
    /// checks `POST …/automations` makes. `by` is who it's from (`linear`, …).
    pub async fn create_automation(&self, owner_id: &str, spec: agent_proto::automation::AutomationSpec, by: &str) -> Result<agent_proto::automation::AutomationSaved> {
        self.start_automations();
        let name = super::super::store::owner_name(self.db.sql(), owner_id)?.ok_or_else(|| crate::Error::NotFound(format!("owner {owner_id}")))?;
        let owner = OwnerRef { id: owner_id.to_string(), name };
        let checked = super::defs::check(self, &owner, &spec)?;
        match super::defs::create(self, &owner, by, checked).await? {
            super::defs::Saved::Done { row, webhook_secret } => {
                self.wake_automations(owner_id);
                Ok(agent_proto::automation::AutomationSaved { automation: super::defs::view(self, &row), webhook_secret })
            }
            super::defs::Saved::TooMany => Err(crate::Error::Unprocessable(format!("an owner has at most {} automations", agent_proto::automation::limits::AUTOMATIONS))),
            _ => Err(crate::Error::Conflict("the automation wasn't saved".into())),
        }
    }

    /// Record an event in `owner_id`'s inbox, durably, and wake the engine. `None` if its key
    /// was recorded before (a redelivery): nothing runs twice.
    pub fn automation_event(&self, owner_id: &str, e: NewEvent) -> Result<Option<AutomationEventId>> {
        let id = self.db.sql().write(|c| record_event(c, owner_id, &e))?;
        if id.is_some() {
            self.wake_automations(owner_id);
        }
        Ok(id)
    }
}
