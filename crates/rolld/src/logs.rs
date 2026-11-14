//! rolld's logs and metrics (docs/cloud/README.md, "Logs" and "Metrics").
//!
//! `ROLL_LOG_FORMAT=text` (the default) is tracing's usual format. `json` writes one JSON object
//! per line on stdout: `timestamp`, `level`, `target`, `msg` and the event's fields.
//!
//! Every HTTP request rolld answers (the internal API's routers, and the edge's HTTPS listener)
//! gets one access line, when its response body ends: `listener`, `method`, `route` (the matched
//! pattern, never the path: no tenant names, tokens or query strings), `product`, numeric
//! `status`, `duration_ms` (until the body ended), `ttfb_ms` (until the response head) and
//! `bytes` (response body). In JSON the line is also a CloudWatch Embedded Metric Format record
//! (`_aws`): `Latency` (= `ttfb_ms`), `Requests` and `Http5xx` in the `RollCloud` namespace, by
//! `product` and fleet-wide (`service`). A request another node forwarded here is logged with
//! `forwarded: true` and no metrics, so nothing is counted twice. In text, access lines are
//! `debug` events under the `access` target (`RUST_LOG=access=debug`).
//!
//! The cluster's lease counters ([`roll_cluster::Stats`]) go out every minute, and once more
//! after a drain, as `Handoffs`, `HandoffFailures`, `LeaseTakeovers` and `LeasesFenced`.

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::{Method, Response};
use axum::middleware::Next;
use bytes::Bytes;
use clap::ValueEnum;
use http_body::{Frame, SizeHint};
use serde_json::{Map, Value, json};
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

pub const NAMESPACE: &str = "RollCloud";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

static JSON: AtomicBool = AtomicBool::new(false);

fn json_on() -> bool {
    JSON.load(Ordering::Relaxed)
}

const DEFAULT_FILTER: &str = "info,pg_proxy=info,pg_control=info,pg_storage=info,redis_server=info,redis_control=info,stream_broker=info,tasks=info,code_control=info,code_store=info,code_git=info,code_review=info,code_index=info,mq_core=info,mq_amqp=info,mq_cluster=info,mq_mgmt=info,mq_http=info,mq_mqtt=info,mq_stomp=info,mq_shovel=info,nosql_server=info,nosql_control=info,nosql_storage=info,s3_store=info,s3_server=info,s3_control=info";

pub fn init(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into());
    match format {
        LogFormat::Text => tracing_subscriber::fmt().with_env_filter(filter).init(),
        LogFormat::Json => {
            JSON.store(true, Ordering::Relaxed);
            tracing_subscriber::fmt().with_env_filter(filter).event_format(JsonLines).init();
        }
    }
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// One line: `timestamp`, `level`, `target`, `msg`, then `fields`.
fn line(level: &str, target: &str, msg: &str, fields: Map<String, Value>) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("timestamp".into(), timestamp().into());
    o.insert("level".into(), level.into());
    o.insert("target".into(), target.into());
    o.insert("msg".into(), msg.into());
    for (k, v) in fields {
        o.entry(k).or_insert(v);
    }
    o
}

/// Write a whole line at once (stdout's lock is taken per write, so lines never interleave).
fn write_line(o: Map<String, Value>) {
    let mut s = Value::Object(o).to_string();
    s.push('\n');
    let _ = std::io::stdout().write_all(s.as_bytes());
}

/// tracing events as JSON lines.
struct JsonLines;

struct Fields<'a>(&'a mut Map<String, Value>);

impl Fields<'_> {
    fn put(&mut self, field: &Field, v: Value) {
        let name = match field.name() {
            "message" => "msg",
            n => n,
        };
        // An event's field never replaces the line's own keys.
        let name = match name {
            "timestamp" | "level" | "target" | "_aws" => format!("field_{name}"),
            n => n.to_string(),
        };
        self.0.insert(name, v);
    }
}

impl Visit for Fields<'_> {
    fn record_f64(&mut self, f: &Field, v: f64) {
        self.put(f, json!(v));
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.put(f, v.into());
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.put(f, v.into());
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.put(f, v.into());
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.put(f, v.into());
    }
    fn record_error(&mut self, f: &Field, v: &(dyn std::error::Error + 'static)) {
        self.put(f, v.to_string().into());
    }
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.put(f, format!("{v:?}").into());
    }
}

