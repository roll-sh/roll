//! Background loop: scale-to-zero, live autoscaling, base backups, eviction to object storage,
//! retention, storage metering and crash recovery.

use crate::compute::{container_name, dir_size};
use crate::control::Control;
use crate::ids::now;
use crate::store::{EndpointState, LocalState};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Compute sizes the autoscaler moves between.
pub const CU_STEPS: [f64; 10] = [0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 16.0, 32.0];

/// Scale up when CPU use passes this share of the limit, or memory in use passes `MEM_HIGH`…
const SCALE_UP_AT: f64 = 0.70;
const MEM_HIGH: f64 = 0.75;
/// …and down after this many consecutive quiet samples.
const SCALE_DOWN_AT: f64 = 0.20;
const SCALE_DOWN_SAMPLES: u32 = 6;

const TICK: Duration = Duration::from_secs(5);
/// In ticks.
const STORAGE_EVERY: u32 = 12;
const MAINTENANCE_EVERY: u32 = 12;
const GC_EVERY: u32 = 720;

pub fn next_step(cu: f64, up: bool, min: f64, max: f64) -> f64 {
    let next = if up {
        CU_STEPS.iter().copied().find(|s| *s > cu + 1e-9).unwrap_or(cu)
    } else {
        CU_STEPS.iter().rev().copied().find(|s| *s < cu - 1e-9).unwrap_or(cu)
    };
    next.clamp(min, max)
}

/// Bring the store in line with what Docker is actually running (after a daemon restart),
/// and re-attach safekeepers to primaries that kept running.
pub async fn recover(control: &Arc<Control>) -> anyhow::Result<()> {
    for ep in control.store.all_endpoints()? {
        let name = container_name(&ep.id);
        if control.driver.is_running(&name).await
            && let Ok(port) = control.driver.port(&name).await
        {
            control.store.set_endpoint_state(&ep.id, EndpointState::Running, Some(port))?;
            control.store.open_segment(&ep.id, &ep.project_id, ep.cu)?;
            control.store.touch_endpoint(&ep.id)?;
            if ep.is_primary()
                && let Some(project) = control.store.project(&ep.project_id)?
            {
                control.start_receiver(&ep, port, &project.password);
            }
        } else {
            control.store.set_endpoint_state(&ep.id, EndpointState::Suspended, None)?;
            control.store.close_segment(&ep.id)?;
        }
    }
    Ok(())
}

pub async fn run(control: Arc<Control>) {
    let mut low: HashMap<String, u32> = HashMap::new();
    let mut tick = 0u32;
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        if let Err(e) = scale(&control, &mut low).await {
            warn!(error = %e, "autoscaling pass failed");
        }
        if tick.is_multiple_of(MAINTENANCE_EVERY) {
            let c = control.clone();
            // Backups and evictions can take a while; don't hold up scaling decisions.
            tokio::spawn(async move {
                if let Err(e) = maintenance(&c, tick.is_multiple_of(GC_EVERY)).await {
                    warn!(error = %e, "maintenance pass failed");
                }
            });
        }
        if tick.is_multiple_of(STORAGE_EVERY)
            && let Err(e) = meter_storage(&control).await
        {
            warn!(error = %e, "storage metering failed");
        }
        tick = tick.wrapping_add(1);
    }
}

pub async fn scale(control: &Arc<Control>, low: &mut HashMap<String, u32>) -> anyhow::Result<()> {
    let endpoints = control.store.all_endpoints()?;
    let running: Vec<_> = endpoints.iter().filter(|e| e.state == EndpointState::Running).collect();

    // Scale to zero.
    for ep in &running {
        let idle_for = now() - ep.last_active;
        if ep.suspend_timeout_s > 0 && control.connections(&ep.id) == 0 && idle_for >= ep.suspend_timeout_s {
            info!(endpoint = %ep.id, idle_for, "suspending idle compute");
            control.suspend(&ep.id).await?;
        }
    }

    // Autoscale on CPU and memory pressure.
    if running.is_empty() {
        return Ok(());
    }
    let usage = control.driver.cpu_usage().await?;
    for ep in control.store.all_endpoints()?.iter().filter(|e| e.state == EndpointState::Running) {
        let Some(cores) = usage.get(&ep.id) else { continue };
        let cpu = cores / ep.cu;
        let mem = match control.driver.memory_in_use(&ep.id).await {
            Some(bytes) => bytes as f64 / (control.config.mem_mb(ep.cu) as f64 * 1024.0 * 1024.0),
            None => 0.0,
        };
        let target = if cpu > SCALE_UP_AT || mem > MEM_HIGH {
            low.remove(&ep.id);
            next_step(ep.cu, true, ep.min_cu, ep.max_cu)
        } else if cpu < SCALE_DOWN_AT && mem < MEM_HIGH / 2.0 {
            let n = low.entry(ep.id.clone()).or_default();
            *n += 1;
            if *n < SCALE_DOWN_SAMPLES {
                continue;
            }
            *n = 0;
            next_step(ep.cu, false, ep.min_cu, ep.max_cu)
        } else {
            low.remove(&ep.id);
            continue;
        };
        if (target - ep.cu).abs() > 1e-9 {
            info!(endpoint = %ep.id, from = ep.cu, to = target, cpu, mem, "autoscaling");
            control.resize(ep, target).await?;
        }
    }
    Ok(())
}

