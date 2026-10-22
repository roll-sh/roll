//! Bucket configuration subresources: tagging, policy and policy status, public access block,
//! ownership controls, ACL, encryption, website, logging, accelerate, request payment,
//! replication, notifications, and the analytics, metrics, inventory and intelligent-tiering
//! configurations. Values are stored with `Store::set_bucket_config` (durable metadata writes).
//!
//! Reading configuration needs `read` on the bucket and changing it `admin`, like CORS and
//! lifecycle, unless a bucket policy says otherwise.

use crate::auth::Need;
use crate::ctx::{Ctx, no_content, read_small};
use crate::error::StoreResult;
use crate::policy::Policy;
use crate::response::{self, Reply};
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_proto::xml::{self, Node, escape};
use s3_proto::{Code, S3Error, XMLNS};
use s3_store::config::kind;
use s3_store::{Bucket, Class, PublicAccessBlock, StoreError};
use serde::{Deserialize, Serialize};

/// The bucket subresources handled here.
const SUBRESOURCES: [&str; 17] = [
    "tagging",
    "policyStatus",
    "policy",
    "publicAccessBlock",
    "ownershipControls",
    "acl",
    "encryption",
    "website",
    "logging",
    "accelerate",
    "requestPayment",
    "replication",
    "notification",
    "analytics",
    "metrics",
    "inventory",
    "intelligent-tiering",
];

/// Whether this is a bucket configuration request handled here.
pub(crate) fn handles(ctx: &Ctx) -> bool {
    ctx.key.is_none() && SUBRESOURCES.iter().any(|s| ctx.has(s))
}

/// A small XML document writer (S3's namespace on the root).
pub(crate) struct Doc(String);

impl Doc {
    pub fn new(root: &str) -> Self {
        Self(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} xmlns=\"{XMLNS}\">"))
    }

    pub fn open(&mut self, name: &str) -> &mut Self {
        self.0.push_str(&format!("<{name}>"));
        self
    }

    pub fn close(&mut self, name: &str) -> &mut Self {
        self.0.push_str(&format!("</{name}>"));
        self
    }

    pub fn el(&mut self, name: &str, text: &str) -> &mut Self {
        self.0.push_str(&format!("<{name}>{}</{name}>", escape(text)));
        self
    }

    pub fn opt(&mut self, name: &str, text: Option<&str>) -> &mut Self {
        if let Some(t) = text {
            self.el(name, t);
        }
        self
    }

    pub fn raw(&mut self, s: &str) -> &mut Self {
        self.0.push_str(s);
        self
    }

    pub fn finish(mut self, root: &str) -> String {
        self.0.push_str(&format!("</{root}>"));
        self.0
    }
}

/// An empty document: `<Root xmlns="…"/>`.
pub(crate) fn empty_doc(root: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} xmlns=\"{XMLNS}\"/>")
}

/// A parsed element back to XML (no namespace; text only on leaves).
pub(crate) fn node_xml(n: &Node) -> String {
    let mut s = String::new();
    write_node(&mut s, n);
    s
}

fn write_node(s: &mut String, n: &Node) {
    if n.children.is_empty() {
        s.push_str(&format!("<{0}>{1}</{0}>", n.name, escape(&n.text)));
        return;
    }
    s.push_str(&format!("<{}>", n.name));
    for c in &n.children {
        write_node(s, c);
    }
    s.push_str(&format!("</{}>", n.name));
}

pub(crate) fn malformed() -> S3Error {
    S3Error::new(Code::MalformedXML)
}

/// Read and parse an XML body whose root must be `root`.
pub(crate) async fn read_xml(ctx: &Ctx, body: Body, root: &str) -> Result<Node, S3Error> {
    let raw = read_small(ctx, body, xml::MAX_XML_BYTES).await?;
    let n = xml::parse(&raw)?;
    if n.name != root {
        return Err(malformed());
    }
    Ok(n)
}

