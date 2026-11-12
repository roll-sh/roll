//! Smart HTTP at the wire level: advertisements (v0 and v2), ls-refs arguments, gzip request
//! bodies, protocol errors as ERR packets, auth statuses, request limits, paths, and push options
//! reaching the backend.

mod common;

use code_git::{Limits, ServerConfig};
use common::{Mem, git, git_raw, serve, serve_with};
use std::io::Write;

fn pkt(s: &str) -> String {
    format!("{:04x}{s}", s.len() + 4)
}

/// The data lines of a pkt-line stream (flush/delim dropped).
fn lines(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let Ok(len) = usize::from_str_radix(std::str::from_utf8(&body[i..i + 4]).unwrap_or("zzzz"), 16) else { break };
        if len < 4 {
            i += 4;
            continue;
        }
        if i + len > body.len() {
            break;
        }
        out.push(String::from_utf8_lossy(&body[i + 4..i + len]).trim_end_matches('\n').to_string());
        i += len;
    }
    out
}

async fn get(url: &str, v2: bool) -> reqwest::Response {
    let mut r = reqwest::Client::new().get(url);
    if v2 {
        r = r.header("git-protocol", "version=2");
    }
    r.send().await.unwrap()
}

async fn post(url: &str, body: Vec<u8>, gzip: bool) -> (u16, Vec<u8>) {
    let mut r = reqwest::Client::new()
        .post(url)
        .header("git-protocol", "version=2")
        .header("content-type", "application/x-git-upload-pack-request");
    if gzip {
        r = r.header("content-encoding", "gzip");
    }
    let resp = r.body(body).send().await.unwrap();
    (resp.status().as_u16(), resp.bytes().await.unwrap().to_vec())
}

