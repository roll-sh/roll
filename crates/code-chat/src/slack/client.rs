//! The Web API calls roll makes. Methods take JSON with the workspace's bot token as a bearer,
//! answer `{ok: true, …}` or `{ok: false, error: "<code>"}` (both HTTP 200), and answer 429 with
//! `Retry-After` (seconds) when rate limited. The client doesn't retry: callers queue, and
//! [`Error::transient`] says whether to try again.

use crate::{Error, read_bounded};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// What a workspace install yields.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthAccess {
    /// The bot token, `xoxb-…`.
    pub access_token: String,
    pub bot_user_id: String,
    pub app_id: String,
    pub team_id: String,
    pub team_name: String,
    pub enterprise_id: Option<String>,
    /// The Slack user who installed the app (`authed_user.id`).
    pub installer: String,
    /// The granted scopes, comma-separated.
    pub scope: String,
}

impl std::fmt::Debug for OAuthAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthAccess")
            .field("bot_user_id", &self.bot_user_id)
            .field("app_id", &self.app_id)
            .field("team_id", &self.team_id)
            .field("team_name", &self.team_name)
            .field("enterprise_id", &self.enterprise_id)
            .field("installer", &self.installer)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// A message to post or an update to one. `text` is the notification and accessibility
/// fallback; `blocks` (from [`super::blocks`]) is what clients show.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub channel: String,
    pub text: String,
    pub blocks: Option<Value>,
    /// Reply in this thread.
    pub thread_ts: Option<String>,
}

/// A message read back by [`Client::fetch_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// `None` for a bot's message.
    pub user: Option<String>,
    pub text: String,
    pub thread_ts: Option<String>,
}

#[derive(Deserialize)]
struct FetchedRaw {
    #[serde(default)]
    ts: String,
    user: Option<String>,
    #[serde(default)]
    text: String,
    thread_ts: Option<String>,
}

impl FetchedRaw {
    fn into_fetched(self) -> Fetched {
        Fetched {
            user: self.user,
            text: self.text,
            thread_ts: self.thread_ts,
        }
    }
}

/// A posted message: its channel (a DM's real id when posted by user id) and `ts`, its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub channel: String,
    pub ts: String,
}

#[derive(Clone)]
pub struct Client {
    api_base: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("slack::Client")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct PostedAnswer {
    channel: String,
    ts: String,
}

impl Client {
    pub fn new(api_base: &str) -> Client {
        Client {
            api_base: api_base.trim_end_matches('/').to_owned(),
            http: crate::http_client(),
        }
    }

    /// Finish an install: trade the redirect's `code` for the workspace's bot token. The team
    /// comes from Slack's answer, never from the redirect.
    pub async fn oauth_access(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<OAuthAccess, Error> {
        #[derive(Deserialize)]
        struct Named {
            id: String,
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize)]
        struct AuthedUser {
            id: String,
        }
        #[derive(Deserialize)]
        struct Answer {
            access_token: String,
            #[serde(default)]
            token_type: String,
            #[serde(default)]
            scope: String,
            bot_user_id: Option<String>,
            #[serde(default)]
            app_id: String,
            team: Option<Named>,
            enterprise: Option<Named>,
            authed_user: AuthedUser,
        }
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", code)
            .append_pair("client_id", client_id)
            .append_pair("client_secret", client_secret)
            .append_pair("redirect_uri", redirect_uri)
            .finish();
        let req = self
            .http
            .post(format!("{}/oauth.v2.access", self.api_base))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form);
        let a: Answer = self.send(req).await?;
        if a.token_type != "bot" {
            return Err(Error::Decode(format!("token_type {:?}", a.token_type)));
        }
        let Some(team) = a.team else {
            return Err(Error::Api("org_install_unsupported".into()));
        };
        let bot_user_id = a
            .bot_user_id
            .ok_or_else(|| Error::Decode("no bot_user_id".into()))?;
        Ok(OAuthAccess {
            access_token: a.access_token,
            bot_user_id,
            app_id: a.app_id,
            team_id: team.id,
            team_name: team.name,
            enterprise_id: a.enterprise.map(|e| e.id),
            installer: a.authed_user.id,
            scope: a.scope,
        })
    }

