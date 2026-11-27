//! An OTLP/HTTP intake on the cluster's own endpoint (docs/search/logs-end-to-end.md, section 1):
//! `POST /_otlp/v1/logs` takes an `ExportLogsServiceRequest` in protobuf
//! (`application/x-protobuf`) or JSON (`application/json`, at most [`MAX_JSON_BODY`]), gzip
//! allowed (the node inflates it within its body limits), so an OpenTelemetry SDK or Collector's
//! `otlphttp` exporter can point at a cluster with an API key header.
//!
//! The records become OTel-native documents ([`logs`]) written by `_bulk`s of `create` actions
//! (at most [`logs::CHUNK_BYTES`] each) as the client that sent them: each
//! `logs-<dataset>.otel-<namespace>` target is authorized as any write is (`create_doc` and
//! `auto_configure`, which the `otel` key access gives on `logs-*.otel-*` and nothing else).
//!
//! Memory: decoding runs off the reactor, charging what it builds to the request breaker as it
//! builds it ([`logs::Budget`]); the JSON form's parsed tree is charged at
//! [`JSON_TREE_FACTOR`] times its text (a parsed value's worst case per byte) before it's parsed,
//! which is why that form has its own, smaller cap. A trip answers 429 with `Retry-After`.
//!
//! Answers follow OTLP/HTTP: 200 with an empty `ExportLogsServiceResponse`, or with
//! `partial_success` naming how many records were rejected and the first reason (or, with none
//! rejected, how many values were cut), in the request's encoding; a request whose every record
//! was refused for the same 401, 403 or 429 answers that status; another content type is a 415.

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
/// The largest OTLP/JSON body taken (protobuf, what Collectors send, goes to the cluster's limit).
pub const MAX_JSON_BODY: usize = 4 * 1024 * 1024;
/// What a parsed JSON tree may cost per byte of its text: an empty object (`{}`, 2 bytes) is a
/// `Value` and its slot, about 100 bytes.
const JSON_TREE_FACTOR: u64 = 80;

/// Whether `content_type` names OTLP's protobuf encoding.
pub fn is_protobuf(content_type: Option<&str>) -> bool {
    mime(content_type).is_some_and(|m| m.eq_ignore_ascii_case(PROTOBUF))
}

fn mime(content_type: Option<&str>) -> Option<&str> {
    content_type.and_then(|c| c.split(';').next()).map(str::trim)
}

/// `ExportLogsServiceResponse`, in the request's encoding.
fn answer(proto: bool, rejected: u64, reason: Option<&str>) -> Reply {
    if proto {
        let mut body = Vec::new();
        if rejected > 0 || reason.is_some() {
            let mut partial = Vec::new();
            if rejected > 0 {
                put_varint(&mut partial, 1 << 3);
                put_varint(&mut partial, rejected);
            }
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
    let v = if rejected > 0 || reason.is_some() {
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

fn busy(ctx: &Ctx<'_>, e: EsError) -> EsError {
    if e.status == 429 {
        ctx.add_header("Retry-After", "1");
    }
    e
}

/// `POST /_otlp/v1/logs`.
pub async fn logs(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let content_type = ctx.request.header("content-type");
    let proto = is_protobuf(content_type);
    if !proto && !mime(content_type).is_some_and(|m| m.eq_ignore_ascii_case("application/json")) {
        return Err(EsError::status(415, "OTLP/HTTP takes application/x-protobuf or application/json"));
    }
    if !proto && ctx.body.len() > MAX_JSON_BODY {
        return Err(EsError::status(413, format!("an OTLP/JSON body of {} bytes: at most {MAX_JSON_BODY} (send protobuf for larger batches)", ctx.body.len())));
    }
    let budget = {
        let st = ctx.state();
        ctx.cluster.breakers.request(ctx.cluster, &st)
    };
    let mut tree = Reservation::new(budget.clone());
    if !proto {
        tree.add(ctx.body.len() as u64 * JSON_TREE_FACTOR, "<otlp_json>").map_err(|e| busy(ctx, e))?;
    }
    let body = ctx.body.clone();
    let now = ctx.now_ms;
    // Decoding builds a request's documents: off the reactor.
    let decoded = tokio::task::spawn_blocking(move || {
        let mut held = logs::Budget::new(Reservation::new(budget));
        let batch = if proto {
            logs::from_protobuf(&body, now, &mut held)
        } else {
            match crate::json::parse(&body) {
                Ok(Some(v)) => logs::from_json(&v, now, &mut held),
                Ok(None) => logs::from_json(&Value::Obj(Default::default()), now, &mut held),
                Err(e) => Err(logs::Fail::Malformed(format!("the OTLP/JSON body isn't JSON: {e}"))),
            }
        };
        (batch, held)
    })
    .await;
    drop(tree);
    let (batch, mut held) = decoded.map_err(|_| EsError::status(500, "decoding the OTLP request stopped on an internal error"))?;
    let mut batch = match batch {
        Ok(b) => b,
        Err(logs::Fail::Tripped(e)) => return Err(busy(ctx, e)),
        Err(logs::Fail::Malformed(why)) => return Err(EsError::illegal_argument(format!("an OTLP logs request that doesn't decode: {why}"))),
    };
    let caller = internal::Caller::client(ctx);
    let (mut items, mut failed) = (0u64, 0u64);
    let mut first: Option<(i64, Value)> = None;
    let mut same_status = true;
    for chunk in std::mem::take(&mut batch.chunks) {
        let len = chunk.len();
        let out = internal::as_caller(caller.clone(), internal::call_ndjson(ctx.cluster, "POST", "/_bulk", chunk)).await;
        held.sent(len);
        let v = out.map_err(|e| busy(ctx, e))?;
        for i in v.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
            items += 1;
            let Some(c) = i.get("create") else { continue };
            let status = c.get("status").and_then(Value::as_i64).unwrap_or(200);
            if status >= 300 {
                failed += 1;
                match &first {
                    None => first = Some((status, c.get("error").cloned().unwrap_or(Value::Null))),
                    Some((s, _)) if *s != status => same_status = false,
                    _ => {}
                }
            }
        }
    }
    // Every record refused for one reason the client must fix: that refusal (a 403 for a key
    // that may not write there), not a partial success a Collector would drop data on.
    if let Some((status, error)) = &first
        && failed == items
        && batch.rejected == 0
        && same_status
        && matches!(status, 401 | 403 | 429)
    {
        return Err(busy(ctx, internal::error_from_body(*status as u16, &obj! { "error" => error.clone() })));
    }
    let reason = batch
        .reason
        .clone()
        .or_else(|| first.as_ref().and_then(|(_, e)| e.get("reason").and_then(Value::as_str)).map(str::to_string))
        .or_else(|| (batch.truncated > 0).then(|| format!("{} values were cut at {} bytes", batch.truncated, logs::MAX_VALUE_BYTES)));
    Ok(answer(proto, batch.rejected + failed, reason.as_deref()))
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
        // A warning with nothing rejected: error_message alone.
        let Reply::Raw(200, _, b) = answer(true, 0, Some("w")) else { panic!() };
        assert_eq!(&b[..], &[0x0a, 0x03, 0x12, 0x01, b'w']);
        let Reply::Raw(200, _, b) = answer(false, 2, Some("why")) else { panic!() };
        assert_eq!(&b[..], br#"{"partialSuccess":{"rejectedLogRecords":"2","errorMessage":"why"}}"#);
        assert!(is_protobuf(Some("application/x-protobuf; charset=binary")) && !is_protobuf(Some("application/json")));
    }
}
