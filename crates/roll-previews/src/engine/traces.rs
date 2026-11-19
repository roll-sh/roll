//! A preview's traces (Vercel's preview traces, roll apps' `traces`): with the project's
//! `traces: true`, its `roll.toml` services get the OpenTelemetry SDK's own variables
//! (`OTEL_EXPORTER_OTLP_ENDPOINT` to `…/v1/previews/{id}/otlp/{token}` on rolld), so an app with an
//! SDK sends its spans with no setup. They're kept on the preview's holder, in memory: the newest
//! [`MAX_SPANS`] a preview within [`MAX_BYTES`], for [`KEEP_MS`], and at most [`NODE_BYTES`] a node
//! (past it, the largest preview's oldest go first): a preview's traces are for looking at it now,
//! not for the record, and its code (a fork's once approved) holds the token. Read back masked as its logs are: a span's name and attributes are its code's words
//! and can carry what it was given.

use super::Engine;
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

/// Spans kept a preview, the newest, and their bytes at most.
pub const MAX_SPANS: usize = 20_000;
pub const MAX_BYTES: usize = 16 << 20;
/// Every preview's spans on a node, at most.
pub const NODE_BYTES: usize = 256 << 20;
/// How long a span is kept.
pub const KEEP_MS: i64 = 6 * 3_600_000;

/// A span, as previews keep it (roll apps' `otlp::Span`, read by rolld).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    /// Hex, 32 characters.
    pub trace: String,
    /// Hex, 16 characters.
    pub span: String,
    /// Empty for a root span.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// Unix nanoseconds.
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<(String, String)>,
}

/// A trace as its list says it: its id, its root span's name and service, when (ms), how long,
/// how many spans, whether any failed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraceRow {
    pub trace: String,
    pub name: String,
    #[serde(default)]
    pub service: String,
    pub start_ms: u64,
    pub ms: u64,
    pub spans: usize,
    #[serde(default)]
    pub error: bool,
}

/// A preview's spans, oldest first, each with its size, and their bytes.
#[derive(Default)]
pub(crate) struct PreviewSpans {
    list: VecDeque<(Span, usize)>,
    bytes: usize,
}

impl PreviewSpans {
    fn pop(&mut self) {
        if let Some((_, b)) = self.list.pop_front() {
            self.bytes -= b;
        }
    }
}

/// Every preview's spans on this node.
pub(crate) type Kept = HashMap<String, PreviewSpans>;

/// What a span holds, in bytes (its strings, and a little for the rest).
fn size(s: &Span) -> usize {
    64 + s.trace.len() + s.span.len() + s.parent.len() + s.name.len() + s.service.len() + s.attrs.iter().map(|(k, v)| 16 + k.len() + v.len()).sum::<usize>()
}

/// A route's times: how many traces, their p50 and p95 (ms).
#[derive(Clone, Debug, PartialEq)]
pub struct RouteStat {
    pub name: String,
    pub count: usize,
    pub p50: u64,
    pub p95: u64,
}

