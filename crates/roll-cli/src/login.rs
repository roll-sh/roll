//! `roll login` and `roll logout`: a person signs the CLI in through the browser, as `gh auth
//! login` does (docs/permissions.md, "Signing the CLI in"). The CLI registers itself as a public
//! OAuth client (RFC 7591) with a loopback redirect on a port it holds (RFC 8252), opens roll's
//! consent screen with PKCE, and keeps what the exchange answers in `~/.config/roll/logins.json`
//! (0600), one entry per API URL. Commands use it only when no other credential is set
//! (`ROLL_TOKEN`, `ROLL_IDENTITY`, `ROLL_ADMIN_TOKEN`), refreshing it as it ends; `roll logout`
//! revokes it and forgets it.

use anyhow::{Context, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(clap::Args)]
pub struct Login {
    /// The preset the consent screen starts from (you choose what it may do there): read-only,
    /// agent, project, development, previews.
    #[arg(long, default_value = "development")]
    scope: String,
    /// Don't open a browser here: show a code to enter on roll in any browser (over SSH, in a
    /// container). Over SSH it's the default.
    #[arg(long)]
    no_browser: bool,
}

/// One API's login.
#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    client_id: String,
    grant_id: String,
    access_token: String,
    refresh_token: String,
    /// When the access token ends (ms).
    expires_at: i64,
    token_endpoint: String,
    revocation_endpoint: String,
}

fn logins_path() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty()).context("HOME isn't set")?).join(".config"),
    };
    Ok(base.join("roll").join("logins.json"))
}

/// Where this API's login is kept, when there is one (for `roll token whoami`).
pub fn kept(api: &str) -> Option<PathBuf> {
    load().ok()?.contains_key(api).then(|| logins_path().ok()).flatten()
}

fn load() -> anyhow::Result<BTreeMap<String, Saved>> {
    let path = logins_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("{} isn't roll's logins", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the logins readable only by this user (written aside and renamed into place).
fn save(all: &BTreeMap<String, Saved>) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = logins_path()?;
    let dir = path.parent().context("the config path has no directory")?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".logins.json.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(all)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Held while a login is read and refreshed, so two commands never spend one refresh token
/// (roll revokes a connection whose spent refresh token comes back).
struct Lock(std::fs::File);

impl Lock {
    fn take() -> anyhow::Result<Lock> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let path = logins_path()?.with_file_name("logins.lock");
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the descriptor is open for the call; flock only takes the lock.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("locking {}: {}", path.display(), std::io::Error::last_os_error());
        }
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: as above; the lock also goes when the file closes.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

fn random(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the system's random source");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The access token of this API's login, refreshed when it ends within a minute; `None` when
/// there's no login, or it ended (said once on stderr: the command goes on without it).
pub async fn token(api: &str, http: &reqwest::Client) -> Option<String> {
    let fresh = |s: &Saved| s.expires_at - now_ms() > 60_000;
    match load() {
        Ok(all) => match all.get(api) {
            Some(s) if fresh(s) => return Some(s.access_token.clone()),
            Some(_) => {}
            None => return None,
        },
        Err(e) => {
            eprintln!("note: {e:#}");
            return None;
        }
    }
    let refreshed = async {
        let _lock = Lock::take()?;
        // Another command may have refreshed it while this one waited.
        let mut all = load()?;
        let Some(s) = all.get(api).cloned() else { return Ok(None) };
        if fresh(&s) {
            return Ok(Some(s.access_token));
        }
        let fields = [("grant_type", "refresh_token"), ("refresh_token", s.refresh_token.as_str()), ("client_id", s.client_id.as_str())];
        let res = form(http.post(&s.token_endpoint), &fields).send().await.with_context(|| format!("can't reach {api} to refresh your login"))?;
        let ok = res.status().is_success();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if !ok {
            bail!("your roll login for {api} ended ({}): run `roll login`", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("refused"));
        }
        let next = Saved { access_token: str_of(&v, "access_token")?, refresh_token: str_of(&v, "refresh_token")?, expires_at: now_ms() + v["expires_in"].as_i64().unwrap_or(3600) * 1000, ..s };
        let access = next.access_token.clone();
        all.insert(api.to_string(), next);
        save(&all)?;
        Ok::<_, anyhow::Error>(Some(access))
    };
    match refreshed.await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("note: {e:#}");
            None
        }
    }
}

/// An `application/x-www-form-urlencoded` body (OAuth's token and revocation endpoints).
fn form(req: reqwest::RequestBuilder, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish();
    req.header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(body)
}

fn str_of(v: &Value, k: &str) -> anyhow::Result<String> {
    v[k].as_str().map(str::to_string).with_context(|| format!("the answer has no {k}"))
}

async fn get_json(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let res = http.get(url).send().await.with_context(|| format!("can't reach {url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("{url}: {status}");
    }
    res.json().await.with_context(|| format!("{url} didn't answer with JSON"))
}

/// What the browser came back with: the query of the redirect.
struct Callback {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
}

fn parse_callback(url: &url::Url) -> Callback {
    let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    Callback { code: get("code"), state: get("state"), iss: get("iss"), error: get("error_description").or(get("error")) }
}

/// The first request to `/callback` on the loopback listener; other paths (a favicon) get a 404.
async fn serve_callback(listener: tokio::net::TcpListener, base: &str) -> anyhow::Result<url::Url> {
    loop {
        let (mut conn, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let mut n = 0;
        while n < buf.len() && !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            match conn.read(&mut buf[n..]).await {
                Ok(0) | Err(_) => break,
                Ok(m) => n += m,
            }
        }
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("");
        let Ok(url) = url::Url::parse(&format!("{base}{target}")) else { continue };
        if url.path() != "/callback" {
            let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            continue;
        }
        let page = "<!doctype html><meta charset=utf-8><title>roll</title><body style=\"background:#000;color:#ededed;font:14px system-ui;display:grid;place-items:center;height:90vh\"><p>Back to your terminal: roll has your answer. You can close this tab.</p>";
        let _ = conn.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{page}", page.len()).as_bytes()).await;
        return Ok(url);
    }
}

/// A line pasted on stdin: the address the browser ended on, from a browser on another machine.
async fn pasted() -> anyhow::Result<url::Url> {
    // A thread of its own, not `spawn_blocking`: the runtime would wait for it at exit, and a
    // login the browser answered must not wait for Enter.
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = tx.send(std::io::stdin().read_line(&mut s).map(|_| s));
    });
    let line = rx.await.unwrap_or_else(|_| Ok(String::new()))?;
    let line = line.trim();
    if line.is_empty() {
        // No terminal to paste in: only the browser can answer.
        std::future::pending::<()>().await;
    }
    url::Url::parse(line).context("that isn't the address the browser ended on")
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; the name is NUL-terminated within it.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(h) if ok && !h.is_empty() => h.chars().take(40).collect(),
        _ => "this machine".into(),
    }
}

