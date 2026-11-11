//! A store that dies after a given number of writes, as a crashed node does: the write it dies
//! at doesn't happen, and nothing after it does. A new engine on the inner store is the restart.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use serde_json::Value;
use tasks::model::*;
use tasks::store::*;

pub struct Crashy {
    pub inner: Arc<dyn Store>,
    /// Writes left before the crash (negative: never).
    left: AtomicI64,
    dead: AtomicBool,
}

impl Crashy {
    pub fn new(inner: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self { inner, left: AtomicI64::new(-1), dead: AtomicBool::new(false) })
    }

    /// Die at the `n`th write from now (0: the next one).
    pub fn crash_at(&self, n: i64) {
        self.left.store(n, Ordering::SeqCst);
    }

    pub fn crashed(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    fn alive(&self) -> Result<()> {
        anyhow::ensure!(!self.crashed(), "crashed");
        Ok(())
    }

    fn write(&self) -> Result<()> {
        self.alive()?;
        if self.left.fetch_sub(1, Ordering::SeqCst) == 0 {
            self.dead.store(true, Ordering::SeqCst);
            anyhow::bail!("crashed (failpoint)");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Store for Crashy {
    fn describe(&self) -> String {
        format!("crashy {}", self.inner.describe())
    }

    fn shards(&self) -> u32 {
        self.inner.shards()
    }

    async fn write_all(&self, writes: &[Write]) -> Result<bool> {
        self.write()?;
        self.inner.write_all(writes).await
    }

    async fn msg_insert(&self, m: &Message, body: &[u8]) -> Result<()> {
        self.write()?;
        self.inner.msg_insert(m, body).await
    }

    async fn msg_get(&self, id: &str) -> Result<Option<Message>> {
        self.alive()?;
        self.inner.msg_get(id).await
    }

    async fn msg_body(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.alive()?;
        self.inner.msg_body(id).await
    }

    async fn msg_update(&self, m: &Message, expect: u64) -> Result<bool> {
        self.write()?;
        self.inner.msg_update(m, expect).await
    }

    async fn msg_delete(&self, id: &str) -> Result<()> {
        self.write()?;
        self.inner.msg_delete(id).await
    }

    async fn msg_due(&self, lane: &str, now: Ms, limit: usize) -> Result<Due> {
        self.alive()?;
        self.inner.msg_due(lane, now, limit).await
    }

    async fn msg_group_head(&self, group: &str) -> Result<Option<String>> {
        self.alive()?;
        self.inner.msg_group_head(group).await
    }

    async fn msg_list(&self, q: &MessageQuery) -> Result<Page<Message>> {
        self.alive()?;
        self.inner.msg_list(q).await
    }

    async fn lane_depth(&self, lane: &str, now: Ms) -> Result<(u64, u64, u64)> {
        self.alive()?;
        self.inner.lane_depth(lane, now).await
    }

    async fn attempt_add(&self, a: &Attempt) -> Result<()> {
        self.write()?;
        self.inner.attempt_add(a).await
    }

    async fn attempts(&self, message_id: &str) -> Result<Vec<Attempt>> {
        self.alive()?;
        self.inner.attempts(message_id).await
    }

    async fn kv_get(&self, key: &str) -> Result<Option<Versioned<Value>>> {
        self.alive()?;
        self.inner.kv_get(key).await
    }

    async fn kv_put(&self, key: &str, value: &Value, expect: Expect, expires_at: Option<Ms>) -> Result<bool> {
        self.write()?;
        self.inner.kv_put(key, value, expect, expires_at).await
    }

    async fn kv_delete(&self, key: &str, expect: Expect) -> Result<bool> {
        self.write()?;
        self.inner.kv_delete(key, expect).await
    }

    async fn doc_get(&self, kind: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        self.alive()?;
        self.inner.doc_get(kind, id).await
    }

    async fn doc_put(&self, kind: &str, id: &str, value: &Value, expect: Expect) -> Result<bool> {
        self.write()?;
        self.inner.doc_put(kind, id, value, expect).await
    }

    async fn doc_delete(&self, kind: &str, id: &str) -> Result<bool> {
        self.write()?;
        self.inner.doc_delete(kind, id).await
    }

    async fn doc_list(&self, kind: &str) -> Result<Vec<Versioned<Value>>> {
        self.alive()?;
        self.inner.doc_list(kind).await
    }

    async fn run_get(&self, id: &str) -> Result<Option<Run>> {
        self.alive()?;
        self.inner.run_get(id).await
    }

    async fn run_put(&self, run: &Run, expect: Expect) -> Result<bool> {
        self.write()?;
        self.inner.run_put(run, expect).await
    }

    async fn run_list(&self, q: &RunQuery) -> Result<Page<Run>> {
        self.alive()?;
        self.inner.run_list(q).await
    }

    async fn steps(&self, run_id: &str) -> Result<Vec<Step>> {
        self.alive()?;
        self.inner.steps(run_id).await
    }

    async fn step_put(&self, step: &Step) -> Result<()> {
        self.write()?;
        self.inner.step_put(step).await
    }

    async fn event_put(&self, e: &Event) -> Result<()> {
        self.write()?;
        self.inner.event_put(e).await
    }

    async fn event_get(&self, id: &str) -> Result<Option<Event>> {
        self.alive()?;
        self.inner.event_get(id).await
    }

    async fn event_list(&self, q: &EventQuery) -> Result<Page<Event>> {
        self.alive()?;
        self.inner.event_list(q).await
    }

    async fn wait_put(&self, w: &Wait) -> Result<()> {
        self.write()?;
        self.inner.wait_put(w).await
    }

    async fn wait_delete(&self, w: &Wait) -> Result<()> {
        self.write()?;
        self.inner.wait_delete(w).await
    }

    async fn waits(&self, event: &str, key: Option<&str>) -> Result<Vec<Wait>> {
        self.alive()?;
        self.inner.waits(event, key).await
    }

    async fn usage_add(&self, day: i64, u: &Usage) -> Result<()> {
        self.write()?;
        self.inner.usage_add(day, u).await
    }

    async fn usage(&self, from_day: i64) -> Result<Vec<(i64, Usage)>> {
        self.alive()?;
        self.inner.usage(from_day).await
    }

    async fn sweep(&self, now: Ms) -> Result<u64> {
        self.write()?;
        self.inner.sweep(now).await
    }
}
