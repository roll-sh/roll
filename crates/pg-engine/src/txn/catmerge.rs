//! Concurrent DDL: a transaction's catalog merged onto the catalog other transactions
//! committed since its DDL began.
//!
//! The catalog is one value, versioned as a whole, where Postgres has rows in catalog tables
//! that transactions change independently, serialized by locks on the objects (two CREATE
//! INDEXes on different tables, or on one table, run at once; `pg_restore -j` makes a
//! database's indexes and constraints that way). So a commit whose catalog's base is no
//! longer the current one carries its changes over: a three-way merge of the serialized
//! catalogs (base, this transaction's, current). Objects (map entries, keyed by oid) only
//! one side changed take that side's version; an object both changed is merged field by
//! field; a list both changed (a table's indexes, a set of oids) keeps the current one's
//! items, less those this transaction removed, plus those it added. What can't be merged
//! (one value both changed differently, a name both took) fails as a serialization failure,
//! which is what the transaction got before.

use crate::catalog::Catalog;
use crate::error::{Error, Result, code};
use serde_json::{Map, Value};

fn conflict() -> Error {
    Error::new(
        code::SERIALIZATION_FAILURE,
        "could not serialize access due to concurrent schema change",
    )
}

/// `mine` (made from `base`) carried onto `cur` (also made from `base`).
pub fn merge(base: &Catalog, mine: &Catalog, cur: &Catalog) -> Result<Catalog> {
    let to = |c: &Catalog| serde_json::to_value(c).map_err(|e| crate::error::internal(e.to_string()));
    let (b, m, c) = (to(base)?, to(mine)?, to(cur)?);
    let (Value::Object(b), Value::Object(m), Value::Object(c)) = (b, m, c) else {
        return Err(conflict());
    };
    let mut out = Map::new();
    let keys: std::collections::BTreeSet<&String> = b.keys().chain(m.keys()).chain(c.keys()).collect();
    for k in keys {
        let (bv, mv, cv) = (b.get(k), m.get(k), c.get(k));
        let v = match k.as_str() {
            // (Bumped by the caller.)
            "version" => cv.cloned(),
            // Counters: the larger.
            "next_oid" => Some(max_num(mv, cv)),
            // pg_class's heap order: this transaction's new positions go after the current
            // catalog's.
            "class_order" | "class_tick" => continue,
            _ => merge_opt(bv, mv, cv)?,
        };
        if let Some(v) = v {
            out.insert(k.clone(), v);
        }
    }
    class_order(&b, &m, &c, &mut out);
    let mut merged: Catalog =
        serde_json::from_value(Value::Object(out)).map_err(|_| conflict())?;
    merged.oids = mine.oids.clone();
    // (Not serialized: as stored, `cur`'s.)
    merged.catalog_format = cur.catalog_format;
    merged.reindex();
    unique_names(&merged)?;
    Ok(merged)
}

fn max_num(a: Option<&Value>, b: Option<&Value>) -> Value {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    Value::from(n(a).max(n(b)))
}

/// A value that may be absent on any side (absent: not there, or a default skipped).
fn merge_opt(b: Option<&Value>, m: Option<&Value>, c: Option<&Value>) -> Result<Option<Value>> {
    if m == b {
        return Ok(c.cloned());
    }
    if c == b || c == m {
        return Ok(m.cloned());
    }
    match (b, m, c) {
        // Both changed it: merged when both still have it.
        (_, Some(m), Some(c)) => {
            let empty = match m {
                Value::Object(_) => Value::Object(Map::new()),
                Value::Array(_) => Value::Array(Vec::new()),
                _ => Value::Null,
            };
            merge_value(b.unwrap_or(&empty), m, c).map(Some)
        }
        _ => Err(conflict()),
    }
}

fn merge_value(b: &Value, m: &Value, c: &Value) -> Result<Value> {
    if m == b {
        return Ok(c.clone());
    }
    if c == b || c == m {
        return Ok(m.clone());
    }
    match (b, m, c) {
        (Value::Object(b), Value::Object(m), Value::Object(c)) => {
            let mut out = Map::new();
            let keys: std::collections::BTreeSet<&String> =
                b.keys().chain(m.keys()).chain(c.keys()).collect();
            for k in keys {
                if let Some(v) = merge_opt(b.get(k), m.get(k), c.get(k))? {
                    out.insert(k.clone(), v);
                }
            }
            Ok(Value::Object(out))
        }
        (Value::Array(b), Value::Array(m), Value::Array(c)) => {
            let removed: Vec<&Value> = b.iter().filter(|x| !m.contains(x)).collect();
            let mut out: Vec<Value> = c.iter().filter(|x| !removed.contains(x)).cloned().collect();
            for x in m.iter().filter(|x| !b.contains(x)) {
                if !out.contains(x) {
                    out.push(x.clone());
                }
            }
            Ok(Value::Array(out))
        }
        _ => Err(conflict()),
    }
}

/// `class_order` / `class_tick`: the current catalog's, plus the relations whose position
/// this transaction set (new rows, rows updated other than in place), after them.
fn class_order(b: &Map<String, Value>, m: &Map<String, Value>, c: &Map<String, Value>, out: &mut Map<String, Value>) {
    let obj = |v: Option<&Value>| v.and_then(Value::as_object).cloned().unwrap_or_default();
    let (bo, mo, mut co) = (obj(b.get("class_order")), obj(m.get("class_order")), obj(c.get("class_order")));
    let mut tick = c.get("class_tick").and_then(Value::as_u64).unwrap_or(0);
    // Removed here (the relation dropped).
    for k in bo.keys().filter(|k| !mo.contains_key(*k)) {
        co.remove(k);
    }
    let mut moved: Vec<(u64, &String)> = mo
        .iter()
        .filter(|(k, v)| bo.get(*k) != Some(*v))
        .map(|(k, v)| (v.as_u64().unwrap_or(0), k))
        .collect();
    moved.sort();
    for (_, k) in moved {
        tick += 1;
        co.insert(k.clone(), Value::from(tick));
    }
    out.insert("class_order".into(), Value::Object(co));
    out.insert("class_tick".into(), Value::from(tick));
}

/// pg_class_relname_nsp_index / pg_type_typname_nsp_index: two transactions may not both
/// have taken a name.
fn unique_names(c: &Catalog) -> Result<()> {
    let mut rels = std::collections::HashSet::new();
    let mut take = |schema: u32, name: &str| {
        if rels.insert((schema, name.to_string())) {
            Ok(())
        } else {
            Err(Error::new(
                code::UNIQUE_VIOLATION,
                "duplicate key value violates unique constraint \"pg_class_relname_nsp_index\"",
            )
            .detail(format!("Key (relname, relnamespace)=({name}, {schema}) already exists.")))
        }
    };
    for t in c.tables.values() {
        take(t.schema, &t.name)?;
    }
    for v in c.views.values() {
        take(v.schema, &v.name)?;
    }
    for s in c.sequences.values() {
        take(s.schema, &s.name)?;
    }
    for i in c.indexes.values() {
        // (An index is in its table's schema.)
        if let Some(t) = c.index_table(i.table) {
            take(t.schema, &i.name)?;
        }
    }
    Ok(())
}
