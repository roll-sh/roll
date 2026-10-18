//! Metering and cost: messages, GB-months stored and connection-hours, priced from what they
//! cost to run on AWS, and compared with Amazon MQ and CloudAMQP for the same workload.

use crate::sampler::Delta;
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

const DAY_S: u64 = 86_400;
const HOURS_PER_MONTH: f64 = 730.0;
const BYTE_SECONDS_PER_GB_MONTH: f64 = 1e9 * HOURS_PER_MONTH * 3600.0;

/// Prices, derived from AWS list prices (us-east-1) with headroom:
///
/// - **Messages.** A Graviton `c7g.large` broker ($0.0725/h on demand, about $0.03/h as Spot,
///   which is what the fleet runs above its on-demand base) sustains ~30k message operations per
///   second at 60% CPU: under $0.0007 per million. Replication dominates: every published
///   message goes to two more AZs at $0.01/GB each way, $0.04 per million 1 KB messages as they
///   are, about a third of that for typical (JSON, text) payloads once batches are
///   zstd-compressed. Queues live in their clients' AZ, so client traffic adds no cross-AZ
///   bytes. Priced per operation (publish or deliver), so publish + consume of one message costs
///   2 operations: $0.03 per million covers incompressible payloads too.
/// - **Storage.** Sealed segments live in S3 Standard ($0.023/GB-month); keepers hold only
///   minutes of hot data on gp3.
/// - **Connections.** An idle AMQP connection takes ~30 KB; a `c7g.large` holds ~50k at half
///   its memory: $0.0000015 per connection-hour.
#[derive(Clone, Debug, Serialize)]
pub struct Pricing {
    pub usd_per_million_messages: f64,
    pub usd_per_gb_month: f64,
    pub usd_per_connection_hour: f64,
    pub basis: String,
}

impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            usd_per_million_messages: 0.03,
            usd_per_gb_month: 0.05,
            usd_per_connection_hour: 0.000005,
            basis: "AWS us-east-1: Graviton brokers (an on-demand base, Spot above it), 3-AZ replication at $0.01/GB (zstd-compressed, \
                    queues in their clients' AZ), S3 Standard at $0.023/GB-month; \
                    priced per message operation (publish or deliver), GB-month stored and connection-hour, nothing while idle"
                .into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Acc {
    published: u64,
    delivered: u64,
    byte_seconds: f64,
    conn_seconds: f64,
    stored_bytes: u64,
}

pub struct Meter {
    db: Mutex<Connection>,
    pending: Mutex<HashMap<(u64, String), Acc>>,
    pub pricing: Pricing,
}

