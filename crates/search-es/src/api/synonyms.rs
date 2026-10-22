//! The synonyms API (`/_synonyms`): synonym sets and rules, with Elasticsearch's parsing,
//! validation, paging and reload reporting.
//!
//! Sets live in the cluster metadata ([`crate::meta::synonyms`]). A change bumps the metadata's
//! `synonyms_version`, which makes every cached analysis chain rebuild on next use; the response
//! reports the reload the way Elasticsearch's `ReloadAnalyzersAction` does (a broadcast over the
//! open indices, listing those whose updateable synonym filters use the set). With
//! `refresh=false` the reload report is left out.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, key_location, locate};
use crate::json::{Map, Value};
use crate::meta::ClusterState;
use crate::meta::names::time_based_id;
use crate::meta::synonyms::{self, SynonymSet};
use crate::obj;
use crate::rest::{Ctx, Reply};
use std::sync::Arc;

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

struct Parser<'a> {
    raw: &'a [u8],
}

/// A parsed rule (`id`, `synonyms`), or the error with where the enclosing parser reports it.
type RuleResult = Result<(Option<String>, String), (EsError, (usize, usize))>;

impl Parser<'_> {
    fn at(&self, path: &[Seg]) -> (usize, usize) {
        locate(self.raw, path, Target::Value).unwrap_or((1, 1))
    }

    fn xparse(&self, (l, c): (usize, usize), msg: String) -> EsError {
        EsError::x_content_parse(l, c, msg)
    }

    /// The location just past a value (Elasticsearch reports a failed field there).
    fn after(&self, path: &[Seg]) -> (usize, usize) {
        let (l, c) = self.at(path);
        // Find the value's last byte on its line: good enough for single-line bodies, which is
        // what clients send; multi-line bodies report the value's start.
        let line_start = self.raw.split(|b| *b == b'\n').take(l - 1).map(|s| s.len() + 1).sum::<usize>();
        let start = line_start + c - 1;
        match end_of_value(self.raw, start) {
            Some(end) if !self.raw[start..end].contains(&b'\n') => (l, c + (end - start) - 1),
            _ => (l, c),
        }
    }

    /// `SynonymRule.PARSER` on one object; `parser` names it in messages. Errors carry the
    /// location of the token the parser stopped at, where an enclosing parser reports them.
    fn rule(&self, parser: &str, v: &Value, path: &[Seg], allow_id: bool) -> RuleResult {
        let Value::Obj(m) = v else {
            let at = self.at(path);
            return Err((self.xparse(at, format!("[{parser}] Expected START_OBJECT but was: {}", token_name(v))), at));
        };
        let (mut id, mut syn) = (None, None);
        for (k, fv) in m {
            let mut p = path.to_vec();
            p.push(Seg::Key(k));
            let value_at = self.at(&p);
            let unsupported = |field: &str, other: &Value| (self.xparse(value_at, format!("[{parser}] {field} doesn't support values of type: {}", token_name(other))), value_at);
            match k.as_str() {
                "id" if allow_id => match fv {
                    Value::Str(s) => id = Some(s.clone()),
                    other => return Err(unsupported("id", other)),
                },
                "synonyms" => match fv {
                    Value::Str(s) => syn = Some(s.clone()),
                    other => return Err(unsupported("synonyms", other)),
                },
                other => {
                    let loc = key_location(self.raw, path, other).unwrap_or((1, 1));
                    return Err((self.xparse(loc, format!("[{parser}] unknown field [{other}]")), value_at));
                }
            }
        }
        match syn {
            Some(s) => Ok((id, s)),
            None => Err((EsError::illegal_argument("Required [synonyms]"), self.after(path))),
        }
    }

    /// `PutSynonymsAction.Request` content: `{"synonyms_set": [rule, …]}` (or one rule object).
    fn set(&self, body: &Value) -> EsResult<Vec<(Option<String>, String)>> {
        let Value::Obj(m) = body else {
            return Err(self.xparse((1, 1), format!("[synonyms_set] Expected START_OBJECT but was: {}", token_name(body))));
        };
        let mut rules = None;
        for (k, v) in m {
            match k.as_str() {
                "synonyms_set" => {
                    let path = [Seg::Key("synonyms_set")];
                    let wrap = |(e, at): (EsError, (usize, usize))| self.xparse(at, "[synonyms_set] failed to parse field [synonyms_set]".into()).caused_by(e);
                    let mut out = Vec::new();
                    match v {
                        Value::Obj(_) => out.push(self.rule("synonym_rule", v, &path, true).map_err(wrap)?),
                        Value::Arr(items) => {
                            let mut i = 0;
                            while i < items.len() {
                                if !matches!(items[i], Value::Obj(_)) {
                                    // `ObjectParser` reads one more token when not at an object.
                                    match items.get(i + 1) {
                                        Some(n @ Value::Obj(_)) => {
                                            let np = [Seg::Key("synonyms_set"), Seg::Index(i + 1)];
                                            out.push(self.rule("synonym_rule", n, &np, true).map_err(wrap)?);
                                            i += 2;
                                            continue;
                                        }
                                        next => {
                                            let (at, ty) = match next {
                                                Some(n) => (self.at(&[Seg::Key("synonyms_set"), Seg::Index(i + 1)]), token_name(n)),
                                                None => (self.after(&path), "END_ARRAY"),
                                            };
                                            let e = self.xparse(at, format!("[synonym_rule] Expected START_OBJECT but was: {ty}"));
                                            return Err(wrap((e, at)));
                                        }
                                    }
                                }
                                let p = [Seg::Key("synonyms_set"), Seg::Index(i)];
                                out.push(self.rule("synonym_rule", &items[i], &p, true).map_err(wrap)?);
                                i += 1;
                            }
                        }
                        other => {
                            return Err(self.xparse(self.at(&path), format!("[synonyms_set] synonyms_set doesn't support values of type: {}", token_name(other))));
                        }
                    }
                    rules = Some(out);
                }
                other => {
                    let loc = key_location(self.raw, &[], other).unwrap_or((1, 1));
                    return Err(self.xparse(loc, format!("[synonyms_set] unknown field [{other}]")));
                }
            }
        }
        rules.ok_or_else(|| EsError::illegal_argument("Required [synonyms_set]"))
    }
}

