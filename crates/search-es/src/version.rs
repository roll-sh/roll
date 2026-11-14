//! The Elasticsearch (and OpenSearch) version roll search reports.

pub const ES_VERSION: &str = "9.5.5";
pub const ES_BUILD_HASH: &str = "48acba663b09e70b115dd879329141506c377890";
pub const ES_BUILD_DATE: &str = "2026-09-30T22:12:03.984586187Z";
pub const LUCENE_VERSION: &str = "10.5.1";
pub const MIN_WIRE_COMPAT: &str = "8.19.0";
pub const MIN_INDEX_COMPAT: &str = "8.0.0";
/// `index.version.created` of indices created by this version.
pub const INDEX_VERSION_ID: &str = "9111000";
/// The oldest index version this version reads (8.0.0), as `DiscoveryNode` prints it.
pub const MIN_INDEX_VERSION_ID: &str = "8000099";
/// `index.version.created_string` of indices created by this version (`?human`).
pub const INDEX_VERSION_STRING: &str = "9.5.2-9.5.5";
/// The transport version Elasticsearch 9.5.5 reports.
pub const TRANSPORT_VERSION: i64 = 9458013;
pub const TAGLINE: &str = "You Know, for Search";

pub const OPENSEARCH_VERSION: &str = "3.9.0";
pub const OPENSEARCH_LUCENE_VERSION: &str = "10.5.1";
/// `index.version.created` of indices OpenSearch 3.9.0 creates.
pub const OPENSEARCH_INDEX_VERSION_ID: &str = "137307827";
pub const OPENSEARCH_BUILD_HASH: &str = "4ee42a94e87f66fbf1e62a9871b1b87f91e02472";
pub const OPENSEARCH_BUILD_DATE: &str = "2026-09-23T01:01:31.883915140Z";
pub const OPENSEARCH_TAGLINE: &str = "The OpenSearch Project: https://opensearch.org/";

/// The OpenSearch dialect's `Warning` prefix.
pub fn opensearch_warning_prefix() -> String {
    format!("299 OpenSearch-{OPENSEARCH_VERSION}-{OPENSEARCH_BUILD_HASH}")
}

/// The prefix of deprecation `Warning` headers: `299 Elasticsearch-<version>-<hash>`.
pub fn warning_prefix() -> String {
    format!("299 Elasticsearch-{ES_VERSION}-{ES_BUILD_HASH}")
}