impl Meter {
    /// Open (or create) the metering database; `None` keeps it in memory.
    pub fn open(path: Option<&Path>) -> anyhow::Result<Self> {
        let db = match path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                Connection::open(p)?
            }
            None => Connection::open_in_memory()?,
        };
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS usage (
               day INTEGER NOT NULL, vhost TEXT NOT NULL,
               published INTEGER NOT NULL DEFAULT 0, delivered INTEGER NOT NULL DEFAULT 0,
               byte_seconds REAL NOT NULL DEFAULT 0, conn_seconds REAL NOT NULL DEFAULT 0,
               stored_bytes INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (day, vhost));",
        )?;
        Ok(Meter { db: Mutex::new(db), pending: Mutex::new(HashMap::new()), pricing: Pricing::default() })
    }

    /// Add one sample covering `dt_s` seconds.
    pub fn record(&self, now_s: u64, dt_s: f64, delta: &Delta) {
        let day = now_s / DAY_S * DAY_S;
        // Storage accrues over the whole interval, which may cover several days when the
        // sampler was parked (quiet broker): each day gets its share.
        let mut shares = Vec::new();
        let mut end = now_s as f64;
        let start = end - dt_s.max(0.0);
        while end > start {
            let d = ((end - 1e-9).max(0.0) as u64) / DAY_S * DAY_S;
            let from = start.max(d as f64);
            shares.push((d, end - from));
            end = from;
        }
        let mut p = self.pending.lock();
        for (vhost, (published, delivered, bytes, conn_s)) in &delta.vhosts {
            let a = p.entry((day, vhost.clone())).or_default();
            a.published += published;
            a.delivered += delivered;
            a.conn_seconds += conn_s;
            a.stored_bytes = *bytes;
            for &(d, secs) in &shares {
                let a = p.entry((d, vhost.clone())).or_default();
                a.byte_seconds += *bytes as f64 * secs;
                a.stored_bytes = *bytes;
            }
        }
    }

    /// Write pending counters to the database.
    pub fn flush(&self) -> anyhow::Result<()> {
        let pending = std::mem::take(&mut *self.pending.lock());
        if pending.is_empty() {
            return Ok(());
        }
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        for ((day, vhost), a) in pending {
            tx.execute(
                "INSERT INTO usage (day, vhost, published, delivered, byte_seconds, conn_seconds, stored_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (day, vhost) DO UPDATE SET
                   published = published + excluded.published, delivered = delivered + excluded.delivered,
                   byte_seconds = byte_seconds + excluded.byte_seconds, conn_seconds = conn_seconds + excluded.conn_seconds,
                   stored_bytes = excluded.stored_bytes",
                params![day as i64, vhost, a.published as i64, a.delivered as i64, a.byte_seconds, a.conn_seconds, a.stored_bytes as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Usage, cost and comparison for the last `days` days (optionally one vhost).
    pub fn report(&self, days: u64, vhost: Option<&str>, current_bytes: u64, live_msgs_per_s: f64, live_connections: u64) -> anyhow::Result<Value> {
        self.flush()?;
        let now = mq_core::now_ms() / 1000;
        let end_day = now / DAY_S * DAY_S;
        let start = end_day.saturating_sub((days.max(1) - 1) * DAY_S);
        let rows: Vec<(u64, u64, u64, f64, f64, u64)> = {
            let db = self.db.lock();
            let mut st = db.prepare(
                "SELECT day, SUM(published), SUM(delivered), SUM(byte_seconds), SUM(conn_seconds), SUM(stored_bytes)
                 FROM usage WHERE day >= ?1 AND (?2 IS NULL OR vhost = ?2) GROUP BY day ORDER BY day",
            )?;
            st.query_map(params![start as i64, vhost], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? as u64))
            })?
            .collect::<Result<_, _>>()?
        };
        let p = &self.pricing;
        let msg_usd = |n: u64| n as f64 / 1e6 * p.usd_per_million_messages;
        let (mut published, mut delivered, mut byte_s, mut conn_s) = (0u64, 0u64, 0f64, 0f64);
        let mut peak_daily_ops = 0u64;
        let daily: Vec<Value> = rows
            .iter()
            .map(|&(day, pu, de, bs, cs, sb)| {
                published += pu;
                delivered += de;
                byte_s += bs;
                conn_s += cs;
                peak_daily_ops = peak_daily_ops.max(pu + de);
                let usd = msg_usd(pu + de) + bs / BYTE_SECONDS_PER_GB_MONTH * p.usd_per_gb_month + cs / 3600.0 * p.usd_per_connection_hour;
                json!({ "day": day, "published": pu, "delivered": de, "stored_bytes": sb, "connection_hours": cs / 3600.0, "usd": round(usd) })
            })
            .collect();
        let gb_months = byte_s / BYTE_SECONDS_PER_GB_MONTH;
        let conn_hours = conn_s / 3600.0;
        let messages_usd = msg_usd(published + delivered);
        let storage_usd = gb_months * p.usd_per_gb_month;
        let conn_usd = conn_hours * p.usd_per_connection_hour;
        let total = messages_usd + storage_usd + conn_usd;
        let first_day = rows.first().map(|r| r.0).unwrap_or(end_day);
        let elapsed_days = ((now - first_day) as f64 / DAY_S as f64).max(1.0 / 24.0);
        let projected = total / elapsed_days * 30.0;

        // The workload to price elsewhere: peak hour ≈ 3× the busiest day's average, or what's
        // happening right now if that's more.
        let avg_conns = if elapsed_days > 0.0 { conn_hours / (elapsed_days * 24.0) } else { 0.0 };
        let workload = Workload {
            msgs_per_s: (peak_daily_ops as f64 / DAY_S as f64 * 3.0).max(live_msgs_per_s),
            connections: avg_conns.max(live_connections as f64),
            stored_gb: current_bytes as f64 / 1e9,
            monthly_messages: (published + delivered) as f64 / elapsed_days * 30.0,
        };
        let roll_monthly = projected.max(0.0);

        Ok(json!({
            "period": { "start": start, "end": now, "days": days },
            "messages": { "published": published, "delivered": delivered, "usd": round(messages_usd) },
            "storage": { "current_bytes": current_bytes, "gb_months": gb_months, "usd": round(storage_usd) },
            "connections": { "hours": conn_hours, "usd": round(conn_usd) },
            "total_usd": round(total),
            "projected_month_usd": round(roll_monthly),
            "pricing": p,
            "workload": { "peak_msgs_per_s": workload.msgs_per_s, "connections": workload.connections, "stored_gb": workload.stored_gb },
            "compare": compare(&workload),
            "daily": daily,
        }))
    }
}

