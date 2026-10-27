//! ScyllaDB's audit logging (`docs/operating-scylla/security/auditing.rst`): CQL statements,
//! logins and Alternator requests recorded in `audit.audit_log`.
//!
//! - **Settings.** `audit` (not live; `table` in the reference image) picks the sinks: roll
//!   writes the `table` sink and accepts `syslog` without writing it. `audit_categories`,
//!   `audit_keyspaces`, `audit_tables` and `audit_rules` are live (`UPDATE system.config`,
//!   [`crate::guardrails::config_table`]); they live in [`Config`].
//! - **Filtering** ([`Config::wants`]). An event is logged when its category is in
//!   `audit_categories` and its keyspace is empty (DCL, AUTH, ADMIN, ListTables, batches), in
//!   `audit_keyspaces`, or its `keyspace.table` is in `audit_tables` (`alternator.T` names the
//!   Alternator table `T`, keyspace `alternator_T`); or when an `audit_rules` rule with the
//!   `table` sink matches its category and role, and — for the table-scoped categories (DML,
//!   DDL, QUERY) — its `keyspace.table` (fnmatch with `FNM_EXTMATCH` patterns, [`glob`]).
//!   Rules only see events with a table context.
//! - **CQL** ([`statement`]): one row per statement after it ran, `operation` the query text,
//!   `error` whether it failed; requests refused as unauthorized are not logged (the reference
//!   checks access before auditing), nor are statements that don't parse. A batch logs a row per
//!   statement it holds. Logins ([`login`]) are `AUTH` rows with operation `LOGIN`.
//! - **Rows.** `date` is the day (UTC midnight) of `event_time` (a timeuuid), `node` the
//!   database's address, `source` the client's, `username` the role (`anonymous` without one).
//!   They are written before the response returns, like the reference does.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast::{BatchStatement, QualifiedName, Statement, StatementKind};
use nosql_types::Dialect;

use crate::database::Database;
use crate::error::{ErrorCode, Result};
use crate::response::{Param, Response};
use crate::session::SessionState;
use crate::trace::{internal, p_bigint, p_inet, p_text, p_uuid};

/// Statement categories (`audit_categories`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Query,
    Dml,
    Ddl,
    Dcl,
    Auth,
    Admin,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Query,
        Category::Dml,
        Category::Ddl,
        Category::Dcl,
        Category::Auth,
        Category::Admin,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Query => "QUERY",
            Category::Dml => "DML",
            Category::Ddl => "DDL",
            Category::Dcl => "DCL",
            Category::Auth => "AUTH",
            Category::Admin => "ADMIN",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Category::ALL
            .into_iter()
            .find(|c| c.name().eq_ignore_ascii_case(s.trim()))
    }

    /// Whether rules match the event's table (the other categories ignore tables).
    fn table_scoped(self) -> bool {
        matches!(self, Category::Query | Category::Dml | Category::Ddl)
    }
}

/// One `audit_rules` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub sinks: Vec<String>,
    pub categories: Vec<Category>,
    /// `keyspace.table` patterns.
    pub qualified_table_names: Vec<String>,
    /// Role patterns.
    pub roles: Vec<String>,
}

/// The audit settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `audit` includes `table`.
    pub table_sink: bool,
    /// The live options as set (what `system.config` shows, JSON-quoted).
    pub categories_text: String,
    pub keyspaces_text: String,
    pub tables_text: String,
    pub categories: BTreeSet<Category>,
    pub keyspaces: BTreeSet<String>,
    pub tables: BTreeSet<(String, String)>,
    pub rules: Vec<Rule>,
}

impl Default for Config {
    /// The reference image's `scylla.yaml`: `audit: "table"`, `audit_categories:
    /// "DCL,DDL,AUTH,ADMIN"`.
    fn default() -> Self {
        let mut c = Config {
            table_sink: true,
            categories_text: String::new(),
            keyspaces_text: String::new(),
            tables_text: String::new(),
            categories: BTreeSet::new(),
            keyspaces: BTreeSet::new(),
            tables: BTreeSet::new(),
            rules: Vec::new(),
        };
        c.set_categories("DCL,DDL,AUTH,ADMIN");
        c
    }
}

