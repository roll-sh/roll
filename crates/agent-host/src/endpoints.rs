//! Provider endpoints on this machine (docs/code/agents.md, "Endpoints"): base URLs and keys the
//! owner set from roll, laid over the providers' own config.
//!
//! - **The seal key.** The daemon makes an X25519 key pair once and keeps it in `agent.db`; the
//!   public half rides in `hello` (`MachineInfo::seal_key`). A key set from roll arrives sealed to
//!   it ([`SealedSecret`], `roll-seal-v1`, the same box `apps/dashboard/src/lib/agents-seal.ts`
//!   makes), so neither the relay nor anything else between the browser and this daemon can read
//!   it.
//! - **Stored here only.** An opened key is kept in `agent.db` (mode 0600) and handed only to
//!   the processes of the providers that use the endpoint ([`EndpointUse`]): never to terminals,
//!   records, logs or `hello`, which says only whether a key is set ([`KeyState`]).
//! - **1Password.** A key may be a reference (`op://…`, T3 Code's secret providers,
//!   `packages/secret-provider-onepassword` there): read with the `op` CLI on this machine, kept in
//!   memory only.
//! - **Models.** The endpoint's `/models`, bounded (10 s, 2 MiB, 500 models, no redirects), offered
//!   by every provider that uses it. The daemon calls the endpoint, so a private or tailnet address
//!   is fine; roll's relay never calls it.

use crate::provider::ProviderConfig;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use agent_proto::{EndpointKind, EndpointModel, EndpointSpec, KeyState, ProviderId, SEAL_VERSION, SealedSecret, endpoint_limits};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use curve25519_dalek::montgomery::MontgomeryPoint;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::Duration;
use zeroize::Zeroizing;

/// The HKDF info of a `roll-seal-v1` box.
const SEAL_INFO: &[u8] = b"roll-seal-v1";
/// How long a models list may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of a models list is read.
const LIST_BYTES: usize = 2 << 20;
/// How long `op read` may take (it may wait on the user's unlock).
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// A key in memory: never printed, wiped when dropped.
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

/// The daemon's X25519 key pair.
pub struct SealKey {
    secret: Zeroizing<[u8; 32]>,
    public: [u8; 32],
}

impl SealKey {
    pub fn from_secret(secret: [u8; 32]) -> SealKey {
        let public = MontgomeryPoint::mul_base_clamped(secret).to_bytes();
        SealKey { secret: Zeroizing::new(secret), public }
    }

    pub fn generate() -> SealKey {
        SealKey::from_secret(rand::random())
    }

    /// The store's key pair, made and stored on first use.
    pub fn load_or_create(store: &crate::store::Store) -> anyhow::Result<SealKey> {
        if let Some(hex) = store.setting::<Zeroizing<String>>(SEAL_SETTING)? {
            let bytes = Zeroizing::new(hex::decode(hex.as_bytes())?);
            let secret: [u8; 32] = bytes.as_slice().try_into().map_err(|_| anyhow::anyhow!("the stored seal key isn't 32 bytes"))?;
            return Ok(SealKey::from_secret(secret));
        }
        let key = SealKey::generate();
        store.put_setting(SEAL_SETTING, &Zeroizing::new(hex::encode(*key.secret)))?;
        Ok(key)
    }

    /// The public half, as `hello` carries it.
    pub fn public(&self) -> String {
        B64.encode(self.public)
    }

    /// Opens a key sealed to this daemon for `endpoint`, or says why it doesn't open (never with
    /// any of its bytes).
    pub fn open(&self, endpoint: &str, sealed: &SealedSecret) -> Result<Secret, String> {
        self.open_with(&SealedSecret::context(endpoint), sealed)
    }

