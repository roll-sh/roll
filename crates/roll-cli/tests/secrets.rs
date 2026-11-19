//! `roll secrets` and `roll run` end to end: the workspace and secrets routers in process, and
//! the real `roll` binary talking to them through `ROLL_API_URL`.

use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs in `$ROLL_TEST_CWD` when it's set (the setup check), with its own config home.
async fn roll(api: &str, env: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> Out {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll"));
    cmd.args(args)
        .env("ROLL_API_URL", api)
        .env_remove("ROLL_TOKEN")
        .env_remove("ROLL_PROJECT")
        .env_remove("ROLL_ENV")
        .env_remove("ROLL_ADMIN_TOKEN")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        if *k == "ROLL_TEST_CWD" {
            cmd.current_dir(v);
            continue;
        }
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(input.as_bytes()).await.unwrap();
        pipe.shutdown().await.unwrap();
    }
    let out = child.wait_with_output().await.unwrap();
    Out { code: out.status.code(), stdout: String::from_utf8_lossy(&out.stdout).into(), stderr: String::from_utf8_lossy(&out.stderr).into() }
}

async fn ok(api: &str, env: &[(&str, &str)], args: &[&str], stdin: Option<&str>) -> String {
    let o = roll(api, env, args, stdin).await;
    assert_eq!(o.code, Some(0), "roll {args:?}: {}\n{}", o.stderr, o.stdout);
    o.stdout
}

/// roll-access as far as `roll secrets tokens` and `ROLL_TOKEN` see it: `POST /v1/access/tokens`
/// keeps the rules it's given, and the gate puts a known token's grant on the request.
type Tokens = Arc<std::sync::Mutex<std::collections::BTreeMap<String, roll_auth::Grant>>>;

async fn create_token(axum::extract::State(tokens): axum::extract::State<Tokens>, axum::Json(b): axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
    let rules: Vec<roll_auth::scope::Rule> = serde_json::from_value(b["rules"].clone()).unwrap();
    let mut t = tokens.lock().unwrap();
    let (secret, id) = (format!("rlt_cli{}", t.len()), format!("tok_{}", t.len()));
    let name = b["name"].as_str().unwrap().to_string();
    t.insert(secret.clone(), roll_auth::Grant { ask_otherwise: false, user: None, id: id.clone().into(), name: name.clone().into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(rules)) });
    axum::Json(serde_json::json!({ "grant": { "id": id, "name": name }, "token": secret }))
}

async fn gate(axum::extract::State(tokens): axum::extract::State<Tokens>, mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Some(t) = roll_auth::bearer(req.headers()).map(str::to_string) {
        let Some(g) = tokens.lock().unwrap().get(&t).cloned() else { return axum::http::StatusCode::UNAUTHORIZED.into_response() };
        req.extensions_mut().insert(g);
    }
    next.run(req).await
}

/// The digest route's requests, and whether it fails (503) for now: what `roll run --watch` polls.
#[derive(Default)]
struct Polls {
    failing: std::sync::atomic::AtomicBool,
    asked: std::sync::atomic::AtomicUsize,
}

