//! Linear's lookups: teams, workflow states (a team's, or every team's), users, labels (a team's
//! and the workspace's), projects, cycles (a team's) and issues, through the owner's connection
//! (`Code::linear_graphql`: the sealed token, refreshed when it's due). Labels can be made here.
//! Each kind filters on Linear's side (`containsIgnoreCase`) and pages with Linear's cursors;
//! `ids` resolves stored values, which may be ids or what the fields took before pickers (a team's
//! key, a state's or a label's name, an issue's identifier).

use super::{Failure, Item, New, Page, Problem, Query, s};
use crate::Code;
use crate::agents::linear::client::LinearError;
use serde_json::{Value, json};

pub const KINDS: &[&str] = &["teams", "states", "users", "labels", "projects", "cycles", "issues"];

fn problem(e: LinearError) -> Failure {
    Failure::Problem(match e {
        LinearError::NotConnected => Problem::NotConnected { message: "Linear isn't connected".into() },
        LinearError::Reauth(_) | LinearError::Unauthorized => Problem::Reconnect { message: "Linear needs connecting again".into() },
        LinearError::RateLimited(s) => Problem::RateLimited { retry_after: s },
        LinearError::Transient(m) | LinearError::Failed(m) | LinearError::Server(m) => Problem::Unavailable { message: m },
    })
}

/// `{ first, after, filter }`, with `filter` only when it says something.
fn vars(q: &Query, filter: Value) -> Value {
    let first = if q.ids.is_empty() { q.limit } else { q.ids.len().max(1) };
    let mut v = json!({ "first": first, "after": if q.ids.is_empty() { q.cursor.clone() } else { None } });
    if filter.as_object().is_some_and(|f| !f.is_empty()) {
        v["filter"] = filter;
    }
    v
}

fn contains(q: &str) -> Value {
    json!({ "containsIgnoreCase": q })
}

/// `{ and: [a, b…] }` of the parts that say something.
fn all(parts: Vec<Value>) -> Value {
    let parts: Vec<Value> = parts.into_iter().filter(|p| p.as_object().is_some_and(|o| !o.is_empty())).collect();
    match parts.len() {
        0 => json!({}),
        1 => parts.into_iter().next().unwrap(),
        _ => json!({ "and": parts }),
    }
}

fn page(d: &Value, conn: &str, row: impl Fn(&Value) -> Option<Item>) -> Page {
    let c = &d[conn];
    let items = c["nodes"].as_array().map(|ns| ns.iter().filter_map(&row).collect()).unwrap_or_default();
    let next = (c["pageInfo"]["hasNextPage"].as_bool() == Some(true)).then(|| c["pageInfo"]["endCursor"].as_str().map(str::to_string)).flatten();
    Page { items, next, problem: None }
}

const PAGE_INFO: &str = "pageInfo { hasNextPage endCursor }";

