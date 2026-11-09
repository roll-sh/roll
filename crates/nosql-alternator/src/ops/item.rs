//! Single-item operations: PutItem, GetItem, DeleteItem, UpdateItem.

use std::sync::Mutex;

use super::*;
use crate::backend::{Mutation, WriteFn};
use crate::expr::eval::{Evaluator, RCond, resolve_cond};
use crate::expr::cache;
use crate::expr::project::project;
use crate::expr::resolve::{Bindings, PathSet, RPath, check_attr_name, check_size};
use crate::expr::update::{self as upd, RAction, RUpdate};
use crate::legacy::{self, AttrUpdate};
use crate::ops::tags::write_isolation;
use crate::service::WriteIsolation;
use crate::size;
use crate::value::Item;

pub(crate) const FORBID_RMW: &str = "Read-modify-write operations are disabled by 'forbid_rmw' write isolation policy. Refer to https://github.com/scylladb/scylla/blob/master/docs/alternator/alternator.md#write-isolation-policies for more information.";

/// A parse error; ScyllaDB builds the message as a C string, so a NUL in the expression ends it.
pub(crate) fn parse_error(param: &str, src: &str, reason: &str) -> Error {
    let msg = format!("Failed parsing {param} '{src}': {reason}");
    Error::validation(msg.split('\0').next().unwrap_or_default())
}

/// A string expression parameter: `None` when absent.
pub(crate) fn expression_param<'a>(body: &'a Json, name: &str) -> Result<Option<&'a str>> {
    match body.get(name) {
        None => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        Some(_) => Err(Error::validation(format!("{name} must be a string"))),
    }
}

/// Parses and resolves a condition-language parameter (`ConditionExpression`,
/// `FilterExpression`).
pub(crate) fn condition_param(src: &str, param: &str, b: &mut Bindings) -> Result<RCond> {
    if src.is_empty() {
        return Err(Error::validation(format!("{param} must not be empty")));
    }
    check_size(param, src)?;
    let c = cache::condition(src).map_err(|e| parse_error(param, src, &e.reason()))?;
    resolve_cond(&c, b, "ConditionExpression")
}

/// The condition of a write: ConditionExpression and/or Expected (both must hold).
fn write_condition(
    body: &Json,
    b: &mut Bindings,
    op: &str,
    update_expression: bool,
) -> Result<RCond> {
    let cond = match expression_param(body, "ConditionExpression")? {
        Some(src) => Some(condition_param(src, "ConditionExpression", b)?),
        None => {
            if !update_expression {
                if b.names.is_some() {
                    return Err(Error::validation(
                        "ExpressionAttributeNames cannot be used without ConditionExpression",
                    ));
                }
                if b.values.is_some() {
                    return Err(Error::validation(
                        "ExpressionAttributeValues cannot be used without ConditionExpression",
                    ));
                }
            }
            None
        }
    };
    let _ = op;
    let expected = legacy::parse_expected(body)?;
    Ok(match (cond, expected) {
        (Some(c), Some(e)) => RCond::And(vec![c, RCond::Legacy(e)]),
        (Some(c), None) => c,
        (None, Some(e)) => RCond::Legacy(e),
        (None, None) => RCond::True,
    })
}

/// Whether a write has a condition (an `Expected` parameter counts, even empty).
fn has_condition(c: &RCond) -> bool {
    !matches!(c, RCond::True)
}

/// Checks an item's attributes (in request order, first duplicate wins) and builds it.
fn build_item(obj: &Json, schema: &TableSchema, op: &str) -> Result<Item> {
    let mut item = Item::new();
    for (name, v) in obj.as_object().unwrap_or_default() {
        if item.contains_key(name) {
            continue;
        }
        if !schema.key.contains(name) {
            value::validate(v, op)?;
            if let Err(e) = check_attr_name(name) {
                return Err(Error::validation(e));
            }
            check_index_key(schema, name, v)?;
        }
        item.insert(name.clone(), v.clone());
    }
    super::vector::check_item(schema, &item, op)?;
    Ok(item)
}

