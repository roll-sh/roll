//! Traces (Vercel's Observability, Cloud Run's Cloud Trace, Heroku's OpenTelemetry): an
//! environment with `traces: true` gets the OpenTelemetry SDK's own variables
//! (`OTEL_EXPORTER_OTLP_ENDPOINT` to its address here, `OTEL_SERVICE_NAME`), so an app with an SDK
//! sends its spans to roll with no setup. They arrive at `POST /v1/apps/otlp/{service}/{env}/
//! {token}/v1/traces` (OTLP/HTTP, protobuf or JSON, gzip or not; the token in the path is the
//! credential, as a deploy hook's), on the service's owner, and are kept as the environment's
//! `trace:` log (a JSON line a span), with its retention. Read back as recent traces and as one
//! trace's spans.

use crate::otlp::Span;
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

/// At most this much a request (after gzip).
const MAX_BODY: u64 = 8 << 20;
/// Span lines one read looks at (newest first).
const MAX_LINES: usize = 100_000;

pub(crate) fn key(service: &str, env: &str) -> String {
    format!("trace:{service}/{env}")
}

fn metric_key(service: &str, env: &str) -> String {
    format!("metric:{service}/{env}")
}

/// One of OTLP's signals (`/v1/traces`, `/v1/logs`, `/v1/metrics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Logs,
    Metrics,
}

impl Signal {
    pub fn of(path: &str) -> Option<Signal> {
        match path {
            "traces" => Some(Signal::Traces),
            "logs" => Some(Signal::Logs),
            "metrics" => Some(Signal::Metrics),
            _ => None,
        }
    }
}

impl Apps {
    /// Spans an environment's instances sent (OTLP/HTTP): kept, how many. Anything that isn't the
    /// environment's live token is the same 404.
    pub async fn ingest_traces(self: &Arc<Self>, service: &str, env: &str, token: &str, json: bool, gzip: bool, body: &[u8]) -> Result<usize> {
        self.ingest_otlp(service, env, token, Signal::Traces, json, gzip, body).await
    }

