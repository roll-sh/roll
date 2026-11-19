//! The app's own Prometheus endpoint, scraped (Fly's `[metrics]`, Render's and Railway's
//! Prometheus scraping; docs/apps/README.md, "The app's own metrics"): with `run.metrics: {port?,
//! path?}` the owner GETs each running instance's endpoint every 15 s (never one that's
//! suspended or asleep: a scrape wakes nothing) and keeps its samples beside what an OpenTelemetry
//! SDK sends, so Metrics shows them the same way. The text exposition format is read with bounds:
//! 4 MiB an answer, 2,000 series a scrape, names and labels cut, histograms and summaries kept as
//! their sum and count.

use crate::otlp::Point;
use crate::{Apps, live::EnvLive};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How often each instance is scraped, in the reaper's seconds.
pub const EVERY_S: u64 = 15;
/// The most of an answer read.
const MAX_BODY: usize = 4 << 20;
/// The most series kept from one scrape.
const MAX_SERIES: usize = 2000;
const MAX_LABELS: usize = 16;

type Labels = Vec<(String, String)>;

/// A scrape's samples as points: a counter's (`# TYPE … counter`) a sum that counts up, a gauge's
/// or an untyped one's a gauge, a histogram's or summary's one point of its `_sum` and `_count`
/// (buckets and quantiles aren't kept). `instance` is added to each one's labels.
pub fn parse(text: &str, now_ns: u64, instance: &str) -> Vec<Point> {
    let mut types: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Point> = Vec::new();
    // A histogram's or summary's point, by name and labels, while its _sum and _count come in.
    let mut agg: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            if let Some((name, t)) = rest.split_once(' ') {
                types.insert(name.to_string(), t.trim().to_string());
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, labels, value)) = sample(line) else { continue };
        if !value.is_finite() {
            continue;
        }
        let mut attrs: Vec<(String, String)> = labels.into_iter().filter(|(k, _)| k != "instance").collect();
        attrs.push(("instance".into(), instance.to_string()));
        // A histogram's or summary's parts: its family's type says so.
        let family = ["_sum", "_count", "_bucket"].iter().find_map(|s| name.strip_suffix(s).filter(|f| matches!(types.get(*f).map(String::as_str), Some("histogram" | "summary")))).map(str::to_string);
        if let Some(f) = family {
            if name.ends_with("_bucket") {
                continue;
            }
            let key = format!("{f}{}", attrs.iter().map(|(k, v)| format!("|{k}={v}")).collect::<String>());
            let i = match agg.get(&key) {
                Some(i) => *i,
                None => {
                    if out.len() >= MAX_SERIES {
                        continue;
                    }
                    out.push(Point { name: f.clone(), kind: "histogram".into(), unit: String::new(), time: now_ns, value: 0.0, count: Some(0), counter: false, attrs: attrs.clone() });
                    agg.insert(key, out.len() - 1);
                    out.len() - 1
                }
            };
            if name.ends_with("_sum") {
                out[i].value = value;
            } else {
                out[i].count = Some(value.max(0.0) as u64);
            }
            continue;
        }
        // A summary's quantile lines carry `quantile`; they're not kept.
        if matches!(types.get(&name).map(String::as_str), Some("summary" | "histogram")) {
            continue;
        }
        if out.len() >= MAX_SERIES {
            break;
        }
        let counter = types.get(&name).map(String::as_str) == Some("counter") || (name.ends_with("_total") && !types.contains_key(&name));
        out.push(Point { name, kind: if counter { "sum" } else { "gauge" }.into(), unit: String::new(), time: now_ns, value, count: None, counter, attrs });
    }
    out
}

/// One sample line: `name{a="b",…} value [timestamp]`, its name and labels cut to size.
fn sample(line: &str) -> Option<(String, Labels, f64)> {
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let name = &line[..name_end];
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':') {
        return None;
    }
    let mut rest = &line[name_end..];
    let mut labels = Vec::new();
    if let Some(r) = rest.strip_prefix('{') {
        let mut chars = r.char_indices().peekable();
        let mut end = None;
        while let Some((i, c)) = chars.next() {
            if c == '}' {
                end = Some(i);
                break;
            }
            if c == ',' || c.is_whitespace() {
                continue;
            }
            // A label: name="value", the value with \" \\ \n escapes.
            let start = i;
            let mut eq = None;
            for (j, d) in chars.by_ref() {
                if d == '=' {
                    eq = Some(j);
                    break;
                }
            }
            let key = r[start..eq?].trim().to_string();
            if chars.next().map(|(_, q)| q) != Some('"') {
                return None;
            }
            let mut val = String::new();
            loop {
                let (_, d) = chars.next()?;
                match d {
                    '"' => break,
                    '\\' => match chars.next()?.1 {
                        'n' => val.push('\n'),
                        o => val.push(o),
                    },
                    o => val.push(o),
                }
            }
            if labels.len() < MAX_LABELS {
                labels.push((key.chars().take(100).collect(), val.chars().take(200).collect()));
            }
        }
        rest = &r[end? + 1..];
    }
    let value = rest.split_whitespace().next()?;
    let value = match value {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        v => v.parse().ok()?,
    };
    Some((name.chars().take(200).collect(), labels, value))
}

