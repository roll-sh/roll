//! XML functions and `XmlExpr` evaluation (utils/adt/xml.c, ExecEvalXmlExpr).

use super::{FnCtx, Registry};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::expr::{XmlExpr, XmlOp, eval};
use crate::types::datetime::{
    DATE_NEG_INF, DATE_POS_INF, TS_NEG_INF, TS_POS_INF, format_date, format_timestamp_xsd,
};
use crate::types::text::{FormatCtx, format_value};
use crate::types::xml::{self, XmlOption};
use crate::types::{DataType, Value};

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        _ => "",
    }
}

/// `map_sql_value_to_xml_value`.
pub fn map_sql_value_to_xml_value(
    v: &Value,
    ty: &DataType,
    escape: bool,
    fmt: &FormatCtx,
) -> Result<String> {
    if let Value::Array(a) = v {
        let mut out = String::new();
        for item in &a.items {
            if item.is_null() {
                continue;
            }
            out.push_str("<element>");
            out.push_str(&map_sql_value_to_xml_value(item, &a.elem, true, fmt)?);
            out.push_str("</element>");
        }
        return Ok(out);
    }
    let range = |what: &str, detail: bool| {
        let e = Error::new(
            code::DATETIME_FIELD_OVERFLOW,
            format!("{what} out of range"),
        );
        if detail {
            e.detail(format!("XML does not support infinite {what} values."))
        } else {
            e
        }
    };
    Ok(match v {
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Date(d) => {
            if matches!(*d, DATE_NEG_INF | DATE_POS_INF) {
                return Err(range("date", true));
            }
            format_date(*d)
        }
        Value::Timestamp(t) | Value::Timestamptz(t) => {
            if matches!(*t, TS_NEG_INF | TS_POS_INF) {
                return Err(range("timestamp", true));
            }
            format_timestamp_xsd(*t, matches!(v, Value::Timestamptz(_)).then_some(&fmt.tz))
        }
        Value::Bytea(b) => {
            if fmt.xml_binary_hex {
                b.iter().map(|x| format!("{x:02X}")).collect()
            } else {
                base64(b)
            }
        }
        _ => {
            let s = format_value(v, ty, fmt);
            if matches!(ty, DataType::Xml) || !escape {
                s
            } else {
                xml::escape_xml(&s)
            }
        }
    })
}

/// `xmlOutputBufferWriteBase64`: lines of 72 characters, separated by CRLF.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut linelen = 0;
    for chunk in data.chunks(3) {
        let g = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let mut o = [
            T[(g[0] >> 2) as usize],
            T[(((g[0] & 3) << 4) | (g[1] >> 4)) as usize],
            T[(((g[1] & 0xf) << 2) | (g[2] >> 6)) as usize],
            T[(g[2] & 0x3f) as usize],
        ];
        if chunk.len() < 3 {
            o[3] = b'=';
            if chunk.len() < 2 {
                o[2] = b'=';
            }
        }
        if linelen >= 72 {
            out.push_str("\r\n");
            linelen = 0;
        }
        out.extend(o.iter().map(|&c| c as char));
        linelen += 4;
    }
    out
}

/// `xmlelement`.
fn element(x: &XmlExpr, named: &[Value], args: &[Value], fmt: &FormatCtx) -> Result<String> {
    let mut attrs = Vec::new();
    for (v, e) in named.iter().zip(&x.named_args) {
        attrs.push(if v.is_null() {
            None
        } else {
            Some(map_sql_value_to_xml_value(v, &e.ty(), false, fmt)?)
        });
    }
    let mut content = Vec::new();
    for (v, e) in args.iter().zip(&x.args) {
        if !v.is_null() {
            content.push(map_sql_value_to_xml_value(v, &e.ty(), true, fmt)?);
        }
    }
    // xmlTextWriter: the start tag stays open (and closes as "/>") until content is written.
    let name = x.name.as_deref().unwrap_or("");
    let mut out = format!("<{name}");
    for (v, n) in attrs.iter().zip(&x.arg_names) {
        if let Some(v) = v {
            out.push_str(&format!(" {n}=\""));
            xml::attr_escape(&mut out, v);
            out.push('"');
        }
    }
    if content.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        for c in content {
            out.push_str(&c);
        }
        out.push_str(&format!("</{name}>"));
    }
    Ok(out)
}

