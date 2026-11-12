//! IAM users, groups, access keys and inline policies.

use super::*;
use s3_store::iam::{self as si, Group, User, kind};
use s3_store::{AccessKey, Permission};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreateUser" => create_user(c).await?,
        "GetUser" => get_user(c)?,
        "ListUsers" => list_users(c)?,
        "DeleteUser" => return delete_user(c).await.map(|_| None),
        "UpdateUser" => return update_user(c).await.map(|_| None),
        "TagUser" => return tag_user(c, true).await.map(|_| None),
        "UntagUser" => return tag_user(c, false).await.map(|_| None),
        "ListUserTags" => list_user_tags(c)?,
        "PutUserPermissionsBoundary" => return user_boundary(c, true).await.map(|_| None),
        "DeleteUserPermissionsBoundary" => return user_boundary(c, false).await.map(|_| None),
        "CreateGroup" => create_group(c).await?,
        "GetGroup" => get_group(c)?,
        "ListGroups" => list_groups(c)?,
        "DeleteGroup" => return delete_group(c).await.map(|_| None),
        "AddUserToGroup" => return membership(c, true).await.map(|_| None),
        "RemoveUserFromGroup" => return membership(c, false).await.map(|_| None),
        "ListGroupsForUser" => list_groups_for_user(c)?,
        "CreateAccessKey" => create_access_key(c).await?,
        "ListAccessKeys" => list_access_keys(c)?,
        "UpdateAccessKey" => return update_access_key(c).await.map(|_| None),
        "DeleteAccessKey" => return delete_access_key(c).await.map(|_| None),
        "GetAccessKeyLastUsed" => access_key_last_used(c)?,
        "PutUserPolicy" => return put_inline(c, Target::User).await.map(|_| None),
        "GetUserPolicy" => get_inline(c, Target::User)?,
        "ListUserPolicies" => list_inline(c, Target::User)?,
        "DeleteUserPolicy" => return delete_inline(c, Target::User).await.map(|_| None),
        "PutGroupPolicy" => return put_inline(c, Target::Group).await.map(|_| None),
        "GetGroupPolicy" => get_inline(c, Target::Group)?,
        "ListGroupPolicies" => list_inline(c, Target::Group)?,
        "DeleteGroupPolicy" => return delete_inline(c, Target::Group).await.map(|_| None),
        _ => unreachable!("routed here"),
    }))
}

fn no_user(name: &str) -> StsError {
    no_such(format!("The user with name {name} cannot be found."))
}

fn no_group(name: &str) -> StsError {
    no_such(format!("The group with name {name} cannot be found."))
}

pub(super) fn user_xml(c: &Call, u: &User) -> String {
    format!(
        "{}{}{}{}{}{}{}",
        el("Path", &u.path),
        el("UserName", &u.name),
        el("UserId", &u.id),
        el("Arn", &c.arn("user", &u.path, &u.name)),
        el("CreateDate", &date(u.created_at)),
        boundary_xml(&u.boundary),
        tags_xml(&u.tags)
    )
}

fn group_xml(c: &Call, g: &Group) -> String {
    format!("{}{}{}{}{}", el("Path", &g.path), el("GroupName", &g.name), el("GroupId", &g.id), el("Arn", &c.arn("group", &g.path, &g.name)), el("CreateDate", &date(g.created_at)))
}

/// A user that must exist.
pub(super) fn user(c: &Call, name: &str) -> Result<User, StsError> {
    c.store.iam_user(&c.account, name).map_err(store_error)?.ok_or_else(|| no_user(name))
}

fn group(c: &Call, name: &str) -> Result<Group, StsError> {
    c.store.iam_group(&c.account, name).map_err(store_error)?.ok_or_else(|| no_group(name))
}

/// A managed policy ARN a boundary or attachment may name (AWS's built-ins or the account's).
pub(super) fn attachable(c: &Call, arn: &str) -> Result<(), StsError> {
    if crate::identity::managed_document(c.store, &c.account, arn).is_none() {
        return Err(no_such(format!("Policy {arn} does not exist or is not attachable.")));
    }
    Ok(())
}

