//! Changes, work items and files as documents, across the caller's repos: `GET /v1/code/search`
//! and `GET /v1/code/documents/{id}`, the routes of the `search` and `fetch` tools that ChatGPT's
//! deep research and company knowledge ask an MCP server for (results with a canonical URL, then
//! one document's text). Only repos the caller owns, or a review app is installed on: someone
//! else's public repo isn't the caller's knowledge. Text written by someone who can't push (a
//! fork's change, a GitHub issue) is marked `outside`, so lockdown withholds it.
//!
//! Changes and work items come from control.db, which every node shares. Files come from each
//! repo's code index, which lives on the repo's owner: each repo's part runs there
//! (`Code::to_owner`, an internal request), a few at a time, under per-repo and overall deadlines,
//! and every repo that wasn't searched is said in `skipped` (docs/code/search.md).

use crate::model::RepoRow;
use crate::{Caller, Code, Error, Result};
use base64::Engine;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

/// Results a search answers at most.
pub const MAX_RESULTS: usize = 50;
/// Repos one statement names (an `IN` list), so a caller with thousands stays within limits.
const REPOS_PER_QUERY: usize = 400;
/// What a result's snippet keeps of the text around the match.
const SNIPPET: usize = 200;
/// Repos a file search covers (the most recently changed); the rest are skipped as `limit`.
const FILE_REPOS: usize = 200;
/// Matches each repo's index answers for a file search.
const PER_REPO: usize = 5;
/// Repos searched at once.
const FILE_CONCURRENCY: usize = 16;
/// How long one repo's search may take, and the whole file search.
const REPO_BUDGET: Duration = Duration::from_secs(1);
const FILE_BUDGET: Duration = Duration::from_secs(3);
/// A matching line is cut to this many characters.
const LINE_CHARS: usize = 300;
/// Lines a file document answers around its match.
const FILE_LINES: usize = 400;

/// Which kinds a search covers (`kinds=changes,work,files`; changes and work items by default).
#[derive(Clone, Copy)]
struct Kinds {
    changes: bool,
    work: bool,
    files: bool,
}

impl Kinds {
    fn parse(v: Option<&str>) -> Result<Kinds> {
        let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else { return Ok(Kinds { changes: true, work: true, files: false }) };
        let mut k = Kinds { changes: false, work: false, files: false };
        for part in v.split(',').map(str::trim) {
            match part {
                "changes" => k.changes = true,
                "work" => k.work = true,
                "files" => k.files = true,
                other => return Err(Error::Invalid(format!("no kind {other:?}: changes, work, files"))),
            }
        }
        Ok(k)
    }
}

