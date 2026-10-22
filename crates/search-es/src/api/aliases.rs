//! Alias APIs: `GET/HEAD _alias`, `PUT/DELETE {index}/_alias/{name}` and `POST _aliases`
//! (add / remove / remove_index actions, with per-action results when only some fail).

use super::index_expressions;
use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, key_location};
use crate::json::{Map, Value};
use crate::meta::aliases::parse_alias;
use crate::meta::blocks::{self, Level};
use crate::meta::names::validate_alias_name;
use crate::meta::resolve::{self, IndicesOptions};
use crate::meta::{AliasMeta, ClusterState, IndexMeta};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The `{name}` patterns (`_all` alone means every alias, as `paramAsStringArrayOrEmptyIfAll`).
fn alias_patterns(ctx: &Ctx) -> Vec<String> {
    let names = ctx.list_param("name").unwrap_or_default();
    if names.len() == 1 && names[0] == "_all" { Vec::new() } else { names }
}

/// Whether alias name patterns select `alias` (`Metadata.findAliasInfo`): patterns apply in
/// order, `-pattern` removes what earlier ones selected, `_all` and globs include.
pub fn alias_selected(patterns: &[String], alias: &str) -> bool {
    let mut matched = patterns.is_empty();
    for p in patterns {
        match p.strip_prefix('-') {
            Some(ex) => {
                if matched {
                    matched = !glob_match(ex, alias);
                }
            }
            None => {
                if !matched {
                    matched = p == "_all" || glob_match(p, alias);
                }
            }
        }
    }
    matched
}

/// Requested names reported missing (`RestGetAliasesAction.computeMissingAliases`): explicit
/// names not returned and not excluded by a later `-pattern` (a leading `-` before the first
/// wildcard is a literal name). Sorted.
fn missing_aliases(requested: &[String], returned: &BTreeSet<String>) -> Vec<String> {
    let first_wildcard = requested.iter().position(|a| a.contains('*')).unwrap_or(requested.len());
    let mut missing = BTreeSet::new();
    for (i, a) in requested.iter().enumerate() {
        if a == "_all" || a.contains('*') || (i > first_wildcard && a.starts_with('-')) {
            continue;
        }
        let excluded = requested.iter().skip((i + 1).max(first_wildcard)).any(|e| {
            e.strip_prefix('-').is_some_and(|ex| ex == "_all" || glob_match(ex, a))
        });
        if !excluded && !returned.contains(a) {
            missing.insert(a.clone());
        }
    }
    missing.into_iter().collect()
}

/// The aliases `GET _alias` reports: per index (resolved from `{index}`), the aliases matching
/// `{name}`; plus the requested names that matched nothing.
fn collect(ctx: &Ctx) -> EsResult<(Map, Vec<String>)> {
    let st = ctx.state();
    let names = alias_patterns(ctx);
    let explicit = ctx.param("name").is_some();
    let opts = ctx.indices_options(IndicesOptions { expand_closed: true, ..IndicesOptions::STRICT_EXPAND_OPEN })?;
    let indices = super::resolve_indices(ctx, opts)?;
    super::warn_system_access(ctx, &indices);
    let mut out = Map::new();
    let mut found: BTreeSet<String> = BTreeSet::new();
    for n in &indices {
        let idx = &st.metadata.indices[n];
        // Backing indices can't have aliases; their data streams' aliases are reported below.
        if st.metadata.data_stream_of(n).is_some() {
            continue;
        }
        let mut aliases = Map::new();
        for (a, am) in &idx.aliases {
            if !alias_selected(&names, a) {
                continue;
            }
            aliases.insert(a.clone(), am.to_value());
            found.insert(a.clone());
        }
        // With explicit names only indices that have a matching alias are shown.
        if !aliases.is_empty() || !explicit {
            out.insert(n.clone(), obj! { "aliases" => Value::Obj(aliases) });
        }
    }
    // The response map is a `HashMap` sized for its entries; data stream aliases follow.
    let mut out = crate::json::java_hash_map_expected(out);
    let streams: Vec<String> = match ctx.path_param("index") {
        None => st.metadata.data_streams.keys().cloned().collect(),
        Some(p) => data_streams_matching(&st, &resolve::split_expressions(p), ctx.now_ms)?,
    };
    for d in streams {
        let mut aliases = Map::new();
        for (name, a) in &st.metadata.data_stream_aliases {
            if !a.data_streams.contains(&d) || !alias_selected(&names, name) {
                continue;
            }
            let mut m = Map::new();
            if a.write_data_stream.as_deref() == Some(d.as_str()) {
                m.insert("is_write_index".into(), true.into());
            }
            if let Some(f) = a.filters.get(&d) {
                m.insert("filter".into(), f.clone());
            }
            aliases.insert(name.clone(), Value::Obj(m));
            found.insert(name.clone());
        }
        if !aliases.is_empty() || !explicit {
            out.insert(d, obj! { "aliases" => Value::Obj(aliases) });
        }
    }
    Ok((out, missing_aliases(&names, &found)))
}

