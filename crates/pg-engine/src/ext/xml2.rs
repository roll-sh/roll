//! xml2 (`contrib/xml2`): the XPath functions over libxml's model (`types::xml`), and
//! xpath_table. xslt_process fails as it does in a PostgreSQL built without libxslt: the
//! engine has no XSLT processor (tests/contrib/STATUS.md has why).

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::xml::parser::{Options, Parser};
use crate::types::xml::save::Saver;
use crate::types::xml::tree::{Doc, Kind};
use crate::types::xml::xpath::{self, XDoc, XNode, XValue};
use crate::types::{DataType, Value};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "xml_encode_special_chars" => Native::scalar(|a, _| {
            Ok(Value::Text(crate::types::xml::encode_special_chars(text(&a[0])).into()))
        }),
        "xpath_string" => Native::scalar(xpath_string),
        "xpath_nodeset" => Native::scalar(xpath_nodeset),
        "xpath_list" => Native::scalar(xpath_list),
        "xpath_number" => Native::scalar(xpath_number),
        "xpath_bool" => Native::scalar(xpath_bool),
        "xpath_table" => Native::set(xpath_table),
        "xslt_process" => Native::scalar(|_, _| {
            Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "xslt_process() is not available without libxslt",
            ))
        }),
        _ => return None,
    })
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(t) => t,
        _ => "",
    }
}

/// `xmlReadMemory(doc, XML_PARSE_NOENT)` under PG_XML_STRICTNESS_LEGACY: None for a document
/// that isn't well-formed (no error).
fn read_memory(s: &str) -> Option<XDoc> {
    let opts = Options {
        replace_entities: true,
        keep_blanks: true,
        dtd_attr: false,
    };
    let mut p = Parser::new(Doc::new(None), opts);
    let ok = p.parse_document(s.as_bytes());
    (ok && p.well_formed).then(|| XDoc::new(p.doc))
}

/// `xmlXPathCtxtCompile`, failing as xml2 reports it.
fn compile(path: &str) -> Result<xpath::Compiled> {
    xpath::compile(path).map_err(|mut e| {
        e.message = "XPath Syntax Error".into();
        e
    })
}

/// The XPath's result over the document's root element (`pgxml_xpath`): None when the
/// document isn't well-formed or the evaluation fails.
fn eval_in(x: &XDoc, comp: &xpath::Compiled) -> Option<XValue> {
    let root = x.doc.root_element()?;
    xpath::eval(comp, x, &XNode::Node(root), &xpath::NsMap::new()).ok()
}

fn pgxml_xpath(doc: &str, path: &str) -> Result<Option<(XDoc, XValue)>> {
    let Some(x) = read_memory(doc) else {
        return Ok(None);
    };
    let comp = compile(path)?;
    Ok(eval_in(&x, &comp).map(|v| (x, v)))
}

/// `xmlNodeDump(buf, doc, node, 1, 0)` (content with xmlEscapeContent, attributes with
/// non-ASCII as character references).
fn node_dump(x: &XDoc, id: usize) -> String {
    let mut s = Saver::new(&x.doc, false, false);
    s.node_dump();
    s.save_tree(id);
    s.out
}

/// `pgxmlNodeSetToText`.
fn nodeset_text(x: &XDoc, set: &[XNode], top: &str, sep: &str, plain: Option<&str>) -> String {
    let mut out = String::new();
    if !top.is_empty() {
        out.push_str(&format!("<{top}>"));
    }
    for (i, n) in set.iter().enumerate() {
        if let Some(p) = plain {
            out.push_str(&xpath::cast_node_to_string(x, n));
            if i + 1 < set.len() {
                out.push_str(p);
            }
            continue;
        }
        if !sep.is_empty() {
            out.push_str(&format!("<{sep}>"));
        }
        match n {
            XNode::Node(id) if x.doc.nodes[*id].kind != Kind::Namespace => out.push_str(&node_dump(x, *id)),
            n => out.push_str(&xpath::cast_node_to_string(x, n)),
        }
        if !sep.is_empty() {
            out.push_str(&format!("</{sep}>"));
        }
    }
    if !top.is_empty() {
        out.push_str(&format!("</{top}>"));
    }
    out
}

/// `pgxml_result_to_text`.
fn result_text(c: &FnCtx, r: Option<(XDoc, XValue)>, top: &str, sep: &str, plain: Option<&str>) -> Value {
    let Some((x, v)) = r else { return Value::Null };
    let s = match v {
        XValue::NodeSet(set) => nodeset_text(&x, &set, top, sep, plain),
        XValue::String(s) => s,
        v => unsupported(c, &v),
    };
    Value::Text(s.into())
}

