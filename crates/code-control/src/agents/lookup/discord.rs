//! Discord's lookups, as roll's bot (the server's one bot token, `Code::discord`), only in the
//! servers the owner installed it in: `guilds` (the installs themselves), `channels` (text,
//! announcement and forum channels, grouped by category), `roles` and `members`.
//!
//! A server's channels and roles come whole (Discord doesn't page them) and are kept for the
//! cache's TTL, then searched here ([`super::page_of`]). Members are searched on Discord's side
//! (a prefix of the username or nickname) and paged by user id; both need the application's
//! Server Members Intent.
//!
//! `parent` names the server (its guild id, or the install's id); a server the owner hasn't
//! installed roll in is a 404. Without one, the owner's only server, or every server's rows
//! grouped under its name.

use super::{Failure, Item, Known, MAX_IDS, Page, Problem, Query, page_of};
use crate::Code;
use crate::chatops::{self, Install, Platform};
use code_chat::discord::{Channel, Client, Error, Member, Role};
use std::sync::Arc;

pub const KINDS: &[&str] = &["guilds", "channels", "roles", "members"];

const SOURCE: &str = "discord";
/// The channel types a message can go to: text, announcement, forum.
const POSTABLE: &[u64] = &[0, 5, 15];
const CATEGORY: u64 = 4;

fn problem(kind: &str, e: Error) -> Failure {
    Failure::Problem(match e {
        Error::RateLimited { retry_after_s } => Problem::RateLimited { retry_after: retry_after_s },
        Error::Api(detail) => {
            let code = detail.split(':').next().unwrap_or_default();
            match code {
                "50001" if kind == "members" => Problem::Unavailable { message: "Listing members needs the Server Members Intent on roll's Discord app".into() },
                "50001" | "50013" | "10004" => Problem::Reconnect { message: "Add roll's bot to the Discord server again".into() },
                _ if detail.contains("401") => Problem::Unavailable { message: "Discord refused roll's bot".into() },
                _ => Problem::Unavailable { message: format!("Discord answered {detail}") },
            }
        }
        e => Problem::Unavailable { message: e.to_string() },
    })
}

fn installs(code: &Code, owner: &str, q: &Query) -> Result<Vec<Install>, Failure> {
    if code.discord().is_none() {
        return Err(Problem::NotConnected { message: "Discord isn't set up on this server".into() }.into());
    }
    let all = chatops::installs(code, owner, Platform::Discord)?;
    if all.is_empty() {
        return Err(Problem::NotConnected { message: "Discord isn't connected".into() }.into());
    }
    match q.parent.as_deref() {
        // Only a server the owner installed roll in: never one a caller names.
        Some(p) => Ok(vec![all.into_iter().find(|i| i.team_id == p || i.id == p).ok_or_else(|| crate::Error::NotFound(format!("Discord server {p}")))?]),
        None => Ok(all),
    }
}

pub async fn list(code: &Code, owner: &str, kind: &str, q: &Query) -> Result<Page, Failure> {
    if kind == "guilds" {
        let mut q = q.clone();
        q.parent = None;
        let rows: Vec<Known> = installs(code, owner, &q)?.iter().map(|i| Known::listed(Item::new(&i.team_id, &i.team_name).key(Some(&i.id)).hint(Some(&i.team_id)).icon(Some("team")))).collect();
        return Ok(page_of(&rows, &q));
    }
    let installs = installs(code, owner, q)?;
    let (_, client) = code.discord().ok_or(Problem::NotConnected { message: "Discord isn't set up on this server".into() })?;
    let several = installs.len() > 1;
    if kind == "members" {
        let mut items = Vec::new();
        let mut next = None;
        for i in &installs {
            let mut p = members(client, &i.team_id, q).await.map_err(|e| problem(kind, e))?;
            if several {
                p.items.iter_mut().for_each(|it| it.group = Some(i.team_name.clone()));
            } else {
                next = p.next;
            }
            items.extend(p.items);
        }
        return Ok(Page { items, next, problem: None });
    }
    let mut rows = Vec::new();
    for i in &installs {
        let r = whole(code, owner, client, i, kind).await?;
        match several {
            true => rows.extend(r.iter().map(|k| {
                let group = match &k.item.group {
                    Some(c) => format!("{} · {c}", i.team_name),
                    None => i.team_name.clone(),
                };
                Known { item: k.item.clone().group(Some(group)), listed: k.listed }
            })),
            false => rows.extend(r.iter().cloned()),
        }
    }
    Ok(page_of(&rows, q))
}