/// `true`/`false` (S3 accepts any case).
pub(crate) fn parse_bool(n: &Node, name: &str) -> Result<Option<bool>, S3Error> {
    match n.text_of(name).map(str::trim) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(_) => Err(malformed()),
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// Reading configuration: `read` on the bucket, metered as class B.
pub(crate) fn reading(ctx: &Ctx, bucket: &Bucket) -> Result<(), S3Error> {
    ctx.require(&bucket.name, Need::Read)?;
    ctx.meter(&bucket.id, Class::B);
    Ok(())
}

/// Changing configuration: `admin` on the bucket, metered as class A (deletes are free).
pub(crate) fn writing(ctx: &Ctx, bucket: &Bucket) -> Result<(), S3Error> {
    ctx.require(&bucket.name, Need::Admin)?;
    ctx.meter(&bucket.id, if ctx.method == Method::DELETE { Class::Free } else { Class::A });
    Ok(())
}

/// Store a one-per-bucket setting (`None` removes it).
pub(crate) async fn save(ctx: &Ctx, bucket: &Bucket, kind: &str, value: Option<&str>) -> Result<(), S3Error> {
    ctx.store().set_bucket_config(&bucket.name, kind, "", value, None).await.s3()?;
    ctx.forget_access(&bucket.name);
    Ok(())
}

fn ok() -> Reply {
    Ok(response::empty(StatusCode::OK))
}

pub(crate) async fn route(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    let sub = SUBRESOURCES.iter().copied().find(|s| ctx.has(s)).unwrap_or_default();
    let m = ctx.method.clone();
    if m != Method::GET && m != Method::PUT && m != Method::DELETE {
        return Err(S3Error::new(Code::MethodNotAllowed));
    }
    let (get, put) = (m == Method::GET, m == Method::PUT);
    match sub {
        "tagging" if get => get_tagging(ctx, bucket),
        "tagging" if put => put_tagging(ctx, bucket, body).await,
        "tagging" => {
            writing(ctx, bucket)?;
            save(ctx, bucket, kind::TAGGING, None).await?;
            no_content()
        }
        "policy" if get => get_policy(ctx, bucket),
        "policy" if put => put_policy(ctx, bucket, body).await,
        "policy" => {
            writing(ctx, bucket)?;
            save(ctx, bucket, kind::POLICY, None).await?;
            no_content()
        }
        "policyStatus" if get => policy_status(ctx, bucket),
        "publicAccessBlock" if get => get_public_access_block(ctx, bucket),
        "publicAccessBlock" if put => put_public_access_block(ctx, bucket, body).await,
        "publicAccessBlock" => {
            writing(ctx, bucket)?;
            save(ctx, bucket, kind::PUBLIC_ACCESS_BLOCK, None).await?;
            no_content()
        }
        "ownershipControls" if get => get_ownership(ctx, bucket),
        "ownershipControls" if put => put_ownership(ctx, bucket, body).await,
        "ownershipControls" => {
            writing(ctx, bucket)?;
            // Deleted controls aren't the default: the bucket then has none (as on S3).
            save(ctx, bucket, kind::OWNERSHIP, Some("")).await?;
            no_content()
        }
        "acl" if get => crate::acl::get_bucket_acl(ctx, bucket),
        "acl" if put => crate::acl::put_bucket_acl(ctx, bucket, body).await,
        "encryption" if get => get_encryption(ctx, bucket),
        "encryption" if put => put_encryption(ctx, bucket, body).await,
        "encryption" => {
            writing(ctx, bucket)?;
            save(ctx, bucket, kind::ENCRYPTION, None).await?;
            no_content()
        }
        "website" if get => crate::website::get_config(ctx, bucket),
        "website" if put => crate::website::put_config(ctx, bucket, body).await,
        "website" => {
            writing(ctx, bucket)?;
            save(ctx, bucket, kind::WEBSITE, None).await?;
            no_content()
        }
        "logging" if get => crate::logging::get_config(ctx, bucket),
        "logging" if put => crate::logging::put_config(ctx, bucket, body).await,
        "accelerate" if get => get_accelerate(ctx, bucket),
        "accelerate" if put => put_accelerate(ctx, bucket, body).await,
        "requestPayment" if get => {
            reading(ctx, bucket)?;
            response::ok_xml(Doc::new("RequestPaymentConfiguration").el("Payer", "BucketOwner").clone_finish("RequestPaymentConfiguration"))
        }
        "requestPayment" if put => put_request_payment(ctx, bucket, body).await,
        "replication" if get => {
            reading(ctx, bucket)?;
            Err(S3Error::new(Code::ReplicationConfigurationNotFoundError).with("BucketName", bucket.name.clone()))
        }
        "replication" => {
            writing(ctx, bucket)?;
            Err(S3Error::msg(Code::NotImplemented, "Replication is not supported yet; the backing bucket can be replicated by the operator."))
        }
        "notification" if get => {
            reading(ctx, bucket)?;
            response::ok_xml(empty_doc("NotificationConfiguration"))
        }
        "notification" if put => put_notification(ctx, bucket, body).await,
        "analytics" | "metrics" | "inventory" | "intelligent-tiering" => {
            let spec = IdConfigSpec::of(sub);
            if get && ctx.q("id").is_none() {
                list_id_configs(ctx, bucket, spec)
            } else if get {
                get_id_config(ctx, bucket, spec)
            } else if put {
                put_id_config(ctx, bucket, spec, body).await
            } else {
                delete_id_config(ctx, bucket, spec).await
            }
        }
        _ => Err(S3Error::new(Code::MethodNotAllowed)),
    }
}

impl Doc {
    /// `finish` through a `&mut` chain.
    fn clone_finish(&mut self, root: &str) -> String {
        std::mem::replace(self, Doc(String::new())).finish(root)
    }
}

// ---- Tagging ----

fn get_tagging(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let tags = ctx.store().bucket_tags(&bucket.name).s3()?;
    if tags.is_empty() {
        return Err(S3Error::new(Code::NoSuchTagSet).with("BucketName", bucket.name.clone()));
    }
    let tags: Vec<(String, String)> = tags.into_iter().collect();
    response::ok_xml(xml::tagging(&tags))
}

async fn put_tagging(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "Tagging").await?;
    let set = n.child("TagSet").ok_or_else(malformed)?;
    let mut tags = Vec::new();
    for t in set.all("Tag") {
        tags.push((t.text_of("Key").ok_or_else(malformed)?.to_string(), t.text_of("Value").ok_or_else(malformed)?.to_string()));
    }
    s3_store::config::validate_bucket_tags(tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(|e| match e {
        StoreError::InvalidArgument(m) => S3Error::msg(Code::InvalidTag, m),
        e => crate::error::store_error(e),
    })?;
    let map = tags.into_iter().collect();
    ctx.store().set_bucket_tags(&bucket.name, &map).await.s3()?;
    no_content()
}

// ---- Policy ----

fn get_policy(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let a = ctx.access(&bucket.name)?;
    let p = a.settings.get(kind::POLICY).ok_or_else(|| S3Error::new(Code::NoSuchBucketPolicy).with("BucketName", bucket.name.clone()))?;
    let mut r = response::empty(StatusCode::OK);
    *r.body_mut() = Body::from(p.clone());
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(r)
}

async fn put_policy(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let raw = read_small(ctx, body, crate::policy::MAX_POLICY_BYTES + 1).await.map_err(|e| {
        if e.code == Code::MaxMessageLengthExceeded { S3Error::msg(Code::MalformedPolicy, "Policy exceeds the maximum allowed document size.") } else { e }
    })?;
    let policy = Policy::parse(&raw, &bucket.name)?;
    let a = ctx.access(&bucket.name)?;
    if policy.is_public() && a.block.block_public_policy {
        return Err(S3Error::msg(Code::AccessDenied, "Access Denied: public policies are blocked by the BlockPublicPolicy setting"));
    }
    let text = String::from_utf8(raw.to_vec()).map_err(|_| S3Error::new(Code::MalformedPolicy))?;
    save(ctx, bucket, kind::POLICY, Some(&text)).await?;
    no_content()
}

fn policy_status(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let a = ctx.access(&bucket.name)?;
    if !a.settings.contains_key(kind::POLICY) && !bucket.public {
        return Err(S3Error::new(Code::NoSuchBucketPolicy).with("BucketName", bucket.name.clone()));
    }
    response::ok_xml(Doc::new("PolicyStatus").el("IsPublic", bool_str(a.is_public())).clone_finish("PolicyStatus"))
}

// ---- Public access block ----

fn get_public_access_block(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let (own, _) = ctx.store().public_access_block(&bucket.id).s3()?;
    let b = own.ok_or_else(|| S3Error::new(Code::NoSuchPublicAccessBlockConfiguration).with("BucketName", bucket.name.clone()))?;
    let mut d = Doc::new("PublicAccessBlockConfiguration");
    d.el("BlockPublicAcls", bool_str(b.block_public_acls))
        .el("IgnorePublicAcls", bool_str(b.ignore_public_acls))
        .el("BlockPublicPolicy", bool_str(b.block_public_policy))
        .el("RestrictPublicBuckets", bool_str(b.restrict_public_buckets));
    response::ok_xml(d.finish("PublicAccessBlockConfiguration"))
}

async fn put_public_access_block(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "PublicAccessBlockConfiguration").await?;
    let b = PublicAccessBlock {
        block_public_acls: parse_bool(&n, "BlockPublicAcls")?.unwrap_or(false),
        ignore_public_acls: parse_bool(&n, "IgnorePublicAcls")?.unwrap_or(false),
        block_public_policy: parse_bool(&n, "BlockPublicPolicy")?.unwrap_or(false),
        restrict_public_buckets: parse_bool(&n, "RestrictPublicBuckets")?.unwrap_or(false),
    };
    let v = serde_json::to_string(&b).map_err(|_| S3Error::new(Code::InternalError))?;
    save(ctx, bucket, kind::PUBLIC_ACCESS_BLOCK, Some(&v)).await?;
    ok()
}