async fn flaky_digest(axum::extract::State(p): axum::extract::State<Arc<Polls>>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    use std::sync::atomic::Ordering;
    if req.uri().path().ends_with("/digest") {
        p.asked.fetch_add(1, Ordering::SeqCst);
        if p.failing.load(Ordering::SeqCst) {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    next.run(req).await
}

/// The workspace, secrets and access routers on a local port, with the project `shop`.
struct Server {
    api: String,
    secrets: Arc<roll_secrets::Secrets>,
    projects: Arc<roll_workspace::Workspace>,
    polls: Arc<Polls>,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    ws.create("", "shop", "", None).unwrap();
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
    let opts = roll_secrets::Options {
        keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
        key_source: roll_secrets::KeySource::Env,
        links: Default::default(),
        syncs: Default::default(),
        open: true,
        trust_forwarded_for: false,
        web_url: None,
        deliver: None,
        grants: None,
        people: None,
        clouds: None,
        outside: None,
        directory: None,
        github: None,
        kms: None,
        sts: None,
        workloads: None,
        gateways: None,
    };
    let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
    let served = secrets.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let projects = ws.clone();
    let tokens = Tokens::default();
    let polls = Arc::new(Polls::default());
    let access = axum::Router::new().route("/v1/access/tokens", axum::routing::post(create_token)).with_state(tokens.clone());
    let router = roll_workspace::router(ws).merge(roll_secrets::router(served)).merge(access).layer(axum::middleware::from_fn_with_state(tokens, gate)).layer(axum::middleware::from_fn_with_state(polls.clone(), flaky_digest));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { api, secrets, projects, polls, _dir: dir }
}

#[tokio::test]
async fn roll_run_injects_an_environments_secrets_and_refuses_unresolved_references() {
    let Server { api, projects, _dir: dir, .. } = start().await;
    let at = ["secrets", "-p", "shop", "-e", "production"];
    ok(&api, &[], &[&at[..], &["set", "HOST=db.internal", "PORT=5432"]].concat(), None).await;
    ok(&api, &[], &[&at[..], &["set", "PASSWORD"]].concat(), Some("s3cret\n")).await;
    // A key's password policy: `required add` sets it, and `set KEY --generate` follows it.
    ok(&api, &[], &["secrets", "-p", "shop", "required", "add", "API_SECRET", "--password-length", "20", "--classes", "lower,digit"], None).await;
    ok(&api, &[], &[&at[..], &["set", "API_SECRET", "--generate"]].concat(), None).await;
    let made = ok(&api, &[], &[&at[..], &["get", "API_SECRET", "--raw"]].concat(), None).await.trim().to_string();
    assert!(made.len() == 20 && made.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) && made.chars().any(|c| c.is_ascii_digit()), "{} characters", made.len());
    ok(&api, &[], &[&at[..], &["import", "-"]].concat(), Some("DATABASE_URL=postgres://app:${PASSWORD}@${HOST}:${PORT}/shop\n")).await;
    let ls = ok(&api, &[], &[&at[..], &["ls"]].concat(), None).await;
    assert!(ls.contains("DATABASE_URL") && !ls.contains("s3cret"), "values are masked: {ls}");

    // The child sees resolved values; the shell's own variables win unless --override.
    let out = ok(&api, &[("PORT", "6000")], &["run", "-p", "shop", "-e", "production", "--no-mask", "--", "sh", "-c", "echo $DATABASE_URL $PORT"], None).await;
    assert_eq!(out.trim(), "postgres://app:s3cret@db.internal:5432/shop 6000");
    // In CI, output that isn't a terminal or a file (a build log) goes through roll, which
    // conceals the values it added; the command's exit code is still roll's.
    let logged = roll(&api, &[("PORT", "6000"), ("CI", "true")], &["run", "-p", "shop", "-e", "production", "--", "sh", "-c", "echo $DATABASE_URL $PORT >&2; echo $HOST"], None).await;
    assert_eq!((logged.code, logged.stdout.trim(), logged.stderr.trim()), (Some(0), "<concealed by roll>", "<concealed by roll> 6000"));
    let failed = roll(&api, &[], &["run", "-p", "shop", "-e", "production", "--", "sh", "-c", "exit 3"], None).await;
    assert_eq!(failed.code, Some(3));

    // An env file committed with references and no value: `roll://ENV/KEY` resolved at run time,
    // plain values as written, and the file's entries win over the environment's.
    let files = tempfile::tempdir().unwrap();
    let env_file = files.path().join(".env");
    std::fs::write(&env_file, "# app\nPORT=7000\nDB=roll://production/DATABASE_URL\nOTHER=roll://shop@production/HOST\n").unwrap();
    let out = ok(&api, &[], &["run", "-p", "shop", "-e", "production", "--env-file", env_file.to_str().unwrap(), "--", "sh", "-c", "echo $DB $OTHER $PORT"], None).await;
    assert_eq!(out.trim(), "postgres://app:s3cret@db.internal:5432/shop db.internal 7000");
    // Masked, the references are concealed and the file's own plain values aren't.
    let logged = ok(&api, &[("CI", "true")], &["run", "-p", "shop", "-e", "production", "--env-file", env_file.to_str().unwrap(), "--", "sh", "-c", "echo $DB $OTHER $PORT"], None).await;
    assert_eq!(logged.trim(), "<concealed by roll> <concealed by roll> 7000");
    std::fs::write(&env_file, "MISSING=roll://production/NOPE\n").unwrap();
    let refused = roll(&api, &[], &["run", "-p", "shop", "-e", "production", "--env-file", env_file.to_str().unwrap(), "--", "true"], None).await;
    assert!(refused.code != Some(0) && refused.stderr.contains("roll://production/NOPE"), "{}", refused.stderr);

    // A token made for the project (a roll-access grant with one rule) implies its project and
    // reaches only its environments.
    let token = ok(&api, &[], &["secrets", "-p", "shop", "tokens", "create", "ci", "--for-env", "production"], None).await;
    let t = [("ROLL_TOKEN", token.trim())];
    assert_eq!(ok(&api, &t, &["run", "-e", "production", "--no-mask", "--", "sh", "-c", "echo $HOST"], None).await.trim(), "db.internal");
    let denied = roll(&api, &t, &["run", "-e", "staging", "--", "true"], None).await;
    assert_ne!(denied.code, Some(0));
    // `kubernetes` prints what External Secrets Operator needs: its store's URL, with the token it
    // made, reads the environment.
    let yaml = ok(&api, &[], &["secrets", "-p", "shop", "-e", "production", "kubernetes", "--url", &api], None).await;
    let field = |k: &str| yaml.lines().find_map(|l| l.trim().strip_prefix(k)).unwrap().trim_matches('"').to_string();
    let url = field("url: ").replace("{{ .remoteRef.key | urlquery }}", "production");
    let pulled: serde_json::Value = reqwest::Client::new().get(url).bearer_auth(field("token: ")).send().await.unwrap().json().await.unwrap();
    assert_eq!(pulled["values"]["HOST"], "db.internal");

    // Through an outage: the last values fetched (kept encrypted with --fallback) start the
    // command, and only with the key they were kept with.
    let fallback = dir.path().join("production.fallback");
    let fb = fallback.to_str().unwrap();
    let key = [("ROLL_FALLBACK_KEY", "fallback-key-of-at-least-32-chars")];
    // An env file's references are kept too.
    std::fs::write(&env_file, "PORT=7000\nDB=roll://production/DATABASE_URL\n").unwrap();
    let ef = env_file.to_str().unwrap();
    let echo = ["run", "-p", "shop", "-e", "production", "--fallback", fb, "--env-file", ef, "--", "sh", "-c", "echo $HOST $DB $PORT"];
    let all = "db.internal postgres://app:s3cret@db.internal:5432/shop 7000";
    assert_eq!(ok(&api, &key, &echo, None).await.trim(), all);
    assert!(!std::fs::read_to_string(&fallback).unwrap().contains("s3cret"), "kept encrypted");
    let down = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    assert_eq!(ok(&down, &key, &echo, None).await.trim(), all);
    // A reference added since the last fetch can't start from it.
    std::fs::write(&env_file, "NEW=roll://production/PORT\n").unwrap();
    let added = roll(&down, &key, &echo, None).await;
    assert!(added.code != Some(0) && added.stderr.contains("roll://production/PORT isn't in the fallback"), "{}", added.stderr);
    assert_ne!(roll(&down, &[("ROLL_FALLBACK_KEY", "another-key-of-at-least-32-chars!")], &echo, None).await.code, Some(0));

    // `roll secrets setup` remembers the project and environment for a directory and the ones
    // under it, in the user's config: then `roll run` needs no flags there.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("app/web")).unwrap();
    let (app, web) = (home.path().join("app"), home.path().join("app/web"));
    let cfg = home.path().join("config");
    let (app, web, cfg) = (app.to_str().unwrap(), web.to_str().unwrap(), cfg.to_str().unwrap());
    ok(&api, &[("XDG_CONFIG_HOME", cfg), ("ROLL_TEST_CWD", app)], &["secrets", "setup", "-p", "shop", "-e", "production"], None).await;
    let here = ok(&api, &[("XDG_CONFIG_HOME", cfg), ("ROLL_TEST_CWD", web)], &["run", "--no-mask", "--", "sh", "-c", "echo $HOST"], None).await;
    assert_eq!(here.trim(), "db.internal");
    let doctor = ok(&api, &[("XDG_CONFIG_HOME", cfg), ("ROLL_TEST_CWD", web)], &["secrets", "doctor"], None).await;
    assert!(doctor.contains("ok   using shop production:/ (all from your setup)") && !doctor.contains("FAIL"), "{doctor}");

    // A template's keys checked against the environment, by name only.
    let tpl = dir.path().join(".env.example");
    std::fs::write(&tpl, "# what the app reads\nHOST=\nexport PORT=5432\nNEW_FLAG\n").unwrap();
    let checked = roll(&api, &[], &[&at[..], &["check", tpl.to_str().unwrap()]].concat(), None).await;
    assert!(checked.code != Some(0) && checked.stdout.contains("missing  NEW_FLAG") && !checked.stdout.contains("missing  HOST"), "{}{}", checked.stdout, checked.stderr);

    // A pre-commit scan finds a stored value in a file, and says where (never the value).
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("config.yml"), "name: shop\ndb:\n  host: db.internal\n").unwrap();
    let scanned = roll(&api, &[], &["secrets", "-p", "shop", "scan", repo.path().to_str().unwrap()], None).await;
    assert!(scanned.code != Some(0) && scanned.stdout.contains("config.yml:3  HOST (production)"), "{}{}", scanned.stdout, scanned.stderr);

    // A credential roll secrets doesn't hold: a pre-commit scan refuses it, `--store` moves it in
    // from the file and says what to write instead. (Built from pieces: push protection scans
    // this repo.)
    let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(repo.path()).status().unwrap().success());
    git(&["init", "-q"]);
    let aws = ["AKIA", "ROLLFIXTURE", "23456"].concat();
    std::fs::write(repo.path().join("deploy.ts"), format!("// deploys\nexport const awsKey = \"{aws}\";\n")).unwrap();
    git(&["add", "deploy.ts"]);
    let cwd = [("ROLL_TEST_CWD", repo.path().to_str().unwrap())];
    let refused = roll(&api, &cwd, &["secrets", "-p", "shop", "scan", "--staged"], None).await;
    assert!(refused.code != Some(0) && refused.stdout.contains("deploy.ts:2  an AWS access key, not in roll secrets: roll secrets scan --store AWS_KEY=deploy.ts:2") && !refused.stdout.contains(&aws), "{}{}", refused.stdout, refused.stderr);
    ok(&api, &cwd, &["secrets", "-p", "shop", "scan", "--staged", "--allow-unstored"], None).await;
    let stored = ok(&api, &cwd, &[&at[..], &["scan", "--store", "deploy.ts:2"]].concat(), None).await;
    assert!(stored.contains("as AWS_KEY in production:/") && stored.contains("Replace it with process.env.AWS_KEY") && !stored.contains(&aws), "{stored}");
    let again = roll(&api, &cwd, &["secrets", "-p", "shop", "scan", "--staged"], None).await;
    assert!(again.code != Some(0) && again.stdout.contains("deploy.ts:2  AWS_KEY (production)") && !again.stdout.contains("not in roll secrets"), "now it's the project's value: {}", again.stdout);

    // A config file filled from the environment: `\${…}` and shell syntax stay as written.
    let tpl = "url: http://${HOST}:${PORT}\nliteral: \\${HOST} ${HOME:-x} $PORT\n";
    let filled = ok(&api, &[], &[&at[..], &["render", "-"]].concat(), Some(tpl)).await;
    assert_eq!(filled, "url: http://db.internal:5432\nliteral: ${HOST} ${HOME:-x} $PORT\n");
    // References stand where they are; neither kind of value is read again as a template.
    let mixed = ok(&api, &[], &[&at[..], &["render", "-"]].concat(), Some("host: ${HOST}\nurl: \"roll://production/DATABASE_URL\"\n")).await;
    assert_eq!(mixed, "host: db.internal\nurl: \"postgres://app:s3cret@db.internal:5432/shop\"\n");
    let unset = roll(&api, &[], &[&at[..], &["render", "-"]].concat(), Some("${HOST} ${MISSING}")).await;
    assert!(unset.code != Some(0) && unset.stderr.contains("MISSING isn't set") && !unset.stdout.contains("db.internal"), "{}", unset.stderr);

    ok(&api, &[], &[&at[..], &["set", "BROKEN=${NOPE}"]].concat(), None).await;
    let refused = roll(&api, &[], &["run", "-p", "shop", "-e", "production", "--", "true"], None).await;
    assert_ne!(refused.code, Some(0));
    assert!(refused.stderr.contains("BROKEN: NOPE doesn't exist"), "{}", refused.stderr);

    // Straight from another manager: a Vault KV v2 secret, read with the user's own token.
    let vault = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let vault_addr = format!("http://{}", vault.local_addr().unwrap());
    let kv = axum::Router::new().route(
        "/v1/secret/data/shop",
        axum::routing::get(|h: axum::http::HeaderMap| async move {
            if h.get("x-vault-token").and_then(|t| t.to_str().ok()) != Some("hvs.test") {
                return (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "errors": ["permission denied"] })));
            }
            (axum::http::StatusCode::OK, axum::Json(serde_json::json!({ "data": { "data": { "FROM_VAULT": "v-1", "RETRIES": 3 } } })))
        }),
    ).route("/v1/secret/data/shop/stg", axum::routing::get(|| async { axum::Json(serde_json::json!({ "data": { "data": { "STAGED": "s-1" } } })) }));
    tokio::spawn(async move { axum::serve(vault, kv).await.unwrap() });
    let env = [("VAULT_ADDR", vault_addr.as_str()), ("VAULT_TOKEN", "hvs.test")];
    ok(&api, &env, &[&at[..], &["import", "--from", "vault", "--source", "secret/shop"]].concat(), None).await;
    let got = ok(&api, &[], &["run", "-p", "shop", "-e", "production", "--allow-unresolved", "--", "sh", "-c", "echo $FROM_VAULT $RETRIES"], None).await;
    assert_eq!(got.trim(), "v-1 3");
    // Several environments at once: each source path into its environment.
    let mapped = ok(&api, &env, &["secrets", "-p", "shop", "import", "--from", "vault", "--source", "secret/shop", "--map", "stg=staging"], None).await;
    assert!(mapped.contains("stg → staging: 1 value"), "{mapped}");

    // A backup sealed to a key only this machine holds restores into another project.
    let keys = tempfile::tempdir().unwrap();
    let (key, file) = (keys.path().join("backup.key"), keys.path().join("shop.backup"));
    let (key, file) = (key.to_str().unwrap(), file.to_str().unwrap());
    let public = ok(&api, &[], &["secrets", "keygen", "--out", key], None).await;
    ok(&api, &[], &["secrets", "-p", "shop", "backup", "--to", public.trim(), "--out", file], None).await;
    assert!(!std::fs::read_to_string(file).unwrap().contains("s3cret"), "sealed at rest");
    // Its key split 2 of 3 and the key file gone: two holders' shares recover it.
    let shares = ok(&api, &[], &["secrets", "key", "split", "--shares", "3", "--threshold", "2", "--file", key], None).await;
    let shares: Vec<&str> = shares.lines().collect();
    assert!(shares.len() == 3 && shares.iter().all(|s| s.starts_with("rlks_")), "{shares:?}");
    std::fs::remove_file(key).unwrap();
    let mixed = roll(&api, &[], &["secrets", "key", "combine"], Some(&format!("{}\n{}\n", shares[0], shares[0]))).await;
    assert!(mixed.code != Some(0) && mixed.stdout.is_empty() && mixed.stderr.contains("given twice"), "{}", mixed.stderr);
    ok(&api, &[], &["secrets", "key", "combine", "--out", key], Some(&format!("{}\n{}\n", shares[2], shares[0]))).await;
    projects.create("", "copy", "", None).unwrap();
    ok(&api, &[], &["secrets", "-p", "copy", "restore", file, "--key", key], None).await;
    let restored = ok(&api, &[], &["run", "-p", "copy", "-e", "production", "--allow-unresolved", "--no-mask", "--", "sh", "-c", "echo $HOST $PASSWORD"], None).await;
    assert_eq!(restored.trim(), "db.internal s3cret");
}