impl<S, N> FormatEvent<S, N> for JsonLines
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _: &FmtContext<'_, S, N>, mut w: Writer<'_>, event: &tracing::Event<'_>) -> std::fmt::Result {
        let meta = event.metadata();
        let mut fields = Map::new();
        event.record(&mut Fields(&mut fields));
        let msg = match fields.remove("msg") {
            Some(Value::String(s)) => s,
            Some(v) => v.to_string(),
            None => String::new(),
        };
        writeln!(w, "{}", Value::Object(line(meta.level().as_str(), meta.target(), &msg, fields)))
    }
}

// ---- metrics ----

/// The `_aws` member of an Embedded Metric Format record: `metrics` (name, unit) by each set of
/// `dimensions`. The line must carry every metric and dimension as a top-level field.
fn emf(metrics: &[(&str, &str)], dimensions: &[&[&str]]) -> Value {
    json!({
        "Timestamp": epoch_ms(),
        "CloudWatchMetrics": [{
            "Namespace": NAMESPACE,
            "Dimensions": dimensions,
            "Metrics": metrics.iter().map(|(name, unit)| json!({ "Name": name, "Unit": unit })).collect::<Vec<_>>(),
        }],
    })
}

/// `service`, the fleet-wide dimension's value.
const SERVICE: &str = "rolld";

// ---- access lines ----

/// The route an inner router matched, handed to the edge's listener (which logs the request).
#[derive(Clone)]
pub struct Routed(pub String);

/// What we know of a request once its response head is out.
struct Access {
    listener: &'static str,
    method: Method,
    route: String,
    product: &'static str,
    forwarded: bool,
    status: u16,
    started: Instant,
    ttfb: Duration,
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

impl Access {
    fn fields(&self, bytes: u64) -> Map<String, Value> {
        let mut f = Map::new();
        f.insert("listener".into(), self.listener.into());
        f.insert("method".into(), self.method.as_str().into());
        f.insert("route".into(), self.route.clone().into());
        f.insert("product".into(), self.product.into());
        f.insert("status".into(), self.status.into());
        f.insert("duration_ms".into(), json!(ms(self.started.elapsed())));
        f.insert("ttfb_ms".into(), json!(ms(self.ttfb)));
        f.insert("bytes".into(), bytes.into());
        if self.forwarded {
            f.insert("forwarded".into(), true.into());
        } else {
            f.insert("service".into(), SERVICE.into());
            f.insert("Latency".into(), json!(ms(self.ttfb)));
            f.insert("Requests".into(), 1.into());
            f.insert("Http5xx".into(), u8::from(self.status >= 500).into());
            f.insert("_aws".into(), emf(&[("Latency", "Milliseconds"), ("Requests", "Count"), ("Http5xx", "Count")], &[&["product"], &["service"]]));
        }
        f
    }

    fn emit(&self, bytes: u64) {
        if json_on() {
            write_line(line("INFO", "access", "request", self.fields(bytes)));
        } else {
            let (method, route) = (self.method.as_str(), self.route.as_str());
            tracing::debug!(target: "access", listener = self.listener, method, route, product = self.product, status = self.status, duration_ms = ms(self.started.elapsed()), bytes, forwarded = self.forwarded, "request");
        }
    }