// ---- Ownership controls ----

fn get_ownership(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let a = ctx.access(&bucket.name)?;
    let o = a.ownership.ok_or_else(|| S3Error::new(Code::OwnershipControlsNotFoundError).with("BucketName", bucket.name.clone()))?;
    response::ok_xml(Doc::new("OwnershipControls").open("Rule").el("ObjectOwnership", o.as_str()).close("Rule").clone_finish("OwnershipControls"))
}

async fn put_ownership(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "OwnershipControls").await?;
    let rules: Vec<&Node> = n.all("Rule").collect();
    if rules.len() != 1 {
        return Err(malformed());
    }
    let o = rules[0].text_of("ObjectOwnership").and_then(crate::acl::Ownership::parse).ok_or_else(malformed)?;
    if o == crate::acl::Ownership::BucketOwnerEnforced && !ctx.access(&bucket.name)?.bucket_acl.owner_only() {
        return Err(S3Error::msg(Code::InvalidBucketAclWithObjectOwnership, "Bucket cannot have ACLs set with ObjectOwnership's BucketOwnerEnforced setting"));
    }
    save(ctx, bucket, kind::OWNERSHIP, Some(o.as_str())).await?;
    ok()
}

// ---- Encryption ----

/// A bucket's default encryption rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptionRule {
    algorithm: String,
    kms_key_id: Option<String>,
    bucket_key_enabled: bool,
}

