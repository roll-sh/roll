//! Image optimization (docs/apps/README.md, "Image optimization"; Vercel's and Netlify's image
//! CDN, Next.js's `/_next/image`): `GET /__roll/image?url=/hero.jpg&w=640&q=75` on an
//! environment's own host answers that image of the environment's, resized to a width from
//! [`WIDTHS`] and encoded as WebP when the visitor accepts it (AVIF, JPEG or PNG with `f=`), by
//! roll storage's engine (`s3_image`: no I/O, bounded input and pixels, metadata dropped).
//!
//! The source is fetched in process the way the visitor's own request for it would be: through
//! the environment's proxy with the visitor's headers, so its gates (access, allow list,
//! firewall, rate limit) hold and a static site's files and an app's answers both work. Only a
//! path of the environment's own (`/…`, never a URL elsewhere), so it can't be made to fetch
//! another host. What it makes is kept on the node ([`ImageCache`], keyed by the environment, its
//! live deployment, the path and the plan, so a deploy changes the key), within [`CACHE_BYTES`];
//! at most [`AT_ONCE`] transforms run at once on the node.

use crate::Apps;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Its path on every environment's host.
pub const PATH: &str = "/__roll/image";
/// The widths it makes (Next.js's device and image sizes, up to the engine's largest side).
pub const WIDTHS: [u32; 16] = [16, 32, 48, 64, 96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048, 2500];
/// What's kept on a node, in bytes.
pub const CACHE_BYTES: usize = 64 << 20;
/// One image larger than this isn't kept (it's still answered).
const KEEP_AT_MOST: usize = 8 << 20;
/// Transforms at once on a node (each is CPU work on rolld's own blocking pool).
pub const AT_ONCE: usize = 2;
/// The quality when none is asked (Next.js's).
const DEFAULT_QUALITY: u8 = 75;

#[derive(Clone)]
struct Kept {
    bytes: Bytes,
    content_type: &'static str,
    cache_control: Option<HeaderValue>,
    vary: bool,
}

/// Images made on this node, newest last, within [`CACHE_BYTES`].
pub struct ImageCache {
    kept: Mutex<(HashMap<String, Kept>, VecDeque<String>, usize)>,
    slots: tokio::sync::Semaphore,
}

impl Default for ImageCache {
    fn default() -> Self {
        ImageCache { kept: Mutex::new((HashMap::new(), VecDeque::new(), 0)), slots: tokio::sync::Semaphore::new(AT_ONCE) }
    }
}

impl ImageCache {
    fn get(&self, key: &str) -> Option<Kept> {
        self.kept.lock().0.get(key).cloned()
    }

    fn put(&self, key: String, k: Kept) {
        if k.bytes.len() > KEEP_AT_MOST {
            return;
        }
        let mut g = self.kept.lock();
        let (map, order, bytes) = &mut *g;
        if let Some(old) = map.insert(key.clone(), k.clone()) {
            *bytes -= old.bytes.len();
        } else {
            order.push_back(key);
        }
        *bytes += k.bytes.len();
        while *bytes > CACHE_BYTES {
            let Some(oldest) = order.pop_front() else { break };
            if let Some(gone) = map.remove(&oldest) {
                *bytes -= gone.bytes.len();
            }
        }
    }
}

fn answer(status: StatusCode, text: &str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text.to_string()).into_response()
}

/// What a request asks: the path, the width, the quality and the format.
pub(crate) fn parse(query: &str) -> Result<(String, s3_image::Params), String> {
    let (mut url, mut w, mut q, mut f) = (None, None, None, None);
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "url" => url = Some(v.into_owned()),
            "w" => w = Some(v.parse::<u32>().map_err(|_| "w is a width".to_string())?),
            "q" => q = Some(v.parse::<u8>().map_err(|_| "q is a quality, 20–100".to_string())?),
            "f" => f = Some(v.into_owned()),
            other => return Err(format!("{other} isn't a parameter (url, w, q, f)")),
        }
    }
    let url = url.ok_or("url is the image's path on this host (/hero.jpg)")?;
    // A path of its own, never another host (`//evil`, `https://…`) nor roll's own paths.
    if !url.starts_with('/') || url.starts_with("//") || url.starts_with("/__roll/") || url.contains('\\') || url.len() > 2000 || url.chars().any(char::is_control) {
        return Err("url is the image's path on this host (/hero.jpg)".into());
    }
    let w = w.ok_or("w is the width to make")?;
    if !WIDTHS.contains(&w) {
        return Err(format!("w is one of {}", WIDTHS.map(|n| n.to_string()).join(", ")));
    }
    let q = q.unwrap_or(DEFAULT_QUALITY);
    if !(s3_image::MIN_QUALITY..=s3_image::MAX_QUALITY).contains(&q) {
        return Err("q is a quality, 20–100".into());
    }
    let format = match f.as_deref() {
        None | Some("auto") => s3_image::Format::Auto,
        Some("webp") => s3_image::Format::Webp,
        Some("avif") => s3_image::Format::Avif,
        Some("jpeg" | "jpg") => s3_image::Format::Jpeg,
        Some("png") => s3_image::Format::Png,
        Some(_) => return Err("f is auto, webp, avif, jpeg or png".into()),
    };
    Ok((url, s3_image::Params { width: Some(w), height: None, resize: s3_image::Resize::Cover, quality: Some(q), format }))
}

