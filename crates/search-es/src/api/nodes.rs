//! Node APIs (`_nodes`, `_nodes/stats`, `_nodes/usage`) for the one node a roll cluster reports.
//!
//! A roll cluster is served by a node shared with other tenants, so by default the numbers are
//! fixed synthetic figures ([`NodeMetrics::synthetic`]): the process's memory, load, file
//! descriptors and the host's disks would tell one tenant about the others. A dedicated node
//! (`ClusterConfig::host_metrics`, e.g. the dev server) reports the process's real numbers
//! instead, sampled from `/proc` and `statvfs` at most once a second; nothing is collected in
//! the background. JVM-specific sections report resident memory as "heap" so dashboards that
//! read them keep working.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::version::*;

/// The roles of the node, in Elasticsearch's order (those of a default single-node cluster).
pub const ROLES: &[&str] = &[
    "data", "data_cold", "data_content", "data_frozen", "data_hot", "data_warm", "ingest", "master", "ml", "remote_cluster_client", "transform",
];

/// An OpenSearch node's default roles.
pub const OS_ROLES: &[&str] = &["cluster_manager", "data", "ingest", "remote_cluster_client"];

/// The node's roles in a dialect.
pub fn roles(d: crate::cluster::Dialect) -> &'static [&'static str] {
    match d {
        crate::cluster::Dialect::Elasticsearch => ROLES,
        crate::cluster::Dialect::OpenSearch => OS_ROLES,
    }
}

/// `DiscoveryNode.getRoleAbbreviationString()` for the roles we report.
pub fn role_abbreviations(d: crate::cluster::Dialect) -> &'static str {
    match d {
        crate::cluster::Dialect::Elasticsearch => "cdfhilmrstw",
        crate::cluster::Dialect::OpenSearch => "dimr",
    }
}

/// Ingest processor types the node reports (and pipelines may use).
pub const INGEST_PROCESSORS: &[&str] = &[
    "append", "attachment", "bytes", "cef", "circle", "community_id", "convert", "csv", "date", "date_index_name", "dissect",
    "dot_expander", "drop", "enrich", "fail", "fingerprint", "foreach", "geo_grid", "geoip", "grok", "gsub", "html_strip",
    "inference", "ip_location", "join", "json", "kv", "lowercase", "network_direction", "normalize_for_stream", "pipeline",
    "recover_failure_document", "redact", "registered_domain", "remove", "rename", "reroute", "script", "set",
    "set_security_user", "sort", "split", "terminate", "trim", "uppercase", "uri_parts", "urldecode", "user_agent",
];

/// A sample of process and machine numbers.
#[derive(Clone, Debug, Default)]
pub struct NodeMetrics {
    pub pid: u32,
    pub processors: usize,
    pub rss: i64,
    pub heap_max: i64,
    pub mem_total: i64,
    pub mem_used: i64,
    pub mem_free: i64,
    pub open_fds: i64,
    pub max_fds: i64,
    pub cpu_percent: i64,
    pub load: [f64; 3],
    pub disk_total: i64,
    pub disk_free: i64,
    pub disk_avail: i64,
    pub threads: i64,
    pub os_version: String,
    /// The file system type of the data directory.
    pub fs_type: String,
    pub arch: &'static str,
}