async fn create_user(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("UserName", 64)?.to_string();
    let path = c.path("Path")?;
    c.allow("CreateUser", &c.arn("user", &path, &name))?;
    let boundary = c.p.get("PermissionsBoundary").map(str::to_string);
    if let Some(b) = &boundary {
        attachable(c, b)?;
    }
    let u = User { name: name.clone(), id: si::entity_id("AIDA"), path, created_at: c.store.now(), tags: c.tags()?, boundary, ..Default::default() };
    let acct = c.account.clone();
    let saved = u.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::USER, &name)? {
                return Err(StoreError::Conflict(format!("User with name {name} already exists.")));
            }
            if t.count(&acct, kind::USER)? >= si::MAX_USERS {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for UsersPerAccount: {}", si::MAX_USERS)));
            }
            t.put(&acct, kind::USER, &name, &saved)
        })
        .await
        .map_err(store_error)?;
    Ok(format!("<User>{}</User>", user_xml(c, &u)))
}

fn get_user(c: &Call) -> Result<String, StsError> {
    match c.user_or_self()? {
        Some(name) => {
            let u = user(c, &name)?;
            c.allow("GetUser", &c.arn("user", &u.path, &u.name))?;
            Ok(format!("<User>{}</User>", user_xml(c, &u)))
        }
        // The root user.
        None => Ok(format!(
            "<User>{}{}{}</User>",
            el("UserId", &c.account),
            el("Arn", &crate::identity::root_arn(&c.account)),
            el("CreateDate", &date(c.id.account.created_at))
        )),
    }
}

fn path_prefix(c: &Call) -> Result<String, StsError> {
    let p = c.p.get("PathPrefix").unwrap_or("/");
    if p.is_empty() || p.len() > 512 || !p.starts_with('/') {
        return Err(validation("1 validation error detected: Value at 'pathPrefix' failed to satisfy constraint: Member must satisfy regular expression pattern: \\u002F[\\u0021-\\u007F]*"));
    }
    Ok(p.to_string())
}

fn list_users(c: &Call) -> Result<String, StsError> {
    c.allow("ListUsers", &c.arn("user", "/", "*"))?;
    let prefix = path_prefix(c)?;
    let users: Vec<User> = c.store.iam_read(|t| t.list(&c.account, kind::USER)).map_err(store_error)?.into_iter().filter(|u: &User| u.path.starts_with(&prefix)).collect();
    page(c.p, users, |u| u.name.to_lowercase(), |u| user_xml(c, u), "Users")
}

async fn delete_user(c: &Call<'_>) -> Result<(), StsError> {
    let name = c.name("UserName", 64)?.to_string();
    let u = user(c, &name)?;
    c.allow("DeleteUser", &c.arn("user", &u.path, &u.name))?;
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            let Some(u) = t.get::<User>(&acct, kind::USER, &name)? else { return Err(StoreError::NotFound(format!("The user with name {name} cannot be found."))) };
            let conflict = |what: &str| Err(StoreError::DeleteConflict(format!("Cannot delete entity, must {what} first.")));
            if !t.user_keys(&acct, &u.name)?.is_empty() {
                return conflict("delete access keys");
            }
            if !u.policies.is_empty() || !u.attached.is_empty() {
                return conflict("delete policies");
            }
            if !u.groups.is_empty() {
                return conflict("remove users from group");
            }
            if !t.mfa_devices_of(&acct, Some(&u.name))?.is_empty() {
                return conflict("delete MFA device");
            }
            if u.login.is_some() {
                return conflict("delete login profile");
            }
            if t.list::<si::SigningCert>(&acct, kind::SIGNING_CERT)?.iter().any(|s| s.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name))) {
                return conflict("delete signing certificates");
            }
            if !u.ssh_keys.is_empty() {
                return conflict("delete SSH public keys");
            }
            t.delete(&acct, kind::USER, &name)?;
            Ok(())
        })
        .await
        .map_err(store_error)
}

