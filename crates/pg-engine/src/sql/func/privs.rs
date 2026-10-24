//! Privilege inquiry functions (has_table_privilege & co., pg_has_role) and aclitem support
//! (acldefault, aclexplode, makeaclitem, aclcontains).

use super::{FnCtx, Registry, SrfIter};
use crate::catalog::Catalog;
use crate::catalog::acl::{self, AclItem, ObjKind};
use crate::error::{Error, Result, code};
use crate::types::{DataType, Value};
use std::sync::Arc;

fn text(s: impl AsRef<str>) -> Value {
    Value::Text(Arc::from(s.as_ref()))
}

fn str_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        v => v.as_str().map(str::to_string).unwrap_or_default(),
    }
}

fn is_oid(v: &Value) -> bool {
    !matches!(v, Value::Text(_))
}

/// The role a function argument names (by name, or by oid as is).
fn role_arg(cat: &Catalog, v: &Value) -> Result<u32> {
    if is_oid(v) {
        return Ok(v.as_i64().unwrap_or(0) as u32);
    }
    let name = str_of(v);
    if name == "public" {
        return Ok(0);
    }
    cat.role_by_name(&name).map(|r| r.oid).ok_or_else(|| {
        Error::new(
            code::UNDEFINED_OBJECT,
            format!("role \"{name}\" does not exist"),
        )
    })
}

/// The privileges a mode string asks about: (privileges, as grant options).
fn parse_modes(s: &str, allowed: &[(&str, u32)]) -> Result<(u32, u32)> {
    let mut privs = 0;
    let mut grants = 0;
    for part in s.split(',') {
        let words: Vec<String> = part
            .split_whitespace()
            .map(|w| w.to_ascii_uppercase())
            .collect();
        let phrase = words.join(" ");
        let (base, grant) = match phrase
            .strip_suffix(" WITH GRANT OPTION")
            .or_else(|| phrase.strip_suffix(" WITH ADMIN OPTION"))
        {
            Some(b) => (b.to_string(), true),
            None => (phrase.clone(), false),
        };
        let Some((_, bit)) = allowed.iter().find(|(n, _)| *n == base) else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized privilege type: \"{}\"", part.trim()),
            ));
        };
        if grant {
            grants |= bit;
        } else {
            privs |= bit;
        }
    }
    Ok((privs, grants))
}

const TABLE_MODES: &[(&str, u32)] = &[
    ("SELECT", acl::SELECT),
    ("INSERT", acl::INSERT),
    ("UPDATE", acl::UPDATE),
    ("DELETE", acl::DELETE),
    ("TRUNCATE", acl::TRUNCATE),
    ("REFERENCES", acl::REFERENCES),
    ("TRIGGER", acl::TRIGGER),
    ("MAINTAIN", acl::MAINTAIN),
    ("RULE", 0),
];
const COLUMN_MODES: &[(&str, u32)] = &[
    ("SELECT", acl::SELECT),
    ("INSERT", acl::INSERT),
    ("UPDATE", acl::UPDATE),
    ("REFERENCES", acl::REFERENCES),
];
const SEQUENCE_MODES: &[(&str, u32)] = &[
    ("USAGE", acl::USAGE),
    ("SELECT", acl::SELECT),
    ("UPDATE", acl::UPDATE),
];
const SCHEMA_MODES: &[(&str, u32)] = &[("CREATE", acl::CREATE), ("USAGE", acl::USAGE)];
const DATABASE_MODES: &[(&str, u32)] = &[
    ("CREATE", acl::CREATE),
    ("CONNECT", acl::CONNECT),
    ("TEMPORARY", acl::TEMPORARY),
    ("TEMP", acl::TEMPORARY),
];
const EXECUTE_MODES: &[(&str, u32)] = &[("EXECUTE", acl::EXECUTE)];
const USAGE_MODES: &[(&str, u32)] = &[("USAGE", acl::USAGE)];
const TABLESPACE_MODES: &[(&str, u32)] = &[("CREATE", acl::CREATE)];
const LARGEOBJECT_MODES: &[(&str, u32)] = &[("SELECT", acl::SELECT), ("UPDATE", acl::UPDATE)];
const PARAMETER_MODES: &[(&str, u32)] = &[("SET", acl::SET), ("ALTER SYSTEM", acl::ALTER_SYSTEM)];

