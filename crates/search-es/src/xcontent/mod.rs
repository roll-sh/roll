//! Content types: media-type parsing and negotiation, and rendering responses as JSON, YAML or
//! CBOR (`format=`, `Accept`), with `filter_path` applied first.
//!
//! Media types follow Elasticsearch's `ParsedMediaType`: `type/subtype` plus `;`-separated
//! parameters, case-insensitive, where the vendor types
//! `application/vnd.elasticsearch+json; compatible-with=N` select the REST compatibility
//! version. Unlike Elasticsearch, a comma-separated `Accept` list is resolved to its first
//! supported entry rather than ignored, and `compatible-with=7` is accepted (served with the
//! version 8 behaviour), so older clients in compatibility mode keep working.

pub mod cbor;
pub mod filter;
pub mod yaml;

use crate::error::EsError;
use crate::json::{self, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XContentType {
    Json,
    Yaml,
    Cbor,
    Smile,
}

impl XContentType {
    /// From a `format` query parameter.
    pub fn from_format(f: &str) -> Option<XContentType> {
        match f.to_ascii_lowercase().as_str() {
            "json" => Some(XContentType::Json),
            "yaml" => Some(XContentType::Yaml),
            "cbor" => Some(XContentType::Cbor),
            "smile" => Some(XContentType::Smile),
            _ => None,
        }
    }

    /// From `type/subtype` (lower-case, without parameters).
    pub fn from_mime(mime: &str) -> Option<XContentType> {
        match mime {
            "application/json" | "application/x-ndjson" | "application/*" => Some(XContentType::Json),
            "application/vnd.elasticsearch+json" | "application/vnd.elasticsearch+x-ndjson" => Some(XContentType::Json),
            "application/yaml" | "application/vnd.elasticsearch+yaml" => Some(XContentType::Yaml),
            "application/cbor" | "application/vnd.elasticsearch+cbor" => Some(XContentType::Cbor),
            "application/smile" | "application/vnd.elasticsearch+smile" => Some(XContentType::Smile),
            _ => None,
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            XContentType::Json => "application/json",
            XContentType::Yaml => "application/yaml",
            XContentType::Cbor => "application/cbor",
            XContentType::Smile => "application/smile",
        }
    }

    fn vendor_mime(self) -> &'static str {
        match self {
            XContentType::Json => "application/vnd.elasticsearch+json",
            XContentType::Yaml => "application/vnd.elasticsearch+yaml",
            XContentType::Cbor => "application/vnd.elasticsearch+cbor",
            XContentType::Smile => "application/vnd.elasticsearch+smile",
        }
    }

    /// The response `Content-Type`: the vendor type with `compatible-with` when the request
    /// asked for a compatibility version.
    pub fn content_type(self, compat: Option<u8>) -> String {
        match compat {
            Some(v) => format!("{};compatible-with={v}", self.vendor_mime()),
            None => self.mime().to_string(),
        }
    }
}

/// A parsed `Content-Type` / `Accept` value.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaType {
    pub original: String,
    /// `type/subtype`, lower-case.
    pub mime: String,
    pub params: Vec<(String, String)>,
}

impl MediaType {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The `compatible-with` version, if any.
    pub fn compatible_with(&self) -> Result<Option<u8>, String> {
        match self.param("compatible-with") {
            None => Ok(None),
            Some(v) => v.parse::<u8>().map(Some).map_err(|_| format!("For input string: \"{v}\"")),
        }
    }

