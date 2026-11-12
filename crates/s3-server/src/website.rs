//! Static website hosting: Get/Put/DeleteBucketWebsite, and the website endpoint.
//!
//! With `Config::website_domain` set, a request whose `Host` is `<bucket>.<website_domain>`
//! gets S3's website semantics instead of the REST API:
//!
//! - `GET` and `HEAD` only (else `405`), always anonymous (signatures are ignored).
//! - Objects are readable only if anonymous users may `GetObject` them: the bucket's `public`
//!   flag, its policy or ACLs, within its public access block.
//! - `RedirectAllRequestsTo` sends every request to the other host (`301`).
//! - Routing rules with only a `KeyPrefixEquals` condition redirect before the lookup; rules
//!   with `HttpErrorCodeReturnedEquals` redirect when the lookup fails with that status.
//! - `/` and `dir/` serve the index document; `dir` with no object but a `dir/<index>` gets a
//!   `302` to `dir/`.
//! - An object with `x-amz-website-redirect-location` answers `301` to it.
//! - Errors serve the `ErrorDocument` with the error's status, or S3's HTML error page.
//!   Missing keys are `404` (S3 answers `403` unless the bucket also grants `s3:ListBucket`).

use crate::config::{Doc, malformed, read_xml, reading, save, writing};
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::{self, Reply, set};
use crate::{Server, object};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use s3_proto::http::uri_encode;
use s3_proto::xml::{Node, escape};
use s3_proto::{Code, S3Error};
use s3_store::config::kind;
use s3_store::{Bucket, Class, RedirectAll, Redirect, RoutingCondition, RoutingRule, WebsiteConfig};

const MAX_ROUTING_RULES: usize = 50;

/// The bucket a website-endpoint `Host` names, if any.
pub(crate) fn host_bucket(server: &Server, headers: &HeaderMap) -> Option<String> {
    let domain = server.cfg.website_domain.as_deref()?.to_ascii_lowercase();
    let host = headers.get("host").and_then(|h| h.to_str().ok())?.to_ascii_lowercase();
    let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|c| c.is_ascii_digit())).map_or(host.as_str(), |(h, _)| h);
    host.strip_suffix(domain.as_str()).and_then(|h| h.strip_suffix('.')).filter(|b| !b.is_empty()).map(str::to_string)
}

// ---- Configuration ----

fn text(n: &Node, name: &str) -> Option<String> {
    n.text_of(name).map(|s| s.trim().to_string())
}

fn invalid(m: &str) -> S3Error {
    S3Error::msg(Code::InvalidArgument, m.to_string())
}

fn protocol(n: &Node) -> Result<Option<String>, S3Error> {
    match text(n, "Protocol") {
        None => Ok(None),
        Some(p) if p == "http" || p == "https" => Ok(Some(p)),
        Some(_) => Err(invalid("Invalid protocol, protocol can be http or https. If not defined the protocol will be selected automatically.")),
    }
}

