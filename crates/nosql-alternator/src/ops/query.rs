//! Query and Scan.
//!
//! Both read a page of rows from the backend (at most `Limit` rows and about 1 MB), filter it
//! (FilterExpression / QueryFilter / ScanFilter: `Count` vs `ScannedCount`), project it, and
//! return the last row read as `LastEvaluatedKey` whenever the page stopped early. A page that
//! ends exactly at the last row still carries a `LastEvaluatedKey`, as in ScyllaDB.

use super::*;
use crate::backend::{RangeRequest, Scope, SortCondition};
use crate::expr::ast::{CmpOp, Cond, Operand};
use crate::expr::eval::{Evaluator, RCond};
use crate::expr::cache;
use crate::expr::project::project;
use crate::expr::resolve::{Bindings, RPath, check_size};
use crate::legacy::{self, Conditions, Op};
use crate::ops::item::{condition_param, expression_param, projection};
use crate::schema::{IndexKind, IndexSchema, KeySchema};
use crate::value::Item;

/// The byte budget of one page.
pub const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Select {
    All,
    AllProjected,
    Specific,
    Count,
}

/// Where a read goes: the table or one of its indexes.
struct Target {
    schema: Arc<TableSchema>,
    index: Option<IndexSchema>,
    /// The rows of an internal table (`.scylla.alternator.KS.T`).
    internal: Option<Arc<crate::backend::InternalTable>>,
}

impl Target {
    fn key(&self) -> &KeySchema {
        self.index.as_ref().map_or(&self.schema.key, |i| &i.key)
    }

    /// The attributes that make up a position (LastEvaluatedKey): index keys then table keys.
    fn position_attrs(&self) -> Vec<KeyAttr> {
        if let Some(t) = &self.internal {
            return t.key.clone();
        }
        let mut v: Vec<KeyAttr> = Vec::new();
        for a in self
            .key()
            .names()
            .chain(self.schema.key.names())
            .filter_map(|n| self.key().attr(n).or_else(|| self.schema.key.attr(n)))
        {
            if !v.iter().any(|x| x.name == a.name) {
                v.push(a.clone());
            }
        }
        v
    }
}

/// [`target`], after checking the SELECT permission on it (its view, for an index).
async fn read_target(alt: &Alternator, req: &Request<'_>) -> Result<Target> {
    let t = target(alt, req.body).await?;
    let on = match (&t.internal, &t.index) {
        (Some(_), _) => {
            let name = table_name(req.body)?;
            let rest = name.strip_prefix(INTERNAL_PREFIX).unwrap_or(name);
            let (ks, table) = rest.split_once('.').unwrap_or((rest, ""));
            crate::backend::Resource::Table {
                keyspace: ks.to_string(),
                table: table.to_string(),
            }
        }
        (None, Some(i)) => {
            crate::backend::Resource::of(&t.schema.name, &i.view_name(&t.schema.name))
        }
        (None, None) => crate::backend::Resource::of(&t.schema.name, &t.schema.name),
    };
    super::authorize(alt, req, crate::backend::Permission::Select, on).await?;
    Ok(t)
}

async fn target(alt: &Alternator, body: &Json) -> Result<Target> {
    let name = table_name(body)?;
    if let Some(t) = super::internal::resolve(alt, name).await? {
        let Some(schema) = super::internal::schema_of(name, &t) else {
            return Err(table_not_found(name));
        };
        return Ok(Target {
            schema: Arc::new(schema),
            index: None,
            internal: Some(Arc::new(t)),
        });
    }
    let index = match body.get("IndexName") {
        None => None,
        Some(Json::String(s)) => {
            if s.len() < 3 {
                return Err(Error::validation(
                    "IndexName must be at least 3 characters long",
                ));
            }
            if !valid_name_chars(s) {
                return Err(Error::validation(format!(
                    "IndexName '{s}' must satisfy regular expression pattern: [a-zA-Z0-9_.-]+"
                )));
            }
            if name.len() + s.len() > 221 {
                return Err(Error::validation(format!(
                    "The total length of TableName ('{name}') and IndexName ('{s}') cannot exceed 221 characters"
                )));
            }
            Some(s.as_str())
        }
        Some(_) => {
            return Err(Error::internal(
                "Internal server error: std::runtime_error (rjson::to_string_view expects a string value, got: kNumberType)",
            ));
        }
    };
    let schema = lookup_table(alt, name).await?;
    super::vector::reject_index(&schema, body)?;
    let index = match index {
        None => None,
        Some(i) => match schema.index(i) {
            Some(x) => Some(x.clone()),
            None => {
                return Err(Error::validation(format!(
                    "Requested resource not found: Index '{i}' for table '{name}'"
                )));
            }
        },
    };
    Ok(Target {
        schema,
        index,
        internal: None,
    })
}

