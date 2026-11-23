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
    /// Those its pages must never serve, by name ([`served_keyed`]).
    keyed: Vec<(String, String)>,
}

/// A preview's values to mask, read the first time something asks (`Apps::errors`): a preview
/// with no errors reads none (each read of its secrets' values is audited).
pub struct ErrorMasks<'a> {
    engine: &'a Engine,
    id: &'a str,
    values: tokio::sync::OnceCell<Vec<String>>,
}

impl<'a> ErrorMasks<'a> {
    pub(super) fn new(engine: &'a Engine, id: &'a str) -> Self {
        ErrorMasks { engine, id, values: tokio::sync::OnceCell::new() }
    }

    /// The values (fails closed: no text without its masks).
    pub async fn values(&self) -> Result<&[String]> {
        self.values.get_or_try_init(|| self.engine.masks(self.id)).await.map(Vec::as_slice)
    }
}

impl Engine {
    /// `lines` as anyone who reads the preview may see them.
    pub(super) async fn masked(&self, id: &str, lines: Vec<String>) -> Result<Vec<String>> {
        let values = self.masks(id).await?;
        Ok(lines.into_iter().map(|l| code_preview::runner::mask(&l, &values)).collect())
    }

    /// `text` (a service's error with its output's tail, say) as anyone who reads the preview may
    /// see it.
    pub(crate) async fn mask_text(&self, id: &str, text: &str) -> Result<String> {
        Ok(code_preview::runner::mask(text, &self.masks(id).await?))
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
        // The values as a start would get them, but no dynamic credential: a fresh one isn't what
        // the running service holds (a start's own are added as it starts, [`Engine::add_masks`]).
        // One a roll secrets from before `dynamic: false` issues anyway is ended at once.
        let (secrets, leases, plain) = self.resolve_secrets(&r, untrusted, false, "logs").await.map_err(|e| Error::Conflict(format!("its logs wait for what they must hide: {e:#}")))?;
        for l in &leases {
            if let Err(e) = self.secrets.revoke(&r.project_id, l).await {
                warn!(preview = %id, lease = %l, error = %e, "previews: ending a credential read to mask logs (it expires by itself)");
            }
        }
        let branches = self.resource_env(id).await.map_err(|e| Error::Conflict(format!("its logs wait for what they must hide: {e:#}")))?;
        let own = project_env(&s, r.branch.as_deref()).into_iter().chain(self.store.vars(id)?).collect::<Vec<_>>();
        let keyed = served_keyed(&secrets, &plain, &own, &branches);
        let mut values = secret_values(&secrets, &plain, &own, &branches);
        // Roll's own credentials a start gets (its traces intake's token, its SMTP password): from
        // the store, so they're hidden after a restart or a move as well.
        values.push(self.store.trace_token(id)?);
        values.push(self.store.mail_password(id)?);
        let mut all = self.log_masks.lock();
        let m = all.entry(id.to_string()).or_default();
        m.read_at = now;
        m.keyed = keyed;
        add(&mut m.values, values);
        Ok(m.values.clone())
    }

    /// Whether its pages served one of its secret values (Netlify's secrets scan): the names and
    /// paths kept for the run's head (`served_secrets`), said in its activity when they change;
    /// what was served is dropped. Values that can't be read leave the last answer as it was.
    pub(super) async fn check_served(&self, r: &Row, shot: &mut crate::ports::Shot) {
        let bodies = std::mem::take(&mut shot.bodies);
        let found = if bodies.is_empty() {
            Vec::new()
        } else {
            if self.masks(&r.id).await.is_err() {
                return;
            }
            let keyed = self.log_masks.lock().get(&r.id).map(|m| m.keyed.clone()).unwrap_or_default();
            crate::served_secrets(&bodies, &keyed)
        };
        let fact = crate::ServedSecrets { head: r.build().to_string(), secrets: found };
        if let Ok(true) = self.store.set_runtime_fact(&r.id, "served_secrets", Some(&fact))
            && !fact.secrets.is_empty()
        {
            let each: Vec<String> = fact.secrets.iter().map(|s| format!("{} in {}", s.key, s.path)).collect();
            let _ = self.store.log(&r.id, &format!("its pages serve secret values ({}): anyone who opens the preview can read them", each.join(", ")));
        }
    }

