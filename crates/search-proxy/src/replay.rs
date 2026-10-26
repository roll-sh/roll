//! Request bodies that can be sent again.
//!
//! A request must be resent when the node it reached provably didn't run it: the node refused
//! the cluster (`X-Roll-Search-Owner`, a handoff) or is shutting down (`node_closed_exception`).
//! Refusals come before the node reads the body, so the body may be anywhere between unsent
//! and fully forwarded when that happens.
//!
//! Store-and-forward would make every body resendable, at the cost of holding the whole
//! request before the node sees its first byte. Instead the body streams straight through and
//! is *teed*: each chunk the client sends is forwarded and kept (a reference-counted [`Bytes`],
//! so keeping it copies nothing). A resend replays the kept chunks and then carries on with the
//! client's stream where the first attempt left off. This works for any body: declared length
//! or chunked, a few bytes or a 100 MB `_bulk`.
//!
//! Kept bytes are bounded twice: per request (`per_request`, like a node's
//! `http.max_content_length`) and across the proxy ([`Budget`]). A body that goes over either
//! stops being kept and still streams on; it just can't be resent any more
//! ([`Replay::replayable`]), and the client gets the refusal and retries itself.
//!
//! One attempt reads at a time. Starting a new attempt supersedes the previous one: if the
//! old connection still polls its body (a node that answered early while the upload went on),
//! it gets an error, so that connection is aborted rather than finishing a truncated upload.
//! Trailers are forwarded but not kept (no Elasticsearch API uses request trailers).

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Frame, SizeHint};
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// What the proxy sends to nodes.
pub type SendBody = BoxBody<Bytes, BoxError>;

/// Bytes kept for resends across all requests of a proxy.
#[derive(Debug)]
pub struct Budget {
    used: AtomicUsize,
    limit: usize,
}

impl Budget {
    pub fn new(limit: usize) -> Arc<Budget> {
        Arc::new(Budget { used: AtomicUsize::new(0), limit })
    }

    /// Bytes kept right now.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    fn take(&self, n: usize) -> bool {
        self.used.try_update(Ordering::AcqRel, Ordering::Acquire, |u| u.checked_add(n).filter(|t| *t <= self.limit)).is_ok()
    }

    fn give(&self, n: usize) {
        if n > 0 {
            self.used.fetch_sub(n, Ordering::AcqRel);
        }
    }
}

struct State<B> {
    /// The client's body; `None` once it ended (or failed).
    source: Option<B>,
    /// Data chunks kept, in order; chunk `i` of the body is `chunks[i - dropped]`.
    chunks: Vec<Bytes>,
    /// Chunks read but not kept (all of them once `overflow`).
    dropped: usize,
    kept: usize,
    overflow: bool,
    /// How the client's body ended: cleanly, or with this error.
    end: Option<Result<(), String>>,
    /// The attempt allowed to read.
    current: u64,
    per_request: usize,
    budget: Arc<Budget>,
}

impl<B> State<B> {
    fn keep(&mut self, data: &Bytes) {
        if !self.overflow {
            let n = data.len();
            if self.kept + n <= self.per_request && self.budget.take(n) {
                self.kept += n;
                self.chunks.push(data.clone());
                return;
            }
            self.overflow = true;
            self.budget.give(std::mem::take(&mut self.kept));
            self.dropped += self.chunks.len();
            self.chunks = Vec::new();
        }
        self.dropped += 1;
    }
}

impl<B> Drop for State<B> {
    fn drop(&mut self) {
        self.budget.give(self.kept);
    }
}

/// A client's request body, sendable more than once (see the module docs).
pub struct Replay<B> {
    state: Arc<Mutex<State<B>>>,
    declared: Option<u64>,
}

