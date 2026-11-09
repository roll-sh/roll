//! Principals in resource policies (bucket and access point policies) and role trust policies
//! are checked against the metadata, as AWS checks them:
//!
//! - an ARN naming a user, role or account that doesn't exist is `Invalid principal in policy`
//!   (S3's `MalformedPolicy`, IAM's `MalformedPolicyDocument`), and so is an IAM ARN that can't
//!   be a principal (a group, say), an OIDC or SAML provider that doesn't exist in a `Federated`
//!   principal, or a roll access key id that doesn't exist;
//! - the users and roles named are remembered with their unique ids ([`Bindings`], kept next to
//!   the policy). AWS stores a principal as the entity's unique id and shows its ARN; once the
//!   entity is deleted (or replaced by a new one of the same name) the policy shows the unique id
//!   instead, which matches nobody, and a user that was renamed shows with its new ARN.
//!   [`rewrite`] does exactly that for reading a policy back and for evaluating it, so a role
//!   created again with the name of a deleted one doesn't inherit its access.
//!
//! `CanonicalUser` and `Service` principals aren't checked (S3's canonical ids of roll keys are
//! one-way), nor are session ARNs (`assumed-role`, `federated-user`) beyond their account.

use s3_store::Store;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;

/// The users and roles a policy's principals named when it was put: ARN → unique id.
pub(crate) type Bindings = BTreeMap<String, String>;

pub(crate) const INVALID: &str = "Invalid principal in policy";

fn statements(v: &Value) -> Vec<&Value> {
    match v.get("Statement") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(o @ Value::Object(_)) => vec![o],
        _ => Vec::new(),
    }
}

fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// `(account, rest)` of `arn:aws:<service>::<account>:<rest>`.
fn arn_parts<'a>(arn: &'a str, service: &str) -> Option<(&'a str, &'a str)> {
    let rest = arn.strip_prefix("arn:aws:")?.strip_prefix(service)?.strip_prefix("::")?;
    let (account, rest) = rest.split_once(':')?;
    (account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit())).then_some((account, rest))
}

/// `/a/b/name` → (`/a/b/`, `name`).
fn path_name(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with('/') {
        return None;
    }
    let i = s.rfind('/')?;
    let (path, name) = (&s[..=i], &s[i + 1..]);
    (!name.is_empty()).then_some((path, name))
}

fn norm_path(p: &str) -> &str {
    if p.is_empty() { "/" } else { p }
}

/// Whether the account exists.
fn account_exists(store: &Store, id: &str) -> Result<bool, String> {
    store.account(id).map(|a| a.is_some()).map_err(|e| e.to_string())
}

/// What an `AWS` principal names, checked: `Ok(Some((arn, unique id)))` for a user or role,
/// `Ok(None)` for something valid that isn't bound (an account, a key, a session), `Err` if it
/// names nothing.
fn check_aws(store: &Store, p: &str) -> Result<Option<(String, String)>, Option<String>> {
    let invalid = || Err(None);
    if p == "*" {
        return Ok(None);
    }
    if p.len() == 12 && p.bytes().all(|b| b.is_ascii_digit()) {
        return if account_exists(store, p).map_err(Some)? { Ok(None) } else { invalid() };
    }
    if let Some((account, rest)) = arn_parts(p, "iam") {
        if !account_exists(store, account).map_err(Some)? {
            return invalid();
        }
        if rest == "root" {
            return Ok(None);
        }
        if let Some((path, name)) = rest.strip_prefix("user").and_then(path_name) {
            if let Some(u) = store.iam_user(account, name).map_err(|e| Some(e.to_string()))?
                && norm_path(&u.path) == path
                && u.name == name
            {
                return Ok(Some((p.to_string(), u.id)));
            }
            // A roll access key's user ARN (`arn:aws:iam::<account>:user/<key id>`).
            if path == "/"
                && let Some(k) = store.access_key(name).map_err(|e| Some(e.to_string()))?
                && k.account == account
                && k.user.is_none()
                && !k.is_temporary()
            {
                return Ok(None);
            }
            return invalid();
        }
        if let Some((path, name)) = rest.strip_prefix("role").and_then(path_name) {
            return match store.role_in(account, name).map_err(|e| Some(e.to_string()))? {
                Some(r) if norm_path(&r.iam.path) == path && r.name == name => Ok(Some((p.to_string(), r.id))),
                _ => invalid(),
            };
        }
        return invalid();
    }
    if let Some((account, rest)) = arn_parts(p, "sts") {
        if (rest.starts_with("assumed-role/") || rest.starts_with("federated-user/")) && account_exists(store, account).map_err(Some)? {
            return Ok(None);
        }
        return invalid();
    }
    if p.starts_with("arn:") {
        return invalid();
    }
    // A unique id (`AIDA…`, `AROA…`): what a deleted principal reads as; it matches nobody.
    if p.len() == 21 && ["AIDA", "AROA"].iter().any(|x| p.starts_with(x)) && p.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
        return Ok(None);
    }
    // A roll access key id.
    match store.access_key(p).map_err(|e| Some(e.to_string()))? {
        Some(k) if !k.is_temporary() => Ok(None),
        _ => invalid(),
    }
}

