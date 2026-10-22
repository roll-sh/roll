//! Service and bucket operations: buckets, listings, CORS, lifecycle, batch deletes.

use crate::auth::Need;
use crate::ctx::{Ctx, no_content, read_small};
use crate::error::{StoreResult, store_error};
use crate::multipart::{self, encoding_type};
use crate::object::{checksum_of, private_acl};
use crate::response::{self, Reply, set};
use axum::body::Body;
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_proto::checksum::ChecksumType;
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, BucketOptions, BucketUpdate, Class, CorsRule, LifecycleRule, ListQuery, ObjectMeta};

fn owner() -> xml::Owner {
    xml::Owner { id: "roll".into(), display_name: "roll".into() }
}

pub(crate) fn list_buckets(ctx: &mut Ctx) -> Reply {
    let Some(a) = &ctx.auth else { return Err(S3Error::new(Code::AccessDenied)) };
    let buckets = ctx.store().buckets().s3()?;
    let entries: Vec<xml::BucketEntry> = buckets
        .into_iter()
        .filter(|b| a.key.covers(&b.name))
        .map(|b| xml::BucketEntry { name: b.name, created_ms: b.created_at, region: Some(ctx.server.cfg.region.clone()) })
        .collect();
    response::ok_xml(xml::list_buckets(&owner(), &entries))
}

/// Bucket subresources S3 has and we don't.
const UNSUPPORTED: [&str; 22] = [
    "policy",
    "policyStatus",
    "website",
    "replication",
    "logging",
    "notification",
    "encryption",
    "object-lock",
    "ownershipControls",
    "publicAccessBlock",
    "requestPayment",
    "accelerate",
    "analytics",
    "inventory",
    "metrics",
    "intelligent-tiering",
    "versions",
    "metadataTable",
    "metadataConfiguration",
    "session",
    "abac",
    "torrent",
];

pub(crate) async fn route(ctx: &mut Ctx, body: Body) -> Reply {
    let name = ctx.bucket_name().to_string();
    if let Some(a) = &ctx.auth
        && !a.key.covers(&name)
    {
        return Err(S3Error::new(Code::AccessDenied));
    }
    if let Some(sub) = UNSUPPORTED.iter().find(|s| ctx.has(s)) {
        return Err(S3Error::msg(Code::NotImplemented, format!("?{sub} is not supported")));
    }
    if ctx.method == Method::PUT && ctx.query.iter().all(|(k, _)| k.starts_with("X-Amz-") || k.starts_with("x-id")) {
        return create(ctx, body).await;
    }
    let bucket = ctx.store().bucket(&name).s3()?;
    match ctx.method.clone() {
        Method::HEAD => head(ctx, &bucket),
        Method::GET if ctx.has("location") => {
            ctx.require(&name, Need::Read)?;
            ctx.meter(&bucket.id, Class::B);
            response::ok_xml(xml::location_constraint(&ctx.server.cfg.region))
        }
        Method::GET if ctx.has("versioning") => {
            ctx.require(&name, Need::Read)?;
            ctx.meter(&bucket.id, Class::B);
            response::ok_xml(xml::versioning_configuration())
        }
        Method::GET if ctx.has("cors") => get_cors(ctx, &bucket),
        Method::GET if ctx.has("lifecycle") => get_lifecycle(ctx, &bucket),
        Method::GET if ctx.has("uploads") => multipart::list_uploads(ctx, &bucket),
        Method::GET if ctx.has("tagging") => {
            ctx.require(&name, Need::Read)?;
            Err(S3Error::new(Code::NoSuchTagSet))
        }
        Method::GET if ctx.has("acl") => {
            ctx.require(&name, Need::Read)?;
            ctx.meter(&bucket.id, Class::B);
            response::ok_xml(private_acl())
        }
        Method::GET if ctx.q("list-type") == Some("2") => list_v2(ctx, &bucket),
        Method::GET => list_v1(ctx, &bucket),
        Method::PUT if ctx.has("cors") => put_cors(ctx, &bucket, body).await,
        Method::PUT if ctx.has("lifecycle") => put_lifecycle(ctx, &bucket, body).await,
        Method::PUT if ctx.has("versioning") => Err(S3Error::msg(Code::NotImplemented, "Versioning is not supported; deleted and overwritten objects are kept in the bucket's trash.")),
        Method::PUT if ctx.has("tagging") => Err(S3Error::msg(Code::NotImplemented, "Bucket tagging is not supported.")),
        Method::PUT if ctx.has("acl") => {
            ctx.require(&name, Need::Admin)?;
            match ctx.h("x-amz-acl") {
                Some(_) => Ok(response::empty(StatusCode::OK)),
                None => Err(S3Error::msg(Code::NotImplemented, "ACLs are not supported; use the bucket's public flag and scoped keys.")),
            }
        }
        Method::DELETE if ctx.has("cors") => {
            ctx.require(&name, Need::Admin)?;
            ctx.meter(&bucket.id, Class::Free);
            ctx.store().update_bucket(&name, BucketUpdate { cors: Some(Vec::new()), ..Default::default() }).await.s3()?;
            no_content()
        }
        Method::DELETE if ctx.has("lifecycle") => {
            ctx.require(&name, Need::Admin)?;
            ctx.meter(&bucket.id, Class::Free);
            ctx.store().update_bucket(&name, BucketUpdate { lifecycle: Some(Vec::new()), ..Default::default() }).await.s3()?;
            no_content()
        }
        Method::DELETE if ctx.has("tagging") => {
            ctx.require(&name, Need::Admin)?;
            no_content()
        }
        Method::DELETE => {
            ctx.require(&name, Need::Admin)?;
            ctx.meter(&bucket.id, Class::Free);
            ctx.store().delete_bucket(&name, false).await.s3()?;
            no_content()
        }
        Method::POST if ctx.has("delete") => delete_objects(ctx, &bucket, body).await,
        _ => Err(S3Error::new(Code::MethodNotAllowed)),
    }
}

