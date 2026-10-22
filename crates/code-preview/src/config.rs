//! `roll.toml`: the checks a repo runs and how its previews start.
//!
//! ```toml
//! [checks.test]
//! image = "rust:1"
//! run = "cargo test"
//! timeout = 900          # seconds, default 600
//!
//! [preview]
//! image = "node:22"
//! run = "pnpm i && pnpm dev --port $PORT"
//! port = 3000
//! postgres = "prj_…"     # roll Postgres project to branch
//! redis = "rdb_…"        # roll Redis database to branch
//!
//! [queue]                # the merge queue (code_control::settings)
//! checks = ["test"]
//! batch_size = 8
//!
//! [main]
//! auto_revert = true
//! ```
//!
//! Parsing is strict (unknown keys are errors), and every error names the key at fault, because
//! it ends up as the summary of a failed check or preview.

use code_control::settings::{MainSettings, QueueSettings, RawMain, RawQueue};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub const FILE: &str = "roll.toml";
/// `roll.toml` is configuration, not content: refuse anything bigger.
const MAX_BYTES: usize = 64 << 10;
const DEFAULT_TIMEOUT_S: u64 = 600;
const MAX_TIMEOUT_S: u64 = 3600;
const DEFAULT_MEMORY_MB: u32 = 1024;
const MAX_MEMORY_MB: u32 = 16 << 10;
const MAX_CHECKS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct RollToml {
    pub checks: BTreeMap<String, CheckSpec>,
    pub preview: Option<PreviewSpec>,
    pub queue: QueueSettings,
    pub main: MainSettings,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckSpec {
    pub image: String,
    pub run: String,
    pub timeout: Duration,
    pub memory_mb: u32,
    pub cpus: f64,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewSpec {
    pub image: String,
    pub run: String,
    pub port: u16,
    /// roll Postgres project (`prj_…`) whose `main` each preview branches.
    pub postgres: Option<String>,
    /// roll Redis database (`rdb_…`) each preview branches.
    pub redis: Option<String>,
    /// Give every change a preview (they cost nothing until visited). `false`: only on request.
    pub auto: bool,
    pub memory_mb: u32,
    pub cpus: f64,
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    checks: BTreeMap<String, RawCheck>,
    preview: Option<RawPreview>,
    queue: Option<RawQueue>,
    main: Option<RawMain>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    image: String,
    run: String,
    timeout: Option<u64>,
    memory: Option<u32>,
    cpus: Option<f64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreview {
    image: String,
    run: String,
    port: u16,
    postgres: Option<String>,
    redis: Option<String>,
    auto: Option<bool>,
    memory: Option<u32>,
    cpus: Option<f64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl RollToml {
    /// `Ok(None)` when the tree has no `roll.toml`.
    pub fn load(dir: &Path) -> Result<Option<RollToml>, String> {
        let path = dir.join(FILE);
        match std::fs::symlink_metadata(&path) {
            Err(_) => return Ok(None),
            Ok(m) if !m.is_file() => return Err(format!("{FILE} must be a regular file")),
            Ok(m) if m.len() as usize > MAX_BYTES => return Err(format!("{FILE} is larger than {} KiB", MAX_BYTES >> 10)),
            Ok(_) => {}
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{FILE}: {e}"))?;
        Self::parse(&text).map(Some)
    }

    pub fn parse(text: &str) -> Result<RollToml, String> {
        if text.len() > MAX_BYTES {
            return Err(format!("{FILE} is larger than {} KiB", MAX_BYTES >> 10));
        }
        let raw: RawFile = toml::from_str(text).map_err(|e| format!("{FILE}: {}", e.to_string().trim()))?;
        if raw.checks.len() > MAX_CHECKS {
            return Err(format!("{FILE}: at most {MAX_CHECKS} checks"));
        }
        let mut checks = BTreeMap::new();
        for (name, c) in raw.checks {
            let at = format!("[checks.{name}]");
            if name.is_empty() || name.len() > 100 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
                return Err(format!("{FILE}: check name {name:?} must be 1-100 of [A-Za-z0-9._-]"));
            }
            let timeout = c.timeout.unwrap_or(DEFAULT_TIMEOUT_S);
            if !(1..=MAX_TIMEOUT_S).contains(&timeout) {
                return Err(format!("{FILE} {at}: timeout must be 1-{MAX_TIMEOUT_S} seconds"));
            }
            checks.insert(
                name,
                CheckSpec {
                    image: image(&at, c.image)?,
                    run: run(&at, c.run)?,
                    timeout: Duration::from_secs(timeout),
                    memory_mb: memory(&at, c.memory)?,
                    cpus: cpus(&at, c.cpus)?,
                    env: env(&at, c.env)?,
                },
            );
        }
        let preview = match raw.preview {
            None => None,
            Some(p) => {
                let at = "[preview]";
                if p.port == 0 {
                    return Err(format!("{FILE} {at}: port must be 1-65535"));
                }
                if let Some(id) = &p.postgres
                    && !valid_id(id, "prj_")
                {
                    return Err(format!("{FILE} {at}: postgres must be a roll Postgres project id (prj_…)"));
                }
                if let Some(id) = &p.redis
                    && !valid_id(id, "rdb_")
                {
                    return Err(format!("{FILE} {at}: redis must be a roll Redis database id (rdb_…)"));
                }
                Some(PreviewSpec {
                    image: image(at, p.image)?,
                    run: run(at, p.run)?,
                    port: p.port,
                    postgres: p.postgres,
                    redis: p.redis,
                    auto: p.auto.unwrap_or(true),
                    memory_mb: memory(at, p.memory)?,
                    cpus: cpus(at, p.cpus)?,
                    env: env(at, p.env)?,
                })
            }
        };
        let queue = match raw.queue {
            None => QueueSettings::default(),
            Some(q) => code_control::settings::queue(q)?,
        };
        if let Some(names) = &queue.checks
            && let Some(missing) = names.iter().find(|n| !checks.contains_key(*n))
        {
            return Err(format!("{FILE} [queue]: checks names {missing:?}, which isn't a [checks.{missing}] table"));
        }
        let main = raw.main.map(code_control::settings::main).unwrap_or_default();
        Ok(RollToml { checks, preview, queue, main })
    }
}

fn valid_id(id: &str, prefix: &str) -> bool {
    id.len() <= 64 && id.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn image(at: &str, image: String) -> Result<String, String> {
    let image = image.trim().to_string();
    // An image reference: registry/name:tag@digest. No whitespace, and never an option.
    if image.is_empty()
        || image.len() > 255
        || image.starts_with('-')
        || !image.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
    {
        return Err(format!("{FILE} {at}: image {image:?} isn't a valid image reference"));
    }
    Ok(image)
}

fn run(at: &str, run: String) -> Result<String, String> {
    if run.trim().is_empty() {
        return Err(format!("{FILE} {at}: run must be a shell command"));
    }
    if run.len() > 8192 {
        return Err(format!("{FILE} {at}: run is longer than 8 KiB"));
    }
    Ok(run)
}

fn memory(at: &str, mb: Option<u32>) -> Result<u32, String> {
    let mb = mb.unwrap_or(DEFAULT_MEMORY_MB);
    if !(64..=MAX_MEMORY_MB).contains(&mb) {
        return Err(format!("{FILE} {at}: memory must be 64-{MAX_MEMORY_MB} (MiB)"));
    }
    Ok(mb)
}

fn cpus(at: &str, cpus: Option<f64>) -> Result<f64, String> {
    let cpus = cpus.unwrap_or(1.0);
    if !(0.1..=16.0).contains(&cpus) {
        return Err(format!("{FILE} {at}: cpus must be 0.1-16"));
    }
    Ok(cpus)
}

fn env(at: &str, env: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    for (k, v) in &env {
        let mut bytes = k.bytes();
        let valid = bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid || k.len() > 128 {
            return Err(format!("{FILE} {at}: env name {k:?} must match [A-Za-z_][A-Za-z0-9_]*"));
        }
        if k.starts_with("ROLL_") {
            return Err(format!("{FILE} {at}: env name {k} is reserved (ROLL_*)"));
        }
        if v.contains('\0') || v.len() > 8192 {
            return Err(format!("{FILE} {at}: env {k} has an invalid value"));
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let t = RollToml::parse(
            r#"
            [checks.test]
            image = "rust:1"
            run = "cargo test"

            [checks.lint]
            image = "ghcr.io/acme/lint@sha256:abc"
            run = "lint ."
            timeout = 120
            memory = 512
            cpus = 0.5
            env = { RUST_LOG = "debug" }

            [preview]
            image = "node:22"
            run = "pnpm i && pnpm dev --port $PORT"
            port = 3000
            postgres = "prj_abc123"
            redis = "rdb_def456"
            "#,
        )
        .unwrap();
        assert_eq!(t.checks.len(), 2);
        let test = &t.checks["test"];
        assert_eq!((test.timeout, test.memory_mb, test.cpus), (Duration::from_secs(600), 1024, 1.0));
        let lint = &t.checks["lint"];
        assert_eq!((lint.timeout.as_secs(), lint.memory_mb, lint.env["RUST_LOG"].as_str()), (120, 512, "debug"));
        let p = t.preview.unwrap();
        assert_eq!((p.port, p.postgres.as_deref(), p.redis.as_deref(), p.auto), (3000, Some("prj_abc123"), Some("rdb_def456"), true));
    }

    #[test]
    fn queue_and_main_tables() {
        let t = RollToml::parse(
            "[checks.test]\nimage = \"a\"\nrun = \"x\"\n[queue]\nchecks = [\"test\"]\nbatch_size = 4\nrequire_change_checks = true\n[main]\nauto_revert = true\n",
        )
        .unwrap();
        assert_eq!(t.queue.checks.as_deref(), Some(&["test".to_string()][..]));
        assert_eq!(t.queue.batch_size, 4);
        assert!(t.queue.require_change_checks);
        assert!(t.main.auto_revert);
    }

    #[test]
    fn empty_file_has_nothing() {
        assert_eq!(RollToml::parse("").unwrap(), RollToml { checks: BTreeMap::new(), preview: None, queue: Default::default(), main: Default::default() });
    }

    #[test]
    fn errors_name_the_key() {
        let cases = [
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\nbogus = 1", "bogus"),
            ("[checks.t]\nimage = \"a\"", "run"),
            ("[checks.t]\nimage = \"--privileged\"\nrun = \"x\"", "[checks.t]: image"),
            ("[checks.t]\nimage = \"a b\"\nrun = \"x\"", "image"),
            ("[checks.t]\nimage = \"a\"\nrun = \"  \"", "[checks.t]: run"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\ntimeout = 0", "timeout"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\ntimeout = 99999", "timeout"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\ntimeout = \"60s\"", "timeout"),
            ("[checks.\"a b\"]\nimage = \"a\"\nrun = \"x\"", "check name"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\nenv = { ROLL_X = \"1\" }", "reserved"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\nenv = { \"1A\" = \"1\" }", "env name"),
            ("[checks.t]\nimage = \"a\"\nrun = \"x\"\nmemory = 1", "memory"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 0", "port"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\npostgres = \"db\"", "postgres"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\nredis = \"rdb_\"", "redis"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"", "port"),
            ("[queue]\nbatch_size = 0", "batch_size"),
            ("[queue]\nbogus = 1", "bogus"),
            ("[queue]\nchecks = [\"nope\"]", "[checks.nope]"),
            ("[main]\nrevert = true", "revert"),
            ("not toml at all", "roll.toml"),
        ];
        for (text, needle) in cases {
            let err = RollToml::parse(text).expect_err(text);
            assert!(err.contains(needle), "{text:?}: {err:?} should mention {needle:?}");
        }
    }

    #[test]
    fn refuses_huge_files() {
        let big = format!("# {}", "x".repeat(MAX_BYTES));
        assert!(RollToml::parse(&big).unwrap_err().contains("larger"));
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(RollToml::load(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join(FILE), "[preview]\nimage = \"busybox\"\nrun = \"httpd -f\"\nport = 80\nauto = false\n").unwrap();
        assert!(!RollToml::load(dir.path()).unwrap().unwrap().preview.unwrap().auto);
    }
}