/// Answers each CLI's arguments with the file named after them (other characters `_`), from the
/// mode's folder or `all`, and logs its arguments: a value must never be among them.
const FAKE_CLI: &str = r#"#!/bin/sh
bin=$(basename "$0")
echo "$bin $*" >> "$FAKE_DIR/argv.log"
k=${bin}_$(printf '%s' "$*" | tr -c 'A-Za-z0-9' '_')
for d in "$FAKE_DIR/$FAKE_MODE" "$FAKE_DIR/all"; do
  if [ -f "$d/$k.err" ]; then cat "$d/$k.err" >&2; exit 1; fi
  if [ -f "$d/$k" ]; then cat "$d/$k"; exit 0; fi
done
echo "fake $bin: nothing recorded for: $*" >&2
exit 2
"#;

/// Records `bin args`' answer in `mode` (`all`, `fail`, `collide`); `Err` text is its stderr.
fn answer(dir: &std::path::Path, mode: &str, bin: &str, args: &str, out: Result<serde_json::Value, &str>) {
    let key: String = format!("{bin}_{args}").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    std::fs::create_dir_all(dir.join(mode)).unwrap();
    match out {
        Ok(v) => std::fs::write(dir.join(mode).join(key), v.to_string()).unwrap(),
        Err(e) => std::fs::write(dir.join(mode).join(format!("{key}.err")), format!("{e}\n")).unwrap(),
    }
}

