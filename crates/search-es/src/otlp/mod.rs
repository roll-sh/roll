//! An OTLP/HTTP intake on the cluster's own endpoint (docs/search/logs-end-to-end.md, section 1):
//! `POST /_otlp/v1/logs` takes an `ExportLogsServiceRequest` in protobuf
//! (`application/x-protobuf`) or JSON, gzip allowed (the node inflates it within its body
//! limits), so an OpenTelemetry SDK or Collector's `otlphttp` exporter can point at a cluster
//! with an API key header.
//!
//! The records become OTel-native documents ([`logs`]) written by one `_bulk` of `create`
//! actions as the client that sent them: each `logs-<dataset>.otel-<namespace>` target is
//! authorized as any write is (`create_doc` and `auto_configure`, which the `otel` key access
//! gives on `logs-*.otel-*` and nothing else). The documents' NDJSON is charged to the request
//! breaker as it's built, and the JSON form's parsed tree is charged before it's parsed.
//!
//! Answers follow OTLP/HTTP: 200 with an empty `ExportLogsServiceResponse`, or with
//! `partial_success` naming how many records were rejected and the first reason, in the request's
//! encoding; a request whose every record was refused for permission answers 403; the write's
//! 429 answers 429 with `Retry-After`, which Collectors back off on.

pub mod logs;
pub mod wire;

use crate::breaker::Reservation;
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::internal;
use bytes::Bytes;

pub const PROTOBUF: &str = "application/x-protobuf";

/// NDJSON charged at a time (so a large request doesn't take the breaker's lock per record).
const CHARGE_STEP: usize = 64 * 1024;
/// What a parsed JSON tree may cost per byte of its text, charged before it's parsed.
const JSON_TREE_FACTOR: u64 = 8;

/// Whether `content_type` names OTLP's protobuf encoding.
pub fn is_protobuf(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|c| c.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case(PROTOBUF)))
}

/// `ExportLogsServiceResponse`, in the request's encoding.
fn answer(proto: bool, rejected: u64, reason: Option<&str>) -> Reply {
    if proto {
        let mut body = Vec::new();
        if rejected > 0 {
            let mut partial = Vec::new();
            put_varint(&mut partial, 1 << 3);
            put_varint(&mut partial, rejected);
            if let Some(r) = reason {
                put_varint(&mut partial, 2 << 3 | 2);
                put_varint(&mut partial, r.len() as u64);
                partial.extend_from_slice(r.as_bytes());
            }
            put_varint(&mut body, 1 << 3 | 2);
            put_varint(&mut body, partial.len() as u64);
            body.extend(partial);
        }
        return Reply::Raw(200, PROTOBUF.into(), Bytes::from(body));
    }
    let v = if rejected > 0 {
        obj! { "partialSuccess" => obj! { "rejectedLogRecords" => rejected.to_string(), "errorMessage" => reason.unwrap_or_default() } }
    } else {
        obj! {}
    };
    Reply::Raw(200, "application/json".into(), Bytes::from(v.to_json()))
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// `POST /_otlp/v1/logs`.
pub async fn logs(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let proto = is_protobuf(ctx.request.header("content-type"));
    let budget = {
        let st = ctx.state();
        ctx.cluster.breakers.request(ctx.cluster, &st)
    };
    let mut held = Reservation::new(budget);
    let mut tripped: Option<EsError> = None;
    let mut charged = 0usize;
    let mut charge = |n: usize| -> Result<(), String> {
        if n < charged + CHARGE_STEP {
            return Ok(());
        }
        match held.add((n - charged) as u64, "<otlp_documents>") {
            Ok(()) => {
                charged = n;
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                tripped = Some(e);
                Err(msg)
            }
        }
    };
    let now = ctx.now_ms;
    let batch = if proto {
        logs::from_protobuf(&ctx.body, now, &mut charge)
    } else {
        let mut tree = Reservation::new(ctx.cluster.breakers.request_budget().clone());
        tree.add(ctx.body.len() as u64 * JSON_TREE_FACTOR, "<otlp_json>")?;
        let parsed = match crate::json::parse(&ctx.body) {
            Ok(Some(v)) => v,
            Ok(None) => Value::Obj(Default::default()),
            Err(e) => return Err(EsError::illegal_argument(format!("the OTLP/JSON body isn't JSON: {e}"))),
        };
        logs::from_json(&parsed, now, &mut charge)
    };
    drop(charge);
    let batch = match (batch, tripped) {
        (_, Some(e)) => return Err(e),
        (Ok(b), None) => b,
        (Err(why), None) => return Err(EsError::illegal_argument(format!("an OTLP logs request that doesn't decode: {why}"))),
    };
    if batch.accepted == 0 {
        return Ok(answer(proto, batch.rejected, batch.reason.as_deref()));
    }
    let caller = internal::Caller::client(ctx);
    let out = internal::as_caller(caller, internal::call_ndjson(ctx.cluster, "POST", "/_bulk", batch.ndjson)).await;
    let v = match out {
        Ok(v) => v,
        Err(e) => {
            if e.status == 429 {
                ctx.add_header("Retry-After", "1");
            }
            return Err(e);
        }
    };
    let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let failed: Vec<&Value> = items.iter().filter_map(|i| i.get("create")).filter(|c| c.get("status").and_then(Value::as_i64).is_some_and(|s| s >= 300)).collect();
    // Every record refused for one reason the client must fix: that refusal (a 403 for a key
    // that may not write there), not a partial success a Collector would drop data on.
    if !failed.is_empty() && failed.len() == items.len() {
        let status = failed[0].get("status").and_then(Value::as_i64).unwrap_or(500);
        if failed.iter().all(|f| f.get("status").and_then(Value::as_i64) == Some(status)) && matches!(status, 401 | 403 | 429) {
            if status == 429 {
                ctx.add_header("Retry-After", "1");
            }
            return Err(internal::error_from_body(status as u16, &obj! { "error" => failed[0].get("error").cloned().unwrap_or(Value::Null) }));
        }
    }
    let reason = batch.reason.or_else(|| {
        failed.first().and_then(|f| f.pointer(&["error", "reason"]).and_then(Value::as_str)).map(str::to_string)
    });
    Ok(answer(proto, batch.rejected + failed.len() as u64, reason.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_otlps_in_the_requests_encoding() {
        let Reply::Raw(200, ct, b) = answer(true, 0, None) else { panic!() };
        assert_eq!((ct.as_str(), b.len()), (PROTOBUF, 0));
        let Reply::Raw(200, _, b) = answer(true, 3, Some("no")) else { panic!() };
        // partial_success { rejected_log_records: 3, error_message: "no" }
        assert_eq!(&b[..], &[0x0a, 0x06, 0x08, 0x03, 0x12, 0x02, b'n', b'o']);
        let Reply::Raw(200, _, b) = answer(false, 2, Some("why")) else { panic!() };
        assert_eq!(&b[..], br#"{"partialSuccess":{"rejectedLogRecords":"2","errorMessage":"why"}}"#);
        assert!(is_protobuf(Some("application/x-protobuf; charset=binary")) && !is_protobuf(Some("application/json")));
    }
}
