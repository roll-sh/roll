//! What chat may cost (docs/search/rag.md, "Cost bounds").
//!
//! **Browser keys** (#358) chat only through their own policy entry, `limits.chat`
//! ([`ChatPolicy`]): the profiles they may run (none unless listed), and per minute the answers
//! (`turns_per_minute`), the estimated prompt tokens (`input_tokens_per_minute`) and the output
//! tokens (`output_tokens_per_minute`) they may use, with `max_concurrent` answers at once and
//! at most `max_history_turns` turns of history a request. A key derived from one
//! ([`crate::xpack::security::derived`]) with a `user` also has that user's share: a quarter of
//! each rate, one answer at once. Every answer is also charged `cost` items to the key's rate
//! limits on the node ([`crate::xpack::security::browser::charge_work`]), as its searches are.
//!
//! **Every caller** is held to the profile's `max_tokens_per_day` (prompt and output together).
//!
//! An answer charges its turn when it starts, and its tokens once the prompt is built and before
//! the model is called: the prompt's estimate, and the output cap in full. When it ends, what it
//! didn't use of the output cap goes back (the provider's count, else the estimate of the text).
//! A refusal (429 with `Retry-After`) takes nothing of that step; what earlier steps took stays
//! taken. The buckets live in memory per loaded cluster, on the node that holds it, and are
//! bounded ([`MAX_BUCKETS`]): an unload or a takeover starts them full again.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most buckets (and running counts) one loaded cluster keeps.
pub const MAX_BUCKETS: usize = 50_000;
/// The most history turns any request may send, whatever a key or profile says.
pub const MAX_HISTORY_TURNS: usize = 100;

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);

/// A browser key's chat entry (`metadata.roll_browser.limits.chat`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatPolicy {
    /// The chat profiles it may run.
    pub profiles: Vec<String>,
    pub turns_per_minute: u64,
    pub input_tokens_per_minute: u64,
    pub output_tokens_per_minute: u64,
    pub max_concurrent: u64,
    pub max_history_turns: usize,
    /// Items an answer charges to the key's rate limits.
    pub cost: u64,
}

impl ChatPolicy {
    pub const DEFAULT_TURNS_PER_MINUTE: u64 = 10;
    pub const DEFAULT_INPUT_TOKENS_PER_MINUTE: u64 = 20_000;
    pub const DEFAULT_OUTPUT_TOKENS_PER_MINUTE: u64 = 5_000;
    pub const DEFAULT_MAX_CONCURRENT: u64 = 2;
    pub const DEFAULT_MAX_HISTORY_TURNS: usize = 10;
    pub const DEFAULT_COST: u64 = 10;

    /// The entry a key's `limits` give (`None`: it may not chat).
    pub fn of(limits: Option<&Value>) -> Option<ChatPolicy> {
        let c = limits?.get("chat")?;
        let n = |k: &str| c.get(k).and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).filter(|n| *n > 0);
        let profiles: Vec<String> = c.get("profiles").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        Some(ChatPolicy {
            profiles,
            turns_per_minute: n("turns_per_minute").unwrap_or(Self::DEFAULT_TURNS_PER_MINUTE),
            input_tokens_per_minute: n("input_tokens_per_minute").unwrap_or(Self::DEFAULT_INPUT_TOKENS_PER_MINUTE),
            output_tokens_per_minute: n("output_tokens_per_minute").unwrap_or(Self::DEFAULT_OUTPUT_TOKENS_PER_MINUTE),
            max_concurrent: n("max_concurrent").unwrap_or(Self::DEFAULT_MAX_CONCURRENT),
            max_history_turns: n("max_history_turns").map_or(Self::DEFAULT_MAX_HISTORY_TURNS, |n| (n as usize).min(MAX_HISTORY_TURNS)),
            cost: n("cost").unwrap_or(Self::DEFAULT_COST),
        })
    }
}

/// A user's share of a key's rate: a quarter, at least one.
fn share(n: u64) -> u64 {
    (n / 4).max(1)
}

/// Who an answer is charged to: a browser key (and a derived key's user) under its policy.
#[derive(Clone, Debug)]
pub struct Payer {
    pub key: String,
    pub user: Option<String>,
    pub policy: ChatPolicy,
}

/// What a step charges: `(bucket, capacity per period, period, amount)`.
type Ask = (String, u64, Duration, u64);

