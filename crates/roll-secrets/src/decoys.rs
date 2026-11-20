//! Decoy keys (docs/secrets.md, "Decoys"): a key whose value is a credential made to be found, as
//! GitGuardian's honeytokens are. It's stored, read, synced and exported like any other key; only
//! the project's admins, signed in or the operator, see that it's a decoy ([`Caller::sees_decoys`]):
//! a token's answers about it are a normal key's, admin or not. Any use of it raises `decoy_used` on the project.
//!
//! An `rlt` decoy is a roll-access decoy token ([`crate::Workloads::mint_decoy`]): roll-access
//! answers its use as it answers an unknown token, tells its owner and the admins, and tells roll
//! secrets ([`Secrets::decoy_used`]). A `postgres` decoy is a role of the database's
//! ([`crate::LinkProvider::plant_decoy`]): roll Postgres' proxy refuses its logins as Postgres
//! refuses a wrong password, without waking a compute, and tells roll secrets the same way.
//! Changing or deleting the key ends the decoy: the write marks it ended in its own transaction,
//! and its token or role goes after the write ([`Secrets::end_decoys`]; the sweeper retries what
//! fails).

use crate::access::{Action, Caller, Scope};
use crate::caller::Origin;
use crate::envs;
use crate::write::{Item, ItemAction, SecretType, current};
use crate::{Error, Planted, Result, Secret, Secrets, Value, audit, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS decoys (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    secret_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    uses INTEGER NOT NULL,
    used_at INTEGER,
    used_ip TEXT,
    used_route TEXT,
    alerted_at INTEGER,
    ended_at INTEGER,
    revoked_at INTEGER,
    revoke_error TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS decoys_ref ON decoys (kind, ref);
CREATE INDEX IF NOT EXISTS decoys_secret ON decoys (secret_id);
CREATE INDEX IF NOT EXISTS decoys_project ON decoys (project_id);
CREATE INDEX IF NOT EXISTS decoys_ended ON decoys (revoked_at, ended_at);
";

/// One alert an hour per decoy (the audit row and the notice); every use is counted.
const ALERT_EVERY_MS: i64 = 3_600_000;

/// The audit actions only [`Secrets::sees_decoys`] callers see (and only sinks they made get): they
/// say a decoy was used. `secrets.decoy` is a plant's row from before plants wrote none: a plant's
/// only row is its key's `secrets.write`, as any key's, so no gap in a token's view points at it.
pub(crate) const DECOY_ACTIONS: [&str; 2] = ["secrets.decoy", "secrets.alert.decoy_used"];

/// `POST …/environments/{env}/decoys`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDecoy {
    pub key: String,
    /// `rlt`: a roll-access token; `postgres`: a role of `resource`, a roll Postgres database.
    /// (`aws` isn't here.)
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    /// The database a `postgres` decoy's role is made in (on the project's canvas).
    #[serde(default)]
    pub resource: Option<String>,
}

/// That a key is a decoy, on [`Secret::decoy`] for [`Secrets::sees_decoys`] callers only.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Decoy {
    pub kind: String,
    pub created_at: i64,
    pub created_by: String,
    /// Times it was used (each refused).
    pub uses: i64,
    pub used_at: Option<i64>,
    pub used_ip: Option<String>,
}

/// A live decoy that was used, for health.
pub(crate) struct UsedDecoy {
    pub env_id: String,
    pub path: String,
    pub key: String,
    pub uses: i64,
    pub used_at: i64,
    pub used_ip: Option<String>,
}

impl Secrets {
    /// Whether `caller` sees which keys are decoys: the project's admins, signed in or the operator
    /// ([`Caller::sees_decoys`]), never a token.
    pub(crate) fn sees_decoys(&self, project: &str, caller: &Caller) -> bool {
        caller.sees_decoys() && caller.allows(Action::Admin, &Scope::whole(project))
    }

