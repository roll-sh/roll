//! Console passwords (login profiles), the account's password policy, users changing their own
//! password, account aliases, and signing in with a password (the dashboard's sign-in,
//! `console.rs`).

use super::*;
use crate::password;
use s3_store::iam::{self as si, LoginProfile, PasswordPolicy, User, kind};

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "CreateLoginProfile" => create(c).await?,
        "GetLoginProfile" => get(c)?,
        "UpdateLoginProfile" => return update(c).await.map(|_| None),
        "DeleteLoginProfile" => return delete(c).await.map(|_| None),
        "ChangePassword" => return change_password(c).await.map(|_| None),
        "GetAccountPasswordPolicy" => get_policy(c)?,
        "UpdateAccountPasswordPolicy" => return update_policy(c).await.map(|_| None),
        "DeleteAccountPasswordPolicy" => return delete_policy(c).await.map(|_| None),
        "CreateAccountAlias" => return create_alias(c).await.map(|_| None),
        "ListAccountAliases" => list_aliases(c)?,
        "DeleteAccountAlias" => return delete_alias(c).await.map(|_| None),
        _ => unreachable!("routed here"),
    }))
}

pub(crate) fn violation(m: impl Into<String>) -> StsError {
    StsError::new("PasswordPolicyViolation", 400, m)
}

/// The account's password policy (`None`: AWS's default one applies).
pub(crate) fn account_policy(store: &Store, account: &str) -> Result<Option<PasswordPolicy>, StsError> {
    store.iam_read(|t| t.get(account, kind::PASSWORD_POLICY, si::ACCOUNT)).map_err(store_error)
}

/// Check a new password against the account's policy, and (`previous`) that it isn't one of
/// the last `PasswordReusePrevention` ones.
pub(crate) async fn check_new(store: &Store, account: &s3_store::Account, password: &str, previous: &[String]) -> Result<Option<PasswordPolicy>, StsError> {
    let policy = account_policy(store, &account.id)?;
    let names: Vec<String> = [Some(account.name.clone()), account.email.clone()].into_iter().flatten().collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    password::check(policy.as_ref(), password, &names).map_err(violation)?;
    if let Some(n) = policy.as_ref().and_then(|p| p.reuse_prevention)
        && password::reused(password, &previous[..previous.len().min(n as usize)]).await
    {
        return Err(violation(format!("Password should not be one of your {n} previous passwords")));
    }
    Ok(policy)
}

/// A new login profile's (or a changed password's) record.
pub(crate) async fn set_password(lp: Option<&LoginProfile>, password: &str, reset_required: bool, keep: usize, now: i64) -> Result<LoginProfile, StsError> {
    let hash = password::hash(password).await.map_err(|e| {
        tracing::error!(error = e, "hashing a password failed");
        StsError::new("ServiceFailure", 500, "The request processing has failed because of an unknown error, exception or failure.")
    })?;
    let mut previous = Vec::new();
    if let Some(old) = lp {
        previous.push(old.hash.clone());
        previous.extend(old.previous.iter().cloned());
    }
    previous.truncate(keep);
    Ok(LoginProfile { hash, created_at: lp.map_or(now, |l| l.created_at), changed_at: now, reset_required, previous, last_used_at: lp.and_then(|l| l.last_used_at) })
}

/// How many earlier hashes to keep.
fn keep(policy: &Option<PasswordPolicy>) -> usize {
    policy.as_ref().and_then(|p| p.reuse_prevention).unwrap_or(0) as usize
}

fn bool_param(c: &Call, name: &str) -> Result<Option<bool>, StsError> {
    match c.p.get(name) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(v) => Err(validation(format!("1 validation error detected: Value '{v}' at '{}' failed to satisfy constraint: Member must be a boolean", lower_first(name)))),
    }
}

fn profile_xml(u: &User, lp: &LoginProfile) -> String {
    format!("<LoginProfile>{}{}<PasswordResetRequired>{}</PasswordResetRequired></LoginProfile>", el("UserName", &u.name), el("CreateDate", &date(lp.created_at)), lp.reset_required)
}

fn no_profile(name: &str) -> StsError {
    no_such(format!("Login Profile for User {name} cannot be found."))
}

/// The user a login-profile call is about (`UserName`, or the calling user).
fn target(c: &Call) -> Result<User, StsError> {
    let name = c.user_or_self()?.ok_or_else(|| validation("Must specify userName when calling with non-User credentials"))?;
    super::users::user(c, &name)
}

