//! Bucket configuration, policies, ACLs, public access blocks, websites and access logs,
//! through the AWS SDK (and raw HTTP for anonymous and website requests).

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    AccelerateConfiguration, AccessControlPolicy, AnalyticsConfiguration, AnalyticsFilter, BucketAccelerateStatus, BucketCannedAcl, BucketLoggingStatus, Condition, Delete, ErrorDocument, Grant, Grantee,
    IndexDocument, IntelligentTieringAccessTier, IntelligentTieringConfiguration, IntelligentTieringStatus, InventoryConfiguration, InventoryDestination, InventoryFormat, InventoryFrequency,
    InventoryIncludedObjectVersions, InventoryS3BucketDestination, InventorySchedule, LoggingEnabled, MetricsConfiguration, MetricsFilter, NotificationConfiguration, ObjectCannedAcl, ObjectIdentifier,
    ObjectOwnership, Owner, OwnershipControls, OwnershipControlsRule, PartitionDateSource, PartitionedPrefix, Payer, Permission, PublicAccessBlockConfiguration, Redirect, RedirectAllRequestsTo,
    RequestPaymentConfiguration, RoutingRule, ServerSideEncryption, ServerSideEncryptionByDefault, ServerSideEncryptionConfiguration, ServerSideEncryptionRule, StorageClassAnalysis, Tag, Tagging,
    TargetObjectKeyFormat, Tiering, Type, WebsiteConfiguration,
};
use common::*;
use s3_store::{BucketUpdate, Permission as KeyPermission};
use serde_json::json;

fn code<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map_or(0, |r| r.status().as_u16())
}

/// Status and S3 error code of an SDK error.
fn err<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> (u16, String) {
    (status(&e), code(&e))
}

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

async fn put(t: &TestServer, b: &str, k: &str, body: &[u8]) {
    t.client.put_object().bucket(b).key(k).body(ByteStream::from(body.to_vec())).send().await.unwrap();
}

/// An anonymous request.
async fn anon(t: &TestServer, method: &str, path: &str) -> u16 {
    reqwest::Client::new().request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", t.endpoint)).send().await.unwrap().status().as_u16()
}

/// A signed request for a bucket subresource: `(status, error code)`.
async fn sub(t: &TestServer, method: &'static str, path: &'static str, query: &'static str) -> (u16, String) {
    let mut r = Raw::new(method, path);
    r.query = query;
    status_code(r.send(t).await).await
}

async fn set_policy(t: &TestServer, b: &str, p: serde_json::Value) {
    t.client.put_bucket_policy().bucket(b).policy(p.to_string()).send().await.unwrap();
}

async fn ownership(t: &TestServer, b: &str, o: ObjectOwnership) {
    let c = OwnershipControls::builder().rules(OwnershipControlsRule::builder().object_ownership(o).build().unwrap()).build().unwrap();
    t.client.put_bucket_ownership_controls().bucket(b).ownership_controls(c).send().await.unwrap();
}

fn pab(acls: bool, ignore: bool, policy: bool, restrict: bool) -> PublicAccessBlockConfiguration {
    PublicAccessBlockConfiguration::builder().block_public_acls(acls).ignore_public_acls(ignore).block_public_policy(policy).restrict_public_buckets(restrict).build()
}

fn public_read(b: &str) -> serde_json::Value {
    json!({"Version": "2012-10-17", "Statement": [{"Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": format!("arn:aws:s3:::{b}/*")}]})
}

