//! `GetAccountAuthorizationDetails` (every user, group, role and managed policy with their
//! policies) and the credential report (`GenerateCredentialReport`, `GetCredentialReport`: AWS's
//! CSV, one row for the root user and one per IAM user).

use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use s3_store::iam::{self as si, CredentialReport, Group, ManagedPolicy, MfaDevice, SigningCert, User, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "GetAccountAuthorizationDetails" => details(c)?,
        "GenerateCredentialReport" => generate(c).await?,
        "GetCredentialReport" => get_report(c)?,
        _ => unreachable!("routed here"),
    }))
}

/// A report stays current this long (AWS: four hours); after that GetCredentialReport says it
/// expired.
const REPORT_TTL_MS: i64 = 4 * 3_600_000;

fn inline_list(name: &str, policies: &std::collections::BTreeMap<String, String>) -> String {
    let m: String = policies.iter().map(|(n, d)| format!("<member>{}{}</member>", el("PolicyName", n), el("PolicyDocument", &url_encode(d)))).collect();
    format!("<{name}>{m}</{name}>")
}

fn attached_list(arns: &[String]) -> String {
    let m: String = arns.iter().map(|a| format!("<member>{}{}</member>", el("PolicyName", a.rsplit('/').next().unwrap_or(a)), el("PolicyArn", a))).collect();
    format!("<AttachedManagedPolicies>{m}</AttachedManagedPolicies>")
}