/// Moving in through other managers' own CLIs (fakes printing what their docs show): each
/// imports, stops on a secret it can't read, and stops on two secrets giving one key, and no
/// value is ever on a command line or in an error.
#[tokio::test]
async fn import_from_other_managers_through_their_clis() {
    use serde_json::json;
    let Server { api, .. } = start().await;
    let fake = tempfile::tempdir().unwrap();
    let (dir, bin) = (fake.path().join("answers"), fake.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["op", "bws", "aws", "gcloud", "az", "akeyless"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(bin.join(name), FAKE_CLI).unwrap();
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let a = |mode: &str, bin: &str, args: &str, out: Result<serde_json::Value, &str>| answer(&dir, mode, bin, args, out);

    // 1Password: `op item list`, then `op item get` per item.
    let item = |id: &str, title: &str, fields: serde_json::Value| json!({ "id": id, "title": title, "version": 1, "vault": { "id": "vlt0fake", "name": "Fake" }, "category": "LOGIN", "fields": fields });
    let listed = |id: &str, title: &str| json!({ "id": id, "title": title, "version": 1, "vault": { "id": "vlt0fake", "name": "Fake" }, "category": "LOGIN" });
    a("all", "op", "item list --vault Fake --format json", Ok(json!([listed("itm1", "Stripe"), listed("itm2", "Main DB")])));
    a("all", "op", "item get itm1 --vault Fake --format json --reveal", Ok(item("itm1", "Stripe", json!([
        { "id": "credential", "type": "CONCEALED", "label": "credential", "value": "fakevalue-op-1", "reference": "op://Fake/Stripe/credential" },
        { "id": "notesPlain", "type": "STRING", "purpose": "NOTES", "label": "notesPlain", "value": "fakevalue-note" }
    ]))));
    let db = item("itm2", "Main DB", json!([
        { "id": "username", "type": "STRING", "purpose": "USERNAME", "label": "username", "value": "fakevalue-op-2" },
        { "id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password", "value": "fakevalue-op-3" },
        { "id": "abc", "section": { "id": "s1", "label": "Replica" }, "type": "STRING", "label": "port", "value": "6543" },
        { "id": "otp", "type": "OTP", "label": "one-time password", "value": "otpauth://totp/fakevalue-otp" }
    ]));
    a("all", "op", "item get itm2 --vault Fake --format json --reveal", Ok(db.clone()));
    a("all", "op", "item get Main DB --vault Fake --format json --reveal", Ok(db));
    a("fail", "op", "item get itm2 --vault Fake --format json --reveal", Err("[ERROR] 2026/11/18 12:00:00 You don't have permission to view this item"));
    a("collide", "op", "item list --vault Fake --format json", Ok(json!([listed("itm1", "Stripe"), listed("itm3", "stripe")])));
    a("collide", "op", "item get itm3 --vault Fake --format json --reveal", Ok(item("itm3", "stripe", json!([{ "id": "credential", "type": "CONCEALED", "label": "credential", "value": "fakevalue-op-4" }]))));

    // Bitwarden Secrets Manager: `bws secret list PROJECT_ID`.
    let bws = |id: &str, key: &str, value: &str| json!({ "object": "secret", "id": id, "organizationId": "00000000-0000-0000-0000-00000000f00d", "projectId": "00000000-0000-0000-0000-0000000000aa", "key": key, "value": value, "note": "", "creationDate": "2026-01-01T00:00:00Z", "revisionDate": "2026-01-01T00:00:00Z" });
    let project = "00000000-0000-0000-0000-0000000000aa";
    let b1 = bws("00000000-0000-0000-0000-000000000001", "FAKE_API_KEY", "fakevalue-bws-1");
    let b2 = bws("00000000-0000-0000-0000-000000000002", "FAKE_TOKEN", "fakevalue-bws-2");
    a("all", "bws", &format!("secret list {project} --output json"), Ok(json!([b1, b2])));
    a("fail", "bws", &format!("secret list {project} --output json"), Err("Error: \n   0: Received error message from server: [401 Unauthorized]"));
    a("collide", "bws", &format!("secret list {project} --output json"), Ok(json!([b1, b2, bws("00000000-0000-0000-0000-000000000003", "FAKE_TOKEN", "fakevalue-bws-3")])));

    // Parameter Store: one `aws ssm get-parameters-by-path` (the CLI pages itself).
    let ssm_args = "ssm get-parameters-by-path --path /fake/shop --recursive --with-decryption --max-items 501 --output json";
    let p = |name: &str, kind: &str, value: &str| json!({ "Name": name, "Type": kind, "Value": value, "Version": 1, "LastModifiedDate": "2026-01-01T00:00:00Z", "ARN": format!("arn:aws:ssm:us-east-1:000000000000:parameter{name}"), "DataType": "text" });
    let params = vec![p("/fake/shop/API_URL", "String", "https://fakevalue.example"), p("/fake/shop/db/password", "SecureString", "fakevalue-ssm-1"), p("/fake/shop/db/db-user", "String", "fakevalue-ssm-2")];
    a("all", "aws", ssm_args, Ok(json!({ "Parameters": params })));
    a("fail", "aws", ssm_args, Err("An error occurred (AccessDeniedException) when calling the GetParametersByPath operation: The ciphertext refers to a customer master key that does not exist"));
    let mut more = params.clone();
    more.push(p("/fake/shop/api-url", "String", "fakevalue-ssm-3"));
    a("collide", "aws", ssm_args, Ok(json!({ "Parameters": more })));

    // Google Secret Manager: `gcloud secrets list`, then the latest version of each.
    use base64::Engine;
    let gsm = |id: &str| json!({ "name": format!("projects/123456789/secrets/{id}"), "createTime": "2026-01-01T00:00:00Z", "replication": { "automatic": {} }, "etag": "\"fake\"" });
    let version = |id: &str, value: &str| json!({ "name": format!("projects/123456789/secrets/{id}/versions/1"), "payload": { "data": base64::engine::general_purpose::STANDARD.encode(value), "dataCrc32c": "1" } });
    let access = |id: &str| format!("secrets versions access latest --secret {id} --project fake-project --format json");
    a("all", "gcloud", "secrets list --project fake-project --format json", Ok(json!([gsm("fake-db-url"), gsm("fake-api-key"), gsm("fake-empty"), gsm("other-thing")])));
    a("all", "gcloud", &access("fake-db-url"), Ok(version("fake-db-url", "fakevalue-gcp-1")));
    a("all", "gcloud", &access("fake-api-key"), Ok(version("fake-api-key", "fakevalue-gcp-2")));
    a("all", "gcloud", &access("fake-empty"), Err("ERROR: (gcloud.secrets.versions.access) NOT_FOUND: Secret Version [projects/123456789/secrets/fake-empty/versions/latest] not found or has no versions."));
    a("fail", "gcloud", &access("fake-api-key"), Err("ERROR: (gcloud.secrets.versions.access) PERMISSION_DENIED: Permission 'secretmanager.versions.access' denied for resource 'projects/fake-project/secrets/fake-api-key/versions/latest'"));
    a("collide", "gcloud", "secrets list --project fake-project --format json", Ok(json!([gsm("fake-db-url"), gsm("fake-DB_URL")])));
    a("collide", "gcloud", &access("fake-DB_URL"), Ok(version("fake-DB_URL", "fakevalue-gcp-3")));

    // Azure Key Vault: `az keyvault secret list`, then `show` per enabled secret. Its names are
    // letters, digits and `-`, case-insensitive: two can't give one key.
    let akv = |name: &str, enabled: bool| json!({ "attributes": { "enabled": enabled, "recoveryLevel": "Recoverable+Purgeable" }, "contentType": null, "id": format!("https://fake-vault.vault.azure.net/secrets/{name}"), "managed": null, "name": name, "tags": {} });
    let shown = |name: &str, value: &str| json!({ "attributes": { "enabled": true }, "contentType": null, "id": format!("https://fake-vault.vault.azure.net/secrets/{name}/0123456789abcdef"), "kid": null, "managed": null, "name": name, "tags": {}, "value": value });
    let show = |name: &str| format!("keyvault secret show --vault-name fake-vault --name {name} --output json");
    a("all", "az", "keyvault secret list --vault-name fake-vault --output json", Ok(json!([akv("db-url", true), akv("api-key", true), akv("old-key", false)])));
    a("all", "az", &show("db-url"), Ok(shown("db-url", "fakevalue-az-1")));
    a("all", "az", &show("api-key"), Ok(shown("api-key", "fakevalue-az-2")));
    a("fail", "az", &show("api-key"), Err("ERROR: (Forbidden) The user, group or application 'appid=00000000-0000-0000-0000-000000000000' does not have secrets get permission on key vault 'fake-vault'."));

    // Akeyless: `akeyless list-items`, then `get-secret-value` per static secret.
    let ak = |name: &str| json!({ "item_name": name, "item_id": 1, "item_type": "STATIC_SECRET", "item_sub_type": "generic", "item_tags": [] });
    let list_args = "list-items --path /fake/shop --type static-secret --json";
    let get = |name: &str| format!("get-secret-value --name {name} --json");
    a("all", "akeyless", list_args, Ok(json!({ "items": [ak("/fake/shop/api-key"), ak("/fake/shop/db/url")], "folders": ["/fake/shop/db"], "next_page": "" })));
    a("all", "akeyless", &get("/fake/shop/api-key"), Ok(json!({ "/fake/shop/api-key": "fakevalue-akl-1" })));
    a("all", "akeyless", &get("/fake/shop/db/url"), Ok(json!({ "/fake/shop/db/url": "fakevalue-akl-2" })));
    a("fail", "akeyless", &get("/fake/shop/db/url"), Err("Error: failed to get secret value: 403 Forbidden: access denied"));
    a("collide", "akeyless", list_args, Ok(json!({ "items": [ak("/fake/shop/api-key"), ak("/fake/shop/API_KEY")], "folders": [], "next_page": "" })));
    a("collide", "akeyless", &get("/fake/shop/API_KEY"), Ok(json!({ "/fake/shop/API_KEY": "fakevalue-akl-3" })));

    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let fake_dir = dir.to_str().unwrap().to_string();
    let import = |env_name: &'static str, at: &'static str, from: &'static str, source: &'static str, extra: &'static [&'static str]| {
        [&["secrets", "-p", "shop", "-e", env_name, "--path", at, "import", "--from", from, "--source", source][..], extra].concat()
    };
    let run = |at: &'static str, echo: &'static str| ["run", "-p", "shop", "-e", "production", "--path", at, "--no-mask", "--", "sh", "-c", echo];
    let call = async |mode: &str, args: &[&str]| roll(&api, &[("PATH", path.as_str()), ("FAKE_DIR", fake_dir.as_str()), ("FAKE_MODE", mode)], args, None).await;
    let done = async |mode: &str, args: &[&str]| {
        let o = call(mode, args).await;
        assert_eq!(o.code, Some(0), "roll {args:?}: {}\n{}", o.stderr, o.stdout);
        assert!(!o.stdout.contains("fakevalue") && !o.stderr.contains("fakevalue"), "a value printed: {}{}", o.stdout, o.stderr);
        o
    };
    // A failure or a collision stops it before anything is written (into staging, which stays
    // empty), naming what stopped it and never a value.
    let stops = async |mode: &str, args: &[&str], why: &str| {
        let o = call(mode, args).await;
        assert!(o.code != Some(0) && o.stderr.contains(why), "roll {args:?} should stop with {why:?}: {}\n{}", o.stderr, o.stdout);
        assert!(!o.stdout.contains("fakevalue") && !o.stderr.contains("fakevalue"), "a value printed: {}{}", o.stdout, o.stderr);
    };

    // 1Password: a dry run first (names only), then the vault; one item's fields by its name.
    let dry = done("all", &import("production", "/op", "1password", "Fake", &["--dry-run"])).await.stdout;
    assert!(dry.contains("STRIPE_CREDENTIAL") && dry.contains("MAIN_DB_REPLICA_PORT") && !dry.contains("NOTES") && !dry.contains("ONE_TIME") && dry.contains("Dry run: nothing imported (4 keys in 1 change)."), "{dry}");
    let one = done("all", &import("production", "/op", "1password", "Fake/Main DB", &["--dry-run"])).await.stdout;
    assert!(one.contains("  PASSWORD\n") && one.contains("  REPLICA_PORT\n") && one.contains("  USERNAME\n"), "{one}");
    done("all", &import("production", "/op", "1password", "Fake", &[])).await;
    assert_eq!(ok(&api, &[], &run("/op", "echo $STRIPE_CREDENTIAL $MAIN_DB_USERNAME $MAIN_DB_PASSWORD $MAIN_DB_REPLICA_PORT"), None).await.trim(), "fakevalue-op-1 fakevalue-op-2 fakevalue-op-3 6543");
    stops("fail", &import("staging", "/op", "1password", "Fake", &[]), "1password Fake/Main DB").await;
    stops("collide", &import("staging", "/op", "1password", "Fake", &[]), "op://Fake/Stripe/credential and op://Fake/stripe/credential both give STRIPE_CREDENTIAL").await;

    done("all", &import("production", "/bws", "bitwarden", project, &[])).await;
    assert_eq!(ok(&api, &[], &run("/bws", "echo $FAKE_API_KEY $FAKE_TOKEN"), None).await.trim(), "fakevalue-bws-1 fakevalue-bws-2");
    stops("fail", &import("staging", "/bws", "bitwarden", project, &[]), "401 Unauthorized").await;
    stops("collide", &import("staging", "/bws", "bitwarden", project, &[]), "bitwarden secret 00000000-0000-0000-0000-000000000002 and bitwarden secret 00000000-0000-0000-0000-000000000003 both give FAKE_TOKEN").await;

    // Parameter Store: the path's tree into folders, one change each; SecureStrings sensitive.
    let dry = done("all", &import("production", "/ssm", "aws-parameter-store", "/fake/shop", &["--sensitive", "--dry-run"])).await.stdout;
    assert!(dry.contains("/ssm/db: 2 keys\n  DB_USER\n  PASSWORD  (sensitive)\n"), "{dry}");
    let out = done("all", &import("production", "/ssm", "aws-parameter-store", "/fake/shop", &["--sensitive"])).await.stdout;
    assert!(out.contains("/ssm: 1 value") && out.contains("/ssm/db: 2 values"), "{out}");
    assert_eq!(ok(&api, &[], &run("/ssm", "echo $API_URL"), None).await.trim(), "https://fakevalue.example");
    assert_eq!(ok(&api, &[], &run("/ssm/db", "echo $DB_USER"), None).await.trim(), "fakevalue-ssm-2");
    let sensitive = done("all", &run("/ssm/db", "echo ${PASSWORD:-withheld}")).await;
    assert!(sensitive.stdout.trim() == "withheld" && sensitive.stderr.contains("left out PASSWORD (sensitive"), "{}{}", sensitive.stdout, sensitive.stderr);
    stops("fail", &import("staging", "/ssm", "aws-parameter-store", "/fake/shop", &[]), "AccessDeniedException").await;
    stops("collide", &import("staging", "/ssm", "aws-parameter-store", "/fake/shop", &[]), "/fake/shop/API_URL and /fake/shop/api-url both give API_URL").await;

    // Google: the prefix's secrets, a secret with no version left out (named), others alone.
    let o = done("all", &import("production", "/gcp", "gcp", "fake-project/fake-", &[])).await;
    assert!(o.stderr.contains("gcp fake-project/fake-empty: no version, left out"), "{}", o.stderr);
    assert_eq!(ok(&api, &[], &run("/gcp", "echo $DB_URL $API_KEY ${EMPTY:-none} ${OTHER_THING:-none}"), None).await.trim(), "fakevalue-gcp-1 fakevalue-gcp-2 none none");
    stops("fail", &import("staging", "/gcp", "gcp", "fake-project/fake-", &[]), "PERMISSION_DENIED").await;
    stops("collide", &import("staging", "/gcp", "gcp", "fake-project/fake-", &[]), "gcp fake-project/fake-db-url and gcp fake-project/fake-DB_URL both give DB_URL").await;

    let o = done("all", &import("production", "/az", "azure", "fake-vault", &[])).await;
    assert!(o.stderr.contains("azure fake-vault/old-key: disabled, left out"), "{}", o.stderr);
    assert_eq!(ok(&api, &[], &run("/az", "echo $DB_URL $API_KEY ${OLD_KEY:-none}"), None).await.trim(), "fakevalue-az-1 fakevalue-az-2 none");
    stops("fail", &import("staging", "/az", "azure", "fake-vault", &[]), "does not have secrets get permission").await;

    done("all", &import("production", "/akl", "akeyless", "/fake/shop", &[])).await;
    assert_eq!(ok(&api, &[], &run("/akl", "echo $API_KEY"), None).await.trim(), "fakevalue-akl-1");
    assert_eq!(ok(&api, &[], &run("/akl/db", "echo $URL"), None).await.trim(), "fakevalue-akl-2");
    stops("fail", &import("staging", "/akl", "akeyless", "/fake/shop", &[]), "akeyless /fake/shop/db/url").await;
    stops("collide", &import("staging", "/akl", "akeyless", "/fake/shop", &[]), "akeyless /fake/shop/api-key and akeyless /fake/shop/API_KEY both give API_KEY").await;

    for at in ["/op", "/bws", "/ssm", "/gcp", "/az", "/akl"] {
        let staged = call("all", &["secrets", "-p", "shop", "-e", "staging", "--path", at, "ls"]).await;
        assert!(!["STRIPE_CREDENTIAL", "MAIN_DB", "FAKE_", "API_URL", "DB_URL", "API_KEY"].iter().any(|k| staged.stdout.contains(k)), "a stopped import wrote into {at}: {}", staged.stdout);
    }
    let argv = std::fs::read_to_string(dir.join("argv.log")).unwrap();
    assert!(argv.contains("gcloud secrets versions access latest --secret fake-db-url") && !argv.contains("fakevalue"), "{argv}");
}

/// A sealed start's ceremony: `key split` names the split rolld starts sealed with, and `unseal`
/// gives a sealed node one share at a time (read from stdin, printed nowhere) until it unseals.
#[tokio::test(flavor = "multi_thread")]
async fn roll_secrets_unseal_gives_a_sealed_node_its_shares() {
    const KEY: &str = "fake-deployment-key-for-the-cli-test-000";
    let split = roll("http://127.0.0.1:9", &[], &["secrets", "key", "split", "--shares", "3", "--threshold", "2"], Some(KEY)).await;
    assert_eq!(split.code, Some(0), "{}", split.stderr);
    let shares: Vec<&str> = split.stdout.lines().collect();
    let named = split.stderr.split("ROLL_SECRET_KEY_SEALED=").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or_else(|| panic!("{}", split.stderr));
    let sealed = roll_secrets::unseal::Sealed::parse(named).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
    let (unsealer, unsealed) = roll_secrets::unseal::Unsealer::new(sealed, db).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let node = roll_secrets::unseal::sealed_router(unsealer).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, node).await.unwrap() });

    assert!(ok(&api, &[], &["secrets", "unseal", "--status"], None).await.contains("sealed, 0 of 2 shares"));
    let one = roll(&api, &[], &["secrets", "unseal"], Some(&format!("{}\n", shares[2]))).await;
    assert!(one.code == Some(0) && one.stdout.contains("sealed, 1 of 2 shares"), "{}{}", one.stdout, one.stderr);
    assert!(!format!("{}{}", one.stdout, one.stderr).contains("rlks_"), "a share was printed");
    let done = ok(&api, &[], &["secrets", "unseal", "--node", &api], Some(&format!("{}\n", shares[0]))).await;
    assert_eq!(done.trim(), format!("{api}: unsealed"));
    assert_eq!(unsealed.await.unwrap().as_str(), KEY);
}

