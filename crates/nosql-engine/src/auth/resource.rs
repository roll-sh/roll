//! Resources permissions are granted on, ported from Cassandra's `DataResource`, `RoleResource`,
//! `FunctionResource` and `JMXResource` (plus ScyllaDB's service-level root).
//!
//! A resource has a stored *name* (`data/ks/t`, `roles/r`, `functions/ks/f[Int32Type]`, the key
//! of `role_permissions.resource`) and a *display* form used in messages and `LIST PERMISSIONS`
//! (`<table ks.t>`). Resources form chains (`<table ks.t>` → `<all tables in ks>` (Cassandra) →
//! `<keyspace ks>` → `<all keyspaces>`): a permission granted on a parent applies below it.

use nosql_cql::ast::Permission;
use nosql_types::Dialect;

/// A permission target.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resource {
    /// `ALL KEYSPACES`.
    DataRoot,
    Keyspace(String),
    /// `ALL TABLES IN KEYSPACE ks` (Cassandra 5 only).
    AllTables(String),
    Table(String, String),
    RolesRoot,
    Role(String),
    FunctionsRoot,
    FunctionsIn(String),
    /// A function with its argument types as marshal class names (`Int32Type`, as Cassandra's
    /// `AbstractType.toString`) and as CQL names (for the display form).
    Function {
        keyspace: String,
        name: String,
        marshal: Vec<String>,
        cql: Vec<String>,
    },
    MbeansRoot,
    Mbean(String),
    /// ScyllaDB's `<all service levels>`.
    ServiceLevels,
}

use Permission as P;

const KEYSPACE_LEVEL: &[Permission] = &[
    P::Create,
    P::Alter,
    P::Drop,
    P::Select,
    P::Modify,
    P::Authorize,
    P::Unmask,
    P::SelectMasked,
];
const TABLE_LEVEL: &[Permission] = &[
    P::Alter,
    P::Drop,
    P::Select,
    P::Modify,
    P::Authorize,
    P::Unmask,
    P::SelectMasked,
];
const SCYLLA_KEYSPACE_LEVEL: &[Permission] = &[
    P::Create,
    P::Alter,
    P::Drop,
    P::Select,
    P::Modify,
    P::Authorize,
];
const SCYLLA_TABLE_LEVEL: &[Permission] = &[P::Alter, P::Drop, P::Select, P::Modify, P::Authorize];
const ROLES_ROOT: &[Permission] = &[P::Create, P::Alter, P::Drop, P::Authorize, P::Describe];
const ROLE_LEVEL: &[Permission] = &[P::Alter, P::Drop, P::Authorize, P::Describe];
const SCYLLA_ROLE_LEVEL: &[Permission] = &[P::Alter, P::Drop, P::Authorize];
const FUNCTION_COLLECTION: &[Permission] =
    &[P::Create, P::Alter, P::Drop, P::Authorize, P::Execute];
const FUNCTION_LEVEL: &[Permission] = &[P::Alter, P::Drop, P::Authorize, P::Execute];
const MBEAN: &[Permission] = &[P::Authorize, P::Describe, P::Execute, P::Modify, P::Select];
const SERVICE_LEVELS: &[Permission] = &[P::Create, P::Alter, P::Drop, P::Authorize, P::Describe];

impl Resource {
    /// The stored name (`IResource.getName`).
    pub fn name(&self) -> String {
        match self {
            Resource::DataRoot => "data".into(),
            Resource::Keyspace(k) => format!("data/{k}"),
            Resource::AllTables(k) => format!("data/{k}/*"),
            Resource::Table(k, t) => format!("data/{k}/{t}"),
            Resource::RolesRoot => "roles".into(),
            Resource::Role(r) => format!("roles/{r}"),
            Resource::FunctionsRoot => "functions".into(),
            Resource::FunctionsIn(k) => format!("functions/{k}"),
            Resource::Function {
                keyspace,
                name,
                marshal,
                ..
            } => format!("functions/{keyspace}/{name}[{}]", marshal.join("^")),
            Resource::MbeansRoot => "mbean".into(),
            Resource::Mbean(n) => format!("mbean/{n}"),
            Resource::ServiceLevels => "service_levels".into(),
        }
    }

