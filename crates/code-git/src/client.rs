//! git smart HTTP client: protocol v2 `ls-refs` and `fetch` (importing from GitHub), and
//! receive-pack pushes (the GitHub mirror). Bodies are buffered and bounded by [`Limits`].

use crate::backend::{Credentials, Filter, Ref, RefList, RefStatus, RefUpdate};
use crate::object::{Object, ObjectId};
use crate::pack::{self, Limits};
use crate::pktline::{self, Pkt, Reader};
use anyhow::{Context, anyhow, bail, ensure};
use bytes::{Bytes, BytesMut};

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    limits: Limits,
    agent: String,
}

/// What to fetch.
#[derive(Clone, Debug, Default)]
pub struct FetchArgs {
    /// Objects to fetch. Empty means every branch and tag tip.
    pub wants: Vec<ObjectId>,
    /// Commits we already have, so the server sends only what's missing.
    pub haves: Vec<ObjectId>,
    pub depth: Option<u32>,
    pub filter: Option<Filter>,
}

#[derive(Clone, Debug, Default)]
pub struct Fetched {
    pub refs: RefList,
    /// Every object in the received pack, deltas resolved.
    pub objects: Vec<Object>,
    /// Commits our history is cut at (shallow fetches).
    pub shallow: Vec<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushReport {
    pub unpack: Result<(), String>,
    pub refs: Vec<RefStatus>,
    /// `remote:` lines the server sent.
    pub messages: Vec<String>,
}

impl PushReport {
    /// Every ref was accepted.
    pub fn ok(&self) -> bool {
        self.unpack.is_ok() && self.refs.iter().all(|r| r.result.is_ok())
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Client {
    pub fn new(limits: Limits) -> Self {
        Client::with_http(reqwest::Client::new(), limits)
    }

    pub fn with_http(http: reqwest::Client, limits: Limits) -> Self {
        Client { http, limits, agent: concat!("roll-code/", env!("CARGO_PKG_VERSION")).into() }
    }

    fn url(base: &str, suffix: &str) -> String {
        format!("{}/{suffix}", base.trim_end_matches('/'))
    }

    fn request(&self, method: reqwest::Method, url: &str, auth: Option<&Credentials>) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, url).header("user-agent", format!("git/2.45.0 {}", self.agent));
        if let Some(c) = auth {
            r = if c.username.is_empty() { r.bearer_auth(&c.password) } else { r.basic_auth(&c.username, Some(&c.password)) };
        }
        r
    }

