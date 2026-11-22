//! Query suggestions from search analytics, the control plane's part: building an index's
//! suggestions index from the analytics the catalog keeps (`crate::analytics`), on the
//! cluster's owner, on a schedule. What becomes a suggestion and its weight is
//! `search_es::query_suggestions`; docs/search/README.md, "Query suggestions", has the design.
//!
//! - **A build** reads the index's analytics over its lookback window (the catalog's rings plus
//!   what this node holds that it hasn't handed on), at most [`MAX_CANDIDATES`] queries charged
//!   to the cluster's `request` breaker, picks the suggestions, and writes them to a new index
//!   (`<alias>-<ms>`), refreshes it and moves the alias to it in one `_aliases` call that also
//!   removes the previous build, so readers see the old suggestions or the new, never half.
//!   When the suggestions come out the same as the last build's ([`qs::digest`]), nothing is
//!   written.
//! - **The schedule**: the catalog row of each index (`search_suggestions`) says when it is next
//!   due. The metering pass builds what's due on the clusters loaded here (and right away when
//!   the settings changed). A sleeping cluster is checked from the catalog alone: it is woken
//!   only when the build would change something, so an idle cluster with nothing new pays
//!   nothing ([`Control::wake_for_suggestions`]). Unloading records the settings the cluster
//!   leaves with ([`Control::note_suggestions`]).
//! - **Privacy**: a suggestions index whose source is deleted, replaced, or has analytics off is
//!   deleted on the next pass, as analytics delete what they kept; its catalog row goes with the
//!   analytics' rows (`Store::record_analytics`).

use crate::control::{Control, ControlError, Result, now, spec_for};
use crate::meter::now_ms;
use crate::store::{SearchCluster, SuggestionsRow};
use search_es::EsRequest;
use search_es::analytics::{self, Bucket};
use search_es::meta::{ClusterState, IndexMeta};
use search_es::query_suggestions::{self as qs, Config, Suggestion};
use search_server::node::Loaded;
use serde_json::{Value, json};
use std::sync::Arc;

/// Queries a build weighs at most: more than the catalog's hourly ring holds for an index over
/// 7 days (168 × 100) or its daily ring over 30 (31 × 300).
pub const MAX_CANDIDATES: usize = 20_000;
/// Suggestions of each build the catalog keeps for the dashboard.
pub const SAMPLE: usize = 50;
/// Documents per bulk request of a build.
const BULK: usize = 1_000;
/// A sleeping cluster whose build failed before recording why tries again after this (ms).
const RETRY_MS: i64 = 600_000;

/// An index with analytics and suggestions on.
struct Wanted {
    index: String,
    cfg: Config,
}

fn wanted(st: &ClusterState) -> Vec<Wanted> {
    st.metadata.indices.values().filter(|i| analytics::enabled(i)).filter_map(|i| Config::of(i).map(|cfg| Wanted { index: i.name.clone(), cfg })).collect()
}

/// Suggestions indices roll built whose source is gone, was replaced, or has analytics off.
fn orphaned(st: &ClusterState) -> Vec<String> {
    let ix = &st.metadata.indices;
    ix.values()
        .filter(|i| {
            let Some(source) = i.settings.get(qs::SOURCE) else { return false };
            !ix.get(source).is_some_and(|s| analytics::enabled(s) && qs::built_from(i, s))
        })
        .map(|i| i.name.clone())
        .collect()
}

/// The built index the alias `target` points at, when it is one roll built from `src`: its
/// name and creation time (ms).
fn live_build(st: &ClusterState, target: &str, src: &IndexMeta) -> Option<(String, i64)> {
    let behind: Vec<&Arc<IndexMeta>> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(target)).collect();
    match behind.as_slice() {
        [one] if qs::built_from(one, src) => Some((one.name.clone(), one.creation_date())),
        _ => None,
    }
}

fn sample_json(list: &[Suggestion]) -> String {
    let v: Vec<Value> = list.iter().take(SAMPLE).map(|s| json!({ "query": s.text, "weight": s.weight, "searches": s.searches, "clicks": s.clicks })).collect();
    Value::Array(v).to_string()
}

/// What a build wrote.
struct Written {
    changed: bool,
    digest: String,
    index: String,
    list: Vec<Suggestion>,
}

fn reason(v: &Value) -> String {
    v.pointer("/error/reason").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.to_string().chars().take(300).collect())
}

