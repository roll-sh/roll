//! ScyllaDB's internal tables (`.scylla.alternator.KS.T`): the rows of the engine's system
//! tables, read with CQL and shaped as ScyllaDB's Alternator shapes them (from the reference):
//! `text` is `S`, `blob` is `B`, `boolean` is `BOOL` holding the *string* `"true"` / `"false"`,
//! and every other type is `S` holding its text form, maps as `({k : v}, {k2 : v2})` and sets
//! and lists as their elements joined by `, `. Null cells are left out.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use nosql_engine::{Database, QueryParams, ResponseBody};
use nosql_types::{CqlType, Dialect};

use super::engine_error;
use crate::backend::InternalTable;
use crate::error::Result;
use crate::json::Json;
use crate::schema::{KeyAttr, ScalarType};
use crate::value::Item;

fn text(ty: &CqlType, b: &[u8]) -> String {
    match ty {
        CqlType::Map { key, value, .. } => {
            let entries = nosql_types::map_entries(b).unwrap_or_default();
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{{{} : {}}}", text(key, k), text(value, v)))
                .collect();
            format!("({})", parts.join(", "))
        }
        CqlType::Set { elem, .. } | CqlType::List { elem, .. } => {
            let elems = nosql_types::collection_elements(b).unwrap_or_default();
            elems
                .iter()
                .map(|e| e.map_or_else(|| "null".to_string(), |e| text(elem, e)))
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => nosql_types::to_text(ty, b, Dialect::Scylla)
            .ok()
            .flatten()
            .unwrap_or_default(),
    }
}

fn value(ty: &CqlType, b: &[u8]) -> Json {
    let one = |t: &str, v: Json| Json::object().with(t, v);
    match ty {
        CqlType::Text | CqlType::Ascii => one("S", Json::str(String::from_utf8_lossy(b))),
        CqlType::Blob => one("B", Json::str(STANDARD.encode(b))),
        CqlType::Boolean => one(
            "BOOL",
            Json::str(if b.first().is_some_and(|x| *x != 0) {
                "true"
            } else {
                "false"
            }),
        ),
        _ => one("S", Json::str(text(ty, b))),
    }
}

fn key_type(ty: &CqlType) -> ScalarType {
    match ty {
        CqlType::Blob => ScalarType::B,
        _ => ScalarType::S,
    }
}

pub async fn rows(
    db: &Arc<Database>,
    keyspace: &str,
    table: &str,
) -> Result<Option<InternalTable>> {
    let schema = db.schema();
    let Some(t) = schema
        .keyspace(keyspace)
        .filter(|k| k.is_builtin())
        .and_then(|k| k.tables.get(table))
        .cloned()
    else {
        return Ok(None);
    };
    let key: Vec<KeyAttr> = t
        .partition_key_columns()
        .chain(t.clustering_columns())
        .map(|c| KeyAttr {
            name: c.name.clone(),
            ty: key_type(&c.ty),
        })
        .collect();
    let mut session = db.connect(Default::default());
    session.state.user = db.config().superuser.as_ref().map(|s| s.name.clone());
    let q = format!(
        "SELECT * FROM {}.{}",
        nosql_types::quote_identifier(keyspace, Dialect::Scylla),
        nosql_types::quote_identifier(table, Dialect::Scylla)
    );
    let resp = session
        .query(&q, QueryParams::default())
        .await
        .map_err(engine_error)?;
    let ResponseBody::Rows(r) = resp.body else {
        return Ok(None);
    };
    let pk: Vec<usize> = t
        .partition_key_columns()
        .filter_map(|c| r.metadata.columns.iter().position(|s| s.name == c.name))
        .collect();
    let tokens = r
        .rows
        .iter()
        .map(|row| {
            let parts: Vec<&[u8]> = pk
                .iter()
                .map(|&i| row.get(i).and_then(|c| c.as_deref()).unwrap_or(&[]))
                .collect();
            nosql_types::partition_key(&parts)
                .map(|k| nosql_types::token(&k))
                .unwrap_or(0)
        })
        .collect();
    let rows = r
        .rows
        .iter()
        .map(|row| {
            let mut item = Item::new();
            for (spec, cell) in r.metadata.columns.iter().zip(row) {
                if let Some(b) = cell {
                    item.insert(spec.name.clone(), value(&spec.ty, b));
                }
            }
            item
        })
        .collect();
    Ok(Some(InternalTable {
        key,
        rows,
        tokens,
        partition_key: t.partition_key.len(),
    }))
}