async fn update_user(c: &Call<'_>) -> Result<(), StsError> {
    let name = c.name("UserName", 64)?.to_string();
    let u = user(c, &name)?;
    c.allow("UpdateUser", &c.arn("user", &u.path, &u.name))?;
    let new_name = match c.p.get("NewUserName") {
        Some(_) => Some(c.name("NewUserName", 64)?.to_string()),
        None => None,
    };
    let new_path = match c.p.get("NewPath") {
        Some(_) => Some(c.path("NewPath")?),
        None => None,
    };
    if let Some(n) = &new_name {
        c.allow("UpdateUser", &c.arn("user", new_path.as_deref().unwrap_or(&u.path), n))?;
    }
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            let Some(mut u) = t.get::<User>(&acct, kind::USER, &name)? else { return Err(StoreError::NotFound(format!("The user with name {name} cannot be found."))) };
            if let Some(p) = new_path {
                u.path = p;
            }
            match new_name {
                Some(n) if n != u.name => {
                    if !n.eq_ignore_ascii_case(&u.name) && t.exists(&acct, kind::USER, &n)? {
                        return Err(StoreError::Conflict(format!("User with name {n} already exists.")));
                    }
                    let old = u.name.clone();
                    t.delete(&acct, kind::USER, &old)?;
                    t.rename_user_keys(&acct, &old, &n)?;
                    for mut d in t.list::<si::MfaDevice>(&acct, kind::MFA)? {
                        if d.user.as_deref() == Some(old.as_str()) {
                            d.user = Some(n.clone());
                            let key = d.name.clone();
                            t.put(&acct, kind::MFA, &key, &d)?;
                        }
                    }
                    for mut s in t.list::<si::SigningCert>(&acct, kind::SIGNING_CERT)? {
                        if s.user.as_deref() == Some(old.as_str()) {
                            s.user = Some(n.clone());
                            let key = s.id.clone();
                            t.put(&acct, kind::SIGNING_CERT, &key, &s)?;
                        }
                    }
                    u.name = n.clone();
                    t.put(&acct, kind::USER, &n, &u)
                }
                _ => t.put(&acct, kind::USER, &name, &u),
            }
        })
        .await
        .map_err(store_error)
}

async fn edit_user(c: &Call<'_>, action: &str, f: impl FnOnce(&mut User) -> Result<(), StsError> + Send) -> Result<(), StsError> {
    let name = c.name("UserName", 64)?.to_string();
    let u = user(c, &name)?;
    c.allow(action, &c.arn("user", &u.path, &u.name))?;
    let acct = c.account.clone();
    let mut err = None;
    let r = c
        .store
        .iam_write(|t| {
            let Some(mut u) = t.get::<User>(&acct, kind::USER, &name)? else { return Err(StoreError::NotFound(format!("The user with name {name} cannot be found."))) };
            if let Err(e) = f(&mut u) {
                err = Some(e);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            t.put(&acct, kind::USER, &name, &u)
        })
        .await;
    match (r, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

async fn tag_user(c: &Call<'_>, add: bool) -> Result<(), StsError> {
    if add {
        let tags = c.tags()?;
        edit_user(c, "TagUser", |u| merge_tags(&mut u.tags, tags)).await
    } else {
        let keys = c.p.list("TagKeys")?;
        edit_user(c, "UntagUser", |u| {
            u.tags.retain(|(k, _)| !keys.iter().any(|x| x.eq_ignore_ascii_case(k)));
            Ok(())
        })
        .await
    }
}

fn list_user_tags(c: &Call) -> Result<String, StsError> {
    let u = user(c, c.name("UserName", 64)?)?;
    c.allow("ListUserTags", &c.arn("user", &u.path, &u.name))?;
    page(c.p, u.tags, |(k, _)| k.clone(), |(k, v)| format!("{}{}", el("Key", k), el("Value", v)), "Tags")
}

async fn user_boundary(c: &Call<'_>, put: bool) -> Result<(), StsError> {
    if put {
        let arn = c.p.required("PermissionsBoundary", "permissionsBoundary")?.to_string();
        attachable(c, &arn)?;
        edit_user(c, "PutUserPermissionsBoundary", |u| {
            u.boundary = Some(arn);
            Ok(())
        })
        .await
    } else {
        edit_user(c, "DeleteUserPermissionsBoundary", |u| {
            if u.boundary.take().is_none() {
                return Err(no_such(format!("The user with name {} has no permissions boundary.", u.name)));
            }
            Ok(())
        })
        .await
    }
}

async fn create_group(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("GroupName", 128)?.to_string();
    let path = c.path("Path")?;
    c.allow("CreateGroup", &c.arn("group", &path, &name))?;
    let g = Group { name: name.clone(), id: si::entity_id("AGPA"), path, created_at: c.store.now(), ..Default::default() };
    let acct = c.account.clone();
    let saved = g.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::GROUP, &name)? {
                return Err(StoreError::Conflict(format!("Group with name {name} already exists.")));
            }
            if t.count(&acct, kind::GROUP)? >= si::MAX_GROUPS {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for GroupsPerAccount: {}", si::MAX_GROUPS)));
            }
            t.put(&acct, kind::GROUP, &name, &saved)
        })
        .await
        .map_err(store_error)?;
    Ok(format!("<Group>{}</Group>", group_xml(c, &g)))
}