    /// What an environment's instances sent on one of OTLP's signals: spans as its `trace:` log,
    /// log records as lines of its own log (JSON: `level`, `msg`, `trace_id`, `span_id`, its
    /// attributes; so the log's level and field filters read them), data points as its `metric:`
    /// log. How many were kept.
    #[allow(clippy::too_many_arguments)]
    pub async fn ingest_otlp(self: &Arc<Self>, service: &str, env: &str, token: &str, signal: Signal, json: bool, gzip: bool, body: &[u8]) -> Result<usize> {
        let (sid, e) = (service.to_string(), env.to_string());
        let kept = self.db(move |s| s.trace_token(&sid, &e)).await.ok().flatten();
        if !kept.is_some_and(|k| crate::public::same(&k, token)) {
            return Err(Error::NotFound("no such trace address".into()));
        }
        let mut raw = Vec::new();
        let body: &[u8] = if gzip {
            flate2::read::GzDecoder::new(body).take(MAX_BODY + 1).read_to_end(&mut raw).map_err(|e| Error::BadRequest(format!("its gzip: {e}")))?;
            if raw.len() as u64 > MAX_BODY {
                return Err(Error::TooLarge("traces are at most 8 MiB a request".into()));
            }
            &raw
        } else {
            body
        };
        let v: Option<Value> = if json { Some(serde_json::from_slice(body).map_err(|e| Error::BadRequest(format!("its JSON: {e}")))?) } else { None };
        let bad = |e: anyhow::Error| Error::BadRequest(e.to_string());
        match signal {
            Signal::Traces => {
                let spans = match &v {
                    Some(v) => crate::otlp::from_json(v),
                    None => crate::otlp::from_protobuf(body),
                }
                .map_err(bad)?;
                let sink = self.logs.sink(key(service, env), crate::logs::Stream::System);
                for s in &spans {
                    sink.line(serde_json::to_string(s).expect("a span serializes"));
                }
                Ok(spans.len())
            }
            Signal::Logs => {
                let records = match &v {
                    Some(v) => crate::otlp::logs_from_json(v),
                    None => crate::otlp::logs_from_protobuf(body),
                }
                .map_err(bad)?;
                let sink = self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::Stdout);
                for r in &records {
                    let mut line = serde_json::Map::new();
                    // Its attributes first, so roll's own names win.
                    for (k, val) in &r.attrs {
                        line.insert(k.clone(), json!(val));
                    }
                    if !r.level.is_empty() {
                        line.insert("level".into(), json!(r.level));
                    }
                    line.insert("msg".into(), json!(r.body));
                    if !r.trace.is_empty() {
                        line.insert("trace_id".into(), json!(r.trace));
                        line.insert("span_id".into(), json!(r.span));
                    }
                    if !r.service.is_empty() {
                        line.insert("service".into(), json!(r.service));
                    }
                    sink.line(Value::Object(line).to_string());
                }
                Ok(records.len())
            }
            Signal::Metrics => {
                let points = match &v {
                    Some(v) => crate::otlp::metrics_from_json(v),
                    None => crate::otlp::metrics_from_protobuf(body),
                }
                .map_err(bad)?;
                let sink = self.logs.sink(metric_key(service, env), crate::logs::Stream::System);
                for p in &points {
                    sink.line(serde_json::to_string(p).expect("a point serializes"));
                }
                Ok(points.len())
            }
        }
    }

    async fn spans(self: &Arc<Self>, service: &str, env: &str, since: i64, trace: Option<&str>) -> (Vec<Span>, bool) {
        let (lines, cut) = self.logs.read(&key(service, env), |l| trace.is_none_or(|t| l.text.contains(t)), Some(since), None, MAX_LINES).await;
        (lines.iter().filter_map(|l| serde_json::from_str::<Span>(&l.text).ok()).filter(|s| trace.is_none_or(|t| s.trace == t)).collect(), cut || lines.len() >= MAX_LINES)
    }

    /// `GET …/environments/{env}/traces?minutes=&min_ms=`: recent traces, newest first (at most
    /// 100): each one's root span (its name and service), duration, span count and whether any
    /// span failed. `min_ms` keeps the slow ones.
    pub async fn traces(self: &Arc<Self>, service: &str, env: &str, minutes: i64, min_ms: u64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 10_080);
        let (spans, cut) = self.spans(service, env, code_sql::now_ms() - minutes * 60_000, None).await;
        let mut by: HashMap<&str, Vec<&Span>> = HashMap::new();
        for s in &spans {
            by.entry(s.trace.as_str()).or_default().push(s);
        }
        let mut out: Vec<Value> = by
            .into_iter()
            .filter_map(|(t, ss)| {
                let start = ss.iter().map(|s| s.start).min()?;
                let end = ss.iter().map(|s| s.end).max()?;
                let root = ss.iter().find(|s| s.parent.is_empty()).or_else(|| ss.iter().min_by_key(|s| s.start))?;
                let ms = end.saturating_sub(start) / 1_000_000;
                (ms >= min_ms).then(|| json!({ "trace": t, "name": root.name, "service": root.service, "start_ms": start / 1_000_000, "ms": ms, "spans": ss.len(), "error": ss.iter().any(|s| s.error) }))
            })
            .collect();
        out.sort_by(|a, b| b["start_ms"].as_u64().cmp(&a["start_ms"].as_u64()));
        out.truncate(100);
        Ok(json!({ "minutes": minutes, "traces": out, "cut": cut }))
    }

    /// `GET …/environments/{env}/app-metrics?minutes=`: what the app's OpenTelemetry SDK sent
    /// as metrics (traces on), each series (a name and its attributes, at most 50, the busiest
    /// first) bucketed to at most 60 points: a gauge's last value, a counter's rate a second, a
    /// histogram's mean.
    pub async fn app_metrics(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 10_080);
        let now = code_sql::now_ms();
        let since = now - minutes * 60_000;
        let (lines, cut) = self.logs.read(&metric_key(service, env), |_| true, Some(since), None, MAX_LINES).await;
        let step_ms = (minutes * 60_000 / 60).max(60_000);
        let mut series: HashMap<String, (crate::otlp::Point, std::collections::BTreeMap<i64, crate::otlp::Point>, usize)> = HashMap::new();
        for l in &lines {
            let Ok(p) = serde_json::from_str::<crate::otlp::Point>(&l.text) else { continue };
            let key = format!("{}{}", p.name, p.attrs.iter().map(|(k, v)| format!("|{k}={v}")).collect::<String>());
            let t = if p.time > 0 { (p.time / 1_000_000) as i64 } else { l.t };
            let e = series.entry(key).or_insert_with(|| (p.clone(), Default::default(), 0));
            e.2 += 1;
            // The bucket's last point (lines are read newest first: keep the newest).
            let b = e.1.entry(t - t.rem_euclid(step_ms)).or_insert_with(|| p.clone());
            if p.time > b.time {
                *b = p.clone();
            }
            if p.time >= e.0.time {
                e.0 = p;
            }
        }
        let mut out: Vec<(usize, Value)> = series
            .into_values()
            .map(|(last, buckets, n)| {
                let shows = if last.counter { "per_second" } else if last.kind == "histogram" { "mean" } else { "value" };
                let mut points = Vec::new();
                let mut prev: Option<&crate::otlp::Point> = None;
                for (at, p) in &buckets {
                    let v = match shows {
                        // Between this bucket's last and the one before; a reset starts again.
                        "per_second" => prev.and_then(|q| (p.value >= q.value && p.time > q.time).then(|| (p.value - q.value) / ((p.time - q.time) as f64 / 1e9))),
                        "mean" => match (prev.and_then(|q| q.count.map(|c| (q.value, c))), p.count) {
                            (Some((qs, qc)), Some(c)) if c > qc => Some((p.value - qs) / (c - qc) as f64),
                            (_, Some(c)) if c > 0 => Some(p.value / c as f64),
                            _ => None,
                        },
                        _ => Some(p.value),
                    };
                    if let Some(v) = v.filter(|v| v.is_finite()) {
                        points.push(json!([at, v]));
                    }
                    prev = Some(p);
                }
                (n, json!({ "name": last.name, "kind": last.kind, "unit": last.unit, "attrs": last.attrs.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(), "shows": shows, "points": points }))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1["name"].as_str().cmp(&b.1["name"].as_str())));
        out.truncate(50);
        Ok(json!({ "minutes": minutes, "step_minutes": step_ms / 60_000, "series": out.into_iter().map(|(_, v)| v).collect::<Vec<_>>(), "cut": cut || lines.len() >= MAX_LINES }))
    }

    /// `GET …/environments/{env}/traces/{trace}`: its spans, in start order (the last week's).
    pub async fn trace(self: &Arc<Self>, service: &str, env: &str, trace: &str) -> Result<Value> {
        self.service_env(service, env).await?;
        let trace = trace.to_ascii_lowercase();
        if trace.len() != 32 || !trace.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::BadRequest("a trace id is 32 hex characters".into()));
        }
        let (mut spans, _) = self.spans(service, env, code_sql::now_ms() - 7 * 86_400_000, Some(&trace)).await;
        if spans.is_empty() {
            return Err(Error::NotFound(format!("no trace {trace} here in the last week")));
        }
        spans.sort_by_key(|s| s.start);
        Ok(json!({ "trace": trace, "spans": spans }))
    }
}
