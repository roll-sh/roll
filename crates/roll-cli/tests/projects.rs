//! `roll projects …` against the real `/v1/workspace` router in process (docs/members.md), as a
//! signed-in admin `ana`: invite, accept (in process: accepting is a person's session), list,
//! change a role, remove.

use std::sync::Arc;

struct People;

impl roll_workspace::People for People {
    fn person(&self, user: &str) -> Option<roll_workspace::Person> {
        let handle = user.strip_prefix("usr_")?;
        Some(roll_workspace::Person { handle: handle.into(), email: Some(format!("{handle}@example.com")), admin: handle == "ana" })
    }
}

struct Server {
    api: String,
    ws: Arc<roll_workspace::Workspace>,
    _home: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn server() -> Server {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let home = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(home.path().join("ws.db")), Default::default()).unwrap());
    ws.set_people(Arc::new(People));
    ws.set_web_url("https://roll.example");
    ws.create("", "shop", "", None).unwrap();
    let w = ws.clone();
    let api = rt.block_on(async move {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let ana = roll_auth::SignedIn { user: "usr_ana".into(), handle: "ana".into(), admin: true, code_owner: None, session: "ses_ana".into() };
        let router = roll_workspace::router(w).layer(axum::Extension(ana));
        tokio::spawn(async move { axum::serve(l, router).await });
        format!("http://{addr}")
    });
    Server { api, ws, _home: home, _rt: rt }
}

impl Server {
    fn roll(&self, args: &[&str]) -> String {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_roll")).env("ROLL_API_URL", &self.api).env("HOME", self._home.path()).env_remove("ROLL_TOKEN").env_remove("ROLL_ADMIN_TOKEN").args(args).output().unwrap();
        let (out, err) = (String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string());
        assert!(o.status.success(), "roll {args:?}: {out}{err}");
        out
    }
}

#[test]
fn members_are_invited_listed_changed_and_removed() {
    let s = server();
    let out = s.roll(&["projects", "members", "invite", "bob@example.com", "--role", "developer", "--env", "staging,production", "-p", "shop"]);
    assert!(out.contains("invited bob@example.com as developer") && out.contains("https://roll.example/dashboard/invites?token=rlv_"), "{out}");
    let token = out.split("token=").nth(1).unwrap().trim().to_string();
    assert!(s.roll(&["projects", "invites", "list", "-p", "shop"]).contains("bob@example.com"));
    s.ws.accept("usr_bob", &token, None).unwrap();

    let list = s.roll(&["projects", "members", "list", "-p", "shop"]);
    assert!(list.contains("bob") && list.contains("developer") && list.contains("staging,production"), "{list}");
    let changed = s.roll(&["projects", "members", "set-role", "bob", "--role", "viewer", "-p", "shop"]);
    assert!(changed.contains("viewer"), "{changed}");
    assert!(s.roll(&["projects", "invites", "list", "-p", "shop"]).trim().is_empty(), "the invite was used");
    s.roll(&["projects", "members", "remove", "bob", "-p", "shop"]);
    assert!(s.roll(&["projects", "members", "list", "-p", "shop"]).trim().is_empty());
    let roles = s.roll(&["projects", "roles"]);
    assert!(roles.contains("developer") && roles.contains("where not protected"), "{roles}");
    assert!(s.roll(&["invites", "accept", "pit_x"]).contains("/dashboard/invites?token=pit_x"));
}
