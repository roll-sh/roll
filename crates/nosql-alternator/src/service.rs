//! The Alternator service: HTTP handling, authentication and dispatch to the operations.

use std::sync::Arc;

use crate::auth::{self, Credentials};
use crate::backend::Backend;
use crate::error::{Error, ErrorKind, Result};
use crate::http::{self, HttpRequest, HttpResponse, Route};
use crate::json::{self, Json};
use crate::ops;

/// Write isolation policies (`--alternator-write-isolation`, tag `system:write_isolation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteIsolation {
    AlwaysUseLwt,
    ForbidRmw,
    OnlyRmwUsesLwt,
    UnsafeRmw,
}

impl WriteIsolation {
    pub fn parse(s: &str) -> Option<WriteIsolation> {
        Some(match s {
            "a" | "always" | "always_use_lwt" => WriteIsolation::AlwaysUseLwt,
            "f" | "forbid" | "forbid_rmw" => WriteIsolation::ForbidRmw,
            "o" | "only_rmw_uses_lwt" => WriteIsolation::OnlyRmwUsesLwt,
            "u" | "unsafe" | "unsafe_rmw" => WriteIsolation::UnsafeRmw,
            _ => return None,
        })
    }
}

/// Settings that change what clients see (the `alternator_*` options of `scylla.yaml`).
#[derive(Clone, Debug)]
pub struct Config {
    /// `alternator_enforce_authorization`.
    pub enforce_authorization: bool,
    /// `alternator_max_items_in_batch_write`.
    pub max_items_in_batch_write: usize,
    /// `alternator_write_isolation`.
    pub write_isolation: WriteIsolation,
    /// `alternator_response_compression_threshold_in_bytes`.
    pub response_compression_threshold: usize,
    /// `alternator_response_gzip_compression_level`.
    pub gzip_compression_level: u32,
    /// `alternator_http_response_server_header` (none by default).
    pub server_header: Option<String>,
    /// `alternator_http_response_disable_date_header`.
    pub disable_date_header: bool,
    /// Addresses `/localnodes` lists, with their datacenter and rack.
    pub local_nodes: Vec<LocalNode>,
    /// `alternator_describe_endpoints`: the address DescribeEndpoints returns; the request's
    /// `Host` when empty.
    pub describe_endpoints: Option<String>,
    /// Accepted clock skew of signed requests, in seconds.
    pub max_clock_skew_secs: i64,
    /// `alternator_streams_time_window_s`: a stream record is readable this long after its write.
    pub streams_time_window_ms: i64,
    /// `alternator_streams_increased_compatibility`: streams name events by the item's
    /// existence before and after, and leave out writes that change nothing, in every view type.
    pub streams_increased_compatibility: bool,
    /// `alternator_allow_system_table_write`: `system.config` can be written through
    /// `.scylla.alternator.system.config` (live options take effect at once).
    pub allow_system_table_write: bool,
    /// `alternator_http_response_disable_content_type_header`.
    pub disable_content_type_header: bool,
    /// `alternator_warn_authorization`: without enforcement, still check signatures and
    /// permissions and count failures (`authentication_failures`, `authorization_failures`),
    /// but let the requests through.
    pub warn_authorization: bool,
    /// `alternator_force_read_before_write`: writes read the item first (metrics see its size).
    pub force_read_before_write: bool,
    /// `max_concurrent_requests_per_shard`: requests beyond this many in flight are refused
    /// with `RequestLimitExceeded` (Alternator sheds at `>=`, so 0 refuses everything).
    pub max_concurrent_requests: u64,
    /// `alternator_max_expression_cache_entries_per_shard` ([`crate::expr::cache`]).
    pub max_expression_cache_entries: usize,
    /// `alternator_max_users_query_size_in_trace_output`: request bytes kept in a trace's
    /// `query` parameter.
    pub max_users_query_size_in_trace_output: usize,
}

#[derive(Clone, Debug)]
pub struct LocalNode {
    pub address: String,
    pub datacenter: String,
    pub rack: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            enforce_authorization: false,
            max_items_in_batch_write: 100,
            write_isolation: WriteIsolation::AlwaysUseLwt,
            response_compression_threshold: 4096,
            gzip_compression_level: 6,
            server_header: None,
            disable_date_header: false,
            local_nodes: vec![LocalNode {
                address: "127.0.0.1".into(),
                datacenter: "datacenter1".into(),
                rack: "rack1".into(),
            }],
            describe_endpoints: None,
            max_clock_skew_secs: 15 * 60,
            streams_time_window_ms: 10_000,
            streams_increased_compatibility: false,
            allow_system_table_write: false,
            disable_content_type_header: false,
            warn_authorization: false,
            force_read_before_write: false,
            max_concurrent_requests: u64::from(u32::MAX),
            max_expression_cache_entries: 2000,
            max_users_query_size_in_trace_output: 4096,
        }
    }
}

