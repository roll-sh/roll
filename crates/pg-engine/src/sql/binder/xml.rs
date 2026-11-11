//! XML expressions (parse_expr.c's transformXmlExpr / transformXmlSerialize).

use super::Binder;
use super::ast::{self, inner, with_pos};
use crate::error::{Error, Result, code};
use crate::sql::expr::{Expr, XmlExpr, XmlOp};
use crate::types::DataType;
use crate::types::cast::CastContext;
use crate::types::xml::map_sql_identifier_to_xml_name;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};

impl Binder<'_, '_> {
    /// `coerce_to_specific_type`: an assignment coercion, or "argument of X must be type …".
    fn coerce_specific(&mut self, e: Expr, to: &DataType, what: &str, n: &Node) -> Result<Expr> {
        let loc = ast::location(n);
        let what = what.to_string();
        let target = to.clone();
        self.coerce_with(e, to, CastContext::Assignment, &|from| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "argument of {what} must be type {}, not type {}",
                    target.display(),
                    from.display()
                ),
            )
        })
        .map_err(|e| {
            if e.code == code::DATATYPE_MISMATCH {
                with_pos(e, loc)
            } else {
                e
            }
        })
    }

    pub(crate) fn bind_xml_expr(&mut self, x: &pb::XmlExpr) -> Result<Expr> {
        let op = match pb::XmlExprOp::try_from(x.op).unwrap_or(pb::XmlExprOp::Undefined) {
            pb::XmlExprOp::IsXmlconcat => XmlOp::Concat,
            pb::XmlExprOp::IsXmlelement => XmlOp::Element,
            pb::XmlExprOp::IsXmlforest => XmlOp::Forest,
            pb::XmlExprOp::IsXmlparse => XmlOp::Parse,
            pb::XmlExprOp::IsXmlpi => XmlOp::Pi,
            pb::XmlExprOp::IsXmlroot => XmlOp::Root,
            pb::XmlExprOp::IsXmlserialize => XmlOp::Serialize,
            pb::XmlExprOp::IsDocument => XmlOp::IsDocument,
            pb::XmlExprOp::Undefined => {
                return Err(crate::error::internal("unrecognized XML operation"));
            }
        };
        let name =
            (!x.name.is_empty()).then(|| map_sql_identifier_to_xml_name(&x.name, false, false));
        let mut named_args = Vec::new();
        let mut arg_names: Vec<String> = Vec::new();
        for n in &x.named_args {
            let Some(NodeEnum::ResTarget(r)) = inner(n) else {
                continue;
            };
            let val = r.val.as_deref().unwrap();
            let e = self.bind_expr(val)?;
            let argname = if !r.name.is_empty() {
                map_sql_identifier_to_xml_name(&r.name, false, false)
            } else if let Some(NodeEnum::ColumnRef(c)) = inner(val) {
                let col = match c.fields.last().and_then(inner) {
                    Some(NodeEnum::String(s)) => s.sval.clone(),
                    _ => "?column?".into(),
                };
                map_sql_identifier_to_xml_name(&col, true, false)
            } else {
                let msg = if op == XmlOp::Element {
                    "unnamed XML attribute value must be a column reference"
                } else {
                    "unnamed XML element value must be a column reference"
                };
                return Err(with_pos(Error::new(code::SYNTAX_ERROR, msg), r.location));
            };
            if op == XmlOp::Element && arg_names.contains(&argname) {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!("XML attribute name \"{argname}\" appears more than once"),
                    ),
                    r.location,
                ));
            }
            named_args.push(e);
            arg_names.push(argname);
        }
        let mut args = Vec::new();
        for (i, n) in x.args.iter().enumerate() {
            let e = self.bind_expr(n)?;
            let e = match op {
                XmlOp::Concat => self.coerce_specific(e, &DataType::Xml, "XMLCONCAT", n)?,
                XmlOp::Element => e,
                XmlOp::Forest => self.coerce_specific(e, &DataType::Xml, "XMLFOREST", n)?,
                XmlOp::Parse if i == 0 => {
                    self.coerce_specific(e, &DataType::Text, "XMLPARSE", n)?
                }
                XmlOp::Parse => self.coerce_bool_at(e, "XMLPARSE", n)?,
                XmlOp::Pi => self.coerce_specific(e, &DataType::Text, "XMLPI", n)?,
                XmlOp::Root if i == 0 => self.coerce_specific(e, &DataType::Xml, "XMLROOT", n)?,
                XmlOp::Root if i == 1 => self.coerce_specific(e, &DataType::Text, "XMLROOT", n)?,
                XmlOp::Root => self.coerce_specific(e, &DataType::Int4, "XMLROOT", n)?,
                XmlOp::Serialize => e,
                XmlOp::IsDocument => self.coerce_specific(e, &DataType::Xml, "IS DOCUMENT", n)?,
            };
            args.push(e);
        }
        let document = x.xmloption == pb::XmlOptionType::XmloptionDocument as i32;
        Ok(Expr::Xml(Box::new(XmlExpr {
            op,
            name,
            named_args,
            arg_names,
            args,
            document,
            indent: x.indent,
            target: DataType::Xml,
        })))
    }

    pub(crate) fn bind_xml_serialize(&mut self, xs: &pb::XmlSerialize) -> Result<Expr> {
        let n = xs.expr.as_deref().unwrap();
        let e = self.bind_expr(n)?;
        let e = self.coerce_specific(e, &DataType::Xml, "XMLSERIALIZE", n)?;
        let tn = xs.type_name.as_ref().unwrap();
        let (target, domain) = super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?;
        let document = xs.xmloption == pb::XmlOptionType::XmloptionDocument as i32;
        let x = Expr::Xml(Box::new(XmlExpr {
            op: XmlOp::Serialize,
            name: None,
            named_args: Vec::new(),
            arg_names: Vec::new(),
            args: vec![e],
            document,
            indent: xs.indent,
            target: target.clone(),
        }));
        let shown = target.without_typmod().display();
        let loc = xs.location;
        let out = self
            .coerce_with(x, &target, CastContext::Implicit, &|_| {
                Error::new(
                    code::CANNOT_COERCE,
                    format!("cannot cast XMLSERIALIZE result to {shown}"),
                )
            })
            .map_err(|e| {
                if e.code == code::CANNOT_COERCE {
                    with_pos(e, loc)
                } else {
                    e
                }
            })?;
        match domain {
            Some(d) => self.domain_check(out, d),
            None => Ok(out),
        }
    }
}