/// A value written to an index key attribute must have the index key's type.
pub(crate) fn check_index_key(schema: &TableSchema, name: &str, v: &Json) -> Result<()> {
    for idx in schema.gsis.iter().chain(&schema.lsis) {
        if let Some(a) = idx.key.attr(name) {
            let t = value::type_tag(v).unwrap_or("");
            if t != a.ty.name() {
                return Err(Error::validation(format!(
                    "Type mismatch: expected type {} for GSI or LSI key attribute {name}, got type {t}",
                    a.ty.name()
                )));
            }
            if single_member(v)
                .and_then(|(_, p)| p.as_str())
                .is_some_and(str::is_empty)
            {
                return Err(Error::validation(format!(
                    "The AttributeValue for a key attribute cannot contain an empty string value. Key: {name}"
                )));
            }
        }
    }
    Ok(())
}

/// A write's outcome, noting a failed condition for the metrics.
pub(crate) fn written<T>(req: &Request<'_>, schema: &TableSchema, r: Result<T>) -> Result<T> {
    if let Err(e) = &r
        && e.kind == crate::error::ErrorKind::ConditionalCheckFailed
    {
        req.stats.table(&schema.name);
        req.stats.condition_failed(&schema.name);
    }
    r
}

/// The `ConditionalCheckFailedException`, carrying the old item when asked for.
fn condition_failed(old: Option<&Item>, return_old: bool) -> Error {
    let mut e = Error::condition_failed();
    if return_old && let Some(o) = old {
        e.item = Some(item_json(o));
    }
    e
}

/// `forbid_rmw` rejects requests that need the item's current value.
fn check_rmw(alt: &Alternator, schema: &TableSchema, needs_read: bool) -> Result<()> {
    if needs_read && write_isolation(alt, schema) == WriteIsolation::ForbidRmw {
        return Err(Error::validation(FORBID_RMW));
    }
    Ok(())
}

fn with_capacity(mut out: Json, rcc: bool, units: f64) -> Json {
    if rcc {
        out.push("ConsumedCapacity", capacity(units));
    }
    out
}

pub async fn put_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    if let Some(r) = super::internal::refuse_write(alt, req).await? {
        return Ok(r);
    }
    let schema = get_table(alt, body).await?;
    authorize_table(alt, req, crate::backend::Permission::Modify, &schema.name).await?;
    let rcc = return_consumed_capacity(body)?;
    let rv = return_values(body, "PutItem", false)?;
    let rvf = return_values_on_failure(body)?;
    let Some(obj) = body.get("Item") else {
        return Err(Error::validation("JSON parameter Item not found"));
    };
    if !obj.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let key = primary_key(obj, &schema, false)?;
    let item = build_item(obj, &schema, "PutItem")?;
    let mut b = Bindings::new(body);
    let cond = write_condition(body, &mut b, "PutItem", false)?;
    b.check_unused("PutItem")?;
    let reads = has_condition(&cond) || rv == ReturnValues::AllOld;
    check_rmw(alt, &schema, reads)?;
    let new_size = size::item_size(&item);
    check_key_length(&key)?;
    let f: WriteFn = Box::new(move |old: Option<&Item>| {
        if !Evaluator::new(old).eval(&cond)? {
            return Err(condition_failed(old, rvf));
        }
        Ok(Some(Mutation::Put { item, rmw: reads }))
    });
    let out = written(req, &schema, alt.backend.write_item(&schema, &key, f).await)?;
    let mut resp = Json::object();
    if rv == ReturnValues::AllOld
        && let Some(o) = &out.old
    {
        resp.push("Attributes", item_json(o));
    }
    let sz = if reads {
        new_size.max(out.old.as_ref().map_or(0, size::item_size))
    } else {
        new_size
    };
    super::note_write(alt, req, &schema, reads, size::write_units(sz));
    req.stats.item_size(&schema.name, new_size);
    Ok(with_capacity(resp, rcc, size::write_units(sz) as f64))
}