/// Whether (privs, goptions) held covers any of the asked (privs, as grant options).
fn any_of(held: (u32, u32), want: (u32, u32)) -> bool {
    held.0 & want.0 != 0 || held.1 & want.1 != 0
}

/// Split (user?, object, mode) arguments: the user defaults to current_user.
fn split_args<'v>(a: &'v [Value], c: &FnCtx, n: usize) -> Result<(u32, &'v [Value])> {
    if a.len() > n {
        let cat = c.exec.catalog();
        Ok((role_arg(&cat, &a[0])?, &a[1..]))
    } else {
        Ok((c.exec.current_user(), a))
    }
}

fn rel_arg(cat: &Catalog, c: &FnCtx, v: &Value) -> Result<Option<u32>> {
    if is_oid(v) {
        let oid = v.as_i64().unwrap_or(0) as u32;
        return Ok(cat.rel_owner(oid).map(|_| oid));
    }
    let names = c
        .exec
        .fmt()
        .names
        .clone()
        .ok_or_else(|| crate::error::internal("no name resolver"))?;
    Ok(Some(names.rel_oid(&str_of(v))?))
}

/// row_security_active(table): whether row-level security applies to the current user.
fn row_security_active(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let Some(oid) = rel_arg(&cat, c, &a[0])? else {
        return Ok(Value::Null);
    };
    let Some(t) = cat.tables.get(&oid).cloned() else {
        return Ok(Value::Bool(false));
    };
    // check_enable_rls(noError): row_security = off still counts as active.
    Ok(Value::Bool(
        crate::sql::binder::Binder::new(c.exec, Vec::new())
            .rls_enabled(&t)
            .unwrap_or(true),
    ))
}

fn has_table_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let Some(oid) = rel_arg(&cat, c, &a[0])? else {
        return Ok(Value::Null);
    };
    let want = parse_modes(&str_of(&a[1]), TABLE_MODES)?;
    let held = cat.rel_mask(oid, role, c.exec.db_owner()).unwrap_or((0, 0));
    Ok(Value::Bool(any_of(held, want)))
}

fn has_sequence_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let Some(oid) = rel_arg(&cat, c, &a[0])? else {
        return Ok(Value::Null);
    };
    if !cat.sequences.contains_key(&oid) {
        let name = if is_oid(&a[0]) {
            oid.to_string()
        } else {
            str_of(&a[0])
        };
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is not a sequence"),
        ));
    }
    let want = parse_modes(&str_of(&a[1]), SEQUENCE_MODES)?;
    let held = cat.rel_mask(oid, role, c.exec.db_owner()).unwrap_or((0, 0));
    Ok(Value::Bool(any_of(held, want)))
}