fn select(body: &Json, has_projection: bool, has_index: bool) -> Result<Select> {
    let s = match body.get("Select") {
        None => None,
        Some(Json::String(s)) => Some(s.as_str()),
        Some(_) => return Err(Error::validation("Select parameter must be a string")),
    };
    if has_projection && !matches!(s, None | Some("SPECIFIC_ATTRIBUTES")) {
        return Err(Error::validation(
            "AttributesToGet or ProjectionExpression require Select to be either SPECIFIC_ATTRIBUTES or missing",
        ));
    }
    Ok(match s {
        None => {
            if has_projection {
                Select::Specific
            } else {
                Select::All
            }
        }
        Some("ALL_ATTRIBUTES") => Select::All,
        Some("ALL_PROJECTED_ATTRIBUTES") => {
            if !has_index {
                return Err(Error::validation(
                    "ALL_PROJECTED_ATTRIBUTES only allowed for indexes",
                ));
            }
            Select::AllProjected
        }
        Some("SPECIFIC_ATTRIBUTES") => {
            if !has_projection {
                return Err(Error::validation(
                    "Select=SPECIFIC_ATTRIBUTES requires AttributesToGet or ProjectionExpression",
                ));
            }
            Select::Specific
        }
        Some("COUNT") => Select::Count,
        Some(other) => {
            return Err(Error::validation(format!(
                "Unknown Select value '{other}'. Allowed choices: ALL_ATTRIBUTES, SPECIFIC_ATTRIBUTES, ALL_PROJECTED_ATTRIBUTES, COUNT"
            )));
        }
    })
}

fn limit(body: &Json) -> Result<usize> {
    match body.get("Limit") {
        None => Ok(usize::MAX),
        Some(v) => match v.as_number() {
            Some(n) if !n.is_double() => {
                let x = n.as_i64().unwrap_or(i64::MIN);
                if x <= 0 {
                    return Err(Error::validation("Limit must be greater than 0"));
                }
                Ok(x as usize)
            }
            _ => Err(json_assert("IsInt()")),
        },
    }
}

/// The filter of a read (FilterExpression, or the legacy filter `param`).
fn filter(body: &Json, b: &mut Bindings, param: &str) -> Result<RCond> {
    let fe = expression_param(body, "FilterExpression")?;
    let legacy_filter = body.get(param);
    if fe.is_some() && legacy_filter.is_some() {
        return Err(Error::validation(format!(
            "FilterExpression and {param} are not allowed together"
        )));
    }
    let checks = match legacy_filter {
        Some(v) => legacy::parse_filter(v)?,
        None => Vec::new(),
    };
    let or = legacy::conditional_operator(body, !checks.is_empty(), param)?;
    if let Some(src) = fe {
        return condition_param(src, "FilterExpression", b);
    }
    if legacy_filter.is_some() {
        return Ok(RCond::Legacy(Conditions { checks, or }));
    }
    Ok(RCond::True)
}

