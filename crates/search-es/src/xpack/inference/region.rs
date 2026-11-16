//! The deployment's inference region policy (`_inference/_region_policy`): which regions or
//! geographies Elastic's hosted inference service may run this deployment's models in. roll
//! calls no hosted Elastic service, so the policy is kept and answered exactly as Elasticsearch
//! keeps it (one document of `.inference`, `RegionPolicyDoc`) and restricts nothing.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;

const FAMILY: &str = "inference.region_policy";
const ID: &str = "region_policy";

fn none() -> EsError {
    EsError::resource_not_found("No region policy is configured for this deployment")
}

/// Where the value of `region_policy` ends (its closing brace), for parse errors about it.
fn end_of_policy(ctx: &Ctx) -> (usize, usize) {
    let Some(src) = ctx.raw_json() else { return (1, 1) };
    let Some((line, col)) = locate(src, &[Seg::Key("region_policy")], Target::Value) else { return (1, 1) };
    let mut offset = 0;
    for _ in 1..line {
        offset += src[offset..].iter().position(|&b| b == b'\n').map_or(src.len() - offset, |p| p + 1);
    }
    let start = offset + col - 1;
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in src.iter().enumerate().skip(start) {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    let line_start = src[..i].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
                    return (line, i - line_start + 1);
                }
            }
            _ => {}
        }
    }
    (1, 1)
}

/// `PUT _inference/_region_policy`.
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if k != "region_policy" {
            return Err(crate::xpack::unknown_field(ctx, "put_region_policy_request", k));
        }
    }
    let Some(policy) = body.get("region_policy").and_then(Value::as_object).cloned() else {
        return Err(EsError::illegal_argument("Required [region_policy]"));
    };
    let parse_err = |reason: String| {
        let (l, c) = end_of_policy(ctx);
        EsError::x_content_parse(l, c, "[put_region_policy_request] failed to parse field [region_policy]").caused_by(EsError::illegal_argument(reason))
    };
    for k in policy.keys() {
        if k != "allowed_geos" && k != "allowed_regions" {
            return Err(parse_err(format!("[region_policy] unknown field [{k}]")));
        }
    }
    if policy.contains_key("allowed_geos") && policy.contains_key("allowed_regions") {
        return Err(parse_err("The following fields are not allowed together: [allowed_geos, allowed_regions] ".into()));
    }
    for k in ["allowed_geos", "allowed_regions"] {
        if policy.get(k).and_then(Value::as_array).is_some_and(|a| a.is_empty()) {
            return Err(EsError::action_request_validation(&[format!("[region_policy.{k}] must not be empty")]));
        }
    }
    let user = ctx.principal.get().map(|p| p.username.clone());
    let now = crate::xpack::entsearch::now_nanos_string();
    let now_ms = ctx.now_ms;
    let doc = ctx
        .cluster
        .update(move |st| {
            let mut doc = Map::new();
            doc.insert("region_policy".into(), Value::Obj(policy));
            match st.metadata.xpack.get(FAMILY, ID).and_then(Value::as_object) {
                None => {
                    doc.insert("created_at".into(), now.into());
                    if let Some(u) = user {
                        doc.insert("created_by".into(), u.into());
                    }
                }
                Some(old) => {
                    for k in ["created_at", "created_by"] {
                        if let Some(v) = old.get(k) {
                            doc.insert(k.into(), v.clone());
                        }
                    }
                    doc.insert("updated_at".into(), now.into());
                    if let Some(u) = user {
                        doc.insert("updated_by".into(), u.into());
                    }
                }
            }
            let doc = Value::Obj(doc);
            store::put(st, FAMILY, ID, doc.clone(), now_ms)?;
            Ok(doc)
        })
        .await?;
    Reply::ok(doc)
}

/// `GET _inference/_region_policy`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(ctx.state().metadata.xpack.get(FAMILY, ID).cloned().ok_or_else(none)?)
}

/// `DELETE _inference/_region_policy`.
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.cluster.update(|st| store::remove(st, FAMILY, ID).map(|_| ()).ok_or_else(none)).await?;
    Reply::ok(obj! { "acknowledged" => true })
}
