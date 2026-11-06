//! GitHub's lookups: the owner's GitHub-connected repos, and per repo its branches, labels (made
//! here too), assignees, reviewers (collaborators and the organization's teams), teams,
//! milestones, workflows, environments and open issues and pull requests.
//!
//! - **Only through the owner's repos.** An installation spans tenants, so GitHub is only ever
//!   asked about a repo the owner connected, with that repo's installation token
//!   (`Hub::client`); a `parent` naming anything else is a 404. `parent` is a repo as the GitHub
//!   steps take it (`app`, `acme/app`) or its GitHub name (`acme-inc/app`); without one, the
//!   rows of each connected repo (the first [`MAX_REPOS`]), under the repo's name.
//! - **Pages are GitHub's.** The cursor is GitHub's page number. GitHub's lists don't search, so
//!   a search reads up to [`SCAN_PAGES`] pages of 100 and keeps what matches; `ids` resolve the
//!   same way (issues by number).
//! - **Problems.** No App or no connected repo: `not_connected`; a refused token or permission:
//!   `reconnect`, naming the permission; a rate limit (GitHub's `retry-after`,
//!   `x-ratelimit-reset`): `rate_limited`, without waiting.

use super::{Failure, Item, New, Page, Problem, Query};
use crate::github::{Hub, Link, link_of, linked_repos};
use crate::model::RepoRow;
use crate::{Code, Error};
use code_github::ListPage;
use std::collections::HashSet;
use std::future::Future;

pub const KINDS: &[&str] = &["repos", "branches", "labels", "users", "reviewers", "teams", "milestones", "workflows", "environments", "issues"];

/// Repos whose rows are shown together when no `parent` says which.
const MAX_REPOS: usize = 5;
/// Pages of 100 read to find what a search or `ids` name.
const SCAN_PAGES: u32 = 5;
/// Issues fetched by number to show stored values.
const MAX_ISSUE_IDS: usize = 10;

type R<T> = Result<T, Failure>;

fn not_connected(message: &str) -> Failure {
    Failure::Problem(Problem::NotConnected { message: message.into() })
}

/// What a GitHub error means for a picker. `needs`: the permission the call takes.
fn problem(e: code_github::Error, needs: &str) -> Failure {
    use code_github::Error as E;
    Failure::Problem(match e {
        E::RateLimited { reset } => Problem::RateLimited { retry_after: reset.saturating_sub(code_github::auth::unix_now()).max(1) },
        E::Auth(_) | E::Config(_) | E::Status { status: 401, .. } => Problem::Reconnect { message: "GitHub refused the App's credentials: install the GitHub App again".into() },
        E::Status { status: 403, .. } => Problem::Reconnect { message: format!("The GitHub App needs the {needs} permission: accept it in the installation's settings") },
        E::Status { status: 404, .. } => Problem::Unavailable { message: "GitHub can't find it: is the GitHub App still installed on the repo?".into() },
        E::Status { status, .. } => Problem::Unavailable { message: format!("GitHub answered {status}") },
        E::Http(_) | E::Decode(_) => Problem::Unavailable { message: "GitHub didn't answer".into() },
    })
}

/// The owner's repos linked to GitHub, connected or not.
fn linked(code: &Code, owner: &str) -> R<Vec<(RepoRow, Link)>> {
    Ok(linked_repos(code)?.into_iter().filter(|r| r.owner_id == owner).filter_map(|r| link_of(&r).map(|l| (r, l))).collect())
}

/// `p` names the repo: as the GitHub steps take it, or by its GitHub name.
fn names(r: &RepoRow, l: &Link, p: &str) -> bool {
    crate::chatops::github::names(r, p) || p.eq_ignore_ascii_case(&l.full_name())
}

/// The repos a lookup reads: `parent`'s (one of the owner's, else a 404), or the connected ones.
fn targets(code: &Code, owner: &str, parent: Option<&str>) -> R<Vec<(RepoRow, Link)>> {
    let all = linked(code, owner)?;
    match parent {
        Some(p) => {
            let Some((r, l)) = all.into_iter().find(|(r, l)| names(r, l, p)) else {
                return Err(Failure::Refused(Error::NotFound(format!("{p} isn't one of the owner's GitHub-connected repos"))));
            };
            if !l.connected {
                return Err(Failure::Problem(Problem::Reconnect { message: format!("GitHub lost access to {}: connect it again", r.full_name()) }));
            }
            Ok(vec![(r, l)])
        }
        None => {
            let c: Vec<_> = all.into_iter().filter(|(_, l)| l.connected).take(MAX_REPOS).collect();
            if c.is_empty() {
                return Err(not_connected("No repo of this owner is connected to GitHub"));
            }
            Ok(c)
        }
    }
}

