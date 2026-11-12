//! Permission checks: `AllowAllAuthorizer` and `CassandraAuthorizer` semantics, ported from
//! Cassandra's `ClientState.ensurePermission` / `CassandraAuthorizer.authorize`, with the
//! ScyllaDB differences observed on its auth-enabled reference.
//!
//! - **Chains.** A permission granted on a resource or any parent applies. Superusers (also
//!   through granted roles) pass every check. Cassandra excludes `ALL KEYSPACES` grants from
//!   system keyspaces for ordinary users; ScyllaDB doesn't.
//! - **Always readable.** The system tables drivers read (`system.local`, `system.peers`,
//!   `system_schema.*`, and in Cassandra `peers_v2`, `size_estimates`, `table_estimates`,
//!   `system_traces.events/sessions`, `system_virtual_schema.*`).
//! - **System keyspaces.** Schema changes there fail before the permission check, with or
//!   without an authorizer (`preventSystemKSSchemaModification`; ScyllaDB's "is protected").
//! - **Caching (Cassandra).** Permission sets (per user and resource), role closures and
//!   credentials are cached for `permissions_validity` / `roles_validity` /
//!   `credentials_validity` (2 s), with no invalidation on change, so a GRANT takes up to that
//!   long to apply to a connected user — as observed on the reference. ScyllaDB applies
//!   changes at once.
//!
//! [`Access`] → permission and resource:
//!
//! | access | permission | resource |
//! |---|---|---|
//! | `Select` | SELECT | the table (a view's or CDC log's base table) |
//! | `Modify` | MODIFY | the table; Cassandra adds SELECT on it and MODIFY on its views when it has views |
//! | `Truncate` | MODIFY | the table |
//! | `Use` | — | (login and datacenter access only) |
//! | `Schema "CREATE KEYSPACE"` | CREATE | `<all keyspaces>` |
//! | `Schema "ALTER KEYSPACE"` / `"DROP KEYSPACE"` | ALTER / DROP | the keyspace |
//! | `Schema "CREATE TABLE"`, `"CREATE TYPE"` | CREATE | Cassandra `<all tables in ks>`, ScyllaDB `<keyspace ks>` |
//! | `Schema "ALTER TYPE"` / `"DROP TYPE"` | ALTER / DROP | (as above) |
//! | `Schema "ALTER TABLE"`, `"CREATE INDEX"`, `"DROP INDEX"`, `"CREATE MATERIALIZED VIEW"`, `"ALTER MATERIALIZED VIEW"`, `"DROP MATERIALIZED VIEW"` | ALTER | the (base) table |
//! | `Schema "DROP TABLE"` | DROP | the table |
//! | `Schema "CREATE FUNCTION"` / `"CREATE AGGREGATE"` | CREATE | `<all functions in ks>` |
//! | `Schema "CREATE TRIGGER"` / `"DROP TRIGGER"` | superuser | — |
//! | `Permission { permission, resource }` | as given | as given (e.g. DROP / EXECUTE on a function) |

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nosql_cql::ast::Permission;
use nosql_types::Dialect;
use parking_lot::Mutex;

use super::messages as m;
use super::resource::{Resource, permission_from_name};
use super::store::{RoleRow, Snapshot};
use crate::Database;
use crate::error::Result;
use crate::hooks::{Access, Authorizer};
use crate::session::SessionState;

/// Cassandra's auth caches (`AuthCache`): entries expire `validity` after they were loaded.
#[derive(Default)]
pub struct Caches {
    perms: Cache<(String, String), BTreeSet<Permission>>,
    roles: Cache<String, Arc<Vec<RoleRow>>>,
    creds: Cache<String, String>,
}

/// Entries with the time they were loaded.
type Cache<K, V> = Mutex<HashMap<K, (Instant, V)>>;