/// A server's whole list of channels or roles, from the cache or Discord.
async fn whole(code: &Code, owner: &str, client: &Client, i: &Install, kind: &str) -> Result<Arc<Vec<Known>>, Failure> {
    let name = format!("{}/{kind}", i.team_id);
    if let Some(rows) = code.agents.lookups.list(owner, SOURCE, &name) {
        return Ok(rows);
    }
    let rows = Arc::new(match kind {
        "channels" => channels(&client.guild_channels(&i.team_id).await.map_err(|e| problem(kind, e))?),
        "roles" => roles(&i.team_id, &client.guild_roles(&i.team_id).await.map_err(|e| problem(kind, e))?),
        _ => unreachable!("known kinds only"),
    });
    code.agents.lookups.keep_list(owner, SOURCE, &name, rows.clone());
    Ok(rows)
}

/// The channels a message can go to, under their category, in Discord's order.
fn channels(all: &[Channel]) -> Vec<Known> {
    let category = |id: &Option<String>| id.as_ref().and_then(|id| all.iter().find(|c| c.kind == CATEGORY && &c.id == id));
    let mut rows: Vec<(i64, i64, Item)> = all
        .iter()
        .filter(|c| POSTABLE.contains(&c.kind))
        .map(|c| {
            let cat = category(&c.parent_id);
            let name = c.name.clone().unwrap_or_else(|| c.id.clone());
            let hint = match c.kind {
                5 => Some("announcements"),
                15 => Some("forum"),
                _ => None,
            };
            let item = Item::new(&c.id, &name).key(Some(&name)).hint(hint).group(cat.and_then(|c| c.name.clone())).icon(Some("channel"));
            // Channels outside a category come first, as Discord shows them.
            (cat.map_or(-1, |c| c.position.unwrap_or(0)), c.position.unwrap_or(0), item)
        })
        .collect();
    rows.sort_by_key(|(cat, pos, _)| (*cat, *pos));
    rows.into_iter().map(|(_, _, i)| Known::listed(i)).collect()
}

/// Roles, highest first; `@everyone` isn't one to pick, a bot's own role only resolves.
fn roles(guild: &str, all: &[Role]) -> Vec<Known> {
    let mut rs: Vec<&Role> = all.iter().filter(|r| r.id != guild).collect();
    rs.sort_by_key(|r| std::cmp::Reverse(r.position));
    rs.iter()
        .map(|r| {
            let color = (r.color != 0).then(|| format!("#{:06x}", r.color & 0xff_ffff));
            let item = Item::new(&r.id, &r.name).key(Some(&r.name)).color(color.as_deref()).icon(Some("role")).hint(r.managed.then_some("bot"));
            Known { item, listed: !r.managed }
        })
        .collect()
}

fn is_snowflake(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
}

/// A page of members: stored ids one by one, a search on Discord's side, or the list by user id.
async fn members(client: &Client, guild: &str, q: &Query) -> Result<Page, Error> {
    if !q.ids.is_empty() {
        let mut items = Vec::new();
        for id in q.ids.iter().filter(|i| is_snowflake(i)).take(MAX_IDS) {
            if let Some(m) = client.guild_member(guild, id).await? {
                items.push(member(&m));
            }
        }
        return Ok(Page { items, ..Page::default() });
    }
    if !q.q.is_empty() {
        let found = client.search_guild_members(guild, q.q.trim_start_matches('@'), q.limit).await?;
        return Ok(Page { items: found.iter().filter(|m| !m.user.bot).map(member).collect(), ..Page::default() });
    }
    let after = q.cursor.as_deref().filter(|c| is_snowflake(c));
    let page = client.list_guild_members(guild, after, q.limit).await?;
    let next = (page.len() >= q.limit).then(|| page.last().map(|m| m.user.id.clone())).flatten();
    Ok(Page { items: page.iter().filter(|m| !m.user.bot).map(member).collect(), next, problem: None })
}

