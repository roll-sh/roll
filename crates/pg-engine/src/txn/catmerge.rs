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
    // The header: scalars and lists (comments, ACLs, memberships, ...). Only the fields this
    // transaction changed are merged (`cur`'s are the rest): serializing all three headers,
    // whose lists hold an item per commented or granted object, made every racing commit
    // O(catalog).
    let mut head = Map::new();
    for k in persist::header_changed(base, mine) {
        let v = match k {
            // (Bumped by the caller; the format is `cur`'s.)
            "version" | "catalog_format" => continue,
            // pg_class's heap order: below.
            "class_tick" => continue,
            // Counters: the larger.
            "next_oid" => Some(Value::from(mine.next_oid.max(cur.next_oid))),
            // Comments by object: the current ones, those this transaction set or removed
            // changed (pg_restore -j comments every object as it goes).
            "comments" => {
                out.comments = merge_comments(base, mine, cur);
                continue;
            }
            _ => {
                let field = |c: &Catalog| persist::header_field(c, k);
                merge_opt(field(base)?.as_ref(), field(mine)?.as_ref(), field(cur)?.as_ref())?
            }
        };
        if let Some(v) = v {
            head.insert(k.to_string(), v);
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
    // The relations whose name index entries may change (tables, indexes, views, sequences).
    let mut rels = BTreeSet::new();
    for (tag, oid, v) in changes {
        if tag == persist::CLASS_ORDER_TAG {
            moved.push((oid, v));
            continue;
        }
        if matches!(tag, 2..=5) {
            rels.insert(oid);
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
    // (The name index and children of the relations this transaction changed, and the
    // check that no two took one name: `cur`'s were whole. Rebuilding them over the whole
    // catalog, under the commit lock, made every racing DDL commit O(catalog).)
    out.reindex_rels(cur, &rels)?;
    // (Checked against the full rebuild in debug builds, over catalogs small enough not to
    // hide what the merge itself costs: tests/ddl_scaling.rs times it.)
    #[cfg(debug_assertions)]
    if out.tables.len() + out.indexes.len() < 500 {
        let mut full = out.clone();
        full.reindex();
        debug_assert!(full.names_eq(&out), "a merged catalog's name index");
        if unique_names(&full).is_err() {
            panic!("a merged catalog with a name taken twice");
        }
    }
    Ok(out)
}

/// `cur`'s comments with this transaction's changes since `base`: those it removed or set
/// (keyed by object and sub-id) go, then those it set follow in its order (a comment set
/// again is a new row at pg_description's end, as the transaction had it).
fn merge_comments(base: &Catalog, mine: &Catalog, cur: &Catalog) -> std::sync::Arc<Vec<(u32, i32, String)>> {
    let key = |c: &(u32, i32, String)| (c.0, c.1);
    let before: HashMap<(u32, i32), &String> = base.comments.iter().map(|c| (key(c), &c.2)).collect();
    let after: HashMap<(u32, i32), &String> = mine.comments.iter().map(|c| (key(c), &c.2)).collect();
    let mut touched: std::collections::HashSet<(u32, i32)> =
        before.keys().filter(|k| after.get(*k) != before.get(*k)).copied().collect();
    let set: Vec<&(u32, i32, String)> =
        mine.comments.iter().filter(|c| before.get(&key(c)) != Some(&&c.2)).collect();
    touched.extend(set.iter().map(|c| key(c)));
    let mut out: Vec<(u32, i32, String)> =
        cur.comments.iter().filter(|c| !touched.contains(&key(c))).cloned().collect();
    out.extend(set.into_iter().cloned());
    std::sync::Arc::new(out)
}

fn max_num(a: Option<&Value>, b: Option<&Value>) -> Value {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    Value::from(n(a).max(n(b)))
}

/// A prepared transaction's catalog changes, kept in its state record (`hb` / `hm`: the
/// header fields of the catalog its DDL started from and of its own; `changes`: its records
/// that differ from that base's, `persist::record_changes`), carried onto `cur` when it is
/// recovered. The objects it changed were locked since it prepared, so its records go in as
/// they are; header fields merge as in `merge`.
pub fn reapply(
    cur: &Catalog,
    hb: &Map<String, Value>,
    hm: &Map<String, Value>,
    changes: Vec<(u8, u32, Option<Bytes>)>,
) -> Result<Catalog> {
    let mut out = cur.clone();
    let hc = persist::header_fields(cur)?;
    let mut head = Map::new();
    let keys: BTreeSet<&String> = hb.keys().chain(hm.keys()).chain(hc.keys()).collect();
    for k in keys {
        let (bv, mv, cv) = (hb.get(k), hm.get(k), hc.get(k));
        let v = match k.as_str() {
            "version" | "catalog_format" | "class_tick" => continue,
            "next_oid" => Some(max_num(mv, cv)),
            _ => merge_opt(bv, mv, cv)?,
        };
        if let Some(v) = v {
            head.insert(k.clone(), v);
        }
    }
    persist::set_header(&mut out, &head)?;
    let mut set = Vec::new();
    for (tag, oid, v) in changes {
        if tag == persist::CLASS_ORDER_TAG {
            match v {
                None => {
                    out.class_order.remove(&oid);
                }
                Some(b) => set.push((serde_json::from_slice::<u64>(&b).unwrap_or(0), oid)),
            }
            continue;
        }
        persist::apply_record(&mut out, tag, oid, v.as_deref())?;
    }
    set.sort();
    for (_, oid) in set {
        out.class_tick += 1;
        out.class_order.insert(oid, out.class_tick);
    }
    out.catalog_format = cur.catalog_format;
    out.reindex();
    Ok(out)
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
            // (By the items' text in hash sets: lists such as the header's comments and ACLs
            // hold an item per object, and comparing every pair was quadratic.)
            let key = |x: &Value| x.to_string();
            let (bk, mk): (std::collections::HashSet<String>, std::collections::HashSet<String>) =
                (b.iter().map(key).collect(), m.iter().map(key).collect());
            let removed: std::collections::HashSet<&String> = bk.iter().filter(|x| !mk.contains(*x)).collect();
            let mut seen = std::collections::HashSet::new();
            let mut out: Vec<Value> = Vec::with_capacity(c.len() + m.len());
            for x in c {
                let k = key(x);
                if !removed.contains(&k) {
                    seen.insert(k);
                    out.push(x.clone());
                }
            }
            for x in m {
                let k = key(x);
                if !bk.contains(&k) && seen.insert(k) {
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
/// have taken a name. (Debug builds check `Catalog::reindex_rels`'s check with it.)
#[cfg(debug_assertions)]
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