impl NodeMetrics {
    pub fn sample(cluster: &Cluster) -> NodeMetrics {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "aarch64",
            other => other,
        };
        let mut m = if cluster.config.host_metrics { Self::host() } else { Self::synthetic() };
        m.arch = arch;
        // `node.processors` caps what the node reports (Elasticsearch also sizes the JVM's
        // active processor count from it).
        if let Some(p) = cluster.config.processors() {
            m.processors = p;
        }
        m
    }

    /// What a shared node reports: one processor, 1 GiB of memory (the heap half of it), an
    /// idle machine, a 1 TiB disk; nothing measured.
    pub fn synthetic() -> NodeMetrics {
        const GIB: i64 = 1 << 30;
        NodeMetrics {
            pid: 1,
            processors: 1,
            heap_max: GIB / 2,
            mem_total: GIB,
            mem_free: GIB,
            max_fds: 65535,
            disk_total: 1024 * GIB,
            disk_free: 1024 * GIB,
            disk_avail: 1024 * GIB,
            threads: 1,
            os_version: String::new(),
            fs_type: "roll".into(),
            ..Default::default()
        }
    }

    /// The process's and host's numbers, sampled at most once a second.
    fn host() -> NodeMetrics {
        static LAST: parking_lot::Mutex<Option<(std::time::Instant, NodeMetrics)>> = parking_lot::Mutex::new(None);
        let mut last = LAST.lock();
        if let Some((at, m)) = last.as_ref()
            && at.elapsed() < std::time::Duration::from_secs(1)
        {
            return m.clone();
        }
        let mut m = NodeMetrics {
            pid: std::process::id(),
            processors: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            ..Default::default()
        };
        #[cfg(target_os = "linux")]
        linux::fill(&mut m);
        if m.heap_max == 0 {
            m.heap_max = (m.mem_total / 2).max(m.rss);
        }
        *last = Some((std::time::Instant::now(), m.clone()));
        m
    }

    pub fn heap_percent(&self) -> i64 {
        if self.heap_max > 0 { self.rss * 100 / self.heap_max } else { 0 }
    }

    pub fn ram_percent(&self) -> i64 {
        if self.mem_total > 0 { (self.mem_used as f64 * 100.0 / self.mem_total as f64).ceil() as i64 } else { 0 }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::NodeMetrics;

    fn kb_field(text: &str, key: &str) -> Option<i64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        line[key.len()..].split_whitespace().next()?.parse::<i64>().ok().map(|kb| kb * 1024)
    }

    pub fn fill(m: &mut NodeMetrics) {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            m.rss = kb_field(&s, "VmRSS:").unwrap_or(0);
            m.threads = s.lines().find(|l| l.starts_with("Threads:")).and_then(|l| l[8..].trim().parse().ok()).unwrap_or(0);
        }
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            m.mem_total = kb_field(&s, "MemTotal:").unwrap_or(0);
            m.mem_free = kb_field(&s, "MemAvailable:").or_else(|| kb_field(&s, "MemFree:")).unwrap_or(0);
            m.mem_used = (m.mem_total - m.mem_free).max(0);
        }
        if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
            let mut it = s.split_whitespace().map(|x| x.parse::<f64>().unwrap_or(0.0));
            m.load = [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)];
        }
        if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
            m.open_fds = rd.count() as i64;
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/limits")
            && let Some(l) = s.lines().find(|l| l.starts_with("Max open files"))
        {
            m.max_fds = l.split_whitespace().nth(3).and_then(|v| v.parse().ok()).unwrap_or(0);
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/mounts") {
            m.fs_type = s
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let (_, mount, ty) = (it.next()?, it.next()?, it.next()?);
                    Some((mount.to_string(), ty.to_string()))
                })
                .find(|(mount, _)| mount == "/")
                .map(|(_, ty)| ty)
                .unwrap_or_default();
        }
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            m.os_version = s.trim().to_string();
        }
        // SAFETY: statvfs fills the zeroed struct for a valid NUL-terminated path.
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c".".as_ptr(), &mut st) == 0 {
                let f = st.f_frsize as i64;
                m.disk_total = st.f_blocks as i64 * f;
                m.disk_free = st.f_bfree as i64 * f;
                m.disk_avail = st.f_bavail as i64 * f;
            }
        }
    }
}

/// Node attributes (`_cat/nodeattrs`, node info): the ones Elasticsearch sets itself plus the
/// `node.attr.*` node settings, in `DiscoveryNode`'s (Java `HashMap`) order.
pub fn node_attributes(cluster: &Cluster, m: &NodeMetrics) -> Vec<(String, String)> {
    let mut attrs: Vec<(String, String)> = match cluster.dialect {
        crate::cluster::Dialect::Elasticsearch => vec![("xpack.installed".into(), "true".into()), ("ml.allocated_processors".into(), m.processors.to_string())],
        crate::cluster::Dialect::OpenSearch => vec![("shard_indexing_pressure_enabled".into(), "true".into())],
    };
    for (k, v) in &cluster.config.node_settings {
        if let Some(name) = k.strip_prefix("node.attr.") {
            match attrs.iter_mut().find(|(n, _)| n == name) {
                Some(e) => e.1 = v.clone(),
                None => attrs.push((name.to_string(), v.clone())),
            }
        }
    }
    let order = crate::json::java_hash_order(&attrs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), 16);
    order.into_iter().map(|i| attrs[i].clone()).collect()
}

/// `DiscoveryNode.toString()`: `{name}{id}{ephemeralId}{host}{ip}{transport}{roles}{version}
/// {minIndexVersion-maxIndexVersion}{attributes}`, as allocation messages print a node.
pub fn discovery_node_string(cluster: &Cluster) -> String {
    let n = &cluster.node;
    let m = NodeMetrics::sample(cluster);
    let attrs: Vec<String> = node_attributes(cluster, &m).into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(
        "{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{ES_VERSION}}}{{{MIN_INDEX_VERSION_ID}-{INDEX_VERSION_ID}}}{{{}}}",
        n.name,
        n.id,
        n.ephemeral_id,
        n.host,
        n.ip,
        n.transport_address,
        role_abbreviations(cluster.dialect),
        attrs.join(", ")
    )
}

const INFO_METRICS: &[&str] =
    &["settings", "os", "process", "jvm", "thread_pool", "transport", "http", "remote_cluster_server", "plugins", "ingest", "aggregations", "indices"];

