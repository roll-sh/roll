//! `roll storage …` end to end: the store, the S3 API and the control plane run in process (as in
//! s3-control's tests), and the real `roll` binary talks to them through `ROLL_API_URL`.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use reqwest::Client;
use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

struct T {
    api: String,
    s3: String,
    store: Arc<Store>,
    http: Client,
    /// The working directory of every `roll` run.
    work: tempfile::TempDir,
    _data: tempfile::TempDir,
}

struct Out {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

async fn start() -> T {
    let data = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("roll-cli").await;
    let store = Store::open(StoreConfig::new(data.path().join("meta")).manual(), backend).await.unwrap();

    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    // Webhooks may reach this machine (the tests' receivers listen on loopback).
    let notify = s3_server::NotifyConfig { webhook_allow_private: true, ..Default::default() };
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: data.path().to_path_buf(), notify, ..Default::default() });
    let notifier = server.notifier.clone();
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });

    let control = Control::new(store.clone(), ControlConfig::local(s3.clone())).await.unwrap();
    control.set_notifier(notifier);
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, s3, store, http: Client::new(), work: tempfile::tempdir().unwrap(), _data: data }
}

impl T {
    fn path(&self, rel: &str) -> PathBuf {
        self.work.path().join(rel)
    }

    fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, data).unwrap();
        p
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// `roll <args>` against `api`, with `stdin` (or nothing) as its standard input.
    async fn run_at(&self, api: &str, args: &[&str], stdin: Option<Vec<u8>>) -> Out {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_roll"));
        cmd.args(args)
            .current_dir(self.work.path())
            .env("ROLL_API_URL", api)
            .env_remove("ROLL_API_KEY")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        // Feed stdin while the child runs, so a large payload can't deadlock on full pipes.
        let feeder = stdin.map(|data| {
            let mut pipe = child.stdin.take().unwrap();
            tokio::spawn(async move {
                pipe.write_all(&data).await.unwrap();
                pipe.shutdown().await.unwrap();
            })
        });
        let out = child.wait_with_output().await.unwrap();
        if let Some(f) = feeder {
            f.await.unwrap();
        }
        Out { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
    }

    /// `roll storage <args>`.
    async fn run(&self, args: &[&str]) -> Out {
        let mut all = vec!["storage"];
        all.extend_from_slice(args);
        self.run_at(&self.api, &all, None).await
    }

    /// `roll storage <args>`, which must succeed; its stdout.
    async fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args).await;
        assert_eq!(o.code, Some(0), "roll storage {args:?} failed: {}\n{}", o.stderr, o.text());
        o.text()
    }

    /// `roll storage <args> --json`, which must succeed; its stdout as JSON.
    async fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let text = self.ok(&all).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("roll storage {all:?}: not JSON ({e}): {text}"))
    }

    /// `roll storage <args>`, which must fail with exit status 1 and print nothing on stdout; its
    /// stderr.
    async fn fail(&self, args: &[&str]) -> String {
        let o = self.run(args).await;
        assert_eq!(o.code, Some(1), "roll storage {args:?} should fail: {}\n{}", o.text(), o.stderr);
        assert!(o.stdout.is_empty(), "roll storage {args:?}: stdout {}", o.text());
        o.stderr
    }

    /// The bytes of an object, straight from the API (not through the CLI).
    async fn get(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        let r = self.http.get(format!("{}/v1/storage/buckets/{bucket}/download?key={}", self.api, enc(key))).send().await.unwrap();
        if r.status() == 404 {
            return None;
        }
        assert!(r.status().is_success(), "download {bucket}/{key}: {}", r.status());
        Some(r.bytes().await.unwrap().to_vec())
    }

    /// Upload straight through the API.
    async fn put(&self, bucket: &str, key: &str, data: &[u8]) {
        let r = self.http.put(format!("{}/v1/storage/buckets/{bucket}/upload?key={}", self.api, enc(key))).body(data.to_vec()).send().await.unwrap();
        assert!(r.status().is_success(), "upload {bucket}/{key}: {}", r.text().await.unwrap());
    }

    fn sdk(&self, access: &str, secret: &str) -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&self.s3)
            .credentials_provider(Credentials::new(access, secret, None, None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}

/// Percent-encode everything but unreserved characters.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn keys(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap().to_string()).collect()
}

