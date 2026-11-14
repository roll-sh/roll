//! Remote clusters (`cluster.remote.<alias>.*`) and cross-cluster search.
//!
//! roll nodes don't speak Elasticsearch's transport protocol, so the only remote cluster a node
//! can reach is itself: a remote whose seeds (sniff mode) or proxy address name the node's own
//! transport address is a loopback, as in Elasticsearch's own test setups, and searches its
//! `alias:index` expressions against the local indices, with hits and errors naming the indices
//! `alias:index`. Any other remote is unavailable: skipped when `skip_unavailable` (the
//! default) and an error otherwise.
//!
//! Cross-cluster searches are counted for `_cluster/stats`' `ccs._search` (totals, `took`
//! maximum, average and 90th percentile per cluster and overall, remotes per search).

use std::collections::BTreeMap;

use parking_lot::Mutex;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::obj;

/// A configured remote cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub alias: String,
    pub proxy: bool,
    pub seeds: Vec<String>,
    pub proxy_address: Option<String>,
    pub skip_unavailable: bool,
}

/// The remote clusters the cluster settings configure (transient over persistent), by alias.
pub fn remotes(st: &ClusterState) -> Vec<Remote> {
    let mut merged = st.metadata.persistent.by_prefix("cluster.remote.");
    for (k, v) in st.metadata.transient.by_prefix("cluster.remote.").iter() {
        merged.insert(k.clone(), v.clone());
    }
    let mut aliases: Vec<String> = merged.keys().filter_map(|k| k.split_once('.').map(|(a, _)| a.to_string())).collect();
    aliases.dedup();
    let mut out = Vec::new();
    for alias in aliases {
        let get = |k: &str| merged.get(&format!("{alias}.{k}")).map(str::to_string);
        let proxy = get("mode").is_some_and(|m| m.eq_ignore_ascii_case("proxy"));
        let seeds = merged.get_list(&format!("{alias}.seeds")).unwrap_or_default();
        let proxy_address = get("proxy_address").filter(|a| !a.is_empty());
        // A remote without seeds (or proxy address) isn't configured (`RemoteClusterAware`).
        if if proxy { proxy_address.is_none() } else { seeds.is_empty() } {
            continue;
        }
        let skip_unavailable = get("skip_unavailable").is_none_or(|v| v != "false");
        out.push(Remote { alias, proxy, seeds, proxy_address, skip_unavailable });
    }
    out
}

impl Remote {
    /// Whether this remote is the node itself.
    pub fn is_loopback(&self, cluster: &Cluster) -> bool {
        let me = cluster.node.transport_address.as_str();
        if self.proxy { self.proxy_address.as_deref() == Some(me) } else { self.seeds.iter().any(|s| s == me) }
    }

    fn mode(&self) -> &'static str {
        if self.proxy { "proxy" } else { "sniff" }
    }
}

/// `GET _remote/info`.
pub fn remote_info(cluster: &Cluster, st: &ClusterState) -> Value {
    let mut m = Map::new();
    for r in remotes(st) {
        let connected = r.is_loopback(cluster);
        let mut e = Map::new();
        e.insert("connected".into(), Value::Bool(connected));
        e.insert("mode".into(), r.mode().into());
        if r.proxy {
            e.insert("proxy_address".into(), r.proxy_address.clone().unwrap_or_default().into());
            e.insert("server_name".into(), "".into());
            e.insert("num_proxy_sockets_connected".into(), Value::from(if connected { 18i64 } else { 0 }));
            e.insert("max_proxy_socket_connections".into(), Value::from(18i64));
        } else {
            e.insert("seeds".into(), Value::Arr(r.seeds.iter().cloned().map(Value::Str).collect()));
            e.insert("num_nodes_connected".into(), Value::from(i64::from(connected)));
            e.insert("max_connections_per_cluster".into(), Value::from(3i64));
        }
        e.insert("initial_connect_timeout".into(), "30s".into());
        e.insert("skip_unavailable".into(), Value::Bool(r.skip_unavailable));
        m.insert(r.alias.clone(), Value::Obj(e));
    }
    Value::Obj(m)
}

/// An index expression split into its cluster part: `alias:expr` (the alias may be a pattern)
/// or a local expression.
pub fn split_expression(e: &str) -> Option<(&str, &str)> {
    if e.starts_with('<') {
        return None;
    }
    // `index::selector` is a local expression.
    let base = match e.rfind("::") {
        Some(i) => &e[..i],
        None => e,
    };
    let i = base.find(':')?;
    Some((&e[..i], &e[i + 1..]))
}

