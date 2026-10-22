//! HTTP-level S3 rules as pure functions: ranges, conditional requests, user metadata,
//! `response-*` overrides, ETags, bucket names and keys, URI encoding, copy sources, tags.

use crate::error::{Code, S3Error};
use std::collections::BTreeMap;

/// Longest key, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;
/// User metadata limit: keys (without `x-amz-meta-`) plus values, in bytes.
pub const MAX_METADATA_BYTES: usize = 2048;
pub const META_PREFIX: &str = "x-amz-meta-";

/// `%XX`-encode everything but `A-Z a-z 0-9 - _ . ~` (and `/` unless `encode_slash`), as SigV4
/// and S3 URLs want.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    uri_encode_bytes(s.as_bytes(), encode_slash)
}

pub fn uri_encode_bytes(b: &[u8], encode_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(b.len() + b.len() / 2);
    for &c in b {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') || (c == b'/' && !encode_slash) {
            out.push(c as char);
        } else {
            out.push('%');
            out.push(HEX[(c >> 4) as usize] as char);
            out.push(HEX[(c & 15) as usize] as char);
        }
    }
    out
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode `%XX` escapes (a `%` not followed by two hex digits stays as is; `+` stays `+`).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// [`percent_decode`] into UTF-8 (`InvalidArgument` if it isn't).
pub fn percent_decode_utf8(s: &str) -> Result<String, S3Error> {
    String::from_utf8(percent_decode(s)).map_err(|_| S3Error::msg(Code::InvalidArgument, "URL-decoded value is not valid UTF-8"))
}

/// A raw query string as decoded `(name, value)` pairs, in order. `+` is literal, like the
/// SigV4 canonicalisation treats it.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (String::from_utf8_lossy(&percent_decode(k)).into_owned(), String::from_utf8_lossy(&percent_decode(v)).into_owned())
        })
        .collect()
}

/// `encoding-type=url` in listings: like a query component (space as `+`), keeping `/`.
pub fn url_encode_listing(s: &str) -> String {
    s.split(' ').map(|part| uri_encode(part, false)).collect::<Vec<_>>().join("+")
}

/// `"<etag>"`.
pub fn quote_etag(etag: &str) -> String {
    format!("\"{}\"", unquote_etag(etag))
}

/// An ETag without quotes or a weak `W/` prefix.
pub fn unquote_etag(etag: &str) -> &str {
    let e = etag.trim();
    let e = e.strip_prefix("W/").unwrap_or(e);
    e.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(e)
}

fn etag_list_matches(list: &str, etag: &str) -> bool {
    let etag = unquote_etag(etag);
    list.split(',').map(str::trim).any(|e| e == "*" || unquote_etag(e) == etag)
}

/// S3 general-purpose bucket naming rules.
pub fn valid_bucket_name(name: &str) -> bool {
    let b = name.as_bytes();
    if !(3..=63).contains(&b.len()) {
        return false;
    }
    if !b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.') {
        return false;
    }
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !edge(b[0]) || !edge(b[b.len() - 1]) {
        return false;
    }
    if name.contains("..") || name.contains(".-") || name.contains("-.") {
        return false;
    }
    // Not formatted as an IP address.
    if name.split('.').count() == 4 && name.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    const PREFIXES: [&str; 4] = ["xn--", "sthree-", "amzn-s3-demo-", "sthree-configurator"];
    const SUFFIXES: [&str; 5] = ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"];
    !PREFIXES.iter().any(|p| name.starts_with(p)) && !SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Keys: 1–1024 bytes.
pub fn validate_key(key: &str) -> Result<(), S3Error> {
    if key.is_empty() {
        return Err(S3Error::msg(Code::InvalidArgument, "Key must not be empty"));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(S3Error::new(Code::KeyTooLongError).with("Size", key.len().to_string()).with("MaxSizeAllowed", MAX_KEY_BYTES.to_string()));
    }
    Ok(())
}

