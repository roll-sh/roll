//! What a repo's `roll.toml` says about landing: the merge queue (`[queue]`) and the default
//! branch (`[main]`). Read from the default branch's head, never from the change being landed,
//! so a change can't loosen the rules it's checked against.
//!
//! ```toml
//! [queue]
//! checks = ["test", "lint"]    # what a batch must pass (default: every check)
//! batch_size = 8               # most changes tested together (1-64)
//! timeout = 3600               # seconds a batch may test for
//! require_change_checks = true # a change's own checks must pass before it's queued
//!
//! [main]
//! auto_revert = true           # revert the change that broke the default branch
//! ```
//!
//! code-preview validates the whole file (strictly) and calls [`parse`] for these two tables.

use serde::Deserialize;

pub const FILE: &str = "roll.toml";
const MAX_BYTES: usize = 64 << 10;
pub const DEFAULT_BATCH: usize = 8;
pub const MAX_BATCH: usize = 64;
pub const DEFAULT_TIMEOUT_S: u64 = 3600;
const MAX_TIMEOUT_S: u64 = 6 * 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct QueueSettings {
    /// Checks a batch must pass; `None`: every check `roll.toml` declares.
    pub checks: Option<Vec<String>>,
    pub batch_size: usize,
    pub timeout_s: u64,
    pub require_change_checks: bool,
}

impl Default for QueueSettings {
    fn default() -> Self {
        QueueSettings { checks: None, batch_size: DEFAULT_BATCH, timeout_s: DEFAULT_TIMEOUT_S, require_change_checks: false }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MainSettings {
    pub auto_revert: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub queue: QueueSettings,
    pub main: MainSettings,
    /// Names of the `[checks.*]` tables.
    pub declared_checks: Vec<String>,
    /// Why `roll.toml` couldn't be read (the defaults apply meanwhile).
    pub error: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawFile {
    #[serde(default)]
    checks: toml::Table,
    queue: Option<RawQueue>,
    main: Option<RawMain>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawQueue {
    checks: Option<Vec<String>>,
    batch_size: Option<i64>,
    timeout: Option<i64>,
    require_change_checks: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawMain {
    auto_revert: Option<bool>,
}

/// The landing settings in a `roll.toml`. Other tables are ignored here (code-preview checks
/// them); errors name the key at fault.
pub fn parse(text: &str) -> Result<Settings, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("{FILE} is larger than {} KiB", MAX_BYTES >> 10));
    }
    let raw: RawFile = toml::from_str(text).map_err(|e| format!("{FILE}: {}", e.to_string().trim()))?;
    let queue = match raw.queue {
        None => QueueSettings::default(),
        Some(q) => queue(q)?,
    };
    let main = raw.main.map(main).unwrap_or_default();
    Ok(Settings { queue, main, declared_checks: raw.checks.keys().cloned().collect(), error: None })
}

/// Validate `[queue]` (code-preview's strict parser hands it here).
pub fn queue(q: RawQueue) -> Result<QueueSettings, String> {
    let batch_size = q.batch_size.unwrap_or(DEFAULT_BATCH as i64);
    if !(1..=MAX_BATCH as i64).contains(&batch_size) {
        return Err(format!("{FILE} [queue]: batch_size must be 1-{MAX_BATCH}"));
    }
    let timeout = q.timeout.unwrap_or(DEFAULT_TIMEOUT_S as i64);
    if !(1..=MAX_TIMEOUT_S as i64).contains(&timeout) {
        return Err(format!("{FILE} [queue]: timeout must be 1-{MAX_TIMEOUT_S} seconds"));
    }
    if let Some(checks) = &q.checks {
        if checks.len() > 32 {
            return Err(format!("{FILE} [queue]: at most 32 checks"));
        }
        if let Some(bad) = checks.iter().find(|c| c.is_empty() || c.len() > 100) {
            return Err(format!("{FILE} [queue]: check name {bad:?} must be 1-100 characters"));
        }
    }
    Ok(QueueSettings {
        checks: q.checks,
        batch_size: batch_size as usize,
        timeout_s: timeout as u64,
        require_change_checks: q.require_change_checks.unwrap_or(false),
    })
}

/// Validate `[main]`.
pub fn main(m: RawMain) -> MainSettings {
    MainSettings { auto_revert: m.auto_revert.unwrap_or(false) }
}

impl Settings {
    /// The checks a batch must pass: `[queue] checks`, else every check declared.
    pub fn batch_checks(&self) -> Vec<String> {
        self.queue.checks.clone().unwrap_or_else(|| self.declared_checks.clone())
    }

    /// The checks a change must have passed before it can be queued: the repo's
    /// `required_checks` (any source, external CI included) and, with
    /// `require_change_checks`, the batch's checks too.
    pub fn change_checks(&self, repo_required: &[String]) -> Vec<String> {
        let mut out = repo_required.to_vec();
        if self.queue.require_change_checks {
            for c in self.batch_checks() {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_tables() {
        let s = parse("[checks.test]\nimage = \"alpine\"\nrun = \"true\"\n").unwrap();
        assert_eq!(s.queue, QueueSettings::default());
        assert!(!s.main.auto_revert);
        assert_eq!(s.declared_checks, vec!["test".to_string()]);
        assert_eq!(s.batch_checks(), vec!["test".to_string()]);
        assert!(s.change_checks(&[]).is_empty(), "a change's own checks aren't required by default");
        assert_eq!(s.change_checks(&["ci/external".into()]), vec!["ci/external".to_string()]);
    }

    #[test]
    fn queue_and_main_tables() {
        let s = parse(
            "[queue]\nchecks = [\"a\"]\nbatch_size = 3\ntimeout = 60\nrequire_change_checks = true\n[main]\nauto_revert = true\n",
        )
        .unwrap();
        assert_eq!(s.queue, QueueSettings { checks: Some(vec!["a".into()]), batch_size: 3, timeout_s: 60, require_change_checks: true });
        assert!(s.main.auto_revert);
        assert_eq!(s.batch_checks(), vec!["a".to_string()]);
        assert_eq!(s.change_checks(&["ext".into(), "a".into()]), vec!["ext".to_string(), "a".to_string()]);
    }

    #[test]
    fn errors_name_the_key() {
        for (text, needle) in [
            ("[queue]\nbatch_size = 0\n", "batch_size"),
            ("[queue]\nbatch_size = 65\n", "batch_size"),
            ("[queue]\ntimeout = 0\n", "timeout"),
            ("[queue]\nbogus = 1\n", "bogus"),
            ("[main]\nrevert = true\n", "revert"),
            ("[queue]\nchecks = [\"\"]\n", "check name"),
            ("not toml", "roll.toml"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(needle), "{text:?}: {e}");
        }
        assert!(parse(&"#".repeat(MAX_BYTES + 1)).unwrap_err().contains("larger"));
    }
}