/// How a cross-cluster search's expressions split over clusters.
pub struct Plan {
    /// Local expressions (`None`: the search names only remote clusters).
    pub local: Option<Vec<String>>,
    /// Loopback remotes and the expressions to search there.
    pub remote: Vec<(String, Vec<String>)>,
    /// Remotes skipped because they can't be reached.
    pub skipped: Vec<String>,
    /// The search used a cluster wildcard.
    pub wildcards: bool,
}

/// Splits `expressions` over the local cluster and the remotes they name; `None` when they
/// name no remote cluster. Unknown aliases and unreachable remotes that may not be skipped
/// fail like Elasticsearch's.
pub fn plan(cluster: &Cluster, st: &ClusterState, expressions: &[String]) -> EsResult<Option<Plan>> {
    if !expressions.iter().any(|e| split_expression(e.trim_start_matches('-')).is_some()) {
        return Ok(None);
    }
    let configured = remotes(st);
    let mut local: Vec<String> = Vec::new();
    let mut remote: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut wildcards = false;
    for e in expressions {
        let Some((pattern, index)) = split_expression(e) else {
            local.push(e.clone());
            continue;
        };
        if pattern.contains('*') {
            wildcards = true;
        }
        let matched: Vec<&Remote> = configured.iter().filter(|r| crate::xcontent::filter::glob_match(pattern, &r.alias)).collect();
        if matched.is_empty() && !pattern.contains('*') {
            return Err(EsError::new(404, "no_such_remote_cluster_exception", format!("no such remote cluster: [{pattern}]")));
        }
        for r in matched {
            if r.is_loopback(cluster) {
                remote.entry(r.alias.clone()).or_default().push(index.to_string());
            } else if r.skip_unavailable {
                if !skipped.contains(&r.alias) {
                    skipped.push(r.alias.clone());
                }
            } else {
                return Err(EsError::new(500, "connect_transport_exception", format!("[{}] unable to connect to remote cluster", r.alias)));
            }
        }
    }
    Ok(Some(Plan { local: (!local.is_empty()).then_some(local), remote: remote.into_iter().collect(), skipped, wildcards }))
}

/// A `took` series (`ccs._search.took` and per cluster): max, average, 90th percentile.
#[derive(Default, Debug)]
struct Took {
    values: Vec<u64>,
}

impl Took {
    fn add(&mut self, ms: u64) {
        // Elasticsearch keeps an HDR histogram; a bounded sample keeps memory flat.
        if self.values.len() < 10_000 {
            self.values.push(ms);
        }
    }

    fn to_value(&self) -> Value {
        if self.values.is_empty() {
            return obj! { "max" => 0, "avg" => 0, "p90" => 0 };
        }
        let mut v = self.values.clone();
        v.sort_unstable();
        let max = *v.last().unwrap_or(&0);
        let avg = v.iter().sum::<u64>() / v.len() as u64;
        let p90 = v[((v.len() as f64 * 0.9).ceil() as usize).clamp(1, v.len()) - 1];
        obj! { "max" => max as i64, "avg" => avg as i64, "p90" => p90 as i64 }
    }
}

#[derive(Default, Debug)]
struct ClusterUsage {
    total: u64,
    skipped: u64,
    took: Took,
}

#[derive(Default, Debug)]
struct Usage {
    total: u64,
    success: u64,
    skipped: u64,
    took: Took,
    took_mrt_true: Took,
    took_mrt_false: Took,
    remotes_max: u64,
    remotes_sum: u64,
    features: BTreeMap<String, u64>,
    clusters: BTreeMap<String, ClusterUsage>,
}

/// `CCSUsageTelemetry`: what cross-cluster searches did.
#[derive(Default, Debug)]
pub struct CcsStats {
    usage: Mutex<Usage>,
}