/// Deterministic, non-repeating-looking bytes.
fn pattern(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x9e37_79b9;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn info_and_bucket_listing() {
    let t = start().await;

    let text = t.ok(&["info"]).await;
    assert!(text.contains(&format!("endpoint      {}", t.s3)), "{text}");
    assert!(text.contains("region        us-east-1"), "{text}");
    assert!(text.contains("buckets       0  objects 0  0 B"), "{text}");
    assert!(text.contains("recover       7 days by default"), "{text}");
    assert!(text.contains("pricing       $0.023/GB-month"), "{text}");
    assert!(!text.contains("virtual host"), "{text}");
    let v = t.json(&["info"]).await;
    assert_eq!(v["endpoint"], t.s3);
    assert_eq!(v["region"], "us-east-1");
    assert_eq!(v["pricing"]["usd_per_gb_month"], 0.023);
    assert_eq!(v["buckets"], 0);

    // No buckets yet.
    assert_eq!(t.ok(&["buckets"]).await, "no buckets; create one with `roll storage create <name>`\n");
    assert_eq!(t.json(&["buckets"]).await, json!([]));
    assert_eq!(t.json(&["ls"]).await, json!([]));

    // `roll s3` is the same command, and `--json` works before the subcommand too.
    let a = t.run_at(&t.api, &["--json", "s3", "info"], None).await;
    assert_eq!(a.code, Some(0), "{}", a.stderr);
    assert_eq!(serde_json::from_str::<Value>(&a.text()).unwrap()["endpoint"], t.s3);
    // A trailing slash on the API URL is fine.
    let a = t.run_at(&format!("{}/", t.api), &["storage", "buckets", "--json"], None).await;
    assert_eq!(a.code, Some(0), "{}", a.stderr);

    t.ok(&["create", "photos"]).await;
    t.ok(&["create", "assets", "--public", "--recover-days", "0"]).await;
    let text = t.ok(&["buckets"]).await;
    let header = text.lines().next().unwrap();
    for col in ["name", "objects", "size", "access", "recover", "branch of", "created"] {
        assert!(header.contains(col), "{header}");
    }
    let photos = text.lines().find(|l| l.starts_with("photos")).unwrap_or_else(|| panic!("{text}"));
    assert!(photos.contains("private") && photos.contains("7d"), "{photos}");
    let assets = text.lines().find(|l| l.starts_with("assets")).unwrap_or_else(|| panic!("{text}"));
    assert!(assets.contains("public") && assets.contains("0d"), "{assets}");

    let list = t.json(&["buckets"]).await;
    let mut names: Vec<&str> = list.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["assets", "photos"]);
    // `ls` without a path is the bucket list.
    assert_eq!(t.json(&["ls"]).await, list);
    assert_eq!(t.ok(&["ls"]).await, t.ok(&["buckets"]).await);
    let v = t.json(&["info"]).await;
    assert_eq!(v["buckets"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_show_and_errors() {
    let t = start().await;
    let text = t.ok(&["create", "photos"]).await;
    assert_eq!(text, format!("created bucket photos\n{}/photos\n", t.s3));

    let b = t.json(&["create", "public-assets", "--public", "--recover-days", "30"]).await;
    assert_eq!((b["name"].as_str(), b["public"].as_bool(), b["recover_days"].as_u64()), (Some("public-assets"), Some(true), Some(30)));
    assert!(b["id"].as_str().unwrap().starts_with("bkt_"));
    assert_eq!(b["url"], format!("{}/public-assets", t.s3));
    let b = t.json(&["create", "no-trash", "--recover-days", "0"]).await;
    assert_eq!((b["public"].as_bool(), b["recover_days"].as_u64()), (Some(false), Some(0)));
    let b = t.json(&["create", "defaults"]).await;
    assert_eq!((b["public"].as_bool(), b["recover_days"].as_u64()), (Some(false), Some(7)));

    // The API's error message reaches stderr, with a non-zero exit status.
    assert!(t.fail(&["create", "photos"]).await.contains("a bucket named photos already exists"));
    assert!(t.fail(&["create", "Bad_Name"]).await.contains("invalid bucket name"));
    assert!(t.fail(&["create", "ok-name", "--recover-days", "31"]).await.contains("recover_days is 0-30"));
    assert!(!t.fail(&["create", "ok-name", "--recover-days=-1"]).await.is_empty());
    assert!(t.fail(&["show", "missing"]).await.contains("no bucket named missing"));
    assert!(t.fail(&["show", "missing", "--json"]).await.contains("no bucket named missing"));
    // Usage errors come from clap: status 2.
    let o = t.run(&["create"]).await;
    assert_eq!(o.code, Some(2), "{}", o.stderr);

    t.put("photos", "a.txt", b"hello").await;
    let text = t.ok(&["show", "photos"]).await;
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("photos  bkt_") && first.ends_with("  us-east-1"), "{text}");
    assert!(text.contains("  objects    1  (5 B)"), "{text}");
    assert!(text.contains("  trash      0 B  (recoverable 7 days)"), "{text}");
    assert!(text.contains("  access     private"), "{text}");
    assert!(text.contains(&format!("  url        {}/photos", t.s3)), "{text}");
    assert!(!text.contains("branch") && !text.contains("cors"), "{text}");
    assert!(t.ok(&["show", "public-assets"]).await.contains("  access     public (anonymous GET)"));

    let v = t.json(&["show", "photos"]).await;
    assert_eq!((v["objects"].as_u64(), v["bytes"].as_u64()), (Some(1), Some(5)));
    assert_eq!(v["endpoint"], t.s3);
    assert_eq!(v["parent"], Value::Null);
    assert_eq!(v["branches"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn update_settings() {
    let t = start().await;
    t.ok(&["create", "site"]).await;

    let v = t.json(&["update", "site", "--public"]).await;
    assert_eq!(v["public"], true);
    let v = t.json(&["update", "site", "--recover-days", "14"]).await;
    assert_eq!((v["public"].as_bool(), v["recover_days"].as_u64()), (Some(true), Some(14)));
    let v = t.json(&["update", "site", "--private"]).await;
    assert_eq!((v["public"].as_bool(), v["recover_days"].as_u64()), (Some(false), Some(14)));

    let v = t.json(&["update", "site", "--cors-origin", "https://app.example", "--cors-origin", "*"]).await;
    assert_eq!(
        v["cors"],
        json!([{
            "id": null,
            "allowed_origins": ["https://app.example", "*"],
            "allowed_methods": ["GET", "HEAD", "PUT", "POST", "DELETE"],
            "allowed_headers": ["*"],
            "expose_headers": ["ETag"],
            "max_age_s": 3600,
        }])
    );
    // The CORS rule lets browsers in through the S3 API.
    let pre = t
        .http
        .request(reqwest::Method::OPTIONS, format!("{}/site/x", t.s3))
        .header("origin", "https://app.example")
        .header("access-control-request-method", "PUT")
        .send()
        .await
        .unwrap();
    assert!(pre.status().is_success(), "{}", pre.status());
    let allowed = pre.headers()["access-control-allow-origin"].to_str().unwrap().to_string();
    assert!(allowed == "https://app.example" || allowed == "*", "{allowed}");

    // Text output is the bucket again.
    let text = t.ok(&["update", "site", "--public", "--recover-days", "3"]).await;
    assert!(text.contains("  access     public (anonymous GET)"), "{text}");
    assert!(text.contains("recoverable 3 days"), "{text}");
    assert!(text.contains("  cors       https://app.example,* → GET,HEAD,PUT,POST,DELETE"), "{text}");

    let v = t.json(&["update", "site", "--no-cors"]).await;
    assert_eq!(v["cors"], json!([]));
    // Other settings stay.
    assert_eq!((v["public"].as_bool(), v["recover_days"].as_u64()), (Some(true), Some(3)));
    assert!(!t.ok(&["show", "site"]).await.contains("cors"));

    // Nothing to change: refused before calling the API.
    let e = t.fail(&["update", "site"]).await;
    assert!(e.contains("nothing to change: pass --public, --private, --recover-days, --cors-origin, --no-cors, --tag, --no-tags or --versioning"), "{e}");

    // Bucket tags: replaced whole, shown, removed.
    let v = t.json(&["update", "site", "--tag", "team=web", "--tag", "env=prod"]).await;
    assert_eq!(v["tags"], json!({ "env": "prod", "team": "web" }));
    let shown = t.ok(&["show", "site"]).await;
    assert!(shown.contains("tag        env = prod") && shown.contains("tag        team = web"), "{shown}");
    assert_eq!(t.json(&["update", "site", "--tag", "only=one"]).await["tags"], json!({ "only": "one" }));
    assert_eq!(t.json(&["update", "site", "--no-tags"]).await["tags"], json!({}));
    assert!(t.fail(&["update", "site", "--tag", "novalue"]).await.contains("expected key=value"));
    assert!(t.fail(&["update", "site", "--tag", "aws:x=1"]).await.contains("System tags"));
    let e = t.run_at("http://127.0.0.1:1", &["storage", "update", "site"], None).await;
    assert!(e.stderr.contains("nothing to change"), "{}", e.stderr);
    // Conflicting flags are usage errors.
    assert_eq!(t.run(&["update", "site", "--public", "--private"]).await.code, Some(2));
    assert_eq!(t.run(&["update", "site", "--no-cors", "--cors-origin", "*"]).await.code, Some(2));
    // The API's validation.
    assert!(t.fail(&["update", "site", "--recover-days", "99"]).await.contains("recover_days is 0-30"));
    assert!(t.fail(&["update", "site", "--cors-origin", "https://*.a.*"]).await.contains("more than one wildcard"));
    assert!(t.fail(&["update", "nope", "--public"]).await.contains("no bucket named nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_and_delete() {
    let t = start().await;
    t.ok(&["create", "main"]).await;
    t.put("main", "a.txt", b"one").await;

    let text = t.ok(&["branch", "main", "preview"]).await;
    assert_eq!(text, format!("branch preview of main (copy-on-write, nothing copied)\n{}/preview\n", t.s3));
    let b = t.json(&["branch", "main", "preview-2"]).await;
    assert_eq!(b["name"], "preview-2");
    assert_eq!(b["parent"]["name"], "main");
    assert!(b["parent"]["at"].as_i64().unwrap() > 0);

    // The branch sees the parent's objects; writes stay on their own side.
    assert_eq!(t.get("preview", "a.txt").await.unwrap(), b"one");
    t.put("main", "a.txt", b"two").await;
    assert_eq!(t.get("preview", "a.txt").await.unwrap(), b"one");

    let text = t.ok(&["show", "main"]).await;
    assert!(text.contains("  branches   preview, preview-2"), "{text}");
    let text = t.ok(&["show", "preview"]).await;
    assert!(text.contains("  branch of  main at "), "{text}");
    let list = t.ok(&["buckets"]).await;
    let row = list.lines().find(|l| l.starts_with("preview-2")).unwrap();
    assert!(row.contains("main"), "{row}");

    assert!(t.fail(&["branch", "main", "main"]).await.contains("already exists"));
    assert!(t.fail(&["branch", "missing", "x-branch"]).await.contains("no bucket named missing"));

    // Not while it has branches, even forced.
    assert!(t.fail(&["delete", "main"]).await.contains("bucket main has branches; delete them first"));
    assert!(t.fail(&["delete", "main", "--force"]).await.contains("has branches"));

    // A branch sees its parent's objects, so it isn't empty either; forcing it leaves the parent
    // alone.
    assert!(t.fail(&["delete", "preview-2"]).await.contains("bucket preview-2 isn't empty"));
    assert_eq!(t.ok(&["delete", "preview-2", "--force"]).await, "deleted bucket preview-2\n");
    assert_eq!(t.json(&["delete", "preview", "--force"]).await, json!({ "deleted": "preview" }));
    assert_eq!(t.get("main", "a.txt").await.unwrap(), b"two");
    assert_eq!(t.json(&["show", "main"]).await["branches"], json!([]));

    // Not empty: refused without --force.
    assert!(t.fail(&["delete", "main"]).await.contains("bucket main isn't empty"));
    assert_eq!(t.json(&["delete", "main", "--force"]).await, json!({ "deleted": "main" }));
    assert_eq!(t.json(&["buckets"]).await, json!([]));
    assert!(t.fail(&["delete", "main"]).await.contains("no bucket named main"));

    t.ok(&["create", "empty"]).await;
    assert_eq!(t.ok(&["delete", "empty"]).await, "deleted bucket empty\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_variants() {
    let t = start().await;
    t.ok(&["create", "upb"]).await;
    t.write("hello.txt", b"hello world");
    t.write("page.html", b"<h1>hi</h1>");
    t.write("blob", b"\x00\x01\x02");

    // file → key
    let text = t.ok(&["cp", "hello.txt", "upb/greeting.txt"]).await;
    assert_eq!(text, "hello.txt → upb/greeting.txt  11 B\n");
    assert_eq!(t.get("upb", "greeting.txt").await.unwrap(), b"hello world");
    // file → "dir/" keeps the file name; a bare bucket or `bucket/` uploads to the top level.
    let v = t.json(&["cp", "hello.txt", "upb/docs/"]).await;
    assert_eq!((v["key"].as_str(), v["size"].as_u64()), (Some("docs/hello.txt"), Some(11)));
    assert_eq!(v["content_type"], "text/plain; charset=utf-8");
    assert_eq!(v["etag"], "5eb63bbbe01eeed093cb22bb8f5acdc3");
    assert_eq!(t.json(&["cp", "hello.txt", "upb"]).await["key"], "hello.txt");
    assert_eq!(t.json(&["cp", "page.html", "s3://upb/"]).await["key"], "page.html");
    // An absolute path.
    let abs = t.path("hello.txt");
    assert_eq!(t.json(&["cp", abs.to_str().unwrap(), "s3://upb/abs/x.txt"]).await["key"], "abs/x.txt");

    // Content types: guessed from the extension, or given.
    assert_eq!(t.json(&["stat", "upb/page.html"]).await["content_type"], "text/html; charset=utf-8");
    assert_eq!(t.json(&["cp", "blob", "upb/blob"]).await["content_type"], "application/octet-stream");
    let v = t.json(&["cp", "hello.txt", "upb/typed.bin", "--content-type", "application/x-custom"]).await;
    assert_eq!(v["content_type"], "application/x-custom");
    let r = t.http.get(format!("{}/v1/storage/buckets/upb/download?key=typed.bin", t.api)).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "application/x-custom");

    // Keys with spaces, unicode, `+`, `%`, `?` and `#` survive the query string.
    let odd = "dir/a b+c%20d?e#f ü日本.txt";
    let v = t.json(&["cp", "hello.txt", &format!("upb/{odd}")]).await;
    assert_eq!(v["key"], odd);
    assert_eq!(t.get("upb", odd).await.unwrap(), b"hello world");
    assert_eq!(t.json(&["stat", &format!("upb/{odd}")]).await["key"], odd);
    assert!(t.get("upb", "dir/a b c%20d").await.is_none());

    // An empty file.
    t.write("empty.txt", b"");
    let v = t.json(&["cp", "empty.txt", "upb/empty.txt"]).await;
    assert_eq!(v["size"], 0);

    // -r: a directory, nested, under a prefix (with or without the trailing slash).
    t.write("site/index.html", b"<html>");
    t.write("site/css/app.css", b"body{}");
    t.write("site/img/deep/logo.svg", b"<svg/>");
    let text = t.ok(&["cp", "-r", "site", "upb/www"]).await;
    assert!(text.ends_with("uploaded 3 files (18 B)\n"), "{text}");
    assert!(text.contains("→ upb/www/css/app.css"), "{text}");
    let v = t.json(&["ls", "-r", "upb/www/"]).await;
    assert_eq!(keys(&v["objects"]), ["www/css/app.css", "www/img/deep/logo.svg", "www/index.html"]);
    assert_eq!(t.json(&["stat", "upb/www/css/app.css"]).await["content_type"], "text/css; charset=utf-8");
    assert_eq!(t.json(&["stat", "upb/www/img/deep/logo.svg"]).await["content_type"], "image/svg+xml");
    assert_eq!(t.json(&["cp", "--recursive", "site", "upb/www2/"]).await, json!({ "uploaded": 3, "bytes": 18 }));
    assert_eq!(t.json(&["cp", "-r", "site", "s3://upb"]).await, json!({ "uploaded": 3, "bytes": 18 }));
    assert_eq!(t.get("upb", "index.html").await.unwrap(), b"<html>");
    // --content-type applies to every file of a -r upload.
    t.json(&["cp", "-r", "site", "upb/typed/", "--content-type", "text/plain"]).await;
    assert_eq!(t.json(&["stat", "upb/typed/css/app.css"]).await["content_type"], "text/plain");

    // Mistakes.
    assert!(t.fail(&["cp", "site", "upb/x/"]).await.contains("site is a directory; use -r"));
    assert!(t.fail(&["cp", "-r", "hello.txt", "upb/x/"]).await.contains("hello.txt is not a directory"));
    assert!(t.fail(&["cp", "hello.txt", "/key"]).await.contains("expected <bucket>/<key>"));
    assert!(t.fail(&["cp", "hello.txt", "nope/x"]).await.contains("no bucket named nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_uploads_and_stdout_downloads() {
    let t = start().await;
    t.ok(&["create", "pipe"]).await;

    let small = t.run_at(&t.api, &["storage", "cp", "-", "pipe/small.txt"], Some(b"from stdin\n".to_vec())).await;
    assert_eq!(small.code, Some(0), "{}", small.stderr);
    assert_eq!(small.text(), "stdin → pipe/small.txt  11 B\n");
    assert_eq!(t.get("pipe", "small.txt").await.unwrap(), b"from stdin\n");
    // stdin has no extension to guess from.
    assert_eq!(t.json(&["stat", "pipe/small.txt"]).await["content_type"], "application/octet-stream");

    // A few MB, with --content-type and --json.
    let big = pattern(5 * 1024 * 1024 + 123);
    let o = t.run_at(&t.api, &["--json", "storage", "cp", "-", "s3://pipe/big.bin", "--content-type", "application/x-big"], Some(big.clone())).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!((v["key"].as_str(), v["size"].as_u64()), (Some("big.bin"), Some(big.len() as u64)));
    assert_eq!(v["content_type"], "application/x-big");
    assert_eq!(t.get("pipe", "big.bin").await.unwrap(), big);

    // Empty stdin is an empty object.
    let o = t.run_at(&t.api, &["--json", "storage", "cp", "-", "pipe/empty"], Some(Vec::new())).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(serde_json::from_slice::<Value>(&o.stdout).unwrap()["size"], 0);
    assert_eq!(t.get("pipe", "empty").await.unwrap(), b"");

    // A key is required.
    for dst in ["pipe", "pipe/", "pipe/dir/"] {
        let o = t.run_at(&t.api, &["storage", "cp", "-", dst], Some(b"x".to_vec())).await;
        assert_eq!(o.code, Some(1));
        assert!(o.stderr.contains("give the object a key when uploading from stdin"), "{}", o.stderr);
    }

    // Download to stdout: the bytes and nothing else, with or without --json.
    let o = t.run(&["cp", "pipe/big.bin", "-"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stdout == big, "stdout download differs ({} bytes)", o.stdout.len());
    let o = t.run(&["cp", "s3://pipe/small.txt", "-", "--json"]).await;
    assert_eq!(o.stdout, b"from stdin\n");
    let o = t.run(&["cp", "pipe/empty", "-"]).await;
    assert_eq!((o.code, o.stdout.len()), (Some(0), 0));

    // stdin → stdout round trip through a key with odd characters.
    let key = "pipe/π dir/a+b.bin";
    let o = t.run_at(&t.api, &["storage", "cp", "-", key], Some(vec![0, 255, 10, 13])).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(t.run(&["cp", key, "-"]).await.stdout, [0, 255, 10, 13]);

    assert!(t.fail(&["cp", "pipe/missing", "-"]).await.contains("no such key missing"));
    assert!(t.fail(&["cp", "-r", "pipe/", "-"]).await.contains("can't download several objects to stdout"));
}

#[tokio::test(flavor = "multi_thread")]
async fn download_variants() {
    let t = start().await;
    t.ok(&["create", "dlb"]).await;
    t.put("dlb", "docs/a.txt", b"aaa").await;
    t.put("dlb", "docs/sub/b.txt", b"bbbb").await;
    t.put("dlb", "docs/sub/deeper/c.txt", b"c").await;
    t.put("dlb", "other/x.txt", b"x").await;
    t.put("dlb", "top.txt", b"top").await;

    // To a file.
    let text = t.ok(&["cp", "dlb/docs/a.txt", "out.txt"]).await;
    assert_eq!(text, "dlb/docs/a.txt → out.txt  3 B\n");
    assert_eq!(t.read("out.txt"), b"aaa");
    // Overwrites an existing file.
    t.write("out.txt", b"old contents, longer");
    t.ok(&["cp", "dlb/docs/a.txt", "out.txt"]).await;
    assert_eq!(t.read("out.txt"), b"aaa");
    // Into an existing directory, or a new one named with a trailing slash; parents are created.
    std::fs::create_dir(t.path("existing")).unwrap();
    t.ok(&["cp", "dlb/docs/sub/b.txt", "existing"]).await;
    assert_eq!(t.read("existing/b.txt"), b"bbbb");
    let v = t.json(&["cp", "s3://dlb/docs/sub/b.txt", "fresh/dir/"]).await;
    assert_eq!(v, json!({ "key": "docs/sub/b.txt", "path": "fresh/dir/b.txt", "bytes": 4 }));
    assert_eq!(t.read("fresh/dir/b.txt"), b"bbbb");
    t.ok(&["cp", "dlb/top.txt", "nested/path/renamed.txt"]).await;
    assert_eq!(t.read("nested/path/renamed.txt"), b"top");
    // "." is a directory.
    t.ok(&["cp", "dlb/docs/sub/deeper/c.txt", "."]).await;
    assert_eq!(t.read("c.txt"), b"c");

    // -r: every key under the prefix, keeping the structure below it.
    let text = t.ok(&["cp", "-r", "dlb/docs/", "mirror"]).await;
    assert!(text.ends_with("downloaded 3 objects (8 B)\n"), "{text}");
    assert_eq!(t.read("mirror/a.txt"), b"aaa");
    assert_eq!(t.read("mirror/sub/b.txt"), b"bbbb");
    assert_eq!(t.read("mirror/sub/deeper/c.txt"), b"c");
    assert!(!t.path("mirror/x.txt").exists() && !t.path("mirror/other").exists());
    let v = t.json(&["cp", "-r", "s3://dlb/docs", "mirror2"]).await;
    assert_eq!(v, json!({ "downloaded": 3, "bytes": 8 }));
    assert_eq!(t.read("mirror2/sub/deeper/c.txt"), b"c");
    // The whole bucket.
    assert_eq!(t.json(&["cp", "-r", "dlb", "all"]).await, json!({ "downloaded": 5, "bytes": 12 }));
    assert_eq!(t.read("all/other/x.txt"), b"x");
    assert_eq!(t.read("all/top.txt"), b"top");
    // An empty prefix downloads nothing.
    assert_eq!(t.json(&["cp", "-r", "dlb/nothing/", "none"]).await, json!({ "downloaded": 0, "bytes": 0 }));

    // Errors: no partial file is left behind.
    assert!(t.fail(&["cp", "dlb/docs/missing.txt", "miss.txt"]).await.contains("no such key docs/missing.txt"));
    assert!(!t.path("miss.txt").exists());
    assert!(t.fail(&["cp", "dlb", "x"]).await.contains("use -r for a prefix"));
    assert!(t.fail(&["cp", "nope/a.txt", "x"]).await.contains("no bucket named nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ls_rm_and_stat() {
    let t = start().await;
    t.ok(&["create", "files"]).await;
    assert_eq!(t.ok(&["ls", "files"]).await, "no objects under files/\n");
    assert_eq!(t.json(&["ls", "files"]).await, json!({ "objects": [], "prefixes": [] }));

    for k in ["a.txt", "b.txt", "dir/x", "dir/y", "dir/sub/z", "dirt.txt", "space dir/q"] {
        t.put("files", k, k.as_bytes()).await;
    }

    // One level.
    let v = t.json(&["ls", "files"]).await;
    assert_eq!(keys(&v["objects"]), ["a.txt", "b.txt", "dirt.txt"]);
    assert_eq!(v["prefixes"], json!(["dir/", "space dir/"]));
    let o = &v["objects"][0];
    assert_eq!((o["size"].as_u64(), o["etag"].is_string(), o["last_modified"].is_i64()), (Some(5), true, true));
    let text = t.ok(&["ls", "files"]).await;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    assert!(lines[0].trim_start().starts_with("DIR") && lines[0].ends_with("dir/"), "{text}");
    assert!(lines[1].ends_with("space dir/"), "{text}");
    assert!(lines[2].ends_with("5 B  a.txt"), "{text}");
    assert_eq!(t.ok(&["ls", "s3://files/"]).await, text);

    // A prefix.
    let v = t.json(&["ls", "files/dir/"]).await;
    assert_eq!(keys(&v["objects"]), ["dir/x", "dir/y"]);
    assert_eq!(v["prefixes"], json!(["dir/sub/"]));
    // Without the slash, `dir` is a plain prefix: `dir/` and `dirt.txt`.
    let v = t.json(&["ls", "s3://files/dir"]).await;
    assert_eq!(keys(&v["objects"]), ["dirt.txt"]);
    assert_eq!(v["prefixes"], json!(["dir/"]));
    // -r: every key.
    let v = t.json(&["ls", "-r", "files"]).await;
    assert_eq!(keys(&v["objects"]), ["a.txt", "b.txt", "dir/sub/z", "dir/x", "dir/y", "dirt.txt", "space dir/q"]);
    assert_eq!(v["prefixes"], json!([]));
    let v = t.json(&["ls", "--recursive", "files/dir/"]).await;
    assert_eq!(keys(&v["objects"]), ["dir/sub/z", "dir/x", "dir/y"]);
    assert_eq!(t.ok(&["ls", "files/nothing/"]).await, "no objects under files/nothing/\n");
    assert!(t.fail(&["ls", "nope"]).await.contains("no bucket named nope"));

    // stat
    let v = t.json(&["stat", "files/space dir/q"]).await;
    assert_eq!((v["key"].as_str(), v["size"].as_u64()), (Some("space dir/q"), Some(11)));
    for f in ["metadata", "tags", "cache_control", "checksum", "parts", "etag", "last_modified"] {
        assert!(v.get(f).is_some(), "stat is missing {f}: {v}");
    }
    let r = t
        .http
        .put(format!("{}/v1/storage/buckets/files/upload?key=meta.txt", t.api))
        .header("content-type", "text/plain")
        .header("cache-control", "max-age=60")
        .header("x-amz-meta-owner", "ops")
        .header("x-amz-tagging", "env=prod")
        .body("meta")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let text = t.ok(&["stat", "s3://files/meta.txt"]).await;
    assert!(text.starts_with("files/meta.txt\n"), "{text}");
    for want in ["  size           4 B (4 bytes)", "  etag           ", "  content-type   text/plain", "  last modified  ", "  cache-control  max-age=60", "  meta  owner = ops", "  tag   env = prod"] {
        assert!(text.contains(want), "{want:?} not in\n{text}");
    }
    assert!(t.fail(&["stat", "files/missing"]).await.contains("no such key missing"));
    assert!(t.fail(&["stat", "files"]).await.contains("expected <bucket>/<key>"));
    assert!(t.fail(&["stat", "files/"]).await.contains("expected <bucket>/<key>"));

    // rm one key.
    assert_eq!(t.ok(&["rm", "files/a.txt"]).await, "deleted files/a.txt\n");
    assert!(t.get("files", "a.txt").await.is_none());
    assert_eq!(t.json(&["rm", "s3://files/space dir/q"]).await, json!({ "deleted": "space dir/q" }));
    assert!(t.fail(&["rm", "files"]).await.contains("expected <bucket>/<key>"));
    // rm -r a prefix.
    assert_eq!(t.ok(&["rm", "-r", "files/dir/"]).await, "deleted 3 objects under files/dir/ (recoverable from the trash)\n");
    assert_eq!(keys(&t.json(&["ls", "-r", "files"]).await["objects"]), ["b.txt", "dirt.txt", "meta.txt"]);
    assert_eq!(t.json(&["rm", "-r", "files/nothing/"]).await, json!({ "deleted": 0 }));
    // rm -r of the bucket empties it.
    assert_eq!(t.json(&["rm", "-r", "s3://files"]).await, json!({ "deleted": 3 }));
    assert_eq!(t.json(&["ls", "-r", "files"]).await, json!({ "objects": [], "prefixes": [] }));
    assert!(t.fail(&["rm", "-r", "nope/"]).await.contains("no bucket named nope"));
}

/// The (key, version id, latest, delete marker) of every entry of `ls --versions --json`.
fn versions_of(v: &Value) -> Vec<(String, String, bool, bool)> {
    v["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["key"].as_str().unwrap().to_string(), e["version_id"].as_str().unwrap().to_string(), e["is_latest"].as_bool().unwrap(), e["delete_marker"].as_bool().unwrap()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn versioning_and_versions() {
    let t = start().await;
    t.ok(&["create", "ver"]).await;
    let text = t.ok(&["show", "ver"]).await;
    assert!(text.contains("  versioning off (never enabled)"), "{text}");
    assert!(!text.contains("versions   ") && !text.contains("lock "), "{text}");
    assert_eq!(t.json(&["show", "ver"]).await["versioning"], Value::Null);
    t.put("ver", "before.txt", b"from before").await;

    // On.
    let v = t.json(&["update", "ver", "--versioning", "on"]).await;
    assert_eq!(v["versioning"], "Enabled");
    let text = t.ok(&["show", "ver"]).await;
    assert!(text.contains("  versioning enabled") && text.contains("  versions   0 noncurrent  (0 B)"), "{text}");

    t.put("ver", "k", b"one").await;
    t.put("ver", "k", b"two!").await;
    t.put("ver", "dir/x", b"x").await;
    let v = t.json(&["ls", "--versions", "ver"]).await;
    let all = versions_of(&v);
    assert_eq!(all.iter().map(|e| (e.0.as_str(), e.2, e.3)).collect::<Vec<_>>(), [("before.txt", true, false), ("dir/x", true, false), ("k", true, false), ("k", false, false)]);
    assert_eq!(all[0].1, "null", "objects from before versioning are the null version");
    let (v2, v1) = (all[2].1.clone(), all[3].1.clone());
    assert_ne!(v1, v2);
    let e = &v["versions"][3];
    assert_eq!((e["size"].as_u64(), e["etag"].is_string(), e["last_modified"].is_i64()), (Some(3), true, true));
    // A prefix.
    assert_eq!(versions_of(&t.json(&["ls", "--versions", "ver/k"]).await).len(), 2);
    assert_eq!(versions_of(&t.json(&["ls", "--versions", "s3://ver/dir/"]).await).len(), 1);
    let text = t.ok(&["ls", "--versions", "ver/k"]).await;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "a header and two versions: {text}");
    assert!(lines[0].contains("version id") && lines[0].ends_with("key"), "{text}");
    assert!(lines[1].contains("latest") && lines[1].contains(&v2) && lines[1].contains("4 B"), "{text}");
    assert!(!lines[2].contains("latest") && lines[2].contains(&v1) && lines[2].ends_with(" k"), "{text}");
    assert_eq!(t.ok(&["ls", "--versions", "ver/none/"]).await, "no versions under ver/none/\n");
    let text = t.ok(&["show", "ver"]).await;
    assert!(text.contains("  versions   1 noncurrent  (3 B)"), "{text}");
    let b = t.json(&["show", "ver"]).await;
    assert_eq!((b["versions"].as_u64(), b["version_bytes"].as_u64()), (Some(1), Some(3)));

    // stat and cp of a version.
    let text = t.ok(&["stat", "ver/k"]).await;
    assert!(text.contains(&format!("  version        {v2}\n")) && text.contains("  size           4 B"), "{text}");
    assert!(!text.contains("lock"), "{text}");
    let s1 = t.json(&["stat", "ver/k", "--version-id", &v1]).await;
    assert_eq!((s1["version_id"].as_str(), s1["size"].as_u64(), &s1["lock"]), (Some(v1.as_str()), Some(3), &Value::Null));
    assert_eq!(t.json(&["stat", "ver/before.txt"]).await["version_id"], "null");
    let text = t.ok(&["cp", "s3://ver/k", "old.txt", "--version-id", &v1]).await;
    assert_eq!(text, format!("ver/k (version {v1}) → old.txt  3 B\n"));
    assert_eq!(t.read("old.txt"), b"one");
    let o = t.run(&["cp", "s3://ver/k", "-", "--version-id", &v1]).await;
    assert_eq!((o.code, o.stdout.as_slice()), (Some(0), &b"one"[..]));
    let v = t.json(&["cp", "s3://ver/k", "out/", "--version-id", &v2]).await;
    assert_eq!((v["bytes"].as_u64(), v["version_id"].as_str()), (Some(4), Some(v2.as_str())));
    assert_eq!(t.read("out/k"), b"two!");
    assert!(t.fail(&["cp", "s3://ver/k", "x", "--version-id", &"0".repeat(32)]).await.contains("0000"));
    assert!(t.fail(&["cp", "s3://ver/k", "x", "--version-id", "bad!"]).await.contains("invalid version id"));
    t.write("up.txt", b"up");
    assert!(t.fail(&["cp", "up.txt", "ver/up.txt", "--version-id", &v1]).await.contains("--version-id is for downloads"));
    assert_eq!(t.run(&["cp", "-r", "s3://ver/", "dir", "--version-id", &v1]).await.code, Some(2));

    // rm without a version id adds a delete marker; the versions stay.
    let text = t.ok(&["rm", "ver/k"]).await;
    assert!(text.starts_with("deleted ver/k (delete marker ") && text.ends_with("; older versions are kept)\n"), "{text}");
    let all = versions_of(&t.json(&["ls", "--versions", "ver/k"]).await);
    assert_eq!(all.iter().map(|e| (e.2, e.3)).collect::<Vec<_>>(), [(true, true), (false, false), (false, false)]);
    let marker = all[0].1.clone();
    assert!(t.ok(&["ls", "--versions", "ver/k"]).await.lines().nth(1).unwrap().contains("latest, delete marker"));
    assert!(t.get("ver", "k").await.is_none());
    assert!(t.fail(&["stat", "ver/k"]).await.contains("delete marker"));
    // Removing the marker brings the object back.
    let v = t.json(&["rm", "ver/k", "--version-id", &marker]).await;
    assert_eq!(v, json!({ "deleted": "k", "version_id": marker, "delete_marker": true }));
    assert_eq!(t.get("ver", "k").await.unwrap(), b"two!");
    // A version deleted for good goes to the trash.
    let text = t.ok(&["rm", "s3://ver/k", "--version-id", &v1]).await;
    assert_eq!(text, format!("deleted version {v1} of ver/k for good (recoverable from the trash)\n"));
    assert_eq!(versions_of(&t.json(&["ls", "--versions", "ver/k"]).await).len(), 1);
    let trash = t.json(&["trash", "ver"]).await;
    assert_eq!((trash[0]["key"].as_str(), trash[0]["version_id"].as_str()), (Some("k"), Some(v1.as_str())));
    assert!(t.fail(&["rm", "ver/k", "--version-id", &v1]).await.contains(&v1), "already gone");
    assert!(t.fail(&["rm", "ver/k", "--version-id", "nope!"]).await.contains("invalid version id"));
    assert_eq!(t.run(&["rm", "-r", "ver/", "--version-id", &v2]).await.code, Some(2));
    assert!(t.fail(&["ls", "--versions"]).await.contains("--versions lists a bucket"));

    // Off is Suspended: it can't go back to never versioned.
    let v = t.json(&["update", "ver", "--versioning", "off"]).await;
    assert_eq!(v["versioning"], "Suspended");
    assert!(t.ok(&["show", "ver"]).await.contains("  versioning suspended"));
    assert_eq!(t.run(&["update", "ver", "--versioning", "maybe"]).await.code, Some(2));

    // An unversioned bucket's objects are all `null`.
    t.ok(&["create", "plain"]).await;
    t.put("plain", "p", b"p").await;
    let all = versions_of(&t.json(&["ls", "--versions", "plain"]).await);
    assert_eq!(all, [("p".to_string(), "null".to_string(), true, false)]);
    assert_eq!(t.json(&["rm", "plain/p"]).await, json!({ "deleted": "p" }), "versions are only mentioned where there are any");
}

#[tokio::test(flavor = "multi_thread")]
async fn object_lock_lifecycle_and_website() {
    use aws_sdk_s3::types::{DefaultRetention, IndexDocument, ErrorDocument, ObjectLockConfiguration, ObjectLockEnabled, ObjectLockRetentionMode, ObjectLockRule, WebsiteConfiguration};
    let t = start().await;
    let b = t.json(&["create", "locked", "--object-lock"]).await;
    assert_eq!((b["versioning"].as_str(), b["object_lock"]["mode"].clone()), (Some("Enabled"), Value::Null));
    assert!(t.ok(&["show", "locked"]).await.contains("  lock       on, no default retention"));
    let k = t.store.create_key("admin", s3_store::Permission::Admin, None).await.unwrap();
    let s3 = t.sdk(&k.id, &k.secret);
    let rule = ObjectLockRule::builder().default_retention(DefaultRetention::builder().mode(ObjectLockRetentionMode::Governance).days(1).build()).build();
    s3.put_object_lock_configuration()
        .bucket("locked")
        .object_lock_configuration(ObjectLockConfiguration::builder().object_lock_enabled(ObjectLockEnabled::Enabled).rule(rule).build())
        .send()
        .await
        .unwrap();
    let text = t.ok(&["show", "locked"]).await;
    assert!(text.contains("  lock       on, new versions kept 1 day (GOVERNANCE)"), "{text}");
    assert!(text.contains("  versioning enabled"), "{text}");
    let b = t.json(&["show", "locked"]).await;
    assert_eq!(b["object_lock"], json!({ "mode": "GOVERNANCE", "days": 1, "years": null }));
    t.put("locked", "doc", b"keep me").await;
    let text = t.ok(&["stat", "locked/doc"]).await;
    assert!(text.contains("  lock           GOVERNANCE until "), "{text}");
    let v = t.json(&["stat", "locked/doc"]).await;
    assert_eq!(v["lock"]["mode"], "GOVERNANCE");
    assert!(v["lock"]["retain_until"].as_i64().unwrap() > now_ms());
    let vid = v["version_id"].as_str().unwrap().to_string();
    let e = t.fail(&["rm", "locked/doc", "--version-id", &vid]).await;
    assert!(e.to_lowercase().contains("lock"), "{e}");
    assert!(t.fail(&["update", "locked", "--versioning", "off"]).await.to_lowercase().contains("object lock"));

    // Lifecycle rules with every kind of action and filter.
    let lifecycle = json!([
        { "id": "logs", "prefix": "logs/", "enabled": true, "expire_days": 30, "abort_multipart_days": null,
          "tags": { "tier": "cold" }, "size_greater_than": 1024, "size_less_than": 1048576 },
        { "id": "versions", "prefix": "", "enabled": false, "expire_days": null, "abort_multipart_days": null,
          "noncurrent_days": 10, "newer_noncurrent_versions": 2, "expire_delete_markers": true },
        { "id": "dated", "prefix": "tmp/", "enabled": true, "expire_days": null, "abort_multipart_days": 7,
          "expire_at": 1798761600000i64, "transitions": [{ "days": 30, "date": null, "storage_class": "GLACIER" }],
          "noncurrent_transitions": [{ "noncurrent_days": 5, "newer_noncurrent_versions": null, "storage_class": "STANDARD_IA" }] },
    ]);
    t.ok(&["create", "rules"]).await;
    let r = t.http.patch(format!("{}/v1/storage/buckets/rules", t.api)).json(&json!({ "lifecycle": lifecycle })).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let text = t.ok(&["show", "rules"]).await;
    for want in [
        "  lifecycle  logs: prefix \"logs/\", tag tier=cold, size > 1.0 KB, size < 1.0 MB → expire after 30d",
        "  lifecycle  versions: prefix \"\" → remove lone delete markers, delete versions noncurrent for 10d (keep the newest 2) (disabled)",
        "  lifecycle  dated: prefix \"tmp/\" → expire on 2027-01-01, transition to GLACIER after 30d (stored only), noncurrent to STANDARD_IA after 5d (stored only), abort uploads after 7d",
    ] {
        assert!(text.contains(want), "{want:?} not in\n{text}");
    }
    let v = t.json(&["show", "rules"]).await;
    assert_eq!(v["lifecycle"][1]["noncurrent_days"], 10);
    assert_eq!(v["lifecycle"][2]["transitions"][0]["storage_class"], "GLACIER");

    // Website and tags.
    s3.put_bucket_website()
        .bucket("rules")
        .website_configuration(
            WebsiteConfiguration::builder()
                .index_document(IndexDocument::builder().suffix("index.html").build().unwrap())
                .error_document(ErrorDocument::builder().key("404.html").build().unwrap())
                .build(),
        )
        .send()
        .await
        .unwrap();
    t.ok(&["update", "rules", "--tag", "team=web"]).await;
    let text = t.ok(&["show", "rules"]).await;
    assert!(text.contains("  website    index index.html, error 404.html"), "{text}");
    assert!(text.contains("  tag        team = web"), "{text}");
    let v = t.json(&["show", "rules"]).await;
    assert_eq!((v["website"]["index_document"].as_str(), v["tags"]["team"].as_str()), (Some("index.html"), Some("web")));
}

#[tokio::test(flavor = "multi_thread")]
async fn listings_page_past_1000() {
    let t = start().await;
    t.ok(&["create", "many"]).await;
    let n = 1_005;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        let (http, api) = (t.http.clone(), t.api.clone());
        set.spawn(async move {
            let r = http.put(format!("{api}/v1/storage/buckets/many/upload?key=k/{i:05}")).body(vec![b'x'; 3]).send().await.unwrap();
            assert!(r.status().is_success());
        });
        if set.len() >= 64 {
            set.join_next().await.unwrap().unwrap();
        }
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }

    let v = t.json(&["ls", "-r", "many"]).await;
    let all = keys(&v["objects"]);
    assert_eq!(all.len(), n);
    assert_eq!((all[0].as_str(), all[n - 1].as_str()), ("k/00000", "k/01004"));
    assert!(all.windows(2).all(|w| w[0] < w[1]));
    let v = t.json(&["ls", "many/k/"]).await;
    assert_eq!(v["objects"].as_array().unwrap().len(), n);

    assert_eq!(t.json(&["rm", "-r", "many/k/"]).await, json!({ "deleted": n }));
    let trash = t.json(&["trash", "many"]).await;
    assert_eq!(trash.as_array().unwrap().len(), n);
    let tk = keys(&trash);
    assert!(tk.windows(2).all(|w| w[0] < w[1]), "trash keys are unique and sorted");
}

#[tokio::test(flavor = "multi_thread")]
async fn presigned_urls() {
    let t = start().await;
    t.ok(&["create", "share"]).await;
    let data = pattern(70_000);
    t.put("share", "dir/hello world+ü.bin", &data).await;

    // GET: the URL on stdout, what it is on stderr.
    let o = t.run(&["presign", "share/dir/hello world+ü.bin"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    let url = o.text().trim().to_string();
    assert!(url.starts_with(&format!("{}/share/dir/hello%20world%2B%C3%BC.bin?", t.s3)), "{url}");
    assert!(url.contains("X-Amz-Expires=3600"), "{url}");
    assert!(o.stderr.starts_with("GET, valid until "), "{}", o.stderr);
    let r = t.http.get(&url).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(r.bytes().await.unwrap().to_vec(), data);

    // --json, and the expiry.
    let before = now_ms();
    let v = t.json(&["presign", "s3://share/dir/hello world+ü.bin", "--expires", "15m"]).await;
    assert_eq!(v["method"], "GET");
    let at = v["expires_at"].as_i64().unwrap();
    assert!((before + 899_000..=now_ms() + 901_000).contains(&at), "{at} vs {before}");
    assert!(v["url"].as_str().unwrap().contains("X-Amz-Expires=900"));
    for (arg, secs) in [("90s", 90), ("90", 90), ("1h", 3600), ("7d", 604_800), ("1s", 1)] {
        let v = t.json(&["presign", "share/x", "--expires", arg]).await;
        assert!(v["url"].as_str().unwrap().contains(&format!("X-Amz-Expires={secs}&")) || v["url"].as_str().unwrap().ends_with(&format!("X-Amz-Expires={secs}")), "{arg}: {v}");
    }

    // PUT, then actually upload through it, with any Content-Type.
    let o = t.run(&["presign", "share/up/new.json", "--put", "--expires", "60s"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stderr.starts_with("PUT, valid until "), "{}", o.stderr);
    let put_url = o.text().trim().to_string();
    let r = t.http.put(&put_url).header("content-type", "application/json").body(r#"{"ok":true}"#).send().await.unwrap();
    assert!(r.status().is_success(), "{}: {}", r.status(), r.text().await.unwrap());
    assert_eq!(t.run(&["cp", "share/up/new.json", "-"]).await.stdout, br#"{"ok":true}"#);
    assert_eq!(t.json(&["stat", "share/up/new.json"]).await["content_type"], "application/json");
    let v = t.json(&["presign", "share/up/2.bin", "--put"]).await;
    assert_eq!(v["method"], "PUT");
    let big = pattern(1_500_000);
    let r = t.http.put(v["url"].as_str().unwrap()).body(big.clone()).send().await.unwrap();
    assert!(r.status().is_success());
    assert_eq!(t.get("share", "up/2.bin").await.unwrap(), big);
    // A GET URL can't upload.
    let get_url = t.json(&["presign", "share/up/2.bin"]).await["url"].as_str().unwrap().to_string();
    assert!(!t.http.put(&get_url).body("x").send().await.unwrap().status().is_success());

    // --expires parsing and range, checked before calling the API.
    for bad in ["0", "0s", "8d", "604801", "999999999999999d", "99999999999999999999"] {
        let e = t.run_at("http://127.0.0.1:1", &["storage", "presign", "share/x", "--expires", bad], None).await;
        assert_eq!(e.code, Some(1), "{bad}");
        assert!(e.stderr.contains("--expires must be between 1s and 7d") || e.stderr.contains("bad duration"), "{bad}: {}", e.stderr);
    }
    for bad in ["1w", "h", "", "-5m", "1.5h", "10 m"] {
        let e = t.fail(&["presign", "share/x", &format!("--expires={bad}")]).await;
        assert!(e.contains("bad duration"), "{bad}: {e}");
    }
    assert!(t.fail(&["presign", "share"]).await.contains("expected <bucket>/<key>"));
    assert!(t.fail(&["presign", "nope/x"]).await.contains("no bucket named nope"));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test(flavor = "multi_thread")]
async fn trash_and_restore() {
    let t = start().await;
    t.ok(&["create", "bin"]).await;
    assert_eq!(t.ok(&["trash", "bin"]).await, "the trash of bin is empty\n");
    assert_eq!(t.json(&["trash", "bin"]).await, json!([]));

    t.put("bin", "notes.txt", b"v1").await;
    t.put("bin", "notes.txt", b"v2 longer").await; // the overwritten v1 goes to the trash
    t.put("bin", "logs/a", b"aa").await;
    t.put("bin", "logs/b", b"bbb").await;
    t.ok(&["rm", "bin/notes.txt"]).await;
    t.ok(&["rm", "-r", "bin/logs/"]).await;

    let entries = t.json(&["trash", "bin"]).await;
    let list = entries.as_array().unwrap();
    assert_eq!(keys(&entries), ["logs/a", "logs/b", "notes.txt", "notes.txt"]);
    for e in list {
        let (d, x) = (e["deleted_at"].as_i64().unwrap(), e["expires_at"].as_i64().unwrap());
        assert!(x > d && e["etag"].is_string() && e["size"].is_u64(), "{e}");
    }
    let prefixed = t.json(&["trash", "bin", "--prefix", "logs/"]).await;
    assert_eq!(keys(&prefixed), ["logs/a", "logs/b"]);
    assert_eq!(t.json(&["trash", "bin", "--prefix", "zzz"]).await, json!([]));

    let text = t.ok(&["trash", "bin"]).await;
    assert!(text.lines().next().unwrap().contains("deleted_at"), "{text}");
    assert!(text.contains(&list[0]["deleted_at"].to_string()), "{text}");
    assert!(text.ends_with("restore with `roll storage restore bin/<key> --at <deleted_at>`\n"), "{text}");

    // Restore the newest notes.txt (v2), by its deleted_at.
    let v2 = list.iter().filter(|e| e["key"] == "notes.txt").max_by_key(|e| e["deleted_at"].as_i64()).unwrap();
    let at = v2["deleted_at"].to_string();
    let v = t.json(&["restore", "bin/notes.txt", "--at", &at]).await;
    assert_eq!((v["key"].as_str(), v["size"].as_u64()), (Some("notes.txt"), Some(9)));
    assert_eq!(t.get("bin", "notes.txt").await.unwrap(), b"v2 longer");
    // The older version, in text.
    let v1 = list.iter().filter(|e| e["key"] == "notes.txt").min_by_key(|e| e["deleted_at"].as_i64()).unwrap();
    let text = t.ok(&["restore", "s3://bin/notes.txt", "--at", &v1["deleted_at"].to_string()]).await;
    assert_eq!(text, "restored bin/notes.txt  2 B\n");
    assert_eq!(t.get("bin", "notes.txt").await.unwrap(), b"v1");
    let logs_a = &list[0];
    t.ok(&["restore", "bin/logs/a", "--at", &logs_a["deleted_at"].to_string()]).await;
    assert_eq!(t.get("bin", "logs/a").await.unwrap(), b"aa");

    // Restored entries leave the trash.
    let at_a = logs_a["deleted_at"].to_string();
    assert!(t.fail(&["restore", "bin/logs/a", "--at", &at_a]).await.contains(&format!("logs/a deleted at {at_a} isn't in the trash")));
    assert!(t.fail(&["restore", "bin/logs/b", "--at", "1"]).await.contains("isn't in the trash"));
    assert_eq!(t.run(&["restore", "bin/logs/b"]).await.code, Some(2), "--at is required");
    assert_eq!(t.run(&["restore", "bin/logs/b", "--at", "yesterday"]).await.code, Some(2));
    assert!(t.fail(&["restore", "bin", "--at", "1"]).await.contains("expected <bucket>/<key>"));
    assert!(t.fail(&["trash", "nope"]).await.contains("no bucket named nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_one_and_all() {
    let t = start().await;
    t.ok(&["create", "metered"]).await;
    t.ok(&["create", "quiet"]).await;
    t.write("f.bin", &[7u8; 1000]);
    t.ok(&["cp", "f.bin", "metered/f.bin"]).await;
    t.ok(&["cp", "f.bin", "metered/g.bin"]).await;
    t.ok(&["cp", "metered/f.bin", "back.bin"]).await;
    t.store.flush_meter().unwrap();

    let u = t.json(&["usage", "metered"]).await;
    assert_eq!((u["bucket"].as_str(), u["days"].as_i64()), (Some("metered"), Some(30)));
    assert_eq!((u["class_a"].as_u64(), u["class_b"].as_u64()), (Some(2), Some(1)));
    assert_eq!(u["bytes_in"], 2000);
    assert_eq!(u["egress_internal_bytes"], 1000);
    assert_eq!(u["egress_internet_bytes"], 0);
    assert_eq!(u["storage_bytes"], 2000);
    for f in ["storage_usd", "class_a_usd", "class_b_usd", "egress_usd", "total_usd", "projected_month_usd"] {
        assert!(u["cost"][f].is_number(), "{f}: {u}");
    }
    assert_eq!(t.json(&["usage", "metered", "--days", "7"]).await["days"], 7);

    let text = t.ok(&["usage", "metered", "--days", "7"]).await;
    assert!(text.starts_with("metered — last 7 days\n"), "{text}");
    for want in ["  storage   ", "  class A            2", "  class B            1", "  internal  ", "  uploaded  ", "  total ", "  projected / month "] {
        assert!(text.contains(want), "{want:?} not in\n{text}");
    }
    assert!(text.contains("2.0 KB"), "{text}");

    let all = t.json(&["usage"]).await;
    assert_eq!(all["days"], 30);
    let mut names: Vec<&str> = all["buckets"].as_array().unwrap().iter().map(|b| b["bucket"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["metered", "quiet"]);
    assert!(all["cost"]["total_usd"].is_number() && all["daily"].is_array());
    let text = t.ok(&["usage", "--days", "3"]).await;
    assert!(text.starts_with("last 3 days\n"), "{text}");
    assert!(text.lines().nth(1).unwrap().contains("gb-months"), "{text}");
    assert!(text.lines().any(|l| l.starts_with("metered")) && text.lines().any(|l| l.starts_with("quiet")), "{text}");
    assert!(text.lines().last().unwrap().starts_with("total $"), "{text}");

    assert!(t.fail(&["usage", "nope"]).await.contains("no bucket named nope"));
    assert_eq!(t.run(&["usage", "--days", "x"]).await.code, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn access_keys() {
    let t = start().await;
    t.ok(&["create", "team-a"]).await;
    t.ok(&["create", "team-b"]).await;
    t.put("team-a", "a.txt", b"for a").await;
    t.put("team-b", "b.txt", b"for b").await;

    // No keys yet (the control plane's own key is never listed): just the header.
    assert_eq!(t.json(&["keys"]).await, json!([]));
    assert_eq!(t.ok(&["keys"]).await.lines().count(), 1);

    // A read key for one bucket, as JSON.
    let k = t.json(&["keys", "create", "ci", "--permission", "read", "--bucket", "team-a"]).await;
    let (id, secret) = (k["id"].as_str().unwrap().to_string(), k["secret_access_key"].as_str().unwrap().to_string());
    assert!(id.starts_with("RK") && id.len() == 20, "{id}");
    assert_eq!(secret.len(), 40);
    assert_eq!((k["name"].as_str(), k["permission"].as_str()), (Some("ci"), Some("read")));
    assert_eq!(k["buckets"], json!(["team-a"]));
    let s3 = t.sdk(&id, &secret);
    let got = s3.get_object().bucket("team-a").key("a.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"for a");
    assert!(s3.get_object().bucket("team-b").key("b.txt").send().await.is_err(), "limited to team-a");
    assert!(s3.put_object().bucket("team-a").key("w").body(b"x".to_vec().into()).send().await.is_err(), "read-only");

    // Text output: the env vars to use it, secret once; the default permission is write, for
    // every bucket.
    let o = t.run(&["keys", "create", "deploy"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stderr.contains("the secret is shown only once"), "{}", o.stderr);
    let text = o.text();
    assert!(text.starts_with("created write key RK"), "{text}");
    let env = |name: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{name}="))).unwrap_or_else(|| panic!("{name} in {text}")).to_string();
    assert_eq!(env("AWS_ENDPOINT_URL"), t.s3);
    assert_eq!(env("AWS_REGION"), "us-east-1");
    let (wid, wsecret) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY"));
    let ws = t.sdk(&wid, &wsecret);
    ws.put_object().bucket("team-b").key("from-sdk.txt").body(b"sdk".to_vec().into()).send().await.unwrap();
    assert_eq!(t.run(&["cp", "team-b/from-sdk.txt", "-"]).await.stdout, b"sdk");
    assert!(ws.create_bucket().bucket("not-allowed").send().await.is_err(), "write keys can't create buckets");

    // Admin, several buckets.
    let a = t.json(&["keys", "create", "root", "--permission", "admin", "--bucket", "team-a", "--bucket", "team-b"]).await;
    assert_eq!((a["permission"].as_str(), a["buckets"].clone()), (Some("admin"), json!(["team-a", "team-b"])));
    let a_all = t.json(&["keys", "create", "root-all", "--permission", "admin"]).await;
    assert_eq!(a_all["buckets"], Value::Null);
    t.sdk(a_all["id"].as_str().unwrap(), a_all["secret_access_key"].as_str().unwrap()).create_bucket().bucket("made-by-sdk").send().await.unwrap();
    assert!(t.json(&["buckets"]).await.as_array().unwrap().iter().any(|b| b["name"] == "made-by-sdk"));

    // Listed without secrets.
    let list = t.json(&["keys"]).await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert!(list.as_array().unwrap().iter().all(|k| k.get("secret_access_key").is_none() && k.get("secret").is_none()));
    let ci = list.as_array().unwrap().iter().find(|k| k["id"] == id.as_str()).unwrap();
    assert!(ci["last_used_at"].is_i64(), "the key was used: {ci}");
    let text = t.ok(&["keys"]).await;
    let header = text.lines().next().unwrap();
    assert!(header.contains("access key id") && header.contains("last used"), "{header}");
    let row = text.lines().find(|l| l.starts_with(&id)).unwrap_or_else(|| panic!("{text}"));
    assert!(row.contains("ci") && row.contains("read") && row.contains("team-a"), "{row}");
    let row = text.lines().find(|l| l.starts_with(&wid)).unwrap();
    assert!(row.contains("deploy") && row.contains("write") && row.contains("all"), "{row}");
    assert!(!text.contains(&secret) && !text.contains(&wsecret));

    // Errors.
    assert_eq!(t.run(&["keys", "create", "x", "--permission", "owner"]).await.code, Some(2));
    assert!(!t.fail(&["keys", "create", ""]).await.is_empty());
    assert!(!t.fail(&["keys", "create", "x", "--bucket", "Not A Bucket"]).await.is_empty());
    assert!(t.fail(&["keys", "delete", "RKNOSUCHKEY000000000"]).await.contains("RKNOSUCHKEY000000000"));
    assert!(!t.fail(&["keys", "delete", s3_control::control::CONTROL_KEY_ID]).await.is_empty());

    // Delete: the key stops working.
    assert_eq!(t.ok(&["keys", "delete", &id]).await, format!("deleted key {id}\n"));
    assert!(s3.get_object().bucket("team-a").key("a.txt").send().await.is_err(), "deleted keys don't work");
    assert_eq!(t.json(&["keys", "delete", &wid]).await, json!({ "deleted": wid }));
    assert_eq!(t.json(&["keys"]).await.as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_or_wrong_api() {
    let t = start().await;
    t.write("f.txt", b"x");
    // Nothing listens on port 1: a clear message for JSON calls, uploads and downloads alike.
    let dead = "http://127.0.0.1:1";
    for args in [
        vec!["storage", "info"],
        vec!["storage", "buckets", "--json"],
        vec!["storage", "cp", "f.txt", "b/f.txt"],
        vec!["storage", "cp", "s3://b/f.txt", "out.txt"],
        vec!["storage", "keys", "create", "x"],
    ] {
        let o = t.run_at(dead, &args, None).await;
        assert_eq!(o.code, Some(1), "{args:?}");
        assert!(o.stderr.contains("can't reach http://127.0.0.1:1 (is it running?)"), "{args:?}: {}", o.stderr);
        assert!(o.stdout.is_empty());
    }
    let o = t.run_at(dead, &["storage", "cp", "-", "b/k"], Some(b"data".to_vec())).await;
    assert!(o.stderr.contains("can't reach"), "{}", o.stderr);
    assert!(!t.path("out.txt").exists());

    // Pointed at the S3 endpoint by mistake: its errors are XML, not `{error}`. Say what came
    // back rather than failing to parse it.
    let o = t.run_at(&t.s3, &["storage", "show", "x"], None).await;
    assert_eq!(o.code, Some(1));
    assert!(!o.stderr.contains("expected value at line"), "{}", o.stderr);
    assert!(o.stderr.contains("40"), "the HTTP status: {}", o.stderr);
    let o = t.run_at(&t.s3, &["storage", "cp", "s3://x/y", "-"], None).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.contains("40"), "{}", o.stderr);
    // A server that isn't roll at all (empty 404s).
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, axum::Router::new()).await.unwrap() });
    let o = t.run_at(&other, &["storage", "buckets"], None).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.contains("404 Not Found"), "{}", o.stderr);
    let o = t.run_at(&other, &["storage", "cp", "f.txt", "b/f.txt"], None).await;
    assert!(o.stderr.contains("404 Not Found"), "{}", o.stderr);
    // A non-JSON success body isn't silently accepted.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let html = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, axum::Router::new().fallback(|| async { "<html>hi</html>" })).await.unwrap() });
    let o = t.run_at(&html, &["storage", "info"], None).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.contains("didn't answer with JSON"), "{}", o.stderr);
}

#[test]
fn helpers() {
    assert_eq!(enc("a b+c/ü"), "a%20b%2Bc%2F%C3%BC");
    assert_ne!(pattern(64)[..32], pattern(64)[32..]);
}

#[tokio::test]
async fn roles_and_sessions() {
    let t = start().await;
    t.ok(&["create", "uploads"]).await;
    t.ok(&["create", "private"]).await;
    let k = t.json(&["keys", "create", "backend", "--permission", "read"]).await;
    let key_id = k["id"].as_str().unwrap().to_string();

    // No roles yet: just the header.
    assert_eq!(t.json(&["roles"]).await, json!([]));
    let text = t.ok(&["roles", "create", "web", "--permission", "write", "--bucket", "uploads", "--bucket", "private", "--trust-key", &key_id, "--max-session", "2h", "--description", "browser uploads"]).await;
    let arn = format!("arn:aws:iam::{}:role/web", t.store.account_id());
    assert_eq!(text.trim(), format!("created role {arn}"));
    std::fs::write(t.path("policy.json"), r#"{"Statement":[{"Effect":"Allow","Action":"s3:GetObject","Resource":"*"}]}"#).unwrap();
    let g = t
        .json(&["roles", "create", "google", "--oidc-issuer", "https://accounts.google.com", "--audience", "123.apps.googleusercontent.com", "--subject", "1*", "--policy", "@policy.json"])
        .await;
    assert_eq!(g["trust"]["oidc"][0]["issuer"], "https://accounts.google.com");
    assert_eq!(g["policy"]["Statement"][0]["Action"], "s3:GetObject");
    assert_eq!(g["permission"], "read");
    let list = t.ok(&["roles"]).await;
    assert!(list.lines().next().unwrap().contains("trusted"), "{list}");
    assert!(list.contains(&key_id) && list.contains("oidc:https://accounts.google.com"), "{list}");
    let show = t.ok(&["roles", "show", "web"]).await;
    assert!(show.contains(&arn) && show.contains("uploads,private") && show.contains("7200s") && show.contains("browser uploads"), "{show}");

    // Credentials for a backend to hand out: env vars on stdout only.
    let o = t.run(&["sessions", "create", "web", "--duration", "15m", "--bucket", "uploads", "--name", "user-7", "--tag", "user=7"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stderr.contains("assumed-role/web/user-7"), "{}", o.stderr);
    let text = o.text();
    assert_eq!(text.lines().count(), 5, "{text}");
    let env = |name: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{name}="))).unwrap_or_else(|| panic!("{name} in {text}")).to_string();
    assert!(env("AWS_ACCESS_KEY_ID").starts_with("RT"));
    assert_eq!(env("AWS_ENDPOINT_URL"), t.s3);
    let creds = Credentials::new(env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY"), Some(env("AWS_SESSION_TOKEN")), None, "cli");
    let conf = aws_sdk_s3::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new("us-east-1")).endpoint_url(&t.s3).credentials_provider(creds).force_path_style(true).build();
    let s3 = aws_sdk_s3::Client::from_conf(conf);
    s3.put_object().bucket("uploads").key("from-session.txt").body(b"temp".to_vec().into()).send().await.unwrap();
    assert_eq!(t.get("uploads", "from-session.txt").await.unwrap(), b"temp");
    assert!(s3.put_object().bucket("private").key("x").body(b"x".to_vec().into()).send().await.is_err(), "narrowed to --bucket uploads");

    // JSON, a read-only narrowing, and errors.
    let j = t.json(&["sessions", "create", "web", "--permission", "read"]).await;
    assert_eq!(j["permission"], "read");
    assert_eq!(j["env"]["AWS_SESSION_TOKEN"], j["session_token"]);
    assert!(t.fail(&["sessions", "create", "web", "--permission", "admin"]).await.contains("more than the role"));
    assert!(t.fail(&["sessions", "create", "web", "--duration", "3h"]).await.contains("duration_s"));
    assert!(t.fail(&["sessions", "create", "nope"]).await.contains("no role"));
    assert!(t.fail(&["sessions", "create", "web", "--tag", "novalue"]).await.contains("key=value"));
    assert!(t.fail(&["roles", "create", "bad", "--oidc-issuer", "https://x"]).await.contains("--audience"));
    assert!(t.fail(&["roles", "create", "bad", "--policy", "{not json"]).await.contains("JSON"));
    assert!(t.fail(&["roles", "update", "web"]).await.contains("nothing to change"));

    // Updating, revoking and deleting end the sessions.
    let updated = t.ok(&["roles", "update", "web", "--all-buckets", "--max-session", "1h"]).await;
    assert!(updated.contains("no longer work"), "{updated}");
    let err = s3.put_object().bucket("uploads").key("y").body(b"y".to_vec().into()).send().await.unwrap_err();
    assert!(format!("{err:?}").contains("InvalidToken"), "{err:?}");
    let r = t.json(&["roles", "show", "web"]).await;
    assert_eq!((r["buckets"].clone(), r["max_session_s"].as_i64(), r["generation"].as_i64()), (Value::Null, Some(3600), Some(2)));
    assert!(t.ok(&["roles", "revoke", "web"]).await.contains("revoked"));
    assert!(t.ok(&["sessions", "rotate-key"]).await.contains("sealed with key"));
    assert_eq!(t.ok(&["roles", "delete", "google"]).await.trim(), "deleted role google");
    assert!(t.fail(&["roles", "show", "google"]).await.contains("no role"));
    assert_eq!(t.json(&["roles"]).await.as_array().unwrap().len(), 1);
}

/// What a webhook receiver got: each delivery's headers and body.
type Received = Arc<std::sync::Mutex<Vec<(axum::http::HeaderMap, Vec<u8>)>>>;

/// A webhook on loopback that records every POST to `/hook`; its URL.
async fn webhook() -> (String, Received) {
    let got: Received = Default::default();
    let sink = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push((headers, body.to_vec()));
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/hook", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

/// Wait (up to 10 s) for `f` to hold.
async fn eventually<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..200 {
        if f().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The S3 event records a receiver got, as `(eventName, key)`.
fn records(got: &Received) -> Vec<(String, String)> {
    got.lock()
        .unwrap()
        .iter()
        .filter_map(|(_, b)| serde_json::from_slice::<Value>(b).ok())
        .flat_map(|v| v["Records"].as_array().cloned().unwrap_or_default())
        .map(|r| (r["eventName"].as_str().unwrap_or("").to_string(), r["s3"]["object"]["key"].as_str().unwrap_or("").to_string()))
        .collect()
}

#[tokio::test]
async fn notifications_to_a_webhook() {
    let t = start().await;
    t.ok(&["create", "events"]).await;
    assert!(t.ok(&["notify", "events"]).await.contains("no notifications on events"));
    let (url, got) = webhook().await;

    // Added with a friendly destination; S3's validation sends it a test event first.
    let out = t.ok(&["notify", "add", "events", "--to", &url, "--prefix", "img/", "--id", "hook"]).await;
    assert!(out.contains(&format!("s3:ObjectCreated:* img/* → {url} (id hook)")), "{out}");
    assert!(got.lock().unwrap().iter().any(|(_, b)| String::from_utf8_lossy(b).contains("s3:TestEvent")), "the test event");
    let n = t.json(&["notify", "events"]).await;
    assert_eq!(n["configurations"][0]["destination"], format!("arn:roll:webhook:{url}"));
    assert_eq!(n["configurations"][0]["type"], "topic");
    let listed = t.ok(&["notify", "events"]).await;
    assert!(listed.contains("hook") && listed.contains(&url) && listed.contains("img/*"), "{listed}");
    let secret = t.ok(&["notify", "secret", "events"]).await.trim().to_string();
    assert!(secret.starts_with("whsec_"), "{secret}");

    // A real event: only keys under the prefix, signed with the bucket's secret.
    t.put("events", "doc/skip.txt", b"no").await;
    t.put("events", "img/cat.png", b"meow").await;
    eventually("the ObjectCreated event", || async { records(&got).iter().any(|(_, k)| k == "img/cat.png") }).await;
    let rs = records(&got);
    assert!(rs.contains(&("ObjectCreated:Put".to_string(), "img/cat.png".to_string())), "{rs:?}");
    assert!(!rs.iter().any(|(_, k)| k == "doc/skip.txt"), "outside the filter: {rs:?}");
    {
        let all = got.lock().unwrap();
        let (h, body) = all.iter().find(|(_, b)| String::from_utf8_lossy(b).contains("img/cat.png")).unwrap();
        let sig = h["x-roll-signature"].to_str().unwrap();
        let ts: i64 = sig.strip_prefix("t=").and_then(|r| r.split(',').next()).unwrap().parse().unwrap();
        assert_eq!(sig, s3_server::notify::webhook_signature(&secret, ts, body));
        assert_eq!(h["x-roll-event"].to_str().unwrap(), "s3:ObjectCreated:Put");
    }

    // Rotating the secret changes it and keeps the destination.
    let rotated = t.ok(&["notify", "secret", "events", "--rotate"]).await.trim().to_string();
    assert!(rotated.starts_with("whsec_") && rotated != secret);
    assert_eq!(t.json(&["notify", "events"]).await["configurations"].as_array().unwrap().len(), 1);

    // Other destinations: a queue (refused: no roll queues in this test), forms that aren't
    // destinations, bad events, and a stream added without validation.
    let e = t.fail(&["notify", "add", "events", "--to", "queue:orders", "--events", "s3:ObjectRemoved:*"]).await;
    assert!(e.contains("arn:roll:queues:%2F:orders"), "{e}");
    let e = t.fail(&["notify", "add", "events", "--to", "ftp://example.com"]).await;
    assert!(e.contains("expected queue:<vhost>/<queue>"), "{e}");
    let e = t.fail(&["notify", "add", "events", "--to", "stream:s/t", "--events", "s3:Nope"]).await;
    assert!(e.contains("s3:Nope"), "{e}");
    t.ok(&["notify", "add", "events", "--to", "stream:events/uploads", "--events", "s3:ObjectRemoved:*,s3:ObjectTagging:*", "--skip-validation", "--id", "removed"]).await;
    let n = t.json(&["notify", "events"]).await;
    assert_eq!(n["configurations"][1]["destination"], "arn:roll:streams:events:uploads");
    assert_eq!(n["configurations"][1]["events"], json!(["s3:ObjectRemoved:*", "s3:ObjectTagging:*"]));
    let show = t.ok(&["show", "events"]).await;
    assert!(show.contains(&format!("notify     hook: s3:ObjectCreated:* img/* → {url}")), "{show}");
    assert!(show.contains("notify     removed: s3:ObjectRemoved:*,s3:ObjectTagging:* * → stream:events/uploads"), "{show}");

    // Removing one keeps the other; an unknown id is an error.
    assert!(t.ok(&["notify", "rm", "events", "removed"]).await.contains("removed notification removed"));
    assert!(t.fail(&["notify", "rm", "events", "removed"]).await.contains("no notification removed"));
    t.ok(&["notify", "rm", "events", "hook"]).await;
    assert!(t.ok(&["notify", "events"]).await.contains("no notifications"));
    assert!(t.fail(&["notify"]).await.contains("give a bucket"));
}

#[tokio::test]
async fn replication_between_buckets() {
    let t = start().await;
    t.ok(&["create", "origin"]).await;
    t.ok(&["create", "mirror"]).await;
    assert!(t.ok(&["replicate", "origin"]).await.contains("doesn't replicate"));
    // Both buckets must be versioned; the error says how.
    let e = t.fail(&["replicate", "origin", "--to", "mirror"]).await;
    assert!(e.contains("Versioning must be 'Enabled'") && e.contains("roll storage update origin --versioning on"), "{e}");
    t.ok(&["update", "origin", "--versioning", "on"]).await;
    let e = t.fail(&["replicate", "origin", "--to", "mirror"]).await;
    assert!(e.contains("Destination bucket must have versioning enabled: mirror"), "{e}");
    t.ok(&["update", "mirror", "--versioning", "on"]).await;
    assert!(t.fail(&["replicate", "origin", "--to", "nowhere"]).await.contains("Destination bucket must exist"));
    assert!(t.fail(&["replicate", "origin", "--to", "origin"]).await.contains("cannot be the same"));

    let o = t.run(&["replicate", "origin", "--to", "mirror", "--prefix", "img/", "--delete-markers", "--id", "imgs"]).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.text().contains("origin/img/* → mirror (rule imgs)"), "{}", o.text());
    assert!(o.stderr.contains("no bytes copied"), "{}", o.stderr);
    let r = t.json(&["replicate", "origin"]).await;
    assert_eq!(r["rules"][0]["id"], "imgs");
    assert_eq!((r["rules"][0]["delete_markers"].as_bool(), r["rules"][0]["prefix"].as_str()), (Some(true), Some("img/")));

    // New versions under the prefix appear in the mirror.
    t.put("origin", "img/a.png", b"aaaa").await;
    t.put("origin", "notes/b.txt", b"bbbb").await;
    eventually("the replica", || async { t.get("mirror", "img/a.png").await.is_some() }).await;
    assert_eq!(t.get("mirror", "img/a.png").await.unwrap(), b"aaaa");
    assert!(t.get("mirror", "notes/b.txt").await.is_none());
    // A second rule, with existing objects: the version already there follows.
    t.ok(&["replicate", "origin", "--to", "mirror", "--prefix", "notes/", "--existing", "--id", "notes"]).await;
    eventually("the backfilled replica", || async { t.get("mirror", "notes/b.txt").await.is_some() }).await;
    let listed = t.ok(&["replicate", "origin"]).await;
    assert!(listed.contains("imgs") && listed.contains("notes") && listed.contains("img/*"), "{listed}");
    let show = t.ok(&["show", "origin"]).await;
    assert!(show.contains("replicate  imgs: img/* → mirror (delete markers)"), "{show}");

    // One rule, then all of them.
    assert!(t.ok(&["replicate", "rm", "origin", "notes"]).await.contains("removed replication rule notes"));
    assert!(t.fail(&["replicate", "rm", "origin", "notes"]).await.contains("no replication rule notes"));
    assert_eq!(t.json(&["replicate", "origin"]).await["rules"].as_array().unwrap().len(), 1);
    assert!(t.ok(&["replicate", "rm", "origin"]).await.contains("no longer replicates"));
    assert!(t.ok(&["replicate", "origin"]).await.contains("doesn't replicate"));
    assert!(t.store.replication("origin").unwrap().is_none());
}
