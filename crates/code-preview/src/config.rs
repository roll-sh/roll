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
//! health = "/healthz"    # ready only once this answers 2xx/3xx (optional)
//! postgres = "prj_…"     # roll Postgres project to branch
//! redis = "rdb_…"        # roll Redis database to branch
//!
//! [queue]                # the merge queue (code_control::settings)
//! checks = ["test"]
//! batch_size = 8
//!
//! [main]
//! auto_revert = true
//!
//! [secrets]              # roll code's landing rule for credentials (code_control::settings)
//! unstored_credentials = "block"
//! project = "shop"       # where teammates' roll secrets are: the `roll` CLI reads it
//! ```
//!
//! Parsing is strict (unknown keys are errors), and every error names the key at fault, because
//! it ends up as the summary of a failed check or preview.

use code_control::settings::{MainSettings, QueueSettings, RawMain, RawQueue, RawSecrets};
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
    /// roll apps' `[app.previews.screenshots]`: what roll previews' screenshots of the service do
    /// (its pages, what to blank, its states), checked here, the rest of `[app]` roll apps' own.
    pub app_screenshots: Option<Screenshots>,
}

/// What a service's preview screenshots take beyond its first page, from its repo: `[preview]`'s
/// `pages`, `hide` and `[[states]]`, or roll apps' `[app.previews.screenshots]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Screenshots {
    pub pages: Vec<String>,
    pub hide: Vec<String>,
    pub states: Vec<StateRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScreenshots {
    #[serde(default)]
    pages: Vec<String>,
    #[serde(default)]
    hide: Vec<String>,
    #[serde(default)]
    states: Vec<StateRule>,
}