/// The satisfiable `Range` of an object of `size` bytes as inclusive `(first, last)`. `None`
/// means "send it all": no header, a header S3 ignores (malformed, or several ranges).
/// `InvalidRange` (416) when it starts past the end.
pub fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, S3Error> {
    let Some(h) = header else { return Ok(None) };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Ok(None) };
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let unsatisfiable = || Err(S3Error::new(Code::InvalidRange).with("RangeRequested", h.trim().to_string()).with("ActualObjectSize", size.to_string()));
    if a.is_empty() {
        if !digits(b) {
            return Ok(None);
        }
        let Ok(n) = b.parse::<u64>() else { return Ok(None) };
        if n == 0 || size == 0 {
            return unsatisfiable();
        }
        return Ok(Some((size.saturating_sub(n), size - 1)));
    }
    if !digits(a) || !(b.is_empty() || digits(b)) {
        return Ok(None);
    }
    let Ok(first) = a.parse::<u64>() else { return Ok(None) };
    let last = if b.is_empty() {
        u64::MAX
    } else {
        b.parse::<u64>().unwrap_or(u64::MAX)
    };
    if last < first {
        return Ok(None);
    }
    if first >= size {
        return unsatisfiable();
    }
    Ok(Some((first, last.min(size - 1))))
}

/// `Content-Range: bytes first-last/size`.
pub fn content_range(first: u64, last: u64, size: u64) -> String {
    format!("bytes {first}-{last}/{size}")
}

/// `x-amz-copy-source-range: bytes=first-last` (both required, within the source).
pub fn parse_copy_source_range(header: &str, size: u64) -> Result<(u64, u64), S3Error> {
    let bad = || S3Error::msg(Code::InvalidArgument, "The x-amz-copy-source-range value must be of the form bytes=first-last where first and last are the zero-based offsets of the first and last bytes to copy");
    let spec = header.trim().strip_prefix("bytes=").ok_or_else(bad)?;
    let (a, b) = spec.split_once('-').ok_or_else(bad)?;
    let first: u64 = a.trim().parse().map_err(|_| bad())?;
    let last: u64 = b.trim().parse().map_err(|_| bad())?;
    if last < first {
        return Err(bad());
    }
    if last >= size {
        return Err(S3Error::msg(Code::InvalidRange, "The requested range is not satisfiable").with("RangeRequested", header.trim().to_string()).with("ActualObjectSize", size.to_string()));
    }
    Ok((first, last))
}

/// The byte range of part `n` (1-based) of a multipart object, inclusive.
pub fn part_range(part_sizes: &[u64], n: u32) -> Option<(u64, u64)> {
    let idx = (n as usize).checked_sub(1)?;
    let size = *part_sizes.get(idx)?;
    let start: u64 = part_sizes[..idx].iter().sum();
    if size == 0 {
        return None;
    }
    Some((start, start + size - 1))
}

/// `x-amz-copy-source`: `[/]bucket/key[?versionId=…]`, percent-encoded. Only the `null`
/// version exists here.
pub fn parse_copy_source(header: &str) -> Result<(String, String), S3Error> {
    let bad = || S3Error::msg(Code::InvalidArgument, "Copy Source must mention the source bucket and key: sourcebucket/sourcekey").with("ArgumentName", "x-amz-copy-source");
    let raw = header.trim();
    let (path, query) = raw.split_once('?').unwrap_or((raw, ""));
    for (k, v) in parse_query(query) {
        if k == "versionId" && v != "null" {
            return Err(S3Error::msg(Code::NoSuchKey, "The specified version does not exist.").with("VersionId", v));
        }
    }
    let decoded = percent_decode_utf8(path)?;
    let p = decoded.strip_prefix('/').unwrap_or(&decoded);
    let (bucket, key) = p.split_once('/').ok_or_else(bad)?;
    if bucket.is_empty() || key.is_empty() {
        return Err(bad());
    }
    Ok((bucket.to_string(), key.to_string()))
}

/// `x-amz-copy-source` with its version: `[/]bucket/key[?versionId=…]` → `(bucket, key,
/// version id)`. The version id is checked by [`crate::versioning::valid_version_id`].
pub fn parse_copy_source_version(header: &str) -> Result<(String, String, Option<String>), S3Error> {
    let raw = header.trim();
    let (path, query) = raw.split_once('?').unwrap_or((raw, ""));
    let mut version = None;
    for (k, v) in parse_query(query) {
        if k == "versionId" {
            if !crate::versioning::valid_version_id(&v) {
                return Err(S3Error::msg(Code::InvalidArgument, "Invalid version id specified").with("ArgumentName", "x-amz-copy-source").with("ArgumentValue", v));
            }
            version = Some(v);
        }
    }
    let (bucket, key) = parse_copy_source(path)?;
    Ok((bucket, key, version))
}

