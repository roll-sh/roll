//! SAML federation: IAM SAML providers and STS `AssumeRoleWithSAML`, with responses signed here
//! by a generated RSA key (the IdP), and the attacks a verifier has to stand up to: signature
//! wrapping (a second assertion, a moved signature, a reference to another element, duplicate
//! IDs), tampering, comments inside values, DTDs, other keys, wrong algorithms, expired or
//! misdirected assertions.
//!
//! The canonical forms are written out literally (attributes in canonical order, namespaces
//! where exclusive canonicalization puts them), so the digests and signatures here don't depend
//! on roll's canonicalization: if roll canonicalizes differently, verification fails.

mod common;
#[path = "common/iamkit.rs"]
mod iamkit;
#[path = "common/samlkit.rs"]
mod samlkit;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_sts::config::{BehaviorVersion, Region};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use iamkit::*;
use samlkit::*;
use serde_json::json;

fn sts(t: &TestServer) -> aws_sdk_sts::Client {
    let conf = aws_sdk_sts::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new(REGION)).endpoint_url(&t.endpoint).build();
    aws_sdk_sts::Client::from_conf(conf)
}

struct Fed {
    e: Env,
    role: String,
    provider: String,
}

/// A SAML provider `idp`, a role `fed` trusting it (for the AWS sign-in audience), allowed to
/// read `saml/<its saml:sub>/*`, and a bucket `saml`.
async fn fed() -> Fed {
    let e = env().await;
    let root = Creds::root();
    let acct = e.account();
    let out = root.ok(&e, &[("Action", "CreateSAMLProvider"), ("Name", "idp"), ("SAMLMetadataDocument", &idp().metadata(ISSUER))]).await;
    let provider = tag(&out, "SAMLProviderArn");
    assert_eq!(provider, format!("arn:aws:iam::{acct}:saml-provider/idp"));
    let trust = json!({"Version": "2012-10-17", "Statement": [{
        "Effect": "Allow", "Principal": {"Federated": provider}, "Action": ["sts:AssumeRoleWithSAML", "sts:TagSession"],
        "Condition": {"StringEquals": {"SAML:aud": SIGNIN}, "ForAnyValue:StringEquals": {"saml:edupersonaffiliation": "staff"}}
    }]});
    let out = root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "fed"), ("AssumeRolePolicyDocument", &trust.to_string()), ("MaxSessionDuration", "7200")]).await;
    let role = tag(&out, "Arn");
    let policy = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::saml/${saml:sub}/*"}]});
    root.ok(&e, &[("Action", "PutRolePolicy"), ("RoleName", "fed"), ("PolicyName", "own"), ("PolicyDocument", &policy.to_string())]).await;
    e.t.client.create_bucket().bucket("saml").send().await.unwrap();
    for k in ["alice@example.com/a.txt", "bob@example.com/b.txt"] {
        e.t.client.put_object().bucket("saml").key(k).body(ByteStream::from_static(b"hi")).send().await.unwrap();
    }
    Fed { e, role, provider }
}

impl Fed {
    async fn assume(&self, saml: &str) -> Result<aws_sdk_sts::operation::assume_role_with_saml::AssumeRoleWithSamlOutput, String> {
        use aws_sdk_sts::error::ProvideErrorMetadata;
        sts(&self.e.t).assume_role_with_saml().role_arn(&self.role).principal_arn(&self.provider).saml_assertion(b64(saml)).send().await.map_err(|e| {
            let se = e.as_service_error();
            format!("{}: {}", se.and_then(|s| s.code()).unwrap_or("?"), se.and_then(|s| s.message()).map(str::to_string).unwrap_or_else(|| format!("{e:?}")))
        })
    }

    async fn refused(&self, saml: &str, want: &str) {
        let r = self.assume(saml).await;
        let err = r.err().unwrap_or_else(|| panic!("must be refused ({want})"));
        assert!(err.contains(want), "{want}: {err}");
    }
}

#[tokio::test]
async fn saml_providers() {
    let e = env().await;
    let root = Creds::root();
    let acct = e.account();
    let meta = idp().metadata(ISSUER);
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "x"), ("SAMLMetadataDocument", "<short/>")]).await, (400, "ValidationError".into()));
    let no_cert = format!("<md:EntityDescriptor xmlns:md=\"urn:oasis:names:tc:SAML:2.0:metadata\" entityID=\"{ISSUER}\"><md:IDPSSODescriptor/></md:EntityDescriptor>{}", " ".repeat(1000));
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "x"), ("SAMLMetadataDocument", &no_cert)]).await, (400, "InvalidInput".into()));
    let doctype = format!("<!DOCTYPE x [<!ENTITY a \"b\">]>{meta}");
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "x"), ("SAMLMetadataDocument", &doctype)]).await.1, "InvalidInput");
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "bad name"), ("SAMLMetadataDocument", &meta)]).await.1, "ValidationError");
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "x"), ("SAMLMetadataDocument", &meta), ("AssertionEncryptionMode", "Required")]).await.1, "ValidationError");
    let out = root.ok(&e, &[("Action", "CreateSAMLProvider"), ("Name", "okta"), ("SAMLMetadataDocument", &meta), ("Tags.member.1.Key", "env"), ("Tags.member.1.Value", "prod")]).await;
    let arn = tag(&out, "SAMLProviderArn");
    assert_eq!(arn, format!("arn:aws:iam::{acct}:saml-provider/okta"));
    assert_eq!(root.fail(&e, &[("Action", "CreateSAMLProvider"), ("Name", "okta"), ("SAMLMetadataDocument", &meta)]).await, (409, "EntityAlreadyExists".into()));
    let got = root.ok(&e, &[("Action", "GetSAMLProvider"), ("SAMLProviderArn", &arn)]).await;
    assert!(got.contains("<SAMLMetadataDocument>") && got.contains("<ValidUntil>") && got.contains("<SAMLProviderUUID>") && got.contains("<Key>env</Key>"), "{got}");
    let list = root.ok(&e, &[("Action", "ListSAMLProviders")]).await;
    assert_eq!(tags(&list, "Arn"), vec![arn.clone()]);
    root.ok(&e, &[("Action", "UpdateSAMLProvider"), ("SAMLProviderArn", &arn), ("SAMLMetadataDocument", &evil().metadata("https://other.example.test"))]).await;
    root.ok(&e, &[("Action", "UntagSAMLProvider"), ("SAMLProviderArn", &arn), ("TagKeys.member.1", "env")]).await;
    assert!(!root.ok(&e, &[("Action", "ListSAMLProviderTags"), ("SAMLProviderArn", &arn)]).await.contains("<Key>"));
    // A trust policy naming a provider that doesn't exist is refused, as on AWS.
    let trust = |p: &str| json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Federated": p}, "Action": "sts:AssumeRoleWithSAML"}]}).to_string();
    let (code, msg) = root.fail_message(&e, &[("Action", "CreateRole"), ("RoleName", "r"), ("AssumeRolePolicyDocument", &trust(&format!("arn:aws:iam::{acct}:saml-provider/nope")))]).await;
    assert_eq!(code, "MalformedPolicyDocument");
    assert!(msg.contains("Invalid principal in policy"), "{msg}");
    root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "r"), ("AssumeRolePolicyDocument", &trust(&arn))]).await;
    root.ok(&e, &[("Action", "DeleteSAMLProvider"), ("SAMLProviderArn", &arn)]).await;
    assert_eq!(root.fail(&e, &[("Action", "GetSAMLProvider"), ("SAMLProviderArn", &arn)]).await, (404, "NoSuchEntity".into()));
    // An IAM user needs permission.
    let alice = user(&e, "alice").await;
    assert_eq!(alice.fail(&e, &[("Action", "ListSAMLProviders")]).await, (403, "AccessDenied".into()));
}

#[tokio::test]
async fn assume_role_with_saml() {
    let f = fed().await;
    let e = &f.e;
    let acct = e.account();
    let a = A::new(e.now_s() * 1000, &f.role, &f.provider);
    let out = f.assume(&signed(&a, idp())).await.unwrap();
    assert_eq!(out.subject(), Some("alice@example.com"));
    assert_eq!(out.subject_type(), Some("persistent"));
    assert_eq!(out.issuer(), Some(ISSUER));
    assert_eq!(out.audience(), Some(SIGNIN));
    let nq = STANDARD.encode(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, format!("{ISSUER}{acct}/idp").as_bytes()));
    assert_eq!(out.name_qualifier(), Some(nq.as_str()));
    assert_eq!(out.assumed_role_user().unwrap().arn(), format!("arn:aws:sts::{acct}:assumed-role/fed/alice"));
    let creds = Creds::from_sts(out.credentials().unwrap());
    // The session acts as the role, with `${saml:sub}` in its policy.
    let s3 = creds.s3(&e.t);
    s3.get_object().bucket("saml").key("alice@example.com/a.txt").send().await.unwrap();
    assert_eq!(code(s3.get_object().bucket("saml").key("bob@example.com/b.txt").send().await), "AccessDenied");
    let who = creds.sts(&e.t).get_caller_identity().send().await.unwrap();
    assert_eq!(who.arn(), Some(format!("arn:aws:sts::{acct}:assumed-role/fed/alice").as_str()));
    // Expires within the default hour, and within the IdP's session.
    let exp = out.credentials().unwrap().expiration().secs();
    assert!((exp - e.now_s() - 3600).abs() <= 5, "{exp}");
    let mut short = a.clone();
    short.session_not_on_or_after = Some(e.now_s() * 1000 + 1_200_000);
    let out = f.assume(&signed(&short, idp())).await.unwrap();
    assert!((out.credentials().unwrap().expiration().secs() - e.now_s() - 1200).abs() <= 5, "SessionNotOnOrAfter caps the session");
    // DurationSeconds up to the role's maximum.
    let r = sts(&e.t).assume_role_with_saml().role_arn(&f.role).principal_arn(&f.provider).saml_assertion(b64(&signed(&a, idp()))).duration_seconds(7201).send().await;
    assert_eq!(code(r), "ValidationError");
    // RSA-SHA1 signatures are accepted too.
    let sig = signature(idp(), &a.id, &a.xml(true, ""), true);
    f.assume(&response(ISSUER, "", &a.xml(true, &sig))).await.unwrap();
    // A signed Response (and an unsigned assertion inside it) works; both signed works.
    let sig = signature(idp(), "_r91", &canonical_response(ISSUER, &a.xml(true, "")), false);
    f.assume(&response(ISSUER, &sig, &a.xml(false, ""))).await.unwrap();
    let asig = signature(idp(), &a.id, &a.xml(true, ""), false);
    let rsig = signature(idp(), "_r91", &canonical_response(ISSUER, &a.xml(true, &asig)), false);
    f.assume(&response(ISSUER, &rsig, &a.xml(false, &asig))).await.unwrap();
    // A response signature doesn't verify once the assertion inside it changes.
    let tampered = response(ISSUER, &sig, &a.xml(false, "").replace("alice@example.com</saml:NameID>", "bob@example.com</saml:NameID>"));
    assert!(f.assume(&tampered).await.unwrap_err().contains("digest"));
}

#[tokio::test]
async fn saml_assertions_must_be_valid_and_for_this_role() {
    let f = fed().await;
    let e = &f.e;
    let a = A::new(e.now_s() * 1000, &f.role, &f.provider);
    let with = |g: &dyn Fn(&mut A)| {
        let mut x = a.clone();
        g(&mut x);
        signed(&x, idp())
    };
    // Times.
    f.refused(&with(&|x| x.not_on_or_after = e.now_s() * 1000 - 600_000), "ExpiredTokenException").await;
    f.refused(&with(&|x| x.not_before = e.now_s() * 1000 + 600_000), "ExpiredTokenException").await;
    // Who it's for, and from.
    f.refused(&with(&|x| x.audience = "https://elsewhere.example".into()), "Audience").await;
    f.refused(&with(&|x| x.recipient = "https://elsewhere.example".into()), "Recipient").await;
    f.refused(&with(&|x| x.issuer = "https://impostor.example".into()), "Issuer").await;
    // The Role attribute must pair this role with this provider; RoleSessionName is required.
    let other_role = f.role.replace("role/fed", "role/other");
    f.refused(&with(&|x| x.attrs[0].1 = vec![format!("{other_role},{}", f.provider)]), "AccessDenied").await;
    f.assume(&with(&|x| x.attrs[0].1 = vec![format!("{},{}", f.provider, f.role)])).await.expect("either order");
    f.assume(&with(&|x| x.attrs[0].1 = vec![format!("{other_role},{}", f.provider), format!("{},{}", f.role, f.provider)])).await.expect("one of several roles");
    f.refused(&with(&|x| x.attrs.retain(|(n, _)| !n.ends_with("RoleSessionName"))), "RoleSessionName").await;
    // The trust policy's conditions on SAML keys.
    f.refused(&with(&|x| x.attrs[3].1 = vec!["student".into()]), "AccessDenied").await;
    // A provider that's gone.
    let root = Creds::root();
    root.ok(e, &[("Action", "DeleteSAMLProvider"), ("SAMLProviderArn", &f.provider)]).await;
    f.refused(&signed(&a, idp()), "InvalidIdentityToken").await;
}

#[tokio::test]
async fn saml_signatures_resist_wrapping_and_tampering() {
    let f = fed().await;
    let e = &f.e;
    let a = A::new(e.now_s() * 1000, &f.role, &f.provider);
    let good = signed(&a, idp());
    f.assume(&good).await.unwrap();

    // Unsigned, or signed by someone else (even with their certificate in KeyInfo).
    f.refused(&response(ISSUER, "", &a.xml(true, "")), "isn't signed").await;
    f.refused(&signed(&a, evil()), "doesn't verify").await;

    // Tampering with anything signed: the subject, an attribute, a time.
    f.refused(&good.replace("alice@example.com</saml:NameID>", "bob@example.com</saml:NameID>"), "digest").await;
    f.refused(&good.replace("<saml:AttributeValue>staff</saml:AttributeValue>", "<saml:AttributeValue>admin</saml:AttributeValue>"), "digest").await;
    let mut later = a.clone();
    later.not_on_or_after += 3_600_000;
    f.refused(&good.replace(&iso(a.not_on_or_after), &iso(later.not_on_or_after)), "digest").await;

    // XSW: the signed assertion moved aside (inside Extensions) and a forged one in its place.
    let mut forged = a.clone();
    forged.id = "_forged".into();
    forged.subject = "bob@example.com".into();
    let sig = signature(idp(), &a.id, &a.xml(true, ""), false);
    let moved = response(ISSUER, "", &format!("<samlp:Extensions>{}</samlp:Extensions>{}", a.xml(true, &sig), forged.xml(true, "")));
    f.refused(&moved, "exactly one Assertion").await;
    // XSW: the forged assertion carries the signature of the real one, which it references
    // (the real one is elsewhere, so the reference isn't the signature's parent).
    let wrapped = response(ISSUER, "", &format!("{}<samlp:Extensions>{}</samlp:Extensions>", forged.xml(true, &sig), a.xml(true, "")));
    f.refused(&wrapped, "").await;
    let only_forged = response(ISSUER, "", &forged.xml(true, &sig));
    f.refused(&only_forged, "the Reference doesn't name the signed element").await;
    // XSW: a forged assertion with the same ID as the signed one.
    let mut same_id = forged.clone();
    same_id.id = a.id.clone();
    let dup = response(ISSUER, "", &format!("{}<samlp:Extensions>{}</samlp:Extensions>", same_id.xml(true, &sig), a.xml(true, "")));
    f.refused(&dup, "duplicate ID").await;
    // A signature nested where it signs nothing it may.
    let nested = response(ISSUER, "", &a.xml(true, "").replace("<saml:Subject>", &format!("<saml:Subject>{sig}")));
    f.refused(&nested, "may only sign the Response or the Assertion").await;
    // A response signature over the response, with a second assertion added after signing.
    let rsig = signature(idp(), "_r91", &canonical_response(ISSUER, &a.xml(true, "")), false);
    f.assume(&response(ISSUER, &rsig, &a.xml(false, ""))).await.expect("the response signature is good");
    f.refused(&response(ISSUER, &rsig, &format!("{}{}", a.xml(false, ""), forged.xml(true, ""))), "exactly one Assertion").await;

    // Comments are dropped by canonicalization, and a value is all of its text: a comment can't
    // cut the subject short (`alice@example.com.evil.test` stays itself).
    let mut long = a.clone();
    long.subject = "alice@example.com.evil.test".into();
    let ok = signed(&long, idp()).replace("alice@example.com.evil.test", "alice@example.com<!---->.evil.test");
    let out = f.assume(&ok).await.unwrap();
    assert_eq!(out.subject(), Some("alice@example.com.evil.test"));

    // No DTDs or entities, no processing instructions.
    f.refused(&format!("<!DOCTYPE r [<!ENTITY x \"alice\">]>{good}"), "DTD").await;
    f.refused(&good.replace("<samlp:Status>", "<?evil?><samlp:Status>"), "processing instructions").await;
    // Only exclusive canonicalization, enveloped then exclusive, one reference.
    f.refused(&good.replace(&format!("<ds:CanonicalizationMethod Algorithm=\"{EXC}\">"), "<ds:CanonicalizationMethod Algorithm=\"http://www.w3.org/TR/2001/REC-xml-c14n-20010315\">"), "exclusive canonicalization").await;
    f.refused(&good.replacen(&format!("<ds:Transform Algorithm=\"{ENVELOPED}\"></ds:Transform>"), "", 1), "transforms").await;
    f.refused(&good.replace(RSA_SHA256, "http://www.w3.org/2000/09/xmldsig#hmac-sha1"), "RSA-SHA256").await;
    // Encrypted assertions aren't supported.
    f.refused(&response(ISSUER, "", "<saml:EncryptedAssertion></saml:EncryptedAssertion>"), "Encrypted").await;
    // Not base64 / not XML.
    let r = sts(&e.t).assume_role_with_saml().role_arn(&f.role).principal_arn(&f.provider).saml_assertion("!!!!").send().await;
    assert_eq!(code(r), "InvalidIdentityToken");
}
