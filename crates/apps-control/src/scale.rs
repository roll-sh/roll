//! Workers that follow their work (docs/apps/README.md, "Workers that follow their work"): a
//! worker with `run.scale` runs one instance per `per_instance` messages its stream consumer
//! group is behind, within `min_instances`–`max_instances`. Every ten seconds the service's
//! owner reads the lag (only for a stream on the service's own project: another tenant's stream
//! is never read) and sets the worker's target; the reaper starts instances at once and stops
//! one only after fewer were wanted for `scale_down_after`.
//!
//! With `metric` instead, the worker follows a gauge of its own: the newest point of each of its
//! series from the last minute (its Prometheus endpoint's, [`crate::prom`], or its SDK's OTLP
//! metrics), the largest of them or their sum. KEDA's Prometheus scaler, Cloud Run's custom
//! metrics. A counter or histogram isn't a level, so it's said and not followed.

use crate::Apps;
use crate::model::RunType;
use std::sync::Arc;

impl Apps {
    /// One pass over the scaling workers this node owns.
    pub async fn scale_workers(self: &Arc<Self>) {
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(tpl) = l.template() else { continue };
            let Some(sc) = tpl.run.scale.clone() else {
                // Not following anything (any more): no stale target for when it does again.
                l.set_wanted(0, None);
                continue;
            };
            let run = &tpl.run;
            if run.kind != RunType::Worker {
                continue;
            }
            if !sc.metric.is_empty() {
                match self.gauge(&l.service, &l.env, &sc.metric, sc.sum).await {
                    Ok(v) => {
                        l.note_scale("");
                        let level = v.max(0.0).ceil() as u64;
                        l.set_wanted(sc.instances(level, run.min_instances, run.max_instances), Some(level));
                    }
                    Err(why) => l.note_scale(&format!("scale: {why}")),
                }
                continue;
            }
            let sid = l.service.clone();
            let Ok(svc) = self.db(move |s| s.service(&sid)).await else { continue };
            // A canvas proves ownership only where the operator placed it (as roll secrets
            // refuses links on a person's project).
            if let Err(why) = self.stream_followable(&svc.project, &sc.stream) {
                l.note_scale(&format!("scaling on a backlog is off: {why}"));
                l.set_wanted(0, None);
                continue;
            }
            match self.deps.resources.backlog(&sc.stream, &sc.group).await {
                Ok(lag) => {
                    l.note_scale("");
                    l.set_wanted(sc.instances(lag, run.min_instances, run.max_instances), Some(lag));
                }
                Err(e) => l.note_scale(&format!("scale: reading {}'s lag: {e:#}", sc.group)),
            }
        }
    }
}

impl Apps {
    /// A gauge of the environment's own, now: each series' newest point of the last minute, the
    /// largest or the sum. An error says why there's nothing to follow.
    pub(crate) async fn gauge(self: &Arc<Self>, service: &str, env: &str, name: &str, sum: bool) -> Result<f64, String> {
        let since = code_sql::now_ms() - 60_000;
        let needle = format!("\"name\":{}", serde_json::to_string(name).unwrap_or_default());
        let (lines, _) = self.logs.read(&crate::traces::metric_key(service, env), |l| l.text.contains(&needle), Some(since), None, 20_000).await;
        let mut newest: std::collections::HashMap<String, crate::otlp::Point> = std::collections::HashMap::new();
        for l in &lines {
            let Ok(p) = serde_json::from_str::<crate::otlp::Point>(&l.text) else { continue };
            if p.name != name {
                continue;
            }
            if p.kind != "gauge" {
                return Err(format!("{name} is a {}, not a gauge: scale on a level (a queue's depth, jobs in flight)", if p.counter { "counter" } else { p.kind.as_str() }));
            }
            let key = p.attrs.iter().map(|(k, v)| format!("{k}={v}|")).collect::<String>();
            if newest.get(&key).is_none_or(|q| p.time >= q.time) {
                newest.insert(key, p);
            }
        }
        if newest.is_empty() {
            return Err(format!("no {name} in the last minute (its metrics endpoint, or traces on for an SDK's)"));
        }
        let values = newest.values().map(|p| p.value).filter(|v| v.is_finite());
        Ok(if sum { values.sum() } else { values.fold(0.0, f64::max) })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_backlog_wants_an_instance_per_share_within_the_bounds() {
        let sc = crate::model::Scale { stream: "str_a".into(), group: "g".into(), metric: String::new(), sum: false, per_instance: 100 };
        assert_eq!(sc.instances(0, 1, 5), 1, "the floor");
        assert_eq!(sc.instances(250, 1, 5), 3);
        assert_eq!(sc.instances(10_000, 1, 5), 5, "the ceiling");
    }
}
