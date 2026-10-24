//! Declarative partitioning: partition keys (pg_partitioned_table) and partition bounds
//! (pg_class.relpartbound), their canonical order, matching and display.

use crate::types::collation::Collation;
use crate::types::hash::{PARTITION_SEED, combine64, hash_bytes_extended, value_hash};
use crate::types::{DataType, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::sync::Arc;

/// The collations a partition key's columns compare under (`partcollation`): an explicit
/// COLLATE, else the column's; None where that compares as bytes (or the column isn't
/// collatable).
pub fn key_collations(cat: &super::Catalog, t: &super::Table) -> Vec<Option<Arc<Collation>>> {
    let Some(key) = &t.partkey else {
        return Vec::new();
    };
    key.cols
        .iter()
        .enumerate()
        .map(|(i, k)| {
            // A made btree operator class whose comparison function is a made one: bounds
            // and keys compare by calling it.
            if let Some(f) = cat.btree_support_function(t, i) {
                return comparator(f);
            }
            if !k.ty.collatable() {
                return None;
            }
            let name = k.collation.clone().or_else(|| {
                k.col
                    .and_then(|i| t.columns.get(i))
                    .and_then(|c| c.collation.clone())
            })?;
            crate::types::collation::by_name(cat, &name).filter(|c| !c.bytewise() || !c.deterministic)
        })
        .collect()
}

thread_local! {
    /// The key collations bound and key comparisons follow (see `under`).
    static COLLS: RefCell<Vec<Option<Arc<Collation>>>> = const { RefCell::new(Vec::new()) };
}

/// While the guard lives, bounds and keys compare (and hash) under `colls`, the partition
/// key's collations (`key_collations`).
pub fn under(colls: Vec<Option<Arc<Collation>>>) -> KeyGuard {
    KeyGuard(Some(COLLS.with(|c| c.replace(colls))))
}

pub struct KeyGuard(Option<Vec<Option<Arc<Collation>>>>);

impl Drop for KeyGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            COLLS.with(|c| *c.borrow_mut() = prev);
        }
    }
}

fn key_coll(k: usize) -> Option<Arc<Collation>> {
    COLLS.with(|c| c.borrow().get(k).cloned().flatten())
}

/// The name of the stand-in collation `key_collations` gives a key column a made comparison
/// function orders (the function's oid follows).
const COMPARATOR: &str = "\0comparator:";

/// A stand-in collation naming comparison function `f`.
pub fn comparator(f: u32) -> Option<Arc<Collation>> {
    let def = crate::types::collation::CollationDef {
        oid: 0,
        name: String::new(),
        namespace: 0,
        owner: 0,
        provider: 'c',
        deterministic: true,
        encoding: -1,
        collate: Some("C".into()),
        ctype: Some("C".into()),
        locale: None,
        icu_rules: None,
        version: None,
    };
    let mut c = Collation::open(&def).ok()?;
    c.name = format!("{COMPARATOR}{f}");
    Some(Arc::new(c))
}

/// Call comparison function `f` on two values (with the running statement's context);
/// None when it can't be.
fn call_comparator(f: u32, a: &Value, b: &Value) -> Option<Ordering> {
    crate::session::current::with_current(|ctx| {
        let cat = ctx.catalog();
        let func = cat.functions.get(&f)?.clone();
        let rows = crate::plpgsql::call_function(ctx, &func, vec![a.clone(), b.clone()]).ok()?;
        let v = rows.first()?.first()?.as_i64()?;
        Some(v.cmp(&0))
    })
    .flatten()
}

/// Two non-null values compared by the comparison function a `comparator` stand-in
/// collation names; None when `c` isn't one (or the function can't be called).
pub fn comparator_cmp(c: &Collation, a: &Value, b: &Value) -> Option<Ordering> {
    let rest = c.name.strip_prefix(COMPARATOR)?;
    let f = rest.split(':').next()?.parse().ok()?;
    call_comparator(f, a, b)
}

/// The comparison function a `comparator` stand-in collation names.
pub fn comparator_fn(c: &Collation) -> Option<u32> {
    c.name.strip_prefix(COMPARATOR)?.split(':').next()?.parse().ok()
}

