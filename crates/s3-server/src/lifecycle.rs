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