/// The bucket's configured default encryption: `(algorithm, KMS key id, bucket key enabled)`,
/// with algorithm `AES256`, `aws:kms` or `aws:kms:dsse`. `None` when nothing is configured,
/// which means SSE-S3 (`AES256`): data is encrypted at rest by the backing bucket. For the
/// SSE-KMS work, which makes new objects honour a KMS default.
#[allow(dead_code)]
pub(crate) fn default_encryption(ctx: &Ctx, bucket: &Bucket) -> Option<(String, Option<String>, bool)> {
    default_encryption_of(ctx.store(), bucket)
}

/// [`default_encryption`] for writes without a request context (the control plane's uploads).
pub(crate) fn default_encryption_of(store: &s3_store::Store, bucket: &Bucket) -> Option<(String, Option<String>, bool)> {
    let v = store.bucket_config(&bucket.name, kind::ENCRYPTION, "").ok()??;
    let r: EncryptionRule = serde_json::from_str(&v).ok()?;
    Some((r.algorithm, r.kms_key_id, r.bucket_key_enabled))
}

fn get_encryption(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let a = ctx.access(&bucket.name)?;
    let r = a.settings.get(kind::ENCRYPTION).and_then(|v| serde_json::from_str::<EncryptionRule>(v).ok()).unwrap_or(EncryptionRule { algorithm: "AES256".into(), kms_key_id: None, bucket_key_enabled: false });
    let mut d = Doc::new("ServerSideEncryptionConfiguration");
    d.open("Rule").open("ApplyServerSideEncryptionByDefault").el("SSEAlgorithm", &r.algorithm).opt("KMSMasterKeyID", r.kms_key_id.as_deref()).close("ApplyServerSideEncryptionByDefault");
    d.el("BucketKeyEnabled", bool_str(r.bucket_key_enabled)).close("Rule");
    response::ok_xml(d.finish("ServerSideEncryptionConfiguration"))
}

