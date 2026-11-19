//! A preview's traces (Vercel's preview traces, roll apps' `traces`): with the project's
//! `traces: true`, its `roll.toml` services get the OpenTelemetry SDK's own variables
//! (`OTEL_EXPORTER_OTLP_ENDPOINT` to `…/v1/previews/{id}/otlp/{token}` on rolld), so an app with an
//! SDK sends its spans with no setup. They're kept on the preview's holder, in memory: the newest
//! [`MAX_SPANS`] a preview, for [`KEEP_MS`] (a preview's traces are for looking at it now, not for
//! the record). Read back masked as its logs are: a span's name and attributes are its code's words
//! and can carry what it was given.

use super::Engine;
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

/// Spans kept a preview, the newest.
pub const MAX_SPANS: usize = 20_000;
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

/// A preview's spans, oldest first.
pub(crate) type Kept = HashMap<String, VecDeque<Span>>;

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

    /// `POST …/{id}/otlp/{token}/v1/traces`: spans a preview's service sent, kept when the token is
    /// the preview's (else a 404, as nothing is there). How many were kept.
    pub fn keep_spans(&self, id: &str, token: &str, spans: Vec<Span>) -> Result<usize> {
        let r = self.store.row(id).map_err(|_| Error::NotFound("no such trace address".into()))?;
        if r.desired != crate::Desired::Active || !crate::runtime::same(&self.store.trace_token(id)?, token) {
            return Err(Error::NotFound("no such trace address".into()));
        }
        let now_ns = (self.store.now_ms()?.max(0) as u64) * 1_000_000;
        let n = spans.len();
        let mut kept = self.traces.lock();
        let list = kept.entry(id.to_string()).or_default();
        list.extend(spans);
        let cut = now_ns.saturating_sub(KEEP_MS as u64 * 1_000_000);
        while list.front().is_some_and(|s| s.end < cut) || list.len() > MAX_SPANS {
            list.pop_front();
        }
        Ok(n)
    }

    /// `GET …/{id}/traces?minutes=`: its recent traces, newest first (at most 100): each one's root
    /// span's name and service, when, how long, how many spans, whether any failed. Masked.
    pub async fn traces(&self, id: &str, minutes: i64) -> Result<Value> {
        self.store.row(id)?;
        let minutes = minutes.clamp(5, KEEP_MS / 60_000);
        let since = (self.store.now_ms()? - minutes * 60_000).max(0) as u64 * 1_000_000;
        let spans: Vec<Span> = self.traces.lock().get(id).map(|l| l.iter().filter(|s| s.end >= since).cloned().collect()).unwrap_or_default();
        let mut by: HashMap<&str, Vec<&Span>> = HashMap::new();
        for s in &spans {
            by.entry(s.trace.as_str()).or_default().push(s);
        }
        let mut out: Vec<(u64, String, String, u64, usize, bool, String)> = by
            .into_iter()
            .filter_map(|(t, ss)| {
                let start = ss.iter().map(|s| s.start).min()?;
                let end = ss.iter().map(|s| s.end).max()?;
                let root = ss.iter().find(|s| s.parent.is_empty()).or_else(|| ss.iter().min_by_key(|s| s.start))?;
                Some((start, root.name.clone(), root.service.clone(), end.saturating_sub(start) / 1_000_000, ss.len(), ss.iter().any(|s| s.error), t.to_string()))
            })
            .collect();
        out.sort_by_key(|o| std::cmp::Reverse(o.0));
        out.truncate(100);
        let names = self.masked(id, out.iter().map(|o| o.1.clone()).collect()).await?;
        let traces: Vec<Value> = out
            .into_iter()
            .zip(names)
            .map(|((start, _, service, ms, n, error, trace), name)| json!({ "trace": trace, "name": name, "service": service, "start_ms": start / 1_000_000, "ms": ms, "spans": n, "error": error }))
            .collect();
        Ok(json!({ "minutes": minutes, "traces": traces }))
    }

    /// `GET …/{id}/traces/{trace}`: one trace's spans, oldest first, names and attributes masked.
    pub async fn trace(&self, id: &str, trace: &str) -> Result<Value> {
        self.store.row(id)?;
        let trace = trace.to_ascii_lowercase();
        if trace.len() != 32 || !trace.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::BadRequest("a trace id is 32 hex characters".into()));
        }
        let mut spans: Vec<Span> = self.traces.lock().get(id).map(|l| l.iter().filter(|s| s.trace == trace).cloned().collect()).unwrap_or_default();
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
