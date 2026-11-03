//! Public shares of agent threads (docs/code/agents.md, "Shares"): the owner freezes a copy of a
//! thread's records from the mirror (and, with `subtree`, of the subagent threads it delegated
//! to) and anyone with the link reads it at `/agents/s/<id>`. A share is a copy, not a view:
//! later work, a revoked machine or a deleted thread don't change it; revoking the share removes
//! it. Terminals, attachments' bytes and projects' repos and remotes are left out, home
//! directories become `~` and credentials `[redacted]`.

use super::api::{manageable, random_hex, readable};
use crate::api::Body;
use crate::{Caller, Code, Error, Result};
use agent_proto::{Entity, Relation, Share, ShareSummary, SharedMachine, Thread, ThreadId};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use code_sql::{OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_shares (
    -- shr_ and 24 hex characters: the public link, so it can't be guessed.
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    thread TEXT NOT NULL,
    title TEXT NOT NULL,
    subtree INTEGER NOT NULL,
    threads INTEGER NOT NULL,
    runs INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    -- The Share JSON, frozen.
    json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_shares_owner ON agent_shares (owner_id, created_at);
"#;

/// Largest copy (its JSON); past it tool outputs and diffs are shortened, then it's refused.
pub const MAX_SHARE_BYTES: usize = 4 << 20;
/// Shares one owner may have.
pub const MAX_SHARES: i64 = 500;
/// Threads one share may hold (the shared one and its subagents).
pub const MAX_SHARE_THREADS: usize = 64;
/// What's kept of each tool's output, and of each diff, when a copy is too large.
const TRIMMED_OUTPUT: usize = 4 << 10;
const TRIMMED_DIFF: usize = 32 << 10;

type S = State<Arc<Code>>;

// ── the copy ──

/// A thread's records read from the mirror, ready to store.
struct Copy {
    machine_id: String,
    share: Share,
    threads: u32,
    runs: u32,
}

fn parse(json: &str) -> Option<Entity> {
    serde_json::from_str(json).ok()
}

/// Read a thread (and its subagent threads) out of an owner's mirror. `None` if the owner has no
/// such thread.
fn copy(code: &Code, owner_id: &str, owner: &str, thread: &str, subtree: bool) -> Result<Option<Copy>> {
    let db = code.db.sql();
    let found: Option<(String, String)> = db
        .query_row("SELECT machine_id, json FROM agent_entities WHERE owner_id = ? AND kind = 'thread' AND id = ?", [owner_id, thread], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((machine_id, json)) = found else { return Ok(None) };
    let Some(Entity::Thread(root)) = parse(&json) else { return Ok(None) };
    let machine = super::store::machine(db, &machine_id)?;
    let home = machine.as_ref().and_then(|m| m.info.as_ref()).map(|i| i.home.clone());

    let mut threads = vec![root.clone()];
    if subtree {
        let all: Vec<Thread> = db
            .query("SELECT json FROM agent_entities WHERE owner_id = ? AND machine_id = ? AND kind = 'thread'", [owner_id, machine_id.as_str()], |r| r.get::<_, String>(0))?
            .iter()
            .filter_map(|j| match parse(j) {
                Some(Entity::Thread(t)) => Some(t),
                _ => None,
            })
            .collect();
        let mut children: HashMap<&ThreadId, Vec<&Thread>> = HashMap::new();
        for t in &all {
            if let Some(l) = t.lineage.as_ref().filter(|l| l.relation == Relation::Subagent) {
                children.entry(&l.parent).or_default().push(t);
            }
        }
        let mut i = 0;
        while i < threads.len() && threads.len() < MAX_SHARE_THREADS {
            let mut next: Vec<Thread> = children.get(&threads[i].id).map(|c| c.iter().map(|t| (*t).clone()).collect()).unwrap_or_default();
            next.sort_by_key(|t| t.created_at);
            next.truncate(MAX_SHARE_THREADS - threads.len());
            threads.extend(next);
            i += 1;
        }
    }

    let ids: Vec<code_sql::Value> = threads.iter().map(|t| code_sql::Value::Text(t.id.0.clone())).collect();
    let marks = vec!["?"; ids.len()].join(", ");
    let mut p = vec![code_sql::Value::Text(owner_id.into()), code_sql::Value::Text(machine_id.clone())];
    p.extend(ids);
    let rows: Vec<String> = db.query(
        &format!("SELECT json FROM agent_entities WHERE owner_id = ? AND machine_id = ? AND thread IN ({marks}) AND kind IN ('run', 'node', 'request', 'checkpoint')"),
        p,
        |r| r.get(0),
    )?;
    let project = db
        .query_row("SELECT json FROM agent_entities WHERE machine_id = ? AND kind = 'project' AND id = ?", [machine_id.as_str(), root.project.as_str()], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|j| parse(&j));

    let mut entities: Vec<Entity> = project.into_iter().collect();
    entities.extend(threads.iter().cloned().map(Entity::Thread));
    let mut details: Vec<Entity> = rows.iter().filter_map(|j| parse(j)).filter(|e| !matches!(e, Entity::Removed(_) | Entity::Terminal(_))).collect();
    details.sort_by_key(order);
    let runs = details.iter().filter(|e| matches!(e, Entity::Run(_))).count() as u32;
    entities.extend(details);
    for e in &mut entities {
        strip(e);
    }
    let entities = entities.into_iter().map(|e| redact_entity(e, home.as_deref())).collect();

    let share = Share {
        id: format!("shr_{}", random_hex(12)),
        owner: owner.into(),
        title: scrub(&root.title, home.as_deref()),
        thread: root.id.clone(),
        subtree,
        machine: SharedMachine { name: machine.as_ref().map_or_else(|| "machine".into(), |m| m.name.clone()), os: machine.and_then(|m| m.info).map(|i| i.os) },
        created_at: code_sql::now_ms().max(0) as u64,
        entities,
        trimmed: false,
    };
    Ok(Some(Copy { machine_id, share, threads: threads.len() as u32, runs }))
}

/// Runs, then requests and checkpoints, then nodes; each in the order it happened.
fn order(e: &Entity) -> (u8, u64, u32, String) {
    match e {
        Entity::Run(r) => (0, r.requested_at, r.ordinal, r.id.0.clone()),
        Entity::Request(r) => (1, r.created_at, 0, r.id.0.clone()),
        Entity::Checkpoint(c) => (2, c.created_at, c.ordinal, c.id.0.clone()),
        Entity::Node(n) => (3, n.started_at, n.ordinal, n.id.0.clone()),
        _ => (4, 0, 0, String::new()),
    }
}

/// What a share never holds: attachments' bytes (names and types stay), queued messages, what
/// roll code knows about a thread (its change and work item may be in a private repo), and a
/// project's repo and git state (a remote names a private repo, and could carry a credential).
fn strip(e: &mut Entity) {
    match e {
        Entity::Run(r) => r.attachments.iter_mut().for_each(|a| a.data.clear()),
        Entity::Thread(t) => {
            t.queued.clear();
            t.change = None;
            t.work = None;
        }
        Entity::Project(p) => {
            p.repo = None;
            p.git = None;
        }
        _ => {}
    }
}

/// Every string of a record: home directories become `~` and secrets `[redacted]`.
fn redact_entity(e: Entity, home: Option<&str>) -> Entity {
    let mut v = serde_json::to_value(&e).unwrap_or(Value::Null);
    redact(&mut v, home);
    serde_json::from_value(v).unwrap_or(e)
}

fn redact(v: &mut Value, home: Option<&str>) {
    match v {
        Value::String(s) => {
            let t = scrub(s, home);
            if t != *s {
                *s = t;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact(x, home)),
        Value::Object(o) => o.values_mut().for_each(|x| redact(x, home)),
        _ => {}
    }
}

/// What a public copy of `s` shows: [`secrets`] redacted, then [`tilde`].
pub(crate) fn scrub(s: &str, home: Option<&str>) -> String {
    tilde(&secrets(s), home)
}

fn path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '~' | '$')
}

/// `s` with the machine's home directory, and any `/home/<user>`, `/Users/<user>` or `/root`,
/// written as `~` wherever they are (`file:///home/me/…`, `-I/home/me/…`, `HOME=/root`). A public
/// copy would rather lose a path that only looks like a home than keep a user name.
pub(crate) fn tilde(s: &str, home: Option<&str>) -> String {
    let mut prefixes: Vec<String> = Vec::new();
    if let Some(h) = home.map(|h| h.trim_end_matches('/')).filter(|h| h.len() > 1) {
        prefixes.push(h.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let b = s.as_bytes();
    while i < s.len() {
        if b[i] == b'/'
            && let Some(len) = home_at(&s[i..], &prefixes)
        {
            out.push('~');
            i += len;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// The length of the home directory `s` starts with, if it does (and it ends there or at `/`).
fn home_at(s: &str, known: &[String]) -> Option<usize> {
    let ends = |n: usize| s[n..].chars().next().is_none_or(|c| !path_char(c) || c == '/');
    for h in known {
        if s.starts_with(h.as_str()) && ends(h.len()) {
            return Some(h.len());
        }
    }
    for base in ["/home/", "/Users/"] {
        if let Some(rest) = s.strip_prefix(base) {
            let user = rest.find(|c: char| !path_char(c) || c == '/').unwrap_or(rest.len());
            if user > 0 {
                return Some(base.len() + user);
            }
        }
    }
    if s.starts_with("/root") && ends(5) {
        return Some(5);
    }
    None
}

/// Credentials that look like what they are, kept as their prefix and `[redacted]`: roll's
/// tokens, GitHub's, OpenAI's and Anthropic's keys, Stripe's, Slack's and AWS access key ids,
/// plus whatever follows `Bearer`/`Basic` and a URL's userinfo. (prefix, characters after it at
/// least, only upper-case letters and digits after it.)
const SECRET_PREFIXES: &[(&str, usize, bool)] = &[
    ("rlm_", 8, false),
    ("rlg_", 8, false),
    ("rlc_", 8, false),
    ("rlx_", 8, false),
    ("rla_", 8, false),
    ("rld_", 8, false),
    ("ghp_", 8, false),
    ("gho_", 8, false),
    ("ghu_", 8, false),
    ("ghs_", 8, false),
    ("ghr_", 8, false),
    ("github_pat_", 8, false),
    ("sk-", 16, false),
    ("sk_live_", 8, false),
    ("sk_test_", 8, false),
    ("rk_live_", 8, false),
    ("xoxa-", 8, false),
    ("xoxb-", 8, false),
    ("xoxp-", 8, false),
    ("xoxr-", 8, false),
    ("xoxs-", 8, false),
    ("xoxe-", 8, false),
    ("AKIA", 16, true),
    ("ASIA", 16, true),
];

fn token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')
}

/// `s` with credentials replaced (see [`SECRET_PREFIXES`]).
pub(crate) fn secrets(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < b.len() {
        let starts = i == 0 || !token_char(b[i - 1]);
        if starts {
            for &(prefix, min, upper) in SECRET_PREFIXES {
                if b[i..].starts_with(prefix.as_bytes()) {
                    let rest = &b[i + prefix.len()..];
                    let n = rest.iter().take_while(|c| if upper { c.is_ascii_uppercase() || c.is_ascii_digit() } else { token_char(**c) }).count();
                    if n >= min {
                        out.push_str(prefix);
                        out.push_str("[redacted]");
                        i += prefix.len() + n;
                        continue 'scan;
                    }
                }
            }
            // `Bearer <token>`, `Basic <token>` (an Authorization header in a command or log).
            for scheme in ["bearer", "basic"] {
                if b.len() > i + scheme.len() && b[i..i + scheme.len()].eq_ignore_ascii_case(scheme.as_bytes()) && matches!(b[i + scheme.len()], b' ' | b'\t') {
                    let start = i + scheme.len() + 1;
                    let start = start + b[start..].iter().take_while(|c| **c == b' ').count();
                    let n = b[start..].iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')).count();
                    if n >= 8 {
                        out.push_str(&s[i..start]);
                        out.push_str("[redacted]");
                        i = start + n;
                        continue 'scan;
                    }
                }
            }
        }
        // A URL's userinfo: `scheme://user:password@host` → `scheme://[redacted]@host`.
        if b[i..].starts_with(b"://") {
            let start = i + 3;
            let authority = b[start..].iter().take_while(|c| !matches!(c, b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'"' | b'\'' | b'<' | b'>')).count();
            if let Some(at) = b[start..start + authority].iter().rposition(|c| *c == b'@')
                && at > 0
            {
                out.push_str("://[redacted]");
                i = start + at;
                continue 'scan;
            }
        }
        let c = s[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// The middle of `s` cut out to keep about `max` bytes.
fn cut(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut head = max / 2;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - max / 2;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    *s = format!("{}\n… {} bytes left out of this share …\n{}", &s[..head], tail - head, &s[tail..]);
    true
}

/// Shorten tool outputs and diffs. True if anything changed.
fn trim(share: &mut Share) -> bool {
    let mut changed = false;
    for e in &mut share.entities {
        if let Entity::Node(n) = e
            && let agent_proto::NodeBody::Tool(t) = &mut n.body
        {
            if let Some(o) = &mut t.output {
                changed |= cut(o, TRIMMED_OUTPUT);
            }
            for f in &mut t.files {
                if let Some(d) = &mut f.diff {
                    changed |= cut(d, TRIMMED_DIFF);
                }
            }
        }
    }
    changed
}

// ── the table ──

const SUMMARY_COLS: &str = "id, title, thread, subtree, threads, runs, bytes, created_at, created_by";

fn summary(code: &Code, r: &code_sql::Row<'_>) -> code_sql::Result<ShareSummary> {
    let id: String = r.get(0)?;
    Ok(ShareSummary {
        url: share_url(code, &id),
        id,
        title: r.get(1)?,
        thread: ThreadId(r.get(2)?),
        subtree: r.get::<_, i64>(3)? != 0,
        threads: r.get::<_, i64>(4)?.max(0) as u32,
        runs: r.get::<_, i64>(5)?.max(0) as u32,
        bytes: r.get::<_, i64>(6)?.max(0) as u64,
        created_at: r.get::<_, i64>(7)?.max(0) as u64,
        created_by: r.get(8)?,
    })
}

fn share_url(code: &Code, id: &str) -> String {
    format!("{}/agents/s/{id}", code.config.web_url.trim_end_matches('/'))
}

// ── routes ──

/// `POST /v1/code/agents/{owner}/threads/{thread}/shares {subtree?}`: freeze a copy of the thread.
pub(super) async fn create(State(code): S, caller: Caller, Path((owner, thread)): Path<(String, String)>, Body(b): Body) -> Result<(StatusCode, Json<ShareSummary>)> {
    let o = manageable(&code, &caller, &owner)?;
    let subtree = match b.get("subtree") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(v)) => *v,
        Some(_) => return Err(Error::Invalid("subtree must be a boolean".into())),
    };
    let Some(mut c) = copy(&code, &o.id, &o.name, &thread, subtree)? else {
        return Err(Error::NotFound(format!("thread {thread}")));
    };
    let mut json = serde_json::to_string(&c.share).map_err(|e| Error::Internal(e.into()))?;
    if json.len() > MAX_SHARE_BYTES && trim(&mut c.share) {
        c.share.trimmed = true;
        json = serde_json::to_string(&c.share).map_err(|e| Error::Internal(e.into()))?;
    }
    if json.len() > MAX_SHARE_BYTES {
        return Err(Error::Unprocessable(format!("this thread is too large to share ({} MB; at most {} MB)", json.len() >> 20, MAX_SHARE_BYTES >> 20)));
    }
    let by = caller.identity().unwrap_or_else(|| "anonymous".into());
    let s = &c.share;
    let added = code.db.sql().write(|db| {
        let n: i64 = db.query_row("SELECT COUNT(*) FROM agent_shares WHERE owner_id = ?", [&o.id], |r| r.get(0))?;
        if n >= MAX_SHARES {
            return Ok(false);
        }
        db.execute(
            "INSERT INTO agent_shares (id, owner_id, machine_id, thread, title, subtree, threads, runs, bytes, created_by, created_at, json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![s.id, o.id, c.machine_id, s.thread.0, s.title, s.subtree as i64, c.threads as i64, c.runs as i64, json.len() as i64, by, s.created_at as i64, json],
        )?;
        Ok::<_, code_sql::Error>(true)
    })?;
    if !added {
        return Err(Error::Unprocessable(format!("{owner} has {MAX_SHARES} shares; revoke one first")));
    }
    let summary = ShareSummary {
        id: s.id.clone(),
        title: s.title.clone(),
        thread: s.thread.clone(),
        subtree: s.subtree,
        url: share_url(&code, &s.id),
        threads: c.threads,
        runs: c.runs,
        bytes: json.len() as u64,
        created_at: s.created_at,
        created_by: by,
    };
    Ok((StatusCode::CREATED, Json(summary)))
}

/// `GET /v1/code/agents/{owner}/shares`: newest first.
pub(super) async fn list(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Vec<ShareSummary>>> {
    let o = readable(&code, &caller, &owner)?;
    let rows = code.db.sql().query(&format!("SELECT {SUMMARY_COLS} FROM agent_shares WHERE owner_id = ? ORDER BY created_at DESC, id"), [&o.id], |r| summary(&code, r))?;
    Ok(Json(rows))
}

/// `DELETE /v1/code/agents/{owner}/shares/{id}`: the link stops working at once.
pub(super) async fn revoke(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    if code.db.sql().execute("DELETE FROM agent_shares WHERE id = ? AND owner_id = ?", [&id, &o.id])? == 0 {
        return Err(Error::NotFound(format!("share {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/code/agents/shares/{id}`: the copy, for anyone with the link. Never cached, so a
/// revoked share is gone at once.
pub(super) async fn public(State(code): S, Path(id): Path<String>) -> Result<Response> {
    let json: Option<String> = code.db.sql().query_row("SELECT json FROM agent_shares WHERE id = ?", [&id], |r| r.get(0)).optional()?;
    let json = json.ok_or_else(|| Error::NotFound(format!("share {id}")))?;
    Ok(([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], json).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_directories_become_tilde() {
        let home = Some("/home/ada");
        assert_eq!(tilde("/home/ada/src/roll", home), "~/src/roll");
        assert_eq!(tilde("cd /home/ada && ls /home/ada", home), "cd ~ && ls ~");
        assert_eq!(tilde("\"/Users/bob/x\" and /home/carol", None), "\"~/x\" and ~");
        assert_eq!(tilde("/root/.cargo and /rooted", None), "~/.cargo and /rooted");
        // Wherever they are: after a scheme, a flag, a variable, another path.
        assert_eq!(tilde("file:///home/ada/x.rs", home), "file://~/x.rs");
        assert_eq!(tilde("cc -I/home/ada/include -L/Users/bob/lib", home), "cc -I~/include -L~/lib");
        assert_eq!(tilde("HOME=/root PATH=/x:/home/ada/bin", None), "HOME=~ PATH=/x:~/bin");
        assert_eq!(tilde("a/home/ada and /srv/home/ada/x", home), "a~ and /srv~/x");
        assert_eq!(tilde("/home/adam/x", Some("/home/ada")), "~/x");
        // No user: left alone.
        assert_eq!(tilde("/home/ and /Users/", None), "/home/ and /Users/");
        // A home outside /home and /Users (a container, a sandbox), anywhere.
        assert_eq!(tilde("/workspace/me/p:/workspace/me", Some("/workspace/me/")), "~/p:~");
        assert_eq!(tilde("--root=x/workspace/me/y", Some("/workspace/me")), "--root=x~/y");
        assert_eq!(tilde("/workspace/meh", Some("/workspace/me")), "/workspace/meh");
        assert_eq!(tilde("ünïcode /home/ada/é", home), "ünïcode ~/é");
    }

    #[test]
    fn credentials_are_redacted() {
        let table = [
            ("git push https://ada:ghp_0123456789abcdefABCDEF@github.com/ada/x.git", "git push https://[redacted]@github.com/ada/x.git"),
            ("remote: http://tok@example.com/x", "remote: http://[redacted]@example.com/x"),
            ("ssh://git:pw@host:22/x and https://host/a@b", "ssh://[redacted]@host:22/x and https://host/a@b"),
            ("export GITHUB_TOKEN=ghp_0123456789abcdef", "export GITHUB_TOKEN=ghp_[redacted]"),
            ("github_pat_11ABCDEFG0123456789_abc", "github_pat_[redacted]"),
            ("ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnop", "ANTHROPIC_API_KEY=sk-[redacted]"),
            ("OPENAI_API_KEY='sk-proj-abcdefghijklmnopqrst'", "OPENAI_API_KEY='sk-[redacted]'"),
            ("pip install sk-learn", "pip install sk-learn"),
            ("stripe sk_live_51Habcdefghij and rk_live_0123456789", "stripe sk_live_[redacted] and rk_live_[redacted]"),
            ("slack xoxb-1234-5678-abcdefgh", "slack xoxb-[redacted]"),
            ("aws AKIAIOSFODNN7EXAMPLE secret", "aws AKIA[redacted] secret"),
            ("AKIAlowercase0123456789 ASIAABCDEFGHIJKLMNOP", "AKIAlowercase0123456789 ASIA[redacted]"),
            ("roll rlm_0123456789abcdef rlg_aaaaaaaabbbb rlc_x rlx_0123456789ab", "roll rlm_[redacted] rlg_[redacted] rlc_x rlx_[redacted]"),
            ("curl -H 'Authorization: Bearer eyJhbGciOi.J9.abc_def' x", "curl -H 'Authorization: Bearer [redacted]' x"),
            ("authorization: basic dXNlcjpwYXNzd29yZA==", "authorization: basic [redacted]"),
            ("a bearer of news, Basic types", "a bearer of news, Basic types"),
            ("myghp_0123456789abcdef stays", "myghp_0123456789abcdef stays"),
            ("ünïcode ghp_0123456789abcdef é", "ünïcode ghp_[redacted] é"),
        ];
        for (input, want) in table {
            assert_eq!(secrets(input), want, "{input}");
        }
        assert_eq!(scrub("cat /home/ada/.env # rla_0123456789abcdef", Some("/home/ada")), "cat ~/.env # rla_[redacted]");
    }

    #[test]
    fn cutting_keeps_both_ends_on_char_boundaries() {
        let mut s = "é".repeat(5000);
        assert!(cut(&mut s, 1000));
        assert!(s.len() < 1200 && s.contains("bytes left out"));
        let mut short = "short".to_string();
        assert!(!cut(&mut short, 1000));
    }
}