/// The users in a group.
fn members(c: &Call, g: &Group) -> Result<Vec<User>, StsError> {
    Ok(c.store.iam_read(|t| t.list::<User>(&c.account, kind::USER)).map_err(store_error)?.into_iter().filter(|u| u.groups.iter().any(|x| x.eq_ignore_ascii_case(&g.name))).collect())
}

fn get_group(c: &Call) -> Result<String, StsError> {
    let g = group(c, c.name("GroupName", 128)?)?;
    c.allow("GetGroup", &c.arn("group", &g.path, &g.name))?;
    let users = members(c, &g)?;
    Ok(format!("<Group>{}</Group>{}", group_xml(c, &g), page(c.p, users, |u| u.name.to_lowercase(), |u| user_xml(c, u), "Users")?))
}

fn list_groups(c: &Call) -> Result<String, StsError> {
    c.allow("ListGroups", &c.arn("group", "/", "*"))?;
    let prefix = path_prefix(c)?;
    let groups: Vec<Group> = c.store.iam_read(|t| t.list(&c.account, kind::GROUP)).map_err(store_error)?.into_iter().filter(|g: &Group| g.path.starts_with(&prefix)).collect();
    page(c.p, groups, |g| g.name.to_lowercase(), |g| group_xml(c, g), "Groups")
}

async fn delete_group(c: &Call<'_>) -> Result<(), StsError> {
    let g = group(c, c.name("GroupName", 128)?)?;
    c.allow("DeleteGroup", &c.arn("group", &g.path, &g.name))?;
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            let Some(g) = t.get::<Group>(&acct, kind::GROUP, &g.name)? else { return Err(StoreError::NotFound(format!("The group with name {} cannot be found.", g.name))) };
            if t.list::<User>(&acct, kind::USER)?.iter().any(|u| u.groups.iter().any(|x| x.eq_ignore_ascii_case(&g.name))) {
                return Err(StoreError::DeleteConflict("Cannot delete entity, must remove users from group first.".into()));
            }
            if !g.policies.is_empty() || !g.attached.is_empty() {
                return Err(StoreError::DeleteConflict("Cannot delete entity, must delete policies first.".into()));
            }
            t.delete(&acct, kind::GROUP, &g.name)?;
            Ok(())
        })
        .await
        .map_err(store_error)
}

async fn membership(c: &Call<'_>, add: bool) -> Result<(), StsError> {
    let g = group(c, c.name("GroupName", 128)?)?;
    c.allow(if add { "AddUserToGroup" } else { "RemoveUserFromGroup" }, &c.arn("group", &g.path, &g.name))?;
    let gname = g.name.clone();
    edit_user(c, if add { "AddUserToGroup" } else { "RemoveUserFromGroup" }, move |u| {
        let have = u.groups.iter().position(|x| x.eq_ignore_ascii_case(&gname));
        match (add, have) {
            (true, Some(_)) => Ok(()),
            (true, None) if u.groups.len() >= si::MAX_GROUPS_PER_USER => Err(limit(format!("Cannot exceed quota for GroupsPerUser: {}", si::MAX_GROUPS_PER_USER))),
            (true, None) => {
                u.groups.push(gname);
                Ok(())
            }
            (false, Some(i)) => {
                u.groups.remove(i);
                Ok(())
            }
            (false, None) => Err(no_such(format!("The user with name {} cannot be found in group {gname}.", u.name))),
        }
    })
    .await
}

