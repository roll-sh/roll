//! CREATE / ALTER / DROP TEXT SEARCH PARSER / DICTIONARY / TEMPLATE / CONFIGURATION
//! (Postgres's tsearchcmds.c), and their RENAME, SET SCHEMA and OWNER TO.

use super::Ddl;
use crate::catalog::tsearch::{TsCatalog, TsKind};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, inner, names};
use crate::types::tsearch::config::{
    DictOption, PARSER_FUNCS, TEMPLATE_FUNCS, TsConfigDef, TsDictDef, TsParserDef,
    TsTemplateDef,
};
use crate::types::tsearch::parser;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// The kind of text search object a statement's object type names.
pub(super) fn ts_kind(t: pb::ObjectType) -> Option<TsKind> {
    use pb::ObjectType as O;
    Some(match t {
        O::ObjectTsconfiguration => TsKind::Config,
        O::ObjectTsdictionary => TsKind::Dict,
        O::ObjectTsparser => TsKind::Parser,
        O::ObjectTstemplate => TsKind::Template,
        _ => return None,
    })
}

/// The built-in functions a parser or template can name: (oid, name, argument types,
/// result type). Only C functions fit, and these are the only ones there are.
const TS_FUNCS: [(u32, &str, &[&str], &str); 15] = [
    (
        PARSER_FUNCS[0].0,
        PARSER_FUNCS[0].1,
        &["internal", "integer"],
        "internal",
    ),
    (
        PARSER_FUNCS[1].0,
        PARSER_FUNCS[1].1,
        &["internal", "internal", "internal"],
        "internal",
    ),
    (PARSER_FUNCS[2].0, PARSER_FUNCS[2].1, &["internal"], "void"),
    (
        PARSER_FUNCS[3].0,
        PARSER_FUNCS[3].1,
        &["internal", "internal", "tsquery"],
        "internal",
    ),
    (
        PARSER_FUNCS[4].0,
        PARSER_FUNCS[4].1,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[0].1,
        TEMPLATE_FUNCS[0].2,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[0].3,
        TEMPLATE_FUNCS[0].4,
        &["internal", "internal", "internal", "internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[1].1,
        TEMPLATE_FUNCS[1].2,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[1].3,
        TEMPLATE_FUNCS[1].4,
        &["internal", "internal", "internal", "internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[2].1,
        TEMPLATE_FUNCS[2].2,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[2].3,
        TEMPLATE_FUNCS[2].4,
        &["internal", "internal", "internal", "internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[3].1,
        TEMPLATE_FUNCS[3].2,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[3].3,
        TEMPLATE_FUNCS[3].4,
        &["internal", "internal", "internal", "internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[4].1,
        TEMPLATE_FUNCS[4].2,
        &["internal"],
        "internal",
    ),
    (
        TEMPLATE_FUNCS[4].3,
        TEMPLATE_FUNCS[4].4,
        &["internal", "internal", "internal", "internal"],
        "internal",
    ),
];

/// `defGetQualifiedName`.
fn def_qualified(d: &pb::DefElem) -> Result<Vec<String>> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::TypeName(tn)) => Ok(names(&tn.names)),
        Some(NodeEnum::List(l)) => Ok(names(&l.items)),
        Some(NodeEnum::String(s)) => Ok(vec![s.sval.clone()]),
        None => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a parameter", d.defname),
        )),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("argument of {} must be a name", d.defname),
        )),
    }
}

/// A dictionary option as `defGetString` reads it; numbers are stored unquoted. None when
/// the option has no value (ALTER's "remove it").
fn dict_option(d: &pb::DefElem) -> Option<DictOption> {
    let arg = d.arg.as_deref()?;
    let (value, number) = match inner(arg) {
        Some(NodeEnum::Integer(i)) => (i.ival.to_string(), true),
        Some(NodeEnum::Float(f)) => (f.fval.clone(), true),
        Some(NodeEnum::TypeName(tn)) => {
            let mut s = names(&tn.names).join(".");
            if tn.pct_type {
                s.push_str("%TYPE");
            }
            if !tn.array_bounds.is_empty() {
                s.push_str("[]");
            }
            (s, false)
        }
        Some(NodeEnum::List(l)) => (names(&l.items).join("."), false),
        _ => (ast::const_str(arg).unwrap_or_default(), false),
    };
    Some(DictOption {
        name: d.defname.clone(),
        value,
        number,
    })
}

