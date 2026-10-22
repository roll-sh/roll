//! Versioning and Object Lock: Get/PutBucketVersioning, ListObjectVersions,
//! Get/PutObjectLockConfiguration, Get/PutObjectRetention, Get/PutObjectLegalHold, the
//! versioned DeleteObjects, and the version and lock headers of object reads and writes.

use crate::auth::Need;
use crate::ctx::{Ctx, read_small};
use crate::error::{StoreResult, store_error};
use crate::multipart::encoding_type;
use crate::object::checksum_of;
use crate::response::{self, Reply, set};
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use s3_proto::checksum::ChecksumType;
use s3_proto::time::{iso8601_ms, parse_iso8601};
use s3_proto::versioning::{self as v, invalid_version_id, valid_version_id};
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, BucketUpdate, Class, ObjectLock, ObjectLockConfig, VersionsQuery};

fn owner() -> xml::Owner {
    xml::Owner { id: "roll".into(), display_name: "roll".into() }
}

/// `?versionId=`, if given. A malformed one is S3's `InvalidArgument`.
pub(crate) fn version_id(ctx: &Ctx) -> Result<Option<String>, S3Error> {
    match ctx.q("versionId") {
        None => Ok(None),
        Some(id) if valid_version_id(id) => Ok(Some(id.to_string())),
        Some(id) => Err(invalid_version_id(id)),
    }
}

/// `x-amz-version-id`, sent when the bucket is (or was) versioned or a version was asked for.
pub(crate) fn set_version(r: &mut Response, bucket: &Bucket, version_id: &str, asked: bool) {
    if bucket.versioning.is_some() || asked {
        set(r, "x-amz-version-id", version_id);
    }
}

/// A version's Object Lock headers (GET and HEAD).
pub(crate) fn set_lock(r: &mut Response, lock: &ObjectLock) {
    if let (Some(mode), Some(until)) = (&lock.mode, lock.retain_until) {
        set(r, "x-amz-object-lock-mode", mode);
        set(r, "x-amz-object-lock-retain-until-date", &iso8601_ms(until));
    }
    if let Some(h) = lock.legal_hold {
        set(r, "x-amz-object-lock-legal-hold", if h { "ON" } else { "OFF" });
    }
}

fn missing_config() -> S3Error {
    S3Error::msg(Code::InvalidRequest, "Bucket is missing Object Lock Configuration")
}

/// The Object Lock a write asks for, from `x-amz-object-lock-*` headers (or POST form fields,
/// through `get`). Empty: none asked for (the bucket's default retention applies).
pub(crate) fn lock_from<'a>(get: impl Fn(&str) -> Option<&'a str>, lock_enabled: bool, now_ms: i64) -> Result<ObjectLock, S3Error> {
    let (mode, until, hold) = (get("x-amz-object-lock-mode"), get("x-amz-object-lock-retain-until-date"), get("x-amz-object-lock-legal-hold"));
    if mode.is_none() && until.is_none() && hold.is_none() {
        return Ok(ObjectLock::default());
    }
    if !lock_enabled {
        return Err(missing_config());
    }
    let mut lock = ObjectLock::default();
    match (mode, until) {
        (None, None) => {}
        (Some(m), Some(u)) => {
            let mode = v::lock_mode(m).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Unknown wormMode directive.").with("ArgumentName", "x-amz-object-lock-mode"))?;
            let until = parse_iso8601(u.trim()).ok_or_else(|| {
                S3Error::msg(Code::InvalidArgument, "The retain until date must be provided in ISO 8601 format").with("ArgumentName", "x-amz-object-lock-retain-until-date")
            })?;
            if until <= now_ms {
                return Err(S3Error::msg(Code::InvalidArgument, "The retain until date must be in the future!").with("ArgumentName", "x-amz-object-lock-retain-until-date"));
            }
            lock.mode = Some(mode.to_string());
            lock.retain_until = Some(until);
        }
        _ => return Err(S3Error::msg(Code::InvalidArgument, "x-amz-object-lock-retain-until-date and x-amz-object-lock-mode must both be supplied")),
    }
    if let Some(h) = hold {
        lock.legal_hold = Some(match h.trim() {
            "ON" => true,
            "OFF" => false,
            _ => return Err(S3Error::msg(Code::InvalidArgument, "Legal Hold must be either of 'ON' or 'OFF'").with("ArgumentName", "x-amz-object-lock-legal-hold")),
        });
    }
    Ok(lock)
}

/// [`lock_from`] the request's headers.
pub(crate) fn lock_headers(ctx: &Ctx, bucket: &Bucket) -> Result<ObjectLock, S3Error> {
    lock_from(|n| ctx.h(n), bucket.object_lock.is_some(), ctx.store().now())
}

