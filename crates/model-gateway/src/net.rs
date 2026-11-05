//! The gateway's HTTP client for upstreams: rustls, no redirects, timeouts, and private
//! addresses refused (a literal host, or a name resolving to one) unless the upstream allows
//! them (`UpstreamSpec::allow_private`): AGENTS.md's `PublicOnly` rule, as
//! `code-control/src/review/apps.rs` does it.
//!
//! The proxy helper owns this module.
