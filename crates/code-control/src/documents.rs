//! Changes and work items as documents, across the caller's repos: `GET /v1/code/search` and
//! `GET /v1/code/documents/{id}`, the routes of the `search` and `fetch` tools that ChatGPT's deep
//! research and company knowledge ask an MCP server for (results with a canonical URL, then one
//! document's text). Only repos the caller owns, or a review app is installed on: someone else's
//! public repo isn't the caller's knowledge. Text written by someone who can't push (a fork's
//! change, a GitHub issue) is marked `outside`, so lockdown withholds it.

use crate::model::RepoRow;
use crate::{Caller, Code, Error, Result};
use serde_json::{Value, json};

/// Results a search answers at most.
pub const MAX_RESULTS: usize = 50;
/// Repos one statement names (an `IN` list), so a caller with thousands stays within limits.
const REPOS_PER_QUERY: usize = 400;
/// What a result's snippet keeps of the text around the match.
const SNIPPET: usize = 200;

/// One change or work item that matched.
struct Hit {
    kind: &'static str,
    id: String,
    repo_id: String,
    number: i64,
    title: String,
    body: String,
    state: String,
    outside: bool,
    updated_at: i64,
}

/// `%needle%` for LIKE, with the needle's own wildcards escaped.
fn pattern(needle: &str) -> String {
    let mut p = String::from("%");
    for ch in needle.to_lowercase().chars() {
        if matches!(ch, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(ch);
    }
    p.push('%');
    p
}

/// The text around the needle's first match (or the start), on character boundaries.
fn snippet(text: &str, needle: &str) -> String {
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths; fall back to the start when the offsets don't line up.
    let at = lower.find(&needle.to_lowercase()).filter(|_| lower.len() == text.len()).unwrap_or(0);
    let start = text[..at].char_indices().rev().nth(SNIPPET / 4).map_or(0, |(i, _)| i);
    let mut out: String = text[start..].chars().take(SNIPPET).collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        out.insert(0, '…');
    }
    out
}

impl Code {
    /// Whether a repo is the caller's own knowledge: an owner's (a member's), or an installed
    /// review app's. Public repos of others stay out.
    fn searchable(caller: &Caller, r: &RepoRow) -> bool {
        match caller {
            Caller::App { owner_id, repos, .. } => *owner_id == r.owner_id && (repos.is_empty() || repos.contains(&r.id)),
            _ => Self::owns(caller, &r.owner_id),
        }
    }

    fn document_url(&self, r: &RepoRow, kind: &str, number: i64) -> String {
        match kind {
            "change" => format!("{}/changes/{number}", self.web_url(r)),
            _ => format!("{}/work#w{number}", self.web_url(r)),
        }
    }

    /// Changes and work items in the caller's repos whose title or text holds `q` (any case),
    /// most recently updated first.
    pub fn search_documents(&self, caller: &Caller, q: &str, limit: usize) -> Result<Value> {
        let q = q.trim();
        if q.is_empty() || q.chars().count() > 200 {
            return Err(Error::Invalid("q is 1-200 characters".into()));
        }
        let limit = limit.clamp(1, MAX_RESULTS);
        let repos: Vec<RepoRow> = self.db.repos(None)?.into_iter().filter(|r| Self::searchable(caller, r)).collect();
        let like = pattern(q);
        let mut hits = Vec::new();
        for chunk in repos.chunks(REPOS_PER_QUERY) {
            let marks = vec!["?"; chunk.len()].join(",");
            for (kind, table, outside) in [("change", "changes", "trusted = 0"), ("work_item", "work_items", "outside = 1")] {
                let sql = format!(
                    "SELECT id, repo_id, number, title, body, state, CASE WHEN {outside} THEN 1 ELSE 0 END, updated_at FROM {table} \
                     WHERE repo_id IN ({marks}) AND (lower(title) LIKE ? ESCAPE '\\' OR lower(body) LIKE ? ESCAPE '\\') \
                     ORDER BY updated_at DESC LIMIT ?"
                );
                let mut params: Vec<code_sql::Value> = chunk.iter().map(|r| code_sql::Value::Text(r.id.clone())).collect();
                params.extend([code_sql::Value::Text(like.clone()), code_sql::Value::Text(like.clone()), code_sql::Value::Int(limit as i64)]);
                hits.extend(self.db.sql().query(&sql, params, |row| {
                    Ok(Hit {
                        kind,
                        id: row.get(0)?,
                        repo_id: row.get(1)?,
                        number: row.get(2)?,
                        title: row.get(3)?,
                        body: row.get(4)?,
                        state: row.get(5)?,
                        outside: row.get::<_, i64>(6)? == 1,
                        updated_at: row.get(7)?,
                    })
                })?);
            }
        }
        hits.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(limit);
        let by_id: std::collections::HashMap<&str, &RepoRow> = repos.iter().map(|r| (r.id.as_str(), r)).collect();
        let results: Vec<Value> = hits
            .iter()
            .filter_map(|h| {
                let r = by_id.get(h.repo_id.as_str())?;
                let text = if h.body.is_empty() { &h.title } else { &h.body };
                Some(json!({
                    "id": h.id,
                    "title": format!("{} #{}: {}", r.full_name(), h.number, h.title),
                    "url": self.document_url(r, h.kind, h.number),
                    "kind": h.kind,
                    "repo": r.full_name(),
                    "number": h.number,
                    "state": h.state,
                    "text": snippet(text, q),
                    "outside": h.outside,
                    "updated_at": h.updated_at,
                }))
            })
            .collect();
        Ok(json!({ "results": results }))
    }

    /// One change or work item as a document: its title, its whole text, its URL and what else
    /// says what it is.
    pub fn document(&self, caller: &Caller, id: &str) -> Result<Value> {
        let missing = || Error::NotFound(format!("document {id}"));
        let (r, kind, number, title, body, state, outside, extra) = if id.starts_with("chg_") {
            let (r, c) = self.readable_change(caller, id).map_err(|_| missing())?;
            let extra = json!({ "author": c.author, "branch": c.branch, "work_item": c.work_item, "github_url": c.github_url });
            (r, "change", c.number as i64, c.title, c.body, c.state.as_str().to_string(), c.outside, extra)
        } else if id.starts_with("wi_") {
            let (r, w) = self.readable_work(caller, id).map_err(|_| missing())?;
            let extra = json!({ "labels": w.labels, "priority": w.priority, "changes": w.changes, "github_url": w.github_url });
            (r, "work_item", w.number as i64, w.title, w.body, w.state.as_str().to_string(), w.outside, extra)
        } else {
            return Err(missing());
        };
        if !Self::searchable(caller, &r) {
            return Err(missing());
        }
        let mut metadata = json!({ "kind": kind, "repo": r.full_name(), "number": number, "state": state });
        if let (Some(m), Value::Object(e)) = (metadata.as_object_mut(), extra) {
            m.extend(e.into_iter().filter(|(_, v)| !v.is_null()));
        }
        Ok(json!({
            "id": id,
            "title": format!("{} #{number}: {title}", r.full_name()),
            "text": body,
            "url": self.document_url(&r, kind, number),
            "metadata": metadata,
            "repo": r.full_name(),
            "outside": outside,
        }))
    }
}
