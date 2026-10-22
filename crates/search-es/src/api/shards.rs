//! Shard-level views of a one-node cluster: `_search_shards`, `_shard_stores`, `_segments`, and
//! `_nodes/reload_secure_settings`.
//!
//! Every primary is started on the node and replicas stay unassigned, so these answers follow
//! from the metadata: which shards a search would reach (routing values and alias routing hash
//! to shards as for documents, see [`crate::meta::routing`]), the stores holding each copy, and
//! segments (none until the engine reports them).

use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::{IndexMeta, routing};
use crate::obj;
use crate::rest::{Ctx, Reply};
use std::collections::BTreeSet;

/// `DiscoveryNode.toXContent`: the node as shard APIs print it.
fn node_value(ctx: &Ctx) -> Value {
    let node = &ctx.cluster.node;
    let m = super::nodes::NodeMetrics::sample(ctx.cluster);
    let mut attrs = Map::new();
    for (k, v) in super::nodes::node_attributes(ctx.cluster, &m) {
        attrs.insert(k, v.into());
    }
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        return obj! {
            "name" => node.name.clone(),
            "ephemeral_id" => node.ephemeral_id.clone(),
            "transport_address" => node.transport_address.clone(),
            "attributes" => Value::Obj(attrs),
        };
    }
    obj! {
        "name" => node.name.clone(),
        "ephemeral_id" => node.ephemeral_id.clone(),
        "transport_address" => node.transport_address.clone(),
        "external_id" => node.name.clone(),
        "attributes" => Value::Obj(attrs),
        "roles" => super::nodes::roles(ctx.cluster.dialect).to_vec(),
        "version" => crate::version::ES_VERSION,
        "min_index_version" => crate::version::MIN_INDEX_VERSION_ID.parse::<i64>().unwrap_or(0),
        "max_index_version" => crate::version::INDEX_VERSION_ID.parse::<i64>().unwrap_or(0),
    }
}

fn shard_routing(ctx: &Ctx, idx: &IndexMeta, shard: u32) -> Value {
    super::cluster::shard_routing(ctx, idx, shard, true)
}

