//! Next.js's cache across an environment's instances and after it sleeps (Vercel's ISR and Data
//! Cache, OpenNext's on Netlify; docs/apps/README.md, "Next.js across instances"). A Next build
//! gets roll's adapter (`apps_build::next_cache`), which points Next's `cacheHandler` (ISR pages,
//! route handlers, images, `fetch`) and `cacheHandlers` (`"use cache"`) at this listener, and its
//! instances get [`URL_VAR`] and [`TOKEN_VAR`] (`rlr_`, sealed). Entries are opaque bytes in object
//! storage (each upload a key of its own) with their tags; a deployment's pages are its own
//! (`d.<deployment>`), Next's fetch cache (`data`) every deployment's. `revalidateTag` and
//! `revalidatePath` are marks on the environment's tags, and a read is answered against them in
//! the same place, so every instance sees an invalidation at once. Read from the store and object
//! storage, so any node answers. An entry unused for [`KEEP_DAYS`] goes, or past its own expiry,
//! and an environment's least recently used past [`MAX_CACHE_BYTES`] as each lands.
//!
//! - `GET {PATH}/{scope}/{key}` (`x-roll-soft-tags`: the path's tags, a percent-encoded JSON
//!   list): the bytes, `x-roll-written` and `x-roll-stale: 1` when a tag made it stale; 404 when
//!   there's none, it expired or a tag expired it.
//! - `PUT {PATH}/{scope}/{key}` (`x-roll-tags`, `x-roll-written` in unix ms, `x-roll-expire` in
//!   seconds). The bytes are the handler's own framing of Next's entry.
//! - `POST {PATH}/tags` `{"tags": […], "expire": seconds}`: expired now, or (with `expire`) stale
//!   now and expired that much later.

use crate::Apps;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

/// Where instances reach it.
pub const PATH: &str = "/_roll/next";
/// The variables an instance gets.
pub const URL_VAR: &str = "ROLL_NEXT_CACHE_URL";
pub const TOKEN_VAR: &str = "ROLL_NEXT_CACHE_TOKEN";
/// The largest entry taken.
pub const MAX_ENTRY: u64 = 32 << 20;
/// What one environment's cache holds at most; the least recently used go past it.
pub const MAX_CACHE_BYTES: i64 = 5 << 30;
/// Days an entry nobody read or wrote is kept.
pub const KEEP_DAYS: i64 = 7;
/// Tag invalidations kept an environment; past them the oldest go, with the entries before them.
pub const MAX_MARKS: usize = 20_000;
/// Tags on one entry or one call, and a tag's length (Next's own limits: 128 tags of 256).
const MAX_TAGS: usize = 128;
const MAX_TAG: usize = 256;

/// One entry as the store has it, and whether a tag made it stale.
#[derive(Clone, Debug)]
pub struct Entry {
    pub object: String,
    pub bytes: i64,
    pub written_at: i64,
    pub used_at: i64,
    pub stale: bool,
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": { "message": msg } }))).into_response()
}

/// A scope: `data`, or `d.` and a deployment id.
fn scope_ok(s: &str) -> bool {
    s == "data" || s.strip_prefix("d.").is_some_and(|d| (1..=64).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
}

/// A key: the handler's SHA-256 of Next's own, in hex.
fn key_ok(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A percent-encoded JSON list of tags from a header: at most [`MAX_TAGS`], each 1–[`MAX_TAG`]
/// bytes.
fn tags_header(h: &HeaderMap, name: &str) -> Result<Vec<String>, &'static str> {
    let Some(v) = h.get(name) else { return Ok(Vec::new()) };
    let text = percent_encoding::percent_decode(v.as_bytes()).decode_utf8().map_err(|_| "tags aren't UTF-8")?;
    let tags: Vec<String> = serde_json::from_str(&text).map_err(|_| "tags are a percent-encoded JSON list of strings")?;
    tags_ok(&tags)?;
    Ok(tags)
}

fn tags_ok(tags: &[String]) -> Result<(), &'static str> {
    if tags.len() > MAX_TAGS || tags.iter().any(|t| t.is_empty() || t.len() > MAX_TAG) {
        return Err("at most 128 tags, each 1–256 bytes");
    }
    Ok(())
}

fn object_key(service: &str, env: &str, scope: &str, key: &str) -> String {
    format!("apps/next-cache/{service}/{env}/{scope}/{key}.{}", crate::model::new_id("u"))
}

fn header_i64(h: &HeaderMap, name: &str) -> Option<i64> {
    h.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
}

