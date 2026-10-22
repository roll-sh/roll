//! The S3 Control API for access points and Object Lambda access points, on the S3 listener.
//!
//! SDKs reach it with a custom endpoint as `<account>.<endpoint host>` (the account id is a
//! host prefix) and always send `x-amz-account-id`; AWS's own host is
//! `<account>.s3-control.<region>.amazonaws.com`. Requests are recognised by their path
//! (`/v20180820/…`) with that header. The account must be the deployment's.
//!
//! Operations: Create/Get/Delete/ListAccessPoints, Put/Get/DeleteAccessPointPolicy,
//! GetAccessPointPolicyStatus, and their `…ForObjectLambda` counterparts plus
//! Get/PutAccessPointConfigurationForObjectLambda. Creating, deleting and changing need an
//! `admin` key covering the bucket; reading needs `read`.

use super::{ap_arn, olap_arn};
use crate::auth::Need;
use crate::ctx::{Ctx, read_small};
use crate::error::StoreResult;
use crate::response::{self, Reply};
use axum::body::Body;
use axum::http::{Method, StatusCode};
use s3_proto::http::percent_decode_utf8;
use s3_proto::xml::{Node, escape};
use s3_proto::{Code, S3Error};
use s3_store::{AccessPoint, AccessPointKind, NewAccessPoint, PublicAccessBlock, StoreError};

const NS: &str = "http://awss3control.amazonaws.com/doc/2018-08-20/";
const PREFIX: &str = "/v20180820/";

/// An S3 Control XML document.
struct Doc(String);

impl Doc {
    fn new(root: &str) -> Self {
        Self(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} xmlns=\"{NS}\">"))
    }
    fn open(&mut self, n: &str) -> &mut Self {
        self.0.push_str(&format!("<{n}>"));
        self
    }
    fn close(&mut self, n: &str) -> &mut Self {
        self.0.push_str(&format!("</{n}>"));
        self
    }
    fn el(&mut self, n: &str, v: &str) -> &mut Self {
        self.0.push_str(&format!("<{n}>{}</{n}>", escape(v)));
        self
    }
    fn raw(&mut self, s: &str) -> &mut Self {
        self.0.push_str(s);
        self
    }
    fn finish(mut self, root: &str) -> String {
        self.0.push_str(&format!("</{root}>"));
        self.0
    }
}

/// Whether the request is for the S3 Control API.
pub(crate) fn is_control(ctx: &Ctx) -> bool {
    ctx.raw_path.starts_with(PREFIX) && ctx.headers.contains_key("x-amz-account-id")
}

fn bool_el(d: &mut Doc, n: &str, v: bool) {
    d.el(n, if v { "true" } else { "false" });
}

fn block_xml(d: &mut Doc, b: &PublicAccessBlock) {
    d.open("PublicAccessBlockConfiguration");
    bool_el(d, "BlockPublicAcls", b.block_public_acls);
    bool_el(d, "IgnorePublicAcls", b.ignore_public_acls);
    bool_el(d, "BlockPublicPolicy", b.block_public_policy);
    bool_el(d, "RestrictPublicBuckets", b.restrict_public_buckets);
    d.close("PublicAccessBlockConfiguration");
}

fn parse_block(n: Option<&Node>) -> Result<PublicAccessBlock, S3Error> {
    let Some(n) = n else { return Ok(PublicAccessBlock::ALL) };
    let flag = |name: &str| -> Result<bool, S3Error> {
        match n.text_of(name).map(str::trim) {
            None => Ok(false),
            Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
            Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(S3Error::new(Code::MalformedXML)),
        }
    };
    Ok(PublicAccessBlock { block_public_acls: flag("BlockPublicAcls")?, ignore_public_acls: flag("IgnorePublicAcls")?, block_public_policy: flag("BlockPublicPolicy")?, restrict_public_buckets: flag("RestrictPublicBuckets")? })
}

