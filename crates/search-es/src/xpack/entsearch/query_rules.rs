//! Query rules: rulesets (`_query_rules/<ruleset>`), single rules
//! (`_query_rules/<ruleset>/_rule/<rule>`), `_test`, and the rewrite of the `rule` query.
//!
//! A rule's criteria all have to match the request's `match_criteria`
//! (`QueryRuleCriteriaType`: `exact`, `fuzzy` (Levenshtein similarity above 0.5), `prefix`,
//! `suffix`, `contains`, `lt`/`lte`/`gt`/`gte`, `always`); matching `pinned` rules pin their
//! documents (in rule order) above the organic results, matching `exclude` rules remove theirs.
//! [`rewrite`] turns a `rule` query into the equivalent `pinned`/`bool` query, which the search
//! layer then runs.

use crate::error::{EsError, EsResult};
use crate::json::locate::Seg;
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;

pub const RULESETS: &str = "query_rules";
const MAX_RULES: usize = 100;
const CRITERIA_TYPES: [&str; 10] = ["always", "exact", "fuzzy", "prefix", "suffix", "contains", "lt", "lte", "gt", "gte"];
const RULE_TYPES: [&str; 2] = ["pinned", "exclude"];

/// A rule that failed to parse: the error as the rule's own parser reports it, at `loc` (the
/// location every enclosing parser repeats).
#[derive(Debug)]
struct RuleError {
    error: EsError,
    loc: (usize, usize),
}

/// Where a parse of the JSON object at `path` failed: at the value of `field` when the object's
/// constructor ran as that field arrived, else at the object's closing brace.
fn loc_in(src: Option<&[u8]>, path: &[Seg<'_>], field: Option<&str>) -> (usize, usize) {
    use crate::json::locate::{Target, locate, value_end};
    let Some(src) = src else { return (1, 1) };
    let found = match field {
        Some(f) => {
            let mut p = path.to_vec();
            p.push(Seg::Key(f));
            // Jackson reports a scalar at its first character, a container at its last.
            match locate(src, &p, Target::Value) {
                Some(start) if !matches!(value_at(src, start), Some(b'{' | b'[')) => Some(start),
                _ => value_end(src, &p),
            }
        }
        None => value_end(src, path),
    };
    found.unwrap_or((1, 1))
}

/// The byte at a 1-based `(line, col)` location.
fn value_at(src: &[u8], (line, col): (usize, usize)) -> Option<u8> {
    let start = if line == 1 { 0 } else { src.iter().enumerate().filter(|(_, b)| **b == b'\n').nth(line - 2)?.0 + 1 };
    src.get(start + col - 1).copied()
}

/// The constructor argument that arrives last when an object holds all of `args` (a
/// `ConstructingObjectParser` builds its target then, rather than at the closing brace).
fn trigger<'a>(m: &'a Map, args: &[&str]) -> Option<&'a str> {
    if !args.iter().all(|a| m.contains_key(*a)) {
        return None;
    }
    m.keys().rev().find(|k| args.contains(&k.as_str())).map(String::as_str)
}

/// A constructor failure of parser `name` (`ConstructingObjectParser.buildTarget`).
fn build_failure(name: &str, field: Option<&str>, loc: (usize, usize), cause: EsError) -> EsError {
    let built = EsError::x_content_parse_bare(format!("Failed to build [{name}] after last required field arrived")).caused_by(cause);
    match field {
        Some(f) => EsError::x_content_parse(loc.0, loc.1, format!("[{name}] failed to parse field [{f}]")).caused_by(built),
        None => built,
    }
}

