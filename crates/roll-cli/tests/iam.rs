//! `roll storage accounts …` and `roll storage iam …` end to end: the store, the S3 API and the
//! control plane in process, and the real `roll` binary (with its own config directory, so
//! `accounts use` doesn't touch the developer's).

use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;

struct T {
    api: String,
    store: Arc<Store>,
    config: tempfile::TempDir,
    _data: tempfile::TempDir,
}

async fn start() -> T {
    let data = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("roll-cli-iam").await;
    let store = Store::open(StoreConfig::new(data.path().join("meta")).manual(), backend).await.unwrap();
    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: data.path().to_path_buf(), ..Default::default() });
    let control = Control::new(store.clone(), ControlConfig::local(s3)).await.unwrap();
    control.set_server(server.clone());
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, store, config: tempfile::tempdir().unwrap(), _data: data }
}

impl T {
    /// `roll storage <args>`: (exit code, stdout, stderr).
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> (Option<i32>, String, String) {
        use tokio::io::AsyncWriteExt;
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll"));
        cmd.arg("storage")
            .args(args)
            .env("ROLL_API_URL", &self.api)
            .env("XDG_CONFIG_HOME", self.config.path())
            .env_remove("ROLL_STORAGE_ACCOUNT")
            .env_remove("ROLL_API_KEY")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(input.as_bytes()).await.unwrap();
            pipe.shutdown().await.unwrap();
        }
        let out = child.wait_with_output().await.unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    async fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args, None).await;
        assert_eq!(code, Some(0), "roll storage {args:?}: {err}\n{out}");
        out
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let out = self.ok(&all).await;
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("roll storage {all:?}: not JSON ({e}): {out}"))
    }

    async fn fail(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args, None).await;
        assert_eq!(code, Some(1), "roll storage {args:?} should fail: {out}");
        err
    }
}

#[tokio::test]
async fn accounts() {
    let t = start().await;
    let default = t.store.default_account_id();
    let list = t.ok(&["accounts"]).await;
    assert!(list.contains(&default) && list.contains("(default)") && list.lines().nth(1).unwrap().starts_with('*'), "{list}");
    let created = t.json(&["accounts", "create", "acme", "--email", "ops@acme.test"]).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert!(created["root_key"]["secret_access_key"].as_str().is_some_and(|s| !s.is_empty()), "the root key's secret, once");
    let text = t.ok(&["accounts", "create", "beta", "--no-root-key"]).await;
    assert!(text.contains("beta") && !text.contains("secret access key"), "{text}");
    t.ok(&["accounts", "update", &id, "--name", "Acme Inc"]).await;
    assert_eq!(t.json(&["accounts", "show", &id]).await["name"], "Acme Inc");
    assert!(t.fail(&["accounts", "update", &id]).await.contains("nothing to change"));
    assert!(t.fail(&["accounts", "show", "nope"]).await.contains("no account"));
    // Switching: `roll storage` acts in the chosen account from then on.
    t.ok(&["accounts", "use", "Acme Inc"]).await;
    assert_eq!(std::fs::read_to_string(t.config.path().join("roll/storage-account")).unwrap().trim(), id);
    t.ok(&["create", "acme-data"]).await;
    assert_eq!(t.store.bucket("acme-data").unwrap().account_id, id);
    let list = t.ok(&["accounts"]).await;
    assert!(list.lines().any(|l| l.starts_with('*') && l.contains(&id)), "{list}");
    let buckets = t.json(&["buckets"]).await;
    assert_eq!(buckets.as_array().unwrap().len(), 1);
    // IAM acts there too.
    t.ok(&["iam", "users", "create", "deployer"]).await;
    assert!(t.store.iam_user(&id, "deployer").unwrap().is_some());
    t.ok(&["accounts", "use", "--default"]).await;
    assert!(t.json(&["buckets"]).await.as_array().unwrap().is_empty());
    // An account with buckets can't be deleted.
    assert!(t.fail(&["accounts", "delete", &id]).await.contains("bucket"));
    let beta = t.json(&["accounts"]).await.as_array().unwrap().iter().find(|a| a["name"] == "beta").unwrap()["id"].as_str().unwrap().to_string();
    t.ok(&["accounts", "delete", &beta]).await;
}

