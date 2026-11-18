//! `roll preview …` against the real `/v1/previews`, `/v1/workspace` and `/v1/access` routers,
//! on a real roll code with its git server (in process, on a temp dir, locked: git and its API
//! take the operator's token), as a signed-in admin `ana`. The reconciler doesn't run: this checks
//! the commands against the routes' shapes, not branching (crates/roll-previews/tests/engine.rs
//! does that).

use serde_json::{Value, json};
use std::sync::Arc;

const TOKEN: &str = "operator-token-0123456789abcdef";

struct Server {
    api: String,
    store: Arc<roll_previews::Store>,
    _home: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn server() -> Server {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().to_path_buf();
    let api = rt.block_on(async move {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(path.join("objects"))).unwrap();
        let mut config = code_control::Config::local(path.clone());
        config.admin_token = Some(TOKEN.into());
        config.git_url = format!("http://{addr}/git");
        let code = code_control::Code::open(config, objects).unwrap();
        let caller = code_control::Caller::Admin;
        code.create_owner(&caller, "acme", code_control::model::OwnerKind::Org, None).unwrap();
        let repo = code.create_repo(&caller, &json!({ "owner": "acme", "name": "web" })).await.unwrap();
        let files = serde_json::from_value(json!({ "message": "init", "files": { "index.html": { "content": "hi" } } })).unwrap();
        code.land_api(&repo, &caller, &files, None, false).await.unwrap();
        let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(path.join("ws.db")), Default::default()).unwrap());
        let shop = ws.create("", "shop", "", None).unwrap();
        ws.place(roll_workspace::Scope::All, &shop.id, roll_workspace::Kind::Repo, "acme/web", 0.0, 0.0).unwrap();
        let store = Arc::new(roll_previews::Store::open(&code_sql::Target::Sqlite(path.join("previews.db")), Default::default()).unwrap());
        let store_for_test = store.clone();
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
        let kept = store_for_test.clone();
        let access = roll_access::Access::open(&code_sql::Target::Sqlite(path.join("access.db")), Default::default(), Default::default(), Default::default()).unwrap();
        // Signed in as rolld's gate leaves it: an admin, `ana`.
        let ana = roll_auth::SignedIn { user: "usr_ana".into(), handle: "ana".into(), admin: true, code_owner: None, session: "ses_ana".into() };
        let router = roll_previews::api::router(engine)
            .merge(roll_workspace::router(ws))
            .merge(roll_access::api::router(access))
            .merge(code_control::api::router(code))
            .layer(axum::Extension(ana));
        tokio::spawn(async move { axum::serve(l, router).await });
        (format!("http://{addr}"), kept)
    });
    let (api, store) = api;
    Server { api, store, _home: home, _rt: rt }
}

impl Server {
    fn roll(&self, args: &[&str]) -> (bool, String, String) {
        self.roll_in(self._home.path(), args)
    }

