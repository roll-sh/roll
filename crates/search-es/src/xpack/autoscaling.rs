//! Autoscaling (`_autoscaling`): policies and the capacity they require.
//!
//! A policy names a set of node roles and the deciders that compute the capacity its nodes
//! need. Policies are cluster metadata on Elasticsearch (`AutoscalingMetadata`), here the
//! `autoscaling.policy` family. A policy's *current* nodes are the nodes whose role set is
//! exactly the policy's; the capacity API reports their storage, memory and processors, and
//! each decider's required capacity (the policy's is the largest of them).
//!
//! Deciders: `fixed` (the capacity its settings give, times `nodes`) and the defaults a role set
//! enables (`reactive_storage` and `proactive_storage` for data roles, the `frozen_*` deciders
//! for `data_frozen`, `ml` for `ml`). A single roll node never waits on unassigned shards or
//! queued ML work, so the default deciders require what the nodes have ("storage ok").

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::ByteSize;
use crate::xpack::store;

pub const POLICIES: &str = "autoscaling.policy";

/// `DiscoveryNodeRole` names a policy may list.
const ROLES: &[&str] = &[
    "data", "data_cold", "data_content", "data_frozen", "data_hot", "data_warm", "index", "ingest", "master", "ml", "remote_cluster_client", "search", "transform", "voting_only",
];

const DECIDERS: &[&str] = &["fixed", "frozen_existence", "frozen_shards", "frozen_storage", "ml", "proactive_storage", "reactive_storage"];

/// The settings each decider accepts.
fn decider_settings(name: &str) -> &'static [&'static str] {
    match name {
        "fixed" => &["memory", "nodes", "processors", "storage"],
        "proactive_storage" => &["forecast_window"],
        "frozen_shards" => &["memory_per_shard"],
        "frozen_storage" => &["percentage"],
        "ml" => &["num_anomaly_jobs_in_queue", "num_analytics_jobs_in_queue", "down_scale_delay"],
        _ => &[],
    }
}

/// The deciders a role set enables by default.
fn default_deciders(roles: &[String]) -> Vec<&'static str> {
    let has = |r: &str| roles.iter().any(|x| x == r);
    let mut out = Vec::new();
    if has("data_frozen") {
        out.extend(["frozen_existence", "frozen_shards", "frozen_storage"]);
    }
    if has("ml") {
        out.push("ml");
    }
    if ["data", "data_content", "data_hot", "data_warm", "data_cold"].iter().any(|r| has(r)) {
        out.extend(["proactive_storage", "reactive_storage"]);
    }
    out
}

fn not_found(name: &str) -> EsError {
    EsError::resource_not_found(format!("autoscaling policy with name [{name}] does not exist"))
}

fn strs(v: &Value) -> Vec<String> {
    match v {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
        Value::Null => Vec::new(),
        other => vec![other.to_es_string()],
    }
}

/// Parses the deciders of a put request: known names and settings, values kept as strings (they
/// are `Settings`), sorted by name.
fn parse_deciders(v: &Value) -> EsResult<Map> {
    let Value::Obj(m) = v else { return Err(EsError::illegal_argument("[deciders] must be an object")) };
    let mut out = Map::new();
    for (name, settings) in m {
        if !DECIDERS.contains(&name.as_str()) {
            return Err(EsError::illegal_argument(format!("unknown decider [{name}]")));
        }
        let mut sorted: Vec<(String, Value)> = Vec::new();
        if let Value::Obj(s) = settings {
            for (k, v) in s {
                if !decider_settings(name).contains(&k.as_str()) {
                    return Err(EsError::illegal_argument(format!("unknown setting [{k}] for decider [{name}]")));
                }
                sorted.push((k.clone(), Value::from(v.to_es_string())));
            }
        }
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let settings: Map = sorted.into_iter().collect();
        if name == "fixed" {
            Fixed::parse(&settings)?;
        }
        out.insert(name.clone(), Value::Obj(settings));
    }
    Ok(out)
}

