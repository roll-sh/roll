//! Installing an agent's npm or PyPI package with the machine's own `npm` or `uv`, into the
//! daemon's directory, never globally: T3 Code's `ensureGlobalPackage`,
//! `discoverNpmGlobalPackage` and `discoverUvGlobalPackage` (`AcpRegistrySupport.ts`), with
//! `npm_config_prefix` and `UV_TOOL_DIR`/`UV_TOOL_BIN_DIR` pointed at the agent's folder.

use super::index::Package;
use anyhow::{Context, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// T3's `PACKAGE_INSTALL_TIMEOUT`.
const TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// T3's `MAX_PACKAGE_MANIFEST_BYTES`.
const MAX_MANIFEST: u64 = 1 << 20;
/// How much of a failed install's output the error keeps.
const TAIL: usize = 2048;

/// `npm install --global <spec>` with the prefix at `prefix`; the package's command.
pub async fn npm(npm: &Path, agent: &str, pkg: &Package, prefix: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(prefix)?;
    let env = [
        ("npm_config_prefix", prefix.as_os_str()),
        ("npm_config_update_notifier", "false".as_ref()),
        ("npm_config_fund", "false".as_ref()),
        ("npm_config_audit", "false".as_ref()),
    ];
    run(npm, &["install", "--global", &pkg.spec], &env).await?;
    let root = if cfg!(windows) { prefix.join("node_modules") } else { prefix.join("lib").join("node_modules") };
    let package_root = pkg.name.split('/').fold(root, |p, part| p.join(part));
    let manifest = read_manifest(&package_root.join("package.json"))?;
    if manifest.get("name").and_then(Value::as_str) != Some(pkg.name.as_str()) || manifest.get("version").and_then(Value::as_str) != Some(pkg.version.as_str()) {
        bail!("npm installed something other than {}", pkg.spec);
    }
    let command = npm_command(agent, &pkg.name, manifest.get("bin")).with_context(|| format!("{} doesn't say which of its commands to run", pkg.spec))?;
    let bin = if cfg!(windows) { prefix.join(format!("{command}.cmd")) } else { prefix.join("bin").join(&command) };
    if crate::provider::acp::which(&bin).is_none() {
        bail!("npm installed {} but not its command {command}", pkg.spec);
    }
    Ok(bin)
}

/// `uv tool install --force <spec>` with its tools and commands under `root`; the package's
/// command.
pub async fn uv(uv: &Path, agent: &str, pkg: &Package, root: &Path) -> anyhow::Result<PathBuf> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)?;
    run(uv, &["tool", "install", "--force", &pkg.spec], &[("UV_TOOL_DIR", root.as_os_str()), ("UV_TOOL_BIN_DIR", bin.as_os_str())]).await?;
    command_candidates(agent, &pkg.name)
        .into_iter()
        .map(|c| bin.join(c))
        .find(|p| crate::provider::acp::which(p).is_some())
        .with_context(|| format!("uv installed {} but none of its commands is named after it", pkg.spec))
}

/// T3's `npmCommandName`: the only command, else the one named after the package or the agent,
/// else the one all of them point at.
fn npm_command(agent: &str, package: &str, bin: Option<&Value>) -> Option<String> {
    let base = package.rsplit('/').next().unwrap_or(package);
    match bin? {
        Value::String(_) => Some(base.to_string()),
        Value::Object(map) => {
            let mut names: Vec<&String> = map.keys().filter(|k| command_name(k)).collect();
            names.sort();
            if names.len() == 1 {
                return Some(names[0].clone());
            }
            for want in [base, agent] {
                if map.contains_key(want) && command_name(want) {
                    return Some(want.to_string());
                }
            }
            let first = names.first()?;
            names.iter().all(|n| map[*n] == map[*first]).then(|| first.to_string())
        }
        _ => None,
    }
}

/// T3's `packageCommandCandidates`.
fn command_candidates(agent: &str, package: &str) -> Vec<String> {
    let base = package.rsplit('/').next().unwrap_or(package);
    let mut out: Vec<String> = Vec::new();
    for c in [agent, base, base.strip_suffix("-acp").unwrap_or(base)] {
        if command_name(c) && !out.iter().any(|o| o == c) {
            out.push(c.to_string());
        }
    }
    out
}

/// T3's `PackageCommandName`: `^[a-z0-9][a-z0-9._-]*$` (any case), at most 128 bytes.
fn command_name(s: &str) -> bool {
    s.len() <= 128 && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn read_manifest(path: &Path) -> anyhow::Result<Value> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("no {}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        bail!("{} isn't a package manifest", path.display());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Runs a package manager, at most [`TIMEOUT`]; a failure says the end of what it printed.
async fn run(program: &Path, args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", program.display()))?;
    let mut stderr = child.stderr.take().expect("piped");
    let work = async {
        let mut tail = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = stderr.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > TAIL {
                tail.drain(..tail.len() - TAIL);
            }
        }
        anyhow::Ok((child.wait().await?, tail))
    };
    let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (status, tail) = tokio::time::timeout(TIMEOUT, work).await.map_err(|_| anyhow::anyhow!("{name} didn't finish within {} minutes", TIMEOUT.as_secs() / 60))??;
    if !status.success() {
        let said = String::from_utf8_lossy(&tail);
        bail!("{name} {} failed ({status}): {}", args.join(" "), said.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn npm_commands() {
        assert_eq!(npm_command("cline", "cline", Some(&json!("bin/cline.js"))).as_deref(), Some("cline"));
        assert_eq!(npm_command("gemini", "@google/gemini-cli", Some(&json!({ "gemini": "dist/index.js" }))).as_deref(), Some("gemini"));
        let two = json!({ "qwen": "cli.js", "qwen-code": "other.js" });
        assert_eq!(npm_command("qwen-code", "@qwen-code/qwen-code", Some(&two)).as_deref(), Some("qwen-code"));
        let same = json!({ "a": "cli.js", "b": "cli.js" });
        assert_eq!(npm_command("x", "@s/y", Some(&same)).as_deref(), Some("a"));
        assert_eq!(npm_command("x", "@s/y", Some(&json!({ "a": "1.js", "b": "2.js" }))), None);
        assert_eq!(npm_command("x", "y", None), None);
    }

    #[test]
    fn uv_command_candidates() {
        assert_eq!(command_candidates("fast-agent", "fast-agent-acp"), ["fast-agent", "fast-agent-acp"]);
        assert_eq!(command_candidates("minion-code", "minion-code"), ["minion-code"]);
    }
}