    /// In `dir` (a checkout), git's config kept to it (`HOME`).
    fn roll_in(&self, dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_roll"))
            .current_dir(dir)
            .envs(git_env(self._home.path()))
            .env("ROLL_API_URL", &self.api)
            .env("ROLL_ADMIN_TOKEN", TOKEN)
            .arg("preview")
            .args(args)
            .output()
            .unwrap();
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

    // Its screenshot to a file, byte for byte; none to save is an error that says so.
    let out = s._home.path().join("shot.png");
    let (ok, _, err) = s.roll(&["screenshot", &id, "-o", out.to_str().unwrap()]);
    assert!(!ok && err.contains("no screenshot"), "{err}");
    let png = b"\x89PNG\r\n\x1a\nnot really".to_vec();
    s.store.set_screenshot(&id, "cs_1", &png, None, None).unwrap();
    s.ok(&["screenshot", &id, "-o", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(&out).unwrap(), png);

    s.ok(&["rm", &id]);
    assert_eq!(s.json(&["show", &id])["desired"], "deleted");
    let (ok, _, err) = s.roll(&["wait", &id]);
    assert!(!ok && err.contains("being deleted"), "{err}");
}

/// git kept to `home`: no system or user config of this machine's, a fixed identity.
fn git_env(home: &std::path::Path) -> Vec<(&'static str, std::ffi::OsString)> {
    vec![
        ("HOME", home.as_os_str().to_owned()),
        ("XDG_CONFIG_HOME", home.join(".config").into_os_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_AUTHOR_NAME", "Ana".into()),
        ("GIT_AUTHOR_EMAIL", "ana@example.com".into()),
        ("GIT_COMMITTER_NAME", "Ana".into()),
        ("GIT_COMMITTER_EMAIL", "ana@example.com".into()),
    ]
}

impl Server {
    /// `git <args>` in `dir`; with `auth`, the operator's token for this call only.
    fn git(&self, dir: &std::path::Path, auth: bool, args: &[&str]) -> String {
        let mut c = std::process::Command::new("git");
        c.current_dir(dir).envs(git_env(self._home.path()));
        if auth {
            c.args(["-c", &format!("http.extraHeader=Authorization: Bearer {TOKEN}")]);
        }
        let out = c.args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What `branch` holds in roll code: its commit's subject and parents, and `index.html`,
    /// `new.txt` and whether `secret.env` is there. `None`: no such branch.
    fn pushed(&self, branch: &str) -> Option<(String, String, String, bool)> {
        let dir = tempfile::tempdir_in(self._home.path()).unwrap();
        let url = format!("{}/git/acme/web.git", self.api);
        if self.git(dir.path(), true, &["ls-remote", &url, &format!("refs/heads/{branch}")]).is_empty() {
            return None;
        }
        self.git(dir.path(), true, &["clone", "-q", "--branch", branch, &url, "c"]);
        let c = dir.path().join("c");
        let read = |f: &str| std::fs::read_to_string(c.join(f)).unwrap_or_default();
        Some((self.git(&c, false, &["log", "-1", "--format=%s %P"]), read("index.html"), read("new.txt"), c.join("secret.env").exists()))
    }
}

#[test]
fn up_previews_the_working_tree_and_down_removes_it() {
    let s = server();
    s.ok(&["settings", "shop", "--set", "enabled=true"]);
    // A checkout of roll's repo, on a branch, with changes nobody committed: an edit, a new file,
    // something staged, and a file .gitignore leaves out.
    let url = format!("{}/git/acme/web.git", s.api);
    s.git(s._home.path(), true, &["clone", "-q", &url, "work"]);
    let work = s._home.path().join("work");
    s.git(&work, false, &["switch", "-q", "-c", "feat/x"]);
    std::fs::write(work.join("index.html"), "changed").unwrap();
    std::fs::write(work.join("staged.txt"), "staged").unwrap();
    s.git(&work, false, &["add", "staged.txt"]);
    std::fs::write(work.join("new.txt"), "new").unwrap();
    std::fs::write(work.join(".gitignore"), "secret.env\n").unwrap();
    std::fs::write(work.join("secret.env"), "KEY=1").unwrap();
    let mine = |s: &Server| [&["rev-parse", "HEAD"][..], &["ls-files", "--stage"], &["status", "--porcelain"], &["for-each-ref"], &["config", "--list", "--local"]].map(|a| s.git(&work, false, a));
    let before = mine(&s);
    let head = before[0].clone();

    let up = |s: &Server| {
        let (ok, out, err) = s.roll_in(&work, &["--json", "up", "--no-wait"]);
        assert!(ok, "roll preview up:\n{out}\n{err}");
        serde_json::from_str::<Value>(&out).unwrap()
    };
    let p = up(&s);
    let id = p["id"].as_str().unwrap().to_string();
    assert_eq!((p["branch"].as_str(), p["repo"].as_str()), (Some("preview/ana/feat-x"), Some("acme/web")), "{p}");
    let (log, index, new, secret) = s.pushed("preview/ana/feat-x").expect("the branch");
    assert_eq!((log, index.as_str(), new.as_str(), secret), (format!("roll preview up {head}"), "changed", "new", false));
    // Nothing of the user's moved: HEAD, the index, refs, files, config (no credential written).
    assert_eq!(mine(&s), before);

    // Again, after another edit: the same preview, its branch moved (forced: both are on HEAD).
    std::fs::write(work.join("index.html"), "again").unwrap();
    let again = up(&s);
    assert_eq!(again["id"].as_str(), Some(id.as_str()));
    assert_eq!(s.pushed("preview/ana/feat-x").unwrap().1, "again");

    let (ok, out, err) = s.roll_in(&work, &["--json", "down"]);
    assert!(ok, "roll preview down:\n{out}\n{err}");
    assert_eq!(s.json(&["show", &id])["desired"], "deleted");
    assert!(s.pushed("preview/ana/feat-x").is_none(), "the branch is gone");
    assert_eq!(s.git(&work, false, &["rev-parse", "HEAD"]), head);
}