/// S3 wants an integrity check on a PUT that sets Object Lock: `Content-MD5` or an
/// `x-amz-checksum-*` (header or trailer).
pub(crate) fn require_integrity(ctx: &Ctx) -> Result<(), S3Error> {
    let has = ctx.h("content-md5").is_some()
        || ctx.h("x-amz-sdk-checksum-algorithm").is_some()
        || ctx.h("x-amz-trailer").is_some_and(|t| t.to_ascii_lowercase().contains("x-amz-checksum-"))
        || ctx.headers.keys().any(|k| k.as_str().starts_with("x-amz-checksum-") && k.as_str() != "x-amz-checksum-mode" && k.as_str() != "x-amz-checksum-type" && k.as_str() != "x-amz-checksum-algorithm");
    if has { Ok(()) } else { Err(S3Error::msg(Code::InvalidRequest, "Content-MD5 OR x-amz-checksum- HTTP header is required for Put Object requests with Object Lock parameters")) }
}

/// `x-amz-bypass-governance-retention: true`, allowed for admin keys only.
pub(crate) fn bypass(ctx: &Ctx) -> Result<bool, S3Error> {
    if !ctx.h("x-amz-bypass-governance-retention").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return Ok(false);
    }
    match &ctx.auth {
        Some(a) if a.key.is_admin() => Ok(true),
        _ => Err(S3Error::msg(Code::AccessDenied, "Bypassing a governance retention needs an admin key.")),
    }
}

pub(crate) fn get_versioning(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    response::ok_xml(v::versioning_configuration(bucket.versioning.as_deref()))
}

pub(crate) async fn put_versioning(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let req = v::parse_versioning(&raw)?;
    if req.mfa_delete.as_deref() == Some("Enabled") {
        return Err(S3Error::msg(Code::NotImplemented, "MFA Delete is not supported: roll has no MFA devices. Object Lock protects versions instead."));
    }
    if let Some(status) = req.status {
        ctx.store().update_bucket(&bucket.name, BucketUpdate { versioning: Some(status), ..Default::default() }).await.s3()?;
    }
    Ok(response::empty(StatusCode::OK))
}

fn max_keys(ctx: &Ctx) -> Result<u32, S3Error> {
    match ctx.q("max-keys") {
        None => Ok(1000),
        Some(m) => m.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| n.min(1000) as u32).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Provided max-keys not an integer or within integer range").with("ArgumentName", "max-keys")),
    }
}

/// ListObjectVersions (`GET ?versions`).
pub(crate) fn list_versions(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::A);
    let url_encode = encoding_type(ctx)?;
    let max = max_keys(ctx)?;
    let key_marker = ctx.q("key-marker").unwrap_or_default().to_string();
    let vid_marker = ctx.q("version-id-marker").unwrap_or_default().to_string();
    if !vid_marker.is_empty() {
        if key_marker.is_empty() {
            return Err(S3Error::msg(Code::InvalidArgument, "A version-id marker cannot be specified without a key marker.").with("ArgumentName", "version-id-marker"));
        }
        if !valid_version_id(&vid_marker) {
            return Err(invalid_version_id(&vid_marker));
        }
    }
    let q = VersionsQuery {
        prefix: ctx.q("prefix").unwrap_or_default().to_string(),
        delimiter: ctx.q("delimiter").filter(|d| !d.is_empty()).map(str::to_string),
        key_marker: Some(key_marker.clone()).filter(|k| !k.is_empty()),
        version_id_marker: Some(vid_marker.clone()).filter(|v| !v.is_empty()),
        max_keys: max as usize,
    };
    let page = ctx.store().list_versions(&bucket.name, &q).s3()?;
    let truncated = page.truncated && max > 0;
    let l = v::ListVersions {
        name: bucket.name.clone(),
        prefix: q.prefix.clone(),
        key_marker,
        version_id_marker: vid_marker,
        next_key_marker: page.next_key_marker.clone().filter(|_| truncated),
        next_version_id_marker: page.next_version_id_marker.clone().filter(|_| truncated),
        delimiter: q.delimiter.clone(),
        max_keys: max,
        is_truncated: truncated,
        url_encode,
        entries: page
            .versions
            .iter()
            .map(|e| v::VersionEntry {
                key: e.key.clone(),
                version_id: e.version_id.clone(),
                is_latest: e.is_latest,
                delete_marker: e.delete_marker,
                last_modified_ms: e.last_modified,
                etag: e.etag.clone(),
                size: e.size,
                storage_class: "STANDARD".into(),
                checksum: e.checksum.as_ref().and_then(|c| Some((checksum_of(c)?, ChecksumType::parse(&c.kind)?))),
            })
            .collect(),
        common_prefixes: page.prefixes.clone(),
        owner: owner(),
    };
    response::ok_xml(v::list_versions(&l))
}

pub(crate) fn get_object_lock(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    let Some(c) = &bucket.object_lock else {
        return Err(S3Error::new(Code::ObjectLockConfigurationNotFoundError).with("BucketName", bucket.name.clone()));
    };
    response::ok_xml(v::object_lock_configuration(&v::ObjectLockConfig { mode: c.mode.clone(), days: c.days, years: c.years }))
}

pub(crate) async fn put_object_lock(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, Class::A);
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let c = v::parse_object_lock_configuration(&raw)?;
    ctx.store().put_object_lock(&bucket.name, ObjectLockConfig { mode: c.mode, days: c.days, years: c.years }).await.s3()?;
    Ok(response::empty(StatusCode::OK))
}