#[tokio::test]
async fn iam_users_groups_policies_roles_and_the_simulator() {
    let t = start().await;
    let acct = t.store.default_account_id();
    let dir = tempfile::tempdir().unwrap();
    let doc = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::data/*"}]});
    let doc_file = dir.path().join("read.json");
    std::fs::write(&doc_file, doc.to_string()).unwrap();
    let at = format!("@{}", doc_file.display());

    // Users.
    let u = t.json(&["iam", "users", "create", "alice", "--tag", "team=web"]).await;
    assert_eq!(u["User"]["Arn"], format!("arn:aws:iam::{acct}:user/alice"));
    assert!(t.fail(&["iam", "users", "create", "alice"]).await.contains("already exists"));
    let key = t.json(&["iam", "users", "create-key", "alice"]).await;
    let key_id = key["AccessKey"]["AccessKeyId"].as_str().unwrap().to_string();
    assert!(t.ok(&["iam", "users", "keys", "alice"]).await.contains(&key_id));
    t.ok(&["iam", "users", "update-key", "alice", &key_id, "--inactive"]).await;
    assert_eq!(t.json(&["iam", "users", "keys", "alice"]).await[0]["Status"], "Inactive");
    // A console password: policy-checked, read from stdin.
    let (code, _, err) = t.run(&["iam", "users", "set-password", "alice", "--password-stdin"], Some("weak\n")).await;
    assert_eq!(code, Some(1));
    assert!(err.contains("minimum length"), "{err}");
    let (code, _, err) = t.run(&["iam", "users", "set-password", "alice", "--password-stdin", "--reset-required"], Some("Correct-Horse-9\n")).await;
    assert_eq!(code, Some(0), "{err}");
    let shown = t.json(&["iam", "users", "show", "alice"]).await;
    assert_eq!(shown["login_profile"]["PasswordResetRequired"], "true");
    assert_eq!(shown["access_keys"][0]["AccessKeyId"], key_id.as_str());
    t.ok(&["iam", "users", "set-password", "alice", "--password", "Battery-Staple-12"]).await;
    // Policies: inline, managed (versions), attached.
    t.ok(&["iam", "users", "put-policy", "alice", "read", &at]).await;
    assert!(t.fail(&["iam", "users", "put-policy", "alice", "bad", r#"{"Statement":[{"Effect":"Allow"}]}"#]).await.contains("Action"), "the parser's message");
    let p = t.json(&["iam", "policies", "create", "readers", &at, "--description", "read data"]).await;
    let arn = p["Policy"]["Arn"].as_str().unwrap().to_string();
    assert_eq!(arn, format!("arn:aws:iam::{acct}:policy/readers"));
    let wider = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "s3:*", "Resource": "*"}]}).to_string();
    for _ in 0..5 {
        t.ok(&["iam", "policies", "update", "readers", &wider]).await;
    }
    let shown = t.json(&["iam", "policies", "show", "readers"]).await;
    assert_eq!(shown["policy"]["DefaultVersionId"], "v6", "older versions make room");
    assert!(t.ok(&["iam", "policies"]).await.contains("readers"));
    assert!(t.ok(&["iam", "policies", "--scope", "aws"]).await.contains("AmazonS3ReadOnlyAccess"));
    // Groups.
    t.ok(&["iam", "groups", "create", "ops"]).await;
    t.ok(&["iam", "groups", "add-user", "ops", "alice"]).await;
    t.ok(&["iam", "groups", "attach", "ops", "readers"]).await;
    assert!(t.ok(&["iam", "groups", "show", "ops"]).await.contains("alice"));
    // The simulator: the group's managed policy (now s3:*) allows; nothing allows IAM.
    let sim = t.json(&["iam", "simulate", "user/alice", "--action", "s3:PutObject", "--action", "iam:CreateUser", "--resource", "arn:aws:s3:::data/x"]).await;
    let results = sim["EvaluationResults"].as_array().unwrap();
    assert_eq!((results[0]["EvalDecision"].as_str(), results[1]["EvalDecision"].as_str()), (Some("allowed"), Some("implicitDeny")));
    assert_eq!(results[0]["MatchedStatements"][0]["SourcePolicyId"], "readers");
    let table = t.ok(&["iam", "simulate", "alice", "--action", "s3:GetObject", "--resource", "arn:aws:s3:::data/x"]).await;
    assert!(table.contains("allowed") && table.contains("DECISION"), "{table}");
    // Roles.
    let trust = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{acct}:user/alice")}, "Action": "sts:AssumeRole"}]}).to_string();
    let r = t.json(&["iam", "roles", "create", "deploy", "--trust", &trust, "--max-session", "2h"]).await;
    assert_eq!(r["Role"]["MaxSessionDuration"], "7200");
    let missing = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"AWS": format!("arn:aws:iam::{acct}:user/nobody")}, "Action": "sts:AssumeRole"}]}).to_string();
    assert!(t.fail(&["iam", "roles", "update-trust", "deploy", &missing]).await.contains("Invalid principal in policy"));
    t.ok(&["iam", "roles", "attach", "deploy", "AmazonS3ReadOnlyAccess"]).await;
    let shown = t.ok(&["iam", "roles", "show", "deploy"]).await;
    assert!(shown.contains("AmazonS3ReadOnlyAccess") && shown.contains("sts:AssumeRole"), "{shown}");
    assert!(t.ok(&["iam", "roles"]).await.contains("deploy"));
    // Cleaning up, in IAM's order.
    assert!(t.fail(&["iam", "users", "delete", "alice"]).await.contains("must delete"));
    t.ok(&["iam", "roles", "detach", "deploy", "AmazonS3ReadOnlyAccess"]).await;
    t.ok(&["iam", "roles", "delete", "deploy"]).await;
    t.ok(&["iam", "groups", "detach", "ops", "readers"]).await;
    t.ok(&["iam", "groups", "remove-user", "ops", "alice"]).await;
    t.ok(&["iam", "groups", "delete", "ops"]).await;
    t.ok(&["iam", "policies", "delete", "readers"]).await;
    t.ok(&["iam", "users", "delete-policy", "alice", "read"]).await;
    t.ok(&["iam", "users", "remove-password", "alice"]).await;
    t.ok(&["iam", "users", "delete-key", "alice", &key_id]).await;
    t.ok(&["iam", "users", "delete", "alice"]).await;
    assert!(t.ok(&["iam", "users"]).await.contains("no users"));
}
