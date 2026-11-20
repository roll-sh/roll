//! roll search control plane: the `/v1/search` API for the dashboard and the CLI.
//!
//! Clusters are served by a multi-tenant [`search_server::Node`]: loaded from object storage on
//! their first request, unloaded when idle. This crate owns what exists (SQLite,
//! [`store`]), credentials, branches (instant, or at a point in time), the request console,
//! metering ([`meter`]) and the cost estimate against Elastic Cloud and Amazon OpenSearch
//! Service ([`pricing`]). See `docs/search/architecture.md` and `docs/search/contracts.md`.
//!
//! ```ignore
//! let control = Control::new(SearchConfig::local(home), objects).await?;
//! search_server::serve(listener, control.node.clone(), ListenOptions::default());   // the ES REST port
//! let router = search_control::api::router(control.clone());                       // /v1/search on the API port
//! // on exit:
//! control.shutdown().await?;
//! ```

pub mod analytics;
pub mod api;
pub mod cluster;
pub mod control;
pub mod meter;
pub mod migrate;
pub mod pricing;
pub mod remotes;
pub mod store;
pub mod suggestions;
mod wake;

pub use control::{ClusterOptions, ConsoleRequest, ConsoleResponse, Control, ControlError, Defaults, SearchConfig, SqlCatalog};
pub use pricing::{Cost, Pricing};
pub use search_es::seal::Sealer;
/// Node setting that lets `s3` snapshot repositories reach private addresses (local development).
pub use search_es::api::snapshots::S3_ALLOW_PRIVATE;
/// Node setting that lets inference endpoints reach private addresses (local development, tests).
pub use search_es::xpack::inference::http::ALLOW_PRIVATE as INFERENCE_ALLOW_PRIVATE;
pub use search_es::api::remote::ALLOW_PRIVATE as REINDEX_ALLOW_PRIVATE;
pub use search_es::api::remote::MAX_DURATION as REINDEX_MAX_DURATION;
pub use search_es::api::remote::DENY_ADDRESSES as REINDEX_DENY_ADDRESSES;

/// Whether `n` is an address or network the reindex deny list takes.
pub fn reindex_deny_net_ok(n: &str) -> bool {
    search_es::api::remote::Net::parse(n).is_some()
}

/// Checks a time value given to a flag (`24h`, `90m`), as Elasticsearch parses one.
pub fn check_time_value(v: &str, flag: &str) -> anyhow::Result<()> {
    search_es::units::TimeValue::parse(v, flag).map(|_| ()).map_err(|e| anyhow::anyhow!("{flag}: {}", e.reason()))
}
