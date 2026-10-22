//! S3 XML: response documents, and bounded parsers for request bodies.

use crate::XMLNS;
use crate::checksum::{Algorithm, ChecksumType};
use crate::error::{Code, S3Error};
use crate::http::url_encode_listing;
use crate::time::iso8601_ms;
use quick_xml::events::Event;

/// Largest XML request body accepted.
pub const MAX_XML_BYTES: usize = 1 << 20;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 100_000;

/// Escape text for an element or attribute. Control characters become character references so
/// they survive parsing.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("&#x{:X};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A tiny XML writer.
pub(crate) struct W(pub(crate) String);

impl W {
    pub(crate) fn root(name: &str) -> Self {
        let mut s = String::with_capacity(1024);
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<");
        s.push_str(name);
        s.push_str(" xmlns=\"");
        s.push_str(XMLNS);
        s.push_str("\">");
        W(s)
    }

    pub(crate) fn open(&mut self, name: &str) {
        self.0.push('<');
        self.0.push_str(name);
        self.0.push('>');
    }

    pub(crate) fn close(&mut self, name: &str) {
        self.0.push_str("</");
        self.0.push_str(name);
        self.0.push('>');
    }

    pub(crate) fn el(&mut self, name: &str, text: &str) {
        self.open(name);
        self.0.push_str(&escape(text));
        self.close(name);
    }

    pub(crate) fn opt(&mut self, name: &str, text: Option<&str>) {
        if let Some(t) = text {
            self.el(name, t);
        }
    }

    pub(crate) fn finish(mut self, root: &str) -> String {
        self.close(root);
        self.0
    }
}

pub(crate) fn enc(s: &str, url: bool) -> String {
    if url { url_encode_listing(s) } else { s.to_string() }
}

pub(crate) fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// An owner (`<Owner><ID/><DisplayName/></Owner>`).
#[derive(Clone, Debug, Default)]
pub struct Owner {
    pub id: String,
    pub display_name: String,
}

pub(crate) fn owner(w: &mut W, tag: &str, o: &Owner) {
    w.open(tag);
    w.el("ID", &o.id);
    w.el("DisplayName", &o.display_name);
    w.close(tag);
}

#[derive(Clone, Debug)]
pub struct BucketEntry {
    pub name: String,
    pub created_ms: i64,
    pub region: Option<String>,
}

/// `ListAllMyBucketsResult`.
pub fn list_buckets(o: &Owner, buckets: &[BucketEntry]) -> String {
    list_buckets_page(o, buckets, None, None)
}

/// `ListAllMyBucketsResult` of a page (`max-buckets`): `next` is the `ContinuationToken` of the
/// next page, `prefix` the request's.
pub fn list_buckets_page(o: &Owner, buckets: &[BucketEntry], next: Option<&str>, prefix: Option<&str>) -> String {
    let mut w = W::root("ListAllMyBucketsResult");
    w.opt("ContinuationToken", next);
    w.opt("Prefix", prefix);
    owner(&mut w, "Owner", o);
    w.open("Buckets");
    for b in buckets {
        w.open("Bucket");
        w.el("Name", &b.name);
        w.el("CreationDate", &iso8601_ms(b.created_ms));
        w.opt("BucketRegion", b.region.as_deref());
        w.close("Bucket");
    }
    w.close("Buckets");
    w.finish("ListAllMyBucketsResult")
}

/// One object in a listing.
#[derive(Clone, Debug)]
pub struct ObjectEntry {
    pub key: String,
    pub last_modified_ms: i64,
    /// Unquoted.
    pub etag: String,
    pub size: u64,
    pub storage_class: String,
    pub checksum: Option<(Algorithm, ChecksumType)>,
    pub owner: Option<Owner>,
}

fn contents(w: &mut W, o: &ObjectEntry, url: bool) {
    w.open("Contents");
    w.el("Key", &enc(&o.key, url));
    w.el("LastModified", &iso8601_ms(o.last_modified_ms));
    w.el("ETag", &crate::http::quote_etag(&o.etag));
    if let Some((a, t)) = o.checksum {
        w.el("ChecksumAlgorithm", a.as_str());
        w.el("ChecksumType", t.as_str());
    }
    w.el("Size", &o.size.to_string());
    if let Some(ow) = &o.owner {
        owner(w, "Owner", ow);
    }
    w.el("StorageClass", &o.storage_class);
    w.close("Contents");
}

fn common_prefixes(w: &mut W, prefixes: &[String], url: bool) {
    for p in prefixes {
        w.open("CommonPrefixes");
        w.el("Prefix", &enc(p, url));
        w.close("CommonPrefixes");
    }
}

/// `ListObjectsV2` result.
#[derive(Clone, Debug, Default)]
pub struct ListV2 {
    pub name: String,
    pub prefix: String,
    pub delimiter: Option<String>,
    pub max_keys: u32,
    pub is_truncated: bool,
    pub continuation_token: Option<String>,
    pub next_continuation_token: Option<String>,
    pub start_after: Option<String>,
    /// `encoding-type=url`.
    pub url_encode: bool,
    pub contents: Vec<ObjectEntry>,
    pub common_prefixes: Vec<String>,
}

pub fn list_objects_v2(l: &ListV2) -> String {
    let u = l.url_encode;
    let mut w = W::root("ListBucketResult");
    w.el("Name", &l.name);
    w.el("Prefix", &enc(&l.prefix, u));
    w.opt("Delimiter", l.delimiter.as_deref().map(|d| enc(d, u)).as_deref());
    w.el("MaxKeys", &l.max_keys.to_string());
    if u {
        w.el("EncodingType", "url");
    }
    w.el("KeyCount", &(l.contents.len() + l.common_prefixes.len()).to_string());
    w.el("IsTruncated", bool_str(l.is_truncated));
    w.opt("ContinuationToken", l.continuation_token.as_deref());
    w.opt("NextContinuationToken", l.next_continuation_token.as_deref());
    w.opt("StartAfter", l.start_after.as_deref().map(|s| enc(s, u)).as_deref());
    for o in &l.contents {
        contents(&mut w, o, u);
    }
    common_prefixes(&mut w, &l.common_prefixes, u);
    w.finish("ListBucketResult")
}

