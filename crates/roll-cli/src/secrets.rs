//! `roll secrets …` and `roll run` (docs/secrets.md, "Consumption"): a project's secrets and
//! environment variables, and a command run with them in its environment, without files.

use crate::Api;
use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::Read;

/// Which project, environment and folder: flags, then `ROLL_PROJECT`, `ROLL_ENV` and
/// `ROLL_SECRETS_PATH`, then what `roll secrets setup` remembered for this directory or a parent,
/// then `development` and `/`. A scoped token implies its project.
#[derive(Args, Clone, Debug)]
pub struct AtArgs {
    /// The workspace project: a name or a `prj_…` id.
    #[arg(short, long, env = "ROLL_PROJECT", global = true)]
    pub project: Option<String>,
    /// The environment's slug (default: this directory's, else development).
    #[arg(short, long = "env", env = "ROLL_ENV", global = true)]
    pub env: Option<String>,
    /// The folder (default: this directory's, else /).
    #[arg(long, env = "ROLL_SECRETS_PATH", global = true)]
    pub path: Option<String>,
}

/// [`AtArgs`] with the directory's defaults filled in.
#[derive(Clone, Debug)]
pub struct At {
    pub project: Option<String>,
    pub env: String,
    pub path: String,
}

impl AtArgs {
    pub fn resolve(&self) -> At {
        let here = std::env::current_dir().ok().and_then(|d| setup::nearest(&d));
        At {
            project: self.project.clone().or_else(|| here.as_ref().map(|h| h.project.clone())),
            env: self.env.clone().or_else(|| here.as_ref().map(|h| h.env.clone())).unwrap_or_else(|| "development".into()),
            path: self.path.clone().or_else(|| here.as_ref().map(|h| h.path.clone())).unwrap_or_else(|| "/".into()),
        }
    }
}

/// What `roll secrets setup` remembers per directory, in `~/.config/roll/secrets.json` (the
/// user's own config, never the repo: like `doppler setup`).
mod setup {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct Dir {
        pub project: String,
        pub name: String,
        pub env: String,
        pub path: String,
    }

    fn file() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("roll").join("secrets.json"))
    }

    pub fn load() -> BTreeMap<String, Dir> {
        file().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(all: &BTreeMap<String, Dir>) -> anyhow::Result<PathBuf> {
        let f = file().ok_or_else(|| anyhow::anyhow!("no HOME to keep the setup in"))?;
        std::fs::create_dir_all(f.parent().expect("a parent"))?;
        std::fs::write(&f, serde_json::to_string_pretty(all)? + "\n")?;
        Ok(f)
    }

    /// The setup of `dir` or its nearest parent that has one, and the directory it's for.
    pub fn nearest(dir: &Path) -> Option<Dir> {
        let all = load();
        dir.ancestors().find_map(|d| all.get(&d.to_string_lossy().to_string()).cloned())
    }
}

#[derive(Subcommand)]
pub enum Secrets {
    /// Remember the project, environment and folder for this directory (and the ones under it),
    /// so `roll run -- <cmd>` needs no flags here.
    Setup {
        /// Show what applies here.
        #[arg(long)]
        show: bool,
        /// Forget this directory's setup.
        #[arg(long, conflicts_with = "show")]
        clear: bool,
    },
    /// List a folder's secrets (values masked; `--reveal` to show them).
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        reveal: bool,
    },
    /// Print a secret's value (references resolved; `--raw` for the stored text).
    Get {
        key: String,
        #[arg(long)]
        raw: bool,
    },
    /// Set secrets: `KEY=VALUE…`, or one `KEY` with the value on stdin.
    Set {
        #[arg(required = true)]
        pairs: Vec<String>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// A value of your own that replaces the shared one for you only.
        #[arg(long)]
        personal: bool,
        /// Make each KEY a fresh random value of this many characters (letters and digits), made
        /// here: `roll secrets set SESSION_SECRET --generate 48`.
        #[arg(long, value_name = "LENGTH")]
        generate: Option<usize>,
        /// Open a change request instead of writing (protected environments).
        #[arg(long)]
        request: bool,
        #[arg(short, long)]
        message: Option<String>,
        /// When the value stops working outside roll: a date (2027-03-31), RFC 3339, how long from
        /// now (90d), or `never`. Insights flag it and reminders announce it two weeks ahead.
        #[arg(long, value_name = "WHEN")]
        expires: Option<String>,
    },
    /// Date when keys' values stop working outside roll (a vendor's key, a certificate), keeping
    /// the values: `roll secrets expire STRIPE_KEY --on 2027-03-31` (or `90d`, or `never`).
    Expire {
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long, value_name = "WHEN")]
        on: String,
        #[arg(long)]
        request: bool,
    },
    /// Delete secrets.
    #[command(alias = "delete")]
    Rm {
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long)]
        request: bool,
    },
    /// Edit the folder as a `.env` file in `$EDITOR`; saving applies what changed as one change
    /// (removed keys are deleted). Linked keys aren't in the file.
    Edit {
        #[arg(long)]
        request: bool,
    },
    /// Import a `.env` or JSON file (`-` for stdin) as one change. JSON is an object of
    /// `KEY: value` or an array of `{key, value}` (Infisical's and Doppler's exports).
    Import {
        /// A .env or JSON file (`-`: stdin); or `--from` another manager.
        #[arg(required_unless_present = "from")]
        file: Option<String>,
        /// Read it from another manager's API, with your own token from its usual variable:
        /// doppler (`DOPPLER_TOKEN`), vault (`VAULT_ADDR`, `VAULT_TOKEN`), infisical (`INFISICAL_TOKEN`),
        /// aws (Secrets Manager through your `aws` CLI: its profiles and SSO).
        #[arg(long, conflicts_with = "file", requires = "source")]
        from: Option<String>,
        /// What to read there: doppler `project/config`, vault `mount/path` (KV v2), infisical
        /// `workspace-id/environment[/path]`, aws a name prefix (`prod/app/`). With `--map`, the
        /// part before the mapped name.
        #[arg(long)]
        source: Option<String>,
        /// Several environments at once: `dev=development,prd=production` reads `<source>/dev`
        /// into development, and so on (one change each).
        #[arg(long, value_delimiter = ',', requires = "from")]
        map: Vec<String>,
        /// Vault: every secret under the path, each into its own folder (`app/db` → `/db`), one
        /// change per folder.
        #[arg(long, requires = "from")]
        recursive: bool,
        /// Replace keys that exist (default: only add new ones).
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        request: bool,
    },
    /// Fill `${KEY}` in a config file (`-`: stdin) from the environment; a key that isn't set fails.
    /// `\${KEY}` stays as written. Prints it, or writes `--out` (0600).
    Render {
        file: String,
        #[arg(long)]
        out: Option<String>,
        /// Keep running: write `--out` again when the environment changes (checked every
        /// `--interval` seconds), renewing dynamic credentials' leases meanwhile.
        #[arg(long, requires = "out")]
        watch: bool,
        #[arg(long, default_value_t = 5, requires = "watch")]
        interval: u64,
        /// With --watch: a command run (through `sh -c`) after each new file, such as a reload.
        #[arg(long, requires = "watch", value_name = "COMMAND")]
        exec: Option<String>,
    },
    /// Every finding in every environment, worst first: expired, missing, exposed, weak,
    /// expiring, stale, unread. Fails when there's an expired, missing or exposed one.
    Health {
        /// Every project you can read, not only this one.
        #[arg(long, conflicts_with = "code")]
        all: bool,
        /// Also the keys no repository on the project's canvas mentions (roll code's search).
        #[arg(long)]
        code: bool,
    },
    /// Check this machine's setup: the token and who it is, this directory's project and
    /// environment, the folder's keys against `.env.example`, staged files for values. Fails if a
    /// check does.
    Doctor,
    /// The keys a template (`.env.example`) names that the environment doesn't have, without
    /// reading a value; fails when one is missing (`--strict`: also when it has keys the template
    /// doesn't name).
    Check {
        #[arg(default_value = ".env.example")]
        template: String,
        #[arg(long)]
        strict: bool,
    },
    /// Look for this project's values in files (a directory is walked; `--staged`: what `git
    /// commit` would record, for a pre-commit hook). Says where, never the value; fails if found.
    Scan {
        paths: Vec<String>,
        #[arg(long, conflicts_with = "paths")]
        staged: bool,
    },
    /// Print the resolved values: dotenv, json, shell, yaml, docker (`--env-file`) or kubernetes (a Secret manifest).
    Export {
        #[arg(long, default_value = "dotenv")]
        format: String,
        /// kubernetes: the Secret's name (default: the environment's).
        #[arg(long)]
        name: Option<String>,
        /// kubernetes: its namespace.
        #[arg(long)]
        namespace: Option<String>,
        /// The folder's own values as they were after this change (as written then).
        #[arg(long, value_name = "SEQ", conflicts_with = "at")]
        change: Option<i64>,
        /// ...or at this time (RFC 3339, unix seconds, or how long ago: 2h).
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
    },
    /// Environments: list, add, rename, delete, order, protect.
    #[command(subcommand, alias = "env")]
    Envs(Envs),
    /// The environment's changes, or one key's versions.
    History { key: Option<String> },
    /// Bring the environment back to a change (`--to 12`) or a time (`--at 2h`, RFC 3339).
    Rollback {
        #[arg(long)]
        to: Option<i64>,
        #[arg(long, conflicts_with = "to")]
        at: Option<String>,
    },
    /// Compare two environments' folder (values never shown).
    Diff {
        #[arg(long, requires = "to", conflicts_with = "file")]
        from: Option<String>,
        #[arg(long, requires = "from")]
        to: Option<String>,
        /// A local copy (`.env` or JSON) against this environment: the keys it adds, changes and
        /// lacks, by name (values compared on the server by fingerprint, never printed).
        #[arg(long, required_unless_present = "from")]
        file: Option<String>,
    },
    /// Copy keys (default: every key that differs) from one environment to another.
    Promote {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        keys: Vec<String>,
    },
    /// Rotate a linked secret's credential.
    Rotate { key: String },
    /// Change requests: list, approve, reject, apply, close.
    #[command(subcommand)]
    Requests(Requests),
    /// Scoped tokens for CI and services: list, create, revoke.
    #[command(subcommand)]
    Tokens(Tokens),
    /// Where a key is set, in every project you can read (`STRIPE_*` for a prefix). Never values.
    Find { key: String },
    /// A key pair for backups: the secret key written to a file only you can read, the public key
    /// printed (give it to `backup --to`).
    Keygen {
        /// Where the secret key goes (created 0600; never overwritten).
        #[arg(long, default_value = "roll-secrets-backup.key")]
        out: std::path::PathBuf,
    },
    /// Back the project up, sealed to a public key (`keygen`): every environment, folder, value,
    /// link, import, policy and required key. Written to stdout or --out.
    Backup {
        /// The recipient's public key (base64url X25519).
        #[arg(long)]
        to: String,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Load a backup into this project (which has no secrets yet), opened with the secret key.
    Restore {
        file: std::path::PathBuf,
        /// The secret key file (`keygen`).
        #[arg(long)]
        key: std::path::PathBuf,
    },
    /// The folders this folder imports (its own project's, or a folder another project shares
    /// with it): list, add, rm.
    #[command(subcommand)]
    Imports(ImportsCmd),
    /// The projects this folder is shared with (they can then import it): list, add, rm.
    #[command(subcommand)]
    Exports(ExportsCmd),
    /// Keys every standard environment must set, and what their values must be: list, add, rm.
    #[command(subcommand)]
    Required(RequiredCmd),
    /// Workload identities: CI jobs and clusters log in with their own OIDC token
    /// (`ROLL_IDENTITY`), no stored token: list, add, rm.
    #[command(subcommand)]
    Identities(Identities),
    /// Hand a value to someone outside roll: a link that opens once (or `--views`) and expires.
    /// The value is encrypted here; the key is only in the link.
    Share {
        /// A secret of the environment (default: the value on stdin).
        key: Option<String>,
        #[arg(long, default_value_t = 1)]
        views: i64,
        /// How long the link lives: 30m, 12h, 7d.
        #[arg(long, default_value = "24h")]
        ttl: String,
        /// What it is, for whoever opens it (never the value).
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Open a share's link (or `<id>#<key>`) and print its value; it counts as one of its opens.
    Open { link: String },
    /// Pushes of an environment to outside platforms (GitHub Actions, Vercel, Cloudflare, AWS
    /// Parameter Store, webhooks): list, add, run now, remove.
    #[command(subcommand)]
    Syncs(Syncs),
    /// Streams of the audit log to a SIEM: list, add, remove.
    #[command(subcommand)]
    Sinks(Sinks),
    /// Destroy the older values of a key for good (after a leak and a rotation).
    Purge {
        key: String,
        /// Only versions before this one (default: every version but the current one).
        #[arg(long)]
        before: Option<i64>,
    },
    /// How long old values are kept: a number of days, or `forever`.
    Retention { days: String },
    /// When each key was last read and changed, and which are stale or unused.
    Insights {
        /// Only keys not read in the last 90 days.
        #[arg(long)]
        unused: bool,
    },
    /// The project's one-time shares (`--revoke ID` to revoke one).
    Shares {
        #[arg(long)]
        revoke: Option<String>,
    },
    /// The leases of dynamic credentials (`--renew ID`, `--revoke ID`).
    Leases {
        #[arg(long, conflicts_with = "revoke")]
        renew: Option<String>,
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Who read and changed what (never values).
    Audit {
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Export instead: `csv` or `jsonl` (up to 10,000 rows).
        #[arg(long)]
        format: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Syncs {
    #[command(alias = "list")]
    Ls,
    /// Add a sync of this environment and folder: `add github --target repo=acme/web`.
    Add {
        /// The kind: github, gitlab, vercel, aws-secrets-manager, … (`syncs ls` lists them).
        kind: String,
        /// The target's fields as `name=value` (repeat), as `syncs ls` describes them.
        #[arg(long = "target", value_name = "NAME=VALUE")]
        target: Vec<String>,
        /// Only these keys (default: all of the folder's).
        #[arg(long, value_delimiter = ',')]
        keys: Vec<String>,
        /// Prepend this to every name at the target.
        #[arg(long)]
        prefix: Option<String>,
        /// Only say what it would push (names), without adding it.
        #[arg(long)]
        preview: bool,
    },
    /// Push it now, every key.
    Run { id: String },
    #[command(alias = "remove")]
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum Sinks {
    #[command(alias = "list")]
    Ls,
    /// Stream the audit log to an HTTPS URL (its signing secret is printed once).
    Add {
        url: String,
        /// Send the rows the project already has first.
        #[arg(long)]
        from_start: bool,
    },
    #[command(alias = "remove")]
    Rm { id: String },
}

#[derive(Subcommand)]
pub enum Envs {
    #[command(alias = "list")]
    Ls,
    /// Add an environment; with `--base`, an ephemeral one that inherits it.
    Add {
        slug: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// How long an ephemeral environment lives: 30m, 12h, 7d.
        #[arg(long)]
        ttl: Option<String>,
        /// `key=value` labels (ephemeral environments; delete by label with `rm --label`).
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    /// Rename an environment; references to it are rewritten.
    Rename {
        from: String,
        to: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete an environment (`--label k=v`: every ephemeral one with that label).
    Rm {
        slug: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Confirm deleting a protected environment.
        #[arg(long)]
        yes: bool,
    },
    /// Put the standard environments in this order.
    Order { slugs: Vec<String> },
    /// Writes need a change request with this many approvals.
    Protect {
        slug: String,
        #[arg(long, default_value_t = 1)]
        approvals: i64,
    },
    Unprotect { slug: String },
}

#[derive(Subcommand)]
pub enum Requests {
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        state: Option<String>,
    },
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Apply an approved request (`--bypass`: without approvals, as an admin; audited).
    Apply {
        id: String,
        #[arg(long)]
        bypass: bool,
    },
    Close { id: String },
}

#[derive(Subcommand)]
pub enum Tokens {
    #[command(alias = "list")]
    Ls,
    /// Create a token; it's printed once.
    Create {
        name: String,
        /// Environments it reaches (default: all).
        #[arg(long = "for-env")]
        envs: Vec<String>,
        /// Folders it reaches (default: all).
        #[arg(long = "for-path")]
        paths: Vec<String>,
        /// read, reveal, write, delete, admin (default: read and reveal).
        #[arg(long = "action")]
        actions: Vec<String>,
        /// Days until it expires.
        #[arg(long)]
        ttl_days: Option<i64>,
        /// Only from this address or CIDR (repeatable).
        #[arg(long = "allow-ip")]
        allow_ips: Vec<String>,
    },
    Revoke { id: String },
}

#[derive(Subcommand)]
pub enum ImportsCmd {
    #[command(alias = "list")]
    Ls,
    /// Import `ENV` or `ENV:/folder` (after this folder's own keys, before its base).
    Add {
        from: String,
        /// A folder another project shares with this one (`roll secrets exports add` there).
        #[arg(long)]
        from_project: Option<String>,
    },
    Rm {
        from: String,
        #[arg(long)]
        from_project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ExportsCmd {
    #[command(alias = "list")]
    Ls,
    /// Share this folder with a project (an id); its admin then imports it.
    Add {
        #[arg(value_name = "PROJECT")]
        to: String,
    },
    Rm {
        #[arg(value_name = "PROJECT")]
        to: String,
    },
}

#[derive(Subcommand)]
pub enum RequiredCmd {
    #[command(alias = "list")]
    Ls,
    /// Require a key (in a folder), optionally typed: `add PORT --type port`.
    Add {
        key: String,
        #[arg(long, default_value = "/")]
        path: String,
        /// url, int, port, bool, json or email.
        #[arg(long = "type")]
        kind: Option<String>,
        /// A regular expression the whole value must match.
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long, default_value = "")]
        comment: String,
        /// Who supplies it (a person or team): health and the daily notice name them.
        #[arg(long)]
        owner: Option<String>,
    },
    Rm {
        key: String,
        #[arg(long, default_value = "/")]
        path: String,
    },
}

// Parsed once per run.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum Identities {
    #[command(alias = "list")]
    Ls,
    /// Let workloads whose OIDC tokens match log in. For GitHub Actions:
    /// `add deploy --github acme/web --claim ref=refs/heads/main --for-env production`.
    Add {
        name: String,
        /// A GitHub repo (`owner/name`): GitHub Actions' issuer, and every workflow of the repo
        /// (narrow with `--claim`).
        #[arg(long, conflicts_with_all = ["issuer", "subject"])]
        github: Option<String>,
        /// The token's issuer (`iss`), for any other OIDC provider.
        #[arg(long, requires = "subject")]
        issuer: Option<String>,
        /// The token's `sub`, or a pattern of it with `*`.
        #[arg(long)]
        subject: Option<String>,
        /// The token's audience (`aud`; default: this server's URL, which the CLI also asks GitHub
        /// Actions for).
        #[arg(long)]
        audience: Option<String>,
        /// Another claim that must match, `name=value` (`*` for any run of characters).
        #[arg(long = "claim")]
        claims: Vec<String>,
        #[arg(long = "for-env")]
        envs: Vec<String>,
        #[arg(long = "for-path")]
        paths: Vec<String>,
        /// read, reveal, write, delete, admin (default: read and reveal).
        #[arg(long = "action")]
        actions: Vec<String>,
        /// How long a login's token lasts, 5–60 minutes (default 15).
        #[arg(long)]
        ttl_minutes: Option<i64>,
        /// Only log in (and use the token) from this address or CIDR (repeatable).
        #[arg(long = "allow-ip")]
        allow_ips: Vec<String>,
    },
    /// Delete an identity; the tokens it gave out stop working.
    Rm { id: String },
}

/// `roll run`: a command with an environment's secrets in its environment.
#[derive(Args)]
pub struct Run {
    #[command(flatten)]
    pub at: AtArgs,
    /// Run even if a reference doesn't resolve (it keeps its `${…}` text).
    #[arg(long)]
    pub allow_unresolved: bool,
    /// Replace variables already set in this shell (default: the shell's value wins).
    #[arg(long = "override")]
    pub override_env: bool,
    /// Restart the command when the environment changes (checked every `--interval` seconds).
    #[arg(long)]
    pub watch: bool,
    #[arg(long, default_value_t = 5, requires = "watch")]
    pub interval: u64,
    /// The command and its arguments.
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// A client for one project's secrets.
struct Client<'a> {
    api: &'a Api,
    project: String,
    at: At,
}

impl Client<'_> {
    fn base(&self) -> String {
        format!("/v1/secrets/projects/{}", enc(&self.project))
    }

    fn env(&self, env: &str) -> String {
        format!("{}/environments/{}", self.base(), enc(env))
    }

    fn q(&self) -> String {
        format!("path={}", enc(&self.at.path))
    }

    async fn call(&self, m: Method, p: &str, b: Option<Value>) -> anyhow::Result<Value> {
        self.api.call(m, p, b).await
    }
}

/// The project's id: `prj_…` as given, a name looked up, or the token's own project.
async fn project(api: &Api, given: Option<&str>) -> anyhow::Result<String> {
    match given {
        Some(p) if p.starts_with("prj_") => Ok(p.to_string()),
        Some(name) => {
            let list = api.get("/v1/workspace/projects").await.context("finding the project by name (pass its prj_… id with a scoped token)")?;
            list.as_array()
                .into_iter()
                .flatten()
                .find(|p| p["name"] == name)
                .and_then(|p| p["id"].as_str().map(String::from))
                .with_context(|| format!("no project named {name}"))
        }
        None => {
            let who = api.get("/v1/secrets/whoami").await?;
            who["project"].as_str().map(String::from).context("which project? pass -p <name> or set ROLL_PROJECT")
        }
    }
}

fn masked(v: &str) -> String {
    match v.chars().count() {
        0 => "(empty)".into(),
        _ => "••••••••".into(),
    }
}

fn split_pair(p: &str) -> anyhow::Result<(String, Option<String>)> {
    match p.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), Some(v.to_string()))),
        None => Ok((p.to_string(), None)),
    }
}

