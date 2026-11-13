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
const TAG: &[u8] = b"<script src=\"/__roll/feedback.js\" defer></script>";
const SCRIPT: &str = include_str!("../assets/feedback.js");

/// Adds the tag to a page as it streams: before the first `</body>` (matched across chunks, in
/// any case), or at the end of a page without one. Memory stays one chunk.
struct Tagger {
    held: Vec<u8>,
    done: bool,
}

impl Tagger {
    const CLOSE: &'static [u8] = b"</body>";

    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(chunk);
        if let Some(at) = buf.windows(Self::CLOSE.len()).position(|w| w.eq_ignore_ascii_case(Self::CLOSE)) {
            self.done = true;
            buf.splice(at..at, TAG.iter().copied());
            return buf;
        }
        let keep = (Self::CLOSE.len() - 1).min(buf.len());
        self.held = buf.split_off(buf.len() - keep);
        buf
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut rest = std::mem::take(&mut self.held);
        if !self.done {
            self.done = true;
            rest.extend_from_slice(TAG);
        }
        rest
    }
}

/// A page with the button added as it streams; anything that isn't an uncompressed 200 HTML page
/// passes as it came.
pub fn add(resp: Response) -> Response {
    let html = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("text/html"));
    let plain = resp.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_none_or(|e| e.eq_ignore_ascii_case("identity"));
    if resp.status() != StatusCode::OK || !html || !plain {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    // This visitor's page now: no shared cache may hand it to anyone else.
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ETAG);
    parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    let stream = futures::stream::unfold((body.into_data_stream(), Tagger { held: Vec::new(), done: false }, false), |(mut s, mut t, ended)| async move {
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
pub fn script(target: &str) -> Response {
    let js = SCRIPT.replace("__ROLL_TARGET__", &serde_json::to_string(target).unwrap_or_else(|_| "\"\"".into()));
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "private, max-age=300")], js).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tag_goes_before_the_body_closes_even_across_chunks() {
        let mut t = super::Tagger { held: Vec::new(), done: false };
        let mut out = t.push(b"<p>hi</p></BO");
        out.extend(t.push(b"DY></html>"));
        out.extend(t.finish());
        assert_eq!(String::from_utf8(out).unwrap(), r#"<p>hi</p><script src="/__roll/feedback.js" defer></script></BODY></html>"#);
    }
}
