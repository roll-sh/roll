//! IAM roles: `CreateRole` with an `AssumeRolePolicyDocument` (trust policy), inline policies,
//! permissions boundaries, tags. Control-plane roles (`/v1/storage/roles`) show through IAM
//! too: their trust as the equivalent trust policy, their permission level as roll's.

use super::*;
use s3_store::sts::{self as st, IamRoleSpec, Role};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreateRole" => create(c).await?,
        "GetRole" => get(c)?,
        "ListRoles" => list(c)?,
        "DeleteRole" => return delete(c).await.map(|_| None),
        "UpdateRole" | "UpdateRoleDescription" => {
            let r = update(c, action).await?;
            if action == "UpdateRole" {
                return Ok(None);
            }
            format!("<Role>{}</Role>", role_xml(c.store, &r))
        }
        "UpdateAssumeRolePolicy" => return update_trust(c).await.map(|_| None),
        "PutRolePolicy" => return put_inline(c).await.map(|_| None),
        "GetRolePolicy" => get_inline(c)?,
        "ListRolePolicies" => list_inline(c)?,
        "DeleteRolePolicy" => return delete_inline(c).await.map(|_| None),
        "PutRolePermissionsBoundary" => return boundary(c, true).await.map(|_| None),
        "DeleteRolePermissionsBoundary" => return boundary(c, false).await.map(|_| None),
        "TagRole" => return tag(c, true).await.map(|_| None),
        "UntagRole" => return tag(c, false).await.map(|_| None),
        "ListRoleTags" => list_tags(c)?,
        _ => unreachable!("routed here"),
    }))
}

/// A role's trust policy as it reads now (principals deleted since show as their unique ids),
/// or a control-plane role's trust as the equivalent trust policy.
pub(crate) fn trust_document(store: &s3_store::Store, r: &Role) -> String {
    if let Some(d) = &r.iam.trust_policy {
        return crate::principals::rewrite(store, d, &r.iam.trust_principals).into_owned();
    }
    let mut statements = Vec::new();
    if !r.trust.keys.is_empty() {
        let principals: Vec<String> = r.trust.keys.iter().map(|k| format!("arn:aws:iam::{}:user/{k}", r.account)).collect();
        let mut s = serde_json::json!({"Effect": "Allow", "Principal": {"AWS": principals}, "Action": "sts:AssumeRole"});
        if r.trust.external_id.is_some() {
            // The external id itself is a secret of the trust; it isn't shown.
            s["Condition"] = serde_json::json!({"Null": {"sts:ExternalId": "false"}});
        }
        statements.push(s);
    }
    for p in &r.trust.oidc {
        let name = p.name();
        let mut cond = serde_json::json!({"StringEquals": {format!("{name}:aud"): p.audiences}});
        if let Some(sub) = &p.subject {
            cond["StringLike"] = serde_json::json!({format!("{name}:sub"): sub});
        }
        statements.push(serde_json::json!({"Effect": "Allow", "Principal": {"Federated": format!("arn:aws:iam::{}:oidc-provider/{name}", r.account)}, "Action": "sts:AssumeRoleWithWebIdentity", "Condition": cond}));
    }
    serde_json::json!({"Version": "2012-10-17", "Statement": statements}).to_string()
}

pub(crate) fn role_xml(store: &s3_store::Store, r: &Role) -> String {
    let path = if r.iam.path.is_empty() { "/" } else { r.iam.path.as_str() };
    format!(
        "{}{}{}{}{}{}{}<MaxSessionDuration>{}</MaxSessionDuration>{}{}<RoleLastUsed/>",
        el("Path", path),
        el("RoleName", &r.name),
        el("RoleId", &r.id),
        el("Arn", &r.own_arn()),
        el("CreateDate", &date(r.created_at)),
        el("AssumeRolePolicyDocument", &url_encode(&trust_document(store, r))),
        if r.description.is_empty() { String::new() } else { el("Description", &r.description) },
        r.max_session_s,
        boundary_xml(&r.iam.boundary),
        tags_xml(&r.iam.tags)
    )
}

fn role(c: &Call) -> Result<Role, StsError> {
    let name = c.name("RoleName", 64)?;
    c.store.role_in(&c.account, name).map_err(store_error)?.ok_or_else(|| no_such(format!("The role with name {name} cannot be found.")))
}

