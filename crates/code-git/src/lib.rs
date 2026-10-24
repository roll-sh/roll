//! git wire protocol for roll code: pkt-line, packs, smart HTTP server and client.
//!
//! This crate knows nothing about how repositories are stored. The server is generic over
//! [`GitBackend`]; code-control implements it over code-store. See docs/code/architecture.md.

pub mod backend;
pub mod budget;
pub mod client;
pub mod object;
pub mod pack;
pub mod pktline;
pub mod server;
pub mod walk;

pub use backend::{
    Access, Credentials, FetchPlan, FetchRequest, Filter, GitBackend, GitError, GitResult, ReceiveOutcome, ReceiveRequest, Ref,
    RefList, RefStatus, RefUpdate,
};
pub use budget::{BudgetError, Lease, MemoryBudget};
pub use client::{Client, FetchArgs, Fetched, PushReport};
pub use object::{CommitInfo, Kind, Object, ObjectId, TreeEntry, mode, parse_commit, parse_tag, tree_entries};
pub use pack::Limits;
pub use server::{ServerConfig, router};