async fn body_xml(ctx: &Ctx, body: Body, root: &str) -> Result<Node, S3Error> {
    let raw = read_small(ctx, body, s3_proto::xml::MAX_XML_BYTES).await?;
    let n = s3_proto::xml::parse(&raw)?;
    if n.name != root {
        return Err(S3Error::new(Code::MalformedXML));
    }
    Ok(n)
}

fn no_such(name: &str) -> S3Error {
    S3Error::msg(Code::NoSuchAccessPoint, format!("The specified accesspoint does not exist: {name}")).with("AccessPointName", name.to_string())
}

fn store_err(e: StoreError, name: &str) -> S3Error {
    match e {
        StoreError::NotFound(_) => no_such(name),
        StoreError::Conflict(m) if m.contains("already exists") => S3Error::new(Code::AccessPointAlreadyOwnedByYou),
        StoreError::Conflict(m) => S3Error::msg(Code::InvalidRequest, m),
        StoreError::InvalidArgument(m) => S3Error::msg(Code::InvalidRequest, m),
        e => crate::error::store_error(e),
    }
}

/// An access point name in the path, or the name inside an ARN given there.
fn name_of(seg: &str) -> Result<String, S3Error> {
    let s = percent_decode_utf8(seg)?;
    Ok(match s.rsplit_once("accesspoint/") {
        Some((_, n)) if s.starts_with("arn:") => n.to_string(),
        _ => s,
    })
}

fn kind_of(seg: &str) -> Option<AccessPointKind> {
    match seg {
        "accesspoint" => Some(AccessPointKind::Standard),
        "accesspointforobjectlambda" => Some(AccessPointKind::ObjectLambda),
        _ => None,
    }
}

struct Call<'a> {
    account: String,
    region: String,
    kind: AccessPointKind,
    name: Option<String>,
    sub: Option<&'a str>,
}

/// S3 Control's errors are wrapped: `<ErrorResponse><Error><Code>…</Code>…</Error></ErrorResponse>`.
pub(crate) async fn handle(ctx: &mut Ctx, body: Body) -> Reply {
    match route(ctx, body).await {
        Ok(r) => Ok(r),
        Err(e) => {
            let mut x = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ErrorResponse><Error><Code>{}</Code><Message>{}</Message>", e.code.as_str(), escape(&e.message));
            for (k, v) in &e.extra {
                x.push_str(&format!("<{k}>{}</{k}>", escape(v)));
            }
            x.push_str("</Error></ErrorResponse>");
            let mut r = response::xml(response::status(e.status()), x);
            for (name, value) in &e.headers {
                response::set(&mut r, name, value);
            }
            Ok(r)
        }
    }
}

async fn route(ctx: &mut Ctx, body: Body) -> Reply {
    let account = ctx.store().account_id();
    if ctx.h("x-amz-account-id").map(str::trim) != Some(account.as_str()) {
        return Err(S3Error::msg(Code::AccessDenied, "The account id isn't this deployment's"));
    }
    if ctx.auth.is_none() {
        return Err(S3Error::new(Code::AccessDenied));
    }
    let raw = ctx.raw_path.clone();
    let rest = &raw[PREFIX.len()..];
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let Some(kind) = segs.first().and_then(|s| kind_of(s)) else {
        return Err(S3Error::msg(Code::NotImplemented, format!("S3 Control's /{} isn't supported (only access points)", segs.first().unwrap_or(&""))));
    };
    let call = Call { account, region: ctx.server.cfg.region.clone(), kind, name: segs.get(1).map(|s| name_of(s)).transpose()?, sub: segs.get(2).copied() };
    if segs.len() > 3 {
        return Err(S3Error::new(Code::MethodNotAllowed));
    }
    match (ctx.method.clone(), call.name.is_some(), call.sub) {
        (Method::GET, false, None) => list(ctx, &call),
        (Method::PUT, true, None) => create(ctx, &call, body).await,
        (Method::GET, true, None) => get(ctx, &call),
        (Method::DELETE, true, None) => delete(ctx, &call).await,
        (Method::PUT, true, Some("policy")) => put_policy(ctx, &call, body).await,
        (Method::GET, true, Some("policy")) => get_policy(ctx, &call),
        (Method::DELETE, true, Some("policy")) => delete_policy(ctx, &call).await,
        (Method::GET, true, Some("policyStatus")) => policy_status(ctx, &call),
        (Method::GET, true, Some("configuration")) if kind == AccessPointKind::ObjectLambda => get_configuration(ctx, &call),
        (Method::PUT, true, Some("configuration")) if kind == AccessPointKind::ObjectLambda => put_configuration(ctx, &call, body).await,
        _ => Err(S3Error::new(Code::MethodNotAllowed)),
    }
}

