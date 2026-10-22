//! POST Object: browser form uploads (`multipart/form-data`) with a presigned POST policy.
//!
//! The form is parsed as it streams: fields before the file are read (bounded), the file is
//! hashed into a spool file (its length isn't known up front, and the backend needs it), and
//! anything after the file is ignored, as S3 does.

use crate::body::Finished;
use crate::ctx::Ctx;
use crate::error::{StoreResult, store_error};
use crate::object::{InflightBlob, MAX_PUT};
use crate::response::{self, Reply, set};
use axum::body::{Body, BodyDataStream};
use axum::http::StatusCode;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use s3_proto::checksum::{Algorithm, BodyHasher};
use s3_proto::http::{quote_etag, uri_encode, user_metadata, validate_key, validate_tags};
use s3_proto::sigv4::{check_post_policy, parse_post_auth, signing_key, verify_post_signature};
use s3_proto::{Code, S3Error, xml};
use s3_store::{Checksum, Class, NewObject, ObjectHeaders, WriteCondition};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Fields before the file, all together.
const MAX_FIELDS_BYTES: usize = 64 << 10;
/// One part's headers.
const MAX_PART_HEADERS: usize = 16 << 10;

fn malformed(why: &str) -> S3Error {
    S3Error::msg(Code::MalformedPOSTRequest, format!("The body of your POST request is not well-formed multipart/form-data: {why}"))
}

/// `multipart/form-data; boundary=…`
fn boundary(content_type: &str) -> Option<String> {
    content_type.split(';').map(str::trim).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
    })
}

struct PartHeader {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
}

/// A streaming `multipart/form-data` reader with bounded buffering.
struct Form {
    inner: BodyDataStream,
    buf: BytesMut,
    /// `\r\n--<boundary>`.
    delim: Vec<u8>,
    eof: bool,
}

fn quoted_param(header: &str, name: &str) -> Option<String> {
    header.split(';').map(str::trim).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"').to_string())
    })
}

impl Form {
    fn new(body: Body, boundary: &str) -> Self {
        let mut buf = BytesMut::new();
        // The first delimiter has no CRLF before it.
        buf.extend_from_slice(b"\r\n");
        Self { inner: body.into_data_stream(), buf, delim: format!("\r\n--{boundary}").into_bytes(), eof: false }
    }

    /// Read more of the body; `false` at its end.
    async fn fill(&mut self) -> Result<bool, S3Error> {
        if self.eof {
            return Ok(false);
        }
        match self.inner.next().await {
            Some(Ok(b)) => {
                self.buf.extend_from_slice(&b);
                Ok(true)
            }
            Some(Err(_)) => Err(S3Error::new(Code::IncompleteBody)),
            None => {
                self.eof = true;
                Ok(false)
            }
        }
    }

    /// Skip to just after the next delimiter, keeping at most `limit` bytes of what was before it
    /// (`None` if more were skipped); the bytes before go to `sink` when given.
    async fn until_delim(&mut self, mut sink: Option<&mut Vec<u8>>, limit: usize) -> Result<(), S3Error> {
        loop {
            if let Some(i) = memchr::memmem::find(&self.buf, &self.delim) {
                let before = self.buf.split_to(i);
                let _ = self.buf.split_to(self.delim.len());
                if let Some(s) = sink.as_deref_mut() {
                    if s.len() + before.len() > limit {
                        return Err(S3Error::msg(Code::MaxMessageLengthExceeded, "A form field is too large."));
                    }
                    s.extend_from_slice(&before);
                }
                return Ok(());
            }
            let keep = self.delim.len().saturating_sub(1);
            if self.buf.len() > keep {
                let n = self.buf.len() - keep;
                let before = self.buf.split_to(n);
                if let Some(s) = sink.as_deref_mut() {
                    if s.len() + before.len() > limit {
                        return Err(S3Error::msg(Code::MaxMessageLengthExceeded, "A form field is too large."));
                    }
                    s.extend_from_slice(&before);
                }
            }
            if !self.fill().await? {
                return Err(malformed("missing closing boundary"));
            }
        }
    }