/// `ListObjects` (v1) result.
#[derive(Clone, Debug, Default)]
pub struct ListV1 {
    pub name: String,
    pub prefix: String,
    pub marker: String,
    /// Only sent with a delimiter, as S3 does.
    pub next_marker: Option<String>,
    pub delimiter: Option<String>,
    pub max_keys: u32,
    pub is_truncated: bool,
    pub url_encode: bool,
    pub contents: Vec<ObjectEntry>,
    pub common_prefixes: Vec<String>,
}

pub fn list_objects_v1(l: &ListV1) -> String {
    let u = l.url_encode;
    let mut w = W::root("ListBucketResult");
    w.el("Name", &l.name);
    w.el("Prefix", &enc(&l.prefix, u));
    w.el("Marker", &enc(&l.marker, u));
    w.opt("NextMarker", l.next_marker.as_deref().map(|s| enc(s, u)).as_deref());
    w.el("MaxKeys", &l.max_keys.to_string());
    w.opt("Delimiter", l.delimiter.as_deref().map(|d| enc(d, u)).as_deref());
    if u {
        w.el("EncodingType", "url");
    }
    w.el("IsTruncated", bool_str(l.is_truncated));
    for o in &l.contents {
        contents(&mut w, o, u);
    }
    common_prefixes(&mut w, &l.common_prefixes, u);
    w.finish("ListBucketResult")
}

pub fn initiate_multipart(bucket: &str, key: &str, upload_id: &str) -> String {
    let mut w = W::root("InitiateMultipartUploadResult");
    w.el("Bucket", bucket);
    w.el("Key", key);
    w.el("UploadId", upload_id);
    w.finish("InitiateMultipartUploadResult")
}

/// `checksum`: `(algorithm, base64 value, type)`.
pub fn complete_multipart(location: &str, bucket: &str, key: &str, etag: &str, checksum: Option<(Algorithm, &str, ChecksumType)>) -> String {
    let mut w = W::root("CompleteMultipartUploadResult");
    w.el("Location", location);
    w.el("Bucket", bucket);
    w.el("Key", key);
    w.el("ETag", &crate::http::quote_etag(etag));
    if let Some((a, v, t)) = checksum {
        w.el(a.xml_element(), v);
        w.el("ChecksumType", t.as_str());
    }
    w.finish("CompleteMultipartUploadResult")
}