impl Control {
    async fn suggest_lock(&self, id: &str, index: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let m = self.suggest_locks.lock().entry(format!("{id}\0{index}")).or_default().clone();
        m.lock_owned().await
    }

    /// One request on the cluster as its superuser; an answer over 299 is an error naming `what`.
    async fn suggest_call(&self, c: &SearchCluster, what: &str, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<Value> {
        let mut req = EsRequest::new(method, path);
        if let Some((ct, b)) = body {
            req.headers.push(("Content-Type".into(), ct.into()));
            req.body = b.into();
        }
        let resp = Box::pin(self.call(c, req)).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if resp.status >= 300 {
            return Err(ControlError::Conflict(format!("{what} failed ({}): {}", resp.status, reason(&v))));
        }
        Ok(v)
    }

    /// Index `index`'s analytics over the window `cfg` looks back: the catalog's buckets plus
    /// what this node holds that it hasn't handed on, at most [`MAX_CANDIDATES`] queries.
    async fn suggestion_inputs(&self, c: &SearchCluster, index: &str, cfg: &Config) -> Result<Bucket> {
        let res: i64 = if cfg.lookback_ms <= 7 * 86_400_000 { 3600 } else { 86_400 };
        let from = {
            let t = now() - cfg.lookback_ms / 1000;
            t - t.rem_euclid(res) + res
        };
        let pending = self.node.peek_observations(&c.name, &c.id).analytics;
        // Turned off (or replaced) and not handed on yet: the catalog's rows are going.
        let gone = pending.cleared.iter().any(|i| i == index) || pending.enabled.as_ref().is_some_and(|on| !on.iter().any(|o| o == index));
        let mut total = Bucket::new(MAX_CANDIDATES, 0);
        if !gone {
            for (_, _, b) in self.store.analytics_since(&c.id, Some(index), res, from).await? {
                total.merge(&b);
            }
        }
        for (i, hour, b) in &pending.hours {
            if i == index && (*hour as i64).saturating_mul(3600) >= from {
                total.merge(b);
            }
        }
        Ok(total)
    }

    /// Builds index `index`'s suggestions on cluster `c` now (loading it), unless they'd come out
    /// as the last build's. `manual`: `POST …/suggestions/rebuild`; the schedule instead adopts
    /// a build the catalog has no row of (a branch's copy of its parent's) until it is due.
    /// Records the outcome in the catalog either way.
    pub(crate) async fn build_suggestions(&self, c: &SearchCluster, index: &str, manual: bool) -> Result<Value> {
        let _one_at_a_time = self.suggest_lock(&c.id, index).await;
        let l = self.node.open(spec_for(c)).await.map_err(|e| ControlError::Internal(anyhow::anyhow!("loading search cluster {} failed: {e}", c.name)))?;
        let st = l.cluster.state();
        let row = self.store.suggestion(&c.id, index).await?;
        let Some(src) = st.metadata.indices.get(index).cloned() else {
            if row.is_some() {
                self.store.delete_suggestion(&c.id, index).await?;
            }
            return Err(ControlError::NotFound(format!("no such index [{index}] (name the index itself, not an alias)")));
        };
        let cfg = match (analytics::enabled(&src), Config::of(&src)) {
            (true, Some(cfg)) => cfg,
            (on, _) => {
                if row.is_some() {
                    self.store.delete_suggestion(&c.id, index).await?;
                }
                return Err(ControlError::Invalid(if on {
                    format!("query suggestions are off for [{index}]: turn them on with {}", qs::ENABLED)
                } else {
                    format!("search analytics are off for [{index}], and suggestions are built from them: turn them on with {}", analytics::SETTING)
                }));
            }
        };
        let started = now_ms();
        let mut next = row.clone().unwrap_or_else(|| SuggestionsRow { index: index.to_string(), sample: "[]".into(), ..Default::default() });
        next.config = cfg.to_json().to_string();
        next.due_at = started.saturating_add(cfg.interval_ms);
        if row.is_none()
            && !manual
            && let Some((built, created)) = live_build(&st, &cfg.index, &src)
        {
            next.built_index = Some(built);
            next.built_at = Some(created);
            next.due_at = created.saturating_add(cfg.interval_ms);
            self.store.put_suggestion(&c.id, &next).await?;
            return Ok(status_json(index, Some(&next), Some((true, true)), &cfg));
        }
        next.attempted_at = Some(started);
        let t0 = std::time::Instant::now();
        match Box::pin(self.write_suggestions(c, &l, &st, &src, &cfg, row.as_ref(), started)).await {
            Ok(w) => {
                next.error = None;
                next.digest = Some(w.digest);
                if w.changed {
                    next.built_at = Some(started);
                    next.built_index = Some(w.index);
                    next.count = w.list.len() as i64;
                    next.took_ms = t0.elapsed().as_millis().min(i64::MAX as u128) as i64;
                    next.sample = sample_json(&w.list);
                }
                self.store.put_suggestion(&c.id, &next).await?;
                let mut v = status_json(index, Some(&next), Some((true, true)), &cfg);
                v["changed"] = w.changed.into();
                Ok(v)
            }
            Err(e) => {
                next.error = Some(e.to_string());
                self.store.put_suggestion(&c.id, &next).await?;
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_suggestions(&self, c: &SearchCluster, l: &Loaded, st: &ClusterState, src: &IndexMeta, cfg: &Config, row: Option<&SuggestionsRow>, started: i64) -> Result<Written> {
        let total = self.suggestion_inputs(c, &src.name, cfg).await?;
        // The queries, the suggestions picked from them and a bulk request's worth of documents.
        let mut held = search_es::breaker::Reservation::new(l.cluster.breakers.request(&l.cluster, st));
        let bytes: usize = total.top.iter().map(|(q, _)| q.len() + 96).sum::<usize>() * 3;
        held.add(bytes as u64, "<roll_query_suggestions>").map_err(|e| ControlError::Conflict(e.reason().to_string()))?;
        let list = qs::plan(&total.top, total.clicks > 0, cfg);
        drop(total);
        let digest = qs::digest(&cfg.index, &list);
        // The alias is always `<index>-suggestions`. Whatever already holds that name and isn't
        // roll's build from this very index (its uuid in the build's creation-only settings) is
        // never touched: the build stops before writing anything.
        let target = cfg.index.as_str();
        let refuse = |what: String| ControlError::Conflict(format!("{what}, so [{}]'s suggestions can't be built there: rename or delete it", src.name));
        if st.metadata.indices.contains_key(target) {
            return Err(refuse(format!("[{target}] is an index roll didn't build")));
        }
        if st.metadata.data_streams.contains_key(target) || st.metadata.data_stream_aliases.contains_key(target) {
            return Err(refuse(format!("[{target}] is a data stream or a data stream's alias")));
        }
        let behind: Vec<&Arc<IndexMeta>> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(target)).collect();
        if let Some(other) = behind.iter().find(|i| !qs::built_from(i, src)) {
            return Err(refuse(format!("the alias [{target}] points at [{}], which roll didn't build from [{}]", other.name, src.name)));
        }
        if let Some(r) = row
            && r.digest.as_deref() == Some(digest.as_str())
            && let [one] = behind.as_slice()
            && r.built_index.as_deref() == Some(one.name.as_str())
        {
            return Ok(Written { changed: false, digest, index: one.name.clone(), list });
        }
        let mut name = format!("{target}-{started}");
        while st.metadata.indices.contains_key(&name) {
            name.push('0');
        }
        if name.len() > 255 {
            return Err(ControlError::Invalid(format!("[{}] is too long a name to build suggestions for ([{target}-<ms>] would be over 255 bytes)", src.name)));
        }
        // Only roll's earlier builds from this index go: each carries its uuid from creation.
        let previous: Vec<String> = st.metadata.indices.values().filter(|i| qs::built_from(i, src)).map(|i| i.name.clone()).collect();
        // Created and marked in one step in process: the mark is private, so no request (this
        // node's own included) can set it.
        Box::pin(qs::create_build(&l.cluster, &name, src)).await.map_err(|e| ControlError::Conflict(format!("creating the suggestions index failed: {}", e.reason())))?;
        let path = format!("/{}", urlencoding::encode(&name));
        let filled = async {
            for chunk in list.chunks(BULK) {
                let mut body = String::new();
                for s in chunk {
                    body.push_str(&json!({ "index": { "_id": s.text } }).to_string());
                    body.push('\n');
                    body.push_str(&qs::document(s).to_string());
                    body.push('\n');
                }
                let v = self.suggest_call(c, "writing suggestions", "POST", &format!("{path}/_bulk"), Some(("application/x-ndjson", body.into_bytes()))).await?;
                if v["errors"] == true {
                    let first = v["items"].as_array().into_iter().flatten().find_map(|i| i["index"]["error"]["reason"].as_str()).unwrap_or("unknown");
                    return Err(ControlError::Conflict(format!("writing suggestions failed: {first}")));
                }
            }
            self.suggest_call(c, "refreshing the suggestions index", "POST", &format!("{path}/_refresh"), None).await?;
            let mut actions = vec![json!({ "add": { "index": name, "alias": target } })];
            actions.extend(previous.iter().map(|p| json!({ "remove_index": { "index": p } })));
            self.suggest_call(c, "moving the alias", "POST", "/_aliases", Some(("application/json", json!({ "actions": actions }).to_string().into_bytes()))).await?;
            Ok(())
        };
        if let Err(e) = Box::pin(filled).await {
            // Readers never saw it: the alias moves last.
            if let Err(d) = self.suggest_call(c, "removing the unfinished build", "DELETE", &path, None).await {
                tracing::warn!(cluster = c.name, index = name, error = %d, "removing an unfinished suggestions build failed");
            }
            return Err(e);
        }
        Ok(Written { changed: true, digest, index: name, list })
    }

    /// The metering pass's part, for the clusters loaded here: deletes suggestions indices whose
    /// source lost its analytics, and builds what's due (or whose settings changed).
    pub(crate) async fn run_suggestions(&self) {
        for name in self.node.loaded() {
            let Some(l) = self.node.get_loaded(&name) else { continue };
            let st = l.cluster.state();
            drop(l);
            let (wanted, orphans) = (wanted(&st), orphaned(&st));
            if (wanted.is_empty() && orphans.is_empty()) || self.wakes.is_deleting(&name) {
                continue;
            }
            let Ok(c) = self.get(&name).await else { continue };
            for o in orphans {
                let path = format!("/{}", urlencoding::encode(&o));
                if let Err(e) = self.suggest_call(&c, "deleting suggestions whose source has no analytics", "DELETE", &path, None).await {
                    tracing::warn!(cluster = name, index = o, error = %e, "deleting an orphaned suggestions index failed");
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let rows = match self.store.suggestions(&c.id).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(cluster = name, error = %e, "reading query suggestion rows failed");
                    continue;
                }
            };
            let at = now_ms();
            for w in wanted {
                let row = rows.iter().find(|r| r.index == w.index);
                let config = w.cfg.to_json().to_string();
                if row.is_none_or(|r| r.due_at <= at || r.config != config)
                    && let Err(e) = Box::pin(self.build_suggestions(&c, &w.index, false)).await
                {
                    tracing::info!(cluster = name, index = w.index, error = %e, "building query suggestions failed");
                }
            }
        }
    }

    /// Checks the due suggestions of sleeping clusters from the catalog alone, and wakes a
    /// cluster only when its build would change something (`job_wake_interval`, with the other
    /// background jobs). The rest are just due again an interval on.
    pub(crate) async fn wake_for_suggestions(&self) {
        let due = match self.store.due_suggestions(now_ms()).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "listing due query suggestions failed");
                return;
            }
        };
        for (id, row) in due {
            let Ok(c) = self.get(&id).await else { continue };
            // Loaded here, the metering pass builds it; elsewhere, its owner does.
            if self.node.get_loaded(&c.name).is_some() || self.wakes.is_deleting(&c.name) || !self.wakes_here(&c.name) {
                continue;
            }
            let at = now_ms();
            let cfg = serde_json::from_str::<Value>(&row.config).ok().and_then(|v| Config::from_json(&v));
            if let Some(cfg) = &cfg
                && row.digest.is_some()
                && let Ok(total) = self.suggestion_inputs(&c, &row.index, cfg).await
                && Some(qs::digest(&cfg.index, &qs::plan(&total.top, total.clicks > 0, cfg))) == row.digest
            {
                if let Err(e) = self.store.set_suggestion_due(&c.id, &row.index, at.saturating_add(cfg.interval_ms), Some(at)).await {
                    tracing::warn!(cluster = c.name, error = %e, "recording a query suggestions check failed");
                }
                continue;
            }
            if let Err(e) = Box::pin(self.build_suggestions(&c, &row.index, false)).await {
                tracing::info!(cluster = c.name, index = row.index, error = %e, "building query suggestions of a sleeping cluster failed");
                // A build that failed before recording why (the cluster didn't load) waits too.
                if let Ok(Some(r)) = self.store.suggestion(&c.id, &row.index).await
                    && r.due_at <= at
                {
                    let _ = self.store.set_suggestion_due(&c.id, &row.index, at + RETRY_MS, None).await;
                }
            }
        }
    }

