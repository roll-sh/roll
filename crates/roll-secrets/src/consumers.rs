//! Other roll products reading a secret by reference (docs/secrets.md, "Other roll products"): a
//! product that needs an outside credential (storage's replication targets) keeps
//! `roll://ENV[/folder]/KEY` instead of a sealed copy, and asks for the value each time it opens
//! the connection, as 1Password's `op://` and Vault Agent do. rolld hands products this in
//! process; roll-secrets knows no product, only the workspace resource asking.
//!
//! **Trust**, as roll apps' services (`ForApps` in rolld): the project is the one whose canvas
//! holds the resource *and* that `Workspace::trusts` for it, never membership alone; a reference
//! may name a project (`roll://PROJECT@ENV/KEY`) only if it's that one. The resource then reads
//! as its project's runtime, protected environments included, except that an ephemeral
//! environment that isn't trusted never reaches a protected one (its base, an import or a
//! reference): what only comes from there is refused as protected. No dynamic credential: a
//! product keeps its connection past any lease.
//!
//! **Untrusted copies.** A resource whose code or data didn't come through its owner (a search
//! branch keeps its cluster's id, as a preview keeps its service's) asks with
//! [`ResourceRef::untrusted`]: it then reads as checks, sandboxes and previews do
//! (`Secrets::untrusted`, not a runtime), so a protected environment is refused as protected and a
//! sensitive key as unusable. The hint only narrows; nothing widens a resource past its project's
//! runtime.
//!
//! Every read is audited as `secrets.resolve` by the resource (`storage:bucket:<ref>`, with
//! ` (untrusted)` after it for an untrusted copy). Nothing is cached here: a product polls
//! [`Secrets::digest_for`] (no value, not audited; refused exactly as the resolve would be) and
//! resolves again when it moves.

use crate::access::Caller;
use crate::{Error, Secrets, Value, names};
use roll_workspace::Kind;

/// The product resource asking, as on the workspace canvas: its kind and ref, and whether it's an
/// untrusted copy (private: set only by [`ResourceRef::untrusted`], so it can't be cleared).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRef {
    pub kind: Kind,
    pub r#ref: String,
    untrusted: bool,
}

impl ResourceRef {
    pub fn new(kind: Kind, r#ref: impl Into<String>) -> ResourceRef {
        ResourceRef { kind, r#ref: r#ref.into(), untrusted: false }
    }

    /// The same resource as a copy nobody vetted (a branch, a clone, anything whose code or data
    /// didn't come through the resource's owner): it reads as an untrusted caller, never a
    /// protected environment or a sensitive key. A restore of the resource itself stays trusted.
    pub fn untrusted(mut self) -> ResourceRef {
        self.untrusted = true;
        self
    }

    pub fn is_untrusted(&self) -> bool {
        self.untrusted
    }

    /// Who it is in the audit log: `storage:bucket:<ref>`, `search:search:<ref>`, …, with
    /// ` (untrusted)` after it for an untrusted copy.
    pub fn consumer(&self) -> String {
        let product = match self.kind {
            Kind::Bucket => "storage",
            Kind::Stream => "streams",
            Kind::Queue => "queues",
            Kind::Repo => "code",
            Kind::Service => "apps",
            k => k.as_str(),
        };
        let untrusted = if self.untrusted { " (untrusted)" } else { "" };
        format!("{product}:{}:{}{untrusted}", self.kind.as_str(), self.r#ref)
    }
}

/// A resolved reference. `value` redacts itself and is wiped on drop.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub value: Value,
    /// The version of the key read.
    pub version: i64,
    /// When the value stops working outside roll, if the key says (ms).
    pub expires_at: Option<i64>,
}

/// Why a reference didn't resolve: which part failed. Never holds a value.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("not a roll reference: {0}")]
    BadReference(String),
    /// No project's canvas holds the resource, or several do and the reference names none.
    #[error("no project: {0}")]
    NoProject(String),
    /// The projects holding it don't stand for it (`Workspace::trusts`), or the reference names
    /// another project.
    #[error("not trusted: {0}")]
    NotTrusted(String),
    #[error("no such key: {0}")]
    NoSuchKey(String),
    /// A protected environment, or only reachable through one, for an untrusted reader (an
    /// untrusted ephemeral environment, or an untrusted [`ResourceRef`]).
    #[error("protected: {0}")]
    Protected(String),
    /// Set, but not usable here (a dynamic credential, a sensitive key for an untrusted reader, a
    /// reference inside it that doesn't resolve).
    #[error("can't be used: {0}")]
    Unusable(String),
    #[error("{0}")]
    Failed(Error),
}

impl From<Error> for Refusal {
    fn from(e: Error) -> Refusal {
        Refusal::Failed(e)
    }
}

/// `roll://[PROJECT@]ENV[/folder…]/KEY`, as roll-cli reads it (docs/secrets.md, "References in files").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub project: Option<String>,
    pub env: String,
    pub path: String,
    pub key: String,
}

