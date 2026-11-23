//! `roll mcp install`: roll's MCP server added to the agents installed here, as Stripe's
//! `stripe agent setup` does. Each client connects with OAuth (it asks you to sign in), never a
//! token, so nothing secret is written. A file is merged, never replaced: an entry named `roll`
//! that's there already is left alone (and said so), and anything else in the file stays.
//! Claude Code is told through its own `claude mcp add`; Cursor, VS Code (1.146's
//! `~/.copilot/mcp-config.json`) and Codex (`~/.codex/config.toml`) by their files.

use crate::access::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What installing in one client did (or, dry, would do).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Added(String),
    Already(String),
    Skipped(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Added(s) => write!(f, "added: {s}"),
            Outcome::Already(s) => write!(f, "already there: {s}"),
            Outcome::Skipped(s) => write!(f, "skipped: {s}"),
        }
    }
}

/// Where this machine's clients keep their config, and how to reach Claude Code's CLI.
pub struct Here<'a> {
    pub home: PathBuf,
    /// Whether a program is on PATH.
    pub on_path: &'a dyn Fn(&str) -> bool,
    /// Run `claude` with these arguments; whether it succeeded.
    pub claude: &'a dyn Fn(&[&str]) -> bool,
}

/// The clients found here (each one's own sign: its CLI or its config directory).
pub fn found(here: &Here<'_>) -> Vec<Client> {
    let mut out = Vec::new();
    if (here.on_path)("claude") {
        out.push(Client::ClaudeCode);
    }
    if here.home.join(".cursor").is_dir() {
        out.push(Client::Cursor);
    }
    if here.home.join(".copilot").is_dir() || (here.on_path)("code") {
        out.push(Client::Vscode);
    }
    if here.home.join(".codex").is_dir() || (here.on_path)("codex") {
        out.push(Client::Codex);
    }
    out
}

/// Add roll's server at `url` to `client` (`dry`: only say what would change).
pub fn install(here: &Here<'_>, client: Client, url: &str, dry: bool) -> anyhow::Result<Outcome> {
    match client {
        Client::ClaudeCode => {
            if !(here.on_path)("claude") {
                return Ok(Outcome::Skipped("Claude Code: no claude on PATH".into()));
            }
            if (here.claude)(&["mcp", "get", "roll"]) {
                return Ok(Outcome::Already("Claude Code has a server named roll (claude mcp remove roll to replace it)".into()));
            }
            if !dry && !(here.claude)(&["mcp", "add", "--transport", "http", "--scope", "user", "roll", url]) {
                anyhow::bail!("claude mcp add failed");
            }
            Ok(Outcome::Added(format!("Claude Code (claude mcp add --scope user roll {url}; it asks you to sign in)")))
        }
        Client::Cursor => json_file(&here.home.join(".cursor/mcp.json"), json!({ "url": url }), "Cursor", dry),
        Client::Vscode => json_file(&here.home.join(".copilot/mcp-config.json"), json!({ "type": "http", "url": url }), "VS Code", dry),
        Client::Codex => codex(&here.home.join(".codex/config.toml"), url, dry),
        Client::ClaudeDesktop => Ok(Outcome::Skipped(format!("Claude Desktop connects in Settings → Connectors → Add custom connector: {url}"))),
        Client::Json => Ok(Outcome::Skipped("json is a config to print (roll mcp config json), not a client".into())),
    }
}

/// Merge `roll` into a `{"mcpServers": {…}}` file, written whole only after it's read and changed.
fn json_file(path: &Path, entry: Value, name: &str, dry: bool) -> anyhow::Result<Outcome> {
    let mut doc: Value = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => json!({}),
        Ok(text) => serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{} isn't JSON ({e}): left as it is", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let Some(top) = doc.as_object_mut() else { anyhow::bail!("{} isn't a JSON object: left as it is", path.display()) };
    let servers = top.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else { anyhow::bail!("{}'s mcpServers isn't an object: left as it is", path.display()) };
    if servers.contains_key("roll") {
        return Ok(Outcome::Already(format!("{name} ({} has a server named roll)", path.display())));
    }
    servers.insert("roll".into(), entry);
    if !dry {
        write(path, &format!("{}\n", serde_json::to_string_pretty(&doc)?))?;
    }
    Ok(Outcome::Added(format!("{name} ({}; it asks you to sign in)", path.display())))
}

/// Append `[mcp_servers.roll]` to Codex's config, unless it has one.
fn codex(path: &Path, url: &str, dry: bool) -> anyhow::Result<Outcome> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let parsed: toml::Table = text.parse().map_err(|e| anyhow::anyhow!("{} isn't TOML ({e}): left as it is", path.display()))?;
    if parsed.get("mcp_servers").and_then(|m| m.get("roll")).is_some() {
        return Ok(Outcome::Already(format!("Codex ({} has [mcp_servers.roll])", path.display())));
    }
    let gap = if text.is_empty() || text.ends_with("\n\n") { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
    let next = format!("{text}{gap}[mcp_servers.roll]\nurl = {}\n", toml::Value::String(url.to_string()));
    if !dry {
        write(path, &next)?;
    }
    Ok(Outcome::Added(format!("Codex ({}; then: codex mcp login roll)", path.display())))
}

