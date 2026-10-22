use crate::{Error, Result};

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_WEB_BASE: &str = "https://github.com";

/// A GitHub App. `api_base` and `web_base` point at GitHub Enterprise Server or a test server.
#[derive(Clone)]
pub struct AppConfig {
    pub app_id: u64,
    pub private_key_pem: String,
    pub webhook_secret: String,
    /// The App's URL name (`github.com/apps/<slug>`); its bot commits as `<slug>[bot]`.
    pub slug: String,
    pub api_base: String,
    pub web_base: String,
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("app_id", &self.app_id)
            .field("slug", &self.slug)
            .field("api_base", &self.api_base)
            .field("web_base", &self.web_base)
            .finish_non_exhaustive()
    }
}

impl AppConfig {
    pub fn new(
        app_id: u64,
        private_key_pem: impl Into<String>,
        webhook_secret: impl Into<String>,
        slug: impl Into<String>,
    ) -> Self {
        AppConfig {
            app_id,
            private_key_pem: private_key_pem.into(),
            webhook_secret: webhook_secret.into(),
            slug: slug.into(),
            api_base: DEFAULT_API_BASE.into(),
            web_base: DEFAULT_WEB_BASE.into(),
        }
    }

    /// From `ROLL_GITHUB_APP_ID`, `ROLL_GITHUB_PRIVATE_KEY` (PEM) or `ROLL_GITHUB_PRIVATE_KEY_PATH`,
    /// `ROLL_GITHUB_WEBHOOK_SECRET`, `ROLL_GITHUB_APP_SLUG`, and optionally `ROLL_GITHUB_API_URL`
    /// and `ROLL_GITHUB_WEB_URL`. `Ok(None)` when no App id is set: GitHub isn't configured.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let Some(id) = get("ROLL_GITHUB_APP_ID") else {
            return Ok(None);
        };
        let app_id = id
            .trim()
            .parse()
            .map_err(|_| Error::Config("ROLL_GITHUB_APP_ID must be a number".into()))?;
        let private_key_pem = match (
            get("ROLL_GITHUB_PRIVATE_KEY"),
            get("ROLL_GITHUB_PRIVATE_KEY_PATH"),
        ) {
            // Env files often carry the PEM on one line with literal `\n`s.
            (Some(pem), _) => pem.replace("\\n", "\n"),
            (None, Some(path)) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("ROLL_GITHUB_PRIVATE_KEY_PATH {path}: {e}")))?,
            (None, None) => {
                return Err(Error::Config(
                    "set ROLL_GITHUB_PRIVATE_KEY or ROLL_GITHUB_PRIVATE_KEY_PATH".into(),
                ));
            }
        };
        let webhook_secret = get("ROLL_GITHUB_WEBHOOK_SECRET")
            .ok_or_else(|| Error::Config("set ROLL_GITHUB_WEBHOOK_SECRET".into()))?;
        let slug = get("ROLL_GITHUB_APP_SLUG")
            .ok_or_else(|| Error::Config("set ROLL_GITHUB_APP_SLUG".into()))?;
        let mut config = AppConfig::new(app_id, private_key_pem, webhook_secret, slug);
        if let Some(api) = get("ROLL_GITHUB_API_URL") {
            config.api_base = api.trim_end_matches('/').into();
        }
        if let Some(web) = get("ROLL_GITHUB_WEB_URL") {
            config.web_base = web.trim_end_matches('/').into();
        }
        Ok(Some(config))
    }

    /// Where a user installs the App. `state` comes back on the setup callback.
    pub fn install_url(&self, state: Option<&str>) -> String {
        let mut url = format!("{}/apps/{}/installations/new", self.web_base, self.slug);
        if let Some(state) = state {
            url.push_str("?state=");
            url.push_str(&percent_encode(state));
        }
        url
    }

    /// The login the App's bot acts as (pushes, comments, check runs).
    pub fn bot_login(&self) -> String {
        format!("{}[bot]", self.slug)
    }
}

pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn from_lookup() {
        let env: HashMap<&str, &str> = [
            ("ROLL_GITHUB_APP_ID", "42"),
            (
                "ROLL_GITHUB_PRIVATE_KEY",
                "-----BEGIN-----\\nabc\\n-----END-----",
            ),
            ("ROLL_GITHUB_WEBHOOK_SECRET", "s"),
            ("ROLL_GITHUB_APP_SLUG", "roll-code"),
            ("ROLL_GITHUB_API_URL", "https://ghe.example/api/v3/"),
        ]
        .into();
        let c = AppConfig::from_lookup(|k| env.get(k).map(|v| v.to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(c.app_id, 42);
        assert_eq!(c.private_key_pem, "-----BEGIN-----\nabc\n-----END-----");
        assert_eq!(c.api_base, "https://ghe.example/api/v3");
        assert_eq!(
            c.install_url(Some("a b")),
            "https://github.com/apps/roll-code/installations/new?state=a%20b"
        );
        assert_eq!(c.bot_login(), "roll-code[bot]");
        assert!(AppConfig::from_lookup(|_| None).unwrap().is_none());
        assert!(
            AppConfig::from_lookup(|k| (k == "ROLL_GITHUB_APP_ID").then(|| "x".into())).is_err()
        );
    }
}