async fn put_encryption(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "ServerSideEncryptionConfiguration").await?;
    let rules: Vec<&Node> = n.all("Rule").collect();
    if rules.len() != 1 {
        return Err(malformed());
    }
    let by_default = rules[0].child("ApplyServerSideEncryptionByDefault").ok_or_else(malformed)?;
    let algorithm = by_default.text_of("SSEAlgorithm").map(str::trim).ok_or_else(malformed)?.to_string();
    if !matches!(algorithm.as_str(), "AES256" | "aws:kms" | "aws:kms:dsse") {
        return Err(malformed());
    }
    let kms_key_id = by_default.text_of("KMSMasterKeyID").map(str::trim).filter(|k| !k.is_empty()).map(str::to_string);
    if kms_key_id.is_some() && algorithm == "AES256" {
        return Err(S3Error::msg(Code::InvalidArgument, "a KMSMasterKeyID is not applicable if the default sse algorithm is not aws:kms or aws:kms:dsse"));
    }
    let bucket_key_enabled = parse_bool(rules[0], "BucketKeyEnabled")?.unwrap_or(false);
    let r = EncryptionRule { algorithm, kms_key_id, bucket_key_enabled };
    let v = serde_json::to_string(&r).map_err(|_| S3Error::new(Code::InternalError))?;
    save(ctx, bucket, kind::ENCRYPTION, Some(&v)).await?;
    ok()
}

// ---- Accelerate, request payment, notifications ----

fn get_accelerate(ctx: &mut Ctx, bucket: &Bucket) -> Reply {
    reading(ctx, bucket)?;
    let a = ctx.access(&bucket.name)?;
    match a.settings.get(kind::ACCELERATE) {
        Some(s) => response::ok_xml(Doc::new("AccelerateConfiguration").el("Status", s).clone_finish("AccelerateConfiguration")),
        None => response::ok_xml(empty_doc("AccelerateConfiguration")),
    }
}