/// A `comparator` stand-in for `ORDER BY ... USING op`, naming the operator too.
pub fn comparator_for_op(f: u32, op: &str) -> Option<Arc<Collation>> {
    let mut c = (*comparator(f)?).clone();
    c.name = format!("{COMPARATOR}{f}:{op}");
    Some(Arc::new(c))
}

/// The ordering operator a `comparator_for_op` stand-in names.
pub fn comparator_op(c: &Collation) -> Option<&str> {
    c.name.strip_prefix(COMPARATOR)?.split_once(':').map(|x| x.1)
}

/// Two values of key column `k` (`partsupfunc` under `partcollation`).
pub fn key_cmp(k: usize, a: &Value, b: &Value) -> Ordering {
    let coll = key_coll(k);
    if let Some(c) = &coll
        && let Some(f) = c.name.strip_prefix(COMPARATOR)
        && !a.is_null()
        && !b.is_null()
    {
        if let Some(o) = f.parse().ok().and_then(|f| call_comparator(f, a, b)) {
            return o;
        }
        return a.cmp_total(b);
    }
    match (a, b, coll) {
        (Value::Text(x), Value::Text(y), Some(c)) => c.cmp(x, y),
        _ => a.cmp_total(b),
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PartStrategy {
    List,
    Range,
    Hash,
}

impl PartStrategy {
    /// pg_partitioned_table.partstrat
    pub fn code(self) -> char {
        match self {
            PartStrategy::List => 'l',
            PartStrategy::Range => 'r',
            PartStrategy::Hash => 'h',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PartStrategy::List => "LIST",
            PartStrategy::Range => "RANGE",
            PartStrategy::Hash => "HASH",
        }
    }

    pub fn lower(self) -> &'static str {
        match self {
            PartStrategy::List => "list",
            PartStrategy::Range => "range",
            PartStrategy::Hash => "hash",
        }
    }
}

/// One column of a partition key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartKeyCol {
    /// The column, or None for an expression.
    pub col: Option<usize>,
    /// The expression (SQL over the table's columns).
    pub expr: Option<String>,
    /// The key's type.
    pub ty: DataType,
    /// Explicit COLLATE.
    pub collation: Option<String>,
    /// Explicit operator class.
    pub opclass: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartKey {
    pub strategy: PartStrategy,
    pub cols: Vec<PartKeyCol>,
}

impl PartKey {
    pub fn types(&self) -> Vec<DataType> {
        self.cols.iter().map(|c| c.ty.clone()).collect()
    }
}

/// A bound value, stored in the row encoding.
#[derive(Clone, Debug)]
pub struct Datum(pub Value);

impl Serialize for Datum {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut out = Vec::new();
        crate::storage::row::encode_value(&mut out, &self.0);
        let h: String = out.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&h)
    }
}

impl<'de> Deserialize<'de> for Datum {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let h = String::deserialize(d)?;
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)?;
        let mut pos = 0;
        crate::storage::row::decode_value(&bytes, &mut pos)
            .map(Datum)
            .map_err(serde::de::Error::custom)
    }
}

impl PartialEq for Datum {
    fn eq(&self, other: &Self) -> bool {
        self.0.cmp_total(&other.0) == Ordering::Equal
    }
}

/// A range bound's value: MINVALUE, MAXVALUE or a value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum RangeDatum {
    MinValue,
    MaxValue,
    Value(Datum),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PartBound {
    Default,
    /// FOR VALUES IN (...), possibly including NULL.
    List(Vec<Datum>),
    /// FOR VALUES FROM (...) TO (...): from inclusive, to exclusive.
    Range {
        from: Vec<RangeDatum>,
        to: Vec<RangeDatum>,
    },
    /// FOR VALUES WITH (MODULUS m, REMAINDER r)
    Hash {
        modulus: u32,
        remainder: u32,
    },
}

impl PartBound {
    pub fn is_default(&self) -> bool {
        matches!(self, PartBound::Default)
    }
}