fn list_groups_for_user(c: &Call) -> Result<String, StsError> {
    let u = user(c, c.name("UserName", 64)?)?;
    c.allow("ListGroupsForUser", &c.arn("user", &u.path, &u.name))?;
    let mut groups = Vec::new();
    for g in &u.groups {
        if let Some(g) = c.store.iam_group(&c.account, g).map_err(store_error)? {
            groups.push(g);
        }
    }
    page(c.p, groups, |g| g.name.to_lowercase(), |g| group_xml(c, g), "Groups")
}

// ---- Access keys ----

fn key_meta(user: Option<&str>, k: &AccessKey, secret: bool) -> String {
    format!(
        "{}{}{}{}{}",
        user.map(|u| el("UserName", u)).unwrap_or_default(),
        el("AccessKeyId", &k.id),
        el("Status", if k.active { "Active" } else { "Inactive" }),
        if secret { el("SecretAccessKey", &k.secret) } else { String::new() },
        el("CreateDate", &date(k.created_at))
    )
}

/// The keys a call is about: a user's, or (for the root user) the account's own.
fn keys_of(c: &Call, user: Option<&str>) -> Result<Vec<AccessKey>, StsError> {
    c.store
        .iam_read(|t| match user {
            Some(u) => t.user_keys(&c.account, u),
            None => Ok(t.account_keys(&c.account)?.into_iter().filter(|k| !k.id.starts_with(CONTROL_KEY_PREFIX)).collect()),
        })
        .map_err(store_error)
}

/// The ARN `iam:` key actions act on.
fn key_owner_arn(c: &Call, user: Option<&User>) -> String {
    match user {
        Some(u) => c.arn("user", &u.path, &u.name),
        None => crate::identity::root_arn(&c.account),
    }
}

fn target_user(c: &Call, action: &str) -> Result<Option<User>, StsError> {
    let u = match c.user_or_self()? {
        Some(n) => Some(user(c, &n)?),
        None => None,
    };
    if u.is_none() && !c.id.is_root() {
        return Err(validation("Must specify userName when calling with non-User credentials"));
    }
    c.allow(action, &key_owner_arn(c, u.as_ref()))?;
    Ok(u)
}

async fn create_access_key(c: &Call<'_>) -> Result<String, StsError> {
    let u = target_user(c, "CreateAccessKey")?;
    let quota = || limit(format!("Cannot exceed quota for AccessKeysPerUser: {}", si::MAX_KEYS_PER_USER));
    let k = match &u {
        Some(u) => {
            let (acct, name) = (c.account.clone(), u.name.clone());
            c.store
                .iam_write(move |t| {
                    if t.get::<User>(&acct, kind::USER, &name)?.is_none() {
                        return Err(StoreError::NotFound(format!("The user with name {name} cannot be found.")));
                    }
                    if t.user_keys(&acct, &name)?.len() >= si::MAX_KEYS_PER_USER {
                        return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for AccessKeysPerUser: {}", si::MAX_KEYS_PER_USER)));
                    }
                    t.create_user_key(&acct, &name)
                })
                .await
                .map_err(store_error)?
        }
        // The root user's own keys: account keys with admin over every bucket.
        None => {
            if keys_of(c, None)?.iter().filter(|k| k.is_root()).count() >= si::MAX_KEYS_PER_USER {
                return Err(quota());
            }
            c.store.create_key_in(&c.account, "root", Permission::Admin, None).await.map_err(store_error)?
        }
    };
    Ok(format!("<AccessKey>{}</AccessKey>", key_meta(u.as_ref().map(|u| u.name.as_str()), &k, true)))
}