/// A column argument (name or attnum) → attnum; None if a numbered column doesn't exist.
fn column_arg(cat: &Catalog, rel: u32, v: &Value) -> Result<Option<i32>> {
    let sys = crate::catalog::system::SysTable::by_oid(rel);
    let cols: Vec<(String, bool)> = if let Some(t) = cat.tables.get(&rel) {
        t.columns
            .iter()
            .map(|c| (c.name.clone(), c.dropped))
            .collect()
    } else if let Some(v) = cat.views.get(&rel) {
        v.columns.iter().map(|c| (c.0.clone(), false)).collect()
    } else if let Some(st) = sys {
        st.columns().iter().map(|c| (c.0.clone(), false)).collect()
    } else {
        Vec::new()
    };
    if is_oid(v) {
        let n = v.as_i64().unwrap_or(0) as i32;
        return Ok(
            if n >= 1 && (n as usize) <= cols.len() && !cols[n as usize - 1].1 {
                Some(n)
            } else {
                None
            },
        );
    }
    let name = str_of(v);
    if let Some(i) = cols.iter().position(|(c, d)| *c == name && !d) {
        return Ok(Some(i as i32 + 1));
    }
    // A dropped column (by the name it has once dropped): NULL.
    if cols.iter().enumerate().any(|(i, (_, d))| {
        *d && name == format!("........pg.dropped.{}........", i + 1)
    }) {
        return Ok(None);
    }
    // A table's system columns.
    if cat.tables.contains_key(&rel)
        && let Some(i) = crate::exec::scan::SYS_COLS
            .iter()
            .position(|(n, _)| *n == name)
    {
        return Ok(Some(-(i as i32 + 1)));
    }
    let rel_name = cat
        .tables
        .get(&rel)
        .map(|t| t.name.clone())
        .or_else(|| cat.views.get(&rel).map(|v| v.name.clone()))
        .or_else(|| sys.map(|s| s.name().to_string()));
    match rel_name {
        Some(rel_name) => Err(Error::new(
            code::UNDEFINED_COLUMN,
            format!("column \"{name}\" of relation \"{rel_name}\" does not exist"),
        )),
        None => Ok(None),
    }
}

fn has_column_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 3)?;
    let cat = c.exec.catalog();
    let Some(oid) = rel_arg(&cat, c, &a[0])? else {
        return Ok(Value::Null);
    };
    let Some(attnum) = column_arg(&cat, oid, &a[1])? else {
        return Ok(Value::Null);
    };
    let want = parse_modes(&str_of(&a[2]), COLUMN_MODES)?;
    let db_owner = c.exec.db_owner();
    let held = cat.rel_mask(oid, role, db_owner).unwrap_or((0, 0));
    if any_of(held, want) {
        return Ok(Value::Bool(true));
    }
    Ok(Value::Bool(any_of(
        cat.col_mask(oid, attnum, role, db_owner),
        want,
    )))
}

fn has_any_column_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let Some(oid) = rel_arg(&cat, c, &a[0])? else {
        return Ok(Value::Null);
    };
    let want = parse_modes(&str_of(&a[1]), COLUMN_MODES)?;
    let db_owner = c.exec.db_owner();
    if any_of(cat.rel_mask(oid, role, db_owner).unwrap_or((0, 0)), want) {
        return Ok(Value::Bool(true));
    }
    let any = cat
        .acls
        .iter()
        .filter(|e| e.class == 1259 && e.oid == oid && e.sub > 0)
        .any(|e| any_of(cat.col_mask(oid, e.sub, role, db_owner), want));
    Ok(Value::Bool(any))
}

fn has_largeobject_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let want = parse_modes(&str_of(&a[1]), LARGEOBJECT_MODES)?;
    let cat = c.exec.catalog();
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    let Some(m) = crate::largeobj::meta(&c.exec.txn.borrow(), oid, crate::largeobj::Snap::Latest)?
    else {
        return Ok(Value::Null);
    };
    // has_lo_priv_byid: lo_compat_privileges grants everything.
    if c.exec.sess.settings.borrow().get_raw("lo_compat_privileges").as_deref() == Some("on") {
        return Ok(Value::Bool(true));
    }
    Ok(Value::Bool(any_of(
        crate::largeobj::mask(&cat, &m, role, c.exec.db_owner()),
        want,
    )))
}

fn has_schema_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let oid = if is_oid(&a[0]) {
        let oid = a[0].as_i64().unwrap_or(0) as u32;
        if !cat.schemas.contains_key(&oid) {
            return Ok(Value::Null);
        }
        oid
    } else {
        let name = str_of(&a[0]);
        cat.schema_by_name(&name).map(|s| s.oid).ok_or_else(|| {
            Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{name}\" does not exist"),
            )
        })?
    };
    let want = parse_modes(&str_of(&a[1]), SCHEMA_MODES)?;
    Ok(Value::Bool(any_of(
        cat.obj_mask(ObjKind::Schema, oid, role, c.exec.db_owner()),
        want,
    )))
}