    /// As a cluster unloads: records the suggestion settings it leaves with, so the catalog
    /// alone can say when it is due. A changed setting is due at once; an index that turned them
    /// off loses its row.
    pub(crate) async fn note_suggestions(&self, name: &str, st: &ClusterState) {
        let wanted = wanted(st);
        let Ok(c) = self.get(name).await else { return };
        let rows = match self.store.suggestions(&c.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(cluster = name, error = %e, "reading query suggestion rows failed");
                return;
            }
        };
        let at = now_ms();
        for r in rows.iter().filter(|r| !wanted.iter().any(|w| w.index == r.index)) {
            let _ = self.store.delete_suggestion(&c.id, &r.index).await;
        }
        for w in &wanted {
            let config = w.cfg.to_json().to_string();
            let row = match rows.iter().find(|r| r.index == w.index) {
                Some(r) if r.config == config => continue,
                Some(r) => SuggestionsRow { config, due_at: at, ..r.clone() },
                None => SuggestionsRow { index: w.index.clone(), config, due_at: at, sample: "[]".into(), ..Default::default() },
            };
            if let Err(e) = self.store.put_suggestion(&c.id, &row).await {
                tracing::warn!(cluster = name, error = %e, "recording query suggestion settings failed");
            }
        }
    }

    /// `GET …/indices/{index}/suggestions`: the index's settings, last build and its heaviest
    /// suggestions. Doesn't wake the cluster: asleep, the settings are the ones it left with.
    pub async fn suggestions(&self, id: &str, index: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let row = self.store.suggestion(&c.id, index).await?;
        let live = self.node.get_loaded(&c.name).and_then(|l| l.cluster.state().metadata.indices.get(index).cloned());
        if let Some(i) = &live {
            return Ok(status_json(index, row.as_ref(), Some((analytics::enabled(i), qs::enabled(i))), &Config::read(i)));
        }
        let kept = row.as_ref().and_then(|r| serde_json::from_str::<Value>(&r.config).ok()).and_then(|v| Config::from_json(&v));
        let on = kept.is_some().then_some((true, true));
        Ok(status_json(index, row.as_ref(), on, &kept.unwrap_or_else(|| default_config(index))))
    }

    /// `POST …/indices/{index}/suggestions/rebuild`.
    pub async fn rebuild_suggestions(&self, id: &str, index: &str) -> Result<Value> {
        let c = self.get(id).await?;
        Box::pin(self.build_suggestions(&c, index, true)).await
    }
}

