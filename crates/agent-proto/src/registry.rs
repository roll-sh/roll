//! The ACP registry on a machine (docs/code/agents.md, "The ACP registry"): which agents the
//! registry lists and how each would install there (`registry.list`), and the ones the daemon
//! installed itself (`registry.install`, [`crate::MachineConfig::registry`]). The daemon fetches
//! the registry and installs; the relay only forwards. Shapes follow T3 Code's
//! `packages/contracts/src/acpRegistry.ts` (`AcpRegistrySearchAgent`).

use crate::{Millis, ProviderId};
use serde::{Deserialize, Serialize};

/// How the registry distributes an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryKind {
    /// An archive (or a bare executable) for this machine's OS and architecture.
    Binary,
    /// An npm package, installed with the machine's own `npm` into the daemon's directory.
    Npx,
    /// A Python package, installed with the machine's own `uv` into the daemon's directory.
    Uvx,
}

/// What vouches for the bytes an install would run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryIntegrity {
    /// A binary whose SHA-256 the registry lists: the download must match it.
    Sha256,
    /// A package pinned to an exact version on npm or PyPI (T3's `registry`).
    Registry,
    /// A binary the registry gives no checksum for.
    None,
}

/// One agent of the registry, as a daemon sees it (T3's `AcpRegistrySearchAgent`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryAgent {
    /// The registry's id (`goose`, `github-copilot-cli`); the provider it installs as is
    /// `acp:<id>`.
    pub id: String,
    pub name: String,
    /// The registry's current version of it.
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Its icon on the registry's CDN (an SVG).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The distributions that suit this machine, in the order an install tries them: a binary only
    /// when there is one for its OS and architecture. Empty: none does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<RegistryKind>,
    /// `registry.install` can install it here: a binary for this machine, or `npm`/`uv` on its
    /// `PATH` for a package.
    pub installable: bool,
    /// What vouches for what an install would run (the first installable kind's).
    pub integrity: RegistryIntegrity,
    /// The version the daemon installed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    /// The provider on this machine that is this agent already (installed by the daemon, set up
    /// in `config.toml`, found on `PATH`, or a native driver of the same agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offered: Option<ProviderId>,
}

/// An agent the daemon installed from the registry ([`crate::MachineConfig::registry`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryInstalled {
    /// The registry's id.
    pub id: String,
    pub version: String,
    /// `acp:<id>`.
    pub provider: ProviderId,
    pub kind: RegistryKind,
    pub installed_at: Millis,
}

/// What `registry.list` answers ([`crate::Reply::Registry`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryList {
    pub agents: Vec<RegistryAgent>,
    /// When the daemon last fetched the registry (0: never).
    pub fetched_at: Millis,
    /// Why the list is old or empty: the registry couldn't be fetched just now, and this is the
    /// copy the daemon kept (or nothing, when it has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The registry's agent ids: `^[a-z0-9][a-z0-9._-]*$`, at most 128 bytes (T3's
/// `AcpRegistryAgentId`).
pub fn is_registry_id(id: &str) -> bool {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    id.len() <= 128 && id.bytes().next().is_some_and(alnum) && id.bytes().all(|b| alnum(b) || b"._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids() {
        for ok in ["goose", "github-copilot-cli", "opencode-v2", "a.b_c", "0x"] {
            assert!(is_registry_id(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "Goose", "a/b", "a b", &"a".repeat(129)] {
            assert!(!is_registry_id(bad), "{bad}");
        }
    }
}