/// `30m`, `12h`, `7d` in minutes.
fn minutes(s: &str) -> anyhow::Result<i64> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().with_context(|| format!("{s}: a duration like 30m, 12h or 7d"))?;
    Ok(match unit {
        "m" | "" => n,
        "h" => n * 60,
        "d" => n * 1440,
        _ => bail!("{s}: a duration like 30m, 12h or 7d"),
    })
}

/// A time: RFC 3339, unix seconds, or how long ago (`2h`).
fn when(s: &str) -> anyhow::Result<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(secs) = s.parse::<i64>() {
        return Ok(secs * 1000);
    }
    Ok(chrono::Utc::now().timestamp_millis() - minutes(s)? * 60_000)
}

/// An expiry: a date (its start, UTC), RFC 3339, how long from now (`90d`), or `never` (None).
fn expiry(s: &str) -> anyhow::Result<Option<i64>> {
    if s == "never" || s == "none" {
        return Ok(None);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis()));
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(Some(t.timestamp_millis()));
    }
    let m = minutes(s).map_err(|_| anyhow::anyhow!("{s}: a date (2027-03-31), RFC 3339, how long from now (90d), or never"))?;
    Ok(Some(chrono::Utc::now().timestamp_millis() + m * 60_000))
}

/// 2027-03-31
fn day(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn approval_hint(e: anyhow::Error) -> anyhow::Error {
    if e.to_string().contains("is protected") {
        return e.context("the environment is protected: add --request to open a change request");
    }
    e
}

pub async fn run(api: &Api, json_out: bool, args: AtArgs, cmd: Secrets) -> anyhow::Result<()> {
    if let Secrets::Setup { show, clear } = cmd {
        return setup_dir(api, &args, show, clear).await;
    }
    // Opening a link needs no project: the link names its share.
    if let Secrets::Open { link } = &cmd {
        return open_share(api, link).await;
    }
    // Health across projects spans them too.
    if let Secrets::Doctor = &cmd {
        return doctor(api, &args).await;
    }
    if let Secrets::Health { all: true, .. } = &cmd {
        let v = api.get("/v1/secrets/findings").await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        let all: Vec<Value> = v.as_array().into_iter().flatten().flat_map(|p| p["findings"].as_array().cloned().unwrap_or_default().into_iter().map(move |mut f| {
            f["project_name"] = p["project_name"].clone();
            f
        })).collect();
        return report_health(&all, true);
    }
    // Finding a key spans projects.
    if let Secrets::Find { key } = &cmd {
        let v = api.get(&format!("/v1/secrets/search?key={}", enc(key))).await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
        let rows = v.as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("{key} isn't set anywhere you can read");
        }
        for p in rows {
            println!("{:<20} {:<14} {:<16} {:<28} v{:<4} {}{}", s(&p["project_name"]), s(&p["environment"]), s(&p["path"]), s(&p["key"]), p["version"], ago(p["updated_at"].as_i64().unwrap_or(0)), if p["linked"] == true { "  linked" } else { "" });
        }
        return Ok(());
    }
    // A key pair is this machine's, not a project's.
    if let Secrets::Keygen { out } = &cmd {
        let (secret, public) = crate::seal::keypair();
        write_private(out, &secret)?;
        println!("{public}");
        eprintln!("The secret key is in {} (0600): keep it apart from the backups. The public key above goes to backup --to.", out.display());
        return Ok(());
    }
    let at = args.resolve();
    let c = Client { project: project(api, at.project.as_deref()).await?, api, at };
    let env = c.env(&c.at.env);
    let print = |v: &Value| -> anyhow::Result<()> {
        println!("{}", serde_json::to_string_pretty(v)?);
        Ok(())
    };
    match cmd {
        Secrets::Setup { .. } => unreachable!("handled above"),
        Secrets::Ls { reveal } => {
            let list = c.call(Method::GET, &format!("{env}/secrets?{}", c.q()), None).await?;
            let values = if reveal { c.call(Method::GET, &format!("{env}/values?{}", c.q()), None).await?["values"].take() } else { Value::Null };
            if json_out {
                return print(&json!({ "secrets": list, "values": values }));
            }
            let rows = list.as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("No secrets in {}:{} yet. Add one: roll secrets set KEY=value -e {}", c.at.env, c.at.path, c.at.env);
                return Ok(());
            }
            let width = rows.iter().map(|r| r["key"].as_str().unwrap_or_default().len()).max().unwrap_or(0);
            for r in rows {
                let key = r["key"].as_str().unwrap_or_default();
                let value = match values.get(key).and_then(Value::as_str) {
                    Some(v) => v.replace('\n', "⏎"),
                    None if r["link"].is_object() => format!("→ {} {} {} ({})", r["link"]["kind"].as_str().unwrap_or(""), r["link"]["ref"].as_str().unwrap_or(""), r["link"]["field"].as_str().unwrap_or(""), r["link"]["mode"].as_str().unwrap_or("")),
                    None => masked("x"),
                };
                let mut notes = vec![];
                match r["source"]["kind"].as_str() {
                    Some("import") => notes.push(format!("from {}:{}", s(&r["source"]["environment"]), s(&r["source"]["path"]))),
                    Some("base") => notes.push(format!("from {}", s(&r["source"]["environment"]))),
                    _ => {}
                }
                if r["type"] == "personal" {
                    notes.push("personal".into());
                }
                if let Some(refs) = r["references"].as_array().filter(|a| !a.is_empty()) {
                    notes.push(format!("refs {}", refs.iter().map(s).collect::<Vec<_>>().join(", ")));
                }
                let notes = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join("; ")) };
                println!("{key:width$}  {value}  v{} {}{notes}", r["version"], ago(r["updated_at"].as_i64().unwrap_or(0)));
            }
        }
        Secrets::Get { key, raw } => {
            let v = c.call(Method::GET, &format!("{env}/secrets/{}/value?{}&resolve={}", enc(&key), c.q(), !raw), None).await?;
            if json_out {
                return print(&v);
            }
            for p in v["problems"].as_array().into_iter().flatten() {
                eprintln!("warning: {}", s(&p["problem"]));
            }
            println!("{}", s(&v["value"]));
        }
        Secrets::Set { pairs, comment, tags, personal, generate, request, message, expires } => {
            let expires = expires.as_deref().map(expiry).transpose()?;
            let mut ops = vec![];
            for p in &pairs {
                let (key, value) = split_pair(p)?;
                let value = match (value, generate) {
                    (Some(_), Some(_)) => bail!("{key}: --generate makes the value; give only the key"),
                    (None, Some(n)) => random_value(n)?,
                    (Some(v), None) => v,
                    (None, None) if pairs.len() == 1 => {
                        let mut v = String::new();
                        std::io::stdin().read_to_string(&mut v).context("reading the value from stdin")?;
                        v.strip_suffix('\n').map(String::from).unwrap_or(v)
                    }
                    (None, None) => bail!("{key}: KEY=VALUE (or one KEY with its value on stdin)"),
                };
                let mut op = json!({ "op": "set", "key": key, "value": value });
                if let Some(c) = &comment {
                    op["comment"] = json!(c);
                }
                if !tags.is_empty() {
                    op["tags"] = json!(tags);
                }
                if personal {
                    op["type"] = json!("personal");
                }
                if let Some(e) = expires {
                    op["expires_at"] = json!(e);
                }
                ops.push(op);
            }
            write(&c, &env, ops, message, request, json_out).await?;
        }
        Secrets::Expire { keys, on, request } => {
            let at = expiry(&on)?;
            // A set without a value would make a missing key, empty: only keys already here.
            let list = c.call(Method::GET, &format!("{env}/secrets?{}", c.q()), None).await?;
            let here: Vec<&str> = list.as_array().into_iter().flatten().filter(|x| x["source"]["kind"] == "own").filter_map(|x| x["key"].as_str()).collect();
            if let Some(k) = keys.iter().find(|k| !here.contains(&k.as_str())) {
                bail!("{k} isn't set in {}:{} (an imported or inherited key is dated where it's set)", c.at.env, c.at.path);
            }
            let ops = keys.iter().map(|k| json!({ "op": "set", "key": k, "expires_at": at })).collect();
            write(&c, &env, ops, None, request, json_out).await?;
        }
        Secrets::Rm { keys, request } => {
            let ops = keys.iter().map(|k| json!({ "op": "delete", "key": k })).collect();
            write(&c, &env, ops, None, request, json_out).await?;
        }
        Secrets::Edit { request } => edit(&c, &env, request, json_out).await?,
        Secrets::Import { file, from, source, map, recursive, overwrite, request } => {
            if recursive {
                anyhow::ensure!(from.as_deref() == Some("vault") && map.is_empty(), "--recursive reads a Vault KV v2 tree: --from vault --source mount/path");
                let tree = vault_tree(source.as_deref().unwrap_or_default()).await?;
                anyhow::ensure!(!tree.is_empty(), "nothing to import under {}", source.as_deref().unwrap_or_default());
                for (folder, values) in &tree {
                    let path = format!("{}/{}", c.at.path.trim_end_matches('/'), folder).replace("//", "/");
                    let path = if path.len() > 1 { path.trim_end_matches('/').to_string() } else { path };
                    let body = import_body(&serde_json::to_string(values)?, &path, overwrite, request)?;
                    let v = c.call(Method::POST, &format!("{env}/import"), Some(body)).await.map_err(approval_hint).with_context(|| format!("importing into {path}"))?;
                    let skipped = v["skipped"].as_array().map_or(0, Vec::len);
                    match v.get("request").filter(|r| r.is_object()) {
                        Some(r) => println!("{path}: change request {} ({} values)", s(&r["id"]), values.len()),
                        None => println!("{path}: {} value{}{}", values.len(), if values.len() == 1 { "" } else { "s" }, if skipped > 0 { format!(", {skipped} existing skipped (use --overwrite)") } else { String::new() }),
                    }
                }
                return Ok(());
            }
            if let (Some(from), false) = (&from, map.is_empty()) {
                let base = source.as_deref().unwrap_or_default().trim_end_matches('/');
                for pair in &map {
                    let (src, dst) = pair.split_once('=').with_context(|| format!("{pair}: --map is source=environment"))?;
                    let values = import_from(from, &format!("{base}/{src}")).await.with_context(|| format!("reading {base}/{src}"))?;
                    let body = import_body(&serde_json::to_string(&values)?, &c.at.path, overwrite, request)?;
                    let v = c.call(Method::POST, &format!("{}/import", c.env(dst)), Some(body)).await.map_err(approval_hint).with_context(|| format!("importing into {dst}"))?;
                    let skipped = v["skipped"].as_array().map_or(0, Vec::len);
                    match v.get("request").filter(|r| r.is_object()) {
                        Some(r) => println!("{src} → {dst}: change request {} ({} values)", s(&r["id"]), values.len()),
                        None => println!("{src} → {dst}: {} value{}{}", values.len(), if values.len() == 1 { "" } else { "s" }, if skipped > 0 { format!(", {skipped} existing skipped (use --overwrite)") } else { String::new() }),
                    }
                }
                return Ok(());
            }
            let text = match (file.as_deref(), from) {
                (_, Some(from)) => serde_json::to_string(&import_from(&from, source.as_deref().unwrap_or_default()).await?)?,
                (Some("-"), None) => {
                    let mut t = String::new();
                    std::io::stdin().read_to_string(&mut t)?;
                    t
                }
                (Some(f), None) => std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?,
                (None, None) => bail!("a file, or --from another manager"),
            };
            let v = c
                .call(Method::POST, &format!("{env}/import"), Some(import_body(&text, &c.at.path, overwrite, request)?))
                .await
                .map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            if let Some(r) = v.get("request").filter(|r| r.is_object()) {
                println!("Opened change request {} on {}", s(&r["id"]), c.at.env);
            } else {
                summary(&v);
            }
            if let Some(sk) = v["skipped"].as_array().filter(|a| !a.is_empty()) {
                println!("Skipped {} existing (use --overwrite): {}", sk.len(), sk.iter().map(s).collect::<Vec<_>>().join(", "));
            }
        }
        Secrets::Render { file, out, watch, interval, exec } => {
            let tpl = if file == "-" {
                let mut t = String::new();
                std::io::stdin().read_to_string(&mut t)?;
                t
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let (text, leases) = render_env(&c, &env, &file, &tpl).await?;
            let Some(out) = out else {
                print!("{text}");
                return Ok(());
            };
            replace_private(&out, &text)?;
            if watch {
                render_watch(&c, &env, &file, &tpl, &out, text, leases, interval, exec.as_deref()).await?;
            }
        }
        Secrets::Health { all: false, code } => {
            let v = c.call(Method::GET, &format!("{}/health{}", c.base(), if code { "?code=1" } else { "" }), None).await?;
            if json_out {
                return print(&v);
            }
            report_health(&v.as_array().cloned().unwrap_or_default(), false)?;
        }
        Secrets::Check { template, strict } => {
            let text = std::fs::read_to_string(&template).with_context(|| format!("reading {template}"))?;
            let wanted: std::collections::BTreeSet<String> = template_keys(&text);
            let list = c.call(Method::GET, &format!("{env}/secrets?{}", c.q()), None).await?;
            let have: std::collections::BTreeSet<String> = list.as_array().into_iter().flatten().filter_map(|x| x["key"].as_str().map(String::from)).collect();
            let missing: Vec<&String> = wanted.difference(&have).collect();
            let extra: Vec<&String> = have.difference(&wanted).collect();
            if json_out {
                print(&json!({ "missing": missing, "extra": extra }))?;
            } else {
                let at = format!("{}:{}", c.at.env, c.at.path);
                for k in &missing {
                    println!("missing  {k}  ({template} names it; {at} doesn't set it)");
                }
                for k in &extra {
                    println!("extra    {k}  ({at} sets it; {template} doesn't name it)");
                }
                if missing.is_empty() && (extra.is_empty() || !strict) {
                    eprintln!("{at} has every key {template} names ({})", wanted.len());
                }
            }
            if !missing.is_empty() || (strict && !extra.is_empty()) {
                bail!("{} doesn't match {template}", c.at.env);
            }
        }
        Secrets::Scan { paths, staged } => {
            let files = if staged { staged_files()? } else { walk(if paths.is_empty() { vec![".".into()] } else { paths })? };
            let mut by: std::collections::BTreeMap<String, Vec<(String, usize)>> = Default::default();
            for (name, text) in &files {
                for (n, line) in text.lines().enumerate() {
                    let mut found = std::collections::HashSet::new();
                    line_candidates(line, &mut found);
                    for c in found {
                        by.entry(c).or_default().push((name.clone(), n + 1));
                    }
                }
            }
            let all: Vec<String> = by.keys().cloned().collect();
            let mut hits: Vec<(String, usize, String, String, String)> = vec![];
            for chunk in all.chunks(20_000) {
                let v = c.call(Method::POST, &format!("{}/leaks", c.base()), Some(json!({ "candidates": chunk }))).await?;
                for f in v.as_array().into_iter().flatten() {
                    let Some(cand) = f["index"].as_u64().and_then(|i| chunk.get(i as usize)) else { continue };
                    for (file, line) in &by[cand] {
                        hits.push((file.clone(), *line, s(&f["key"]), s(&f["environment"]), s(&f["path"])));
                    }
                }
            }
            hits.sort();
            hits.dedup();
            if json_out {
                return print(&json!(hits.iter().map(|h| json!({ "file": h.0, "line": h.1, "key": h.2, "environment": h.3, "path": h.4 })).collect::<Vec<_>>()));
            }
            for (file, line, key, env, path) in &hits {
                println!("{file}:{line}  {key} ({env}{})", if path == "/" { String::new() } else { format!(" {path}") });
            }
            if !hits.is_empty() {
                let files = hits.iter().map(|h| &h.0).collect::<std::collections::BTreeSet<_>>().len();
                let (what, it) = if hits.len() == 1 { ("a value of this project".to_string(), "it") } else { (format!("{} values of this project", hits.len()), "them") };
                bail!("{what} in {files} {}: move {it} out of the code (roll run puts values in the environment)", if files == 1 { "file" } else { "files" });
            }
            eprintln!("No values of this project in {} {}", files.len(), if files.len() == 1 { "file" } else { "files" });
        }
        Secrets::Export { format, name, namespace, change, at } => {
            let at = at.as_deref().map(when).transpose()?;
            let mut url = format!("{}{env}/values?{}&format={}", c.api.base, c.q(), enc(&format));
            for (k, v) in [("name", name), ("namespace", namespace), ("seq", change.map(|c| c.to_string())), ("at", at.map(|t| t.to_string()))] {
                if let Some(v) = v {
                    url.push_str(&format!("&{k}={}", enc(&v)));
                }
            }
            let mut req = c.api.http.get(url);
            if let Some(k) = &c.api.key {
                req = req.bearer_auth(k);
            }
            let res = req.send().await?;
            let status = res.status();
            let text = res.text().await?;
            if !status.is_success() {
                let e: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                bail!("{}", e["error"].as_str().unwrap_or(status.as_str()));
            }
            print!("{text}");
        }
        Secrets::Envs(e) => envs(&c, e, json_out).await?,
        Secrets::History { key } => {
            let v = match &key {
                Some(k) => c.call(Method::GET, &format!("{env}/secrets/{}/versions?{}", enc(k), c.q()), None).await?,
                None => c.call(Method::GET, &format!("{env}/changes"), None).await?,
            };
            if json_out {
                return print(&v);
            }
            for r in v.as_array().into_iter().flatten() {
                match &key {
                    Some(_) => println!("v{:<4} #{:<5} {:<8} {:<20} {} {}", r["version"], r["seq"], s(&r["op"]), r["what"].as_array().map(|w| w.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_default(), s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0))),
                    None => println!("#{:<5} {:<9} {:>3} keys  {:<12} {}  {}", r["seq"], s(&r["kind"]), r["keys"], s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0)), s(&r["message"])),
                }
            }
        }
        Secrets::Rollback { to, at } => {
            let body = match (to, at) {
                (Some(t), _) => json!({ "to": t }),
                (None, Some(a)) => json!({ "at": when(&a)? }),
                _ => bail!("rollback --to <change> or --at <time>"),
            };
            let v = c.call(Method::POST, &format!("{env}/rollback"), Some(body)).await.map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            summary(&v);
        }
        Secrets::Diff { file: Some(file), .. } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?;
            let v = c.call(Method::POST, &format!("{env}/diff"), Some(import_body(&text, &c.at.path, false, false)?)).await?;
            if json_out {
                return print(&v);
            }
            let list = |k: &str| v[k].as_array().into_iter().flatten().map(s).collect::<Vec<_>>();
            for (k, mark, what) in [("added", "+", "only in the file"), ("changed", "~", "different here"), ("missing", "-", "not in the file"), ("references", "?", "has references: not compared"), ("linked", "?", "linked: not compared")] {
                for name in list(k) {
                    println!("{mark} {name:<32} {what}");
                }
            }
            let differs = list("added").len() + list("changed").len() + list("missing").len();
            if differs == 0 {
                eprintln!("{file} matches {}:{} ({} same)", c.at.env, c.at.path, list("same").len());
            } else {
                bail!("{file} differs from {}:{} in {differs} key(s)", c.at.env, c.at.path);
            }
        }
        Secrets::Diff { from, to, .. } => {
            let (from, to) = (from.unwrap_or_default(), to.unwrap_or_default());
            let v = c.call(Method::GET, &format!("{}/compare?from={}&to={}&{}", c.base(), enc(&from), enc(&to), c.q()), None).await?;
            if json_out {
                return print(&v);
            }
            for r in v["rows"].as_array().into_iter().flatten() {
                let mark = match r["status"].as_str() {
                    Some("same") => " ",
                    Some("different") => "~",
                    Some("only_from") => "+",
                    _ => "-",
                };
                println!("{mark} {}", s(&r["key"]));
            }
        }
        Secrets::Promote { from, to, keys } => {
            let body = if keys.is_empty() { json!({ "path": c.at.path }) } else { json!({ "path": c.at.path, "keys": keys }) };
            let v = c.call(Method::POST, &format!("{}/promote?from={}", c.env(&to), enc(&from)), Some(body)).await.map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            summary(&v);
        }
        Secrets::Rotate { key } => {
            let v = c.call(Method::POST, &format!("{env}/secrets/{}/rotate?{}", enc(&key), c.q()), None).await?;
            if json_out {
                return print(&v);
            }
            println!("Rotated {key}");
        }
        Secrets::Requests(r) => {
            let (path, body) = match &r {
                Requests::Ls { state } => {
                    let v = c.call(Method::GET, &format!("{env}/requests{}", state.as_ref().map(|s| format!("?state={}", enc(s))).unwrap_or_default()), None).await?;
                    if json_out {
                        return print(&v);
                    }
                    for r in v.as_array().into_iter().flatten() {
                        println!("{}  {:<9} {}/{} approvals  {:<12} {}  {}", s(&r["id"]), s(&r["state"]), r["approvals"], r["approvals_needed"], s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0)), s(&r["message"]));
                    }
                    return Ok(());
                }
                Requests::Approve { id, comment } => (format!("{id}/review"), json!({ "verdict": "approve", "comment": comment })),
                Requests::Reject { id, comment } => (format!("{id}/review"), json!({ "verdict": "reject", "comment": comment })),
                Requests::Apply { id, bypass } => (format!("{id}/apply"), json!({ "bypass": bypass })),
                Requests::Close { id } => (format!("{id}/close"), json!({})),
            };
            let v = c.call(Method::POST, &format!("{env}/requests/{}", path.split('/').map(enc).collect::<Vec<_>>().join("/")), Some(body)).await?;
            if json_out {
                return print(&v);
            }
            println!("{} is {}", s(&v["id"]), s(&v["state"]));
        }
        Secrets::Tokens(t) => match t {
            Tokens::Ls => {
                let v = c.call(Method::GET, &format!("{}/tokens", c.base()), None).await?;
                if json_out {
                    return print(&v);
                }
                for t in v.as_array().into_iter().flatten() {
                    let list = |x: &Value| x.as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_else(|| "all".into());
                    println!("{}  {:<20} envs {}  paths {}  {}  used {}", s(&t["id"]), s(&t["name"]), list(&t["environments"]), list(&t["paths"]), list(&t["actions"]), t["last_used_at"].as_i64().map(ago).unwrap_or_else(|| "never".into()));
                }
            }
            Tokens::Create { name, envs, paths, actions, ttl_days, allow_ips } => {
                let actions = if actions.is_empty() { vec!["read".to_string(), "reveal".to_string()] } else { actions };
                let mut body = json!({ "name": name, "actions": actions });
                if !envs.is_empty() {
                    body["environments"] = json!(envs);
                }
                if !paths.is_empty() {
                    body["paths"] = json!(paths);
                }
                if let Some(d) = ttl_days {
                    body["ttl_days"] = json!(d);
                }
                if !allow_ips.is_empty() {
                    body["allowed_ips"] = json!(allow_ips);
                }
                let v = c.call(Method::POST, &format!("{}/tokens", c.base()), Some(body)).await?;
                if json_out {
                    return print(&v);
                }
                println!("{}", s(&v["token"]));
                eprintln!("Created {} ({}). This is the only time it's shown: use it as ROLL_TOKEN.", s(&v["name"]), s(&v["id"]));
            }
            Tokens::Revoke { id } => {
                c.call(Method::DELETE, &format!("{}/tokens/{}", c.base(), enc(&id)), None).await?;
                println!("Revoked {id}");
            }
        },
        Secrets::Keygen { .. } | Secrets::Find { .. } | Secrets::Doctor | Secrets::Health { all: true, .. } => unreachable!("handled before a project is picked"),
        Secrets::Backup { to, out } => {
            let v = c.call(Method::POST, &format!("{}/backup", c.base()), Some(json!({ "recipient": to }))).await?;
            let text = serde_json::to_string_pretty(&v)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, text)?;
                    eprintln!("Backed up {} to {}: sealed, only the secret key opens it.", c.project, path.display());
                }
                None => println!("{text}"),
            }
        }
        Secrets::Restore { file, key } => {
            let secret = std::fs::read_to_string(&key).with_context(|| format!("reading {}", key.display()))?;
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?)?;
            let plain = crate::seal::open(&secret, v["context"].as_str().unwrap_or_default(), &v["sealed"])?;
            let archive: Value = serde_json::from_slice(&plain)?;
            let r = c.call(Method::POST, &format!("{}/restore", c.base()), Some(archive)).await?;
            if json_out {
                return print(&r);
            }
            println!("Restored {} environments and {} keys into {}", r["environments"], r["secrets"], c.project);
            for w in r["warnings"].as_array().into_iter().flatten() {
                eprintln!("warning: {}", s(w));
            }
        }
        Secrets::Imports(cmd) => {
            let url = format!("{env}/imports?{}", c.q());
            let list = c.call(Method::GET, &url, None).await?;
            let mut rows: Vec<Value> = list.as_array().cloned().unwrap_or_default();
            let parse = |from: &str, project: &Option<String>| {
                let (e, p) = from.split_once(':').unwrap_or((from, "/"));
                let mut v = json!({ "environment": e, "path": p });
                if let Some(pr) = project {
                    v["project"] = json!(pr);
                }
                v
            };
            let same = |a: &Value, b: &Value| a["environment"] == b["environment"] && a["path"] == b["path"] && a.get("project") == b.get("project");
            match cmd {
                ImportsCmd::Ls => {
                    if json_out {
                        return print(&list);
                    }
                    for i in &rows {
                        let from = i["project"].as_str().map(|p| format!("{p}/")).unwrap_or_default();
                        println!("{from}{}:{}", s(&i["environment"]), s(&i["path"]));
                    }
                }
                ImportsCmd::Add { from, from_project } => {
                    let new = parse(&from, &from_project);
                    rows.retain(|x| !same(x, &new));
                    rows.push(new);
                    c.call(Method::PUT, &url, Some(Value::Array(rows))).await?;
                    println!("{}:{} imports {from}", c.at.env, c.at.path);
                }
                ImportsCmd::Rm { from, from_project } => {
                    let gone = parse(&from, &from_project);
                    let before = rows.len();
                    rows.retain(|x| !same(x, &gone));
                    anyhow::ensure!(rows.len() < before, "{}:{} doesn't import {from}", c.at.env, c.at.path);
                    c.call(Method::PUT, &url, Some(Value::Array(rows))).await?;
                    println!("{}:{} doesn't import {from} any more", c.at.env, c.at.path);
                }
            }
        }
        Secrets::Exports(cmd) => {
            let url = format!("{env}/exports?{}", c.q());
            let list = c.call(Method::GET, &url, None).await?;
            let mut to: Vec<String> = list.as_array().into_iter().flatten().filter_map(|e| e["project"].as_str().map(String::from)).collect();
            let body = |to: &[String]| Value::Array(to.iter().map(|p| json!({ "project": p })).collect());
            match cmd {
                ExportsCmd::Ls => {
                    if json_out {
                        return print(&list);
                    }
                    for e in list.as_array().into_iter().flatten() {
                        println!("{}  shared {} by {}", s(&e["project"]), ago(e["created_at"].as_i64().unwrap_or(0)), s(&e["created_by"]));
                    }
                }
                ExportsCmd::Add { to: project } => {
                    if !to.contains(&project) {
                        to.push(project.clone());
                    }
                    c.call(Method::PUT, &url, Some(body(&to))).await?;
                    println!("{}:{} is shared with {project}: import it there with --from-project {}", c.at.env, c.at.path, c.project);
                }
                ExportsCmd::Rm { to: project } => {
                    let before = to.len();
                    to.retain(|p| p != &project);
                    anyhow::ensure!(to.len() < before, "{}:{} isn't shared with {project}", c.at.env, c.at.path);
                    c.call(Method::PUT, &url, Some(body(&to))).await?;
                    println!("{}:{} isn't shared with {project} any more (its imports stop now)", c.at.env, c.at.path);
                }
            }
        }
        Secrets::Required(r) => {
            let url = format!("{}/required", c.base());
            let list = c.call(Method::GET, &url, None).await?;
            let mut rows: Vec<Value> = list.as_array().cloned().unwrap_or_default();
            let same = |x: &Value, key: &str, path: &str| x["key"] == key && x["path"] == path;
            let put = |rows: Vec<Value>| async move { c.call(Method::PUT, &format!("{}/required", c.base()), Some(Value::Array(rows))).await };
            match r {
                RequiredCmd::Ls => {
                    if json_out {
                        return print(&list);
                    }
                    for x in &rows {
                        let rule = [x["type"].as_str().map(|t| format!("type {t}")), x["pattern"].as_str().map(|p| format!("/{p}/"))].into_iter().flatten().collect::<Vec<_>>().join(" ");
                        println!("{:<28} {:<12} {:<28} {}{}", s(&x["key"]), s(&x["path"]), rule, x["comment"].as_str().unwrap_or_default(), x["owner"].as_str().map(|o| format!("  (owner {o})")).unwrap_or_default());
                    }
                }
                RequiredCmd::Add { key, path, kind, pattern, comment, owner } => {
                    rows.retain(|x| !same(x, &key, &path));
                    let mut row = json!({ "key": key, "path": path, "comment": comment });
                    if let Some(k) = kind {
                        row["type"] = json!(k);
                    }
                    if let Some(p) = pattern {
                        row["pattern"] = json!(p);
                    }
                    if let Some(o) = owner {
                        row["owner"] = json!(o);
                    }
                    rows.push(row);
                    put(rows).await?;
                    println!("Required {key}");
                }
                RequiredCmd::Rm { key, path } => {
                    let before = rows.len();
                    rows.retain(|x| !same(x, &key, &path));
                    anyhow::ensure!(rows.len() < before, "{key} isn't required in {path}");
                    put(rows).await?;
                    println!("{key} isn't required any more");
                }
            }
        }
        Secrets::Identities(i) => match i {
            Identities::Ls => {
                let v = c.call(Method::GET, &format!("{}/identities", c.base()), None).await?;
                if json_out {
                    return print(&v);
                }
                for i in v.as_array().into_iter().flatten() {
                    let list = |x: &Value| x.as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_else(|| "all".into());
                    let used = i["last_used_at"].as_i64().map(|at| format!("{} by {}", ago(at), s(&i["last_subject"]))).unwrap_or_else(|| "never".into());
                    println!("{}  {:<16} {}  {}  envs {}  {}  {} min  used {used}", s(&i["id"]), s(&i["name"]), s(&i["issuer"]), s(&i["subject"]), list(&i["environments"]), list(&i["actions"]), i["ttl_minutes"]);
                }
            }
            Identities::Add { name, github, issuer, subject, audience, claims, envs, paths, actions, ttl_minutes, allow_ips } => {
                let (issuer, subject) = match (github, issuer, subject) {
                    (Some(repo), _, _) => ("https://token.actions.githubusercontent.com".to_string(), format!("repo:{repo}:*")),
                    (None, Some(i), Some(s)) => (i, s),
                    _ => bail!("give --github owner/repo, or --issuer and --subject"),
                };
                let claims: serde_json::Map<String, Value> = claims
                    .iter()
                    .map(|c| c.split_once('=').map(|(k, v)| (k.trim().to_string(), json!(v.trim()))).with_context(|| format!("--claim {c}: name=value")))
                    .collect::<anyhow::Result<_>>()?;
                let actions = if actions.is_empty() { vec!["read".to_string(), "reveal".to_string()] } else { actions };
                let audience = audience.unwrap_or_else(|| c.api.base.clone());
                let mut body = json!({ "name": name, "issuer": issuer, "audience": audience, "subject": subject, "claims": claims, "actions": actions });
                if !envs.is_empty() {
                    body["environments"] = json!(envs);
                }
                if !paths.is_empty() {
                    body["paths"] = json!(paths);
                }
                if let Some(m) = ttl_minutes {
                    body["ttl_minutes"] = json!(m);
                }
                if !allow_ips.is_empty() {
                    body["allowed_ips"] = json!(allow_ips);
                }
                let v = c.call(Method::POST, &format!("{}/identities", c.base()), Some(body)).await?;
                if json_out {
                    return print(&v);
                }
                println!("{}", s(&v["id"]));
                eprintln!("Created {}. In the workload: ROLL_IDENTITY={} (and ROLL_API_URL), then `roll run …` logs in by itself.", s(&v["name"]), s(&v["id"]));
            }
            Identities::Rm { id } => {
                c.call(Method::DELETE, &format!("{}/identities/{}", c.base(), enc(&id)), None).await?;
                println!("Deleted {id}");
            }
        },
        Secrets::Share { key, views, ttl, note } => {
            let value = match &key {
                Some(k) => {
                    let v = c.call(Method::GET, &format!("{env}/secrets/{}/value?{}", enc(k), c.q()), None).await?;
                    s(&v["value"])
                }
                None => {
                    let mut v = String::new();
                    std::io::stdin().read_to_string(&mut v).context("reading the value from stdin")?;
                    v.strip_suffix('\n').map(String::from).unwrap_or(v)
                }
            };
            let (ciphertext, secret) = share_seal(&value);
            let note = if note.is_empty() { key.clone().unwrap_or_default() } else { note };
            let body = json!({ "ciphertext": ciphertext, "note": note, "views": views, "ttl_minutes": minutes(&ttl)? });
            let v = c.call(Method::POST, &format!("{}/shares", c.base()), Some(body)).await?;
            let id = s(&v["id"]);
            // The server's link (it knows the dashboard), else ROLL_WEB_URL here, else `<id>#<key>`
            // for `roll secrets open`.
            match v["link"].as_str().map(String::from).or_else(|| std::env::var("ROLL_WEB_URL").ok().filter(|u| !u.is_empty()).map(|w| format!("{}/share/{id}", w.trim_end_matches('/')))) {
                Some(link) => println!("{link}#{secret}"),
                None => println!("{id}#{secret}"),
            }
            eprintln!("Opens {views} time{}, until {}. The key is only in this link: send it, don't store it.", if views == 1 { "" } else { "s" }, chrono::DateTime::from_timestamp_millis(v["expires_at"].as_i64().unwrap_or(0)).map(|t| t.to_rfc3339()).unwrap_or_default());
        }
        Secrets::Open { .. } => unreachable!("handled above"),
        Secrets::Syncs(cmd) => syncs(&c, cmd, json_out).await?,
        Secrets::Sinks(cmd) => sinks(&c, cmd, json_out).await?,
        Secrets::Purge { key, before } => {
            let body = before.map(|b| json!({ "before_version": b })).unwrap_or_else(|| json!({}));
            let v = c.call(Method::POST, &format!("{env}/secrets/{}/purge?{}", enc(&key), c.q()), Some(body)).await?;
            if json_out {
                return print(&v);
            }
            println!("Destroyed {} older value{} of {key}", v["purged"], if v["purged"] == 1 { "" } else { "s" });
        }
        Secrets::Retention { days } => {
            let d = match days.as_str() {
                "forever" | "off" | "none" => Value::Null,
                n => json!(n.trim_end_matches('d').parse::<i64>().with_context(|| format!("{n}: a number of days, or forever"))?),
            };
            let v = c.call(Method::PATCH, &c.base(), Some(json!({ "history_days": d }))).await?;
            match v["history_days"].as_i64() {
                Some(n) => println!("Old values are destroyed after {n} days"),
                None => println!("Old values are kept"),
            }
        }
        Secrets::Insights { unused } => {
            let v = c.call(Method::GET, &format!("{env}/insights?{}", c.q()), None).await?;
            if json_out {
                return print(&v);
            }
            for k in v["keys"].as_array().into_iter().flatten() {
                if unused && k["last_read_at"].is_number() {
                    continue;
                }
                let read = k["last_read_at"].as_i64().map(|at| format!("read {} by {}", ago(at), s(&k["last_read_by"]))).unwrap_or_else(|| "not read in 90 days".into());
                let stale = if k["stale"] == true { "  STALE" } else { "" };
                let expiry = match k["expires_at"].as_i64() {
                    Some(t) if t <= chrono::Utc::now().timestamp_millis() => format!("  EXPIRED {}", day(t)),
                    Some(t) if k["expiring"] == true => format!("  EXPIRES {}", day(t)),
                    Some(t) => format!("  expires {}", day(t)),
                    None => String::new(),
                };
                let exposed: Vec<String> = k["exposed_in"].as_array().into_iter().flatten().map(s).collect();
                let exposed = if exposed.is_empty() { String::new() } else { format!("  ALSO IN {}", exposed.join(", ")) };
                let exposed = match k["weak"].as_str() {
                    Some(w) => format!("{exposed}  WEAK ({w})"),
                    None => exposed,
                };
                println!("{:<32} changed {:<8} {read} ({}x in 30d){stale}{expiry}{exposed}", s(&k["key"]), ago(k["updated_at"].as_i64().unwrap_or(0)), k["reads_30d"]);
            }
        }
        Secrets::Shares { revoke } => {
            if let Some(id) = revoke {
                c.call(Method::DELETE, &format!("{}/shares/{}", c.base(), enc(&id)), None).await?;
                println!("Revoked {id}");
                return Ok(());
            }
            let v = c.call(Method::GET, &format!("{}/shares", c.base()), None).await?;
            if json_out {
                return print(&v);
            }
            for sh in v.as_array().into_iter().flatten() {
                let state = if sh["open"] == true { format!("{} open{} left", sh["views_left"], if sh["views_left"] == 1 { "" } else { "s" }) } else { "closed".into() };
                println!("{}  {:<16} {:<22} by {}  {}", s(&sh["id"]), state, s(&sh["note"]), s(&sh["created_by"]), ago(sh["created_at"].as_i64().unwrap_or(0)));
            }
        }
        Secrets::Leases { renew, revoke } => {
            if let Some(id) = renew {
                let v = c.call(Method::POST, &format!("{}/leases/{}/renew", c.base(), enc(&id)), None).await?;
                println!("{id} now ends {}", chrono::DateTime::from_timestamp_millis(v["expires_at"].as_i64().unwrap_or(0)).map(|t| t.to_rfc3339()).unwrap_or_default());
                return Ok(());
            }
            if let Some(id) = revoke {
                c.call(Method::DELETE, &format!("{}/leases/{}", c.base(), enc(&id)), None).await?;
                println!("Revoked {id}");
                return Ok(());
            }
            let v = c.call(Method::GET, &format!("{}/leases", c.base()), None).await?;
            if json_out {
                return print(&v);
            }
            for l in v.as_array().into_iter().flatten() {
                let state = if l["revoked_at"].is_number() { "revoked".to_string() } else { format!("ends {}", chrono::DateTime::from_timestamp_millis(l["expires_at"].as_i64().unwrap_or(0)).map(|t| t.to_rfc3339()).unwrap_or_default()) };
                println!("{}  {:<8} {:<12} {:<24} {}  {state}", s(&l["id"]), s(&l["kind"]), s(&l["environment"]), s(&l["key"]), s(&l["consumer"]));
            }
        }
        Secrets::Audit { key, actor, limit, format: Some(f) } => {
            let mut q = format!("limit={limit}&format={}", enc(&f));
            if let Some(k) = &key {
                q.push_str(&format!("&key={}", enc(k)));
            }
            if let Some(a) = &actor {
                q.push_str(&format!("&actor={}", enc(a)));
            }
            print!("{}", text_of(c.api, &format!("{}/audit?{q}", c.base())).await?);
        }
        Secrets::Audit { key, actor, limit, format: None } => {
            let mut q = format!("limit={limit}");
            if let Some(k) = &key {
                q.push_str(&format!("&key={}", enc(k)));
            }
            if let Some(a) = &actor {
                q.push_str(&format!("&actor={}", enc(a)));
            }
            let v = c.call(Method::GET, &format!("{}/audit?{q}", c.base()), None).await?;
            if json_out {
                return print(&v);
            }
            for a in v.as_array().into_iter().flatten() {
                println!("{:<8} {:<16} {:<20} {:<12} {}", ago(a["at"].as_i64().unwrap_or(0)), s(&a["actor"]), s(&a["action"]), a["environment"].as_str().unwrap_or("-"), s(&a["summary"]));
            }
        }
    }
    Ok(())
}

