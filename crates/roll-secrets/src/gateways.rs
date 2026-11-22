//! Gateways into private networks (docs/secrets.md, "Gateways"): a project's record of a process
//! the team runs inside their network, which dials out to rolld (`roll gateway up`) and relays
//! roll's conversations with private databases and directories. rolld holds the link
//! (crates/rolld/src/secrets/gateway.rs); this keeps the records, their status as the links
//! report it, and who may open a gateway's link.
//!
//! A gateway's credential is roll-access's: a roll token with one rule naming the gateway
//! (`Workloads::token`, made as `POST /v1/access/tokens` makes one), or a workload identity made
//! for the gateway, whose logins mint that same rule. Roll secrets keeps no token of its own.

use crate::access::{Action, Caller, Everything, Scope};
use crate::{Error, Result, Secrets, audit, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS gateways (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    token_id TEXT,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    conn TEXT,
    node TEXT,
    version TEXT,
    allow TEXT,
    connected_at INTEGER,
    last_seen_at INTEGER,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

/// A link is online while it reports at least this often (it reports every 30 s).
const ONLINE_MS: i64 = 90_000;
const GATEWAYS_MAX: i64 = 20;
/// Allow-list entries a status keeps (the gateway's own limit).
const ALLOW_MAX: usize = 256;

/// The one rule a gateway's credential carries: write on the gateway, which only its link's
/// route names.
pub fn rule(gateway: &str) -> roll_auth::scope::Rule {
    use roll_auth::scope;
    scope::Rule {
        product: "secrets".into(),
        resources: scope::Resources::Some(vec![scope::Selector::id(gateway)]),
        environments: None,
        paths: None,
        unprotected: false,
        actions: std::collections::BTreeMap::from([(scope::Action::Write, scope::Effect::Allow)]),
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Gateway {
    pub id: String,
    pub name: String,
    /// The roll token made with it (its id), if one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    pub created_at: i64,
    pub created_by: String,
    pub status: Status,
}

/// What the gateway's link last said, from whichever node holds it.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Status {
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The allow-list as the gateway reported it (it enforces it; roll never edits it).
    pub allow: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewGateway {
    pub name: String,
    /// Make its roll token (the default); `false` for a gateway that logs in through a workload
    /// identity, or whose token is made later.
    #[serde(default)]
    pub token: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct MadeGateway {
    pub gateway: Gateway,
    /// The token's secret, shown once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Gateways' links, where they're held (rolld): re-check gateway `id`'s link now, closing it if
/// its credential or its record is gone. Blocking; a holder that can't be reached isn't an error
/// (the link checks before every stream it opens).
pub trait Links: Send + Sync {
    fn recheck(&self, id: &str);
}

/// Who a link belongs to, once its caller may open it.
#[derive(Debug, Clone)]
pub struct LinkOf {
    pub project: String,
    pub name: String,
}

/// What a link reports when it's up.
pub struct Hello<'a> {
    pub conn: &'a str,
    pub node: &'a str,
    pub version: &'a str,
    pub allow: &'a [String],
    /// The grant that opened it, for the audit log.
    pub grant: Option<&'a str>,
}

const COLUMNS: &str = "id, name, token_id, created_at, created_by, node, version, allow, connected_at, last_seen_at, conn";

fn row(now: i64) -> impl Fn(&code_sql::Row<'_>) -> code_sql::Result<Gateway> {
    move |r| {
        let last_seen_at: Option<i64> = r.get(9)?;
        let conn: Option<String> = r.get(10)?;
        let connected = conn.is_some();
        Ok(Gateway {
            id: r.get(0)?,
            name: r.get(1)?,
            token_id: r.get(2)?,
            created_at: r.get(3)?,
            created_by: r.get(4)?,
            status: Status {
                online: connected && last_seen_at.is_some_and(|t| now - t < ONLINE_MS),
                node: r.get(5)?,
                version: r.get(6)?,
                allow: r.get::<_, Option<String>>(7)?.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default(),
                connected_at: r.get(8)?,
                last_seen_at,
            },
        })
    }
}

/// The audit log's name for a gateway's link.
fn link_caller(id: &str) -> Caller {
    Caller::acting(format!("gateway:{id}"), Arc::new(Everything))
}

impl Secrets {
    pub fn gateways(&self, project: &str, caller: &Caller) -> Result<Vec<Gateway>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM gateways WHERE project_id = ? ORDER BY name"), [project], row(now))?))
    }

    /// Make a gateway, and (unless `token: false`) its roll token, made for the caller's request
    /// (`ext`) as `POST /v1/access/tokens` would: it's theirs, listed and revoked with their tokens.
    pub fn add_gateway(&self, project: &str, n: NewGateway, caller: &Caller, ext: &axum::http::Extensions) -> Result<MadeGateway> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a gateway's name is letters, digits, - and _ (64 at most)".into()));
        }
        let token = n.token.unwrap_or(true);
        let workloads = self.workloads.clone();
        if token {
            if workloads.is_none() {
                return Err(Error::BadRequest("roll tokens aren't on this server: make the gateway with \"token\": false".into()));
            }
            // A project-wide rule doesn't cover a gateway's id: roll makes its token for people and
            // the operator, as it makes identities.
            if caller.principal.starts_with("token:") || caller.principal.starts_with("identity:") {
                return Err(Error::Forbidden("a token doesn't make a gateway's token: sign in, or make it with \"token\": false and log the gateway in through an identity".into()));
            }
        }
        self.project(project)?;
        let id = names::new_id("gw", 16);
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM gateways WHERE project_id = ? AND name = ?", [project, &n.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a gateway named {} exists", n.name)));
            }
            let count: i64 = c.query_row("SELECT COUNT(*) FROM gateways WHERE project_id = ?", [project], |r| r.get(0))?;
            if count >= GATEWAYS_MAX {
                return Err(Error::TooMany(format!("a project has at most {GATEWAYS_MAX} gateways")));
            }
            c.execute("INSERT INTO gateways (id, project_id, name, created_at, created_by) VALUES (?, ?, ?, ?, ?)", params![id, project, n.name, now, caller.principal])?;
            audit::record(c, self.keys()?, project, caller, "gateway.add", None, None, &[], &format!("added gateway {} ({id})", n.name), now)
        })?;
        // Issued outside the transaction; a failure takes the record back out.
        let mut secret = None;
        if let (true, Some(w)) = (token, workloads) {
            // The operator's token is the operator's, as `POST /v1/access/tokens` makes it: on a
            // server without sign-in the gate marks no one, and roll-access makes a token only for
            // a request that names its caller.
            let mut ext = ext.clone();
            if caller.principal == "operator" {
                ext.insert(roll_auth::Operator);
            }
            match w.token(&ext, &format!("gateway {}", n.name), rule(&id)) {
                Ok(m) => {
                    self.db.write(|c| c.execute("UPDATE gateways SET token_id = ? WHERE id = ?", params![m.id, id]))?;
                    secret = Some(m.secret);
                }
                Err(e) => {
                    let _ = self.db.write(|c| c.execute("DELETE FROM gateways WHERE id = ?", [&id]));
                    return Err(e);
                }
            }
        }
        let gateway = self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM gateways WHERE id = ?"), [&id], row(now)))?;
        Ok(MadeGateway { gateway, token: secret })
    }

    /// Delete a gateway no database or directory goes through; its token is revoked and its link
    /// closes at once.
    pub fn delete_gateway(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let token: Option<String> = self.db.write(|c| {
            let (name, token): (String, Option<String>) =
                c.query_row("SELECT name, token_id FROM gateways WHERE id = ? AND project_id = ?", [id, project], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| Error::NotFound(format!("no gateway {id}")))?;
            let users: Vec<String> = c.query(
                "SELECT name FROM outside_dbs WHERE gateway = ? UNION ALL SELECT name FROM directories WHERE gateway = ? UNION ALL SELECT name FROM identities WHERE gateway = ? LIMIT 5",
                [id, id, id],
                |r| r.get(0),
            )?;
            if !users.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} is used by {}: delete those first", users.join(", "))));
            }
            c.execute("DELETE FROM gateways WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "gateway.delete", None, None, &[], &format!("deleted gateway {name} ({id})"), now)?;
            Ok::<_, Error>(token)
        })?;
        if let (Some(t), Some(w)) = (token, &self.workloads)
            && let Err(e) = w.revoke_token(&t, "its gateway was deleted")
        {
            tracing::warn!(gateway = id, "revoking a deleted gateway's token failed: {e}");
        }
        self.recheck_gateway(id);
        Ok(())
    }

    /// Close gateway `id`'s link now if what opened it is gone (outside any transaction).
    pub(crate) fn recheck_gateway(&self, id: &str) {
        if let Some(l) = &self.gateway_links {
            l.recheck(id);
        }
    }

    /// The gateway a database or directory is registered through: one of the project's.
    pub(crate) fn check_gateway(&self, project: &str, gateway: Option<String>) -> Result<Option<String>> {
        let Some(g) = gateway.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()) else { return Ok(None) };
        let found = self.db.read(|c| c.query_row("SELECT 1 FROM gateways WHERE id = ? AND project_id = ?", [&g, project], |_| Ok(())).optional())?;
        found.map(|_| Some(g.clone())).ok_or_else(|| Error::BadRequest(format!("{g} isn't one of this project's gateways")))
    }

    /// Whether this request may open gateway `id`'s link, looked up now (never cached): a grant
    /// whose rules name the gateway (its token, an identity's login), the operator, or a person
    /// who administers its project. Whoever made it must still reach the project.
    pub fn gateway_link(&self, id: &str, operator: bool, user: Option<&roll_auth::SignedIn>, grant: Option<&roll_auth::Grant>) -> Result<LinkOf> {
        let denied = || Error::Forbidden(format!("this credential doesn't open gateway {id}"));
        // Who's asking first (no credential: 401), and a grant that doesn't name the gateway learns
        // nothing about it, not even whether it exists.
        let caller = match grant {
            Some(g) => {
                use roll_auth::scope;
                let r = scope::Resource { product: "secrets".into(), id: Some(id.into()), ..Default::default() };
                if g.allows(&r, scope::Action::Write) != scope::Outcome::Allow {
                    return Err(denied());
                }
                None
            }
            None => Some(self.caller_of(operator, user, None)?),
        };
        let found: Option<(String, String, String)> = self.db.read(|c| c.query_row("SELECT project_id, name, created_by FROM gateways WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        let (project, name, created_by) = found.ok_or_else(|| Error::NotFound(format!("no gateway {id}: it was deleted")))?;
        // A deleted project's gateways open nothing, in its window or after (deleted.rs).
        let live: i64 = self.db.read(|c| c.query_row("SELECT COUNT(*) FROM projects WHERE project_id = ? AND forgotten_at IS NULL", [&project], |r| r.get(0)))?;
        if live == 0 {
            return Err(Error::NotFound(format!("no gateway {id}: its project was deleted")));
        }
        if let Some(caller) = caller {
            caller.need(Action::Admin, &Scope::project(&project)).map_err(|_| denied())?;
        }
        // A gateway is the project admin's: it works while its maker still is one.
        if !self.maker_reaches(&created_by, &[(Action::Admin, Scope::whole(&project))])? {
            return Err(denied());
        }
        Ok(LinkOf { project, name })
    }

    /// A link is up: its status, and the audit row.
    pub fn gateway_connected(&self, id: &str, h: &Hello<'_>) -> Result<()> {
        let now = self.db.now_ms()?;
        let allow: Vec<&String> = h.allow.iter().take(ALLOW_MAX).collect();
        self.db.write(|c| {
            let Some(project) = c.query_row("SELECT project_id FROM gateways WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()? else { return Err(Error::NotFound(format!("no gateway {id}"))) };
            c.execute(
                "UPDATE gateways SET conn = ?, node = ?, version = ?, allow = ?, connected_at = ?, last_seen_at = ? WHERE id = ?",
                params![h.conn, h.node, h.version.chars().take(64).collect::<String>(), crate::json(&allow), now, now, id],
            )?;
            let by = h.grant.map(|g| format!(", token {g}")).unwrap_or_default();
            audit::record(c, self.keys()?, &project, &link_caller(id), "gateway.connect", None, None, &[], &format!("connected to node {} (roll {}{by}), allowing {}", h.node, h.version, allow.len()), now)
        })
    }

    /// A link is still up (every 30 s). False when the gateway is gone, or another link took over.
    pub fn gateway_seen(&self, id: &str, conn: &str) -> Result<bool> {
        let now = self.db.now_ms()?;
        Ok(self.db.write(|c| c.execute("UPDATE gateways SET last_seen_at = ? WHERE id = ? AND conn = ?", params![now, id, conn]))? > 0)
    }

    /// A link ended; the status changes only if it's still the gateway's newest link.
    pub fn gateway_disconnected(&self, id: &str, conn: &str, why: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let Some(project) = c.query_row("SELECT project_id FROM gateways WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()? else { return Ok(()) };
            c.execute("UPDATE gateways SET conn = NULL WHERE id = ? AND conn = ?", params![id, conn])?;
            // A deleted project's log closed meanwhile: nothing more goes in it.
            match audit::record(c, self.keys()?, &project, &link_caller(id), "gateway.disconnect", None, None, &[], why, now) {
                Err(Error::Gone(_)) => Ok(()),
                r => r,
            }
        })
    }

    /// One stream the gateway opened (or refused): its target and bytes, never what they held.
    pub fn gateway_stream(&self, id: &str, target: &str, outcome: &str) {
        let project = self.db.read(|c| c.query_row("SELECT project_id FROM gateways WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()).ok().flatten();
        if let Some(project) = project {
            self.audit_now(&project, &link_caller(id), "gateway.stream", None, None, &[], &format!("{target}: {outcome}"));
        }
    }

    /// A gateway's project (for the identity that logs it in).
    pub(crate) fn gateway_project(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT project_id FROM gateways WHERE id = ?", [id], |r| r.get(0)).optional())?)
    }

    /// Seal a provider call one node forwards to the gateway's holder (it carries an admin URL
    /// and passwords): the deployment key every node shares, bound to the gateway.
    pub fn seal_hop(&self, gateway: &str, msg: &[u8]) -> Result<Vec<u8>> {
        Ok(self.keys()?.seal_hop(&format!("gateway:{gateway}"), msg))
    }

    pub fn open_hop(&self, gateway: &str, raw: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        self.keys()?.open_hop(&format!("gateway:{gateway}"), raw).map_err(|_| Error::BadRequest("a forwarded call that doesn't open".into()))
    }
}
