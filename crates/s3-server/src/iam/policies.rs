//! Managed policies: customer-managed ones (`CreatePolicy`, versions, attachments) and AWS's
//! built-ins (`AmazonS3FullAccess`, `AmazonS3ReadOnlyAccess`).

use super::*;
use crate::identity::AWS_MANAGED;
use s3_store::iam::{self as si, Group, ManagedPolicy, PolicyVersion, User, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreatePolicy" => create(c).await?,
        "GetPolicy" => get(c)?,
        "ListPolicies" => list(c)?,
        "DeletePolicy" => return delete(c).await.map(|_| None),
        "CreatePolicyVersion" => create_version(c).await?,
        "GetPolicyVersion" => get_version(c)?,
        "ListPolicyVersions" => list_versions(c)?,
        "SetDefaultPolicyVersion" => return set_default(c).await.map(|_| None),
        "DeletePolicyVersion" => return delete_version(c).await.map(|_| None),
        "AttachUserPolicy" => return attach(c, Entity::User, true).await.map(|_| None),
        "DetachUserPolicy" => return attach(c, Entity::User, false).await.map(|_| None),
        "ListAttachedUserPolicies" => list_attached(c, Entity::User)?,
        "AttachGroupPolicy" => return attach(c, Entity::Group, true).await.map(|_| None),
        "DetachGroupPolicy" => return attach(c, Entity::Group, false).await.map(|_| None),
        "ListAttachedGroupPolicies" => list_attached(c, Entity::Group)?,
        "AttachRolePolicy" => return attach(c, Entity::Role, true).await.map(|_| None),
        "DetachRolePolicy" => return attach(c, Entity::Role, false).await.map(|_| None),
        "ListAttachedRolePolicies" => list_attached(c, Entity::Role)?,
        "ListEntitiesForPolicy" => entities(c)?,
        _ => unreachable!("routed here"),
    }))
}

/// A policy as `GetPolicy` and friends show it.
struct Shown {
    name: String,
    id: String,
    arn: String,
    path: String,
    description: String,
    default_version: String,
    created_at: i64,
    updated_at: i64,
    attachments: usize,
    boundaries: usize,
}

fn shown_xml(p: &Shown) -> String {
    format!(
        "{}{}{}{}{}{}<AttachmentCount>{}</AttachmentCount><PermissionsBoundaryUsageCount>{}</PermissionsBoundaryUsageCount><IsAttachable>true</IsAttachable>{}{}",
        el("PolicyName", &p.name),
        el("PolicyId", &p.id),
        el("Arn", &p.arn),
        el("Path", &p.path),
        if p.description.is_empty() { String::new() } else { el("Description", &p.description) },
        el("DefaultVersionId", &p.default_version),
        p.attachments,
        p.boundaries,
        el("CreateDate", &date(p.created_at)),
        el("UpdateDate", &date(p.updated_at))
    )
}

/// Who refers to a policy: what it's attached to, and how many use it as their boundary.
struct Usage {
    users: Vec<User>,
    groups: Vec<Group>,
    roles: Vec<s3_store::sts::Role>,
    boundaries: usize,
}

impl Usage {
    fn attachments(&self) -> usize {
        self.users.len() + self.groups.len() + self.roles.len()
    }
}

fn usage(c: &Call, arn: &str) -> Result<Usage, StsError> {
    let (users, groups, roles) = c.store.iam_read(|t| Ok((t.list::<User>(&c.account, kind::USER)?, t.list::<Group>(&c.account, kind::GROUP)?, t.roles(&c.account)?))).map_err(store_error)?;
    let boundaries = users.iter().filter(|u| u.boundary.as_deref() == Some(arn)).count() + roles.iter().filter(|r| r.iam.boundary.as_deref() == Some(arn)).count();
    let has = |v: &[String]| v.iter().any(|a| a == arn);
    Ok(Usage {
        users: users.into_iter().filter(|u| has(&u.attached)).collect(),
        groups: groups.into_iter().filter(|g| has(&g.attached)).collect(),
        roles: roles.into_iter().filter(|r| has(&r.iam.attached)).collect(),
        boundaries,
    })
}

fn customer_arn(c: &Call, p: &ManagedPolicy) -> String {
    c.arn("policy", &p.path, &p.name)
}