impl Call<'_> {
    fn name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }

    fn arn(&self) -> String {
        match self.kind {
            AccessPointKind::Standard => ap_arn(&self.region, &self.account, self.name()),
            AccessPointKind::ObjectLambda => olap_arn(&self.region, &self.account, self.name()),
        }
    }

    fn find(&self, ctx: &Ctx) -> Result<AccessPoint, S3Error> {
        ctx.store().access_point(&self.account, self.kind, self.name()).s3()?.ok_or_else(|| no_such(self.name()))
    }
}

/// The bucket an access point is on (an Object Lambda one's, through its supporting one).
fn bucket_of(ctx: &Ctx, call: &Call, ap: &AccessPoint) -> Result<String, S3Error> {
    Ok(match ap.kind {
        AccessPointKind::Standard => ap.bucket.clone(),
        AccessPointKind::ObjectLambda => ctx.store().access_point(&call.account, AccessPointKind::Standard, &ap.bucket).s3()?.map(|s| s.bucket).unwrap_or_default(),
    })
}

/// The request's key may do `need` on the access point's bucket, and its session policies
/// allow `action`.
fn permit(ctx: &Ctx, bucket: &str, need: Need, action: &str) -> Result<(), S3Error> {
    let k = &ctx.auth.as_ref().ok_or_else(|| S3Error::new(Code::AccessDenied))?.key;
    let ok = match need {
        Need::Read => k.can_read(bucket),
        Need::Write => k.can_write(bucket),
        Need::Admin => k.is_admin() && k.covers(bucket),
    };
    ctx.require_identity(action, Some(bucket), ok)
}

fn action(kind: AccessPointKind, base: &str) -> String {
    match kind {
        AccessPointKind::Standard => format!("s3:{base}"),
        AccessPointKind::ObjectLambda => format!("s3:{base}ForObjectLambda"),
    }
}