    pub fn xcontent(&self) -> Option<XContentType> {
        XContentType::from_mime(&self.mime)
    }
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// Parses a single media type. `Ok(None)` for `*/*` (treated as absent).
pub fn parse_media_type(header: &str) -> Result<Option<MediaType>, String> {
    if header.trim() == "*/*" {
        return Ok(None);
    }
    let lower = header.to_ascii_lowercase();
    let mut parts = lower.split(';');
    let mime = parts.next().unwrap_or("");
    let ok = match mime.split_once('/') {
        Some((t, s)) => {
            let (t, s) = (t.trim(), s.trim());
            !t.is_empty() && !s.is_empty() && t.chars().all(is_tchar) && s.chars().all(is_tchar)
        }
        None => false,
    };
    if !ok {
        return Err(format!("invalid media-type [{header}]"));
    }
    let (t, s) = mime.split_once('/').unwrap();
    let mut params = Vec::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let kv: Vec<&str> = p.split('=').collect();
        if kv.len() != 2 || kv[0].ends_with(char::is_whitespace) || kv[0].is_empty() || kv[1].starts_with(char::is_whitespace) || kv[1].is_empty() {
            return Err(format!("invalid parameters for header [{header}]"));
        }
        params.push((kv[0].trim().to_string(), kv[1].trim().to_string()));
    }
    Ok(Some(MediaType { original: header.to_string(), mime: format!("{}/{}", t.trim(), s.trim()), params }))
}

/// Parses an `Accept` header, which may be a list: the first entry Elasticsearch can produce
/// (or the first well-formed entry) wins.
pub fn parse_accept(header: &str) -> Result<Option<MediaType>, String> {
    if !header.contains(',') {
        return parse_media_type(header);
    }
    let mut first = None;
    for entry in header.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Ok(Some(mt)) = parse_media_type(entry) {
            if mt.xcontent().is_some() || mt.mime.starts_with("text/") {
                return Ok(Some(mt));
            }
            first.get_or_insert(mt);
        }
    }
    Ok(first)
}

/// Serializes a response body.
pub fn render(v: &Value, ty: XContentType, pretty: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    match ty {
        XContentType::Json | XContentType::Smile => {
            json::write::write_json(&mut out, v, pretty);
            if pretty {
                out.push(b'\n');
            }
        }
        XContentType::Yaml => yaml::write_yaml(&mut out, v),
        XContentType::Cbor => cbor::write_cbor(&mut out, v),
    }
    out
}

/// Parses a request body of the given type. `Ok(None)` for an empty body.
pub fn parse_body(body: &[u8], ty: XContentType) -> Result<Option<Value>, EsError> {
    match ty {
        XContentType::Json => Ok(json::parse(body)?),
        XContentType::Cbor => {
            if body.is_empty() {
                return Ok(None);
            }
            cbor::read_cbor(body).map(Some).map_err(EsError::parse)
        }
        XContentType::Yaml | XContentType::Smile => Err(EsError::illegal_argument(format!(
            "request body of type [{}] is not supported by roll search, send JSON or CBOR",
            ty.mime()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types() {
        let m = parse_media_type("application/vnd.elasticsearch+json; compatible-with=8").unwrap().unwrap();
        assert_eq!(m.mime, "application/vnd.elasticsearch+json");
        assert_eq!(m.compatible_with().unwrap(), Some(8));
        let m = parse_media_type("application/vnd.elasticsearch+json;compatible-with=9").unwrap().unwrap();
        assert_eq!(m.compatible_with().unwrap(), Some(9));
        assert_eq!(parse_media_type("Application/JSON; charset=UTF-8").unwrap().unwrap().mime, "application/json");
        assert!(parse_media_type("json").is_err());
        assert!(parse_media_type("application/json; compatible-with =8").is_err());
        assert_eq!(parse_media_type("*/*").unwrap(), None);
    }

    #[test]
    fn accept_lists() {
        let m = parse_accept("application/vnd.elasticsearch+json; compatible-with=8,text/plain").unwrap().unwrap();
        assert_eq!(m.compatible_with().unwrap(), Some(8));
        let m = parse_accept("text/html, application/json").unwrap().unwrap();
        assert_eq!(m.mime, "text/html");
    }

    #[test]
    fn content_types() {
        assert_eq!(XContentType::Json.content_type(Some(8)), "application/vnd.elasticsearch+json;compatible-with=8");
        assert_eq!(XContentType::Yaml.content_type(None), "application/yaml");
    }
}
