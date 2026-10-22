//! `POST <target>/_rollover[/<new_index>]`: rolls an alias or a data stream over to a new write
//! index, optionally only when conditions are met (`TransportRolloverAction`,
//! `MetadataRolloverService`).
//!
//! - Aliases: the new index is named after the write index's `index.provided_name` with its
//!   trailing counter incremented (`logs-000001` → `logs-000002`, date math re-resolved), or
//!   the given name; it is created like any index (templates apply) with the request's
//!   settings, mappings and aliases, and the alias moves to it (an explicit write index keeps
//!   the old index as a read member).
//! - Data streams: a new backing index of the next generation, from the matching template.
//!   `lazy` only marks the data stream to roll over on its next write.
//!
//! Conditions are evaluated against the source index's stats; their results are reported in a
//! `HashMap` keyed by the condition's text (`[max_age: 1d]`), as Elasticsearch does.

use super::indices::{BackingOf, CreateRequest, create_backing_index, create_index};
use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, key_location, locate, value_end};
use crate::json::{Map, Value};
use crate::meta::data_streams::{BACKING_INDEX_PREFIX, FAILURE_STORE_PREFIX, backing_index_name};
use crate::meta::resolve::resolve_date_math;
use crate::meta::settings::Settings;
use crate::meta::templates::find_composable;
use crate::meta::{AliasMeta, ClusterState, IndexMeta, names};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::{ByteSize, TimeValue};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Max,
    Min,
}

#[derive(Clone, Debug)]
enum Cond {
    Age(Kind, TimeValue),
    Docs(Kind, i64),
    Size(Kind, ByteSize),
    PrimaryShardSize(Kind, ByteSize),
    PrimaryShardDocs(Kind, i64),
}

impl Cond {
    fn kind(&self) -> Kind {
        match self {
            Cond::Age(k, _) | Cond::Docs(k, _) | Cond::Size(k, _) | Cond::PrimaryShardSize(k, _) | Cond::PrimaryShardDocs(k, _) => *k,
        }
    }

    fn name(&self) -> String {
        let p = if self.kind() == Kind::Max { "max" } else { "min" };
        let n = match self {
            Cond::Age(..) => "age",
            Cond::Docs(..) => "docs",
            Cond::Size(..) => "size",
            Cond::PrimaryShardSize(..) => "primary_shard_size",
            Cond::PrimaryShardDocs(..) => "primary_shard_docs",
        };
        format!("{p}_{n}")
    }

    /// `Condition.toString()`: `[max_age: 1d]`.
    fn label(&self) -> String {
        let v = match self {
            Cond::Age(_, t) => t.human(),
            Cond::Docs(_, n) | Cond::PrimaryShardDocs(_, n) => n.to_string(),
            Cond::Size(_, b) | Cond::PrimaryShardSize(_, b) => b.human(),
        };
        format!("[{}: {v}]", self.name())
    }

    fn evaluate(&self, s: &Stats) -> bool {
        let ge = |kind: Kind, have: i64, want: i64| match kind {
            Kind::Max | Kind::Min => have >= want,
        };
        match self {
            Cond::Age(k, t) => ge(*k, s.age_ms, t.as_millis()),
            Cond::Docs(k, n) => ge(*k, s.docs, *n),
            Cond::Size(k, b) => ge(*k, s.size, b.bytes()),
            Cond::PrimaryShardSize(k, b) => ge(*k, s.max_primary_shard_size, b.bytes()),
            Cond::PrimaryShardDocs(k, n) => ge(*k, s.max_primary_shard_docs, *n),
        }
    }
}

/// What conditions are evaluated against (`Condition.Stats`).
#[derive(Default)]
struct Stats {
    docs: i64,
    size: i64,
    age_ms: i64,
    max_primary_shard_size: i64,
    max_primary_shard_docs: i64,
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

struct Request {
    conditions: Vec<Cond>,
    create: CreateRequest,
    has_create_parts: bool,
}

struct Parser<'a> {
    raw: &'a [u8],
}