    /// Plant a decoy key: a new key in `env` whose value is a decoy credential. It's an ordinary
    /// change (as a `PUT` of the key makes), so the key reads like any other to everyone else. In
    /// a protected environment it lands without a change request: it changes no value anyone
    /// has, and only an admin, who could lift the protection, may plant one. Signed in or the
    /// operator: a token that planted one could tell it from the rest by elimination.
    pub fn add_decoy(&self, project: &str, env: &str, n: NewDecoy, caller: &Caller) -> Result<Secret> {
        caller.need(Action::Admin, &Scope::whole(project))?;
        if !caller.sees_decoys() {
            return Err(Error::Forbidden("decoys are planted from a signed-in session (the dashboard) or with the operator token, never a roll token".into()));
        }
        let path = names::path(n.path.as_deref().unwrap_or("/"))?;
        names::key(&n.key)?;
        match (n.kind.as_str(), &n.resource) {
            ("rlt", None) | ("postgres", Some(_)) => {}
            ("rlt", Some(_)) => return Err(Error::BadRequest("an rlt decoy has no resource".into())),
            ("postgres", None) => return Err(Error::BadRequest("a postgres decoy names its database: resource".into())),
            ("aws", _) => return Err(Error::BadRequest("aws decoys aren't here: they need an AWS account and CloudTrail. Use rlt or postgres".into())),
            (other, _) => return Err(Error::BadRequest(format!("kind is rlt or postgres, not {other:?}"))),
        }
        self.project(project)?;
        let exists = self.db.read(|c| {
            let e = envs::by_slug(c, project, env)?;
            Ok::<_, Error>(current(c, &e.id, &path, &n.key, "")?.is_some())
        })?;
        if exists {
            return Err(Error::Conflict("exists", format!("{} is already in {env}:{path}: a decoy is a new key", n.key)));
        }
        // Whose it is: they're mailed when it's used, as the admins are.
        let owner = match self.maker(&caller.principal)? {
            Origin::Person(u) => Some(u),
            Origin::Operator => None,
            Origin::Gone => return Err(Error::Forbidden("whoever made this token can't make decoys any more".into())),
        };
        let planted = match &n.resource {
            None => {
                let workloads = self.workloads.clone().ok_or_else(|| Error::BadRequest("decoys aren't on this server".into()))?;
                let name: String = format!("secrets decoy {} in {env}", n.key).chars().take(64).collect();
                let rule = crate::access::rule(project, Some(vec![env.to_string()]), Some(vec![path.clone()]), &[Action::Read, Action::Reveal]);
                let minted = workloads.mint_decoy(&name, rule, owner.as_deref())?;
                Planted { value: Value::new(minted.secret), handle: minted.id }
            }
            // A database the project may reach, as a link's.
            Some(db) => {
                if !self.members(project)?.contains(&(n.kind.clone(), db.clone())) {
                    return Err(Error::BadRequest(self.not_linkable(project, &n.kind, db)));
                }
                self.links.get(&n.kind)?.plant_decoy(db)?
            }
        };
        let item = Item { path: path.clone(), key: n.key.clone(), owner: String::new(), action: ItemAction::set_value(planted.value) };
        let (key, kind, handle, by) = (n.key.clone(), n.kind.clone(), planted.handle, caller.made_by().to_string());
        // `if_version` 0: only while the key doesn't exist. The decoy's row in the change's own
        // transaction: never a decoy key without its mark. It's also the plant's only record past
        // the key's ordinary `secrets.write` (who and when, on [`Decoy`]): an audit row of its own
        // would leave a gap right after that write in a token's view.
        let written = self.commit_then(project, env, vec![item], &[(0, 0)], "write", "", None, true, caller, &|c, e, _, now| {
            let id = current(c, &e.id, &path, &key, "")?.ok_or_else(|| Error::Internal(format!("{key} wasn't written")))?.id;
            c.execute(
                "INSERT INTO decoys (id, project_id, env_id, secret_id, path, key, kind, ref, created_by, created_at, uses) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                params![names::new_id("dcy", 16), project, e.id, id, path, key, kind, handle, by, now],
            )?;
            Ok(())
        });
        if let Err(e) = written {
            if let Err(r) = self.remove_decoy(&n.kind, &handle) {
                tracing::warn!(kind = %n.kind, "removing a decoy whose key wasn't written failed: {r}");
            }
            return Err(e);
        }
        self.secret(project, env, &path, &n.key, Some(SecretType::Shared), caller)
    }

