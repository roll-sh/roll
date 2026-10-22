//! Data stream APIs: create, get, delete, stats, migrate, promote, modify, and the lifecycle
//! APIs (`_lifecycle`, `_lifecycle/explain`, `_lifecycle/stats`), data stream options
//! (`_options`) and data-stream-level settings and mappings.
//!
//! The model lives in [`crate::meta::data_streams`]; backing indices are created through the
//! regular index creation path ([`super::indices::create_backing_index`]) so they get exactly the
//! settings, mappings and validation a template gives any index. Data stream names in requests
//! are resolved as `GET _data_stream` resolves them: wildcards over data stream names
//! (hidden ones only with `expand_wildcards=hidden` or a leading dot), `-` exclusions, and
//! concrete names that must exist (aliases are rejected with Elasticsearch's text).

use super::indices::{BackingOf, create_backing_index};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::data_streams::{self, BACKING_INDEX_PREFIX, DataStream, DataStreamView, backing_index_name, lifecycle_value};
use crate::meta::resolve::{IndicesOptions, is_wildcard, split_expressions};
use crate::meta::settings::Settings;
use crate::meta::templates::{self, ComposableTemplate, find_composable};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::{ByteSize, TimeValue};
use crate::xcontent::filter::glob_match;
use std::collections::BTreeSet;
use std::sync::Arc;

fn names(ctx: &Ctx) -> Vec<String> {
    ctx.param("name").map(split_expressions).unwrap_or_default()
}

/// Resolves data stream expressions (`IndexNameExpressionResolver.dataStreamNames`), sorted.
pub fn resolve_data_streams(st: &ClusterState, exprs: &[String], opts: &IndicesOptions) -> EsResult<Vec<String>> {
    let md = &st.metadata;
    // Data streams are always open: any expansion other than `none` reaches them.
    let visible = |ds: &DataStream, dot: bool| (!ds.hidden || opts.expand_hidden || dot) && (opts.allow_wildcards || opts.expand_hidden);
    let mut out: BTreeSet<String> = BTreeSet::new();
    if exprs.is_empty() || (exprs.len() == 1 && (exprs[0] == "*" || exprs[0] == "_all")) {
        for (n, ds) in &md.data_streams {
            if visible(ds, false) {
                out.insert(n.clone());
            }
        }
        return Ok(out.into_iter().collect());
    }
    for e in exprs {
        let (exclude, base) = match e.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, e.as_str()),
        };
        if is_wildcard(base) {
            let dot = base.starts_with('.');
            let matched: Vec<String> = md.data_streams.iter().filter(|(n, ds)| glob_match(base, n) && visible(ds, dot)).map(|(n, _)| n.clone()).collect();
            for m in matched {
                if exclude {
                    out.remove(&m);
                } else {
                    out.insert(m);
                }
            }
            continue;
        }
        if exclude {
            out.remove(base);
            continue;
        }
        if md.data_streams.contains_key(base) {
            out.insert(base.to_string());
        } else if md.indices.contains_key(base) {
            // A concrete index (a backing index included) names no data stream.
        } else if st.has_alias(base) || md.data_stream_aliases.contains_key(base) {
            return Err(EsError::illegal_argument(format!(
                "The provided expression [{base}] matches an alias, specify the corresponding concrete indices instead."
            )));
        } else if !opts.ignore_unavailable {
            return Err(EsError::index_not_found(base));
        }
    }
    Ok(out.into_iter().collect())
}

/// The composable template a data stream matches now (`template` in `GET _data_stream`).
fn current_template<'a>(st: &'a ClusterState, ds: &DataStream) -> Option<(&'a String, &'a ComposableTemplate)> {
    find_composable(&st.metadata, &ds.name, ds.hidden)
}

/// The lifecycle a template composes to (component templates first, then its own).
fn template_lifecycle(st: &ClusterState, t: &ComposableTemplate) -> Option<Value> {
    crate::meta::templates::composed_lifecycle(&st.metadata, t)
}

/// The data stream options a template composes to.
fn template_options(st: &ClusterState, t: &ComposableTemplate) -> Option<Value> {
    crate::meta::templates::composed_options(&st.metadata, t).and_then(|o| crate::meta::templates::effective_options(&o))
}

/// Creates the data stream `name` from its matching template (`MetadataCreateDataStreamService`).
pub fn create_data_stream(st: &mut ClusterState, name: &str, now_ms: i64) -> EsResult<Vec<String>> {
    if st.metadata.data_streams.contains_key(name) {
        return Err(EsError::resource_already_exists(format!("data_stream [{name}] already exists")));
    }
    data_streams::validate_name(name)?;
    let (tname, template) = match find_composable(&st.metadata, name, false) {
        Some((n, t)) => (n.clone(), t.clone()),
        None => return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{name}]"))),
    };
    let Some(dst) = template.data_stream.clone() else {
        return Err(EsError::illegal_argument(format!("matching index template [{tname}] for data stream [{name}] has no data stream template")));
    };
    let index = backing_index_name(BACKING_INDEX_PREFIX, name, 1, now_ms);
    if st.metadata.indices.contains_key(&index) {
        return Err(EsError::status(400, format!("data stream could not be created because backing index [{index}] already exists")));
    }
    let empty = Settings::new();
    let warnings = create_backing_index(st, &index, BackingOf { data_stream: name, template: &template, settings: &empty, mappings: None }, now_ms)?;
    let index_mode = st.metadata.indices.get(&index).and_then(|i| i.settings.get("index.mode")).map(str::to_string);
    let ds = DataStream {
        name: name.to_string(),
        indices: vec![index],
        generation: 1,
        meta: template.meta.clone(),
        hidden: dst.get("hidden").and_then(Value::as_bool).unwrap_or(false),
        allow_custom_routing: dst.get("allow_custom_routing").and_then(Value::as_bool).unwrap_or(false),
        index_mode,
        lifecycle: template_lifecycle(st, &template),
        options: template_options(st, &template),
        // The failure store is created lazily, on the first failed write.
        failure_rollover_on_write: true,
        ..Default::default()
    };
    st.metadata.data_streams.insert(name.to_string(), Arc::new(ds));
    // Template aliases become data stream aliases.
    let composed = templates::compose(&st.metadata, &template)?;
    for (alias, a) in composed.aliases {
        let entry = st.metadata.data_stream_aliases.entry(alias).or_default();
        if !entry.data_streams.iter().any(|d| d == name) {
            entry.data_streams.push(name.to_string());
            entry.data_streams.sort();
        }
        if a.is_write_index == Some(true) {
            entry.write_data_stream = Some(name.to_string());
        }
        if let Some(f) = a.filter {
            entry.filters.insert(name.to_string(), f);
        }
    }
    st.check_unique_names()?;
    Ok(warnings)
}