/// Every request under [`PATH`].
pub(crate) async fn serve(apps: Arc<Apps>, req: Request) -> Response {
    let path = req.uri().path()[PATH.len()..].to_string();
    let Some(objects) = apps.deps.objects.clone() else { return err(StatusCode::SERVICE_UNAVAILABLE, "this node has no object storage for a Next.js cache") };
    let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim).unwrap_or("").to_string();
    if !token.starts_with("rlr_") {
        return err(StatusCode::UNAUTHORIZED, "a Next.js cache token is needed");
    }
    let hash = crate::public::token_hash(&token);
    let Ok(Some((service, env))) = apps.db(move |s| s.next_cache_owner(&hash)).await else { return err(StatusCode::UNAUTHORIZED, "not a Next.js cache token") };
    let now = code_sql::now_ms();
    if path == "/tags" {
        if req.method() != Method::POST {
            return err(StatusCode::METHOD_NOT_ALLOWED, "POST");
        }
        let Ok(body) = axum::body::to_bytes(req.into_body(), 64 << 10).await else { return err(StatusCode::PAYLOAD_TOO_LARGE, "at most 64 KiB") };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else { return err(StatusCode::BAD_REQUEST, "a JSON body: {\"tags\": […], \"expire\": seconds}") };
        let Some(tags) = v["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect::<Vec<_>>()).filter(|t| !t.is_empty()) else {
            return err(StatusCode::BAD_REQUEST, "tags is a list of strings");
        };
        if let Err(m) = tags_ok(&tags) {
            return err(StatusCode::BAD_REQUEST, m);
        }
        let expire = v["expire"].as_f64().filter(|s| s.is_finite() && *s >= 0.0).map(|s| (s as i64).min(366 * 86_400));
        return match apps.db(move |s| s.mark_next_tags(&service, &env, &tags, expire, now, MAX_MARKS)).await {
            Ok(gone) => {
                apps.drop_cache_objects(gone);
                StatusCode::NO_CONTENT.into_response()
            }
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }
    let Some((scope, key)) = path.strip_prefix('/').and_then(|p| p.split_once('/')).map(|(s, k)| (s.to_string(), k.to_string())) else {
        return err(StatusCode::NOT_FOUND, "not a Next.js cache path");
    };
    if !scope_ok(&scope) || !key_ok(&key) {
        return err(StatusCode::BAD_REQUEST, "a scope (data, d.<deployment>) and a key (SHA-256 in hex)");
    }
    match *req.method() {
        Method::GET => {
            let soft = match tags_header(req.headers(), "x-roll-soft-tags") {
                Ok(t) => t,
                Err(m) => return err(StatusCode::BAD_REQUEST, m),
            };
            let (s, e, sc, k) = (service.clone(), env.clone(), scope.clone(), key.clone());
            let Ok(Some(entry)) = apps.db(move |st| st.next_entry(&s, &e, &sc, &k, &soft, now)).await else { return err(StatusCode::NOT_FOUND, "no such entry") };
            // Used: kept another week (written at most hourly).
            if now - entry.used_at > 3_600_000 {
                let _ = apps.db(move |st| st.touch_next_entry(&service, &env, &scope, &key, now)).await;
            }
            // Streamed from object storage, never held whole.
            let (mut tx, rx) = tokio::io::duplex(256 << 10);
            let object = entry.object.clone();
            tokio::spawn(async move {
                let _ = objects.download(&object, &mut tx).await;
            });
            let mut r = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(rx)));
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(entry.bytes));
            h.insert("x-roll-written", HeaderValue::from(entry.written_at));
            if entry.stale {
                h.insert("x-roll-stale", HeaderValue::from_static("1"));
            }
            r
        }
        Method::PUT => {
            if header_i64(req.headers(), header::CONTENT_LENGTH.as_str()).is_some_and(|n| n as u64 > MAX_ENTRY) {
                return err(StatusCode::PAYLOAD_TOO_LARGE, "an entry is at most 32 MiB");
            }
            let tags = match tags_header(req.headers(), "x-roll-tags") {
                Ok(t) => t,
                Err(m) => return err(StatusCode::BAD_REQUEST, m),
            };
            // When its render started (what an invalidation after it makes old), never ahead of
            // now nor more than ten minutes back.
            let written = header_i64(req.headers(), "x-roll-written").unwrap_or(now).clamp(now - 600_000, now);
            let expires = header_i64(req.headers(), "x-roll-expire").filter(|s| *s > 0).map(|s| written + s.min(366 * 86_400) * 1000);
            use futures::TryStreamExt;
            let body = http_body_util::Limited::new(req.into_body(), MAX_ENTRY as usize);
            let stream = Body::new(body).into_data_stream().map_err(|e| std::io::Error::other(e.to_string()));
            let reader = tokio_util::io::StreamReader::new(stream);
            let object = object_key(&service, &env, &scope, &key);
            let bytes = match objects.upload(&object, reader).await {
                Ok(n) => n as i64,
                Err(e) => {
                    let _ = objects.delete(&object).await;
                    return err(StatusCode::BAD_REQUEST, &format!("the entry didn't arrive whole: {e:#}"));
                }
            };
            let o = object.clone();
            match apps.db(move |s| s.put_next_entry(&service, &env, &scope, &key, &o, bytes, &tags, written, expires, now, MAX_CACHE_BYTES)).await {
                Ok(gone) => {
                    apps.drop_cache_objects(gone);
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(e) => {
                    let _ = objects.delete(&object).await;
                    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
                }
            }
        }
        _ => err(StatusCode::METHOD_NOT_ALLOWED, "GET or PUT"),
    }
}

