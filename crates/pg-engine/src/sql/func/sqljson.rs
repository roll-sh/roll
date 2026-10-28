//! SQL/JSON constructors, predicates and query functions: JSON_OBJECT, JSON_ARRAY, JSON(),
//! JSON_SCALAR, JSON_SERIALIZE, IS JSON, JSON_EXISTS, JSON_VALUE and JSON_QUERY. The binder
//! calls these definitions directly (they aren't in pg_proc); their options travel as leading
//! constant arguments, and the call's type is the RETURNING type.

use super::json::{value_to_json_text, value_to_jsonb};
use super::{FnCtx, FuncDef, Ret};
use crate::error::{Error, Result, code};
use crate::types::cast::cast_value;
use crate::types::json::{Jsonb, parse_json_dups, parse_jsonb, write_json_string};
use crate::types::jsonpath;
use crate::types::{DataType as T, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Option bits of the constructors (their first argument).
pub const ABSENT_ON_NULL: i64 = 1;
pub const UNIQUE_KEYS: i64 = 2;

/// IS JSON item types (low bits of its first argument; UNIQUE_KEYS << 2 above them).
pub const IS_ANY: i64 = 0;
pub const IS_OBJECT: i64 = 1;
pub const IS_ARRAY: i64 = 2;
pub const IS_SCALAR: i64 = 3;

/// ON EMPTY / ON ERROR behaviors (Postgres's JsonBehaviorType).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behavior {
    Null = 1,
    Error = 2,
    Empty = 3,
    True = 4,
    False = 5,
    Unknown = 6,
    EmptyArray = 7,
    EmptyObject = 8,
    Default = 9,
}

impl Behavior {
    pub fn from_i64(v: i64) -> Behavior {
        match v {
            2 => Behavior::Error,
            3 => Behavior::Empty,
            4 => Behavior::True,
            5 => Behavior::False,
            6 => Behavior::Unknown,
            7 => Behavior::EmptyArray,
            8 => Behavior::EmptyObject,
            9 => Behavior::Default,
            _ => Behavior::Null,
        }
    }

    /// How a view definition shows it (`get_json_behavior`).
    pub fn word(self) -> &'static str {
        match self {
            Behavior::Null => "NULL",
            Behavior::Error => "ERROR",
            Behavior::Empty => "EMPTY",
            Behavior::True => "TRUE",
            Behavior::False => "FALSE",
            Behavior::Unknown => "UNKNOWN",
            Behavior::EmptyArray => "EMPTY ARRAY",
            Behavior::EmptyObject => "EMPTY OBJECT",
            Behavior::Default => "DEFAULT",
        }
    }
}

/// JSON_EXISTS / JSON_VALUE / JSON_QUERY.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Exists = 1,
    Query = 2,
    Value = 3,
}

/// JSON_QUERY wrappers (Postgres's JsonWrapper).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrapper {
    Unspec = 1,
    None = 2,
    Conditional = 3,
    Unconditional = 4,
}

/// How a query function's result reaches the RETURNING type (JsonExpr's use_io_coercion /
/// use_json_coercion).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coerce {
    /// The function's own type (text, jsonb, boolean): no coercion.
    None = 0,
    /// Through the type's input function (JSON_VALUE).
    Io = 1,
    /// json_populate_type over the jsonb result.
    Json = 2,
}

/// The options of a query function, packed into one constant argument.
#[derive(Clone, Copy, Debug)]
pub struct FuncOpts {
    pub op: Op,
    pub on_empty: Behavior,
    pub on_error: Behavior,
    pub wrapper: Wrapper,
    pub omit_quotes: bool,
    pub coerce: Coerce,
    /// The ON EMPTY / ON ERROR value is coerced when used (json_populate_type: NULL, jsonb
    /// and boolean values).
    pub empty_coerce: bool,
    pub error_coerce: bool,
    /// The ON EMPTY / ON ERROR value is checked against the RETURNING domain when used (a
    /// DEFAULT expression's CoerceToDomain, or a boolean cast to a domain over integer).
    pub empty_domain: bool,
    pub error_domain: bool,
    /// RETURNING a domain: its oid (0: none).
    pub domain: u32,
}

impl FuncOpts {
    pub fn new(op: Op, on_empty: Behavior, on_error: Behavior) -> FuncOpts {
        FuncOpts {
            op,
            on_empty,
            on_error,
            wrapper: Wrapper::Unspec,
            omit_quotes: false,
            coerce: Coerce::None,
            empty_coerce: false,
            error_coerce: false,
            empty_domain: false,
            error_domain: false,
            domain: 0,
        }
    }

    pub fn pack(self) -> i64 {
        self.op as i64
            | (self.on_empty as i64) << 4
            | (self.on_error as i64) << 8
            | (self.wrapper as i64) << 12
            | (self.omit_quotes as i64) << 16
            | (self.coerce as i64) << 17
            | (self.empty_coerce as i64) << 19
            | (self.error_coerce as i64) << 20
            | (self.empty_domain as i64) << 21
            | (self.error_domain as i64) << 22
            | (self.domain as i64) << 32
    }