fn s(v: &Value) -> String {
    crate::s(v)
}

/// How long ago a time in unix milliseconds was.
fn ago(ms: i64) -> String {
    crate::ago(ms / 1000)
}

fn summary(v: &Value) {
    match v["change"].as_object() {
        Some(ch) => {
            let items = v["items"].as_array().cloned().unwrap_or_default();
            println!("#{} {} ({})", ch["seq"], ch["environment"].as_str().unwrap_or(""), items.iter().map(|i| format!("{} {}", s(&i["what"][0]), s(&i["key"]))).collect::<Vec<_>>().join(", "));
        }
        None => println!("No changes"),
    }
    for w in v["warnings"].as_array().into_iter().flatten() {
        eprintln!("warning: {}", s(w));
    }
}

async fn write(c: &Client<'_>, env: &str, ops: Vec<Value>, message: Option<String>, request: bool, json_out: bool) -> anyhow::Result<()> {
    let body = json!({ "path": c.at.path, "message": message, "ops": ops });
    if request {
        let v = c.call(Method::POST, &format!("{env}/requests"), Some(body)).await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("Opened change request {} on {}: it needs {} approval(s)", s(&v["id"]), c.at.env, v["approvals_needed"]);
        }
        return Ok(());
    }
    let v = c.call(Method::POST, &format!("{env}/changes"), Some(body)).await.map_err(approval_hint)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        summary(&v);
    }
    Ok(())
}

