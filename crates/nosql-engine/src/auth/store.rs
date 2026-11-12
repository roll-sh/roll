//! The auth state of a database: an in-memory [`Snapshot`] of the stored auth tables, reloaded
//! after every change, and the internal CQL the subsystem reads and writes them with.
//!
//! The tables are the source of truth (they persist, branch and restore with the database); the
//! snapshot answers the synchronous permission checks. Every auth statement takes the write
//! lock, writes through CQL, then reloads. Writes from elsewhere (a superuser's `INSERT INTO
//! system_auth.roles`, observed by [`super::Observer`]) mark the snapshot dirty; it reloads at
//! the next async entry point (login, auth statement, schema change) and in the background.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use nosql_types::Dialect;
use parking_lot::{Mutex, RwLock};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::response::{Param, QueryParams, Response, ResponseBody};
use crate::statements::{self, Plan, Prepared};

/// A row of `roles`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleRow {
    pub name: String,
    pub can_login: bool,
    pub is_superuser: bool,
    pub member_of: BTreeSet<String>,
    pub salted_hash: Option<String>,
    /// `writetime(salted_hash)` (microseconds), for Cassandra's password-change rate limit.
    pub hash_written: Option<i64>,
}

/// A row of ScyllaDB's `system.service_levels_v2`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceLevel {
    pub shares: Option<i32>,
    /// A serialised `duration`.
    pub timeout: Option<Bytes>,
    pub workload_type: Option<String>,
}

/// Everything the auth tables hold.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub roles: BTreeMap<String, RoleRow>,
    /// `role_members`: role → its members.
    pub members: BTreeMap<String, BTreeSet<String>>,
    /// `role_permissions`: role → resource name → permission names.
    pub permissions: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// Cassandra's `resource_role_permissons_index`: resource name → roles.
    pub index: BTreeMap<String, BTreeSet<String>>,
    /// Cassandra's `network_permissions`, keyed by role resource name (`roles/r`).
    pub network: BTreeMap<String, BTreeSet<String>>,
    /// Cassandra's `cidr_permissions`.
    pub cidr: BTreeMap<String, BTreeSet<String>>,
    /// Cassandra's `identity_to_role`.
    pub identities: BTreeMap<String, String>,
    /// ScyllaDB's `role_attributes`: role → name → value.
    pub attributes: BTreeMap<String, BTreeMap<String, String>>,
    /// ScyllaDB's `service_levels_v2`.
    pub service_levels: BTreeMap<String, ServiceLevel>,
}

impl Snapshot {
    pub fn exists(&self, role: &str) -> bool {
        self.roles.contains_key(role)
    }

    /// `IRoleManager.getRoles`: the role itself and the roles granted to it (transitively when
    /// `recursive`). Empty for a role that doesn't exist.
    pub fn roles_of(&self, role: &str, recursive: bool) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(r) = self.roles.get(role) else {
            return out;
        };
        out.insert(r.name.clone());
        let mut todo: Vec<&String> = r.member_of.iter().collect();
        while let Some(g) = todo.pop() {
            let Some(gr) = self.roles.get(g) else {
                continue;
            };
            if out.insert(gr.name.clone()) && recursive {
                todo.extend(gr.member_of.iter());
            }
        }
        out
    }

    /// Superuser status, inherited through granted roles (`Roles.hasSuperuserStatus`).
    pub fn is_super(&self, role: &str) -> bool {
        self.roles_of(role, true)
            .iter()
            .any(|r| self.roles.get(r).is_some_and(|x| x.is_superuser))
    }

    /// The permission names granted to `role` itself on resource `name`.
    pub fn granted(&self, role: &str, resource: &str) -> BTreeSet<String> {
        self.permissions
            .get(role)
            .and_then(|m| m.get(resource))
            .cloned()
            .unwrap_or_default()
    }
}

/// The per-database auth state (`Database::auth`).
#[derive(Default)]
pub struct State {
    snap: RwLock<Arc<Snapshot>>,
    loaded: AtomicBool,
    dirty: AtomicBool,
    /// Serialises auth changes and reloads.
    pub(crate) write: tokio::sync::Mutex<()>,
    prepared: Mutex<HashMap<String, Arc<Prepared>>>,
    pub(crate) caches: super::authorizer::Caches,
    /// Added to the clock by the password-change rate limit (tests replaying captured waits).
    pub(crate) clock_skew_us: std::sync::atomic::AtomicI64,
    /// When this process last wrote each role's `salted_hash`, on the (skewed) clock of the
    /// rate limit; roles not written since open fall back to `writetime(salted_hash)`.
    pub(crate) hash_written: Mutex<HashMap<String, i64>>,
}

