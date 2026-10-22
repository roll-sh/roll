//! Snapshot and repository APIs (`/_snapshot`): register, get, verify, clean up and delete
//! repositories; create, get, status, clone, delete and restore snapshots; the
//! `repositories.default_repository` setting.
//!
//! Repositories follow Elasticsearch's rules for names, types (`fs` under `path.repo`, `url`
//! within `path.repo` or `repositories.url.allowed_urls`, `source` over a delegate type; other
//! types answer as on a node without their plugin) and read-only flags. Contents are kept per
//! location ([`crate::meta::snapshots`]): a snapshot captures index and data stream metadata
//! and, with `include_global_state`, templates, pipelines, scripts and persistent settings.
//! Shard data isn't captured yet, so each shard snapshot is one small, successful file.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::names::random_uuid;
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::settings::Settings;
use crate::meta::snapshots::{Repository, RepositoryData, Snapshot};
use crate::meta::{ClusterState, IndexMeta, IndexState, Metadata};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::sync::Arc;

/// Bytes and files reported for each shard snapshot (an empty shard's commit point).
const SHARD_BYTES: i64 = 225;

/// `Strings.INVALID_FILENAME_CHARS` in the order Elasticsearch prints the set.
const INVALID_CHARS: &[char] = &[' ', '"', '*', '\\', '<', '|', ',', '>', '/', '?'];
const INVALID_CHARS_TEXT: &str = "[ , \", *, \\, <, |, ,, >, /, ?]";

fn repository_exception(name: &str, msg: impl std::fmt::Display) -> EsError {
    EsError::new(500, "repository_exception", format!("[{name}] {msg}"))
}

fn repository_missing(name: &str) -> EsError {
    EsError::new(404, "repository_missing_exception", format!("[{name}] missing"))
}

fn snapshot_missing(repo: &str, snap: &str) -> EsError {
    EsError::new(404, "snapshot_missing_exception", format!("[{repo}:{snap}] is missing"))
}

/// Registered repositories, for the health report.
pub fn repository_count(st: &ClusterState) -> usize {
    st.metadata.repositories.len()
}

/// `RepositoriesService.validateRepositoryName`.
fn validate_repository_name(name: &str) -> EsResult<()> {
    if name.is_empty() {
        return Err(repository_exception(name, "cannot be empty"));
    }
    if name.contains('#') {
        return Err(repository_exception(name, "must not contain '#'"));
    }
    if name.contains(INVALID_CHARS) {
        return Err(repository_exception(name, format!("must not contain the following characters {INVALID_CHARS_TEXT}")));
    }
    Ok(())
}

/// `path.repo` of the node.
fn repo_paths(ctx: &Ctx) -> Vec<String> {
    ctx.cluster.config.node_setting("path.repo").map(|v| v.split(',').map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| !p.is_empty()).collect()).unwrap_or_default()
}

/// `Environment.resolveRepoDir`: a relative location resolves under the first `path.repo`; an
/// absolute one must lie under one of them.
fn resolve_location(paths: &[String], location: &str) -> Option<String> {
    let first = paths.first()?;
    let full = if location.starts_with('/') { location.to_string() } else { format!("{first}/{location}") };
    let normalized = normalize_path(&full);
    paths.iter().any(|p| normalized == *p || normalized.starts_with(&format!("{p}/"))).then_some(normalized)
}

fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Validates a repository definition the way creating the repository instance does.
fn validate_repository(ctx: &Ctx, name: &str, ty: &str, settings: &Settings) -> EsResult<()> {
    let paths = repo_paths(ctx);
    let fs_location = |settings: &Settings| -> EsResult<()> {
        let location = settings.get("location").unwrap_or_default();
        if location.is_empty() {
            return Err(repository_exception(name, "missing location"));
        }
        if resolve_location(&paths, location).is_none() {
            let reason = if paths.is_empty() {
                format!("location [{location}] doesn't match any of the locations specified by path.repo because this setting is empty")
            } else {
                format!("location [{location}] doesn't match any of the locations specified by path.repo")
            };
            return Err(repository_exception(name, reason));
        }
        Ok(())
    };
    let failed = |e: EsError| repository_exception(name, "failed to create repository").caused_by(e);
    match ty {
        "fs" => fs_location(settings).map_err(failed),
        "source" => {
            let Some(delegate) = settings.get("delegate_type").filter(|d| !d.is_empty()) else {
                return Err(failed(EsError::illegal_argument("Repository [source] requires a [delegate_type] setting")));
            };
            match delegate {
                "fs" => fs_location(settings).map_err(failed),
                "url" => validate_url(ctx, name, settings, &paths),
                other => Err(repository_exception(name, format!("repository type [{other}] does not exist for project [default]"))),
            }
        }
        "url" => validate_url(ctx, name, settings, &paths),
        other => Err(repository_exception(name, format!("repository type [{other}] does not exist for project [default]"))),
    }
}

/// `URLRepository`: `file:` URLs within `path.repo`, others within
/// `repositories.url.allowed_urls`; a mismatch fails the repository's verification.
fn validate_url(ctx: &Ctx, name: &str, settings: &Settings, paths: &[String]) -> EsResult<()> {
    let url = settings.get("url").unwrap_or_default();
    if url.is_empty() {
        return Err(repository_exception(name, "failed to create repository").caused_by(EsError::illegal_argument("[url] must be set")));
    }
    let allowed: Vec<String> = ctx
        .cluster
        .config
        .node_setting("repositories.url.allowed_urls")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let ok = match url.strip_prefix("file:") {
        Some(path) => resolve_location(paths, path.trim_start_matches("//")).is_some(),
        None => allowed.iter().any(|a| glob_match(a, url)),
    };
    if ok {
        return Ok(());
    }
    Err(EsError::new(500, "repository_verification_exception", format!("[{name}] path  is not accessible on master node")).caused_by(repository_exception(
        name,
        format!("file url [{url}] doesn't match any of the locations specified by path.repo or repositories.url.allowed_urls"),
    )))
}

