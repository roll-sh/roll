//! Machine clients: OAuth client credentials (`io.modelcontextprotocol/oauth-client-credentials`;
//! docs/permissions.md, "Machine clients"). Automation with no person (CI, a scheduled agent)
//! trades its credential at the token endpoint for an hour's access token. A machine client is a
//! confidential `oauth_clients` row, plus one OAuth grant holding the rules the operator gave it.
//! Each token it gets is one more `access` secret on that grant, so revoking the client (its
//! grant) ends them all. Grants are looked up per request, as always.
//!
//! Its credential is a public key (`private_key_jwt`, RFC 7523: nothing secret is stored) or a
//! secret roll makes (`client_secret_basic`/`_post`: shown once, stored as its hash).

use super::*;

/// What a machine client proves itself with.
pub enum ClientKey {
    /// A secret roll makes: shown once, stored as its hash.
    Secret,
    /// A public key (a JWK) its JWT assertions are signed with.
    Jwk(String),
}

/// A machine client as the token endpoint checks it: its credential, and the one it had before a
/// rotation while that one still works.
pub struct MachineClient {
    pub grant_id: String,
    /// Its credential's rotation count: [`Store::issue_client_access`] issues only if it's still
    /// this (a token checked against a credential rotated away meanwhile gets nothing).
    pub generation: i64,
    pub secret_hash: Option<String>,
    pub jwk: Option<String>,
    pub old_secret_hash: Option<String>,
    pub old_jwk: Option<String>,
}

/// A live machine client and its own grant, by `client_id` (`?`) at `now` (`?`, twice): not
/// revoked, not expired, not waiting for an admin, started.
const LIVE: &str = "FROM oauth_clients o JOIN grants g ON g.id = o.grant_id \
     WHERE o.id = ? AND o.confidential = 1 AND g.revoked_at IS NULL AND (g.expires_at IS NULL OR g.expires_at > ?) \
     AND (g.pending IS NULL OR g.pending = 0) AND (g.not_before IS NULL OR g.not_before <= ?)";

/// A machine client's secret prefix (with a checksum, as every roll secret has).
pub const CLIENT_SECRET_PREFIX: &str = "rlc_";

impl Store {
    /// A new machine client: its confidential client row and its grant (`new`'s rules). Answers
    /// the client id, the grant and, for a secret, the secret (once).
    pub fn create_machine_client(&self, new: &NewGrant, key: ClientKey, by: Issuer<'_>) -> Result<(String, Grant, Option<String>)> {
        check_oauth_grant(new)?;
        let client_id = new_id("mcl");
        let grant_id = new_id("tok");
        let secret = matches!(key, ClientKey::Secret).then(|| crate::new_secret(CLIENT_SECRET_PREFIX));
        let (secret_hash, jwk) = match &key {
            ClientKey::Secret => (secret.as_deref().map(hash), None),
            ClientKey::Jwk(j) => (None, Some(j.clone())),
        };
        let client = Client { id: client_id.clone(), name: new.name.clone(), uri: None, redirect_uris: Vec::new(), created_at: 0, last_used_at: None };
        let now = code_sql::now_ms();
        self.db.write(|c| {
            insert_oauth_grant(c, &grant_id, &client, new, by, now)?;
            c.execute("UPDATE oauth_clients SET confidential = 1, grant_id = ?, secret_hash = ?, jwk = ? WHERE id = ?", params![grant_id, secret_hash, jwk, client_id])?;
            Ok::<_, Error>(())
        })?;
        Ok((client_id, self.get(&grant_id)?, secret))
    }

    /// A live machine client's credential and grant (`None`: no such client, an ordinary one, or
    /// one whose grant is revoked, expired, or waiting for an admin's approval).
    pub fn machine_client(&self, client_id: &str) -> Result<Option<MachineClient>> {
        let now = code_sql::now_ms();
        Ok(self.db.read(|c| {
            c.query_row(
                &format!("SELECT g.id, o.generation, o.secret_hash, o.jwk, CASE WHEN o.old_until > ? THEN o.old_secret_hash END, CASE WHEN o.old_until > ? THEN o.old_jwk END {LIVE}"),
                params![now, now, client_id, now, now],
                |r| Ok(MachineClient { grant_id: r.get(0)?, generation: r.get(1)?, secret_hash: r.get(2)?, jwk: r.get(3)?, old_secret_hash: r.get(4)?, old_jwk: r.get(5)? }),
            )
            .optional()
        })?)
    }

