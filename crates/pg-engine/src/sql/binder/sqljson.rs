//! SQL/JSON expressions (parse_expr.c's transformJson*): constructors, IS JSON and the query
//! functions become calls of `func::sqljson`'s definitions; JSON_ARRAYAGG / JSON_OBJECTAGG
//! become the json[b]_agg / json[b]_object_agg aggregates.

use super::Binder;
use super::ast::{self, inner, mk, with_pos};
use crate::error::{Error, Result, code};
use crate::sql::expr::Expr;
use crate::sql::func::sqljson::{self as sj, Behavior, FuncOpts, Op, Wrapper};
use crate::sql::plan::{OutCol, Plan, SrfCall};
use crate::types::cast::{CastContext, cast_context};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};

fn flag(v: i64) -> Expr {
    Expr::Const(Value::Int8(v), DataType::Int8)
}

/// transformJsonConstructorOutput: without RETURNING, a constructor with a jsonb argument
/// returns jsonb.
fn derived_returning(out: Option<&pb::JsonOutput>, ret: DataType, args: &[Expr]) -> DataType {
    if out.and_then(|o| o.type_name.as_ref()).is_none()
        && args.iter().any(|a| matches!(a.ty(), DataType::Jsonb))
    {
        return DataType::Jsonb;
    }
    ret
}

fn wrapper_of(w: i32) -> Wrapper {
    match pb::JsonWrapper::try_from(w).unwrap_or(pb::JsonWrapper::JswUnspec) {
        pb::JsonWrapper::JswNone => Wrapper::None,
        pb::JsonWrapper::JswConditional => Wrapper::Conditional,
        pb::JsonWrapper::JswUnconditional => Wrapper::Unconditional,
        _ => Wrapper::Unspec,
    }
}

/// The QUOTES and ON EMPTY / ON ERROR clauses a query function (or JSON_TABLE column
/// `column`) allows (transformJsonFuncExpr).
fn check_behaviors(
    op: Op,
    column: Option<&str>,
    wrapper: Wrapper,
    quotes: pb::JsonQuotes,
    on_empty: Option<&pb::JsonBehavior>,
    on_error: Option<&pb::JsonBehavior>,
    location: i32,
) -> Result<()> {
    use Behavior as B;
    if op == Op::Query
        && quotes == pb::JsonQuotes::JsQuotesOmit
        && matches!(wrapper, Wrapper::Conditional | Wrapper::Unconditional)
    {
        return Err(with_pos(
            Error::new(
                code::SYNTAX_ERROR,
                "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used",
            ),
            location,
        ));
    }
    let bad = |clause: &str, b: &pb::JsonBehavior, allowed: &str, func: &str, cols: &str| {
        let e = match column {
            None => Error::new(code::SYNTAX_ERROR, format!("invalid {clause} behavior"))
                .detail(format!("Only {allowed} is allowed in {clause} for {func}.")),
            Some(c) => Error::new(
                code::SYNTAX_ERROR,
                format!("invalid {clause} behavior for column \"{c}\""),
            )
            .detail(format!("Only {allowed} is allowed in {clause} for {cols}.")),
        };
        Err(with_pos(e, b.location))
    };
    let kind = |b: &pb::JsonBehavior| B::from_i64(b.btype as i64);
    for (clause, b) in [("ON EMPTY", on_empty), ("ON ERROR", on_error)] {
        let Some(b) = b else {
            continue;
        };
        match op {
            Op::Query
                if !matches!(
                    kind(b),
                    B::Error | B::Null | B::Empty | B::EmptyArray | B::EmptyObject | B::Default
                ) =>
            {
                return bad(
                    clause,
                    b,
                    "ERROR, NULL, EMPTY ARRAY, EMPTY OBJECT, or DEFAULT expression",
                    "JSON_QUERY()",
                    "formatted columns",
                );
            }
            Op::Exists
                if clause == "ON ERROR"
                    && !matches!(kind(b), B::Error | B::True | B::False | B::Unknown) =>
            {
                return bad(
                    clause,
                    b,
                    "ERROR, TRUE, FALSE, or UNKNOWN",
                    "JSON_EXISTS()",
                    "EXISTS columns",
                );
            }
            Op::Value if !matches!(kind(b), B::Error | B::Null | B::Default) => {
                return bad(
                    clause,
                    b,
                    "ERROR, NULL, or DEFAULT expression",
                    "JSON_VALUE()",
                    "scalar columns",
                );
            }
            _ => {}
        }
    }
    Ok(())
}

impl Binder<'_, '_> {
    /// RETURNING's type, or `default`. Anything but json / jsonb needs a cast from json.
    fn json_returning(
        &mut self,
        out: Option<&pb::JsonOutput>,
        default: DataType,
    ) -> Result<DataType> {
        Ok(self.json_returning_domain(out, default)?.0)
    }

