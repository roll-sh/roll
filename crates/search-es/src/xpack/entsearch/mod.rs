//! Enterprise search: connectors and their sync jobs ([`connectors`]), search applications and
//! behavioral analytics collections ([`search_application`]), and query rules
//! ([`query_rules`], including the `rule` query's rewrite).
//!
//! All of them are stored objects (in Elasticsearch, documents of the `.elastic-connectors*`,
//! `.search-app-1` and `.query-rules-2` system indices); roll keeps them in the cluster
//! metadata with the same shapes, defaults, validation and state machines. roll is the
//! connector service of the Elastic-managed connectors of the services it implements
//! ([`sync`]: `postgresql` and `s3`); every other connector's jobs move only through the API
//! calls the tenant's own connector service makes.

pub mod connectors;
pub mod query_rules;
pub mod search_application;
pub mod sync;

/// The current time with nanoseconds, as Elasticsearch prints `Instant`s in these documents.
pub fn now_nanos_string() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    crate::time::java_instant_string_nanos(crate::time::Instant { secs: d.as_secs() as i64, nanos: d.subsec_nanos() })
}