    /// A new credential for a machine client, keeping its id and rules: a new secret (answered,
    /// once) or a new key. The old one stops now, along with the tokens it got, or keeps working
    /// for `keep_old_ms` (at most a week) while its automation moves over.
    pub fn rotate_machine_client(&self, client_id: &str, key: ClientKey, keep_old_ms: Option<i64>) -> Result<Option<String>> {
        let secret = matches!(key, ClientKey::Secret).then(|| crate::new_secret(CLIENT_SECRET_PREFIX));
        let (secret_hash, jwk) = match &key {
            ClientKey::Secret => (secret.as_deref().map(hash), None),
            ClientKey::Jwk(j) => (None, Some(j.clone())),
        };
        let now = code_sql::now_ms();
        let keep = keep_old_ms.filter(|ms| *ms > 0).map(|ms| ms.min(MAX_KEEP_OLD_MS));
        self.db.write(|c| {
            let grant: Option<String> = c.query_row("SELECT grant_id FROM oauth_clients WHERE id = ? AND confidential = 1 AND grant_id IS NOT NULL", [client_id], |r| r.get(0)).optional()?;
            let Some(grant) = grant else { return Err(Error::NotFound(format!("machine client {client_id}"))) };
            match keep {
                Some(ms) => c.execute(
                    "UPDATE oauth_clients SET old_secret_hash = secret_hash, old_jwk = jwk, old_until = ?, secret_hash = ?, jwk = ?, generation = generation + 1 WHERE id = ?",
                    params![now + ms, secret_hash, jwk, client_id],
                )?,
                None => {
                    // A credential rotated because it may be out: what it got stops too.
                    c.execute("DELETE FROM grant_secrets WHERE grant_id = ? AND kind = 'access'", [&grant])?;
                    c.execute(
                        "UPDATE oauth_clients SET old_secret_hash = NULL, old_jwk = NULL, old_until = NULL, secret_hash = ?, jwk = ?, generation = generation + 1 WHERE id = ?",
                        params![secret_hash, jwk, client_id],
                    )?
                }
            };
            Ok::<_, Error>(())
        })?;
        Ok(secret)
    }

    /// An hour's access token on a machine client's grant, if the client is still live and its
    /// credential still the one checked (`generation`): checked and issued in one write, so a
    /// rotation or a revocation while a sign-in was being checked leaves it with nothing.
    pub fn issue_client_access(&self, grant_id: &str, client_id: &str, generation: i64) -> Result<(Grant, String)> {
        let access = new_secret(ACCESS_PREFIX);
        let now = code_sql::now_ms();
        self.db.write(|c| {
            let current: Option<(String, i64)> = c.query_row(&format!("SELECT g.id, o.generation {LIVE}"), params![client_id, now, now], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if current.as_ref() != Some(&(grant_id.to_string(), generation)) {
                return Err(Error::OAuth("invalid_client", "the client's credentials didn't check out".into()));
            }
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ? AND kind = 'access' AND expires_at < ?", params![grant_id, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'access', ?, ?)", params![hash(&access), grant_id, now + ACCESS_TTL_MS, now])?;
            c.execute("UPDATE oauth_clients SET last_used_at = ? WHERE id = ?", params![now, client_id])?;
            Ok::<_, Error>(())
        })?;
        Ok((self.get(grant_id)?, access))
    }

    /// Whether a JWT assertion's id is new for this client: false for a replay. Kept until the
    /// assertion would have expired anyway.
    pub fn first_jti(&self, client_id: &str, jti: &str, expires_at: i64) -> Result<bool> {
        let now = code_sql::now_ms();
        let n = self.db.write(|c| {
            c.execute("DELETE FROM client_jtis WHERE expires_at < ?", [now])?;
            c.execute("INSERT INTO client_jtis (client_id, jti, expires_at) VALUES (?, ?, ?) ON CONFLICT (client_id, jti) DO NOTHING", params![client_id, jti, expires_at])
        })?;
        Ok(n == 1)
    }

    /// The machine clients, newest first: id, its grant's id, whether it signs with a key.
    pub fn machine_clients(&self) -> Result<Vec<(String, String, bool)>> {
        Ok(self.db.read(|c| {
            c.query(
                "SELECT o.id, g.id, o.jwk IS NOT NULL FROM oauth_clients o JOIN grants g ON g.id = o.grant_id WHERE o.confidential = 1 ORDER BY o.created_at DESC",
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
            )
        })?)
    }
}