/// Stored and returned; a no-op, since there's one endpoint.
async fn put_accelerate(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "AccelerateConfiguration").await?;
    let status = n.text_of("Status").map(str::trim).filter(|s| *s == "Enabled" || *s == "Suspended").ok_or_else(malformed)?;
    if bucket.name.contains('.') {
        return Err(S3Error::msg(Code::InvalidRequest, "S3 Transfer Acceleration is not supported for buckets with periods (.) in their names"));
    }
    save(ctx, bucket, kind::ACCELERATE, Some(status)).await?;
    ok()
}

async fn put_request_payment(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "RequestPaymentConfiguration").await?;
    match n.text_of("Payer").map(str::trim) {
        Some("BucketOwner") => ok(),
        Some("Requester") => Err(S3Error::msg(Code::NotImplemented, "Requester Pays is not supported: the bucket owner pays for requests and egress.")),
        _ => Err(malformed()),
    }
}

async fn put_notification(ctx: &mut Ctx, bucket: &Bucket, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let n = read_xml(ctx, body, "NotificationConfiguration").await?;
    if !n.children.is_empty() {
        return Err(S3Error::msg(Code::NotImplemented, "Event notifications are not supported yet; only an empty NotificationConfiguration is accepted."));
    }
    ok()
}

// ---- Analytics, metrics, inventory, intelligent-tiering ----

#[derive(Clone, Copy)]
struct IdConfigSpec {
    kind: &'static str,
    root: &'static str,
    list_root: &'static str,
    validate: fn(&Node) -> Result<(), S3Error>,
}

impl IdConfigSpec {
    fn of(sub: &str) -> Self {
        match sub {
            "analytics" => Self { kind: kind::ANALYTICS, root: "AnalyticsConfiguration", list_root: "ListBucketAnalyticsConfigurationResult", validate: validate_analytics },
            "metrics" => Self { kind: kind::METRICS, root: "MetricsConfiguration", list_root: "ListMetricsConfigurationsResult", validate: validate_filter_only },
            "inventory" => Self { kind: kind::INVENTORY, root: "InventoryConfiguration", list_root: "ListInventoryConfigurationsResult", validate: validate_inventory },
            _ => Self {
                kind: kind::INTELLIGENT_TIERING,
                root: "IntelligentTieringConfiguration",
                list_root: "ListBucketIntelligentTieringConfigurationsOutput",
                validate: validate_intelligent_tiering,
            },
        }
    }
}

/// At most this many configurations of a kind per bucket (S3's limit).
const MAX_ID_CONFIGS: usize = 1000;
/// Configurations per list page.
const LIST_PAGE: usize = 100;

fn config_id(ctx: &Ctx) -> Result<String, S3Error> {
    let id = ctx.q("id").unwrap_or_default();
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        return Err(S3Error::msg(Code::InvalidArgument, "The configuration ID is not valid: 1-64 letters, digits, '-', '_' or '.'").with("ArgumentName", "id").with("ArgumentValue", id.to_string()));
    }
    Ok(id.to_string())
}

fn no_such_configuration() -> S3Error {
    S3Error::new(Code::NoSuchConfiguration)
}

fn get_id_config(ctx: &mut Ctx, bucket: &Bucket, spec: IdConfigSpec) -> Reply {
    reading(ctx, bucket)?;
    let id = config_id(ctx)?;
    let v = ctx.store().bucket_config(&bucket.name, spec.kind, &id).s3()?.ok_or_else(no_such_configuration)?;
    response::ok_xml(with_namespace(&v, spec.root))
}

/// A stored `<Root>…</Root>` with S3's namespace and an XML declaration.
fn with_namespace(stored: &str, root: &str) -> String {
    let inner = stored.strip_prefix(&format!("<{root}>")).unwrap_or(stored);
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} xmlns=\"{XMLNS}\">{inner}")
}

