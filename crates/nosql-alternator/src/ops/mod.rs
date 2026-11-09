//! The operations, and the request-reading helpers they share.

mod batch;
mod internal;
mod item;
mod query;
mod streams;
mod table;
mod tags;
pub(crate) mod vector;

use std::sync::Arc;

use crate::backend::PrimaryKey;
use crate::error::{Error, ErrorKind, Result};
use crate::json::Json;
use crate::schema::{KeyAttr, TableSchema};
use crate::service::{Alternator, Request};
use crate::storage::single_member;
use crate::value::{self, json_assert};

/// Runs an operation, traced ([`crate::trace`]) and audited ([`crate::audit`]) when those
/// pick it.
pub(crate) async fn dispatch(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let audit = crate::audit::plan(alt, req).await.unwrap_or(None);
    let trace = crate::trace::begin(alt, req);
    let r = run(alt, req).await;
    if let Some(t) = trace {
        crate::trace::end(t, req, &r).await;
    }
    if let Some(plan) = audit {
        crate::audit::log(alt, req, plan, r.is_err()).await;
    }
    r
}

/// Runs an operation. Each operation's future is boxed so the dispatcher stays small.
async fn run(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    // The table an operation names (for the per-table metrics).
    let named = req
        .body
        .get("TableName")
        .and_then(Json::as_str)
        .map(str::to_string)
        .or_else(|| {
            let arn = req.body.get("ResourceArn").and_then(Json::as_str)?;
            Some(arn.rsplit_once("table/")?.1.to_string())
        })
        .filter(|n| !n.starts_with(INTERNAL_PREFIX) && !n.is_empty());
    if let Some(n) = &named
        && !matches!(req.op, "ListStreams" | "BatchWriteItem" | "BatchGetItem")
    {
        req.stats.table(n);
    }
    match req.op {
        "CreateTable" => Box::pin(table::create_table(alt, req)).await,
        "DescribeTable" => Box::pin(table::describe_table(alt, req)).await,
        "DeleteTable" => Box::pin(table::delete_table(alt, req)).await,
        "UpdateTable" => Box::pin(table::update_table(alt, req)).await,
        "ListTables" => Box::pin(table::list_tables(alt, req)).await,
        "DescribeEndpoints" => Box::pin(table::describe_endpoints(alt, req)).await,
        "DescribeContinuousBackups" => Box::pin(table::describe_continuous_backups(alt, req)).await,
        "PutItem" => Box::pin(item::put_item(alt, req)).await,
        "GetItem" => Box::pin(item::get_item(alt, req)).await,
        "DeleteItem" => Box::pin(item::delete_item(alt, req)).await,
        "UpdateItem" => Box::pin(item::update_item(alt, req)).await,
        "Query" => Box::pin(query::query(alt, req)).await,
        "Scan" => Box::pin(query::scan(alt, req)).await,
        "BatchWriteItem" => Box::pin(batch::batch_write_item(alt, req)).await,
        "BatchGetItem" => Box::pin(batch::batch_get_item(alt, req)).await,
        "TagResource" => Box::pin(tags::tag_resource(alt, req)).await,
        "UntagResource" => Box::pin(tags::untag_resource(alt, req)).await,
        "ListTagsOfResource" => Box::pin(tags::list_tags_of_resource(alt, req)).await,
        "UpdateTimeToLive" => Box::pin(tags::update_time_to_live(alt, req)).await,
        "DescribeTimeToLive" => Box::pin(tags::describe_time_to_live(alt, req)).await,
        "ListStreams" => Box::pin(streams::list_streams(alt, req)).await,
        "DescribeStream" => Box::pin(streams::describe_stream(alt, req)).await,
        "GetShardIterator" => Box::pin(streams::get_shard_iterator(alt, req)).await,
        "GetRecords" => Box::pin(streams::get_records(alt, req)).await,
        op => Err(Error::new(
            ErrorKind::UnknownOperation,
            format!("Unsupported operation {op}"),
        )),
    }
}

/// Prefix of the virtual tables that expose ScyllaDB's internal tables.
pub const INTERNAL_PREFIX: &str = ".scylla.alternator.";

/// `TableName` as every operation reads it.
pub(crate) fn table_name(body: &Json) -> Result<&str> {
    let Some(v) = body.get("TableName") else {
        return Err(Error::validation("Missing TableName field in request"));
    };
    let Some(name) = v.as_str() else {
        return Err(Error::validation("Non-string TableName field in request"));
    };
    Ok(name)
}

/// The length and character rules of table and index names.
pub(crate) fn check_table_name(name: &str) -> Result<()> {
    if name.len() < 3 || name.len() > 192 {
        return Err(Error::validation(
            "TableName must be at least 3 characters long and at most 192 characters long",
        ));
    }
    if !valid_name_chars(name) {
        return Err(Error::validation(
            "TableName must satisfy regular expression pattern: [a-zA-Z0-9_.-]+",
        ));
    }
    Ok(())
}