impl Payer {
    fn each(&self, what: &str, per_minute: u64, n: u64) -> Vec<Ask> {
        let mut v = vec![(format!("{what}:k:{}", self.key), per_minute, MINUTE, n)];
        if let Some(u) = &self.user {
            v.push((format!("{what}:u:{}:{u}", self.key), share(per_minute), MINUTE, n));
        }
        v
    }
}

/// A bucket: `capacity` per `period`, refilled continuously.
struct Bucket {
    tokens: f64,
    last: Instant,
    capacity: u64,
    period: Duration,
}

impl Bucket {
    fn refill(&mut self, capacity: u64, period: Duration, now: Instant) {
        self.capacity = capacity;
        self.period = period;
        let rate = capacity as f64 / period.as_secs_f64();
        self.tokens = (self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate).min(capacity as f64);
        self.last = now;
    }

    /// Seconds until it holds `need` (a minute when it never will: past its capacity).
    fn wait(&self, need: f64) -> u64 {
        if need > self.capacity as f64 {
            return self.period.as_secs().clamp(1, 3600);
        }
        let rate = self.capacity as f64 / self.period.as_secs_f64();
        ((need - self.tokens) / rate).ceil().max(1.0) as u64
    }

    fn fill(&self, now: Instant) -> f64 {
        let rate = self.capacity as f64 / self.period.as_secs_f64();
        ((self.tokens + now.saturating_duration_since(self.last).as_secs_f64() * rate) / self.capacity.max(1) as f64).min(1.0)
    }
}

/// One loaded cluster's chat buckets and running answers.
#[derive(Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    running: Mutex<HashMap<String, u64>>,
}

/// An answer's place among those running ([`Limiter::enter`]), given back when dropped (the
/// answer ended, failed, or its client went away).
pub struct Slot {
    limiter: Arc<Limiter>,
    keys: Vec<String>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut r = self.limiter.running.lock();
        for k in &self.keys {
            if let Some(n) = r.get_mut(k) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    r.remove(k);
                }
            }
        }
    }
}

impl Limiter {
    /// Takes every ask or none; `Err`: seconds until the fullest refusal could pass.
    fn take(&self, asks: &[Ask]) -> Result<(), u64> {
        let now = Instant::now();
        let mut b = self.buckets.lock();
        let mut wait = None;
        for (k, cap, period, n) in asks {
            if !b.contains_key(k) && b.len() >= MAX_BUCKETS {
                room(&mut b, now);
            }
            let bucket = b.entry(k.clone()).or_insert_with(|| Bucket { tokens: *cap as f64, last: now, capacity: *cap, period: *period });
            bucket.refill(*cap, *period, now);
            if bucket.tokens < *n as f64 {
                wait = Some(wait.unwrap_or(0).max(bucket.wait(*n as f64)));
            }
        }
        if let Some(w) = wait {
            return Err(w);
        }
        for (k, _, _, n) in asks {
            if let Some(bucket) = b.get_mut(k) {
                bucket.tokens -= *n as f64;
            }
        }
        Ok(())
    }

    /// Gives back `n` of what [`Limiter::take`] took from bucket `k`.
    fn give(&self, k: &str, n: u64) {
        if let Some(b) = self.buckets.lock().get_mut(k) {
            b.tokens = (b.tokens + n as f64).min(b.capacity as f64);
        }
    }

    /// Starts an answer for `payer` if fewer than its key's `max_concurrent` (and one of its
    /// user's) are running.
    pub fn enter(self: &Arc<Self>, payer: &Payer) -> Option<Slot> {
        let mut keys = vec![(format!("k:{}", payer.key), payer.policy.max_concurrent)];
        if let Some(u) = &payer.user {
            keys.push((format!("u:{}:{u}", payer.key), 1));
        }
        let mut r = self.running.lock();
        if keys.iter().any(|(k, max)| r.get(k).copied().unwrap_or(0) >= *max) || r.len() + keys.len() > MAX_BUCKETS {
            return None;
        }
        for (k, _) in &keys {
            *r.entry(k.clone()).or_insert(0) += 1;
        }
        Some(Slot { limiter: self.clone(), keys: keys.into_iter().map(|(k, _)| k).collect() })
    }

    /// Charges one answer to `payer`'s turns.
    pub fn charge_turn(&self, payer: &Payer) -> Result<(), u64> {
        self.take(&payer.each("turns", payer.policy.turns_per_minute, 1))
    }

