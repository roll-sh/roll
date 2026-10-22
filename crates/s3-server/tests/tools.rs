//! Real S3 tools in Docker against an in-process server: the AWS CLI (also with SSE-C), rclone,
//! the MinIO client and s3cmd (SigV2). Skipped when Docker isn't available, unless
//! `ROLL_COMPAT_REQUIRED=1`. Containers run one at a time, on the host network.

mod common;

use common::*;
use std::path::Path;
use std::process::Command;

fn docker_ok() -> bool {
    let ok = Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
    if !ok && std::env::var("ROLL_COMPAT_REQUIRED").is_ok_and(|v| v == "1") {
        panic!("Docker is required (ROLL_COMPAT_REQUIRED=1) but isn't available");
    }
    if !ok {
        eprintln!("skipping: Docker isn't available");
    }
    ok
}

fn uid_gid() -> String {
    let id = |f: &str| String::from_utf8(Command::new("id").arg(f).output().unwrap().stdout).unwrap().trim().to_string();
    format!("{}:{}", id("-u"), id("-g"))
}

/// The MinIO client image: `minio/mc` is no longer on Docker Hub, so Bitnami's archived build by
/// default (`ROLL_MC_IMAGE` to override; its entrypoint is overridden to `mc` either way).
fn mc_image() -> String {
    std::env::var("ROLL_MC_IMAGE").unwrap_or_else(|_| "bitnamilegacy/minio-client".into())
}

/// Run a container with `dir` mounted at `/data`; returns stdout, panics with stderr on failure.
fn run(image: &str, env: &[(&str, String)], dir: &Path, args: &[&str]) -> String {
    match try_run(image, env, dir, args) {
        Ok(out) => out,
        Err(e) => panic!("{e}"),
    }
}

/// [`run`] for a command that must fail: returns what it printed.
fn run_fails(image: &str, env: &[(&str, String)], dir: &Path, args: &[&str]) -> String {
    match try_run(image, env, dir, args) {
        Ok(out) => panic!("{image} {args:?} succeeded, but must fail:\n{out}"),
        Err(e) => e,
    }
}

fn try_run(image: &str, env: &[(&str, String)], dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--network", "host", "--user", &uid_gid(), "-e", "HOME=/tmp", "-v"]).arg(format!("{}:/data", dir.display()));
    if image == mc_image() {
        cmd.args(["--entrypoint", "mc"]);
    }
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(image).args(args);
    let out = cmd.output().expect("docker run");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        return Err(format!("{image} {args:?} failed ({}):\nstdout:\n{stdout}\nstderr:\n{}", out.status, String::from_utf8_lossy(&out.stderr)));
    }
    Ok(stdout)
}

/// One container at a time, across the tests in this file.
static DOCKER: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(move || {
        let _one = DOCKER.lock().unwrap_or_else(|e| e.into_inner());
        f()
    })
    .await
    .unwrap()
}

/// Files to upload: a small one, a 21 MiB one (multipart for every tool) and a directory.
fn fixtures(dir: &Path) {
    std::fs::write(dir.join("small.txt"), b"hello from a real tool\n").unwrap();
    std::fs::write(dir.join("big.bin"), data(21 << 20, 9)).unwrap();
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("nested/deeper")).unwrap();
    for (i, p) in ["a.txt", "b with space.txt", "nested/c.json", "nested/deeper/d+e.txt"].iter().enumerate() {
        std::fs::write(tree.join(p), data(1000 + i * 777, i as u8)).unwrap();
    }
}

fn same(a: &Path, b: &Path) {
    assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap(), "{} vs {}", a.display(), b.display());
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_cli() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    let presigned = blocking(move || {
        let env = [("AWS_ACCESS_KEY_ID", ACCESS.to_string()), ("AWS_SECRET_ACCESS_KEY", SECRET.to_string()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let aws = |args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str()];
            all.extend_from_slice(args);
            run("amazon/aws-cli", &env, &path, &all)
        };
        aws(&["s3", "mb", "s3://awscli"]);
        aws(&["s3", "cp", "/data/small.txt", "s3://awscli/small.txt"]);
        aws(&["s3", "cp", "/data/big.bin", "s3://awscli/big.bin"]);
        aws(&["s3", "sync", "/data/tree", "s3://awscli/tree"]);
        let ls = aws(&["s3", "ls", "--recursive", "s3://awscli/"]);
        for k in ["small.txt", "big.bin", "tree/a.txt", "tree/b with space.txt", "tree/nested/c.json", "tree/nested/deeper/d+e.txt"] {
            assert!(ls.contains(k), "{k} missing from:\n{ls}");
        }
        // A second sync uploads nothing.
        assert!(aws(&["s3", "sync", "/data/tree", "s3://awscli/tree"]).trim().is_empty());
        aws(&["s3", "cp", "s3://awscli/big.bin", "/data/big.out"]);
        same(&path.join("big.bin"), &path.join("big.out"));
        std::fs::create_dir_all(path.join("down")).unwrap();
        aws(&["s3", "sync", "s3://awscli/tree", "/data/down"]);
        same(&path.join("tree/nested/deeper/d+e.txt"), &path.join("down/nested/deeper/d+e.txt"));
        let head = aws(&["s3api", "head-object", "--bucket", "awscli", "--key", "big.bin"]);
        assert!(head.contains("\"ContentLength\": 22020096"), "{head}");
        // Low-level multipart.
        let up = aws(&["s3api", "create-multipart-upload", "--bucket", "awscli", "--key", "mp.bin", "--query", "UploadId", "--output", "text"]);
        let id = up.trim().to_string();
        let etag = aws(&["s3api", "upload-part", "--bucket", "awscli", "--key", "mp.bin", "--upload-id", &id, "--part-number", "1", "--body", "/data/small.txt", "--query", "ETag", "--output", "text"]);
        std::fs::write(path.join("parts.json"), format!("{{\"Parts\":[{{\"PartNumber\":1,\"ETag\":{}}}]}}", etag.trim())).unwrap();
        aws(&["s3api", "complete-multipart-upload", "--bucket", "awscli", "--key", "mp.bin", "--upload-id", &id, "--multipart-upload", "file:///data/parts.json"]);
        aws(&["s3", "cp", "s3://awscli/mp.bin", "/data/mp.out"]);
        same(&path.join("small.txt"), &path.join("mp.out"));
        let url = aws(&["s3", "presign", "s3://awscli/small.txt", "--expires-in", "300"]);
        aws(&["s3", "rm", "s3://awscli/small.txt"]);
        aws(&["s3", "rm", "--recursive", "s3://awscli/tree"]);
        let ls = aws(&["s3", "ls", "--recursive", "s3://awscli/"]);
        assert!(!ls.contains("tree/"), "{ls}");
        url.trim().to_string()
    })
    .await;
    // The presigned URL was made before the delete: it now finds nothing, but it authenticates.
    let r = reqwest::get(&presigned).await.unwrap();
    assert_eq!(r.status(), 404, "{}", r.text().await.unwrap());
}

