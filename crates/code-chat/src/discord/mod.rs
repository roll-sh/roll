//! Discord (discord.com/developers/docs). One Discord application serves every server (guild)
//! that adds its bot:
//!
//! - Installs are the bot OAuth2 flow ([`DiscordConfig::install_url`]); the code exchange
//!   ([`Client::oauth_guild`]) answers with the guild, so the guild never comes from the
//!   redirect's query. The bot token is the application's, shared by every guild.
//! - Interactions (slash commands, button and select clicks, modal submits) arrive by HTTP POST
//!   at the Interactions Endpoint URL, signed with Ed25519 under the application's public key;
//!   [`verify`] checks them before anything parses them. Discord sends a `PING` when the URL is
//!   saved and refuses the URL unless unsigned or badly signed requests get a 401.
//! - An interaction must be answered within 3 seconds ([`respond`]); later work edits the message.
//! - Messages carry components: action rows (≤ 5 per message) holding up to 5 buttons or one
//!   select. A component's `custom_id` (≤ 100 characters) comes back on the click.

mod client;
mod interaction;
mod render;
mod verify;

pub use crate::{Error, VerifyError};
pub use client::{
    Channel, Client, MAX_MEMBERS_PAGE, Member, MemberUser, Posted, Role,
};
pub use interaction::{Interaction, parse_interaction};
pub use render::{
    ANSWER_PREFIX, MAX_CUSTOM_ID, QUESTION_PREFIX, answer_custom_id, question_custom_id,
    question_id, request, respond, status,
};
pub use verify::{TestSigner, verify};

pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";
pub const DEFAULT_AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";

/// The bot's permissions: view channels, send messages, embed links, read message history,
/// use application commands, create public threads, send messages in threads.
pub const BOT_PERMISSIONS: u64 = (1 << 10) // VIEW_CHANNEL
    | (1 << 11) // SEND_MESSAGES
    | (1 << 14) // EMBED_LINKS
    | (1 << 16) // READ_MESSAGE_HISTORY
    | (1 << 31) // USE_APPLICATION_COMMANDS
    | (1 << 35) // CREATE_PUBLIC_THREADS
    | (1 << 38); // SEND_MESSAGES_IN_THREADS

/// A Discord application with a bot. `api_base` and `authorize_url` point at a test server.
#[derive(Clone)]
pub struct DiscordConfig {
    pub application_id: String,
    /// The application's Ed25519 public key, hex.
    pub public_key: String,
    pub bot_token: String,
    pub client_secret: String,
    pub api_base: String,
    pub authorize_url: String,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("application_id", &self.application_id)
            .field("public_key", &self.public_key)
            .field("api_base", &self.api_base)
            .field("authorize_url", &self.authorize_url)
            .finish_non_exhaustive()
    }
}

impl DiscordConfig {
    /// From `ROLL_DISCORD_APPLICATION_ID`, `ROLL_DISCORD_PUBLIC_KEY`, `ROLL_DISCORD_BOT_TOKEN`
    /// and `ROLL_DISCORD_CLIENT_SECRET` (all four, or none: `Ok(None)`, Discord isn't
    /// configured), and optionally `ROLL_DISCORD_API_BASE` and `ROLL_DISCORD_AUTHORIZE_URL`.
    pub fn from_env() -> Result<Option<DiscordConfig>, String> {
        Self::from_lookup(crate::env)
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<DiscordConfig>, String> {
        let required = [
            "ROLL_DISCORD_APPLICATION_ID",
            "ROLL_DISCORD_PUBLIC_KEY",
            "ROLL_DISCORD_BOT_TOKEN",
            "ROLL_DISCORD_CLIENT_SECRET",
        ];
        let Some(v) = crate::all_or_none(&get, &required)? else {
            return Ok(None);
        };
        let [application_id, public_key, bot_token, client_secret] =
            <[String; 4]>::try_from(v).expect("all_or_none returns one value per name");
        if hex::decode(public_key.trim()).map(|k| k.len()) != Ok(32) {
            return Err("ROLL_DISCORD_PUBLIC_KEY must be 64 hex characters".into());
        }
        Ok(Some(DiscordConfig {
            application_id,
            public_key: public_key.trim().to_owned(),
            bot_token,
            client_secret,
            api_base: get("ROLL_DISCORD_API_BASE")
                .map(|s| s.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| DEFAULT_API_BASE.into()),
            authorize_url: get("ROLL_DISCORD_AUTHORIZE_URL")
                .unwrap_or_else(|| DEFAULT_AUTHORIZE_URL.into()),
        }))
    }

    /// Where "Add to Discord" sends a person. `state` must be single-use and tied to their roll
    /// session; `redirect_uri` must be one of the application's registered redirects.
    pub fn install_url(&self, state: &str, redirect_uri: &str) -> String {
        format!(
            "{}?client_id={}&scope=bot%20applications.commands&permissions={BOT_PERMISSIONS}&response_type=code&redirect_uri={}&state={}",
            self.authorize_url,
            crate::percent_encode(&self.application_id),
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

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn config_is_all_or_none_and_redacted() {
        assert!(DiscordConfig::from_lookup(lookup(&[])).unwrap().is_none());
        let err =
            DiscordConfig::from_lookup(lookup(&[("ROLL_DISCORD_BOT_TOKEN", "t")])).unwrap_err();
        assert!(
            err.contains("ROLL_DISCORD_APPLICATION_ID")
                && err.contains("ROLL_DISCORD_CLIENT_SECRET"),
            "{err}"
        );
        let full = [
            ("ROLL_DISCORD_APPLICATION_ID", "1234"),
            ("ROLL_DISCORD_PUBLIC_KEY", KEY),
            ("ROLL_DISCORD_BOT_TOKEN", "bot-token-value"),
            ("ROLL_DISCORD_CLIENT_SECRET", "client-secret-value"),
        ];
        let c = DiscordConfig::from_lookup(lookup(&full)).unwrap().unwrap();
        assert_eq!(c.api_base, DEFAULT_API_BASE);
        let debug = format!("{c:?}");
        assert!(
            !debug.contains("bot-token-value") && !debug.contains("client-secret-value"),
            "{debug}"
        );
        let mut bad = full;
        bad[1].1 = "nothex";
        assert!(
            DiscordConfig::from_lookup(lookup(&bad))
                .unwrap_err()
                .contains("PUBLIC_KEY")
        );
    }

    #[test]
    fn install_url() {
        let c = DiscordConfig {
            application_id: "1234".into(),
            public_key: KEY.into(),
            bot_token: "t".into(),
            client_secret: "s".into(),
            api_base: DEFAULT_API_BASE.into(),
            authorize_url: DEFAULT_AUTHORIZE_URL.into(),
        };
        let url = c.install_url("s 1", "https://roll.sh/discord/callback");
        assert_eq!(
            url,
            format!(
                "https://discord.com/oauth2/authorize?client_id=1234&scope=bot%20applications.commands&permissions={BOT_PERMISSIONS}&response_type=code&redirect_uri=https%3A%2F%2Froll.sh%2Fdiscord%2Fcallback&state=s%201"
            )
        );
        assert_eq!(BOT_PERMISSIONS, 311_385_213_952);
    }
}