impl Parser<'_> {
    fn at(&self, path: &[Seg]) -> (usize, usize) {
        locate(self.raw, path, Target::Value).unwrap_or((1, 1))
    }

    fn end(&self, path: &[Seg]) -> (usize, usize) {
        value_end(self.raw, path).unwrap_or_else(|| self.at(path))
    }

    /// `RolloverConditions.PARSER`: errors carry where the outer parser reports them.
    fn conditions(&self, m: &Map) -> Result<Vec<Cond>, (EsError, (usize, usize))> {
        let mut out: Vec<Cond> = Vec::new();
        for (k, v) in m {
            let path = [Seg::Key("conditions"), Seg::Key(k)];
            let at = self.at(&path);
            let unsupported = || (EsError::x_content_parse(at.0, at.1, format!("[rollover_conditions] {k} doesn't support values of type: {}", token_name(v))), at);
            let failed = |cause: EsError| (EsError::x_content_parse(at.0, at.1, format!("[rollover_conditions] failed to parse field [{k}]")).caused_by(cause), at);
            let long = || -> Result<i64, (EsError, (usize, usize))> {
                match v {
                    Value::Num(n) => n.as_i64().or_else(|| Some(n.as_f64() as i64)).ok_or_else(unsupported),
                    Value::Str(s) => s.trim().parse::<i64>().map_err(|_| failed(EsError::java(400, "illegal_argument_exception", format!("For input string: \"{s}\"")))),
                    _ => Err(unsupported()),
                }
            };
            let string = || -> Result<&str, (EsError, (usize, usize))> {
                match v {
                    Value::Str(s) => Ok(s.as_str()),
                    _ => Err(unsupported()),
                }
            };
            let time = |kind| -> Result<Cond, (EsError, (usize, usize))> {
                let s = string()?;
                TimeValue::parse(s, k).map(|t| Cond::Age(kind, t)).map_err(|e| failed(EsError::illegal_argument(e.reason().to_string())))
            };
            let size = |kind, primary: bool| -> Result<Cond, (EsError, (usize, usize))> {
                let s = string()?;
                let b = ByteSize::parse(s, k).map_err(failed)?;
                Ok(if primary { Cond::PrimaryShardSize(kind, b) } else { Cond::Size(kind, b) })
            };
            let c = match k.as_str() {
                "max_age" => time(Kind::Max)?,
                "min_age" => time(Kind::Min)?,
                "max_docs" => Cond::Docs(Kind::Max, long()?),
                "min_docs" => Cond::Docs(Kind::Min, long()?),
                "max_size" => size(Kind::Max, false)?,
                "min_size" => size(Kind::Min, false)?,
                "max_primary_shard_size" => size(Kind::Max, true)?,
                "min_primary_shard_size" => size(Kind::Min, true)?,
                "max_primary_shard_docs" => Cond::PrimaryShardDocs(Kind::Max, long()?),
                "min_primary_shard_docs" => Cond::PrimaryShardDocs(Kind::Min, long()?),
                other => {
                    let (l, c) = key_location(self.raw, &[Seg::Key("conditions")], other).unwrap_or((1, 1));
                    return Err((EsError::x_content_parse(l, c, format!("[rollover_conditions] unknown field [{other}]")), at));
                }
            };
            // A condition given twice replaces the earlier one.
            out.retain(|x| x.name() != c.name());
            out.push(c);
        }
        Ok(out)
    }

    fn request(&self, body: &Value) -> EsResult<Request> {
        let Value::Obj(m) = body else {
            return Err(EsError::x_content_parse(1, 1, format!("[rollover] Expected START_OBJECT but was: {}", token_name(body))));
        };
        let mut r = Request { conditions: Vec::new(), create: CreateRequest::default(), has_create_parts: false };
        for (k, v) in m {
            let path = [Seg::Key(k.as_str())];
            let Value::Obj(inner) = v else {
                if matches!(k.as_str(), "conditions" | "settings" | "mappings" | "aliases") {
                    let (l, c) = self.at(&path);
                    return Err(EsError::x_content_parse(l, c, format!("[rollover] {k} doesn't support values of type: {}", token_name(v))));
                }
                let (l, c) = key_location(self.raw, &[], k).unwrap_or((1, 1));
                return Err(EsError::x_content_parse(l, c, format!("[rollover] unknown field [{k}]")));
            };
            let failed = |cause: EsError| {
                let (l, c) = self.end(&path);
                EsError::x_content_parse(l, c, format!("[rollover] failed to parse field [{k}]")).caused_by(cause)
            };
            match k.as_str() {
                "conditions" => {
                    r.conditions = self.conditions(inner).map_err(|(e, (l, c))| EsError::x_content_parse(l, c, "[rollover] failed to parse field [conditions]").caused_by(e))?;
                }
                "settings" => {
                    let mut s = Settings::from_json(v)?;
                    s.normalize_prefix("index.");
                    r.create.settings = s;
                    r.has_create_parts |= !inner.is_empty();
                }
                "mappings" => {
                    if inner.len() == 1 && inner.contains_key("_doc") {
                        return Err(failed(EsError::illegal_argument("The mapping definition cannot be nested under a type")));
                    }
                    r.has_create_parts |= !inner.is_empty();
                    r.create.mappings = Some(inner.clone()).filter(|m| !m.is_empty());
                }
                "aliases" => {
                    r.create.aliases = crate::meta::aliases::parse_aliases(v).map_err(failed)?;
                    r.has_create_parts |= !inner.is_empty();
                }
                other => {
                    let (l, c) = key_location(self.raw, &[], other).unwrap_or((1, 1));
                    return Err(EsError::x_content_parse(l, c, format!("[rollover] unknown field [{other}]")));
                }
            }
        }
        Ok(r)
    }
}