    /// After a delimiter: the next part's headers, or `None` at the closing delimiter.
    async fn next_part(&mut self) -> Result<Option<PartHeader>, S3Error> {
        while self.buf.len() < 2 {
            if !self.fill().await? {
                return Err(malformed("truncated"));
            }
        }
        if &self.buf[..2] == b"--" {
            return Ok(None);
        }
        let mut head = Vec::new();
        loop {
            if let Some(i) = memchr::memmem::find(&self.buf, b"\r\n\r\n") {
                head.extend_from_slice(&self.buf.split_to(i + 4));
                break;
            }
            if self.buf.len() > MAX_PART_HEADERS {
                return Err(malformed("part headers too large"));
            }
            if !self.fill().await? {
                return Err(malformed("truncated part headers"));
            }
        }
        let text = String::from_utf8_lossy(&head);
        let mut name = None;
        let mut filename = None;
        let mut content_type = None;
        for line in text.split("\r\n").map(str::trim).filter(|l| !l.is_empty()) {
            let Some((k, v)) = line.split_once(':') else { continue };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                name = quoted_param(v, "name");
                filename = quoted_param(v, "filename");
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                content_type = Some(v.trim().to_string());
            }
        }
        let name = name.ok_or_else(|| malformed("a part without a name"))?;
        Ok(Some(PartHeader { name, filename, content_type }))
    }

    /// Stream the current part (the file) into `out`, hashing it. Returns its length.
    async fn copy_file(&mut self, out: &mut tokio::fs::File, hasher: &mut BodyHasher, max: u64) -> Result<u64, S3Error> {
        let mut n = 0u64;
        let io = |e: std::io::Error| {
            tracing::error!(error = %e, "spooling a form upload failed");
            S3Error::new(Code::InternalError)
        };
        loop {
            let found = memchr::memmem::find(&self.buf, &self.delim);
            let take = match found {
                Some(i) => i,
                None => self.buf.len().saturating_sub(self.delim.len() - 1),
            };
            if take > 0 {
                let chunk: Bytes = self.buf.split_to(take).freeze();
                n += chunk.len() as u64;
                if n > max {
                    return Err(S3Error::new(Code::EntityTooLarge).with("MaxSizeAllowed", max.to_string()));
                }
                hasher.update(&chunk);
                out.write_all(&chunk).await.map_err(io)?;
            }
            if found.is_some() {
                let _ = self.buf.split_to(self.delim.len());
                out.flush().await.map_err(io)?;
                return Ok(n);
            }
            if !self.fill().await? {
                return Err(malformed("missing boundary after the file"));
            }
        }
    }
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