    pub fn unpack(v: i64) -> FuncOpts {
        FuncOpts {
            op: match v & 0xf {
                1 => Op::Exists,
                2 => Op::Query,
                _ => Op::Value,
            },
            on_empty: Behavior::from_i64(v >> 4 & 0xf),
            on_error: Behavior::from_i64(v >> 8 & 0xf),
            wrapper: match v >> 12 & 0xf {
                2 => Wrapper::None,
                3 => Wrapper::Conditional,
                4 => Wrapper::Unconditional,
                _ => Wrapper::Unspec,
            },
            omit_quotes: v >> 16 & 1 == 1,
            coerce: match v >> 17 & 3 {
                1 => Coerce::Io,
                2 => Coerce::Json,
                _ => Coerce::None,
            },
            empty_coerce: v >> 19 & 1 == 1,
            error_coerce: v >> 20 & 1 == 1,
            empty_domain: v >> 21 & 1 == 1,
            error_domain: v >> 22 & 1 == 1,
            domain: (v >> 32) as u32,
        }
    }

    /// The behavior a clause has when it isn't written.
    pub fn default_behavior(op: Op) -> Behavior {
        if op == Op::Exists {
            Behavior::False
        } else {
            Behavior::Null
        }
    }
}

/// Arguments of a query function before the PASSING values: context item, path, options,
/// ON EMPTY default, ON ERROR default, PASSING names.
pub const FUNC_FIXED_ARGS: usize = 6;

/// The definition of one of these functions by its SQL name (`JSON_OBJECT`, `IS JSON`, ...).
pub fn def(name: &str) -> Arc<FuncDef> {
    static DEFS: OnceLock<HashMap<&'static str, Arc<FuncDef>>> = OnceLock::new();
    let defs = DEFS.get_or_init(|| {
        let mk = |name: &'static str, imp: super::FnImpl| {
            (
                name,
                Arc::new(FuncDef {
                    oid: 0,
                    name,
                    args: Vec::new(),
                    variadic: false,
                    spread: false,
                    ret: Ret::Fixed(T::Json),
                    strict: false,
                    volatility: 's',
                    imp,
                    min_args: 0,
                    srf: None,
                    out_cols: Vec::new(),
                    arg_names: Vec::new(),
                    defaults: Vec::new(),
                    coll: None,
                }),
            )
        };
        HashMap::from([
            mk("JSON_OBJECT", json_object),
            mk("JSON_ARRAY", json_array),
            mk("JSON", json_parse),
            mk("JSON_SCALAR", json_scalar),
            mk("JSON_SERIALIZE", json_serialize),
            mk("IS JSON", is_json),
            mk("JSON_EXISTS", json_func),
            mk("JSON_VALUE", json_func),
            mk("JSON_QUERY", json_func),
            {
                let (n, d) = mk("JSON_TABLE", |_, _| {
                    Err(crate::error::internal("JSON_TABLE in a scalar context"))
                });
                let mut d = Arc::try_unwrap(d).unwrap_or_else(|_| unreachable!());
                d.srf = Some(json_table);
                (n, Arc::new(d))
            },
        ])
    });
    defs[name].clone()
}

/// Whether a call is one of these functions.
pub fn is_sqljson(def: &FuncDef) -> bool {
    def.oid == 0
        && matches!(
            def.name,
            "JSON_OBJECT"
                | "JSON_ARRAY"
                | "JSON"
                | "JSON_SCALAR"
                | "JSON_SERIALIZE"
                | "IS JSON"
                | "JSON_EXISTS"
                | "JSON_VALUE"
                | "JSON_QUERY"
        )
}

fn flags(v: &Value) -> i64 {
    match v {
        Value::Int8(i) => *i,
        Value::Int4(i) => *i as i64,
        _ => 0,
    }
}

/// A json / jsonb result as the RETURNING type.
fn returning(v: Value, from: &T, c: &FnCtx) -> Result<Value> {
    if v.is_null() || from.same_base(c.ret) {
        return Ok(v);
    }
    if let (T::Bytea, Value::Json(s)) = (c.ret, &v) {
        // The json text's bytes (UTF8).
        return Ok(Value::Bytea(s.as_bytes().into()));
    }
    // (an I/O coercion: a string type's length is checked, not truncated to)
    cast_value(v, from, c.ret, c.exec.fmt(), false)
}

fn dup_key(key: Option<&str>) -> Error {
    match key {
        Some(k) => {
            let mut s = String::new();
            write_json_string(&mut s, k);
            Error::new(
                code::DUPLICATE_JSON_OBJECT_KEY_VALUE,
                format!("duplicate JSON object key value: {s}"),
            )
        }
        None => Error::new(
            code::DUPLICATE_JSON_OBJECT_KEY_VALUE,
            "duplicate JSON object key value",
        ),
    }
}