/// `MetadataRolloverService.generateRolloverIndexName`.
fn next_index_name(provided: &str, now_ms: i64) -> EsResult<String> {
    let resolved = resolve_date_math(provided, now_ms)?;
    let is_date_math = resolved != provided;
    let digits = resolved.rsplit_once('-').map(|(_, d)| d).filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    if digits.is_none() {
        return Err(EsError::illegal_argument(format!("index name [{provided}] does not match pattern '^.*-\\d+$'")));
    }
    let dash = provided.rfind('-').expect("checked above");
    let end = if is_date_math { provided.len() - 1 } else { provided.len() };
    let counter: i64 = provided[dash + 1..end].parse().map_err(|_| EsError::illegal_argument(format!("index name [{provided}] does not match pattern '^.*-\\d+$'")))?;
    Ok(format!("{}-{:06}{}", &provided[..dash], counter + 1, if is_date_math { ">" } else { "" }))
}

enum Target2 {
    Alias { write: Arc<IndexMeta> },
    DataStream,
}

/// `MetadataRolloverService.validate` plus the write index lookup.
fn target_of(st: &ClusterState, target: &str, new_index: Option<&str>, req: &Request) -> EsResult<Target2> {
    let md = &st.metadata;
    if let Some(ds) = md.data_streams.get(target) {
        if ds.write_index().is_none() {
            return Err(EsError::illegal_argument(format!("rollover target [{target}] does not point to a write index")));
        }
        if new_index.is_some_and(|n| !n.is_empty()) {
            return Err(EsError::illegal_argument("new index name may not be specified when rolling over a data stream"));
        }
        if req.has_create_parts {
            return Err(EsError::illegal_argument("aliases, mappings, and index settings may not be specified when rolling over a data stream"));
        }
        return Ok(Target2::DataStream);
    }
    if let Some(a) = md.data_stream_aliases.get(target) {
        if a.write_data_stream.is_none() {
            return Err(EsError::illegal_argument(format!("rollover target [{target}] does not point to a write index")));
        }
        return Err(EsError::illegal_state("Aliases to data streams cannot be rolled over. Please rollover the data stream itself."));
    }
    let members: Vec<&Arc<IndexMeta>> = md.indices.values().filter(|i| i.aliases.contains_key(target)).collect();
    if members.is_empty() {
        if md.indices.contains_key(target) {
            return Err(EsError::illegal_argument("rollover target is a [concrete index] but one of [alias,data_stream] was expected"));
        }
        return Err(EsError::resource_not_found(format!("rollover target [{target}] does not exist")));
    }
    let write = members.iter().find(|i| i.aliases[target].is_write_index == Some(true)).copied().or_else(|| {
        (members.len() == 1 && members[0].aliases[target].is_write_index != Some(false)).then_some(members[0])
    });
    match write {
        Some(w) => Ok(Target2::Alias { write: w.clone() }),
        None => Err(EsError::illegal_argument(format!("rollover target [{target}] does not point to a write index"))),
    }
}

