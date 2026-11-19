//! Decoy keys (docs/secrets.md, "Decoys"): a key whose value is a credential made to be found, as
//! GitGuardian's honeytokens are. It's stored, read, synced and exported like any other key; only
//! the project's admins see that it's a decoy. Any use of it raises `decoy_used` on the project.
//!
//! An `rlt` decoy is a roll-access decoy token ([`crate::Workloads::mint_decoy`]): roll-access
//! answers its use as it answers an unknown token, tells its owner and the admins, and tells roll
//! secrets ([`Secrets::decoy_used`]). Changing or deleting the key ends the decoy: the write marks
//! it ended in its own transaction, and its token is revoked after the write
//! ([`Secrets::end_decoys`]; the sweeper retries what fails).

use crate::access::{Action, Caller, Scope};
use crate::caller::Origin;
use crate::envs;
use crate::write::{Item, ItemAction, SecretType, current};
use crate::{Error, Result, Secret, Secrets, Value, audit, names};
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

/// The audit actions only a project's admins see: they say which keys are decoys.
pub(crate) const ADMIN_ACTIONS: [&str; 2] = ["secrets.decoy", "secrets.alert.decoy_used"];

/// `POST …/environments/{env}/decoys`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDecoy {
    pub key: String,
    /// `rlt`: a roll-access token. (`postgres` and `aws` aren't here yet.)
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// That a key is a decoy, on [`Secret::decoy`] for the project's admins only.
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
    /// Whether `caller` sees which keys are decoys: the project's admins.
    pub(crate) fn sees_decoys(&self, project: &str, caller: &Caller) -> bool {
        caller.allows(Action::Admin, &Scope::whole(project))
    }

    /// Plant a decoy key: a new key in `env` whose value is a decoy credential. It's an ordinary
    /// change (as a `PUT` of the key makes), so the key reads like any other to everyone else. In
    /// a protected environment it lands without a change request: it changes no value anyone
    /// has, and only an admin, who could lift the protection, may plant one.
    pub fn add_decoy(&self, project: &str, env: &str, n: NewDecoy, caller: &Caller) -> Result<Secret> {
        caller.need(Action::Admin, &Scope::whole(project))?;
        let path = names::path(n.path.as_deref().unwrap_or("/"))?;
        names::key(&n.key)?;
        match n.kind.as_str() {
            "rlt" => {}
            "postgres" => return Err(Error::BadRequest("postgres decoys aren't here yet (roll Postgres doesn't report a role's logins): use rlt".into())),
            "aws" => return Err(Error::BadRequest("aws decoys aren't here: they need an AWS account and CloudTrail. Use rlt".into())),
            other => return Err(Error::BadRequest(format!("kind is rlt, not {other:?}"))),
        }
        let workloads = self.workloads.clone().ok_or_else(|| Error::BadRequest("decoys aren't on this server".into()))?;
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
        let name: String = format!("secrets decoy {} in {env}", n.key).chars().take(64).collect();
        let rule = crate::access::rule(project, Some(vec![env.to_string()]), Some(vec![path.clone()]), &[Action::Read, Action::Reveal]);
        let minted = workloads.mint_decoy(&name, rule, owner.as_deref())?;
        let item = Item { path: path.clone(), key: n.key.clone(), owner: String::new(), action: ItemAction::set_value(Value::new(minted.secret)) };
        let (key, kind, grant, by) = (n.key.clone(), n.kind.clone(), minted.id.clone(), caller.principal.clone());
        // `if_version` 0: only while the key doesn't exist. The decoy's row in the change's own
        // transaction: never a decoy key without its mark.
        let written = self.commit_then(project, env, vec![item], &[(0, 0)], "write", "", None, true, caller, &|c, e, now| {
            let id = current(c, &e.id, &path, &key, "")?.ok_or_else(|| Error::Internal(format!("{key} wasn't written")))?.id;
            c.execute(
                "INSERT INTO decoys (id, project_id, env_id, secret_id, path, key, kind, ref, created_by, created_at, uses) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                params![names::new_id("dcy", 16), project, e.id, id, path, key, kind, grant, by, now],
            )?;
            audit::record(c, self.keys()?, project, caller, "secrets.decoy", Some(&e.slug), Some(&path), std::slice::from_ref(&key), &format!("planted decoy {key} ({kind})"), now)
        });
        if let Err(e) = written {
            if let Err(r) = workloads.revoke_decoy(&minted.id) {
                tracing::warn!(grant = %minted.id, "revoking a decoy whose key wasn't written failed: {r}");
            }
            return Err(e);
        }
        self.secret(project, env, &path, &n.key, Some(SecretType::Shared), caller)
    }

    /// Roll-access says decoy token `grant` was used, from `ip`, calling `route`. If it's a decoy
    /// key's, the use is counted and, once an hour per decoy, `decoy_used` is raised: an audit row
    /// (the environment's activity, audit sinks) and a notice to the environment's notify URL.
    /// Names, the address and the route, never a value. Whether it was a decoy key's.
    pub fn decoy_used(&self, grant: &str, ip: Option<&str>, route: &str) -> Result<bool> {
        let now = self.db.now_ms()?;
        let ip: Option<String> = ip.map(|i| i.chars().filter(|c| !c.is_control()).take(64).collect());
        let route: String = route.chars().filter(|c| !c.is_control()).take(200).collect();
        let web = self.web_url.clone();
        self.db.write(|c| {
            type Row = (String, String, String, String, String, Option<i64>);
            let row: Option<Row> = c
                .query_row(
                    "SELECT d.id, d.project_id, d.env_id, d.path, d.key, d.alerted_at FROM decoys d JOIN projects p ON p.project_id = d.project_id WHERE d.kind = 'rlt' AND d.ref = ? AND p.forgotten_at IS NULL",
                    [grant],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            let Some((id, project, env_id, path, key, alerted)) = row else { return Ok(false) };
            c.execute("UPDATE decoys SET uses = uses + 1, used_at = ?, used_ip = ?, used_route = ? WHERE id = ?", params![now, ip, route, id])?;
            if alerted.is_some_and(|a| now - a < ALERT_EVERY_MS) {
                return Ok(true);
            }
            c.execute("UPDATE decoys SET alerted_at = ? WHERE id = ?", params![now, id])?;
            let env: Option<(String, bool)> = c.query_row("SELECT slug, notify_url IS NOT NULL FROM environments WHERE id = ?", [&env_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
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

    /// Revoke the tokens of decoys whose keys were changed or deleted (outside the write that
    /// ended them: roll-access is another store). Keys gone another way (their environment or
    /// folder deleted, the project forgotten) end here too. Returns how many were revoked.
    pub(crate) fn end_decoys(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        let due: Vec<(String, String)> = self.db.write(|c| {
            c.execute("UPDATE decoys SET ended_at = ? WHERE ended_at IS NULL AND NOT EXISTS (SELECT 1 FROM secrets s WHERE s.id = decoys.secret_id)", [now])?;
            // A forgotten project's, once revoked, go.
            c.execute("DELETE FROM decoys WHERE revoked_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = decoys.project_id AND p.forgotten_at IS NULL)", ())?;
            c.query("SELECT id, ref FROM decoys WHERE revoked_at IS NULL AND ended_at IS NOT NULL ORDER BY ended_at LIMIT 100", (), |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
        let Some(workloads) = self.workloads.clone() else { return Ok(0) };
        let mut revoked = 0;
        for (id, grant) in due {
            let res = workloads.revoke_decoy(&grant);
            let now = self.db.now_ms()?;
            match res {
                Ok(()) => {
                    revoked += 1;
                    self.db.write(|c| c.execute("UPDATE decoys SET revoked_at = ?, revoke_error = NULL WHERE id = ?", params![now, id]))?;
                }
                Err(e) => {
                    tracing::warn!(decoy = %id, grant = %grant, "revoking an ended decoy's token failed: {e}");
                    let why: String = e.to_string().chars().take(300).collect();
                    self.db.write(|c| c.execute("UPDATE decoys SET revoke_error = ? WHERE id = ?", params![why, id]))?;
                }
            }
        }
        Ok(revoked)
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