/// Rejects mixed old/new parameters the way ScyllaDB words it.
fn check_mixing(body: &Json, legacy_param: &str) -> Result<()> {
    let mixed = |a: &str, b: &str| {
        Err(Error::validation(format!(
            "Cannot use both old-style and new-style parameters in same request: {a} and {b}"
        )))
    };
    if body.get(legacy_param).is_some() && body.get("ProjectionExpression").is_some() {
        return mixed(legacy_param, "ProjectionExpression");
    }
    if body.get("FilterExpression").is_some() && body.get("AttributesToGet").is_some() {
        return mixed("FilterExpression", "AttributesToGet");
    }
    Ok(())
}

/// Names of the attributes a filter reads at the top level (for the key-attribute check).
fn filter_roots(c: &RCond, out: &mut Vec<String>) {
    use crate::expr::eval::ROp;
    fn op(o: &ROp, out: &mut Vec<String>) {
        match o {
            ROp::Path(p) => out.push(p.root.clone()),
            ROp::Value(_) => {}
            ROp::Call(c) => c.args.iter().for_each(|a| op(a, out)),
        }
    }
    match c {
        RCond::Or(v) | RCond::And(v) => v.iter().for_each(|c| filter_roots(c, out)),
        RCond::Not(a) => filter_roots(a, out),
        RCond::Cmp(a, _, b) => {
            op(a, out);
            op(b, out);
        }
        RCond::Between(a, b, c) => {
            op(a, out);
            op(b, out);
            op(c, out);
        }
        RCond::In(a, l) => {
            op(a, out);
            l.iter().for_each(|x| op(x, out));
        }
        RCond::Call(c) => c.args.iter().for_each(|a| op(a, out)),
        RCond::Legacy(l) => out.extend(l.checks.iter().map(|(n, _)| n.clone())),
        RCond::True => {}
    }
}

/// Query rejects filters on the primary key of what it reads.
fn check_filter_keys(t: &Target, f: &RCond) -> Result<()> {
    let mut roots = Vec::new();
    filter_roots(f, &mut roots);
    let key = t.key();
    let sort = sort_attr(t);
    for r in roots {
        if key.hash.name == r {
            return Err(Error::validation(format!(
                "QueryFilter can only contain non-primary key attributes: Partition key attribute: {r}"
            )));
        }
        if sort.as_ref().is_some_and(|k| k.name == r) {
            return Err(Error::validation(format!(
                "QueryFilter can only contain non-primary key attributes: Sort key attribute: {r}"
            )));
        }
    }
    Ok(())
}

/// The read position given by ExclusiveStartKey.
fn exclusive_start(body: &Json, t: &Target) -> Result<Option<Item>> {
    let Some(esk) = body.get("ExclusiveStartKey") else {
        return Ok(None);
    };
    if !esk.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let mut item = Item::new();
    for a in t.position_attrs() {
        let Some(v) = esk.get(&a.name) else {
            return Err(Error::validation(format!(
                "Key column {} not found",
                a.name
            )));
        };
        item.insert(a.name.clone(), key_value(v, &a)?);
    }
    Ok(Some(item))
}

/// The key condition of a Query: the partition, and the sort-key condition.
struct KeyCond {
    hash: Json,
    sort: Option<SortCondition>,
}

/// Resolves names and values of a key condition, left to right.
fn resolve_kce_operand(o: &Operand, b: &mut Bindings) -> Result<KOp> {
    Ok(match o {
        Operand::Path(p) => KOp::Path(b.path(p)?),
        Operand::Value(v) => KOp::Value(b.value(v)?),
        Operand::Call(c) => {
            for a in &c.args {
                resolve_kce_operand(a, b)?;
            }
            KOp::Call
        }
    })
}

enum KOp {
    Path(RPath),
    Value(Json),
    /// A function call where a key or constant was expected (its arguments were resolved).
    Call,
}

enum KPrim {
    Cmp(KOp, CmpOp, KOp),
    Between(KOp, KOp, KOp),
    In,
    Call(String, Vec<KOp>),
}