#[derive(Clone, Debug)]
pub struct PartEntry {
    pub number: u32,
    pub last_modified_ms: i64,
    pub etag: String,
    pub size: u64,
    /// `(algorithm, base64 value)`.
    pub checksum: Option<(Algorithm, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct ListParts {
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub part_number_marker: u32,
    pub next_part_number_marker: Option<u32>,
    pub max_parts: u32,
    pub is_truncated: bool,
    pub parts: Vec<PartEntry>,
    pub storage_class: String,
    pub owner: Owner,
    pub checksum: Option<(Algorithm, ChecksumType)>,
}

pub fn list_parts(l: &ListParts) -> String {
    let mut w = W::root("ListPartsResult");
    w.el("Bucket", &l.bucket);
    w.el("Key", &l.key);
    w.el("UploadId", &l.upload_id);
    owner(&mut w, "Initiator", &l.owner);
    owner(&mut w, "Owner", &l.owner);
    w.el("StorageClass", &l.storage_class);
    w.el("PartNumberMarker", &l.part_number_marker.to_string());
    if let Some(n) = l.next_part_number_marker {
        w.el("NextPartNumberMarker", &n.to_string());
    }
    w.el("MaxParts", &l.max_parts.to_string());
    w.el("IsTruncated", bool_str(l.is_truncated));
    if let Some((a, t)) = l.checksum {
        w.el("ChecksumAlgorithm", a.as_str());
        w.el("ChecksumType", t.as_str());
    }
    for p in &l.parts {
        w.open("Part");
        w.el("PartNumber", &p.number.to_string());
        w.el("LastModified", &iso8601_ms(p.last_modified_ms));
        w.el("ETag", &crate::http::quote_etag(&p.etag));
        w.el("Size", &p.size.to_string());
        if let Some((a, v)) = &p.checksum {
            w.el(a.xml_element(), v);
        }
        w.close("Part");
    }
    w.finish("ListPartsResult")
}

/// One part in GetObjectAttributes' `ObjectParts`.
#[derive(Clone, Debug)]
pub struct AttributesPart {
    pub number: u32,
    pub size: u64,
    /// `(algorithm, base64 value)`, when the part's checksum is known.
    pub checksum: Option<(Algorithm, String)>,
}

/// GetObjectAttributes' `ObjectParts` (multipart objects only).
#[derive(Clone, Debug, Default)]
pub struct AttributesParts {
    pub parts_count: u32,
    pub part_number_marker: u32,
    pub next_part_number_marker: Option<u32>,
    pub max_parts: u32,
    pub is_truncated: bool,
    pub parts: Vec<AttributesPart>,
}

/// A GetObjectAttributes response: each field is `Some` when it was asked for (and applies).
#[derive(Clone, Debug, Default)]
pub struct ObjectAttributes {
    /// Without quotes, as S3 returns it here.
    pub etag: Option<String>,
    /// `(algorithm, base64 value, type)`.
    pub checksum: Option<(Algorithm, String, ChecksumType)>,
    pub parts: Option<AttributesParts>,
    pub storage_class: Option<String>,
    pub size: Option<u64>,
}

pub fn get_object_attributes(a: &ObjectAttributes) -> String {
    let mut w = W::root("GetObjectAttributesResponse");
    w.opt("ETag", a.etag.as_deref());
    if let Some((alg, v, t)) = &a.checksum {
        w.open("Checksum");
        w.el(alg.xml_element(), v);
        w.el("ChecksumType", t.as_str());
        w.close("Checksum");
    }
    if let Some(p) = &a.parts {
        w.open("ObjectParts");
        w.el("PartsCount", &p.parts_count.to_string());
        w.el("PartNumberMarker", &p.part_number_marker.to_string());
        if let Some(n) = p.next_part_number_marker {
            w.el("NextPartNumberMarker", &n.to_string());
        }
        w.el("MaxParts", &p.max_parts.to_string());
        w.el("IsTruncated", bool_str(p.is_truncated));
        for part in &p.parts {
            w.open("Part");
            w.el("PartNumber", &part.number.to_string());
            w.el("Size", &part.size.to_string());
            if let Some((alg, v)) = &part.checksum {
                w.el(alg.xml_element(), v);
            }
            w.close("Part");
        }
        w.close("ObjectParts");
    }
    w.opt("StorageClass", a.storage_class.as_deref());
    if let Some(n) = a.size {
        w.el("ObjectSize", &n.to_string());
    }
    w.finish("GetObjectAttributesResponse")
}

#[derive(Clone, Debug)]
pub struct UploadEntry {
    pub key: String,
    pub upload_id: String,
    pub initiated_ms: i64,
    pub checksum: Option<(Algorithm, ChecksumType)>,
}

#[derive(Clone, Debug, Default)]
pub struct ListUploads {
    pub bucket: String,
    pub key_marker: String,
    pub upload_id_marker: String,
    pub next_key_marker: Option<String>,
    pub next_upload_id_marker: Option<String>,
    pub prefix: String,
    pub delimiter: Option<String>,
    pub max_uploads: u32,
    pub is_truncated: bool,
    pub url_encode: bool,
    pub uploads: Vec<UploadEntry>,
    pub common_prefixes: Vec<String>,
    pub owner: Owner,
    pub storage_class: String,
}

pub fn list_uploads(l: &ListUploads) -> String {
    let u = l.url_encode;
    let mut w = W::root("ListMultipartUploadsResult");
    w.el("Bucket", &l.bucket);
    w.el("KeyMarker", &enc(&l.key_marker, u));
    w.el("UploadIdMarker", &l.upload_id_marker);
    w.opt("NextKeyMarker", l.next_key_marker.as_deref().map(|s| enc(s, u)).as_deref());
    w.opt("NextUploadIdMarker", l.next_upload_id_marker.as_deref());
    w.el("Prefix", &enc(&l.prefix, u));
    w.opt("Delimiter", l.delimiter.as_deref().map(|d| enc(d, u)).as_deref());
    w.el("MaxUploads", &l.max_uploads.to_string());
    if u {
        w.el("EncodingType", "url");
    }
    w.el("IsTruncated", bool_str(l.is_truncated));
    for up in &l.uploads {
        w.open("Upload");
        w.el("Key", &enc(&up.key, u));
        w.el("UploadId", &up.upload_id);
        owner(&mut w, "Initiator", &l.owner);
        owner(&mut w, "Owner", &l.owner);
        w.el("StorageClass", &l.storage_class);
        w.el("Initiated", &iso8601_ms(up.initiated_ms));
        if let Some((a, t)) = up.checksum {
            w.el("ChecksumAlgorithm", a.as_str());
            w.el("ChecksumType", t.as_str());
        }
        w.close("Upload");
    }
    common_prefixes(&mut w, &l.common_prefixes, u);
    w.finish("ListMultipartUploadsResult")
}

/// `CopyObjectResult`; `checksum`: `(algorithm, base64 value)`.
pub fn copy_object_result(etag: &str, last_modified_ms: i64, checksum: Option<(Algorithm, &str)>) -> String {
    let mut w = W::root("CopyObjectResult");
    w.el("ETag", &crate::http::quote_etag(etag));
    w.el("LastModified", &iso8601_ms(last_modified_ms));
    if let Some((a, v)) = checksum {
        w.el(a.xml_element(), v);
    }
    w.finish("CopyObjectResult")
}

pub fn copy_part_result(etag: &str, last_modified_ms: i64, checksum: Option<(Algorithm, &str)>) -> String {
    let mut w = W::root("CopyPartResult");
    w.el("ETag", &crate::http::quote_etag(etag));
    w.el("LastModified", &iso8601_ms(last_modified_ms));
    if let Some((a, v)) = checksum {
        w.el(a.xml_element(), v);
    }
    w.finish("CopyPartResult")
}

/// One failure in a `DeleteResult`.
#[derive(Clone, Debug)]
pub struct DeleteError {
    pub key: String,
    pub code: Code,
    pub message: String,
}

/// `DeleteResult`: in quiet mode only errors are listed.
pub fn delete_result(deleted: &[String], errors: &[DeleteError], quiet: bool) -> String {
    let mut w = W::root("DeleteResult");
    if !quiet {
        for k in deleted {
            w.open("Deleted");
            w.el("Key", k);
            w.close("Deleted");
        }
    }
    for e in errors {
        w.open("Error");
        w.el("Key", &e.key);
        w.el("Code", e.code.as_str());
        w.el("Message", &e.message);
        w.close("Error");
    }
    w.finish("DeleteResult")
}

/// `GetBucketLocation`: us-east-1 is the empty constraint.
pub fn location_constraint(region: &str) -> String {
    if region.is_empty() || region == "us-east-1" {
        return format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LocationConstraint xmlns=\"{XMLNS}\"/>");
    }
    let mut w = W(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LocationConstraint xmlns=\"{XMLNS}\">"));
    w.0.push_str(&escape(region));
    w.finish("LocationConstraint")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorsRule {
    pub id: Option<String>,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_headers: Vec<String>,
    pub expose_headers: Vec<String>,
    pub max_age_s: Option<u32>,
}

pub fn cors_configuration(rules: &[CorsRule]) -> String {
    let mut w = W::root("CORSConfiguration");
    for r in rules {
        w.open("CORSRule");
        w.opt("ID", r.id.as_deref());
        for v in &r.allowed_headers {
            w.el("AllowedHeader", v);
        }
        for v in &r.allowed_methods {
            w.el("AllowedMethod", v);
        }
        for v in &r.allowed_origins {
            w.el("AllowedOrigin", v);
        }
        for v in &r.expose_headers {
            w.el("ExposeHeader", v);
        }
        if let Some(a) = r.max_age_s {
            w.el("MaxAgeSeconds", &a.to_string());
        }
        w.close("CORSRule");
    }
    w.finish("CORSConfiguration")
}

pub fn tagging(tags: &[(String, String)]) -> String {
    let mut w = W::root("Tagging");
    w.open("TagSet");
    for (k, v) in tags {
        w.open("Tag");
        w.el("Key", k);
        w.el("Value", v);
        w.close("Tag");
    }
    w.close("TagSet");
    w.finish("Tagging")
}

/// A parsed element: local name, text, children.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

pub(crate) fn malformed() -> S3Error {
    S3Error::new(Code::MalformedXML)
}

/// Parse a request body into a [`Node`] tree, within [`MAX_XML_BYTES`], a depth and an element
/// count limit. No DTDs; only the predefined entities.
pub fn parse(body: &[u8]) -> Result<Node, S3Error> {
    if body.len() > MAX_XML_BYTES {
        return Err(S3Error::new(Code::MaxMessageLengthExceeded));
    }
    let text = std::str::from_utf8(body).map_err(|_| malformed())?;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut nodes = 0usize;
    let push_text = |stack: &mut Vec<Node>, t: &str| {
        if let Some(n) = stack.last_mut() {
            n.text.push_str(t);
        }
    };
    loop {
        let ev = reader.read_event().map_err(|_| malformed())?;
        match ev {
            Event::Start(_) | Event::Empty(_) if root.is_some() => return Err(malformed()),
            Event::Start(e) => {
                nodes += 1;
                if stack.len() >= MAX_DEPTH || nodes > MAX_NODES {
                    return Err(malformed());
                }
                let name = std::str::from_utf8(e.local_name().as_ref()).map_err(|_| malformed())?.to_string();
                stack.push(Node { name, ..Node::default() });
            }
            Event::Empty(e) => {
                nodes += 1;
                if nodes > MAX_NODES {
                    return Err(malformed());
                }
                let name = std::str::from_utf8(e.local_name().as_ref()).map_err(|_| malformed())?.to_string();
                let n = Node { name, ..Node::default() };
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => root = Some(n),
                }
            }
            Event::End(e) => {
                let n = stack.pop().ok_or_else(malformed)?;
                if e.local_name().as_ref() != n.name.as_bytes() {
                    return Err(malformed());
                }
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => root = Some(n),
                }
            }
            Event::Text(t) => {
                let s = t.decode().map_err(|_| malformed())?;
                if stack.is_empty() && !s.trim().is_empty() {
                    return Err(malformed());
                }
                push_text(&mut stack, &s);
            }
            Event::CData(t) => {
                let s = t.decode().map_err(|_| malformed())?;
                push_text(&mut stack, &s);
            }
            Event::GeneralRef(r) => {
                let s = if r.is_char_ref() {
                    r.resolve_char_ref().map_err(|_| malformed())?.ok_or_else(malformed)?.to_string()
                } else {
                    match r.decode().map_err(|_| malformed())?.as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        _ => return Err(malformed()),
                    }
                };
                push_text(&mut stack, &s);
            }
            Event::DocType(_) => return Err(malformed()),
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(malformed());
    }
    root.ok_or_else(malformed)
}

