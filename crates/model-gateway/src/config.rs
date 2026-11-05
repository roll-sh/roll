//! The standalone gateway's config file (`roll gateway serve --config gateway.toml`), and its
//! hot reload: the file is read again when it changes (checked every 2 s) and applied live
//! with [`crate::Gateway::apply`]; a file that doesn't parse or check keeps the previous
//! settings and logs why.
//!
//! The file is [`agent_proto::GatewaySettings`] in TOML plus `[keys]`: `upstream = "env:VAR"`
//! or `"file:/path"` (never the key itself in the file), and `data_dir`, where `gateway.db`
//! lives (relative paths are from the file's directory; unset: the file's directory). Unlike
//! the daemon's settings, `enabled` defaults to on here: a file you serve is meant to serve.
//! Client keys are made with `roll gateway client add <name>`, which prints the key once.
//!
//! ```toml
//! port = 8790
//! listen = ["100.64.0.10"]           # besides loopback
//!
//! [[upstreams]]
//! id = "anthropic"
//! name = "Anthropic"
//! kind = "anthropic"
//!
//! [[routes]]
//! model = "fast"
//! targets = [{ upstream = "anthropic", model = "claude-haiku-4-5" }]
//!
//! [keys]
//! anthropic = "env:ANTHROPIC_API_KEY"
//! ```
//!
//! A key file is read when the config is (at start and on each change of the config file
//! itself): after rotating a key file, touch the config.
//!
//! The store helper owns this module.

use crate::Secret;
use agent_proto::GatewaySettings;
use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often [`watch`] looks at the file.
pub const INTERVAL: Duration = Duration::from_secs(2);
/// The config file is at most this big.
const FILE_CAP: u64 = 1024 * 1024;
/// A key (from a file or a variable) is at most this big: a Vertex service account's JSON fits.
const KEY_CAP: u64 = 16 * 1024;

/// The keys the file may have besides the settings'.
const OWN_KEYS: &[&str] = &["keys", "data_dir"];
/// [`GatewaySettings`]' fields (a misspelt one is an error, not silently a default).
const SETTINGS_KEYS: &[&str] = &["enabled", "port", "listen", "upstreams", "routes", "prices", "log_bodies", "affinity", "retention_days"];

/// A loaded config: what [`crate::Gateway::apply`] takes, and where the store lives.
pub struct Loaded {
    pub settings: GatewaySettings,
    /// Upstream id → key, for upstreams that have one (a local model server may need none).
    pub keys: HashMap<String, Secret>,
    /// The directory `gateway.db` goes in.
    pub data_dir: PathBuf,
    /// The file as it was read, for [`watch`].
    pub stamp: Stamp,
}

/// When the file last changed, as far as polling can tell (modified time and length).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(Stamp { modified: m.modified().ok(), len: m.len() })
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(flatten)]
    settings: GatewaySettings,
    #[serde(default)]
    keys: BTreeMap<String, String>,
    #[serde(default)]
    data_dir: Option<PathBuf>,
}

/// Reads, parses and checks the file at `path`, then resolves its keys. Every error says
/// what to fix, and never shows a key.
pub fn load(path: &Path) -> anyhow::Result<Loaded> {
    let stamp = Stamp::of(path).with_context(|| format!("can't read {}", path.display()))?;
    let text = read_capped(path, FILE_CAP).with_context(|| format!("reading {}", path.display()))?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut parsed = parse(&text, dir).with_context(|| format!("in {}", path.display()))?;
    parsed.stamp = stamp;
    Ok(parsed)
}

/// [`load`] without the file: `text` as if it were in `dir`.
pub fn parse(text: &str, dir: &Path) -> anyhow::Result<Loaded> {
    let mut table: toml::Table = toml::from_str(text).context("it isn't valid TOML")?;
    for k in table.keys() {
        if !OWN_KEYS.contains(&k.as_str()) && !SETTINGS_KEYS.contains(&k.as_str()) {
            bail!("unknown setting {k:?} (the gateway's settings are {} and {})", SETTINGS_KEYS.join(", "), OWN_KEYS.join(", "));
        }
    }
    table.entry("enabled").or_insert(toml::Value::Boolean(true));
    let file: File = table.try_into().map_err(|e: toml::de::Error| anyhow::anyhow!("{}", e.message()))?;
    let settings = file.settings.check().map_err(anyhow::Error::msg)?;
    for u in &settings.upstreams {
        if u.key_ref.is_some() {
            bail!("upstream {}: key_ref is the daemon's; here, give its key in [keys] as \"env:VAR\" or \"file:/path\"", u.id);
        }
    }
    let mut keys = HashMap::new();
    for (id, source) in file.keys {
        if settings.upstream(&id).is_none() {
            bail!("[keys] names {id:?}, which isn't an upstream");
        }
        let key = resolve(source.trim(), dir).with_context(|| format!("the key for {id}"))?;
        keys.insert(id, key);
    }
    let data_dir = match file.data_dir {
        Some(d) if d.is_absolute() => d,
        Some(d) => dir.join(d),
        None => dir.to_path_buf(),
    };
    Ok(Loaded { settings, keys, data_dir, stamp: Stamp { modified: None, len: 0 } })
}

/// `env:VAR` or `file:/path`; anything else may be a key pasted into the file, so it's
/// refused without being echoed.
fn resolve(source: &str, dir: &Path) -> anyhow::Result<Secret> {
    let key = if let Some(var) = source.strip_prefix("env:") {
        let value = std::env::var(var).map_err(|_| anyhow::anyhow!("${var} isn't set"))?;
        if value.len() as u64 > KEY_CAP {
            bail!("${var} is longer than {KEY_CAP} bytes");
        }
        Secret::new(value.trim().to_string())
    } else if let Some(file) = source.strip_prefix("file:") {
        let path = dir.join(file);
        let text = read_capped(&path, KEY_CAP).with_context(|| format!("reading {}", path.display()))?;
        Secret::new(text.trim().to_string())
    } else {
        bail!("is neither \"env:VAR\" nor \"file:/path\" (never put the key itself in the config file)");
    };
    if key.expose().is_empty() {
        bail!("is empty");
    }
    Ok(key)
}

/// The file's text, refusing one longer than `cap` (read through a bounded reader, so a file
/// that grows meanwhile can't make it read more).
fn read_capped(path: &Path, cap: u64) -> anyhow::Result<String> {
    let mut out = String::new();
    std::fs::File::open(path)?.take(cap + 1).read_to_string(&mut out).context("it isn't UTF-8 text")?;
    if out.len() as u64 > cap {
        bail!("it's bigger than {cap} bytes");
    }
    Ok(out)
}

/// Looks at `path` every `every` (use [`INTERVAL`]) and, when it changed since `since`, loads
/// it and calls `on_change` with the result. A file that doesn't load (gone, invalid, a key
/// missing) is logged and skipped: the caller keeps what it has, and the next change is tried
/// again. Runs until the future is dropped.
pub async fn watch<F, Fut>(path: PathBuf, since: Stamp, every: Duration, mut on_change: F)
where
    F: FnMut(Loaded) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut last = Some(since);
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        let now = Stamp::of(&path);
        if now == last {
            continue;
        }
        last = now;
        if now.is_none() {
            tracing::warn!("{} is gone: keeping the settings in use", path.display());
            continue;
        }
        match load(&path) {
            Ok(loaded) => {
                tracing::info!("{} changed: applying it", path.display());
                on_change(loaded).await;
            }
            Err(e) => tracing::warn!("{} changed but can't be used, so the settings in use stay: {e:#}", path.display()),
        }
    }
}