fn repo_value(st: &ClusterState, name: &str, r: &Repository) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), r.ty.clone().into());
    if let Some(d) = st.metadata.repository_data.get(&r.location_key()) {
        m.insert("uuid".into(), d.uuid.clone().into());
    }
    m.insert("settings".into(), r.settings.to_json(false));
    let _ = name;
    Value::Obj(m)
}

/// The default repository (`repositories.default_repository`), if set.
fn default_repository(md: &Metadata) -> Option<&str> {
    md.cluster_setting("repositories.default_repository").filter(|s| !s.is_empty())
}

/// Validates a new value of `repositories.default_repository` (from the cluster settings API).
pub fn validate_default_repository(md: &Metadata, value: &str) -> EsResult<()> {
    if value.is_empty() {
        return Ok(());
    }
    validate_repository_name(value)?;
    let Some(r) = md.repositories.get(value) else {
        return Err(EsError::illegal_argument(format!(
            "Repository [{value}] is not registered. Cannot set as default repository. Please register the repository prior to setting as default using PUT /_snapshot/{value}"
        )));
    };
    if r.read_only() && r.ty != "url" {
        return Err(EsError::illegal_argument(format!(
            "Repository [{value}] is marked as read-only. Cannot set a read-only repository as the default repository."
        )));
    }
    Ok(())
}