    /// [`SealKey::open`] for a box sealed with any associated data: `context` names what the key
    /// is for (an endpoint's `roll-endpoint:<id>`, a gateway upstream's
    /// [`agent_proto::gateway::upstream_key_context`]), so a box made for one never opens as another.
    pub fn open_with(&self, context: &str, sealed: &SealedSecret) -> Result<Secret, String> {
        sealed.check()?;
        let bad = || "the key wasn't sealed to this machine (reload the page and set it again)".to_string();
        let epk: [u8; 32] = B64.decode(&sealed.epk).ok().and_then(|b| b.try_into().ok()).ok_or_else(bad)?;
        let nonce: [u8; 12] = B64.decode(&sealed.nonce).ok().and_then(|b| b.try_into().ok()).ok_or_else(bad)?;
        let ct = B64.decode(&sealed.ct).map_err(|_| bad())?;
        let shared = Zeroizing::new(MontgomeryPoint(epk).mul_clamped(*self.secret).to_bytes());
        // A low-order point gives an all-zero secret anyone could compute.
        if shared.iter().all(|b| *b == 0) {
            return Err(bad());
        }
        let cipher = box_cipher(&shared, &epk, &self.public);
        let plain = Zeroizing::new(cipher.decrypt(&nonce.into(), Payload { msg: &ct, aad: context.as_bytes() }).map_err(|_| bad())?);
        let key = String::from_utf8(plain.to_vec()).map_err(|_| "the key isn't text".to_string())?;
        check_key(&key)?;
        Ok(Secret::new(key))
    }
}

const SEAL_SETTING: &str = "seal_key";

fn box_cipher(shared: &[u8; 32], epk: &[u8; 32], public: &[u8; 32]) -> aes_gcm::Aes256Gcm {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(epk);
    salt[32..].copy_from_slice(public);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared).expand(SEAL_INFO, okm.as_mut()).expect("32 bytes is a valid HKDF length");
    aes_gcm::Aes256Gcm::new_from_slice(okm.as_ref()).expect("a 32-byte key")
}

/// Seals `key` to the daemon whose public key is `public` (base64url), for `endpoint`: what the
/// dashboard does in the browser (`agents-seal.ts`), for tests and tools.
pub fn seal(public: &str, endpoint: &str, key: &str) -> anyhow::Result<SealedSecret> {
    seal_with(public, &SealedSecret::context(endpoint), key)
}

/// [`seal`] with any associated data (see [`SealKey::open_with`]).
pub fn seal_with(public: &str, context: &str, key: &str) -> anyhow::Result<SealedSecret> {
    let public: [u8; 32] = B64.decode(public)?.try_into().map_err(|_| anyhow::anyhow!("a seal key is 32 bytes"))?;
    let esk = Zeroizing::new(rand::random::<[u8; 32]>());
    let epk = MontgomeryPoint::mul_base_clamped(*esk).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(public).mul_clamped(*esk).to_bytes());
    let nonce: [u8; 12] = rand::random();
    let ct = box_cipher(&shared, &epk, &public)
        .encrypt(&nonce.into(), Payload { msg: key.as_bytes(), aad: context.as_bytes() })
        .map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok(SealedSecret { v: SEAL_VERSION, epk: B64.encode(epk), nonce: B64.encode(nonce), ct: B64.encode(ct) })
}

/// A key goes into an environment variable: one line of text.
fn check_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > endpoint_limits::KEY || key.chars().any(|c| c.is_control()) {
        return Err(format!("a key is one line of 1 to {} bytes", endpoint_limits::KEY));
    }
    Ok(())
}

/// An endpoint as the daemon keeps it in `agent.db` (the `settings` table's `endpoints`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Stored {
    pub spec: EndpointSpec,
    /// The opened sealed key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Zeroizing<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<EndpointModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The providers it took from the machine's other endpoints when it was set (provider, the
    /// endpoint it was on): removing it gives each back, if that endpoint is still there (joining
    /// the gateway and leaving it again leaves a provider on its CLIProxyAPI, not on nothing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub taken: Vec<(ProviderId, String)>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored").field("spec", &self.spec).field("key", &self.key.as_ref().map(|_| "…")).finish_non_exhaustive()
    }
}

/// The store key of the endpoints.
pub const SETTING: &str = "endpoints";