fn kce_conditions(c: &Cond, b: &mut Bindings, out: &mut Vec<KPrim>) -> Result<()> {
    match c {
        Cond::And(v) | Cond::Or(v) => {
            for x in v {
                kce_conditions(x, b, out)?;
            }
        }
        Cond::Not(x) => kce_conditions(x, b, out)?,
        Cond::Cmp(x, op, y) => {
            let x = resolve_kce_operand(x, b)?;
            out.push(KPrim::Cmp(x, *op, resolve_kce_operand(y, b)?));
        }
        Cond::Between(x, lo, hi) => {
            let x = resolve_kce_operand(x, b)?;
            let lo = resolve_kce_operand(lo, b)?;
            out.push(KPrim::Between(x, lo, resolve_kce_operand(hi, b)?));
        }
        Cond::In(x, list) => {
            resolve_kce_operand(x, b)?;
            for o in list {
                resolve_kce_operand(o, b)?;
            }
            out.push(KPrim::In);
        }
        Cond::Call(call) => {
            let mut args = Vec::new();
            for a in &call.args {
                args.push(resolve_kce_operand(a, b)?);
            }
            out.push(KPrim::Call(call.name.clone(), args));
        }
    }
    Ok(())
}

fn has_or_not(c: &Cond) -> Option<&'static str> {
    match c {
        Cond::Or(..) => Some("OR"),
        Cond::Not(..) => Some("NOT"),
        Cond::And(v) => v.iter().find_map(has_or_not),
        _ => None,
    }
}

/// The key attribute a condition may restrict besides the hash key: the range key of the
/// table or index, or (for a GSI without one) the first clustering column of its view.
fn sort_attr(t: &Target) -> Option<KeyAttr> {
    match &t.index {
        Some(i) if i.kind == IndexKind::Global && i.key.range.is_none() => {
            Some(t.schema.key.hash.clone())
        }
        _ => t.key().range.clone(),
    }
}

