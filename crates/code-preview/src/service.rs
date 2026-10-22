//! What triggers checks and previews.
//!
//! - **Changes**: every new head of an open change runs the repo's checks against that head.
//!   Results land on the change, and a result for a head the change has moved past is dropped.
//!   With `[preview] auto`, the change also gets a preview, which sleeps until visited.
//! - **Post-merge**: landings on a repo's default branch run its checks on the new head. While a
//!   run is going, later landings coalesce into one more run on the latest head, so a repo
//!   landing thousands of changes an hour runs checks back to back, not thousands of times. A
//!   failure opens one `broken-main` work item (P0) for an agent to pick up. The item closes
//!   itself when checks pass again, unless an agent already claimed it.
//! - A change that lands or closes loses its preview (and its data branches).

use crate::config::{FILE, RollToml};
use crate::docker;
use futures::future::join_all;
use code_control::Code;
use code_control::broken::Source;
use code_control::Runner;
use code_control::model::{Change, ChangeState, Check, CheckStatus, RepoRow};
use code_store::{ChangesetId, Landed};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

pub use code_control::broken::BROKEN_MAIN;

pub struct Service {
    code: Arc<Code>,
    runner: Arc<dyn Runner>,
    /// The head each open change last had checks started for.
    started: Mutex<HashMap<String, String>>,
    /// Post-merge runs per repo: whether one is going, and the newest head waiting for the next.
    main: Mutex<HashMap<String, MainRun>>,
}

#[derive(Default)]
struct MainRun {
    running: bool,
    pending: Option<ChangesetId>,
}

impl Service {
    pub fn new(code: Arc<Code>, runner: Arc<dyn Runner>) -> Arc<Service> {
        Arc::new(Service { code, runner, started: Mutex::new(HashMap::new()), main: Mutex::new(HashMap::new()) })
    }

    fn work_dir(&self) -> PathBuf {
        self.code.config.home.join("code").join("checks")
    }

