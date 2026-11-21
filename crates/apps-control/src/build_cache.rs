//! A remote build cache for Turborepo and Nx (Vercel's Remote Cache; docs/apps/README.md, "A
//! remote build cache"): each environment of a project has a cache for each source its services
//! build ([`scope`]) in object storage and a token (`rlr_`, sealed), and its builds get `TURBO_API`/`TURBO_TOKEN`/`TURBO_TEAM` and Nx's
//! `NX_SELF_HOSTED_REMOTE_CACHE_SERVER`/`…_ACCESS_TOKEN`, so a monorepo's unchanged tasks are
//! read back instead of run again. Served on the apps listener at [`PATH`]: Turborepo's
//! `/v8/artifacts` and Nx's `/v1/cache`. Read from the store and object storage, so any node
//! answers. An artifact unused for [`KEEP_DAYS`] goes, and a cache's least recently used past
//! [`MAX_CACHE_BYTES`] as each upload lands.

use crate::Apps;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

/// Where builds reach the cache: `TURBO_API` (Turborepo adds `/v8/artifacts/…`) and Nx's server
/// (it adds `/v1/cache/…`).
pub const PATH: &str = "/_roll/cache";
/// The largest artifact taken.
pub const MAX_ARTIFACT: u64 = 256 << 20;
/// Days an artifact nobody read or wrote is kept.
pub const KEEP_DAYS: i64 = 7;
/// What one cache holds at most; the least recently used go past it.
pub const MAX_CACHE_BYTES: i64 = 10 << 30;
/// A tag (Turborepo's signature) kept with an artifact, at most.
const MAX_TAG: usize = 1024;

/// One artifact as the store keeps it: its size, Turborepo's task duration and signature tag.
#[derive(Clone, Debug, PartialEq)]
pub struct Artifact {
    pub bytes: i64,
    pub duration_ms: Option<i64>,
    pub tag: Option<String>,
    pub used_at: i64,
}