/// `GET /__roll/image?…` on `env`'s host.
pub(crate) async fn serve(apps: Arc<Apps>, service: String, env: String, req: Request) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return answer(StatusCode::METHOD_NOT_ALLOWED, "an image is a GET");
    }
    let (url, params) = match parse(req.uri().query().unwrap_or("")) {
        Ok(p) => p,
        Err(why) => return answer(StatusCode::BAD_REQUEST, &why),
    };
    let accept = req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).map(str::to_string);
    let plan = params.resolve(accept.as_deref());
    // A deploy is another key: what was made from the old one's files is never answered again.
    let (sid, e) = (service.clone(), env.clone());
    let live = apps.db(move |s| s.environment(&sid, &e)).await.ok().and_then(|r| r.active).unwrap_or_default();
    let key = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(format!("{service}\n{env}\n{live}\n{url}\n{}", plan.key()).as_bytes()))
    };
    if let Some(k) = apps.images.get(&key) {
        return respond(k, "hit");
    }
    // The source, as the visitor's own request for it would be (its gates, its cookies).
    let mut inner = Request::builder().method(Method::GET).uri(url.as_str());
    for (k, v) in req.headers() {
        if !matches!(k.as_str(), "accept" | "accept-encoding" | "range" | "if-none-match" | "if-modified-since" | "content-length") {
            inner = inner.header(k, v);
        }
    }
    let Ok(mut inner) = inner.header(header::ACCEPT, "image/*").header(header::ACCEPT_ENCODING, "identity").body(Body::empty()) else {
        return answer(StatusCode::BAD_REQUEST, "url is the image's path on this host (/hero.jpg)");
    };
    if let Some(c) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
        inner.extensions_mut().insert(*c);
    }
    let src = crate::public::serve(apps.clone(), service, env, None, inner).await;
    if src.status() != StatusCode::OK {
        let status = src.status();
        return answer(if status.is_server_error() { StatusCode::BAD_GATEWAY } else { status }, &format!("the image's path answered {}", status.as_u16()));
    }
    let is_image = src.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("image/"));
    if !is_image {
        return answer(StatusCode::UNSUPPORTED_MEDIA_TYPE, "the url isn't an image (its Content-Type isn't image/*)");
    }
    let cache_control = src.headers().get(header::CACHE_CONTROL).cloned();
    let limits = s3_image::Limits::default();
    let body = match http_body_util::BodyExt::collect(http_body_util::Limited::new(src.into_body(), limits.max_input_bytes)).await {
        Ok(b) => b.to_bytes(),
        Err(_) => return answer(StatusCode::PAYLOAD_TOO_LARGE, "the image is over 25 MiB"),
    };
    let Ok(_slot) = apps.images.slots.acquire().await else { return answer(StatusCode::SERVICE_UNAVAILABLE, "try again") };
    let input = body.clone();
    let made = tokio::task::spawn_blocking(move || s3_image::transform(&input, &plan, &limits)).await;
    let made = match made {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            let status = match e {
                s3_image::ImageError::InvalidParams(_) => StatusCode::BAD_REQUEST,
                s3_image::ImageError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
                s3_image::ImageError::Unsupported(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                s3_image::ImageError::Corrupt(_) => StatusCode::UNPROCESSABLE_ENTITY,
                s3_image::ImageError::Timeout => StatusCode::GATEWAY_TIMEOUT,
                s3_image::ImageError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return answer(status, &e.to_string());
        }
        Err(_) => return answer(StatusCode::INTERNAL_SERVER_ERROR, "the image couldn't be made"),
    };
    let kept = match made {
        s3_image::Rendered::Image { bytes, format, .. } => Kept { bytes: Bytes::from(bytes), content_type: format.content_type(), cache_control, vary: plan.negotiated },
        // An animated image without a format asked: as it is.
        s3_image::Rendered::Original { format } => Kept { bytes: body, content_type: format.content_type(), cache_control, vary: plan.negotiated },
    };
    apps.images.put(key, kept.clone());
    respond(kept, "miss")
}

fn respond(k: Kept, cache: &'static str) -> Response {
    let mut r = Response::new(Body::from(k.bytes));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(k.content_type));
    // The source's own caching (a site's files are its); else a minute.
    h.insert(header::CACHE_CONTROL, k.cache_control.unwrap_or_else(|| HeaderValue::from_static("public, max-age=60")));
    if k.vary {
        h.insert(header::VARY, HeaderValue::from_static("Accept"));
    }
    h.insert("x-roll-image", HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_its_own_paths_and_listed_widths() {
        assert!(parse("url=%2Fhero.jpg&w=640").is_ok());
        assert_eq!(parse("url=/a.png&w=640&q=50&f=avif").unwrap().1.format, s3_image::Format::Avif);
        for bad in ["url=//evil.example/a.png&w=640", "url=https://evil.example/a.png&w=640", "url=/__roll/image&w=640", "url=/a.png&w=641", "url=/a.png", "w=640", "url=/a.png&w=640&q=5", "url=/a.png&w=640&f=gif", "url=/a.png&w=640&x=1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_cache_keeps_the_newest_within_its_bytes() {
        let c = ImageCache::default();
        let k = |n: usize| Kept { bytes: Bytes::from(vec![0u8; n]), content_type: "image/webp", cache_control: None, vary: false };
        for i in 0..20 {
            c.put(format!("k{i}"), k(5 << 20));
        }
        assert!(c.get("k0").is_none() && c.get("k19").is_some());
        assert!(c.kept.lock().2 <= CACHE_BYTES);
        c.put("big".into(), k(KEEP_AT_MOST + 1));
        assert!(c.get("big").is_none(), "one too big isn't kept");
    }
}