/// Splits `/_nodes/{x}` into node ids or metrics the way `RestNodesInfoAction` does.
fn node_ids_and_metrics(ctx: &Ctx, metrics_known: &[&str]) -> EsResult<(Vec<String>, Option<Vec<String>>)> {
    let node_id = ctx.path_param("node_id").map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>());
    let metric = ctx.path_param("metric").map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>());
    match (node_id, metric) {
        (Some(n), Some(m)) => Ok((n, Some(m))),
        (None, Some(m)) => {
            // `/_nodes/{x}`: metrics when every entry is a metric name, node ids otherwise.
            if m.iter().all(|x| metrics_known.contains(&x.as_str()) || x == "_all" || x == "_none") {
                Ok((vec!["_all".into()], Some(m)))
            } else {
                Ok((m, None))
            }
        }
        // `/_nodes/{x}` routes with the part named `node_id`: metrics when every entry is one.
        (Some(n), None) if n.iter().all(|x| metrics_known.contains(&x.as_str()) || x == "_all" || x == "_none") && !n.iter().all(|x| x == "_all") => {
            Ok((vec!["_all".into()], Some(n)))
        }
        (Some(n), None) => Ok((n, None)),
        (None, None) => Ok((vec!["_all".into()], None)),
    }
}

/// Whether node selectors pick our single node.
pub(crate) fn selects_node(ctx: &Ctx, selectors: &[String]) -> bool {
    let node = &ctx.cluster.node;
    let mut selected = false;
    for s in selectors {
        let s = s.trim();
        match s {
            "_all" | "*" | "_local" | "_master" => selected = true,
            _ if s.starts_with("master:") || s.starts_with("data:") || s.starts_with("ingest:") => {
                selected = s.ends_with(":true") || s.ends_with(":*");
            }
            _ => {
                if crate::xcontent::filter::glob_match(s, &node.name)
                    || s == node.id
                    || s == node.ip
                    || s == node.host
                    || crate::xcontent::filter::glob_match(s, &node.ip)
                {
                    selected = true;
                }
            }
        }
    }
    selected
}

fn header(ctx: &Ctx, selected: bool) -> Map {
    let n = if selected { 1 } else { 0 };
    let mut m = Map::new();
    m.insert("_nodes".into(), obj! { "total" => n, "successful" => n, "failed" => 0 });
    m.insert("cluster_name".into(), ctx.cluster.name.clone().into());
    m
}

fn thread_pool_info(ctx: &Ctx, m: &NodeMetrics) -> Value {
    let mut tp = Map::new();
    for p in super::cat::thread_pools_for(ctx.cluster.dialect, m.processors) {
        let v = if p.fixed {
            obj! { "type" => if p.resizable { "resizable" } else { "fixed" }, "size" => p.size, "queue_size" => p.queue_size }
        } else {
            obj! { "type" => "scaling", "core" => p.core, "max" => p.size, "keep_alive" => p.keep_alive, "queue_size" => -1 }
        };
        tp.insert(p.name.to_string(), v);
    }
    Value::Obj(tp)
}

pub fn info(ctx: &Ctx) -> EsResult<Reply> {
    let mut r = info_inner(ctx)?;
    if ctx.human
        && let Reply::Body(_, v) = &mut r
    {
        crate::units::humanize(v);
    }
    Ok(r)
}