/// The endpoints `/.well-known/oauth-authorization-server` names.
struct Server {
    issuer: Option<String>,
    authorize: String,
    token: String,
    register: String,
    revocation: String,
    /// RFC 8628's, when the server signs devices in.
    device: Option<String>,
}

/// Register the CLI as a public client: with a loopback redirect, or (`None`) for the device
/// flow only.
async fn register(http: &reqwest::Client, server: &Server, redirect: Option<&str>) -> anyhow::Result<String> {
    let name = format!("roll CLI on {}", hostname());
    let body = match redirect {
        Some(r) => json!({ "client_name": name, "redirect_uris": [r], "token_endpoint_auth_method": "none", "grant_types": ["authorization_code", "refresh_token"] }),
        None => json!({ "client_name": name, "redirect_uris": [], "token_endpoint_auth_method": "none", "grant_types": [DEVICE_GRANT, "refresh_token"] }),
    };
    let reg = http.post(&server.register).json(&body).send().await.with_context(|| format!("can't reach {}", server.register))?;
    let ok = reg.status().is_success();
    let reg: Value = reg.json().await.unwrap_or(Value::Null);
    if !ok {
        bail!("registering the CLI: {}", reg["error_description"].as_str().or(reg["error"].as_str()).unwrap_or("refused"));
    }
    str_of(&reg, "client_id")
}

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// What the token endpoint answered, kept as this API's login.
fn saved(v: &Value, client_id: String, server: &Server) -> anyhow::Result<Saved> {
    Ok(Saved {
        client_id,
        grant_id: str_of(v, "grant_id")?,
        access_token: str_of(v, "access_token")?,
        refresh_token: str_of(v, "refresh_token")?,
        expires_at: now_ms() + v["expires_in"].as_i64().unwrap_or(3600) * 1000,
        token_endpoint: server.token.clone(),
        revocation_endpoint: server.revocation.clone(),
    })
}

