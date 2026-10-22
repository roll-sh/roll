//! S3's naming rules and limits.

use crate::error::{Result, StoreError};
use crate::model::{CorsRule, LifecycleRule, ObjectHeaders};
use std::collections::BTreeMap;

pub const MAX_KEY_BYTES: usize = 1024;
pub const MAX_METADATA_BYTES: usize = 2048;
pub const MAX_TAGS: usize = 10;
pub const MAX_PART_NUMBER: u32 = 10_000;
pub const MAX_RECOVER_DAYS: u32 = 30;

/// S3 general purpose bucket naming rules.
pub fn bucket_name(name: &str) -> Result<()> {
    let bad = |why: &str| Err(StoreError::InvalidBucketName(format!("{name:?}: {why}")));
    if !(3..=63).contains(&name.len()) {
        return bad("must be 3-63 characters");
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-') {
        return bad("only lowercase letters, digits, '.' and '-'");
    }
    let first = name.as_bytes()[0];
    let last = name.as_bytes()[name.len() - 1];
    if !(first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric()) {
        return bad("must begin and end with a letter or digit");
    }
    if name.contains("..") {
        return bad("no two adjacent periods");
    }
    if name.parse::<std::net::Ipv4Addr>().is_ok() {
        return bad("can't be formatted as an IP address");
    }
    for p in ["xn--", "sthree-", "amzn-s3-demo-"] {
        if name.starts_with(p) {
            return bad(&format!("can't start with {p}"));
        }
    }
    for s in ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"] {
        if name.ends_with(s) {
            return bad(&format!("can't end with {s}"));
        }
    }
    Ok(())
}

pub fn key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(StoreError::InvalidArgument("empty key".into()));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(StoreError::KeyTooLong);
    }
    Ok(())
}

pub fn tags(tags: &BTreeMap<String, String>) -> Result<()> {
    if tags.len() > MAX_TAGS {
        return Err(StoreError::InvalidArgument(format!("at most {MAX_TAGS} tags")));
    }
    for (k, v) in tags {
        if k.is_empty() || k.chars().count() > 128 || v.chars().count() > 256 {
            return Err(StoreError::InvalidArgument("tag keys are 1-128 characters, values up to 256".into()));
        }
    }
    Ok(())
}

pub fn headers(h: &ObjectHeaders) -> Result<()> {
    let size: usize = h.metadata.iter().map(|(k, v)| k.len() + v.len()).sum();
    if size > MAX_METADATA_BYTES {
        return Err(StoreError::MetadataTooLarge);
    }
    tags(&h.tags)
}

pub fn part_number(n: u32) -> Result<()> {
    if !(1..=MAX_PART_NUMBER).contains(&n) {
        return Err(StoreError::InvalidArgument(format!("part numbers are 1-{MAX_PART_NUMBER}")));
    }
    Ok(())
}

pub fn recover_days(d: u32) -> Result<()> {
    if d > MAX_RECOVER_DAYS {
        return Err(StoreError::InvalidArgument(format!("recover_days is 0-{MAX_RECOVER_DAYS}")));
    }
    Ok(())
}

pub fn cors(rules: &[CorsRule]) -> Result<()> {
    if rules.len() > 100 {
        return Err(StoreError::InvalidArgument("at most 100 CORS rules".into()));
    }
    for r in rules {
        if r.allowed_origins.is_empty() || r.allowed_methods.is_empty() {
            return Err(StoreError::InvalidArgument("a CORS rule needs allowed origins and methods".into()));
        }
        for m in &r.allowed_methods {
            if !["GET", "PUT", "POST", "DELETE", "HEAD"].contains(&m.as_str()) {
                return Err(StoreError::InvalidArgument(format!("unsupported CORS method {m}")));
            }
        }
        for o in &r.allowed_origins {
            if o.matches('*').count() > 1 {
                return Err(StoreError::InvalidArgument(format!("origin {o} has more than one wildcard")));
            }
        }
    }
    Ok(())
}

pub fn lifecycle(rules: &mut [LifecycleRule]) -> Result<()> {
    if rules.len() > 1000 {
        return Err(StoreError::InvalidArgument("at most 1000 lifecycle rules".into()));
    }
    for (i, r) in rules.iter_mut().enumerate() {
        if r.id.is_empty() {
            r.id = format!("rule-{}", i + 1);
        }
        if r.expire_days == Some(0) || r.abort_multipart_days == Some(0) {
            return Err(StoreError::InvalidArgument("lifecycle days must be positive".into()));
        }
        if r.expire_days.is_none() && r.abort_multipart_days.is_none() {
            return Err(StoreError::InvalidArgument(format!("lifecycle rule {} has no action", r.id)));
        }
    }
    let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(StoreError::InvalidArgument("lifecycle rule ids must be unique".into()));
    }
    Ok(())
}

/// An ETag from a header, without quotes or a weak marker.
pub fn bare_etag(s: &str) -> &str {
    s.trim().trim_start_matches("W/").trim_matches('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_names() {
        for ok in ["abc", "my-bucket", "a.b.c", "123", "x".repeat(63).as_str()] {
            assert!(bucket_name(ok).is_ok(), "{ok}");
        }
        for bad in ["ab", "ABC", "-abc", "abc-", "a..b", "192.168.1.1", "xn--abc", "a_b", "abc--x-s3", &"x".repeat(64)] {
            assert!(bucket_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn limits() {
        assert!(key(&"k".repeat(1024)).is_ok());
        assert!(matches!(key(&"k".repeat(1025)), Err(StoreError::KeyTooLong)));
        let mut h = ObjectHeaders::default();
        h.metadata.insert("a".into(), "b".repeat(2047));
        assert!(headers(&h).is_ok());
        h.metadata.insert("c".into(), "d".into());
        assert!(matches!(headers(&h), Err(StoreError::MetadataTooLarge)));
        assert_eq!(bare_etag("W/\"abc\""), "abc");
    }
}