    /// Parses a stored name; `None` for names this engine doesn't know (they are ignored).
    pub fn from_name(s: &str) -> Option<Resource> {
        let mut parts = s.splitn(3, '/');
        let root = parts.next()?;
        let a = parts.next();
        let b = parts.next();
        Some(match (root, a, b) {
            ("data", None, None) => Resource::DataRoot,
            ("data", Some(k), None) => Resource::Keyspace(k.into()),
            ("data", Some(k), Some("*")) => Resource::AllTables(k.into()),
            ("data", Some(k), Some(t)) => Resource::Table(k.into(), t.into()),
            ("roles", None, None) => Resource::RolesRoot,
            ("roles", Some(r), rest) => Resource::Role(match rest {
                Some(rest) => format!("{r}/{rest}"),
                None => r.into(),
            }),
            ("functions", None, None) => Resource::FunctionsRoot,
            ("functions", Some(k), None) => Resource::FunctionsIn(k.into()),
            ("functions", Some(k), Some(f)) => {
                let open = f.rfind('[')?;
                let args = f.strip_suffix(']')?.get(open + 1..)?;
                let marshal: Vec<String> = args
                    .split('^')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect();
                let cql = marshal
                    .iter()
                    .map(|m| {
                        nosql_types::parse_marshal(m)
                            .map(|(t, _)| nosql_types::cql_name(&t))
                            .unwrap_or_else(|_| m.clone())
                    })
                    .collect();
                Resource::Function {
                    keyspace: k.into(),
                    name: f[..open].into(),
                    marshal,
                    cql,
                }
            }
            ("mbean", None, None) => Resource::MbeansRoot,
            ("mbean", Some(n), rest) => Resource::Mbean(match rest {
                Some(rest) => format!("{n}/{rest}"),
                None => n.into(),
            }),
            ("service_levels", None, None) => Resource::ServiceLevels,
            _ => return None,
        })
    }

    /// [`Resource::display`] in a dialect: ScyllaDB quotes a function name that needs it
    /// (`<function ks."Odd[n]"(int)>`, probed).
    pub fn display_in(&self, d: Dialect) -> String {
        match self {
            Resource::Function {
                keyspace,
                name,
                cql,
                ..
            } if d.is_scylla() => format!(
                "<function {keyspace}.{}({})>",
                nosql_types::quote_identifier(name, d),
                cql.join(", ")
            ),
            other => other.display(),
        }
    }

    /// The form shown in messages and `LIST PERMISSIONS` (`IResource.toString`).
    pub fn display(&self) -> String {
        match self {
            Resource::DataRoot => "<all keyspaces>".into(),
            Resource::Keyspace(k) => format!("<keyspace {k}>"),
            Resource::AllTables(k) => format!("<all tables in {k}>"),
            Resource::Table(k, t) => format!("<table {k}.{t}>"),
            Resource::RolesRoot => "<all roles>".into(),
            Resource::Role(r) => format!("<role {r}>"),
            Resource::FunctionsRoot => "<all functions>".into(),
            Resource::FunctionsIn(k) => format!("<all functions in {k}>"),
            Resource::Function {
                keyspace,
                name,
                cql,
                ..
            } => format!("<function {keyspace}.{name}({})>", cql.join(", ")),
            Resource::MbeansRoot => "<all mbeans>".into(),
            Resource::Mbean(n) => format!("<mbean {n}>"),
            Resource::ServiceLevels => "<all service levels>".into(),
        }
    }

    /// The direct parent (`IResource.getParent`). ScyllaDB has no `ALL TABLES` level.
    pub fn parent(&self, d: Dialect) -> Option<Resource> {
        Some(match self {
            Resource::Keyspace(_) => Resource::DataRoot,
            Resource::AllTables(k) => Resource::Keyspace(k.clone()),
            Resource::Table(k, _) if d.is_scylla() => Resource::Keyspace(k.clone()),
            Resource::Table(k, _) => Resource::AllTables(k.clone()),
            Resource::Role(_) => Resource::RolesRoot,
            Resource::FunctionsIn(_) => Resource::FunctionsRoot,
            Resource::Function { keyspace, .. } => Resource::FunctionsIn(keyspace.clone()),
            Resource::Mbean(_) => Resource::MbeansRoot,
            _ => return None,
        })
    }

    /// The resource and its parents, nearest first (`Resources.chain`).
    pub fn chain(&self, d: Dialect) -> Vec<Resource> {
        let mut out = vec![self.clone()];
        while let Some(p) = out.last().and_then(|r| r.parent(d)) {
            out.push(p);
        }
        out
    }