    /// A start's or a job's values, masked in the preview's output from now on.
    pub(super) fn add_masks(&self, id: &str, values: Vec<String>) {
        add(&mut self.log_masks.lock().entry(id.to_string()).or_default().values, values);
    }

    /// What it's given changed (a variable set, the project's rules, a converge's ensure): the
    /// next read takes the values again, rotated secrets included.
    pub(super) fn stale_masks(&self, id: &str) {
        if let Some(m) = self.log_masks.lock().get_mut(id) {
            m.read_at = 0;
        }
    }

    pub(super) fn forget_masks(&self, id: &str) {
        self.log_masks.lock().remove(id);
    }

    /// A screenshot run's words as anyone who reads the preview (and the pull request's comment)
    /// may see them: what went wrong on the page, its links, text, markup and API operations are
    /// the page's own, which its code wrote. (A picture can't be masked: a page that renders a
    /// secret shows it.)
    pub(super) async fn mask_shot(&self, id: &str, shot: &mut crate::ports::Shot) -> Result<()> {
        let values = self.masks(id).await?;
        mask_strings(&mut shot.health, &values);
        mask_strings(&mut shot.a11y, &values);
        mask_strings(&mut shot.links, &values);
        mask_strings(&mut shot.text, &values);
        mask_strings(&mut shot.api, &values);
        for p in &mut shot.pages {
            mask_strings(&mut p.report, &values);
        }
        // Masked whole, then cut to what's shown: a value a cut splits isn't left half shown.
        let report_cut = |a11y: &mut Option<crate::PageA11y>, links: &mut Option<crate::PageLinks>, text: &mut Option<String>| {
            a11y.iter_mut().for_each(crate::PageA11y::cut);
            links.iter_mut().for_each(crate::PageLinks::cut);
            text.iter_mut().for_each(crate::cut_page_text);
        };
        shot.health.iter_mut().for_each(crate::PageHealth::cut);
        report_cut(&mut shot.a11y, &mut shot.links, &mut shot.text);
        for p in &mut shot.pages {
            report_cut(&mut p.report.a11y, &mut p.report.links, &mut p.report.text);
        }
        Ok(())
    }
}