fn key_text(v: &Value, ty: &T, c: &FnCtx) -> Result<String> {
    match v {
        Value::Null => Err(Error::new(
            code::NULL_VALUE_NOT_ALLOWED,
            "null value not allowed for object key",
        )),
        Value::Array(_) | Value::Record(_) | Value::Json(_) | Value::Jsonb(_) => Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json",
        )),
        _ if !matches!(ty, T::Text) && super::json::has_json_cast(ty, c) => Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json",
        )),
        Value::Text(s) => Ok(s.to_string()),
        v => Ok(crate::types::text::format_value(v, ty, c.exec.fmt())),
    }
}

/// JSON_OBJECT(k : v, ...): [options, k1, v1, ...].
fn json_object(a: &[Value], c: &FnCtx) -> Result<Value> {
    let f = flags(&a[0]);
    let (absent, unique) = (f & ABSENT_ON_NULL != 0, f & UNIQUE_KEYS != 0);
    let binary = matches!(c.ret, T::Jsonb);
    let mut seen = std::collections::HashSet::new();
    let mut text = String::from("{");
    let mut pairs = Vec::new();
    for i in (1..a.len()).step_by(2) {
        let k = key_text(&a[i], &c.arg_type(i), c)?;
        let v = &a[i + 1];
        // (keys are unique counting the absent ones)
        if unique && !seen.insert(k.clone()) {
            return Err(dup_key((!binary).then_some(k.as_str())));
        }
        if absent && v.is_null() {
            continue;
        }
        if binary {
            pairs.push((k, value_to_jsonb(v, &c.arg_type(i + 1), c.exec.fmt())?));
        } else {
            if text.len() > 1 {
                text.push_str(", ");
            }
            write_json_string(&mut text, &k);
            text.push_str(" : ");
            value_to_json_text(v, &c.arg_type(i + 1), c.exec.fmt(), &mut text)?;
        }
    }
    if binary {
        return Ok(Value::Jsonb(Arc::new(Jsonb::object(pairs))));
    }
    text.push('}');
    returning(Value::Json(text.into()), &T::Json, c)
}

/// JSON_ARRAY(v, ...): [options, v1, ...].
fn json_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    let absent = flags(&a[0]) & ABSENT_ON_NULL != 0;
    let binary = matches!(c.ret, T::Jsonb);
    let mut text = String::from("[");
    let mut items = Vec::new();
    for (i, v) in a.iter().enumerate().skip(1) {
        if absent && v.is_null() {
            continue;
        }
        if binary {
            items.push(value_to_jsonb(v, &c.arg_type(i), c.exec.fmt())?);
        } else {
            if text.len() > 1 {
                text.push_str(", ");
            }
            value_to_json_text(v, &c.arg_type(i), c.exec.fmt(), &mut text)?;
        }
    }
    if binary {
        return Ok(Value::Jsonb(Arc::new(Jsonb::Array(items))));
    }
    text.push(']');
    returning(Value::Json(text.into()), &T::Json, c)
}

/// The text of a json-ish input (text, json, bytea in UTF8).
fn input_text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) | Value::Json(s) => Some(s.to_string()),
        Value::Bytea(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Jsonb(j) => Some(j.to_string()),
        _ => None,
    }
}

/// JSON(expr [WITH UNIQUE KEYS]): [options, expr].
fn json_parse(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(s) = input_text(&a[1]) else {
        return Ok(Value::Null);
    };
    let (_, dup) = parse_json_dups(&s)?;
    if dup && flags(&a[0]) & UNIQUE_KEYS != 0 {
        return Err(dup_key(None));
    }
    returning(Value::Json(s.into()), &T::Json, c)
}

/// JSON_SCALAR(expr): the value as a json scalar.
fn json_scalar(a: &[Value], c: &FnCtx) -> Result<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let mut s = String::new();
    value_to_json_text(&a[0], &c.arg_type(0), c.exec.fmt(), &mut s)?;
    returning(Value::Json(s.into()), &T::Json, c)
}