fn info_inner(ctx: &Ctx) -> EsResult<Reply> {
    let (ids, metrics) = node_ids_and_metrics(ctx, INFO_METRICS)?;
    let selected = selects_node(ctx, &ids);
    let mut out = header(ctx, selected);
    let mut nodes = Map::new();
    if selected {
        let node = &ctx.cluster.node;
        let m = NodeMetrics::sample(ctx.cluster);
        let want = |s: &str| match &metrics {
            None => true,
            Some(ms) => ms.iter().any(|x| x == s || x == "_all"),
        };
        let mut n = Map::new();
        n.insert("name".into(), node.name.clone().into());
        n.insert("transport_address".into(), node.transport_address.clone().into());
        n.insert("host".into(), node.host.clone().into());
        n.insert("ip".into(), node.ip.clone().into());
        let opensearch = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
        if opensearch {
            n.insert("version".into(), OPENSEARCH_VERSION.into());
            n.insert("build_type".into(), "tar".into());
            n.insert("build_hash".into(), OPENSEARCH_BUILD_HASH.into());
        } else {
            n.insert("version".into(), ES_VERSION.into());
            n.insert("transport_version".into(), Value::from(TRANSPORT_VERSION));
            n.insert("index_version".into(), Value::from(INDEX_VERSION_ID.parse::<i64>().unwrap_or(0)));
            n.insert("component_versions".into(), obj! {});
            n.insert("build_flavor".into(), "default".into());
            n.insert("build_type".into(), "docker".into());
            n.insert("build_hash".into(), ES_BUILD_HASH.into());
        }
        if metrics.as_ref().is_none_or(|ms| ms.iter().any(|x| x == "_all" || x == "indices")) {
            // OpenSearch 3 names the byte count `_in_bytes` (with a `?human` variant).
            if opensearch {
                if ctx.human {
                    n.insert("total_indexing_buffer".into(), crate::units::ByteSize(m.heap_max / 10).human().into());
                }
                n.insert("total_indexing_buffer_in_bytes".into(), Value::from(m.heap_max / 10));
            } else {
                n.insert("total_indexing_buffer".into(), Value::from(m.heap_max / 10));
            }
        }
        n.insert("roles".into(), Value::from(roles(ctx.cluster.dialect).to_vec()));
        let mut attrs = Map::new();
        for (k, v) in node_attributes(ctx.cluster, &m) {
            attrs.insert(k, v.into());
        }
        n.insert("attributes".into(), Value::Obj(attrs));
        if want("settings") {
            let mut s = crate::meta::settings::Settings::new();
            s.set("cluster.name", ctx.cluster.name.clone());
            for (k, v) in node_attributes(ctx.cluster, &m) {
                s.set(format!("node.attr.{k}"), v);
            }
            s.set("node.name", node.name.clone());
            for (k, v) in &ctx.cluster.config.node_settings {
                if !k.starts_with("node.attr.") {
                    s.set(k.clone(), v.clone());
                }
            }
            s.set("discovery.type", "single-node");
            s.set("http.type.default", "roll");
            s.set("transport.type.default", "roll");
            if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
                s.set("client.type", "node");
            }
            n.insert("settings".into(), s.to_json(ctx.flat_settings()?));
        }
        if want("os") {
            n.insert(
                "os".into(),
                obj! {
                    "refresh_interval_in_millis" => 1000,
                    "name" => "Linux",
                    "pretty_name" => "Linux",
                    "arch" => m.arch,
                    "version" => m.os_version.clone(),
                    "available_processors" => m.processors,
                    "allocated_processors" => m.processors,
                },
            );
        }
        if want("process") {
            n.insert("process".into(), obj! { "refresh_interval_in_millis" => 1000, "id" => m.pid, "mlockall" => false });
        }
        if want("jvm") {
            n.insert(
                "jvm".into(),
                obj! {
                    "pid" => m.pid,
                    "version" => "27",
                    "vm_name" => "roll",
                    "vm_version" => ES_VERSION,
                    "vm_vendor" => "roll",
                    "using_bundled_jdk" => Value::Null,
                    "start_time_in_millis" => crate::time::now_millis() - ctx.cluster.uptime_ms() as i64,
                    "mem" => obj! {
                        "heap_init_in_bytes" => m.heap_max,
                        "heap_max_in_bytes" => m.heap_max,
                        "non_heap_init_in_bytes" => 0,
                        "non_heap_max_in_bytes" => 0,
                        "direct_max_in_bytes" => 0,
                    },
                    "gc_collectors" => Value::Arr(Vec::new()),
                    "memory_pools" => Value::Arr(Vec::new()),
                    "using_compressed_ordinary_object_pointers" => "true",
                    "input_arguments" => Value::Arr(Vec::new()),
                },
            );
        }
        if want("thread_pool") {
            n.insert("thread_pool".into(), thread_pool_info(ctx, &m));
        }
        if want("transport") {
            n.insert(
                "transport".into(),
                obj! { "bound_address" => vec![node.transport_address.clone()], "publish_address" => node.transport_address.clone(), "profiles" => obj! {} },
            );
        }
        if want("http") {
            n.insert(
                "http".into(),
                obj! { "bound_address" => vec![node.http_address.clone()], "publish_address" => node.http_address.clone(), "max_content_length_in_bytes" => MAX_CONTENT_LENGTH },
            );
        }
        if want("plugins") {
            n.insert("plugins".into(), Value::Arr(Vec::new()));
            n.insert("modules".into(), Value::Arr(MODULES.iter().map(|m| m.to_value()).collect()));
        }
        if want("ingest") {
            let procs: Vec<Value> = INGEST_PROCESSORS.iter().map(|p| obj! { "type" => *p }).collect();
            n.insert("ingest".into(), obj! { "processors" => Value::Arr(procs) });
        }
        if want("aggregations") {
            let aggs = crate::json::parse(include_bytes!("../../assets/node_aggregations.json")).ok().flatten().unwrap_or_default();
            n.insert("aggregations".into(), aggs);
        }
        if want("indices") && metrics.is_some() {
            n.insert("indices".into(), obj! { "total_indexing_buffer_in_bytes" => m.heap_max / 10 });
        }
        nodes.insert(node.id.clone(), Value::Obj(n));
    }
    out.insert("nodes".into(), Value::Obj(nodes));
    Reply::ok(Value::Obj(out))
}

/// `http.max_content_length`.
pub const MAX_CONTENT_LENGTH: i64 = 100 * 1024 * 1024;

const STATS_METRICS: &[&str] = &[
    "_all", "adaptive_selection", "allocations", "breaker", "discovery", "fs", "http", "indexing_pressure", "indices", "ingest", "jvm",
    "os", "process", "repositories", "script", "script_cache", "thread_pool", "transport",
];

const INDEX_METRICS: &[&str] = &[
    "_all", "bulk", "completion", "dense_vector", "docs", "fielddata", "flush", "get", "indexing", "mappings", "merge", "query_cache",
    "recovery", "refresh", "request_cache", "search", "segments", "shard_stats", "sparse_vector", "store", "translog", "warmer",
];

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let mut r = stats_inner(ctx)?;
    if ctx.human
        && let Reply::Body(_, v) = &mut r
    {
        crate::units::humanize(v);
    }
    Ok(r)
}

