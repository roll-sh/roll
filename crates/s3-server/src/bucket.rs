//! Service and bucket operations: buckets, listings, CORS, lifecycle, batch deletes.

use crate::auth::Need;
use crate::ctx::{Ctx, no_content, read_small};
use crate::error::StoreResult;
use crate::multipart::{self, encoding_type};
use crate::acl;
use crate::object::checksum_of;
use crate::response::{self, Reply, set};
use crate::versioning;
use axum::body::Body;
use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_proto::checksum::ChecksumType;
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, BucketOptions, BucketUpdate, Class, CorsRule, LifecycleRule, ListQuery, ObjectMeta};

pub(crate) fn list_buckets(ctx: &mut Ctx) -> Reply {
    let Some(a) = &ctx.auth else { return Err(S3Error::new(Code::AccessDenied)) };
    ctx.require_session("s3:ListAllMyBuckets", None, None)?;
    // Pages (`max-buckets`, `continuation-token`), `prefix` and `bucket-region`, as S3 has
    // since 2024. Buckets come in name order; the token is the last name of the page.
    let invalid = |name: &str, msg: &str| S3Error::msg(Code::InvalidArgument, msg.to_string()).with("ArgumentName", name.to_string());
    let max = match ctx.q("max-buckets") {
        None => None,
        Some(v) => Some(v.parse::<usize>().ok().filter(|n| (1..=10_000).contains(n)).ok_or_else(|| invalid("max-buckets", "max-buckets must be an integer between 1 and 10000"))?),
    };
    let after = match ctx.q("continuation-token") {
        None => None,
        Some(t) => Some(URL_SAFE_NO_PAD.decode(t).ok().and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| invalid("continuation-token", "The continuation token provided is incorrect"))?),
    };
    let prefix = ctx.q("prefix").filter(|p| !p.is_empty()).map(str::to_string);
    let region = ctx.server.cfg.region.clone();
    if ctx.q("bucket-region").is_some_and(|r| r != region) {
        return response::ok_xml(xml::list_buckets_page(&acl::owner(ctx.store()), &[], None, prefix.as_deref()));
    }
    let mut entries: Vec<xml::BucketEntry> = ctx
        .store()
        .buckets()
        .s3()?
        .into_iter()
        .filter(|b| a.key.covers(&b.name))
        .filter(|b| prefix.as_ref().is_none_or(|p| b.name.starts_with(p.as_str())))
        .filter(|b| after.as_ref().is_none_or(|t| b.name.as_str() > t.as_str()))
        .map(|b| xml::BucketEntry { name: b.name, created_ms: b.created_at, region: Some(region.clone()) })
        .collect();
    let next = match max {
        Some(n) if entries.len() > n => {
            entries.truncate(n);
            entries.last().map(|b| URL_SAFE_NO_PAD.encode(b.name.as_bytes()))
        }
        _ => None,
    };
    response::ok_xml(xml::list_buckets_page(&acl::owner(ctx.store()), &entries, next.as_deref(), prefix.as_deref()))
}

/// Bucket subresources S3 has and we don't.
const UNSUPPORTED: [&str; 5] = ["metadataTable", "metadataConfiguration", "session", "abac", "torrent"];

pub(crate) async fn route(ctx: &mut Ctx, body: Body) -> Reply {
    let name = ctx.bucket_name().to_string();
    if let Some(sub) = UNSUPPORTED.iter().find(|s| ctx.has(s)) {
        if let Some(a) = &ctx.auth
            && !a.key.covers(&name)
        {
            return Err(S3Error::new(Code::AccessDenied));
        }
        return Err(S3Error::msg(Code::NotImplemented, format!("?{sub} is not supported")));
    }
    if ctx.method == Method::PUT && ctx.query.iter().all(|(k, _)| k.starts_with("X-Amz-") || k.starts_with("x-id")) {
        return create(ctx, body).await;
    }
    // A key that can't see the bucket learns nothing about it (a policy may still let it in).
    let bucket = match ctx.store().bucket(&name) {
        Ok(b) => b,
        Err(e) => {
            return Err(match &ctx.auth {
                Some(a) if !a.key.covers(&name) => S3Error::new(Code::AccessDenied),
                _ => crate::error::store_error(e),
            });
        }
    };
    if crate::config::handles(ctx) {
        return crate::config::route(ctx, &bucket, body).await;
    }
    match ctx.method.clone() {
        Method::HEAD => head(ctx, &bucket),
        Method::GET if ctx.has("location") => {
            ctx.require(&name, Need::Read)?;
            ctx.meter(&bucket.id, Class::B);
            response::ok_xml(xml::location_constraint(&ctx.server.cfg.region))
        }
        Method::GET if ctx.has("versioning") => versioning::get_versioning(ctx, &bucket),
        Method::GET if ctx.has("versions") => versioning::list_versions(ctx, &bucket),
        Method::GET if ctx.has("object-lock") => versioning::get_object_lock(ctx, &bucket),
        Method::GET if ctx.has("cors") => get_cors(ctx, &bucket),
        Method::GET if ctx.has("lifecycle") => get_lifecycle(ctx, &bucket),
        Method::GET if ctx.has("uploads") => multipart::list_uploads(ctx, &bucket),
        Method::GET if ctx.q("list-type") == Some("2") => list_v2(ctx, &bucket),
        Method::GET => list_v1(ctx, &bucket),
        Method::PUT if ctx.has("cors") => put_cors(ctx, &bucket, body).await,
        Method::PUT if ctx.has("lifecycle") => put_lifecycle(ctx, &bucket, body).await,
        Method::PUT if ctx.has("versioning") => versioning::put_versioning(ctx, &bucket, body).await,
        Method::PUT if ctx.has("object-lock") => versioning::put_object_lock(ctx, &bucket, body).await,
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
        Method::DELETE => {
            ctx.require(&name, Need::Admin)?;
            ctx.meter(&bucket.id, Class::Free);
            ctx.store().delete_bucket(&name, false).await.s3()?;
            no_content()
        }
        Method::POST if ctx.has("delete") => versioning::delete_objects(ctx, &bucket, body).await,
        _ => Err(S3Error::new(Code::MethodNotAllowed)),
    }
}