/// Every operation Alternator implements; anything else is `UnknownOperationException`.
pub const OPERATIONS: &[&str] = &[
    "CreateTable",
    "DescribeTable",
    "DeleteTable",
    "UpdateTable",
    "PutItem",
    "UpdateItem",
    "GetItem",
    "DeleteItem",
    "ListTables",
    "Scan",
    "DescribeEndpoints",
    "BatchWriteItem",
    "BatchGetItem",
    "Query",
    "TagResource",
    "UntagResource",
    "ListTagsOfResource",
    "UpdateTimeToLive",
    "DescribeTimeToLive",
    "ListStreams",
    "DescribeStream",
    "GetShardIterator",
    "GetRecords",
    "DescribeContinuousBackups",
];

/// A request being executed.
pub struct Request<'a> {
    pub op: &'a str,
    pub body: &'a Json,
    /// The authenticated role, when authorization is enforced (or warned about).
    pub user: Option<&'a str>,
    pub host: Option<&'a str>,
    /// The client's address, when known.
    pub peer: Option<std::net::SocketAddr>,
    /// What the operation did, for the metrics.
    pub stats: &'a crate::metrics::RequestStats,
}

/// The DynamoDB API over a [`Backend`].
pub struct Alternator {
    pub backend: Arc<dyn Backend>,
    /// The configuration (live options change through `system.config`, see [`Alternator::set_option`]).
    config: parking_lot::RwLock<Config>,
    /// `system.config` values written through Alternator, by option name.
    overrides: parking_lot::RwLock<std::collections::BTreeMap<String, String>>,
    /// DescribeTable's table sizes, with when they were computed (ms), by table id.
    sizes: parking_lot::Mutex<std::collections::HashMap<uuid::Uuid, (i64, u64)>>,
    pub credentials: Option<Arc<dyn Credentials>>,
    /// The database's Alternator metrics.
    pub metrics: Arc<crate::metrics::Metrics>,
    /// Added to the wall clock (tests move time forward instead of sleeping).
    clock_offset_ms: std::sync::atomic::AtomicI64,
    /// Parsed expressions ([`crate::expr::cache`]).
    pub(crate) expressions: Arc<crate::expr::cache::Cache>,
    /// API requests being handled (`max_concurrent_requests_per_shard`).
    in_flight: std::sync::atomic::AtomicU64,
}

impl Alternator {
    pub fn new(backend: Arc<dyn Backend>, config: Config) -> Alternator {
        backend.set_streams_increased_compatibility(config.streams_increased_compatibility);
        let metrics: Arc<crate::metrics::Metrics> = Default::default();
        let expressions = Arc::new(crate::expr::cache::Cache::new(
            config.max_expression_cache_entries,
            metrics.clone(),
        ));
        Alternator {
            backend,
            config: parking_lot::RwLock::new(config),
            overrides: Default::default(),
            sizes: Default::default(),
            credentials: None,
            metrics,
            clock_offset_ms: Default::default(),
            expressions,
            in_flight: Default::default(),
        }
    }

    /// The current configuration.
    pub fn config(&self) -> Config {
        self.config.read().clone()
    }

