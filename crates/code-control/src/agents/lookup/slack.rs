//! Slack's lookups, through the owner's installs (each workspace's sealed bot token,
//! `Install::token`): `workspaces` (the installs themselves), `channels` (public ones and the
//! private ones the bot is in), `users`, `usergroups`, and `targets` (channels and people in one
//! list, for a field that takes either).
//!
//! Slack can't search channels or people, so a workspace's whole list is read once (Slack's
//! cursor, [`PAGE`] rows a call, at most [`MAX_ROWS`]) and kept for the cache's TTL; searches,
//! pages and stored values are answered from it ([`super::page_of`]).
//!
//! `parent` names the workspace (its team id, or the install's id). Without one, the owner's only
//! workspace, or every workspace's rows grouped under its name.

use super::{Failure, Item, Known, Page, Problem, Query, page_of};
use crate::Code;
use crate::chatops::{self, Install, Platform};
use code_chat::slack::{Channel, Client, Error, User, UserGroup};
use std::sync::Arc;

pub const KINDS: &[&str] = &["workspaces", "channels", "users", "usergroups", "targets"];

/// Rows asked for per call (Slack's ceiling).
const PAGE: usize = code_chat::slack::MAX_LIST_PAGE;
/// The most rows of one list read from a workspace.
const MAX_ROWS: usize = 2000;
const SOURCE: &str = "slack";

fn problem(e: Error) -> Failure {
    Failure::Problem(match e {
        Error::RateLimited { retry_after_s } => Problem::RateLimited { retry_after: retry_after_s },
        Error::Api(code) => match code.as_str() {
            "missing_scope" => Problem::Reconnect { message: "Reconnect Slack: roll needs permission to list this".into() },
            "not_authed" | "invalid_auth" | "account_inactive" | "token_revoked" | "token_expired" | "no_permission" | "not_allowed_token_type" => {
                Problem::Reconnect { message: "Slack needs connecting again".into() }
            }
            "paid_teams_only" | "plan_upgrade_required" => Problem::Unavailable { message: "User groups need a paid Slack plan".into() },
            _ => Problem::Unavailable { message: format!("Slack answered {code}") },
        },
        e => Problem::Unavailable { message: e.to_string() },
    })
}

/// The installs a query is about: the one `parent` names, else all of the owner's.
fn installs(code: &Code, owner: &str, q: &Query) -> Result<Vec<Install>, Failure> {
    if code.slack().is_none() {
        return Err(Problem::NotConnected { message: "Slack isn't set up on this server".into() }.into());
    }
    let all = chatops::installs(code, owner, Platform::Slack)?;
    if all.is_empty() {
        return Err(Problem::NotConnected { message: "Slack isn't connected".into() }.into());
    }
    match q.parent.as_deref() {
        Some(p) => {
            let one = all.into_iter().find(|i| i.team_id == p || i.id == p).ok_or_else(|| crate::Error::NotFound(format!("Slack workspace {p}")))?;
            Ok(vec![one])
        }
        None => Ok(all),
    }
}

