//! `roll secrets import --from` for the managers read through the user's own CLI, signed in as
//! they already are (docs/secrets.md, "Moving from another manager"): 1Password (`op`),
//! Bitwarden Secrets Manager (`bws`), AWS Parameter Store (`aws ssm`), Google Secret Manager
//! (`gcloud`), Azure Key Vault (`az`) and Akeyless (`akeyless`).
//!
//! Names go on the command line, never values: values come back on the CLI's stdout only, and no
//! error or line printed here holds one. Every source stops at 500 secrets, a secret it can't
//! read stops it (only "not found" is nothing there), and two secrets that give one key stop it,
//! naming both.

use anyhow::{Context, bail, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::io::AsyncReadExt;

/// The `--from` values this module reads.
pub const SOURCES: &[&str] = &["1password", "bitwarden", "aws-parameter-store", "gcp", "azure", "akeyless"];

const MAX_SECRETS: usize = 500;
/// The most a CLI's answer may hold, and how long one may take.
const OUTPUT_MAX: usize = 16 << 20;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A key to import. `secret` is the source's own mark (Parameter Store's SecureString,
/// 1Password's concealed fields): what `--sensitive` tags.
pub struct Entry {
    pub key: String,
    pub value: String,
    pub secret: bool,
    /// What the source says about it (Doppler's note, Infisical's comment): the key's comment.
    pub comment: String,
}

/// What a source gives, by folder under the import's path (`""` is the path itself).
pub type Folders = Vec<(String, Vec<Entry>)>;

/// Keys by folder, each with where it came from: a second secret giving a key stops the import.
#[derive(Default)]
struct Tree(BTreeMap<String, BTreeMap<String, (String, String, bool)>>);

impl Tree {
    fn put(&mut self, folder: &str, key: String, value: String, secret: bool, origin: String) -> anyhow::Result<()> {
        let keys = self.0.entry(folder.to_string()).or_default();
        if let Some((other, ..)) = keys.get(&key) {
            let at = if folder.is_empty() { String::new() } else { format!(" in /{folder}") };
            bail!("{other} and {origin} both give {key}{at}: rename one there, or import them separately (a narrower --source each)");
        }
        keys.insert(key, (origin, value, secret));
        Ok(())
    }

    fn folders(self) -> Folders {
        self.0.into_iter().map(|(f, keys)| (f, keys.into_iter().map(|(key, (_, value, secret))| Entry { key, value, secret, comment: String::new() }).collect())).collect()
    }
}

fn at_most(n: usize, what: &str) -> anyhow::Result<()> {
    ensure!(n <= MAX_SECRETS, "{n} secrets in {what}: {MAX_SECRETS} at most at once; import a narrower --source");
    Ok(())
}

/// A name as a roll key: letters uppercased, any other character `_`, a leading digit after a
/// `_` (`prod-db-url` → `PROD_DB_URL`). Azure Key Vault's names are letters, digits and `-` (and
/// case-insensitive), so this is the azure-key-vault sync's `_` → `-` in reverse.
pub fn upper_key(name: &str) -> String {
    let mut key: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    if key.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        key.insert(0, '_');
    }
    key
}

/// Words people wrote (1Password's item titles, section and field labels) as one key, joined by
/// `_`: `Main DB` + `password` → `MAIN_DB_PASSWORD`, runs of other characters one `_`.
fn label_key(parts: &[&str]) -> Option<String> {
    let mut key = String::new();
    for c in parts.join("_").chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_uppercase());
        } else if !key.is_empty() && !key.ends_with('_') {
            key.push('_');
        }
    }
    let key = key.trim_end_matches('_').to_string();
    if key.is_empty() {
        return None;
    }
    Some(if key.starts_with(|c: char| c.is_ascii_digit()) { format!("_{key}") } else { key })
}