fn default_config(index: &str) -> Config {
    Config {
        index: qs::default_target(index),
        min_count: qs::DEFAULT_MIN_COUNT,
        min_hits: qs::DEFAULT_MIN_HITS,
        lookback_ms: qs::DEFAULT_LOOKBACK_MS,
        interval_ms: qs::DEFAULT_INTERVAL_MS,
        max: qs::DEFAULT_MAX,
        block: Vec::new(),
        block_prefix: Vec::new(),
        click_boost: qs::DEFAULT_CLICK_BOOST,
    }
}

/// The status answer. `on`: (analytics, suggestions) when known (`None`: a sleeping cluster with
/// no row, so whatever it last had is unknown here, and it isn't building).
fn status_json(index: &str, row: Option<&SuggestionsRow>, on: Option<(bool, bool)>, cfg: &Config) -> Value {
    let last = row.filter(|r| r.built_at.is_some() || r.error.is_some()).map(|r| {
        json!({
            "at": r.built_at,
            "index": r.built_index,
            "count": r.count,
            "took_ms": r.took_ms,
            "checked_at": r.attempted_at,
            "error": r.error,
        })
    });
    let building = on.is_some_and(|(a, s)| a && s);
    json!({
        "index": index,
        "analytics": on.map(|(a, _)| a),
        "enabled": on.is_some_and(|(_, s)| s),
        "config": cfg.to_json(),
        "last_build": last,
        "next_at": row.filter(|_| building).map(|r| r.due_at),
        "suggestions": row.and_then(|r| serde_json::from_str::<Value>(&r.sample).ok()).unwrap_or_else(|| json!([])),
    })
}
