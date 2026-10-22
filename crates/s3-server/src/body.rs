//! Request bodies: decoded (`aws-chunked`), hashed and verified while they stream to object
//! storage.
//!
//! Nothing may be committed until the whole body checked out (chunk signatures, length,
//! `Content-MD5`, the payload SHA-256, `x-amz-checksum-*`). So [`Incoming`] holds back the
//! body's last bytes until the client's stream has ended and every check passed: if one fails,
//! the stream ends in an error instead, and the backend stores nothing. The handler checks the
//! outcome again before committing.

use crate::ctx::Ctx;
use axum::body::{Body, BodyDataStream};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use s3_proto::checksum::{Algorithm, BodyDigest, BodyHasher, requested};
use s3_proto::chunked::{Decoder, Mode};
use s3_proto::sigv4::Payload;
use s3_proto::{Code, S3Error};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// A body that checked out.
#[derive(Clone, Debug)]
pub(crate) struct Finished {
    pub digest: BodyDigest,
    /// The checksum (requested or default), base64.
    pub checksum: Option<(Algorithm, String)>,
}

type Outcome = Arc<Mutex<Option<Result<Finished, S3Error>>>>;

/// Where a handler finds the outcome after the body went to the backend.
#[derive(Clone)]
pub(crate) struct BodyResult(Outcome);

impl BodyResult {
    /// The verified body, or why it failed (`IncompleteBody` if it never finished).
    pub fn get(&self) -> Result<Finished, S3Error> {
        self.0.lock().unwrap().clone().unwrap_or_else(|| Err(S3Error::new(Code::IncompleteBody)))
    }
}

pub(crate) struct Incoming {
    inner: BodyDataStream,
    decoder: Option<Decoder>,
    hasher: Option<BodyHasher>,
    queue: VecDeque<Bytes>,
    held: Option<Bytes>,
    /// Decoded length the client declared, if known.
    len: Option<u64>,
    seen: u64,
    max: Option<u64>,
    /// `(algorithm, base64 value if sent in a header)`.
    checksum: Option<(Algorithm, Option<String>)>,
    content_md5: Option<String>,
    sha256: Option<String>,
    outcome: Outcome,
    done: bool,
}

fn header_u64(ctx: &Ctx, name: &str) -> Result<Option<u64>, S3Error> {
    match ctx.h(name) {
        None => Ok(None),
        Some(v) => v.trim().parse().map(Some).map_err(|_| S3Error::msg(Code::InvalidArgument, format!("{name} must be a number")).with("ArgumentName", name.to_string())),
    }
}

/// The checksum a write asks for (header, trailer or SDK hint).
pub(crate) fn requested_checksum(ctx: &Ctx) -> Result<Option<(Algorithm, Option<String>)>, S3Error> {
    requested(|n| ctx.h(n))
}

impl Incoming {
    /// `checksum`: what to compute and (if a value is given or arrives in a trailer) verify.
    /// `max`: refuse bodies longer than this.
    pub fn new(ctx: &Ctx, body: Body, checksum: Option<(Algorithm, Option<String>)>, max: Option<u64>) -> Result<Self, S3Error> {
        let payload = ctx.auth.as_ref().map_or(Payload::Unsigned, |a| a.verified.payload.clone());
        let (decoder, len) = if payload.is_chunked() {
            let len = header_u64(ctx, "x-amz-decoded-content-length")?.ok_or_else(|| S3Error::new(Code::MissingContentLength))?;
            let mode = match (&payload, ctx.auth.as_ref().and_then(|a| a.verified.stream.clone())) {
                (Payload::Streaming { trailer }, Some(signing)) => Mode::Signed { signing, trailer: *trailer },
                (Payload::StreamingUnsignedTrailer, _) => Mode::UnsignedTrailer,
                _ => return Err(S3Error::msg(Code::InvalidRequest, "streaming payload without a signature")),
            };
            (Some(Decoder::new(mode, Some(len))), Some(len))
        } else {
            (None, header_u64(ctx, "content-length")?)
        };
        if let (Some(m), Some(l)) = (max, len)
            && l > m
        {
            return Err(S3Error::new(Code::MaxMessageLengthExceeded));
        }
        let sha256 = match payload {
            Payload::Sha256(h) => Some(h),
            _ => None,
        };
        Ok(Self {
            inner: body.into_data_stream(),
            decoder,
            hasher: Some(BodyHasher::new(checksum.as_ref().map(|c| c.0), sha256.is_some())),
            queue: VecDeque::new(),
            held: None,
            len,
            seen: 0,
            max,
            checksum,
            content_md5: ctx.h("content-md5").map(str::to_string),
            sha256,
            outcome: Arc::new(Mutex::new(None)),
            done: false,
        })
    }