fn is_key(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && s.len() <= 256 && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// A name under `base` in a path-shaped manager (Parameter Store, Akeyless) as its folder and key:
/// `/prod/shop/db/password` under `/prod/shop` → (`db`, `PASSWORD`).
fn place(name: &str, base: &str, from: &str) -> anyhow::Result<(String, String)> {
    let rel = name.strip_prefix(base).filter(|r| base.is_empty() || r.starts_with('/')).with_context(|| format!("{from}: {name} isn't under {base}"))?;
    let mut segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let last = segments.pop().with_context(|| format!("{from}: {name} has no name under {base}"))?;
    let segment = |s: &&str| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if let Some(bad) = segments.iter().find(|s| !segment(s)) {
        bail!("{from}: {name}: {bad:?} can't be a roll folder (letters, digits, _ and -): import it alone, with its folder as --source");
    }
    ensure!(segments.len() <= 8, "{from}: {name} is more than 8 folders under {base}; import it in parts");
    Ok((segments.join("/"), upper_key(last)))
}

/// A CLI's run: its stdout, or the last line of its stderr when it failed.
enum Ran {
    Ok(Vec<u8>),
    Failed(String),
}

/// Run `bin args` with the user's own environment (their logins), with nothing on stdin. Its
/// stdout is read up to 16 MiB, its stderr's end kept, and it's killed after 60 s.
async fn run(bin: &str, args: &[&str]) -> anyhow::Result<Ran> {
    let what = format!("{bin} {}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "));
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {bin} (install its CLI and sign in to it)"))?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let tail = tokio::spawn(async move {
        let (mut keep, mut buf) = (Vec::new(), [0u8; 4096]);
        while let Ok(n @ 1..) = stderr.read(&mut buf).await {
            keep.extend_from_slice(&buf[..n]);
            if keep.len() > 8192 {
                keep.drain(..keep.len() - 8192);
            }
        }
        keep
    });
    let mut stdout = child.stdout.take().context("stdout")?;
    let read = async {
        let mut out = Vec::new();
        (&mut stdout).take(OUTPUT_MAX as u64 + 1).read_to_end(&mut out).await.with_context(|| what.clone())?;
        ensure!(out.len() <= OUTPUT_MAX, "{what}: its answer is over 16 MiB; import a narrower --source");
        let status = child.wait().await.with_context(|| what.clone())?;
        Ok((status, out))
    };
    let (status, out) = tokio::time::timeout(TIMEOUT, read).await.map_err(|_| anyhow::anyhow!("{what}: no answer in 60 s"))??;
    if status.success() {
        return Ok(Ran::Ok(out));
    }
    let err = tail.await.unwrap_or_default();
    let last = String::from_utf8_lossy(&err).lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().chars().take(300).collect();
    Ok(Ran::Failed(last))
}

/// `bin args`'s JSON; `None` when it failed and `missing` says its error is "not found". Any other
/// failure stops the import, with the CLI's error (never its output: it holds values).
async fn json_or_missing(bin: &str, args: &[&str], missing: impl Fn(&str) -> bool) -> anyhow::Result<Option<Value>> {
    let what = format!("{bin} {}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "));
    match run(bin, args).await? {
        Ran::Ok(out) => Ok(Some(serde_json::from_slice(&out).with_context(|| format!("{what}: its answer isn't JSON"))?)),
        Ran::Failed(e) if missing(&e) => Ok(None),
        Ran::Failed(e) => bail!("{what}: {e}"),
    }
}

async fn json(bin: &str, args: &[&str]) -> anyhow::Result<Value> {
    Ok(json_or_missing(bin, args, |_| false).await?.unwrap_or_default())
}

/// The user's `aws` CLI's JSON (`ROLL_AWS_CLI` names another binary).
pub async fn aws(args: &[&str]) -> anyhow::Result<Value> {
    json(&std::env::var("ROLL_AWS_CLI").unwrap_or_else(|_| "aws".into()), args).await
}

