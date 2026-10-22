//! `roll code …`, the real binary against a real roll code API on a free port: every
//! subcommand, its exit codes (1 unavailable/error, 2 conflict, 3 no work), `--json`, files from
//! disk and stdin, `watch` following landings, and `delete` refusing to run unattended
//! without `--yes`.

use code_control::{Code, Config};
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

struct Env {
    api: String,
    work: PathBuf,
    _dir: tempfile::TempDir,
    _code: Arc<Code>,
}

async fn env(admin_token: Option<&str>) -> Env {
    env_with(|c| c.admin_token = admin_token.map(str::to_string)).await
}

async fn env_with(f: impl FnOnce(&mut Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let objects = Objects::new(&StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let mut config = Config::local(dir.path().join("home"));
    f(&mut config);
    let code = Code::open(config, objects).unwrap();
    code.start_reviewer();
    let router = code_control::api::router(code.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    Env { api, work, _dir: dir, _code: code }
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
        c.args(args)
            .current_dir(&self.work)
            .env("ROLL_API_URL", &self.api)
            .env("NO_COLOR", "1")
            .env_remove("ROLL_CODE_TOKEN")
            .env_remove("ROLL_AGENT")
            .env_remove("ROLL_CODE_AUTHOR")
            .env_remove("ROLL_API_KEY");
        c
    }

    /// Run `roll` off the async runtime; stdin is empty unless given.
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut c = self.cmd(args);
        let input = stdin.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = c.spawn().expect("run roll");
            let mut sin = child.stdin.take().unwrap();
            if let Some(i) = input {
                sin.write_all(i.as_bytes()).unwrap();
            }
            drop(sin);
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    async fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args, None).await;
        assert!(out.status.success(), "roll {args:?} failed ({:?}):\n{}\n{}", out.status.code(), stdout(&out), stderr(&out));
        stdout(&out)
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.ok(&all).await;
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("roll {args:?}: not JSON ({e}): {out}"))
    }

    /// Run `roll`, expect exit `code`; its stderr.
    async fn fails(&self, args: &[&str], code: i32) -> String {
        let out = self.run(args, None).await;
        assert_eq!(out.status.code(), Some(code), "roll {args:?}:\n{}\n{}", stdout(&out), stderr(&out));
        stderr(&out)
    }

    fn write(&self, path: &str, content: &str) {
        let p = self.work.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owners_repos_and_reading() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org", "--display-name", "Acme Inc"]).await;
    let owners = e.json(&["code", "owners"]).await;
    assert_eq!(owners[0]["name"], "acme");
    assert_eq!(owners[0]["kind"], "org");
    assert!(e.ok(&["code", "owners"]).await.contains("acme"));

    // Name checks: 0 when free, 1 when taken/reserved/invalid, with the reason.
    let free = e.run(&["code", "name", "fresh-name"], None).await;
    assert!(free.status.success(), "{}", stderr(&free));
    for (name, reason) in [("acme", "taken"), ("dashboard", "reserved"), ("x_y", "invalid")] {
        let out = e.run(&["code", "name", name], None).await;
        assert_eq!(out.status.code(), Some(1), "{name}");
        assert!(format!("{}{}", stdout(&out), stderr(&out)).contains(reason), "{name}: {} {}", stdout(&out), stderr(&out));
    }

    let r = e.json(&["code", "create", "acme/web", "-d", "The site"]).await;
    assert_eq!(r["full_name"], "acme/web");
    assert_eq!(r["description"], "The site");
    let r = e.json(&["code", "create", "acme/secret", "--private", "--default-branch", "trunk"]).await;
    assert_eq!(r["visibility"], "private");
    assert_eq!(r["default_branch"], "trunk");
    let out = e.run(&["code", "create", "acme/web"], None).await;
    assert_eq!(out.status.code(), Some(1), "taken");
    let out = e.run(&["code", "create", "not-a-full-name"], None).await;
    assert!(!out.status.success());

    let repos = e.json(&["code", "repos", "--owner", "acme"]).await;
    assert_eq!(repos.as_array().unwrap().len(), 2);
    assert!(e.ok(&["code", "repos"]).await.contains("acme/web"));
    assert_eq!(e.ok(&["code", "clone-url", "acme/web.git"]).await.trim(), "http://127.0.0.1:7070/git/acme/web.git");
    assert!(e.ok(&["code", "repo", "acme/web"]).await.contains("acme/web"));

    // Land from local files (path and local=repo/path), then read it back every way.
    e.write("README.md", "# web\n");
    e.write("local/app.ts", "export const app = 1;\n");
    std::fs::write(e.work.join("run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(e.work.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let landed = e
        .json(&["code", "land", "acme/web", "--base", "main", "-m", "init", "--author", "Ada <ada@x.dev>", "README.md", "local/app.ts=src/app.ts", "run.sh"])
        .await;
    assert_eq!(landed["status"], "landed");
    let cs = landed["changeset"].as_str().unwrap().to_string();

    assert_eq!(e.ok(&["code", "cat", "acme/web", "src/app.ts"]).await, "export const app = 1;\n");
    assert_eq!(e.ok(&["code", "cat", &format!("acme/web@{cs}"), "README.md"]).await, "# web\n");
    let ls = e.ok(&["code", "ls", "acme/web"]).await;
    assert!(ls.contains("src") && ls.contains("README.md") && ls.contains("run.sh"), "{ls}");
    let tree = e.json(&["code", "ls", "acme/web@main", "src"]).await;
    assert_eq!(tree["entries"][0]["path"], "src/app.ts");
    let modes = e.json(&["code", "ls", "acme/web"]).await;
    let run = modes["entries"].as_array().unwrap().iter().find(|x| x["name"] == "run.sh").unwrap();
    assert_eq!(run["mode"], "100755", "the exec bit travels");
    let log = e.ok(&["code", "log", "acme/web", "-n", "5"]).await;
    assert!(log.contains("init"), "{log}");
    let log = e.json(&["code", "log", "acme/web", "--path", "src/app.ts"]).await;
    assert_eq!(log["items"].as_array().unwrap().len(), 1);
    assert_eq!(log["items"][0]["author"]["name"], "Ada");
    let show = e.ok(&["code", "show", "acme/web", &cs]).await;
    assert!(show.contains("+export const app = 1;"), "{show}");
    let out = e.run(&["code", "cat", "acme/web", "missing.txt"], None).await;
    assert_eq!(out.status.code(), Some(1));
    let out = e.run(&["code", "repo", "acme/nope"], None).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));

    // Rename keeps working under the new name; delete refuses without --yes when unattended.
    e.ok(&["code", "rename", "acme/web", "site"]).await;
    assert!(e.ok(&["code", "repo", "acme/site"]).await.contains("acme/site"));
    let out = e.run(&["code", "delete", "acme/secret"], None).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert_eq!(e.json(&["code", "repos"]).await.as_array().unwrap().len(), 2, "nothing deleted");
    e.ok(&["code", "delete", "acme/secret", "--yes"]).await;
    assert_eq!(e.json(&["code", "repos"]).await.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn landing_changes_checks_and_conflicts() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    let notes = (0..10).map(|i| format!("line {i}\n")).collect::<String>();
    let body = json!({ "message": "init", "files": { "notes.txt": { "content": notes } } }).to_string();
    let base = e.run(&["--json", "code", "land", "acme/api", "--base", "main", "--stdin-json"], Some(&body)).await;
    assert!(base.status.success(), "{}", stderr(&base));
    let base: Value = serde_json::from_str(&stdout(&base)).unwrap();
    let base = base["changeset"].as_str().unwrap().to_string();

    // A bare files map on stdin, plus a deletion and a binary file from disk.
    std::fs::write(e.work.join("logo.bin"), [0u8, 159, 146, 150, 255]).unwrap();
    let files = json!({ "a.txt": { "content": "a\n" } }).to_string();
    let out = e.run(&["code", "land", "acme/api", "--base", &base, "-m", "more", "--stdin-json", "logo.bin"], Some(&files)).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(e.ok(&["code", "cat", "acme/api", "a.txt"]).await, "a\n");
    let bin = e.cmd(&["code", "cat", "acme/api", "logo.bin"]).output().unwrap();
    assert_eq!(bin.stdout, [0u8, 159, 146, 150, 255], "binary files round-trip");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "rm a", "--delete", "a.txt"]).await;
    assert_eq!(e.run(&["code", "cat", "acme/api", "a.txt"], None).await.status.code(), Some(1));

    // Same line from a stale base: exit 2 with the path.
    e.write("notes.txt", &notes.replace("line 1\n", "line one\n"));
    e.ok(&["code", "land", "acme/api", "--base", &base, "-m", "one", "notes.txt"]).await;
    e.write("notes.txt", &notes.replace("line 1\n", "line uno\n"));
    let out = e.run(&["code", "land", "acme/api", "--base", &base, "-m", "uno", "notes.txt"], None).await;
    assert_eq!(out.status.code(), Some(2), "{} {}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("notes.txt"), "{}", stderr(&out));
    let out = e.run(&["--json", "code", "land", "acme/api", "--base", &base, "-m", "uno", "notes.txt"], None).await;
    assert_eq!(out.status.code(), Some(2));
    // Usage errors.
    let out = e.run(&["code", "land", "acme/api", "--base", "main", "notes.txt"], None).await;
    assert!(!out.status.success() && stderr(&out).contains("message"), "{}", stderr(&out));
    let out = e.run(&["code", "land", "acme/api", "--base", "main", "-m", "x", "../outside"], None).await;
    assert!(!out.status.success());

    // Changes: create from files, list, show by number, check, revise, land; close another.
    e.write("feature.txt", "f1\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "Add feature", "--body", "Why", "--base", "main", "-m", "feature", "feature.txt"]).await;
    let id = c["id"].as_str().unwrap().to_string();
    let num = format!("acme/api#{}", c["number"]);
    assert_eq!(c["title"], "Add feature");
    assert_eq!(c["author"], "roll");
    assert!(e.ok(&["code", "change", "list", "acme/api"]).await.contains("Add feature"));
    assert_eq!(e.json(&["code", "change", "list", "acme/api", "--state", "open"]).await.as_array().unwrap().len(), 1);
    assert_eq!(e.json(&["code", "change", "show", &num]).await["id"], id);
    let c = e.json(&["code", "check", &id, "test", "success", "--url", "https://ci/1", "--summary", "ok"]).await;
    assert_eq!(c["checks"][0]["status"], "success");
    assert!(!e.run(&["code", "check", &id, "test", "great"], None).await.status.success(), "statuses are validated");
    e.write("feature.txt", "f2\n");
    let c = e.json(&["code", "change", "revise", &id, "--base", "main", "-m", "feature v2", "feature.txt"]).await;
    assert_eq!(c["revision"], 2);
    let l = e.json(&["code", "change", "land", &num]).await;
    assert_eq!(l["status"], "landed");
    assert_eq!(e.ok(&["code", "cat", "acme/api", "feature.txt"]).await, "f2\n");
    let other = e.json(&["code", "change", "create", "acme/api", "-t", "Nope", "--changeset", "main"]).await;
    let closed = e.json(&["code", "change", "close", other["id"].as_str().unwrap()]).await;
    assert_eq!(closed["state"], "closed");
    // A conflicting change exits 2 on land.
    e.write("notes.txt", &notes.replace("line 1\n", "line ichi\n"));
    let stale = e.json(&["code", "change", "create", "acme/api", "-t", "Stale", "--base", &base, "-m", "stale", "notes.txt"]).await;
    let out = e.run(&["code", "change", "land", stale["id"].as_str().unwrap()], None).await;
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_queue_tokens_and_watch() {
    let e = env(Some("admin-secret")).await;
    let admin = |args: &[&str]| {
        let mut v = vec!["--token", "admin-secret"];
        v.extend_from_slice(args);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let run = |args: Vec<String>| {
        let e = &e;
        async move { e.ok(&args.iter().map(String::as_str).collect::<Vec<_>>()).await }
    };
    run(admin(&["code", "owners", "create", "acme", "--org"])).await;
    let token = run(admin(&["code", "token", "create", "acme", "agent"])).await.trim().to_string();
    assert!(token.starts_with("rlc_"), "{token}");
    // Without a token, writes are refused; ROLL_CODE_TOKEN or --token is the owner's.
    let out = e.run(&["code", "create", "acme/q"], None).await;
    assert_eq!(out.status.code(), Some(1));
    let mut c = e.cmd(&["code", "create", "acme/q"]);
    assert!(c.env("ROLL_CODE_TOKEN", &token).output().unwrap().status.success());
    let t = |args: &[&str]| {
        let mut v = vec!["--token", token.as_str()];
        v.extend_from_slice(args);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };

    run(t(&["code", "work", "create", "acme/q", "Fix login", "--label", "auth", "--label", "bug", "-p", "1", "--body", "Steps"])).await;
    run(t(&["code", "work", "create", "acme/q", "Outage", "-p", "0"])).await;
    let list = run(t(&["--json", "code", "work", "list", "acme/q"])).await;
    assert_eq!(serde_json::from_str::<Value>(&list).unwrap().as_array().unwrap().len(), 2);
    let auth = run(t(&["--json", "code", "work", "list", "acme/q", "--label", "auth"])).await;
    assert_eq!(serde_json::from_str::<Value>(&auth).unwrap().as_array().unwrap().len(), 1);

    // claim prints only the id; most urgent first; exit 3 when there's nothing left.
    let id = run(t(&["code", "work", "claim", "acme/q", "--agent", "a1", "--lease", "60"])).await.trim().to_string();
    assert!(id.starts_with("wi_"), "{id}");
    let shown: Value = serde_json::from_str(&run(t(&["--json", "code", "work", "show", &id])).await).unwrap();
    assert_eq!(shown["title"], "Outage");
    assert_eq!(shown["claimed_by"], "a1");
    let id2 = run(t(&["code", "work", "claim", "acme/q", "--agent", "a2", "--label", "auth"])).await.trim().to_string();
    let none = e.run(&t(&["code", "work", "claim", "acme/q", "--agent", "a3"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await;
    assert_eq!(none.status.code(), Some(3));
    assert!(stdout(&none).is_empty());
    run(t(&["code", "work", "heartbeat", &id, "--agent", "a1"])).await;
    let other = e.run(&t(&["code", "work", "heartbeat", &id, "--agent", "a9"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await;
    assert_eq!(other.status.code(), Some(1), "someone else's lease");
    let r: Value = serde_json::from_str(&run(t(&["--json", "code", "work", "release", &id, "--agent", "a1"])).await).unwrap();
    assert_eq!(r["state"], "open");
    let d: Value = serde_json::from_str(&run(t(&["--json", "code", "work", "complete", &id2, "--agent", "a2"])).await).unwrap();
    assert_eq!(d["state"], "done");
    assert!(run(t(&["code", "work", "show", &id2])).await.contains("done"));

    // watch: one line per landing, as it happens.
    let mut child = e.cmd(&["--token", &token, "code", "watch", "acme/q"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    e.write("w.txt", "w\n");
    run(t(&["code", "land", "acme/q", "--base", "main", "-m", "watched landing", "w.txt"])).await;
    let line = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a watch line");
    let line = if line.contains("watched landing") { line } else { rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap() };
    assert!(line.contains("✓") && line.contains("watched landing") && line.contains("cs_"), "{line}");
    child.kill().unwrap();
    let _ = child.wait();

    // watch --json: one JSON object per event.
    let mut child = e.cmd(&["--token", &token, "--json", "code", "watch", "acme/q"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    run(t(&["code", "work", "create", "acme/q", "Watched item"])).await;
    let line = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a json event");
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line}"));
    assert!(v.to_string().contains("Watched item"), "{v}");
    child.kill().unwrap();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_queue_commands() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    e.write("a.txt", "a\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "a.txt"]).await;
    let out = e.run(&["code", "queue"], None).await;
    assert!(!out.status.success(), "a repo is required");
    // Switch the repo to the queue.
    let r = e._code.find_repo("acme", "api").unwrap();
    e._code.update_repo(&code_control::Caller::Admin, "acme", "api", &json!({ "land_policy": "queue" })).await.unwrap();
    assert_eq!(e._code.find_repo("acme", "api").unwrap().land_policy, code_control::model::LandPolicy::Queue);

    // `land` on a queue repo queues (exit 0) and says how to watch.
    e.write("b.txt", "b\n");
    let out = e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "add b", "b.txt"]).await;
    assert!(out.contains("queued acme/api #1 at position 1"), "{out}");
    assert!(out.contains("roll code queue acme/api --watch"), "{out}");
    let q = e.json(&["code", "queue", "acme/api"]).await;
    assert_eq!(q["stats"]["depth"], 1);
    let text = e.ok(&["code", "queue", "acme/api"]).await;
    assert!(text.contains("1 queued") && text.contains("#1") && text.contains("add b"), "{text}");

    // A change: draft blocks it, ready lets it in; urgent goes first; dequeue takes it out.
    e.write("c.txt", "c\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "add c", "--base", "main", "c.txt"]).await;
    e.ok(&["code", "change", "draft", "acme/api#2"]).await;
    let out = e.run(&["code", "change", "enqueue", "acme/api#2"], None).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("draft"), "{}", stderr(&out));
    assert!(e.ok(&["code", "change", "ready", "acme/api#2"]).await.contains("ready"));
    let v = e.json(&["code", "change", "enqueue", "acme/api#2", "--urgent"]).await;
    assert_eq!(v["position"], 1);
    assert_eq!(v["change"]["queue"]["lane"], "urgent");
    let out = e.ok(&["code", "change", "dequeue", c["id"].as_str().unwrap()]).await;
    assert!(out.contains("dequeued acme/api #2"), "{out}");
    let out = e.ok(&["code", "change", "land", "acme/api#2"]).await;
    assert!(out.contains("queued acme/api #2"), "{out}");
    // An admin can land around the queue.
    e.write("d.txt", "d\n");
    e.ok(&["code", "change", "create", "acme/api", "-t", "hotfix", "--base", "main", "d.txt"]).await;
    let out = e.ok(&["code", "change", "land", "acme/api#3", "--bypass"]).await;
    assert!(out.contains("✓ landed"), "{out}");

    // Pause and resume print the queue.
    assert!(e.ok(&["code", "queue", "pause", "acme/api"]).await.contains("paused"));
    let v = e.json(&["code", "queue", "resume", "acme/api"]).await;
    assert_eq!(v["paused"], false);

    // Start the queue: everything lands.
    e._code.start_queues().unwrap();
    for _ in 0..500 {
        if e.json(&["code", "queue", "acme/api"]).await["stats"]["depth"] == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let q = e.json(&["code", "queue", "acme/api"]).await;
    assert_eq!(q["stats"]["depth"], 0, "{q}");
    let text = e.ok(&["code", "queue", "acme/api"]).await;
    assert!(text.contains("recently:") && text.contains("landed"), "{text}");
    let log = e.ok(&["code", "log", "acme/api", "-n", "10"]).await;
    for subject in ["add b", "add c", "hotfix"] {
        assert!(log.contains(subject), "{log}");
    }
    // Approving a run (here: a trusted change, which is a no-op but allowed).
    let out = e.ok(&["code", "change", "approve", "acme/api#3"]).await;
    assert!(out.contains("approved running acme/api #3"), "{out}");
    let _ = r;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviews_threads_and_review_apps() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    e.write("notes.txt", "1\n2\n3\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "notes.txt"]).await;

    // Rules: one approval.
    let s = e.json(&["code", "review-settings", "acme/api", "--set", "required_approvals=1", "--set", "auto_approve=[{\"paths\":[\"docs/**\"]}]"]).await;
    assert_eq!(s["effective"]["required_approvals"], 1);
    assert!(e.ok(&["code", "review-settings", "acme/api"]).await.contains("required approvals     1"));
    let out = e.run(&["code", "review-settings", "acme/api", "--set", "required_approvals=99"], None).await;
    assert_eq!(out.status.code(), Some(1));

    e.write("notes.txt", "1\ntwo\n3\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "Two", "--base", "main", "-m", "two", "notes.txt"]).await;
    let id = c["id"].as_str().unwrap().to_string();
    let status = e.ok(&["code", "review", &id]).await;
    assert!(status.contains("review pending") && status.contains("0/1 approvals") && status.contains("blocked: needs 1 approval"), "{status}");
    // Landing says why it can't.
    let out = e.run(&["code", "change", "land", &id], None).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("needs 1 approval"), "{}", stderr(&out));

    // A comment review with inline comments (as an agent), then an approval.
    let v = e.json(&["code", "review", &id, "comment", "-m", "a few notes", "--comment", "notes.txt:2=why two?", "--comment", "notes.txt:1-3=```suggestion\none\ntwo\nthree\n```", "--agent", "linter"]).await;
    assert_eq!(v["reviewer"]["kind"], "agent");
    assert_eq!(v["threads"].as_array().unwrap().len(), 2);
    let out = e.run(&["code", "review", &id, "comment", "--comment", "notes.txt:x=bad"], None).await;
    assert!(!out.status.success());
    let threads = e.json(&["code", "thread", "list", &id]).await;
    assert_eq!(threads.as_array().unwrap().len(), 2);
    let list = e.ok(&["code", "thread", "list", &id, "--open"]).await;
    assert!(list.contains("notes.txt:2") && list.contains("[suggestion]"), "{list}");
    let t0 = threads[0]["id"].as_str().unwrap().to_string();
    let t1 = threads[1]["id"].as_str().unwrap().to_string();
    let r = e.json(&["code", "thread", "reply", &t0, "-m", "because", "--agent", "author-bot"]).await;
    assert_eq!(r["comments"].as_array().unwrap().len(), 2);
    let t = e.json(&["code", "thread", "open", &id, "--path", "notes.txt", "--line", "3", "-m", "and three"]).await;
    assert_eq!(t["line"], 3);
    e.ok(&["code", "thread", "resolve", &t0]).await;
    e.ok(&["code", "thread", "unresolve", &t0]).await;
    let applied = e.json(&["code", "thread", "apply", &t1]).await;
    assert_eq!(applied["change"]["revision"], 2);
    assert_eq!(e.ok(&["code", "cat", &format!("acme/api@{}", applied["change"]["head"].as_str().unwrap()), "notes.txt"]).await, "one\ntwo\nthree\n");
    let ok = e.ok(&["code", "review", &id, "approve", "-m", "LGTM"]).await;
    assert!(ok.contains("approve by @admin"), "{ok}");
    let st = e.json(&["code", "review", &id]).await;
    assert_eq!(st["status"]["state"], "approved");
    assert_eq!(st["reviews"].as_array().unwrap().len(), 2);

    // Request a reviewer; dismiss a review.
    let req = e.json(&["code", "review", &id, "request", "roll", "--required"]).await;
    assert_eq!(req[0]["reviewer"]["kind"], "roll");
    assert!(e.ok(&["code", "review", &id]).await.contains("waiting for roll"));
    let rc = e.json(&["code", "review", &id, "request-changes", "-m", "hold on"]).await;
    let out = e.ok(&["code", "review", &id, "dismiss", rc["id"].as_str().unwrap(), "-m", "never mind"]).await;
    assert!(out.contains("dismissed"), "{out}");
    let out = e.run(&["code", "review", &id, "merge"], None).await;
    assert!(!out.status.success() && stderr(&out).contains("unknown review action"));
    let out = e.run(&["code", "review", &id, "dismiss"], None).await;
    assert!(!out.status.success());

    // Review apps.
    let app = e.json(&["code", "review-apps", "create", "acme", "greptile", "--url", "http://127.0.0.1:9/hook", "--event", "change.opened", "--repo", "api", "--required"]).await;
    let app_id = app["app"]["id"].as_str().unwrap().to_string();
    assert!(app["token"].as_str().unwrap().starts_with("rla_"));
    assert!(e.ok(&["code", "review-apps", "list", "acme"]).await.contains("greptile"));
    assert!(e.ok(&["code", "review-apps", "show", &app_id]).await.contains("required"));
    let v = e.json(&["code", "review-apps", "update", &app_id, "--required", "false", "--event", "thread.comment"]).await;
    assert_eq!(v["required"], false);
    let rotated = e.ok(&["code", "review-apps", "rotate", &app_id]).await;
    assert!(rotated.contains("token  rla_") && rotated.contains("secret rls_"), "{rotated}");
    let deliveries = e.json(&["code", "review-apps", "deliveries", &app_id]).await;
    assert!(deliveries.is_array());
    let out = e.run(&["code", "review-apps", "redeliver", &app_id, "dl_nope"], None).await;
    assert_eq!(out.status.code(), Some(1));
    e.ok(&["code", "review-apps", "delete", &app_id]).await;
    assert!(!e.ok(&["code", "review-apps", "list", "acme"]).await.contains("greptile"));
}

/// A mock Anthropic API: one bug on the first added line of every file it's shown.
async fn anthropic_mock() -> String {
    let app = axum::Router::new().fallback(|uri: axum::http::Uri, body: axum::body::Bytes| async move {
        use axum::response::IntoResponse;
        if uri.path().ends_with("/count_tokens") {
            return axum::Json(json!({ "input_tokens": 10 })).into_response();
        }
        let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let user = b["messages"][0]["content"][0]["text"].as_str().unwrap_or("").to_string();
        let answer = if user.contains("# Thread") {
            json!({ "reply": "Because.", "suggestion": null })
        } else {
            let line: u64 = user.lines().find_map(|l| l.strip_prefix("+ ")).and_then(|r| r.split('|').next()).and_then(|n| n.trim().parse().ok()).unwrap_or(1);
            json!({ "walkthrough": "Edits notes.", "files": [{ "path": "notes.txt", "summary": "edited" }], "diagram": null, "notes": [],
                    "findings": [{ "path": "notes.txt", "side": "new", "start_line": line, "line": line, "severity": "bug", "title": "Odd number",
                                   "body": "Two isn't a word here.", "suggestion": null, "confidence": 0.9 }] })
        };
        axum::Json(json!({ "model": "claude-opus-5-5", "stop_reason": "end_turn", "content": [{ "type": "text", "text": answer.to_string() }],
                           "usage": { "input_tokens": 100, "output_tokens": 20 } }))
        .into_response()
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_reviewer_commands() {
    let ai = anthropic_mock().await;
    let e = env_with(|c| {
        c.review.hosted = Some(code_control::ai::Hosted { provider: "anthropic".into(), api_key: "sk-hosted".into(), base_url: Some(ai.clone()), model: None, secondary_model: None });
        c.review.secret_key = Some("0123456789abcdef0123456789abcdef".into());
        c.review.anthropic_url = ai.clone();
    })
    .await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api", "--private"]).await;
    e.write("notes.txt", "1\n2\n3\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "notes.txt"]).await;

    // Settings.
    let off = e.ok(&["code", "reviewer", "acme/api"]).await;
    assert!(off.contains("enabled      false") && off.contains("anthropic · claude-opus-5-5 (hosted)"), "{off}");
    let v = e.json(&["code", "reviewer", "acme/api", "--enable", "--mode", "standard", "--speed", "balanced", "--set", "required=true"]).await;
    assert_eq!(v["effective"]["enabled"], true);
    assert_eq!(v["effective"]["required"], true);
    let out = e.run(&["code", "reviewer", "acme/api", "--mode", "huge"], None).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("reviewer.mode"), "{}", stderr(&out));

    // A change is reviewed on its own; --ai asks again.
    e.write("notes.txt", "1\ntwo\n3\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "Two", "--base", "main", "-m", "two", "notes.txt"]).await;
    let id = c["id"].as_str().unwrap().to_string();
    let mut reviewed = false;
    for _ in 0..200 {
        let st = e.json(&["code", "review", &id]).await;
        if st["reviews"].as_array().unwrap().iter().any(|r| r["reviewer"]["kind"] == "roll" && !r["ai"].is_null()) {
            reviewed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(reviewed, "roll reviewed the change");
    let q = e.ok(&["code", "review", &id, "--ai", "--mode", "deep", "--full"]).await;
    assert!(q.contains("queued: roll will review in deep mode (full)"), "{q}");
    let out = e.run(&["code", "review", &id, "--mode", "deep"], None).await;
    assert!(!out.status.success(), "--mode needs --ai");

    // Usage and billing.
    let mut u = String::new();
    for _ in 0..200 {
        u = e.ok(&["code", "usage", "acme"]).await;
        if u.contains("reviews      2") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(u.contains("reviews      2") && u.contains("standard") && u.contains("deep") && u.contains("acme/api"), "{u}");
    // jobs: both reviews, done, with their mode and how many file groups they took.
    let jobs = e.json(&["code", "jobs", &id]).await;
    assert_eq!(jobs.as_array().unwrap().len(), 2, "{jobs}");
    assert!(jobs.as_array().unwrap().iter().all(|j| j["stage"] == "done"), "{jobs}");
    let text = e.ok(&["code", "jobs", &id]).await;
    assert!(text.contains("deep") && text.contains("done") && text.contains("1/1 groups"), "{text}");
    let uj = e.json(&["code", "usage", "acme", "--month", "2026-01"]).await;
    assert_eq!(uj["totals"]["reviews"], 0);
    let b = e.ok(&["code", "billing", "acme", "--cap", "25", "--on-cap", "standard_only"]).await;
    assert!(b.contains("cap $25.00/month") && b.contains("then standard_only"), "{b}");
    assert!(e.ok(&["code", "billing", "acme", "--cap", "none"]).await.contains("no cap"));
    assert!(!e.run(&["code", "billing", "acme", "--cap", "lots"], None).await.status.success());

    // Bring your own key.
    let p = e.ok(&["code", "provider", "set", "acme", "--api-key", "sk-ant-mine-4321"]).await;
    assert!(p.contains("anthropic") && p.contains("…4321") && !p.contains("mine"), "{p}");
    assert_eq!(e.json(&["code", "provider", "show", "acme"]).await["key_last4"], "4321");
    assert!(e.ok(&["code", "provider", "clear", "acme"]).await.contains("hosted reviewer again"));
    assert!(e.ok(&["code", "provider", "show", "acme"]).await.contains("hosted"));
    assert_eq!(e.json(&["code", "provider", "show", "acme"]).await["provider"], Value::Null);

    // Learnings.
    assert!(e.ok(&["code", "learnings", "list", "acme/api"]).await.contains("no learnings yet"));
    let l = e.json(&["code", "learnings", "add", "acme/api", "Numbers are spelled out", "--path", "*.txt"]).await;
    let lid = l["id"].as_str().unwrap().to_string();
    assert!(e.ok(&["code", "learnings", "list", "acme/api"]).await.contains("Numbers are spelled out (*.txt)"));
    assert!(e.ok(&["code", "learnings", "rm", "acme/api", &lid]).await.contains("forgot"));

    // Search.
    let s = e.ok(&["code", "search", "acme/api", "2"]).await;
    assert_eq!(s.trim(), "notes.txt:2: 2");
    let sj = e.json(&["code", "search", "acme/api", "^[0-9]$", "--regex", "-n", "2"]).await;
    assert_eq!(sj["matches"].as_array().unwrap().len(), 2);
    assert_eq!(sj["truncated"], true);
    assert!(!e.run(&["code", "search", "acme/api", "(", "--regex"], None).await.status.success());
    // `@rev` searches a revision: the resolved changeset comes back.
    let at = e.json(&["code", "search", "acme/api@main", "2"]).await;
    assert!(at["rev"].as_str().unwrap().starts_with("cs_"), "{at}");
    assert_eq!(at["matches"][0]["path"], "notes.txt");
    assert!(!e.run(&["code", "search", "acme/api@no-such-branch", "2"], None).await.status.success());
}

/// `webhooks`, `status`, `blame`, `jobs` and `token list|revoke`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhooks_statuses_blame_jobs_and_tokens() {
    let e = env_with(|c| {
        c.admin_token = Some("admin-secret".into());
        c.allow_private_webhooks = Some(true);
        c.review_app_retry_ms = 20;
    })
    .await;
    let a = |args: &[&str]| {
        let mut v = vec!["--token", "admin-secret"];
        v.extend_from_slice(args);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let ok = |args: Vec<String>| {
        let e = &e;
        async move { e.ok(&args.iter().map(String::as_str).collect::<Vec<_>>()).await }
    };
    let js = |args: Vec<String>| {
        let e = &e;
        async move {
            let mut all = vec!["--json".to_string()];
            all.extend(args);
            let out = e.ok(&all.iter().map(String::as_str).collect::<Vec<_>>()).await;
            serde_json::from_str::<Value>(&out).unwrap_or_else(|err| panic!("not JSON ({err}): {out}"))
        }
    };
    ok(a(&["code", "owners", "create", "acme", "--org"])).await;
    ok(a(&["code", "create", "acme/w"])).await;
    e.write("f.txt", "one\ntwo\n");
    ok(a(&["code", "land", "acme/w", "--base", "main", "-m", "first", "f.txt", "--author", "Ada <ada@roll.sh>"])).await;
    e.write("f.txt", "one\nTWO\nthree\n");
    let landed = js(a(&["code", "land", "acme/w", "--base", "main", "-m", "second", "f.txt", "--author", "Bo <bo@roll.sh>"])).await;
    let head = landed["changeset"].as_str().unwrap().to_string();

    // A webhook receiver that answers with a short body.
    let got = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let g = got.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let g = g.clone();
        async move {
            g.lock().unwrap().push(headers.get("x-roll-event").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
            "ok from receiver"
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    // webhooks create (generated secret printed once), list, update, ping, deliveries, redeliver, delete.
    let out = e.run(&a(&["code", "webhooks", "create", "acme/w", &url, "--event", "landed", "--event", "check"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.lines().any(|l| l.starts_with("rlw_")), "the secret: {text}");
    assert!(stderr(&out).contains("shown once"));
    let hooks = js(a(&["code", "webhooks", "list", "acme/w"])).await;
    let id = hooks[0]["id"].as_str().unwrap().to_string();
    assert_eq!(hooks[0]["events"], json!(["check", "landed"]));
    assert!(ok(a(&["code", "webhooks", "list", "acme/w"])).await.contains(&url));
    let updated = js(a(&["code", "webhooks", "update", "acme/w", &id, "--pause"])).await;
    assert_eq!(updated["active"], false);
    let updated = js(a(&["code", "webhooks", "update", "acme/w", &id, "--resume", "--event", "*"])).await;
    assert_eq!(updated["active"], true);
    assert_eq!(updated["events"].as_array().unwrap().len(), 9);
    assert_eq!(e.run(&a(&["code", "webhooks", "update", "acme/w", &id]).iter().map(String::as_str).collect::<Vec<_>>(), None).await.status.code(), Some(1), "nothing to change");
    ok(a(&["code", "webhooks", "ping", "acme/w", &id])).await;
    let mut deliveries = Value::Null;
    for _ in 0..200 {
        deliveries = js(a(&["code", "webhooks", "deliveries", "acme/w", &id])).await;
        if deliveries.as_array().unwrap().iter().filter(|d| d["state"] == "delivered").count() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let d = deliveries.as_array().unwrap().iter().find(|d| d["state"] == "delivered").unwrap().clone();
    assert_eq!(d["last_response"], "ok from receiver");
    let human = ok(a(&["code", "webhooks", "deliveries", "acme/w", &id])).await;
    assert!(human.contains("delivered") && human.contains("ok from receiver"), "{human}");
    let again = js(a(&["code", "webhooks", "redeliver", "acme/w", &id, d["id"].as_str().unwrap()])).await;
    assert_eq!(again["id"], d["id"]);
    ok(a(&["code", "webhooks", "delete", "acme/w", &id])).await;
    assert!(js(a(&["code", "webhooks", "list", "acme/w"])).await.as_array().unwrap().is_empty());

    // status: post one, show the combined state; a bad state is refused.
    let posted = js(a(&["code", "status", "acme/w", &head, "success", "--context", "ci/teamcity", "--url", "https://ci.example/1", "--description", "green"])).await;
    assert_eq!(posted["status"]["state"], "success");
    ok(a(&["code", "status", "acme/w", "main", "pending", "--context", "lint"])).await;
    let shown = ok(a(&["code", "status", "acme/w", "main"])).await;
    assert!(shown.contains("pending") && shown.contains("ci/teamcity") && shown.contains("green"), "{shown}");
    assert_eq!(js(a(&["code", "status", "acme/w", "main"])).await["total_count"], 2);
    assert_eq!(e.run(&a(&["code", "status", "acme/w", "main", "great"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await.status.code(), Some(1));

    // blame: aligned, one block per range, lines numbered.
    let b = ok(a(&["code", "blame", "acme/w", "f.txt"])).await;
    let lines: Vec<&str> = b.lines().collect();
    assert_eq!(lines.len(), 3, "{b}");
    assert!(lines[0].contains("Ada") && lines[0].ends_with("1 │ one"), "{b}");
    assert!(lines[1].contains("Bo") && lines[1].ends_with("2 │ TWO"), "{b}");
    assert!(lines[2].ends_with("3 │ three") && !lines[2].contains("Bo"), "a range shows its author once: {b}");
    let width = |l: &str| l.find('│').unwrap();
    assert!(lines.iter().all(|l| width(l) == width(lines[0])), "aligned: {b}");
    let bj = js(a(&["code", "blame", "acme/w@main", "f.txt"])).await;
    assert_eq!(bj["ranges"].as_array().unwrap().len(), 2, "{bj}");
    assert_eq!(e.run(&a(&["code", "blame", "acme/w", "nope.txt"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await.status.code(), Some(1));

    // jobs: none yet.
    e.write("g.txt", "g\n");
    let ch = js(a(&["code", "change", "create", "acme/w", "--title", "t", "--base", "main", "g.txt"])).await;
    let num = format!("acme/w#{}", ch["number"]);
    assert!(ok(a(&["code", "jobs", &num])).await.contains("no AI review jobs"));
    assert_eq!(js(a(&["code", "jobs", &num])).await, json!([]));

    // token list (no secrets) and revoke (stops working at once).
    let token = ok(a(&["code", "token", "create", "acme", "ci"])).await.trim().to_string();
    let listed = js(a(&["code", "token", "list", "acme"])).await;
    let t = listed.as_array().unwrap().iter().find(|t| t["name"] == "ci").unwrap().clone();
    assert!(!listed.to_string().contains(&token));
    let human = ok(a(&["code", "token", "list", "acme"])).await;
    assert!(human.contains("ci") && human.contains("never used") && human.contains(&token[..8]), "{human}");
    let mine = |args: &[&str]| {
        let mut v = vec!["--token", token.as_str()];
        v.extend_from_slice(args);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    ok(mine(&["code", "repo", "acme/w"])).await;
    assert!(ok(a(&["code", "token", "list", "acme"])).await.contains("used "), "last use shows");
    ok(a(&["code", "token", "revoke", "acme", t["id"].as_str().unwrap()])).await;
    let out = e.run(&mine(&["code", "repo", "acme/w"]).iter().map(String::as_str).collect::<Vec<_>>(), None).await;
    assert_eq!(out.status.code(), Some(1), "a revoked token is refused");
    assert!(got.lock().unwrap().iter().any(|ev| ev == "ping"));
}

/// `roll code … | head`: a reader that goes away must end `roll` quietly, not with a panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_pipe_ends_quietly() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    for i in 0..40 {
        e.ok(&["code", "create", &format!("acme/repo-{i}"), "--description", &"x".repeat(200)]).await;
    }
    let mut c = e.cmd(&["code", "repos"]);
    let out = tokio::task::spawn_blocking(move || {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        // Close the read end before roll writes anything.
        drop(child.stdout.take());
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}

/// Owners, repos and `land` flags the other tests don't use (`--user`, `ls-repos`, `--github`,
/// `--ref`, `show --json`), every `land` usage error, `token` errors, usage errors exiting 1
/// (2 means a conflict), and an API that isn't running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_land_and_usage_edges() {
    let e = env(None).await;
    let o = e.json(&["code", "owners", "create", "ada", "--user", "--display-name", "Ada L"]).await;
    assert_eq!(o["kind"], "user");
    assert!(e.ok(&["code", "owners"]).await.contains("Ada L"));
    assert!(e.ok(&["code", "owners", "create", "acme", "--org"]).await.contains("claimed acme (org)"));
    e.fails(&["code", "owners", "create", "acme"], 1).await;
    e.fails(&["code", "owners", "create", "x_y"], 1).await;
    let made = e.ok(&["code", "create", "acme/api", "-d", "The API"]).await;
    assert!(made.contains("acme/api") && made.contains("main (empty)") && made.contains("The API"), "{made}");
    assert!(e.ok(&["code", "ls-repos", "--owner", "acme"]).await.contains("acme/api"));
    assert!(e.json(&["code", "repos", "--owner", "ada"]).await.as_array().unwrap().is_empty());
    let repo = e.ok(&["code", "repo", "acme/api"]).await;
    assert!(repo.contains("git clone") && repo.contains("landing  post-merge checks") && repo.contains("0 changesets"), "{repo}");
    e.fails(&["code", "repo", "acme"], 1).await;

    // --github takes owner/repo and needs a GitHub App that can see it.
    let err = e.fails(&["code", "create", "acme/gh", "--github", "noslash", "--installation", "1"], 1).await;
    assert!(err.contains("expected owner/repo"), "{err}");
    e.fails(&["code", "create", "acme/gh", "--github", "octo/gh", "--installation", "1"], 1).await;
    assert_eq!(e.json(&["code", "repos", "--owner", "acme"]).await.as_array().unwrap().len(), 1, "nothing created");

    // --ref lands on a branch; the default branch doesn't change.
    e.write("a.txt", "a\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "a.txt"]).await;
    e.write("b.txt", "b\n");
    let out = e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "on a branch", "--ref", "feature/x", "b.txt"]).await;
    assert!(out.contains("✓ landed cs_") && out.contains("on feature/x"), "{out}");
    assert_eq!(e.ok(&["code", "cat", "acme/api@feature/x", "b.txt"]).await, "b\n");
    e.fails(&["code", "cat", "acme/api", "b.txt"], 1).await;
    assert!(e.ok(&["code", "log", "acme/api@feature/x", "-n", "1"]).await.contains("on a branch"));
    assert!(!e.ok(&["code", "log", "acme/api"]).await.contains("on a branch"));
    assert!(e.ok(&["code", "ls", "acme/api@feature/x"]).await.contains("b.txt"));
    let log = e.json(&["code", "log", "acme/api", "-n", "1"]).await;
    let cs = log["items"][0]["id"].as_str().unwrap().to_string();
    let show = e.json(&["code", "show", "acme/api", &cs]).await;
    assert_eq!((show["id"].as_str(), show["files"][0]["path"].as_str()), (Some(cs.as_str()), Some("a.txt")));
    e.fails(&["code", "show", "acme/api", "cs_00000000"], 1).await;

    // land usage errors, each with what's wrong.
    for (args, want) in [
        (vec!["-m", "x", "a.txt"], "--base is required"),
        (vec!["--base", "main", "-m", "x"], "nothing to write"),
        (vec!["--base", "main", "-m", "x", "missing.txt"], "can't read"),
        (vec!["--base", "main", "-m", "x", "--author", "nobody", "a.txt"], "Name <email>"),
        (vec!["--base", "main", "-m", "x", "a.txt", "--delete", "a.txt"], "given twice"),
        (vec!["--base", "main", "-m", "x", "/etc/hostname"], "relative"),
    ] {
        let mut all = vec!["code", "land", "acme/api"];
        all.extend(args);
        let err = e.fails(&all, 1).await;
        assert!(err.contains(want), "{all:?}: {err}");
    }
    for (stdin, want) in [("nope", "stdin is not JSON"), ("[1]", "must be an object"), (r#"{"files": [1]}"#, "`files` must be an object")] {
        let out = e.run(&["code", "land", "acme/api", "--base", "main", "-m", "x", "--stdin-json"], Some(stdin)).await;
        assert_eq!(out.status.code(), Some(1), "{stdin}");
        assert!(stderr(&out).contains(want), "{stdin}: {}", stderr(&out));
    }
    // An unknown ref to build on is the API's error.
    e.fails(&["code", "land", "acme/api", "--base", "no-such-branch", "-m", "x", "a.txt"], 1).await;

    // Usage errors exit 1, since agents read 2 as a conflict and 3 as no work; help exits 0.
    let err = e.fails(&["code", "land", "acme/api", "--bogus"], 1).await;
    assert!(err.contains("--bogus"), "{err}");
    e.fails(&["code", "change", "frobnicate"], 1).await;
    e.fails(&["code", "work", "claim", "acme/api"], 1).await;
    e.fails(&["--token", "t", "--json", "code", "change", "land"], 1).await;
    e.fails(&["code"], 1).await;
    let help = e.run(&["code", "--help"], None).await;
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("land"));

    // Tokens: an unknown owner, an unknown token.
    e.fails(&["code", "token", "create", "nobody", "t"], 1).await;
    e.fails(&["code", "token", "revoke", "acme", "tok_nope"], 1).await;
    assert!(e.json(&["code", "token", "list", "acme"]).await.as_array().unwrap().is_empty());

    // rename onto a taken name, delete what isn't there.
    e.ok(&["code", "create", "acme/web"]).await;
    e.fails(&["code", "rename", "acme/web", "api"], 1).await;
    let err = e.fails(&["code", "delete", "acme/nope", "--yes"], 1).await;
    assert!(err.contains("not found"), "{err}");

    // No API at the address: exit 1 and a hint.
    let out = e.cmd(&["code", "repos"]).env("ROLL_API_URL", "http://127.0.0.1:1").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("is rolld running?"), "{}", stderr(&out));
}

/// `change` and `work` flags the other tests don't use: `--work` (id and number), `revise
/// --changeset`, the `ls` aliases, `--state`, `ROLL_AGENT`, every lease command's text, and
/// the errors: priority bounds, someone else's or no lease, landing a landed or closed change
/// (exit 1, not the conflict exit 2), dequeueing what isn't queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_and_work_edges() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    e.write("a.txt", "a\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "a.txt"]).await;

    // Work.
    let out = e.ok(&["code", "work", "create", "acme/api", "Fix it", "--label", "bug"]).await;
    assert!(out.contains("#1 · Fix it"), "{out}");
    let docs = e.json(&["code", "work", "create", "acme/api", "Docs", "-p", "3"]).await;
    let err = e.fails(&["code", "work", "create", "acme/api", "Too low", "-p", "4"], 1).await;
    assert!(err.contains("--priority is 0"), "{err}");
    assert_eq!(e.json(&["code", "work", "ls", "acme/api"]).await.as_array().unwrap().len(), 2);
    assert!(e.json(&["code", "work", "list", "acme/api", "--state", "done"]).await.as_array().unwrap().is_empty());
    let list = e.ok(&["code", "work", "list", "acme/api"]).await;
    assert!(list.contains("P3") && list.contains("[bug]"), "{list}");
    // ROLL_AGENT stands in for --agent.
    let out = e.cmd(&["code", "work", "claim", "acme/api", "--label", "bug"]).env("ROLL_AGENT", "env-bot").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let id = stdout(&out).trim().to_string();
    assert!(stderr(&out).contains("claimed #1"), "{}", stderr(&out));
    let shown = e.ok(&["code", "work", "show", "acme/api#1"]).await;
    assert!(shown.contains("claimed by env-bot, lease") && shown.contains("labels bug"), "{shown}");
    let out = e.cmd(&["code", "work", "heartbeat", &id]).env("ROLL_AGENT", "env-bot").output().unwrap();
    assert!(out.status.success() && stdout(&out).contains("lease renewed"), "{}", stderr(&out));
    assert!(e.ok(&["code", "work", "heartbeat", "acme/api#1", "--agent", "env-bot"]).await.contains("lease renewed"));
    e.fails(&["code", "work", "release", &id, "--agent", "intruder"], 1).await;
    assert!(e.ok(&["code", "work", "release", "acme/api#1", "--agent", "env-bot"]).await.contains("released"));
    let err = e.fails(&["code", "work", "heartbeat", &id, "--agent", "env-bot"], 1).await;
    assert!(err.contains("isn't claimed"), "{err}");
    e.ok(&["code", "work", "claim", "acme/api", "--agent", "a1", "--label", "bug"]).await;
    assert!(e.ok(&["code", "work", "complete", "acme/api#1", "--agent", "a1"]).await.contains("· done"));
    assert!(e.ok(&["code", "work", "show", &id]).await.contains("worked on by a1"));
    e.fails(&["code", "work", "show", "wi_nope"], 1).await;
    let err = e.fails(&["code", "work", "show", "acme/api#x"], 1).await;
    assert!(err.contains("expected owner/repo#number"), "{err}");
    e.fails(&["code", "work", "complete", "acme/api"], 1).await;

    // A change for a work item, by id and by number: the item goes to review.
    let wid = docs["id"].as_str().unwrap().to_string();
    e.write("doc.md", "d\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "Docs", "--base", "main", "--work", &wid, "doc.md"]).await;
    assert_eq!(c["work_item"], wid.as_str());
    assert_eq!(e.json(&["code", "work", "show", &wid]).await["state"], "review");
    let num = format!("acme/api#{}", c["number"]);
    let shown = e.ok(&["code", "change", "show", &num]).await;
    assert!(shown.contains(&format!("resolves {wid}")) && shown.contains("Docs") && shown.contains("revision 1"), "{shown}");
    let third = e.json(&["code", "work", "create", "acme/api", "Third"]).await;
    let c2 = e.json(&["code", "change", "create", "acme/api", "-t", "Third", "--changeset", "main", "--work", &third["number"].to_string()]).await;
    assert_eq!(c2["work_item"], third["id"]);
    e.fails(&["code", "change", "create", "acme/api", "-t", "x", "--changeset", "main", "--work", "99"], 1).await;
    assert_eq!(e.json(&["code", "change", "ls", "acme/api", "--state", "open"]).await.as_array().unwrap().len(), 2);

    // revise --changeset points a change at an existing changeset (a branch's head); revise
    // from files takes the title as the message.
    e.write("b.txt", "b\n");
    let br = e.json(&["code", "land", "acme/api", "--base", "main", "-m", "topic", "--ref", "topic", "b.txt"]).await;
    let r = e.json(&["code", "change", "revise", c["id"].as_str().unwrap(), "--changeset", br["changeset"].as_str().unwrap()]).await;
    assert_eq!((r["revision"].as_u64(), r["head"].as_str()), (Some(2), br["changeset"].as_str()), "{r}");
    e.write("doc.md", "d2\n");
    let out = e.ok(&["code", "change", "revise", &num, "--base", "main", "doc.md"]).await;
    assert!(out.contains("revision 3"), "{out}");

    // check by number, and what show prints of it.
    let out = e.ok(&["code", "check", &num, "lint", "running", "--summary", "linting", "--url", "https://ci.example/9"]).await;
    assert!(out.contains("lint running"), "{out}");
    let shown = e.ok(&["code", "change", "show", &num]).await;
    assert!(shown.contains("check lint") && shown.contains("running") && shown.contains("linting") && shown.contains("https://ci.example/9"), "{shown}");
    e.fails(&["code", "check", "chg_nope", "lint", "success"], 1).await;

    // Not queued: dequeue refuses. A draft can't land; ready, it lands; again is exit 1.
    e.fails(&["code", "change", "dequeue", &num], 1).await;
    assert!(e.ok(&["code", "change", "draft", &num]).await.contains("is a draft"));
    let err = e.fails(&["code", "change", "land", &num], 1).await;
    assert!(err.contains("draft"), "{err}");
    assert!(e.ok(&["code", "change", "ready", &num]).await.contains("is ready"));
    assert!(e.ok(&["code", "change", "land", &num]).await.contains("✓ landed"));
    let err = e.fails(&["code", "change", "land", &num], 1).await;
    assert!(!err.contains("conflict on"), "a landed change isn't a conflict: {err}");
    let out = e.run(&["--json", "code", "change", "land", &num], None).await;
    assert_eq!(out.status.code(), Some(1));
    // Closed: text, then landing it is exit 1 too; a closed change can't be revised.
    let c2n = format!("acme/api#{}", c2["number"]);
    assert!(e.ok(&["code", "change", "close", &c2n]).await.contains(&format!("closed {c2n}").replace('#', " #")));
    e.fails(&["code", "change", "land", &c2n], 1).await;
    e.fails(&["code", "change", "revise", &c2n, "--changeset", "main"], 1).await;
    e.fails(&["code", "change", "show", "acme/api#99"], 1).await;
}

/// `roll code queue <repo> --watch`: the queue, then its events as they happen; with `--json`,
/// one JSON event per line. Following a repo that doesn't exist fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_watch_follows_the_queue() {
    let e = env(None).await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    e.write("a.txt", "a\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "a.txt"]).await;
    e._code.update_repo(&code_control::Caller::Admin, "acme", "api", &json!({ "land_policy": "queue" })).await.unwrap();
    e.fails(&["code", "queue", "acme/nope", "--watch"], 1).await;
    e.fails(&["code", "watch", "acme/nope"], 1).await;

    let follow = |args: &[&str]| {
        let mut child = e.cmd(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        (child, rx)
    };
    let next = |rx: &std::sync::mpsc::Receiver<String>, want: &dyn Fn(&str) -> bool| {
        let mut seen = Vec::new();
        loop {
            let line = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| panic!("no matching line in {seen:?}"));
            if want(&line) {
                return line;
            }
            seen.push(line);
        }
    };

    let (mut child, rx) = follow(&["code", "queue", "acme/api", "--watch"]);
    let first = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("the queue first");
    assert!(first.starts_with("acme/api · ") && first.contains("0 queued"), "{first}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    e.write("b.txt", "b\n");
    assert!(e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "add b", "b.txt"]).await.contains("queued"));
    let line = next(&rx, &|l| l.starts_with("· change #1 queued") || l.starts_with("· queue "));
    assert!(line.contains("queued"), "{line}");
    child.kill().unwrap();
    let _ = child.wait();

    let (mut child, rx) = follow(&["--json", "code", "queue", "acme/api", "--watch"]);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    e.write("c.txt", "c\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "add c", "c.txt"]).await;
    let line = next(&rx, &|l| l.contains("add c"));
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
    assert!(v["event"].is_string() && v["data"].is_object(), "{v}");
    child.kill().unwrap();
    let _ = child.wait();
}

/// Reviewer, billing, provider, learnings, search, webhook, thread, review-app and review flags
/// and errors the other tests don't reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_webhook_and_thread_edges() {
    let e = env_with(|c| {
        c.review.secret_key = Some("0123456789abcdef0123456789abcdef".into());
        c.allow_private_webhooks = Some(true);
    })
    .await;
    e.ok(&["code", "owners", "create", "acme", "--org"]).await;
    e.ok(&["code", "create", "acme/api"]).await;
    e.write("notes.txt", "1\n2\n3\n");
    e.write("src/x.rs", "Two two\n");
    e.write("docs/y.md", "two\n");
    e.ok(&["code", "land", "acme/api", "--base", "main", "-m", "init", "notes.txt", "src/x.rs", "docs/y.md"]).await;

    // reviewer: --disable, and its usage and value errors.
    assert_eq!(e.json(&["code", "reviewer", "acme/api", "--enable"]).await["effective"]["enabled"], true);
    assert!(e.ok(&["code", "reviewer", "acme/api", "--disable"]).await.contains("enabled      false"));
    e.fails(&["code", "reviewer", "acme/api", "--enable", "--disable"], 1).await;
    e.fails(&["code", "reviewer", "acme/api", "--speed", "warp"], 1).await;
    let err = e.fails(&["code", "reviewer", "acme/api", "--set", "novalue"], 1).await;
    assert!(err.contains("expected key=value"), "{err}");
    e.fails(&["code", "reviewer", "acme/nope"], 1).await;

    // billing: shown without flags; a bad --on-cap.
    assert!(e.ok(&["code", "billing", "acme"]).await.contains("acme: no cap"));
    e.fails(&["code", "billing", "acme", "--on-cap", "explode"], 1).await;
    assert_eq!(e.json(&["code", "billing", "acme", "--on-cap", "pause"]).await["on_cap"], "pause");
    e.fails(&["code", "usage", "nobody"], 1).await;

    // provider: OpenAI-compatible with a base URL and models, the key from ROLL_REVIEW_API_KEY.
    let out = e
        .cmd(&["code", "provider", "set", "acme", "--provider", "openai_compatible", "--base-url", "https://llm.example.com/v1", "--model", "m-large", "--secondary-model", "m-small"])
        .env("ROLL_REVIEW_API_KEY", "sk-env-key-0000-9876")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("openai_compatible · m-large · key …9876") && !text.contains("sk-env"), "{text}");
    let p = e.json(&["code", "provider", "show", "acme"]).await;
    assert_eq!((p["model"].as_str(), p["key_last4"].as_str()), (Some("m-large"), Some("9876")), "{p}");
    assert!(!p.to_string().contains("sk-env-key"));
    e.fails(&["code", "provider", "set", "acme", "--provider", "openai_compatible", "--api-key", "sk-x-1234"], 1).await;
    e.fails(&["code", "provider", "set", "acme", "--provider", "gemini", "--api-key", "sk-x-1234"], 1).await;
    let out = e.cmd(&["code", "provider", "set", "acme"]).env_remove("ROLL_REVIEW_API_KEY").output().unwrap();
    assert_eq!(out.status.code(), Some(1), "no key: {}", stderr(&out));

    // learnings: a repo-wide one, and forgetting what isn't there.
    let l = e.json(&["code", "learnings", "add", "acme/api", "Prefer early returns"]).await;
    assert!(l["path"].is_null());
    assert!(e.ok(&["code", "learnings", "list", "acme/api"]).await.contains("Prefer early returns"));
    e.fails(&["code", "learnings", "rm", "acme/api", "lrn_nope"], 1).await;

    // search: --case and --path.
    assert_eq!(e.ok(&["code", "search", "acme/api", "Two", "--case"]).await.trim(), "src/x.rs:1: Two two");
    assert_eq!(e.ok(&["code", "search", "acme/api", "two", "--path", "docs/**"]).await.trim(), "docs/y.md:1: two");
    assert_eq!(e.ok(&["code", "search", "acme/api", "two", "--path", "nowhere/**"]).await.trim(), "");
    e.fails(&["code", "search", "acme/nope", "two"], 1).await;

    // webhooks: none yet; your own secret (not printed back) and created paused; refusals.
    assert!(e.ok(&["code", "webhooks", "list", "acme/api"]).await.contains("no webhooks"));
    let out = e.run(&["code", "webhooks", "create", "acme/api", "http://127.0.0.1:9/h", "--secret", "my-own-secret-123456", "--inactive"], None).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("paused") && !stdout(&out).contains("my-own-secret") && !stderr(&out).contains("shown once"), "{}", stdout(&out));
    let hooks = e.json(&["code", "webhooks", "list", "acme/api"]).await;
    let hid = hooks[0]["id"].as_str().unwrap().to_string();
    assert_eq!((hooks[0]["active"].as_bool(), hooks[0]["events"].clone()), (Some(false), json!(["landed"])), "the default event");
    e.fails(&["code", "webhooks", "create", "acme/api", "http://127.0.0.1:9/h", "--secret", "short"], 1).await;
    e.fails(&["code", "webhooks", "create", "acme/api", "http://127.0.0.1:9/h", "--event", "nope"], 1).await;
    let v = e.json(&["code", "webhooks", "update", "acme/api", &hid, "--url", "http://127.0.0.1:9/other", "--secret", "another-secret-123456"]).await;
    assert_eq!(v["url"], "http://127.0.0.1:9/other");
    e.fails(&["code", "webhooks", "update", "acme/api", &hid, "--pause", "--resume"], 1).await;
    for args in [vec!["ping", "acme/api", "wh_nope"], vec!["delete", "acme/api", "wh_nope"], vec!["deliveries", "acme/api", "wh_nope"], vec!["redeliver", "acme/api", hid.as_str(), "dl_nope"]] {
        let mut all = vec!["code", "webhooks"];
        all.extend(args);
        e.fails(&all, 1).await;
    }

    // threads: a range on the old side; applying one comment's suggestion out of two.
    e.write("notes.txt", "1\ntwo\n3\n");
    let c = e.json(&["code", "change", "create", "acme/api", "-t", "Two", "--base", "main", "-m", "two", "notes.txt"]).await;
    let id = c["id"].as_str().unwrap().to_string();
    let t = e.json(&["code", "thread", "open", &id, "--path", "notes.txt", "--line", "2", "--start-line", "1", "--side", "old", "-m", "was 1-2"]).await;
    assert_eq!((t["start_line"].as_u64(), t["line"].as_u64(), t["side"].as_str()), (Some(1), Some(2), Some("old")), "{t}");
    assert!(e.ok(&["code", "thread", "list", &id]).await.contains("notes.txt:1-2 (old)  open"));
    let opened = e.ok(&["code", "thread", "open", &id, "--path", "notes.txt", "--line", "3", "-m", "three?"]).await;
    assert!(opened.contains("notes.txt:3  open") && opened.contains("three?"), "{opened}");
    let th = e.json(&["code", "thread", "open", &id, "--path", "notes.txt", "--line", "2", "-m", "```suggestion\nTWO\n```"]).await;
    let th_id = th["id"].as_str().unwrap().to_string();
    let first = th["comments"][0]["id"].as_str().unwrap().to_string();
    e.ok(&["code", "thread", "reply", &th_id, "-m", "```suggestion\nDos\n```"]).await;
    e.fails(&["code", "thread", "apply", &th_id, "--comment", "cm_nope"], 1).await;
    let applied = e.ok(&["code", "thread", "apply", &th_id, "--comment", &first]).await;
    assert!(applied.contains("applied: change #1 is at revision 2"), "{applied}");
    let head = e.json(&["code", "change", "show", &id]).await["head"].as_str().unwrap().to_string();
    assert_eq!(e.ok(&["code", "cat", &format!("acme/api@{head}"), "notes.txt"]).await, "1\nTWO\n3\n", "the first suggestion, not the latest");
    assert!(e.ok(&["code", "thread", "resolve", &th_id]).await.contains("resolved"));
    assert!(!e.ok(&["code", "thread", "list", &id, "--open"]).await.contains(&th_id));
    // Unresolved again; its line changed with the suggestion, so it's outdated now.
    let un = e.ok(&["code", "thread", "unresolve", &th_id]).await;
    assert!(un.contains("notes.txt:2  outdated"), "{un}");
    for args in [vec!["reply", "th_nope", "-m", "x"], vec!["resolve", "th_nope"], vec!["list", "chg_nope"]] {
        let mut all = vec!["code", "thread"];
        all.extend(args);
        e.fails(&all, 1).await;
    }

    // review: what request and dismiss need, an unknown review, an unknown change.
    assert!(e.fails(&["code", "review", &id, "request"], 1).await.contains("whom?"));
    e.fails(&["code", "review", &id, "dismiss", "rv_nope"], 1).await;
    e.fails(&["code", "review", "chg_nope"], 1).await;
    e.fails(&["code", "review", &id, "approve", "--comment", "nocolon"], 1).await;
    let out = e.ok(&["code", "review", &id, "comment", "-m", "a note", "--comment", "old:notes.txt:2=was 2"]).await;
    assert!(out.contains("comment by") && out.contains("with 1 comment"), "{out}");
    assert!(e.ok(&["code", "review", &id, "request", "@ada"]).await.contains("requested a review from @ada"));
    e.fails(&["code", "review-settings", "acme/api", "--set", "novalue"], 1).await;
    e.fails(&["code", "jobs", "acme/api#99"], 1).await;
    e.fails(&["code", "status", "acme/api", "no-such-branch"], 1).await;
    e.fails(&["code", "blame", "acme/nope", "notes.txt"], 1).await;

    // review apps: --active false, and refusals.
    let app = e.json(&["code", "review-apps", "create", "acme", "bot", "--url", "http://127.0.0.1:9/h"]).await;
    let app_id = app["app"]["id"].as_str().unwrap().to_string();
    let v = e.json(&["code", "review-apps", "update", &app_id, "--active", "false", "--url", "http://127.0.0.1:9/new", "--repo", "api"]).await;
    assert_eq!((v["active"].as_bool(), v["webhook_url"].as_str()), (Some(false), Some("http://127.0.0.1:9/new")), "{v}");
    e.fails(&["code", "review-apps", "show", "app_nope"], 1).await;
    e.fails(&["code", "review-apps", "create", "acme", "x", "--url", "http://127.0.0.1:9/h", "--event", "bogus"], 1).await;
    e.fails(&["code", "review-apps", "list", "nobody"], 1).await;
}
