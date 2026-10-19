//! roll queues replicated log: keepers, quorum appends with fencing, S3 tiering.
//!
//! Brokers are stateless; every durable queue and stream is a [`mq_core::log::Log`]. This crate
//! provides the production backends of [`mq_core::log::LogStore`]:
//!
//! - [`keeper`]: storage nodes (one per AZ) that keep the hot tail of every log on local disk
//!   and speak a small pipelined binary protocol ([`proto`]).
//! - [`QuorumStore`]: the writer side. Appends go to all keepers in parallel and return after a
//!   write quorum has fsynced them; opens fence older owners and recover the log (see
//!   [`quorum`] for the invariants). Sealed segments tier to object storage ([`TierConfig`]),
//!   and reads below the hot tail come from there through an LRU cache.
//! - [`S3ExpressStore`]: logs directly on S3 Express One Zone, for deployments without keepers,
//!   optionally grouping the appends of all logs into one stream per broker ([`group`]).
//!
//! Wiring it into a broker:
//!
//! ```no_run
//! # async fn f() -> anyhow::Result<()> {
//! use mq_storage::{QuorumConfig, QuorumStore};
//! let store = QuorumStore::connect(QuorumConfig {
//!     keepers: vec![("10.0.1.10:9580".parse()?, "use1-az1".into()), ("10.0.2.10:9580".parse()?, "use1-az2".into()),
//!                   ("10.0.3.10:9580".parse()?, "use1-az4".into())],
//!     write_quorum: 2,
//!     az: "use1-az1".into(),
//!     ..Default::default()
//! })
//! .await?;
//! // mq_core::Broker::start(config, meta, store).await?;
//! # Ok(()) }
//! ```

pub mod client;
pub mod express;
pub mod group;
pub mod keeper;
pub mod proto;
pub mod quorum;
pub mod stats;
pub mod tier;

pub use express::{ExpressConfig, ExpressLog, S3ExpressStore};
pub use group::GroupConfig;
pub use keeper::{Keeper, KeeperConfig};
pub use quorum::{FencedError, QuorumConfig, QuorumLog, QuorumStore, ReplicaInfo};
pub use stats::{Histogram, StorageStats};
pub use tier::TierConfig;

/// Default keeper port.
pub const DEFAULT_KEEPER_PORT: u16 = 9580;