/// `PUT _autoscaling/policy/{name}`: a new policy needs `roles`; an update keeps what it
/// doesn't set.
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if k != "roles" && k != "deciders" {
            return Err(crate::xpack::unknown_field(ctx, "put_autocaling_policy_request", k));
        }
    }
    let roles: Option<Vec<String>> = body.get("roles").map(strs);
    if let Some(r) = &roles {
        let unknown: Vec<String> = r.iter().filter(|x| !ROLES.contains(&x.as_str())).cloned().collect();
        if !unknown.is_empty() {
            return Err(EsError::action_request_validation(&unknown));
        }
    }
    let deciders = body.get("deciders").map(parse_deciders).transpose()?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let current = st.metadata.xpack.get(POLICIES, &name).cloned();
            let mut roles = match (roles, &current) {
                (Some(r), _) => r,
                (None, Some(c)) => c.get("roles").map(strs).unwrap_or_default(),
                (None, None) => return Err(EsError::illegal_argument(format!("new policy {name} with no roles defined, must provide empty list for no roles"))),
            };
            roles.sort();
            roles.dedup();
            let deciders = match (deciders, &current) {
                (Some(d), _) => d,
                (None, Some(c)) => c.get("deciders").and_then(Value::as_object).cloned().unwrap_or_default(),
                (None, None) => Map::new(),
            };
            if deciders.is_empty() && default_deciders(&roles).is_empty() {
                return Err(EsError::illegal_argument(format!("no default nor user configured deciders for policy [{name}] with roles [[{}]]", roles.join(", "))));
            }
            let policy = obj! { "roles" => Value::Arr(roles.into_iter().map(Value::from).collect()), "deciders" => Value::Obj(deciders) };
            store::put(st, POLICIES, &name, policy, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

/// `GET _autoscaling/policy/{name}`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default();
    ctx.state().metadata.xpack.get(POLICIES, name).cloned().map_or_else(|| Err(not_found(name)), Reply::ok)
}

/// `DELETE _autoscaling/policy/{name}`: a name, or a wildcard pattern (which may match none).
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            if name.contains('*') {
                let matched: Vec<String> = st.metadata.xpack.iter(POLICIES).map(|(k, _)| k.clone()).filter(|k| crate::xcontent::filter::glob_match(&name, k)).collect();
                for k in matched {
                    store::remove(st, POLICIES, &k);
                }
                return Ok(());
            }
            store::remove(st, POLICIES, &name).map(|_| ()).ok_or_else(|| not_found(&name))
        })
        .await?;
    Reply::acknowledged()
}

/// A capacity: storage and memory in bytes, processors when known.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Capacity {
    storage: i64,
    memory: i64,
    processors: Option<f64>,
}

impl Capacity {
    fn to_value(self) -> Value {
        let mut m = Map::new();
        m.insert("storage".into(), self.storage.into());
        m.insert("memory".into(), self.memory.into());
        if let Some(p) = self.processors {
            m.insert("processors".into(), Value::from(p));
        }
        Value::Obj(m)
    }