/// `pages`, `hide` and `states` within roll previews' rules, or what's wrong, under `at`.
fn screenshot_rules(at: &str, pages: &[String], hide: &[String], states: &[StateRule]) -> Result<(), String> {
    if pages.len() > 10 || pages.iter().any(|x| !page_path_ok(x) || x == "/") {
        return Err(format!("{FILE} {at}: pages are at most 10 paths besides / (each starting with /, no spaces, quotes or #)"));
    }
    let selector_ok = |s: &str| !s.trim().is_empty() && s.len() <= 200 && !s.contains(['{', '}', '<', ';', '\\']) && !s.contains("/*") && !s.chars().any(char::is_control);
    if hide.len() > 20 || !hide.iter().all(|s| selector_ok(s)) {
        return Err(format!("{FILE} {at}: hide is at most 20 CSS selectors"));
    }
    let bad_state = states.iter().enumerate().any(|(i, s)| {
        !state_name_ok(&s.name) || states[..i].iter().any(|o| o.name == s.name) || !page_path_ok(&s.page) || s.steps.is_empty() || s.steps.len() > MAX_STEPS || !s.steps.iter().all(step_ok)
    });
    if states.len() > MAX_STATES || bad_state {
        return Err(format!(
            "{FILE} {at}: states are at most {MAX_STATES}, each a name (a-z, 0-9, -), a page (a path) and 1 to {MAX_STEPS} steps: click or type on a selector, press one of {}, or wait up to 5000 ms",
            STEP_KEYS.join(", ")
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckSpec {
    pub image: String,
    pub run: String,
    pub timeout: Duration,
    pub memory_mb: u32,
    pub cpus: f64,
    pub env: BTreeMap<String, String>,
    /// A roll secrets environment (`development`, `staging:/api`) of the repo's workspace project
    /// whose values the check gets (docs/secrets.md); never a protected one.
    pub secrets: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewSpec {
    /// The service's name in a roll preview environment (docs/previews.md, "Services"); the
    /// repo's name when not set.
    pub name: Option<String>,
    pub image: String,
    pub run: String,
    pub port: u16,
    /// A path the service answers 2xx/3xx on once it works: a preview is ready only then.
    pub health: Option<String>,
    /// roll Postgres project (`prj_…`) whose `main` each preview branches.
    pub postgres: Option<String>,
    /// roll Redis database (`rdb_…`) each preview branches.
    pub redis: Option<String>,
    /// Give every change a preview (they cost nothing until visited). `false`: only on request.
    pub auto: bool,
    pub memory_mb: u32,
    pub cpus: f64,
    pub env: BTreeMap<String, String>,
    /// Page states its screenshots reach (`[[preview.states]]`), as roll previews' `github.states`.
    pub states: Vec<StateRule>,
    /// More of its own pages its screenshots take (`pages = ["/pricing"]`), and selectors blanked
    /// in them (`hide = [".clock"]`), as roll previews' `github.pages` and `github.hide`.
    pub pages: Vec<String>,
    pub hide: Vec<String>,
}

/// Whether `p` is a page path the screenshot may visit: absolute, on the service's own origin.
pub fn page_path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && p.len() <= 200 && p.bytes().all(|b| b.is_ascii_graphic() && !b"\\#\"'<>`".contains(&b))
}

/// A page state (roll previews' `github.states`, or a repo's `[[preview.states]]`): `page` of the first service's, then its steps.
#[derive(Clone, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRule {
    pub name: String,
    pub page: String,
    pub steps: Vec<Step>,
}

/// One step towards a state, on the page's own elements: `{"click": selector}`, `{"type":
/// [selector, text]}`, `{"press": key}`, `{"wait": ms}`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Click(String),
    Type(String, String),
    Press(String),
    Wait(u32),
}

/// The most states, and steps in each.
pub const MAX_STATES: usize = 3;
pub const MAX_STEPS: usize = 10;
/// The keys a `press` step may name.
pub const STEP_KEYS: [&str; 9] = ["Enter", "Escape", "Tab", "ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Space", "Backspace"];

/// A state's name: 1–40 of a-z, 0-9 and `-`.
pub fn state_name_ok(n: &str) -> bool {
    (1..=40).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A `state:<name>` page entry's name.
pub fn state_entry(e: &str) -> Option<&str> {
    e.strip_prefix("state:").filter(|n| state_name_ok(n))
}

/// Whether a step is one roll knows, bounded (checked when set, and again before a run).
pub fn step_ok(s: &Step) -> bool {
    let text = |t: &str| t.len() <= 200 && !t.chars().any(char::is_control);
    let selector = |t: &str| !t.trim().is_empty() && text(t);
    match s {
        Step::Click(sel) => selector(sel),
        Step::Type(sel, t) => selector(sel) && text(t),
        Step::Press(k) => STEP_KEYS.contains(&k.as_str()),
        Step::Wait(ms) => *ms <= 5000,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    checks: BTreeMap<String, RawCheck>,
    preview: Option<RawPreview>,
    queue: Option<RawQueue>,
    main: Option<RawMain>,
    /// roll code's landing settings for credentials (`code_control::settings::secrets`), and roll
    /// secrets' suggestion for teammates (project, environment, path, branches), which the `roll`
    /// CLI reads (`roll-cli/src/secrets/repo.rs`).
    secrets: Option<RawSecrets>,
    /// roll apps' section (`apps_control::config`): read there; here only its
    /// `previews.screenshots`, roll previews'.
    app: Option<toml::Table>,
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
    secrets: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreview {
    name: Option<String>,
    image: String,
    run: String,
    port: u16,
    health: Option<String>,
    postgres: Option<String>,
    redis: Option<String>,
    auto: Option<bool>,
    memory: Option<u32>,
    cpus: Option<f64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Page states the preview's screenshots reach (roll previews): versioned with the code
    /// whose selectors they name.
    #[serde(default)]
    states: Vec<StateRule>,
    #[serde(default)]
    pages: Vec<String>,
    #[serde(default)]
    hide: Vec<String>,
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
                    secrets: secrets(&at, c.secrets)?,
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
                let label = |n: &str| {
                    (1..=20).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-') && !n.ends_with('-')
                };
                if p.name.as_deref().is_some_and(|n| !label(n)) {
                    return Err(format!("{FILE} {at}: name is 1-20 characters of a-z, 0-9 and -"));
                }
                screenshot_rules(at, &p.pages, &p.hide, &p.states)?;
                Some(PreviewSpec {
                    name: p.name,
                    image: image(at, p.image)?,
                    run: run(at, p.run)?,
                    port: p.port,
                    health: match p.health {
                        Some(h) if h.starts_with('/') && h.len() <= 200 && !h.contains(|c: char| c.is_control() || c.is_whitespace()) => Some(h),
                        Some(_) => return Err(format!("{FILE} {at}: health is a path starting with / (at most 200 characters, no spaces)")),
                        None => None,
                    },
                    postgres: p.postgres,
                    redis: p.redis,
                    auto: p.auto.unwrap_or(true),
                    memory_mb: memory(at, p.memory)?,
                    cpus: cpus(at, p.cpus)?,
                    env: env(at, p.env)?,
                    states: p.states,
                    pages: p.pages,
                    hide: p.hide,
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
        raw.secrets.map(code_control::settings::secrets).transpose()?;
        // roll apps' `[app.previews.screenshots]`, the rest of `[app]` left to roll apps.
        let app_screenshots = match raw.app.as_ref().and_then(|a| a.get("previews")).and_then(|p| p.get("screenshots")) {
            None => None,
            Some(v) => {
                let at = "[app.previews.screenshots]";
                let s: RawScreenshots = v.clone().try_into().map_err(|e| format!("{FILE} {at}: {e}"))?;
                screenshot_rules(at, &s.pages, &s.hide, &s.states)?;
                Some(Screenshots { pages: s.pages, hide: s.hide, states: s.states })
            }
        };
        Ok(RollToml { checks, preview, queue, main, app_screenshots })
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

/// `env` or `env:/folder`: an environment's slug and a folder path.
fn secrets(at: &str, s: Option<String>) -> Result<Option<String>, String> {
    let Some(s) = s else { return Ok(None) };
    let (env, path) = s.split_once(':').unwrap_or((&s, "/"));
    let slug = (1..=32).contains(&env.len()) && env.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let folder = path.starts_with('/') && path.len() <= 600 && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b));
    if !slug || !folder {
        return Err(format!("{FILE} {at}: secrets is an environment (development) or an environment and folder (staging:/api)"));
    }
    Ok(Some(s))
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
            r##"
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

            pages = ["/pricing"]
            hide = [".clock"]

            [[preview.states]]
            name = "cart"
            page = "/"
            steps = [{ click = "#open-cart" }, { type = ["#coupon", "SAVE10"] }, { press = "Enter" }, { wait = 300 }]
            "##,
        )
        .unwrap();
        assert_eq!(t.checks.len(), 2);
        let test = &t.checks["test"];
        assert_eq!((test.timeout, test.memory_mb, test.cpus), (Duration::from_secs(600), 1024, 1.0));
        let lint = &t.checks["lint"];
        assert_eq!((lint.timeout.as_secs(), lint.memory_mb, lint.env["RUST_LOG"].as_str()), (120, 512, "debug"));
        let p = t.preview.unwrap();
        assert_eq!((p.port, p.postgres.as_deref(), p.redis.as_deref(), p.auto), (3000, Some("prj_abc123"), Some("rdb_def456"), true));
        assert_eq!((p.pages.as_slice(), p.hide.as_slice(), p.states.len()), (&["/pricing".to_string()][..], &[".clock".to_string()][..], 1));
        assert_eq!(p.states[0].steps, [Step::Click("#open-cart".into()), Step::Type("#coupon".into(), "SAVE10".into()), Step::Press("Enter".into()), Step::Wait(300)]);
    }

    #[test]
    fn queue_and_main_tables() {
        let t = RollToml::parse(
            // [app] is roll apps' (apps_control::config): allowed here, not read.
            "[checks.test]\nimage = \"a\"\nrun = \"x\"\n[queue]\nchecks = [\"test\"]\nbatch_size = 4\nrequire_change_checks = true\n[main]\nauto_revert = true\n[app]\nbuild = \"pnpm build\"\n",
        )
        .unwrap();
        assert_eq!(t.queue.checks.as_deref(), Some(&["test".to_string()][..]));
        assert_eq!(t.queue.batch_size, 4);
        assert!(t.queue.require_change_checks);
        assert!(t.main.auto_revert);
        assert_eq!(t.app_screenshots, None);
        // roll apps' service names what roll previews' screenshots do under [app.previews.screenshots].
        let a = RollToml::parse("[app]\nbuild = \"x\"\n[app.previews.screenshots]\npages = [\"/pricing\"]\nhide = [\".clock\"]\n[[app.previews.screenshots.states]]\nname = \"cart\"\npage = \"/\"\nsteps = [{ press = \"Tab\" }]\n").unwrap();
        let s = a.app_screenshots.unwrap();
        assert_eq!((s.pages, s.hide, s.states.len()), (vec!["/pricing".to_string()], vec![".clock".to_string()], 1));
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
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\nhealth = \"healthz\"", "health"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\nredis = \"rdb_\"", "redis"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"", "port"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\nhide = [\"a{}\"]", "hide"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\npages = [\"pricing\"]", "pages"),
            ("[preview]\nimage = \"a\"\nrun = \"x\"\nport = 3000\n[[preview.states]]\nname = \"cart\"\npage = \"/\"\nsteps = [{ press = \"F12\" }]", "states"),
            ("[app.previews.screenshots]\npages = [\"pricing\"]", "[app.previews.screenshots]"),
            ("[app.previews.screenshots]\nbogus = 1", "[app.previews.screenshots]"),
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

    /// One `[secrets]` table holds roll code's landing rule and the CLI's suggestion: both parsers
    /// take it, and roll code's rule still reads.
    #[test]
    fn the_secrets_table_holds_the_landing_rule_and_the_suggestion() {
        let text = "[secrets]\nunstored_credentials = \"block\"\nproject = \"shop\"\nenvironment = \"development\"\npath = \"/api\"\nbranches = { \"release/*\" = \"staging\" }\n";
        RollToml::parse(text).unwrap();
        let s = code_control::settings::parse(text).unwrap();
        assert_eq!(s.secrets.unstored_credentials, code_control::settings::Unstored::Block);
        assert!(RollToml::parse("[secrets]\nprojects = \"shop\"\n").unwrap_err().contains("projects"));
    }

    #[test]
    fn refuses_huge_files() {
        let big = format!("# {}", "x".repeat(MAX_BYTES));
        assert!(RollToml::parse(&big).unwrap_err().contains("larger"));
    }
}
