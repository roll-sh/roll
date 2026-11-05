//! The registry's index (`registry.json`), parsed with T3 Code's bounds
//! (`packages/provider-acp-registry/src/server/AcpRegistrySupport.ts`: `AcpRegistryAgent`,
//! `AcpRegistryIndexEnvelope`, `resolveAcpRegistryDistribution`, `normalizeRegistryCommandPath`).
//!
//! The envelope must be valid; an agent that breaks any bound is dropped, the rest stay. Versions
//! become directory names, so `.` and `..` never parse; packages must name an exact version; every
//! URL is HTTPS without credentials.

use agent_proto::{RegistryIntegrity, RegistryKind, is_credential_env, is_registry_id};
use serde_json::Value;
use std::collections::BTreeMap;

/// T3's `ACP_REGISTRY_MAX_AGENTS`.
pub const MAX_AGENTS: usize = 512;
/// T3's `MAX_REGISTRY_BYTES`.
pub const MAX_BYTES: usize = 1 << 20;

const MAX_NAME: usize = 160;
const MAX_VERSION: usize = 128;
const MAX_DESCRIPTION: usize = 1024;
const MAX_URL: usize = 2048;
const MAX_ARG: usize = 1024;
const MAX_ARGS: usize = 64;
const MAX_META: usize = 256;
const MAX_PACKAGE: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub version: String,
    pub agents: Vec<Agent>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub icon: Option<String>,
    /// By platform target (`linux-x86_64`).
    pub binary: BTreeMap<String, Binary>,
    pub npx: Option<Package>,
    pub uvx: Option<Package>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Binary {
    pub archive: String,
    /// The executable inside the archive, as path segments ([`command_path`]).
    pub cmd: Vec<String>,
    /// Lowercase hex.
    pub sha256: Option<String>,
    pub launch: Launch,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    /// As the registry gives it (`@scope/name@1.2.3`, `name==1.2.3`).
    pub spec: String,
    pub name: String,
    pub version: String,
    pub launch: Launch,
}

/// How the installed command starts in ACP mode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Launch {
    pub args: Vec<String>,
    /// Configuration such as `AUGMENT_DISABLE_AUTO_UPDATE`; a name that could hold a credential
    /// or an address ([`is_credential_env`]) is dropped: roll never passes one along.
    pub env: Vec<(String, String)>,
}

/// What an install of an agent uses on this machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Choice<'a> {
    Binary { target: &'a str, binary: &'a Binary },
    Npx(&'a Package),
    Uvx(&'a Package),
}

impl Choice<'_> {
    pub fn kind(&self) -> RegistryKind {
        match self {
            Choice::Binary { .. } => RegistryKind::Binary,
            Choice::Npx(_) => RegistryKind::Npx,
            Choice::Uvx(_) => RegistryKind::Uvx,
        }
    }

    pub fn integrity(&self) -> RegistryIntegrity {
        match self {
            Choice::Binary { binary, .. } if binary.sha256.is_some() => RegistryIntegrity::Sha256,
            Choice::Binary { .. } => RegistryIntegrity::None,
            Choice::Npx(_) | Choice::Uvx(_) => RegistryIntegrity::Registry,
        }
    }

    pub fn launch(&self) -> &Launch {
        match self {
            Choice::Binary { binary, .. } => &binary.launch,
            Choice::Npx(p) | Choice::Uvx(p) => &p.launch,
        }
    }
}

impl Agent {
    /// The distributions that suit `platform`, in the order an install tries them (T3's `auto`:
    /// binary, npx, uvx). Windows on ARM falls back to x86-64 binaries, as in T3.
    pub fn choices(&self, platform: Option<&str>) -> Vec<Choice<'_>> {
        let mut all = Vec::new();
        if let Some((target, binary)) = candidates(platform).into_iter().find_map(|t| self.binary.get_key_value(t)) {
            all.push(Choice::Binary { target, binary });
        }
        all.extend(self.npx.as_ref().map(Choice::Npx));
        all.extend(self.uvx.as_ref().map(Choice::Uvx));
        all
    }
}

/// This machine's registry target (`darwin-aarch64`, `linux-x86_64`, `windows-x86_64`), if the
/// registry has a name for it.
pub fn platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        _ => return None,
    })
}