/// Bucket configuration with the AWS CLI: a public-read policy (then read anonymously, by the
/// CLI and by plain HTTP), tags, a public access block, ACLs and a static website.
#[tokio::test(flavor = "multi_thread")]
async fn aws_cli_bucket_configuration() {
    if !docker_ok() {
        return;
    }
    let t = start_with(|c| c.website_domain = Some("web.localtest".into())).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), b"<h1>home</h1>").unwrap();
    std::fs::write(dir.path().join("error.html"), b"<h1>missing</h1>").unwrap();
    let policy = serde_json::json!({"Version": "2012-10-17", "Statement": [{"Sid": "PublicRead", "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::awssite/*"}]});
    std::fs::write(dir.path().join("policy.json"), policy.to_string()).unwrap();
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let env = [("AWS_ACCESS_KEY_ID", ACCESS.to_string()), ("AWS_SECRET_ACCESS_KEY", SECRET.to_string()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let aws = |args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str()];
            all.extend_from_slice(args);
            run("amazon/aws-cli", &env, &path, &all)
        };
        aws(&["s3", "mb", "s3://awssite"]);
        aws(&["s3", "cp", "/data/index.html", "s3://awssite/index.html"]);
        aws(&["s3", "cp", "/data/error.html", "s3://awssite/error.html"]);
        aws(&["s3api", "put-bucket-policy", "--bucket", "awssite", "--policy", "file:///data/policy.json"]);
        let got = aws(&["s3api", "get-bucket-policy", "--bucket", "awssite", "--query", "Policy", "--output", "text"]);
        assert!(got.contains("PublicRead"), "{got}");
        let status = aws(&["s3api", "get-bucket-policy-status", "--bucket", "awssite", "--query", "PolicyStatus.IsPublic", "--output", "text"]);
        assert_eq!(status.trim(), "True");
        // Anonymous reads through the CLI itself.
        aws(&["--no-sign-request", "s3", "cp", "s3://awssite/index.html", "/data/anon.html"]);
        same(&path.join("index.html"), &path.join("anon.html"));
        aws(&["s3api", "put-bucket-tagging", "--bucket", "awssite", "--tagging", "TagSet=[{Key=env,Value=prod}]"]);
        let tags = aws(&["s3api", "get-bucket-tagging", "--bucket", "awssite", "--query", "TagSet[0].Value", "--output", "text"]);
        assert_eq!(tags.trim(), "prod");
        aws(&["s3", "website", "s3://awssite/", "--index-document", "index.html", "--error-document", "error.html"]);
        let site = aws(&["s3api", "get-bucket-website", "--bucket", "awssite", "--query", "IndexDocument.Suffix", "--output", "text"]);
        assert_eq!(site.trim(), "index.html");
        let enc = aws(&["s3api", "get-bucket-encryption", "--bucket", "awssite", "--query", "ServerSideEncryptionConfiguration.Rules[0].ApplyServerSideEncryptionByDefault.SSEAlgorithm", "--output", "text"]);
        assert_eq!(enc.trim(), "AES256");
        let own = aws(&["s3api", "get-bucket-ownership-controls", "--bucket", "awssite", "--query", "OwnershipControls.Rules[0].ObjectOwnership", "--output", "text"]);
        assert_eq!(own.trim(), "BucketOwnerEnforced");
        aws(&["s3api", "put-bucket-ownership-controls", "--bucket", "awssite", "--ownership-controls", "Rules=[{ObjectOwnership=BucketOwnerPreferred}]"]);
        aws(&["s3api", "put-object-acl", "--bucket", "awssite", "--key", "error.html", "--acl", "public-read"]);
        let acl = aws(&["s3api", "get-object-acl", "--bucket", "awssite", "--key", "error.html"]);
        assert!(acl.contains("AllUsers"), "{acl}");
        aws(&["s3api", "put-public-access-block", "--bucket", "awssite", "--public-access-block-configuration", "RestrictPublicBuckets=true"]);
        let pab = aws(&["s3api", "get-public-access-block", "--bucket", "awssite", "--query", "PublicAccessBlockConfiguration.RestrictPublicBuckets", "--output", "text"]);
        assert_eq!(pab.trim(), "True");
        aws(&["s3api", "delete-public-access-block", "--bucket", "awssite"]);
    })
    .await;
    // The website, as a browser sees it.
    let http = reqwest::Client::new();
    let get = |path: &str| http.get(format!("{}{path}", t.endpoint)).header("host", "awssite.web.localtest").send();
    let r = get("/").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "<h1>home</h1>");
    let r = get("/nope").await.unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.text().await.unwrap(), "<h1>missing</h1>");
    assert_eq!(reqwest::get(format!("{}/awssite/index.html", t.endpoint)).await.unwrap().status(), 200, "anonymous REST read");
}