/// Conditional request headers. Dates are Unix seconds; unparseable dates are ignored, as
/// HTTP says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conditions {
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<i64>,
    pub if_unmodified_since: Option<i64>,
}

impl Conditions {
    /// `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`.
    pub fn from_headers<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        Self::read(get, "")
    }

    /// The `x-amz-copy-source-if-*` headers of a copy.
    pub fn copy_source_from_headers<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        Self::read(get, "x-amz-copy-source-")
    }

    fn read<'a>(get: impl Fn(&str) -> Option<&'a str>, prefix: &str) -> Self {
        let h = |n: &str| get(&format!("{prefix}{n}")).map(str::trim).filter(|s| !s.is_empty());
        Self {
            if_match: h("if-match").map(str::to_string),
            if_none_match: h("if-none-match").map(str::to_string),
            if_modified_since: h("if-modified-since").and_then(crate::time::parse_http_date),
            if_unmodified_since: h("if-unmodified-since").and_then(crate::time::parse_http_date),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `Err(412)` or `Err(304)`: GET/HEAD semantics (RFC 7232, as S3 applies it): a true
    /// `If-Match` overrides `If-Unmodified-Since`, a false `If-None-Match` overrides
    /// `If-Modified-Since`.
    pub fn check_read(&self, etag: &str, last_modified_ms: i64) -> Result<(), S3Error> {
        match self.evaluate(etag, last_modified_ms) {
            Outcome::Ok => Ok(()),
            Outcome::Failed => Err(S3Error::new(Code::PreconditionFailed).with("Condition", self.failed_condition(etag, last_modified_ms))),
            Outcome::NotModified => Err(S3Error::new(Code::NotModified)),
        }
    }

    /// Copy-source conditions: like reads, but every failure is a 412.
    pub fn check_copy_source(&self, etag: &str, last_modified_ms: i64) -> Result<(), S3Error> {
        match self.evaluate(etag, last_modified_ms) {
            Outcome::Ok => Ok(()),
            _ => Err(S3Error::new(Code::PreconditionFailed)),
        }
    }

    /// Conditional writes: `If-None-Match: *` (fails if the key exists) and `If-Match: <etag>`
    /// (fails if it doesn't, 404, or holds something else, 412). `current` is the ETag of the
    /// object there now.
    pub fn check_write(&self, current: Option<&str>) -> Result<(), S3Error> {
        if let Some(inm) = &self.if_none_match {
            if inm.trim() != "*" {
                return Err(S3Error::msg(Code::NotImplemented, "If-None-Match only supports '*' on writes").with("Header", "If-None-Match"));
            }
            if current.is_some() {
                return Err(S3Error::new(Code::PreconditionFailed).with("Condition", "If-None-Match"));
            }
        }
        if let Some(im) = &self.if_match {
            match current {
                None => return Err(S3Error::new(Code::NoSuchKey)),
                Some(e) if !etag_list_matches(im, e) => return Err(S3Error::new(Code::PreconditionFailed).with("Condition", "If-Match")),
                _ => {}
            }
        }
        Ok(())
    }

    fn evaluate(&self, etag: &str, last_modified_ms: i64) -> Outcome {
        let lm = last_modified_ms.div_euclid(1000);
        match &self.if_match {
            Some(m) if !etag_list_matches(m, etag) => return Outcome::Failed,
            Some(_) => {}
            None => {
                if let Some(t) = self.if_unmodified_since
                    && lm > t
                {
                    return Outcome::Failed;
                }
            }
        }
        match &self.if_none_match {
            Some(m) if etag_list_matches(m, etag) => return Outcome::NotModified,
            Some(_) => {}
            None => {
                if let Some(t) = self.if_modified_since
                    && lm <= t
                {
                    return Outcome::NotModified;
                }
            }
        }
        Outcome::Ok
    }

    fn failed_condition(&self, etag: &str, _last_modified_ms: i64) -> &'static str {
        match &self.if_match {
            Some(m) if !etag_list_matches(m, etag) => "If-Match",
            _ => "If-Unmodified-Since",
        }
    }
}