pub(crate) fn parse_config(n: &Node) -> Result<WebsiteConfig, S3Error> {
    let mut c = WebsiteConfig::default();
    if let Some(r) = n.child("RedirectAllRequestsTo") {
        let host_name = text(r, "HostName").filter(|h| !h.is_empty()).ok_or_else(malformed)?;
        if n.children.len() > 1 {
            return Err(invalid("RedirectAllRequestsTo cannot be provided in conjunction with other Routing Rules."));
        }
        c.redirect_all = Some(RedirectAll { host_name, protocol: protocol(r)? });
        return Ok(c);
    }
    let suffix = n.child("IndexDocument").and_then(|i| text(i, "Suffix")).ok_or_else(|| invalid("A value for IndexDocument Suffix must be provided if RedirectAllRequestsTo is empty"))?;
    if suffix.is_empty() || suffix.contains('/') {
        return Err(invalid("The IndexDocument Suffix is not well formed"));
    }
    c.index_document = Some(suffix);
    if let Some(e) = n.child("ErrorDocument") {
        c.error_document = Some(text(e, "Key").filter(|k| !k.is_empty()).ok_or_else(|| invalid("The ErrorDocument Key is not well formed"))?);
    }
    if let Some(rules) = n.child("RoutingRules") {
        for r in rules.all("RoutingRule") {
            if c.routing_rules.len() >= MAX_ROUTING_RULES {
                return Err(invalid("The number of routing rules must not exceed 50"));
            }
            let condition = match r.child("Condition") {
                Some(cn) => {
                    let code = match text(cn, "HttpErrorCodeReturnedEquals") {
                        Some(s) => Some(s.parse::<u16>().ok().filter(|c| (400..600).contains(c)).ok_or_else(|| invalid("The provided HTTP error code is not valid. Valid codes are 4XX or 5XX."))?),
                        None => None,
                    };
                    let prefix = text(cn, "KeyPrefixEquals");
                    if code.is_none() && prefix.is_none() {
                        return Err(invalid("Condition cannot be empty. To redirect all requests without a condition, the condition element shouldn't be present."));
                    }
                    Some(RoutingCondition { key_prefix_equals: prefix, http_error_code_returned_equals: code })
                }
                None => None,
            };
            let rd = r.child("Redirect").ok_or_else(|| invalid("Redirect is required in a RoutingRule"))?;
            let redirect = Redirect {
                host_name: text(rd, "HostName").filter(|h| !h.is_empty()),
                protocol: protocol(rd)?,
                http_redirect_code: match text(rd, "HttpRedirectCode") {
                    Some(s) => Some(s.parse::<u16>().ok().filter(|c| (300..400).contains(c) && *c != 300 && *c != 304).ok_or_else(|| invalid("The provided HTTP redirect code is not valid. It should be a string containing a number."))?),
                    None => None,
                },
                replace_key_prefix_with: text(rd, "ReplaceKeyPrefixWith"),
                replace_key_with: text(rd, "ReplaceKeyWith"),
            };
            if redirect.replace_key_prefix_with.is_some() && redirect.replace_key_with.is_some() {
                return Err(invalid("You can only define ReplaceKeyPrefix or ReplaceKey but not both."));
            }
            if redirect == Redirect::default() {
                return Err(invalid("Redirect must contain at least one of the following elements: HostName, Protocol, HttpRedirectCode, ReplaceKeyPrefixWith or ReplaceKeyWith"));
            }
            c.routing_rules.push(RoutingRule { condition, redirect });
        }
    }
    Ok(c)
}

pub(crate) fn render_config(c: &WebsiteConfig) -> String {
    let mut d = Doc::new("WebsiteConfiguration");
    if let Some(r) = &c.redirect_all {
        d.open("RedirectAllRequestsTo").el("HostName", &r.host_name).opt("Protocol", r.protocol.as_deref()).close("RedirectAllRequestsTo");
    }
    if let Some(s) = &c.index_document {
        d.open("IndexDocument").el("Suffix", s).close("IndexDocument");
    }
    if let Some(k) = &c.error_document {
        d.open("ErrorDocument").el("Key", k).close("ErrorDocument");
    }
    if !c.routing_rules.is_empty() {
        d.open("RoutingRules");
        for r in &c.routing_rules {
            d.open("RoutingRule");
            if let Some(cn) = &r.condition {
                d.open("Condition");
                d.opt("HttpErrorCodeReturnedEquals", cn.http_error_code_returned_equals.map(|c| c.to_string()).as_deref());
                d.opt("KeyPrefixEquals", cn.key_prefix_equals.as_deref());
                d.close("Condition");
            }
            let rd = &r.redirect;
            d.open("Redirect")
                .opt("HostName", rd.host_name.as_deref())
                .opt("HttpRedirectCode", rd.http_redirect_code.map(|c| c.to_string()).as_deref())
                .opt("Protocol", rd.protocol.as_deref())
                .opt("ReplaceKeyPrefixWith", rd.replace_key_prefix_with.as_deref())
                .opt("ReplaceKeyWith", rd.replace_key_with.as_deref())
                .close("Redirect");
            d.close("RoutingRule");
        }
        d.close("RoutingRules");
    }
    d.finish("WebsiteConfiguration")
}