/// `PUT/POST _snapshot/{repository}`.
pub async fn put_repository(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("repository").unwrap_or_default().to_string();
    let verify = ctx.bool_param("verify", true)?;
    let body = ctx.body_map()?.unwrap_or_default();
    let ty = body.get("type").map(Value::to_es_string);
    let settings = match body.get("settings") {
        None | Some(Value::Null) => Settings::default(),
        Some(v @ Value::Obj(_)) => Settings::from_json(v)?,
        Some(_) => return Err(EsError::new(400, "settings_exception", "Malformed settings section, should include an inner object")),
    };
    let Some(ty) = ty else { return Err(EsError::action_request_validation(&["type is missing".into()])) };
    validate_repository_name(&name)?;
    validate_repository(ctx, &name, &ty, &settings)?;
    let _ = verify;
    let repo = Repository { ty, settings };
    ctx.cluster
        .update(move |st| {
            let default = default_repository(&st.metadata).map(str::to_string);
            if let Some(existing) = st.metadata.repositories.get(&name) {
                if repo.read_only() && !existing.read_only() && default.as_deref() == Some(name.as_str()) {
                    return Err(EsError::illegal_argument(format!(
                        "Repository [{name}] is the default repository. Cannot mark the default repository as read-only. Please change the default repository setting before marking this repository as read-only."
                    )));
                }
            } else if repo.read_only() && default.as_deref() == Some(name.as_str()) {
                return Err(EsError::illegal_argument(format!(
                    "Repository [{name}] is the default repository. Cannot create the default repository as read-only. Please change the default repository setting before creating this repository as read-only."
                )));
            }
            st.metadata.repositories.insert(name, Arc::new(repo));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// Repository names matching the `{repository}` expressions (`_all`, `*`, wildcards,
/// exclusions); explicit names must exist.
fn match_repositories(md: &Metadata, exprs: &[String]) -> EsResult<Vec<String>> {
    if exprs.is_empty() || exprs.iter().any(|e| e == "_all") {
        return Ok(md.repositories.keys().cloned().collect());
    }
    let mut out: Vec<String> = Vec::new();
    for e in exprs {
        if let Some(ex) = e.strip_prefix('-')
            && !out.is_empty()
        {
            out.retain(|n| !glob_match(ex, n));
            continue;
        }
        if e.contains('*') {
            out.extend(md.repositories.keys().filter(|n| glob_match(e, n) && !out.contains(n)).cloned().collect::<Vec<_>>());
        } else if md.repositories.contains_key(e) {
            if !out.contains(e) {
                out.push(e.clone());
            }
        } else {
            return Err(repository_missing(e));
        }
    }
    Ok(out)
}

/// `GET _snapshot[/{repository}]`.
pub fn get_repository(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let exprs = ctx.list_param("repository").unwrap_or_default();
    let names = match_repositories(&st.metadata, &exprs)?;
    let mut out = Map::new();
    for n in names {
        out.insert(n.clone(), repo_value(&st, &n, &st.metadata.repositories[&n]));
    }
    Reply::ok(Value::Obj(out))
}

/// `DELETE _snapshot/{repository}`.
pub async fn delete_repository(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let exprs = ctx.list_param("repository").unwrap_or_default();
    ctx.cluster
        .update(move |st| {
            let names = match_repositories(&st.metadata, &exprs)?;
            if names.is_empty() && !exprs.is_empty() && exprs.iter().all(|e| !e.contains('*') && e != "_all") {
                return Err(repository_missing(&exprs[0]));
            }
            if let Some(d) = default_repository(&st.metadata)
                && names.iter().any(|n| n == d)
            {
                return Err(EsError::illegal_argument(
                    "cannot delete the default repository. Please change the default repository cluster setting repositories.default_repository before deleting this repository.",
                ));
            }
            for n in names {
                st.metadata.repositories.remove(&n);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `POST _snapshot/{repository}/_verify`: every (the one) node can access it.
pub fn verify_repository(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("repository").unwrap_or_default();
    let st = ctx.state();
    if !st.metadata.repositories.contains_key(name) {
        return Err(repository_missing(name));
    }
    let node = &ctx.cluster.node;
    let mut nodes = Map::new();
    nodes.insert(node.id.clone(), obj! { "name" => node.name.clone() });
    Reply::ok(obj! { "nodes" => Value::Obj(nodes) })
}

/// `POST _snapshot/{repository}/_cleanup`: nothing unreferenced is ever left behind.
pub fn cleanup_repository(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("repository").unwrap_or_default();
    let st = ctx.state();
    let Some(r) = st.metadata.repositories.get(name) else { return Err(repository_missing(name)) };
    if r.read_only() {
        return Err(repository_exception(name, "cannot run cleanup on readonly repository"));
    }
    Reply::ok(obj! { "results" => obj! { "deleted_bytes" => 0, "deleted_blobs" => 0 } })
}

/// `SnapshotsService.validate`: snapshot name rules.
fn validate_snapshot_name(repo: &str, snap: &str) -> EsResult<()> {
    let invalid = |why: &str| EsError::new(400, "invalid_snapshot_name_exception", format!("[{repo}:{snap}] Invalid snapshot name [{snap}], {why}"));
    if snap.is_empty() {
        return Err(invalid("cannot be empty"));
    }
    if snap.contains('#') {
        return Err(invalid("must not contain '#'"));
    }
    if snap.starts_with('_') {
        return Err(invalid("must not start with '_'"));
    }
    if snap.to_lowercase() != snap {
        return Err(invalid("must be lowercase"));
    }
    if snap.contains(INVALID_CHARS) {
        return Err(invalid(&format!("must not contain the following characters {INVALID_CHARS_TEXT}")));
    }
    Ok(())
}

fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
        Value::Null => Vec::new(),
        other => other.to_es_string().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
    }
}

/// The global state a snapshot keeps.
fn global_state(md: &Metadata) -> Value {
    obj! {
        "index_templates" => Value::Obj(md.index_templates.iter().map(|(k, t)| (k.clone(), t.to_value())).collect()),
        "component_templates" => Value::Obj(md.component_templates.iter().map(|(k, t)| (k.clone(), t.to_value())).collect()),
        "templates" => Value::Obj(md.templates.iter().map(|(k, t)| (k.clone(), t.to_value(true))).collect()),
        "pipelines" => Value::Obj(md.pipelines.iter().map(|(k, p)| (k.clone(), p.clone())).collect()),
        "scripts" => Value::Obj(md.scripts.iter().map(|(k, s)| (k.clone(), s.to_value())).collect()),
        "persistent" => md.persistent.to_flat_json(),
    }
}

fn date(ms: i64) -> String {
    crate::time::DateFormatter::new("strict_date_optional_time").map(|f| f.format_millis(ms)).unwrap_or_default()
}

/// `SnapshotInfo.toXContentExternal`.
fn snapshot_info(ctx: &Ctx, repo: Option<&str>, s: &Snapshot, verbose: bool, index_names: bool, index_details: bool) -> Value {
    let mut m = Map::new();
    m.insert("snapshot".into(), s.name.clone().into());
    m.insert("uuid".into(), s.uuid.clone().into());
    if let Some(r) = repo {
        m.insert("repository".into(), r.into());
    }
    if verbose {
        m.insert("version_id".into(), crate::version::INDEX_VERSION_ID.parse::<i64>().unwrap_or(0).into());
        m.insert("version".into(), crate::version::INDEX_VERSION_STRING.into());
    }
    if index_names {
        m.insert("indices".into(), Value::from(s.indices.clone()));
    }
    if index_details && verbose && !s.index_meta.is_empty() {
        let mut d = Map::new();
        for (name, meta) in &s.index_meta {
            let shards = meta.get("settings").and_then(|st| st.get("index.number_of_shards")).and_then(|v| v.as_str().and_then(|s| s.parse::<i64>().ok())).unwrap_or(1);
            let mut e = Map::new();
            e.insert("shard_count".into(), shards.into());
            if ctx.human {
                e.insert("size".into(), crate::units::ByteSize(shards * SHARD_BYTES).human().into());
            }
            e.insert("size_in_bytes".into(), (shards * SHARD_BYTES).into());
            e.insert("max_segments_per_shard".into(), 0.into());
            d.insert(name.clone(), Value::Obj(e));
        }
        m.insert("index_details".into(), Value::Obj(d));
    }
    m.insert("data_streams".into(), Value::from(s.data_streams.clone()));
    if verbose {
        m.insert("include_global_state".into(), s.include_global_state.into());
        if let Some(meta) = &s.metadata {
            m.insert("metadata".into(), Value::Obj(meta.clone()));
        }
    }
    m.insert("state".into(), s.state.clone().into());
    if verbose {
        m.insert("start_time".into(), date(s.start_ms).into());
        m.insert("start_time_in_millis".into(), s.start_ms.into());
        m.insert("end_time".into(), date(s.end_ms).into());
        m.insert("end_time_in_millis".into(), s.end_ms.into());
        let dur = (s.end_ms - s.start_ms).max(0);
        if ctx.human {
            m.insert("duration".into(), crate::units::TimeValue::millis(dur).human().into());
        }
        m.insert("duration_in_millis".into(), dur.into());
        m.insert("failures".into(), Value::Arr(Vec::new()));
        m.insert("shards".into(), obj! { "total" => s.total_shards, "failed" => s.total_shards - s.successful_shards, "successful" => s.successful_shards });
        m.insert("feature_states".into(), Value::Arr(s.feature_states.clone()));
    }
    Value::Obj(m)
}

/// The system features a snapshot can include (`feature_states`) and their indices.
fn features(md: &Metadata) -> Vec<(&'static str, Vec<String>)> {
    let synonyms: Vec<String> = md.indices.keys().filter(|n| n.as_str() == super::synonyms::SYSTEM_INDEX).cloned().collect();
    vec![("synonyms", synonyms)]
}

/// `PUT/POST _snapshot/{repository}/{snapshot}`.
pub async fn create(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let repo = ctx.path_param("repository").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot").unwrap_or_default().to_string();
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    let mut indices_expr: Vec<String> = Vec::new();
    let mut ignore_unavailable = false;
    let mut include_global_state = true;
    let mut feature_states: Option<Vec<String>> = None;
    let mut metadata: Option<Map> = None;
    let mut expand: Option<String> = None;
    for (k, v) in &body {
        match k.as_str() {
            "indices" => indices_expr = str_list(v),
            "ignore_unavailable" => ignore_unavailable = crate::units::parse_bool(&v.to_es_string())?,
            "include_global_state" => include_global_state = crate::units::parse_bool(&v.to_es_string())?,
            "feature_states" => feature_states = Some(str_list(v)),
            "metadata" => match v {
                Value::Obj(m) => metadata = Some(m.clone()),
                Value::Null => {}
                _ => return Err(EsError::illegal_argument("malformed metadata, should be an object")),
            },
            "expand_wildcards" => expand = Some(str_list(v).join(",")),
            "partial" | "allow_no_indices" | "index_settings" => {}
            other => return Err(EsError::illegal_argument(format!("Unknown parameter {other}"))),
        }
    }
    let _ = expand;
    validate_snapshot_name(&repo, &snap)?;
    let now = ctx.now_ms;
    let r2 = repo.clone();
    let snapshot = ctx
        .cluster
        .update(move |st| {
            let Some(r) = st.metadata.repositories.get(&r2).cloned() else { return Err(repository_missing(&r2)) };
            if r.read_only() {
                return Err(EsError::illegal_argument(format!("[{r2}] cannot create snapshot in a readonly repository")));
            }
            let key = r.location_key();
            if st.metadata.repository_data.get(&key).is_some_and(|d| d.find(&snap).is_some()) {
                return Err(EsError::new(400, "invalid_snapshot_name_exception", format!(
                    "[{r2}:{snap}] Invalid snapshot name [{snap}], snapshot with the same name already exists"
                )));
            }
            // Indices (hidden ones too; system indices only through feature states).
            let opts = IndicesOptions { ignore_unavailable, allow_no_indices: true, expand_hidden: true, expand_closed: true, include_data_streams: true, ..IndicesOptions::STRICT_EXPAND_OPEN };
            let exprs = if indices_expr.is_empty() { vec!["_all".to_string()] } else { indices_expr.clone() };
            let mut names = resolve::resolve(st, &exprs, &opts, now)?;
            names.retain(|n| !st.metadata.indices[n].system);
            let wanted_features: Vec<&str> = match &feature_states {
                Some(f) if f.iter().any(|x| x == "none") => Vec::new(),
                Some(f) => f.iter().map(String::as_str).collect(),
                None if include_global_state => vec!["synonyms"],
                None => Vec::new(),
            };
            let mut feature_out = Vec::new();
            for (feature, idx) in features(&st.metadata) {
                if wanted_features.contains(&feature) && !idx.is_empty() {
                    names.extend(idx.iter().cloned());
                    feature_out.push(obj! { "feature_name" => feature, "indices" => Value::from(idx) });
                }
            }
            let mut data_streams: Vec<String> = Vec::new();
            for n in &names {
                if let Some(ds) = st.metadata.data_stream_of(n)
                    && !data_streams.contains(&ds.name)
                {
                    data_streams.push(ds.name.clone());
                }
            }
            names.sort();
            names.dedup();
            let shards: i64 = names.iter().map(|n| st.metadata.indices[n].number_of_shards() as i64).sum();
            let snapshot = Snapshot {
                name: snap.clone(),
                uuid: random_uuid(),
                indices: names.clone(),
                data_streams: data_streams.clone(),
                include_global_state,
                metadata,
                state: "SUCCESS".into(),
                start_ms: now,
                end_ms: now,
                total_shards: shards,
                successful_shards: shards,
                feature_states: feature_out,
                index_meta: names.iter().map(|n| (n.clone(), st.metadata.indices[n].to_json())).collect(),
                data_stream_meta: data_streams.iter().filter_map(|d| st.metadata.data_streams.get(d).map(|ds| (d.clone(), ds.to_stored()))).collect(),
                global: include_global_state.then(|| global_state(&st.metadata)),
            };
            let mut data = st.metadata.repository_data.get(&key).map(|d| (**d).clone()).unwrap_or_else(|| RepositoryData { uuid: random_uuid(), ..Default::default() });
            data.generation += 1;
            let snapshot = Arc::new(snapshot);
            data.snapshots.push(snapshot.clone());
            st.metadata.repository_data.insert(key, Arc::new(data));
            Ok(snapshot)
        })
        .await?;
    if !wait {
        return Reply::ok(obj! { "accepted" => true });
    }
    Reply::ok(obj! { "snapshot" => snapshot_info(ctx, Some(&repo), &snapshot, true, true, false) })
}

/// `SnapshotState` names (`state=` filter).
const STATES: &[&str] = &["IN_PROGRESS", "SUCCESS", "FAILED", "PARTIAL", "INCOMPATIBLE"];

/// `GET _snapshot/{repository}/{snapshot}`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let repos = match_repositories(&st.metadata, &ctx.list_param("repository").unwrap_or_default())?;
    let snaps = ctx.list_param("snapshot").unwrap_or_default();
    let verbose = ctx.bool_param("verbose", true)?;
    let ignore_unavailable = ctx.bool_param("ignore_unavailable", false)?;
    let index_names = ctx.bool_param("index_names", true)?;
    let index_details = ctx.bool_param("index_details", false)?;
    let include_repository = ctx.bool_param("include_repository", true)?;
    let states: Option<Vec<String>> = match ctx.list_param("state") {
        None => None,
        Some(list) => {
            let mut out = Vec::new();
            for s in list {
                let up = s.to_uppercase();
                if !STATES.contains(&up.as_str()) {
                    return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.snapshots.SnapshotState.{up}")));
                }
                out.push(up);
            }
            Some(out)
        }
    };
    let sort = ctx.query("sort").unwrap_or("start_time");
    if !matches!(sort, "start_time" | "name" | "duration" | "index_count" | "shard_count" | "failed_shard_count" | "repository") {
        return Err(EsError::illegal_argument(format!("unknown snapshot sort key [{sort}]")));
    }
    let order_desc = match ctx.query("order") {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(o) => return Err(EsError::illegal_argument(format!("unknown order [{o}]"))),
    };
    let size = ctx.int_param("size", -1)?;
    let offset = ctx.int_param("offset", 0)?;
    let mut found: Vec<(String, Arc<Snapshot>)> = Vec::new();
    for r in &repos {
        let data = st.metadata.repository_data.get(&st.metadata.repositories[r].location_key()).cloned().unwrap_or_default();
        let all = snaps.is_empty() || snaps.iter().any(|s| s == "_all" || s == "*");
        for s in &data.snapshots {
            let selected = all
                || snaps.iter().any(|p| (p.contains('*') && glob_match(p, &s.name)) || *p == s.name)
                    && !snaps.iter().any(|p| p.strip_prefix('-').is_some_and(|x| glob_match(x, &s.name)));
            if selected {
                found.push((r.clone(), s.clone()));
            }
        }
        if !all && !ignore_unavailable {
            for p in snaps.iter().filter(|p| !p.contains('*') && !p.starts_with('-') && *p != "_current") {
                if data.find(p).is_none() {
                    return Err(snapshot_missing(r, p));
                }
            }
        }
    }
    if let Some(states) = &states {
        found.retain(|(_, s)| states.contains(&s.state));
    }
    found.sort_by(|(ra, a), (rb, b)| {
        let k = match sort {
            "name" => a.name.cmp(&b.name),
            "duration" => (a.end_ms - a.start_ms).cmp(&(b.end_ms - b.start_ms)),
            "index_count" => a.indices.len().cmp(&b.indices.len()),
            "shard_count" => a.total_shards.cmp(&b.total_shards),
            "failed_shard_count" => (a.total_shards - a.successful_shards).cmp(&(b.total_shards - b.successful_shards)),
            "repository" => ra.cmp(rb),
            _ => a.start_ms.cmp(&b.start_ms),
        };
        k.then_with(|| a.name.cmp(&b.name)).then_with(|| ra.cmp(rb))
    });
    if order_desc {
        found.reverse();
    }
    let total = found.len();
    let skip = (offset.max(0) as usize).min(total);
    let take = if size < 0 { total - skip } else { (size as usize).min(total - skip) };
    let page: Vec<Value> = found[skip..skip + take]
        .iter()
        .map(|(r, s)| snapshot_info(ctx, include_repository.then_some(r.as_str()), s, verbose, index_names, index_details))
        .collect();
    Reply::ok(obj! { "snapshots" => Value::Arr(page), "total" => total, "remaining" => total - skip - take })
}

/// `DELETE _snapshot/{repository}/{snapshot}`.
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    ctx.bool_param("wait_for_completion", true)?;
    let repo = ctx.path_param("repository").unwrap_or_default().to_string();
    let snaps = ctx.list_param("snapshot").unwrap_or_default();
    ctx.cluster
        .update(move |st| {
            let Some(r) = st.metadata.repositories.get(&repo).cloned() else { return Err(repository_missing(&repo)) };
            let key = r.location_key();
            let mut data = st.metadata.repository_data.get(&key).map(|d| (**d).clone()).unwrap_or_default();
            for p in snaps.iter().filter(|p| !p.contains('*') && *p != "_all") {
                if data.find(p).is_none() {
                    return Err(snapshot_missing(&repo, p));
                }
            }
            // Nothing to delete is done before the repository is even written to.
            if !data.snapshots.iter().any(|s| snaps.iter().any(|p| p == "_all" || glob_match(p, &s.name))) {
                return Ok(());
            }
            if r.read_only() {
                return Err(repository_exception(&repo, "repository is readonly"));
            }
            data.snapshots.retain(|s| !snaps.iter().any(|p| p == "_all" || glob_match(p, &s.name)));
            data.generation += 1;
            if !data.uuid.is_empty() {
                st.metadata.repository_data.insert(key, Arc::new(data));
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

fn file_stats(shards: i64) -> Value {
    obj! { "file_count" => shards, "size_in_bytes" => shards * SHARD_BYTES }
}

fn status_stats(shards: i64, start: i64, time: i64) -> Value {
    obj! { "incremental" => file_stats(shards), "total" => file_stats(shards), "start_time_in_millis" => start, "time_in_millis" => time }
}

fn shards_stats(n: i64) -> Value {
    obj! { "initializing" => 0, "started" => 0, "finalizing" => 0, "done" => n, "failed" => 0, "total" => n }
}

/// `GET _snapshot[/{repository}[/{snapshot}]]/_status`: no snapshot is ever running, so
/// without a snapshot name the list is empty.
pub fn status(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let Some(repo) = ctx.path_param("repository") else { return Reply::ok(obj! { "snapshots" => Value::Arr(Vec::new()) }) };
    let Some(r) = st.metadata.repositories.get(repo) else { return Err(repository_missing(repo)) };
    let Some(snaps) = ctx.list_param("snapshot") else { return Reply::ok(obj! { "snapshots" => Value::Arr(Vec::new()) }) };
    let ignore_unavailable = ctx.bool_param("ignore_unavailable", false)?;
    let data = st.metadata.repository_data.get(&r.location_key()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for name in &snaps {
        let Some(s) = data.find(name) else {
            if ignore_unavailable {
                continue;
            }
            return Err(snapshot_missing(repo, name));
        };
        let time = (s.end_ms - s.start_ms).max(0);
        let mut indices = Map::new();
        for (idx, meta) in &s.index_meta {
            let n = meta.get("settings").and_then(|st| st.get("index.number_of_shards")).and_then(|v| v.as_str()).and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
            let mut shards = Map::new();
            for i in 0..n {
                shards.insert(i.to_string(), obj! { "stage" => "DONE", "stats" => status_stats(1, s.start_ms, time) });
            }
            indices.insert(idx.clone(), obj! { "shards_stats" => shards_stats(n), "stats" => status_stats(n, s.start_ms, time), "shards" => Value::Obj(shards) });
        }
        out.push(obj! {
            "snapshot" => s.name.clone(), "repository" => repo, "uuid" => s.uuid.clone(), "state" => s.state.clone(),
            "include_global_state" => s.include_global_state,
            "shards_stats" => shards_stats(s.total_shards),
            "stats" => status_stats(s.total_shards, s.start_ms, time),
            "indices" => Value::Obj(indices),
        });
    }
    Reply::ok(obj! { "snapshots" => Value::Arr(out) })
}

/// `PUT _snapshot/{repository}/{snapshot}/_clone/{target_snapshot}`.
pub async fn clone(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let repo = ctx.path_param("repository").unwrap_or_default().to_string();
    let source = ctx.path_param("snapshot").unwrap_or_default().to_string();
    let target = ctx.path_param("target_snapshot").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let patterns = body.get("indices").map(str_list).unwrap_or_default();
    if patterns.is_empty() {
        return Err(EsError::action_request_validation(&["indices patterns are empty".into()]));
    }
    validate_snapshot_name(&repo, &target)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(r) = st.metadata.repositories.get(&repo).cloned() else { return Err(repository_missing(&repo)) };
            let key = r.location_key();
            let mut data = st.metadata.repository_data.get(&key).map(|d| (**d).clone()).unwrap_or_default();
            let Some(src) = data.find(&source).cloned() else { return Err(snapshot_missing(&repo, &source)) };
            if data.find(&target).is_some() {
                return Err(EsError::new(400, "invalid_snapshot_name_exception", format!(
                    "[{repo}:{target}] Invalid snapshot name [{target}], snapshot with the same name already exists"
                )));
            }
            let indices: Vec<String> = src.indices.iter().filter(|i| patterns.iter().any(|p| glob_match(p, i))).cloned().collect();
            let shards: i64 = indices
                .iter()
                .filter_map(|i| src.index_meta.get(i))
                .map(|m| m.get("settings").and_then(|s| s.get("index.number_of_shards")).and_then(|v| v.as_str()).and_then(|v| v.parse::<i64>().ok()).unwrap_or(1))
                .sum();
            let cloned = Snapshot {
                name: target.clone(),
                uuid: random_uuid(),
                index_meta: src.index_meta.iter().filter(|(k, _)| indices.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect(),
                indices,
                start_ms: now,
                end_ms: now,
                total_shards: shards,
                successful_shards: shards,
                ..(*src).clone()
            };
            data.snapshots.push(Arc::new(cloned));
            data.generation += 1;
            st.metadata.repository_data.insert(key, Arc::new(data));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `POST _snapshot/{repository}/{snapshot}/_restore`.
pub async fn restore(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let repo = ctx.path_param("repository").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot").unwrap_or_default().to_string();
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    let patterns = body.get("indices").map(str_list).unwrap_or_default();
    let ignore_unavailable = body.get("ignore_unavailable").map(|v| crate::units::parse_bool(&v.to_es_string())).transpose()?.unwrap_or(false);
    let include_global_state = body.get("include_global_state").map(|v| crate::units::parse_bool(&v.to_es_string())).transpose()?.unwrap_or(false);
    let include_aliases = body.get("include_aliases").map(|v| crate::units::parse_bool(&v.to_es_string())).transpose()?.unwrap_or(true);
    let rename_pattern = body.get("rename_pattern").map(Value::to_es_string);
    let rename_replacement = body.get("rename_replacement").map(Value::to_es_string);
    let index_settings = match body.get("index_settings") {
        Some(v @ Value::Obj(_)) => {
            let mut s = Settings::from_json(v)?;
            s.normalize_prefix("index.");
            s
        }
        _ => Settings::default(),
    };
    let ignore_settings: Vec<String> = body
        .get("ignore_index_settings")
        .map(str_list)
        .unwrap_or_default()
        .into_iter()
        .map(|k| if k.starts_with("index.") { k } else { format!("index.{k}") })
        .collect();
    for k in index_settings.keys().chain(ignore_settings.iter()) {
        if matches!(k.as_str(), "index.number_of_shards" | "index.uuid" | "index.creation_date" | "index.version.created") {
            return Err(EsError::new(500, "snapshot_restore_exception", format!("[{repo}:{snap}] cannot modify setting [{k}] on restore")));
        }
    }
    let rename = match (&rename_pattern, &rename_replacement) {
        (Some(p), r) => Some((regex::Regex::new(p).map_err(|e| EsError::illegal_argument(e.to_string()))?, r.clone().unwrap_or_default())),
        _ => None,
    };
    let now = ctx.now_ms;
    let (restored, shards) = ctx
        .cluster
        .update(move |st| {
            let Some(r) = st.metadata.repositories.get(&repo).cloned() else { return Err(repository_missing(&repo)) };
            let data = st.metadata.repository_data.get(&r.location_key()).cloned().unwrap_or_default();
            let Some(s) = data.find(&snap).cloned() else { return Err(snapshot_missing(&repo, &snap)) };
            // Which indices: patterns over the snapshot's (data streams restore their backing indices).
            let mut selected: Vec<String> = Vec::new();
            if patterns.is_empty() || patterns.iter().any(|p| p == "_all" || p == "*") {
                selected = s.indices.iter().filter(|i| s.index_meta.get(*i).and_then(|m| m.get("system")).and_then(Value::as_bool) != Some(true)).cloned().collect();
            } else {
                for p in &patterns {
                    if let Some(ex) = p.strip_prefix('-') {
                        selected.retain(|i| !glob_match(ex, i));
                        continue;
                    }
                    let ds_backing: Vec<String> = s
                        .data_stream_meta
                        .get(p)
                        .and_then(|d| d.get("indices"))
                        .and_then(Value::as_array)
                        .map(|a| a.iter().map(Value::to_es_string).collect())
                        .unwrap_or_default();
                    let hits: Vec<String> = s.indices.iter().filter(|i| glob_match(p, i) || ds_backing.contains(i)).cloned().collect();
                    if hits.is_empty() && !p.contains('*') && !ignore_unavailable {
                        return Err(EsError::new(500, "snapshot_restore_exception", format!(
                            "[{repo}:{snap}/{}] index [{p}] not found",
                            s.uuid
                        )));
                    }
                    for h in hits {
                        if !selected.contains(&h) {
                            selected.push(h);
                        }
                    }
                }
            }
            let mut restored = Vec::new();
            let mut total_shards = 0i64;
            for name in &selected {
                let Some(stored) = s.index_meta.get(name) else { continue };
                let target = match &rename {
                    Some((re, rep)) => re.replace_all(name, rep.as_str()).into_owned(),
                    None => name.clone(),
                };
                let mut idx = IndexMeta::from_json_in(&target, stored, st.dialect)?;
                if let Some(existing) = st.metadata.indices.get(&target) {
                    if !existing.is_closed() {
                        return Err(EsError::new(500, "snapshot_restore_exception", format!(
                            "[{repo}:{snap}/{}] cannot restore index [{target}] because an open index with same name already exists in the cluster. Either close or delete the existing index or restore the index under a different name by providing a rename pattern and replacement name",
                            s.uuid
                        )));
                    }
                    if existing.number_of_shards() != idx.number_of_shards() {
                        return Err(EsError::new(500, "snapshot_restore_exception", format!(
                            "[{repo}:{snap}/{}] cannot restore index [{target}] with [{}] shards from a snapshot of index [{name}] with [{}] shards",
                            s.uuid,
                            existing.number_of_shards(),
                            idx.number_of_shards()
                        )));
                    }
                }
                let uuid = random_uuid();
                idx.name = target.clone();
                idx.uuid = uuid.clone();
                idx.state = IndexState::Open;
                idx.settings.set("index.uuid", uuid);
                idx.settings.remove("index.verified_before_close");
                for k in &ignore_settings {
                    idx.settings.remove(k);
                }
                idx.settings.merge(&index_settings);
                if !include_aliases {
                    idx.aliases.clear();
                }
                idx.version += 1;
                let mut rs = Map::new();
                rs.insert("repository".into(), repo.clone().into());
                rs.insert("snapshot".into(), snap.clone().into());
                rs.insert("version".into(), crate::version::ES_VERSION.into());
                rs.insert("index".into(), name.clone().into());
                rs.insert("restoreUUID".into(), random_uuid().into());
                idx.recovery_source = Some(Arc::new(rs));
                total_shards += idx.number_of_shards() as i64;
                st.metadata.indices.insert(target.clone(), Arc::new(idx));
                restored.push(target);
            }
            // Data streams whose backing indices all came back (under their own names).
            for (ds_name, stored) in &s.data_stream_meta {
                if rename.is_none()
                    && let Ok(ds) = crate::meta::data_streams::DataStream::from_stored(ds_name, stored)
                    && !ds.indices.is_empty()
                    && ds.indices.iter().all(|i| restored.contains(i))
                {
                    st.metadata.data_streams.insert(ds_name.clone(), Arc::new(ds));
                }
            }
            if include_global_state && let Some(g) = &s.global {
                restore_global(&mut st.metadata, g)?;
            }
            st.check_unique_names()?;
            Ok((restored, total_shards))
        })
        .await?;
    let _ = now;
    if !wait {
        return Reply::ok(obj! { "accepted" => true });
    }
    let snap = ctx.path_param("snapshot").unwrap_or_default().to_string();
    Reply::ok(obj! {
        "snapshot" => obj! {
            "snapshot" => snap,
            "indices" => Value::from(restored),
            "shards" => obj! { "total" => shards, "failed" => 0, "successful" => shards },
        }
    })
}

/// Restores the global state of a snapshot over the cluster's.
fn restore_global(md: &mut Metadata, g: &Value) -> EsResult<()> {
    use crate::meta::templates::{ComponentTemplate, ComposableTemplate, LegacyTemplate};
    if let Some(Value::Obj(m)) = g.get("component_templates") {
        for (k, v) in m {
            md.component_templates.insert(k.clone(), Arc::new(ComponentTemplate::from_stored(v)?));
        }
    }
    if let Some(Value::Obj(m)) = g.get("index_templates") {
        for (k, v) in m {
            md.index_templates.insert(k.clone(), Arc::new(ComposableTemplate::from_stored(v)?));
        }
    }
    if let Some(Value::Obj(m)) = g.get("templates") {
        for (k, v) in m {
            md.templates.insert(k.clone(), Arc::new(LegacyTemplate::from_stored(v)?));
        }
    }
    if let Some(Value::Obj(m)) = g.get("pipelines") {
        for (k, v) in m {
            md.pipelines.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Obj(m)) = g.get("scripts") {
        for (k, v) in m {
            md.scripts.insert(
                k.clone(),
                crate::meta::StoredScript {
                    lang: v.get("lang").map(Value::to_es_string).unwrap_or_else(|| "painless".into()),
                    source: v.get("source").cloned().unwrap_or(Value::Null),
                    options: v.get("options").and_then(Value::as_object).cloned(),
                },
            );
        }
    }
    if let Some(p) = g.get("persistent") {
        md.persistent.merge(&Settings::from_json(p)?);
    }
    Ok(())
}

/// Rows of `_cat/repositories`: (name, type), sorted by name.
pub fn repository_rows(st: &ClusterState) -> Vec<(String, String)> {
    st.metadata.repositories.iter().map(|(k, r)| (k.clone(), r.ty.clone())).collect()
}

/// Snapshots of the `_cat/snapshots` repositories, oldest first.
pub fn snapshot_rows(st: &ClusterState, repos: &[String]) -> EsResult<Vec<(String, Arc<Snapshot>)>> {
    let names = match_repositories(&st.metadata, repos)?;
    let mut out = Vec::new();
    for r in names {
        if let Some(d) = st.metadata.repository_data.get(&st.metadata.repositories[&r].location_key()) {
            out.extend(d.snapshots.iter().map(|s| (r.clone(), s.clone())));
        }
    }
    out.sort_by(|(ra, a), (rb, b)| a.start_ms.cmp(&b.start_ms).then_with(|| a.name.cmp(&b.name)).then_with(|| ra.cmp(rb)));
    Ok(out)
}

/// `_cluster/stats` `snapshots.repositories`: nothing is ever in progress.
pub fn stats_repositories(st: &ClusterState, now_ms: i64) -> Value {
    let mut out = Map::new();
    for (name, r) in &st.metadata.repositories {
        let states: Map = ["INIT", "SUCCESS", "FAILED", "ABORTED", "MISSING", "WAITING", "QUEUED", "PAUSED_FOR_NODE_REMOVAL"]
            .iter()
            .map(|s| (s.to_string(), Value::from(0)))
            .collect();
        out.insert(
            name.clone(),
            obj! {
                "type" => r.ty.clone(),
                "current_counts" => obj! {
                    "snapshots" => 0, "clones" => 0, "finalizations" => 0, "deletions" => 0, "snapshot_deletions" => 0, "active_deletions" => 0,
                    "shards" => obj! { "total" => 0, "complete" => 0, "incomplete" => 0, "states" => Value::Obj(states) },
                },
                "oldest_start_time" => date(now_ms),
                "oldest_start_time_millis" => now_ms,
            },
        );
    }
    Value::Obj(out)
}

/// `_cluster/stats` `repositories`: counts by type.
pub fn repository_usage(st: &ClusterState) -> Value {
    let mut by_type: std::collections::BTreeMap<&str, (i64, i64)> = Default::default();
    for r in st.metadata.repositories.values() {
        let e = by_type.entry(r.ty.as_str()).or_default();
        if r.read_only() {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    let mut out = Map::new();
    for (ty, (ro, rw)) in by_type {
        let mut m = Map::new();
        m.insert("count".into(), (ro + rw).into());
        if ro > 0 {
            m.insert("read_only".into(), ro.into());
        }
        if rw > 0 {
            m.insert("read_write".into(), rw.into());
        }
        out.insert(ty.to_string(), Value::Obj(m));
    }
    Value::Obj(out)
}

/// Repository metadata for the cluster state (`metadata.repositories`).
pub fn state_repositories(st: &ClusterState) -> Value {
    let mut out = Map::new();
    for (name, r) in &st.metadata.repositories {
        let data = st.metadata.repository_data.get(&r.location_key());
        let mut m = Map::new();
        m.insert("type".into(), r.ty.clone().into());
        m.insert("uuid".into(), data.map(|d| d.uuid.clone()).unwrap_or_else(|| "_na_".into()).into());
        m.insert("settings".into(), r.settings.to_json(false));
        let generation = data.map(|d| d.generation).unwrap_or(-1);
        m.insert("generation".into(), generation.into());
        m.insert("pending_generation".into(), generation.into());
        out.insert(name.clone(), Value::Obj(m));
    }
    Value::Obj(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_paths() {
        let paths = vec!["/repo".to_string()];
        assert_eq!(resolve_location(&paths, "loc").as_deref(), Some("/repo/loc"));
        assert_eq!(resolve_location(&paths, "/repo/x/../y").as_deref(), Some("/repo/y"));
        assert_eq!(resolve_location(&paths, "/other"), None);
        assert_eq!(resolve_location(&paths, "../escape"), None);
        assert_eq!(resolve_location(&[], "loc"), None);
    }

    #[test]
    fn names() {
        assert!(validate_repository_name("ok").is_ok());
        assert_eq!(validate_repository_name("a#b").unwrap_err().reason(), "[a#b] must not contain '#'");
        assert!(validate_repository_name("a b").is_err());
        assert!(validate_snapshot_name("r", "Upper").is_err());
        assert!(validate_snapshot_name("r", "_x").is_err());
        assert!(validate_snapshot_name("r", "fine-1").is_ok());
    }
}