/// An endpoint while the daemon runs: what's stored, and the key a reference resolved to.
#[derive(Clone, Debug)]
pub struct Live {
    pub stored: Stored,
    /// `key_ref`'s value, read from 1Password at start and when the endpoint is set.
    pub resolved: Option<Secret>,
    /// Why the reference couldn't be read.
    pub ref_error: Option<String>,
}

impl Live {
    pub fn new(stored: Stored) -> Live {
        Live { stored, resolved: None, ref_error: None }
    }

    /// The key the providers get: the reference's, else the sealed one.
    pub fn key(&self) -> Option<Secret> {
        if self.stored.spec.key_ref.is_some() {
            return self.resolved.clone();
        }
        self.stored.key.as_ref().map(|k| Secret::new(k.to_string()))
    }

    pub fn key_state(&self) -> KeyState {
        match (&self.stored.spec.key_ref, &self.stored.key) {
            (Some(_), _) => KeyState::Reference,
            (None, Some(_)) => KeyState::Sealed,
            (None, None) => KeyState::None,
        }
    }

    pub fn info(&self) -> agent_proto::EndpointInfo {
        agent_proto::EndpointInfo {
            spec: self.stored.spec.clone(),
            key: self.key_state(),
            models: self.stored.models.clone(),
            checked_at: self.stored.checked_at,
            error: self.ref_error.clone().or_else(|| self.stored.error.clone()),
        }
    }
}

/// What using an endpoint adds to a provider's process: variables over its own, and arguments
/// before the adapter's (Codex's `-c` overrides).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointUse {
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
}

impl EndpointUse {
    /// `cfg` with this laid over it: variables of the same name replaced, arguments first.
    pub fn apply(self, mut cfg: ProviderConfig) -> ProviderConfig {
        cfg.env.retain(|(k, _)| !self.env.iter().any(|(n, _)| n == k));
        cfg.env.extend(self.env);
        let mut args = self.args;
        args.extend(cfg.args);
        cfg.args = args;
        cfg
    }
}

/// How any provider uses an endpoint unless its adapter says otherwise
/// ([`crate::provider::Provider::endpoint_use`]): the variables each API's own SDKs and CLIs
/// read. CLIProxyAPI serves the Anthropic, OpenAI and Gemini APIs at once, so it sets all three.
pub fn generic_use(spec: &EndpointSpec, key: Option<&str>) -> EndpointUse {
    let mut env = Vec::new();
    let mut put = |name: &str, value: Option<&str>| {
        if let Some(v) = value {
            env.push((name.to_string(), v.to_string()));
        }
    };
    let base = spec.base_url.as_str();
    match spec.kind {
        EndpointKind::Anthropic => anthropic_env(&mut put, base, key),
        EndpointKind::Openai => {
            put("OPENAI_BASE_URL", Some(base));
            put("OPENAI_API_KEY", key);
        }
        EndpointKind::Cliproxy => {
            anthropic_env(&mut put, base, key);
            put("OPENAI_BASE_URL", Some(&format!("{base}/v1")));
            put("OPENAI_API_KEY", key);
            put("GOOGLE_GEMINI_BASE_URL", Some(base));
            put("GEMINI_API_KEY", key);
        }
        // roll's gateway: CLIProxyAPI's Anthropic and OpenAI routes, no Gemini.
        EndpointKind::Gateway => {
            anthropic_env(&mut put, base, key);
            put("OPENAI_BASE_URL", Some(&format!("{base}/v1")));
            put("OPENAI_API_KEY", key);
        }
        EndpointKind::Custom => {
            if let Some(name) = &spec.url_env {
                put(name, Some(base));
            }
            if let Some(name) = &spec.key_env {
                put(name, key);
            }
        }
    }
    EndpointUse { env, args: Vec::new() }
}

/// Claude Code's gateway variables: the base URL, and the key as a bearer token (what CLIProxyAPI
/// and most gateways take; `ANTHROPIC_API_KEY` would make Claude Code ask before using it).
pub fn anthropic_env(put: &mut impl FnMut(&str, Option<&str>), base: &str, key: Option<&str>) {
    put("ANTHROPIC_BASE_URL", Some(base));
    put("ANTHROPIC_AUTH_TOKEN", key);
}