/// The unique index a duplicate name violates, as the catalog insert reports it.
fn duplicate(kind: TsKind, name: &str, namespace: u32) -> Error {
    let (table, index, cols) = match kind {
        TsKind::Config => (
            "pg_ts_config",
            "pg_ts_config_cfgname_index",
            "cfgname, cfgnamespace",
        ),
        TsKind::Dict => (
            "pg_ts_dict",
            "pg_ts_dict_dictname_index",
            "dictname, dictnamespace",
        ),
        TsKind::Parser => (
            "pg_ts_parser",
            "pg_ts_parser_prsname_index",
            "prsname, prsnamespace",
        ),
        TsKind::Template => (
            "pg_ts_template",
            "pg_ts_template_tmplname_index",
            "tmplname, tmplnamespace",
        ),
    };
    Error::new(
        code::UNIQUE_VIOLATION,
        format!("duplicate key value violates unique constraint \"{index}\""),
    )
    .detail(format!(
        "Key ({cols})=({name}, {namespace}) already exists."
    ))
    .table("pg_catalog", table)
    .constraint(index)
}

impl Ddl<'_, '_> {
    fn ts(&self) -> TsCatalog<'_> {
        TsCatalog {
            catalog: &self.cat,
            search_path: self.search_path(),
        }
    }

    /// `get_ts_*_oid` for a DDL statement: a missing object (or schema) is an error, or a
    /// "skipping" notice and None when `missing_ok`.
    pub(super) fn ts_object(
        &self,
        kind: TsKind,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        if let [.., sc, _] = parts {
            if self.cat.schema_by_name(sc).is_none() {
                if missing_ok {
                    self.notice(format!("schema \"{sc}\" does not exist, skipping"));
                    return Ok(None);
                }
                return Err(Error::new(
                    code::UNDEFINED_SCHEMA,
                    format!("schema \"{sc}\" does not exist"),
                ));
            }
            self.require_schema_usage(sc)?;
        }
        match self.cat.ts_lookup(kind, parts, self.search_path(), true)? {
            Some(oid) => Ok(Some(oid)),
            None if missing_ok => {
                self.notice(format!(
                    "{} \"{}\" does not exist, skipping",
                    kind.noun(),
                    parts.join(".")
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("{} \"{}\" does not exist", kind.noun(), parts.join(".")),
            )),
        }
    }

    /// `QualifiedNameGetCreationNamespace`: schema and name of a new object.
    fn ts_target(&mut self, parts: &[String]) -> Result<(u32, String)> {
        match parts {
            [.., sc, n] => {
                let schema = if sc == "pg_temp" {
                    self.temp_schema()?
                } else {
                    self.cat.schema_oid(sc)?
                };
                Ok((schema, crate::catalog::truncate_ident(n)))
            }
            [n] => Ok((
                self.creation_schema()?,
                crate::catalog::truncate_ident(n),
            )),
            [] => Err(crate::error::internal("empty name")),
        }
    }

    /// Ownership of a configuration or dictionary; parsers and templates are superuser
    /// business (`check_object_ownership`).
    pub(super) fn ts_require_owner(&self, kind: TsKind, oid: u32, shown: &str) -> Result<()> {
        match kind {
            TsKind::Parser | TsKind::Template => {
                if self.is_superuser() {
                    Ok(())
                } else {
                    Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        "must be superuser",
                    ))
                }
            }
            _ => self.require_owner(self.cat.ts_owner(kind, oid), kind.noun(), shown),
        }
    }

    /// DROP's check: the owner, or the owner of the object's schema.
    pub(super) fn ts_require_drop(&self, kind: TsKind, oid: u32, shown: &str) -> Result<()> {
        let schema = self.cat.ts_ident(kind, oid).map_or(0, |o| o.0);
        if let Some(s) = self.cat.schemas.get(&schema)
            && self
                .cat
                .has_privs_of_role(self.me(), s.owner, self.db_owner())
        {
            return Ok(());
        }
        self.ts_require_owner(kind, oid, shown)
    }

    pub(super) fn create_ts(&mut self, kind: TsKind, s: &pb::DefineStmt) -> Result<String> {
        let parts = names(&s.defnames);
        let defs: Vec<&pb::DefElem> = super::def_elems(&s.definition).collect();
        match kind {
            TsKind::Dict => self.create_ts_dictionary(&parts, &defs),
            TsKind::Config => self.create_ts_configuration(&parts, &defs),
            TsKind::Parser => self.create_ts_parser(&parts, &defs),
            TsKind::Template => self.create_ts_template(&parts, &defs),
        }
    }

    /// `verify_dictoptions`: the template's init function accepts the options, (name,
    /// value) with no value for an option given without one.
    fn verify_dict_options(
        &self,
        template: u32,
        options: Vec<(String, Option<DictOption>)>,
    ) -> Result<Vec<DictOption>> {
        let t = self.cat.ts_template(template).ok_or_else(|| {
            crate::error::internal(format!(
                "cache lookup failed for text search template {template}"
            ))
        })?;
        if t.init == 0 {
            if !options.is_empty() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "text search template \"{}\" does not accept options",
                        t.name
                    ),
                ));
            }
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(options.len());
        for (name, o) in options {
            out.push(o.ok_or_else(|| {
                Error::new(code::SYNTAX_ERROR, format!("{name} requires a parameter"))
            })?);
        }
        crate::types::tsearch::config::init_template(&t, &out, &self.ts())?;
        Ok(out)
    }

    fn create_ts_dictionary(&mut self, parts: &[String], defs: &[&pb::DefElem]) -> Result<String> {
        let (namespace, name) = self.ts_target(parts)?;
        self.require_schema_create(namespace)?;
        let mut template = 0;
        let mut options = Vec::new();
        for d in defs {
            if d.defname == "template" {
                template = self
                    .ts_object(TsKind::Template, &def_qualified(d)?, false)?
                    .unwrap_or(0);
            } else {
                options.push((d.defname.clone(), dict_option(d)));
            }
        }
        if template == 0 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "text search template is required",
            ));
        }
        let options = self.verify_dict_options(template, options)?;
        if self.cat.ts_oid_in(TsKind::Dict, namespace, &name).is_some() {
            return Err(duplicate(TsKind::Dict, &name, namespace));
        }
        let oid = self.cat.alloc_oid();
        let owner = self.me();
        self.cat.ts_put_dict(TsDictDef {
            oid,
            name,
            namespace,
            owner,
            template,
            options,
        });
        self.save()?;
        Ok("CREATE TEXT SEARCH DICTIONARY".into())
    }

    fn create_ts_configuration(
        &mut self,
        parts: &[String],
        defs: &[&pb::DefElem],
    ) -> Result<String> {
        let (namespace, name) = self.ts_target(parts)?;
        self.require_schema_create(namespace)?;
        let (mut parser, mut source) = (0, 0);
        for d in defs {
            match d.defname.as_str() {
                "parser" => {
                    parser = self
                        .ts_object(TsKind::Parser, &def_qualified(d)?, false)?
                        .unwrap_or(0)
                }
                "copy" => {
                    source = self
                        .ts_object(TsKind::Config, &def_qualified(d)?, false)?
                        .unwrap_or(0)
                }
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("text search configuration parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        if parser != 0 && source != 0 {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot specify both PARSER and COPY options",
            ));
        }
        let mut map = Vec::new();
        if source != 0 {
            let src = self.cat.ts_config(source).ok_or_else(|| {
                crate::error::internal(format!(
                    "cache lookup failed for text search configuration {source}"
                ))
            })?;
            parser = src.parser;
            map = src.map;
        }
        if parser == 0 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "text search parser is required",
            ));
        }
        if self
            .cat
            .ts_oid_in(TsKind::Config, namespace, &name)
            .is_some()
        {
            return Err(duplicate(TsKind::Config, &name, namespace));
        }
        let oid = self.cat.alloc_oid();
        let owner = self.me();
        self.cat.ts_put_config(TsConfigDef {
            oid,
            name,
            namespace,
            owner,
            parser,
            map,
        });
        self.save()?;
        Ok("CREATE TEXT SEARCH CONFIGURATION".into())
    }

    /// `get_ts_parser_func` / `get_ts_template_func`: the function a definition names, which
    /// must take `args` and return `ret`.
    fn ts_support_func(&self, d: &pb::DefElem, args: &[&str], ret: &str) -> Result<u32> {
        let parts = def_qualified(d)?;
        let shown = || format!("{}({})", parts.join("."), args.join(", "));
        let (schema, fname) = match parts.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] | [_, s, n] => (Some(s.as_str()), n.as_str()),
            _ => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "improper qualified name (too many dotted names): {}",
                        parts.join(".")
                    ),
                ));
            }
        };
        if let Some(s) = schema {
            self.cat.schema_oid(s)?;
            self.require_schema_usage(s)?;
        }
        let builtin = schema.is_none_or(|s| s == "pg_catalog");
        let found = TS_FUNCS
            .iter()
            .find(|f| builtin && f.1 == fname && f.2 == args);
        let Some(f) = found else {
            // A function made by CREATE FUNCTION (an extension's C template functions):
            // LookupFuncName by name and argument types, through the search path.
            let path: Vec<u32> = match schema {
                Some(s) => vec![self.cat.schema_oid(s)?],
                None => self.cat.routine_path(self.search_path()),
            };
            let user = path.iter().find_map(|sc| {
                self.cat.functions_named(fname).find(|f| {
                    f.schema == *sc
                        && f.args.len() == args.len()
                        && f.args.iter().zip(args).all(|(a, t)| a.1.display() == *t)
                })
            });
            let Some(u) = user else {
                return Err(Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("function {} does not exist", shown()),
                ));
            };
            if u.ret.display() != ret {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("function {} should return type {ret}", shown()),
                ));
            }
            return Ok(u.oid);
        };
        if f.3 != ret {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("function {} should return type {ret}", shown()),
            ));
        }
        Ok(f.0)
    }

    fn create_ts_parser(&mut self, parts: &[String], defs: &[&pb::DefElem]) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create text search parsers",
            ));
        }
        let (namespace, name) = self.ts_target(parts)?;
        let mut p = TsParserDef {
            oid: 0,
            name,
            namespace,
            start: 0,
            token: 0,
            end: 0,
            headline: 0,
            lextype: 0,
        };
        for d in defs {
            match d.defname.as_str() {
                "start" => {
                    p.start = self.ts_support_func(d, &["internal", "integer"], "internal")?
                }
                "gettoken" => {
                    p.token =
                        self.ts_support_func(d, &["internal", "internal", "internal"], "internal")?
                }
                "end" => p.end = self.ts_support_func(d, &["internal"], "void")?,
                "headline" => {
                    p.headline =
                        self.ts_support_func(d, &["internal", "internal", "tsquery"], "internal")?
                }
                "lextypes" => p.lextype = self.ts_support_func(d, &["internal"], "internal")?,
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("text search parser parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        for (oid, what) in [
            (p.start, "start"),
            (p.token, "gettoken"),
            (p.end, "end"),
            (p.lextype, "lextypes"),
        ] {
            if oid == 0 {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("text search parser {what} method is required"),
                ));
            }
        }
        if self
            .cat
            .ts_oid_in(TsKind::Parser, namespace, &p.name)
            .is_some()
        {
            return Err(duplicate(TsKind::Parser, &p.name, namespace));
        }
        p.oid = self.cat.alloc_oid();
        self.cat.ts_put_parser(p);
        self.save()?;
        Ok("CREATE TEXT SEARCH PARSER".into())
    }

    fn create_ts_template(&mut self, parts: &[String], defs: &[&pb::DefElem]) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create text search templates",
            ));
        }
        let (namespace, name) = self.ts_target(parts)?;
        let mut t = TsTemplateDef {
            oid: 0,
            name,
            namespace,
            init: 0,
            lexize: 0,
        };
        for d in defs {
            match d.defname.as_str() {
                "init" => t.init = self.ts_support_func(d, &["internal"], "internal")?,
                "lexize" => {
                    t.lexize = self.ts_support_func(
                        d,
                        &["internal", "internal", "internal", "internal"],
                        "internal",
                    )?
                }
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("text search template parameter \"{other}\" not recognized"),
                    ));
                }
            }
        }
        if t.lexize == 0 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "text search template lexize method is required",
            ));
        }
        if self
            .cat
            .ts_oid_in(TsKind::Template, namespace, &t.name)
            .is_some()
        {
            return Err(duplicate(TsKind::Template, &t.name, namespace));
        }
        t.oid = self.cat.alloc_oid();
        self.cat.ts_put_template(t);
        self.save()?;
        Ok("CREATE TEXT SEARCH TEMPLATE".into())
    }

    /// ALTER TEXT SEARCH DICTIONARY name (option = value, option, ...).
    pub(super) fn alter_ts_dictionary(&mut self, s: &pb::AlterTsDictionaryStmt) -> Result<String> {
        let parts = names(&s.dictname);
        let oid = self.ts_object(TsKind::Dict, &parts, false)?.unwrap_or(0);
        let d = self.cat.ts_dict(oid).ok_or_else(|| {
            crate::error::internal(format!(
                "cache lookup failed for text search dictionary {oid}"
            ))
        })?;
        self.require_owner(d.owner, TsKind::Dict.noun(), &parts.join("."))?;
        let mut options: Vec<(String, Option<DictOption>)> = d
            .options
            .iter()
            .map(|o| (o.name.clone(), Some(o.clone())))
            .collect();
        for e in super::def_elems(&s.options) {
            options.retain(|o| o.0 != e.defname);
            if e.arg.is_some() {
                options.push((e.defname.clone(), dict_option(e)));
            }
        }
        let options = self.verify_dict_options(d.template, options)?;
        self.cat.ts_put_dict(TsDictDef { options, ..d });
        self.save()?;
        Ok("ALTER TEXT SEARCH DICTIONARY".into())
    }

    /// ALTER TEXT SEARCH CONFIGURATION: ADD / ALTER / DROP MAPPING.
    pub(super) fn alter_ts_configuration(
        &mut self,
        s: &pb::AlterTsConfigurationStmt,
    ) -> Result<String> {
        let parts = names(&s.cfgname);
        if let [.., sc, _] = parts.as_slice() {
            self.require_schema_usage(sc)?;
        }
        let Some(oid) = self
            .cat
            .ts_lookup(TsKind::Config, &parts, self.search_path(), true)?
        else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "text search configuration \"{}\" does not exist",
                    parts.join(".")
                ),
            ));
        };
        let mut cfg = self.cat.ts_config(oid).ok_or_else(|| {
            crate::error::internal(format!(
                "cache lookup failed for text search configuration {oid}"
            ))
        })?;
        self.require_owner(cfg.owner, TsKind::Config.noun(), &parts.join("."))?;
        // getTokenTypes: the token types named, each once.
        let mut tokens: Vec<(usize, String)> = Vec::new();
        for n in &s.tokentype {
            let name = ast::sval(n).unwrap_or("").to_string();
            if tokens.iter().any(|t| t.1 == name) {
                continue;
            }
            let Some(num) = (1..=parser::LASTNUM as usize).find(|i| parser::TOK_ALIAS[*i] == name)
            else {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("token type \"{name}\" does not exist"),
                ));
            };
            tokens.push((num, name));
        }
        let grow = |map: &mut Vec<Vec<u32>>, t: usize| {
            if map.len() <= t {
                map.resize(t + 1, Vec::new());
            }
        };
        if !s.dicts.is_empty() {
            if s.r#override {
                for (t, _) in &tokens {
                    if let Some(m) = cfg.map.get_mut(*t) {
                        m.clear();
                    }
                }
            }
            let mut dicts = Vec::new();
            for d in &s.dicts {
                let parts = match inner(d) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                dicts.push(self.ts_object(TsKind::Dict, &parts, false)?.unwrap_or(0));
            }
            if s.replace {
                let (old, new) = (dicts[0], dicts[1]);
                for (t, m) in cfg.map.iter_mut().enumerate() {
                    if !tokens.is_empty() && !tokens.iter().any(|x| x.0 == t) {
                        continue;
                    }
                    for d in m.iter_mut().filter(|d| **d == old) {
                        *d = new;
                    }
                }
            } else {
                for (t, _) in &tokens {
                    grow(&mut cfg.map, *t);
                    if !cfg.map[*t].is_empty() {
                        return Err(Error::new(code::UNIQUE_VIOLATION, "duplicate key value violates unique constraint \"pg_ts_config_map_index\"")
                            .detail(format!("Key (mapcfg, maptokentype, mapseqno)=({oid}, {t}, 1) already exists."))
                            .table("pg_catalog", "pg_ts_config_map")
                            .constraint("pg_ts_config_map_index"));
                    }
                    cfg.map[*t] = dicts.clone();
                }
            }
        } else {
            for (t, name) in &tokens {
                match cfg.map.get_mut(*t).filter(|m| !m.is_empty()) {
                    Some(m) => m.clear(),
                    None if s.missing_ok => self.notice(format!(
                        "mapping for token type \"{name}\" does not exist, skipping"
                    )),
                    None => {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("mapping for token type \"{name}\" does not exist"),
                        ));
                    }
                }
            }
        }
        self.cat.ts_put_config(cfg);
        self.save()?;
        Ok("ALTER TEXT SEARCH CONFIGURATION".into())
    }

    /// ALTER TEXT SEARCH ... RENAME TO (`AlterObjectRename_internal`).
    pub(super) fn rename_ts(&mut self, kind: TsKind, s: &pb::RenameStmt) -> Result<String> {
        let tag = format!("ALTER {}", kind.noun().to_uppercase());
        let parts = match s.object.as_deref().and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            _ => Vec::new(),
        };
        let oid = self.ts_object(kind, &parts, false)?.unwrap_or(0);
        let (namespace, old) = self.cat.ts_ident(kind, oid).unwrap_or_default();
        let newname = crate::catalog::truncate_ident(&s.newname);
        if !self.is_superuser() {
            if matches!(kind, TsKind::Parser | TsKind::Template) {
                let what = self
                    .cat
                    .ts_qualified(kind, oid, self.search_path())
                    .unwrap_or_default();
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("must be superuser to rename {} {what}", kind.noun()),
                ));
            }
            self.require_owner(self.cat.ts_owner(kind, oid), kind.noun(), &old)?;
            self.require_schema_create(namespace)?;
        }
        if self.cat.ts_oid_in(kind, namespace, &newname).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "{} \"{newname}\" already exists in schema \"{}\"",
                    kind.noun(),
                    self.cat.schema_name(namespace)
                ),
            ));
        }
        self.cat.ts_set_name(kind, oid, namespace, newname);
        self.save()?;
        Ok(tag)
    }

    /// ALTER TEXT SEARCH ... SET SCHEMA (`AlterObjectNamespace_internal`).
    pub(super) fn set_schema_ts(
        &mut self,
        kind: TsKind,
        s: &pb::AlterObjectSchemaStmt,
    ) -> Result<String> {
        let tag = format!("ALTER {}", kind.noun().to_uppercase());
        let parts = match s.object.as_deref().and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            _ => Vec::new(),
        };
        let oid = self.ts_object(kind, &parts, false)?.unwrap_or(0);
        let new_schema = self.cat.schema_oid(&s.newschema)?;
        self.require_schema_create(new_schema)?;
        let (old_schema, name) = self.cat.ts_ident(kind, oid).unwrap_or_default();
        if old_schema == new_schema {
            return Ok(tag);
        }
        let temp = |o: u32| self.cat.schema_name(o).starts_with("pg_temp");
        if temp(old_schema) || temp(new_schema) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if !self.is_superuser() {
            if matches!(kind, TsKind::Parser | TsKind::Template) {
                let what = self
                    .cat
                    .ts_qualified(kind, oid, self.search_path())
                    .unwrap_or_default();
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("must be superuser to set schema of {} {what}", kind.noun()),
                ));
            }
            self.require_owner(self.cat.ts_owner(kind, oid), kind.noun(), &name)?;
        }
        if self.cat.ts_oid_in(kind, new_schema, &name).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "{} \"{name}\" already exists in schema \"{}\"",
                    kind.noun(),
                    s.newschema
                ),
            ));
        }
        self.cat.ts_set_name(kind, oid, new_schema, name);
        self.save()?;
        Ok(tag)
    }

    /// ALTER TEXT SEARCH DICTIONARY / CONFIGURATION ... OWNER TO.
    pub(super) fn owner_ts(
        &mut self,
        kind: TsKind,
        obj: Option<&pb::Node>,
        new: u32,
    ) -> Result<()> {
        let parts = match obj.and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            _ => Vec::new(),
        };
        let oid = self.ts_object(kind, &parts, false)?.unwrap_or(0);
        let (schema, name) = self.cat.ts_ident(kind, oid).unwrap_or_default();
        let owner = self.cat.ts_owner(kind, oid);
        if owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.require_owner(owner, kind.noun(), &name)?;
        }
        self.check_new_owner(new, Some(schema))?;
        match kind {
            TsKind::Config => {
                if let Some(c) = self.cat.ts_config(oid) {
                    self.cat.ts_put_config(TsConfigDef { owner: new, ..c });
                }
            }
            TsKind::Dict => {
                if let Some(d) = self.cat.ts_dict(oid) {
                    self.cat.ts_put_dict(TsDictDef { owner: new, ..d });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_functions_are_the_builtin_ones() {
        for (oid, name, ..) in TS_FUNCS {
            assert!(
                crate::sql::func::pg_oids::PROCS
                    .iter()
                    .any(|p| p.0 == oid && p.1 == name),
                "{name}"
            );
        }
    }
}