fn key_condition_expression(src: &str, t: &Target, b: &mut Bindings) -> Result<KeyCond> {
    const P: &str = "KeyConditionExpression";
    if src.is_empty() {
        return Err(Error::validation(format!("{P} must not be empty")));
    }
    check_size(P, src)?;
    let c = cache::condition(src).map_err(|e| crate::ops::item::parse_error(P, src, &e.reason()))?;
    let mut prims = Vec::new();
    kce_conditions(&c, b, &mut prims)?;
    if let Some(w) = has_or_not(&c) {
        return Err(Error::validation(format!("{P} cannot use {w}")));
    }
    if prims.is_empty() || prims.len() > 2 {
        return Err(Error::validation(format!(
            "{P} syntax error: must have 1 or 2 conditions"
        )));
    }
    let key = t.key().clone();
    let sort = sort_attr(t);
    let mut hash: Option<Json> = None;
    let mut sort_cond: Option<SortCondition> = None;
    let mut seen: Vec<String> = Vec::new();
    for p in prims {
        // Normalise to (path, op, values).
        let (path, op, values): (RPath, &str, Vec<Json>) = match p {
            KPrim::In => {
                return Err(Error::validation(format!(
                    "{P} does not support IN operator"
                )));
            }
            KPrim::Cmp(_, CmpOp::Ne, _) => {
                return Err(Error::validation(format!(
                    "{P} does not support NE operator"
                )));
            }
            KPrim::Cmp(a, op, b) => {
                let (path, v, op) = match (a, b) {
                    (KOp::Path(p), KOp::Value(v)) => (p, v, op),
                    (KOp::Value(v), KOp::Path(p)) => (
                        p,
                        v,
                        match op {
                            CmpOp::Lt => CmpOp::Gt,
                            CmpOp::Le => CmpOp::Ge,
                            CmpOp::Gt => CmpOp::Lt,
                            CmpOp::Ge => CmpOp::Le,
                            o => o,
                        },
                    ),
                    _ => {
                        return Err(Error::validation(format!(
                            "{P} must compare attribute with constant"
                        )));
                    }
                };
                (path, op.symbol(), vec![v])
            }
            KPrim::Between(a, lo, hi) => match (a, lo, hi) {
                (KOp::Path(p), KOp::Value(lo), KOp::Value(hi)) => (p, "BETWEEN", vec![lo, hi]),
                _ => {
                    return Err(Error::validation(format!(
                        "{P} must compare attribute with constant"
                    )));
                }
            },
            KPrim::Call(name, args) => {
                if name != "begins_with" {
                    return Err(Error::validation(format!(
                        "{P} function '{name}' not supported"
                    )));
                }
                let mut args = args.into_iter();
                match (args.next(), args.next(), args.next()) {
                    (Some(KOp::Path(p)), Some(KOp::Value(v)), None) => (p, "begins_with", vec![v]),
                    _ => {
                        return Err(Error::validation(format!(
                            "{P} begins_with() takes attribute and value"
                        )));
                    }
                }
            }
        };
        if !path.is_top() {
            return Err(Error::validation(format!(
                "{P} does not support nested attributes"
            )));
        }
        let name = path.root;
        let attr = if key.hash.name == name {
            key.hash.clone()
        } else if let Some(s) = sort.as_ref().filter(|s| s.name == name) {
            s.clone()
        } else {
            return Err(Error::validation(format!(
                "{P} condition on non-key attribute {name}"
            )));
        };
        if seen.contains(&name) {
            return Err(Error::validation(format!(
                "{P} allows only one condition for each key"
            )));
        }
        seen.push(name.clone());
        let is_hash = attr.name == key.hash.name;
        if is_hash && op != "=" {
            return Err(Error::validation(format!(
                "{P} only '=' condition is supported on partition key {name}"
            )));
        }
        let mut vals = Vec::with_capacity(values.len());
        for v in &values {
            vals.push(key_value(v, &attr)?);
        }
        if op == "begins_with" && attr.ty == crate::schema::ScalarType::N {
            return Err(Error::validation(format!(
                "{P} begins_with() not supported on type N"
            )));
        }
        if is_hash {
            hash = vals.pop();
        } else {
            let mut it = vals.into_iter();
            let a = it.next().unwrap_or(Json::Null);
            sort_cond = Some(match op {
                "=" => SortCondition::Eq(a),
                "<" => SortCondition::Lt(a),
                "<=" => SortCondition::Le(a),
                ">" => SortCondition::Gt(a),
                ">=" => SortCondition::Ge(a),
                "BETWEEN" => SortCondition::Between(a, it.next().unwrap_or(Json::Null)),
                _ => SortCondition::BeginsWith(a),
            });
        }
    }
    let Some(hash) = hash else {
        return Err(Error::validation(format!(
            "{P} requires a condition on partition key {}",
            key.hash.name
        )));
    };
    Ok(KeyCond {
        hash,
        sort: sort_cond,
    })
}

