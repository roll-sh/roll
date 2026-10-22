//! Access points: requests addressed to an access point instead of a bucket, and how they're
//! authorized. The S3 Control API that manages them is `control.rs`; Object Lambda access
//! points are `lambda.rs`.
//!
//! **Addressing.** A request names an access point by
//! - its host: `<name>-<account>.<anything>` (what SDKs make of an access point ARN with a
//!   custom endpoint), including AWS's `<name>-<account>.s3-accesspoint.<region>.<domain>` and
//!   `<name>-<account>.s3-object-lambda.<region>.<domain>`;
//! - the ARN as the path's first segment: `/arn:aws:s3:<region>:<account>:accesspoint/<name>/<key>`
//!   (percent-encoded or not);
//! - its alias as a bucket name: `<name>-<random>-s3alias` (`--ol-s3` for Object Lambda).
//!
//! The request then works on the access point's bucket, and [`authorize`] decides with both
//! the access point's policy and the bucket's.
//!
//! **Authorization**, as AWS evaluates a request through an access point:
//! 1. a `Deny` in the access point policy or the bucket policy refuses it;
//! 2. the key's own permission on the bucket allows it (an identity allow, same account);
//! 3. otherwise both policies must allow it: the access point policy (resources
//!    `arn:aws:s3:<region>:<account>:accesspoint/<name>[/object/<key>]`) and the bucket policy,
//!    which can delegate with `s3:DataAccessPointAccount` / `s3:DataAccessPointArn`
//!    conditions. A public grant doesn't apply while either public access block restricts
//!    public buckets.
//!
//! ACL grants and the bucket's `public` flag don't apply through access points.
//!
//! **Network origin.** `VPC` access points only take requests from inside roll's network
//! (`internal_cidrs`): roll can't see VPC ids, so the VPC is the deployment's network.

pub(crate) mod control;
pub(crate) mod lambda;

use crate::access::{Access, Cond, Who};
use crate::auth::Need;
use crate::ctx::Ctx;
use crate::error::StoreResult;
use crate::policy::{Account, Decision, Policy};
use axum::http::Method;
use s3_proto::http::percent_decode_utf8;
use s3_proto::{Code, S3Error};
use s3_store::{AccessPoint, AccessPointKind};
use std::sync::{Arc, OnceLock};

/// The access point a request goes through.
pub(crate) struct ApRequest {
    /// The standard access point (for Object Lambda, the supporting one).
    pub ap: AccessPoint,
    pub arn: String,
    pub policy: Option<Policy>,
    /// Addressed to an Object Lambda access point: it, its ARN and policy.
    pub olap: Option<(AccessPoint, String, Option<Policy>)>,
}

pub(crate) fn ap_arn(region: &str, account: &str, name: &str) -> String {
    format!("arn:aws:s3:{region}:{account}:accesspoint/{name}")
}

pub(crate) fn olap_arn(region: &str, account: &str, name: &str) -> String {
    format!("arn:aws:s3-object-lambda:{region}:{account}:accesspoint/{name}")
}

/// Parse a stored access point policy (validated when it was put). Object Lambda policies
/// name `s3-object-lambda:` actions, evaluated as their `s3:` counterparts.
pub(crate) fn load_policy(doc: Option<&str>) -> Result<Option<Policy>, S3Error> {
    let Some(doc) = doc else { return Ok(None) };
    Policy::load(doc.replace("\"s3-object-lambda:", "\"s3:").as_bytes()).map(Some).map_err(|e| {
        tracing::error!(error = %e, "a stored access point policy doesn't parse");
        S3Error::new(Code::AccessDenied)
    })
}

/// How a request names an access point.
enum Named {
    /// `<name>-<account>.…`; `Some(kind)` when the host says which (`s3-accesspoint`,
    /// `s3-object-lambda`).
    Host { name: String, kind: Option<AccessPointKind> },
    Arn { kind: AccessPointKind, account: String, name: String, key: Option<String> },
    Alias(String),
}

/// `<name>-<12 digits>` of the host's first label.
fn host_label(ctx: &Ctx) -> Option<(String, String, Option<AccessPointKind>)> {
    let host = ctx.h("host")?.to_ascii_lowercase();
    let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|c| c.is_ascii_digit())).map_or(host.as_str(), |(h, _)| h).to_string();
    let mut labels = host.split('.');
    let first = labels.next()?;
    let kind = match labels.next() {
        Some("s3-accesspoint") => Some(AccessPointKind::Standard),
        Some("s3-object-lambda") => Some(AccessPointKind::ObjectLambda),
        _ => None,
    };
    let (name, account) = first.rsplit_once('-')?;
    (account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit()) && !name.is_empty()).then(|| (name.to_string(), account.to_string(), kind))
}

