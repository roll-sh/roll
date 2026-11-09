//! ScyllaDB's internal tables through Alternator: Query and Scan on
//! `.scylla.alternator.<keyspace>.<table>`.
//!
//! Only system keyspaces are readable; their rows come from [`Backend::internal_table`] (the
//! in-memory backend has none). A Query conditions the first partition key column (and the
//! first clustering column) like a table's hash (and range) key; LastEvaluatedKey and
//! ExclusiveStartKey hold every primary key column. `Segment`, `TotalSegments` and `IndexName`
//! are ignored.

use super::*;
use crate::backend::{InternalTable, RangePage, RangeRequest, Scope};
use crate::schema::KeySchema;
use crate::value::Item;

/// Keyspaces whose tables can be read through internal names.
const SYSTEM_KEYSPACES: [&str; 9] = [
    "audit",
    "system",
    "system_schema",
    "system_auth",
    "system_distributed",
    "system_distributed_everywhere",
    "system_traces",
    "system_replicated_keys",
    "system_views",
];

/// Resolves an internal table name (`.scylla.alternator.KS.T`) to its rows. `Ok(None)` when
/// `name` doesn't use the prefix.
pub(crate) async fn resolve(alt: &Alternator, name: &str) -> Result<Option<InternalTable>> {
    let Some(rest) = name.strip_prefix(INTERNAL_PREFIX) else {
        return Ok(None);
    };
    let Some((ks, table)) = rest.split_once('.') else {
        return Err(table_not_found(name));
    };
    if !SYSTEM_KEYSPACES.contains(&ks) {
        return Err(table_not_found(name));
    }
    check_table_name(table)?;
    match alt.backend.internal_table(ks, table).await? {
        Some(mut t) => {
            if (ks, table) == ("system", "config") {
                let o = alt.option_overrides();
                for row in &mut t.rows {
                    let name = row
                        .get("name")
                        .and_then(|n| n.get("S"))
                        .and_then(Json::as_str);
                    if let Some(v) = name.and_then(|n| o.get(n)) {
                        row.insert(
                            "value".into(),
                            Json::object().with("S", Json::str(v.clone())),
                        );
                    }
                }
            }
            Ok(Some(t))
        }
        None => Err(Error::not_found(format!(
            "Requested resource not found: Internal table: {ks}.{table} not found"
        ))),
    }
}

/// The table schema a Query or Scan of an internal table validates against.
pub(crate) fn schema_of(name: &str, t: &InternalTable) -> Option<TableSchema> {
    let hash = t.key.first()?.clone();
    let range = t.key.get(t.partition_key.max(1)).cloned();
    Some(TableSchema {
        name: name.to_string(),
        id: uuid::Uuid::nil(),
        created_at_ms: 0,
        key: KeySchema { hash, range },
        attribute_definitions: t.key.clone(),
        gsis: Vec::new(),
        lsis: Vec::new(),
        billing_mode: "PAY_PER_REQUEST".into(),
        billing_changed_ms: 0,
        provisioned: (0, 0),
        stream: None,
        tags: Vec::new(),
        ttl_attribute: None,
        options: Vec::new(),
        vector_indexes: Vec::new(),
        tablets: false,
    })
}