    pub fn len(&self) -> Option<u64> {
        self.len
    }

    pub fn result(&self) -> BodyResult {
        BodyResult(self.outcome.clone())
    }

    /// The outcome, once the stream ended.
    pub fn finished(&self) -> Result<Finished, S3Error> {
        self.result().get()
    }

    /// Why the stream failed, if it did.
    pub fn error(&self) -> Option<S3Error> {
        self.outcome.lock().unwrap().as_ref().and_then(|r| r.as_ref().err().cloned())
    }

    /// As a boxed stream for the backend.
    pub fn into_stream(self) -> s3_store::ByteStream {
        StreamExt::boxed(self)
    }

    fn fail(&mut self, e: S3Error) -> Poll<Option<std::io::Result<Bytes>>> {
        self.done = true;
        let msg = e.message.clone();
        *self.outcome.lock().unwrap() = Some(Err(e));
        Poll::Ready(Some(Err(std::io::Error::other(msg))))
    }

    /// Account for decoded bytes: hash them, and queue (or hold back the last of) them.
    fn accept(&mut self, pieces: Vec<Bytes>) -> Result<(), S3Error> {
        for p in pieces {
            if p.is_empty() {
                continue;
            }
            self.seen += p.len() as u64;
            if let Some(m) = self.max
                && self.seen > m
            {
                return Err(S3Error::new(Code::MaxMessageLengthExceeded));
            }
            if let Some(l) = self.len
                && self.seen > l
            {
                return Err(S3Error::msg(Code::IncompleteBody, "The request body is longer than its declared length."));
            }
            if let Some(h) = &mut self.hasher {
                h.update(&p);
            }
            if let Some(prev) = self.held.take() {
                self.queue.push_back(prev);
            }
            if self.len.is_some_and(|l| self.seen == l) {
                self.held = Some(p);
            } else {
                self.queue.push_back(p);
            }
        }
        Ok(())
    }

    /// The client's stream ended: run every check.
    fn verify(&mut self) -> Result<Finished, S3Error> {
        let trailers = match self.decoder.take() {
            Some(d) => d.finish()?,
            None => Vec::new(),
        };
        if let Some(l) = self.len
            && self.seen != l
        {
            return Err(S3Error::new(Code::IncompleteBody));
        }
        let digest = self.hasher.take().expect("verified once").finish();
        digest.verify_content_md5(self.content_md5.as_deref())?;
        if let Some(h) = &self.sha256 {
            digest.verify_sha256(h)?;
        }
        let mut checksum = None;
        if let Some((a, value)) = &self.checksum {
            let sent = value.clone().or_else(|| trailers.iter().find(|(k, _)| k.eq_ignore_ascii_case(a.header())).map(|(_, v)| v.trim().to_string()));
            if let Some(v) = &sent {
                digest.verify_checksum(*a, v)?;
            }
            checksum = digest.checksum_b64();
        }
        Ok(Finished { digest, checksum })
    }
}

impl Stream for Incoming {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(b) = this.queue.pop_front() {
                return Poll::Ready(Some(Ok(b)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(e))) => {
                    tracing::debug!(error = %e, "request body ended early");
                    return this.fail(S3Error::new(Code::IncompleteBody));
                }
                Poll::Ready(Some(Ok(raw))) => {
                    let mut pieces = Vec::new();
                    let r = match &mut this.decoder {
                        Some(d) => d.feed(raw, &mut pieces),
                        None => {
                            pieces.push(raw);
                            Ok(())
                        }
                    };
                    if let Err(e) = r.and_then(|_| this.accept(pieces)) {
                        return this.fail(e);
                    }
                }
                Poll::Ready(None) => match this.verify() {
                    Ok(f) => {
                        *this.outcome.lock().unwrap() = Some(Ok(f));
                        this.done = true;
                        if let Some(h) = this.held.take() {
                            return Poll::Ready(Some(Ok(h)));
                        }
                        return Poll::Ready(None);
                    }
                    Err(e) => return this.fail(e),
                },
            }
        }
    }
}
