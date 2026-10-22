//! Versioning and Object Lock: version ids, `VersioningConfiguration`, `ListVersionsResult`,
//! `ObjectLockConfiguration`, `Retention`, `LegalHold` and the versioned `DeleteResult`.

use crate::checksum::{Algorithm, ChecksumType};
use crate::error::{Code, S3Error};
use crate::http::quote_etag;
use crate::time::{iso8601_ms, parse_iso8601};
use crate::xml::{Owner, W, bool_str, enc, expect_root, malformed, owner};

/// Whether `v` can be a version id here: `null`, or 32 lowercase letters and digits (what the
/// store hands out). Anything else is S3's `InvalidArgument: Invalid version id specified`.
pub fn valid_version_id(v: &str) -> bool {
    v == "null" || (v.len() == 32 && v.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// The error S3 gives for a malformed version id.
pub fn invalid_version_id(v: &str) -> S3Error {
    S3Error::msg(Code::InvalidArgument, "Invalid version id specified").with("ArgumentName", "versionId").with("ArgumentValue", v.to_string())
}

/// `GetBucketVersioning`: no `Status` for a bucket that was never versioned.
pub fn versioning_configuration(status: Option<&str>) -> String {
    let mut w = W::root("VersioningConfiguration");
    w.opt("Status", status);
    w.finish("VersioningConfiguration")
}

/// A `PutBucketVersioning` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersioningRequest {
    /// `Enabled` or `Suspended`; `None` leaves it.
    pub status: Option<String>,
    /// `Enabled` or `Disabled`.
    pub mfa_delete: Option<String>,
}

pub fn parse_versioning(body: &[u8]) -> Result<VersioningRequest, S3Error> {
    let n = expect_root(body, "VersioningConfiguration")?;
    let status = match n.text_of("Status").map(str::trim) {
        None => None,
        Some(s @ ("Enabled" | "Suspended")) => Some(s.to_string()),
        Some(_) => return Err(malformed()),
    };
    let mfa_delete = match n.text_of("MfaDelete").map(str::trim) {
        None => None,
        Some(s @ ("Enabled" | "Disabled")) => Some(s.to_string()),
        Some(_) => return Err(malformed()),
    };
    Ok(VersioningRequest { status, mfa_delete })
}

/// One `<Version>` or `<DeleteMarker>` of a `ListVersionsResult`.
#[derive(Clone, Debug, Default)]
pub struct VersionEntry {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    pub delete_marker: bool,
    pub last_modified_ms: i64,
    /// Unquoted.
    pub etag: String,
    pub size: u64,
    pub storage_class: String,
    pub checksum: Option<(Algorithm, ChecksumType)>,
}

#[derive(Clone, Debug, Default)]
pub struct ListVersions {
    pub name: String,
    pub prefix: String,
    pub key_marker: String,
    pub version_id_marker: String,
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<String>,
    pub delimiter: Option<String>,
    pub max_keys: u32,
    pub is_truncated: bool,
    pub url_encode: bool,
    pub entries: Vec<VersionEntry>,
    pub common_prefixes: Vec<String>,
    pub owner: Owner,
}

/// `ListVersionsResult` (ListObjectVersions).
pub fn list_versions(l: &ListVersions) -> String {
    let url = l.url_encode;
    let mut w = W::root("ListVersionsResult");
    w.el("Name", &l.name);
    w.el("Prefix", &enc(&l.prefix, url));
    w.el("KeyMarker", &enc(&l.key_marker, url));
    w.el("VersionIdMarker", &l.version_id_marker);
    if let Some(k) = &l.next_key_marker {
        w.el("NextKeyMarker", &enc(k, url));
    }
    w.opt("NextVersionIdMarker", l.next_version_id_marker.as_deref());
    w.el("MaxKeys", &l.max_keys.to_string());
    if let Some(d) = &l.delimiter {
        w.el("Delimiter", &enc(d, url));
    }
    if url {
        w.el("EncodingType", "url");
    }
    w.el("IsTruncated", bool_str(l.is_truncated));
    for e in &l.entries {
        let tag = if e.delete_marker { "DeleteMarker" } else { "Version" };
        w.open(tag);
        w.el("Key", &enc(&e.key, url));
        w.el("VersionId", &e.version_id);
        w.el("IsLatest", bool_str(e.is_latest));
        w.el("LastModified", &iso8601_ms(e.last_modified_ms));
        if !e.delete_marker {
            w.el("ETag", &quote_etag(&e.etag));
            w.el("Size", &e.size.to_string());
        }
        owner(&mut w, "Owner", &l.owner);
        if !e.delete_marker {
            w.el("StorageClass", &e.storage_class);
            if let Some((a, t)) = e.checksum {
                w.el("ChecksumAlgorithm", a.as_str());
                w.el("ChecksumType", t.as_str());
            }
        }
        w.close(tag);
    }
    for p in &l.common_prefixes {
        w.open("CommonPrefixes");
        w.el("Prefix", &enc(p, url));
        w.close("CommonPrefixes");
    }
    w.finish("ListVersionsResult")
}

/// `GOVERNANCE` or `COMPLIANCE` (S3 takes them exactly so).
pub fn lock_mode(s: &str) -> Option<&'static str> {
    match s.trim() {
        "GOVERNANCE" => Some("GOVERNANCE"),
        "COMPLIANCE" => Some("COMPLIANCE"),
        _ => None,
    }
}

