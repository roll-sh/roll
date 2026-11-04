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
