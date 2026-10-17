//! Flow control shared by every worker, built on compare-and-set records:
//!
//! - **Slots** cap how many deliveries run at once for a key. Holders expire with their lease,
//!   so a crashed worker never leaks capacity. Messages turned away are remembered and woken
//!   when a slot frees, instead of polling.
//! - **Rates** use GCRA (a token bucket kept as one timestamp): `limit` per `period`, with a
//!   burst of up to `limit`.

use crate::engine::Tasks;
use crate::model::{Ms, Rate, now_ms};
use crate::store::Expect;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
struct Slots {
    /// holder → lease expiry
    holders: BTreeMap<String, Ms>,
    /// Messages to wake when a slot frees, oldest first.
    #[serde(default)]
    waiting: Vec<String>,
}

const MAX_WAITING: usize = 1000;
const CAS_TRIES: usize = 50;

pub enum Acquire {
    Granted,
    /// Full; the holder was queued for a wake-up.
    Full,
}

fn slot_key(name: &str) -> String {
    format!("slot:{name}")
}

async fn load(t: &Tasks, key: &str) -> anyhow::Result<(Slots, Expect)> {
    Ok(match t.store.kv_get(key).await? {
        Some(v) => (serde_json::from_value(v.value).unwrap_or_default(), Expect::Version(v.version)),
        None => (Slots::default(), Expect::Absent),
    })
}

pub async fn acquire(t: &Tasks, name: &str, limit: u32, holder: &str, until: Ms) -> anyhow::Result<Acquire> {
    let key = slot_key(name);
    for _ in 0..CAS_TRIES {
        let now = now_ms();
        let (mut s, expect) = load(t, &key).await?;
        s.holders.retain(|_, exp| *exp > now);
        let granted = s.holders.contains_key(holder) || s.holders.len() < limit as usize;
        if granted {
            s.holders.insert(holder.into(), until);
            s.waiting.retain(|w| w != holder);
        } else if !s.waiting.iter().any(|w| w == holder) {
            if s.waiting.len() >= MAX_WAITING {
                // Enough waiters to keep the slots busy; this one retries on its own.
                return Ok(Acquire::Full);
            }
            s.waiting.push(holder.into());
        } else {
            return Ok(Acquire::Full);
        }
        let expiry = s.holders.values().max().copied().map(|m| m + 86_400_000);
        if t.store.kv_put(&key, &serde_json::to_value(&s)?, expect, expiry).await? {
            return Ok(if granted { Acquire::Granted } else { Acquire::Full });
        }
    }
    anyhow::bail!("slot {name} is contended")
}

/// Free a slot; returns a waiting message to wake, if any.
pub async fn release(t: &Tasks, name: &str, holder: &str) -> anyhow::Result<Option<String>> {
    let key = slot_key(name);
    for _ in 0..CAS_TRIES {
        let Some(v) = t.store.kv_get(&key).await? else { return Ok(None) };
        let mut s: Slots = serde_json::from_value(v.value).unwrap_or_default();
        let now = now_ms();
        s.holders.retain(|h, exp| *exp > now && h != holder);
        let wake = if s.waiting.is_empty() { None } else { Some(s.waiting.remove(0)) };
        let expiry = s.holders.values().max().copied().map(|m| m + 86_400_000);
        let written = if s.holders.is_empty() && s.waiting.is_empty() {
            t.store.kv_delete(&key, Expect::Version(v.version)).await?
        } else {
            t.store.kv_put(&key, &serde_json::to_value(&s)?, Expect::Version(v.version), expiry).await?
        };
        if written {
            return Ok(wake);
        }
    }
    anyhow::bail!("slot {name} is contended")
}

/// Push a holder's expiry out (long deliveries).
pub async fn extend(t: &Tasks, name: &str, holder: &str, until: Ms) -> anyhow::Result<()> {
    let key = slot_key(name);
    for _ in 0..CAS_TRIES {
        let Some(v) = t.store.kv_get(&key).await? else { return Ok(()) };
        let mut s: Slots = serde_json::from_value(v.value).unwrap_or_default();
        match s.holders.get_mut(holder) {
            Some(exp) => *exp = until,
            None => return Ok(()),
        }
        let expiry = s.holders.values().max().copied().map(|m| m + 86_400_000);
        if t.store.kv_put(&key, &serde_json::to_value(&s)?, Expect::Version(v.version), expiry).await? {
            return Ok(());
        }
    }
    Ok(())
}

pub async fn slot_holders(t: &Tasks, name: &str) -> anyhow::Result<usize> {
    let now = now_ms();
    Ok(match t.store.kv_get(&slot_key(name)).await? {
        Some(v) => {
            let s: Slots = serde_json::from_value(v.value).unwrap_or_default();
            s.holders.values().filter(|exp| **exp > now).count()
        }
        None => 0,
    })
}

#[derive(Serialize, Deserialize)]
struct Gcra {
    tat: Ms,
}

/// Take one unit of `rate` for `name`. `Ok(0)` means go; otherwise wait that many ms.
pub async fn take(t: &Tasks, name: &str, rate: Rate) -> anyhow::Result<Ms> {
    let key = format!("rate:{name}");
    let interval = (rate.period_ms / rate.limit.max(1) as i64).max(1);
    let tolerance = rate.period_ms - interval;
    for _ in 0..CAS_TRIES {
        let now = now_ms();
        let (tat, expect) = match t.store.kv_get(&key).await? {
            Some(v) => (serde_json::from_value::<Gcra>(v.value).map(|g| g.tat).unwrap_or(now), Expect::Version(v.version)),
            None => (now, Expect::Absent),
        };
        let tat = tat.max(now);
        if tat - now > tolerance {
            return Ok(tat - now - tolerance);
        }
        let next = tat + interval;
        if t.store.kv_put(&key, &serde_json::to_value(Gcra { tat: next })?, expect, Some(next + rate.period_ms)).await? {
            return Ok(0);
        }
    }
    anyhow::bail!("rate {name} is contended")
}

/// When a rate next has room, without taking from it.
pub async fn next_free(t: &Tasks, name: &str, rate: Rate) -> anyhow::Result<Ms> {
    let now = now_ms();
    let interval = (rate.period_ms / rate.limit.max(1) as i64).max(1);
    let tolerance = rate.period_ms - interval;
    Ok(match t.store.kv_get(&format!("rate:{name}")).await? {
        Some(v) => {
            let tat = serde_json::from_value::<Gcra>(v.value).map(|g| g.tat).unwrap_or(now);
            (tat - tolerance).max(now)
        }
        None => now,
    })
}