/// Legacy KeyConditions.
fn key_conditions(v: &Json, t: &Target) -> Result<KeyCond> {
    let Some(m) = v.as_object() else {
        return Err(json_assert("IsObject()"));
    };
    let key = t.key().clone();
    let sort = sort_attr(t);
    let max = if sort.is_some() { 2 } else { 1 };
    if m.len() > max {
        return Err(Error::validation(if sort.is_some() {
            "Only one or two conditions allowed in table with hash key and sort key"
        } else {
            "Only one condition allowed in table with only hash key"
        }));
    }
    let mut hash = None;
    let mut sort_cond = None;
    for (name, spec) in m {
        if !spec.is_object() {
            return Err(json_assert("IsObject()"));
        }
        let Some(op_v) = spec.get("ComparisonOperator") else {
            return Err(Error::validation(
                "JSON parameter ComparisonOperator not found",
            ));
        };
        let Some(op_s) = op_v.as_str() else {
            return Err(Error::validation(format!(
                "Invalid comparison operator definition {}",
                op_v.to_json_string()
            )));
        };
        let Some(op) = Op::parse(op_s) else {
            return Err(Error::validation(format!(
                "Unsupported comparison operator {op_s}"
            )));
        };
        let Some(list) = spec.get("AttributeValueList") else {
            return Err(Error::validation(
                "JSON parameter AttributeValueList not found",
            ));
        };
        let list = list.as_array().unwrap_or_default();
        let list_json = Json::Array(list.to_vec()).to_json_string();
        if *name == key.hash.name {
            if op != Op::Eq {
                return Err(Error::validation(format!(
                    "Hash key can only be restricted with equality operator (EQ). \"{}\" not supported.",
                    op.name()
                )));
            }
            if list.len() != 1 {
                return Err(Error::validation(format!(
                    "A single attribute is required for a hash key EQ restriction: {list_json}"
                )));
            }
            hash = Some(key_value(&list[0], &key.hash)?);
        } else if let Some(s) = sort.as_ref().filter(|s| s.name == *name) {
            let want = if op == Op::Between { 2 } else { 1 };
            if list.len() != want {
                return Err(Error::validation(format!(
                    "{want} arguments expected for a sort key restriction: {list_json}"
                )));
            }
            let mut vals = Vec::new();
            for v in list {
                vals.push(key_value(v, s)?);
            }
            if !matches!(
                op,
                Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Between | Op::BeginsWith
            ) {
                return Err(Error::validation(format!(
                    "Operator \"{}\" not supported for sort key",
                    op.name()
                )));
            }
            if op == Op::BeginsWith && s.ty == crate::schema::ScalarType::N {
                return Err(Error::validation(
                    "BEGINS_WITH operator cannot be applied to type N",
                ));
            }
            let mut it = vals.into_iter();
            let a = it.next().unwrap_or(Json::Null);
            sort_cond = Some(match op {
                Op::Eq => SortCondition::Eq(a),
                Op::Lt => SortCondition::Lt(a),
                Op::Le => SortCondition::Le(a),
                Op::Gt => SortCondition::Gt(a),
                Op::Ge => SortCondition::Ge(a),
                Op::Between => SortCondition::Between(a, it.next().unwrap_or(Json::Null)),
                _ => SortCondition::BeginsWith(a),
            });
        } else if hash.is_none() {
            return Err(Error::validation(format!(
                "Query missing condition on hash key '{}'",
                key.hash.name
            )));
        } else if let Some(s) = &sort {
            return Err(Error::validation(format!(
                "Query missing condition on sort key '{}'",
                s.name
            )));
        }
    }
    let Some(hash) = hash else {
        return Err(Error::validation(format!(
            "Query missing condition on hash key '{}'",
            key.hash.name
        )));
    };
    Ok(KeyCond {
        hash,
        sort: sort_cond,
    })
}

/// Reads, filters and shapes one page.
#[allow(clippy::too_many_arguments)]
async fn read_page(
    alt: &Alternator,
    stats: &crate::metrics::RequestStats,
    t: &Target,
    scope: Scope,
    start: Option<Item>,
    forward: bool,
    limit: usize,
    filter: &RCond,
    paths: Option<&[RPath]>,
    select: Select,
) -> Result<Json> {
    let req = RangeRequest {
        index: t.index.as_ref().map(|i| i.name.clone()),
        scope,
        start_after: start,
        forward,
        limit,
        max_bytes: PAGE_BYTES,
    };
    let page = match &t.internal {
        Some(rows) => super::internal::page(rows, &t.schema, &req),
        None => alt.backend.read_range(&t.schema, &req).await?,
    };
    let scanned = page.items.len();
    let mut items = Vec::new();
    for it in &page.items {
        if Evaluator::new(Some(it)).eval(filter)? {
            items.push(it);
        }
    }
    if t.internal.is_none() {
        let name = &t.schema.name;
        stats.table(name);
        stats.storage(name, false);
        let bytes: usize = page.items.iter().map(crate::size::item_size).sum();
        stats.rcu(name, crate::size::read_units(Some(bytes), false));
        if select != Select::Count {
            stats.returned_items(name, items.len() as u64);
        }
    }
    let mut out = Json::object()
        .with("Count", Json::u64(items.len() as u64))
        .with("ScannedCount", Json::u64(scanned as u64));
    if select != Select::Count {
        let list = items
            .iter()
            .map(|it| match paths {
                Some(p) => item_json(&project(it, p)),
                None => item_json(it),
            })
            .collect();
        out.push("Items", Json::Array(list));
    }
    // After the last item read, or where the read stopped among tombstones past it.
    if page.more
        && let Some(last) = page.resume.as_ref().or(page.items.last())
    {
        let mut k = Json::object();
        for a in t.position_attrs() {
            if let Some(v) = last.get(&a.name) {
                k.push(a.name.clone(), v.clone());
            }
        }
        out.push("LastEvaluatedKey", k);
    }
    Ok(out)
}