pub(crate) fn expect_root(body: &[u8], name: &str) -> Result<Node, S3Error> {
    let n = parse(body)?;
    if n.name != name {
        return Err(malformed());
    }
    Ok(n)
}

/// `CreateBucketConfiguration`: the location constraint, if any (an empty body is fine).
pub fn parse_create_bucket(body: &[u8]) -> Result<Option<String>, S3Error> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None);
    }
    let n = expect_root(body, "CreateBucketConfiguration")?;
    Ok(n.text_of("LocationConstraint").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// One part of a `CompleteMultipartUpload` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletePart {
    pub number: u32,
    /// Unquoted.
    pub etag: String,
    /// `(algorithm, base64 value)` the client claims for the part.
    pub checksums: Vec<(Algorithm, String)>,
}

pub fn parse_complete_multipart(body: &[u8]) -> Result<Vec<CompletePart>, S3Error> {
    let n = expect_root(body, "CompleteMultipartUpload")?;
    let mut parts = Vec::new();
    for p in n.all("Part") {
        let number: u32 = p.text_of("PartNumber").and_then(|s| s.trim().parse().ok()).ok_or_else(malformed)?;
        if !(1..=10_000).contains(&number) {
            return Err(S3Error::msg(Code::InvalidArgument, "Part number must be an integer between 1 and 10000, inclusive"));
        }
        let etag = crate::http::unquote_etag(p.text_of("ETag").ok_or_else(malformed)?).to_string();
        let checksums = p.children.iter().filter_map(|c| Algorithm::from_xml_element(&c.name).map(|a| (a, c.text.trim().to_string()))).collect();
        parts.push(CompletePart { number, etag, checksums });
    }
    if parts.is_empty() {
        return Err(malformed());
    }
    Ok(parts)
}

/// Parts must be in strictly ascending order.
pub fn check_part_order(parts: &[CompletePart]) -> Result<(), S3Error> {
    if parts.windows(2).all(|w| w[0].number < w[1].number) { Ok(()) } else { Err(S3Error::new(Code::InvalidPartOrder)) }
}

/// A `Delete` (DeleteObjects) body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRequest {
    pub quiet: bool,
    pub keys: Vec<String>,
    /// Each key's `<VersionId>`, if given (same order as `keys`).
    pub version_ids: Vec<Option<String>>,
}

pub fn parse_delete(body: &[u8]) -> Result<DeleteRequest, S3Error> {
    let n = expect_root(body, "Delete")?;
    let quiet = n.text_of("Quiet").is_some_and(|q| q.trim().eq_ignore_ascii_case("true"));
    let objects: Vec<(String, Option<String>)> =
        n.all("Object").map(|o| Ok((o.text_of("Key").map(str::to_string).ok_or_else(malformed)?, o.text_of("VersionId").map(|v| v.trim().to_string())))).collect::<Result<_, S3Error>>()?;
    if objects.is_empty() || objects.len() > 1000 {
        return Err(malformed());
    }
    let (keys, version_ids) = objects.into_iter().unzip();
    Ok(DeleteRequest { quiet, keys, version_ids })
}

const CORS_METHODS: [&str; 5] = ["GET", "PUT", "POST", "DELETE", "HEAD"];