pub fn get_alias(ctx: &Ctx) -> EsResult<Reply> {
    let (out, missing) = collect(ctx)?;
    if !missing.is_empty() {
        let msg = if missing.len() == 1 { format!("alias [{}] missing", missing[0]) } else { format!("aliases [{}] missing", missing.join(",")) };
        let mut body = Map::new();
        body.insert("error".into(), msg.into());
        body.insert("status".into(), 404.into());
        for (k, v) in out {
            body.insert(k, v);
        }
        return Ok(Reply::Body(404, Value::Obj(body)));
    }
    Reply::ok(Value::Obj(out))
}

/// One parsed `_aliases` action.
#[derive(Debug)]
enum Action {
    /// `given`: the alias properties the action spelled out (data stream aliases reject some).
    Add { indices: Vec<String>, aliases: Vec<String>, meta: AliasMeta, given: Vec<String> },
    Remove { indices: Vec<String>, aliases: Vec<String>, must_exist: Option<bool> },
    RemoveIndex { indices: Vec<String> },
}

/// A parse failure inside one action: the error, and where the wrappers
/// (`[alias_action] failed to parse field [add]`, `[aliases] failed to parse field [actions]`)
/// point (`None`: the end of the `actions` array, for the request validation errors).
type ActionError = (EsError, Option<(usize, usize)>);

/// The fields each action's `ObjectParser` declares (deprecated names included), for the
/// unknown-field suggestions.
fn declared_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "add" => &[
            "index", "indices", "alias", "aliases", "filter", "routing", "index_routing", "indexRouting", "index-routing", "search_routing",
            "searchRouting", "search-routing", "is_write_index", "is_hidden",
        ],
        "remove" => &["index", "indices", "alias", "aliases", "must_exist"],
        _ => &["index", "indices", "alias", "aliases"],
    }
}

