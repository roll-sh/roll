//! The privilege model: Elasticsearch's named cluster and index privileges as action-name
//! patterns, the action each REST API performs, and the check of a set of role descriptors
//! against an action.
//!
//! Elasticsearch authorizes *transport actions* (`cluster:monitor/health`,
//! `indices:data/read/search`); a privilege is a set of action patterns, and a role grants an
//! action when one of its privileges' patterns matches it (for index actions, on every index the
//! request targets). roll keeps the same model: [`action_of`] names the action of each REST API
//! (generated from the API's name for the families that follow Elasticsearch's naming scheme),
//! and [`CLUSTER`] / [`INDEX`] list the privileges in Elasticsearch's order, narrowest first, so
//! "granted by" lists in errors read the same.

use crate::json::Value;

/// A named privilege and the action patterns it grants (`!pattern` excludes).
pub struct Named {
    pub name: &'static str,
    pub patterns: &'static [&'static str],
}

/// Cluster privileges (the names `GET _security/privilege/_builtin` lists), narrowest first.
pub const CLUSTER: &[Named] = &[
    Named { name: "none", patterns: &[] },
    Named { name: "cancel_task", patterns: &["cluster:admin/tasks/cancel"] },
    Named { name: "clone_api_key", patterns: &["cluster:admin/xpack/security/api_key/clone"] },
    Named { name: "create_snapshot", patterns: &["cluster:admin/snapshot/create*", "cluster:admin/snapshot/status*", "cluster:admin/snapshot/get", "cluster:admin/repository/get"] },
    Named { name: "cross_cluster_replication", patterns: &["cluster:monitor/state", "cluster:admin/xpack/ccr/internal/*"] },
    Named { name: "cross_cluster_search", patterns: &["cluster:monitor/xpack/enrich/esql/*", "cluster:admin/xpack/security/remote_cluster/*"] },
    Named { name: "delegate_pki", patterns: &["cluster:admin/xpack/security/delegate_pki"] },
    Named { name: "grant_api_key", patterns: &["cluster:admin/xpack/security/api_key/grant*"] },
    Named { name: "manage_own_api_key", patterns: &["cluster:admin/xpack/security/api_key/*"] },
    Named { name: "manage_token", patterns: &["cluster:admin/xpack/security/token/*", "cluster:admin/xpack/security/refresh_token*"] },
    Named { name: "manage_user_profile", patterns: &["cluster:admin/xpack/security/profile/*"] },
    Named { name: "manage_service_account", patterns: &["cluster:admin/xpack/security/service_account/*"] },
    Named { name: "monitor_connector", patterns: &["cluster:monitor/xpack/connector/*"] },
    Named { name: "monitor_enrich", patterns: &["cluster:monitor/xpack/enrich/*", "cluster:admin/xpack/enrich/get"] },
    Named { name: "monitor_esql", patterns: &["cluster:monitor/xpack/esql/*"] },
    Named { name: "monitor_inference", patterns: &["cluster:monitor/xpack/inference*"] },
    Named { name: "monitor_ml", patterns: &["cluster:monitor/xpack/ml/*"] },
    Named { name: "monitor_rollup", patterns: &["cluster:monitor/xpack/rollup/*"] },
    Named { name: "monitor_snapshot", patterns: &["cluster:admin/snapshot/status*", "cluster:admin/snapshot/get", "cluster:admin/repository/get"] },
    Named { name: "monitor_stats", patterns: &["cluster:monitor/stats*"] },
    Named { name: "monitor_text_structure", patterns: &["cluster:monitor/text_structure/*"] },
    Named { name: "monitor_transform", patterns: &["cluster:monitor/data_frame/*", "cluster:monitor/transform/*"] },
    Named { name: "monitor_data_frame_transforms", patterns: &["cluster:monitor/data_frame/*", "cluster:monitor/transform/*"] },
    Named { name: "monitor_watcher", patterns: &["cluster:monitor/xpack/watcher/*"] },
    Named { name: "post_behavioral_analytics_event", patterns: &["cluster:admin/xpack/application/analytics/post_event"] },
    Named { name: "read_ccr", patterns: &["cluster:monitor/state", "cluster:monitor/xpack/ccr/*"] },
    Named { name: "read_ilm", patterns: &["cluster:admin/ilm/get", "cluster:admin/ilm/operation_mode/get"] },
    Named { name: "read_pipeline", patterns: &["cluster:admin/ingest/pipeline/get", "cluster:admin/ingest/pipeline/simulate"] },
    Named { name: "read_security", patterns: &["cluster:admin/xpack/security/*/get", "cluster:admin/xpack/security/*/query", "cluster:admin/xpack/security/user/has_privileges", "cluster:admin/xpack/security/profile/suggest"] },
    Named { name: "read_slm", patterns: &["cluster:admin/slm/get", "cluster:admin/slm/status", "cluster:admin/ilm/operation_mode/get"] },
    Named { name: "transport_client", patterns: &["cluster:monitor/nodes/liveness", "cluster:monitor/state"] },
    Named { name: "manage_autoscaling", patterns: &["cluster:admin/autoscaling/*"] },
    Named { name: "manage_behavioral_analytics", patterns: &["cluster:admin/xpack/application/analytics/*"] },
    Named { name: "manage_ccr", patterns: &["cluster:admin/xpack/ccr/*", "cluster:monitor/state", "cluster:monitor/xpack/ccr/*"] },
    Named { name: "manage_connector", patterns: &["cluster:admin/xpack/connector/*", "cluster:monitor/xpack/connector/*"] },
    Named { name: "manage_enrich", patterns: &["cluster:admin/xpack/enrich/*", "cluster:monitor/xpack/enrich/*"] },
    Named { name: "manage_ilm", patterns: &["cluster:admin/ilm/*"] },
    Named { name: "manage_index_templates", patterns: &["indices:admin/template/*", "indices:admin/index_template/*", "cluster:admin/component_template/*"] },
    Named { name: "manage_inference", patterns: &["cluster:admin/xpack/inference/*", "cluster:monitor/xpack/inference*"] },
    Named { name: "manage_logstash_pipelines", patterns: &["cluster:admin/logstash/pipeline/*"] },
    Named { name: "manage_ml", patterns: &["cluster:admin/xpack/ml/*", "cluster:monitor/xpack/ml/*"] },
    Named { name: "manage_rollup", patterns: &["cluster:admin/xpack/rollup/*", "cluster:monitor/xpack/rollup/*"] },
    Named { name: "manage_search_application", patterns: &["cluster:admin/xpack/application/search_application/*"] },
    Named { name: "manage_search_query_rules", patterns: &["cluster:admin/xpack/query_rules/*"] },
    Named { name: "manage_search_synonyms", patterns: &["cluster:admin/synonyms/*"] },
    Named { name: "manage_slm", patterns: &["cluster:admin/slm/*", "cluster:admin/ilm/start", "cluster:admin/ilm/stop", "cluster:admin/ilm/operation_mode/get"] },
    Named { name: "manage_watcher", patterns: &["cluster:admin/xpack/watcher/*", "cluster:monitor/xpack/watcher/*"] },
    Named { name: "manage_transform", patterns: &["cluster:admin/data_frame/*", "cluster:monitor/data_frame/*", "cluster:admin/transform/*", "cluster:monitor/transform/*"] },
    Named { name: "manage_data_frame_transforms", patterns: &["cluster:admin/data_frame/*", "cluster:monitor/data_frame/*", "cluster:admin/transform/*", "cluster:monitor/transform/*"] },
    Named { name: "manage_ingest_pipelines", patterns: &["cluster:admin/ingest/pipeline/*"] },
    Named { name: "manage_pipeline", patterns: &["cluster:admin/ingest/pipeline/*"] },
    Named { name: "manage_api_key", patterns: &["cluster:admin/xpack/security/api_key/*"] },
    Named { name: "manage_security", patterns: &["cluster:admin/xpack/security/*", "cluster:monitor/xpack/security/*", "cluster:admin/xpack/security_settings/*"] },
    Named { name: "monitor", patterns: &["cluster:monitor/*"] },
    Named { name: "manage", patterns: &["cluster:*", "indices:admin/template/*", "indices:admin/index_template/*", "!cluster:admin/xpack/security/*", "!cluster:monitor/xpack/security/*", "!cluster:admin/xpack/security_settings/*"] },
    Named { name: "all", patterns: &["cluster:*", "indices:admin/template/*", "indices:admin/index_template/*"] },
];

