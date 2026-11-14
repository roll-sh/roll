//! A project's preview rules (docs/previews.md, `Settings`). Stored as JSON per project; a
//! project without a row has the defaults, previews off.

use crate::{Error, Kind, Mode, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_ENV: usize = 200;
pub const MAX_BRANCH_GLOBS: usize = 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Globs on a change's branch (`*` for every branch). A change with no branch name (made in
    /// roll) counts as `*`.
    pub branches: Vec<String>,
    /// Previews for draft changes too.
    pub drafts: bool,
    /// Only changes whose title asks (`[preview]`) get one (Render's manual mode).
    pub manual: bool,
    /// Gitignore-style patterns (CODEOWNERS'): a change gets a preview only when it touches a path
    /// one matches (empty: any path) …
    pub paths: Vec<String>,
    /// … that none of these match (empty: none ignored). A change's paths are everything since
    /// its merge base with the default branch.
    pub ignore_paths: Vec<String>,
    /// `"<kind>:<ref>"` → `branch`, `schema` or `skip`; resources not listed get `data`.
    pub resources: BTreeMap<String, ResourceRule>,
    pub data: Mode,
    /// Re-branch every resource from its parent this often (0: never).
    pub reset_every_hours: u32,
    pub sleep_after_s: u32,
    pub ttl_hours: u32,
    pub max_previews: u32,
    pub max_bytes: u64,
    /// What the project's previews may cost in a month, in USD (0: no cap). Once they've cost
    /// that this month, new previews wait (`queued`) and the running ones carry on.
    pub max_usd_month: f64,
    pub secrets: SecretsRule,
    pub env: BTreeMap<String, String>,
    /// Variables for a family of branches (Vercel's per-branch preview variables): a branch glob
    /// → `{name: value}`, over `env` and under a preview's own. Every glob that matches a
    /// preview's branch applies, in the globs' order.
    pub env_for: BTreeMap<String, BTreeMap<String, String>>,
    pub github: GithubRule,
    /// Who may open a preview's URLs: `link` (default) needs the preview's link, which carries
    /// its access token once and then a cookie; `public` is anyone who can reach the domain.
    pub access: Access,
    /// Chromatic's UI Review, with screenshots on: `check` gives a change a visual review (the
    /// `roll/visual` commit status), pending while its pages differ from the baseline's and
    /// nobody has accepted them at that head; `required` also keeps it from landing in roll.
    pub visual_review: VisualReviewRule,
    /// `catch` (the default): services send mail to roll's catcher (`SMTP_*` are roll's, mail
    /// providers' API keys are removed), listed on the preview, never delivered; `deliver`: the
    /// project's own mail settings reach previews.
    #[serde(default)]
    pub mail: MailRule,
    /// Branch the data as of a point in time (Neon's time-travel branches), where the product
    /// keeps history: `-24h` (`-30m`, `-7d`) before each branching or reset, or an RFC 3339 time;
    /// at most 30 days back. Products without history branch as of now and say so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_at: Option<String>,
    /// A feedback button on the pages of people who came in through roll (link access): it pins a
    /// note to an element and posts it on the change, from roll.
    pub feedback: bool,
    /// Wake the change's services as soon as a new head is deployed, so the first visit (often
    /// the pull request comment's link) doesn't wait for a cold start.
    pub warm: bool,
    /// A command run once on the preview's fresh data (after branching, and after each reset)
    /// before its services deploy: anonymise, migrate, load fixtures.
    pub seed: Option<Seed>,
    /// A command run at each new head of the change (after the seed, before services deploy,
    /// and again after a reset): migrations, Heroku's release phase. A failure fails the preview,
    /// and its services stay on what they ran.
    pub release: Option<Seed>,
    /// A command run once when the preview is deleted, before its services, secrets environment
    /// and branches go (Heroku's `pr-predestroy`): what its code made outside roll (a test-mode
    /// webhook, an OAuth redirect URI, a DNS record) cleaned up with the preview's own variables.
    /// Only at a head the preview ran; a failure is logged and the deletion goes on. A crash
    /// during it runs it again: it should be safe to repeat.
    pub teardown: Option<Seed>,
    /// Column masks for NoSQL branches: `"nosql:<ref>"` → `{"keyspace.table.column": "mask_inner(1, 1)"}`
    /// (Cassandra 5's mask functions). Merged onto what the branch inherits from its parent,
    /// whenever it's made or reset: the preview reads masked values, the parent is untouched.
    pub masking: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    /// The container image it runs in (`postgres:18-alpine`, `node:22`).
    pub image: String,
    /// Run as `sh -c <run>` in the change's tree, with the branches' variables.
    pub run: String,
    #[serde(default = "seed_timeout")]
    pub timeout_s: u32,
    #[serde(default = "seed_memory")]
    pub memory_mb: u32,
}

