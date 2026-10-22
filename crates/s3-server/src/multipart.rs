//! Multipart uploads: create, upload (and copy) parts, complete, abort, list.

use crate::auth::Need;
use crate::body::{Incoming, requested_checksum};
use crate::ctx::{Ctx, no_content, read_small_object_checksum};
use crate::error::{StoreResult, store_error};
use crate::object::{InflightBlob, MAX_PUT, object_headers, precheck, set_checksum, write_condition};
use crate::response::{self, Reply, set};
use axum::body::Body;
use axum::http::StatusCode;
use s3_proto::checksum::{Algorithm, ChecksumType, b64, combine_crc, composite, crc_bytes, crc_from_bytes};
use s3_proto::http::{Conditions, parse_copy_source, parse_copy_source_range, quote_etag, uri_encode};
use s3_proto::{Code, S3Error, xml};
use s3_store::{Bucket, Checksum, Class, Part, Upload, UploadsQuery};
use std::collections::HashMap;

fn owner() -> xml::Owner {
    xml::Owner { id: "roll".into(), display_name: "roll".into() }
}

fn upload_id(ctx: &Ctx) -> Result<String, S3Error> {
    ctx.q("uploadId").map(str::to_string).filter(|s| !s.is_empty()).ok_or_else(|| S3Error::msg(Code::InvalidArgument, "uploadId is required").with("ArgumentName", "uploadId"))
}

fn part_number(ctx: &Ctx) -> Result<u32, S3Error> {
    ctx.q("partNumber")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|n| (1..=10_000).contains(n))
        .ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Part number must be an integer between 1 and 10000, inclusive").with("ArgumentName", "partNumber"))
}

/// The upload, which must be for this request's key.
fn find_upload(ctx: &Ctx, bucket: &Bucket) -> Result<Upload, S3Error> {
    let id = upload_id(ctx)?;
    let up = ctx.store().upload(&bucket.name, &id).s3()?;
    if up.key != ctx.key_name() {
        return Err(S3Error::new(Code::NoSuchUpload).with("UploadId", id));
    }
    Ok(up)
}

fn upload_checksum(up: &Upload) -> Option<(Algorithm, ChecksumType)> {
    let a = Algorithm::parse(up.checksum_algorithm.as_deref()?)?;
    let t = up.checksum_type.as_deref().and_then(ChecksumType::parse).unwrap_or(a.default_multipart_type());
    Some((a, t))
}

pub(crate) async fn create(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let headers = object_headers(ctx)?;
    let invalid = |m: String| S3Error::msg(Code::InvalidRequest, m);
    let alg = ctx.h("x-amz-checksum-algorithm").map(|a| Algorithm::parse(a).ok_or_else(|| invalid(format!("Checksum algorithm {a} is not supported")))).transpose()?;
    let typ = ctx.h("x-amz-checksum-type").map(|t| ChecksumType::parse(t).ok_or_else(|| invalid(format!("Checksum type {t} is not supported")))).transpose()?;
    let typ = match (alg, typ) {
        (None, Some(_)) => return Err(invalid("The x-amz-checksum-type header can only be used with the x-amz-checksum-algorithm header.".into())),
        (Some(a), t) => Some(t.unwrap_or(a.default_multipart_type())),
        (None, None) => None,
    };
    if let (Some(a), Some(t)) = (alg, typ)
        && !a.supports(t)
    {
        return Err(invalid(format!("The {} checksum type cannot be used with the {} checksum algorithm.", t.as_str(), a.as_str())));
    }
    ctx.meter(&bucket.id, Class::A);
    let up = ctx.store().create_upload(&bucket.name, ctx.key_name(), headers, alg.map(|a| a.as_str().to_string()), typ.map(|t| t.as_str().to_string())).await.s3()?;
    let mut r = response::xml(StatusCode::OK, xml::initiate_multipart(&bucket.name, ctx.key_name(), &up.id));
    if let (Some(a), Some(t)) = (alg, typ) {
        set(&mut r, "x-amz-checksum-algorithm", a.as_str());
        set(&mut r, "x-amz-checksum-type", t.as_str());
    }
    Ok(r)
}