fn items(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty())
}

impl Config {
    /// `audit_categories`: a comma-separated list (unknown names are ignored).
    pub fn set_categories(&mut self, text: &str) {
        self.categories_text = text.to_string();
        self.categories = items(text).filter_map(Category::parse).collect();
    }

    /// `audit_keyspaces`: a comma-separated list.
    pub fn set_keyspaces(&mut self, text: &str) {
        self.keyspaces_text = text.to_string();
        self.keyspaces = items(text).map(str::to_string).collect();
    }

    /// `audit_tables`: comma-separated `keyspace.table` names; `alternator.T` is Alternator's
    /// table `T` (keyspace `alternator_T`).
    pub fn set_tables(&mut self, text: &str) {
        self.tables_text = text.to_string();
        self.tables = items(text)
            .filter_map(|n| {
                let (ks, t) = n.split_once('.')?;
                let ks = if ks == "alternator" {
                    format!("alternator_{t}")
                } else {
                    ks.to_string()
                };
                Some((ks, t.to_string()))
            })
            .collect();
    }

    /// `audit_rules`: a JSON (YAML flow) list of `{sinks, categories, qualified_table_names,
    /// roles}` objects; `Err` when it isn't one.
    pub fn set_rules(&mut self, text: &str) -> std::result::Result<(), &'static str> {
        use nosql_types::json::Json;
        const PARSE: &str = "cannot parse option value";
        let text = text.trim();
        let parsed = if text.is_empty() {
            Json::List(Vec::new())
        } else {
            nosql_types::json::parse_json(text, Dialect::Scylla).map_err(|_| PARSE)?
        };
        let Json::List(list) = parsed else {
            return Err(PARSE);
        };
        let strings = |v: Option<&Json>| -> std::result::Result<Vec<String>, &'static str> {
            match v {
                None | Some(Json::Null) => Ok(Vec::new()),
                Some(Json::List(l)) => l
                    .iter()
                    .map(|e| match e {
                        Json::Str(s) => Ok(s.clone()),
                        _ => Err(PARSE),
                    })
                    .collect(),
                _ => Err(PARSE),
            }
        };
        let mut rules = Vec::new();
        for r in &list {
            let Json::Map(m) = r else {
                return Err(PARSE);
            };
            let get = |k: &str| m.iter().find(|(n, _)| n == k).map(|(_, v)| v);
            if m.iter().any(|(n, _)| {
                !matches!(
                    n.as_str(),
                    "sinks" | "categories" | "qualified_table_names" | "roles"
                )
            }) {
                return Err(PARSE);
            }
            let categories = strings(get("categories"))?
                .iter()
                .map(|c| Category::parse(c).ok_or(PARSE))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rules.push(Rule {
                sinks: strings(get("sinks"))?,
                categories,
                qualified_table_names: strings(get("qualified_table_names"))?,
                roles: strings(get("roles"))?,
            });
        }
        self.rules = rules;
        Ok(())
    }

    /// `audit_rules` as `system.config` shows it.
    pub fn rules_text(&self) -> String {
        let list = |v: &[String]| {
            let q: Vec<String> = v.iter().map(|s| nosql_types::json::quote_json(s)).collect();
            format!("[{}]", q.join(","))
        };
        let rules: Vec<String> = self
            .rules
            .iter()
            .map(|r| {
                let cats: Vec<String> = r.categories.iter().map(|c| c.name().to_string()).collect();
                format!(
                    "{{\"sinks\":{},\"categories\":{},\"qualified_table_names\":{},\"roles\":{}}}",
                    list(&r.sinks),
                    list(&cats),
                    list(&r.qualified_table_names),
                    list(&r.roles)
                )
            })
            .collect();
        format!("[{}]", rules.join(","))
    }

    /// Whether an event goes to the table sink (`keyspace` and `table` empty: no table
    /// context; `user` as the row shows it).
    pub fn wants(&self, category: Category, keyspace: &str, table: &str, user: &str) -> bool {
        if !self.table_sink {
            return false;
        }
        if self.categories.contains(&category)
            && (keyspace.is_empty()
                || self.keyspaces.contains(keyspace)
                || self
                    .tables
                    .contains(&(keyspace.to_string(), table.to_string())))
        {
            return true;
        }
        if keyspace.is_empty() && category.table_scoped() {
            return false;
        }
        let qualified = format!("{keyspace}.{table}");
        self.rules.iter().any(|r| {
            r.sinks.iter().any(|s| s == "table")
                && r.categories.contains(&category)
                && r.roles.iter().any(|p| glob(p, user))
                && (!category.table_scoped()
                    || r.qualified_table_names.iter().any(|p| glob(p, &qualified)))
        })
    }

    /// Whether any event of `category` can be logged (a cheap check before building one).
    pub fn may_log(&self, category: Category) -> bool {
        self.table_sink
            && (self.categories.contains(&category)
                || self.rules.iter().any(|r| r.categories.contains(&category)))
    }
}

