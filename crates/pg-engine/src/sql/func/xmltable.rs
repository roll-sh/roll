//! XMLTABLE's rows (nodeTableFuncscan.c over xml.c's XmlTable* routines) and its text
//! (ruleutils.c's get_xmltable).

use super::{FnCtx, FuncDef, Ret, SrfIter};
use crate::catalog::quote_ident;
use crate::error::{Error, Result, code};
use crate::sql::expr::{Expr, eval};
use crate::types::text::parse_value;
use crate::types::xml::{self, xpath};
use crate::types::{Category, DataType, Value};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

/// What the call's arguments mean. The arguments are: this spec, the document, the row
/// path, the namespace URIs, then each column's PATH, DEFAULT and domain check (when it has
/// them; `path`, `default` and `domain` give their positions).
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct TableSpec {
    pub names: Vec<String>,
    pub types: Vec<DataType>,
    /// Each column's type as `format_type_with_typemod` shows it.
    pub type_names: Vec<String>,
    pub not_null: Vec<bool>,
    pub ordinality: Option<usize>,
    pub path: Vec<Option<usize>>,
    pub default: Vec<Option<usize>>,
    pub domain: Vec<Option<usize>>,
    /// Namespace names (None: DEFAULT), whose URIs are arguments 3...
    pub ns_names: Vec<Option<String>>,
}

pub fn spec(call_args: &[Expr]) -> Option<TableSpec> {
    let Some(Expr::Const(Value::Text(s), _)) = call_args.first() else {
        return None;
    };
    serde_json::from_str(s).ok()
}

/// The XMLTABLE pseudo-function a function scan calls.
pub fn def() -> Arc<FuncDef> {
    static DEF: OnceLock<Arc<FuncDef>> = OnceLock::new();
    DEF.get_or_init(|| {
        Arc::new(FuncDef {
            oid: 0,
            name: "XMLTABLE",
            args: Vec::new(),
            variadic: false,
            spread: false,
            ret: Ret::Fixed(DataType::Record),
            strict: false,
            volatility: 's',
            imp: |_, _| Err(crate::error::internal("XMLTABLE in a scalar context")),
            min_args: 0,
            srf: Some(xmltable),
            out_cols: Vec::new(),
            arg_names: Vec::new(),
            defaults: Vec::new(),
            coll: None,
        })
    })
    .clone()
}

/// Whether a call is XMLTABLE's.
pub fn is_xmltable(f: &FuncDef) -> bool {
    f.oid == 0 && f.name == "XMLTABLE"
}

fn not_null_err(msg: &str) -> Error {
    Error::new(code::NULL_VALUE_NOT_ALLOWED, msg.to_string())
}

/// `tfuncFetchRows`: the arguments are evaluated here, as the scan needs them. libxml's
/// WARNINGs become notices.
fn xmltable(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let r = rows(c);
    c.exec.take_input_notices();
    r
}