/// A trust policy parameter, checked and minified, with the users and roles its principals
/// name (each must exist, as on AWS).
fn trust_policy(c: &Call, param: &str) -> Result<(String, std::collections::BTreeMap<String, String>), StsError> {
    let doc = c.p.required(param, "assumeRolePolicyDocument")?;
    if doc.len() > MAX_TRUST_POLICY_BYTES {
        return Err(limit(format!("Cannot exceed quota for ACLSizePerRole: {MAX_TRUST_POLICY_BYTES}")));
    }
    Policy::parse_trust(doc.as_bytes(), MAX_TRUST_POLICY_BYTES).map_err(|e| StsError::new("MalformedPolicyDocument", 400, e.message))?;
    let doc = serde_json::from_str::<serde_json::Value>(doc).map(|v| v.to_string()).map_err(|e| StsError::new("MalformedPolicyDocument", 400, e.to_string()))?;
    let bound = crate::principals::bind(c.store, &doc).map_err(|e| match e {
        Some(p) => StsError::new("MalformedPolicyDocument", 400, format!("{}: {p}", crate::principals::INVALID)),
        None => StsError::new("ServiceFailure", 500, "The request processing has failed because of an unknown error, exception or failure."),
    })?;
    Ok((doc, bound))
}

fn max_session(c: &Call) -> Result<Option<i64>, StsError> {
    match c.p.get("MaxSessionDuration") {
        None => Ok(None),
        Some(v) => v.trim().parse::<i64>().ok().filter(|n| (st::MIN_SESSION_S..=st::ROLE_MAX_SESSION_S).contains(n)).map(Some).ok_or_else(|| validation(format!("The requested MaxSessionDuration {v} is not between {} and {}.", st::MIN_SESSION_S, st::ROLE_MAX_SESSION_S))),
    }
}

fn description(c: &Call) -> Result<Option<String>, StsError> {
    match c.p.get("Description") {
        Some(d) if d.chars().count() > 1000 => Err(validation("1 validation error detected: Value at 'description' failed to satisfy constraint: Member must have length less than or equal to 1000")),
        d => Ok(d.map(str::to_string)),
    }
}

async fn create(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("RoleName", 64)?.to_string();
    let path = c.path("Path")?;
    c.allow("CreateRole", &c.arn("role", &path, &name))?;
    let boundary = c.p.get("PermissionsBoundary").map(str::to_string);
    if let Some(b) = &boundary {
        super::users::attachable(c, b)?;
    }
    let (trust, trust_principals) = trust_policy(c, "AssumeRolePolicyDocument")?;
    let spec = IamRoleSpec { path, trust_policy: trust, description: description(c)?.unwrap_or_default(), max_session_s: max_session(c)?, boundary, tags: c.tags()?, trust_principals };
    let r = c.store.create_iam_role(&c.account, &name, spec).await.map_err(store_error)?;
    Ok(format!("<Role>{}</Role>", role_xml(c.store, &r)))
}

fn get(c: &Call) -> Result<String, StsError> {
    let r = role(c)?;
    c.allow("GetRole", &r.own_arn())?;
    Ok(format!("<Role>{}</Role>", role_xml(c.store, &r)))
}

fn list(c: &Call) -> Result<String, StsError> {
    c.allow("ListRoles", &c.arn("role", "/", "*"))?;
    let prefix = c.p.get("PathPrefix").unwrap_or("/");
    let roles: Vec<Role> = c.store.roles_in(&c.account).map_err(store_error)?.into_iter().filter(|r| (if r.iam.path.is_empty() { "/" } else { r.iam.path.as_str() }).starts_with(prefix)).collect();
    page(c.p, roles, |r| r.name.to_lowercase(), |r| role_xml(c.store, r), "Roles")
}

async fn delete(c: &Call<'_>) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow("DeleteRole", &r.own_arn())?;
    if !r.iam.attached.is_empty() || r.iam.policies.keys().any(|k| k != st::REVOKE_POLICY_NAME) {
        return Err(delete_conflict("Cannot delete entity, must detach all policies first."));
    }
    c.store.delete_role_in(&c.account, &r.name).await.map_err(store_error)
}

/// Edit a role (one transaction); errors of `f` come back as they are.
async fn edit(c: &Call<'_>, r: &Role, f: impl FnOnce(&mut Role) -> Result<(), StsError>) -> Result<Role, StsError> {
    let mut err = None;
    let out = c
        .store
        .edit_role(&c.account, &r.name, |_, r| {
            if let Err(e) = f(r) {
                err = Some(e);
                return Err(StoreError::InvalidRequest("refused".into()));
            }
            Ok(())
        })
        .await;
    match (out, err) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(store_error),
    }
}

