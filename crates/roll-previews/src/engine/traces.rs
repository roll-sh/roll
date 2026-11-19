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
        self.store.row(id)?;
        let minutes = minutes.clamp(5, KEEP_MS / 60_000);
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
        out.truncate(100);
        let names = self.masked(id, out.iter().map(|o| o.name.clone()).collect()).await?;
        for (o, name) in out.iter_mut().zip(names) {
            o.name = name;
        }
        Ok(json!({ "minutes": minutes, "traces": out }))
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