/// `fnmatch(pattern, s, FNM_EXTMATCH)`: `*`, `?`, `[...]` (`!` or `^` negates), `\` escapes,
/// and the extended forms `?(a|b)`, `*(…)`, `+(…)`, `@(…)` and `!(…)`.
pub fn glob(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    glob_at(&p, &s)
}

/// The end (index of the closing `)`) of the extended group opening at `p[i]` (`p[i + 1]` is
/// `(`), and its `|`-separated alternatives.
fn ext_group(p: &[char], i: usize) -> Option<(usize, Vec<&[char]>)> {
    let mut depth = 0;
    let mut start = i + 2;
    let mut alts = Vec::new();
    let mut j = i + 1;
    while j < p.len() {
        match p[j] {
            '\\' => j += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    alts.push(&p[start..j]);
                    return Some((j, alts));
                }
            }
            '|' if depth == 1 => {
                alts.push(&p[start..j]);
                start = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn any_alt(alts: &[&[char]], s: &[char]) -> bool {
    alts.iter().any(|a| glob_at(a, s))
}

fn glob_at(p: &[char], s: &[char]) -> bool {
    let Some(&c) = p.first() else {
        return s.is_empty();
    };
    if matches!(c, '?' | '*' | '+' | '@' | '!')
        && p.get(1) == Some(&'(')
        && let Some((end, alts)) = ext_group(p, 0)
    {
        let rest = &p[end + 1..];
        return match c {
            '@' => (0..=s.len()).any(|k| any_alt(&alts, &s[..k]) && glob_at(rest, &s[k..])),
            '?' => {
                glob_at(rest, s)
                    || (0..=s.len()).any(|k| any_alt(&alts, &s[..k]) && glob_at(rest, &s[k..]))
            }
            // Zero or more: one alternative of at least one character, then the group again.
            '*' => {
                glob_at(rest, s)
                    || (1..=s.len()).any(|k| any_alt(&alts, &s[..k]) && glob_at(p, &s[k..]))
            }
            // One or more: an alternative, then `*(…)` of the same group.
            '+' => {
                let mut again = vec!['*'];
                again.extend_from_slice(&p[1..]);
                (0..=s.len()).any(|k| any_alt(&alts, &s[..k]) && glob_at(&again, &s[k..]))
            }
            // Anything that is none of the alternatives.
            _ => (0..=s.len()).any(|k| !any_alt(&alts, &s[..k]) && glob_at(rest, &s[k..])),
        };
    }
    match c {
        '*' => (0..=s.len()).any(|k| glob_at(&p[1..], &s[k..])),
        '?' => !s.is_empty() && glob_at(&p[1..], &s[1..]),
        '[' => {
            let Some(&ch) = s.first() else {
                return false;
            };
            match class(p, ch) {
                Some((matched, end)) => matched && glob_at(&p[end..], &s[1..]),
                // An unterminated `[` is a literal.
                None => ch == '[' && glob_at(&p[1..], &s[1..]),
            }
        }
        '\\' if p.len() > 1 => s.first() == Some(&p[1]) && glob_at(&p[2..], &s[1..]),
        _ => s.first() == Some(&c) && glob_at(&p[1..], &s[1..]),
    }
}

/// Whether `ch` is in the bracket expression at the start of `p`, and where it ends.
fn class(p: &[char], ch: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        let c = p[i];
        if c == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
            if (c..=p[i + 2]).contains(&ch) {
                matched = true;
            }
            i += 3;
            continue;
        }
        if c == ch {
            matched = true;
        }
        i += 1;
    }
    None
}