/// Open `url` in this machine's browser: `$BROWSER` when set (as `gh` does), else the system's.
fn open_browser(url: &str) -> bool {
    let opener = std::env::var("BROWSER").ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| if cfg!(target_os = "macos") { "open" } else { "xdg-open" }.into());
    std::process::Command::new(opener).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

pub async fn login(api: &str, http: &reqwest::Client, json_out: bool, l: Login) -> anyhow::Result<()> {
    let meta = get_json(http, &format!("{api}/.well-known/oauth-authorization-server")).await.context("this roll doesn't sign in through the browser")?;
    let server = Server {
        issuer: meta["issuer"].as_str().map(str::to_string),
        authorize: str_of(&meta, "authorization_endpoint")?,
        token: str_of(&meta, "token_endpoint")?,
        register: str_of(&meta, "registration_endpoint")?,
        revocation: str_of(&meta, "revocation_endpoint")?,
        device: meta["device_authorization_endpoint"].as_str().map(str::to_string),
    };
    // Over SSH the browser is on another machine: a code to enter there beats a redirect to a
    // port it can't reach (as `gh auth login` does).
    let remote = std::env::var_os("SSH_CONNECTION").is_some() && std::env::var_os("BROWSER").is_none();
    let saved = match &server.device {
        Some(device) if l.no_browser || remote => device_flow(http, &server, device, &l.scope).await?,
        // No browser opens here: a code instead.
        Some(device) => match browser_flow(http, &server, &l.scope, false, true).await? {
            Some(s) => s,
            None => device_flow(http, &server, device, &l.scope).await?,
        },
        None => browser_flow(http, &server, &l.scope, l.no_browser, false).await?.context("no browser opened")?,
    };
    finish(api, http, json_out, saved).await
}

/// RFC 8628: show a code to enter on roll in any browser, and poll until its person decides.
async fn device_flow(http: &reqwest::Client, server: &Server, endpoint: &str, scope: &str) -> anyhow::Result<Saved> {
    let client_id = register(http, server, None).await?;
    let res = form(http.post(endpoint), &[("client_id", client_id.as_str()), ("scope", scope)]).send().await.with_context(|| format!("can't reach {endpoint}"))?;
    let ok = res.status().is_success();
    let d: Value = res.json().await.unwrap_or(Value::Null);
    if !ok {
        bail!("starting the sign-in: {}", d["error_description"].as_str().or(d["error"].as_str()).unwrap_or("refused"));
    }
    let device_code = str_of(&d, "device_code")?;
    let code = str_of(&d, "user_code")?;
    let page = str_of(&d, "verification_uri")?;
    let link = d["verification_uri_complete"].as_str().unwrap_or(&page).to_string();
    eprintln!("To sign this CLI in, open this on any device and choose what it may do:\n\n  {link}\n\nor go to {page} and enter the code {code}. Waiting…");
    let mut interval = d["interval"].as_u64().unwrap_or(5).max(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(d["expires_in"].as_u64().unwrap_or(600));
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if std::time::Instant::now() > deadline {
            bail!("no one entered the code in time: run `roll login` again");
        }
        let fields = [("grant_type", DEVICE_GRANT), ("device_code", device_code.as_str()), ("client_id", client_id.as_str())];
        let res = form(http.post(&server.token), &fields).send().await.with_context(|| format!("can't reach {}", server.token))?;
        let ok = res.status().is_success();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if ok {
            return saved(&v, client_id, server);
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => bail!("not signed in: it was refused"),
            Some("expired_token") => bail!("no one entered the code in time: run `roll login` again"),
            _ => bail!("signing in: {}", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("refused")),
        }
    }
}