fn cached<K: std::hash::Hash + Eq + Clone, V: Clone>(
    map: &Mutex<HashMap<K, (Instant, V)>>,
    key: &K,
    validity_ms: u64,
    load: impl FnOnce() -> V,
) -> V {
    if validity_ms == 0 {
        return load();
    }
    let now = Instant::now();
    let ttl = Duration::from_millis(validity_ms);
    if let Some((at, v)) = map.lock().get(key)
        && now.duration_since(*at) < ttl
    {
        return v.clone();
    }
    let v = load();
    let mut g = map.lock();
    if g.len() > 10_000 {
        g.clear();
    }
    g.insert(key.clone(), (now, v.clone()));
    v
}

impl Caches {
    /// Drops every cached entry (the validities changed: `Database::set_settings`).
    pub fn clear(&self) {
        self.creds.lock().clear();
        self.roles.lock().clear();
        self.perms.lock().clear();
    }

    /// Drops the cached state of `role` (`nodetool invalidatecredentialscache` and friends).
    pub fn invalidate(&self, role: &str) {
        self.creds.lock().remove(role);
        self.roles.lock().remove(role);
        self.perms.lock().retain(|(u, _), _| u != role);
    }

    /// The stored hash of `role` (`PasswordAuthenticator.CredentialsCache`; missing roles are
    /// not cached).
    pub fn credentials(
        &self,
        validity_ms: u64,
        role: &str,
        load: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        if validity_ms > 0
            && let Some((at, h)) = self.creds.lock().get(role)
            && at.elapsed() < Duration::from_millis(validity_ms)
        {
            return Some(h.clone());
        }
        let h = load()?;
        if validity_ms > 0 {
            self.creds
                .lock()
                .insert(role.to_string(), (Instant::now(), h.clone()));
        }
        Some(h)
    }
}

/// Which authorizer a database runs (`authorizer` setting).
pub fn enforcing(db: &Database) -> bool {
    !db.settings().authorizer.ends_with("AllowAllAuthorizer")
        && !db.settings().authorizer.is_empty()
}

/// Whether the authenticator requires credentials (`requireAuthentication`).
pub fn authenticating(db: &Database) -> bool {
    super::requires_authentication(&db.cfg)
}

/// What a check needs about the current user.
pub struct Ctx<'a> {
    pub db: &'a Database,
    pub d: Dialect,
    /// The authenticated role (`None`: anonymous).
    pub user: Option<&'a str>,
    pub snap: Arc<Snapshot>,
    pub enforce: bool,
    pub authn: bool,
}