pub(crate) fn get_retention(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    if bucket.object_lock.is_none() {
        return Err(missing_config());
    }
    let vid = version_id(ctx)?;
    let m = ctx.store().get_object_version(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    match (&m.lock.mode, m.lock.retain_until) {
        (Some(mode), Some(until)) => response::ok_xml(v::retention(mode, until)),
        _ => Err(S3Error::new(Code::NoSuchObjectLockConfiguration)),
    }
}

pub(crate) async fn put_retention(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    let vid = version_id(ctx)?;
    let bypass = bypass(ctx)?;
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let retention = v::parse_retention(&raw)?;
    let m = ctx.store().put_retention(&bucket.name, ctx.key_name(), vid.as_deref(), retention, bypass).await.s3()?;
    let mut r = response::empty(StatusCode::OK);
    set_version(&mut r, bucket, &m.version_id, vid.is_some());
    Ok(r)
}

pub(crate) fn get_legal_hold(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    if bucket.object_lock.is_none() {
        return Err(missing_config());
    }
    let vid = version_id(ctx)?;
    let m = ctx.store().get_object_version(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    match m.lock.legal_hold {
        Some(on) => response::ok_xml(v::legal_hold(on)),
        None => Err(S3Error::new(Code::NoSuchObjectLockConfiguration)),
    }
}

pub(crate) async fn put_legal_hold(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::A);
    let vid = version_id(ctx)?;
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let on = v::parse_legal_hold(&raw)?;
    let m = ctx.store().put_legal_hold(&bucket.name, ctx.key_name(), vid.as_deref(), on).await.s3()?;
    let mut r = response::empty(StatusCode::OK);
    set_version(&mut r, bucket, &m.version_id, vid.is_some());
    Ok(r)
}

/// DeleteObjects, with a `VersionId` per key.
pub(crate) async fn delete_objects(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    ctx.meter(&bucket.id, Class::Free);
    let bypass = bypass(ctx)?;
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let req = xml::parse_delete(&raw)?;
    let mut errors = Vec::new();
    let mut items = Vec::with_capacity(req.keys.len());
    for (key, vid) in req.keys.into_iter().zip(req.version_ids) {
        match vid {
            Some(id) if !valid_version_id(&id) => errors.push(v::DeleteFailure { key, version_id: Some(id), code: Code::InvalidArgument, message: "Invalid version id specified".into() }),
            vid => items.push((key, vid)),
        }
    }
    let results = ctx.store().delete_versions(&bucket.name, &items, bypass).await.s3()?;
    let mut deleted = Vec::new();
    for ((key, vid), r) in items.into_iter().zip(results) {
        match r {
            Ok(o) => deleted.push(match &vid {
                Some(id) => v::Deleted { key, version_id: Some(id.clone()), delete_marker: o.delete_marker, delete_marker_version_id: o.delete_marker.then(|| id.clone()) },
                None => v::Deleted { key, version_id: None, delete_marker: o.delete_marker, delete_marker_version_id: o.version_id.filter(|_| o.delete_marker) },
            }),
            Err(e) => {
                let e = store_error(e);
                errors.push(v::DeleteFailure { key, version_id: vid, code: e.code, message: e.message });
            }
        }
    }
    response::ok_xml(v::delete_result(&deleted, &errors, req.quiet))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_headers_parse_like_s3() {
        let now = 1_700_000_000_000;
        let parse = |pairs: &[(&str, &str)], lock: bool| {
            let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            lock_from(|n| pairs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()), lock, now)
        };
        assert_eq!(parse(&[], false).unwrap(), ObjectLock::default());
        assert_eq!(parse(&[("x-amz-object-lock-legal-hold", "ON")], false).unwrap_err().code, Code::InvalidRequest);
        let full = parse(&[("x-amz-object-lock-mode", "COMPLIANCE"), ("x-amz-object-lock-retain-until-date", "2030-01-01T00:00:00Z"), ("x-amz-object-lock-legal-hold", "OFF")], true).unwrap();
        assert_eq!(full, ObjectLock { mode: Some("COMPLIANCE".into()), retain_until: Some(1_893_456_000_000), legal_hold: Some(false) });
        for bad in [
            vec![("x-amz-object-lock-mode", "COMPLIANCE")],
            vec![("x-amz-object-lock-mode", "LOCKED"), ("x-amz-object-lock-retain-until-date", "2030-01-01T00:00:00Z")],
            vec![("x-amz-object-lock-mode", "GOVERNANCE"), ("x-amz-object-lock-retain-until-date", "tomorrow")],
            vec![("x-amz-object-lock-mode", "GOVERNANCE"), ("x-amz-object-lock-retain-until-date", "2001-01-01T00:00:00Z")],
            vec![("x-amz-object-lock-legal-hold", "on")],
        ] {
            assert_eq!(parse(&bad, true).unwrap_err().code, Code::InvalidArgument, "{bad:?}");
        }
    }
}