async fn create(ctx: &mut Ctx, call: &Call<'_>, body: Body) -> Reply {
    let name = call.name().to_string();
    match call.kind {
        AccessPointKind::Standard => {
            let n = body_xml(ctx, body, "CreateAccessPointRequest").await?;
            let bucket = n.text_of("Bucket").map(str::trim).filter(|b| !b.is_empty()).ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Bucket is required"))?;
            let bucket = bucket.strip_prefix("arn:aws:s3:::").unwrap_or(bucket).to_string();
            if let Some(a) = n.text_of("BucketAccountId").map(str::trim)
                && a != call.account
            {
                return Err(S3Error::msg(Code::InvalidRequest, "BucketAccountId must be the bucket owner's account (this deployment's)"));
            }
            permit(ctx, &bucket, Need::Admin, "s3:CreateAccessPoint")?;
            let vpc_id = n.child("VpcConfiguration").and_then(|v| v.text_of("VpcId")).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let block = parse_block(n.child("PublicAccessBlockConfiguration"))?;
            let ap = ctx.store().create_access_point(AccessPointKind::Standard, NewAccessPoint { account: call.account.clone(), name: name.clone(), bucket, vpc_id, block, config: None }).await.map_err(|e| store_err(e, &name))?;
            let mut d = Doc::new("CreateAccessPointResult");
            d.el("AccessPointArn", &call.arn()).el("Alias", &ap.alias);
            response::ok_xml(d.finish("CreateAccessPointResult"))
        }
        AccessPointKind::ObjectLambda => {
            let n = body_xml(ctx, body, "CreateAccessPointForObjectLambdaRequest").await?;
            let cfg = super::lambda::OlapConfig::parse(n.child("Configuration").ok_or_else(|| S3Error::new(Code::MalformedXML))?)?;
            let supporting = ctx.store().access_point(&call.account, AccessPointKind::Standard, &cfg.supporting).s3()?.ok_or_else(|| S3Error::msg(Code::InvalidRequest, format!("The supporting access point {} doesn't exist", cfg.supporting)))?;
            permit(ctx, &supporting.bucket, Need::Admin, "s3:CreateAccessPointForObjectLambda")?;
            let config = serde_json::to_string(&cfg).map_err(|_| S3Error::new(Code::InternalError))?;
            let ap = ctx
                .store()
                .create_access_point(AccessPointKind::ObjectLambda, NewAccessPoint { account: call.account.clone(), name: name.clone(), bucket: cfg.supporting.clone(), vpc_id: None, block: PublicAccessBlock::ALL, config: Some(config) })
                .await
                .map_err(|e| store_err(e, &name))?;
            let mut d = Doc::new("CreateAccessPointForObjectLambdaResult");
            d.el("ObjectLambdaAccessPointArn", &call.arn()).open("Alias").el("Value", &ap.alias).el("Status", "READY").close("Alias");
            response::ok_xml(d.finish("CreateAccessPointForObjectLambdaResult"))
        }
    }
}

fn endpoints(ctx: &Ctx, call: &Call, ap: &AccessPoint) -> String {
    let host = ctx.h("host").unwrap_or_default();
    let base = host.split_once('.').map_or(host, |(_, rest)| rest);
    let label = format!("{}-{}", ap.name, call.account);
    format!("<Endpoints><entry><key>ipv4</key><value>{}</value></entry></Endpoints>", escape(&format!("{label}.{base}")))
}

fn get(ctx: &Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Read, &action(call.kind, "GetAccessPoint"))?;
    let created = s3_proto::time::iso8601_ms(ap.created_at);
    match call.kind {
        AccessPointKind::Standard => {
            let mut d = Doc::new("GetAccessPointResult");
            d.el("Name", &ap.name).el("Bucket", &ap.bucket).el("NetworkOrigin", &ap.network_origin);
            if let Some(v) = &ap.vpc_id {
                d.open("VpcConfiguration").el("VpcId", v).close("VpcConfiguration");
            }
            block_xml(&mut d, &ap.block);
            d.el("CreationDate", &created).el("Alias", &ap.alias).el("AccessPointArn", &call.arn());
            d.raw(&endpoints(ctx, call, &ap));
            d.el("BucketAccountId", &call.account);
            response::ok_xml(d.finish("GetAccessPointResult"))
        }
        AccessPointKind::ObjectLambda => {
            let mut d = Doc::new("GetAccessPointForObjectLambdaResult");
            d.el("Name", &ap.name);
            block_xml(&mut d, &ap.block);
            d.el("CreationDate", &created).open("Alias").el("Value", &ap.alias).el("Status", "READY").close("Alias");
            response::ok_xml(d.finish("GetAccessPointForObjectLambdaResult"))
        }
    }
}