/// `arn:aws:s3[-object-lambda]:<region>:<account>:accesspoint/<name>`.
fn parse_arn(arn: &str) -> Option<(AccessPointKind, String, String)> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" || !parts[1].starts_with("aws") {
        return None;
    }
    let kind = match parts[2] {
        "s3" => AccessPointKind::Standard,
        "s3-object-lambda" => AccessPointKind::ObjectLambda,
        _ => return None,
    };
    let name = parts[5].strip_prefix("accesspoint/").or_else(|| parts[5].strip_prefix("accesspoint:"))?;
    (!name.is_empty() && !name.contains('/')).then(|| (kind, parts[4].to_string(), name.to_string()))
}

fn named(ctx: &Ctx, account: &str) -> Result<Option<Named>, S3Error> {
    if let Some((name, acct, kind)) = host_label(ctx)
        && acct == account
    {
        return Ok(Some(Named::Host { name, kind }));
    }
    let path = ctx.raw_path.strip_prefix('/').unwrap_or(&ctx.raw_path);
    let (first, rest) = path.split_once('/').unwrap_or((path, ""));
    let first = percent_decode_utf8(first)?;
    if first.starts_with("arn:") {
        // `/arn:…:accesspoint/<name>/<key>`, the ARN's slash encoded or not.
        let (arn, rest) = if first.contains("accesspoint/") {
            (first.clone(), rest.to_string())
        } else {
            let (n, r) = rest.split_once('/').unwrap_or((rest, ""));
            (format!("{first}/{}", percent_decode_utf8(n)?), r.to_string())
        };
        let (kind, acct, name) = parse_arn(&arn).ok_or_else(|| S3Error::msg(Code::InvalidAccessPoint, "The access point ARN is not valid").with("AccessPointArn", arn.clone()))?;
        let key = Some(percent_decode_utf8(&rest)?).filter(|k| !k.is_empty());
        return Ok(Some(Named::Arn { kind, account: acct, name, key }));
    }
    if let Some(b) = &ctx.bucket
        && s3_store::access_points::is_alias(b)
    {
        return Ok(Some(Named::Alias(b.clone())));
    }
    Ok(None)
}

/// The signing service of the request (`s3-object-lambda` for Object Lambda).
fn signed_for_object_lambda(ctx: &Ctx) -> bool {
    let cred = ctx.h("authorization").and_then(|a| a.split("Credential=").nth(1)).or_else(|| ctx.q("X-Amz-Credential"));
    cred.is_some_and(|c| c.split([',', ' ']).next().unwrap_or_default().split('/').nth(3) == Some("s3-object-lambda"))
}

/// If the request is addressed to an access point, point it at its bucket (and key) and
/// remember the access point.
pub(crate) fn resolve(ctx: &mut Ctx) -> Result<(), S3Error> {
    // Cheap checks first: most requests name no access point.
    let maybe = host_label(ctx).is_some() || ctx.raw_path.starts_with("/arn") || ctx.bucket.as_deref().is_some_and(s3_store::access_points::is_alias);
    if !maybe {
        return Ok(());
    }
    let store = ctx.store().clone();
    let account = store.account_id();
    let Some(n) = named(ctx, &account)? else { return Ok(()) };
    let path_key = || -> Result<Option<String>, S3Error> {
        let p = ctx.raw_path.strip_prefix('/').unwrap_or(&ctx.raw_path);
        Ok(Some(percent_decode_utf8(p)?).filter(|k| !k.is_empty()))
    };
    let no_such = |name: &str| S3Error::msg(Code::NoSuchAccessPoint, format!("The specified accesspoint does not exist: {name}"));
    let (found, key) = match n {
        Named::Host { name, kind } => {
            let first = kind.unwrap_or(if signed_for_object_lambda(ctx) { AccessPointKind::ObjectLambda } else { AccessPointKind::Standard });
            let mut ap = store.access_point(&account, first, &name).s3()?;
            if ap.is_none() && kind.is_none() {
                let other = if first == AccessPointKind::Standard { AccessPointKind::ObjectLambda } else { AccessPointKind::Standard };
                ap = store.access_point(&account, other, &name).s3()?;
            }
            match ap {
                Some(ap) => (ap, path_key()?),
                // A bucket whose name looks like one (virtual-host style): not an access point.
                None if kind.is_none() => return Ok(()),
                None => return Err(no_such(&name)),
            }
        }
        Named::Arn { kind, account: acct, name, key } => {
            if acct != account {
                return Err(S3Error::msg(Code::AccessDenied, "The access point belongs to another account"));
            }
            (store.access_point(&account, kind, &name).s3()?.ok_or_else(|| no_such(&name))?, key)
        }
        Named::Alias(alias) => {
            let ap = store.access_point_by_alias(&alias).s3()?.ok_or_else(|| S3Error::new(Code::NoSuchBucket).with("BucketName", alias.clone()))?;
            (ap, ctx.key.clone())
        }
    };
    let region = ctx.server.cfg.region.clone();
    let req = match found.kind {
        AccessPointKind::Standard => {
            let arn = ap_arn(&region, &account, &found.name);
            let policy = load_policy(found.policy.as_deref())?;
            ApRequest { ap: found, arn, policy, olap: None }
        }
        AccessPointKind::ObjectLambda => {
            let supporting = store.access_point(&account, AccessPointKind::Standard, &found.bucket).s3()?.ok_or_else(|| no_such(&found.bucket))?;
            let arn = ap_arn(&region, &account, &supporting.name);
            let policy = load_policy(supporting.policy.as_deref())?;
            let oarn = olap_arn(&region, &account, &found.name);
            let opolicy = load_policy(found.policy.as_deref())?;
            ApRequest { ap: supporting, arn, policy, olap: Some((found, oarn, opolicy)) }
        }
    };
    ctx.bucket = Some(req.ap.bucket.clone());
    ctx.key = key;
    ctx.access_point = Some(Arc::new(req));
    Ok(())
}