/// Parses one criterion (`QueryRuleCriteria`).
fn parse_criterion(c: &Value) -> Result<Value, (Option<&str>, EsError)> {
    const ARGS: [&str; 3] = ["type", "metadata", "values"];
    let Value::Obj(m) = c else { return Err((None, EsError::illegal_argument("criteria must be objects"))) };
    let Some(raw_type) = m.get("type").map(Value::to_es_string) else { return Err((None, EsError::illegal_argument("Required [type]"))) };
    let field = trigger(m, &ARGS);
    let fail = |e: EsError| Err((Some(field.unwrap_or("")), e));
    let cty = raw_type.to_lowercase();
    if !CRITERIA_TYPES.contains(&cty.as_str()) {
        return fail(EsError::illegal_argument(format!("Unknown QueryRuleCriteriaType: {raw_type}")));
    }
    let mut cm = Map::new();
    cm.insert("type".into(), cty.clone().into());
    let meta = m.get("metadata").filter(|v| !v.is_null());
    let values = match m.get("values") {
        Some(Value::Arr(a)) => Some(a.clone()),
        _ => None,
    };
    if cty != "always" {
        if meta.is_none_or(|v| v.to_es_string().is_empty()) {
            return fail(EsError::illegal_argument("criteriaMetadata cannot be blank"));
        }
        if values.as_ref().is_none_or(Vec::is_empty) {
            return fail(EsError::illegal_argument("criteriaValues cannot be null or empty"));
        }
    }
    if let Some(v) = meta {
        cm.insert("metadata".into(), v.clone());
    }
    if let Some(v) = values {
        cm.insert("values".into(), Value::Arr(v));
    }
    Ok(Value::Obj(cm))
}

/// `QueryRuleCriteriaType.isValidForInput`.
fn valid_input(cty: &str, v: &Value) -> bool {
    match cty {
        "exact" => matches!(v, Value::Str(_) | Value::Num(_)),
        "fuzzy" | "prefix" | "suffix" | "contains" => matches!(v, Value::Str(_)),
        "lt" | "lte" | "gt" | "gte" => as_double(v).is_some(),
        _ => false,
    }
}

