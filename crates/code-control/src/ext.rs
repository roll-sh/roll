//! Extension points that run code: previews (branched Postgres and Redis plus a container) and
//! the checks runner. rolld plugs implementations in with [`crate::Code::set_preview_backend`]
//! and [`crate::Code::set_runner`]; without them previews and runner checks are unavailable.
//! [`SecretEnv`] is roll secrets, for checks, sandboxes and the `secret_leak` blocker
//! ([`crate::Code::set_secret_env`]).

use crate::model::{Change, Check, Preview, RepoRow};
use async_trait::async_trait;
use std::path::Path;

/// What `@roll preview …` asks of a change's preview.
pub use code_review::commands::PreviewAction;

/// Creates and tears down a change's preview.
#[async_trait]
pub trait PreviewBackend: Send + Sync {
    /// Start (or update) the preview of `change` from the tree materialized at `dir`.
    async fn create(&self, repo: &RepoRow, change: &Change, dir: &Path) -> anyhow::Result<Preview>;
    async fn delete(&self, repo: &RepoRow, change: &Change) -> anyhow::Result<()>;
    /// `by` accepts how the change's preview's pages look at `head`'s screenshot (its visual
    /// review, `roll/visual`): a person, the caller has checked; refused when a newer screenshot
    /// came in. Backends without one can't.
    async fn accept_visual(&self, _change: &Change, _by: &str, _head: &str) -> anyhow::Result<()> {
        anyhow::bail!("this preview backend has no visual review")
    }
    /// `by` (someone who can write to the change's repo: the caller checked) asks `action` of the
    /// change's preview (`@roll preview …`): what was done, said for the answer. Backends without
    /// one can't.
    async fn act(&self, _change: &Change, _action: PreviewAction, _by: &str) -> anyhow::Result<String> {
        anyhow::bail!("this change's previews can't be acted on from a comment")
    }
}

/// Runs the checks a repo's `roll.toml` declares against a materialized tree.
#[async_trait]
pub trait Runner: Send + Sync {
    /// The checks to run for this tree, as queued [`Check`]s.
    async fn plan(&self, repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>>;
    /// Run one check to completion.
    async fn run(&self, repo: &RepoRow, change: Option<&Change>, dir: &Path, check: &Check) -> anyhow::Result<Check>;
    /// Why the runner can't use this `roll.toml` text (its `[checks.*]` tables), if it can't.
    /// The landing settings only read `[queue]` and `[main]`, so without this a broken check
    /// table would look healthy in the queue's settings until a run failed.
    fn validate(&self, _text: &str) -> Option<String> {
        None
    }
    /// Remove what a previous process of this node left running; called before checks resume.
    async fn remove_leftovers(&self) {}
}

/// Who asks for an environment's values, as rolld's gate put them on the request: roll secrets
/// answers as it would a request of theirs to it.
#[derive(Clone, Copy, Default)]
pub struct Asker<'a> {
    pub operator: bool,
    pub user: Option<&'a roll_auth::SignedIn>,
    pub grant: Option<&'a roll_auth::Grant>,
}