fn list_access_keys(c: &Call) -> Result<String, StsError> {
    let u = target_user(c, "ListAccessKeys")?;
    let name = u.as_ref().map(|u| u.name.clone());
    let keys = keys_of(c, name.as_deref())?;
    let mut out = page(c.p, keys, |k| k.id.clone(), |k| key_meta(name.as_deref(), k, false), "AccessKeyMetadata")?;
    if let Some(n) = &name {
        out.push_str(&el("UserName", n));
    }
    Ok(out)
}

/// The key `AccessKeyId` names, if it's the target's.
fn owned_key(c: &Call, user: Option<&User>) -> Result<AccessKey, StsError> {
    let id = c.p.required("AccessKeyId", "accessKeyId")?;
    let k = c.store.iam_read(|t| t.key(id)).map_err(store_error)?;
    match k {
        Some(k) if k.account == c.account && k.user.as_deref() == user.map(|u| u.name.as_str()) && !k.id.starts_with(CONTROL_KEY_PREFIX) => Ok(k),
        _ => Err(no_such(format!("The Access Key with id {id} cannot be found."))),
    }
}

async fn update_access_key(c: &Call<'_>) -> Result<(), StsError> {
    let u = target_user(c, "UpdateAccessKey")?;
    let k = owned_key(c, u.as_ref())?;
    let active = match c.p.required("Status", "status")? {
        "Active" => true,
        "Inactive" => false,
        s => return Err(validation(format!("1 validation error detected: Value '{s}' at 'status' failed to satisfy constraint: Member must satisfy enum value set: [Active, Inactive]"))),
    };
    c.store.set_key_active(&k.id, active).await.map_err(store_error)
}

async fn delete_access_key(c: &Call<'_>) -> Result<(), StsError> {
    let u = target_user(c, "DeleteAccessKey")?;
    let k = owned_key(c, u.as_ref())?;
    c.store.delete_key(&k.id).await.map_err(store_error)
}

fn access_key_last_used(c: &Call) -> Result<String, StsError> {
    let id = c.p.required("AccessKeyId", "accessKeyId")?;
    let k = c.store.iam_read(|t| t.key(id)).map_err(store_error)?.filter(|k| k.account == c.account && !k.id.starts_with(CONTROL_KEY_PREFIX)).ok_or_else(|| no_such(format!("The Access Key with id {id} cannot be found.")))?;
    let owner = match &k.user {
        Some(n) => Some(user(c, n)?),
        None => None,
    };
    c.allow("GetAccessKeyLastUsed", &key_owner_arn(c, owner.as_ref()))?;
    let used = match k.last_used_at {
        Some(at) => format!("{}{}{}", el("LastUsedDate", &date(at)), el("ServiceName", "s3"), el("Region", &c.ctx.server.cfg.region)),
        None => format!("{}{}", el("ServiceName", "N/A"), el("Region", "N/A")),
    };
    Ok(format!("{}<AccessKeyLastUsed>{used}</AccessKeyLastUsed>", el("UserName", k.user.as_deref().unwrap_or("root"))))
}

// ---- Inline policies ----

#[derive(Clone, Copy)]
enum Target {
    User,
    Group,
}

impl Target {
    fn kind(self) -> &'static str {
        match self {
            Target::User => kind::USER,
            Target::Group => kind::GROUP,
        }
    }

    fn param(self) -> &'static str {
        match self {
            Target::User => "UserName",
            Target::Group => "GroupName",
        }
    }

    fn max_name(self) -> usize {
        match self {
            Target::User => 64,
            Target::Group => 128,
        }
    }

    fn max_bytes(self) -> usize {
        match self {
            Target::User => si::MAX_USER_INLINE_BYTES,
            Target::Group => si::MAX_GROUP_INLINE_BYTES,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Target::User => "user",
            Target::Group => "group",
        }
    }

    fn action(self, verb: &str) -> String {
        match self {
            Target::User => format!("{verb}UserPolicy"),
            Target::Group => format!("{verb}GroupPolicy"),
        }
    }
}

