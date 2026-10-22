//! Bucket policies: parsing, validation (`MalformedPolicy`), evaluation and "is it public".
//!
//! A policy is the IAM JSON document S3 takes: `Version` (`2012-10-17` or `2008-10-17`), and
//! `Statement` as one object or an array of them, each with an optional `Sid`, `Effect`
//! (`Allow`/`Deny`), `Principal` or `NotPrincipal`, `Action` or `NotAction`, `Resource` or
//! `NotResource`, and an optional `Condition`.
//!
//! **Principals.** A principal matches a request when it is:
//!
//! - `"*"` or `{"AWS": "*"}`: anyone, including anonymous requests;
//! - the requester itself: an IAM user's ARN (`arn:aws:iam::<account>:user/<path><name>`), a
//!   roll account key's id (`{"AWS": "RK…"}`) or `arn:aws:iam::<account>:user/<key id>`, a role
//!   session's `arn:aws:sts::<account>:assumed-role/<role>/<session>`, a federated user's
//!   `arn:aws:sts::<account>:federated-user/<name>`, a key's canonical user id;
//! - the requester's role, `arn:aws:iam::<account>:role/<path><name>` (any session of it);
//! - the requester's account, `<account>`, `arn:aws:iam::<account>:root` or the account's
//!   canonical user id: as on AWS this *delegates* to the account (its root user, and whoever
//!   its own policies allow).
//!
//! [`Policy::evaluate`] says which of these matched ([`Via`]), since AWS treats them
//! differently: an allow naming the requester itself isn't bounded by its session policies or
//! permissions boundary, one naming its role is, and one naming its account needs the account's
//! identity policies to allow too (unless the requester is the account's root).
//! `Service` and `Federated` principals are accepted (log-delivery policies use them) and never
//! match an S3 request.
//!
//! **Evaluation**: a matching `Deny` wins; otherwise a matching `Allow` allows; otherwise the
//! policy has no say. Missing condition keys make a condition false, except for the negated
//! operators, `…IfExists` and `Null`, as in IAM.

use s3_proto::{Code, S3Error};
use serde_json::Value;
use std::net::IpAddr;

/// Largest policy document S3 accepts.
pub(crate) const MAX_POLICY_BYTES: usize = 20 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// No statement applies.
    None,
    Allow,
    Deny,
}

/// What a document is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// A bucket policy (principals, S3 actions and resources).
    Resource,
    /// An identity policy (no principals; any service's actions).
    Identity,
    /// A trust policy (principals, any service's actions, no resources).
    Trust,
    /// Any service's resource policy (the policy simulator's `ResourcePolicy`).
    AnyResource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq)]
enum Principals {
    /// `"*"` or `{"AWS": "*"}`.
    Anyone,
    Some { aws: Vec<String>, canonical: Vec<String>, federated: Vec<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    StringEquals,
    StringNotEquals,
    StringEqualsIgnoreCase,
    StringNotEqualsIgnoreCase,
    StringLike,
    StringNotLike,
    NumericEquals,
    NumericNotEquals,
    NumericLessThan,
    NumericLessThanEquals,
    NumericGreaterThan,
    NumericGreaterThanEquals,
    DateEquals,
    DateNotEquals,
    DateLessThan,
    DateLessThanEquals,
    DateGreaterThan,
    DateGreaterThanEquals,
    Bool,
    IpAddress,
    NotIpAddress,
    ArnEquals,
    ArnLike,
    ArnNotEquals,
    ArnNotLike,
    Null,
}

impl Op {
    fn parse(s: &str) -> Option<Self> {
        use Op::*;
        Some(match s {
            "StringEquals" => StringEquals,
            "StringNotEquals" => StringNotEquals,
            "StringEqualsIgnoreCase" => StringEqualsIgnoreCase,
            "StringNotEqualsIgnoreCase" => StringNotEqualsIgnoreCase,
            "StringLike" => StringLike,
            "StringNotLike" => StringNotLike,
            "NumericEquals" => NumericEquals,
            "NumericNotEquals" => NumericNotEquals,
            "NumericLessThan" => NumericLessThan,
            "NumericLessThanEquals" => NumericLessThanEquals,
            "NumericGreaterThan" => NumericGreaterThan,
            "NumericGreaterThanEquals" => NumericGreaterThanEquals,
            "DateEquals" => DateEquals,
            "DateNotEquals" => DateNotEquals,
            "DateLessThan" => DateLessThan,
            "DateLessThanEquals" => DateLessThanEquals,
            "DateGreaterThan" => DateGreaterThan,
            "DateGreaterThanEquals" => DateGreaterThanEquals,
            "Bool" => Bool,
            "IpAddress" => IpAddress,
            "NotIpAddress" => NotIpAddress,
            "ArnEquals" => ArnEquals,
            "ArnLike" => ArnLike,
            "ArnNotEquals" => ArnNotEquals,
            "ArnNotLike" => ArnNotLike,
            "Null" => Null,
            _ => return None,
        })
    }

    /// Negated operators are true when the key is missing.
    fn negated(self) -> bool {
        use Op::*;
        matches!(self, StringNotEquals | StringNotEqualsIgnoreCase | StringNotLike | NumericNotEquals | DateNotEquals | NotIpAddress | ArnNotEquals | ArnNotLike)
    }

    /// One request value against one policy value (before negation).
    fn test(self, have: &str, want: &str) -> bool {
        use Op::*;
        match self {
            StringEquals | StringNotEquals => have == want,
            StringEqualsIgnoreCase | StringNotEqualsIgnoreCase => have.eq_ignore_ascii_case(want),
            StringLike | StringNotLike | ArnLike | ArnNotLike => wildcard(want, have, false),
            ArnEquals | ArnNotEquals => have == want,
            NumericEquals | NumericNotEquals | NumericLessThan | NumericLessThanEquals | NumericGreaterThan | NumericGreaterThanEquals => {
                let (Ok(h), Ok(w)) = (have.trim().parse::<f64>(), want.trim().parse::<f64>()) else { return false };
                match self {
                    NumericEquals | NumericNotEquals => h == w,
                    NumericLessThan => h < w,
                    NumericLessThanEquals => h <= w,
                    NumericGreaterThan => h > w,
                    _ => h >= w,
                }
            }
            DateEquals | DateNotEquals | DateLessThan | DateLessThanEquals | DateGreaterThan | DateGreaterThanEquals => {
                let (Some(h), Some(w)) = (parse_date(have), parse_date(want)) else { return false };
                match self {
                    DateEquals | DateNotEquals => h == w,
                    DateLessThan => h < w,
                    DateLessThanEquals => h <= w,
                    DateGreaterThan => h > w,
                    _ => h >= w,
                }
            }
            Bool => have.eq_ignore_ascii_case(want),
            IpAddress | NotIpAddress => ip_in(have, want),
            Null => unreachable!("Null is decided on presence"),
        }
    }
}

/// An ISO 8601 date-time or Unix seconds, as Unix ms.
fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n * 1000);
    }
    if let Some(ms) = s3_proto::time::parse_iso8601(s) {
        return Some(ms);
    }
    // A bare date.
    s3_proto::time::parse_iso8601(&format!("{s}T00:00:00Z"))
}

