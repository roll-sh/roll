//! BatchWriteItem and BatchGetItem.
//!
//! The whole batch is validated before anything is written or read. Writes are unconditional
//! and independent (not atomic), as in DynamoDB; nothing is ever left unprocessed.

use super::*;
use crate::backend::{Mutation, WriteFn};
use crate::expr::project::project;
use crate::expr::resolve::Bindings;
use crate::ops::item::projection;
use crate::size;
use crate::value::Item;

/// Keys compared as BatchWriteItem's duplicate check does: numbers by value.
fn same_key(a: &crate::backend::PrimaryKey, b: &crate::backend::PrimaryKey) -> bool {
    fn eq(x: &Json, y: &Json) -> bool {
        match (single_member(x), single_member(y)) {
            (Some(("N", Json::String(p))), Some(("N", Json::String(q)))) => {
                match (
                    crate::number::Number::parse(p),
                    crate::number::Number::parse(q),
                ) {
                    (Ok(p), Ok(q)) => p == q,
                    _ => p == q,
                }
            }
            _ => x == y,
        }
    }
    eq(&a.hash, &b.hash)
        && match (&a.range, &b.range) {
            (Some(x), Some(y)) => eq(x, y),
            (None, None) => true,
            _ => false,
        }
}

fn check_duplicate(writes: &[Write], key: &crate::backend::PrimaryKey) -> Result<()> {
    for w in writes {
        let (Write::Put(k, _) | Write::Delete(k)) = w;
        if same_key(k, key) {
            return Err(Error::validation(
                "Provided list of item keys contains duplicates",
            ));
        }
    }
    Ok(())
}

enum Write {
    Put(crate::backend::PrimaryKey, Item),
    Delete(crate::backend::PrimaryKey),
}

