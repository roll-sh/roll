//! The cluster-wide catalogs (pg_authid, pg_auth_members, pg_db_role_setting, pg_shdescription
//! and the ACLs of databases, tablespaces and parameters). Every database keeps a copy in its
//! own catalog, stamped with the version of the shared state it holds; the engine keeps the
//! authoritative copy and brings the others up to date when one of them changes it.

use super::acl::{AuthMember, ObjAcl};
use super::{Catalog, Role, RoleSetting};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// pg_database, pg_tablespace and pg_parameter_acl: the shared classes `acls` keys.
const SHARED_CLASSES: [u32; 3] = [1262, 1213, 6243];
/// pg_default and pg_global.
const TABLESPACES: [u32; 2] = [1663, 1664];

/// CREATE TABLESPACE: a pg_tablespace row (pg_default and pg_global are built in).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tablespace {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
    /// spcoptions, as `name=value` (None: NULL).
    pub options: Option<Vec<String>>,
    /// LOCATION; empty for one made in place (`pg_tblspc/<oid>`).
    pub location: String,
}

/// A pg_subscription_rel row (a database's own catalog: the relation is that database's).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubRel {
    pub relid: u32,
    /// srsubstate: 'i' init, 'd' data copy, 'f' finished copy, 's' synchronized, 'r' ready.
    pub state: char,
    /// srsublsn (0: NULL).
    pub lsn: u64,
}

/// CREATE SUBSCRIPTION: a pg_subscription row (its database's, in the shared catalog).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub oid: u32,
    pub dbid: u32,
    /// subskiplsn (0: none).
    pub skiplsn: u64,
    pub name: String,
    pub owner: u32,
    pub enabled: bool,
    pub binary: bool,
    /// substream: 'f' off, 't' on, 'p' parallel.
    pub stream: char,
    /// subtwophasestate: 'd' disabled, 'p' pending, 'e' enabled.
    pub twophase: char,
    pub disable_on_err: bool,
    pub password_required: bool,
    pub run_as_owner: bool,
    pub failover: bool,
    pub conninfo: String,
    pub slot_name: Option<String>,
    pub synccommit: String,
    pub publications: Vec<String>,
    pub origin: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SharedCatalog {
    pub roles: BTreeMap<u32, Role>,
    pub members: Vec<AuthMember>,
    pub role_settings: Vec<RoleSetting>,
    pub acls: Vec<ObjAcl>,
    /// Comments on roles, databases and tablespaces.
    pub comments: Vec<(u32, i32, String)>,
    /// pg_parameter_acl's (oid, name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<(u32, String)>,
    /// CREATE TABLESPACE tablespaces.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tablespaces: BTreeMap<u32, Tablespace>,
    /// Every database's subscriptions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub subscriptions: BTreeMap<u32, Subscription>,
}

impl SharedCatalog {
    /// The shared part of `c`; `databases` are the cluster's database oids.
    pub fn extract(c: &Catalog, databases: &BTreeSet<u32>) -> SharedCatalog {
        // (The shared objects' comments, looked up by object: every commit of DDL reads
        // them, and a scan of every comment made that O(catalog).)
        let ids: BTreeSet<u32> = (c.roles.keys().chain(databases).chain(&TABLESPACES))
            .chain(c.tablespaces.keys())
            .copied()
            .collect();
        let comments: Vec<(u32, i32, String)> = (ids.iter())
            .flat_map(|o| c.comments_of(*o).iter().map(move |(s, _, t)| (*o, *s, t.clone())))
            .collect();
        SharedCatalog {
            roles: c.roles.clone(),
            members: c.members.clone(),
            role_settings: c.role_settings.clone(),
            acls: c
                .acls
                .iter()
                .filter(|a| SHARED_CLASSES.contains(&a.class))
                .cloned()
                .collect(),
            comments,
            parameters: c.parameters.clone(),
            tablespaces: c.tablespaces.clone(),
            subscriptions: c.subscriptions.clone(),
        }
    }

    /// Replace the shared part of `c` with this one, stamped as `version`.
    pub fn apply(&self, c: &mut Catalog, databases: &BTreeSet<u32>, version: u64) {
        let ids: BTreeSet<u32> = (c.roles.keys().chain(c.tablespaces.keys()))
            .chain(self.roles.keys())
            .chain(databases)
            .chain(&TABLESPACES)
            .chain(self.tablespaces.keys())
            .copied()
            .collect();
        // The shared objects' comments as this copy has them (those that differ set again).
        for o in ids {
            let want: Vec<(i32, String)> =
                (self.comments.iter()).filter(|x| x.0 == o).map(|x| (x.1, x.2.clone())).collect();
            let same = {
                let have = c.comments_of(o);
                have.len() == want.len() && have.iter().zip(&want).all(|(h, w)| h.0 == w.0 && h.2 == w.1)
            };
            if !same {
                c.drop_comments(o);
                for (s, t) in want {
                    c.set_comment(o, s, t);
                }
            }
        }
        c.acls.retain(|a| !SHARED_CLASSES.contains(&a.class));
        c.acls.extend(self.acls.iter().cloned());
        c.roles = self.roles.clone();
        c.members = self.members.clone();
        c.role_settings = self.role_settings.clone();
        c.parameters = self.parameters.clone();
        c.tablespaces = self.tablespaces.clone();
        c.subscriptions = self.subscriptions.clone();
        c.shared_stamp = (version, self.hash());
    }