fn has_database_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let dbs = c.exec.sess.cluster.databases();
    let db = if is_oid(&a[0]) {
        let oid = a[0].as_i64().unwrap_or(0) as u32;
        match dbs.iter().find(|d| d.0 == oid) {
            Some(d) => d.clone(),
            None => return Ok(Value::Null),
        }
    } else {
        let name = str_of(&a[0]);
        dbs.iter().find(|d| d.1 == name).cloned().ok_or_else(|| {
            Error::new(
                code::UNDEFINED_DATABASE,
                format!("database \"{name}\" does not exist"),
            )
        })?
    };
    let want = parse_modes(&str_of(&a[1]), DATABASE_MODES)?;
    let owner = c
        .exec
        .sess
        .cluster
        .database_owner(&db.1)
        .and_then(|n| cat.role_by_name(&n).map(|r| r.oid))
        .unwrap_or(db.2);
    Ok(Value::Bool(any_of(
        cat.obj_mask(ObjKind::Database, db.0, role, owner),
        want,
    )))
}

/// A function by oid or by `name(argtypes)` text (regprocedure input).
fn function_arg(c: &FnCtx, v: &Value) -> Result<Option<u32>> {
    if is_oid(v) {
        return Ok(Some(v.as_i64().unwrap_or(0) as u32));
    }
    let names = c
        .exec
        .fmt()
        .names
        .clone()
        .ok_or_else(|| crate::error::internal("no name resolver"))?;
    names.procedure_oid(&str_of(v)).map(Some)
}

fn has_function_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let Some(oid) = function_arg(c, &a[0])? else {
        return Ok(Value::Null);
    };
    let want = parse_modes(&str_of(&a[1]), EXECUTE_MODES)?;
    Ok(Value::Bool(any_of(
        cat.obj_mask(ObjKind::Function, oid, role, c.exec.db_owner()),
        want,
    )))
}

fn has_type_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let mut oid = if is_oid(&a[0]) {
        a[0].as_i64().unwrap_or(0) as u32
    } else {
        let names = c
            .exec
            .fmt()
            .names
            .clone()
            .ok_or_else(|| crate::error::internal("no name resolver"))?;
        names.type_oid(&str_of(&a[0]))?
    };
    // Arrays: their element type's privileges.
    if let Some((_, _, true)) = cat.user_type_name(oid) {
        oid -= 1;
    }
    let want = parse_modes(&str_of(&a[1]), USAGE_MODES)?;
    Ok(Value::Bool(any_of(
        cat.obj_mask(ObjKind::Type, oid, role, c.exec.db_owner()),
        want,
    )))
}

fn has_language_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let oid = if is_oid(&a[0]) {
        a[0].as_i64().unwrap_or(0) as u32
    } else {
        let name = str_of(&a[0]);
        match cat.language_by_name(&name) {
            Some(l) => l.oid,
            None => {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("language \"{name}\" does not exist"),
                ));
            }
        }
    };
    let want = parse_modes(&str_of(&a[1]), USAGE_MODES)?;
    if matches!(oid, 12 | 13) && !cat.is_superuser(role) {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(any_of(
        cat.obj_mask(ObjKind::Language, oid, role, c.exec.db_owner()),
        want,
    )))
}

fn has_tablespace_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    if !is_oid(&a[0]) {
        let name = str_of(&a[0]);
        if name != "pg_default" && name != "pg_global" {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("tablespace \"{name}\" does not exist"),
            ));
        }
    }
    parse_modes(&str_of(&a[1]), TABLESPACE_MODES)?;
    // pg_default: CREATE is implied for everyone; pg_global only for superusers.
    let global = str_of(&a[0]) == "pg_global" || a[0].as_i64() == Some(1664);
    Ok(Value::Bool(cat.is_superuser(role) || !global))
}

