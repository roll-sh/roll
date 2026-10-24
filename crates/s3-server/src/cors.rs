//! CORS: preflight requests and the headers on actual requests, from the bucket's rules.

use crate::ctx::Ctx;
use crate::response::{self, Reply, set};
use axum::http::StatusCode;
use axum::response::Response;
use s3_proto::{Code, S3Error};
use s3_store::CorsRule;

/// `pattern` with at most one `*` wildcard, case-insensitive.
fn wildcard(pattern: &str, value: &str) -> bool {
    let (p, v) = (pattern.to_ascii_lowercase(), value.to_ascii_lowercase());
    match p.split_once('*') {
        None => p == v,
        Some((pre, suf)) => v.len() >= pre.len() + suf.len() && v.starts_with(pre) && v.ends_with(suf),
    }
}

fn matches<'a>(rules: &'a [CorsRule], origin: &str, method: &str, headers: &[String]) -> Option<&'a CorsRule> {
    rules.iter().find(|r| {
        r.allowed_origins.iter().any(|o| wildcard(o, origin))
            && r.allowed_methods.iter().any(|m| m.eq_ignore_ascii_case(method))
            && headers.iter().all(|h| r.allowed_headers.iter().any(|a| wildcard(a, h)))
    })
}

fn apply(r: &mut Response, rule: &CorsRule, origin: &str) {
    let any = rule.allowed_origins.iter().any(|o| o == "*");
    set(r, "access-control-allow-origin", if any { "*" } else { origin });
    if !any {
        set(r, "access-control-allow-credentials", "true");
    }
    set(r, "access-control-allow-methods", &rule.allowed_methods.join(", "));
    if !rule.expose_headers.is_empty() {
        set(r, "access-control-expose-headers", &rule.expose_headers.join(", "));
    }
    if let Some(a) = rule.max_age_s {
        set(r, "access-control-max-age", &a.to_string());
    }
    set(r, "vary", "Origin, Access-Control-Request-Headers, Access-Control-Request-Method");
}

/// `OPTIONS`: allowed if one of the bucket's rules matches the origin, method and headers.
pub(crate) fn preflight(ctx: &Ctx) -> Reply {
    let forbidden = |m: &str| S3Error::msg(Code::AccessForbidden, m.to_string());
    let origin = ctx.h("origin").ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Insufficient information. Origin request header needed."))?;
    let method = ctx.h("access-control-request-method").ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Invalid Access-Control-Request-Method: null"))?;
    let Some(name) = &ctx.bucket else { return Err(forbidden("CORSResponse: Bucket not found")) };
    let bucket = ctx.store().bucket(name).map_err(|_| forbidden("CORSResponse: Bucket not found"))?;
    if bucket.cors.is_empty() {
        return Err(forbidden("CORSResponse: CORS is not enabled for this bucket."));
    }
    let headers: Vec<String> = ctx.h("access-control-request-headers").map(|h| h.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let rule = matches(&bucket.cors, origin, method, &headers).ok_or_else(|| S3Error::new(Code::AccessForbidden))?;
    let mut r = response::empty(StatusCode::OK);
    apply(&mut r, rule, origin);
    if !headers.is_empty() {
        set(&mut r, "access-control-allow-headers", &headers.join(", "));
    }
    Ok(r)
}

/// CORS headers on an actual request from a browser (`Origin` set), when a rule allows it.
pub(crate) fn decorate(ctx: &Ctx, r: &mut Response) {
    let (Some(origin), Some(name)) = (ctx.h("origin"), &ctx.bucket) else { return };
    let Ok(bucket) = ctx.store().bucket(name) else { return };
    if let Some(rule) = matches(&bucket.cors, origin, ctx.method.as_str(), &[]) {
        apply(r, rule, origin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "https://a.example"));
        assert!(wildcard("https://*.example.com", "https://app.example.com"));
        assert!(!wildcard("https://*.example.com", "https://example.org"));
        assert!(wildcard("x-amz-*", "X-Amz-Date"));
        assert!(wildcard("Content-Type", "content-type"));
        assert!(!wildcard("https://a.example", "https://b.example"));
    }

    #[test]
    fn rule_matching() {
        let rules = vec![CorsRule { allowed_origins: vec!["https://app.example".into()], allowed_methods: vec!["GET".into(), "PUT".into()], allowed_headers: vec!["*".into()], ..Default::default() }];
        assert!(matches(&rules, "https://app.example", "PUT", &["content-type".into()]).is_some());
        assert!(matches(&rules, "https://app.example", "DELETE", &[]).is_none());
        assert!(matches(&rules, "https://evil.example", "GET", &[]).is_none());
    }
}
