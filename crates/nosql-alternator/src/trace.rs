//! Tracing of Alternator requests (ScyllaDB's `docs/dev/tracing.md`, "Alternator specific"):
//! a request picked by the trace probability, or seen by the slow query log, is a
//! `system_traces` session named `Alternator <Op>` whose parameters are `alternator_op`, the
//! request (`query`, cut at `alternator_max_users_query_size_in_trace_output` bytes and marked
//! `<truncated>`) and its tables (`table`, or `table[0]`, `table[1]`, … sorted when there are
//! several). Its events are the operation's name, then the storage work it did: an LWT's CAS
//! round, a plain write's mutations, or a read's query.

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::{RequestTrace, TraceStart};
use crate::error::Result;
use crate::json::Json;
use crate::ops::INTERNAL_PREFIX;
use crate::service::{Alternator, Request};

/// The tables a request uses.
fn tables(req: &Request<'_>) -> BTreeSet<String> {
    let body = req.body;
    if let Some(items) = body.get("RequestItems").and_then(Json::as_object) {
        return items.iter().map(|(t, _)| t.clone()).collect();
    }
    body.get("TableName")
        .and_then(Json::as_str)
        .map(str::to_string)
        .or_else(|| {
            let arn = body.get("ResourceArn").and_then(Json::as_str)?;
            Some(arn.rsplit_once("table/")?.1.to_string())
        })
        .into_iter()
        .collect()
}

/// `text` cut at `max` bytes (on a character boundary), marked `<truncated>`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}<truncated>", &text[..end])
}

/// Starts tracing `req` when tracing picks it.
pub(crate) fn begin(alt: &Alternator, req: &Request<'_>) -> Option<Box<dyn RequestTrace>> {
    let tables = tables(req);
    let mut parameters = BTreeMap::new();
    parameters.insert("alternator_op".to_string(), req.op.to_string());
    let body = req.body.to_json_string();
    let max = alt.config().max_users_query_size_in_trace_output;
    parameters.insert("query".to_string(), truncate(&body, max));
    match tables.len() {
        0 => {}
        1 => {
            parameters.insert("table".into(), tables.iter().next()?.clone());
        }
        _ => {
            for (i, t) in tables.iter().enumerate() {
                parameters.insert(format!("table[{i}]"), t.clone());
            }
        }
    }
    let start = TraceStart {
        op: req.op.to_string(),
        parameters,
        tables: tables
            .into_iter()
            .filter(|t| !t.starts_with(INTERNAL_PREFIX))
            .collect(),
        user: req.user.map(str::to_string),
        client: req.peer.map(|p| p.ip()),
        request_size: body.len(),
    };
    let mut t = alt.backend.trace(start)?;
    t.event(req.op);
    Some(t)
}

/// Ends the trace of `req`, answered with `r`.
pub(crate) async fn end(mut t: Box<dyn RequestTrace>, req: &Request<'_>, r: &Result<Json>) {
    let ok = r.is_ok();
    let events: &[&str] = match req.op {
        "PutItem" | "UpdateItem" | "DeleteItem" | "BatchWriteItem" if req.stats.used_lwt() => {
            if ok {
                &[
                    "Reading existing values for CAS precondition",
                    "CAS precondition is met; proposing client-requested updates",
                    "CAS successful",
                ]
            } else {
                &[
                    "Reading existing values for CAS precondition",
                    "CAS precondition does not match current values",
                    "CAS failed",
                ]
            }
        }
        "PutItem" | "UpdateItem" | "DeleteItem" | "BatchWriteItem" => &[
            "Creating write handler",
            "Sending a mutation to the replicas",
            "Mutation successfully completed",
        ],
        "GetItem" | "BatchGetItem" | "Query" => &[
            "Creating read executor",
            "read_data: querying locally",
            "Querying is done",
        ],
        "Scan" => &[
            "Performing a database query",
            "read_data: querying locally",
            "Querying is done",
        ],
        _ => &[],
    };
    for e in events {
        t.event(e);
    }
    t.event("Done processing - preparing a result");
    let size = match r {
        Ok(j) => j.to_json_string().len(),
        Err(_) => 0,
    };
    t.end(size).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 3), "abc<truncated>");
        // Never inside a character.
        assert_eq!(truncate("aé", 2), "a<truncated>");
    }
}