fn has_parameter_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let want = parse_modes(&str_of(&a[1]), PARAMETER_MODES)?;
    // What pg_parameter_acl grants (superusers hold everything).
    let name = str_of(&a[0]).to_ascii_lowercase();
    let held = match cat.parameters.iter().find(|p| p.1 == name) {
        Some(p) => cat.obj_mask(ObjKind::Parameter, p.0, role, c.exec.db_owner()),
        None if cat.is_superuser(role) => {
            let all = acl::SET | acl::ALTER_SYSTEM;
            (all, all)
        }
        None => (0, 0),
    };
    Ok(Value::Bool(any_of(held, want)))
}

fn has_fdw_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    fdw_privilege(a, c, false)
}

fn has_server_privilege(a: &[Value], c: &FnCtx) -> Result<Value> {
    fdw_privilege(a, c, true)
}

/// has_foreign_data_wrapper_privilege / has_server_privilege.
fn fdw_privilege(a: &[Value], c: &FnCtx, server: bool) -> Result<Value> {
    let (role, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let oid = if is_oid(&a[0]) {
        let oid = a[0].as_i64().unwrap_or(0) as u32;
        let exists = if server {
            cat.servers.contains_key(&oid)
        } else {
            cat.fdws.contains_key(&oid)
        };
        if !exists {
            return Ok(Value::Null);
        }
        oid
    } else {
        let name = str_of(&a[0]);
        let found = if server {
            cat.servers.values().find(|x| x.name == name).map(|x| x.oid)
        } else {
            cat.fdws.values().find(|x| x.name == name).map(|x| x.oid)
        };
        found.ok_or_else(|| {
            Error::new(
                code::UNDEFINED_OBJECT,
                if server {
                    format!("server \"{name}\" does not exist")
                } else {
                    format!("foreign-data wrapper \"{name}\" does not exist")
                },
            )
        })?
    };
    let want = parse_modes(&str_of(&a[1]), USAGE_MODES)?;
    let kind = if server {
        ObjKind::ForeignServer
    } else {
        ObjKind::ForeignDataWrapper
    };
    Ok(Value::Bool(any_of(
        cat.obj_mask(kind, oid, role, c.exec.db_owner()),
        want,
    )))
}

fn pg_has_role(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (member, a) = split_args(a, c, 2)?;
    let cat = c.exec.catalog();
    let role = role_arg(&cat, &a[0])?;
    let modes: &[(&str, u32)] = &[("MEMBER", 1), ("USAGE", 2), ("SET", 4)];
    let (want, admin) = parse_modes(&str_of(&a[1]), modes)?;
    let db_owner = c.exec.db_owner();
    if admin != 0 {
        let ok = cat.is_superuser(member)
            || cat.roles_of(member, true, db_owner).into_iter().any(|r| {
                cat.members
                    .iter()
                    .any(|m| m.role == role && m.member == r && m.admin)
            });
        return Ok(Value::Bool(ok));
    }
    let ok = (want & 1 != 0 && cat.is_member_of_role(member, role, db_owner))
        || (want & 2 != 0 && cat.has_privs_of_role(member, role, db_owner))
        || (want & 4 != 0 && cat.can_set_role(member, role));
    Ok(Value::Bool(ok))
}

// ── aclitem ──

fn names_fn(cat: &Catalog) -> impl Fn(u32) -> Option<String> + '_ {
    move |oid| cat.roles.get(&oid).map(|r| r.name.clone())
}

/// aclitemin, validated and normalised.
pub fn aclitem_in(s: &str, cat: &Catalog) -> Result<String> {
    let (item, _) = acl::parse_item(s, &|n| cat.role_by_name(n).map(|r| r.oid))?;
    Ok(acl::format_item(&item, &names_fn(cat)))
}

fn parse_aclitem(v: &Value, cat: &Catalog) -> Result<AclItem> {
    Ok(acl::parse_item(&str_of(v), &|n| {
        cat.role_by_name(n)
            .map(|r| r.oid)
            .or_else(|| n.parse().ok())
    })?
    .0)
}

