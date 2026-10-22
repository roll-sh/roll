//! Real S3 tools in Docker against an in-process server: the AWS CLI, rclone and the MinIO
//! client. Skipped when Docker isn't available, unless `ROLL_COMPAT_REQUIRED=1`. Containers run
//! one at a time, on the host network.

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
        panic!("{image} {args:?} failed ({}):\nstdout:\n{stdout}\nstderr:\n{}", out.status, String::from_utf8_lossy(&out.stderr));
    }
    stdout
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