    /// Charges an answer's tokens before the model is called: `input` estimated for the prompt
    /// and `output` (the cap) to `payer` (when a browser key's), and both to the profile's day.
    pub fn charge_tokens(&self, payer: Option<&Payer>, profile: &str, per_day: Option<u64>, input: u64, output: u64) -> Result<(), u64> {
        let mut asks = Vec::new();
        if let Some(p) = payer {
            asks.extend(p.each("in", p.policy.input_tokens_per_minute, input));
            asks.extend(p.each("out", p.policy.output_tokens_per_minute, output));
        }
        if let Some(d) = per_day {
            asks.push((format!("day:{profile}"), d, DAY, input + output));
        }
        self.take(&asks)
    }

    /// Gives back the output tokens an answer was charged for and didn't use.
    pub fn refund_output(&self, payer: Option<&Payer>, profile: &str, per_day: Option<u64>, unused: u64) {
        if unused == 0 {
            return;
        }
        if let Some(p) = payer {
            for (k, ..) in p.each("out", p.policy.output_tokens_per_minute, unused) {
                self.give(&k, unused);
            }
        }
        if per_day.is_some() {
            self.give(&format!("day:{profile}"), unused);
        }
    }
}

/// Makes room for a new bucket: the fullest go first (a full bucket is as good as none), down to
/// three quarters.
fn room(map: &mut HashMap<String, Bucket>, now: Instant) {
    let mut by_fill: Vec<(f64, String)> = map.iter().map(|(k, b)| (b.fill(now), k.clone())).collect();
    by_fill.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, k) in by_fill.into_iter().take(MAX_BUCKETS / 4) {
        map.remove(&k);
    }
}

/// The 429 for a chat refused by a limit, with `Retry-After`.
pub fn too_many(api: &str, retry_after: u64, why: impl std::fmt::Display) -> EsError {
    EsError::too_many_requests(format!("rejected execution of [{api}]: {why}")).with_header("Retry-After", retry_after.to_string())
}

/// `Err` as [`too_many`].
pub fn refused<T>(api: &str, r: Result<T, u64>, why: impl FnOnce() -> String) -> EsResult<T> {
    r.map_err(|retry| too_many(api, retry, why()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payer(user: Option<&str>) -> Payer {
        let policy = ChatPolicy::of(Some(&crate::obj! { "chat" => crate::obj! { "profiles" => crate::arr!["help"], "turns_per_minute" => 8, "max_concurrent" => 2 } })).unwrap();
        Payer { key: "k".into(), user: user.map(str::to_owned), policy }
    }

    #[test]
    fn turns_run_out_per_key_and_per_user() {
        let l = Arc::new(Limiter::default());
        let alice = payer(Some("alice"));
        // A user gets a quarter of the key's eight turns.
        assert!(l.charge_turn(&alice).is_ok() && l.charge_turn(&alice).is_ok());
        assert!(l.charge_turn(&alice).is_err());
        let key = payer(None);
        for _ in 0..6 {
            assert!(l.charge_turn(&key).is_ok());
        }
        assert!(l.charge_turn(&key).is_err(), "the key's eight are spent");
        // Answers at once: the key's two, its user's one.
        let a = l.enter(&alice).unwrap();
        assert!(l.enter(&alice).is_none());
        let _b = l.enter(&key).unwrap();
        assert!(l.enter(&key).is_none());
        drop(a);
        assert!(l.enter(&key).is_some());
    }

    #[test]
    fn tokens_are_refunded_and_a_profile_has_a_day() {
        let l = Limiter::default();
        let p = payer(None);
        assert!(l.charge_tokens(Some(&p), "help", Some(10_000), 100, 4000).is_ok());
        assert!(l.charge_tokens(Some(&p), "help", Some(10_000), 100, 4000).is_err(), "5,000 output tokens a minute");
        l.refund_output(Some(&p), "help", Some(10_000), 3900);
        assert!(l.charge_tokens(Some(&p), "help", Some(10_000), 100, 4000).is_ok(), "the unused output came back");
        // The day has 10,000 - 4,100 - 4,100 + 3,900 left, for every caller of the profile.
        assert!(l.charge_tokens(None, "help", Some(10_000), 6000, 0).is_err());
        assert!(l.charge_tokens(None, "other", None, 100, 4000).is_ok());
    }
}
