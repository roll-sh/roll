//! A preview's output in its project's log drains (roll apps' drains that take previews'): roll
//! apps sends its own `pv-` environments' instances' lines, and this sends the services on roll's
//! runtime, masked as logs are, in the same `pv-` environment. One follower per running service on
//! the node that holds its preview, each starting where the last one stopped (Docker's stamps), so
//! a restart or a redeploy loses no lines and repeats none.

use super::{Engine, preview_key};
use crate::ports::{DrainLine, DrainSource, Drains};
use crate::{Desired, ServiceState};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

/// The most lines a follower sends at once, and the longest a line waits for its batch.
const BATCH: usize = 200;
const LINGER: Duration = Duration::from_secs(1);
/// How often a follower asks whether its project's drains still want previews' output.
const RECHECK: Duration = Duration::from_secs(30);

/// One service's follower: whether it runs, where the last one stopped (the stamp of the last line
/// sent on each stream, stdout then stderr: the two come through pipes of their own, in no order
/// between them), and whether its backend can't be followed (roll apps' own, which drains itself).
#[derive(Default)]
pub(crate) struct Draining {
    alive: AtomicBool,
    last: parking_lot::Mutex<[Option<String>; 2]>,
    elsewhere: AtomicBool,
}

impl Engine {
    /// A follower for each running service of the previews this node holds, while their project
    /// has a drain for previews' output; followers of previews that went are forgotten.
    pub(super) async fn tend_drains(self: &Arc<Self>) {
        let Some(drains) = self.drains.get().cloned() else { return };
        let Ok(rows) = self.store.rows(None) else { return };
        let mut wanted: HashMap<String, bool> = HashMap::new();
        let mut kept: HashSet<String> = HashSet::new();
        for r in rows.iter().filter(|r| r.desired == Desired::Active) {
            if !wanted.contains_key(&r.project_id) {
                wanted.insert(r.project_id.clone(), drains.wanted(&r.project_id, DrainSource::Runtime).await);
            }
            if !wanted[&r.project_id] {
                continue;
            }
            // Only where its services run (its lease's holder).
            if let Some(c) = self.cluster.get()
                && !matches!(c.route_async(&preview_key(&r.id)).await, Ok(roll_cluster::Route::Local(_)))
            {
                continue;
            }
            kept.insert(r.id.clone());
            let Ok(services) = self.store.services(&r.id) else { continue };
            for s in services {
                if !matches!(self.apps.status(&r.id, &s.name), Some(ServiceState::Running)) {
                    continue;
                }
                let d = self.draining.lock().entry((r.id.clone(), s.name.clone())).or_default().clone();
                if d.elsewhere.load(Ordering::Relaxed) || d.alive.swap(true, Ordering::AcqRel) {
                    continue;
                }
                // From the earlier of the two streams' last lines sent (the other's lines up to its
                // own last are skipped as they come again), or from when this process started
                // (what an earlier one ran was its to send).
                let since = d.last.lock().iter().flatten().min().cloned().unwrap_or_else(|| self.started_stamp.clone());
                let Some(rx) = self.apps.drain_from(&r.id, &s.name, &since).await else {
                    d.elsewhere.store(true, Ordering::Relaxed);
                    d.alive.store(false, Ordering::Release);
                    continue;
                };
                let (this, drains, project, id, name) = (self.clone(), drains.clone(), r.project_id.clone(), r.id.clone(), s.name.clone());
                tokio::spawn(async move {
                    this.drain_service(drains.as_ref(), &project, &id, &name, &d, rx).await;
                    d.alive.store(false, Ordering::Release);
                });
            }
        }
        self.draining.lock().retain(|(p, _), d| kept.contains(p) || d.alive.load(Ordering::Acquire));
    }

    /// Follow one service's output to the drains until it ends (the container stopped), the
    /// project's drains stop wanting it, or its masks can't be read (nothing goes unmasked).
    async fn drain_service(&self, drains: &dyn Drains, project: &str, id: &str, service: &str, d: &Draining, mut rx: tokio::sync::mpsc::Receiver<(bool, String)>) {
        let env = crate::branch_name(id);
        // Lines waiting: their stamp, ms, stream and text; and each stream's last stamp taken.
        let mut batch: Vec<(String, i64, bool, String)> = Vec::new();
        let mut taken: [Option<String>; 2] = d.last.lock().clone();
        let mut first = Instant::now();
        let mut checked = Instant::now();
        let mut tick = tokio::time::interval(LINGER / 4);
        loop {
            let ended = tokio::select! {
                got = rx.recv() => match got {
                    Some((stderr, line)) => {
                        let (stamp, text) = line.split_once(' ').unwrap_or((&line, ""));
                        // Docker's `--since` starts at the stamp itself; its fixed width orders as
                        // text. Compared within the line's own stream only: a stderr line can come
                        // before an earlier stdout one.
                        let last = &mut taken[stderr as usize];
                        if last.as_deref().is_some_and(|l| stamp <= l) {
                            continue;
                        }
                        *last = Some(stamp.to_string());
                        let Ok(t) = humantime::parse_rfc3339(stamp) else { continue };
                        let t = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
                        if batch.is_empty() {
                            first = Instant::now();
                        }
                        batch.push((stamp.to_string(), t, stderr, text.to_string()));
                        false
                    }
                    None => true,
                },
                _ = tick.tick() => {
                    if checked.elapsed() >= RECHECK {
                        checked = Instant::now();
                        if !drains.wanted(project, DrainSource::Runtime).await {
                            return;
                        }
                    }
                    false
                }
            };
            if !batch.is_empty() && (ended || batch.len() >= BATCH || first.elapsed() >= LINGER) {
                let lines = std::mem::take(&mut batch);
                let texts: Vec<String> = lines.iter().map(|l| l.3.clone()).collect();
                let masked = match self.masked(id, texts).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(preview = %id, %service, error = %e, "previews: a log drain's lines weren't sent: their masks couldn't be read");
                        return;
                    }
                };
                drains.send(project, &env, service, DrainSource::Runtime, lines.into_iter().zip(masked).map(|((_, t, stderr, _), text)| DrainLine { t, stderr, text }).collect());
                *d.last.lock() = taken.clone();
            }
            if ended {
                return;
            }
        }
    }
}

impl Engine {
    /// A job's output (`release`, `seed`, `teardown`), already masked with what it was given, to
    /// the drains that take previews' builds, once it's done: its tail, masked again as the
    /// preview's logs are, a line each, its name as the service's.
    pub(super) async fn drain_job(&self, project: &str, id: &str, what: &str, output: &str) {
        let Some(drains) = self.drains.get() else { return };
        if !drains.wanted(project, DrainSource::Build).await {
            return;
        }
        let lines: Vec<String> = output.lines().map(str::to_string).collect();
        let Ok(masked) = self.masked(id, lines).await else {
            warn!(preview = %id, job = %what, "previews: a job's output wasn't sent to the log drains: its masks couldn't be read");
            return;
        };
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        drains.send(project, &crate::branch_name(id), what, DrainSource::Build, masked.into_iter().map(|text| DrainLine { t, stderr: false, text }).collect());
    }
}

/// Now as Docker's `--since` takes Unix seconds.
pub(super) fn now_stamp() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
}