async fn envs(c: &Client<'_>, e: Envs, json_out: bool) -> anyhow::Result<()> {
    let base = c.base();
    let v = match e {
        Envs::Ls => {
            let v = c.call(Method::GET, &format!("{base}/environments"), None).await?;
            if !json_out {
                for e in v.as_array().into_iter().flatten() {
                    let mut notes = vec![];
                    if e["protected"] == true {
                        notes.push(format!("protected, {} approval(s)", e["approvals"]));
                    }
                    if e["kind"] == "ephemeral" {
                        notes.push(format!("ephemeral from {}", s(&e["base"])));
                        if let Some(at) = e["expires_at"].as_i64() {
                            notes.push(format!("expires {}", chrono::DateTime::from_timestamp_millis(at).map(|t| t.to_rfc3339()).unwrap_or_default()));
                        }
                    }
                    println!("{:<20} {:<24} {:>4} secrets  {}", s(&e["slug"]), s(&e["name"]), e["secrets"], notes.join(", "));
                }
                return Ok(());
            }
            v
        }
        Envs::Add { slug, name, base: from, ttl, labels } => {
            let mut body = json!({ "slug": slug });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(b) = from {
                body["base"] = json!(b);
            }
            if let Some(t) = ttl {
                body["ttl_minutes"] = json!(minutes(&t)?);
            }
            if !labels.is_empty() {
                let mut l = serde_json::Map::new();
                for kv in labels {
                    let (k, v) = kv.split_once('=').with_context(|| format!("{kv}: a label is key=value"))?;
                    l.insert(k.into(), json!(v));
                }
                body["labels"] = Value::Object(l);
            }
            c.call(Method::POST, &format!("{base}/environments"), Some(body)).await?
        }
        Envs::Rename { from, to, name } => {
            let mut body = json!({ "slug": to });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            c.call(Method::PATCH, &c.env(&from), Some(body)).await?
        }
        Envs::Rm { slug, label, yes } => match (slug, label) {
            (_, Some(l)) => c.call(Method::DELETE, &format!("{base}/environments?label={}", enc(&l)), None).await?,
            (Some(sl), None) => {
                let q = if yes { format!("?confirm={}", enc(&sl)) } else { String::new() };
                c.call(Method::DELETE, &format!("{}{q}", c.env(&sl)), None).await?;
                json!({ "deleted": [sl] })
            }
            (None, None) => bail!("envs rm <slug> or --label key=value"),
        },
        Envs::Order { slugs } => c.call(Method::PUT, &format!("{base}/environments/order"), Some(json!({ "order": slugs }))).await?,
        Envs::Protect { slug, approvals } => c.call(Method::PUT, &format!("{}/policy", c.env(&slug)), Some(json!({ "protected": true, "approvals": approvals }))).await?,
        Envs::Unprotect { slug } => c.call(Method::PUT, &format!("{}/policy", c.env(&slug)), Some(json!({ "protected": false }))).await?,
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if let Some(d) = v["deleted"].as_array() {
        println!("Deleted {}", d.iter().map(s).collect::<Vec<_>>().join(", "));
    } else if let Some(list) = v.as_array() {
        println!("{}", list.iter().map(|e| s(&e["slug"])).collect::<Vec<_>>().join(" → "));
    } else {
        println!("{} ({})", s(&v["slug"]), s(&v["name"]));
    }
    Ok(())
}

/// An import's body: a JSON file (an object of `KEY: value` or an array of `{key, value}`, as
/// other managers export) when it parses as one, else a `.env` file.
/// An environment's values from another manager's API, as `{KEY: value}`: read here, with the
/// user's own token, and never through roll's server.
async fn import_from(from: &str, source: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let need = |k: &str| env(k).with_context(|| format!("set {k}"));
    // No redirects: a token never follows one to another host.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
    let get = |req: reqwest::RequestBuilder, what: String| async move {
        let res = req.send().await.with_context(|| what.clone())?;
        let status = res.status();
        // Never the answer in an error: it holds values.
        anyhow::ensure!(status.is_success(), "{what}: {status}");
        res.json::<Value>().await.with_context(|| format!("{what}: not JSON"))
    };
    let mut out = serde_json::Map::new();
    let mut put = |k: &str, v: &Value| {
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Null => return,
            other => other.to_string(),
        };
        out.insert(k.to_string(), Value::String(text));
    };
    match from {
        "doppler" => {
            let (project, config) = source.split_once('/').context("--source is project/config")?;
            let host = env("DOPPLER_API_HOST").unwrap_or_else(|| "https://api.doppler.com".into());
            let url = format!("{host}/v3/configs/config/secrets/download?project={}&config={}&format=json", enc(project), enc(config));
            let v = get(http.get(url).bearer_auth(need("DOPPLER_TOKEN")?), format!("doppler {source}")).await?;
            for (k, val) in v.as_object().into_iter().flatten() {
                // Doppler's own, describing the config.
                if !k.starts_with("DOPPLER_") {
                    put(k, val);
                }
            }
        }
        "vault" => {
            let (mount, path) = source.split_once('/').context("--source is mount/path (KV v2)")?;
            let addr = need("VAULT_ADDR")?;
            let url = format!("{}/v1/{mount}/data/{path}", addr.trim_end_matches('/'));
            let v = get(http.get(url).header("x-vault-token", need("VAULT_TOKEN")?), format!("vault {source}")).await?;
            for (k, val) in v["data"]["data"].as_object().into_iter().flatten() {
                put(k, val);
            }
        }
        "infisical" => {
            let mut parts = source.splitn(3, '/');
            let (ws, environment) = (parts.next().unwrap_or_default(), parts.next().context("--source is workspace-id/environment[/path]")?);
            let path = format!("/{}", parts.next().unwrap_or_default());
            let api = env("INFISICAL_API_URL").unwrap_or_else(|| "https://app.infisical.com".into());
            let url = format!("{}/api/v3/secrets/raw?workspaceId={}&environment={}&secretPath={}", api.trim_end_matches('/'), enc(ws), enc(environment), enc(&path));
            let v = get(http.get(url).bearer_auth(need("INFISICAL_TOKEN")?), format!("infisical {source}")).await?;
            for x in v["secrets"].as_array().into_iter().flatten() {
                if let Some(k) = x["secretKey"].as_str() {
                    put(k, &x["secretValue"]);
                }
            }
        }
        "aws" => {
            // The user's own `aws` CLI (profiles, SSO, roles); values come back on its stdout,
            // never on a command line.
            let list = aws(&["secretsmanager", "list-secrets", "--filters", &format!("Key=name,Values={source}"), "--output", "json"]).await?;
            let names: Vec<String> = list["SecretList"].as_array().into_iter().flatten().filter_map(|x| x["Name"].as_str().map(String::from)).filter(|n| n.starts_with(source)).collect();
            anyhow::ensure!(names.len() <= 500, "{} secrets start with {source}: 500 at most at once; import a narrower prefix", names.len());
            // Where each key came from: two secrets that give one key stop it, naming both.
            let mut from: std::collections::BTreeMap<String, String> = Default::default();
            for name in names {
                let v = aws(&["secretsmanager", "get-secret-value", &format!("--secret-id={name}"), "--output", "json"]).await?;
                let Some(text) = v["SecretString"].as_str() else {
                    eprintln!("{name}: binary, left out");
                    continue;
                };
                for (k, val) in aws_entries(&name, source, text) {
                    if let Some(other) = from.insert(k.clone(), name.clone()) {
                        bail!("{other} and {name} both give {k}: import them into different folders (a narrower --source each)");
                    }
                    put(&k, &Value::String(val));
                }
            }
        }
        other => bail!("{other}: --from is doppler, vault, infisical or aws"),
    }
    anyhow::ensure!(!out.is_empty(), "{from} {source} has no values to import");
    Ok(out)
}