fn acldefault(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let kind = str_of(&a[0])
        .chars()
        .next()
        .and_then(ObjKind::from_letter)
        .ok_or_else(|| {
            Error::new(
                code::INTERNAL_ERROR,
                format!("unrecognized object type abbreviation: {}", str_of(&a[0])),
            )
        })?;
    let owner = a[1].as_i64().unwrap_or(0) as u32;
    let items = acl::format_acl(&acl::default_acl(kind, owner), &names_fn(&cat));
    Ok(Value::Array(Arc::new(
        crate::types::array::ArrayValue::from_vec(
            DataType::Aclitem,
            items.into_iter().map(text).collect(),
        ),
    )))
}

/// pg_get_acl(classid, objid, objsubid): the object's stored ACL (NULL: none, or no such
/// object).
fn pg_get_acl(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let class = a[0].as_i64().unwrap_or(0) as u32;
    let oid = a[1].as_i64().unwrap_or(0) as u32;
    let sub = a[2].as_i64().unwrap_or(0) as i32;
    let Some(stored) = cat
        .acls
        .iter()
        .find(|x| x.class == class && x.oid == oid && x.sub == sub)
    else {
        return Ok(Value::Null);
    };
    let items = acl::format_acl(&stored.acl, &names_fn(&cat));
    Ok(Value::Array(Arc::new(
        crate::types::array::ArrayValue::from_vec(
            DataType::Aclitem,
            items.into_iter().map(text).collect(),
        ),
    )))
}

fn aclexplode(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let cat = c.exec.catalog();
    let mut rows = Vec::new();
    if let Value::Array(arr) = &a[0] {
        for v in &arr.items {
            if v.is_null() {
                continue;
            }
            let item = parse_aclitem(v, &cat)?;
            for (bit, name) in acl::NAMES {
                if item.privs & bit != 0 {
                    rows.push(Ok(vec![
                        Value::Oid(item.grantor),
                        Value::Oid(item.grantee),
                        text(name),
                        Value::Bool(item.goptions & bit != 0),
                    ]));
                }
            }
        }
    }
    Ok(Box::new(rows.into_iter()))
}

fn makeaclitem(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let grantee = a[0].as_i64().unwrap_or(0) as u32;
    let grantor = a[1].as_i64().unwrap_or(0) as u32;
    let all: Vec<(&str, u32)> = acl::NAMES.iter().map(|(b, n)| (*n, *b)).collect();
    let (privs, _) = parse_modes(&str_of(&a[2]), &all)?;
    let grant = a[3].as_bool().unwrap_or(false);
    let item = AclItem {
        grantee,
        grantor,
        privs,
        goptions: if grant { privs } else { 0 },
    };
    Ok(text(acl::format_item(&item, &names_fn(&cat))))
}

fn aclcontains(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let want = parse_aclitem(&a[1], &cat)?;
    if let Value::Array(arr) = &a[0] {
        for v in arr.items.iter().filter(|v| !v.is_null()) {
            let i = parse_aclitem(v, &cat)?;
            if i.grantee == want.grantee
                && i.grantor == want.grantor
                && i.privs & want.privs == want.privs
                && i.goptions & want.goptions == want.goptions
            {
                return Ok(Value::Bool(true));
            }
        }
    }
    Ok(Value::Bool(false))
}