/// What an access point allows a request to do at all (before its permissions): its network
/// origin, and object and listing operations only (bucket configuration goes to the bucket).
pub(crate) fn check_request(ctx: &Ctx) -> Result<(), S3Error> {
    let Some(r) = &ctx.access_point else { return Ok(()) };
    let vpc = r.ap.network_origin == "VPC" || r.olap.as_ref().is_some_and(|(o, ..)| o.network_origin == "VPC");
    if vpc && !ctx.internal() {
        return Err(S3Error::msg(Code::AccessDenied, "This access point only accepts requests from its VPC (inside roll's network)"));
    }
    if ctx.key.is_none() {
        let action = crate::access::action_of(ctx);
        let listing = matches!(ctx.method, Method::GET | Method::HEAD) && matches!(action, "s3:ListBucket" | "s3:ListBucketVersions" | "s3:ListBucketMultipartUploads" | "s3:GetBucketLocation");
        let delete_objects = ctx.method == Method::POST && action == "s3:DeleteObject";
        if !listing && !delete_objects {
            return Err(S3Error::msg(Code::InvalidRequest, "This operation isn't supported through an access point: address the bucket itself."));
        }
    }
    Ok(())
}

/// Authorize a request through an access point (see the module docs). `None`: it isn't one
/// (or it concerns another bucket, like a copy's source), so the usual rules apply.
#[allow(clippy::too_many_arguments)]
pub(crate) fn authorize(ctx: &Ctx, who: Who, a: &Access, bucket: &str, key: Option<&str>, action: &str, need: Need, prefix: Option<&str>) -> Option<Result<(), S3Error>> {
    let r = ctx.access_point.as_ref().filter(|r| r.ap.bucket == bucket)?;
    let denied = || S3Error::new(Code::AccessDenied);
    let account = Account { id: ctx.store().account_id(), owner: ctx.store().owner_id() };
    let p = crate::access::principal_of(ctx, who);
    let cond = Cond { ctx, who, bucket, key, account: &account, tags: OnceLock::new(), prefix };
    let ap_resource = match key {
        Some(k) => format!("{}/object/{k}", r.arn),
        None => r.arn.clone(),
    };
    let ap_decision = r.policy.as_ref().map_or(Decision::None, |pol| pol.evaluate(&p, &account, action, &ap_resource, &cond));
    let bucket_decision = a.policy.as_ref().map_or(Decision::None, |pol| pol.evaluate(&p, &account, action, &crate::access::resource_arn(bucket, key), &cond));
    if ap_decision == Decision::Deny || bucket_decision == Decision::Deny {
        return Some(Err(denied()));
    }
    if crate::access::key_allows(who, bucket, need) {
        return Some(Ok(()));
    }
    if ap_decision == Decision::Allow && bucket_decision == Decision::Allow {
        let public = r.policy.as_ref().is_some_and(Policy::is_public) || a.policy_public;
        let restricted = r.ap.block.restrict_public_buckets || a.block.restrict_public_buckets;
        if !(matches!(who, Who::Anonymous) && public && restricted) {
            return Some(Ok(()));
        }
    }
    Some(Err(denied()))
}