fn import_body(text: &str, path: &str, overwrite: bool, request: bool) -> anyhow::Result<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let j: Value = serde_json::from_str(text).context("the file looks like JSON but doesn't parse")?;
        return Ok(json!({ "json": j, "path": path, "overwrite": overwrite, "request": request }));
    }
    Ok(json!({ "dotenv": text, "path": path, "overwrite": overwrite, "request": request }))
}

/// A route's answer as text (exports), with the API's error on failure.
async fn text_of(api: &Api, path: &str) -> anyhow::Result<String> {
    let mut req = api.http.get(format!("{}{path}", api.base));
    if let Some(k) = &api.key {
        req = req.bearer_auth(k);
    }
    let res = req.send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        let e: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        bail!("{}", e["error"].as_str().unwrap_or(status.as_str()));
    }
    Ok(text)
}

async fn syncs(c: &Client<'_>, cmd: Syncs, json_out: bool) -> anyhow::Result<()> {
    let base = format!("{}/syncs", c.base());
    match cmd {
        Syncs::Ls => {
            let v = c.call(Method::GET, &base, None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for x in v["syncs"].as_array().into_iter().flatten() {
                let when = x["last_synced_at"].as_i64().map(ago).unwrap_or_else(|| "never".into());
                let err = x["error"].as_str().map(|e| format!("  ({e})")).unwrap_or_default();
                println!("{}  {:<10} {}:{:<10} {:<8} {} keys, {when}{err}", s(&x["id"]), s(&x["kind"]), s(&x["environment"]), s(&x["path"]), s(&x["status"]), x["synced"]);
            }
            let kinds: Vec<String> = v["kinds"].as_array().into_iter().flatten().map(|k| s(&k["kind"])).collect();
            eprintln!("Kinds: {}", kinds.join(", "));
        }
        Syncs::Add { kind, target, keys, prefix, preview } => {
            let mut t = serde_json::Map::new();
            for kv in target {
                let (k, v) = kv.split_once('=').with_context(|| format!("{kv}: --target is name=value"))?;
                t.insert(k.into(), json!(v));
            }
            let mut body = json!({ "environment": c.at.env, "path": c.at.path, "kind": kind, "target": t });
            if !keys.is_empty() {
                body["keys"] = json!(keys);
            }
            if let Some(p) = prefix {
                body["prefix"] = json!(p);
            }
            if preview {
                let v = c.call(Method::POST, &format!("{base}/preview"), Some(body)).await?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
                let list = |k: &str| v[k].as_array().into_iter().flatten().map(s).collect::<Vec<_>>();
                let names = list("names");
                println!("It would push {} name(s): {}", names.len(), if names.is_empty() { "nothing".into() } else { names.join(", ") });
                if !list("credentials").is_empty() {
                    println!("Never its credential: {}", list("credentials").join(", "));
                }
                for (k, why) in [("missing_credentials", "isn't set here: the first push would fail"), ("unknown_keys", "isn't a key here")] {
                    for n in list(k) {
                        println!("{n} {why}");
                    }
                }
                return Ok(());
            }
            let v = c.call(Method::POST, &base, Some(body)).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("Added {} ({} of {}:{}); it pushes at the next sweep", s(&v["id"]), s(&v["kind"]), c.at.env, c.at.path);
            if let Some(secret) = v["secret"].as_str() {
                println!("{secret}");
                eprintln!("That's its signing secret (X-Roll-Signature), shown this once.");
            }
        }
        Syncs::Run { id } => {
            let v = c.call(Method::POST, &format!("{base}/{}/run", enc(&id)), None).await?;
            println!("{} is {}{}", id, s(&v["status"]), v["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default());
        }
        Syncs::Rm { id } => {
            c.call(Method::DELETE, &format!("{base}/{}", enc(&id)), None).await?;
            println!("Removed {id} (what it pushed stays at the target)");
        }
    }
    Ok(())
}

async fn sinks(c: &Client<'_>, cmd: Sinks, json_out: bool) -> anyhow::Result<()> {
    let base = format!("{}/audit/sinks", c.base());
    match cmd {
        Sinks::Ls => {
            let v = c.call(Method::GET, &base, None).await?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for x in v.as_array().into_iter().flatten() {
                let when = x["delivered_at"].as_i64().map(ago).unwrap_or_else(|| "never".into());
                println!("{}  {:<6} {}  delivered {when}{}", s(&x["id"]), s(&x["status"]), s(&x["url"]), x["error"].as_str().map(|e| format!("  ({e})")).unwrap_or_default());
            }
        }
        Sinks::Add { url, from_start } => {
            let v = c.call(Method::POST, &base, Some(json!({ "url": url, "from_start": from_start }))).await?;
            println!("{}", s(&v["secret"]));
            eprintln!("Streaming the audit log to {url} ({}). That's its signing secret, shown this once.", s(&v["id"]));
        }
        Sinks::Rm { id } => {
            c.call(Method::DELETE, &format!("{base}/{}", enc(&id)), None).await?;
            println!("Removed {id}");
        }
    }
    Ok(())
}

/// A random value of `n` letters and digits (62 symbols, rejection-sampled: no bias).
fn random_value(n: usize) -> anyhow::Result<String> {
    anyhow::ensure!((8..=4096).contains(&n), "--generate is 8–4096 characters");
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 64];
    while out.len() < n {
        getrandom::fill(&mut buf).expect("system randomness");
        for b in buf {
            if b < 248 && out.len() < n {
                out.push(ALPHABET[(b % 62) as usize] as char);
            }
        }
    }
    Ok(out)
}

/// Write `text` to a new file only its owner can read (never over an existing one).
fn write_private(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(path).with_context(|| format!("creating {} (it mustn't exist yet)", path.display()))?.write_all(text.as_bytes())?;
    Ok(())
}

/// A workload identity's login (`ROLL_IDENTITY`): the workload's own OIDC token, from
/// `ROLL_OIDC_TOKEN`, `ROLL_OIDC_TOKEN_FILE` (a Kubernetes projected token) or GitHub Actions
/// (`permissions: id-token: write`; audience `ROLL_OIDC_AUDIENCE`, default `roll`), for a
/// short-lived secrets token.
pub async fn identity_login(api: &Api, identity: &str) -> anyhow::Result<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|t| !t.trim().is_empty());
    let oidc = if let Some(t) = env("ROLL_OIDC_TOKEN") {
        t
    } else if let Some(f) = env("ROLL_OIDC_TOKEN_FILE") {
        std::fs::read_to_string(&f).with_context(|| format!("reading ROLL_OIDC_TOKEN_FILE {f}"))?
    } else if let (Some(url), Some(bearer)) = (env("ACTIONS_ID_TOKEN_REQUEST_URL"), env("ACTIONS_ID_TOKEN_REQUEST_TOKEN")) {
        // The server's own URL by default: a token minted for one roll server isn't good at another.
        let audience = env("ROLL_OIDC_AUDIENCE").unwrap_or_else(|| api.base.clone());
        let sep = if url.contains('?') { '&' } else { '?' };
        let r = reqwest::Client::new().get(format!("{url}{sep}audience={}", enc(&audience))).bearer_auth(bearer).send().await.context("asking GitHub Actions for its OIDC token")?;
        anyhow::ensure!(r.status().is_success(), "GitHub Actions answered {} for its OIDC token (does the job have `permissions: id-token: write`?)", r.status());
        let v: Value = r.json().await?;
        v["value"].as_str().context("GitHub Actions' OIDC answer has no value")?.to_string()
    } else {
        anyhow::bail!("ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`");
    };
    let anon = Api { base: api.base.clone(), http: api.http.clone(), key: None, headers: api.headers.clone() };
    let v = anon.post("/v1/secrets/login", json!({ "identity": identity, "token": oidc.trim() })).await.context("logging in with the workload identity")?;
    v["token"].as_str().map(str::to_string).context("the login answered no token")
}