fn list(ctx: &Ctx, call: &Call<'_>) -> Reply {
    let k = ctx.auth.as_ref().map(|a| a.key.clone()).ok_or_else(|| S3Error::new(Code::AccessDenied))?;
    ctx.require_identity(&action(call.kind, "ListAccessPoints"), None, true)?;
    let max = match ctx.q("maxResults") {
        None => 1000,
        Some(m) => m.parse::<usize>().ok().filter(|n| (1..=1000).contains(n)).ok_or_else(|| S3Error::msg(Code::InvalidRequest, "maxResults must be 1-1000"))?,
    };
    let after = ctx.q("nextToken").map(str::to_string);
    let bucket = ctx.q("bucket").filter(|b| !b.is_empty()).map(|b| b.strip_prefix("arn:aws:s3:::").unwrap_or(b).to_string());
    let filter_bucket = if call.kind == AccessPointKind::Standard { bucket.as_deref() } else { None };
    // Pages in name order; only access points on buckets the key can read.
    let mut out = Vec::new();
    let mut cursor = after.clone();
    let mut more = false;
    loop {
        let page = ctx.store().access_points(&call.account, call.kind, filter_bucket, cursor.as_deref(), 1000).s3()?;
        let full = page.len() == 1000;
        for ap in page {
            cursor = Some(ap.name.clone());
            let b = bucket_of(ctx, call, &ap)?;
            if k.can_read(&b) {
                if out.len() == max {
                    more = true;
                    break;
                }
                out.push(ap);
            }
        }
        if more || !full {
            break;
        }
    }
    let region = &call.region;
    match call.kind {
        AccessPointKind::Standard => {
            let mut d = Doc::new("ListAccessPointsResult");
            d.open("AccessPointList");
            for ap in &out {
                d.open("AccessPoint").el("Name", &ap.name).el("NetworkOrigin", &ap.network_origin);
                if let Some(v) = &ap.vpc_id {
                    d.open("VpcConfiguration").el("VpcId", v).close("VpcConfiguration");
                }
                d.el("Bucket", &ap.bucket).el("AccessPointArn", &ap_arn(region, &call.account, &ap.name)).el("Alias", &ap.alias).el("BucketAccountId", &call.account).close("AccessPoint");
            }
            d.close("AccessPointList");
            if more && let Some(last) = out.last() {
                d.el("NextToken", &last.name);
            }
            response::ok_xml(d.finish("ListAccessPointsResult"))
        }
        AccessPointKind::ObjectLambda => {
            let mut d = Doc::new("ListAccessPointsForObjectLambdaResult");
            d.open("ObjectLambdaAccessPointList");
            for ap in &out {
                d.open("ObjectLambdaAccessPoint").el("Name", &ap.name).el("ObjectLambdaAccessPointArn", &olap_arn(region, &call.account, &ap.name)).open("Alias").el("Value", &ap.alias).el("Status", "READY").close("Alias").close("ObjectLambdaAccessPoint");
            }
            d.close("ObjectLambdaAccessPointList");
            if more && let Some(last) = out.last() {
                d.el("NextToken", &last.name);
            }
            response::ok_xml(d.finish("ListAccessPointsForObjectLambdaResult"))
        }
    }
}

async fn delete(ctx: &mut Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Admin, &action(call.kind, "DeleteAccessPoint"))?;
    ctx.store().delete_access_point(&call.account, call.kind, call.name()).await.map_err(|e| store_err(e, call.name()))?;
    Ok(response::empty(StatusCode::NO_CONTENT))
}

/// The public access block in force for an access point's policy: its own, its bucket's and
/// the deployment's.
fn blocks_public_policy(ctx: &Ctx, bucket: &str, ap: &AccessPoint) -> bool {
    let bucket_block = ctx.store().bucket(bucket).ok().and_then(|b| ctx.store().public_access_block(&b.id).ok()).map(|(_, eff)| eff).unwrap_or_default();
    ap.block.block_public_policy || bucket_block.block_public_policy || ctx.store().cfg.block_public_access
}