/// A result of another type: `<unsupported/>`, with a NOTICE naming its xmlXPathObjectType.
fn unsupported(c: &FnCtx, v: &XValue) -> String {
    let t = match v {
        XValue::NodeSet(_) => 1,
        XValue::Bool(_) => 2,
        XValue::Number(_) => 3,
        XValue::String(_) => 4,
    };
    let mut e = Error::new(code::SUCCESSFUL_COMPLETION, format!("unsupported XQuery result: {t}"));
    e.severity = crate::error::Severity::Notice;
    c.exec.notice(e);
    "<unsupported/>".into()
}

fn xpath_string(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = pgxml_xpath(text(&a[0]), &format!("string({})", text(&a[1])))?;
    Ok(result_text(c, r, "", "", None))
}

fn xpath_nodeset(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = pgxml_xpath(text(&a[0]), text(&a[1]))?;
    Ok(result_text(c, r, text(&a[2]), text(&a[3]), None))
}

fn xpath_list(a: &[Value], c: &FnCtx) -> Result<Value> {
    let r = pgxml_xpath(text(&a[0]), text(&a[1]))?;
    Ok(result_text(c, r, "", "", Some(text(&a[2]))))
}

fn xpath_number(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some((x, mut v)) = pgxml_xpath(text(&a[0]), text(&a[1]))? else {
        return Ok(Value::Null);
    };
    let n = xpath::cast_to_number(&x, &mut v) as f32;
    Ok(if n.is_nan() { Value::Null } else { Value::Float4(n) })
}

fn xpath_bool(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Some((x, mut v)) = pgxml_xpath(text(&a[0]), text(&a[1]))? else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(match &v {
        XValue::NodeSet(s) => !s.is_empty(),
        XValue::Bool(b) => *b,
        XValue::Number(n) => *n != 0.0 && !n.is_nan(),
        XValue::String(_) => !xpath::cast_to_string(&x, &mut v).is_empty(),
    }))
}

/// `xpath_table(key, document, relation, xpaths, condition)`: per row of `SELECT key,
/// document FROM relation WHERE condition`, a row per position in the paths' node sets (the
/// key first, one column per path; NULLs for a document that isn't well-formed).
fn xpath_table(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let types: Vec<DataType> = match c.ret {
        DataType::Row(r) => r.fields.iter().map(|f| f.1.clone()).collect(),
        t => vec![t.clone()],
    };
    if types.is_empty() {
        return Err(Error::new(code::SYNTAX_ERROR, "xpath_table must have at least one output column"));
    }
    // (Only as many paths as there are columns for them.)
    let mut paths: Vec<&str> = Vec::new();
    let mut rest = text(&a[3]);
    while paths.len() < types.len() - 1 {
        match rest.split_once('|') {
            Some((p, r)) => {
                paths.push(p);
                rest = r;
            }
            None => {
                paths.push(rest);
                break;
            }
        }
    }
    let sql = format!("SELECT {}, {} FROM {} WHERE {}", text(&a[0]), text(&a[1]), text(&a[2]), text(&a[4]));
    let (rows, cols) = crate::plpgsql::spi_query(c.exec, &sql).map_err(|e| match e.position {
        Some(p) => e.internal_query(p, &sql),
        None => e,
    })?;
    if cols.len() != 2 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "expression returning multiple columns is not valid in parameter list",
        )
        .detail(format!("Expected two columns in SPI result, got {}.", cols.len())));
    }
    let fmt = c.exec.fmt();
    let as_text = |v: &Value, t: &DataType| (!v.is_null()).then(|| crate::types::text::format_value(v, t, fmt));
    let build = |values: &[Option<String>]| -> Result<Vec<Value>> {
        types
            .iter()
            .zip(values)
            .map(|(t, v)| match v {
                None => Ok(Value::Null),
                Some(s) => crate::types::text::parse_value(s, t, fmt),
            })
            .collect()
    };
    let comps = paths.iter().map(|p| compile(p)).collect::<Result<Vec<_>>>();
    let mut out = Vec::new();
    for r in &rows {
        let mut values: Vec<Option<String>> = vec![None; types.len()];
        values[0] = as_text(&r[0], &cols[0].ty);
        let doc = as_text(&r[1], &cols[1].ty).and_then(|d| read_memory(&d));
        let Some(x) = doc else {
            out.push(build(&values)?);
            continue;
        };
        let comps = match &comps {
            Ok(v) => v,
            Err(e) => return Err(e.clone()),
        };
        let mut rownr = 0;
        loop {
            let mut had_values = false;
            for (j, comp) in comps.iter().enumerate() {
                if let Some(v) = eval_in(&x, comp) {
                    values[j + 1] = match v {
                        XValue::NodeSet(set) => set.get(rownr).map(|n| {
                            had_values = true;
                            xpath::cast_node_to_string(&x, n)
                        }),
                        XValue::String(s) => Some(s),
                        v => Some(unsupported(c, &v)),
                    };
                }
            }
            if !had_values {
                break;
            }
            out.push(build(&values)?);
            rownr += 1;
        }
    }
    Ok(Box::new(out.into_iter().map(Ok)))
}
