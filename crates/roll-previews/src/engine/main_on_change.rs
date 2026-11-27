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
        let ran = self.main_on_copies(r, s, &mut made).await;
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

    async fn main_on_copies(&self, r: &Row, s: &Settings, made: &mut Vec<(Arc<dyn Brancher>, String, String, String)>) -> anyhow::Result<Vec<crate::MainOnChangePage>> {
        let main = self.main_preview(r).await?.ok_or_else(|| anyhow::anyhow!("the project has no preview of main"))?;
        let mut builds = Vec::new();
        for svc in self.store.services(&main.id)? {
            if let Some(live) = self.apps.live_build(&main.id, &svc.name).await {
                builds.push((svc, live));
            }
        }
        anyhow::ensure!(!builds.is_empty(), "main's preview has no build that can run on a copy (roll apps services can)");
        // The change's branches as its release left them, copied; main's own in its variables
        // swapped for them by value (the names roll makes from them too: `PGHOST`…).
        let name = format!("{}-m", branch_name(&r.id));
        let mut plain = self.resource_env(&main.id).await?;
        let mut branches: Vec<crate::ports::BranchVars> = Vec::new();
        let ours = self.store.resources(&main.id)?;
        for res in self.store.resources(&r.id)?.into_iter().filter(|x| x.kind == Kind::Postgres) {
            let Some(branch) = &res.r#ref else { continue };
            let b = self.brancher(res.kind)?.clone();
            let copy = b.snapshot(&res.parent, branch, &name).await?.ok_or_else(|| anyhow::anyhow!("postgres:{} can't be copied", res.parent))?;
            made.push((b.clone(), res.parent.clone(), copy.r#ref.clone(), name.clone()));
            let new = b.connect(&res.parent, &copy.r#ref, copy.kept.as_deref()).await?;
            if let Some(m) = ours.iter().find(|x| x.kind == res.kind && x.parent == res.parent)
                && let Some(main_branch) = &m.r#ref
            {
                let old = crate::names::with_standard(res.kind, b.connect(&m.parent, main_branch, m.kept.as_deref()).await?);
                let swapped = crate::names::with_standard(res.kind, new.clone());
                for (k, v) in &old {
                    if let Some((_, nv)) = swapped.iter().find(|(nk, _)| nk == k) {
                        for kv in plain.iter_mut().filter(|(_, vv)| vv == v) {
                            kv.1 = nv.clone();
                        }
                    }
                }
            }
            branches.push((res.kind, res.parent.clone(), new));
        }
        anyhow::ensure!(!branches.is_empty(), "the change has no Postgres branch");
        let access = self.store.access(&main.id)?.filter(|a| a.required()).map(|a| a.token);
        let (access, main_id) = (access.as_deref(), main.id.as_str());
        let mut paths = vec!["/".to_string()];
        paths.extend(s.github.pages.iter().filter(|p| *p != "/").cloned());
        paths.truncate(PAGES_MAX);
        let mut pages = Vec::new();
        for (i, (svc, live)) in builds.iter().enumerate() {
            let url = self.apps.view_build(main_id, &svc.name, &live.id, &plain, &branches).await?.ok_or_else(|| anyhow::anyhow!("{} can't run on a copy here", svc.name))?;
            // The project's pages are its first service's; the others are asked for `/`.
            for path in paths.iter().take(if i == 0 { PAGES_MAX } else { 1 }) {
                // No answer (refused, timed out) is 0; a backend that can't read it, no page.
                let read = |at: String| async move { self.apps.fetch(main_id, &svc.name, &at, access, path, false).await.map(|f| f.map(|f| f.status)).unwrap_or(Some(0)) };
                let (Some(status), Some(main_status)) = (read(url.clone()).await, read(svc.url.clone()).await) else { continue };
                pages.push(crate::MainOnChangePage { service: svc.name.clone(), path: path.clone(), status, main_status });
            }
        }
        Ok(pages)
    }
}

/// A status as the activity says it: 0 is no answer.
fn status_words(status: u16) -> String {
    if status == 0 { "no answer".into() } else { status.to_string() }
}
