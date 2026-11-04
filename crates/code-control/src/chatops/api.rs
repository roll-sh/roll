//! Chat ops' HTTP (docs/code/http-api.md, "Chat ops"):
//!
//! - the owner's integrations under `/v1/code/agents/{owner}/integrations…` (routed to the
//!   owner's agents node like the rest of `/v1/code/agents/{owner}`; the owner manages them,
//!   agent tokens can't);
//! - the platforms' endpoints under `/v1/code/chat/…`, which any node answers: each request is
//!   verified (Slack's signing secret, Discord's Ed25519; stale timestamps refused), recorded in
//!   `chat_actions` once per platform id, acknowledged, and handled by the worker.

use super::{Platform, discord, install_by_team};
use crate::api::Body;
use crate::{Caller, Code, Error, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Code>>;

pub fn routes() -> Router<Arc<Code>> {
    Router::new()
        .route("/v1/code/agents/{owner}/integrations", get(list))
        .route("/v1/code/agents/{owner}/integrations/{platform}/install", post(install))
        .route("/v1/code/agents/{owner}/integrations/{platform}/{id}", patch(update).delete(remove))
        .route("/v1/code/chat/slack/oauth", get(slack_oauth))
        .route("/v1/code/chat/slack/events", post(slack_events))
        .route("/v1/code/chat/slack/commands", post(slack_commands))
        .route("/v1/code/chat/slack/interactions", post(slack_interactions))
        .route("/v1/code/chat/discord/oauth", get(discord_oauth))
        .route("/v1/code/chat/discord/interactions", post(discord_interactions))
}

fn platform(p: &str) -> Result<Platform> {
    Platform::parse(p).ok_or_else(|| Error::NotFound(format!("integration {p}")))
}

fn now_s() -> i64 {
    code_sql::now_ms() / 1000
}

// ── the owner's integrations ──

/// `GET …/integrations`: what's connected, and what the server can connect.
async fn list(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = crate::agents::api::manageable(&code, &caller, &owner)?;
    let views = |p: Platform| -> Result<Vec<Value>> { Ok(super::installs(&code, &o.id, p)?.iter().map(|i| i.view()).collect()) };
    let gh = crate::github::info(&code);
    let repos: Vec<Value> = crate::github::linked_repos(&code)?
        .into_iter()
        .filter(|r| r.owner_id == o.id)
        .filter_map(|r| {
            let l = crate::github::link_of(&r)?;
            Some(json!({ "repo": r.full_name(), "github": l.full_name(), "url": l.url, "connected": l.connected, "mirror": l.mirror.state }))
        })
        .collect();
    Ok(Json(json!({
        "slack": { "configured": code.slack().is_some(), "installs": views(Platform::Slack)? },
        "discord": { "configured": code.discord().is_some(), "installs": views(Platform::Discord)? },
        "github": { "configured": gh["configured"], "app_slug": gh["app_slug"], "install_url": gh["install_url"], "repos": repos },
        "sealing": code.ai.sealer.is_some(),
    })))
}

/// `POST …/integrations/{platform}/install`: a link that installs the app for this owner (once,
/// for ten minutes).
async fn install(State(code): S, caller: Caller, Path((owner, p)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = crate::agents::api::manageable(&code, &caller, &owner)?;
    let p = platform(&p)?;
    let redirect = super::redirect_uri(&code, p);
    let by = caller.identity().unwrap_or_else(|| o.name.clone());
    let url = match p {
        Platform::Slack => {
            let (cfg, _) = code.slack().ok_or_else(|| Error::Unprocessable("Slack isn't configured on this server (ROLL_SLACK_*)".into()))?;
            if code.ai.sealer.is_none() {
                return Err(Error::Unprocessable("connecting Slack needs ROLL_SECRET_KEY on the server".into()));
            }
            cfg.install_url(&super::new_oauth_state(&code, &o.id, p, &by)?, &redirect)
        }
        Platform::Discord => {
            let (cfg, _) = code.discord().ok_or_else(|| Error::Unprocessable("Discord isn't configured on this server (ROLL_DISCORD_*)".into()))?;
            cfg.install_url(&super::new_oauth_state(&code, &o.id, p, &by)?, &redirect)
        }
    };
    Ok(Json(json!({ "url": url })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersBody {
    #[serde(default)]
    members: Option<Vec<String>>,
    #[serde(default)]
    anyone: Option<bool>,
}

/// `PATCH …/integrations/{platform}/{id}`: who may drive roll from it.
async fn update(State(code): S, caller: Caller, Path((owner, p, id)): Path<(String, String, String)>, Body(b): Body) -> Result<Json<Value>> {
    let b: MembersBody = serde_json::from_value(b).map_err(|e| Error::Invalid(e.to_string()))?;
    let o = crate::agents::api::manageable(&code, &caller, &owner)?;
    let p = platform(&p)?;
    let i = super::install_by_id(&code, &id)?.filter(|i| i.owner_id == o.id && i.platform == p).ok_or_else(|| Error::NotFound(format!("integration {id}")))?;
    let mut members = b.members.unwrap_or(i.members.clone());
    members.retain(|m| !m.trim().is_empty());
    members.iter_mut().for_each(|m| *m = m.trim().to_string());
    members.dedup();
    if members.len() > 200 || members.iter().any(|m| m.len() > 64 || !m.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Err(Error::Invalid("members are at most 200 platform user ids (letters and digits)".into()));
    }
    super::update_members(&code, &i.id, &members, b.anyone.unwrap_or(i.anyone))?;
    let i = super::install_by_id(&code, &id)?.ok_or_else(|| Error::NotFound(format!("integration {id}")))?;
    Ok(Json(i.view()))
}

/// `DELETE …/integrations/{platform}/{id}`: forget it here (the app stays installed on the
/// platform until removed there).
async fn remove(State(code): S, caller: Caller, Path((owner, p, id)): Path<(String, String, String)>) -> Result<StatusCode> {
    let o = crate::agents::api::manageable(&code, &caller, &owner)?;
    let p = platform(&p)?;
    let i = super::install_by_id(&code, &id)?.filter(|i| i.owner_id == o.id && i.platform == p).ok_or_else(|| Error::NotFound(format!("integration {id}")))?;
    super::delete_install(&code, &i.id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── OAuth ──

#[derive(Deserialize)]
struct OAuthQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn back(code: &Code, query: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, super::settings_url(code, query))]).into_response()
}

fn query_value(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The end of an install: the state names the owner, the exchange names the team (never the
/// redirect's query), and the dashboard's settings show the result.
async fn finish(code: &Arc<Code>, p: Platform, q: OAuthQuery) -> Response {
    let name = p.as_str();
    let configured = match p {
        Platform::Slack => code.slack().is_some(),
        Platform::Discord => code.discord().is_some(),
    };
    if !configured {
        return Error::NotFound(name.into()).into_response();
    }
    if let Some(e) = q.error {
        return back(code, &format!("integration={name}&error={}", query_value(&e)));
    }
    let (Some(state), Some(grant)) = (q.state, q.code) else { return back(code, &format!("integration={name}&error=missing_code")) };
    let owner_id = match super::take_oauth_state(code, &state, p) {
        Ok(Some(o)) => o,
        Ok(None) => return back(code, &format!("integration={name}&error=expired")),
        Err(e) => return e.into_response(),
    };
    let redirect = super::redirect_uri(code, p);
    let owner = code.db.owner_by_id(&owner_id).ok().flatten().map(|o| o.name).unwrap_or_default();
    let failed = |e: String| back(code, &format!("owner={}&integration={name}&error={}", query_value(&owner), query_value(&e)));
    let new = match p {
        Platform::Slack => {
            let Some((cfg, client)) = code.slack() else { return Error::NotFound("Slack".into()).into_response() };
            match client.oauth_access(&cfg.client_id, &cfg.client_secret, &grant, &redirect).await {
                Ok(a) => super::NewInstall {
                    platform: p,
                    owner_id: owner_id.clone(),
                    team_id: a.team_id.clone(),
                    team_name: a.team_name.clone(),
                    bot_user_id: a.bot_user_id.clone(),
                    app_id: a.app_id.clone(),
                    token: Some(a.access_token.clone()),
                    installer: a.installer.clone(),
                },
                Err(e) => return failed(e.to_string()),
            }
        }
        Platform::Discord => {
            let Some((cfg, client)) = code.discord() else { return Error::NotFound("Discord".into()).into_response() };
            match client.oauth_guild(&cfg.application_id, &cfg.client_secret, &grant, &redirect).await {
                Ok((guild, guild_name)) => {
                    if let Err(e) = client.register_commands(&cfg.application_id, Some(&guild)).await {
                        tracing::warn!(error = %e, "registering /roll in a Discord server");
                    }
                    super::NewInstall {
                        platform: p,
                        owner_id: owner_id.clone(),
                        team_id: guild,
                        team_name: guild_name,
                        bot_user_id: cfg.application_id.clone(),
                        app_id: cfg.application_id.clone(),
                        token: None,
                        // Discord's bot install doesn't say who installed it: the owner adds members.
                        installer: String::new(),
                    }
                }
                Err(e) => return failed(e.to_string()),
            }
        }
    };
    match super::save_install(code, &new) {
        Ok(i) => back(code, &format!("owner={}&integration={name}&connected={}", query_value(&owner), query_value(&i.team_name))),
        Err(Error::Conflict(m)) => failed(m),
        Err(e) => e.into_response(),
    }
}

async fn slack_oauth(State(code): S, Query(q): Query<OAuthQuery>) -> Response {
    finish(&code, Platform::Slack, q).await
}

async fn discord_oauth(State(code): S, Query(q): Query<OAuthQuery>) -> Response {
    finish(&code, Platform::Discord, q).await
}

// ── Slack's requests ──

fn header<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok())
}

fn slack_verified(code: &Code, h: &HeaderMap, body: &[u8]) -> Result<()> {
    let (cfg, _) = code.slack().ok_or_else(|| Error::NotFound("Slack".into()))?;
    if body.len() > super::MAX_INBOUND {
        return Err(Error::Invalid("too large".into()));
    }
    code_chat::slack::verify(&cfg.signing_secret, header(h, "x-slack-request-timestamp"), header(h, "x-slack-signature"), body, now_s()).map_err(|_| Error::Unauthorized)
}

/// Record and start handling an action; a redelivery is just acknowledged.
fn take(code: &Arc<Code>, key: &str, install: &super::Install, kind: &str, body: &[u8]) -> Result<()> {
    if let Some((id, until)) = super::record_action(code, key, &install.id, kind, &String::from_utf8_lossy(body))? {
        super::spawn_action(code, id, until);
    }
    Ok(())
}

fn body_key(body: &[u8]) -> String {
    crate::hash_token(&String::from_utf8_lossy(body))[..32].to_string()
}

/// `POST /v1/code/chat/slack/events` (the Events API).
async fn slack_events(State(code): S, headers: HeaderMap, body: Bytes) -> Result<Response> {
    slack_verified(&code, &headers, &body)?;
    match code_chat::slack::parse_envelope(&body).map_err(Error::Invalid)? {
        code_chat::slack::Envelope::UrlVerification { challenge } => Ok(Json(json!({ "challenge": challenge })).into_response()),
        code_chat::slack::Envelope::Callback { team_id, event_id, .. } => {
            if let Some(i) = install_by_team(&code, Platform::Slack, &team_id)? {
                take(&code, &format!("slack:event:{event_id}"), &i, "slack.event", &body)?;
            }
            Ok(StatusCode::OK.into_response())
        }
        code_chat::slack::Envelope::Other(_) => Ok(StatusCode::OK.into_response()),
    }
}

fn ephemeral(text: &str) -> Response {
    Json(json!({ "response_type": "ephemeral", "text": text })).into_response()
}

/// `POST /v1/code/chat/slack/commands` (`/roll …`). Answered at once (an ephemeral line); the
/// run's messages come after.
async fn slack_commands(State(code): S, headers: HeaderMap, body: Bytes) -> Result<Response> {
    slack_verified(&code, &headers, &body)?;
    let c = code_chat::slack::parse_command(&body).map_err(Error::Invalid)?;
    let Some(i) = install_by_team(&code, Platform::Slack, &c.team_id)? else {
        return Ok(ephemeral("This workspace isn't connected to roll. The roll owner connects it in the agents settings."));
    };
    if !i.allows(&c.user_id) {
        return Ok(ephemeral("Only people the roll owner allows can use roll here."));
    }
    if c.text.trim().is_empty() {
        return Ok(ephemeral("Tell roll what to do: `/roll fix the flaky login test`."));
    }
    if !super::anyone_listens(&code, &i.owner_id, super::slack::SOURCE, "command") {
        return Ok(ephemeral("No automation answers /roll yet. In roll, add one from the template \"/roll in Slack\"."));
    }
    take(&code, &format!("slack:command:{}", c.trigger_id), &i, "slack.command", &body)?;
    Ok(ephemeral(&format!("Sent to roll: {}", code_chat::slack::escape(c.text.trim()))))
}

/// `POST /v1/code/chat/slack/interactions` (buttons, selects, modals).
async fn slack_interactions(State(code): S, headers: HeaderMap, body: Bytes) -> Result<Response> {
    slack_verified(&code, &headers, &body)?;
    let team = match code_chat::slack::parse_interaction(&body).map_err(Error::Invalid)? {
        code_chat::slack::Interaction::BlockActions(b) => b.team_id,
        code_chat::slack::Interaction::ViewSubmission(v) => v.team_id,
        code_chat::slack::Interaction::Other(_) => return Ok(StatusCode::OK.into_response()),
    };
    if let Some(i) = install_by_team(&code, Platform::Slack, &team)? {
        take(&code, &format!("slack:interaction:{}", body_key(&body)), &i, "slack.interaction", &body)?;
    }
    Ok(StatusCode::OK.into_response())
}

// ── Discord's ──

/// `POST /v1/code/chat/discord/interactions`.
async fn discord_interactions(State(code): S, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let (cfg, _) = code.discord().ok_or_else(|| Error::NotFound("Discord".into()))?;
    if body.len() > super::MAX_INBOUND {
        return Err(Error::Invalid("too large".into()));
    }
    code_chat::discord::verify(&cfg.public_key, header(&headers, "x-signature-ed25519"), header(&headers, "x-signature-timestamp"), &body, now_s()).map_err(|_| Error::Unauthorized)?;
    let i = code_chat::discord::parse_interaction(&body).map_err(Error::Invalid)?;
    let (id, guild) = discord::ids(&i);
    let install = match guild {
        Some(g) => install_by_team(&code, Platform::Discord, g)?,
        None => None,
    };
    let (answer, record) = discord::immediate(&code, install.as_ref(), &i);
    if record && let (Some(id), Some(install)) = (id, install.as_ref()) {
        take(&code, &format!("discord:{id}"), install, "discord.interaction", &body)?;
    }
    Ok(Json(answer).into_response())
}