/// JSON_SERIALIZE(expr [RETURNING type]).
fn json_serialize(a: &[Value], c: &FnCtx) -> Result<Value> {
    let s = match &a[0] {
        Value::Null => return Ok(Value::Null),
        // Postgres hands back jsonb's binary form here, which text reads up to its first
        // zero byte: the start of the container header.
        Value::Jsonb(j) => {
            let header: u32 = match &**j {
                Jsonb::Array(items) => items.len() as u32 | 0x4000_0000,
                Jsonb::Object(pairs) => pairs.len() as u32 | 0x2000_0000,
                _ => 1 | 0x5000_0000,
            };
            let bytes: Vec<u8> = header
                .to_le_bytes()
                .into_iter()
                .take_while(|b| *b != 0)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        v => input_text(v).unwrap_or_default(),
    };
    Ok(match c.ret {
        T::Bytea => Value::Bytea(s.into_bytes().into()),
        T::Text => Value::Text(s.into()),
        ret => cast_value(Value::Text(s.into()), &T::Text, ret, c.exec.fmt(), false)?,
    })
}

fn has_dup_keys(j: &str) -> bool {
    parse_json_dups(j).map(|(_, d)| d).unwrap_or(false)
}

/// expr IS JSON [VALUE | OBJECT | ARRAY | SCALAR] [WITH UNIQUE KEYS]: [options, expr].
fn is_json(a: &[Value], _c: &FnCtx) -> Result<Value> {
    let f = flags(&a[0]);
    let (kind, unique) = (f & 3, f & 4 != 0);
    let (j, text) = match &a[1] {
        Value::Null => return Ok(Value::Null),
        Value::Jsonb(j) => ((**j).clone(), None),
        v => {
            // bytea is read as UTF8 (convert_from), which must be valid.
            if let Value::Bytea(b) = v {
                crate::types::mbconv::verify(crate::types::mbconv::UTF8, b)?;
            }
            let Some(s) = input_text(v) else {
                return Ok(Value::Bool(false));
            };
            match parse_json_dups(&s) {
                Ok((j, _)) => (j, Some(s)),
                Err(_) => return Ok(Value::Bool(false)),
            }
        }
    };
    let ok = match kind {
        IS_OBJECT => matches!(j, Jsonb::Object(_)),
        IS_ARRAY => matches!(j, Jsonb::Array(_)),
        IS_SCALAR => !matches!(j, Jsonb::Object(_) | Jsonb::Array(_)),
        _ => true,
    };
    Ok(Value::Bool(
        ok && !(unique && text.is_some_and(|t| has_dup_keys(&t))),
    ))
}

fn no_item(column: Option<&str>) -> Error {
    match column {
        Some(c) => Error::new(
            code::NO_SQL_JSON_ITEM,
            format!("no SQL/JSON item found for specified path of column \"{c}\""),
        ),
        None => Error::new(
            code::NO_SQL_JSON_ITEM,
            "no SQL/JSON item found for specified path",
        ),
    }
}

/// The PASSING values: a jsonpath variables object, and the date/time ones (kept as
/// date/time items, JsonItemFromDatum).
#[derive(Default)]
pub struct Vars {
    pub json: Option<Jsonb>,
    pub dt: Vec<(String, jsonpath::DateTime)>,
}

/// A date/time PASSING value as a jsonpath item (a timestamptz carries no zone of its own).
fn dt_item(v: &Value) -> Option<jsonpath::DateTime> {
    use jsonpath::DtKind as K;
    let (kind, value, offset) = match v {
        Value::Date(d) => (K::Date, *d as i64, 0),
        Value::Time(t) => (K::Time, *t, 0),
        Value::Timetz(t, z) => (K::TimeTz, *t, -*z),
        Value::Timestamp(t) => (K::Timestamp, *t, 0),
        Value::Timestamptz(t) => (K::TimestampTz, *t, 0),
        _ => return None,
    };
    Some(jsonpath::DateTime {
        kind,
        value,
        offset,
    })
}

/// A date/time item as a SQL value of its type.
fn dt_value(d: &jsonpath::DateTime) -> (Value, T) {
    use jsonpath::DtKind as K;
    match d.kind {
        K::Date => (Value::Date(d.value as i32), T::Date),
        K::Time => (Value::Time(d.value), T::Time(-1)),
        K::TimeTz => (Value::Timetz(d.value, -d.offset), T::Timetz(-1)),
        K::Timestamp => (Value::Timestamp(d.value), T::Timestamp(-1)),
        K::TimestampTz => (Value::Timestamptz(d.value), T::Timestamptz(-1)),
    }
}

/// The PASSING values as jsonpath variables: `names` (text[]) for the values from argument
/// `first` on.
fn passing_vars(names: &Value, a: &[Value], first: usize, c: &FnCtx) -> Result<Vars> {
    let Value::Array(names) = names else {
        return Ok(Vars::default());
    };
    if names.items.is_empty() {
        return Ok(Vars::default());
    }
    let mut vars = Vars::default();
    let mut pairs = Vec::new();
    for (i, n) in names.items.iter().enumerate() {
        let at = first + i;
        let name = n.as_str().unwrap_or_default().to_string();
        if let Some(d) = dt_item(&a[at]) {
            vars.dt.push((name, d));
            continue;
        }
        pairs.push((name, value_to_jsonb(&a[at], &c.arg_type(at), c.exec.fmt())?));
    }
    vars.json = Some(Jsonb::object(pairs));
    Ok(vars)
}

/// The context item as jsonb (None for NULL).
fn context_item(v: &Value) -> Result<Option<Arc<Jsonb>>> {
    Ok(match v {
        Value::Null => None,
        Value::Jsonb(j) => Some(j.clone()),
        v => Some(Arc::new(parse_jsonb(&input_text(v).unwrap_or_default())?)),
    })
}

/// JSON_EXISTS / JSON_VALUE / JSON_QUERY: [context, path, options, empty default, error
/// default, PASSING names, PASSING values...].
fn json_func(a: &[Value], c: &FnCtx) -> Result<Value> {
    let o = FuncOpts::unpack(flags(&a[2]));
    let Value::Text(path) = &a[1] else {
        return Ok(Value::Null);
    };
    let Some(target) = context_item(&a[0])? else {
        // A NULL context item gives NULL, coerced as a result would be (a domain's NOT NULL).
        return match o.coerce {
            Coerce::Json => coercion(o, None, c.ret, c),
            _ => Ok(Value::Null),
        }
        .or_else(|e| result_error(o, e, c.ret, &a[4], c));
    };
    let path = jsonpath::parse(path)?;
    let vars = passing_vars(&a[5], a, FUNC_FIXED_ARGS, c)?;
    run_query(o, &target, &path, &vars, c.ret, &a[3], &a[4], None, c)
}

/// The RETURNING type's domain constraints (`oid` 0: none).
fn domain_check(oid: u32, v: &Value, c: &FnCtx) -> Result<()> {
    if oid == 0 {
        return Ok(());
    }
    if let Some(dc) = crate::sql::binder::Binder::new(c.exec, Vec::new()).domain_coerce(oid)? {
        dc.check(v, c.exec)?;
    }
    Ok(())
}

/// A type's input function over `s` (the target's typmod applied as input does).
pub(super) fn input(s: &str, ty: &T, c: &FnCtx) -> Result<Value> {
    match ty {
        T::Text => Ok(Value::Text(s.into())),
        T::Json => {
            crate::types::json::validate_json(s)?;
            Ok(Value::Json(s.into()))
        }
        T::Jsonb => Ok(Value::Jsonb(Arc::new(parse_jsonb(s)?))),
        _ => crate::types::text::parse_value(s, ty, c.exec.fmt()),
    }
}

/// json_populate_type for SQL/JSON (`j` None: NULL) as `ty` (the base type of RETURNING
/// domain `domain`): with `omit_quotes` a string goes in unquoted; a scalar otherwise goes
/// through the type's input function as its JSON text, an array or object fills an array
/// or a row.
fn populate(j: Option<&Jsonb>, ty: &T, domain: u32, omit_quotes: bool, c: &FnCtx) -> Result<Value> {
    let v = match j {
        None => Value::Null,
        Some(j) if omit_quotes => {
            let s = match j {
                Jsonb::String(s) => s.clone(),
                j => j.to_string(),
            };
            input(&s, ty, c)?
        }
        Some(j) => match ty {
            T::Jsonb => Value::Jsonb(Arc::new(j.clone())),
            T::Array(_) => match j {
                Jsonb::Array(_) => super::json::populate_value(j, ty, c.exec.fmt())?,
                _ => {
                    return Err(Error::new(
                        code::INVALID_TEXT_REPRESENTATION,
                        "expected JSON array",
                    ));
                }
            },
            T::Row(_) | T::Record => match j {
                Jsonb::Object(_) => super::json::populate_value(j, ty, c.exec.fmt())?,
                Jsonb::Array(_) => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "cannot call populate_composite on an array",
                    ));
                }
                _ => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "cannot call populate_composite on a scalar",
                    ));
                }
            },
            _ => input(&j.to_string(), ty, c)?,
        },
    };
    domain_check(domain, &v, c)?;
    Ok(v)
}

