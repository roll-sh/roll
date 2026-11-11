//! A database's own options (dbcommands.c): `CREATE DATABASE` / `ALTER DATABASE ...
//! IS_TEMPLATE / ALLOW_CONNECTIONS / CONNECTION LIMIT`, and a superuser's `UPDATE pg_database
//! SET datistemplate / datallowconn / datconnlimit` (`catalog_dml.rs`). They are kept with
//! the cluster's list of databases (`engine.rs` `DbEntry`, pg_database's columns), checked
//! when a session connects (`Engine::connect`, `Session::login`) and when a database is
//! copied or dropped; a transaction that rolls back puts them back (`Txn::db_options_undo`).

use super::Ddl;
use crate::error::{Error, Result, code};
use crate::session::state::{DATCONNLIMIT_UNLIMITED, DbOptions};
use crate::sql::binder::ast::{inner, with_pos};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

fn redundant(d: &pb::DefElem) -> Error {
    with_pos(
        Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
        d.location,
    )
}

/// defGetBoolean.
fn def_get_boolean(d: &pb::DefElem) -> Result<bool> {
    let bad = || {
        Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )
    };
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    let s = match inner(arg) {
        Some(NodeEnum::Integer(i)) => {
            return match i.ival {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(bad()),
            };
        }
        Some(NodeEnum::Boolean(b)) => return Ok(b.boolval),
        _ => crate::sql::binder::ast::def_arg_str(d).unwrap_or_default(),
    };
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(bad()),
    }
}

/// defGetInt32.
fn def_get_int32(d: &pb::DefElem) -> Result<i32> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::Integer(i)) => Ok(i.ival),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires an integer value", d.defname),
        )),
    }
}

/// The IS_TEMPLATE / ALLOW_CONNECTIONS / CONNECTION LIMIT options of a CREATE or ALTER
/// DATABASE.
#[derive(Default)]
pub(crate) struct DbOptionElems<'a> {
    is_template: Option<&'a pb::DefElem>,
    allow_connections: Option<&'a pb::DefElem>,
    connection_limit: Option<&'a pb::DefElem>,
}

/// What the options set: (datistemplate, datallowconn, datconnlimit), None where not given.
pub(crate) type DbOptionValues = (Option<bool>, Option<bool>, Option<i32>);

impl<'a> DbOptionElems<'a> {
    /// Takes `d` when it is one of these options (false: it isn't); a repeated one is an
    /// error (errorConflictingDefElem).
    pub(crate) fn take(&mut self, d: &'a pb::DefElem) -> Result<bool> {
        let slot = match d.defname.as_str() {
            "is_template" => &mut self.is_template,
            "allow_connections" => &mut self.allow_connections,
            "connection_limit" => &mut self.connection_limit,
            _ => return Ok(false),
        };
        if slot.is_some() {
            return Err(redundant(d));
        }
        *slot = Some(d);
        Ok(true)
    }

    /// The values given (an option given as DEFAULT, without an argument, is its default),
    /// with the connection limit's check.
    pub(crate) fn values(&self) -> Result<DbOptionValues> {
        let bool_of = |d: Option<&pb::DefElem>, default: bool| -> Result<Option<bool>> {
            match d {
                Some(d) if d.arg.is_some() => def_get_boolean(d).map(Some),
                Some(_) => Ok(Some(default)),
                None => Ok(None),
            }
        };
        let is_template = bool_of(self.is_template, false)?;
        let allow = bool_of(self.allow_connections, true)?;
        let limit = match self.connection_limit {
            Some(d) if d.arg.is_some() => {
                let l = def_get_int32(d)?;
                if l < DATCONNLIMIT_UNLIMITED {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid connection limit: {l}"),
                    ));
                }
                Some(l)
            }
            Some(_) => Some(DATCONNLIMIT_UNLIMITED),
            None => None,
        };
        Ok((is_template, allow, limit))
    }
}

/// `base` with the values given.
pub(crate) fn apply_values(mut base: DbOptions, v: DbOptionValues) -> DbOptions {
    if let Some(t) = v.0 {
        base.is_template = t;
    }
    if let Some(a) = v.1 {
        base.allow_connections = a;
    }
    if let Some(l) = v.2 {
        base.conn_limit = l;
    }
    base
}

impl Ddl<'_, '_> {
    /// (oid, owner, options) of database `name`.
    pub(super) fn database_entry(&self, name: &str) -> Option<(u32, u32, DbOptions)> {
        let cluster = &self.ctx.sess.cluster;
        let (oid, _, owner) = cluster.databases().into_iter().find(|d| d.1 == name)?;
        Some((oid, owner, cluster.database_options(oid)))
    }

    /// Changes database `name`'s options, put back if the transaction rolls back.
    pub(super) fn set_database_options(
        &mut self,
        name: &str,
        old: DbOptions,
        new: DbOptions,
    ) -> Result<()> {
        if old == new {
            return Ok(());
        }
        self.ctx
            .txn
            .borrow_mut()
            .db_options_undo
            .push((name.to_string(), old));
        self.ctx.sess.cluster.set_database_options(name, new)
    }

    /// ALTER DATABASE name [WITH] option ... (AlterDatabase).
    pub(super) fn alter_database(&mut self, s: &pb::AlterDatabaseStmt) -> Result<String> {
        let mut opts = DbOptionElems::default();
        let mut tablespace = None;
        for d in super::def_elems(&s.options) {
            if opts.take(d)? {
                continue;
            }
            if d.defname == "tablespace" {
                if tablespace.is_some() {
                    return Err(redundant(d));
                }
                tablespace = Some(d);
                continue;
            }
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("option \"{}\" not recognized", d.defname),
                ),
                d.location,
            ));
        }
        if let Some(d) = tablespace {
            if s.options.len() != 1 {
                return Err(with_pos(
                    Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "option \"{}\" cannot be specified with other options",
                            d.defname
                        ),
                    ),
                    d.location,
                ));
            }
            // (movedb: a database's relations stay where they are.)
            return Ok("ALTER DATABASE".into());
        }
        let values = opts.values()?;
        let Some((oid, owner, old)) = self.database_entry(&s.dbname) else {
            return Err(Error::new(
                code::UNDEFINED_DATABASE,
                format!("database \"{}\" does not exist", s.dbname),
            ));
        };
        if old.is_invalid() {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot alter invalid database \"{}\"", s.dbname),
            )
            .hint("Use DROP DATABASE to drop invalid databases.")
            .fatal());
        }
        self.require_owner(owner, "database", &s.dbname)?;
        // (No locking oneself out.)
        if values.1 == Some(false) && oid == self.ctx.sess.db.oid {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "cannot disallow connections for current database",
            ));
        }
        self.set_database_options(&s.dbname, old, apply_values(old, values))?;
        Ok("ALTER DATABASE".into())
    }
}
