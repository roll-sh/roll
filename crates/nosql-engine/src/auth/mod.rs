//! Authentication and authorization: roles, permissions, network permissions, identities and
//! ScyllaDB service levels, with each reference's semantics and messages.
//!
//! Ported from Cassandra 5.0's `PasswordAuthenticator`, `CassandraRoleManager`,
//! `CassandraAuthorizer`, `CassandraNetworkAuthorizer`, `ClientState` and the role / permission
//! statements (`CreateRoleStatement` … `ListPermissionsStatement`, `AddIdentityStatement`);
//! ScyllaDB's behaviour (messages, ordering, auth v2 tables in `system`, service levels) comes
//! from its documentation and its auth-enabled reference (`tests/captured_auth.rs`).
//!
//! # Pieces
//!
//! - [`tables`]: the stored tables (`system_auth.*` / ScyllaDB `system.roles` …).
//! - [`store`]: the [`State`] each [`Database`] holds — a snapshot of those tables, reloaded
//!   after every change — and the internal CQL that reads and writes them.
//! - [`authorizer`]: the `Hooks.authorizer` (both `AllowAllAuthorizer` and
//!   `CassandraAuthorizer`, chosen by `Settings::authorizer`), the `Access` → permission table,
//!   Cassandra's auth caches.
//! - [`statements`]: the `StatementHandler` for role, permission, user and identity statements,
//!   and creator grants / cleanup after schema changes.
//! - [`service_levels`]: ScyllaDB's service levels.
//! - [`hash`]: bcrypt (Cassandra) and crypt(3) SHA-512 (ScyllaDB) password hashes.
//!
//! # Settings
//!
//! `Settings::authenticator` (`AllowAllAuthenticator` | `PasswordAuthenticator`, or a
//! [`certificate`] authenticator; short or qualified class names), `authorizer` (`AllowAllAuthorizer` | `CassandraAuthorizer`),
//! `network_authorizer` (Cassandra), the cache validities, the bcrypt cost and the password
//! change interval. `DbConfig::superuser` is roll's initial superuser for hosted databases.
//!
//! # Bootstrap
//!
//! At open, the initial superuser is created when missing; then, in the Cassandra dialect and
//! only when no role exists, the default `cassandra`/`cassandra` superuser
//! (`CassandraRoleManager.setupDefaultRole`, written at timestamp 0, also with
//! `AllowAllAuthenticator` as on the image). ScyllaDB has no default superuser; its `driver`
//! service level is written at timestamp 0 so a later DROP stays dropped.
//!
//! # For other subsystems
//!
//! - Masking: [`can_unmask`] / [`can_select_masked`] (`UNMASK` / `SELECT_MASKED`).
//! - Alternator: [`salted_hash`] (the SigV4 secret of a role).
//! - DESCRIBE (`WITH INTERNALS [AND PASSWORDS]`): [`snapshot`] holds the roles, grants and
//!   service levels.
//! - UDF / index / view handlers: authorize with `Access::Schema` statement names or
//!   `Access::Permission` (see [`authorizer`]); schema changes they return are seen by
//!   `Authorizer::schema_changed` automatically.

pub mod authorizer;
pub mod certificate;
pub mod hash;
pub mod messages;
pub mod resource;
pub mod service_levels;
pub mod statements;
pub mod store;
pub mod tables;

use std::sync::Arc;

use futures::future::BoxFuture;
use nosql_storage::PartitionUpdate;
use nosql_types::Dialect;

pub use resource::Resource;
pub use store::{Snapshot, State};

use crate::Database;
use crate::config::DbConfig;
use crate::error::Result;
use crate::hooks::{Hooks, WriteObserver};
use crate::schema::Table;
use crate::session::{AuthStep, SessionState};
use messages as m;
use store::{Write, p_bool, p_text};

/// Installs the auth hooks: the authorizer, the statement handler and the observer that notices
/// direct writes to the auth tables.
pub fn install(hooks: &mut Hooks, _cfg: &DbConfig) {
    hooks.authorizer = Arc::new(authorizer::Authz);
    hooks.statements.push(Arc::new(statements::Handler));
    hooks.observers.push(Arc::new(Observer));
}

/// Whether clients must log in (`IAuthenticator.requireAuthentication`).
pub fn requires_authentication(cfg: &DbConfig) -> bool {
    let a = &cfg.settings.authenticator;
    !(a.is_empty() || a.ends_with("AllowAllAuthenticator"))
}

/// The current snapshot of the auth tables.
pub fn snapshot(db: &Database) -> Arc<Snapshot> {
    db.auth.snapshot()
}

/// The stored password hash of `role` (Alternator uses it as the SigV4 secret).
pub async fn salted_hash(db: &Arc<Database>, role: &str) -> Result<Option<String>> {
    let s = store::ensure_fresh(db).await?;
    Ok(s.roles.get(role).and_then(|r| r.salted_hash.clone()))
}

pub use authorizer::{can_select_masked, can_unmask};

