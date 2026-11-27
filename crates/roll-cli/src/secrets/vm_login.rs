//! A VM's way in without a long-lived token (docs/secrets.md, "A local agent", "On a VM"):
//!
//! - `ROLL_IDENTITY_KIND=gcp`: the instance's service account identity token from the Compute
//!   Engine metadata server (`Metadata-Flavor: Google`; `GCE_METADATA_HOST` overrides the host, as
//!   Google's own libraries let it).
//! - `ROLL_IDENTITY_KIND=azure`: the VM's managed identity token from Azure's instance metadata
//!   service (`Metadata: true`; `ROLL_AZURE_CLIENT_ID` picks a user-assigned identity).
//! - `ROLL_IDENTITY_KIND=machine` (or a bootstrap token set): a machine identity's credential, kept
//!   in a 0600 file under the state directory, made from a bootstrap token once and replaced at half
//!   its life. A spent credential presented again revokes every one of the machine's, so the file
//!   is only ever replaced under a lock, and a login with it holds the lock too. The login's token
//!   is kept in the same file and reused by every process on the machine until a quarter of its
//!   life (at least [`LOGIN_MARGIN_MS`]) is left: a machine holds a few live logins at most
//!   (roll-access's `CHAIN_MAX_LOGINS`), and a restart storm or a cron job a minute must never
//!   reach that.
//!
//! The metadata servers are link-local and fixed: asked directly (never through a proxy), with no
//! redirects, and each login asks again for a fresh token. A token or credential never goes in an
//! error, a log or argv.

use crate::Api;
use anyhow::Context;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const METADATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The link-local metadata servers: directly, never redirected.
fn metadata_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(METADATA_TIMEOUT).build()?)
}

/// A platform's identity token for `audience` (`ROLL_OIDC_AUDIENCE`, default the server's URL).
pub(super) async fn platform_token(kind: &str, audience: &str) -> anyhow::Result<String> {
    let http = metadata_client()?;
    let enc = |s: &str| urlencoding::encode(s).into_owned();
    match kind {
        "gcp" => {
            let host = std::env::var("GCE_METADATA_HOST").ok().filter(|h| !h.trim().is_empty()).unwrap_or_else(|| "metadata.google.internal".into());
            let url = format!("http://{}/computeMetadata/v1/instance/service-accounts/default/identity?audience={}&format=full", host.trim(), enc(audience));
            let r = http.get(url).header("Metadata-Flavor", "Google").send().await.context("asking the Compute Engine metadata server for the instance's identity token (is this a GCE VM with a service account?)")?;
            anyhow::ensure!(r.status().is_success(), "the Compute Engine metadata server answered {} for the identity token", r.status());
            anyhow::ensure!(r.headers().get("metadata-flavor").is_some_and(|v| v == "Google"), "the answer for the identity token isn't the Compute Engine metadata server's");
            let token = r.text().await?.trim().to_string();
            anyhow::ensure!(token.split('.').count() == 3, "the Compute Engine metadata server's identity token isn't a JWT");
            Ok(token)
        }
        "azure" => {
            let mut url = format!("http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}", enc(audience));
            if let Some(id) = std::env::var("ROLL_AZURE_CLIENT_ID").ok().filter(|i| !i.trim().is_empty()) {
                url.push_str(&format!("&client_id={}", enc(id.trim())));
            }
            let r = http.get(url).header("Metadata", "true").send().await.context("asking Azure's instance metadata service for the VM's managed identity token (is this an Azure VM with a managed identity?)")?;
            let status = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            // Its error says why (no identity, a resource Entra doesn't know); never a token.
            anyhow::ensure!(status.is_success(), "Azure's instance metadata service answered {status} for the managed identity token: {}", v["error_description"].as_str().unwrap_or("no reason given"));
            Ok(v["access_token"].as_str().context("Azure's managed identity answer has no access_token")?.to_string())
        }
        k => anyhow::bail!("no metadata server for ROLL_IDENTITY_KIND={k}"),
    }
}

/// What the machine keeps in its credential file.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    identity: String,
    credential: String,
    expires_at: i64,
    refresh_at: i64,
    /// The bootstrap token's SHA-256 it came from: the same token in the environment isn't
    /// exchanged again (that would revoke the machine), a new one is.
    bootstrap: String,
    /// The last login's token, shared by the machine's processes ([`Login::fresh`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<Login>,
}

/// A login's token and its times (ms): when it was got and when it ends.
#[derive(serde::Serialize, serde::Deserialize)]
struct Login {
    token: String,
    at: i64,
    expires_at: i64,
}

