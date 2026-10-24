//! CREATE TYPE for base types and shell types (DefineType, TypeShellMake) and ALTER TYPE
//! ... SET (AlterType). A base type's input and output functions are internal-language
//! functions naming built-in ones (`AS 'int4in'`); its values are that built-in type's.

use super::Ddl;
use crate::catalog::userobj::BaseTypeDef;
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{const_str, inner, names, with_pos};
use crate::types::{BaseType, DataType};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const CSTRING: u32 = 2275;
const INTERNAL: u32 = 2281;
const OID: u32 = 26;
const INT4: u32 = 23;
const BOOL: u32 = 16;
const BYTEA: u32 = 17;
const CSTRING_ARRAY: u32 = 1263;
/// raw_array_subscript_handler, array_subscript_handler
const RAW_ARRAY_SUBSCRIPT: u32 = 4492;
const ARRAY_SUBSCRIPT: u32 = 6179;

/// A DefElem argument as a qualified name (defGetQualifiedName).
fn def_qualified(d: &pb::DefElem) -> Vec<String> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::TypeName(tn)) => names(&tn.names),
        Some(NodeEnum::List(l)) => names(&l.items),
        Some(NodeEnum::String(s)) => vec![s.sval.clone()],
        _ => d.arg.as_deref().and_then(const_str).into_iter().collect(),
    }
}

/// defGetString.
fn def_string(d: &pb::DefElem) -> String {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::TypeName(tn)) => names(&tn.names).join("."),
        Some(NodeEnum::List(l)) => names(&l.items).join("."),
        _ => d.arg.as_deref().and_then(const_str).unwrap_or_default(),
    }
}