    /// The permissions that can be granted on this resource (`applicablePermissions`).
    pub fn applicable(&self, d: Dialect) -> &'static [Permission] {
        let scylla = d.is_scylla();
        match self {
            Resource::DataRoot | Resource::Keyspace(_) | Resource::AllTables(_) if scylla => {
                SCYLLA_KEYSPACE_LEVEL
            }
            Resource::DataRoot | Resource::Keyspace(_) | Resource::AllTables(_) => KEYSPACE_LEVEL,
            Resource::Table(..) if scylla => SCYLLA_TABLE_LEVEL,
            Resource::Table(..) => TABLE_LEVEL,
            Resource::RolesRoot => ROLES_ROOT,
            Resource::Role(_) if scylla => SCYLLA_ROLE_LEVEL,
            Resource::Role(_) => ROLE_LEVEL,
            Resource::FunctionsRoot | Resource::FunctionsIn(_) => FUNCTION_COLLECTION,
            Resource::Function { .. } => FUNCTION_LEVEL,
            Resource::MbeansRoot | Resource::Mbean(_) => MBEAN,
            Resource::ServiceLevels => SERVICE_LEVELS,
        }
    }

    /// The keyspace of a data resource below the root.
    pub fn data_keyspace(&self) -> Option<&str> {
        match self {
            Resource::Keyspace(k) | Resource::AllTables(k) | Resource::Table(k, _) => Some(k),
            _ => None,
        }
    }

    /// ScyllaDB lists permissions grouped by resource kind: data, roles, functions.
    pub(crate) fn kind_order(&self) -> u8 {
        match self {
            Resource::DataRoot
            | Resource::Keyspace(_)
            | Resource::AllTables(_)
            | Resource::Table(..) => 0,
            Resource::RolesRoot | Resource::Role(_) => 1,
            Resource::FunctionsRoot | Resource::FunctionsIn(_) | Resource::Function { .. } => 2,
            Resource::MbeansRoot | Resource::Mbean(_) => 3,
            Resource::ServiceLevels => 4,
        }
    }
}

/// A permission's stored name.
pub fn permission_name(p: Permission) -> &'static str {
    p.as_str()
}

/// Parses a stored permission name.
pub fn permission_from_name(s: &str) -> Option<Permission> {
    Permission::ALL.iter().copied().find(|p| p.as_str() == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_display() {
        let f = Resource::Function {
            keyspace: "ks".into(),
            name: "f".into(),
            marshal: vec![
                "org.apache.cassandra.db.marshal.Int32Type".into(),
                "org.apache.cassandra.db.marshal.UTF8Type".into(),
            ],
            cql: vec!["int".into(), "text".into()],
        };
        for r in [
            Resource::DataRoot,
            Resource::Keyspace("ks".into()),
            Resource::AllTables("ks".into()),
            Resource::Table("ks".into(), "t".into()),
            Resource::RolesRoot,
            Resource::Role("r".into()),
            Resource::FunctionsRoot,
            Resource::FunctionsIn("ks".into()),
            f.clone(),
            Resource::MbeansRoot,
            Resource::Mbean("org.apache.cassandra.db:type=*".into()),
        ] {
            assert_eq!(Resource::from_name(&r.name()), Some(r.clone()), "{r:?}");
        }
        assert_eq!(f.display(), "<function ks.f(int, text)>");
        assert_eq!(
            f.name(),
            "functions/ks/f[org.apache.cassandra.db.marshal.Int32Type^org.apache.cassandra.db.marshal.UTF8Type]"
        );
        assert_eq!(
            Resource::AllTables("ks".into()).display(),
            "<all tables in ks>"
        );
        assert_eq!(Resource::from_name("nope/x"), None);
    }

    #[test]
    fn chains_per_dialect() {
        let t = Resource::Table("ks".into(), "t".into());
        let names = |d| t.chain(d).iter().map(|r| r.name()).collect::<Vec<_>>();
        assert_eq!(
            names(Dialect::Cassandra),
            ["data/ks/t", "data/ks/*", "data/ks", "data"]
        );
        assert_eq!(names(Dialect::Scylla), ["data/ks/t", "data/ks", "data"]);
        assert!(
            !Resource::Role("r".into())
                .applicable(Dialect::Scylla)
                .contains(&Permission::Describe)
        );
        assert!(
            Resource::Role("r".into())
                .applicable(Dialect::Cassandra)
                .contains(&Permission::Describe)
        );
    }
}