/// Sets the password of an existing role (the control plane's rotation of the initial
/// superuser's password; takes effect at once, unlike `ALTER ROLE` under Cassandra's
/// credentials cache); `false` when the role doesn't exist.
pub async fn set_password(db: &Arc<Database>, role: &str, password: &str) -> Result<bool> {
    let _g = db.auth.write.lock().await;
    let s = store::reload(db).await?;
    if !s.exists(role) {
        return Ok(false);
    }
    let h = hash_password(db, password).await;
    let ks = tables::names(db.dialect()).keyspace;
    store::apply(
        db,
        vec![Write::new(
            format!("UPDATE {ks}.roles SET salted_hash = ? WHERE role = ?"),
            vec![p_text(&h), p_text(role)],
        )],
        None,
    )
    .await?;
    store::reload(db).await?;
    db.auth.caches.invalidate(role);
    Ok(true)
}

/// A new hash of `password` in the dialect's format, computed off the async workers.
pub(crate) async fn hash_password(db: &Database, password: &str) -> String {
    let (pw, d, cost) = (
        password.to_string(),
        db.dialect(),
        db.settings().auth_bcrypt_log2_rounds,
    );
    tokio::task::spawn_blocking(move || hash::hash(&pw, d, cost))
        .await
        .unwrap_or_default()
}

/// The bcrypt hash of `cassandra` the default role gets (computing one per new database would
/// cost a bcrypt round each open; any valid hash of the password is equivalent to clients).
const CASSANDRA_DEFAULT_HASH: &str = "$2a$10$9tOsOB7hv7yuTpOaO0rnZ.8PkletfBblQxxL38KQdry4bTgbUyaiS";

/// Creates the initial roles and service levels (see the module docs) and loads the snapshot.
pub async fn bootstrap(db: &Arc<Database>) -> Result<()> {
    let _g = db.auth.write.lock().await;
    let mut s = store::reload(db).await?;
    let d = db.dialect();
    let ks = tables::names(d).keyspace;
    let insert_role = format!(
        "INSERT INTO {ks}.roles (role, is_superuser, can_login, salted_hash) VALUES (?, ?, ?, ?)"
    );
    let mut changed = false;
    if let Some(su) = &db.cfg.superuser
        && !s.exists(&su.name)
    {
        let h = hash_password(db, &su.password).await;
        store::apply(
            db,
            vec![Write::new(
                insert_role.clone(),
                vec![p_text(&su.name), p_bool(true), p_bool(true), p_text(&h)],
            )],
            None,
        )
        .await?;
        changed = true;
    }
    if d == Dialect::Cassandra && s.roles.is_empty() && !changed {
        store::apply(
            db,
            vec![Write::new(
                insert_role,
                vec![
                    p_text("cassandra"),
                    p_bool(true),
                    p_bool(true),
                    p_text(CASSANDRA_DEFAULT_HASH),
                ],
            )],
            Some(0),
        )
        .await?;
        changed = true;
    }
    if d == Dialect::Scylla && !s.service_levels.contains_key("driver") {
        store::apply(
            db,
            vec![Write::new(
                "INSERT INTO system.service_levels_v2 (service_level, shares, workload_type) VALUES ('driver', 200, 'batch')",
                Vec::new(),
            )],
            Some(0),
        )
        .await?;
        changed = true;
    }
    if changed {
        s = store::reload(db).await?;
    }
    let _ = s;
    Ok(())
}

/// One SASL step with the database's authenticator.
///
/// PLAIN tokens are parsed as each reference does: Cassandra scans from the end
/// (`PlainTextSaslAuthenticator.decodeCredentials`: `Password must not be null`,
/// `Authentication ID must not be null`, `Credential format error: …`); ScyllaDB takes the last
/// two NUL-separated fields and only rejects a token without both. A wrong password and an
/// unknown role fail alike; a role without `LOGIN` fails after its password matched.
pub async fn authenticate(
    db: &Arc<Database>,
    state: &mut SessionState,
    sasl: &[u8],
) -> Result<AuthStep> {
    if !requires_authentication(&db.cfg) {
        state.user = None;
        return Ok(success(db.dialect()));
    }
    // Certificate authenticators ignore the token (`certificate`).
    let has_cert = state.client.certificate.is_some();
    if certificate::Authenticator::of(&db.cfg.settings.authenticator).by_certificate(has_cert) {
        return certificate::authenticate(db, state).await;
    }
    let d = db.dialect();
    let (user, password) = decode_plain(d, sasl)?;
    let snap = store::ensure_fresh(db).await?;
    let lookup = || snap.roles.get(&user).and_then(|r| r.salted_hash.clone());
    let hash = match d {
        Dialect::Cassandra => {
            db.auth
                .caches
                .credentials(db.settings().credentials_validity_ms, &user, lookup)
        }
        Dialect::Scylla => lookup(),
    };
    let Some(hash) = hash else {
        return Err(m::bad_credentials(d, &user));
    };
    let ok = {
        let (pw, h) = (password.clone(), hash.clone());
        tokio::task::spawn_blocking(move || hash::verify(&pw, &h, d))
            .await
            .unwrap_or(false)
    };
    if !ok {
        return Err(m::bad_credentials(d, &user));
    }
    let probe = SessionState {
        user: Some(user.clone()),
        ..Default::default()
    };
    let cx = authorizer::Ctx::new(db, &probe);
    let can_login = cx
        .user_roles()
        .iter()
        .find(|r| r.name == user)
        .is_some_and(|r| r.can_login);
    if !can_login {
        return Err(m::not_permitted_to_login(&user));
    }
    state.user = Some(user);
    Ok(success(d))
}

