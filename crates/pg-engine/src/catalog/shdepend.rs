//! pg_shdepend: objects that refer to roles, as their owner ('o'), in their ACL ('a') or as
//! a policy's target ('r'). Postgres records these rows as objects change; here they're
//! derived from the catalog. Pinned roles (the bootstrap superuser, predefined roles)
//! aren't recorded.

use super::acl::ObjKind;
use super::depend::{NAMESPACE, OPERATOR, POLICY, PROC, REL, STATEXT, TYPE, ts_class};
use super::tsearch::TsKind;
use super::{BOOTSTRAP_SUPERUSER_OID, Catalog};

pub const OWNER: char = 'o';
pub const ACL: char = 'a';
pub const POLICY_TARGET: char = 'r';

/// pg_default_acl's oid.
pub const DEFACL: u32 = 826;
pub const DATABASE: u32 = 1262;
/// pg_tablespace's oid.
pub const TABLESPACE: u32 = 1213;
/// pg_auth_members' oid.
pub const AUTH_MEMBERS: u32 = 1261;

/// One pg_shdepend row (refclassid is always pg_authid).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShDep {
    /// The object lives in this database (false: a shared object, dbid 0).
    pub local: bool,
    pub classid: u32,
    pub objid: u32,
    pub objsubid: i32,
    pub role: u32,
    pub deptype: char,
}

/// Roles below FirstUnpinnedObjectId are pinned: no dependencies on them are recorded.
fn pinned(role: u32) -> bool {
    role < 12000
}

impl Catalog {
    /// The owner of an object with an ACL, by its class (None: not found or unowned).
    pub fn owner_by_class(&self, class: u32, oid: u32, db_owner: u32) -> Option<u32> {
        match class {
            REL => self.rel_owner(oid).map(|o| o.0),
            NAMESPACE => self.schemas.get(&oid).map(|s| s.owner),
            PROC => self.functions.get(&oid).map(|f| f.owner),
            TYPE => self.obj_owner(ObjKind::Type, oid, db_owner),
            DATABASE => Some(db_owner),
            super::depend::FDW => self.fdws.get(&oid).map(|f| f.owner),
            super::depend::SERVER => self.servers.get(&oid).map(|f| f.owner),
            super::depend::LANGUAGE => self.languages.get(&oid).map(|f| f.owner),
            _ => None,
        }
    }