fn candidates(platform: Option<&str>) -> Vec<&str> {
    match platform {
        None => vec![],
        Some("windows-aarch64") => vec!["windows-aarch64", "windows-x86_64"],
        Some(p) => vec![p],
    }
}

/// Parses the index; `Err` only when the envelope itself is wrong. `insecure` is an origin
/// (`http://127.0.0.1:8080`) whose plain-HTTP URLs are accepted too: the daemon's own
/// `[registry] url` when it is HTTP.
pub fn parse(text: &str, insecure: Option<&str>) -> Result<Index, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("the registry is over {MAX_BYTES} bytes"));
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("the registry isn't JSON: {e}"))?;
    let version = v.get("version").and_then(Value::as_str).filter(|s| version_ok(s)).ok_or("the registry has no valid version")?;
    let agents = v.get("agents").and_then(Value::as_array).ok_or("the registry lists no agents")?;
    if agents.len() > MAX_AGENTS {
        return Err(format!("the registry lists more than {MAX_AGENTS} agents"));
    }
    let mut out: Vec<Agent> = Vec::new();
    for a in agents {
        match agent(a, insecure) {
            Some(a) if !out.iter().any(|o| o.id == a.id) => out.push(a),
            Some(a) => tracing::debug!(agent = %a.id, "the ACP registry lists an agent twice; keeping the first"),
            None => tracing::debug!(agent = ?a.get("id"), "dropping an ACP registry entry that breaks its bounds"),
        }
    }
    Ok(Index { version: version.to_string(), agents: out })
}

fn agent(v: &Value, insecure: Option<&str>) -> Option<Agent> {
    let obj = v.as_object()?;
    let id = obj.get("id")?.as_str().filter(|id| is_registry_id(id))?;
    let name = text(obj.get("name")?, MAX_NAME).filter(|n| !n.is_empty())?;
    let version = obj.get("version")?.as_str().map(str::trim).filter(|v| version_ok(v))?;
    let description = match obj.get("description") {
        None => String::new(),
        Some(d) => d.as_str().filter(|d| d.len() <= MAX_DESCRIPTION)?.to_string(),
    };
    if let Some(authors) = obj.get("authors") {
        let authors = authors.as_array()?;
        if authors.len() > 16 || authors.iter().any(|a| a.as_str().is_none_or(|a| a.len() > MAX_META)) {
            return None;
        }
    }
    if obj.get("license").is_some_and(|l| l.as_str().is_none_or(|l| l.len() > 128)) {
        return None;
    }
    let url = |key: &str| -> Option<Option<String>> {
        match obj.get(key) {
            None => Some(None),
            Some(u) => Some(Some(https_url(u.as_str()?, insecure)?)),
        }
    };
    let (website, repository, icon) = (url("website")?, url("repository")?, url("icon")?);
    let dist = obj.get("distribution")?.as_object()?;
    let mut binary = BTreeMap::new();
    if let Some(targets) = dist.get("binary") {
        for (target, b) in targets.as_object()? {
            binary.insert(target.clone(), binary_target(b, insecure)?);
        }
    }
    let npx = match dist.get("npx") {
        None => None,
        Some(p) => Some(package(p, npx_spec)?),
    };
    let uvx = match dist.get("uvx") {
        None => None,
        Some(p) => Some(package(p, uvx_spec)?),
    };
    Some(Agent { id: id.to_string(), name, version: version.to_string(), description, website, repository, icon, binary, npx, uvx })
}

fn binary_target(v: &Value, insecure: Option<&str>) -> Option<Binary> {
    let obj = v.as_object()?;
    let archive = https_url(obj.get("archive")?.as_str()?, insecure)?;
    let cmd = command_path(obj.get("cmd")?.as_str().filter(|c| c.len() <= MAX_ARG)?)?;
    let sha256 = match obj.get("sha256") {
        None => None,
        Some(h) => Some(h.as_str().filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))?.to_ascii_lowercase()),
    };
    Some(Binary { archive, cmd, sha256, launch: launch(obj)? })
}

fn package(v: &Value, spec: fn(&str) -> Option<(String, String)>) -> Option<Package> {
    let obj = v.as_object()?;
    let given = obj.get("package")?.as_str().filter(|p| p.len() <= MAX_PACKAGE)?;
    let (name, version) = spec(given)?;
    Some(Package { spec: given.to_string(), name, version, launch: launch(obj)? })
}

