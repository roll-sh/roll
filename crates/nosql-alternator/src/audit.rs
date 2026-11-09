//! ScyllaDB's audit logging of Alternator requests (`docs/operating-scylla/security/auditing.rst`,
//! "Auditing Alternator Requests"): every operation has a category (DDL: CreateTable,
//! DeleteTable, UpdateTable, TagResource, UntagResource, UpdateTimeToLive; DML: PutItem,
//! UpdateItem, DeleteItem, BatchWriteItem; QUERY: the reads, the describes and the Streams
//! API), and a request the filters select ([`crate::Backend::audit_wants`]) is written as one
//! `audit.audit_log` row once it is answered, failed or not.
//!
//! - `operation` is `<Op>|<request JSON>`.
//! - `keyspace_name` / `table_name` are `alternator_T` / `T` for an operation on table `T`
//!   (looked up before the operation runs: a request naming a missing table has no audit
//!   context, as in ScyllaDB, except CreateTable); empty for ListTables, DescribeEndpoints and
//!   ListStreams without a table (logged whenever their category is). DescribeStream,
//!   GetShardIterator and GetRecords name `T|T_scylla_cdc_log`.
//! - Batches (BatchWriteItem, BatchGetItem) leave the keyspace empty, list the audited tables
//!   in `table_name` (`|`-separated), and drop the other tables from the logged request; a
//!   batch with no audited table logs nothing.
//! - `consistency`: `LOCAL_QUORUM` for writes and strongly consistent reads, `LOCAL_ONE` for
//!   eventually consistent ones, `ANY` for BatchGetItem (items choose their own), `ONE` for
//!   DescribeStream (one node), `LOCAL_QUORUM` for GetRecords, empty for the rest.

use crate::backend::{AuditCategory as Category, AuditEvent};
use crate::error::Result;
use crate::json::Json;
use crate::ops::INTERNAL_PREFIX;
use crate::service::{Alternator, Request};

/// The category of an operation.
pub fn category(op: &str) -> Option<Category> {
    Some(match op {
        "CreateTable" | "DeleteTable" | "UpdateTable" | "TagResource" | "UntagResource"
        | "UpdateTimeToLive" => Category::Ddl,
        "PutItem" | "UpdateItem" | "DeleteItem" | "BatchWriteItem" => Category::Dml,
        "GetItem"
        | "BatchGetItem"
        | "Query"
        | "Scan"
        | "DescribeTable"
        | "ListTables"
        | "DescribeEndpoints"
        | "ListTagsOfResource"
        | "DescribeTimeToLive"
        | "DescribeContinuousBackups"
        | "ListStreams"
        | "DescribeStream"
        | "GetShardIterator"
        | "GetRecords" => Category::Query,
        _ => return None,
    })
}

/// What a request is audited as, decided before it runs.
pub(crate) struct Plan {
    category: Category,
    consistency: &'static str,
    keyspace: String,
    table: String,
    operation: String,
}

fn consistent_read(body: &Json) -> &'static str {
    if body.get("ConsistentRead").and_then(Json::as_bool) == Some(true) {
        "LOCAL_QUORUM"
    } else {
        "LOCAL_ONE"
    }
}

fn keyspace(table: &str) -> String {
    format!("alternator_{table}")
}

/// The table an operation names (`TableName`, or the table of a `ResourceArn`).
fn named_table(body: &Json) -> Option<String> {
    let name = body
        .get("TableName")
        .and_then(Json::as_str)
        .map(str::to_string)
        .or_else(|| {
            let arn = body.get("ResourceArn").and_then(Json::as_str)?;
            Some(arn.rsplit_once("table/")?.1.to_string())
        })?;
    (!name.is_empty() && !name.starts_with(INTERNAL_PREFIX)).then_some(name)
}

/// The table of a stream ARN (`…table/alternator_T@T_scylla_cdc_log/stream/…`).
fn stream_table(body: &Json) -> Option<String> {
    let arn = body.get("StreamArn").and_then(Json::as_str)?;
    let t = arn
        .split_once("table/")?
        .1
        .split_once("/stream/")?
        .0
        .split_once('@')?
        .1;
    Some(t.strip_suffix("_scylla_cdc_log")?.to_string())
}

/// The table of a shard iterator (`I<table id>:<position>:<shard>`).
async fn iterator_table(alt: &Alternator, body: &Json) -> Result<Option<String>> {
    let Some(id) = body
        .get("ShardIterator")
        .and_then(Json::as_str)
        .and_then(|s| s.get(1..37))
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
    else {
        return Ok(None);
    };
    for n in alt.backend.list_tables().await? {
        if let Some(t) = alt.backend.table(&n).await?
            && t.id == id
        {
            return Ok(Some(t.name.clone()));
        }
    }
    Ok(None)
}