pub(crate) async fn upload_part(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let n = part_number(ctx)?;
    let up = find_upload(ctx, bucket)?;
    ctx.meter(&bucket.id, Class::A);
    if ctx.h("x-amz-copy-source").is_some() {
        return copy_part(ctx, &up, n).await;
    }
    let declared = upload_checksum(&up).map(|(a, _)| a);
    let want = match (declared, requested_checksum(ctx)?) {
        (Some(a), Some((r, _))) if a != r => {
            return Err(S3Error::msg(Code::InvalidRequest, format!("Checksum Type mismatch occurred, expected checksum Type: {}, actual checksum Type: {}", a.as_str().to_lowercase(), r.as_str().to_lowercase())));
        }
        (Some(a), Some((_, v))) => Some((a, v)),
        (Some(a), None) => Some((a, None)),
        (None, req) => req,
    };
    let incoming = Incoming::new(ctx, body, want, None)?;
    let len = incoming.len().ok_or_else(|| S3Error::new(Code::MissingContentLength))?;
    if len > MAX_PUT {
        return Err(S3Error::new(Code::EntityTooLarge).with("ProposedSize", len.to_string()).with("MaxSizeAllowed", MAX_PUT.to_string()));
    }
    let store = ctx.store().clone();
    let result = incoming.result();
    let uploaded = store.put_part_data(&up, n, incoming.into_stream(), len).await;
    // A part that failed its checks is never recorded: a retry of the part replaces it.
    let (data, finished) = match (uploaded, result.get()) {
        (Ok(d), Ok(f)) => (d, f),
        (Err(e), body) => return Err(body.err().unwrap_or_else(|| store_error(e))),
        (Ok(_), Err(e)) => return Err(e),
    };
    store.meter.bytes_in(&bucket.id, len);
    let etag = finished.digest.etag();
    let checksum = finished.checksum.clone();
    store.commit_part(&up, n, &data, &etag, checksum.as_ref().map(|(_, v)| v.clone())).await.s3()?;
    let mut r = response::empty(StatusCode::OK);
    set(&mut r, "etag", &quote_etag(&etag));
    if let Some((a, v)) = &checksum {
        set(&mut r, a.header(), v);
    }
    Ok(r)
}

async fn copy_part(ctx: &mut Ctx, up: &Upload, n: u32) -> Reply {
    let (src_bucket, src_key) = parse_copy_source(ctx.h("x-amz-copy-source").unwrap_or_default())?;
    ctx.require(&src_bucket, Need::Read)?;
    let src = ctx.store().get_object(&src_bucket, &src_key).s3()?;
    Conditions::copy_source_from_headers(|h| ctx.h(h)).check_copy_source(&src.etag, src.modified_at)?;
    let range = match ctx.h("x-amz-copy-source-range") {
        Some(h) => {
            let (a, b) = parse_copy_source_range(h, src.size)?;
            Some(a..b + 1)
        }
        None => None,
    };
    let len = range.as_ref().map_or(src.size, |r| r.end - r.start);
    if len > MAX_PUT {
        return Err(S3Error::new(Code::EntityTooLarge).with("ProposedSize", len.to_string()).with("MaxSizeAllowed", MAX_PUT.to_string()));
    }
    let part = ctx.store().copy_part(up, n, &src_bucket, &src_key, range, Some(&src.etag)).await.s3()?;
    response::ok_xml(xml::copy_part_result(&part.etag, part.created_at, None))
}