pub async fn create(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    let warnings = ctx.cluster.update(move |st| create_data_stream(st, &name, now)).await?;
    ctx.warn_all(warnings);
    Reply::acknowledged()
}

fn dsl_enabled(ds: &DataStream) -> bool {
    ds.lifecycle.as_ref().is_some_and(|l| l.get("enabled").and_then(Value::as_bool).unwrap_or(true))
}

/// Whether the failure store of `ds` is effectively enabled: its options, else the
/// `data_streams.failure_store.enabled` cluster setting's name patterns.
fn failure_store_enabled(st: &ClusterState, ds: &DataStream) -> bool {
    if let Some(e) = ds.failure_store_enabled() {
        return e;
    }
    let md = &st.metadata;
    let patterns = md.transient.get_list("data_streams.failure_store.enabled").or_else(|| md.persistent.get_list("data_streams.failure_store.enabled"));
    !ds.system && patterns.is_some_and(|p| p.iter().any(|p| glob_match(p, &ds.name)))
}

fn time_setting(st: &ClusterState, key: &str) -> Option<TimeValue> {
    let md = &st.metadata;
    md.transient.get(key).or_else(|| md.persistent.get(key)).and_then(|v| TimeValue::parse(v, key).ok())
}

/// `DataStreamLifecycle.getEffectiveDataRetentionWithSource`: the retention in effect and what
/// determined it.
fn effective_retention(st: &ClusterState, ds: &DataStream, lifecycle: &Value) -> Option<(TimeValue, &'static str)> {
    let configured = lifecycle.get("data_retention").and_then(Value::as_str).and_then(|v| TimeValue::parse(v, "data_retention").ok());
    let internal = ds.system || ds.name.starts_with('.');
    if internal {
        return configured.map(|r| (r, "data_stream_configuration"));
    }
    let max = time_setting(st, "data_streams.lifecycle.retention.max");
    let default = time_setting(st, "data_streams.lifecycle.retention.default");
    match configured {
        Some(r) => match max {
            Some(m) if m.as_millis() < r.as_millis() => Some((m, "max_global_retention")),
            _ => Some((r, "data_stream_configuration")),
        },
        None => match (default, max) {
            (Some(d), _) => Some((d, "default_global_retention")),
            (None, Some(m)) => Some((m, "max_global_retention")),
            _ => None,
        },
    }
}

/// `RolloverConfiguration.evaluateMaxAgeCondition`.
fn automatic_max_age(retention: Option<&TimeValue>) -> &'static str {
    let day = 24 * 3600 * 1000;
    match retention.map(|r| r.as_millis()) {
        None => "30d",
        Some(ms) if ms <= day => "1h",
        Some(ms) if ms <= 14 * day => "1d",
        Some(ms) if ms <= 90 * day => "7d",
        Some(_) => "30d",
    }
}

/// The retention a template's lifecycle would give a (non-internal) data stream, with the
/// global retention settings applied.
fn template_effective_retention(st: &ClusterState, lifecycle: &Value) -> Option<TimeValue> {
    let configured = lifecycle.get("data_retention").and_then(Value::as_str).and_then(|v| TimeValue::parse(v, "data_retention").ok());
    let max = time_setting(st, "data_streams.lifecycle.retention.max");
    let default = time_setting(st, "data_streams.lifecycle.retention.default");
    match configured {
        Some(r) => match max {
            Some(m) if m.as_millis() < r.as_millis() => Some(m),
            _ => Some(r),
        },
        None => default.or(max),
    }
}

/// With `include_defaults`, template APIs print a lifecycle's rollover conditions (the
/// `data_streams.lifecycle.rollover` defaults, `max_age` derived from the retention).
pub fn add_rollover_defaults(st: &ClusterState, lifecycle: &mut Value) {
    let eff = template_effective_retention(st, lifecycle);
    if let Value::Obj(m) = lifecycle {
        m.insert(
            "rollover".into(),
            obj! {
                "min_docs" => 1,
                "max_primary_shard_docs" => 200_000_000,
                "max_primary_shard_size" => "50gb",
                "max_age" => format!("{} [automatic]", automatic_max_age(eff.as_ref())),
            },
        );
    }
}

/// A data lifecycle as the data stream APIs print it: with its effective retention and, with
/// `include_defaults`, the rollover configuration.
fn lifecycle_with_effective(st: &ClusterState, ds: &DataStream, l: &Value, include_defaults: bool) -> Value {
    let mut v = lifecycle_value(l);
    let eff = effective_retention(st, ds, l);
    if let Value::Obj(m) = &mut v {
        let mut out = Map::new();
        for (k, val) in std::mem::take(m) {
            let after_retention = k == "data_retention";
            out.insert(k, val);
            if after_retention && let Some((r, src)) = &eff {
                out.insert("effective_retention".into(), Value::from(r.string_rep()));
                out.insert("retention_determined_by".into(), Value::from(*src));
            }
        }
        if !out.contains_key("effective_retention")
            && let Some((r, src)) = &eff
        {
            // No configured retention: the effective one follows `enabled`.
            let mut ordered = Map::new();
            for (k, val) in out {
                let is_enabled = k == "enabled";
                ordered.insert(k, val);
                if is_enabled {
                    ordered.insert("effective_retention".into(), Value::from(r.string_rep()));
                    ordered.insert("retention_determined_by".into(), Value::from(*src));
                }
            }
            out = ordered;
        }
        if include_defaults {
            out.insert(
                "rollover".into(),
                obj! {
                    "min_docs" => 1,
                    "max_primary_shard_docs" => 200_000_000,
                    "max_primary_shard_size" => "50gb",
                    "max_age" => format!("{} [automatic]", automatic_max_age(eff.as_ref().map(|(r, _)| r))),
                },
            );
        }
        *m = out;
    }
    v
}