/// Validates and normalizes one rule (`QueryRule`) at `path` in the request body: field order
/// `rule_id, type, criteria, actions, priority`. Errors come as the rule's parser reports them.
fn parse_rule(src: Option<&[u8]>, path: &[Seg<'_>], rule: &Value, id_override: Option<&str>) -> Result<Value, RuleError> {
    const ARGS: [&str; 5] = ["rule_id", "type", "criteria", "actions", "priority"];
    let Value::Obj(m) = rule else { return Err(RuleError { error: EsError::illegal_argument("rule must be an object"), loc: loc_in(src, path, None) }) };
    // Criteria parse (and fail) as the rule's fields arrive, before the rule is built.
    let mut out_criteria = Vec::new();
    if let Some(Value::Arr(criteria)) = m.get("criteria") {
        for (j, c) in criteria.iter().enumerate() {
            let mut cpath = path.to_vec();
            cpath.push(Seg::Key("criteria"));
            cpath.push(Seg::Index(j));
            match parse_criterion(c) {
                Ok(v) => out_criteria.push(v),
                Err((field, cause)) => {
                    let field = field.filter(|f| !f.is_empty());
                    let loc = loc_in(src, &cpath, field);
                    let inner = if cause.reason().starts_with("Required [") { cause } else { build_failure("query_rule_criteria", field, loc, cause) };
                    let error = EsError::x_content_parse(loc.0, loc.1, "[query_rule] failed to parse field [criteria]").caused_by(inner);
                    return Err(RuleError { error, loc });
                }
            }
        }
    }
    let missing: Vec<&str> = ["type", "criteria", "actions"].into_iter().filter(|k| !m.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(RuleError { error: EsError::illegal_argument(format!("Required [{}]", missing.join(", "))), loc: loc_in(src, path, None) });
    }
    let field = trigger(m, &ARGS);
    let loc = loc_in(src, path, field);
    let fail = |cause: EsError| Err(RuleError { error: build_failure("query_rule", field, loc, cause), loc });
    let ty = m["type"].to_es_string().to_lowercase();
    if !RULE_TYPES.contains(&ty.as_str()) {
        return fail(EsError::illegal_argument(format!("Unknown QueryRuleType: {}", m["type"].to_es_string())));
    }
    if out_criteria.is_empty() {
        return fail(EsError::illegal_argument("Query rule criteria cannot be empty"));
    }
    let actions = m["actions"].as_object().cloned().unwrap_or_default();
    if actions.is_empty() {
        return fail(EsError::illegal_argument("Query rule actions cannot be empty"));
    }
    let priority = match m.get("priority") {
        None | Some(Value::Null) => None,
        Some(p) => Some(p.as_i64().unwrap_or(-1)),
    };
    if let Some(pr) = priority
        && !(0..=1_000_000).contains(&pr)
    {
        return fail(EsError::illegal_argument(format!("Priority was {pr}, must be between 0 and 1000000")));
    }
    if actions.contains_key("ids") == actions.contains_key("docs") {
        return fail(EsError::parse(format!("{ty} query rule actions must contain only one of either ids or docs")));
    }
    match actions.get("ids").or_else(|| actions.get("docs")) {
        Some(Value::Arr(a)) if a.is_empty() => return fail(EsError::parse(format!("{ty} query rule actions cannot be empty"))),
        Some(Value::Arr(a)) if a.len() > 100 => return fail(EsError::parse(format!("{ty} documents cannot exceed 100"))),
        Some(Value::Arr(_)) | None => {}
        Some(_) => return fail(EsError::parse(format!("{ty} query rule actions must be a list"))),
    }
    for c in &out_criteria {
        let cty = c.get("type").and_then(Value::as_str).unwrap_or_default();
        for v in c.get("values").and_then(Value::as_array).into_iter().flatten() {
            if !valid_input(cty, v) {
                return fail(EsError::illegal_argument(format!("Input [{}] is not valid for CriteriaType [{cty}]", v.to_es_string())));
            }
        }
    }
    let mut out = Map::new();
    out.insert("rule_id".into(), id_override.map(Value::from).or_else(|| m.get("rule_id").cloned()).unwrap_or(Value::Null));
    out.insert("type".into(), ty.into());
    out.insert("criteria".into(), Value::Arr(out_criteria));
    out.insert("actions".into(), Value::Obj(actions));
    if let Some(p) = priority {
        out.insert("priority".into(), p.into());
    }
    Ok(Value::Obj(out))
}

/// `PutQueryRulesetAction.Request.validate`: every rule needs an id.
fn missing_rule_id(rules: &[Value]) -> Option<EsError> {
    let errors: Vec<String> = rules
        .iter()
        .filter(|r| r.get("rule_id").is_none_or(|v| v.is_null() || v.as_str() == Some("")))
        .map(|r| format!("rule_id cannot be null or empty. rule: [{}]", r.to_json()))
        .collect();
    (!errors.is_empty()).then(|| EsError::action_request_validation(&errors))
}

/// Rules sorted by priority (rules without one keep their order, after prioritized ones).
fn sorted_rules(mut rules: Vec<Value>) -> Vec<Value> {
    rules.sort_by_key(|r| r.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));
    rules
}