/// Parses one action object as `IndicesAliasesRequest.AliasActions`' parsers do: typed fields
/// (`[add] index doesn't support values of type: VALUE_NUMBER`), setter checks wrapped as
/// `[add] failed to parse field [indices]` at the value's last token, numeric routings read as
/// text, and deprecated routing names with a warning.
fn parse_action(kind: &str, body: &Map, raw: Option<&[u8]>, path: &[Seg], warnings: &mut Vec<String>) -> Result<Action, ActionError> {
    use crate::json::locate::{Target, locate, value_end};
    fn field_path<'a>(path: &[Seg<'a>], f: &'a str) -> Vec<Seg<'a>> {
        let mut p = path.to_vec();
        p.push(Seg::Key(f));
        p
    }
    let at = |f: &str| raw.and_then(|r| locate(r, &field_path(path, f), Target::Value));
    let at_end = |f: &str| raw.and_then(|r| value_end(r, &field_path(path, f)));
    let xparse = |loc: Option<(usize, usize)>, msg: String| match loc {
        Some((l, c)) => EsError::x_content_parse(l, c, msg),
        None => EsError::new(400, "x_content_parse_exception", msg),
    };
    let type_err = |f: &str, v: &Value| -> ActionError {
        let loc = at(f);
        (xparse(loc, format!("[{kind}] {f} doesn't support values of type: {}", crate::meta::aliases::token_name(v))), loc)
    };
    // A setter's exception: reported where the parser stood once it had read the value.
    let setter_err = |f: &str, v: &Value, e: EsError| -> ActionError {
        let loc = if matches!(v, Value::Arr(_) | Value::Obj(_)) { at_end(f) } else { at(f) };
        (xparse(loc, format!("[{kind}] failed to parse field [{f}]")).caused_by(e), loc)
    };
    let iae = |m: &str| EsError::illegal_argument(m);
    let type_name = match kind {
        "add" => "ADD",
        "remove" => "REMOVE",
        _ => "REMOVE_INDEX",
    };
    // `fromList` over `XContentParser::text`: numbers read as text, `null` fails.
    let text_list = |f: &str, v: &Value| -> Result<Vec<String>, ActionError> {
        match v {
            Value::Str(s) => Ok(vec![s.clone()]),
            Value::Arr(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Value::Str(s) => out.push(s.clone()),
                        Value::Num(_) | Value::Bool(_) => out.push(item.to_es_string()),
                        Value::Null => {
                            let mut p = field_path(path, f);
                            p.push(Seg::Index(i));
                            let (l, c) = raw.and_then(|r| locate(r, &p, Target::Value)).unwrap_or((1, 1));
                            return Err(setter_err(f, v, iae(&format!("Expected text at {l}:{c} but found VALUE_NULL"))));
                        }
                        other => return Err(type_err(f, other)),
                    }
                }
                Ok(out)
            }
            other => Err(type_err(f, other)),
        }
    };
    let bool_value = |f: &str, v: &Value| -> Result<bool, ActionError> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Str(s) => crate::units::parse_bool(s).map_err(|e| setter_err(f, v, e)),
            other => Err(type_err(f, other)),
        }
    };
    let mut indices: Option<Vec<String>> = None;
    let mut aliases: Option<Vec<String>> = None;
    let mut meta_src = Map::new();
    let mut given = Vec::new();
    let mut must_exist = None;
    for (k, v) in body {
        match k.as_str() {
            "index" => {
                let Value::Str(s) = v else { return Err(type_err(k, v)) };
                if indices.is_some() {
                    return Err(setter_err(k, v, iae("Only one of [index] and [indices] is supported")));
                }
                if s.is_empty() {
                    return Err(setter_err(k, v, iae("[index] can't be empty string")));
                }
                indices = Some(vec![s.clone()]);
            }
            "indices" => {
                let list = text_list(k, v)?;
                if indices.is_some() {
                    return Err(setter_err(k, v, iae("Only one of [index] and [indices] is supported")));
                }
                if list.is_empty() {
                    return Err(setter_err(k, v, iae("[indices] can't be empty")));
                }
                if list.iter().any(String::is_empty) {
                    return Err(setter_err(k, v, iae("[indices] can't contain empty string")));
                }
                indices = Some(list);
            }
            "alias" => {
                let Value::Str(s) = v else { return Err(type_err(k, v)) };
                if aliases.as_ref().is_some_and(|a| !a.is_empty()) {
                    return Err(setter_err(k, v, iae("Only one of [alias] and [aliases] is supported")));
                }
                if kind == "remove_index" {
                    return Err(setter_err(k, v, iae(&format!("[alias] is unsupported for [{type_name}]"))));
                }
                if s.is_empty() {
                    return Err(setter_err(k, v, iae("[alias] can't be empty string")));
                }
                aliases = Some(vec![s.clone()]);
            }
            "aliases" => {
                let list = text_list(k, v)?;
                if aliases.as_ref().is_some_and(|a| !a.is_empty()) {
                    return Err(setter_err(k, v, iae("Only one of [alias] and [aliases] is supported")));
                }
                if kind == "remove_index" {
                    return Err(setter_err(k, v, iae(&format!("[aliases] is unsupported for [{type_name}]"))));
                }
                if list.is_empty() {
                    return Err(setter_err(k, v, iae("[aliases] can't be empty")));
                }
                if list.iter().any(String::is_empty) {
                    return Err(setter_err(k, v, iae("[aliases] can't contain empty string")));
                }
                aliases = Some(list);
            }
            "filter" if kind == "add" => {
                if !matches!(v, Value::Obj(_)) {
                    return Err(type_err(k, v));
                }
                meta_src.insert("filter".into(), v.clone());
                given.push("filter".to_string());
            }
            "routing" | "index_routing" | "indexRouting" | "index-routing" | "search_routing" | "searchRouting" | "search-routing" if kind == "add" => {
                if !matches!(v, Value::Str(_) | Value::Num(_)) {
                    return Err(type_err(k, v));
                }
                let canonical = match k.as_str() {
                    "routing" => "routing",
                    "index_routing" | "indexRouting" | "index-routing" => "index_routing",
                    _ => "search_routing",
                };
                if canonical != k {
                    let (l, c) = at(k).unwrap_or((1, 1));
                    warnings.push(format!("[{kind}][{l}:{c}] Deprecated field [{k}] used, expected [{canonical}] instead"));
                }
                meta_src.insert(canonical.into(), Value::Str(v.to_es_string()));
                given.push(canonical.to_string());
            }
            "is_write_index" | "is_hidden" if kind == "add" => {
                let b = bool_value(k, v)?;
                meta_src.insert(k.clone(), Value::Bool(b));
                given.push(k.clone());
            }
            "must_exist" if kind == "remove" => must_exist = Some(bool_value(k, v)?),
            other => {
                let loc = raw.and_then(|r| key_location(r, path, other));
                let hint = crate::rest::params::suggest(other, declared_fields(kind));
                // The wrappers point where the parser stopped: the unknown field's value.
                return Err((xparse(loc, format!("[{kind}] unknown field [{other}]{hint}")), at(other)));
            }
        }
    }
    let indices = indices.unwrap_or_default();
    let aliases = aliases.unwrap_or_default();
    if indices.is_empty() {
        return Err((iae("One of [index] or [indices] is required"), None));
    }
    if kind != "remove_index" && aliases.is_empty() {
        return Err((iae("One of [alias] or [aliases] is required"), None));
    }
    Ok(match kind {
        "add" => {
            let name = aliases.first().cloned().unwrap_or_default();
            // The filter is checked when the action is applied, as in Elasticsearch.
            let mut unchecked = meta_src.clone();
            // `routing` only fills in the specific routings that aren't given (`AliasActions`).
            if let Some(r) = unchecked.shift_remove("routing") {
                unchecked.entry("index_routing".to_string()).or_insert_with(|| r.clone());
                unchecked.entry("search_routing".to_string()).or_insert(r);
            }
            let filter = unchecked.shift_remove("filter");
            let mut meta = parse_alias(&name, &Value::Obj(unchecked)).map_err(|e| (e, None))?;
            if let Some(Value::Obj(f)) = filter
                && !f.is_empty()
            {
                meta.filter = Some(Value::Obj(f));
            }
            Action::Add { indices, aliases, meta, given }
        }
        "remove" => Action::Remove { indices, aliases, must_exist },
        _ => Action::RemoveIndex { indices },
    })
}