/// Writes to internal tables are refused unless `alternator_allow_system_table_write` is on;
/// then a PutItem or UpdateItem of `system.config` sets an option ([`Alternator::set_option`]),
/// answered with `Some(response)`.
pub(crate) async fn refuse_write(alt: &Alternator, req: &Request<'_>) -> Result<Option<Json>> {
    let body = req.body;
    let Some(name) = body.get("TableName").and_then(Json::as_str) else {
        return Ok(None);
    };
    if resolve(alt, name).await?.is_none() {
        return Ok(None);
    }
    let config = name == format!("{INTERNAL_PREFIX}system.config");
    if alt.config().allow_system_table_write && config && req.op != "DeleteItem" {
        super::authorize(
            alt,
            req,
            crate::backend::Permission::Modify,
            crate::backend::Resource::Table {
                keyspace: "system".into(),
                table: "config".into(),
            },
        )
        .await?;
        if let Some(role) = req.user
            && !alt.backend.is_superuser(role).await?
        {
            return Err(Error::new(
                crate::error::ErrorKind::AccessDenied,
                format!(
                    "MODIFY access on table system.config is denied to role {role}: writing a system table needs a superuser"
                ),
            ));
        }
        let s = |v: Option<&Json>| {
            v.and_then(|v| v.get("S"))
                .and_then(Json::as_str)
                .map(str::to_string)
        };
        let text = |v: &Json| -> Option<String> {
            let (t, p) = crate::storage::single_member(v)?;
            match (t, p) {
                ("S" | "N", Json::String(x)) => Some(x.clone()),
                ("BOOL", Json::Bool(b)) => Some(b.to_string()),
                _ => None,
            }
        };
        let (option, value) = match req.op {
            "PutItem" => {
                let item = body.get("Item");
                (
                    s(item.and_then(|i| i.get("name"))),
                    item.and_then(|i| i.get("value")).and_then(text),
                )
            }
            _ => {
                let option = s(body.get("Key").and_then(|k| k.get("name")));
                let mut value = None;
                if let Some(u) = body.get("UpdateExpression").and_then(Json::as_str)
                    && let Some((lhs, rhs)) =
                        u.trim().strip_prefix("SET").and_then(|r| r.split_once('='))
                {
                    let lhs = lhs.trim();
                    let attr = match lhs.strip_prefix('#') {
                        Some(_) => body
                            .get("ExpressionAttributeNames")
                            .and_then(|n| n.get(lhs))
                            .and_then(Json::as_str),
                        None => Some(lhs),
                    };
                    if attr == Some("value") {
                        value = body
                            .get("ExpressionAttributeValues")
                            .and_then(|v| v.get(rhs.trim()))
                            .and_then(text);
                    }
                } else if let Some(v) = body
                    .get("AttributeUpdates")
                    .and_then(|u| u.get("value"))
                    .and_then(|v| v.get("Value"))
                {
                    value = text(v);
                }
                (option, value)
            }
        };
        let Some(option) = option else {
            return Err(Error::validation("Missing the key attribute name"));
        };
        alt.set_option(&option, value.as_deref());
        return Ok(Some(Json::object()));
    }
    Err(Error::not_found(format!(
        "Table {name} is an internal table, and writing to it is forbidden by the alternator_allow_system_table_write configuration"
    )))
}

fn key_eq(a: Option<&Json>, b: &Json) -> bool {
    a.and_then(crate::storage::encode_key) == crate::storage::encode_key(b)
}

/// One page of an internal table's rows, as a backend range read would return it.
pub(crate) fn page(t: &InternalTable, schema: &TableSchema, req: &RangeRequest) -> RangePage {
    let hash = &schema.key.hash;
    let mut rows: Vec<&Item> = t
        .rows
        .iter()
        .zip(t.tokens.iter().copied().chain(std::iter::repeat(0)))
        .filter(|(it, token)| match &req.scope {
            Scope::Partition { hash: h, sort } => {
                key_eq(it.get(&hash.name), h)
                    && sort.as_ref().is_none_or(|c| {
                        schema.key.range.as_ref().is_some_and(|r| {
                            it.get(&r.name)
                                .is_some_and(|v| crate::memory::sort_matches_value(r.ty, v, c))
                        })
                    })
            }
            Scope::Tokens { start, end } => token >= start && token <= end,
        })
        .map(|(it, _)| it)
        .collect();
    if !req.forward {
        rows.reverse();
    }
    if let Some(start) = &req.start_after {
        let same = |it: &&Item| t.key.iter().all(|k| it.get(&k.name) == start.get(&k.name));
        if let Some(i) = rows.iter().position(same) {
            rows.drain(..=i);
        }
    }
    let more = rows.len() >= req.limit;
    RangePage {
        items: rows.into_iter().take(req.limit).cloned().collect(),
        more,
        resume: None,
    }
}
