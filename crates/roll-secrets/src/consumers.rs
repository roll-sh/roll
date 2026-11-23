//! Other roll products reading a secret by reference (docs/secrets.md, "Other roll products"): a
//! product that needs an outside credential keeps `roll://ENV[/folder]/KEY` instead of a sealed
//! copy, and asks for the value each time it opens the connection, as 1Password's `op://` and
//! Vault Agent do. rolld hands products this in process; roll-secrets knows no product, only the
//! workspace resource asking.
//!
//! **Who chose the reference** decides which call a product makes:
//! - A person or a token chose it (search's snapshot repositories): [`Secrets::bind_ref`] when
//!   it's written, with the request's caller, returns a [`Maker`] the product stores beside it;
//!   every read is [`Secrets::resolve_as`] / [`Secrets::digest_as`] with that maker. The reference
//!   reads as its maker, re-checked on every use (an admin while still one, a person while they
//!   can sign in and own the project, a token while it lives), never a runtime: no sensitive key,
//!   imports and bases only where the maker may reveal, no protected environment for an untrusted
//!   copy. Otherwise anyone who may write a product's settings could send any value the project's
//!   runtime reads to a host of their choosing.
//! - A deployer chose it for the resource's own runtime (roll apps' git login, a deploy token a
//!   person pasted, tagged sensitive): [`Secrets::bind_ref_runtime`] with the one environment the
//!   product checked they may deploy to. A reference to exactly that environment is a runtime
//!   binding (`usr_…@runtime:production`): it reads as the resource's runtime there, sensitive keys
//!   included, while its maker can sign in, still reaches the project and may reveal the folder.
//!   A deployer can already run code that reads them, so this adds no reach. A reference to any
//!   other environment binds as above.
//! - The operator wrote it (storage's replication targets, an operator-only API):
//!   [`Secrets::resolve_for`] / [`Secrets::digest_for`] read as the project's runtime, sensitive
//!   keys included.
//!
//! **Trust**, as roll apps' services (`ForApps` in rolld): the project is the one whose canvas
//! holds the resource *and* that `Workspace::trusts` for it, never membership alone; a reference
//! may name a project (`roll://PROJECT@ENV/KEY`) only if it's that one. The resource then reads
//! as its project's runtime, protected environments included, except that an ephemeral
//! environment that isn't trusted never reaches a protected one (its base, an import or a
//! reference): what only comes from there is refused as protected. No dynamic credential: a
//! product keeps its connection past any lease.
//!
//! **A roll code owner's own settings** (its reviewer's provider key; [`ResourceRef::code_owner`]):
//! no canvas holds an owner, so its project is the one whose canvas holds one of the owner's
//! repositories and stands for it (`Workspace::trusts` for that repo, as its checks read), named
//! in the reference when there are several. Audited as `code:review:<owner>`. One of the owner's
//! agent machines (an endpoint's key; [`ResourceRef::agent_machine`]) reads through the same
//! project, audited as `agent:<machine>`.
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
use crate::{Error, Secrets, Value};
use roll_workspace::Kind;

/// The product resource asking, as on the workspace canvas: its kind and ref, and whether it's an
/// untrusted copy (private: set only by [`ResourceRef::untrusted`], so it can't be cleared).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRef {
    pub kind: Kind,
    pub r#ref: String,
    untrusted: bool,
    placeholders: bool,
    /// Not a canvas resource: something of a roll code owner's, read through the project that
    /// stands for the owner.
    owner: Option<OwnersOwn>,
}

/// What of a roll code owner's reads a reference ([`ResourceRef::code_owner`],
/// [`ResourceRef::agent_machine`]): the owner's name, and what of theirs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OwnersOwn {
    Reviewer(String),
    Machine(String),
}

impl OwnersOwn {
    fn owner(&self) -> &str {
        match self {
            OwnersOwn::Reviewer(o) | OwnersOwn::Machine(o) => o,
        }
    }
}

