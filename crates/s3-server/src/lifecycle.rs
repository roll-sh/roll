//! Lifecycle rules between their S3 XML form (`s3_proto::lifecycle`) and the store's model.

use s3_proto::lifecycle::{NoncurrentTransition as XNoncurrent, Rule, Transition as XTransition};
use s3_store::{LifecycleRule, NoncurrentTransition, Transition};

pub(crate) fn from_xml(r: Rule) -> LifecycleRule {
    LifecycleRule {
        id: r.id.unwrap_or_default(),
        prefix: r.prefix,
        enabled: r.enabled,
        expire_days: r.expire_days,
        abort_multipart_days: r.abort_multipart_days,
        tags: r.tags.into_iter().collect(),
        size_greater_than: r.size_greater_than,
        size_less_than: r.size_less_than,
        expire_at: r.expire_date,
        expire_delete_markers: r.expire_delete_markers,
        noncurrent_days: r.noncurrent_days,
        newer_noncurrent_versions: r.newer_noncurrent_versions,
        transitions: r.transitions.into_iter().map(|t| Transition { days: t.days, date: t.date, storage_class: t.storage_class }).collect(),
        noncurrent_transitions: r
            .noncurrent_transitions
            .into_iter()
            .map(|t| NoncurrentTransition { noncurrent_days: t.noncurrent_days, newer_noncurrent_versions: t.newer_noncurrent_versions, storage_class: t.storage_class })
            .collect(),
        legacy_prefix: r.legacy_prefix,
    }
}

pub(crate) fn to_xml(r: &LifecycleRule) -> Rule {
    Rule {
        id: Some(r.id.clone()).filter(|s| !s.is_empty()),
        enabled: r.enabled,
        legacy_prefix: r.legacy_prefix,
        prefix: r.prefix.clone(),
        tags: r.tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        size_greater_than: r.size_greater_than,
        size_less_than: r.size_less_than,
        expire_days: r.expire_days,
        expire_date: r.expire_at,
        expire_delete_markers: r.expire_delete_markers,
        noncurrent_days: r.noncurrent_days,
        newer_noncurrent_versions: r.newer_noncurrent_versions,
        transitions: r.transitions.iter().map(|t| XTransition { days: t.days, date: t.date, storage_class: t.storage_class.clone() }).collect(),
        noncurrent_transitions: r
            .noncurrent_transitions
            .iter()
            .map(|t| XNoncurrent { noncurrent_days: t.noncurrent_days, newer_noncurrent_versions: t.newer_noncurrent_versions, storage_class: t.storage_class.clone() })
            .collect(),
        abort_multipart_days: r.abort_multipart_days,
    }
}

/// `x-amz-expiration` of the current version `meta`: when the earliest enabled `Expiration`
/// rule matching it (by prefix, tags and size) makes it eligible, as roll counts it (`Days`
/// from the version's time, without S3's rounding to midnight, or the rule's `Date`).
pub(crate) fn expiration_header(bucket: &s3_store::Bucket, meta: &s3_store::ObjectMeta) -> Option<String> {
    const DAY_MS: i64 = 86_400_000;
    bucket
        .lifecycle
        .iter()
        .filter(|r| r.enabled && r.matches(&meta.key, &meta.headers.tags, meta.size))
        .filter_map(|r| {
            let at = match (r.expire_days, r.expire_at) {
                (Some(days), _) => meta.modified_at.saturating_add(i64::from(days) * DAY_MS),
                (None, Some(at)) => at,
                _ => return None,
            };
            Some((at, r.id.as_str()))
        })
        .min()
        // HTTP dates are whole seconds: round up, never showing a time before the real one.
        .map(|(at, id)| format!("expiry-date=\"{}\", rule-id=\"{id}\"", s3_proto::time::http_date(at.saturating_add(999))))
}

/// Set `x-amz-expiration` on a response about the current version `meta`.
pub(crate) fn set_expiration(r: &mut axum::response::Response, bucket: &s3_store::Bucket, meta: &s3_store::ObjectMeta) {
    if let Some(v) = expiration_header(bucket, meta) {
        crate::response::set(r, "x-amz-expiration", &v);
    }
}
