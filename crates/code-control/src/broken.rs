//! A broken default branch, from either side that can notice it:
//!
//! - **roll's checks runner** (code-preview's post-merge runs): one `broken-main` item at a
//!   time for the runner's checks, bisected by re-running the failing checks.
//! - **External CI** posting commit statuses (TeamCity, Jenkins, Buildkite, …): a failing
//!   status on a commit of the default branch opens one item per status context (labelled
//!   `status:<context>`). roll can't re-run someone else's CI, so the culprit is found from the
//!   statuses CI already reported: when the commit right after the context's last success is the
//!   first one reported failing, that's the culprit (and it gets a revert, landed with
//!   `[main] auto_revert`); otherwise the item names the range to look at. A success for the
//!   context on that commit or a later one closes the item, unless an agent claimed it.
//!
//! Both share how items are opened, how reverts are made and landed, and how they close.

use crate::db::Db;
use crate::model::{ChangeState, RepoRow, WorkItem, WorkState};
use crate::{Code, Result, cs_id};
use code_sql::{OptionalExtension, params};
use code_store::ChangesetId;
use serde_json::json;
use tracing::{info, warn};

pub const BROKEN_MAIN: &str = "broken-main";
/// First-parent steps walked back looking for a context's last success.
const MAX_STATUS_WALK: usize = 1_000;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS broken_main_at (
    work_id TEXT PRIMARY KEY,
    changeset TEXT NOT NULL
);
"#;

impl Db {
    /// The commit a status-opened `broken-main` item was opened for.
    pub(crate) fn broken_at(&self, work_id: &str) -> Result<Option<String>> {
        Ok(self.sql().query_row("SELECT changeset FROM broken_main_at WHERE work_id = ?", [work_id], |r| r.get(0)).optional()?)
    }

    pub(crate) fn set_broken_at(&self, work_id: &str, cs: &str) -> Result<()> {
        self.sql().execute(
            "INSERT INTO broken_main_at (work_id, changeset) VALUES (?, ?) ON CONFLICT (work_id) DO UPDATE SET changeset = excluded.changeset",
            params![work_id, cs],
        )?;
        Ok(())
    }
}

/// The label of a `broken-main` item opened for an external status context.
pub fn status_label(context: &str) -> String {
    format!("status:{context}")
}

/// What a failure is about: roll's runner checks, or one external status context.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    Runner,
    Status(&'a str),
}

impl Code {
    /// Open (not done or closed) `broken-main` items of one source.
    pub fn open_broken_main(&self, r: &RepoRow, source: Source<'_>) -> Result<Vec<WorkItem>> {
        let items = self.db.work_items(&r.id, None, Some(BROKEN_MAIN), 200)?;
        Ok(items
            .into_iter()
            .filter(|w| !matches!(w.state, WorkState::Done | WorkState::Closed))
            .filter(|w| match source {
                Source::Runner => !w.labels.iter().any(|l| l.starts_with("status:")),
                Source::Status(ctx) => w.labels.contains(&status_label(ctx)),
            })
            .collect())
    }

    /// Open a `broken-main` item for `failed` (name and output) at `cs`. With a `culprit`, a
    /// revert is opened too, and landed (or queued) with `[main] auto_revert`; otherwise the
    /// item stays open for an agent.
    pub async fn report_broken_main(
        &self,
        r: &RepoRow,
        cs: &ChangesetId,
        source: Source<'_>,
        failed: &[(String, Option<String>)],
        culprit: Option<ChangesetId>,
        note: Option<String>,
    ) -> Result<WorkItem> {
        let id = cs_id(cs);
        let short = &id[..id.len().min(15)];
        let names: Vec<&str> = failed.iter().map(|(n, _)| n.as_str()).collect();
        let what = match source {
            Source::Runner => "Post-merge checks failed",
            Source::Status(_) => "CI reported a failure",
        };
        let mut body = format!("{what} on `{}` at `{id}`.\n", r.default_branch);
        if let Ok(Some(m)) = self.store.meta(&r.id, cs) {
            let message = String::from_utf8_lossy(&m.cs.message);
            let subject = message.lines().next().unwrap_or("").trim().to_string();
            if !subject.is_empty() {
                body.push_str(&format!("\nLanded: {subject}\n"));
            }
        }
        for (name, output) in failed {
            body.push_str(&format!("\n### {name}\n\n```\n{}\n```\n", output.as_deref().unwrap_or("").replace("```", "ʼʼʼ")));
        }
        let culprit_info = culprit.and_then(|c| self.store.info(&r.id, &c).ok().flatten());
        if let Some(info) = &culprit_info {
            body.push_str(&format!("\nBisected to `{}`: {} ({}).\n", cs_id(&info.id), info.summary, info.author.name()));
        }
        if let Some(note) = &note {
            body.push_str(&format!("\n{note}\n"));
        }
        body.push_str("\nFix it or revert the change that broke it. This item closes itself when checks pass again.\n");
        let title = format!("{} broken at {short} ({})", r.default_branch, names.join(", "));
        let mut labels = vec![BROKEN_MAIN.to_string()];
        if let Source::Status(ctx) = source {
            labels.push(status_label(ctx));
        }
        let item = self.create_work(r, &json!({ "title": title, "body": body, "labels": labels, "priority": 0 }))?;
        warn!(repo = %r.full_name(), checks = ?names, "main is broken");
        if let Some(culprit) = culprit {
            let why = format!("It broke {} on {} (checks: {}).", short, r.default_branch, names.join(", "));
            // Without auto_revert the revert is a suggestion: the item stays open for an agent
            // to claim (and land the revert, or fix forward).
            let auto = self.settings(r).await.main.auto_revert;
            let revert = self.revert(r, &culprit, Some(&item.id), &why, !auto).await?;
            if auto {
                match self.land_revert(r, &revert).await {
                    Ok(_) => info!(repo = %r.full_name(), change = revert.number, "auto-revert landed or queued"),
                    Err(e) => warn!(repo = %r.full_name(), change = revert.number, error = %e, "landing an auto-revert"),
                }
            }
        }
        self.work_or_404(&item.id)
    }