/// Other names Elasticsearch accepts for cluster privileges (no actions of their own in roll).
const CLUSTER_EXTRA: &[&str] = &[
    "manage_data_stream_global_retention",
    "monitor_data_stream_global_retention",
    "manage_oidc",
    "manage_saml",
    "manage_reindex",
    "monitor_reindex",
    "read_connector_secrets",
    "read_fleet_secrets",
    "read_project_routing",
    "write_connector_secrets",
    "write_fleet_secrets",
    "manage_project_routing",
];

/// Index privileges, narrowest first.
pub const INDEX: &[Named] = &[
    Named { name: "none", patterns: &[] },
    Named { name: "auto_configure", patterns: &["indices:admin/auto_create", "indices:admin/mapping/auto_put", "indices:admin/data_stream/create"] },
    Named { name: "create_doc", patterns: &["indices:data/write/index:op_type/create", "indices:data/write/bulk*", "indices:data/write/simulate/bulk*"] },
    Named { name: "create", patterns: &["indices:data/write/index*", "indices:data/write/bulk*", "indices:data/write/simulate/bulk*"] },
    Named { name: "create_index", patterns: &["indices:admin/create", "indices:admin/auto_create", "indices:admin/data_stream/create"] },
    Named { name: "delete", patterns: &["indices:data/write/delete*", "indices:data/write/bulk*"] },
    Named { name: "delete_index", patterns: &["indices:admin/delete", "indices:admin/data_stream/delete"] },
    Named { name: "index", patterns: &["indices:data/write/index*", "indices:data/write/update*", "indices:data/write/bulk*", "indices:data/write/simulate/bulk*"] },
    Named { name: "maintenance", patterns: &["indices:admin/refresh*", "indices:admin/flush*", "indices:admin/synced_flush", "indices:admin/forcemerge*"] },
    Named { name: "manage_data_stream_lifecycle", patterns: &["indices:admin/data_stream/lifecycle/*"] },
    Named { name: "manage_failure_store", patterns: &["indices:admin/data_stream/failure_store/*"] },
    Named { name: "manage_follow_index", patterns: &["indices:admin/xpack/ccr/*", "indices:admin/close", "indices:admin/create"] },
    Named { name: "manage_ilm", patterns: &["indices:admin/ilm/*"] },
    Named { name: "manage_leader_index", patterns: &["indices:admin/xpack/ccr/forget_follower*"] },
    Named { name: "read_cross_cluster", patterns: &["internal:transport/proxy/indices:data/read/*", "indices:data/read/sql/*", "indices:data/read/esql/*"] },
    Named { name: "read_failure_store", patterns: &["indices:data/read/failure_store/*"] },
    Named {
        name: "view_index_metadata",
        patterns: &[
            "indices:admin/aliases/get",
            "indices:admin/get",
            "indices:admin/mappings/fields/get*",
            "indices:admin/mappings/get",
            "indices:admin/settings/get",
            "indices:monitor/settings/get",
            "indices:admin/resolve/index",
            "indices:admin/validate/query*",
            "indices:data/read/field_caps*",
            "indices:admin/data_stream/get",
            "indices:admin/ilm/explain",
            "indices:admin/data_stream/lifecycle/get",
            "indices:admin/data_stream/lifecycle/explain",
            "indices:admin/exists",
        ],
    },
    Named { name: "monitor", patterns: &["indices:monitor/*", "indices:admin/data_stream/get", "indices:admin/data_stream/lifecycle/explain"] },
    Named { name: "read", patterns: &["indices:data/read/*", "indices:admin/resolve/index"] },
    Named { name: "write", patterns: &["indices:data/write/*", "indices:admin/mapping/auto_put"] },
    Named { name: "manage", patterns: &["indices:monitor/*", "indices:admin/*"] },
    Named { name: "all", patterns: &["indices:*"] },
];