pub async fn batch_write_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let Some(ri) = body.get("RequestItems") else {
        return Err(Error::validation(
            "BatchWriteItem content is missing a mandatory member RequestItems",
        ));
    };
    let Some(tables) = ri.as_object() else {
        return Err(Error::validation("RequestItems must be an object"));
    };
    if tables.is_empty() {
        return Err(Error::validation("RequestItems can't be empty"));
    }
    let mut total = 0usize;
    for (_, list) in tables {
        match list.as_array() {
            Some(a) if !a.is_empty() => total += a.len(),
            _ => {
                return Err(Error::validation(
                    "Member of RequestItems must be a non-empty array of WriteRequest objects",
                ));
            }
        }
    }
    let max = alt.config().max_items_in_batch_write;
    if total > max {
        return Err(Error::validation(format!(
            "Invalid length of BatchWriteItem command, got {total} items, maximum is {max} (from configuration variable alternator_max_items_in_batch_write)"
        )));
    }
    let rcc = return_consumed_capacity(body)?;
    let mut plan: Vec<(Arc<TableSchema>, Vec<Write>)> = Vec::new();
    for (name, list) in tables {
        let schema = get_table_named(alt, name).await?;
        authorize_table(alt, req, crate::backend::Permission::Modify, &schema.name).await?;
        let mut writes = Vec::new();
        for w in list.as_array().unwrap_or_default() {
            let (kind, v) = match w.as_object() {
                Some([(k, v)]) => (k.as_str(), v),
                _ => {
                    return Err(Error::validation(
                        "RequestItems element: expected an object with a single member.",
                    ));
                }
            };
            match kind {
                "PutRequest" => {
                    if !v.is_object() {
                        return Err(Error::validation("PutRequest must be an object"));
                    }
                    let Some(item) = v.get("Item") else {
                        return Err(Error::validation(
                            "PutRequest is missing a mandatory member Item",
                        ));
                    };
                    if !item.is_object() {
                        return Err(Error::validation("Item in PutRequest must be an object"));
                    }
                    let key = primary_key(item, &schema, false)?;
                    let mut it = Item::new();
                    for (n, x) in item.as_object().unwrap_or_default() {
                        if it.contains_key(n) {
                            continue;
                        }
                        if !schema.key.contains(n) {
                            value::validate(x, "PutItem")?;
                            if let Err(e) = crate::expr::resolve::check_attr_name(n) {
                                return Err(Error::validation(e));
                            }
                            super::item::check_index_key(&schema, n, x)?;
                        }
                        it.insert(n.clone(), x.clone());
                    }
                    super::vector::check_item(&schema, &it, "PutItem")?;
                    check_duplicate(&writes, &key)?;
                    writes.push(Write::Put(key, it));
                }
                "DeleteRequest" => {
                    if !v.is_object() {
                        return Err(Error::validation("DeleteRequest must be an object"));
                    }
                    let Some(k) = v.get("Key") else {
                        return Err(Error::validation(
                            "DeleteRequest is missing a mandatory member Key",
                        ));
                    };
                    if !k.is_object() {
                        return Err(Error::validation("Key in DeleteRequest must be an object"));
                    }
                    let key = primary_key(k, &schema, true)?;
                    check_duplicate(&writes, &key)?;
                    writes.push(Write::Delete(key));
                }
                other => {
                    return Err(Error::validation(format!(
                        "Unknown BatchWriteItem request type: {other}"
                    )));
                }
            }
        }
        plan.push((schema, writes));
    }
    for (_, writes) in &plan {
        for w in writes {
            let (Write::Put(k, _) | Write::Delete(k)) = w;
            check_key_length(k)?;
        }
    }
    let mut consumed = Vec::new();
    let force_read = alt.config().force_read_before_write;
    for (schema, writes) in plan {
        let mut units = 0u64;
        req.stats.batch_items(&schema.name, writes.len() as u64);
        for w in writes {
            let (key, m, size, op) = match w {
                Write::Put(k, item) => {
                    let n = size::item_size(&item);
                    (k, Mutation::Put { item, rmw: false }, Some(n), "PutItem")
                }
                Write::Delete(k) => (k, Mutation::Delete, None, "DeleteItem"),
            };
            let f: WriteFn = Box::new(move |_| Ok(Some(m)));
            let out = alt.backend.write_item(&schema, &key, f).await?;
            super::note_write(alt, req, &schema, false, 0);
            // With `alternator_force_read_before_write`, the old item's size counts too.
            let old = out.old.as_ref().map(size::item_size).filter(|_| force_read);
            let wcu = size::write_units(size.unwrap_or(0).max(old.unwrap_or(0)));
            units += wcu;
            req.stats.wcu_as(op, &schema.name, wcu as f64);
            if let Some(n) = size.or(old).map(|n| n.max(old.unwrap_or(0))) {
                req.stats.item_size(&schema.name, n);
            }
        }
        consumed.push(
            Json::object()
                .with("TableName", Json::str(schema.name.clone()))
                .with("CapacityUnits", Json::u64(units)),
        );
    }
    let mut out = Json::object().with("UnprocessedItems", Json::object());
    if rcc {
        out.push("ConsumedCapacity", Json::Array(consumed));
    }
    Ok(out)
}

