//! The fastpath function call protocol (FunctionCall message, Postgres's fastpath.c): a
//! function called by oid with raw arguments, as libpq's lo_* client functions do.

use super::{Outcome, Prepared, Session, TxnStatus};
use crate::catalog::acl::{EXECUTE, ObjKind, USAGE};
use crate::catalog::{Catalog, PG_CATALOG_OID, quote_ident};
use crate::error::{Error, Result, code};
use crate::types::{DataType, Value};
use parking_lot::Mutex;
use std::sync::Arc;

/// fetch_fp_info: what calling a function by oid needs.
pub struct FpInfo {
    oid: u32,
    schema: u32,
    name: String,
    /// Argument types, with the domain (if any) each was declared as.
    args: Vec<(DataType, Option<u32>)>,
    ret: DataType,
    strict: bool,
    /// A built-in (else a user function, in this language).
    language: Option<String>,
}

impl FpInfo {
    pub fn nargs(&self) -> usize {
        self.args.len()
    }
}

fn cannot_call(name: &str) -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        format!("cannot call function \"{name}\" via fastpath interface"),
    )
}

fn lookup(cat: &Catalog, oid: u32) -> Result<FpInfo> {
    if let Some(f) = cat.functions.get(&oid) {
        if f.procedure || f.agg.is_some() || f.returns_set {
            return Err(cannot_call(&f.name));
        }
        let args = f
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| (a.1.clone(), f.arg_domains.get(i).copied().flatten()))
            .collect();
        return Ok(FpInfo {
            oid,
            schema: f.schema,
            name: f.name.clone(),
            args,
            ret: f.ret.clone(),
            strict: f.strict,
            language: Some(f.language.clone()),
        });
    }
    if let Some(d) = crate::sql::func::def_by_oid(oid) {
        if d.srf.is_some() {
            return Err(cannot_call(d.name));
        }
        let args: Vec<(DataType, Option<u32>)> = d.args.iter().map(|t| (t.clone(), None)).collect();
        let ret = match &d.ret {
            crate::sql::func::Ret::Fixed(t) => t.clone(),
            crate::sql::func::Ret::Arg(i) => d.args.get(*i).cloned().unwrap_or(DataType::Unknown),
            crate::sql::func::Ret::Poly => DataType::AnyElement,
            crate::sql::func::Ret::PolyArray => DataType::AnyArray,
        };
        return Ok(FpInfo {
            oid,
            schema: PG_CATALOG_OID,
            name: d.name.to_string(),
            args,
            ret,
            strict: d.strict,
            language: None,
        });
    }
    if let Some(d) = crate::exec::agg::agg_registry()
        .by_name
        .values()
        .flatten()
        .find(|a| a.sig.oid == oid)
    {
        return Err(cannot_call(d.sig.name));
    }
    // The constructors of user range types and their multiranges.
    for r in cat.ranges.values().filter(|r| r.fn_oids.len() == 6) {
        let Some(i) = r.fn_oids.iter().take(5).position(|o| *o == oid) else {
            continue;
        };
        let range = DataType::Range(r.range_type());
        let st = r.subtype.clone();
        let (ctor, multi) = if r.ctors.0.is_empty() {
            (&r.name, &r.multi_name)
        } else {
            (&r.ctors.0, &r.ctors.1)
        };
        let multirange = DataType::Multirange(r.range_type());
        let (name, args, ret) = match i {
            0 => (ctor, vec![st.clone(), st], range),
            1 => (ctor, vec![st.clone(), st, DataType::Text], range),
            2 => (multi, vec![], multirange),
            3 => (multi, vec![range], multirange),
            _ => (multi, vec![DataType::array_of(range)], multirange),
        };
        return Ok(FpInfo {
            oid,
            schema: r.schema,
            name: name.clone(),
            args: args.into_iter().map(|t| (t, None)).collect(),
            ret,
            strict: i != 1,
            language: Some("internal".into()),
        });
    }
    Err(Error::new(
        code::UNDEFINED_FUNCTION,
        format!("function with OID {oid} does not exist"),
    ))
}

