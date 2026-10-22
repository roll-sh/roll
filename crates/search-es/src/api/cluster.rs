//! Cluster APIs: health (with its wait conditions), state (filtered by metric and index),
//! stats, settings, allocation explain, pending tasks and `_info`.
//!
//! A roll cluster is one node. Primaries are always assigned to it and replicas never are, as
//! on a one-node Elasticsearch: an index with replicas is `yellow`, the routing table lists the
//! replicas as `UNASSIGNED` (`INDEX_CREATED`), and allocation explain says why (`same_shard`).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::settings::Settings;
use crate::meta::{ClusterState, IndexMeta, cluster_settings};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::TimeValue;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Green,
    Yellow,
    Red,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Green => "green",
            Status::Yellow => "yellow",
            Status::Red => "red",
        }
    }

    fn parse(s: &str) -> EsResult<Status> {
        match s.to_ascii_uppercase().as_str() {
            "GREEN" => Ok(Status::Green),
            "YELLOW" => Ok(Status::Yellow),
            "RED" => Ok(Status::Red),
            other => Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.cluster.health.ClusterHealthStatus.{other}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShardHealth {
    pub status: Status,
    pub active: u32,
    pub unassigned: u32,
}

#[derive(Clone, Debug)]
pub struct IndexHealth {
    pub name: String,
    pub status: Status,
    pub shards: u32,
    pub replicas: u32,
    pub active_primary_shards: u32,
    pub active_shards: u32,
    pub unassigned_shards: u32,
    pub per_shard: Vec<ShardHealth>,
}

#[derive(Clone, Debug)]
pub struct Health {
    pub status: Status,
    pub active_primary_shards: u32,
    pub active_shards: u32,
    pub unassigned_shards: u32,
    pub unassigned_primary_shards: u32,
    pub indices: Vec<IndexHealth>,
}

impl Health {
    pub fn active_shards_percent(&self) -> f64 {
        let total = self.active_shards + self.unassigned_shards;
        if total == 0 { 100.0 } else { 100.0 * self.active_shards as f64 / total as f64 }
    }
}

fn index_health(idx: &IndexMeta, failed: bool) -> IndexHealth {
    let shards = idx.number_of_shards();
    let replicas = idx.number_of_replicas();
    if failed {
        let per_shard = (0..shards).map(|_| ShardHealth { status: Status::Red, active: 0, unassigned: 1 + replicas }).collect();
        return IndexHealth {
            name: idx.name.clone(),
            status: Status::Red,
            shards,
            replicas,
            active_primary_shards: 0,
            active_shards: 0,
            unassigned_shards: shards * (1 + replicas),
            per_shard,
        };
    }
    // Closed indices keep their shards allocated (replicated closed indices), like open ones.
    let status = if replicas > 0 { Status::Yellow } else { Status::Green };
    let per_shard = (0..shards).map(|_| ShardHealth { status, active: 1, unassigned: replicas }).collect();
    IndexHealth {
        name: idx.name.clone(),
        status,
        shards,
        replicas,
        active_primary_shards: shards,
        active_shards: shards,
        unassigned_shards: shards * replicas,
        per_shard,
    }
}

/// Health of `indices` (every index when `None`).
pub fn compute_health(cluster: &crate::Cluster, st: &ClusterState, indices: Option<&[String]>) -> Health {
    let mut h = Health {
        status: Status::Green,
        active_primary_shards: 0,
        active_shards: 0,
        unassigned_shards: 0,
        unassigned_primary_shards: 0,
        indices: Vec::new(),
    };
    let all: Vec<&IndexMeta> = match indices {
        None => st.metadata.indices.values().map(|i| i.as_ref()).collect(),
        Some(names) => names.iter().filter_map(|n| st.metadata.indices.get(n).map(|i| i.as_ref())).collect(),
    };
    for idx in all {
        let ih = index_health(idx, cluster.shards_failed(st, idx));
        h.unassigned_primary_shards += ih.shards - ih.active_primary_shards;
        h.status = h.status.max(ih.status);
        h.active_primary_shards += ih.active_primary_shards;
        h.active_shards += ih.active_shards;
        h.unassigned_shards += ih.unassigned_shards;
        h.indices.push(ih);
    }
    h
}

fn health_body(ctx: &Ctx, h: &Health, timed_out: bool, level: &str) -> Value {
    let mut m = Map::new();
    m.insert("cluster_name".into(), ctx.cluster.name.clone().into());
    m.insert("status".into(), h.status.as_str().into());
    m.insert("timed_out".into(), timed_out.into());
    m.insert("number_of_nodes".into(), 1.into());
    m.insert("number_of_data_nodes".into(), 1.into());
    let opensearch = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    if opensearch {
        m.insert("discovered_master".into(), true.into());
        m.insert("discovered_cluster_manager".into(), true.into());
    }
    m.insert("active_primary_shards".into(), h.active_primary_shards.into());
    m.insert("active_shards".into(), h.active_shards.into());
    m.insert("relocating_shards".into(), 0.into());
    m.insert("initializing_shards".into(), 0.into());
    m.insert("unassigned_shards".into(), h.unassigned_shards.into());
    if !opensearch {
        m.insert("unassigned_primary_shards".into(), h.unassigned_primary_shards.into());
    }
    m.insert("delayed_unassigned_shards".into(), 0.into());
    m.insert("number_of_pending_tasks".into(), 0.into());
    m.insert("number_of_in_flight_fetch".into(), 0.into());
    m.insert("task_max_waiting_in_queue_millis".into(), 0.into());
    if ctx.human {
        m.insert("task_max_waiting_in_queue".into(), "0s".into());
    }
    m.insert("active_shards_percent_as_number".into(), h.active_shards_percent().into());
    if ctx.human {
        m.insert("active_shards_percent".into(), format!("{:.1}%", h.active_shards_percent()).into());
    }
    if level != "cluster" {
        let mut indices = Map::new();
        for ih in &h.indices {
            let mut im = Map::new();
            im.insert("status".into(), ih.status.as_str().into());
            im.insert("number_of_shards".into(), ih.shards.into());
            im.insert("number_of_replicas".into(), ih.replicas.into());
            im.insert("active_primary_shards".into(), ih.active_primary_shards.into());
            im.insert("active_shards".into(), ih.active_shards.into());
            im.insert("relocating_shards".into(), 0.into());
            im.insert("initializing_shards".into(), 0.into());
            im.insert("unassigned_shards".into(), ih.unassigned_shards.into());
            im.insert("unassigned_primary_shards".into(), 0.into());
            if level == "shards" {
                let mut shards = Map::new();
                for (i, s) in ih.per_shard.iter().enumerate() {
                    shards.insert(
                        i.to_string(),
                        obj! {
                            "status" => s.status.as_str(),
                            "primary_active" => true,
                            "active_shards" => s.active,
                            "relocating_shards" => 0,
                            "initializing_shards" => 0,
                            "unassigned_shards" => s.unassigned,
                            "unassigned_primary_shards" => 0,
                        },
                    );
                }
                im.insert("shards".into(), Value::Obj(shards));
            }
            indices.insert(ih.name.clone(), Value::Obj(im));
        }
        m.insert("indices".into(), Value::Obj(indices));
    }
    Value::Obj(m)
}

/// `wait_for_nodes`: `N`, `>=N`, `<=N`, `>N`, `<N`, `ge(N)`, `le(N)`, `gt(N)`, `lt(N)`.
fn nodes_ok(expr: &str) -> EsResult<bool> {
    let e = expr.trim();
    if e.is_empty() {
        return Ok(true);
    }
    let parse = |s: &str| -> EsResult<i64> {
        crate::units::parse_java_long(s.trim()).ok_or_else(|| EsError::java(400, "number_format_exception", format!("For input string: \"{}\"", s.trim())))
    };
    let (op, n) = if let Some(r) = e.strip_prefix(">=") {
        (">=", parse(r)?)
    } else if let Some(r) = e.strip_prefix("<=") {
        ("<=", parse(r)?)
    } else if let Some(r) = e.strip_prefix('>') {
        (">", parse(r)?)
    } else if let Some(r) = e.strip_prefix('<') {
        ("<", parse(r)?)
    } else if let Some(r) = e.strip_prefix("ge(").and_then(|r| r.strip_suffix(')')) {
        (">=", parse(r)?)
    } else if let Some(r) = e.strip_prefix("le(").and_then(|r| r.strip_suffix(')')) {
        ("<=", parse(r)?)
    } else if let Some(r) = e.strip_prefix("gt(").and_then(|r| r.strip_suffix(')')) {
        (">", parse(r)?)
    } else if let Some(r) = e.strip_prefix("lt(").and_then(|r| r.strip_suffix(')')) {
        ("<", parse(r)?)
    } else {
        ("=", parse(e)?)
    };
    Ok(match op {
        ">=" => 1 >= n,
        "<=" => 1 <= n,
        ">" => 1 > n,
        "<" => 1 < n,
        _ => 1 == n,
    })
}

pub async fn health(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let level = ctx.query("level").unwrap_or("cluster");
    if !matches!(level, "cluster" | "indices" | "shards") {
        return Err(EsError::illegal_argument(format!("level parameter must be one of [cluster] or [indices] or [shards] but was [{level}]")));
    }
    let wait_status = ctx.query("wait_for_status").filter(|s| !s.is_empty()).map(Status::parse).transpose()?;
    let timeout = ctx.time_param("timeout", TimeValue::seconds(30))?;
    ctx.time_param("master_timeout", TimeValue::seconds(30))?;
    if let Some(p) = ctx.query("wait_for_events")
        && !matches!(p.to_ascii_uppercase().as_str(), "IMMEDIATE" | "URGENT" | "HIGH" | "NORMAL" | "LOW" | "LANGUID")
    {
        return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.common.Priority.{}", p.to_ascii_uppercase())));
    }
    for p in ["wait_for_no_relocating_shards", "wait_for_no_initializing_shards", "local"] {
        ctx.bool_param(p, false)?;
    }
    let wait_active = match ctx.query("wait_for_active_shards") {
        None => None,
        Some("all") => Some(u32::MAX),
        Some(v) => match crate::units::parse_java_int(v) {
            Some(n) if n >= 0 => Some(n as u32),
            Some(_) => return Err(EsError::illegal_argument("shard count cannot be a negative value")),
            None => return Err(EsError::illegal_argument(format!("cannot parse ActiveShardCount[{v}]"))),
        },
    };
    let nodes_wait = ctx.query("wait_for_nodes").map(str::to_string);
    if let Some(n) = &nodes_wait {
        nodes_ok(n)?;
    }
    // Health uses lenient options that include hidden indices.
    let opts = ctx.indices_options(IndicesOptions { ignore_unavailable: true, expand_closed: true, expand_hidden: true, ..IndicesOptions::STRICT_EXPAND_OPEN })?;
    let exprs = super::index_expressions(ctx);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout.as_millis().max(0) as u64);
    let mut changes = ctx.cluster.subscribe();
    loop {
        let st = ctx.state();
        let selected: Option<Vec<String>> = if exprs.is_empty() || resolve::is_all(&exprs) {
            None
        } else {
            Some(resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?)
        };
        let missing = exprs.iter().any(|e| {
            let e = resolve::resolve_date_math(e, ctx.now_ms).unwrap_or_else(|_| e.clone());
            let md = &st.metadata;
            !resolve::is_wildcard(&e)
                && e != "_all"
                && !e.starts_with('-')
                && !md.indices.contains_key(&e)
                && !st.has_alias(&e)
                && !md.data_streams.contains_key(&e)
                && !md.data_stream_aliases.contains_key(&e)
        });
        let h = compute_health(ctx.cluster, &st, selected.as_deref());
        let mut ok = !missing;
        if let Some(w) = wait_status {
            ok &= h.status <= w;
        }
        if let Some(n) = wait_active {
            let total = h.active_shards + h.unassigned_shards;
            ok &= if n == u32::MAX { h.unassigned_shards == 0 } else { h.active_shards >= n.min(total.max(n)) };
        }
        if let Some(n) = &nodes_wait {
            ok &= nodes_ok(n)?;
        }
        if ok {
            return Ok(Reply::Body(200, health_body(ctx, &h, false, level)));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, changes.changed()).await.is_err() {
            let h = if missing {
                Health {
                    status: Status::Red,
                    active_primary_shards: 0,
                    active_shards: 0,
                    unassigned_shards: 0,
                    unassigned_primary_shards: 0,
                    indices: Vec::new(),
                }
            } else {
                h
            };
            return Ok(Reply::Body(408, health_body(ctx, &h, true, level)));
        }
    }
}

/// A stable allocation id for a shard copy (Elasticsearch's are random base64 UUIDs).
pub fn allocation_id(idx: &IndexMeta, shard: u32) -> String {
    use base64::Engine;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        for b in idx.uuid.bytes().chain(shard.to_le_bytes()).chain([i as u8]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        chunk.copy_from_slice(&h.to_le_bytes());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
}

fn date_time(ms: i64) -> String {
    crate::time::DateFormatter::new("strict_date_time").expect("built-in format").format_millis(ms)
}

/// `ShardRouting.toXContent` for a started primary or an unassigned replica, in the dialect's
/// shape (OpenSearch adds `searchOnly` and has no relocation failure count).
pub(crate) fn shard_routing(ctx: &Ctx, idx: &IndexMeta, shard: u32, primary: bool) -> Value {
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let mut m = Map::new();
    if primary {
        m.insert("state".into(), "STARTED".into());
        m.insert("primary".into(), true.into());
        if os {
            m.insert("searchOnly".into(), false.into());
        }
        m.insert("node".into(), ctx.cluster.node.id.clone().into());
        m.insert("relocating_node".into(), Value::Null);
        m.insert("shard".into(), shard.into());
        m.insert("index".into(), idx.name.clone().into());
        m.insert("allocation_id".into(), obj! { "id" => allocation_id(idx, shard) });
    } else {
        m.insert("state".into(), "UNASSIGNED".into());
        m.insert("primary".into(), false.into());
        if os {
            m.insert("searchOnly".into(), false.into());
        }
        m.insert("node".into(), Value::Null);
        m.insert("relocating_node".into(), Value::Null);
        m.insert("shard".into(), shard.into());
        m.insert("index".into(), idx.name.clone().into());
        m.insert("recovery_source".into(), obj! { "type" => "PEER" });
        m.insert(
            "unassigned_info".into(),
            obj! { "reason" => "INDEX_CREATED", "at" => date_time(idx.creation_date()), "delayed" => false, "allocation_status" => "no_attempt" },
        );
    }
    if !os {
        m.insert("relocation_failure_info".into(), obj! { "failed_attempts" => 0 });
    }
    Value::Obj(m)
}

/// An index as the cluster state's metadata prints it.
fn index_metadata(idx: &IndexMeta, flat: bool) -> Value {
    let mut m = Map::new();
    m.insert("version".into(), idx.version.into());
    m.insert("transport_version".into(), crate::version::TRANSPORT_VERSION.to_string().into());
    m.insert("mapping_version".into(), idx.mapping_version.into());
    m.insert("settings_version".into(), idx.settings_version.into());
    m.insert("aliases_version".into(), idx.aliases_version.into());
    m.insert("routing_num_shards".into(), idx.routing_num_shards.into());
    m.insert("state".into(), idx.state.as_str().into());
    m.insert("settings".into(), idx.settings.to_json(flat));
    let mapping = idx.mapping_json(false);
    let mappings = if mapping.as_object().is_some_and(|o| o.is_empty()) { obj! {} } else { obj! { "_doc" => mapping } };
    m.insert("mappings".into(), mappings);
    m.insert("aliases".into(), Value::from(idx.aliases.keys().cloned().collect::<Vec<_>>()));
    let mut terms = Map::new();
    let mut in_sync = Map::new();
    for s in 0..idx.number_of_shards() {
        terms.insert(s.to_string(), Value::from(idx.primary_terms.get(s as usize).copied().unwrap_or(1)));
        in_sync.insert(s.to_string(), Value::from(vec![allocation_id(idx, s)]));
    }
    m.insert("primary_terms".into(), Value::Obj(terms));
    m.insert("in_sync_allocations".into(), Value::Obj(in_sync));
    m.insert("rollover_info".into(), obj! {});
    m.insert("mappings_updated_version".into(), Value::from(crate::version::INDEX_VERSION_ID.parse::<i64>().unwrap_or(0)));
    m.insert("system".into(), idx.system.into());
    m.insert("timestamp_range".into(), obj! { "unknown" => true });
    m.insert("event_ingested_range".into(), obj! { "unknown" => true });
    Value::Obj(m)
}

const STATE_METRICS: &[&str] = &["version", "master_node", "blocks", "nodes", "metadata", "routing_table", "routing_nodes", "customs"];

/// Index blocks as the cluster state prints them.
fn blocks_value(indices: &[&IndexMeta]) -> Value {
    let mut out = Map::new();
    for idx in indices {
        let blocks = crate::meta::blocks::index_blocks(idx);
        if blocks.is_empty() {
            continue;
        }
        let mut bm = Map::new();
        for b in blocks {
            bm.insert(
                b.id.to_string(),
                obj! { "description" => b.description, "retryable" => b.code == 429, "levels" => b.level_names() },
            );
        }
        out.insert(idx.name.clone(), Value::Obj(bm));
    }
    if out.is_empty() { obj! {} } else { obj! { "indices" => Value::Obj(out) } }
}

pub fn state(ctx: &Ctx) -> EsResult<Reply> {
    let metrics: Vec<String> = ctx.list_param("metric").unwrap_or_else(|| vec!["_all".into()]);
    Reply::ok(Value::Obj(state_value(ctx, &metrics, true)?))
}

/// The cluster state filtered to `metrics` (`_all` for everything), as `GET _cluster/state`
/// (and OpenSearch's reroute response) prints it.
pub fn state_value(ctx: &Ctx, metrics: &[String], with_cluster_name: bool) -> EsResult<Map> {
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let flat = ctx.flat_settings()?;
    let all = metrics.iter().any(|m| m == "_all");
    let want = |m: &str| all || metrics.iter().any(|x| x == m);
    let st = ctx.state();
    let node = &ctx.cluster.node;
    // The index filter applies to metadata, routing and blocks; missing indices are ignored.
    let opts = ctx.indices_options(IndicesOptions { ignore_unavailable: true, expand_closed: true, expand_hidden: true, ..IndicesOptions::STRICT_EXPAND_OPEN })?;
    let exprs = super::index_expressions(ctx);
    let names: Vec<String> = if exprs.is_empty() { st.metadata.indices.keys().cloned().collect() } else { resolve::resolve(&st, &exprs, &opts, ctx.now_ms)? };
    let indices: Vec<&IndexMeta> = names.iter().filter_map(|n| st.metadata.indices.get(n).map(|i| i.as_ref())).collect();
    let mut out = Map::new();
    if with_cluster_name {
        out.insert("cluster_name".into(), ctx.cluster.name.clone().into());
    }
    out.insert("cluster_uuid".into(), st.metadata.cluster_uuid.clone().into());
    if want("version") {
        out.insert("version".into(), st.version.into());
        out.insert("state_uuid".into(), st.state_uuid.clone().into());
    }
    if os {
        // OpenSearch has the two names as separate metrics.
        if want("master_node") {
            out.insert("master_node".into(), node.id.clone().into());
        }
        if want("cluster_manager_node") {
            out.insert("cluster_manager_node".into(), node.id.clone().into());
        }
    } else if want("master_node") {
        out.insert("master_node".into(), node.id.clone().into());
    }
    if want("blocks") {
        out.insert("blocks".into(), blocks_value(&indices));
    }
    if want("nodes") {
        let m = super::nodes::NodeMetrics::sample(ctx.cluster);
        let mut attrs = Map::new();
        for (k, v) in super::nodes::node_attributes(ctx.cluster, &m) {
            attrs.insert(k, v.into());
        }
        let mut nodes = Map::new();
        if os {
            nodes.insert(
                node.id.clone(),
                obj! {
                    "name" => node.name.clone(),
                    "ephemeral_id" => node.ephemeral_id.clone(),
                    "transport_address" => node.transport_address.clone(),
                    "attributes" => Value::Obj(attrs),
                },
            );
            out.insert("nodes".into(), Value::Obj(nodes));
        } else {
        nodes.insert(
            node.id.clone(),
            obj! {
                "name" => node.name.clone(),
                "ephemeral_id" => node.ephemeral_id.clone(),
                "transport_address" => node.transport_address.clone(),
                "external_id" => node.name.clone(),
                "attributes" => Value::Obj(attrs),
                "roles" => super::nodes::roles(ctx.cluster.dialect).to_vec(),
                "version" => crate::version::ES_VERSION,
                "min_index_version" => 8000099,
                "max_index_version" => crate::version::INDEX_VERSION_ID.parse::<i64>().unwrap_or(0),
            },
        );
        out.insert("nodes".into(), Value::Obj(nodes));
        out.insert(
            "nodes_versions".into(),
            Value::Arr(vec![obj! { "node_id" => node.id.clone(), "transport_version" => crate::version::TRANSPORT_VERSION.to_string(), "mappings_versions" => obj! {} }]),
        );
        out.insert("nodes_features".into(), Value::Arr(vec![obj! { "node_id" => node.id.clone(), "features" => crate::api::root::node_features(&ctx.cluster.config) }]));
        }
    }
    if want("metadata") {
        let md = &st.metadata;
        let mut m = Map::new();
        m.insert("cluster_uuid".into(), md.cluster_uuid.clone().into());
        m.insert("cluster_uuid_committed".into(), true.into());
        m.insert(
            "cluster_coordination".into(),
            obj! {
                "term" => 1,
                "last_committed_config" => vec![node.id.clone()],
                "last_accepted_config" => vec![node.id.clone()],
                "voting_config_exclusions" => Value::Arr(md.voting_exclusions.iter().map(|(i, n)| obj! { "node_id" => i.clone(), "node_name" => n.clone() }).collect()),
            },
        );
        if !md.persistent.is_empty() {
            m.insert("cluster_persistent_settings".into(), md.persistent.to_json(flat));
        }
        // An index filter restricts metadata to those indices (no templates or customs).
        let filtered = !exprs.is_empty();
        let mut templates = Map::new();
        for (n, t) in md.templates.iter().filter(|_| !filtered) {
            let mut tv = t.to_value(flat);
            if let Some(o) = tv.as_object_mut()
                && let Some(Value::Obj(mm)) = o.get("mappings").cloned()
                && !mm.is_empty()
            {
                o.insert("mappings".into(), obj! { "_doc" => Value::Obj(mm) });
            }
            templates.insert(n.clone(), tv);
        }
        m.insert("templates".into(), Value::Obj(templates));
        let mut im = Map::new();
        for idx in &indices {
            im.insert(idx.name.clone(), index_metadata(idx, flat));
        }
        m.insert("indices".into(), Value::Obj(im));
        if !md.index_templates.is_empty() && !filtered {
            let mut it = Map::new();
            for (n, t) in &md.index_templates {
                it.insert(n.clone(), t.to_value_flat(flat));
            }
            m.insert("index_template".into(), obj! { "index_template" => Value::Obj(it) });
        }
        if !md.pipelines.is_empty() && !filtered {
            let list: Vec<Value> = md.pipelines.iter().map(|(id, p)| obj! { "id" => id.clone(), "config" => p.clone() }).collect();
            m.insert("ingest".into(), obj! { "pipeline" => Value::Arr(list) });
        }
        m.insert("index-graveyard".into(), obj! { "tombstones" => Value::Arr(Vec::new()) });
        if !md.component_templates.is_empty() && !filtered {
            let mut ct = Map::new();
            for (n, t) in &md.component_templates {
                ct.insert(n.clone(), t.to_value_flat(flat));
            }
            m.insert("component_template".into(), obj! { "component_template" => Value::Obj(ct) });
        }
        if !md.scripts.is_empty() && !filtered {
            let mut sc = Map::new();
            for (n, s) in &md.scripts {
                sc.insert(n.clone(), s.to_value());
            }
            m.insert("stored_scripts".into(), Value::Obj(sc));
        }
        if !md.repositories.is_empty() && !filtered {
            m.insert("repositories".into(), super::snapshots::state_repositories(&st));
        }
        if let Some(d) = md.desired_nodes.as_ref().filter(|_| !filtered) {
            m.insert("desired_nodes".into(), obj! { "latest" => d.to_value(true) });
        }
        if !os {
            m.insert("reserved_state".into(), obj! {});
        }
        out.insert("metadata".into(), Value::Obj(m));
    }
    if want("routing_table") {
        let mut rt = Map::new();
        for idx in &indices {
            if idx.is_closed() && idx.number_of_replicas() == 0 && false {
                continue;
            }
            let mut shards = Map::new();
            for s in 0..idx.number_of_shards() {
                let mut copies = vec![shard_routing(ctx, idx, s, true)];
                for _ in 0..idx.number_of_replicas() {
                    copies.push(shard_routing(ctx, idx, s, false));
                }
                shards.insert(s.to_string(), Value::Arr(copies));
            }
            rt.insert(idx.name.clone(), obj! { "shards" => Value::Obj(shards) });
        }
        out.insert("routing_table".into(), obj! { "indices" => Value::Obj(rt) });
    }
    if want("routing_nodes") {
        let mut unassigned = Vec::new();
        let mut assigned = Vec::new();
        for idx in &indices {
            for s in 0..idx.number_of_shards() {
                assigned.push(shard_routing(ctx, idx, s, true));
                for _ in 0..idx.number_of_replicas() {
                    unassigned.push(shard_routing(ctx, idx, s, false));
                }
            }
        }
        let mut nodes = Map::new();
        nodes.insert(node.id.clone(), Value::Arr(assigned));
        out.insert("routing_nodes".into(), obj! { "unassigned" => Value::Arr(unassigned), "nodes" => Value::Obj(nodes) });
    }
    let _ = STATE_METRICS;
    Ok(out)
}

/// Per-type usage counters: total uses and number of indices using the type.
#[derive(Default)]
struct Usage(BTreeMap<String, (i64, i64)>);

impl Usage {
    /// Records one index's uses (name → count within the index).
    fn add_index(&mut self, uses: BTreeMap<String, i64>) {
        for (name, n) in uses {
            let e = self.0.entry(name).or_default();
            e.0 += n;
            e.1 += 1;
        }
    }

    fn to_value(&self) -> Value {
        Value::Arr(self.0.iter().map(|(n, (c, i))| obj! { "name" => n.clone(), "count" => *c, "index_count" => *i }).collect())
    }
}

#[derive(Default)]
struct RuntimeUsage {
    count: i64,
    index_count: i64,
    scriptless: i64,
    shadowed: i64,
    langs: std::collections::BTreeSet<String>,
    lines: (i64, i64),
    chars: (i64, i64),
    source: (i64, i64),
    doc: (i64, i64),
}

/// `DenseVectorFieldStats`: what the printed mappings say about `dense_vector` fields.
struct VectorStats {
    indexed: i64,
    dim_min: i64,
    dim_max: i64,
    index_types: Map,
    similarities: Map,
    element_types: Map,
}

impl Default for VectorStats {
    fn default() -> Self {
        VectorStats { indexed: 0, dim_min: -1, dim_max: -1, index_types: Map::new(), similarities: Map::new(), element_types: Map::new() }
    }
}

fn bump_count(m: &mut Map, key: &str) {
    let n = m.get(key).and_then(Value::as_i64).unwrap_or(0);
    m.insert(key.to_string(), (n + 1).into());
}

impl VectorStats {
    /// Visits the fields of a printed mapping (`MappingVisitor`: properties and multi-fields).
    fn visit(&mut self, props: &Map) {
        for f in props.values() {
            let Some(f) = f.as_object() else { continue };
            if f.get("type").and_then(Value::as_str) == Some("dense_vector") {
                self.add(f);
            }
            for nested in ["properties", "fields"] {
                if let Some(p) = f.get(nested).and_then(Value::as_object) {
                    self.visit(p);
                }
            }
        }
    }

    fn add(&mut self, f: &Map) {
        if let Some(s) = f.get("similarity") {
            bump_count(&mut self.similarities, &s.to_es_string());
        }
        bump_count(&mut self.element_types, f.get("element_type").and_then(Value::as_str).unwrap_or("float"));
        if f.get("index").and_then(Value::as_bool) == Some(true) {
            // A missing `index_options` on an indexed vector is HNSW (the quantized defaults
            // are written into the mapping).
            let ty = f.get("index_options").and_then(|o| o.get("type")).and_then(Value::as_str).unwrap_or("hnsw");
            bump_count(&mut self.index_types, ty);
            self.indexed += 1;
            if let Some(d) = f.get("dims").and_then(Value::as_i64) {
                if self.dim_min == -1 || d < self.dim_min {
                    self.dim_min = d;
                }
                // Elasticsearch compares the maximum against the minimum's unset marker.
                if self.dim_min == -1 || d > self.dim_max {
                    self.dim_max = d;
                }
            }
        } else {
            bump_count(&mut self.index_types, "not_indexed");
        }
    }

    fn write(self, o: &mut Map) {
        o.insert("indexed_vector_count".into(), self.indexed.into());
        o.insert("indexed_vector_dim_min".into(), self.dim_min.into());
        o.insert("indexed_vector_dim_max".into(), self.dim_max.into());
        for (k, m) in [("vector_index_type_count", self.index_types), ("vector_similarity_type_count", self.similarities), ("vector_element_type_count", self.element_types)] {
            if !m.is_empty() {
                o.insert(k.into(), Value::Obj(crate::json::java_hash_map_expected(m)));
            }
        }
    }
}

/// `indices.mappings` of `_cluster/stats`: field types, runtime field types and source modes.
/// System indices are left out, as in Elasticsearch.
fn mapping_stats(st: &ClusterState) -> Value {
    let mut field_types = Usage::default();
    let mut runtime: BTreeMap<String, RuntimeUsage> = BTreeMap::new();
    let mut modes: BTreeMap<String, i64> = BTreeMap::new();
    let mut total = 0usize;
    let mut sizes: std::collections::BTreeSet<String> = Default::default();
    let mut vectors = VectorStats::default();
    let mut has_vectors = false;
    for idx in st.metadata.indices.values().filter(|i| !i.system) {
        let mut uses: BTreeMap<String, i64> = BTreeMap::new();
        let paths = idx.mapping.field_paths();
        for (_, m) in &paths {
            *uses.entry(m.type_name().to_string()).or_default() += 1;
            total += 1;
        }
        field_types.add_index(uses.clone());
        let mut seen: std::collections::BTreeSet<&str> = Default::default();
        for (name, rf) in &idx.mapping.runtime {
            let r = runtime.entry(rf.ty.clone()).or_default();
            r.count += 1;
            if seen.insert(rf.ty.as_str()) {
                r.index_count += 1;
            }
            if paths.iter().any(|(p, _)| p == name) {
                r.shadowed += 1;
            }
            let src = match rf.params.get("script") {
                Some(Value::Str(s)) => Some(s.clone()),
                Some(Value::Obj(o)) => {
                    if let Some(lang) = o.get("lang").and_then(Value::as_str) {
                        r.langs.insert(lang.to_string());
                    } else {
                        r.langs.insert("painless".into());
                    }
                    o.get("source").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            };
            match src {
                None => r.scriptless += 1,
                Some(s) => {
                    if matches!(rf.params.get("script"), Some(Value::Str(_))) {
                        r.langs.insert("painless".into());
                    }
                    let lines = s.lines().count().max(1) as i64;
                    let chars = s.chars().count() as i64;
                    let source = s.matches("params._source").count() as i64;
                    let doc = s.matches("doc[").count() as i64;
                    for (acc, v) in [(&mut r.lines, lines), (&mut r.chars, chars), (&mut r.source, source), (&mut r.doc, doc)] {
                        acc.0 = acc.0.max(v);
                        acc.1 += v;
                    }
                }
            }
        }
        *modes.entry(idx.mapping.source.mode_name()).or_default() += 1;
        let printed = idx.mapping_json(false);
        if uses.contains_key("dense_vector") {
            has_vectors = true;
            if let Some(props) = printed.get("properties").and_then(Value::as_object) {
                vectors.visit(props);
            }
        }
        let mapping = printed.to_json();
        if mapping != "{}" {
            sizes.insert(mapping);
        }
    }
    let rt: Vec<Value> = runtime
        .into_iter()
        .map(|(n, r)| {
            obj! {
                "name" => n, "count" => r.count, "index_count" => r.index_count, "scriptless_count" => r.scriptless,
                "shadowed_count" => r.shadowed, "lang" => r.langs.into_iter().collect::<Vec<_>>(),
                "lines_max" => r.lines.0, "lines_total" => r.lines.1, "chars_max" => r.chars.0, "chars_total" => r.chars.1,
                "source_max" => r.source.0, "source_total" => r.source.1, "doc_max" => r.doc.0, "doc_total" => r.doc.1,
            }
        })
        .collect();
    let mut ft = field_types.to_value();
    if let Value::Arr(items) = &mut ft {
        for it in items.iter_mut() {
            if let Some(o) = it.as_object_mut() {
                // `DenseVectorFieldStats` replaces the script counters with its own.
                if has_vectors && o.get("name").and_then(Value::as_str) == Some("dense_vector") {
                    std::mem::take(&mut vectors).write(o);
                } else {
                    o.insert("script_count".into(), 0.into());
                }
            }
        }
    }
    let mut source_modes = Map::new();
    for (k, v) in modes {
        source_modes.insert(k, v.into());
    }
    obj! {
        "total_field_count" => total,
        "total_deduplicated_field_count" => total,
        "total_deduplicated_mapping_size_in_bytes" => sizes.iter().map(|s| s.len()).sum::<usize>(),
        "field_types" => ft,
        "runtime_field_types" => Value::Arr(rt),
        "source_modes" => Value::Obj(source_modes),
    }
}

/// `indices.analysis` of `_cluster/stats`, computed as `AnalysisStats.of` does: custom component
/// types, the components custom analyzers reference ("built-in" unless an index already seen
/// defines one by that name: the removal is cumulative across indices, as in Elasticsearch),
/// analyzers named by mappings (always counted as built-in), synonym sources and analyzers
/// chaining several `synonym_graph` filters. System indices are skipped.
fn analysis_stats(st: &ClusterState) -> Value {
    type Counts = BTreeMap<String, (i64, i64)>;
    fn bump(m: &mut Counts, name: &str, seen: &mut std::collections::HashSet<String>) {
        let e = m.entry(name.to_string()).or_default();
        e.0 += 1;
        if seen.insert(name.to_string()) {
            e.1 += 1;
        }
    }
    fn to_value(m: &Counts) -> Value {
        Value::Arr(m.iter().map(|(n, (c, i))| obj! { "name" => n.clone(), "count" => *c, "index_count" => *i }).collect())
    }
    /// The names of a settings group (`index.analysis.filter.<name>.…`).
    fn group_names(analysis: &crate::meta::settings::Settings, kind: &str) -> Vec<String> {
        let group = analysis.by_prefix(&format!("{kind}."));
        let mut names: Vec<String> = group.keys().filter_map(|key| key.split('.').next()).map(str::to_string).collect();
        names.dedup();
        names
    }
    let (mut char_filter_types, mut tokenizer_types, mut filter_types, mut analyzer_types) =
        (Counts::new(), Counts::new(), Counts::new(), Counts::new());
    let (mut bi_char_filters, mut bi_tokenizers, mut bi_filters, mut bi_analyzers) = (Counts::new(), Counts::new(), Counts::new(), Counts::new());
    // `synonyms` keyed by source kind: (count, index_count).
    let mut synonyms: BTreeMap<&'static str, (i64, i64)> = BTreeMap::new();
    let mut synonym_ids_used: std::collections::HashSet<String> = Default::default();
    let mut synonym_ids_in_indices: std::collections::HashSet<String> = Default::default();
    let (mut multi_sg_analyzers, mut multi_sg_indices) = (0i64, 0i64);
    // Elasticsearch walks the indices in its metadata map's (hash) order.
    let names: Vec<&String> = st.metadata.indices.keys().collect();
    let order = crate::json::java_hash_order(&names, names.len().max(1) * 4 / 3 + 1);
    for i in order {
        let idx = &st.metadata.indices[names[i]];
        if idx.system {
            continue;
        }
        let analysis = idx.settings.by_prefix("index.analysis.");
        let (mut idx_char_filters, mut idx_tokenizers, mut idx_filters) = (Default::default(), Default::default(), Default::default());
        let mut idx_analyzer_types = Default::default();
        let filter_names = group_names(&analysis, "filter");
        let sg_filters: std::collections::HashSet<&String> =
            filter_names.iter().filter(|n| analysis.get(&format!("filter.{n}.type")) == Some("synonym_graph")).collect();
        let analyzer_names = group_names(&analysis, "analyzer");
        for n in &analyzer_names {
            bi_analyzers.remove(n);
        }
        let mut idx_multi_sg = false;
        for n in &analyzer_names {
            let ty = analysis.get(&format!("analyzer.{n}.type")).unwrap_or("custom").to_string();
            bump(&mut analyzer_types, &ty, &mut idx_analyzer_types);
            for cf in analysis.get_list(&format!("analyzer.{n}.char_filter")).unwrap_or_default() {
                bump(&mut bi_char_filters, &cf, &mut idx_char_filters);
            }
            if let Some(t) = analysis.get(&format!("analyzer.{n}.tokenizer")) {
                bump(&mut bi_tokenizers, t, &mut idx_tokenizers);
            }
            let mut sg = 0;
            for f in analysis.get_list(&format!("analyzer.{n}.filter")).unwrap_or_default() {
                bump(&mut bi_filters, &f, &mut idx_filters);
                if sg_filters.contains(&f) {
                    sg += 1;
                }
            }
            if sg > 1 {
                multi_sg_analyzers += 1;
                idx_multi_sg = true;
            }
        }
        multi_sg_indices += idx_multi_sg as i64;
        for (kind, types, builtin) in [
            ("char_filter", &mut char_filter_types, &mut bi_char_filters),
            ("tokenizer", &mut tokenizer_types, &mut bi_tokenizers),
            ("filter", &mut filter_types, &mut bi_filters),
        ] {
            let defined = group_names(&analysis, kind);
            for n in &defined {
                builtin.remove(n);
            }
            let mut seen = Default::default();
            for n in &defined {
                if let Some(ty) = analysis.get(&format!("{kind}.{n}.type")) {
                    bump(types, ty, &mut seen);
                }
            }
        }
        for n in &filter_names {
            let ty = analysis.get(&format!("filter.{n}.type")).unwrap_or_default();
            if ty != "synonym" && ty != "synonym_graph" {
                continue;
            }
            let (rule, id) = if let Some(set) = analysis.get(&format!("filter.{n}.synonyms_set")) {
                ("synonyms_set", Some(set.to_string()))
            } else if let Some(path) = analysis.get(&format!("filter.{n}.synonyms_path")) {
                ("synonyms_path", Some(path.to_string()))
            } else {
                ("synonyms", None)
            };
            let key = match rule {
                "synonyms_set" => "sets",
                "synonyms_path" => "paths",
                _ => "inline",
            };
            let e = synonyms.entry(key).or_default();
            if synonym_ids_in_indices.insert(format!("{rule}{}", idx.name)) {
                e.1 += 1;
            }
            if id.as_ref().is_none_or(|id| synonym_ids_used.insert(format!("{rule}{id}"))) {
                e.0 += 1;
            }
        }
    }
    // Analyzers named by mappings, counted after every index (so never removed).
    for idx in st.metadata.indices.values().filter(|i| !i.system) {
        let mut seen = Default::default();
        for (_, m) in idx.mapping.field_paths() {
            if let crate::mapping::MapperRef::Field(f) = m {
                for p in ["analyzer", "search_analyzer", "search_quote_analyzer"] {
                    if let Some(Value::Str(a)) = f.raw(p) {
                        bump(&mut bi_analyzers, a, &mut seen);
                    }
                }
            }
        }
    }
    // `builder.map(usedSynonyms)` of a `HashMap`.
    let mut syn = Map::new();
    for (k, (c, i)) in &synonyms {
        syn.insert(k.to_string(), obj! { "count" => *c, "index_count" => *i });
    }
    obj! {
        "char_filter_types" => to_value(&char_filter_types),
        "tokenizer_types" => to_value(&tokenizer_types),
        "filter_types" => to_value(&filter_types),
        "analyzer_types" => to_value(&analyzer_types),
        "built_in_char_filters" => to_value(&bi_char_filters),
        "built_in_tokenizers" => to_value(&bi_tokenizers),
        "built_in_filters" => to_value(&bi_filters),
        "built_in_analyzers" => to_value(&bi_analyzers),
        "synonyms" => Value::Obj(crate::json::java_hash_map(syn, 16)),
        "multiple_synonym_graph_filters" => obj! { "analyzer_count" => multi_sg_analyzers, "index_count" => multi_sg_indices },
    }
}

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
    let st = ctx.state();
    let h = compute_health(ctx.cluster, &st, None);
    let m = super::nodes::NodeMetrics::sample(ctx.cluster);
    let n_idx = st.metadata.indices.len();
    let shard_counts: Vec<f64> = st.metadata.indices.values().filter(|i| !i.is_closed()).map(|i| i.number_of_shards() as f64).collect();
    let mm = |v: &[f64]| -> Value {
        if v.is_empty() {
            return obj! {};
        }
        let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        obj! { "min" => min as i64, "max" => max as i64, "avg" => avg }
    };
    let idx_stats = crate::api::indices::stats_block(&[], h.active_primary_shards as u64);
    let pick = |k: &str| idx_stats.get(k).cloned().unwrap_or_else(|| obj! {});
    let shards = if shard_counts.is_empty() {
        obj! {}
    } else {
        obj! {
            "total" => h.active_shards,
            "primaries" => h.active_primary_shards,
            "replication" => 0.0,
            "index" => obj! {
                "shards" => mm(&shard_counts),
                "primaries" => mm(&shard_counts),
                "replication" => obj! { "min" => 0.0, "max" => 0.0, "avg" => 0.0 },
            },
        }
    };
    let body = obj! {
        "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "cluster_uuid" => st.metadata.cluster_uuid.clone(),
        "timestamp" => crate::time::now_millis(),
        "status" => h.status.as_str(),
        "indices" => obj! {
            "count" => n_idx,
            "shards" => shards,
            "docs" => obj! { "count" => 0, "deleted" => 0, "total_size_in_bytes" => 0 },
            "store" => obj! { "size_in_bytes" => 0, "total_data_set_size_in_bytes" => 0, "reserved_in_bytes" => 0 },
            "fielddata" => obj! { "memory_size_in_bytes" => 0, "evictions" => 0, "global_ordinals" => obj! { "build_time_in_millis" => 0 } },
            "query_cache" => pick("query_cache"),
            "completion" => obj! { "size_in_bytes" => 0 },
            "segments" => pick("segments"),
            "mappings" => mapping_stats(&st),
            "analysis" => analysis_stats(&st),
            "versions" => if n_idx == 0 { Value::Arr(Vec::new()) } else {
                Value::Arr(vec![obj! { "version" => crate::version::ES_VERSION, "index_count" => n_idx, "primary_shard_count" => h.active_primary_shards, "total_primary_bytes" => 0 }])
            },
            "search" => obj! { "total" => 0, "queries" => obj! {}, "rescorers" => obj! {}, "sections" => obj! {}, "retrievers" => obj! {} },
            "dense_vector" => obj! { "value_count" => 0 },
            "sparse_vector" => obj! { "value_count" => 0 },
        },
        "nodes" => obj! {
            "count" => if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
                obj! {
                    "total" => 1, "cluster_manager" => 1, "coordinating_only" => 0, "data" => 1, "ingest" => 1, "master" => 1,
                    "remote_cluster_client" => 1, "search" => 0, "warm" => 0,
                }
            } else {
                obj! {
                    "total" => 1, "coordinating_only" => 0, "data" => 1, "data_cold" => 1, "data_content" => 1, "data_frozen" => 1,
                    "data_hot" => 1, "data_warm" => 1, "index" => 0, "ingest" => 1, "master" => 1, "ml" => 0, "remote_cluster_client" => 1,
                    "search" => 0, "transform" => 0, "voting_only" => 0,
                }
            },
            "versions" => vec![crate::version::ES_VERSION],
            "os" => obj! {
                "available_processors" => m.processors,
                "allocated_processors" => m.processors,
                "names" => Value::Arr(vec![obj! { "name" => "Linux", "count" => 1 }]),
                "pretty_names" => Value::Arr(vec![obj! { "pretty_name" => "Linux", "count" => 1 }]),
                "architectures" => Value::Arr(vec![obj! { "arch" => m.arch, "count" => 1 }]),
                "mem" => obj! {
                    "total_in_bytes" => m.mem_total, "adjusted_total_in_bytes" => m.mem_total, "free_in_bytes" => m.mem_free,
                    "used_in_bytes" => m.mem_used, "free_percent" => 100 - m.ram_percent(), "used_percent" => m.ram_percent(),
                },
            },
            "process" => obj! {
                "cpu" => obj! { "percent" => m.cpu_percent },
                "open_file_descriptors" => obj! { "min" => m.open_fds, "max" => m.open_fds, "avg" => m.open_fds },
            },
            "jvm" => obj! {
                "max_uptime_in_millis" => ctx.cluster.uptime_ms(),
                "versions" => Value::Arr(Vec::new()),
                "mem" => obj! { "heap_used_in_bytes" => m.rss, "heap_max_in_bytes" => m.heap_max },
                "threads" => m.threads,
            },
            "fs" => obj! { "total_in_bytes" => m.disk_total, "free_in_bytes" => m.disk_free, "available_in_bytes" => m.disk_avail },
            "plugins" => Value::Arr(Vec::new()),
            "network_types" => obj! { "transport_types" => obj! {}, "http_types" => obj! { "roll" => 1 } },
            "discovery_types" => obj! { "single-node" => 1 },
            "packaging_types" => Value::Arr(vec![obj! { "flavor" => "default", "type" => "docker", "count" => 1 }]),
            "ingest" => obj! { "number_of_pipelines" => st.metadata.pipelines.len(), "processor_stats" => obj! {} },
            "indexing_pressure" => super::nodes::indexing_pressure(m.heap_max / 10),
        },
        "snapshots" => obj! {
            "current_counts" => obj! { "snapshots" => 0, "shard_snapshots" => 0, "snapshot_deletions" => 0, "concurrent_operations" => 0, "cleanups" => 0 },
            "repositories" => super::snapshots::stats_repositories(&st, ctx.now_ms),
        },
        "repositories" => super::snapshots::repository_usage(&st),
        "ccs" => obj! { "_search" => ccs_search_stats() },
    };
    Reply::ok(body)
}

/// `ccs._search` of `_cluster/stats`: no remote clusters, no cross-cluster searches.
fn ccs_search_stats() -> Value {
    let took = || obj! { "max" => 0, "avg" => 0, "p90" => 0 };
    obj! {
        "total" => 0,
        "success" => 0,
        "skipped" => 0,
        "took" => took(),
        "took_mrt_true" => took(),
        "took_mrt_false" => took(),
        "remotes_per_search_max" => 0,
        "remotes_per_search_avg" => 0.0,
        "failure_reasons" => obj! {},
        "features" => obj! {},
        "clients" => obj! {},
        "clusters" => obj! {},
    }
}

pub fn get_settings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let flat = ctx.flat_settings()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let st = ctx.state();
    let mut out = Map::new();
    out.insert("persistent".into(), st.metadata.persistent.to_json(flat));
    out.insert("transient".into(), st.metadata.transient.to_json(flat));
    if include_defaults {
        let mut set = st.metadata.persistent.clone();
        set.merge(&st.metadata.transient);
        let mut defaults = cluster_settings::defaults_for_in(ctx.cluster.dialect, &set, &ctx.cluster.name, &ctx.cluster.node.name);
        // Node settings are the "defaults" of cluster settings: they apply until a cluster-level
        // value overrides them.
        let m = super::nodes::NodeMetrics::sample(ctx.cluster);
        let mut node = vec![("node.attr.ml.allocated_processors_double".to_string(), format!("{:.1}", m.processors as f64))];
        node.extend(super::nodes::node_attributes(ctx.cluster, &m).into_iter().map(|(k, v)| (format!("node.attr.{k}"), v)));
        node.extend(ctx.cluster.config.node_settings.iter().cloned());
        for (k, v) in node {
            if !set.contains(&k) {
                defaults.set(k, v);
            }
        }
        out.insert("defaults".into(), defaults.to_json(flat));
    }
    Reply::ok(Value::Obj(out))
}