fn parse_actions(body: &Map, raw: Option<&[u8]>, warnings: &mut Vec<String>) -> EsResult<Vec<Action>> {
    use crate::json::locate::{Target, locate, value_end};
    let xparse = |loc: Option<(usize, usize)>, msg: String| match loc {
        Some((l, c)) => EsError::x_content_parse(l, c, msg),
        None => EsError::new(400, "x_content_parse_exception", msg),
    };
    let mut actions = Vec::new();
    for (k, v) in body {
        if k != "actions" {
            return Err(xparse(raw.and_then(|r| key_location(r, &[], k)), format!("[aliases] unknown field [{k}]")));
        }
        let is_array = matches!(v, Value::Arr(_));
        let items: Vec<&Value> = match v {
            Value::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        // Errors from building an action surface once the whole array is read (at its `]`).
        let array_end = raw.and_then(actions_end);
        for (i, item) in items.iter().enumerate() {
            let Value::Obj(am) = item else { continue };
            let mut base = vec![Seg::Key("actions")];
            if is_array {
                base.push(Seg::Index(i));
            }
            let declared = am.keys().filter(|k| matches!(k.as_str(), "add" | "remove" | "remove_index")).count();
            for (kind, inner) in am {
                let mut path = base.clone();
                path.push(Seg::Key(kind));
                let value_loc = raw.and_then(|r| locate(r, &path, Target::Value));
                match kind.as_str() {
                    "add" | "remove" | "remove_index" => {
                        let Value::Obj(inner) = inner else { continue };
                        let action = parse_action(kind, inner, raw, &path, warnings).map_err(|(e, loc)| {
                            if e.kind == "x_content_parse_exception" {
                                let mid = xparse(loc, format!("[alias_action] failed to parse field [{kind}]")).caused_by(e);
                                xparse(loc, "[aliases] failed to parse field [actions]".into()).caused_by(mid)
                            } else {
                                xparse(array_end, "[aliases] failed to parse field [actions]".into()).caused_by(e)
                            }
                        })?;
                        actions.push(action);
                    }
                    other => {
                        let e = xparse(raw.and_then(|r| key_location(r, &base, other)), format!("[alias_action] unknown field [{other}]"));
                        return Err(xparse(value_loc, "[aliases] failed to parse field [actions]".into()).caused_by(e));
                    }
                }
            }
            if declared > 1 {
                // `ConstructingObjectParser` builds the entry once its last field has arrived.
                let e = EsError::new(400, "x_content_parse_exception", "Failed to build [alias_action] after last required field arrived")
                    .caused_by(EsError::illegal_argument("Too many operations declared on operation entry"));
                let end = raw.and_then(|r| value_end(r, &base));
                return Err(xparse(end, "[aliases] failed to parse field [actions]".into()).caused_by(e));
            }
        }
    }
    if actions.is_empty() {
        return Err(EsError::illegal_argument("No action specified"));
    }
    Ok(actions)
}

/// Location of the `actions` array's closing bracket.
fn actions_end(raw: &[u8]) -> Option<(usize, usize)> {
    let start = crate::json::locate::locate(raw, &[Seg::Key("actions")], crate::json::locate::Target::Value)?;
    let text = std::str::from_utf8(raw).ok()?;
    let mut line = 1;
    let mut col = 1;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut started = false;
    for ch in text.chars() {
        if (line, col) == start {
            started = true;
        }
        if started {
            if in_str {
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_str = false;
                }
            } else {
                match ch {
                    '"' => in_str = true,
                    '[' | '{' => depth += 1,
                    ']' | '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((line, col));
                        }
                    }
                    _ => {}
                }
            }
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += ch.len_utf8();
        }
    }
    None
}

