//! Who is calling and what they may do (docs/secrets.md, "Access").
//!
//! The roll-wide permission model (docs/permissions.md) classifies each request in rolld's gate.
//! Inside the product, the checks that the gate can't make from the request alone (a second
//! environment, a path inside a batch, each finding of a filtered answer) go through [`Grants`]:
//! the operator (everything), a roll-access grant (`rlt_…`/`rlo_…`, [`GrantGrants`]; a person's
//! own within their session, [`Within`]) or a signed-in person, whose session is the grant of their
//! memberships' roles (docs/members.md), decided the same way.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Names, metadata, versions, history; never a value.
    Read,
    /// Values.
    Reveal,
    Write,
    Delete,
    /// Environment settings, protection, reviewing requests.
    Admin,
}

impl Action {
    pub const ALL: [Action; 5] = [Action::Read, Action::Reveal, Action::Write, Action::Delete, Action::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Reveal => "reveal",
            Action::Write => "write",
            Action::Delete => "delete",
            Action::Admin => "admin",
        }
    }
}

/// What an action is on: a project, and optionally one environment and folder in it.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub project: &'a str,
    pub env: Option<&'a str>,
    pub path: Option<&'a str>,
    /// About the project as a whole and not filtered per environment or folder afterwards (its
    /// shares, required keys, rotators, the keys other environments have): only a rule over all of
    /// it reaches it. A plain project-wide read answers per environment and folder instead.
    pub whole: bool,
}

impl<'a> Scope<'a> {
    pub fn project(project: &'a str) -> Self {
        Scope { project, env: None, path: None, whole: false }
    }

    /// The project as a whole, unfiltered (see [`Scope::whole`]).
    pub fn whole(project: &'a str) -> Self {
        Scope { project, env: None, path: None, whole: true }
    }

    pub fn env(project: &'a str, env: &'a str) -> Self {
        Scope { project, env: Some(env), path: None, whole: false }
    }

    pub fn at(project: &'a str, env: &'a str, path: &'a str) -> Self {
        Scope { project, env: Some(env), path: Some(path), whole: false }
    }
}

/// What a caller may do.
pub trait Grants: Send + Sync + std::fmt::Debug {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool;
    /// The one project the caller is limited to, if any (`whoami`, and the CLI's default).
    fn project(&self) -> Option<&str> {
        None
    }
    /// A consumer running code nobody vetted (`Secrets::untrusted`): not even a folder another
    /// project shares reaches it from a protected environment.
    fn untrusted(&self) -> bool {
        false
    }
    /// Whether the caller may read repository `repo`'s (`owner/name`, on `project`'s canvas) code:
    /// health's `code` check says something about a repository's contents. roll-access answers from
    /// the caller's Grant (`code`, the repo in that project, `read`); here only the operator may.
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        let _ = (project, repo);
        false
    }
    /// Whether the caller may know `project` exists: a person's projects only (those they're a
    /// member of), so anyone else's answers 404, as the workspace does.
    fn sees(&self, project: &str) -> bool {
        let _ = project;
        true
    }
    /// The operator itself: what's about the whole server (the deployment key) is only theirs.
    fn everything(&self) -> bool {
        false
    }
    /// Whether the caller may `action` encryption key `name` of `project` (roll-access's rules
    /// can name one key: `{id: "prj_…", index: "<name>"}`). Others answer for the whole project.
    fn reaches_key(&self, action: Action, project: &str, name: &str) -> bool {
        let _ = name;
        self.allows(action, &Scope::project(project))
    }
}

/// Whether `repo` (`owner/name`) is under a person's own roll code owner.
pub fn owns_repo(code_owner: Option<&str>, repo: &str) -> bool {
    code_owner.is_some_and(|o| repo.split_once('/').is_some_and(|(owner, _)| owner == o))
}

/// Which of a project's environments are protected, as roll secrets has them now: what a rule
/// limited to unprotected environments (`roll_auth::scope::Rule::unprotected`, a developer's
/// default) reaches. Asked once per environment for one request's checks, never kept past it.
#[derive(Clone, Default)]
pub struct Protected {
    lookup: Option<Lookup>,
    seen: std::sync::Arc<std::sync::Mutex<Seen>>,
}

/// Whether `(project, env)` is protected (`None`: not known).
type Lookup = std::sync::Arc<dyn Fn(&str, &str) -> Option<bool> + Send + Sync>;
/// What one request asked already, by `(project, env)`.
type Seen = std::collections::HashMap<(String, String), Option<bool>>;