const INDEX_EXTRA: &[&str] = &[
    "create_dataset",
    "create_view",
    "cross_cluster_replication",
    "cross_cluster_replication_internal",
    "delete_dataset",
    "delete_view",
    "manage_dataset",
    "manage_view",
    "read_dataset_metadata",
    "read_view_metadata",
];

/// `*` matches any run of characters (Elasticsearch's action automata).
pub fn glob(pattern: &str, s: &str) -> bool {
    crate::xcontent::filter::glob_match(pattern, s)
}

fn named_matches(n: &Named, action: &str) -> bool {
    let mut ok = false;
    for p in n.patterns {
        if let Some(ex) = p.strip_prefix('!') {
            if glob(ex, action) {
                return false;
            }
        } else if glob(p, action) {
            ok = true;
        }
    }
    ok
}

/// Whether a privilege name (or a raw action pattern) grants `action`.
pub fn cluster_grants(privilege: &str, action: &str) -> bool {
    if let Some(n) = CLUSTER.iter().find(|n| n.name == privilege) {
        return named_matches(n, action);
    }
    privilege.contains(':') && glob(privilege, action)
}

pub fn index_grants(privilege: &str, action: &str) -> bool {
    if let Some(n) = INDEX.iter().find(|n| n.name == privilege) {
        return named_matches(n, action);
    }
    privilege.contains(':') && glob(privilege, action)
}