/// ProjectionExpression or AttributesToGet of a read: the paths to return (`None`: all).
pub(crate) fn projection(body: &Json, b: &mut Bindings, op: &str) -> Result<Option<Vec<RPath>>> {
    let pe = match body.get("ProjectionExpression") {
        None => None,
        Some(Json::String(s)) => Some(s.as_str()),
        Some(v) => {
            return Err(Error::internal(format!(
                "Internal server error: std::runtime_error (rjson::to_string_view expects a string value, got: {})",
                rapid_type(v)
            )));
        }
    };
    let atg = body.get("AttributesToGet");
    if pe.is_some() && atg.is_some() {
        return Err(Error::validation(format!(
            "{op} does not allow both ProjectionExpression and AttributesToGet to be given together"
        )));
    }
    if let Some(src) = pe {
        check_size("ProjectionExpression", src)?;
        let paths = cache::projection(src)
            .map_err(|e| parse_error("ProjectionExpression", src, &e.reason()))?;
        let mut set = PathSet::default();
        let mut out = Vec::with_capacity(paths.len());
        for p in paths.iter() {
            let r = b.path(p)?;
            if let Err((overlap, at)) = set.insert(&r) {
                let what = if overlap { "overlap" } else { "conflict" };
                return Err(Error::validation(format!(
                    "Invalid ProjectionExpression: two document paths {what} at {at}"
                )));
            }
            out.push(r);
        }
        return Ok(Some(out));
    }
    if let Some(a) = atg {
        let names = legacy::attributes_to_get(a)?;
        return Ok(Some(names.into_iter().map(RPath::top).collect()));
    }
    Ok(None)
}

pub(crate) use super::rapid_type;

pub async fn get_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let schema = get_table(alt, body).await?;
    authorize_table(alt, req, crate::backend::Permission::Select, &schema.name).await?;
    let consistent = consistent_read(body)?;
    let Some(k) = body.get("Key") else {
        return Err(json_assert("IsObject()"));
    };
    if !k.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let key = primary_key(k, &schema, true)?;
    let rcc = return_consumed_capacity(body)?;
    let mut b = Bindings::new(body);
    let paths = projection(body, &mut b, "GetItem")?;
    b.mark_all_values_used();
    b.check_unused("GetItem")?;
    check_key_length(&key)?;
    let item = alt.backend.get_item(&schema, &key).await?;
    let units = size::read_units(item.as_ref().map(size::item_size), consistent);
    req.stats.table(&schema.name);
    req.stats.rcu(&schema.name, units);
    req.stats.storage(&schema.name, false);
    if let Some(it) = &item {
        req.stats.item_size(&schema.name, size::item_size(it));
    }
    let mut resp = Json::object();
    if let Some(it) = item {
        let it = match &paths {
            Some(p) => project(&it, p),
            None => it,
        };
        resp.push("Item", item_json(&it));
    }
    Ok(with_capacity(resp, rcc, units))
}

pub async fn delete_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    if let Some(r) = super::internal::refuse_write(alt, req).await? {
        return Ok(r);
    }
    let schema = get_table(alt, body).await?;
    authorize_table(alt, req, crate::backend::Permission::Modify, &schema.name).await?;
    let rcc = return_consumed_capacity(body)?;
    let rv = return_values(body, "DeleteItem", false)?;
    let rvf = return_values_on_failure(body)?;
    let Some(k) = body.get("Key") else {
        return Err(Error::validation("JSON parameter Key not found"));
    };
    if !k.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let key = primary_key(k, &schema, true)?;
    let mut b = Bindings::new(body);
    let cond = write_condition(body, &mut b, "DeleteItem", false)?;
    b.check_unused("DeleteItem")?;
    let reads = has_condition(&cond) || rv == ReturnValues::AllOld;
    check_rmw(alt, &schema, reads)?;
    check_key_length(&key)?;
    let f: WriteFn = Box::new(move |old: Option<&Item>| {
        if !Evaluator::new(old).eval(&cond)? {
            return Err(condition_failed(old, rvf));
        }
        Ok(Some(Mutation::Delete))
    });
    let out = written(req, &schema, alt.backend.write_item(&schema, &key, f).await)?;
    let mut resp = Json::object();
    if rv == ReturnValues::AllOld
        && let Some(o) = &out.old
    {
        resp.push("Attributes", item_json(o));
    }
    let sz = if reads {
        out.old.as_ref().map_or(0, size::item_size)
    } else {
        0
    };
    super::note_write(alt, req, &schema, reads, size::write_units(sz));
    // The deleted item's size is only known when the item was read.
    if (reads || alt.config().force_read_before_write)
        && let Some(o) = &out.old
    {
        req.stats.item_size(&schema.name, size::item_size(o));
    }
    Ok(with_capacity(resp, rcc, size::write_units(sz) as f64))
}