    /// Follow changes and landings until the control plane goes away.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        // Subscribe before anything else, so nothing that happens while we clean up is missed.
        let mut changes = this.code.subscribe();
        let mut landed = this.code.store.subscribe_all();
        let dir = self.work_dir();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            docker::remove_leftovers("check").await;
            this.recover();
            loop {
                tokio::select! {
                    ev = changes.recv() => match ev {
                        Ok(ev) if ev.kind == "change" => this.on_change(&ev.data),
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return,
                    },
                    ev = landed.recv() => match ev {
                        Ok(l) => this.on_landed(l),
                        Err(RecvError::Lagged(n)) => warn!(missed = n, "post-merge checks fell behind on landings"),
                        Err(RecvError::Closed) => return,
                    },
                }
            }
        })
    }

    /// After a restart: checks that were queued or running died with the old process (their
    /// containers are gone), so run them again on each open change's current head.
    pub fn recover(self: &Arc<Self>) {
        let open = match self.code.db.open_changes(10_000) {
            Ok(open) => open,
            Err(e) => return warn!(error = %e, "listing open changes to resume their checks"),
        };
        for c in open {
            let interrupted =
                c.checks.iter().any(|k| k.source == "runner" && matches!(k.status, CheckStatus::Queued | CheckStatus::Running));
            if interrupted && let Ok(data) = serde_json::to_value(&c) {
                info!(change = %c.id, "resuming checks interrupted by a restart");
                self.on_change(&data);
            }
        }
    }

    fn on_change(self: &Arc<Self>, data: &Value) {
        let s = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (id, head, state) = (s("id"), s("head"), s("state"));
        match ChangeState::parse(&state) {
            Some(ChangeState::Landed | ChangeState::Closed) => {
                self.started.lock().remove(&id);
                if !data.get("preview").is_none_or(Value::is_null) {
                    let code = self.code.clone();
                    tokio::spawn(async move {
                        if let Ok(c) = code.change_or_404(&id)
                            && let Ok(r) = code.repo_by_id(&c.repo_id)
                            && let Err(e) = code.delete_preview(&r, &c).await
                        {
                            warn!(change = %id, error = %e, "removing a finished change's preview");
                        }
                    });
                }
            }
            // A fork's pull request: running its code (with the repo's data) waits for a writer.
            Some(ChangeState::Open | ChangeState::Queued)
                if data.get("trusted").and_then(Value::as_bool) == Some(false)
                    && data.get("run_approved").and_then(Value::as_bool) != Some(true) => {}
            Some(ChangeState::Open | ChangeState::Queued) => {
                {
                    let mut started = self.started.lock();
                    if started.get(&id) == Some(&head) {
                        return;
                    }
                    if started.len() > 100_000 {
                        started.clear();
                    }
                    started.insert(id.clone(), head.clone());
                }
                let this = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = this.check_change(&id, &head).await {
                        warn!(change = %id, error = %format!("{e:#}"), "running a change's checks");
                    }
                });
            }
            _ => {}
        }
    }

    /// The change, if its head is still `head`.
    fn current(&self, id: &str, head: &str) -> Option<Change> {
        self.code.change_or_404(id).ok().filter(|c| c.head == head && matches!(c.state, ChangeState::Open | ChangeState::Queued))
    }

    fn report(&self, id: &str, head: &str, k: &Check) {
        let Some(c) = self.current(id, head) else { return };
        let body = json!({ "name": k.name, "status": k.status.as_str(), "source": "runner", "url": k.url, "summary": k.summary, "head": head });
        match self.code.report_check(&c, &body) {
            // The change moved on while the check ran: its result is for an old head.
            Ok(_) | Err(code_control::Error::Conflict(_)) => {}
            Err(e) => warn!(change = %id, check = %k.name, error = %e, "recording a check"),
        }
    }

    async fn check_change(self: &Arc<Self>, id: &str, head: &str) -> anyhow::Result<()> {
        let Some(c) = self.current(id, head) else { return Ok(()) };
        let r = self.code.repo_by_id(&c.repo_id)?;
        let cs = self.code.store.resolve_or_404(&r.id, head)?;
        // Most trees have no roll.toml: don't materialize them.
        if self.code.store.entry(&r.id, &cs, FILE)?.is_none() {
            return Ok(());
        }
        let tree = self.code.checkout(&r.id, &cs).await?;
        self.check_change_in(&r, &c, head, tree.path()).await
    }

    async fn check_change_in(self: &Arc<Self>, r: &RepoRow, c: &Change, head: &str, dir: &std::path::Path) -> anyhow::Result<()> {
        if let Ok(Some(RollToml { preview: Some(p), .. })) = RollToml::load(dir)
            && p.auto
            && self.code.previews.read().is_some()
        {
            let code = self.code.clone();
            let (r, c) = (r.clone(), c.clone());
            tokio::spawn(async move {
                if let Err(e) = code.create_preview(&r, &c).await {
                    warn!(change = %c.id, error = %e, "creating a preview");
                }
            });
        }
        let plan = self.runner.plan(r, dir).await?;
        for k in &plan {
            self.report(&c.id, head, k);
        }
        // The runner's slots bound how many actually run at once.
        let runs = plan.into_iter().filter(|k| k.status == CheckStatus::Queued).map(|k| {
            let this = self.clone();
            let (r, c, dir, head) = (r.clone(), c.clone(), dir.to_path_buf(), head.to_string());
            async move {
                // Revised or landed while queued: this head doesn't matter any more.
                if this.current(&c.id, &head).is_none() {
                    return;
                }
                let running = Check { status: CheckStatus::Running, ..k.clone() };
                this.report(&c.id, &head, &running);
                let done = match this.runner.run(&r, Some(&c), &dir, &k).await {
                    Ok(done) => done,
                    Err(e) => Check { status: CheckStatus::Failure, summary: Some(format!("the runner failed: {e:#}")), ..k },
                };
                this.report(&c.id, &head, &done);
            }
        });
        join_all(runs).await;
        Ok(())
    }

    fn on_landed(self: &Arc<Self>, l: Landed) {
        if l.already {
            return;
        }
        let Ok(r) = self.code.repo_by_id(&l.repo) else { return };
        if l.ref_name.strip_prefix("refs/heads/").unwrap_or(&l.ref_name) != r.default_branch {
            return;
        }
        {
            let mut main = self.main.lock();
            let run = main.entry(r.id.clone()).or_default();
            if run.running {
                run.pending = Some(l.changeset);
                return;
            }
            run.running = true;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut cs = l.changeset;
            loop {
                if let Err(e) = this.check_main(&r.id, cs).await {
                    warn!(repo = %r.full_name(), error = %format!("{e:#}"), "post-merge checks");
                }
                let mut main = this.main.lock();
                let run = main.entry(r.id.clone()).or_default();
                match run.pending.take() {
                    Some(next) => cs = next,
                    None => {
                        run.running = false;
                        return;
                    }
                }
            }
        });
    }

    async fn check_main(&self, repo_id: &str, cs: ChangesetId) -> anyhow::Result<()> {
        if self.code.store.entry(repo_id, &cs, FILE)?.is_none() {
            return Ok(());
        }
        let r = self.code.repo_by_id(repo_id)?;
        let tree = self.code.checkout(repo_id, &cs).await?;
        let dir = tree.path().to_path_buf();
        let plan = self.runner.plan(&r, &dir).await;
        let results = match plan {
            Ok(plan) => {
                let runs = plan.into_iter().map(|k| {
                    let (r, dir) = (&r, &dir);
                    async move {
                        if k.status != CheckStatus::Queued {
                            return k;
                        }
                        match self.runner.run(r, None, dir, &k).await {
                            Ok(done) => done,
                            Err(e) => Check { status: CheckStatus::Failure, summary: Some(format!("the runner failed: {e:#}")), ..k },
                        }
                    }
                });
                join_all(runs).await
            }
            Err(e) => vec![Check {
                name: FILE.into(),
                status: CheckStatus::Failure,
                source: "runner".into(),
                url: None,
                summary: Some(format!("{e:#}")),
                started_at: None,
                finished_at: None,
            }],
        };
        drop(tree);
        self.after_main(&r, &cs, &results).await
    }

    /// Open a `broken-main` item for failures (one at a time), or close it when main is green.
    /// A new failure is bisected back to the last green head, and the change that broke it
    /// gets a revert (landed right away with `[main] auto_revert`). Items opened for external
    /// CI statuses (`status:<context>`) are handled by code-control, not here.
    async fn after_main(&self, r: &RepoRow, cs: &ChangesetId, results: &[Check]) -> anyhow::Result<()> {
        let id = code_control::cs_id(cs);
        let open = self.code.open_broken_main(r, Source::Runner)?;
        let failed: Vec<&Check> = results.iter().filter(|k| k.status == CheckStatus::Failure).collect();
        if failed.is_empty() {
            if results.is_empty() {
                return Ok(());
            }
            self.code.db.set_last_green(&r.id, &id)?;
            // Items an agent claimed stay theirs; a revert roll opened isn't needed any more.
            self.code.main_green(r, cs, open)?;
            return Ok(());
        }
        if !open.is_empty() {
            return Ok(());
        }
        let failing: BTreeSet<String> = failed.iter().map(|k| k.name.clone()).collect();
        let culprit = match self.code.find_culprit(r, cs, &failing).await {
            Ok(c) => c,
            Err(e) => {
                warn!(repo = %r.full_name(), error = %format!("{e:#}"), "bisecting a broken default branch");
                None
            }
        };
        let failed: Vec<(String, Option<String>)> = failed.iter().map(|k| (k.name.clone(), k.summary.clone())).collect();
        self.code.report_broken_main(r, cs, Source::Runner, &failed, culprit, None).await?;
        Ok(())
    }
}