#[tokio::test]
async fn an_exported_audit_log_verifies_offline_with_saved_keys_for_its_own_project_and_an_edit_fails_it() {
    let Server { api, secrets, projects, _dir, .. } = start().await;
    let shop = projects.list(roll_workspace::Scope::All).unwrap().into_iter().find(|p| p.name == "shop").unwrap().id;
    let at = ["secrets", "-p", "shop", "-e", "production"];
    ok(&api, &[], &[&at[..], &["set", "HOST=db.internal", "PORT=5432"]].concat(), None).await;
    let s = secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep_audit_chain(i64::MAX / 2).unwrap()).await.unwrap();
    ok(&api, &[], &[&at[..], &["set", "PORT=6543"]].concat(), None).await;
    let export = ok(&api, &[], &["secrets", "-p", "shop", "audit", "--format", "jsonl", "--limit", "10000"], None).await;
    let files = tempfile::tempdir().unwrap();
    let (log, keys) = (files.path().join("audit.jsonl"), files.path().join("jwks.json"));
    std::fs::write(&keys, secrets.oidc_jwks().unwrap().to_string()).unwrap();
    std::fs::write(&log, &export).unwrap();
    // Nothing but the file, the keys and the project's id: the API isn't there.
    let offline = "http://127.0.0.1:9";
    let verify = ["secrets", "-p", shop.as_str(), "audit", "verify", "--file", log.to_str().unwrap(), "--jwks", keys.to_str().unwrap()];
    let out = ok(offline, &[], &verify, None).await;
    assert!(out.contains("signed through #"), "{out}");
    // Keys fetched from the server being checked would vouch for themselves: saved ones only.
    let o = roll(offline, &[], &verify[..7], None).await;
    assert!(o.code != Some(0) && o.stderr.contains("--jwks"), "{}", o.stderr);
    // Another project's log isn't this one's, however well it verifies.
    let mut other = verify;
    other[2] = "prj_someoneelse";
    let o = roll(offline, &[], &other, None).await;
    assert!(o.code != Some(0) && o.stderr.contains("not prj_someoneelse's"), "{}", o.stderr);
    std::fs::write(&log, export.replacen("\"summary\":\"", "\"summary\":\"not ", 1)).unwrap();
    let o = roll(offline, &[], &verify, None).await;
    assert!(o.code != Some(0) && o.stderr.contains("edited"), "{}{}", o.stdout, o.stderr);
}

