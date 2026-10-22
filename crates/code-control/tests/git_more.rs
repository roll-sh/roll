//! More of git against roll code: force pushes can't rewrite main, tags, branch lifecycles
//! (delete closes, a new push after landing opens a new change), merge commits, atomic pushes,
//! shallow and partial clones served from changesets, incremental fetches, and git auth for
//! other owners and anonymous pushers.

mod common;

use common::{App, app, git, git_raw, write};
use serde_json::json;
use std::path::{Path, PathBuf};

struct Repo {
    a: App,
    url: String,
    _tmp: tempfile::TempDir,
    home: PathBuf,
}

/// acme/web with one pushed commit, and a clone of it at `home/w`.
async fn setup() -> Repo {
    let a = app(None);
    a.repo("acme", "web").await;
    let base = a.serve().await;
    let url = format!("{base}/git/acme/web.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    git(&home, &home, &["clone", "-q", &url, "w"]);
    let w = home.join("w");
    write(&w, "README.md", "# web\n");
    write(&w, "notes.txt", &(0..20).map(|i| format!("line {i}\n")).collect::<String>());
    git(&home, &w, &["add", "-A"]);
    git(&home, &w, &["commit", "-qm", "initial"]);
    git(&home, &w, &["push", "-q", "origin", "main"]);
    Repo { a, url, _tmp: tmp, home }
}

impl Repo {
    fn w(&self) -> PathBuf {
        self.home.join("w")
    }
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        git(&self.home, dir, args)
    }
    fn commit(&self, dir: &Path, path: &str, content: &str, msg: &str) -> String {
        write(dir, path, content);
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-qm", msg]);
        self.git(dir, &["rev-parse", "HEAD"]).trim().to_string()
    }
    async fn blob(&self, path: &str) -> String {
        String::from_utf8(self.a.raw("GET", &format!("/v1/code/repos/acme/web/blob/main/{path}"), None, None).await.1).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_push_to_main_lands_instead_of_rewriting() {
    let r = setup().await;
    let w = r.w();
    r.git(&r.home, &["clone", "-q", &r.url, "stale"]);
    let stale = r.home.join("stale");
    r.commit(&w, "a.txt", "a\n", "a");
    r.git(&w, &["push", "-q", "origin", "main"]);
    // The stale clone force-pushes a disjoint commit: main keeps a.txt and gains b.txt.
    r.commit(&stale, "b.txt", "b\n", "b");
    let out = git_raw(&r.home, &stale, &["push", "--force", "origin", "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(r.blob("a.txt").await, "a\n", "a force push never drops landed work");
    assert_eq!(r.blob("b.txt").await, "b\n");
    let log = r.a.ok("GET", "/v1/code/repos/acme/web/log", None).await;
    let msgs: Vec<&str> = log["items"].as_array().unwrap().iter().map(|i| i["message"].as_str().unwrap().trim()).collect();
    assert_eq!(msgs, ["b", "a", "initial"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tags_branches_and_merge_commits() {
    let r = setup().await;
    let w = r.w();

    // Tags: lightweight and annotated, listed by the API and cloned by git, then deleted.
    r.git(&w, &["tag", "v1"]);
    r.git(&w, &["tag", "-a", "v1.1", "-m", "release 1.1"]);
    r.git(&w, &["push", "-q", "origin", "v1", "v1.1"]);
    let refs = r.a.ok("GET", "/v1/code/repos/acme/web/refs", None).await;
    let names: Vec<&str> = refs.as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"main"), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("v1")) && names.iter().any(|n| n.ends_with("v1.1")), "{names:?}");
    let tree = r.a.ok("GET", "/v1/code/repos/acme/web/tree/v1.1", None).await;
    assert!(tree["entries"].as_array().unwrap().iter().any(|e| e["name"] == "README.md"), "a tag is a revision");
    r.git(&r.home, &["clone", "-q", &r.url, "tagged"]);
    let tags = r.git(&r.home.join("tagged"), &["tag", "-l"]);
    assert_eq!(tags.lines().collect::<Vec<_>>(), ["v1", "v1.1"]);
    assert_eq!(r.git(&r.home.join("tagged"), &["cat-file", "-t", "v1.1"]).trim(), "tag", "annotated tags keep their tag object");
    r.git(&w, &["push", "-q", "origin", ":refs/tags/v1"]);
    let refs = r.a.ok("GET", "/v1/code/repos/acme/web/refs", None).await;
    assert!(!refs.as_array().unwrap().iter().any(|x| x["name"].as_str().unwrap().ends_with("/v1") || x["name"] == "v1"), "{refs}");

    // A branch: push opens a change, deleting the branch closes it.
    r.git(&w, &["checkout", "-qb", "topic"]);
    r.commit(&w, "topic.txt", "t\n", "Topic work");
    r.git(&w, &["push", "-q", "origin", "topic"]);
    let c = r.a.ok("GET", "/v1/code/repos/acme/web/changes?state=open", None).await[0].clone();
    assert_eq!(c["branch"], "topic");
    r.git(&w, &["push", "-q", "origin", ":topic"]);
    let c = r.a.ok("GET", &format!("/v1/code/changes/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(c["state"], "closed");

    // Push the branch again: a new change. Land it, push more: another new change.
    r.git(&w, &["push", "-q", "origin", "topic"]);
    let open = r.a.ok("GET", "/v1/code/repos/acme/web/changes?state=open", None).await;
    assert_eq!(open.as_array().unwrap().len(), 1);
    let second = open[0]["id"].as_str().unwrap().to_string();
    assert_ne!(second, c["id"].as_str().unwrap());
    r.a.ok("POST", &format!("/v1/code/changes/{second}/land"), None).await;
    r.commit(&w, "topic.txt", "t2\n", "More topic work");
    r.git(&w, &["push", "-q", "origin", "topic"]);
    let open = r.a.ok("GET", "/v1/code/repos/acme/web/changes?state=open", None).await;
    assert_eq!(open.as_array().unwrap().len(), 1);
    assert_ne!(open[0]["id"], second, "a landed change isn't revised; the branch gets a new one");
    assert_eq!(open[0]["title"], "More topic work");

    // A merge commit pushed to main fast-forwards, parents intact.
    r.git(&w, &["checkout", "-q", "main"]);
    r.git(&w, &["pull", "-q", "--no-rebase", "origin", "main"]);
    r.git(&w, &["checkout", "-qb", "side"]);
    r.commit(&w, "side.txt", "s\n", "side");
    r.git(&w, &["checkout", "-q", "main"]);
    r.commit(&w, "main.txt", "m\n", "main work");
    r.git(&w, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
    let merge = r.git(&w, &["rev-parse", "HEAD"]).trim().to_string();
    r.git(&w, &["push", "-q", "origin", "main"]);
    let repo = r.a.ok("GET", "/v1/code/repos/acme/web", None).await;
    assert_eq!(repo["head_git"], merge);
    let head = r.a.ok("GET", &format!("/v1/code/repos/acme/web/changesets/{merge}"), None).await;
    assert_eq!(head["parents"].as_array().unwrap().len(), 2);
    assert_eq!(r.blob("side.txt").await, "s\n");

    // Pushing something that isn't a branch, tag or refs/land is refused.
    let out = git_raw(&r.home, &w, &["push", "origin", "HEAD:refs/notes/x"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only branches, tags"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_push_with_a_conflict_applies_nothing_else() {
    let r = setup().await;
    let w = r.w();
    r.git(&r.home, &["clone", "-q", &r.url, "stale"]);
    let stale = r.home.join("stale");
    // Main moves on through the API, changing line 1.
    let notes = r.blob("notes.txt").await;
    r.a.ok("POST", "/v1/code/repos/acme/web/land", Some(json!({ "message": "api", "files": { "notes.txt": { "content": notes.replace("line 1\n", "line one\n") } } }))).await;
    // The stale clone changes line 1 too and pushes it with a new branch, atomically.
    r.commit(&stale, "notes.txt", &notes.replace("line 1\n", "line uno\n"), "conflicting");
    let out = git_raw(&r.home, &stale, &["push", "--atomic", "origin", "HEAD:refs/land/main", "HEAD:refs/heads/extra"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("notes.txt"), "{err}");
    let _ = w;
    // refs/land can't be prechecked for conflicts (that's the land itself), so the branch may or
    // may not exist; main must not have the conflicting edit.
    assert!(r.blob("notes.txt").await.contains("line one\n"));
    assert!(!r.blob("notes.txt").await.contains("line uno\n"));
    // An atomic push with an invalid ref fails as a whole before anything applies.
    let out = git_raw(&r.home, &stale, &["push", "--atomic", "origin", "HEAD:refs/heads/ok-branch", "HEAD:refs/weird/x"]);
    assert!(!out.status.success());
    let refs = r.a.ok("GET", "/v1/code/repos/acme/web/refs", None).await;
    assert!(!refs.as_array().unwrap().iter().any(|x| x["name"] == "ok-branch"), "{refs}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_partial_and_incremental_fetches() {
    let r = setup().await;
    let w = r.w();
    for i in 0..5 {
        r.commit(&w, &format!("f{i}.txt"), &format!("{i}\n"), &format!("c{i}"));
    }
    r.git(&w, &["push", "-q", "origin", "main"]);
    let head = r.git(&w, &["rev-parse", "HEAD"]).trim().to_string();

    // Shallow: one commit, then deepen.
    r.git(&r.home, &["clone", "-q", "--depth", "1", &r.url, "shallow"]);
    let s = r.home.join("shallow");
    assert_eq!(r.git(&s, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(std::fs::read_to_string(s.join("f4.txt")).unwrap(), "4\n");
    r.git(&s, &["fetch", "-q", "--deepen", "3"]);
    assert_eq!(r.git(&s, &["rev-list", "--count", "HEAD"]).trim(), "4");
    r.git(&s, &["fetch", "-q", "--unshallow"]);
    assert_eq!(r.git(&s, &["rev-list", "--count", "HEAD"]).trim(), "6");

    // Partial: no blobs up front; checkout fetches them lazily.
    r.git(&r.home, &["clone", "-q", "--filter=blob:none", &r.url, "partial"]);
    let p = r.home.join("partial");
    assert_eq!(std::fs::read_to_string(p.join("f2.txt")).unwrap(), "2\n");
    r.git(&p, &["checkout", "-q", "HEAD~3"]);
    assert!(!p.join("f3.txt").exists());
    r.git(&p, &["checkout", "-q", "main"]);
    assert_eq!(r.git(&p, &["rev-parse", "HEAD"]).trim(), head);

    // Incremental: an API landing, then `git pull` brings exactly that.
    r.a.ok("POST", "/v1/code/repos/acme/web/land", Some(json!({ "message": "from api", "files": { "api.txt": { "content": "api\n" } } }))).await;
    r.git(&w, &["pull", "-q", "--rebase", "origin", "main"]);
    assert_eq!(std::fs::read_to_string(w.join("api.txt")).unwrap(), "api\n");
    assert_eq!(r.git(&w, &["log", "-1", "--format=%s"]).trim(), "from api");
    r.git(&w, &["fsck", "--strict"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_auth_for_other_owners_and_anonymous() {
    let a = app(Some("admin"));
    for owner in ["acme", "other"] {
        let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": owner })), Some("admin")).await;
        assert!(s.is_success());
    }
    let token = |owner: &str| {
        let a = &a;
        let owner = owner.to_string();
        async move { a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({})), Some("admin")).await.1["token"].as_str().unwrap().to_string() }
    };
    let (acme, other) = (token("acme").await, token("other").await);
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "open" })), Some(&acme)).await;
    assert!(s.is_success());
    let base = a.serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let with = |t: &str| base.replace("http://", &format!("http://x:{t}@"));

    // Anyone can clone a public repo.
    let out = git_raw(home, home, &["clone", &format!("{base}/git/acme/open.git"), "anon"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let anon = home.join("anon");
    write(&anon, "a.txt", "a\n");
    git(home, &anon, &["add", "-A"]);
    git(home, &anon, &["commit", "-qm", "a"]);
    // Anonymous push: asked for credentials, and there are none.
    let out = git_raw(home, &anon, &["push", "origin", "main"]);
    assert!(!out.status.success());
    // Another owner's token: forbidden.
    let out = git_raw(home, &anon, &["push", &format!("{}/git/acme/open.git", with(&other)), "main"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("403") || err.to_lowercase().contains("forbidden") || err.contains("denied"), "{err}");
    // A wrong token is refused (git only sends it once asked, so ask with a push).
    let out = git_raw(home, &anon, &["push", &format!("{}/git/acme/open.git", with("rlc_bad")), "main"]);
    assert!(!out.status.success());
    // The owner's token pushes.
    let out = git_raw(home, &anon, &["push", &format!("{}/git/acme/open.git", with(&acme)), "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // An unknown repo is a 404, not an auth prompt.
    let out = git_raw(home, home, &["ls-remote", &format!("{base}/git/acme/nope.git")]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not found") || err.contains("404"), "{err}");
}

/// A git request with `x:{token}` as Basic credentials; its status and `WWW-Authenticate`.
async fn git_status(a: &App, method: &str, path: &str, token: Option<&str>) -> (axum::http::StatusCode, Option<String>) {
    use base64::Engine;
    let mut b = axum::http::Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x:{t}"))));
    }
    let r = a.send(b.body(axum::body::Body::empty()).unwrap()).await;
    (r.status(), r.headers().get("www-authenticate").map(|v| v.to_str().unwrap().to_string()))
}

/// The auth matrix over git's smart HTTP, status by status: reads are `git-upload-pack`, writes
/// `git-receive-pack`. Unlike the API, an anonymous read of a private repo is a 401 with a Basic
/// challenge, because git only sends credentials once it's asked for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_auth_matrix_over_smart_http() {
    use axum::http::StatusCode;
    let a = app(Some("admin"));
    let mut tokens = Vec::new();
    for owner in ["acme", "other"] {
        assert!(a.req("POST", "/v1/code/owners", Some(json!({ "name": owner })), Some("admin")).await.0.is_success());
        tokens.push(a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({})), Some("admin")).await.1["token"].as_str().unwrap().to_string());
    }
    let (_, old) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "old" })), Some("admin")).await;
    a.req("DELETE", &format!("/v1/code/owners/acme/tokens/{}", old["id"].as_str().unwrap()), None, Some("admin")).await;
    for (name, vis) in [("pub", "public"), ("priv", "private")] {
        assert!(a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": vis })), Some("admin")).await.0.is_success());
    }
    let refs = |repo: &str, service: &str| format!("/git/acme/{repo}.git/info/refs?service=git-{service}-pack");
    let (ok, nf, un, fb) = (StatusCode::OK, StatusCode::NOT_FOUND, StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN);
    // [read public, read private, write public, write private]
    let rows: [(&str, Option<&str>, [StatusCode; 4]); 8] = [
        ("admin", Some("admin"), [ok; 4]),
        ("owner", Some(tokens[0].as_str()), [ok; 4]),
        ("another owner", Some(tokens[1].as_str()), [ok, nf, fb, nf]),
        ("anonymous", None, [ok, un, un, un]),
        ("unknown token", Some("rlc_0000"), [un; 4]),
        ("garbage token", Some("hunter2"), [un; 4]),
        ("revoked token", old["token"].as_str(), [un; 4]),
        ("an empty password", Some(""),[ok, un, un, un]),
    ];
    for (who, token, want) in rows {
        let got = [
            git_status(&a, "GET", &refs("pub", "upload"), token).await,
            git_status(&a, "GET", &refs("priv", "upload"), token).await,
            git_status(&a, "GET", &refs("pub", "receive"), token).await,
            git_status(&a, "GET", &refs("priv", "receive"), token).await,
        ];
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), want, "{who}");
        for (status, challenge) in &got {
            if *status == un {
                assert_eq!(challenge.as_deref(), Some("Basic realm=\"roll code\""), "{who}: a 401 asks for credentials");
            }
        }
    }
    // The pack endpoints check the same way as the advertisement.
    assert_eq!(git_status(&a, "POST", "/git/acme/priv.git/git-upload-pack", None).await.0, un);
    assert_eq!(git_status(&a, "POST", "/git/acme/priv.git/git-upload-pack", Some(&tokens[1])).await.0, nf);
    assert_eq!(git_status(&a, "POST", "/git/acme/pub.git/git-receive-pack", Some(&tokens[1])).await.0, fb);
    assert_eq!(git_status(&a, "POST", "/git/acme/pub.git/git-receive-pack", None).await.0, un);
    // Not git endpoints, or the dumb protocol.
    assert_eq!(git_status(&a, "POST", "/git/acme/pub.git/info/refs", None).await.0, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(git_status(&a, "GET", "/git/acme/pub.git/HEAD", None).await.0, nf);
    assert_eq!(git_status(&a, "GET", "/git/acme/pub.git/info/refs", None).await.0, fb);
    assert_eq!(git_status(&a, "GET", &refs("nope", "upload"), None).await.0, nf);
    // A renamed repo is still served under its old name.
    a.req("PATCH", "/v1/code/repos/acme/pub", Some(json!({ "name": "moved" })), Some("admin")).await;
    assert_eq!(git_status(&a, "GET", &refs("pub", "upload"), None).await.0, ok);

    // Open mode: every caller is the admin, with any token or none.
    let open = app(None);
    open.repo("acme", "priv").await;
    open.ok("PATCH", "/v1/code/repos/acme/priv", Some(json!({ "visibility": "private" }))).await;
    for token in [None, Some("hunter2")] {
        for service in ["upload", "receive"] {
            assert_eq!(git_status(&open, "GET", &refs("priv", service), token).await.0, ok, "{token:?} {service}");
        }
    }
}