pub(crate) fn valid_name_chars(name: &str) -> bool {
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

pub(crate) fn table_not_found(name: &str) -> Error {
    Error::not_found(format!(
        "Requested resource not found: Table: {name} not found"
    ))
}

/// Checks the authenticated role's permission (`alternator_enforce_authorization`; requests
/// that weren't authenticated aren't checked).
pub(crate) async fn authorize(
    alt: &Alternator,
    req: &Request<'_>,
    perm: crate::backend::Permission,
    on: crate::backend::Resource,
) -> Result<()> {
    let Some(role) = req.user else {
        return Ok(());
    };
    match alt.backend.authorize(role, perm, &on).await {
        Ok(()) => Ok(()),
        Err(e) => {
            req.stats.authorization_failure();
            // `alternator_warn_authorization` without enforcement: counted, not refused.
            if alt.config().enforce_authorization {
                Err(e)
            } else {
                Ok(())
            }
        }
    }
}

/// Records what a write did for the metrics: its table, units, whether it read the item first
/// and whether it was an LWT (by the table's write isolation).
pub(crate) fn note_write(
    alt: &Alternator,
    req: &Request<'_>,
    schema: &TableSchema,
    rmw: bool,
    units: u64,
) {
    use crate::service::WriteIsolation as W;
    let lwt = match tags::write_isolation(alt, schema) {
        W::AlwaysUseLwt => true,
        W::OnlyRmwUsesLwt => rmw,
        W::ForbidRmw | W::UnsafeRmw => false,
    };
    req.stats.table(&schema.name);
    req.stats.wcu(&schema.name, units as f64);
    req.stats.write(
        &schema.name,
        rmw || alt.config().force_read_before_write,
        lwt,
    );
    req.stats.storage(&schema.name, true);
}

/// [`authorize`] on table `table` itself.
pub(crate) async fn authorize_table(
    alt: &Alternator,
    req: &Request<'_>,
    perm: crate::backend::Permission,
    table: &str,
) -> Result<()> {
    authorize(alt, req, perm, crate::backend::Resource::of(table, table)).await
}

/// Looks up the table a request names (validating the name first).
pub(crate) async fn get_table(alt: &Alternator, body: &Json) -> Result<Arc<TableSchema>> {
    let name = table_name(body)?;
    get_table_named(alt, name).await
}

pub(crate) async fn get_table_named(alt: &Alternator, name: &str) -> Result<Arc<TableSchema>> {
    check_table_name(name)?;
    lookup_table(alt, name).await
}

/// Looks up a table without validating its name (Query and Scan, which also accept internal
/// table names).
pub(crate) async fn lookup_table(alt: &Alternator, name: &str) -> Result<Arc<TableSchema>> {
    if name.len() < 3 || name.len() > 222 || !valid_name_chars(name) {
        return Err(table_not_found(name));
    }
    alt.backend
        .table(name)
        .await?
        .ok_or_else(|| table_not_found(name))
}

/// A key attribute value: a one-member object of the key's type with a non-empty, valid
/// payload.
pub(crate) fn key_value(v: &Json, attr: &KeyAttr) -> Result<Json> {
    let malformed = || {
        Error::validation(format!(
            "Malformed value object for key column {}: {}",
            attr.name,
            v.to_json_string()
        ))
    };
    let Some((tag, payload)) = single_member(v) else {
        return Err(malformed());
    };
    if tag != attr.ty.name() {
        return Err(Error::validation(format!(
            "Type mismatch: expected type {} for key column {}, got type \"{tag}\"",
            attr.ty.name(),
            attr.name
        )));
    }
    let Some(s) = payload.as_str() else {
        return Err(malformed());
    };
    if s.is_empty() {
        return Err(Error::validation(format!(
            "The AttributeValue for a key attribute cannot contain an empty string value. Key: {}",
            attr.name
        )));
    }
    value::validate(v, "")?;
    Ok(v.clone())
}

/// The primary key held in an item or a `Key` object. `exact` rejects other attributes.
pub(crate) fn primary_key(obj: &Json, schema: &TableSchema, exact: bool) -> Result<PrimaryKey> {
    let find = |a: &KeyAttr| -> Result<Json> {
        match obj.get(&a.name) {
            Some(v) => key_value(v, a),
            None => Err(Error::validation(format!(
                "Key column {} not found",
                a.name
            ))),
        }
    };
    let hash = find(&schema.key.hash)?;
    let range = schema.key.range.as_ref().map(find).transpose()?;
    if exact {
        let n = obj.as_object().map_or(0, |m| m.len());
        let want = 1 + range.is_some() as usize;
        if n > want
            || obj
                .as_object()
                .is_some_and(|m| m.iter().any(|(k, _)| !schema.key.contains(k)))
        {
            return Err(Error::validation("Given key attribute not in schema"));
        }
    }
    Ok(PrimaryKey { hash, range })
}

/// CQL's limit on a key component (65535 bytes including its 2-byte length), which ScyllaDB
/// reports as an internal error when the statement executes.
pub(crate) fn check_key_value_length(v: &Json) -> Result<()> {
    let n = crate::storage::encode_key(v).map_or(0, |b| b.len()) + 2;
    if n > 65535 {
        return Err(Error::internal(format!(
            "Internal server error: exceptions::invalid_request_exception (Key length of {n} is longer than maximum of 65535)"
        )));
    }
    Ok(())
}

pub(crate) fn check_key_length(key: &PrimaryKey) -> Result<()> {
    check_key_value_length(&key.hash)?;
    if let Some(r) = &key.range {
        check_key_value_length(r)?;
    }
    Ok(())
}

/// The key attributes of a primary key, as an item.
pub(crate) fn key_item(schema: &TableSchema, key: &PrimaryKey) -> value::Item {
    let mut item = value::Item::new();
    item.insert(schema.key.hash.name.clone(), key.hash.clone());
    if let (Some(r), Some(v)) = (&schema.key.range, &key.range) {
        item.insert(r.name.clone(), v.clone());
    }
    item
}

/// `ConsistentRead` (a boolean, or null).
pub(crate) fn consistent_read(body: &Json) -> Result<bool> {
    match body.get("ConsistentRead") {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(_) => Err(Error::validation("ConsistentRead flag must be a boolean")),
    }
}

/// `ReturnConsumedCapacity`: whether `TOTAL` was asked for.
pub(crate) fn return_consumed_capacity(body: &Json) -> Result<bool> {
    match body.get("ReturnConsumedCapacity") {
        None => Ok(false),
        Some(Json::String(s)) => match s.as_str() {
            "TOTAL" => Ok(true),
            "INDEXES" => Err(Error::validation(
                "INDEXES consumed capacity is not supported",
            )),
            s => Err(Error::validation(format!("Unknown consumed capacity {s}"))),
        },
        Some(_) => Err(Error::validation(
            "Non-string ReturnConsumedCapacity field in request",
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReturnValues {
    None,
    AllOld,
    UpdatedOld,
    AllNew,
    UpdatedNew,
}

/// `ReturnValues`; `allowed_all` = UpdateItem (otherwise only NONE / ALL_OLD).
pub(crate) fn return_values(body: &Json, op: &str, update: bool) -> Result<ReturnValues> {
    let Some(v) = body.get("ReturnValues") else {
        return Ok(ReturnValues::None);
    };
    let Some(s) = v.as_str() else {
        return Err(Error::validation(format!(
            "Expected string value for ReturnValues, got: {}",
            v.to_json_string()
        )));
    };
    let rv = match s {
        "NONE" => ReturnValues::None,
        "ALL_OLD" => ReturnValues::AllOld,
        "UPDATED_OLD" => ReturnValues::UpdatedOld,
        "ALL_NEW" => ReturnValues::AllNew,
        "UPDATED_NEW" => ReturnValues::UpdatedNew,
        _ => {
            return Err(Error::validation(format!(
                "Unrecognized value for ReturnValues: {s}"
            )));
        }
    };
    if !update && !matches!(rv, ReturnValues::None | ReturnValues::AllOld) {
        return Err(Error::validation(format!(
            "{op} supports only NONE or ALL_OLD for ReturnValues"
        )));
    }
    Ok(rv)
}

/// `ReturnValuesOnConditionCheckFailure`: whether ALL_OLD was asked for.
pub(crate) fn return_values_on_failure(body: &Json) -> Result<bool> {
    let Some(v) = body.get("ReturnValuesOnConditionCheckFailure") else {
        return Ok(false);
    };
    let Some(s) = v.as_str() else {
        return Err(Error::validation(format!(
            "Expected string value for ReturnValuesOnConditionCheckFailure, got: {}",
            v.to_json_string()
        )));
    };
    match s {
        "ALL_OLD" => Ok(true),
        "NONE" => Ok(false),
        _ => Err(Error::validation(format!(
            "Unrecognized value for ReturnValuesOnConditionCheckFailure: {s}"
        ))),
    }
}

/// An item as a JSON object.
pub(crate) fn item_json(item: &value::Item) -> Json {
    Json::Object(item.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `{"CapacityUnits": x}`.
pub(crate) fn capacity(units: f64) -> Json {
    Json::object().with("CapacityUnits", Json::f64(units))
}

/// RapidJSON's type name, as ScyllaDB's internal errors print it.
pub fn rapid_type(v: &Json) -> &'static str {
    match v {
        Json::Null => "kNullType",
        Json::Bool(false) => "kFalseType",
        Json::Bool(true) => "kTrueType",
        Json::Object(_) => "kObjectType",
        Json::Array(_) => "kArrayType",
        Json::String(_) => "kStringType",
        Json::Number(_) => "kNumberType",
    }
}
