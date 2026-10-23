//! The policy simulator: `SimulatePrincipalPolicy`, `SimulateCustomPolicy` and
//! `GetContextKeysFor{Principal,Custom}Policy`.
//!
//! Decisions come from the evaluator requests are checked with, never a copy of it: the
//! principal's policies are collected as `identity.rs` collects them for a request
//! ([`crate::identity::user_sources`] and friends), an [`Identity`] is built from them and asked
//! for its [`Identity::verdict`], a `ResourcePolicy` is [`Policy::evaluate`]d against the caller,
//! and the two are combined by [`crate::access::combine`] (same account: either; another
//! account: both). As on AWS, the only condition values are the `ContextEntries` given; the
//! condition keys the deciding statements need but weren't given are `MissingContextValues`.
//! `MatchedStatements` are the statements that decided: the denying ones for `explicitDeny`, the
//! allowing ones for `allowed`, with their positions in the policy's text.

use super::*;
use crate::identity::{Source, bounding, group_sources, role_sources, user_sources};
use crate::policy::{Context, Principal, Requester};
use s3_store::iam::{Group, User, kind};
use std::sync::Arc;

pub(super) async fn run(c: &Call<'_>, action: &str) -> Result<Option<String>, StsError> {
    Ok(Some(match action {
        "SimulatePrincipalPolicy" => simulate(c, true)?,
        "SimulateCustomPolicy" => simulate(c, false)?,
        "GetContextKeysForPrincipalPolicy" => context_keys(c, true)?,
        "GetContextKeysForCustomPolicy" => context_keys(c, false)?,
        _ => unreachable!("routed here"),
    }))
}

/// Largest input policy.
const MAX_INPUT: usize = 131_072;

/// The simulation's condition values: only what `ContextEntries` gives.
struct SimCtx(Vec<(String, Vec<String>)>);

impl Context for SimCtx {
    fn values(&self, key: &str) -> Option<Vec<String>> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }
}

/// A policy taking part, its text (for positions) and how it takes part.
struct Input {
    source: Source,
    policy: Option<Arc<Policy>>,
}

fn malformed(m: impl Into<String>) -> StsError {
    StsError::new("MalformedPolicyDocument", 400, m)
}

/// `PolicyInputList.member.N` (or another list of documents) as `none`-type sources.
fn inputs(c: &Call, param: &str) -> Result<Vec<Input>, StsError> {
    let mut out = Vec::new();
    for (i, doc) in c.p.list(param)?.into_iter().enumerate() {
        if doc.len() > MAX_INPUT {
            return Err(limit(format!("Cannot exceed quota for PolicySize: {MAX_INPUT}")));
        }
        let p = Policy::parse_identity(doc.as_bytes(), MAX_INPUT).map_err(|e| malformed(e.message))?;
        out.push(Input { source: Source { id: format!("{param}.{}", i + 1), kind: "none", doc: Some(doc) }, policy: Some(Arc::new(p)) });
    }
    Ok(out)
}

fn from_sources(src: Vec<Source>) -> Vec<Input> {
    src.into_iter().map(|s| Input { policy: s.doc.as_deref().and_then(crate::identity::parsed), source: s }).collect()
}

/// `ContextEntries.member.N`: names, types and values.
fn context(c: &Call) -> Result<SimCtx, StsError> {
    let names = c.p.members("ContextEntries", "ContextKeyName")?;
    let mut out = Vec::new();
    for (n, name) in names {
        let prefix = format!("ContextEntries.member.{n}.ContextKeyValues");
        let values = c.p.list(&prefix)?;
        let ty = c.p.get(&format!("ContextEntries.member.{n}.ContextKeyType")).unwrap_or("string");
        const TYPES: [&str; 12] = ["string", "stringList", "numeric", "numericList", "boolean", "booleanList", "ip", "ipList", "binary", "binaryList", "date", "dateList"];
        if !TYPES.contains(&ty) {
            return Err(validation(format!("1 validation error detected: Value '{ty}' at 'contextEntries.{n}.member.contextKeyType' failed to satisfy constraint: Member must satisfy enum value set: [{}]", TYPES.join(", "))));
        }
        if !ty.ends_with("List") && values.len() > 1 {
            return Err(invalid_input(format!("The context key {name} of type {ty} takes one value.")));
        }
        out.push((name.to_ascii_lowercase(), values));
    }
    Ok(SimCtx(out))
}

