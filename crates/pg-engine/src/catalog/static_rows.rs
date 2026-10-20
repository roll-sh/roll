//! The rows Postgres 18 ships in system catalogs that hold no per-database state here (access
//! method operators and support functions, conversions, collations, casts, comments, the system
//! catalogs' own constraints, indexes and dependencies, the system views' rules), exported from
//! a fresh database in COPY text format with reg* columns as plain oids.

use super::system::SysTable;
use crate::types::text::{FormatCtx, parse_value};
use crate::types::{DataType, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

type Row = Vec<Value>;

fn source(table: SysTable) -> Option<std::borrow::Cow<'static, str>> {
    use SysTable::*;
    Some(
        match table {
            PgAggregate => include_str!("data/pg_aggregate.tsv"),
            PgAm => include_str!("data/pg_am.tsv"),
            PgAmop => include_str!("data/pg_amop.tsv"),
            PgAmproc => include_str!("data/pg_amproc.tsv"),
            PgAuthMembers => include_str!("data/pg_auth_members.tsv"),
            PgCast => include_str!("data/pg_cast.tsv"),
            PgCollation => include_str!("data/pg_collation.tsv"),
            PgConstraint => include_str!("data/pg_constraint.tsv"),
            PgConversion => include_str!("data/pg_conversion.tsv"),
            PgDepend => include_str!("data/pg_depend.tsv"),
            PgDescription => include_str!("data/pg_description.tsv"),
            PgIndex => include_str!("data/pg_index.tsv"),
            PgInitPrivs => include_str!("data/pg_init_privs.tsv"),
            PgOpclass => include_str!("data/pg_opclass.tsv"),
            PgOperator => include_str!("data/pg_operator.tsv"),
            PgOpfamily => include_str!("data/pg_opfamily.tsv"),
            PgShdescription => include_str!("data/pg_shdescription.tsv"),
            PgClass => include_str!("data/pg_class.tsv"),
            PgType => include_str!("data/pg_type.tsv"),
            PgAttribute => include_str!("data/pg_attribute.tsv"),
            PgStatistic => include_str!("data/pg_statistic.tsv"),
            IsSqlFeatures => include_str!("data/is_sql_features.tsv"),
            IsSqlImplementationInfo => include_str!("data/is_sql_implementation_info.tsv"),
            IsSqlParts => include_str!("data/is_sql_parts.tsv"),
            IsSqlSizing => include_str!("data/is_sql_sizing.tsv"),
            // The view rules' trees are large: kept compressed.
            PgRewrite => {
                let raw = zstd::decode_all(&include_bytes!("data/pg_rewrite.tsv.zst")[..])
                    .expect("pg_rewrite data");
                return Some(String::from_utf8(raw).expect("pg_rewrite data").into());
            }
            _ => return None,
        }
        .into(),
    )
}

fn is_reg(ty: &DataType) -> bool {
    use DataType::*;
    matches!(
        ty,
        Regclass
            | Regtype
            | Regproc
            | Regprocedure
            | Regnamespace
            | Regrole
            | Regconfig
            | Regdictionary
            | Regoper
            | Regoperator
            | Regcollation
    )
}

fn field(raw: &str, ty: &DataType, fmt: &FormatCtx) -> Value {
    let Some(s) = super::system::copy_field(raw) else {
        return Value::Null;
    };
    if is_reg(ty) {
        return Value::Oid(s.parse().expect("catalog oid"));
    }
    match ty {
        DataType::Name | DataType::Text | DataType::PgNodeTree => Value::Text(Arc::from(s)),
        // Typed later where the element type is known (`type_stat_values`).
        DataType::AnyArray => {
            parse_value(&s, &DataType::array_of(DataType::Text), fmt).expect("catalog array")
        }
        _ => parse_value(&s, ty, fmt)
            .unwrap_or_else(|e| panic!("catalog value {s:?} as {ty:?}: {e:?}")),
    }
}

/// Postgres's own rows of `table` (empty for catalogs without any).
pub fn rows(table: SysTable) -> Arc<Vec<Row>> {
    rows_of(table)
}

fn rows_of(table: SysTable) -> Arc<Vec<Row>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, Arc<Vec<Row>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap().get(&table.oid()) {
        return r.clone();
    }
    let cols = table.columns();
    let fmt = FormatCtx::default();
    let parsed: Vec<Row> = match source(table) {
        Some(text) => text
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                assert_eq!(f.len(), cols.len(), "{} row {l:?}", table.name());
                f.iter()
                    .zip(cols.iter())
                    .map(|(x, (_, ty))| field(x, ty, &fmt))
                    .collect()
            })
            .collect(),
        None => Vec::new(),
    };
    let mut parsed = parsed;
    if table == SysTable::PgStatistic {
        type_stat_values(&mut parsed);
    }
    let parsed = Arc::new(parsed);
    cache.lock().unwrap().insert(table.oid(), parsed.clone());
    parsed
}

/// The oid in column `i` of a row.
pub fn oid_at(r: &[Value], i: usize) -> u32 {
    match r.get(i) {
        Some(Value::Oid(o)) => *o,
        _ => 0,
    }
}

/// The text in column `i` of a row.
pub fn text_at(r: &[Value], i: usize) -> &str {
    match r.get(i) {
        Some(Value::Text(t)) => t,
        _ => "",
    }
}

/// pg_statistic's stavaluesN as arrays of the column's type (its elements' for most-common
/// elements); text where the engine has no such scalar type.
fn type_stat_values(rows: &mut [Row]) {
    let attrs = rows_of(SysTable::PgAttribute);
    let types = rows_of(SysTable::PgType);
    let atttype: HashMap<(u32, Value), u32> = attrs
        .iter()
        .map(|a| ((oid_at(a, 0), a[4].clone()), oid_at(a, 2)))
        .collect();
    let base: HashMap<u32, u32> = types
        .iter()
        .filter(|t| t[6] == Value::Char(b'd'))
        .map(|t| (oid_at(t, 0), oid_at(t, 25)))
        .collect();
    let fmt = FormatCtx::default();
    for r in rows.iter_mut() {
        let Some(&typ) = atttype.get(&(oid_at(r, 0), r[1].clone())) else {
            continue;
        };
        let typ = base.get(&typ).copied().unwrap_or(typ);
        for slot in 0..5 {
            let Value::Array(a) = &r[26 + slot] else {
                continue;
            };
            let elem = match (&r[6 + slot], DataType::from_oid(typ)) {
                (Value::Int2(4), Some(DataType::Array(e))) => *e,
                (_, Some(t)) => t,
                _ => continue,
            };
            if matches!(elem, DataType::Array(_)) {
                continue;
            }
            let items: Option<Vec<Value>> = a
                .items
                .iter()
                .map(|v| parse_value(text_at(std::slice::from_ref(v), 0), &elem, &fmt).ok())
                .collect();
            if let Some(items) = items {
                let mut typed = (**a).clone();
                typed.elem = elem;
                typed.items = items;
                r[26 + slot] = Value::Array(Arc::new(typed));
            }
        }
    }
}
