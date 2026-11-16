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

fn key(service: &str, env: &str) -> String {
    format!("trace:{service}/{env}")
}

impl Apps {
    /// Spans an environment's instances sent (OTLP/HTTP): kept, how many. Anything that isn't the
    /// environment's live token is the same 404.
    pub async fn ingest_traces(self: &Arc<Self>, service: &str, env: &str, token: &str, json: bool, gzip: bool, body: &[u8]) -> Result<usize> {
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
        let spans = if json {
            let v: Value = serde_json::from_slice(body).map_err(|e| Error::BadRequest(format!("its JSON: {e}")))?;
            crate::otlp::from_json(&v)
        } else {
            crate::otlp::from_protobuf(body)
        }
        .map_err(|e| Error::BadRequest(e.to_string()))?;
        let sink = self.logs.sink(key(service, env), crate::logs::Stream::System);
        for s in &spans {
            sink.line(serde_json::to_string(s).expect("a span serializes"));
        }
        Ok(spans.len())
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