/// Which cache a service's builds in `env` use: the environment's, for its source (a repo, a Git
/// URL; an image or upload service its own), so a service built from someone else's repository
/// never writes what the team's monorepo builds read, and a preview never writes production's.
pub fn scope(source: &crate::model::Source, service: &str, env: &str) -> String {
    use sha2::{Digest, Sha256};
    let of = match source {
        crate::model::Source::Repo { repo, .. } => format!("repo:{repo}"),
        crate::model::Source::Git { url, .. } => format!("git:{url}"),
        _ => format!("service:{service}"),
    };
    format!("{env}.{}", Sha256::digest(of.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect::<String>())
}

/// Where one upload's bytes go: a key of its own (a later upload of the same hash never writes
/// over what a read is streaming, nor what an expiry is deleting).
fn object_key(project: &str, ns: &str, hash: &str) -> String {
    format!("apps/build-cache/{project}/{ns}/{hash}.{}", crate::model::new_id("u"))
}

/// A content hash as both tools send it: letters and digits, at most 128.
fn hash_ok(h: &str) -> bool {
    (1..=128).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": { "message": msg } }))).into_response()
}

/// Every request under [`PATH`].
pub(crate) async fn serve(apps: Arc<Apps>, req: Request) -> Response {
    let path = req.uri().path()[PATH.len()..].to_string();
    let Some(objects) = apps.deps.objects.clone() else { return err(StatusCode::SERVICE_UNAVAILABLE, "this node has no object storage for a build cache") };
    // The environment whose cache the bearer token is; a wrong one or none is a 401, as both
    // tools expect.
    let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim).unwrap_or("").to_string();
    if !token.starts_with("rlr_") {
        return err(StatusCode::UNAUTHORIZED, "a build cache token is needed");
    }
    let hash = crate::public::token_hash(&token);
    let Ok(Some((project, ns))) = apps.db(move |s| s.build_cache_owner(&hash)).await else { return err(StatusCode::UNAUTHORIZED, "not a build cache token") };
    let (turbo, artifact) = match path.as_str() {
        "/v8/artifacts/status" => return axum::Json(json!({ "status": "enabled" })).into_response(),
        "/v8/artifacts/events" => return axum::Json(json!({})).into_response(),
        p => match (p.strip_prefix("/v8/artifacts/"), p.strip_prefix("/v1/cache/")) {
            (Some(h), _) => (true, h.to_string()),
            (_, Some(h)) => (false, h.to_string()),
            _ => return err(StatusCode::NOT_FOUND, "not a build cache path"),
        },
    };
    if !hash_ok(&artifact) {
        return err(StatusCode::BAD_REQUEST, "an artifact's hash is letters and digits, at most 128");
    }
    let now = code_sql::now_ms();
    match *req.method() {
        Method::HEAD | Method::GET => {
            let (p, n, h) = (project.clone(), ns.clone(), artifact.clone());
            let Ok(Some((a, key))) = apps.db(move |s| s.artifact(&p, &n, &h)).await else { return err(StatusCode::NOT_FOUND, "no such artifact") };
            // Used: kept another week (written at most hourly).
            if now - a.used_at > 3_600_000 {
                let (p, n, h) = (project.clone(), ns.clone(), artifact.clone());
                let _ = apps.db(move |s| s.touch_artifact(&p, &n, &h, now)).await;
            }
            let mut r = if req.method() == Method::HEAD {
                Response::new(Body::empty())
            } else {
                // Streamed from object storage, never held whole.
                let (mut tx, rx) = tokio::io::duplex(256 << 10);
                tokio::spawn(async move {
                    let _ = objects.download(&key, &mut tx).await;
                });
                Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(rx)))
            };
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(a.bytes));
            if let Some(d) = a.duration_ms {
                h.insert("x-artifact-duration", HeaderValue::from(d));
            }
            if let Some(t) = a.tag.as_deref().and_then(|t| HeaderValue::from_str(t).ok()) {
                h.insert("x-artifact-tag", t);
            }
            r
        }
        Method::PUT => {
            // Nx never overwrites one (a 409, as its server's contract says); Turborepo may. Said
            // before the upload, and again when it's put in (two at once: one wins).
            if !turbo {
                let (p, n, h) = (project.clone(), ns.clone(), artifact.clone());
                if let Ok(Some(_)) = apps.db(move |s| s.artifact(&p, &n, &h)).await {
                    return err(StatusCode::CONFLICT, "that artifact is already cached");
                }
            }
            if req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n > MAX_ARTIFACT) {
                return err(StatusCode::PAYLOAD_TOO_LARGE, "an artifact is at most 256 MiB");
            }
            let duration = req.headers().get("x-artifact-duration").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()).filter(|d| *d >= 0);
            let tag = req.headers().get("x-artifact-tag").and_then(|v| v.to_str().ok()).filter(|t| t.len() <= MAX_TAG).map(str::to_string);
            // Streamed into object storage, cut off past the limit (the upload fails, nothing is kept).
            use futures::TryStreamExt;
            let body = http_body_util::Limited::new(req.into_body(), MAX_ARTIFACT as usize);
            let stream = Body::new(body).into_data_stream().map_err(|e| std::io::Error::other(e.to_string()));
            let reader = tokio_util::io::StreamReader::new(stream);
            let key = object_key(&project, &ns, &artifact);
            let bytes = match objects.upload(&key, reader).await {
                Ok(n) => n as i64,
                Err(e) => {
                    let _ = objects.delete(&key).await;
                    return err(StatusCode::BAD_REQUEST, &format!("the artifact didn't arrive whole: {e:#}"));
                }
            };
            let (p, n, h, k) = (project.clone(), ns.clone(), artifact.clone(), key.clone());
            let a = Artifact { bytes, duration_ms: duration, tag, used_at: now };
            match apps.db(move |s| s.put_artifact(&p, &n, &h, &k, &a, turbo, MAX_CACHE_BYTES)).await {
                Ok(Some(gone)) => {
                    for o in gone {
                        let _ = objects.delete(&o).await;
                    }
                }
                Ok(None) => {
                    let _ = objects.delete(&key).await;
                    return err(StatusCode::CONFLICT, "that artifact is already cached");
                }
                Err(e) => {
                    let _ = objects.delete(&key).await;
                    return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
                }
            }
            if turbo { (StatusCode::ACCEPTED, axum::Json(json!({ "urls": [] }))).into_response() } else { StatusCode::OK.into_response() }
        }
        _ => err(StatusCode::METHOD_NOT_ALLOWED, "GET, HEAD or PUT"),
    }
}

