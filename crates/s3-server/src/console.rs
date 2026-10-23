//! The dashboard's sign-in for IAM users, and what such a session may do through the control
//! plane (`s3-control`'s `console.rs`). This is roll's whole dashboard auth model, scoped to
//! storage:
//!
//! - **Signing in** takes an account (its 12-digit id or its alias), an IAM user name and the
//!   user's console password (a login profile), plus a code of the user's MFA device if it has
//!   one, as the AWS console's IAM-user sign-in does (`iam/login.rs`). A password that must be
//!   reset (or has expired) is changed as part of signing in.
//! - **The session** is an STS session of the user (sealed claims, never stored; `console` set,
//!   no access key behind it), for 12 hours. The dashboard holds it as a bearer token,
//!   `<temporary key id>.<session token>`. It isn't usable with SigV4: its secret is never
//!   handed out. It ends when the user is deleted, its login profile is deleted, or its sessions
//!   expire; `aws:MultiFactorAuthPresent` is `true` when MFA was used.
//! - **What it may do** is the user's IAM permissions: every control-plane operation it calls is
//!   authorized as the S3 request it stands for, by the S3 API's own evaluator
//!   ([`Signed::authorize`], `access.rs`), and IAM calls go through the IAM API's own dispatch as
//!   the user ([`iam`]).
//!
//! Without a session, the control plane is the operator's, as before (it acts as an account's
//! root user): the control-plane listener must stay private to the operator.

use crate::auth::Auth;
use crate::ctx::Ctx;
use crate::identity::{Identity, Kind};
use crate::sts::{Params, StsError};
use crate::{Server, auth::Need};
use axum::http::{HeaderMap, HeaderValue, Method};
use s3_store::sts::{Session, SessionError};
use s3_store::{AccessKey, Permission};
use serde::Serialize;
use std::net::IpAddr;
use std::sync::Arc;

/// How long a dashboard session lasts (the AWS console's default for IAM users).
pub const SESSION_S: i64 = 12 * 3600;
/// Largest IAM call from the dashboard (as the IAM query API's).
pub const MAX_IAM_BODY: usize = crate::sts::MAX_BODY;

/// A failed console call: an HTTP status, an error code and a message for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl ConsoleError {
    fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into() }
    }

    fn from_sts(e: StsError) -> Self {
        Self::new(e.status, e.code, e.message)
    }
}

/// A signed-in user, as the dashboard shows it.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// The bearer token (only when signing in).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Unix ms.
    pub expires_at: i64,
    pub account_id: String,
    pub account_alias: Option<String>,
    pub user: String,
    pub arn: String,
    /// MFA was used to sign in.
    pub mfa: bool,
}

/// A request in the shape the S3 API's evaluator takes, for checks outside an S3 request.
fn request(server: &Arc<Server>, peer: Option<IpAddr>, user_agent: Option<&str>, auth: Option<Auth>) -> Ctx {
    let mut headers = HeaderMap::new();
    if let Some(ua) = user_agent.and_then(|u| HeaderValue::from_str(u).ok()) {
        headers.insert("user-agent", ua);
    }
    Ctx {
        server: server.clone(),
        method: Method::POST,
        raw_path: "/".into(),
        raw_query: String::new(),
        query: Vec::new(),
        headers,
        bucket: None,
        key: None,
        vhost: false,
        peer,
        auth,
        website: false,
        access_cache: Default::default(),
        started: std::time::Instant::now(),
        access_point: None,
        payers: Default::default(),
        cdn: None,
    }
}

fn info(server: &Server, key: &AccessKey, id: &Identity, token: Option<String>) -> SessionInfo {
    let s = key.session.as_ref();
    SessionInfo {
        token,
        expires_at: s.map_or(0, |s| s.expires_at),
        account_id: id.account.id.clone(),
        account_alias: server.store.account_alias(&id.account.id).ok().flatten(),
        user: id.username.clone().unwrap_or_default(),
        arn: id.caller_arn.clone(),
        mfa: s.is_some_and(|s| s.mfa_at.is_some()),
    }
}

