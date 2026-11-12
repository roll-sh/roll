//! The HTTP layer, independent of any web framework (see [`crate::server`] for the hyper/axum
//! adapter).
//!
//! Behaviour measured on ScyllaDB 2026.3.3:
//!
//! - **Routing.** `POST /` is the API. `GET /` is the health check (`healthy: <Host>`, plain
//!   text); unknown all-uppercase methods count as GET. `OPTIONS /` answers 200 with an empty
//!   body. `GET`/`OPTIONS /localnodes` lists the node addresses (`?dc=`, `?rack=` filter).
//!   A query string and one trailing `/` are ignored. Everything else is 404 with
//!   `{"message": "Not found", "code": 404}`.
//! - **Operation.** The name is everything after the first `.` of `X-Amz-Target` (the prefix is
//!   not checked), trimmed; an unknown name is `UnknownOperationException`. The request
//!   Content-Type is ignored; responses are `application/x-amz-json-1.0`.
//! - **Order of checks:** body size (16 MiB, inclusive) → `Content-Encoding` → signature →
//!   operation name → JSON syntax → body must be an object → the operation.
//! - **Request decompression:** exactly `gzip` or `deflate` (zlib framing), concatenated members
//!   allowed, decompressed size bounded by the same 16 MiB limit. Other encodings are a 500
//!   `Unsupported Content-Encoding`.
//! - **Response compression:** a body of at least 4096 bytes is compressed (zlib level 6) when
//!   `Accept-Encoding` selects gzip or deflate; see [`choose_encoding`]. Only `Content-Encoding`
//!   is added.
//! - **CORS:** a non-empty `Origin` adds `Access-Control-Allow-Origin: *` and
//!   `Access-Control-Expose-Headers`; `OPTIONS` also echoes the requested methods and headers
//!   and adds `Access-Control-Max-Age: 7200`. Only the handlers on `/` do this.
//! - No CRC32 (`x-amz-crc32`) header is sent: ScyllaDB doesn't send one.

use std::io::{Read, Write};

use bytes::Bytes;

use crate::error::{Error, ErrorKind};
use crate::json::Json;

/// A request, as received.
#[derive(Clone, Debug, Default)]
pub struct HttpRequest {
    pub method: String,
    /// The path without the query string.
    pub path: String,
    /// The raw query string (without `?`).
    pub query: String,
    /// Header names in any case; repeated headers appear repeatedly.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// The client address, if known.
    pub peer: Option<std::net::SocketAddr>,
}

