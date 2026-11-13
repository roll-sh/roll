//! Request parsing and validation of `_delete_by_query`, `_update_by_query` and `_reindex`,
//! in Elasticsearch's order so the first failure (and its exception type, text and location)
//! is the one Elasticsearch reports:
//!
//! 1. the body: `RestReindexAction` runs `ReindexRequest.PARSER` (an `ObjectParser`: unknown
//!    fields, `failed to parse field [..]` wrapping with locations, `source` read as a map whose
//!    `index` and `remote` are taken out and whose rest is re-serialized and parsed as a search
//!    body); the by-query handlers take `conflicts`, `max_docs` (and `script`) out of the body
//!    map and parse the re-serialized rest as a search request;
//! 2. the URL options common to all three (`setCommonOptions`: `refresh`, `timeout`, `slices`,
//!    `wait_for_active_shards`, `requests_per_second`, `max_docs`);
//! 3. `validate()`: the search request in its scroll context, then the by-scroll and
//!    per-API checks, all collected into one `action_request_validation_exception`.
//!
//! Re-serialized bodies keep the locations of nested errors relative to the compact JSON
//! Elasticsearch re-parses, as it does.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::resolve::IndicesOptions;
use crate::query::script::Script;
use crate::query::xparser::{Field, Tokens, XParser};
use crate::rest::Ctx;
use crate::search::SearchRequest;
use crate::search_source::SearchSource;
use crate::units::TimeValue;

/// `AbstractBulkByScrollRequest.DEFAULT_SCROLL_SIZE`.
pub const DEFAULT_SCROLL_SIZE: i32 = 1000;

/// `source.remote` of a reindex.
#[derive(Clone, Debug, Default)]
pub struct Remote {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path_prefix: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub headers: Vec<(String, String)>,
    pub socket_timeout: Option<TimeValue>,
    pub connect_timeout: Option<TimeValue>,
    /// `source.query`, sent to the remote cluster as is (`RemoteInfo.queryForRemote`).
    pub query: Option<Value>,
}

/// `dest` of a reindex (the `IndexRequest` template).
#[derive(Clone, Debug, Default)]
pub struct Dest {
    pub index: Option<String>,
    pub routing: Option<String>,
    pub op_type: Option<String>,
    pub pipeline: Option<String>,
    pub version_type: Option<search_index::VersionType>,
}

/// A parsed by-scroll request.
#[derive(Clone, Debug)]
pub struct ByScroll {
    pub search: SearchRequest,
    /// `-1`: every match.
    pub max_docs: i64,
    /// `0`: `auto`.
    pub slices: u32,
    /// `-1`: unthrottled.
    pub rps: f64,
    pub proceed: bool,
    pub script: Option<Script>,
    pub refresh: bool,
    pub dest: Dest,
    pub remote: Option<Remote>,
    /// `pipeline` (update by query).
    pub pipeline: Option<String>,
}

impl ByScroll {
    fn new(search: SearchRequest) -> ByScroll {
        ByScroll {
            search,
            max_docs: -1,
            slices: 1,
            rps: -1.0,
            proceed: false,
            script: None,
            refresh: false,
            dest: Dest::default(),
            remote: None,
            pipeline: None,
        }
    }

    /// The scroll batch size (`size` of the search).
    pub fn batch(&self) -> u64 {
        self.search.source.size.unwrap_or(DEFAULT_SCROLL_SIZE).max(1) as u64
    }

    /// `max_docs` as a limit.
    pub fn limit(&self) -> Option<u64> {
        (self.max_docs >= 0).then_some(self.max_docs as u64)
    }

    /// The manual slice (`source.slice` / body `slice`): (id, max).
    pub fn slice(&self) -> Option<(u32, u32)> {
        self.search.source.slice.as_ref().map(|s| (s.id.max(0) as u32, s.max.max(1) as u32))
    }

