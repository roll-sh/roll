//! roll code agents: the daemon behind `roll agent up` (docs/code/agents.md).
//!
//! The daemon is the authority for its machine, like a T3 Code server: it spawns the providers,
//! owns the files, keeps every record in an SQLite store stamped with a `seq`, and mirrors them
//! to the relay over one outbound WebSocket.

use agent_proto::{CodeGrant, Command, Delta, MachineInfo, Reply, Stamped};
use async_trait::async_trait;
use tokio::sync::broadcast;

pub mod config;
pub mod endpoints;
pub mod fs;
pub mod gateway;
pub mod github;
pub mod mcp;
pub mod git;
pub mod logo;
pub mod orchestrator;
pub mod provider;
pub mod registry;
pub mod relay;
pub mod store;
pub mod terminal;

pub use orchestrator::Host;

/// Something the daemon has to tell the relay, as it happens.
#[derive(Clone, Debug)]
pub enum Outbound {
    Entity(Box<Stamped>),
    Delta(Delta),
    /// `MachineInfo` changed (a provider's probe answered, the settings changed): send `hello`
    /// again.
    Info(MachineInfo),
}

/// What the relay link needs from the daemon. The orchestrator implements it; the relay link
/// (`relay.rs`) only talks to this, so each side is tested on its own.
#[async_trait]
pub trait Daemon: Send + Sync + 'static {
    async fn info(&self) -> MachineInfo;

    /// Stored records with `seq > after`, oldest first, at most `limit`. Each record appears
    /// once, at its newest `seq`.
    async fn since(&self, after: u64, limit: usize) -> anyhow::Result<Vec<Stamped>>;

    /// Everything from now on. Subscribe before calling `since` so nothing falls in between;
    /// duplicates are harmless (the relay keeps the newest `seq`). A lagged receiver must
    /// resubscribe and replay from its last acknowledged `seq`.
    fn subscribe(&self) -> broadcast::Receiver<Outbound>;

    /// The newest `seq` written. Every record at or below it has already been broadcast, so a
    /// command's reply waits until the link has sent everything up to the `last_seq` read after
    /// the command finished.
    async fn last_seq(&self) -> anyhow::Result<u64>;

    /// Run a command. `id` is the client's idempotency key, per sender: a second call with the
    /// same `by` and `id` returns the first call's reply without running anything (another sender
    /// may use the same id for its own command). `by` is who sent it.
    async fn run(&self, id: String, by: String, command: Command) -> Reply;

    /// [`Daemon::run`] with the roll code access the relay granted for this command (cloning,
    /// publishing). A daemon that never touches roll code ignores it.
    async fn run_granted(&self, id: String, by: String, command: Command, grant: Option<CodeGrant>) -> Reply {
        let _ = grant;
        self.run(id, by, command).await
    }
}