/// A kept login is reused until this much (or a quarter of its life, if more) is left.
const LOGIN_MARGIN_MS: i64 = 2 * 60 * 1000;

impl Login {
    /// Whether it's good to hand out at `now`: well before it ends, so the process using it has
    /// most of its life (a renewing process logs in again a quarter before it ends, and finds it
    /// past this), and it isn't the token the server just refused (`stale`).
    fn fresh(&self, now: i64, stale: Option<&str>) -> bool {
        let margin = ((self.expires_at - self.at) / 4).max(LOGIN_MARGIN_MS);
        now >= self.at && now + margin < self.expires_at && stale != Some(self.token.as_str())
    }
}

/// Whether this login is a machine identity's: `ROLL_IDENTITY_KIND=machine`, or a bootstrap token
/// set with no kind.
pub(super) fn is_machine(kind: Option<&str>) -> bool {
    kind == Some("machine") || (kind.is_none() && bootstrap_token().ok().flatten().is_some())
}

/// `ROLL_BOOTSTRAP_TOKEN`, or the contents of `ROLL_BOOTSTRAP_TOKEN_FILE`.
fn bootstrap_token() -> anyhow::Result<Option<String>> {
    let env = |k: &str| std::env::var(k).ok().filter(|t| !t.trim().is_empty());
    if let Some(t) = env("ROLL_BOOTSTRAP_TOKEN") {
        return Ok(Some(t.trim().to_string()));
    }
    match env("ROLL_BOOTSTRAP_TOKEN_FILE") {
        Some(f) => Ok(Some(std::fs::read_to_string(&f).with_context(|| format!("reading ROLL_BOOTSTRAP_TOKEN_FILE {f}"))?.trim().to_string()).filter(|t| !t.is_empty())),
        None => Ok(None),
    }
}

/// Where the machine credential is kept: `ROLL_MACHINE_CREDENTIAL_FILE`, else
/// `machine-<identity>.json` under systemd's `$STATE_DIRECTORY`, `$XDG_STATE_HOME/roll`
/// (Windows: `%LOCALAPPDATA%\roll`) or `~/.local/state/roll`.
fn credential_file(identity: &str) -> anyhow::Result<PathBuf> {
    if let Some(f) = std::env::var_os("ROLL_MACHINE_CREDENTIAL_FILE").filter(|f| !f.is_empty()) {
        return Ok(PathBuf::from(f));
    }
    let state = std::env::var_os("STATE_DIRECTORY")
        .filter(|d| !d.is_empty())
        // systemd may list several, `:`-separated: the first is this unit's.
        .map(|d| PathBuf::from(d.to_string_lossy().split(':').next().unwrap_or_default()))
        .or_else(|| std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("roll")));
    #[cfg(windows)]
    let state = state.or_else(|| std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("roll")));
    let dir = state
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/roll")))
        .context("no state directory for the machine credential: set ROLL_MACHINE_CREDENTIAL_FILE")?;
    let safe: String = identity.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    Ok(dir.join(format!("machine-{safe}.json")))
}

fn sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The file written whole or not at all: a 0600 temporary file, synced, renamed over it.
fn keep(path: &Path, k: &Kept) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir).with_context(|| format!("making {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    #[cfg(windows)]
    crate::sys::acl::exclusive(&mut o);
    let mut f = o.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(windows)]
    crate::sys::acl::private(&f).with_context(|| format!("making {} private", tmp.display()))?;
    f.write_all(serde_json::to_string(k)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("keeping the machine credential in {}", path.display()))?;
    Ok(())
}

/// A machine identity's login: under the credential file's lock, the login kept there while it's
/// fresh (and isn't `stale`, the token the caller had refused or is renewing); else its credential
/// (made from the bootstrap token the first time, or from a new one; replaced once it's past half
/// its life), then `login` with it, kept for the machine's other processes.
pub(super) async fn machine_login(api: &Api, identity: &str, stale: Option<&str>) -> anyhow::Result<(String, i64)> {
    let path = credential_file(identity)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let f = o.open(&lock_path).with_context(|| format!("opening {}", lock_path.display()))?;
        f.lock().with_context(|| format!("locking {}", lock_path.display()))?;
        Ok(f)
    })
    .await??;
    let out = locked(api, identity, &path, stale).await;
    drop(lock);
    out
}

