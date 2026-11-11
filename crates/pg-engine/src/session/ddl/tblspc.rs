//! Tablespaces (tablespace.c): CREATE / ALTER / DROP TABLESPACE, the tablespace a new
//! relation goes to (an explicit TABLESPACE, `default_tablespace`, `temp_tablespaces`, a
//! partitioned parent's), ALTER ... SET TABLESPACE and ALTER ... ALL IN TABLESPACE. Nothing
//! is stored apart: a tablespace is a catalog row and each relation's pg_class.reltablespace.

use super::Ddl;
use crate::catalog::Tablespace;
use crate::catalog::acl::ObjKind;
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{def_arg_str, inner};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// pg_default and pg_global.
pub(super) const DEFAULT_TABLESPACE: u32 = 1663;
pub(super) const GLOBAL_TABLESPACE: u32 = 1664;

/// tablespace_reloptions: (name, integer, minimum, maximum).
const OPTIONS: &[(&str, bool, f64, f64)] = &[
    ("random_page_cost", false, 0.0, f64::MAX),
    ("seq_page_cost", false, 0.0, f64::MAX),
    ("effective_io_concurrency", true, 0.0, 1000.0),
    ("maintenance_io_concurrency", true, 0.0, 1000.0),
];

/// The options of a tablespace after SET (or RESET) `opts` over `base`
/// (transformRelOptions, then tablespace_reloptions' validation).
fn tablespace_options(base: &[String], opts: &[pb::Node], reset: bool) -> Result<Vec<String>> {
    let mut out: Vec<String> = base.to_vec();
    for d in super::def_elems(opts) {
        if reset {
            if d.arg.is_some() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "RESET must not include values for parameters",
                ));
            }
        } else if !d.defnamespace.is_empty() {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter namespace \"{}\"", d.defnamespace),
            ));
        }
        out.retain(|o| o.split_once('=').map_or(o.as_str(), |(n, _)| n) != d.defname);
        if !reset {
            let v = def_arg_str(d).unwrap_or_else(|| "true".into());
            out.push(format!("{}={v}", d.defname));
        }
    }
    for o in &out {
        let (name, value) = o.split_once('=').unwrap_or((o.as_str(), ""));
        let Some((_, int, min, max)) = OPTIONS.iter().find(|x| x.0 == name) else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{name}\""),
            ));
        };
        let v = value.trim();
        if *int {
            let Ok(n) = v.parse::<i64>() else {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid value for integer option \"{name}\": {value}"),
                ));
            };
            if (n as f64) < *min || (n as f64) > *max {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("value {value} out of bounds for option \"{name}\""),
                )
                .detail(format!(
                    "Valid values are between \"{}\" and \"{}\".",
                    *min as i64, *max as i64
                )));
            }
        } else {
            let Ok(n) = v.parse::<f64>() else {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid value for floating point option \"{name}\": {value}"),
                ));
            };
            if n < *min || n > *max {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("value {value} out of bounds for option \"{name}\""),
                )
                .detail(format!(
                    "Valid values are between \"{min:.6}\" and \"{max:.6}\"."
                )));
            }
        }
    }
    Ok(out)
}

/// The "pg_" prefix is reserved for system tablespaces.
fn reserved_name(name: &str) -> Result<()> {
    if name.starts_with("pg_") {
        return Err(Error::new(
            code::RESERVED_NAME,
            format!("unacceptable tablespace name \"{name}\""),
        )
        .detail("The prefix \"pg_\" is reserved for system tablespaces."));
    }
    Ok(())
}

fn no_such(name: &str) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!("tablespace \"{name}\" does not exist"),
    )
}

impl Ddl<'_, '_> {
    fn setting(&self, name: &str) -> String {
        self.ctx
            .sess
            .settings
            .borrow()
            .get_raw(name)
            .unwrap_or_default()
    }

    pub(super) fn tablespace_named(&self, name: &str) -> Result<u32> {
        self.cat.tablespace_oid(name).ok_or_else(|| no_such(name))
    }

    /// CREATE privilege on a tablespace (not needed for the database's default).
    pub(super) fn require_tablespace_create(&self, spc: u32) -> Result<()> {
        if spc == 0 || spc == DEFAULT_TABLESPACE {
            return Ok(());
        }
        let (p, _) = self
            .cat
            .obj_mask(ObjKind::Tablespace, spc, self.me(), self.db_owner());
        if p & crate::catalog::acl::CREATE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "permission denied for tablespace {}",
                    self.cat.tablespace_name(spc).unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }

    /// GetDefaultTablespace: `temp_tablespaces`' first usable one for a temporary relation,
    /// else `default_tablespace` (0: the database's).
    pub(super) fn default_tablespace(&self, temp: bool, partitioned: bool) -> Result<u32> {
        if temp {
            let list = self.setting("temp_tablespaces");
            for n in list.split(',').map(|n| n.trim().trim_matches('"')) {
                if n.is_empty() {
                    continue;
                }
                if let Some(o) = self.cat.tablespace_oid(n) {
                    if o == DEFAULT_TABLESPACE {
                        return Ok(0);
                    }
                    if self.require_tablespace_create(o).is_ok() {
                        return Ok(o);
                    }
                }
            }
            return Ok(0);
        }
        let name = self.setting("default_tablespace");
        if name.is_empty() {
            return Ok(0);
        }
        match self.cat.tablespace_oid(&name) {
            Some(DEFAULT_TABLESPACE) if partitioned => Err(partitioned_default()),
            Some(DEFAULT_TABLESPACE) | None => Ok(0),
            Some(o) => Ok(o),
        }
    }

    /// DefineRelation's / DefineIndex's tablespace: the one named, else (a partition) its
    /// parent's, else the default; checked for CREATE and against pg_global.
    pub(super) fn choose_tablespace(
        &self,
        explicit: &str,
        partitioned: bool,
        temp: bool,
        inherited: Option<u32>,
    ) -> Result<u32> {
        let mut spc = if !explicit.is_empty() {
            let o = self.tablespace_named(explicit)?;
            if partitioned && o == DEFAULT_TABLESPACE {
                return Err(partitioned_default());
            }
            o
        } else {
            inherited.unwrap_or(0)
        };
        if spc == 0 {
            spc = self.default_tablespace(temp, partitioned)?;
        }
        self.require_tablespace_create(spc)?;
        if spc == GLOBAL_TABLESPACE {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ));
        }
        Ok(if spc == DEFAULT_TABLESPACE { 0 } else { spc })
    }

    /// CREATE TABLESPACE.
    pub(super) fn create_tablespace(&mut self, s: &pb::CreateTableSpaceStmt) -> Result<String> {
        super::prevent_in_block("CREATE TABLESPACE")?;
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create tablespace \"{}\"", s.tablespacename),
            )
            .hint("Must be superuser to create a tablespace."));
        }
        let owner = match &s.owner {
            Some(r) => self.role_oid(r)?,
            None => self.me(),
        };
        let mut location = s.location.clone();
        while location.len() > 1 && location.ends_with('/') {
            location.pop();
        }
        if location.contains('\'') {
            return Err(Error::new(
                code::INVALID_NAME,
                "tablespace location cannot contain single quotes",
            ));
        }
        let in_place = location.is_empty() && self.setting("allow_in_place_tablespaces") == "on";
        if !in_place && !location.starts_with('/') {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "tablespace location must be an absolute path",
            ));
        }
        reserved_name(&s.tablespacename)?;
        if self.cat.tablespace_oid(&s.tablespacename).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("tablespace \"{}\" already exists", s.tablespacename),
            ));
        }
        let options = tablespace_options(&[], &s.options, false)?;
        if !in_place && !std::path::Path::new(&location).is_dir() {
            return Err(Error::new(
                code::UNDEFINED_FILE,
                format!("directory \"{location}\" does not exist"),
            ));
        }
        let oid = self.cat.alloc_oid();
        self.cat.tablespaces.insert(
            oid,
            Tablespace {
                oid,
                name: s.tablespacename.clone(),
                owner,
                options: (!options.is_empty()).then_some(options),
                location,
            },
        );
        self.save()?;
        Ok("CREATE TABLESPACE".into())
    }

    /// The relations of this database in tablespace `spc`: (oid, has storage).
    fn tablespace_users(&self, spc: u32) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        for (rel, s) in &self.cat.rel_tablespace {
            if *s != spc {
                continue;
            }
            let storage = if let Some(t) = self.cat.tables.get(rel) {
                t.partkey.is_none() && t.foreign.is_none()
            } else if let Some(ix) = self.cat.indexes.get(rel) {
                self.cat
                    .index_table(ix.table)
                    .is_none_or(|t| t.partkey.is_none())
            } else if self.cat.views.contains_key(rel) {
                true
            } else {
                continue;
            };
            out.push((*rel, storage));
        }
        out
    }

    /// DROP TABLESPACE.
    pub(super) fn drop_tablespace(&mut self, s: &pb::DropTableSpaceStmt) -> Result<String> {
        const TAG: &str = "DROP TABLESPACE";
        super::prevent_in_block("DROP TABLESPACE")?;
        let name = &s.tablespacename;
        let Some(oid) = self.cat.tablespace_oid(name) else {
            if s.missing_ok {
                self.notice(format!("tablespace \"{name}\" does not exist, skipping"));
                return Ok(TAG.into());
            }
            return Err(no_such(name));
        };
        let owner = self.cat.tablespace_owner(oid);
        if !self.cat.has_privs_of_role(self.me(), owner, self.db_owner()) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of tablespace {name}"),
            ));
        }
        if oid == DEFAULT_TABLESPACE || oid == GLOBAL_TABLESPACE {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {name}"),
            ));
        }
        // Partitioned tables and indexes keep only a catalog reference (pg_shdepend).
        let users = self.tablespace_users(oid);
        let refs: Vec<String> = users
            .iter()
            .filter(|(_, storage)| !storage)
            .map(|(rel, _)| {
                let what = crate::catalog::depend::describe(
                    &self.cat,
                    &[],
                    (crate::catalog::depend::REL, *rel, 0),
                )
                .unwrap_or_default();
                format!("tablespace for {what}")
            })
            .collect();
        if !refs.is_empty() {
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("tablespace \"{name}\" cannot be dropped because some objects depend on it"),
            )
            .detail(refs.join("\n")));
        }
        if users.iter().any(|(_, storage)| *storage) {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("tablespace \"{name}\" is not empty"),
            ));
        }
        self.cat.tablespaces.remove(&oid);
        self.cat.acls.retain(|a| !(a.class == 1213 && a.oid == oid));
        self.cat.drop_comments(oid);
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER TABLESPACE ... SET / RESET (options).
    pub(super) fn alter_tablespace_options(
        &mut self,
        s: &pb::AlterTableSpaceOptionsStmt,
    ) -> Result<String> {
        let name = &s.tablespacename;
        let oid = self.tablespace_named(name)?;
        let owner = self.cat.tablespace_owner(oid);
        if !self.cat.has_privs_of_role(self.me(), owner, self.db_owner()) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of tablespace {name}"),
            ));
        }
        let Some(t) = self.cat.tablespaces.get(&oid).cloned() else {
            // The built-in ones keep no options here.
            tablespace_options(&[], &s.options, s.is_reset)?;
            return Ok("ALTER TABLESPACE".into());
        };
        let opts = tablespace_options(t.options.as_deref().unwrap_or(&[]), &s.options, s.is_reset)?;
        self.cat.tablespaces.insert(
            oid,
            Tablespace {
                options: (!opts.is_empty()).then_some(opts),
                ..t
            },
        );
        self.save()?;
        Ok("ALTER TABLESPACE".into())
    }

    /// ALTER TABLESPACE ... RENAME TO.
    pub(super) fn rename_tablespace(&mut self, old: &str, new: &str) -> Result<String> {
        let oid = self.tablespace_named(old)?;
        let owner = self.cat.tablespace_owner(oid);
        if !self.cat.has_privs_of_role(self.me(), owner, self.db_owner()) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of tablespace {old}"),
            ));
        }
        reserved_name(new)?;
        if self.cat.tablespace_oid(new).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("tablespace \"{new}\" already exists"),
            ));
        }
        if let Some(t) = self.cat.tablespaces.get_mut(&oid) {
            t.name = new.to_string();
        }
        self.save()?;
        Ok("ALTER TABLESPACE".into())
    }

    /// ALTER TABLESPACE ... OWNER TO (the caller checked the new owner).
    pub(super) fn set_tablespace_owner(&mut self, oid: u32, new: u32) {
        let old = self.cat.tablespace_owner(oid);
        self.reown_acls(ObjKind::Tablespace, oid, old, new);
        if let Some(t) = self.cat.tablespaces.get_mut(&oid) {
            t.owner = new;
        }
    }

    /// ALTER TABLE / INDEX / MATERIALIZED VIEW ... SET TABLESPACE of relation `rel` (a
    /// partitioned table or index only records it, for its future partitions).
    pub(super) fn move_to_tablespace(&mut self, rel: u32, name: &str) -> Result<()> {
        let spc = self.tablespace_named(name)?;
        if spc == GLOBAL_TABLESPACE {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ));
        }
        self.require_tablespace_create(spc)?;
        self.cat.set_rel_tablespace(rel, spc);
        Ok(())
    }

    /// ALTER TABLE / INDEX / MATERIALIZED VIEW ALL IN TABLESPACE ... SET TABLESPACE.
    pub(super) fn move_all(&mut self, s: &pb::AlterTableMoveAllStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.objtype).unwrap_or(O::ObjectTable);
        let from = self.tablespace_named(&s.orig_tablespacename)?;
        let to = self.tablespace_named(&s.new_tablespacename)?;
        if from == GLOBAL_TABLESPACE || to == GLOBAL_TABLESPACE {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "cannot move relations in to or out of pg_global tablespace",
            ));
        }
        self.require_tablespace_create(to)?;
        let roles: Vec<u32> = s
            .roles
            .iter()
            .filter_map(|r| match inner(r) {
                Some(NodeEnum::RoleSpec(rs)) => self.role_oid(rs).ok(),
                _ => None,
            })
            .collect();
        let from_key = if from == DEFAULT_TABLESPACE { 0 } else { from };
        let mut moved = Vec::new();
        let mut rels: Vec<(u32, u32, u32)> = Vec::new();
        match kind {
            O::ObjectTable => rels.extend(
                self.cat
                    .tables
                    .values()
                    .filter(|t| t.foreign.is_none())
                    .map(|t| (t.oid, t.schema, t.owner)),
            ),
            O::ObjectIndex => {
                for ix in self.cat.indexes.values() {
                    if let Some(t) = self.cat.index_table(ix.table) {
                        rels.push((ix.oid, t.schema, t.owner));
                    }
                }
            }
            _ => rels.extend(
                self.cat
                    .views
                    .values()
                    .filter(|v| v.materialized)
                    .map(|v| (v.oid, v.schema, v.owner)),
            ),
        }
        rels.sort();
        for (rel, schema, owner) in rels {
            let sname = self.cat.schema_name(schema).to_string();
            if self.cat.rel_tablespace(rel) != from_key
                || sname == "pg_catalog"
                || sname == "information_schema"
                || sname.starts_with("pg_toast")
                || (!roles.is_empty() && !roles.contains(&owner))
            {
                continue;
            }
            moved.push(rel);
        }
        if moved.is_empty() {
            self.notice_code(
                code::NO_DATA_FOUND,
                format!(
                    "no matching relations in tablespace \"{}\" found",
                    s.orig_tablespacename
                ),
            );
        }
        for rel in moved {
            self.cat.set_rel_tablespace(rel, to);
        }
        self.save()?;
        Ok(format!(
            "ALTER {}",
            match kind {
                O::ObjectIndex => "INDEX",
                O::ObjectMatview => "MATERIALIZED VIEW",
                _ => "TABLE",
            }
        ))
    }
}