    /// Changes the configuration (tests, embedding).
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.config.write();
        f(&mut c);
        self.backend
            .set_streams_increased_compatibility(c.streams_increased_compatibility);
    }

    /// Sets a `system.config` option, as an UpdateItem of `.scylla.alternator.system.config`
    /// does: ScyllaDB's live-updatable Alternator options take effect at once, and every value
    /// reads back through the internal table. `None` restores the option's configured value.
    pub fn set_option(&self, name: &str, value: Option<&str>) {
        match value {
            Some(v) => self
                .overrides
                .write()
                .insert(name.to_string(), v.to_string()),
            None => self.overrides.write().remove(name),
        };
        let Some(v) = value else { return };
        let v = v.trim().trim_matches('"');
        let flag = || matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on");
        self.update_config(|c| match name {
            "alternator_streams_increased_compatibility" => {
                c.streams_increased_compatibility = flag()
            }
            "alternator_enforce_authorization" => c.enforce_authorization = flag(),
            "alternator_warn_authorization" => c.warn_authorization = flag(),
            "alternator_force_read_before_write" => c.force_read_before_write = flag(),
            "alternator_http_response_disable_date_header" => c.disable_date_header = flag(),
            "alternator_http_response_disable_content_type_header" => {
                c.disable_content_type_header = flag()
            }
            "alternator_http_response_server_header" => {
                // Control characters are dropped; nothing left (or only spaces) means no header.
                let clean: String = v.chars().filter(|c| !c.is_control()).collect();
                let clean = clean.trim();
                c.server_header = (!clean.is_empty()).then(|| clean.to_string())
            }
            "alternator_response_compression_threshold_in_bytes" => {
                if let Ok(n) = v.parse() {
                    c.response_compression_threshold = n;
                }
            }
            "alternator_response_gzip_compression_level" => {
                if let Ok(n) = v.parse() {
                    c.gzip_compression_level = n;
                }
            }
            "alternator_max_items_in_batch_write" => {
                if let Ok(n) = v.parse() {
                    c.max_items_in_batch_write = n;
                }
            }
            "alternator_streams_time_window_s" => {
                if let Ok(n) = v.parse::<f64>() {
                    c.streams_time_window_ms = (n * 1000.0) as i64;
                }
            }
            "max_concurrent_requests_per_shard" => {
                if let Ok(n) = v.parse() {
                    c.max_concurrent_requests = n;
                }
            }
            "alternator_max_expression_cache_entries_per_shard" => {
                if let Ok(n) = v.parse() {
                    c.max_expression_cache_entries = n;
                    self.expressions.set_capacity(n);
                }
            }
            "alternator_max_users_query_size_in_trace_output" => {
                if let Ok(n) = v.parse() {
                    c.max_users_query_size_in_trace_output = n;
                }
            }
            _ => {}
        });
    }

    /// Follows an option set elsewhere (CQL `UPDATE system.config`): like
    /// [`Alternator::set_option`], without keeping an Alternator-written value.
    pub fn follow_option(&self, name: &str, value: &str) {
        self.overrides.write().remove(name);
        self.set_option(name, Some(value));
        self.overrides.write().remove(name);
    }

    /// A table's size for DescribeTable, cached for
    /// `alternator_describe_table_info_cache_validity_in_seconds` (default 6 hours).
    pub async fn table_size(&self, s: &crate::schema::TableSchema) -> Result<u64> {
        let validity_ms = self
            .overrides
            .read()
            .get("alternator_describe_table_info_cache_validity_in_seconds")
            .and_then(|v| v.trim().trim_matches('"').parse::<f64>().ok())
            .map_or(21_600_000, |v| (v * 1000.0) as i64);
        let now = self.now_ms();
        if let Some((at, size)) = self.sizes.lock().get(&s.id).copied()
            && now - at < validity_ms
        {
            return Ok(size);
        }
        let size = self.backend.table_size_bytes(s).await?;
        self.sizes.lock().insert(s.id, (now, size));
        Ok(size)
    }

    /// `system.config` values written through Alternator.
    pub fn option_overrides(&self) -> std::collections::BTreeMap<String, String> {
        self.overrides.read().clone()
    }

    /// Milliseconds since the epoch, as Alternator sees them.
    pub fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
            + self
                .clock_offset_ms
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Moves Alternator's clock forward (stream visibility, index backfills).
    pub fn advance_clock(&self, ms: i64) {
        self.clock_offset_ms
            .fetch_add(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Enables SigV4 verification against `credentials`.
    pub fn with_credentials(mut self, credentials: Arc<dyn Credentials>) -> Alternator {
        self.credentials = Some(credentials);
        self
    }

    /// Handles one HTTP request.
    pub async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let mut resp = match http::route(&req.method, &req.path) {
            Route::Api => {
                let mut r = self.api(&req).await;
                http::add_cors(&req, &mut r, false);
                r
            }
            Route::Health => {
                let host = req.header("host").unwrap_or_default();
                let mut r = HttpResponse::new(200, "text/plain", format!("healthy: {host}"));
                http::add_cors(&req, &mut r, false);
                r
            }
            Route::Options => {
                let mut r = HttpResponse::new(200, "text/plain", "");
                http::add_cors(&req, &mut r, true);
                r
            }
            Route::LocalNodes => {
                let (dc, rack) = http::localnodes_filter(&req.query);
                let list = self
                    .config()
                    .local_nodes
                    .iter()
                    .filter(|n| dc.as_ref().is_none_or(|d| *d == n.datacenter))
                    .filter(|n| rack.as_ref().is_none_or(|r| *r == n.rack))
                    .map(|n| Json::str(n.address.clone()))
                    .collect();
                HttpResponse::new(200, "application/json", Json::Array(list).to_json_string())
            }
            Route::NotFound => return http::not_found(),
        };
        let config = self.config();
        if let Some(s) = &config.server_header {
            resp.headers.push(("Server".into(), s.clone()));
        }
        if !config.disable_date_header {
            resp.headers.push(("Date".into(), http::http_date()));
        }
        if config.disable_content_type_header {
            resp.headers
                .retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
        }
        resp
    }

    async fn api(&self, req: &HttpRequest) -> HttpResponse {
        use std::sync::atomic::Ordering;
        // `max_concurrent_requests_per_shard`: shed (the body is already read, so the
        // connection stays usable).
        let limit = self.config().max_concurrent_requests;
        if self.in_flight.load(Ordering::Relaxed) >= limit {
            return HttpResponse::error(&Error::new(
                ErrorKind::RequestLimitExceeded,
                format!(
                    "too many in-flight requests (configured via max_concurrent_requests_per_shard): {limit}"
                ),
            ));
        }
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        let resp = self.api_counted(req).await;
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        resp
    }

    async fn api_counted(&self, req: &HttpRequest) -> HttpResponse {
        let started = std::time::Instant::now();
        let op = http::operation(req.header("x-amz-target").as_deref());
        let stats = crate::metrics::RequestStats::default();
        let mut resp = self.api_inner(req, &op, &stats).await;
        let known = OPERATIONS.contains(&op.as_str());
        const FAILED: &[u8] = b"ConditionalCheckFailedException";
        let condition_failed =
            resp.status == 400 && resp.body.windows(FAILED.len()).any(|w| w == FAILED);
        self.metrics.finish(
            known.then_some(op.as_str()),
            &stats,
            resp.status,
            condition_failed,
            started.elapsed().as_secs_f64() * 1e6,
        );
        let config = self.config();
        if resp.body.len() >= config.response_compression_threshold
            && config.gzip_compression_level > 0
            && let Some(enc) = req
                .header("accept-encoding")
                .as_deref()
                .and_then(http::choose_encoding)
        {
            let z = http::compress(enc, config.gzip_compression_level, &resp.body);
            resp.body = z.into();
            resp.headers.push((
                "Content-Encoding".into(),
                match enc {
                    http::Encoding::Gzip => "gzip",
                    http::Encoding::Deflate => "deflate",
                }
                .into(),
            ));
        }
        resp
    }

    async fn api_inner(
        &self,
        req: &HttpRequest,
        op: &str,
        stats: &crate::metrics::RequestStats,
    ) -> HttpResponse {
        if req.body.len() > http::MAX_REQUEST_BYTES {
            return HttpResponse::error(&http::payload_too_large());
        }
        let body =
            match http::decompress(req.header("content-encoding").as_deref(), req.body.clone()) {
                Ok(b) => b,
                Err(e) => return HttpResponse::error(&e),
            };
        let config = self.config();
        let user = if config.enforce_authorization || config.warn_authorization {
            let verified = match &self.credentials {
                None => Err(Error::new(
                    ErrorKind::UnrecognizedClient,
                    "The security token included in the request is invalid.",
                )),
                Some(creds) => {
                    auth::verify(req, &req.body, creds.as_ref(), config.max_clock_skew_secs)
                        .await
                        .map(|u| creds.role_of(&u))
                }
            };
            match verified {
                Ok(u) => Some(u),
                Err(e) => {
                    self.metrics
                        .add("scylla_alternator_authentication_failures", &[], 1.0);
                    if config.enforce_authorization {
                        return HttpResponse::error(&e);
                    }
                    None
                }
            }
        } else {
            None
        };
        let result = self
            .call_with(
                op,
                &body,
                user.as_deref(),
                req.header("host").as_deref(),
                req.peer,
                stats,
            )
            .await;
        match result {
            // Operations without a result (TagResource, UntagResource) send an empty body.
            Ok(Json::Null) => HttpResponse::new(200, http::JSON_CONTENT_TYPE, ""),
            Ok(j) => HttpResponse::json(200, &j),
            Err(e) => HttpResponse::error(&e),
        }
    }

    /// Runs an operation on a (decompressed, authenticated) body.
    pub async fn call(
        &self,
        op: &str,
        body: &[u8],
        user: Option<&str>,
        host: Option<&str>,
    ) -> Result<Json> {
        let stats = crate::metrics::RequestStats::default();
        self.call_with(op, body, user, host, None, &stats).await
    }

    async fn call_with(
        &self,
        op: &str,
        body: &[u8],
        user: Option<&str>,
        host: Option<&str>,
        peer: Option<std::net::SocketAddr>,
        stats: &crate::metrics::RequestStats,
    ) -> Result<Json> {
        if !OPERATIONS.contains(&op) {
            return Err(Error::new(
                ErrorKind::UnknownOperation,
                format!("Unsupported operation {op}"),
            ));
        }
        let json = json::parse(body).map_err(|e| {
            if e.too_deep {
                Error::validation(format!("Max nested level reached: {}", json::MAX_DEPTH))
            } else {
                Error::validation(format!("Parsing JSON failed: {e}"))
            }
        })?;
        if !json.is_object() {
            return Err(Error::validation("Request content must be an object"));
        }
        let r = Request {
            op,
            body: &json,
            user,
            host,
            peer,
            stats,
        };
        let out = crate::expr::cache::scope(self.expressions.clone(), ops::dispatch(self, &r)).await;
        if stats.uses_storage() {
            stats.set_scheduling_group(self.backend.scheduling_group(user).await);
        }
        out
    }
}