/// Fastpath calls pass no collation: what a collation-dependent built-in can't then do.
fn needs_collation(f: &FpInfo) -> Option<String> {
    let collatable = |t: &DataType| {
        matches!(
            t,
            DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) | DataType::Name
        )
    };
    if f.language.is_some() || !f.args.iter().any(|a| collatable(&a.0)) {
        return None;
    }
    let n = f.name.as_str();
    Some(match n {
        "lower" | "casefold" => "lower() function".into(),
        "upper" | "initcap" => format!("{n}() function"),
        "strpos" | "position" | "replace" | "split_part" | "starts_with" | "bttextcmp"
        | "btnamecmp" | "btnametextcmp" | "bttextnamecmp" | "bpcharcmp" | "text_larger"
        | "text_smaller" | "bpchar_larger" | "bpchar_smaller" => "string comparison".into(),
        _ if ["text", "bpchar", "name"].iter().any(|p| {
            n.strip_prefix(p).is_some_and(|op| {
                matches!(
                    op,
                    "eq" | "ne"
                        | "lt"
                        | "le"
                        | "gt"
                        | "ge"
                        | "_lt"
                        | "_le"
                        | "_gt"
                        | "_ge"
                        | "eqtext"
                        | "netext"
                        | "lttext"
                        | "letext"
                        | "gttext"
                        | "getext"
                        | "eqname"
                        | "nename"
                        | "ltname"
                        | "lename"
                        | "gtname"
                        | "gename"
                )
            })
        }) =>
        {
            "string comparison".into()
        }
        "textlike" | "textnlike" | "bpcharlike" | "bpcharnlike" | "namelike" | "namenlike"
        | "like" | "notlike" => "LIKE".into(),
        "texticlike" | "texticnlike" | "bpchariclike" | "bpcharicnlike" | "nameiclike"
        | "nameicnlike" => "ILIKE".into(),
        "hashtext" | "hashtextextended" | "hashbpchar" | "hashbpcharextended" => {
            "string hashing".into()
        }
        "substring" | "textregexsubstr" if f.args.get(1).is_some_and(|a| collatable(&a.0)) => {
            "regular expression".into()
        }
        _ if n.starts_with("regexp_") || n.contains("regexeq") || n.contains("regexne") => {
            "regular expression".into()
        }
        _ => return None,
    })
}

/// The pseudo-types' input and receive functions refuse every value.
fn pseudo(t: &DataType) -> bool {
    use DataType as T;
    matches!(
        t,
        T::Any
            | T::AnyElement
            | T::AnyArray
            | T::AnyNonArray
            | T::AnyEnum
            | T::AnyRange
            | T::AnyMultirange
            | T::AnyCompatible
            | T::AnyCompatibleNonArray
            | T::AnyCompatibleArray
            | T::AnyCompatibleRange
            | T::AnyCompatibleMultirange
            | T::Trigger
            | T::Void
            | T::Record
    ) || matches!(t, T::Array(e) if matches!(**e, T::Any))
}

impl Session {
    /// The function a FunctionCall message names, once the caller may run it.
    pub fn fastpath_function(&mut self, oid: u32) -> Result<FpInfo> {
        let r = self.fastpath_function_inner(oid);
        if r.is_err() {
            self.on_error();
        }
        r
    }

