//! Provider endpoints (docs/code/agents.md, "Endpoints"): a base URL and a key a machine's
//! providers use instead of (or besides) their own login: CLIProxyAPI, any Anthropic- or
//! OpenAI-compatible gateway, or a provider's own variables.
//!
//! The owner sets them from roll, and a key never passes through roll in the clear: the browser
//! seals it to the target daemon's public key ([`MachineInfo::seal_key`](crate::MachineInfo)),
//! the relay forwards only the ciphertext ([`SealedSecret`]) and keeps none, and only the daemon
//! can open it. What the daemon reports back ([`EndpointInfo`]) says whether a key is set, never
//! any part of it. A key may instead be a 1Password reference (`op://…`) the daemon reads on the
//! machine (T3 Code's secret providers, `packages/secret-provider-onepassword` there).
//!
//! Profiles ([`EndpointProfile`]) are the owner's endpoints kept on the relay without a key, to
//! apply to many machines at once: the browser seals the key once per machine.

use crate::{Millis, ProviderId};
use serde::{Deserialize, Serialize};

/// The limits [`EndpointSpec::check`] and [`SealedSecret::check`] hold endpoints to.
pub mod endpoint_limits {
    pub const ID: usize = 64;
    pub const NAME: usize = 64;
    pub const URL: usize = 2048;
    pub const ENV_NAME: usize = 128;
    pub const KEY_REF: usize = 512;
    /// A key's plaintext (an API key, a JWT).
    pub const KEY: usize = 8192;
    /// A sealed key's base64 ciphertext: the key plus the 16-byte tag, base64.
    pub const SEALED: usize = 11_000;
    /// Endpoints per machine, and profiles per owner.
    pub const ENDPOINTS: usize = 32;
    /// Models kept from one endpoint's `/models`.
    pub const MODELS: usize = 500;
    pub const MODEL_ID: usize = 256;
}

/// What an endpoint speaks, which decides the variables a provider gets and how its models are
/// listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// The Anthropic Messages API (`/v1/messages`): Claude Code's `ANTHROPIC_BASE_URL` and
    /// `ANTHROPIC_AUTH_TOKEN`; models from `/v1/models`.
    Anthropic,
    /// The OpenAI API: the base URL includes `/v1` (`OPENAI_BASE_URL`); Codex gets a model
    /// provider of its own; models from `<base>/models`.
    Openai,
    /// CLIProxyAPI (github.com/router-for-me/CLIProxyAPI): one server with the Anthropic,
    /// OpenAI (chat and responses) and Gemini routes at its root, so every provider can use it.
    Cliproxy,
    /// The provider's own variables: the base URL goes in `url_env` and the key in `key_env`.
    Custom,
}

impl EndpointKind {
    pub fn label(self) -> &'static str {
        match self {
            EndpointKind::Anthropic => "Anthropic-compatible",
            EndpointKind::Openai => "OpenAI-compatible",
            EndpointKind::Cliproxy => "CLIProxyAPI",
            EndpointKind::Custom => "Custom variables",
        }
    }
}

/// An endpoint as the owner sets it: everything but the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSpec {
    /// Chosen by the client (`[a-z0-9_-]`, at most 64): the same id on several machines is one
    /// profile applied to them. The sealed key is bound to it.
    pub id: String,
    pub name: String,
    pub kind: EndpointKind,
    /// `http://` or `https://`. Private and tailnet addresses are fine: the daemon calls them,
    /// roll never does.
    pub base_url: String,
    /// The providers that use it. A provider uses at most one endpoint on a machine: setting an
    /// endpoint takes its providers from the machine's other endpoints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// `custom`: the variable the base URL goes in (unset: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_env: Option<String>,
    /// `custom`: the variable the key goes in (unset: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// The key is read on the machine from 1Password (`op://vault/item/field`, with the `op`
    /// CLI signed in there) instead of being sealed to the daemon. A reference isn't a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_ref: Option<String>,
}

