//! Leaked roll tokens (docs/permissions.md, "Leaked tokens"): what lands in any repo is read after
//! it lands (from the store's durable land log, never inside the lander) for roll grant secrets,
//! and each live one is revoked through the hook rolld sets (roll-access checks the checksums and
//! revokes). One node at a time (a cluster singleton), with a cursor of its own so webhooks never
//! wait on it; changed files up to 1 MiB, every changeset a landing added.

use crate::{Code, Result};
use code_store::model::FileChange;
use code_sql::OptionalExtension;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS leak_scan_cursor (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL
);
";

const SINGLETON: &str = "leak-scan";
const PAGE: usize = 100;
/// Files bigger than this aren't read (generated or binary).
const MAX_FILE: u64 = 1 << 20;
/// Changesets of one landing read at most (a stack).
const MAX_STACK: usize = 50;
/// A roll grant secret starts with one of these.
const PREFIXES: [&[u8]; 3] = [b"rlt_", b"rlo_", b"rlf_"];

/// What a file that may hold a roll token goes to: its text and where it is ("acme/site .env at
/// 1a2b3c4d5e6f"); answers how many live tokens it revoked.
pub type LeakHandler = Arc<dyn Fn(String, String) -> usize + Send + Sync>;

impl Code {
    /// Where leaked tokens go (rolld: roll's access service), and the scanner starts.
    pub fn on_leaked_secrets(self: &Arc<Self>, f: LeakHandler) {
        if self.leak_handler.set(f).is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(code) = weak.upgrade() else { return };
                if let Err(e) = code.scan_landings().await {
                    tracing::warn!(error = %e, "scanning landings for leaked tokens");
                }
                drop(code);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    fn leak_cursor(&self) -> Result<Option<i64>> {
        Ok(self.db.sql().query_row("SELECT seq FROM leak_scan_cursor WHERE id = 1", (), |r| r.get(0)).optional()?)
    }

    /// One pass over the land log after the cursor: `(landings read, tokens revoked)`. The first
    /// pass starts at the present.
    pub async fn scan_landings(&self) -> Result<(usize, usize)> {
        let Some(handler) = self.leak_handler.get().cloned() else { return Ok((0, 0)) };
        let mut done = (0, 0);
        loop {
            if !self.singleton(SINGLETON) {
                return Ok(done);
            }
            let head = self.store.land_log_head()?;
            let Some(cursor) = self.leak_cursor()? else {
                self.db.sql().execute("INSERT INTO leak_scan_cursor (id, seq) VALUES (1, ?) ON CONFLICT (id) DO NOTHING", [head])?;
                return Ok(done);
            };
            if head <= cursor {
                return Ok(done);
            }
            let page = self.store.land_log(cursor, PAGE)?;
            let Some(&(last, _)) = page.last() else { return Ok(done) };
            for (_, l) in &page {
                if l.already {
                    continue;
                }
                done.1 += self.scan_landing(l, &handler).await?;
                done.0 += 1;
            }
            // Another scanner that moved the cursor first wins; this one reads on from there.
            self.db.sql().execute("UPDATE leak_scan_cursor SET seq = ? WHERE id = 1 AND seq = ?", [last, cursor])?;
            if page.len() < PAGE {
                return Ok(done);
            }
        }
    }

    /// The files every changeset of one landing changed, read for roll tokens.
    async fn scan_landing(&self, l: &code_store::Landed, handler: &LeakHandler) -> Result<usize> {
        let repo = self.db.repo_by_id(&l.repo)?.map_or_else(|| l.repo.clone(), |r| r.full_name());
        let mut revoked = 0;
        let mut id = l.changeset;
        for _ in 0..l.count.clamp(1, MAX_STACK) {
            let Some(cs) = self.store.changeset(&l.repo, &id)? else { break };
            for (path, change) in &cs.files {
                let FileChange::Modify { blob, .. } = change else { continue };
                if self.store.blob_size(&l.repo, blob)?.is_none_or(|n| n > MAX_FILE) {
                    continue;
                }
                let Some(bytes) = self.store.get_blob(&l.repo, blob).await? else { continue };
                if !PREFIXES.iter().any(|p| bytes.windows(p.len()).any(|w| w == *p)) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let at = format!("{repo} {} at {}", path.as_str(), &id.to_string()[..12.min(id.to_string().len())]);
                let h = handler.clone();
                revoked += tokio::task::spawn_blocking(move || h(text, at)).await.unwrap_or(0);
            }
            match cs.parents.first() {
                Some(p) => id = *p,
                None => break,
            }
        }
        Ok(revoked)
    }
}
