//! Virtual MFA devices, OpenID Connect providers and the account summary.

use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use s3_store::iam::{self as si, Group, ManagedPolicy, MfaDevice, OidcProviderEntry, User, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreateVirtualMFADevice" => create_device(c).await?,
        "EnableMFADevice" => return enable(c).await.map(|_| None),
        "ResyncMFADevice" => return resync(c).await.map(|_| None),
        "DeactivateMFADevice" => return deactivate(c).await.map(|_| None),
        "ListMFADevices" => list_devices(c)?,
        "ListVirtualMFADevices" => list_virtual(c)?,
        "DeleteVirtualMFADevice" => return delete_device(c).await.map(|_| None),
        "CreateOpenIDConnectProvider" => create_oidc(c).await?,
        "GetOpenIDConnectProvider" => get_oidc(c)?,
        "ListOpenIDConnectProviders" => list_oidc(c)?,
        "DeleteOpenIDConnectProvider" => return delete_oidc(c).await.map(|_| None),
        "GetAccountSummary" => summary(c)?,
        _ => unreachable!("routed here"),
    }))
}

fn serial_of(c: &Call, path: &str, name: &str) -> String {
    format!("arn:aws:iam::{}:mfa{path}{name}", c.account)
}

/// The device `SerialNumber` names (`None`: not one of this account's).
fn device(c: &Call) -> Result<(String, MfaDevice), StsError> {
    let serial = c.p.required("SerialNumber", "serialNumber")?;
    let gone = || no_such(format!("VirtualMFADevice with serial {serial} doesn't exist."));
    let name = si::mfa_name_of(&c.account, serial).ok_or_else(gone)?;
    let d = c.store.iam_read(|t| t.get::<MfaDevice>(&c.account, kind::MFA, &name)).map_err(store_error)?.ok_or_else(gone)?;
    Ok((name, d))
}

async fn create_device(c: &Call<'_>) -> Result<String, StsError> {
    let name = c.name("VirtualMFADeviceName", 226)?.to_string();
    let path = c.path("Path")?;
    let serial = serial_of(c, &path, &name);
    c.allow("CreateVirtualMFADevice", &serial)?;
    let seed = crate::mfa::new_seed();
    let key = format!("{}{name}", &path[1..]);
    let d = MfaDevice { name: key.clone(), serial: serial.clone(), seed: seed.clone(), created_at: c.store.now(), tags: c.tags()?, ..Default::default() };
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::MFA, &key)? {
                return Err(StoreError::Conflict("MFADevice entity at the same path and name already exists.".into()));
            }
            t.put(&acct, kind::MFA, &key, &d)
        })
        .await
        .map_err(store_error)?;
    let label = match &c.id.kind {
        Kind::User { name, .. } => name.clone(),
        _ => "root".into(),
    };
    let png = crate::mfa::qr_png(&crate::mfa::otpauth_uri(&c.account, &label, &seed)).unwrap_or_default();
    Ok(format!(
        "<VirtualMFADevice>{}{}{}</VirtualMFADevice>",
        el("SerialNumber", &serial),
        el("Base32StringSeed", &STANDARD.encode(seed.as_bytes())),
        el("QRCodePNG", &STANDARD.encode(png))
    ))
}

/// Whose device a call is about: `UserName`'s, or (without it) the caller's own (the root
/// user's when it's the root).
fn owner(c: &Call) -> Result<Option<User>, StsError> {
    match c.user_or_self()? {
        Some(n) => Ok(Some(super::users::user(c, &n)?)),
        None => Ok(None),
    }
}

fn owner_arn(c: &Call, u: &Option<User>) -> String {
    match u {
        Some(u) => c.arn("user", &u.path, &u.name),
        None => crate::identity::root_arn(&c.account),
    }
}

fn codes(c: &Call) -> Result<(u32, u32), StsError> {
    let code = |n: &str, f: &str| {
        let v = c.p.required(n, f)?;
        crate::mfa::parse_code(v).ok_or_else(|| validation(format!("1 validation error detected: Value at '{f}' failed to satisfy constraint: Member must satisfy regular expression pattern: [\\d]+ and have length 6")))
    };
    Ok((code("AuthenticationCode1", "authenticationCode1")?, code("AuthenticationCode2", "authenticationCode2")?))
}

fn invalid_code() -> StsError {
    StsError::new("InvalidAuthenticationCode", 403, "Authentication code for device is not valid.")
}