/// The failures lifecycle of a failure store: its configured retention (capped by the max
/// global retention) or the default failures retention, and with `include_defaults` the
/// rollover conditions.
fn failures_lifecycle(st: &ClusterState, ds: &DataStream, include_defaults: bool) -> Value {
    let configured = ds.options.as_ref().and_then(|o| o.get("failure_store")).and_then(|f| f.get("lifecycle")).filter(|l| !l.is_null()).cloned();
    let l = configured.unwrap_or_else(|| obj! { "enabled" => true });
    let v = lifecycle_value(&l);
    let retention = l.get("data_retention").and_then(Value::as_str).and_then(|r| TimeValue::parse(r, "data_retention").ok());
    let internal = ds.system || ds.name.starts_with('.');
    let max = if internal { None } else { time_setting(st, "data_streams.lifecycle.retention.max") };
    let eff = match retention {
        Some(r) => match max {
            Some(m) if m.as_millis() < r.as_millis() => Some((m, "max_global_retention")),
            _ => Some((r, "data_stream_configuration")),
        },
        None => {
            let default = time_setting(st, "data_streams.lifecycle.retention.failures_default").unwrap_or_else(|| TimeValue::parse("30d", "").expect("valid"));
            Some((default, "default_failures_retention"))
        }
    };
    let Value::Obj(m) = v else { return v };
    let mut out = Map::new();
    let has_retention = m.contains_key("data_retention");
    for (k, val) in m {
        let after = if has_retention { k == "data_retention" } else { k == "enabled" };
        out.insert(k, val);
        if after && let Some((r, src)) = &eff {
            out.insert("effective_retention".into(), Value::from(r.string_rep()));
            out.insert("retention_determined_by".into(), Value::from(*src));
        }
    }
    if include_defaults {
        out.insert(
            "rollover".into(),
            obj! {
                "min_docs" => 1,
                "max_primary_shard_docs" => 200_000_000,
                "max_primary_shard_size" => "50gb",
                "max_age" => format!("{} [automatic]", automatic_max_age(eff.as_ref().map(|(r, _)| r))),
            },
        );
    }
    Value::Obj(out)
}

fn data_stream_value(ctx: &Ctx, st: &ClusterState, ds: &DataStream, include_defaults: bool, verbose: bool) -> Value {
    let template = current_template(st, ds);
    let composed = template.and_then(|(_, t)| templates::compose(&st.metadata, t).ok());
    let ilm_policy = composed.as_ref().and_then(|c| c.settings.get("index.lifecycle.name")).filter(|p| !p.is_empty()).map(str::to_string);
    let prefer_ilm = composed.as_ref().and_then(|c| c.settings.get("index.lifecycle.prefer_ilm")).map(|v| v != "false").unwrap_or(true);
    let fs_enabled = failure_store_enabled(st, ds);
    // `index_mode` is the data stream's own (set when backing indices are created) or else the
    // one the current template gives the next backing index.
    let index_mode = ds.index_mode.clone().or_else(|| composed.as_ref().map(|c| {
        let mut s = c.settings.clone();
        s.merge(&ds.settings);
        // Only the mode providers decide is needed here, not the time bounds.
        if !s.contains("index.mode") && crate::xcontent::filter::glob_match("logs-*-*", &ds.name) {
            let md = &st.metadata;
            let enabled = md.transient.get("cluster.logsdb.enabled").or_else(|| md.persistent.get("cluster.logsdb.enabled")).is_none_or(|v| v == "true");
            if enabled {
                s.set("index.mode", "logsdb");
            }
        }
        crate::meta::index_mode::IndexMode::of(&s).name().to_string()
    }));
    let view = DataStreamView { ds, template: template.map(|(n, _)| n.clone()), ilm_policy, prefer_ilm, failure_store_enabled: fs_enabled, verbose, index_mode };
    let mut v = view.to_value(st);
    if let Value::Obj(m) = &mut v {
        if let Some(l) = &ds.lifecycle {
            m.insert("lifecycle".into(), lifecycle_with_effective(st, ds, l, include_defaults));
        }
        // The failures lifecycle shows once the failure store is enabled or has indices.
        if (fs_enabled || !ds.failure_indices.is_empty())
            && let Some(Value::Obj(fs)) = m.get_mut("failure_store")
        {
            fs.insert("lifecycle".into(), failures_lifecycle(st, ds, include_defaults));
        }
        if ctx.flat_settings().unwrap_or(false) {
            m.insert("settings".into(), ds.settings.to_flat_json());
        }
    }
    v
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let verbose = ctx.bool_param("verbose", false)?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &opts)?;
    let list: Vec<Value> = names.iter().filter_map(|n| st.metadata.data_streams.get(n)).map(|ds| data_stream_value(ctx, &st, ds, include_defaults, verbose)).collect();
    Reply::ok(obj! { "data_streams" => Value::Arr(list) })
}

/// Removes data streams with their backing and failure indices (and their data stream aliases).
pub fn delete_data_streams(st: &mut ClusterState, names: &[String]) -> EsResult<()> {
    for n in names {
        let Some(ds) = st.metadata.data_streams.remove(n) else {
            return Err(EsError::resource_not_found(format!("data stream [{n}] not found")));
        };
        for i in ds.indices.iter().chain(&ds.failure_indices) {
            st.metadata.indices.remove(i);
        }
        st.metadata.data_stream_aliases.retain(|_, a| {
            a.data_streams.retain(|d| d != n);
            a.filters.remove(n);
            if a.write_data_stream.as_deref() == Some(n.as_str()) {
                a.write_data_stream = None;
            }
            !a.data_streams.is_empty()
        });
    }
    Ok(())
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let exprs = names(ctx);
    ctx.cluster
        .update(move |st| {
            let names = resolve_data_streams(st, &exprs, &opts)?;
            if names.is_empty() && !exprs.is_empty() && !exprs.iter().any(|e| is_wildcard(e) || e == "_all") {
                return Err(EsError::resource_not_found(format!("data streams [{}] not found", exprs.join(", "))));
            }
            delete_data_streams(st, &names)
        })
        .await?;
    Reply::acknowledged()
}