impl Protected {
    pub fn of(f: impl Fn(&str, &str) -> Option<bool> + Send + Sync + 'static) -> Protected {
        Protected { lookup: Some(std::sync::Arc::new(f)), seen: Default::default() }
    }

    /// Whether `project`'s environment `env` is protected (`None`: not known).
    pub fn get(&self, project: &str, env: &str) -> Option<bool> {
        let f = self.lookup.as_ref()?;
        let key = (project.to_string(), env.to_string());
        if let Some(v) = self.seen.lock().expect("protected").get(&key) {
            return *v;
        }
        let v = f(project, env);
        self.seen.lock().expect("protected").insert(key, v);
        v
    }
}

impl std::fmt::Debug for Protected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Protected")
    }
}

/// People's accounts as they are now (docs/accounts.md; rolld asks roll-accounts). What a person
/// made that acts later (a sync, a token, an identity) acts only while they can still sign in.
pub trait People: Send + Sync {
    /// `None` when the account is gone or disabled; an error when it couldn't be looked up.
    fn person(&self, user: &str) -> Result<Option<Person>, String>;
    /// How many accounts are admins now (they reveal everything; reach counts them). `None`:
    /// nothing tells.
    fn admins(&self) -> Result<Option<usize>, String> {
        Ok(None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub admin: bool,
    /// Their roll code owner's name, if they have one.
    pub code_owner: Option<String>,
}

/// A person's user id (`usr_…`), as a principal.
pub fn is_person(principal: &str) -> bool {
    principal.starts_with("usr_")
}

/// A person's own token, acting as them: what its rules allow, and only what their session's grant
/// (their memberships now) allows too: a person's token never reaches further than their session.
#[derive(Debug)]
pub struct Within(pub GrantGrants, pub GrantGrants);

impl Grants for Within {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.1.allows(action, at) && self.0.allows(action, at)
    }
    fn project(&self) -> Option<&str> {
        self.0.project().or_else(|| self.1.project())
    }
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        self.1.reads_code(project, repo) && self.0.reads_code(project, repo)
    }
    fn sees(&self, project: &str) -> bool {
        self.1.sees(project)
    }
    fn reaches_key(&self, action: Action, project: &str, name: &str) -> bool {
        self.0.reaches_key(action, project, name) && self.1.reaches_key(action, project, name)
    }
}

/// The operator: everything.
#[derive(Debug)]
pub struct Everything;

impl Grants for Everything {
    fn allows(&self, _: Action, _: &Scope<'_>) -> bool {
        true
    }
    fn everything(&self) -> bool {
        true
    }
    fn reads_code(&self, _: &str, _: &str) -> bool {
        true
    }
}

/// Everything except the environments it names: what checks and sandboxes get, so a protected
/// environment never reaches them, through a reference, an import or a base either.
#[derive(Debug)]
pub struct Except(pub Vec<String>);

impl Grants for Except {
    fn allows(&self, _: Action, at: &Scope<'_>) -> bool {
        at.env.is_none_or(|e| !self.0.iter().any(|x| x == e))
    }
}

/// `0`'s grants, never in `1`'s environments.
#[derive(Debug)]
struct Narrowed(std::sync::Arc<dyn Grants>, Except);

impl Grants for Narrowed {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.1.allows(action, at) && self.0.allows(action, at)
    }
    fn project(&self) -> Option<&str> {
        self.0.project()
    }
    fn untrusted(&self) -> bool {
        true
    }
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        self.0.reads_code(project, repo)
    }
    fn sees(&self, project: &str) -> bool {
        self.0.sees(project)
    }
}

/// One rule of roll-access's model for secrets: `project`, its environments and folders (all
/// when `None`), these actions allowed. What a token made from secrets' page, and a workload
/// identity's login, may do.
pub fn rule(project: &str, environments: Option<Vec<String>>, paths: Option<Vec<String>>, actions: &[Action]) -> roll_auth::scope::Rule {
    use roll_auth::scope;
    let action = |a: Action| match a {
        Action::Read => scope::Action::Read,
        Action::Reveal => scope::Action::Reveal,
        Action::Write => scope::Action::Write,
        Action::Delete => scope::Action::Delete,
        Action::Admin => scope::Action::Admin,
    };
    scope::Rule {
        product: "secrets".into(),
        resources: scope::Resources::Some(vec![scope::Selector::Project { project: project.into() }]),
        environments,
        paths,
        unprotected: false,
        actions: actions.iter().map(|a| (action(*a), scope::Effect::Allow)).collect(),
    }
}

