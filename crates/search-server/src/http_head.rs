//! A connection's first request head, checked the way Elasticsearch's Netty decoder checks it,
//! before hyper parses it.
//!
//! hyper closes a connection whose request head it can't parse without a word; Netty answers a
//! 400 naming what it rejected (`text is empty (possibly HTTP/0.9)`, `invalid version format`,
//! `Unexpected http protocol version`, a header name with a non-token character, several
//! `Content-Length` fields, `Transfer-Encoding` on HTTP/1.0 or next to `Content-Length`, a bare
//! line feed), and closes silently on a `Content-Length` that isn't a number. [`FirstHead`]
//! wraps the connection's IO: it holds the bytes of the first head back from hyper until the
//! head is complete, checks it, and then either hands everything over unchanged or writes
//! Elasticsearch's answer itself and shows hyper an empty connection. Later heads on a kept-alive
//! connection are hyper's alone (the body boundaries are hyper's to know), so only the first
//! one is checked; a client sending a malformed request opens a connection for it in practice.
//! Elasticsearch keeps a few of these connections open after answering (it discards what
//! follows); roll closes them.
//!
//! Cost: one copy of the first head (a few hundred bytes) per connection, nothing after.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Heads longer than this are hyper's to refuse (431).
const MAX_HEAD: usize = 64 * 1024;

enum State {
    /// Collecting the first head.
    Reading(Vec<u8>),
    /// Handing over what was held back (from `.1`).
    Passing(Vec<u8>, usize),
    /// Writing Elasticsearch's answer (from `.1`); then the connection reads as closed.
    Rejecting(Vec<u8>, usize),
    Closed,
    Done,
}

/// The IO of a connection whose first request head is checked before hyper sees it.
pub struct FirstHead<S> {
    inner: S,
    state: State,
}

impl<S> FirstHead<S> {
    pub fn new(inner: S) -> FirstHead<S> {
        FirstHead { inner, state: State::Reading(Vec::new()) }
    }
}

/// What a complete (or failed) head calls for.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// Answer with this response and close.
    Answer(Vec<u8>),
    /// Close without an answer.
    Drop,
}

/// Where the head ends, or a line feed without its carriage return (which Netty refuses as
/// soon as it sees it).
enum HeadEnd {
    Complete(usize),
    BareLf,
    Incomplete,
}

fn head_end(buf: &[u8]) -> HeadEnd {
    // Empty lines before the request line are skipped, as Netty does.
    let mut start = 0;
    while buf[start..].starts_with(b"\r\n") {
        start += 2;
    }
    let mut line_start = start;
    for i in start..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        if i == 0 || buf[i - 1] != b'\r' {
            return HeadEnd::BareLf;
        }
        if i - 1 == line_start && line_start > start {
            return HeadEnd::Complete(i + 1);
        }
        line_start = i + 1;
    }
    HeadEnd::Incomplete
}

/// Netty's `HttpVersion.valueOf`, then Elasticsearch's check for 1.0 and 1.1: the version to
/// answer with, or `(status line version, reason)` of the refusal.
fn version(text: &str) -> Result<&'static str, (String, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Err(("HTTP/1.0".into(), "text is empty (possibly HTTP/0.9)".into()));
    }
    let upper = text.to_ascii_uppercase();
    match upper.as_str() {
        "HTTP/1.1" => return Ok("HTTP/1.1"),
        "HTTP/1.0" => return Ok("HTTP/1.0"),
        _ => {}
    }
    // `(\S+)/(\d+)\.(\d+)` over the whole text.
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let valid = upper.split_once('/').is_some_and(|(proto, nums)| {
        !proto.is_empty() && !proto.contains(char::is_whitespace) && nums.split_once('.').is_some_and(|(a, b)| digits(a) && digits(b))
    });
    if !valid {
        return Err(("HTTP/1.0".into(), format!("invalid version format: {upper}")));
    }
    Err((upper.clone(), format!("Unexpected http protocol version: {upper}")))
}