/// One audited event.
#[derive(Clone, Debug)]
pub struct Event<'a> {
    pub category: Category,
    /// The consistency level's name (empty for requests without one).
    pub consistency: &'a str,
    pub keyspace: &'a str,
    pub table: &'a str,
    pub operation: &'a str,
    pub error: bool,
    /// The role (`None`: anonymous).
    pub user: Option<&'a str>,
    pub source: Option<IpAddr>,
}

/// The name a role is matched and shown as.
pub fn username(user: Option<&str>) -> &str {
    user.unwrap_or("anonymous")
}

impl Database {
    /// Whether an event would be written (ScyllaDB databases only).
    pub fn audit_wants(
        &self,
        category: Category,
        keyspace: &str,
        table: &str,
        user: Option<&str>,
    ) -> bool {
        self.dialect().is_scylla()
            && self
                .settings()
                .scylla
                .audit
                .wants(category, keyspace, table, username(user))
    }

    /// Whether any event of `category` can be logged now.
    pub fn audit_may_log(&self, category: Category) -> bool {
        self.dialect().is_scylla() && self.settings().scylla.audit.may_log(category)
    }
}

/// Writes an event to `audit.audit_log` (the caller checked [`Database::audit_wants`]). A
/// failed write is logged, never returned: auditing doesn't fail requests.
pub async fn write(db: &Arc<Database>, e: &Event<'_>) {
    if let Err(err) = try_write(db, e).await {
        tracing::warn!(db = %db.config().id, error = %err.message, "writing an audit row failed");
    }
}

async fn try_write(db: &Arc<Database>, e: &Event<'_>) -> Result<()> {
    let node = db.config().address;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let day = now_ms - now_ms.rem_euclid(86_400_000);
    let values = vec![
        p_bigint(day),
        p_inet(&node),
        p_uuid(&crate::uuid::timeuuid()),
        p_text(e.category.name()),
        p_text(e.consistency),
        Param::new(Bytes::from_static(if e.error { &[1] } else { &[0] })),
        p_text(e.keyspace),
        p_text(e.operation),
        p_inet(&e.source.unwrap_or(node)),
        p_text(e.table),
        p_text(username(e.user)),
    ];
    internal(
        db,
        "INSERT INTO audit.audit_log (date, node, event_time, category, consistency, error, keyspace_name, operation, source, table_name, username) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        values,
    )
    .await
}