/// A bucket's Object Lock configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectLockConfig {
    /// The default retention for new versions: mode and a period in days or years.
    pub mode: Option<String>,
    pub days: Option<u32>,
    pub years: Option<u32>,
}

pub fn object_lock_configuration(c: &ObjectLockConfig) -> String {
    let mut w = W::root("ObjectLockConfiguration");
    w.el("ObjectLockEnabled", "Enabled");
    if let Some(m) = &c.mode {
        w.open("Rule");
        w.open("DefaultRetention");
        w.el("Mode", m);
        if let Some(d) = c.days {
            w.el("Days", &d.to_string());
        }
        if let Some(y) = c.years {
            w.el("Years", &y.to_string());
        }
        w.close("DefaultRetention");
        w.close("Rule");
    }
    w.finish("ObjectLockConfiguration")
}

/// A `PutObjectLockConfiguration` body. `ObjectLockEnabled` must be `Enabled` (it can't be
/// turned off); a default retention needs a mode and exactly one of `Days` and `Years`.
pub fn parse_object_lock_configuration(body: &[u8]) -> Result<ObjectLockConfig, S3Error> {
    let n = expect_root(body, "ObjectLockConfiguration")?;
    if n.text_of("ObjectLockEnabled").map(str::trim) != Some("Enabled") {
        return Err(malformed());
    }
    let Some(rule) = n.child("Rule") else { return Ok(ObjectLockConfig::default()) };
    let r = rule.child("DefaultRetention").ok_or_else(malformed)?;
    let mode = r.text_of("Mode").and_then(lock_mode).ok_or_else(malformed)?;
    let period = |name: &str, max: u32| -> Result<Option<u32>, S3Error> {
        let Some(t) = r.text_of(name) else { return Ok(None) };
        let v: i64 = t.trim().parse().map_err(|_| malformed())?;
        if v <= 0 {
            return Err(S3Error::msg(Code::InvalidArgument, "Default retention period must be a positive integer value."));
        }
        if v > max as i64 {
            return Err(S3Error::msg(Code::InvalidArgument, "Default retention period is too large."));
        }
        Ok(Some(v as u32))
    };
    let days = period("Days", 36_500)?;
    let years = period("Years", 100)?;
    if days.is_some() == years.is_some() {
        return Err(malformed());
    }
    Ok(ObjectLockConfig { mode: Some(mode.to_string()), days, years })
}

pub fn retention(mode: &str, retain_until_ms: i64) -> String {
    let mut w = W::root("Retention");
    w.el("Mode", mode);
    w.el("RetainUntilDate", &iso8601_ms(retain_until_ms));
    w.finish("Retention")
}

/// A `PutObjectRetention` body: `Some((mode, retain until ms))`, or `None` for an empty
/// `<Retention/>` (remove the retention).
pub fn parse_retention(body: &[u8]) -> Result<Option<(String, i64)>, S3Error> {
    let n = expect_root(body, "Retention")?;
    match (n.text_of("Mode"), n.text_of("RetainUntilDate")) {
        (None, None) => Ok(None),
        (Some(m), Some(d)) => {
            let mode = lock_mode(m).ok_or_else(malformed)?;
            let until = parse_iso8601(d.trim()).ok_or_else(malformed)?;
            Ok(Some((mode.to_string(), until)))
        }
        _ => Err(malformed()),
    }
}

pub fn legal_hold(on: bool) -> String {
    let mut w = W::root("LegalHold");
    w.el("Status", if on { "ON" } else { "OFF" });
    w.finish("LegalHold")
}

pub fn parse_legal_hold(body: &[u8]) -> Result<bool, S3Error> {
    let n = expect_root(body, "LegalHold")?;
    match n.text_of("Status").map(str::trim) {
        Some("ON") => Ok(true),
        Some("OFF") => Ok(false),
        _ => Err(malformed()),
    }
}

/// One `<Deleted>` of a versioned `DeleteResult`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub key: String,
    /// The version deleted, if the request named one.
    pub version_id: Option<String>,
    pub delete_marker: bool,
    /// The marker created, or the marker version deleted.
    pub delete_marker_version_id: Option<String>,
}