    /// Every pg_shdepend row of this database's objects and of the shared objects, given the
    /// large objects (oid, owner, ACL) the transaction sees and the cluster's databases
    /// (oid, owner).
    pub fn shdepends(
        &self,
        dbs: &[(u32, u32)],
        los: &[(u32, u32, Option<&super::acl::Acl>)],
    ) -> Vec<ShDep> {
        let mut out = Vec::new();
        let local = |classid: u32, objid: u32, objsubid: i32, role: u32, deptype: char| ShDep {
            local: true,
            classid,
            objid,
            objsubid,
            role,
            deptype,
        };
        let owner = |out: &mut Vec<ShDep>, class: u32, oid: u32, role: u32| {
            if !pinned(role) {
                out.push(local(class, oid, 0, role, OWNER));
            }
        };
        for s in self
            .schemas
            .values()
            .filter(|s| !s.name.starts_with("pg_temp") && !s.name.starts_with("pg_toast_temp"))
        {
            owner(&mut out, NAMESPACE, s.oid, s.owner);
        }
        for t in self.tables.values() {
            owner(&mut out, REL, t.oid, t.owner);
        }
        for v in self.views.values() {
            owner(&mut out, REL, v.oid, v.owner);
        }
        for q in self.sequences.values() {
            owner(&mut out, REL, q.oid, q.owner);
        }
        for f in self.functions.values() {
            owner(&mut out, PROC, f.oid, f.owner);
        }
        for e in self.enums.values() {
            owner(&mut out, TYPE, e.oid, e.owner);
        }
        for c in self.composites.values() {
            owner(&mut out, TYPE, c.oid, c.owner);
        }
        for d in self.domains.values() {
            owner(&mut out, TYPE, d.oid, d.owner);
        }
        for r in self.ranges.values() {
            owner(&mut out, TYPE, r.oid, r.owner);
        }
        for o in self.operators.values() {
            owner(&mut out, OPERATOR, o.oid, o.owner);
        }
        for s in self.statistics.values() {
            owner(&mut out, STATEXT, s.oid, s.owner);
        }
        for k in [TsKind::Config, TsKind::Dict] {
            for (oid, _, _) in self.ts_objects(k) {
                owner(&mut out, ts_class(k), oid, self.ts_owner(k, oid));
            }
        }
        for (oid, lo_owner, _) in los {
            owner(&mut out, crate::largeobj::CLASS_OID, *oid, *lo_owner);
        }
        {
            use super::depend::{CONVERSION, FDW, LANGUAGE, OPCLASS, OPFAMILY, SERVER, USER_MAPPING};
            for b in self.base_types.values() {
                owner(&mut out, TYPE, b.oid(), b.owner);
            }
            for f in self.opfamilies.values() {
                owner(&mut out, OPFAMILY, f.oid, f.owner);
            }
            for c in self.opclasses.values() {
                owner(&mut out, OPCLASS, c.oid, c.owner);
            }
            for c in self.conversions.values() {
                owner(&mut out, CONVERSION, c.oid, c.owner);
            }
            for l in self.languages.values() {
                owner(&mut out, LANGUAGE, l.oid, l.owner);
            }
            for f in self.fdws.values() {
                owner(&mut out, FDW, f.oid, f.owner);
            }
            for x in self.servers.values() {
                owner(&mut out, SERVER, x.oid, x.owner);
            }
            for e in self.event_triggers.values() {
                owner(&mut out, super::depend::EVENT_TRIGGER, e.oid, e.owner);
            }
            // A user mapping depends on its user like on an owner.
            for m in self.user_mappings.values() {
                if m.user != 0 {
                    owner(&mut out, USER_MAPPING, m.oid, m.user);
                }
            }
        }
        for d in &self.default_acls {
            owner(&mut out, DEFACL, d.oid, d.role);
        }
        for p in self.publications.values() {
            owner(&mut out, super::depend::PUBLICATION, p.oid, p.owner);
        }
        // Tablespaces and subscriptions are shared objects (dbid 0).
        let shared_owner = |out: &mut Vec<ShDep>, class: u32, oid: u32, role: u32| {
            if !pinned(role) {
                out.push(ShDep {
                    local: false,
                    classid: class,
                    objid: oid,
                    objsubid: 0,
                    role,
                    deptype: OWNER,
                });
            }
        };
        for t in self.tablespaces.values() {
            shared_owner(&mut out, TABLESPACE, t.oid, t.owner);
        }
        for x in self.subscriptions.values() {
            shared_owner(&mut out, super::depend::SUBSCRIPTION, x.oid, x.owner);
        }
        // Roles named in ACLs, other than the object's owner.
        let acl_roles = |out: &mut Vec<ShDep>,
                         shared: bool,
                         class: u32,
                         oid: u32,
                         sub: i32,
                         own: Option<u32>,
                         acl: &super::acl::Acl| {
            let mut roles: Vec<u32> = acl
                .iter()
                .flat_map(|i| [i.grantee, i.grantor])
                .filter(|r| *r != 0 && !pinned(*r) && Some(*r) != own)
                .collect();
            roles.sort_unstable();
            roles.dedup();
            out.extend(roles.into_iter().map(|role| ShDep {
                local: !shared,
                classid: class,
                objid: oid,
                objsubid: sub,
                role,
                deptype: ACL,
            }));
        };
        for a in &self.acls {
            let shared = matches!(a.class, DATABASE | 1213 | 6243);
            let own = if a.class == DATABASE {
                let Some(d) = dbs.iter().find(|d| d.0 == a.oid) else {
                    continue;
                };
                Some(d.1)
            } else {
                self.owner_by_class(a.class, a.oid, BOOTSTRAP_SUPERUSER_OID)
            };
            acl_roles(&mut out, shared, a.class, a.oid, a.sub, own, &a.acl);
        }
        for (oid, lo_owner, acl) in los {
            if let Some(acl) = acl {
                acl_roles(
                    &mut out,
                    false,
                    crate::largeobj::CLASS_OID,
                    *oid,
                    0,
                    Some(*lo_owner),
                    acl,
                );
            }
        }
        for d in &self.default_acls {
            acl_roles(&mut out, false, DEFACL, d.oid, 0, Some(d.role), &d.acl);
        }
        for p in self.policies.values() {
            let mut roles: Vec<u32> = p
                .roles
                .iter()
                .copied()
                .filter(|r| *r != 0 && !pinned(*r))
                .collect();
            roles.sort_unstable();
            roles.dedup();
            out.extend(
                roles
                    .into_iter()
                    .map(|r| local(POLICY, p.oid, 0, r, POLICY_TARGET)),
            );
        }
        // A role membership depends on its grantor (an ACL-type dependency).
        for m in &self.members {
            if !pinned(m.grantor) {
                out.push(ShDep {
                    local: false,
                    classid: AUTH_MEMBERS,
                    objid: m.oid,
                    objsubid: 0,
                    role: m.grantor,
                    deptype: ACL,
                });
            }
        }
        for (db, db_owner) in dbs {
            if !pinned(*db_owner) {
                out.push(ShDep {
                    local: false,
                    classid: DATABASE,
                    objid: *db,
                    objsubid: 0,
                    role: *db_owner,
                    deptype: OWNER,
                });
            }
        }
        out
    }
}

/// The cluster's databases: (oid, owner).
pub fn databases(sess: &crate::session::state::SessionState) -> Vec<(u32, u32)> {
    sess.cluster
        .databases()
        .into_iter()
        .map(|d| (d.0, d.2))
        .collect()
}

/// The pg_shdepend rows of every other database's own objects: (oid, name, rows).
pub fn remote(sess: &crate::session::state::SessionState) -> Vec<(u32, String, Vec<ShDep>)> {
    let me = sess.database_oid();
    sess.cluster
        .other_databases(me)
        .into_iter()
        .map(|(oid, name, db)| {
            let txn = db.begin(
                crate::txn::Isolation::ReadCommitted,
                true,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            let los = crate::largeobj::all(&txn).unwrap_or_default();
            let los: Vec<(u32, u32, Option<&super::acl::Acl>)> = los
                .iter()
                .map(|(o, m)| (*o, m.owner, m.acl.as_ref()))
                .collect();
            let deps = txn
                .catalog()
                .shdepends(&[], &los)
                .into_iter()
                .filter(|d| d.local)
                .collect();
            (oid, name, deps)
        })
        .collect()
}

/// shared_dependency_comparator: object oid, then class, subid (unsigned) and deptype.
pub fn sort(deps: &mut [ShDep]) {
    deps.sort_by_key(|d| (d.objid, d.classid, d.objsubid as u32, d.deptype));
}