impl State {
    /// The current snapshot (empty before the first load).
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.read().clone()
    }

    pub(crate) fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn fresh(&self) -> bool {
        self.loaded.load(Ordering::Acquire) && !self.dirty.load(Ordering::Acquire)
    }
}

/// Takes the auth write lock and returns a fresh snapshot.
pub async fn lock(db: &Arc<Database>) -> Result<(tokio::sync::MutexGuard<'_, ()>, Arc<Snapshot>)> {
    let g = db.auth.write.lock().await;
    let s = if db.auth.fresh() {
        db.auth.snapshot()
    } else {
        reload(db).await?
    };
    Ok((g, s))
}

/// Reloads the snapshot if it is stale. Callers holding `State::write` use [`reload`].
pub async fn ensure_fresh(db: &Arc<Database>) -> Result<Arc<Snapshot>> {
    let st = &db.auth;
    if st.fresh() {
        return Ok(st.snapshot());
    }
    let _g = st.write.lock().await;
    if st.fresh() {
        return Ok(st.snapshot());
    }
    reload(db).await
}

/// Reads every auth table into a new snapshot (call with `State::write` held).
pub async fn reload(db: &Arc<Database>) -> Result<Arc<Snapshot>> {
    let st = &db.auth;
    st.dirty.store(false, Ordering::Release);
    let d = db.dialect();
    let ks = super::tables::names(d).keyspace;
    let mut s = Snapshot::default();
    for row in select(
        db,
        &format!(
            "SELECT role, can_login, is_superuser, member_of, salted_hash, writetime(salted_hash) FROM {ks}.roles"
        ),
    )
    .await?
    {
        let name = text(&row[0]).unwrap_or_default();
        s.roles.insert(
            name.clone(),
            RoleRow {
                name,
                can_login: boolean(&row[1]),
                is_superuser: boolean(&row[2]),
                member_of: text_set(&row[3]),
                salted_hash: text(&row[4]),
                hash_written: row[5]
                    .as_ref()
                    .and_then(|b| <[u8; 8]>::try_from(b.as_ref()).ok())
                    .map(i64::from_be_bytes),
            },
        );
    }
    for row in select(db, &format!("SELECT role, member FROM {ks}.role_members")).await? {
        if let (Some(r), Some(m)) = (text(&row[0]), text(&row[1])) {
            s.members.entry(r).or_default().insert(m);
        }
    }
    for row in select(
        db,
        &format!("SELECT role, resource, permissions FROM {ks}.role_permissions"),
    )
    .await?
    {
        if let (Some(r), Some(res)) = (text(&row[0]), text(&row[1])) {
            let perms = text_set(&row[2]);
            if !perms.is_empty() {
                s.permissions.entry(r).or_default().insert(res, perms);
            }
        }
    }
    match d {
        Dialect::Cassandra => {
            for row in select(
                db,
                "SELECT resource, role FROM system_auth.resource_role_permissons_index",
            )
            .await?
            {
                if let (Some(res), Some(r)) = (text(&row[0]), text(&row[1])) {
                    s.index.entry(res).or_default().insert(r);
                }
            }
            for row in select(db, "SELECT role, dcs FROM system_auth.network_permissions").await? {
                if let Some(r) = text(&row[0]) {
                    s.network.insert(r, text_set(&row[1]));
                }
            }
            for row in select(
                db,
                "SELECT role, cidr_groups FROM system_auth.cidr_permissions",
            )
            .await?
            {
                if let Some(r) = text(&row[0]) {
                    s.cidr.insert(r, text_set(&row[1]));
                }
            }
            for row in select(
                db,
                "SELECT identity, role FROM system_auth.identity_to_role",
            )
            .await?
            {
                if let (Some(i), Some(r)) = (text(&row[0]), text(&row[1])) {
                    s.identities.insert(i, r);
                }
            }
        }
        Dialect::Scylla => {
            for row in select(db, "SELECT role, name, value FROM system.role_attributes").await? {
                if let (Some(r), Some(n), Some(v)) = (text(&row[0]), text(&row[1]), text(&row[2])) {
                    s.attributes.entry(r).or_default().insert(n, v);
                }
            }
            for row in select(
                db,
                "SELECT service_level, shares, timeout, workload_type FROM system.service_levels_v2",
            )
            .await?
            {
                if let Some(n) = text(&row[0]) {
                    s.service_levels.insert(
                        n,
                        ServiceLevel {
                            shares: row[1]
                                .as_ref()
                                .and_then(|b| <[u8; 4]>::try_from(b.as_ref()).ok())
                                .map(i32::from_be_bytes),
                            timeout: row[2].clone(),
                            workload_type: text(&row[3]),
                        },
                    );
                }
            }
        }
    }
    let s = Arc::new(s);
    *st.snap.write() = s.clone();
    st.loaded.store(true, Ordering::Release);
    Ok(s)
}