async fn locked(api: &Api, identity: &str, path: &Path, stale: Option<&str>) -> anyhow::Result<(String, i64)> {
    let kept: Option<Kept> = match std::fs::read_to_string(path) {
        Ok(text) => Some(serde_json::from_str::<Kept>(&text).with_context(|| format!("{} isn't a machine credential file", path.display()))?).filter(|k| k.identity == identity),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    };
    // A bootstrap token this credential didn't come from: a new start (a machine re-enrolled).
    let fresh = bootstrap_token()?.filter(|t| kept.as_ref().is_none_or(|k| k.bootstrap != sha256(t)));
    let now = super::now_ms();
    let mut kept = match (kept, fresh) {
        (_, Some(token)) => {
            let k = exchange_retrying(api, identity, "bootstrap", json!({ "identity": identity, "token": token }), sha256(&token)).await?;
            keep(path, &k)?;
            eprintln!("roll: exchanged the bootstrap token for a machine credential, kept in {}", path.display());
            k
        }
        (Some(k), _) => k,
        (None, None) => anyhow::bail!("no machine credential in {} and no bootstrap token: set ROLL_BOOTSTRAP_TOKEN (or ROLL_BOOTSTRAP_TOKEN_FILE) to one made with `roll secrets identities bootstrap {identity}`", path.display()),
    };
    anyhow::ensure!(kept.expires_at > now, "the machine credential in {} has ended: the machine needs a new bootstrap token (roll secrets identities bootstrap {identity})", path.display());
    if now >= kept.refresh_at {
        let body = json!({ "identity": identity, "credential": kept.credential });
        // Unanswered even when retried, the old one may be spent already (rolld kept the exchange
        // and the answer was lost): logging in with it would end the machine, so no login now. The
        // next run within the server's grace retries the same refresh.
        let next = exchange_retrying(api, identity, "refresh", body, kept.bootstrap.clone()).await.map_err(|e| {
            if super::no_answer(&e) { e.context("replacing the machine credential got no answer, so it isn't used to log in: run again within two minutes and the replacement is retried") } else { e }
        })?;
        // A replaced credential ends no login: the machine's goes on (one chain).
        let next = Kept { login: kept.login.take(), ..next };
        keep(path, &next)?;
        kept = next;
    }
    // The machine's own login, still good: no new one.
    if let Some(l) = kept.login.take_if(|l| l.fresh(now, stale)) {
        return Ok((l.token, l.expires_at));
    }
    let (token, expires_at) = super::post_login(api, json!({ "identity": identity, "machine": kept.credential })).await?;
    kept.login = Some(Login { token: token.clone(), at: now, expires_at });
    // Not kept, it's only logged in again by the next process: never a reason to fail this one.
    if let Err(e) = keep(path, &kept) {
        eprintln!("roll: couldn't keep the login in {} for the machine's other processes: {e:#}", path.display());
    }
    Ok((token, expires_at))
}

/// [`exchange`], asked again a few times while it gets no answer: rolld answers a retry of an
/// exchange it kept (its answer lost) within two minutes with a new credential, where a later
/// login with the spent one would end the machine (roll-access's store/bootstrap.rs).
async fn exchange_retrying(api: &Api, identity: &str, step: &str, body: Value, bootstrap: String) -> anyhow::Result<Kept> {
    let mut wait = std::time::Duration::from_secs(1);
    for _ in 0..3 {
        match exchange(api, identity, step, body.clone(), bootstrap.clone()).await {
            Err(e) if super::no_answer(&e) => {
                eprintln!("roll: {} got no answer ({e:#}); asking again", if step == "bootstrap" { "exchanging the bootstrap token" } else { "replacing the machine credential" });
                tokio::time::sleep(wait).await;
                wait *= 3;
            }
            done => return done,
        }
    }
    exchange(api, identity, step, body, bootstrap).await
}

/// `POST /v1/secrets/login/{step}`: the next machine credential.
async fn exchange(api: &Api, identity: &str, step: &str, body: Value, bootstrap: String) -> anyhow::Result<Kept> {
    let mut req = api.http.post(format!("{}/v1/secrets/login/{step}", api.base)).json(&body);
    for (k, v) in &api.headers {
        req = req.header(k, v);
    }
    let res = req.send().await.with_context(|| format!("can't reach {} for the machine credential", api.base))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let message = format!("{} the machine credential of {identity}: {}", if step == "bootstrap" { "exchanging the bootstrap token for" } else { "replacing" }, v["error"].as_str().map(String::from).unwrap_or_else(|| status.to_string()));
        return Err(anyhow::Error::new(crate::ApiStatus { status, message }));
    }
    let credential = v["secret"].as_str().context("the answer has no machine credential")?.to_string();
    let expires_at = v["expires_at"].as_i64().context("the answer has no expiry")?;
    let refresh_at = v["refresh_at"].as_i64().unwrap_or(expires_at);
    Ok(Kept { identity: identity.to_string(), credential, expires_at, refresh_at, bootstrap, login: None })
}