fn list<'a>(v: &'a Value, what: &str) -> anyhow::Result<&'a Vec<Value>> {
    v.as_array().with_context(|| format!("{what}: expected a list"))
}

/// What `from` gives for `source`, or `None` when `from` isn't one of [`SOURCES`].
pub async fn read(from: &str, source: &str) -> anyhow::Result<Option<Folders>> {
    let mut t = Tree::default();
    match from {
        "1password" => onepassword(source, &mut t).await?,
        "bitwarden" => bitwarden(source, &mut t).await?,
        "aws-parameter-store" => parameter_store(source, &mut t).await?,
        "gcp" => gcp(source, &mut t).await?,
        "azure" => azure(source, &mut t).await?,
        "akeyless" => akeyless(source, &mut t).await?,
        _ => return Ok(None),
    }
    ensure!(!t.0.is_empty(), "{from} {source} has no values to import");
    Ok(Some(t.folders()))
}

/// 1Password: `VAULT` (every item, keys `ITEM_FIELD`) or `VAULT/ITEM` (one item, keys `FIELD`).
/// A field in a named section adds it (`ITEM_SECTION_FIELD`). Every field with a value comes in,
/// as `op run` users reference any field, except notes and one-time passwords; concealed ones
/// (passwords, credentials) are the source's secrets.
async fn onepassword(source: &str, t: &mut Tree) -> anyhow::Result<()> {
    let (vault, item) = source.split_once('/').map_or((source, None), |(v, i)| (v, Some(i)));
    ensure!(!vault.is_empty() && item != Some(""), "--source is a 1Password VAULT, or VAULT/ITEM");
    let get = |id: String| async move { json_or_missing("op", &["item", "get", &id, "--vault", vault, "--format", "json", "--reveal"], |e| e.contains("isn't an item")).await };
    if let Some(item) = item {
        let v = get(item.to_string()).await.with_context(|| format!("1password {vault}/{item}"))?.with_context(|| format!("1password: no item {item} in {vault}"))?;
        return op_fields(vault, &v, false, t);
    }
    let items = json("op", &["item", "list", "--vault", vault, "--format", "json"]).await?;
    let items = list(&items, "op item list")?;
    at_most(items.len(), &format!("1password {vault}"))?;
    for it in items {
        let (id, title) = (it["id"].as_str().unwrap_or_default(), it["title"].as_str().unwrap_or_default());
        match get(id.to_string()).await.with_context(|| format!("1password {vault}/{title}"))? {
            Some(v) => op_fields(vault, &v, true, t)?,
            None => eprintln!("1password {vault}/{title}: gone since it was listed, left out"),
        }
    }
    Ok(())
}

fn op_fields(vault: &str, item: &Value, prefixed: bool, t: &mut Tree) -> anyhow::Result<()> {
    let title = item["title"].as_str().unwrap_or_default();
    fn text(v: &Value) -> Option<&str> {
        v.as_str().filter(|s| !s.trim().is_empty())
    }
    for f in item["fields"].as_array().into_iter().flatten() {
        if f["purpose"] == "NOTES" || f["type"] == "OTP" {
            continue;
        }
        let (Some(value), Some(label)) = (text(&f["value"]), text(&f["label"]).or(text(&f["id"]))) else { continue };
        let section = text(&f["section"]["label"]);
        let parts: Vec<&str> = [prefixed.then_some(title), section, Some(label)].into_iter().flatten().collect();
        let origin = format!("op://{vault}/{}", [Some(title), section, Some(label)].into_iter().flatten().collect::<Vec<_>>().join("/"));
        let key = label_key(&parts).with_context(|| format!("{origin}: no letters or digits to name a key after"))?;
        t.put("", key, value.to_string(), f["type"] == "CONCEALED", origin)?;
    }
    Ok(())
}

