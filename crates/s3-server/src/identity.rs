//! Who is asking: the identity behind a request's key, resolved once per request.
//!
//! - **The account's root user**: an account key with `admin` over every bucket (and its
//!   `GetSessionToken` sessions). Its own account's resources it may do anything with, as on
//!   AWS; another account's only what their resource policies and ACLs grant.
//! - **Account keys** with a roll permission level (`read`/`write`/`admin`, optionally scoped to
//!   buckets): the compatibility layer. Their level acts as an attached identity policy (see
//!   `access::key_allows` and `docs/storage/compatibility.md`).
//! - **IAM users** (`iam/`): their inline and managed policies and their groups', bounded by a
//!   permissions boundary.
//! - **Role sessions**: the role's *current* policies (and a control-plane role's permission
//!   level), bounded by its boundary, its control-plane policy and the session's policies.
//! - **Federated users** (`GetFederationToken`): the calling user's policies bounded by the
//!   session's.
//!
//! Every policy document comes from the metadata as it is now; parsed documents are cached by
//! their text.

use crate::policy::{Decision, Policy, Principal, Requester};
use s3_store::sts::{MAX_ROLE_POLICY_BYTES, MAX_SESSION_POLICY_BYTES, Session};
use s3_store::{AccessKey, Account, Store, iam};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The AWS-managed policies roll ships, by ARN.
pub(crate) const AWS_MANAGED: &[(&str, &str, &str)] = &[
    (
        "arn:aws:iam::aws:policy/AmazonS3FullAccess",
        "ANPAIFIR6V6BVTRAHWINE",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:*","s3-object-lambda:*"],"Resource":"*"}]}"#,
    ),
    (
        "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess",
        "ANPAIZTJ4DXE7G6AGAE6M",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:Get*","s3:List*","s3:Describe*","s3-object-lambda:Get*","s3-object-lambda:List*"],"Resource":"*"}]}"#,
    ),
];

/// The largest document any policy kind may be (a role's inline policies together).
const MAX_DOC: usize = MAX_ROLE_POLICY_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The account's root user.
    Root,
    /// A roll account key with a permission level.
    Key,
    User { name: String, id: String, arn: String },
    Role { name: String, id: String, session: String, arn: String },
    Federated { name: String },
}

pub(crate) struct Identity {
    pub account: Account,
    pub kind: Kind,
    /// How resource policies and ACLs see it.
    pub principal: Principal,
    /// `aws:PrincipalArn`.
    pub principal_arn: String,
    /// `GetCallerIdentity`'s `Arn` (a role session's assumed-role ARN).
    pub caller_arn: String,
    /// `aws:userid`.
    pub user_id: String,
    /// `aws:username` (users and account keys).
    pub username: Option<String>,
    /// `aws:PrincipalTag/…`: a user's tags, or a session's.
    pub tags: Vec<(String, String)>,
    /// Identity-based policies: any `Allow` grants.
    pub policies: Vec<Arc<Policy>>,
    /// Upper bounds (a permissions boundary, a control-plane role's policy): each must allow.
    pub boundaries: Vec<Arc<Policy>>,
    /// Session policies: each must allow.
    pub session_policies: Vec<Arc<Policy>>,
    /// Temporary credentials.
    pub session: Option<Arc<Session>>,
    /// The session may do nothing at all (see `Session::dead`), or the user is gone.
    pub dead: bool,
}

/// What a requester's own (identity) side says about a request.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IdVerdict {
    /// An explicit `Deny` in any of its policies.
    pub deny: bool,
    /// Its identity policies (or permission level, or being root) allow it …
    pub allow: bool,
    /// … and its boundaries and session policies allow it too.
    pub bounded: bool,
}

impl IdVerdict {
    pub fn allowed(&self) -> bool {
        !self.deny && self.allow && self.bounded
    }
}

fn cache() -> &'static Mutex<HashMap<String, Option<Arc<Policy>>>> {
    static C: OnceLock<Mutex<HashMap<String, Option<Arc<Policy>>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// A stored identity policy, parsed (cached by its text). `None`: it doesn't parse any more.
pub(crate) fn parsed(doc: &str) -> Option<Arc<Policy>> {
    if let Some(p) = cache().lock().unwrap().get(doc) {
        return p.clone();
    }
    let p = match Policy::parse_identity(doc.as_bytes(), MAX_DOC) {
        Ok(p) => Some(Arc::new(p)),
        Err(e) => {
            tracing::error!(error = e.message, "a stored identity policy doesn't parse");
            None
        }
    };
    let mut c = cache().lock().unwrap();
    if c.len() >= 4096 {
        c.clear();
    }
    c.insert(doc.to_string(), p.clone());
    p
}