pub(crate) fn get_config(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let c = ctx.store().bucket_website(&bucket.name).s3()?.ok_or_else(|| S3Error::new(Code::NoSuchWebsiteConfiguration).with("BucketName", bucket.name.clone()))?;
    response::ok_xml(render_config(&c))
}

pub(crate) async fn put_config(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "WebsiteConfiguration").await?;
    let c = parse_config(&n)?;
    let v = serde_json::to_string(&c).map_err(|_| S3Error::new(Code::InternalError))?;
    save(ctx, bucket, kind::WEBSITE, Some(&v)).await?;
    Ok(response::empty(StatusCode::OK))
}

/// Validate `x-amz-website-redirect-location` (stored with the object).
pub(crate) fn redirect_location(ctx: &Ctx) -> Result<Option<String>, S3Error> {
    ctx.h("x-amz-website-redirect-location").map(check_redirect).transpose()
}

/// A website redirect location: a path (`/…`) or an `http(s)://` URL, at most 2 KB.
pub(crate) fn check_redirect(v: &str) -> Result<String, S3Error> {
    if !(v.starts_with('/') || v.starts_with("http://") || v.starts_with("https://")) || v.len() > 2048 {
        return Err(S3Error::new(Code::InvalidRedirectLocation));
    }
    Ok(v.to_string())
}

// ---- Serving ----

fn reason(status: u16) -> &'static str {
    StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Error")
}

/// S3's HTML error page.
fn error_page(e: &S3Error, key: Option<&str>, doc_error: Option<(&S3Error, &str)>, head: bool) -> Response {
    let status = e.status();
    let title = format!("{status} {}", reason(status));
    let mut items = format!("<li>Code: {}</li>\n<li>Message: {}</li>\n", e.code.as_str(), escape(&e.message));
    if let Some(k) = key {
        items.push_str(&format!("<li>Key: {}</li>\n", escape(k)));
    }
    for (name, v) in &e.extra {
        if *name == "BucketName" {
            items.push_str(&format!("<li>BucketName: {}</li>\n", escape(v)));
        }
    }
    let mut html = format!("<html>\n<head><title>{title}</title></head>\n<body>\n<h1>{title}</h1>\n<ul>\n{items}</ul>\n");
    if let Some((de, dk)) = doc_error {
        html.push_str(&format!(
            "<h3>An Error Occurred While Attempting to Retrieve a Custom Error Document</h3>\n<ul>\n<li>Code: {}</li>\n<li>Message: {}</li>\n<li>Key: {}</li>\n</ul>\n",
            de.code.as_str(),
            escape(&de.message),
            escape(dk)
        ));
    }
    html.push_str("<hr/>\n</body>\n</html>\n");
    let mut r = Response::new(if head { Body::empty() } else { Body::from(html) });
    *r.status_mut() = response::status(status);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    set(&mut r, "x-amz-error-code", e.code.as_str());
    set(&mut r, "x-amz-error-message", &e.message);
    r
}

fn redirect(code: u16, location: &str) -> Response {
    let mut r = response::empty(response::status(code));
    set(&mut r, "location", location);
    r
}

fn rule_prefix_matches(c: &Option<RoutingCondition>, key: &str) -> bool {
    c.as_ref().and_then(|c| c.key_prefix_equals.as_deref()).is_none_or(|p| key.starts_with(p))
}

/// Where a routing rule sends `key`.
fn rule_redirect(ctx: &Ctx, rule: &RoutingRule, key: &str) -> Response {
    let r = &rule.redirect;
    let proto = r.protocol.clone().unwrap_or_else(|| if crate::access::secure(ctx) { "https".into() } else { "http".into() });
    let host = r.host_name.clone().unwrap_or_else(|| ctx.h("host").unwrap_or_default().to_string());
    let new_key = if let Some(k) = &r.replace_key_with {
        k.clone()
    } else if let Some(p) = &r.replace_key_prefix_with {
        let matched = rule.condition.as_ref().and_then(|c| c.key_prefix_equals.as_deref()).unwrap_or("");
        format!("{p}{}", key.strip_prefix(matched).unwrap_or(key))
    } else {
        key.to_string()
    };
    redirect(r.http_redirect_code.unwrap_or(301), &format!("{proto}://{host}/{}", uri_encode(&new_key, false)))
}