/// Bitwarden Secrets Manager: a project's secrets (`bws secret list PROJECT_ID`), keys as they are.
async fn bitwarden(project: &str, t: &mut Tree) -> anyhow::Result<()> {
    let v = json("bws", &["secret", "list", project, "--output", "json"]).await?;
    let secrets = list(&v, "bws secret list")?;
    at_most(secrets.len(), &format!("bitwarden project {project}"))?;
    for s in secrets {
        let (id, key) = (s["id"].as_str().unwrap_or_default(), s["key"].as_str().unwrap_or_default());
        ensure!(is_key(key), "bitwarden secret {id}: {key:?} isn't a roll key (a letter or _, then letters, digits and _): rename it there");
        let value = s["value"].as_str().with_context(|| format!("bitwarden secret {id} ({key}): no value"))?;
        t.put("", key.to_string(), value.to_string(), true, format!("bitwarden secret {id}"))?;
    }
    Ok(())
}

/// AWS Systems Manager Parameter Store: every parameter under a path, each into the folder of its
/// path under it, SecureStrings marked secret.
async fn parameter_store(source: &str, t: &mut Tree) -> anyhow::Result<()> {
    ensure!(source.starts_with('/'), "--source is a parameter path such as /prod/shop");
    let base = source.trim_end_matches('/');
    let v = aws(&["ssm", "get-parameters-by-path", "--path", if base.is_empty() { "/" } else { base }, "--recursive", "--with-decryption", "--max-items", "501", "--output", "json"]).await?;
    let params = v["Parameters"].as_array().context("aws ssm get-parameters-by-path: no Parameters")?;
    ensure!(v["NextToken"].is_null() || params.len() > MAX_SECRETS, "aws ssm get-parameters-by-path: the answer was cut short");
    at_most(params.len(), &format!("aws-parameter-store {base}"))?;
    for p in params {
        let name = p["Name"].as_str().unwrap_or_default();
        let value = p["Value"].as_str().with_context(|| format!("aws-parameter-store {name}: no value"))?;
        let (folder, key) = place(name, base, "aws-parameter-store")?;
        t.put(&folder, key, value.to_string(), p["Type"] == "SecureString", name.to_string())?;
    }
    Ok(())
}

/// Google Secret Manager: `PROJECT[/PREFIX]`, each secret whose id starts with the prefix as a key
/// named after the rest of its id (`prod-db-url` under `prod-` → `DB_URL`), its latest version.
async fn gcp(source: &str, t: &mut Tree) -> anyhow::Result<()> {
    let (project, prefix) = source.split_once('/').unwrap_or((source, ""));
    ensure!(!project.is_empty(), "--source is a Google Cloud PROJECT, or PROJECT/PREFIX");
    let v = json("gcloud", &["secrets", "list", "--project", project, "--format", "json"]).await?;
    let ids: Vec<&str> = list(&v, "gcloud secrets list")?.iter().filter_map(|s| s["name"].as_str()).map(|n| n.rsplit('/').next().unwrap_or(n)).filter(|id| id.starts_with(prefix)).collect();
    at_most(ids.len(), &format!("gcp {source}"))?;
    for id in ids {
        let origin = format!("gcp {project}/{id}");
        let args = ["secrets", "versions", "access", "latest", "--secret", id, "--project", project, "--format", "json"];
        let Some(v) = json_or_missing("gcloud", &args, |e| e.contains("NOT_FOUND")).await.with_context(|| origin.clone())? else {
            eprintln!("{origin}: no version, left out");
            continue;
        };
        use base64::Engine;
        let data = v["payload"]["data"].as_str().with_context(|| format!("{origin}: no payload"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data)).with_context(|| format!("{origin}: the payload isn't base64"))?;
        let Ok(value) = String::from_utf8(bytes) else {
            eprintln!("{origin}: binary, left out");
            continue;
        };
        let rest = id.strip_prefix(prefix).filter(|r| !r.is_empty()).unwrap_or(id);
        t.put("", upper_key(rest), value, true, origin)?;
    }
    Ok(())
}