async fn put_id_config(ctx: &mut Ctx, bucket: &Bucket, spec: IdConfigSpec, body: Body) -> Reply {
    writing(ctx, bucket)?;
    let id = config_id(ctx)?;
    let n = read_xml(ctx, body, spec.root).await?;
    if n.text_of("Id").map(str::trim) != Some(id.as_str()) {
        return Err(S3Error::msg(Code::InvalidArgument, "The Id in the configuration must match the id query parameter").with("ArgumentName", "Id"));
    }
    (spec.validate)(&n)?;
    let v = node_xml(&n);
    ctx.store().set_bucket_config(&bucket.name, spec.kind, &id, Some(&v), Some(MAX_ID_CONFIGS)).await.map_err(|e| match e {
        StoreError::InvalidArgument(_) => S3Error::new(Code::TooManyConfigurations),
        e => crate::error::store_error(e),
    })?;
    ok()
}

async fn delete_id_config(ctx: &mut Ctx, bucket: &Bucket, spec: IdConfigSpec) -> Reply {
    writing(ctx, bucket)?;
    let id = config_id(ctx)?;
    if ctx.store().bucket_config(&bucket.name, spec.kind, &id).s3()?.is_none() {
        return Err(no_such_configuration());
    }
    ctx.store().set_bucket_config(&bucket.name, spec.kind, &id, None, None).await.s3()?;
    no_content()
}

fn list_id_configs(ctx: &mut Ctx, bucket: &Bucket, spec: IdConfigSpec) -> Reply {
    reading(ctx, bucket)?;
    let token = ctx.q("continuation-token").filter(|t| !t.is_empty()).map(str::to_string);
    let after = match &token {
        Some(t) => Some(
            URL_SAFE_NO_PAD
                .decode(t)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| S3Error::msg(Code::InvalidArgument, "The continuation token provided is incorrect").with("ArgumentName", "continuation-token"))?,
        ),
        None => None,
    };
    let mut rows = ctx.store().list_bucket_config(&bucket.name, spec.kind, after.as_deref(), LIST_PAGE + 1).s3()?;
    let truncated = rows.len() > LIST_PAGE;
    rows.truncate(LIST_PAGE);
    let mut d = Doc::new(spec.list_root);
    for (_, v) in &rows {
        d.raw(v);
    }
    d.el("IsTruncated", bool_str(truncated));
    d.opt("ContinuationToken", token.as_deref());
    if truncated && let Some((last, _)) = rows.last() {
        d.el("NextContinuationToken", &URL_SAFE_NO_PAD.encode(last.as_bytes()));
    }
    response::ok_xml(d.finish(spec.list_root))
}

/// `Filter`: `Prefix`, one `Tag`, `AccessPointArn`, or `And` of those.
fn validate_filter(n: &Node) -> Result<(), S3Error> {
    let Some(f) = n.child("Filter") else { return Ok(()) };
    let tag_ok = |t: &Node| t.text_of("Key").is_some_and(|k| !k.is_empty()) && t.child("Value").is_some();
    for c in &f.children {
        match c.name.as_str() {
            "Prefix" | "AccessPointArn" => {}
            "Tag" if tag_ok(c) => {}
            "And" => {
                for a in &c.children {
                    match a.name.as_str() {
                        "Prefix" | "AccessPointArn" => {}
                        "Tag" if tag_ok(a) => {}
                        _ => return Err(malformed()),
                    }
                }
            }
            _ => return Err(malformed()),
        }
    }
    if f.children.len() > 1 {
        return Err(malformed());
    }
    Ok(())
}

fn validate_filter_only(n: &Node) -> Result<(), S3Error> {
    validate_filter(n)
}

fn validate_destination(d: &Node, formats: &[&str]) -> Result<(), S3Error> {
    let s3 = d.child("S3BucketDestination").ok_or_else(malformed)?;
    let bucket = s3.text_of("Bucket").ok_or_else(malformed)?;
    if !bucket.starts_with("arn:") {
        return Err(S3Error::msg(Code::InvalidArgument, "The destination bucket must be an ARN (arn:aws:s3:::bucket)"));
    }
    if !s3.text_of("Format").is_some_and(|f| formats.contains(&f.trim())) {
        return Err(malformed());
    }
    Ok(())
}