/// Each route's (a trace's root span's name) count, p50 and p95, the busiest first.
pub fn route_stats(rows: &[TraceRow]) -> Vec<RouteStat> {
    let mut by: HashMap<&str, Vec<u64>> = HashMap::new();
    for r in rows {
        by.entry(r.name.as_str()).or_default().push(r.ms);
    }
    let pct = |v: &[u64], p: usize| v[((v.len() - 1) * p) / 100];
    let mut out: Vec<RouteStat> = by
        .into_iter()
        .map(|(name, mut ms)| {
            ms.sort_unstable();
            RouteStat { name: name.to_string(), count: ms.len(), p50: pct(&ms, 50), p95: pct(&ms, 95) }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

/// A route against main's same route.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteCompared {
    pub name: String,
    pub count: usize,
    pub p50: u64,
    pub p95: u64,
    pub main_p50: Option<u64>,
    pub main_count: usize,
    /// Clearly slower than main's: at least [`MIN_SAMPLES`] traces on both sides, its p50 at least
    /// half again main's and [`SLOWER_MS`] more (the noise a few requests make otherwise).
    pub slower: bool,
}

pub const MIN_SAMPLES: usize = 5;
/// How often the holder reads its previews' slower routes onto their records.
pub const ROUTES_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
pub const SLOWER_MS: u64 = 100;

/// Each of the preview's routes with main's same one; the slower ones first.
pub fn compare_routes(mine: &[RouteStat], main: &[RouteStat]) -> Vec<RouteCompared> {
    let mut out: Vec<RouteCompared> = mine
        .iter()
        .map(|m| {
            let b = main.iter().find(|b| b.name == m.name);
            let slower = b.is_some_and(|b| m.count >= MIN_SAMPLES && b.count >= MIN_SAMPLES && m.p50 >= b.p50 + SLOWER_MS && m.p50 * 2 >= b.p50 * 3);
            RouteCompared { name: m.name.clone(), count: m.count, p50: m.p50, p95: m.p95, main_p50: b.map(|b| b.p50), main_count: b.map_or(0, |b| b.count), slower }
        })
        .collect();
    out.sort_by_key(|c| !c.slower);
    out
}

impl Engine {
    /// The OpenTelemetry SDK's variables a service of a preview with traces on gets: the intake
    /// on rolld (as containers reach it), its token in the path.
    pub(super) fn otlp_vars(&self, preview: &str, service: &str) -> Result<Option<Vec<(&'static str, String)>>> {
        let Some(addr) = &self.cfg.otlp else { return Ok(None) };
        let token = self.store.trace_token(preview)?;
        let url = format!("http://{addr}/v1/previews/{preview}/otlp/{token}");
        Ok(Some(vec![
            ("OTEL_EXPORTER_OTLP_ENDPOINT", url),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".into()),
            ("OTEL_TRACES_EXPORTER", "otlp".into()),
            // Traces only: metrics and logs have no home here, and the logs are its output already.
            ("OTEL_METRICS_EXPORTER", "none".into()),
            ("OTEL_LOGS_EXPORTER", "none".into()),
            ("OTEL_SERVICE_NAME", service.to_string()),
        ]))
    }

    /// Whether `token` is a live preview's intake token (else a 404, as nothing is there): checked
    /// before a request's body is read.
    pub fn check_trace_token(&self, id: &str, token: &str) -> Result<()> {
        let r = self.store.row(id).map_err(|_| Error::NotFound("no such trace address".into()))?;
        if r.desired != crate::Desired::Active || !crate::runtime::same(&self.store.trace_token(id)?, token) {
            return Err(Error::NotFound("no such trace address".into()));
        }
        Ok(())
    }

    /// `POST …/{id}/otlp/{token}/v1/traces`: spans a preview's service sent, kept when the token is
    /// the preview's. How many were kept.
    pub fn keep_spans(&self, id: &str, token: &str, spans: Vec<Span>) -> Result<usize> {
        self.check_trace_token(id, token)?;
        let now_ns = (self.store.now_ms()?.max(0) as u64) * 1_000_000;
        let n = spans.len();
        let cut = now_ns.saturating_sub(KEEP_MS as u64 * 1_000_000);
        let mut kept = self.traces.lock();
        let mine = kept.entry(id.to_string()).or_default();
        for s in spans {
            let b = size(&s);
            mine.bytes += b;
            mine.list.push_back((s, b));
        }
        while mine.list.front().is_some_and(|(s, _)| s.end < cut) || mine.list.len() > MAX_SPANS || mine.bytes > MAX_BYTES {
            mine.pop();
        }
        // The node's budget: the largest preview's oldest go first.
        while kept.values().map(|p| p.bytes).sum::<usize>() > NODE_BYTES {
            let Some(biggest) = kept.values_mut().max_by_key(|p| p.bytes) else { break };
            biggest.pop();
        }
        kept.retain(|_, p| !p.list.is_empty());
        Ok(n)
    }

    /// `GET …/{id}/traces?minutes=`: its recent traces, newest first (at most 100): each one's root
    /// span's name and service, when, how long, how many spans, whether any failed. Masked.
    pub async fn traces(&self, id: &str, minutes: i64) -> Result<Value> {
        let minutes = minutes.clamp(5, KEEP_MS / 60_000);
        let mut out = self.trace_rows(id, minutes).await?;
        out.truncate(100);
        let names = self.masked(id, out.iter().map(|o| o.name.clone()).collect()).await?;
        for (o, name) in out.iter_mut().zip(names) {
            o.name = name;
        }
        Ok(json!({ "minutes": minutes, "traces": out }))
    }

    /// Every trace of the preview in the last `minutes`, newest first, unmasked: the runtime's kept
    /// spans summed, and its roll apps services' own while the project's traces are on.
    async fn trace_rows(&self, id: &str, minutes: i64) -> Result<Vec<TraceRow>> {
        self.store.row(id)?;
        let since = (self.store.now_ms()? - minutes * 60_000).max(0) as u64 * 1_000_000;
        // Summed under the lock without copying the spans (only each trace's row leaves it).
        let mut out: Vec<TraceRow> = {
            let kept = self.traces.lock();
            let mut by: HashMap<&str, Vec<&Span>> = HashMap::new();
            for (s, _) in kept.get(id).map(|p| p.list.iter()).into_iter().flatten().filter(|(s, _)| s.end >= since) {
                by.entry(s.trace.as_str()).or_default().push(s);
            }
            by.into_iter()
                .filter_map(|(t, ss)| {
                    let start = ss.iter().map(|s| s.start).min()?;
                    let end = ss.iter().map(|s| s.end).max()?;
                    let root = ss.iter().find(|s| s.parent.is_empty()).or_else(|| ss.iter().min_by_key(|s| s.start))?;
                    Some(TraceRow { trace: t.to_string(), name: root.name.clone(), service: root.service.clone(), start_ms: start / 1_000_000, ms: end.saturating_sub(start) / 1_000_000, spans: ss.len(), error: ss.iter().any(|s| s.error) })
                })
                .collect()
        };
        // A service on roll apps keeps its own (its `pv-` environment's), each once; only while the
        // project's traces are on (turned off, what roll apps kept isn't the preview's to show).
        let on = self.store.settings(&self.store.row(id)?.project_id).map(|s| s.traces).unwrap_or(false);
        for svc in self.store.services(id)?.into_iter().filter(|_| on) {
            for row in self.apps.traces(id, &svc.name, minutes).await {
                if !out.iter().any(|o| o.trace == row.trace) {
                    out.push(row);
                }
            }
        }
        out.sort_by_key(|o| std::cmp::Reverse(o.start_ms));
        Ok(out)
    }

    /// `GET …/{id}/routes?minutes=`: each route's time from its traces (a root span's name, as an
    /// SDK names a request by its route), against the same route on main's preview (the repo's
    /// default branch's, else the project's baseline), the slower ones said (Vercel's
    /// Observability, Sentry's performance). Names masked.
    pub async fn routes(&self, id: &str, minutes: i64) -> Result<Value> {
        let minutes = minutes.clamp(5, KEEP_MS / 60_000);
        let r = self.store.row(id)?;
        let mine = route_stats(&self.trace_rows(id, minutes).await?);
        let base = self.main_preview(&r).await?;
        let main = match &base {
            Some(b) => route_stats(&self.trace_rows(&b.id, minutes).await?),
            None => Vec::new(),
        };
        let mut compared = compare_routes(&mine, &main);
        if !compared.is_empty() {
            let names = self.masked(id, compared.iter().map(|c| c.name.clone()).collect()).await?;
            for (c, name) in compared.iter_mut().zip(names) {
                c.name = name;
            }
        }
        let routes: Vec<Value> = compared
            .into_iter()
            .map(|c| json!({ "name": c.name, "count": c.count, "p50_ms": c.p50, "p95_ms": c.p95, "main_p50_ms": c.main_p50, "main_count": c.main_count, "slower": c.slower }))
            .collect();
        Ok(json!({ "minutes": minutes, "main": base.map(|b| b.id), "routes": routes }))
    }

    /// Main's preview, as a change's screenshot finds it: the repo's default branch's, else the
    /// project's baseline.
    pub(super) async fn main_preview(&self, r: &crate::store::Row) -> Result<Option<crate::store::Row>> {
        Ok(self.main_of(&r.project_id, &r.repo).await?.filter(|b| b.id != r.id))
    }

    /// A project's preview of a repo's default branch, else its baseline (main's preview itself
    /// included: who asks may be it).
    async fn main_of(&self, project: &str, repo: &str) -> Result<Option<crate::store::Row>> {
        let rows: Vec<crate::store::Row> = self.store.rows(Some(project))?.into_iter().filter(|x| x.change_id.is_none() && x.desired == crate::Desired::Active).collect();
        let default = self.source.default_branch(repo).await.ok().flatten();
        Ok(rows.iter().find(|x| x.repo == repo && default.is_some() && x.branch == default).or_else(|| rows.iter().find(|x| x.is_baseline())).cloned())
    }

    /// Whether this node holds a preview (runs its services, keeps its spans): always on one node.
    pub(super) async fn holds(&self, id: &str) -> bool {
        match self.cluster.get() {
            Some(c) => matches!(c.route_async(&super::preview_key(id)).await, Ok(roll_cluster::Route::Local(_))),
            None => true,
        }
    }

    /// The previews this node holds, while their project keeps traces: their routes slower than
    /// main's kept on the record (the verdict strip, the pull request's comment), read at most
    /// once a [`ROUTES_EVERY`]; a change reports the preview again.
    pub(super) fn tend_routes(self: &std::sync::Arc<Self>) {
        {
            let mut last = self.routes_read.lock();
            if last.is_some_and(|at| at.elapsed() < ROUTES_EVERY) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        // Off the pass (a round reads every held preview's traces): one round at a time.
        if self.routes_running.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.read_slower_routes().await;
            me.routes_running.store(false, std::sync::atomic::Ordering::Release);
        });
    }

    /// What `tend_routes` does each [`ROUTES_EVERY`], now: main's routes read once for every
    /// preview compared with it, and only where its spans are (its holder: elsewhere nothing is
    /// said rather than everything); then the previews', a few at a time.
    pub async fn read_slower_routes(&self) {
        use futures::StreamExt;
        let Ok(rows) = self.store.rows(None) else { return };
        let mut bases: HashMap<(String, String), Option<crate::store::Row>> = HashMap::new();
        let mut mains: HashMap<String, Option<Vec<RouteStat>>> = HashMap::new();
        let mut work = Vec::new();
        for r in rows.into_iter().filter(|r| r.desired == crate::Desired::Active && (r.change_id.is_some() || r.branch.is_some())) {
            if !self.store.settings(&r.project_id).is_ok_and(|s| s.traces) || !self.holds(&r.id).await {
                continue;
            }
            let key = (r.project_id.clone(), r.repo.clone());
            if !bases.contains_key(&key) {
                let base = self.main_of(&r.project_id, &r.repo).await.ok().flatten();
                bases.insert(key.clone(), base);
            }
            let main = match bases[&key].clone().filter(|b| b.id != r.id) {
                Some(b) => {
                    if !mains.contains_key(&b.id) {
                        let stats = if self.holds(&b.id).await { self.trace_rows(&b.id, 60).await.ok().map(|rows| route_stats(&rows)) } else { None };
                        mains.insert(b.id.clone(), stats);
                    }
                    mains[&b.id].clone().map(|stats| (b, stats))
                }
                None => None,
            };
            work.push((r, main));
        }
        let kept: std::collections::HashSet<String> = work.iter().map(|(r, _)| r.id.clone()).collect();
        futures::stream::iter(work).for_each_concurrent(4, |(r, main)| self.read_routes_of(r, main)).await;
        self.routes_masked.lock().retain(|id, _| kept.contains(id));
    }

    /// One preview's slower routes against main's (`None`: nothing of main's to compare with).
    /// Only the slower few are masked, and only names not masked before (an audited read of its
    /// values); it's reported again only when which routes are slower moves, not their times.
    async fn read_routes_of(&self, r: crate::store::Row, main: Option<(crate::store::Row, Vec<RouteStat>)>) {
        let kept = match main {
            None => None,
            Some((base, main)) => {
                let Ok(rows) = self.trace_rows(&r.id, 60).await else { return };
                let mut slower: Vec<RouteCompared> = compare_routes(&route_stats(&rows), &main).into_iter().filter(|c| c.slower).collect();
                slower.sort_by_key(|c| std::cmp::Reverse(c.p50.saturating_sub(c.main_p50.unwrap_or_default())));
                slower.truncate(crate::SlowerRoutes::MAX);
                if slower.is_empty() {
                    None
                } else {
                    let known = self.routes_masked.lock().get(&r.id).cloned().unwrap_or_default();
                    let fresh: Vec<String> = slower.iter().map(|c| c.name.clone()).filter(|n| !known.contains_key(n)).collect();
                    let mut names = known;
                    if !fresh.is_empty() {
                        let Ok(masked) = self.masked(&r.id, fresh.clone()).await else { return };
                        names.extend(fresh.into_iter().zip(masked));
                    }
                    let routes = slower.iter().map(|c| crate::SlowerRoute { name: names.get(&c.name).cloned().unwrap_or_default(), count: c.count, p50_ms: c.p50, main_p50_ms: c.main_p50.unwrap_or_default() }).collect();
                    self.routes_masked.lock().insert(r.id.clone(), names);
                    Some(crate::SlowerRoutes { branch: base.branch.clone().unwrap_or_else(|| "main".into()), routes })
                }
            }
        };
        let names = |f: &Option<crate::SlowerRoutes>| {
            let mut n: Vec<String> = f.iter().flat_map(|s| s.routes.iter().map(|x| x.name.clone())).collect();
            n.sort();
            n
        };
        let before: Option<crate::SlowerRoutes> = self.store.runtime_fact(&r.id, "slower_routes").ok().flatten();
        let moved = names(&before) != names(&kept);
        if self.store.set_runtime_fact(&r.id, "slower_routes", kept.as_ref()).unwrap_or(false) && moved {
            self.report(r).await;
        }
    }

    /// `GET …/{id}/traces/{trace}`: one trace's spans, oldest first, names and attributes masked.
    pub async fn trace(&self, id: &str, trace: &str) -> Result<Value> {
        self.store.row(id)?;
        let trace = trace.to_ascii_lowercase();
        if trace.len() != 32 || !trace.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::BadRequest("a trace id is 32 hex characters".into()));
        }
        let mut spans: Vec<Span> = self.traces.lock().get(id).map(|p| p.list.iter().filter(|(s, _)| s.trace == trace).map(|(s, _)| s.clone()).collect()).unwrap_or_default();
        // Not the runtime's: one of its roll apps services' own.
        let on = self.store.settings(&self.store.row(id)?.project_id).map(|s| s.traces).unwrap_or(false);
        if spans.is_empty() && on {
            for svc in self.store.services(id)? {
                if let Some(found) = self.apps.trace(id, &svc.name, &trace).await.filter(|f| !f.is_empty()) {
                    spans = found;
                    break;
                }
            }
        }
        if spans.is_empty() {
            return Err(Error::NotFound(format!("no trace {trace} in this preview")));
        }
        spans.sort_by_key(|s| s.start);
        let values = self.masks(id).await?;
        for s in &mut spans {
            s.name = code_preview::runner::mask(&s.name, &values);
            for (_, v) in &mut s.attrs {
                *v = code_preview::runner::mask(v, &values);
            }
        }
        Ok(json!({ "trace": trace, "spans": spans }))
    }

    /// A preview that goes takes its traces with it.
    pub(crate) fn forget_traces(&self, id: &str) {
        self.traces.lock().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ms: u64) -> TraceRow {
        TraceRow { trace: String::new(), name: name.into(), service: String::new(), start_ms: 0, ms, spans: 1, error: false }
    }

    /// A route is slower than main's only past the noise: enough traces on both sides, half again
    /// main's p50 and 100 ms more; a route main hasn't seen isn't compared.
    #[test]
    fn a_route_is_slower_than_mains_only_past_the_noise() {
        let mine: Vec<TraceRow> = (0..10).map(|_| row("GET /checkout", 400)).chain((0..10).map(|_| row("GET /", 30))).chain((0..3).map(|_| row("GET /new", 900))).collect();
        let main: Vec<TraceRow> = (0..10).map(|_| row("GET /checkout", 200)).chain((0..10).map(|_| row("GET /", 15))).collect();
        let c = compare_routes(&route_stats(&mine), &route_stats(&main));
        let by = |n: &str| c.iter().find(|r| r.name == n).unwrap().clone();
        assert!(by("GET /checkout").slower, "400 ms against 200: slower");
        assert!(!by("GET /").slower, "30 ms against 15: twice, but 15 ms isn't past the noise");
        assert_eq!((by("GET /new").slower, by("GET /new").main_p50), (false, None), "main hasn't seen it");
        assert_eq!(c[0].name, "GET /checkout", "the slower ones first");
        assert_eq!((by("GET /checkout").p50, by("GET /checkout").p95), (400, 400));
    }
}