fn round(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// A workload described the way other providers size plans.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub msgs_per_s: f64,
    pub connections: f64,
    pub stored_gb: f64,
    pub monthly_messages: f64,
}

/// Amazon MQ for RabbitMQ, 3-node cluster (the highly available deployment), us-east-1.
/// Hourly prices per broker instance; capacities are conservative estimates for persistent
/// 1 KB messages on quorum queues.
const AMAZON_MQ: &[(&str, f64, f64, f64)] = &[
    // (instance, $/h per node, msg/s, connections)
    ("mq.m7g.medium", 0.117, 4_000.0, 1_000.0),
    ("mq.m7g.large", 0.2734, 12_000.0, 5_000.0),
    ("mq.m7g.xlarge", 0.547, 25_000.0, 10_000.0),
    ("mq.m7g.2xlarge", 1.094, 50_000.0, 20_000.0),
    ("mq.m7g.4xlarge", 2.188, 90_000.0, 40_000.0),
    ("mq.m7g.8xlarge", 4.376, 160_000.0, 80_000.0),
    ("mq.m7g.12xlarge", 6.564, 220_000.0, 120_000.0),
    ("mq.m7g.16xlarge", 8.752, 280_000.0, 160_000.0),
];
const AMAZON_MQ_STORAGE_GB_MONTH: f64 = 0.10;

/// CloudAMQP dedicated 3-node cluster plans (monthly, plan throughput, connections).
const CLOUDAMQP_CLUSTER: &[(&str, f64, f64, f64)] = &[
    ("Big Bunny ×3", 297.0, 1_000.0, 9_000.0),
    ("Happy Hare ×3", 597.0, 8_000.0, 12_000.0),
    ("Roaring Rabbit ×3", 897.0, 10_000.0, 15_000.0),
    ("Power Panda ×3", 1_497.0, 25_000.0, 40_000.0),
    ("Awesome Ape ×3", 2_997.0, 50_000.0, 60_000.0),
    ("Heavy Hippo ×3", 5_997.0, 100_000.0, 80_000.0),
    ("Loud Lion ×3", 10_497.0, 200_000.0, 140_000.0),
];

