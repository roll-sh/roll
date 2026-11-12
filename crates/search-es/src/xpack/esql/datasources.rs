//! ES|QL federation metadata: `_query/data_source` (connections to external storage) and
//! `_query/dataset` (a resource of a data source, with an optional declared schema), served only
//! when the node runs with `esql.federation.enabled`, as Elasticsearch registers these handlers.
//!
//! roll stores both in the cluster state's x-pack families (never shown by `GET _cluster/state`,
//! like Elasticsearch's `esql_datasource` / `esql_dataset` customs, which the API context hides)
//! and validates them exactly as Elasticsearch 9.5.5 does with only its `s3` validator loaded:
//! the name rules, the `ObjectParser` errors with their locations, the s3 settings and auth-mode
//! rules (`FileDataSourceConfiguration`), and the dataset's declared mappings. Storing a secret
//! needs cluster-state encryption, which neither Elasticsearch's default node nor roll has, so
//! secrets fail with the same 503. Lists come out in Java `HashMap` order, as Elasticsearch prints
//! its metadata maps. Querying external data (`FROM` a dataset) is not implemented.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;

const DATA_SOURCES: &str = "esql.data_source";
const DATASETS: &str = "esql.dataset";

/// Whether the federation APIs exist on this node (`esql.federation.enabled`).
pub fn enabled(cluster: &Cluster) -> bool {
    cluster.config.node_setting("esql.federation.enabled") == Some("true")
}

/// Whether `api` is one of the federation APIs (absent unless [`enabled`]).
pub fn is_federation_api(api: crate::routes::Api) -> bool {
    use crate::routes::Api::*;
    matches!(api, EsqlPutDataSource | EsqlGetDataSource | EsqlDeleteDataSource | EsqlPutDataset | EsqlGetDataset | EsqlDeleteDataset)
}

/// `MetadataCreateIndexService.validateIndexOrAliasName`'s checks (plus lowercase), every failed
/// one reported (Elasticsearch accumulates them into one validation exception).
pub(super) fn validate_name(what: &str, name: &str) -> EsResult<()> {
    validate_name_with(what, name, Vec::new())
}

