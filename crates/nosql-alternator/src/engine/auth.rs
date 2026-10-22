//! SigV4 credentials and permissions from the database's roles, as ScyllaDB does it: the access
//! key id is a role name and the secret access key is that role's `salted_hash`
//! (`system.roles`), so a role created with CQL can sign Alternator requests at once; with
//! `CassandraAuthorizer`, operations need the CQL permissions of [`crate::backend::Permission`]
//! (granted with CQL `GRANT`), creators are granted everything on what they create, and
//! dropping a table or index revokes everyone's permissions on it.

use std::sync::Arc;

use async_trait::async_trait;
use nosql_cql::ast::Permission as CqlPermission;
use nosql_engine::auth::Resource as EngineResource;
use nosql_engine::auth::authorizer::{self, Ctx};
use nosql_engine::{Database, Dialect, QueryParams, SchemaChange, SessionState};

use super::engine_error;
use crate::auth::{Credentials, RoleSecret};
use crate::backend::{Permission, Resource};
use crate::error::{Error, ErrorKind, Result};

pub struct RoleCredentials(pub Arc<Database>);

#[async_trait]
impl Credentials for RoleCredentials {
    async fn secret(&self, role: &str) -> Result<Option<RoleSecret>> {
        let Some(hash) = nosql_engine::auth::salted_hash(&self.0, role)
            .await
            .map_err(engine_error)?
        else {
            return Ok(None);
        };
        let can_login = nosql_engine::auth::snapshot(&self.0)
            .roles
            .get(role)
            .is_some_and(|r| r.can_login);
        Ok(Some(RoleSecret {
            salted_hash: hash,
            can_login,
        }))
    }
}

/// The engine resource of a check.
fn resource(on: &Resource) -> EngineResource {
    match on {
        Resource::AllKeyspaces => EngineResource::DataRoot,
        Resource::Table { keyspace, table } => {
            EngineResource::Table(keyspace.clone(), table.clone())
        }
    }
}

fn permission(p: Permission) -> CqlPermission {
    match p {
        Permission::Select => CqlPermission::Select,
        Permission::Modify => CqlPermission::Modify,
        Permission::Create => CqlPermission::Create,
        Permission::Drop => CqlPermission::Drop,
        Permission::Alter => CqlPermission::Alter,
    }
}

/// `role` holds `perm` on `on` (the engine's CassandraAuthorizer rules: grants on the resource
/// or a parent, directly or through granted roles; superusers hold everything).
pub fn authorize(db: &Database, role: &str, perm: Permission, on: &Resource) -> Result<()> {
    let state = SessionState {
        user: Some(role.to_string()),
        ..Default::default()
    };
    let cx = Ctx::new(db, &state);
    if cx.ensure(permission(perm), &resource(on)).is_ok() {
        return Ok(());
    }
    let what = match on {
        Resource::AllKeyspaces => "all keyspaces".to_string(),
        Resource::Table { keyspace, table } => format!("table {keyspace}.{table}"),
    };
    Err(Error::new(
        ErrorKind::AccessDenied,
        format!("{} access on {what} is denied to role {role}", perm.name()),
    ))
}

/// Whether `role` is a superuser, or permissions aren't enforced.
pub fn is_superuser(db: &Database, role: &str) -> bool {
    let state = SessionState {
        user: Some(role.to_string()),
        ..Default::default()
    };
    let cx = Ctx::new(db, &state);
    !cx.enforce || cx.user_is_super()
}

/// A session of the database's superuser (for grants Alternator makes on a role's behalf).
fn superuser_session(db: &Arc<Database>) -> Option<nosql_engine::Session> {
    let su = db.config().superuser.as_ref()?;
    let mut s = db.connect(Default::default());
    s.state.user = Some(su.name.clone());
    Some(s)
}

/// Grants `role` every permission on a new table or view.
pub async fn grant_creator(db: &Arc<Database>, role: &str, on: &Resource) -> Result<()> {
    if !authorizer::enforcing(db) {
        return Ok(());
    }
    let Resource::Table { keyspace, table } = on else {
        return Ok(());
    };
    let Some(mut s) = superuser_session(db) else {
        return Ok(());
    };
    let q = |n: &str| nosql_types::quote_identifier(n, Dialect::Scylla).into_owned();
    let cql = format!(
        "GRANT ALL PERMISSIONS ON TABLE {}.{} TO {}",
        q(keyspace),
        q(table),
        q(role)
    );
    s.query(&cql, QueryParams::default())
        .await
        .map_err(engine_error)?;
    Ok(())
}

/// Revokes everyone's permissions on dropped tables, views or a keyspace (`before` is the schema
/// they were in).
pub async fn revoke_dropped(
    db: &Arc<Database>,
    before: &nosql_engine::schema::Schema,
    changes: &[SchemaChange],
) -> Result<()> {
    if !authorizer::enforcing(db) {
        return Ok(());
    }
    let state = SessionState::default();
    let authz = db.hooks().authorizer;
    for c in changes {
        authz
            .schema_changed(db, &state, before, c)
            .await
            .map_err(engine_error)?;
    }
    Ok(())
}