impl EndpointSpec {
    /// Trimmed, or why it can't be kept (an `invalid` reply, a 400 from the relay).
    pub fn check(mut self) -> Result<EndpointSpec, String> {
        use endpoint_limits as l;
        if !valid_id(&self.id) {
            return Err(format!("an endpoint id is 1 to {} of a-z, 0-9, - and _", l::ID));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.len() > l::NAME || self.name.chars().any(|c| c.is_control() || c == '<' || c == '>') {
            return Err(format!("an endpoint's name is one line of 1 to {} bytes, without < or >", l::NAME));
        }
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        check_url(&self.base_url)?;
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if p.0.is_empty() || p.0.len() > l::ID || !seen.insert(p.0.clone()) {
                return Err("list each provider once".into());
            }
        }
        let text = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.url_env = text(self.url_env.take());
        self.key_env = text(self.key_env.take());
        self.key_ref = text(self.key_ref.take());
        for name in [&self.url_env, &self.key_env].into_iter().flatten() {
            if !valid_env_name(name) {
                return Err(format!("{name:?} isn't an environment variable name (letters, digits and _, not starting with a digit)"));
            }
        }
        if self.kind == EndpointKind::Custom {
            if self.url_env.is_none() && self.key_env.is_none() {
                return Err("a custom endpoint names the variable for its base URL, its key, or both".into());
            }
        } else if self.url_env.is_some() || self.key_env.is_some() {
            return Err(format!("only a custom endpoint names its variables ({} sets its own)", self.kind.label()));
        }
        if let Some(r) = &self.key_ref
            && (!r.starts_with("op://") || r.len() > l::KEY_REF || r.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err(format!("a 1Password reference looks like op://vault/item/field (at most {} bytes, no spaces)", l::KEY_REF));
        }
        Ok(self)
    }

    /// Whether `provider` uses it.
    pub fn serves(&self, provider: &ProviderId) -> bool {
        self.providers.contains(provider)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= endpoint_limits::ID && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= endpoint_limits::ENV_NAME
        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An `http(s)://host…` URL without credentials, spaces or a fragment.
fn check_url(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    let Some(rest) = rest else { return Err("an endpoint's base URL starts with http:// or https://".into()) };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if url.len() > endpoint_limits::URL || authority.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) || url.contains('#') {
        return Err(format!("an endpoint's base URL is http(s)://host[:port][/path], at most {} bytes", endpoint_limits::URL));
    }
    if authority.contains('@') {
        return Err("put the key in the key field, not in the URL".into());
    }
    Ok(())
}

/// A key sealed to one daemon (the `roll-seal-v1` box): X25519 between a fresh key pair of the
/// sender's (`epk`) and the daemon's `seal_key`, HKDF-SHA256 (salt `epk ‖ seal_key`, info
/// `roll-seal-v1`) to an AES-256-GCM key, a random 12-byte `nonce`, and the endpoint's id as the
/// associated data (`roll-endpoint:<id>`), so the box opens only on that daemon and for that
/// endpoint. All three are base64url without padding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedSecret {
    pub v: u8,
    pub epk: String,
    pub nonce: String,
    pub ct: String,
}

/// The version of [`SealedSecret`] this crate describes.
pub const SEAL_VERSION: u8 = 1;

impl SealedSecret {
    /// Its shape (the version and lengths); only the daemon can tell whether it opens.
    pub fn check(&self) -> Result<(), String> {
        let b64 = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let ok = self.v == SEAL_VERSION
            && self.epk.len() == 43
            && self.nonce.len() == 16
            && (24..=endpoint_limits::SEALED).contains(&self.ct.len())
            && b64(&self.epk)
            && b64(&self.nonce)
            && b64(&self.ct);
        if ok { Ok(()) } else { Err("the sealed key isn't a roll-seal-v1 box".into()) }
    }

    /// The associated data a key for `endpoint` is sealed with.
    pub fn context(endpoint: &str) -> String {
        format!("roll-endpoint:{endpoint}")
    }
}

/// Never prints the ciphertext (it is only ever useful to its daemon, but logs are no place for
/// it either).
impl std::fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealedSecret(v{}, {} bytes)", self.v, self.ct.len())
    }
}

/// What `endpoint.set` does to the endpoint's key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeyChange {
    /// Leave it as it is (a new endpoint: none).
    #[default]
    Keep,
    /// Forget it.
    Clear,
    /// A new key, sealed to the daemon.
    Set { sealed: SealedSecret },
}

impl KeyChange {
    fn is_keep(&self) -> bool {
        matches!(self, KeyChange::Keep)
    }
}

/// Serde's `skip_serializing_if` for [`KeyChange`].
pub fn key_change_is_keep(k: &KeyChange) -> bool {
    k.is_keep()
}