fn check_federated(store: &Store, p: &str) -> Result<(), Option<String>> {
    let Some((account, rest)) = arn_parts(p, "iam") else {
        // Well-known web identity providers (`accounts.google.com`, `cognito-identity.amazonaws.com`).
        return if p.starts_with("arn:") { Err(None) } else { Ok(()) };
    };
    let found = if let Some(name) = rest.strip_prefix("oidc-provider/") {
        store.iam_oidc_provider(account, name).map_err(|e| Some(e.to_string()))?.is_some()
    } else if let Some(name) = rest.strip_prefix("saml-provider/") {
        store.iam_read(|t| t.exists(account, s3_store::iam::kind::SAML, name)).map_err(|e| Some(e.to_string()))?
    } else {
        false
    };
    if found { Ok(()) } else { Err(None) }
}

/// Check a policy's principals (`doc` already parsed as a policy) and bind the users and roles
/// they name. `Err(Some(principal))` names the first invalid one (`"AWS":"arn:…"`), `Err(None)`
/// is a storage failure (logged).
pub(crate) fn bind(store: &Store, doc: &str) -> Result<Bindings, Option<String>> {
    // The document was parsed as a policy already.
    let Ok(v) = serde_json::from_str::<Value>(doc) else { return Ok(Bindings::new()) };
    let mut out = Bindings::new();
    for s in statements(&v) {
        for field in ["Principal", "NotPrincipal"] {
            let Some(Value::Object(m)) = s.get(field) else { continue };
            for (kind, vals) in m {
                for p in strings(vals) {
                    let r = match kind.as_str() {
                        "AWS" => check_aws(store, p).map(|b| {
                            if let Some((arn, id)) = b {
                                out.insert(arn, id);
                            }
                        }),
                        "Federated" => check_federated(store, p),
                        _ => Ok(()),
                    };
                    match r {
                        Ok(()) => {}
                        Err(None) => return Err(Some(format!("\"{kind}\":\"{p}\""))),
                        Err(Some(e)) => {
                            tracing::error!(error = e, "checking policy principals failed");
                            return Err(None);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The current ARN of the user or role with unique id `id` in `account` (`None`: it's gone).
fn arn_of_id(store: &Store, account: &str, id: &str) -> Option<String> {
    if id.starts_with("AIDA") {
        let users: Vec<s3_store::iam::User> = store.iam_read(|t| t.list(account, s3_store::iam::kind::USER)).ok()?;
        return users.into_iter().find(|u| u.id == id).map(|u| crate::identity::user_arn(account, &u.path, &u.name));
    }
    store.roles_in(account).ok()?.into_iter().find(|r| r.id == id).map(|r| r.own_arn())
}

/// What a bound ARN reads as now: itself while it names the entity it named, the entity's new
/// ARN if it was renamed, else its unique id.
fn current(store: &Store, arn: &str, id: &str) -> Option<String> {
    let (account, rest) = arn_parts(arn, "iam")?;
    let now_id = if let Some((_, name)) = rest.strip_prefix("user").and_then(path_name) {
        store.iam_user(account, name).ok().flatten().filter(|u| crate::identity::user_arn(account, &u.path, &u.name) == arn).map(|u| u.id)
    } else if let Some((_, name)) = rest.strip_prefix("role").and_then(path_name) {
        store.role_in(account, name).ok().flatten().filter(|r| r.own_arn() == arn).map(|r| r.id)
    } else {
        None
    };
    if now_id.as_deref() == Some(id) {
        return None;
    }
    Some(arn_of_id(store, account, id).unwrap_or_else(|| id.to_string()))
}

/// `doc` as it reads now: principals whose user or role was deleted (or replaced, or renamed)
/// since it was put show as AWS shows them. Unchanged (borrowed) when nothing changed.
pub(crate) fn rewrite<'a>(store: &Store, doc: &'a str, bindings: &Bindings) -> Cow<'a, str> {
    if bindings.is_empty() {
        return Cow::Borrowed(doc);
    }
    let changed: BTreeMap<&str, String> = bindings.iter().filter_map(|(arn, id)| current(store, arn, id).map(|now| (arn.as_str(), now))).collect();
    if changed.is_empty() {
        return Cow::Borrowed(doc);
    }
    let Ok(mut v) = serde_json::from_str::<Value>(doc) else { return Cow::Borrowed(doc) };
    let fix = |vals: &mut Value| match vals {
        Value::String(s) => {
            if let Some(n) = changed.get(s.as_str()) {
                *s = n.clone();
            }
        }
        Value::Array(a) => {
            for x in a {
                if let Value::String(s) = x
                    && let Some(n) = changed.get(s.as_str())
                {
                    *s = n.clone();
                }
            }
        }
        _ => {}
    };
    let mut each = |s: &mut Value| {
        for field in ["Principal", "NotPrincipal"] {
            if let Some(Value::Object(m)) = s.get_mut(field)
                && let Some(aws) = m.get_mut("AWS")
            {
                fix(aws);
            }
        }
    };
    match v.get_mut("Statement") {
        Some(Value::Array(a)) => a.iter_mut().for_each(&mut each),
        Some(o @ Value::Object(_)) => each(o),
        _ => {}
    }
    Cow::Owned(to_json(&v))
}

/// Bindings as stored (JSON).
pub(crate) fn to_stored(b: &Bindings) -> String {
    serde_json::to_string(b).unwrap_or_else(|_| "{}".into())
}

pub(crate) fn from_stored(s: Option<&String>) -> Bindings {
    s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
}

/// A policy document as AWS writes one: minified, with policy keys in their usual order
/// (`Version`, `Id`, `Statement`; `Sid`, `Effect`, `Principal`, … `Condition`).
pub(crate) fn to_json(v: &Value) -> String {
    const ORDER: [&str; 12] = ["Version", "Id", "Statement", "Sid", "Effect", "Principal", "NotPrincipal", "Action", "NotAction", "Resource", "NotResource", "Condition"];
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by_key(|k| (ORDER.iter().position(|o| o == k).unwrap_or(ORDER.len()), k.to_string()));
            let inner: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), to_json(&m[k.as_str()]))).collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(to_json).collect::<Vec<_>>().join(",")),
        x => x.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arns_and_paths() {
        assert_eq!(arn_parts("arn:aws:iam::123456789012:user/a", "iam"), Some(("123456789012", "user/a")));
        assert_eq!(arn_parts("arn:aws:iam:::user/a", "iam"), None, "an account is required");
        assert_eq!(arn_parts("arn:aws:sts::123456789012:assumed-role/r/s", "sts"), Some(("123456789012", "assumed-role/r/s")));
        assert_eq!(path_name("/a/b/name"), Some(("/a/b/", "name")));
        assert_eq!(path_name("/name"), Some(("/", "name")));
        assert_eq!(path_name("/a/"), None);
        assert_eq!(path_name("name"), None);
    }

    #[test]
    fn documents_keep_aws_order() {
        let v = serde_json::json!({"Statement": [{"Resource": "*", "Action": "s3:*", "Principal": {"AWS": ["b", "a"]}, "Effect": "Allow", "Sid": "x"}], "Version": "2012-10-17"});
        assert_eq!(to_json(&v), r#"{"Version":"2012-10-17","Statement":[{"Sid":"x","Effect":"Allow","Principal":{"AWS":["b","a"]},"Action":"s3:*","Resource":"*"}]}"#);
    }
}