fn rows(c: &FnCtx) -> Result<SrfIter> {
    let ctx = c.exec;
    let args = c.args;
    let spec = spec(args).ok_or_else(|| crate::error::internal("XMLTABLE spec"))?;
    let arg = |i: usize| eval(&args[i], &[], ctx);
    let doc = arg(1)?;
    let Value::Text(doc) = doc else {
        return Ok(Box::new(std::iter::empty()));
    };
    // tfuncInitialize
    let doc = xpath::parse_document(&xml::xml_out(&doc))?;
    let mut namespaces = xpath::NsMap::new();
    for (i, name) in spec.ns_names.iter().enumerate() {
        let uri = match arg(3 + i)? {
            Value::Text(s) => s,
            _ => return Err(not_null_err("namespace URI must not be null")),
        };
        let Some(name) = name else {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "DEFAULT namespace is not supported",
            ));
        };
        if !namespaces.register(name, &uri) {
            return Err(Error::new(
                code::INVALID_ARGUMENT_FOR_XQUERY,
                "could not set XML namespace",
            ));
        }
    }
    let row_path = match arg(2)? {
        Value::Text(s) => s,
        _ => return Err(not_null_err("row filter expression must not be null")),
    };
    if row_path.is_empty() {
        return Err(Error::new(
            code::INVALID_ARGUMENT_FOR_XQUERY,
            "row path filter must not be empty string",
        ));
    }
    let row_comp = xpath::compile(&row_path)?;
    let ncols = spec.names.len();
    let mut col_comp = Vec::with_capacity(ncols);
    for i in 0..ncols {
        if spec.ordinality == Some(i) {
            col_comp.push(None);
            continue;
        }
        let filter = match spec.path[i] {
            Some(p) => match arg(p)? {
                Value::Text(s) => s.to_string(),
                _ => {
                    return Err(not_null_err("column filter expression must not be null")
                        .detail(format!("Filter for column \"{}\" is null.", spec.names[i])));
                }
            },
            None => spec.names[i].clone(),
        };
        if filter.is_empty() {
            return Err(Error::new(
                code::INVALID_ARGUMENT_FOR_XQUERY,
                "column path filter must not be empty string",
            ));
        }
        col_comp.push(Some(xpath::compile(&filter)?));
    }
    // tfuncLoadRows
    let rows = match xpath::eval(&row_comp, &doc, &doc.root(), &namespaces)? {
        xpath::XValue::NodeSet(nodes) => nodes,
        _ => Vec::new(),
    };
    let fmt = ctx.fmt();
    let mut out = Vec::with_capacity(rows.len());
    for (ordinal, node) in rows.iter().enumerate() {
        ctx.check()?;
        let mut values = Vec::with_capacity(ncols);
        for (i, comp) in col_comp.iter().enumerate() {
            let Some(comp) = comp else {
                values.push(Value::Int4(ordinal as i32 + 1));
                continue;
            };
            let ty = &spec.types[i];
            let cstr = match xpath::eval(comp, &doc, node, &namespaces)? {
                xpath::XValue::NodeSet(nodes) if nodes.is_empty() => None,
                xpath::XValue::NodeSet(nodes) if matches!(ty, DataType::Xml) => Some(
                    nodes
                        .iter()
                        .map(|n| xpath::xml_xmlnodetoxmltype(&doc, n))
                        .collect::<String>(),
                ),
                xpath::XValue::NodeSet(nodes) => {
                    if nodes.len() > 1 {
                        return Err(Error::new(
                            code::CARDINALITY_VIOLATION,
                            "more than one value returned by column XPath expression",
                        ));
                    }
                    Some(xpath::cast_node_to_string(&doc, &nodes[0]))
                }
                xpath::XValue::String(s) => Some(if matches!(ty, DataType::Xml) {
                    xml::escape_xml(&s)
                } else {
                    s
                }),
                xpath::XValue::Bool(b) => Some(if ty.category() == Category::Numeric {
                    xpath::cast_number_to_string(if b { 1.0 } else { 0.0 })
                } else {
                    (if b { "true" } else { "false" }).to_string()
                }),
                xpath::XValue::Number(f) => Some(xpath::cast_number_to_string(f)),
                // (Only libxslt's variables make result tree fragments.)
                xpath::XValue::Tree(_) => None,
            };
            let mut v = match cstr {
                Some(s) => {
                    let v = parse_value(&s, ty, fmt)?;
                    if let Some(d) = spec.domain[i]
                        && let Expr::CoerceToDomain(_, dc) = &args[d]
                    {
                        dc.check(&v, ctx)?;
                    }
                    v
                }
                None => Value::Null,
            };
            if v.is_null()
                && let Some(d) = spec.default[i]
            {
                v = arg(d)?;
            }
            if v.is_null() && spec.not_null[i] {
                return Err(not_null_err(&format!(
                    "null is not allowed in column \"{}\"",
                    spec.names[i]
                )));
            }
            values.push(v);
        }
        out.push(Ok(values));
    }
    Ok(Box::new(out.into_iter()))
}

/// `get_xmltable`: an XMLTABLE call's text. `show` renders its expressions.
pub fn xml_table_text(
    args: &[Expr],
    show: &mut dyn FnMut(&Expr) -> Option<String>,
) -> Option<String> {
    let spec = spec(args)?;
    let mut buf = String::from("XMLTABLE(");
    if !spec.ns_names.is_empty() {
        buf.push_str("XMLNAMESPACES (");
        for (i, name) in spec.ns_names.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            let uri = show(args.get(3 + i)?)?;
            match name {
                Some(n) => {
                    buf.push_str(&uri);
                    buf.push_str(" AS ");
                    buf.push_str(&quote_ident(n));
                }
                None => {
                    buf.push_str("DEFAULT ");
                    buf.push_str(&uri);
                }
            }
        }
        buf.push_str("), ");
    }
    buf.push('(');
    buf.push_str(&show(args.get(2)?)?);
    buf.push_str(") PASSING (");
    buf.push_str(&show(args.get(1)?)?);
    buf.push(')');
    if !spec.names.is_empty() {
        buf.push_str(" COLUMNS ");
        for i in 0..spec.names.len() {
            if i > 0 {
                buf.push_str(", ");
            }
            buf.push_str(&quote_ident(&spec.names[i]));
            buf.push(' ');
            if spec.ordinality == Some(i) {
                buf.push_str("FOR ORDINALITY");
                continue;
            }
            buf.push_str(&spec.type_names[i]);
            if let Some(d) = spec.default[i] {
                buf.push_str(" DEFAULT (");
                buf.push_str(&show(args.get(d)?)?);
                buf.push(')');
            }
            if let Some(p) = spec.path[i] {
                buf.push_str(" PATH (");
                buf.push_str(&show(args.get(p)?)?);
                buf.push(')');
            }
            if spec.not_null[i] {
                buf.push_str(" NOT NULL");
            }
        }
    }
    buf.push(')');
    Some(buf)
}