    /// A decoy credential of `kind` was used, from `ip`, calling `route`: roll-access's decoy token
    /// `handle` (`rlt`), or the role a product's [`crate::LinkProvider::plant_decoy`] named
    /// `handle`. If it's a decoy key's, the use is counted and, once an hour per decoy,
    /// `decoy_used` is raised: an audit row (the environment's activity, audit sinks) and a notice
    /// to the environment's notify URL. Names, the address and the route, never a value. Whether
    /// it was a decoy key's.
    pub fn decoy_used(&self, kind: &str, handle: &str, ip: Option<&str>, route: &str) -> Result<bool> {
        let now = self.db.now_ms()?;
        let ip: Option<String> = ip.map(|i| i.chars().filter(|c| !c.is_control()).take(64).collect());
        let route: String = route.chars().filter(|c| !c.is_control()).take(200).collect();
        let web = self.web_url.clone();
        self.db.write(|c| {
            type Row = (String, String, String, String, String, Option<i64>);
            let row: Option<Row> = c
                .query_row(
                    "SELECT d.id, d.project_id, d.env_id, d.path, d.key, d.alerted_at FROM decoys d JOIN projects p ON p.project_id = d.project_id WHERE d.kind = ? AND d.ref = ? AND p.forgotten_at IS NULL",
                    [kind, handle],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            let Some((id, project, env_id, path, key, alerted)) = row else { return Ok(false) };
            c.execute("UPDATE decoys SET uses = uses + 1, used_at = ?, used_ip = ?, used_route = ? WHERE id = ?", params![now, ip, route, id])?;
            if alerted.is_some_and(|a| now - a < ALERT_EVERY_MS) {
                return Ok(true);
            }
            c.execute("UPDATE decoys SET alerted_at = ? WHERE id = ?", params![now, id])?;
            // Told only to a notify URL a session or the operator set (`notices::tells_decoy`).
            let env: Option<(String, bool)> =
                c.query_row("SELECT slug, notify_url IS NOT NULL AND COALESCE(notify_set_by_session, 0) = 1 FROM environments WHERE id = ?", [&env_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (slug, notify) = env.unwrap_or_else(|| ("(deleted)".into(), false));
            let from = ip.as_deref().map(|i| format!(" from {i}")).unwrap_or_default();
            let text = format!("decoy {key} in {slug}:{path} was used{from}, calling {route}");
            let mut who = Caller::consumer("roll:decoys");
            who.ip = ip.clone();
            audit::record(c, self.keys()?, &project, &who, "secrets.alert.decoy_used", Some(&slug), Some(&path), std::slice::from_ref(&key), &text, now)?;
            if notify {
                let link = web.as_ref().map(|w| format!("{}/dashboard/projects/{project}/secrets?env={slug}&tab=activity", w.trim_end_matches('/')));
                let mut said = format!("roll secrets: {text}. Whoever used it found it where it was planted: cut them off and rotate what sits beside it.");
                if let Some(l) = &link {
                    said.push_str(&format!(" {l}"));
                }
                let alert = json!({ "kind": "decoy_used", "key": key, "path": path, "ip": ip, "route": route });
                let body = json!({ "text": said, "content": said, "allowed_mentions": { "parse": [] }, "project": project, "environment": slug, "alert": alert, "at": now, "link": link });
                c.execute(
                    "INSERT INTO notices (id, project_id, env_id, body, attempts, next_at, sent_at, created_at) VALUES (?, ?, ?, ?, 0, ?, NULL, ?)",
                    params![names::new_id("ntc", 16), project, env_id, body.to_string(), now, now],
                )?;
            }
            Ok::<_, Error>(true)
        })
    }

    /// Revoke the tokens, or drop the roles, of decoys whose keys were changed or deleted
    /// (outside the write that ended them: they're another store's). Keys gone another way (their
    /// environment or folder deleted, the project forgotten) end here too. Returns how many went.
    pub(crate) fn end_decoys(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        let due: Vec<(String, String, String)> = self.db.write(|c| {
            c.execute("UPDATE decoys SET ended_at = ? WHERE ended_at IS NULL AND NOT EXISTS (SELECT 1 FROM secrets s WHERE s.id = decoys.secret_id)", [now])?;
            // A forgotten project's, once revoked, go.
            c.execute("DELETE FROM decoys WHERE revoked_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = decoys.project_id AND p.forgotten_at IS NULL)", ())?;
            c.query("SELECT id, kind, ref FROM decoys WHERE revoked_at IS NULL AND ended_at IS NOT NULL ORDER BY ended_at LIMIT 100", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        })?;
        let mut revoked = 0;
        for (id, kind, handle) in due {
            let res = self.remove_decoy(&kind, &handle);
            let now = self.db.now_ms()?;
            match res {
                Ok(()) => {
                    revoked += 1;
                    self.db.write(|c| c.execute("UPDATE decoys SET revoked_at = ?, revoke_error = NULL WHERE id = ?", params![now, id]))?;
                }
                Err(e) => {
                    tracing::warn!(decoy = %id, %kind, handle = %handle, "removing an ended decoy failed: {e}");
                    let why: String = e.to_string().chars().take(300).collect();
                    self.db.write(|c| c.execute("UPDATE decoys SET revoke_error = ? WHERE id = ?", params![why, id]))?;
                }
            }
        }
        Ok(revoked)
    }

    /// End one decoy at its product: roll-access's token, or what a link provider planted.
    fn remove_decoy(&self, kind: &str, handle: &str) -> Result<()> {
        match kind {
            "rlt" => match &self.workloads {
                Some(w) => w.revoke_decoy(handle),
                None => Err(Error::Unavailable("decoys aren't on this server".into())),
            },
            _ => self.links.get(kind)?.remove_decoy(handle),
        }
    }

    /// The project's live decoys by secret id, for its admins' listings.
    pub(crate) fn decoys_of(&self, project: &str) -> Result<HashMap<String, Decoy>> {
        Ok(self
            .db
            .read(|c| {
                c.query("SELECT secret_id, kind, created_at, created_by, uses, used_at, used_ip FROM decoys WHERE project_id = ? AND ended_at IS NULL", [project], |r| {
                    Ok((r.get::<_, String>(0)?, Decoy { kind: r.get(1)?, created_at: r.get(2)?, created_by: r.get(3)?, uses: r.get(4)?, used_at: r.get(5)?, used_ip: r.get(6)? }))
                })
            })?
            .into_iter()
            .collect())
    }

    /// Where the project's live decoys are: (env id, path, key).
    pub(crate) fn live_decoys(&self, project: &str) -> Result<std::collections::HashSet<(String, String, String)>> {
        Ok(self.db.read(|c| c.query("SELECT env_id, path, key FROM decoys WHERE project_id = ? AND ended_at IS NULL", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.into_iter().collect())
    }

    /// Live decoys that were used, for health.
    pub(crate) fn used_decoys(&self, project: &str) -> Result<Vec<UsedDecoy>> {
        Ok(self.db.read(|c| {
            c.query(
                "SELECT env_id, path, key, uses, used_at, used_ip FROM decoys WHERE project_id = ? AND ended_at IS NULL AND used_at IS NOT NULL",
                [project],
                |r| Ok(UsedDecoy { env_id: r.get(0)?, path: r.get(1)?, key: r.get(2)?, uses: r.get(3)?, used_at: r.get(4)?, used_ip: r.get(5)? }),
            )
        })?)
    }
}