/// Write a whole file through a temporary one beside it, so a failure never leaves half of it.
fn write(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("roll-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// An Agent Plugins 1.0 bundle (agent-plugins.org, the format Stripe, Vercel, Sentry and Datadog
/// ship): `plugin.json`; `mcp.json` naming roll's server at `url` over streamable HTTP (the spec
/// defines no auth: the client signs in with OAuth, so nothing secret is written); and
/// `skills/<name>/SKILL.md` for each skill. A skill's name comes from the server, so one that isn't
/// an Agent Skills name (lowercase letters, digits, single hyphens, at most 64) is refused before it
/// becomes a folder. How many skills were written.
pub fn write_plugin(dir: &Path, url: &str, version: &str, skills: &[(String, String)]) -> anyhow::Result<usize> {
    let good = |n: &str| n.len() <= 64 && !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-') && !n.ends_with('-') && !n.contains("--");
    if let Some((bad, _)) = skills.iter().find(|(n, _)| !good(n)) {
        anyhow::bail!("the server named a skill {bad:?}, which isn't an Agent Skills name: nothing written");
    }
    let manifest = json!({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
        "name": "roll",
        "version": version,
        "description": "roll's MCP server (every product's tools, through roll's permissions) and its skills: workflows for deploying, debugging and securing what runs on roll.",
        "homepage": "https://roll.sh",
        "keywords": ["roll", "mcp", "postgres", "deploy", "previews", "secrets", "search"],
    });
    let mcp = json!({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
        "mcpServers": { "roll": { "type": "streamable-http", "url": url } },
    });
    write(&dir.join("plugin.json"), &format!("{}\n", serde_json::to_string_pretty(&manifest)?))?;
    write(&dir.join("mcp.json"), &format!("{}\n", serde_json::to_string_pretty(&mcp)?))?;
    for (name, text) in skills {
        write(&dir.join("skills").join(name).join("SKILL.md"), text)?;
    }
    Ok(skills.len())
}

/// Whether `program` is on PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plugin bundle: the manifest and the MCP entry as the spec has them (no token anywhere),
    /// each skill in its folder; a skill name that could leave the folder is refused, nothing written.
    #[test]
    fn a_plugin_bundle_follows_agent_plugins_1_0() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://api.roll.sh/v1/mcp";
        let skills = vec![("using-roll".to_string(), "---\nname: using-roll\n---\n".to_string()), ("ship-a-service".to_string(), "---\nname: ship-a-service\n---\n".to_string())];
        assert_eq!(write_plugin(dir.path(), url, "1.2.3", &skills).unwrap(), 2);
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("plugin.json")).unwrap()).unwrap();
        assert_eq!((manifest["$schema"].as_str(), manifest["name"].as_str()), (Some("https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"), Some("roll")));
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["roll"], json!({ "type": "streamable-http", "url": url }));
        assert!(std::fs::read_to_string(dir.path().join("skills/ship-a-service/SKILL.md")).unwrap().contains("ship-a-service"));
        let other = tempfile::tempdir().unwrap();
        assert!(write_plugin(other.path(), url, "1", &[("../escape".into(), "x".into())]).is_err());
        assert!(!other.path().join("plugin.json").exists(), "nothing written");
    }

    /// Each client found gets roll once, by its own mechanism; what's in its file stays; a second
    /// run changes nothing; a dry run writes nothing.
    #[test]
    fn install_adds_roll_once_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::write(home.join(".cursor/mcp.json"), r#"{"mcpServers": {"other": {"url": "https://x.example/mcp"}}, "keep": 1}"#).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"o5\"\n").unwrap();
        let ran = std::cell::RefCell::new(Vec::new());
        let claude = |args: &[&str]| {
            ran.borrow_mut().push(args.join(" "));
            args[1] == "add"
        };
        let here = Here { home: home.clone(), on_path: &|p| p == "claude", claude: &claude };
        assert_eq!(found(&here), [Client::ClaudeCode, Client::Cursor, Client::Codex]);
        let url = "https://api.roll.sh/v1/mcp?tools=search";

        // Dry: nothing written, nothing run but the check.
        assert!(matches!(install(&here, Client::Cursor, url, true).unwrap(), Outcome::Added(_)));
        assert!(!std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap().contains("api.roll.sh"));

        for c in found(&here) {
            assert!(matches!(install(&here, c, url, false).unwrap(), Outcome::Added(_)), "{c:?}");
        }
        let cursor: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap()).unwrap();
        assert_eq!((&cursor["mcpServers"]["roll"]["url"], &cursor["mcpServers"]["other"]["url"], &cursor["keep"]), (&json!(url), &json!("https://x.example/mcp"), &json!(1)));
        let codex: toml::Table = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap().parse().unwrap();
        assert_eq!((codex["model"].as_str(), codex["mcp_servers"]["roll"]["url"].as_str()), (Some("o5"), Some(url)));
        assert!(ran.borrow().iter().any(|r| r == &format!("mcp add --transport http --scope user roll {url}")), "{:?}", ran.borrow());

        // Again: everything's there; the files are as they were.
        let before = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(matches!(install(&here, Client::Codex, url, false).unwrap(), Outcome::Already(_)));
        assert!(matches!(install(&here, Client::Cursor, url, false).unwrap(), Outcome::Already(_)));
        assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), before);
        // A file that isn't JSON is left alone, with an error that says so.
        std::fs::write(home.join(".cursor/mcp.json"), "{ not json").unwrap();
        assert!(install(&here, Client::Cursor, url, false).is_err());
        assert_eq!(std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap(), "{ not json");
    }
}
