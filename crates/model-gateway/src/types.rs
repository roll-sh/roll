//! What the modules hand each other.

use agent_proto::{Attempt, MachineId, WireFormat};
use zeroize::Zeroizing;

/// A key in memory (an upstream's, or a client key just minted): never printed, wiped when
/// dropped.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(s: String) -> Self {
        Secret(Zeroizing::new(s))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Tokens a response used, as its API reports them (Anthropic's `input_tokens` excludes cached
/// tokens; OpenAI's `prompt_tokens`/`input_tokens` include them, so `input` here is OpenAI's
/// minus `cache_read`: the same meaning whichever API answered).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// The client a request's key belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRef {
    /// `gwc_…`.
    pub id: String,
    pub machine: Option<MachineId>,
}

/// One finished request, for the store (usage roll-ups and the request log).
#[derive(Clone, Debug)]
pub struct Record {
    pub at: u64,
    pub client: Option<ClientRef>,
    /// From the member daemon's `x-roll-project` / `x-roll-thread` headers (ids only).
    pub project: Option<String>,
    pub thread: Option<String>,
    pub format: WireFormat,
    /// What the client asked for.
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub ms: u32,
    pub first_byte_ms: Option<u32>,
    /// Every upstream tried; the last one answered.
    pub attempts: Vec<Attempt>,
    pub usage: Usage,
    pub error: Option<String>,
    /// Only when `log_bodies` is on: at most 64 KiB each.
    pub bodies: Option<(String, String)>,
}