/// `roll secrets edit`: the folder's own keys as a `.env` in `$EDITOR` (a 0600 file under
/// `$XDG_RUNTIME_DIR` when there is one, removed afterwards), and what changed as one change.
async fn edit(c: &Client<'_>, env: &str, request: bool, json_out: bool) -> anyhow::Result<()> {
    let (seq, text) = edit_text(c, env).await?;
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    let file = tempfile::Builder::new().prefix("roll-secrets-").suffix(".env").tempfile_in(dir)?;
    std::fs::write(file.path(), &text)?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(file.path()).status().with_context(|| format!("running {editor}"))?;
    anyhow::ensure!(status.success(), "{editor} exited with {status}: nothing applied");
    let edited = std::fs::read_to_string(file.path())?;
    drop(file);
    if edited == text {
        println!("No changes");
        return Ok(());
    }
    // Only over what was edited: a change to the environment meanwhile is retried once when it
    // left this folder as it was, and otherwise nothing is applied.
    let body = |seq: i64| json!({ "dotenv": edited, "path": c.at.path, "replace": true, "request": request, "message": "edited with roll secrets edit", "if_seq": seq });
    let v = match c.call(Method::POST, &format!("{env}/import"), Some(body(seq))).await {
        Err(e) if e.to_string().contains("changed after #") => {
            let (seq, now) = edit_text(c, env).await?;
            anyhow::ensure!(now == text, "{}:{} changed while you were editing: nothing applied (run roll secrets edit again)", c.at.env, c.at.path);
            c.call(Method::POST, &format!("{env}/import"), Some(body(seq))).await
        }
        v => v,
    }
    .map_err(approval_hint)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if let Some(r) = v.get("request").filter(|r| r.is_object()) {
        println!("Opened change request {} on {}", s(&r["id"]), c.at.env);
    } else {
        summary(&v);
    }
    Ok(())
}

/// The environment's last change, then the folder's own keys as the `.env` that `edit` opens.
async fn edit_text(c: &Client<'_>, env: &str) -> anyhow::Result<(i64, String)> {
    let changes = c.call(Method::GET, &format!("{env}/changes?limit=1"), None).await?;
    let seq = changes.as_array().and_then(|a| a.first()).and_then(|ch| ch["seq"].as_i64()).unwrap_or(0);
    let list = c.call(Method::GET, &format!("{env}/secrets?{}", c.q()), None).await?;
    let own: Vec<&Value> = list.as_array().into_iter().flatten().filter(|s| s["source"]["kind"] == "own" && s["type"] == "shared").collect();
    let values = c.call(Method::GET, &format!("{env}/values?{}&resolve=false", c.q()), None).await?;
    let mut text = format!("# {}:{} of {} (roll secrets edit). Save to apply what changed as one change; remove a line to delete its key.\n\n", c.at.env, c.at.path, c.project);
    let mut linked = vec![];
    for s in &own {
        let key = s["key"].as_str().unwrap_or_default();
        if s["link"].is_object() {
            linked.push(key.to_string());
            continue;
        }
        if let Some(cm) = s["comment"].as_str().filter(|c| !c.is_empty()) {
            text.push_str(&format!("# {}\n", cm.replace('\n', " ")));
        }
        let v = values["values"][key].as_str().unwrap_or_default();
        text.push_str(&format!("{key}={}\n", dotenv_quote(v)));
    }
    if !linked.is_empty() {
        text.push_str(&format!("# Linked, not editable here: {}\n", linked.join(", ")));
    }
    Ok((seq, text))
}

