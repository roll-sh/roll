//! GetObjectAttributes (`GET /<bucket>/<key>?attributes`): an object's ETag, checksum, parts,
//! storage class and size, without its body.

use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::response::{self, Reply, set};
use crate::versioning;
use axum::http::StatusCode;
use s3_proto::checksum::{Algorithm, ChecksumType};
use s3_proto::http::{Conditions, quote_etag};
use s3_proto::time::http_date;
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, Class, ObjectMeta};

/// What `x-amz-object-attributes` can ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Wanted {
    etag: bool,
    checksum: bool,
    parts: bool,
    storage_class: bool,
    size: bool,
}

/// The attributes asked for: comma-separated names, in one header or several.
fn wanted(ctx: &Ctx) -> Result<Wanted, S3Error> {
    let mut w = Wanted::default();
    let mut any = false;
    for v in ctx.headers.get_all("x-amz-object-attributes") {
        let v = v.to_str().map_err(|_| invalid_attribute())?;
        for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            any = true;
            match name {
                "ETag" => w.etag = true,
                "Checksum" => w.checksum = true,
                "ObjectParts" => w.parts = true,
                "StorageClass" => w.storage_class = true,
                "ObjectSize" => w.size = true,
                _ => return Err(invalid_attribute()),
            }
        }
    }
    if !any {
        return Err(S3Error::msg(Code::InvalidArgument, "The x-amz-object-attributes header specifying the attributes to be retrieved is either missing or empty")
            .with("ArgumentName", "x-amz-object-attributes"));
    }
    Ok(w)
}

fn invalid_attribute() -> S3Error {
    S3Error::msg(Code::InvalidArgument, "Invalid attribute name specified.").with("ArgumentName", "x-amz-object-attributes")
}

/// A non-negative integer header, or `default` when it's absent.
fn int_header(ctx: &Ctx, name: &str, default: u32) -> Result<u32, S3Error> {
    match ctx.h(name).map(str::trim) {
        None => Ok(default),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(|n| n.min(i64::from(u32::MAX)) as u32)
            .ok_or_else(|| S3Error::msg(Code::InvalidArgument, format!("Provided {name} not an integer or within integer range")).with("ArgumentName", name.to_string())),
    }
}

/// A page of a multipart object's parts, numbered 1..=N as for `GET ?partNumber=`, after
/// `marker`, at most `max`.
fn parts_page(meta: &ObjectMeta, marker: u32, max: u32) -> xml::AttributesParts {
    let alg = meta.checksum.as_ref().and_then(|c| Algorithm::parse(&c.algorithm));
    let count = meta.part_sizes.len() as u32;
    let start = marker.min(count);
    let end = start.saturating_add(max).min(count);
    let parts: Vec<xml::AttributesPart> = (start..end)
        .map(|i| xml::AttributesPart {
            number: i + 1,
            size: meta.part_sizes[i as usize],
            checksum: alg.zip(meta.part_checksums.get(i as usize).cloned()),
        })
        .collect();
    // An empty page asked for (`max-parts=0`) isn't truncated: there'd be no marker to go on from.
    let truncated = end < count && max > 0;
    xml::AttributesParts {
        parts_count: meta.parts.max(count),
        part_number_marker: marker,
        next_part_number_marker: truncated.then_some(end),
        max_parts: max,
        is_truncated: truncated,
        parts,
    }
}

pub(crate) fn get_attributes(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require_object_read(bucket)?;
    let want = wanted(ctx)?;
    let max = int_header(ctx, "x-amz-max-parts", 1000)?.min(1000);
    let marker = int_header(ctx, "x-amz-part-number-marker", 0)?;
    let vid = versioning::version_id(ctx)?;
    let meta = ctx.store().get_object_version(&bucket.name, ctx.key_name(), vid.as_deref()).s3()?;
    ctx.meter(&bucket.id, Class::B);
    if let Err(e) = Conditions::from_headers(|n| ctx.h(n)).check_read(&meta.etag, meta.modified_at) {
        if e.code == Code::NotModified {
            let mut r = response::empty(StatusCode::NOT_MODIFIED);
            set(&mut r, "etag", &quote_etag(&meta.etag));
            set(&mut r, "last-modified", &http_date(meta.modified_at));
            versioning::set_version(&mut r, bucket, &meta.version_id, vid.is_some());
            return Ok(r);
        }
        return Err(e);
    }
    let checksum = meta.checksum.as_ref().and_then(|c| Some((Algorithm::parse(&c.algorithm)?, c.value.clone(), ChecksumType::parse(&c.kind)?)));
    let a = xml::ObjectAttributes {
        etag: want.etag.then(|| meta.etag.clone()),
        checksum: checksum.filter(|_| want.checksum),
        // Only multipart objects have parts.
        parts: (want.parts && meta.parts > 0).then(|| parts_page(&meta, marker, max)),
        storage_class: want.storage_class.then(|| "STANDARD".to_string()),
        size: want.size.then_some(meta.size),
    };
    let mut r = response::xml(StatusCode::OK, xml::get_object_attributes(&a));
    set(&mut r, "last-modified", &http_date(meta.modified_at));
    versioning::set_version(&mut r, bucket, &meta.version_id, vid.is_some());
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3_store::{Checksum, ObjectHeaders, ObjectLock};

    fn meta(sizes: &[u64], checksums: &[&str]) -> ObjectMeta {
        ObjectMeta {
            key: "k".into(),
            blob_id: "b".into(),
            size: sizes.iter().sum(),
            etag: "e-3".into(),
            headers: ObjectHeaders::default(),
            checksum: Some(Checksum { algorithm: "CRC32".into(), kind: "COMPOSITE".into(), value: "x-3".into() }),
            parts: sizes.len() as u32,
            part_sizes: sizes.to_vec(),
            part_checksums: checksums.iter().map(|s| s.to_string()).collect(),
            modified_at: 0,
            version_id: "null".into(),
            lock: ObjectLock::default(),
        }
    }

    #[test]
    fn pages_of_parts() {
        let m = meta(&[5, 6, 7], &["a", "b", "c"]);
        let p = parts_page(&m, 0, 2);
        assert_eq!(p.parts.iter().map(|p| (p.number, p.size)).collect::<Vec<_>>(), [(1, 5), (2, 6)]);
        assert_eq!((p.is_truncated, p.next_part_number_marker, p.parts_count), (true, Some(2), 3));
        assert_eq!(p.parts[1].checksum, Some((Algorithm::Crc32, "b".to_string())));
        let p = parts_page(&m, 2, 2);
        assert_eq!(p.parts.iter().map(|p| p.number).collect::<Vec<_>>(), [3]);
        assert_eq!((p.is_truncated, p.next_part_number_marker), (false, None));
        assert!(parts_page(&m, 9, 2).parts.is_empty());
        let p = parts_page(&m, 0, 0);
        assert!(p.parts.is_empty() && !p.is_truncated);
        // Without kept checksums, parts still have numbers and sizes.
        let p = parts_page(&meta(&[5, 6], &[]), 0, 1000);
        assert!(p.parts.iter().all(|p| p.checksum.is_none()));
        assert_eq!(p.parts.len(), 2);
    }
}