/// What Terraform's `aws_s3_bucket` refresh reads: every subresource of a fresh bucket answers
/// as S3 does, never 501. (`?object-lock` belongs to the Object Lock work.)
#[tokio::test(flavor = "multi_thread")]
async fn terraform_refresh_reads_every_subresource() {
    let t = start().await;
    bucket(&t, "tfrefresh").await;
    let ok = (200, String::new());
    let missing = |c: &str| (404, c.to_string());
    for (q, want) in [
        ("acl", ok.clone()),
        ("policy", missing("NoSuchBucketPolicy")),
        ("policyStatus", missing("NoSuchBucketPolicy")),
        ("versioning", ok.clone()),
        ("accelerate", ok.clone()),
        ("requestPayment", ok.clone()),
        ("logging", ok.clone()),
        ("lifecycle", missing("NoSuchLifecycleConfiguration")),
        ("replication", missing("ReplicationConfigurationNotFoundError")),
        ("encryption", ok.clone()),
        ("tagging", missing("NoSuchTagSet")),
        ("website", missing("NoSuchWebsiteConfiguration")),
        ("cors", missing("NoSuchCORSConfiguration")),
        ("publicAccessBlock", missing("NoSuchPublicAccessBlockConfiguration")),
        ("ownershipControls", ok.clone()),
        ("notification", ok.clone()),
        ("analytics", ok.clone()),
        ("metrics", ok.clone()),
        ("inventory", ok.clone()),
        ("intelligent-tiering", ok.clone()),
        ("location", ok.clone()),
        ("uploads", ok.clone()),
    ] {
        assert_eq!(sub(&t, "GET", "/tfrefresh", q).await, want, "?{q}");
    }
    // And the SDK parses every answer.
    let c = &t.client;
    let acl = c.get_bucket_acl().bucket("tfrefresh").send().await.unwrap();
    assert_eq!(acl.owner().unwrap().id(), Some(t.store.owner_id().as_str()));
    assert_eq!(acl.grants().len(), 1);
    assert!(c.get_bucket_accelerate_configuration().bucket("tfrefresh").send().await.unwrap().status().is_none());
    assert_eq!(c.get_bucket_request_payment().bucket("tfrefresh").send().await.unwrap().payer(), Some(&Payer::BucketOwner));
    assert!(c.get_bucket_logging().bucket("tfrefresh").send().await.unwrap().logging_enabled().is_none());
    let enc = c.get_bucket_encryption().bucket("tfrefresh").send().await.unwrap();
    let rule = &enc.server_side_encryption_configuration().unwrap().rules()[0];
    assert_eq!(rule.apply_server_side_encryption_by_default().unwrap().sse_algorithm(), &ServerSideEncryption::Aes256);
    assert_eq!(c.get_bucket_ownership_controls().bucket("tfrefresh").send().await.unwrap().ownership_controls().unwrap().rules()[0].object_ownership(), &ObjectOwnership::BucketOwnerEnforced);
    let n = c.get_bucket_notification_configuration().bucket("tfrefresh").send().await.unwrap();
    assert!(n.topic_configurations().is_empty() && n.queue_configurations().is_empty());
    assert_eq!(code(&c.get_bucket_replication().bucket("tfrefresh").send().await.unwrap_err().into_service_error()), "ReplicationConfigurationNotFoundError");
    assert_eq!(code(&c.get_bucket_website().bucket("tfrefresh").send().await.unwrap_err().into_service_error()), "NoSuchWebsiteConfiguration");
    assert!(c.list_bucket_metrics_configurations().bucket("tfrefresh").send().await.unwrap().metrics_configuration_list().is_empty());
    assert!(c.list_bucket_analytics_configurations().bucket("tfrefresh").send().await.unwrap().analytics_configuration_list().is_empty());
    assert!(c.list_bucket_inventory_configurations().bucket("tfrefresh").send().await.unwrap().inventory_configuration_list().is_empty());
    assert!(c.list_bucket_intelligent_tiering_configurations().bucket("tfrefresh").send().await.unwrap().intelligent_tiering_configuration_list().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn bucket_tagging() {
    let t = start().await;
    bucket(&t, "tags").await;
    let tag = |k: &str, v: &str| Tag::builder().key(k).value(v).build().unwrap();
    let tagging = |tags: Vec<Tag>| Tagging::builder().set_tag_set(Some(tags)).build().unwrap();
    t.client.put_bucket_tagging().bucket("tags").tagging(tagging(vec![tag("env", "prod"), tag("team", "")])).send().await.unwrap();
    let got = t.client.get_bucket_tagging().bucket("tags").send().await.unwrap();
    let got: Vec<(String, String)> = got.tag_set().iter().map(|t| (t.key().to_string(), t.value().to_string())).collect();
    assert_eq!(got, vec![("env".into(), "prod".into()), ("team".into(), "".into())]);
    assert_eq!(t.store.bucket_tags("tags").unwrap().len(), 2);
    // S3's limits: 50 tags, no aws: prefix, unique keys, key and value lengths.
    let fifty: Vec<Tag> = (0..50).map(|i| tag(&format!("k{i}"), "v")).collect();
    t.client.put_bucket_tagging().bucket("tags").tagging(tagging(fifty.clone())).send().await.unwrap();
    let mut too_many = fifty;
    too_many.push(tag("k50", "v"));
    for bad in [too_many, vec![tag("aws:x", "y")], vec![tag("a", "1"), tag("a", "2")], vec![tag(&"k".repeat(129), "v")], vec![tag("k", &"v".repeat(257))]] {
        assert_eq!(err(t.client.put_bucket_tagging().bucket("tags").tagging(tagging(bad)).send().await.unwrap_err()), (400, "InvalidTag".into()));
    }
    assert_eq!(t.store.bucket_tags("tags").unwrap().len(), 50, "a refused PUT changes nothing");
    t.client.delete_bucket_tagging().bucket("tags").send().await.unwrap();
    assert_eq!(err(t.client.get_bucket_tagging().bucket("tags").send().await.unwrap_err()), (404, "NoSuchTagSet".into()));
    // Configuration needs admin; reading it, read.
    let k = t.store.create_key("writer", KeyPermission::Write, None).await.unwrap();
    let wc = client(&t.endpoint, &k.id, &k.secret);
    assert_eq!(err(wc.put_bucket_tagging().bucket("tags").tagging(tagging(vec![tag("a", "b")])).send().await.unwrap_err()).1, "AccessDenied");
    assert_eq!(err(wc.get_bucket_tagging().bucket("tags").send().await.unwrap_err()).1, "NoSuchTagSet");
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_documents_and_status() {
    let t = start().await;
    bucket(&t, "pol").await;
    let doc = public_read("pol");
    set_policy(&t, "pol", doc.clone()).await;
    let got = t.client.get_bucket_policy().bucket("pol").send().await.unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(got.policy().unwrap()).unwrap(), doc);
    assert_eq!(t.client.get_bucket_policy_status().bucket("pol").send().await.unwrap().policy_status().unwrap().is_public(), Some(true));
    // Not public when pinned to a network.
    let mut pinned = doc.clone();
    pinned["Statement"][0]["Condition"] = json!({"IpAddress": {"aws:SourceIp": "203.0.113.0/24"}});
    set_policy(&t, "pol", pinned).await;
    assert_eq!(t.client.get_bucket_policy_status().bucket("pol").send().await.unwrap().policy_status().unwrap().is_public(), Some(false));
    for (bad, why) in [
        ("not json".to_string(), "json"),
        (json!({"Statement": []}).to_string(), "no statements"),
        (json!({"Statement": {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::other/*"}}).to_string(), "another bucket"),
        (json!({"Statement": {"Effect": "Allow", "Principal": "*", "Action": "s3:Fly", "Resource": "arn:aws:s3:::pol/*"}}).to_string(), "unknown action"),
        (json!({"Statement": {"Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::pol/*"}}).to_string(), "no principal"),
        (json!({"Statement": {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::pol/*", "Condition": {"Sometimes": {"aws:Referer": "x"}}}}).to_string(), "condition"),
        (format!("{{\"Id\": \"{}\", \"Statement\": []}}", "x".repeat(21 * 1024)), "too big"),
    ] {
        assert_eq!(err(t.client.put_bucket_policy().bucket("pol").policy(bad).send().await.unwrap_err()), (400, "MalformedPolicy".into()), "{why}");
    }
    t.client.delete_bucket_policy().bucket("pol").send().await.unwrap();
    assert_eq!(err(t.client.get_bucket_policy().bucket("pol").send().await.unwrap_err()), (404, "NoSuchBucketPolicy".into()));
    assert_eq!(err(t.client.get_bucket_policy_status().bucket("pol").send().await.unwrap_err()), (404, "NoSuchBucketPolicy".into()));
    // The public flag shows up as a public bucket.
    t.store.update_bucket("pol", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(t.client.get_bucket_policy_status().bucket("pol").send().await.unwrap().policy_status().unwrap().is_public(), Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn policies_are_enforced() {
    let t = start().await;
    for b in ["pol", "other"] {
        bucket(&t, b).await;
    }
    for k in ["public/a", "private/a", "secret/a", "keep/a", "keep/b"] {
        put(&t, "pol", k, k.as_bytes()).await;
    }
    let reader = t.store.create_key("reader", KeyPermission::Read, None).await.unwrap();
    let scoped = t.store.create_key("scoped", KeyPermission::Write, Some(vec!["other".into()])).await.unwrap();
    let account = t.store.account_id();
    set_policy(
        &t,
        "pol",
        json!({"Version": "2012-10-17", "Statement": [
            {"Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::pol/public/*"},
            {"Sid": "AnonList", "Effect": "Allow", "Principal": {"AWS": "*"}, "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::pol", "Condition": {"StringEquals": {"s3:prefix": "public/"}}},
            {"Sid": "ScopedWriter", "Effect": "Allow", "Principal": {"AWS": scoped.id}, "Action": ["s3:PutObject", "s3:ListBucket"], "Resource": ["arn:aws:s3:::pol", "arn:aws:s3:::pol/*"]},
            {"Sid": "NoSecrets", "Effect": "Deny", "Principal": {"AWS": format!("arn:aws:iam::{account}:user/{}", reader.id)}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::pol/secret/*"},
            {"Sid": "Keep", "Effect": "Deny", "Principal": "*", "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::pol/keep/*"}
        ]}),
    )
    .await;
    // Anonymous: what the policy allows, nothing else.
    assert_eq!(anon(&t, "GET", "/pol/public/a").await, 200);
    assert_eq!(anon(&t, "GET", "/pol/private/a").await, 403);
    assert_eq!(anon(&t, "PUT", "/pol/public/b").await, 403);
    assert_eq!(anon(&t, "GET", "/pol?list-type=2&prefix=public/").await, 200);
    assert_eq!(anon(&t, "GET", "/pol?list-type=2&prefix=private/").await, 403);
    assert_eq!(anon(&t, "GET", "/pol?list-type=2").await, 403);
    // A key scoped to another bucket gets what the policy grants it.
    let sc = client(&t.endpoint, &scoped.id, &scoped.secret);
    sc.put_object().bucket("pol").key("from-scoped").body(ByteStream::from_static(b"x")).send().await.unwrap();
    sc.list_objects_v2().bucket("pol").send().await.unwrap();
    sc.head_bucket().bucket("pol").send().await.unwrap();
    assert_eq!(err(sc.get_object().bucket("pol").key("private/a").send().await.unwrap_err()).0, 403);
    assert_eq!(err(sc.delete_object().bucket("pol").key("private/a").send().await.unwrap_err()).1, "AccessDenied");
    // A Deny beats the key's own permission.
    let rc = client(&t.endpoint, &reader.id, &reader.secret);
    rc.get_object().bucket("pol").key("private/a").send().await.unwrap();
    assert_eq!(err(rc.get_object().bucket("pol").key("secret/a").send().await.unwrap_err()).1, "AccessDenied");
    t.client.get_object().bucket("pol").key("secret/a").send().await.unwrap();
    // Even for admins; batch deletes are checked key by key.
    assert_eq!(err(t.client.delete_object().bucket("pol").key("keep/a").send().await.unwrap_err()).1, "AccessDenied");
    let del = Delete::builder()
        .objects(ObjectIdentifier::builder().key("keep/b").build().unwrap())
        .objects(ObjectIdentifier::builder().key("private/a").build().unwrap())
        .build()
        .unwrap();
    let r = t.client.delete_objects().bucket("pol").delete(del).send().await.unwrap();
    assert_eq!(r.deleted().iter().filter_map(|d| d.key()).collect::<Vec<_>>(), vec!["private/a"]);
    assert_eq!(r.errors().iter().map(|e| (e.key().unwrap(), e.code().unwrap())).collect::<Vec<_>>(), vec![("keep/b", "AccessDenied")]);
    t.client.head_object().bucket("pol").key("keep/b").send().await.unwrap();
    // Copies read their source under the source's policy.
    assert_eq!(err(rc.copy_object().bucket("pol").key("c").copy_source("pol/secret/a").send().await.unwrap_err()).1, "AccessDenied");
    let wk = t.store.create_key("w", KeyPermission::Write, Some(vec!["other".into()])).await.unwrap();
    let wc = client(&t.endpoint, &wk.id, &wk.secret);
    wc.copy_object().bucket("other").key("c").copy_source("pol/public/a").send().await.unwrap();
    assert_eq!(err(wc.copy_object().bucket("other").key("c").copy_source("pol/private/a").send().await.unwrap_err()).1, "AccessDenied");
    // A branch doesn't inherit the policy (it names the parent), and works normally.
    t.store.branch_bucket("pol", "pol-branch").await.unwrap();
    assert_eq!(err(t.client.get_bucket_policy().bucket("pol-branch").send().await.unwrap_err()).1, "NoSuchBucketPolicy");
    t.client.delete_object().bucket("pol-branch").key("keep/a").send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/pol-branch/public/a").await, 403);
    // A policy naming another bucket can't be put.
    let e = t.client.put_bucket_policy().bucket("pol-branch").policy(public_read("pol").to_string()).send().await.unwrap_err();
    assert_eq!(err(e).1, "MalformedPolicy");
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_conditions() {
    let t = start().await;
    bucket(&t, "cond").await;
    put(&t, "cond", "k", b"v").await;
    t.client.put_object().bucket("cond").key("tagged").tagging("class=secret").body(ByteStream::from_static(b"s")).send().await.unwrap();
    set_policy(
        &t,
        "cond",
        json!({"Version": "2012-10-17", "Statement": [
            {"Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "Resource": "arn:aws:s3:::cond/*", "Condition": {"StringNotEquals": {"s3:x-amz-acl": "bucket-owner-full-control"}}},
            {"Effect": "Deny", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::cond/*", "Condition": {"StringEqualsIgnoreCase": {"s3:ExistingObjectTag/class": "SECRET"}}},
            {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::cond/*", "Condition": {"IpAddress": {"aws:SourceIp": "127.0.0.0/8"}, "Bool": {"aws:SecureTransport": "false"}, "StringLike": {"aws:UserAgent": "*"}}},
            {"Effect": "Deny", "Principal": "*", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::cond", "Condition": {"NumericGreaterThan": {"s3:max-keys": 100}}}
        ]}),
    )
    .await;
    assert_eq!(err(t.client.put_object().bucket("cond").key("n").body(ByteStream::from_static(b"x")).send().await.unwrap_err()).1, "AccessDenied");
    t.client.put_object().bucket("cond").key("n").acl(ObjectCannedAcl::BucketOwnerFullControl).body(ByteStream::from_static(b"x")).send().await.unwrap();
    assert_eq!(err(t.client.get_object().bucket("cond").key("tagged").send().await.unwrap_err()).1, "AccessDenied");
    t.client.get_object().bucket("cond").key("k").send().await.unwrap();
    // Anonymous from loopback, over plain HTTP: allowed by the IP condition.
    let r = reqwest::Client::new().get(format!("{}/cond/k", t.endpoint)).header("user-agent", "curl/8").send().await.unwrap();
    assert_eq!(r.status(), 200);
    t.client.list_objects_v2().bucket("cond").max_keys(100).send().await.unwrap();
    assert_eq!(err(t.client.list_objects_v2().bucket("cond").max_keys(500).send().await.unwrap_err()).1, "AccessDenied");
    // A policy that denies everything still can't lock the owner out of the policy.
    set_policy(&t, "cond", json!({"Statement": {"Effect": "Deny", "Principal": "*", "Action": "s3:*", "Resource": ["arn:aws:s3:::cond", "arn:aws:s3:::cond/*"]}})).await;
    assert_eq!(err(t.client.get_object().bucket("cond").key("k").send().await.unwrap_err()).1, "AccessDenied");
    t.client.get_bucket_policy().bucket("cond").send().await.unwrap();
    t.client.delete_bucket_policy().bucket("cond").send().await.unwrap();
    t.client.get_object().bucket("cond").key("k").send().await.unwrap();
    // A source-IP pin that doesn't match denies anonymous reads.
    set_policy(&t, "cond", json!({"Statement": {"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::cond/*", "Condition": {"IpAddress": {"aws:SourceIp": "203.0.113.0/24"}}}})).await;
    assert_eq!(anon(&t, "GET", "/cond/k").await, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn public_access_block() {
    let t = start().await;
    bucket(&t, "pab").await;
    put(&t, "pab", "k", b"v").await;
    assert_eq!(err(t.client.get_public_access_block().bucket("pab").send().await.unwrap_err()), (404, "NoSuchPublicAccessBlockConfiguration".into()));
    set_policy(&t, "pab", public_read("pab")).await;
    assert_eq!(anon(&t, "GET", "/pab/k").await, 200);
    // RestrictPublicBuckets: a public policy only lets roll keys in.
    t.client.put_public_access_block().bucket("pab").public_access_block_configuration(pab(false, false, false, true)).send().await.unwrap();
    let got = t.client.get_public_access_block().bucket("pab").send().await.unwrap();
    let c = got.public_access_block_configuration().unwrap();
    assert_eq!((c.block_public_acls(), c.ignore_public_acls(), c.block_public_policy(), c.restrict_public_buckets()), (Some(false), Some(false), Some(false), Some(true)));
    assert_eq!(anon(&t, "GET", "/pab/k").await, 403);
    let k = t.store.create_key("other", KeyPermission::Read, Some(vec!["elsewhere".into()])).await.unwrap();
    client(&t.endpoint, &k.id, &k.secret).get_object().bucket("pab").key("k").send().await.unwrap();
    // ... and the public flag is ignored too.
    t.client.delete_bucket_policy().bucket("pab").send().await.unwrap();
    t.store.update_bucket("pab", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(anon(&t, "GET", "/pab/k").await, 403);
    t.client.delete_public_access_block().bucket("pab").send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/pab/k").await, 200);
    t.store.update_bucket("pab", BucketUpdate { public: Some(false), ..Default::default() }).await.unwrap();
    // BlockPublicPolicy: public policies (and the public flag) are refused; private ones are fine.
    t.client.put_public_access_block().bucket("pab").public_access_block_configuration(pab(false, false, true, false)).send().await.unwrap();
    assert_eq!(err(t.client.put_bucket_policy().bucket("pab").policy(public_read("pab").to_string()).send().await.unwrap_err()), (403, "AccessDenied".into()));
    let mut pinned = public_read("pab");
    pinned["Statement"][0]["Principal"] = json!({"AWS": k.id});
    set_policy(&t, "pab", pinned).await;
    assert!(t.store.update_bucket("pab", BucketUpdate { public: Some(true), ..Default::default() }).await.is_err());
    // BlockPublicAcls and IgnorePublicAcls (with ACLs enabled).
    ownership(&t, "pab", ObjectOwnership::BucketOwnerPreferred).await;
    t.client.put_public_access_block().bucket("pab").public_access_block_configuration(pab(false, false, false, false)).send().await.unwrap();
    t.client.put_object().bucket("pab").key("pr").acl(ObjectCannedAcl::PublicRead).body(ByteStream::from_static(b"p")).send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/pab/pr").await, 200);
    t.client.put_public_access_block().bucket("pab").public_access_block_configuration(pab(true, true, false, false)).send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/pab/pr").await, 403, "public grants are ignored");
    assert_eq!(err(t.client.put_object().bucket("pab").key("pr2").acl(ObjectCannedAcl::PublicRead).body(ByteStream::from_static(b"p")).send().await.unwrap_err()), (403, "AccessDenied".into()));
    assert_eq!(err(t.client.put_object_acl().bucket("pab").key("k").acl(ObjectCannedAcl::PublicRead).send().await.unwrap_err()), (403, "AccessDenied".into()));
    t.client.put_object_acl().bucket("pab").key("k").acl(ObjectCannedAcl::Private).send().await.unwrap();
    t.client.delete_public_access_block().bucket("pab").send().await.unwrap();
    assert_eq!(err(t.client.get_public_access_block().bucket("pab").send().await.unwrap_err()).1, "NoSuchPublicAccessBlockConfiguration");
}

#[tokio::test(flavor = "multi_thread")]
async fn deployment_wide_public_access_block() {
    let (t, _) = start_full(|s| s.block_public_access = true, |_| {}).await;
    bucket(&t, "dwide").await;
    put(&t, "dwide", "k", b"v").await;
    assert_eq!(err(t.client.put_bucket_policy().bucket("dwide").policy(public_read("dwide").to_string()).send().await.unwrap_err()), (403, "AccessDenied".into()));
    assert!(t.store.update_bucket("dwide", BucketUpdate { public: Some(true), ..Default::default() }).await.is_err());
    assert!(t.store.create_bucket("dwide2", s3_store::BucketOptions { public: Some(true), ..Default::default() }).await.is_err());
    let e = t.client.create_bucket().bucket("dwide3").object_ownership(ObjectOwnership::ObjectWriter).acl(BucketCannedAcl::PublicRead).send().await.unwrap_err();
    assert_eq!(err(e), (403, "AccessDenied".into()));
    ownership(&t, "dwide", ObjectOwnership::ObjectWriter).await;
    assert_eq!(err(t.client.put_object().bucket("dwide").key("p").acl(ObjectCannedAcl::PublicRead).body(ByteStream::from_static(b"p")).send().await.unwrap_err()).1, "AccessDenied");
    // The bucket's own block is still its own.
    assert_eq!(err(t.client.get_public_access_block().bucket("dwide").send().await.unwrap_err()).1, "NoSuchPublicAccessBlockConfiguration");
    assert_eq!(anon(&t, "GET", "/dwide/k").await, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn ownership_and_acls() {
    let t = start().await;
    bucket(&t, "acl").await;
    put(&t, "acl", "k", b"v").await;
    let owner = t.store.owner_id();
    assert_eq!(owner.len(), 64);
    assert!(owner.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(t.client.list_buckets().send().await.unwrap().owner().unwrap().id(), Some(owner.as_str()));
    let listed = t.client.list_objects_v2().bucket("acl").fetch_owner(true).send().await.unwrap();
    assert_eq!(listed.contents()[0].owner().unwrap().id(), Some(owner.as_str()));
    // BucketOwnerEnforced (the default): ACLs are off.
    assert_eq!(err(t.client.put_object_acl().bucket("acl").key("k").acl(ObjectCannedAcl::PublicRead).send().await.unwrap_err()), (400, "AccessControlListNotSupported".into()));
    assert_eq!(err(t.client.put_bucket_acl().bucket("acl").acl(BucketCannedAcl::PublicRead).send().await.unwrap_err()), (400, "AccessControlListNotSupported".into()));
    assert_eq!(err(t.client.put_object().bucket("acl").key("g").grant_read("uri=\"http://acs.amazonaws.com/groups/global/AllUsers\"").body(ByteStream::from_static(b"x")).send().await.unwrap_err()).1, "AccessControlListNotSupported");
    t.client.put_object().bucket("acl").key("o").grant_full_control(format!("id=\"{owner}\"")).body(ByteStream::from_static(b"x")).send().await.unwrap();
    assert_eq!(err(t.client.create_bucket().bucket("acl2").acl(BucketCannedAcl::PublicRead).send().await.unwrap_err()), (400, "InvalidBucketAclWithObjectOwnership".into()));
    // ACLs on.
    ownership(&t, "acl", ObjectOwnership::BucketOwnerPreferred).await;
    assert_eq!(t.client.get_bucket_ownership_controls().bucket("acl").send().await.unwrap().ownership_controls().unwrap().rules()[0].object_ownership(), &ObjectOwnership::BucketOwnerPreferred);
    t.client.put_object().bucket("acl").key("pub").acl(ObjectCannedAcl::PublicRead).body(ByteStream::from_static(b"p")).send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/acl/pub").await, 200);
    assert_eq!(anon(&t, "GET", "/acl/k").await, 403);
    let grants = t.client.get_object_acl().bucket("acl").key("pub").send().await.unwrap();
    let all_users = grants.grants().iter().any(|g| g.grantee().and_then(|g| g.uri()) == Some("http://acs.amazonaws.com/groups/global/AllUsers") && g.permission() == Some(&Permission::Read));
    assert!(all_users, "{grants:?}");
    // Copies don't take the source's ACL.
    t.client.copy_object().bucket("acl").key("copy").copy_source("acl/pub").send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/acl/copy").await, 403);
    t.client.put_object_acl().bucket("acl").key("pub").acl(ObjectCannedAcl::Private).send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/acl/pub").await, 403);
    // Grants to keys: by access key id, or by canonical id in an XML body.
    let k = t.store.create_key("elsewhere", KeyPermission::Read, Some(vec!["nothing".into()])).await.unwrap();
    let kc = client(&t.endpoint, &k.id, &k.secret);
    assert_eq!(err(kc.get_object().bucket("acl").key("k").send().await.unwrap_err()).1, "AccessDenied");
    t.client.put_object_acl().bucket("acl").key("k").grant_read(format!("id={}", k.id)).send().await.unwrap();
    kc.get_object().bucket("acl").key("k").send().await.unwrap();
    let canonical = t.store.canonical_user(&k.id);
    let got = t.client.get_object_acl().bucket("acl").key("k").send().await.unwrap();
    assert_eq!(got.grants()[0].grantee().unwrap().id(), Some(canonical.as_str()));
    assert_eq!(got.grants()[0].grantee().unwrap().display_name(), Some("elsewhere"));
    let policy = |perm: Permission| {
        AccessControlPolicy::builder()
            .owner(Owner::builder().id(&owner).build())
            .grants(Grant::builder().grantee(Grantee::builder().r#type(Type::CanonicalUser).id(&canonical).build().unwrap()).permission(perm).build())
            .build()
    };
    assert_eq!(err(kc.list_objects_v2().bucket("acl").send().await.unwrap_err()).1, "AccessDenied");
    t.client.put_bucket_acl().bucket("acl").access_control_policy(policy(Permission::Read)).send().await.unwrap();
    kc.list_objects_v2().bucket("acl").send().await.unwrap();
    assert_eq!(err(kc.put_object().bucket("acl").key("w").body(ByteStream::from_static(b"w")).send().await.unwrap_err()).1, "AccessDenied");
    t.client.put_bucket_acl().bucket("acl").access_control_policy(policy(Permission::Write)).send().await.unwrap();
    kc.put_object().bucket("acl").key("w").body(ByteStream::from_static(b"w")).send().await.unwrap();
    // Bad grantees.
    assert_eq!(err(t.client.put_object_acl().bucket("acl").key("k").grant_read("emailAddress=\"a@example.com\"").send().await.unwrap_err()).1, "UnresolvableGrantByEmailAddress");
    assert_eq!(err(t.client.put_object_acl().bucket("acl").key("k").grant_read("id=\"nobody\"").send().await.unwrap_err()).1, "InvalidArgument");
    assert_eq!(err(t.client.put_object_acl().bucket("acl").key("k").grant_read("uri=\"http://example.com/group\"").send().await.unwrap_err()).1, "InvalidArgument");
    assert_eq!(err(t.client.put_object_acl().bucket("acl").key("gone").acl(ObjectCannedAcl::Private).send().await.unwrap_err()).1, "NoSuchKey");
    // Back to BucketOwnerEnforced: refused while the bucket ACL grants others.
    let c = OwnershipControls::builder().rules(OwnershipControlsRule::builder().object_ownership(ObjectOwnership::BucketOwnerEnforced).build().unwrap()).build().unwrap();
    assert_eq!(err(t.client.put_bucket_ownership_controls().bucket("acl").ownership_controls(c.clone()).send().await.unwrap_err()).1, "InvalidBucketAclWithObjectOwnership");
    t.client.put_bucket_acl().bucket("acl").acl(BucketCannedAcl::Private).send().await.unwrap();
    t.client.put_bucket_ownership_controls().bucket("acl").ownership_controls(c).send().await.unwrap();
    assert_eq!(err(kc.get_object().bucket("acl").key("k").send().await.unwrap_err()).1, "AccessDenied", "object ACLs are off again");
    // Deleted controls: no controls at all (ObjectWriter behaviour).
    t.client.delete_bucket_ownership_controls().bucket("acl").send().await.unwrap();
    assert_eq!(err(t.client.get_bucket_ownership_controls().bucket("acl").send().await.unwrap_err()), (404, "OwnershipControlsNotFoundError".into()));
    kc.get_object().bucket("acl").key("k").send().await.unwrap();
    // CreateBucket with ownership and a canned ACL.
    t.client.create_bucket().bucket("listed").object_ownership(ObjectOwnership::ObjectWriter).acl(BucketCannedAcl::PublicRead).send().await.unwrap();
    assert_eq!(anon(&t, "GET", "/listed?list-type=2").await, 200);
    assert_eq!(anon(&t, "PUT", "/listed/x").await, 403);
    t.client.create_bucket().bucket("open").object_ownership(ObjectOwnership::ObjectWriter).acl(BucketCannedAcl::PublicReadWrite).send().await.unwrap();
    let r = reqwest::Client::new().put(format!("{}/open/anon.txt", t.endpoint)).body("hi").send().await.unwrap();
    assert_eq!(r.status(), 200, "public-read-write lets anyone write");
    assert_eq!(t.client.get_object().bucket("open").key("anon.txt").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn encryption_configuration() {
    let t = start().await;
    bucket(&t, "enc").await;
    let rule = |alg: ServerSideEncryption, key: Option<&str>| {
        let mut d = ServerSideEncryptionByDefault::builder().sse_algorithm(alg);
        if let Some(k) = key {
            d = d.kms_master_key_id(k);
        }
        ServerSideEncryptionConfiguration::builder().rules(ServerSideEncryptionRule::builder().apply_server_side_encryption_by_default(d.build().unwrap()).bucket_key_enabled(true).build()).build().unwrap()
    };
    let get = || async {
        let g = t.client.get_bucket_encryption().bucket("enc").send().await.unwrap();
        let r = g.server_side_encryption_configuration().unwrap().rules()[0].clone();
        let d = r.apply_server_side_encryption_by_default().unwrap().clone();
        (d.sse_algorithm().clone(), d.kms_master_key_id().map(str::to_string), r.bucket_key_enabled())
    };
    assert_eq!(get().await, (ServerSideEncryption::Aes256, None, Some(false)));
    t.client.put_bucket_encryption().bucket("enc").server_side_encryption_configuration(rule(ServerSideEncryption::AwsKms, Some("arn:aws:kms:us-east-1:111122223333:key/k1"))).send().await.unwrap();
    assert_eq!(get().await, (ServerSideEncryption::AwsKms, Some("arn:aws:kms:us-east-1:111122223333:key/k1".into()), Some(true)));
    t.client.put_bucket_encryption().bucket("enc").server_side_encryption_configuration(rule(ServerSideEncryption::Aes256, None)).send().await.unwrap();
    assert_eq!(get().await, (ServerSideEncryption::Aes256, None, Some(true)));
    assert_eq!(err(t.client.put_bucket_encryption().bucket("enc").server_side_encryption_configuration(rule(ServerSideEncryption::Aes256, Some("k"))).send().await.unwrap_err()).1, "InvalidArgument");
    t.client.delete_bucket_encryption().bucket("enc").send().await.unwrap();
    assert_eq!(get().await, (ServerSideEncryption::Aes256, None, Some(false)));
}

#[tokio::test(flavor = "multi_thread")]
async fn small_configurations() {
    let t = start().await;
    bucket(&t, "cfg").await;
    let c = &t.client;
    // Accelerate: stored and returned (one endpoint, so it changes nothing).
    c.put_bucket_accelerate_configuration().bucket("cfg").accelerate_configuration(AccelerateConfiguration::builder().status(BucketAccelerateStatus::Enabled).build()).send().await.unwrap();
    assert_eq!(c.get_bucket_accelerate_configuration().bucket("cfg").send().await.unwrap().status(), Some(&BucketAccelerateStatus::Enabled));
    c.put_bucket_accelerate_configuration().bucket("cfg").accelerate_configuration(AccelerateConfiguration::builder().status(BucketAccelerateStatus::Suspended).build()).send().await.unwrap();
    assert_eq!(c.get_bucket_accelerate_configuration().bucket("cfg").send().await.unwrap().status(), Some(&BucketAccelerateStatus::Suspended));
    // Request payment: the owner pays.
    c.put_bucket_request_payment().bucket("cfg").request_payment_configuration(RequestPaymentConfiguration::builder().payer(Payer::BucketOwner).build().unwrap()).send().await.unwrap();
    let e = c.put_bucket_request_payment().bucket("cfg").request_payment_configuration(RequestPaymentConfiguration::builder().payer(Payer::Requester).build().unwrap()).send().await.unwrap_err();
    assert_eq!(err(e), (501, "NotImplemented".into()));
    // Notifications: only the empty configuration for now.
    c.put_bucket_notification_configuration().bucket("cfg").notification_configuration(NotificationConfiguration::builder().build()).send().await.unwrap();
    let mut r = Raw::new("PUT", "/cfg");
    r.query = "notification";
    r.body = b"<NotificationConfiguration><QueueConfiguration><Queue>arn:aws:sqs:us-east-1:1:q</Queue><Event>s3:ObjectCreated:*</Event></QueueConfiguration></NotificationConfiguration>".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (501, "NotImplemented".into()));
    // Replication: not yet.
    assert_eq!(err(c.delete_bucket_replication().bucket("cfg").send().await.unwrap_err()).0, 501);
    // RestoreObject: nothing is archived.
    put(&t, "cfg", "k", b"v").await;
    assert_eq!(err(c.restore_object().bucket("cfg").key("k").send().await.unwrap_err()), (403, "InvalidObjectState".into()));
    assert_eq!(err(c.restore_object().bucket("cfg").key("missing").send().await.unwrap_err()), (404, "NoSuchKey".into()));
    // Bad bodies.
    let mut r = Raw::new("PUT", "/cfg");
    r.query = "accelerate";
    r.body = b"<AccelerateConfiguration><Status>Fast</Status></AccelerateConfiguration>".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (400, "MalformedXML".into()));
    let mut r = Raw::new("PUT", "/cfg");
    r.query = "publicAccessBlock";
    r.body = b"<PublicAccessBlockConfiguration><BlockPublicAcls>maybe</BlockPublicAcls></PublicAccessBlockConfiguration>".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (400, "MalformedXML".into()));
    bucket(&t, "dotted.bucket").await;
    let e = c.put_bucket_accelerate_configuration().bucket("dotted.bucket").accelerate_configuration(AccelerateConfiguration::builder().status(BucketAccelerateStatus::Enabled).build()).send().await.unwrap_err();
    assert_eq!(err(e).1, "InvalidRequest");
}

#[tokio::test(flavor = "multi_thread")]
async fn analytics_metrics_inventory_and_tiering() {
    let t = start().await;
    bucket(&t, "ids").await;
    let c = &t.client;
    // Metrics: CRUD and paging.
    for i in 0..105 {
        let id = format!("m{i:03}");
        let cfg = MetricsConfiguration::builder().id(&id).filter(MetricsFilter::Prefix(format!("p{i}/"))).build().unwrap();
        c.put_bucket_metrics_configuration().bucket("ids").id(&id).metrics_configuration(cfg).send().await.unwrap();
    }
    let got = c.get_bucket_metrics_configuration().bucket("ids").id("m007").send().await.unwrap();
    assert_eq!(got.metrics_configuration().unwrap().filter(), Some(&MetricsFilter::Prefix("p7/".into())));
    let page = c.list_bucket_metrics_configurations().bucket("ids").send().await.unwrap();
    assert_eq!(page.metrics_configuration_list().len(), 100);
    assert_eq!(page.is_truncated(), Some(true));
    let page2 = c.list_bucket_metrics_configurations().bucket("ids").continuation_token(page.next_continuation_token().unwrap()).send().await.unwrap();
    assert_eq!(page2.metrics_configuration_list().iter().map(|m| m.id()).collect::<Vec<_>>(), vec!["m100", "m101", "m102", "m103", "m104"]);
    assert_eq!(page2.is_truncated(), Some(false));
    c.delete_bucket_metrics_configuration().bucket("ids").id("m007").send().await.unwrap();
    assert_eq!(err(c.get_bucket_metrics_configuration().bucket("ids").id("m007").send().await.unwrap_err()), (404, "NoSuchConfiguration".into()));
    assert_eq!(err(c.delete_bucket_metrics_configuration().bucket("ids").id("m007").send().await.unwrap_err()), (404, "NoSuchConfiguration".into()));
    // Analytics.
    let a = AnalyticsConfiguration::builder().id("a1").filter(AnalyticsFilter::Prefix("logs/".into())).storage_class_analysis(StorageClassAnalysis::builder().build()).build().unwrap();
    c.put_bucket_analytics_configuration().bucket("ids").id("a1").analytics_configuration(a).send().await.unwrap();
    let got = c.get_bucket_analytics_configuration().bucket("ids").id("a1").send().await.unwrap();
    assert_eq!(got.analytics_configuration().unwrap().id(), "a1");
    assert_eq!(c.list_bucket_analytics_configurations().bucket("ids").send().await.unwrap().analytics_configuration_list().len(), 1);
    // Inventory (stored; reports aren't generated).
    let dest = InventoryDestination::builder().s3_bucket_destination(InventoryS3BucketDestination::builder().bucket("arn:aws:s3:::ids").format(InventoryFormat::Csv).build().unwrap()).build();
    let inv = InventoryConfiguration::builder()
        .id("inv")
        .is_enabled(true)
        .destination(dest)
        .included_object_versions(InventoryIncludedObjectVersions::Current)
        .schedule(InventorySchedule::builder().frequency(InventoryFrequency::Daily).build().unwrap())
        .build()
        .unwrap();
    c.put_bucket_inventory_configuration().bucket("ids").id("inv").inventory_configuration(inv).send().await.unwrap();
    let got = c.get_bucket_inventory_configuration().bucket("ids").id("inv").send().await.unwrap();
    assert_eq!(got.inventory_configuration().unwrap().schedule().unwrap().frequency(), &InventoryFrequency::Daily);
    assert_eq!(c.list_bucket_inventory_configurations().bucket("ids").send().await.unwrap().inventory_configuration_list().len(), 1);
    c.delete_bucket_inventory_configuration().bucket("ids").id("inv").send().await.unwrap();
    // Intelligent-tiering archive tiers: never (nothing is archived).
    let it = IntelligentTieringConfiguration::builder()
        .id("it")
        .status(IntelligentTieringStatus::Enabled)
        .tierings(Tiering::builder().days(90).access_tier(IntelligentTieringAccessTier::ArchiveAccess).build().unwrap())
        .build()
        .unwrap();
    assert_eq!(err(c.put_bucket_intelligent_tiering_configuration().bucket("ids").id("it").intelligent_tiering_configuration(it).send().await.unwrap_err()).0, 501);
    // Ids must match and be valid.
    let m = MetricsConfiguration::builder().id("x").build().unwrap();
    assert_eq!(err(c.put_bucket_metrics_configuration().bucket("ids").id("y").metrics_configuration(m).send().await.unwrap_err()).1, "InvalidArgument");
    let m = MetricsConfiguration::builder().id("bad id").build().unwrap();
    assert_eq!(err(c.put_bucket_metrics_configuration().bucket("ids").id("bad id").metrics_configuration(m).send().await.unwrap_err()).1, "InvalidArgument");
}

/// A website request: `(status, location, body)`.
async fn web(t: &TestServer, method: &str, host: &str, path: &str) -> (u16, Option<String>, String) {
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", t.endpoint)).header("host", host).send().await.unwrap();
    let loc = r.headers().get("location").map(|v| v.to_str().unwrap().to_string());
    (r.status().as_u16(), loc, r.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn websites() {
    let t = start_with(|c| c.website_domain = Some("web.localtest".into())).await;
    bucket(&t, "site").await;
    let host = "site.web.localtest";
    for (k, body) in [("index.html", "home"), ("dir/index.html", "dir home"), ("404.html", "not here"), ("page.html", "a page"), ("documents/a.html", "moved")] {
        t.client.put_object().bucket("site").key(k).content_type("text/html").body(ByteStream::from(body.as_bytes().to_vec())).send().await.unwrap();
    }
    t.client.put_object().bucket("site").key("old.html").website_redirect_location("/page.html").body(ByteStream::from_static(b"")).send().await.unwrap();
    assert_eq!(t.client.head_object().bucket("site").key("old.html").send().await.unwrap().website_redirect_location(), Some("/page.html"));
    assert_eq!(err(t.client.put_object().bucket("site").key("x").website_redirect_location("elsewhere").body(ByteStream::from_static(b"")).send().await.unwrap_err()).1, "InvalidRedirectLocation");
    // No configuration yet.
    let (s, _, body) = web(&t, "GET", host, "/").await;
    assert_eq!(s, 404);
    assert!(body.contains("NoSuchWebsiteConfiguration"), "{body}");
    let cfg = WebsiteConfiguration::builder()
        .index_document(IndexDocument::builder().suffix("index.html").build().unwrap())
        .error_document(ErrorDocument::builder().key("404.html").build().unwrap())
        .routing_rules(RoutingRule::builder().condition(Condition::builder().key_prefix_equals("docs/").build()).redirect(Redirect::builder().replace_key_prefix_with("documents/").build()).build())
        .routing_rules(
            RoutingRule::builder()
                .condition(Condition::builder().key_prefix_equals("gone/").http_error_code_returned_equals("404").build())
                .redirect(Redirect::builder().host_name("archive.example").http_redirect_code("302").build())
                .build(),
        )
        .build();
    t.client.put_bucket_website().bucket("site").website_configuration(cfg).send().await.unwrap();
    let got = t.client.get_bucket_website().bucket("site").send().await.unwrap();
    assert_eq!(got.index_document().unwrap().suffix(), "index.html");
    assert_eq!(got.error_document().unwrap().key(), "404.html");
    assert_eq!(got.routing_rules().len(), 2);
    assert_eq!(t.store.bucket_website("site").unwrap().unwrap().index_document.as_deref(), Some("index.html"));
    // Not public yet.
    let (s, _, body) = web(&t, "GET", host, "/").await;
    assert_eq!(s, 403);
    assert!(body.contains("<li>Code: AccessDenied</li>"), "{body}");
    set_policy(&t, "site", public_read("site")).await;
    assert_eq!(web(&t, "GET", host, "/").await, (200, None, "home".into()));
    assert_eq!(web(&t, "GET", host, "/page.html").await, (200, None, "a page".into()));
    assert_eq!(web(&t, "GET", host, "/dir/").await, (200, None, "dir home".into()));
    assert_eq!(web(&t, "GET", host, "/dir").await.0, 302);
    assert_eq!(web(&t, "GET", host, "/dir").await.1.as_deref(), Some("/dir/"));
    assert_eq!(web(&t, "GET", host, "/missing.html").await, (404, None, "not here".into()));
    assert_eq!(web(&t, "GET", host, "/old.html").await.1.as_deref(), Some("/page.html"));
    assert_eq!(web(&t, "GET", host, "/old.html").await.0, 301);
    let (s, loc, _) = web(&t, "GET", host, "/docs/a.html").await;
    assert_eq!((s, loc.as_deref()), (301, Some("http://site.web.localtest/documents/a.html")));
    let (s, loc, _) = web(&t, "GET", host, "/gone/x.html").await;
    assert_eq!((s, loc.as_deref()), (302, Some("http://archive.example/gone/x.html")));
    let (s, _, body) = web(&t, "HEAD", host, "/page.html").await;
    assert_eq!((s, body.as_str()), (200, ""));
    assert_eq!(web(&t, "POST", host, "/page.html").await.0, 405);
    assert_eq!(web(&t, "DELETE", host, "/page.html").await.0, 405);
    t.client.head_object().bucket("site").key("page.html").send().await.unwrap();
    // Without an error document: S3's HTML page.
    t.client.delete_object().bucket("site").key("404.html").send().await.unwrap();
    let (s, _, body) = web(&t, "GET", host, "/nope").await;
    assert_eq!(s, 404);
    assert!(body.contains("<li>Code: NoSuchKey</li>") && body.contains("<li>Key: nope</li>") && body.contains("Custom Error Document"), "{body}");
    // Unknown bucket.
    let (s, _, body) = web(&t, "GET", "ghost.web.localtest", "/").await;
    assert_eq!(s, 404);
    assert!(body.contains("NoSuchBucket"), "{body}");
    // Redirect everything.
    let cfg = WebsiteConfiguration::builder().redirect_all_requests_to(RedirectAllRequestsTo::builder().host_name("example.com").protocol(aws_sdk_s3::types::Protocol::Https).build().unwrap()).build();
    t.client.put_bucket_website().bucket("site").website_configuration(cfg).send().await.unwrap();
    let (s, loc, _) = web(&t, "GET", host, "/a/b.html").await;
    assert_eq!((s, loc.as_deref()), (301, Some("https://example.com/a/b.html")));
    t.client.delete_bucket_website().bucket("site").send().await.unwrap();
    assert_eq!(err(t.client.get_bucket_website().bucket("site").send().await.unwrap_err()).1, "NoSuchWebsiteConfiguration");
    // The REST API on the same server is untouched.
    assert_eq!(anon(&t, "GET", "/site/page.html").await, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn access_logs_are_delivered() {
    let (t, server) = start_full(|_| {}, |c| c.access_log_interval = None).await;
    bucket(&t, "src").await;
    bucket(&t, "logs").await;
    let enabled = |prefix: &str, partitioned: bool| {
        let mut l = LoggingEnabled::builder().target_bucket("logs").target_prefix(prefix);
        if partitioned {
            l = l.target_object_key_format(TargetObjectKeyFormat::builder().partitioned_prefix(PartitionedPrefix::builder().partition_date_source(PartitionDateSource::EventTime).build()).build());
        }
        BucketLoggingStatus::builder().logging_enabled(l.build().unwrap()).build()
    };
    let e = t.client.put_bucket_logging().bucket("src").bucket_logging_status(BucketLoggingStatus::builder().logging_enabled(LoggingEnabled::builder().target_bucket("nowhere").target_prefix("x/").build().unwrap()).build()).send().await.unwrap_err();
    assert_eq!(err(e), (400, "InvalidTargetBucketForLogging".into()));
    t.client.put_bucket_logging().bucket("src").bucket_logging_status(enabled("access/", false)).send().await.unwrap();
    let got = t.client.get_bucket_logging().bucket("src").send().await.unwrap();
    assert_eq!(got.logging_enabled().unwrap().target_bucket(), "logs");
    assert_eq!(got.logging_enabled().unwrap().target_prefix(), "access/");
    put(&t, "src", "a.txt", b"hello").await;
    t.client.get_object().bucket("src").key("a.txt").send().await.unwrap();
    assert!(t.client.get_object().bucket("src").key("missing").send().await.is_err());
    assert_eq!(anon(&t, "GET", "/src/a.txt").await, 403);
    assert_eq!(server.flush_access_logs().await, 1);
    assert_eq!(server.flush_access_logs().await, 0, "nothing new");
    let listed = t.client.list_objects_v2().bucket("logs").prefix("access/").send().await.unwrap();
    assert_eq!(listed.contents().len(), 1);
    let key = listed.contents()[0].key().unwrap().to_string();
    assert!(key.len() == "access/".len() + "2026-01-01-00-00-00-0123456789ABCDEF".len(), "{key}");
    let body = t.client.get_object().bucket("logs").key(&key).send().await.unwrap().body.collect().await.unwrap().into_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 4, "{text}");
    let owner = t.store.owner_id();
    assert!(lines.iter().all(|l| l.starts_with(&format!("{owner} src ["))), "{text}");
    assert!(text.contains("REST.PUT.OBJECT a.txt \"PUT /src/a.txt"), "{text}");
    assert!(text.contains("REST.GET.OBJECT a.txt \"GET /src/a.txt"), "{text}");
    assert!(text.contains(" 404 NoSuchKey "), "{text}");
    assert!(text.contains(" 403 AccessDenied "), "{text}");
    assert!(text.contains(&t.store.canonical_user(ACCESS)), "the requester's canonical id");
    assert!(text.contains(" SigV4 - AuthHeader 127.0.0.1:"), "{text}");
    // Partitioned keys.
    t.client.put_bucket_logging().bucket("src").bucket_logging_status(enabled("p/", true)).send().await.unwrap();
    t.client.head_object().bucket("src").key("a.txt").send().await.unwrap();
    assert_eq!(server.flush_access_logs().await, 1);
    let listed = t.client.list_objects_v2().bucket("logs").prefix("p/").send().await.unwrap();
    let key = listed.contents()[0].key().unwrap();
    assert!(key.starts_with(&format!("p/{}/us-east-1/src/", t.store.account_id())), "{key}");
    // Logging off.
    t.client.put_bucket_logging().bucket("src").bucket_logging_status(BucketLoggingStatus::builder().build()).send().await.unwrap();
    assert!(t.client.get_bucket_logging().bucket("src").send().await.unwrap().logging_enabled().is_none());
    t.client.head_object().bucket("src").key("a.txt").send().await.unwrap();
    assert_eq!(server.flush_access_logs().await, 0);
}