fn details(c: &Call) -> Result<String, StsError> {
    c.allow("GetAccountAuthorizationDetails", "*")?;
    let filter = c.p.list("Filter")?;
    let want = |k: &str| filter.is_empty() || filter.iter().any(|f| f == k);
    let acct = &c.account;
    let (users, groups, policies, roles) = c
        .store
        .iam_read(|t| Ok((t.list::<User>(acct, kind::USER)?, t.list::<Group>(acct, kind::GROUP)?, t.list::<ManagedPolicy>(acct, kind::POLICY)?, t.roles(acct)?)))
        .map_err(store_error)?;
    let mut items: Vec<(u8, String)> = Vec::new();
    if want("User") {
        for u in &users {
            let groups: String = u.groups.iter().map(|g| format!("<member>{}</member>", escape(g))).collect();
            items.push((
                0,
                format!(
                    "{}{}{}{}{}{}<GroupList>{groups}</GroupList>{}{}{}",
                    el("Path", &u.path),
                    el("UserName", &u.name),
                    el("UserId", &u.id),
                    el("Arn", &c.arn("user", &u.path, &u.name)),
                    el("CreateDate", &date(u.created_at)),
                    inline_list("UserPolicyList", &u.policies),
                    attached_list(&u.attached),
                    boundary_xml(&u.boundary),
                    tags_xml(&u.tags)
                ),
            ));
        }
    }
    if want("Group") {
        for g in &groups {
            items.push((
                1,
                format!(
                    "{}{}{}{}{}{}{}",
                    el("Path", &g.path),
                    el("GroupName", &g.name),
                    el("GroupId", &g.id),
                    el("Arn", &c.arn("group", &g.path, &g.name)),
                    el("CreateDate", &date(g.created_at)),
                    inline_list("GroupPolicyList", &g.policies),
                    attached_list(&g.attached)
                ),
            ));
        }
    }
    if want("Role") {
        for r in &roles {
            let path = if r.iam.path.is_empty() { "/" } else { r.iam.path.as_str() };
            items.push((
                2,
                format!(
                    "{}{}{}{}{}{}<InstanceProfileList/>{}{}{}{}<RoleLastUsed/>",
                    el("Path", path),
                    el("RoleName", &r.name),
                    el("RoleId", &r.id),
                    el("Arn", &r.own_arn()),
                    el("CreateDate", &date(r.created_at)),
                    el("AssumeRolePolicyDocument", &url_encode(&super::roles::trust_document(c.store, r))),
                    inline_list("RolePolicyList", &r.iam.policies),
                    attached_list(&r.iam.attached),
                    boundary_xml(&r.iam.boundary),
                    tags_xml(&r.iam.tags)
                ),
            ));
        }
    }
    let attachments = |arn: &str| users.iter().filter(|u| u.attached.iter().any(|a| a == arn)).count() + groups.iter().filter(|g| g.attached.iter().any(|a| a == arn)).count() + roles.iter().filter(|r| r.iam.attached.iter().any(|a| a == arn)).count();
    let boundaries = |arn: &str| users.iter().filter(|u| u.boundary.as_deref() == Some(arn)).count() + roles.iter().filter(|r| r.iam.boundary.as_deref() == Some(arn)).count();
    let policy_xml = |name: &str, id: &str, arn: &str, path: &str, default: u32, created: i64, updated: i64, versions: String| {
        format!(
            "{}{}{}{}{}<AttachmentCount>{}</AttachmentCount><PermissionsBoundaryUsageCount>{}</PermissionsBoundaryUsageCount><IsAttachable>true</IsAttachable>{}{}<PolicyVersionList>{versions}</PolicyVersionList>",
            el("PolicyName", name),
            el("PolicyId", id),
            el("Arn", arn),
            el("Path", path),
            el("DefaultVersionId", &format!("v{default}")),
            attachments(arn),
            boundaries(arn),
            el("CreateDate", &date(created)),
            el("UpdateDate", &date(updated))
        )
    };
    if want("LocalManagedPolicy") {
        for p in &policies {
            let arn = c.arn("policy", &p.path, &p.name);
            let versions: String = p
                .versions
                .iter()
                .map(|v| format!("<member>{}{}<IsDefaultVersion>{}</IsDefaultVersion>{}</member>", el("Document", &url_encode(&v.document)), el("VersionId", &format!("v{}", v.id)), v.id == p.default_version, el("CreateDate", &date(v.created_at))))
                .collect();
            items.push((3, policy_xml(&p.name, &p.id, &arn, if p.path.is_empty() { "/" } else { &p.path }, p.default_version, p.created_at, p.updated_at, versions)));
        }
    }
    if want("AWSManagedPolicy") {
        for (arn, id, doc) in crate::identity::AWS_MANAGED {
            // Only the ones in use, as AWS lists them.
            if attachments(arn) + boundaries(arn) == 0 {
                continue;
            }
            let versions = format!("<member>{}<VersionId>v1</VersionId><IsDefaultVersion>true</IsDefaultVersion>{}</member>", el("Document", &url_encode(doc)), el("CreateDate", &date(0)));
            items.push((3, policy_xml(arn.rsplit('/').next().unwrap_or(arn), id, arn, "/", 1, 0, 0, versions)));
        }
    }
    // One page across the lists (the marker counts entities).
    let max = match c.p.get("MaxItems") {
        None => 100,
        Some(m) => m.trim().parse::<usize>().ok().filter(|n| (1..=1000).contains(n)).ok_or_else(|| validation("1 validation error detected: Value at 'maxItems' failed to satisfy constraint: Member must have value between 1 and 1000"))?,
    };
    let from: usize = match c.p.get("Marker") {
        Some(m) => m.parse().map_err(|_| invalid_input("Invalid Marker"))?,
        None => 0,
    };
    let page_items: Vec<&(u8, String)> = items.iter().skip(from).take(max).collect();
    let truncated = from + page_items.len() < items.len();
    let list = |kind: u8, name: &str| format!("<{name}>{}</{name}>", page_items.iter().filter(|(k, _)| *k == kind).map(|(_, x)| format!("<member>{x}</member>")).collect::<String>());
    let mut out = format!("{}{}{}{}<IsTruncated>{truncated}</IsTruncated>", list(0, "UserDetailList"), list(1, "GroupDetailList"), list(2, "RoleDetailList"), list(3, "Policies"));
    if truncated {
        out.push_str(&el("Marker", &(from + page_items.len()).to_string()));
    }
    Ok(out)
}

/// A credential report date (`2026-01-02T03:04:05+00:00`), or what AWS writes without one.
fn when(ms: Option<i64>, none: &str) -> String {
    match ms {
        Some(t) => date(t).trim_end_matches('Z').to_string() + "+00:00",
        None => none.to_string(),
    }
}