    /// Log the request once `resp`'s body ends (or is dropped).
    fn wrap(self, resp: Response<Body>) -> Response<Body> {
        resp.map(|inner| Body::new(Counted { inner, access: Some(self), bytes: 0 }))
    }
}

/// A response body that counts its bytes and logs its request when it ends.
struct Counted {
    inner: Body,
    access: Option<Access>,
    bytes: u64,
}

impl Counted {
    fn finish(&mut self) {
        if let Some(a) = self.access.take() {
            a.emit(self.bytes);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.finish();
    }
}

impl http_body::Body for Counted {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        let r = Pin::new(&mut this.inner).poll_frame(cx);
        match &r {
            Poll::Ready(Some(Ok(f))) => {
                if let Some(d) = f.data_ref() {
                    this.bytes += d.len() as u64;
                }
            }
            Poll::Ready(_) => this.finish(),
            Poll::Pending => {}
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The product a matched route belongs to; git's smart-HTTP routes are `git` (pushes and clones
/// take as long as the pack, which mustn't count as code's API latency).
fn route_product(route: &str) -> &'static str {
    if route == "/git" || route.starts_with("/git/") {
        return "git";
    }
    crate::products::Product::of_route(route).map_or(SERVICE, crate::products::Product::name)
}

const UNMATCHED: &str = "unmatched";

/// The access-log layer for rolld's routers (`Router::layer(axum::middleware::from_fn(access))`,
/// so the matched route is known). Behind the edge's HTTPS listener it only reports the route:
/// the listener logs the request.
pub async fn access(req: Request, next: Next) -> Response<Body> {
    let started = Instant::now();
    let route = req.extensions().get::<MatchedPath>().map_or(UNMATCHED, |m| m.as_str()).to_string();
    let via_edge = req.extensions().get::<roll_edge::http::ViaEdge>().is_some();
    let forwarded = req.extensions().get::<roll_cluster::Forwarded>().is_some();
    let method = req.method().clone();
    let mut resp = next.run(req).await;
    if via_edge {
        resp.extensions_mut().insert(Routed(route));
        return resp;
    }
    let product = route_product(&route);
    Access { listener: "api", method, route, product, forwarded, status: resp.status().as_u16(), started, ttfb: started.elapsed() }.wrap(resp)
}

/// The edge HTTPS listener's access lines.
pub struct EdgeAccess;

impl roll_edge::http::Observe for EdgeAccess {
    fn observe(&self, host: &roll_edge::Host, method: &Method, started: Instant, resp: Response<Body>) -> Response<Body> {
        use roll_edge::Host;
        let routed = resp.extensions().get::<Routed>().map(|r| r.0.clone());
        let by_host = match host {
            Host::Redis(_) => Some("redis"),
            Host::Kafka(_) => Some("streams"),
            Host::Tasks => Some("tasks"),
            Host::S3 { .. } => Some("storage"),
            Host::Nosql(_) => Some("nosql"),
            Host::Mq(_) => Some("queues"),
            Host::Preview(_) => Some("preview"),
            Host::App(_) => Some("apps"),
            Host::Dashboard | Host::Api | Host::Git | Host::Pg(_) | Host::Site(_) | Host::Origin | Host::Unknown => None,
        };
        let (route, product) = match (routed, by_host) {
            (Some(r), Some(p)) => (r, p),
            (Some(r), None) => {
                let p = route_product(&r);
                (r, p)
            }
            (None, Some(p)) => (p.to_string(), p),
            (None, None) if matches!(host, Host::Dashboard) => ("dashboard".to_string(), "dashboard"),
            // Answered by the edge itself (an unknown host, a missing origin-pull certificate).
            (None, None) => ("edge".to_string(), "edge"),
        };
        let status = resp.status().as_u16();
        Access { listener: "https", method: method.clone(), route, product, forwarded: false, status, started, ttfb: started.elapsed() }.wrap(resp)
    }
}

// ---- cluster metrics ----

/// Publishes the cluster's lease counters (JSON logs only).
pub struct ClusterMetrics {
    cluster: Arc<roll_cluster::Cluster>,
    last: std::sync::Mutex<roll_cluster::Stats>,
}

impl ClusterMetrics {
    pub const EVERY: Duration = Duration::from_secs(60);

    /// Every minute from now on; `flush` once more after a drain.
    pub fn start(cluster: Arc<roll_cluster::Cluster>) -> Arc<ClusterMetrics> {
        let m = Arc::new(ClusterMetrics { cluster, last: std::sync::Mutex::new(roll_cluster::Stats::default()) });
        if json_on() {
            let weak = Arc::downgrade(&m);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Self::EVERY);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let Some(m) = weak.upgrade() else { return };
                    m.flush();
                }
            });
        }
        m
    }

    /// What changed since the last line, as one EMF line.
    fn fields(node: &str, d: roll_cluster::Stats) -> Map<String, Value> {
        let mut f = Map::new();
        f.insert("node".into(), node.into());
        f.insert("service".into(), SERVICE.into());
        f.insert("Handoffs".into(), d.handoffs.into());
        f.insert("HandoffFailures".into(), d.handoff_failures.into());
        f.insert("LeaseTakeovers".into(), d.takeovers.into());
        f.insert("LeasesFenced".into(), d.fenced.into());
        f.insert("_aws".into(), emf(&[("Handoffs", "Count"), ("HandoffFailures", "Count"), ("LeaseTakeovers", "Count"), ("LeasesFenced", "Count")], &[&["service"]]));
        f
    }

    pub fn flush(&self) {
        if !json_on() {
            return;
        }
        let now = self.cluster.stats();
        let d = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let d = roll_cluster::Stats {
                handoffs: now.handoffs - last.handoffs,
                handoff_failures: now.handoff_failures - last.handoff_failures,
                takeovers: now.takeovers - last.takeovers,
                fenced: now.fenced - last.fenced,
            };
            *last = now;
            d
        };
        write_line(line("INFO", "metrics", "cluster", Self::fields(self.cluster.node_id(), d)));
    }
}

// ---- previews metrics ----

/// Publishes what this node's previews engine did: converges, screenshots and their failures
/// (JSON logs only), every minute while the engine runs.
pub struct PreviewMetrics {
    engine: std::sync::Weak<roll_previews::Engine>,
    last: std::sync::Mutex<roll_previews::engine::Stats>,
}

