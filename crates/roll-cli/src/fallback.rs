//! `roll run --fallback FILE` (docs/secrets.md, "Running through an outage"): every successful
//! fetch also writes the values to `FILE`, encrypted, and when rolld can't be reached `roll run`
//! starts from it. A deploy or a cron job keeps starting through an outage; a revoked token or a
//! refused request never falls back.
//!
//! The file is JSON: `{v, project, environment, path, fetched_at, salt, nonce, ct}`. The key is
//! HKDF-SHA256 of `ROLL_FALLBACK_KEY` (a workload identity's tokens change at every login) or of
//! the token, with the file's own salt and info `roll-fallback-v1`; AES-256-GCM seals the values
//! with everything else in the file as associated data, so none of it can be swapped or edited.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;

const INFO: &[u8] = b"roll-fallback-v1";

/// What a file is for: which values, fetched when.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Header {
    pub v: u8,
    pub project: String,
    pub environment: String,
    pub path: String,
    /// Unix ms.
    pub fetched_at: i64,
}

#[derive(Serialize, Deserialize)]
struct File {
    #[serde(flatten)]
    header: Header,
    salt: String,
    nonce: String,
    ct: String,
}

/// The secret the file's key comes from: `ROLL_FALLBACK_KEY`, else the token in use.
pub fn secret(token: Option<&str>) -> anyhow::Result<String> {
    match std::env::var("ROLL_FALLBACK_KEY").ok().filter(|k| !k.trim().is_empty()) {
        Some(k) => Ok(k),
        None => token.map(str::to_string).context("--fallback needs ROLL_TOKEN or ROLL_FALLBACK_KEY (a workload identity's tokens change at every login)"),
    }
}

fn cipher(secret: &str, salt: &[u8]) -> Aes256Gcm {
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), secret.as_bytes()).expand(INFO, key.as_mut()).expect("32 bytes is a valid length");
    Aes256Gcm::new_from_slice(key.as_ref()).expect("a 32-byte key")
}

fn aad(h: &Header) -> Vec<u8> {
    serde_json::to_vec(h).expect("a header serializes")
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Write `values` for `header` to `file`: a new file beside it, 0600, renamed over the old one.
pub fn write(file: &Path, secret: &str, header: Header, values: &Map<String, Value>) -> anyhow::Result<()> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).context("randomness")?;
    getrandom::fill(&mut nonce).context("randomness")?;
    let plain = zeroize::Zeroizing::new(serde_json::to_vec(values)?);
    let ct = cipher(secret, &salt).encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &aad(&header) }).map_err(|_| anyhow::anyhow!("sealing the fallback"))?;
    let out = File { header, salt: b64().encode(salt), nonce: b64().encode(nonce), ct: b64().encode(ct) };
    let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("writing next to {}", file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec(&out)?)?;
    tmp.persist(file).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// The file's header, before it's opened (which project it is for when rolld can't say).
pub fn header(file: &Path) -> anyhow::Result<Header> {
    let f: File = serde_json::from_slice(&std::fs::read(file).with_context(|| format!("reading {}", file.display()))?).context("the fallback file isn't one")?;
    Ok(f.header)
}

/// The values in `file`, if it is for `want` (but its time) and opens with `secret`, and isn't
/// older than `max_age_ms` at `now` (Unix ms).
pub fn read(file: &Path, secret: &str, want: &Header, now: i64, max_age_ms: i64) -> anyhow::Result<(Map<String, Value>, Header)> {
    let f: File = serde_json::from_slice(&std::fs::read(file).with_context(|| format!("reading {}", file.display()))?).context("the fallback file isn't one")?;
    let h = &f.header;
    if h.v != 1 || (h.project.as_str(), h.environment.as_str(), h.path.as_str()) != (want.project.as_str(), want.environment.as_str(), want.path.as_str()) {
        bail!("{} is for {} {}:{}, not this", file.display(), h.project, h.environment, h.path);
    }
    if now - h.fetched_at > max_age_ms {
        bail!("{} is from {} minutes ago, older than --fallback-max-age", file.display(), (now - h.fetched_at) / 60_000);
    }
    let unb64 = |s: &str| b64().decode(s).context("the fallback file is damaged");
    let (salt, nonce, ct) = (unb64(&f.salt)?, unb64(&f.nonce)?, unb64(&f.ct)?);
    if nonce.len() != 12 {
        bail!("the fallback file is damaged");
    }
    let plain = zeroize::Zeroizing::new(
        cipher(secret, &salt).decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad(h) }).map_err(|_| anyhow::anyhow!("{} doesn't open with this token (or ROLL_FALLBACK_KEY), or was changed", file.display()))?,
    );
    let values: Map<String, Value> = serde_json::from_slice(&plain).context("the fallback file is damaged")?;
    Ok((values, f.header))
}

/// `72h`, `30m`, `7d`, or seconds.
pub fn duration_ms(s: &str) -> anyhow::Result<i64> {
    let (n, unit) = s.trim().split_at(s.trim().find(|c: char| !c.is_ascii_digit()).unwrap_or(s.trim().len()));
    let n: i64 = n.parse().with_context(|| format!("{s} isn't a duration (72h, 30m, 7d)"))?;
    let unit = match unit {
        "" | "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("{s} isn't a duration (72h, 30m, 7d)"),
    };
    Ok(n.saturating_mul(unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(at: i64) -> Header {
        Header { v: 1, project: "prj_a".into(), environment: "production".into(), path: "/".into(), fetched_at: at }
    }

    #[test]
    fn a_fallback_opens_only_with_its_key_for_its_environment_and_while_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("prod.fallback");
        let values: Map<String, Value> = serde_json::from_str(r#"{"DB_URL":"postgres://db/shop"}"#).unwrap();
        write(&file, "rlt_token", header(1_000), &values).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(!std::fs::read_to_string(&file).unwrap().contains("postgres://"), "values are sealed");
        let day = 86_400_000;
        let (got, h) = read(&file, "rlt_token", &header(0), 1_000 + 60_000, day).unwrap();
        assert_eq!((got, h.fetched_at), (values, 1_000));
        assert!(read(&file, "rlt_other", &header(0), 2_000, day).is_err(), "another token");
        assert!(read(&file, "rlt_token", &Header { environment: "staging".into(), ..header(0) }, 2_000, day).is_err(), "another environment");
        assert!(read(&file, "rlt_token", &header(0), 1_000 + 2 * day, day).is_err(), "too old");
        // Its header can't be edited to look fresher: it's bound to the values.
        let edited = std::fs::read_to_string(&file).unwrap().replace("\"fetched_at\":1000", "\"fetched_at\":2000");
        std::fs::write(&file, edited).unwrap();
        assert!(read(&file, "rlt_token", &header(0), 3_000, day).is_err(), "edited");
        assert_eq!(duration_ms("72h").unwrap(), 72 * 3_600_000);
        assert!(duration_ms("soon").is_err());
    }
}
