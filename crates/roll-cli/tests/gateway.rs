//! `roll gateway`: the real binary on a config file in a temp directory. Client keys are made,
//! listed and revoked through the CLI and checked against the same `gateway.db` the server reads
//! (`model_gateway::store::Store::verify`, which the server calls on every request).

use model_gateway::store::Store;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const CONFIG: &str = r#"
port = 18799
data_dir = "data"

[[upstreams]]
id = "anthropic"
name = "Anthropic"
kind = "anthropic"

[keys]
anthropic = "env:ROLL_GATEWAY_CLI_TEST_KEY"
"#;

fn roll(config: Option<&Path>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
    c.env_remove("ROLL_GATEWAY_CONFIG").env("ROLL_GATEWAY_CLI_TEST_KEY", "sk-test-upstream").arg("gateway").args(args);
    if let Some(p) = config {
        c.arg("--config").arg(p);
    }
    c.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn client_keys_are_made_listed_and_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    std::fs::write(&config, CONFIG).unwrap();

    let out = roll(Some(&config), &["client", "add", "laptop", "--machine", "mch_laptop"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let key = text(&out.stdout).trim().to_string();
    assert!(key.starts_with("rgk_") && !key.contains('\n'), "{key}");
    assert!(text(&out.stderr).contains("only time its key is shown"), "{}", text(&out.stderr));
    // And how a client uses it.
    assert!(text(&out.stderr).contains("ANTHROPIC_BASE_URL=http://<address>:8790 ANTHROPIC_AUTH_TOKEN=<key>"), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("OPENAI_BASE_URL=http://<address>:8790/v1"), "{}", text(&out.stderr));

    // The server's lookup accepts it, for that machine.
    let db = dir.path().join("data").join("gateway.db");
    let client = Store::open(&db).unwrap().verify(&key, 1).unwrap().expect("the key is live");
    assert_eq!(client.machine.as_ref().map(|m| m.as_str()), Some("mch_laptop"));

    // A key by hand, with --json; the config comes from the environment this time.
    let out = Command::new(env!("CARGO_BIN_EXE_roll")).env("ROLL_GATEWAY_CONFIG", &config).args(["--json", "gateway", "client", "add", "ci"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let made: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(made["client"]["name"], "ci");
    assert!(made["key"].as_str().unwrap().starts_with("rgk_"));

    // The list never shows a key.
    let out = roll(Some(&config), &["client", "list"]);
    assert!(out.status.success());
    let listed = text(&out.stdout);
    assert!(listed.contains(&client.id) && listed.contains("laptop") && listed.contains("mch_laptop") && listed.contains("ci"), "{listed}");
    assert!(!listed.contains("rgk_"), "{listed}");
    let out = Command::new(env!("CARGO_BIN_EXE_roll")).args(["--json", "gateway", "--config"]).arg(&config).args(["client", "ls"]).output().unwrap();
    let rows: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(rows[0]["last_used_at"].is_u64(), "the laptop's key was used: {rows}");
    assert!(!text(&out.stdout).contains("rgk_"));

    // Revoked: the next lookup refuses it.
    let out = roll(Some(&config), &["client", "revoke", &client.id]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(Store::open(&db).unwrap().verify(&key, 2).unwrap(), None);
    let out = roll(Some(&config), &["client", "revoke", &client.id]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("no client"), "{}", text(&out.stderr));

    // A new key for the machine replaces its old one.
    let first = text(&roll(Some(&config), &["client", "add", "box", "--machine", "mch_box"]).stdout).trim().to_string();
    let second = text(&roll(Some(&config), &["client", "add", "box", "--machine", "mch_box"]).stdout).trim().to_string();
    let store = Store::open(&db).unwrap();
    assert_eq!(store.verify(&first, 3).unwrap(), None);
    assert!(store.verify(&second, 3).unwrap().is_some());
    assert!(!roll(Some(&config), &["client", "add", "x", "--machine", "laptop"]).status.success());
}

#[test]
fn the_client_commands_need_no_upstream_key_but_serve_checks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("gateway.toml");
    // The key's variable isn't set for `client …`: it only needs the data directory.
    std::fs::write(&config, CONFIG.replace("ROLL_GATEWAY_CLI_TEST_KEY", "ROLL_GATEWAY_CLI_TEST_UNSET")).unwrap();
    assert!(roll(Some(&config), &["client", "add", "laptop"]).status.success());
    // `serve` loads the whole file: an unset key is an error that says which.
    let out = roll(Some(&config), &["serve"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("ROLL_GATEWAY_CLI_TEST_UNSET isn't set"), "{}", text(&out.stderr));
    // So is a key pasted into the file, without echoing it.
    std::fs::write(&config, CONFIG.replace("env:ROLL_GATEWAY_CLI_TEST_KEY", "sk-pasted-in")).unwrap();
    let out = roll(Some(&config), &["serve"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("never put the key itself") && !err.contains("sk-pasted-in"), "{err}");
    // No config at all.
    let out = roll(None, &["client", "list"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--config"), "{}", text(&out.stderr));
}