    /// `json_returning`, with the domain RETURNING names (checkJsonOutputFormat's checks of
    /// its FORMAT clause).
    fn json_returning_domain(
        &mut self,
        out: Option<&pb::JsonOutput>,
        default: DataType,
    ) -> Result<(DataType, Option<u32>)> {
        let Some(tn) = out.and_then(|o| o.type_name.as_ref()) else {
            return Ok((default, None));
        };
        let (ty, domain) = super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?;
        if let Some(fmt) = out
            .and_then(|o| o.returning.as_ref())
            .and_then(|r| r.format.as_ref())
            && fmt.format_type == pb::JsonFormatType::JsFormatJson as i32
        {
            let enc = pb::JsonEncoding::try_from(fmt.encoding).unwrap_or(pb::JsonEncoding::Undefined);
            let given = !matches!(enc, pb::JsonEncoding::JsEncDefault | pb::JsonEncoding::Undefined);
            if !matches!(ty, DataType::Bytea) && given {
                return Err(with_pos(
                    Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot set JSON encoding for non-bytea output types",
                    ),
                    fmt.location,
                ));
            }
            if given && enc != pb::JsonEncoding::JsEncUtf8 {
                return Err(with_pos(
                    Error::new(code::FEATURE_NOT_SUPPORTED, "unsupported JSON encoding")
                        .hint("Only UTF8 JSON encoding is supported."),
                    fmt.location,
                ));
            }
        }
        if !matches!(ty, DataType::Json | DataType::Jsonb | DataType::Bytea)
            && cast_context(&DataType::Json, &ty).is_none()
        {
            return Err(Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type json to {}", ty.display()),
            ));
        }
        Ok((ty, domain))
    }

    /// A constructor's result checked against its RETURNING domain.
    fn returning_domain_check(&mut self, e: Expr, domain: Option<u32>) -> Result<Expr> {
        match domain {
            Some(d) => self.domain_check(e, d),
            None => Ok(e),
        }
    }

    /// A value (`expr [FORMAT JSON]`): unknown literals are text; FORMAT JSON reads text as json.
    fn json_value_expr(&mut self, v: &pb::JsonValueExpr) -> Result<Expr> {
        self.json_value_expr_as(v, false)
    }

    /// `json_value_expr`; `default_json`: the context's default format is JSON
    /// (JSON_SERIALIZE's).
    fn json_value_expr_as(&mut self, v: &pb::JsonValueExpr, default_json: bool) -> Result<Expr> {
        let n = v.raw_expr.as_deref().unwrap();
        let e = self.bind_expr(n)?;
        let format_json = v
            .format
            .as_ref()
            .is_some_and(|f| f.format_type == pb::JsonFormatType::JsFormatJson as i32);
        let ty = e.ty();
        // A literal read as json is folded into a json constant when planned (a view shows
        // the text it reads).
        if (format_json || default_json)
            && !self.deparse
            && matches!(ty, DataType::Unknown)
            && matches!(e, Expr::Const(..))
        {
            // (Postgres reads it when simplifying the expression: errors have no position.)
            return self
                .coerce_implicit(e, &DataType::Json)
                .map_err(|e| crate::error::Error { position: None, ..e });
        }
        if format_json
            && matches!(
                ty,
                DataType::Unknown | DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_)
            )
        {
            // Read as json when evaluated (a view shows `text FORMAT JSON`).
            let e = self.coerce_implicit(e, &DataType::Text)?;
            return Ok(Expr::Cast(
                Box::new(e),
                DataType::Text,
                DataType::Json,
                false,
            ));
        }
        if format_json && matches!(ty, DataType::Bytea) {
            // Encoded JSON text (makeJsonByteaToTextConversion).
            let written = v
                .format
                .as_ref()
                .and_then(|f| pb::JsonEncoding::try_from(f.encoding).ok());
            let enc = match written {
                Some(pb::JsonEncoding::JsEncUtf16) => "UTF16",
                Some(pb::JsonEncoding::JsEncUtf32) => "UTF32",
                _ => "UTF8",
            };
            // (A written ENCODING is a `name`, the default `text`: definitions show it so.)
            let enc_ty = match written {
                Some(pb::JsonEncoding::JsEncDefault | pb::JsonEncoding::Undefined) | None => {
                    DataType::Text
                }
                _ => DataType::Name,
            };
            let r = crate::sql::func::resolve_function(
                "convert_from",
                &[DataType::Bytea, DataType::Name],
            )?;
            let text = Expr::Call(
                r.def,
                vec![e, Expr::Const(Value::Text(enc.into()), enc_ty)],
                DataType::Text,
            );
            return Ok(Expr::Cast(
                Box::new(text),
                DataType::Text,
                DataType::Json,
                false,
            ));
        }
        if matches!(ty, DataType::Unknown) {
            return self.coerce_implicit(e, &DataType::Text);
        }
        Ok(e)
    }

    /// A constructor's or JSON_SERIALIZE's argument (transformJsonValueExpr's checks of its
    /// FORMAT clause): ENCODING only for bytea, FORMAT JSON only for strings and bytea.
    fn json_value_checked(&mut self, v: &pb::JsonValueExpr) -> Result<Expr> {
        self.json_value_checked_as(v, false)
    }

    fn json_value_checked_as(&mut self, v: &pb::JsonValueExpr, default_json: bool) -> Result<Expr> {
        if let Some(fmt) = v.format.as_ref()
            && fmt.format_type == pb::JsonFormatType::JsFormatJson as i32
        {
            let n = v.raw_expr.as_deref().unwrap();
            let e = self.bind_expr(n)?;
            let ty = e.ty();
            if !matches!(
                pb::JsonEncoding::try_from(fmt.encoding).unwrap_or(pb::JsonEncoding::Undefined),
                pb::JsonEncoding::JsEncDefault | pb::JsonEncoding::Undefined
            ) && !matches!(ty, DataType::Bytea)
            {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        "JSON ENCODING clause is only allowed for bytea input type",
                    ),
                    fmt.location,
                ));
            }
            if !matches!(ty, DataType::Json | DataType::Jsonb | DataType::Bytea)
                && !matches!(
                    ty.category(),
                    crate::types::Category::String | crate::types::Category::Unknown
                )
            {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        "cannot use non-string types with explicit FORMAT JSON clause",
                    ),
                    if fmt.location >= 0 {
                        fmt.location
                    } else {
                        ast::location(n)
                    },
                ));
            }
        }
        self.json_value_expr_as(v, default_json)
    }

    /// A PASSING value (transformJsonPassingArgs, format JSONB): json is read as jsonb.
    fn json_passing_expr(&mut self, v: &pb::JsonValueExpr) -> Result<Expr> {
        let e = self.json_value_expr(v)?;
        if matches!(e.ty(), DataType::Json) {
            let c = self.coerce_with(e, &DataType::Jsonb, CastContext::Explicit, &|from| {
                Error::new(
                    code::CANNOT_COERCE,
                    format!("cannot cast type {} to jsonb", from.display()),
                )
            })?;
            // An implicit-format coercion: definitions show the json value.
            return Ok(match c {
                Expr::Cast(x, f, t, true) => Expr::Cast(x, f, t, false),
                c => c,
            });
        }
        Ok(e)
    }

    pub(crate) fn bind_sqljson(&mut self, n: &NodeEnum) -> Result<Expr> {
        match n {
            NodeEnum::JsonObjectConstructor(c) => {
                let (ret, domain) = self.json_returning_domain(c.output.as_ref(), DataType::Json)?;
                let mut args = vec![flag(
                    if c.absent_on_null {
                        sj::ABSENT_ON_NULL
                    } else {
                        0
                    } | if c.unique { sj::UNIQUE_KEYS } else { 0 },
                )];
                for kv in &c.exprs {
                    let Some(NodeEnum::JsonKeyValue(kv)) = inner(kv) else {
                        continue;
                    };
                    let k = self.bind_expr(kv.key.as_deref().unwrap())?;
                    let k = if matches!(k.ty(), DataType::Unknown) {
                        self.coerce_implicit(k, &DataType::Text)?
                    } else {
                        k
                    };
                    args.push(k);
                    args.push(self.json_value_checked(kv.value.as_deref().unwrap())?);
                }
                let ret = derived_returning(c.output.as_ref(), ret, &args[1..]);
                let e = Expr::Call(sj::def("JSON_OBJECT"), args, ret);
                self.returning_domain_check(e, domain)
            }
            NodeEnum::JsonArrayConstructor(c) => {
                let (ret, domain) = self.json_returning_domain(c.output.as_ref(), DataType::Json)?;
                let mut args = vec![flag(if c.absent_on_null {
                    sj::ABSENT_ON_NULL
                } else {
                    0
                })];
                for v in &c.exprs {
                    let Some(NodeEnum::JsonValueExpr(v)) = inner(v) else {
                        continue;
                    };
                    args.push(self.json_value_checked(v)?);
                }
                let ret = derived_returning(c.output.as_ref(), ret, &args[1..]);
                let e = Expr::Call(sj::def("JSON_ARRAY"), args, ret);
                self.returning_domain_check(e, domain)
            }
            NodeEnum::JsonArrayQueryConstructor(c) => self.json_array_query(c),
            NodeEnum::JsonArrayAgg(a) => {
                let ctor = a.constructor.as_deref().unwrap();
                let ret = self.json_returning(ctor.output.as_ref(), DataType::Json)?;
                let binary = matches!(ret, DataType::Jsonb);
                let name = format!(
                    "{}_agg{}",
                    if binary { "jsonb" } else { "json" },
                    if a.absent_on_null { "_strict" } else { "" }
                );
                let jv = a.arg.as_deref().unwrap();
                let (arg, fmt) = self.json_agg_value(jv);
                let e = self.json_agg_call(&name, vec![arg], ctor, ret)?;
                self.json_agg_formatted(&e, 0, fmt);
                Ok(e)
            }
            NodeEnum::JsonObjectAgg(a) => {
                let ctor = a.constructor.as_deref().unwrap();
                let ret = self.json_returning(ctor.output.as_ref(), DataType::Json)?;
                let binary = matches!(ret, DataType::Jsonb);
                let name = format!(
                    "{}_object_agg{}{}",
                    if binary { "jsonb" } else { "json" },
                    if a.unique { "_unique" } else { "" },
                    if a.absent_on_null { "_strict" } else { "" }
                );
                let kv = a.arg.as_deref().unwrap();
                let k = kv.key.as_deref().unwrap().clone();
                let (v, fmt) = self.json_agg_value(kv.value.as_deref().unwrap());
                let e = self.json_agg_call(&name, vec![k, v], ctor, ret)?;
                self.json_agg_formatted(&e, 1, fmt);
                Ok(e)
            }
            NodeEnum::JsonParseExpr(p) => {
                let ret = self.json_returning(p.output.as_ref(), DataType::Json)?;
                let v = p.expr.as_deref().unwrap();
                let n = v.raw_expr.as_deref().unwrap();
                let format_json = v
                    .format
                    .as_ref()
                    .is_some_and(|f| f.format_type == pb::JsonFormatType::JsFormatJson as i32);
                let e = if format_json {
                    match self.json_value_checked(v)? {
                        // With the unique-keys check a literal stays text read as json.
                        Expr::Const(Value::Json(s), DataType::Json) if p.unique_keys => Expr::Cast(
                            Box::new(Expr::Const(Value::Text(s), DataType::Text)),
                            DataType::Text,
                            DataType::Json,
                            false,
                        ),
                        e => e,
                    }
                } else {
                    self.bind_expr(n)?
                };
                let e = match e.ty() {
                    DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) => e,
                    t if t.category() == crate::types::Category::String => {
                        self.string_to_text(e, ast::location(n))?
                    }
                    _ => e,
                };
                if p.unique_keys
                    && !matches!(
                        e.ty(),
                        DataType::Unknown
                            | DataType::Text
                            | DataType::Varchar(_)
                            | DataType::Bpchar(_)
                            | DataType::Bytea
                    )
                    && !matches!(e, Expr::Cast(_, DataType::Text, DataType::Json, false))
                {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            "cannot use non-string types with WITH UNIQUE KEYS clause",
                        ),
                        p.location,
                    ));
                }
                let e = match e.ty() {
                    // A literal is cast to json while parsing (the unique-keys check reads
                    // it as text).
                    // (Read when the expression is simplified: errors have no position.)
                    DataType::Unknown
                        if !p.unique_keys && !self.deparse && matches!(e, Expr::Const(..)) =>
                    {
                        self
                        .coerce_implicit(e, &DataType::Json)
                            .map_err(|e| Error { position: None, ..e })?
                    }
                    DataType::Unknown => self.coerce_implicit(e, &DataType::Text)?,
                    DataType::Jsonb => self.coerce_with(e, &DataType::Json, CastContext::Explicit, &|from| {
                        Error::new(code::CANNOT_COERCE, format!("cannot cast type {} to json", from.display()))
                    })?,
                    DataType::Text
                    | DataType::Varchar(_)
                    | DataType::Bpchar(_)
                    | DataType::Bytea
                    | DataType::Json => e,
                    t => {
                        return Err(with_pos(
                            Error::new(
                                code::CANNOT_COERCE,
                                format!("cannot cast type {} to json", t.display()),
                            ),
                            ast::location(n),
                        ));
                    }
                };
                Ok(Expr::Call(
                    sj::def("JSON"),
                    vec![flag(if p.unique_keys { sj::UNIQUE_KEYS } else { 0 }), e],
                    ret,
                ))
            }
            NodeEnum::JsonScalarExpr(s) => {
                let ret = self.json_returning(s.output.as_ref(), DataType::Json)?;
                let e = self.bind_expr(s.expr.as_deref().unwrap())?;
                let e = if matches!(e.ty(), DataType::Unknown) {
                    self.coerce_implicit(e, &DataType::Text)?
                } else {
                    e
                };
                Ok(Expr::Call(sj::def("JSON_SCALAR"), vec![e], ret))
            }
            NodeEnum::JsonSerializeExpr(s) => {
                let ret = match s.output.as_ref().and_then(|o| o.type_name.as_ref()) {
                    Some(tn) => super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?.0,
                    None => DataType::Text,
                };
                if !matches!(ret, DataType::Bytea)
                    && ret.category() != crate::types::Category::String
                {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "cannot use type {} in RETURNING clause of JSON_SERIALIZE()",
                            ret.without_typmod().display()
                        ),
                    )
                    .hint("Try returning a string type or bytea."));
                }
                let domain = match s.output.as_ref().and_then(|o| o.type_name.as_ref()) {
                    Some(tn) => super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?.1,
                    None => None,
                };
                let v = s.expr.as_deref().unwrap();
                let e = self.json_value_checked_as(v, true)?;
                let e = Expr::Call(sj::def("JSON_SERIALIZE"), vec![e], ret);
                self.returning_domain_check(e, domain)
            }
            NodeEnum::JsonIsPredicate(p) => {
                let n = p.expr.as_deref().unwrap();
                let e = self.bind_expr(n)?;
                let e = match e.ty() {
                    DataType::Unknown => self.coerce_implicit(e, &DataType::Text)?,
                    DataType::Text
                    | DataType::Varchar(_)
                    | DataType::Bpchar(_)
                    | DataType::Bytea
                    | DataType::Json
                    | DataType::Jsonb => e,
                    // Another string type goes to text (transformJsonParseArg).
                    t if t.category() == crate::types::Category::String => {
                        self.string_to_text(e, ast::location(n))?
                    }
                    t => {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            format!("cannot use type {} in IS JSON predicate", t.display()),
                        ));
                    }
                };
                let kind = match pb::JsonValueType::try_from(p.item_type)
                    .unwrap_or(pb::JsonValueType::JsTypeAny)
                {
                    pb::JsonValueType::JsTypeObject => sj::IS_OBJECT,
                    pb::JsonValueType::JsTypeArray => sj::IS_ARRAY,
                    pb::JsonValueType::JsTypeScalar => sj::IS_SCALAR,
                    _ => sj::IS_ANY,
                };
                Ok(Expr::Call(
                    sj::def("IS JSON"),
                    vec![flag(kind | if p.unique_keys { 4 } else { 0 }), e],
                    DataType::Bool,
                ))
            }
            NodeEnum::JsonFuncExpr(f) => self.json_func(f),
            _ => Err(crate::error::unsupported("this SQL/JSON expression")),
        }
    }

    /// A string-category input as text: an implicit cast, or "cannot cast" at `loc`.
    fn string_to_text(&mut self, e: Expr, loc: i32) -> Result<Expr> {
        self.coerce_with(e, &DataType::Text, CastContext::Implicit, &|from| {
            Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type {} to text", from.display()),
            )
        })
        .map_err(|err| if err.position.is_none() { with_pos(err, loc) } else { err })
    }

    /// An aggregate's `expr [FORMAT JSON]` argument as SQL: a string read as json, bytea
    /// through convert_from. Also whether it is read so (and for bytea, whether its ENCODING
    /// was written).
    fn json_agg_value(&mut self, jv: &pb::JsonValueExpr) -> (Node, Option<bool>) {
        let arg = jv.raw_expr.as_deref().unwrap().clone();
        let Some(f) = jv
            .format
            .as_ref()
            .filter(|f| f.format_type == pb::JsonFormatType::JsFormatJson as i32)
        else {
            return (arg, None);
        };
        let s = |v: &str| mk(NodeEnum::String(pb::String { sval: v.into() }));
        let to_json = |n: Node| {
            mk(NodeEnum::TypeCast(Box::new(pb::TypeCast {
                arg: Some(Box::new(n)),
                type_name: Some(pb::TypeName {
                    names: vec![s("pg_catalog"), s("json")],
                    typemod: -1,
                    location: -1,
                    ..Default::default()
                }),
                location: -1,
            })))
        };
        let ty = self.bind_expr(&arg).map(|e| e.ty());
        match ty {
            Ok(t) if matches!(
                t.category(),
                crate::types::Category::String | crate::types::Category::Unknown
            ) =>
            {
                (to_json(arg), Some(false))
            }
            Ok(DataType::Bytea) => {
                let written = pb::JsonEncoding::try_from(f.encoding).ok();
                let enc = match written {
                    Some(pb::JsonEncoding::JsEncUtf16) => "UTF16",
                    Some(pb::JsonEncoding::JsEncUtf32) => "UTF32",
                    _ => "UTF8",
                };
                let call = mk(NodeEnum::FuncCall(Box::new(pb::FuncCall {
                    funcname: vec![s("pg_catalog"), s("convert_from")],
                    args: vec![
                        arg,
                        mk(NodeEnum::AConst(pb::AConst {
                            isnull: false,
                            val: Some(pb::a_const::Val::Sval(pb::String { sval: enc.into() })),
                            location: -1,
                        })),
                    ],
                    location: -1,
                    ..Default::default()
                })));
                let written = !matches!(
                    written,
                    Some(pb::JsonEncoding::JsEncDefault | pb::JsonEncoding::Undefined) | None
                );
                (to_json(call), Some(written))
            }
            _ => (arg, None),
        }
    }

    /// The aggregate's argument `i` was made by `json_agg_value` (`fmt`): shown as written
    /// (`expr FORMAT JSON [ENCODING …]`).
    fn json_agg_formatted(&mut self, e: &Expr, i: usize, fmt: Option<bool>) {
        let Some(written) = fmt else { return };
        let agg = match e {
            Expr::Agg(j, _) => *j,
            Expr::Cast(x, ..) => match &**x {
                Expr::Agg(j, _) => *j,
                _ => return,
            },
            _ => return,
        };
        let Some(a) = self.level().st.aggs.get_mut(agg) else {
            return;
        };
        if let Some(Expr::Cast(x, from, to, explicit)) = a.args.get_mut(i)
            && matches!((&*from, &*to), (DataType::Text, DataType::Json))
        {
            *explicit = false;
            if let Expr::Call(d, args, _) = &mut **x
                && d.name == "convert_from"
                && let Some(Expr::Const(_, t)) = args.get_mut(1)
            {
                *t = if written { DataType::Name } else { DataType::Text };
            }
        }
    }

    /// JSON_ARRAYAGG / JSON_OBJECTAGG as the aggregate call they stand for.
    fn json_agg_call(
        &mut self,
        name: &str,
        args: Vec<Node>,
        ctor: &pb::JsonAggConstructor,
        ret: DataType,
    ) -> Result<Expr> {
        let fc = pb::FuncCall {
            funcname: vec![mk(NodeEnum::String(pb::String {
                sval: name.to_string(),
            }))],
            args,
            agg_order: ctor.agg_order.clone(),
            agg_filter: ctor.agg_filter.clone(),
            over: ctor.over.clone(),
            location: ctor.location,
            ..Default::default()
        };
        let e = self.bind_expr(&mk(NodeEnum::FuncCall(Box::new(fc))))?;
        if let Expr::Agg(i, _) = &e
            && let Some(a) = self.level().st.aggs.get_mut(*i)
        {
            a.json_ctor = true;
        }
        // (As a window function: its aggregate likewise.)
        if let Expr::Window(i, _) = &e
            && let Some(w) = self.level().st.windows.get_mut(*i)
            && let crate::sql::plan::WinFunc::Agg(a) = &mut w.func
        {
            a.json_ctor = true;
        }
        let from = e.ty();
        if from.same_base(&ret) {
            return Ok(e);
        }
        // coerceJsonFuncExpr: an explicit cast, but an implicit-format one, so a length
        // limit errors rather than truncates.
        let err = |from: &DataType| {
            Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type {} to {}", from.display(), ret.display()),
            )
        };
        let e = self.coerce_with(e, &ret.without_typmod(), CastContext::Explicit, &err)?;
        if ret.typmod() == ret.without_typmod().typmod() {
            return Ok(e);
        }
        self.coerce_with(e, &ret, CastContext::Assignment, &err)
    }

    /// JSON_ARRAY(query): `(SELECT JSON_ARRAYAGG(a ...) FROM (query) q(a))`.
    fn json_array_query(&mut self, c: &pb::JsonArrayQueryConstructor) -> Result<Expr> {
        let query = c.query.as_deref().unwrap();
        // The query is analyzed once only to count its columns.
        let ((_, qcols), _) = self.in_level(|b| b.bind_select_node(query))?;
        if qcols.len() != 1 {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "subquery must return only one column"),
                c.location,
            ));
        }
        let col = mk(NodeEnum::ColumnRef(pb::ColumnRef {
            fields: vec![
                mk(NodeEnum::String(pb::String { sval: "q".into() })),
                mk(NodeEnum::String(pb::String { sval: "a".into() })),
            ],
            location: -1,
        }));
        let agg = pb::JsonArrayAgg {
            constructor: Some(Box::new(pb::JsonAggConstructor {
                output: c.output.clone(),
                agg_filter: None,
                agg_order: Vec::new(),
                over: None,
                location: c.location,
            })),
            arg: Some(Box::new(pb::JsonValueExpr {
                raw_expr: Some(Box::new(col)),
                formatted_expr: None,
                format: c.format,
            })),
            absent_on_null: c.absent_on_null,
        };
        let sel = pb::SelectStmt {
            target_list: vec![mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                val: Some(Box::new(mk(NodeEnum::JsonArrayAgg(Box::new(agg))))),
                location: -1,
                ..Default::default()
            })))],
            from_clause: vec![mk(NodeEnum::RangeSubselect(Box::new(pb::RangeSubselect {
                lateral: false,
                subquery: Some(Box::new(query.clone())),
                alias: Some(pb::Alias {
                    aliasname: "q".into(),
                    colnames: vec![mk(NodeEnum::String(pb::String { sval: "a".into() }))],
                }),
            })))],
            limit_option: pb::LimitOption::Default as i32,
            op: pb::SetOperation::SetopNone as i32,
            ..Default::default()
        };
        let link = pb::SubLink {
            sub_link_type: pb::SubLinkType::ExprSublink as i32,
            subselect: Some(Box::new(mk(NodeEnum::SelectStmt(Box::new(sel))))),
            location: c.location,
            ..Default::default()
        };
        self.bind_expr(&mk(NodeEnum::SubLink(Box::new(link))))
    }

    /// JSON_EXISTS / JSON_VALUE / JSON_QUERY (transformJsonFuncExpr).
    fn json_func(&mut self, f: &pb::JsonFuncExpr) -> Result<Expr> {
        let op = match pb::JsonExprOp::try_from(f.op).unwrap_or(pb::JsonExprOp::JsonValueOp) {
            pb::JsonExprOp::JsonExistsOp => Op::Exists,
            pb::JsonExprOp::JsonQueryOp => Op::Query,
            _ => Op::Value,
        };
        let name = match op {
            Op::Exists => "JSON_EXISTS",
            Op::Query => "JSON_QUERY",
            Op::Value => "JSON_VALUE",
        };
        // FORMAT JSON in RETURNING only means something to JSON_QUERY.
        if op != Op::Query
            && let Some(fmt) = f
                .output
                .as_ref()
                .and_then(|o| o.returning.as_ref())
                .and_then(|r| r.format.as_ref())
            && ((fmt.format_type != pb::JsonFormatType::JsFormatDefault as i32
                && fmt.format_type != pb::JsonFormatType::Undefined as i32)
                || (fmt.encoding != pb::JsonEncoding::JsEncDefault as i32
                    && fmt.encoding != pb::JsonEncoding::Undefined as i32))
        {
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("cannot specify FORMAT JSON in RETURNING clause of {name}()"),
                ),
                fmt.location,
            ));
        }
        let wrapper = wrapper_of(f.wrapper);
        let quotes = pb::JsonQuotes::try_from(f.quotes).unwrap_or(pb::JsonQuotes::JsQuotesUnspec);
        check_behaviors(
            op,
            None,
            wrapper,
            quotes,
            f.on_empty.as_deref(),
            f.on_error.as_deref(),
            f.location,
        )?;
        // The context item, as jsonb.
        let ci = f.context_item.as_deref().unwrap();
        let cn = ci.raw_expr.as_deref().unwrap();
        let ctx = self.bind_expr(cn)?;
        let ctx = match ctx.ty() {
            DataType::Jsonb
            | DataType::Text
            | DataType::Varchar(_)
            | DataType::Bpchar(_)
            | DataType::Json => ctx,
            DataType::Unknown => self.coerce_implicit(ctx, &DataType::Text)?,
            t => {
                return Err(with_pos(
                    Error::new(
                        code::CANNOT_COERCE,
                        format!("cannot cast type {} to jsonb", t.display()),
                    ),
                    ast::location(cn),
                ));
            }
        };
        // The path: anything that casts (explicitly) to jsonpath.
        let pn = f.pathspec.as_deref().unwrap();
        let path = self.bind_expr(pn)?;
        let path = match path.ty() {
            DataType::JsonPath => path,
            DataType::Unknown => self.coerce_implicit(path, &DataType::JsonPath)?,
            t => {
                let at = ast::location(pn);
                let shown = t.display();
                let mismatch = move |_: &DataType| {
                    with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "JSON path expression must be of type jsonpath, not of type {shown}"
                            ),
                        ),
                        at,
                    )
                };
                if !matches!(
                    t.category(),
                    crate::types::Category::String | crate::types::Category::Unknown
                ) {
                    return Err(mismatch(&t));
                }
                self.coerce_with(path, &DataType::JsonPath, CastContext::Explicit, &mismatch)?
            }
        };
        let default_ret = match op {
            Op::Exists => DataType::Bool,
            Op::Query => DataType::Jsonb,
            Op::Value => DataType::Text,
        };
        let (ret, domain) = match f.output.as_ref().and_then(|o| o.type_name.as_ref()) {
            Some(tn) => {
                if tn.setof {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "returning SETOF types is not supported in SQL/JSON functions",
                    ));
                }
                super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?
            }
            None => (default_ret, None),
        };
        let (opts, empty_default, error_default) = self.query_behaviors(
            op,
            &ret,
            domain,
            wrapper,
            quotes,
            f.on_empty.as_deref(),
            f.on_error.as_deref(),
        )?;
        let mut names = Vec::new();
        let mut values = Vec::new();
        for p in &f.passing {
            let Some(NodeEnum::JsonArgument(a)) = inner(p) else {
                continue;
            };
            names.push(Value::Text(a.name.as_str().into()));
            values.push(self.json_passing_expr(a.val.as_deref().unwrap())?);
        }
        let names = Expr::Const(
            Value::Array(std::sync::Arc::new(
                crate::types::array::ArrayValue::from_vec(DataType::Text, names),
            )),
            DataType::array_of(DataType::Text),
        );
        let null = Expr::Const(Value::Null, ret.clone());
        let mut args = vec![
            ctx,
            path,
            flag(opts.pack()),
            empty_default.unwrap_or_else(|| null.clone()),
            error_default.unwrap_or(null),
            names,
        ];
        args.extend(values);
        let call = Expr::Call(sj::def(name), args, ret.clone());
        // A RETURNING domain declared with a collation gives the result that collation.
        if !self.deparse
            && ret.collatable()
            && let Some(c) = domain
                .and_then(|d| self.cat.domains.get(&d))
                .and_then(|d| d.collation.clone())
        {
            return Ok(Expr::Collate(Box::new(call), c, super::IMPLICIT_COLLATE));
        }
        Ok(call)
    }

    /// The RETURNING type and ON EMPTY / ON ERROR clauses of a query function or JSON_TABLE
    /// column (transformJsonBehavior): the options, and the DEFAULT expressions (coerced to
    /// `ret`, the base type of `domain`, unless they are coerced when used).
    #[allow(clippy::too_many_arguments)]
    fn query_behaviors(
        &mut self,
        op: Op,
        ret: &DataType,
        domain: Option<u32>,
        wrapper: Wrapper,
        quotes: pb::JsonQuotes,
        on_empty: Option<&pb::JsonBehavior>,
        on_error: Option<&pb::JsonBehavior>,
    ) -> Result<(FuncOpts, Option<Expr>, Option<Expr>)> {
        if ret.category() == crate::types::Category::Pseudo {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "returning pseudo-types is not supported in SQL/JSON functions",
            ));
        }
        let omit_quotes = quotes == pb::JsonQuotes::JsQuotesOmit;
        let constrained = domain.is_some_and(|d| {
            self.cat.domain_type(d).is_some_and(|(dd, _)| {
                let (nn, checks) = self.cat.domain_constraints(dd);
                nn || !checks.is_empty()
            })
        });
        let coerce = match op {
            Op::Exists if domain.is_none() && matches!(ret, DataType::Bool) => sj::Coerce::None,
            Op::Exists => sj::Coerce::Json,
            Op::Query if domain.is_none() && matches!(ret, DataType::Jsonb) && !omit_quotes => {
                sj::Coerce::None
            }
            Op::Query => sj::Coerce::Json,
            Op::Value if domain.is_none() && matches!(ret, DataType::Text) => sj::Coerce::None,
            Op::Value if constrained => sj::Coerce::Json,
            Op::Value => sj::Coerce::Io,
        };
        let mut opts = FuncOpts::new(op, FuncOpts::default_behavior(op), FuncOpts::default_behavior(op));
        opts.wrapper = wrapper;
        opts.omit_quotes = omit_quotes;
        opts.coerce = coerce;
        opts.domain = domain.unwrap_or(0);
        let mut defaults = [None, None];
        for (i, b) in [on_empty, on_error].into_iter().enumerate() {
            let kind = b.map_or(FuncOpts::default_behavior(op), |b| {
                Behavior::from_i64(b.btype as i64)
            });
            // (coerced when used, checked against the domain when used)
            let (coerce, check) = match kind {
                Behavior::Error => (false, false),
                Behavior::Null | Behavior::Unknown | Behavior::Empty => (domain.is_some(), false),
                Behavior::EmptyArray | Behavior::EmptyObject => {
                    (domain.is_some() || !matches!(ret, DataType::Jsonb), false)
                }
                Behavior::True | Behavior::False => {
                    if domain.is_none() && matches!(ret, DataType::Bool) {
                        (false, false)
                    } else if matches!(ret, DataType::Int4) {
                        (false, domain.is_some())
                    } else {
                        (true, false)
                    }
                }
                Behavior::Default => {
                    let n = b.and_then(|b| b.expr.as_deref()).unwrap();
                    let (e, coerce, check) = self.behavior_default(n, ret, domain)?;
                    defaults[i] = Some(e);
                    (coerce, check)
                }
            };
            if i == 0 {
                (opts.on_empty, opts.empty_coerce, opts.empty_domain) = (kind, coerce, check);
            } else {
                (opts.on_error, opts.error_coerce, opts.error_domain) = (kind, coerce, check);
            }
        }
        let [empty_default, error_default] = defaults;
        Ok((opts, empty_default, error_default))
    }

    /// DEFAULT expr ON EMPTY / ON ERROR: (the expression, coerced when used, checked
    /// against the domain when used).
    fn behavior_default(
        &mut self,
        n: &Node,
        ret: &DataType,
        domain: Option<u32>,
    ) -> Result<(Expr, bool, bool)> {
        let at = ast::location(n);
        let e = self.bind_expr(n)?;
        // ValidJsonBehaviorDefaultExpr: a constant, function or operator call (under casts).
        fn valid(e: &Expr) -> bool {
            match e {
                Expr::Const(..) | Expr::Call(..) | Expr::Srf(..) => true,
                Expr::Cast(x, ..)
                | Expr::CoerceToDomain(x, _)
                | Expr::Collate(x, ..)
                | Expr::Relabel(x, _) => valid(x),
                _ => false,
            }
        }
        let mismatch = |msg: &str| with_pos(Error::new(code::DATATYPE_MISMATCH, msg.to_string()), at);
        if !valid(&e) {
            return Err(mismatch(
                "can only specify a constant, non-aggregate function, or operator expression for DEFAULT",
            ));
        }
        let (mut cols, mut set) = (false, false);
        e.walk(&mut |x| match x {
            Expr::Col(..) => cols = true,
            Expr::Srf(..) => set = true,
            _ => {}
        });
        if cols {
            return Err(mismatch("DEFAULT expression must not contain column references"));
        }
        if set {
            return Err(mismatch("DEFAULT expression must not return a set"));
        }
        // (transformJsonBehavior: a DEFAULT with a collation of its own must have the one
        // the RETURNING domain declares.)
        if let Some(dcoll) = domain
            .and_then(|d| self.cat.domains.get(&d))
            .and_then(|d| d.collation.clone())
            && (e.ty().collatable() || matches!(e, Expr::Collate(..)))
            && let Ok(super::Coll::Implicit(c) | super::Coll::Explicit(c, _)) = self.collation_of(&e)
            && c != dcoll
        {
            return Err(with_pos(
                Error::new(
                    code::COLLATION_MISMATCH,
                    "collation of DEFAULT expression conflicts with RETURNING clause",
                )
                .detail(format!("\"{c}\" versus \"{dcoll}\"")),
                at,
            ));
        }
        // A cast to the RETURNING domain is its check, made when the value is used.
        let (e, own_check) = match e {
            Expr::CoerceToDomain(x, dc) if Some(dc.oid) == domain => (*x, true),
            e => (e, false),
        };
        let ety = e.ty();
        let null = matches!(e, Expr::Const(Value::Null, _));
        if (domain.is_none() || own_check) && ety.same_base(ret) && (ety == *ret || null) {
            return Ok((e, false, own_check));
        }
        // NULL, jsonb and boolean values are coerced when used (json_populate_type), a
        // boolean cast when RETURNING integer.
        if null
            || matches!(ety, DataType::Jsonb)
            || (matches!(ety, DataType::Bool) && !matches!(ret, DataType::Int4))
        {
            return Ok((e, true, false));
        }
        let cc = if matches!(
            ret.category(),
            crate::types::Category::String | crate::types::Category::BitString
        ) {
            CastContext::Assignment
        } else {
            CastContext::Explicit
        };
        // (format_type_be: without the typmod)
        let to = ret.without_typmod().display();
        let e = self.coerce_with(e, ret, cc, &|from| {
            with_pos(
                Error::new(
                    code::CANNOT_COERCE,
                    format!(
                        "cannot cast behavior expression of type {} to {to}",
                        from.display()
                    ),
                )
                .hint(format!("You will need to explicitly cast the expression to type {to}.")),
                at,
            )
        })?;
        Ok((e, false, domain.is_some()))
    }

    /// FROM JSON_TABLE(...): a function scan whose rows come from the row pattern paths.
    pub(crate) fn bind_json_table(
        &mut self,
        jt: &pb::JsonTable,
    ) -> Result<(Plan, super::Scope, Vec<Expr>)> {
        let mut cols: Vec<OutCol> = Vec::new();
        let mut names_seen: Vec<String> = Vec::new();
        if let Some(e) = jt.on_error.as_deref()
            && !matches!(
                Behavior::from_i64(e.btype as i64),
                Behavior::Error | Behavior::Empty | Behavior::EmptyArray
            )
        {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "invalid ON ERROR behavior").detail(
                    "Only EMPTY [ ARRAY ] or ERROR is allowed in the top-level ON ERROR clause.",
                ),
                e.location,
            ));
        }
        let (call, params) = self.in_level(|b| {
            let ci = jt.context_item.as_deref().unwrap();
            let cn = ci.raw_expr.as_deref().unwrap();
            let ctx = b.in_clause(super::Clause::FromFunction, |b| b.bind_expr(cn))?;
            let ctx = match ctx.ty() {
                DataType::Jsonb
                | DataType::Text
                | DataType::Varchar(_)
                | DataType::Bpchar(_)
                | DataType::Json => ctx,
                DataType::Unknown => b.coerce_implicit(ctx, &DataType::Text)?,
                t => {
                    return Err(with_pos(
                        Error::new(
                            code::CANNOT_COERCE,
                            format!("cannot cast type {} to jsonb", t.display()),
                        ),
                        ast::location(cn),
                    ));
                }
            };
            let mut args = vec![ctx, Expr::Const(Value::Null, DataType::Text)];
            let mut pnames = Vec::new();
            let mut pvalues = Vec::new();
            for p in &jt.passing {
                let Some(NodeEnum::JsonArgument(a)) = inner(p) else {
                    continue;
                };
                pnames.push(Value::Text(a.name.as_str().into()));
                pvalues.push(b.json_passing_expr(a.val.as_deref().unwrap())?);
            }
            let npassing = pvalues.len();
            args.push(Expr::Const(
                Value::Array(std::sync::Arc::new(
                    crate::types::array::ArrayValue::from_vec(DataType::Text, pnames),
                )),
                DataType::array_of(DataType::Text),
            ));
            args.extend(pvalues);
            let ps = jt.pathspec.as_deref().unwrap();
            let root_path = b.json_table_path(ps)?;
            let mut generated = 0;
            let root_name = if ps.name.is_empty() {
                generated += 1;
                "json_table_path_0".to_string()
            } else {
                names_seen.push(ps.name.clone());
                ps.name.clone()
            };
            let (root_cols, mut nested) =
                b.json_table_columns(&jt.columns, &mut cols, &mut names_seen, &mut args)?;
            fn name_paths(ps: &mut [sj::PathSpec], n: &mut usize) {
                for p in ps {
                    if p.name.is_empty() {
                        p.name = format!("json_table_path_{n}");
                        *n += 1;
                    }
                    name_paths(&mut p.nested, n);
                }
            }
            name_paths(&mut nested, &mut generated);
            let error_on_error = jt
                .on_error
                .as_deref()
                .is_some_and(|e| e.btype == pb::JsonBehaviorType::JsonBehaviorError as i32);
            let spec = sj::TableSpec {
                root: sj::PathSpec {
                    path: root_path,
                    name: root_name,
                    columns: root_cols,
                    nested,
                },
                error_on_error,
                ncols: cols.len(),
                npassing,
            };
            args[1] = Expr::Const(
                Value::Text(
                    serde_json::to_string(&spec)
                        .map_err(|e| crate::error::internal(e.to_string()))?
                        .into(),
                ),
                DataType::Text,
            );
            let ty = DataType::row(
                cols.iter()
                    .map(|c| (c.name.clone(), c.ty.clone()))
                    .collect(),
            );
            Ok(SrfCall {
                func: sj::def("JSON_TABLE"),
                args,
                ty,
                ncols: cols.len(),
                body: None,
            })
        })?;
        let name = jt
            .alias
            .as_ref()
            .map_or_else(|| "json_table".to_string(), |a| a.aliasname.clone());
        if let Some(a) = &jt.alias {
            if a.colnames.len() > cols.len() {
                return Err(Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    format!(
                        "JSON_TABLE function has {} columns available but {} columns specified",
                        cols.len(),
                        a.colnames.len()
                    ),
                ));
            }
            for (c, n) in cols.iter_mut().zip(ast::names(&a.colnames)) {
                c.name = n;
            }
        }
        let rel = super::derived_rel(&name, &cols, 0);
        let names = cols.iter().map(|c| c.name.clone()).collect();
        Ok((
            Plan::FunctionScan {
                calls: vec![call],
                ordinality: false,
                alias: name,
                cols: names,
            },
            super::Scope { rels: vec![rel] },
            params,
        ))
    }

    /// A JSON_TABLE path: a string literal, checked now.
    fn json_table_path(&mut self, ps: &pb::JsonTablePathSpec) -> Result<String> {
        let n = ps.string.as_deref().unwrap();
        let Some(path) = ast::const_str(n) else {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "only string constants are supported in JSON_TABLE path specification",
                ),
                ast::location(n),
            ));
        };
        crate::types::jsonpath::parse(&path).map_err(|e| with_pos(e, ast::location(n)))?;
        Ok(path)
    }

    /// JSON_TABLE columns at one path level: its columns and nested paths; output columns are
    /// appended to `cols` and DEFAULT expressions to `args`.
    fn json_table_columns(
        &mut self,
        list: &[Node],
        cols: &mut Vec<OutCol>,
        seen: &mut Vec<String>,
        args: &mut Vec<Expr>,
    ) -> Result<(Vec<sj::ColSpec>, Vec<sj::PathSpec>)> {
        let mut out = Vec::new();
        let mut nested = Vec::new();
        // A level's own columns come first, then its nested paths' (transformJsonTableColumns).
        let is_nested = |n: &&Node| {
            matches!(inner(n), Some(NodeEnum::JsonTableColumn(c))
                if c.coltype == pb::JsonTableColumnType::JtcNested as i32)
        };
        let ordered: Vec<&Node> = list
            .iter()
            .filter(|n| !is_nested(n))
            .chain(list.iter().filter(|n| is_nested(n)))
            .collect();
        for n in ordered {
            let Some(NodeEnum::JsonTableColumn(c)) = inner(n) else {
                continue;
            };
            let kind = pb::JsonTableColumnType::try_from(c.coltype)
                .unwrap_or(pb::JsonTableColumnType::JtcRegular);
            if kind == pb::JsonTableColumnType::JtcNested {
                let ps = c.pathspec.as_deref().unwrap();
                if !ps.name.is_empty() {
                    if seen.contains(&ps.name) {
                        return Err(with_pos(
                            Error::new(
                                code::DUPLICATE_ALIAS,
                                format!("duplicate JSON_TABLE column or path name: {}", ps.name),
                            ),
                            ps.name_location,
                        ));
                    }
                    seen.push(ps.name.clone());
                }
                let path = self.json_table_path(ps)?;
                let (columns, inner_nested) =
                    self.json_table_columns(&c.columns, cols, seen, args)?;
                nested.push(sj::PathSpec {
                    path,
                    name: ps.name.clone(),
                    columns,
                    nested: inner_nested,
                });
                continue;
            }
            if seen.contains(&c.name) {
                return Err(with_pos(
                    Error::new(
                        code::DUPLICATE_ALIAS,
                        format!("duplicate JSON_TABLE column or path name: {}", c.name),
                    ),
                    c.location,
                ));
            }
            seen.push(c.name.clone());
            let at = cols.len();
            if kind == pb::JsonTableColumnType::JtcForOrdinality {
                if out.iter().any(|o: &sj::ColSpec| o.query.is_none()) {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, "only one FOR ORDINALITY column is allowed"),
                        c.location,
                    ));
                }
                cols.push(OutCol::new(c.name.clone(), DataType::Int4));
                out.push(sj::ColSpec {
                    name: c.name.clone(),
                    at,
                    ty: DataType::Int4,
                    query: None,
                    type_shown: None,
                });
                continue;
            }
            let (ty, domain) = super::resolve_type_name(
                &self.cat,
                &self.ctx.search_path,
                c.type_name.as_ref().unwrap(),
            )?;
            let path = match c.pathspec.as_deref() {
                Some(ps) => self.json_table_path(ps)?,
                None => {
                    let mut p = String::from("$.");
                    crate::types::json::write_json_string(&mut p, &c.name);
                    p
                }
            };
            let wrapper = wrapper_of(c.wrapper);
            let quotes =
                pb::JsonQuotes::try_from(c.quotes).unwrap_or(pb::JsonQuotes::JsQuotesUnspec);
            // isCompositeType: json, jsonb, arrays and rows are better read by JSON_QUERY, as
            // a column with FORMAT JSON, WRAPPER or QUOTES is.
            let composite = matches!(
                ty,
                DataType::Json
                    | DataType::Jsonb
                    | DataType::Array(_)
                    | DataType::Row(_)
                    | DataType::Record
            );
            let op = if kind == pb::JsonTableColumnType::JtcExists {
                Op::Exists
            } else if kind == pb::JsonTableColumnType::JtcFormatted
                || composite
                || wrapper != Wrapper::Unspec
                || quotes != pb::JsonQuotes::JsQuotesUnspec
            {
                Op::Query
            } else {
                Op::Value
            };
            check_behaviors(
                op,
                Some(&c.name),
                wrapper,
                quotes,
                c.on_empty.as_deref(),
                c.on_error.as_deref(),
                c.location,
            )?;
            let (opts, empty_default, error_default) = self.query_behaviors(
                op,
                &ty,
                domain,
                wrapper,
                quotes,
                c.on_empty.as_deref(),
                c.on_error.as_deref(),
            )?;
            let mut arg = |e: Option<Expr>| {
                e.map(|e| {
                    args.push(e);
                    args.len() - 1
                })
            };
            let empty_default = arg(empty_default);
            let error_default = arg(error_default);
            let mut col = OutCol::new(c.name.clone(), ty.clone());
            col.domain = domain;
            cols.push(col);
            let type_shown = domain.map(|d| self.cat.format_type(&ty, Some(d), &self.ctx.search_path));
            out.push(sj::ColSpec {
                name: c.name.clone(),
                at,
                type_shown,
                ty,
                query: Some(sj::ColQuery {
                    path,
                    opts: opts.pack(),
                    empty_default,
                    error_default,
                }),
            });
        }
        Ok((out, nested))
    }
}