/// The whole object's checksum from its parts' (`None` if a part has none, e.g. a copied one).
fn object_checksum(ctx: &Ctx, up: &Upload, chosen: &[&Part], sent: &[xml::CompletePart]) -> Result<Option<Checksum>, S3Error> {
    let Some((a, t)) = upload_checksum(up) else { return Ok(None) };
    for (p, s) in chosen.iter().zip(sent) {
        for (sa, sv) in &s.checksums {
            if *sa != a {
                return Err(S3Error::msg(Code::InvalidPart, format!("The upload was created using the {} checksum algorithm.", a.as_str())).with("PartNumber", p.part_number.to_string()));
            }
            if p.checksum.as_deref() != Some(sv.as_str()) {
                return Err(S3Error::new(Code::InvalidPart).with("PartNumber", p.part_number.to_string()));
            }
        }
    }
    let mut raws = Vec::with_capacity(chosen.len());
    for p in chosen {
        let Some(v) = &p.checksum else { return Ok(None) };
        raws.push(a.decode(v)?);
    }
    let value = match t {
        ChecksumType::Composite => composite(a, &raws),
        ChecksumType::FullObject => {
            let mut crcs = Vec::with_capacity(raws.len());
            for (raw, p) in raws.iter().zip(chosen) {
                let Some(c) = crc_from_bytes(a, raw) else { return Ok(None) };
                crcs.push((c, p.size));
            }
            let Some(c) = combine_crc(a, &crcs) else { return Ok(None) };
            b64(&crc_bytes(a, c))
        }
    };
    // A full-object checksum the client computed must match.
    if let Some(sent) = ctx.h(a.header())
        && sent.trim() != value
        && !(t == ChecksumType::Composite && value.starts_with(&format!("{}-", sent.trim())))
    {
        return Err(S3Error::msg(Code::BadDigest, format!("The {} you specified did not match the calculated checksum.", a.as_str())));
    }
    Ok(Some(Checksum { algorithm: a.as_str().into(), kind: t.as_str().into(), value }))
}

pub(crate) async fn complete(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let up = find_upload(ctx, bucket)?;
    let raw = read_small_object_checksum(ctx, body, xml::MAX_XML_BYTES).await?;
    let sent = xml::parse_complete_multipart(&raw)?;
    xml::check_part_order(&sent)?;
    let cond = write_condition(ctx)?;
    ctx.meter(&bucket.id, Class::A);
    precheck(ctx, &cond)?;
    let (stored, _) = ctx.store().list_parts(&bucket.name, &up.id, 0, 10_000).s3()?;
    let by_number: HashMap<u32, &Part> = stored.iter().map(|p| (p.part_number, p)).collect();
    let mut chosen = Vec::with_capacity(sent.len());
    for s in &sent {
        match by_number.get(&s.number) {
            Some(p) if p.etag == s.etag => chosen.push(*p),
            _ => return Err(S3Error::new(Code::InvalidPart).with("PartNumber", s.number.to_string())),
        }
    }
    let checksum = object_checksum(ctx, &up, &chosen, &sent)?;
    let parts: Vec<(u32, String)> = sent.iter().map(|p| (p.number, p.etag.clone())).collect();
    // The store marks the blob in flight while it completes; release it even if the client
    // goes away mid-way.
    let guard = InflightBlob::adopt(ctx.store(), up.blob_id.clone());
    let meta = ctx.store().complete_upload(&bucket.name, &up.id, &parts, checksum, &cond).await.s3()?;
    drop(guard);
    let host = ctx.h("host").unwrap_or("localhost").to_string();
    let location = format!("http://{host}/{}/{}", bucket.name, uri_encode(&meta.key, false));
    let c = meta.checksum.as_ref().and_then(|c| Some((Algorithm::parse(&c.algorithm)?, c.value.as_str(), ChecksumType::parse(&c.kind)?)));
    let mut r = response::xml(StatusCode::OK, xml::complete_multipart(&location, &bucket.name, &meta.key, &meta.etag, c));
    if let Some(c) = &meta.checksum {
        set_checksum(&mut r, c);
    }
    Ok(r)
}

pub(crate) async fn abort(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Write)?;
    let up = find_upload(ctx, bucket)?;
    ctx.meter(&bucket.id, Class::Free);
    ctx.store().abort_bucket_upload(&bucket.name, &up.id).await.s3()?;
    no_content()
}