impl CcsStats {
    /// Records one cross-cluster search over `clusters` (`(local)` for the local one) that took
    /// `took_ms`; `minimize_roundtrips` is `ccs_minimize_roundtrips` (on by default).
    pub fn record(&self, plan: &Plan, took_ms: u64, ok: bool, minimize_roundtrips: bool) {
        let mut u = self.usage.lock();
        u.total += 1;
        if ok {
            u.success += 1;
        }
        if !plan.skipped.is_empty() {
            u.skipped += 1;
        }
        u.took.add(took_ms);
        if minimize_roundtrips {
            u.took_mrt_true.add(took_ms);
            *u.features.entry("mrt_on".into()).or_insert(0) += 1;
        } else {
            u.took_mrt_false.add(took_ms);
        }
        if plan.wildcards {
            *u.features.entry("wildcards".into()).or_insert(0) += 1;
        }
        let remotes = (plan.remote.len() + plan.skipped.len()) as u64;
        u.remotes_max = u.remotes_max.max(remotes);
        u.remotes_sum += remotes;
        let mut names: Vec<&str> = plan.remote.iter().map(|(a, _)| a.as_str()).collect();
        if plan.local.is_some() {
            names.push("(local)");
        }
        for n in names {
            let c = u.clusters.entry(n.to_string()).or_default();
            c.total += 1;
            c.took.add(took_ms);
        }
        for s in &plan.skipped {
            u.clusters.entry(s.clone()).or_default().skipped += 1;
        }
    }

    /// `ccs._search`.
    pub fn to_value(&self) -> Value {
        let u = self.usage.lock();
        let mut clusters = Map::new();
        for (name, c) in &u.clusters {
            clusters.insert(name.clone(), obj! { "total" => c.total as i64, "skipped" => c.skipped as i64, "took" => c.took.to_value() });
        }
        let mut features = Map::new();
        for (k, v) in &u.features {
            features.insert(k.clone(), Value::from(*v as i64));
        }
        obj! {
            "total" => u.total as i64,
            "success" => u.success as i64,
            "skipped" => u.skipped as i64,
            "took" => u.took.to_value(),
            "took_mrt_true" => u.took_mrt_true.to_value(),
            "took_mrt_false" => u.took_mrt_false.to_value(),
            "remotes_per_search_max" => u.remotes_max as i64,
            "remotes_per_search_avg" => if u.total == 0 { 0.0 } else { u.remotes_sum as f64 / u.total as f64 },
            "failure_reasons" => obj! {},
            "features" => Value::Obj(features),
            "clients" => obj! {},
            "clusters" => Value::Obj(clusters),
        }
    }
}

/// `_cluster/stats?include_remotes`: what each remote reports about itself (a loopback reports
/// this cluster; an unreachable one only its configuration).
pub fn remote_cluster_stats(cluster: &Cluster, st: &ClusterState, local: &Value) -> Value {
    let mut out = Map::new();
    for r in remotes(st) {
        let mut m = Map::new();
        if r.is_loopback(cluster) {
            m.insert("cluster_uuid".into(), st.metadata.cluster_uuid.clone().into());
        }
        m.insert("mode".into(), r.mode().into());
        m.insert("skip_unavailable".into(), Value::Bool(r.skip_unavailable));
        m.insert("transport.compress".into(), "indexing_data".into());
        if r.is_loopback(cluster) {
            let get = |path: &[&str]| path.iter().try_fold(local, |v, k| v.get(k)).cloned().unwrap_or(Value::from(0i64));
            m.insert("status".into(), get(&["status"]));
            m.insert("version".into(), Value::Arr(vec![crate::version::ES_VERSION.into()]));
            m.insert("nodes_count".into(), get(&["nodes", "count", "total"]));
            m.insert("shards_count".into(), get(&["indices", "shards", "total"]));
            m.insert("indices_count".into(), get(&["indices", "count"]));
            m.insert("indices_total_size_in_bytes".into(), get(&["indices", "store", "size_in_bytes"]));
            m.insert("max_heap_in_bytes".into(), get(&["nodes", "jvm", "mem", "heap_max_in_bytes"]));
            m.insert("mem_total_in_bytes".into(), get(&["nodes", "os", "mem", "total_in_bytes"]));
        }
        out.insert(r.alias.clone(), Value::Obj(m));
    }
    Value::Obj(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions() {
        assert_eq!(split_expression("c1:idx"), Some(("c1", "idx")));
        assert_eq!(split_expression("*:*"), Some(("*", "*")));
        assert_eq!(split_expression("idx::failures"), None);
        assert_eq!(split_expression("c1:idx::data"), Some(("c1", "idx::data")));
        assert_eq!(split_expression("<logs-{now/d}>"), None);
        assert_eq!(split_expression("idx"), None);
    }

    #[test]
    fn took_percentiles() {
        let mut t = Took::default();
        for ms in 1..=10 {
            t.add(ms);
        }
        assert_eq!(t.to_value(), obj! { "max" => 10, "avg" => 5, "p90" => 9 });
        assert_eq!(Took::default().to_value(), obj! { "max" => 0, "avg" => 0, "p90" => 0 });
    }
}