pub async fn put_ruleset(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("ruleset_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if k != "rules" {
            return Err(crate::xpack::unknown_field(ctx, "query_ruleset", k));
        }
    }
    let Some(Value::Arr(rules)) = body.get("rules") else { return Err(EsError::illegal_argument("Required [rules]")) };
    let mut parsed = Vec::with_capacity(rules.len());
    for (i, r) in rules.iter().enumerate() {
        let path = [Seg::Key("rules"), Seg::Index(i)];
        parsed.push(parse_rule(ctx.raw_json(), &path, r, None).map_err(|e| {
            EsError::x_content_parse(e.loc.0, e.loc.1, "[query_ruleset] failed to parse field [rules]").caused_by(e.error)
        })?);
    }
    if parsed.is_empty() {
        return Err(EsError::x_content_parse_bare("Failed to build [query_ruleset] after last required field arrived").caused_by(EsError::illegal_argument("rules cannot be empty")));
    }
    if let Some(e) = missing_rule_id(&parsed) {
        return Err(e);
    }
    if rules.len() > MAX_RULES {
        return Err(EsError::action_request_validation(&[format!("The number of rules in a ruleset cannot exceed [{MAX_RULES}]. This maximum can be set by changing the [xpack.applications.rules.max_rules_per_ruleset] setting.")]));
    }
    let ruleset = obj! { "ruleset_id" => id.clone(), "rules" => Value::Arr(sorted_rules(parsed)) };
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, RULESETS, &id, ruleset, now)).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" } }))
}

pub fn get_ruleset(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("ruleset_id").unwrap_or_default();
    ctx.state().metadata.xpack.get(RULESETS, id).cloned().map_or_else(|| Err(EsError::resource_not_found(id.to_string())), Reply::ok)
}

pub async fn delete_ruleset(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("ruleset_id").unwrap_or_default().to_string();
    ctx.cluster.update(move |st| store::remove(st, RULESETS, &id).map(|_| ()).ok_or_else(|| EsError::resource_not_found(id.clone()))).await?;
    Reply::acknowledged()
}

pub fn list_rulesets(ctx: &Ctx) -> EsResult<Reply> {
    let from = ctx.int_param("from", 0)?;
    let size = ctx.int_param("size", 100)?;
    if from < 0 {
        return Err(EsError::action_request_validation(&[format!("[from] parameter cannot be negative, found [{from}]")]));
    }
    if size < 0 {
        return Err(EsError::action_request_validation(&[format!("[size] parameter cannot be negative, found [{size}]")]));
    }
    let st = ctx.state();
    let all: Vec<Value> = st
        .metadata
        .xpack
        .iter(RULESETS)
        .map(|(id, rs)| {
            let rules = rs.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut crit = [0usize; CRITERIA_TYPES.len()];
            let mut types = [0usize; RULE_TYPES.len()];
            for r in &rules {
                if let Some(i) = RULE_TYPES.iter().position(|t| Some(*t) == r.get("type").and_then(Value::as_str)) {
                    types[i] += 1;
                }
                for c in r.get("criteria").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(i) = CRITERIA_TYPES.iter().position(|t| Some(*t) == c.get("type").and_then(Value::as_str)) {
                        crit[i] += 1;
                    }
                }
            }
            let cm: Map = CRITERIA_TYPES.iter().zip(crit).filter(|(_, n)| *n > 0).map(|(t, n)| (t.to_string(), Value::from(n))).collect();
            let tm: Map = RULE_TYPES.iter().zip(types).filter(|(_, n)| *n > 0).map(|(t, n)| (t.to_string(), Value::from(n))).collect();
            obj! { "ruleset_id" => id.clone(), "rule_total_count" => rules.len(), "rule_criteria_types_counts" => Value::Obj(cm), "rule_type_counts" => Value::Obj(tm) }
        })
        .collect();
    let count = all.len();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(all.into_iter().skip(from as usize).take(size as usize).collect()) })
}