pub async fn put_settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let flat = ctx.flat_settings()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let mut persistent = Settings::new();
    let mut transient = Settings::new();
    for (k, v) in &body {
        match k.as_str() {
            "persistent" => persistent = Settings::from_json(v)?,
            "transient" => transient = Settings::from_json(v)?,
            _ => {}
        }
    }
    if persistent.is_empty() && transient.is_empty() {
        return Err(EsError::action_request_validation(&["no settings to update".into()]));
    }
    cluster_settings::validate_update_in(ctx.cluster.dialect, "persistent", &persistent)?;
    cluster_settings::validate_update_in(ctx.cluster.dialect, "transient", &transient)?;
    let (p, t) = (persistent.clone(), transient.clone());
    ctx.cluster
        .update(move |st| {
            // Settings with validators reading the cluster state.
            for s in [&t, &p] {
                if let Some(v) = s.get("repositories.default_repository") {
                    let from = st.metadata.cluster_setting("repositories.default_repository").unwrap_or_default().to_string();
                    super::snapshots::validate_default_repository(&st.metadata, v).map_err(|e| {
                        EsError::illegal_argument(format!("illegal value can't update [repositories.default_repository] from [{from}] to [{v}]")).caused_by(e)
                    })?;
                }
            }
            let mut watermarks: Vec<&str> = cluster_settings::watermarks_in(&t).chain(cluster_settings::watermarks_in(&p)).collect();
            watermarks.sort_unstable();
            watermarks.dedup();
            if !watermarks.is_empty() {
                let (mut pers, mut trans) = (st.metadata.persistent.clone(), st.metadata.transient.clone());
                pers.merge(&p);
                trans.merge(&t);
                cluster_settings::validate_watermarks(|k| trans.get(k).or_else(|| pers.get(k)).map(str::to_string), watermarks.len())?;
            }
            st.metadata.transient.merge(&t);
            st.metadata.persistent.merge(&p);
            Ok(())
        })
        .await?;
    let applied = |s: &Settings| -> Settings {
        let mut out = Settings::new();
        for (k, v) in s.iter() {
            if !matches!(v, crate::meta::settings::SettingValue::Null) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    };
    Reply::ok(obj! {
        "acknowledged" => true,
        "persistent" => applied(&persistent).to_json(flat),
        "transient" => applied(&transient).to_json(flat),
    })
}