impl ResourceRef {
    pub fn new(kind: Kind, r#ref: impl Into<String>) -> ResourceRef {
        ResourceRef { kind, r#ref: r#ref.into(), untrusted: false, placeholders: false, owner: None }
    }

    /// roll code owner `owner`'s reviewer (its provider key): `code:review:<owner>`, read through
    /// the project whose canvas holds one of the owner's repositories and stands for it.
    pub fn code_owner(owner: impl Into<String>) -> ResourceRef {
        let owner = owner.into();
        ResourceRef { owner: Some(OwnersOwn::Reviewer(owner.clone())), ..ResourceRef::new(Kind::Repo, owner) }
    }

    /// roll code owner `owner`'s agent machine `machine` (its name; an endpoint's key):
    /// `agent:<machine>`, read through the same project as [`ResourceRef::code_owner`].
    pub fn agent_machine(owner: impl Into<String>, machine: impl Into<String>) -> ResourceRef {
        ResourceRef { owner: Some(OwnersOwn::Machine(owner.into())), ..ResourceRef::new(Kind::Repo, machine) }
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

    /// A resource whose requests go out through roll's egress proxy: a key bound to hosts
    /// resolves as a placeholder the proxy swaps (docs/secrets.md, "Using a key without seeing
    /// it"), sensitive or not, so the resource never holds its value.
    pub fn with_placeholders(mut self) -> ResourceRef {
        self.placeholders = true;
        self
    }

    pub fn wants_placeholders(&self) -> bool {
        self.placeholders
    }

    /// Who it is in the audit log: `storage:bucket:<ref>`, `search:search:<ref>`, …, with
    /// ` (untrusted)` after it for an untrusted copy.
    pub fn consumer(&self) -> String {
        match &self.owner {
            Some(OwnersOwn::Reviewer(o)) => return format!("code:review:{o}"),
            Some(OwnersOwn::Machine(_)) => return format!("agent:{}", self.r#ref),
            None => {}
        }
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

/// Who chose a reference a product holds ([`Secrets::bind_ref`]): opaque, stored by the product
/// as it serializes (a string), handed back to each [`Secrets::resolve_as`] and
/// [`Secrets::digest_as`], which look its rights up again. It's who to name when a reference stops
/// working ("usr_… set it and can't reach it any more"), never a credential. A runtime binding
/// ([`Secrets::bind_ref_runtime`]) is the maker with its environment after it,
/// `usr_…@runtime:production`; a product stores one only as `bind_ref*` returned it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Maker(String);

/// Between a runtime binding's maker and its environment (neither a maker nor a slug holds `@`).
const RUNTIME: &str = "@runtime:";

impl Maker {
    /// Who chose it, and the one environment it reads as the resource's runtime, if it's a
    /// runtime binding.
    fn parts(&self) -> (&str, Option<&str>) {
        match self.0.split_once(RUNTIME) {
            Some((by, env)) => (by, Some(env)),
            None => (&self.0, None),
        }
    }
}

impl std::fmt::Display for Maker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.parts().0)
    }
}

/// A resolved reference. `value` redacts itself and is wiped on drop.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The value, or for a key bound to hosts that a resource asked placeholders for, the
    /// placeholder (`rlph_…`) its proxy swaps.
    pub value: Value,
    /// The placeholder's record, when `value` is one.
    pub placeholder: Option<crate::placeholders::Placeholder>,
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
    /// Whoever chose the reference may not reveal it ([`Secrets::bind_ref`]'s caller).
    #[error("not allowed: {0}")]
    NotAllowed(String),
    /// Whoever chose it (a [`Maker`]) can no longer reveal it, or sign in: demoted, disabled,
    /// revoked or no longer on the project ([`crate::Error::MakerUnreachable`] over HTTP).
    #[error("maker unreachable: {0}")]
    MakerUnreachable(String),
    #[error("no such key: {0}")]
    NoSuchKey(String),
    /// A protected environment, or only reachable through one, for an untrusted reader (an
    /// untrusted ephemeral environment, or an untrusted [`ResourceRef`]).
    #[error("protected: {0}")]
    Protected(String),
    /// Set, but not usable here (a dynamic credential, a sensitive key for any reader but a
    /// trusted runtime, a reference inside it that doesn't resolve).
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

/// `roll://[PROJECT@]ENV[/folder…]/KEY`, as roll-cli reads it (docs/secrets.md, "References in
/// files"): `Reference::parse` is the one parser every product uses. It lives in
/// `roll-secrets-ref` (no I/O) so the agents' wire can use it without roll secrets.
pub use roll_secrets_ref::Reference;

impl From<roll_secrets_ref::BadReference> for Refusal {
    fn from(e: roll_secrets_ref::BadReference) -> Refusal {
        Refusal::BadReference(e.0)
    }
}

/// A reference, or why `text` isn't one.
fn required(text: &str) -> Result<Reference, Refusal> {
    Reference::parse(text)?.ok_or_else(|| Refusal::BadReference("it doesn't start with roll://".into()))
}

impl Secrets {
    /// Check that `caller` (the request writing `reference` into `resource`'s settings) may
    /// choose it, and return who to store as its [`Maker`]: the caller may reveal its folder in
    /// the one project that holds and stands for `resource`, and it resolves for `resource` read
    /// as they are ([`Secrets::digest_as`]): a sensitive key, a key only a protected environment
    /// holds for an untrusted copy, or one reached only through what they can't reveal is refused
    /// here, not at the first use. An admin's session is stored as the person; a workload
    /// identity's login is refused (it ends with its token). Not audited (nothing is read).
    pub fn bind_ref(&self, resource: &ResourceRef, reference: &str, caller: &Caller) -> Result<Maker, Refusal> {
        self.bind(resource, reference, caller, None)
    }

    /// [`Secrets::bind_ref`] for a reference the resource's runtime reads in `runtime_env` (roll
    /// apps' git login). **Contract:** the product calls it only after its own check that `caller`
    /// holds deploy on `resource` in `runtime_env` (whoever may deploy there can already run code
    /// that reads it); anything else calls `bind_ref`. A reference to exactly `runtime_env` is a
    /// runtime binding: sensitive keys are allowed there, as for the resource's own runtime read,
    /// within its trust (its one trusted project; an untrusted copy still gets no protected
    /// environment or sensitive key). Every read still re-checks that the maker can sign in,
    /// reaches the project and may reveal the folder. A reference to any other environment binds
    /// exactly as `bind_ref` does. A workload identity's login is refused, as there.
    pub fn bind_ref_runtime(&self, resource: &ResourceRef, reference: &str, caller: &Caller, runtime_env: &str) -> Result<Maker, Refusal> {
        self.bind(resource, reference, caller, Some(runtime_env))
    }

    fn bind(&self, resource: &ResourceRef, reference: &str, caller: &Caller, runtime_env: Option<&str>) -> Result<Maker, Refusal> {
        let by = self.maker_of(caller)?;
        let r = required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        self.may_reveal_for(&project, &r, caller)?;
        let maker = match runtime_env {
            Some(env) if env == r.env => Maker(format!("{by}{RUNTIME}{env}")),
            _ => Maker(by),
        };
        self.digest_as(resource, reference, &maker)?;
        Ok(maker)
    }

    /// `reference`'s value for `resource`, read as `maker` may now (see the module doc), audited
    /// as `secrets.resolve` by the resource, naming the maker.
    pub fn resolve_as(&self, resource: &ResourceRef, reference: &str, maker: &Maker) -> Result<Resolved, Refusal> {
        let (project, r, caller) = self.as_maker(resource, reference, maker)?;
        self.resolve_one(&project, &r.env, &r.path, &r.key, &caller, resource.untrusted, resource)
    }

    /// The digest of the folder `reference` reads, as `maker` may now: refused exactly as
    /// [`Secrets::resolve_as`] would be, not audited, no value.
    pub fn digest_as(&self, resource: &ResourceRef, reference: &str, maker: &Maker) -> Result<String, Refusal> {
        let (project, r, caller) = self.as_maker(resource, reference, maker)?;
        self.digest_one(&project, &r, &caller, resource)
    }

    /// The project, the reference, and the caller a read as `maker` runs as: the maker's rights
    /// now, named for the resource, not a runtime, and allowed to reveal the folder. A runtime
    /// binding then reads its one environment as the resource's runtime, named
    /// `<resource> (runtime read for <maker>)`, while the maker still reaches the project.
    fn as_maker(&self, resource: &ResourceRef, reference: &str, maker: &Maker) -> Result<(String, Reference, Caller), Refusal> {
        let r = required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        let out = self.as_maker_in(resource, &project, &r, maker);
        if let Err(Refusal::MakerUnreachable(why)) = &out {
            self.note_unreachable(&project, &r.env, &r.path, maker.parts().0, &resource.consumer(), why);
        }
        out.map(|c| (project, r, c))
    }

    fn as_maker_in(&self, resource: &ResourceRef, project: &str, r: &Reference, maker: &Maker) -> Result<Caller, Refusal> {
        let (by, runtime_env) = maker.parts();
        let gone = || Refusal::MakerUnreachable(format!("{by}, who chose it, can no longer reveal {}:{} or can no longer sign in", r.env, r.path));
        let caller = self.maker_caller(by, &resource.consumer()).map_err(|e| match e {
            Error::Forbidden(_) => gone(),
            e => e.into(),
        })?;
        self.may_reveal_for(project, r, &caller).map_err(|e| match e {
            Refusal::NotAllowed(_) => gone(),
            e => e,
        })?;
        let Some(env) = runtime_env else { return Ok(caller.not_runtime()) };
        if r.env != env {
            return Err(Refusal::NotAllowed(format!("{by} chose this for {env}'s runtime, not {}'s", r.env)));
        }
        // While its maker may still reveal what it reads, as they are now (a member removed or
        // demoted, or no longer an admin, stops it at once).
        if !self.maker_reaches(by, &[(crate::access::Action::Reveal, crate::access::Scope::at(project, &r.env, &r.path))])? {
            return Err(gone());
        }
        Ok(Caller::consumer(&format!("{} (runtime read for {by})", resource.consumer())))
    }

    /// Whether `caller` may reveal `r`'s folder in `project`. Never says more than a reveal's 403.
    pub(crate) fn may_reveal_for(&self, project: &str, r: &Reference, caller: &Caller) -> Result<(), Refusal> {
        if caller.allows(crate::access::Action::Reveal, &crate::access::Scope::at(project, &r.env, &r.path)) {
            return Ok(());
        }
        let who = caller.maker.as_deref().unwrap_or(&caller.principal);
        Err(Refusal::NotAllowed(format!("{who} may not reveal {}:{}", r.env, r.path)))
    }

    /// `reference`'s value for `resource`, read as described in the module doc and audited.
    pub fn resolve_for(&self, resource: &ResourceRef, reference: &str) -> Result<Resolved, Refusal> {
        let r = required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        self.resolve_one(&project, &r.env, &r.path, &r.key, &Caller::consumer(&resource.consumer()), resource.untrusted, resource)
    }

    /// The digest of the folder `reference` reads, under the same rules (no value, not audited):
    /// refused as the resolve would be, and it moves when the value would, so a product polls it
    /// and resolves again only then.
    pub fn digest_for(&self, resource: &ResourceRef, reference: &str) -> Result<String, Refusal> {
        let r = required(reference)?;
        let project = self.project_for(resource, r.project.as_deref())?;
        self.digest_one(&project, &r, &Caller::consumer(&resource.consumer()), resource)
    }

    fn digest_one(&self, project: &str, r: &Reference, caller: &Caller, resource: &ResourceRef) -> Result<String, Refusal> {
        let (reader, _, _) = self.consumer_entry(project, &r.env, &r.path, &r.key, caller, resource.untrusted, resource.wants_placeholders())?;
        match self.digest(project, &r.env, &r.path, &reader) {
            Err(Error::NotFound(m)) => Err(Refusal::NoSuchKey(m)),
            d => Ok(d?),
        }
    }

    /// The one project whose canvas holds `resource` and stands for it (and is `named`, if given).
    pub(crate) fn project_for(&self, resource: &ResourceRef, named: Option<&str>) -> Result<String, Refusal> {
        let who = resource.consumer();
        let ws = |e: roll_workspace::Error| Error::Internal(e.to_string());
        let mut trusting = vec![];
        if let Some(own) = &resource.owner {
            // One of the owner's repositories on its canvas, which it stands for.
            let owner = own.owner();
            let prefix = format!("{owner}/");
            let theirs = |p: &roll_workspace::Project| p.resources.iter().any(|x| x.kind == Kind::Repo && x.r#ref.starts_with(&prefix) && self.workspace.trusts(p, Kind::Repo, &x.r#ref));
            trusting = self.workspace.list(roll_workspace::Scope::All).map_err(ws)?.into_iter().filter(theirs).collect();
            if trusting.is_empty() {
                return Err(Refusal::NoProject(format!("no project's canvas holds one of {owner}'s repositories and stands for it")));
            }
        } else {
            let ids = self.workspace.projects_of(resource.kind.as_str(), &resource.r#ref).map_err(ws)?;
            if ids.is_empty() {
                return Err(Refusal::NoProject(format!("{who} isn't on any project's canvas")));
            }
            for id in &ids {
                match self.workspace.get(roll_workspace::Scope::All, id) {
                    Ok(p) if self.workspace.trusts(&p, resource.kind, &resource.r#ref) => trusting.push(p),
                    Ok(_) | Err(roll_workspace::Error::NotFound(_)) => {}
                    Err(e) => return Err(ws(e).into()),
                }
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
    fn resources_are_named_for_the_audit_log() {
        // A malformed reference is a refusal of its own (the parser's tests are in roll-secrets-ref).
        assert!(matches!(required(" roll://prod/KEY"), Err(Refusal::BadReference(_))));
        assert!(matches!(required("plain"), Err(Refusal::BadReference(_))));
        assert_eq!(ResourceRef::new(Kind::Bucket, "photos").consumer(), "storage:bucket:photos");
        assert_eq!(ResourceRef::new(Kind::Bucket, "photos").untrusted().consumer(), "storage:bucket:photos (untrusted)");
        assert_eq!(ResourceRef::code_owner("acme").consumer(), "code:review:acme");
        assert_eq!(ResourceRef::agent_machine("acme", "acme").consumer(), "agent:acme");
    }
}