/// What a CQL statement is audited as: its category, keyspace and table (`None`: not
/// audited).
pub fn statement_info(
    kind: &StatementKind<'_>,
    session_keyspace: Option<&str>,
) -> Option<(Category, String, String)> {
    use Category::*;
    let named = |q: &QualifiedName<'_>| {
        let ks = q
            .keyspace
            .as_ref()
            .map(|k| k.name().to_string())
            .or_else(|| session_keyspace.map(str::to_string))
            .unwrap_or_default();
        (ks, q.name.name().to_string())
    };
    let with = |c: Category, (ks, t): (String, String)| Some((c, ks, t));
    let none = |c: Category| Some((c, String::new(), String::new()));
    match kind {
        StatementKind::Select(s) => with(Query, named(&s.table)),
        StatementKind::Insert(s) => with(Dml, named(&s.table)),
        StatementKind::Update(s) => with(Dml, named(&s.table)),
        StatementKind::Delete(s) => with(Dml, named(&s.table)),
        StatementKind::Truncate(s) => with(Dml, named(&s.table)),
        StatementKind::Batch(_) => none(Dml),
        StatementKind::CreateKeyspace(s) => Some((Ddl, s.name.name().to_string(), String::new())),
        StatementKind::AlterKeyspace(s) => Some((Ddl, s.name.name().to_string(), String::new())),
        StatementKind::DropKeyspace(s) => {
            Some((Ddl, s.name.name.name().to_string(), String::new()))
        }
        StatementKind::CreateTable(s) => with(Ddl, named(&s.name)),
        StatementKind::AlterTable(s) => with(Ddl, named(&s.name)),
        StatementKind::DropTable(s) | StatementKind::DropView(s) => with(Ddl, named(&s.name)),
        StatementKind::CreateIndex(s) => with(Ddl, named(&s.table)),
        StatementKind::DropIndex(s) | StatementKind::DropType(s) => {
            let (ks, _) = named(&s.name);
            Some((Ddl, ks, String::new()))
        }
        StatementKind::CreateType(s) => Some((Ddl, named(&s.name).0, String::new())),
        StatementKind::AlterType(s) => Some((Ddl, named(&s.name).0, String::new())),
        StatementKind::CreateView(s) => with(Ddl, named(&s.name)),
        StatementKind::AlterView(s) => with(Ddl, named(&s.name)),
        // Functions carry no keyspace (observed: a failed CREATE FUNCTION is logged with none).
        StatementKind::CreateFunction(_)
        | StatementKind::DropFunction(_)
        | StatementKind::CreateAggregate(_)
        | StatementKind::DropAggregate(_)
        | StatementKind::CreateTrigger(_)
        | StatementKind::DropTrigger(_) => none(Ddl),
        StatementKind::Grant(_)
        | StatementKind::Revoke(_)
        | StatementKind::ListPermissions(_)
        | StatementKind::CreateRole(_)
        | StatementKind::AlterRole(_)
        | StatementKind::DropRole(_)
        | StatementKind::ListRoles(_)
        | StatementKind::GrantRole(_)
        | StatementKind::RevokeRole(_)
        | StatementKind::CreateUser(_)
        | StatementKind::AlterUser(_)
        | StatementKind::DropUser(_)
        | StatementKind::ListUsers
        | StatementKind::AddIdentity(_)
        | StatementKind::DropIdentity(_) => none(Dcl),
        StatementKind::CreateServiceLevel(_)
        | StatementKind::AlterServiceLevel(_)
        | StatementKind::DropServiceLevel(_)
        | StatementKind::AttachServiceLevel(_)
        | StatementKind::DetachServiceLevel(_)
        | StatementKind::ListServiceLevels(_) => none(Admin),
        StatementKind::Use(_) | StatementKind::Describe(_) | StatementKind::PruneView(_) => None,
    }
}

/// Whether a statement's outcome is audited: refusals for missing permissions aren't.
fn audited(r: &Result<Response>) -> bool {
    !matches!(r, Err(e) if matches!(e.code, ErrorCode::Unauthorized | ErrorCode::BadCredentials))
}