pub async fn list(code: &Code, owner: &str, kind: &str, q: &Query) -> Result<Page, Failure> {
    if kind == "workspaces" {
        let mut q = q.clone();
        q.parent = None;
        let rows: Vec<Known> = installs(code, owner, &q)?.iter().map(|i| Known::listed(Item::new(&i.team_id, &i.team_name).key(Some(&i.id)).hint(Some(&i.team_id)).icon(Some("team")))).collect();
        return Ok(page_of(&rows, &q));
    }
    let installs = installs(code, owner, q)?;
    let several = installs.len() > 1;
    let mut rows = Vec::new();
    let mut failed = None;
    let mut worked = false;
    for i in &installs {
        let at = several.then_some(i.team_name.as_str());
        let got = match kind {
            "targets" => match (rows_of(code, owner, i, "channels").await, rows_of(code, owner, i, "users").await) {
                (Ok(c), Ok(u)) => Ok(regroup(&c, &label(at, "Channels")).into_iter().chain(regroup(&u, &label(at, "People"))).collect::<Vec<_>>()),
                (Err(e), _) | (_, Err(e)) => Err(e),
            },
            _ => rows_of(code, owner, i, kind).await.map(|r| match at {
                Some(name) => regroup(&r, name),
                None => r.to_vec(),
            }),
        };
        match got {
            Ok(r) => {
                worked = true;
                rows.extend(r);
            }
            // A rate limit holds off every workspace's lookups.
            Err(Failure::Problem(p @ Problem::RateLimited { .. })) => return Err(p.into()),
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    match failed {
        Some(e) if !worked => Err(e),
        _ => Ok(page_of(&rows, q)),
    }
}

fn label(workspace: Option<&str>, what: &str) -> String {
    match workspace {
        Some(w) => format!("{what} · {w}"),
        None => what.to_string(),
    }
}

fn regroup(rows: &[Known], group: &str) -> Vec<Known> {
    rows.iter().map(|r| Known { item: r.item.clone().group(Some(group)), listed: r.listed }).collect()
}

/// A workspace's whole list of `kind`, from the cache or Slack.
async fn rows_of(code: &Code, owner: &str, i: &Install, kind: &str) -> Result<Arc<Vec<Known>>, Failure> {
    let name = format!("{}/{kind}", i.team_id);
    if let Some(rows) = code.agents.lookups.list(owner, SOURCE, &name) {
        return Ok(rows);
    }
    let token = i.token(code).ok_or(Problem::Reconnect { message: "Slack needs connecting again".into() })?;
    let (_, client) = code.slack().ok_or(Problem::NotConnected { message: "Slack isn't set up on this server".into() })?;
    let rows = Arc::new(read(client, &token, kind).await.map_err(problem)?);
    code.agents.lookups.keep_list(owner, SOURCE, &name, rows.clone());
    Ok(rows)
}

async fn read(client: &Client, token: &str, kind: &str) -> Result<Vec<Known>, Error> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    match kind {
        "channels" => loop {
            let page = client.list_channels(token, cursor.as_deref(), PAGE).await?;
            rows.extend(page.items.iter().map(|c| Known::listed(channel(c))));
            cursor = page.next;
            if cursor.is_none() || rows.len() >= MAX_ROWS {
                // The bot's own channels first (it can post there), then by name.
                rows.sort_by_key(|r: &Known| (r.item.hint.as_deref() == Some(NOT_A_MEMBER), r.item.label.to_lowercase()));
                break;
            }
        },
        "users" => loop {
            let page = client.list_users(token, cursor.as_deref(), PAGE).await?;
            rows.extend(page.items.iter().map(user));
            cursor = page.next;
            if cursor.is_none() || rows.len() >= MAX_ROWS {
                rows.sort_by_key(|r: &Known| r.item.label.to_lowercase());
                break;
            }
        },
        "usergroups" => {
            rows = client.list_usergroups(token).await?.iter().take(MAX_ROWS).map(|g| Known::listed(usergroup(g))).collect();
            rows.sort_by_key(|r| r.item.label.to_lowercase());
        }
        _ => unreachable!("known kinds only"),
    }
    rows.truncate(MAX_ROWS);
    Ok(rows)
}

const NOT_A_MEMBER: &str = "not a member";

fn channel(c: &Channel) -> Item {
    let hint = if !c.is_member {
        Some(NOT_A_MEMBER.to_string())
    } else {
        c.num_members.map(|n| if n == 1 { "1 member".to_string() } else { format!("{n} members") })
    };
    Item::new(&c.id, &c.name).key(Some(&c.name)).hint(hint).icon(Some(if c.is_private { "private" } else { "channel" }))
}

/// A person; deactivated people and bots only resolve stored values.
fn user(u: &User) -> Known {
    let nonempty = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    let name = nonempty(&u.real_name).or(nonempty(&u.profile.real_name)).or(nonempty(&u.profile.display_name)).unwrap_or_else(|| u.name.clone());
    let hint = if u.deleted { "deactivated".to_string() } else if u.is_bot { "bot".to_string() } else { format!("@{}", u.name) };
    let item = Item::new(&u.id, name).key(Some(&u.name)).hint(Some(hint)).avatar(u.profile.image_48.as_deref()).icon(Some("user"));
    Known { item, listed: !u.deleted && !u.is_bot && u.id != "USLACKBOT" }
}

fn usergroup(g: &UserGroup) -> Item {
    Item::new(&g.id, &g.name).key(Some(&g.handle)).hint(Some(format!("@{}", g.handle))).icon(Some("group"))
}