fn show(c: &Call, p: &ManagedPolicy) -> Result<Shown, StsError> {
    let arn = customer_arn(c, p);
    let u = usage(c, &arn)?;
    Ok(Shown {
        name: p.name.clone(),
        id: p.id.clone(),
        arn,
        path: p.path.clone(),
        description: p.description.clone(),
        default_version: format!("v{}", p.default_version),
        created_at: p.created_at,
        updated_at: p.updated_at,
        attachments: u.attachments(),
        boundaries: u.boundaries,
    })
}

fn show_aws(c: &Call, arn: &str, id: &str) -> Result<Shown, StsError> {
    let u = usage(c, arn)?;
    Ok(Shown {
        name: arn.rsplit('/').next().unwrap_or_default().to_string(),
        id: id.to_string(),
        arn: arn.to_string(),
        path: "/".into(),
        description: String::new(),
        default_version: "v1".into(),
        created_at: 1_438_000_000_000,
        updated_at: 1_438_000_000_000,
        attachments: u.attachments(),
        boundaries: u.boundaries,
    })
}

/// The customer-managed policy `PolicyArn` names (`Err` for AWS's: they can't change).
fn policy_of(c: &Call) -> Result<ManagedPolicy, StsError> {
    let arn = c.p.required("PolicyArn", "policyArn")?;
    if AWS_MANAGED.iter().any(|(a, ..)| *a == arn) {
        return Err(access_denied(format!("AWS managed policy {arn} can't be changed")));
    }
    let gone = || no_such(format!("Policy {arn} does not exist or is not attachable."));
    let path_name = arn.strip_prefix(&format!("arn:aws:iam::{}:policy", c.account)).ok_or_else(gone)?;
    let name = path_name.rsplit('/').next().unwrap_or_default();
    let p = c.store.iam_policy(&c.account, name).map_err(store_error)?.ok_or_else(gone)?;
    if customer_arn(c, &p) != arn {
        return Err(gone());
    }
    Ok(p)
}

async fn create(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("PolicyName", 128)?.to_string();
    let path = c.path("Path")?;
    c.allow("CreatePolicy", &c.arn("policy", &path, &name))?;
    let doc = c.document("PolicyDocument", si::MAX_MANAGED_POLICY_BYTES)?;
    let description = c.p.get("Description").unwrap_or_default().to_string();
    if description.chars().count() > 1000 {
        return Err(validation("1 validation error detected: Value at 'description' failed to satisfy constraint: Member must have length less than or equal to 1000"));
    }
    let now = c.store.now();
    let p = ManagedPolicy {
        name: name.clone(),
        id: si::entity_id("ANPA"),
        path,
        description,
        created_at: now,
        updated_at: now,
        default_version: 1,
        versions: vec![PolicyVersion { id: 1, document: doc, created_at: now }],
        next_version: 2,
        tags: c.tags()?,
    };
    let acct = c.account.clone();
    let saved = p.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::POLICY, &name)? {
                return Err(StoreError::Conflict(format!("A policy called {name} already exists. Duplicate names are not allowed.")));
            }
            if t.count(&acct, kind::POLICY)? >= si::MAX_POLICIES {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for PoliciesPerAccount: {}", si::MAX_POLICIES)));
            }
            t.put(&acct, kind::POLICY, &name, &saved)
        })
        .await
        .map_err(store_error)?;
    Ok(format!("<Policy>{}{}</Policy>", shown_xml(&show(c, &p)?), tags_xml(&p.tags)))
}

fn get(c: &Call) -> Result<String, StsError> {
    let arn = c.p.required("PolicyArn", "policyArn")?;
    c.allow("GetPolicy", arn)?;
    if let Some((a, id, _)) = AWS_MANAGED.iter().find(|(a, ..)| *a == arn) {
        return Ok(format!("<Policy>{}</Policy>", shown_xml(&show_aws(c, a, id)?)));
    }
    let p = policy_of(c)?;
    Ok(format!("<Policy>{}{}</Policy>", shown_xml(&show(c, &p)?), tags_xml(&p.tags)))
}