pub async fn list(code: &Code, owner: &str, kind: &str, q: &Query) -> R<Page> {
    let Some(hub) = code.github_hub().cloned() else { return Err(not_connected("The GitHub App isn't set up on this server")) };
    if kind == "repos" {
        return repos(code, owner, q);
    }
    let targets = targets(code, owner, q.parent.as_deref())?;
    if let [(r, l)] = targets.as_slice() {
        return one(&hub, r, l, kind, q).await;
    }
    // Several repos: the first page of each, under its name.
    let first = Query { cursor: None, ..q.clone() };
    let mut p = Page::default();
    let mut seen = HashSet::new();
    for (r, l) in &targets {
        for i in one(&hub, r, l, kind, &first).await?.items {
            if seen.insert(i.id.clone()) {
                p.items.push(i.group(Some(r.full_name())));
            }
        }
    }
    Ok(p)
}

/// The owner's connected repos (the stored values of any linked one resolve).
fn repos(code: &Code, owner: &str, q: &Query) -> R<Page> {
    let all = linked(code, owner)?;
    if all.is_empty() {
        return Err(not_connected("No repo of this owner is connected to GitHub"));
    }
    let rows: Vec<Item> = all
        .into_iter()
        .filter(|(_, l)| l.connected || !q.ids.is_empty())
        .map(|(r, l)| Item::new(r.full_name(), r.full_name()).key(Some(r.name.clone())).hint(Some(l.full_name())).icon(Some("repo")))
        .filter(|i| wanted(q, i))
        .collect();
    let from: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let next = (rows.len() > from + q.limit).then(|| (from + q.limit).to_string());
    Ok(Page { items: rows.into_iter().skip(from).take(q.limit).collect(), next, problem: None })
}

/// Whether a row is one a search or `ids` asks for.
fn wanted(q: &Query, i: &Item) -> bool {
    if !q.ids.is_empty() {
        return q.ids.iter().any(|v| v.eq_ignore_ascii_case(&i.id) || i.key.as_deref().is_some_and(|k| v.eq_ignore_ascii_case(k)));
    }
    let needle = q.q.to_lowercase();
    needle.is_empty() || [Some(&i.label), Some(&i.id), i.key.as_ref(), i.hint.as_ref()].into_iter().flatten().any(|s| s.to_lowercase().contains(&needle))
}

/// GitHub's page number from the cursor (1 without one).
fn start(q: &Query) -> R<u32> {
    match q.cursor.as_deref() {
        None => Ok(1),
        Some(c) => c.parse::<u32>().ok().filter(|n| (1..=code_github::client::MAX_PAGE).contains(n)).ok_or_else(|| Failure::Refused(Error::Invalid("cursor is a page number".into()))),
    }
}

/// One page as asked, or (a search, `ids`) up to [`SCAN_PAGES`] pages of 100 filtered.
async fn scan<F, Fut>(q: &Query, fetch: F) -> R<Page>
where
    F: Fn(u32, u8) -> Fut,
    Fut: Future<Output = R<ListPage<Item>>>,
{
    let first = start(q)?;
    if q.ids.is_empty() && q.q.is_empty() {
        let p = fetch(first, q.limit.min(100) as u8).await?;
        return Ok(Page { items: p.items, next: p.next.map(|n| n.to_string()), problem: None });
    }
    let mut items: Vec<Item> = Vec::new();
    let mut page = Some(first);
    for _ in 0..SCAN_PAGES {
        let Some(n) = page else { break };
        let p = fetch(n, 100).await?;
        items.extend(p.items.into_iter().filter(|i| wanted(q, i)));
        page = p.next;
        let found_all = !q.ids.is_empty() && q.ids.iter().all(|v| items.iter().any(|i| wanted(&Query { ids: vec![v.clone()], ..Query::default() }, i)));
        if items.len() >= q.limit || found_all {
            break;
        }
    }
    // The next page carries on from the last one read (whole pages, so nothing is skipped).
    let next = if q.ids.is_empty() { page.map(|n| n.to_string()) } else { None };
    Ok(Page { items, next, problem: None })
}