fn csv(c: &Call) -> Result<String, StsError> {
    let acct = &c.account;
    let (users, devices, certs, root_keys) = c
        .store
        .iam_read(|t| Ok((t.list::<User>(acct, kind::USER)?, t.list::<MfaDevice>(acct, kind::MFA)?, t.list::<SigningCert>(acct, kind::SIGNING_CERT)?, t.account_keys(acct)?)))
        .map_err(store_error)?;
    let policy = super::login::account_policy(c.store, acct)?;
    let mut out = String::from(
        "user,arn,user_creation_time,password_enabled,password_last_used,password_last_changed,password_next_rotation,mfa_active,access_key_1_active,access_key_1_last_rotated,access_key_1_last_used_date,access_key_1_last_used_region,access_key_1_last_used_service,access_key_2_active,access_key_2_last_rotated,access_key_2_last_used_date,access_key_2_last_used_region,access_key_2_last_used_service,cert_1_active,cert_1_last_rotated,cert_2_active,cert_2_last_rotated\n",
    );
    let region = c.ctx.server.cfg.region.clone();
    let keys_cols = |keys: &[s3_store::AccessKey]| {
        let mut cols = Vec::new();
        for i in 0..2 {
            match keys.get(i) {
                Some(k) => {
                    cols.push(k.active.to_string());
                    cols.push(when(Some(k.created_at), "N/A"));
                    cols.push(when(k.last_used_at, "N/A"));
                    cols.push(if k.last_used_at.is_some() { region.clone() } else { "N/A".into() });
                    cols.push(if k.last_used_at.is_some() { "s3".into() } else { "N/A".into() });
                }
                None => cols.extend(["false", "N/A", "N/A", "N/A", "N/A"].map(String::from)),
            }
        }
        cols.join(",")
    };
    let cert_cols = |mine: Vec<&SigningCert>| {
        let mut cols = Vec::new();
        for i in 0..2 {
            match mine.get(i) {
                Some(s) => {
                    cols.push(s.active.to_string());
                    cols.push(when(Some(s.uploaded_at), "N/A"));
                }
                None => cols.extend(["false", "N/A"].map(String::from)),
            }
        }
        cols.join(",")
    };
    // The root user.
    let root: Vec<s3_store::AccessKey> = root_keys.into_iter().filter(|k| k.is_root() && !k.id.starts_with(CONTROL_KEY_PREFIX)).collect();
    let root_mfa = devices.iter().any(|d| d.root && d.enabled_at.is_some());
    out.push_str(&format!(
        "<root_account>,{},{},not_supported,no_information,not_supported,not_supported,{root_mfa},{},{}\n",
        crate::identity::root_arn(acct),
        when(Some(c.id.account.created_at), "N/A"),
        keys_cols(&root),
        cert_cols(certs.iter().filter(|s| s.user.is_none()).collect())
    ));
    for u in &users {
        let keys = c.store.iam_read(|t| t.user_keys(acct, &u.name)).map_err(store_error)?;
        let mfa = devices.iter().any(|d| d.enabled_at.is_some() && d.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name)));
        let (enabled, used, changed, next) = match &u.login {
            Some(l) => (
                "true",
                when(l.last_used_at, "no_information"),
                when(Some(l.changed_at), "N/A"),
                policy.as_ref().and_then(|p| p.max_age_days).map_or("N/A".to_string(), |d| when(Some(l.changed_at + i64::from(d) * 86_400_000), "N/A")),
            ),
            None => ("false", "N/A".to_string(), "N/A".to_string(), "N/A".to_string()),
        };
        out.push_str(&format!(
            "{},{},{},{enabled},{used},{changed},{next},{mfa},{},{}\n",
            u.name,
            c.arn("user", &u.path, &u.name),
            when(Some(u.created_at), "N/A"),
            keys_cols(&keys),
            cert_cols(certs.iter().filter(|s| s.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name))).collect())
        ));
    }
    Ok(out)
}

async fn generate(c: &Call<'_>) -> Result<String, StsError> {
    c.allow("GenerateCredentialReport", "*")?;
    let now = c.store.now();
    let have: Option<CredentialReport> = c.store.iam_read(|t| t.get(&c.account, kind::CREDENTIAL_REPORT, si::ACCOUNT)).map_err(store_error)?;
    if have.is_some_and(|r| now - r.generated_at < REPORT_TTL_MS) {
        return Ok("<State>COMPLETE</State>".into());
    }
    let r = CredentialReport { generated_at: now, csv: csv(c)? };
    let acct = c.account.clone();
    c.store.iam_write(move |t| t.put(&acct, kind::CREDENTIAL_REPORT, si::ACCOUNT, &r)).await.map_err(store_error)?;
    Ok("<State>STARTED</State><Description>No report exists. Starting a new report generation task</Description>".into())
}

fn get_report(c: &Call) -> Result<String, StsError> {
    c.allow("GetCredentialReport", "*")?;
    let r: CredentialReport = c
        .store
        .iam_read(|t| t.get(&c.account, kind::CREDENTIAL_REPORT, si::ACCOUNT))
        .map_err(store_error)?
        .ok_or_else(|| StsError::new("ReportNotPresent", 410, "Credential report not present. Use GenerateCredentialReport to generate one."))?;
    if c.store.now() - r.generated_at >= REPORT_TTL_MS {
        return Err(StsError::new("ReportExpired", 410, "Credential report expired. Use GenerateCredentialReport to generate a new one."));
    }
    Ok(format!("{}<ReportFormat>text/csv</ReportFormat>{}", el("Content", &STANDARD.encode(r.csv.as_bytes())), el("GeneratedTime", &date(r.generated_at))))
}