fn max_param(ctx: &Ctx, name: &str) -> Result<u32, S3Error> {
    match ctx.q(name) {
        None => Ok(1000),
        Some(v) => v.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| n.min(1000) as u32).ok_or_else(|| S3Error::msg(Code::InvalidArgument, format!("Provided {name} not an integer or within integer range")).with("ArgumentName", name.to_string())),
    }
}

pub(crate) fn list_parts(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    let up = find_upload(ctx, bucket)?;
    ctx.meter(&bucket.id, Class::A);
    let marker = match ctx.q("part-number-marker") {
        Some(m) => m.parse::<u32>().map_err(|_| S3Error::msg(Code::InvalidArgument, "Provided part-number-marker not an integer or within integer range").with("ArgumentName", "part-number-marker"))?,
        None => 0,
    };
    let max = max_param(ctx, "max-parts")?;
    let (parts, more) = ctx.store().list_parts(&bucket.name, &up.id, marker, max as usize).s3()?;
    // An empty page asked for (`max-parts=0`) isn't truncated: there'd be no marker to go on from.
    let more = more && max > 0;
    let ck = upload_checksum(&up);
    let entries: Vec<xml::PartEntry> = parts
        .iter()
        .map(|p| xml::PartEntry {
            number: p.part_number,
            last_modified_ms: p.created_at,
            etag: p.etag.clone(),
            size: p.size,
            checksum: ck.and_then(|(a, _)| p.checksum.clone().map(|v| (a, v))),
        })
        .collect();
    let l = xml::ListParts {
        bucket: bucket.name.clone(),
        key: up.key.clone(),
        upload_id: up.id.clone(),
        part_number_marker: marker,
        next_part_number_marker: if more { parts.last().map(|p| p.part_number) } else { None },
        max_parts: max,
        is_truncated: more,
        parts: entries,
        storage_class: "STANDARD".into(),
        owner: owner(),
        checksum: ck,
    };
    response::ok_xml(xml::list_parts(&l))
}

pub(crate) fn list_uploads(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::A);
    let url_encode = encoding_type(ctx)?;
    let max = max_param(ctx, "max-uploads")?;
    let q = UploadsQuery {
        prefix: ctx.q("prefix").unwrap_or_default().to_string(),
        delimiter: ctx.q("delimiter").filter(|d| !d.is_empty()).map(str::to_string),
        key_marker: ctx.q("key-marker").filter(|m| !m.is_empty()).map(str::to_string),
        upload_id_marker: ctx.q("upload-id-marker").filter(|m| !m.is_empty()).map(str::to_string),
        max_uploads: max as usize,
    };
    let page = ctx.store().list_uploads(&bucket.name, &q).s3()?;
    let truncated = page.truncated && max > 0;
    let l = xml::ListUploads {
        bucket: bucket.name.clone(),
        key_marker: q.key_marker.clone().unwrap_or_default(),
        upload_id_marker: q.upload_id_marker.clone().unwrap_or_default(),
        next_key_marker: page.next_key_marker.clone().filter(|_| truncated),
        next_upload_id_marker: page.next_upload_id_marker.clone().filter(|_| truncated),
        prefix: q.prefix.clone(),
        delimiter: q.delimiter.clone(),
        max_uploads: max,
        is_truncated: truncated,
        url_encode,
        uploads: page.uploads.iter().map(|u| xml::UploadEntry { key: u.key.clone(), upload_id: u.id.clone(), initiated_ms: u.created_at, checksum: upload_checksum(u) }).collect(),
        common_prefixes: page.prefixes.clone(),
        owner: owner(),
        storage_class: "STANDARD".into(),
    };
    response::ok_xml(xml::list_uploads(&l))
}

/// `encoding-type=url` (the only value S3 knows).
pub(crate) fn encoding_type(ctx: &Ctx) -> Result<bool, S3Error> {
    match ctx.q("encoding-type") {
        None => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("url") => Ok(true),
        Some(_) => Err(S3Error::msg(Code::InvalidArgument, "Invalid Encoding Method specified in Request").with("ArgumentName", "encoding-type")),
    }
}