impl<'a> Ctx<'a> {
    pub fn new(db: &'a Database, session: &'a SessionState) -> Ctx<'a> {
        Ctx {
            db,
            d: db.dialect(),
            user: session.user.as_deref(),
            snap: db.auth.snapshot(),
            enforce: enforcing(db),
            authn: authenticating(db),
        }
    }

    pub fn user_name(&self) -> &str {
        self.user.unwrap_or("anonymous")
    }

    /// The user's role and the roles granted to it, with their rows (Cassandra: through the
    /// roles cache).
    pub fn user_roles(&self) -> Arc<Vec<RoleRow>> {
        let Some(u) = self.user else {
            return Arc::new(Vec::new());
        };
        let load = || {
            Arc::new(
                self.snap
                    .roles_of(u, true)
                    .iter()
                    .filter_map(|r| self.snap.roles.get(r).cloned())
                    .collect::<Vec<_>>(),
            )
        };
        match self.d {
            Dialect::Cassandra => cached(
                &self.db.auth.caches.roles,
                &u.to_string(),
                self.db.settings().roles_validity_ms,
                load,
            ),
            Dialect::Scylla => load(),
        }
    }

    pub fn user_role_names(&self) -> BTreeSet<String> {
        self.user_roles().iter().map(|r| r.name.clone()).collect()
    }

    /// `AuthenticatedUser.isSuper`: the user's role (or one granted to it) is a superuser.
    pub fn user_is_super(&self) -> bool {
        self.user_roles().iter().any(|r| r.is_superuser)
    }

    /// `ClientState.isSuper`: everyone is with an authenticator that requires no login.
    pub fn is_super(&self) -> bool {
        !self.authn || self.user_is_super()
    }

    /// `IAuthorizer.authorize(user, resource)`: every permission the user holds on `r` itself.
    pub fn permissions_on(&self, r: &Resource) -> BTreeSet<Permission> {
        if !self.enforce || self.user_is_super() {
            return r.applicable(self.d).iter().copied().collect();
        }
        let load = || {
            let mut out = BTreeSet::new();
            let name = r.name();
            for role in self.user_roles().iter() {
                for p in self.snap.granted(&role.name, &name) {
                    if let Some(p) = permission_from_name(&p) {
                        out.insert(p);
                    }
                }
            }
            out
        };
        match self.d {
            Dialect::Cassandra => cached(
                &self.db.auth.caches.perms,
                &(self.user_name().to_string(), r.name()),
                self.db.settings().permissions_validity_ms,
                load,
            ),
            Dialect::Scylla => load(),
        }
    }

    /// `ClientState.validateLogin`: the user's datacenter access (Cassandra network
    /// authorizer).
    pub fn validate_login(&self) -> Result<()> {
        if self.d.is_scylla() || !network_enforcing(self.db) {
            return Ok(());
        }
        let Some(u) = self.user else {
            return Ok(());
        };
        if !dc_permissions(self.db, &self.snap, u).can_access(&self.db.cfg.datacenter) {
            return Err(m::no_dc_access(&self.db.cfg.datacenter));
        }
        Ok(())
    }

    /// `ClientState.ensureNotAnonymous`.
    pub fn ensure_not_anonymous(&self) -> Result<()> {
        self.validate_login()?;
        if self.user.is_none() {
            return Err(m::anonymous());
        }
        Ok(())
    }

    /// `ensurePermission(perm, resource)`: the permission on the resource or a parent.
    pub fn ensure(&self, perm: Permission, r: &Resource) -> Result<()> {
        if !self.enforce {
            return Ok(());
        }
        // Built-in functions are unrestricted.
        if let Resource::Function { keyspace, .. } = r
            && keyspace == "system"
        {
            return Ok(());
        }
        if self.user_is_super() {
            return Ok(());
        }
        let mut chain = r.chain(self.d);
        if self.d == Dialect::Cassandra
            && r.data_keyspace().is_some_and(is_system_keyspace_cassandra)
        {
            chain.retain(|x| *x != Resource::DataRoot);
        }
        if chain.iter().any(|x| self.permissions_on(x).contains(&perm)) {
            return Ok(());
        }
        if self.d.is_scylla()
            && perm == Permission::Select
            && matches!(r, Resource::Table(k, t) if k == "system" && SCYLLA_INDEXING_TABLES.contains(&t.as_str()))
        {
            return Err(m::none_of_indexing(self.user_name(), &r.display()));
        }
        Err(m::no_permission(
            self.user_name(),
            perm.as_str(),
            &r.display_in(self.d),
        ))
    }

    /// `ensurePermission(keyspace, perm, dataResource)`: login, system keyspace protection,
    /// always-readable tables, then the chain.
    pub fn ensure_data(&self, perm: Permission, r: &Resource) -> Result<()> {
        self.validate_login()?;
        prevent_system_change(self.d, perm, r, self.enforce && !self.user_is_super())?;
        if !self.enforce {
            return Ok(());
        }
        if perm == Permission::Select && always_readable(self.d, r) {
            return Ok(());
        }
        if self.d.is_scylla()
            && !self.user_is_super()
            && !matches!(perm, Permission::Select | Permission::Describe)
            && let Some(ks @ ("system" | "system_schema")) = r.data_keyspace()
        {
            return Err(m::system_select_only(ks));
        }
        self.ensure(perm, r)
    }
}

