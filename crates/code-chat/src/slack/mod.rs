//! Slack (docs.slack.dev). One Slack app serves every workspace that installs it:
//!
//! - Installs are OAuth v2 ([`SlackConfig::install_url`], [`Client::oauth_access`]): each
//!   workspace yields its own bot token (`xoxb-…`) and bot user id. Org-wide (Enterprise Grid)
//!   installs have no team and are refused.
//! - Everything Slack sends us (Events API, slash commands, interactivity) is signed with the
//!   app's signing secret; [`verify`] checks it over the raw body before anything parses it.
//! - Web API calls ([`Client`]) answer HTTP 200 with `{ok: false, error}` on failure, and 429
//!   with `Retry-After` when rate limited.
//! - Messages are Block Kit ([`blocks`]); mrkdwn text escapes `&`, `<` and `>` ([`escape`]).

pub mod blocks;
mod client;
mod events;
mod verify;

pub use crate::{Error, VerifyError};
pub use client::{Client, Fetched, Message, OAuthAccess, Posted};
pub use events::{
    ActionHit, BlockActions, Envelope, Event, Interaction, MessageEvent, Reaction, Selected,
    SlashCommand, ViewSubmission, escape, parse_command, parse_envelope, parse_interaction,
    strip_mention,
};
pub use verify::{sign, verify};

pub const DEFAULT_API_BASE: &str = "https://slack.com/api";
pub const DEFAULT_AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";

/// The bot scopes roll asks for: hear mentions and thread replies, post and update its
/// messages, take `/roll`, react, DM people and resolve their names.
pub const BOT_SCOPES: &[&str] = &[
    "app_mentions:read",
    "channels:history",
    "groups:history",
    "im:history",
    "mpim:history",
    "chat:write",
    "commands",
    "reactions:read",
    "reactions:write",
    "im:write",
    "users:read",
];

/// A Slack app. `api_base` and `authorize_url` point at a test server.
#[derive(Clone)]
pub struct SlackConfig {
    pub client_id: String,
    pub client_secret: String,
    pub signing_secret: String,
    pub api_base: String,
    pub authorize_url: String,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("client_id", &self.client_id)
            .field("api_base", &self.api_base)
            .field("authorize_url", &self.authorize_url)
            .finish_non_exhaustive()
    }
}

impl SlackConfig {
    /// From `ROLL_SLACK_CLIENT_ID`, `ROLL_SLACK_CLIENT_SECRET` and `ROLL_SLACK_SIGNING_SECRET`
    /// (all three, or none: `Ok(None)`, Slack isn't configured), and optionally
    /// `ROLL_SLACK_API_BASE` and `ROLL_SLACK_AUTHORIZE_URL`.
    pub fn from_env() -> Result<Option<SlackConfig>, String> {
        Self::from_lookup(crate::env)
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<SlackConfig>, String> {
        let required = [
            "ROLL_SLACK_CLIENT_ID",
            "ROLL_SLACK_CLIENT_SECRET",
            "ROLL_SLACK_SIGNING_SECRET",
        ];
        let Some(v) = crate::all_or_none(&get, &required)? else {
            return Ok(None);
        };
        let [client_id, client_secret, signing_secret] =
            <[String; 3]>::try_from(v).expect("all_or_none returns one value per name");
        Ok(Some(SlackConfig {
            client_id,
            client_secret,
            signing_secret,
            api_base: get("ROLL_SLACK_API_BASE")
                .map(|s| s.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| DEFAULT_API_BASE.into()),
            authorize_url: get("ROLL_SLACK_AUTHORIZE_URL")
                .unwrap_or_else(|| DEFAULT_AUTHORIZE_URL.into()),
        }))
    }

    /// Where "Add to Slack" sends a person. `state` must be single-use and tied to their roll
    /// session; `redirect_uri` must be one of the app's registered redirect URLs.
    pub fn install_url(&self, state: &str, redirect_uri: &str) -> String {
        format!(
            "{}?client_id={}&scope={}&redirect_uri={}&state={}",
            self.authorize_url,
            crate::percent_encode(&self.client_id),
            crate::percent_encode(&BOT_SCOPES.join(",")),
            crate::percent_encode(redirect_uri),
            crate::percent_encode(state),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn config_is_all_or_none() {
        assert_eq!(
            SlackConfig::from_lookup(lookup(&[]))
                .unwrap()
                .map(|c| c.client_id),
            None
        );
        let err = SlackConfig::from_lookup(lookup(&[("ROLL_SLACK_CLIENT_ID", "1.2")])).unwrap_err();
        assert!(err.contains("ROLL_SLACK_CLIENT_SECRET"), "{err}");
        assert!(err.contains("ROLL_SLACK_SIGNING_SECRET"), "{err}");
        assert!(!err.contains("ROLL_SLACK_CLIENT_ID"), "{err}");
        let c = SlackConfig::from_lookup(lookup(&[
            ("ROLL_SLACK_CLIENT_ID", "1.2"),
            ("ROLL_SLACK_CLIENT_SECRET", "cs-secret"),
            ("ROLL_SLACK_SIGNING_SECRET", "ss-secret"),
            ("ROLL_SLACK_API_BASE", "http://127.0.0.1:9/api/"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.api_base, "http://127.0.0.1:9/api");
        assert_eq!(c.authorize_url, DEFAULT_AUTHORIZE_URL);
        let debug = format!("{c:?}");
        assert!(
            !debug.contains("cs-secret") && !debug.contains("ss-secret"),
            "{debug}"
        );
    }

    #[test]
    fn install_url_encodes_everything() {
        let c = SlackConfig {
            client_id: "123.456".into(),
            client_secret: "s".into(),
            signing_secret: "s".into(),
            api_base: DEFAULT_API_BASE.into(),
            authorize_url: DEFAULT_AUTHORIZE_URL.into(),
        };
        let url = c.install_url("st&ate", "https://roll.sh/slack/callback");
        assert!(url.starts_with("https://slack.com/oauth/v2/authorize?client_id=123.456&scope=app_mentions%3Aread%2Cchannels%3Ahistory"), "{url}");
        assert!(
            url.contains("&redirect_uri=https%3A%2F%2Froll.sh%2Fslack%2Fcallback"),
            "{url}"
        );
        assert!(url.ends_with("&state=st%26ate"), "{url}");
    }
}