async fn create(ctx: &mut Ctx, body: Body) -> Reply {
    let name = ctx.bucket_name().to_string();
    ctx.require(&name, Need::Admin)?;
    if ctx.h("x-amz-bucket-object-lock-enabled").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return Err(S3Error::msg(Code::NotImplemented, "Object Lock is not supported."));
    }
    if !s3_proto::http::valid_bucket_name(&name) {
        return Err(S3Error::new(Code::InvalidBucketName).with("BucketName", name));
    }
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    xml::parse_create_bucket(&raw)?;
    let b = ctx.store().create_bucket(&name, BucketOptions::default()).await.s3()?;
    ctx.meter(&b.id, Class::A);
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "location", &format!("/{name}"));
    Ok(r)
}

fn head(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "x-amz-bucket-region", &ctx.server.cfg.region);
    Ok(r)
}

fn get_cors(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    if bucket.cors.is_empty() {
        return Err(S3Error::new(Code::NoSuchCORSConfiguration).with("BucketName", bucket.name.clone()));
    }
    let rules: Vec<xml::CorsRule> = bucket
        .cors
        .iter()
        .map(|r| xml::CorsRule {
            id: r.id.clone(),
            allowed_origins: r.allowed_origins.clone(),
            allowed_methods: r.allowed_methods.clone(),
            allowed_headers: r.allowed_headers.clone(),
            expose_headers: r.expose_headers.clone(),
            max_age_s: r.max_age_s,
        })
        .collect();
    response::ok_xml(xml::cors_configuration(&rules))
}