fn scope_action(action: Action) -> roll_auth::scope::Action {
    use roll_auth::scope;
    match action {
        Action::Read => scope::Action::Read,
        Action::Reveal => scope::Action::Reveal,
        Action::Write => scope::Action::Write,
        Action::Delete => scope::Action::Delete,
        Action::Admin => scope::Action::Admin,
    }
}

/// Whether grant `g` stays inside `project`: every rule, of any product, names only this project
/// (`{project}`, or secrets' own `{id}`, which is the project's id), never `*` or another project,
/// and nothing it isn't allowed asks a person instead (`ask_otherwise`). Read from the rules as
/// data, not evaluated: anything it can't prove confined reaches further. An incident's cut-off
/// leaves those to the operator, so one project's admin can't cut off another's.
pub fn confined_to(g: &roll_auth::Grant, project: &str) -> bool {
    roll_auth::roles::confined_to(&g.scope, g.ask_otherwise, project)
}

/// Whether `rule` can be about `project`'s secrets at all: secrets' or every product's, over every
/// project or naming this one (`{project}`, or `{id}`, which is the project's id). Read as data, a
/// superset of the rules that allow anything there: it narrows roll-access's tokens before each is
/// judged ([`Workloads::grants_naming`]).
pub fn rule_may_name(rule: &roll_auth::scope::Rule, project: &str) -> bool {
    use roll_auth::scope::{Resources, Selector};
    (rule.product == "secrets" || rule.product == "*")
        && match &rule.resources {
            Resources::All => true,
            Resources::Some(sel) => sel.iter().any(|s| match s {
                Selector::Project { project: p } => p == project,
                Selector::Id { id, .. } => id == project,
            }),
        }
}

/// Short-lived roll-access tokens for workload identities' logins (rolld: `Access::mint_workload`).
/// The login is the proof, so there's no caller to gate: roll-access checks the rule is within the
/// identity's scope and that its owner can still sign in.
pub trait Workloads: Send + Sync {
    /// A token for `minutes` with `rule`, from `ip_allow` only, owned by `owner` (`None`: the
    /// operator).
    fn mint(&self, name: &str, rule: roll_auth::scope::Rule, ip_allow: Option<Vec<String>>, minutes: u32, owner: Option<&str>) -> crate::Result<Minted>;
    /// End one at once (its identity was deleted).
    fn revoke(&self, id: &str) -> crate::Result<()>;
    /// roll-access's live tokens other than workload logins, as their grants now (reach lists
    /// those that reach a folder). Blocking.
    fn grants(&self) -> crate::Result<Vec<roll_auth::Grant>> {
        Ok(vec![])
    }
    /// Those of [`Workloads::grants`] with a rule that can name `project` ([`rule_may_name`]): the
    /// only ones that may reach it. An implementation narrows before resolving each. Blocking.
    fn grants_naming(&self, project: &str) -> crate::Result<Vec<roll_auth::Grant>> {
        Ok(self.grants()?.into_iter().filter(|g| g.scope.0.iter().any(|r| rule_may_name(r, project))).collect())
    }
    /// A long-lived roll token with `rule`, made for the request `ext` belongs to as `POST
    /// /v1/access/tokens` makes one (its owner the caller; a gateway's). Blocking.
    fn token(&self, ext: &axum::http::Extensions, name: &str, rule: roll_auth::scope::Rule) -> crate::Result<Minted> {
        let _ = (ext, name, rule);
        Err(crate::Error::BadRequest("roll tokens aren't on this server".into()))
    }
    /// Revoke a token [`Workloads::token`] made, saying why. Blocking.
    fn revoke_token(&self, id: &str, why: &str) -> crate::Result<()> {
        let _ = (id, why);
        Ok(())
    }
    /// A decoy token (docs/secrets.md, "Decoys"; roll-access's decoys): it looks as `rule` says and
    /// allows nothing, and roll-access tells roll secrets of each use
    /// ([`crate::Secrets::decoy_used`]). Owned by `owner` (`None`: the operator), who's mailed
    /// on its use. `expires_at` is 0: a decoy doesn't end on its own.
    fn mint_decoy(&self, name: &str, rule: roll_auth::scope::Rule, owner: Option<&str>) -> crate::Result<Minted> {
        let _ = (name, rule, owner);
        Err(crate::Error::BadRequest("decoys aren't on this server".into()))
    }
    /// End a decoy token now (its key was deleted or changed). `Ok` when it's gone, whether or not
    /// it was live.
    fn revoke_decoy(&self, id: &str) -> crate::Result<()> {
        let _ = id;
        Ok(())
    }
    /// Revoke these grants at once, with why and who (an incident's cut-off: rolld calls
    /// `Access::revoke_many`). All or none: an id roll-access doesn't know refuses the lot. At most
    /// [`REVOKE_MAX`] ids. Blocking.
    fn revoke_many(&self, ids: &[String], why: &str, by: &str) -> crate::Result<()>;
}