pub fn parse_cors(body: &[u8]) -> Result<Vec<CorsRule>, S3Error> {
    let n = expect_root(body, "CORSConfiguration")?;
    let texts = |r: &Node, name: &str| r.all(name).map(|c| c.text.trim().to_string()).collect::<Vec<_>>();
    let mut rules = Vec::new();
    for r in n.all("CORSRule") {
        let rule = CorsRule {
            id: r.text_of("ID").map(|s| s.trim().to_string()),
            allowed_origins: texts(r, "AllowedOrigin"),
            allowed_methods: texts(r, "AllowedMethod"),
            allowed_headers: texts(r, "AllowedHeader"),
            expose_headers: texts(r, "ExposeHeader"),
            max_age_s: match r.text_of("MaxAgeSeconds") {
                Some(s) => Some(s.trim().parse().map_err(|_| malformed())?),
                None => None,
            },
        };
        if rule.allowed_origins.is_empty() || rule.allowed_methods.is_empty() {
            return Err(malformed());
        }
        if let Some(m) = rule.allowed_methods.iter().find(|m| !CORS_METHODS.contains(&m.as_str())) {
            return Err(S3Error::msg(Code::InvalidRequest, format!("Found unsupported HTTP method in CORS config. Unsupported method is {m}")));
        }
        if rule.allowed_origins.iter().chain(&rule.allowed_headers).any(|o| o.matches('*').count() > 1) {
            return Err(S3Error::msg(Code::InvalidRequest, "AllowedOrigin and AllowedHeader can have at most one * wildcard"));
        }
        rules.push(rule);
    }
    if rules.is_empty() || rules.len() > 100 {
        return Err(malformed());
    }
    Ok(rules)
}