const ALLOCATION_EXPLAIN_DOCS: &str = "https://www.elastic.co/docs/api/doc/elasticsearch/v9/operation/operation-cluster-allocation-explain";

pub fn allocation_explain(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let body = ctx.body_map()?.unwrap_or_default();
    // With a body, the query parameters aren't read (so they are unrecognized).
    if !ctx.body.is_empty() {
        let given: std::collections::BTreeSet<String> =
            ["index", "shard", "primary", "current_node"].iter().filter(|p| ctx.has_query(p)).map(|p| p.to_string()).collect();
        if !given.is_empty() {
            let cands = ["error_trace", "filter_path", "format", "human", "include_disk_info", "include_yes_decisions", "master_timeout", "pretty"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            return Err(EsError::illegal_argument(crate::rest::params::unrecognized(&ctx.path, &given, &cands, "parameter")));
        }
    }
    let mut index = ctx.query("index").map(str::to_string);
    let mut shard: Option<i64> = ctx.query("shard").map(|s| s.parse().unwrap_or(-1));
    let mut primary: Option<bool> = ctx.opt_bool_param("primary")?;
    for (k, v) in &body {
        match k.as_str() {
            "index" => index = Some(v.to_es_string()),
            "shard" => shard = v.as_i64(),
            "primary" => primary = v.as_bool(),
            "current_node" => {}
            other => {
                return Err(EsError::new(400, "x_content_parse_exception", format!("[cluster/allocation/explain] unknown field [{other}]")));
            }
        }
    }
    let m = super::nodes::NodeMetrics::sample(ctx.cluster);
    let node = &ctx.cluster.node;
    let mut attrs = Map::new();
    for (k, v) in super::nodes::node_attributes(ctx.cluster, &m) {
        attrs.insert(k, v.into());
    }
    let (idx, shard_no, is_primary, note) = match index {
        None => {
            if shard.is_some() || primary.is_some() {
                let mut errs = vec!["index must be specified".to_string()];
                if shard.is_none() {
                    errs.push("shard must be specified".into());
                }
                if primary.is_none() {
                    errs.push("primary must be specified".into());
                }
                return Err(EsError::action_request_validation(&errs));
            }
            let pick = st.metadata.indices.values().find(|i| i.number_of_replicas() > 0);
            match pick {
                Some(i) => (i.clone(), 0u32, false, true),
                None => {
                    return Err(EsError::illegal_argument(format!(
                        "There are no unassigned shards in this cluster. Specify an assigned shard in the request body to explain its allocation. See {ALLOCATION_EXPLAIN_DOCS} for more information."
                    )));
                }
            }
        }
        Some(name) => {
            let mut errs = Vec::new();
            if shard.is_none() {
                errs.push("shard must be specified".to_string());
            }
            if primary.is_none() {
                errs.push("primary must be specified".to_string());
            }
            if !errs.is_empty() {
                return Err(EsError::action_request_validation(&errs));
            }
            let Some(i) = st.metadata.indices.get(&name) else {
                return Err(EsError::new(404, "index_not_found_exception", format!("no such index [{name}]")).with_meta("index_uuid", "_na_").with_meta("index", name));
            };
            let s = shard.unwrap_or(0);
            if s < 0 || s >= i.number_of_shards() as i64 {
                return Err(EsError::new(404, "shard_not_found_exception", "no such shard")
                    .with_meta("index_uuid", i.uuid.clone())
                    .with_meta("shard", s.to_string())
                    .with_meta("index", i.name.clone()));
            }
            if primary == Some(false) && i.number_of_replicas() == 0 {
                return Err(EsError::illegal_argument(format!(
                    "unable to find any shards to explain [ClusterAllocationExplainRequest[index={},shard={},primary?=false,include_yes_decisions?={},include_disk_info?={}] in the routing table",
                    i.name,
                    s,
                    ctx.bool_param("include_yes_decisions", false)?,
                    ctx.bool_param("include_disk_info", false)?
                )));
            }
            (i.clone(), s as u32, primary.unwrap_or(true), false)
        }
    };
    let mut out = Map::new();
    if note {
        out.insert(
            "note".into(),
            format!("No shard was specified in the explain API request, so this response explains a randomly chosen unassigned shard. There may be other unassigned shards in this cluster which cannot be assigned for different reasons. It may not be possible to assign this shard until one of the other shards is assigned correctly. To explain the allocation of other shards (whether assigned or unassigned) you must specify the target shard in the request to this API. See {ALLOCATION_EXPLAIN_DOCS} for more information.").into(),
        );
    }
    out.insert("index".into(), idx.name.clone().into());
    out.insert("shard".into(), shard_no.into());
    out.insert("primary".into(), is_primary.into());
    if is_primary {
        out.insert("current_state".into(), "started".into());
        out.insert(
            "current_node".into(),
            obj! { "id" => node.id.clone(), "name" => node.name.clone(), "transport_address" => node.transport_address.clone(), "attributes" => Value::Obj(attrs), "roles" => super::nodes::roles(ctx.cluster.dialect).to_vec(), "weight_ranking" => 1 },
        );
        out.insert("can_remain_on_current_node".into(), "yes".into());
        if idx.number_of_replicas() > 0 {
            out.insert("can_rebalance_cluster".into(), "no".into());
            out.insert(
                "can_rebalance_cluster_decisions".into(),
                Value::Arr(vec![obj! { "decider" => "rebalance_only_when_active", "decision" => "NO", "explanation" => "rebalancing is not allowed until all copies of this shard are active" }]),
            );
        } else {
            out.insert("can_rebalance_cluster".into(), "yes".into());
        }
        out.insert("can_rebalance_to_other_node".into(), "no".into());
        out.insert(
            "rebalance_explanation".into(),
            "Elasticsearch is not allowed to allocate or rebalance this shard to another node. If you expect this shard to be rebalanced to another node, find this node in the node-by-node explanation and address the reasons which prevent Elasticsearch from rebalancing this shard there.".into(),
        );
    } else {
        out.insert("current_state".into(), "unassigned".into());
        out.insert(
            "unassigned_info".into(),
            obj! { "reason" => "INDEX_CREATED", "at" => date_time(idx.creation_date()), "last_allocation_status" => "no_attempt" },
        );
        out.insert("can_allocate".into(), "no".into());
        out.insert(
            "allocate_explanation".into(),
            "Elasticsearch isn't allowed to allocate this shard to any of the nodes in the cluster. Choose a node to which you expect this shard to be allocated, find this node in the node-by-node explanation, and address the reasons which prevent Elasticsearch from allocating this shard there.".into(),
        );
        let explanation = format!(
            "a copy of this shard is already allocated to this node [[{}][{}], node[{}], [P], s[STARTED], a[id={}], failed_attempts[0]]",
            idx.name,
            shard_no,
            node.id,
            allocation_id(&idx, shard_no)
        );
        out.insert(
            "node_allocation_decisions".into(),
            Value::Arr(vec![obj! {
                "node_id" => node.id.clone(),
                "node_name" => node.name.clone(),
                "transport_address" => node.transport_address.clone(),
                "node_attributes" => Value::Obj(attrs),
                "roles" => super::nodes::roles(ctx.cluster.dialect).to_vec(),
                "node_decision" => "no",
                "weight_ranking" => 1,
                "deciders" => Value::Arr(vec![obj! { "decider" => "same_shard", "decision" => "NO", "explanation" => explanation }]),
            }]),
        );
    }
    if ctx.bool_param("include_disk_info", false)? {
        let used = (m.disk_total - m.disk_avail).max(0);
        let pct = |v: i64| if m.disk_total > 0 { 100.0 * v as f64 / m.disk_total as f64 } else { 0.0 };
        let usage = obj! {
            "path" => "/data",
            "total_bytes" => m.disk_total,
            "used_bytes" => used,
            "free_bytes" => m.disk_avail,
            "free_disk_percent" => pct(m.disk_avail),
            "used_disk_percent" => pct(used),
        };
        let mut nodes = Map::new();
        nodes.insert(node.id.clone(), obj! { "node_name" => node.name.clone(), "least_available" => usage.clone(), "most_available" => usage });
        out.insert(
            "cluster_info".into(),
            obj! {
                "nodes" => Value::Obj(nodes),
                "shard_sizes" => obj! {},
                "shard_data_set_sizes" => obj! {},
                "shard_paths" => obj! {},
                "reserved_sizes" => Value::Arr(Vec::new()),
            },
        );
    }
    Reply::ok(Value::Obj(out))
}

pub fn pending_tasks(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Reply::ok(obj! { "tasks" => Value::Arr(Vec::new()) })
}

const INFO_TARGETS: &[&str] = &["_all", "http", "ingest", "thread_pool", "script"];

pub fn info(ctx: &Ctx) -> EsResult<Reply> {
    let targets = ctx.list_param("target").unwrap_or_default();
    let invalid: std::collections::BTreeSet<String> = targets.iter().filter(|t| !INFO_TARGETS.contains(&t.as_str())).cloned().collect();
    if !invalid.is_empty() {
        let cands = INFO_TARGETS.iter().map(|s| s.to_string()).collect();
        return Err(EsError::illegal_argument(crate::rest::params::unrecognized(&ctx.path, &invalid, &cands, "target")));
    }
    let all = targets.iter().any(|t| t == "_all");
    if all && targets.len() > 1 {
        return Err(EsError::illegal_argument(format!(
            "request [{}] contains _all and individual target [{}]",
            ctx.path,
            targets.join(",")
        )));
    }
    let want = |t: &str| all || targets.iter().any(|x| x == t);
    let mut out = Map::new();
    out.insert("cluster_name".into(), ctx.cluster.name.clone().into());
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
        out.insert("http".into(), obj! { "current_open" => 1, "total_opened" => 1, "clients" => Value::Arr(vec![client]) });
    }
    if want("ingest") {
        out.insert("ingest".into(), obj! { "total" => obj! { "count" => 0, "time_in_millis" => 0, "current" => 0, "failed" => 0 }, "pipelines" => obj! {} });
    }
    if want("thread_pool") {
        let mut tp = Map::new();
        for p in super::cat::thread_pools(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)) {
            tp.insert(p.name.to_string(), obj! { "threads" => 0, "queue" => 0, "active" => 0, "rejected" => 0, "largest" => 0, "completed" => 0 });
        }
        out.insert("thread_pool".into(), Value::Obj(tp));
    }
    if want("script") {
        out.insert("script".into(), obj! { "compilations" => 0, "cache_evictions" => 0, "compilation_limit_triggered" => 0 });
    }
    Reply::ok(Value::Obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_nodes_expressions() {
        assert!(nodes_ok("1").unwrap());
        assert!(nodes_ok(">=1").unwrap());
        assert!(!nodes_ok(">1").unwrap());
        assert!(nodes_ok("le(2)").unwrap());
        assert!(!nodes_ok("2").unwrap());
        assert!(nodes_ok("x").is_err());
    }

    #[test]
    fn statuses() {
        assert!(Status::Green < Status::Yellow);
        assert_eq!(Status::parse("yellow").unwrap(), Status::Yellow);
        assert_eq!(Status::parse("bogus").unwrap_err().reason(), "No enum constant org.elasticsearch.cluster.health.ClusterHealthStatus.BOGUS");
    }
}