fn partitioned_default() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "cannot specify default tablespace for partitioned relations",
    )
}

/// The index REINDEX (TABLESPACE ...) of system relation `rel` (a table, TOAST table or
/// index) would move first: an index itself, else the table's own first index.
pub(super) fn system_index_name(rel: u32) -> String {
    use crate::catalog::static_rows::{oid_at, rows, text_at};
    use crate::catalog::system::SysTable;
    let class = rows(SysTable::PgClass);
    let name = |oid: u32| {
        class
            .iter()
            .find(|r| oid_at(r, 0) == oid)
            .map(|r| text_at(r, 1).to_string())
            .unwrap_or_default()
    };
    let first = rows(SysTable::PgIndex)
        .iter()
        .filter(|r| oid_at(r, 1) == rel)
        .map(|r| oid_at(r, 0))
        .min();
    name(first.unwrap_or(rel))
}

impl Ddl<'_, '_> {
    /// REINDEX (TABLESPACE name): the tablespace, checked for CREATE (but the database's).
    pub(super) fn reindex_tablespace_param(&self, params: &[pb::Node]) -> Result<Option<u32>> {
        let Some(name) = super::def_elems(params)
            .find(|d| d.defname == "tablespace")
            .and_then(def_arg_str)
        else {
            return Ok(None);
        };
        let spc = self.tablespace_named(&name)?;
        self.require_tablespace_create(spc)?;
        Ok(Some(spc))
    }

    /// REINDEX (TABLESPACE): the rebuilt indexes go to `spc` (CheckRelationTableSpaceMove).
    pub(super) fn reindex_move(&mut self, indexes: &[u32], spc: u32, concurrently: bool) -> Result<()> {
        for ix in indexes {
            let cur = self.cat.rel_tablespace(*ix);
            let same = cur == spc || (spc == DEFAULT_TABLESPACE && cur == 0);
            if spc == GLOBAL_TABLESPACE && (concurrently || !same) {
                return Err(if concurrently {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "cannot move non-shared relation to tablespace \"pg_global\"",
                    )
                } else {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "only shared relations can be placed in pg_global tablespace",
                    )
                });
            }
            self.cat.set_rel_tablespace(*ix, spc);
        }
        Ok(())
    }
}