/// The tags a purge of `paths` marks: Next's own for a path (`revalidatePath`), its layout's for
/// a prefix (`/blog/*`: `revalidatePath("/blog", "layout")`).
pub fn path_tags(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| match p.strip_suffix("/*") {
            Some(prefix) => format!("_N_T_{}/layout", prefix.trim_end_matches('/')),
            None => format!("_N_T_{p}"),
        })
        .collect()
}

impl Apps {
    /// What a Next build's instances get: the cache's address and its token (sealed), or none
    /// on a node without object storage or a public base.
    pub(crate) async fn next_cache_vars(self: &Arc<Self>, service: &str, env: &str) -> crate::Result<Option<(String, String)>> {
        let (Some(base), Some(_)) = (&self.cfg.otlp_base, &self.deps.objects) else { return Ok(None) };
        let (s, e) = (service.to_string(), env.to_string());
        let token = self.db(move |st| st.next_cache_token(&s, &e)).await?;
        Ok(Some((self.deps.runtime.reach(&format!("{}{PATH}", base.trim_end_matches('/'))), token)))
    }

    /// An environment's Next.js cache after a purge (docs: "Purging"): everything gone with no
    /// paths or tags, else those tags (and the paths' own) expired.
    pub(crate) async fn purge_next_cache(self: &Arc<Self>, service: &str, env: &str, paths: Option<&[String]>, tags: Option<&[String]>) {
        let (s, e) = (service.to_string(), env.to_string());
        let gone = match (paths, tags) {
            (None, None) => self.db(move |st| st.drop_next_cache(&s, Some(&e), false)).await,
            _ => {
                let mut marks = paths.map(path_tags).unwrap_or_default();
                marks.extend(tags.unwrap_or_default().iter().cloned());
                self.db(move |st| st.mark_next_tags(&s, &e, &marks, None, code_sql::now_ms(), MAX_MARKS)).await
            }
        };
        if let Ok(gone) = gone {
            self.drop_cache_objects(gone);
        }
    }

    /// Entries unused for [`KEEP_DAYS`] or past their expiry deleted, and what each
    /// environment's cache holds metered, a GB-hour each hour (`stored_gb_hours`), by its
    /// service's owner, each hour once.
    pub(crate) async fn next_cache_pass(self: &Arc<Self>, now: i64) {
        if let Ok(gone) = self.db(move |s| s.expire_next_entries(now - KEEP_DAYS * 86_400_000, now)).await {
            self.drop_cache_objects(gone);
        }
        let Ok(held) = self.db(|s| s.next_cache_bytes()).await else { return };
        for (service, env, bytes) in held.into_iter().filter(|(s, _, _)| self.owns(s)) {
            let _ = self.db(move |s| s.meter_stored(&service, &env, "next", now / 3_600_000, bytes as f64 / 1e9)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scopes_keys_and_path_tags() {
        assert!(super::scope_ok("data") && super::scope_ok("d.dep_abc-1"));
        for bad in ["", "d.", "x", "d.../a", "d.a/b"] {
            assert!(!super::scope_ok(bad), "{bad}");
        }
        assert!(super::key_ok(&"a".repeat(64)) && !super::key_ok(&"g".repeat(64)) && !super::key_ok("ab"));
        assert_eq!(super::path_tags(&["/blog/a".into(), "/blog/*".into()]), ["_N_T_/blog/a", "_N_T_/blog/layout"]);
    }
}
