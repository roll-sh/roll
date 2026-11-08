//! GitHub doesn't redeliver a failed webhook delivery on its own, so roll asks it to
//! (docs/code/architecture.md, "Catching up"). On start and every
//! [`super::Options::catchup_every`], one node (the singleton [`SINGLETON`]) reads the App's
//! delivery log (`GET /app/hook/deliveries`, kept three days) back to its last look, and for each
//! delivery that no attempt got through and roll hasn't recorded (its `X-GitHub-Delivery`
//! isn't in `github_deliveries`) asks for another attempt
//! (`POST /app/hook/deliveries/{id}/attempts`). The redelivery arrives as an ordinary webhook.
//! A delivery is asked for at most [`MAX_ASKS`] times, and not again while its last ask's attempt
//! hasn't shown up in the log. Anything older than the log is the reconcile's
//! ([`super::reconcile`]).

use super::{mark, set_mark};
use crate::{Code, Error, Result};
use code_github::model::HookDelivery;
use code_sql::{OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Arc;

/// The cluster-wide worker that asks GitHub for redeliveries.
pub(crate) const SINGLETON: &str = "github-catchup";
/// How long GitHub keeps its delivery log.
const WINDOW: i64 = 3 * 24 * 3600;
/// Deliveries this long before the last look are read again (attempts still in flight).
const MARGIN: i64 = 10 * 60;
/// Redeliveries roll asks for, per delivery.
const MAX_ASKS: u32 = 5;
const SCANNED: &str = "catchup:scanned";

pub(super) fn start(code: &Arc<Code>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Some(every) = code.github_hub().map(|h| h.opts.catchup_every) else { return };
    let me = Arc::downgrade(code);
    tokio::spawn(async move {
        loop {
            let Some(code) = me.upgrade() else { return };
            if code.singleton(SINGLETON)
                && let Err(e) = catch_up(&code).await
            {
                tracing::warn!(error = %e, "asking GitHub to redeliver missed webhooks; trying again later");
            }
            drop(code);
            tokio::time::sleep(every.max(std::time::Duration::from_millis(100))).await;
        }
    });
}

/// One look at the delivery log; how many redeliveries were asked for.
pub(super) async fn catch_up(code: &Code) -> Result<usize> {
    let hub = code.github_hub().cloned().ok_or_else(|| Error::NotFound("GitHub App".into()))?;
    let now = crate::db::now();
    let oldest = now - WINDOW + MARGIN;
    let since = mark(code, SCANNED)?.and_then(|v| v.parse::<i64>().ok()).map_or(oldest, |s| (s - MARGIN).max(oldest));
    let log = hub.client.hook_deliveries(since.max(0) as u64).await.map_err(|e| Error::Internal(e.into()))?;
    let mut by_guid: HashMap<&str, Vec<&HookDelivery>> = HashMap::new();
    for d in &log {
        by_guid.entry(d.guid.as_str()).or_default().push(d);
    }
    let mut asked = 0;
    for (guid, attempts) in by_guid {
        if attempts.iter().any(|d| d.ok()) || recorded(code, guid)? {
            continue;
        }
        let Some(latest) = attempts.iter().max_by_key(|d| d.id) else { continue };
        let at = latest.delivered_at.as_deref().and_then(code_github::auth::parse_timestamp).map(|t| t as i64);
        if at.is_some_and(|t| t < oldest) {
            continue;
        }
        let key = format!("redeliver:{guid}");
        let (n, last) = match mark(code, &key)? {
            Some(v) => match v.split_once(':') {
                Some((n, id)) => (n.parse::<u32>().unwrap_or(MAX_ASKS), id.parse::<u64>().ok()),
                None => (MAX_ASKS, None),
            },
            None => (0, None),
        };
        // Given up, or the attempt last asked for hasn't come back yet.
        if n >= MAX_ASKS || last == Some(latest.id) {
            continue;
        }
        match hub.client.redeliver(latest.id).await {
            Ok(()) => {
                set_mark(code, &key, &format!("{}:{}", n + 1, latest.id))?;
                asked += 1;
            }
            // GitHub is struggling: the next round tries again.
            Err(e) if e.status().is_none_or(|s| s >= 500 || s == 429) => return Err(Error::Internal(e.into())),
            Err(e) => {
                tracing::warn!(delivery = guid, error = %e, "GitHub refused to redeliver a webhook");
                set_mark(code, &key, &format!("{MAX_ASKS}:{}", latest.id))?;
            }
        }
    }
    if asked > 0 {
        tracing::info!(deliveries = asked, "asked GitHub to redeliver webhooks roll missed");
    }
    set_mark(code, SCANNED, &now.to_string())?;
    code.db.sql().execute("DELETE FROM github_marks WHERE key LIKE 'redeliver:%' AND at < ?", params![now - WINDOW])?;
    Ok(asked)
}

/// roll recorded this delivery (it's handled or waiting in the inbox).
fn recorded(code: &Code, guid: &str) -> Result<bool> {
    Ok(code.db.sql().query_row("SELECT 1 FROM github_deliveries WHERE id = ?", [guid], |r| r.get::<_, i64>(0)).optional()?.is_some())
}

impl Code {
    /// Tests: look at the App's delivery log and ask for redeliveries now.
    #[doc(hidden)]
    pub async fn github_catch_up_now(&self) -> Result<usize> {
        catch_up(self).await
    }
}
