//! `amazonbedrock`: Titan and Cohere embeddings, `Converse` completions of every provider,
//! SigV4-signed; create, infer, update, delete and the error cases.

use crate::*;

#[tokio::test]
async fn amazon_bedrock_matches_elasticsearch_and_hides_its_keys() {
    let r = replay("amazonbedrock", true).await;
    assert_none(&r.problems);
    assert_secrets_hidden(&r, &["br-secret-1", "br-secret-2", "br-secret-3", "br-secret-new", "AKIAEXAMPLEKEY", "AKIANEW"]);
}

/// The signature is SigV4's for service `bedrock` in the endpoint's region, over the headers
/// the AWS SDK signs.
#[tokio::test]
async fn bedrock_requests_are_signed_for_the_region() {
    let m = mock().await;
    let c = cluster(&m).await;
    let put = r#"{"service":"amazonbedrock","service_settings":{"access_key":"AKIDEXAMPLE","secret_key":"wJalrXUtnFEMI","region":"eu-west-3","provider":"amazontitan","model":"amazon.titan-embed-text-v2:0"}}"#;
    let _ = call(&c, "PUT", "/_inference/text_embedding/sig", Some(put)).await;
    let seen = m.take();
    let auth = seen[0].header("authorization").unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{auth}");
    assert!(auth.contains("/eu-west-3/bedrock/aws4_request, SignedHeaders=amz-sdk-invocation-id;amz-sdk-request;content-length;content-type;host;x-amz-content-sha256;x-amz-date, Signature="), "{auth}");
    assert!(!auth.contains("wJalrXUtnFEMI"));
}