async fn create(ctx: &mut Ctx, body: Body) -> Reply {
    let name = ctx.bucket_name().to_string();
    // There's no bucket (or policy) yet: only the key's own permission counts.
    if !ctx.auth.as_ref().is_some_and(|a| a.key.is_admin() && a.key.covers(&name)) {
        return Err(S3Error::new(Code::AccessDenied));
    }
    ctx.require_session("s3:CreateBucket", Some(&name), None)?;
    // Object Lock (which turns versioning on) from the start.
    let object_lock = ctx.h("x-amz-bucket-object-lock-enabled").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    if !s3_proto::http::valid_bucket_name(&name) {
        return Err(S3Error::new(Code::InvalidBucketName).with("BucketName", name));
    }
    // Object Ownership and an ACL may come with the bucket (ACLs need ownership other than the
    // default BucketOwnerEnforced).
    let ownership = match ctx.h("x-amz-object-ownership") {
        Some(o) => Some(acl::Ownership::parse(o).ok_or_else(|| S3Error::new(Code::InvalidArgument).with("ArgumentName", "x-amz-object-ownership").with("ArgumentValue", o.to_string()))?),
        None => None,
    };
    let bucket_acl = acl::from_headers(ctx, true)?.filter(|a| !a.owner_only());
    if let Some(a) = &bucket_acl {
        if ownership.is_none_or(|o| o == acl::Ownership::BucketOwnerEnforced) {
            return Err(S3Error::new(Code::InvalidBucketAclWithObjectOwnership));
        }
        if a.is_public() && ctx.store().cfg.block_public_access {
            return Err(S3Error::msg(Code::AccessDenied, "Access Denied: public ACLs are blocked for this deployment"));
        }
    }
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    xml::parse_create_bucket(&raw)?;
    // In us-east-1, S3 answers creating a bucket you already own with a 200 and changes
    // nothing (every other region says BucketAlreadyOwnedByYou).
    if ctx.server.cfg.region == "us-east-1" && ctx.store().bucket(&name).is_ok() {
        let mut r = response::empty(StatusCode::OK);
        set(&mut r, "location", &format!("/{name}"));
        return Ok(r);
    }
    let b = ctx.store().create_bucket(&name, BucketOptions { object_lock: Some(object_lock), ..Default::default() }).await.s3()?;
    ctx.meter(&b.id, Class::A);
    if let Some(o) = ownership.filter(|o| *o != acl::Ownership::BucketOwnerEnforced) {
        ctx.store().set_bucket_config(&name, s3_store::config::kind::OWNERSHIP, "", Some(o.as_str()), None).await.s3()?;
    }
    if let Some(stored) = bucket_acl.and_then(|a| a.stored()) {
        ctx.store().set_bucket_config(&name, s3_store::config::kind::ACL, "", Some(&stored), None).await.s3()?;
    }
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
    let rules: Vec<s3_proto::lifecycle::Rule> = bucket.lifecycle.iter().map(crate::lifecycle::to_xml).collect();
    response::ok_xml(s3_proto::lifecycle::render(&rules))
}

async fn put_lifecycle(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let rules: Vec<LifecycleRule> = s3_proto::lifecycle::parse(&raw)?.into_iter().map(crate::lifecycle::from_xml).collect();
    ctx.store().update_bucket(&bucket.name, BucketUpdate { lifecycle: Some(rules), ..Default::default() }).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

fn entry(m: &ObjectMeta, owner: Option<&xml::Owner>) -> xml::ObjectEntry {
    xml::ObjectEntry {
        key: m.key.clone(),
        last_modified_ms: m.modified_at,
        etag: m.etag.clone(),
        size: m.size,
        storage_class: "STANDARD".into(),
        checksum: m.checksum.as_ref().and_then(|c| Some((checksum_of(c)?, ChecksumType::parse(&c.kind)?))),
        owner: owner.cloned(),
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
    // `max-keys=0` asks for nothing, and S3 says there's nothing to continue from.
    let truncated = page.truncated && max > 0;
    let fetch_owner = ctx.q("fetch-owner") == Some("true");
    let owner = acl::owner(ctx.store());
    let l = xml::ListV2 {
        name: bucket.name.clone(),
        prefix: q.prefix.clone(),
        delimiter: q.delimiter.clone(),
        max_keys: max,
        is_truncated: truncated,
        continuation_token: token,
        next_continuation_token: page.next.as_ref().filter(|_| truncated).map(|n| URL_SAFE_NO_PAD.encode(n.as_bytes())),
        start_after,
        url_encode,
        contents: page.objects.iter().map(|m| entry(m, fetch_owner.then_some(&owner))).collect(),
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
    let truncated = page.truncated && max > 0;
    let owner = acl::owner(ctx.store());
    let l = xml::ListV1 {
        name: bucket.name.clone(),
        prefix: q.prefix.clone(),
        marker,
        next_marker: page.next.clone().filter(|_| truncated && q.delimiter.is_some()),
        delimiter: q.delimiter.clone(),
        max_keys: max,
        is_truncated: truncated,
        url_encode,
        contents: page.objects.iter().map(|m| entry(m, Some(&owner))).collect(),
        common_prefixes: page.prefixes.clone(),
    };
    response::ok_xml(xml::list_objects_v1(&l))
}