/// Azure Key Vault: every enabled secret of a vault, `db-url` → `DB_URL`. Disabled secrets and
/// the ones backing certificates (`managed`) are left out, named.
async fn azure(vault: &str, t: &mut Tree) -> anyhow::Result<()> {
    ensure!(!vault.is_empty(), "--source is a Key Vault's name");
    let v = json("az", &["keyvault", "secret", "list", "--vault-name", vault, "--output", "json"]).await?;
    let secrets = list(&v, "az keyvault secret list")?;
    at_most(secrets.len(), &format!("azure {vault}"))?;
    for s in secrets {
        let name = s["name"].as_str().unwrap_or_default();
        let origin = format!("azure {vault}/{name}");
        if s["attributes"]["enabled"] == false || s["managed"] == true {
            eprintln!("{origin}: {}, left out", if s["managed"] == true { "a certificate's" } else { "disabled" });
            continue;
        }
        let Some(v) = json_or_missing("az", &["keyvault", "secret", "show", "--vault-name", vault, "--name", name, "--output", "json"], |e| e.contains("SecretNotFound")).await.with_context(|| origin.clone())? else {
            eprintln!("{origin}: gone since it was listed, left out");
            continue;
        };
        let value = v["value"].as_str().with_context(|| format!("{origin}: no value"))?;
        t.put("", upper_key(name), value.to_string(), true, origin)?;
    }
    Ok(())
}

/// Akeyless: every static secret under a path, each into the folder of its path under it.
async fn akeyless(source: &str, t: &mut Tree) -> anyhow::Result<()> {
    ensure!(source.starts_with('/'), "--source is an Akeyless path such as /prod/shop");
    let base = source.trim_end_matches('/');
    let v = json("akeyless", &["list-items", "--path", if base.is_empty() { "/" } else { base }, "--type", "static-secret", "--json"]).await?;
    let names: Vec<&str> = v["items"].as_array().into_iter().flatten().filter(|i| i["item_type"].as_str().is_none_or(|k| k == "STATIC_SECRET")).filter_map(|i| i["item_name"].as_str()).collect();
    at_most(names.len(), &format!("akeyless {base}"))?;
    for name in names {
        let origin = format!("akeyless {name}");
        let missing = |e: &str| e.to_ascii_lowercase().contains("item not found");
        let Some(v) = json_or_missing("akeyless", &["get-secret-value", "--name", name, "--json"], missing).await.with_context(|| origin.clone())? else {
            eprintln!("{origin}: gone since it was listed, left out");
            continue;
        };
        let value = match v.get(name).unwrap_or(&v) {
            Value::String(s) => s.clone(),
            Value::Null | Value::Object(_) => bail!("{origin}: no value in its answer"),
            other => other.to_string(),
        };
        let (folder, key) = place(name, base, "akeyless")?;
        t.put(&folder, key, value, true, origin)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_become_keys() {
        assert_eq!(super::upper_key("db-url"), "DB_URL");
        assert_eq!(super::upper_key("1st"), "_1ST");
        assert_eq!(super::label_key(&["Main DB", "Replica", "port"]).as_deref(), Some("MAIN_DB_REPLICA_PORT"));
        assert_eq!(super::label_key(&["Stripe - Live", "secret key"]).as_deref(), Some("STRIPE_LIVE_SECRET_KEY"));
        assert_eq!(super::label_key(&["🔑"]), None);
        assert_eq!(super::place("/prod/shop/db/password", "/prod/shop", "x").unwrap(), ("db".to_string(), "PASSWORD".to_string()));
        assert_eq!(super::place("/prod/shop/api-url", "/prod/shop", "x").unwrap(), (String::new(), "API_URL".to_string()));
        assert!(super::place("/prod/shopping/x", "/prod/shop", "x").is_err());
        assert!(super::place("/prod/shop/v1.2/x", "/prod/shop", "x").is_err());
    }
}