/// A file match's document id: `f.<repo id>.<rev>.<path, base64url>.<line>`.
fn file_id(repo_id: &str, rev: &str, path: &str, line: u64) -> String {
    format!("f.{repo_id}.{rev}.{}.{line}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path))
}

/// The parts of a file document id: repo id, revision, path, line.
fn parse_file_id(id: &str) -> Option<(String, String, String, u64)> {
    let mut p = id.strip_prefix("f.")?.split('.');
    let (repo, rev, path, line) = (p.next()?, p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || repo.is_empty() || rev.is_empty() {
        return None;
    }
    let path = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(path).ok()?).ok()?;
    Some((repo.to_string(), rev.to_string(), path, line.parse().ok()?))
}

/// `text` cut to `n` characters.
fn cut(text: &str, n: usize) -> String {
    match text.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

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
    // Lowercasing can change byte lengths (one character longer, another shorter, the total the
    // same): an offset in `lower` is used only where it's a character boundary in `text` too.
    let at = lower.find(&needle.to_lowercase()).filter(|&i| lower.len() == text.len() && text.is_char_boundary(i)).unwrap_or(0);
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
    /// most recently updated first, then (with `kinds` naming `files`) files whose content holds
    /// it, interleaved by repo. `reads` keeps the repos a scoped grant may read (all otherwise).
    pub async fn search_documents(&self, caller: &Caller, q: &str, kinds: Option<&str>, limit: usize, reads: &(dyn Fn(&str) -> bool + Sync)) -> Result<Value> {
        let q = q.trim();
        if q.is_empty() || q.chars().count() > 200 {
            return Err(Error::Invalid("q is 1-200 characters".into()));
        }
        let kinds = Kinds::parse(kinds)?;
        if kinds.files && q.chars().count() < 3 {
            return Err(Error::Invalid("a file search needs at least 3 characters".into()));
        }
        let limit = limit.clamp(1, MAX_RESULTS);
        let owner = match caller {
            Caller::Owner { id, .. } => Some(id.as_str()),
            Caller::App { owner_id, .. } => Some(owner_id.as_str()),
            _ => None,
        };
        let repos: Vec<RepoRow> = self.db.repos(owner)?.into_iter().filter(|r| Self::searchable(caller, r) && reads(&r.full_name())).collect();
        let like = pattern(q);
        let mut hits = Vec::new();
        for chunk in repos.chunks(REPOS_PER_QUERY) {
            let marks = vec!["?"; chunk.len()].join(",");
            let tables = [("change", "changes", "trusted = 0", kinds.changes), ("work_item", "work_items", "outside = 1", kinds.work)];
            for (kind, table, outside, _) in tables.into_iter().filter(|t| t.3) {
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
        let mut results: Vec<Value> = hits
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
        if !kinds.files {
            return Ok(json!({ "results": results }));
        }
        let (files, skipped) = self.file_hits(&repos, q).await;
        results.extend(files);
        results.truncate(limit);
        Ok(json!({ "results": results, "skipped": skipped }))
    }

    /// Files whose content holds `q` in up to [`FILE_REPOS`] of `repos` (the most recently
    /// changed first), each repo's matches from its owner's index, interleaved by repo; and every
    /// repo not searched, with why.
    async fn file_hits(&self, repos: &[RepoRow], q: &str) -> (Vec<Value>, Vec<Value>) {
        let mut order: Vec<&RepoRow> = repos.iter().collect();
        order.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        let (take, rest) = order.split_at(order.len().min(FILE_REPOS));
        let mut skipped: Vec<Value> = rest.iter().map(|r| json!({ "repo": r.full_name(), "why": "limit" })).collect();
        let answers = parking_lot::Mutex::new(Vec::new());
        // Boxed, so the stream's item type names no closure lifetimes (the handler's future is Send).
        type Part<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = (usize, std::result::Result<Value, &'static str>)> + Send + 'a>>;
        let parts: Vec<Part<'_>> = take
            .iter()
            .enumerate()
            .map(|(i, r)| -> Part<'_> {
                let r = *r;
                Box::pin(async move { (i, tokio::time::timeout(REPO_BUDGET, self.repo_file_search(r, q)).await.unwrap_or(Err("timeout"))) })
            })
            .collect();
        let run = futures::stream::iter(parts)
            .buffer_unordered(FILE_CONCURRENCY)
            .for_each(|a| {
                answers.lock().push(a);
                std::future::ready(())
            });
        let _ = tokio::time::timeout(FILE_BUDGET, run).await;
        let mut per_repo: Vec<Option<Vec<Value>>> = vec![None; take.len()];
        for (i, answer) in answers.into_inner() {
            let r = take[i];
            match answer {
                Ok(v) => {
                    let rev = v["rev"].as_str().unwrap_or_default().to_string();
                    let items = v["matches"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(|m| {
                        let (path, line) = (m["path"].as_str()?, m["line"].as_u64()?);
                        Some(json!({
                            "id": file_id(&r.id, &rev, path, line),
                            "title": format!("{}: {path}:{line}", r.full_name()),
                            "url": format!("{}/blob/{rev}/{path}#L{line}", self.web_url(r)),
                            "kind": "file",
                            "repo": r.full_name(),
                            "path": path,
                            "line": line,
                            "text": cut(m["text"].as_str().unwrap_or_default().trim(), LINE_CHARS),
                            "outside": false,
                        }))
                    });
                    per_repo[i] = Some(items.collect());
                }
                Err(why) => {
                    per_repo[i] = Some(Vec::new());
                    skipped.push(json!({ "repo": r.full_name(), "why": why }));
                }
            }
        }
        // What didn't answer within the whole search's budget.
        for (i, got) in per_repo.iter().enumerate() {
            if got.is_none() {
                skipped.push(json!({ "repo": take[i].full_name(), "why": "timeout" }));
            }
        }
        // Round-robin, most recently changed repo first: one big repo can't fill the page.
        let mut lists: Vec<std::vec::IntoIter<Value>> = per_repo.into_iter().map(|l| l.unwrap_or_default().into_iter()).collect();
        let mut out = Vec::new();
        loop {
            let before = out.len();
            for l in &mut lists {
                out.extend(l.next());
            }
            if out.len() == before {
                break;
            }
        }
        (out, skipped)
    }

    /// One repo's file search, on its owner: its index's answer, or why there's none.
    async fn repo_file_search(&self, r: &RepoRow, q: &str) -> std::result::Result<Value, &'static str> {
        if self.cluster().is_none() {
            return self.search_code(r, q, false, false, None, PER_REPO, None).await.map_err(|e| match e {
                Error::Indexing => "indexing",
                Error::Unprocessable(_) => "off",
                _ => "error",
            });
        }
        let body = bytes::Bytes::from(json!({ "q": q, "per_repo": PER_REPO }).to_string());
        let resp = self.to_owner(&r.id, "search", body).await;
        let status = resp.status();
        let read = axum::body::to_bytes(resp.into_body(), 256 << 10).await.ok();
        let v: Value = read.as_deref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(Value::Null);
        match status {
            s if s.is_success() => Ok(v),
            axum::http::StatusCode::SERVICE_UNAVAILABLE if v["indexing"] == true => Err("indexing"),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY => Err("off"),
            axum::http::StatusCode::SERVICE_UNAVAILABLE | axum::http::StatusCode::BAD_GATEWAY | axum::http::StatusCode::GATEWAY_TIMEOUT | axum::http::StatusCode::LOOP_DETECTED => Err("unavailable"),
            _ => Err("error"),
        }
    }

    /// One change or work item as a document: its title, its whole text, its URL and what else
    /// says what it is.
    pub async fn document(&self, caller: &Caller, id: &str) -> Result<Value> {
        let missing = || Error::NotFound(format!("document {id}"));
        if id.starts_with("f.") {
            return self.file_document(caller, id).await;
        }
        // Not found stays not found (whatever was missing); anything else (the database) is itself.
        let hide = |e: Error| if matches!(e, Error::NotFound(_)) { missing() } else { e };
        let (r, kind, number, title, body, state, outside, extra) = if id.starts_with("chg_") {
            let (r, c) = self.readable_change(caller, id).map_err(hide)?;
            let extra = json!({ "author": c.author, "branch": c.branch, "work_item": c.work_item, "github_url": c.github_url });
            (r, "change", c.number as i64, c.title, c.body, c.state.as_str().to_string(), c.outside, extra)
        } else if id.starts_with("wi_") {
            let (r, w) = self.readable_work(caller, id).map_err(hide)?;
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

    /// A file match as a document: the lines around it at the revision searched (up to
    /// [`FILE_LINES`]), on the repo's owner (the id names the repo).
    async fn file_document(&self, caller: &Caller, id: &str) -> Result<Value> {
        let missing = || Error::NotFound(format!("document {id}"));
        let (repo_id, rev, path, line) = parse_file_id(id).ok_or_else(missing)?;
        let r = self.db.repo_by_id(&repo_id)?.ok_or_else(missing)?;
        if !Self::can_read(caller, &r) || !Self::searchable(caller, &r) {
            return Err(missing());
        }
        let (cs, file) = self.blob(&r, &rev, &path).await.map_err(|e| if matches!(e, Error::NotFound(_)) { missing() } else { e })?;
        let text = String::from_utf8_lossy(&file.data);
        let lines: Vec<&str> = text.lines().collect();
        let line = (line as usize).clamp(1, lines.len().max(1));
        let from = line.saturating_sub(FILE_LINES / 10).max(1);
        let to = (from + FILE_LINES - 1).min(lines.len());
        let body = lines.get(from - 1..to).unwrap_or_default().join("\n");
        let rev = crate::cs_id(&cs);
        Ok(json!({
            "id": id,
            "title": format!("{}: {path}", r.full_name()),
            "text": body,
            "url": format!("{}/blob/{rev}/{path}#L{line}", self.web_url(&r)),
            "metadata": { "kind": "file", "repo": r.full_name(), "path": path, "rev": rev, "line": line, "from": from, "to": to, "lines": lines.len() },
            "repo": r.full_name(),
            "outside": false,
        }))
    }
}
