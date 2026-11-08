//! Where membership and leases are kept: the [`Backend`] trait and its three implementations.
//!
//! The lease state machine ([`crate::Cluster`]) is the same on every backend; a backend only
//! answers "who is alive", "which leases does this node hold" and "take, release or delete
//! these", each as one atomic step. What a backend promises:
//!
//! - **One holder.** [`Backend::acquire`] takes a key only if it's free, or its holder is gone
//!   by the backend's rule, or it's already this node's; two nodes can't both win.
//! - **Deadlines.** Every [`Confirmed`] carries a `basis` (on [`crate::clock`]): no other node
//!   can take any of the confirmed leases before `basis + lease_ttl` (allowing for clock rates
//!   within `lease_margin / lease_ttl` of each other). The state machine acts on them only until
//!   `basis + lease_ttl - lease_margin` (self-fencing).
//! - **No revival.** A renewal never extends a lease that another node may already be taking.
//! - **Incarnations.** [`Backend::join`] waits until a previous process with the same node id
//!   looks dead and its leases can be taken, then registers a new incarnation; the old one's
//!   heartbeats and acquisitions fail from then on ([`crate::Error::Superseded`]).
//!
//! How each backend decides that a holder is gone:
//!
//! | backend | liveness | renewals |
//! | --- | --- | --- |
//! | [`Memory`] | an `expires_at` on the process's monotonic clock | one pass over the node's leases |
//! | [`Postgres`] | an `expires_at` on the database's clock | one `UPDATE … WHERE node_id = me` |
//! | [`DynamoDb`] | the holder's node item stops changing for `lease_ttl`, as seen on the contender's own clock | one conditional write of the node item |
//!
//! Calls block (the state machine runs them on its own threads, or on Tokio's blocking pool).

mod dynamodb;
mod memory;
mod postgres;

pub use dynamodb::{DynamoDb, DynamoDbConfig};
pub use memory::{Memory, MemoryStore};
pub use postgres::Postgres;

use crate::{Node, Result};
use std::time::Duration;

/// What a node registers as.
#[derive(Clone, Debug)]
pub struct Registration {
    pub node_id: String,
    pub url: String,
    /// Where it runs (an availability zone), recorded for placement hints.
    pub zone: Option<String>,
}

/// The timings a backend needs (from [`crate::ClusterConfig`]).
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// A node is live while it heartbeated within this.
    pub node_ttl: Duration,
    /// How long a lease outlives its holder's last renewal.
    pub lease_ttl: Duration,
    /// How often the node renews (the DynamoDB backend's node writes).
    pub renew_every: Duration,
}

/// A live member, as last seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub node: Node,
    pub zone: Option<String>,
    pub draining: bool,
}

/// Leases the backend confirmed this node holds, and when the confirmation was sent (on
/// [`crate::clock`]): see the module docs, "Deadlines".
#[derive(Debug)]
pub struct Confirmed {
    pub leases: Vec<(String, i64)>,
    pub basis: Duration,
    /// How many of `leases` this answer took from another holder that never released them (it
    /// died, was cut off, or left with requests in flight): takeovers, not planned handoffs.
    /// Always 0 for a renewal.
    pub taken_over: usize,
}

/// Who holds a key, for routing.
#[derive(Clone, Debug)]
pub enum Holder {
    /// Nobody that counts: no lease, a released or expired one, or one whose holder is gone.
    /// Its preferred node may take it.
    Free,
    /// This node, as far as the backend knows (it may not be usable here: being released,
    /// fenced, or about to be confirmed by the next renewal). `left`: until it can be taken.
    Me { left: Duration },
    /// Another node. `url` is `None` when its membership record is gone; `live` when it
    /// heartbeated recently. `left`: until it can be taken if the holder stays silent.
    Other { id: String, url: Option<String>, live: bool, left: Duration },
}

/// Membership and leases for one node: each node has a backend value of its own, bound to its
/// registration by [`Backend::join`].
pub trait Backend: Send + Sync + 'static {
    /// `memory`, `postgres` or `dynamodb`.
    fn name(&self) -> &'static str;
    /// Register `me`, waiting (up to about two lease lengths) for a previous incarnation with the
    /// same id to look dead and its leases to become takeable. Called once, before anything else.
    fn join(&self, me: &Registration, t: Timing) -> Result<()>;
    /// Prove this node is alive (a backend whose renewals are the heartbeat may do nothing).
    /// [`crate::Error::Superseded`] once another process took the id.
    fn heartbeat(&self, draining: bool) -> Result<()>;
    /// The live members, this node included while its heartbeats go through.
    fn members(&self) -> Result<Vec<Member>>;
    /// Mark this node draining (other nodes stop placing keys on it).
    fn set_draining(&self) -> Result<()>;
    /// Renew every lease this node holds: every lease the backend says is this node's, so the
    /// caller adopts one whose acquisition answer was lost (or that it fenced while the backend
    /// was slow), and releases again one it meant to give up.
    fn renew(&self) -> Result<Confirmed>;
    /// Take each key that's free (by the backend's rule) or already ours; the ones held now.
    fn acquire(&self, keys: &[String]) -> Result<Confirmed>;
    /// Make leases of ours free at once (same holder and epoch only), so another node may take
    /// them; nothing is deleted.
    fn release(&self, leases: &[(String, i64)]) -> Result<()>;
    /// Delete leases of ours (same holder and epoch only): nobody takes them until asked to.
    fn delete(&self, leases: &[(String, i64)]) -> Result<()>;
    /// The next page (at most `page`) of keys that may be takeable, and whether more pages
    /// follow. The backend keeps the cursor; a round ends when no more pages follow.
    fn sweep(&self, page: usize) -> Result<(Vec<String>, bool)>;
    /// Who holds `key` now.
    fn lookup(&self, key: &str) -> Result<Holder>;
    /// Remove this node's membership. `released_all`: every lease it held was released (else
    /// the rest lapse as a dead node's would).
    fn leave(&self, released_all: bool) -> Result<()>;
    /// Remove long-dead members' records.
    fn cleanup(&self) -> Result<()>;
    /// `key`'s lease record, if there is one: the node it names (empty once released, on
    /// backends that forget the releaser) and its epoch. For diagnostics and tests; a record
    /// doesn't mean the lease is held.
    fn holder(&self, key: &str) -> Result<Option<(String, i64)>>;
}
