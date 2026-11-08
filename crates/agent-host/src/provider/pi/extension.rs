//! roll's Pi extension (`extension.ts`): written under the daemon's data directory when its
//! content changes, and configured through the environment of each Pi process (T3:
//! `materializePiT3McpExtension` and `buildPiRpcLaunch` in `mcpInjection.ts`). The extension
//! holds no secrets: MCP servers and their headers (roll's orchestrator token among them) only
//! ever travel in the process's environment.

use crate::provider::McpServer;
use agent_proto::RuntimeMode;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The extension's source, embedded.
pub const SOURCE: &str = include_str!("extension.ts");
/// Its file name under the provider's directory.
pub const FILE_NAME: &str = "roll-pi-extension.ts";

/// The MCP servers, as JSON `[{name, url, headers}]`.
pub const MCP_ENV: &str = "ROLL_PI_MCP";
/// `full_access`, `auto_edit` or `ask`.
pub const MODE_ENV: &str = "ROLL_PI_RUNTIME_MODE";
/// Text joined to the system prompt.
pub const INSTRUCTIONS_ENV: &str = "ROLL_PI_INSTRUCTIONS";
/// Inherited values of these belong to some other session: never pass them on.
pub const ALL_ENV: [&str; 3] = [MCP_ENV, MODE_ENV, INSTRUCTIONS_ENV];

/// Write the extension into `dir` unless it's already there as it is; its path.
pub fn materialize(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    if std::fs::read(&path).ok().as_deref() != Some(SOURCE.as_bytes()) {
        // Write beside it and rename, so a Pi starting meanwhile never loads half a file.
        let tmp = dir.join(format!(".{FILE_NAME}.{}", std::process::id()));
        std::fs::write(&tmp, SOURCE)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

/// The environment that configures the extension for one session.
pub fn env(mode: RuntimeMode, mcp: &[McpServer], instructions: Option<&str>) -> Vec<(String, String)> {
    let mode = match mode {
        RuntimeMode::FullAccess => "full_access",
        RuntimeMode::AutoEdit => "auto_edit",
        RuntimeMode::Ask => "ask",
    };
    let mut env = vec![(MODE_ENV.to_string(), mode.to_string())];
    if !mcp.is_empty() {
        let servers: Vec<Value> = mcp
            .iter()
            .map(|s| {
                let headers: Map<String, Value> = s.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({ "name": s.name, "url": s.url, "headers": headers })
            })
            .collect();
        env.push((MCP_ENV.to_string(), Value::Array(servers).to_string()));
    }
    if let Some(text) = instructions.map(str::trim).filter(|t| !t.is_empty()) {
        env.push((INSTRUCTIONS_ENV.to_string(), text.to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = materialize(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        materialize(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
        std::fs::write(&path, "stale").unwrap();
        materialize(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SOURCE);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