/// The query's result `j` (None: NULL) coerced to RETURNING by json_populate_type (with
/// JSON_EXISTS's boolean as jsonb, or cast when RETURNING integer).
fn coercion(o: FuncOpts, j: Option<&Jsonb>, ret: &T, c: &FnCtx) -> Result<Value> {
    if o.op == Op::Exists {
        let Some(Jsonb::Bool(b)) = j else {
            return populate(j, ret, o.domain, false, c);
        };
        if matches!(ret, T::Int4) {
            let v = Value::Int4(*b as i32);
            domain_check(o.domain, &v, c)?;
            return Ok(v);
        }
        return populate(j, ret, o.domain, false, c);
    }
    populate(j, ret, o.domain, o.omit_quotes || o.op == Op::Value, c)
}

/// ExecGetJsonValueItemString: a JSON_VALUE scalar as text (a string unquoted, a date/time
/// value as its type outputs it).
fn item_string(it: &jsonpath::Item, c: &FnCtx) -> String {
    match it {
        jsonpath::Item::Json(Jsonb::String(s)) => s.clone(),
        // (boolout)
        jsonpath::Item::Json(Jsonb::Bool(b)) => if *b { "t" } else { "f" }.into(),
        jsonpath::Item::Json(j) => j.to_string(),
        jsonpath::Item::Datetime(d) => {
            let (v, ty) = dt_value(d);
            crate::types::text::format_value(&v, &ty, c.exec.fmt())
        }
    }
}