/// The most grants one [`Workloads::revoke_many`] takes (roll-access's `MAX_REVOKE`).
pub const REVOKE_MAX: usize = 500;

/// A workload token: its grant's id, the secret (shown once) and when it ends (Unix ms).
pub struct Minted {
    pub id: String,
    pub secret: String,
    pub expires_at: i64,
}


/// A roll-access grant (`rlt_…` tokens, OAuth connections, a person's session; docs/permissions.md):
/// rolld's gate classified the request already; this answers the second resources the product
/// checks itself. `1` says which environments are protected, for rules that leave those out.
#[derive(Debug)]
pub struct GrantGrants(pub roll_auth::Grant, pub Protected);

impl Grants for GrantGrants {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        use roll_auth::scope;
        let action = scope_action(action);
        // A project-wide read that answers per environment and folder (its environments' names,
        // health, findings, the audit log) answers only what the grant reaches in each: a rule
        // limited to some of them takes part in it, as roll-access's `filters_itself` routes do.
        // One about the project as a whole (`Scope::whole`) needs a rule over all of it.
        let filtered = at.env.is_none() && action == scope::Action::Read && !at.whole;
        // The resource as rolld's gate saw it: the project is also its id, so a rule naming it by
        // id (`{id: "prj_…"}`, as the builder and Tighten write it) reaches it as one naming the
        // project does.
        let protected = at.env.and_then(|e| self.1.get(at.project, e));
        let r = scope::Resource { product: "secrets".into(), id: Some(at.project.into()), project: Some(at.project.into()), environment: at.env.map(Into::into), protected, path: at.path.map(Into::into), filters_itself: filtered, ..Default::default() };
        // `ask` pauses a whole request at the gate; inside the product it's a no.
        self.0.allows(&r, action) == scope::Outcome::Allow
    }

    /// One key, as rolld's gate judges a request naming it (the project's id, the key as its index).
    fn reaches_key(&self, action: Action, project: &str, name: &str) -> bool {
        use roll_auth::scope;
        let r = scope::Resource { product: "secrets".into(), id: Some(project.into()), project: Some(project.into()), index: Some(name.into()), ..Default::default() };
        self.allows(action, &Scope::project(project)) || self.0.allows(&r, scope_action(action)) == scope::Outcome::Allow
    }

    /// Code read on the repository, as it sits on `project`'s canvas.
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        use roll_auth::scope;
        let r = scope::Resource { product: "code".into(), id: Some(repo.into()), project: Some(project.into()), ..Default::default() };
        self.0.allows(&r, scope::Action::Read) == scope::Outcome::Allow
    }

    /// A person's session knows only the projects they're a member of: anyone else's is a 404.
    fn sees(&self, project: &str) -> bool {
        self.0.kind != roll_auth::GrantKind::Session || roll_auth::roles::names_project(&self.0.scope, project)
    }

    /// The one project its secrets rules name, if they name exactly one.
    fn project(&self) -> Option<&str> {
        use roll_auth::scope::{Resources, Selector};
        let mut found: Option<&str> = None;
        for rule in self.0.scope.0.iter().filter(|r| r.product == "secrets" || r.product == "*") {
            let Resources::Some(sel) = &rule.resources else { return None };
            for s in sel {
                let Selector::Project { project } = s else { return None };
                match found {
                    Some(p) if p != project => return None,
                    _ => found = Some(project),
                }
            }
        }
        found
    }
}