/// Stats of the index conditions are evaluated against. Document counts and sizes come from
/// the engine; until it reports them they are zero.
fn stats_of(idx: &IndexMeta, now_ms: i64) -> Stats {
    Stats { age_ms: (now_ms - idx.creation_date()).max(0), ..Stats::default() }
}

fn rollover_response(old: &str, new: &str, rolled: bool, dry_run: bool, lazy: bool, acknowledged: bool, results: Map) -> Value {
    obj! {
        "acknowledged" => acknowledged,
        // A lazy rollover creates no index, so no shards are waited for.
        "shards_acknowledged" => acknowledged && !lazy,
        "old_index" => old,
        "new_index" => new,
        "rolled_over" => rolled,
        "dry_run" => dry_run,
        "lazy" => lazy,
        "conditions" => Value::Obj(results),
    }
}

/// Rolls `target` over in `st` (no condition checks); returns the new index name.
fn apply(st: &mut ClusterState, target: &str, new_index: Option<&str>, create: CreateRequest, now_ms: i64) -> EsResult<(String, String)> {
    let req = Request { conditions: Vec::new(), has_create_parts: false, create: CreateRequest::default() };
    match target_of(st, target, new_index, &req)? {
        Target2::DataStream => {
            let ds = st.metadata.data_streams[target].clone();
            let old = ds.write_index().unwrap_or_default().to_string();
            let generation = ds.generation + 1;
            let name = backing_index_name(BACKING_INDEX_PREFIX, target, generation, now_ms);
            if st.metadata.indices.contains_key(&name) {
                return Err(EsError::index_already_exists(&name, &st.metadata.indices[&name].uuid));
            }
            let (_, template) = match find_composable(&st.metadata, target, ds.hidden) {
                Some((n, t)) if t.data_stream.is_some() => (n.clone(), t.clone()),
                _ => return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{target}]"))),
            };
            create_backing_index(st, &name, BackingOf { data_stream: target, template: &template, settings: &ds.settings, mappings: ds.mappings.as_ref() }, now_ms)?;
            let mut d = (*ds).clone();
            d.indices.push(name.clone());
            d.generation = generation;
            d.rollover_on_write = false;
            // The data stream's mode follows its write index (a template change applies here).
            d.index_mode = st.metadata.indices.get(&name).and_then(|i| i.settings.get("index.mode")).map(str::to_string);
            st.metadata.data_streams.insert(target.to_string(), Arc::new(d));
            Ok((old, name))
        }
        Target2::Alias { write } => {
            let provided = write.settings.get("index.provided_name").unwrap_or(&write.name).to_string();
            let unresolved = match new_index {
                Some(n) => n.to_string(),
                None => next_index_name(&provided, now_ms)?,
            };
            let name = resolve_date_math(&unresolved, now_ms)?;
            names::validate_index_name(&name)?;
            if let Some(existing) = st.metadata.indices.get(&name) {
                return Err(EsError::index_already_exists(&name, &existing.uuid));
            }
            let old_alias = write.aliases[target].clone();
            let explicit = old_alias.is_write_index == Some(true);
            let hidden = create.settings.get("index.hidden").map(|h| h == "true");
            check_template_alias(st, &name, target, hidden.unwrap_or(false))?;
            let mut create = create;
            create.aliases.retain(|(a, _)| a != target);
            let warnings = create_index(st, &unresolved, &name, create, now_ms)?;
            let _ = warnings;
            // Move the alias: the new index becomes the write index.
            let mut new_idx = (*st.metadata.indices[&name]).clone();
            new_idx.aliases.insert(target.to_string(), AliasMeta { is_write_index: if explicit { Some(true) } else { None }, is_hidden: old_alias.is_hidden, ..AliasMeta::default() });
            st.metadata.indices.insert(name.clone(), Arc::new(new_idx));
            let mut old_idx = (*write).clone();
            if explicit {
                if let Some(a) = old_idx.aliases.get_mut(target) {
                    a.is_write_index = Some(false);
                }
            } else {
                old_idx.aliases.remove(target);
            }
            old_idx.aliases_version += 1;
            st.metadata.indices.insert(old_idx.name.clone(), Arc::new(old_idx));
            st.check_write_indices()?;
            Ok((write.name.clone(), name))
        }
    }
}