/// One `<Error>` of a versioned `DeleteResult`.
#[derive(Clone, Debug)]
pub struct DeleteFailure {
    pub key: String,
    pub version_id: Option<String>,
    pub code: Code,
    pub message: String,
}

/// `DeleteResult` with version ids and delete markers; in quiet mode only errors are listed.
pub fn delete_result(deleted: &[Deleted], errors: &[DeleteFailure], quiet: bool) -> String {
    let mut w = W::root("DeleteResult");
    if !quiet {
        for d in deleted {
            w.open("Deleted");
            w.el("Key", &d.key);
            w.opt("VersionId", d.version_id.as_deref());
            if d.delete_marker {
                w.el("DeleteMarker", "true");
            }
            w.opt("DeleteMarkerVersionId", d.delete_marker_version_id.as_deref());
            w.close("Deleted");
        }
    }
    for e in errors {
        w.open("Error");
        w.el("Key", &e.key);
        w.opt("VersionId", e.version_id.as_deref());
        w.el("Code", e.code.as_str());
        w.el("Message", &e.message);
        w.close("Error");
    }
    w.finish("DeleteResult")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::parse;

    #[test]
    fn version_ids() {
        assert!(valid_version_id("null"));
        assert!(valid_version_id("0000000000000000abcdefghijklmnop"));
        for bad in ["", "NULL", "3HL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY", "abc", &"a".repeat(33), "0000000000000000ABCDEFGHIJKLMNOP"] {
            assert!(!valid_version_id(bad), "{bad}");
        }
        assert_eq!(invalid_version_id("x").code, Code::InvalidArgument);
    }

    #[test]
    fn versioning_documents() {
        assert!(!versioning_configuration(None).contains("Status"));
        let doc = versioning_configuration(Some("Enabled"));
        assert_eq!(parse(doc.as_bytes()).unwrap().text_of("Status"), Some("Enabled"));
        assert_eq!(parse_versioning(doc.as_bytes()).unwrap(), VersioningRequest { status: Some("Enabled".into()), mfa_delete: None });
        let both = b"<VersioningConfiguration><Status>Suspended</Status><MfaDelete>Enabled</MfaDelete></VersioningConfiguration>";
        assert_eq!(parse_versioning(both).unwrap(), VersioningRequest { status: Some("Suspended".into()), mfa_delete: Some("Enabled".into()) });
        for bad in [&b"<VersioningConfiguration><Status>On</Status></VersioningConfiguration>"[..], b"<VersioningConfiguration><MfaDelete>x</MfaDelete></VersioningConfiguration>", b"<Other/>"] {
            assert_eq!(parse_versioning(bad).unwrap_err().code, Code::MalformedXML);
        }
    }

    #[test]
    fn listing() {
        let l = ListVersions {
            name: "b".into(),
            prefix: "a b".into(),
            key_marker: "k".into(),
            next_key_marker: Some("n k".into()),
            next_version_id_marker: Some("v2".into()),
            max_keys: 2,
            is_truncated: true,
            url_encode: true,
            delimiter: Some("/".into()),
            entries: vec![
                VersionEntry { key: "a b".into(), version_id: "v1".into(), is_latest: true, delete_marker: true, last_modified_ms: 0, ..Default::default() },
                VersionEntry { key: "a b".into(), version_id: "v2".into(), etag: "e".into(), size: 3, storage_class: "STANDARD".into(), checksum: Some((Algorithm::Crc32, ChecksumType::FullObject)), ..Default::default() },
            ],
            common_prefixes: vec!["d/".into()],
            ..Default::default()
        };
        let doc = list_versions(&l);
        let n = parse(doc.as_bytes()).unwrap();
        assert_eq!(n.name, "ListVersionsResult");
        assert_eq!((n.text_of("NextKeyMarker"), n.text_of("NextVersionIdMarker"), n.text_of("EncodingType")), (Some("n+k"), Some("v2"), Some("url")));
        let dm = n.child("DeleteMarker").unwrap();
        assert_eq!((dm.text_of("VersionId"), dm.text_of("IsLatest"), dm.text_of("ETag")), (Some("v1"), Some("true"), None));
        let v = n.child("Version").unwrap();
        assert_eq!((v.text_of("Key"), v.text_of("ETag"), v.text_of("Size"), v.text_of("IsLatest"), v.text_of("ChecksumAlgorithm")), (Some("a+b"), Some("\"e\""), Some("3"), Some("false"), Some("CRC32")));
        assert!(doc.find("<DeleteMarker>").unwrap() < doc.find("<Version>").unwrap(), "entries keep their order");
        assert_eq!(n.child("CommonPrefixes").unwrap().text_of("Prefix"), Some("d/"));
        let empty = list_versions(&ListVersions::default());
        assert!(!empty.contains("NextKeyMarker") && !empty.contains("Delimiter") && !empty.contains("EncodingType"));
    }

    #[test]
    fn object_lock_documents() {
        let c = ObjectLockConfig { mode: Some("GOVERNANCE".into()), days: Some(3), years: None };
        assert_eq!(parse_object_lock_configuration(object_lock_configuration(&c).as_bytes()).unwrap(), c);
        let bare = ObjectLockConfig::default();
        assert_eq!(parse_object_lock_configuration(object_lock_configuration(&bare).as_bytes()).unwrap(), bare);
        let y = ObjectLockConfig { mode: Some("COMPLIANCE".into()), days: None, years: Some(1) };
        assert_eq!(parse_object_lock_configuration(object_lock_configuration(&y).as_bytes()).unwrap(), y);
        let err = |rule: &str| parse_object_lock_configuration(format!("<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled>{rule}</ObjectLockConfiguration>").as_bytes()).unwrap_err().code;
        let dr = |inner: &str| format!("<Rule><DefaultRetention>{inner}</DefaultRetention></Rule>");
        assert_eq!(err(&dr("<Mode>GOVERNANCE</Mode><Days>1</Days><Years>1</Years>")), Code::MalformedXML);
        assert_eq!(err(&dr("<Mode>GOVERNANCE</Mode>")), Code::MalformedXML);
        assert_eq!(err(&dr("<Mode>governance</Mode><Days>1</Days>")), Code::MalformedXML);
        assert_eq!(err(&dr("<Mode>GOVERNANCE</Mode><Days>0</Days>")), Code::InvalidArgument);
        assert_eq!(err(&dr("<Mode>GOVERNANCE</Mode><Years>101</Years>")), Code::InvalidArgument);
        assert_eq!(err("<Rule/>"), Code::MalformedXML);
        assert_eq!(parse_object_lock_configuration(b"<ObjectLockConfiguration><ObjectLockEnabled>Disabled</ObjectLockEnabled></ObjectLockConfiguration>").unwrap_err().code, Code::MalformedXML);
    }

    #[test]
    fn retention_and_legal_hold() {
        let doc = retention("COMPLIANCE", 1_800_000_000_000);
        assert_eq!(parse_retention(doc.as_bytes()).unwrap(), Some(("COMPLIANCE".into(), 1_800_000_000_000)));
        assert_eq!(parse_retention(b"<Retention/>").unwrap(), None);
        assert_eq!(parse_retention(b"<Retention><Mode>GOVERNANCE</Mode></Retention>").unwrap_err().code, Code::MalformedXML);
        assert_eq!(parse_retention(b"<Retention><Mode>X</Mode><RetainUntilDate>2030-01-01T00:00:00Z</RetainUntilDate></Retention>").unwrap_err().code, Code::MalformedXML);
        assert_eq!(parse_retention(b"<Retention><Mode>GOVERNANCE</Mode><RetainUntilDate>soon</RetainUntilDate></Retention>").unwrap_err().code, Code::MalformedXML);
        assert!(parse_legal_hold(legal_hold(true).as_bytes()).unwrap());
        assert!(!parse_legal_hold(legal_hold(false).as_bytes()).unwrap());
        assert_eq!(parse_legal_hold(b"<LegalHold><Status>on</Status></LegalHold>").unwrap_err().code, Code::MalformedXML);
        assert_eq!(lock_mode(" GOVERNANCE "), Some("GOVERNANCE"));
        assert_eq!(lock_mode("governance"), None);
    }

    #[test]
    fn delete_results() {
        let d = vec![
            Deleted { key: "a".into(), version_id: None, delete_marker: true, delete_marker_version_id: Some("m1".into()) },
            Deleted { key: "b".into(), version_id: Some("v1".into()), ..Default::default() },
        ];
        let e = vec![DeleteFailure { key: "c".into(), version_id: Some("v2".into()), code: Code::AccessDenied, message: "locked".into() }];
        let doc = delete_result(&d, &e, false);
        assert!(doc.contains("<Deleted><Key>a</Key><DeleteMarker>true</DeleteMarker><DeleteMarkerVersionId>m1</DeleteMarkerVersionId></Deleted>"), "{doc}");
        assert!(doc.contains("<Deleted><Key>b</Key><VersionId>v1</VersionId></Deleted>"), "{doc}");
        assert!(doc.contains("<Error><Key>c</Key><VersionId>v2</VersionId><Code>AccessDenied</Code><Message>locked</Message></Error>"), "{doc}");
        let quiet = delete_result(&d, &e, true);
        assert!(!quiet.contains("Deleted") && quiet.contains("<Error>"));
    }
}