fn stats_inner(ctx: &Ctx) -> EsResult<Reply> {
    let ids: Vec<String> = ctx.list_param("node_id").unwrap_or_else(|| vec!["_all".into()]);
    let metrics: Option<Vec<String>> = ctx.list_param("metric");
    if let Some(ms) = &metrics {
        let bad: Vec<&String> = ms.iter().filter(|m| !STATS_METRICS.contains(&m.as_str())).collect();
        if let Some(b) = bad.first() {
            let mut cands = std::collections::BTreeSet::new();
            for s in STATS_METRICS {
                cands.insert(s.to_string());
            }
            let inv: std::collections::BTreeSet<String> = bad.iter().map(|s| s.to_string()).collect();
            let _ = b;
            return Err(EsError::illegal_argument(crate::rest::params::unrecognized(&ctx.path, &inv, &cands, "metric")));
        }
    }
    if let Some(im) = ctx.list_param("index_metric") {
        let bad: std::collections::BTreeSet<String> = im.iter().filter(|m| !INDEX_METRICS.contains(&m.as_str())).cloned().collect();
        if !bad.is_empty() {
            let cands = INDEX_METRICS.iter().map(|s| s.to_string()).collect();
            return Err(EsError::illegal_argument(crate::rest::params::unrecognized(&ctx.path, &bad, &cands, "index metric")));
        }
    }
    let selected = selects_node(ctx, &ids);
    let mut out = header(ctx, selected);
    let mut nodes = Map::new();
    if selected {
        let node = &ctx.cluster.node;
        let m = NodeMetrics::sample(ctx.cluster);
        let want = |s: &str| match &metrics {
            None => true,
            Some(ms) => ms.iter().any(|x| x == s || x == "_all"),
        };
        let now = crate::time::now_millis();
        let mut n = Map::new();
        n.insert("timestamp".into(), now.into());
        n.insert("name".into(), node.name.clone().into());
        n.insert("transport_address".into(), node.transport_address.clone().into());
        n.insert("host".into(), node.host.clone().into());
        n.insert("ip".into(), vec![node.transport_address.clone(), "NONE".to_string()].into());
        n.insert("roles".into(), Value::from(roles(ctx.cluster.dialect).to_vec()));
        let mut attrs = Map::new();
        for (k, v) in node_attributes(ctx.cluster, &m) {
            attrs.insert(k, v.into());
        }
        n.insert("attributes".into(), Value::Obj(attrs));
        if want("indices") {
            n.insert("indices".into(), node_indices_stats(ctx)?);
        }
        if want("os") {
            n.insert(
                "os".into(),
                obj! {
                    "timestamp" => now,
                    "cpu" => obj! { "percent" => m.cpu_percent, "load_average" => obj! { "1m" => m.load[0], "5m" => m.load[1], "15m" => m.load[2] } },
                    "mem" => obj! {
                        "total_in_bytes" => m.mem_total, "adjusted_total_in_bytes" => m.mem_total, "free_in_bytes" => m.mem_free,
                        "used_in_bytes" => m.mem_used, "free_percent" => 100 - m.ram_percent(), "used_percent" => m.ram_percent(),
                    },
                    "swap" => obj! { "total_in_bytes" => 0, "free_in_bytes" => 0, "used_in_bytes" => 0 },
                },
            );
        }
        if want("process") {
            n.insert(
                "process".into(),
                obj! {
                    "timestamp" => now,
                    "open_file_descriptors" => m.open_fds,
                    "max_file_descriptors" => m.max_fds,
                    "cpu" => obj! { "percent" => m.cpu_percent, "total_in_millis" => 0 },
                    "mem" => obj! { "total_virtual_in_bytes" => m.rss },
                },
            );
        }
        if want("jvm") {
            n.insert(
                "jvm".into(),
                obj! {
                    "timestamp" => now,
                    "uptime_in_millis" => ctx.cluster.uptime_ms(),
                    "mem" => obj! {
                        "heap_used_in_bytes" => m.rss, "heap_used_percent" => m.heap_percent(), "heap_committed_in_bytes" => m.rss,
                        "heap_max_in_bytes" => m.heap_max, "non_heap_used_in_bytes" => 0, "non_heap_committed_in_bytes" => 0,
                        "pools" => obj! {},
                    },
                    "threads" => obj! { "count" => m.threads, "peak_count" => m.threads },
                    "gc" => obj! { "collectors" => obj! {} },
                    "buffer_pools" => obj! {
                        "direct" => obj! { "count" => 0, "used_in_bytes" => 0, "total_capacity_in_bytes" => 0 },
                        "mapped" => obj! { "count" => 0, "used_in_bytes" => 0, "total_capacity_in_bytes" => 0 },
                    },
                    "classes" => obj! { "current_loaded_count" => 0, "total_loaded_count" => 0, "total_unloaded_count" => 0 },
                },
            );
        }
        if want("thread_pool") {
            let mut tp = Map::new();
            for p in super::cat::thread_pools_for(ctx.cluster.dialect, m.processors) {
                let mut s = obj! { "threads" => 0, "queue" => 0, "active" => 0, "rejected" => 0, "largest" => 0, "completed" => 0 };
                if p.resizable && let Some(o) = s.as_object_mut() {
                    o.insert("total_wait_time_in_nanos".into(), 0.into());
                }
                tp.insert(p.name.to_string(), s);
            }
            n.insert("thread_pool".into(), Value::Obj(tp));
        }
        if want("fs") {
            n.insert(
                "fs".into(),
                obj! {
                    "timestamp" => now,
                    "total" => obj! { "total_in_bytes" => m.disk_total, "free_in_bytes" => m.disk_free, "available_in_bytes" => m.disk_avail },
                    "data" => Value::Arr(vec![obj! {
                        "path" => "/data", "mount" => "/", "type" => if m.fs_type.is_empty() { "unknown".to_string() } else { m.fs_type.clone() },
                        "total_in_bytes" => m.disk_total, "free_in_bytes" => m.disk_free,
                        "available_in_bytes" => m.disk_avail,
                        "low_watermark_free_space_in_bytes" => m.disk_total / 100 * 15,
                        "high_watermark_free_space_in_bytes" => m.disk_total / 100 * 10,
                        "flood_stage_free_space_in_bytes" => m.disk_total / 100 * 5,
                    }]),
                },
            );
        }
        if want("transport") {
            n.insert(
                "transport".into(),
                obj! {
                    "server_open" => 0, "total_outbound_connections" => 0, "rx_count" => 0, "rx_size_in_bytes" => 0, "tx_count" => 0, "tx_size_in_bytes" => 0,
                    "inbound_handling_time_histogram" => Value::Arr(vec![obj! { "lt_millis" => 1, "count" => 0 }]),
                    "outbound_handling_time_histogram" => Value::Arr(vec![obj! { "lt_millis" => 1, "count" => 0 }]),
                    "actions" => obj! {},
                },
            );
        }
        if want("http") {
            let now = crate::time::now_millis();
            let client = obj! {
                "id" => 1,
                "agent" => ctx.request.header("user-agent").unwrap_or(""),
                "local_address" => ctx.cluster.node.http_address.clone(),
                "remote_address" => "127.0.0.1:0",
                "last_uri" => ctx.uri.clone(),
                "opened_time_millis" => now,
                "last_request_time_millis" => now,
                "request_count" => 1,
                "request_size_bytes" => ctx.body.len(),
            };
            n.insert("http".into(), obj! { "current_open" => 1, "total_opened" => 1, "clients" => Value::Arr(vec![client]), "routes" => obj! {} });
        }
        if want("breaker") {
            let mut bs = Map::new();
            for (name, limit, overhead) in breakers(&m) {
                bs.insert(
                    name.to_string(),
                    obj! { "limit_size_in_bytes" => limit, "limit_size" => crate::units::ByteSize(limit).human(), "estimated_size_in_bytes" => 0, "estimated_size" => "0b", "overhead" => overhead, "tripped" => 0 },
                );
            }
            n.insert("breakers".into(), Value::Obj(bs));
        }
        if want("script") {
            n.insert("script".into(), obj! { "compilations" => 0, "cache_evictions" => 0, "compilation_limit_triggered" => 0 });
        }
        if want("ingest") {
            n.insert(
                "ingest".into(),
                obj! { "total" => obj! { "count" => 0, "time_in_millis" => 0, "current" => 0, "failed" => 0 }, "pipelines" => obj! {} },
            );
        }
        if want("indexing_pressure") {
            n.insert("indexing_pressure".into(), indexing_pressure(m.heap_max / 10));
        }
        if want("discovery") {
            let timings = || {
                obj! {
                    "count" => 0, "computation_time_millis" => 0, "notification_time_millis" => 0, "context_construction_time_millis" => 0,
                    "commit_time_millis" => 0, "completion_time_millis" => 0, "master_apply_time_millis" => 0, "publication_time_millis" => 0,
                }
            };
            let sizes = || obj! { "count" => 0, "uncompressed_size_in_bytes" => 0, "compressed_size_in_bytes" => 0 };
            n.insert(
                "discovery".into(),
                obj! {
                    "cluster_state_queue" => obj! { "total" => 0, "pending" => 0, "committed" => 0 },
                    "serialized_cluster_states" => obj! { "full_states" => sizes(), "diffs" => sizes() },
                    "published_cluster_states" => obj! { "full_states" => 0, "incompatible_diffs" => 0, "compatible_diffs" => 0 },
                    "cluster_state_update" => obj! {
                        "unchanged" => obj! { "count" => 0, "computation_time_millis" => 0, "notification_time_millis" => 0 },
                        "success" => timings(),
                        "failure" => timings(),
                    },
                    "cluster_applier_stats" => obj! {
                        "recordings" => Value::Arr(vec![obj! { "name" => "apply cluster state", "cumulative_execution_count" => 1, "cumulative_execution_time_millis" => 1 }]),
                    },
                },
            );
        }
        if want("repositories") {
            let st = ctx.state();
            let repos: Map = st
                .metadata
                .repositories
                .keys()
                .map(|k| (k.clone(), obj! { "total_read_throttled_time_nanos" => 0, "total_write_throttled_time_nanos" => 0 }))
                .collect();
            n.insert("repositories".into(), Value::Obj(repos));
        }
        if want("allocations") {
            let h = super::cluster::compute_health(ctx.cluster, &ctx.state(), None);
            n.insert(
                "allocations".into(),
                obj! { "shards" => h.active_shards, "undesired_shards" => 0, "forecasted_ingest_load" => 0.0, "forecasted_disk_usage_in_bytes" => 0, "current_disk_usage_in_bytes" => 0 },
            );
        }
        nodes.insert(node.id.clone(), Value::Obj(n));
    }
    out.insert("nodes".into(), Value::Obj(nodes));
    Reply::ok(Value::Obj(out))
}