fn launch(obj: &serde_json::Map<String, Value>) -> Option<Launch> {
    let mut out = Launch::default();
    if let Some(args) = obj.get("args") {
        let args = args.as_array().filter(|a| a.len() <= MAX_ARGS)?;
        for a in args {
            out.args.push(a.as_str().filter(|a| a.len() <= MAX_ARG && !a.contains('\0'))?.to_string());
        }
    }
    if let Some(env) = obj.get("env") {
        for (k, v) in env.as_object()? {
            let v = v.as_str().filter(|v| v.len() <= MAX_ARG && !v.contains('\0'))?;
            if k.len() > MAX_META || !env_name(k) {
                return None;
            }
            if is_credential_env(k) {
                tracing::debug!(variable = %k, "an ACP registry entry sets a variable that may hold a credential; leaving it out");
                continue;
            }
            out.env.push((k.clone(), v.to_string()));
        }
    }
    Some(out)
}

fn env_name(k: &str) -> bool {
    k.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn text(v: &Value, max: usize) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| s.len() <= max).map(str::to_string)
}

/// T3's `BoundedVersion`: `^[A-Za-z0-9][A-Za-z0-9._+-]*$`, at most 128 bytes, so never `.` or `..`.
fn version_ok(v: &str) -> bool {
    v.len() <= MAX_VERSION
        && v.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// An absolute HTTPS URL without credentials (T3's `HttpsUrl`), or one on the `insecure` origin.
fn https_url(u: &str, insecure: Option<&str>) -> Option<String> {
    if u.len() > MAX_URL {
        return None;
    }
    let parsed = reqwest::Url::parse(u).ok()?;
    let secure = parsed.scheme() == "https" || (parsed.scheme() == "http" && insecure == Some(parsed.origin().ascii_serialization().as_str()));
    (secure && parsed.username().is_empty() && parsed.password().is_none() && parsed.host_str().is_some()).then(|| u.to_string())
}

/// T3's `normalizeRegistryCommandPath`: `cmd` (or an archive entry) as relative path segments;
/// `None` for an absolute path, a drive, or a `.` or `..` segment.
pub fn command_path(cmd: &str) -> Option<Vec<String>> {
    let normalized = cmd.trim().replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let b = normalized.as_bytes();
    if normalized.starts_with('/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return None;
    }
    let segments: Vec<String> = normalized.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    if segments.is_empty() || segments.iter().any(|s| s == "." || s == ".." || s.contains('\0')) {
        return None;
    }
    Some(segments)
}

/// T3's `EXACT_RUNNER_VERSION`: `v?N.N.N`, then an optional `-prerelease` and `+build`, both of
/// `[0-9A-Za-z.-]`. Gives the version without its `v`.
fn exact_version(v: &str) -> Option<String> {
    let v = v.strip_prefix(['v', 'V']).unwrap_or(v);
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let tail_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let ok = match rest.split_once('+') {
        _ if rest.is_empty() => true,
        Some((pre, build)) => (pre.is_empty() || pre.strip_prefix('-').is_some_and(tail_ok)) && tail_ok(build),
        None => rest.strip_prefix('-').is_some_and(tail_ok),
    };
    ok.then(|| v.to_string())
}

/// `name@1.2.3` or `@scope/name@1.2.3` (T3's `EXACT_NPX_PACKAGE`, case-insensitive).
fn npx_spec(spec: &str) -> Option<(String, String)> {
    let at = spec.rfind('@').filter(|&i| i > 0)?;
    let (name, version) = (&spec[..at], &spec[at + 1..]);
    let name_ok = match name.strip_prefix('@') {
        Some(scoped) => {
            let (scope, pkg) = scoped.split_once('/')?;
            !scope.is_empty() && !pkg.is_empty() && !scope.contains(['/', '@']) && !pkg.contains('@') && !name.contains(char::is_whitespace)
        }
        None => plain_name(name),
    };
    name_ok.then_some(())?;
    Some((name.to_string(), exact_version(version)?))
}

/// `name==1.2.3` or `name@1.2.3` (T3's `EXACT_UVX_PACKAGE`).
fn uvx_spec(spec: &str) -> Option<(String, String)> {
    let (name, version) = spec.rsplit_once("==").or_else(|| spec.rsplit_once('@'))?;
    plain_name(name).then_some(())?;
    Some((name.to_string(), exact_version(version)?))
}

fn plain_name(name: &str) -> bool {
    name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> Value {
        json!({
            "id": "goose",
            "name": "goose",
            "version": "1.57.0",
            "description": "An agent",
            "authors": ["Block"],
            "license": "Apache-2.0",
            "repository": "https://github.com/block/goose",
            "icon": "https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg",
            "distribution": {
                "binary": {
                    "linux-x86_64": {
                        "archive": "https://github.com/block/goose/releases/download/v1.57.0/goose-x86_64-unknown-linux-gnu.tar.bz2",
                        "cmd": "./goose",
                        "args": ["acp"],
                        "sha256": "AB".repeat(32)
                    }
                },
                "npx": { "package": "@block/goose@1.57.0", "args": ["acp"], "env": { "GOOSE_NO_UPDATE": "1", "OPENAI_API_KEY": "x" } }
            }
        })
    }

    fn parse_(text: &str) -> Result<Index, String> {
        parse(text, None)
    }

    fn index(agents: Vec<Value>) -> String {
        json!({ "version": "1.0.0", "agents": agents, "extensions": [] }).to_string()
    }

    #[test]
    fn parses_an_entry() {
        let got = parse_(&index(vec![entry()])).unwrap();
        assert_eq!(got.version, "1.0.0");
        let a = &got.agents[0];
        assert_eq!((a.id.as_str(), a.version.as_str(), a.icon.is_some()), ("goose", "1.57.0", true));
        let b = &a.binary["linux-x86_64"];
        assert_eq!(b.cmd, ["goose"]);
        assert_eq!(b.sha256.as_deref(), Some("ab".repeat(32).as_str()));
        let npx = a.npx.as_ref().unwrap();
        assert_eq!((npx.name.as_str(), npx.version.as_str()), ("@block/goose", "1.57.0"));
        // A credential's variable never comes along.
        assert_eq!(npx.launch.env, [("GOOSE_NO_UPDATE".to_string(), "1".to_string())]);
    }

    #[test]
    fn drops_entries_that_break_a_bound_and_keeps_the_rest() {
        let with = |f: &dyn Fn(&mut Value)| {
            let mut e = entry();
            f(&mut e);
            e
        };
        let bad = vec![
            with(&|e| e["id"] = json!("Goose")),
            with(&|e| e["id"] = json!("a".repeat(129))),
            with(&|e| e["version"] = json!("..")),
            with(&|e| e["version"] = json!(".")),
            with(&|e| e["version"] = json!("1/2")),
            with(&|e| e["name"] = json!("")),
            with(&|e| e["name"] = json!("n".repeat(161))),
            with(&|e| e["description"] = json!("d".repeat(1025))),
            with(&|e| e["icon"] = json!("http://cdn.agentclientprotocol.com/x.svg")),
            with(&|e| e["website"] = json!("https://user:pw@example.com")),
            with(&|e| e["distribution"]["binary"]["linux-x86_64"]["archive"] = json!("http://example.com/a.tar.gz")),
            with(&|e| e["distribution"]["binary"]["linux-x86_64"]["cmd"] = json!("../goose")),
            with(&|e| e["distribution"]["binary"]["linux-x86_64"]["cmd"] = json!("/usr/bin/goose")),
            with(&|e| e["distribution"]["binary"]["linux-x86_64"]["sha256"] = json!("abc")),
            with(&|e| e["distribution"]["npx"]["package"] = json!("@block/goose")),
            with(&|e| e["distribution"]["npx"]["package"] = json!("@block/goose@latest")),
            with(&|e| e["distribution"]["npx"]["package"] = json!("@block/goose@^1.2.0")),
            with(&|e| e["distribution"]["npx"]["args"] = json!(vec!["x"; 65])),
            with(&|e| e["distribution"]["npx"]["env"] = json!({ "1BAD": "x" })),
            with(&|e| e["distribution"] = json!({ "uvx": { "package": "fast-agent-acp>=0.10" } })),
            json!("not an object"),
        ];
        // Plain HTTP only on the daemon's own registry's origin.
        let local = with(&|e| e["distribution"]["binary"]["linux-x86_64"]["archive"] = json!("http://127.0.0.1:9/a.tar.gz"));
        assert!(agent(&local, None).is_none());
        assert!(agent(&local, Some("http://127.0.0.1:9")).is_some());
        assert!(agent(&local, Some("http://127.0.0.1:10")).is_none());
        let n = bad.len();
        let mut all = bad;
        all.push(entry());
        all.push(with(&|e| e["name"] = json!("a second goose")));
        let got = parse_(&index(all)).unwrap();
        assert_eq!(got.agents.len(), 1, "{n} bad entries and a duplicate dropped");
        assert_eq!(got.agents[0].name, "goose");
    }

    #[test]
    fn envelope_bounds() {
        assert!(parse_("{").is_err());
        assert!(parse_(&json!({ "agents": [] }).to_string()).is_err());
        assert!(parse_(&json!({ "version": "1", "agents": vec![json!({}); MAX_AGENTS + 1] }).to_string()).is_err());
        assert!(parse_(&" ".repeat(MAX_BYTES + 1)).is_err());
        assert!(parse_(&json!({ "version": "1", "agents": [] }).to_string()).unwrap().agents.is_empty());
    }

    #[test]
    fn exact_package_versions() {
        assert_eq!(npx_spec("cline@3.0.74"), Some(("cline".into(), "3.0.74".into())));
        assert_eq!(npx_spec("@google/gemini-cli@v0.67.0-preview.1+b2"), Some(("@google/gemini-cli".into(), "0.67.0-preview.1+b2".into())));
        assert_eq!(uvx_spec("fast-agent-acp==0.10.1"), Some(("fast-agent-acp".into(), "0.10.1".into())));
        assert_eq!(uvx_spec("minion-code@0.1.44"), Some(("minion-code".into(), "0.1.44".into())));
        for bad in ["cline", "cline@3", "cline@3.0", "cline@latest", "@x@1.0.0", "@/x@1.0.0", "a b@1.0.0", "cline@1.0.0-", "cline@1.0.0+"] {
            assert_eq!(npx_spec(bad), None, "{bad}");
        }
        assert_eq!(uvx_spec("x>=1.0.0"), None);
    }

    #[test]
    fn commands_are_relative_paths_inside_the_archive() {
        assert_eq!(command_path("./bin/goose"), Some(vec!["bin".into(), "goose".into()]));
        assert_eq!(command_path("dist\\agent.exe"), Some(vec!["dist".into(), "agent.exe".into()]));
        assert_eq!(command_path("a//b"), Some(vec!["a".into(), "b".into()]));
        for bad in ["", "./", "/bin/sh", "C:\\x.exe", "../x", "a/../../x", "a/./b"] {
            assert_eq!(command_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn choices_go_binary_then_npx_then_uvx() {
        let mut a = agent(&entry(), None).unwrap();
        a.uvx = Some(Package { spec: "goose==1.57.0".into(), name: "goose".into(), version: "1.57.0".into(), launch: Launch::default() });
        let kinds = |p: Option<&str>| a.choices(p).iter().map(Choice::kind).collect::<Vec<_>>();
        assert_eq!(kinds(Some("linux-x86_64")), [RegistryKind::Binary, RegistryKind::Npx, RegistryKind::Uvx]);
        assert_eq!(kinds(Some("darwin-aarch64")), [RegistryKind::Npx, RegistryKind::Uvx]);
        assert_eq!(kinds(None), [RegistryKind::Npx, RegistryKind::Uvx]);
        assert_eq!(a.choices(Some("linux-x86_64"))[0].integrity(), RegistryIntegrity::Sha256);
        assert_eq!(a.choices(Some("linux-x86_64"))[1].integrity(), RegistryIntegrity::Registry);
        // Windows on ARM runs x86-64 binaries.
        let win = Binary { archive: "https://x/a.zip".into(), cmd: vec!["a.exe".into()], sha256: None, launch: Launch::default() };
        a.binary.insert("windows-x86_64".into(), win);
        assert!(matches!(a.choices(Some("windows-aarch64"))[0], Choice::Binary { target: "windows-x86_64", .. }));
        assert!(platform().is_some());
    }
}