/// Change a user's document (one transaction); `f`'s errors come back as they are.
async fn edit(c: &Call<'_>, name: &str, f: impl FnOnce(&mut User) -> Result<(), StsError> + Send) -> Result<(), StsError> {
    let acct = c.account.clone();
    let name = name.to_string();
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

async fn create(c: &Call<'_>) -> Result<String, StsError> {
    let u = target(c)?;
    c.allow("CreateLoginProfile", &c.arn("user", &u.path, &u.name))?;
    let pw = c.p.required("Password", "password")?;
    if u.login.is_some() {
        return Err(exists(format!("Login Profile for user {} already exists.", u.name)));
    }
    let policy = check_new(c.store, &c.id.account, pw, &[]).await?;
    let reset = bool_param(c, "PasswordResetRequired")?.unwrap_or(false);
    let lp = set_password(None, pw, reset, keep(&policy), c.store.now()).await?;
    let saved = lp.clone();
    let name = u.name.clone();
    edit(c, &u.name, move |u| {
        if u.login.is_some() {
            return Err(exists(format!("Login Profile for user {name} already exists.")));
        }
        u.login = Some(saved);
        Ok(())
    })
    .await?;
    Ok(profile_xml(&u, &lp))
}

fn get(c: &Call) -> Result<String, StsError> {
    let u = target(c)?;
    c.allow("GetLoginProfile", &c.arn("user", &u.path, &u.name))?;
    let lp = u.login.clone().ok_or_else(|| no_profile(&u.name))?;
    Ok(profile_xml(&u, &lp))
}

async fn update(c: &Call<'_>) -> Result<(), StsError> {
    let u = target(c)?;
    c.allow("UpdateLoginProfile", &c.arn("user", &u.path, &u.name))?;
    let lp = u.login.clone().ok_or_else(|| no_profile(&u.name))?;
    let reset = bool_param(c, "PasswordResetRequired")?;
    let new = match c.p.get("Password") {
        Some(pw) => {
            let mut prev = vec![lp.hash.clone()];
            prev.extend(lp.previous.iter().cloned());
            let policy = check_new(c.store, &c.id.account, pw, &prev).await?;
            Some(set_password(Some(&lp), pw, reset.unwrap_or(lp.reset_required), keep(&policy), c.store.now()).await?)
        }
        None => None,
    };
    let name = u.name.clone();
    edit(c, &u.name, move |u| {
        let Some(have) = &mut u.login else { return Err(no_profile(&name)) };
        if let Some(n) = new {
            *have = n;
        } else if let Some(r) = reset {
            have.reset_required = r;
        }
        Ok(())
    })
    .await
}

async fn delete(c: &Call<'_>) -> Result<(), StsError> {
    let u = target(c)?;
    c.allow("DeleteLoginProfile", &c.arn("user", &u.path, &u.name))?;
    let name = u.name.clone();
    edit(c, &u.name, move |u| u.login.take().map(|_| ()).ok_or_else(|| no_profile(&name))).await
}

/// Wrong passwords a user may type within the window before sign-in (and `ChangePassword`)
/// refuse every password until the window passes. Durable, like the MFA throttle.
pub(crate) const MAX_PASSWORD_FAILS: u32 = 10;
pub(crate) const PASSWORD_WINDOW_MS: i64 = 300_000;

pub(crate) fn password_key(user: &str) -> String {
    format!("password:{}", user.to_lowercase())
}

/// A user changing its own password (`OldPassword`, `NewPassword`).
async fn change_password(c: &Call<'_>) -> Result<(), StsError> {
    let Kind::User { name, arn, .. } = &c.id.kind else {
        return Err(StsError::new("InvalidUserType", 400, "The request was rejected because the type of user for the transaction was incorrect."));
    };
    let policy = account_policy(c.store, &c.account)?;
    // `AllowUsersToChangePassword` lets every user change its own; otherwise it takes
    // iam:ChangePassword.
    if !policy.as_ref().is_some_and(|p| p.allow_users_to_change) {
        c.allow("ChangePassword", arn)?;
    }
    let old = c.p.required("OldPassword", "oldPassword")?;
    let new = c.p.required("NewPassword", "newPassword")?;
    let u = super::users::user(c, name)?;
    let lp = u.login.clone().ok_or_else(|| no_profile(&u.name))?;
    let key = password_key(&u.name);
    if c.store.throttled(&c.account, &key, MAX_PASSWORD_FAILS, PASSWORD_WINDOW_MS).map_err(store_error)? {
        return Err(StsError::new("EntityTemporarilyUnmodifiable", 409, "Too many wrong passwords: try again in a few minutes."));
    }
    if !password::verify(old, &lp.hash).await {
        c.store.record_failure(&c.account, &key, PASSWORD_WINDOW_MS).map_err(store_error)?;
        return Err(access_denied("The old password is incorrect."));
    }
    c.store.clear_failures(&c.account, &key).map_err(store_error)?;
    if policy.as_ref().is_some_and(|p| p.hard_expiry == Some(true)) && password::expired(policy.as_ref(), lp.changed_at, c.store.now()) {
        return Err(StsError::new("EntityTemporarilyUnmodifiable", 409, "The password has expired: an administrator must reset it."));
    }
    let mut prev = vec![lp.hash.clone()];
    prev.extend(lp.previous.iter().cloned());
    let policy = check_new(c.store, &c.id.account, new, &prev).await?;
    let next = set_password(Some(&lp), new, false, keep(&policy), c.store.now()).await?;
    let user = u.name.clone();
    edit(c, &u.name, move |u| {
        let Some(have) = &mut u.login else { return Err(no_profile(&user)) };
        *have = next;
        Ok(())
    })
    .await
}

fn policy_xml(p: &PasswordPolicy) -> String {
    let mut out = format!(
        "<PasswordPolicy><MinimumPasswordLength>{}</MinimumPasswordLength><RequireSymbols>{}</RequireSymbols><RequireNumbers>{}</RequireNumbers><RequireUppercaseCharacters>{}</RequireUppercaseCharacters><RequireLowercaseCharacters>{}</RequireLowercaseCharacters><AllowUsersToChangePassword>{}</AllowUsersToChangePassword><ExpirePasswords>{}</ExpirePasswords>",
        p.min_length,
        p.require_symbols,
        p.require_numbers,
        p.require_uppercase,
        p.require_lowercase,
        p.allow_users_to_change,
        p.max_age_days.is_some()
    );
    if let Some(d) = p.max_age_days {
        out.push_str(&format!("<MaxPasswordAge>{d}</MaxPasswordAge>"));
    }
    if let Some(n) = p.reuse_prevention {
        out.push_str(&format!("<PasswordReusePrevention>{n}</PasswordReusePrevention>"));
    }
    if let Some(h) = p.hard_expiry {
        out.push_str(&format!("<HardExpiry>{h}</HardExpiry>"));
    }
    out.push_str("</PasswordPolicy>");
    out
}

fn no_policy(c: &Call) -> StsError {
    no_such(format!("The Password Policy with domain name {} cannot be found.", c.account))
}

fn get_policy(c: &Call) -> Result<String, StsError> {
    c.allow("GetAccountPasswordPolicy", "*")?;
    account_policy(c.store, &c.account)?.map(|p| policy_xml(&p)).ok_or_else(|| no_policy(c))
}

fn number(c: &Call, name: &str, min: u32, max: u32) -> Result<Option<u32>, StsError> {
    let Some(v) = c.p.get(name) else { return Ok(None) };
    let field = lower_first(name);
    let n: u32 = v.trim().parse().map_err(|_| validation(format!("1 validation error detected: Value '{v}' at '{field}' failed to satisfy constraint: Member must be a number")))?;
    if n < min {
        return Err(validation(format!("1 validation error detected: Value '{n}' at '{field}' failed to satisfy constraint: Member must have value greater than or equal to {min}")));
    }
    if n > max {
        return Err(validation(format!("1 validation error detected: Value '{n}' at '{field}' failed to satisfy constraint: Member must have value less than or equal to {max}")));
    }
    Ok(Some(n))
}

async fn update_policy(c: &Call<'_>) -> Result<(), StsError> {
    c.allow("UpdateAccountPasswordPolicy", "*")?;
    // Every parameter left out gets its default, as on AWS.
    let d = PasswordPolicy::default();
    let p = PasswordPolicy {
        min_length: number(c, "MinimumPasswordLength", 6, 128)?.unwrap_or(d.min_length),
        require_symbols: bool_param(c, "RequireSymbols")?.unwrap_or(false),
        require_numbers: bool_param(c, "RequireNumbers")?.unwrap_or(false),
        require_uppercase: bool_param(c, "RequireUppercaseCharacters")?.unwrap_or(false),
        require_lowercase: bool_param(c, "RequireLowercaseCharacters")?.unwrap_or(false),
        allow_users_to_change: bool_param(c, "AllowUsersToChangePassword")?.unwrap_or(false),
        max_age_days: number(c, "MaxPasswordAge", 1, 1095)?,
        reuse_prevention: number(c, "PasswordReusePrevention", 1, 24)?,
        hard_expiry: bool_param(c, "HardExpiry")?,
    };
    let acct = c.account.clone();
    c.store.iam_write(move |t| t.put(&acct, kind::PASSWORD_POLICY, si::ACCOUNT, &p)).await.map_err(store_error)
}

async fn delete_policy(c: &Call<'_>) -> Result<(), StsError> {
    c.allow("DeleteAccountPasswordPolicy", "*")?;
    let acct = c.account.clone();
    if !c.store.iam_write(move |t| t.delete(&acct, kind::PASSWORD_POLICY, si::ACCOUNT)).await.map_err(store_error)? {
        return Err(no_policy(c));
    }
    Ok(())
}

/// IAM's alias rule: 3-63 lowercase letters, digits and hyphens, starting and ending with a
/// letter or digit, no two hyphens in a row. A 12-digit alias would read as an account id at
/// sign-in, so it's refused.
pub(crate) fn valid_alias(a: &str) -> bool {
    let b = a.as_bytes();
    (3..=63).contains(&b.len())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !a.contains("--")
        && !(b.len() == 12 && b.iter().all(u8::is_ascii_digit))
}

fn alias_param(c: &Call) -> Result<String, StsError> {
    let a = c.p.required("AccountAlias", "accountAlias")?;
    if !valid_alias(a) {
        return Err(validation(format!(
            "1 validation error detected: Value '{a}' at 'accountAlias' failed to satisfy constraint: Member must satisfy regular expression pattern: ^[a-z0-9]([a-z0-9]|-(?!-)){{1,61}}[a-z0-9]$"
        )));
    }
    Ok(a.to_string())
}

async fn create_alias(c: &Call<'_>) -> Result<(), StsError> {
    c.allow("CreateAccountAlias", "*")?;
    let alias = alias_param(c)?;
    let acct = c.account.clone();
    c.store
        .iam_write(move |t| {
            match t.alias_owner(&alias)? {
                Some(owner) if owner == acct => return Ok(()),
                Some(_) => return Err(StoreError::Conflict(format!("The account alias {alias} already exists."))),
                None => {}
            }
            // An account has one alias: a new one replaces it.
            if let Some(old) = t.alias_of(&acct)? {
                t.delete(&acct, kind::ALIAS, &old)?;
            }
            t.put(&acct, kind::ALIAS, &alias, &alias)
        })
        .await
        .map_err(store_error)
}

fn list_aliases(c: &Call) -> Result<String, StsError> {
    c.allow("ListAccountAliases", "*")?;
    let aliases: Vec<String> = c.store.iam_read(|t| t.list(&c.account, kind::ALIAS)).map_err(store_error)?;
    page(c.p, aliases, |a| a.clone(), |a| escape(a), "AccountAliases")
}

async fn delete_alias(c: &Call<'_>) -> Result<(), StsError> {
    c.allow("DeleteAccountAlias", "*")?;
    let alias = alias_param(c)?;
    let acct = c.account.clone();
    let gone = alias.clone();
    if !c.store.iam_write(move |t| if t.alias_of(&acct)?.as_deref() == Some(alias.as_str()) { t.delete(&acct, kind::ALIAS, &alias) } else { Ok(false) }).await.map_err(store_error)? {
        return Err(no_such(format!("The account alias {gone} cannot be found.")));
    }
    Ok(())
}

// ---- Signing in with a password ----

/// Why a sign-in failed.
#[derive(Debug)]
pub(crate) enum SignInError {
    /// A wrong account, user or password, or a user without a password: one answer for all.
    InvalidCredentials,
    MfaRequired,
    InvalidMfaCode,
    PasswordResetRequired,
    PasswordExpired,
    Policy(String),
    Throttled,
    Failed(StsError),
}

/// Who signed in.
pub(crate) struct SignedIn {
    pub account: s3_store::Account,
    pub user: User,
    pub mfa_at: Option<i64>,
}

/// A hash to verify against when there's no user, so a sign-in takes as long either way (no
/// telling which user names exist).
async fn dummy_verify(pw: &str) {
    static DUMMY: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let h = DUMMY.get_or_init(|| async { password::hash("roll: no such user").await.unwrap_or_default() }).await;
    let _ = password::verify(pw, h).await;
}

/// Check an IAM user's password (and MFA code, if the user has a device), as the AWS console's
/// sign-in does. `account` is the 12-digit id or the account's alias. With `new_password`, a
/// password that must be reset (or has expired) is changed first.
pub(crate) async fn sign_in(store: &Store, account: &str, user: &str, pw: &str, mfa_code: Option<&str>, new_password: Option<&str>) -> Result<SignedIn, SignInError> {
    let fail = |e: StoreError| SignInError::Failed(store_error(e));
    let account = account.trim();
    let id = if account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit()) { Some(account.to_string()) } else { store.account_by_alias(&account.to_ascii_lowercase()).map_err(fail)? };
    let acct = match id {
        Some(id) => store.account(&id).map_err(fail)?,
        None => None,
    };
    let found = match &acct {
        Some(a) if valid_name(user, 64) => store.iam_user(&a.id, user).map_err(fail)?,
        _ => None,
    };
    let (Some(acct), Some(u)) = (acct, found) else {
        dummy_verify(pw).await;
        return Err(SignInError::InvalidCredentials);
    };
    let Some(lp) = u.login.clone() else {
        dummy_verify(pw).await;
        return Err(SignInError::InvalidCredentials);
    };
    let key = password_key(&u.name);
    if store.throttled(&acct.id, &key, MAX_PASSWORD_FAILS, PASSWORD_WINDOW_MS).map_err(fail)? {
        return Err(SignInError::Throttled);
    }
    if !password::verify(pw, &lp.hash).await {
        store.record_failure(&acct.id, &key, PASSWORD_WINDOW_MS).map_err(fail)?;
        return Err(SignInError::InvalidCredentials);
    }
    store.clear_failures(&acct.id, &key).map_err(fail)?;
    // MFA: any of the user's enabled devices.
    let devices = store.iam_read(|t| t.mfa_devices_of(&acct.id, Some(&u.name))).map_err(fail)?;
    let mut mfa_at = None;
    if !devices.is_empty() {
        let Some(code) = mfa_code.map(str::trim).filter(|c| !c.is_empty()) else { return Err(SignInError::MfaRequired) };
        let mut ok = false;
        for d in &devices {
            if crate::mfa::check(store, &acct.id, &d.serial, code, Some(&u.name)).await.map_err(fail)? {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err(SignInError::InvalidMfaCode);
        }
        mfa_at = Some(store.now());
    }
    // A password that must change: the user sets a new one now.
    let policy = account_policy(store, &acct.id).map_err(SignInError::Failed)?;
    let expired = password::expired(policy.as_ref(), lp.changed_at, store.now());
    if expired && policy.as_ref().is_some_and(|p| p.hard_expiry == Some(true)) {
        return Err(SignInError::PasswordExpired);
    }
    let mut lp = lp;
    if lp.reset_required || expired {
        let Some(new) = new_password else { return Err(SignInError::PasswordResetRequired) };
        if new == pw {
            return Err(SignInError::Policy("The new password must be different from the current one".into()));
        }
        let mut prev = vec![lp.hash.clone()];
        prev.extend(lp.previous.iter().cloned());
        let policy = check_new(store, &acct, new, &prev).await.map_err(|e| if e.code == "PasswordPolicyViolation" { SignInError::Policy(e.message) } else { SignInError::Failed(e) })?;
        lp = set_password(Some(&lp), new, false, keep(&policy), store.now()).await.map_err(SignInError::Failed)?;
        let (a, n, saved) = (acct.id.clone(), u.name.clone(), lp.clone());
        store
            .iam_write(move |t| {
                let Some(mut u) = t.get::<User>(&a, kind::USER, &n)? else { return Err(StoreError::NotFound("gone".into())) };
                u.login = Some(saved);
                t.put(&a, kind::USER, &n, &u)
            })
            .await
            .map_err(|_| SignInError::InvalidCredentials)?;
    }
    // When the password was last used (the credential report): a relaxed write.
    let (a, n, now) = (acct.id.clone(), u.name.clone(), store.now());
    let _ = store.iam_write_relaxed(move |t| {
        if let Some(mut u) = t.get::<User>(&a, kind::USER, &n)?
            && let Some(l) = &mut u.login
        {
            l.last_used_at = Some(now);
            t.put(&a, kind::USER, &n, &u)?;
        }
        Ok(())
    });
    Ok(SignedIn { account: acct, user: u, mfa_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases() {
        for ok in ["abc", "my-company", "a1-b2-c3", "123456789", "1234567890123"] {
            assert!(valid_alias(ok), "{ok}");
        }
        for bad in ["ab", "-abc", "abc-", "a--b", "ABC", "a_b", "123456789012", &"a".repeat(64)] {
            assert!(!valid_alias(bad), "{bad}");
        }
    }
}