/// `AUTH_SUCCESS`'s token: ScyllaDB sends empty bytes, Cassandra null (both verified on the
/// auth-enabled references).
pub fn success(d: Dialect) -> AuthStep {
    AuthStep::Success(match d {
        Dialect::Scylla => Some(bytes::Bytes::new()),
        Dialect::Cassandra => None,
    })
}

fn decode_plain(d: Dialect, b: &[u8]) -> Result<(String, String)> {
    let s = |x: &[u8]| String::from_utf8_lossy(x).into_owned();
    match d {
        Dialect::Cassandra => {
            let mut user: Option<&[u8]> = None;
            let mut pass: Option<&[u8]> = None;
            let mut end = b.len();
            for i in (0..b.len()).rev() {
                if b[i] == 0 {
                    if pass.is_none() {
                        pass = Some(&b[i + 1..end]);
                    } else if user.is_none() {
                        user = Some(&b[i + 1..end]);
                    } else {
                        return Err(m::bad_credentials_text(
                            "Credential format error: username or password is empty or contains NUL(\\0) character",
                        ));
                    }
                    end = i;
                }
            }
            let pass = pass.filter(|p| !p.is_empty());
            let Some(pass) = pass else {
                return Err(m::bad_credentials_text("Password must not be null"));
            };
            let Some(user) = user.filter(|u| !u.is_empty()) else {
                return Err(m::bad_credentials_text(
                    "Authentication ID must not be null",
                ));
            };
            Ok((s(user), s(pass)))
        }
        Dialect::Scylla => {
            let fields: Vec<&[u8]> = b.split(|c| *c == 0).collect();
            if fields.len() < 2 {
                return Err(m::bad_credentials_text(
                    "Authentication ID must not be null",
                ));
            }
            let user = fields[fields.len() - 2];
            let pass = fields[fields.len() - 1];
            if user.is_empty() && pass.is_empty() {
                return Err(m::bad_credentials_text(
                    "Authentication ID must not be null",
                ));
            }
            Ok((s(user), s(pass)))
        }
    }
}

/// Notices writes to the auth tables that bypass the auth statements and refreshes the snapshot
/// once they are applied.
pub struct Observer;

impl WriteObserver for Observer {
    fn on_write<'a>(
        &'a self,
        db: &'a Arc<Database>,
        writes: &'a [(Arc<Table>, PartitionUpdate)],
    ) -> BoxFuture<'a, Result<crate::hooks::Derived>> {
        Box::pin(async move {
            if writes
                .iter()
                .any(|(table, _)| tables::is_auth_table(db.dialect(), &table.keyspace, &table.name))
            {
                db.auth.mark_dirty();
                let db = db.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    let _ = store::ensure_fresh(&db).await;
                });
            }
            Ok(crate::hooks::Derived::default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tokens_per_dialect() {
        let c = |t: &[u8]| decode_plain(Dialect::Cassandra, t).map_err(|e| e.message);
        let s = |t: &[u8]| decode_plain(Dialect::Scylla, t).map_err(|e| e.message);
        let ok = |u: &str, p: &str| Ok((u.to_string(), p.to_string()));
        assert_eq!(c(b"\0u\0p"), ok("u", "p"));
        assert_eq!(c(b"z\0u\0p"), ok("u", "p"));
        assert_eq!(c(b"\0\0").unwrap_err(), "Password must not be null");
        assert_eq!(c(b"abc").unwrap_err(), "Password must not be null");
        assert_eq!(c(b"\0u\0").unwrap_err(), "Password must not be null");
        assert_eq!(
            c(b"\0\0p").unwrap_err(),
            "Authentication ID must not be null"
        );
        assert_eq!(
            c(b"ab\0c").unwrap_err(),
            "Authentication ID must not be null"
        );
        assert!(
            c(b"\0\0\0")
                .unwrap_err()
                .starts_with("Credential format error")
        );
        assert!(
            c(b"a\0b\0u\0p")
                .unwrap_err()
                .starts_with("Credential format error")
        );
        assert_eq!(s(b"a\0b\0u\0p"), ok("u", "p"));
        assert_eq!(s(b"\0\0p"), ok("", "p"));
        assert_eq!(s(b"ab\0c"), ok("ab", "c"));
        assert_eq!(s(b"").unwrap_err(), "Authentication ID must not be null");
        assert_eq!(s(b"abc").unwrap_err(), "Authentication ID must not be null");
        assert_eq!(
            s(b"\0\0\0").unwrap_err(),
            "Authentication ID must not be null"
        );
    }
}