impl Apps {
    /// Every owned environment's running instances with `run.metrics`, scraped (each on a task of
    /// its own, one at a time per instance).
    pub fn scrape_pass(self: &Arc<Self>) {
        for l in self.lives().into_iter().filter(|l| self.owns(&l.service)) {
            let Some(tpl) = l.template() else { continue };
            let Some(m) = tpl.run.metrics.clone() else { continue };
            let Some(port) = m.port.or(tpl.run.port) else { continue };
            for inst in l.instances() {
                let Some(addr) = (if Some(port) == tpl.run.port { inst.running_addr() } else { inst.running_addr().and(inst.port_addr(port)) }) else { continue };
                if !inst.begin_scrape() {
                    continue;
                }
                let (this, l, path) = (self.clone(), l.clone(), m.path.clone());
                tokio::spawn(async move {
                    this.scrape(&l, &inst.name, addr, &path).await;
                    inst.end_scrape();
                });
            }
        }
    }

    async fn scrape(self: &Arc<Self>, l: &Arc<EnvLive>, instance: &str, addr: std::net::SocketAddr, path: &str) {
        let url = format!("http://{addr}{path}");
        let got = async {
            let mut resp = self.scrape_client.get(&url).header("accept", "text/plain;version=0.0.4").send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("answered {}", resp.status().as_u16()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(format!("its answer is over {} MiB", MAX_BODY >> 20));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(String::from_utf8_lossy(&body).into_owned())
        };
        let key = crate::env_key(&l.service, &l.env);
        match tokio::time::timeout(Duration::from_secs(10), got).await.map_err(|_| "no answer in 10 s".to_string()).and_then(|r| r) {
            Ok(text) => {
                self.scrape_errors.lock().remove(&key);
                let now_ns = code_sql::now_ms() as u64 * 1_000_000;
                let sink = self.logs.sink(crate::traces::metric_key(&l.service, &l.env), crate::logs::Stream::System);
                for p in parse(&text, now_ns, instance) {
                    sink.line(serde_json::to_string(&p).expect("a point serializes"));
                }
            }
            // Said once in its log until a scrape works again.
            Err(e) => {
                if self.scrape_errors.lock().insert(key) {
                    l.log.stream(crate::logs::Stream::System).line(format!("metrics: GET {path} on {instance} failed: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_format_reads_as_points() {
        let text = "# HELP http_requests_total Requests.\n# TYPE http_requests_total counter\nhttp_requests_total{method=\"get\",path=\"/a \\\"b\\\"\"} 12 1700000000000\nhttp_requests_total{method=\"post\"} 3\n# TYPE queue_depth gauge\nqueue_depth 7.5\n# TYPE req_seconds histogram\nreq_seconds_bucket{le=\"0.1\"} 4\nreq_seconds_bucket{le=\"+Inf\"} 5\nreq_seconds_sum 1.25\nreq_seconds_count 5\n# TYPE rpc summary\nrpc{quantile=\"0.5\"} 0.2\nrpc_sum 9\nrpc_count 30\nuntyped_thing NaN\nbad line\njobs_done_total 4\n";
        let p = parse(text, 1, "web-1");
        let show: Vec<(&str, &str, f64, Option<u64>, bool)> = p.iter().map(|p| (p.name.as_str(), p.kind.as_str(), p.value, p.count, p.counter)).collect();
        assert_eq!(
            show,
            [
                ("http_requests_total", "sum", 12.0, None, true),
                ("http_requests_total", "sum", 3.0, None, true),
                ("queue_depth", "gauge", 7.5, None, false),
                ("req_seconds", "histogram", 1.25, Some(5), false),
                ("rpc", "histogram", 9.0, Some(30), false),
                ("jobs_done_total", "sum", 4.0, None, true),
            ]
        );
        assert_eq!(p[0].attrs, [("method".to_string(), "get".to_string()), ("path".into(), "/a \"b\"".into()), ("instance".into(), "web-1".into())]);
        let many: String = (0..5000).map(|i| format!("g{i} 1\n")).collect();
        assert_eq!(parse(&many, 1, "x").len(), MAX_SERIES);
    }
}