/// `roll run --watch`: a write restarts the command with the new value, a poll that fails waits its
/// interval and keeps the command running (never spins), and SIGTERM to roll reaches the command,
/// roll exiting as it did.
#[cfg(unix)]
#[tokio::test]
async fn roll_run_watch_restarts_on_a_write_waits_out_failures_and_passes_sigterm_on() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let s = start().await;
    let at = ["secrets", "-p", "shop", "-e", "production"];
    ok(&s.api, &[], &[&at[..], &["set", "GREETING=one"]].concat(), None).await;
    let seen_at = s._dir.path().join("seen");
    let seen = || std::fs::read_to_string(&seen_at).unwrap_or_default();
    let wait_for = |want: &'static str| {
        let seen = &seen;
        async move {
            let started = std::time::Instant::now();
            while seen() != want {
                assert!(started.elapsed() < Duration::from_secs(20), "waited for {want:?}, the command wrote {:?}", seen());
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    // Each start writes its value; SIGTERM writes `term` and exits 7.
    let script = r#"trap 'echo term >> "$SEEN"; exit 7' TERM; echo "$GREETING" >> "$SEEN"; while :; do sleep 0.05; done"#;
    // Every poll fails from the start: the first read (values, not the digest) still starts it.
    s.polls.failing.store(true, Ordering::SeqCst);
    let mut roll = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll"))
        .args(["run", "-p", "shop", "-e", "production", "--watch", "--interval", "1", "--no-mask", "--", "sh", "-c", script])
        .env("ROLL_API_URL", &s.api)
        .env("SEEN", &seen_at)
        .env_remove("ROLL_TOKEN")
        .env_remove("ROLL_PROJECT")
        .env_remove("ROLL_ENV")
        .env_remove("ROLL_ADMIN_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for("one\n").await;
    let before = s.polls.asked.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let polls = s.polls.asked.load(Ordering::SeqCst) - before;
    assert!((2..=6).contains(&polls), "{polls} failing polls in 3 s at a 1 s interval");
    assert_eq!(seen(), "one\n", "a failing poll keeps the running command");

    s.polls.failing.store(false, Ordering::SeqCst);
    ok(&s.api, &[], &[&at[..], &["set", "GREETING=two"]].concat(), None).await;
    wait_for("one\nterm\ntwo\n").await;

    // SAFETY: kill(2) on the roll process this test started.
    unsafe { libc::kill(roll.id().unwrap() as libc::pid_t, libc::SIGTERM) };
    let status = tokio::time::timeout(Duration::from_secs(10), roll.wait()).await.expect("roll exits with its command").unwrap();
    assert_eq!(status.code(), Some(7));
    assert_eq!(seen(), "one\nterm\ntwo\nterm\n");
}

/// A repo's roll.toml suggests where its secrets are, and the person approves it (direnv's
/// `allow`): unapproved or changed it's ignored with a hint, flags beat it, and it never chooses a
/// protected environment.
#[tokio::test]
async fn a_repos_roll_toml_suggests_where_its_secrets_are_and_the_person_approves_it() {
    let Server { api, .. } = start().await;
    for (env, value) in [("development", "dev"), ("staging", "stg"), ("production", "prod")] {
        ok(&api, &[], &["secrets", "-p", "shop", "-e", env, "set", &format!("WHERE={value}")], None).await;
    }
    ok(&api, &[], &["secrets", "-p", "shop", "envs", "protect", "production", "--approvals", "1"], None).await;

    let home = tempfile::tempdir().unwrap();
    let (repo, web, cfg) = (home.path().join("repo"), home.path().join("repo/web"), home.path().join("config"));
    std::fs::create_dir_all(&web).unwrap();
    let toml = |secrets: &str| std::fs::write(repo.join("roll.toml"), format!("[queue]\nbatch_size = 2\n\n[secrets]\nunstored_credentials = \"warn\"\n{secrets}")).unwrap();
    let e = [("XDG_CONFIG_HOME", cfg.to_str().unwrap()), ("ROLL_TEST_CWD", web.to_str().unwrap())];
    let echo = ["run", "--no-mask", "--", "sh", "-c", "echo $WHERE"];

    // Unapproved: nothing from it is used (so no project here), and roll says how to approve it.
    // A key a repo never holds is named and left out.
    toml("project = \"shop\"\nenvironment = \"staging\"\napi_url = \"https://elsewhere.example\"\n");
    let o = roll(&api, &e, &echo, None).await;
    assert!(o.code != Some(0) && o.stderr.contains("suggests project shop, environment staging, but it isn't used until you approve it: roll secrets setup"), "{}", o.stderr);
    assert!(o.stderr.contains("api_url: a repo never holds an API URL, a token or a value; ignored"), "{}", o.stderr);
    // Outside a terminal, setup asks for --approve rather than prompting.
    let o = roll(&api, &e, &["secrets", "setup"], None).await;
    assert!(o.code != Some(0) && o.stderr.contains("roll secrets setup --approve"), "{}", o.stderr);

    // Approved, it's used; flags beat it.
    assert!(ok(&api, &e, &["secrets", "setup", "--approve"], None).await.contains("Approved"));
    assert_eq!(ok(&api, &e, &echo, None).await.trim(), "stg");
    assert_eq!(ok(&api, &e, &["run", "-e", "development", "--no-mask", "--", "sh", "-c", "echo $WHERE"], None).await.trim(), "dev");

    // A changed table asks again.
    toml("project = \"shop\"\nenvironment = \"development\"\n");
    let o = roll(&api, &e, &echo, None).await;
    assert!(o.code != Some(0) && o.stderr.contains("changed since you approved it (it was: project shop, environment staging"), "{}", o.stderr);
    ok(&api, &e, &["secrets", "setup", "--approve"], None).await;
    assert_eq!(ok(&api, &e, &echo, None).await.trim(), "dev");

    // A protected environment is refused even approved; the person can still choose it.
    toml("project = \"shop\"\nenvironment = \"production\"\n");
    let approved = roll(&api, &e, &["secrets", "setup", "--approve"], None).await;
    assert!(approved.code == Some(0) && approved.stderr.contains("production is protected"), "{}", approved.stderr);
    let o = roll(&api, &e, &echo, None).await;
    assert!(o.code != Some(0) && o.stderr.contains("chooses production, which is protected: a repo never chooses a protected environment, even approved"), "{}", o.stderr);
    assert_eq!(ok(&api, &e, &["run", "-e", "production", "--no-mask", "--", "sh", "-c", "echo $WHERE"], None).await.trim(), "prod");

    // Environments by branch, with roll code's globs; doctor says where each choice came from.
    toml("project = \"shop\"\nenvironment = \"development\"\nbranches = { \"release/*\" = \"staging\" }\n");
    let git = std::process::Command::new("git").args(["init", "-q", "-b", "release/1"]).current_dir(&repo).status().unwrap();
    assert!(git.success());
    ok(&api, &e, &["secrets", "setup", "--approve"], None).await;
    assert_eq!(ok(&api, &e, &echo, None).await.trim(), "stg");
    let doctor = roll(&api, &e, &["secrets", "doctor"], None).await.stdout;
    assert!(doctor.contains("ok   using shop staging:/ (project from roll.toml, environment from roll.toml (branch release/*), path from the default)") && doctor.contains(": approved"), "{doctor}");
}
