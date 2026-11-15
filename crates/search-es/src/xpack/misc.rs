//! The small x-pack APIs: `GET _xpack` (info), `_xpack/usage`, `_license`, `_ssl/certificates`.
//!
//! roll provides every x-pack feature it implements on every cluster, so `_xpack` reports them
//! all available and enabled, and `_license` an active, non-expiring `enterprise` license issued
//! by roll. `_xpack/usage` has Elasticsearch 9.5.5's shape (captured), with the counts roll
//! tracks filled in from the cluster's objects.

use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};

const FEATURES: &[(&str, bool)] = &[
    ("aggregate_metric", true),
    ("analytics", true),
    ("archive", true),
    ("ccr", true),
    ("data_streams", true),
    ("data_tiers", true),
    ("enrich", true),
    ("enterprise_search", true),
    ("eql", true),
    ("esql", true),
    ("gpu_vector_indexing", false),
    ("graph", true),
    ("ilm", true),
    ("logsdb", true),
    ("logstash", true),
    ("ml", true),
    ("monitoring", true),
    ("rollup", true),
    ("searchable_snapshots", true),
    ("security", true),
    ("slm", true),
    ("spatial", true),
    ("sql", true),
    ("transform", true),
    ("universal_profiling", true),
    ("voting_only", true),
    ("watcher", true),
];

fn license_uid(ctx: &Ctx) -> String {
    let u = ctx.cluster.uuid();
    // A stable UUID-shaped id derived from the cluster's uuid.
    let d = crate::xpack::security::digest(&[b"license", u.as_bytes()]);
    let h: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// roll's licenses never expire: Elasticsearch prints such a license with this expiry.
const NO_EXPIRY_MS: i64 = 253_402_300_799_999;

pub fn xpack_info(ctx: &Ctx) -> EsResult<Reply> {
    let cats = ctx.list_param("categories").unwrap_or_else(|| vec!["build".into(), "license".into(), "features".into()]);
    let mut out = Map::new();
    for c in &cats {
        if !["build", "license", "features", "_none", "_all"].iter().any(|k| k.eq_ignore_ascii_case(c)) {
            // `Category.valueOf(name.toUpperCase(Locale.ROOT))`.
            return Err(crate::error::EsError::illegal_argument(format!(
                "No enum constant org.elasticsearch.protocol.xpack.XPackInfoRequest.Category.{}",
                c.to_uppercase()
            )));
        }
    }
    if cats.iter().any(|c| c == "build") {
        out.insert("build".into(), obj! { "hash" => crate::version::ES_BUILD_HASH, "date" => crate::version::ES_BUILD_DATE });
    }
    if cats.iter().any(|c| c == "license") {
        out.insert("license".into(), obj! {
            "uid" => license_uid(ctx), "type" => "enterprise", "mode" => "enterprise", "status" => "active", "expiry_date_in_millis" => NO_EXPIRY_MS,
        });
    }
    if cats.iter().any(|c| c == "features") {
        let f: Map = FEATURES.iter().map(|(n, e)| (n.to_string(), obj! { "available" => true, "enabled" => *e })).collect();
        out.insert("features".into(), Value::Obj(f));
    }
    if ctx.query("human") != Some("false") {
        out.insert("tagline".into(), "You know, for X".into());
    }
    Reply::ok(Value::Obj(out))
}

pub fn license(ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! { "license" => obj! {
        "status" => "active",
        "uid" => license_uid(ctx),
        "type" => "enterprise",
        "issue_date" => crate::time::java_instant_string(0),
        "issue_date_in_millis" => 0,
        "expiry_date" => crate::time::java_instant_string(NO_EXPIRY_MS),
        "expiry_date_in_millis" => NO_EXPIRY_MS,
        "max_nodes" => Value::Null,
        "max_resource_units" => 1000,
        "issued_to" => ctx.cluster.name.clone(),
        "issuer" => "roll",
        "start_date_in_millis" => -1,
    } })
}

pub fn xpack_usage(ctx: &Ctx) -> EsResult<Reply> {
    let mut v = crate::json::parse(include_bytes!("usage.json")).ok().flatten().unwrap_or_else(Value::object);
    let st = ctx.state();
    let x = &st.metadata.xpack;
    if let Value::Obj(m) = &mut v {
        if let Some(Value::Obj(e)) = m.get_mut("esql") {
            ctx.cluster.xpack.esql_telemetry.fill(e);
        }
        let rules_family = crate::xpack::entsearch::query_rules::RULESETS;
        let rulesets: Vec<usize> = x.iter(rules_family).map(|(_, r)| r.get("rules").and_then(Value::as_array).map_or(0, |a| a.len())).collect();
        let total_rules: usize = rulesets.iter().sum();
        // `QueryRulesetUsage`: criteria and rule types counted over every rule, the whole
        // section in a `HashMap`'s order.
        let (mut criteria, mut types) = (Map::new(), Map::new());
        for (_, rs) in x.iter(rules_family) {
            for r in rs.get("rules").and_then(Value::as_array).into_iter().flatten() {
                let ty = r.get("type").map(Value::to_es_string).unwrap_or_default();
                let n = types.get(&ty).and_then(Value::as_i64).unwrap_or(0) + 1;
                types.insert(ty, n.into());
                for c in r.get("criteria").and_then(Value::as_array).into_iter().flatten() {
                    let ct = c.get("type").map(Value::to_es_string).unwrap_or_default();
                    let n = criteria.get(&ct).and_then(Value::as_i64).unwrap_or(0) + 1;
                    criteria.insert(ct, n.into());
                }
            }
        }
        let mut qr = Map::new();
        qr.insert("total_rule_count".into(), total_rules.into());
        if !criteria.is_empty() {
            qr.insert("rule_criteria_total_counts".into(), Value::Obj(crate::json::java_hash_map(criteria, 16)));
        }
        qr.insert("total_count".into(), rulesets.len().into());
        qr.insert("min_rule_count".into(), rulesets.iter().min().copied().unwrap_or(0).into());
        qr.insert("max_rule_count".into(), rulesets.iter().max().copied().unwrap_or(0).into());
        if !types.is_empty() {
            qr.insert("rule_type_total_counts".into(), Value::Obj(crate::json::java_hash_map(types, 16)));
        }
        let qr = crate::json::java_hash_map(qr, 16);
        m.insert(
            "enterprise_search".into(),
            obj! {
                "available" => true,
                "enabled" => true,
                "search_applications" => obj! { "count" => x.len("search_application") },
                "analytics_collections" => obj! { "count" => crate::xpack::entsearch::search_application::collection_count(&st) },
                "query_rulesets" => Value::Obj(qr),
            },
        );
    }
    Reply::ok(v)
}

/// `GET _ssl/certificates`: the certificates the node's TLS listener uses. roll terminates TLS
/// in the node (search-server) or a proxy in front; the cluster sees neither, so the node's
/// certificate is described as configured (`ROLL_TLS_CERT`-style paths aren't exposed).
pub fn ssl_certificates(ctx: &Ctx) -> EsResult<Reply> {
    let path = ctx.cluster.config.node_setting("xpack.security.http.ssl.certificate").unwrap_or("certs/http.crt").to_string();
    Reply::ok(Value::Arr(vec![obj! {
        "path" => path,
        "format" => "PEM",
        "alias" => Value::Null,
        "subject_dn" => "CN=roll",
        "serial_number" => "01",
        "has_private_key" => true,
        "expiry" => crate::time::java_instant_string(NO_EXPIRY_MS),
        "issuer" => "CN=roll",
    }]))
}