pub async fn maintenance(control: &Arc<Control>, gc: bool) -> anyhow::Result<()> {
    let cfg = &control.config;
    for branch in control.store.all_branches()? {
        let Some(project) = control.store.project(&branch.project_id)? else { continue };
        let endpoints = control.store.branch_endpoints(&branch.id)?;
        let Some(primary) = endpoints.iter().find(|e| e.is_primary()) else { continue };

        // A running primary always has a safekeeper.
        if primary.state == EndpointState::Running
            && let Some(port) = primary.host_port
            && !control.receivers.lock().unwrap().get(&branch.id).is_some_and(|h| !h.is_finished())
        {
            warn!(branch = %branch.id, "safekeeper not running; restarting it");
            control.start_receiver(primary, port, &project.password);
        }

        // Fresh base images keep restores short.
        if primary.state == EndpointState::Running && !control.backups.lock().unwrap().contains(&branch.id) {
            let latest = control.store.bases(&branch.id)?.into_iter().next();
            let position = control.store.wal_position(&branch.id)?;
            let due = match (&latest, position) {
                (Some(b), Some(p)) => {
                    now() - b.created_at >= cfg.base_every_s
                        || p.flush_lsn.saturating_sub(b.end_lsn) >= cfg.base_every_wal_bytes
                        || b.timeline != p.timeline
                }
                _ => false, // the safekeeper triggers the first one
            };
            if due && let Err(e) = control.take_base_backup(&branch.id).await {
                warn!(branch = %branch.id, error = %e, "scheduled base backup failed");
            }
        }

        // Idle replicas just drop their cache; they rebuild from object storage.
        for ep in endpoints.iter().filter(|e| !e.is_primary()) {
            let dir = control.replica_path(ep);
            if ep.state == EndpointState::Suspended && now() - ep.last_active >= cfg.evict_after_s && dir.exists() {
                info!(endpoint = %ep.id, "dropping idle replica cache");
                let _ = control.driver.remove(&ep.id).await;
                let _ = std::fs::remove_dir_all(&dir);
            }
        }

        // Idle branches move to object storage entirely.
        let replicas_idle = endpoints.iter().all(|e| e.is_primary() || e.state == EndpointState::Suspended);
        if branch.local_state == LocalState::Local
            && !branch.pending_recovery
            && primary.state == EndpointState::Suspended
            && replicas_idle
            && now() - primary.last_active >= cfg.evict_after_s
        {
            match control.evict_branch(&branch.id).await {
                Ok(()) => info!(branch = %branch.id, "evicted idle branch"),
                Err(e) => warn!(branch = %branch.id, error = %e, "eviction failed"),
            }
        }

        if gc {
            match control.gc_branch(&branch, project.history_retention_s).await {
                Ok(n) if n > 0 => info!(branch = %branch.id, deleted = n, "retention cleanup"),
                Ok(_) => {}
                Err(e) => warn!(branch = %branch.id, error = %e, "retention cleanup failed"),
            }
        }
    }
    Ok(())
}

pub async fn meter_storage(control: &Arc<Control>) -> anyhow::Result<()> {
    for branch in control.store.all_branches()? {
        let mut local = 0;
        if branch.local_state == LocalState::Local {
            let dir = control.branch_path(&branch);
            local = tokio::task::spawn_blocking(move || dir_size(&dir)).await?;
        }
        control.store.insert_storage_sample(&branch.project_id, &branch.id, "local", local as i64)?;
        let objects = control.object_bytes(&branch.id).await?;
        control.store.insert_storage_sample(&branch.project_id, &branch.id, "s3", objects as i64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::next_step;

    #[test]
    fn steps_clamp_to_range() {
        assert_eq!(next_step(0.25, true, 0.25, 2.0), 0.5);
        assert_eq!(next_step(2.0, true, 0.25, 2.0), 2.0);
        assert_eq!(next_step(1.0, false, 0.5, 4.0), 0.5);
        assert_eq!(next_step(0.5, false, 0.5, 4.0), 0.5);
        assert_eq!(next_step(1.5, true, 0.25, 8.0), 2.0);
    }
}