pub async fn put_rule(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ruleset = ctx.path_param("ruleset_id").unwrap_or_default().to_string();
    let rule_id = ctx.path_param("rule_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !matches!(k.as_str(), "type" | "criteria" | "actions" | "priority" | "rule_id") {
            return Err(crate::xpack::unknown_field(ctx, "query_rule", k));
        }
    }
    let rule = parse_rule(ctx.raw_json(), &[], &Value::Obj(body), Some(&rule_id)).map_err(|e| e.error)?;
    let now = ctx.now_ms;
    let created = ctx
        .cluster
        .update(move |st| {
            let mut rules = st.metadata.xpack.get(RULESETS, &ruleset).and_then(|r| r.get("rules")).and_then(Value::as_array).cloned().unwrap_or_default();
            let created = match rules.iter().position(|r| r.get("rule_id").and_then(Value::as_str) == Some(&rule_id)) {
                Some(i) => {
                    rules[i] = rule;
                    false
                }
                None => {
                    if rules.len() >= MAX_RULES {
                        return Err(EsError::illegal_argument(format!("Ruleset [{ruleset}] cannot exceed [{MAX_RULES}] rules")));
                    }
                    rules.push(rule);
                    true
                }
            };
            store::put(st, RULESETS, &ruleset, obj! { "ruleset_id" => ruleset.clone(), "rules" => Value::Arr(sorted_rules(rules)) }, now)?;
            Ok(created)
        })
        .await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" } }))
}

fn rule_not_found(rule: &str, ruleset: &str) -> EsError {
    EsError::resource_not_found(format!("rule id {rule} not found in ruleset {ruleset}"))
}

pub fn get_rule(ctx: &Ctx) -> EsResult<Reply> {
    let ruleset = ctx.path_param("ruleset_id").unwrap_or_default();
    let rule = ctx.path_param("rule_id").unwrap_or_default();
    let st = ctx.state();
    let Some(rs) = st.metadata.xpack.get(RULESETS, ruleset) else { return Err(EsError::resource_not_found(ruleset.to_string())) };
    rs.get("rules")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|r| r.get("rule_id").and_then(Value::as_str) == Some(rule)))
        .cloned()
        .map_or_else(|| Err(rule_not_found(rule, ruleset)), Reply::ok)
}