/// The entity's (path, name, inline policies).
fn inline_of(c: &Call, t: Target) -> Result<(String, String, std::collections::BTreeMap<String, String>), StsError> {
    let name = c.name(t.param(), t.max_name())?;
    Ok(match t {
        Target::User => {
            let u = user(c, name)?;
            (u.path, u.name, u.policies)
        }
        Target::Group => {
            let g = group(c, name)?;
            (g.path, g.name, g.policies)
        }
    })
}

async fn put_inline(c: &Call<'_>, t: Target) -> Result<(), StsError> {
    let (path, name, _) = inline_of(c, t)?;
    c.allow(&t.action("Put"), &c.arn(t.noun(), &path, &name))?;
    let policy = c.name("PolicyName", 128)?.to_string();
    let doc = c.document("PolicyDocument", t.max_bytes())?;
    let acct = c.account.clone();
    let max = t.max_bytes();
    let noun = t.noun();
    c.store
        .iam_write(move |tx| {
            let fit = |p: &std::collections::BTreeMap<String, String>| p.values().map(String::len).sum::<usize>() <= max;
            let over = || StoreError::LimitExceeded(format!("Maximum policy size of {max} bytes exceeded for {noun} {name}"));
            match t {
                Target::User => {
                    let mut u: User = tx.get(&acct, kind::USER, &name)?.ok_or_else(|| StoreError::NotFound(format!("The user with name {name} cannot be found.")))?;
                    u.policies.insert(policy, doc);
                    if !fit(&u.policies) {
                        return Err(over());
                    }
                    tx.put(&acct, t.kind(), &name, &u)
                }
                Target::Group => {
                    let mut g: Group = tx.get(&acct, kind::GROUP, &name)?.ok_or_else(|| StoreError::NotFound(format!("The group with name {name} cannot be found.")))?;
                    g.policies.insert(policy, doc);
                    if !fit(&g.policies) {
                        return Err(over());
                    }
                    tx.put(&acct, t.kind(), &name, &g)
                }
            }
        })
        .await
        .map_err(store_error)
}

fn get_inline(c: &Call, t: Target) -> Result<String, StsError> {
    let (path, name, policies) = inline_of(c, t)?;
    c.allow(&t.action("Get"), &c.arn(t.noun(), &path, &name))?;
    let policy = c.name("PolicyName", 128)?;
    let doc = policies.get(policy).ok_or_else(|| no_such(format!("The {} policy with name {policy} cannot be found.", t.noun())))?;
    Ok(format!("{}{}{}", el(t.param(), &name), el("PolicyName", policy), el("PolicyDocument", &url_encode(doc))))
}

fn list_inline(c: &Call, t: Target) -> Result<String, StsError> {
    let (path, name, policies) = inline_of(c, t)?;
    c.allow(&t.action("List"), &c.arn(t.noun(), &path, &name))?;
    let names: Vec<String> = policies.into_keys().collect();
    page(c.p, names, |n| n.clone(), |n| escape(n), "PolicyNames")
}

async fn delete_inline(c: &Call<'_>, t: Target) -> Result<(), StsError> {
    let (path, name, _) = inline_of(c, t)?;
    c.allow(&t.action("Delete"), &c.arn(t.noun(), &path, &name))?;
    let policy = c.name("PolicyName", 128)?.to_string();
    let acct = c.account.clone();
    c.store
        .iam_write(move |tx| {
            let gone = || StoreError::NotFound(format!("The policy with name {policy} cannot be found."));
            match t {
                Target::User => {
                    let mut u: User = tx.get(&acct, kind::USER, &name)?.ok_or_else(gone)?;
                    u.policies.remove(&policy).ok_or_else(gone)?;
                    tx.put(&acct, t.kind(), &name, &u)
                }
                Target::Group => {
                    let mut g: Group = tx.get(&acct, kind::GROUP, &name)?.ok_or_else(gone)?;
                    g.policies.remove(&policy).ok_or_else(gone)?;
                    tx.put(&acct, t.kind(), &name, &g)
                }
            }
        })
        .await
        .map_err(store_error)
}