/// Sign an IAM user in with its password (and MFA code; with `new_password`, a password that
/// must change is changed first).
pub async fn sign_in(server: &Arc<Server>, account: &str, user: &str, password: &str, mfa_code: Option<&str>, new_password: Option<&str>) -> Result<SessionInfo, ConsoleError> {
    use crate::iam::login::SignInError as E;
    let store = &server.store;
    let signed = crate::iam::login::sign_in(store, account, user, password, mfa_code, new_password).await.map_err(|e| match e {
        E::InvalidCredentials => ConsoleError::new(401, "InvalidCredentials", "Your authentication information is incorrect. Please try again."),
        E::MfaRequired => ConsoleError::new(401, "MfaRequired", "Enter the code of your MFA device."),
        E::InvalidMfaCode => ConsoleError::new(401, "InvalidMfaCode", "The MFA code isn't valid. Wait for the next code and try again."),
        E::PasswordResetRequired => ConsoleError::new(401, "PasswordResetRequired", "Your password must be changed: choose a new one."),
        E::PasswordExpired => ConsoleError::new(401, "PasswordExpired", "Your password has expired. Ask an administrator to reset it."),
        E::Policy(m) => ConsoleError::new(400, "PasswordPolicyViolation", m),
        E::Throttled => ConsoleError::new(429, "Throttled", "Too many failed sign-in attempts. Try again in a few minutes."),
        E::Failed(e) => ConsoleError::from_sts(e),
    })?;
    let mut s = Session::new(Permission::Read);
    s.account = signed.account.id.clone();
    s.user = Some(signed.user.name.clone());
    s.console = true;
    s.buckets = Some(Vec::new());
    s.mfa_at = signed.mfa_at;
    let (_, creds) = store.mint_session(s, SESSION_S).await.map_err(|e| ConsoleError::from_sts(crate::sts::store_failure(e)))?;
    let token = format!("{}.{}", creds.access_key_id, creds.session_token);
    let key = store.session_key(&creds.access_key_id, &creds.session_token).map_err(|_| ConsoleError::new(500, "InternalFailure", "The new session doesn't open."))?;
    let id = crate::identity::resolve(store, &key).map_err(|e| ConsoleError::from_sts(crate::sts::store_failure(e)))?;
    Ok(info(server, &key, &id, Some(token)))
}

/// Where a dashboard request comes from (`aws:SourceIp`, `aws:UserAgent`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Origin<'a> {
    pub peer: Option<IpAddr>,
    pub user_agent: Option<&'a str>,
}

/// A verified dashboard session.
pub struct Signed {
    key: AccessKey,
    identity: Arc<Identity>,
}

impl std::fmt::Debug for Signed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signed").field("account", &self.identity.account.id).field("user", &self.identity.username).finish_non_exhaustive()
    }
}

/// Check a bearer token: a live dashboard session of an IAM user.
pub fn session(server: &Arc<Server>, token: &str) -> Result<Signed, ConsoleError> {
    let invalid = || ConsoleError::new(401, "InvalidToken", "The session isn't valid: sign in again.");
    let (id, tok) = token.trim().split_once('.').ok_or_else(invalid)?;
    let key = server.store.session_key(id, tok).map_err(|e| match e {
        SessionError::Expired => ConsoleError::new(401, "ExpiredToken", "The session has expired: sign in again."),
        SessionError::Store(e) => ConsoleError::from_sts(crate::sts::store_failure(e)),
        _ => invalid(),
    })?;
    // Only dashboard sessions: other STS sessions need their secret (SigV4).
    if !key.session.as_ref().is_some_and(|s| s.console) {
        return Err(invalid());
    }
    let identity = crate::identity::resolve(&server.store, &key).map_err(|e| ConsoleError::from_sts(crate::sts::store_failure(e)))?;
    if identity.dead || !matches!(identity.kind, Kind::User { .. }) {
        return Err(ConsoleError::new(401, "InvalidToken", "The session has ended: sign in again."));
    }
    Ok(Signed { key, identity: Arc::new(identity) })
}

impl Signed {
    pub fn info(&self, server: &Server) -> SessionInfo {
        info(server, &self.key, &self.identity, None)
    }

    /// The account the session acts in.
    pub fn account(&self) -> &str {
        &self.identity.account.id
    }

    fn ctx(&self, server: &Arc<Server>, peer: Option<IpAddr>, user_agent: Option<&str>) -> Ctx {
        let verified = s3_proto::sigv4::Verified { access_key: self.key.id.clone(), payload: s3_proto::sigv4::Payload::Unsigned, stream: None };
        request(server, peer, user_agent, Some(Auth { key: self.key.clone(), verified, identity: self.identity.clone() }))
    }

    /// May the user do `action` (an S3 action) on `bucket` (and `key`)? With a bucket, the
    /// bucket's rules decide too (its policy, ACLs, cross-account), exactly as for an S3
    /// request; without one (`ListAllMyBuckets`, `CreateBucket` of `name`), the user's own
    /// policies.
    pub fn authorize(&self, server: &Arc<Server>, from: Origin<'_>, action: &str, bucket: Option<&str>, key: Option<&str>, identity_only: bool) -> Result<(), ConsoleError> {
        let ctx = self.ctx(server, from.peer, from.user_agent);
        let denied = |_| ConsoleError::new(403, "AccessDenied", format!("User: {} is not authorized to perform: {action}{}", self.identity.caller_arn, bucket.map(|b| format!(" on resource: {}", crate::access::resource_arn(b, key))).unwrap_or_default()));
        let r = match bucket {
            Some(b) if !identity_only => crate::access::authorize(&ctx, ctx.who(), b, key, action, Need::Admin),
            b => ctx.require_identity(action, b, false),
        };
        r.map_err(|e| if e.code == s3_proto::Code::NoSuchBucket { ConsoleError::new(404, "NoSuchBucket", "The specified bucket does not exist") } else { denied(e) })
    }
}