fn ip_in(have: &str, want: &str) -> bool {
    let Ok(ip) = have.parse::<IpAddr>() else { return false };
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match want.parse::<ipnet::IpNet>() {
        Ok(net) => net.contains(&ip),
        Err(_) => want.parse::<IpAddr>().is_ok_and(|w| w == ip),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetOp {
    Single,
    ForAnyValue,
    ForAllValues,
}

#[derive(Clone, Debug, PartialEq)]
struct Condition {
    op: Op,
    set: SetOp,
    if_exists: bool,
    /// Lowercase (condition keys are case-insensitive).
    key: String,
    /// As the policy spells it (what the policy simulator reports).
    name: String,
    values: Vec<String>,
    /// Policy variables (`${aws:username}`) are substituted (`Version` 2012-10-17).
    vars: bool,
}

/// Substitute IAM policy variables: `${key}` with the request's value of the condition key,
/// `${key, 'default'}` with a default, `${*}`, `${?}` and `${$}` with those characters. `None`
/// when a variable has no value (what it's in then matches nothing, as in IAM).
fn expand(s: &str, ctx: &dyn Context) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return Some(out);
        };
        let inner = &after[..j];
        let (key, default) = match inner.split_once(',') {
            Some((k, d)) => (k.trim(), Some(d.trim().trim_matches('\''))),
            None => (inner.trim(), None),
        };
        let v = match key {
            "*" | "?" | "$" => Some(key.to_string()),
            k => ctx.values(&k.to_ascii_lowercase()).and_then(|v| v.into_iter().next()).or_else(|| default.map(str::to_string)),
        };
        out.push_str(&v?);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// The condition keys `${…}` policy variables in `s` name (not `${*}`, `${?}`, `${$}`).
fn variables(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else { break };
        let key = after[..j].split(',').next().unwrap_or_default().trim();
        if !matches!(key, "*" | "?" | "$") && !key.is_empty() {
            out.push(key.to_string());
        }
        rest = &after[j + 1..];
    }
    out
}

impl Condition {
    fn holds(&self, ctx: &dyn Context) -> bool {
        let have = ctx.values(&self.key);
        if self.op == Op::Null {
            let missing = have.as_ref().is_none_or(|v| v.is_empty());
            return self.values.iter().any(|w| w.eq_ignore_ascii_case(if missing { "true" } else { "false" }));
        }
        let Some(have) = have else {
            return self.if_exists || self.op.negated() || self.set == SetOp::ForAllValues;
        };
        // Policy variables in the values (a value whose variable is missing matches nothing).
        let expanded: Vec<String>;
        let values: &[String] = if self.vars && self.values.iter().any(|v| v.contains("${")) {
            expanded = self.values.iter().filter_map(|v| expand(v, ctx)).collect();
            &expanded
        } else {
            &self.values
        };
        let matches_one = |h: &str| values.iter().any(|w| self.op.test(h, w));
        let positive = |h: &str| if self.op.negated() { !matches_one(h) } else { matches_one(h) };
        match self.set {
            SetOp::Single => have.first().is_some_and(|h| positive(h)),
            SetOp::ForAnyValue => have.iter().any(|h| positive(h)),
            SetOp::ForAllValues => have.iter().all(|h| positive(h)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Statement {
    effect: Effect,
    principal: Option<Principals>,
    not_principal: Option<Principals>,
    action: Option<Vec<String>>,
    not_action: Option<Vec<String>>,
    resource: Option<Vec<String>>,
    not_resource: Option<Vec<String>>,
    conditions: Vec<Condition>,
    /// Policy variables are substituted in resources and condition values.
    vars: bool,
}

/// Who's asking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Principal {
    Anonymous,
    /// An authenticated requester.
    Known(Box<Requester>),
}

/// An authenticated requester, as resource policies and ACLs see it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Requester {
    /// Its account's 12-digit id and canonical user id.
    pub account: String,
    pub account_canonical: String,
    /// The account's root user (or a session of it).
    pub root: bool,
    /// Ids and ARNs naming exactly this requester.
    pub direct: Vec<String>,
    /// Canonical user ids naming exactly this requester (a roll key's).
    pub direct_canonical: Vec<String>,
    /// Its role's ARN (role sessions).
    pub role: Option<String>,
    /// The OIDC provider ARN of a web identity (`AssumeRoleWithWebIdentity` trust policies'
    /// `Federated` principals).
    pub federated: Option<String>,
}

impl Principal {
    pub fn requester(&self) -> Option<&Requester> {
        match self {
            Principal::Known(r) => Some(r),
            Principal::Anonymous => None,
        }
    }
}

/// How an `Allow` named the requester (AWS evaluates them differently).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Via {
    /// Its account (`<account>`, `arn:aws:iam::<account>:root`, the account's canonical id):
    /// delegated to the account's own policies.
    Account,
    /// Its role (bounded by its session policies and permissions boundary).
    Role,
    /// Itself, or anyone (`*`).
    Direct,
}

/// A policy's say on a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub deny: bool,
    /// The most direct way a matching `Allow` named the requester.
    pub allow: Option<Via>,
}

impl Verdict {
    pub fn decision(self) -> Decision {
        if self.deny {
            Decision::Deny
        } else if self.allow.is_some() {
            Decision::Allow
        } else {
            Decision::None
        }
    }
}

/// Condition values of the request.
pub(crate) trait Context {
    /// The values of a condition key (lowercase), `None` when the request doesn't have it.
    fn values(&self, key: &str) -> Option<Vec<String>>;
}

impl Principals {
    fn matches(&self, who: &Principal) -> Option<Via> {
        let Principals::Some { aws, canonical, federated } = self else { return Some(Via::Direct) };
        let r = match who {
            Principal::Known(r) => r,
            Principal::Anonymous => return aws.iter().any(|a| a == "*").then_some(Via::Direct),
        };
        let account = if r.root { Via::Direct } else { Via::Account };
        let root_arn = format!("arn:aws:iam::{}:root", r.account);
        let mut best: Option<Via> = None;
        let mut see = |v: Via| best = best.max(Some(v));
        for a in aws {
            if a == "*" || r.direct.iter().any(|d| d == a) {
                see(Via::Direct);
            } else if r.role.as_deref() == Some(a.as_str()) {
                see(Via::Role);
            } else if *a == r.account || *a == root_arn {
                see(account);
            }
        }
        for c in canonical {
            if r.direct_canonical.iter().any(|d| d == c) {
                see(Via::Direct);
            } else if *c == r.account_canonical {
                see(account);
            }
        }
        if r.federated.as_ref().is_some_and(|f| federated.contains(f)) {
            see(Via::Direct);
        }
        best
    }

    fn anyone(&self) -> bool {
        match self {
            Principals::Anyone => true,
            Principals::Some { aws, .. } => aws.iter().any(|a| a == "*"),
        }
    }
}

/// A parsed bucket policy.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Policy {
    statements: Vec<Statement>,
}

fn malformed(why: impl Into<String>) -> S3Error {
    S3Error::msg(Code::MalformedPolicy, why)
}

/// A string or an array of strings.
fn strings(v: &Value, what: &str) -> Result<Vec<String>, S3Error> {
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) if !a.is_empty() => a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| malformed(format!("{what} values must be strings")))).collect(),
        _ => Err(malformed(format!("{what} must be a string or a non-empty array of strings"))),
    }
}

/// Condition values: strings, numbers or booleans, alone or in an array.
fn condition_values(v: &Value) -> Result<Vec<String>, S3Error> {
    let one = |x: &Value| match x {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(malformed("Condition values must be strings, numbers or booleans")),
    };
    match v {
        Value::Array(a) if !a.is_empty() => a.iter().map(one).collect(),
        Value::Array(_) => Err(malformed("Condition values can't be empty")),
        x => Ok(vec![one(x)?]),
    }
}