    async fn body(&self, resp: reqwest::Response, max: u64) -> anyhow::Result<Bytes> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("HTTP {status}: {}", text.trim());
        }
        let mut resp = resp;
        let mut out = BytesMut::new();
        while let Some(chunk) = resp.chunk().await? {
            ensure!((out.len() + chunk.len()) as u64 <= max, "response larger than {max} bytes");
            out.extend_from_slice(&chunk);
        }
        Ok(out.freeze())
    }

    /// Checks a protocol v2 capability advertisement.
    async fn v2_caps(&self, url: &str, auth: Option<&Credentials>) -> anyhow::Result<Vec<String>> {
        let resp = self
            .request(reqwest::Method::GET, &Self::url(url, "info/refs?service=git-upload-pack"), auth)
            .header("git-protocol", "version=2")
            .send()
            .await?;
        let body = self.body(resp, 1 << 20).await?;
        let mut r = Reader::new(&body);
        let mut first = r.expect_pkt()?;
        if first.text().is_some_and(|t| t.starts_with(b"# service=")) {
            while !matches!(r.expect_pkt()?, Pkt::Flush) {}
            first = r.expect_pkt()?;
        }
        ensure!(first.text() == Some(b"version 2"), "server does not speak git protocol v2");
        let mut caps = Vec::new();
        while let p @ Pkt::Data(_) = r.expect_pkt()? {
            caps.push(String::from_utf8_lossy(p.text().unwrap()).into_owned());
        }
        Ok(caps)
    }

    async fn v2_command(&self, url: &str, auth: Option<&Credentials>, command: &str, args: &[String], max: u64) -> anyhow::Result<Bytes> {
        let mut b = BytesMut::new();
        pktline::write_line(&mut b, &format!("command={command}"));
        pktline::write_line(&mut b, &format!("agent={}", self.agent));
        pktline::write_line(&mut b, "object-format=sha1");
        pktline::delim(&mut b);
        for a in args {
            pktline::write_line(&mut b, a);
        }
        pktline::flush(&mut b);
        let resp = self
            .request(reqwest::Method::POST, &Self::url(url, "git-upload-pack"), auth)
            .header("git-protocol", "version=2")
            .header("content-type", "application/x-git-upload-pack-request")
            .header("accept", "application/x-git-upload-pack-result")
            .body(b.freeze())
            .send()
            .await?;
        self.body(resp, max).await
    }

    /// The remote's refs and `HEAD`.
    pub async fn ls_refs(&self, url: &str, auth: Option<&Credentials>) -> anyhow::Result<RefList> {
        self.v2_caps(url, auth).await?;
        let args: Vec<String> = ["symrefs", "peel", "unborn", "ref-prefix HEAD", "ref-prefix refs/heads/", "ref-prefix refs/tags/"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let body = self.v2_command(url, auth, "ls-refs", &args, 64 << 20).await?;
        parse_ls_refs(&body)
    }

    /// Fetches objects (all branch and tag tips if `args.wants` is empty).
    pub async fn fetch(&self, url: &str, auth: Option<&Credentials>, args: &FetchArgs) -> anyhow::Result<Fetched> {
        let caps = self.v2_caps(url, auth).await?;
        let refs = {
            let a: Vec<String> = ["symrefs", "peel", "unborn", "ref-prefix HEAD", "ref-prefix refs/heads/", "ref-prefix refs/tags/"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            parse_ls_refs(&self.v2_command(url, auth, "ls-refs", &a, 64 << 20).await?)?
        };
        let mut wants = args.wants.clone();
        if wants.is_empty() {
            wants = refs.refs.iter().map(|r| r.oid).collect();
            wants.sort();
            wants.dedup();
        }
        let have_set: std::collections::HashSet<_> = args.haves.iter().collect();
        wants.retain(|w| !have_set.contains(w));
        if wants.is_empty() {
            return Ok(Fetched { refs, ..Default::default() });
        }
        let fetch_caps = caps.iter().find_map(|c| c.strip_prefix("fetch=")).unwrap_or("");
        let mut a: Vec<String> = vec!["ofs-delta".into(), "no-progress".into()];
        a.extend(wants.iter().map(|w| format!("want {w}")));
        a.extend(args.haves.iter().map(|h| format!("have {h}")));
        if let Some(d) = args.depth {
            ensure!(fetch_caps.split(' ').any(|c| c == "shallow"), "server does not support shallow fetches");
            a.push(format!("deepen {d}"));
        }
        if let Some(f) = args.filter {
            ensure!(fetch_caps.split(' ').any(|c| c == "filter"), "server does not support filters");
            a.push(format!("filter {}", f.spec()));
        }
        a.push("done".into());
        let body = self.v2_command(url, auth, "fetch", &a, self.limits.max_pack_bytes + (16 << 20)).await?;
        let (pack, shallow) = parse_fetch_response(&body, self.limits.max_pack_bytes)?;
        let limits = self.limits;
        let objects = if pack.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || pack::read(&pack, &limits)).await?.context("reading fetched pack")?
        };
        Ok(Fetched { refs, objects, shallow })
    }

    /// Pushes ref updates with the objects they need. `objects` may be empty for deletes or
    /// for refs pointing at objects the remote already has.
    pub async fn push(&self, url: &str, auth: Option<&Credentials>, updates: &[RefUpdate], objects: &[Object]) -> anyhow::Result<PushReport> {
        ensure!(!updates.is_empty(), "nothing to push");
        let resp = self.request(reqwest::Method::GET, &Self::url(url, "info/refs?service=git-receive-pack"), auth).send().await?;
        let body = self.body(resp, 64 << 20).await?;
        let (_, caps) = parse_v0_advertisement(&body)?;
        let has = |c: &str| caps.iter().any(|x| x == c);
        let mut want_caps = vec!["report-status"];
        if has("side-band-64k") {
            want_caps.push("side-band-64k");
        }
        if has("quiet") {
            want_caps.push("quiet");
        }
        if updates.len() > 1 && has("atomic") {
            want_caps.push("atomic");
        }
        let agent = format!("agent={}", self.agent);
        want_caps.push(&agent);
        if updates.iter().any(|u| u.is_delete()) {
            ensure!(has("delete-refs"), "server does not allow deleting refs");
        }

        let mut b = BytesMut::new();
        for (i, u) in updates.iter().enumerate() {
            let mut line = format!("{} {} {}", u.old, u.new, u.name).into_bytes();
            if i == 0 {
                line.push(0);
                line.extend_from_slice(want_caps.join(" ").as_bytes());
            }
            line.push(b'\n');
            pktline::write(&mut b, &line);
        }
        pktline::flush(&mut b);
        if updates.iter().any(|u| !u.is_delete()) {
            b.extend_from_slice(&pack::write(objects));
        }
        let resp = self
            .request(reqwest::Method::POST, &Self::url(url, "git-receive-pack"), auth)
            .header("content-type", "application/x-git-receive-pack-request")
            .header("accept", "application/x-git-receive-pack-result")
            .body(b.freeze())
            .send()
            .await?;
        let body = self.body(resp, 16 << 20).await?;
        let (report, progress) = if has("side-band-64k") {
            let d = pktline::demux(&mut Reader::new(&body), 16 << 20)?;
            (Bytes::from(d.data), d.progress)
        } else {
            (body, Vec::new())
        };
        let messages = String::from_utf8_lossy(&progress).lines().map(str::to_string).filter(|l| !l.is_empty()).collect();
        let mut r = Reader::new(&report);
        let first = r.expect_pkt()?.text().map(|t| String::from_utf8_lossy(t).into_owned()).unwrap_or_default();
        let unpack = match first.strip_prefix("unpack ") {
            Some("ok") => Ok(()),
            Some(e) => Err(e.to_string()),
            None => bail!("bad report-status: {first}"),
        };
        let mut refs = Vec::new();
        while let Some(Pkt::Data(d)) = r.next_pkt()? {
            let line = String::from_utf8_lossy(d.strip_suffix(b"\n").unwrap_or(d)).into_owned();
            if let Some(name) = line.strip_prefix("ok ") {
                refs.push(RefStatus { name: name.into(), result: Ok(()) });
            } else if let Some(rest) = line.strip_prefix("ng ") {
                let (name, reason) = rest.split_once(' ').unwrap_or((rest, ""));
                refs.push(RefStatus { name: name.into(), result: Err(reason.into()) });
            }
        }
        Ok(PushReport { unpack, refs, messages })
    }
}