fn aclitem_eq(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    Ok(Value::Bool(
        parse_aclitem(&a[0], &cat)? == parse_aclitem(&a[1], &cat)?,
    ))
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    let two_three: &[(&'static str, super::FnImpl)] = &[
        ("has_table_privilege", has_table_privilege),
        ("has_sequence_privilege", has_sequence_privilege),
        ("has_any_column_privilege", has_any_column_privilege),
        ("has_schema_privilege", has_schema_privilege),
        ("has_database_privilege", has_database_privilege),
        ("has_function_privilege", has_function_privilege),
        ("has_type_privilege", has_type_privilege),
        ("has_language_privilege", has_language_privilege),
        ("has_tablespace_privilege", has_tablespace_privilege),
        ("has_foreign_data_wrapper_privilege", has_fdw_privilege),
        ("has_server_privilege", has_server_privilege),
    ];
    for (n, f) in two_three {
        r.func(n, vec![Text, Text], Bool, *f).stable();
        r.func(n, vec![Oid, Text], Bool, *f).stable();
        r.func(n, vec![Name, Text, Text], Bool, *f).stable();
        r.func(n, vec![Name, Oid, Text], Bool, *f).stable();
        r.func(n, vec![Oid, Text, Text], Bool, *f).stable();
        r.func(n, vec![Oid, Oid, Text], Bool, *f).stable();
    }
    r.func(
        "has_largeobject_privilege",
        vec![Oid, Text],
        Bool,
        has_largeobject_privilege,
    )
    .stable();
    r.func(
        "has_largeobject_privilege",
        vec![Name, Oid, Text],
        Bool,
        has_largeobject_privilege,
    )
    .stable();
    r.func(
        "has_largeobject_privilege",
        vec![Oid, Oid, Text],
        Bool,
        has_largeobject_privilege,
    )
    .stable();
    r.func(
        "has_parameter_privilege",
        vec![Text, Text],
        Bool,
        has_parameter_privilege,
    )
    .stable();
    r.func(
        "has_parameter_privilege",
        vec![Name, Text, Text],
        Bool,
        has_parameter_privilege,
    )
    .stable();
    r.func(
        "has_parameter_privilege",
        vec![Oid, Text, Text],
        Bool,
        has_parameter_privilege,
    )
    .stable();
    let n = "has_column_privilege";
    for (t, col) in [(Text, Text), (Text, Int2), (Oid, Text), (Oid, Int2)] {
        r.func(
            n,
            vec![t.clone(), col.clone(), Text],
            Bool,
            has_column_privilege,
        )
        .stable();
        r.func(
            n,
            vec![Name, t.clone(), col.clone(), Text],
            Bool,
            has_column_privilege,
        )
        .stable();
        r.func(
            n,
            vec![Oid, t.clone(), col.clone(), Text],
            Bool,
            has_column_privilege,
        )
        .stable();
    }
    r.func("row_security_active", vec![Text], Bool, row_security_active)
        .stable();
    r.func("row_security_active", vec![Oid], Bool, row_security_active)
        .stable();
    r.func("pg_has_role", vec![Name, Text], Bool, pg_has_role)
        .stable();
    r.func("pg_has_role", vec![Oid, Text], Bool, pg_has_role)
        .stable();
    r.func("pg_has_role", vec![Name, Name, Text], Bool, pg_has_role)
        .stable();
    r.func("pg_has_role", vec![Name, Oid, Text], Bool, pg_has_role)
        .stable();
    r.func("pg_has_role", vec![Oid, Name, Text], Bool, pg_has_role)
        .stable();
    r.func("pg_has_role", vec![Oid, Oid, Text], Bool, pg_has_role)
        .stable();
    r.func(
        "acldefault",
        vec![Char, Oid],
        DataType::array_of(Aclitem),
        acldefault,
    );
    r.func(
        "pg_get_acl",
        vec![Oid, Oid, Int4],
        DataType::array_of(Aclitem),
        pg_get_acl,
    )
    .stable();
    r.srf_cols(
        "aclexplode",
        vec![DataType::array_of(Aclitem)],
        vec![
            ("grantor", Oid),
            ("grantee", Oid),
            ("privilege_type", Text),
            ("is_grantable", Bool),
        ],
        aclexplode,
    );
    r.func(
        "makeaclitem",
        vec![Oid, Oid, Text, Bool],
        Aclitem,
        makeaclitem,
    )
    .stable();
    r.func(
        "aclcontains",
        vec![DataType::array_of(Aclitem), Aclitem],
        Bool,
        aclcontains,
    )
    .stable();
    r.binop("=", Aclitem, Aclitem, Bool, aclitem_eq);
}