enum Outcome {
    Ok,
    Failed,
    NotModified,
}

/// User metadata from `x-amz-meta-*` headers: names lowercased without the prefix, repeated
/// headers joined with `,`. `MetadataTooLarge` past 2 KB.
pub fn user_metadata<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<BTreeMap<String, String>, S3Error> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        // POST form field names are arbitrary UTF-8: never slice inside a character.
        if !name.get(..META_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(META_PREFIX)) {
            continue;
        }
        let key = name[META_PREFIX.len()..].to_ascii_lowercase();
        let value = value.trim();
        out.entry(key)
            .and_modify(|v| {
                v.push(',');
                v.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }
    let size: usize = out.iter().map(|(k, v)| k.len() + v.len()).sum();
    if size > MAX_METADATA_BYTES {
        return Err(S3Error::new(Code::MetadataTooLarge).with("Size", size.to_string()).with("MaxSizeAllowed", MAX_METADATA_BYTES.to_string()));
    }
    Ok(out)
}

/// `response-*` query parameters of a GET, overriding response headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub content_type: Option<String>,
    pub content_language: Option<String>,
    pub expires: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
}

impl Overrides {
    pub fn from_query<'a>(query: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut o = Self::default();
        for (k, v) in query {
            let slot = match k {
                "response-content-type" => &mut o.content_type,
                "response-content-language" => &mut o.content_language,
                "response-expires" => &mut o.expires,
                "response-cache-control" => &mut o.cache_control,
                "response-content-disposition" => &mut o.content_disposition,
                "response-content-encoding" => &mut o.content_encoding,
                _ => continue,
            };
            *slot = Some(v.to_string());
        }
        o
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Maximum tags per object, and key/value lengths in characters.
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG_KEY: usize = 128;
pub const MAX_TAG_VALUE: usize = 256;

/// Validate an object tag set.
pub fn validate_tags(tags: &[(String, String)]) -> Result<(), S3Error> {
    if tags.len() > MAX_TAGS {
        return Err(S3Error::msg(Code::InvalidTag, "Object tags cannot be greater than 10"));
    }
    let mut seen = std::collections::HashSet::new();
    for (k, v) in tags {
        if k.is_empty() || k.chars().count() > MAX_TAG_KEY {
            return Err(S3Error::msg(Code::InvalidTag, "The TagKey you have provided is invalid"));
        }
        if v.chars().count() > MAX_TAG_VALUE {
            return Err(S3Error::msg(Code::InvalidTag, "The TagValue you have provided is invalid"));
        }
        if k.starts_with("aws:") {
            return Err(S3Error::msg(Code::InvalidTag, "Your TagKey cannot be prefixed with aws:"));
        }
        if !seen.insert(k.as_str()) {
            return Err(S3Error::msg(Code::InvalidTag, "Cannot provide multiple Tags with the same key"));
        }
    }
    Ok(())
}

/// `x-amz-tagging: k1=v1&k2=v2` (URL-encoded).
pub fn parse_tagging_header(h: &str) -> Result<Vec<(String, String)>, S3Error> {
    let tags = parse_query(h.trim());
    validate_tags(&tags)?;
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert_eq!(uri_encode("a b/c+d~é", false), "a%20b/c%2Bd~%C3%A9");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(percent_decode("a%20b%2fc%zz%4"), b"a b/c%zz%4");
        assert_eq!(percent_decode("a+b"), b"a+b");
        assert_eq!(parse_query("a=1&b&c=x%20y&&d=%2B+"), vec![("a".into(), "1".into()), ("b".into(), "".into()), ("c".into(), "x y".into()), ("d".into(), "++".into())]);
        assert_eq!(url_encode_listing("a b/c+d"), "a+b/c%2Bd");
        assert_eq!(url_encode_listing(" x "), "+x+");
        assert_eq!(quote_etag("abc"), "\"abc\"");
        assert_eq!(quote_etag("\"abc\""), "\"abc\"");
        assert_eq!(unquote_etag("W/\"abc\""), "abc");
    }

