//! roll search front proxy and fleet protocol.
//!
//! On AWS, clients reach the search nodes through a small proxy per availability zone (see
//! `docs/search/architecture.md`, "On AWS", and `docs/search/deploy-aws.md`):
//!
//! - [`fleet`]: node membership (heartbeated records in Postgres, SQLite or memory) and the
//!   node-side [`fleet::Heartbeat`]. Shared by the nodes (`search-control`) and the proxy.
//! - [`route`]: which cluster a request names and which node should serve it (rendezvous
//!   hashing over live nodes, own AZ first).
//! - [`proxy`]: the HTTP proxy itself: routing, following owner redirects, holding requests
//!   while the fleet wakes ([`wake`]), retrying requests a node provably didn't run, TLS with
//!   SNI routing, the PROXY protocol ([`proxy_protocol`]) and graceful shutdown. Request bodies
//!   stream through and stay resendable within a memory budget ([`replay`]).
//! - [`upstream`]: TLS from the proxy to nodes, each node's certificate pinned by its record.
//! - [`lambda`]: the same image as a Lambda function URL in front of the proxies.
//! - [`pg`], [`aws`]: the Postgres connection and environment lookups both sides use.
//!
//! Ownership itself is the nodes' business: a cluster is served by the node holding its lease
//! (`search_server::lease`, the epoch fencing writes in search-store); the proxy never takes or
//! reads leases, it follows the `X-Roll-Search-Owner` header of a refusal. This keeps the proxy
//! free of the search engine: it depends on nothing else in roll search.

pub mod aws;
pub mod fleet;
pub mod lambda;
pub mod pg;
pub mod proxy;
pub mod proxy_protocol;
pub mod replay;
pub mod route;
pub mod upstream;
pub mod wake;

pub use fleet::{Beat, Heartbeat, Members, MemoryMembers, NodeRecord, PgMembers, SqliteMembers};
pub use proxy::{ClientInfo, Directory, ListenOptions, Port, Proxy, ProxyConfig};
pub use wake::{CommandWaker, EcsWaker, Waker};
