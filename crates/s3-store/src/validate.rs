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
        lifecycle_rule(r)?;
    }
    let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(StoreError::InvalidArgument("lifecycle rule ids must be unique".into()));
    }
    Ok(())
}

/// Storage classes a lifecycle transition may name (S3's).
pub const TRANSITION_CLASSES: [&str; 6] = ["GLACIER", "STANDARD_IA", "ONEZONE_IA", "INTELLIGENT_TIERING", "DEEP_ARCHIVE", "GLACIER_IR"];

/// One lifecycle rule, by S3's rules (the S3 API checks the XML the same way first).
fn lifecycle_rule(r: &LifecycleRule) -> Result<()> {
    let bad = |why: &str| Err(StoreError::InvalidArgument(format!("lifecycle rule {}: {why}", r.id)));
    if r.id.len() > 255 {
        return bad("IDs are at most 255 characters");
    }
    if [r.expire_days, r.abort_multipart_days, r.noncurrent_days].contains(&Some(0)) {
        return Err(StoreError::InvalidArgument("lifecycle days must be positive".into()));
    }
    let actions = r.expire_days.is_some()
        || r.expire_at.is_some()
        || r.expire_delete_markers
        || r.noncurrent_days.is_some()
        || r.abort_multipart_days.is_some()
        || !r.transitions.is_empty()
        || !r.noncurrent_transitions.is_empty();
    if !actions {
        return bad("no action");
    }
    if r.expire_days.is_some() && r.expire_at.is_some() {
        return bad("expire by days or by date, not both");
    }
    if r.expire_delete_markers && (r.expire_days.is_some() || r.expire_at.is_some()) {
        return bad("expire_delete_markers can't be combined with expire_days or expire_at");
    }
    if r.expire_delete_markers && !r.tags.is_empty() {
        return bad("expire_delete_markers can't be used with a tag filter");
    }
    if r.abort_multipart_days.is_some() && (!r.tags.is_empty() || r.size_greater_than.is_some() || r.size_less_than.is_some()) {
        return bad("abort_multipart_days can't be used with tag or size filters");
    }
    if r.expire_at.is_some_and(|d| d.rem_euclid(86_400_000) != 0) {
        return bad("expire_at must be midnight UTC");
    }
    if r.newer_noncurrent_versions.is_some() && r.noncurrent_days.is_none() {
        return bad("newer_noncurrent_versions needs noncurrent_days");
    }
    let newer_ok = |n: Option<u32>| n.is_none_or(|n| (1..=100).contains(&n));
    if !newer_ok(r.newer_noncurrent_versions) || !r.noncurrent_transitions.iter().all(|t| newer_ok(t.newer_noncurrent_versions)) {
        return bad("newer_noncurrent_versions is 1-100");
    }
    if let (Some(gt), Some(lt)) = (r.size_greater_than, r.size_less_than)
        && gt >= lt
    {
        return bad("size_greater_than must be less than size_less_than");
    }
    tags(&r.tags)?;
    for t in &r.transitions {
        if !TRANSITION_CLASSES.contains(&t.storage_class.as_str()) {
            return bad(&format!("unknown storage class {}", t.storage_class));
        }
        if t.days.is_some() == t.date.is_some() {
            return bad("a transition has days or a date");
        }
        if t.date.is_some_and(|d| d.rem_euclid(86_400_000) != 0) {
            return bad("transition dates must be midnight UTC");
        }
    }
    for t in &r.noncurrent_transitions {
        if !TRANSITION_CLASSES.contains(&t.storage_class.as_str()) {
            return bad(&format!("unknown storage class {}", t.storage_class));
        }
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

    #[test]
    fn rules_and_ranges() {
        let mut t = BTreeMap::new();
        t.insert("k".repeat(128), "v".repeat(256));
        assert!(tags(&t).is_ok());
        for (k, v) in [(String::new(), "v".to_string()), ("k".repeat(129), "v".into()), ("k".into(), "v".repeat(257))] {
            assert!(tags(&BTreeMap::from([(k.clone(), v)])).is_err(), "{k}");
        }
        let mut h = ObjectHeaders::default();
        h.tags.insert(String::new(), "x".into());
        assert!(matches!(headers(&h), Err(StoreError::InvalidArgument(_))), "headers check their tags");
        assert!(part_number(0).is_err() && part_number(10_001).is_err());
        assert!(part_number(1).is_ok() && part_number(10_000).is_ok());
        assert!(recover_days(30).is_ok() && recover_days(31).is_err());
        assert!(matches!(key(""), Err(StoreError::InvalidArgument(_))));

        let rule = |o: &[&str], m: &[&str]| CorsRule { allowed_origins: o.iter().map(|s| s.to_string()).collect(), allowed_methods: m.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        assert!(cors(&[rule(&["https://*.x.dev"], &["GET", "PUT", "POST", "DELETE", "HEAD"])]).is_ok());
        assert!(cors(&[rule(&[], &["GET"])]).is_err());
        assert!(cors(&[rule(&["*"], &[])]).is_err());
        assert!(cors(&[rule(&["*"], &["PATCH"])]).is_err());
        assert!(cors(&[rule(&["https://*.*.dev"], &["GET"])]).is_err());
        assert!(cors(&vec![rule(&["*"], &["GET"]); 101]).is_err());

        let lc = |id: &str, expire: Option<u32>, abort: Option<u32>| LifecycleRule { id: id.into(), prefix: String::new(), enabled: true, expire_days: expire, abort_multipart_days: abort, ..Default::default() };
        let mut ok = vec![lc("", Some(1), None), lc("", None, Some(2)), lc("named", Some(3), Some(3))];
        lifecycle(&mut ok).unwrap();
        assert_eq!(ok.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["rule-1", "rule-2", "named"]);
        assert!(lifecycle(&mut [lc("a", Some(0), None)]).is_err());
        assert!(lifecycle(&mut [lc("a", None, Some(0))]).is_err());
        assert!(lifecycle(&mut [lc("a", None, None)]).is_err());
        assert!(lifecycle(&mut [lc("a", Some(1), None), lc("a", Some(2), None)]).is_err());
        assert!(lifecycle(&mut [lc("rule-2", Some(1), None), lc("", Some(2), None)]).is_err(), "a generated id can collide too");
        assert!(lifecycle(&mut vec![lc("", Some(1), None); 1001]).is_err());
    }
}
