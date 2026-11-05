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

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Query {
        Query { q: text.into(), limit: 50, ..Query::default() }
    }

    #[test]
    fn channels_say_whether_the_bot_is_in_them() {
        let c = |id: &str, private, member, n| Channel { id: id.into(), name: id.to_lowercase(), is_private: private, is_member: member, num_members: n };
        let i = channel(&c("C1", false, true, Some(12)));
        assert_eq!((i.label.as_str(), i.key.as_deref(), i.hint.as_deref(), i.icon.as_deref()), ("c1", Some("c1"), Some("12 members"), Some("channel")));
        assert_eq!(channel(&c("C2", false, false, Some(3))).hint.as_deref(), Some(NOT_A_MEMBER));
        assert_eq!(channel(&c("G3", true, true, Some(1))).icon.as_deref(), Some("private"));
        assert_eq!(channel(&c("G3", true, true, Some(1))).hint.as_deref(), Some("1 member"));
    }

    #[test]
    fn people_have_names_handles_and_safe_pictures_and_bots_stay_out_of_the_list() {
        let mut u = User { id: "U1".into(), name: "ada".into(), real_name: Some("Ada Lovelace".into()), ..User::default() };
        u.profile.image_48 = Some("https://avatars.slack-edge.com/a.png".into());
        let k = user(&u);
        assert!(k.listed);
        assert_eq!((k.item.label.as_str(), k.item.hint.as_deref(), k.item.avatar.as_deref()), ("Ada Lovelace", Some("@ada"), Some("https://avatars.slack-edge.com/a.png")));
        u.profile.image_48 = Some("http://x/a.png".into());
        assert_eq!(user(&u).item.avatar, None);
        let bot = user(&User { id: "B1".into(), name: "deploy".into(), is_bot: true, ..User::default() });
        let gone = user(&User { id: "U2".into(), name: "eve".into(), deleted: true, ..User::default() });
        assert!(!bot.listed && !gone.listed);
        assert_eq!((bot.item.label.as_str(), gone.item.hint.as_deref()), ("deploy", Some("deactivated")));
        // Hidden from the list, still found by a stored value.
        let rows = vec![user(&u), bot, gone];
        assert_eq!(page_of(&rows, &q("")).items.len(), 1);
        let mut by_id = q("");
        by_id.ids = vec!["U2".into(), "@deploy".into()];
        assert_eq!(page_of(&rows, &by_id).items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["B1", "U2"]);
    }

    #[test]
    fn whole_lists_are_searched_and_paged_here() {
        let rows: Vec<Known> = (0..5).map(|n| Known::listed(channel(&Channel { id: format!("C{n}"), name: format!("team-{n}"), is_member: true, ..Channel::default() }))).collect();
        let mut first = q("#team");
        first.limit = 2;
        let p = page_of(&rows, &first);
        assert_eq!((p.items.len(), p.next.as_deref()), (2, Some("2")));
        first.cursor = Some("4".into());
        let p = page_of(&rows, &first);
        assert_eq!((p.items[0].id.as_str(), p.next), ("C4", None));
        assert_eq!(page_of(&rows, &q("TEAM-3")).items[0].id, "C3");
        let mut stored = q("");
        stored.ids = vec!["#team-1".into(), "C2".into()];
        assert_eq!(page_of(&rows, &stored).items.len(), 2);
        let g = usergroup(&UserGroup { id: "S1".into(), name: "On call".into(), handle: "oncall".into(), user_count: None });
        assert_eq!((g.hint.as_deref(), g.icon.as_deref()), (Some("@oncall"), Some("group")));
    }
}