/// A write of the auth subsystem: one CQL statement with its values.
pub struct Write {
    pub cql: String,
    pub values: Vec<Param>,
}

impl Write {
    pub fn new(cql: impl Into<String>, values: Vec<Param>) -> Self {
        Write {
            cql: cql.into(),
            values,
        }
    }
}

/// Applies `writes` atomically (one logged batch), at `timestamp` when given.
pub async fn apply(db: &Arc<Database>, writes: Vec<Write>, timestamp: Option<i64>) -> Result<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let (cql, values) = if writes.len() == 1 {
        let w = writes.into_iter().next().unwrap_or_else(|| unreachable!());
        (w.cql, w.values)
    } else {
        let mut cql = String::from("BEGIN BATCH ");
        let mut values = Vec::new();
        for w in writes {
            cql.push_str(&w.cql);
            cql.push_str("; ");
            values.extend(w.values);
        }
        cql.push_str("APPLY BATCH");
        (cql, values)
    };
    run(db, &cql, values, timestamp).await.map(|_| ())
}

/// Runs a SELECT and returns its rows.
pub async fn select(db: &Arc<Database>, cql: &str) -> Result<Vec<Vec<Option<Bytes>>>> {
    match run(db, cql, Vec::new(), None).await?.body {
        ResponseBody::Rows(r) => Ok(r.rows),
        _ => Ok(Vec::new()),
    }
}

/// Executes internal CQL without authorization (Cassandra's `QueryProcessor.process` with the
/// internal client state).
async fn run(
    db: &Arc<Database>,
    cql: &str,
    values: Vec<Param>,
    timestamp: Option<i64>,
) -> Result<Response> {
    let p = prepared(db, cql)?;
    let params = QueryParams {
        values,
        timestamp,
        ..Default::default()
    };
    match &p.plan {
        Plan::Select(plan) => statements::select::execute(db, plan, &params).await,
        Plan::Modify(plan) => statements::modify::execute(db, plan, &params, &p.stmt).await,
        Plan::Batch(plan) => statements::batch::execute(db, plan, &params, &p.stmt).await,
        _ => Err(Error::server(format!("internal auth statement {cql}"))),
    }
}

fn prepared(db: &Arc<Database>, cql: &str) -> Result<Arc<Prepared>> {
    let schema = db.schema();
    if let Some(p) = db.auth.prepared.lock().get(cql)
        && p.is_valid(&schema)
    {
        return Ok(p.clone());
    }
    let stmt = crate::session::parse(cql, db.dialect())?;
    let p = Arc::new(statements::prepare(db, &schema, stmt, cql, None)?);
    let mut cache = db.auth.prepared.lock();
    if cache.len() > 256 {
        cache.clear();
    }
    cache.insert(cql.to_string(), p.clone());
    Ok(p)
}

pub fn text(v: &Option<Bytes>) -> Option<String> {
    v.as_ref().map(|b| String::from_utf8_lossy(b).into_owned())
}

pub fn boolean(v: &Option<Bytes>) -> bool {
    v.as_ref()
        .is_some_and(|b| b.first().is_some_and(|x| *x != 0))
}

pub fn text_set(v: &Option<Bytes>) -> BTreeSet<String> {
    let Some(b) = v else {
        return BTreeSet::new();
    };
    nosql_types::collection_elements(b)
        .map(|items| {
            items
                .into_iter()
                .flatten()
                .map(|e| String::from_utf8_lossy(e).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Serialises a value for a bind marker.
pub fn p_text(s: &str) -> Param {
    Param::new(Bytes::copy_from_slice(s.as_bytes()))
}

pub fn p_bool(b: bool) -> Param {
    Param::new(Bytes::from_static(if b { &[1] } else { &[0] }))
}

pub fn p_set<'a>(items: impl IntoIterator<Item = &'a str>) -> Param {
    Param::new(nosql_types::pack_collection(
        items.into_iter().map(str::as_bytes),
    ))
}