/// defGetBoolean.
fn def_boolean(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    match inner(arg) {
        Some(NodeEnum::Integer(i)) if i.ival == 0 || i.ival == 1 => return Ok(i.ival == 1),
        Some(NodeEnum::AConst(c)) if matches!(&c.val, Some(pb::a_const::Val::Ival(i)) if i.ival == 0 || i.ival == 1) => {
            return Ok(matches!(&c.val, Some(pb::a_const::Val::Ival(i)) if i.ival == 1));
        }
        _ => {}
    }
    match const_str(arg).unwrap_or_default().to_ascii_lowercase().as_str() {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

impl Ddl<'_, '_> {
    /// TypeShellMake: a shell type `name` in `schema`.
    pub(super) fn make_shell_type(&mut self, schema: u32, name: &str) -> Result<DataType> {
        let oid = self.cat.alloc_oid();
        let def = BaseTypeDef {
            ty: Arc::new(BaseType {
                oid,
                name: Arc::from(name),
                schema,
                array_oid: 0,
                repr: None,
                len: 4,
                byval: true,
                align: 'i',
                storage: 'p',
                category: 'P',
                preferred: false,
                collatable: false,
                pseudo: false,
                ext: 0,
            }),
            owner: self.me(),
            input: 2398,
            output: 2399,
            receive: 0,
            send: 0,
            typmod_in: 0,
            typmod_out: 0,
            analyze: 0,
            subscript: 0,
            element: 0,
            delimiter: ',',
            storage: 'p',
            default: None,
        };
        let t = def.data_type();
        self.cat.base_types.insert(oid, Arc::new(def));
        Ok(t)
    }

    /// The function a type support-function option names, with exactly these argument
    /// types (LookupFuncName): (oid, return type oid, volatility).
    fn type_support_fn(&self, parts: &[String], args: &[u32]) -> Option<(u32, u32, char)> {
        let name = parts.last()?;
        let schemas: Vec<u32> = match parts {
            [.., sc, _] => vec![self.cat.schema_by_name(sc)?.oid],
            _ => self.cat.effective_path(self.search_path()),
        };
        for s in schemas {
            for f in self.cat.functions.values().filter(|f| f.schema == s && &f.name == name) {
                let p = super::opclass::proc_info(&self.cat, f.oid)?;
                if p.args == args {
                    return Some((f.oid, p.ret, f.volatility));
                }
            }
            if s == crate::catalog::PG_CATALOG_OID
                && let Some(p) = crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .find(|p| p.name == name && p.args == args)
            {
                return Some((p.oid, p.ret, p.volatility));
            }
        }
        None
    }

    fn no_such_function(&self, parts: &[String], args: &[u32]) -> Error {
        let t: Vec<String> = args.iter().map(|o| self.type_text(*o)).collect();
        Error::new(
            code::UNDEFINED_FUNCTION,
            format!("function {}({}) does not exist", parts.join("."), t.join(", ")),
        )
    }

    /// findTypeInputFunction & co: a support function with one of the signatures, which
    /// must return `ret`.
    fn type_function(
        &self,
        parts: &[String],
        sigs: &[&[u32]],
        ret: u32,
        what: &str,
        ret_name: Option<&str>,
    ) -> Result<u32> {
        let found: Vec<(u32, u32, char)> = sigs
            .iter()
            .filter_map(|a| self.type_support_fn(parts, a))
            .collect();
        let (oid, rt, vol) = match found.as_slice() {
            [one] => *one,
            [] => return Err(self.no_such_function(parts, sigs[0])),
            _ => {
                return Err(Error::new(
                    code::AMBIGUOUS_FUNCTION,
                    format!("{what} {} has multiple matches", parts.join(".")),
                ));
            }
        };
        if rt != ret {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!(
                    "{what} {} must return type {}",
                    parts.join("."),
                    ret_name.map_or_else(|| self.type_text(ret), str::to_string)
                ),
            ));
        }
        if vol == 'v' && matches!(what, "type input function" | "type output function") {
            self.ctx.notice(
                crate::session::warning(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("{what} {} should not be volatile", parts.join(".")),
                ),
            );
        }
        Ok(oid)
    }

    /// The built-in type whose values a type with input function `input` holds: the
    /// return type of the built-in function an internal-language input function names.
    fn base_repr(&self, input: u32) -> DataType {
        let f = self.cat.functions.get(&input);
        let builtin = match f {
            Some(f) if f.language == "internal" => crate::catalog::system::builtin_by_prosrc(&f.body),
            Some(_) => None,
            None => Some(input),
        };
        builtin
            .and_then(|b| {
                crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .find(|p| p.oid == b)
                    .map(|p| p.ret)
            })
            .and_then(DataType::from_oid)
            .filter(|t| !t.is_polymorphic() && !matches!(t, DataType::Any | DataType::Cstring))
            .unwrap_or(DataType::Text)
    }

    /// CREATE TYPE name [(...)] for a base or shell type (DefineType).
    pub(super) fn define_base_type(&mut self, s: &pb::DefineStmt) -> Result<String> {
        const TAG: &str = "CREATE TYPE";
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create a base type",
            ));
        }
        let parts = names(&s.defnames);
        let name = crate::catalog::truncate_ident(parts.last().map(String::as_str).unwrap_or(""));
        let schema = match parts.as_slice() {
            [.., sc, _] => self.cat.schema_oid(sc)?,
            _ => self.cat.creation_schema(self.search_path())?,
        };
        let existing = self.cat.type_by_name(Some(schema), &name, &[]);
        let shell = existing.and_then(|o| self.cat.base_types.get(&o).filter(|d| d.is_shell()).cloned());
        let taken = (existing.is_some() && shell.is_none())
            || matches!(
                self.cat.relation_in(schema, &name),
                Some(crate::catalog::Relation::Table(_) | crate::catalog::Relation::View(_))
            );
        if taken {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            ));
        }
        if s.definition.is_empty() {
            if shell.is_some() {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("type \"{name}\" already exists"),
                ));
            }
            self.require_schema_create(schema)?;
            self.make_shell_type(schema, &name)?;
            self.save()?;
            return Ok(TAG.into());
        }
        let Some(shell) = shell else {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("type \"{name}\" does not exist"),
            )
            .hint("Create the type as a shell type, then create its I/O functions, then do a full CREATE TYPE."));
        };
        let typoid = shell.oid();
        let mut seen: std::collections::HashMap<&str, &pb::DefElem> = Default::default();
        for d in super::def_elems(&s.definition) {
            let key = match d.defname.as_str() {
                "analyse" => "analyze",
                k @ ("like" | "internallength" | "input" | "output" | "receive" | "send"
                | "typmod_in" | "typmod_out" | "analyze" | "subscript" | "category"
                | "preferred" | "delimiter" | "element" | "default" | "passedbyvalue"
                | "alignment" | "storage" | "collatable") => k,
                other => {
                    self.ctx.notice(with_pos(
                        crate::session::warning(
                            code::SYNTAX_ERROR,
                            format!("type attribute \"{other}\" not recognized"),
                        ),
                        d.location,
                    ));
                    continue;
                }
            };
            if seen.insert(key, d).is_some() {
                return Err(with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
        }
        let (mut len, mut byval, mut align, mut storage) = (-1i16, false, 'i', 'p');
        if let Some(d) = seen.get("like") {
            let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
                return Err(Error::new(code::SYNTAX_ERROR, "like requires a type name"));
            };
            let (t, dom) = crate::sql::binder::resolve_type_name(&self.cat, self.search_path(), tn)?;
            let t = match dom.and_then(|o| self.cat.domains.get(&o)) {
                Some(d) => d.base.clone(),
                None => t,
            };
            len = t.typlen();
            byval = t.by_value();
            align = crate::catalog::system::align(&t);
            storage = crate::catalog::system::typstorage(&t);
        }
        if let Some(d) = seen.get("internallength") {
            let v = def_string(d);
            len = if v.eq_ignore_ascii_case("variable") {
                -1
            } else {
                v.parse::<i16>().map_err(|_| {
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!("invalid argument for {}: \"{v}\"", d.defname),
                    )
                })?
            };
        }
        let mut category = 'U';
        if let Some(d) = seen.get("category") {
            let p = def_string(d);
            category = p.chars().next().unwrap_or('\0');
            if !(32..=126).contains(&(category as u32)) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid type category \"{p}\": must be simple ASCII"),
                ));
            }
        }
        let preferred = match seen.get("preferred") {
            Some(d) => def_boolean(d)?,
            None => false,
        };
        let delimiter = seen
            .get("delimiter")
            .and_then(|d| def_string(d).chars().next())
            .unwrap_or(',');
        let mut element = 0;
        if let Some(d) = seen.get("element") {
            let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
                return Err(Error::new(code::SYNTAX_ERROR, "element requires a type name"));
            };
            element = self.type_name_oid(tn)?;
            if crate::types::oid::other_pseudo_name(element).is_some()
                || DataType::from_oid(element).is_some_and(|t| t.category() == crate::types::Category::Pseudo)
            {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!("array element type cannot be {}", self.type_text(element)),
                ));
            }
        }
        let default = seen.get("default").map(|d| def_string(d));
        if let Some(d) = seen.get("passedbyvalue") {
            byval = def_boolean(d)?;
        }
        if let Some(d) = seen.get("alignment") {
            let a = def_string(d).to_ascii_lowercase();
            align = match a.as_str() {
                "double" | "float8" | "pg_catalog.float8" => 'd',
                "int4" | "pg_catalog.int4" => 'i',
                "int2" | "pg_catalog.int2" => 's',
                "char" | "pg_catalog.bpchar" => 'c',
                _ => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("alignment \"{}\" not recognized", def_string(d)),
                    ));
                }
            };
        }
        if let Some(d) = seen.get("storage") {
            storage = storage_letter(&def_string(d))?;
        }
        let collatable = match seen.get("collatable") {
            Some(d) => def_boolean(d)?,
            None => false,
        };
        let name_of = |k: &str| seen.get(k).map(|d| def_qualified(d));
        let (Some(input), Some(output)) = (name_of("input"), name_of("output")) else {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                if name_of("input").is_none() {
                    "type input function must be specified"
                } else {
                    "type output function must be specified"
                },
            ));
        };
        if name_of("typmod_in").is_none() && name_of("typmod_out").is_some() {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "type modifier output function is useless without a type modifier input function",
            ));
        }
        let input_oid = self.type_function(
            &input,
            &[&[CSTRING], &[CSTRING, OID, INT4]],
            typoid,
            "type input function",
            None,
        )?;
        let output_oid = self.type_function(
            &output,
            &[&[typoid]],
            CSTRING,
            "type output function",
            Some("cstring"),
        )?;
        let receive = match name_of("receive") {
            Some(n) => self.type_function(
                &n,
                &[&[INTERNAL], &[INTERNAL, OID, INT4]],
                typoid,
                "type receive function",
                None,
            )?,
            None => 0,
        };
        let send = match name_of("send") {
            Some(n) => {
                self.type_function(&n, &[&[typoid]], BYTEA, "type send function", Some("bytea"))?
            }
            None => 0,
        };
        let typmod_in = match name_of("typmod_in") {
            Some(n) => self.type_function(
                &n,
                &[&[CSTRING_ARRAY]],
                INT4,
                "typmod_in function",
                Some("integer"),
            )?,
            None => 0,
        };
        let typmod_out = match name_of("typmod_out") {
            Some(n) => self.type_function(
                &n,
                &[&[INT4]],
                CSTRING,
                "typmod_out function",
                Some("cstring"),
            )?,
            None => 0,
        };
        let analyze = match name_of("analyze") {
            Some(n) => self.type_function(
                &n,
                &[&[INTERNAL]],
                BOOL,
                "type analyze function",
                Some("boolean"),
            )?,
            None => 0,
        };
        let subscript = match name_of("subscript") {
            Some(n) => {
                let f = self.type_function(
                    &n,
                    &[&[INTERNAL]],
                    INTERNAL,
                    "type subscripting function",
                    Some("internal"),
                )?;
                if f == ARRAY_SUBSCRIPT {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        format!(
                            "user-defined types cannot use subscripting function {}",
                            n.join(".")
                        ),
                    ));
                }
                f
            }
            None if element != 0 => {
                let elem_len = DataType::from_oid(element)
                    .map(|t| t.typlen())
                    .or_else(|| self.cat.user_data_type(element).map(|t| t.typlen()))
                    .unwrap_or(-1);
                if len > 0 && !byval && elem_len > 0 {
                    RAW_ARRAY_SUBSCRIPT
                } else {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "element type cannot be specified without a subscripting function",
                    ));
                }
            }
            None => 0,
        };
        let repr = self.base_repr(input_oid);
        // A type an extension's script makes that the engine implements natively.
        let ext = super::extension::creating_extension()
            .and_then(|e| self.cat.extensions.iter().find(|x| x.0 == e))
            .map_or(0, |e| crate::ext::types::kind_for(&e.1, &name));
        let array_oid = self.cat.alloc_oid();
        let ty = Arc::new(BaseType {
            ext,
            oid: typoid,
            name: Arc::from(name.as_str()),
            schema,
            array_oid,
            repr: Some(repr),
            len,
            byval,
            align,
            storage,
            category,
            preferred,
            collatable,
            pseudo: false,
        });
        let def = BaseTypeDef {
            ty: ty.clone(),
            owner: shell.owner,
            input: input_oid,
            output: output_oid,
            receive,
            send,
            typmod_in,
            typmod_out,
            analyze,
            subscript,
            element,
            delimiter,
            storage,
            default,
        };
        self.cat.base_types.insert(typoid, Arc::new(def));
        self.refresh_base_type(&ty);
        self.save()?;
        Ok(TAG.into())
    }

    /// Signatures that named the type while it was a shell (or before an ALTER TYPE) take
    /// its definition.
    pub(super) fn refresh_base_type(&mut self, ty: &Arc<BaseType>) {
        fn fix(t: &mut DataType, ty: &Arc<BaseType>) -> bool {
            match t {
                DataType::Base(b) if b.oid == ty.oid => {
                    *b = ty.clone();
                    true
                }
                DataType::Array(e) => fix(e, ty),
                _ => false,
            }
        }
        let funcs: Vec<u32> = self.cat.functions.keys().copied().collect();
        for oid in funcs {
            let mut f = (*self.cat.functions[&oid]).clone();
            let mut changed = fix(&mut f.ret, ty);
            for a in f.args.iter_mut() {
                changed |= fix(&mut a.1, ty);
            }
            for a in f.out_cols.iter_mut() {
                changed |= fix(&mut a.1, ty);
            }
            if changed {
                std::sync::Arc::make_mut(&mut self.cat.functions).insert(oid, Arc::new(f));
            }
        }
        let ops: Vec<u32> = self.cat.operators.keys().copied().collect();
        for oid in ops {
            let mut o = (*self.cat.operators[&oid]).clone();
            let mut changed = fix(&mut o.right, ty);
            if let Some(l) = o.left.as_mut() {
                changed |= fix(l, ty);
            }
            if let Some(r) = o.result.as_mut() {
                changed |= fix(r, ty);
            }
            if changed {
                self.cat.operators.insert(oid, Arc::new(o));
            }
        }
        let tables: Vec<u32> = self.cat.tables.keys().copied().collect();
        for oid in tables {
            let t = self.cat.tables[&oid].clone();
            if t.columns.iter().any(|c| matches!(&c.ty, DataType::Base(b) if b.oid == ty.oid) || matches!(&c.ty, DataType::Array(e) if matches!(&**e, DataType::Base(b) if b.oid == ty.oid))) {
                let mut t = (*t).clone();
                for c in t.columns.iter_mut() {
                    fix(&mut c.ty, ty);
                }
                self.cat.put_table(t);
            }
        }
        let domains: Vec<u32> = self.cat.domains.keys().copied().collect();
        for oid in domains {
            let mut d = (*self.cat.domains[&oid]).clone();
            if fix(&mut d.base, ty) {
                self.cat.domains.insert(oid, Arc::new(d));
            }
        }
    }

    /// ALTER TYPE name SET (...) (AlterType).
    pub(super) fn alter_base_type(&mut self, s: &pb::AlterTypeStmt) -> Result<String> {
        const TAG: &str = "ALTER TYPE";
        let tn = pb::TypeName {
            names: s.type_name.clone(),
            typemod: -1,
            ..Default::default()
        };
        let (t, dom) = crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), &tn)?;
        let shown = names(&s.type_name).join(".");
        let oid = dom.unwrap_or_else(|| crate::catalog::system::type_oid(&self.cat, &t));
        let owner = self
            .cat
            .obj_owner(crate::catalog::acl::ObjKind::Type, oid, self.db_owner())
            .unwrap_or(0);
        self.require_owner(owner, "type", &shown)?;
        let Some(def) = self.cat.base_types.get(&oid).cloned() else {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("{} is not a base type", self.type_text(oid)),
            ));
        };
        if def.is_shell() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("type \"{shown}\" is only a shell"),
            ));
        }
        let mut new = (*def).clone();
        let mut ty = (*def.ty).clone();
        for d in super::def_elems(&s.options) {
            let fname = || def_qualified(d);
            let none = d.arg.is_none()
                || matches!(d.arg.as_deref().and_then(inner), Some(NodeEnum::TypeName(tn)) if names(&tn.names) == ["none"]);
            match d.defname.as_str() {
                "storage" => {
                    let st = storage_letter(&def_string(d))?;
                    if st != 'p' && ty.len != -1 {
                        return Err(Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            "fixed-size types must have storage PLAIN",
                        ));
                    }
                    if st == 'p' && def.storage != 'p' && ty.len == -1 {
                        return Err(Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            "cannot change type's storage to PLAIN",
                        ));
                    }
                    new.storage = st;
                    ty.storage = st;
                }
                "receive" => {
                    new.receive = if none {
                        0
                    } else {
                        self.type_function(
                            &fname(),
                            &[&[INTERNAL], &[INTERNAL, OID, INT4]],
                            oid,
                            "type receive function",
                            None,
                        )?
                    }
                }
                "send" => {
                    new.send = if none {
                        0
                    } else {
                        self.type_function(&fname(), &[&[oid]], BYTEA, "type send function", Some("bytea"))?
                    }
                }
                "typmod_in" => {
                    new.typmod_in = if none {
                        0
                    } else {
                        self.type_function(
                            &fname(),
                            &[&[CSTRING_ARRAY]],
                            INT4,
                            "typmod_in function",
                            Some("integer"),
                        )?
                    }
                }
                "typmod_out" => {
                    new.typmod_out = if none {
                        0
                    } else {
                        self.type_function(&fname(), &[&[INT4]], CSTRING, "typmod_out function", Some("cstring"))?
                    }
                }
                "analyze" | "analyse" => {
                    new.analyze = if none {
                        0
                    } else {
                        self.type_function(&fname(), &[&[INTERNAL]], BOOL, "type analyze function", Some("boolean"))?
                    }
                }
                "subscript" => {
                    new.subscript = if none {
                        0
                    } else {
                        self.type_function(
                            &fname(),
                            &[&[INTERNAL]],
                            INTERNAL,
                            "type subscripting function",
                            Some("internal"),
                        )?
                    }
                }
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("type attribute \"{other}\" cannot be changed"),
                    ));
                }
            }
        }
        let ty = Arc::new(ty);
        new.ty = ty.clone();
        self.cat.base_types.insert(oid, Arc::new(new));
        // Domains over the type take its storage (AlterTypeRecurse).
        self.refresh_base_type(&ty);
        self.save()?;
        Ok(TAG.into())
    }
}

/// A typstorage name.
fn storage_letter(a: &str) -> Result<char> {
    Ok(match a.to_ascii_lowercase().as_str() {
        "plain" => 'p',
        "external" => 'e',
        "extended" => 'x',
        "main" => 'm',
        _ => {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("storage \"{a}\" not recognized"),
            ));
        }
    })
}