    /// `setMaxDocsValidateIdentical` + `setMaxDocs`.
    fn set_max_docs(&mut self, m: i64) -> EsResult<()> {
        if self.max_docs != -1 && self.max_docs != m {
            return Err(EsError::illegal_argument(format!("[max_docs] set to two different values [{}] and [{m}]", self.max_docs)));
        }
        if m < 0 {
            return Err(EsError::illegal_argument(format!("[max_docs] parameter cannot be negative, found [{m}]")));
        }
        if m < self.slices as i64 {
            return Err(EsError::illegal_argument("[max_docs] should be >= [slices]"));
        }
        self.max_docs = m;
        Ok(())
    }

    fn set_conflicts(&mut self, c: &str) -> EsResult<()> {
        self.proceed = match c {
            "proceed" => true,
            "abort" => false,
            o => return Err(EsError::illegal_argument(format!("conflicts may only be \"proceed\" or \"abort\" but was [{o}]"))),
        };
        Ok(())
    }

    /// `AbstractBaseReindexRestHandler.setCommonOptions`.
    fn common_options(&mut self, ctx: &Ctx) -> EsResult<()> {
        self.refresh = ctx.bool_param("refresh", false)?;
        ctx.validate_timeouts()?;
        if let Some(s) = ctx.query("slices") {
            self.slices = parse_slices(s)?;
        }
        if let Some(r) = parse_requests_per_second(ctx)? {
            self.rps = r;
        }
        if ctx.has_query("max_docs") {
            let m = ctx.int_param("max_docs", -1)?;
            self.set_max_docs(m)?;
        }
        Ok(())
    }

    /// `AbstractBulkByScrollRequest.validate` (with the search request's own checks first).
    fn validate_common(&self, errs: &mut Vec<String>) {
        let s = &self.search.source;
        errs.extend(crate::search::validate(&self.search));
        if self.search.request_cache == Some(true) {
            errs.push("[request_cache] cannot be used in a scroll context".into());
        }
        if s.from.is_some() {
            errs.push("from is not supported in this context".into());
        }
        if s.stored_fields.is_some() {
            errs.push("stored_fields is not supported in this context".into());
        }
        if s.slice.is_some() && self.slices != 1 {
            errs.push("can't specify both manual and automatic slicing at the same time".into());
        }
    }
}

/// `requests_per_second` (`AbstractBaseReindexRestHandler.parseRequestsPerSecond`): `-1` turns
/// throttling off.
pub fn parse_requests_per_second(ctx: &Ctx) -> EsResult<Option<f64>> {
    let Some(v) = ctx.query("requests_per_second") else { return Ok(None) };
    const MSG: &str = "[requests_per_second] must be a float greater than 0. Use -1 to disable throttling.";
    let f = match crate::units::parse_java_double(v) {
        Some(f) => f as f32,
        None => return Err(EsError::illegal_argument(MSG).caused_by(EsError::number_format(v))),
    };
    if f == -1.0 {
        return Ok(Some(-1.0));
    }
    if f.is_nan() || f <= 0.0 {
        return Err(EsError::illegal_argument(MSG));
    }
    Ok(Some(f as f64))
}

fn parse_slices(s: &str) -> EsResult<u32> {
    if s == "auto" {
        return Ok(0);
    }
    let msg = || format!("[slices] must be a positive integer or the string \"auto\", but was [{s}]");
    match crate::units::parse_java_int(s) {
        None => Err(EsError::illegal_argument(msg()).caused_by(EsError::number_format(s))),
        Some(n) if n < 1 => Err(EsError::illegal_argument(msg())),
        Some(n) => Ok(n as u32),
    }
}

/// A search request with by-scroll defaults (`scroll` 5m, `size` 1000).
fn scroll_search(ctx: &Ctx, source: SearchSource) -> EsResult<SearchRequest> {
    Ok(SearchRequest {
        indices: Vec::new(),
        opts: ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?,
        source,
        routing: None,
        preference: None,
        dfs: false,
        request_cache: None,
        total_as_int: false,
        typed_keys: false,
        allow_partial: false,
        scroll: Some(TimeValue::parse("5m", "scroll")?),
        include_named_queries_score: false,
        force_synthetic_source: false,
        fused: None,
        rank_only: false,
        security: crate::xpack::security::dls::ReadSecurity::of(ctx),
        now_ms: ctx.now_ms,
    })
}