/// The tables ScyllaDB's external index services read (`VECTOR_SEARCH_INDEXING` and friends).
const SCYLLA_INDEXING_TABLES: &[&str] = &[
    "group0_history",
    "versions",
    "cdc_streams",
    "cdc_timestamps",
    "tablets",
];

pub fn is_system_keyspace_cassandra(ks: &str) -> bool {
    matches!(
        ks,
        "system"
            | "system_schema"
            | "system_views"
            | "system_virtual_schema"
            | "system_auth"
            | "system_distributed"
            | "system_traces"
    )
}

fn is_replicated_system(ks: &str) -> bool {
    matches!(ks, "system_auth" | "system_distributed" | "system_traces")
}

fn is_local_system(d: Dialect, ks: &str) -> bool {
    match d {
        Dialect::Cassandra => matches!(
            ks,
            "system" | "system_schema" | "system_views" | "system_virtual_schema"
        ),
        Dialect::Scylla => matches!(ks, "system" | "system_schema"),
    }
}

/// `preventSystemKSSchemaModification` (Cassandra) and the ScyllaDB equivalents observed on its
/// reference. `ordinary`: the user is checked against grants afterwards (ScyllaDB lets
/// superusers through to the schema code for some of these).
fn prevent_system_change(
    d: Dialect,
    perm: Permission,
    r: &Resource,
    _ordinary: bool,
) -> Result<()> {
    if !matches!(
        perm,
        Permission::Create | Permission::Alter | Permission::Drop
    ) {
        return Ok(());
    }
    let Some(ks) = r.data_keyspace() else {
        return Ok(());
    };
    if is_local_system(d, ks) {
        return Err(m::not_user_modifiable(ks));
    }
    if !is_replicated_system(ks) {
        return Ok(());
    }
    match d {
        Dialect::Cassandra => {
            if perm == Permission::Alter && matches!(r, Resource::Keyspace(_)) {
                return Ok(());
            }
            Err(m::cannot(perm.as_str(), &r.display()))
        }
        Dialect::Scylla => {
            if let Resource::Table(k, t) = r
                && k == "system_auth"
                && matches!(
                    t.as_str(),
                    "roles" | "role_members" | "role_permissions" | "role_attributes"
                )
                && perm != Permission::Create
            {
                return Err(m::protected(&r.display()));
            }
            if perm == Permission::Drop
                && (matches!(r, Resource::Keyspace(_)) || ks == "system_traces")
            {
                return Err(m::cannot(perm.as_str(), &r.display()));
            }
            Ok(())
        }
    }
}

fn always_readable(d: Dialect, r: &Resource) -> bool {
    let Resource::Table(k, t) = r else {
        return false;
    };
    match (d, k.as_str()) {
        (_, "system_schema") => true,
        (Dialect::Scylla, "system") => matches!(t.as_str(), "local" | "peers"),
        (Dialect::Cassandra, "system") => matches!(
            t.as_str(),
            "local" | "peers" | "peers_v2" | "size_estimates" | "table_estimates"
        ),
        (Dialect::Cassandra, "system_traces") => matches!(t.as_str(), "events" | "sessions"),
        (Dialect::Cassandra, "system_virtual_schema") => true,
        _ => false,
    }
}

/// Cassandra's `network_authorizer` is `CassandraNetworkAuthorizer`.
pub fn network_enforcing(db: &Database) -> bool {
    db.settings()
        .network_authorizer
        .ends_with("CassandraNetworkAuthorizer")
}

/// `DCPermissions` of a role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DcPermissions {
    All,
    None,
    Subset(BTreeSet<String>),
}

impl DcPermissions {
    pub fn can_access(&self, dc: &str) -> bool {
        match self {
            DcPermissions::All => true,
            DcPermissions::None => false,
            DcPermissions::Subset(s) => s.contains(dc),
        }
    }