/// `Tagging` body → tags (validated).
pub fn parse_tagging(body: &[u8]) -> Result<Vec<(String, String)>, S3Error> {
    let n = expect_root(body, "Tagging")?;
    let set = n.child("TagSet").ok_or_else(malformed)?;
    let tags: Vec<(String, String)> = set.all("Tag").map(|t| Ok((t.text_of("Key").ok_or_else(malformed)?.to_string(), t.text_of("Value").unwrap_or_default().to_string()))).collect::<Result<_, S3Error>>()?;
    crate::http::validate_tags(&tags)?;
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str) -> ObjectEntry {
        ObjectEntry { key: key.into(), last_modified_ms: 1_255_369_830_000, etag: "abc".into(), size: 5, storage_class: "STANDARD".into(), checksum: None, owner: None }
    }

    #[test]
    fn listing() {
        let l = ListV2 {
            name: "b".into(),
            prefix: "a b/".into(),
            delimiter: Some("/".into()),
            max_keys: 1000,
            url_encode: true,
            contents: vec![entry("a b/x&y.txt")],
            common_prefixes: vec!["a b/dir/".into()],
            next_continuation_token: Some("tok".into()),
            is_truncated: true,
            ..Default::default()
        };
        let x = list_objects_v2(&l);
        assert!(x.contains("<Name>b</Name><Prefix>a%20b/</Prefix><Delimiter>/</Delimiter><MaxKeys>1000</MaxKeys><EncodingType>url</EncodingType>"), "{x}");
        assert!(x.contains("<Key>a%20b/x%26y.txt</Key>"));
        assert!(x.contains("<ETag>&quot;abc&quot;</ETag>"));
        assert!(x.contains("<KeyCount>2</KeyCount>"));
        assert!(x.contains("<NextContinuationToken>tok</NextContinuationToken>"));
        assert!(x.contains("<CommonPrefixes><Prefix>a%20b/dir/</Prefix></CommonPrefixes>"));
        assert!(x.contains("<LastModified>2009-10-12T17:50:30.000Z</LastModified>"));
        // It parses back.
        let n = parse(x.as_bytes()).unwrap();
        assert_eq!(n.child("Contents").unwrap().text_of("ETag"), Some("\"abc\""));
        let v1 = list_objects_v1(&ListV1 { name: "b".into(), contents: vec![entry("k\u{1}")], max_keys: 2, ..Default::default() });
        assert!(v1.contains("<Key>k&#x1;</Key>"));
        assert_eq!(parse(v1.as_bytes()).unwrap().child("Contents").unwrap().text_of("Key"), Some("k\u{1}"));
        let b = list_buckets(&Owner { id: "o".into(), display_name: "roll".into() }, &[BucketEntry { name: "b".into(), created_ms: 0, region: Some("local".into()) }]);
        assert!(b.contains("<Bucket><Name>b</Name><CreationDate>1970-01-01T00:00:00.000Z</CreationDate><BucketRegion>local</BucketRegion></Bucket>"));
    }

    #[test]
    fn multipart_documents() {
        assert!(initiate_multipart("b", "k<", "u1").contains("<Key>k&lt;</Key><UploadId>u1</UploadId>"));
        let c = complete_multipart("http://h/b/k", "b", "k", "abc-2", Some((Algorithm::Crc32, "AAAAAA==-2", ChecksumType::Composite)));
        assert!(c.contains("<ETag>&quot;abc-2&quot;</ETag><ChecksumCRC32>AAAAAA==-2</ChecksumCRC32><ChecksumType>COMPOSITE</ChecksumType>"));
        let body = br#"<CompleteMultipartUpload xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <Part><PartNumber>1</PartNumber><ETag>"aa"</ETag><ChecksumCRC32>AAAAAA==</ChecksumCRC32></Part>
            <Part><ETag>&quot;bb&quot;</ETag><PartNumber>2</PartNumber></Part>
        </CompleteMultipartUpload>"#;
        let parts = parse_complete_multipart(body).unwrap();
        assert_eq!(parts, vec![
            CompletePart { number: 1, etag: "aa".into(), checksums: vec![(Algorithm::Crc32, "AAAAAA==".into())] },
            CompletePart { number: 2, etag: "bb".into(), checksums: vec![] },
        ]);
        check_part_order(&parts).unwrap();
        let rev: Vec<_> = parts.iter().rev().cloned().collect();
        assert_eq!(check_part_order(&rev).unwrap_err().code, Code::InvalidPartOrder);
        assert_eq!(parse_complete_multipart(b"<CompleteMultipartUpload/>").unwrap_err().code, Code::MalformedXML);
        assert_eq!(parse_complete_multipart(b"<CompleteMultipartUpload><Part><PartNumber>0</PartNumber><ETag>a</ETag></Part></CompleteMultipartUpload>").unwrap_err().code, Code::InvalidArgument);
        let lp = list_parts(&ListParts { bucket: "b".into(), key: "k".into(), upload_id: "u".into(), max_parts: 1000, parts: vec![PartEntry { number: 1, last_modified_ms: 0, etag: "e".into(), size: 5, checksum: None }], storage_class: "STANDARD".into(), ..Default::default() });
        assert!(lp.contains("<Part><PartNumber>1</PartNumber>"));
        let lu = list_uploads(&ListUploads { bucket: "b".into(), uploads: vec![UploadEntry { key: "k".into(), upload_id: "u".into(), initiated_ms: 0, checksum: None }], max_uploads: 1000, ..Default::default() });
        assert!(lu.contains("<Upload><Key>k</Key><UploadId>u</UploadId>"));
        assert!(copy_object_result("e", 0, None).contains("<ETag>&quot;e&quot;</ETag>"));
        assert!(copy_part_result("e", 0, Some((Algorithm::Sha256, "x"))).contains("<ChecksumSHA256>x</ChecksumSHA256>"));
    }

    #[test]
    fn delete_documents() {
        let d = parse_delete(b"<Delete><Quiet>true</Quiet><Object><Key> a </Key></Object><Object><Key>b&amp;c</Key><VersionId>null</VersionId></Object></Delete>").unwrap();
        assert_eq!(d, DeleteRequest { quiet: true, keys: vec![" a ".into(), "b&c".into()], version_ids: vec![None, Some("null".into())] });
        assert_eq!(parse_delete(b"<Delete></Delete>").unwrap_err().code, Code::MalformedXML);
        let many: String = (0..1001).map(|i| format!("<Object><Key>{i}</Key></Object>")).collect();
        assert_eq!(parse_delete(format!("<Delete>{many}</Delete>").as_bytes()).unwrap_err().code, Code::MalformedXML);
        let r = delete_result(&["a".into()], &[DeleteError { key: "b".into(), code: Code::AccessDenied, message: "no".into() }], false);
        assert!(r.contains("<Deleted><Key>a</Key></Deleted><Error><Key>b</Key><Code>AccessDenied</Code><Message>no</Message></Error>"));
        assert!(!delete_result(&["a".into()], &[], true).contains("Deleted"));
    }

    #[test]
    fn bucket_configs_round_trip() {
        let cors = vec![CorsRule { id: Some("r1".into()), allowed_origins: vec!["https://*.example.com".into()], allowed_methods: vec!["GET".into(), "PUT".into()], allowed_headers: vec!["*".into()], expose_headers: vec!["ETag".into()], max_age_s: Some(3000) }];
        assert_eq!(parse_cors(cors_configuration(&cors).as_bytes()).unwrap(), cors);
        assert_eq!(parse_cors(b"<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>PATCH</AllowedMethod></CORSRule></CORSConfiguration>").unwrap_err().code, Code::InvalidRequest);
        assert_eq!(parse_cors(b"<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin></CORSRule></CORSConfiguration>").unwrap_err().code, Code::MalformedXML);
        let tags = vec![("k".to_string(), "v<".to_string()), ("e".to_string(), "".to_string())];
        assert_eq!(parse_tagging(tagging(&tags).as_bytes()).unwrap(), tags);
        assert_eq!(parse_tagging(b"<Tagging><TagSet><Tag><Key>a</Key><Value>1</Value></Tag><Tag><Key>a</Key><Value>2</Value></Tag></TagSet></Tagging>").unwrap_err().code, Code::InvalidTag);
        assert_eq!(parse_create_bucket(b"").unwrap(), None);
        assert_eq!(parse_create_bucket(b"<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>eu-west-1</LocationConstraint></CreateBucketConfiguration>").unwrap(), Some("eu-west-1".into()));
        assert!(location_constraint("us-east-1").ends_with("/>"));
        assert!(location_constraint("eu-west-1").contains(">eu-west-1</LocationConstraint>"));
    }

    #[test]
    fn hostile_input() {
        let err = |b: &[u8]| parse(b).unwrap_err().code;
        assert_eq!(err(b"<a><b></a>"), Code::MalformedXML);
        assert_eq!(err(b"<a>"), Code::MalformedXML);
        assert_eq!(err(b""), Code::MalformedXML);
        assert_eq!(err(b"<a/><b/>"), Code::MalformedXML);
        assert_eq!(err(b"<!DOCTYPE a [<!ENTITY x \"y\">]><a>&x;</a>"), Code::MalformedXML);
        assert_eq!(err(b"<a>&unknown;</a>"), Code::MalformedXML);
        assert_eq!(err(&[0xff, 0xfe]), Code::MalformedXML);
        let deep = "<a>".repeat(100) + &"</a>".repeat(100);
        assert_eq!(err(deep.as_bytes()), Code::MalformedXML);
        assert_eq!(err(&vec![b' '; MAX_XML_BYTES + 1]), Code::MaxMessageLengthExceeded);
        let n = parse(b"<?xml version=\"1.0\"?><!-- c --><x:a xmlns:x=\"u\"><x:b>1&#x41;&lt;<![CDATA[<raw>]]></x:b></x:a>").unwrap();
        assert_eq!(n.name, "a");
        assert_eq!(n.text_of("b"), Some("1A<<raw>"));
    }

    #[test]
    fn parser_limits() {
        let err = |b: &[u8]| parse(b).unwrap_err().code;
        // Element count and depth caps, just inside and just past.
        assert_eq!(parse(format!("<a>{}</a>", "<b/>".repeat(MAX_NODES - 1)).as_bytes()).unwrap().children.len(), MAX_NODES - 1);
        assert_eq!(err(format!("<a>{}</a>", "<b/>".repeat(MAX_NODES)).as_bytes()), Code::MalformedXML);
        let nest = |n: usize| "<a>".repeat(n) + &"</a>".repeat(n);
        parse(nest(MAX_DEPTH).as_bytes()).unwrap();
        assert_eq!(err(nest(MAX_DEPTH + 1).as_bytes()), Code::MalformedXML);
        assert_eq!(parse(&vec![b' '; MAX_XML_BYTES - 4].into_iter().chain(*b"<a/>").collect::<Vec<u8>>()).unwrap().name, "a", "exactly the cap is fine");
        for bad in [&b"x<a/>"[..], b"<a/>x", b"</a>", b"<a>&#0;</a>", b"<a>&#xD800;</a>", b"<a>&#xZZ;</a>", b"<a><![CDATA[x</a>", b"<a></b>", b"<?xml version=\"1.0\"?>"] {
            assert_eq!(err(bad), Code::MalformedXML, "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(parse(b" \n<a/>\n ").unwrap().name, "a");
        assert_eq!(parse(b"<a>x<b/>y<?pi?></a>").unwrap().text, "xy");
    }

    #[test]
    fn request_bodies() {
        let mal = Code::MalformedXML;
        // CreateBucket.
        assert_eq!(parse_create_bucket(b" \r\n\t").unwrap(), None);
        assert_eq!(parse_create_bucket(b"<CreateBucketConfiguration><LocationConstraint> </LocationConstraint></CreateBucketConfiguration>").unwrap(), None);
        assert_eq!(parse_create_bucket(b"<Other/>").map_err(|e| e.code).unwrap_err(), mal);
        // Delete.
        assert_eq!(parse_delete(b"<Delete><Object><VersionId>null</VersionId></Object></Delete>").map_err(|e| e.code).unwrap_err(), mal);
        assert_eq!(parse_delete(b"<Remove><Object><Key>a</Key></Object></Remove>").map_err(|e| e.code).unwrap_err(), mal);
        let thousand: String = (0..1000).map(|i| format!("<Object><Key>{i}</Key></Object>")).collect();
        assert_eq!(
            parse_delete(format!("<Delete><Quiet>TRUE</Quiet>{thousand}</Delete>").as_bytes()).unwrap(),
            DeleteRequest { quiet: true, keys: (0..1000).map(|i| i.to_string()).collect(), version_ids: vec![None; 1000] }
        );
        let versioned = parse_delete(b"<Delete><Object><Key>a</Key><VersionId> v1 </VersionId></Object><Object><Key>b</Key></Object></Delete>").unwrap();
        assert_eq!(versioned.version_ids, vec![Some("v1".to_string()), None]);
        assert!(!parse_delete(b"<Delete><Quiet>1</Quiet><Object><Key>a</Key></Object></Delete>").unwrap().quiet);
        assert_eq!(parse_delete(b"<Delete><Object><Key></Key></Object></Delete>").unwrap().keys, vec![String::new()]);
        // CompleteMultipartUpload.
        let cmu = |parts: &str| parse_complete_multipart(format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>").as_bytes()).map_err(|e| e.code);
        assert_eq!(cmu("<Part><PartNumber>1</PartNumber></Part>").unwrap_err(), mal);
        assert_eq!(cmu("<Part><PartNumber>x</PartNumber><ETag>a</ETag></Part>").unwrap_err(), mal);
        assert_eq!(cmu("<Part><PartNumber>10001</PartNumber><ETag>a</ETag></Part>").unwrap_err(), Code::InvalidArgument);
        assert_eq!(cmu("<Part><PartNumber> 10000 </PartNumber><ETag>W/\"a\"</ETag><ChecksumSHA1> x </ChecksumSHA1><Other>y</Other></Part>").unwrap(), vec![CompletePart { number: 10_000, etag: "a".into(), checksums: vec![(Algorithm::Sha1, "x".into())] }]);
        let dup = cmu("<Part><PartNumber>1</PartNumber><ETag>a</ETag></Part><Part><PartNumber>1</PartNumber><ETag>b</ETag></Part>").unwrap();
        assert_eq!(check_part_order(&dup).unwrap_err().code, Code::InvalidPartOrder, "duplicates are out of order");
        assert_eq!(cmu("").unwrap_err(), Code::MalformedXML);
        // CORS.
        let cors = |rules: &str| parse_cors(format!("<CORSConfiguration>{rules}</CORSConfiguration>").as_bytes()).map_err(|e| e.code);
        let rule = |extra: &str| format!("<CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>GET</AllowedMethod>{extra}</CORSRule>");
        assert_eq!(cors(&rule("<AllowedOrigin>https://*.*.example</AllowedOrigin>")).unwrap_err(), Code::InvalidRequest);
        assert_eq!(cors(&rule("<AllowedHeader>x-*-*</AllowedHeader>")).unwrap_err(), Code::InvalidRequest);
        assert_eq!(cors(&rule("<MaxAgeSeconds>-1</MaxAgeSeconds>")).unwrap_err(), mal);
        assert_eq!(cors("").unwrap_err(), mal);
        assert_eq!(cors(&rule("").repeat(100)).unwrap().len(), 100);
        assert_eq!(cors(&rule("").repeat(101)).unwrap_err(), mal);
        assert_eq!(cors("<CORSRule><AllowedMethod>GET</AllowedMethod></CORSRule>").unwrap_err(), mal);
        assert_eq!(cors("<CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>get</AllowedMethod></CORSRule>").unwrap_err(), Code::InvalidRequest, "methods are case-sensitive, as on S3");
        // Tagging.
        let tg = |b: &str| parse_tagging(b.as_bytes()).map_err(|e| e.code);
        assert_eq!(tg("<Tagging></Tagging>").unwrap_err(), mal);
        assert_eq!(tg("<Tagging><TagSet><Tag><Value>v</Value></Tag></TagSet></Tagging>").unwrap_err(), mal);
        assert_eq!(tg("<Tagging><TagSet><Tag><Key>k</Key></Tag></TagSet></Tagging>"), Ok(vec![("k".into(), "".into())]));
        assert_eq!(tg("<Tagging><TagSet/></Tagging>"), Ok(vec![]));
        let eleven: String = (0..11).map(|i| format!("<Tag><Key>k{i}</Key><Value>v</Value></Tag>")).collect();
        assert_eq!(tg(&format!("<Tagging><TagSet>{eleven}</TagSet></Tagging>")).unwrap_err(), Code::InvalidTag);
        assert_eq!(tg("<Other><TagSet/></Other>").unwrap_err(), mal);
    }

    #[test]
    fn response_documents() {
        assert_eq!(escape("a\"b'c\t\u{1f}<&>é"), "a&quot;b&apos;c&#x9;&#x1F;&lt;&amp;&gt;é");
        let odd = "x\r\n\t\u{1}\"'<&>é";
        assert_eq!(parse(format!("<a>{}</a>", escape(odd)).as_bytes()).unwrap().text, odd, "escaping round-trips");
        let well_formed = |doc: &str, root: &str| {
            let n = parse(doc.as_bytes()).unwrap_or_else(|e| panic!("{doc}: {e}"));
            assert_eq!(n.name, root);
            assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
            n
        };
        let owner = Owner { id: "o".into(), display_name: "d".into() };
        let e = ObjectEntry { checksum: Some((Algorithm::Crc64Nvme, ChecksumType::FullObject)), owner: Some(owner.clone()), ..entry("a b+c") };
        let v1 = list_objects_v1(&ListV1 { name: "b".into(), prefix: "a b".into(), marker: "m n".into(), next_marker: Some("c+d".into()), delimiter: Some(" ".into()), max_keys: 7, is_truncated: true, url_encode: true, contents: vec![e.clone()], common_prefixes: vec!["p q ".into()] });
        assert!(v1.contains("<Prefix>a%20b</Prefix><Marker>m%20n</Marker><NextMarker>c%2Bd</NextMarker><MaxKeys>7</MaxKeys><Delimiter>%20</Delimiter><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>"), "{v1}");
        assert!(v1.contains("<Key>a%20b%2Bc</Key>") && v1.contains("<Prefix>p%20q%20</Prefix>"));
        assert!(v1.contains("<ChecksumAlgorithm>CRC64NVME</ChecksumAlgorithm><ChecksumType>FULL_OBJECT</ChecksumType><Size>5</Size><Owner><ID>o</ID><DisplayName>d</DisplayName></Owner><StorageClass>STANDARD</StorageClass>"), "{v1}");
        well_formed(&v1, "ListBucketResult");
        let v2 = list_objects_v2(&ListV2 { name: "b".into(), continuation_token: Some("t<".into()), start_after: Some("s t".into()), contents: vec![entry("a b")], ..Default::default() });
        assert!(v2.contains("<KeyCount>1</KeyCount><IsTruncated>false</IsTruncated><ContinuationToken>t&lt;</ContinuationToken><StartAfter>s t</StartAfter>"), "{v2}");
        assert!(!v2.contains("EncodingType") && !v2.contains("Delimiter") && !v2.contains("NextContinuationToken") && !v2.contains("<Owner>"));
        well_formed(&v2, "ListBucketResult");
        let lu = list_uploads(&ListUploads {
            bucket: "b".into(),
            key_marker: "k m".into(),
            next_key_marker: Some("n k".into()),
            next_upload_id_marker: Some("u2".into()),
            url_encode: true,
            uploads: vec![UploadEntry { key: "a b".into(), upload_id: "u".into(), initiated_ms: 0, checksum: Some((Algorithm::Crc32, ChecksumType::Composite)) }],
            common_prefixes: vec!["d ".into()],
            is_truncated: true,
            owner: owner.clone(),
            ..Default::default()
        });
        assert!(lu.contains("<KeyMarker>k%20m</KeyMarker><UploadIdMarker></UploadIdMarker><NextKeyMarker>n%20k</NextKeyMarker><NextUploadIdMarker>u2</NextUploadIdMarker>"), "{lu}");
        assert!(lu.contains("<Key>a%20b</Key>") && lu.contains("<ChecksumAlgorithm>CRC32</ChecksumAlgorithm><ChecksumType>COMPOSITE</ChecksumType></Upload><CommonPrefixes><Prefix>d%20</Prefix></CommonPrefixes>"), "{lu}");
        well_formed(&lu, "ListMultipartUploadsResult");
        let lp = list_parts(&ListParts {
            next_part_number_marker: Some(3),
            part_number_marker: 2,
            is_truncated: true,
            checksum: Some((Algorithm::Sha256, ChecksumType::Composite)),
            parts: vec![PartEntry { number: 3, last_modified_ms: 0, etag: "e".into(), size: 1, checksum: Some((Algorithm::Sha256, "c".into())) }],
            ..Default::default()
        });
        assert!(lp.contains("<PartNumberMarker>2</PartNumberMarker><NextPartNumberMarker>3</NextPartNumberMarker><MaxParts>0</MaxParts><IsTruncated>true</IsTruncated><ChecksumAlgorithm>SHA256</ChecksumAlgorithm><ChecksumType>COMPOSITE</ChecksumType>"), "{lp}");
        assert!(lp.contains("<Size>1</Size><ChecksumSHA256>c</ChecksumSHA256></Part>"));
        well_formed(&lp, "ListPartsResult");
        let c = complete_multipart("http://h/b/a%20b", "b", "a b", "e-2", Some((Algorithm::Crc64Nvme, "AAAAAAAAAAA=", ChecksumType::FullObject)));
        assert!(c.contains("<ChecksumCRC64NVME>AAAAAAAAAAA=</ChecksumCRC64NVME><ChecksumType>FULL_OBJECT</ChecksumType>"));
        assert_eq!(well_formed(&c, "CompleteMultipartUploadResult").text_of("ETag"), Some("\"e-2\""));
        assert!(!complete_multipart("l", "b", "k", "e", None).contains("Checksum"));
        well_formed(&initiate_multipart("b", "k", "u"), "InitiateMultipartUploadResult");
        well_formed(&copy_object_result("e", 0, Some((Algorithm::Crc32c, "x"))), "CopyObjectResult");
        well_formed(&copy_part_result("e", 0, None), "CopyPartResult");
        let quiet = delete_result(&["a".into()], &[DeleteError { key: "b<".into(), code: Code::InternalError, message: "m".into() }], true);
        assert!(!quiet.contains("Deleted") && quiet.contains("<Key>b&lt;</Key><Code>InternalError</Code>"));
        well_formed(&quiet, "DeleteResult");
        assert_eq!(well_formed(&location_constraint(""), "LocationConstraint").text, "");
        assert_eq!(well_formed(&location_constraint("a<b"), "LocationConstraint").text, "a<b");
        well_formed(&tagging(&[]), "Tagging");
        well_formed(&cors_configuration(&[CorsRule::default()]), "CORSConfiguration");
        assert!(list_buckets(&owner, &[BucketEntry { name: "b".into(), created_ms: 0, region: None }]).contains("</CreationDate></Bucket>"));
    }
}