/// The named privileges that grant an action, narrowest first ("this action is granted by").
pub fn granted_by(action: &str, cluster: bool) -> Vec<&'static str> {
    let table = if cluster { CLUSTER } else { INDEX };
    table.iter().filter(|n| named_matches(n, action)).map(|n| n.name).collect()
}

/// Every cluster privilege name in the order Elasticsearch lists them in validation errors.
const CLUSTER_ERROR_ORDER: &str = "manage_own_api_key,manage_data_stream_global_retention,monitor_data_stream_global_retention,none,cancel_task,clone_api_key,cross_cluster_replication,cross_cluster_search,delegate_pki,grant_api_key,manage_autoscaling,manage_index_templates,manage_logstash_pipelines,manage_oidc,manage_saml,manage_search_application,manage_search_query_rules,manage_search_synonyms,manage_service_account,manage_token,manage_user_profile,monitor_connector,monitor_enrich,monitor_esql,monitor_inference,monitor_ml,monitor_reindex,monitor_rollup,monitor_snapshot,monitor_stats,monitor_text_structure,monitor_watcher,post_behavioral_analytics_event,read_ccr,read_connector_secrets,read_fleet_secrets,read_ilm,read_pipeline,read_project_routing,read_security,read_slm,transport_client,write_connector_secrets,write_fleet_secrets,create_snapshot,manage_behavioral_analytics,manage_ccr,manage_connector,manage_enrich,manage_ilm,manage_inference,manage_ml,manage_project_routing,manage_reindex,manage_rollup,manage_slm,manage_watcher,monitor_data_frame_transforms,monitor_transform,manage_ingest_pipelines,manage_pipeline,manage_api_key,manage_data_frame_transforms,manage_transform,manage_security,monitor,manage,all";

const INDEX_ERROR_ORDER: &str = "all,auto_configure,create,create_dataset,create_doc,create_index,create_view,cross_cluster_replication,cross_cluster_replication_internal,delete,delete_dataset,delete_index,delete_view,index,maintenance,manage,manage_data_stream_lifecycle,manage_dataset,manage_failure_store,manage_follow_index,manage_ilm,manage_leader_index,manage_view,monitor,none,read,read_cross_cluster,read_dataset_metadata,read_failure_store,read_view_metadata,view_index_metadata,write";