/// Versioning with the AWS CLI's s3api, and rclone's version listing.
#[tokio::test(flavor = "multi_thread")]
async fn versions_with_aws_cli_and_rclone() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let env = [("AWS_ACCESS_KEY_ID", ACCESS.to_string()), ("AWS_SECRET_ACCESS_KEY", SECRET.to_string()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let aws = |args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str()];
            all.extend_from_slice(args);
            run("amazon/aws-cli", &env, &path, &all)
        };
        let json = |s: String| -> serde_json::Value { serde_json::from_str(&s).unwrap_or_else(|e| panic!("{e}: {s}")) };
        aws(&["s3api", "create-bucket", "--bucket", "awsver", "--object-lock-enabled-for-bucket"]);
        let v = json(aws(&["s3api", "get-bucket-versioning", "--bucket", "awsver"]));
        assert_eq!(v["Status"], "Enabled", "{v}");
        aws(&["s3api", "put-bucket-versioning", "--bucket", "awsver", "--versioning-configuration", "Status=Enabled"]);
        let first = json(aws(&["s3api", "put-object", "--bucket", "awsver", "--key", "doc.txt", "--body", "/data/small.txt"]));
        aws(&["s3", "cp", "/data/tree/a.txt", "s3://awsver/doc.txt"]);
        aws(&["s3", "rm", "s3://awsver/doc.txt"]);
        let l = json(aws(&["s3api", "list-object-versions", "--bucket", "awsver", "--prefix", "doc"]));
        let versions = l["Versions"].as_array().unwrap();
        let markers = l["DeleteMarkers"].as_array().unwrap();
        assert_eq!((versions.len(), markers.len()), (2, 1), "{l}");
        assert_eq!(markers[0]["IsLatest"], true);
        let v1 = first["VersionId"].as_str().unwrap();
        assert!(versions.iter().any(|v| v["VersionId"] == v1 && v["IsLatest"] == false), "{l}");
        aws(&["s3api", "get-object", "--bucket", "awsver", "--key", "doc.txt", "--version-id", v1, "/data/v1.out"]);
        same(&path.join("small.txt"), &path.join("v1.out"));
        // Paged by the CLI itself.
        let paged = json(aws(&["s3api", "list-object-versions", "--bucket", "awsver", "--page-size", "1"]));
        assert_eq!(paged["Versions"].as_array().unwrap().len(), 2, "{paged}");
        // Deleting the marker brings the newest version back.
        let marker = markers[0]["VersionId"].as_str().unwrap();
        aws(&["s3api", "delete-object", "--bucket", "awsver", "--key", "doc.txt", "--version-id", marker]);
        aws(&["s3", "cp", "s3://awsver/doc.txt", "/data/current.out"]);
        same(&path.join("tree/a.txt"), &path.join("current.out"));
        // Object Lock through the CLI.
        aws(&[
            "s3api",
            "put-object-lock-configuration",
            "--bucket",
            "awsver",
            "--object-lock-configuration",
            "{\"ObjectLockEnabled\":\"Enabled\",\"Rule\":{\"DefaultRetention\":{\"Mode\":\"GOVERNANCE\",\"Days\":1}}}",
        ]);
        let locked = json(aws(&["s3api", "put-object", "--bucket", "awsver", "--key", "locked.txt", "--body", "/data/small.txt"]));
        let r = json(aws(&["s3api", "get-object-retention", "--bucket", "awsver", "--key", "locked.txt"]));
        assert_eq!(r["Retention"]["Mode"], "GOVERNANCE", "{r}");
        aws(&["s3api", "delete-object", "--bucket", "awsver", "--key", "locked.txt", "--version-id", locked["VersionId"].as_str().unwrap(), "--bypass-governance-retention"]);

        // rclone lists old versions with --s3-versions.
        let renv = [
            ("RCLONE_CONFIG_R_TYPE", "s3".to_string()),
            ("RCLONE_CONFIG_R_PROVIDER", "Other".to_string()),
            ("RCLONE_CONFIG_R_ACCESS_KEY_ID", ACCESS.to_string()),
            ("RCLONE_CONFIG_R_SECRET_ACCESS_KEY", SECRET.to_string()),
            ("RCLONE_CONFIG_R_ENDPOINT", endpoint.clone()),
            ("RCLONE_CONFIG_R_REGION", REGION.to_string()),
            ("RCLONE_CONFIG_R_FORCE_PATH_STYLE", "true".to_string()),
        ];
        let ls = run("rclone/rclone", &renv, &path, &["lsf", "--s3-versions", "r:awsver"]);
        let names: Vec<&str> = ls.lines().filter(|l| l.starts_with("doc")).collect();
        assert_eq!(names.len(), 2, "the current version and a dated old one: {ls}");
        assert!(names.contains(&"doc.txt") && names.iter().any(|n| n.starts_with("doc-v")), "{ls}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rclone() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let env = [
            ("RCLONE_CONFIG_R_TYPE", "s3".to_string()),
            ("RCLONE_CONFIG_R_PROVIDER", "Other".to_string()),
            ("RCLONE_CONFIG_R_ACCESS_KEY_ID", ACCESS.to_string()),
            ("RCLONE_CONFIG_R_SECRET_ACCESS_KEY", SECRET.to_string()),
            ("RCLONE_CONFIG_R_ENDPOINT", endpoint.clone()),
            ("RCLONE_CONFIG_R_REGION", REGION.to_string()),
            ("RCLONE_CONFIG_R_FORCE_PATH_STYLE", "true".to_string()),
        ];
        let rclone = |args: &[&str]| run("rclone/rclone", &env, &path, args);
        rclone(&["mkdir", "r:rclone"]);
        rclone(&["copy", "/data/tree", "r:rclone/tree"]);
        rclone(&["copy", "--s3-upload-cutoff", "5M", "--s3-chunk-size", "5M", "/data/big.bin", "r:rclone/"]);
        rclone(&["check", "/data/tree", "r:rclone/tree"]);
        rclone(&["check", "--download", "/data/tree", "r:rclone/tree"]);
        std::fs::write(path.join("tree/a.txt"), b"changed").unwrap();
        std::fs::remove_file(path.join("tree/b with space.txt")).unwrap();
        rclone(&["sync", "/data/tree", "r:rclone/tree"]);
        rclone(&["check", "/data/tree", "r:rclone/tree"]);
        let ls = rclone(&["lsjson", "-R", "r:rclone"]);
        let v: serde_json::Value = serde_json::from_str(&ls).unwrap();
        let names: Vec<&str> = v.as_array().unwrap().iter().filter(|e| e["IsDir"] == false).map(|e| e["Path"].as_str().unwrap()).collect();
        assert!(names.contains(&"big.bin") && names.contains(&"tree/nested/deeper/d+e.txt") && !names.contains(&"tree/b with space.txt"), "{names:?}");
        rclone(&["copyto", "r:rclone/big.bin", "/data/big.out"]);
        same(&path.join("big.bin"), &path.join("big.out"));
        rclone(&["delete", "r:rclone/tree"]);
        rclone(&["rmdirs", "r:rclone/tree"]);
        let ls = rclone(&["lsf", "-R", "r:rclone"]);
        assert_eq!(ls.trim(), "big.bin", "{ls}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn minio_client() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let port = t.port;
    let path = dir.path().to_path_buf();
    blocking(move || {
        let env = [("MC_HOST_r", format!("http://{ACCESS}:{SECRET}@127.0.0.1:{port}")), ("MC_CONFIG_DIR", "/tmp/mc".to_string())];
        let mc = |args: &[&str]| {
            let mut all = vec!["--no-color"];
            all.extend_from_slice(args);
            run(&mc_image(), &env, &path, &all)
        };
        mc(&["mb", "r/mcbucket"]);
        mc(&["cp", "/data/small.txt", "r/mcbucket/small.txt"]);
        mc(&["cp", "/data/big.bin", "r/mcbucket/big.bin"]);
        mc(&["mirror", "/data/tree", "r/mcbucket/tree"]);
        let ls = mc(&["ls", "--recursive", "r/mcbucket"]);
        for k in ["small.txt", "big.bin", "tree/a.txt", "tree/b with space.txt", "tree/nested/deeper/d+e.txt"] {
            assert!(ls.contains(k), "{k} missing from:\n{ls}");
        }
        let stat = mc(&["stat", "r/mcbucket/small.txt"]);
        assert!(stat.contains("small.txt"), "{stat}");
        assert_eq!(mc(&["cat", "r/mcbucket/small.txt"]), "hello from a real tool\n");
        mc(&["cp", "r/mcbucket/big.bin", "/data/big.out"]);
        same(&path.join("big.bin"), &path.join("big.out"));
        mc(&["rm", "--recursive", "--force", "r/mcbucket/tree"]);
        mc(&["rm", "r/mcbucket/small.txt"]);
        let ls = mc(&["ls", "--recursive", "r/mcbucket"]);
        assert!(ls.contains("big.bin") && !ls.contains("tree/") && !ls.contains("small.txt"), "{ls}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn aws_cli_sse_c() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    for op in [s3_store::SseOp::Put, s3_store::SseOp::Multipart, s3_store::SseOp::Copy] {
        if let Some(why) = s3_store::sse_c_unsupported(&**t.store.backend(), op).await {
            eprintln!("skipping: the backing object store doesn't support SSE-C here: {why}");
            return;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    // The CLI takes raw 32-byte keys (and base64s them itself).
    std::fs::write(dir.path().join("key1.bin"), b"0123456789abcdef0123456789abcdef").unwrap();
    std::fs::write(dir.path().join("key2.bin"), b"fedcba9876543210fedcba9876543210").unwrap();
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let env = [("AWS_ACCESS_KEY_ID", ACCESS.to_string()), ("AWS_SECRET_ACCESS_KEY", SECRET.to_string()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let with_endpoint = |a: &[&str]| {
            let mut all = vec!["--endpoint-url".to_string(), endpoint.clone()];
            all.extend(a.iter().map(|s| s.to_string()));
            all
        };
        let aws = |a: &[&str]| run("amazon/aws-cli", &env, &path, &with_endpoint(a).iter().map(String::as_str).collect::<Vec<_>>());
        let aws_fails = |a: &[&str]| run_fails("amazon/aws-cli", &env, &path, &with_endpoint(a).iter().map(String::as_str).collect::<Vec<_>>());
        let k1 = ["--sse-c", "AES256", "--sse-c-key", "fileb:///data/key1.bin"];
        let k2 = ["--sse-c", "AES256", "--sse-c-key", "fileb:///data/key2.bin"];
        aws(&["s3", "mb", "s3://ssec"]);
        aws(&[&["s3", "cp", "/data/small.txt", "s3://ssec/small.txt"][..], &k1].concat());
        // 21 MiB: a multipart upload, every part (and the completion) with the key.
        aws(&[&["s3", "cp", "/data/big.bin", "s3://ssec/big.bin"][..], &k1].concat());
        aws(&[&["s3", "cp", "s3://ssec/big.bin", "/data/big.out"][..], &k1].concat());
        same(&path.join("big.bin"), &path.join("big.out"));
        let head = aws(&["s3api", "head-object", "--bucket", "ssec", "--key", "small.txt", "--sse-customer-algorithm", "AES256", "--sse-customer-key", "fileb:///data/key1.bin"]);
        assert!(head.contains("\"SSECustomerAlgorithm\": \"AES256\"") && head.contains("\"ContentLength\": 23"), "{head}");
        // Without the key, or with another one, nothing comes back.
        let e = aws_fails(&["s3", "cp", "s3://ssec/small.txt", "/data/nokey.out"]);
        assert!(e.contains("400"), "{e}");
        let e = aws_fails(&[&["s3", "cp", "s3://ssec/small.txt", "/data/wrong.out"][..], &k2].concat());
        assert!(e.contains("403"), "{e}");
        assert!(!path.join("nokey.out").exists() && !path.join("wrong.out").exists());
        // A copy to another key, done server-side.
        aws(&[&["s3", "cp", "s3://ssec/small.txt", "s3://ssec/small2.txt", "--sse-c-copy-source", "AES256", "--sse-c-copy-source-key", "fileb:///data/key1.bin"][..], &k2].concat());
        aws(&[&["s3", "cp", "s3://ssec/small2.txt", "/data/small2.out"][..], &k2].concat());
        same(&path.join("small.txt"), &path.join("small2.out"));
        let ls = aws(&["s3", "ls", "s3://ssec/"]);
        assert!(ls.contains("22020096 big.bin") && ls.contains("23 small.txt"), "plaintext sizes:\n{ls}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s3cmd_sigv2() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let host = format!("127.0.0.1:{}", t.port);
    let cfg = format!("[default]\naccess_key = {ACCESS}\nsecret_key = {SECRET}\nhost_base = {host}\nhost_bucket = {host}\nuse_https = False\nsignature_v2 = True\nbucket_location = {REGION}\n");
    std::fs::write(dir.path().join("s3cfg"), cfg).unwrap();
    let path = dir.path().to_path_buf();
    let url = blocking(move || {
        let s3cmd = |a: &[&str]| {
            let mut all = vec!["-c", "/data/s3cfg"];
            all.extend_from_slice(a);
            run("d3fk/s3cmd", &[], &path, &all)
        };
        s3cmd(&["mb", "s3://s3cmd"]);
        s3cmd(&["put", "/data/small.txt", "s3://s3cmd/small.txt"]);
        // 21 MiB: s3cmd's multipart upload (15 MiB chunks), signed with SigV2 throughout.
        s3cmd(&["put", "/data/big.bin", "s3://s3cmd/big.bin"]);
        s3cmd(&["sync", "/data/tree/", "s3://s3cmd/tree/"]);
        let ls = s3cmd(&["ls", "--recursive", "s3://s3cmd/"]);
        for k in ["small.txt", "big.bin", "tree/a.txt", "tree/b with space.txt", "tree/nested/deeper/d+e.txt"] {
            assert!(ls.contains(k), "{k} missing from:\n{ls}");
        }
        s3cmd(&["get", "s3://s3cmd/big.bin", "/data/big.out"]);
        same(&path.join("big.bin"), &path.join("big.out"));
        std::fs::create_dir_all(path.join("down")).unwrap();
        s3cmd(&["sync", "s3://s3cmd/tree/", "/data/down/"]);
        same(&path.join("tree/nested/deeper/d+e.txt"), &path.join("down/nested/deeper/d+e.txt"));
        let info = s3cmd(&["info", "s3://s3cmd/small.txt"]);
        assert!(info.contains("23"), "{info}");
        let url = s3cmd(&["signurl", "s3://s3cmd/small.txt", "+300"]);
        s3cmd(&["del", "s3://s3cmd/big.bin"]);
        s3cmd(&["del", "--recursive", "s3://s3cmd/tree/"]);
        let ls = s3cmd(&["ls", "--recursive", "s3://s3cmd/"]);
        assert!(!ls.contains("big.bin") && !ls.contains("tree/") && ls.contains("small.txt"), "{ls}");
        url.trim().to_string()
    })
    .await;
    // s3cmd's presigned URL (SigV2 query-string auth).
    let r = reqwest::get(&url).await.unwrap();
    assert_eq!(r.status(), 200, "{url}");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"hello from a real tool\n");
}

/// Temporary credentials with the AWS CLI: `aws sts assume-role` against roll, then S3 with
/// what it returned (`AWS_SESSION_TOKEN`), and the same through `get-session-token`.
#[tokio::test(flavor = "multi_thread")]
async fn aws_cli_sts() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    t.client.create_bucket().bucket("ststool").send().await.unwrap();
    t.client.put_object().bucket("ststool").key("seen.txt").body(aws_sdk_s3::primitives::ByteStream::from_static(b"hi")).send().await.unwrap();
    t.client.create_bucket().bucket("hidden").send().await.unwrap();
    let key = t.store.create_key("ci", s3_store::Permission::Read, None).await.unwrap();
    let trust = s3_store::sts::Trust { keys: vec![key.id.clone()], ..Default::default() };
    let spec = s3_store::sts::RoleSpec { description: "CI".into(), permission: s3_store::Permission::Write, buckets: Some(vec!["ststool".into()]), policy: None, trust, max_session_s: None };
    t.store.create_role("ci-deploy", spec).await.unwrap();
    let arn = format!("arn:aws:iam::{}:role/ci-deploy", t.store.account_id());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("up.txt"), b"deployed by a role\n").unwrap();
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let aws = |env: &[(&str, String)], args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str(), "--output", "json"];
            all.extend_from_slice(args);
            run("amazon/aws-cli", env, &path, &all)
        };
        let fails = |env: &[(&str, String)], args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str()];
            all.extend_from_slice(args);
            run_fails("amazon/aws-cli", env, &path, &all)
        };
        let long = [("AWS_ACCESS_KEY_ID", key.id.clone()), ("AWS_SECRET_ACCESS_KEY", key.secret.clone()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let session = |out: &str| {
            let v: serde_json::Value = serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"));
            let c = &v["Credentials"];
            vec![
                ("AWS_ACCESS_KEY_ID", c["AccessKeyId"].as_str().unwrap().to_string()),
                ("AWS_SECRET_ACCESS_KEY", c["SecretAccessKey"].as_str().unwrap().to_string()),
                ("AWS_SESSION_TOKEN", c["SessionToken"].as_str().unwrap().to_string()),
                ("AWS_DEFAULT_REGION", REGION.to_string()),
            ]
        };
        let out = aws(&long, &["sts", "assume-role", "--role-arn", &arn, "--role-session-name", "github-actions", "--duration-seconds", "900"]);
        assert!(out.contains("assumed-role/ci-deploy/github-actions"), "{out}");
        let env = session(&out);
        let who = aws(&env, &["sts", "get-caller-identity"]);
        assert!(who.contains("assumed-role/ci-deploy/github-actions"), "{who}");
        let ls = aws(&env, &["s3", "ls", "s3://ststool/"]);
        assert!(ls.contains("seen.txt"), "{ls}");
        // The role writes where the key alone couldn't, and only in its bucket.
        aws(&env, &["s3", "cp", "/data/up.txt", "s3://ststool/up.txt"]);
        let denied = fails(&env, &["s3", "ls", "s3://hidden/"]);
        assert!(denied.contains("AccessDenied"), "{denied}");
        // Without the session token the temporary key is unknown.
        let no_token: Vec<(&str, String)> = env.iter().filter(|(k, _)| *k != "AWS_SESSION_TOKEN").cloned().collect();
        let err = fails(&no_token, &["s3", "ls", "s3://ststool/"]);
        assert!(err.contains("InvalidAccessKeyId"), "{err}");
        // GetSessionToken: the key's own permission (read), temporarily.
        let gst = session(&aws(&long, &["sts", "get-session-token", "--duration-seconds", "900"]));
        assert!(aws(&gst, &["s3", "ls", "s3://ststool/"]).contains("up.txt"));
        let err = fails(&gst, &["s3", "cp", "/data/up.txt", "s3://ststool/again.txt"]);
        assert!(err.contains("AccessDenied"), "{err}");
    })
    .await;
    let got = t.client.get_object().bucket("ststool").key("up.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"deployed by a role\n");
}

/// Access points with the AWS CLI: `aws s3control create-access-point` (S3 Control, which the
/// CLI sends to `<account>.<endpoint host>`), then `aws s3` by access point ARN (sent to
/// `<name>-<account>.<endpoint host>`) and by alias. The containers resolve those names to the
/// host, as DNS for a wildcard domain would.
#[tokio::test(flavor = "multi_thread")]
async fn aws_cli_access_points() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    t.client.create_bucket().bucket("apcli").send().await.unwrap();
    t.client.put_object().bucket("apcli").key("seen/by-arn.txt").body(aws_sdk_s3::primitives::ByteStream::from_static(b"through an access point\n")).send().await.unwrap();
    let account = t.store.account_id();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("up.txt"), b"uploaded by arn\n").unwrap();
    let port = t.port;
    let path = dir.path().to_path_buf();
    let alias = blocking(move || {
        let endpoint = format!("http://localhost:{port}");
        let aws = |args: &[&str]| -> String {
            let mut cmd = Command::new("docker");
            cmd.args(["run", "--rm", "--network", "host", "--user", &uid_gid(), "-e", "HOME=/tmp", "-v"]).arg(format!("{}:/data", path.display()));
            for h in [format!("{account}.localhost"), format!("cli-ap-{account}.localhost")] {
                cmd.arg("--add-host").arg(format!("{h}:127.0.0.1"));
            }
            for (k, v) in [("AWS_ACCESS_KEY_ID", ACCESS), ("AWS_SECRET_ACCESS_KEY", SECRET), ("AWS_DEFAULT_REGION", REGION)] {
                cmd.arg("-e").arg(format!("{k}={v}"));
            }
            cmd.args(["amazon/aws-cli", "--endpoint-url", &endpoint]).args(args);
            let out = cmd.output().expect("docker run");
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(out.status.success(), "aws {args:?} failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
            stdout
        };
        let created: serde_json::Value = serde_json::from_str(&aws(&["s3control", "create-access-point", "--account-id", &account, "--name", "cli-ap", "--bucket", "apcli"])).unwrap();
        let arn = created["AccessPointArn"].as_str().unwrap().to_string();
        assert_eq!(arn, format!("arn:aws:s3:us-east-1:{account}:accesspoint/cli-ap"));
        let got: serde_json::Value = serde_json::from_str(&aws(&["s3control", "get-access-point", "--account-id", &account, "--name", "cli-ap"])).unwrap();
        assert_eq!(got["Bucket"], "apcli");
        let listed = aws(&["s3control", "list-access-points", "--account-id", &account, "--bucket", "apcli"]);
        assert!(listed.contains("cli-ap"), "{listed}");
        let ls = aws(&["s3", "ls", "--recursive", &format!("s3://{arn}/")]);
        assert!(ls.contains("seen/by-arn.txt"), "{ls}");
        aws(&["s3", "cp", "/data/up.txt", &format!("s3://{arn}/up.txt")]);
        aws(&["s3api", "get-object", "--bucket", &arn, "--key", "up.txt", "/data/back.txt"]);
        assert_eq!(std::fs::read(path.join("back.txt")).unwrap(), b"uploaded by arn\n");
        // A copy whose source is named by the access point's object ARN (the CLI URL-encodes
        // the value), and by its alias.
        let copied: serde_json::Value = serde_json::from_str(&aws(&["s3api", "copy-object", "--bucket", "apcli", "--key", "copied/one.txt", "--copy-source", &format!("{arn}/object/seen/by-arn.txt")])).unwrap();
        assert!(copied["CopyObjectResult"]["ETag"].as_str().is_some(), "{copied}");
        let alias = created["Alias"].as_str().unwrap().to_string();
        aws(&["s3api", "copy-object", "--bucket", "apcli", "--key", "copied/two.txt", "--copy-source", &format!("{alias}/up.txt")]);
        alias
    })
    .await;
    let got = t.client.get_object().bucket(&alias).key("up.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"uploaded by arn\n");
    assert!(t.client.head_object().bucket("apcli").key("up.txt").send().await.is_ok());
    let one = t.client.get_object().bucket("apcli").key("copied/one.txt").send().await.unwrap();
    assert_eq!(one.body.collect().await.unwrap().into_bytes().as_ref(), b"through an access point\n");
    let two = t.client.get_object().bucket("apcli").key("copied/two.txt").send().await.unwrap();
    assert_eq!(two.body.collect().await.unwrap().into_bytes().as_ref(), b"uploaded by arn\n");
}

/// IAM with the AWS CLI (`--endpoint-url` for every service): the account root creates a user,
/// a key and an inline policy, and the user lists and copies with `aws s3`; then a group with
/// AWS's `AmazonS3ReadOnlyAccess`, a virtual MFA device and `get-account-summary`.
#[tokio::test(flavor = "multi_thread")]
async fn aws_cli_iam() {
    if !docker_ok() {
        return;
    }
    let t = start().await;
    for b in ["iamtool", "elsewhere"] {
        t.client.create_bucket().bucket(b).send().await.unwrap();
        t.client.put_object().bucket(b).key("seen.txt").body(aws_sdk_s3::primitives::ByteStream::from_static(b"hi")).send().await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("up.txt"), b"uploaded by an IAM user\n").unwrap();
    let policy = serde_json::json!({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": ["s3:ListBucket", "s3:GetObject", "s3:PutObject"], "Resource": ["arn:aws:s3:::iamtool", "arn:aws:s3:::iamtool/*"]},
        {"Effect": "Allow", "Action": "s3:ListAllMyBuckets", "Resource": "*"},
    ]});
    std::fs::write(dir.path().join("policy.json"), policy.to_string()).unwrap();
    let account = t.store.account_id();
    let endpoint = t.endpoint.clone();
    let path = dir.path().to_path_buf();
    blocking(move || {
        let aws = |env: &[(&str, String)], args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str(), "--output", "json"];
            all.extend_from_slice(args);
            run("amazon/aws-cli", env, &path, &all)
        };
        let fails = |env: &[(&str, String)], args: &[&str]| {
            let mut all = vec!["--endpoint-url", endpoint.as_str()];
            all.extend_from_slice(args);
            run_fails("amazon/aws-cli", env, &path, &all)
        };
        let json = |out: &str| serde_json::from_str::<serde_json::Value>(out).unwrap_or_else(|e| panic!("{e}: {out}"));
        let root = [("AWS_ACCESS_KEY_ID", ACCESS.to_string()), ("AWS_SECRET_ACCESS_KEY", SECRET.to_string()), ("AWS_DEFAULT_REGION", REGION.to_string())];
        let user = json(&aws(&root, &["iam", "create-user", "--user-name", "deployer", "--tags", "Key=team,Value=web"]));
        assert_eq!(user["User"]["Arn"], format!("arn:aws:iam::{account}:user/deployer"));
        let err = fails(&root, &["iam", "create-user", "--user-name", "deployer"]);
        assert!(err.contains("EntityAlreadyExists"), "{err}");
        let key = json(&aws(&root, &["iam", "create-access-key", "--user-name", "deployer"]));
        let env = [
            ("AWS_ACCESS_KEY_ID", key["AccessKey"]["AccessKeyId"].as_str().unwrap().to_string()),
            ("AWS_SECRET_ACCESS_KEY", key["AccessKey"]["SecretAccessKey"].as_str().unwrap().to_string()),
            ("AWS_DEFAULT_REGION", REGION.to_string()),
        ];
        // Before any policy the user may do nothing.
        assert!(fails(&env, &["s3", "ls"]).contains("AccessDenied"));
        aws(&root, &["iam", "put-user-policy", "--user-name", "deployer", "--policy-name", "iamtool", "--policy-document", "file:///data/policy.json"]);
        let got = json(&aws(&root, &["iam", "get-user-policy", "--user-name", "deployer", "--policy-name", "iamtool"]));
        assert_eq!(got["PolicyDocument"]["Statement"][1]["Action"], "s3:ListAllMyBuckets", "the CLI decodes the document: {got}");
        let ls = aws(&env, &["s3", "ls"]);
        assert!(ls.contains("iamtool") && ls.contains("elsewhere"), "{ls}");
        assert!(aws(&env, &["s3", "ls", "s3://iamtool/"]).contains("seen.txt"));
        aws(&env, &["s3", "cp", "/data/up.txt", "s3://iamtool/up.txt"]);
        assert!(fails(&env, &["s3", "ls", "s3://elsewhere/"]).contains("AccessDenied"));
        let who = json(&aws(&env, &["sts", "get-caller-identity"]));
        assert_eq!(who["Arn"], format!("arn:aws:iam::{account}:user/deployer"));
        // A group with AWS's read-only policy opens the other bucket for reading.
        aws(&root, &["iam", "create-group", "--group-name", "readers"]);
        aws(&root, &["iam", "attach-group-policy", "--group-name", "readers", "--policy-arn", "arn:aws:iam::aws:policy/AmazonS3ReadOnlyAccess"]);
        aws(&root, &["iam", "add-user-to-group", "--group-name", "readers", "--user-name", "deployer"]);
        assert!(aws(&env, &["s3", "ls", "s3://elsewhere/"]).contains("seen.txt"));
        let groups = json(&aws(&root, &["iam", "list-groups-for-user", "--user-name", "deployer"]));
        assert_eq!(groups["Groups"][0]["GroupName"], "readers");
        // Keys and devices.
        let keys = json(&aws(&root, &["iam", "list-access-keys", "--user-name", "deployer"]));
        assert_eq!(keys["AccessKeyMetadata"][0]["Status"], "Active");
        let mfa = json(&aws(&root, &["iam", "create-virtual-mfa-device", "--virtual-mfa-device-name", "deployer", "--outfile", "/data/qr.png", "--bootstrap-method", "QRCodePNG"]));
        assert_eq!(mfa["VirtualMFADevice"]["SerialNumber"], format!("arn:aws:iam::{account}:mfa/deployer"));
        let summary = json(&aws(&root, &["iam", "get-account-summary"]));
        assert_eq!(summary["SummaryMap"]["Users"], 1);
        // Deactivated, the key no longer works.
        aws(&root, &["iam", "update-access-key", "--user-name", "deployer", "--access-key-id", &env[0].1, "--status", "Inactive"]);
        assert!(fails(&env, &["s3", "ls"]).contains("InvalidAccessKeyId"));
    })
    .await;
    assert!(std::fs::read(dir.path().join("qr.png")).unwrap().starts_with(b"\x89PNG"), "the CLI saved the QR code");
    let got = t.client.get_object().bucket("iamtool").key("up.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"uploaded by an IAM user\n");
}