/// `ExecEvalXmlExpr`.
pub fn eval_xml_expr(x: &XmlExpr, row: &[Value], ctx: &ExecCtx) -> Result<Value> {
    let named = x
        .named_args
        .iter()
        .map(|e| eval(e, row, ctx))
        .collect::<Result<Vec<_>>>()?;
    let args = x
        .args
        .iter()
        .map(|e| eval(e, row, ctx))
        .collect::<Result<Vec<_>>>()?;
    let fmt = ctx.fmt();
    Ok(match x.op {
        XmlOp::Concat => {
            let values: Vec<&str> = args.iter().filter(|v| !v.is_null()).map(text).collect();
            if values.is_empty() {
                Value::Null
            } else {
                Value::text(xml::concat(&values))
            }
        }
        XmlOp::Forest => {
            let mut out = String::new();
            let mut any = false;
            for ((v, e), n) in named.iter().zip(&x.named_args).zip(&x.arg_names) {
                if !v.is_null() {
                    out.push_str(&format!(
                        "<{n}>{}</{n}>",
                        map_sql_value_to_xml_value(v, &e.ty(), true, fmt)?
                    ));
                    any = true;
                }
            }
            if any { Value::text(out) } else { Value::Null }
        }
        XmlOp::Element => Value::text(element(x, &named, &args, fmt)?),
        XmlOp::Parse => {
            if args[0].is_null() || args[1].is_null() {
                return Ok(Value::Null);
            }
            let opt = if x.document {
                XmlOption::Document
            } else {
                XmlOption::Content
            };
            xml::validate(text(&args[0]), opt, matches!(args[1], Value::Bool(true)))?;
            args[0].clone()
        }
        XmlOp::Pi => {
            let arg = args
                .first()
                .map(|v| if v.is_null() { None } else { Some(text(v)) });
            match xml::pi(x.name.as_deref().unwrap_or(""), arg)? {
                Some(s) => Value::text(s),
                None => Value::Null,
            }
        }
        XmlOp::Root => {
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let version = if args[1].is_null() {
                None
            } else {
                Some(text(&args[1]))
            };
            let standalone = match args[2] {
                Value::Int4(i) => i,
                _ => 3,
            };
            Value::text(xml::root(text(&args[0]), version, standalone))
        }
        XmlOp::Serialize => {
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            Value::text(xml::to_text_with_options(
                text(&args[0]),
                x.document,
                x.indent,
            )?)
        }
        XmlOp::IsDocument => {
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            Value::Bool(xml::is_document(text(&args[0])))
        }
    })
}

fn xmloption(c: &FnCtx) -> XmlOption {
    if c.exec.fmt().xml_document {
        XmlOption::Document
    } else {
        XmlOption::Content
    }
}

fn well_formed(s: &str, opt: XmlOption) -> Value {
    Value::Bool(xml::xml_parse(s, opt, true).is_ok())
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    r.func("xmlcomment", vec![Text], Xml, |a, _| {
        Ok(Value::text(xml::comment(text(&a[0]))?))
    });
    r.func("xmltext", vec![Text], Xml, |a, _| {
        Ok(Value::text(xml::encode_special_chars(text(&a[0]))))
    });
    r.func("xml", vec![Text], Xml, |a, c| {
        xml::validate(text(&a[0]), xmloption(c), true)?;
        Ok(a[0].clone())
    })
    .stable();
    r.func("xmlvalidate", vec![Xml, Text], Bool, |_, _| {
        Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "xmlvalidate is not implemented",
        ))
    });
    r.func("xmlconcat2", vec![Xml, Xml], Xml, |a, _| {
        Ok(match (&a[0], &a[1]) {
            (Value::Null, b) => b.clone(),
            (a, Value::Null) => a.clone(),
            (a, b) => Value::text(xml::concat(&[text(a), text(b)])),
        })
    })
    .non_strict();
    r.func("xml_is_well_formed", vec![Text], Bool, |a, c| {
        Ok(well_formed(text(&a[0]), xmloption(c)))
    })
    .stable();
    r.func("xml_is_well_formed_document", vec![Text], Bool, |a, _| {
        Ok(well_formed(text(&a[0]), XmlOption::Document))
    });
    r.func("xml_is_well_formed_content", vec![Text], Bool, |a, _| {
        Ok(well_formed(text(&a[0]), XmlOption::Content))
    });
    r.func(
        "xpath",
        vec![Text, Xml, DataType::array_of(Text)],
        DataType::array_of(Xml),
        |a, c| xpath_array(&a[0], &a[1], a.get(2), c),
    );
    r.func("xpath", vec![Text, Xml], DataType::array_of(Xml), |a, c| {
        xpath_array(&a[0], &a[1], None, c)
    });
    r.func(
        "xpath_exists",
        vec![Text, Xml, DataType::array_of(Text)],
        Bool,
        |a, c| xpath_exists(&a[0], &a[1], a.get(2), c),
    );
    r.func("xpath_exists", vec![Text, Xml], Bool, |a, c| {
        xpath_exists(&a[0], &a[1], None, c)
    });
    r.func("xmlexists", vec![Text, Xml], Bool, |a, c| {
        xpath_exists(&a[0], &a[1], None, c)
    });
}

fn ns_array(v: Option<&Value>) -> Option<&crate::types::array::ArrayValue> {
    match v {
        Some(Value::Array(a)) => Some(a),
        _ => None,
    }
}

/// `xpath`: the results as an xml array.
fn xpath_array(expr: &Value, data: &Value, ns: Option<&Value>, c: &FnCtx) -> Result<Value> {
    let items = xml::xpath::xpath(text(expr), text(data), ns_array(ns), c.exec.fmt());
    libxml_warnings(c);
    let items = items?;
    let items = items.into_iter().map(Value::text).collect();
    Ok(Value::Array(std::sync::Arc::new(
        crate::types::array::ArrayValue::from_vec(DataType::Xml, items),
    )))
}

/// `xpath_exists` / `xmlexists`.
fn xpath_exists(expr: &Value, data: &Value, ns: Option<&Value>, c: &FnCtx) -> Result<Value> {
    let r = xml::xpath::exists(text(expr), text(data), ns_array(ns));
    libxml_warnings(c);
    Ok(Value::Bool(r?))
}

/// The WARNINGs libxml raised, with the CONTEXT of where they were raised.
fn libxml_warnings(c: &FnCtx) {
    c.exec.take_input_notices();
}
