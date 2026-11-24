//! Links that name their resource by something other than the id it goes by for good (a search
//! cluster by its name, from before the provider took ids only): listed, and kept by the id from
//! then on, in one write each, audited (`link.rekey`, never a value). Which id a ref stands for
//! is the caller's to decide (rolld's start-up pass, from the product and the workspace).

use crate::access::Caller;
use crate::links::Link;
use crate::{Error, Result, Secrets, audit};
use code_sql::params;

impl Secrets {
    /// Every project's links of `kind`, as `(project, ref)`, once each.
    pub fn link_refs(&self, kind: &str) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = self.db.read(|c| c.query("SELECT project_id, link FROM secrets WHERE link IS NOT NULL", params![], |r| Ok((r.get(0)?, r.get(1)?))))?;
        let mut out: Vec<(String, String)> = rows
            .into_iter()
            .filter_map(|(project, link)| serde_json::from_str::<Link>(&link).ok().filter(|l| l.kind == kind).map(|l| (project, l.r#ref)))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// `project`'s links of `kind` to `from` name `to` from now on, in one write, audited as the
    /// operator's `link.rekey` with the keys it moved (never a value). Their fingerprints stay:
    /// what they resolve to is the same resource, so nothing redeploys for it. How many moved.
    pub fn rekey_links(&self, project: &str, kind: &str, from: &str, to: &str) -> Result<usize> {
        let mac = self.keys()?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let rows: Vec<(String, String, String)> = c.query("SELECT id, key, link FROM secrets WHERE project_id = ? AND link IS NOT NULL", params![project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut keys = Vec::new();
            for (id, key, link) in rows {
                let Ok(mut l) = serde_json::from_str::<Link>(&link) else { continue };
                if l.kind != kind || l.r#ref != from {
                    continue;
                }
                l.r#ref = to.to_string();
                let json = serde_json::to_string(&l).map_err(|e| Error::Internal(e.to_string()))?;
                c.execute("UPDATE secrets SET link = ? WHERE id = ?", params![json, id])?;
                keys.push(key);
            }
            if !keys.is_empty() {
                let summary = format!("{} {kind} link(s) to {from} name it by its id, {to}, from now on", keys.len());
                audit::record(c, mac, project, &Caller::operator(), "link.rekey", None, None, &keys, &summary, now)?;
            }
            Ok::<_, Error>(keys.len())
        })
    }
}