fn parse_ls_refs(body: &[u8]) -> anyhow::Result<RefList> {
    let mut list = RefList::default();
    let mut r = Reader::new(body);
    while let Some(p) = r.next_pkt()? {
        let Some(t) = p.text() else { break };
        let line = std::str::from_utf8(t)?;
        if let Some(e) = line.strip_prefix("ERR ") {
            bail!("remote error: {e}");
        }
        let mut parts = line.split(' ');
        let oid = parts.next().unwrap_or("");
        let name = parts.next().ok_or_else(|| anyhow!("bad ls-refs line {line}"))?;
        let mut peeled = None;
        let mut target = None;
        for attr in parts {
            if let Some(t) = attr.strip_prefix("symref-target:") {
                target = Some(t.to_string());
            } else if let Some(p) = attr.strip_prefix("peeled:") {
                peeled = p.parse().ok();
            }
        }
        if name == "HEAD" {
            list.head = target;
            continue;
        }
        list.refs.push(Ref { name: name.into(), oid: oid.parse().map_err(|_| anyhow!("bad ls-refs line {line}"))?, peeled });
    }
    Ok(list)
}

/// A v0 ref advertisement: refs and capabilities.
fn parse_v0_advertisement(body: &[u8]) -> anyhow::Result<(Vec<Ref>, Vec<String>)> {
    let mut r = Reader::new(body);
    let mut refs = Vec::new();
    let mut caps = Vec::new();
    let mut first = true;
    while let Some(p) = r.next_pkt()? {
        let Some(t) = p.text() else {
            if first {
                continue; // flush after `# service=…`
            }
            break;
        };
        if t.starts_with(b"# service=") {
            continue;
        }
        if let Some(e) = t.strip_prefix(b"ERR ") {
            bail!("remote error: {}", String::from_utf8_lossy(e));
        }
        let (line, cap) = match t.iter().position(|&b| b == 0) {
            Some(i) => (&t[..i], Some(&t[i + 1..])),
            None => (t, None),
        };
        if let Some(c) = cap {
            caps = String::from_utf8_lossy(c).split(' ').map(str::to_string).collect();
        }
        first = false;
        let line = std::str::from_utf8(line)?;
        let (oid, name) = line.split_once(' ').ok_or_else(|| anyhow!("bad advertisement line"))?;
        if name != "capabilities^{}" && !name.ends_with("^{}") {
            refs.push(Ref { name: name.into(), oid: oid.parse().map_err(|_| anyhow!("bad advertisement line"))?, peeled: None });
        }
    }
    Ok((refs, caps))
}

/// Splits a v2 fetch response into the pack and the shallow list.
fn parse_fetch_response(body: &[u8], max_pack: u64) -> anyhow::Result<(Vec<u8>, Vec<ObjectId>)> {
    let mut r = Reader::new(body);
    let mut shallow = Vec::new();
    loop {
        let p = r.expect_pkt()?;
        let header = p.text().ok_or_else(|| anyhow!("unexpected packet in fetch response"))?;
        if let Some(e) = header.strip_prefix(b"ERR ") {
            bail!("remote error: {}", String::from_utf8_lossy(e));
        }
        match header {
            b"packfile" => {
                let d = pktline::demux(&mut r, usize::try_from(max_pack).unwrap_or(usize::MAX))?;
                return Ok((d.data, shallow));
            }
            _ => {
                // acknowledgments, shallow-info, wanted-refs, packfile-uris: lines until delim/flush
                loop {
                    match r.expect_pkt()? {
                        Pkt::Delim => break,
                        Pkt::Flush | Pkt::ResponseEnd => return Ok((Vec::new(), shallow)),
                        Pkt::Data(d) => {
                            if header == b"shallow-info" {
                                let d = d.strip_suffix(b"\n").unwrap_or(d);
                                if let Some(id) = d.strip_prefix(b"shallow ") {
                                    shallow.push(ObjectId::from_hex(id).ok_or_else(|| anyhow!("bad shallow line"))?);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
