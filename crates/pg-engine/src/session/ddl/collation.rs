//! CREATE / ALTER / DROP COLLATION (collationcmds.c, pg_collation.c).

use super::Ddl;
use crate::error::{Error, Result, code};
use crate::session::{notice, warning};
use crate::sql::binder::ast::{self, inner, names, with_pos};
use crate::types::collation::{self as coll, CollationDef, icu};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const UTF8: i32 = 6;

/// `icu_validation_level` as an elevel: None for messages nobody sees, Some(true) for errors.
enum Level {
    Quiet,
    Notice,
    Warning,
    Error,
}

impl Ddl<'_, '_> {
    fn icu_level(&self) -> Level {
        match self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("icu_validation_level")
            .as_deref()
        {
            Some("error") => Level::Error,
            Some("warning") => Level::Warning,
            Some("notice") => Level::Notice,
            _ => Level::Quiet,
        }
    }

    /// Report `e` at `icu_validation_level`.
    fn icu_report(&self, e: Error) -> Result<()> {
        match self.icu_level() {
            Level::Error => Err(e),
            Level::Warning => {
                let mut w = warning(e.code, e.message.clone());
                w.hint = e.hint.clone();
                self.ctx.notice(w);
                Ok(())
            }
            Level::Notice => {
                let mut n = notice(e.message.clone());
                n.code = e.code;
                n.hint = e.hint.clone();
                self.ctx.notice(n);
                Ok(())
            }
            Level::Quiet => Ok(()),
        }
    }