/// A value as a dotenv line's right-hand side: as is when plain, else double-quoted.
fn dotenv_quote(v: &str) -> String {
    let plain = !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@+,=%".contains(c));
    if plain {
        return v.to_string();
    }
    let mut out = String::from("\"");
    for ch in v.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '$' => out.push_str("\\$"),
            o => out.push(o),
        }
    }
    out.push('"');
    out
}

/// A value encrypted for a share: (base64url(nonce ‖ ciphertext), base64url(key)). AES-256-GCM
/// with `roll-share-v1` as associated data, the dashboard's share page's format.
fn share_seal(value: &str) -> (String, String) {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    let (mut key, mut nonce) = ([0u8; 32], [0u8; 12]);
    getrandom::fill(&mut key).expect("system randomness");
    getrandom::fill(&mut nonce).expect("system randomness");
    let ct = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32 bytes").encrypt(&nonce.into(), Payload { msg: value.as_bytes(), aad: b"roll-share-v1" }).expect("encrypt");
    (B64.encode([&nonce[..], &ct].concat()), B64.encode(key))
}

/// `roll secrets open <link>`: one of the share's opens, decrypted here.
async fn open_share(api: &Api, link: &str) -> anyhow::Result<()> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    let (head, key) = link.trim().rsplit_once('#').context("a share's link ends with #<key>")?;
    let id = head.rsplit('/').next().unwrap_or(head);
    let key = B64.decode(key).ok().filter(|k| k.len() == 32).context("the link's key isn't a share key")?;
    let v = api.call(Method::POST, &format!("/v1/secrets/shares/{}/open", enc(id)), None).await?;
    let raw = B64.decode(s(&v["ciphertext"])).context("the share isn't base64url")?;
    anyhow::ensure!(raw.len() > 12, "the share is too short");
    let plain = aes_gcm::Aes256Gcm::new_from_slice(&key)
        .expect("32 bytes")
        .decrypt(raw[..12].into(), Payload { msg: &raw[12..], aad: b"roll-share-v1" })
        .map_err(|_| anyhow::anyhow!("the link's key doesn't open this share"))?;
    print!("{}", String::from_utf8_lossy(&plain));
    if !plain.ends_with(b"\n") {
        println!();
    }
    Ok(())
}

/// `roll secrets setup`: remember (or show, or forget) where this directory's secrets are.
async fn setup_dir(api: &Api, args: &AtArgs, show: bool, clear: bool) -> anyhow::Result<()> {
    let here = std::env::current_dir()?;
    let key = here.to_string_lossy().to_string();
    let mut all = setup::load();
    if show {
        match setup::nearest(&here) {
            Some(d) => println!("{} ({})  -e {}  --path {}", d.name, d.project, d.env, d.path),
            None => println!("Nothing set up for {key}: roll secrets setup -p <project> -e <environment>"),
        }
        return Ok(());
    }
    if clear {
        let gone = all.remove(&key).is_some();
        setup::save(&all)?;
        println!("{}", if gone { format!("Forgot the setup of {key}") } else { format!("{key} had no setup of its own") });
        return Ok(());
    }
    let name = args.project.clone().context("which project? roll secrets setup -p <project> [-e <environment>] [--path /folder]")?;
    let id = project(api, Some(&name)).await?;
    let env = args.env.clone().unwrap_or_else(|| "development".into());
    let path = args.path.clone().unwrap_or_else(|| "/".into());
    // The environment must exist (and be readable) before it's remembered.
    api.get(&format!("/v1/secrets/projects/{}/environments/{}", enc(&id), enc(&env))).await?;
    let display = if name.starts_with("prj_") { id.clone() } else { name };
    all.insert(key.clone(), setup::Dir { project: id, name: display.clone(), env: env.clone(), path: path.clone() });
    let file = setup::save(&all)?;
    println!("{key} → {display}, {env}{}  (kept in {})", if path == "/" { String::new() } else { format!(" {path}") }, file.display());
    Ok(())
}

/// The template filled from the environment, and the leases of the dynamic credentials in it.
async fn render_env(c: &Client<'_>, env: &str, file: &str, tpl: &str) -> anyhow::Result<(String, Vec<String>)> {
    let v = c.call(Method::GET, &format!("{env}/values?{}", c.q()), None).await?;
    let unresolved: std::collections::BTreeMap<String, String> = v["problems"].as_array().into_iter().flatten().map(|p| (s(&p["key"]), s(&p["problem"]))).collect();
    let text = render(tpl, |k| v["values"][k].as_str().map(String::from)).map_err(|missing| {
        let why: Vec<String> = missing.iter().map(|k| unresolved.get(k).map_or_else(|| format!("{k} isn't set in {}:{}", c.at.env, c.at.path), |p| format!("{k}: {p}"))).collect();
        anyhow::anyhow!("{file} names keys that have no value:\n  {}", why.join("\n  "))
    })?;
    let leases = v["leases"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect();
    Ok((text, leases))
}

/// Replace `path` whole (a file beside it, then a rename), readable only by its owner: a reader
/// never sees half a file.
fn replace_private(path: &str, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let target = std::path::Path::new(path);
    let name = target.file_name().with_context(|| format!("{path}: not a file name"))?.to_string_lossy();
    // A fresh, unguessable name, created new: never one someone else made (or a link to one).
    let tmp = target.with_file_name(format!(".{name}.roll-{}", random_value(16)?));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let written = o.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes()).and_then(|_| f.sync_all())).and_then(|_| std::fs::rename(&tmp, target));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {path}"))
}

/// `roll secrets render --watch`: write the file again when the environment's digest changes and
/// the text with it, then run `exec`; renew dynamic credentials' leases every 10 minutes.
#[allow(clippy::too_many_arguments)]
async fn render_watch(c: &Client<'_>, env: &str, file: &str, tpl: &str, out: &str, mut text: String, mut leases: Vec<String>, interval: u64, exec: Option<&str>) -> anyhow::Result<()> {
    let digest_path = format!("{env}/digest?{}", c.q());
    let mut last = c.call(Method::GET, &digest_path, None).await?["digest"].clone();
    let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + RENEW_EVERY, RENEW_EVERY);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = renew.tick(), if !leases.is_empty() => {
                for l in &leases {
                    if let Err(e) = c.call(Method::POST, &format!("{}/leases/{}/renew", c.base(), enc(l)), None).await {
                        eprintln!("roll secrets render: couldn't renew lease {l} ({e})");
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))) => {
                let now = match c.call(Method::GET, &digest_path, None).await {
                    Ok(v) => v["digest"].clone(),
                    Err(e) => {
                        eprintln!("roll secrets render: couldn't check {} ({e}); {out} stays as it is", c.at.env);
                        continue;
                    }
                };
                if now == last {
                    continue;
                }
                last = now;
                let (next, next_leases) = match render_env(c, env, file, tpl).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("roll secrets render: {} changed, but {e}; {out} stays as it is", c.at.env);
                        continue;
                    }
                };
                leases = next_leases;
                if next == text {
                    continue;
                }
                replace_private(out, &next)?;
                text = next;
                eprintln!("roll secrets render: {}:{} changed, wrote {out}", c.at.env, c.at.path);
                if let Some(cmd) = exec {
                    match tokio::process::Command::new("sh").arg("-c").arg(cmd).status().await {
                        Ok(st) if st.success() => {}
                        Ok(st) => eprintln!("roll secrets render: {cmd} exited with {st}"),
                        Err(e) => eprintln!("roll secrets render: running {cmd}: {e}"),
                    }
                }
            }
        }
    }
}

/// `roll secrets doctor`: each check, ok or not, and why.
async fn doctor(api: &Api, args: &AtArgs) -> anyhow::Result<()> {
    let mut checks: Vec<(bool, String)> = vec![];
    // Who this is.
    let who = match api.get("/v1/secrets/whoami").await {
        Ok(w) => w,
        Err(e) => {
            println!("FAIL roll secrets at {}: {e}", api.base);
            bail!("can't go on without the API (ROLL_API_URL, ROLL_TOKEN or ROLL_IDENTITY)");
        }
    };
    checks.push((true, format!("signed in at {} as {}{}", api.base, s(&who["principal"]), who["project"].as_str().map(|p| format!(", limited to {p}")).unwrap_or_default())));
    // This directory's setup, and what it points at.
    let here = std::env::current_dir().ok().and_then(|d| setup::nearest(&d));
    let at = args.resolve();
    checks.push(match &here {
        Some(h) => (true, format!("this directory is set up for {} {}:{}", h.name, h.env, h.path)),
        None if at.project.is_some() => (true, "no setup here; using the flags and environment".into()),
        None => (false, "no project for this directory: roll secrets setup -p <project> -e <environment>".into()),
    });
    let mut keys: Option<std::collections::BTreeSet<String>> = None;
    match project(api, at.project.as_deref()).await {
        Err(e) => checks.push((false, format!("the project: {e}"))),
        Ok(id) => {
            let env = format!("/v1/secrets/projects/{}/environments/{}", enc(&id), enc(&at.env));
            match api.get(&format!("{env}/secrets?path={}", enc(&at.path))).await {
                Err(e) => checks.push((false, format!("{}:{}: {e}", at.env, at.path))),
                Ok(list) => {
                    let names: std::collections::BTreeSet<String> = list.as_array().into_iter().flatten().filter_map(|x| x["key"].as_str().map(String::from)).collect();
                    checks.push((true, format!("{}:{} has {} key(s) you can read", at.env, at.path, names.len())));
                    keys = Some(names);
                }
            }
            // A local .env, if any, still matches (by fingerprint, never printing a value).
            if let Ok(text) = std::fs::read_to_string(".env") {
                match api.call(Method::POST, &format!("{env}/diff"), Some(import_body(&text, &at.path, false, false)?)).await {
                    Ok(d) => {
                        let names = |k: &str| d[k].as_array().into_iter().flatten().map(s).collect::<Vec<_>>();
                        let (changed, added, missing) = (names("changed"), names("added"), names("missing"));
                        checks.push(if changed.is_empty() && added.is_empty() && missing.is_empty() {
                            (true, format!(".env matches {}:{}", at.env, at.path))
                        } else {
                            let mut parts = vec![];
                            for (what, list) in [("differs", &changed), ("only in .env", &added), ("missing from .env", &missing)] {
                                if !list.is_empty() {
                                    parts.push(format!("{what}: {}", list.join(", ")));
                                }
                            }
                            (false, format!(".env is stale against {} ({}); roll secrets export > .env, or roll run", at.env, parts.join("; ")))
                        });
                    }
                    Err(e) => checks.push((false, format!("comparing .env: {e}"))),
                }
            }
            // Nothing in this environment needs fixing now.
            if let Ok(found) = api.get(&format!("/v1/secrets/projects/{}/health", enc(&id))).await {
                let serious: Vec<String> = found
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|f| f["environment"] == at.env.as_str() && ["expired", "missing", "exposed"].contains(&f["issue"].as_str().unwrap_or_default()))
                    .map(|f| format!("{} {} ({})", s(&f["issue"]), s(&f["key"]), s(&f["detail"])))
                    .collect();
                checks.push(if serious.is_empty() { (true, format!("nothing in {} needs fixing now", at.env)) } else { (false, format!("{} needs fixing: {}", at.env, serious.join("; "))) });
            }
            // Staged files hold none of the project's values.
            if let Some(files) = staged_files().ok().filter(|f| !f.is_empty()) {
                let mut cands = std::collections::HashSet::new();
                for (_, text) in &files {
                    text.lines().for_each(|l| line_candidates(l, &mut cands));
                }
                let cands: Vec<String> = cands.into_iter().take(20_000).collect();
                match api.call(Method::POST, &format!("/v1/secrets/projects/{}/leaks", enc(&id)), Some(json!({ "candidates": cands }))).await {
                    Ok(v) if v.as_array().is_some_and(|a| a.is_empty()) => checks.push((true, format!("{} staged file(s) hold none of the project's values", files.len()))),
                    Ok(v) => checks.push((false, format!("staged files hold {} of the project's values: roll secrets scan --staged says where", v.as_array().map_or(0, Vec::len)))),
                    Err(e) => checks.push((false, format!("scanning the staged files: {e}"))),
                }
            }
        }
    }
    // The template the code reads from, if there is one.
    if let (Some(have), Ok(text)) = (&keys, std::fs::read_to_string(".env.example")) {
        let missing: Vec<String> = template_keys(&text).into_iter().filter(|k| !have.contains(k)).collect();
        checks.push(if missing.is_empty() { (true, ".env.example's keys are all set".into()) } else { (false, format!(".env.example names {} that {} doesn't set", missing.join(", "), at.env)) });
    }
    for (ok, what) in &checks {
        println!("{} {what}", if *ok { "ok  " } else { "FAIL" });
    }
    let failed = checks.iter().filter(|c| !c.0).count();
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    Ok(())
}

/// Print findings, and fail when one needs fixing now; `projects`: say which project each is in.
fn report_health(found: &[Value], projects: bool) -> anyhow::Result<()> {
    for f in found {
        let project = if projects { format!("{:<16} ", s(&f["project_name"])) } else { String::new() };
        println!("{:<9} {project}{:<14} {:<28} {}{}", s(&f["issue"]).to_uppercase(), s(&f["environment"]), s(&f["key"]), s(&f["detail"]), if f["path"] == "/" { String::new() } else { format!("  ({})", s(&f["path"])) });
    }
    let serious = found.iter().filter(|f| ["expired", "missing", "exposed"].contains(&f["issue"].as_str().unwrap_or_default())).count();
    if found.is_empty() {
        eprintln!("Nothing to fix");
    }
    if serious > 0 {
        bail!("{serious} to fix now (expired, missing or exposed)");
    }
    Ok(())
}