/// `checkNoDuplicatedAliasInIndexTemplate`.
fn check_template_alias(st: &ClusterState, index: &str, alias: &str, hidden: bool) -> EsResult<()> {
    let md = &st.metadata;
    if let Some((tname, t)) = find_composable(md, index, hidden) {
        let composed = crate::meta::templates::compose(md, t)?;
        if composed.aliases.contains_key(alias) {
            let keys: Vec<&str> = composed.aliases.keys().map(String::as_str).collect();
            return Err(EsError::illegal_argument(format!(
                "Rollover alias [{alias}] can point to multiple indices, found duplicated alias [[{}]] in index template [{tname}]",
                keys.join(", ")
            )));
        }
        return Ok(());
    }
    for (tname, t) in crate::meta::templates::find_legacy(md, index, hidden) {
        if t.aliases.contains_key(alias) {
            let keys: Vec<&str> = t.aliases.keys().map(String::as_str).collect();
            return Err(EsError::illegal_argument(format!(
                "Rollover alias [{alias}] can point to multiple indices, found duplicated alias [[{}]] in index template [{tname}]",
                keys.join(", ")
            )));
        }
    }
    Ok(())
}

/// The `::data` / `::failures` selector of a rollover target.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Selector {
    Data,
    Failures,
}

fn split_selector(raw: &str) -> EsResult<(String, Selector)> {
    match raw.split_once("::") {
        None => Ok((raw.to_string(), Selector::Data)),
        Some((name, "data")) => Ok((name.to_string(), Selector::Data)),
        Some((name, "failures")) => Ok((name.to_string(), Selector::Failures)),
        Some((_, other)) => Err(EsError::action_request_validation(&[format!(
            "Invalid index name [{raw}], invalid usage of :: separator, [{other}] is not a recognized selector"
        )])),
    }
}