    /// `AutoscalingCapacity.upperBound`: the larger of each resource.
    fn max(self, o: Capacity) -> Capacity {
        Capacity {
            storage: self.storage.max(o.storage),
            memory: self.memory.max(o.memory),
            processors: match (self.processors, o.processors) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

/// A required capacity: per node (when the decider knows it) and in total.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Required {
    node: Option<Capacity>,
    total: Capacity,
}

impl Required {
    fn to_value(self) -> Value {
        let mut m = Map::new();
        if let Some(n) = self.node {
            m.insert("node".into(), n.to_value());
        }
        m.insert("total".into(), self.total.to_value());
        Value::Obj(m)
    }

    fn max(self, o: Required) -> Required {
        let node = match (self.node, o.node) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        Required { node, total: self.total.max(o.total) }
    }
}

/// The `fixed` decider's settings.
struct Fixed {
    storage: Option<i64>,
    memory: Option<i64>,
    processors: Option<f64>,
    nodes: i64,
}

impl Fixed {
    fn parse(s: &Map) -> EsResult<Fixed> {
        let bytes = |k: &str| -> EsResult<Option<i64>> { s.get(k).map(|v| ByteSize::parse(&v.to_es_string(), k).map(|b| b.bytes())).transpose() };
        let num = |k: &str| -> EsResult<Option<f64>> {
            s.get(k)
                .map(|v| {
                    let t = v.to_es_string();
                    t.parse::<f64>().map_err(|_| EsError::illegal_argument(format!("Failed to parse value [{t}] for setting [{k}]")))
                })
                .transpose()
        };
        Ok(Fixed { storage: bytes("storage")?, memory: bytes("memory")?, processors: num("processors")?, nodes: num("nodes")?.map_or(1, |n| n as i64) })
    }

    /// The capacity it requires (none when no resource is set).
    fn required(&self) -> Option<Required> {
        if self.storage.is_none() && self.memory.is_none() && self.processors.is_none() {
            return None;
        }
        let node = Capacity { storage: self.storage.unwrap_or(0), memory: self.memory.unwrap_or(0), processors: self.processors };
        let n = self.nodes;
        Some(Required { node: Some(node), total: Capacity { storage: node.storage * n, memory: node.memory * n, processors: node.processors.map(|p| p * n as f64) } })
    }

    fn summary(&self) -> String {
        let human = |b: Option<i64>| b.map_or("null".to_string(), |b| ByteSize(b).human());
        let processors = self.processors.map_or("null".to_string(), |p| format!("{p:?}"));
        format!("fixed storage [{}] memory [{}] processors [{processors}] nodes [{}]", human(self.storage), human(self.memory), self.nodes)
    }

    fn details(&self) -> Value {
        let mut m = Map::new();
        if let Some(b) = self.storage {
            m.insert("storage".into(), ByteSize(b).human().into());
        }
        if let Some(b) = self.memory {
            m.insert("memory".into(), ByteSize(b).human().into());
        }
        m.insert("nodes".into(), self.nodes.into());
        if let Some(p) = self.processors {
            m.insert("processors".into(), Value::from(p));
        }
        Value::Obj(m)
    }
}

/// `GET _autoscaling/capacity`.
pub fn capacity(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let metrics = crate::api::nodes::NodeMetrics::sample(ctx.cluster);
    let processors = ctx.cluster.config.processors().unwrap_or_else(search_index::search::pool::cores) as f64;
    let mut node_roles: Vec<String> = crate::api::nodes::roles(ctx.cluster.dialect).iter().map(|r| r.to_string()).collect();
    node_roles.sort();
    let mut policies = Map::new();
    for (name, p) in st.metadata.xpack.iter(POLICIES) {
        let roles = p.get("roles").map(strs).unwrap_or_default();
        let matches = roles == node_roles;
        let current = if matches {
            Capacity { storage: metrics.disk_total, memory: metrics.mem_total, processors: Some(processors) }
        } else {
            Capacity { storage: 0, memory: 0, processors: Some(0.0) }
        };
        let configured = p.get("deciders").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut names: Vec<String> = default_deciders(&roles).into_iter().map(str::to_string).collect();
        names.extend(configured.keys().cloned());
        names.sort();
        names.dedup();
        let mut required: Option<Required> = None;
        let mut deciders = Map::new();
        for d in names {
            let settings = configured.get(&d).and_then(Value::as_object).cloned().unwrap_or_default();
            let (cap, summary, details): (Option<Required>, String, Value) = match d.as_str() {
                "fixed" => {
                    let f = Fixed::parse(&settings)?;
                    (f.required(), f.summary(), f.details())
                }
                "frozen_existence" => {
                    let zero = Capacity { storage: 0, memory: 0, processors: Some(0.0) };
                    (Some(Required { node: None, total: zero }), "indices []".into(), obj! { "indices" => Value::Arr(Vec::new()) })
                }
                "frozen_shards" => (Some(Required::default()), "shard count [0]".into(), obj! { "shards" => 0 }),
                "frozen_storage" => (Some(Required::default()), "total data set size [0]".into(), obj! { "total_data_set_size" => 0 }),
                "ml" => {
                    let reason = "Requesting scale down as tier and/or node size could be smaller";
                    let cap = Required { node: Some(current), total: current };
                    let details = obj! {
                        "waiting_analytics_jobs" => Value::Arr(Vec::new()), "waiting_anomaly_jobs" => Value::Arr(Vec::new()), "waiting_models" => Value::Arr(Vec::new()),
                        "configuration" => Value::Obj(settings.clone()), "perceived_current_capacity" => cap.to_value(), "reason" => reason,
                    };
                    (Some(cap), reason.into(), details)
                }
                // reactive_storage / proactive_storage: nothing waits for storage on one node.
                _ => {
                    let used = (metrics.disk_total - metrics.disk_free).max(0);
                    let cap = Required {
                        node: Some(Capacity { storage: if matches { used } else { 0 }, memory: 0, processors: None }),
                        total: Capacity { storage: current.storage, memory: 0, processors: None },
                    };
                    let mut details = obj! { "reason" => "storage ok", "unassigned" => 0, "assigned" => 0 };
                    if d == "proactive_storage"
                        && let Value::Obj(m) = &mut details
                    {
                        m.insert("forecasted".into(), 0.into());
                        m.insert("forecast_window".into(), settings.get("forecast_window").cloned().unwrap_or_else(|| Value::from("30m")));
                    }
                    (Some(cap), "storage ok".into(), details)
                }
            };
            let mut dm = Map::new();
            if let Some(c) = cap {
                required = Some(required.map_or(c, |r| r.max(c)));
                dm.insert("required_capacity".into(), c.to_value());
            }
            dm.insert("reason_summary".into(), summary.into());
            dm.insert("reason_details".into(), details);
            deciders.insert(d, Value::Obj(dm));
        }
        let mut pm = Map::new();
        if let Some(r) = required {
            pm.insert("required_capacity".into(), r.to_value());
        }
        pm.insert("current_capacity".into(), Required { node: Some(current), total: current }.to_value());
        let nodes: Vec<Value> = if matches { vec![obj! { "name" => ctx.cluster.node.name.clone() }] } else { Vec::new() };
        pm.insert("current_nodes".into(), Value::Arr(nodes));
        pm.insert("deciders".into(), Value::Obj(deciders));
        policies.insert(name.clone(), Value::Obj(pm));
    }
    Reply::ok(obj! { "policies" => Value::Obj(policies) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Elasticsearch 9.5.5's `fixed` decider answer for the YAML suite's policy.
    #[test]
    fn fixed_decider_matches_elasticsearch() {
        let s: Map = [("memory", "7331b"), ("nodes", "10"), ("processors", "2"), ("storage", "1337b")].into_iter().map(|(k, v)| (k.to_string(), Value::from(v))).collect();
        let f = Fixed::parse(&s).unwrap();
        assert_eq!(
            f.required().unwrap().to_value().to_json(),
            r#"{"node":{"storage":1337,"memory":7331,"processors":2.0},"total":{"storage":13370,"memory":73310,"processors":20.0}}"#
        );
        assert_eq!(f.summary(), "fixed storage [1.3kb] memory [7.1kb] processors [2.0] nodes [10]");
        assert_eq!(f.details().to_json(), r#"{"storage":"1.3kb","memory":"7.1kb","nodes":10,"processors":2.0}"#);
        let mut s2 = s.clone();
        s2.shift_remove("processors");
        assert_eq!(Fixed::parse(&s2).unwrap().summary(), "fixed storage [1.3kb] memory [7.1kb] processors [null] nodes [10]");
    }

    #[test]
    fn defaults_by_role() {
        assert!(default_deciders(&["master".into()]).is_empty());
        assert_eq!(default_deciders(&["data_content".into()]), ["proactive_storage", "reactive_storage"]);
        assert_eq!(default_deciders(&["ml".into(), "data_frozen".into()]), ["frozen_existence", "frozen_shards", "frozen_storage", "ml"]);
    }
}