/// What the same workload costs per month on Amazon MQ and CloudAMQP.
pub fn compare(w: &Workload) -> Vec<Value> {
    let mut out = Vec::new();
    let fit = AMAZON_MQ.iter().find(|(_, _, m, c)| *m >= w.msgs_per_s && *c >= w.connections).unwrap_or(AMAZON_MQ.last().unwrap_or(&("", 0.0, 0.0, 0.0)));
    let amq = fit.1 * 3.0 * HOURS_PER_MONTH + w.stored_gb * AMAZON_MQ_STORAGE_GB_MONTH;
    out.push(json!({
        "name": "Amazon MQ",
        "plan": format!("{} · 3-node cluster", fit.0),
        "monthly_usd": round(amq),
        "note": format!("${:.4}/h per node × 3, billed while idle, plus $0.10/GB-month storage", fit.1),
    }));
    if w.monthly_messages <= 10_000_000.0 && w.connections <= 100.0 {
        out.push(json!({
            "name": "CloudAMQP",
            "plan": "Tough Tiger (shared)",
            "monthly_usd": 19.0,
            "note": "shared single node, 10M messages/month, 100 connections, no high availability",
        }));
    }
    let fit = CLOUDAMQP_CLUSTER.iter().find(|(_, _, m, c)| *m >= w.msgs_per_s && *c >= w.connections).unwrap_or(CLOUDAMQP_CLUSTER.last().unwrap_or(&("", 0.0, 0.0, 0.0)));
    out.push(json!({
        "name": "CloudAMQP",
        "plan": format!("{} · 3-node cluster", fit.0),
        "monthly_usd": fit.1,
        "note": format!("dedicated, up to {} msg/s and {} connections, billed while idle", fit.2, fit.3),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meters_and_prices() {
        let m = Meter::open(None).unwrap();
        let mut d = Delta::default();
        d.vhosts.insert("/".into(), (1_000_000, 1_000_000, 2_000_000_000, 3600.0 * 10.0));
        let now = mq_core::now_ms() / 1000;
        m.record(now, 3600.0, &d);
        let r = m.report(30, None, 2_000_000_000, 0.0, 10).unwrap();
        assert_eq!(r["messages"]["published"], 1_000_000);
        assert_eq!(r["messages"]["usd"], 0.06);
        assert!((r["connections"]["hours"].as_f64().unwrap() - 10.0).abs() < 1e-9);
        let compare = r["compare"].as_array().unwrap();
        assert!(compare.iter().any(|c| c["name"] == "Amazon MQ"));
        assert!(compare.iter().all(|c| c["monthly_usd"].as_f64().unwrap() > r["projected_month_usd"].as_f64().unwrap()));
        assert_eq!(r["daily"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_long_gap_spreads_storage_over_its_days() {
        let m = Meter::open(None).unwrap();
        let mut d = Delta::default();
        d.vhosts.insert("/".into(), (5, 0, 1_000, 0.0));
        // Noon on day 3, after a gap that began at midnight of day 1: 2.5 days.
        let now = 3 * DAY_S + DAY_S / 2;
        m.record(now, 2.5 * DAY_S as f64, &d);
        let p = m.pending.lock();
        let secs = |day: u64| p.get(&(day * DAY_S, "/".to_string())).map(|a| a.byte_seconds / 1_000.0);
        assert_eq!((secs(1), secs(2), secs(3)), (Some(DAY_S as f64), Some(DAY_S as f64), Some(DAY_S as f64 / 2.0)));
        assert_eq!(secs(0), None);
        assert_eq!(p[&(3 * DAY_S, "/".to_string())].published, 5, "activity goes to the day it was seen");
        assert_eq!(p[&(DAY_S, "/".to_string())].published, 0);
    }

    #[test]
    fn compare_picks_sizes() {
        let w = Workload { msgs_per_s: 30_000.0, connections: 500.0, stored_gb: 10.0, monthly_messages: 1e10 };
        let c = compare(&w);
        assert!(c[0]["plan"].as_str().unwrap().starts_with("mq.m7g.2xlarge"));
        assert!(c.last().unwrap()["plan"].as_str().unwrap().starts_with("Awesome Ape"));
    }
}