/// Serve `key` anonymously: the object, its website redirect, or the error.
async fn try_object(ctx: &mut Ctx, bucket: &Bucket, key: &str, head: bool) -> Result<Response, S3Error> {
    ctx.key = Some(key.to_string());
    ctx.require_object_read(bucket)?;
    let meta = ctx.store().get_object(&bucket.name, key).s3()?;
    if let Some(loc) = &meta.headers.website_redirect {
        ctx.meter(&bucket.id, Class::B);
        return Ok(redirect(301, loc));
    }
    object::get(ctx, bucket, head).await
}

/// A request to the website endpoint (`ctx.bucket` from the host, `ctx.key` the path).
pub(crate) async fn serve(ctx: &mut Ctx) -> Response {
    ctx.auth = None;
    let head = ctx.method == Method::HEAD;
    let path_key = ctx.key.clone().unwrap_or_default();
    if ctx.method != Method::GET && !head {
        let mut r = error_page(&S3Error::new(Code::MethodNotAllowed), None, None, head);
        set(&mut r, "allow", "GET, HEAD");
        return r;
    }
    let name = ctx.bucket_name().to_string();
    let bucket = match ctx.store().bucket(&name) {
        Ok(b) => b,
        Err(e) => return error_page(&crate::error::store_error(e), None, None, head),
    };
    let cfg = match ctx.store().bucket_website(&name) {
        Ok(Some(c)) => c,
        Ok(None) => return error_page(&S3Error::new(Code::NoSuchWebsiteConfiguration).with("BucketName", name), None, None, head),
        Err(e) => return error_page(&crate::error::store_error(e), None, None, head),
    };
    if let Some(r) = &cfg.redirect_all {
        ctx.meter(&bucket.id, Class::B);
        let proto = r.protocol.clone().unwrap_or_else(|| if crate::access::secure(ctx) { "https".into() } else { "http".into() });
        return redirect(301, &format!("{proto}://{}/{}", r.host_name, uri_encode(&path_key, false)));
    }
    if let Some(rule) = cfg.routing_rules.iter().find(|r| r.condition.as_ref().is_none_or(|c| c.http_error_code_returned_equals.is_none()) && rule_prefix_matches(&r.condition, &path_key)) {
        ctx.meter(&bucket.id, Class::B);
        return rule_redirect(ctx, rule, &path_key);
    }
    let index = cfg.index_document.clone().unwrap_or_else(|| "index.html".into());
    let key = if path_key.is_empty() || path_key.ends_with('/') { format!("{path_key}{index}") } else { path_key.clone() };
    let err = match try_object(ctx, &bucket, &key, head).await {
        Ok(r) => return r,
        Err(e) => e,
    };
    if err.code == Code::NoSuchKey && key == path_key {
        // `dir` → `dir/` when that has an index document.
        let dir_index = format!("{key}/{index}");
        ctx.key = Some(dir_index.clone());
        if ctx.require_object_read(&bucket).is_ok() && ctx.store().get_object(&name, &dir_index).is_ok() {
            ctx.meter(&bucket.id, Class::B);
            return redirect(302, &format!("/{}/", uri_encode(&key, false)));
        }
    }
    let status = err.status();
    if let Some(rule) = cfg.routing_rules.iter().find(|r| r.condition.as_ref().and_then(|c| c.http_error_code_returned_equals) == Some(status) && rule_prefix_matches(&r.condition, &path_key)) {
        return rule_redirect(ctx, rule, &path_key);
    }
    let shown_key = if err.code == Code::NoSuchKey { Some(key.as_str()) } else { None };
    if let Some(doc) = cfg.error_document.clone().filter(|_| (400..500).contains(&status)) {
        match try_object(ctx, &bucket, &doc, head).await {
            Ok(mut r) if r.status().is_success() => {
                *r.status_mut() = response::status(status);
                r.headers_mut().remove("content-range");
                return r;
            }
            Ok(_) => return error_page(&err, shown_key, None, head),
            Err(de) => return error_page(&err, shown_key, Some((&de, &doc)), head),
        }
    }
    error_page(&err, shown_key, None, head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(x: &str) -> Result<WebsiteConfig, S3Error> {
        parse_config(&s3_proto::xml::parse(x.as_bytes()).unwrap())
    }

    #[test]
    fn configs() {
        let c = parse(
            "<WebsiteConfiguration><IndexDocument><Suffix>index.html</Suffix></IndexDocument><ErrorDocument><Key>404.html</Key></ErrorDocument>\
             <RoutingRules><RoutingRule><Condition><KeyPrefixEquals>docs/</KeyPrefixEquals></Condition><Redirect><ReplaceKeyPrefixWith>documents/</ReplaceKeyPrefixWith></Redirect></RoutingRule>\
             <RoutingRule><Condition><HttpErrorCodeReturnedEquals>404</HttpErrorCodeReturnedEquals></Condition><Redirect><HostName>example.com</HostName><HttpRedirectCode>302</HttpRedirectCode></Redirect></RoutingRule></RoutingRules></WebsiteConfiguration>",
        )
        .unwrap();
        assert_eq!(c.index_document.as_deref(), Some("index.html"));
        assert_eq!(c.error_document.as_deref(), Some("404.html"));
        assert_eq!(c.routing_rules.len(), 2);
        assert_eq!(c.routing_rules[1].redirect.http_redirect_code, Some(302));
        let back = parse_config(&s3_proto::xml::parse(render_config(&c).as_bytes()).unwrap()).unwrap();
        assert_eq!(back, c);
        let r = parse("<WebsiteConfiguration><RedirectAllRequestsTo><HostName>example.com</HostName><Protocol>https</Protocol></RedirectAllRequestsTo></WebsiteConfiguration>").unwrap();
        assert_eq!(r.redirect_all.unwrap().protocol.as_deref(), Some("https"));
        for bad in [
            "<WebsiteConfiguration/>",
            "<WebsiteConfiguration><IndexDocument><Suffix>a/b</Suffix></IndexDocument></WebsiteConfiguration>",
            "<WebsiteConfiguration><IndexDocument><Suffix></Suffix></IndexDocument></WebsiteConfiguration>",
            "<WebsiteConfiguration><RedirectAllRequestsTo><HostName>x</HostName></RedirectAllRequestsTo><IndexDocument><Suffix>i</Suffix></IndexDocument></WebsiteConfiguration>",
            "<WebsiteConfiguration><RedirectAllRequestsTo><HostName>x</HostName><Protocol>ftp</Protocol></RedirectAllRequestsTo></WebsiteConfiguration>",
            "<WebsiteConfiguration><IndexDocument><Suffix>i</Suffix></IndexDocument><RoutingRules><RoutingRule><Redirect><ReplaceKeyWith>a</ReplaceKeyWith><ReplaceKeyPrefixWith>b</ReplaceKeyPrefixWith></Redirect></RoutingRule></RoutingRules></WebsiteConfiguration>",
            "<WebsiteConfiguration><IndexDocument><Suffix>i</Suffix></IndexDocument><RoutingRules><RoutingRule><Redirect/></RoutingRule></RoutingRules></WebsiteConfiguration>",
            "<WebsiteConfiguration><IndexDocument><Suffix>i</Suffix></IndexDocument><RoutingRules><RoutingRule><Condition><HttpErrorCodeReturnedEquals>200</HttpErrorCodeReturnedEquals></Condition><Redirect><HostName>x</HostName></Redirect></RoutingRule></RoutingRules></WebsiteConfiguration>",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hosts() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let store = s3_store::Store::open(s3_store::StoreConfig::new(dir.path().join("m")).manual(), s3_store::test_backend("website-hosts").await).await.unwrap();
            let s = Server::new(store, crate::Config { website_domain: Some("Web.Example".into()), ..Default::default() });
            let h = |host: &str| {
                let mut m = HeaderMap::new();
                m.insert("host", host.parse().unwrap());
                host_bucket(&s, &m)
            };
            assert_eq!(h("site.web.example"), Some("site".into()));
            assert_eq!(h("my.site.web.example:8080"), Some("my.site".into()));
            assert_eq!(h("web.example"), None);
            assert_eq!(h("site.s3.example"), None);
        });
    }
}