fn list(c: &Call) -> Result<String, StsError> {
    c.allow("ListPolicies", &c.arn("policy", "/", "*"))?;
    let scope = c.p.get("Scope").unwrap_or("All");
    if !matches!(scope, "All" | "AWS" | "Local") {
        return Err(validation("1 validation error detected: Value at 'scope' failed to satisfy constraint: Member must satisfy enum value set: [All, AWS, Local]"));
    }
    let only_attached = c.p.get("OnlyAttached").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let prefix = c.p.get("PathPrefix").unwrap_or("/");
    let mut all = Vec::new();
    if scope != "Local" {
        for (arn, id, _) in AWS_MANAGED {
            all.push(show_aws(c, arn, id)?);
        }
    }
    if scope != "AWS" {
        for p in c.store.iam_read(|t| t.list::<ManagedPolicy>(&c.account, kind::POLICY)).map_err(store_error)? {
            all.push(show(c, &p)?);
        }
    }
    let all: Vec<Shown> = all.into_iter().filter(|p| p.path.starts_with(prefix) && (!only_attached || p.attachments > 0)).collect();
    page(c.p, all, |p| p.arn.clone(), shown_xml, "Policies")
}

async fn delete(c: &Call<'_>) -> Result<(), StsError> {
    let p = policy_of(c)?;
    let arn = customer_arn(c, &p);
    c.allow("DeletePolicy", &arn)?;
    let u = usage(c, &arn)?;
    if u.attachments() > 0 || u.boundaries > 0 {
        return Err(delete_conflict("Cannot delete a policy attached to entities."));
    }
    if p.versions.len() > 1 {
        return Err(delete_conflict("This policy has more than one version. Before you delete a policy, you must delete the policy's versions. The default version is deleted with the policy."));
    }
    let acct = c.account.clone();
    c.store.iam_write(move |t| t.delete(&acct, kind::POLICY, &p.name).map(|_| ())).await.map_err(store_error)
}

fn version_xml(v: &PolicyVersion, default: u32, document: bool) -> String {
    format!(
        "{}{}<IsDefaultVersion>{}</IsDefaultVersion>{}",
        if document { el("Document", &url_encode(&v.document)) } else { String::new() },
        el("VersionId", &format!("v{}", v.id)),
        v.id == default,
        el("CreateDate", &date(v.created_at))
    )
}