/// Where an endpoint's key comes from, as the daemon reports it. Never any part of the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    /// No key (a local proxy that takes anything, or not set yet).
    #[default]
    None,
    /// Sealed from roll and kept by the daemon.
    Sealed,
    /// Read from 1Password on the machine (`key_ref`).
    Reference,
}

/// A model an endpoint lists (its `/models`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// An endpoint as its daemon reports it, in `MachineConfig::endpoints`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointInfo {
    #[serde(flatten)]
    pub spec: EndpointSpec,
    #[serde(default)]
    pub key: KeyState,
    /// What its `/models` listed at the last check (each provider that uses it offers them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<EndpointModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<Millis>,
    /// Why the last check failed (the key didn't open, the server didn't answer, 1Password
    /// couldn't read the reference). Never holds the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `endpoint.check`'s answer: the daemon asked the endpoint for its models just now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCheck {
    pub ok: bool,
    /// How many models it listed.
    #[serde(default)]
    pub models: u32,
    /// How long it took to answer.
    #[serde(default)]
    pub ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An owner's endpoint kept on the relay to apply to machines (`/v1/code/agents/{owner}/endpoints`).
/// It has no key, and the relay keeps none: applying it seals the key to each machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointProfile {
    #[serde(flatten)]
    pub spec: EndpointSpec,
    /// Offered first, and picked, in the new-sandbox dialog.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sandbox_default: bool,
    #[serde(default)]
    pub updated_at: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EndpointSpec {
        EndpointSpec {
            id: "cliproxy".into(),
            name: " CLIProxyAPI ".into(),
            kind: EndpointKind::Cliproxy,
            base_url: " http://100.64.0.7:8317/ ".into(),
            providers: vec![ProviderId::claude(), ProviderId::codex()],
            url_env: None,
            key_env: None,
            key_ref: None,
        }
    }

    #[test]
    fn specs_are_trimmed_and_checked() {
        let s = spec().check().unwrap();
        assert_eq!(s.name, "CLIProxyAPI");
        assert_eq!(s.base_url, "http://100.64.0.7:8317");
        let bad = |f: fn(&mut EndpointSpec)| {
            let mut s = spec();
            f(&mut s);
            s.check().unwrap_err()
        };
        assert!(bad(|s| s.id = "Bad Id".into()).contains("id"));
        assert!(bad(|s| s.base_url = "ftp://x".into()).contains("http"));
        assert!(bad(|s| s.base_url = "https://user:sk-1@x".into()).contains("key field"));
        assert!(bad(|s| s.base_url = "https:///v1".into()).contains("host"));
        assert!(bad(|s| s.providers.push(ProviderId::claude())).contains("once"));
        assert!(bad(|s| s.key_env = Some("GEMINI_API_KEY".into())).contains("only a custom"));
        assert!(bad(|s| s.key_ref = Some("vault/item".into())).contains("op://"));
        assert!(bad(|s| s.kind = EndpointKind::Custom).contains("names the variable"));
        let custom = EndpointSpec { kind: EndpointKind::Custom, key_env: Some("GEMINI_API_KEY".into()), ..spec() };
        assert!(custom.check().is_ok());
        let custom = EndpointSpec { kind: EndpointKind::Custom, key_env: Some("1X".into()), ..spec() };
        assert!(custom.check().is_err());
    }

    #[test]
    fn sealed_keys_are_shapes_only_and_never_printed() {
        let s = SealedSecret { v: 1, epk: "A".repeat(43), nonce: "B".repeat(16), ct: "C".repeat(40) };
        assert!(s.check().is_ok());
        assert_eq!(format!("{s:?}"), "SealedSecret(v1, 40 bytes)");
        assert!(SealedSecret { v: 2, ..s.clone() }.check().is_err());
        assert!(SealedSecret { epk: "A".repeat(42), ..s.clone() }.check().is_err());
        assert!(SealedSecret { ct: "C+/=".repeat(10), ..s.clone() }.check().is_err());
        assert!(SealedSecret { ct: "C".repeat(endpoint_limits::SEALED + 1), ..s }.check().is_err());
    }

    #[test]
    fn a_profile_never_keeps_a_key_sent_with_it() {
        let p: EndpointProfile = serde_json::from_value(serde_json::json!({
            "id": "gw", "name": "Gateway", "kind": "openai", "base_url": "https://gw.example/v1",
            "key": "sk-live-123", "api_key": "sk-live-123"
        }))
        .unwrap();
        assert!(!serde_json::to_string(&p).unwrap().contains("sk-live"));
    }
}