/// What the value of an ON EMPTY / ON ERROR clause is, before any coercion.
fn behavior_value(b: Behavior, default: &Value) -> Value {
    match b {
        Behavior::EmptyArray => Value::Jsonb(Arc::new(Jsonb::Array(Vec::new()))),
        Behavior::EmptyObject => Value::Jsonb(Arc::new(Jsonb::Object(Vec::new()))),
        Behavior::True => Value::Bool(true),
        Behavior::False => Value::Bool(false),
        Behavior::Default => default.clone(),
        _ => Value::Null,
    }
}

/// The ON EMPTY (`empty`) or ON ERROR value, coerced to RETURNING when it needs to be; a
/// coercion error is reported as such (unless ERROR ON ERROR lets it through).
fn behavior(o: FuncOpts, empty: bool, default: &Value, ret: &T, c: &FnCtx) -> Result<Value> {
    let (b, coerce, domain) = if empty {
        (o.on_empty, o.empty_coerce, o.empty_domain)
    } else {
        (o.on_error, o.error_coerce, o.error_domain)
    };
    let v = match behavior_value(b, default) {
        // TRUE / FALSE RETURNING integer: cast (bool_int4).
        Value::Bool(x) if !coerce && matches!(ret, T::Int4) => Value::Int4(x as i32),
        v => v,
    };
    let r = if coerce {
        let j = match &v {
            Value::Null => None,
            Value::Jsonb(j) => Some((**j).clone()),
            Value::Bool(x) => Some(Jsonb::Bool(*x)),
            v => Some(value_to_jsonb(v, ret, c.exec.fmt())?),
        };
        populate(j.as_ref(), ret, o.domain, o.omit_quotes || o.op == Op::Value, c)
    } else if domain {
        domain_check(o.domain, &v, c).map(|_| v)
    } else {
        Ok(v)
    };
    match r {
        Err(e) if o.on_error != Behavior::Error && (coerce || domain) => Err(Error::new(
            code::DATATYPE_MISMATCH,
            format!(
                "could not coerce {} expression ({}) to the RETURNING type",
                if empty { "ON EMPTY" } else { "ON ERROR" },
                b.word()
            ),
        )
        .detail(e.message)),
        r => r,
    }
}

/// An error computing or coercing the result: raised under ERROR ON ERROR, else the ON
/// ERROR value.
fn result_error(o: FuncOpts, e: Error, ret: &T, error_default: &Value, c: &FnCtx) -> Result<Value> {
    if o.on_error == Behavior::Error {
        return Err(e);
    }
    behavior(o, false, error_default, ret, c)
}