/// The caller of a request, or of an in-process call.
#[derive(Clone, Debug)]
pub struct Caller {
    /// Stable and safe to log: `operator`, `token:tok_…`, `app:<service>`, `preview:<repo>`.
    pub principal: String,
    pub grants: std::sync::Arc<dyn Grants>,
    /// The client's address, for the audit log.
    pub ip: Option<String>,
    /// A machine roll runs or trusts (apps, previews, checks, sandboxes, syncs, a workload
    /// identity's login): the only callers a sensitive key's value reaches.
    pub runtime: bool,
    /// The person's approval that let this request through roll-access's gate (an `ask` approved,
    /// or a rule an approval remembered for a while), for the audit log.
    pub approval: Option<Approval>,
    /// The person behind an admin's session (which acts as the operator) or behind a person's own
    /// token: what's recorded as the maker of something that acts later (a sync, an identity, a
    /// product's reference, a registration), so it follows them, not the token, and stops when
    /// they're demoted or can't sign in. `None`: the principal is the maker.
    pub maker: Option<String>,
    /// Who it is may read past an incident's freeze, where it has Admin ([`Caller::reads_frozen`]):
    /// the operator, a signed-in person, a roll-access token. Private, so only their constructors
    /// set it: a consumer, an untrusted caller, a sync, a login or a share link never has it.
    frozen: bool,
    /// A signed-in person's session or the operator ([`Caller::sees_decoys`]). Private, so only
    /// their constructors set it.
    session: bool,
}

/// An approval from roll-access's gate, as the audit log records it (roll-secrets-chain, so an
/// export is verified with the same type).
pub use roll_secrets_chain::Approval;

/// The audit log's record of roll-access's approval.
pub(crate) fn approval(a: &roll_auth::Approved) -> Approval {
    Approval { id: a.id.to_string(), by: a.by.iter().map(|b| b.to_string()).collect(), until: a.until }
}

/// roll previews' in-process consumer: the operator's equal for an environment's branches.
pub const PREVIEWS: &str = "roll:previews";

impl Caller {
    pub fn operator() -> Caller {
        Caller { principal: "operator".into(), grants: std::sync::Arc::new(Everything), ip: None, runtime: false, approval: None, frozen: true, maker: None, session: true }
    }

    /// An admin's session: the operator, with the person (`usr_…`) as the maker of what it makes.
    pub(crate) fn admin(user: &str) -> Caller {
        Caller { maker: Some(user.into()), ..Caller::operator() }
    }

    /// An in-process consumer (roll apps, previews, sandboxes, tasks): everything the caller
    /// handing out the call may do, named for the audit log. Never past a freeze: the services,
    /// checks and previews reading a leaked folder are what a freeze stops.
    pub fn consumer(name: &str) -> Caller {
        Caller { principal: name.into(), grants: std::sync::Arc::new(Everything), ip: None, runtime: true, approval: None, frozen: false, maker: None, session: false }
    }

    /// A signed-in person (`usr_…`), with what they may do.
    pub(crate) fn person(user: &str, grants: std::sync::Arc<dyn Grants>) -> Caller {
        Caller { principal: user.into(), grants, ip: None, runtime: false, approval: None, frozen: true, maker: None, session: true }
    }

    /// A roll-access token (`token:<id>`), with what its rules allow here.
    pub(crate) fn token(id: &str, grants: std::sync::Arc<dyn Grants>) -> Caller {
        Caller { principal: format!("token:{id}"), grants, ip: None, runtime: false, approval: None, frozen: true, maker: None, session: false }
    }

    /// Something roll runs on someone's behalf that isn't a person or their token (a sync, a
    /// workload identity's login, a share link): not a runtime, never past a freeze.
    pub(crate) fn acting(principal: String, grants: std::sync::Arc<dyn Grants>) -> Caller {
        Caller { principal, grants, ip: None, runtime: false, approval: None, frozen: false, maker: None, session: false }
    }

    /// This caller's access, acting as `principal` ([`Caller::acting`]): a sync runs as its maker.
    /// This caller with `user` behind it (a person's own token): they're the maker of what it
    /// registers ([`Caller::made_by`]).
    pub(crate) fn of_person(mut self, user: Option<&str>) -> Caller {
        self.maker = user.map(str::to_string);
        self
    }

    pub(crate) fn acting_as(self, principal: String) -> Caller {
        Caller { principal, frozen: false, session: false, ..self }
    }