    /// Checks pass again at `cs`: close `items` nobody claimed, and the reverts roll opened for
    /// them that aren't needed any more.
    pub fn main_green(&self, r: &RepoRow, cs: &ChangesetId, items: Vec<WorkItem>) -> Result<()> {
        let id = cs_id(cs);
        for w in items.into_iter().filter(|w| matches!(w.state, WorkState::Open | WorkState::Review)) {
            for c in self.db.changes_for_work(&w.id)? {
                if let Ok(c) = self.change_or_404(&c)
                    && c.author == "roll"
                    && c.title.starts_with("Revert: ")
                    && matches!(c.state, ChangeState::Open | ChangeState::Queued | ChangeState::Conflict)
                {
                    self.close_change(&c)?;
                }
            }
            let body = format!("{}\n\nChecks pass again at `{id}`.", w.body.trim_end());
            self.update_work(&w, &json!({ "state": "done", "body": body }))?;
            info!(repo = %r.full_name(), item = w.number, "main is green again");
        }
        Ok(())
    }

    /// A status posted for `cs`: when `cs` is on the default branch, open or close the
    /// context's `broken-main` item.
    pub(crate) async fn main_status(&self, r: &RepoRow, cs: &ChangesetId, context: &str, state: &str, output: Option<String>) -> Result<()> {
        let Some(head) = self.store.get_ref(&r.id, &code_store::full_ref(&r.default_branch))? else { return Ok(()) };
        if head != *cs && !self.store.is_fp_ancestor(&r.id, cs, &head)? {
            return Ok(());
        }
        let open = self.open_broken_main(r, Source::Status(context))?;
        match state {
            "success" => {
                // Only a success at or after the commit an item was opened for closes it.
                let mut closing = Vec::new();
                for w in open {
                    let at = self.db.broken_at(&w.id)?.and_then(|c| self.store.resolve(&r.id, &c).ok().flatten());
                    let after = match at {
                        Some(at) => at == *cs || self.store.is_fp_ancestor(&r.id, &at, cs)?,
                        None => true,
                    };
                    if after {
                        closing.push(w);
                    }
                }
                self.main_green(r, cs, closing)
            }
            "failure" | "error" if open.is_empty() => {
                let (culprit, note) = self.status_culprit(r, cs, context)?;
                let item = self.report_broken_main(r, cs, Source::Status(context), &[(context.to_string(), output)], culprit, note).await?;
                self.db.set_broken_at(&item.id, &cs_id(cs))?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Where a failing `context` at `bad` started, from the statuses CI reported on the
    /// first-parent commits since its last success: the culprit when the first commit after
    /// that success is the first one reported failing, else a note naming the range.
    fn status_culprit(&self, r: &RepoRow, bad: &ChangesetId, context: &str) -> Result<(Option<ChangesetId>, Option<String>)> {
        // bad and its first parents, newest first, until the context's last success.
        let mut chain = Vec::new();
        let mut cur = *bad;
        let mut green = None;
        while chain.len() < MAX_STATUS_WALK {
            let st = self.db.statuses(&r.id, &cs_id(&cur))?.into_iter().find(|s| s.context == context).map(|s| s.state);
            if st.as_deref() == Some("success") {
                green = Some(cur);
                break;
            }
            chain.push((cur, st));
            let Some(m) = self.store.meta(&r.id, &cur)? else { break };
            match m.cs.parents.first() {
                Some(p) => cur = *p,
                None => break,
            }
        }
        let Some(green) = green else {
            return Ok((None, Some(format!("No earlier success of `{context}` is known, so the change that broke it isn't known."))));
        };
        chain.reverse();
        // The first commit after the success that CI reported failing.
        let Some(first_bad) = chain.iter().position(|(_, s)| matches!(s.as_deref(), Some("failure") | Some("error"))) else {
            return Ok((None, None));
        };
        if first_bad == 0 {
            return Ok((Some(chain[0].0), None));
        }
        let (from, to) = (cs_id(&green), cs_id(&chain[first_bad].0));
        Ok((
            None,
            Some(format!(
                "`{context}` passed at `{from}` and failed at `{to}`: one of the {} changesets after `{from}`, up to and including \
                 `{to}`, broke it. Report `{context}` for the ones in between to narrow it down.",
                first_bad + 1
            )),
        ))
    }
}