    /// `chat.postMessage`. Links aren't unfurled: status cards carry their own.
    pub async fn post_message(&self, token: &str, m: &Message) -> Result<Posted, Error> {
        let mut body = json!({
            "channel": m.channel,
            "text": m.text,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(blocks) = &m.blocks {
            body["blocks"] = blocks.clone();
        }
        if let Some(ts) = &m.thread_ts {
            body["thread_ts"] = json!(ts);
        }
        let p: PostedAnswer = self.call(token, "chat.postMessage", &body).await?;
        Ok(Posted {
            channel: p.channel,
            ts: p.ts,
        })
    }

    /// `chat.update`: replace the text and blocks of the message `ts` in `channel`
    /// (`m.channel` and `m.thread_ts` are ignored; a message can't move).
    pub async fn update_message(
        &self,
        token: &str,
        channel: &str,
        ts: &str,
        m: &Message,
    ) -> Result<Posted, Error> {
        let body = json!({
            "channel": channel,
            "ts": ts,
            "text": m.text,
            // An empty list clears blocks; a missing one would keep the old buttons.
            "blocks": m.blocks.clone().unwrap_or_else(|| json!([])),
        });
        let p: PostedAnswer = self.call(token, "chat.update", &body).await?;
        Ok(Posted {
            channel: p.channel,
            ts: p.ts,
        })
    }

    /// `chat.postEphemeral`: a message only `user` sees, in `channel`.
    pub async fn post_ephemeral(
        &self,
        token: &str,
        channel: &str,
        user: &str,
        text: &str,
    ) -> Result<(), Error> {
        let body = json!({ "channel": channel, "user": user, "text": text });
        self.call::<Value>(token, "chat.postEphemeral", &body)
            .await?;
        Ok(())
    }

    /// `conversations.open`: the DM channel with `user`.
    pub async fn open_dm(&self, token: &str, user: &str) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Channel {
            id: String,
        }
        #[derive(Deserialize)]
        struct Answer {
            channel: Channel,
        }
        let a: Answer = self
            .call(token, "conversations.open", &json!({ "users": user }))
            .await?;
        Ok(a.channel.id)
    }

    /// `views.open`: show a modal. `trigger_id` is from an interaction and lives 3 seconds.
    pub async fn open_view(
        &self,
        token: &str,
        trigger_id: &str,
        view: &Value,
    ) -> Result<(), Error> {
        let body = json!({ "trigger_id": trigger_id, "view": view });
        self.call::<Value>(token, "views.open", &body).await?;
        Ok(())
    }

    /// `reactions.add`. Reacting twice is fine.
    pub async fn add_reaction(
        &self,
        token: &str,
        channel: &str,
        ts: &str,
        name: &str,
    ) -> Result<(), Error> {
        let body = json!({ "channel": channel, "timestamp": ts, "name": name });
        match self.call::<Value>(token, "reactions.add", &body).await {
            Ok(_) => Ok(()),
            Err(Error::Api(code)) if code == "already_reacted" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `chat.getPermalink`.
    pub async fn permalink(&self, token: &str, channel: &str, ts: &str) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Answer {
            permalink: String,
        }
        let a: Answer = self
            .get(
                token,
                "chat.getPermalink",
                &[("channel", channel), ("message_ts", ts)],
            )
            .await?;
        Ok(a.permalink)
    }

    /// One message by `ts`: from `conversations.history` (a top-level message), else from
    /// `conversations.replies` (a reply in a thread). `None` when neither has it.
    pub async fn fetch_message(
        &self,
        token: &str,
        channel: &str,
        ts: &str,
    ) -> Result<Option<Fetched>, Error> {
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            messages: Vec<FetchedRaw>,
        }
        let history: Answer = self
            .get(
                token,
                "conversations.history",
                &[
                    ("channel", channel),
                    ("latest", ts),
                    ("inclusive", "true"),
                    ("limit", "1"),
                ],
            )
            .await?;
        let pick = |a: Answer| {
            a.messages
                .into_iter()
                .find(|m| m.ts == ts)
                .map(FetchedRaw::into_fetched)
        };
        if let Some(m) = pick(history) {
            return Ok(Some(m));
        }
        let replies: Answer = self
            .get(
                token,
                "conversations.replies",
                &[("channel", channel), ("ts", ts)],
            )
            .await?;
        Ok(pick(replies))
    }

    /// A GET method with its arguments in the query string.
    async fn get<T: DeserializeOwned>(
        &self,
        token: &str,
        method: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let query: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", crate::percent_encode(v)))
            .collect();
        let req = self
            .http
            .get(format!("{}/{method}?{}", self.api_base, query.join("&")))
            .header(AUTHORIZATION, format!("Bearer {token}"));
        self.send(req).await
    }

    async fn call<T: DeserializeOwned>(
        &self,
        token: &str,
        method: &str,
        body: &Value,
    ) -> Result<T, Error> {
        let req = self
            .http
            .post(format!("{}/{method}", self.api_base))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body.to_string());
        self.send(req).await
    }

    async fn send<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, Error> {
        let res = req.send().await.map_err(Error::transport)?;
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after_s = res
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(60);
            return Err(Error::RateLimited { retry_after_s });
        }
        if status.is_server_error() {
            return Err(Error::Http(format!("slack answered {}", status.as_u16())));
        }
        let body = read_bounded(res).await?;
        let v: Value = serde_json::from_slice(&body).map_err(|_| {
            if status.is_success() {
                Error::Decode("not json".into())
            } else {
                Error::Api(format!("http_{}", status.as_u16()))
            }
        })?;
        if v.get("ok") != Some(&Value::Bool(true)) {
            let code = v
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("http_{}", status.as_u16()));
            return Err(Error::Api(code));
        }
        serde_json::from_value(v).map_err(crate::shape_error)
    }
}