async fn update(c: &Call<'_>, action: &str) -> Result<Role, StsError> {
    let r = role(c)?;
    c.allow(action, &r.own_arn())?;
    let d = description(c)?;
    let max = max_session(c)?;
    edit(c, &r, |r| {
        if let Some(d) = d {
            r.description = d;
        }
        if let Some(m) = max {
            r.max_session_s = m;
        }
        Ok(())
    })
    .await
}

async fn update_trust(c: &Call<'_>) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow("UpdateAssumeRolePolicy", &r.own_arn())?;
    let (doc, bound) = trust_policy(c, "PolicyDocument")?;
    edit(c, &r, |r| {
        r.iam.trust_policy = Some(doc);
        r.iam.trust_principals = bound;
        Ok(())
    })
    .await
    .map(|_| ())
}

async fn put_inline(c: &Call<'_>) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow("PutRolePolicy", &r.own_arn())?;
    let name = c.name("PolicyName", 128)?.to_string();
    let doc = c.document("PolicyDocument", s3_store::iam::MAX_ROLE_INLINE_BYTES)?;
    let role_name = r.name.clone();
    edit(c, &r, |r| {
        r.iam.policies.insert(name, doc);
        if r.iam.policies.values().map(String::len).sum::<usize>() > s3_store::iam::MAX_ROLE_INLINE_BYTES {
            return Err(limit(format!("Maximum policy size of {} bytes exceeded for role {role_name}", s3_store::iam::MAX_ROLE_INLINE_BYTES)));
        }
        Ok(())
    })
    .await
    .map(|_| ())
}

fn get_inline(c: &Call) -> Result<String, StsError> {
    let r = role(c)?;
    c.allow("GetRolePolicy", &r.own_arn())?;
    let name = c.name("PolicyName", 128)?;
    let doc = r.iam.policies.get(name).ok_or_else(|| no_such(format!("The role policy with name {name} cannot be found.")))?;
    Ok(format!("{}{}{}", el("RoleName", &r.name), el("PolicyName", name), el("PolicyDocument", &url_encode(doc))))
}

fn list_inline(c: &Call) -> Result<String, StsError> {
    let r = role(c)?;
    c.allow("ListRolePolicies", &r.own_arn())?;
    page(c.p, r.iam.policies.into_keys().collect(), |n: &String| n.clone(), |n| escape(n), "PolicyNames")
}

async fn delete_inline(c: &Call<'_>) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow("DeleteRolePolicy", &r.own_arn())?;
    let name = c.name("PolicyName", 128)?.to_string();
    edit(c, &r, |r| r.iam.policies.remove(&name).map(|_| ()).ok_or_else(|| no_such(format!("The role policy with name {name} cannot be found."))))
        .await
        .map(|_| ())
}

async fn boundary(c: &Call<'_>, put: bool) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow(if put { "PutRolePermissionsBoundary" } else { "DeleteRolePermissionsBoundary" }, &r.own_arn())?;
    let arn = if put {
        let a = c.p.required("PermissionsBoundary", "permissionsBoundary")?.to_string();
        super::users::attachable(c, &a)?;
        Some(a)
    } else {
        None
    };
    edit(c, &r, |r| {
        if arn.is_none() && r.iam.boundary.is_none() {
            return Err(no_such(format!("The role with name {} has no permissions boundary.", r.name)));
        }
        r.iam.boundary = arn;
        Ok(())
    })
    .await
    .map(|_| ())
}

async fn tag(c: &Call<'_>, add: bool) -> Result<(), StsError> {
    let r = role(c)?;
    c.allow(if add { "TagRole" } else { "UntagRole" }, &r.own_arn())?;
    if add {
        let tags = c.tags()?;
        edit(c, &r, |r| merge_tags(&mut r.iam.tags, tags)).await.map(|_| ())
    } else {
        let keys = c.p.list("TagKeys")?;
        edit(c, &r, |r| {
            r.iam.tags.retain(|(k, _)| !keys.iter().any(|x| x.eq_ignore_ascii_case(k)));
            Ok(())
        })
        .await
        .map(|_| ())
    }
}

fn list_tags(c: &Call) -> Result<String, StsError> {
    let r = role(c)?;
    c.allow("ListRoleTags", &r.own_arn())?;
    page(c.p, r.iam.tags, |(k, _)| k.clone(), |(k, v)| format!("{}{}", el("Key", k), el("Value", v)), "Tags")
}