pub async fn list(code: &Code, owner: &str, kind: &str, q: &Query) -> Result<Page, Failure> {
    let ids = &q.ids;
    let (query, filter, conn): (String, Value, &str) = match kind {
        "teams" => {
            let f = if !ids.is_empty() {
                json!({ "or": [{ "id": { "in": ids } }, { "key": { "in": ids } }] })
            } else if q.q.is_empty() {
                json!({})
            } else {
                json!({ "or": [{ "name": contains(&q.q) }, { "key": contains(&q.q) }] })
            };
            (format!("query RollLookupTeams($first: Int, $after: String, $filter: TeamFilter) {{ teams(first: $first, after: $after, filter: $filter) {{ nodes {{ id key name color }} {PAGE_INFO} }} }}"), f, "teams")
        }
        "states" => {
            let team = q.parent.as_deref().map(|t| json!({ "team": team(t) })).unwrap_or(json!({}));
            let f = if !ids.is_empty() {
                all(vec![team, json!({ "or": [{ "id": { "in": ids } }, { "name": { "in": ids } }] })])
            } else if q.q.is_empty() {
                team
            } else {
                all(vec![team, json!({ "name": contains(&q.q) })])
            };
            (format!("query RollLookupStates($first: Int, $after: String, $filter: WorkflowStateFilter) {{ workflowStates(first: $first, after: $after, filter: $filter) {{ nodes {{ id name type color position team {{ id key name }} }} {PAGE_INFO} }} }}"), f, "workflowStates")
        }
        "users" => {
            let f = if !ids.is_empty() {
                json!({ "or": [{ "id": { "in": ids } }, { "email": { "in": ids } }, { "name": { "in": ids } }, { "displayName": { "in": ids } }] })
            } else if q.q.is_empty() {
                json!({ "active": { "eq": true } })
            } else {
                json!({ "active": { "eq": true }, "or": [{ "name": contains(&q.q) }, { "displayName": contains(&q.q) }, { "email": contains(&q.q) }] })
            };
            (format!("query RollLookupUsers($first: Int, $after: String, $filter: UserFilter) {{ users(first: $first, after: $after, filter: $filter) {{ nodes {{ id name displayName email avatarUrl }} {PAGE_INFO} }} }}"), f, "users")
        }
        "labels" => {
            // A team's labels and the workspace's (Linear lets an issue have either).
            let team = q.parent.as_deref().map(|t| json!({ "or": [{ "team": team(t) }, { "team": { "null": true } }] })).unwrap_or(json!({}));
            let f = if !ids.is_empty() {
                json!({ "or": [{ "id": { "in": ids } }, { "name": { "in": ids } }] })
            } else if q.q.is_empty() {
                team
            } else {
                all(vec![team, json!({ "name": contains(&q.q) })])
            };
            (format!("query RollLookupLabels($first: Int, $after: String, $filter: IssueLabelFilter) {{ issueLabels(first: $first, after: $after, filter: $filter) {{ nodes {{ id name color isGroup parent {{ name }} team {{ key name }} }} {PAGE_INFO} }} }}"), f, "issueLabels")
        }
        "projects" => {
            let f = if !ids.is_empty() {
                json!({ "or": [{ "id": { "in": ids } }, { "name": { "in": ids } }] })
            } else if q.q.is_empty() {
                json!({})
            } else {
                json!({ "name": contains(&q.q) })
            };
            (format!("query RollLookupProjects($first: Int, $after: String, $filter: ProjectFilter) {{ projects(first: $first, after: $after, filter: $filter) {{ nodes {{ id name color state }} {PAGE_INFO} }} }}"), f, "projects")
        }
        "cycles" => {
            let team = json!({ "team": team(q.parent("team")?) });
            let f = if !ids.is_empty() { all(vec![team, json!({ "id": { "in": ids } })]) } else { all(vec![team, json!({ "isPast": { "eq": false } })]) };
            (format!("query RollLookupCycles($first: Int, $after: String, $filter: CycleFilter) {{ cycles(first: $first, after: $after, filter: $filter) {{ nodes {{ id number name startsAt endsAt isActive }} {PAGE_INFO} }} }}"), f, "cycles")
        }
        "issues" => {
            let f = if !ids.is_empty() {
                issue_refs(ids)
            } else if let Some((key, n)) = identifier(&q.q) {
                json!({ "team": { "key": { "eqIgnoreCase": key } }, "number": { "eq": n } })
            } else if q.q.is_empty() {
                json!({})
            } else {
                json!({ "title": contains(&q.q) })
            };
            (format!("query RollLookupIssues($first: Int, $after: String, $filter: IssueFilter) {{ issues(first: $first, after: $after, filter: $filter, orderBy: updatedAt) {{ nodes {{ id identifier title state {{ name type color }} team {{ key }} }} {PAGE_INFO} }} }}"), f, "issues")
        }
        _ => unreachable!("known kinds only"),
    };
    let d = code.linear_graphql(owner, &query, vars(q, filter)).await.map_err(problem)?;
    let mut p = page(&d, conn, |n| row(kind, n));
    if kind == "states" && q.parent.is_none() {
        // Every team's: grouped by team, in the team's order.
        p.items.sort_by(|a, b| a.group.cmp(&b.group));
    }
    if kind == "cycles" {
        p.items.sort_by_key(|i| i.key.as_deref().and_then(|k| k.parse::<i64>().ok()).unwrap_or(0));
    }
    Ok(p)
}

/// One node as a row.
fn row(kind: &str, n: &Value) -> Option<Item> {
    let id = s(n, &["id"])?;
    Some(match kind {
        "teams" => Item::new(id, s(n, &["name"]).unwrap_or(id)).key(s(n, &["key"])).hint(s(n, &["key"])).color(s(n, &["color"])),
        "states" => Item::new(id, s(n, &["name"]).unwrap_or(id))
            .icon(s(n, &["type"]))
            .color(s(n, &["color"]))
            .group(s(n, &["team", "name"]))
            .hint(s(n, &["team", "key"])),
        "users" => {
            let name = s(n, &["name"]).or(s(n, &["displayName"])).unwrap_or(id);
            Item::new(id, name).key(s(n, &["email"])).hint(s(n, &["email"])).avatar(s(n, &["avatarUrl"])).icon(Some("user"))
        }
        "labels" => {
            if n["isGroup"].as_bool() == Some(true) {
                // A group can't be put on an issue; its labels can.
                return None;
            }
            let name = s(n, &["name"]).unwrap_or(id);
            let label = match s(n, &["parent", "name"]) {
                Some(group) => format!("{group} / {name}"),
                None => name.to_string(),
            };
            Item::new(id, label).key(Some(name)).color(s(n, &["color"])).hint(Some(s(n, &["team", "key"]).unwrap_or("Workspace")))
        }
        "projects" => Item::new(id, s(n, &["name"]).unwrap_or(id)).color(s(n, &["color"])).hint(s(n, &["state"])),
        "cycles" => {
            let number = n["number"].as_f64().map(|x| (x as i64).to_string());
            let label = match (s(n, &["name"]), &number) {
                (Some(name), _) => name.to_string(),
                (None, Some(num)) => format!("Cycle {num}"),
                (None, None) => id.to_string(),
            };
            let when = match (s(n, &["startsAt"]), s(n, &["endsAt"])) {
                (Some(a), Some(b)) => Some(format!("{} – {}", &a[..a.len().min(10)], &b[..b.len().min(10)])),
                _ => None,
            };
            let hint = if n["isActive"].as_bool() == Some(true) { Some(format!("Current · {}", when.unwrap_or_default())) } else { when };
            Item::new(id, label).key(number).hint(hint)
        }
        "issues" => {
            let ident = s(n, &["identifier"]).unwrap_or(id);
            Item::new(id, s(n, &["title"]).unwrap_or(ident)).key(Some(ident)).hint(Some(ident)).icon(s(n, &["state", "type"])).color(s(n, &["state", "color"]))
        }
        _ => return None,
    })
}