/// What an UpdateItem does.
enum UpdateKind {
    Expression(RUpdate),
    Legacy(Vec<(String, AttrUpdate)>),
    Nothing,
}

impl UpdateKind {
    /// Paths an update touches (for UPDATED_OLD / UPDATED_NEW).
    fn paths(&self) -> Vec<RPath> {
        match self {
            UpdateKind::Expression(u) => u.actions.iter().map(|a| a.path().clone()).collect(),
            UpdateKind::Legacy(l) => l.iter().map(|(n, _)| RPath::top(n.clone())).collect(),
            UpdateKind::Nothing => Vec::new(),
        }
    }

    fn reads_item(&self) -> bool {
        match self {
            UpdateKind::Expression(u) => u.actions.iter().any(RAction::reads_item),
            UpdateKind::Legacy(l) => l
                .iter()
                .any(|(_, u)| matches!(u, AttrUpdate::Add(_) | AttrUpdate::Delete(Some(_)))),
            UpdateKind::Nothing => false,
        }
    }

    /// Whether an update of a missing item creates it.
    fn creates(&self) -> bool {
        match self {
            UpdateKind::Expression(u) => u
                .actions
                .iter()
                .any(|a| matches!(a, RAction::Set(..) | RAction::Add(..))),
            UpdateKind::Legacy(l) => {
                l.is_empty()
                    || l.iter()
                        .any(|(_, u)| matches!(u, AttrUpdate::Put(_) | AttrUpdate::Add(_)))
            }
            UpdateKind::Nothing => true,
        }
    }
}