async fn enable(c: &Call<'_>) -> Result<(), StsError> {
    let u = owner(c)?;
    c.allow("EnableMFADevice", &owner_arn(c, &u))?;
    let (name, d) = device(c)?;
    if d.enabled_at.is_some() {
        return Err(exists("MFA device is already in use."));
    }
    let (first, second) = codes(c)?;
    let seed = crate::mfa::base32_decode(&d.seed).ok_or_else(invalid_code)?;
    let step = crate::mfa::consecutive(&seed, first, second, c.ctx.now_s()).ok_or_else(invalid_code)?;
    let acct = c.account.clone();
    let user = u.map(|u| u.name);
    c.store
        .iam_write(move |t| {
            let Some(mut d) = t.get::<MfaDevice>(&acct, kind::MFA, &name)? else { return Err(StoreError::NotFound("VirtualMFADevice doesn't exist.".into())) };
            if d.enabled_at.is_some() {
                return Err(StoreError::Conflict("MFA device is already in use.".into()));
            }
            if t.mfa_devices_of(&acct, user.as_deref())?.len() >= si::MAX_MFA_PER_USER {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for MFADevicesPerUser: {}", si::MAX_MFA_PER_USER)));
            }
            d.root = user.is_none();
            d.user = user;
            d.enabled_at = Some(t.now());
            d.last_step = step;
            t.put(&acct, kind::MFA, &name, &d)
        })
        .await
        .map_err(store_error)
}

async fn resync(c: &Call<'_>) -> Result<(), StsError> {
    let u = owner(c)?;
    c.allow("ResyncMFADevice", &owner_arn(c, &u))?;
    let (name, d) = device(c)?;
    let mine = match &u {
        Some(u) => d.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name)),
        None => d.root,
    };
    if d.enabled_at.is_none() || !mine {
        return Err(no_such("MFA device isn't enabled for this user."));
    }
    let (first, second) = codes(c)?;
    let seed = crate::mfa::base32_decode(&d.seed).ok_or_else(invalid_code)?;
    let step = crate::mfa::consecutive(&seed, first, second, c.ctx.now_s()).ok_or_else(invalid_code)?;
    if step <= d.last_step {
        return Err(invalid_code());
    }
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            let Some(mut d) = t.get::<MfaDevice>(&acct, kind::MFA, &name)? else { return Err(StoreError::NotFound("VirtualMFADevice doesn't exist.".into())) };
            d.last_step = d.last_step.max(step);
            t.put(&acct, kind::MFA, &name, &d)
        })
        .await
        .map_err(store_error)
}

async fn deactivate(c: &Call<'_>) -> Result<(), StsError> {
    let u = owner(c)?;
    c.allow("DeactivateMFADevice", &owner_arn(c, &u))?;
    let (name, d) = device(c)?;
    let mine = match &u {
        Some(u) => d.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&u.name)),
        None => d.root,
    };
    if d.enabled_at.is_none() || !mine {
        return Err(no_such(format!("MFA Device with serial number {} does not exist.", d.serial)));
    }
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            let Some(mut d) = t.get::<MfaDevice>(&acct, kind::MFA, &name)? else { return Err(StoreError::NotFound("VirtualMFADevice doesn't exist.".into())) };
            d.enabled_at = None;
            d.user = None;
            d.root = false;
            t.put(&acct, kind::MFA, &name, &d)
        })
        .await
        .map_err(store_error)
}

fn list_devices(c: &Call) -> Result<String, StsError> {
    let u = owner(c)?;
    c.allow("ListMFADevices", &owner_arn(c, &u))?;
    let name = u.as_ref().map(|u| u.name.clone());
    let devices = c.store.iam_read(|t| t.mfa_devices_of(&c.account, name.as_deref())).map_err(store_error)?;
    page(c.p, devices, |d| d.serial.clone(), |d| format!("{}{}{}", name.as_deref().map(|n| el("UserName", n)).unwrap_or_default(), el("SerialNumber", &d.serial), el("EnableDate", &date(d.enabled_at.unwrap_or_default()))), "MFADevices")
}

fn list_virtual(c: &Call) -> Result<String, StsError> {
    c.allow("ListVirtualMFADevices", &format!("arn:aws:iam::{}:mfa/*", c.account))?;
    let status = c.p.get("AssignmentStatus").unwrap_or("Any");
    let devices: Vec<MfaDevice> = c
        .store
        .iam_read(|t| t.list::<MfaDevice>(&c.account, kind::MFA))
        .map_err(store_error)?
        .into_iter()
        .filter(|d| match status {
            "Assigned" => d.enabled_at.is_some(),
            "Unassigned" => d.enabled_at.is_none(),
            _ => true,
        })
        .collect();
    let users: Vec<User> = c.store.iam_read(|t| t.list(&c.account, kind::USER)).map_err(store_error)?;
    page(
        c.p,
        devices,
        |d| d.serial.clone(),
        |d| {
            let user = match (&d.user, d.root, d.enabled_at) {
                (Some(n), _, Some(_)) => users.iter().find(|u| u.name.eq_ignore_ascii_case(n)).map(|u| format!("<User>{}</User>", super::users::user_xml(c, u))).unwrap_or_default(),
                (None, true, Some(_)) => format!("<User>{}{}</User>", el("UserId", &c.account), el("Arn", &crate::identity::root_arn(&c.account))),
                _ => String::new(),
            };
            format!("{}{user}{}", el("SerialNumber", &d.serial), d.enabled_at.map(|a| el("EnableDate", &date(a))).unwrap_or_default())
        },
        "VirtualMFADevices",
    )
}