impl<B> Replay<B>
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::fmt::Display,
{
    /// `declared`: the request's `Content-Length`, if any.
    pub fn new(body: B, declared: Option<u64>, per_request: usize, budget: Arc<Budget>) -> Replay<B> {
        // A body declared too large is never kept: no point filling the budget first.
        let overflow = declared.is_some_and(|n| n > per_request as u64);
        let state = State { source: Some(body), chunks: Vec::new(), dropped: 0, kept: 0, overflow, end: None, current: 0, per_request, budget };
        Replay { state: Arc::new(Mutex::new(state)), declared }
    }

    /// Whether every byte read so far is kept, so a new attempt can send the whole body.
    pub fn replayable(&self) -> bool {
        // Chunks are only dropped once over a bound.
        self.state.lock().dropped == 0
    }

    /// Whether the *client's* stream failed (it disconnected or timed out mid-upload). An
    /// attempt that broke then broke on the client's side, not the node's: the node must not be
    /// blamed for it, and there is nothing whole left to resend.
    pub fn client_failed(&self) -> bool {
        matches!(self.state.lock().end, Some(Err(_)))
    }

    /// The body for the next attempt; earlier attempts' bodies fail from now on. `None` when
    /// the body can't be sent whole again.
    pub fn attempt(&self) -> Option<SendBody> {
        let mut s = self.state.lock();
        if s.dropped > 0 {
            return None;
        }
        s.current += 1;
        let reader = Reader { state: self.state.clone(), attempt: s.current, next: 0, sent: 0, declared: self.declared };
        Some(reader.boxed())
    }
}

struct Reader<B> {
    state: Arc<Mutex<State<B>>>,
    attempt: u64,
    /// Index of the next data chunk.
    next: usize,
    sent: u64,
    declared: Option<u64>,
}

