//! roll code on many nodes: which node owns which repo, and getting requests there.
//!
//! Every node runs the same code against one database (Postgres in a cluster, SQLite for one
//! node, through `code-sql`). This crate is generic over string keys: a repo id, or
//! `singleton:<name>` for a cluster-wide single worker ([`Cluster::singleton`]). The design is
//! `docs/code/cluster.md`.
//!
//! # Pieces
//!
//! - **Membership.** [`Cluster::join`] inserts a `code_nodes` row; a dedicated OS thread (not a
//!   tokio task: a starved runtime must not make a live node look dead) heartbeats it every
//!   `heartbeat`. A node is live while its heartbeat is younger than `node_ttl` on the
//!   database's clock. [`Cluster::leave`] (and drop) deletes the row; [`Cluster::drain`] hands
//!   everything off first.
//! - **Leases.** One `code_leases` row per key: `(key, node_id, epoch, expires_at)`.
//!   [`Cluster::acquire`] is a compare-and-swap that only takes a row that's free, expired on
//!   the database's clock, or already ours, and bumps `epoch` when the holder changes. The same
//!   thread renews all of this node's leases in one statement every `renew_every`.
//! - **Placement.** Rendezvous hashing of `(key, node_id)` over live, non-draining nodes
//!   ([`Cluster::preferred`], [`placement`]).
//! - **Routing.** [`Cluster::route`]: [`Route::Local`] with a [`Guard`] if this node holds a
//!   valid lease; [`Route::Remote`] to the holder (or to the preferred node of a free key);
//!   [`Route::Unavailable`] while a key moves. A free key whose preferred node is this one is
//!   acquired on the spot.
//! - **Sweeper.** Every `sweep_every`, expired leases (a bounded page at a time) whose
//!   preferred node is this one are taken.
//! - **Handoff and rebalancing.** To hand a key off, the node stops granting guards for it
//!   (routes answer Unavailable, [`Guard::ending`] fires so event streams end), waits until its
//!   guards are dropped (or `handoff_timeout`), emits [`Ownership::Lost`], and then expires the
//!   lease so the preferred node takes it at its next sweep (or at once, on a request). Every
//!   `rebalance_every`, up to `handoffs_per_round` keys whose preferred node is another live
//!   node are handed off.
//! - **Ownership events** ([`Cluster::subscribe`]) start and stop a repo's background work.
//! - **Signed internal requests** ([`sign`]) and **forwarding** ([`Cluster::dispatch`],
//!   [`Cluster::forward`], [`accept_forwarded`]).
//!
//! # Timing invariants (why a node stops before anyone else starts)
//!
//! Each node keeps, per lease, a local deadline on its own monotonic clock
//! (`CLOCK_BOOTTIME` on Linux, which keeps counting through a suspend):
//!
//! ```text
//! deadline = t_sent + lease_ttl - lease_margin
//! ```
//!
//! where `t_sent` is when the acquisition or renewal that succeeded was **sent**, not when it
//! returned. The database sets `expires_at = db_now + lease_ttl` while it runs the statement,
//! which is after `t_sent`. So, as long as the node's clock and the database's clock advance
//! at rates within `lease_margin / lease_ttl` of each other (30% by default), the server-side
//! expiry comes at least `lease_margin` after the local deadline, however long the statement
//! took or however long the answer was delayed:
//!
//! ```text
//! t_sent ──(statement runs: expires_at = db_now + ttl)──▶ local deadline ──margin──▶ expires_at
//! ```
//!
//! - **Self-fencing.** A lease past its local deadline is never used: [`Cluster::owns`] is
//!   false, `route` answers Unavailable and grants no guard. A watchdog thread of its own (so
//!   a renewal stuck in the database for many seconds can't delay it) emits
//!   [`Ownership::Lost`] at the deadline. A node cut off from the database therefore stops
//!   acting as owner of every key before any other node can take one.
//! - **No early takeover.** A lease is only taken once `expires_at <= now` on the database's
//!   clock, in the same statement that takes it. Heartbeat age never lets a node take a lease
//!   early; it only steers placement.
//! - **Renewals never revive.** A renewal only extends leases that haven't expired; an expired
//!   lease may be about to be taken by someone else.
//! - `renew_every <= (lease_ttl - lease_margin) / 2`, so at least two renewals are tried
//!   before a lease is fenced; failed renewals are retried after `renew_every / 4`.
//! - `node_ttl <= lease_ttl`: a dead node drops out of placement before its leases expire, so
//!   the node that sweeps them is the right one.
//! - Late answers are safe: an acquisition that comes back after its deadline is not used, and
//!   the next renewal adopts or releases whatever the database says this node holds.
//! - A node id pinned across restarts can't run twice: each process registers a new
//!   incarnation (`started_at`), and renewals and acquisitions only work for the current one.
//!   A new process with the same id waits until the old one looks dead and its leases expired.
//!
//! Correctness of repo data never rests on leases (ref updates are compare-and-swaps, writes
//! are serializable); leases are about locality. A request that got its guard just before a
//! deadline may still be running after it: long operations should check
//! [`Guard::is_valid`], and their writes must be compare-and-swaps.
//!
//! # Ownership events
//!
//! [`Cluster::subscribe`] returns an unbounded receiver of [`Ownership`]. It first replays
//! `Acquired` for every key held, then every change. Per key, events strictly alternate
//! `Acquired`, `Lost`, `Acquired`, …: `Lost` is always delivered before a later `Acquired` of
//! the same key on this node. `Lost` comes before the lease is released (handoff), at the
//! local deadline (fencing), or when a renewal finds the lease gone. `Acquired` comes when
//! `route`, `acquire`, the sweeper or `singleton` take a key, or when a renewal finds a lease
//! still ours that this node had stopped using (after a database outage shorter than the
//! lease).
//!
//! # Forwarding
//!
//! [`Cluster::dispatch`] runs a request where it belongs. Remote requests go to the owner's
//! URL with the original path and query, method and headers (minus hop-by-hop ones; `Host` is
//! kept), streaming bodies both ways (git pushes, server-sent events), signed, with
//! `x-roll-code-hop` and `x-roll-code-client`. Bodies with a known length up to
//! `forward_buffer` (4 MiB) are buffered so the forward can be retried once, after routing
//! again, on a connection error or on an Unavailable answer (a 503 carrying
//! `x-roll-code-unavailable`, from [`unavailable`]); bigger or unsized bodies stream with one
//! attempt. Past [`MAX_HOPS`] forwards the answer is 508. The receiving side runs
//! [`accept_forwarded`]: requests carrying `x-roll-code-*` headers must verify (else 401), and
//! get a [`Forwarded`] extension with the sender, hop count and client address.
//!
//! # Blocking
//!
//! `code-sql` is synchronous, so [`Cluster::join`], [`Cluster::acquire`], [`Cluster::route`]
//! (when it must read the lease) and [`Cluster::leave`] block on the database. From async code
//! use [`Cluster::route_async`] / [`Cluster::dispatch`], which answer from memory when they can
//! and use the blocking pool otherwise. [`Cluster::owns`], [`Cluster::guard`] and
//! [`Cluster::preferred`] never touch the database.

mod clock;
mod cluster;
mod config;
mod forward;
pub mod placement;
pub mod sign;
mod sql;

pub use cluster::{Cluster, Guard, Node, Ownership, Route};
pub use config::ClusterConfig;
pub use forward::{ClientAddr, Forwarded, MAX_HOPS, accept_forwarded, client_addr, hops, is_upgrade, require_signed, unavailable};
pub use sign::VerifyError;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] code_sql::Error),
    #[error("invalid cluster configuration: {0}")]
    Config(String),
    /// Another live process runs as this node id (or its leases haven't expired yet).
    #[error("node id {0} is in use by another live process")]
    NodeIdInUse(String),
    /// Another process joined with this node id; this one is no longer a member.
    #[error("this node id was taken over by another process")]
    Superseded,
    /// A test fault is active.
    #[error("{0}")]
    Fault(&'static str),
    #[error("{0}")]
    Internal(String),
}