fn seed_timeout() -> u32 {
    600
}

fn seed_memory() -> u32 {
    1024
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailRule {
    #[default]
    Catch,
    Deliver,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisualReviewRule {
    #[default]
    Off,
    Check,
    Required,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Link,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceRule {
    Branch,
    Schema,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsRule {
    pub base: String,
    pub allow_production: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubRule {
    pub comment: bool,
    pub status: bool,
    pub deployment: bool,
    /// A screenshot of the first service at each new head (it wakes the service once per push).
    #[serde(default)]
    pub screenshot: bool,
    /// With screenshots on: keep a preview of the default branch (Chromatic's baseline) while
    /// the project has a change preview, so each change is compared with it. It follows the
    /// branch, sleeps between screenshots and holds no slot.
    #[serde(default = "yes")]
    pub baseline: bool,
    /// More of the first service's pages to take with `/` (Percy's snapshot list): paths, at most
    /// [`MAX_PAGES`], each compared with its own screenshot before and with the baseline's.
    #[serde(default)]
    pub pages: Vec<String>,
    /// CSS selectors blanked before each screenshot (Percy's `percyCSS`): a clock, an ad, a
    /// random avatar, so they don't count as a change. Their box stays.
    #[serde(default)]
    pub hide: Vec<String>,
    /// How slow and heavy the first page may get (Lighthouse budgets): over it, the `roll/speed`
    /// commit status fails. 0 is no limit.
    #[serde(default)]
    pub budget: Budget,
    /// A page counts as changed for the visual review only past this share of its pixels (a
    /// percent, up to 50; Chromatic's `diffThreshold`, Percy's sensitivity): for what `hide`
    /// can't name. 0: any change counts.
    #[serde(default)]
    pub visual_threshold: f64,
    /// More widths the first page is taken at (Percy's widths: a tablet's 768), each compared
    /// and reviewed like another page (kept as `@768`). Up to 3, 320 to 2560 pixels; they share
    /// a run's five other pages with `pages`.
    #[serde(default)]
    pub widths: Vec<u32>,
    /// A path of the first service's serving its OpenAPI document as JSON (`/openapi.json`,
    /// `/v3/api-docs`): read with the screenshot, its operations are compared with main's (Optic's
    /// and Bump's API diff), an operation main has and the change doesn't named as breaking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openapi: Option<String>,
}

/// The unix second `data_at` means at `now` (unix seconds): `-24h`/`-30m`/`-7d` back from now, or
/// an RFC 3339 time; `None` for anything else or more than 30 days back.
pub fn data_at_secs(rule: &str, now: i64) -> Option<i64> {
    const MAX: i64 = 30 * 86_400;
    let rule = rule.trim();
    let at = match rule.strip_prefix('-') {
        Some(rel) => {
            let (n, unit) = rel.split_at(rel.len().checked_sub(1)?);
            let n: i64 = n.parse().ok().filter(|n| *n > 0)?;
            let per = match unit {
                "m" => 60,
                "h" => 3_600,
                "d" => 86_400,
                _ => return None,
            };
            now - n.checked_mul(per)?
        }
        None => humantime::parse_rfc3339_weak(rule).ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
    };
    (at <= now && now - at <= MAX).then_some(at)
}

/// A width's page entry (`@768`): the first page at that width.
pub fn width_entry(e: &str) -> Option<u32> {
    e.strip_prefix('@').filter(|n| !n.starts_with('0')).and_then(|n| n.parse::<u32>().ok()).filter(|w| (320..=2560).contains(w))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    pub load_ms: u32,
    pub bytes: u64,
    /// Largest contentful paint (Core Web Vitals' "good" is up to 2500).
    pub lcp_ms: u32,
    /// Cumulative layout shift ("good" is up to 0.1).
    pub cls: f64,
}

pub const MAX_HIDE: usize = 20;

/// How a search branch's field is masked: `redact` (`[masked]`), `remove` (the field goes) or
/// `email` (`user-<document id>@example.invalid`, so it still looks like one and stays unique).
pub const SEARCH_MASKS: [&str; 3] = ["redact", "remove", "email"];

/// Whether `s` may go into the screenshot's stylesheet as a selector: it can't end its rule.
pub fn selector_ok(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 200 && !s.contains(['{', '}', '<', ';', '\\']) && !s.contains("/*") && s.chars().all(|c| !c.is_control())
}

pub const MAX_PAGES: usize = 10;

/// A `github.pages` entry: a path of the first service's, or `<service>:<path>` of another's
/// (Percy snapshots each app). `None` when it's neither.
pub fn page_entry(e: &str) -> Option<(Option<&str>, &str)> {
    if let Some((svc, _)) = e.split_once(":/")
        && !svc.is_empty()
        && svc.len() <= 63
        && svc.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        let path = &e[svc.len() + 1..];
        return page_path_ok(path).then_some((Some(svc), path));
    }
    page_path_ok(e).then_some((None, e))
}

/// Whether `p` is a page path the screenshot may visit: absolute, on the service's own origin.
pub fn page_path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && p.len() <= 200 && p.bytes().all(|b| b.is_ascii_graphic() && !b"\\#\"'<>`".contains(&b))
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            branches: vec!["*".into()],
            drafts: false,
            manual: false,
            paths: Vec::new(),
            ignore_paths: Vec::new(),
            resources: BTreeMap::new(),
            data: Mode::Branch,
            reset_every_hours: 0,
            sleep_after_s: 600,
            ttl_hours: 168,
            max_previews: 10,
            max_bytes: 10 << 30,
            max_usd_month: 0.0,
            secrets: SecretsRule::default(),
            env: BTreeMap::new(),
            env_for: BTreeMap::new(),
            github: GithubRule::default(),
            access: Access::Link,
            visual_review: VisualReviewRule::Off,
            mail: MailRule::Catch,
            data_at: None,
            feedback: true,
            warm: true,
            seed: None,
            release: None,
            teardown: None,
            masking: BTreeMap::new(),
        }
    }
}

impl Default for SecretsRule {
    fn default() -> Self {
        SecretsRule { base: "development".into(), allow_production: false }
    }
}

impl Default for GithubRule {
    fn default() -> Self {
        GithubRule { comment: true, status: true, deployment: true, screenshot: false, baseline: true, pages: Vec::new(), hide: Vec::new(), budget: Budget::default(), visual_threshold: 0.0, widths: Vec::new(), openapi: None }
    }
}

/// Whether an environment name is production's.
/// Whether an environment name looks like production's. Errs on the side of yes: `prod`,
/// `production-eu`, `prd`, `live`, `main`, `master`.
pub fn is_production(env: &str) -> bool {
    let e = env.to_ascii_lowercase();
    e.contains("prod") || e.split(['-', '_']).any(|w| matches!(w, "prd" | "live" | "main" | "master"))
}

impl Settings {
    /// These settings with `patch`'s fields on top (objects merge, everything else replaces),
    /// checked.
    pub fn patched(&self, patch: &Value) -> Result<Settings> {
        if !patch.is_object() {
            return Err(Error::BadRequest("settings are a JSON object".into()));
        }
        let mut v = serde_json::to_value(self).expect("settings serialize");
        merge(&mut v, patch);
        let s: Settings = serde_json::from_value(v).map_err(|e| Error::BadRequest(format!("settings: {e}")))?;
        s.check()?;
        Ok(s)
    }

    pub fn check(&self) -> Result<()> {
        let bad = |m: String| Err(Error::BadRequest(m));
        if self.branches.is_empty() || self.branches.len() > MAX_BRANCH_GLOBS || self.branches.iter().any(|b| b.is_empty() || b.len() > 200) {
            return bad(format!("branches: 1–{MAX_BRANCH_GLOBS} globs of 1–200 characters"));
        }
        for (name, list) in [("paths", &self.paths), ("ignore_paths", &self.ignore_paths)] {
            if list.len() > MAX_BRANCH_GLOBS || list.iter().any(|p| p.len() > 200 || code_control::review::pattern::Pattern::new(p).is_none()) {
                return bad(format!("{name}: at most {MAX_BRANCH_GLOBS} patterns of 1–200 characters"));
            }
            if let Some(p) = list.iter().find(|p| p.trim_start().starts_with('!')) {
                return bad(format!("{name}: {p:?}: negation (!) isn't supported; narrow `paths` instead"));
            }
        }
        for key in self.resources.keys() {
            let Some((kind, r)) = key.split_once(':') else { return bad(format!("resources: {key:?} isn't \"<kind>:<ref>\"")) };
            let kind = Kind::parse(kind)?;
            if self.resources[key] == ResourceRule::Schema && !kind.has_schema_mode() {
                return bad(format!("resources: {key}: a {} branch can't be structure-only yet; branch it or skip it", kind.as_str()));
            }
            if r.is_empty() {
                return bad(format!("resources: {key:?} has no ref"));
            }
        }
        if !(60..=7 * 86_400).contains(&self.sleep_after_s) {
            return bad("sleep_after_s: 60 to 604800".into());
        }
        if !(1..=24 * 90).contains(&self.ttl_hours) {
            return bad("ttl_hours: 1 to 2160".into());
        }
        if self.reset_every_hours > 24 * 90 {
            return bad("reset_every_hours: 0 (never) to 2160".into());
        }
        if !(1..=500).contains(&self.max_previews) {
            return bad("max_previews: 1 to 500".into());
        }
        if !self.max_usd_month.is_finite() || self.max_usd_month < 0.0 {
            return bad("max_usd_month: 0 (no cap) or more".into());
        }
        if self.max_bytes < (1 << 20) {
            return bad("max_bytes: at least 1 MiB".into());
        }
        let base = &self.secrets.base;
        if base.is_empty() || base.len() > 64 || !base.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return bad("secrets.base: an environment name (1–64 of a-z, 0-9, - and _)".into());
        }
        if is_production(base) && !self.secrets.allow_production {
            return bad(format!("secrets.base {base:?} is production's: set secrets.allow_production to inherit it"));
        }
        for (name, job) in [("seed", &self.seed), ("release", &self.release), ("teardown", &self.teardown)] {
            let Some(job) = job else { continue };
            if job.image.trim().is_empty() || job.image.len() > 255 || job.image.contains(char::is_whitespace) {
                return bad(format!("{name}.image: an image reference"));
            }
            if job.run.trim().is_empty() || job.run.len() > 8192 {
                return bad(format!("{name}.run: a command, at most 8 KiB"));
            }
            if !(10..=3600).contains(&job.timeout_s) {
                return bad(format!("{name}.timeout_s: 10 to 3600"));
            }
            if !(64..=16384).contains(&job.memory_mb) {
                return bad(format!("{name}.memory_mb: 64 to 16384"));
            }
        }
        for (key, cols) in &self.masking {
            let of = |k: &str| key.strip_prefix(k).is_some_and(|r| !r.is_empty());
            let (postgres, search) = (of("postgres:"), of("search:"));
            if !postgres && !search && !of("nosql:") {
                return bad(format!("masking: {key:?} isn't \"postgres:<ref>\", \"nosql:<ref>\" or \"search:<ref>\" (only those branches can mask)"));
            }
            if cols.len() > 200 {
                return bad(format!("masking: {key}: at most 200 columns"));
            }
            if search {
                // `index.field` (a nested field's path: `users.address.city`) → a search rule.
                for (field, rule) in cols {
                    let ok = field.len() <= 200 && field.split('.').count() >= 2 && !field.split('.').any(str::is_empty) && !field.contains(['*', ',', '/', ' ', '\\']);
                    if !ok || !SEARCH_MASKS.contains(&rule.as_str()) {
                        return bad(format!("masking: {key}: {field:?} → {rule:?} isn't \"index.field\" → one of {}", SEARCH_MASKS.join(", ")));
                    }
                }
                continue;
            }
            for (col, f) in cols {
                if col.split('.').count() != 3 || col.split('.').any(str::is_empty) || col.len() > 200 || f.trim().is_empty() || f.len() > 200 {
                    return bad(format!("masking: {key}: {col:?} → {f:?} isn't \"schema.table.column\" → a rule"));
                }
                if postgres && !crate::mask::is_rule(f) {
                    return bad(format!("masking: {key}: {col}: {f:?} isn't a rule ({})", crate::mask::RULES));
                }
            }
        }
        if self.env.len() > MAX_ENV {
            return bad(format!("env: at most {MAX_ENV} variables"));
        }
        if self.env_for.len() > MAX_BRANCH_GLOBS || self.env_for.keys().any(|g| g.is_empty() || g.len() > 200) {
            return bad(format!("env_for: at most {MAX_BRANCH_GLOBS} branch globs of 1–200 characters"));
        }
        if self.env_for.values().map(BTreeMap::len).sum::<usize>() > MAX_ENV {
            return bad(format!("env_for: at most {MAX_ENV} variables in all"));
        }
        for (k, v) in self.env.iter().chain(self.env_for.values().flatten()) {
            let name_ok = !k.is_empty()
                && k.len() <= 128
                && k.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !name_ok || k.starts_with("ROLL_") {
                return bad(format!("env: {k:?} isn't a variable name (ROLL_* is roll's)"));
            }
            if v.len() > 32 << 10 {
                return bad(format!("env: {k} is over 32 KiB"));
            }
        }
        if self.github.hide.len() > MAX_HIDE || !self.github.hide.iter().all(|s| selector_ok(s)) {
            return bad(format!("github.hide: at most {MAX_HIDE} CSS selectors of up to 200 characters (no braces, semicolons or comments)"));
        }
        if let Some(at) = &self.data_at {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            if data_at_secs(at, now).is_none() {
                return bad(format!("data_at: {at:?} isn't -<n>m, -<n>h, -<n>d or an RFC 3339 time within 30 days"));
            }
        }
        let w = &self.github.widths;
        if w.len() > 3 || w.iter().any(|x| width_entry(&format!("@{x}")).is_none()) || (1..w.len()).any(|i| w[..i].contains(&w[i])) {
            return bad("github.widths: up to 3 different widths, 320 to 2560 pixels".into());
        }
        if !(0.0..=50.0).contains(&self.github.visual_threshold) {
            return bad("github.visual_threshold: a percent from 0 to 50".into());
        }
        let b = &self.github.budget;
        if b.load_ms > 600_000 || b.bytes > 10 << 30 || b.lcp_ms > 600_000 || !(0.0..=100.0).contains(&b.cls) {
            return bad("github.budget: load_ms and lcp_ms up to 600000, bytes up to 10 GiB, cls from 0 to 100".into());
        }
        let pages = &self.github.pages;
        if pages.len() > MAX_PAGES || pages.iter().any(|p| page_entry(p).is_none() || p == "/") || pages.iter().enumerate().any(|(i, p)| pages[..i].contains(p)) {
            return bad(format!(
                "github.pages: at most {MAX_PAGES} different paths besides /, each starting with / or naming a service (admin:/users), up to 200 characters, no #, quotes or spaces"
            ));
        }
        if self.github.openapi.as_deref().is_some_and(|p| !page_path_ok(p)) {
            return bad("github.openapi: a path of the first service's (/openapi.json), up to 200 characters, no #, quotes or spaces".into());
        }
        Ok(())
    }

    /// How a project resource is branched, or `None` to leave it out. With `data: "schema"`,
    /// a kind that can't be structure-only is left out unless a rule says to branch it: a
    /// project that asked for no rows never gets a full copy by default.
    pub fn mode_of(&self, kind: Kind, r#ref: &str) -> Option<Mode> {
        match self.resources.get(&format!("{}:{}", kind.as_str(), r#ref)) {
            Some(ResourceRule::Skip) => None,
            Some(ResourceRule::Schema) => kind.has_schema_mode().then_some(Mode::Schema),
            Some(ResourceRule::Branch) => Some(Mode::Branch),
            None if self.data == Mode::Schema && !kind.has_schema_mode() => None,
            None => Some(self.data),
        }
    }
}

fn merge(into: &mut Value, patch: &Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    // `resources`, `env`, `env_for` and `masking` are maps: a patch replaces them
                    // whole, so a key can be removed.
                    Some(slot) if v.is_object() && !matches!(k.as_str(), "resources" | "env" | "env_for" | "masking") => merge(slot, v),
                    _ => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patches_merge_and_production_needs_a_yes() {
        // A point in time for the data: a while back (at most 30 days), or a time.
        assert_eq!(data_at_secs("-24h", 1_000_000), Some(1_000_000 - 86_400));
        assert_eq!(data_at_secs("1970-01-12T00:00:00Z", 1_000_000), Some(950_400));
        for bad in ["-31d", "-0h", "24h", "-5w", "yesterday"] {
            assert_eq!(data_at_secs(bad, 10_000_000), None, "{bad}");
        }
        assert!(Settings::default().patched(&json!({ "data_at": "soon" })).is_err());
        // Widths: up to three different ones a tablet or a wide screen has, kept as `@<width>`.
        assert!(Settings::default().patched(&json!({ "github": { "widths": [768, 1920] } })).is_ok());
        for bad in [json!([768, 768]), json!([100]), json!([400, 500, 600, 700])] {
            assert!(Settings::default().patched(&json!({ "github": { "widths": bad } })).is_err(), "{bad}");
        }
        assert_eq!((width_entry("@768"), width_entry("@0768"), width_entry("/pricing")), (Some(768), None, None));
        let s = Settings::default().patched(&json!({ "enabled": true, "github": { "comment": false } })).unwrap();
        assert!(s.enabled && !s.github.comment && s.github.status);
        assert!(Settings::default().patched(&json!({ "secrets": { "base": "production" } })).is_err());
        let p = Settings::default().patched(&json!({ "secrets": { "base": "production", "allow_production": true } })).unwrap();
        assert_eq!(p.secrets.base, "production");
        assert!(Settings::default().patched(&json!({ "nope": 1 })).is_err());
        assert!(Settings::default().patched(&json!({ "env": { "ROLL_X": "1" } })).is_err());
        // Per-branch variables: checked like `env`, and replaced whole (a removed one goes).
        assert!(Settings::default().patched(&json!({ "env_for": { "feat/*": { "ROLL_X": "1" } } })).is_err());
        let two = Settings::default().patched(&json!({ "env_for": { "feat/*": { "A": "1", "B": "2" } } })).unwrap();
        let one = two.patched(&json!({ "env_for": { "feat/*": { "A": "1" } } })).unwrap();
        assert_eq!(one.env_for["feat/*"].len(), 1);
        let r = Settings::default().patched(&json!({ "resources": { "redis:rdb_1": "skip", "nosql:n1": "schema" } })).unwrap();
        assert_eq!(r.mode_of(Kind::Redis, "rdb_1"), None);
        assert_eq!(r.mode_of(Kind::Nosql, "n1"), Some(Mode::Schema));
        assert_eq!(r.mode_of(Kind::Bucket, "b"), Some(Mode::Branch));
        // Structure-only where a kind can't do it: refused, never a silent full copy.
        assert!(Settings::default().patched(&json!({ "resources": { "bucket:b": "schema" } })).is_err());
        assert!(Settings::default().patched(&json!({ "github": { "hide": ["time", ".clock, #ad"] } })).is_ok());
        assert!(Settings::default().patched(&json!({ "masking": { "search:catalog": { "users.email": "email", "users.address.city": "redact" } } })).is_ok());
        for bad in [json!({ "users": "redact" }), json!({ "users.email": "hash" }), json!({ "*.email": "redact" })] {
            assert!(Settings::default().patched(&json!({ "masking": { "search:catalog": bad } })).is_err(), "{bad}");
        }
        assert!(Settings::default().patched(&json!({ "github": { "hide": ["a} body {display:none"] } })).is_err());
        assert!(Settings::default().patched(&json!({ "github": { "pages": ["/pricing", "admin:/", "admin:/users"] } })).is_ok());
        assert_eq!(page_entry("admin:/users"), Some((Some("admin"), "/users")));
        assert_eq!(page_entry("/a:/b"), Some((None, "/a:/b")));
        for pages in [json!(["Admin:/"]), json!(["admin://evil.example/"]), json!(["pricing"]), json!(["/a\"b"]), json!(["/a", "/a"]), json!(["/1", "/2", "/3", "/4", "/5", "/6", "/7", "/8", "/9", "/10", "/11"])] {
            assert!(Settings::default().patched(&json!({ "github": { "pages": pages } })).is_err(), "{pages}");
        }
        let schema = Settings::default().patched(&json!({ "data": "schema" })).unwrap();
        assert_eq!(schema.mode_of(Kind::Redis, "rdb_1"), None);
        assert_eq!(schema.mode_of(Kind::Postgres, "prj_1"), Some(Mode::Schema));
        assert_eq!(schema.mode_of(Kind::Stream, "st_1"), Some(Mode::Schema));
        assert!(Settings::default().patched(&json!({ "masking": { "nosql:n1": { "app.users.email": "mask_default()" } } })).is_ok());
        assert!(Settings::default().patched(&json!({ "masking": { "postgres:p1": { "a.b.c": "mask_null()" } } })).is_err(), "Postgres takes rules, not functions");
        assert!(Settings::default().patched(&json!({ "masking": { "postgres:p1": { "public.users.email": "email" } } })).is_ok());
        assert!(Settings::default().patched(&json!({ "masking": { "redis:r1": { "a.b.c": "null" } } })).is_err(), "Redis can't mask");
    }
}