impl<B> Body for Reader<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let mut s = this.state.lock();
        if s.current != this.attempt {
            return Poll::Ready(Some(Err("the request was resent to another node".into())));
        }
        if this.next < s.dropped {
            // Unreachable: chunks are only dropped at the tip of the attempt reading.
            return Poll::Ready(Some(Err("the request body is no longer available".into())));
        }
        if let Some(c) = s.chunks.get(this.next - s.dropped).cloned() {
            this.next += 1;
            this.sent += c.len() as u64;
            return Poll::Ready(Some(Ok(Frame::data(c))));
        }
        match &s.end {
            Some(Ok(())) => return Poll::Ready(None),
            Some(Err(e)) => return Poll::Ready(Some(Err(e.clone().into()))),
            None => {}
        }
        let Some(src) = s.source.as_mut() else { return Poll::Ready(None) };
        match Pin::new(src).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                s.source = None;
                s.end = Some(Ok(()));
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                let msg = format!("reading the request body failed: {e}");
                s.source = None;
                s.end = Some(Err(msg.clone()));
                Poll::Ready(Some(Err(msg.into())))
            }
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) if data.is_empty() => Poll::Ready(Some(Ok(Frame::data(data)))),
                Ok(data) => {
                    s.keep(&data);
                    this.next += 1;
                    this.sent += data.len() as u64;
                    Poll::Ready(Some(Ok(Frame::data(data))))
                }
                Err(trailers) => Poll::Ready(Some(Ok(trailers))),
            },
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.declared {
            Some(n) => SizeHint::with_exact(n.saturating_sub(self.sent)),
            None => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::StreamBody;
    use std::convert::Infallible;

    type Chunks = StreamBody<futures_util::stream::Iter<std::vec::IntoIter<Result<Frame<Bytes>, Infallible>>>>;

    fn chunks(parts: &[&'static str]) -> Chunks {
        let v: Vec<Result<Frame<Bytes>, Infallible>> = parts.iter().map(|p| Ok(Frame::data(Bytes::from_static(p.as_bytes())))).collect();
        StreamBody::new(futures_util::stream::iter(v))
    }

    async fn read(b: SendBody) -> Result<Bytes, String> {
        b.collect().await.map(|c| c.to_bytes()).map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn resends_replay_what_was_read_then_continue() {
        let budget = Budget::new(1 << 20);
        let r = Replay::new(chunks(&["ab", "cd", "ef"]), Some(6), 1 << 20, budget.clone());
        // The first attempt reads one chunk, then the node refuses.
        let mut first = r.attempt().unwrap();
        assert_eq!(first.frame().await.unwrap().unwrap().into_data().unwrap(), "ab");
        assert_eq!(first.size_hint().exact(), Some(4));
        assert!(r.replayable());
        let second = r.attempt().unwrap();
        // The superseded attempt fails instead of sending on.
        assert!(first.frame().await.unwrap().is_err());
        assert!(!r.client_failed(), "a superseded attempt is not the client's failure");
        assert_eq!(read(second).await.unwrap(), "abcdef");
        assert_eq!(budget.used(), 6);
        // And again, entirely from what was kept.
        assert_eq!(read(r.attempt().unwrap()).await.unwrap(), "abcdef");
        drop((r, first));
        assert_eq!(budget.used(), 0, "kept bytes go back to the budget once every attempt is gone");
    }

    #[tokio::test]
    async fn bodies_over_a_bound_stream_but_cannot_be_resent() {
        // Per request: declared too large, nothing is kept.
        let budget = Budget::new(1 << 20);
        let r = Replay::new(chunks(&["abc", "def"]), Some(6), 4, budget.clone());
        assert!(r.replayable(), "nothing sent yet");
        assert_eq!(read(r.attempt().unwrap()).await.unwrap(), "abcdef");
        assert!(!r.replayable());
        assert!(r.attempt().is_none());
        assert_eq!(budget.used(), 0);
        // Undeclared length: kept until the bound, then dropped (and the budget given back).
        let r = Replay::new(chunks(&["abc", "def", "g"]), None, 4, budget.clone());
        let mut a = r.attempt().unwrap();
        assert_eq!(a.frame().await.unwrap().unwrap().into_data().unwrap(), "abc");
        assert_eq!(budget.used(), 3);
        assert_eq!(a.size_hint().exact(), None);
        assert_eq!(a.frame().await.unwrap().unwrap().into_data().unwrap(), "def");
        assert_eq!(budget.used(), 0);
        assert!(!r.replayable());
        assert_eq!(read(a).await.unwrap(), "g", "the attempt in progress carries on");
        assert!(r.attempt().is_none());
        // Across requests: the shared budget runs out.
        let small = Budget::new(5);
        let one = Replay::new(chunks(&["abcd"]), Some(4), 100, small.clone());
        let two = Replay::new(chunks(&["efgh"]), Some(4), 100, small.clone());
        assert_eq!(read(one.attempt().unwrap()).await.unwrap(), "abcd");
        assert_eq!(read(two.attempt().unwrap()).await.unwrap(), "efgh");
        assert!(one.replayable() && !two.replayable());
        drop(one);
        assert_eq!(small.used(), 0);
    }

    #[tokio::test]
    async fn client_errors_reach_every_attempt() {
        let v: Vec<Result<Frame<Bytes>, String>> = vec![Ok(Frame::data(Bytes::from_static(b"ab"))), Err("connection reset".into())];
        let r = Replay::new(StreamBody::new(futures_util::stream::iter(v)), None, 100, Budget::new(100));
        assert!(!r.client_failed());
        assert!(read(r.attempt().unwrap()).await.unwrap_err().contains("connection reset"));
        assert!(r.client_failed(), "the client's own error is told apart");
        let e = read(r.attempt().unwrap()).await.unwrap_err();
        assert!(e.contains("reading the request body failed: connection reset"), "{e}");
    }

    #[tokio::test]
    async fn empty_frames_and_trailers_pass_through() {
        let mut trailers = hyper::HeaderMap::new();
        trailers.insert("x-t", hyper::header::HeaderValue::from_static("1"));
        let v: Vec<Result<Frame<Bytes>, Infallible>> = vec![Ok(Frame::data(Bytes::new())), Ok(Frame::data(Bytes::from_static(b"x"))), Ok(Frame::trailers(trailers))];
        let r = Replay::new(StreamBody::new(futures_util::stream::iter(v)), None, 100, Budget::new(100));
        let c = r.attempt().unwrap().collect().await.unwrap();
        assert!(c.trailers().is_some());
        assert_eq!(c.to_bytes(), "x");
        assert_eq!(read(r.attempt().unwrap()).await.unwrap(), "x", "trailers aren't kept");
    }
}