/// The browser on this machine, back through a loopback port (RFC 8252). `None` when no browser
/// opened and `else_device` (the server signs devices in instead); otherwise the address the
/// browser ends on can be pasted back.
async fn browser_flow(http: &reqwest::Client, server: &Server, scope: &str, no_browser: bool, else_device: bool) -> anyhow::Result<Option<Saved>> {
    // A port of our own on loopback, registered exactly (the consent screen matches it exactly).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("opening a port on 127.0.0.1 for the browser to come back to")?;
    let base = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let redirect = format!("{base}/callback");
    let verifier = random(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random(16);
    let client_id = register(http, server, Some(&redirect)).await?;
    let mut url = url::Url::parse(&server.authorize).context("the authorization endpoint isn't a URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("scope", scope);

    let opened = !no_browser && open_browser(url.as_str());
    if opened {
        eprintln!("Opened roll in your browser to choose what this CLI may do. If nothing opened, go to:\n\n  {url}\n");
    } else if else_device {
        return Ok(None);
    } else {
        eprintln!("Open this address in a browser (any machine) and choose what this CLI may do:\n\n  {url}\n");
        eprintln!("If the browser isn't on this machine, paste the address it ends on (http://127.0.0.1…) here:");
    }
    let back = tokio::select! {
        r = serve_callback(listener, &base) => r?,
        r = pasted() => r?,
        _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => bail!("no answer from the browser in 10 minutes: run `roll login` again"),
    };
    let cb = parse_callback(&back);
    if let Some(e) = cb.error {
        bail!("not signed in: {e}");
    }
    if cb.state.as_deref() != Some(state.as_str()) {
        bail!("the answer isn't for this login (its state doesn't match): run `roll login` again");
    }
    // RFC 9207: the answer names who issued it, and it's who we asked.
    if let (Some(iss), Some(want)) = (&cb.iss, &server.issuer)
        && iss != want
    {
        bail!("the answer came from {iss}, not {want}");
    }
    let code = cb.code.context("the answer has no code")?;
    let fields = [("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect.as_str()), ("client_id", client_id.as_str()), ("code_verifier", verifier.as_str())];
    let res = form(http.post(&server.token), &fields).send().await.with_context(|| format!("can't reach {}", server.token))?;
    let ok = res.status().is_success();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !ok {
        bail!("exchanging the code: {}", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("refused"));
    }
    Ok(Some(saved(&v, client_id, server)?))
}

/// Keep the login, end the one it replaces, and say who it is.
async fn finish(api: &str, http: &reqwest::Client, json_out: bool, saved: Saved) -> anyhow::Result<()> {
    let access = saved.access_token.clone();
    let replaced = {
        let _lock = Lock::take()?;
        let mut all = load()?;
        let old = all.insert(api.to_string(), saved);
        save(&all)?;
        old
    };
    // The login this one replaces ends now, rather than lingering for its 30 days.
    if let Some(old) = replaced {
        let _ = form(http.post(&old.revocation_endpoint), &[("token", old.refresh_token.as_str())]).send().await;
    }

    let me = http.get(format!("{api}/v1/access")).bearer_auth(&access).send().await?.json::<Value>().await.unwrap_or(Value::Null);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "api": api, "grant": me["grant"] }))?);
        return Ok(());
    }
    let g = &me["grant"];
    println!("Signed in to {api} as {} ({}).", g["name"].as_str().unwrap_or("roll CLI"), g["id"].as_str().unwrap_or_default());
    if g["pending"] == true {
        println!("It waits for an admin's approval (they're mailed): commands work once one approves it.");
    }
    println!("Commands use it when ROLL_TOKEN, ROLL_IDENTITY and ROLL_ADMIN_TOKEN aren't set. `roll logout` ends it.");
    Ok(())
}

pub async fn logout(api: &str, http: &reqwest::Client, json_out: bool) -> anyhow::Result<()> {
    let gone = {
        let _lock = Lock::take()?;
        let mut all = load()?;
        let gone = all.remove(api);
        if gone.is_some() {
            save(&all)?;
        }
        gone
    };
    let Some(s) = gone else {
        if json_out {
            println!("{}", json!({ "api": api, "signed_out": false }));
        } else {
            println!("Not signed in to {api}.");
        }
        return Ok(());
    };
    // Revoking the refresh token ends the connection and every access token it gave (RFC 7009).
    let revoked = form(http.post(&s.revocation_endpoint), &[("token", s.refresh_token.as_str())]).send().await.is_ok_and(|r| r.status().is_success());
    if json_out {
        println!("{}", json!({ "api": api, "signed_out": true, "revoked": revoked, "grant_id": s.grant_id }));
    } else if revoked {
        println!("Signed out of {api}: {} is revoked.", s.grant_id);
    } else {
        println!("Signed out here, but {api} couldn't be reached to revoke {}: revoke it with `roll token revoke {}`.", s.grant_id, s.grant_id);
    }
    Ok(())
}
