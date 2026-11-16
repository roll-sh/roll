//! Machine clients: OAuth client credentials (`io.modelcontextprotocol/oauth-client-credentials`;
//! docs/permissions.md, "Machine clients"). Automation with no person (CI, a scheduled agent)
//! trades its credential at the token endpoint for an hour's access token. A machine client is a
//! confidential `oauth_clients` row, plus one OAuth grant holding the rules the operator gave it.
//! Each token it gets is one more `access` secret on that grant, so revoking the client (its
//! grant) ends them all. Grants are looked up per request, as always.
//!
//! Its credential is a public key (`private_key_jwt`, RFC 7523: nothing secret is stored), a
//! secret roll makes (`client_secret_basic`/`_post`: shown once, stored as its hash), or a trusted
//! identity: CI's own OIDC token from an issuer, subject and claims (RFC 7523's `jwt-bearer`
//! grant; nothing is stored or held anywhere, as trusted publishing works).

use super::*;

/// What a machine client proves itself with.
pub enum ClientKey {
    /// A secret roll makes: shown once, stored as its hash.
    Secret,
    /// A public key (a JWK) its JWT assertions are signed with.
    Jwk(String),
    /// A trusted identity ([`TrustedIdentity`] as JSON): CI's OIDC tokens that match it.
    Oidc(String),
}

impl ClientKey {
    /// The client's columns for it: (secret hash, JWK, identity), and the secret to show once.
    fn columns(&self) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
        match self {
            ClientKey::Secret => {
                let secret = crate::new_secret(CLIENT_SECRET_PREFIX);
                (Some(hash(&secret)), None, None, Some(secret))
            }
            ClientKey::Jwk(j) => (None, Some(j.clone()), None, None),
            ClientKey::Oidc(o) => (None, None, Some(o.clone()), None),
        }
    }
}

/// Which OIDC tokens a machine client trusts: from `issuer`, for `audience`, whose `sub` matches
/// `subject` (`*` for any run) and whose other `claims` match theirs; the issuer's keys fetched,
/// or `jwks` pinned. Checked by roll-oidc, the same rules as roll secrets' workload identities.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrustedIdentity {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    #[serde(default)]
    pub claims: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<String>,
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
    pub oidc: Option<String>,
    pub old_oidc: Option<String>,
}

/// A machine client as it's listed: its id, its grant's id, whether it signs with a key, and the
/// identity it trusts (if that's its credential).
pub struct ListedClient {
    pub id: String,
    pub grant_id: String,
    pub signs_with_key: bool,
    pub trusts: Option<TrustedIdentity>,
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
        let (secret_hash, jwk, oidc, secret) = key.columns();
        let client = Client { id: client_id.clone(), name: new.name.clone(), uri: None, redirect_uris: Vec::new(), created_at: 0, last_used_at: None };
        let now = code_sql::now_ms();
        self.db.write(|c| {
            insert_oauth_grant(c, &grant_id, &client, new, by, now)?;
            c.execute("UPDATE oauth_clients SET confidential = 1, grant_id = ?, secret_hash = ?, jwk = ?, oidc = ? WHERE id = ?", params![grant_id, secret_hash, jwk, oidc, client_id])?;
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
                &format!(
                    "SELECT g.id, o.generation, o.secret_hash, o.jwk, CASE WHEN o.old_until > ? THEN o.old_secret_hash END, CASE WHEN o.old_until > ? THEN o.old_jwk END, \
                     o.oidc, CASE WHEN o.old_until > ? THEN o.old_oidc END {LIVE}"
                ),
                params![now, now, now, client_id, now, now],
                |r| Ok(MachineClient { grant_id: r.get(0)?, generation: r.get(1)?, secret_hash: r.get(2)?, jwk: r.get(3)?, old_secret_hash: r.get(4)?, old_jwk: r.get(5)?, oidc: r.get(6)?, old_oidc: r.get(7)? }),
            )
            .optional()
        })?)
    }

    /// A new credential for a machine client, keeping its id and rules: a new secret (answered,
    /// once), a new key or a new trusted identity. The old one stops now, along with the tokens it got, or keeps working
    /// for `keep_old_ms` (at most a week) while its automation moves over.
    pub fn rotate_machine_client(&self, client_id: &str, key: ClientKey, keep_old_ms: Option<i64>) -> Result<Option<String>> {
        let (secret_hash, jwk, oidc, secret) = key.columns();
        let now = code_sql::now_ms();
        let keep = keep_old_ms.filter(|ms| *ms > 0).map(|ms| ms.min(MAX_KEEP_OLD_MS));
        self.db.write(|c| {
            let grant: Option<String> = c.query_row("SELECT grant_id FROM oauth_clients WHERE id = ? AND confidential = 1 AND grant_id IS NOT NULL", [client_id], |r| r.get(0)).optional()?;
            let Some(grant) = grant else { return Err(Error::NotFound(format!("machine client {client_id}"))) };
            match keep {
                Some(ms) => c.execute(
                    "UPDATE oauth_clients SET old_secret_hash = secret_hash, old_jwk = jwk, old_oidc = oidc, old_until = ?, secret_hash = ?, jwk = ?, oidc = ?, generation = generation + 1 WHERE id = ?",
                    params![now + ms, secret_hash, jwk, oidc, client_id],
                )?,
                None => {
                    // A credential rotated because it may be out: what it got stops too.
                    c.execute("DELETE FROM grant_secrets WHERE grant_id = ? AND kind = 'access'", [&grant])?;
                    c.execute(
                        "UPDATE oauth_clients SET old_secret_hash = NULL, old_jwk = NULL, old_oidc = NULL, old_until = NULL, secret_hash = ?, jwk = ?, oidc = ?, generation = generation + 1 WHERE id = ?",
                        params![secret_hash, jwk, oidc, client_id],
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

    /// The machine clients, newest first.
    pub fn machine_clients(&self) -> Result<Vec<ListedClient>> {
        let rows: Vec<(String, String, bool, Option<String>)> = self.db.read(|c| {
            c.query(
                "SELECT o.id, g.id, o.jwk IS NOT NULL, o.oidc FROM oauth_clients o JOIN grants g ON g.id = o.grant_id WHERE o.confidential = 1 ORDER BY o.created_at DESC",
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
            )
        })?;
        Ok(rows.into_iter().map(|(id, grant_id, signs_with_key, oidc)| ListedClient { id, grant_id, signs_with_key, trusts: oidc.and_then(|o| serde_json::from_str(&o).ok()) }).collect())
    }
}