    /// `toString`, as `LIST ROLES` shows it.
    pub fn text(&self) -> String {
        match self {
            DcPermissions::All => "ALL".into(),
            DcPermissions::None => "n/a".into(),
            DcPermissions::Subset(s) => s.iter().cloned().collect::<Vec<_>>().join(", "),
        }
    }
}

/// `INetworkAuthorizer.authorize(role)`.
pub fn dc_permissions(db: &Database, snap: &Snapshot, role: &str) -> DcPermissions {
    if !network_enforcing(db) {
        return DcPermissions::All;
    }
    if !snap.roles.get(role).is_some_and(|r| r.can_login) {
        return DcPermissions::None;
    }
    if snap.is_super(role) {
        return DcPermissions::All;
    }
    match snap.network.get(&format!("roles/{role}")) {
        Some(dcs) if !dcs.is_empty() => DcPermissions::Subset(dcs.clone()),
        _ => DcPermissions::All,
    }
}

/// The authorizer installed in `Hooks.authorizer`.
pub struct Authz;

impl Authorizer for Authz {
    fn authorize(&self, db: &Database, session: &SessionState, access: &Access) -> Result<()> {
        let cx = Ctx::new(db, session);
        let d = cx.d;
        match access {
            Access::Select { keyspace, table } => {
                let (k, t) = base_table(db, keyspace, table);
                cx.ensure_data(Permission::Select, &Resource::Table(k, t))
            }
            Access::Modify { keyspace, table } => {
                let r = Resource::Table(keyspace.to_string(), table.to_string());
                cx.ensure_data(Permission::Modify, &r)?;
                if d == Dialect::Cassandra {
                    let views = views_of(db, keyspace, table);
                    if !views.is_empty() {
                        cx.ensure_data(Permission::Select, &r)?;
                        for v in views {
                            cx.ensure_data(
                                Permission::Modify,
                                &Resource::Table(keyspace.to_string(), v),
                            )?;
                        }
                    }
                }
                Ok(())
            }
            Access::Truncate { keyspace, table } => {
                // A missing table fails first, in the schema code (as both references resolve
                // it at prepare).
                let exists = db
                    .schema()
                    .keyspace(keyspace)
                    .is_some_and(|k| k.tables.contains_key(*table));
                if !exists {
                    return Ok(());
                }
                cx.ensure_data(
                    Permission::Modify,
                    &Resource::Table(keyspace.to_string(), table.to_string()),
                )
            }
            Access::Use { .. } => cx.validate_login(),
            Access::Schema {
                keyspace,
                table,
                statement,
            } => schema(&cx, keyspace, table, statement),
            Access::Permission {
                permission,
                resource,
            } => {
                cx.validate_login()?;
                cx.ensure(*permission, resource)
            }
        }
    }

    fn schema_changed<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a SessionState,
        before: &'a crate::schema::Schema,
        change: &'a crate::response::SchemaChange,
    ) -> futures::future::BoxFuture<'a, Result<()>> {
        Box::pin(super::statements::schema_changed(
            db, session, before, change,
        ))
    }
}