/// [`anthropic_env`] alone.
pub fn anthropic_use(base: &str, key: Option<&str>) -> EndpointUse {
    let mut env = Vec::new();
    anthropic_env(&mut |k: &str, v: Option<&str>| env.extend(v.map(|v| (k.to_string(), v.to_string()))), base, key);
    EndpointUse { env, args: Vec::new() }
}

/// The variable Codex reads an endpoint's key from (`env_key`).
pub const CODEX_KEY_ENV: &str = "ROLL_ENDPOINT_KEY";

/// Codex's model provider for an endpoint: `-c` overrides before `app-server`, the key in
/// [`CODEX_KEY_ENV`]. CLIProxyAPI's OpenAI routes are under `/v1`; an OpenAI-compatible base URL
/// already ends there.
pub fn codex_use(spec: &EndpointSpec, key: Option<&str>) -> Result<EndpointUse, String> {
    let base = match spec.kind {
        EndpointKind::Openai => spec.base_url.clone(),
        EndpointKind::Cliproxy | EndpointKind::Gateway => format!("{}/v1", spec.base_url),
        EndpointKind::Anthropic => return Err("Codex speaks the OpenAI API: use an OpenAI-compatible endpoint or CLIProxyAPI".into()),
        EndpointKind::Custom => return Ok(generic_use(spec, key)),
    };
    let id = format!("roll_{}", spec.id.replace('-', "_"));
    // A TOML basic string: JSON's escapes are TOML's.
    let toml = |v: &str| serde_json::to_string(v).unwrap_or_default();
    let mut set = vec![
        format!("model_provider={}", toml(&id)),
        format!("model_providers.{id}.name={}", toml(&spec.name)),
        format!("model_providers.{id}.base_url={}", toml(&base)),
        format!("model_providers.{id}.wire_api=\"responses\""),
        format!("model_providers.{id}.requires_openai_auth=false"),
    ];
    if spec.kind == EndpointKind::Gateway {
        // Each session's process names its thread and project (`gateway::attribution_env`).
        use crate::gateway::{PROJECT_ENV, PROJECT_HEADER, THREAD_ENV, THREAD_HEADER};
        set.push(format!("model_providers.{id}.env_http_headers={{{}={},{}={}}}", toml(THREAD_HEADER), toml(THREAD_ENV), toml(PROJECT_HEADER), toml(PROJECT_ENV)));
    }
    let mut env = Vec::new();
    if let Some(k) = key {
        set.push(format!("model_providers.{id}.env_key={}", toml(CODEX_KEY_ENV)));
        env.push((CODEX_KEY_ENV.to_string(), k.to_string()));
    }
    Ok(EndpointUse { env, args: set.into_iter().flat_map(|s| ["-c".to_string(), s]).collect() })
}