/// The circuit breakers (name, limit, overhead), with Elasticsearch's default limits relative to
/// the node's memory budget.
pub fn breakers(m: &NodeMetrics) -> Vec<(&'static str, i64, f64)> {
    let heap = m.heap_max;
    vec![
        ("request", heap * 60 / 100, 1.0),
        ("fielddata", heap * 40 / 100, 1.03),
        ("inflight_requests", heap, 2.0),
        ("parent", heap * 95 / 100, 1.0),
    ]
}

/// `indexing_pressure` with every counter at zero.
pub(crate) fn indexing_pressure(limit: i64) -> Value {
    let current = obj! {
        "combined_coordinating_and_primary_in_bytes" => 0, "coordinating_in_bytes" => 0, "primary_in_bytes" => 0, "replica_in_bytes" => 0, "all_in_bytes" => 0,
    };
    let total = obj! {
        "combined_coordinating_and_primary_in_bytes" => 0, "coordinating_in_bytes" => 0, "primary_in_bytes" => 0, "replica_in_bytes" => 0, "all_in_bytes" => 0,
        "coordinating_rejections" => 0, "primary_rejections" => 0, "replica_rejections" => 0, "primary_document_rejections" => 0,
        "large_operation_rejections" => 0,
    };
    obj! { "memory" => obj! { "current" => current, "total" => total, "limit_in_bytes" => limit } }
}