fn principals(v: &Value) -> Result<Principals, S3Error> {
    match v {
        Value::String(s) if s == "*" => Ok(Principals::Anyone),
        Value::Object(m) if !m.is_empty() => {
            let mut aws = Vec::new();
            let mut canonical = Vec::new();
            let mut federated = Vec::new();
            for (k, v) in m {
                let vals = strings(v, "Principal")?;
                match k.as_str() {
                    "AWS" => {
                        for a in &vals {
                            if !valid_aws_principal(a) {
                                return Err(malformed("Invalid principal in policy"));
                            }
                        }
                        aws.extend(vals);
                    }
                    "CanonicalUser" => {
                        if vals.iter().any(|c| c.len() != 64 || !c.bytes().all(|b| b.is_ascii_hexdigit())) {
                            return Err(malformed("Invalid principal in policy"));
                        }
                        canonical.extend(vals.into_iter().map(|c| c.to_ascii_lowercase()));
                    }
                    // Accepted, never matches a roll request.
                    "Service" => {}
                    // OIDC providers (web identities in trust policies).
                    "Federated" => federated.extend(vals),
                    _ => return Err(malformed("Invalid principal in policy")),
                }
            }
            Ok(Principals::Some { aws, canonical, federated })
        }
        _ => Err(malformed("Invalid principal in policy")),
    }
}

/// `*`, an account id, an IAM ARN, or a roll access key id.
fn valid_aws_principal(a: &str) -> bool {
    a == "*"
        || (a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()))
        || ((a.starts_with("arn:aws:iam::") || a.starts_with("arn:aws:sts::")) && a.len() > 13 && !a.contains('*'))
        || (!a.is_empty() && a.len() <= 128 && a.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn validate_action(a: &str) -> Result<(), S3Error> {
    if a == "*" {
        return Ok(());
    }
    let invalid = || malformed("Policy has invalid action");
    let name = a.strip_prefix("s3:").or_else(|| a.strip_prefix("S3:")).ok_or_else(invalid)?;
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'*' || b == b'?') {
        return Err(invalid());
    }
    if !name.contains(['*', '?']) && !ACTIONS.iter().any(|k| k.eq_ignore_ascii_case(name)) {
        return Err(invalid());
    }
    Ok(())
}

/// `*`, or an S3 ARN of this bucket (`arn:aws:s3:::bucket` or `arn:aws:s3:::bucket/key…`,
/// with wildcards).
fn validate_resource(r: &str, bucket: &str) -> Result<(), S3Error> {
    if r == "*" {
        return Ok(());
    }
    let invalid = || malformed("Policy has invalid resource");
    let rest = r.strip_prefix("arn:").ok_or_else(invalid)?;
    let (partition, rest) = rest.split_once(':').ok_or_else(invalid)?;
    if partition.is_empty() || !partition.bytes().all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'*') {
        return Err(invalid());
    }
    let path = rest.strip_prefix("s3:::").ok_or_else(invalid)?;
    let b = path.split('/').next().unwrap_or_default();
    // The resource must be (able to be) this bucket.
    if b.is_empty() || !wildcard(b, bucket, false) {
        return Err(invalid());
    }
    Ok(())
}

impl Policy {
    /// Parse and validate a policy document for `bucket` (`PutBucketPolicy`).
    pub(crate) fn parse(doc: &[u8], bucket: &str) -> Result<Self, S3Error> {
        Self::parse_for(doc, Some(bucket))
    }

    /// Parse a stored policy (validated when it was put; its resources aren't checked again).
    pub(crate) fn load(doc: &[u8]) -> Result<Self, S3Error> {
        Self::parse_for(doc, None)
    }

    /// Parse an identity policy: a role's inline policy or a session policy (no `Principal`;
    /// any resource; other services' actions are accepted and never match). At most `max`
    /// bytes.
    pub(crate) fn parse_identity(doc: &[u8], max: usize) -> Result<Self, S3Error> {
        Self::parse_as(doc, None, Mode::Identity, max)
    }

    /// Parse a role's trust policy (`AssumeRolePolicyDocument`): principals, `sts:` actions,
    /// no resources.
    pub(crate) fn parse_trust(doc: &[u8], max: usize) -> Result<Self, S3Error> {
        Self::parse_as(doc, None, Mode::Trust, max)
    }

    /// Parse any service's resource policy (principals, any actions and resources): what the
    /// policy simulator takes as `ResourcePolicy`.
    pub(crate) fn parse_resource_any(doc: &[u8], max: usize) -> Result<Self, S3Error> {
        Self::parse_as(doc, None, Mode::AnyResource, max)
    }

    fn parse_for(doc: &[u8], bucket: Option<&str>) -> Result<Self, S3Error> {
        Self::parse_as(doc, bucket, Mode::Resource, MAX_POLICY_BYTES)
    }