/// Who is simulated: what its own side is made of.
struct Principal_ {
    identity: crate::identity::Identity,
    /// The roll permission level of a control-plane role (on its buckets).
    level: Option<(s3_store::Permission, Option<Vec<String>>)>,
}

/// An `Identity` with these policies, as `identity::resolve` makes one.
fn identity(c: &Call, kind_: Kind, arn: &str, user_id: &str, policies: &[Input], bounds: &[Input], req: Requester) -> crate::identity::Identity {
    crate::identity::Identity {
        account: c.id.account.clone(),
        kind: kind_,
        principal: Principal::Known(Box::new(req)),
        principal_arn: arn.to_string(),
        caller_arn: arn.to_string(),
        user_id: user_id.to_string(),
        username: None,
        tags: Vec::new(),
        policies: policies.iter().filter_map(|i| i.policy.clone()).collect(),
        boundaries: bounds.iter().map(|i| i.policy.clone().unwrap_or_else(|| bounding(&i.source))).collect(),
        session_policies: Vec::new(),
        session: None,
        dead: false,
        app: None,
    }
}

/// What a roll permission level allows (its equivalent policy, as documented): `read` is
/// `s3:Get*`/`s3:List*`, `write` adds the object writes, `admin` is `s3:*`, on its buckets.
fn level_allows(level: &Option<(s3_store::Permission, Option<Vec<String>>)>, action: &str, resource: &str) -> bool {
    let Some((p, buckets)) = level else { return false };
    let Some(name) = action.strip_prefix("s3:") else { return false };
    let bucket = resource.strip_prefix("arn:aws:s3:::").map(|r| r.split('/').next().unwrap_or_default());
    let covered = match (buckets, bucket) {
        (None, _) => true,
        (Some(bs), Some(b)) => bs.iter().any(|x| x == b),
        (Some(_), None) => false,
    };
    let read = name.starts_with("Get") || name.starts_with("List");
    let write = read || name.starts_with("PutObject") || name.starts_with("DeleteObject") || matches!(name, "AbortMultipartUpload" | "RestoreObject");
    covered
        && match p {
            s3_store::Permission::Read => read,
            s3_store::Permission::Write => write,
            s3_store::Permission::Admin => true,
        }
}

/// The account of an ARN (`arn:aws:<svc>::<account>:…`), or a bare account id.
fn account_of(arn: &str) -> Option<String> {
    if arn.len() == 12 && arn.bytes().all(|b| b.is_ascii_digit()) {
        return Some(arn.to_string());
    }
    let a = arn.split(':').nth(4)?;
    (a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit())).then(|| a.to_string())
}