/// A policy that allows nothing (and denies nothing): what a bound that's gone or no longer
/// parses becomes, and a federated user's bound without a session policy.
fn nothing() -> Arc<Policy> {
    static P: OnceLock<Arc<Policy>> = OnceLock::new();
    P.get_or_init(|| Arc::new(Policy::parse_identity(br#"{"Statement":{"Effect":"Allow","Action":"roll:Nothing","Resource":"arn:roll:nothing"}}"#, 1024).expect("valid"))).clone()
}

/// The document of a managed policy (AWS's built-ins, or the account's own), by ARN.
pub(crate) fn managed_document(store: &Store, account: &str, arn: &str) -> Option<String> {
    if let Some((_, _, doc)) = AWS_MANAGED.iter().find(|(a, ..)| *a == arn) {
        return Some((*doc).to_string());
    }
    let (acct, path_name) = arn.strip_prefix("arn:aws:iam::")?.split_once(":policy")?;
    if acct != account {
        return None;
    }
    let name = path_name.rsplit('/').next()?;
    let p = store.iam_policy(account, name).ok()??;
    let path = if p.path.is_empty() { "/" } else { p.path.as_str() };
    if format!("{path}{}", p.name) != path_name {
        return None;
    }
    p.document().map(str::to_string)
}

/// A policy that applies to a principal, and where it comes from (what the policy simulator
/// reports as `SourcePolicyId` and `SourcePolicyType`).
#[derive(Clone, Debug)]
pub(crate) struct Source {
    /// The inline policy's name, or the managed policy's.
    pub id: String,
    /// IAM's `PolicySourceType`: `user`, `group`, `role`, `aws-managed`, `user-managed`, `none`.
    pub kind: &'static str,
    /// `None`: a managed policy that's gone.
    pub doc: Option<String>,
}

/// Inline documents (of a `user`, `group` or `role`) and attached managed policies, as sources.
fn sources(store: &Store, account: &str, kind: &'static str, inline: &std::collections::BTreeMap<String, String>, attached: &[String], into: &mut Vec<Source>) {
    into.extend(inline.iter().map(|(name, doc)| Source { id: name.clone(), kind, doc: Some(doc.clone()) }));
    into.extend(attached.iter().map(|arn| managed_source(store, account, arn)));
}

fn managed_source(store: &Store, account: &str, arn: &str) -> Source {
    let kind = if arn.starts_with("arn:aws:iam::aws:") { "aws-managed" } else { "user-managed" };
    Source { id: arn.rsplit('/').next().unwrap_or(arn).to_string(), kind, doc: managed_document(store, account, arn) }
}

/// Allowing policies from sources (a managed policy that's gone allows nothing).
fn allowing(src: &[Source]) -> impl Iterator<Item = Arc<Policy>> + '_ {
    src.iter().filter_map(|s| s.doc.as_deref().and_then(parsed))
}

/// A bound from a source: one that's gone or doesn't parse allows nothing.
pub(crate) fn bounding(s: &Source) -> Arc<Policy> {
    s.doc.as_deref().and_then(parsed).unwrap_or_else(nothing)
}

/// The identity policies of an IAM user (its own and its groups') and its permissions boundary.
pub(crate) fn user_sources(store: &Store, account: &str, u: &iam::User) -> (Vec<Source>, Option<Source>) {
    let mut out = Vec::new();
    sources(store, account, "user", &u.policies, &u.attached, &mut out);
    for g in &u.groups {
        if let Ok(Some(g)) = store.iam_group(account, g) {
            sources(store, account, "group", &g.policies, &g.attached, &mut out);
        }
    }
    (out, u.boundary.as_deref().map(|b| managed_source(store, account, b)))
}

/// A group's policies.
pub(crate) fn group_sources(store: &Store, account: &str, g: &iam::Group) -> Vec<Source> {
    let mut out = Vec::new();
    sources(store, account, "group", &g.policies, &g.attached, &mut out);
    out
}

/// A role's identity policies, and its bounds: its permissions boundary and a control-plane
/// role's policy.
pub(crate) fn role_sources(store: &Store, account: &str, role: &s3_store::sts::Role) -> (Vec<Source>, Vec<Source>) {
    let mut out = Vec::new();
    sources(store, account, "role", &role.iam.policies, &role.iam.attached, &mut out);
    let mut bounds: Vec<Source> = role.iam.boundary.as_deref().map(|b| managed_source(store, account, b)).into_iter().collect();
    if let Some(p) = &role.policy {
        bounds.push(Source { id: "roll role policy".into(), kind: "role", doc: Some(p.clone()) });
    }
    (out, bounds)
}

/// A bound from a managed policy ARN: one that's gone or doesn't parse allows nothing.
fn bound(store: &Store, account: &str, arn: &str) -> Arc<Policy> {
    bounding(&managed_source(store, account, arn))
}

/// Who did it, in event records (`userIdentity.principalId`): an account key's id (the root
/// user's or a permission key's), else `aws:userid` (an IAM user's id, `<role id>:<session>`).
pub(crate) fn event_principal(k: &AccessKey, id: &Identity) -> String {
    match id.kind {
        Kind::Root | Kind::Key => k.acting_key().unwrap_or(&k.id).to_string(),
        _ => id.user_id.clone(),
    }
}

pub(crate) fn user_arn(account: &str, path: &str, name: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    format!("arn:aws:iam::{account}:user{path}{name}")
}

pub(crate) fn root_arn(account: &str) -> String {
    format!("arn:aws:iam::{account}:root")
}

pub(crate) fn saml_provider_arn(account: &str, name: &str) -> String {
    format!("arn:aws:iam::{account}:saml-provider/{name}")
}

/// The identity policies of an IAM user and its groups, and its boundary.
fn user_policies(store: &Store, account: &str, u: &iam::User, policies: &mut Vec<Arc<Policy>>, boundaries: &mut Vec<Arc<Policy>>) {
    let (src, boundary) = user_sources(store, account, u);
    policies.extend(allowing(&src));
    boundaries.extend(boundary.as_ref().map(bounding));
}

/// An IAM user's identity: `false` if the user is gone.
fn as_user(store: &Store, acct: &str, name: &str, id: &mut Identity, req: &mut Requester) -> Result<bool, s3_store::StoreError> {
    let Some(u) = store.iam_user(acct, name)? else { return Ok(false) };
    let arn = user_arn(acct, &u.path, &u.name);
    id.kind = Kind::User { name: u.name.clone(), id: u.id.clone(), arn: arn.clone() };
    id.principal_arn = arn.clone();
    id.caller_arn = arn.clone();
    id.user_id = u.id.clone();
    id.username = Some(u.name.clone());
    id.tags = u.tags.clone();
    req.direct = vec![arn];
    user_policies(store, acct, &u, &mut id.policies, &mut id.boundaries);
    Ok(true)
}

/// Resolve the identity behind `k`. `Err` only for storage failures.
pub(crate) fn resolve(store: &Store, k: &AccessKey) -> Result<Identity, s3_store::StoreError> {
    let found = store.account(&k.account)?;
    let gone = found.is_none();
    let account = match found {
        Some(a) => a,
        // An account deleted under a live session.
        None => Account { id: k.account.clone(), canonical: "0".repeat(64), name: String::new(), email: None, created_at: 0 },
    };
    let acct = account.id.clone();
    let mut id = Identity {
        account: account.clone(),
        kind: Kind::Key,
        principal: Principal::Anonymous,
        principal_arn: String::new(),
        caller_arn: String::new(),
        user_id: String::new(),
        username: None,
        tags: Vec::new(),
        policies: Vec::new(),
        boundaries: Vec::new(),
        session_policies: Vec::new(),
        session: k.session.clone(),
        dead: k.session.as_ref().is_some_and(|s| s.dead) || gone,
    };
    let mut req = Requester { account: acct.clone(), account_canonical: account.canonical.clone(), ..Default::default() };
    // The permanent key acting (itself, or the one behind a GetSessionToken/GetFederationToken
    // session).
    let parent: Option<AccessKey> = match &k.session {
        None => Some(k.clone()),
        Some(s) if s.role.is_none() => s.key.as_deref().and_then(|p| store.access_key(p).ok().flatten()),
        Some(_) => None,
    };
    match (&k.session, &parent) {
        (Some(s), _) if s.role.is_some() => {
            let r = s.role.as_ref().expect("a role session");
            let role_arn = s.role_arn(&acct).unwrap_or_default();
            let session_arn = s.assumed_role_arn(&acct).unwrap_or_default();
            id.kind = Kind::Role { name: r.name.clone(), id: r.id.clone(), session: s.name.clone().unwrap_or_default(), arn: session_arn.clone() };
            id.principal_arn = role_arn.clone();
            id.caller_arn = session_arn.clone();
            id.user_id = s.user_id().unwrap_or_default();
            id.tags = s.tags.clone();
            req.direct = vec![session_arn];
            req.direct_canonical = vec![store.canonical_user(&k.id)];
            req.role = Some(role_arn);
            req.federated = s.web.as_ref().map(|w| format!("arn:aws:iam::{acct}:oidc-provider/{}", w.provider())).or_else(|| s.saml.as_ref().map(|sa| saml_provider_arn(&acct, &sa.provider)));
            if let Some(role) = &s.role_now {
                let (src, bounds) = role_sources(store, &acct, role);
                id.policies.extend(allowing(&src));
                id.boundaries.extend(bounds.iter().map(bounding));
            }
            session_policies(store, &acct, s, &mut id.session_policies);
        }
        (session, Some(p)) => {
            match &p.user {
                Some(name) => {
                    if !as_user(store, &acct, name, &mut id, &mut req)? {
                        id.dead = true;
                    }
                }
                None if p.is_root() => {
                    id.kind = Kind::Root;
                    id.principal_arn = root_arn(&acct);
                    id.caller_arn = root_arn(&acct);
                    id.user_id = acct.clone();
                    req.root = true;
                    req.direct = vec![root_arn(&acct)];
                }
                None => {
                    let arn = format!("arn:aws:iam::{acct}:user/{}", p.id);
                    id.kind = Kind::Key;
                    id.principal_arn = arn.clone();
                    id.caller_arn = arn.clone();
                    id.user_id = p.id.clone();
                    id.username = Some(p.id.clone());
                    req.direct = vec![p.id.clone(), arn];
                    req.direct_canonical = vec![store.canonical_user(&p.id)];
                }
            }
            if let Some(s) = session {
                if let Some(name) = &s.federated {
                    // A federated user acts with its caller's policies, bounded by the session's.
                    let arn = s.federated_arn(&acct).unwrap_or_default();
                    id.kind = Kind::Federated { name: name.clone() };
                    id.principal_arn = arn.clone();
                    id.caller_arn = arn.clone();
                    id.user_id = format!("{acct}:{name}");
                    id.username = None;
                    id.tags = s.tags.clone();
                    req.root = false;
                    req.direct = vec![arn];
                    req.direct_canonical = Vec::new();
                    session_policies(store, &acct, s, &mut id.session_policies);
                    // Without a session policy, a federated user has no permissions of its own.
                    if id.session_policies.is_empty() {
                        id.session_policies.push(nothing());
                    }
                } else {
                    // A GetSessionToken session is its key (and its session's key canonical id).
                    req.direct_canonical.push(store.canonical_user(&k.id));
                }
            }
        }
        // A dashboard sign-in session: its IAM user (`console.rs`).
        (Some(s), None) if s.console => {
            let name = s.user.clone().unwrap_or_default();
            if !as_user(store, &acct, &name, &mut id, &mut req)? || s.dead {
                id.dead = true;
            }
        }
        // A GetSessionToken/GetFederationToken session whose key is gone.
        (Some(_), None) => id.dead = true,
        (None, None) => unreachable!("a permanent key is its own parent"),
    }
    id.principal = Principal::Known(Box::new(req));
    Ok(id)
}

fn session_policies(store: &Store, account: &str, s: &Session, into: &mut Vec<Arc<Policy>>) {
    if let Some(p) = &s.policy {
        into.push(Policy::parse_identity(p.as_bytes(), MAX_SESSION_POLICY_BYTES).map(Arc::new).unwrap_or_else(|_| nothing()));
    }
    for arn in &s.policy_arns {
        into.push(bound(store, account, arn));
    }
}

impl Identity {
    pub fn is_root(&self) -> bool {
        self.kind == Kind::Root
    }

    /// What its own side says about `action` on `resource`. `level` says whether its roll
    /// permission level (account keys, control-plane roles) allows it.
    pub fn verdict(&self, action: &str, resource: &str, cond: &dyn crate::policy::Context, level: bool) -> IdVerdict {
        if self.dead {
            return IdVerdict { deny: true, ..Default::default() };
        }
        let mut v = IdVerdict { bounded: true, ..Default::default() };
        for p in &self.policies {
            match p.decide(action, resource, cond) {
                Decision::Deny => return IdVerdict { deny: true, ..Default::default() },
                Decision::Allow => v.allow = true,
                Decision::None => {}
            }
        }
        for p in self.boundaries.iter().chain(&self.session_policies) {
            match p.decide(action, resource, cond) {
                Decision::Deny => return IdVerdict { deny: true, ..Default::default() },
                Decision::Allow => {}
                Decision::None => v.bounded = false,
            }
        }
        v.allow |= self.is_root() || level;
        v
    }

    /// The condition keys an identity sets (`key` lowercase). `None`: not one of these;
    /// `Some(None)`: absent for this requester.
    pub fn condition(&self, key: &str, now_ms: i64) -> Option<Option<Vec<String>>> {
        let one = |v: Option<String>| Some(v.map(|v| vec![v]));
        if let Some(name) = key.strip_prefix("aws:principaltag/") {
            return one(self.tags.iter().find(|(t, _)| t.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()));
        }
        match key {
            "aws:principalarn" => return one(Some(self.principal_arn.clone())),
            "aws:principalaccount" => return one(Some(self.account.id.clone())),
            "aws:userid" => return one(Some(self.user_id.clone())),
            "aws:username" => return one(self.username.clone()),
            "aws:principaltype" => {
                return one(Some(
                    match self.kind {
                        Kind::Root => "Account",
                        Kind::Key | Kind::User { .. } => "User",
                        Kind::Role { .. } => "AssumedRole",
                        Kind::Federated { .. } => "FederatedUser",
                    }
                    .into(),
                ));
            }
            "aws:principalisawsservice" => return one(Some("false".into())),
            _ => {}
        }
        // The rest are temporary credentials' (absent for long-term keys, as on AWS).
        let s = self.session.as_ref()?;
        match key {
            "aws:tokenissuetime" => return one(Some(s3_proto::time::iso8601_ms(s.issued_at))),
            "aws:multifactorauthpresent" => return one(Some(s.mfa_at.is_some().to_string())),
            "aws:multifactorauthage" => return one(s.mfa_at.map(|at| ((now_ms - at).max(0) / 1000).to_string())),
            "aws:sourceidentity" => return one(s.source_identity.clone()),
            "aws:federatedprovider" => {
                return one(s.web.as_ref().map(|w| format!("arn:aws:iam::{}:oidc-provider/{}", self.account.id, w.provider())).or_else(|| s.saml.as_ref().map(|sa| saml_provider_arn(&self.account.id, &sa.provider))));
            }
            _ => {}
        }
        if let Some(sa) = &s.saml
            && key.starts_with("saml:")
        {
            return one(match key {
                "saml:sub" => Some(sa.subject.clone()),
                "saml:sub_type" => Some(sa.subject_type.clone()),
                "saml:iss" => Some(sa.issuer.clone()),
                "saml:aud" => Some(sa.audience.clone()),
                "saml:namequalifier" => Some(sa.name_qualifier.clone()),
                _ => None,
            });
        }
        if let Some(w) = &s.web {
            let provider = w.provider().to_ascii_lowercase();
            if let Some(claim) = key.strip_prefix(provider.as_str()).and_then(|r| r.strip_prefix(':')) {
                return match claim {
                    "sub" => one(Some(w.subject.clone())),
                    "aud" => one(Some(w.audience.clone())),
                    _ => Some(None),
                };
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_managed_policies_parse() {
        for (arn, id, doc) in AWS_MANAGED {
            assert!(parsed(doc).is_some(), "{arn}");
            assert_eq!(id.len(), 21);
        }
        assert!(parsed("not a policy").is_none());
        struct No;
        impl crate::policy::Context for No {
            fn values(&self, _: &str) -> Option<Vec<String>> {
                None
            }
        }
        let ro = parsed(AWS_MANAGED[1].2).unwrap();
        assert_eq!(ro.decide("s3:GetObject", "arn:aws:s3:::b/k", &No), Decision::Allow);
        assert_eq!(ro.decide("s3:PutObject", "arn:aws:s3:::b/k", &No), Decision::None);
        assert_eq!(nothing().decide("s3:GetObject", "arn:aws:s3:::b/k", &No), Decision::None);
        assert_eq!(nothing().decide("iam:CreateUser", "arn:aws:iam::1:user/x", &No), Decision::None);
    }
}