fn consistent_check(body: &Json, t: &Target) -> Result<()> {
    let c = consistent_read(body)?;
    if c && t
        .index
        .as_ref()
        .is_some_and(|i| i.kind == IndexKind::Global)
    {
        return Err(Error::validation(
            "Consistent reads are not allowed on global indexes (GSI)",
        ));
    }
    Ok(())
}

fn projection_and_select(
    body: &Json,
    t: &Target,
    b: &mut Bindings,
    op: &str,
) -> Result<(Option<Vec<RPath>>, Select)> {
    let _ = op;
    if body
        .get("AttributesToGet")
        .and_then(Json::as_array)
        .is_some_and(|a| a.is_empty())
    {
        return Err(Error::validation(
            "Empty AttributesToGet is not allowed. Consider using Select=COUNT instead.",
        ));
    }
    let has = body.get("ProjectionExpression").is_some() || body.get("AttributesToGet").is_some();
    let sel = select(body, has, t.index.is_some())?;
    let paths = projection(body, b, "GetItem")?;
    Ok((paths, sel))
}

pub async fn query(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    if let Some(r) = super::vector::search(alt, req).await? {
        return Ok(r);
    }
    let t = read_target(alt, req).await?;
    consistent_check(body, &t)?;
    let forward = match body.get("ScanIndexForward") {
        None => true,
        Some(Json::Bool(b)) => *b,
        Some(_) => {
            return Err(Error::validation(format!(
                "Expected boolean value for attribute ScanIndexForward, got: {}",
                body.to_json_string()
            )));
        }
    };
    let mut b = Bindings::new(body);
    let kce = expression_param(body, "KeyConditionExpression")?;
    let kc = body.get("KeyConditions");
    let key = match (kce, kc) {
        (Some(_), Some(_)) => {
            return Err(Error::validation(
                "Query does not allow both KeyConditions and KeyConditionExpression to be given together",
            ));
        }
        (Some(src), None) => key_condition_expression(src, &t, &mut b)?,
        (None, Some(v)) => key_conditions(v, &t)?,
        (None, None) => {
            return Err(Error::validation(
                "Query must have one of KeyConditions or KeyConditionExpression",
            ));
        }
    };
    let (paths, sel) = projection_and_select(body, &t, &mut b, "Query")?;
    check_mixing(body, "QueryFilter")?;
    let limit = limit(body)?;
    let f = filter(body, &mut b, "QueryFilter")?;
    check_filter_keys(&t, &f)?;
    b.check_unused("Query")?;
    let start = exclusive_start(body, &t)?;
    if let Some(s) = &start {
        let ka = &t.key().hash;
        if s.get(&ka.name).map(storage_key) != Some(storage_key(&key.hash)) {
            let mut out = Json::object()
                .with("Count", Json::u64(0))
                .with("ScannedCount", Json::u64(0));
            if sel != Select::Count {
                out.push("Items", Json::Array(Vec::new()));
            }
            return Ok(out);
        }
    }
    check_key_value_length(&key.hash)?;
    match &key.sort {
        Some(SortCondition::Between(a, b)) => {
            check_key_value_length(a)?;
            check_key_value_length(b)?;
        }
        Some(
            SortCondition::Eq(a)
            | SortCondition::Lt(a)
            | SortCondition::Le(a)
            | SortCondition::Gt(a)
            | SortCondition::Ge(a)
            | SortCondition::BeginsWith(a),
        ) => check_key_value_length(a)?,
        None => {}
    }
    let scope = Scope::Partition {
        hash: key.hash,
        sort: key.sort,
    };
    read_page(
        alt,
        req.stats,
        &t,
        scope,
        start,
        forward,
        limit,
        &f,
        paths.as_deref(),
        sel,
    )
    .await
}