/// `_shards` of a broadcast over `indices` on a single node: every copy counts, primaries
/// answer.
fn broadcast_shards<'a>(indices: impl Iterator<Item = &'a IndexMeta>) -> Value {
    let (mut total, mut ok) = (0i64, 0i64);
    for idx in indices {
        if idx.is_closed() {
            continue;
        }
        let s = idx.number_of_shards() as i64;
        total += s * (1 + idx.number_of_replicas() as i64);
        ok += s;
    }
    obj! { "total" => total, "successful" => ok, "failed" => 0 }
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &opts)?;
    let streams: Vec<&Arc<DataStream>> = names.iter().filter_map(|n| st.metadata.data_streams.get(n)).collect();
    let backing: Vec<&IndexMeta> = streams.iter().flat_map(|d| d.indices.iter()).filter_map(|i| st.metadata.indices.get(i)).map(|i| i.as_ref()).collect();
    let size = |bytes: i64, key: &str, m: &mut Map| {
        if ctx.human {
            m.insert(key.to_string(), Value::from(ByteSize(bytes).human()));
        }
        m.insert(format!("{key}_bytes"), Value::from(bytes));
    };
    let mut out = Map::new();
    out.insert("_shards".into(), broadcast_shards(backing.iter().copied()));
    out.insert("data_stream_count".into(), Value::from(streams.len()));
    out.insert("backing_indices".into(), Value::from(backing.len()));
    size(0, "total_store_size", &mut out);
    let list: Vec<Value> = streams
        .iter()
        .map(|d| {
            let mut m = Map::new();
            m.insert("data_stream".into(), Value::from(d.name.clone()));
            m.insert("backing_indices".into(), Value::from(d.indices.len()));
            size(0, "store_size", &mut m);
            m.insert("maximum_timestamp".into(), Value::from(0));
            Value::Obj(m)
        })
        .collect();
    out.insert("data_streams".into(), Value::Arr(list));
    Reply::ok(Value::Obj(out))
}

