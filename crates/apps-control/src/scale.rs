//! Workers that follow their work (docs/apps/README.md, "Workers that follow their work"): a
//! worker with `run.scale` runs one instance per `per_instance` messages its stream consumer
//! group is behind, within `min_instances`–`max_instances`. Every ten seconds the service's
//! owner reads the lag (only for a stream on the service's own project: another tenant's stream
//! is never read) and sets the worker's target; the reaper starts instances at once and stops
//! one only after fewer were wanted for `scale_down_after`.

use crate::Apps;
use crate::model::RunType;
use std::sync::Arc;

impl Apps {
    /// One pass over the scaling workers this node owns.
    pub async fn scale_workers(self: &Arc<Self>) {
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(tpl) = l.template() else { continue };
            let (run, Some(sc)) = (&tpl.run, tpl.run.scale.clone()) else { continue };
            if run.kind != RunType::Worker {
                continue;
            }
            let sid = l.service.clone();
            let Ok(svc) = self.db(move |s| s.service(&sid)).await else { continue };
            let placed = self.deps.workspace.as_ref().is_some_and(|ws| {
                ws.get(roll_workspace::Scope::All, &svc.project).is_ok_and(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Stream && r.r#ref == sc.stream))
            });
            if !placed {
                l.log.stream(crate::logs::Stream::System).line(format!("scale: stream {} isn't in this service's project; scaling on it is off", sc.stream));
                l.set_wanted(0, None);
                continue;
            }
            match self.deps.resources.backlog(&sc.stream, &sc.group).await {
                Ok(lag) => l.set_wanted(sc.instances(lag, run.min_instances, run.max_instances), Some(lag)),
                Err(e) => l.log.stream(crate::logs::Stream::System).line(format!("scale: reading {}'s lag: {e:#}", sc.group)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_backlog_wants_an_instance_per_share_within_the_bounds() {
        let sc = crate::model::Scale { stream: "str_a".into(), group: "g".into(), per_instance: 100 };
        assert_eq!(sc.instances(0, 1, 5), 1, "the floor");
        assert_eq!(sc.instances(250, 1, 5), 3);
        assert_eq!(sc.instances(10_000, 1, 5), 5, "the ceiling");
    }
}