    /// Whether it may be told which keys are decoys (docs/secrets.md, "Decoys"), where it's an admin
    /// ([`crate::Secrets::sees_decoys`]): a signed-in person's session (the dashboard) or the
    /// operator. Never a token, admin or not (`rlt_`, `rlo_`, a workload identity's login), an
    /// in-process consumer, a sync or anything acting for someone: a prompt-injected agent or a
    /// leaked admin token that could tell the decoys would skip them. Planting one needs it too, or
    /// a token could plant one and learn the mark by elimination. A member's session sees them only
    /// where their role manages the project (docs/members.md): a developer or a viewer gets the
    /// answers a normal key's would, as a token does.
    pub fn sees_decoys(&self) -> bool {
        self.session
    }

    /// Whether it reads past an incident's freeze where it's an admin: the operator, a signed-in
    /// person, a roll-access token, never a runtime, code nobody vetted, a sync, a login or a link.
    pub fn reads_frozen(&self) -> bool {
        self.frozen && !self.runtime && !self.grants.untrusted()
    }

    /// Whether it reads `env`'s `path` of `project` while it's frozen (`cutoff.rs`).
    pub(crate) fn passes_freeze(&self, project: &str, env: &str, path: &str) -> bool {
        self.reads_frozen() && self.allows(Action::Admin, &Scope::at(project, env, path))
    }

    /// Whether this caller may learn anything of a row's value: reveal it, see its fingerprint, or
    /// test a guess against it. A sensitive row's only reaches a runtime.
    pub fn sees(&self, project: &str, env: &str, path: &str, tags: &[String]) -> bool {
        (self.runtime || !crate::names::sensitive(tags)) && self.allows(Action::Reveal, &Scope::at(project, env, path))
    }

    /// The same caller, not a runtime: sensitive values are left out (checks, sandboxes).
    pub fn not_runtime(mut self) -> Caller {
        self.runtime = false;
        self
    }

    /// This caller, kept out of `except` (a project's protected environments) whatever it may
    /// otherwise do: an untrusted one ([`Secrets::untrusted`](crate::Secrets::untrusted)), which
    /// never reads past a freeze.
    pub fn without(&self, except: Vec<String>) -> Caller {
        Caller { principal: self.principal.clone(), grants: std::sync::Arc::new(Narrowed(self.grants.clone(), Except(except))), ip: self.ip.clone(), runtime: self.runtime, approval: self.approval.clone(), frozen: false, maker: self.maker.clone(), session: false }
    }

    pub fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.grants.allows(action, at)
    }

    /// As [`Caller::need`], for encryption key `name` of `project`: a grant limited to that key
    /// reaches it too.
    pub fn need_key(&self, action: Action, project: &str, name: &str) -> crate::Result<()> {
        if self.grants.reaches_key(action, project, name) {
            return Ok(());
        }
        self.need(action, &Scope::project(project))
    }

    pub fn need(&self, action: Action, at: &Scope<'_>) -> crate::Result<()> {
        if self.allows(action, at) {
            return Ok(());
        }
        if !self.grants.sees(at.project) {
            return Err(crate::Error::NotFound(format!("no project {}", at.project)));
        }
        let place = match (at.env, at.path) {
            (Some(e), Some(p)) if p != "/" => format!("{e}:{p}"),
            (Some(e), _) => e.to_string(),
            _ => at.project.to_string(),
        };
        Err(crate::Error::Forbidden(format!("this token may not {} in {place}", action.as_str())))
    }

    /// Who to record as the maker of something that acts later as them: the person behind an
    /// admin's session or a person's own token, else the principal.
    pub fn made_by(&self) -> &str {
        self.maker.as_deref().unwrap_or(&self.principal)
    }

    /// Whether the caller can own personal overrides (people, not tokens or services).
    pub fn personal(&self) -> bool {
        !self.principal.starts_with("token:") && !self.principal.contains(':')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secrets_rule_names_its_project_environments_folders_and_actions() {
        let r = rule("prj_a", Some(vec!["production".into()]), Some(vec!["/api".into()]), &[Action::Read, Action::Reveal]);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["product"], "secrets");
        assert_eq!(json["resources"], serde_json::json!([{ "project": "prj_a" }]));
        assert_eq!((json["environments"].clone(), json["paths"].clone()), (serde_json::json!(["production"]), serde_json::json!(["/api"])));
        assert_eq!(json["actions"], serde_json::json!({ "read": "allow", "reveal": "allow" }));
    }
}