async fn put_cors(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let rules: Vec<CorsRule> = xml::parse_cors(&raw)?
        .into_iter()
        .map(|r| CorsRule { id: r.id, allowed_origins: r.allowed_origins, allowed_methods: r.allowed_methods, allowed_headers: r.allowed_headers, expose_headers: r.expose_headers, max_age_s: r.max_age_s })
        .collect();
    ctx.store().update_bucket(&bucket.name, BucketUpdate { cors: Some(rules), ..Default::default() }).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

fn get_lifecycle(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    if bucket.lifecycle.is_empty() {
        return Err(S3Error::new(Code::NoSuchLifecycleConfiguration).with("BucketName", bucket.name.clone()));
    }
    let rules: Vec<xml::LifecycleRule> = bucket
        .lifecycle
        .iter()
        .map(|r| xml::LifecycleRule { id: Some(r.id.clone()).filter(|s| !s.is_empty()), prefix: r.prefix.clone(), enabled: r.enabled, expire_days: r.expire_days, abort_multipart_days: r.abort_multipart_days })
        .collect();
    response::ok_xml(xml::lifecycle_configuration(&rules))
}

async fn put_lifecycle(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let rules: Vec<LifecycleRule> = xml::parse_lifecycle(&raw)?
        .into_iter()
        .map(|r| LifecycleRule { id: r.id.unwrap_or_default(), prefix: r.prefix, enabled: r.enabled, expire_days: r.expire_days, abort_multipart_days: r.abort_multipart_days })
        .collect();
    ctx.store().update_bucket(&bucket.name, BucketUpdate { lifecycle: Some(rules), ..Default::default() }).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

fn entry(m: &ObjectMeta, with_owner: bool) -> xml::ObjectEntry {
    xml::ObjectEntry {
        key: m.key.clone(),
        last_modified_ms: m.modified_at,
        etag: m.etag.clone(),
        size: m.size,
        storage_class: "STANDARD".into(),
        checksum: m.checksum.as_ref().and_then(|c| Some((checksum_of(c)?, ChecksumType::parse(&c.kind)?))),
        owner: with_owner.then(owner),
    }
}

fn max_keys(ctx: &Ctx) -> Result<u32, S3Error> {
    match ctx.q("max-keys") {
        None => Ok(1000),
        Some(v) => v.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| n.min(1000) as u32).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Provided max-keys not an integer or within integer range").with("ArgumentName", "max-keys")),
    }
}

fn delimiter(ctx: &Ctx) -> Option<String> {
    ctx.q("delimiter").filter(|d| !d.is_empty()).map(str::to_string)
}

fn list_v2(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::A);
    let url_encode = encoding_type(ctx)?;
    let max = max_keys(ctx)?;
    let token = ctx.q("continuation-token").map(str::to_string);
    let from_token = match &token {
        Some(t) => Some(
            URL_SAFE_NO_PAD
                .decode(t)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| S3Error::msg(Code::InvalidArgument, "The continuation token provided is incorrect").with("ArgumentName", "continuation-token"))?,
        ),
        None => None,
    };
    let start_after = ctx.q("start-after").filter(|s| !s.is_empty()).map(str::to_string);
    let q = ListQuery { prefix: ctx.q("prefix").unwrap_or_default().to_string(), delimiter: delimiter(ctx), start_after: from_token.or_else(|| start_after.clone()), max_keys: max as usize };
    let page = ctx.store().list_objects(&bucket.name, &q).s3()?;
    let fetch_owner = ctx.q("fetch-owner") == Some("true");
    let l = xml::ListV2 {
        name: bucket.name.clone(),
        prefix: q.prefix.clone(),
        delimiter: q.delimiter.clone(),
        max_keys: max,
        is_truncated: page.truncated,
        continuation_token: token,
        next_continuation_token: page.next.as_ref().filter(|_| page.truncated).map(|n| URL_SAFE_NO_PAD.encode(n.as_bytes())),
        start_after,
        url_encode,
        contents: page.objects.iter().map(|m| entry(m, fetch_owner)).collect(),
        common_prefixes: page.prefixes.clone(),
    };
    response::ok_xml(xml::list_objects_v2(&l))
}

fn list_v1(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::A);
    let url_encode = encoding_type(ctx)?;
    let max = max_keys(ctx)?;
    let marker = ctx.q("marker").unwrap_or_default().to_string();
    let q = ListQuery { prefix: ctx.q("prefix").unwrap_or_default().to_string(), delimiter: delimiter(ctx), start_after: Some(marker.clone()).filter(|m| !m.is_empty()), max_keys: max as usize };
    let page = ctx.store().list_objects(&bucket.name, &q).s3()?;
    let l = xml::ListV1 {
        name: bucket.name.clone(),
        prefix: q.prefix.clone(),
        marker,
        next_marker: page.next.clone().filter(|_| page.truncated && q.delimiter.is_some()),
        delimiter: q.delimiter.clone(),
        max_keys: max,
        is_truncated: page.truncated,
        url_encode,
        contents: page.objects.iter().map(|m| entry(m, true)).collect(),
        common_prefixes: page.prefixes.clone(),
    };
    response::ok_xml(xml::list_objects_v1(&l))
}

async fn delete_objects(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::Free);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let req = xml::parse_delete(&raw)?;
    let results = ctx.store().delete_objects(&bucket.name, &req.keys).await.s3()?;
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for (key, r) in results {
        match r {
            Ok(_) => deleted.push(key),
            Err(e) => {
                let e = store_error(e);
                errors.push(xml::DeleteError { key, code: e.code, message: e.message });
            }
        }
    }
    response::ok_xml(xml::delete_result(&deleted, &errors, req.quiet))
}
