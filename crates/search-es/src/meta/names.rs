//! Names and identifiers: index/alias name validation with Elasticsearch's texts, and the
//! random base64 UUIDs Elasticsearch uses for indices, clusters and nodes.

use crate::error::EsError;
use base64::Engine;
use rand::RngCore;

/// `UUIDs.randomBase64UUID()`: 16 random bytes, URL-safe base64 without padding (22 chars),
/// with the version/variant bits of a type-4 UUID.
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// A 20-character URL-safe id like Elasticsearch's auto-generated document ids (time-based:
/// millisecond timestamp, a per-process sequence and random bytes), so ids generated in a burst
/// sort roughly by creation time and never collide.
pub fn time_based_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let ms = crate::time::now_millis() as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut b = [0u8; 15];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6..9].copy_from_slice(&seq.to_be_bytes()[1..]);
    rand::thread_rng().fill_bytes(&mut b[9..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The characters Elasticsearch forbids in index and alias names, as it prints them.
pub const INVALID_CHARS_TEXT: &str = "[' ','\"','*',',','/','<','>','?','\\','|']";

fn invalid_char(c: char) -> bool {
    matches!(c, ' ' | '"' | '*' | ',' | '/' | '<' | '>' | '?' | '\\' | '|')
}

/// Why `name` isn't a valid index (or alias) name, or `None`.
pub(crate) fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("must not be empty".into());
    }
    if name.chars().any(invalid_char) {
        return Some(format!("must not contain the following characters {INVALID_CHARS_TEXT}"));
    }
    if name.contains('#') {
        return Some("must not contain '#'".into());
    }
    if name.contains(':') {
        return Some("must not contain ':'".into());
    }
    if name.starts_with(['_', '-', '+']) {
        return Some("must not start with '_', '-', or '+'".into());
    }
    let len = name.len();
    if len > 255 {
        return Some(format!("index name is too long, ({len} > 255)"));
    }
    if name == "." || name == ".." {
        return Some("must not be '.' or '..'".into());
    }
    None
}

/// `MetadataCreateIndexService.validateIndexName`: the checks plus lower case.
pub fn validate_index_name(name: &str) -> Result<(), EsError> {
    if let Some(p) = name_problem(name) {
        return Err(EsError::invalid_index_name(name, &p));
    }
    if name.chars().any(|c| c.is_uppercase()) {
        return Err(EsError::invalid_index_name(name, "must be lowercase"));
    }
    Ok(())
}

/// Index name patterns of Elasticsearch's system indices (no dot-prefix deprecation warning).
const SYSTEM_INDEX_PATTERNS: &[&str] = &[
    ".tasks*", ".security*", ".async-search*", ".fleet-*", ".kibana*", ".ml-*", ".transform-internal-*", ".watches*",
    ".triggered_watches*", ".geoip_databases*", ".snapshot-blob-cache*", ".logstash*", ".apm-*", ".synonyms*", ".enrich-*",
    ".connector-secrets*", ".secrets-inference*", ".inference*", ".query-rules*", ".search-app*", ".slm-history*",
];

pub fn is_system_index(name: &str) -> bool {
    SYSTEM_INDEX_PATTERNS.iter().any(|p| crate::xcontent::filter::glob_match(p, name))
}

/// Deprecation warnings for an index name (dot-prefixed names that aren't system indices).
pub fn index_name_warnings(name: &str) -> Vec<String> {
    if name.starts_with('.') && !is_system_index(name) {
        vec![format!("Index [{name}] name begins with a dot (.), which is deprecated, and will not be allowed in a future Elasticsearch version.")]
    } else {
        Vec::new()
    }
}

/// `AliasValidator.validateAliasStandalone` name rules.
pub fn validate_alias_name(alias: &str) -> Result<(), EsError> {
    if alias.trim().is_empty() {
        return Err(EsError::illegal_argument("alias name is required"));
    }
    if let Some(p) = name_problem(alias) {
        return Err(EsError::invalid_alias_name(alias, &p));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids() {
        let u = random_uuid();
        assert_eq!(u.len(), 22);
        assert!(u.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(u, random_uuid());
    }

    #[test]
    fn index_names() {
        let r = |n: &str| validate_index_name(n).err().map(|e| e.reason().to_string());
        assert_eq!(r("UP").unwrap(), "Invalid index name [UP], must be lowercase");
        assert_eq!(r("a*b").unwrap(), "Invalid index name [a*b], must not contain the following characters [' ','\"','*',',','/','<','>','?','\\\\','|']".replace("\\\\", "\\"));
        assert_eq!(r("a#b").unwrap(), "Invalid index name [a#b], must not contain '#'");
        assert_eq!(r("a:b").unwrap(), "Invalid index name [a:b], must not contain ':'");
        assert_eq!(r("_a").unwrap(), "Invalid index name [_a], must not start with '_', '-', or '+'");
        assert_eq!(r(&"a".repeat(256)).unwrap(), format!("Invalid index name [{}], index name is too long, (256 > 255)", "a".repeat(256)));
        assert_eq!(r(".."), Some("Invalid index name [..], must not be '.' or '..'".into()));
        assert_eq!(r("aé"), None);
        assert_eq!(r(".hidden"), None);
    }
}