pub fn is_cluster_privilege(p: &str) -> bool {
    CLUSTER.iter().any(|n| n.name == p) || CLUSTER_EXTRA.contains(&p) || p.starts_with("cluster:") || p.starts_with("indices:admin/template") || p.starts_with("indices:admin/index_template")
}

pub fn is_index_privilege(p: &str) -> bool {
    INDEX.iter().any(|n| n.name == p) || INDEX_EXTRA.contains(&p) || p.starts_with("indices:") || p.starts_with("internal:transport/proxy/indices:")
}

/// `RoleDescriptor` validation's message for an unknown cluster privilege.
pub fn unknown_cluster_privilege(p: &str) -> String {
    format!(
        "unknown cluster privilege [{p}]. a privilege must be either one of the predefined cluster privilege names [{CLUSTER_ERROR_ORDER}] or a pattern over one of the available cluster actions"
    )
}

pub fn unknown_index_privilege(p: &str) -> String {
    format!(
        "unknown index privilege [{p}]. a privilege must be either one of the predefined fixed indices privileges [{INDEX_ERROR_ORDER}] or a pattern over one of the available index actions"
    )
}

/// The scope of an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Cluster,
    /// On the indices the request targets.
    Index,
    /// Any authenticated principal (`_authenticate`, own password, `has_privileges`).
    Any,
}