/// One repo's rows of `kind`.
async fn one(hub: &Hub, r: &RepoRow, l: &Link, kind: &str, q: &Query) -> R<Page> {
    match kind {
        "issues" => issues(hub, l, q).await,
        "teams" => teams(hub, l, q).await,
        "reviewers" => {
            // People (the repo's collaborators), then on the first page the organization's teams
            // when the App may see them.
            let mut p = scan(q, |page, per| rows(hub, r, l, "collaborators", page, per)).await?;
            for i in &mut p.items {
                i.group = Some("People".into());
            }
            if q.cursor.is_none() {
                match teams(hub, l, q).await {
                    Ok(t) => p.items.extend(t.items.into_iter().map(|i| i.group(Some("Teams")))),
                    Err(Failure::Problem(Problem::RateLimited { retry_after })) => return Err(Failure::Problem(Problem::RateLimited { retry_after })),
                    Err(_) => {}
                }
            }
            Ok(p)
        }
        _ => scan(q, |page, per| rows(hub, r, l, kind, page, per)).await,
    }
}

/// A page of `kind` as rows.
async fn rows(hub: &Hub, r: &RepoRow, l: &Link, kind: &str, page: u32, per: u8) -> R<ListPage<Item>> {
    let c = &hub.client;
    let (inst, o, n) = (l.installation_id, l.owner.as_str(), l.repo.as_str());
    fn map<T>(p: ListPage<T>, f: impl Fn(T) -> Item) -> ListPage<Item> {
        ListPage { items: p.items.into_iter().map(f).collect(), next: p.next }
    }
    Ok(match kind {
        "branches" => map(c.branches_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Contents (read)"))?, |b| {
            let hint = if b.name == r.default_branch { Some("default") } else if b.protected { Some("protected") } else { None };
            Item::new(b.name.clone(), b.name).hint(hint).icon(Some("branch"))
        }),
        "labels" => map(c.labels_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Issues (read)"))?, label),
        "users" => map(c.assignees_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Issues (read)"))?, person),
        "collaborators" => map(c.collaborators_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Metadata (read)"))?, person),
        "milestones" => map(c.milestones_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Issues (read)"))?, |m| {
            let hint = match (&m.due_on, m.open_issues) {
                (Some(due), _) => Some(format!("due {}", &due[..due.len().min(10)])),
                (None, Some(open)) => Some(format!("{open} open")),
                _ => None,
            };
            Item::new(m.number.to_string(), m.title.clone()).key(Some(m.title)).hint(hint).icon(Some("milestone"))
        }),
        "workflows" => map(c.workflows_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Actions (read)"))?, |w| {
            // The file name: what `workflow_dispatch` takes, and it reads better than the id.
            let file = w.path.rsplit('/').next().unwrap_or(&w.path).to_string();
            let hint = if w.state.as_deref().is_some_and(|s| s.starts_with("disabled")) { format!("disabled · {}", w.path) } else { w.path.clone() };
            Item::new(file, w.name).key(Some(w.id.to_string())).hint(Some(hint)).icon(Some("workflow"))
        }),
        "environments" => map(c.environments_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Environments (read)"))?, |e| Item::new(e.name.clone(), e.name).icon(Some("environment"))),
        _ => return Err(Failure::Refused(Error::NotFound(format!("github lookup {kind:?}")))),
    })
}

fn label(l: code_github::model::RepoLabel) -> Item {
    let color = l.color.map(|c| format!("#{c}"));
    Item::new(l.name.clone(), l.name).color(color.as_deref()).hint(l.description)
}

fn person(p: code_github::model::Person) -> Item {
    let bot = p.kind.as_deref() == Some("Bot");
    Item::new(p.login.clone(), p.login).avatar(p.avatar_url.as_deref()).hint(bot.then_some("bot")).icon(Some("user"))
}

/// The organization's teams (`org/slug`). A user's repo has none; a missing permission is said.
async fn teams(hub: &Hub, l: &Link, q: &Query) -> R<Page> {
    let org = l.owner.as_str();
    let got = scan(q, |page, per| async move {
        let p = hub.client.org_teams_page(l.installation_id, org, page, per).await;
        match p {
            Ok(p) => Ok(ListPage {
                items: p.items.into_iter().map(|t| Item::new(format!("{org}/{}", t.slug), t.name).key(Some(t.slug.clone())).hint(Some(format!("@{org}/{}", t.slug))).icon(Some("team"))).collect(),
                next: p.next,
            }),
            // Not an organization.
            Err(e) if e.is_not_found() => Ok(ListPage { items: Vec::new(), next: None }),
            Err(e) => Err(problem(e, "organization Members (read)")),
        }
    })
    .await?;
    Ok(got)
}

/// `#12` or `12`.
fn number(s: &str) -> Option<u64> {
    s.trim().trim_start_matches('#').parse().ok().filter(|n| *n > 0)
}

fn issue_row(i: code_github::model::Issue) -> Item {
    let pr = i.is_pull_request();
    Item::new(i.number.to_string(), i.title).key(Some(format!("#{}", i.number))).hint(Some(format!("#{}", i.number))).icon(pr.then_some("pull"))
}

/// Open issues and pull requests; a number is looked up directly (open or not).
async fn issues(hub: &Hub, l: &Link, q: &Query) -> R<Page> {
    let (inst, o, n) = (l.installation_id, l.owner.as_str(), l.repo.as_str());
    let by_number: Vec<u64> = if !q.ids.is_empty() { q.ids.iter().filter_map(|v| number(v)).take(MAX_ISSUE_IDS).collect() } else { number(&q.q).into_iter().collect() };
    if !q.ids.is_empty() || !by_number.is_empty() {
        let mut items = Vec::new();
        for num in by_number {
            match hub.client.issue_for_picker(inst, o, n, num).await {
                Ok(i) => items.push(issue_row(i)),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(problem(e, "Issues (read)")),
            }
        }
        return Ok(Page { items, next: None, problem: None });
    }
    scan(q, |page, per| async move {
        let p = hub.client.open_issues_page(inst, o, n, page, per).await.map_err(|e| problem(e, "Issues (read)"))?;
        Ok(ListPage { items: p.items.into_iter().map(issue_row).collect(), next: p.next })
    })
    .await
}

/// Make a label on `parent` (or on the owner's only connected repo).
pub async fn create(code: &Code, owner: &str, kind: &str, new: &New) -> R<Item> {
    if kind != "labels" {
        return Err(Failure::Refused(Error::Unprocessable(format!("roll can't make GitHub {kind}"))));
    }
    let Some(hub) = code.github_hub().cloned() else { return Err(not_connected("The GitHub App isn't set up on this server")) };
    let targets = targets(code, owner, new.parent.as_deref())?;
    let [(_, l)] = targets.as_slice() else { return Err(Failure::Refused(Error::Invalid("say which repo (`parent`)".into()))) };
    match hub.client.create_label(l.installation_id, &l.owner, &l.repo, &new.name, new.color.as_deref()).await {
        Ok(made) => Ok(label(made)),
        Err(code_github::Error::Status { status: 422, .. }) => Err(Failure::Refused(Error::Conflict(format!("{} has a label {:?} already", l.full_name(), new.name)))),
        Err(e) => Err(problem(e, "Issues (write)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, ids: &[&str]) -> Query {
        Query { q: text.into(), ids: ids.iter().map(|s| s.to_string()).collect(), limit: 50, ..Query::default() }
    }

    #[test]
    fn numbers_and_cursors() {
        assert_eq!((number("#12"), number(" 7 "), number("fix"), number("0")), (Some(12), Some(7), None, None));
        assert_eq!(start(&Query::default()).ok(), Some(1));
        assert_eq!(start(&Query { cursor: Some("3".into()), ..Query::default() }).ok(), Some(3));
        for bad in ["0", "x", "https://evil.example/next", "1001"] {
            assert!(start(&Query { cursor: Some(bad.into()), ..Query::default() }).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_search_reads_whole_pages_and_carries_on_from_the_last() {
        let page = |n: u32| ListPage { items: (0..100).map(|i| Item::new(format!("{n}-{i}"), if i % 50 == 0 { "match" } else { "other" })).collect(), next: (n < 9).then_some(n + 1) };
        let fetch = |n: u32, per: u8| {
            assert_eq!(per, 100);
            async move { Ok(page(n)) }
        };
        let mut query = q("match", &[]);
        query.limit = 3;
        let p = scan(&query, fetch).await.ok().unwrap();
        assert_eq!((p.items.len(), p.next.as_deref()), (4, Some("3")), "two pages read, all their matches kept");
        query.limit = 50;
        let p = scan(&query, fetch).await.ok().unwrap();
        assert_eq!((p.items.len(), p.next.as_deref()), (10, Some("6")), "at most SCAN_PAGES pages");
        let p = scan(&q("", &["2-7"]), fetch).await.ok().unwrap();
        assert_eq!((p.items.len(), p.next), (1, None));
    }
}