/// Change a customer-managed policy in one transaction.
async fn edit(c: &Call<'_>, p: &ManagedPolicy, f: impl FnOnce(&mut ManagedPolicy) -> Result<(), StsError>) -> Result<ManagedPolicy, StsError> {
    let acct = c.account.clone();
    let name = p.name.clone();
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let Some(mut p) = t.get::<ManagedPolicy>(&acct, kind::POLICY, &name)? else { return Err(StoreError::NotFound(format!("Policy {name} does not exist."))) };
            if let Err(e) = f(&mut p) {
                err = Some(e);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            p.updated_at = t.now();
            t.put(&acct, kind::POLICY, &name, &p)?;
            Ok(p)
        })
        .await;
    match (r, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

async fn create_version(c: &Call<'_>) -> Result<String, StsError> {
    let p = policy_of(c)?;
    c.allow("CreatePolicyVersion", &customer_arn(c, &p))?;
    let doc = c.document("PolicyDocument", si::MAX_MANAGED_POLICY_BYTES)?;
    let default = c.p.get("SetAsDefault").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let now = c.store.now();
    let p = edit(c, &p, |p| {
        if p.versions.len() >= si::MAX_POLICY_VERSIONS {
            return Err(limit(format!("A managed policy can have up to {} versions. Before you create a new version, you must delete an existing version.", si::MAX_POLICY_VERSIONS)));
        }
        let id = p.next_version;
        p.next_version += 1;
        p.versions.push(PolicyVersion { id, document: doc, created_at: now });
        if default {
            p.default_version = id;
        }
        Ok(())
    })
    .await?;
    let v = p.versions.last().expect("just added");
    Ok(format!("<PolicyVersion>{}</PolicyVersion>", version_xml(v, p.default_version, false)))
}

fn version_id(c: &Call) -> Result<u32, StsError> {
    let v = c.p.required("VersionId", "versionId")?;
    v.strip_prefix('v').and_then(|n| n.parse().ok()).ok_or_else(|| validation(format!("1 validation error detected: Value '{v}' at 'versionId' failed to satisfy constraint: Member must satisfy regular expression pattern: v[1-9][0-9]*(\\.[A-Za-z0-9-]*)?")))
}

fn get_version(c: &Call) -> Result<String, StsError> {
    let arn = c.p.required("PolicyArn", "policyArn")?;
    c.allow("GetPolicyVersion", arn)?;
    if let Some((_, _, doc)) = AWS_MANAGED.iter().find(|(a, ..)| *a == arn) {
        if version_id(c)? != 1 {
            return Err(no_such(format!("Policy {arn} version {} does not exist or is not attachable.", c.p.get("VersionId").unwrap_or_default())));
        }
        let v = PolicyVersion { id: 1, document: (*doc).to_string(), created_at: 1_438_000_000_000 };
        return Ok(format!("<PolicyVersion>{}</PolicyVersion>", version_xml(&v, 1, true)));
    }
    let p = policy_of(c)?;
    let n = version_id(c)?;
    let v = p.versions.iter().find(|v| v.id == n).ok_or_else(|| no_such(format!("Policy {arn} version v{n} does not exist or is not attachable.")))?;
    Ok(format!("<PolicyVersion>{}</PolicyVersion>", version_xml(v, p.default_version, true)))
}

fn list_versions(c: &Call) -> Result<String, StsError> {
    let arn = c.p.required("PolicyArn", "policyArn")?;
    c.allow("ListPolicyVersions", arn)?;
    if let Some((_, _, doc)) = AWS_MANAGED.iter().find(|(a, ..)| *a == arn) {
        let v = PolicyVersion { id: 1, document: (*doc).to_string(), created_at: 1_438_000_000_000 };
        return page(c.p, vec![v], |v| format!("v{}", v.id), |v| version_xml(v, 1, false), "Versions");
    }
    let p = policy_of(c)?;
    let default = p.default_version;
    let mut versions = p.versions;
    versions.sort_by_key(|v| std::cmp::Reverse(v.id));
    page(c.p, versions, |v| format!("v{:010}", u32::MAX - v.id), |v| version_xml(v, default, false), "Versions")
}

async fn set_default(c: &Call<'_>) -> Result<(), StsError> {
    let p = policy_of(c)?;
    c.allow("SetDefaultPolicyVersion", &customer_arn(c, &p))?;
    let n = version_id(c)?;
    edit(c, &p, |p| {
        if !p.versions.iter().any(|v| v.id == n) {
            return Err(no_such(format!("Policy version v{n} does not exist or is not attachable.")));
        }
        p.default_version = n;
        Ok(())
    })
    .await
    .map(|_| ())
}

async fn delete_version(c: &Call<'_>) -> Result<(), StsError> {
    let p = policy_of(c)?;
    c.allow("DeletePolicyVersion", &customer_arn(c, &p))?;
    let n = version_id(c)?;
    edit(c, &p, |p| {
        if p.default_version == n {
            return Err(delete_conflict("Cannot delete the default version of a policy."));
        }
        let before = p.versions.len();
        p.versions.retain(|v| v.id != n);
        if p.versions.len() == before {
            return Err(no_such(format!("Policy version v{n} does not exist or is not attachable.")));
        }
        Ok(())
    })
    .await
    .map(|_| ())
}

#[derive(Clone, Copy)]
enum Entity {
    User,
    Group,
    Role,
}

impl Entity {
    fn param(self) -> (&'static str, usize) {
        match self {
            Entity::User => ("UserName", 64),
            Entity::Group => ("GroupName", 128),
            Entity::Role => ("RoleName", 64),
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Entity::User => "User",
            Entity::Group => "Group",
            Entity::Role => "Role",
        }
    }
}

/// The entity's ARN and its attached policies.
fn attached_of(c: &Call, e: Entity) -> Result<(String, Vec<String>), StsError> {
    let (param, max) = e.param();
    let name = c.name(param, max)?;
    Ok(match e {
        Entity::User => {
            let u = super::users::user(c, name)?;
            (c.arn("user", &u.path, &u.name), u.attached)
        }
        Entity::Group => {
            let g = c.store.iam_group(&c.account, name).map_err(store_error)?.ok_or_else(|| no_such(format!("The group with name {name} cannot be found.")))?;
            (c.arn("group", &g.path, &g.name), g.attached)
        }
        Entity::Role => {
            let r = c.store.role_in(&c.account, name).map_err(store_error)?.ok_or_else(|| no_such(format!("The role with name {name} cannot be found.")))?;
            (r.own_arn(), r.iam.attached)
        }
    })
}

async fn attach(c: &Call<'_>, e: Entity, on: bool) -> Result<(), StsError> {
    let (arn, _) = attached_of(c, e)?;
    c.allow(&format!("{}{}Policy", if on { "Attach" } else { "Detach" }, e.noun()), &arn)?;
    let policy = c.p.required("PolicyArn", "policyArn")?.to_string();
    if on {
        super::users::attachable(c, &policy)?;
    }
    let (param, _) = e.param();
    let name = c.p.get(param).unwrap_or_default().to_string();
    let edit = |list: &mut Vec<String>| -> Result<(), StoreError> {
        let have = list.iter().position(|a| *a == policy);
        match (on, have) {
            (true, Some(_)) => Ok(()),
            (true, None) if list.len() >= si::MAX_ATTACHED => Err(StoreError::LimitExceeded(format!("Cannot exceed quota for PoliciesPerUser: {}", si::MAX_ATTACHED))),
            (true, None) => {
                list.push(policy.clone());
                Ok(())
            }
            (false, Some(i)) => {
                list.remove(i);
                Ok(())
            }
            (false, None) => Err(StoreError::NotFound(format!("Policy {policy} was not found."))),
        }
    };
    let acct = c.account.clone();
    match e {
        Entity::Role => c.store.edit_role(&acct, &name, |_, r| edit(&mut r.iam.attached)).await.map(|_| ()).map_err(store_error),
        Entity::User => c
            .store
            .iam_write(|t| {
                let mut u: User = t.get(&acct, kind::USER, &name)?.ok_or_else(|| StoreError::NotFound(format!("The user with name {name} cannot be found.")))?;
                edit(&mut u.attached)?;
                t.put(&acct, kind::USER, &name, &u)
            })
            .await
            .map_err(store_error),
        Entity::Group => c
            .store
            .iam_write(|t| {
                let mut g: Group = t.get(&acct, kind::GROUP, &name)?.ok_or_else(|| StoreError::NotFound(format!("The group with name {name} cannot be found.")))?;
                edit(&mut g.attached)?;
                t.put(&acct, kind::GROUP, &name, &g)
            })
            .await
            .map_err(store_error),
    }
}

fn policy_name(arn: &str) -> &str {
    arn.rsplit('/').next().unwrap_or(arn)
}

fn list_attached(c: &Call, e: Entity) -> Result<String, StsError> {
    let (arn, attached) = attached_of(c, e)?;
    c.allow(&format!("ListAttached{}Policies", e.noun()), &arn)?;
    page(c.p, attached, |a| a.clone(), |a| format!("{}{}", el("PolicyName", policy_name(a)), el("PolicyArn", a)), "AttachedPolicies")
}

fn entities(c: &Call) -> Result<String, StsError> {
    let arn = c.p.required("PolicyArn", "policyArn")?.to_string();
    c.allow("ListEntitiesForPolicy", &arn)?;
    if !AWS_MANAGED.iter().any(|(a, ..)| *a == arn) {
        policy_of(c)?;
    }
    let Usage { users, groups, roles, .. } = usage(c, &arn)?;
    let m = |kind: &str, items: Vec<(String, String)>| {
        let members: String = items.iter().map(|(n, id)| format!("<member>{}{}</member>", el(&format!("{kind}Name"), n), el(&format!("{kind}Id"), id))).collect();
        format!("<Policy{kind}s>{members}</Policy{kind}s>")
    };
    Ok(format!(
        "{}{}{}<IsTruncated>false</IsTruncated>",
        m("Group", groups.into_iter().map(|g| (g.name, g.id)).collect()),
        m("User", users.into_iter().map(|u| (u.name, u.id)).collect()),
        m("Role", roles.into_iter().map(|r| (r.name, r.id)).collect())
    ))
}
