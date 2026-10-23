//! `multipart/form-data` uploads (storage-js sends a `Blob` or `File` this way): the fields
//! before the file are read (bounded), the first file is streamed into a spool file (its length
//! isn't known up front), and anything after it is ignored, as Supabase does.

use crate::error::{ApiError, ApiResult};
use axum::body::{Body, BodyDataStream};
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Fields before the file, all together.
const MAX_FIELDS: usize = 1 << 20;
/// One part's headers.
const MAX_PART_HEADERS: usize = 16 << 10;

pub(crate) struct FormUpload {
    /// The fields before the file (`cacheControl`, `contentType`, `metadata`, …).
    pub fields: Vec<(String, String)>,
    /// The file part's own `Content-Type`.
    pub file_content_type: Option<String>,
    pub file: tempfile::NamedTempFile,
    pub len: u64,
}

impl FormUpload {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn malformed(why: &str) -> ApiError {
    ApiError::invalid(format!("No content provided: the multipart/form-data body isn't valid ({why})"))
}

/// The `boundary` parameter of a `multipart/form-data` content type.
pub(crate) fn boundary(content_type: &str) -> Option<String> {
    content_type.split(';').map(str::trim).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
    })
}

fn param(header: &str, name: &str) -> Option<String> {
    header.split(';').map(str::trim).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"').to_string())
    })
}

struct Reader {
    stream: BodyDataStream,
    buf: BytesMut,
}

impl Reader {
    /// One more chunk; `false` at the end of the body.
    async fn fill(&mut self) -> ApiResult<bool> {
        match self.stream.next().await {
            Some(Ok(b)) => {
                self.buf.extend_from_slice(&b);
                Ok(true)
            }
            Some(Err(_)) => Err(ApiError::invalid("the upload was interrupted")),
            None => Ok(false),
        }
    }

    /// The bytes before `pat` (consumed with it), at most `max` of them.
    async fn until(&mut self, pat: &[u8], max: usize, what: &str) -> ApiResult<Bytes> {
        let finder = memchr::memmem::Finder::new(pat);
        loop {
            if let Some(i) = finder.find(&self.buf) {
                let out = self.buf.split_to(i).freeze();
                self.buf.advance(pat.len());
                return Ok(out);
            }
            if self.buf.len() > max + pat.len() {
                return Err(malformed(&format!("{what} too long")));
            }
            if !self.fill().await? {
                return Err(malformed("it ends early"));
            }
        }
    }

    async fn need(&mut self, n: usize) -> ApiResult<()> {
        while self.buf.len() < n {
            if !self.fill().await? {
                return Err(malformed("it ends early"));
            }
        }
        Ok(())
    }
}

/// Read a form up to its first file, spooling the file (at most `max` bytes, else
/// `EntityTooLarge`) under `spool_dir`.
pub(crate) async fn read(body: Body, content_type: &str, spool_dir: &Path, max: u64) -> ApiResult<FormUpload> {
    let b = boundary(content_type).filter(|b| !b.is_empty() && b.len() <= 200).ok_or_else(|| malformed("no boundary"))?;
    let mut r = Reader { stream: body.into_data_stream(), buf: BytesMut::new() };
    // The preamble, then the first delimiter.
    let first = format!("--{b}");
    r.until(first.as_bytes(), 64 << 10, "preamble").await?;
    let delim = format!("\r\n--{b}");
    let mut fields = Vec::new();
    let mut fields_bytes = 0usize;
    loop {
        r.need(2).await?;
        if &r.buf[..2] == b"--" {
            return Err(ApiError::invalid("No content provided"));
        }
        if &r.buf[..2] != b"\r\n" {
            return Err(malformed("bad delimiter"));
        }
        r.buf.advance(2);
        let head = r.until(b"\r\n\r\n", MAX_PART_HEADERS, "part headers").await?;
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut disposition = None;
        let mut part_type = None;
        for line in head.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => disposition = Some(v.trim().to_string()),
                "content-type" => part_type = Some(v.trim().to_string()),
                _ => {}
            }
        }
        let disposition = disposition.ok_or_else(|| malformed("a part has no Content-Disposition"))?;
        let name = param(&disposition, "name").unwrap_or_default();
        let is_file = param(&disposition, "filename").is_some();
        if !is_file {
            let v = r.until(delim.as_bytes(), MAX_FIELDS, "field").await?;
            fields_bytes += v.len() + name.len();
            if fields_bytes > MAX_FIELDS {
                return Err(malformed("fields too long"));
            }
            fields.push((name, String::from_utf8_lossy(&v).into_owned()));
            continue;
        }
        // The file: stream it to the spool, holding back what could be the delimiter's start.
        let spool = tempfile::NamedTempFile::new_in(spool_dir).map_err(|e| {
            tracing::error!(error = %e, "creating a spool file failed");
            ApiError::internal()
        })?;
        let mut out = tokio::fs::File::from_std(spool.reopen().map_err(|_| ApiError::internal())?);
        let finder = memchr::memmem::Finder::new(delim.as_bytes());
        let keep = delim.len() - 1;
        let mut len = 0u64;
        let io = |_| ApiError::internal();
        loop {
            if let Some(i) = finder.find(&r.buf) {
                len += i as u64;
                if len > max {
                    return Err(ApiError::too_large("object"));
                }
                out.write_all(&r.buf[..i]).await.map_err(io)?;
                break;
            }
            if r.buf.len() > keep {
                let n = r.buf.len() - keep;
                len += n as u64;
                if len > max {
                    return Err(ApiError::too_large("object"));
                }
                out.write_all(&r.buf[..n]).await.map_err(io)?;
                r.buf.advance(n);
            }
            if !r.fill().await? {
                return Err(malformed("it ends inside the file"));
            }
        }
        out.flush().await.map_err(io)?;
        return Ok(FormUpload { fields, file_content_type: part_type, file: spool, len });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(body: &'static [u8], max: u64) -> ApiResult<(FormUpload, Vec<u8>)> {
        let dir = tempfile::tempdir().unwrap();
        // Tiny chunks, so delimiters straddle them.
        let chunks: Vec<Result<Bytes, std::io::Error>> = body.chunks(3).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let f = read(Body::from_stream(futures::stream::iter(chunks)), "multipart/form-data; boundary=XyZ", dir.path(), max).await?;
        let bytes = std::fs::read(f.file.path()).unwrap();
        Ok((f, bytes))
    }

    #[tokio::test]
    async fn fields_then_file() {
        let body = b"preamble\r\n--XyZ\r\nContent-Disposition: form-data; name=\"cacheControl\"\r\n\r\n3600\r\n--XyZ\r\nContent-Disposition: form-data; name=\"\"; filename=\"blob\"\r\nContent-Type: image/png\r\n\r\nhello\r\n--Xy world\r\n--XyZ--\r\n";
        let (f, bytes) = parse(body, 100).await.unwrap();
        assert_eq!(f.field("cacheControl"), Some("3600"));
        assert_eq!(f.file_content_type.as_deref(), Some("image/png"));
        assert_eq!(bytes, b"hello\r\n--Xy world");
        assert_eq!(f.len, bytes.len() as u64);
        assert_eq!(parse(body, 5).await.err().unwrap().status, 413);
        let empty = b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nb\r\n--XyZ--\r\n";
        assert_eq!(parse(empty, 100).await.err().unwrap().status, 400);
        assert!(parse(b"--XyZ\r\nContent-Disposition: form-data; name=\"\"; filename=\"x\"\r\n\r\nno end", 100).await.is_err());
    }
}
