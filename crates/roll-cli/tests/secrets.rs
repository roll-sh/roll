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

#[tokio::test]
async fn roll_run_injects_an_environments_secrets_and_refuses_unresolved_references() {
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
        kms: None,
        sts: None,
    };
    let secrets = Arc::new(roll_secrets::Secrets::new(db, ws.clone(), opts).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let projects = ws.clone();
    let router = roll_workspace::router(ws).merge(roll_secrets::router(secrets));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let at = ["secrets", "-p", "shop", "-e", "production"];
    ok(&api, &[], &[&at[..], &["set", "HOST=db.internal", "PORT=5432"]].concat(), None).await;
    ok(&api, &[], &[&at[..], &["set", "PASSWORD"]].concat(), Some("s3cret\n")).await;
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
    std::fs::write(&env_file, "MISSING=roll://production/NOPE\n").unwrap();
    let refused = roll(&api, &[], &["run", "-p", "shop", "-e", "production", "--env-file", env_file.to_str().unwrap(), "--", "true"], None).await;
    assert!(refused.code != Some(0) && refused.stderr.contains("roll://production/NOPE"), "{}", refused.stderr);

    // A scoped token implies its project and reaches only its environments.
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
    let echo = ["run", "-p", "shop", "-e", "production", "--fallback", fb, "--", "sh", "-c", "echo $HOST"];
    assert_eq!(ok(&api, &key, &echo, None).await.trim(), "db.internal");
    assert!(!std::fs::read_to_string(&fallback).unwrap().contains("s3cret"), "kept encrypted");
    let down = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
    };
    assert_eq!(ok(&down, &key, &echo, None).await.trim(), "db.internal");
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
    assert!(doctor.contains("ok   this directory is set up for shop production:/") && !doctor.contains("FAIL"), "{doctor}");

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
    projects.create("", "copy", "", None).unwrap();
    ok(&api, &[], &["secrets", "-p", "copy", "restore", file, "--key", key], None).await;
    let restored = ok(&api, &[], &["run", "-p", "copy", "-e", "production", "--allow-unresolved", "--no-mask", "--", "sh", "-c", "echo $HOST $PASSWORD"], None).await;
    assert_eq!(restored.trim(), "db.internal s3cret");
}
