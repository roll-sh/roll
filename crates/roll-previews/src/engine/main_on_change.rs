//! Main's code on the change's migrated data (pgroll's and Reshape's expand/contract, tested on
//! real data instead of assumed): while a change deploys, the version before it still serves on
//! the schema its release just changed. Once per head, main's own build runs on a copy of the
//! change's branches as the release left them, and its pages are read there and on main's
//! preview: one that answers on main's data but not on the change's is what production would do.

use super::*;

/// The most pages read on each (`/` and the project's `github.pages`).
const PAGES_MAX: usize = 10;

impl Engine {
    /// Whether main's code is due on this change's data at its head: a trusted change (its data
    /// is read by main's build, with main's secrets) whose release changed the schema, not yet
    /// tried at this head.
    pub(super) fn main_on_change_due(&self, r: &Row, s: &Settings) -> anyhow::Result<bool> {
        if r.change_id.is_none() || !r.trusted || s.release.is_none() {
            return Ok(false);
        }
        if self.store.schema_head(&r.id)?.as_deref() != Some(r.build()) || self.store.schema_changes(&r.id)?.is_none_or(|c| c.is_empty()) {
            return Ok(false);
        }
        Ok(self.store.runtime_fact::<crate::MainOnChange>(&r.id, "main_on_change")?.is_none_or(|m| m.head != r.build()))
    }