impl Reference {
    /// `Some` for a `roll://` value, an error for a malformed one, `None` for anything else.
    pub fn parse(text: &str) -> Result<Option<Reference>, Refusal> {
        let Some(rest) = text.trim().strip_prefix("roll://") else { return Ok(None) };
        let bad = || Refusal::BadReference("a reference is roll://ENV/KEY, roll://ENV/folder/KEY or roll://PROJECT@ENV/KEY".into());
        let (project, rest) = match rest.split_once('@') {
            Some((p, r)) if !p.is_empty() && !p.contains('/') => (Some(p.to_string()), r),
            Some(_) => return Err(bad()),
            None => (None, rest),
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 || !parts.iter().all(|p| names::segment(p)) || names::slug(parts[0]).is_err() || names::key(parts[parts.len() - 1]).is_err() {
            return Err(bad());
        }
        let path = if parts.len() == 2 { "/".to_string() } else { format!("/{}", parts[1..parts.len() - 1].join("/")) };
        let path = names::path(&path).map_err(|_| bad())?;
        Ok(Some(Reference { project, env: parts[0].to_string(), path, key: parts[parts.len() - 1].to_string() }))
    }

    fn required(text: &str) -> Result<Reference, Refusal> {
        Reference::parse(text)?.ok_or_else(|| Refusal::BadReference("it doesn't start with roll://".into()))
    }
}

impl Secrets {
    /// `reference`'s value for `resource`, read as described in the module doc and audited.
    pub fn resolve_for(&self, resource: &ResourceRef, reference: &str) -> Result<Resolved, Refusal> {
        let r = Reference::required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        self.resolve_one(&project, &r.env, &r.path, &r.key, &Caller::consumer(&resource.consumer()), resource.untrusted)
    }

    /// The digest of the folder `reference` reads, under the same rules (no value, not audited):
    /// refused as the resolve would be, and it moves when the value would, so a product polls it
    /// and resolves again only then.
    pub fn digest_for(&self, resource: &ResourceRef, reference: &str) -> Result<String, Refusal> {
        let r = Reference::required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        let (reader, _) = self.consumer_entry(&project, &r.env, &r.path, &r.key, &Caller::consumer(&resource.consumer()), resource.untrusted)?;
        match self.digest(&project, &r.env, &r.path, &reader) {
            Err(Error::NotFound(m)) => Err(Refusal::NoSuchKey(m)),
            d => Ok(d?),
        }
    }

    /// Whether `caller` may reveal the folder `reference` reads for `resource` (in the one project
    /// that holds and stands for it, as [`Secrets::resolve_for`] finds it): what a product checks
    /// before it stores a reference a caller wrote, and with [`Secrets::maker_caller`] before each
    /// resolve after. `Ok(())`, or why not (never a value).
    pub fn may_reveal_for(&self, resource: &ResourceRef, reference: &str, caller: &Caller) -> Result<(), Refusal> {
        let r = Reference::required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        if !caller.allows(crate::access::Action::Reveal, &crate::access::Scope::at(&project, &r.env, &r.path)) {
            return Err(Refusal::NotTrusted(format!("{} may not reveal {}:{}", caller.principal, r.env, r.path)));
        }
        Ok(())
    }

    /// The one project whose canvas holds `resource` and stands for it (and is `named`, if given).
    fn project_for(&self, resource: &ResourceRef, named: Option<&str>) -> Result<String, Refusal> {
        let who = resource.consumer();
        let ws = |e: roll_workspace::Error| Error::Internal(e.to_string());
        let ids = self.workspace.projects_of(resource.kind.as_str(), &resource.r#ref).map_err(ws)?;
        if ids.is_empty() {
            return Err(Refusal::NoProject(format!("{who} isn't on any project's canvas")));
        }
        let mut trusting = vec![];
        for id in &ids {
            match self.workspace.get(roll_workspace::Scope::All, id) {
                Ok(p) if self.workspace.trusts(&p, resource.kind, &resource.r#ref) => trusting.push(p),
                Ok(_) | Err(roll_workspace::Error::NotFound(_)) => {}
                Err(e) => return Err(ws(e).into()),
            }
        }
        let mut found: Vec<_> = match named {
            Some(n) => trusting.into_iter().filter(|p| p.id == n || p.name == n).collect(),
            None => trusting,
        };
        match (found.len(), named) {
            (1, _) => Ok(found.remove(0).id),
            (0, Some(n)) => Err(Refusal::NotTrusted(format!("project {n} doesn't hold {who} on its canvas, or doesn't stand for it"))),
            (0, None) => Err(Refusal::NotTrusted(format!("no project whose canvas holds {who} stands for it"))),
            (n, Some(name)) => Err(Refusal::NoProject(format!("{n} projects named {name} hold {who}: name one by its prj_… id"))),
            (n, None) => Err(Refusal::NoProject(format!("{who} is on {n} projects' canvases: name one, as roll://PROJECT@ENV/KEY"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_read_as_roll_cli_reads_them() {
        let r = |v: &str| Reference::parse(v).unwrap().unwrap();
        assert_eq!(r("roll://production/AWS_SECRET_ACCESS_KEY"), Reference { project: None, env: "production".into(), path: "/".into(), key: "AWS_SECRET_ACCESS_KEY".into() });
        assert_eq!(r("roll://shop@staging/payments/stripe/KEY").path, "/payments/stripe");
        assert_eq!(r("roll://shop@staging/KEY").project.as_deref(), Some("shop"));
        assert!(Reference::parse("not-a-reference").unwrap().is_none());
        for bad in ["roll://production", "roll://prod/../KEY", "roll://@prod/KEY", "roll://prod//KEY"] {
            assert!(matches!(Reference::parse(bad), Err(Refusal::BadReference(_))), "{bad}");
        }
        assert_eq!(ResourceRef::new(Kind::Bucket, "photos").consumer(), "storage:bucket:photos");
        assert_eq!(ResourceRef::new(Kind::Bucket, "photos").untrusted().consumer(), "storage:bucket:photos (untrusted)");
    }
}