/// Rolls a data stream's failure store over (initializing it when it has no index yet).
async fn rollover_failure_store(ctx: &Ctx<'_>, target: &str, req: &Request, dry_run: bool, lazy: bool) -> EsResult<Reply> {
    let st = ctx.state();
    let md = &st.metadata;
    let Some(ds) = md.data_streams.get(target) else {
        if md.indices.contains_key(target) || st.has_alias(target) || md.data_stream_aliases.contains_key(target) {
            return Err(EsError::illegal_state("Rolling over failure stores is only possible on data streams."));
        }
        return Err(EsError::resource_not_found(format!("rollover target [{target}] does not exist")));
    };
    let now = ctx.now_ms;
    let source = ds.failure_write_index().unwrap_or("_none_").to_string();
    if ds.failure_indices.is_empty() && !req.conditions.is_empty() {
        return Err(EsError::illegal_state("Rolling over/initializing an empty failure store is only supported without conditions."));
    }
    let trial = backing_index_name(FAILURE_STORE_PREFIX, target, ds.generation + 1, now);
    if lazy {
        if !req.conditions.is_empty() {
            return Err(EsError::illegal_argument(
                "Lazy rollover can be used only without any conditions. Please remove the conditions from the request body or the query parameter 'lazy'.",
            ));
        }
        if dry_run {
            return Reply::ok(rollover_response(&source, &trial, false, true, true, false, Map::new()));
        }
        let t = target.to_string();
        ctx.cluster
            .update(move |st| {
                if let Some(ds) = st.metadata.data_streams.get(&t).cloned() {
                    let mut d = (*ds).clone();
                    d.failure_rollover_on_write = true;
                    st.metadata.data_streams.insert(t, Arc::new(d));
                }
                Ok(())
            })
            .await?;
        return Reply::ok(rollover_response(&source, &trial, false, false, true, true, Map::new()));
    }
    let stats = st.metadata.indices.get(&source).map(|i| stats_of(i, now)).unwrap_or_default();
    let mut results = Map::new();
    for c in &req.conditions {
        results.insert(c.label(), Value::Bool(c.evaluate(&stats)));
    }
    let met = results.is_empty() || {
        let all_min = req.conditions.iter().filter(|c| c.kind() == Kind::Min).all(|c| results.get(&c.label()) == Some(&Value::Bool(true)));
        let any_max = req.conditions.iter().filter(|c| c.kind() == Kind::Max).any(|c| results.get(&c.label()) == Some(&Value::Bool(true)));
        all_min && any_max
    };
    let results = crate::json::java_hash_map(results, 16);
    if dry_run || !met {
        return Reply::ok(rollover_response(&source, &trial, false, dry_run, false, false, results));
    }
    let t = target.to_string();
    let (old, new) = ctx
        .cluster
        .update(move |st| {
            let ds = st.metadata.data_streams.get(&t).cloned().ok_or_else(|| EsError::resource_not_found(format!("rollover target [{t}] does not exist")))?;
            let old = ds.failure_write_index().unwrap_or("_none_").to_string();
            let generation = ds.generation + 1;
            let name = backing_index_name(FAILURE_STORE_PREFIX, &t, generation, now);
            if let Some(existing) = st.metadata.indices.get(&name) {
                return Err(EsError::index_already_exists(&name, &existing.uuid));
            }
            let template = match find_composable(&st.metadata, &t, ds.hidden) {
                Some((_, tpl)) if tpl.data_stream.is_some() => tpl.clone(),
                _ => return Err(EsError::illegal_argument(format!("no matching index template found for data stream [{t}]"))),
            };
            super::indices::create_failure_index(st, &name, &template, &ds.settings, now)?;
            let mut d = (*ds).clone();
            d.failure_indices.push(name.clone());
            d.generation = generation;
            d.failure_rollover_on_write = false;
            st.metadata.data_streams.insert(t, Arc::new(d));
            Ok((old, name))
        })
        .await?;
    Reply::ok(rollover_response(&old, &new, true, false, false, true, results))
}