/// RFC 7230 `tchar`.
fn is_token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

fn answer(version: &str, kind: &str, reason: &str) -> Vec<u8> {
    let reason = serde_json::to_string(reason).unwrap_or_default();
    let body = format!(r#"{{"error":{{"root_cause":[{{"type":"{kind}","reason":{reason}}}],"type":"{kind}","reason":{reason}}},"status":400}}"#);
    format!("{version} 400 Bad Request\r\nX-elastic-product: Elasticsearch\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

/// Netty's refusal of a `Content-Length` next to `Transfer-Encoding`.
const CONTENT_LENGTH_NOT_ALLOWED: &str = "Content-Length are not allowed in HTTP/1.1 messages that contains a Transfer-Encoding header.";

/// Checks a complete head (request line and header lines, CRLF-terminated).
fn check(head: &[u8]) -> Verdict {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n").skip_while(|l| l.is_empty());
    let Some(request_line) = lines.next() else { return Verdict::Pass };
    // `HttpObjectDecoder.splitInitialLine`: method, target, and the rest as the version.
    let rest = request_line.trim_start();
    let after_method = rest.find(char::is_whitespace).map_or("", |i| rest[i..].trim_start());
    let after_target = after_method.find(char::is_whitespace).map_or("", |i| after_method[i..].trim_start());
    let version = match version(after_target) {
        Ok(v) => v,
        Err((v, reason)) => return Verdict::Answer(answer(&v, "illegal_argument_exception", &reason)),
    };
    let mut content_lengths: Vec<&str> = Vec::new();
    let mut chunked = false;
    for line in lines.filter(|l| !l.is_empty()) {
        if line.starts_with([' ', '\t']) {
            // A folded continuation of the previous header.
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Verdict::Answer(answer(version, "illegal_argument_exception", "No colon found"));
        };
        if let Some(i) = name.bytes().position(|c| !is_token(c)) {
            let reason = format!(
                "a header name can only contain \"token\" characters, but found invalid character 0x{:x} at index {i} of header '{name}'.",
                name.as_bytes()[i]
            );
            return Verdict::Answer(answer(version, "illegal_argument_exception", &reason));
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_lengths.push(value.trim());
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
    }
    if content_lengths.len() > 1 {
        return Verdict::Answer(answer(version, "illegal_argument_exception", &format!("Multiple Content-Length values found: [{}]", content_lengths.join(", "))));
    }
    if content_lengths.iter().any(|v| v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit())) {
        return Verdict::Drop;
    }
    if chunked && version == "HTTP/1.0" {
        return Verdict::Answer(answer(version, "transfer_encoding_not_allowed_exception", "The Transfer-Encoding header is only allowed in HTTP/1.1 or newer"));
    }
    if chunked && !content_lengths.is_empty() {
        return Verdict::Answer(answer(version, "content_length_not_allowed_exception", CONTENT_LENGTH_NOT_ALLOWED));
    }
    Verdict::Pass
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for FirstHead<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            match &mut this.state {
                State::Done => return Pin::new(&mut this.inner).poll_read(cx, buf),
                State::Closed => return Poll::Ready(Ok(())),
                State::Passing(held, at) => {
                    let n = (held.len() - *at).min(buf.remaining());
                    buf.put_slice(&held[*at..*at + n]);
                    *at += n;
                    if *at == held.len() {
                        this.state = State::Done;
                    }
                    return Poll::Ready(Ok(()));
                }
                State::Rejecting(resp, at) => {
                    while *at < resp.len() {
                        match Pin::new(&mut this.inner).poll_write(cx, &resp[*at..]) {
                            Poll::Ready(Ok(0)) | Poll::Ready(Err(_)) => break,
                            Poll::Ready(Ok(n)) => *at += n,
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    if Pin::new(&mut this.inner).poll_flush(cx).is_pending() {
                        return Poll::Pending;
                    }
                    this.state = State::Closed;
                }
                State::Reading(held) => {
                    let mut chunk = [0u8; 4096];
                    let mut rb = ReadBuf::new(&mut chunk);
                    match Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => {}
                    }
                    let got = rb.filled();
                    if got.is_empty() {
                        // The client stopped mid-head: hyper sees what it sent.
                        let held = std::mem::take(held);
                        this.state = if held.is_empty() { State::Done } else { State::Passing(held, 0) };
                        continue;
                    }
                    held.extend_from_slice(got);
                    let verdict = match head_end(held) {
                        HeadEnd::Complete(end) => check(&held[..end]),
                        HeadEnd::BareLf => Verdict::Answer(answer(
                            "HTTP/1.0",
                            "invalid_line_separator_exception",
                            "Line Feed must be preceded by Carriage Return when terminating HTTP start- and header field-lines",
                        )),
                        HeadEnd::Incomplete if held.len() > MAX_HEAD => Verdict::Pass,
                        HeadEnd::Incomplete => continue,
                    };
                    this.state = match verdict {
                        Verdict::Pass => State::Passing(std::mem::take(held), 0),
                        Verdict::Answer(resp) => State::Rejecting(resp, 0),
                        Verdict::Drop => State::Closed,
                    };
                }
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for FirstHead<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(raw: &[u8]) -> Verdict {
        match head_end(raw) {
            HeadEnd::Complete(end) => check(&raw[..end]),
            HeadEnd::BareLf => Verdict::Answer(b"bare lf".to_vec()),
            HeadEnd::Incomplete => panic!("incomplete"),
        }
    }

    fn reason(v: Verdict) -> String {
        match v {
            Verdict::Answer(r) => String::from_utf8(r).unwrap(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn heads_netty_accepts_pass() {
        for raw in [&b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..], b"\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n", b"GET / http/1.1\r\n\r\n", b"POST /x HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}"] {
            assert_eq!(verdict(raw), Verdict::Pass, "{}", String::from_utf8_lossy(raw));
        }
        assert!(matches!(head_end(b"GET / HTTP/1.1\r\nHost"), HeadEnd::Incomplete));
        assert!(matches!(head_end(b"GET / HTTP/1.1\nHost: x\n\n"), HeadEnd::BareLf));
    }

    #[test]
    fn refusals_name_what_netty_rejected() {
        let r = reason(verdict(b"GET / HTTP/2.5\r\nHost: x\r\n\r\n"));
        assert!(r.starts_with("HTTP/2.5 400 Bad Request\r\n") && r.contains(r#""reason":"Unexpected http protocol version: HTTP/2.5""#), "{r}");
        let r = reason(verdict(b"GET / HTTP/1.1 extra\r\n\r\n"));
        assert!(r.starts_with("HTTP/1.0 400") && r.contains("invalid version format: HTTP/1.1 EXTRA"), "{r}");
        assert!(reason(verdict(b"GARBAGE\r\n\r\n")).contains("text is empty (possibly HTTP/0.9)"));
        assert!(reason(verdict(b"GET / HTTP/x\r\n\r\n")).contains("invalid version format: HTTP/X"));
        let r = reason(verdict(b"GET / HTTP/1.1\r\nFoo : bar\r\n\r\n"));
        assert!(r.starts_with("HTTP/1.1 400") && r.contains("but found invalid character 0x20 at index 3 of header 'Foo '."), "{r}");
        assert!(reason(verdict(b"GET / HTTP/1.1\r\nBad Header\r\n\r\n")).contains(r#""reason":"No colon found""#));
        assert!(reason(verdict(b"POST / HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n")).contains("Multiple Content-Length values found: [2, 3]"));
        assert_eq!(verdict(b"POST / HTTP/1.1\r\nContent-Length: 2, 2\r\n\r\n"), Verdict::Drop);
        assert!(reason(verdict(b"POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n")).contains("transfer_encoding_not_allowed_exception"));
        assert!(reason(verdict(b"POST / HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n")).contains("content_length_not_allowed_exception"));
    }
}
