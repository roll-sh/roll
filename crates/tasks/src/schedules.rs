//! Schedules. Each armed schedule has exactly one *tick* message in the due index, so firing
//! gets the same leases, failover and horizontal scale as any delivery. A tick publishes the
//! schedule's target (deduplicated per fire time, so a retried tick never fires twice), then
//! arms the next tick.

use crate::dispatch::Done;
use crate::engine::{NewMessage, PublishRequest, Result, TaskError, Tasks, invalid};
use crate::ids::new_id;
use crate::model::*;
use crate::schedule::{self, Spec};
use crate::store::{self, Expect, Versioned};
use crate::engine::KIND_SCHEDULE;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScheduleRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub cron: String,
    #[serde(default)]
    pub tz: Option<String>,
    /// Publish this (url / topic / queue + message options) on every fire …
    #[serde(default)]
    pub publish: Option<PublishRequest>,
    /// … or send an event …
    #[serde(default)]
    pub event: Option<EventTemplate>,
    /// … or start a function run.
    #[serde(default)]
    pub function: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct EventTemplate {
    pub name: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ScheduleUpdate {
    pub cron: Option<String>,
    #[serde(default, deserialize_with = "crate::engine::double_option")]
    pub tz: Option<Option<String>>,
    pub publish: Option<PublishRequest>,
    pub event: Option<EventTemplate>,
    pub function: Option<String>,
    pub paused: Option<bool>,
    #[serde(default, deserialize_with = "crate::engine::double_option")]
    pub label: Option<Option<String>>,
}

fn spec(s: &Schedule) -> Result<Spec> {
    schedule::parse(&s.cron, s.tz.as_deref()).map_err(TaskError::Invalid)
}

impl Tasks {
    pub async fn schedules(&self) -> Result<Vec<Schedule>> {
        Ok(store::list_docs(&*self.store, KIND_SCHEDULE).await?)
    }

    pub async fn schedule(&self, id: &str) -> Result<Versioned<Schedule>> {
        store::get_doc(&*self.store, KIND_SCHEDULE, id).await?.ok_or_else(|| TaskError::NotFound(format!("schedule {id}")))
    }

    async fn check_target(&self, target: &ScheduleTarget) -> Result<()> {
        match target {
            ScheduleTarget::Publish { request } => {
                let r = request.as_ref();
                if r.url.is_none() && r.topic.is_none() && r.queue.is_none() {
                    return invalid("a schedule's publish needs a url, topic or queue");
                }
                if let Some(u) = &r.url {
                    crate::engine::check_url(u, self.config.block_private_urls)?;
                }
                crate::engine::request_body(r)?;
            }
            ScheduleTarget::Event { name, .. } if name.is_empty() => return invalid("event name is empty"),
            ScheduleTarget::Function { function_id } if self.cache.read().unwrap().registry.function(function_id).is_none() => {
                return Err(TaskError::NotFound(format!("function {function_id}")));
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn create_schedule(&self, req: ScheduleRequest) -> Result<Schedule> {
        let target = match (req.publish, req.event, req.function) {
            (Some(p), None, None) => ScheduleTarget::Publish { request: Box::new(p) },
            (None, Some(e), None) => ScheduleTarget::Event { name: e.name, data: e.data },
            (None, None, Some(f)) => ScheduleTarget::Function { function_id: f },
            _ => return invalid("give exactly one of publish, event or function"),
        };
        let id = req.id.unwrap_or_else(|| new_id("sch"));
        if id.is_empty() || id.len() > 128 || id.contains(['/', ' ']) {
            return invalid("schedule ids are 1–128 characters without spaces or /");
        }
        let now = now_ms();
        let s = Schedule {
            id: id.clone(),
            cron: req.cron,
            tz: req.tz.filter(|t| !t.is_empty()),
            target,
            paused: req.paused,
            next_at: None,
            last_at: None,
            last_message_id: None,
            tick_id: None,
            label: req.label,
            created_at: now,
            updated_at: now,
        };
        spec(&s)?;
        self.check_target(&s.target).await?;
        if !store::put_doc(&*self.store, KIND_SCHEDULE, &id, &s, Expect::Absent).await? {
            return Err(TaskError::Conflict(format!("schedule {id} exists")));
        }
        self.arm(&id).await
    }

    /// Create or replace a schedule with a known id (used for function cron triggers).
    pub(crate) async fn upsert_schedule(&self, id: &str, cron: &str, tz: Option<String>, target: ScheduleTarget) -> Result<()> {
        match self.schedule(id).await {
            Ok(existing) if existing.value.cron == cron && existing.value.tz == tz && existing.value.target == target => Ok(()),
            Ok(_) => {
                let (publish, event, function) = split_target(target);
                self.update_schedule(id, ScheduleUpdate { cron: Some(cron.into()), tz: Some(tz), publish, event, function, ..Default::default() })
                    .await
                    .map(|_| ())
            }
            Err(TaskError::NotFound(_)) => {
                let (publish, event, function) = split_target(target);
                self.create_schedule(ScheduleRequest { id: Some(id.into()), cron: cron.into(), tz, publish, event, function, ..Default::default() })
                    .await
                    .map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn update_schedule(&self, id: &str, u: ScheduleUpdate) -> Result<Schedule> {
        for _ in 0..10 {
            let v = self.schedule(id).await?;
            let mut s = v.value;
            if let Some(c) = &u.cron {
                s.cron = c.clone();
            }
            if let Some(tz) = &u.tz {
                s.tz = tz.clone().filter(|t| !t.is_empty());
            }
            match (&u.publish, &u.event, &u.function) {
                (Some(p), None, None) => s.target = ScheduleTarget::Publish { request: Box::new(p.clone()) },
                (None, Some(e), None) => s.target = ScheduleTarget::Event { name: e.name.clone(), data: e.data.clone() },
                (None, None, Some(f)) => s.target = ScheduleTarget::Function { function_id: f.clone() },
                (None, None, None) => {}
                _ => return invalid("give at most one of publish, event or function"),
            }
            if let Some(p) = u.paused {
                s.paused = p;
            }
            if let Some(l) = &u.label {
                s.label = l.clone();
            }
            spec(&s)?;
            self.check_target(&s.target).await?;
            // Disarm; `arm` sets a fresh tick.
            let old_tick = s.tick_id.take();
            s.next_at = None;
            s.updated_at = now_ms();
            if store::put_doc(&*self.store, KIND_SCHEDULE, id, &s, Expect::Version(v.version)).await? {
                if let Some(t) = old_tick {
                    let _ = self.cancel(&t, "schedule changed").await;
                }
                return self.arm(id).await;
            }
        }
        Err(TaskError::Conflict(format!("schedule {id} is changing; try again")))
    }

    pub async fn pause_schedule(&self, id: &str, paused: bool) -> Result<Schedule> {
        self.update_schedule(id, ScheduleUpdate { paused: Some(paused), ..Default::default() }).await
    }

    pub async fn delete_schedule(&self, id: &str) -> Result<()> {
        let s = self.schedule(id).await?.value;
        self.store.doc_delete(KIND_SCHEDULE, id).await?;
        if let Some(t) = s.tick_id {
            let _ = self.cancel(&t, "schedule deleted").await;
        }
        Ok(())
    }

    /// Fire a schedule now, outside its cadence.
    pub async fn trigger_schedule(&self, id: &str) -> Result<Value> {
        let s = self.schedule(id).await?.value;
        let fired = self.fire_target(&s, now_ms(), None).await?;
        Ok(fired)
    }

    /// Set `next_at` and the tick message for an unpaused schedule with no tick.
    async fn arm(&self, id: &str) -> Result<Schedule> {
        for _ in 0..10 {
            let v = self.schedule(id).await?;
            let mut s = v.value;
            if s.paused || s.tick_id.is_some() {
                return Ok(s);
            }
            let now = now_ms();
            let next = spec(&s)?.next_after(now, s.created_at).ok_or_else(|| TaskError::Invalid("schedule never fires".into()))?;
            let tick = self
                .enqueue(NewMessage { schedule_id: Some(id.into()), ..NewMessage::internal(Target::Tick { schedule_id: id.into(), at: next }, next) }, b"")
                .await?;
            s.next_at = Some(next);
            s.tick_id = Some(tick.id.clone());
            if store::put_doc(&*self.store, KIND_SCHEDULE, id, &s, Expect::Version(v.version)).await? {
                return Ok(s);
            }
            let _ = self.cancel(&tick.id, "superseded").await;
        }
        Err(TaskError::Conflict(format!("schedule {id} is changing; try again")))
    }

    /// Publish / send / start whatever the schedule targets. `at` identifies the fire.
    async fn fire_target(&self, s: &Schedule, at: Ms, dedup: Option<String>) -> Result<Value> {
        match &s.target {
            ScheduleTarget::Publish { request } => {
                let mut req = (**request).clone();
                req.dedup_id = dedup.or(req.dedup_id);
                req.label = req.label.or_else(|| s.label.clone());
                let out = self.publish_from(req, Some(s.id.clone())).await?;
                Ok(json!(out.iter().map(|p| &p.message_id).collect::<Vec<_>>()))
            }
            ScheduleTarget::Event { name, data } => {
                let ids = self
                    .send_events(vec![crate::workflow::EventIn {
                        name: name.clone(),
                        data: data.clone(),
                        id: dedup,
                        ts: Some(at),
                        user: None,
                    }])
                    .await?;
                Ok(json!(ids))
            }
            ScheduleTarget::Function { function_id } => {
                let event = crate::workflow::EventIn {
                    name: "roll/scheduled.timer".into(),
                    data: json!({ "cron": s.cron, "schedule_id": s.id, "at": at }),
                    id: dedup,
                    ts: Some(at),
                    user: None,
                };
                let run = self.invoke_function(function_id, event).await?;
                Ok(json!([run]))
            }
        }
    }
}

fn split_target(t: ScheduleTarget) -> (Option<PublishRequest>, Option<EventTemplate>, Option<String>) {
    match t {
        ScheduleTarget::Publish { request } => (Some(*request), None, None),
        ScheduleTarget::Event { name, data } => (None, Some(EventTemplate { name, data }), None),
        ScheduleTarget::Function { function_id } => (None, None, Some(function_id)),
    }
}

/// A tick came due.
pub(crate) async fn fire(t: &Arc<Tasks>, tick: &Message) -> anyhow::Result<Done> {
    let Target::Tick { schedule_id, at } = &tick.target else { unreachable!() };
    let Some(v) = store::get_doc::<Schedule>(&*t.store, KIND_SCHEDULE, schedule_id).await? else {
        return Ok(Done::Skip("schedule deleted".into()));
    };
    let s = v.value;
    if s.paused || s.tick_id.as_deref() != Some(&tick.id) {
        return Ok(Done::Skip("schedule changed".into()));
    }
    // The fire is deduplicated on (schedule, time): a retried tick can't double-fire.
    let fired = t
        .fire_target(&s, *at, Some(format!("sched:{}:{at}", s.id)))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let last_message_id = fired.as_array().and_then(|a| a.first()).and_then(Value::as_str).map(String::from);

    // Arm the next tick. After downtime, missed fires collapse into the one just made.
    let spec = schedule::parse(&s.cron, s.tz.as_deref()).map_err(|e| anyhow::anyhow!(e))?;
    let next = spec.next_after((*at).max(now_ms()), s.created_at);
    for _ in 0..10 {
        let Some(v) = store::get_doc::<Schedule>(&*t.store, KIND_SCHEDULE, schedule_id).await? else { break };
        let mut s = v.value;
        if s.tick_id.as_deref() != Some(&tick.id) {
            break;
        }
        s.last_at = Some(*at);
        s.last_message_id = last_message_id.clone();
        s.updated_at = now_ms();
        let new_tick = match next {
            Some(n) => Some(
                t.enqueue(
                    NewMessage { schedule_id: Some(s.id.clone()), ..NewMessage::internal(Target::Tick { schedule_id: s.id.clone(), at: n }, n) },
                    b"",
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            ),
            None => None,
        };
        s.next_at = next;
        s.tick_id = new_tick.as_ref().map(|m| m.id.clone());
        if store::put_doc(&*t.store, KIND_SCHEDULE, schedule_id, &s, Expect::Version(v.version)).await? {
            break;
        }
        if let Some(m) = new_tick {
            let _ = t.cancel(&m.id, "superseded").await;
        }
    }
    Ok(Done::Ok(Outcome { at: now_ms(), body: Some(fired.to_string()), ..Default::default() }))
}
