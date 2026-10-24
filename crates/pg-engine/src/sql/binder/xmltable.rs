//! FROM XMLTABLE(...) (parse_clause.c's transformRangeTableFunc): a function scan whose rows
//! come from the row XPath over the document.

use super::Binder;
use super::ast::{self, inner, with_pos};
use crate::error::{Error, Result, code};
use crate::sql::expr::Expr;
use crate::sql::func::xmltable::{self as xt, TableSpec};
use crate::sql::plan::{OutCol, Plan, SrfCall};
use crate::types::cast::CastContext;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

impl Binder<'_, '_> {
    /// `coerce_to_specific_type_typmod`: the error names the type without its typmod.
    fn coerce_xmltable(&mut self, e: Expr, to: &DataType, n: &pb::Node) -> Result<Expr> {
        let loc = ast::location(n);
        let target = to.without_typmod();
        self.coerce_with(e, to, CastContext::Assignment, &|from| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "argument of XMLTABLE must be type {}, not type {}",
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

    pub(crate) fn bind_xml_table(
        &mut self,
        rtf: &pb::RangeTableFunc,
    ) -> Result<(Plan, super::Scope, Vec<Expr>)> {
        let mut cols: Vec<OutCol> = Vec::new();
        let (call, params) = self.in_level(|b| {
            b.in_clause(super::Clause::FromFunction, |b| {
                let rn = rtf.rowexpr.as_deref().unwrap();
                let row = b.bind_expr(rn)?;
                let row = b.coerce_xmltable(row, &DataType::Text, rn)?;
                let dn = rtf.docexpr.as_deref().unwrap();
                let doc = b.bind_expr(dn)?;
                let doc = b.coerce_xmltable(doc, &DataType::Xml, dn)?;
                let mut args = vec![Expr::Const(Value::Null, DataType::Text), doc, row];
                let mut spec = TableSpec::default();
                let mut col_args = Vec::new();
                for (colno, n) in rtf.columns.iter().enumerate() {
                    let Some(NodeEnum::RangeTableFuncCol(c)) = inner(n) else {
                        continue;
                    };
                    let (ty, domain) = if c.for_ordinality {
                        if spec.ordinality.is_some() {
                            return Err(with_pos(
                                Error::new(
                                    code::SYNTAX_ERROR,
                                    "only one FOR ORDINALITY column is allowed",
                                ),
                                c.location,
                            ));
                        }
                        spec.ordinality = Some(colno);
                        (DataType::Int4, None)
                    } else {
                        let tn = c.type_name.as_ref().unwrap();
                        if tn.setof {
                            return Err(with_pos(
                                Error::new(
                                    code::INVALID_TABLE_DEFINITION,
                                    format!("column \"{}\" cannot be declared SETOF", c.colname),
                                ),
                                c.location,
                            ));
                        }
                        super::resolve_type_name(&b.cat, &b.ctx.search_path, tn)?
                    };
                    let path = match c.colexpr.as_deref() {
                        Some(pn) => {
                            let e = b.bind_expr(pn)?;
                            Some(b.coerce_xmltable(e, &DataType::Text, pn)?)
                        }
                        None => None,
                    };
                    let default = match c.coldefexpr.as_deref() {
                        Some(dn) => {
                            let e = b.bind_expr(dn)?;
                            let e = match b.coerce_xmltable(e, &ty, dn)? {
                                // Shown as get_tablefunc shows the length coercion (with
                                // implicit casts): `(e)::type(n)`.
                                Expr::Cast(i, from, to, false)
                                    if b.deparse && from.same_base(&to) =>
                                {
                                    Expr::Cast(i, from, to, true)
                                }
                                e => e,
                            };
                            Some(match domain {
                                Some(d) => b.domain_check(e, d)?,
                                None => e,
                            })
                        }
                        None => None,
                    };
                    if spec.names.contains(&c.colname) {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                format!("column name \"{}\" is not unique", c.colname),
                            ),
                            c.location,
                        ));
                    }
                    spec.names.push(c.colname.clone());
                    spec.not_null.push(c.is_not_null);
                    spec.types.push(ty.clone());
                    spec.type_names
                        .push(match domain.and_then(|d| b.cat.domain_type(d)) {
                            Some((d, false)) => {
                                b.cat.qualified_name(d.schema, &d.name, &b.ctx.search_path)
                            }
                            _ => ty.display(),
                        });
                    // A domain's constraints, applied to each value read: a cast of a NULL
                    // placeholder carries them.
                    let carrier = match domain {
                        Some(d) => match b.domain_check(Expr::Const(Value::Null, ty.clone()), d)? {
                            e @ Expr::CoerceToDomain(..) => Some(e),
                            _ => None,
                        },
                        None => None,
                    };
                    col_args.push((path, default, carrier));
                    let mut oc = OutCol::new(c.colname.clone(), ty);
                    oc.domain = domain;
                    cols.push(oc);
                }
                for n in &rtf.namespaces {
                    let Some(NodeEnum::ResTarget(r)) = inner(n) else {
                        continue;
                    };
                    let vn = r.val.as_deref().unwrap();
                    let uri = b.bind_expr(vn)?;
                    let uri = b.coerce_xmltable(uri, &DataType::Text, vn)?;
                    if !r.name.is_empty() {
                        if spec
                            .ns_names
                            .iter()
                            .any(|x| x.as_deref() == Some(r.name.as_str()))
                        {
                            return Err(with_pos(
                                Error::new(
                                    code::SYNTAX_ERROR,
                                    format!("namespace name \"{}\" is not unique", r.name),
                                ),
                                r.location,
                            ));
                        }
                        spec.ns_names.push(Some(r.name.clone()));
                    } else {
                        if spec.ns_names.iter().any(Option::is_none) {
                            return Err(with_pos(
                                Error::new(
                                    code::SYNTAX_ERROR,
                                    "only one default namespace is allowed",
                                ),
                                r.location,
                            ));
                        }
                        spec.ns_names.push(None);
                    }
                    args.push(uri);
                }
                for (path, default, carrier) in col_args {
                    let mut at = |e: Option<Expr>| {
                        e.map(|e| {
                            args.push(e);
                            args.len() - 1
                        })
                    };
                    spec.path.push(at(path));
                    spec.default.push(at(default));
                    spec.domain.push(at(carrier));
                }
                args[0] = Expr::Const(
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
                    func: xt::def(),
                    args,
                    ty,
                    ncols: cols.len(),
                    body: None,
                })
            })
        })?;
        let name = rtf
            .alias
            .as_ref()
            .map_or_else(|| "xmltable".to_string(), |a| a.aliasname.clone());
        if let Some(a) = &rtf.alias {
            let names = ast::names(&a.colnames);
            if names.len() > cols.len() {
                return Err(Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    format!(
                        "XMLTABLE function has {} columns available but {} columns specified",
                        cols.len(),
                        names.len()
                    ),
                ));
            }
            for (c, n) in cols.iter_mut().zip(names) {
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
                rti: self.next_rti(),
            },
            super::Scope { rels: vec![rel] },
            params,
        ))
    }
}