/// The `indices` section of node stats: `index_metric` filtering, `level=indices|shards`, and
/// the node-level `mappings` and `shard_stats` counters.
fn node_indices_stats(ctx: &Ctx) -> EsResult<Value> {
    let st = ctx.state();
    let level = ctx.query("level").unwrap_or("node");
    if !matches!(level, "node" | "indices" | "shards") {
        return Err(EsError::illegal_argument(format!("level parameter must be one of [node] or [indices] or [shards] but was [{level}]")));
    }
    let wanted: Vec<String> = ctx.list_param("index_metric").unwrap_or_default();
    let open: Vec<&crate::meta::IndexMeta> = st.metadata.indices.values().filter(|i| !i.is_closed()).map(|i| i.as_ref()).collect();
    let shards: u64 = open.iter().map(|i| i.number_of_shards() as u64).sum();
    let d = ctx.cluster.dialect;
    let os = d == crate::cluster::Dialect::OpenSearch;
    let mut idx = crate::api::indices::stats_block_in(d, &wanted, shards);
    let all = wanted.is_empty() || wanted.iter().any(|w| w == "_all");
    if let Some(o) = idx.as_object_mut() {
        if os {
            // OpenSearch's per-status request counters, whatever metrics were asked for.
            let zeros = || obj! { "success" => 0, "user_error" => 0, "system_failure" => 0 };
            o.insert("status_counter".into(), obj! { "doc_status" => zeros(), "search_response_status" => zeros() });
        } else if all || wanted.iter().any(|w| w == "mappings") {
            let fields: usize = open.iter().map(|i| i.mapping.total_fields()).sum();
            o.insert(
                "mappings".into(),
                obj! { "total_count" => fields, "total_estimated_overhead_in_bytes" => fields as u64 * 1024, "total_segments" => 0, "total_segment_fields" => 0, "average_fields_per_segment" => 0 },
            );
        }
        match level {
            "indices" => {
                let mut per = Map::new();
                for i in &open {
                    let mut b = crate::api::indices::stats_block_in(d, &wanted, i.number_of_shards() as u64);
                    if let Some(bo) = b.as_object_mut() {
                        bo.shift_remove("shard_stats");
                    }
                    per.insert(i.name.clone(), b);
                }
                o.insert("indices".into(), Value::Obj(per));
            }
            "shards" => {
                let mut per = Map::new();
                for i in &open {
                    let list: Vec<Value> = (0..i.number_of_shards())
                        .map(|s| {
                            let mut b = crate::api::indices::stats_block_in(d, &wanted, 1);
                            if let Some(bo) = b.as_object_mut() {
                                bo.shift_remove("shard_stats");
                            }
                            let mut m = Map::new();
                            m.insert(s.to_string(), b);
                            Value::Obj(m)
                        })
                        .collect();
                    per.insert(i.name.clone(), Value::Arr(list));
                }
                o.insert("shards".into(), Value::Obj(per));
            }
            _ => {}
        }
    }
    Ok(idx)
}

