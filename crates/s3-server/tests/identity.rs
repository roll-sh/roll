//! IAM beyond users and roles: principals in policies checked against the metadata (and deleted
//! ones read as unique ids), login profiles and the password policy, account aliases, server and
//! signing certificates and SSH keys, the policy simulator (on the real evaluator), the
//! authorization details and the credential report, the dashboard's sign-in, and the MFA
//! throttle.

mod common;
#[path = "common/iamkit.rs"]
mod iamkit;

use aws_sdk_s3::primitives::ByteStream;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use iamkit::*;
use serde_json::{Value, json};

/// An IAM response as JSON (`s3_server::console::xml_json`): `<Action>Result`'s content.
fn result(body: &str, action: &str) -> Value {
    let body = body.split_once("?>").map_or(body, |(_, b)| b);
    s3_server::console::xml_json(body)[format!("{action}Response")][format!("{action}Result")].clone()
}

fn list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        x => vec![x.clone()],
    }
}

async fn get(c: &aws_sdk_s3::Client, b: &str, k: &str) -> Result<(), String> {
    c.get_object().bucket(b).key(k).send().await.map(|_| ()).map_err(|e| code::<(), _>(Err(e)))
}

async fn bucket_with(t: &TestServer, b: &str, k: &str) {
    t.client.create_bucket().bucket(b).send().await.unwrap();
    t.client.put_object().bucket(b).key(k).body(ByteStream::from_static(b"hi")).send().await.unwrap();
}

/// An IAM call from owned parameters.
async fn run(e: &Env, by: &Creds, p: &[(String, String)]) -> (u16, String) {
    by.iam(e, &p.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>()).await
}