/// The condition keys of a request through an access point.
pub(crate) fn condition(ctx: &Ctx, key: &str) -> Option<Option<Vec<String>>> {
    let r = ctx.access_point.as_ref();
    let v = match key {
        "s3:dataaccesspointarn" => r.map(|r| r.arn.clone()),
        "s3:dataaccesspointaccount" => r.map(|r| r.ap.account.clone()),
        "s3:accesspointnetworkorigin" => r.map(|r| r.ap.network_origin.clone()),
        _ => return None,
    };
    Some(v.map(|v| vec![v]))
}

/// Check an access point policy as `PutAccessPointPolicy` does: valid, and every resource is
/// this access point (`arn` itself, or objects under `<arn>/object/`). Returns whether it's
/// public.
pub(crate) fn validate_policy(doc: &str, arn: &str) -> Result<bool, S3Error> {
    let malformed = |m: &str| S3Error::msg(Code::MalformedPolicy, m.to_string());
    if doc.len() > crate::policy::MAX_POLICY_BYTES {
        return Err(malformed("Policy exceeds the maximum allowed document size."));
    }
    let v: serde_json::Value = serde_json::from_str(doc).map_err(|e| malformed(&format!("Policies must be valid JSON: {e}")))?;
    let stmts: Vec<&serde_json::Value> = match v.get("Statement") {
        Some(serde_json::Value::Array(a)) => a.iter().collect(),
        Some(o @ serde_json::Value::Object(_)) => vec![o],
        _ => return Err(malformed("Missing required field Statement")),
    };
    for s in stmts {
        for field in ["Resource", "NotResource"] {
            let rs: Vec<&str> = match s.get(field) {
                None => continue,
                Some(serde_json::Value::String(x)) => vec![x.as_str()],
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect(),
                Some(_) => return Err(malformed("Policy has invalid resource")),
            };
            for res in rs {
                let rest = res.strip_prefix(arn);
                let ok = res == "*" || rest.is_some_and(|r| r.is_empty() || r.starts_with("/object/") || r == "/*" || r == "*");
                if !ok {
                    return Err(malformed("Policy has invalid resource"));
                }
            }
        }
    }
    let p = load_policy(Some(doc)).map_err(|_| malformed("Policies must be valid JSON and the first byte must be '{'"))?.ok_or_else(|| malformed("empty"))?;
    // Checked as a bucket policy would be (actions, principals, conditions).
    Policy::load(doc.replace("\"s3-object-lambda:", "\"s3:").as_bytes())?;
    Ok(p.is_public())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arns() {
        assert_eq!(parse_arn("arn:aws:s3:us-east-1:123456789012:accesspoint/ap1"), Some((AccessPointKind::Standard, "123456789012".into(), "ap1".into())));
        assert_eq!(parse_arn("arn:aws:s3-object-lambda:us-east-1:123456789012:accesspoint/o"), Some((AccessPointKind::ObjectLambda, "123456789012".into(), "o".into())));
        assert_eq!(parse_arn("arn:aws:s3:::bucket"), None);
        assert_eq!(parse_arn("arn:aws:s3:us-east-1:1:accesspoint/a/b"), None);
        assert_eq!(ap_arn("r", "1", "n"), "arn:aws:s3:r:1:accesspoint/n");
    }

    #[test]
    fn policies() {
        let arn = "arn:aws:s3:us-east-1:123456789012:accesspoint/ap1";
        let ok = format!(r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":"RKX"}},"Action":"s3:GetObject","Resource":"{arn}/object/*"}}]}}"#);
        assert_eq!(validate_policy(&ok, arn).unwrap(), false);
        let public = format!(r#"{{"Statement":[{{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"{arn}/object/*"}}]}}"#);
        assert!(validate_policy(&public, arn).unwrap());
        let other = r#"{"Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::bucket/*"}]}"#;
        assert_eq!(validate_policy(other, arn).unwrap_err().code, Code::MalformedPolicy);
        let olap = "arn:aws:s3-object-lambda:us-east-1:123456789012:accesspoint/o";
        let lam = format!(r#"{{"Statement":[{{"Effect":"Allow","Principal":{{"AWS":"RKX"}},"Action":"s3-object-lambda:GetObject","Resource":"{olap}"}}]}}"#);
        assert!(!validate_policy(&lam, olap).unwrap());
    }
}