/// Run one query function over `target` (ExecEvalJsonExprPath): its result as `ret`, after
/// the ON EMPTY / ON ERROR behaviors. `column`: the JSON_TABLE column it computes.
#[allow(clippy::too_many_arguments)]
pub fn run_query(
    o: FuncOpts,
    target: &Jsonb,
    path: &jsonpath::JsonPath,
    vars: &Vars,
    ret: &T,
    empty_default: &Value,
    error_default: &Value,
    column: Option<&str>,
    c: &FnCtx,
) -> Result<Value> {
    let throw = o.on_error == Behavior::Error;
    // JSON_EXISTS in lax mode stops at the first item found (JsonPathExists: no result list),
    // so an error past it never happens.
    if o.op == Op::Exists && !path.strict && vars.dt.is_empty() {
        let found = jsonpath::exists_with(target, path, vars.json.as_ref(), !throw, true, &c.exec.fmt().tz, &|_| {})?;
        let Some(b) = found else {
            return behavior(o, false, error_default, ret, c);
        };
        return match o.coerce {
            Coerce::None => Ok(Value::Bool(b)),
            _ => coercion(o, Some(&Jsonb::Bool(b)), ret, c).or_else(|e| result_error(o, e, ret, error_default, c)),
        };
    }
    // Errors of the path are SQL/JSON errors (others, as a missing variable, always raise).
    let found = jsonpath::query_items(
        target,
        path,
        vars.json.as_ref(),
        &vars.dt,
        !throw,
        // (JsonPathQuery and its kin run the path with useTz.)
        true,
        &c.exec.fmt().tz,
    )?;
    let on_error = |e: Error| result_error(o, e, ret, error_default, c);
    let Some(mut items) = found else {
        return behavior(o, false, error_default, ret, c);
    };
    // An error raised now, or (under ON ERROR) its value.
    let fail = |e: Error| -> Result<Value> {
        if throw {
            Err(e)
        } else {
            behavior(o, false, error_default, ret, c)
        }
    };
    let empty = || -> Result<Value> {
        if o.on_empty == Behavior::Error {
            return Err(no_item(column));
        }
        behavior(o, true, empty_default, ret, c)
    };
    match o.op {
        Op::Exists => {
            let b = !items.is_empty();
            match o.coerce {
                Coerce::None => Ok(Value::Bool(b)),
                _ => coercion(o, Some(&Jsonb::Bool(b)), ret, c).or_else(on_error),
            }
        }
        Op::Value => {
            if items.is_empty() {
                return empty();
            }
            if items.len() > 1 {
                return fail(match column {
                    Some(col) => Error::new(
                        code::MORE_THAN_ONE_SQL_JSON_ITEM,
                        format!("JSON path expression for column \"{col}\" must return single scalar item"),
                    ),
                    None => Error::new(
                        code::MORE_THAN_ONE_SQL_JSON_ITEM,
                        "JSON path expression in JSON_VALUE must return single scalar item",
                    ),
                });
            }
            let it = items.pop().unwrap();
            if matches!(it, jsonpath::Item::Json(Jsonb::Array(_) | Jsonb::Object(_))) {
                return fail(match column {
                    Some(col) => Error::new(
                        code::SQL_JSON_SCALAR_REQUIRED,
                        format!("JSON path expression for column \"{col}\" must return single scalar item"),
                    ),
                    None => Error::new(
                        code::SQL_JSON_SCALAR_REQUIRED,
                        "JSON path expression in JSON_VALUE must return single scalar item",
                    ),
                });
            }
            let r = if matches!(it, jsonpath::Item::Json(Jsonb::Null)) {
                // A JSON null is NULL, which only a domain's constraints may refuse.
                match o.coerce {
                    Coerce::Json => coercion(o, None, ret, c),
                    _ => Ok(Value::Null),
                }
            } else if matches!(ret, T::Json | T::Jsonb) && o.coerce != Coerce::Json {
                input(&it.into_jsonb().to_string(), ret, c)
            } else {
                match o.coerce {
                    Coerce::Json => coercion(o, Some(&it.into_jsonb()), ret, c),
                    Coerce::Io => input(&item_string(&it, c), ret, c),
                    Coerce::None => Ok(Value::Text(item_string(&it, c).into())),
                }
            };
            r.or_else(on_error)
        }
        Op::Query => {
            let n = items.len();
            let wrap = match o.wrapper {
                _ if n == 0 => false,
                Wrapper::Unconditional => true,
                Wrapper::Conditional => n > 1,
                _ => false,
            };
            let item = if wrap {
                Jsonb::Array(items.into_iter().map(jsonpath::Item::into_jsonb).collect())
            } else {
                match n {
                    0 => return empty(),
                    1 => items.pop().unwrap().into_jsonb(),
                    _ => {
                        return fail(match column {
                            Some(col) => Error::new(
                                code::MORE_THAN_ONE_SQL_JSON_ITEM,
                                format!("JSON path expression for column \"{col}\" must return single item when no wrapper is requested"),
                            ),
                            None => Error::new(
                                code::MORE_THAN_ONE_SQL_JSON_ITEM,
                                "JSON path expression in JSON_QUERY must return single item when no wrapper is requested",
                            ),
                        }
                        .hint("Use the WITH WRAPPER clause to wrap SQL/JSON items into an array."));
                    }
                }
            };
            match o.coerce {
                Coerce::Json => coercion(o, Some(&item), ret, c).or_else(on_error),
                _ => Ok(Value::Jsonb(Arc::new(item))),
            }
        }
    }
}

/// jspIsMutable: whether a query function's path makes it mutable (contain_mutable_functions):
/// date/time methods that need the session time zone, or comparisons between date/time
/// values of differing zonedness. `args`: the call's (a non-constant path is mutable).
pub fn path_mutable(args: &[crate::sql::expr::Expr]) -> bool {
    use crate::sql::expr::Expr;
    let path = match args.get(1) {
        Some(Expr::Const(Value::Text(p), _)) => p,
        Some(Expr::Const(Value::Null, _)) => return false,
        _ => return true,
    };
    let Ok(path) = jsonpath::parse(path) else {
        return false;
    };
    let names: Vec<String> = match args.get(5) {
        Some(Expr::Const(Value::Array(a), _)) => a
            .items
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    };
    let vars: Vec<(String, T)> = names
        .into_iter()
        .zip(args.iter().skip(FUNC_FIXED_ARGS).map(|e| e.ty()))
        .collect();
    jsonpath::is_mutable(&path, &|name| {
        vars.iter().find(|v| v.0 == name).map(|v| match v.1 {
            T::Date | T::Time(_) | T::Timestamp(_) => jsonpath::DtStatus::NonZoned,
            T::Timetz(_) | T::Timestamptz(_) => jsonpath::DtStatus::Zoned,
            _ => jsonpath::DtStatus::Non,
        })
    })
}

