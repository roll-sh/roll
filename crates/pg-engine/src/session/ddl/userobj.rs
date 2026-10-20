//! CREATE / DROP / ALTER for casts (CreateCast), conversions (CreateConversionCommand),
//! access methods (CreateAccessMethod), procedural languages (CreateProceduralLanguage) and
//! transforms (CreateTransform).

use super::Ddl;
use crate::catalog::userobj::{AccessMethod, Cast, Conversion, Language, Transform};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{inner, names};
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const INTERNAL: u32 = 2281;
const INT4: u32 = 23;
const BOOL: u32 = 16;
const CSTRING: u32 = 2275;
const OID: u32 = 26;

/// The typtype letter of a type ('b', 'c', 'd', 'e', 'p', 'r', 'm').
fn typtype(cat: &crate::catalog::Catalog, oid: u32) -> char {
    if cat.domains.contains_key(&oid) {
        return 'd';
    }
    if cat.enums.contains_key(&oid) {
        return 'e';
    }
    if let Some(b) = cat.base_types.get(&oid) {
        return if b.is_shell() { 'p' } else { 'b' };
    }
    if cat.ranges.contains_key(&oid) {
        return 'r';
    }
    if cat.ranges.values().any(|r| r.multi_oid == oid) {
        return 'm';
    }
    if crate::types::oid::other_pseudo_name(oid).is_some() {
        return 'p';
    }
    match cat.user_data_type(oid).or_else(|| DataType::from_oid(oid)) {
        Some(DataType::Row(_)) => 'c',
        Some(DataType::Record) => 'p',
        Some(DataType::Range(_)) => 'r',
        Some(DataType::Multirange(_)) => 'm',
        Some(t) if t.category() == crate::types::Category::Pseudo => 'p',
        _ => 'b',
    }
}