/// Every string in `v` masked (through its JSON: every field, nested ones too).
fn mask_strings<T: serde::Serialize + serde::de::DeserializeOwned>(v: &mut T, values: &[String]) {
    fn walk(j: &mut serde_json::Value, values: &[String]) {
        match j {
            serde_json::Value::String(s) => *s = code_preview::runner::mask(s, values),
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, values)),
            serde_json::Value::Object(o) => o.values_mut().for_each(|x| walk(x, values)),
            _ => {}
        }
    }
    let Ok(mut j) = serde_json::to_value(&*v) else { return };
    walk(&mut j, values);
    if let Ok(back) = serde_json::from_value(j) {
        *v = back;
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

/// What of a preview's environment is secret: every value from roll secrets (but a `plain` one:
/// a linked host or port, only a URL's password in it) and of the project's and the preview's own
/// variables (they're revealed only by `reveal` routes), and of the branches' variables their
/// credentials (a password, a key or token, a URL's password), not their hosts and names.
pub(super) fn secret_values(secrets: &[(String, String)], plain: &[String], own: &[(String, String)], branches: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in secrets {
        if plain.contains(k) {
            out.extend(url_password(v));
        } else {
            out.push(v.clone());
        }
    }
    out.extend(own.iter().map(|(_, v)| v.clone()));
    for (k, v) in branches {
        if credential_name(k) {
            out.push(v.clone());
        }
        out.extend(url_password(v));
    }
    out.retain(|v| v.lines().any(|l| l.trim().len() >= 6));
    out
}

/// What a preview's pages must never serve, by the name it gets each as: roll secrets' values (a
/// `plain` one's URL password only), the branches' credentials, and its own variables named like a
/// credential. Never one meant for the browser (`NEXT_PUBLIC_`, `VITE_`, …), nor one under 8
/// characters (a short value is in pages by chance).
pub(super) fn served_keyed(secrets: &[(String, String)], plain: &[String], own: &[(String, String)], branches: &[(String, String)]) -> Vec<(String, String)> {
    const PUBLIC: [&str; 9] = ["NEXT_PUBLIC_", "VITE_", "PUBLIC_", "REACT_APP_", "NUXT_PUBLIC_", "EXPO_PUBLIC_", "GATSBY_", "VUE_APP_", "STORYBOOK_"];
    let public = |k: &str| {
        let u = k.to_ascii_uppercase();
        PUBLIC.iter().any(|p| u.starts_with(p))
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in secrets.iter() {
        if plain.contains(k) {
            out.extend(url_password(v).into_iter().map(|p| (k.clone(), p)));
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out.extend(own.iter().filter(|(k, _)| credential_name(k) && !public(k)).cloned());
    for (k, v) in branches {
        if credential_name(k) {
            out.push((k.clone(), v.clone()));
        }
        out.extend(url_password(v).into_iter().map(|p| (k.clone(), p)));
    }
    out.retain(|(_, v)| v.trim().chars().count() >= 8);
    out
}

/// A variable or field named like a credential: a password, secret, token or key, or a client's
/// properties (Kafka's `sasl.jaas.config=… password="…"`).
pub(super) fn credential_name(name: &str) -> bool {
    let k = name.to_ascii_uppercase();
    ["PASSWORD", "PASSWD", "PASS", "SECRET", "TOKEN", "KEY", "PROPERTIES"].iter().any(|w| k.ends_with(w) || k.contains(&format!("{w}_")))
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
    fn a_pages_own_words_are_masked_wherever_they_sit() {
        let mut health = Some(crate::PageHealth { errors: 1, first_error: Some("TypeError: key sk_test_51Hx9 rejected".into()), failed: 1, first_failed: Some("GET /api?token=sk_test_51Hx9 → 401".into()), unstable: false });
        mask_strings(&mut health, &["sk_test_51Hx9".to_string()]);
        let h = health.unwrap();
        assert_eq!((h.first_error.as_deref(), h.first_failed.as_deref(), h.errors), (Some("TypeError: key *** rejected"), Some("GET /api?token=*** → 401"), 1));
        // A value the shown 200 characters would split: parsed wide, masked whole, then cut.
        let long = format!("{{\"errors\": [\"{}hunter2pass\"], \"failed\": []}}", "x".repeat(195));
        let mut h = crate::PageHealth::parse(&long);
        mask_strings(&mut h, &["hunter2pass".to_string()]);
        let mut h = h.unwrap();
        h.cut();
        let shown = h.first_error.unwrap();
        assert!(shown.ends_with("***") && !shown.contains("hunte"), "{shown}");
    }

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
        let v = secret_values(&secrets, &[], &own, &branches);
        assert_eq!(v, ["sk_test_abcdef", "line-one\nline-two", "s3cretpw", "p%40ss-word", "p@ss-word", "wJalrXUtnFEMI"]);
        let line = "connecting to postgres://pv_abc:p@ss-word@pv-abc.postgres.internal with sk_test_abcdef, line-two";
        assert_eq!(code_preview::runner::mask(line, &v), "connecting to postgres://pv_abc:***@pv-abc.postgres.internal with ***, ***");
        // roll secrets' `plain` (a linked host): shown, but a URL's password in one isn't.
        let linked = vec![("STAGING_HOST".to_string(), "staging-db.internal".to_string()), ("STAGING_URL".into(), "postgres://app:hunter22@staging-db.internal/app".into())];
        let plain = ["STAGING_HOST".to_string(), "STAGING_URL".to_string()];
        assert_eq!(secret_values(&linked, &plain, &[], &[]), ["hunter22"]);
    }
}