    /// Main's live build of each of its services, taking no traffic, on copies of the change's
    /// Postgres branches (masked as they are; roll apps' rollback with variables), its pages read
    /// there and on main's preview; the copies go after, whatever happened. Kept as
    /// `main_on_change` at this head, said in the activity; what it couldn't do is said there too
    /// (never the converge's failure).
    pub(super) async fn main_on_change(&self, r: &Row, s: &Settings) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut made: Vec<(Arc<dyn Brancher>, String, String, String)> = Vec::new();
        let mut ran_here: Vec<String> = Vec::new();
        let ran = self.main_on_copies(r, s, &mut made, &mut ran_here).await;
        // What ran on roll's runtime stops (its variables go with it), then the copies go.
        for name in ran_here {
            self.main_runs.lock().remove(&(r.id.clone(), name.clone()));
            if let Err(e) = self.apps.remove(&r.id, &format!("{name}@main")).await {
                warn!(preview = %r.id, error = %e, "previews: stopping main's code on the copy");
            }
        }
        for (b, parent, branch, name) in made {
            if let Err(e) = b.delete(&parent, &branch, &name).await {
                warn!(preview = %r.id, error = %e, "previews: deleting the copy main's code ran on");
            }
        }
        let (pages, error) = match ran {
            Ok(pages) => (pages, None),
            Err(e) => (Vec::new(), Some(self.mask_text(&r.id, &format!("{e:#}").lines().next().unwrap_or("").chars().take(300).collect::<String>()).await?)),
        };
        let broke: Vec<String> = pages.iter().filter(|p| p.broke()).map(|p| format!("{} {} ({} on main's data)", p.path, status_words(p.status), status_words(p.main_status))).collect();
        let line = match (&error, broke.is_empty()) {
            (Some(e), _) => format!("main's code couldn't run on this change's migrated data: {e}"),
            (None, true) => format!("main's code still answers on this change's migrated data ({} page(s))", pages.len()),
            (None, false) => format!("main's code breaks on this change's migrated data: {}", broke.join(", ")),
        };
        self.store.log(&r.id, &line)?;
        let done = crate::MainOnChange { head: r.build().to_string(), at: self.store.now_ms()?, ms: started.elapsed().as_millis() as i64, pages, error };
        self.store.set_runtime_fact(&r.id, "main_on_change", Some(&done))?;
        Ok(())
    }

    async fn main_on_copies(&self, r: &Row, s: &Settings, made: &mut Vec<(Arc<dyn Brancher>, String, String, String)>, ran_here: &mut Vec<String>) -> anyhow::Result<Vec<crate::MainOnChangePage>> {
        let main = self.main_preview(r).await?.ok_or_else(|| anyhow::anyhow!("the project has no preview of main"))?;
        // Each of main's services: its roll apps build, or, on roll's runtime, its revision run in
        // the change's preview as `<name>@main` (only the change's repo's: the others run main's
        // code in the change's preview already).
        let mut runs = Vec::new();
        for svc in self.store.services(&main.id)? {
            if let Some(live) = self.apps.live_build(&main.id, &svc.name).await {
                runs.push((svc, Run::Apps(live.id)));
            } else if svc.repo == r.repo
                && let Some(rev) = svc.revision.clone().filter(|v| !v.is_empty())
                && self.store.services(&r.id)?.iter().any(|x| x.name == svc.name)
                && self.apps.can_swap(&r.id, &svc.name).await.unwrap_or(false)
            {
                runs.push((svc, Run::Here(rev)));
            }
        }
        anyhow::ensure!(!runs.is_empty(), "main's preview has no service of the change's repo that can run on a copy");
        // The change's branches as its release left them, copied; the branches' variables in
        // main's environment and in the change's swapped for the copy's by value (the names roll
        // makes from them too: `PGHOST`…).
        // Named by the head, so a copy a crash left behind is never an earlier head's.
        let name = format!("{}-m{}", branch_name(&r.id), crate::label_part(r.build().trim_start_matches("cs_"), 8));
        let mut mains = self.resource_env(&main.id).await?;
        let mut changes = self.resource_env(&r.id).await?;
        let mut branches: Vec<crate::ports::BranchVars> = Vec::new();
        let ours = self.store.resources(&main.id)?;
        // Every branch the change's services get, copied: main's code never runs on the change's
        // own (a second consumer on its queue, writes to its bucket). One a product can't copy
        // stops it, saying which.
        for res in self.store.resources(&r.id)? {
            let Some(branch) = &res.r#ref else { continue };
            let b = self.brancher(res.kind)?.clone();
            let copy = b.snapshot(&res.parent, branch, &name).await?.ok_or_else(|| anyhow::anyhow!("{}:{} can't be copied, so main's code would share the change's own", res.kind.as_str(), res.parent))?;
            made.push((b.clone(), res.parent.clone(), copy.r#ref.clone(), name.clone()));
            let new = b.connect(&res.parent, &copy.r#ref, copy.kept.as_deref()).await?;
            let swapped = crate::names::with_standard(res.kind, new.clone());
            swap(&mut changes, &crate::names::with_standard(res.kind, b.connect(&res.parent, branch, res.kept.as_deref()).await?), &swapped);
            if let Some(m) = ours.iter().find(|x| x.kind == res.kind && x.parent == res.parent)
                && let Some(main_branch) = &m.r#ref
            {
                swap(&mut mains, &crate::names::with_standard(res.kind, b.connect(&m.parent, main_branch, m.kept.as_deref()).await?), &swapped);
            }
            branches.push((res.kind, res.parent.clone(), new));
        }
        anyhow::ensure!(branches.iter().any(|(k, _, _)| *k == Kind::Postgres), "the change has no Postgres branch");
        let main_access = self.store.access(&main.id)?.filter(|a| a.required()).map(|a| a.token);
        let own_access = self.store.access(&r.id)?.filter(|a| a.required()).map(|a| a.token);
        let mut paths = vec!["/".to_string()];
        paths.extend(s.github.pages.iter().filter(|p| *p != "/").cloned());
        paths.truncate(PAGES_MAX);
        let mut pages = Vec::new();
        for (i, (svc, run)) in runs.iter().enumerate() {
            // Where main's code on the copy answers: (preview, service, URL, link token).
            let (at_preview, at_service, at_url, at_access) = match run {
                Run::Apps(deployment) => {
                    let url = self.apps.view_build(&main.id, &svc.name, deployment, &mains, &branches).await?.ok_or_else(|| anyhow::anyhow!("{} can't run on a copy here", svc.name))?;
                    (main.id.as_str(), svc.name.clone(), url, main_access.as_deref())
                }
                Run::Here(rev) => {
                    self.main_runs.lock().insert((r.id.clone(), svc.name.clone()), (rev.clone(), changes.clone()));
                    ran_here.push(svc.name.clone());
                    (r.id.as_str(), format!("{}@main", svc.name), String::new(), own_access.as_deref())
                }
            };
            // The project's pages are its first service's; the others are asked for `/`.
            for path in paths.iter().take(if i == 0 { PAGES_MAX } else { 1 }) {
                let (Some(status), Some(main_status)) = (self.status_of(at_preview, &at_service, &at_url, at_access, path).await, self.status_of(&main.id, &svc.name, &svc.url, main_access.as_deref(), path).await) else { continue };
                pages.push(crate::MainOnChangePage { service: svc.name.clone(), path: path.clone(), status, main_status });
            }
        }
        Ok(pages)
    }

    /// `path`'s status there: 0 when nothing answered (refused, timed out); `None` where the
    /// backend can't read it (no page).
    async fn status_of(&self, preview: &str, service: &str, url: &str, access: Option<&str>, path: &str) -> Option<u16> {
        self.apps.fetch(preview, service, url, access, path, false).await.map(|f| f.map(|f| f.status)).unwrap_or(Some(0))
    }
}

/// Where main's code runs on the copy: its roll apps deployment again (by id), or its revision on
/// roll's runtime in the change's preview.
enum Run {
    Apps(String),
    Here(String),
}

/// `env`'s values that are `old`'s swapped for `new`'s under the same name.
fn swap(env: &mut [(String, String)], old: &[(String, String)], new: &[(String, String)]) {
    for (k, v) in old {
        if let Some((_, nv)) = new.iter().find(|(nk, _)| nk == k) {
            for kv in env.iter_mut().filter(|(_, vv)| vv == v) {
                kv.1 = nv.clone();
            }
        }
    }
}

/// A status as the activity says it: 0 is no answer.
fn status_words(status: u16) -> String {
    if status == 0 { "no answer".into() } else { status.to_string() }
}