impl Apps {
    /// What a build gets for the cache `ns` ([`scope`]) of `project`: its variables (the token
    /// among them, to hide in its log), or none on a node without object storage or a public base.
    pub async fn build_cache_vars(self: &Arc<Self>, project: &str, ns: &str) -> crate::Result<Vec<(String, String)>> {
        let (Some(base), Some(_)) = (&self.cfg.otlp_base, &self.deps.objects) else { return Ok(Vec::new()) };
        let (p, n) = (project.to_string(), ns.to_string());
        let token = self.db(move |s| s.build_cache_token(&p, &n)).await?;
        let url = format!("{}{PATH}", base.trim_end_matches('/'));
        Ok(vec![
            ("TURBO_API".into(), url.clone()),
            ("TURBO_TOKEN".into(), token.clone()),
            ("TURBO_TEAM".into(), "roll".into()),
            ("NX_SELF_HOSTED_REMOTE_CACHE_SERVER".into(), url),
            ("NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN".into(), token),
        ])
    }

    /// Artifacts unused for [`KEEP_DAYS`] deleted (their rows first, in one write: a read never
    /// finds a row without its object, and no upload's object is another's).
    /// What each cache holds is metered too, a GB-hour each hour (`stored_gb_hours`), shared by the
    /// services that build from it ([`scope`]), each node for the services it owns and each hour
    /// once (so a fleet, a restart or a takeover counts it once).
    pub(crate) async fn build_cache_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        let Some(objects) = self.deps.objects.clone() else { return };
        let Ok(gone) = self.db(move |s| s.expire_artifacts(now - KEEP_DAYS * 86_400_000)).await else { return };
        for o in gone {
            let _ = objects.delete(&o).await;
        }
        let Ok((held, envs)) = self.db(|s| Ok((s.cache_bytes()?, s.all_environments()?))).await else { return };
        for (project, ns, bytes) in held {
            let env = ns.split_once('.').map(|(e, _)| e).unwrap_or(&ns);
            let users: Vec<&crate::store::ServiceRow> = services.iter().filter(|s| s.project == project && envs.iter().any(|e| e.service == s.id && e.name == env) && scope(&s.source, &s.id, env) == ns).collect();
            if users.is_empty() {
                continue;
            }
            let share = bytes as f64 / 1e9 / users.len() as f64;
            for svc in users.into_iter().filter(|s| self.owns(&s.id)) {
                let (sid, e) = (svc.id.clone(), env.to_string());
                let _ = self.db(move |s| s.meter_stored(&sid, &e, "cache", now / 3_600_000, share)).await;
            }
        }
    }

    /// Objects no cache row names any more, deleted in the background.
    pub(crate) fn drop_cache_objects(self: &Arc<Self>, gone: Vec<String>) {
        let Some(objects) = self.deps.objects.clone() else { return };
        if gone.is_empty() {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                for o in gone {
                    let _ = objects.delete(&o).await;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_artifacts_hash_is_letters_and_digits() {
        assert!(super::hash_ok("a1b2c3d4e5f60718"));
        for bad in ["", "../x", "a/b", "a.b", &"a".repeat(129)] {
            assert!(!super::hash_ok(bad), "{bad}");
        }
    }
}