fn owned(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
async fn principals_must_exist_and_deleted_ones_read_as_unique_ids() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let acct = e.account();
    bucket_with(t, "princ", "k").await;
    let alice = user(&e, "alice").await;
    let alice_arn = format!("arn:aws:iam::{acct}:user/alice");
    let alice_id = tag(&root.ok(&e, &[("Action", "GetUser"), ("UserName", "alice")]).await, "UserId");
    let stmt = |aws: Value| json!({"Version": "2012-10-17", "Statement": [{"Sid": "read", "Effect": "Allow", "Principal": {"AWS": aws}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::princ/*"}]}).to_string();
    let put = |doc: String| async move { t.client.put_bucket_policy().bucket("princ").policy(doc).send().await };
    let read = || async { t.client.get_bucket_policy().bucket("princ").send().await.unwrap().policy().unwrap().to_string() };

    // Principals that name nothing (or can't be principals) are refused, as on AWS.
    for bad in [
        format!("arn:aws:iam::{acct}:user/nobody"),
        format!("arn:aws:iam::{acct}:user/team/alice"),
        format!("arn:aws:iam::{acct}:role/nobody"),
        format!("arn:aws:iam::{acct}:group/admins"),
        "111122223333".into(),
        "arn:aws:iam::111122223333:root".into(),
        "arn:aws:sts::111122223333:assumed-role/r/s".into(),
        "RKNOSUCHKEY0000000000".into(),
    ] {
        assert_eq!(code(put(stmt(json!(bad))).await), "MalformedPolicy", "{bad}");
    }
    assert_eq!(message(put(stmt(json!(format!("arn:aws:iam::{acct}:user/nobody")))).await), "Invalid principal in policy");

    // A real user: the policy reads as put, and grants it.
    assert_eq!(get(&alice.s3(t), "princ", "k").await, Err("AccessDenied".into()));
    put(stmt(json!([alice_arn, acct, format!("arn:aws:iam::{acct}:root")]))).await.unwrap();
    assert!(read().await.contains(&alice_arn));
    get(&alice.s3(t), "princ", "k").await.unwrap();

    // Deleted, it reads as its unique id, which matches nobody: a new user of the same name
    // doesn't get the old one's access.
    root.ok(&e, &[("Action", "DeleteAccessKey"), ("UserName", "alice"), ("AccessKeyId", &alice.0)]).await;
    root.ok(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await;
    let p = read().await;
    assert!(p.contains(&alice_id) && !p.contains(&alice_arn), "{p}");
    let alice2 = user(&e, "alice").await;
    assert_eq!(get(&alice2.s3(t), "princ", "k").await, Err("AccessDenied".into()));
    assert!(read().await.contains(&alice_id), "still the old one's id");
    // A policy naming the new one grants it again.
    put(stmt(json!(alice_arn))).await.unwrap();
    get(&alice2.s3(t), "princ", "k").await.unwrap();

    // A renamed user reads with its new name (the policy names the user, not the name).
    let bob = user(&e, "bob").await;
    put(stmt(json!(format!("arn:aws:iam::{acct}:user/bob")))).await.unwrap();
    root.ok(&e, &[("Action", "UpdateUser"), ("UserName", "bob"), ("NewUserName", "robert"), ("NewPath", "/eng/")]).await;
    let p = read().await;
    assert!(p.contains(&format!("arn:aws:iam::{acct}:user/eng/robert")), "{p}");
    get(&bob.s3(t), "princ", "k").await.unwrap();

    // Trust policies: principals must exist; deleted ones read as unique ids.
    let carol = user(&e, "carol").await;
    let carol_id = tag(&root.ok(&e, &[("Action", "GetUser"), ("UserName", "carol")]).await, "UserId");
    let trust = |p: &str| json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": p}, "Action": "sts:AssumeRole"}]}).to_string();
    let (c, m) = root.fail_message(&e, &[("Action", "CreateRole"), ("RoleName", "deployer"), ("AssumeRolePolicyDocument", &trust(&format!("arn:aws:iam::{acct}:user/nobody")))]).await;
    assert_eq!(c, "MalformedPolicyDocument");
    assert_eq!(m, format!("Invalid principal in policy: &quot;AWS&quot;:&quot;arn:aws:iam::{acct}:user/nobody&quot;"));
    let carol_arn = format!("arn:aws:iam::{acct}:user/carol");
    let role = tag(&root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "deployer"), ("AssumeRolePolicyDocument", &trust(&carol_arn))]).await, "Arn");
    let assume = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "sts:AssumeRole", "Resource": role}]}).to_string();
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "carol"), ("PolicyName", "assume"), ("PolicyDocument", &assume)]).await;
    carol.sts(t).assume_role().role_arn(&role).role_session_name("c1").send().await.unwrap();
    let r = root.fail_message(&e, &[("Action", "UpdateAssumeRolePolicy"), ("RoleName", "deployer"), ("PolicyDocument", &trust(&format!("arn:aws:iam::{acct}:role/nobody")))]).await;
    assert_eq!(r.0, "MalformedPolicyDocument");
    root.ok(&e, &[("Action", "DeleteUserPolicy"), ("UserName", "carol"), ("PolicyName", "assume")]).await;
    root.ok(&e, &[("Action", "DeleteAccessKey"), ("UserName", "carol"), ("AccessKeyId", &carol.0)]).await;
    root.ok(&e, &[("Action", "DeleteUser"), ("UserName", "carol")]).await;
    let doc = decoded(&tag(&root.ok(&e, &[("Action", "GetRole"), ("RoleName", "deployer")]).await, "AssumeRolePolicyDocument"));
    assert!(doc.contains(&carol_id) && !doc.contains(&carol_arn), "{doc}");
    let carol2 = user(&e, "carol").await;
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "carol"), ("PolicyName", "assume"), ("PolicyDocument", &assume)]).await;
    assert_eq!(code(carol2.sts(t).assume_role().role_arn(&role).role_session_name("c2").send().await), "AccessDenied", "the new carol isn't the one trusted");
    // Federated principals: an OIDC provider that doesn't exist.
    let web = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Federated": format!("arn:aws:iam::{acct}:oidc-provider/nope.example")}, "Action": "sts:AssumeRoleWithWebIdentity"}]}).to_string();
    assert_eq!(root.fail(&e, &[("Action", "CreateRole"), ("RoleName", "web"), ("AssumeRolePolicyDocument", &web)]).await.1, "MalformedPolicyDocument");

    // Access point policies too.
    let ctl = control_client(t.port, ACCESS, SECRET);
    ctl.create_access_point().account_id(&acct).name("reader").bucket("princ").send().await.unwrap();
    let ap = format!("arn:aws:s3:us-east-1:{acct}:accesspoint/reader");
    let ap_policy = |p: &str| json!({"Statement": [{"Effect": "Allow", "Principal": {"AWS": p}, "Action": "s3:GetObject", "Resource": format!("{ap}/object/*")}]}).to_string();
    let r = ctl.put_access_point_policy().account_id(&acct).name("reader").policy(ap_policy(&format!("arn:aws:iam::{acct}:user/nobody"))).send().await;
    assert_eq!(code(r), "MalformedPolicy");
    ctl.put_access_point_policy().account_id(&acct).name("reader").policy(ap_policy(&alice_arn)).send().await.unwrap();
}

#[tokio::test]
async fn login_profiles_and_the_password_policy() {
    let e = env().await;
    let root = Creds::root();
    let alice = user(&e, "alice").await;
    let lp = |pw: &'static str| [("Action", "CreateLoginProfile"), ("UserName", "alice"), ("Password", pw)];
    // AWS's default policy: 8 characters, three kinds of characters.
    let (c, m) = root.fail_message(&e, &lp("short1A")).await;
    assert_eq!((c.as_str(), m.as_str()), ("PasswordPolicyViolation", "Password should have a minimum length of 8"));
    assert_eq!(root.fail(&e, &lp("alllowercase")).await, (400, "PasswordPolicyViolation".into()));
    let out = root.ok(&e, &[("Action", "CreateLoginProfile"), ("UserName", "alice"), ("Password", "Correct-Horse-9"), ("PasswordResetRequired", "true")]).await;
    assert_eq!(tag(&out, "PasswordResetRequired"), "true");
    assert!(!out.contains("Correct-Horse-9"), "a password never comes back");
    assert_eq!(root.fail(&e, &lp("Correct-Horse-9")).await, (409, "EntityAlreadyExists".into()));
    let got = root.ok(&e, &[("Action", "GetLoginProfile"), ("UserName", "alice")]).await;
    assert_eq!(tag(&got, "UserName"), "alice");
    assert_eq!(root.fail(&e, &[("Action", "GetLoginProfile"), ("UserName", "nobody")]).await.1, "NoSuchEntity");
    // The account's own policy.
    assert_eq!(root.fail(&e, &[("Action", "GetAccountPasswordPolicy")]).await, (404, "NoSuchEntity".into()));
    assert_eq!(root.fail(&e, &[("Action", "UpdateAccountPasswordPolicy"), ("MinimumPasswordLength", "5")]).await.1, "ValidationError");
    root.ok(&e, &[("Action", "UpdateAccountPasswordPolicy"), ("MinimumPasswordLength", "12"), ("RequireSymbols", "true"), ("RequireNumbers", "true"), ("PasswordReusePrevention", "2"), ("AllowUsersToChangePassword", "true"), ("MaxPasswordAge", "90")]).await;
    let pol = result(&root.ok(&e, &[("Action", "GetAccountPasswordPolicy")]).await, "GetAccountPasswordPolicy");
    assert_eq!(
        pol["PasswordPolicy"],
        json!({"MinimumPasswordLength": "12", "RequireSymbols": "true", "RequireNumbers": "true", "RequireUppercaseCharacters": "false", "RequireLowercaseCharacters": "false", "AllowUsersToChangePassword": "true", "ExpirePasswords": "true", "MaxPasswordAge": "90", "PasswordReusePrevention": "2"})
    );
    let (c, m) = root.fail_message(&e, &[("Action", "UpdateLoginProfile"), ("UserName", "alice"), ("Password", "no-numbers-here!")]).await;
    assert_eq!((c.as_str(), m.as_str()), ("PasswordPolicyViolation", "Password should have at least one number"));
    // A user changes its own password: the old one must be right, the new one by the policy and
    // not one of the last two.
    let change = |old: &str, new: &str| owned(&[("Action", "ChangePassword"), ("OldPassword", old), ("NewPassword", new)]);
    assert_eq!(error_code(&run(&e, &alice, &change("wrong-Password-1", "New-Password-12")).await.1), "AccessDenied");
    assert_eq!(error_code(&run(&e, &alice, &change("Correct-Horse-9", "short-1!")).await.1), "PasswordPolicyViolation");
    assert_eq!(error_code(&run(&e, &alice, &change("Correct-Horse-9", "Correct-Horse-9")).await.1), "PasswordPolicyViolation", "not the current one");
    assert_eq!(run(&e, &alice, &change("Correct-Horse-9", "Battery-Staple-12")).await.0, 200);
    assert!(root.ok(&e, &[("Action", "GetLoginProfile"), ("UserName", "alice")]).await.contains("<PasswordResetRequired>false</PasswordResetRequired>"), "changing it clears the reset");
    assert_eq!(error_code(&run(&e, &alice, &change("Battery-Staple-12", "Correct-Horse-9")).await.1), "PasswordPolicyViolation", "PasswordReusePrevention");
    // Without AllowUsersToChangePassword it takes iam:ChangePassword.
    root.ok(&e, &[("Action", "UpdateAccountPasswordPolicy"), ("MinimumPasswordLength", "12")]).await;
    assert_eq!(error_code(&run(&e, &alice, &change("Battery-Staple-12", "Another-Pass-34")).await.1), "AccessDenied");
    // The root user has no password to change here.
    let (s, b) = root.iam(&e, &[("Action", "ChangePassword"), ("OldPassword", "a"), ("NewPassword", "b")]).await;
    assert_eq!((s, error_code(&b)), (400, "InvalidUserType".into()));
    // A user with a password can't be deleted until it's gone.
    root.ok(&e, &[("Action", "DeleteAccessKey"), ("UserName", "alice"), ("AccessKeyId", &alice.0)]).await;
    assert_eq!(root.fail(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await, (409, "DeleteConflict".into()));
    root.ok(&e, &[("Action", "DeleteLoginProfile"), ("UserName", "alice")]).await;
    assert_eq!(root.fail(&e, &[("Action", "DeleteLoginProfile"), ("UserName", "alice")]).await.1, "NoSuchEntity");
    root.ok(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await;
    root.ok(&e, &[("Action", "DeleteAccountPasswordPolicy")]).await;
    assert_eq!(root.fail(&e, &[("Action", "DeleteAccountPasswordPolicy")]).await, (404, "NoSuchEntity".into()));
}

#[tokio::test]
async fn account_aliases() {
    let e = env().await;
    let root = Creds::root();
    let (_, other) = other_account(&e.t, "other").await;
    assert!(!root.ok(&e, &[("Action", "ListAccountAliases")]).await.contains("<member>"));
    for bad in ["ab", "Has-Upper", "a--b", "-start", "123456789012"] {
        assert_eq!(root.fail(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", bad)]).await.1, "ValidationError", "{bad}");
    }
    root.ok(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "acme")]).await;
    root.ok(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "acme")]).await;
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListAccountAliases")]).await, "member"), vec!["acme"]);
    // Unique across the deployment.
    assert_eq!(other.fail(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "acme")]).await, (409, "EntityAlreadyExists".into()));
    // One per account: a new one replaces it.
    root.ok(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "acme-prod")]).await;
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListAccountAliases")]).await, "member"), vec!["acme-prod"]);
    assert_eq!(e.t.store.account_by_alias("acme-prod").unwrap(), Some(e.account()));
    other.ok(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "acme")]).await;
    assert_eq!(root.fail(&e, &[("Action", "DeleteAccountAlias"), ("AccountAlias", "acme")]).await, (404, "NoSuchEntity".into()), "another account's");
    root.ok(&e, &[("Action", "DeleteAccountAlias"), ("AccountAlias", "acme-prod")]).await;
    assert_eq!(e.t.store.account_alias(&e.account()).unwrap(), None);
    let alice = user(&e, "alice").await;
    assert_eq!(alice.fail(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "mine")]).await.1, "AccessDenied");
}

fn cert(key: &rcgen::KeyPair, expired: bool) -> String {
    let mut p = rcgen::CertificateParams::new(vec!["www.example.test".into()]).unwrap();
    if expired {
        p.not_before = rcgen::date_time_ymd(2020, 1, 1);
        p.not_after = rcgen::date_time_ymd(2021, 1, 1);
    }
    p.self_signed(key).unwrap().pem()
}

fn rsa_pair() -> (rsa::RsaPrivateKey, rcgen::KeyPair) {
    use rsa::pkcs8::EncodePrivateKey;
    let k = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let pem = k.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let kp = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&pem, &rcgen::PKCS_RSA_SHA256).unwrap();
    (k, kp)
}

fn ssh_rsa(k: &rsa::RsaPrivateKey) -> String {
    use rsa::traits::PublicKeyParts;
    let mpint = |b: Vec<u8>| if b[0] & 0x80 != 0 { [vec![0], b].concat() } else { b };
    let mut blob = Vec::new();
    for part in [b"ssh-rsa".to_vec(), mpint(k.e().to_bytes_be()), mpint(k.n().to_bytes_be())] {
        blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        blob.extend_from_slice(&part);
    }
    format!("ssh-rsa {} me@laptop", STANDARD.encode(blob))
}

#[tokio::test]
async fn certificates_and_ssh_keys() {
    let e = env().await;
    let root = Creds::root();
    let acct = e.account();
    let ec = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (rsa_key, rsa_kp) = rsa_pair();
    let ec_cert = cert(&ec, false);
    let rsa_cert = cert(&rsa_kp, false);
    let upload = |name: &str, body: &str, key: &str| owned(&[("Action", "UploadServerCertificate"), ("ServerCertificateName", name), ("CertificateBody", body), ("PrivateKey", key)]);
    // Server certificates: valid, and with the private key that matches.
    assert_eq!(error_code(&run(&e, &root, &upload("web", &ec_cert, &other.serialize_pem())).await.1), "KeyPairMismatch");
    assert_eq!(error_code(&run(&e, &root, &upload("web", "not a certificate", &ec.serialize_pem())).await.1), "MalformedCertificate");
    assert_eq!(error_code(&run(&e, &root, &upload("web", &cert(&ec, true), &ec.serialize_pem())).await.1), "MalformedCertificate", "expired");
    assert_eq!(error_code(&run(&e, &root, &upload("web", &ec_cert, "garbage")).await.1), "MalformedCertificate");
    let (s, out) = run(&e, &root, &upload("web", &ec_cert, &ec.serialize_pem())).await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(tag(&out, "Arn"), format!("arn:aws:iam::{acct}:server-certificate/web"));
    assert!(tag(&out, "ServerCertificateId").starts_with("ASCA"));
    assert!(out.contains("<Expiration>"));
    let (s, out) = run(&e, &root, &upload("rsa", &rsa_cert, &rsa_kp.serialize_pem())).await;
    assert_eq!(s, 200, "an RSA key (PKCS#8): {out}");
    {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let pkcs1 = rsa_key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let (s, out) = run(&e, &root, &upload("rsa1", &rsa_cert, &pkcs1)).await;
        assert_eq!(s, 200, "an RSA key (PKCS#1): {out}");
    }
    assert_eq!(error_code(&run(&e, &root, &upload("web", &ec_cert, &ec.serialize_pem())).await.1), "EntityAlreadyExists");
    let got = root.ok(&e, &[("Action", "GetServerCertificate"), ("ServerCertificateName", "web")]).await;
    assert!(got.contains("BEGIN CERTIFICATE") && !got.contains("PRIVATE KEY"), "the key is never returned");
    assert_eq!(tags(&root.ok(&e, &[("Action", "ListServerCertificates")]).await, "ServerCertificateName"), vec!["rsa", "rsa1", "web"]);
    root.ok(&e, &[("Action", "UpdateServerCertificate"), ("ServerCertificateName", "web"), ("NewServerCertificateName", "www"), ("NewPath", "/cloudfront/")]).await;
    let got = root.ok(&e, &[("Action", "GetServerCertificate"), ("ServerCertificateName", "www")]).await;
    assert_eq!(tag(&got, "Arn"), format!("arn:aws:iam::{acct}:server-certificate/cloudfront/www"));
    root.ok(&e, &[("Action", "DeleteServerCertificate"), ("ServerCertificateName", "www")]).await;
    assert_eq!(root.fail(&e, &[("Action", "GetServerCertificate"), ("ServerCertificateName", "www")]).await.1, "NoSuchEntity");

    // Signing certificates: two per user, none twice in the account.
    root.ok(&e, &[("Action", "CreateUser"), ("UserName", "alice")]).await;
    let sc = |body: &str| owned(&[("Action", "UploadSigningCertificate"), ("UserName", "alice"), ("CertificateBody", body)]);
    let (s, out) = run(&e, &root, &sc(&ec_cert)).await;
    assert_eq!(s, 200, "{out}");
    let id = tag(&out, "CertificateId");
    assert_eq!(tag(&out, "Status"), "Active");
    assert_eq!(error_code(&run(&e, &root, &sc(&ec_cert)).await.1), "DuplicateCertificate");
    assert_eq!(run(&e, &root, &sc(&rsa_cert)).await.0, 200);
    let third = cert(&other, false);
    assert_eq!(error_code(&run(&e, &root, &sc(&third)).await.1), "LimitExceeded");
    assert_eq!(error_code(&run(&e, &root, &sc("nope")).await.1), "MalformedCertificate");
    root.ok(&e, &[("Action", "UpdateSigningCertificate"), ("UserName", "alice"), ("CertificateId", &id), ("Status", "Inactive")]).await;
    let l = root.ok(&e, &[("Action", "ListSigningCertificates"), ("UserName", "alice")]).await;
    assert_eq!(tags(&l, "CertificateId").len(), 2);
    assert!(l.contains("<Status>Inactive</Status>"));
    // The root user's own, without UserName.
    assert_eq!(root.iam(&e, &[("Action", "UploadSigningCertificate"), ("CertificateBody", &third)]).await.0, 200);
    assert!(root.ok(&e, &[("Action", "GetAccountSummary")]).await.contains("<key>AccountSigningCertificatesPresent</key><value>1</value>"));

    // SSH public keys: RSA, at least 2048 bits, in either encoding.
    let body = ssh_rsa(&rsa_key);
    let out = root.ok(&e, &[("Action", "UploadSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyBody", &body)]).await;
    let kid = tag(&out, "SSHPublicKeyId");
    assert!(kid.starts_with("APKA"));
    assert_eq!(tag(&out, "Fingerprint").split(':').count(), 16);
    assert_eq!(root.fail(&e, &[("Action", "UploadSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyBody", &body)]).await.1, "DuplicateSSHPublicKey");
    let small = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
    assert_eq!(root.fail(&e, &[("Action", "UploadSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyBody", &ssh_rsa(&small))]).await.1, "InvalidPublicKey");
    assert_eq!(root.fail(&e, &[("Action", "UploadSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyBody", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA me")]).await.1, "UnrecognizedPublicKeyEncoding");
    let pem = root.ok(&e, &[("Action", "GetSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyId", &kid), ("Encoding", "PEM")]).await;
    assert!(tag(&pem, "SSHPublicKeyBody").starts_with("-----BEGIN PUBLIC KEY-----"));
    let ssh = root.ok(&e, &[("Action", "GetSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyId", &kid), ("Encoding", "SSH")]).await;
    assert!(body.starts_with(&tag(&ssh, "SSHPublicKeyBody")), "the same key");
    root.ok(&e, &[("Action", "UpdateSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyId", &kid), ("Status", "Inactive")]).await;
    assert!(root.ok(&e, &[("Action", "ListSSHPublicKeys"), ("UserName", "alice")]).await.contains("<Status>Inactive</Status>"));
    // What a user holds keeps it from being deleted.
    assert_eq!(root.fail(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await.1, "DeleteConflict");
    root.ok(&e, &[("Action", "DeleteSSHPublicKey"), ("UserName", "alice"), ("SSHPublicKeyId", &kid)]).await;
    for c in tags(&root.ok(&e, &[("Action", "ListSigningCertificates"), ("UserName", "alice")]).await, "CertificateId") {
        root.ok(&e, &[("Action", "DeleteSigningCertificate"), ("UserName", "alice"), ("CertificateId", &c)]).await;
    }
    root.ok(&e, &[("Action", "DeleteUser"), ("UserName", "alice")]).await;
}

#[tokio::test]
async fn the_policy_simulator_decides_as_requests_are_decided() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let acct = e.account();
    bucket_with(t, "sim", "a/k").await;
    let alice = user(&e, "alice").await;
    let alice_arn = format!("arn:aws:iam::{acct}:user/alice");
    let inline = json!({"Version": "2012-10-17", "Statement": [
        {"Sid": "ReadFromOffice", "Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::sim/*", "Condition": {"IpAddress": {"aws:SourceIp": "127.0.0.0/8"}}},
        {"Sid": "NoDeletes", "Effect": "Deny", "Action": "s3:DeleteObject", "Resource": "*"},
        {"Sid": "List", "Effect": "Allow", "Action": "s3:ListBucket", "Resource": "arn:aws:s3:::sim"}
    ]});
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "office"), ("PolicyDocument", &inline.to_string())]).await;
    root.ok(&e, &[("Action", "CreateGroup"), ("GroupName", "writers")]).await;
    root.ok(&e, &[("Action", "AddUserToGroup"), ("GroupName", "writers"), ("UserName", "alice")]).await;
    root.ok(&e, &[("Action", "AttachGroupPolicy"), ("GroupName", "writers"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3FullAccess")]).await;
    let principal = owned(&[
        ("Action", "SimulatePrincipalPolicy"),
        ("PolicySourceArn", &alice_arn),
        ("ActionNames.member.1", "s3:GetObject"),
        ("ActionNames.member.2", "s3:DeleteObject"),
        ("ActionNames.member.3", "iam:CreateUser"),
        ("ResourceArns.member.1", "arn:aws:s3:::sim/a/k"),
    ]);
    let results = |body: &str, action: &str| list(&result(body, action)["EvaluationResults"]);
    let (s, body) = run(&e, &root, &principal).await;
    assert_eq!(s, 200, "{body}");
    let r = results(&body, "SimulatePrincipalPolicy");
    assert_eq!(r.len(), 3);
    // GetObject: the group's AmazonS3FullAccess allows it whatever the IP.
    assert_eq!(r[0]["EvalDecision"], "allowed");
    let matched = list(&r[0]["MatchedStatements"]);
    assert!(matched.iter().any(|m| m["SourcePolicyId"] == "AmazonS3FullAccess" && m["SourcePolicyType"] == "aws-managed"), "{matched:?}");
    assert_eq!(list(&r[0]["MissingContextValues"]), vec![json!("aws:SourceIp")], "the inline statement's condition needs it");
    // DeleteObject: the inline Deny wins.
    assert_eq!(r[1]["EvalDecision"], "explicitDeny");
    let m = &list(&r[1]["MatchedStatements"])[0];
    assert_eq!((m["SourcePolicyId"].as_str(), m["SourcePolicyType"].as_str()), (Some("office"), Some("user")));
    assert_eq!(m["StartPosition"]["Line"], "1");
    // iam:CreateUser: nothing allows it.
    assert_eq!(r[2]["EvalDecision"], "implicitDeny");
    assert_eq!(r[2]["MatchedStatements"], Value::Null);

    // A permissions boundary allowing only reads, as input.
    let boundary = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:Get*", "Resource": "*"}]}).to_string();
    let mut bounded = principal.clone();
    bounded.push(("PermissionsBoundaryPolicyInputList.member.1".into(), boundary));
    let r = results(&run(&e, &root, &bounded).await.1, "SimulatePrincipalPolicy");
    assert_eq!(r[0]["EvalDecision"], "allowed");
    assert_eq!(r[0]["PermissionsBoundaryDecisionDetail"]["AllowedByPermissionsBoundary"], "true");
    assert_eq!(r[2]["EvalDecision"], "implicitDeny");
    assert_eq!(r[2]["PermissionsBoundaryDecisionDetail"]["AllowedByPermissionsBoundary"], "false");

    // The same decisions as real requests.
    get(&alice.s3(t), "sim", "a/k").await.unwrap();
    assert_eq!(code(alice.s3(t).delete_object().bucket("sim").key("a/k").send().await), "AccessDenied");

    // A custom policy: conditions, context entries, positions in the text, a resource policy.
    let custom = "{\n  \"Version\": \"2012-10-17\",\n  \"Statement\": [\n    {\n      \"Effect\": \"Allow\",\n      \"Action\": \"s3:PutObject\",\n      \"Resource\": \"arn:aws:s3:::sim/${aws:username}/*\",\n      \"Condition\": {\"StringEquals\": {\"s3:x-amz-acl\": \"private\"}}\n    }\n  ]\n}";
    let custom_sim = |ctx: &[(&str, &str)]| {
        let mut p = owned(&[("Action", "SimulateCustomPolicy"), ("PolicyInputList.member.1", custom), ("ActionNames.member.1", "s3:PutObject"), ("ResourceArns.member.1", "arn:aws:s3:::sim/alice/x")]);
        for (i, (k, v)) in ctx.iter().enumerate() {
            p.push((format!("ContextEntries.member.{}.ContextKeyName", i + 1), k.to_string()));
            p.push((format!("ContextEntries.member.{}.ContextKeyType", i + 1), "string".into()));
            p.push((format!("ContextEntries.member.{}.ContextKeyValues.member.1", i + 1), v.to_string()));
        }
        p
    };
    let one = |body: String| results(&body, "SimulateCustomPolicy").remove(0);
    let r = one(run(&e, &root, &custom_sim(&[])).await.1);
    assert_eq!(r["EvalDecision"], "implicitDeny");
    let mut missing: Vec<Value> = list(&r["MissingContextValues"]);
    missing.sort_by_key(|v| v.to_string());
    assert_eq!(missing, vec![json!("aws:username"), json!("s3:x-amz-acl")]);
    let r = one(run(&e, &root, &custom_sim(&[("aws:username", "alice"), ("s3:x-amz-acl", "private")])).await.1);
    assert_eq!(r["EvalDecision"], "allowed");
    let m = &list(&r["MatchedStatements"])[0];
    assert_eq!((m["SourcePolicyId"].as_str(), m["SourcePolicyType"].as_str()), (Some("PolicyInputList.1"), Some("none")));
    assert_eq!((m["StartPosition"]["Line"].as_str(), m["StartPosition"]["Column"].as_str()), (Some("4"), Some("5")));
    assert_eq!((m["EndPosition"]["Line"].as_str(), m["EndPosition"]["Column"].as_str()), (Some("9"), Some("6")));
    let r = one(run(&e, &root, &custom_sim(&[("aws:username", "bob"), ("s3:x-amz-acl", "private")])).await.1);
    assert_eq!(r["EvalDecision"], "implicitDeny", "the policy variable decides the resource");
    // A resource policy alone grants a principal of the same account ...
    let rp = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": alice_arn}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::sim/*"}]}).to_string();
    let nothing = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:ListBucket", "Resource": "*"}]}).to_string();
    let with_rp = owned(&[("Action", "SimulateCustomPolicy"), ("PolicyInputList.member.1", &nothing), ("ActionNames.member.1", "s3:GetObject"), ("ResourceArns.member.1", "arn:aws:s3:::sim/k"), ("ResourcePolicy", &rp), ("CallerArn", &alice_arn)]);
    let r = one(run(&e, &root, &with_rp).await.1);
    assert_eq!(r["EvalDecision"], "allowed");
    assert_eq!(list(&r["MatchedStatements"])[0]["SourcePolicyType"], "resource");
    assert_eq!(r["EvalDecisionDetails"]["Resource Policy"], "allowed");
    // ... but not one of another account, which needs its own side too.
    let mut cross = with_rp.clone();
    cross.push(("ResourceOwner".into(), "arn:aws:iam::999999999999:root".into()));
    assert_eq!(one(run(&e, &root, &cross).await.1)["EvalDecision"], "implicitDeny");
    // Context keys a policy needs.
    let body = root.ok(&e, &[("Action", "GetContextKeysForCustomPolicy"), ("PolicyInputList.member.1", custom)]).await;
    let mut keys = list(&result(&body, "GetContextKeysForCustomPolicy")["ContextKeyNames"]);
    keys.sort_by_key(|v| v.to_string());
    assert_eq!(keys, vec![json!("aws:username"), json!("s3:x-amz-acl")]);
    let body = root.ok(&e, &[("Action", "GetContextKeysForPrincipalPolicy"), ("PolicySourceArn", &alice_arn)]).await;
    assert_eq!(list(&result(&body, "GetContextKeysForPrincipalPolicy")["ContextKeyNames"]), vec![json!("aws:SourceIp")]);
    // Errors.
    assert_eq!(root.fail(&e, &[("Action", "SimulatePrincipalPolicy"), ("PolicySourceArn", &format!("arn:aws:iam::{acct}:user/nobody")), ("ActionNames.member.1", "s3:GetObject")]).await.1, "NoSuchEntity");
    assert_eq!(root.fail(&e, &[("Action", "SimulateCustomPolicy"), ("PolicyInputList.member.1", "{"), ("ActionNames.member.1", "s3:GetObject")]).await.1, "MalformedPolicyDocument");
    assert_eq!(alice.fail(&e, &[("Action", "SimulateCustomPolicy"), ("PolicyInputList.member.1", &nothing), ("ActionNames.member.1", "s3:GetObject")]).await.1, "AccessDenied", "AmazonS3FullAccess has no iam:*");
}

#[tokio::test]
async fn authorization_details_and_the_credential_report() {
    let e = env().await;
    let root = Creds::root();
    let acct = e.account();
    let alice = user(&e, "alice").await;
    root.ok(&e, &[("Action", "CreateLoginProfile"), ("UserName", "alice"), ("Password", "Correct-Horse-9")]).await;
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "p"), ("PolicyDocument", r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:*","Resource":"*"}]}"#)]).await;
    root.ok(&e, &[("Action", "CreateGroup"), ("GroupName", "g")]).await;
    root.ok(&e, &[("Action", "AttachGroupPolicy"), ("GroupName", "g"), ("PolicyArn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess")]).await;
    root.ok(&e, &[("Action", "CreatePolicy"), ("PolicyName", "mine"), ("PolicyDocument", r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:GetObject","Resource":"*"}]}"#)]).await;
    let trust = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{acct}:root")}, "Action": "sts:AssumeRole"}]}).to_string();
    root.ok(&e, &[("Action", "CreateRole"), ("RoleName", "r"), ("AssumeRolePolicyDocument", &trust)]).await;
    let d = result(&root.ok(&e, &[("Action", "GetAccountAuthorizationDetails")]).await, "GetAccountAuthorizationDetails");
    let users = list(&d["UserDetailList"]);
    assert_eq!(users[0]["UserName"], "alice");
    assert!(list(&users[0]["UserPolicyList"])[0]["PolicyDocument"].as_str().unwrap().contains("s3:*"), "documents are decoded");
    assert_eq!(list(&d["GroupDetailList"])[0]["AttachedManagedPolicies"][0]["PolicyName"], "AmazonS3ReadOnlyAccess");
    assert_eq!(list(&d["RoleDetailList"])[0]["RoleName"], "r");
    let names: Vec<String> = list(&d["Policies"]).iter().map(|p| p["PolicyName"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["mine", "AmazonS3ReadOnlyAccess"], "the account's, and AWS's that are attached");
    let only = result(&root.ok(&e, &[("Action", "GetAccountAuthorizationDetails"), ("Filter.member.1", "Role")]).await, "GetAccountAuthorizationDetails");
    assert!(only["UserDetailList"].is_null() && list(&only["RoleDetailList"]).len() == 1);
    let paged = result(&root.ok(&e, &[("Action", "GetAccountAuthorizationDetails"), ("MaxItems", "2")]).await, "GetAccountAuthorizationDetails");
    assert_eq!(paged["IsTruncated"], "true");

    // The credential report: STARTED, then COMPLETE; a CSV like AWS's.
    assert_eq!(root.fail(&e, &[("Action", "GetCredentialReport")]).await, (410, "ReportNotPresent".into()));
    assert_eq!(tag(&root.ok(&e, &[("Action", "GenerateCredentialReport")]).await, "State"), "STARTED");
    assert_eq!(tag(&root.ok(&e, &[("Action", "GenerateCredentialReport")]).await, "State"), "COMPLETE");
    let r = root.ok(&e, &[("Action", "GetCredentialReport")]).await;
    assert_eq!(tag(&r, "ReportFormat"), "text/csv");
    let csv = String::from_utf8(STANDARD.decode(tag(&r, "Content")).unwrap()).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert!(lines[0].starts_with("user,arn,user_creation_time,password_enabled,password_last_used,password_last_changed,password_next_rotation,mfa_active,access_key_1_active"));
    assert_eq!(lines[0].split(',').count(), 22);
    assert!(lines[1].starts_with(&format!("<root_account>,arn:aws:iam::{acct}:root,")));
    let a: Vec<&str> = lines[2].split(',').collect();
    let arn = format!("arn:aws:iam::{acct}:user/alice");
    assert_eq!((a[0], a[1], a[3], a[7], a[8]), ("alice", arn.as_str(), "true", "false", "true"));
    assert_eq!(a.len(), 22);
    // It expires after four hours.
    e.advance_s(4 * 3600);
    assert_eq!(root.fail(&e, &[("Action", "GetCredentialReport")]).await, (410, "ReportExpired".into()));
    assert_eq!(alice.fail(&e, &[("Action", "GenerateCredentialReport")]).await.1, "AccessDenied");
}

#[tokio::test]
async fn dashboard_sign_in_acts_as_the_user() {
    use s3_server::console;
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let acct = e.account();
    bucket_with(t, "dash", "k").await;
    root.ok(&e, &[("Action", "CreateUser"), ("UserName", "alice")]).await;
    let s = &e.server;
    let code_of = |r: Result<console::SessionInfo, console::ConsoleError>| r.err().map(|e| e.code).unwrap_or_default();
    // No password yet: the same answer as a wrong one.
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, None).await), "InvalidCredentials");
    root.ok(&e, &[("Action", "CreateLoginProfile"), ("UserName", "alice"), ("Password", "Correct-Horse-9")]).await;
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "wrong-Horse-9", None, None).await), "InvalidCredentials");
    assert_eq!(code_of(console::sign_in(s, &acct, "nobody", "Correct-Horse-9", None, None).await), "InvalidCredentials");
    assert_eq!(code_of(console::sign_in(s, "999999999999", "alice", "Correct-Horse-9", None, None).await), "InvalidCredentials");
    let signed = console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, None).await.unwrap();
    assert_eq!((signed.user.as_str(), signed.arn.as_str(), signed.mfa), ("alice", format!("arn:aws:iam::{acct}:user/alice").as_str(), false));
    let token = signed.token.clone().unwrap();
    // By alias too.
    root.ok(&e, &[("Action", "CreateAccountAlias"), ("AccountAlias", "dash-co")]).await;
    let by_alias = console::sign_in(s, "dash-co", "alice", "Correct-Horse-9", None, None).await.unwrap();
    assert_eq!(by_alias.account_alias.as_deref(), Some("dash-co"));

    // The session is the user's: no permissions until a policy gives some.
    let session = console::session(s, &token).unwrap();
    let denied = session.authorize(s, console::Origin::default(), "s3:GetObject", Some("dash"), Some("k"), false).unwrap_err();
    assert_eq!((denied.status, denied.code.as_str()), (403, "AccessDenied"));
    root.ok(&e, &[("Action", "PutUserPolicy"), ("UserName", "alice"), ("PolicyName", "read"), ("PolicyDocument", r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","iam:GetUser"],"Resource":"*"}]}"#)]).await;
    // Each control-plane request checks the token again, with the user as it is now.
    let session = console::session(s, &token).unwrap();
    session.authorize(s, console::Origin::default(), "s3:GetObject", Some("dash"), Some("k"), false).unwrap();
    assert!(session.authorize(s, console::Origin::default(), "s3:PutObject", Some("dash"), Some("k"), false).is_err());
    // A bucket policy can grant it too (it's the user's ARN to the evaluator).
    let bp = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{acct}:user/alice")}, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::dash/*"}]});
    t.client.put_bucket_policy().bucket("dash").policy(bp.to_string()).send().await.unwrap();
    session.authorize(s, console::Origin::default(), "s3:PutObject", Some("dash"), Some("up"), false).unwrap();
    // IAM as the user.
    let me = console::iam(s, console::Caller::Session(&session), None, None, owned(&[("Action", "GetUser")])).await.unwrap();
    assert_eq!(me["User"]["UserName"], "alice");
    let no = console::iam(s, console::Caller::Session(&session), None, None, owned(&[("Action", "ListUsers")])).await.unwrap_err();
    assert_eq!(no.code, "AccessDenied");
    // ... and as the operator (the account's root user).
    let all = console::iam(s, console::Caller::Root(&acct), None, None, owned(&[("Action", "ListUsers")])).await.unwrap();
    assert_eq!(all["Users"][0]["UserName"], "alice");
    // Bearer tokens are only dashboard sessions, and only whole.
    assert_eq!(console::session(s, "nonsense").unwrap_err().code, "InvalidToken");
    assert_eq!(console::session(s, &token[..token.len() - 2]).unwrap_err().code, "InvalidToken");
    let other = root.sts(t).get_session_token().send().await.unwrap();
    let c = other.credentials().unwrap();
    assert_eq!(console::session(s, &format!("{}.{}", c.access_key_id(), c.session_token())).unwrap_err().code, "InvalidToken", "not a dashboard session");
    // A session ends with its password (and its user), and after 12 hours.
    root.ok(&e, &[("Action", "DeleteLoginProfile"), ("UserName", "alice")]).await;
    assert_eq!(console::session(s, &token).unwrap_err().code, "InvalidToken");
    root.ok(&e, &[("Action", "CreateLoginProfile"), ("UserName", "alice"), ("Password", "Correct-Horse-9"), ("PasswordResetRequired", "true")]).await;
    let late = console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, Some("Battery-Staple-1")).await.unwrap();
    e.advance_s(12 * 3600 + 1);
    assert_eq!(console::session(s, late.token.as_deref().unwrap()).unwrap_err().code, "ExpiredToken");

    // A password that must be reset is changed at sign-in, by the policy.
    root.ok(&e, &[("Action", "UpdateLoginProfile"), ("UserName", "alice"), ("Password", "Correct-Horse-9"), ("PasswordResetRequired", "true")]).await;
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, None).await), "PasswordResetRequired");
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, Some("weak")).await), "PasswordPolicyViolation");
    console::sign_in(s, &acct, "alice", "Correct-Horse-9", None, Some("Fresh-Password-77")).await.unwrap();
    console::sign_in(s, &acct, "alice", "Fresh-Password-77", None, None).await.unwrap();

    // With an MFA device, a code is needed (and used once).
    let (_, seed) = mfa_device(&e, &root, "alice-phone", Some("alice")).await;
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Fresh-Password-77", None, None).await), "MfaRequired");
    let wrong = if totp(&seed, e.now_s()) == "000000" { "111111" } else { "000000" };
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Fresh-Password-77", Some(wrong), None).await), "InvalidMfaCode");
    let code = totp(&seed, e.now_s());
    let with_mfa = console::sign_in(s, &acct, "alice", "Fresh-Password-77", Some(&code), None).await.unwrap();
    assert!(with_mfa.mfa);
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Fresh-Password-77", Some(&code), None).await), "InvalidMfaCode", "a code works once");

    // Ten wrong passwords lock signing in for five minutes, the right one too.
    for _ in 0..10 {
        assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Wrong-Password-1", None, None).await), "InvalidCredentials");
    }
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Fresh-Password-77", None, None).await), "Throttled");
    e.advance_s(301);
    assert_eq!(code_of(console::sign_in(s, &acct, "alice", "Fresh-Password-77", None, None).await), "MfaRequired");
}

#[tokio::test]
async fn wrong_mfa_codes_lock_a_device_durably() {
    let e = env().await;
    let t = &e.t;
    let root = Creds::root();
    let alice = user(&e, "alice").await;
    let (serial, seed) = mfa_device(&e, &root, "alice", Some("alice")).await;
    let try_code = |c: String| {
        let sts = alice.sts(t);
        let serial = serial.clone();
        async move { sts.get_session_token().serial_number(serial).token_code(c).send().await.is_ok() }
    };
    // Codes that aren't valid now (or a step either side).
    let valid: Vec<String> = [-30, 0, 30].iter().map(|d| totp(&seed, e.now_s() + d)).collect();
    let wrong: Vec<String> = (0..1000).map(|i| format!("{:06}", i * 997)).filter(|c| !valid.contains(c)).take(10).collect();
    for c in wrong {
        assert!(!try_code(c).await);
    }
    // Locked: even the right code fails, for the rest of the five minutes.
    assert!(!try_code(totp(&seed, e.now_s())).await);
    // The count is in the metadata (it survives a restart and a takeover: s3-store's
    // `throttle.rs` tests).
    assert!(t.store.throttled(&e.account(), "mfa:alice", 10, 300_000).unwrap());
    e.advance_s(301);
    assert!(try_code(totp(&seed, e.now_s())).await, "the window passed");
    assert!(!t.store.throttled(&e.account(), "mfa:alice", 10, 300_000).unwrap(), "a right code clears it");
}