/// Decides how `req` is audited (`None`: it isn't).
pub(crate) async fn plan(alt: &Alternator, req: &Request<'_>) -> Result<Option<Plan>> {
    let Some(category) = category(req.op) else {
        return Ok(None);
    };
    let backend = &alt.backend;
    if !backend.audit_may_log(category) {
        return Ok(None);
    }
    let body = req.body;
    let wants = |ks: &str, t: &str| backend.audit_wants(category, ks, t, req.user);
    let operation = |body: &Json| format!("{}|{}", req.op, body.to_json_string());
    let single = |consistency: &'static str, table: String| Plan {
        category,
        consistency,
        keyspace: keyspace(&table),
        table,
        operation: operation(body),
    };
    let global = || Plan {
        category,
        consistency: "",
        keyspace: String::new(),
        table: String::new(),
        operation: operation(body),
    };
    let plan = match req.op {
        "ListTables" | "DescribeEndpoints" => wants("", "").then(global),
        "ListStreams" => match named_table(body) {
            None => wants("", "").then(global),
            Some(t) => existing(alt, &t, wants).await?.then(|| single("", t)),
        },
        "DescribeStream" | "GetShardIterator" | "GetRecords" => {
            let t = match req.op {
                "GetRecords" => iterator_table(alt, body).await?,
                _ => stream_table(body),
            };
            let Some(t) = t else {
                return Ok(None);
            };
            let consistency = match req.op {
                "DescribeStream" => "ONE",
                "GetRecords" => "LOCAL_QUORUM",
                _ => "",
            };
            existing(alt, &t, wants).await?.then(|| Plan {
                category,
                consistency,
                keyspace: keyspace(&t),
                table: format!("{t}|{t}_scylla_cdc_log"),
                operation: operation(body),
            })
        }
        "BatchWriteItem" | "BatchGetItem" => {
            let Some(items) = body.get("RequestItems").and_then(Json::as_object) else {
                return Ok(None);
            };
            let audited: Vec<&(String, Json)> = items
                .iter()
                .filter(|(t, _)| wants(&keyspace(t), t))
                .collect();
            if audited.is_empty() {
                return Ok(None);
            }
            // The logged request holds the audited tables only.
            let mut logged = Json::object();
            for (k, v) in body.as_object().unwrap_or_default() {
                if k == "RequestItems" {
                    let mut ri = Json::object();
                    for (t, spec) in &audited {
                        ri.push(t.clone(), spec.clone());
                    }
                    logged.push(k.clone(), ri);
                } else {
                    logged.push(k.clone(), v.clone());
                }
            }
            let tables: Vec<&str> = audited.iter().map(|(t, _)| t.as_str()).collect();
            Some(Plan {
                category,
                consistency: if req.op == "BatchGetItem" {
                    "ANY"
                } else {
                    "LOCAL_QUORUM"
                },
                keyspace: String::new(),
                table: tables.join("|"),
                operation: operation(&logged),
            })
        }
        op => {
            let Some(t) = named_table(body) else {
                return Ok(None);
            };
            let consistency = match op {
                "PutItem" | "UpdateItem" | "DeleteItem" => "LOCAL_QUORUM",
                "GetItem" | "Query" | "Scan" => consistent_read(body),
                _ => "",
            };
            let found = if op == "CreateTable" {
                wants(&keyspace(&t), &t)
            } else {
                existing(alt, &t, wants).await?
            };
            found.then(|| single(consistency, t))
        }
    };
    Ok(plan)
}

/// Whether table `t` is audited and exists.
async fn existing(alt: &Alternator, t: &str, wants: impl Fn(&str, &str) -> bool) -> Result<bool> {
    Ok(wants(&keyspace(t), t) && alt.backend.table(t).await?.is_some())
}

/// Writes the row of a request audited by `plan`, which ended with `failed`.
pub(crate) async fn log(alt: &Alternator, req: &Request<'_>, plan: Plan, failed: bool) {
    alt.backend
        .audit(AuditEvent {
            category: plan.category,
            consistency: plan.consistency,
            keyspace: plan.keyspace,
            table: plan.table,
            operation: plan.operation,
            error: failed,
            user: req.user.map(str::to_string),
            source: req.peer.map(|p| p.ip()),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_and_names() {
        assert_eq!(category("PutItem"), Some(Category::Dml));
        assert_eq!(category("BatchWriteItem"), Some(Category::Dml));
        assert_eq!(category("UpdateTimeToLive"), Some(Category::Ddl));
        assert_eq!(category("GetRecords"), Some(Category::Query));
        assert_eq!(category("DescribeEndpoints"), Some(Category::Query));
        assert_eq!(category("TransactWriteItems"), None);
        let arn = Json::object().with(
            "StreamArn",
            Json::str("arn:aws:dynamodb:us-east-1:000000000000:table/alternator_T@T_scylla_cdc_log/stream/2026-01-01T00:00:00.000000000"),
        );
        assert_eq!(stream_table(&arn).as_deref(), Some("T"));
        let tagged = Json::object().with(
            "ResourceArn",
            Json::str("arn:aws:dynamodb:us-east-1:000000000000:table/T"),
        );
        assert_eq!(named_table(&tagged).as_deref(), Some("T"));
        let internal =
            Json::object().with("TableName", Json::str(".scylla.alternator.system.config"));
        assert_eq!(named_table(&internal), None);
    }
}