/// `(line, column)` spans (1-based, the end one past the closing brace) of a policy's
/// statements, in document order.
pub(crate) fn statement_spans(text: &str) -> Vec<((usize, usize), (usize, usize))> {
    let b = text.as_bytes();
    let pos = |i: usize| {
        let before = &text[..i];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        (line, col)
    };
    // Walk the JSON with a stack of open containers (bracket, where it opened): the value of
    // the top-level `Statement` key is one statement object, or an array of them.
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut key: Option<&str> = None;
    // 0: before `Statement`'s value, 1: inside it, 2: past it.
    let mut state = 0;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let s = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if stack.len() == 1 && b.get(j) == Some(&b':') {
                    key = text.get(s..i.min(b.len()));
                }
            }
            c @ (b'{' | b'[') => {
                stack.push((c, i));
                if stack.len() == 2 && state == 0 && key == Some("Statement") {
                    state = 1;
                }
            }
            c @ (b'}' | b']') => {
                let Some((open, s)) = stack.pop() else { break };
                if state == 1 && c == b'}' && open == b'{' {
                    // The Statement value itself (one statement), or an element of its array.
                    let element = stack.len() == 2 && stack[1].0 == b'[';
                    if stack.len() == 1 || element {
                        out.push((pos(s), pos(i + 1)));
                    }
                }
                if state == 1 && stack.len() == 1 {
                    state = 2;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// A statement's start and end in its policy's text: (line, column) each.
type Span = ((usize, usize), (usize, usize));

fn statements_xml(matched: &[(String, &'static str, Span)]) -> String {
    let members: String = matched
        .iter()
        .map(|(id, kind, ((l1, c1), (l2, c2)))| {
            format!(
                "<member>{}{}<StartPosition><Line>{l1}</Line><Column>{c1}</Column></StartPosition><EndPosition><Line>{l2}</Line><Column>{c2}</Column></EndPosition></member>",
                el("SourcePolicyId", id),
                el("SourcePolicyType", kind)
            )
        })
        .collect();
    format!("<MatchedStatements>{members}</MatchedStatements>")
}

fn simulate(c: &Call, principal: bool) -> Result<String, StsError> {
    let actions = c.p.list("ActionNames")?;
    if actions.is_empty() {
        return Err(validation("1 validation error detected: Value null at 'actionNames' failed to satisfy constraint: Member must not be null"));
    }
    let mut resources = c.p.list("ResourceArns")?;
    if resources.is_empty() {
        resources.push("*".into());
    }
    let ctx = context(c)?;
    let caller_arn = c.p.get("CallerArn").map(str::to_string);
    let extra = inputs(c, "PolicyInputList")?;
    let boundary_inputs = inputs(c, "PermissionsBoundaryPolicyInputList")?;
    // Who: the principal and its policies (or, for a custom simulation, the input policies).
    let (mut sim, mut own, mut bounds): (Principal_, Vec<Input>, Vec<Input>);
    let acct = c.account.clone();
    if principal {
        let source = c.p.required("PolicySourceArn", "policySourceArn")?.to_string();
        c.allow("SimulatePrincipalPolicy", &source)?;
        let gone = || no_such(format!("The entity {source} cannot be found."));
        let rest = source.strip_prefix(&format!("arn:aws:iam::{acct}:")).ok_or_else(gone)?;
        let name = rest.rsplit('/').next().unwrap_or_default();
        let mut req = Requester { account: acct.clone(), account_canonical: c.id.account.canonical.clone(), ..Default::default() };
        if rest.starts_with("user/") {
            let u: User = c.store.iam_user(&acct, name).map_err(store_error)?.filter(|u| crate::identity::user_arn(&acct, &u.path, &u.name) == source).ok_or_else(gone)?;
            let (src, b) = user_sources(c.store, &acct, &u);
            own = from_sources(src);
            bounds = from_sources(b.into_iter().collect());
            req.direct = vec![source.clone()];
            let id = identity(c, Kind::User { name: u.name.clone(), id: u.id.clone(), arn: source.clone() }, &source, &u.id, &[], &[], req);
            sim = Principal_ { identity: id, level: None };
        } else if rest.starts_with("group/") {
            let g: Group = c.store.iam_read(|t| t.get(&acct, kind::GROUP, name)).map_err(store_error)?.ok_or_else(gone)?;
            own = from_sources(group_sources(c.store, &acct, &g));
            bounds = Vec::new();
            req.direct = vec![source.clone()];
            sim = Principal_ { identity: identity(c, Kind::Key, &source, &g.id, &[], &[], req), level: None };
        } else if rest.starts_with("role/") {
            let r = c.store.role_in(&acct, name).map_err(store_error)?.filter(|r| r.own_arn() == source).ok_or_else(gone)?;
            let (src, b) = role_sources(c.store, &acct, &r);
            own = from_sources(src);
            bounds = from_sources(b);
            req.role = Some(source.clone());
            let level = r.permission.map(|p| (p, r.buckets.clone()));
            sim = Principal_ { identity: identity(c, Kind::Role { name: r.name.clone(), id: r.id.clone(), session: String::new(), arn: source.clone() }, &source, &r.id, &[], &[], req), level };
        } else {
            return Err(invalid_input(format!("{source} isn't a user, group or role.")));
        }
    } else {
        c.allow("SimulateCustomPolicy", "*")?;
        if extra.is_empty() {
            return Err(validation("1 validation error detected: Value null at 'policyInputList' failed to satisfy constraint: Member must not be null"));
        }
        own = Vec::new();
        bounds = Vec::new();
        let arn = caller_arn.clone().unwrap_or_default();
        let req = Requester { account: account_of(&arn).unwrap_or_else(|| acct.clone()), direct: caller_arn.iter().cloned().collect(), ..Default::default() };
        sim = Principal_ { identity: identity(c, Kind::Key, &arn, "", &[], &[], req), level: None };
    }
    own.extend(extra);
    if !boundary_inputs.is_empty() {
        bounds = boundary_inputs;
    }
    // The caller a resource policy sees.
    if let Some(arn) = &caller_arn
        && let Principal::Known(r) = &mut sim.identity.principal
    {
        r.direct = vec![arn.clone()];
        r.account = account_of(arn).unwrap_or_else(|| acct.clone());
    }
    let caller_account = sim.identity.principal.requester().map(|r| r.account.clone()).unwrap_or_else(|| acct.clone());
    let resource_policy = match c.p.get("ResourcePolicy") {
        Some(doc) => {
            let p = Policy::parse_resource_any(doc.as_bytes(), MAX_INPUT).map_err(|e| malformed(e.message))?;
            Some(Input { source: Source { id: "ResourcePolicy".into(), kind: "resource", doc: Some(doc.to_string()) }, policy: Some(Arc::new(p)) })
        }
        None => None,
    };
    let owner = match c.p.get("ResourceOwner") {
        Some(o) => account_of(o).ok_or_else(|| invalid_input(format!("ResourceOwner {o} isn't an account ARN.")))?,
        None => caller_account.clone(),
    };
    sim.identity.policies = own.iter().filter_map(|i| i.policy.clone()).collect();
    sim.identity.boundaries = bounds.iter().map(|i| i.policy.clone().unwrap_or_else(|| bounding(&i.source))).collect();
    let spans = |i: &Input| i.source.doc.as_deref().map(statement_spans).unwrap_or_default();
    let mut results = Vec::new();
    for action in &actions {
        for resource in &resources {
            // The real evaluator: the identity's verdict, the resource policy's, combined.
            let v = sim.identity.verdict(action, resource, &ctx, level_allows(&sim.level, action, resource));
            let rv = resource_policy.as_ref().and_then(|i| i.policy.as_ref()).map(|p| p.evaluate(&sim.identity.principal, action, resource, &ctx)).unwrap_or_default();
            let deny = v.deny || rv.deny;
            let allowed = !deny && crate::access::combine(Some(v), rv.allow, caller_account == owner);
            let decision = if deny { "explicitDeny" } else if allowed { "allowed" } else { "implicitDeny" };
            // The statements that decided, and the context they'd need.
            let mut matched = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            let who = &sim.identity.principal;
            let anyone = Principal::Anonymous;
            let mut look = |i: &Input, principals: bool, counts: bool| {
                let Some(p) = &i.policy else { return };
                let sp = spans(i);
                if counts && decision != "implicitDeny" {
                    for (n, is_deny) in p.matching(if principals { who } else { &anyone }, action, resource, &ctx) {
                        if is_deny == deny
                            && let Some(span) = sp.get(n)
                        {
                            matched.push((i.source.id.clone(), i.source.kind, *span));
                        }
                    }
                }
                for k in p.condition_keys(Some(action), resource, &ctx) {
                    if ctx.values(&k.to_ascii_lowercase()).is_none() && !missing.iter().any(|m| m.eq_ignore_ascii_case(&k)) {
                        missing.push(k);
                    }
                }
            };
            for i in &own {
                look(i, false, true);
            }
            for i in &bounds {
                look(i, false, deny);
            }
            if let Some(i) = &resource_policy {
                look(i, true, deny || rv.allow.is_some());
            }
            let mut r = format!("{}{}{}", el("EvalActionName", action), el("EvalResourceName", resource), el("EvalDecision", decision));
            r.push_str(&statements_xml(&matched));
            r.push_str(&format!("<MissingContextValues>{}</MissingContextValues>", missing.iter().map(|m| format!("<member>{}</member>", escape(m))).collect::<String>()));
            if !bounds.is_empty() {
                r.push_str(&format!("<PermissionsBoundaryDecisionDetail><AllowedByPermissionsBoundary>{}</AllowedByPermissionsBoundary></PermissionsBoundaryDecisionDetail>", v.bounded && !v.deny));
            }
            if resource_policy.is_some() {
                let identity_decision = if v.deny { "explicitDeny" } else if v.allowed() { "allowed" } else { "implicitDeny" };
                let resource_decision = if rv.deny { "explicitDeny" } else if rv.allow.is_some() { "allowed" } else { "implicitDeny" };
                r.push_str(&format!("<EvalDecisionDetails><entry><key>IAM Policy</key><value>{identity_decision}</value></entry><entry><key>Resource Policy</key><value>{resource_decision}</value></entry></EvalDecisionDetails>"));
            }
            results.push(r);
        }
    }
    let indexed: Vec<(usize, String)> = results.into_iter().enumerate().collect();
    page(c.p, indexed, |(i, _)| format!("{i:08}"), |(_, r)| r.clone(), "EvaluationResults")
}

fn context_keys(c: &Call, principal: bool) -> Result<String, StsError> {
    let mut docs = inputs(c, "PolicyInputList")?;
    if principal {
        let source = c.p.required("PolicySourceArn", "policySourceArn")?.to_string();
        c.allow("GetContextKeysForPrincipalPolicy", &source)?;
        let acct = c.account.clone();
        let gone = || no_such(format!("The entity {source} cannot be found."));
        let rest = source.strip_prefix(&format!("arn:aws:iam::{acct}:")).ok_or_else(gone)?;
        let name = rest.rsplit('/').next().unwrap_or_default();
        if rest.starts_with("user/") {
            let u: User = c.store.iam_user(&acct, name).map_err(store_error)?.ok_or_else(gone)?;
            docs.extend(from_sources(user_sources(c.store, &acct, &u).0));
        } else if rest.starts_with("group/") {
            let g: Group = c.store.iam_read(|t| t.get(&acct, kind::GROUP, name)).map_err(store_error)?.ok_or_else(gone)?;
            docs.extend(from_sources(group_sources(c.store, &acct, &g)));
        } else if rest.starts_with("role/") {
            let r = c.store.role_in(&acct, name).map_err(store_error)?.ok_or_else(gone)?;
            docs.extend(from_sources(role_sources(c.store, &acct, &r).0));
        } else {
            return Err(gone());
        }
    } else {
        c.allow("GetContextKeysForCustomPolicy", "*")?;
    }
    let none = SimCtx(Vec::new());
    let mut keys: Vec<String> = Vec::new();
    for d in &docs {
        if let Some(p) = &d.policy {
            for k in p.condition_keys(None, "*", &none) {
                if !keys.iter().any(|x| x.eq_ignore_ascii_case(&k)) {
                    keys.push(k);
                }
            }
        }
    }
    Ok(format!("<ContextKeyNames>{}</ContextKeyNames>", keys.iter().map(|k| format!("<member>{}</member>", escape(k))).collect::<String>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_of_statements() {
        let one_line = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:*","Resource":"*"},{"Effect":"Deny","Action":"s3:Delete*","Resource":"*"}]}"#;
        let s = statement_spans(one_line);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, (1, 38));
        assert_eq!(&one_line[s[0].0.1 - 1..s[0].1.1 - 1], r#"{"Effect":"Allow","Action":"s3:*","Resource":"*"}"#);
        let pretty = "{\n  \"Statement\": {\n    \"Effect\": \"Allow\",\n    \"Action\": \"s3:*\",\n    \"Resource\": \"*\"\n  }\n}";
        assert_eq!(statement_spans(pretty), vec![((2, 16), (6, 4))]);
        let tricky = r#"{"Id":"{[\"x","Statement":[{"Sid":"}]","Effect":"Allow","Action":"*","Resource":"*","Condition":{"Bool":{"aws:SecureTransport":"true"}}}]}"#;
        let t = statement_spans(tricky);
        assert_eq!(t.len(), 1);
        assert!(tricky[t[0].0.1 - 1..t[0].1.1 - 1].ends_with("}}}"));
    }

    #[test]
    fn levels() {
        let read = Some((s3_store::Permission::Read, Some(vec!["a".to_string()])));
        assert!(level_allows(&read, "s3:GetObject", "arn:aws:s3:::a/k"));
        assert!(!level_allows(&read, "s3:GetObject", "arn:aws:s3:::b/k"));
        assert!(!level_allows(&read, "s3:PutObject", "arn:aws:s3:::a/k"));
        let write = Some((s3_store::Permission::Write, None));
        assert!(level_allows(&write, "s3:PutObject", "arn:aws:s3:::b/k"));
        assert!(!level_allows(&write, "s3:PutBucketPolicy", "arn:aws:s3:::b"));
        assert!(!level_allows(&write, "iam:CreateUser", "*"));
        assert!(!level_allows(&None, "s3:GetObject", "*"));
        assert_eq!(account_of("arn:aws:iam::123456789012:root").as_deref(), Some("123456789012"));
        assert_eq!(account_of("123456789012").as_deref(), Some("123456789012"));
        assert_eq!(account_of("arn:aws:s3:::b"), None);
    }
}
