//! The REST calls roll makes, as the application's bot (`Authorization: Bot <token>`). Errors
//! answer with `{code, message}` and a 4xx; a 429 carries `retry_after` (seconds, a float) in
//! its body. The client doesn't retry: callers queue, and [`Error::transient`] says whether to
//! try again.

use crate::{Error, percent_encode, read_bounded};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// A message: its channel and id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub id: String,
}

/// The most members Discord answers in one page.
pub const MAX_MEMBERS_PAGE: usize = 1000;

/// A guild channel. `kind` is Discord's `type` (0 text, 2 voice, 4 category, 5 announcement,
/// 15 forum…).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Channel {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: u64,
    #[serde(default)]
    pub name: Option<String>,
    /// The category it's in.
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

/// A guild role. `color` is an RGB integer (0: none).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Role {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: u64,
    #[serde(default)]
    pub position: i64,
    /// A bot's or an integration's own role.
    #[serde(default)]
    pub managed: bool,
}

/// A guild member.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Member {
    pub user: MemberUser,
    /// The member's nickname in this guild.
    #[serde(default)]
    pub nick: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct MemberUser {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    /// The avatar's hash (`cdn.discordapp.com/avatars/{id}/{hash}.png`).
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

#[derive(Clone)]
pub struct Client {
    api_base: String,
    bot_token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("discord::Client")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct MessageAnswer {
    id: String,
    channel_id: String,
}

impl Client {
    pub fn new(api_base: &str, bot_token: &str) -> Client {
        Client {
            api_base: api_base.trim_end_matches('/').to_owned(),
            bot_token: bot_token.to_owned(),
            http: crate::http_client(),
        }
    }

    /// Finish a bot install: trade the redirect's `code` for the guild the bot joined. The guild
    /// comes from Discord's answer, never from the redirect. The user token it also returns is
    /// dropped.
    pub async fn oauth_guild(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<(String, String), Error> {
        #[derive(Deserialize)]
        struct Guild {
            id: String,
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize)]
        struct Answer {
            guild: Option<Guild>,
        }
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", client_id)
            .append_pair("client_secret", client_secret)
            .finish();
        let req = self
            .http
            .post(format!("{}/oauth2/token", self.api_base))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form);
        let a: Answer = send(req).await?;
        let guild = a.guild.ok_or_else(|| Error::Api("no_guild".into()))?;
        Ok((guild.id, guild.name))
    }

    /// `POST /channels/{channel}/messages`.
    pub async fn create_message(&self, channel: &str, payload: &Value) -> Result<Posted, Error> {
        let path = format!("/channels/{}/messages", percent_encode(channel));
        let m: MessageAnswer = self.call(Method::POST, &path, Some(payload)).await?;
        Ok(Posted {
            channel: m.channel_id,
            id: m.id,
        })
    }

    /// `PATCH /channels/{channel}/messages/{id}`: only the bot's own messages.
    pub async fn edit_message(
        &self,
        channel: &str,
        id: &str,
        payload: &Value,
    ) -> Result<Posted, Error> {
        let path = format!(
            "/channels/{}/messages/{}",
            percent_encode(channel),
            percent_encode(id)
        );
        let m: MessageAnswer = self.call(Method::PATCH, &path, Some(payload)).await?;
        Ok(Posted {
            channel: m.channel_id,
            id: m.id,
        })
    }

    /// The DM channel with `user` (`POST /users/@me/channels`). Fails when the user shares no
    /// guild with the bot or has DMs closed.
    pub async fn dm_channel(&self, user: &str) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Channel {
            id: String,
        }
        let body = json!({ "recipient_id": user });
        let c: Channel = self
            .call(Method::POST, "/users/@me/channels", Some(&body))
            .await?;
        Ok(c.id)
    }

    /// Install roll's slash commands, replacing any others: `/roll prompt:<text>`. Globally, or in
    /// one guild (which takes effect at once; global commands can take a while to appear).
    pub async fn register_commands(
        &self,
        application_id: &str,
        guild: Option<&str>,
    ) -> Result<(), Error> {
        let app = percent_encode(application_id);
        let path = match guild {
            Some(g) => format!("/applications/{app}/guilds/{}/commands", percent_encode(g)),
            None => format!("/applications/{app}/commands"),
        };
        let commands = json!([{
            "name": "roll",
            "type": 1,
            "description": "Ask a roll agent",
            "options": [{
                "type": 3,
                "name": "prompt",
                "description": "What the agent should do",
                "required": true,
            }],
        }]);
        self.call::<Value>(Method::PUT, &path, Some(&commands))
            .await?;
        Ok(())
    }

    /// `GET /guilds/{guild}/channels`: every channel of the guild the bot can see, categories
    /// included (Discord doesn't page them).
    pub async fn guild_channels(&self, guild: &str) -> Result<Vec<Channel>, Error> {
        let path = format!("/guilds/{}/channels", percent_encode(guild));
        self.call(Method::GET, &path, None).await
    }

    /// `GET /guilds/{guild}/roles`.
    pub async fn guild_roles(&self, guild: &str) -> Result<Vec<Role>, Error> {
        let path = format!("/guilds/{}/roles", percent_encode(guild));
        self.call(Method::GET, &path, None).await
    }

    /// `GET /guilds/{guild}/members/search`: members whose username or nickname starts with
    /// `query`, at most `limit` (clamped to 1..=[`MAX_MEMBERS_PAGE`]). Needs the application's
    /// Server Members Intent.
    pub async fn search_guild_members(
        &self,
        guild: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Member>, Error> {
        let path = format!(
            "/guilds/{}/members/search?query={}&limit={}",
            percent_encode(guild),
            percent_encode(query),
            limit.clamp(1, MAX_MEMBERS_PAGE)
        );
        self.call(Method::GET, &path, None).await
    }

    /// `GET /guilds/{guild}/members`: a page of members by user id, after `after`, at most
    /// `limit` (clamped to 1..=[`MAX_MEMBERS_PAGE`]). Needs the Server Members Intent.
    pub async fn list_guild_members(
        &self,
        guild: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Member>, Error> {
        let mut path = format!(
            "/guilds/{}/members?limit={}",
            percent_encode(guild),
            limit.clamp(1, MAX_MEMBERS_PAGE)
        );
        if let Some(a) = after.filter(|a| !a.is_empty()) {
            path.push_str(&format!("&after={}", percent_encode(a)));
        }
        self.call(Method::GET, &path, None).await
    }

    /// `GET /guilds/{guild}/members/{user}`: `None` when they aren't a member.
    pub async fn guild_member(&self, guild: &str, user: &str) -> Result<Option<Member>, Error> {
        let path = format!(
            "/guilds/{}/members/{}",
            percent_encode(guild),
            percent_encode(user)
        );
        match self.call(Method::GET, &path, None).await {
            Ok(m) => Ok(Some(m)),
            // Unknown Member, Unknown User.
            Err(Error::Api(e)) if e.starts_with("10007:") || e.starts_with("10013:") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, Error> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.api_base))
            .header(AUTHORIZATION, format!("Bot {}", self.bot_token));
        if let Some(body) = body {
            req = req
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        send(req).await
    }
}

async fn send<T: DeserializeOwned>(req: RequestBuilder) -> Result<T, Error> {
    let res = req.send().await.map_err(Error::transport)?;
    let status = res.status();
    let retry_header: Option<f64> = res
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    if status.is_server_error() {
        return Err(Error::Http(format!("discord answered {}", status.as_u16())));
    }
    let body = read_bounded(res).await?;
    if status == StatusCode::TOO_MANY_REQUESTS {
        let in_body = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("retry_after").and_then(Value::as_f64));
        let secs = in_body.or(retry_header).unwrap_or(1.0);
        return Err(Error::RateLimited {
            retry_after_s: secs.max(0.0).ceil() as u64,
        });
    }
    if !status.is_success() {
        #[derive(Deserialize)]
        struct Failure {
            code: Option<u64>,
            message: Option<String>,
            error: Option<String>,
        }
        let f: Option<Failure> = serde_json::from_slice(&body).ok();
        let detail = match f {
            Some(Failure {
                code: Some(code),
                message,
                ..
            }) => {
                format!(
                    "{code}: {}",
                    crate::truncate(&message.unwrap_or_default(), 200)
                )
            }
            // OAuth errors: `{error: "invalid_grant", …}`.
            Some(Failure { error: Some(e), .. }) => crate::truncate(&e, 100),
            _ => format!("http_{}", status.as_u16()),
        };
        return Err(Error::Api(detail));
    }
    if body.is_empty() || status == StatusCode::NO_CONTENT {
        return serde_json::from_value(Value::Null).map_err(crate::shape_error);
    }
    serde_json::from_slice(&body).map_err(crate::shape_error)
}