    fn parse_as(doc: &[u8], bucket: Option<&str>, mode: Mode, max: usize) -> Result<Self, S3Error> {
        let identity = mode == Mode::Identity;
        let any_service = mode != Mode::Resource;
        if doc.len() > max {
            return Err(malformed("Policy exceeds the maximum allowed document size."));
        }
        let text = std::str::from_utf8(doc).map_err(|_| malformed("Policies must be valid JSON"))?;
        if !text.trim_start().starts_with('{') {
            return Err(S3Error::new(Code::MalformedPolicy));
        }
        let v: Value = serde_json::from_str(text).map_err(|e| malformed(format!("Policies must be valid JSON: {e}")))?;
        let top = v.as_object().ok_or_else(|| S3Error::new(Code::MalformedPolicy))?;
        for k in top.keys() {
            if !matches!(k.as_str(), "Version" | "Id" | "Statement") {
                return Err(malformed(format!("Unknown field {k}")));
            }
        }
        // Policy variables only exist in 2012-10-17 policies, as in IAM.
        let vars = match top.get("Version") {
            None => false,
            Some(Value::String(s)) if s == "2012-10-17" => true,
            Some(Value::String(s)) if s == "2008-10-17" => false,
            Some(_) => return Err(malformed("Invalid policy version")),
        };
        let stmts: Vec<&Value> = match top.get("Statement") {
            Some(Value::Array(a)) if !a.is_empty() => a.iter().collect(),
            Some(o @ Value::Object(_)) => vec![o],
            _ => return Err(malformed("Missing required field Statement")),
        };
        let mut sids = std::collections::HashSet::new();
        let mut statements = Vec::with_capacity(stmts.len());
        for s in stmts {
            let m = s.as_object().ok_or_else(|| malformed("Statements must be objects"))?;
            for k in m.keys() {
                if !matches!(k.as_str(), "Sid" | "Effect" | "Principal" | "NotPrincipal" | "Action" | "NotAction" | "Resource" | "NotResource" | "Condition") {
                    return Err(malformed(format!("Unknown field {k}")));
                }
            }
            if let Some(sid) = m.get("Sid") {
                let sid = sid.as_str().ok_or_else(|| malformed("Sid must be a string"))?;
                if !sid.is_empty() && !sids.insert(sid.to_string()) {
                    return Err(malformed("Statement IDs (SID) in a single policy must be unique"));
                }
            }
            let effect = match m.get("Effect").and_then(Value::as_str) {
                Some("Allow") => Effect::Allow,
                Some("Deny") => Effect::Deny,
                Some(e) => return Err(malformed(format!("Invalid effect: {e}"))),
                None => return Err(malformed("Missing required field Effect")),
            };
            let (principal, not_principal) = if identity {
                if m.contains_key("Principal") || m.contains_key("NotPrincipal") {
                    return Err(malformed("Policy document should not specify a principal."));
                }
                (Some(Principals::Anyone), None)
            } else {
                (m.get("Principal").map(principals).transpose()?, m.get("NotPrincipal").map(principals).transpose()?)
            };
            if principal.is_some() == not_principal.is_some() {
                return Err(malformed("Missing required field Principal"));
            }
            let action = m.get("Action").map(|v| strings(v, "Action")).transpose()?;
            let not_action = m.get("NotAction").map(|v| strings(v, "NotAction")).transpose()?;
            if action.is_some() == not_action.is_some() {
                return Err(malformed("Missing required field Action"));
            }
            for a in action.iter().chain(not_action.iter()).flatten() {
                match a.split_once(':') {
                    // Another service's action in an identity or trust policy (`sts:…`, `kms:…`).
                    Some((svc, name)) if any_service && !svc.eq_ignore_ascii_case("s3") => {
                        if svc.is_empty() || !svc.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'*' || b == b'?') {
                            return Err(malformed("Policy has invalid action"));
                        }
                    }
                    _ => validate_action(a)?,
                }
            }
            let mut resource = m.get("Resource").map(|v| strings(v, "Resource")).transpose()?;
            let not_resource = m.get("NotResource").map(|v| strings(v, "NotResource")).transpose()?;
            if mode == Mode::Trust {
                // The role is the resource of a trust policy.
                if resource.is_some() || not_resource.is_some() {
                    return Err(malformed("Has prohibited field Resource"));
                }
                resource = Some(vec!["*".into()]);
            }
            if resource.is_some() == not_resource.is_some() {
                return Err(malformed("Missing required field Resource"));
            }
            if let Some(bucket) = bucket {
                for r in resource.iter().chain(not_resource.iter()).flatten() {
                    validate_resource(r, bucket)?;
                }
            }
            let mut conditions = Vec::new();
            if let Some(c) = m.get("Condition") {
                let c = c.as_object().ok_or_else(|| malformed("Condition must be an object"))?;
                for (op_name, block) in c {
                    let (set, rest) = if let Some(r) = op_name.strip_prefix("ForAnyValue:") {
                        (SetOp::ForAnyValue, r)
                    } else if let Some(r) = op_name.strip_prefix("ForAllValues:") {
                        (SetOp::ForAllValues, r)
                    } else {
                        (SetOp::Single, op_name.as_str())
                    };
                    let (base, if_exists) = match rest.strip_suffix("IfExists") {
                        Some(b) if b != "Null" => (b, true),
                        _ => (rest, false),
                    };
                    let op = Op::parse(base).ok_or_else(|| malformed(format!("Invalid Condition type : {op_name}")))?;
                    let block = block.as_object().filter(|b| !b.is_empty()).ok_or_else(|| malformed(format!("Invalid Condition block for {op_name}")))?;
                    for (key, vals) in block {
                        let values = condition_values(vals)?;
                        if op == Op::IpAddress || op == Op::NotIpAddress {
                            for v in &values {
                                if v.parse::<ipnet::IpNet>().is_err() && v.parse::<IpAddr>().is_err() {
                                    return Err(malformed(format!("Invalid IP address: {v}")));
                                }
                            }
                        }
                        if matches!(op, Op::Bool | Op::Null) && values.iter().any(|v| !v.eq_ignore_ascii_case("true") && !v.eq_ignore_ascii_case("false")) {
                            return Err(malformed(format!("{op_name} takes true or false")));
                        }
                        conditions.push(Condition { op, set, if_exists, key: key.to_ascii_lowercase(), name: key.clone(), values, vars });
                    }
                }
            }
            statements.push(Statement { effect, principal, not_principal, action, not_action, resource, not_resource, conditions, vars });
        }
        Ok(Self { statements })
    }

    /// The policy's say on `who` doing `action` on `resource` (an ARN).
    pub(crate) fn evaluate(&self, who: &Principal, action: &str, resource: &str, ctx: &dyn Context) -> Verdict {
        let mut v = Verdict::default();
        for s in &self.statements {
            let Some(via) = s.applies(who, action, resource, ctx) else { continue };
            match s.effect {
                Effect::Deny => {
                    v.deny = true;
                    return v;
                }
                Effect::Allow => v.allow = v.allow.max(Some(via)),
            }
        }
        v
    }

    /// An identity policy's say (no principals): Deny, Allow or nothing.
    pub(crate) fn decide(&self, action: &str, resource: &str, ctx: &dyn Context) -> Decision {
        self.evaluate(&Principal::Anonymous, action, resource, ctx).decision()
    }

    /// The statements (by index, in document order) that apply to `who` doing `action` on
    /// `resource`, and whether each is a `Deny`: what [`Policy::evaluate`] decided from (the
    /// policy simulator's `MatchedStatements`).
    pub(crate) fn matching(&self, who: &Principal, action: &str, resource: &str, ctx: &dyn Context) -> Vec<(usize, bool)> {
        self.statements.iter().enumerate().filter(|(_, s)| s.applies(who, action, resource, ctx).is_some()).map(|(i, s)| (i, s.effect == Effect::Deny)).collect()
    }

    /// The condition keys (as spelled) of the statements about `action` on `resource`, and the
    /// keys their policy variables use: what a request must supply for them to be decided (the
    /// policy simulator's `MissingContextValues`). Without an action, every statement's.
    pub(crate) fn condition_keys(&self, action: Option<&str>, resource: &str, ctx: &dyn Context) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in &self.statements {
            if let Some(a) = action {
                let action_ok = match (&s.action, &s.not_action) {
                    (Some(x), _) => x.iter().any(|p| wildcard(p, a, true)),
                    (None, Some(nx)) => !nx.iter().any(|p| wildcard(p, a, true)),
                    _ => false,
                };
                let resource_ok = match (&s.resource, &s.not_resource) {
                    (Some(r), _) => r.iter().any(|p| p.contains("${") || s.resource_matches(p, resource, ctx)),
                    (None, Some(nr)) => !nr.iter().any(|p| !p.contains("${") && s.resource_matches(p, resource, ctx)),
                    _ => false,
                };
                if !action_ok || !resource_ok {
                    continue;
                }
            }
            let mut keys: Vec<String> = s.conditions.iter().map(|c| c.name.clone()).collect();
            if s.vars {
                for text in s.resource.iter().chain(s.not_resource.iter()).flatten().chain(s.conditions.iter().flat_map(|c| c.values.iter())) {
                    keys.extend(variables(text));
                }
            }
            for k in keys {
                if !out.iter().any(|o| o.eq_ignore_ascii_case(&k)) {
                    out.push(k);
                }
            }
        }
        out
    }

    /// The OIDC provider ARNs `Federated` principals name (a trust policy's web identities).
    pub(crate) fn federated_principals(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in &self.statements {
            if let Some(Principals::Some { federated, .. }) = &s.principal {
                for f in federated {
                    if !out.contains(f) {
                        out.push(f.clone());
                    }
                }
            }
        }
        out
    }

    /// S3's rule: a policy is public if it allows anyone without pinning the request to fixed
    /// values of certain keys (source IPs, VPCs, accounts, ARNs).
    pub(crate) fn is_public(&self) -> bool {
        self.statements.iter().any(|s| {
            let anyone = match (&s.principal, &s.not_principal) {
                (Some(p), _) => p.anyone(),
                // `NotPrincipal` with Allow: everyone but a few.
                (None, Some(_)) => true,
                _ => false,
            };
            s.effect == Effect::Allow && anyone && !s.conditions.iter().any(Condition::restricts)
        })
    }
}