async fn delete_device(c: &Call<'_>) -> Result<(), StsError> {
    let (name, d) = device(c)?;
    c.allow("DeleteVirtualMFADevice", &d.serial)?;
    if d.enabled_at.is_some() {
        return Err(delete_conflict("MFA VirtualDevice in use. Must deactivate first."));
    }
    let acct = c.account.clone();
    c.store.iam_write(move |t| t.delete(&acct, kind::MFA, &name).map(|_| ())).await.map_err(store_error)
}

fn oidc_arn(c: &Call, name: &str) -> String {
    format!("arn:aws:iam::{}:oidc-provider/{name}", c.account)
}

async fn create_oidc(c: &Call<'_>) -> Result<String, StsError> {
    let url = c.p.required("Url", "url")?.trim().to_string();
    let https = url.starts_with("https://") || (c.ctx.server.cfg.oidc_allow_http && url.starts_with("http://"));
    if !https || url.len() > 255 || url.contains(char::is_whitespace) || url.contains(['?', '#']) {
        return Err(validation("The URL must be an https:// URL of at most 255 characters, without a query or fragment."));
    }
    let entry = OidcProviderEntry {
        url: url.clone(),
        client_ids: c.p.list("ClientIDList")?,
        thumbprints: c.p.list("ThumbprintList")?,
        created_at: c.store.now(),
        tags: c.tags()?,
    };
    if entry.client_ids.len() > 100 || entry.client_ids.iter().any(|id| id.is_empty() || id.len() > 255) {
        return Err(validation("ClientIDList holds at most 100 client ids of 1-255 characters."));
    }
    if entry.thumbprints.len() > 5 || entry.thumbprints.iter().any(|t| t.len() != 40 || !t.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(validation("ThumbprintList holds at most 5 SHA-1 thumbprints of 40 hex characters."));
    }
    let name = entry.name();
    let arn = oidc_arn(c, &name);
    c.allow("CreateOpenIDConnectProvider", &arn)?;
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            if t.exists(&acct, kind::OIDC, &name)? {
                return Err(StoreError::Conflict(format!("Provider with url {url} already exists.")));
            }
            if t.count(&acct, kind::OIDC)? >= si::MAX_OIDC_PROVIDERS {
                return Err(StoreError::LimitExceeded(format!("Cannot exceed quota for OpenIdConnectProvidersPerAccount: {}", si::MAX_OIDC_PROVIDERS)));
            }
            t.put(&acct, kind::OIDC, &name, &entry)
        })
        .await
        .map_err(store_error)?;
    Ok(el("OpenIDConnectProviderArn", &arn))
}

fn oidc_of(c: &Call) -> Result<OidcProviderEntry, StsError> {
    let arn = c.p.required("OpenIDConnectProviderArn", "openIDConnectProviderArn")?;
    let gone = || no_such(format!("OpenIDConnect Provider not found for arn {arn}"));
    let name = arn.strip_prefix(&format!("arn:aws:iam::{}:oidc-provider/", c.account)).ok_or_else(gone)?;
    c.store.iam_oidc_provider(&c.account, name).map_err(store_error)?.ok_or_else(gone)
}

fn get_oidc(c: &Call) -> Result<String, StsError> {
    let e = oidc_of(c)?;
    c.allow("GetOpenIDConnectProvider", &oidc_arn(c, &e.name()))?;
    let list = |name: &str, items: &[String]| format!("<{name}>{}</{name}>", items.iter().map(|i| format!("<member>{}</member>", escape(i))).collect::<String>());
    Ok(format!("{}{}{}{}{}", el("Url", &e.name()), list("ClientIDList", &e.client_ids), list("ThumbprintList", &e.thumbprints), el("CreateDate", &date(e.created_at)), tags_xml(&e.tags)))
}

