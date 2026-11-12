//! The stored auth tables, exactly as each reference defines them (`docs/nosql/reference/
//! system-tables.md`, `describe-system.md`):
//!
//! - Cassandra 5.0 keeps them in `system_auth` (`AuthKeyspace`): `roles`, `role_members`,
//!   `role_permissions`, `resource_role_permissons_index` (sic), `network_permissions`,
//!   `cidr_groups`, `cidr_permissions`, `identity_to_role`.
//! - ScyllaDB 2026.3 keeps them in `system` (auth v2, managed by group 0): `roles`,
//!   `role_members`, `role_permissions`, `role_attributes` and `service_levels_v2`;
//!   `system_auth` exists but has no tables.
//!
//! They are ordinary storage tables (attached at open), so `SELECT` works on them and branches
//! carry them along.

use nosql_types::Dialect;

use crate::schema::TableKind;
use crate::system::{TableDef, c, def};

/// Where a dialect keeps each auth table.
#[derive(Clone, Copy, Debug)]
pub struct Names {
    pub keyspace: &'static str,
    pub roles: &'static str,
    pub role_members: &'static str,
    pub role_permissions: &'static str,
}

pub fn names(d: Dialect) -> Names {
    Names {
        keyspace: if d.is_scylla() {
            "system"
        } else {
            "system_auth"
        },
        roles: "roles",
        role_members: "role_members",
        role_permissions: "role_permissions",
    }
}

/// Whether `keyspace.table` is one of the dialect's auth tables.
pub fn is_auth_table(d: Dialect, keyspace: &str, table: &str) -> bool {
    tables(d)
        .iter()
        .any(|t| t.keyspace == keyspace && t.name == table)
}

/// The auth table definitions of `d`.
pub fn tables(d: Dialect) -> Vec<TableDef> {
    let r = TableKind::Regular;
    match d {
        Dialect::Cassandra => {
            let ks = "system_auth";
            let gc = 7_776_000;
            vec![
                def(
                    ks,
                    "roles",
                    "role definitions",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("can_login", "boolean", 'r', -1),
                        c("is_superuser", "boolean", 'r', -1),
                        c("salted_hash", "text", 'r', -1),
                        c("member_of", "set<text>", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "role_members",
                    "role memberships lookup table",
                    gc,
                    r,
                    vec![c("role", "text", 'p', 0), c("member", "text", 'c', 0)],
                ),
                def(
                    ks,
                    "role_permissions",
                    "permissions granted to db roles",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("resource", "text", 'c', 0),
                        c("permissions", "set<text>", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "resource_role_permissons_index",
                    "index of db roles with permissions granted on a resource",
                    gc,
                    r,
                    vec![c("resource", "text", 'p', 0), c("role", "text", 'c', 0)],
                ),
                def(
                    ks,
                    "network_permissions",
                    "user network permissions",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("dcs", "frozen<set<text>>", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "cidr_groups",
                    "cidr groups to cidrs mapping",
                    gc,
                    r,
                    vec![
                        c("cidr_group", "text", 'p', 0),
                        c(
                            "cidrs",
                            "frozen<set<frozen<tuple<inet, smallint>>>>",
                            'r',
                            -1,
                        ),
                    ],
                ),
                def(
                    ks,
                    "cidr_permissions",
                    "user cidr permissions",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("cidr_groups", "frozen<set<text>>", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "identity_to_role",
                    "mtls authorized identities lookup table",
                    gc,
                    r,
                    vec![c("identity", "text", 'p', 0), c("role", "text", 'r', -1)],
                ),
            ]
        }
        Dialect::Scylla => {
            let ks = "system";
            let gc = 864_000;
            vec![
                def(
                    ks,
                    "roles",
                    "roles for authentication and RBAC",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("can_login", "boolean", 'r', -1),
                        c("is_superuser", "boolean", 'r', -1),
                        c("member_of", "set<text>", 'r', -1),
                        c("salted_hash", "text", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "role_members",
                    "joins users and their granted roles in RBAC",
                    gc,
                    r,
                    vec![c("role", "text", 'p', 0), c("member", "text", 'c', 0)],
                ),
                def(
                    ks,
                    "role_permissions",
                    "role permissions for CassandraAuthorizer",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("resource", "text", 'c', 0),
                        c("permissions", "set<text>", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "role_attributes",
                    "role permissions in RBAC",
                    gc,
                    r,
                    vec![
                        c("role", "text", 'p', 0),
                        c("name", "text", 'c', 0),
                        c("value", "text", 'r', -1),
                    ],
                ),
                def(
                    ks,
                    "service_levels_v2",
                    "",
                    gc,
                    r,
                    vec![
                        c("service_level", "text", 'p', 0),
                        c("shares", "int", 'r', -1),
                        c("timeout", "duration", 'r', -1),
                        c("workload_type", "text", 'r', -1),
                    ],
                ),
            ]
        }
    }
}