/// Who an IAM call from the dashboard acts as.
pub enum Caller<'a> {
    /// The operator: the account's root user.
    Root(&'a str),
    Session(&'a Signed),
}

/// Run an IAM query action (`params` as the query API takes them) for the dashboard: the same
/// dispatch and authorization as the IAM API. `Ok`: the action's result as JSON.
pub async fn iam(server: &Arc<Server>, caller: Caller<'_>, peer: Option<IpAddr>, user_agent: Option<&str>, params: Vec<(String, String)>) -> Result<serde_json::Value, ConsoleError> {
    let p = Params(params);
    let action = p.get("Action").ok_or_else(|| ConsoleError::new(400, "MissingAction", "Missing Action"))?.to_string();
    let store = server.store.clone();
    let (ctx, identity) = match caller {
        Caller::Session(s) => (s.ctx(server, peer, user_agent), s.identity.clone()),
        Caller::Root(account) => {
            store.require_account(account).map_err(|e| ConsoleError::new(404, "NoSuchEntity", e.to_string()))?;
            // The account's root user (an admin key over every bucket, never stored).
            let root = AccessKey {
                id: "RKROLLCONSOLEROOT000".into(),
                secret: String::new(),
                name: "root".into(),
                permission: Permission::Admin,
                buckets: None,
                created_at: 0,
                last_used_at: None,
                account: account.to_string(),
                user: None,
                active: true,
                session: None,
            };
            let id = crate::identity::resolve(&store, &root).map_err(|e| ConsoleError::from_sts(crate::sts::store_failure(e)))?;
            (request(server, peer, user_agent, None), Arc::new(id))
        }
    };
    let out = crate::iam::execute(&ctx, &store, &identity, &p, &action).await.map_err(ConsoleError::from_sts)?;
    Ok(out.map(|x| xml_json(&x)).unwrap_or_else(|| serde_json::json!({})))
}

/// An IAM `Result` element's content as JSON: elements whose children are all `member` are
/// arrays, `entry` maps (`key`/`value`) are objects, leaves are strings, empty elements
/// `null`; policy documents (URL-encoded in IAM's XML) come back decoded.
pub fn xml_json(inner: &str) -> serde_json::Value {
    use s3_proto::xmldsig::Doc;
    use serde_json::{Map, Value};
    fn conv(d: &Doc, n: usize) -> Value {
        let kids: Vec<usize> = d.elements(n).collect();
        if kids.is_empty() {
            let t = d.text_of(n);
            return if t.is_empty() { Value::Null } else { Value::String(t) };
        }
        if kids.iter().all(|k| d.el(*k).local == "member") {
            return Value::Array(kids.iter().map(|k| conv(d, *k)).collect());
        }
        if kids.iter().all(|k| d.el(*k).local == "entry") {
            let mut m = Map::new();
            let child = |k: usize, name: &str| d.elements(k).find(|x| d.el(*x).local == name);
            for k in kids {
                let key = child(k, "key").map(|x| d.text_of(x)).unwrap_or_default();
                let v = child(k, "value").map(|x| conv(d, x)).unwrap_or(Value::Null);
                m.insert(key, v);
            }
            return Value::Object(m);
        }
        let mut m = Map::new();
        for k in kids {
            let name = d.el(k).local.clone();
            let mut v = conv(d, k);
            if matches!(name.as_str(), "PolicyDocument" | "AssumeRolePolicyDocument" | "Document")
                && let Value::String(s) = &v
                && let Ok(dec) = s3_proto::http::percent_decode_utf8(s)
            {
                v = Value::String(dec);
            }
            m.insert(name, v);
        }
        Value::Object(m)
    }
    match Doc::parse(&format!("<r>{inner}</r>")) {
        Ok(d) => conv(&d, d.root()),
        Err(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iam_xml_as_json() {
        let v = xml_json("<Users><member><UserName>a</UserName><Tags><member><Key>k</Key><Value>v</Value></member></Tags></member></Users><IsTruncated>false</IsTruncated><Groups/>");
        assert_eq!(v, serde_json::json!({"Users": [{"UserName": "a", "Tags": [{"Key": "k", "Value": "v"}]}], "IsTruncated": "false", "Groups": null}));
        let v = xml_json("<SummaryMap><entry><key>Users</key><value>2</value></entry></SummaryMap>");
        assert_eq!(v["SummaryMap"]["Users"], "2");
        let v = xml_json("<PolicyDocument>%7B%22a%22%3A1%7D</PolicyDocument><UserName>x &amp; y</UserName>");
        assert_eq!(v, serde_json::json!({"PolicyDocument": "{\"a\":1}", "UserName": "x & y"}));
    }
}
