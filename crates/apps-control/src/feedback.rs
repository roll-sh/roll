//! The preview feedback button (roll previews' "Feedback on the preview page", docs/previews.md)
//! on a preview environment of roll apps: when roll previews asked for it (`feedback` on the
//! environment) and the visitor came in through roll (the environment's access cookie), a page
//! the app serves gets `<script src="/__roll/feedback.js" defer>` before `</body>`, and that
//! script is served from the preview's own host. It holds no credential: it opens roll's
//! dashboard, where the signed-in person posts the note on the change. The script and the
//! tagger are roll previews' (`roll-previews/src/feedback.js`, `runtime.rs` `Tagger`), kept equal
//! until one copy serves both (docs/apps/README.md, "With the other platform branches").

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

pub const PATH: &str = "/__roll/feedback.js";
/// What's been said about a page (`?path=`), for the button's list.
pub const NOTES: &str = "/__roll/feedback.json";
/// The most a list holds.
const MAX_NOTES: usize = 200;
const TAG: &[u8] = b"<script src=\"/__roll/feedback.js\" defer></script>";
const SCRIPT: &str = include_str!("../assets/feedback.js");

/// Adds the tag to a page as it streams: before the first `</body>` (matched across chunks, in
/// any case), or at the end of a page without one. Memory stays one chunk.
struct Tagger {
    tag: Bytes,
    /// Put before this tag (any case).
    close: &'static [u8],
    /// A page without it gets the tag at its end anyway (`</body>`), or not at all (`</head>`).
    at_end: bool,
    held: Vec<u8>,
    done: bool,
}

impl Tagger {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        if let Some(at) = buf.windows(self.close.len()).position(|w| w.eq_ignore_ascii_case(self.close)) {
            self.done = true;
            buf.splice(at..at, self.tag.iter().copied());
            return buf;
        }
        let keep = (self.close.len() - 1).min(buf.len());
        self.held = buf.split_off(buf.len() - keep);
        buf
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut rest = std::mem::take(&mut self.held);
        if !self.done {
            self.done = true;
            if self.at_end {
                rest.extend_from_slice(&self.tag);
            }
        }
        rest
    }
}

/// A page with the button added as it streams; anything that isn't an uncompressed 200 HTML page
/// passes as it came.
pub fn add(resp: Response) -> Response {
    tag(resp, TAG, true)
}

/// A page with `tag` added as it streams, before `</body>` (roll's own scripts: the feedback
/// button, Web Vitals); anything that isn't an uncompressed 200 HTML page passes as it came.
/// `personal`: the page is this visitor's now (no shared cache keeps it); otherwise every visitor
/// gets the same bytes, so its caching stays the app's (its ETag made weak: the bytes changed).
pub fn tag(resp: Response, tag: &'static [u8], personal: bool) -> Response {
    inject(resp, Bytes::from_static(tag), b"</body>", true, personal)
}

/// An environment's own snippets ([`crate::model::Snippet`]): `head` before `</head>` (a page
/// without one gets none), `body` before `</body>`; every visitor gets the same bytes.
pub fn snippets(resp: Response, head: &Bytes, body: &Bytes) -> Response {
    let resp = if head.is_empty() { resp } else { inject(resp, head.clone(), b"</head>", false, false) };
    if body.is_empty() { resp } else { inject(resp, body.clone(), b"</body>", true, false) }
}

fn inject(resp: Response, tag: Bytes, close: &'static [u8], at_end: bool, personal: bool) -> Response {
    let html = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("text/html"));
    let plain = resp.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_none_or(|e| e.eq_ignore_ascii_case("identity"));
    if resp.status() != StatusCode::OK || !html || !plain {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    if personal {
        // This visitor's page now: no shared cache may hand it to anyone else.
        parts.headers.remove(header::ETAG);
        // An app's `no-store` stays: the page still mustn't be kept.
        let no_store = parts.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().contains("no-store"));
        parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(if no_store { "private, no-store" } else { "private, no-cache" }));
    } else if let Some(e) = parts.headers.get(header::ETAG).and_then(|v| v.to_str().ok()).filter(|e| !e.starts_with("W/")).map(|e| format!("W/{e}"))
        && let Ok(v) = HeaderValue::from_str(&e)
    {
        parts.headers.insert(header::ETAG, v);
    }
    let stream = futures::stream::unfold((body.into_data_stream(), Tagger { tag, close, at_end, held: Vec::new(), done: false }, false), |(mut s, mut t, ended)| async move {
        if ended {
            return None;
        }
        match s.next().await {
            Some(Ok(c)) => Some((Ok::<_, axum::Error>(Bytes::from(t.push(&c))), (s, t, false))),
            Some(Err(e)) => Some((Err(e), (s, t, true))),
            None => Some((Ok(Bytes::from(t.finish())), (s, t, true))),
        }
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// The button's script, sending notes to `target` (the preview in roll's dashboard).
/// The notes on the page `query`'s `path` names (the path only, at most 2 KiB), from roll
/// previews: `no-store`, at most [`MAX_NOTES`]; 404 without previews, 502 when it fails (the
/// button then shows no list).
pub async fn notes(notes: Option<std::sync::Arc<dyn crate::integrations::FeedbackNotes>>, env: &str, query: Option<&str>) -> Response {
    let Some(n) = notes else { return StatusCode::NOT_FOUND.into_response() };
    let path = query
        .unwrap_or("")
        .split('&')
        .find_map(|p| p.strip_prefix("path="))
        .map(|p| percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned())
        .filter(|p| p.starts_with('/') && p.len() <= 2048);
    let Some(path) = path else { return (StatusCode::BAD_REQUEST, "path is the page's path").into_response() };
    match tokio::time::timeout(std::time::Duration::from_secs(5), n.notes(env, &path)).await {
        Ok(Ok(mut list)) => {
            let over = list.len().saturating_sub(MAX_NOTES);
            list.drain(..over);
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(list)).into_response()
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub fn script(target: &str) -> Response {
    let js = SCRIPT.replace("__ROLL_TARGET__", &serde_json::to_string(target).unwrap_or_else(|_| "\"\"".into()));
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "private, max-age=300")], js).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tag_goes_before_the_body_closes_even_across_chunks() {
        let mut t = super::Tagger { tag: bytes::Bytes::from_static(super::TAG), close: b"</body>", at_end: true, held: Vec::new(), done: false };
        let mut out = t.push(b"<p>hi</p></BO");
        out.extend(t.push(b"DY></html>"));
        out.extend(t.finish());
        assert_eq!(String::from_utf8(out).unwrap(), r#"<p>hi</p><script src="/__roll/feedback.js" defer></script></BODY></html>"#);
        // A head snippet on a page with no head: nothing added.
        let mut t = super::Tagger { tag: bytes::Bytes::from_static(b"<script>x</script>"), close: b"</head>", at_end: false, held: Vec::new(), done: false };
        let mut out = t.push(b"<p>no head</p>");
        out.extend(t.finish());
        assert_eq!(out, b"<p>no head</p>");
    }
}