/// `POST _data_stream/_migrate/<alias>`: turns an alias with a write index into a data stream.
pub async fn migrate(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let alias = ctx.path_param("name").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let members: Vec<Arc<IndexMeta>> = st.metadata.indices.values().filter(|i| i.aliases.contains_key(&alias)).cloned().collect();
            if members.is_empty() {
                return Err(EsError::illegal_argument(format!("alias [{alias}] does not exist")));
            }
            let write = members
                .iter()
                .find(|i| i.aliases[&alias].is_write_index == Some(true))
                .or_else(|| (members.len() == 1 && members[0].aliases[&alias].is_write_index != Some(false)).then(|| &members[0]))
                .cloned();
            let Some(write) = write else {
                return Err(EsError::illegal_argument(format!("alias [{alias}] must specify a write index")));
            };
            let am = &write.aliases[&alias];
            if am.filter.is_some() || am.index_routing.is_some() || am.search_routing.is_some() {
                return Err(EsError::illegal_argument(format!("alias [{alias}] may not have custom filtering or routing")));
            }
            let mut others: Vec<&str> = members.iter().filter(|i| i.aliases.len() > 1).map(|i| i.name.as_str()).collect();
            others.sort_unstable();
            if !others.is_empty() {
                return Err(EsError::illegal_argument(format!(
                    "other aliases referencing indices [{}] must be removed before migrating to a data stream",
                    others.join(",")
                )));
            }
            let (tname, template) = match find_composable(&st.metadata, &alias, false) {
                Some((n, t)) => (n.clone(), t.clone()),
                None => return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{alias}]"))),
            };
            let Some(dst) = template.data_stream.clone() else {
                return Err(EsError::illegal_argument(format!("matching index template [{tname}] for data stream [{alias}] has no data stream template")));
            };
            // Each index becomes a hidden backing index with the data stream timestamp mapper.
            for idx in &members {
                if idx.mapping.is_empty() {
                    return Err(EsError::illegal_argument(format!("backing index [{}] must have mappings for a timestamp field", idx.name)));
                }
                let mut ni = (**idx).clone();
                let ctx = ni.mapping_ctx();
                let snippet = crate::mapping::Mapping::parse(&super::indices::data_stream_mapping_snippet(), &ctx)?;
                let merged = ni.mapping.merge(&snippet, &ctx, crate::mapping::MergeReason::MappingUpdate)?;
                merged.validate(&ctx)?;
                ni.mapping = Arc::new(merged);
                ni.mapping_version += 1;
                ni.aliases.remove(&alias);
                ni.aliases_version += 1;
                if ni.settings.get("index.hidden") != Some("true") {
                    ni.settings.set("index.hidden", "true");
                    ni.settings_version += 1;
                }
                ni.version += 1;
                st.metadata.indices.insert(ni.name.clone(), Arc::new(ni));
            }
            let mut indices: Vec<String> = members.iter().filter(|i| i.name != write.name).map(|i| i.name.clone()).collect();
            indices.push(write.name.clone());
            let ds = DataStream {
                name: alias.clone(),
                indices,
                generation: 1,
                meta: template.meta.clone(),
                hidden: dst.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                allow_custom_routing: dst.get("allow_custom_routing").and_then(Value::as_bool).unwrap_or(false),
                lifecycle: template_lifecycle(st, &template),
                options: template_options(st, &template),
                failure_rollover_on_write: true,
                ..Default::default()
            };
            st.metadata.data_streams.insert(alias.clone(), Arc::new(ds));
            let _ = now;
            st.check_unique_names()?;
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn promote(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            let ds = st.metadata.data_streams.get(&name).cloned().ok_or_else(|| EsError::resource_not_found(format!("data stream [{name}] does not exist")))?;
            if ds.replicated {
                let mut d = (*ds).clone();
                d.replicated = false;
                st.metadata.data_streams.insert(name.clone(), Arc::new(d));
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

enum ModifyAction {
    Add { data_stream: String, index: String, failure_store: bool },
    Remove { data_stream: String, index: String, failure_store: bool },
    /// Removes the index from the data stream and deletes it.
    Delete { data_stream: String, index: String, failure_store: bool },
}

fn parse_modify(body: &Map) -> EsResult<Vec<ModifyAction>> {
    let mut out = Vec::new();
    for (k, v) in body {
        if k != "actions" {
            return Err(EsError::parse(format!("[data_stream_actions] unknown field [{k}]")));
        }
        let Some(items) = v.as_array() else {
            return Err(EsError::parse("[data_stream_actions] actions doesn't support values of type: ".to_string() + crate::mapping::params::java_simple_class(v)));
        };
        for item in items {
            let Some(m) = item.as_object() else { continue };
            for (kind, spec) in m {
                let field = |f: &str| spec.get(f).and_then(Value::as_str).map(str::to_string);
                let (Some(ds), Some(index)) = (field("data_stream"), field("index")) else {
                    return Err(EsError::illegal_argument("must specify a data stream name and an index name"));
                };
                let failure_store = spec.get("failure_store").and_then(Value::as_bool).unwrap_or(false);
                match kind.as_str() {
                    "add_backing_index" => out.push(ModifyAction::Add { data_stream: ds, index, failure_store }),
                    "remove_backing_index" => out.push(ModifyAction::Remove { data_stream: ds, index, failure_store }),
                    "delete_backing_index" => out.push(ModifyAction::Delete { data_stream: ds, index, failure_store }),
                    other => return Err(EsError::parse(format!("[data_stream_action] unknown field [{other}]"))),
                }
            }
        }
    }
    if out.is_empty() {
        return Err(EsError::action_request_validation(&["must specify at least one data stream modification action".into()]));
    }
    Ok(out)
}

fn data_stream_for_modify(st: &ClusterState, name: &str) -> EsResult<DataStream> {
    st.metadata
        .data_streams
        .get(name)
        .map(|d| (**d).clone())
        .ok_or_else(|| EsError::illegal_argument(format!("data stream [{name}] not found")))
}

/// `POST _data_stream/_modify`: adds and removes backing indices.
pub async fn modify(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    let actions = parse_modify(&body)?;
    ctx.cluster
        .update(move |st| {
            for a in actions {
                let delete = matches!(a, ModifyAction::Delete { .. });
                let a = match a {
                    ModifyAction::Delete { data_stream, index, failure_store } => ModifyAction::Remove { data_stream, index, failure_store },
                    other => other,
                };
                match a {
                    ModifyAction::Add { data_stream, index, failure_store } => {
                        let mut ds = data_stream_for_modify(st, &data_stream)?;
                        let idx = st.metadata.indices.get(&index).cloned().ok_or_else(|| EsError::illegal_argument(format!("index [{index}] not found")))?;
                        if let Some(parent) = st.metadata.data_stream_of(&index) {
                            let in_failure = parent.failure_indices.contains(&index);
                            if parent.name != data_stream || in_failure != failure_store {
                                return Err(EsError::illegal_argument(format!(
                                    "cannot add index [{index}] to data stream [{data_stream}] because it is already a {} index on data stream [{}]",
                                    if in_failure { "failure store" } else { "backing" },
                                    parent.name
                                )));
                            }
                            continue;
                        }
                        if !idx.aliases.is_empty() {
                            let mut names: Vec<&str> = idx.aliases.keys().map(String::as_str).collect();
                            names.sort_unstable();
                            let many = names.len() > 1;
                            return Err(EsError::illegal_argument(format!(
                                "cannot add index [{index}] to data stream [{data_stream}] until its {} [{}] {} removed",
                                if many { "aliases" } else { "alias" },
                                names.join(","),
                                if many { "are" } else { "is" }
                            )));
                        }
                        if idx.mapping.is_empty() {
                            return Err(EsError::illegal_argument(format!("backing index [{index}] must have mappings for a timestamp field")));
                        }
                        let mut ni = (*idx).clone();
                        let mctx = ni.mapping_ctx();
                        let snippet = crate::mapping::Mapping::parse(&super::indices::data_stream_mapping_snippet(), &mctx)?;
                        let merged = ni.mapping.merge(&snippet, &mctx, crate::mapping::MergeReason::MappingUpdate)?;
                        merged.validate(&mctx)?;
                        ni.mapping = Arc::new(merged);
                        ni.mapping_version += 1;
                        if ni.settings.get("index.hidden") != Some("true") {
                            ni.settings.set("index.hidden", "true");
                            ni.settings_version += 1;
                        }
                        st.metadata.indices.insert(index.clone(), Arc::new(ni));
                        if failure_store {
                            ds.failure_indices.insert(0, index);
                        } else {
                            ds.indices.insert(0, index);
                        }
                        ds.generation += 1;
                        st.metadata.data_streams.insert(data_stream, Arc::new(ds));
                    }
                    ModifyAction::Remove { data_stream, index, failure_store } | ModifyAction::Delete { data_stream, index, failure_store } => {
                        let mut ds = data_stream_for_modify(st, &data_stream)?;
                        let list = if failure_store { &mut ds.failure_indices } else { &mut ds.indices };
                        let Some(pos) = list.iter().position(|i| *i == index) else {
                            return Err(EsError::illegal_argument(format!("index [{index}] not found in data stream [{data_stream}]")));
                        };
                        if pos + 1 == list.len() && !failure_store {
                            return Err(EsError::illegal_argument(format!(
                                "cannot remove backing index [{index}] of data stream [{data_stream}] because it is the write index"
                            )));
                        }
                        list.remove(pos);
                        ds.generation += 1;
                        st.metadata.data_streams.insert(data_stream, Arc::new(ds));
                        if delete {
                            st.metadata.indices.remove(&index);
                        } else if let Some(idx) = st.metadata.indices.get(&index).cloned() {
                            let mut ni = (*idx).clone();
                            ni.settings.set("index.hidden", "false");
                            ni.settings_version += 1;
                            st.metadata.indices.insert(index, Arc::new(ni));
                        }
                    }
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// `GET _data_stream/<name>/_lifecycle`.
pub fn get_lifecycle(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &opts)?;
    let list: Vec<Value> = names
        .iter()
        .filter_map(|n| st.metadata.data_streams.get(n))
        .map(|ds| {
            let mut m = Map::new();
            m.insert("name".into(), Value::from(ds.name.clone()));
            if let Some(l) = &ds.lifecycle {
                m.insert("lifecycle".into(), lifecycle_with_effective(&st, ds, l, include_defaults));
            }
            Value::Obj(m)
        })
        .collect();
    let mut global = Map::new();
    for (key, out) in [("data_streams.lifecycle.retention.max", "max_retention"), ("data_streams.lifecycle.retention.default", "default_retention")] {
        if let Some(t) = time_setting(&st, key) {
            global.insert(out.into(), Value::from(t.string_rep()));
        }
    }
    Reply::ok(obj! { "global_retention" => Value::Obj(global), "data_streams" => Value::Arr(list) })
}

/// Parses a lifecycle body (`DataStreamLifecycle.PARSER`).
fn parse_lifecycle(ctx: &Ctx, body: &Map) -> EsResult<Value> {
    let raw = ctx.raw_json();
    let at = |path: &[crate::json::locate::Seg], target| raw.and_then(|r| crate::json::locate::locate(r, path, target)).unwrap_or((1, 1));
    let mut out = Map::new();
    for (k, v) in body {
        match k.as_str() {
            "enabled" => match v {
                Value::Bool(b) => {
                    out.insert("enabled".into(), (*b).into());
                }
                other => {
                    let (l, c) = at(&[crate::json::locate::Seg::Key("enabled")], crate::json::locate::Target::Value);
                    return Err(EsError::x_content_parse(l, c, format!("[lifecycle] enabled doesn't support values of type: {}", token_name(other))));
                }
            },
            "data_retention" => {
                if v.is_null() {
                    continue;
                }
                let text = v.to_es_string();
                if let Err(e) = TimeValue::parse(&text, "data_retention") {
                    let (l, c) = at(&[crate::json::locate::Seg::Key("data_retention")], crate::json::locate::Target::Value);
                    return Err(EsError::x_content_parse(l, c, "[lifecycle] failed to parse field [data_retention]")
                        .caused_by(EsError::illegal_argument(e.reason().to_string())));
                }
                out.insert("data_retention".into(), Value::from(text));
            }
            "downsampling" | "downsampling_method" | "frozen_after" => {
                out.insert(k.clone(), v.clone());
            }
            other => {
                let (l, c) = raw.and_then(|r| crate::json::locate::key_location(r, &[], other)).unwrap_or((1, 1));
                return Err(EsError::x_content_parse(l, c, format!("[lifecycle] unknown field [{other}]")));
            }
        }
    }
    if !out.contains_key("enabled") {
        out.shift_insert(0, "enabled".into(), true.into());
    }
    Ok(Value::Obj(out))
}

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

/// Applies `f` to every data stream the request names (they must exist).
async fn update_streams(ctx: &Ctx<'_>, f: impl Fn(&mut DataStream) + Send + 'static) -> EsResult<Reply> {
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let exprs = names(ctx);
    ctx.cluster
        .update(move |st| {
            let names = resolve_data_streams(st, &exprs, &opts)?;
            for n in names {
                if let Some(ds) = st.metadata.data_streams.get(&n).cloned() {
                    let mut d = (*ds).clone();
                    f(&mut d);
                    st.metadata.data_streams.insert(n, Arc::new(d));
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn put_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let lifecycle = parse_lifecycle(ctx, &body)?;
    update_streams(ctx, move |d| d.lifecycle = Some(lifecycle.clone())).await
}

pub async fn delete_lifecycle(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    update_streams(ctx, |d| d.lifecycle = None).await
}

/// `GET <index>/_lifecycle/explain`.
pub fn explain_lifecycle(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let include_defaults = ctx.bool_param("include_defaults", false)?;
    let st = ctx.state();
    let names = super::resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let mut out = Map::new();
    for n in names {
        let Some(idx) = st.metadata.indices.get(&n) else { continue };
        let mut m = Map::new();
        m.insert("index".into(), Value::from(n.clone()));
        let ds = st.metadata.data_stream_of(&n);
        let managed = ds.and_then(|d| d.lifecycle.as_ref().filter(|_| dsl_enabled(d)).map(|l| (d, l)));
        let ilm = idx.settings.get("index.lifecycle.name").is_some_and(|p| !p.is_empty()) && idx.settings.get("index.lifecycle.prefer_ilm") != Some("false");
        match managed {
            Some((d, l)) if !ilm => {
                m.insert("managed_by_lifecycle".into(), true.into());
                let created = idx.creation_date();
                m.insert("index_creation_date_millis".into(), Value::from(created));
                m.insert("time_since_index_creation".into(), Value::from(TimeValue::millis((ctx.now_ms - created).max(0)).human()));
                if d.write_index() != Some(n.as_str())
                    && let Some(next) = d.indices.iter().skip_while(|i| **i != n).nth(1)
                    && let Some(next_idx) = st.metadata.indices.get(next)
                {
                    let rolled = next_idx.creation_date();
                    m.insert("rollover_date_millis".into(), Value::from(rolled));
                    m.insert("time_since_rollover".into(), Value::from(TimeValue::millis((ctx.now_ms - rolled).max(0)).human()));
                    m.insert("generation_time".into(), Value::from(TimeValue::millis((ctx.now_ms - rolled).max(0)).human()));
                }
                m.insert("lifecycle".into(), lifecycle_with_effective(&st, d, l, include_defaults));
            }
            _ => {
                m.insert("managed_by_lifecycle".into(), false.into());
            }
        }
        out.insert(n, Value::Obj(m));
    }
    Reply::ok(obj! { "indices" => Value::Obj(out) })
}

/// `GET _lifecycle/stats`.
pub fn lifecycle_stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let list: Vec<Value> = st
        .metadata
        .data_streams
        .values()
        .filter(|d| dsl_enabled(d))
        .map(|d| obj! { "name" => d.name.clone(), "backing_indices_in_total" => d.indices.len(), "backing_indices_in_error" => 0 })
        .collect();
    let poll = time_setting(&st, "data_streams.lifecycle.poll_interval").map(|t| t.as_millis()).unwrap_or(300_000);
    let mut m = Map::new();
    if !list.is_empty() {
        m.insert("last_run_duration_in_millis".into(), Value::from(0));
        m.insert("time_between_starts_in_millis".into(), Value::from(poll));
    }
    m.insert("data_stream_count".into(), Value::from(list.len()));
    m.insert("data_streams".into(), Value::Arr(list));
    let mut v = Value::Obj(m);
    if ctx.human {
        crate::units::humanize(&mut v);
    }
    Reply::ok(v)
}

/// `GET _data_stream/<name>/_options`.
pub fn get_options(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &opts)?;
    let list: Vec<Value> = names
        .iter()
        .filter_map(|n| st.metadata.data_streams.get(n))
        .map(|ds| {
            let mut m = Map::new();
            m.insert("name".into(), Value::from(ds.name.clone()));
            if let Some(o) = &ds.options {
                m.insert("options".into(), o.clone());
            }
            Value::Obj(m)
        })
        .collect();
    Reply::ok(obj! { "data_streams" => Value::Arr(list) })
}

fn parse_options(body: &Map) -> EsResult<Value> {
    let mut out = Map::new();
    for (k, v) in body {
        match k.as_str() {
            "failure_store" => {
                let Some(fs) = v.as_object() else {
                    if v.is_null() {
                        continue;
                    }
                    return Err(EsError::parse("[options] failure_store doesn't support values of type: ".to_string() + crate::mapping::params::java_simple_class(v)));
                };
                let mut f = Map::new();
                for (fk, fv) in fs {
                    match fk.as_str() {
                        "enabled" => {
                            f.insert("enabled".into(), fv.as_bool().unwrap_or(true).into());
                        }
                        "lifecycle" => {
                            if fv.get("frozen_after").is_some_and(|v| !v.is_null()) {
                                return Err(EsError::action_request_validation(&[
                                    "Failure store lifecycle does not support freezing, please remove the frozen_after configuration.".into(),
                                ]));
                            }
                            let l = if fv.is_null() { Value::Null } else { lifecycle_value(fv) };
                            f.insert("lifecycle".into(), l);
                        }
                        other => return Err(EsError::parse(format!("[failure_store] unknown field [{other}]"))),
                    }
                }
                out.insert("failure_store".into(), Value::Obj(f));
            }
            other => return Err(EsError::parse(format!("[options] unknown field [{other}]"))),
        }
    }
    Ok(Value::Obj(out))
}

pub async fn put_options(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let options = parse_options(&body)?;
    update_streams(ctx, move |d| d.options = Some(options.clone())).await
}

pub async fn delete_options(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    update_streams(ctx, |d| d.options = None).await
}

/// The settings new backing indices of `ds` get: the template's, then the data stream's.
fn effective_settings(st: &ClusterState, ds: &DataStream) -> Settings {
    let mut s = current_template(st, ds).and_then(|(_, t)| templates::compose(&st.metadata, t).ok()).map(|c| c.settings).unwrap_or_default();
    s.merge(&ds.settings);
    if !s.contains("index.routing.allocation.include._tier_preference") {
        s.set("index.routing.allocation.include._tier_preference", "data_hot");
    }
    s
}

/// `GET _data_stream/<name>/_settings`.
pub fn get_settings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &IndicesOptions::STRICT_EXPAND_OPEN)?;
    let list: Vec<Value> = names
        .iter()
        .filter_map(|n| st.metadata.data_streams.get(n))
        .map(|ds| {
            obj! {
                "name" => ds.name.clone(),
                "settings" => ds.settings.to_nested_json(),
                "effective_settings" => effective_settings(&st, ds).to_nested_json(),
            }
        })
        .collect();
    Reply::ok(obj! { "data_streams" => Value::Arr(list) })
}

/// The mapping new backing indices of `ds` get.
fn effective_mapping(st: &ClusterState, ds: &DataStream) -> EsResult<Value> {
    let Some((_, t)) = current_template(st, ds) else { return Ok(obj! {}) };
    let composed = templates::compose(&st.metadata, t)?;
    let custom_routing = t.data_stream.as_ref().and_then(|d| d.get("allow_custom_routing")).and_then(Value::as_bool).unwrap_or(false);
    let mut sources = vec![super::indices::default_timestamp_mapping(custom_routing)];
    sources.extend(composed.mappings);
    if let Some(m) = &ds.mappings {
        sources.push(m.clone());
    }
    sources.push(super::indices::data_stream_mapping_snippet());
    let settings = effective_settings(st, ds);
    let mctx = crate::mapping::MappingCtx::from_settings_in(st.dialect, &settings);
    let mapping = templates::compose_mappings(&sources, &mctx)?;
    Ok(mapping.to_value(&mctx, false))
}

/// `GET _data_stream/<name>/_mappings`.
pub fn get_mappings(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let names = resolve_data_streams(&st, &names(ctx), &IndicesOptions::STRICT_EXPAND_OPEN)?;
    let mut list = Vec::new();
    for n in &names {
        let Some(ds) = st.metadata.data_streams.get(n) else { continue };
        list.push(obj! {
            "name" => ds.name.clone(),
            "mappings" => Value::Obj(ds.mappings.clone().unwrap_or_default()),
            "effective_mappings" => effective_mapping(&st, ds)?,
        });
    }
    Reply::ok(obj! { "data_streams" => Value::Arr(list) })
}

/// Settings `PUT _data_stream/<name>/_settings` accepts, by where they apply
/// (`TransportUpdateDataStreamSettingsAction`).
const DS_SETTINGS_WRITE_INDEX: &[&str] = &["index.number_of_replicas"];
const DS_SETTINGS_ALL_BACKING: &[&str] = &["index.lifecycle.name", "index.lifecycle.prefer_ilm", "index.refresh_interval"];
const DS_SETTINGS_DATA_STREAM_ONLY: &[&str] = &["index.number_of_shards"];

fn ds_settings_response(name: &str, applied: bool, error: Option<String>, settings: Value, effective: Value, results: [Vec<String>; 3]) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), Value::from(name));
    m.insert("applied_to_data_stream".into(), applied.into());
    if let Some(e) = error {
        m.insert("error".into(), Value::from(e));
    }
    m.insert("settings".into(), settings);
    m.insert("effective_settings".into(), effective);
    let [only, write, backing] = results;
    m.insert(
        "index_settings_results".into(),
        obj! {
            "applied_to_data_stream_only" => only,
            "applied_to_data_stream_and_write_indices" => write,
            "applied_to_data_stream_and_backing_indices" => backing,
        },
    );
    Value::Obj(m)
}

/// Validates one data stream settings value (`Settings.getAsInt` and the setting parsers).
fn validate_ds_setting(key: &str, v: &crate::meta::settings::SettingValue) -> Result<(), String> {
    use crate::meta::settings::SettingValue;
    let SettingValue::Str(s) = v else { return Ok(()) };
    if DS_SETTINGS_WRITE_INDEX.contains(&key) || DS_SETTINGS_DATA_STREAM_ONLY.contains(&key) {
        if crate::units::parse_java_int(s).is_none() {
            return Err(format!("Failed to parse int setting [{key}] with value [{s}]"));
        }
        return Ok(());
    }
    match crate::meta::index_settings::lookup(key) {
        Some(def) => crate::meta::index_settings::validate_value(def, v).map_err(|e| e.reason().to_string()),
        None => Ok(()),
    }
}

/// `PUT _data_stream/<name>/_settings`: stores settings on the data stream (for its next
/// backing indices) and applies the dynamic ones to its existing indices. Per-stream problems
/// are reported in the response, not as errors.
pub async fn put_settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let dry_run = ctx.bool_param("dry_run", false)?;
    let body = ctx.body()?.unwrap_or_else(|| obj! {});
    let update = Settings::from_json(&body)?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let exprs = names(ctx);
    let out = ctx
        .cluster
        .update(move |st| {
            let names = resolve_data_streams(st, &exprs, &opts)?;
            let mut out = Vec::new();
            for n in names {
                let Some(ds) = st.metadata.data_streams.get(&n).cloned() else { continue };
                let empty = || obj! {};
                let rejected: Vec<String> = update
                    .keys()
                    .filter(|k| !DS_SETTINGS_WRITE_INDEX.contains(&k.as_str()) && !DS_SETTINGS_ALL_BACKING.contains(&k.as_str()) && !DS_SETTINGS_DATA_STREAM_ONLY.contains(&k.as_str()))
                    .cloned()
                    .collect();
                if !rejected.is_empty() {
                    // Collected in a `HashSet`.
                    let order = crate::json::java_hash_order(&rejected, 16);
                    let list: Vec<&str> = order.into_iter().map(|i| rejected[i].as_str()).collect();
                    let e = format!("Cannot set the following settings on a data stream: [{}]", list.join(","));
                    out.push(ds_settings_response(&n, false, Some(e), empty(), empty(), Default::default()));
                    continue;
                }
                if ds.system {
                    out.push(ds_settings_response(&n, false, Some("Cannot update a system data stream".into()), empty(), empty(), Default::default()));
                    continue;
                }
                if let Some(e) = update.iter().find_map(|(k, v)| validate_ds_setting(k, v).err()) {
                    out.push(ds_settings_response(&n, false, Some(e), empty(), empty(), Default::default()));
                    continue;
                }
                let mut d = (*ds).clone();
                for (k, v) in update.iter() {
                    match v {
                        crate::meta::settings::SettingValue::Null => {
                            d.settings.remove(k);
                        }
                        v => d.settings.insert(k.clone(), v.clone()),
                    }
                }
                let effective = effective_settings(st, &d);
                let mut results: [Vec<String>; 3] = Default::default();
                let mut to_write = Settings::new();
                let mut to_all = Settings::new();
                for k in update.keys() {
                    let v = effective.get_value(k).cloned().unwrap_or(crate::meta::settings::SettingValue::Null);
                    if DS_SETTINGS_WRITE_INDEX.contains(&k.as_str()) {
                        to_write.insert(k.clone(), v);
                        results[1].push(k.clone());
                    } else if DS_SETTINGS_ALL_BACKING.contains(&k.as_str()) {
                        to_write.insert(k.clone(), v.clone());
                        to_all.insert(k.clone(), v);
                        results[2].push(k.clone());
                    } else {
                        results[0].push(k.clone());
                    }
                }
                let response = ds_settings_response(&n, true, None, d.settings.to_nested_json(), effective.to_nested_json(), results);
                if !dry_run {
                    let write = d.write_index().map(str::to_string);
                    for index in d.indices.clone() {
                        let apply = if Some(&index) == write.as_ref() { &to_write } else { &to_all };
                        if apply.is_empty() {
                            continue;
                        }
                        if let Some(idx) = st.metadata.indices.get(&index).cloned() {
                            let mut ni = (*idx).clone();
                            for (k, v) in apply.iter() {
                                match v {
                                    crate::meta::settings::SettingValue::Null => {
                                        ni.settings.remove(k);
                                    }
                                    v => ni.settings.insert(k.clone(), v.clone()),
                                }
                            }
                            crate::meta::auto_expand::apply(&mut ni.settings);
                            ni.settings_version += 1;
                            ni.version += 1;
                            st.metadata.indices.insert(index, Arc::new(ni));
                        }
                    }
                    st.metadata.data_streams.insert(n.clone(), Arc::new(d));
                }
                out.push(response);
            }
            Ok(out)
        })
        .await?;
    Reply::ok(obj! { "data_streams" => Value::Arr(out) })
}

/// `PUT _data_stream/<name>/_mappings`: replaces the data stream's own mappings (applied to its
/// next backing indices on top of the template's).
pub async fn put_mappings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let dry_run = ctx.bool_param("dry_run", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let exprs = names(ctx);
    let out = ctx
        .cluster
        .update(move |st| {
            let names = resolve_data_streams(st, &exprs, &opts)?;
            let mut out = Vec::new();
            for n in names {
                let Some(ds) = st.metadata.data_streams.get(&n).cloned() else { continue };
                let failed = |e: String| {
                    obj! {
                        "name" => n.clone(),
                        "applied_to_data_stream" => false,
                        "error" => e,
                        "mappings" => obj! { "_doc" => obj! {} },
                        "effective_mappings" => obj! { "_doc" => obj! {} },
                    }
                };
                if body.len() > 1 && body.contains_key("_doc") {
                    out.push(failed("cannot merge a map with multiple roots, one of which is [_doc]".into()));
                    continue;
                }
                let mapping = crate::mapping::unwrap_type(&body);
                let mut d = (*ds).clone();
                d.mappings = (!mapping.is_empty()).then(|| mapping.clone());
                let effective = match effective_mapping(st, &d) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(failed(e.reason().to_string()));
                        continue;
                    }
                };
                let mut m = Map::new();
                m.insert("name".into(), Value::from(n.clone()));
                m.insert("applied_to_data_stream".into(), true.into());
                if let Some(own) = &d.mappings {
                    m.insert("mappings".into(), Value::Obj(own.clone()));
                }
                m.insert("effective_mappings".into(), effective);
                out.push(Value::Obj(m));
                if !dry_run {
                    st.metadata.data_streams.insert(n.clone(), Arc::new(d));
                }
            }
            Ok(out)
        })
        .await?;
    Reply::ok(obj! { "data_streams" => Value::Arr(out) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_max_age_follows_retention() {
        let d = |n: i64| TimeValue::millis(n * 24 * 3600 * 1000);
        assert_eq!(automatic_max_age(None), "30d");
        assert_eq!(automatic_max_age(Some(&d(1))), "1h");
        assert_eq!(automatic_max_age(Some(&d(7))), "1d");
        assert_eq!(automatic_max_age(Some(&d(30))), "7d");
        assert_eq!(automatic_max_age(Some(&d(365))), "30d");
    }
}
