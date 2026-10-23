//! Image transformations: `GET /render/image/{public|authenticated|sign}/{bucket}/{key}?width=
//! &height=&resize=cover|contain|fill&quality=&format=origin|avif|webp`.
//!
//! The routes are reserved and their parameters checked; the transform itself answers `501`
//! until the image engine (`s3-image`) is wired in: replace [`render_image`]'s body. Everything
//! it needs is in [`RenderRequest`]: who may read is decided the way the object routes decide it
//! (public: the bucket's public flag or policy; authenticated: the caller's rules; sign: the
//! token, whose `transformations` claim must match).

use crate::error::{ApiError, ApiResult};
use crate::req::{Req, object_path};
use crate::{Api, auth};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;
use std::sync::Arc;

/// How a render request is authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    Public,
    Authenticated,
    Signed,
}

/// `resize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resize {
    Cover,
    Contain,
    Fill,
}

/// `format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// The source's format.
    Origin,
    Avif,
    Webp,
}

/// Transform options (Supabase's: width and height 1–2500 px, quality 20–100).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransformOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub resize: Option<Resize>,
    pub quality: Option<u8>,
    pub format: Option<Format>,
}

impl TransformOptions {
    /// Parse and check query parameters.
    pub fn parse(q: &[(String, String)]) -> Result<Self, String> {
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty());
        let int = |k: &str, lo: u32, hi: u32| -> Result<Option<u32>, String> {
            match get(k) {
                None => Ok(None),
                Some(v) => match v.parse::<u32>() {
                    Ok(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
                    _ => Err(format!("{k} must be an integer from {lo} to {hi}")),
                },
            }
        };
        Ok(Self {
            width: int("width", 1, 2500)?,
            height: int("height", 1, 2500)?,
            quality: int("quality", 20, 100)?.map(|n| n as u8),
            resize: match get("resize") {
                None => None,
                Some("cover") => Some(Resize::Cover),
                Some("contain") => Some(Resize::Contain),
                Some("fill") => Some(Resize::Fill),
                Some(o) => return Err(format!("resize must be cover, contain or fill (got {o})")),
            },
            format: match get("format") {
                None => None,
                Some("origin") => Some(Format::Origin),
                Some("avif") => Some(Format::Avif),
                Some("webp") => Some(Format::Webp),
                Some(o) => return Err(format!("format must be origin, avif or webp (got {o})")),
            },
        })
    }
}

/// One render request.
pub struct RenderRequest {
    pub mode: RenderMode,
    pub bucket: String,
    pub key: String,
    pub options: TransformOptions,
    /// The whole query (`token`, `download`, …).
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub peer: Option<IpAddr>,
    /// The caller (anonymous for public and signed renders).
    pub caller: s3_server::app::AppCaller,
}

/// Render an image. Not implemented yet: the lead wires the image engine in here.
pub async fn render_image(_api: &Arc<Api>, _req: RenderRequest) -> Result<Response, ApiError> {
    Err(ApiError::not_implemented("Image transformations aren't available yet").real())
}

pub(crate) async fn route(r: &Req, m: &Method, rest: &str) -> Response {
    match prepare(r, m, rest).await {
        Ok(req) => match render_image(&r.api, req).await {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

async fn prepare(r: &Req, m: &Method, rest: &str) -> ApiResult<RenderRequest> {
    if *m != Method::GET {
        return Err(crate::req::method_not_allowed(m));
    }
    let (mode, path) = if let Some(p) = rest.strip_prefix("public/") {
        (RenderMode::Public, p)
    } else if let Some(p) = rest.strip_prefix("authenticated/") {
        (RenderMode::Authenticated, p)
    } else if let Some(p) = rest.strip_prefix("sign/") {
        (RenderMode::Signed, p)
    } else {
        return Err(ApiError::new(404, "NotFound", "Not Found", "Route not found"));
    };
    let (bucket, key) = object_path(path)?;
    let options = TransformOptions::parse(&r.query).map_err(ApiError::invalid)?;
    let caller = match mode {
        RenderMode::Authenticated => auth::required(r).await?,
        _ => s3_server::app::AppCaller::anonymous(),
    };
    if mode == RenderMode::Signed {
        let token = r.q("token").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::missing("token"))?;
        crate::sign::verify_token(r, token, &format!("{bucket}/{key}"), crate::sign::DOWNLOAD).await?;
    }
    Ok(RenderRequest { mode, bucket, key, options, query: r.query.clone(), headers: r.headers.clone(), peer: r.peer, caller })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let q = |s: &str| s3_proto::http::parse_query(s);
        assert_eq!(TransformOptions::parse(&q("width=100&resize=cover&format=webp&quality=80")).unwrap(), TransformOptions { width: Some(100), height: None, resize: Some(Resize::Cover), quality: Some(80), format: Some(Format::Webp) });
        assert!(TransformOptions::parse(&q("width=0")).is_err());
        assert!(TransformOptions::parse(&q("resize=stretch")).is_err());
        assert!(TransformOptions::parse(&q("quality=5")).is_err());
        assert_eq!(TransformOptions::parse(&q("")).unwrap(), TransformOptions::default());
    }
}