/// Index expressions naming a remote cluster (`cluster:index`).
fn remote_expressions(indices: &[String]) -> Vec<String> {
    indices.iter().filter(|i| !i.starts_with('<') && i.contains(':')).cloned().collect()
}

fn java_class(v: &Value) -> &'static str {
    match v {
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Obj(_) => "java.util.HashMap",
        _ => "java.lang.Object",
    }
}

/// `_delete_by_query` / `_update_by_query` (`AbstractBulkByQueryRestHandler.parseInternalRequest`).
pub fn parse_by_query(ctx: &Ctx, update: bool) -> EsResult<ByScroll> {
    let body = ctx.body_map()?;
    let mut req = ByScroll::new(scroll_search(ctx, SearchSource::default())?);
    let mut rest: Option<Map> = None;
    if let Some(mut body) = body {
        // `extractRequestSpecificFields` reads the body with `parser.map()`: hash maps at every
        // level, so the re-serialized search body (and which error comes first) follows their
        // order.
        crate::ingest::doc::java_hash_order(&mut body, true);
        if let Some(c) = body.shift_remove("conflicts").filter(|v| !v.is_null()) {
            req.set_conflicts(&c.to_es_string())?;
        }
        if let Some(m) = body.shift_remove("max_docs").filter(|v| !v.is_null()) {
            let n = match &m {
                // `((Number) s).intValue()`.
                Value::Num(n) => n.as_f64() as i64 as i32 as i64,
                other => {
                    return Err(EsError::java(
                        500,
                        "class_cast_exception",
                        format!("class {} cannot be cast to class java.lang.Number", java_class(other)),
                    ));
                }
            };
            req.set_max_docs(n)?;
        }
        if update && let Some(s) = body.shift_remove("script").filter(|v| !v.is_null()) {
            let mut p = XParser::from_value(&s);
            p.next_token();
            req.script = Some(Script::parse(&mut p)?);
        }
        rest = Some(body);
    }
    let raw = rest.as_ref().map(|m| Value::Obj(m.clone()).to_json());
    let body_v = rest.map(Value::Obj);
    let mut search = crate::search::parse_request(ctx, body_v.as_ref(), raw.as_deref().map(str::as_bytes))?;
    if search.source.size.is_none() {
        search.source.size = Some(DEFAULT_SCROLL_SIZE);
    }
    if ctx.has_query("scroll_size") {
        let n = ctx.int_param("scroll_size", DEFAULT_SCROLL_SIZE as i64)?;
        // `SearchSourceBuilder.size`.
        if n < 0 {
            return Err(EsError::illegal_argument(format!("[size] parameter cannot be negative, found [{n}]")));
        }
        search.source.size = Some(n as i32);
    }
    if search.scroll.is_none() {
        search.scroll = Some(TimeValue::parse("5m", "scroll")?);
    }
    if let Some(t) = ctx.query("search_timeout") {
        search.source.timeout = Some(TimeValue::parse(t, "search_timeout")?);
    }
    req.search = search;
    if let Some(c) = ctx.query("conflicts") {
        req.set_conflicts(c)?;
    }
    if update {
        req.pipeline = ctx.query("pipeline").map(str::to_string);
    }
    req.common_options(ctx)?;
    let mut errs = Vec::new();
    req.validate_common(&mut errs);
    if req.search.indices.is_empty() {
        errs.push(format!("use _all if you really want to {} all existing indexes", if update { "update" } else { "delete from" }));
    }
    if !update && req.search.source.query.is_none() {
        errs.push("query is missing".into());
    }
    let remote = remote_expressions(&req.search.indices);
    if !remote.is_empty() {
        errs.push(format!("Cross-cluster calls are not supported in this context but remote indices were requested: [{}]", remote.join(", ")));
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    Ok(req)
}

const REINDEX_FIELDS: &[Field] = &[
    Field::new("source", Tokens::OBJECT),
    Field::new("dest", Tokens::OBJECT),
    Field::new("max_docs", Tokens::INT),
    Field::new("script", Tokens::OBJECT),
    Field::new("conflicts", Tokens::STRING),
];

const DEST_FIELDS: &[Field] = &[
    Field::new("index", Tokens::STRING),
    Field::new("routing", Tokens::STRING),
    Field::new("op_type", Tokens::STRING),
    Field::new("pipeline", Tokens::STRING),
    Field::new("version_type", Tokens::STRING),
    // `index.slice.enabled` indices: the slice value stands in for routing (same commands).
    Field::new("_slice", Tokens::STRING),
];

/// `ReindexRequest.buildRemoteInfo` (`source` is what's left of the `source` map).
fn remote_info(mut remote: Map, source: &mut Map) -> EsResult<Remote> {
    fn string(m: &mut Map, name: &str) -> EsResult<Option<String>> {
        match m.shift_remove(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(v) => Err(EsError::illegal_argument(format!("Expected [{name}] to be a string but was [{}]", v.to_es_string()))),
        }
    }
    let username = string(&mut remote, "username")?;
    let password = string(&mut remote, "password")?;
    let Some(host_in) = string(&mut remote, "host")? else {
        return Err(EsError::java(500, "null_pointer_exception", "[host] must be specified to reindex from a remote cluster"));
    };
    let mut r = parse_host(&host_in)?;
    r.username = username;
    r.password = password;
    match remote.shift_remove("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Obj(h)) => {
            for (k, v) in h {
                match v {
                    Value::Str(s) => r.headers.push((k, s)),
                    other => {
                        return Err(EsError::illegal_argument(format!(
                            "Expected [headers] to be an object containing strings but has [{k}={}]",
                            other.to_es_string()
                        )));
                    }
                }
            }
        }
        Some(v) => {
            return Err(EsError::illegal_argument(format!("Expected [headers] to be an object containing strings but was [{}]", v.to_es_string())));
        }
    }
    if let Some(s) = string(&mut remote, "socket_timeout")? {
        r.socket_timeout = Some(TimeValue::parse(&s, "socket_timeout")?);
    }
    if let Some(s) = string(&mut remote, "connect_timeout")? {
        r.connect_timeout = Some(TimeValue::parse(&s, "connect_timeout")?);
    }
    if !remote.is_empty() {
        let keys: Vec<&str> = remote.keys().map(String::as_str).collect();
        return Err(EsError::illegal_argument(format!("Unsupported fields in [remote]: [{}]", keys.join(","))));
    }
    r.query = source.shift_remove("query");
    Ok(r)
}