impl PreviewMetrics {
    pub fn start(engine: &Arc<roll_previews::Engine>) {
        if !json_on() {
            return;
        }
        let m = PreviewMetrics { engine: Arc::downgrade(engine), last: std::sync::Mutex::new(Default::default()) };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(ClusterMetrics::EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                if !m.flush() {
                    return;
                }
            }
        });
    }

    fn fields(d: roll_previews::engine::Stats) -> Map<String, Value> {
        let mut f = Map::new();
        f.insert("service".into(), SERVICE.into());
        f.insert("PreviewConverges".into(), d.converges.into());
        f.insert("PreviewConvergeFailures".into(), d.converge_failures.into());
        f.insert("PreviewScreenshots".into(), d.screenshots.into());
        f.insert("PreviewScreenshotFailures".into(), d.screenshot_failures.into());
        f.insert(
            "_aws".into(),
            emf(&[("PreviewConverges", "Count"), ("PreviewConvergeFailures", "Count"), ("PreviewScreenshots", "Count"), ("PreviewScreenshotFailures", "Count")], &[&["service"]]),
        );
        f
    }

    /// `false` once the engine is gone.
    fn flush(&self) -> bool {
        let Some(engine) = self.engine.upgrade() else { return false };
        let now = engine.stats();
        let d = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let d = roll_previews::engine::Stats {
                converges: now.converges - last.converges,
                converge_failures: now.converge_failures - last.converge_failures,
                screenshots: now.screenshots - last.screenshots,
                screenshot_failures: now.screenshot_failures - last.screenshot_failures,
            };
            *last = now;
            d
        };
        write_line(line("INFO", "metrics", "previews", Self::fields(d)));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every metric and dimension an EMF record declares is a top-level field of the right type,
    /// in the namespace the stack's alarms read.
    fn check_emf(line: &Map<String, Value>) {
        let aws = &line["_aws"];
        assert!(aws["Timestamp"].is_u64(), "{aws}");
        let directives = aws["CloudWatchMetrics"].as_array().unwrap();
        assert_eq!(directives.len(), 1);
        let d = &directives[0];
        assert_eq!(d["Namespace"], NAMESPACE);
        for set in d["Dimensions"].as_array().unwrap() {
            let set = set.as_array().unwrap();
            assert!(!set.is_empty());
            for key in set {
                assert!(line[key.as_str().unwrap()].is_string(), "dimension {key} isn't a string field");
            }
        }
        for m in d["Metrics"].as_array().unwrap() {
            let name = m["Name"].as_str().unwrap();
            assert!(line[name].is_number(), "metric {name} isn't a number field");
            assert!(["Count", "Milliseconds"].contains(&m["Unit"].as_str().unwrap()));
        }
    }

    #[test]
    fn access_and_cluster_lines_are_emf_records() {
        let a = Access {
            listener: "api",
            method: Method::POST,
            route: "/v1/code/repos/{owner}/{repo}".into(),
            product: "code",
            forwarded: false,
            status: 503,
            started: Instant::now(),
            ttfb: Duration::from_micros(1500),
        };
        let l = line("INFO", "access", "request", a.fields(42));
        check_emf(&l);
        assert_eq!((l["status"].as_u64(), l["Http5xx"].as_u64(), l["Latency"].as_f64(), l["bytes"].as_u64()), (Some(503), Some(1), Some(1.5), Some(42)));
        assert_eq!(l["product"], "code");
        let f = Access { forwarded: true, ..a }.fields(0);
        assert!(!f.contains_key("_aws") && !f.contains_key("Http5xx"), "a forwarded request isn't counted twice");

        let stats = roll_cluster::Stats { handoffs: 3, handoff_failures: 1, takeovers: 2, fenced: 0 };
        let l = line("INFO", "metrics", "cluster", ClusterMetrics::fields("i-1", stats));
        check_emf(&l);
        assert_eq!((l["HandoffFailures"].as_u64(), l["LeaseTakeovers"].as_u64()), (Some(1), Some(2)));
        let l = line("INFO", "metrics", "previews", PreviewMetrics::fields(roll_previews::engine::Stats { converges: 9, converge_failures: 1, screenshots: 4, screenshot_failures: 2 }));
        check_emf(&l);
        assert_eq!((l["PreviewConvergeFailures"].as_u64(), l["PreviewScreenshotFailures"].as_u64()), (Some(1), Some(2)));
        // Pushes and clones aren't code's API latency.
        assert_eq!((route_product("/git/{*rest}"), route_product("/v1/code/repos"), route_product(UNMATCHED)), ("git", "code", "rolld"));
    }
}