fn validate_analytics(n: &Node) -> Result<(), S3Error> {
    validate_filter(n)?;
    let sca = n.child("StorageClassAnalysis").ok_or_else(malformed)?;
    if let Some(export) = sca.child("DataExport") {
        if export.text_of("OutputSchemaVersion").map(str::trim) != Some("V_1") {
            return Err(malformed());
        }
        validate_destination(export.child("Destination").ok_or_else(malformed)?, &["CSV"])?;
    }
    Ok(())
}

fn validate_inventory(n: &Node) -> Result<(), S3Error> {
    validate_filter(n)?;
    parse_bool(n, "IsEnabled")?.ok_or_else(malformed)?;
    validate_destination(n.child("Destination").ok_or_else(malformed)?, &["CSV", "ORC", "Parquet"])?;
    if !n.text_of("IncludedObjectVersions").is_some_and(|v| matches!(v.trim(), "All" | "Current")) {
        return Err(malformed());
    }
    if !n.child("Schedule").and_then(|s| s.text_of("Frequency")).is_some_and(|f| matches!(f.trim(), "Daily" | "Weekly")) {
        return Err(malformed());
    }
    Ok(())
}

fn validate_intelligent_tiering(n: &Node) -> Result<(), S3Error> {
    validate_filter(n)?;
    if !n.text_of("Status").is_some_and(|s| matches!(s.trim(), "Enabled" | "Disabled")) {
        return Err(malformed());
    }
    let tierings: Vec<&Node> = n.all("Tiering").collect();
    for t in &tierings {
        if !t.text_of("AccessTier").is_some_and(|a| matches!(a.trim(), "ARCHIVE_ACCESS" | "DEEP_ARCHIVE_ACCESS")) || t.text_of("Days").and_then(|d| d.trim().parse::<u32>().ok()).is_none() {
            return Err(malformed());
        }
    }
    if !tierings.is_empty() {
        return Err(S3Error::msg(Code::NotImplemented, "Intelligent-Tiering archive tiers are not supported: roll never archives objects, so they never need a restore."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_round_trip() {
        let n = xml::parse(b"<?xml version=\"1.0\"?><MetricsConfiguration xmlns=\"x\">\n  <Id>a&amp;b</Id>\n  <Filter><Prefix>p/</Prefix></Filter>\n</MetricsConfiguration>").unwrap();
        let s = node_xml(&n);
        assert_eq!(s, "<MetricsConfiguration><Id>a&amp;b</Id><Filter><Prefix>p/</Prefix></Filter></MetricsConfiguration>");
        let back = xml::parse(with_namespace(&s, "MetricsConfiguration").as_bytes()).unwrap();
        assert_eq!(back.text_of("Id"), Some("a&b"));
        assert!(with_namespace(&s, "MetricsConfiguration").contains(&format!("<MetricsConfiguration xmlns=\"{XMLNS}\"><Id>")));
    }

    #[test]
    fn filters() {
        let ok = |x: &str| validate_filter(&xml::parse(x.as_bytes()).unwrap()).is_ok();
        assert!(ok("<C><Id>a</Id></C>"));
        assert!(ok("<C><Filter><Prefix>a</Prefix></Filter></C>"));
        assert!(ok("<C><Filter><And><Prefix>a</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter></C>"));
        assert!(!ok("<C><Filter><Prefix>a</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></Filter></C>"));
        assert!(!ok("<C><Filter><Size>1</Size></Filter></C>"));
    }

    #[test]
    fn docs() {
        let mut d = Doc::new("R");
        d.el("A", "x<y").opt("B", None).open("C").close("C");
        assert_eq!(d.finish("R"), format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<R xmlns=\"{XMLNS}\"><A>x&lt;y</A><C></C></R>"));
        assert!(empty_doc("NotificationConfiguration").ends_with("/>"));
    }
}