/// Compare range bounds lexicographically (MINVALUE below every value, MAXVALUE above).
pub fn range_cmp(a: &[RangeDatum], b: &[RangeDatum]) -> Ordering {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let o = match (x, y) {
            (RangeDatum::MinValue, RangeDatum::MinValue)
            | (RangeDatum::MaxValue, RangeDatum::MaxValue) => return Ordering::Equal,
            (RangeDatum::MinValue, _) | (_, RangeDatum::MaxValue) => Ordering::Less,
            (_, RangeDatum::MinValue) | (RangeDatum::MaxValue, _) => Ordering::Greater,
            (RangeDatum::Value(x), RangeDatum::Value(y)) => key_cmp(i, &x.0, &y.0),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// A key tuple against a range bound: Less when the key sorts below it.
pub fn key_vs_bound(key: &[Value], b: &[RangeDatum]) -> Ordering {
    for (i, (k, d)) in key.iter().zip(b).enumerate() {
        let o = match d {
            RangeDatum::MinValue => Ordering::Greater,
            RangeDatum::MaxValue => Ordering::Less,
            RangeDatum::Value(d) => key_cmp(i, k, &d.0),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// The row hash of a key (compute_partition_hash_value); NULL keys don't contribute.
pub fn row_hash(key: &[Value], types: &[DataType]) -> u64 {
    let mut h = 0u64;
    for (i, (v, ty)) in key.iter().zip(types).enumerate() {
        if v.is_null() {
            continue;
        }
        // The hash itself, from a made operator class's support function.
        if *ty == DataType::Void {
            if let Some(x) = v.as_i64() {
                h = combine64(h, x as u64);
            }
            continue;
        }
        // A nondeterministic collation hashes the sort key (hashtextextended).
        if let Value::Text(s) = v
            && let Some(c) = key_coll(i).filter(|c| !c.deterministic)
        {
            h = combine64(h, hash_bytes_extended(&c.key(s), PARTITION_SEED));
            continue;
        }
        // An array of them (hash_array_extended over the elements' hashes, under it).
        if let Value::Array(a) = v
            && matches!(a.elem, DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_))
            && let Some(c) = key_coll(i).filter(|c| !c.deterministic)
        {
            let mut result: u64 = 1;
            for e in &a.items {
                let eh = match e {
                    Value::Text(s) => hash_bytes_extended(&c.key(s), PARTITION_SEED),
                    _ => 0,
                };
                result = (result << 5).wrapping_sub(result).wrapping_add(eh);
            }
            h = combine64(h, result);
            continue;
        }
        if let Some(x) = value_hash(v, ty, PARTITION_SEED) {
            h = combine64(h, x);
        }
    }
    h
}

/// Whether a non-default bound holds a key.
pub fn accepts(bound: &PartBound, key: &[Value], types: &[DataType]) -> bool {
    match bound {
        PartBound::Default => false,
        PartBound::List(vals) => vals.iter().any(|d| {
            if key[0].is_null() {
                d.0.is_null()
            } else {
                !d.0.is_null() && key_cmp(0, &d.0, &key[0]) == Ordering::Equal
            }
        }),
        PartBound::Range { from, to } => {
            !key.iter().any(Value::is_null)
                && key_vs_bound(key, from) != Ordering::Less
                && key_vs_bound(key, to) == Ordering::Less
        }
        PartBound::Hash { modulus, remainder } => {
            row_hash(key, types) % *modulus as u64 == *remainder as u64
        }
    }
}

/// Which of `bounds` holds a key: the matching one, else the default.
pub fn route<'a>(
    bounds: impl Iterator<Item = &'a PartBound> + Clone,
    key: &[Value],
    types: &[DataType],
) -> Option<usize> {
    bounds
        .clone()
        .position(|b| accepts(b, key, types))
        .or_else(|| bounds.into_iter().position(PartBound::is_default))
}

/// Whether a partition with `bound` holds a key, given its siblings (for a default partition).
pub fn fits(bound: &PartBound, siblings: &[PartBound], key: &[Value], types: &[DataType]) -> bool {
    match bound {
        PartBound::Default => !siblings.iter().any(|b| accepts(b, key, types)),
        b => accepts(b, key, types),
    }
}

/// Sort key for the canonical partition order (Postgres's partition bound order): list
/// partitions by their lowest value (NULL-only ones after), range by lower bound, hash by
/// (modulus, remainder), the default last.
pub fn bound_order(a: &PartBound, b: &PartBound) -> Ordering {
    use PartBound as B;
    match (a, b) {
        (B::Default, B::Default) => Ordering::Equal,
        (B::Default, _) => Ordering::Greater,
        (_, B::Default) => Ordering::Less,
        (B::List(x), B::List(y)) => {
            let low = |v: &[Datum]| {
                v.iter()
                    .filter(|d| !d.0.is_null())
                    .min_by(|p, q| key_cmp(0, &p.0, &q.0))
                    .map(|d| d.0.clone())
            };
            match (low(x), low(y)) {
                (Some(p), Some(q)) => key_cmp(0, &p, &q),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        (B::Range { from: x, .. }, B::Range { from: y, .. }) => range_cmp(x, y),
        (
            B::Hash {
                modulus: m1,
                remainder: r1,
            },
            B::Hash {
                modulus: m2,
                remainder: r2,
            },
        ) => (m1, r1).cmp(&(m2, r2)),
        _ => Ordering::Equal,
    }
}

/// A bound value as Postgres's get_const_expr shows it without a type (showtype -1).
pub fn const_text(v: &Value, ty: &DataType, fmt: &crate::types::text::FormatCtx) -> String {
    if v.is_null() {
        return "NULL".into();
    }
    let s = crate::types::text::format_value(v, ty, fmt);
    match v {
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Int2(_)
        | Value::Int4(_)
        | Value::Int8(_)
        | Value::Numeric(_)
        | Value::Float4(_)
        | Value::Float8(_)
            if !s.starts_with('-')
                && s.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | 'e' | 'E' | '.')) =>
        {
            s
        }
        _ => format!("'{}'", s.replace('\'', "''")),
    }
}

/// pg_get_expr(relpartbound): `FOR VALUES IN ('eu', 'uk')`, `DEFAULT`, …
pub fn bound_text(
    bound: &PartBound,
    types: &[DataType],
    fmt: &crate::types::text::FormatCtx,
) -> String {
    let ty = |i: usize| types.get(i).cloned().unwrap_or(DataType::Text);
    match bound {
        PartBound::Default => "DEFAULT".into(),
        PartBound::List(vals) => format!(
            "FOR VALUES IN ({})",
            vals.iter()
                .map(|d| const_text(&d.0, &ty(0), fmt))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PartBound::Range { from, to } => {
            let side = |ds: &[RangeDatum]| {
                ds.iter()
                    .enumerate()
                    .map(|(i, d)| match d {
                        RangeDatum::MinValue => "MINVALUE".to_string(),
                        RangeDatum::MaxValue => "MAXVALUE".to_string(),
                        RangeDatum::Value(d) => const_text(&d.0, &ty(i), fmt),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("FOR VALUES FROM ({}) TO ({})", side(from), side(to))
        }
        PartBound::Hash { modulus, remainder } => {
            format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})")
        }
    }
}

/// A bound value as typed SQL (`'x'::text`), for partition constraint expressions.
fn typed_literal(v: &Value, ty: &DataType) -> String {
    if v.is_null() {
        return format!("NULL::{}", ty.display());
    }
    let s = crate::types::text::format_value(v, ty, &crate::types::text::FormatCtx::default());
    format!("'{}'::{}", s.replace('\'', "''"), ty.display())
}

/// The key of a partitioned table as SQL over its rows.
fn key_sql(t: &super::Table, key: &PartKey, leaf: Option<(&str, &str)>) -> Vec<String> {
    let me = super::quote_ident(&t.name);
    key.cols
        .iter()
        .map(|k| match (k.col, &k.expr) {
            (Some(i), _) => super::quote_ident(&t.columns[i].name),
            // A whole-row key over the partition: its row as the parent's
            // (ConvertRowtypeExpr).
            (_, Some(e)) if leaf.is_some() && (*e == me || *e == format!("{me}.*")) => {
                let (schema, name) = leaf.unwrap();
                format!(
                    "({}.*)::{}.{me}",
                    super::quote_ident(name),
                    super::quote_ident(schema)
                )
            }
            (_, Some(e)) => format!("({e})"),
            _ => String::new(),
        })
        .collect()
}

/// `k IS [NOT] NULL` of a partition key, as get_qual_for_list's NullTest (argisrow false)
/// reads: on a composite key, whether the value itself is null.
fn key_null_test(k: &str, ty: &crate::types::DataType, not: bool) -> String {
    match (ty, not) {
        (crate::types::DataType::Row(_) | crate::types::DataType::Record, true) => {
            format!("({k} IS DISTINCT FROM NULL)")
        }
        (crate::types::DataType::Row(_) | crate::types::DataType::Record, false) => {
            format!("({k} IS NOT DISTINCT FROM NULL)")
        }
        (_, true) => format!("({k} IS NOT NULL)"),
        (_, false) => format!("({k} IS NULL)"),
    }
}

/// One level's partition constraint as a list of ANDed SQL items (get_qual_from_partbound).
fn level_items(
    parent: &super::Table,
    key: &PartKey,
    bound: &PartBound,
    siblings: &[PartBound],
    leaf: Option<(&str, &str)>,
) -> Vec<String> {
    let ks = key_sql(parent, key, leaf);
    let types = key.types();
    let list_expr = |vals: &[&Value], with_null: bool| -> Vec<String> {
        let k = &ks[0];
        let cmp = match vals {
            [] => None,
            [v] => Some(format!("({k} = {})", typed_literal(v, &types[0]))),
            // An array key can't be compared with = ANY: an OR of equalities.
            vs if types[0].is_array() => Some(format!(
                "({})",
                vs.iter()
                    .map(|v| format!("({k} = {})", typed_literal(v, &types[0])))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            )),
            vs => Some(format!(
                "({k} = ANY (ARRAY[{}]))",
                vs.iter()
                    .map(|v| typed_literal(v, &types[0]))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
        match (with_null, cmp) {
            (true, Some(c)) => vec![format!("({} OR {c})", key_null_test(k, &types[0], false))],
            (true, None) => vec![key_null_test(k, &types[0], false)],
            (false, Some(c)) => vec![key_null_test(k, &types[0], true), c],
            (false, None) => Vec::new(),
        }
    };
    let range_arms = |from: &[RangeDatum], to: &[RangeDatum]| -> Vec<String> {
        let n = ks.len();
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            match (&from[i], &to[i]) {
                (RangeDatum::Value(a), RangeDatum::Value(b)) if a == b => {
                    out.push(format!("({} = {})", ks[i], typed_literal(&a.0, &types[i])));
                    i += 1;
                }
                _ => break,
            }
        }
        let side = |ds: &[RangeDatum], lower: bool| -> Option<String> {
            let mut arms = Vec::new();
            for j in i..n {
                let RangeDatum::Value(v) = &ds[j] else { break };
                let last = j + 1 == n || !matches!(ds[j + 1], RangeDatum::Value(_));
                let op = match (lower, last) {
                    (true, true) if j + 1 == n || ds[j + 1] == RangeDatum::MinValue => ">=",
                    (true, _) => ">",
                    (false, true) if j + 1 < n && ds[j + 1] == RangeDatum::MaxValue => "<=",
                    (false, _) => "<",
                };
                let mut parts: Vec<String> = (i..j)
                    .map(|k| match &ds[k] {
                        RangeDatum::Value(w) => {
                            format!("({} = {})", ks[k], typed_literal(&w.0, &types[k]))
                        }
                        _ => String::new(),
                    })
                    .collect();
                parts.push(format!(
                    "({} {op} {})",
                    ks[j],
                    typed_literal(&v.0, &types[j])
                ));
                arms.push(if parts.len() == 1 {
                    parts.pop().unwrap()
                } else {
                    format!("({})", parts.join(" AND "))
                });
            }
            match arms.len() {
                0 => None,
                1 => arms.pop(),
                _ => Some(format!("({})", arms.join(" OR "))),
            }
        };
        if i < n {
            out.extend(side(from, true));
            out.extend(side(to, false));
        }
        out
    };
    match (key.strategy, bound) {
        (PartStrategy::Hash, PartBound::Hash { modulus, remainder }) => {
            vec![format!(
                "satisfies_hash_partition('{}'::oid, {modulus}, {remainder}, {})",
                parent.oid,
                ks.join(", ")
            )]
        }
        (PartStrategy::List, PartBound::List(vals)) => {
            let nonnull: Vec<&Value> = vals.iter().map(|d| &d.0).filter(|v| !v.is_null()).collect();
            list_expr(&nonnull, vals.iter().any(|d| d.0.is_null()))
        }
        (PartStrategy::List, PartBound::Default) => {
            let mut all: Vec<&Value> = siblings
                .iter()
                .flat_map(|b| match b {
                    PartBound::List(v) => v.iter().map(|d| &d.0).collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .filter(|v| !v.is_null())
                .collect();
            all.sort_by(|a, b| a.cmp_total(b));
            let null = siblings
                .iter()
                .any(|b| matches!(b, PartBound::List(v) if v.iter().any(|d| d.0.is_null())));
            let items = list_expr(&all, null);
            if items.is_empty() {
                Vec::new()
            } else {
                vec![format!("(NOT ({}))", items.join(" AND "))]
            }
        }
        (PartStrategy::Range, PartBound::Range { from, to }) => {
            let mut items: Vec<String> = (ks.iter().zip(&types))
                .map(|(k, t)| key_null_test(k, t, true))
                .collect();
            items.extend(range_arms(from, to));
            items
        }
        (PartStrategy::Range, PartBound::Default) => {
            let mut sibs: Vec<(&[RangeDatum], &[RangeDatum])> = siblings
                .iter()
                .filter_map(|b| match b {
                    PartBound::Range { from, to } => Some((from.as_slice(), to.as_slice())),
                    _ => None,
                })
                .collect();
            if sibs.is_empty() {
                return Vec::new();
            }
            sibs.sort_by(|a, b| range_cmp(a.0, b.0));
            let arms: Vec<String> = sibs
                .iter()
                .map(|(f, t)| {
                    let a = range_arms(f, t);
                    if a.len() == 1 {
                        a[0].clone()
                    } else {
                        format!("({})", a.join(" AND "))
                    }
                })
                .collect();
            let ors = if arms.len() == 1 {
                arms[0].clone()
            } else {
                format!("({})", arms.join(" OR "))
            };
            let nn: Vec<String> = (ks.iter().zip(&types))
                .map(|(k, t)| key_null_test(k, t, true))
                .collect();
            vec![format!("(NOT ({} AND {ors}))", nn.join(" AND "))]
        }
        _ => Vec::new(),
    }
}

/// A partition's whole partition constraint (its own level and every ancestor's) as SQL
/// over its rows, or None when it has none (pg_get_partition_constraintdef).
pub fn constraint_sql(cat: &super::Catalog, oid: u32) -> Option<String> {
    let mut levels = Vec::new();
    let leaf = cat
        .tables
        .get(&oid)
        .map(|t| (cat.schema_name(t.schema).to_string(), t.name.clone()));
    let leaf = leaf.as_ref().map(|(s, n)| (s.as_str(), n.as_str()));
    let mut cur = oid;
    while let Some(t) = cat.tables.get(&cur)
        && let Some(bound) = &t.partbound
        && let Some(parent) = t.inherits.first().and_then(|p| cat.tables.get(p))
        && let Some(key) = &parent.partkey
    {
        let siblings: Vec<PartBound> = cat
            .children(parent.oid)
            .iter()
            .filter(|c| **c != cur)
            .filter_map(|c| cat.tables.get(c).and_then(|s| s.partbound.clone()))
            .collect();
        levels.push(level_items(parent, key, bound, &siblings, leaf));
        cur = parent.oid;
    }
    let items: Vec<String> = levels.into_iter().rev().flatten().collect();
    if items.is_empty() {
        return None;
    }
    Some(items.join(" AND "))
}