impl<'a> Asker<'a> {
    pub fn on(ext: &'a axum::http::Extensions) -> Asker<'a> {
        Asker { operator: roll_auth::Operator::on(ext), user: ext.get::<roll_auth::SignedIn>(), grant: roll_auth::Grant::on(ext) }
    }
}

/// An environment's values, who revealed them (who made the copy, as roll secrets records a
/// maker: `usr_…`, `token:…`, `operator`), and the project they're from (its id).
pub struct Revealed {
    pub by: String,
    pub project: String,
    pub values: Vec<(String, String)>,
}

/// The asker may not reveal what they asked for (a 403).
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// An environment's secrets from roll secrets (docs/secrets.md, "Consumption"). Values come back
/// resolved; the caller puts them in a process's environment and nowhere else (no records, no
/// logs). Every read is in the project's audit log as `consumer`.
#[async_trait]
pub trait SecretEnv: Send + Sync {
    /// `environment` (`env`, or `env:/folder`) of the workspace project that has repo
    /// `owner/name` on its canvas. Refuses a protected environment and unresolved references.
    async fn for_repo(&self, repo: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>>;
    /// The same for workspace project `project` (an id or a name), which must have one of
    /// `owner`'s repos on its canvas (sandboxes), copied by `who`: they must be able to reveal
    /// `environment` themselves ([`Refused`] when they can't). The audit row is `consumer` for
    /// [`Revealed::by`].
    async fn for_owner(&self, owner: &str, project: &str, environment: &str, consumer: &str, who: Asker<'_>) -> anyhow::Result<Revealed>;
    /// Whether what `by` copied out of `environment` of `project` (a [`Revealed`]'s) may still be
    /// in a sandbox: they still reach the project and may reveal it. Asked before each start;
    /// [`Refused`] when they can't. `None`: a sandbox from before its source was kept, whose
    /// copier must still be able to act at all.
    async fn still_reveals(&self, by: &str, source: Option<(&str, &str)>) -> anyhow::Result<()> {
        let _ = (by, source);
        Err(Refused("roll secrets can't tell whether its copier still may".into()).into())
    }
    /// (candidate index, key, environment, folder) for every secret, in any environment of the
    /// projects that have repo `owner/name` on their canvas, whose value is one of `candidates`
    /// (matched by fingerprint; values never come back). Empty for a repo on no project's canvas.
    async fn leaks(&self, repo: &str, candidates: Vec<String>) -> anyhow::Result<Vec<(usize, String, String, String)>>;
    /// Of `keys`, the ones a standard environment of the project that has repo `owner/name` on its
    /// canvas doesn't define, with those environments: (key, [environment]). Names only.
    async fn missing(&self, repo: &str, keys: Vec<String>) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        let _ = (repo, keys);
        Ok(Vec::new())
    }
    /// Where a credential found in a change of repo `owner/name` would be stored ("Store in
    /// roll"): the project roll secrets trusts the repo for, as `asker` sees it, and its
    /// environments, suggesting `prefer` (what the repo's `roll.toml` names) when it has it.
    async fn store_target(&self, repo: &str, asker: &SecretsAsker, prefer: Option<&str>) -> crate::Result<StoreTarget> {
        let _ = (repo, asker, prefer);
        Err(crate::Error::Unprocessable("roll secrets isn't on this server".into()))
    }
    /// Store a credential read from a change as a new key, as `asker` (who needs `write` in the
    /// folder): one change, or a change request where the folder needs approval.
    async fn store(&self, repo: &str, asker: &SecretsAsker, credential: StoreCredential) -> crate::Result<StoredCredential> {
        let _ = (repo, asker, credential);
        Err(crate::Error::Unprocessable("roll secrets isn't on this server".into()))
    }
    /// Whether credentials a change of repo `owner/name` adds work, asked of their providers when
    /// a project with the repo on its canvas has roll secrets' validity checks on (docs/secrets.md,
    /// "Is it live?"): (value, the AWS secret access key next to an access key id) in, an answer
    /// per credential (by fingerprint) out; empty when checks are off. Waits for the answers: call
    /// it from background work. The values go nowhere but the providers.
    async fn validity(&self, repo: &str, found: Vec<(String, Option<String>)>) -> anyhow::Result<Vec<crate::model::CredentialValidity>> {
        let _ = (repo, found);
        Ok(Vec::new())
    }
    /// `asker` sets owner `owner`'s (its name) reviewer provider key to `reference`, a `roll://`
    /// reference (`roll_secrets::Reference::parse` said so): roll secrets checks they may reveal it
    /// for `code:review:<owner>` (the project whose canvas holds one of the owner's repositories
    /// and stands for it) and answers who to keep as its maker. [`crate::Error::Forbidden`] when
    /// they may not, [`crate::Error::Invalid`] when it can't resolve.
    async fn bind_owner_key(&self, owner: &str, reference: &str, asker: &SecretsAsker) -> crate::Result<String> {
        let _ = (owner, reference, asker);
        Err(crate::Error::Unprocessable("roll:// keys need roll secrets on this server".into()))
    }
    /// That key's value now, read as `maker` may (re-checked on every read, audited as
    /// `code:review:<owner>`). The error says why it can't be read (its maker can't reveal it any
    /// more, it's gone…), never a value.
    async fn owner_key(&self, owner: &str, reference: &str, maker: &str) -> anyhow::Result<CredentialValue> {
        let _ = (owner, reference, maker);
        anyhow::bail!("roll secrets isn't on this server")
    }
}

/// Who asks roll secrets, as rolld's gate put them on the request: roll secrets decides with them
/// as it would a request of its own (a roll-access grant first, then a signed-in person, then the
/// operator).
#[derive(Clone, Debug, Default)]
pub struct SecretsAsker {
    pub operator: bool,
    pub user: Option<roll_auth::SignedIn>,
    pub grant: Option<roll_auth::Grant>,
}

impl SecretsAsker {
    pub fn of(ext: &axum::http::Extensions) -> SecretsAsker {
        SecretsAsker { operator: roll_auth::Operator::on(ext), user: ext.get::<roll_auth::SignedIn>().cloned(), grant: roll_auth::Grant::on(ext).cloned() }
    }
}

/// Where a found credential would go ([`SecretEnv::store_target`]).
#[derive(Clone, Debug, PartialEq)]
pub struct StoreTarget {
    pub project: String,
    pub project_name: String,
    pub environments: Vec<String>,
    /// The one to suggest; none when the project has neither `roll.toml`'s nor `development`.
    pub environment: Option<String>,
}

/// A credential to store ([`SecretEnv::store`]).
#[derive(Debug)]
pub struct StoreCredential {
    pub key: String,
    /// None: [`StoreTarget::environment`].
    pub environment: Option<String>,
    pub prefer: Option<String>,
    pub path: String,
    pub value: CredentialValue,
    pub message: String,
}

/// A credential's value on its way to roll secrets: never printed (`Debug` redacts it).
pub struct CredentialValue(pub String);

impl std::fmt::Debug for CredentialValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialValue(<redacted>)")
    }
}

/// What [`SecretEnv::store`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredCredential {
    pub project: String,
    pub environment: String,
    pub path: String,
    /// The change's number in the environment.
    pub change: Option<i64>,
    /// The change request opened instead (where the folder needs approval).
    pub request: Option<String>,
}