/// The keys a `.env`-style template names: `KEY=…`, `export KEY=…` and a bare `KEY`, not comments.
fn template_keys(text: &str) -> std::collections::BTreeSet<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("export ").unwrap_or(l).trim_start();
            let key = l.split(['=', ' ', '\t']).next()?;
            (!l.starts_with('#') && !key.is_empty() && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| key.to_string())
        })
        .collect()
}

/// A line's candidates, as roll code's scan takes them: runs of `[A-Za-z0-9_\-+/=.:@~]`, the whole
/// trimmed line, and the right-hand side of `KEY=VALUE` or `key: value` (unquoted). The server
/// never matches anything under 8 characters.
fn line_candidates(line: &str, out: &mut std::collections::HashSet<String>) {
    let mut add = |s: &str| {
        if (8..=4096).contains(&s.chars().count()) {
            out.insert(s.to_string());
        }
    };
    for run in line.split(|ch: char| !(ch.is_ascii_alphanumeric() || "_-+/=.:@~".contains(ch))) {
        add(run);
    }
    let trimmed = line.trim();
    add(trimmed);
    if let Some(i) = trimmed.find(['=', ':']) {
        let rhs = trimmed[i + 1..].trim().trim_end_matches([',', ';']).trim();
        add(['"', '\'', '`'].iter().find_map(|q| rhs.strip_prefix(*q).and_then(|s| s.strip_suffix(*q))).unwrap_or(rhs));
    }
}

/// Text files under `paths` (skipping `.git`, `node_modules`, `target`, binaries and files over 1 MiB).
fn walk(paths: Vec<String>) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = vec![];
    let mut todo: Vec<std::path::PathBuf> = paths.into_iter().map(Into::into).collect();
    while let Some(p) = todo.pop() {
        let meta = std::fs::symlink_metadata(&p).with_context(|| format!("reading {}", p.display()))?;
        if meta.is_dir() {
            if p.file_name().is_some_and(|n| [".git", "node_modules", "target"].contains(&n.to_string_lossy().as_ref())) {
                continue;
            }
            for e in std::fs::read_dir(&p)? {
                todo.push(e?.path());
            }
        } else if meta.is_file() && meta.len() <= 1 << 20 {
            let bytes = std::fs::read(&p)?;
            if !bytes.contains(&0) {
                out.push((p.display().to_string().trim_start_matches("./").to_string(), String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
    }
    Ok(out)
}

/// What `git commit` would record: each staged file's staged content.
fn staged_files() -> anyhow::Result<Vec<(String, String)>> {
    let git = |args: &[&str]| -> anyhow::Result<Vec<u8>> {
        let o = std::process::Command::new("git").args(args).output().context("running git")?;
        if !o.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
        }
        Ok(o.stdout)
    };
    let names = git(&["diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z"])?;
    let mut out = vec![];
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        let bytes = git(&["show", &format!(":{name}")])?;
        if bytes.len() <= 1 << 20 && !bytes.contains(&0) {
            out.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    Ok(out)
}

/// `${KEY}` → its value; `\${` is a literal `${`. Anything else (`$KEY`, `${a:-b}`) stays as
/// written, so shell snippets in the file survive. Fails with every key that has no value.
fn render(tpl: &str, get: impl Fn(&str) -> Option<String>) -> Result<String, Vec<String>> {
    let (mut out, mut missing, mut rest) = (String::with_capacity(tpl.len()), Vec::new(), tpl);
    while let Some(i) = rest.find("${") {
        if rest[..i].ends_with('\\') {
            out.push_str(&rest[..i - 1]);
            out.push_str("${");
            rest = &rest[i + 2..];
            continue;
        }
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after.find('}').filter(|&e| {
            let k = &after[..e];
            k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        match end {
            Some(e) => {
                let k = &after[..e];
                match get(k) {
                    Some(v) => out.push_str(&v),
                    None if !missing.iter().any(|m| m == k) => missing.push(k.to_string()),
                    None => {}
                }
                rest = &after[e + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if missing.is_empty() { Ok(out) } else { Err(missing) }
}

/// The variables `roll run` adds: the environment's resolved values, minus the ones the shell
/// already sets (unless `--override`). Fails on a problem unless `--allow-unresolved`.
async fn run_env(c: &Client<'_>, at: &At, r: &Run) -> anyhow::Result<(Vec<(String, String)>, Vec<String>)> {
    let v = c.call(Method::GET, &format!("{}/values?{}", c.env(&at.env), c.q()), None).await?;
    let problems: Vec<String> = v["problems"].as_array().into_iter().flatten().map(|p| format!("{}: {}", s(&p["key"]), s(&p["problem"]))).collect();
    if !problems.is_empty() && !r.allow_unresolved {
        bail!("{} of {}:{} didn't resolve (pass --allow-unresolved to run anyway):\n  {}", problems.len(), at.env, at.path, problems.join("\n  "));
    }
    let env = v["values"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| r.override_env || std::env::var_os(k).is_none())
        .map(|(k, val)| (k.clone(), val.as_str().unwrap_or_default().to_string()))
        .collect();
    let leases = v["leases"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect();
    Ok((env, leases))
}

/// `roll run`: resolve, then exec the command with the values in its environment; with
/// `--watch`, run it as a child and restart it when the environment's digest changes.
pub async fn run_command(api: &Api, r: Run) -> anyhow::Result<()> {
    let at = r.at.resolve();
    let project = project(api, at.project.as_deref()).await?;
    let c = Client { project, api, at: at.clone() };
    let (env, leases) = run_env(&c, &at, &r).await?;
    if r.watch {
        return watch(&c, &at, &r, env, leases).await;
    }
    let mut cmd = std::process::Command::new(&r.command[0]);
    cmd.args(&r.command[1..]).envs(env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        bail!("running {}: {e}", r.command[0]);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().with_context(|| format!("running {}", r.command[0]))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// `roll run --watch`: the command as a child, restarted (SIGTERM, then SIGKILL after 10 s) when
/// the environment changes. A change that doesn't resolve keeps the running command. Exits with
/// the command's own code when it exits by itself.
async fn watch(c: &Client<'_>, at: &At, r: &Run, mut env: Vec<(String, String)>, mut leases: Vec<String>) -> anyhow::Result<()> {
    let digest_path = format!("{}/digest?{}", c.env(&at.env), c.q());
    let mut last = c.call(Method::GET, &digest_path, None).await?["digest"].clone();
    let every = std::time::Duration::from_secs(r.interval.max(1));
    // Dynamic credentials outlive their first lease: renewed every 10 minutes while it runs.
    let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + RENEW_EVERY, RENEW_EVERY);
    loop {
        let mut child = tokio::process::Command::new(&r.command[0]).args(&r.command[1..]).envs(env.iter().cloned()).spawn().with_context(|| format!("running {}", r.command[0]))?;
        loop {
            tokio::select! {
                status = child.wait() => std::process::exit(status?.code().unwrap_or(1)),
                _ = tokio::signal::ctrl_c() => {
                    stop(&mut child).await;
                    std::process::exit(130);
                }
                _ = renew.tick(), if !leases.is_empty() => {
                    for l in &leases {
                        if let Err(e) = c.call(Method::POST, &format!("{}/leases/{}/renew", c.base(), enc(l)), None).await {
                            eprintln!("roll run: couldn't renew lease {l} ({e})");
                        }
                    }
                }
                _ = tokio::time::sleep(every) => {
                    let now = match c.call(Method::GET, &digest_path, None).await {
                        Ok(v) => v["digest"].clone(),
                        Err(e) => {
                            eprintln!("roll run: couldn't check {} ({e}); still running", at.env);
                            continue;
                        }
                    };
                    if now == last {
                        continue;
                    }
                    last = now;
                    match run_env(c, at, r).await {
                        Ok((next, next_leases)) => {
                            eprintln!("roll run: {}:{} changed, restarting {}", at.env, at.path, r.command[0]);
                            env = next;
                            leases = next_leases;
                            stop(&mut child).await;
                            break;
                        }
                        Err(e) => eprintln!("roll run: {} changed, but {e}; keeping the running command", at.env),
                    }
                }
            }
        }
    }
}

/// How often `roll run --watch` renews its dynamic credentials' leases.
const RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// SIGTERM, and SIGKILL if it hasn't exited after 10 seconds.
async fn stop(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) on our own child's pid.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

/// Run the user's `aws` CLI with `args` and read its JSON (errors name the command, never output).
async fn aws(args: &[&str]) -> anyhow::Result<Value> {
    let out = tokio::process::Command::new(std::env::var("ROLL_AWS_CLI").unwrap_or_else(|_| "aws".into()))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running the aws CLI (install it, or set ROLL_AWS_CLI)")?;
    anyhow::ensure!(out.status.success(), "aws {}: {}", args[..2].join(" "), String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default().chars().take(200).collect::<String>());
    serde_json::from_slice(&out.stdout).context("the aws CLI's answer isn't JSON")
}

/// A Secrets Manager secret as keys: a JSON object's fields as they are; anything else one key
/// named after the secret's last segment (`prod/app/db-url` → `DB_URL`).
fn aws_entries(name: &str, prefix: &str, text: &str) -> Vec<(String, String)> {
    if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(text) {
        return fields.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect();
    }
    let last = name.strip_prefix(prefix).unwrap_or(name).rsplit('/').next().unwrap_or(name);
    let mut key: String = last.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    if key.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        key.insert(0, '_');
    }
    vec![(key, text.to_string())]
}

/// Every secret of a Vault KV v2 tree under `mount/path`, by the folder it goes to (its path
/// under `path`; the secret at `path` itself goes to `""`). At most 500 secrets, 8 levels.
async fn vault_tree(source: &str) -> anyhow::Result<Vec<(String, serde_json::Map<String, Value>)>> {
    let (mount, base) = source.split_once('/').map(|(m, p)| (m.to_string(), p.trim_matches('/').to_string())).unwrap_or((source.to_string(), String::new()));
    let addr = std::env::var("VAULT_ADDR").context("set VAULT_ADDR")?;
    let token = std::env::var("VAULT_TOKEN").context("set VAULT_TOKEN")?;
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
    let url = |kind: &str, p: &str| format!("{}/v1/{mount}/{kind}/{}", addr.trim_end_matches('/'), p.trim_matches('/'));
    let segment = |s: &str| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    // An answer read whole, up to 4 MiB; `None` for a 404 (nothing there). Anything else stops
    // it: a secret the token can't read must not be left out unsaid.
    let fetch = |req: reqwest::RequestBuilder, what: String| async move {
        let mut res = req.send().await.with_context(|| what.clone())?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok::<_, anyhow::Error>(None);
        }
        anyhow::ensure!(res.status().is_success(), "{what}: {} (the token needs read and list on it)", res.status());
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.with_context(|| what.clone())? {
            anyhow::ensure!(body.len() + chunk.len() <= 4 << 20, "{what}: the answer is over 4 MiB");
            body.extend_from_slice(&chunk);
        }
        Ok(Some(serde_json::from_slice::<Value>(&body).with_context(|| format!("{what}: not JSON"))?))
    };
    let mut out = vec![];
    let mut visits = 0usize;
    // (path under the mount, folder under the source, depth)
    // (path under the mount, folder under the source, depth, a secret to read, a folder to list).
    // Vault lists a secret as `name` and a folder as `name/`: each is read only as what it is,
    // so a token with read on `app/*` but not on `app` itself still imports `app/…`.
    let mut queue = vec![(base.clone(), String::new(), 0usize, true, true)];
    while let Some((at, folder, depth, secret, listed)) = queue.pop() {
        visits += 1;
        anyhow::ensure!(visits <= 2_000, "vault {mount}/{base}: more than 2,000 paths; import it in parts");
        if secret {
            let read = fetch(http.get(url("data", &at)).header("x-vault-token", &token), format!("vault {mount}/{at}")).await;
            // The source itself may be only a folder the token can't read as a secret.
            let read = match read {
                Err(e) if at == base && e.to_string().contains("403") => {
                    eprintln!("vault {mount}/{at}: not readable as a secret itself, its folders only");
                    None
                }
                other => other?,
            };
            if let Some(v) = read {
                let fields: serde_json::Map<String, Value> = v["data"]["data"].as_object().into_iter().flatten().filter(|(_, x)| !x.is_null()).map(|(k, x)| (k.clone(), Value::String(x.as_str().map(String::from).unwrap_or_else(|| x.to_string())))).collect();
                if !fields.is_empty() {
                    out.push((folder.clone(), fields));
                }
            }
        }
        if !listed {
            continue;
        }
        let Some(v) = fetch(http.request(reqwest::Method::from_bytes(b"LIST").expect("LIST"), url("metadata", &at)).header("x-vault-token", &token), format!("vault {mount}/{at} (list)")).await? else {
            continue;
        };
        for k in v["data"]["keys"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let name = k.trim_end_matches('/');
            anyhow::ensure!(segment(name), "vault {mount}/{at}/{name}: a roll folder is letters, digits, _ and - (rename it, or import it alone)");
            anyhow::ensure!(depth < 8 && out.len() + queue.len() < 500, "vault {mount}/{base}: more than 500 secrets or 8 levels; import it in parts");
            let child_at = if at.is_empty() { name.to_string() } else { format!("{at}/{name}") };
            let child_folder = if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") };
            let is_folder = k.ends_with('/');
            queue.push((child_at, child_folder, depth + 1, !is_folder, is_folder));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

#[cfg(test)]
mod migrate_tests {
    #[test]
    fn an_aws_secret_becomes_keys() {
        assert_eq!(super::aws_entries("prod/app/db", "prod/app/", "{\"DB_USER\":\"app\",\"DB_PORT\":5432}"), [("DB_PORT".to_string(), "5432".to_string()), ("DB_USER".to_string(), "app".to_string())]);
        assert_eq!(super::aws_entries("prod/app/stripe-key", "prod/app/", "sk_live_x"), [("STRIPE_KEY".to_string(), "sk_live_x".to_string())]);
    }
}