pub fn usage(ctx: &Ctx) -> EsResult<Reply> {
    let ids: Vec<String> = ctx.list_param("node_id").unwrap_or_else(|| vec!["_all".into()]);
    let selected = selects_node(ctx, &ids);
    let mut out = header(ctx, selected);
    let mut nodes = Map::new();
    if selected {
        let now = crate::time::now_millis();
        nodes.insert(
            ctx.cluster.node.id.clone(),
            obj! {
                "timestamp" => now,
                "since" => now - ctx.cluster.uptime_ms() as i64,
                "rest_actions" => obj! {},
                "aggregations" => obj! {},
            },
        );
    }
    out.insert("nodes".into(), Value::Obj(nodes));
    Reply::ok(Value::Obj(out))
}

/// A module Elasticsearch reports in `GET _nodes/plugins`.
struct Module {
    name: &'static str,
    description: &'static str,
    classname: &'static str,
    extended_plugins: &'static [&'static str],
}

impl Module {
    fn to_value(&self) -> Value {
        obj! {
            "name" => self.name,
            "version" => crate::version::ES_VERSION,
            "elasticsearch_version" => crate::version::ES_VERSION,
            "java_version" => "21",
            "description" => self.description,
            "classname" => self.classname,
            "extended_plugins" => self.extended_plugins.iter().map(|p| Value::from(*p)).collect::<Vec<_>>(),
            "has_native_controller" => false,
            "licensed" => false,
        }
    }
}

/// The Elasticsearch modules whose features roll search provides, as Elasticsearch 9.5.5
/// describes them (clients and test runners probe this list for features).
const MODULES: &[Module] = &[
    Module { name: "aggregations", description: "Adds \"built in\" aggregations to Elasticsearch.", classname: "org.elasticsearch.aggregations.AggregationsPlugin", extended_plugins: &["lang-painless"] },
    Module { name: "analysis-common", description: "Adds \"built in\" analyzers to Elasticsearch.", classname: "org.elasticsearch.analysis.common.CommonAnalysisPlugin", extended_plugins: &["lang-painless"] },
    Module { name: "constant-keyword", description: "Module for the constant-keyword field type, which is a specialization of keyword for the case when all documents have the same value.", classname: "org.elasticsearch.xpack.constantkeyword.ConstantKeywordMapperPlugin", extended_plugins: &["x-pack-core", "lang-painless"] },
    Module { name: "data-streams", description: "Elasticsearch Expanded Pack Plugin - Data Streams", classname: "org.elasticsearch.datastreams.DataStreamsPlugin", extended_plugins: &[] },
    Module { name: "ingest-common", description: "Module for ingest processors that do not require additional security permissions or have large dependencies and resources", classname: "org.elasticsearch.ingest.common.IngestCommonPlugin", extended_plugins: &["lang-painless"] },
    Module { name: "lang-mustache", description: "Mustache scripting integration for Elasticsearch", classname: "org.elasticsearch.script.mustache.MustachePlugin", extended_plugins: &[] },
    Module { name: "lang-painless", description: "An easy, safe and fast scripting language for Elasticsearch", classname: "org.elasticsearch.painless.PainlessPlugin", extended_plugins: &[] },
    Module { name: "mapper-extras", description: "Adds advanced field mappers", classname: "org.elasticsearch.index.mapper.extras.MapperExtrasPlugin", extended_plugins: &[] },
    Module { name: "mapper-version", description: "A plugin for a field type to store software versions", classname: "org.elasticsearch.xpack.versionfield.VersionFieldPlugin", extended_plugins: &["x-pack-core", "lang-painless"] },
    Module { name: "parent-join", description: "This module adds the support parent-child queries and aggregations", classname: "org.elasticsearch.join.ParentJoinPlugin", extended_plugins: &[] },
    Module { name: "reindex", description: "The Reindex module adds APIs to reindex from one index to another or update documents in place.", classname: "org.elasticsearch.reindex.ReindexPlugin", extended_plugins: &[] },
    Module { name: "rest-root", description: "Adds HEAD and GET / endpoint to Elasticsearch", classname: "org.elasticsearch.rest.root.MainRestPlugin", extended_plugins: &[] },
    Module { name: "runtime-fields-common", description: "Module for runtime fields features and extensions that have large dependencies", classname: "org.elasticsearch.runtimefields.RuntimeFieldsCommonPlugin", extended_plugins: &["lang-painless"] },
    Module { name: "unsigned-long", description: "Module for the unsigned long field type", classname: "org.elasticsearch.xpack.unsignedlong.UnsignedLongMapperPlugin", extended_plugins: &["x-pack-core", "lang-painless"] },
    Module { name: "wildcard", description: "A plugin for a keyword field type with efficient wildcard search", classname: "org.elasticsearch.xpack.wildcard.Wildcard", extended_plugins: &["x-pack-core", "lang-painless"] },
];