/// [`validate_name`], with more request errors to report after the name's.
pub(super) fn validate_name_with(what: &str, name: &str, more: Vec<String>) -> EsResult<()> {
    // Views check no "missing" name: a blank one fails on its characters.
    if what != "view" && name.trim().is_empty() {
        return Err(EsError::action_request_validation(&[format!("{what} name is missing")]));
    }
    let bad = |why: &str| format!("invalid {what} name [{name}], {why}");
    let mut errs = Vec::new();
    if name.contains('#') {
        errs.push(bad("must not contain '#'"));
    }
    if name.contains(':') {
        errs.push(bad("must not contain ':'"));
    }
    if name.starts_with(['_', '-', '+']) {
        errs.push(bad("must not start with '_', '-', or '+'"));
    }
    if name.chars().any(|c| matches!(c, ' ' | '"' | '*' | ',' | '/' | '<' | '>' | '?' | '\\' | '|')) {
        errs.push(bad(r#"must not contain the following characters [' ','"','*',',','/','<','>','?','\','|']"#));
    }
    if name == "." || name == ".." {
        errs.push(bad("must not be '.' or '..'"));
    }
    if name.chars().any(char::is_uppercase) {
        errs.push(bad("must be lowercase"));
    }
    errs.extend(more);
    if errs.is_empty() { Ok(()) } else { Err(EsError::action_request_validation(&errs)) }
}

/// The token type name `ObjectParser` reports for a value it can't take.
fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

/// The request body as an `ObjectParser` (named `parser`) reads it: an object whose fields
/// are all in `fields` (`true`: a string field, `false`: an object field).
pub(super) struct Body<'a> {
    ctx: &'a Ctx<'a>,
    parser: &'static str,
    pub(super) map: Map,
}

impl<'a> Body<'a> {
    pub(super) fn read(ctx: &'a Ctx<'a>, parser: &'static str, fields: &[(&str, bool)]) -> EsResult<Body<'a>> {
        let Some(v) = ctx.body()? else { return Err(EsError::parse("request body or source parameter is required")) };
        let map = match v {
            Value::Obj(m) => m,
            other => return Err(EsError::x_content_parse(1, 1, format!("[{parser}] Expected START_OBJECT but was: {}", token_name(&other)))),
        };
        let b = Body { ctx, parser, map };
        for (k, v) in &b.map {
            let Some((_, string)) = fields.iter().find(|(f, _)| f == k) else {
                let (l, c) = b.at(&[Seg::Key(k)], Target::Name);
                return Err(EsError::x_content_parse(l, c, format!("[{parser}] unknown field [{k}]")));
            };
            let ok = if *string { matches!(v, Value::Str(_) | Value::Null) } else { matches!(v, Value::Obj(_) | Value::Null) };
            if !ok {
                let (l, c) = b.at(&[Seg::Key(k)], Target::Value);
                return Err(EsError::x_content_parse(l, c, format!("[{parser}] {k} doesn't support values of type: {}", token_name(v))));
            }
        }
        Ok(b)
    }

    fn at(&self, path: &[Seg<'_>], t: Target) -> (usize, usize) {
        self.ctx.raw_json().and_then(|src| locate(src, path, t)).unwrap_or((1, 1))
    }

    fn str(&self, k: &str) -> Option<&str> {
        self.map.get(k).and_then(Value::as_str)
    }

    pub(super) fn required(&self, k: &str) -> EsResult<String> {
        self.str(k).map(str::to_string).ok_or_else(|| EsError::illegal_argument(format!("Required [{k}]")))
    }

    fn object(&self, k: &str) -> Map {
        self.map.get(k).and_then(Value::as_object).cloned().unwrap_or_default()
    }
}

/// `validation_exception` (`ValidationException`, not the action-request kind).
fn validation(errs: &[String]) -> EsError {
    let mut s = String::from("Validation Failed: ");
    for (i, e) in errs.iter().enumerate() {
        s.push_str(&format!("{}: {e};", i + 1));
    }
    EsError::java(400, "validation_exception", s)
}

/// The s3 connection settings (`S3Configuration`), in the order Elasticsearch's `HashMap` of them
/// prints in its "known settings" message.
const S3_KNOWN: &str = "[auth, role_session_name, access_key, endpoint, region, secret_key, jwt_audience, role_arn, sts_region, session_token, sts_endpoint]";
const S3_SECRETS: &[&str] = &["access_key", "secret_key", "session_token"];
const S3_FEDERATED: &[&str] = &["role_arn", "role_session_name", "jwt_audience", "sts_endpoint", "sts_region"];
const S3_PLAIN: &[&str] = &["endpoint", "region", "auth"];
const AUTH_VALUES: &str = "[auto, anonymous, static_credentials, federated_identity, managed_identity]";
const MANAGED_IDENTITY_DISABLED: &str = "auth=managed_identity requires the [esql.datasource.managed_identity.enabled] cluster setting to be enabled; it is disabled by default \u{2014} enable it only on single-cloud single-tenant deployments";
const UNRESOLVED_AUTH: &str = "S3 data source requires credentials: set access_key and secret_key (optionally session_token for STS temporary credentials); set auth=anonymous for public buckets; set auth=managed_identity to use the node's instance role (requires the esql.datasource.managed_identity.enabled cluster setting); or configure federated authentication with role_arn";

/// Validates s3 settings (`FileDataSourceValidator` + `S3Configuration`); returns them with a
/// deprecated `auth` alias rewritten, plus the deprecation warning it raised.
fn validate_s3(cluster: &Cluster, mut settings: Map) -> EsResult<(Map, Option<String>)> {
    // An empty settings map is no configuration at all (`S3Configuration.fromMap` gives null).
    if settings.is_empty() {
        return Ok((settings, None));
    }
    let mut errs = Vec::new();
    for k in settings.keys() {
        if !S3_SECRETS.contains(&k.as_str()) && !S3_FEDERATED.contains(&k.as_str()) && !S3_PLAIN.contains(&k.as_str()) {
            errs.push(format!("unknown setting [{k}]; known settings: {S3_KNOWN}"));
        }
    }
    let mut warning = None;
    let mut auth = settings.get("auth").map(Value::to_es_string).map(|a| a.to_ascii_lowercase());
    if let Some(a) = auth.as_deref() {
        let canonical = match a {
            "none" => Some("anonymous"),
            "workload_identity" => Some("managed_identity"),
            _ => None,
        };
        if let Some(c) = canonical {
            warning = Some(format!("auth value [{a}] is deprecated; the canonical value is [{c}]"));
            settings.insert("auth".into(), Value::Str(c.into()));
            auth = Some(c.to_string());
        }
    }
    let present = |k: &str| settings.get(k).is_some_and(|v| !matches!(v, Value::Null));
    let secrets = S3_SECRETS.iter().any(|k| present(k));
    let federated = S3_FEDERATED.iter().any(|k| present(k));
    let credentials = present("access_key") && present("secret_key");
    let auth = auth.as_deref();
    if let Some(a) = auth
        && !matches!(a, "auto" | "anonymous" | "static_credentials" | "federated_identity" | "managed_identity")
    {
        errs.push(format!("Unsupported auth value [{a}]; supported values: {AUTH_VALUES}"));
    }
    match auth {
        Some("anonymous") => {
            if secrets {
                errs.push("auth=anonymous cannot be combined with explicit credentials; anonymous access uses no credentials".into());
            }
            if federated {
                errs.push("auth=anonymous cannot be combined with federated authentication settings; anonymous access uses no credentials".into());
            }
        }
        Some("managed_identity") => {
            if secrets {
                errs.push("auth=managed_identity cannot be combined with explicit credentials".into());
            }
            if federated {
                errs.push("auth=managed_identity cannot be combined with federated authentication settings".into());
            }
        }
        Some("static_credentials") => {
            if !credentials {
                errs.push("auth=static_credentials requires complete explicit credentials".into());
            }
            if federated {
                errs.push("auth=static_credentials cannot be combined with federated authentication settings".into());
            }
        }
        Some("federated_identity") => {
            if !federated {
                errs.push("auth=federated_identity requires federated authentication settings".into());
            }
            if secrets {
                errs.push("auth=federated_identity cannot be combined with explicit credentials".into());
            }
        }
        None | Some("auto") => {
            if !credentials && !federated {
                errs.push(UNRESOLVED_AUTH.into());
            }
            if secrets && federated {
                errs.push("explicit credentials cannot be combined with federated authentication settings".into());
            }
        }
        Some(_) => {}
    }
    if federated && !present("role_arn") {
        errs.push("role_arn is required when federated authentication settings are configured".into());
    }
    if errs.is_empty() {
        // The cluster-setting gates run once the configuration itself is consistent.
        if auth == Some("managed_identity") && cluster.config.node_setting("esql.datasource.managed_identity.enabled") != Some("true") {
            errs.push(MANAGED_IDENTITY_DISABLED.into());
        } else if federated && cluster.config.node_setting("esql.datasource.federated_identity.enabled") != Some("true") {
            errs.push("federated authentication settings require the [esql.datasource.federated_identity.enabled] cluster setting to be enabled; it is disabled by default".into());
        }
    }
    if !errs.is_empty() {
        return Err(validation(&errs));
    }
    Ok((settings, warning))
}

/// The family's entries in the order Elasticsearch's `HashMap` of them iterates.
pub(super) fn hash_ordered<'s>(st: &'s crate::meta::ClusterState, family: &str) -> Vec<&'s Value> {
    let mut all: Vec<(&String, &Value)> = st.metadata.xpack.iter(family).collect();
    all.sort_by_key(|(_, v)| v.get("seq").and_then(Value::as_i64).unwrap_or(0));
    let names: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
    crate::json::java_hash_order(&names, 16).into_iter().map(|i| all[i].1).collect()
}

/// The stored object as the API prints it (without roll's bookkeeping).
fn public(v: &Value) -> Value {
    let mut m = v.as_object().cloned().unwrap_or_default();
    m.shift_remove("seq");
    Value::Obj(m)
}

/// `GET` over `{name}` (a comma-separated list of names and wildcards): every name must exist,
/// a wildcard may match nothing.
fn get(ctx: &Ctx, family: &str, what: &str, key: &str) -> EsResult<Reply> {
    let st = ctx.state();
    let all = hash_ordered(&st, family);
    let name_of = |v: &Value| v.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let out: Vec<Value> = match ctx.list_param("name").filter(|n| !n.is_empty()) {
        None => all.iter().map(|v| public(v)).collect(),
        Some(names) => {
            if let Some(missing) = names.iter().find(|n| !n.contains('*') && !all.iter().any(|v| name_of(v) == **n)) {
                return Err(EsError::resource_not_found(format!("{what} [{missing}] not found")));
            }
            all.iter()
                .filter(|v| {
                    let n = name_of(v);
                    names.iter().any(|p| crate::xcontent::filter::glob_match(p, &n))
                })
                .map(|v| public(v))
                .collect()
        }
    };
    Reply::ok(obj! { key => Value::Arr(out) })
}

fn next_seq(st: &crate::meta::ClusterState, family: &str, name: &str) -> i64 {
    if let Some(seq) = st.metadata.xpack.get(family, name).and_then(|v| v.get("seq")).and_then(Value::as_i64) {
        return seq;
    }
    st.metadata.xpack.iter(family).filter_map(|(_, v)| v.get("seq").and_then(Value::as_i64)).max().unwrap_or(0) + 1
}

/// `PUT _query/data_source/{name}`.
pub async fn put_data_source(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = Body::read(ctx, "esql_put_data_source", &[("type", true), ("description", true), ("settings", false)])?;
    let ty = body.required("type")?;
    validate_name("data source", &name)?;
    if ty != "s3" {
        return Err(EsError::illegal_argument(format!("unknown data source type [{ty}]")));
    }
    let (settings, warning) = validate_s3(ctx.cluster, body.object("settings"))?;
    if S3_SECRETS.iter().any(|k| settings.get(*k).is_some_and(|v| !matches!(v, Value::Null))) {
        let why = "cluster state encryption is unavailable (disabled). Configure cluster.state.encryption.active_password_id and the matching cluster.state.encryption.password.<id> in the keystore.";
        return Err(EsError::java(503, "status_exception", format!("cannot store secrets for data source [{name}]: {why}"))
            .caused_by(EsError::java(503, "encryption_service_unavailable_exception", why)));
    }
    if let Some(w) = warning {
        ctx.warn(w);
    }
    // Settings print in `HashMap` order (`new HashMap<>(settings.size())`).
    let settings = crate::json::java_hash_map(settings.into_iter().filter(|(_, v)| !matches!(v, Value::Null)).collect(), 1);
    let description = body.str("description").map(str::to_string);
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let seq = next_seq(st, DATA_SOURCES, &name);
            let mut v = Map::new();
            v.insert("name".into(), Value::Str(name.clone()));
            v.insert("type".into(), Value::Str(ty));
            if let Some(d) = description {
                v.insert("description".into(), Value::Str(d));
            }
            v.insert("settings".into(), Value::Obj(settings));
            v.insert("seq".into(), Value::from(seq));
            store::put(st, DATA_SOURCES, &name, Value::Obj(v), now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub fn get_data_source(ctx: &Ctx) -> EsResult<Reply> {
    get(ctx, DATA_SOURCES, "data source", "data_sources")
}

/// `DELETE _query/data_source/{name}`: refused while a dataset still uses it.
pub async fn delete_data_source(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let names: Vec<String> = ctx.list_param("name").unwrap_or_default();
    ctx.cluster
        .update(move |st| {
            if let Some(n) = names.iter().find(|n| st.metadata.xpack.get(DATA_SOURCES, n).is_none()) {
                return Err(EsError::resource_not_found(format!("data source [{n}] not found")));
            }
            for n in &names {
                let users: Vec<String> = hash_ordered(st, DATASETS)
                    .into_iter()
                    .filter(|d| d.get("data_source").and_then(Value::as_str) == Some(n.as_str()))
                    .filter_map(|d| d.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect();
                if !users.is_empty() {
                    return Err(EsError::java(409, "status_exception", format!("cannot delete data source [{n}]: referenced by datasets [{}]", users.join(", "))));
                }
            }
            for n in &names {
                store::remove(st, DATA_SOURCES, n);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// The dataset settings an s3 data source's datasets take (`FileDatasetValidator`), in its
/// `HashMap` order.
const DATASET_KNOWN: &[&str] = &[
    "max_errors",
    "partition_path",
    "schema_sample_size",
    "max_error_ratio",
    "format",
    "target_split_size",
    "error_mode",
    "late_materialization",
    "hive_partitioning",
    "optimized_reader",
    "partition_detection",
    "schema_resolution",
];

/// The column types a declared schema may use (`DeclaredSchemaValidator.DECLARABLE_TYPES`).
const DECLARABLE: &str = "[boolean, date_nanos, datetime, double, integer, ip, keyword, long, text, unsigned_long]";

/// Parses and validates `mappings` (`DatasetMapping.parseMappings`, `DatasetFieldMapping` and
/// `DeclaredSchemaValidator`), returning it as Elasticsearch prints it.
fn parse_mappings(body: &Body<'_>, m: &Map) -> EsResult<Value> {
    let outer = |path: &[Seg<'_>], t: Target, cause: EsError| {
        let (l, c) = body.at(path, t);
        EsError::x_content_parse(l, c, format!("[{}] failed to parse field [mappings]", body.parser)).caused_by(cause)
    };
    let mut dynamic = "true";
    let mut properties = Map::new();
    let mut id = None;
    for (k, v) in m {
        match k.as_str() {
            "dynamic" => {
                dynamic = match v.to_es_string().as_str() {
                    "true" => "true",
                    "false" => "false",
                    other => {
                        return Err(outer(&[Seg::Key("mappings"), Seg::Key(k)], Target::Value, EsError::illegal_argument(format!("unknown dynamic value [{other}]; supported values are [true, false]"))));
                    }
                }
            }
            "properties" => {
                for (col, f) in v.as_object().into_iter().flatten() {
                    let mut out = Map::new();
                    for (fk, fv) in f.as_object().into_iter().flatten() {
                        if !matches!(fk.as_str(), "type" | "path" | "format") {
                            let path = [Seg::Key("mappings"), Seg::Key("properties"), Seg::Key(col), Seg::Key(fk)];
                            let (l, c) = body.at(&path, Target::Name);
                            return Err(outer(&path, Target::Value, EsError::x_content_parse(l, c, format!("[dataset_field_mapping] unknown field [{fk}]"))));
                        }
                        out.insert(fk.clone(), fv.clone());
                    }
                    properties.insert(col.clone(), Value::Obj(out));
                }
            }
            "_id" => {
                for (ik, iv) in v.as_object().into_iter().flatten() {
                    if ik != "path" {
                        return Err(outer(&[Seg::Key("mappings"), Seg::Key("_id"), Seg::Key(ik)], Target::Name, EsError::illegal_argument(format!("unknown [_id] field [{ik}]; only [path] is supported"))));
                    }
                    id = Some(iv.to_es_string());
                }
            }
            _ => return Err(outer(&[Seg::Key("mappings"), Seg::Key(k)], Target::Value, EsError::illegal_argument(format!("unknown mappings field [{k}]")))),
        }
    }
    if dynamic == "false" && properties.is_empty() {
        return Err(EsError::illegal_argument("[dynamic: false] requires at least one declared column under [properties]"));
    }
    for (col, f) in &properties {
        let ty = f.get("type").map(Value::to_es_string).unwrap_or_default();
        let resolved = match ty.as_str() {
            "date" => "datetime",
            t => t,
        };
        if !matches!(resolved, "boolean" | "date_nanos" | "datetime" | "double" | "integer" | "ip" | "keyword" | "long" | "text" | "unsigned_long") {
            return Err(EsError::illegal_argument(format!("unsupported declared type [{ty}] for column [{col}]; supported types are {DECLARABLE}")));
        }
        if f.get("format").is_some() && !matches!(resolved, "datetime" | "date_nanos") {
            return Err(EsError::illegal_argument(format!("[format] on column [{col}] is only supported on [date] and [date_nanos] columns")));
        }
    }
    if let Some(p) = &id
        && dynamic == "false"
        && !properties.contains_key(p)
    {
        return Err(EsError::illegal_argument(format!("[_id] references column [{p}] which is not declared, and dynamic is [false]")));
    }
    let mut out = Map::new();
    out.insert("dynamic".into(), Value::Str(dynamic.into()));
    out.insert("properties".into(), Value::Obj(properties));
    if let Some(p) = id {
        out.insert("_id".into(), obj! { "path" => p });
    }
    Ok(Value::Obj(out))
}

/// `PUT _query/dataset/{name}`.
pub async fn put_dataset(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let body = Body::read(ctx, "esql_put_dataset", &[("data_source", true), ("resource", true), ("description", true), ("settings", false), ("mappings", false)])?;
    let mappings = match body.map.get("mappings").and_then(Value::as_object) {
        Some(m) => Some(parse_mappings(&body, m)?),
        None => None,
    };
    let data_source = body.required("data_source")?;
    let resource = body.required("resource")?;
    validate_name("dataset", &name)?;
    if ctx.state().metadata.xpack.get(DATA_SOURCES, &data_source).is_none() {
        return Err(EsError::resource_not_found(format!("data source [{data_source}] not found")));
    }
    let settings = body.object("settings");
    let unknown: Vec<String> = settings
        .keys()
        .filter(|k| !DATASET_KNOWN.contains(&k.as_str()))
        .map(|k| format!("unknown setting [{k}]; known settings: [{}]", DATASET_KNOWN.join(", ")))
        .collect();
    if !unknown.is_empty() {
        return Err(validation(&unknown));
    }
    let description = body.str("description").map(str::to_string);
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(DATA_SOURCES, &data_source).is_none() {
                return Err(EsError::resource_not_found(format!("data source [{data_source}] not found")));
            }
            let seq = next_seq(st, DATASETS, &name);
            let mut v = Map::new();
            v.insert("name".into(), Value::Str(name.clone()));
            v.insert("data_source".into(), Value::Str(data_source));
            v.insert("resource".into(), Value::Str(resource));
            if let Some(d) = description {
                v.insert("description".into(), Value::Str(d));
            }
            if !settings.is_empty() {
                v.insert("settings".into(), Value::Obj(crate::json::java_hash_map(settings, 1)));
            }
            if let Some(m) = mappings {
                v.insert("mappings".into(), m);
            }
            v.insert("seq".into(), Value::from(seq));
            store::put(st, DATASETS, &name, Value::Obj(v), now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub fn get_dataset(ctx: &Ctx) -> EsResult<Reply> {
    get(ctx, DATASETS, "dataset", "datasets")
}

/// `DELETE _query/dataset/{name}`: names and wildcards (a wildcard may match nothing).
pub async fn delete_dataset(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let names: Vec<String> = ctx.list_param("name").unwrap_or_default();
    ctx.cluster
        .update(move |st| {
            if let Some(n) = names.iter().find(|n| !n.contains('*') && st.metadata.xpack.get(DATASETS, n).is_none()) {
                return Err(EsError::resource_not_found(format!("dataset [{n}] not found")));
            }
            let doomed: Vec<String> = st
                .metadata
                .xpack
                .iter(DATASETS)
                .map(|(k, _)| k.clone())
                .filter(|k| names.iter().any(|p| crate::xcontent::filter::glob_match(p, k)))
                .collect();
            for k in doomed {
                store::remove(st, DATASETS, &k);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fail_like_elasticsearch() {
        let reason = |n: &str| validate_name("data source", n).err().map(|e| e.reason().to_string());
        assert_eq!(reason("ok.name"), None);
        assert_eq!(reason(" ").as_deref(), Some("Validation Failed: 1: data source name is missing;"));
        assert_eq!(
            reason("Bad#x").as_deref(),
            Some("Validation Failed: 1: invalid data source name [Bad#x], must not contain '#';2: invalid data source name [Bad#x], must be lowercase;")
        );
        assert!(reason("_x").unwrap().contains("must not start with '_', '-', or '+'"));
        assert!(reason("..").unwrap().contains("must not be '.' or '..'"));
    }

    #[test]
    fn lists_iterate_in_java_hash_map_order() {
        // Captured from Elasticsearch 9.5.5: inserted in this order, listed as x0, x1, .hidden, d1, a...
        let long = "a".repeat(255);
        let keys = [".hidden", long.as_str(), "x1", "x0", "d1"];
        let order: Vec<&str> = crate::json::java_hash_order(&keys, 16).into_iter().map(|i| keys[i]).collect();
        assert_eq!(order, ["x0", "x1", ".hidden", "d1", long.as_str()]);
        let settings = ["auth", "region"];
        let order: Vec<&str> = crate::json::java_hash_order(&settings, 1).into_iter().map(|i| settings[i]).collect();
        assert_eq!(order, ["region", "auth"]);
    }
}