/// Whether a call is JSON_EXISTS, JSON_VALUE or JSON_QUERY.
pub fn is_query_func(def: &FuncDef) -> bool {
    def.oid == 0 && matches!(def.name, "JSON_EXISTS" | "JSON_VALUE" | "JSON_QUERY")
}

// ── JSON_TABLE ──

/// A JSON_TABLE's row pattern and columns, as its constant second argument (serialized).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TableSpec {
    pub root: PathSpec,
    /// ERROR ON ERROR: an error in a row pattern raises instead of giving no rows.
    pub error_on_error: bool,
    pub ncols: usize,
    /// The PASSING values are the arguments after the PASSING names (argument 2).
    pub npassing: usize,
}

/// A row pattern path (the root or a NESTED PATH) with its columns and nested paths.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct PathSpec {
    pub path: String,
    /// `AS name`, or a generated json_table_path_N.
    pub name: String,
    pub columns: Vec<ColSpec>,
    pub nested: Vec<PathSpec>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ColSpec {
    pub name: String,
    /// The output column.
    pub at: usize,
    pub ty: T,
    /// None: FOR ORDINALITY.
    pub query: Option<ColQuery>,
    /// The column's declared type as written when it is a domain (`ty` is its base type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_shown: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ColQuery {
    pub path: String,
    pub opts: i64,
    /// Arguments holding the DEFAULT ... ON EMPTY / ON ERROR values.
    pub empty_default: Option<usize>,
    pub error_default: Option<usize>,
}

/// JSON_TABLE: [context, spec, PASSING names, PASSING values..., DEFAULT values...].
pub fn json_table(a: &[Value], c: &FnCtx) -> Result<super::SrfIter> {
    let Value::Text(spec) = &a[1] else {
        return Ok(Box::new(std::iter::empty()));
    };
    let spec: TableSpec = serde_json::from_str(spec)
        .map_err(|e| crate::error::internal(format!("JSON_TABLE spec: {e}")))?;
    let Some(target) = context_item(&a[0])? else {
        return Ok(Box::new(std::iter::empty()));
    };
    let vars = passing_vars(&a[2], a, 3, c)?;
    let t = TableRun {
        a,
        c,
        vars: &vars,
        spec: &spec,
    };
    let rows = t.rows(&spec.root, &target)?;
    let width = spec.ncols;
    Ok(Box::new(
        rows.into_iter()
            .map(move |r| {
                let mut out = vec![Value::Null; width];
                for (i, v) in r {
                    out[i] = v;
                }
                Ok(out)
            })
            .collect::<Vec<_>>()
            .into_iter(),
    ))
}

struct TableRun<'a, 'c> {
    a: &'a [Value],
    c: &'a FnCtx<'c>,
    vars: &'a Vars,
    spec: &'a TableSpec,
}

/// The values one row sets, by output column.
type PartialRow = Vec<(usize, Value)>;

impl TableRun<'_, '_> {
    /// The rows of a path over `item`: one per match, joined with its nested paths' rows
    /// (sibling paths' rows one after another; a match without nested rows keeps its row).
    fn rows(&self, p: &PathSpec, item: &Jsonb) -> Result<Vec<PartialRow>> {
        let path = jsonpath::parse(&p.path)?;
        // A SQL/JSON error gives no rows (EMPTY ON ERROR) unless ERROR ON ERROR.
        let items: Vec<Jsonb> = jsonpath::query_items(
            item,
            &path,
            self.vars.json.as_ref(),
            &self.vars.dt,
            !self.spec.error_on_error,
            true,
            &self.c.exec.fmt().tz,
        )?
        .unwrap_or_default()
        .into_iter()
        .map(jsonpath::Item::into_jsonb)
        .collect();
        let mut out = Vec::new();
        for (n, it) in items.iter().enumerate() {
            let mut base = Vec::with_capacity(p.columns.len());
            for col in &p.columns {
                let v = match &col.query {
                    None => Value::Int4(n as i32 + 1),
                    Some(q) => {
                        let cpath = jsonpath::parse(&q.path)?;
                        let null = Value::Null;
                        let ed = q.empty_default.map_or(&null, |i| &self.a[i]);
                        let rd = q.error_default.map_or(&null, |i| &self.a[i]);
                        run_query(
                            FuncOpts::unpack(q.opts),
                            it,
                            &cpath,
                            self.vars,
                            &col.ty,
                            ed,
                            rd,
                            Some(&col.name),
                            self.c,
                        )?
                    }
                };
                base.push((col.at, v));
            }
            let mut children = Vec::new();
            for child in &p.nested {
                children.extend(self.rows(child, it)?);
            }
            if children.is_empty() {
                out.push(base);
            } else {
                for ch in children {
                    let mut r = base.clone();
                    r.extend(ch);
                    out.push(r);
                }
            }
        }
        Ok(out)
    }
}