fn schema(cx: &Ctx, ks: &str, table: &str, statement: &str) -> Result<()> {
    use Permission as P;
    let d = cx.d;
    let ks_s = ks.to_string();
    let ks_level = |p: P| -> Result<()> {
        // ScyllaDB checks the keyspace exists first for these (`Can't find a keyspace`).
        if d.is_scylla() && cx.db.schema().keyspace(ks).is_none() && !is_replicated_system(ks) {
            return Ok(());
        }
        let r = match d {
            Dialect::Cassandra => Resource::AllTables(ks_s.clone()),
            Dialect::Scylla => Resource::Keyspace(ks_s.clone()),
        };
        cx.ensure_data(p, &r)
    };
    let table_r = || Resource::Table(ks_s.clone(), table.to_string());
    match statement {
        "CREATE KEYSPACE" => {
            cx.validate_login()?;
            cx.ensure(P::Create, &Resource::DataRoot)
        }
        "ALTER KEYSPACE" => cx.ensure_data(P::Alter, &Resource::Keyspace(ks_s.clone())),
        "DROP KEYSPACE" => cx.ensure_data(P::Drop, &Resource::Keyspace(ks_s.clone())),
        "CREATE TABLE" | "CREATE TYPE" => ks_level(P::Create),
        "ALTER TYPE" => ks_level(P::Alter),
        "DROP TYPE" => ks_level(P::Drop),
        "ALTER TABLE"
        | "CREATE INDEX"
        | "CREATE CUSTOM INDEX"
        | "DROP INDEX"
        | "CREATE MATERIALIZED VIEW"
        | "ALTER MATERIALIZED VIEW"
        | "DROP MATERIALIZED VIEW" => cx.ensure_data(P::Alter, &table_r()),
        "DROP TABLE" => cx.ensure_data(P::Drop, &table_r()),
        "CREATE FUNCTION" | "CREATE AGGREGATE" => {
            cx.validate_login()?;
            cx.ensure(P::Create, &Resource::FunctionsIn(ks_s.clone()))
        }
        "CREATE TRIGGER" => {
            if cx.is_super() {
                Ok(())
            } else {
                Err(m::create_trigger_superuser())
            }
        }
        "DROP TRIGGER" => {
            if cx.is_super() {
                Ok(())
            } else {
                Err(m::drop_trigger_superuser())
            }
        }
        _ => cx.validate_login(),
    }
}

/// A view's (or ScyllaDB CDC log's) base table: views inherit its permissions.
fn base_table(db: &Database, ks: &str, table: &str) -> (String, String) {
    let schema = db.schema();
    if let Some(t) = schema.keyspace(ks).and_then(|k| k.tables.get(table))
        && let Some(v) = &t.view
    {
        return (ks.to_string(), v.base_table_name.clone());
    }
    if db.dialect().is_scylla()
        && let Some(base) = table.strip_suffix("_scylla_cdc_log")
        && schema
            .keyspace(ks)
            .is_some_and(|k| k.tables.contains_key(base))
    {
        return (ks.to_string(), base.to_string());
    }
    (ks.to_string(), table.to_string())
}

/// Names of the materialized views of `ks.table`.
fn views_of(db: &Database, ks: &str, table: &str) -> Vec<String> {
    let schema = db.schema();
    let Some(k) = schema.keyspace(ks) else {
        return Vec::new();
    };
    k.tables
        .values()
        .filter(|t| {
            t.view
                .as_ref()
                .is_some_and(|v| v.base_table_name == table && !t.name.ends_with("_index"))
        })
        .map(|t| t.name.clone())
        .collect()
}

/// Whether the session's user may see unmasked values of `ks.table` (Cassandra's
/// `hasTablePermission(table, UNMASK)`): superusers, `AllowAllAuthorizer`, ScyllaDB (no
/// masking), or an `UNMASK` grant on the table or a parent.
pub fn can_unmask(db: &Database, session: &SessionState, ks: &str, table: &str) -> bool {
    has_table_permission(db, session, ks, table, Permission::Unmask)
}

/// `hasTablePermission(table, SELECT_MASKED)`: may restrict masked columns in `WHERE`.
pub fn can_select_masked(db: &Database, session: &SessionState, ks: &str, table: &str) -> bool {
    has_table_permission(db, session, ks, table, Permission::SelectMasked)
}

fn has_table_permission(
    db: &Database,
    session: &SessionState,
    ks: &str,
    table: &str,
    p: Permission,
) -> bool {
    if db.dialect().is_scylla() {
        return true;
    }
    let cx = Ctx::new(db, session);
    if !cx.enforce {
        return true;
    }
    Resource::Table(ks.to_string(), table.to_string())
        .chain(cx.d)
        .iter()
        .any(|r| cx.permissions_on(r).contains(&p))
}