/// The transport action a REST API performs, Elasticsearch's names where they matter.
pub fn action_of(api: &str) -> (Scope, String) {
    use Scope::*;
    let fixed: Option<(Scope, &str)> = match api {
        "info" | "ping" => Some((Cluster, "cluster:monitor/main")),
        "search" | "async_search.submit" | "search_mvt" | "knn_search" => Some((Index, "indices:data/read/search")),
        "msearch" | "msearch_template" => Some((Index, "indices:data/read/msearch")),
        "search_template" => Some((Index, "indices:data/read/search/template")),
        "count" => Some((Index, "indices:data/read/search")),
        "explain" => Some((Index, "indices:data/read/explain")),
        "field_caps" => Some((Index, "indices:data/read/field_caps")),
        "get" | "exists" | "get_source" | "exists_source" => Some((Index, "indices:data/read/get")),
        "mget" => Some((Index, "indices:data/read/mget")),
        "termvectors" => Some((Index, "indices:data/read/tv")),
        "mtermvectors" => Some((Index, "indices:data/read/mtv")),
        "terms_enum" => Some((Index, "indices:data/read/xpack/termsenum/list")),
        "rank_eval" => Some((Index, "indices:data/read/rank_eval")),
        "open_point_in_time" => Some((Index, "indices:data/read/open_point_in_time")),
        "scroll" | "clear_scroll" | "close_point_in_time" | "async_search.get" | "async_search.status" | "async_search.delete" => {
            Some((Any, "indices:data/read/scroll"))
        }
        "eql.search" => Some((Index, "indices:data/read/eql")),
        "graph.explore" => Some((Index, "indices:data/read/xpack/graph/explore")),
        "esql.query" | "esql.async_query" => Some((Any, "indices:data/read/esql")),
        "sql.query" | "sql.translate" => Some((Any, "indices:data/read/sql")),
        "index" | "create" => Some((Index, "indices:data/write/index")),
        "update" => Some((Index, "indices:data/write/update")),
        "delete" => Some((Index, "indices:data/write/delete")),
        "bulk" => Some((Index, "indices:data/write/bulk")),
        "delete_by_query" => Some((Index, "indices:data/write/delete/byquery")),
        "update_by_query" => Some((Index, "indices:data/write/update/byquery")),
        "reindex" => Some((Cluster, "indices:data/write/reindex")),
        "indices.create" => Some((Index, "indices:admin/create")),
        "indices.delete" => Some((Index, "indices:admin/delete")),
        "indices.get" | "indices.exists" => Some((Index, "indices:admin/get")),
        "indices.get_mapping" => Some((Index, "indices:admin/mappings/get")),
        "indices.get_field_mapping" => Some((Index, "indices:admin/mappings/fields/get")),
        "indices.get_settings" => Some((Index, "indices:monitor/settings/get")),
        "indices.get_alias" | "indices.exists_alias" => Some((Index, "indices:admin/aliases/get")),
        "indices.resolve_index" => Some((Index, "indices:admin/resolve/index")),
        "indices.validate_query" => Some((Index, "indices:admin/validate/query")),
        "indices.refresh" => Some((Index, "indices:admin/refresh")),
        "indices.flush" => Some((Index, "indices:admin/flush")),
        "indices.forcemerge" => Some((Index, "indices:admin/forcemerge")),
        "indices.put_mapping" => Some((Index, "indices:admin/mapping/put")),
        "indices.stats" => Some((Index, "indices:monitor/stats")),
        "indices.get_data_stream" => Some((Index, "indices:admin/data_stream/get")),
        "indices.create_data_stream" => Some((Index, "indices:admin/data_stream/create")),
        "indices.delete_data_stream" => Some((Index, "indices:admin/data_stream/delete")),
        "security.authenticate" | "security.has_privileges" | "security.get_user_privileges" | "security.change_password" => {
            Some((Any, "cluster:admin/xpack/security/user/authenticate"))
        }
        "security.create_api_key"
        | "security.get_api_key"
        | "security.invalidate_api_key"
        | "security.query_api_keys"
        | "security.update_api_key"
        | "security.bulk_update_api_keys" => Some((Cluster, "cluster:admin/xpack/security/api_key/manage")),
        "security.grant_api_key" => Some((Cluster, "cluster:admin/xpack/security/api_key/grant")),
        "security.get_token" | "security.invalidate_token" => Some((Cluster, "cluster:admin/xpack/security/token/manage")),
        _ => None,
    };
    if let Some((s, a)) = fixed {
        return (s, a.to_string());
    }
    let (family, name) = api.split_once('.').unwrap_or(("", api));
    let read = name.starts_with("get")
        || name.starts_with("list")
        || name.starts_with("query")
        || name.contains("stats")
        || name.starts_with("exists")
        || name == "info"
        || name == "status";
    let kind = if read { "monitor" } else { "admin" };
    let action = match family {
        "cluster" | "cat" | "nodes" | "tasks" | "health_report" => {
            if read || family == "cat" || matches!(name, "health" | "state" | "pending_tasks" | "allocation_explain" | "remote_info") {
                format!("cluster:monitor/{family}/{name}")
            } else {
                format!("cluster:admin/{family}/{name}")
            }
        }
        "indices" => {
            if name.contains("template") {
                format!("indices:admin/index_template/{name}")
            } else if read || matches!(name, "segments" | "recovery" | "shard_stores" | "disk_usage" | "field_usage_stats") {
                return (Index, format!("indices:monitor/{name}"));
            } else {
                return (Index, format!("indices:admin/{name}"));
            }
        }
        "ingest" => format!("cluster:admin/ingest/pipeline/{}", if read { "get" } else { name }),
        "snapshot" => {
            if name.contains("repository") {
                format!("cluster:admin/repository/{}", if read { "get" } else { name })
            } else {
                format!("cluster:admin/snapshot/{}", if read { "get" } else { name })
            }
        }
        "security" => format!("cluster:admin/xpack/security/{name}"),
        "ml" => format!("cluster:{kind}/xpack/ml/{name}"),
        "transform" => format!("cluster:{kind}/transform/{name}"),
        "enrich" => format!("cluster:{}/xpack/enrich/{}", if name == "stats" { "monitor" } else { "admin" }, if read { "get" } else { name }),
        "ilm" => format!("cluster:admin/ilm/{}", if read { "get" } else { name }),
        "slm" => format!("cluster:admin/slm/{name}"),
        "watcher" => format!("cluster:{kind}/xpack/watcher/{name}"),
        "connector" => format!("cluster:admin/xpack/connector/{name}"),
        "search_application" => {
            if name.contains("analytics") {
                format!("cluster:admin/xpack/application/analytics/{name}")
            } else {
                format!("cluster:admin/xpack/application/search_application/{name}")
            }
        }
        "query_rules" => format!("cluster:admin/xpack/query_rules/{name}"),
        "synonyms" => format!("cluster:admin/synonyms/{name}"),
        "logstash" => format!("cluster:admin/logstash/pipeline/{name}"),
        "text_structure" => format!("cluster:monitor/text_structure/{name}"),
        "inference" => format!("cluster:admin/xpack/inference/{name}"),
        "esql" | "sql" | "eql" => format!("cluster:monitor/xpack/{family}/{name}"),
        _ => format!("cluster:{kind}/xpack/{family}/{name}"),
    };
    (Cluster, action)
}