pub async fn update_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    if let Some(r) = super::internal::refuse_write(alt, req).await? {
        return Ok(r);
    }
    let schema = get_table(alt, body).await?;
    authorize_table(alt, req, crate::backend::Permission::Modify, &schema.name).await?;
    let rv = return_values(body, "UpdateItem", true)?;
    let rvf = return_values_on_failure(body)?;
    let rcc = return_consumed_capacity(body)?;
    let Some(k) = body.get("Key") else {
        return Err(Error::validation("UpdateItem requires a Key parameter"));
    };
    if !k.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let key = primary_key(k, &schema, true)?;
    let ue = expression_param(body, "UpdateExpression")?;
    let parsed = match ue {
        Some(src) => {
            check_size("UpdateExpression", src)?;
            Some(cache::update(src).map_err(|e| parse_error("UpdateExpression", src, &e.reason()))?)
        }
        None => None,
    };
    let au = body.get("AttributeUpdates");
    if ue.is_some() && au.is_some() {
        return Err(Error::validation(
            "UpdateItem does not allow both AttributeUpdates and UpdateExpression to be given together",
        ));
    }
    if au.is_some() && body.get("ConditionExpression").is_some() {
        return Err(Error::validation(
            "UpdateItem does not allow both old-style AttributeUpdates and new-style ConditionExpression to be given together",
        ));
    }
    if ue.is_some() && body.get("Expected").is_some() {
        return Err(Error::validation(
            "UpdateItem does not allow both old-style Expected and new-style UpdateExpression to be given together",
        ));
    }
    let legacy_updates = au.map(legacy::parse_attribute_updates).transpose()?;
    let mut b = Bindings::new(body);
    let kind = if let Some(u) = &parsed {
        let r = upd::resolve(u, &mut b)?;
        upd::check_paths(&r)?;
        UpdateKind::Expression(r)
    } else if let Some(l) = legacy_updates {
        UpdateKind::Legacy(l)
    } else {
        UpdateKind::Nothing
    };
    // The ConditionExpression's syntax is checked before the key columns.
    let ce = expression_param(body, "ConditionExpression")?;
    if let Some(src) = ce {
        if src.is_empty() {
            return Err(Error::validation("ConditionExpression must not be empty"));
        }
        check_size("ConditionExpression", src)?;
        cache::check_condition(src)
            .map_err(|e| parse_error("ConditionExpression", src, &e.reason()))?;
    }
    for p in kind.paths() {
        if schema.key.contains(&p.root) {
            return Err(Error::validation(format!(
                "UpdateItem cannot update key column {}",
                p.root
            )));
        }
    }
    let cond = write_condition(body, &mut b, "UpdateItem", true)?;
    b.check_unused("UpdateItem")?;
    let reads = has_condition(&cond)
        || matches!(
            rv,
            ReturnValues::AllOld | ReturnValues::UpdatedOld | ReturnValues::AllNew
        )
        || kind.reads_item();
    check_rmw(alt, &schema, reads)?;
    check_key_length(&key)?;
    let key_attrs = key_item(&schema, &key);
    let paths = kind.paths();
    let creates = kind.creates();
    let reads_item = kind.reads_item();
    let touched: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for p in &paths {
            if !v.contains(&p.root) {
                v.push(p.root.clone());
            }
        }
        v
    };
    let rmw = reads;
    let schema2 = schema.clone();
    let result: std::sync::Arc<Mutex<Option<Item>>> = Default::default();
    let result2 = result.clone();
    let f: WriteFn = Box::new(move |old: Option<&Item>| {
        if !Evaluator::new(old).eval(&cond)? {
            return Err(condition_failed(old, rvf));
        }
        let mut new = old.cloned().unwrap_or_else(|| key_attrs.clone());
        match &kind {
            UpdateKind::Expression(u) => upd::apply(u, old, &mut new)?,
            UpdateKind::Legacy(l) => legacy::apply_attribute_updates(l, &mut new)?,
            UpdateKind::Nothing => {}
        }
        let sets = creates;
        let stored = old.is_some() || sets;
        for (name, v) in &new {
            if old.and_then(|o| o.get(name)) != Some(v) {
                check_index_key(&schema2, name, v)?;
            }
        }
        // Only attributes the update changed are checked against vector indexes.
        let changed: Item = new
            .iter()
            .filter(|(k, v)| old.and_then(|o| o.get(*k)) != Some(*v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        super::vector::check_item(&schema2, &changed, "UpdateItem")?;
        if let Ok(mut r) = result2.lock() {
            *r = stored.then(|| new.clone());
        }
        Ok(Some(Mutation::Update {
            item: new,
            sets,
            reads: reads_item,
            rmw,
            touched,
        }))
    });
    let out = written(req, &schema, alt.backend.write_item(&schema, &key, f).await)?;
    let new = result.lock().ok().and_then(|mut r| r.take());
    let attrs = match rv {
        ReturnValues::None => None,
        ReturnValues::AllOld => out.old.clone(),
        ReturnValues::AllNew => new.clone(),
        ReturnValues::UpdatedOld => out.old.as_ref().map(|o| project(o, &paths)),
        ReturnValues::UpdatedNew => new.as_ref().map(|n| project(n, &paths)),
    };
    let mut resp = Json::object();
    if let Some(a) = attrs.filter(|a| !a.is_empty()) {
        resp.push("Attributes", item_json(&a));
    }
    let request = request_size(body, &key, &schema);
    let sz = if reads {
        request.max(out.old.as_ref().map_or(0, size::item_size))
    } else {
        request
    };
    super::note_write(alt, req, &schema, reads, size::write_units(sz));
    req.stats.item_size(&schema.name, request);
    Ok(with_capacity(resp, rcc, size::write_units(sz) as f64))
}

/// The size ScyllaDB charges an UpdateItem with, before considering the old item: the key
/// values plus the expression's names and values.
fn request_size(body: &Json, key: &crate::backend::PrimaryKey, _schema: &TableSchema) -> usize {
    fn j(v: &Json) -> usize {
        match v {
            Json::Object(m) => m.iter().map(|(k, x)| k.len() + j(x)).sum(),
            Json::Array(a) => a.iter().map(j).sum(),
            Json::String(s) => s.len(),
            Json::Bool(_) => 5,
            Json::Number(n) => n.text().len(),
            Json::Null => 0,
        }
    }
    let mut n = size::value_size(&key.hash, true)
        + key.range.as_ref().map_or(0, |r| size::value_size(r, true));
    if let Some(Json::Object(m)) = body.get("ExpressionAttributeValues") {
        n += m.iter().map(|(k, v)| k.len() + j(v)).sum::<usize>();
    }
    if let Some(Json::Object(m)) = body.get("ExpressionAttributeNames") {
        n += m
            .iter()
            .map(|(k, v)| k.len() + v.as_str().map_or(0, str::len))
            .sum::<usize>();
    }
    n
}