    /// `icu_validate_locale`.
    fn icu_validate(&self, loc: &str) -> Result<()> {
        if matches!(self.icu_level(), Level::Quiet) {
            return Ok(());
        }
        let lang = match icu::language(loc) {
            Ok(l) => l,
            Err(e) => {
                return self.icu_report(
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("could not get language from ICU locale \"{loc}\": {e}"),
                    )
                    .hint(
                        "To disable ICU locale validation, set the parameter \"icu_validation_level\" to \"disabled\".",
                    ),
                );
            }
        };
        if !icu::known_language(&lang) {
            self.icu_report(
                Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("ICU locale \"{loc}\" has unknown language \"{lang}\""),
                )
                .hint(
                    "To disable ICU locale validation, set the parameter \"icu_validation_level\" to \"disabled\".",
                ),
            )?;
        }
        icu::open(loc, None)?;
        Ok(())
    }

    /// A collation an ALTER / DROP / COMMENT names (None: missing, with `missing_ok`).
    pub(super) fn collation_object(
        &self,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<CollationDef>> {
        if parts.len() > 1 {
            let schema = &parts[parts.len() - 2];
            if self.cat.schema_by_name(schema).is_none() {
                if missing_ok {
                    return Ok(None);
                }
                return Err(Error::new(
                    code::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                ));
            }
        }
        match coll::resolve(&self.cat, self.search_path(), parts) {
            Some(d) => Ok(Some(d)),
            None if missing_ok => Ok(None),
            None => Err(coll::no_such_collation(parts)),
        }
    }

    /// `IsThereCollationInNamespace`.
    fn collation_name_free(&self, name: &str, ns: u32) -> Result<()> {
        let taken = |enc: i32| {
            self.cat
                .collations
                .values()
                .any(|d| d.namespace == ns && d.name == name && d.encoding == enc)
                || ns == crate::catalog::PG_CATALOG_OID
                    && coll::static_by_name(name).is_some_and(|d| d.encoding == enc)
        };
        let schema = self.cat.schema_name(ns).to_string();
        if taken(UTF8) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "collation \"{name}\" for encoding \"UTF8\" already exists in schema \"{schema}\""
                ),
            ));
        }
        if taken(-1) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("collation \"{name}\" already exists in schema \"{schema}\""),
            ));
        }
        Ok(())
    }

    /// `DefineCollation`.
    pub(super) fn create_collation(&mut self, s: &pb::DefineStmt) -> Result<String> {
        const TAG: &str = "CREATE COLLATION";
        let parts = names(&s.defnames);
        let (ns, name) = match parts.as_slice() {
            [.., sc, n] => (self.cat.schema_oid(sc)?, n.clone()),
            [n] => (self.creation_schema()?, n.clone()),
            [] => return Err(crate::error::internal("collation name")),
        };
        self.require_schema_create(ns)?;
        let mut els: [Option<&pb::DefElem>; 8] = [None; 8];
        const KEYS: [&str; 8] = [
            "from",
            "locale",
            "lc_collate",
            "lc_ctype",
            "provider",
            "deterministic",
            "rules",
            "version",
        ];
        for d in super::def_elems(&s.definition) {
            let Some(i) = KEYS.iter().position(|k| *k == d.defname) else {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!("collation attribute \"{}\" not recognized", d.defname),
                    ),
                    d.location,
                ));
            };
            if els[i].is_some() {
                return Err(with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            els[i] = Some(d);
        }
        let [from, locale, lc_collate, lc_ctype, provider, deterministic, rules, version] = els;
        if locale.is_some() && (lc_collate.is_some() || lc_ctype.is_some()) {
            return Err(
                Error::new(code::SYNTAX_ERROR, "conflicting or redundant options")
                    .detail("LOCALE cannot be specified together with LC_COLLATE or LC_CTYPE."),
            );
        }
        if from.is_some() && s.definition.len() != 1 {
            return Err(
                Error::new(code::SYNTAX_ERROR, "conflicting or redundant options")
                    .detail("FROM cannot be specified together with any other options."),
            );
        }
        let str_of = |d: &pb::DefElem| ast::def_arg_str(d).unwrap_or_default();
        let mut def = if let Some(f) = from {
            let src = match f.arg.as_deref().and_then(inner) {
                Some(NodeEnum::List(l)) => names(&l.items),
                _ => vec![str_of(f)],
            };
            let src = coll::resolve(&self.cat, self.search_path(), &src)
                .ok_or_else(|| coll::no_such_collation(&src))?;
            if src.provider == 'd' {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "collation \"default\" cannot be copied",
                ));
            }
            CollationDef {
                version: None,
                ..src
            }
        } else {
            let provider = match provider.map(str_of) {
                None => 'c',
                Some(p) => match p.to_ascii_lowercase().as_str() {
                    "builtin" => 'b',
                    "icu" => 'i',
                    "libc" => 'c',
                    _ => {
                        return Err(Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            format!("unrecognized collation provider: {p}"),
                        ));
                    }
                },
            };
            let deterministic = match deterministic {
                None => true,
                Some(d) => match ast::def_arg_str(d).map(|v| v.to_ascii_lowercase()).as_deref() {
                    None => true,
                    Some("true" | "on" | "1" | "yes" | "t" | "y") => true,
                    Some("false" | "off" | "0" | "no" | "f" | "n") => false,
                    _ => {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "deterministic requires a Boolean value",
                        ));
                    }
                },
            };
            let (mut collate, mut ctype, mut colllocale) = (None, None, None);
            if let Some(l) = locale {
                if provider == 'c' {
                    collate = Some(str_of(l));
                    ctype = Some(str_of(l));
                } else {
                    colllocale = Some(str_of(l));
                }
            }
            if let Some(l) = lc_collate {
                collate = Some(str_of(l));
            }
            if let Some(l) = lc_ctype {
                ctype = Some(str_of(l));
            }
            let must = |p: &str| {
                Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("parameter \"{p}\" must be specified"),
                )
            };
            match provider {
                'b' => {
                    let l = colllocale.take().ok_or_else(|| must("locale"))?;
                    colllocale = Some(
                        coll::builtin_canonical(&l)
                            .ok_or_else(|| {
                                Error::new(
                                    code::WRONG_OBJECT_TYPE,
                                    format!("invalid locale name \"{l}\" for builtin provider"),
                                )
                            })?
                            .to_string(),
                    );
                    // Only the locale counts for this provider.
                    collate = None;
                    ctype = None;
                }
                'c' => {
                    if collate.is_none() {
                        return Err(must("lc_collate"));
                    }
                    if ctype.is_none() {
                        return Err(must("lc_ctype"));
                    }
                    // check_encoding_locale_matches: an empty name is the environment's
                    // locale, the database's LC_CTYPE; when that is C, musl's codeset for it
                    // ("ASCII") names no encoding.
                    if coll::db_locale(&self.cat).ctype_is_c() {
                        for l in [&ctype, &collate].into_iter().flatten().filter(|l| l.is_empty()) {
                            self.ctx.notice(warning(
                                code::WARNING,
                                format!("could not determine encoding for locale \"{l}\": codeset is \"ASCII\""),
                            ));
                        }
                    }
                }
                _ => {
                    let l = colllocale.take().ok_or_else(|| must("locale"))?;
                    collate = None;
                    ctype = None;
                    let l = match icu::language_tag(&l) {
                        Ok(tag) => {
                            if tag != l {
                                self.notice(format!(
                                    "using standard form \"{tag}\" for ICU locale \"{l}\""
                                ));
                            }
                            tag
                        }
                        Err(e) => {
                            if !matches!(self.icu_level(), Level::Quiet) {
                                self.icu_report(Error::new(
                                    code::INVALID_PARAMETER_VALUE,
                                    format!(
                                        "could not convert locale name \"{l}\" to language tag: {e}"
                                    ),
                                ))?;
                            }
                            l
                        }
                    };
                    self.icu_validate(&l)?;
                    colllocale = Some(l);
                }
            }
            if !deterministic && provider != 'i' {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "nondeterministic collations not supported with this provider",
                ));
            }
            let icu_rules = rules.map(str_of);
            if icu_rules.is_some() && provider != 'i' {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "ICU rules cannot be specified unless locale provider is ICU",
                ));
            }
            let encoding = match provider {
                'b' if colllocale.as_deref() == Some("C") => -1,
                'i' => -1,
                _ => UTF8,
            };
            CollationDef {
                oid: 0,
                name: String::new(),
                namespace: 0,
                owner: 0,
                provider,
                deterministic,
                encoding,
                collate,
                ctype,
                locale: colllocale,
                icu_rules,
                version: version.map(str_of),
            }
        };
        def.name = name.clone();
        def.namespace = ns;
        def.owner = self.me();
        if def.version.is_none() {
            def.version = coll::actual_version(def.provider, def.locale_str())?;
        }
        // CollationCreate: the name must be free for this encoding and for any.
        let exists = |enc: i32| {
            self.cat
                .collations
                .values()
                .any(|d| d.namespace == ns && d.name == name && d.encoding == enc)
                || ns == crate::catalog::PG_CATALOG_OID
                    && coll::static_by_name(&name).is_some_and(|d| d.encoding == enc)
        };
        let dup = if exists(def.encoding) {
            Some(def.encoding)
        } else if exists(if def.encoding == -1 { UTF8 } else { -1 }) {
            Some(-1)
        } else {
            None
        };
        if let Some(enc) = dup {
            let msg = if enc == -1 {
                format!("collation \"{name}\" already exists")
            } else {
                format!("collation \"{name}\" for encoding \"UTF8\" already exists")
            };
            if s.if_not_exists {
                self.notice_code(code::DUPLICATE_OBJECT, format!("{msg}, skipping"));
                return Ok(TAG.into());
            }
            return Err(Error::new(code::DUPLICATE_OBJECT, msg));
        }
        // pg_newlocale_from_collation: it must open, and its version be what it was made
        // with.
        coll::Collation::open(&def)?;
        let actual = coll::actual_version(def.provider, def.locale_str())?;
        match (&def.version, &actual) {
            (Some(_), None) => {
                return Err(Error::new(
                    code::INTERNAL_ERROR,
                    format!("collation \"{name}\" has no actual version, but a version was recorded"),
                ));
            }
            (Some(have), Some(act)) if have != act => {
                let qualified = format!(
                    "{}.{}",
                    crate::catalog::quote_ident(self.cat.schema_name(ns)),
                    crate::catalog::quote_ident(&name)
                );
                let mut w = warning(
                    code::WARNING,
                    format!("collation \"{name}\" has version mismatch"),
                );
                w.detail = Some(format!(
                    "The collation in the database was created using version {have}, but the operating system provides version {act}."
                ));
                w.hint = Some(
                    format!(
                        "Rebuild all objects affected by this collation and run ALTER COLLATION {qualified} REFRESH VERSION, or build PostgreSQL with the right library version."
                    )
                    .into(),
                );
                self.ctx.notice(w);
            }
            _ => {}
        }
        def.oid = self.cat.alloc_oid();
        self.cat.collations.insert(def.oid, Arc::new(def));
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER COLLATION ... REFRESH VERSION.
    pub(super) fn alter_collation(&mut self, s: &pb::AlterCollationStmt) -> Result<String> {
        const TAG: &str = "ALTER COLLATION";
        let parts = names(&s.collname);
        let d = self
            .collation_object(&parts, false)?
            .ok_or_else(|| coll::no_such_collation(&parts))?;
        if d.oid == coll::DEFAULT_OID {
            return Err(
                Error::new(code::INTERNAL_ERROR, "cannot refresh version of default collation")
                    .hint("Use ALTER DATABASE ... REFRESH COLLATION VERSION instead."),
            );
        }
        self.require_owner(d.owner, "collation", &d.name)?;
        let new = coll::actual_version(d.provider, d.locale_str())?;
        match (&d.version, &new) {
            (None, Some(_)) | (Some(_), None) => {
                return Err(Error::new(code::INTERNAL_ERROR, "invalid collation version change"));
            }
            (Some(old), Some(new)) if old != new => {
                self.notice(format!("changing version from {old} to {new}"));
                if let Some(e) = self.cat.collations.get_mut(&d.oid) {
                    *e = Arc::new(CollationDef {
                        version: Some(new.clone()),
                        ..d
                    });
                    self.save()?;
                }
            }
            _ => self.notice("version has not changed"),
        }
        Ok(TAG.into())
    }

    /// ALTER COLLATION ... RENAME TO.
    pub(super) fn rename_collation(&mut self, parts: &[String], newname: &str) -> Result<()> {
        let d = self
            .collation_object(parts, false)?
            .ok_or_else(|| coll::no_such_collation(parts))?;
        self.require_owner(d.owner, "collation", &d.name)?;
        self.collation_name_free(newname, d.namespace)?;
        self.require_schema_create(d.namespace)?;
        self.put_collation(CollationDef {
            name: newname.to_string(),
            ..d
        })
    }

    /// ALTER COLLATION ... SET SCHEMA.
    pub(super) fn collation_set_schema(&mut self, parts: &[String], schema: &str) -> Result<()> {
        let d = self
            .collation_object(parts, false)?
            .ok_or_else(|| coll::no_such_collation(parts))?;
        let ns = self.cat.schema_oid(schema)?;
        self.require_owner(d.owner, "collation", &d.name)?;
        self.require_schema_create(ns)?;
        if ns != d.namespace {
            self.collation_name_free(&d.name, ns)?;
        }
        self.put_collation(CollationDef {
            namespace: ns,
            ..d
        })
    }

    /// ALTER COLLATION ... OWNER TO.
    pub(super) fn collation_owner(&mut self, parts: &[String], new: u32) -> Result<()> {
        let d = self
            .collation_object(parts, false)?
            .ok_or_else(|| coll::no_such_collation(parts))?;
        if d.owner == new {
            return Ok(());
        }
        self.require_owner(d.owner, "collation", &d.name)?;
        self.check_new_owner(new, Some(d.namespace))?;
        self.put_collation(CollationDef { owner: new, ..d })
    }

    fn put_collation(&mut self, d: CollationDef) -> Result<()> {
        if !self.cat.collations.contains_key(&d.oid) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot alter a built-in collation",
            ));
        }
        self.cat.collations.insert(d.oid, Arc::new(d));
        Ok(())
    }

    /// ALTER DATABASE ... REFRESH COLLATION VERSION (`AlterDatabaseRefreshColl`).
    pub(super) fn refresh_database_collation(&mut self, db: &str) -> Result<String> {
        let cluster = self.ctx.sess.cluster.clone();
        let Some(l) = cluster
            .databases()
            .iter()
            .any(|d| d.1 == db)
            .then(|| cluster.database_locale(db))
            .flatten()
        else {
            return Err(Error::new(
                code::UNDEFINED_DATABASE,
                format!("database \"{db}\" does not exist"),
            ));
        };
        if let Some(owner) = cluster.database_owner(db) {
            let owner = self.cat.role_by_name(&owner).map_or(0, |r| r.oid);
            self.require_owner(owner, "database", db)?;
        }
        match (&l.collversion, l.actual_version()?) {
            (None, Some(_)) | (Some(_), None) => {
                return Err(Error::new(code::INTERNAL_ERROR, "invalid collation version change"));
            }
            (Some(old), Some(new)) if *old != new => {
                self.notice(format!("changing version from {old} to {new}"));
                cluster.set_database_collversion(db, Some(new))?;
            }
            _ => self.notice("version has not changed"),
        }
        Ok("ALTER DATABASE".into())
    }
}