    /// Three-way merge: what `mine` changed since `base`, applied to `theirs` (a newer version
    /// another transaction committed). None when both changed the same thing.
    pub fn merge(
        base: &SharedCatalog,
        mine: &SharedCatalog,
        theirs: &SharedCatalog,
    ) -> Option<SharedCatalog> {
        let pairs =
            |m: &BTreeMap<u32, Role>| m.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>();
        let roles = merge_keyed(
            &pairs(&base.roles),
            &pairs(&mine.roles),
            &pairs(&theirs.roles),
        )?
        .into_iter()
        .collect();
        let members = merge_list(&base.members, &mine.members, &theirs.members, |m| m.oid)?;
        let role_settings = merge_list(
            &base.role_settings,
            &mine.role_settings,
            &theirs.role_settings,
            |r| (r.role, r.database.clone()),
        )?;
        let acls = merge_list(&base.acls, &mine.acls, &theirs.acls, |a| {
            (a.class, a.oid, a.sub)
        })?;
        let mut comments = merge_list(&base.comments, &mine.comments, &theirs.comments, |c| {
            (c.0, c.1)
        })?;
        comments.sort_by_key(|x| (x.0, x.1));
        let parameters = merge_list(
            &base.parameters,
            &mine.parameters,
            &theirs.parameters,
            |p| p.0,
        )?;
        let keyed = |m: &BTreeMap<u32, Tablespace>| {
            m.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>()
        };
        let tablespaces = merge_keyed(
            &keyed(&base.tablespaces),
            &keyed(&mine.tablespaces),
            &keyed(&theirs.tablespaces),
        )?
        .into_iter()
        .collect();
        let subs = |m: &BTreeMap<u32, Subscription>| {
            m.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>()
        };
        let subscriptions = merge_keyed(
            &subs(&base.subscriptions),
            &subs(&mine.subscriptions),
            &subs(&theirs.subscriptions),
        )?
        .into_iter()
        .collect();
        Some(SharedCatalog {
            roles,
            members,
            role_settings,
            acls,
            comments,
            parameters,
            tablespaces,
            subscriptions,
        })
    }

    /// FNV-1a of the serialised form: whether a transaction changed its copy.
    pub fn hash(&self) -> u64 {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3)
        })
    }
}

fn same<V: Serialize>(a: Option<&V>, b: Option<&V>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => serde_json::to_vec(x).ok() == serde_json::to_vec(y).ok(),
        _ => false,
    }
}

/// `merge` of keyed entries, in `theirs`'s order with what only `mine` added after it.
fn merge_keyed<K: Ord + Clone, V: Clone + Serialize>(
    base: &[(K, V)],
    mine: &[(K, V)],
    theirs: &[(K, V)],
) -> Option<Vec<(K, V)>> {
    let find = |l: &[(K, V)], k: &K| l.iter().find(|x| &x.0 == k).map(|x| x.1.clone());
    let mut keys: Vec<K> = theirs.iter().map(|x| x.0.clone()).collect();
    keys.extend(
        mine.iter()
            .map(|x| x.0.clone())
            .filter(|k| !theirs.iter().any(|x| &x.0 == k)),
    );
    let gone: Vec<K> = base
        .iter()
        .map(|x| x.0.clone())
        .filter(|k| !keys.contains(k))
        .collect();
    keys.extend(gone);
    let mut out = Vec::new();
    for k in keys {
        let (b, m, t) = (find(base, &k), find(mine, &k), find(theirs, &k));
        let v = if same(m.as_ref(), b.as_ref()) {
            t
        } else if same(t.as_ref(), b.as_ref()) || same(t.as_ref(), m.as_ref()) {
            m
        } else {
            return None;
        };
        if let Some(v) = v {
            out.push((k, v));
        }
    }
    Some(out)
}

fn merge_list<K: Ord + Clone, V: Clone + Serialize>(
    base: &[V],
    mine: &[V],
    theirs: &[V],
    key: impl Fn(&V) -> K,
) -> Option<Vec<V>> {
    let pairs = |l: &[V]| l.iter().map(|v| (key(v), v.clone())).collect::<Vec<_>>();
    Some(
        merge_keyed(&pairs(base), &pairs(mine), &pairs(theirs))?
            .into_iter()
            .map(|x| x.1)
            .collect(),
    )
}

/// The cluster's oid counter (Postgres's is cluster-wide too, so databases, roles and every
/// database's objects never share an oid). `persist` durably records how far it may go before
/// asking again.
pub struct OidGen {
    state: Mutex<(u32, u32)>,
    persist: Box<dyn Fn(u32) + Send + Sync>,
}

impl std::fmt::Debug for OidGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidGen")
            .field("next", &self.state.lock().0)
            .finish()
    }
}

impl OidGen {
    pub fn new(next: u32, persist: impl Fn(u32) + Send + Sync + 'static) -> OidGen {
        OidGen {
            state: Mutex::new((next, next)),
            persist: Box::new(persist),
        }
    }

    /// `n` consecutive oids, the first at least `floor`.
    pub fn take(&self, floor: u32, n: u32) -> u32 {
        let mut s = self.state.lock();
        let o = s.0.max(floor);
        s.0 = o + n;
        if s.0 > s.1 {
            s.1 = s.0 + 1024;
            (self.persist)(s.1);
        }
        o
    }

    /// The next oid to hand out.
    pub fn peek(&self) -> u32 {
        self.state.lock().0
    }
}