fn list_oidc(c: &Call) -> Result<String, StsError> {
    c.allow("ListOpenIDConnectProviders", "*")?;
    let all: Vec<OidcProviderEntry> = c.store.iam_read(|t| t.list(&c.account, kind::OIDC)).map_err(store_error)?;
    let members: String = all.iter().map(|e| format!("<member>{}</member>", el("Arn", &oidc_arn(c, &e.name())))).collect();
    Ok(format!("<OpenIDConnectProviderList>{members}</OpenIDConnectProviderList>"))
}

async fn delete_oidc(c: &Call<'_>) -> Result<(), StsError> {
    let e = oidc_of(c)?;
    let name = e.name();
    c.allow("DeleteOpenIDConnectProvider", &oidc_arn(c, &name))?;
    let acct = c.account.clone();
    c.store.iam_write(move |t| t.delete(&acct, kind::OIDC, &name).map(|_| ())).await.map_err(store_error)
}

fn summary(c: &Call) -> Result<String, StsError> {
    c.allow("GetAccountSummary", "*")?;
    let acct = &c.account;
    let (users, groups, policies, devices, oidc, roles, account_keys) = c
        .store
        .iam_read(|t| {
            Ok((
                t.list::<User>(acct, kind::USER)?,
                t.list::<Group>(acct, kind::GROUP)?,
                t.list::<ManagedPolicy>(acct, kind::POLICY)?,
                t.list::<MfaDevice>(acct, kind::MFA)?,
                t.count(acct, kind::OIDC)? + t.count(acct, kind::SAML)?,
                t.roles(acct)?,
                t.account_keys(acct)?,
            ))
        })
        .map_err(store_error)?;
    let (server_certs, signing) = c.store.iam_read(|t| Ok((t.count(acct, kind::SERVER_CERT)?, t.list::<si::SigningCert>(acct, kind::SIGNING_CERT)?))).map_err(store_error)?;
    let root_signing = signing.iter().any(|s| s.user.is_none());
    let in_use = devices.iter().filter(|d| d.enabled_at.is_some()).count();
    let root_mfa = devices.iter().any(|d| d.root && d.enabled_at.is_some());
    let root_keys = account_keys.iter().any(|k| k.is_root() && !k.id.starts_with(CONTROL_KEY_PREFIX));
    let versions: usize = policies.iter().map(|p| p.versions.len()).sum();
    let entries: Vec<(&str, usize)> = vec![
        ("Users", users.len()),
        ("UsersQuota", si::MAX_USERS),
        ("Groups", groups.len()),
        ("GroupsQuota", si::MAX_GROUPS),
        ("GroupsPerUserQuota", si::MAX_GROUPS_PER_USER),
        ("Roles", roles.len()),
        ("RolesQuota", si::MAX_ROLES),
        ("Policies", policies.len()),
        ("PoliciesQuota", si::MAX_POLICIES),
        ("PolicyVersionsInUse", versions),
        ("PolicyVersionsInUseQuota", 10_000),
        ("VersionsPerPolicyQuota", si::MAX_POLICY_VERSIONS),
        ("PolicySizeQuota", si::MAX_MANAGED_POLICY_BYTES),
        ("AttachedPoliciesPerUserQuota", si::MAX_ATTACHED),
        ("AttachedPoliciesPerGroupQuota", si::MAX_ATTACHED),
        ("AttachedPoliciesPerRoleQuota", si::MAX_ATTACHED),
        ("UserPolicySizeQuota", si::MAX_USER_INLINE_BYTES),
        ("GroupPolicySizeQuota", si::MAX_GROUP_INLINE_BYTES),
        ("RolePolicySizeQuota", si::MAX_ROLE_INLINE_BYTES),
        ("AssumeRolePolicySizeQuota", MAX_TRUST_POLICY_BYTES),
        ("AccessKeysPerUserQuota", si::MAX_KEYS_PER_USER),
        ("MFADevices", devices.len()),
        ("MFADevicesInUse", in_use),
        ("AccountMFAEnabled", usize::from(root_mfa)),
        ("AccountAccessKeysPresent", usize::from(root_keys)),
        ("AccountSigningCertificatesPresent", usize::from(root_signing)),
        ("SigningCertificatesPerUserQuota", si::MAX_SIGNING_CERTS_PER_USER),
        ("ServerCertificates", server_certs),
        ("ServerCertificatesQuota", si::MAX_SERVER_CERTS),
        ("Providers", oidc),
        ("InstanceProfiles", 0),
        ("InstanceProfilesQuota", 1000),
        ("GlobalEndpointTokenVersion", 1),
    ];
    let map: String = entries.iter().map(|(k, v)| format!("<entry><key>{k}</key><value>{v}</value></entry>")).collect();
    Ok(format!("<SummaryMap>{map}</SummaryMap>"))
}