impl HttpRequest {
    /// A header's value; repeated headers are joined with `,` (as ScyllaDB's HTTP server does).
    pub fn header(&self, name: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case(name) {
                match &mut out {
                    Some(s) => {
                        s.push(',');
                        s.push_str(v);
                    }
                    None => out = Some(v.clone()),
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HttpResponse {
    pub fn new(status: u16, content_type: &str, body: impl Into<Bytes>) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }

    pub fn json(status: u16, body: &Json) -> HttpResponse {
        HttpResponse::new(status, JSON_CONTENT_TYPE, body.to_json_string())
    }

    pub fn error(e: &Error) -> HttpResponse {
        HttpResponse::json(e.status(), &e.to_json())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub const JSON_CONTENT_TYPE: &str = "application/x-amz-json-1.0";

/// Maximum request body, before and after decompression.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// What a request is for.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Api,
    Health,
    Options,
    LocalNodes,
    NotFound,
}

/// Routes by method and path as ScyllaDB's Alternator server does.
pub fn route(method: &str, path: &str) -> Route {
    let mut p = path.split(['?', '#']).next().unwrap_or("");
    if p.len() > 1 && p.ends_with('/') {
        p = &p[..p.len() - 1];
    }
    let known = [
        "POST", "OPTIONS", "HEAD", "PUT", "DELETE", "PATCH", "TRACE", "CONNECT",
    ];
    let is_get = method == "GET"
        || (!known.contains(&method)
            && !method.is_empty()
            && method.bytes().all(|b| b.is_ascii_uppercase()));
    match p {
        "/" if method == "POST" => Route::Api,
        "/" if method == "OPTIONS" => Route::Options,
        "/" if is_get => Route::Health,
        "/localnodes" if is_get || method == "OPTIONS" => Route::LocalNodes,
        _ => Route::NotFound,
    }
}

pub fn not_found() -> HttpResponse {
    HttpResponse::new(
        404,
        "application/json",
        r#"{"message": "Not found", "code": 404}"#,
    )
}

pub fn payload_too_large() -> Error {
    Error::new(
        ErrorKind::PayloadTooLarge,
        format!("Request content length limit of {MAX_REQUEST_BYTES} bytes exceeded"),
    )
}

/// The operation named by `X-Amz-Target`.
pub fn operation(target: Option<&str>) -> String {
    let t = target.unwrap_or("").trim();
    match t.split_once('.') {
        Some((_, op)) => op.to_string(),
        None => String::new(),
    }
}

/// Decodes the request body per `Content-Encoding`.
pub fn decompress(encoding: Option<&str>, body: Bytes) -> Result<Bytes, Error> {
    let enc = encoding.unwrap_or("").trim();
    if enc.is_empty() {
        return Ok(body);
    }
    let gzip = match enc {
        "gzip" => true,
        "deflate" => false,
        _ => return Err(Error::internal("Unsupported Content-Encoding")),
    };
    let truncated = || Error::internal("Truncated gzip in request body");
    let failed = || Error::internal("Error during gzip decompression of request body");
    if body.is_empty() {
        return Err(truncated());
    }
    let mut out = Vec::new();
    let mut rest: &[u8] = &body;
    while !rest.is_empty() {
        // A member must start with the right framing (gzip magic, or a zlib header).
        let framed = if gzip {
            rest.len() < 2 || rest[..2] == [0x1f, 0x8b]
        } else {
            rest.len() < 2
                || (rest[0] & 0x0f == 8 && (u16::from(rest[0]) << 8 | u16::from(rest[1])) % 31 == 0)
        };
        if !framed {
            return Err(failed());
        }
        // One member at a time, so concatenated members work and trailing junk is detected.
        let before = rest.len();
        let limit = (MAX_REQUEST_BYTES + 1 - out.len().min(MAX_REQUEST_BYTES)) as u64;
        let res = if gzip {
            let mut d = flate2::bufread::GzDecoder::new(rest);
            let r = (&mut d).take(limit).read_to_end(&mut out);
            let consumed = before - d.into_inner().len();
            (r, consumed)
        } else {
            let mut d = flate2::bufread::ZlibDecoder::new(rest);
            let r = (&mut d).take(limit).read_to_end(&mut out);
            let consumed = d.total_in() as usize;
            (r, consumed)
        };
        match res {
            (Ok(_), consumed) => {
                if out.len() > MAX_REQUEST_BYTES {
                    return Err(payload_too_large());
                }
                if consumed == 0 {
                    return Err(failed());
                }
                rest = &rest[consumed.min(rest.len())..];
            }
            (Err(e), _) => {
                return Err(if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    truncated()
                } else {
                    failed()
                });
            }
        }
    }
    Ok(out.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Gzip,
    Deflate,
}

/// The `q=` value of one Accept-Encoding entry's parameters, parsed like `strtod`: garbage or
/// absence is 1, values above 1 are 1, negative or NaN is 0.
fn q_value(params: &str) -> (f64, bool) {
    let Some(i) = params.find("q=") else {
        return (1.0, false);
    };
    let v = params[i + 2..].split(';').next().unwrap_or("").trim();
    let q = match v.parse::<f64>() {
        Ok(q) => q,
        Err(_) => return (1.0, true),
    };
    if q.is_nan() || q < 0.0 {
        (0.0, true)
    } else {
        (q.min(1.0), true)
    }
}

/// Picks the response encoding from (possibly repeated, already joined) Accept-Encoding.
pub fn choose_encoding(accept: &str) -> Option<Encoding> {
    // Per encoding: (q, explicitly listed, position).
    let mut gzip: Option<(f64, bool, usize)> = None;
    let mut deflate: Option<(f64, bool, usize)> = None;
    let mut star: Option<(f64, usize)> = None;
    for (pos, entry) in accept.split(',').enumerate() {
        let entry = entry.trim();
        let (name, params) = entry.split_once(';').unwrap_or((entry, ""));
        let name = name.trim().to_ascii_lowercase();
        let (q, explicit_q) = q_value(params);
        let slot = match name.as_str() {
            "gzip" => &mut gzip,
            "deflate" => &mut deflate,
            "*" => {
                if star.is_none() {
                    star = Some((q, pos));
                }
                continue;
            }
            _ => continue,
        };
        match slot {
            None => *slot = Some((q, true, pos)),
            Some((old, _, _)) if *old == 0.0 && explicit_q && q > 0.0 => {
                *slot = Some((q, true, pos))
            }
            _ => {}
        }
    }
    if let Some((q, pos)) = star {
        if gzip.is_none() {
            gzip = Some((q, false, pos));
        }
        if deflate.is_none() {
            deflate = Some((q, false, pos));
        }
    }
    let mut best: Option<(Encoding, (f64, bool, usize))> = None;
    for (enc, c) in [(Encoding::Gzip, gzip), (Encoding::Deflate, deflate)] {
        let Some(c) = c else { continue };
        if c.0 <= 0.0 {
            continue;
        }
        let better = match &best {
            None => true,
            Some((_, b)) => {
                c.0 > b.0 || (c.0 == b.0 && ((c.1 && !b.1) || (c.1 == b.1 && c.1 && c.2 < b.2)))
            }
        };
        if better {
            best = Some((enc, c));
        }
    }
    best.map(|(e, _)| e)
}

/// Compresses a response body (zlib level 6; gzip with mtime 0 and OS 3, as zlib writes it).
pub fn compress(enc: Encoding, level: u32, body: &[u8]) -> Vec<u8> {
    let level = flate2::Compression::new(level.min(9));
    match enc {
        Encoding::Gzip => {
            let mut e = flate2::GzBuilder::new()
                .operating_system(3)
                .write(Vec::new(), level);
            let _ = e.write_all(body);
            e.finish().unwrap_or_default()
        }
        Encoding::Deflate => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), level);
            let _ = e.write_all(body);
            e.finish().unwrap_or_default()
        }
    }
}

/// Adds the CORS headers ScyllaDB adds for a non-empty `Origin`.
pub fn add_cors(req: &HttpRequest, resp: &mut HttpResponse, preflight: bool) {
    if req.header("origin").is_none_or(|o| o.is_empty()) {
        return;
    }
    resp.headers
        .push(("Access-Control-Allow-Origin".into(), "*".into()));
    resp.headers.push((
        "Access-Control-Expose-Headers".into(),
        "x-amzn-RequestId,x-amzn-ErrorType,x-amzn-ErrorMessage,Date".into(),
    ));
    if preflight {
        if let Some(m) = req.header("access-control-request-method") {
            resp.headers
                .push(("Access-Control-Allow-Methods".into(), m));
        }
        if let Some(h) = req.header("access-control-request-headers") {
            resp.headers
                .push(("Access-Control-Allow-Headers".into(), h));
        }
        resp.headers
            .push(("Access-Control-Max-Age".into(), "7200".into()));
    }
}

/// `dc=` and `rack=` of a `/localnodes` query string (empty values are ignored).
pub fn localnodes_filter(query: &str) -> (Option<String>, Option<String>) {
    let (mut dc, mut rack) = (None, None);
    for kv in query.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = percent_decode(v);
        if v.is_empty() {
            continue;
        }
        match k {
            "dc" => dc = Some(v),
            "rack" => rack = Some(v),
            _ => {}
        }
    }
    (dc, rack)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The HTTP `Date` header value for now.
pub fn http_date() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("POST", "/"), Route::Api);
        assert_eq!(route("POST", "//"), Route::Api);
        assert_eq!(route("GET", "/"), Route::Health);
        assert_eq!(route("BREW", "/"), Route::Health);
        assert_eq!(route("get", "/"), Route::NotFound);
        assert_eq!(route("OPTIONS", "/"), Route::Options);
        assert_eq!(route("GET", "/localnodes"), Route::LocalNodes);
        assert_eq!(route("GET", "/localnodes/"), Route::LocalNodes);
        assert_eq!(route("OPTIONS", "/localnodes"), Route::LocalNodes);
        assert_eq!(route("POST", "/localnodes"), Route::NotFound);
        assert_eq!(route("GET", "//localnodes"), Route::NotFound);
        assert_eq!(route("GET", "/LOCALNODES"), Route::NotFound);
        assert_eq!(route("HEAD", "/"), Route::NotFound);
        assert_eq!(route("PUT", "/"), Route::NotFound);
        assert_eq!(route("GET", "/foo"), Route::NotFound);
    }

    #[test]
    fn targets() {
        assert_eq!(
            operation(Some("DynamoDB_20120810.ListTables")),
            "ListTables"
        );
        assert_eq!(operation(Some("Foo.ListTables")), "ListTables");
        assert_eq!(operation(Some(".ListTables")), "ListTables");
        assert_eq!(
            operation(Some("  DynamoDB_20120810.ListTables\t")),
            "ListTables"
        );
        assert_eq!(operation(Some("ListTables")), "");
        assert_eq!(operation(None), "");
        assert_eq!(operation(Some("a.b.c")), "b.c");
    }

    #[test]
    fn accept_encoding() {
        use Encoding::*;
        assert_eq!(choose_encoding("gzip"), Some(Gzip));
        assert_eq!(choose_encoding("deflate"), Some(Deflate));
        assert_eq!(choose_encoding("GZIP"), Some(Gzip));
        assert_eq!(choose_encoding("br, identity"), None);
        assert_eq!(choose_encoding("gzip;q=0"), None);
        assert_eq!(choose_encoding("gzip;q=0, gzip"), None);
        assert_eq!(choose_encoding("gzip;q=0, gzip;q=1"), Some(Gzip));
        assert_eq!(choose_encoding("gzip;q=0.5, deflate"), Some(Deflate));
        assert_eq!(choose_encoding("deflate, gzip"), Some(Deflate));
        assert_eq!(choose_encoding("*"), Some(Gzip));
        assert_eq!(choose_encoding("*, deflate"), Some(Deflate));
        assert_eq!(choose_encoding("gzip;nq=0"), None);
        assert_eq!(choose_encoding("gzip;Q=0"), Some(Gzip));
        assert_eq!(choose_encoding("gzip;q=abc"), Some(Gzip));
        assert_eq!(choose_encoding("gzip;q=-1"), None);
        assert_eq!(choose_encoding("gzip;q=5, deflate;q=0.9"), Some(Gzip));
        assert_eq!(choose_encoding(""), None);
    }

    #[test]
    fn decompression() {
        let body = br#"{"a":1}"#;
        let gz = compress(Encoding::Gzip, 6, body);
        assert_eq!(&gz[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(
            &decompress(Some("gzip"), gz.clone().into()).unwrap()[..],
            body
        );
        let two = [gz.clone(), gz.clone()].concat();
        assert_eq!(
            &decompress(Some("gzip"), two.into()).unwrap()[..],
            br#"{"a":1}{"a":1}"#
        );
        let z = compress(Encoding::Deflate, 6, body);
        assert_eq!(&z[..2], &[0x78, 0x9c]);
        assert_eq!(
            &decompress(Some(" deflate "), z.clone().into()).unwrap()[..],
            body
        );
        let msg = |r: Result<Bytes, Error>| r.unwrap_err().message;
        assert_eq!(
            msg(decompress(Some("br"), Bytes::new())),
            "Unsupported Content-Encoding"
        );
        assert_eq!(
            msg(decompress(Some("GZIP"), Bytes::new())),
            "Unsupported Content-Encoding"
        );
        assert_eq!(
            msg(decompress(Some("gzip"), Bytes::new())),
            "Truncated gzip in request body"
        );
        assert_eq!(
            msg(decompress(Some("gzip"), Bytes::from_static(b"{}"))),
            "Error during gzip decompression of request body"
        );
        assert_eq!(
            msg(decompress(Some("gzip"), gz[..gz.len() - 1].to_vec().into())),
            "Truncated gzip in request body"
        );
        let junk = [gz.clone(), b"xx".to_vec()].concat();
        assert_eq!(
            msg(decompress(Some("gzip"), junk.into())),
            "Error during gzip decompression of request body"
        );
        assert_eq!(
            msg(decompress(Some("deflate"), gz.into())),
            "Error during gzip decompression of request body"
        );
        assert_eq!(
            decompress(Some(""), Bytes::from_static(b"x")).unwrap(),
            Bytes::from_static(b"x")
        );
        // A decompression bomb stops at the limit.
        let big = compress(Encoding::Gzip, 6, &vec![b' '; MAX_REQUEST_BYTES + 1]);
        assert_eq!(
            decompress(Some("gzip"), big.into()).unwrap_err().kind,
            ErrorKind::PayloadTooLarge
        );
        let exact = compress(Encoding::Gzip, 6, &vec![b' '; MAX_REQUEST_BYTES]);
        assert_eq!(
            decompress(Some("gzip"), exact.into()).unwrap().len(),
            MAX_REQUEST_BYTES
        );
    }
}
