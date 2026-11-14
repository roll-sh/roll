//! Each endpoint's `rate_limit.requests_per_minute`, enforced per node as Elasticsearch's
//! `RequestExecutorService` does: a token bucket holding at most one token, refilled at the
//! endpoint's rate, where a request reserves its token and waits for it. A request that would
//! wait past its timeout gives the token back and times out instead.
//!
//! Buckets are kept per cluster and endpoint on this node. A cluster runs on one node at a time
//! (the fleet's owner), so this is the cluster's limit; a takeover starts with a full bucket.

use crate::error::EsResult;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    last: Instant,
}

static BUCKETS: LazyLock<parking_lot::Mutex<HashMap<String, Bucket>>> = LazyLock::new(Default::default);

/// Buckets idle this long are forgotten (a full bucket is the same as none).
const IDLE: Duration = Duration::from_secs(600);

/// The wait for a request to `key` at `per_minute` requests a minute, reserved now; `None` when
/// it would exceed `limit` (nothing reserved).
fn reserve(key: &str, per_minute: f64, limit: Duration, now: Instant) -> Option<Duration> {
    let per_sec = per_minute / 60.0;
    let mut buckets = BUCKETS.lock();
    if buckets.len() > 10_000 {
        buckets.retain(|_, b| now.duration_since(b.last) < IDLE);
    }
    let b = buckets.entry(key.to_string()).or_insert(Bucket { tokens: 1.0, last: now });
    let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
    b.tokens = (b.tokens + elapsed * per_sec).min(1.0);
    b.last = now;
    let wait = if b.tokens >= 1.0 { 0.0 } else { (1.0 - b.tokens) / per_sec };
    if wait > limit.as_secs_f64() {
        return None;
    }
    b.tokens -= 1.0;
    Some(Duration::from_secs_f64(wait))
}

/// Waits for a request slot of an endpoint (`requests_per_minute` from its settings; none set:
/// no limit).
pub async fn acquire(cluster: &str, inference_id: &str, per_minute: Option<i64>, timeout: Duration) -> EsResult<()> {
    let Some(rpm) = per_minute.filter(|r| *r > 0) else { return Ok(()) };
    let key = format!("{cluster}\u{0}{inference_id}");
    match reserve(&key, rpm as f64, timeout, Instant::now()) {
        None => Err(super::http::timed_out(inference_id)),
        Some(d) if d.is_zero() => Ok(()),
        Some(d) => {
            tokio::time::sleep(d).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_token_refilled_at_the_rate() {
        let t0 = Instant::now();
        let key = "test\u{0}rate-a";
        // 60 a minute: one a second, the first at once.
        assert_eq!(reserve(key, 60.0, Duration::from_secs(5), t0), Some(Duration::ZERO));
        let w = reserve(key, 60.0, Duration::from_secs(5), t0).unwrap();
        assert!((w.as_secs_f64() - 1.0).abs() < 1e-6, "{w:?}");
        let w = reserve(key, 60.0, Duration::from_secs(5), t0).unwrap();
        assert!((w.as_secs_f64() - 2.0).abs() < 1e-6, "{w:?}");
        // Past the timeout: refused, nothing reserved.
        assert_eq!(reserve(key, 60.0, Duration::from_millis(500), t0), None);
        let w = reserve(key, 60.0, Duration::from_secs(5), t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(w, Duration::ZERO);
    }
}