/// Audits a CQL statement (`text`: what the client sent) that ran with outcome `r`.
/// `keyspace` is the one unqualified names resolve against.
pub async fn statement(
    db: &Arc<Database>,
    state: &SessionState,
    stmt: &Statement<'_>,
    keyspace: Option<&str>,
    text: &str,
    consistency: crate::error::Consistency,
    r: &Result<Response>,
) {
    if !db.dialect().is_scylla() || !audited(r) {
        return;
    }
    let ks = keyspace;
    let mut events: Vec<(Category, String, String)> = Vec::new();
    match &stmt.kind {
        StatementKind::Batch(b) => {
            if !db.audit_may_log(Category::Dml) {
                return;
            }
            for s in &b.statements {
                let t = match s {
                    BatchStatement::Insert(i) => &i.table,
                    BatchStatement::Update(u) => &u.table,
                    BatchStatement::Delete(d) => &d.table,
                };
                let ks = t
                    .keyspace
                    .as_ref()
                    .map(|k| k.name().to_string())
                    .or_else(|| ks.map(str::to_string))
                    .unwrap_or_default();
                events.push((Category::Dml, ks, t.name.name().to_string()));
            }
        }
        kind => {
            let Some(info) = statement_info(kind, ks) else {
                return;
            };
            if !db.audit_may_log(info.0) {
                return;
            }
            events.push(info);
        }
    }
    let error = r.is_err();
    for (category, keyspace, table) in &events {
        if db.audit_wants(*category, keyspace, table, state.user.as_deref()) {
            let e = Event {
                category: *category,
                consistency: consistency.name(),
                keyspace,
                table,
                operation: text,
                error,
                user: state.user.as_deref(),
                source: state.client.address.map(|a| a.ip()),
            };
            write(db, &e).await;
        }
    }
}

/// Audits a statement that failed before it was prepared (it is parsed again to learn its
/// category; one that doesn't parse isn't logged).
pub async fn unprepared(
    db: &Arc<Database>,
    state: &SessionState,
    text: &str,
    consistency: crate::error::Consistency,
    r: &Result<Response>,
) {
    if !db.dialect().is_scylla() || !audited(r) {
        return;
    }
    if !Category::ALL.iter().any(|c| db.audit_may_log(*c)) {
        return;
    }
    if let Ok(stmt) = crate::session::parse(text, db.dialect()) {
        let ks = state.keyspace.as_deref();
        statement(db, state, &stmt, ks, text, consistency, r).await;
    }
}

/// The statements of a protocol batch as they are audited (statement, keyspace, text); none
/// when DML isn't audited.
pub fn batch_statements(
    db: &Database,
    state: &SessionState,
    b: &crate::response::Batch,
) -> Vec<(Arc<Statement<'static>>, Option<String>, String)> {
    if !db.audit_may_log(Category::Dml) {
        return Vec::new();
    }
    b.statements
        .iter()
        .filter_map(|s| match s {
            crate::response::BatchStatement::Query { text, .. } => {
                let stmt = crate::session::parse(text, db.dialect()).ok()?;
                Some((Arc::new(stmt), state.keyspace.clone(), text.clone()))
            }
            crate::response::BatchStatement::Prepared { id, .. } => {
                let p = db.get_prepared(id)?;
                Some((p.stmt.clone(), p.keyspace.clone(), p.text.clone()))
            }
        })
        .collect()
}

/// Audits a login attempt (`AUTH`): `user` the role it named (`None`: none was named).
pub async fn login(db: &Arc<Database>, state: &SessionState, user: Option<&str>, error: bool) {
    if !db.audit_wants(Category::Auth, "", "", Some(user.unwrap_or(""))) {
        return;
    }
    let e = Event {
        category: Category::Auth,
        consistency: "",
        keyspace: "",
        table: "",
        operation: "LOGIN",
        error,
        user: Some(user.unwrap_or("")),
        source: state.client.address.map(|a| a.ip()),
    };
    write(db, &e).await;
}