fn member(m: &Member) -> Item {
    let u = &m.user;
    let name = m.nick.clone().or(u.global_name.clone()).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| u.username.clone());
    let avatar = u
        .avatar
        .as_deref()
        .filter(|h| !h.is_empty() && h.len() <= 64 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .filter(|_| is_snowflake(&u.id))
        .map(|h| format!("https://cdn.discordapp.com/avatars/{}/{h}.png?size=64", u.id));
    Item::new(&u.id, name).key(Some(&u.username)).hint(Some(format!("@{}", u.username))).avatar(avatar.as_deref()).icon(Some("user"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_chat::discord::MemberUser;

    #[test]
    fn channels_are_the_postable_ones_under_their_category() {
        let ch = |id: &str, kind, name: &str, parent: Option<&str>, position| Channel { id: id.into(), kind, name: Some(name.into()), parent_id: parent.map(Into::into), position: Some(position) };
        let all = [
            ch("1", CATEGORY, "Work", None, 1),
            ch("2", 0, "deploys", Some("1"), 2),
            ch("3", 2, "voice", Some("1"), 0),
            ch("4", 0, "welcome", None, 5),
            ch("5", 15, "ideas", Some("1"), 1),
            ch("6", 5, "news", Some("1"), 3),
        ];
        let rows = channels(&all);
        let names: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (r.item.label.as_str(), r.item.group.as_deref())).collect();
        assert_eq!(names, [("welcome", None), ("ideas", Some("Work")), ("deploys", Some("Work")), ("news", Some("Work"))]);
        assert_eq!((rows[1].item.hint.as_deref(), rows[3].item.hint.as_deref()), (Some("forum"), Some("announcements")));
    }

    #[test]
    fn roles_have_their_colour_and_everyone_is_not_one() {
        let r = |id: &str, name: &str, color, position, managed| Role { id: id.into(), name: name.into(), color, position, managed };
        let rows = roles("500", &[r("500", "@everyone", 0, 0, false), r("1", "Maintainers", 0x4c8dff, 2, false), r("2", "roll", 0, 3, true), r("3", "Triage", 0, 1, false)]);
        assert_eq!(rows.iter().map(|k| k.item.label.as_str()).collect::<Vec<_>>(), ["roll", "Maintainers", "Triage"]);
        assert!(!rows[0].listed, "a bot's own role");
        assert_eq!((rows[1].item.color.as_deref(), rows[2].item.color.as_deref()), (Some("#4c8dff"), None));
        assert_eq!(rows[1].item.icon.as_deref(), Some("role"));
    }

    #[test]
    fn members_have_their_nickname_and_a_cdn_avatar() {
        let m = |avatar: Option<&str>, nick: Option<&str>| Member { user: MemberUser { id: "7".into(), username: "ada".into(), global_name: Some("Ada".into()), avatar: avatar.map(Into::into), bot: false }, nick: nick.map(Into::into) };
        let i = member(&m(Some("a_1f2e"), Some("ada (ops)")));
        assert_eq!((i.label.as_str(), i.key.as_deref(), i.hint.as_deref()), ("ada (ops)", Some("ada"), Some("@ada")));
        assert_eq!(i.avatar.as_deref(), Some("https://cdn.discordapp.com/avatars/7/a_1f2e.png?size=64"));
        assert_eq!(member(&m(Some("../x"), None)).avatar, None);
        assert_eq!(member(&m(None, None)).label, "Ada");
    }

    #[test]
    fn discord_errors_are_quiet_problems() {
        let p = |kind: &str, e: Error| match problem(kind, e) {
            Failure::Problem(p) => p,
            Failure::Refused(_) => panic!("refused"),
        };
        assert!(matches!(p("channels", Error::Api("50001: Missing Access".into())), Problem::Reconnect { .. }));
        assert!(matches!(p("members", Error::Api("50001: Missing Access".into())), Problem::Unavailable { message } if message.contains("Members Intent")));
        assert_eq!(p("roles", Error::RateLimited { retry_after_s: 3 }), Problem::RateLimited { retry_after: 3 });
    }
}