/// `new URI(host)` with a required port: `scheme://host:port[/path]`.
fn parse_host(h: &str) -> EsResult<Remote> {
    let bad = |why: &str| {
        EsError::illegal_argument(format!("[host] must be of the form [scheme]://[host]:[port](/[pathPrefix])? but was [{h}]"))
            .caused_by(EsError::java(400, "u_r_i_syntax_exception", format!("{why}: {h}")))
    };
    let no_port = "The port was not defined in the [host]";
    let Some((scheme, rest)) = h.split_once("://") else { return Err(bad(no_port)) };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let Some((host, port)) = authority.rsplit_once(':') else { return Err(bad(no_port)) };
    let Ok(port) = port.parse::<u16>() else { return Err(bad(no_port)) };
    if host.is_empty() || scheme.is_empty() {
        return Err(bad("Expected authority"));
    }
    Ok(Remote {
        scheme: scheme.to_string(),
        host: host.to_string(),
        port,
        path_prefix: (!path.is_empty()).then(|| path.to_string()),
        ..Default::default()
    })
}

/// `_reindex` (`RestReindexAction.buildRequest` + `ReindexRequest.validate`).
pub fn parse_reindex(ctx: &Ctx) -> EsResult<ByScroll> {
    if ctx.has_query("pipeline") {
        return Err(EsError::illegal_argument("_reindex doesn't support [pipeline] as a query parameter. Specify it in the [dest] object instead."));
    }
    let Some(body) = ctx.body()? else { return Err(EsError::parse("request body is required")) };
    let raw = ctx.raw_json();
    let mut req = ByScroll::new(scroll_search(ctx, SearchSource { size: Some(DEFAULT_SCROLL_SIZE), ..Default::default() })?);
    let mut p = XParser::new(&body, raw, Vec::new());
    p.object("reindex", REINDEX_FIELDS, |p, i| match REINDEX_FIELDS[i].name {
        "source" => {
            let mut m = p.map()?;
            // `parser.map()`: hash maps at every level (see `parse_by_query`).
            crate::ingest::doc::java_hash_order(&mut m, true);
            match m.shift_remove("index") {
                None | Some(Value::Null) => {}
                Some(Value::Arr(a)) => req.search.indices = a.iter().map(Value::to_es_string).collect(),
                Some(Value::Str(s)) => req.search.indices = s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect(),
                Some(v) => return Err(EsError::illegal_argument(format!("Expected [index] to be a list or a string but was [{}]", v.to_es_string()))),
            }
            if let Some(r) = m.shift_remove("remote").filter(|v| !v.is_null()) {
                let Value::Obj(r) = r else {
                    return Err(EsError::java(500, "class_cast_exception", format!("class {} cannot be cast to class java.util.Map", java_class(&r))));
                };
                req.remote = Some(remote_info(r, &mut m)?);
            }
            let v = Value::Obj(m);
            let json = v.to_json();
            let mut s = SearchSource::parse(&v, Some(json.as_bytes()))?;
            if s.size.is_none() {
                s.size = Some(DEFAULT_SCROLL_SIZE);
            }
            req.search.source = s;
            Ok(())
        }
        "dest" => {
            let mut d = Dest::default();
            let r = p.object("dest", DEST_FIELDS, |p, j| {
                let s = p.text()?;
                match DEST_FIELDS[j].name {
                    "index" => d.index = Some(s),
                    "routing" => d.routing = Some(s),
                    "op_type" => {
                        if s != "create" && s != "index" {
                            return Err(EsError::illegal_argument(format!("opType must be 'create' or 'index', found: [{s}]")));
                        }
                        d.op_type = Some(s);
                    }
                    "pipeline" => d.pipeline = Some(s),
                    "_slice" => d.routing = Some(s),
                    _ => d.version_type = Some(crate::write::parse_version_type(&s)?),
                }
                Ok(())
            });
            req.dest = d;
            r
        }
        "max_docs" => {
            let m = p.int_value()? as i64;
            req.set_max_docs(m)
        }
        "script" => {
            req.script = Some(Script::parse(p)?);
            Ok(())
        }
        _ => {
            let c = p.text()?;
            req.set_conflicts(&c)
        }
    })?;
    if !req.search.source.sort.is_empty() {
        ctx.warn("The sort option in reindex is deprecated. Instead consider using query filtering to find the desired subset of data.");
    }
    if let Some(s) = ctx.query("scroll") {
        req.search.scroll = Some(TimeValue::parse(s, "scroll")?);
    }
    ctx.opt_bool_param("require_alias")?;
    req.common_options(ctx)?;
    let mut errs = Vec::new();
    req.validate_common(&mut errs);
    if req.search.indices.is_empty() {
        errs.push("use _all if you really want to copy from all existing indexes".into());
    }
    if req.search.source.source.as_ref().is_some_and(|f| !f.fetch) {
        errs.push("_source:false is not supported in this context".into());
    }
    if req.dest.index.is_none() {
        errs.push("index must be specified".into());
        return Err(EsError::action_request_validation(&errs));
    }
    if let Some(r) = &req.dest.routing
        && !(r == "keep" || r == "discard" || r.starts_with('='))
    {
        errs.push("routing must be unset, [keep], [discard] or [=<some new value>]".into());
    }
    if req.dest.op_type.as_deref() == Some("create") && req.dest.version_type.is_some_and(|v| v != search_index::VersionType::Internal) {
        errs.push("create operations only support internal versioning. use index instead".into());
    }
    if req.remote.is_some() && req.slices != 1 {
        errs.push(format!("reindex from remote sources doesn't support slices > 1 but was [{}]", req.slices));
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    Ok(req)
}