/// The user a SASL PLAIN token names (`authzid \0 authcid \0 password`).
pub fn plain_user(sasl: &[u8]) -> Option<String> {
    let mut parts = sasl.split(|b| *b == 0);
    let _authzid = parts.next()?;
    let user = parts.next()?;
    Some(String::from_utf8_lossy(user).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        for (p, s, want) in [
            ("*", "", true),
            ("alternator_t.*", "alternator_t.t", true),
            ("alternator_t.*", "alternator_u.u", false),
            ("a?c", "abc", true),
            ("a[b-d]c", "acc", true),
            ("a[!b-d]c", "acc", false),
            ("a[^x]c", "abc", true),
            ("!(system).*", "ks.t", true),
            ("!(system).*", "system.t", false),
            ("@(a|b).t", "b.t", true),
            ("@(a|b).t", "c.t", false),
            ("+(ab).t", "abab.t", true),
            ("+(ab).t", ".t", false),
            ("*(ab).t", ".t", true),
            ("?(x)y", "y", true),
            ("?(x)y", "xy", true),
            ("?(x)y", "xxy", false),
            ("admin_*", "admin_1", true),
            ("admin_*", "bob", false),
            ("\\*", "*", true),
            ("\\*", "a", false),
            ("", "", true),
            ("[", "[", true),
        ] {
            assert_eq!(glob(p, s), want, "{p} {s}");
        }
    }

    #[test]
    fn filtering() {
        let mut c = Config::default();
        // DCL, DDL, AUTH, ADMIN without keyspaces: only events without a keyspace.
        assert!(c.wants(Category::Dcl, "", "", "anonymous"));
        assert!(!c.wants(Category::Ddl, "ks", "t", "anonymous"));
        assert!(c.wants(Category::Ddl, "", "", "anonymous"));
        assert!(!c.wants(Category::Dml, "", "", "anonymous"));
        c.set_categories("ADMIN,AUTH,QUERY,DML,DDL,DCL");
        c.set_keyspaces("ks");
        assert!(c.wants(Category::Dml, "ks", "t", "u"));
        assert!(!c.wants(Category::Dml, "ks2", "t", "u"));
        c.set_keyspaces("");
        c.set_tables("alternator.T, ks.t");
        assert!(c.wants(Category::Query, "alternator_T", "T", "u"));
        assert!(c.wants(Category::Query, "ks", "t", "u"));
        assert!(!c.wants(Category::Query, "ks", "u", "u"));
        c.set_categories("");
        c.set_tables("");
        c.set_rules(r#"[{"sinks": ["table"], "categories": ["DML", "QUERY"], "qualified_table_names": ["ks.*"], "roles": ["*"]}]"#)
            .unwrap();
        assert!(c.wants(Category::Dml, "ks", "t", "cassandra"));
        assert!(!c.wants(Category::Dml, "ks2", "t", "cassandra"));
        assert!(!c.wants(Category::Ddl, "ks", "t", "cassandra"));
        // Rules need a table context.
        assert!(!c.wants(Category::Query, "", "", "cassandra"));
        c.set_rules(r#"[{"sinks": ["syslog"], "categories": ["DML"], "qualified_table_names": ["*"], "roles": ["*"]}]"#)
            .unwrap();
        assert!(!c.wants(Category::Dml, "ks", "t", "cassandra"));
        c.set_rules(r#"[{"sinks": ["table"], "categories": ["DCL"], "qualified_table_names": [], "roles": ["admin_*"]}]"#)
            .unwrap();
        assert!(c.wants(Category::Dcl, "", "", "admin_1"));
        assert!(!c.wants(Category::Dcl, "", "", "bob"));
        assert_eq!(
            c.rules_text(),
            r#"[{"sinks":["table"],"categories":["DCL"],"qualified_table_names":[],"roles":["admin_*"]}]"#
        );
        assert!(c.set_rules("[1]").is_err());
        assert!(c.set_rules("{").is_err());
        assert!(c.set_rules(r#"[{"bogus": []}]"#).is_err());
        assert!(c.set_rules(r#"[{"categories": ["NOPE"]}]"#).is_err());
        c.set_rules("[]").unwrap();
        assert_eq!(c.rules_text(), "[]");
        c.table_sink = false;
        c.set_categories("DML");
        assert!(!c.wants(Category::Dml, "", "", "u"));
    }

    #[test]
    fn plain_tokens() {
        assert_eq!(plain_user(b"\0cassandra\0pw").as_deref(), Some("cassandra"));
        assert_eq!(plain_user(b"").as_deref(), None);
    }
}