/// A team by id, or by its key (`ENG`: what a field held before pickers).
fn team(t: &str) -> Value {
    if t.len() <= 10 && t.chars().all(|c| c.is_ascii_alphanumeric()) { json!({ "key": { "eqIgnoreCase": t } }) } else { json!({ "id": { "eq": t } }) }
}

/// `ENG-12` → (`ENG`, 12).
fn identifier(q: &str) -> Option<(&str, i64)> {
    let (key, n) = q.trim().rsplit_once('-')?;
    let ok = !key.is_empty() && key.len() <= 10 && key.chars().all(|c| c.is_ascii_alphanumeric());
    if !ok {
        return None;
    }
    Some((key, n.parse().ok()?))
}

/// Stored issue values: ids, and identifiers (`ENG-12`) as a team key and a number.
fn issue_refs(ids: &[String]) -> Value {
    let mut or = Vec::new();
    let uuids: Vec<&String> = ids.iter().filter(|i| identifier(i).is_none()).collect();
    if !uuids.is_empty() {
        or.push(json!({ "id": { "in": uuids } }));
    }
    for (key, n) in ids.iter().filter_map(|i| identifier(i)) {
        or.push(json!({ "team": { "key": { "eqIgnoreCase": key } }, "number": { "eq": n } }));
    }
    json!({ "or": or })
}

const LABEL_CREATE: &str = "mutation RollLookupLabelCreate($input: IssueLabelCreateInput!) { issueLabelCreate(input: $input) { success issueLabel { id name color isGroup parent { name } team { key name } } } }";

/// Make a label (a team's when `parent` names one, else the workspace's).
pub async fn create(code: &Code, owner: &str, kind: &str, new: &New) -> Result<Item, Failure> {
    if kind != "labels" {
        return Err(Failure::Refused(crate::Error::Unprocessable(format!("roll can't make Linear {kind}"))));
    }
    let mut input = json!({ "name": new.name });
    if let Some(t) = &new.parent {
        input["teamId"] = json!(t);
    }
    if let Some(c) = &new.color {
        input["color"] = json!(c);
    }
    let d = code.linear_graphql(owner, LABEL_CREATE, json!({ "input": input })).await.map_err(problem)?;
    row("labels", &d["issueLabelCreate"]["issueLabel"]).ok_or_else(|| Failure::Problem(Problem::Unavailable { message: "Linear didn't make the label".into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_a_team_key_and_a_number() {
        assert_eq!(identifier("ENG-12"), Some(("ENG", 12)));
        assert_eq!(identifier(" eng-7 "), Some(("eng", 7)));
        assert_eq!(identifier("9cfb4d1e-1b5c-4c4a-9d55-3f0f3b8a2f10"), None);
        assert_eq!(identifier("login fails"), None);
        let f = issue_refs(&["ENG-12".into(), "9cfb4d1e-1b5c-4c4a-9d55-3f0f3b8a2f10".into()]);
        assert_eq!(f["or"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn label_groups_are_not_rows_and_children_say_their_group() {
        assert!(row("labels", &json!({ "id": "g", "name": "Type", "isGroup": true })).is_none());
        let i = row("labels", &json!({ "id": "l", "name": "Bug", "color": "#eb5757", "parent": { "name": "Type" }, "team": null })).unwrap();
        assert_eq!((i.label.as_str(), i.key.as_deref(), i.hint.as_deref(), i.color.as_deref()), ("Type / Bug", Some("Bug"), Some("Workspace"), Some("#eb5757")));
    }
}