impl Condition {
    /// Whether the condition limits a statement to fixed principals or networks.
    fn restricts(&self) -> bool {
        const FIXED: [&str; 11] = [
            "aws:sourcearn",
            "aws:sourcevpc",
            "aws:sourcevpce",
            "aws:sourceowner",
            "aws:sourceaccount",
            "aws:principalaccount",
            "aws:principalarn",
            "aws:principalorgid",
            "aws:userid",
            "aws:username",
            "s3:dataaccesspointaccount",
        ];
        if self.if_exists || self.set == SetOp::ForAllValues {
            return false;
        }
        match self.op {
            Op::IpAddress => self.key == "aws:sourceip" && self.values.iter().all(|v| v.parse::<ipnet::IpNet>().map_or(true, |n| n.prefix_len() >= if n.addr().is_ipv4() { 8 } else { 32 })),
            Op::StringEquals | Op::StringEqualsIgnoreCase | Op::ArnEquals => FIXED.contains(&self.key.as_str()),
            Op::StringLike | Op::ArnLike => FIXED.contains(&self.key.as_str()) && self.values.iter().all(|v| !v.contains(['*', '?'])),
            _ => false,
        }
    }
}

impl Statement {
    fn applies(&self, who: &Principal, action: &str, resource: &str, ctx: &dyn Context) -> Option<Via> {
        let via = match (&self.principal, &self.not_principal) {
            (Some(p), _) => p.matches(who),
            (None, Some(np)) => np.matches(who).is_none().then_some(Via::Direct),
            _ => None,
        }?;
        let action_ok = match (&self.action, &self.not_action) {
            (Some(a), _) => a.iter().any(|p| wildcard(p, action, true)),
            (None, Some(na)) => !na.iter().any(|p| wildcard(p, action, true)),
            _ => false,
        };
        let resource_ok = match (&self.resource, &self.not_resource) {
            (Some(r), _) => r.iter().any(|p| self.resource_matches(p, resource, ctx)),
            (None, Some(nr)) => !nr.iter().any(|p| self.resource_matches(p, resource, ctx)),
            _ => false,
        };
        (action_ok && resource_ok && self.conditions.iter().all(|c| c.holds(ctx))).then_some(via)
    }
}

impl Statement {
    /// A resource pattern, its policy variables substituted.
    fn resource_matches(&self, pattern: &str, resource: &str, ctx: &dyn Context) -> bool {
        if self.vars && pattern.contains("${") {
            return expand(pattern, ctx).is_some_and(|p| resource_matches(&p, resource));
        }
        resource_matches(pattern, resource)
    }
}

fn resource_matches(pattern: &str, resource: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // The partition is whatever the client wrote; ours is always `aws`.
    let normalize = |s: &str| s.splitn(3, ':').nth(2).map(str::to_string).unwrap_or_else(|| s.to_string());
    wildcard(&normalize(pattern), &normalize(resource), false)
}

