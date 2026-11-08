//! roll code's own lookups for pickers: the owner's repos and its agents (the `agent:<name>`
//! tokens an automation can act as). Both come from control.db, so they're cheap and need no
//! connection; anything that reads a repo's content (branches, labels) goes through the repo's
//! own routes, which run on the repo's owner node.

use super::{Failure, Item, Page, Query};
use crate::Code;

pub const KINDS: &[&str] = &["repos", "agents"];

fn hit(q: &str, of: &[&str]) -> bool {
    q.is_empty() || of.iter().any(|s| s.to_lowercase().contains(&q.to_lowercase()))
}

pub async fn list(code: &Code, owner: &str, kind: &str, q: &Query) -> Result<Page, Failure> {
    let items: Vec<Item> = match kind {
        "repos" => code
            .db
            .repos(Some(owner))
            .map_err(crate::Error::from)?
            .into_iter()
            .filter(|r| if q.ids.is_empty() { hit(&q.q, &[&r.name, r.description.as_deref().unwrap_or("")]) } else { q.ids.iter().any(|i| i.eq_ignore_ascii_case(&r.name) || i.eq_ignore_ascii_case(&format!("{}/{}", r.owner, r.name))) })
            .map(|r| {
                let full = format!("{}/{}", r.owner, r.name);
                let gh = crate::github::link_of(&r).map(|l| format!("GitHub {}", l.full_name()));
                Item::new(r.name.clone(), r.name.clone()).key(Some(full)).hint(gh.or(r.description.clone())).icon(Some(if r.source == "github" { "github" } else { "repo" }))
            })
            .collect(),
        "agents" => code
            .db
            .tokens(owner)
            .map_err(crate::Error::from)?
            .into_iter()
            .filter_map(|t| t["name"].as_str().and_then(|n| n.strip_prefix("agent:")).map(str::to_string))
            .filter(|n| if q.ids.is_empty() { hit(&q.q, &[n]) } else { q.ids.iter().any(|i| i == n || i.strip_prefix("agent:") == Some(n)) })
            .map(|n| Item::new(n.clone(), n.clone()).key(Some(format!("agent:{n}"))).icon(Some("agent")))
            .collect(),
        _ => unreachable!("known kinds only"),
    };
    // Already bounded by what one owner has; a page is the first `limit`, offset cursors after.
    let from: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let next = (items.len() > from + q.limit).then(|| (from + q.limit).to_string());
    Ok(Page { items: items.into_iter().skip(from).take(q.limit).collect(), next, problem: None })
}