pub(crate) async fn upload(ctx: &mut Ctx, body: Body) -> Reply {
    let b = boundary(ctx.h("content-type").unwrap_or_default()).filter(|b| !b.is_empty() && b.len() <= 200).ok_or_else(|| malformed("no boundary"))?;
    let bucket = ctx.store().bucket(ctx.bucket_name()).s3()?;
    let mut form = Form::new(body, &b);
    form.until_delim(None, 0).await?;
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut used = 0usize;
    let file = loop {
        let Some(part) = form.next_part().await? else { return Err(S3Error::new(Code::IncorrectNumberOfFilesInPostRequest)) };
        if part.name.eq_ignore_ascii_case("file") {
            break part;
        }
        let mut value = Vec::new();
        form.until_delim(Some(&mut value), MAX_FIELDS_BYTES.saturating_sub(used)).await?;
        used += value.len() + part.name.len();
        fields.push((part.name, String::from_utf8_lossy(&value).into_owned()));
    };
    let filename = file.filename.clone().unwrap_or_default();
    let raw_key = field(&fields, "key").ok_or_else(|| S3Error::msg(Code::InvalidArgument, "Bucket POST must contain a field named 'key'.  If it is specified, please check the order of the fields.").with("ArgumentName", "key"))?;
    let key = raw_key.replace("${filename}", &filename);
    validate_key(&key)?;
    for f in fields.iter_mut() {
        if f.0.eq_ignore_ascii_case("key") {
            f.1 = key.clone();
        }
    }

    // Who's uploading: the policy's signer (SigV2 or SigV4), who must be allowed to write here.
    let token = field(&fields, "x-amz-security-token").map(str::trim).filter(|t| !t.is_empty());
    let find = |ak: &str| crate::temporary::resolve(ctx.store(), ak, token);
    let (access, policy_b64) = match s3_proto::sigv2::parse_post_auth(&fields)? {
        Some((ak, policy, signature)) => {
            let access = find(&ak)?;
            s3_proto::sigv2::verify_post_signature(&ak, &policy, &signature, &access.secret)?;
            (access, policy)
        }
        None => {
            let auth = parse_post_auth(&fields)?.ok_or_else(|| S3Error::msg(Code::AccessDenied, "Anonymous POST uploads are not allowed."))?;
            let access = find(&auth.credential.access_key)?;
            let c = &auth.credential;
            verify_post_signature(&auth, &signing_key(&access.secret, &c.date, &c.region, &c.service))?;
            (access, auth.policy)
        }
    };
    let policy = check_post_policy(&policy_b64, &fields, &bucket.name, ctx.store().now())?;
    // The signer is the request's principal from here on (its form body isn't signed in
    // chunks).
    let identity = crate::auth::identity_of(ctx, &access)?;
    let principal = crate::identity::event_principal(&access, &identity);
    ctx.auth = Some(crate::auth::Auth {
        verified: s3_proto::sigv4::Verified { access_key: access.id.clone(), payload: s3_proto::sigv4::Payload::Unsigned, stream: None },
        key: access.clone(),
        identity,
    });
    crate::access::authorize(ctx, ctx.who(), &bucket.name, Some(&key), "s3:PutObject", crate::auth::Need::Write)?;
    s3_store::events::set_origin(|o| {
        o.principal = Some(principal);
        o.create = Some("Post");
    });
    let acl = crate::acl::for_new_object_canned(ctx, &bucket.name, field(&fields, "acl"))?;
    let website_redirect = field(&fields, "x-amz-website-redirect-location").map(crate::website::check_redirect).transpose()?;
    if let Some(id) = access.acting_key() {
        ctx.store().touch_key(id);
    }
    ctx.meter(&bucket.id, Class::A);

    // The object's headers from the form.
    let meta_fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let tags = match field(&fields, "tagging") {
        Some(t) => {
            let tags = xml::parse_tagging(t.as_bytes())?;
            validate_tags(&tags)?;
            tags.into_iter().collect()
        }
        None => BTreeMap::new(),
    };
    let s = |n: &str| field(&fields, n).map(str::to_string);
    let headers = ObjectHeaders {
        content_type: s("content-type").or(file.content_type.clone()),
        cache_control: s("cache-control"),
        content_disposition: s("content-disposition"),
        content_encoding: s("content-encoding"),
        content_language: s("content-language"),
        expires: s("expires"),
        metadata: user_metadata(meta_fields)?,
        tags,
        acl,
        website_redirect,
    };
    let lock = crate::versioning::lock_from(|n| field(&fields, n), bucket.object_lock.is_some(), ctx.store().now())?;

    // Encryption, from the form's fields as from a PUT's headers.
    let target = crate::sse::target_from(ctx.store(), &bucket, |n| field(&fields, n))?;

    // Spool the file, hashing it, then store it.
    let spool = tempfile::NamedTempFile::new_in(&ctx.server.cfg.spool_dir).map_err(|e| {
        tracing::error!(error = %e, "creating a spool file failed");
        S3Error::new(Code::InternalError)
    })?;
    let mut out = tokio::fs::File::from_std(spool.reopen().map_err(|_| S3Error::new(Code::InternalError))?);
    let mut hasher = BodyHasher::new(Some(Algorithm::Crc64Nvme), false);
    let max = policy.content_length_range.map_or(MAX_PUT, |(_, hi)| hi.min(MAX_PUT));
    let len = form.copy_file(&mut out, &mut hasher, max).await?;
    if let Some((lo, hi)) = policy.content_length_range {
        if len < lo {
            return Err(S3Error::msg(Code::EntityTooSmall, "Your proposed upload is smaller than the minimum allowed size").with("ProposedSize", len.to_string()).with("MinSizeAllowed", lo.to_string()));
        }
        if len > hi {
            return Err(S3Error::msg(Code::EntityTooLarge, "Your proposed upload exceeds the maximum allowed size").with("ProposedSize", len.to_string()).with("MaxSizeAllowed", hi.to_string()));
        }
    }
    let digest = hasher.finish();
    let finished = Finished { checksum: digest.checksum_b64(), digest };
    let file_in = tokio::fs::File::open(spool.path()).await.map_err(|_| S3Error::new(Code::InternalError))?;
    let stream = futures::stream::try_unfold(file_in, |mut f| async move {
        let mut buf = vec![0u8; 1 << 20];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some((Bytes::from(buf), f)))
    })
    .boxed();
    let store = ctx.store().clone();
    let guard = InflightBlob::new(&store);
    let blob = guard.id.clone();
    let up = store.put_blob_sse(&blob, &bucket.id, &key, stream, len, headers.content_type.as_deref(), target.sse.as_ref()).await.map_err(store_error)?;
    drop(spool);
    store.meter.bytes_in(&bucket.id, len);
    let checksum = finished.checksum.map(|(a, v)| Checksum { algorithm: a.as_str().into(), kind: "FULL_OBJECT".into(), value: v });
    let enc = target.encryption();
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: crate::sse::etag(enc.as_ref(), finished.digest.etag()), headers, checksum };
    let meta = store.commit_put_full(&bucket.name, &key, obj, &WriteCondition::none(), &lock, enc.as_ref()).await.s3()?;
    drop(guard);
    let version = bucket.versioning.is_some().then(|| meta.version_id.clone());

    let sse_s3 = target.s3;
    let etag = quote_etag(&meta.etag);
    let host = ctx.h("host").unwrap_or("localhost");
    let location = format!("http://{host}/{}/{}", bucket.name, uri_encode(&key, false));
    if let Some(url) = s("success_action_redirect").or_else(|| s("redirect")).filter(|u| !u.is_empty()) {
        let sep = if url.contains('?') { '&' } else { '?' };
        let target = format!("{url}{sep}bucket={}&key={}&etag={}", uri_encode(&bucket.name, true), uri_encode(&key, true), uri_encode(&etag, true));
        let mut r = response::empty(StatusCode::SEE_OTHER);
        set(&mut r, "location", &target);
        set(&mut r, "etag", &etag);
        if let Some(v) = &version {
            set(&mut r, "x-amz-version-id", v);
        }
        crate::sse::set_headers(&mut r, enc.as_ref(), sse_s3 || ctx.store().sse_s3_at_rest());
        return Ok(r);
    }
    let mut r = match field(&fields, "success_action_status") {
        Some("200") => response::empty(StatusCode::OK),
        Some("201") => {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<PostResponse><Location>{}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag></PostResponse>",
                xml::escape(&location),
                xml::escape(&bucket.name),
                xml::escape(&key),
                xml::escape(&etag)
            );
            response::xml(StatusCode::CREATED, body)
        }
        _ => response::empty(StatusCode::NO_CONTENT),
    };
    set(&mut r, "etag", &etag);
    if let Some(v) = &version {
        set(&mut r, "x-amz-version-id", v);
    }
    set(&mut r, "location", &location);
    crate::sse::set_headers(&mut r, enc.as_ref(), sse_s3 || ctx.store().sse_s3_at_rest());
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries() {
        assert_eq!(boundary("multipart/form-data; boundary=abc").as_deref(), Some("abc"));
        assert_eq!(boundary("multipart/form-data; boundary=\"a b\"").as_deref(), Some("a b"));
        assert_eq!(boundary("multipart/form-data"), None);
    }

    async fn parse(body: &'static [u8], b: &str) -> Result<(Vec<(String, String)>, Vec<u8>), S3Error> {
        let chunks: Vec<Result<Bytes, std::io::Error>> = body.chunks(3).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let mut form = Form::new(Body::from_stream(futures::stream::iter(chunks)), b);
        form.until_delim(None, 0).await?;
        let mut fields = Vec::new();
        loop {
            let Some(p) = form.next_part().await? else { return Ok((fields, Vec::new())) };
            if p.name == "file" {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("f");
                let mut out = tokio::fs::File::create(&path).await.unwrap();
                let mut h = BodyHasher::new(None, false);
                form.copy_file(&mut out, &mut h, 1 << 20).await?;
                return Ok((fields, std::fs::read(&path).unwrap()));
            }
            let mut v = Vec::new();
            form.until_delim(Some(&mut v), 1000).await?;
            fields.push((p.name, String::from_utf8(v).unwrap()));
        }
    }

    #[tokio::test]
    async fn streams_a_form_in_small_pieces() {
        let body = b"--XX\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nuploads/${filename}\r\n--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--X world\r\n--XX\r\nContent-Disposition: form-data; name=\"after\"\r\n\r\nignored\r\n--XX--\r\n";
        let (fields, file) = parse(body, "XX").await.unwrap();
        assert_eq!(fields, vec![("key".to_string(), "uploads/${filename}".to_string())]);
        assert_eq!(file, b"hello\r\n--X world");
    }

    #[tokio::test]
    async fn rejects_truncated_forms() {
        let body = b"--XX\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nno end";
        assert!(parse(body, "XX").await.is_err());
    }
}