/// Applies alias actions to the state; returns per-action outcomes (`None` = ok).
/// An action's outcome: its error (if it failed alone) and the concrete indices it targeted.
type Outcome = (Option<EsError>, Option<Vec<String>>);

fn apply(st: &mut ClusterState, actions: &[Action], now: i64) -> EsResult<Vec<Outcome>> {
    let os = st.dialect == crate::cluster::Dialect::OpenSearch;
    // OpenSearch: whether any action resolved to something to do (`finalActions`).
    let mut resolved_any = false;
    let opts = IndicesOptions { expand_closed: true, ..IndicesOptions::STRICT_EXPAND_OPEN };
    let mut outcomes: Vec<Outcome> = vec![(None, None); actions.len()];
    // Index removals apply first, so an index can be replaced by an alias of the same name.
    let order = actions
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, Action::RemoveIndex { .. }))
        .chain(actions.iter().enumerate().filter(|(_, a)| !matches!(a, Action::RemoveIndex { .. })))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    for i in order {
        let a = &actions[i];
        let mut targeted = None;
        let outcome: Option<EsError> = match a {
            Action::Add { indices, aliases, meta, given } => {
                let streams = data_streams_matching(st, indices, now)?;
                if !streams.is_empty() {
                    for k in ["routing", "index_routing", "search_routing", "is_hidden"] {
                        if given.iter().any(|g| g == k) {
                            return Err(EsError::illegal_argument(format!("aliases that point to data streams don't support {k}")));
                        }
                    }
                    if !regular_indices(st, indices, &opts, now)?.is_empty() {
                        return Err(EsError::illegal_argument(format!(
                            "expressions [{}] that match with both data streams and regular indices are disallowed",
                            indices.join(", ")
                        )));
                    }
                    for alias in aliases {
                        validate_alias_name(alias)?;
                        if let Some(f) = &meta.filter {
                            crate::meta::aliases::validate_filter(alias, f)?;
                        }
                        for d in &streams {
                            st.metadata.add_data_stream_alias(alias, d, meta.is_write_index, meta.filter.clone());
                        }
                    }
                    outcomes[i] = (None, Some(streams));
                    continue;
                }
                let names = resolve::resolve(st, indices, &IndicesOptions { allow_aliases: false, include_data_streams: false, ..opts }, now)?;
                reject_backing_indices(st, indices, &names)?;
                targeted = Some(names.clone());
                if names.is_empty() {
                    return Err(EsError::index_not_found(&indices.join(",")));
                }
                blocks::check(names.iter().map(|n| st.metadata.indices[n].as_ref()), Level::MetadataWrite)?;
                for alias in aliases {
                    // Alias names may use date math (`<logs-{now/d}>`).
                    let alias = &resolve::resolve_date_math(alias, now)?;
                    validate_alias_name(alias)?;
                    if let Some(f) = &meta.filter {
                        crate::meta::aliases::validate_filter(alias, f)?;
                    }
                    if meta.index_routing.as_deref().is_some_and(|r| r.contains(',')) {
                        return Err(EsError::illegal_argument(format!("alias [{alias}] has several index routing values associated with it")));
                    }
                    if st.metadata.indices.contains_key(alias) {
                        return Err(EsError::invalid_alias_name(alias, "an index, data stream, ESQL view, or ESQL dataset exists with the same name as the alias"));
                    }
                    for n in &names {
                        let mut ni: IndexMeta = (*st.metadata.indices[n]).clone();
                        crate::meta::index_mode::IndexMode::of(&ni.settings).validate_alias(meta.index_routing.as_deref(), meta.search_routing.as_deref())?;
                        ni.aliases.insert(alias.clone(), meta.clone());
                        ni.aliases_version += 1;
                        ni.version += 1;
                        st.metadata.indices.insert(n.clone(), Arc::new(ni));
                    }
                }
                None
            }
            Action::Remove { indices, aliases, must_exist } => {
                let streams = data_streams_matching(st, indices, now)?;
                let mut ds_targets = Vec::new();
                if !streams.is_empty() {
                    let mut removed = 0;
                    for d in &streams {
                        let selected: Vec<String> = st
                            .metadata
                            .data_stream_aliases
                            .iter()
                            .filter(|(name, a)| a.data_streams.iter().any(|x| x == d) && (aliases.iter().any(|p| p == "*" || p == "_all") || aliases.iter().any(|p| glob_match(p, name))))
                            .map(|(name, _)| name.clone())
                            .collect();
                        for a in selected {
                            removed += usize::from(st.metadata.remove_data_stream_alias(&a, d));
                        }
                    }
                    if regular_indices(st, indices, &opts, now)?.is_empty() {
                        // Elasticsearch collects the missing names only from index actions.
                        let outcome = (removed == 0).then(|| EsError::alias_not_found(&[]));
                        outcomes[i] = (outcome, Some(streams));
                        continue;
                    }
                    ds_targets = streams;
                }
                let names = resolve::resolve(st, indices, &IndicesOptions { allow_aliases: false, include_data_streams: false, ..opts }, now)?;
                reject_backing_indices(st, indices, &names)?;
                let mut all = ds_targets.clone();
                all.extend(names.iter().cloned());
                targeted = Some(all);
                blocks::check(names.iter().map(|n| st.metadata.indices[n].as_ref()), Level::MetadataWrite)?;
                let mut removed_any = false;
                let mut missing = Vec::new();
                for alias in aliases {
                    let mut hit = false;
                    for n in &names {
                        let idx = &st.metadata.indices[n];
                        let matching: Vec<String> = idx.aliases.keys().filter(|k| *k == alias || (resolve::is_wildcard(alias) && glob_match(alias, k))).cloned().collect();
                        if matching.is_empty() {
                            continue;
                        }
                        hit = true;
                        let mut ni: IndexMeta = (**idx).clone();
                        for m in matching {
                            ni.aliases.remove(&m);
                        }
                        ni.aliases_version += 1;
                        ni.version += 1;
                        st.metadata.indices.insert(n.clone(), Arc::new(ni));
                    }
                    removed_any |= hit;
                    if !hit && !resolve::is_wildcard(alias) {
                        missing.push(alias.clone());
                    }
                }
                if *must_exist == Some(true) && !missing.is_empty() {
                    // `must_exist: true` fails the whole request.
                    return Err(EsError::alias_not_found(if removed_any || !os { &missing } else { aliases }));
                }
                if os {
                    // OpenSearch only fails when no action of the request resolved to anything.
                    resolved_any |= removed_any || *must_exist == Some(false);
                    None
                } else if !removed_any {
                    // Elasticsearch fails the action when none of its aliases existed.
                    let list = if missing.is_empty() { aliases.clone() } else { missing };
                    Some(EsError::alias_not_found(&list))
                } else {
                    None
                }
            }
            Action::RemoveIndex { indices } => {
                let names = resolve::resolve(st, indices, &IndicesOptions { allow_aliases: false, include_data_streams: false, ..opts }, now)?;
                crate::api::indices::delete_indices(st, &names)?;
                None
            }
        };
        if os && !matches!(a, Action::Remove { .. }) {
            resolved_any = true;
        }
        outcomes[i] = (outcome, targeted);
    }
    if os && !resolved_any {
        // `AliasesNotFoundException` over the set of every requested alias.
        let mut all: Vec<String> = actions.iter().flat_map(|a| if let Action::Remove { aliases, .. } = a { aliases.clone() } else { Vec::new() }).collect();
        all.sort();
        all.dedup();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let order = crate::json::java_hash_order(&refs, 16);
        let ordered: Vec<String> = order.into_iter().map(|i| all[i].clone()).collect();
        return Err(EsError::alias_not_found(&ordered));
    }
    st.check_write_indices()?;
    st.check_hidden_aliases()?;
    st.check_unique_names()?;
    Ok(outcomes)
}