pub async fn batch_get_item(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let Some(ri) = body.get("RequestItems") else {
        return Err(json_assert("IsObject()"));
    };
    let Some(tables) = ri.as_object() else {
        return Err(json_assert("IsObject()"));
    };
    let rcc = return_consumed_capacity(body)?;
    struct Read {
        schema: Arc<TableSchema>,
        keys: Vec<crate::backend::PrimaryKey>,
        paths: Option<Vec<crate::expr::resolve::RPath>>,
        consistent: bool,
    }
    let mut reads = Vec::new();
    for (name, spec) in tables {
        let schema = get_table_named(alt, name).await?;
        authorize_table(alt, req, crate::backend::Permission::Select, &schema.name).await?;
        if !spec.is_object() {
            return Err(json_assert("IsObject()"));
        }
        let Some(keys) = spec.get("Keys").and_then(Json::as_array) else {
            return Err(json_assert("IsArray()"));
        };
        let consistent = consistent_read(spec)?;
        let mut b = Bindings::new(spec);
        let paths = projection(spec, &mut b, "GetItem")?;
        b.mark_all_values_used();
        b.check_unused("GetItem")?;
        let mut parsed: Vec<crate::backend::PrimaryKey> = Vec::with_capacity(keys.len());
        for k in keys {
            if !k.is_object() {
                return Err(json_assert("IsObject()"));
            }
            let key = primary_key(k, &schema, true)?;
            if parsed.iter().any(|p| same_key(p, &key)) {
                return Err(Error::validation(
                    "Provided list of item keys contains duplicates",
                ));
            }
            parsed.push(key);
        }
        reads.push(Read {
            schema,
            keys: parsed,
            paths,
            consistent,
        });
    }
    for r in &reads {
        for k in &r.keys {
            check_key_length(k)?;
        }
    }
    let mut responses = Json::object();
    let mut consumed = Vec::new();
    for r in reads {
        let mut found = Vec::new();
        let mut units = 0.0;
        req.stats.table(&r.schema.name);
        req.stats.batch_items(&r.schema.name, r.keys.len() as u64);
        req.stats.storage(&r.schema.name, false);
        for k in &r.keys {
            if let Some(it) = alt.backend.get_item(&r.schema, k).await? {
                let n = size::item_size(&it);
                units += size::read_units(Some(n), r.consistent);
                req.stats.item_size(&r.schema.name, n);
                found.push(it);
            }
        }
        req.stats.rcu(&r.schema.name, units);
        // ScyllaDB groups the rows by partition in a hash map; with one or two partitions
        // that is reverse request order, rows of a partition staying in clustering order.
        let ordered: Vec<Item> = order_like_scylla(&r.schema, found)
            .into_iter()
            .map(|it| match &r.paths {
                Some(p) => project(&it, p),
                None => it,
            })
            .collect();
        if !r.keys.is_empty() {
            responses.push(
                r.schema.name.clone(),
                Json::Array(ordered.iter().map(item_json).collect()),
            );
        }
        consumed.push(
            Json::object()
                .with("TableName", Json::str(r.schema.name.clone()))
                .with("CapacityUnits", Json::f64(units)),
        );
    }
    let mut out = Json::object()
        .with("Responses", responses)
        .with("UnprocessedKeys", Json::object());
    if rcc {
        out.push("ConsumedCapacity", Json::Array(consumed));
    }
    Ok(out)
}

/// Groups items by partition (partitions in reverse order of first appearance), keeping the
/// partition's rows in clustering order.
fn order_like_scylla(schema: &TableSchema, items: Vec<Item>) -> Vec<Item> {
    let hash = &schema.key.hash.name;
    let mut groups: Vec<(Json, Vec<Item>)> = Vec::new();
    for it in items {
        let h = it.get(hash).cloned().unwrap_or(Json::Null);
        match groups.iter_mut().find(|(k, _)| *k == h) {
            Some((_, v)) => v.push(it),
            None => groups.push((h, vec![it])),
        }
    }
    let range = schema.key.range.clone();
    let mut out = Vec::new();
    for (_, mut rows) in groups.into_iter().rev() {
        if let Some(r) = &range {
            rows.sort_by(|a, b| {
                let ka = a
                    .get(&r.name)
                    .and_then(crate::storage::encode_key)
                    .unwrap_or_default();
                let kb = b
                    .get(&r.name)
                    .and_then(crate::storage::encode_key)
                    .unwrap_or_default();
                let t = match r.ty {
                    crate::schema::ScalarType::S => nosql_types::CqlType::Text,
                    crate::schema::ScalarType::N => nosql_types::CqlType::Decimal,
                    crate::schema::ScalarType::B => nosql_types::CqlType::Blob,
                };
                nosql_types::compare(&t, &ka, &kb)
            });
        }
        out.extend(rows);
    }
    out
}
