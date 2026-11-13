//! `roll preview …` against the real `/v1/previews` and `/v1/workspace` routers, on a real
//! roll code (in process, on a temp dir). The reconciler doesn't run: this checks the commands
//! against the routes' shapes, not branching (crates/roll-previews/tests/engine.rs does that).

use serde_json::{Value, json};
use std::sync::Arc;

struct Server {
    api: String,
    _home: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn server() -> Server {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().to_path_buf();
    let api = rt.block_on(async move {
        let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(path.join("objects"))).unwrap();
        let code = code_control::Code::open(code_control::Config::local(path.clone()), objects).unwrap();
        let caller = code_control::Caller::Admin;
        code.create_owner(&caller, "acme", code_control::model::OwnerKind::Org, None).unwrap();
        let repo = code.create_repo(&caller, &json!({ "owner": "acme", "name": "web" })).await.unwrap();
        let files = serde_json::from_value(json!({ "message": "init", "files": { "index.html": { "content": "hi" } } })).unwrap();
        code.land_api(&repo, &caller, &files, None, false).await.unwrap();
        let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(path.join("ws.db")), Default::default()).unwrap());
        let shop = ws.create("shop", "", None).unwrap();
        ws.place(&shop.id, roll_workspace::Kind::Repo, "acme/web", 0.0, 0.0).unwrap();
        let store = Arc::new(roll_previews::Store::open(&code_sql::Target::Sqlite(path.join("previews.db")), Default::default()).unwrap());
        let source = roll_previews::code::CodeSource::new(code.clone());
        let runtime = roll_previews::runtime::Runtime::new(Default::default(), Arc::new(code_preview::docker::Docker), source.clone(), store.clone());
        let engine = roll_previews::Engine::new(
            Default::default(),
            store,
            ws.clone(),
            source,
            Vec::new(),
            runtime,
            Arc::new(roll_previews::ports::NoSecrets),
            Box::new(|| false),
        );
        let router = roll_previews::api::router(engine).merge(roll_workspace::router(ws));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router).await });
        format!("http://{addr}")
    });
    Server { api, _home: home, _rt: rt }
}

impl Server {
    fn roll(&self, args: &[&str]) -> (bool, String, String) {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_roll")).env("ROLL_API_URL", &self.api).arg("preview").args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, out, err) = self.roll(args);
        assert!(ok, "roll preview {args:?} failed:\n{out}\n{err}");
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.ok(&a);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }
}

#[test]
fn settings_create_list_and_remove_a_branch_preview() {
    let s = server();
    // Rules by project name, nested keys with --set; a production base needs a yes.
    let rules = s.json(&["settings", "shop", "--set", "enabled=true", "--set", "branches=[\"feat/*\",\"main\"]", "--set", "max_previews=2"]);
    assert_eq!((rules["enabled"].clone(), rules["max_previews"].clone()), (json!(true), json!(2)));
    let (ok, _, err) = s.roll(&["settings", "shop", "--set", "secrets.base=production"]);
    assert!(!ok && err.contains("allow_production"), "{err}");

    let p = s.json(&["create", "--project", "shop", "--repo", "acme/web", "--branch", "main"]);
    let id = p["id"].as_str().unwrap().to_string();
    assert_eq!(p["state"], "creating");
    assert_eq!(p["branch"], "main");
    let listed = s.ok(&["ls", "--project", "shop"]);
    assert!(listed.contains(&id) && listed.contains("acme/web"), "{listed}");
    let (ok, _, err) = s.roll(&["create", "--project", "shop", "--repo", "acme/nope", "--branch", "main"]);
    assert!(!ok && err.contains("isn't in project"), "{err}");

    // CI waits for it: not ready in time (the reconciler doesn't run here) is a failure that
    // says where it got to; so is a preview being deleted.
    let (ok, _, err) = s.roll(&["wait", &id, "--timeout", "0"]);
    assert!(!ok && err.contains("not ready after 0s (it's creating)"), "{err}");

    s.ok(&["rm", &id]);
    assert_eq!(s.json(&["show", &id])["desired"], "deleted");
    let (ok, _, err) = s.roll(&["wait", &id]);
    assert!(!ok && err.contains("being deleted"), "{err}");
}