/// `GET [<index>]/_search_shards`.
pub fn search_shards(ctx: &Ctx) -> EsResult<Reply> {
    ctx.bool_param("local", false)?;
    ctx.time_param("master_timeout", crate::units::TimeValue::seconds(30))?;
    let st = ctx.state();
    let exprs = super::index_expressions(ctx);
    let opts = ctx.indices_options(IndicesOptions::LENIENT_EXPAND_OPEN)?;
    let names = resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?;
    // Aliases named by the request apply their filters and search routing.
    let named_aliases: BTreeSet<String> = exprs.iter().filter(|e| st.has_alias(e)).cloned().collect();
    let request_routing: Option<Vec<String>> = ctx.list_param("routing");
    let opensearch = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let preference = match ctx.param("preference") {
        Some(p) => routing::Preference::parse(p, opensearch)?,
        None => routing::Preference::default(),
    };
    // OpenSearch takes a search source whose `slice` picks a subset of each index's shards.
    let slice = if opensearch { slice_of(ctx)? } else { None };
    let mut indices_out = Map::new();
    let mut groups: Vec<(String, u32)> = Vec::new();
    for n in &names {
        let Some(idx) = st.metadata.indices.get(n) else { continue };
        let mut m = Map::new();
        let via: Vec<(&String, &crate::meta::AliasMeta)> = idx.aliases.iter().filter(|(a, _)| named_aliases.contains(*a)).collect();
        // An index also reached directly (or by a wildcard) isn't filtered by its aliases.
        let direct = exprs.iter().any(|e| e == n || resolve::is_wildcard(e) || e == "_all");
        let mut alias_routing: Option<BTreeSet<String>> = None;
        if !via.is_empty() {
            let mut names: Vec<String> = via.iter().map(|(a, _)| (*a).clone()).collect();
            if direct {
                names = idx.aliases.keys().filter(|a| exprs.iter().any(|e| resolve::is_wildcard(e) && crate::xcontent::filter::glob_match(e, a)) || named_aliases.contains(*a)).cloned().collect();
            }
            names.sort();
            m.insert("aliases".into(), Value::from(names));
            // Elasticsearch combines the filters in an order that varies between runs (it
            // follows an unordered set); roll uses alias name order.
            // Filters print as parsed queries (`AliasFilter`), defaults included as each query
            // prints them.
            let filters: Vec<crate::query::Query> =
                via.iter().filter_map(|(_, a)| a.filter.as_ref()).map(crate::query::parse_stored).collect::<EsResult<_>>()?;
            let unfiltered = via.iter().any(|(_, a)| a.filter.is_none());
            if !direct && !unfiltered && !filters.is_empty() {
                let dialect = ctx.cluster.dialect;
                let f = if filters.len() == 1 {
                    filters[0].to_value_in(dialect)
                } else {
                    crate::query::Query::from(crate::query::compound::Bool { should: filters, ..Default::default() }).to_value_in(dialect)
                };
                m.insert("filter".into(), f);
            }
            if !direct && via.iter().all(|(_, a)| a.search_routing.is_some()) {
                alias_routing = Some(via.iter().filter_map(|(_, a)| a.search_routing.clone()).flat_map(|r| r.split(',').map(str::to_string).collect::<Vec<_>>()).collect());
            }
        } else if exprs.iter().any(|e| resolve::is_wildcard(e)) {
            let mut names: Vec<String> = idx.aliases.keys().filter(|a| exprs.iter().any(|e| crate::xcontent::filter::glob_match(e, a))).cloned().collect();
            names.sort();
            if !names.is_empty() {
                m.insert("aliases".into(), Value::from(names));
            }
        }
        indices_out.insert(n.clone(), Value::Obj(m));
        // The routing values in effect: the request's, narrowed to the alias's when both.
        let routing: Option<Vec<String>> = match (&request_routing, &alias_routing) {
            // No value in common: the routing doesn't narrow the search.
            (Some(r), Some(a)) => Some(r.iter().filter(|x| a.contains(*x)).cloned().collect::<Vec<_>>()).filter(|v| !v.is_empty()),
            (Some(r), None) => Some(r.clone()),
            (None, Some(a)) => Some(a.iter().cloned().collect()),
            (None, None) => None,
        };
        let mut shards: BTreeSet<u32> = match routing {
            Some(rs) => rs.iter().flat_map(|r| routing::search_shards_for(idx, r)).collect(),
            None => (0..idx.number_of_shards()).collect(),
        };
        shards.retain(|s| preference.keeps(*s));
        if let (Some(nodes), Some(first)) = (&preference.only_nodes, shards.first())
            && !super::nodes::selects_node(ctx, &nodes.split(',').map(str::to_string).collect::<Vec<_>>())
        {
            return Err(crate::error::EsError::illegal_argument(format!("no data nodes with criteria [{nodes}] found for shard: [{n}][{first}]")));
        }
        let count = shards.len();
        for (ordinal, s) in shards.into_iter().enumerate() {
            // `SliceBuilder.shardMatches` over the ordinals of the index's targeted shards.
            let keep = slice.is_none_or(|(id, max)| if max >= count as i32 { id % count as i32 == ordinal as i32 } else { ordinal as i32 % max == id });
            if keep {
                groups.push((n.clone(), s));
            }
        }
    }
    groups.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let shards: Vec<Value> = groups
        .iter()
        .filter_map(|(n, s)| st.metadata.indices.get(n).map(|idx| Value::Arr(vec![shard_routing(ctx, idx, *s)])))
        .collect();
    let mut nodes = Map::new();
    if !shards.is_empty() {
        nodes.insert(ctx.cluster.node.id.clone(), node_value(ctx));
    }
    Reply::ok(obj! {
        "nodes" => Value::Obj(nodes),
        "indices" => Value::Obj(crate::json::java_hash_map(indices_out, 16)),
        "shards" => Value::Arr(shards),
    })
}

/// The `slice` of an OpenSearch search_shards body as `(id, max)`, with `SliceBuilder`'s checks.
fn slice_of(ctx: &Ctx) -> EsResult<Option<(i32, i32)>> {
    let Some(body) = ctx.body_map()? else { return Ok(None) };
    let Some(slice) = body.get("slice") else { return Ok(None) };
    use crate::json::locate::{Seg, Target, locate};
    let at = |path: &[Seg]| ctx.raw_json().and_then(|r| locate(r, path, Target::Value)).unwrap_or((1, 1));
    let Some(m) = slice.as_object() else {
        let (l, c) = at(&[Seg::Key("slice")]);
        return Err(crate::error::EsError::x_content_parse(l, c, "[slice] slice doesn't support values of type: VALUE_STRING"));
    };
    let int = |k: &str| -> EsResult<Option<i32>> {
        match m.get(k) {
            None => Ok(None),
            Some(v) => v
                .as_i64()
                .or_else(|| v.as_str().and_then(crate::units::parse_java_long))
                .and_then(|n| i32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| {
                    let (l, c) = at(&[Seg::Key("slice"), Seg::Key(k)]);
                    crate::error::EsError::x_content_parse(l, c, format!("[slice] failed to parse field [{k}]"))
                }),
        }
    };
    let id = int("id")?.unwrap_or(-1);
    let max = int("max")?.unwrap_or(-1);
    if id < 0 {
        return Err(crate::error::EsError::illegal_argument("id must be greater than or equal to 0"));
    }
    if max <= 1 {
        return Err(crate::error::EsError::illegal_argument("max must be greater than 1"));
    }
    if id >= max {
        return Err(crate::error::EsError::illegal_argument("max must be greater than id"));
    }
    Ok(Some((id, max)))
}

