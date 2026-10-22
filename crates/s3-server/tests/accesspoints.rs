//! Access points and Object Lambda access points through aws-sdk-s3control and aws-sdk-s3:
//! the S3 Control API, requests by ARN (host style), by `/arn:…` path and by alias, the
//! access point and bucket policies together, network origins, and Object Lambda with a
//! webhook transformation answering through `WriteGetObjectResponse`.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3control::types::{AwsLambdaTransformation, ObjectLambdaAllowedFeature, ObjectLambdaConfiguration, ObjectLambdaContentTransformation, ObjectLambdaTransformationConfiguration, ObjectLambdaTransformationConfigurationAction, PublicAccessBlockConfiguration, VpcConfiguration};
use common::*;
use s3_store::Permission;

async fn versioned_bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

fn svc_code<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

#[tokio::test]
async fn control_api() {
    let t = start().await;
    let account = t.store.account_id();
    versioned_bucket(&t, "apdata").await;
    let ctl = control_client(t.port, ACCESS, SECRET);

    let out = ctl.create_access_point().account_id(&account).name("reader").bucket("apdata").send().await.unwrap();
    let arn = out.access_point_arn().unwrap().to_string();
    assert_eq!(arn, format!("arn:aws:s3:us-east-1:{account}:accesspoint/reader"));
    let alias = out.alias().unwrap().to_string();
    assert!(alias.starts_with("reader-") && alias.ends_with("-s3alias"), "{alias}");

    let dup = ctl.create_access_point().account_id(&account).name("reader").bucket("apdata").send().await.unwrap_err();
    assert_eq!(dup.as_service_error().map(svc_code).as_deref(), Some("AccessPointAlreadyOwnedByYou"));
    let bad = ctl.create_access_point().account_id(&account).name("UPPER").bucket("apdata").send().await.unwrap_err();
    assert_eq!(bad.as_service_error().map(svc_code).as_deref(), Some("InvalidRequest"));
    let wrong_account = ctl.create_access_point().account_id("111111111111").name("x1x").bucket("apdata").send().await.unwrap_err();
    assert_eq!(wrong_account.as_service_error().map(svc_code).as_deref(), Some("AccessDenied"));

    let vpc = ctl
        .create_access_point()
        .account_id(&account)
        .name("inside")
        .bucket("apdata")
        .vpc_configuration(VpcConfiguration::builder().vpc_id("vpc-1234").build().unwrap())
        .public_access_block_configuration(PublicAccessBlockConfiguration::builder().block_public_acls(true).build())
        .send()
        .await
        .unwrap();
    assert!(vpc.alias().is_some());

    let got = ctl.get_access_point().account_id(&account).name("reader").send().await.unwrap();
    assert_eq!(got.bucket(), Some("apdata"));
    assert_eq!(got.network_origin().map(|o| o.as_str()), Some("Internet"));
    assert_eq!(got.alias(), Some(alias.as_str()));
    assert_eq!(got.access_point_arn(), Some(arn.as_str()));
    let pab = got.public_access_block_configuration().unwrap();
    assert_eq!((pab.block_public_acls(), pab.ignore_public_acls(), pab.block_public_policy(), pab.restrict_public_buckets()), (Some(true), Some(true), Some(true), Some(true)), "all blocked by default");
    assert!(got.creation_date().is_some());
    let inside = ctl.get_access_point().account_id(&account).name("inside").send().await.unwrap();
    assert_eq!(inside.network_origin().map(|o| o.as_str()), Some("VPC"));
    assert_eq!(inside.vpc_configuration().map(|v| v.vpc_id()), Some("vpc-1234"));
    assert_eq!(inside.public_access_block_configuration().unwrap().block_public_policy(), Some(false), "given fields only");

    let all = ctl.list_access_points().account_id(&account).send().await.unwrap();
    let names: Vec<&str> = all.access_point_list().iter().map(|a| a.name()).collect();
    assert_eq!(names, ["inside", "reader"]);
    let page = ctl.list_access_points().account_id(&account).max_results(1).send().await.unwrap();
    assert_eq!(page.access_point_list().len(), 1);
    let next = page.next_token().unwrap().to_string();
    let page2 = ctl.list_access_points().account_id(&account).max_results(1).next_token(next).send().await.unwrap();
    assert_eq!(page2.access_point_list()[0].name(), "reader");
    assert!(page2.next_token().is_none());
    assert!(ctl.list_access_points().account_id(&account).bucket("nope").send().await.unwrap().access_point_list().is_empty());

    // Policies.
    let missing = ctl.get_access_point_policy().account_id(&account).name("reader").send().await.unwrap_err();
    assert_eq!(missing.as_service_error().map(svc_code).as_deref(), Some("NoSuchAccessPointPolicy"));
    let policy = format!(r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":"arn:aws:iam::{account}:root"}},"Action":"s3:GetObject","Resource":"{arn}/object/*"}}]}}"#);
    ctl.put_access_point_policy().account_id(&account).name("reader").policy(&policy).send().await.unwrap();
    assert_eq!(ctl.get_access_point_policy().account_id(&account).name("reader").send().await.unwrap().policy(), Some(policy.as_str()));
    let status = ctl.get_access_point_policy_status().account_id(&account).name("reader").send().await.unwrap();
    assert_eq!(status.policy_status().map(|s| s.is_public()), Some(false));
    let elsewhere = r#"{"Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::apdata/*"}]}"#;
    let e = ctl.put_access_point_policy().account_id(&account).name("reader").policy(elsewhere).send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("MalformedPolicy"));
    let public = format!(r#"{{"Statement":[{{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"{arn}/object/*"}}]}}"#);
    let e = ctl.put_access_point_policy().account_id(&account).name("reader").policy(&public).send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("AccessDenied"), "BlockPublicPolicy is on by default");
    ctl.delete_access_point_policy().account_id(&account).name("reader").send().await.unwrap();
    assert!(ctl.get_access_point_policy().account_id(&account).name("reader").send().await.is_err());

    // A read-only key sees access points but can't make them.
    let reader = t.store.create_key("ro", Permission::Read, None).await.unwrap();
    let ro = control_client(t.port, &reader.id, &reader.secret);
    assert_eq!(ro.list_access_points().account_id(&account).send().await.unwrap().access_point_list().len(), 2);
    let e = ro.create_access_point().account_id(&account).name("mine").bucket("apdata").send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("AccessDenied"));

    ctl.delete_access_point().account_id(&account).name("inside").send().await.unwrap();
    let gone = ctl.get_access_point().account_id(&account).name("inside").send().await.unwrap_err();
    assert_eq!(gone.as_service_error().map(svc_code).as_deref(), Some("NoSuchAccessPoint"));
    let none = ctl.create_access_point().account_id(&account).name("nobucket").bucket("no-such-bucket").send().await.unwrap_err();
    assert_eq!(none.as_service_error().map(svc_code).as_deref(), Some("NoSuchBucket"));
}

#[tokio::test]
async fn data_plane() {
    let t = start().await;
    let account = t.store.account_id();
    versioned_bucket(&t, "apobjs").await;
    let ctl = control_client(t.port, ACCESS, SECRET);
    let out = ctl.create_access_point().account_id(&account).name("front").bucket("apobjs").send().await.unwrap();
    let arn = out.access_point_arn().unwrap().to_string();
    let alias = out.alias().unwrap().to_string();

    // By ARN: the SDK sends it to `front-<account>.localhost`.
    let s3 = host_client(t.port, ACCESS, SECRET);
    s3.put_object().bucket(&arn).key("dir/a.txt").content_type("text/plain").body(ByteStream::from_static(b"via arn")).send().await.unwrap();
    let got = s3.get_object().bucket(&arn).key("dir/a.txt").send().await.unwrap();
    assert_eq!(got.content_type(), Some("text/plain"));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"via arn");
    assert_eq!(s3.head_object().bucket(&arn).key("dir/a.txt").send().await.unwrap().content_length(), Some(7));
    let list = s3.list_objects_v2().bucket(&arn).prefix("dir/").send().await.unwrap();
    assert_eq!(list.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), ["dir/a.txt"]);
    // It's the bucket's object.
    let direct = t.client.get_object().bucket("apobjs").key("dir/a.txt").send().await.unwrap();
    assert_eq!(direct.body.collect().await.unwrap().into_bytes().as_ref(), b"via arn");

    // Multipart through the access point.
    let up = s3.create_multipart_upload().bucket(&arn).key("big").send().await.unwrap();
    let id = up.upload_id().unwrap();
    let part = data(5 << 20, 3);
    let p1 = s3.upload_part().bucket(&arn).key("big").upload_id(id).part_number(1).body(ByteStream::from(part.clone())).send().await.unwrap();
    let p2 = s3.upload_part().bucket(&arn).key("big").upload_id(id).part_number(2).body(ByteStream::from_static(b"tail")).send().await.unwrap();
    let parts = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .parts(aws_sdk_s3::types::CompletedPart::builder().part_number(1).e_tag(p1.e_tag().unwrap()).build())
        .parts(aws_sdk_s3::types::CompletedPart::builder().part_number(2).e_tag(p2.e_tag().unwrap()).build())
        .build();
    s3.complete_multipart_upload().bucket(&arn).key("big").upload_id(id).multipart_upload(parts).send().await.unwrap();
    assert_eq!(t.client.head_object().bucket("apobjs").key("big").send().await.unwrap().content_length(), Some((5 << 20) + 4));

    // By alias, as a bucket name (path style).
    let got = t.client.get_object().bucket(&alias).key("dir/a.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"via arn");
    t.client.put_object().bucket(&alias).key("by-alias").body(ByteStream::from_static(b"x")).send().await.unwrap();
    assert!(t.client.head_object().bucket("apobjs").key("by-alias").send().await.is_ok());
    t.client.delete_object().bucket(&alias).key("by-alias").send().await.unwrap();
    let unknown = t.client.get_object().bucket("front-abcdefghijklmnopqrstuvwxyz-s3alias").key("x").send().await.unwrap_err();
    assert_eq!(unknown.as_service_error().map(svc_code).as_deref(), Some("NoSuchBucket"));

    // By `/arn:…` path.
    for path in [format!("/{arn}/dir/a.txt"), format!("/{}/dir/a.txt", arn.replace(':', "%3A").replace('/', "%2F"))] {
        let r = Raw::new("GET", &path).send(&t).await;
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"via arn");
    }
    let other = "/arn:aws:s3:us-east-1:111111111111:accesspoint/front/dir/a.txt";
    assert_eq!(status_code(Raw::new("GET", other).send(&t).await).await, (403, "AccessDenied".into()));
    let missing = format!("/arn:aws:s3:us-east-1:{account}:accesspoint/nothere/x");
    assert_eq!(status_code(Raw::new("GET", &missing).send(&t).await).await, (404, "NoSuchAccessPoint".into()));

    // Bucket configuration isn't reachable through an access point.
    let e = s3.get_bucket_policy().bucket(&arn).send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("InvalidRequest"));
    // Deletes work.
    s3.delete_object().bucket(&arn).key("dir/a.txt").send().await.unwrap();
    assert!(t.client.head_object().bucket("apobjs").key("dir/a.txt").send().await.is_err());
}

#[tokio::test]
async fn policies_combine() {
    let t = start().await;
    let account = t.store.account_id();
    versioned_bucket(&t, "shared").await;
    versioned_bucket(&t, "elsewhere").await;
    t.client.put_object().bucket("shared").key("doc").body(ByteStream::from_static(b"secret doc")).send().await.unwrap();
    let ctl = control_client(t.port, ACCESS, SECRET);
    let out = ctl.create_access_point().account_id(&account).name("team").bucket("shared").send().await.unwrap();
    let arn = out.access_point_arn().unwrap().to_string();
    // A key for another bucket only.
    let k = t.store.create_key("other", Permission::Read, Some(vec!["elsewhere".into()])).await.unwrap();
    let s3 = host_client(t.port, &k.id, &k.secret);
    let denied = |e: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>| e.as_service_error().map(svc_code).unwrap_or_default();
    assert_eq!(denied(s3.get_object().bucket(&arn).key("doc").send().await.unwrap_err()), "AccessDenied");

    // The access point grants it, but the bucket doesn't yet.
    let ap_policy = format!(r#"{{"Statement":[{{"Effect":"Allow","Principal":{{"AWS":"{}"}},"Action":"s3:GetObject","Resource":"{arn}/object/*"}}]}}"#, k.id);
    ctl.put_access_point_policy().account_id(&account).name("team").policy(&ap_policy).send().await.unwrap();
    assert_eq!(denied(s3.get_object().bucket(&arn).key("doc").send().await.unwrap_err()), "AccessDenied");

    // The bucket delegates to its access points (s3:DataAccessPointAccount).
    let bucket_policy = format!(r#"{{"Statement":[{{"Effect":"Allow","Principal":"*","Action":"*","Resource":["arn:aws:s3:::shared","arn:aws:s3:::shared/*"],"Condition":{{"StringEquals":{{"s3:DataAccessPointAccount":"{account}"}}}}}}]}}"#);
    t.client.put_bucket_policy().bucket("shared").policy(&bucket_policy).send().await.unwrap();
    let got = s3.get_object().bucket(&arn).key("doc").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"secret doc");
    // Only what the access point grants: no writes, and not around it.
    assert!(s3.put_object().bucket(&arn).key("w").body(ByteStream::from_static(b"w")).send().await.is_err());
    assert_eq!(denied(client(&t.endpoint, &k.id, &k.secret).get_object().bucket("shared").key("doc").send().await.unwrap_err()), "AccessDenied");

    // A Deny in the access point policy stops even an admin key.
    let deny = format!(r#"{{"Statement":[{{"Effect":"Deny","Principal":"*","Action":"s3:GetObject","Resource":"{arn}/object/doc"}}]}}"#);
    ctl.put_access_point_policy().account_id(&account).name("team").policy(&deny).send().await.unwrap();
    let admin = host_client(t.port, ACCESS, SECRET);
    assert_eq!(denied(admin.get_object().bucket(&arn).key("doc").send().await.unwrap_err()), "AccessDenied");
    assert!(t.client.get_object().bucket("shared").key("doc").send().await.is_ok(), "the bucket itself isn't affected");
}

#[tokio::test]
async fn vpc_origin() {
    let (t, _s) = start_full(|_| {}, |c| c.internal_cidrs = Vec::new()).await;
    let account = t.store.account_id();
    versioned_bucket(&t, "vpcb").await;
    let ctl = control_client(t.port, ACCESS, SECRET);
    let vpc = VpcConfiguration::builder().vpc_id("vpc-1").build().unwrap();
    let arn = ctl.create_access_point().account_id(&account).name("private").bucket("vpcb").vpc_configuration(vpc).send().await.unwrap().access_point_arn().unwrap().to_string();
    let open = ctl.create_access_point().account_id(&account).name("open").bucket("vpcb").send().await.unwrap().access_point_arn().unwrap().to_string();
    let s3 = host_client(t.port, ACCESS, SECRET);
    // No peer is inside roll's network on this server.
    let e = s3.put_object().bucket(&arn).key("k").body(ByteStream::from_static(b"k")).send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("AccessDenied"));
    s3.put_object().bucket(&open).key("k").body(ByteStream::from_static(b"k")).send().await.unwrap();
}

/// A webhook transformation: uppercases GetObject bodies (read from `inputS3Url`, answered with
/// WriteGetObjectResponse through aws-sdk-s3), answers HeadObject and ListObjectsV2 with JSON.
async fn transformer(port: u16) -> (String, tokio::sync::mpsc::UnboundedReceiver<(serde_json::Value, String, bytes::Bytes)>) {
    use axum::routing::post;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/t",
        post(move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
            let tx = tx.clone();
            async move {
                let ev: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let sig = headers.get("x-roll-signature").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                let _ = tx.send((ev.clone(), sig, body.clone()));
                if let Some(g) = ev.get("getObjectContext") {
                    let url = g["inputS3Url"].as_str().unwrap();
                    let original = reqwest::get(url).await.unwrap();
                    assert_eq!(original.status(), 200, "inputS3Url works");
                    let text = original.text().await.unwrap().to_uppercase();
                    let s3 = host_client(port, ACCESS, SECRET);
                    s3.write_get_object_response()
                        .request_route(g["outputRoute"].as_str().unwrap())
                        .request_token(g["outputToken"].as_str().unwrap())
                        .status_code(200)
                        .content_type("text/x-upper")
                        .metadata("transformed", "yes")
                        .body(ByteStream::from(text.into_bytes()))
                        .send()
                        .await
                        .unwrap();
                    return axum::Json(serde_json::json!({ "status_code": 200 }));
                }
                if ev.get("headObjectContext").is_some() {
                    return axum::Json(serde_json::json!({ "statusCode": 200, "headers": { "Content-Length": "42", "Content-Type": "text/x-head" }, "metadata": { "via": "lambda" } }));
                }
                axum::Json(serde_json::json!({ "statusCode": 200, "listBucketResult": { "name": "olapb", "prefix": "", "isTruncated": false, "keyCount": 1, "contents": [{ "key": "only-this", "size": 1, "eTag": "\"e\"", "lastModified": "2024-01-01T00:00:00.000Z" }] } }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/t"), rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn object_lambda_webhook() {
    let (t, _s) = start_full(|_| {}, |c| c.notify.webhook_allow_private = true).await;
    let account = t.store.account_id();
    versioned_bucket(&t, "olapb").await;
    t.client.put_object().bucket("olapb").key("greeting.txt").body(ByteStream::from_static(b"hello object lambda")).send().await.unwrap();
    let ctl = control_client(t.port, ACCESS, SECRET);
    let ap_arn = ctl.create_access_point().account_id(&account).name("plain").bucket("olapb").send().await.unwrap().access_point_arn().unwrap().to_string();
    let (hook, mut seen) = transformer(t.port).await;
    let transformation = |actions: &[ObjectLambdaTransformationConfigurationAction]| {
        ObjectLambdaTransformationConfiguration::builder()
            .set_actions(Some(actions.to_vec()))
            .content_transformation(ObjectLambdaContentTransformation::AwsLambda(AwsLambdaTransformation::builder().function_arn(format!("arn:roll:webhook:{hook}")).function_payload("upper").build().unwrap()))
            .build()
            .unwrap()
    };
    let cfg = ObjectLambdaConfiguration::builder()
        .supporting_access_point(&ap_arn)
        .allowed_features(ObjectLambdaAllowedFeature::GetObjectRange)
        .transformation_configurations(transformation(&[ObjectLambdaTransformationConfigurationAction::GetObject, ObjectLambdaTransformationConfigurationAction::HeadObject]))
        .transformation_configurations(transformation(&[ObjectLambdaTransformationConfigurationAction::ListObjectsV2]))
        .build()
        .unwrap();
    let created = ctl.create_access_point_for_object_lambda().account_id(&account).name("upper").configuration(cfg).send().await.unwrap();
    let olap = created.object_lambda_access_point_arn().unwrap().to_string();
    assert_eq!(olap, format!("arn:aws:s3-object-lambda:us-east-1:{account}:accesspoint/upper"));
    assert!(created.alias().and_then(|a| a.value()).is_some_and(|a| a.ends_with("--ol-s3")));
    let back = ctl.get_access_point_configuration_for_object_lambda().account_id(&account).name("upper").send().await.unwrap();
    let back = back.configuration().unwrap();
    assert_eq!(back.supporting_access_point(), ap_arn);
    assert_eq!(back.transformation_configurations().len(), 2);
    assert_eq!(ctl.list_access_points_for_object_lambda().account_id(&account).send().await.unwrap().object_lambda_access_point_list().len(), 1);
    assert!(ctl.get_access_point_for_object_lambda().account_id(&account).name("upper").send().await.unwrap().creation_date().is_some());
    // The supporting access point can't go while it's used.
    assert!(ctl.delete_access_point().account_id(&account).name("plain").send().await.is_err());

    let s3 = host_client(t.port, ACCESS, SECRET);
    let got = s3.get_object().bucket(&olap).key("greeting.txt").send().await.unwrap();
    assert_eq!(got.content_type(), Some("text/x-upper"));
    assert_eq!(got.metadata().and_then(|m| m.get("transformed")).map(String::as_str), Some("yes"));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"HELLO OBJECT LAMBDA");
    let (ev, sig, raw) = seen.recv().await.unwrap();
    assert_eq!(ev["configuration"]["accessPointArn"], olap.as_str());
    assert_eq!(ev["configuration"]["supportingAccessPointArn"], ap_arn.as_str());
    assert_eq!(ev["configuration"]["payload"], "upper");
    assert_eq!(ev["protocolVersion"], "1.00");
    assert!(ev["userRequest"]["url"].as_str().unwrap().contains("greeting.txt"));
    assert!(ev["userRequest"]["headers"].get("authorization").is_none(), "no credentials in the event");
    // Signed like notification webhooks, with the bucket's secret.
    let secret = t.store.webhook_secret("olapb", false).await.unwrap();
    let (ts, _) = sig.split_once(',').unwrap();
    let ts: i64 = ts.trim_start_matches("t=").parse().unwrap();
    assert_eq!(sig, s3_server::notify::webhook_signature(&secret, ts, &raw));

    let head = s3.head_object().bucket(&olap).key("greeting.txt").send().await.unwrap();
    assert_eq!(head.content_length(), Some(42));
    assert_eq!(head.content_type(), Some("text/x-head"));
    assert_eq!(head.metadata().and_then(|m| m.get("via")).map(String::as_str), Some("lambda"));
    let list = s3.list_objects_v2().bucket(&olap).send().await.unwrap();
    assert_eq!(list.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), ["only-this"]);

    // Not transformed: passed through to the supporting access point.
    s3.put_object().bucket(&olap).key("new.txt").body(ByteStream::from_static(b"put through")).send().await.unwrap();
    assert!(t.client.head_object().bucket("olapb").key("new.txt").send().await.is_ok());
    // partNumber isn't an allowed feature.
    let e = s3.get_object().bucket(&olap).key("greeting.txt").part_number(1).send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("InvalidRequest"));

    // WriteGetObjectResponse needs a live route and its token.
    let bad = s3.write_get_object_response().request_route("io-nothing").request_token("nope").status_code(200).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(bad.as_service_error().map(svc_code).as_deref(), Some("ValidationError"));

    // A read key of another bucket can't use the Object Lambda access point.
    let k = t.store.create_key("other", Permission::Read, Some(vec!["nothing".into()])).await.unwrap();
    let e = host_client(t.port, &k.id, &k.secret).get_object().bucket(&olap).key("greeting.txt").send().await.unwrap_err();
    assert_eq!(e.as_service_error().map(svc_code).as_deref(), Some("AccessDenied"));

    ctl.delete_access_point_for_object_lambda().account_id(&account).name("upper").send().await.unwrap();
    ctl.delete_access_point().account_id(&account).name("plain").send().await.unwrap();
}