/// One `indices` entry of a role descriptor.
#[derive(Clone, Debug)]
pub struct IndexGrant {
    pub names: Vec<String>,
    pub privileges: Vec<String>,
    pub allow_restricted: bool,
}

/// The permissions of one role descriptor.
#[derive(Clone, Debug, Default)]
pub struct Permission {
    pub cluster: Vec<String>,
    pub indices: Vec<IndexGrant>,
    /// `(application, privileges, resources)`.
    pub applications: Vec<(String, Vec<String>, Vec<String>)>,
    pub run_as: Vec<String>,
}

fn strs(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        Some(Value::Str(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

impl Permission {
    /// Reads the `cluster` / `indices` (or `index`) / `applications` / `run_as` of a stored role
    /// descriptor.
    pub fn of_descriptor(d: &Value) -> Permission {
        let mut p = Permission { cluster: strs(d.get("cluster")), run_as: strs(d.get("run_as")), ..Default::default() };
        for e in d.get("indices").or_else(|| d.get("index")).and_then(Value::as_array).into_iter().flatten() {
            p.indices.push(IndexGrant {
                names: strs(e.get("names")),
                privileges: strs(e.get("privileges")),
                allow_restricted: e.get("allow_restricted_indices").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        for e in d.get("applications").and_then(Value::as_array).into_iter().flatten() {
            p.applications.push((e.get("application").map(Value::to_es_string).unwrap_or_default(), strs(e.get("privileges")), strs(e.get("resources"))));
        }
        p
    }

    pub fn allows_cluster(&self, action: &str) -> bool {
        self.cluster.iter().any(|c| cluster_grants(c, action))
    }

    /// Whether the role grants `action` on `index` (restricted indices need
    /// `allow_restricted_indices`).
    pub fn allows_index(&self, action: &str, index: &str, restricted: bool) -> bool {
        self.indices.iter().any(|g| {
            (!restricted || g.allow_restricted)
                && g.names.iter().any(|n| index_pattern_matches(n, index))
                && g.privileges.iter().any(|p| index_grants(p, action))
        })
    }

    /// Whether the role holds a named index privilege on `index` (directly, through `all`, or
    /// through a privilege granting every action of it).
    pub fn has_index_privilege(&self, privilege: &str, index: &str) -> bool {
        self.indices.iter().any(|g| {
            g.names.iter().any(|n| index_pattern_matches(n, index)) && g.privileges.iter().any(|p| p == privilege || p == "all" || implies_index(p, privilege))
        })
    }

    /// Whether the role holds a cluster privilege (by name or action pattern).
    pub fn has_cluster_privilege(&self, privilege: &str) -> bool {
        self.cluster.iter().any(|c| c == privilege || c == "all" || (privilege.contains(':') && cluster_grants(c, privilege)) || implies_cluster(c, privilege))
    }
}

/// Whether every action pattern of named privilege `b` is granted by `a`.
fn implies(table: &[Named], grants: fn(&str, &str) -> bool, a: &str, b: &str) -> bool {
    let Some(nb) = table.iter().find(|n| n.name == b) else { return false };
    !nb.patterns.is_empty()
        && nb.patterns.iter().filter(|p| !p.starts_with('!')).all(|p| {
            let probe = p.trim_end_matches('*');
            grants(a, if probe.is_empty() { p } else { probe })
        })
}

fn implies_index(a: &str, b: &str) -> bool {
    implies(INDEX, index_grants, a, b)
}

fn implies_cluster(a: &str, b: &str) -> bool {
    implies(CLUSTER, cluster_grants, a, b)
}

/// Index name patterns of roles: wildcards, or `/regex/` (Lucene regexp syntax, which for the
/// patterns roles use agrees with Rust's for the common subset; `~(..)` is the complement).
pub fn index_pattern_matches(pattern: &str, index: &str) -> bool {
    if pattern.len() > 1 && pattern.starts_with('/') && pattern.ends_with('/') {
        let inner = &pattern[1..pattern.len() - 1];
        if let Some(rest) = inner.strip_prefix("~(").and_then(|r| r.strip_suffix(')')) {
            return regex::Regex::new(&format!("^(?:{rest})$")).map(|r| !r.is_match(index)).unwrap_or(false);
        }
        return regex::Regex::new(&format!("^(?:{inner})$")).map(|r| r.is_match(index)).unwrap_or(false);
    }
    glob(pattern, index)
}

/// Restricted indices: the security indices and other internal indices roles must opt into.
pub fn is_restricted(index: &str) -> bool {
    index.starts_with(".security") || index.starts_with(".async-search") || index.starts_with(".fleet-") || index.starts_with(".connector-secrets")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn cluster_privileges_grant_actions() {
        assert!(cluster_grants("monitor", "cluster:monitor/health"));
        assert!(cluster_grants("manage", "cluster:admin/settings/update"));
        assert!(!cluster_grants("manage", "cluster:admin/xpack/security/user/put"));
        assert!(cluster_grants("all", "cluster:admin/xpack/security/user/put"));
        assert!(cluster_grants("manage_security", "cluster:admin/xpack/security/user/put"));
        assert!(cluster_grants("cluster:monitor/*", "cluster:monitor/health"));
        assert_eq!(granted_by("cluster:monitor/health", true), vec!["monitor", "manage", "all"]);
    }

    #[test]
    fn index_privileges_grant_actions() {
        assert!(index_grants("read", "indices:data/read/search"));
        assert!(!index_grants("read", "indices:data/write/index"));
        assert!(index_grants("write", "indices:data/write/bulk"));
        assert_eq!(granted_by("indices:data/read/search", false), vec!["read", "all"]);
        let p = Permission::of_descriptor(&obj! { "indices" => crate::arr![obj! { "names" => "logs-*", "privileges" => crate::arr!["read"] }] });
        assert!(p.allows_index("indices:data/read/search", "logs-1", false));
        assert!(!p.allows_index("indices:data/read/search", "other", false));
        assert!(!p.allows_index("indices:data/write/index", "logs-1", false));
        assert!(p.has_index_privilege("read", "logs-1"));
        assert!(!p.has_index_privilege("write", "logs-1"));
        let all = Permission::of_descriptor(&obj! { "cluster" => crate::arr!["manage"] });
        assert!(all.has_cluster_privilege("monitor"));
        assert!(!all.has_cluster_privilege("manage_security"));
    }

    #[test]
    fn regex_patterns() {
        assert!(index_pattern_matches("/~(([.]|ilm-history-).*)/", "logs"));
        assert!(!index_pattern_matches("/~(([.]|ilm-history-).*)/", ".kibana"));
        assert!(index_pattern_matches("/lo.*/", "logs"));
    }

    #[test]
    fn actions_of_apis() {
        assert_eq!(action_of("cluster.health"), (Scope::Cluster, "cluster:monitor/cluster/health".to_string()));
        assert_eq!(action_of("search").0, Scope::Index);
        assert!(cluster_grants("manage_security", &action_of("security.put_user").1));
        assert!(!cluster_grants("manage", &action_of("security.put_user").1));
        assert!(cluster_grants("monitor", &action_of("cluster.health").1));
        assert!(cluster_grants("manage_ml", &action_of("ml.put_job").1));
        assert!(cluster_grants("monitor_ml", &action_of("ml.get_jobs").1));
        assert!(!cluster_grants("monitor_ml", &action_of("ml.put_job").1));
    }
}