async fn put_policy(ctx: &mut Ctx, call: &Call<'_>, body: Body) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Admin, &action(call.kind, "PutAccessPointPolicy"))?;
    let root = match call.kind {
        AccessPointKind::Standard => "PutAccessPointPolicyRequest",
        AccessPointKind::ObjectLambda => "PutAccessPointPolicyForObjectLambdaRequest",
    };
    let n = body_xml(ctx, body, root).await?;
    let doc = n.text_of("Policy").map(str::trim).filter(|p| !p.is_empty()).ok_or_else(|| S3Error::msg(Code::MalformedPolicy, "Policy is required"))?.to_string();
    let public = super::validate_policy(&doc, &call.arn())?;
    crate::principals::bind(ctx.store(), &doc).map_err(|e| match e {
        Some(_) => S3Error::msg(Code::MalformedPolicy, crate::principals::INVALID),
        None => S3Error::new(Code::InternalError),
    })?;
    if public && blocks_public_policy(ctx, &bucket, &ap) {
        return Err(S3Error::msg(Code::AccessDenied, "Access Denied: public access is blocked (BlockPublicPolicy)"));
    }
    ctx.store().set_access_point_policy(&call.account, call.kind, call.name(), Some(&doc)).await.map_err(|e| store_err(e, call.name()))?;
    Ok(response::empty(StatusCode::OK))
}

fn get_policy(ctx: &Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Read, &action(call.kind, "GetAccessPointPolicy"))?;
    let p = ap.policy.ok_or_else(|| S3Error::new(Code::NoSuchAccessPointPolicy).with("AccessPointName", ap.name.clone()))?;
    let root = match call.kind {
        AccessPointKind::Standard => "GetAccessPointPolicyResult",
        AccessPointKind::ObjectLambda => "GetAccessPointPolicyForObjectLambdaResult",
    };
    let mut d = Doc::new(root);
    d.el("Policy", &p);
    response::ok_xml(d.finish(root))
}

async fn delete_policy(ctx: &mut Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Admin, &action(call.kind, "DeleteAccessPointPolicy"))?;
    ctx.store().set_access_point_policy(&call.account, call.kind, call.name(), None).await.map_err(|e| store_err(e, call.name()))?;
    Ok(response::empty(StatusCode::NO_CONTENT))
}

fn policy_status(ctx: &Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Read, &action(call.kind, "GetAccessPointPolicyStatus"))?;
    let public = match &ap.policy {
        Some(p) => super::validate_policy(p, &call.arn()).unwrap_or(false),
        None => false,
    };
    let root = match call.kind {
        AccessPointKind::Standard => "GetAccessPointPolicyStatusResult",
        AccessPointKind::ObjectLambda => "GetAccessPointPolicyStatusForObjectLambdaResult",
    };
    let mut d = Doc::new(root);
    d.open("PolicyStatus");
    bool_el(&mut d, "IsPublic", public);
    d.close("PolicyStatus");
    response::ok_xml(d.finish(root))
}

fn get_configuration(ctx: &Ctx, call: &Call<'_>) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Read, "s3:GetAccessPointConfigurationForObjectLambda")?;
    let cfg = super::lambda::OlapConfig::of(&ap)?;
    let mut d = Doc::new("GetAccessPointConfigurationForObjectLambdaResult");
    d.raw(&cfg.render(&call.region, &call.account));
    response::ok_xml(d.finish("GetAccessPointConfigurationForObjectLambdaResult"))
}

async fn put_configuration(ctx: &mut Ctx, call: &Call<'_>, body: Body) -> Reply {
    let ap = call.find(ctx)?;
    let bucket = bucket_of(ctx, call, &ap)?;
    permit(ctx, &bucket, Need::Admin, "s3:PutAccessPointConfigurationForObjectLambda")?;
    let n = body_xml(ctx, body, "PutAccessPointConfigurationForObjectLambdaRequest").await?;
    let cfg = super::lambda::OlapConfig::parse(n.child("Configuration").ok_or_else(|| S3Error::new(Code::MalformedXML))?)?;
    if cfg.supporting != ap.bucket {
        return Err(S3Error::msg(Code::InvalidRequest, "The supporting access point of an Object Lambda access point can't change"));
    }
    let config = serde_json::to_string(&cfg).map_err(|_| S3Error::new(Code::InternalError))?;
    ctx.store().set_access_point_config(&call.account, call.kind, call.name(), &config).await.map_err(|e| store_err(e, call.name()))?;
    Ok(response::empty(StatusCode::OK))
}