    fn fastpath_function_inner(&mut self, oid: u32) -> Result<FpInfo> {
        if self.status == TxnStatus::Failed {
            return Err(Error::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let cat = self.state.db.catalog();
        let f = lookup(&cat, oid)?;
        // No name lookup happened, so the schema's USAGE is checked too.
        let (me, db_owner) = (self.state.current_role(&cat), self.state.db_owner(&cat));
        if cat.obj_mask(ObjKind::Schema, f.schema, me, db_owner).0 & USAGE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for schema {}", cat.schema_name(f.schema)),
            ));
        }
        if cat.obj_mask(ObjKind::Function, f.oid, me, db_owner).0 & EXECUTE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for function {}", f.name),
            ));
        }
        Ok(f)
    }

    /// Call `f` with the message's arguments (each in its format: 0 text, 1 binary) and return
    /// its result in `rformat` (None: NULL).
    pub fn fastpath_call(
        &mut self,
        f: &FpInfo,
        formats: &[i16],
        args: Vec<Option<Vec<u8>>>,
        rformat: i16,
    ) -> Result<Option<Vec<u8>>> {
        let r = self.fastpath_call_inner(f, formats, args, rformat);
        if r.is_err() {
            self.on_error();
        }
        let ended = self.end_batch();
        let out = r?;
        ended?;
        Ok(out)
    }

    fn fastpath_call_inner(
        &mut self,
        f: &FpInfo,
        formats: &[i16],
        args: Vec<Option<Vec<u8>>>,
        rformat: i16,
    ) -> Result<Option<Vec<u8>>> {
        let protocol = |m: String| Error::new(code::PROTOCOL_VIOLATION, m);
        if args.len() != f.nargs() {
            return Err(protocol(format!(
                "function call message contains {} arguments but function requires {}",
                args.len(),
                f.nargs()
            )));
        }
        if formats.len() > 1 && formats.len() != args.len() {
            return Err(protocol(format!(
                "function call message contains {} argument formats but {} arguments",
                formats.len(),
                args.len()
            )));
        }
        let fmt = self.state.settings.borrow().format_ctx();
        let mut values = Vec::with_capacity(args.len());
        let mut any_null = false;
        for (i, raw) in args.into_iter().enumerate() {
            let aformat = match formats.len() {
                0 => 0,
                1 => formats[0],
                _ => formats[i],
            };
            let ty = &f.args[i].0;
            if !matches!(aformat, 0 | 1) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("unsupported format code: {aformat}"),
                ));
            }
            let Some(raw) = raw else {
                any_null = true;
                values.push(Value::Null);
                continue;
            };
            if pseudo(ty) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot accept a value of type {}",
                        match ty {
                            DataType::Array(_) => "any".to_string(),
                            t => t.typname(),
                        }
                    ),
                ));
            }
            values.push(if aformat == 1 {
                crate::types::binary::decode(&raw, ty, &fmt).map_err(|e| match e.code {
                    code::INVALID_BINARY_REPRESENTATION => Error::new(
                        code::INVALID_BINARY_REPRESENTATION,
                        format!(
                            "incorrect binary data format in function argument {}",
                            i + 1
                        ),
                    ),
                    _ => e,
                })?
            } else {
                let s = std::str::from_utf8(&raw).map_err(|_| {
                    Error::new(
                        code::CHARACTER_NOT_IN_REPERTOIRE,
                        "invalid byte sequence for encoding \"UTF8\"",
                    )
                })?;
                crate::types::text::parse_value(s, ty, &fmt)?
            });
        }
        // A strict function isn't called with a NULL argument.
        if f.strict && any_null {
            return Ok(None);
        }
        if let Some(what) = needs_collation(f) {
            return Err(Error::new(
                code::INDETERMINATE_COLLATION,
                format!("could not determine which collation to use for {what}"),
            )
            .hint("Use the COLLATE clause to set the collation explicitly."));
        }
        // These find their arguments' types from the call expression, which there isn't.
        if f.language.is_none() && matches!(f.name.as_str(), "concat" | "concat_ws") {
            return Err(crate::error::internal(
                "could not determine data type of concat() input",
            ));
        }
        let cat = self.state.db.catalog();
        let casts: Vec<String> = f
            .args
            .iter()
            .enumerate()
            .map(|(i, (t, dom))| format!("${}::{}", i + 1, cat.format_type(t, *dom, &[])))
            .collect();
        let sql = format!(
            "SELECT {}.{}({})",
            quote_ident(cat.schema_name(f.schema)),
            quote_ident(&f.name),
            casts.join(", ")
        );
        let stmt = crate::sql::parse::parse(&sql)?
            .into_iter()
            .next()
            .and_then(|s| s.stmt.map(|n| *n));
        let p = Arc::new(Prepared {
            sql,
            offset: 0,
            stmt,
            param_types: f.args.iter().map(|a| a.0.clone()).collect(),
            bound: Mutex::new(None),
            plans: Default::default(),
            lit: None,
        });
        let Outcome::Done(r) = self.execute_with(&p, &values, 0)? else {
            return Err(crate::error::internal("fastpath call returned no rows"));
        };
        let v = r
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null);
        let ty = r
            .cols
            .as_ref()
            .and_then(|c| c.first())
            .map_or(f.ret.clone(), |c| c.ty.clone());
        if v.is_null() {
            // void's output is empty, not NULL; an SQL function's void result is NULL.
            return Ok(
                if f.ret == DataType::Void && f.language.as_deref() != Some("sql") {
                    Some(Vec::new())
                } else {
                    None
                },
            );
        }
        match rformat {
            0 => Ok(Some(
                crate::types::text::format_value(&v, &ty, &fmt).into_bytes(),
            )),
            1 => {
                let mut out = bytes::BytesMut::new();
                crate::types::binary::encode(&v, &ty, &fmt, &mut out)?;
                Ok(Some(out.to_vec()))
            }
            _ => Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unsupported format code: {rformat}"),
            )),
        }
    }
}