pub async fn rollover(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let (target, selector) = split_selector(ctx.path_param("alias").unwrap_or_default())?;
    let new_index = ctx.path_param("new_index").map(str::to_string);
    let dry_run = ctx.bool_param("dry_run", false)?;
    let lazy = ctx.bool_param("lazy", false)?;
    let req = match ctx.body()? {
        Some(b) => Parser { raw: ctx.raw_json().unwrap_or(b"") }.request(&b)?,
        None => Request { conditions: Vec::new(), create: CreateRequest::default(), has_create_parts: false },
    };
    if req.conditions.iter().any(|c| matches!(c, Cond::Size(Kind::Max, _))) {
        ctx.warn("Use of the [max_size] rollover condition has been deprecated in favour of the [max_primary_shard_size] condition and will be removed in a later version");
    }
    let has_min = req.conditions.iter().any(|c| c.kind() == Kind::Min);
    let has_max = req.conditions.iter().any(|c| c.kind() == Kind::Max);
    if has_min && !has_max {
        return Err(EsError::action_request_validation(&["at least one max_* rollover condition must be set when using min_* conditions".into()]));
    }
    if selector == Selector::Failures {
        return rollover_failure_store(ctx, &target, &req, dry_run, lazy).await;
    }
    let now = ctx.now_ms;
    let st = ctx.state();
    // Trial names and condition results, from the current state.
    let target_kind = target_of(&st, &target, new_index.as_deref(), &req)?;
    let (source, trial_name) = match &target_kind {
        Target2::DataStream => {
            let ds = &st.metadata.data_streams[&target];
            (ds.write_index().unwrap_or_default().to_string(), backing_index_name(BACKING_INDEX_PREFIX, &target, ds.generation + 1, now))
        }
        Target2::Alias { write } => {
            let provided = write.settings.get("index.provided_name").unwrap_or(&write.name).to_string();
            let unresolved = match &new_index {
                Some(n) => n.clone(),
                None => next_index_name(&provided, now)?,
            };
            (write.name.clone(), resolve_date_math(&unresolved, now)?)
        }
    };
    names::validate_index_name(&trial_name)?;
    if let Some(existing) = st.metadata.indices.get(&trial_name) {
        return Err(EsError::index_already_exists(&trial_name, &existing.uuid));
    }
    if lazy {
        let is_ds = matches!(target_kind, Target2::DataStream);
        if !is_ds || !req.conditions.is_empty() {
            let msg = if is_ds {
                "Lazy rollover can be used only without any conditions. Please remove the conditions from the request body or the query parameter 'lazy'."
            } else if req.conditions.is_empty() {
                "Lazy rollover can be applied only on a data stream. Please remove the query parameter 'lazy'."
            } else {
                "Lazy rollover can be applied only on a data stream with no conditions. Please remove the query parameter 'lazy'."
            };
            return Err(EsError::illegal_argument(msg));
        }
        if dry_run {
            return Reply::ok(rollover_response(&source, &trial_name, false, true, true, false, Map::new()));
        }
        let t = target.clone();
        ctx.cluster
            .update(move |st| {
                if let Some(ds) = st.metadata.data_streams.get(&t).cloned() {
                    let mut d = (*ds).clone();
                    d.rollover_on_write = true;
                    st.metadata.data_streams.insert(t, Arc::new(d));
                }
                Ok(())
            })
            .await?;
        return Reply::ok(rollover_response(&source, &trial_name, false, false, true, true, Map::new()));
    }
    let stats = st.metadata.indices.get(&source).map(|i| stats_of(i, now)).unwrap_or_default();
    let mut results = Map::new();
    for c in &req.conditions {
        results.insert(c.label(), Value::Bool(c.evaluate(&stats)));
    }
    let met = results.is_empty() || {
        let all_min = req.conditions.iter().filter(|c| c.kind() == Kind::Min).all(|c| results.get(&c.label()) == Some(&Value::Bool(true)));
        let any_max = req.conditions.iter().filter(|c| c.kind() == Kind::Max).any(|c| results.get(&c.label()) == Some(&Value::Bool(true)));
        all_min && any_max
    };
    // `Collectors.toMap` builds a default `HashMap`.
    let results = crate::json::java_hash_map(results, 16);
    if dry_run || !met {
        return Reply::ok(rollover_response(&source, &trial_name, false, dry_run, false, false, results));
    }
    let create = req.create;
    let t = target.clone();
    let ni = new_index.clone();
    let (old, new) = ctx.cluster.update(move |st| apply(st, &t, ni.as_deref(), create, now)).await?;
    Reply::ok(rollover_response(&old, &new, true, false, false, true, results))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollover_names() {
        assert_eq!(next_index_name("logs-000001", 0).unwrap(), "logs-000002");
        assert_eq!(next_index_name("logs-9", 0).unwrap(), "logs-000010");
        assert_eq!(next_index_name("logs-999999", 0).unwrap(), "logs-1000000");
        assert_eq!(next_index_name("<logs-{now/d}-000001>", 0).unwrap(), "<logs-{now/d}-000002>");
        assert_eq!(next_index_name("logs", 0).unwrap_err().reason(), "index name [logs] does not match pattern '^.*-\\d+$'");
        assert_eq!(next_index_name("logs-x", 0).unwrap_err().reason(), "index name [logs-x] does not match pattern '^.*-\\d+$'");
    }

    #[test]
    fn condition_labels() {
        let t = TimeValue::parse("24h", "max_age").unwrap();
        assert_eq!(Cond::Age(Kind::Max, t).label(), "[max_age: 1d]");
        assert_eq!(Cond::Size(Kind::Max, ByteSize::parse("1024mb", "max_size").unwrap()).label(), "[max_size: 1gb]");
        assert_eq!(Cond::Docs(Kind::Min, 0).label(), "[min_docs: 0]");
        assert_eq!(Cond::Age(Kind::Min, TimeValue::parse("0ms", "min_age").unwrap()).label(), "[min_age: 0s]");
    }
}
