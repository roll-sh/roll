//! roll code agents: the daemon behind `roll agent up` (docs/code/agents.md).
//!
//! The daemon is the authority for its machine, like a T3 Code server: it spawns the providers,
//! owns the files, keeps every record in an SQLite store stamped with a `seq`, and mirrors them
//! to the relay over one outbound WebSocket.

pub mod provider;
