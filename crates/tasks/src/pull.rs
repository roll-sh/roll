//! Pull consumers: workers that can't take HTTP requests (private networks, long jobs) lease
//! messages from a queue, then ack, nack or extend them. A lease that lapses makes the message
//! available again; after `retries` it goes to the dead-letter queue.

use crate::engine::{Result, TaskError, Tasks, invalid};
use crate::flow::{self, Acquire};
use crate::model::*;
use base64::Engine;
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct Pulled {
    pub id: String,
    /// Pass to ack / nack / extend. Valid while the lease holds.
    pub receipt: String,
    /// UTF-8 bodies as text; anything else is in `body_base64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub attempts: u32,
    pub retries: u32,
    pub created_at: Ms,
    pub lease_until: Ms,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

fn parse_receipt(receipt: &str) -> Result<(&str, &str)> {
    receipt.rsplit_once('.').ok_or_else(|| TaskError::Invalid("bad receipt".into()))
}

impl Tasks {
    pub async fn pull(&self, queue: &str, max: usize, visibility_ms: i64, wait_ms: i64) -> Result<Vec<Pulled>> {
        // Consumers may start before anything is published.
        let q = self.ensure_queue(queue).await?;
        let max = max.clamp(1, 100);
        let visibility_ms = visibility_ms.clamp(1_000, 12 * 3_600_000);
        let deadline = now_ms() + wait_ms.clamp(0, 20_000);
        let lane = Queue::pull_lane(queue);
        loop {
            let mut out = Vec::new();
            if !q.paused && !self.queue_cached(queue).is_some_and(|q| q.paused) {
                let due = self.store.msg_due(&lane, now_ms(), max * 2).await?;
                for id in due.ids {
                    if out.len() >= max {
                        break;
                    }
                    if let Some(p) = self.lease_pulled(&q, &id, visibility_ms).await? {
                        out.push(p);
                    }
                }
            }
            if !out.is_empty() || now_ms() >= deadline {
                return Ok(out);
            }
            let left = (deadline - now_ms()).clamp(1, 250) as u64;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(left)) => {}
                _ = self.pull_wake.notified() => {}
            }
        }
    }

    async fn lease_pulled(&self, q: &Queue, id: &str, visibility_ms: i64) -> Result<Option<Pulled>> {
        let Some(m) = self.store.msg_get(id).await? else { return Ok(None) };
        let now = now_ms();
        if !matches!(m.state, MsgState::Pending | MsgState::Active) || m.due_at > now {
            return Ok(None);
        }
        if let Some(g) = &m.group
            && self.store.msg_group_head(g).await?.as_deref() != Some(id)
        {
            return Ok(None);
        }
        let v = m.version;
        let mut c = m;
        // A lapsed lease counts as a failed attempt.
        if c.attempts > c.retries {
            c.state = MsgState::Failed;
            c.reason = Some("lease lapsed after the last retry".into());
            c.expires_at = Some(now + self.config.retention_ms);
            c.lease = None;
            if self.store.msg_update(&c, v).await? {
                c.version = v + 1;
                self.count(|u| u.dead += 1);
                self.after_final(&c).await;
            }
            return Ok(None);
        }
        let lease = crate::ids::ulid();
        let until = now + visibility_ms;
        if let Some(p) = q.parallelism {
            match flow::acquire(self, &format!("q:{}", q.name), p, &c.id, until).await? {
                Acquire::Granted => {}
                Acquire::Full => return Ok(None),
            }
        }
        if let Some(rate) = q.rate
            && flow::take(self, &format!("q:{}", q.name), rate).await? > 0
        {
            self.release_pull_slot(q, &c.id).await;
            return Ok(None);
        }
        c.state = MsgState::Active;
        c.attempts += 1;
        c.lease = Some(lease.clone());
        c.due_at = until;
        c.updated_at = now;
        if !self.store.msg_update(&c, v).await? {
            self.release_pull_slot(q, &c.id).await;
            return Ok(None);
        }
        self.count(|u| u.deliveries += 1);
        let body = self.store.msg_body(&c.id).await?.unwrap_or_default();
        let (text, b64) = match String::from_utf8(body) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(base64::engine::general_purpose::STANDARD.encode(e.as_bytes()))),
        };
        Ok(Some(Pulled {
            receipt: format!("{}.{lease}", c.id),
            id: c.id,
            body: text,
            body_base64: b64,
            content_type: c.content_type,
            attempts: c.attempts,
            retries: c.retries,
            created_at: c.created_at,
            lease_until: until,
            label: c.label,
            group: c.group.and_then(|g| g.split_once('/').map(|(_, g)| g.to_string())),
        }))
    }

    async fn release_pull_slot(&self, q: &Queue, id: &str) {
        if q.parallelism.is_some() {
            let _ = flow::release(self, &format!("q:{}", q.name), id).await;
        }
    }

    async fn leased(&self, queue: &str, receipt: &str) -> Result<Message> {
        let (id, lease) = parse_receipt(receipt)?;
        let m = self.message(id).await?;
        if m.queue.as_deref() != Some(queue) {
            return Err(TaskError::NotFound(format!("message {id} in queue {queue}")));
        }
        if m.state != MsgState::Active || m.lease.as_deref() != Some(lease) {
            return Err(TaskError::Conflict(format!("receipt for {id} is no longer valid (the lease lapsed or it was settled)")));
        }
        Ok(m)
    }

    pub async fn ack(&self, queue: &str, receipt: &str) -> Result<()> {
        let mut m = self.leased(queue, receipt).await?;
        let v = m.version;
        let now = now_ms();
        m.state = MsgState::Delivered;
        m.lease = None;
        m.updated_at = now;
        m.expires_at = Some(now + self.config.retention_ms);
        m.last = Some(Outcome { at: now, ..Default::default() });
        if !self.store.msg_update(&m, v).await? {
            return Err(TaskError::Conflict("message changed; try again".into()));
        }
        m.version = v + 1;
        self.count(|u| u.delivered += 1);
        if let Some(q) = self.queue_cached(queue) {
            self.release_pull_slot(&q, &m.id).await;
        }
        self.after_final(&m).await;
        Ok(())
    }

    /// Give a message back: retried after `delay_ms` (or the backoff), or dead-lettered.
    /// Give a message back: retried after `delay_ms` (default: its backoff), or dead-lettered
    /// now when `dead` or out of retries.
    pub async fn nack(&self, queue: &str, receipt: &str, delay_ms: Option<i64>, error: Option<String>, dead: bool) -> Result<Message> {
        let mut m = self.leased(queue, receipt).await?;
        let v = m.version;
        let now = now_ms();
        m.lease = None;
        m.updated_at = now;
        m.last = Some(Outcome { at: now, error: error.clone(), ..Default::default() });
        if dead || m.attempts > m.retries {
            m.state = MsgState::Failed;
            m.reason = Some(error.clone().unwrap_or_else(|| "rejected by the consumer".into()));
            m.expires_at = Some(now + self.config.retention_ms);
        } else {
            m.state = MsgState::Pending;
            m.due_at = now + delay_ms.unwrap_or_else(|| m.backoff.delay_ms(m.attempts, crate::ids::random_f64())).max(0);
        }
        if !self.store.msg_update(&m, v).await? {
            return Err(TaskError::Conflict("message changed; try again".into()));
        }
        m.version = v + 1;
        let _ = self
            .store
            .attempt_add(&Attempt {
                message_id: m.id.clone(),
                n: m.attempts,
                at: now,
                duration_ms: 0,
                status: None,
                error,
                response: None,
                next_at: (m.state == MsgState::Pending).then_some(m.due_at),
            })
            .await;
        if let Some(q) = self.queue_cached(queue) {
            self.release_pull_slot(&q, &m.id).await;
        }
        if m.state.is_final() {
            self.count(|u| u.dead += 1);
            self.after_final(&m).await;
        } else {
            self.count(|u| u.failed_attempts += 1);
        }
        Ok(m)
    }

    /// Keep working on a message: push its lease out.
    pub async fn extend_lease(&self, queue: &str, receipt: &str, visibility_ms: i64) -> Result<Ms> {
        if visibility_ms <= 0 {
            return invalid("visibility must be positive");
        }
        let mut m = self.leased(queue, receipt).await?;
        let v = m.version;
        m.due_at = now_ms() + visibility_ms.min(12 * 3_600_000);
        if !self.store.msg_update(&m, v).await? {
            return Err(TaskError::Conflict("message changed; try again".into()));
        }
        if let Some(q) = self.queue_cached(queue)
            && q.parallelism.is_some()
        {
            let _ = flow::extend(self, &format!("q:{queue}"), &m.id, m.due_at).await;
        }
        Ok(m.due_at)
    }
}
