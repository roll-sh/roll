//! Right-sizing advice (Cloud Run's recommender, Render's memory warnings; docs/apps/README.md,
//! "Advice"): what an environment's last week says about its settings, each with the setting
//! that acts on it. roll meters what instances use, not their limits, so a limit set high costs
//! nothing: the advice is about what fails (memory near its limit is an OOM kill on the way, CPU
//! at its limit is a slow app, requests turned away at `max_instances`) and what costs while it
//! waits (a `min_instances` kept warm through idle hours).

use crate::{Apps, Result};
use serde_json::{Value, json};
use std::sync::Arc;

/// The window read: a week of minutes (what's kept by the minute).
const DAYS: i64 = 7;

impl Apps {
    /// `GET …/environments/{env}/advice`: `{days, advice: [{kind, message, set?, saving_usd_month?}]}`,
    /// `set` the body of `PUT …/environments/{env}` that acts on it.
    pub async fn advice(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let run = svc.for_env(&row).map_or(svc.run.clone(), |s| s.run);
        let since = code_sql::now_ms() / 60_000 - DAYS * 24 * 60;
        let (sid, e) = (service.to_string(), env.to_string());
        let rows = self.db(move |s| s.minutes(&sid, &e, since)).await?;
        let up: Vec<_> = rows.iter().filter(|r| r.instances > 0).collect();
        let mut out = Vec::new();
        if up.len() < 60 {
            return Ok(json!({ "days": DAYS, "minutes": up.len(), "advice": out }));
        }
        let limit = run.memory_mb as f64 * 1_048_576.0;
        // Memory: its busiest minute's share of the limit, per instance.
        let mem_peak = up.iter().map(|r| r.memory_bytes as f64 / r.instances as f64 / limit).fold(0.0, f64::max);
        if mem_peak >= 0.85 && run.memory_mb < 16384 {
            let to = ((run.memory_mb as f64 * 1.5 / 256.0).ceil() as u32 * 256).min(16384);
            out.push(json!({ "kind": "memory", "message": format!("its instances reached {:.0}% of their {} MB: one more spike is an OOM kill", mem_peak * 100.0, run.memory_mb), "set": { "run": { "memory_mb": to } } }));
        }
        // CPU: its 95th busiest minute's share of what it has.
        let mut cpu: Vec<f64> = up.iter().filter(|_| run.cpu > 0.0).map(|r| r.cpu / (r.instances as f64 * run.cpu * 60.0)).collect();
        cpu.sort_by(|a, b| a.total_cmp(b));
        let cpu_p95 = cpu.get(cpu.len() * 95 / 100).copied().unwrap_or(0.0);
        if cpu_p95 >= 0.9 && run.cpu < 8.0 {
            let to = (run.cpu * 2.0).min(8.0);
            out.push(json!({ "kind": "cpu", "message": format!("its instances used {:.0}% of their {} vCPU in their busiest minutes: requests wait on it", cpu_p95 * 100.0, run.cpu), "set": { "run": { "cpu": to } } }));
        }
        // Requests turned away with every instance busy at its ceiling.
        let turned: i64 = rows.iter().map(|r| r.over_capacity).sum();
        if turned > 0 && run.max_instances < 100 {
            let to = (run.max_instances + run.max_instances.div_ceil(2)).min(100);
            out.push(json!({ "kind": "max_instances", "message": format!("{turned} requests found all {} instances busy this week", run.max_instances), "set": { "run": { "max_instances": to } } }));
        }
        // Kept warm through idle hours: what those minutes cost, as a month.
        if run.min_instances > 0 {
            let idle: Vec<_> = up.iter().filter(|r| r.requests == 0).collect();
            if idle.len() * 10 >= up.len() * 8 {
                let p = self.cfg.prices;
                let cost: f64 = idle.iter().map(|r| r.cpu * p.cpu_second + r.memory_bytes as f64 / 1e9 * 60.0 * p.gb_second).sum();
                let month = cost / (up.len() as f64 / 60.0 / 24.0).max(1.0 / 24.0) * 30.0;
                out.push(json!({
                    "kind": "min_instances",
                    "message": format!("{} instance(s) kept warm answered nothing in {:.0}% of its minutes: asleep, it wakes on the first request", run.min_instances, idle.len() as f64 * 100.0 / up.len() as f64),
                    "set": { "run": { "min_instances": 0 } },
                    "saving_usd_month": (month * 100.0).round() / 100.0,
                }));
            }
        }
        Ok(json!({ "days": DAYS, "minutes": up.len(), "advice": out }))
    }
}
