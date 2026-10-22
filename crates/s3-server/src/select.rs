//! SelectObjectContent: `POST /<bucket>/<key>?select&select-type=2`.
//!
//! The request and the SQL are parsed and checked first (`s3-select`), so their errors are
//! ordinary S3 XML errors with S3 Select's codes. Then the response streams: the object is read
//! once (its `ScanRange` only), fed to the query chunk by chunk, and the output goes out as AWS
//! event-stream messages (`s3_proto::eventstream`): `Records` as they're produced, `Progress`
//! when asked for, `Cont` to keep a quiet connection alive, then `Stats` and `End`. A failure
//! once the stream has started is an error message in the stream, as on S3.
//!
//! Memory is bounded: the query's own buffers are capped (`s3-select`), output is flushed in
//! messages of at most [`FLUSH`] bytes, and at most [`QUEUE`] messages wait for a slow client
//! (the object is read no faster than the client takes the results). A client that goes away
//! stops the read.

use crate::ctx::{Ctx, read_small};
use crate::error::StoreResult;
use crate::response::Reply;
use crate::{sse, versioning};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use s3_proto::eventstream;
use s3_proto::xml::escape;
use s3_proto::{Code, S3Error};
use s3_select::{Query, SelectError};
use s3_store::{Bucket, Class};
use std::time::{Duration, Instant};

/// Largest request body (the expression is at most 256 KiB on S3).
const MAX_REQUEST: usize = 1 << 20;
/// Output is sent once this much is waiting.
const FLUSH: usize = 256 << 10;
/// Messages waiting for the client.
const QUEUE: usize = 8;
/// A `Progress` event at most this often (in bytes scanned) …
const PROGRESS_EVERY: u64 = 1 << 20;
/// … and a `Cont` after this long without sending anything.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// Object bytes fed to the query at a time.
const FEED: usize = 64 << 10;

/// An S3 Select error before the stream starts: S3's XML error, with S3 Select's own code.
fn error_response(e: &SelectError) -> Reply {
    if let Some(code) = Code::parse(e.code) {
        return Err(S3Error::msg(code, e.message.clone()));
    }
    let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{}</Code><Message>{}</Message></Error>", escape(e.code), escape(&e.message));
    let mut r = crate::response::xml(crate::response::status(e.status), body);
    crate::response::set(&mut r, "x-amz-select-error", e.code);
    Ok(r)
}

pub(crate) async fn select(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require_object_read(bucket)?;
    if ctx.q("select-type").is_some_and(|t| t != "2") {
        return Err(S3Error::msg(Code::InvalidArgument, "select-type must be 2").with("ArgumentName", "select-type"));
    }
    let raw = read_small(ctx, body, MAX_REQUEST).await?;
    let req = match s3_select::parse_request(&raw) {
        Ok(r) => r,
        Err(e) => return error_response(&e),
    };
    let vid = versioning::version_id(ctx)?;
    let (meta, enc) = ctx.store().get_version_encryption(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    let key = sse::key_for(ctx, enc.as_ref(), sse::Use::Read)?;
    let mut query = match Query::new(&req) {
        Ok(q) => q,
        Err(e) => return error_response(&e),
    };
    ctx.meter(&bucket.id, Class::B);
    // Open the object before answering: a missing blob is an ordinary error.
    let input = match query.input_range(meta.size) {
        Some(r) if r.start < r.end => Some(ctx.store().open_blob_sse(&meta, Some(r), key.as_ref()).await.s3()?.1),
        _ => None,
    };
    let progress = req.progress;
    let store = ctx.store().clone();
    let bucket_id = bucket.id.clone();
    let internal = ctx.internal();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(QUEUE);
    tokio::spawn(async move {
        let send = |m: Vec<u8>| {
            let tx = tx.clone();
            async move { tx.send(Ok(Bytes::from(m))).await.is_ok() }
        };
        let mut sent_at = Instant::now();
        let mut next_progress = PROGRESS_EVERY;
        let mut egress = 0u64;
        let fail = |e: &SelectError| eventstream::error(e.code, &e.message);
        if let Some(mut input) = input {
            while let Some(chunk) = input.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading an object for S3 Select failed");
                        let _ = send(eventstream::error("InternalError", "We encountered an internal error. Please try again.")).await;
                        return;
                    }
                };
                for piece in chunk.chunks(FEED) {
                    let mut input = piece;
                    loop {
                        if let Err(e) = query.feed(input) {
                            let _ = send(fail(&e)).await;
                            return;
                        }
                        input = &[];
                        let backlogged = query.backlogged();
                        // The engine holds input back once its output fills: send, then go on.
                        if query.pending_output() >= FLUSH || (backlogged && query.pending_output() > 0) {
                            let out = query.take_output();
                            egress += out.len() as u64;
                            if !send(eventstream::records(&out)).await {
                                return;
                            }
                            sent_at = Instant::now();
                        }
                        if !backlogged {
                            break;
                        }
                    }
                    if query.done() {
                        break;
                    }
                }
                let st = query.stats();
                if progress && st.bytes_scanned >= next_progress {
                    next_progress = st.bytes_scanned + PROGRESS_EVERY;
                    if !send(eventstream::progress(st.bytes_scanned, st.bytes_processed, st.bytes_returned)).await {
                        return;
                    }
                    sent_at = Instant::now();
                }
                if sent_at.elapsed() >= KEEPALIVE {
                    if !send(eventstream::cont()).await {
                        return;
                    }
                    sent_at = Instant::now();
                }
                if query.done() {
                    break;
                }
            }
        }
        if let Err(e) = query.finish() {
            let _ = send(fail(&e)).await;
            return;
        }
        let out = query.take_output();
        if !out.is_empty() {
            egress += out.len() as u64;
            if !send(eventstream::records(&out)).await {
                return;
            }
        }
        let st = query.stats();
        if progress && !send(eventstream::progress(st.bytes_scanned, st.bytes_processed, st.bytes_returned)).await {
            return;
        }
        if send(eventstream::stats(st.bytes_scanned, st.bytes_processed, st.bytes_returned)).await {
            let _ = send(eventstream::end()).await;
        }
        store.meter.egress(&bucket_id, egress, internal);
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
    let mut r = Response::new(Body::from_stream(stream));
    *r.status_mut() = StatusCode::OK;
    crate::response::set(&mut r, "content-type", "application/octet-stream");
    Ok(r)
}