impl Ddl<'_, '_> {
    /// LookupFuncName: a function by name and exact argument type oids.
    pub(super) fn func_by_types(&self, parts: &[String], args: &[u32]) -> Result<super::opclass::ProcInfo> {
        let mut owa = pb::ObjectWithArgs {
            objname: parts
                .iter()
                .map(|p| pb::Node {
                    node: Some(NodeEnum::String(pb::String { sval: p.clone() })),
                })
                .collect(),
            ..Default::default()
        };
        // Types as names the lookup reads back.
        for a in args {
            let name = crate::sql::func::system::format_type_oid(*a, -1, &self.cat, &[])
                .unwrap_or_default();
            let tn = crate::sql::binder::parse_type_string(&name)?;
            owa.objargs.push(pb::Node {
                node: Some(NodeEnum::TypeName(tn)),
            });
        }
        self.lookup_proc(&owa).map_err(|e| {
            if e.code == code::UNDEFINED_FUNCTION {
                let t: Vec<String> = args.iter().map(|o| self.type_text(*o)).collect();
                Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("function {}({}) does not exist", parts.join("."), t.join(", ")),
                )
            } else {
                e
            }
        })
    }

    /// The owner of a type (the bootstrap superuser for Postgres's own).
    fn type_owner(&self, oid: u32) -> u32 {
        self.cat
            .obj_owner(crate::catalog::acl::ObjKind::Type, oid, self.db_owner())
            .unwrap_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID)
    }

    fn owns(&self, owner: u32) -> bool {
        self.cat.has_privs_of_role(self.me(), owner, self.db_owner())
    }

    /// IsBinaryCoercibleWithCast: whether `src` is binary coercible to `target`, and the
    /// cast that makes it so (0 if none is needed).
    pub(super) fn binary_coercible(&self, src: u32, target: u32) -> Option<u32> {
        if src == target || matches!(target, 2276 | 2283 | 5077) {
            return Some(0);
        }
        let src = self
            .cat
            .domains
            .get(&src)
            .map(|d| d.parent.unwrap_or_else(|| crate::catalog::system::type_oid(&self.cat, &d.base)))
            .unwrap_or(src);
        if src == target {
            return Some(0);
        }
        let st = self.cat.user_data_type(src).or_else(|| DataType::from_oid(src));
        match (target, &st) {
            (2277 | 5078, Some(t)) if t.is_array() => return Some(0),
            (2776 | 5079, Some(t)) if !t.is_array() => return Some(0),
            (3500, Some(DataType::Enum(..))) => return Some(0),
            (3831 | 5080, Some(DataType::Range(_))) => return Some(0),
            (4537 | 4538, Some(DataType::Multirange(_))) => return Some(0),
            (2249, Some(DataType::Row(r))) if r.oid != 0 => return Some(0),
            _ => {}
        }
        self.cat
            .casts_all()
            .into_iter()
            .find(|c| c.source == src && c.target == target && c.method == 'b' && c.context == 'i')
            .map(|c| c.oid)
    }

    /// CREATE CAST.
    pub(super) fn create_cast(&mut self, s: &pb::CreateCastStmt) -> Result<String> {
        let (Some(stn), Some(ttn)) = (&s.sourcetype, &s.targettype) else {
            return Err(crate::error::internal("cast types"));
        };
        let shown = |tn: &pb::TypeName| names(&tn.names).join(".");
        let src = self.type_name_oid(stn).map_err(|e| Error { position: None, ..e })?;
        let dst = self.type_name_oid(ttn).map_err(|e| Error { position: None, ..e })?;
        let (st, tt) = (typtype(&self.cat, src), typtype(&self.cat, dst));
        if st == 'p' {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("source data type {} is a pseudo-type", shown(stn)),
            ));
        }
        if tt == 'p' {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("target data type {} is a pseudo-type", shown(ttn)),
            ));
        }
        if !self.owns(self.type_owner(src)) && !self.owns(self.type_owner(dst)) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "must be owner of type {} or type {}",
                    self.type_text(src),
                    self.type_text(dst)
                ),
            ));
        }
        for oid in [src, dst] {
            self.require_type_usage(&crate::types::DataType::Unknown, Some(oid))?;
        }
        if st == 'd' {
            self.ctx.notice(crate::session::warning(
                code::WRONG_OBJECT_TYPE,
                "cast will be ignored because the source data type is a domain",
            ));
        } else if tt == 'd' {
            self.ctx.notice(crate::session::warning(
                code::WRONG_OBJECT_TYPE,
                "cast will be ignored because the target data type is a domain",
            ));
        }
        let method = if s.func.is_some() {
            'f'
        } else if s.inout {
            'i'
        } else {
            'b'
        };
        let invalid = |m: &str| Error::new(code::INVALID_OBJECT_DEFINITION, m.to_string());
        let (mut func, mut nargs, mut incast, mut outcast) = (0, 0, 0, 0);
        if let Some(owa) = &s.func {
            let p = self.lookup_proc(owa)?;
            nargs = p.args.len();
            if !(1..=3).contains(&nargs) {
                return Err(invalid("cast function must take one to three arguments"));
            }
            match self.binary_coercible(src, p.args[0]) {
                Some(c) => incast = c,
                None => {
                    return Err(invalid(
                        "argument of cast function must match or be binary-coercible from source data type",
                    ));
                }
            }
            if nargs > 1 && p.args[1] != INT4 {
                return Err(invalid("second argument of cast function must be type integer"));
            }
            if nargs > 2 && p.args[2] != BOOL {
                return Err(invalid("third argument of cast function must be type boolean"));
            }
            match self.binary_coercible(p.ret, dst) {
                Some(c) => outcast = c,
                None => {
                    return Err(invalid(
                        "return data type of cast function must match or be binary-coercible to target data type",
                    ));
                }
            }
            if let Some(f) = self.cat.functions.get(&p.oid) {
                if f.procedure || f.agg.is_some() {
                    return Err(invalid("cast function must be a normal function"));
                }
                if f.returns_set {
                    return Err(invalid("cast function must not return a set"));
                }
            }
            func = p.oid;
        }
        if method == 'b' {
            if !self.is_superuser() {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "must be superuser to create a cast WITHOUT FUNCTION",
                ));
            }
            let phys = |o: u32| {
                let t = self
                    .cat
                    .user_data_type(o)
                    .or_else(|| DataType::from_oid(o))
                    .unwrap_or(DataType::Text);
                (t.typlen(), t.by_value(), crate::catalog::system::align(&t))
            };
            if phys(src) != phys(dst) {
                return Err(invalid("source and target data types are not physically compatible"));
            }
            if st == 'c' || tt == 'c' {
                return Err(invalid("composite data types are not binary-compatible"));
            }
            let is_array = |o: u32| {
                self.cat
                    .user_data_type(o)
                    .or_else(|| DataType::from_oid(o))
                    .is_some_and(|t| t.is_array())
            };
            if is_array(src) || is_array(dst) {
                return Err(invalid("array data types are not binary-compatible"));
            }
            if matches!(st, 'r' | 'm') || matches!(tt, 'r' | 'm') {
                return Err(invalid("range data types are not binary-compatible"));
            }
            if st == 'e' || tt == 'e' {
                return Err(invalid("enum data types are not binary-compatible"));
            }
            if st == 'd' || tt == 'd' {
                return Err(invalid("domain data types must not be marked binary-compatible"));
            }
        }
        if src == dst && nargs < 2 {
            return Err(invalid("source data type and target data type are the same"));
        }
        let context = match pb::CoercionContext::try_from(s.context) {
            Ok(pb::CoercionContext::CoercionImplicit) => 'i',
            Ok(pb::CoercionContext::CoercionAssignment) => 'a',
            _ => 'e',
        };
        if self
            .cat
            .casts_all()
            .iter()
            .any(|c| c.source == src && c.target == dst)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "cast from type {} to type {} already exists",
                    self.type_text(src),
                    self.type_text(dst)
                ),
            ));
        }
        let oid = self.cat.alloc_oid();
        self.cat.casts.insert(
            oid,
            Arc::new(Cast {
                oid,
                source: src,
                target: dst,
                func,
                context,
                method,
                incast,
                outcast,
            }),
        );
        self.save()?;
        Ok("CREATE CAST".into())
    }

    /// The cast a DROP CAST names, checked droppable; None when missing with IF EXISTS.
    pub(super) fn cast_to_drop(&self, o: &pb::Node, missing_ok: bool) -> Result<Option<u32>> {
        let tns: Vec<&pb::TypeName> = match inner(o) {
            Some(NodeEnum::List(l)) => l
                .items
                .iter()
                .filter_map(|n| match inner(n) {
                    Some(NodeEnum::TypeName(t)) => Some(t),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let [stn, ttn] = tns.as_slice() else {
            return Ok(None);
        };
        let mut oids = Vec::new();
        for tn in [stn, ttn] {
            match self.type_name_oid(tn) {
                Ok(o) => oids.push(o),
                Err(e) if missing_ok => {
                    let msg = if e.code == code::UNDEFINED_SCHEMA {
                        e.message.replace(" does not exist", " does not exist, skipping")
                    } else {
                        format!(
                            "type \"{}\" does not exist, skipping",
                            names(&tn.names).join(".")
                        )
                    };
                    self.notice(msg);
                    return Ok(None);
                }
                Err(e) => return Err(Error { position: None, ..e }),
            }
        }
        let (src, dst) = (oids[0], oids[1]);
        match self
            .cat
            .casts_all()
            .into_iter()
            .find(|c| c.source == src && c.target == dst)
        {
            Some(c) => {
                if !self.owns(self.type_owner(src)) && !self.owns(self.type_owner(dst)) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!(
                            "must be owner of type {} or type {}",
                            self.type_text(src),
                            self.type_text(dst)
                        ),
                    ));
                }
                Ok(Some(c.oid))
            }
            None if missing_ok => {
                self.notice(format!(
                    "cast from type {} to type {} does not exist, skipping",
                    names(&stn.names).join("."),
                    names(&ttn.names).join(".")
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "cast from type {} to type {} does not exist",
                    self.type_text(src),
                    self.type_text(dst)
                ),
            )),
        }
    }

    /// CREATE ACCESS METHOD.
    pub(super) fn create_am(&mut self, s: &pb::CreateAmStmt) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create access method \"{}\"", s.amname),
            )
            .hint("Must be superuser to create an access method."));
        }
        if self.cat.access_method_by_name(&s.amname).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("access method \"{}\" already exists", s.amname),
            ));
        }
        let parts = names(&s.handler_name);
        if parts.is_empty() {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                "handler function is not specified",
            ));
        }
        let kind = if s.amtype == "t" { 't' } else { 'i' };
        let p = self.func_by_types(&parts, &[INTERNAL])?;
        let expected = if kind == 't' { 269 } else { 325 };
        if p.ret != expected {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "function {} must return type {}",
                    parts.last().unwrap(),
                    if kind == 't' {
                        "table_am_handler"
                    } else {
                        "index_am_handler"
                    }
                ),
            ));
        }
        let oid = self.cat.alloc_oid();
        self.cat.access_methods.insert(
            oid,
            Arc::new(AccessMethod {
                oid,
                name: s.amname.clone(),
                handler: p.oid,
                kind,
            }),
        );
        self.save()?;
        Ok("CREATE ACCESS METHOD".into())
    }

    /// get_table_am_oid.
    pub(super) fn table_am(&self, name: &str) -> Result<u32> {
        match self.cat.access_method_by_name(name) {
            Some(a) if a.kind == 't' => Ok(a.oid),
            Some(_) => Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("access method \"{name}\" is not of type TABLE"),
            )),
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("access method \"{name}\" does not exist"),
            )),
        }
    }

    /// default_table_access_method's access method.
    pub(super) fn default_table_am(&self) -> Result<u32> {
        let v = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("default_table_access_method")
            .unwrap_or_else(|| "heap".into());
        self.table_am(if v.is_empty() { "heap" } else { &v })
    }

    /// The access method a new relation records (DefineRelation): an explicit one, else
    /// (for a partition) its parent's, else the default; None when that's what pg_class
    /// shows anyway (heap, or nothing for a partitioned table).
    pub(super) fn new_rel_am(
        &self,
        explicit: &str,
        partitioned: bool,
        parent: Option<u32>,
    ) -> Result<Option<u32>> {
        let am = if !explicit.is_empty() {
            self.table_am(explicit)?
        } else if let Some(p) = parent.and_then(|p| self.cat.rel_am.get(&p).copied())
            && !partitioned
        {
            p
        } else if partitioned {
            return Ok(parent.and_then(|p| self.cat.rel_am.get(&p).copied()));
        } else {
            self.default_table_am()?
        };
        Ok((partitioned || am != 2).then_some(am))
    }

    /// Record a new relation's access method (after it was made).
    pub(super) fn set_rel_am(&mut self, oid: u32, am: Option<u32>) -> Result<()> {
        match am {
            Some(a) => self.cat.rel_am.insert(oid, a),
            None => self.cat.rel_am.remove(&oid),
        };
        self.save()
    }

    /// CREATE CONVERSION.
    pub(super) fn create_conversion(&mut self, s: &pb::CreateConversionStmt) -> Result<String> {
        let parts = names(&s.conversion_name);
        let (schema, name) = match parts.as_slice() {
            [.., sc, n] => (self.cat.schema_oid(sc)?, crate::catalog::truncate_ident(n)),
            [n] => (
                self.cat.creation_schema(self.search_path())?,
                crate::catalog::truncate_ident(n),
            ),
            [] => return Err(crate::error::internal("conversion name")),
        };
        self.require_schema_create(schema)?;
        let enc = |n: &str, what: &str| {
            crate::types::encnames::encoding_id(n).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("{what} encoding \"{n}\" does not exist"),
                )
            })
        };
        let from = enc(&s.for_encoding_name, "source")?;
        let to = enc(&s.to_encoding_name, "destination")?;
        if from == 0 || to == 0 {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "encoding conversion to or from \"SQL_ASCII\" is not supported",
            ));
        }
        let fparts = names(&s.func_name);
        let p = self.func_by_types(&fparts, &[INT4, INT4, CSTRING, INTERNAL, INT4, BOOL])?;
        if p.ret != INT4 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!(
                    "encoding conversion function {} must return type integer",
                    fparts.join(".")
                ),
            ));
        }
        // The function checks the encodings it was called for (CHECK_ENCODING_CONVERSION_ARGS).
        if let Some(b) = self
            .cat
            .conversions_all()
            .into_iter()
            .find(|c| c.proc == p.oid && c.oid < crate::catalog::FIRST_USER_OID)
        {
            let nm = |e: i32| crate::types::encnames::encoding_name(e as i64).unwrap_or("");
            if b.for_encoding != from {
                return Err(Error::new(
                    code::INTERNAL_ERROR,
                    format!(
                        "expected source encoding \"{}\", but got \"{}\"",
                        nm(b.for_encoding),
                        nm(from)
                    ),
                ));
            }
            if b.to_encoding != to {
                return Err(Error::new(
                    code::INTERNAL_ERROR,
                    format!(
                        "expected destination encoding \"{}\", but got \"{}\"",
                        nm(b.to_encoding),
                        nm(to)
                    ),
                ));
            }
        }
        let all = self.cat.conversions_all();
        if all.iter().any(|c| c.schema == schema && c.name == name) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("conversion \"{name}\" already exists"),
            ));
        }
        if s.def
            && all
                .iter()
                .any(|c| c.schema == schema && c.default && c.for_encoding == from && c.to_encoding == to)
        {
            let nm = |e: i32| crate::types::encnames::encoding_name(e as i64).unwrap_or("");
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "default conversion for {} to {} already exists",
                    nm(from),
                    nm(to)
                ),
            ));
        }
        let oid = self.cat.alloc_oid();
        self.cat.conversions.insert(
            oid,
            Arc::new(Conversion {
                oid,
                name,
                schema,
                owner: self.me(),
                for_encoding: from,
                to_encoding: to,
                proc: p.oid,
                default: s.def,
            }),
        );
        self.save()?;
        Ok("CREATE CONVERSION".into())
    }

    /// CREATE [OR REPLACE] [TRUSTED] LANGUAGE.
    pub(super) fn create_language(&mut self, s: &pb::CreatePLangStmt) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create custom procedural language",
            ));
        }
        let hparts = names(&s.plhandler);
        let h = self.func_by_types(&hparts, &[])?;
        if h.ret != 2280 {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "function {} must return type language_handler",
                    hparts.join(".")
                ),
            ));
        }
        let inline = if s.plinline.is_empty() {
            0
        } else {
            self.func_by_types(&names(&s.plinline), &[INTERNAL])?.oid
        };
        let validator = if s.plvalidator.is_empty() {
            0
        } else {
            self.func_by_types(&names(&s.plvalidator), &[OID])?.oid
        };
        let name = s.plname.clone();
        let existing = self.cat.language_by_name(&name);
        let oid = match &existing {
            Some(l) => {
                if !s.replace {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!("language \"{name}\" already exists"),
                    ));
                }
                if !self.cat.languages.contains_key(&l.oid) || !self.owns(l.owner) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of language {name}"),
                    ));
                }
                l.oid
            }
            None => self.cat.alloc_oid(),
        };
        let owner = existing.map_or_else(|| self.me(), |l| l.owner);
        self.cat.languages.insert(
            oid,
            Arc::new(Language {
                oid,
                name,
                owner,
                trusted: s.pltrusted,
                handler: h.oid,
                inline,
                validator,
            }),
        );
        self.save()?;
        Ok("CREATE LANGUAGE".into())
    }

    /// CREATE [OR REPLACE] TRANSFORM.
    pub(super) fn create_transform(&mut self, s: &pb::CreateTransformStmt) -> Result<String> {
        let Some(tn) = &s.type_name else {
            return Err(crate::error::internal("transform type"));
        };
        let ty = self.type_name_oid(tn).map_err(|e| Error { position: None, ..e })?;
        let shown = names(&tn.names).join(".");
        match typtype(&self.cat, ty) {
            'p' => {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("data type {shown} is a pseudo-type"),
                ));
            }
            'd' => {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("data type {shown} is a domain"),
                ));
            }
            _ => {}
        }
        if !self.owns(self.type_owner(ty)) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of type {}", self.type_text(ty)),
            ));
        }
        let Some(lang) = self.cat.language_by_name(&s.lang) else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("language \"{}\" does not exist", s.lang),
            ));
        };
        let check = |d: &Ddl, p: &super::opclass::ProcInfo| -> Result<()> {
            if let Some(f) = d.cat.functions.get(&p.oid) {
                if f.volatility == 'v' {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "transform function must not be volatile",
                    ));
                }
                if f.procedure || f.agg.is_some() {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "transform function must be a normal function",
                    ));
                }
                if f.returns_set {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "transform function must not return a set",
                    ));
                }
            }
            if p.args.len() != 1 {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "transform function must take one argument",
                ));
            }
            if p.args[0] != INTERNAL {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "first argument of transform function must be type internal",
                ));
            }
            Ok(())
        };
        let fromsql = match &s.fromsql {
            Some(owa) => {
                let p = self.lookup_proc(owa)?;
                if p.ret != INTERNAL {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "return data type of FROM SQL function must be internal",
                    ));
                }
                check(self, &p)?;
                p.oid
            }
            None => 0,
        };
        let tosql = match &s.tosql {
            Some(owa) => {
                let p = self.lookup_proc(owa)?;
                if p.ret != ty {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "return data type of TO SQL function must be the transform data type",
                    ));
                }
                check(self, &p)?;
                p.oid
            }
            None => 0,
        };
        let existing = self
            .cat
            .transforms
            .values()
            .find(|t| t.ty == ty && t.lang == lang.oid)
            .cloned();
        let oid = match existing {
            Some(t) if s.replace => t.oid,
            Some(_) => {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "transform for type {} language \"{}\" already exists",
                        self.type_text(ty),
                        s.lang
                    ),
                ));
            }
            None => self.cat.alloc_oid(),
        };
        self.cat.transforms.insert(
            oid,
            Arc::new(Transform {
                oid,
                ty,
                lang: lang.oid,
                fromsql,
                tosql,
            }),
        );
        self.save()?;
        Ok("CREATE TRANSFORM".into())
    }

    /// The object a DROP ACCESS METHOD / LANGUAGE / CONVERSION / TRANSFORM names, checked
    /// droppable: (pg_depend class, oid); None when missing with IF EXISTS.
    pub(super) fn misc_to_drop(
        &self,
        kind: pb::ObjectType,
        o: &pb::Node,
        missing_ok: bool,
    ) -> Result<Option<(u32, u32, i32)>> {
        use crate::catalog::depend::{AM, CONVERSION, LANGUAGE, TRANSFORM};
        use pb::ObjectType as O;
        let skip = |d: &Ddl, msg: String| -> Result<Option<(u32, u32, i32)>> {
            d.notice(format!("{msg}, skipping"));
            Ok(None)
        };
        match kind {
            O::ObjectAccessMethod => {
                let name = crate::sql::binder::ast::sval(o).unwrap_or_default().to_string();
                match self.cat.access_method_by_name(&name) {
                    Some(a) => {
                        if !self.is_superuser() {
                            return Err(Error::new(
                                code::INSUFFICIENT_PRIVILEGE,
                                format!("must be owner of access method {name}"),
                            ));
                        }
                        Ok(Some((AM, a.oid, 0)))
                    }
                    None if missing_ok => skip(self, format!("access method \"{name}\" does not exist")),
                    None => Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("access method \"{name}\" does not exist"),
                    )),
                }
            }
            O::ObjectLanguage => {
                let name = crate::sql::binder::ast::sval(o).unwrap_or_default().to_string();
                match self.cat.language_by_name(&name) {
                    Some(l) => {
                        if !self.owns(l.owner) {
                            return Err(Error::new(
                                code::INSUFFICIENT_PRIVILEGE,
                                format!("must be owner of language {name}"),
                            ));
                        }
                        Ok(Some((LANGUAGE, l.oid, 0)))
                    }
                    None if missing_ok => skip(self, format!("language \"{name}\" does not exist")),
                    None => Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("language \"{name}\" does not exist"),
                    )),
                }
            }
            O::ObjectConversion => {
                let parts = match inner(o) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let found = match self.cat.find_conversion(&parts, self.search_path()) {
                    Err(e) if missing_ok && e.code == code::UNDEFINED_SCHEMA => {
                        return skip(self, e.message.clone());
                    }
                    r => r?,
                };
                match found {
                    Some(c) => {
                        self.require_owner_or_schema(c.owner, c.schema, "conversion", &c.name)?;
                        Ok(Some((CONVERSION, c.oid, 0)))
                    }
                    None if missing_ok => skip(
                        self,
                        format!("conversion \"{}\" does not exist", parts.join(".")),
                    ),
                    None => Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("conversion \"{}\" does not exist", parts.join(".")),
                    )),
                }
            }
            O::ObjectTransform => {
                let items: Vec<&pb::Node> = match inner(o) {
                    Some(NodeEnum::List(l)) => l.items.iter().collect(),
                    _ => Vec::new(),
                };
                let (Some(Some(NodeEnum::TypeName(tn))), Some(lang)) = (
                    items.first().map(|n| inner(n)),
                    items.get(1).and_then(|n| crate::sql::binder::ast::sval(n)),
                ) else {
                    return Ok(None);
                };
                let ty = match self.type_name_oid(tn) {
                    Ok(t) => t,
                    Err(_) if missing_ok => {
                        return skip(
                            self,
                            format!("type \"{}\" does not exist", names(&tn.names).join(".")),
                        );
                    }
                    Err(e) => return Err(Error { position: None, ..e }),
                };
                let Some(l) = self.cat.language_by_name(lang) else {
                    if missing_ok {
                        return skip(
                            self,
                            format!(
                                "transform for type {} language \"{lang}\" does not exist",
                                names(&tn.names).join(".")
                            ),
                        );
                    }
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("language \"{lang}\" does not exist"),
                    ));
                };
                match self.cat.transforms.values().find(|t| t.ty == ty && t.lang == l.oid) {
                    Some(t) => {
                        if !self.owns(self.type_owner(ty)) {
                            return Err(Error::new(
                                code::INSUFFICIENT_PRIVILEGE,
                                format!("must be owner of type {}", self.type_text(ty)),
                            ));
                        }
                        Ok(Some((TRANSFORM, t.oid, 0)))
                    }
                    None if missing_ok => skip(
                        self,
                        format!(
                            "transform for type {} language \"{lang}\" does not exist",
                            names(&tn.names).join(".")
                        ),
                    ),
                    None => Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "transform for type {} language \"{lang}\" does not exist",
                            self.type_text(ty)
                        ),
                    )),
                }
            }
            _ => Ok(None),
        }
    }

    /// ALTER CONVERSION / LANGUAGE ... RENAME TO.
    pub(super) fn rename_misc(&mut self, kind: pb::ObjectType, s: &pb::RenameStmt) -> Result<String> {
        use pb::ObjectType as O;
        let newname = crate::catalog::truncate_ident(&s.newname);
        match kind {
            O::ObjectConversion => {
                let parts = Self::object_list(s.object.as_deref());
                let c = self.conversion_named(&parts)?;
                if !self.is_superuser() {
                    self.require_owner(c.owner, "conversion", &c.name)?;
                    self.require_schema_create(c.schema)?;
                }
                if self
                    .cat
                    .conversions_all()
                    .iter()
                    .any(|x| x.schema == c.schema && x.name == newname)
                {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "conversion \"{newname}\" already exists in schema \"{}\"",
                            self.cat.schema_name(c.schema)
                        ),
                    ));
                }
                self.update_conversion(c.oid, |x| x.name = newname)?;
                self.save()?;
                Ok("ALTER CONVERSION".into())
            }
            _ => {
                let name = s.subname.clone();
                let name = if name.is_empty() {
                    s.object
                        .as_deref()
                        .and_then(crate::sql::binder::ast::sval)
                        .unwrap_or_default()
                        .to_string()
                } else {
                    name
                };
                let l = self.language_named(&name)?;
                if !self.is_superuser() {
                    self.require_owner(l.owner, "language", &l.name)?;
                }
                if self.cat.language_by_name(&newname).is_some() {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!("language \"{newname}\" already exists"),
                    ));
                }
                self.update_language(l.oid, |x| x.name = newname)?;
                self.save()?;
                Ok("ALTER LANGUAGE".into())
            }
        }
    }

    fn conversion_named(&self, parts: &[String]) -> Result<Conversion> {
        self.cat
            .find_conversion(parts, self.search_path())?
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("conversion \"{}\" does not exist", parts.join(".")),
                )
            })
    }

    fn language_named(&self, name: &str) -> Result<Language> {
        self.cat.language_by_name(name).ok_or_else(|| {
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("language \"{name}\" does not exist"),
            )
        })
    }

    fn update_conversion(&mut self, oid: u32, f: impl FnOnce(&mut Conversion)) -> Result<()> {
        let Some(c) = self.cat.conversions.get(&oid).cloned() else {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied: system conversions cannot be altered",
            ));
        };
        let mut c = (*c).clone();
        f(&mut c);
        self.cat.conversions.insert(oid, Arc::new(c));
        Ok(())
    }

    fn update_language(&mut self, oid: u32, f: impl FnOnce(&mut Language)) -> Result<()> {
        let Some(l) = self.cat.languages.get(&oid).cloned() else {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied: system languages cannot be altered",
            ));
        };
        let mut l = (*l).clone();
        f(&mut l);
        self.cat.languages.insert(oid, Arc::new(l));
        Ok(())
    }

    /// ALTER CONVERSION / LANGUAGE ... OWNER TO.
    pub(super) fn owner_misc(&mut self, kind: pb::ObjectType, obj: Option<&pb::Node>, new: u32) -> Result<()> {
        use pb::ObjectType as O;
        if kind == O::ObjectConversion {
            let c = self.conversion_named(&Self::object_list(obj))?;
            if c.owner == new {
                return Ok(());
            }
            if !self.is_superuser() {
                self.require_owner(c.owner, "conversion", &c.name)?;
                self.check_new_owner(new, Some(c.schema))?;
            }
            return self.update_conversion(c.oid, |x| x.owner = new);
        }
        let name = obj
            .and_then(crate::sql::binder::ast::sval)
            .unwrap_or_default()
            .to_string();
        let l = self.language_named(&name)?;
        if l.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.require_owner(l.owner, "language", &l.name)?;
            self.check_new_owner(new, None)?;
        }
        self.update_language(l.oid, |x| x.owner = new)
    }

    /// ALTER CONVERSION ... SET SCHEMA.
    pub(super) fn set_schema_conversion(&mut self, s: &pb::AlterObjectSchemaStmt) -> Result<String> {
        let c = self.conversion_named(&Self::object_list(s.object.as_deref()))?;
        let nsp = self.cat.schema_oid(&s.newschema)?;
        self.check_set_namespace(c.schema, nsp)?;
        if c.schema == nsp {
            return Ok("ALTER CONVERSION".into());
        }
        if !self.is_superuser() {
            self.require_owner(c.owner, "conversion", &c.name)?;
            self.require_schema_create(nsp)?;
        }
        if self
            .cat
            .conversions_all()
            .iter()
            .any(|x| x.schema == nsp && x.name == c.name)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "conversion \"{}\" already exists in schema \"{}\"",
                    c.name,
                    self.cat.schema_name(nsp)
                ),
            ));
        }
        self.update_conversion(c.oid, |x| x.schema = nsp)?;
        self.save()?;
        Ok("ALTER CONVERSION".into())
    }
}