pub async fn delete_rule(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ruleset = ctx.path_param("ruleset_id").unwrap_or_default().to_string();
    let rule = ctx.path_param("rule_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(rs) = st.metadata.xpack.get(RULESETS, &ruleset).cloned() else { return Err(EsError::resource_not_found(ruleset.clone())) };
            let mut rules = rs.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
            let before = rules.len();
            rules.retain(|r| r.get("rule_id").and_then(Value::as_str) != Some(&rule));
            if rules.len() == before {
                return Err(rule_not_found(&rule, &ruleset));
            }
            store::put(st, RULESETS, &ruleset, obj! { "ruleset_id" => ruleset.clone(), "rules" => Value::Arr(rules) }, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub fn test(ctx: &Ctx) -> EsResult<Reply> {
    let ruleset = ctx.path_param("ruleset_id").unwrap_or_default();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(Value::Obj(criteria)) = body.get("match_criteria") else { return Err(EsError::illegal_argument("Required [match_criteria]")) };
    let st = ctx.state();
    let Some(rs) = st.metadata.xpack.get(RULESETS, ruleset) else { return Err(EsError::resource_not_found(ruleset.to_string())) };
    let mut matched = Vec::new();
    for r in rs.get("rules").and_then(Value::as_array).into_iter().flatten() {
        if rule_matches(r, criteria)? {
            matched.push(obj! { "ruleset_id" => ruleset, "rule_id" => r.get("rule_id").cloned().unwrap_or(Value::Null) });
        }
    }
    Reply::ok(obj! { "total_matched_rules" => matched.len(), "matched_rules" => Value::Arr(matched) })
}

fn as_double(v: &Value) -> Option<f64> {
    match v {
        Value::Num(_) => v.as_f64(),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Lucene's `LevenshteinDistance.getDistance`: `1 - distance / max(len)`.
fn levenshtein_similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return if a.len() == b.len() { 1.0 } else { 0.0 };
    }
    let mut prev: Vec<usize> = (0..=a.len()).collect();
    let mut cur = vec![0; a.len() + 1];
    for (j, cb) in b.iter().enumerate() {
        cur[0] = j + 1;
        for (i, ca) in a.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[i + 1] = (cur[i] + 1).min(prev[i + 1] + 1).min(prev[i] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - prev[a.len()] as f32 / a.len().max(b.len()) as f32
}

/// `QueryRuleCriteriaType.isMatch` for one input and criteria value.
fn criterion_matches(ty: &str, input: &Value, value: &Value) -> EsResult<bool> {
    let valid = match ty {
        "exact" => matches!(input, Value::Str(_) | Value::Num(_)),
        "fuzzy" | "prefix" | "suffix" | "contains" => matches!(input, Value::Str(_)),
        "lt" | "lte" | "gt" | "gte" => as_double(input).is_some(),
        _ => true,
    };
    if !valid {
        return Err(EsError::illegal_argument(format!("Input [{}] is not valid for CriteriaType [{ty}]", input.to_es_string())));
    }
    let (si, sv) = (input.as_str(), value.as_str());
    Ok(match ty {
        "always" => true,
        "exact" => match (si, sv) {
            (Some(a), Some(b)) => a == b,
            _ => as_double(input).zip(as_double(value)).is_some_and(|(a, b)| a == b),
        },
        "fuzzy" => si.zip(sv).is_some_and(|(a, b)| levenshtein_similarity(a, b) > 0.5),
        "prefix" => si.zip(sv).is_some_and(|(a, b)| a.starts_with(b)),
        "suffix" => si.zip(sv).is_some_and(|(a, b)| a.ends_with(b)),
        "contains" => si.zip(sv).is_some_and(|(a, b)| a.contains(b)),
        _ => {
            let (a, b) = (as_double(input).unwrap_or(f64::NAN), as_double(value).unwrap_or(f64::NAN));
            match ty {
                "lt" => a < b,
                "lte" => a <= b,
                "gt" => a > b,
                _ => a >= b,
            }
        }
    })
}

/// Whether every criterion of a rule matches the request's criteria.
pub fn rule_matches(rule: &Value, criteria: &Map) -> EsResult<bool> {
    for c in rule.get("criteria").and_then(Value::as_array).into_iter().flatten() {
        let ty = c.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "always" {
            continue;
        }
        let Some(input) = c.get("metadata").and_then(|m| criteria.get(&m.to_es_string())) else { return Ok(false) };
        let inputs: Vec<&Value> = match input {
            Value::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        let mut any = false;
        'outer: for v in c.get("values").and_then(Value::as_array).into_iter().flatten() {
            for i in &inputs {
                if criterion_matches(ty, i, v)? {
                    any = true;
                    break 'outer;
                }
            }
        }
        if !any {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The `rule` query as the `pinned` (and `bool` for exclusions) query it stands for.
pub fn rewrite(st: &crate::meta::ClusterState, r: &crate::query::specialized::Rule) -> EsResult<crate::query::Query> {
    let mut pinned_ids: Vec<Value> = Vec::new();
    let mut pinned_docs: Vec<Value> = Vec::new();
    let mut excluded_ids: Vec<Value> = Vec::new();
    let mut excluded_docs: Vec<Value> = Vec::new();
    for id in &r.ruleset_ids {
        let Some(rs) = st.metadata.xpack.get(RULESETS, id) else { return Err(EsError::resource_not_found(format!("query ruleset {id} not found"))) };
        for rule in rs.get("rules").and_then(Value::as_array).into_iter().flatten() {
            if !rule_matches(rule, &r.match_criteria)? {
                continue;
            }
            let exclude = rule.get("type").and_then(Value::as_str) == Some("exclude");
            let actions = rule.get("actions").cloned().unwrap_or(Value::Null);
            let (ids, docs) = if exclude { (&mut excluded_ids, &mut excluded_docs) } else { (&mut pinned_ids, &mut pinned_docs) };
            for v in actions.get("ids").and_then(Value::as_array).into_iter().flatten() {
                if !ids.contains(v) {
                    ids.push(v.clone());
                }
            }
            for v in actions.get("docs").and_then(Value::as_array).into_iter().flatten() {
                if !docs.contains(v) {
                    docs.push(v.clone());
                }
            }
        }
    }
    let mut organic = r.organic.to_value();
    if !excluded_ids.is_empty() || !excluded_docs.is_empty() {
        let mut must_not = Vec::new();
        if !excluded_ids.is_empty() {
            must_not.push(obj! { "ids" => obj! { "values" => Value::Arr(excluded_ids.clone()) } });
        }
        for d in &excluded_docs {
            let mut filter = vec![obj! { "ids" => obj! { "values" => crate::arr![d.get("_id").cloned().unwrap_or(Value::Null)] } }];
            if let Some(i) = d.get("_index") {
                filter.push(obj! { "term" => obj! { "_index" => i.clone() } });
            }
            must_not.push(obj! { "bool" => obj! { "filter" => Value::Arr(filter) } });
        }
        organic = obj! { "bool" => obj! { "must" => crate::arr![organic], "must_not" => Value::Arr(must_not) } };
        pinned_ids.retain(|i| !excluded_ids.contains(i));
        pinned_docs.retain(|d| !excluded_docs.contains(d));
    }
    let mut q = if !pinned_ids.is_empty() {
        obj! { "pinned" => obj! { "ids" => Value::Arr(pinned_ids), "organic" => organic } }
    } else if !pinned_docs.is_empty() {
        obj! { "pinned" => obj! { "docs" => Value::Arr(pinned_docs), "organic" => organic } }
    } else {
        organic
    };
    if r.common.boost != 1.0 || r.common.name.is_some() {
        let mut m = Map::new();
        m.insert("filter".into(), crate::arr![q]);
        m.insert("boost".into(), Value::from(r.common.boost as f64));
        if let Some(n) = &r.common.name {
            m.insert("_name".into(), n.clone().into());
        }
        q = obj! { "bool" => Value::Obj(m) };
    }
    crate::query::parse_query(&q, None, &[]).map(|(q, _)| q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein() {
        assert_eq!(levenshtein_similarity("rescue dogs", "rescue dogs"), 1.0);
        assert!(levenshtein_similarity("rescue dog", "rescue dogs") > 0.5);
        assert!(levenshtein_similarity("cats", "rescue dogs") < 0.5);
    }

    #[test]
    fn criteria() {
        let rule = obj! { "criteria" => crate::arr![
            obj! { "type" => "contains", "metadata" => "user_query", "values" => crate::arr!["pugs", "puggles"] },
            obj! { "type" => "exact", "metadata" => "user_country", "values" => crate::arr!["us"] },
        ] };
        let mut c = Map::new();
        c.insert("user_query".into(), "i like pugs".into());
        c.insert("user_country".into(), "us".into());
        assert!(rule_matches(&rule, &c).unwrap());
        c.insert("user_country".into(), "ca".into());
        assert!(!rule_matches(&rule, &c).unwrap());
        let num = obj! { "criteria" => crate::arr![obj! { "type" => "gte", "metadata" => "n", "values" => crate::arr![5] }] };
        let mut c = Map::new();
        c.insert("n".into(), "7".into());
        assert!(rule_matches(&num, &c).unwrap());
        c.insert("n".into(), "x".into());
        assert!(rule_matches(&num, &c).is_err());
    }

    #[test]
    fn rule_validation() {
        let r = obj! { "rule_id" => "r", "type" => "pinned", "criteria" => crate::arr![obj! { "type" => "always" }], "actions" => obj! {} };
        let e = parse_rule(None, &[], &r, None).unwrap_err().error;
        assert_eq!(e.reason(), "Failed to build [query_rule] after last required field arrived");
        let r = obj! { "rule_id" => "r", "type" => "pinned", "criteria" => crate::arr![obj! { "type" => "always" }], "actions" => obj! { "ids" => crate::arr!["a"] } };
        assert!(parse_rule(None, &[], &r, None).is_ok());
    }
}