    #[test]
    fn bucket_names() {
        for ok in ["abc", "my-bucket.v2", "a1b", "123", "x".repeat(63).as_str()] {
            assert!(valid_bucket_name(ok), "{ok}");
        }
        for bad in ["ab", "Abc", "-abc", "abc-", "a..b", "a.-b", "192.168.5.4", "xn--abc", "abc-s3alias", "a_b", &"x".repeat(64)] {
            assert!(!valid_bucket_name(bad), "{bad}");
        }
        validate_key("k").unwrap();
        assert_eq!(validate_key("").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(validate_key(&"k".repeat(1025)).unwrap_err().code, Code::KeyTooLongError);
    }

    #[test]
    fn ranges() {
        let r = |h: &str, size: u64| parse_range(Some(h), size).map_err(|e| e.code);
        assert_eq!(parse_range(None, 10).unwrap(), None);
        assert_eq!(r("bytes=0-9", 100), Ok(Some((0, 9))));
        assert_eq!(r("bytes=90-200", 100), Ok(Some((90, 99))));
        assert_eq!(r("bytes=10-", 100), Ok(Some((10, 99))));
        assert_eq!(r("bytes=-10", 100), Ok(Some((90, 99))));
        assert_eq!(r("bytes=-1000", 100), Ok(Some((0, 99))));
        assert_eq!(r("bytes=100-", 100), Err(Code::InvalidRange));
        assert_eq!(r("bytes=100-200", 100), Err(Code::InvalidRange));
        assert_eq!(r("bytes=-0", 100), Err(Code::InvalidRange));
        assert_eq!(r("bytes=0-", 0), Err(Code::InvalidRange));
        assert_eq!(r("bytes=-5", 0), Err(Code::InvalidRange));
        // Ignored, as S3 does.
        assert_eq!(r("bytes=5-1", 100), Ok(None));
        assert_eq!(r("bytes=0-1,5-6", 100), Ok(None));
        assert_eq!(r("items=0-1", 100), Ok(None));
        assert_eq!(r("bytes=a-b", 100), Ok(None));
        assert_eq!(r("bytes=0-99999999999999999999999", 100), Ok(Some((0, 99))));
        assert_eq!(content_range(0, 9, 100), "bytes 0-9/100");
        assert_eq!(parse_copy_source_range("bytes=0-9", 10).unwrap(), (0, 9));
        assert_eq!(parse_copy_source_range("bytes=0-10", 10).unwrap_err().code, Code::InvalidRange);
        assert_eq!(parse_copy_source_range("bytes=5-", 10).unwrap_err().code, Code::InvalidArgument);
        assert_eq!(part_range(&[5, 3, 2], 1), Some((0, 4)));
        assert_eq!(part_range(&[5, 3, 2], 3), Some((8, 9)));
        assert_eq!(part_range(&[5, 3, 2], 4), None);
        assert_eq!(part_range(&[5, 3, 2], 0), None);
    }

    #[test]
    fn copy_source() {
        assert_eq!(parse_copy_source("/b/k%20x/y").unwrap(), ("b".into(), "k x/y".into()));
        assert_eq!(parse_copy_source("b/k?versionId=null").unwrap(), ("b".into(), "k".into()));
        assert_eq!(parse_copy_source("b/k?versionId=abc").unwrap_err().code, Code::NoSuchKey);
        let vid = "0000000000000000abcdefghijklmnop";
        assert_eq!(parse_copy_source_version(&format!("/b/k%20x?versionId={vid}")).unwrap(), ("b".into(), "k x".into(), Some(vid.into())));
        assert_eq!(parse_copy_source_version("b/k?versionId=null").unwrap(), ("b".into(), "k".into(), Some("null".into())));
        assert_eq!(parse_copy_source_version("b/k").unwrap(), ("b".into(), "k".into(), None));
        assert_eq!(parse_copy_source_version("b/k?versionId=abc").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(parse_copy_source_version("b?versionId=null").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(parse_copy_source("b").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(parse_copy_source("/b/").unwrap_err().code, Code::InvalidArgument);
    }

    #[test]
    fn conditions() {
        let t = 1_445_412_480; // Wed, 21 Oct 2015 07:28:00 GMT
        let lm = t * 1000 + 500;
        let c = |im: Option<&str>, inm: Option<&str>, ims: Option<i64>, ius: Option<i64>| Conditions {
            if_match: im.map(Into::into),
            if_none_match: inm.map(Into::into),
            if_modified_since: ims,
            if_unmodified_since: ius,
        };
        let read = |c: Conditions| c.check_read("\"abc\"", lm).map_err(|e| e.code);
        assert_eq!(read(c(None, None, None, None)), Ok(()));
        assert_eq!(read(c(Some("\"abc\""), None, None, None)), Ok(()));
        assert_eq!(read(c(Some("abc"), None, None, None)), Ok(()));
        assert_eq!(read(c(Some("*"), None, None, None)), Ok(()));
        assert_eq!(read(c(Some("\"x\", \"abc\""), None, None, None)), Ok(()));
        assert_eq!(read(c(Some("\"x\""), None, None, None)), Err(Code::PreconditionFailed));
        assert_eq!(read(c(None, Some("\"abc\""), None, None)), Err(Code::NotModified));
        assert_eq!(read(c(None, Some("\"x\""), None, None)), Ok(()));
        assert_eq!(read(c(None, None, Some(t), None)), Err(Code::NotModified));
        assert_eq!(read(c(None, None, Some(t - 1), None)), Ok(()));
        assert_eq!(read(c(None, None, None, Some(t))), Ok(()));
        assert_eq!(read(c(None, None, None, Some(t - 1))), Err(Code::PreconditionFailed));
        // If-Match true overrides If-Unmodified-Since false.
        assert_eq!(read(c(Some("abc"), None, None, Some(t - 1))), Ok(()));
        // If-None-Match false overrides If-Modified-Since true.
        assert_eq!(read(c(None, Some("x"), Some(t), None)), Ok(()));
        // If-Match fails before If-None-Match is looked at.
        assert_eq!(read(c(Some("x"), Some("abc"), None, None)), Err(Code::PreconditionFailed));
        assert_eq!(c(None, Some("abc"), None, None).check_copy_source("abc", lm).unwrap_err().code, Code::PreconditionFailed);

        let w = |c: Conditions, cur: Option<&str>| c.check_write(cur).map_err(|e| e.code);
        assert_eq!(w(c(None, Some("*"), None, None), None), Ok(()));
        assert_eq!(w(c(None, Some("*"), None, None), Some("abc")), Err(Code::PreconditionFailed));
        assert_eq!(w(c(None, Some("\"abc\""), None, None), None), Err(Code::NotImplemented));
        assert_eq!(w(c(Some("abc"), None, None, None), None), Err(Code::NoSuchKey));
        assert_eq!(w(c(Some("abc"), None, None, None), Some("\"abc\"")), Ok(()));
        assert_eq!(w(c(Some("abc"), None, None, None), Some("def")), Err(Code::PreconditionFailed));

        let hdrs = [("If-Match", "\"a\""), ("If-Modified-Since", "Wed, 21 Oct 2015 07:28:00 GMT"), ("if-unmodified-since", "nonsense")];
        let get = |n: &str| hdrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| *v);
        assert_eq!(Conditions::from_headers(get), c(Some("\"a\""), None, Some(t), None));
        let hdrs = [("x-amz-copy-source-if-none-match", "\"a\"")];
        let get = |n: &str| hdrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| *v);
        assert_eq!(Conditions::copy_source_from_headers(get), c(None, Some("\"a\""), None, None));
    }

    #[test]
    fn metadata_and_overrides() {
        let m = user_metadata([("X-Amz-Meta-Color", "red"), ("content-type", "x"), ("x-amz-meta-color", "blue "), ("x-amz-meta-Size", " 1 ")]).unwrap();
        assert_eq!(m.get("color").unwrap(), "red,blue");
        assert_eq!(m.get("size").unwrap(), "1");
        assert_eq!(m.len(), 2);
        let big = "v".repeat(2048);
        assert_eq!(user_metadata([("x-amz-meta-k", big.as_str())]).unwrap_err().code, Code::MetadataTooLarge);
        user_metadata([("x-amz-meta-k", &big[..2047])]).unwrap();
        let o = Overrides::from_query([("response-content-type", "text/plain"), ("x", "y"), ("response-expires", "now")]);
        assert_eq!(o.content_type.as_deref(), Some("text/plain"));
        assert_eq!(o.expires.as_deref(), Some("now"));
        assert!(Overrides::from_query([]).is_empty());
    }

    #[test]
    fn tags() {
        assert_eq!(parse_tagging_header("a=1&b=x%20y").unwrap(), vec![("a".into(), "1".into()), ("b".into(), "x y".into())]);
        assert_eq!(parse_tagging_header("a=1&a=2").unwrap_err().code, Code::InvalidTag);
        assert_eq!(parse_tagging_header("aws:x=1").unwrap_err().code, Code::InvalidTag);
        let many: Vec<String> = (0..11).map(|i| format!("k{i}=v")).collect();
        assert_eq!(parse_tagging_header(&many.join("&")).unwrap_err().code, Code::InvalidTag);
        assert_eq!(parse_tagging_header(&many[..10].join("&")).unwrap().len(), 10);
        let t = |k: &str, v: &str| validate_tags(&[(k.to_string(), v.to_string())]).map_err(|e| e.code);
        assert_eq!(t("", "v"), Err(Code::InvalidTag));
        assert_eq!(t(&"é".repeat(MAX_TAG_KEY), &"é".repeat(MAX_TAG_VALUE)), Ok(()), "limits count characters");
        assert_eq!(t(&"k".repeat(MAX_TAG_KEY + 1), "v"), Err(Code::InvalidTag));
        assert_eq!(t("k", &"v".repeat(MAX_TAG_VALUE + 1)), Err(Code::InvalidTag));
        assert_eq!(parse_tagging_header("").unwrap(), vec![]);
    }

    #[test]
    fn metadata_from_any_names() {
        // POST form field names are arbitrary UTF-8 and reach this unfiltered (never a panic).
        let m = user_metadata([("x-ignore-aé", "1"), ("x-amz-metaé", "1"), ("é", "x"), ("X-AMZ-META-é", "v"), ("x-amz-meta", "no")]).unwrap();
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![("é".to_string(), "v".to_string())]);
        // 2 KB of names plus values, in bytes.
        user_metadata([("x-amz-meta-ab", "v".repeat(2046).as_str())]).unwrap();
        assert_eq!(user_metadata([("x-amz-meta-ab", "é".repeat(1024).as_str())]).unwrap_err().code, Code::MetadataTooLarge);
        assert_eq!(user_metadata([("x-amz-meta-a", "v".repeat(1024).as_str()), ("x-amz-meta-b", "v".repeat(1023).as_str())]).unwrap_err().code, Code::MetadataTooLarge);
    }

    #[test]
    fn parsing_edges() {
        assert_eq!(percent_decode("%"), b"%");
        assert_eq!(percent_decode("%41%4a%4A"), b"AJJ");
        assert_eq!(percent_decode_utf8("%ff").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(percent_decode_utf8("%C3%A9").unwrap(), "é");
        assert_eq!(parse_query("%ff=%fe"), vec![("\u{fffd}".into(), "\u{fffd}".into())]);
        assert_eq!(parse_query("a=b=c"), vec![("a".into(), "b=c".into())]);
        assert_eq!(uri_encode("~-_.!*'()", true), "~-_.%21%2A%27%28%29");
        // Copy sources.
        assert_eq!(parse_copy_source("%ff/k").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(parse_copy_source("//k").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(parse_copy_source(" b/k/ ").unwrap(), ("b".into(), "k/".into()));
        assert_eq!(parse_copy_source("b/a%3Fb?x=1&versionId=null").unwrap(), ("b".into(), "a?b".into()));
        for bad in ["0-9", "bytes=9-0", "bytes=a-1", "bytes=1", "bytes=-5"] {
            assert_eq!(parse_copy_source_range(bad, 100).unwrap_err().code, Code::InvalidArgument, "{bad}");
        }
        assert_eq!(parse_copy_source_range(" bytes=0-0 ", 1).unwrap(), (0, 0));
        assert_eq!(parse_copy_source_range("bytes=0-0", 0).unwrap_err().code, Code::InvalidRange);
        // Ranges.
        let r = |h: &str, size: u64| parse_range(Some(h), size).map_err(|e| e.code);
        assert_eq!(r(" bytes= 0 - 4 ", 10), Ok(Some((0, 4))));
        assert_eq!(r("bytes=0-0", 1), Ok(Some((0, 0))));
        assert_eq!(r("bytes=-1", 1), Ok(Some((0, 0))));
        for ignored in ["bytes=-", "bytes=-a", "bytes=4", "bytes=", "bytes=99999999999999999999999-", "bytes=-99999999999999999999999", "Bytes=0-1"] {
            assert_eq!(r(ignored, 10), Ok(None), "{ignored}");
        }
        assert_eq!(part_range(&[5, 0], 2), None);
        assert_eq!(part_range(&[], 1), None);
        // Bucket names.
        for ok in ["a.b.c", "1.2.3", "1.2.3.4.5", "a-b-c"] {
            assert!(valid_bucket_name(ok), "{ok}");
        }
        for bad in ["sthree-x", "abc--x-s3", "abc.mrap", "amzn-s3-demo-x", "a-.b", "abc--ol-s3", "abc--table-s3", "a b", "abé", "abc."] {
            assert!(!valid_bucket_name(bad), "{bad}");
        }
        validate_key(&"é".repeat(512)).unwrap();
        assert_eq!(validate_key(&"é".repeat(513)).unwrap_err().code, Code::KeyTooLongError, "1026 bytes");
        assert_eq!(unquote_etag("\""), "\"");
        assert_eq!(unquote_etag(" \"a\" "), "a");
    }

    #[test]
    fn overrides_and_condition_edges() {
        let all = [
            ("response-content-type", "t"),
            ("response-content-language", "l"),
            ("response-expires", "e"),
            ("response-cache-control", "c"),
            ("response-content-disposition", "d"),
            ("response-content-encoding", "g"),
            ("Response-Content-Type", "ignored"),
        ];
        let o = Overrides::from_query(all);
        assert_eq!(o, Overrides { content_type: Some("t".into()), content_language: Some("l".into()), expires: Some("e".into()), cache_control: Some("c".into()), content_disposition: Some("d".into()), content_encoding: Some("g".into()) });
        let get = |h: &'static [(&'static str, &'static str)]| move |n: &str| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| *v);
        assert!(Conditions::from_headers(get(&[("if-match", "  "), ("if-none-match", ""), ("if-modified-since", "x")])).is_empty());
        let t = 1_445_412_480;
        let lm = t * 1000;
        let c = |ims: Option<i64>, ius: Option<i64>| Conditions { if_modified_since: ims, if_unmodified_since: ius, ..Default::default() };
        assert_eq!(c(Some(t + 1), None).check_copy_source("e", lm).unwrap_err().code, Code::PreconditionFailed);
        assert_eq!(c(None, Some(t - 1)).check_copy_source("e", lm).unwrap_err().code, Code::PreconditionFailed);
        c(Some(t - 1), Some(t)).check_copy_source("e", lm).unwrap();
        let weak = Conditions { if_match: Some("W/\"e\"".into()), ..Default::default() };
        weak.check_read("e", lm).unwrap();
        weak.check_write(Some("\"e\"")).unwrap();
        let failed = Conditions { if_unmodified_since: Some(t - 1), ..Default::default() }.check_read("e", lm).unwrap_err();
        assert!(failed.extra.contains(&("Condition", "If-Unmodified-Since".into())));
        // If-None-Match: * on a read is "not modified" for any existing object.
        assert_eq!(Conditions { if_none_match: Some("*".into()), ..Default::default() }.check_read("e", lm).unwrap_err().code, Code::NotModified);
        // Both write conditions: the key must not exist and must match, so it always fails.
        let both = Conditions { if_match: Some("e".into()), if_none_match: Some("*".into()), ..Default::default() };
        assert_eq!(both.check_write(None).unwrap_err().code, Code::NoSuchKey);
        assert_eq!(both.check_write(Some("e")).unwrap_err().code, Code::PreconditionFailed);
    }
}
