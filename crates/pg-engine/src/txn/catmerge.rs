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

use crate::catalog::{Catalog, persist};
use crate::error::{Error, Result, code};
use bytes::Bytes;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

fn conflict() -> Error {
    Error::new(
        code::SERIALIZATION_FAILURE,
        "could not serialize access due to concurrent schema change",
    )
}

/// `mine` (made from `base`) carried onto `cur` (also made from `base`). The catalogs are
/// compared record by record as a commit stores them (`persist::record_changes`, whose cost
/// follows the objects changed, not the catalog's size): `cur` with the records only this
/// transaction changed, those both changed merged; the header record's fields merged as
/// values. (It serialized all three catalogs whole, which `pg_restore -j` over thousands of
/// tables paid for every index and constraint: 20x a serial restore.)
pub fn merge(base: &Catalog, mine: &Catalog, cur: &Catalog) -> Result<Catalog> {
    let internal = |e: serde_json::Error| crate::error::internal(e.to_string());
    let mut out = cur.clone();
    // The header: scalars and lists (comments, ACLs, memberships, ...).
    let (hb, hm, hc) = (
        persist::header_fields(base)?,
        persist::header_fields(mine)?,
        persist::header_fields(cur)?,
    );
    let mut head = Map::new();
    let keys: BTreeSet<&String> = hb.keys().chain(hm.keys()).chain(hc.keys()).collect();
    for k in keys {
        let (bv, mv, cv) = (hb.get(k), hm.get(k), hc.get(k));
        let v = match k.as_str() {
            // (Bumped by the caller; the format is `cur`'s.)
            "version" | "catalog_format" => continue,
            // Counters: the larger.
            "next_oid" => Some(max_num(mv, cv)),
            // pg_class's heap order: below.
            "class_tick" => continue,
            _ => merge_opt(bv, mv, cv)?,
        };
        if let Some(v) = v {
            head.insert(k.clone(), v);
        }
    }
    persist::set_header(&mut out, &head)?;
    // The records: this transaction's changes, merged with the current catalog's where they
    // changed the same ones.
    type Records = HashMap<(u8, u32), Option<Bytes>>;
    let theirs: Records = persist::record_changes(base, cur)?
        .into_iter()
        .map(|(t, o, v)| ((t, o), v))
        .collect();
    let changes = persist::record_changes(base, mine)?;
    let before: Records = if changes.iter().any(|(t, o, _)| theirs.contains_key(&(*t, *o))) {
        persist::record_changes(mine, base)?
            .into_iter()
            .map(|(t, o, v)| ((t, o), v))
            .collect()
    } else {
        HashMap::new()
    };
    let mut moved = Vec::new();
    for (tag, oid, v) in changes {
        if tag == persist::CLASS_ORDER_TAG {
            moved.push((oid, v));
            continue;
        }
        let v = match theirs.get(&(tag, oid)) {
            None => v,
            Some(c) if *c == v => continue,
            Some(c) => {
                let parse = |b: &Option<Bytes>| -> Result<Option<Value>> {
                    b.as_ref()
                        .map(|b| serde_json::from_slice(b).map_err(internal))
                        .transpose()
                };
                let b = parse(before.get(&(tag, oid)).unwrap_or(&None))?;
                merge_opt(b.as_ref(), parse(&v)?.as_ref(), parse(c)?.as_ref())?
                    .map(|m| serde_json::to_vec(&m).map(Bytes::from).map_err(internal))
                    .transpose()?
            }
        };
        persist::apply_record(&mut out, tag, oid, v.as_deref()).map_err(|_| conflict())?;
    }
    class_order(mine, moved, &mut out);
    out.oids = mine.oids.clone();
    out.catalog_format = cur.catalog_format;
    out.reindex();
    unique_names(&out)?;
    Ok(out)
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
                // (A table's index counter: both counted on; ids are oids now.)
                let v = if k == "next_index_id" {
                    Some(max_num(m.get(k), c.get(k)))
                } else {
                    merge_opt(b.get(k), m.get(k), c.get(k))?
                };
                if let Some(v) = v {
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
/// this transaction set (new rows, rows updated other than in place), after them in the
/// order it set them; those it removed (dropped relations) gone. `moved`: its class_order
/// records that differ from the base's.
fn class_order(mine: &Catalog, moved: Vec<(u32, Option<Bytes>)>, out: &mut Catalog) {
    let mut tick = out.class_tick;
    let mut set = Vec::new();
    for (oid, v) in moved {
        match v {
            None => {
                out.class_order.remove(&oid);
            }
            Some(_) => set.push((mine.class_order.get(&oid).copied().unwrap_or(0), oid)),
        }
    }
    set.sort();
    for (_, oid) in set {
        tick += 1;
        out.class_order.insert(oid, tick);
    }
    out.class_tick = tick;
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