/// The end (exclusive) of the JSON value starting at `start`.
fn end_of_value(s: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = start;
    let mut in_str = false;
    while i < s.len() {
        let b = s[i];
        if in_str {
            match b {
                b'\\' => i += 1,
                b'"' => {
                    in_str = false;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                    if depth < 0 {
                        return Some(i);
                    }
                }
                b',' if depth == 0 => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    Some(s.len())
}

/// Wraps a body parse failure as `PutSynonymsAction.Request` does.
fn failed_to_parse(ctx: &Ctx, cause: EsError) -> EsError {
    EsError::illegal_argument(format!("Failed to parse: {}", String::from_utf8_lossy(&ctx.body))).caused_by(cause)
}

fn parse_body(ctx: &Ctx) -> EsResult<Value> {
    if ctx.body.is_empty() {
        return Err(failed_to_parse(ctx, EsError::java(400, "null_pointer_exception", "").with_null_reason()));
    }
    match ctx.body()? {
        Some(v) => Ok(v),
        None => Err(failed_to_parse(ctx, EsError::java(400, "null_pointer_exception", "").with_null_reason())),
    }
}

fn validate_rules(rules: &[(Option<String>, String)]) -> EsResult<()> {
    let errors: Vec<String> = rules.iter().filter_map(|(_, s)| synonyms::validate_rule(s)).collect();
    if errors.is_empty() { Ok(()) } else { Err(EsError::action_request_validation(&errors)) }
}

fn max_rules(st: &ClusterState) -> i64 {
    let md = &st.metadata;
    md.transient
        .get("synonyms.max_synonym_rules")
        .or_else(|| md.persistent.get("synonyms.max_synonym_rules"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(synonyms::DEFAULT_MAX_RULES)
}

fn check_count(st: &ClusterState, count: usize) -> EsResult<()> {
    let max = max_rules(st);
    if count as i64 > max {
        return Err(EsError::illegal_argument(format!("The number of synonym rules in a synonym set cannot exceed {max}")));
    }
    Ok(())
}

fn set_not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("synonyms set [{id}] not found"))
}

/// The update response: `result`, then the reload report unless `refresh=false`.
fn update_reply(ctx: &Ctx, result: &str, set: &str, refresh: bool) -> EsResult<Reply> {
    let status = if result == "created" { 201 } else { 200 };
    let mut m = Map::new();
    m.insert("result".into(), Value::from(result));
    if refresh {
        m.insert("reload_analyzers_details".into(), synonyms::reload_details(&ctx.state(), set, &ctx.cluster.node.id));
    }
    Ok(Reply::Body(status, Value::Obj(m)))
}

/// The system index Elasticsearch keeps synonym sets in. roll keeps them in the cluster metadata,
/// but creates the index (hidden, `system`, aliased `.synonyms`) the way Elasticsearch does on
/// the first set, so feature-state tooling (`_features/_reset`, test cluster cleanup) and index
/// listings see what they see on Elasticsearch.
pub const SYSTEM_INDEX: &str = ".synonyms-2";
const SYSTEM_ALIAS: &str = ".synonyms";

fn ensure_system_index(st: &mut ClusterState, now_ms: i64) -> EsResult<()> {
    if st.metadata.indices.contains_key(SYSTEM_INDEX) {
        return Ok(());
    }
    let mut req = super::indices::CreateRequest::default();
    for (k, v) in [("index.hidden", "true"), ("index.number_of_shards", "1"), ("index.auto_expand_replicas", "0-1"), ("index.format", "2")] {
        req.settings.set(k, v);
    }
    let kw = || obj! { "type" => "keyword" };
    if let Value::Obj(m) = obj! {
        "dynamic" => "strict",
        "_meta" => obj! { "managed_index_mappings_version" => 1, "version" => crate::version::ES_VERSION },
        "properties" => obj! { "id" => kw(), "synonyms" => obj! { "type" => "match_only_text" }, "synonyms_set" => kw(), "type" => kw() },
    } {
        req.mappings = Some(m);
    }
    req.aliases.push((
        SYSTEM_ALIAS.to_string(),
        crate::meta::AliasMeta { is_write_index: Some(true), is_hidden: Some(true), ..Default::default() },
    ));
    super::indices::create_index(st, SYSTEM_ALIAS, SYSTEM_INDEX, req, now_ms).map(|_| ())
}

fn bump(st: &mut ClusterState) -> &mut synonyms::Sets {
    st.metadata.synonyms_version += 1;
    Arc::make_mut(&mut st.metadata.synonyms)
}

pub async fn put_set(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let refresh = ctx.bool_param("refresh", true)?;
    let append = ctx.bool_param("append", false)?;
    let body = parse_body(ctx)?;
    let parser = Parser { raw: ctx.raw_json().unwrap_or(b"") };
    let rules = parser.set(&body).map_err(|e| failed_to_parse(ctx, e))?;
    validate_rules(&rules)?;
    let set_id = id.clone();
    let now = ctx.now_ms;
    let created = ctx
        .cluster
        .update(move |st| {
            ensure_system_index(st, now)?;
            let existing = st.metadata.synonyms.get(&set_id).cloned();
            let created = existing.is_none();
            // Appending counts every incoming rule as new (Elasticsearch's best-effort check).
            let count = if append { existing.as_ref().map_or(0, |s| s.rules.len()) + rules.len() } else { rules.len() };
            check_count(st, count)?;
            let mut set = if append { existing.map(|s| (*s).clone()).unwrap_or_default() } else { SynonymSet::default() };
            for (rid, text) in rules {
                set.rules.insert(rid.unwrap_or_else(time_based_id), text);
            }
            bump(st).insert(set_id, Arc::new(set));
            Ok(created)
        })
        .await?;
    update_reply(ctx, if created { "created" } else { "updated" }, &id, refresh)
}

/// `from`/`size` paging parameters, validated like `AbstractSynonymsPagedResultAction`.
fn paging(ctx: &Ctx) -> EsResult<(i64, i64)> {
    let from = ctx.int_param("from", 0)?;
    let size = ctx.int_param("size", 10)?;
    let mut errors = Vec::new();
    for (name, v) in [("from", from), ("size", size)] {
        if v < 0 {
            errors.push(format!("[{name}] must be a positive integer"));
        } else if v > synonyms::MAX_RESULTS {
            errors.push(format!("[{name}] must be less than or equal to {}", synonyms::MAX_RESULTS));
        }
    }
    if from + size > synonyms::MAX_RESULTS {
        errors.push(format!("Too many results to retrieve. [from] + [size] must be less than or equal to {}", synonyms::MAX_RESULTS));
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    Ok((from, size))
}

pub fn get_set(ctx: &Ctx) -> EsResult<Reply> {
    if ctx.has_query("from") && ctx.has_query("search_after") {
        return Err(EsError::illegal_argument("[from] and [search_after] cannot be used together"));
    }
    let (from, size) = paging(ctx)?;
    let id = ctx.path_param("id").unwrap_or_default();
    let st = ctx.state();
    let set = st.metadata.synonyms.get(id).ok_or_else(|| set_not_found(id))?;
    let page: Vec<Value> = match ctx.query("search_after") {
        Some(after) => set
            .rules
            .range::<str, _>((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .take(size as usize)
            .map(|(rid, s)| obj! { "id" => rid.clone(), "synonyms" => s.clone() })
            .collect(),
        None => set.rules.iter().skip(from as usize).take(size as usize).map(|(rid, s)| obj! { "id" => rid.clone(), "synonyms" => s.clone() }).collect(),
    };
    Reply::ok(obj! { "count" => set.rules.len(), "synonyms_set" => Value::Arr(page) })
}

pub async fn delete_set(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            let users = synonyms::indices_using(st, &id);
            if !users.is_empty() {
                return Err(EsError::illegal_argument(format!(
                    "synonyms set [{id}] cannot be deleted as it is used in the following indices: {}",
                    users.join(", ")
                )));
            }
            if !st.metadata.synonyms.contains_key(&id) {
                return Err(set_not_found(&id));
            }
            bump(st).remove(&id);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn list_sets(ctx: &Ctx) -> EsResult<Reply> {
    let (from, size) = paging(ctx)?;
    let st = ctx.state();
    let sets = &st.metadata.synonyms;
    let results: Vec<Value> = sets
        .iter()
        .skip(from as usize)
        .take(size as usize)
        .map(|(name, s)| obj! { "synonyms_set" => name.clone(), "count" => s.rules.len() })
        .collect();
    Reply::ok(obj! { "count" => sets.len(), "results" => Value::Arr(results) })
}

pub async fn put_rule(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let set_id = ctx.path_param("set_id").unwrap_or_default().to_string();
    let rule_id = ctx.path_param("rule_id").unwrap_or_default().to_string();
    let refresh = ctx.bool_param("refresh", true)?;
    let body = parse_body(ctx)?;
    let parser = Parser { raw: ctx.raw_json().unwrap_or(b"") };
    let (_, text) = parser.rule("synonyms", &body, &[], false).map_err(|(e, _)| failed_to_parse(ctx, e))?;
    validate_rules(&[(None, text.clone())])?;
    let sid = set_id.clone();
    let created = ctx
        .cluster
        .update(move |st| {
            let set = st.metadata.synonyms.get(&sid).cloned().ok_or_else(|| set_not_found(&sid))?;
            let others = set.rules.len() - usize::from(set.rules.contains_key(&rule_id));
            check_count(st, others + 1)?;
            let mut set = (*set).clone();
            let created = set.rules.insert(rule_id, text).is_none();
            bump(st).insert(sid, Arc::new(set));
            Ok(created)
        })
        .await?;
    update_reply(ctx, if created { "created" } else { "updated" }, &set_id, refresh)
}

pub fn get_rule(ctx: &Ctx) -> EsResult<Reply> {
    let set_id = ctx.path_param("set_id").unwrap_or_default();
    let rule_id = ctx.path_param("rule_id").unwrap_or_default();
    let st = ctx.state();
    let set = st.metadata.synonyms.get(set_id).ok_or_else(|| set_not_found(set_id))?;
    let text = set.rules.get(rule_id).ok_or_else(|| EsError::resource_not_found(format!("synonym rule [{rule_id}] not found")))?;
    Reply::ok(obj! { "id" => rule_id, "synonyms" => text.clone() })
}

pub async fn delete_rule(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let set_id = ctx.path_param("set_id").unwrap_or_default().to_string();
    let rule_id = ctx.path_param("rule_id").unwrap_or_default().to_string();
    let refresh = ctx.bool_param("refresh", true)?;
    let sid = set_id.clone();
    ctx.cluster
        .update(move |st| {
            let set = st.metadata.synonyms.get(&sid).cloned().ok_or_else(|| set_not_found(&sid))?;
            if !set.rules.contains_key(&rule_id) {
                return Err(EsError::resource_not_found(format!("synonym rule [{rule_id}] not found on synonyms set [{sid}]")));
            }
            let mut set = (*set).clone();
            set.rules.remove(&rule_id);
            bump(st).insert(sid, Arc::new(set));
            Ok(())
        })
        .await?;
    update_reply(ctx, "deleted", &set_id, refresh)
}