/// Reads a 1Password reference with the `op` CLI on this machine. The value never appears in
/// the error; `op`'s own message (stderr) does, shortened.
pub async fn read_reference(reference: &str) -> Result<Secret, String> {
    let mut cmd = tokio::process::Command::new("op");
    cmd.args(["read", "--no-newline", reference]).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(OP_TIMEOUT, cmd.output()).await {
        Err(_) => return Err("1Password: `op read` didn't answer in 30 s (is 1Password unlocked?)".into()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err("1Password: the `op` CLI isn't installed on this machine".into()),
        Ok(Err(e)) => return Err(format!("1Password: couldn't run `op`: {e}")),
        Ok(Ok(out)) => out,
    };
    let value = Zeroizing::new(out.stdout);
    if !out.status.success() {
        let why: String = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("").chars().take(200).collect();
        return Err(format!("1Password: {}", if why.trim().is_empty() { "`op read` failed".to_string() } else { why.trim().to_string() }));
    }
    let key = String::from_utf8(value.to_vec()).map_err(|_| "1Password: the field isn't text".to_string())?;
    let key = key.trim_end_matches(['\r', '\n']).to_string();
    check_key(&key).map_err(|e| format!("1Password: {e}"))?;
    Ok(Secret::new(key))
}

/// Where an endpoint lists its models, if it does.
pub fn models_url(spec: &EndpointSpec) -> Option<String> {
    let base = &spec.base_url;
    match spec.kind {
        EndpointKind::Anthropic => Some(format!("{base}/v1/models?limit=1000")),
        EndpointKind::Cliproxy | EndpointKind::Gateway => Some(format!("{base}/v1/models")),
        EndpointKind::Openai => Some(format!("{base}/models")),
        EndpointKind::Custom => None,
    }
}

/// Asks the endpoint for its models: `{"data": [{"id", "display_name"?}]}` (OpenAI's and
/// Anthropic's shape, and CLIProxyAPI's), at most [`endpoint_limits::MODELS`] of them.
pub async fn list_models(spec: &EndpointSpec, key: Option<&str>) -> Result<Vec<EndpointModel>, String> {
    let Some(url) = models_url(spec) else {
        return Err("a custom endpoint doesn't list models: add them as custom models".into());
    };
    let client = reqwest::Client::builder()
        .timeout(LIST_TIMEOUT)
        .connect_timeout(LIST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("roll-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("couldn't make an HTTP client: {e}"))?;
    let mut req = client.get(&url).header("accept", "application/json");
    if let Some(k) = key {
        req = req.bearer_auth(k);
        if matches!(spec.kind, EndpointKind::Anthropic | EndpointKind::Cliproxy | EndpointKind::Gateway) {
            req = req.header("x-api-key", k);
        }
    }
    if spec.kind == EndpointKind::Anthropic {
        req = req.header("anthropic-version", "2023-06-01");
    }
    let mut resp = req.send().await.map_err(|e| unreachable(&e))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => ": check the key",
            404 => ": check the base URL",
            _ => "",
        };
        return Err(format!("the endpoint answered {status}{hint}"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| unreachable(&e))? {
        if body.len() + chunk.len() > LIST_BYTES {
            return Err("the endpoint's model list is over 2 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    parse_models(&body)
}

/// Never echoes the URL's query or anything the server said beyond the error's kind.
fn unreachable(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "the endpoint didn't answer in 10 s".into()
    } else if e.is_connect() {
        "couldn't connect to the endpoint from this machine".into()
    } else if e.is_redirect() || e.status().is_some_and(|s| s.is_redirection()) {
        "the endpoint redirected: use the URL it redirects to".into()
    } else {
        "the endpoint's answer couldn't be read".into()
    }
}

pub fn parse_models(body: &[u8]) -> Result<Vec<EndpointModel>, String> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        data: Vec<serde_json::Value>,
        #[serde(default)]
        models: Vec<serde_json::Value>,
    }
    let list: List = serde_json::from_slice(body).map_err(|_| "the endpoint's /models isn't a model list".to_string())?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in list.data.iter().chain(&list.models) {
        let field = |k: &str| m.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
        // Gemini's list names models `models/<id>`.
        let Some(id) = field("id").or_else(|| field("name").map(|n| n.strip_prefix("models/").unwrap_or(n))) else { continue };
        if id.len() > endpoint_limits::MODEL_ID || id.chars().any(char::is_control) || !seen.insert(id.to_string()) {
            continue;
        }
        let name = field("display_name").or_else(|| field("displayName")).filter(|n| *n != id && n.len() <= endpoint_limits::MODEL_ID).map(str::to_string);
        out.push(EndpointModel { id: id.to_string(), name });
        if out.len() == endpoint_limits::MODELS {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_key_opens_only_on_its_daemon_and_for_its_endpoint() {
        let daemon = SealKey::generate();
        let sealed = seal(&daemon.public(), "cliproxy", "sk-test-123").unwrap();
        assert!(sealed.check().is_ok());
        assert_eq!(daemon.open("cliproxy", &sealed).unwrap().expose(), "sk-test-123");
        assert!(daemon.open("other", &sealed).is_err(), "bound to the endpoint");
        assert!(SealKey::generate().open("cliproxy", &sealed).is_err(), "bound to the daemon");
        let mut tampered = sealed.clone();
        tampered.ct.replace_range(0..1, if tampered.ct.starts_with('A') { "B" } else { "A" });
        assert!(daemon.open("cliproxy", &tampered).is_err());
        // A low-order point (here, zero) is refused.
        let zero = SealedSecret { epk: B64.encode([0u8; 32]), ..sealed };
        assert!(daemon.open("cliproxy", &zero).is_err());
        assert_eq!(format!("{:?}", Secret::new("sk-test-123".into())), "Secret(…)");
    }

    /// A box the dashboard made with WebCrypto (`agents-seal.ts`; `agents-seal.test.ts` opens it too).
    #[test]
    fn opens_the_dashboards_box() {
        let d = [48, 209, 102, 111, 243, 150, 16, 2, 48, 117, 176, 150, 131, 15, 197, 238, 20, 27, 151, 104, 214, 63, 157, 222, 129, 143, 37, 102, 10, 254, 206, 122];
        let daemon = SealKey::from_secret(d);
        assert_eq!(daemon.public(), "KMIZzg7UJuV-Nxs3gUarEXCHzKlAkKSSSCaXrdLz4zg");
        let sealed = SealedSecret {
            v: 1,
            epk: "BZK0rGd_phBWP5T-hdas0-ymnZ_QtYeeaWy3uBGP2Gc".into(),
            nonce: "AAECAwQFBgcICQoL".into(),
            ct: "dsu-tM5kWenDNMkLzXdVKQ5AytEyazIrW93S".into(),
        };
        assert_eq!(daemon.open("cliproxy", &sealed).unwrap().expose(), "sk-test-123");
        // Sealed by the dashboard's pure-JS box (agents-seal.test.ts pins it, from a fixed key).
        let sealed = SealedSecret {
            v: 1,
            epk: "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw".into(),
            nonce: "AAECAwQFBgcICQoL".into(),
            ct: "w3RVbnNeH9p-sAeRCazhVaXWz4la-gDXQZOD".into(),
        };
        assert_eq!(daemon.open("cliproxy", &sealed).unwrap().expose(), "sk-test-123");
    }

    #[test]
    fn keys_live_only_in_agent_db() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("agent.db")).unwrap();
        let a = SealKey::load_or_create(&store).unwrap();
        let b = SealKey::load_or_create(&store).unwrap();
        assert_eq!(a.public(), b.public(), "made once");
    }

    fn spec(kind: EndpointKind) -> EndpointSpec {
        EndpointSpec {
            id: "e".into(),
            name: "E".into(),
            kind,
            base_url: "http://127.0.0.1:8317".into(),
            providers: vec![],
            url_env: None,
            key_env: None,
            key_ref: None,
            fallback: false,
        }
    }

    #[test]
    fn each_kind_sets_its_apis_variables() {
        let env = |kind, key| generic_use(&spec(kind), key).env;
        assert_eq!(
            env(EndpointKind::Anthropic, Some("k")),
            vec![("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:8317".into()), ("ANTHROPIC_AUTH_TOKEN".into(), "k".into())]
        );
        assert_eq!(env(EndpointKind::Openai, None), vec![("OPENAI_BASE_URL".to_string(), "http://127.0.0.1:8317".to_string())]);
        let names: Vec<String> = env(EndpointKind::Cliproxy, Some("k")).into_iter().map(|(k, _)| k).collect();
        assert_eq!(names, ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "OPENAI_BASE_URL", "OPENAI_API_KEY", "GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY"]);
        let custom = EndpointSpec { key_env: Some("GEMINI_API_KEY".into()), ..spec(EndpointKind::Custom) };
        assert_eq!(generic_use(&custom, Some("k")).env, vec![("GEMINI_API_KEY".to_string(), "k".to_string())]);
    }

    #[test]
    fn codex_gets_a_model_provider_and_the_key_only_in_its_environment() {
        let used = codex_use(&EndpointSpec { id: "cli-proxy".into(), ..spec(EndpointKind::Cliproxy) }, Some("sk-x")).unwrap();
        assert_eq!(
            used.args,
            [
                "-c", "model_provider=\"roll_cli_proxy\"",
                "-c", "model_providers.roll_cli_proxy.name=\"E\"",
                "-c", "model_providers.roll_cli_proxy.base_url=\"http://127.0.0.1:8317/v1\"",
                "-c", "model_providers.roll_cli_proxy.wire_api=\"responses\"",
                "-c", "model_providers.roll_cli_proxy.requires_openai_auth=false",
                "-c", "model_providers.roll_cli_proxy.env_key=\"ROLL_ENDPOINT_KEY\"",
            ]
        );
        assert!(!used.args.iter().any(|a| a.contains("sk-x")));
        assert_eq!(used.env, vec![("ROLL_ENDPOINT_KEY".to_string(), "sk-x".to_string())]);
        let openai = codex_use(&EndpointSpec { base_url: "https://gw/v1".into(), ..spec(EndpointKind::Openai) }, None).unwrap();
        assert!(openai.args.contains(&"model_providers.roll_e.base_url=\"https://gw/v1\"".to_string()));
        assert!(openai.env.is_empty());
        assert!(codex_use(&spec(EndpointKind::Anthropic), None).is_err());

        // A roll gateway: Codex names its thread through `env_http_headers`, read from the
        // session's variables, and the client key is never an argument.
        let gw = EndpointSpec { id: agent_proto::gateway_limits::ENDPOINT_ID.into(), base_url: "http://gw:8790".into(), ..spec(EndpointKind::Gateway) };
        let used = codex_use(&gw, Some("rgk_x")).unwrap();
        assert!(used.args.contains(&r#"model_providers.roll_roll_gateway.env_http_headers={"x-roll-thread"="ROLL_GATEWAY_THREAD","x-roll-project"="ROLL_GATEWAY_PROJECT"}"#.to_string()), "{:?}", used.args);
        assert!(used.args.contains(&r#"model_providers.roll_roll_gateway.base_url="http://gw:8790/v1""#.to_string()));
        assert!(!used.args.iter().any(|a| a.contains("rgk_x")));
    }

    #[test]
    fn an_endpoint_lays_over_the_providers_config() {
        let cfg = ProviderConfig {
            binary: None,
            args: vec!["--own".into()],
            env: vec![("ANTHROPIC_BASE_URL".into(), "https://old".into()), ("KEEP".into(), "1".into())],
            ..Default::default()
        };
        let used = EndpointUse { env: vec![("ANTHROPIC_BASE_URL".into(), "http://new".into())], args: vec!["-c".into(), "x=1".into()] }.apply(cfg);
        assert_eq!(used.args, ["-c", "x=1", "--own"]);
        assert_eq!(used.env, vec![("KEEP".to_string(), "1".to_string()), ("ANTHROPIC_BASE_URL".to_string(), "http://new".to_string())]);
    }

    #[test]
    fn model_lists_are_bounded_and_deduplicated() {
        let body = br#"{"object":"list","data":[{"id":"gpt-5.5","owned_by":"openai"},{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5"},{"id":"gpt-5.5"},{"nope":1}]}"#;
        let got = parse_models(body).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].name.as_deref(), Some("Claude Opus 5.5"));
        let gemini = br#"{"models":[{"name":"models/gemini-2.5-pro","displayName":"Gemini 2.5 Pro"}]}"#;
        assert_eq!(parse_models(gemini).unwrap()[0].id, "gemini-2.5-pro");
        let many = serde_json::json!({ "data": (0..900).map(|i| serde_json::json!({ "id": format!("m{i}") })).collect::<Vec<_>>() });
        assert_eq!(parse_models(many.to_string().as_bytes()).unwrap().len(), endpoint_limits::MODELS);
        assert!(parse_models(b"<html>").is_err());
    }
}