/// A repo with two commits on main, a feature branch and an annotated tag.
async fn populated() -> (std::sync::Arc<Mem>, String, tempfile::TempDir) {
    let mem = Mem::with_repo("acme/app");
    let url = serve(mem.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = home.join("w");
    std::fs::create_dir(&w).unwrap();
    git(home, &w, &["init", "-q"]);
    std::fs::write(w.join("a.txt"), "a\n").unwrap();
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "one"]);
    std::fs::write(w.join("b.txt"), "b\n").unwrap();
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "two"]);
    git(home, &w, &["branch", "feature"]);
    git(home, &w, &["tag", "-a", "v1", "-m", "v1"]);
    git(home, &w, &["push", "-q", &format!("{url}/acme/app.git"), "main", "feature", "v1"]);
    (mem, url, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertisements() {
    let mem = Mem::with_repo("acme/empty");
    let url = serve(mem.clone()).await;

    // v2: capabilities only, no refs.
    let r = get(&format!("{url}/acme/empty.git/info/refs?service=git-upload-pack"), true).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/x-git-upload-pack-advertisement");
    assert!(r.headers()["cache-control"].to_str().unwrap().contains("no-cache"));
    let l = lines(&r.bytes().await.unwrap());
    assert_eq!(l[0], "# service=git-upload-pack");
    for want in ["version 2", "ls-refs=unborn", "fetch=shallow filter", "object-format=sha1"] {
        assert!(l.iter().any(|x| x == want), "{want} in {l:?}");
    }
    assert!(l.iter().any(|x| x.starts_with("agent=roll-code/")), "{l:?}");

    // v0 on an empty repo: the capabilities^{} placeholder carries the capabilities.
    let r = get(&format!("{url}/acme/empty/info/refs?service=git-upload-pack"), false).await;
    let l = lines(&r.bytes().await.unwrap());
    let first = &l[1];
    assert!(first.starts_with("0000000000000000000000000000000000000000 capabilities^{}\0"), "{first:?}");
    for cap in ["side-band-64k", "shallow", "include-tag", "filter", "symref=HEAD:refs/heads/main"] {
        assert!(first.contains(cap), "{cap} in {first:?}");
    }

    // receive-pack advertises what pushes can use.
    let r = get(&format!("{url}/acme/empty.git/info/refs?service=git-receive-pack"), false).await;
    assert_eq!(r.headers()["content-type"], "application/x-git-receive-pack-advertisement");
    let l = lines(&r.bytes().await.unwrap());
    for cap in ["report-status", "delete-refs", "side-band-64k", "quiet", "atomic", "push-options"] {
        assert!(l[1].contains(cap), "{cap} in {:?}", l[1]);
    }

    // Statuses.
    assert_eq!(get(&format!("{url}/acme/empty.git/info/refs"), false).await.status(), 403, "dumb HTTP");
    assert_eq!(get(&format!("{url}/acme/empty.git/info/refs?service=git-frob"), false).await.status(), 403);
    assert_eq!(get(&format!("{url}/acme/nope.git/info/refs?service=git-upload-pack"), false).await.status(), 404);
    assert_eq!(get(&format!("{url}/acme/../x.git/info/refs?service=git-upload-pack"), false).await.status(), 404);
    assert_eq!(get(&format!("{url}/acme/empty.git/HEAD"), false).await.status(), 404);
    let r = reqwest::Client::new().post(format!("{url}/acme/empty.git/info/refs")).send().await.unwrap();
    assert_eq!(r.status(), 405);

    mem.update("acme/empty", |r| r.password = Some("s3cret".into()));
    let r = get(&format!("{url}/acme/empty.git/info/refs?service=git-upload-pack"), false).await;
    assert_eq!(r.status(), 401);
    assert!(r.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    let r = reqwest::Client::new()
        .get(format!("{url}/acme/empty.git/info/refs?service=git-upload-pack"))
        .basic_auth("agent", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = reqwest::Client::new()
        .get(format!("{url}/acme/empty.git/info/refs?service=git-upload-pack"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "a bearer token is the password");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ls_refs_arguments() {
    let (_mem, url, _tmp) = populated().await;
    let up = format!("{url}/acme/app.git/git-upload-pack");
    let req = |args: &[&str]| {
        let mut b = pkt("command=ls-refs\n") + &pkt("agent=test\n") + "0001";
        for a in args {
            b += &pkt(&format!("{a}\n"));
        }
        b + "0000"
    };
    let (s, body) = post(&up, req(&[]).into_bytes(), false).await;
    assert_eq!(s, 200);
    let all = lines(&body);
    assert_eq!(all.len(), 4, "HEAD, two branches and a tag: {all:?}");
    assert!(all[0].ends_with(" HEAD"), "{all:?}");

    let (_, body) = post(&up, req(&["symrefs", "peel", "ref-prefix refs/tags/"]).into_bytes(), false).await;
    let tags = lines(&body);
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert!(tags[0].contains("refs/tags/v1") && tags[0].contains("peeled:"), "{tags:?}");

    let (_, body) = post(&up, req(&["symrefs", "ref-prefix HEAD"]).into_bytes(), false).await;
    let head = lines(&body);
    assert_eq!(head.len(), 1);
    assert!(head[0].contains("symref-target:refs/heads/main"), "{head:?}");

    // An empty repo with `unborn` names the unborn HEAD.
    let mem = Mem::with_repo("e");
    let url = serve(mem).await;
    let (_, body) = post(&format!("{url}/e.git/git-upload-pack"), req(&["symrefs", "unborn"]).into_bytes(), false).await;
    assert_eq!(lines(&body), ["unborn HEAD symref-target:refs/heads/main"]);
    let (_, body) = post(&format!("{url}/e.git/git-upload-pack"), req(&["symrefs"]).into_bytes(), false).await;
    assert!(lines(&body).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gzip_bodies_and_protocol_errors() {
    let (mem, url, _tmp) = populated().await;
    let up = format!("{url}/acme/app.git/git-upload-pack");
    let main = mem.ref_of("acme/app", "refs/heads/main").unwrap();
    let fetch = |args: &[String]| {
        let mut b = pkt("command=fetch\n") + "0001";
        for a in args {
            b += &pkt(&format!("{a}\n"));
        }
        b + "0000"
    };

    // git gzips large requests; the server inflates them.
    let body = fetch(&[format!("want {main}"), "no-progress".into(), "done".into()]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(body.as_bytes()).unwrap();
    let (s, resp) = post(&up, gz.finish().unwrap(), true).await;
    assert_eq!(s, 200);
    let text = String::from_utf8_lossy(&resp);
    assert!(text.contains("packfile") && text.contains("PACK"), "{}", &text[..text.len().min(200)]);
    let (s, _) = post(&up, b"not gzip".to_vec(), true).await;
    assert_eq!(s, 400);

    // Haves without done: acknowledgments only.
    let (_, resp) = post(&up, fetch(&[format!("want {main}"), format!("have {main}")]).into_bytes(), false).await;
    let l = lines(&resp);
    assert_eq!(l[0], "acknowledgments");
    assert!(l.contains(&format!("ACK {main}")) && l.contains(&"ready".to_string()), "{l:?}");
    let unknown = "1".repeat(40);
    let (_, resp) = post(&up, fetch(&[format!("want {main}"), format!("have {unknown}")]).into_bytes(), false).await;
    assert_eq!(lines(&resp), ["acknowledgments", "NAK"]);

    // Errors come back as ERR packets the git client prints.
    for (args, msg) in [
        (vec![format!("want {main}"), "deepen-since 1700000000".into(), "done".into()], "deepen-since is not supported"),
        (vec![format!("want {main}"), "filter sparse:oid=abc".into(), "done".into()], "unsupported filter"),
        (vec!["want zzzz".into(), "done".into()], "invalid object id"),
        (vec![format!("want {main}"), "frobnicate".into(), "done".into()], "unexpected fetch argument"),
        (vec![format!("want {main}"), "deepen x".into(), "done".into()], "invalid deepen"),
    ] {
        let (s, resp) = post(&up, fetch(&args).into_bytes(), false).await;
        assert_eq!(s, 200);
        let l = lines(&resp);
        assert!(l.len() == 1 && l[0].starts_with("ERR ") && l[0].contains(msg), "{args:?}: {l:?}");
    }
    let (_, resp) = post(&up, (pkt("command=destroy\n") + "0000").into_bytes(), false).await;
    assert!(lines(&resp)[0].contains("unknown command"), "{:?}", lines(&resp));
    let (_, resp) = post(&up, b"zzzzgarbage".to_vec(), false).await;
    assert!(lines(&resp).first().is_some_and(|l| l.starts_with("ERR ")), "{:?}", String::from_utf8_lossy(&resp));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_limits() {
    let mem = Mem::with_repo("r");
    let url = serve_with(mem, ServerConfig { max_request_bytes: 64, limits: Limits { max_pack_bytes: 512, ..Limits::default() }, ..ServerConfig::default() }).await;
    let big = pkt("command=ls-refs\n") + &"0001".repeat(40) + "0000";
    let (s, _) = post(&format!("{url}/r.git/git-upload-pack"), big.into_bytes(), false).await;
    assert_eq!(s, 413);
    // A gzip bomb is cut at the limit, not inflated whole.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&vec![b'0'; 1 << 20]).unwrap();
    let (s, _) = post(&format!("{url}/r.git/git-upload-pack"), gz.finish().unwrap(), true).await;
    assert_eq!(s, 413);
    // Pushes are bounded by the pack limit. The server answers without reading the rest of the
    // body and closes the connection, so the client sees either the 413 or the connection closed
    // under it while it was still sending; either way nothing was accepted.
    match reqwest::Client::new().post(format!("{url}/r.git/git-receive-pack")).body(vec![b'0'; 4 << 20]).send().await {
        Ok(r) => assert_eq!(r.status(), 413),
        Err(e) => {
            let mut cause: Option<&dyn std::error::Error> = Some(&e);
            let mut closed = false;
            while let Some(c) = cause {
                let text = c.to_string().to_lowercase();
                closed |= ["reset", "broken pipe", "closed", "canceled"].iter().any(|w| text.contains(w));
                cause = c.source();
            }
            assert!(closed, "{e:?}");
        }
    }
    // And the server is still there.
    let (s, _) = post(&format!("{url}/r.git/git-upload-pack"), (pkt("command=ls-refs\n") + "0000").into_bytes(), false).await;
    assert_eq!(s, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_options_and_pusher_reach_the_backend() {
    let mem = Mem::with_repo("r");
    let url = serve(mem.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = home.join("w");
    std::fs::create_dir(&w).unwrap();
    git(home, &w, &["init", "-q"]);
    std::fs::write(w.join("a"), "a").unwrap();
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "a"]);
    let authed = url.replace("http://", "http://deploy-bot:pw@");
    let out = git_raw(home, &w, &["push", "-q", "-o", "ci.skip", "-o", "reviewer=ada", &format!("{authed}/r.git"), "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pushes = mem.pushes.lock().unwrap().clone();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].0, ["ci.skip", "reviewer=ada"]);
    // Without a password the server never asks, so git pushes anonymously.
    assert!(pushes[0].1.is_none() || pushes[0].1.as_deref() == Some("deploy-bot"), "{pushes:?}");
    mem.update("r", |r| r.password = Some("pw".into()));
    std::fs::write(w.join("b"), "b").unwrap();
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "b"]);
    git(home, &w, &["push", "-q", &format!("{authed}/r.git"), "main"]);
    assert_eq!(mem.pushes.lock().unwrap()[1].1.as_deref(), Some("deploy-bot"), "the authorized identity is the pusher");
}