fn storage_key(v: &Json) -> Option<Vec<u8>> {
    crate::storage::encode_key(v)
}

/// The token range of segment `seg` of `total`.
pub fn segment_range(seg: u64, total: u64) -> (i64, i64) {
    let span = u128::from(u64::MAX) + 1;
    let start = (span * seg as u128 / total as u128) as i128 + i64::MIN as i128;
    let end = (span * (seg as u128 + 1) / total as u128) as i128 + i64::MIN as i128 - 1;
    (start as i64, end.min(i64::MAX as i128) as i64)
}

pub async fn scan(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let t = read_target(alt, req).await?;
    let seg = match (body.get("Segment"), body.get("TotalSegments")) {
        (None, None) => None,
        (Some(s), Some(n)) => {
            let n = n
                .as_number()
                .and_then(|n| n.as_i64())
                .ok_or_else(|| json_assert("IsInt()"))?;
            let s = s
                .as_number()
                .and_then(|n| n.as_i64())
                .ok_or_else(|| json_assert("IsInt()"))?;
            if s < 0 || s >= n {
                return Err(Error::validation(
                    "Segment must be non-negative and less than TotalSegments",
                ));
            }
            if n > 1_000_000 {
                return Err(Error::validation(
                    "TotalSegments must be non-negative and less or equal to 1000000",
                ));
            }
            Some((s as u64, n as u64))
        }
        _ => {
            return Err(Error::validation(
                "Both Segment and TotalSegments attributes need to be present for a parallel scan",
            ));
        }
    };
    consistent_check(body, &t)?;
    let mut b = Bindings::new(body);
    let (paths, sel) = projection_and_select(body, &t, &mut b, "Scan")?;
    check_mixing(body, "ScanFilter")?;
    let limit = limit(body)?;
    let f = filter(body, &mut b, "ScanFilter")?;
    b.check_unused("Scan")?;
    let start = exclusive_start(body, &t)?;
    let (lo, hi) = match seg {
        Some((s, n)) => segment_range(s, n),
        None => (i64::MIN, i64::MAX),
    };
    if let (Some(st), Some((s, n))) = (&start, seg) {
        let hash = st
            .get(&t.key().hash.name)
            .and_then(storage_key)
            .unwrap_or_default();
        let tok = nosql_types::token(&hash);
        if tok < lo || tok > hi {
            return Err(Error::validation(format!(
                "The provided starting key is invalid: Invalid ExclusiveStartKey. Please use ExclusiveStartKey with correct Segment. TotalSegments: {n} Segment: {s}"
            )));
        }
    }
    let _ = check_filter_keys;
    read_page(
        alt,
        req.stats,
        &t,
        Scope::Tokens { start: lo, end: hi },
        start,
        true,
        limit,
        &f,
        paths.as_deref(),
        sel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_the_ring() {
        assert_eq!(segment_range(0, 1), (i64::MIN, i64::MAX));
        let (a, b) = segment_range(0, 2);
        let (c, d) = segment_range(1, 2);
        assert_eq!((a, b, c, d), (i64::MIN, -1, 0, i64::MAX));
        let mut prev = i64::MIN as i128 - 1;
        for s in 0..7 {
            let (x, y) = segment_range(s, 7);
            assert_eq!(x as i128, prev + 1);
            prev = y as i128;
        }
        assert_eq!(prev, i64::MAX as i128);
    }
}