/// `GET [<index>]/_shard_stores`: the store of each primary; `status` picks shards by health
/// (`yellow,red` by default: shards with unassigned replicas).
pub fn shard_stores(ctx: &Ctx) -> EsResult<Reply> {
    let statuses: Vec<String> = ctx.list_param("status").unwrap_or_else(|| vec!["yellow".into(), "red".into()]);
    for s in &statuses {
        if !matches!(s.as_str(), "green" | "yellow" | "red" | "all") {
            return Err(crate::error::EsError::illegal_argument(format!("unknown cluster health status [{s}]")));
        }
    }
    let all = statuses.iter().any(|s| s == "all");
    let names = super::resolve_indices(ctx, IndicesOptions::STRICT_EXPAND)?;
    let st = ctx.state();
    let mut out = Map::new();
    for n in &names {
        let Some(idx) = st.metadata.indices.get(n) else { continue };
        let health = if idx.number_of_replicas() > 0 { "yellow" } else { "green" };
        if !all && !statuses.iter().any(|s| s == health) {
            continue;
        }
        let mut shards = Map::new();
        for s in 0..idx.number_of_shards() {
            let mut store = Map::new();
            store.insert(ctx.cluster.node.id.clone(), node_value(ctx));
            store.insert("allocation_id".into(), Value::from(super::cluster::allocation_id(idx, s)));
            store.insert("allocation".into(), "primary".into());
            shards.insert(s.to_string(), obj! { "stores" => Value::Arr(vec![Value::Obj(store)]) });
        }
        out.insert(n.clone(), obj! { "shards" => Value::Obj(shards) });
    }
    Reply::ok(obj! { "indices" => Value::Obj(crate::json::java_hash_map(out, 16)) })
}

/// `GET [<index>]/_segments`: shards with no segments until the engine reports them.
pub fn segments(ctx: &Ctx) -> EsResult<Reply> {
    let names = super::resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN_FORBID_CLOSED)?;
    let st = ctx.state();
    let (mut total, mut ok) = (0u64, 0u64);
    let mut out = Map::new();
    for n in &names {
        let Some(idx) = st.metadata.indices.get(n) else { continue };
        if idx.is_closed() {
            continue;
        }
        let p = idx.number_of_shards() as u64;
        total += p * (1 + idx.number_of_replicas() as u64);
        ok += p;
        let mut shards = Map::new();
        for s in 0..idx.number_of_shards() {
            shards.insert(
                s.to_string(),
                Value::Arr(vec![obj! {
                    "routing" => obj! { "state" => "STARTED", "primary" => true, "node" => ctx.cluster.node.id.clone() },
                    "num_committed_segments" => 0,
                    "num_search_segments" => 0,
                    "segments" => obj! {},
                }]),
            );
        }
        out.insert(n.clone(), obj! { "shards" => Value::Obj(shards) });
    }
    Reply::ok(obj! {
        "_shards" => obj! { "total" => total, "successful" => ok, "failed" => 0 },
        "indices" => Value::Obj(crate::json::java_hash_map(out, 16)),
    })
}

/// `POST _nodes[/<node_id>]/reload_secure_settings`: roll nodes have no keystore; a password
/// can't match one.
pub fn reload_secure_settings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    let body = ctx.body_map()?.unwrap_or_default();
    let password = body.get("secure_settings_password").map(Value::to_es_string).filter(|p| !p.is_empty());
    let selectors: Vec<String> = ctx.path_param("node_id").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_else(|| vec!["_all".into()]);
    let selected = super::nodes::selects_node(ctx, &selectors);
    let mut nodes = Map::new();
    if selected {
        let node = &ctx.cluster.node;
        let mut m = Map::new();
        m.insert("name".into(), Value::from(node.name.clone()));
        if password.is_some() {
            m.insert(
                "reload_exception".into(),
                obj! { "type" => "security_exception", "reason" => "Provided keystore password was incorrect" },
            );
        } else {
            m.insert("secure_setting_names".into(), Value::Arr(Vec::new()));
        }
        nodes.insert(node.id.clone(), Value::Obj(m));
    }
    let n = if selected { 1 } else { 0 };
    Reply::ok(obj! {
        "_nodes" => obj! { "total" => n, "successful" => n, "failed" => 0 },
        "cluster_name" => ctx.cluster.name.clone(),
        "nodes" => Value::Obj(nodes),
    })
}