/// IAM wildcards: `*` any run of characters, `?` any one.
pub(crate) fn wildcard(pattern: &str, value: &str, ignore_case: bool) -> bool {
    let (p, v): (Vec<char>, Vec<char>) = if ignore_case {
        (pattern.to_lowercase().chars().collect(), value.to_lowercase().chars().collect())
    } else {
        (pattern.chars().collect(), value.chars().collect())
    };
    // Iterative matching with backtracking to the last `*` (linear in practice).
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while j < v.len() {
        if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

/// The S3 actions policies may name (IAM's list for the `s3` service).
const ACTIONS: &[&str] = &[
    "AbortMultipartUpload",
    "BypassGovernanceRetention",
    "CreateBucket",
    "CreateJob",
    "DeleteBucket",
    "DeleteBucketOwnershipControls",
    "DeleteBucketPolicy",
    "DeleteBucketWebsite",
    "DeleteObject",
    "DeleteObjectTagging",
    "DeleteObjectVersion",
    "DeleteObjectVersionTagging",
    "GetAccelerateConfiguration",
    "GetAnalyticsConfiguration",
    "GetBucketAcl",
    "GetBucketCORS",
    "GetBucketLocation",
    "GetBucketLogging",
    "GetBucketNotification",
    "GetBucketObjectLockConfiguration",
    "GetBucketOwnershipControls",
    "GetBucketPolicy",
    "GetBucketPolicyStatus",
    "GetBucketPublicAccessBlock",
    "GetBucketRequestPayment",
    "GetBucketTagging",
    "GetBucketVersioning",
    "GetBucketWebsite",
    "GetEncryptionConfiguration",
    "GetIntelligentTieringConfiguration",
    "GetInventoryConfiguration",
    "GetLifecycleConfiguration",
    "GetMetricsConfiguration",
    "GetObject",
    "GetObjectAcl",
    "GetObjectAttributes",
    "GetObjectLegalHold",
    "GetObjectRetention",
    "GetObjectTagging",
    "GetObjectTorrent",
    "GetObjectVersion",
    "GetObjectVersionAcl",
    "GetObjectVersionAttributes",
    "GetObjectVersionForReplication",
    "GetObjectVersionTagging",
    "GetObjectVersionTorrent",
    "GetReplicationConfiguration",
    "ListAllMyBuckets",
    "ListBucket",
    "ListBucketMultipartUploads",
    "ListBucketVersions",
    "ListMultipartUploadParts",
    "ObjectOwnerOverrideToBucketOwner",
    "PutAccelerateConfiguration",
    "PutAnalyticsConfiguration",
    "PutBucketAcl",
    "PutBucketCORS",
    "PutBucketLogging",
    "PutBucketNotification",
    "PutBucketObjectLockConfiguration",
    "PutBucketOwnershipControls",
    "PutBucketPolicy",
    "PutBucketPublicAccessBlock",
    "PutBucketRequestPayment",
    "PutBucketTagging",
    "PutBucketVersioning",
    "PutBucketWebsite",
    "PutEncryptionConfiguration",
    "PutIntelligentTieringConfiguration",
    "PutInventoryConfiguration",
    "PutLifecycleConfiguration",
    "PutMetricsConfiguration",
    "PutObject",
    "PutObjectAcl",
    "PutObjectLegalHold",
    "PutObjectRetention",
    "PutObjectTagging",
    "PutObjectVersionAcl",
    "PutObjectVersionTagging",
    "PutReplicationConfiguration",
    "ReplicateDelete",
    "ReplicateObject",
    "ReplicateTags",
    "RestoreObject",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Req(HashMap<&'static str, Vec<String>>);

    impl Context for Req {
        fn values(&self, key: &str) -> Option<Vec<String>> {
            self.0.get(key).cloned()
        }
    }

    fn req(pairs: &[(&'static str, &str)]) -> Req {
        let mut m: HashMap<&'static str, Vec<String>> = HashMap::new();
        for (k, v) in pairs {
            m.entry(k).or_default().push(v.to_string());
        }
        Req(m)
    }

    /// The account the test principals belong to.
    struct Account;

    fn acct() -> Account {
        Account
    }

    impl Policy {
        /// [`Policy::evaluate`] as a decision.
        fn eval(&self, who: &Principal, _: &Account, action: &str, resource: &str, ctx: &dyn Context) -> Decision {
            self.evaluate(who, action, resource, ctx).decision()
        }
    }

    const ACCOUNT: &str = "123456789012";

    fn key_c(id: &str, canonical: &str) -> Principal {
        Principal::Known(Box::new(Requester {
            account: ACCOUNT.into(),
            account_canonical: "a".repeat(64),
            direct: vec![id.into(), format!("arn:aws:iam::{ACCOUNT}:user/{id}")],
            direct_canonical: vec![canonical.into()],
            ..Default::default()
        }))
    }

    fn key(id: &str) -> Principal {
        key_c(id, &"b".repeat(64))
    }

    fn session_principal(role: &str, session: &str) -> Principal {
        Principal::Known(Box::new(Requester { account: ACCOUNT.into(), account_canonical: "a".repeat(64), direct: vec![session.into()], role: Some(role.into()), ..Default::default() }))
    }

    #[test]
    fn policy_variables() {
        let p = Policy::parse_identity(
            br#"{"Version":"2012-10-17","Statement":[
                {"Effect":"Allow","Action":"s3:*","Resource":"arn:aws:s3:::home/${aws:username}/*"},
                {"Effect":"Allow","Action":"s3:ListBucket","Resource":"arn:aws:s3:::home","Condition":{"StringLike":{"s3:prefix":["${aws:username}/*", "${aws:PrincipalTag/team, 'none'}/*"]}}}]}"#,
            4096,
        )
        .unwrap();
        let alice = req(&[("aws:username", "alice"), ("s3:prefix", "alice/docs/")]);
        assert_eq!(p.decide("s3:GetObject", "arn:aws:s3:::home/alice/a.txt", &alice), Decision::Allow);
        assert_eq!(p.decide("s3:GetObject", "arn:aws:s3:::home/bob/a.txt", &alice), Decision::None);
        assert_eq!(p.decide("s3:ListBucket", "arn:aws:s3:::home", &alice), Decision::Allow);
        assert_eq!(p.decide("s3:ListBucket", "arn:aws:s3:::home", &req(&[("aws:username", "alice"), ("s3:prefix", "bob/")])), Decision::None);
        assert_eq!(p.decide("s3:ListBucket", "arn:aws:s3:::home", &req(&[("s3:prefix", "none/x")])), Decision::Allow, "the default");
        assert_eq!(p.decide("s3:GetObject", "arn:aws:s3:::home/x/a.txt", &req(&[])), Decision::None, "no username: no match");
        // 2008-10-17 policies have no variables.
        let old = Policy::parse_identity(br#"{"Version":"2008-10-17","Statement":[{"Effect":"Allow","Action":"s3:*","Resource":"arn:aws:s3:::home/${aws:username}/*"}]}"#, 4096).unwrap();
        assert_eq!(old.decide("s3:GetObject", "arn:aws:s3:::home/alice/a.txt", &alice), Decision::None);
    }

    #[test]
    fn how_principals_match() {
        let r = req(&[]);
        let allow = |p: serde_json::Value| policy(serde_json::json!({"Statement": [{"Effect": "Allow", "Principal": p, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"}]}));
        let via = |p: serde_json::Value, who: &Principal| allow(p).evaluate(who, "s3:GetObject", OBJ, &r).allow;
        let user = key("RK1");
        assert_eq!(via(serde_json::json!("*"), &user), Some(Via::Direct));
        assert_eq!(via(serde_json::json!({"AWS": "RK1"}), &user), Some(Via::Direct));
        assert_eq!(via(serde_json::json!({"AWS": ACCOUNT}), &user), Some(Via::Account), "the account delegates");
        assert_eq!(via(serde_json::json!({"AWS": format!("arn:aws:iam::{ACCOUNT}:root")}), &user), Some(Via::Account));
        assert_eq!(via(serde_json::json!({"AWS": "999999999999"}), &user), None, "another account");
        let root = Principal::Known(Box::new(Requester { account: ACCOUNT.into(), root: true, ..Default::default() }));
        assert_eq!(via(serde_json::json!({"AWS": ACCOUNT}), &root), Some(Via::Direct), "the root user is the account");
        let s = session_principal("arn:aws:iam::123456789012:role/r", "arn:aws:sts::123456789012:assumed-role/r/s");
        assert_eq!(via(serde_json::json!({"AWS": "arn:aws:iam::123456789012:role/r"}), &s), Some(Via::Role));
        assert_eq!(via(serde_json::json!({"AWS": ["arn:aws:iam::123456789012:role/r", "arn:aws:sts::123456789012:assumed-role/r/s"]}), &s), Some(Via::Direct), "the most direct wins");
        // Trust policies: Federated principals, no Resource.
        let t = Policy::parse_trust(br#"{"Statement":[{"Effect":"Allow","Principal":{"Federated":"arn:aws:iam::123456789012:oidc-provider/x.example"},"Action":"sts:AssumeRoleWithWebIdentity"}]}"#, 4096).unwrap();
        assert_eq!(t.federated_principals(), vec!["arn:aws:iam::123456789012:oidc-provider/x.example".to_string()]);
        let web = Principal::Known(Box::new(Requester { account: ACCOUNT.into(), federated: Some("arn:aws:iam::123456789012:oidc-provider/x.example".into()), ..Default::default() }));
        assert_eq!(t.evaluate(&web, "sts:AssumeRoleWithWebIdentity", "arn:aws:iam::123456789012:role/r", &r).allow, Some(Via::Direct));
        assert_eq!(t.evaluate(&user, "sts:AssumeRoleWithWebIdentity", "arn:aws:iam::123456789012:role/r", &r).allow, None);
        assert!(Policy::parse_trust(br#"{"Statement":[{"Effect":"Allow","Principal":"*","Action":"sts:AssumeRole","Resource":"*"}]}"#, 4096).is_err(), "no Resource in a trust policy");
        assert!(Policy::parse_trust(br#"{"Statement":[{"Effect":"Allow","Action":"sts:AssumeRole"}]}"#, 4096).is_err(), "a principal is required");
    }

    fn policy(v: serde_json::Value) -> Policy {
        Policy::parse(v.to_string().as_bytes(), "bkt").unwrap()
    }

    fn err(v: serde_json::Value) -> String {
        let e = Policy::parse(v.to_string().as_bytes(), "bkt").unwrap_err();
        assert_eq!(e.code, Code::MalformedPolicy);
        e.message
    }

    const OBJ: &str = "arn:aws:s3:::bkt/a/b.txt";

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "", false));
        assert!(wildcard("a*c", "abbbc", false));
        assert!(wildcard("a?c", "abc", false));
        assert!(!wildcard("a?c", "ac", false));
        assert!(wildcard("s3:Get*", "s3:getobject", true));
        assert!(!wildcard("s3:Get*", "s3:getobject", false));
        assert!(wildcard("arn:aws:s3:::bkt/*", OBJ, false));
        assert!(!wildcard("arn:aws:s3:::bkt/*", "arn:aws:s3:::bkt", false));
        assert!(wildcard("*a*b*", "xxaxxbxx", false));
        assert!(!wildcard("*a*b", "xxaxxbxxc", false));
    }

    #[test]
    fn public_read() {
        let p = policy(serde_json::json!({
            "Version": "2012-10-17",
            "Statement": {"Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"}
        }));
        let r = req(&[]);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&key("RK1"), &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:PutObject", OBJ, &r), Decision::None);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:ListBucket", "arn:aws:s3:::bkt", &r), Decision::None);
        assert!(p.is_public());
    }

    #[test]
    fn deny_wins_and_principals() {
        let p = policy(serde_json::json!({
            "Statement": [
                {"Effect": "Allow", "Principal": {"AWS": "*"}, "Action": ["s3:*"], "Resource": ["arn:aws:s3:::bkt", "arn:aws:s3:::bkt/*"]},
                {"Effect": "Deny", "Principal": {"AWS": ["RKBAD", "arn:aws:iam::123456789012:user/RKWORSE"]}, "Action": "s3:*", "Resource": "arn:aws:s3:::bkt/*"}
            ]
        }));
        let r = req(&[]);
        assert_eq!(p.eval(&key("RKGOOD"), &acct(), "s3:PutObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&key("RKBAD"), &acct(), "s3:GetObject", OBJ, &r), Decision::Deny);
        assert_eq!(p.eval(&key("RKWORSE"), &acct(), "s3:GetObject", OBJ, &r), Decision::Deny);
        assert_eq!(p.eval(&key("RKBAD"), &acct(), "s3:ListBucket", "arn:aws:s3:::bkt", &r), Decision::Allow, "the Deny names objects only");
        // Account-wide and canonical principals.
        let p = policy(serde_json::json!({"Statement": [
            {"Effect": "Allow", "Principal": {"AWS": "arn:aws:iam::123456789012:root"}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"},
            {"Effect": "Allow", "Principal": {"CanonicalUser": "b".repeat(64)}, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::bkt/*"}
        ]}));
        assert_eq!(p.eval(&key("RKANY"), &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", OBJ, &r), Decision::None);
        assert_eq!(p.eval(&key("RKANY"), &acct(), "s3:PutObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&key_c("RKX", &"c".repeat(64)), &acct(), "s3:PutObject", OBJ, &r), Decision::None);
        assert!(!p.is_public());
    }

    #[test]
    fn not_forms() {
        let p = policy(serde_json::json!({"Statement": [
            {"Effect": "Deny", "NotPrincipal": {"AWS": "RKADMIN"}, "NotAction": ["s3:Get*", "s3:List*"], "Resource": "arn:aws:s3:::bkt/*"},
            {"Effect": "Allow", "Principal": "*", "Action": "s3:*", "NotResource": "arn:aws:s3:::bkt/private/*"}
        ]}));
        let r = req(&[]);
        assert_eq!(p.eval(&key("RKADMIN"), &acct(), "s3:PutObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&key("RKOTHER"), &acct(), "s3:PutObject", OBJ, &r), Decision::Deny);
        assert_eq!(p.eval(&key("RKOTHER"), &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(p.eval(&key("RKOTHER"), &acct(), "s3:GetObject", "arn:aws:s3:::bkt/private/x", &r), Decision::None);
    }

    #[test]
    fn conditions() {
        let p = policy(serde_json::json!({"Statement": [
            {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"IpAddress": {"aws:SourceIp": ["10.0.0.0/8", "192.168.1.7"]}, "Bool": {"aws:SecureTransport": "true"}}},
            {"Effect": "Deny", "Principal": "*", "Action": "s3:*", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"DateGreaterThan": {"aws:CurrentTime": "2030-01-01T00:00:00Z"}}},
            {"Effect": "Allow", "Principal": "*", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::bkt",
             "Condition": {"StringLike": {"s3:prefix": ["home/*", ""]}, "NumericLessThanEquals": {"s3:max-keys": 100}, "StringEqualsIfExists": {"s3:delimiter": "/"}}},
            {"Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"StringNotEquals": {"s3:x-amz-acl": ["private", "bucket-owner-full-control"]}}},
            {"Effect": "Deny", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"StringEqualsIgnoreCase": {"s3:ExistingObjectTag/class": "SECRET"}}}
        ]}));
        let anon = Principal::Anonymous;
        let get = |pairs: &[(&'static str, &str)]| p.eval(&anon, &acct(), "s3:GetObject", OBJ, &req(pairs));
        assert_eq!(get(&[("aws:sourceip", "10.1.2.3"), ("aws:securetransport", "true"), ("aws:currenttime", "2026-01-01T00:00:00Z")]), Decision::Allow);
        assert_eq!(get(&[("aws:sourceip", "192.168.1.7"), ("aws:securetransport", "true")]), Decision::Allow);
        assert_eq!(get(&[("aws:sourceip", "8.8.8.8"), ("aws:securetransport", "true")]), Decision::None);
        assert_eq!(get(&[("aws:sourceip", "10.1.2.3"), ("aws:securetransport", "false")]), Decision::None);
        assert_eq!(get(&[("aws:sourceip", "::ffff:10.1.2.3"), ("aws:securetransport", "true")]), Decision::Allow, "v4-mapped addresses");
        assert_eq!(get(&[("aws:sourceip", "10.1.2.3"), ("aws:securetransport", "true"), ("aws:currenttime", "2031-01-01T00:00:00Z")]), Decision::Deny);
        assert_eq!(get(&[("aws:sourceip", "10.1.2.3"), ("aws:securetransport", "true"), ("s3:existingobjecttag/class", "secret")]), Decision::Deny);
        let list = |pairs: &[(&'static str, &str)]| p.eval(&anon, &acct(), "s3:ListBucket", "arn:aws:s3:::bkt", &req(pairs));
        assert_eq!(list(&[("s3:prefix", "home/x/"), ("s3:max-keys", "50")]), Decision::Allow);
        assert_eq!(list(&[("s3:prefix", "home/x/"), ("s3:max-keys", "50"), ("s3:delimiter", "/")]), Decision::Allow);
        assert_eq!(list(&[("s3:prefix", "home/x/"), ("s3:max-keys", "50"), ("s3:delimiter", "-")]), Decision::None);
        assert_eq!(list(&[("s3:prefix", "etc/"), ("s3:max-keys", "50")]), Decision::None);
        assert_eq!(list(&[("s3:prefix", "home/"), ("s3:max-keys", "500")]), Decision::None);
        assert_eq!(list(&[("s3:max-keys", "5")]), Decision::None, "a missing key fails a positive condition");
        let put = |pairs: &[(&'static str, &str)]| p.eval(&anon, &acct(), "s3:PutObject", OBJ, &req(pairs));
        assert_eq!(put(&[("s3:x-amz-acl", "public-read")]), Decision::Deny);
        assert_eq!(put(&[("s3:x-amz-acl", "private")]), Decision::None);
        assert_eq!(put(&[]), Decision::Deny, "negated operators hold when the key is missing");
        assert!(p.is_public(), "the ListBucket statement isn't pinned to fixed sources");
    }

    #[test]
    fn set_operators_and_null() {
        let p = policy(serde_json::json!({"Statement": [
            {"Effect": "Allow", "Principal": "*", "Action": "s3:PutObject", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"ForAllValues:StringEquals": {"s3:RequestObjectTagKeys": ["team", "env"]}, "Null": {"s3:x-amz-server-side-encryption": "false"}}},
            {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*",
             "Condition": {"ForAnyValue:StringLike": {"aws:UserAgent": ["*curl*"]}}}
        ]}));
        let put = |pairs: &[(&'static str, &str)]| p.eval(&Principal::Anonymous, &acct(), "s3:PutObject", OBJ, &req(pairs));
        assert_eq!(put(&[("s3:requestobjecttagkeys", "team"), ("s3:requestobjecttagkeys", "env"), ("s3:x-amz-server-side-encryption", "AES256")]), Decision::Allow);
        assert_eq!(put(&[("s3:requestobjecttagkeys", "team"), ("s3:requestobjecttagkeys", "cost"), ("s3:x-amz-server-side-encryption", "AES256")]), Decision::None);
        assert_eq!(put(&[("s3:x-amz-server-side-encryption", "AES256")]), Decision::Allow, "ForAllValues holds for no values");
        assert_eq!(put(&[("s3:requestobjecttagkeys", "team")]), Decision::None, "Null: false needs the key");
        let get = |pairs: &[(&'static str, &str)]| p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", OBJ, &req(pairs));
        assert_eq!(get(&[("aws:useragent", "curl/8.0")]), Decision::Allow);
        assert_eq!(get(&[("aws:useragent", "wget")]), Decision::None);
        assert_eq!(get(&[]), Decision::None);
    }

    #[test]
    fn publicness() {
        let public = |c: serde_json::Value| {
            let mut s = serde_json::json!({"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"});
            if !c.is_null() {
                s["Condition"] = c;
            }
            policy(serde_json::json!({"Statement": [s]})).is_public()
        };
        assert!(public(serde_json::Value::Null));
        assert!(!public(serde_json::json!({"IpAddress": {"aws:SourceIp": "203.0.113.0/24"}})));
        assert!(public(serde_json::json!({"IpAddress": {"aws:SourceIp": "0.0.0.0/0"}})));
        assert!(!public(serde_json::json!({"StringEquals": {"aws:SourceVpc": "vpc-1"}})));
        assert!(public(serde_json::json!({"StringLike": {"aws:SourceVpc": "vpc-*"}})));
        assert!(public(serde_json::json!({"StringEquals": {"s3:prefix": "x"}})));
        let deny = policy(serde_json::json!({"Statement": {"Effect": "Deny", "Principal": "*", "Action": "s3:*", "Resource": "arn:aws:s3:::bkt/*"}}));
        assert!(!deny.is_public());
    }

    #[test]
    fn malformed_policies() {
        let s = |stmt: serde_json::Value| serde_json::json!({"Version": "2012-10-17", "Statement": stmt});
        let base = || serde_json::json!({"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"});
        let with = |k: &str, v: serde_json::Value| {
            let mut b = base();
            b[k] = v;
            s(b)
        };
        let without = |k: &str| {
            let mut b = base();
            b.as_object_mut().unwrap().remove(k);
            s(b)
        };
        assert!(Policy::parse(b"not json", "bkt").is_err());
        assert!(Policy::parse(b"[]", "bkt").is_err());
        assert!(err(serde_json::json!({"Version": "2012-10-17"})).contains("Statement"));
        assert!(err(serde_json::json!({"Version": "2020-01-01", "Statement": base()})).contains("version"));
        assert!(err(with("Effect", "Maybe".into())).contains("effect"));
        assert!(err(without("Effect")).contains("Effect"));
        assert!(err(without("Principal")).contains("Principal"));
        assert!(err(without("Action")).contains("Action"));
        assert!(err(without("Resource")).contains("Resource"));
        assert!(err(with("Action", "s3:GetObjekt".into())).contains("action"));
        assert!(err(with("Action", "ec2:RunInstances".into())).contains("action"));
        assert!(err(with("Resource", "arn:aws:s3:::other/*".into())).contains("resource"));
        assert!(err(with("Resource", "bkt/*".into())).contains("resource"));
        assert!(err(with("Principal", serde_json::json!({"AWS": "arn:aws:iam::*:user/x"}))).contains("principal"));
        assert!(err(with("Principal", serde_json::json!({"Nobody": "x"}))).contains("principal"));
        assert!(err(with("Condition", serde_json::json!({"StringSortOf": {"aws:Referer": "x"}}))).contains("Condition"));
        assert!(err(with("Condition", serde_json::json!({"IpAddress": {"aws:SourceIp": "nope"}}))).contains("IP"));
        assert!(err(with("Extra", "x".into())).contains("Unknown"));
        assert!(err(s(serde_json::json!([{"Sid": "a", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "*"}, {"Sid": "a", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "*"}]))).contains("unique"));
        let big = format!("{{\"Statement\": [], \"Id\": \"{}\"}}", "x".repeat(MAX_POLICY_BYTES));
        assert!(Policy::parse(big.as_bytes(), "bkt").unwrap_err().message.contains("size"));
        // Fine: wildcards, other partitions, both versions, Service principals.
        policy(with("Action", serde_json::json!(["s3:Get*", "s3:*Object", "*"])));
        policy(with("Resource", serde_json::json!(["arn:aws:s3:::bkt", "arn:aws:s3:::b*/x", "*"])));
        policy(with("Principal", serde_json::json!({"Service": "logging.s3.amazonaws.com"})));
        policy(serde_json::json!({"Version": "2008-10-17", "Id": "x", "Statement": [base()]}));
    }

    #[test]
    fn role_and_session_principals() {
        let role = "arn:aws:iam::123456789012:role/uploader";
        let session = "arn:aws:sts::123456789012:assumed-role/uploader/web-1";
        let who = session_principal(role, session);
        let r = req(&[]);
        let allow = |p: serde_json::Value| policy(serde_json::json!({"Statement": [{"Effect": "Allow", "Principal": p, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::bkt/*"}]}));
        assert_eq!(allow(serde_json::json!({"AWS": role})).eval(&who, &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(allow(serde_json::json!({"AWS": session})).eval(&who, &acct(), "s3:GetObject", OBJ, &r), Decision::Allow);
        assert_eq!(allow(serde_json::json!({"AWS": "arn:aws:sts::123456789012:assumed-role/uploader/web-2"})).eval(&who, &acct(), "s3:GetObject", OBJ, &r), Decision::None);
        assert_eq!(allow(serde_json::json!({"AWS": "arn:aws:iam::123456789012:role/other"})).eval(&who, &acct(), "s3:GetObject", OBJ, &r), Decision::None);
        assert_eq!(allow(serde_json::json!({"AWS": "123456789012"})).eval(&who, &acct(), "s3:GetObject", OBJ, &r), Decision::Allow, "the account");
        assert_eq!(allow(serde_json::json!({"AWS": role})).eval(&key("RK1"), &acct(), "s3:GetObject", OBJ, &r), Decision::None, "a key isn't the role");
        assert!(Policy::parse(serde_json::json!({"Statement": [{"Effect": "Allow", "Principal": {"AWS": "arn:aws:sts::1:assumed-role/*"}, "Action": "s3:*", "Resource": "*"}]}).to_string().as_bytes(), "bkt").is_err(), "no wildcards in ARNs");
    }

    #[test]
    fn identity_policies() {
        let id = |v: serde_json::Value| Policy::parse_identity(v.to_string().as_bytes(), 2048);
        let p = id(serde_json::json!({"Version": "2012-10-17", "Statement": [
            {"Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject", "kms:Decrypt"], "Resource": ["arn:aws:s3:::bkt/up/*", "arn:aws:kms:us-east-1:1:key/x"]},
            {"Effect": "Deny", "Action": "s3:PutObject", "Resource": "*", "Condition": {"StringNotEquals": {"aws:PrincipalTag/team": "red"}}},
        ]}))
        .unwrap();
        let red = req(&[("aws:principaltag/team", "red")]);
        let blue = req(&[("aws:principaltag/team", "blue")]);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", "arn:aws:s3:::bkt/up/a", &red), Decision::Allow);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:PutObject", "arn:aws:s3:::bkt/up/a", &red), Decision::Allow);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:PutObject", "arn:aws:s3:::bkt/up/a", &blue), Decision::Deny);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", "arn:aws:s3:::bkt/other", &red), Decision::None);
        assert_eq!(p.eval(&Principal::Anonymous, &acct(), "s3:GetObject", "arn:aws:s3:::other/up/a", &red), Decision::None, "any bucket can be named; only matching ones allow");
        // A principal, an unknown S3 action, a malformed action and size are refused.
        assert!(id(serde_json::json!({"Statement": [{"Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "*"}]})).is_err());
        assert!(id(serde_json::json!({"Statement": [{"Effect": "Allow", "NotPrincipal": {"AWS": "x"}, "Action": "s3:*", "Resource": "*"}]})).is_err());
        assert!(id(serde_json::json!({"Statement": [{"Effect": "Allow", "Action": "s3:Teleport", "Resource": "*"}]})).is_err());
        assert!(id(serde_json::json!({"Statement": [{"Effect": "Allow", "Action": "not an action", "Resource": "*"}]})).is_err());
        assert!(Policy::parse_identity(&[b' '; 2049], 2048).is_err());
        id(serde_json::json!({"Statement": {"Effect": "Allow", "Action": "*", "Resource": "*"}})).unwrap();
    }
}