/// Data streams an action's index expressions name (`IndexNameExpressionResolver.dataStreamNames`).
fn data_streams_matching(st: &ClusterState, exprs: &[String], now: i64) -> EsResult<Vec<String>> {
    let md = &st.metadata;
    let mut out: BTreeSet<String> = BTreeSet::new();
    for raw in exprs {
        let e = resolve::resolve_date_math(raw, now)?;
        let (exclude, base) = match e.strip_prefix('-') {
            Some(rest) => (true, rest.to_string()),
            None => (false, e.clone()),
        };
        let matched: Vec<String> = if resolve::is_wildcard(&base) || base == "_all" {
            let dot = base.starts_with('.');
            md.data_streams.iter().filter(|(n, d)| (base == "_all" || glob_match(&base, n)) && (!d.hidden || dot)).map(|(n, _)| n.clone()).collect()
        } else {
            if md.data_streams.contains_key(&base) { vec![base.clone()] } else { Vec::new() }
        };
        for m in matched {
            if exclude {
                out.remove(&m);
            } else {
                out.insert(m);
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// Indices an action's expressions select that don't back a data stream.
fn regular_indices(st: &ClusterState, exprs: &[String], opts: &IndicesOptions, now: i64) -> EsResult<Vec<String>> {
    let lenient = IndicesOptions { allow_aliases: false, ignore_unavailable: true, allow_no_indices: true, ..*opts };
    Ok(resolve::resolve(st, exprs, &lenient, now)?.into_iter().filter(|n| st.metadata.data_stream_of(n).is_none()).collect())
}

/// Backing indices can't have aliases.
fn reject_backing_indices(st: &ClusterState, exprs: &[String], names: &[String]) -> EsResult<()> {
    for n in names {
        if let Some(ds) = st.metadata.data_stream_of(n) {
            return Err(EsError::illegal_argument(format!(
                "The provided expressions [{}] match a backing index belonging to data stream [{}]. Data stream backing indices don't support aliases.",
                exprs.join(","),
                ds.name
            )));
        }
    }
    Ok(())
}

/// An action as `action_results` echoes it, with the concrete indices it targeted.
fn action_value(a: &Action, targeted: Option<&Vec<String>>) -> Value {
    match a {
        Action::Add { indices, aliases, .. } => {
            obj! { "type" => "add", "indices" => targeted.unwrap_or(indices).clone(), "aliases" => aliases.clone() }
        }
        Action::Remove { indices, aliases, .. } => {
            obj! { "type" => "remove", "indices" => targeted.unwrap_or(indices).clone(), "aliases" => aliases.clone() }
        }
        Action::RemoveIndex { indices } => obj! { "type" => "remove_index", "indices" => targeted.unwrap_or(indices).clone() },
    }
}

async fn run_actions(ctx: &Ctx<'_>, actions: Vec<Action>) -> EsResult<Reply> {
    let now = ctx.now_ms;
    let (outcomes, actions) = ctx
        .cluster
        .update(move |st| {
            let outcomes = apply(st, &actions, now)?;
            // Everything failed: nothing to acknowledge. Missing aliases are reported together.
            if outcomes.iter().all(|(e, _)| e.is_some()) {
                let errors: Vec<EsError> = outcomes.into_iter().filter_map(|(e, _)| e).collect();
                if errors.len() > 1 && errors.iter().all(|e| e.kind == "aliases_not_found_exception") {
                    let mut names = Vec::new();
                    for e in &errors {
                        match e.meta().iter().find(|(k, _)| k == "resource.id").map(|(_, v)| v) {
                            Some(Value::Arr(a)) => names.extend(a.iter().map(Value::to_es_string)),
                            Some(v) => names.push(v.to_es_string()),
                            None => {}
                        }
                    }
                    return Err(EsError::alias_not_found(&names));
                }
                return Err(errors.into_iter().next().expect("all failed"));
            }
            Ok((outcomes, actions))
        })
        .await?;
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        return Reply::ok(obj! { "acknowledged" => true });
    }
    if outcomes.iter().all(|(e, _)| e.is_none()) {
        return Reply::ok(obj! { "acknowledged" => true, "errors" => false });
    }
    let results: Vec<Value> = actions
        .iter()
        .zip(&outcomes)
        .map(|(a, (e, targeted))| match e {
            None => obj! { "action" => action_value(a, targeted.as_ref()), "status" => 200 },
            Some(e) => obj! { "action" => action_value(a, targeted.as_ref()), "status" => e.status as i64, "error" => e.to_value(false) },
        })
        .collect();
    Reply::ok(obj! { "acknowledged" => true, "errors" => true, "action_results" => Value::Arr(results) })
}

pub async fn update_aliases(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.ok_or_else(|| EsError::action_request_validation(&["Must specify at least one alias action".into()]))?;
    let mut warnings = Vec::new();
    let actions = parse_actions(&body, ctx.raw_json(), &mut warnings)?;
    for w in warnings {
        ctx.warn(w);
    }
    run_actions(ctx, actions).await
}

pub async fn put_alias(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?.unwrap_or_default();
    let mut name = ctx.path_param("name").map(str::to_string);
    let mut indices = index_expressions(ctx);
    // `RestIndexPutAliasAction`'s lenient field handling: scalar values are read as text,
    // `routing` applies first and the specific routings override it.
    let (mut routing, mut index_routing, mut search_routing) = (None, None, None);
    let mut meta = AliasMeta::default();
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    let mut given = Vec::new();
    for (k, v) in &body {
        match v {
            Value::Obj(f) => {
                if k == "filter" && !f.is_empty() {
                    meta.filter = Some(v.clone());
                } else if os && k != "filter" {
                    return Err(EsError::illegal_argument(format!("unknown field [{k}]")));
                }
            }
            Value::Arr(_) => {
                let (l, c) = ctx.raw_json().and_then(|r| key_location(r, &[], k)).map(|(l, c)| (l, c + k.len() + 3)).unwrap_or((1, 1));
                return Err(EsError::new(400, "parsing_exception", "Unexpected token [START_ARRAY]").with_meta("line", l as i64).with_meta("col", c as i64));
            }
            scalar => {
                let text = if scalar.is_null() { None } else { Some(scalar.to_es_string()) };
                match k.as_str() {
                    "index" => indices = text.map(|t| resolve::split_expressions(&t)).unwrap_or_default(),
                    "alias" => name = text,
                    "routing" => routing = text,
                    "indexRouting" | "index-routing" | "index_routing" => index_routing = text,
                    "searchRouting" | "search-routing" | "search_routing" => search_routing = text,
                    "is_write_index" => {
                        meta.is_write_index = match scalar {
                            Value::Bool(b) => Some(*b),
                            other => Some(crate::units::parse_bool(&other.to_es_string())?),
                        }
                    }
                    "is_hidden" if os => {
                        meta.is_hidden = match scalar {
                            Value::Bool(b) => Some(*b),
                            other => Some(crate::units::parse_bool(&other.to_es_string())?),
                        };
                        given.push("is_hidden".to_string());
                    }
                    other if os => return Err(EsError::illegal_argument(format!("unknown field [{other}]"))),
                    other => return Err(EsError::illegal_argument(format!("Unsupported field [{other}]"))),
                }
            }
        }
    }
    for (k, set) in [("routing", routing.is_some()), ("index_routing", index_routing.is_some()), ("search_routing", search_routing.is_some())] {
        if set {
            given.push(k.to_string());
        }
    }
    meta.index_routing = index_routing.or(routing.clone());
    meta.search_routing = search_routing.or(routing);
    let name = name.ok_or_else(|| EsError::action_request_validation(&["[alias] is missing".into()]))?;
    run_actions(ctx, vec![Action::Add { indices, aliases: vec![name], meta, given }]).await
}

pub async fn delete_alias(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let names = alias_patterns(ctx);
    let raw_names = ctx.list_param("name").unwrap_or_default();
    let indices = index_expressions(ctx);
    let indices = if indices.is_empty() { vec!["_all".to_string()] } else { indices };
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let opts = IndicesOptions { expand_closed: true, allow_aliases: false, ..IndicesOptions::STRICT_EXPAND_OPEN };
            let resolved = resolve::resolve(st, &indices, &opts, now)?;
            blocks::check(resolved.iter().map(|n| st.metadata.indices[n].as_ref()), Level::MetadataWrite)?;
            let mut removed = false;
            for n in &resolved {
                let idx = &st.metadata.indices[n];
                let matching: Vec<String> = idx
                    .aliases
                    .keys()
                    .filter(|a| alias_selected(&names, a))
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                removed = true;
                let mut ni: IndexMeta = (**idx).clone();
                for m in matching {
                    ni.aliases.remove(&m);
                }
                ni.aliases_version += 1;
                st.metadata.indices.insert(n.clone(), Arc::new(ni));
            }
            if !removed {
                return Err(EsError::alias_not_found(&raw_names));
            }
            Ok(())
        })
        .await?;
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        return Reply::ok(obj! { "acknowledged" => true });
    }
    Reply::ok(obj! { "acknowledged" => true, "errors" => false })
}
