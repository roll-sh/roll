//! What a preview's output mustn't show: its services' logs (the dashboard, the API, `roll preview
//! logs`) and its jobs' output (the activity, and a teardown's in the pull request's comment) are
//! read by anyone who can read the preview, and its code, a fork's included, can print what it was
//! given. As checks' summaries are (`code_preview::runner::mask`), every value the preview got
//! from roll secrets, the project's and the preview's own variables, and its branches'
//! credentials are replaced by `***` (each line of 6 characters or more).

use super::*;

/// How long the values read for a preview's logs are used before they're read again (a secret
/// rotated meanwhile is masked from then on).
const MASKS_FOR_MS: i64 = 5 * 60_000;
/// The most values kept per preview (a value's every rotation stays masked up to this).
const MASKS_MAX: usize = 2000;

#[derive(Default)]
pub(super) struct Masks {
    /// When the values were last read from roll secrets (0: only a start's or a job's so far).
    read_at: i64,
    values: Vec<String>,
}

impl Engine {
    /// `lines` as anyone who reads the preview may see them.
    pub(super) async fn masked(&self, id: &str, lines: Vec<String>) -> Result<Vec<String>> {
        let values = self.masks(id).await?;
        Ok(lines.into_iter().map(|l| code_preview::runner::mask(&l, &values)).collect())
    }

    /// The preview's values to mask: what its starts and jobs were given, and what its secrets,
    /// variables and branches hold now (read again after [`MASKS_FOR_MS`]). Reading fails closed:
    /// no lines without their masks.
    pub(super) async fn masks(&self, id: &str) -> Result<Vec<String>> {
        let now = self.store.now_ms()?;
        if let Some(m) = self.log_masks.lock().get(id)
            && now - m.read_at < MASKS_FOR_MS
        {
            return Ok(m.values.clone());
        }
        let r = self.store.row(id)?;
        let s = self.store.settings(&r.project_id)?;
        let untrusted = !(s.secrets.allow_production && r.trusted);
        // The values as a start would get them. A dynamic credential issued for this read is
        // ended at once: a start's own are added as it starts ([`Engine::add_masks`]).
        let (secrets, leases) = self.resolve_secrets(&r, untrusted, "logs").await.map_err(|e| Error::Conflict(format!("its logs wait for what they must hide: {e:#}")))?;
        for l in &leases {
            if let Err(e) = self.secrets.revoke(&r.project_id, l).await {
                warn!(preview = %id, lease = %l, error = %e, "previews: ending a credential read to mask logs (it expires by itself)");
            }
        }
        let branches = self.resource_env(id).await.map_err(|e| Error::Conflict(format!("its logs wait for what they must hide: {e:#}")))?;
        let own = project_env(&s, r.branch.as_deref()).into_iter().chain(self.store.vars(id)?).collect::<Vec<_>>();
        let values = secret_values(&secrets, &own, &branches);
        let mut all = self.log_masks.lock();
        let m = all.entry(id.to_string()).or_default();
        m.read_at = now;
        add(&mut m.values, values);
        Ok(m.values.clone())
    }

    /// A start's or a job's values, masked in the preview's output from now on.
    pub(super) fn add_masks(&self, id: &str, values: Vec<String>) {
        add(&mut self.log_masks.lock().entry(id.to_string()).or_default().values, values);
    }

    pub(super) fn forget_masks(&self, id: &str) {
        self.log_masks.lock().remove(id);
    }
}

fn add(have: &mut Vec<String>, values: Vec<String>) {
    for v in values {
        if !have.contains(&v) {
            have.push(v);
        }
    }
    if have.len() > MASKS_MAX {
        let extra = have.len() - MASKS_MAX;
        have.drain(..extra);
    }
}

/// What of a preview's environment is secret: every value from roll secrets and of the project's
/// and the preview's own variables (they're revealed only by `reveal` routes), and of the
/// branches' variables their credentials (a password, a key or token, a URL's password), not
/// their hosts and names.
pub(super) fn secret_values(secrets: &[(String, String)], own: &[(String, String)], branches: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = secrets.iter().chain(own).map(|(_, v)| v.clone()).collect();
    for (k, v) in branches {
        let k = k.to_ascii_uppercase();
        if ["PASSWORD", "PASSWD", "PASS", "SECRET", "TOKEN", "KEY"].iter().any(|w| k.ends_with(w) || k.contains(&format!("{w}_"))) {
            out.push(v.clone());
        }
        out.extend(url_password(v));
    }
    out.retain(|v| v.lines().any(|l| l.trim().len() >= 6));
    out
}

/// The password in `scheme://user:password@host…`, as written and decoded.
fn url_password(v: &str) -> Vec<String> {
    let Some((_, rest)) = v.split_once("://") else { return Vec::new() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _)) = authority.rsplit_once('@') else { return Vec::new() };
    let Some((_, pw)) = userinfo.split_once(':') else { return Vec::new() };
    let decoded = percent_decode(pw);
    if decoded == pw { vec![pw.to_string()] } else { vec![pw.to_string(), decoded] }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_whole_and_branches_credentials_only() {
        let secrets = vec![("STRIPE_KEY".to_string(), "sk_test_abcdef".to_string()), ("FLAG".into(), "on".into())];
        let own = vec![("API_TOKEN".to_string(), "line-one\nline-two".to_string())];
        let branches = vec![
            ("PGHOST".to_string(), "pv-abc.postgres.internal".to_string()),
            ("PGPASSWORD".into(), "s3cretpw".into()),
            ("DATABASE_URL".into(), "postgres://pv_abc:p%40ss-word@pv-abc.postgres.internal:5432/app".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "wJalrXUtnFEMI".into()),
        ];
        let v = secret_values(&secrets, &own, &branches);
        assert_eq!(v, ["sk_test_abcdef", "line-one\nline-two", "s3cretpw", "p%40ss-word", "p@ss-word", "wJalrXUtnFEMI"]);
        let line = "connecting to postgres://pv_abc:p@ss-word@pv-abc.postgres.internal with sk_test_abcdef, line-two";
        assert_eq!(code_preview::runner::mask(line, &v), "connecting to postgres://pv_abc:***@pv-abc.postgres.internal with ***, ***");
    }
}
