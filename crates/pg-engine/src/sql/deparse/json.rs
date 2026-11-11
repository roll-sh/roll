//! XML and SQL/JSON expressions, XMLTABLE and JSON_TABLE.

use super::expr::{Prec, Side};
use super::{Ctx, D, R, Unhandled, en, node};
use pg_query::NodeEnum as E;
use pg_query::protobuf::{self as pb, Node};

impl D {
    pub(super) fn xml_expr(&mut self, x: &pb::XmlExpr, ctx: Ctx) -> R {
        use pb::XmlExprOp as Op;
        let arg = |i: usize| x.args.get(i).ok_or(Unhandled);
        match en::<Op>(x.op) {
            Op::Undefined | Op::IsXmlconcat => {
                self.s("xmlconcat(");
                self.expr_list(&x.args)?;
                self.c(')');
            }
            Op::IsXmlelement => {
                self.s("xmlelement(name ");
                self.ident_field(&x.name)?;
                if !x.named_args.is_empty() {
                    self.s(", xmlattributes(");
                    self.xml_attributes(&x.named_args)?;
                    self.s(")");
                }
                if !x.args.is_empty() {
                    self.s(", ");
                    self.expr_list(&x.args)?;
                }
                self.s(")");
            }
            Op::IsXmlforest => {
                self.s("xmlforest(");
                self.xml_attributes(&x.named_args)?;
                self.c(')');
            }
            Op::IsXmlparse => {
                // libpg_query drops PRESERVE WHITESPACE; `ast::fix_xmlparse` puts it back.
                self.s("xmlparse(");
                self.xml_option(x.xmloption)?;
                self.expr(arg(0)?, Ctx::AExpr)?;
                self.c(')');
            }
            Op::IsXmlpi => {
                self.s("xmlpi(name ");
                self.ident_field(&x.name)?;
                if !x.args.is_empty() {
                    self.s(", ");
                    self.expr(arg(0)?, Ctx::AExpr)?;
                }
                self.c(')');
            }
            Op::IsXmlroot => {
                self.s("xmlroot(");
                self.expr(arg(0)?, Ctx::AExpr)?;
                self.s(", version ");
                match &arg(1)?.node {
                    Some(E::AConst(c)) if c.isnull => self.s("no value"),
                    Some(E::AConst(_)) => self.expr(arg(1)?, Ctx::AExpr)?,
                    _ => return Err(Unhandled),
                }
                let Some(E::AConst(c)) = &arg(2)?.node else {
                    return Err(Unhandled);
                };
                let Some(pb::a_const::Val::Ival(i)) = c.val.as_ref().filter(|_| !c.isnull) else {
                    return Err(Unhandled);
                };
                match i.ival {
                    0 => self.s(", standalone yes"),
                    1 => self.s(", standalone no"),
                    2 => self.s(", standalone no value"),
                    _ => {}
                }
                self.c(')');
            }
            Op::IsXmlserialize => return Err(Unhandled),
            Op::IsDocument => {
                self.operand(arg(0)?, Prec::Is, Side::Left, ctx)?;
                self.s(" IS DOCUMENT");
            }
        }
        Ok(())
    }

    fn xml_option(&mut self, o: i32) -> R {
        self.s(match en::<pb::XmlOptionType>(o) {
            pb::XmlOptionType::XmloptionContent => "content ",
            _ => "document ",
        });
        Ok(())
    }

    /// `xml_attribute_list`.
    fn xml_attributes(&mut self, l: &[Node]) -> R {
        for (i, t) in l.iter().enumerate() {
            let Some(E::ResTarget(t)) = &t.node else {
                return Err(Unhandled);
            };
            if let Some(v) = t.val.as_deref() {
                self.expr(v, Ctx::AExpr)?;
            }
            if !t.name.is_empty() {
                self.s(" AS ");
                self.ident(&t.name);
            }
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        Ok(())
    }

    pub(super) fn xml_serialize(&mut self, x: &pb::XmlSerialize) -> R {
        self.s("xmlserialize(");
        self.xml_option(x.xmloption)?;
        if let Some(e) = x.expr.as_deref() {
            self.expr(e, Ctx::AExpr)?;
        }
        self.s(" AS ");
        self.type_name(x.type_name.as_ref().ok_or(Unhandled)?)?;
        if x.indent {
            self.s(" INDENT");
        }
        self.s(")");
        Ok(())
    }

    /// XMLTABLE.
    pub(super) fn range_table_func(&mut self, f: &pb::RangeTableFunc) -> R {
        if f.lateral {
            self.s("LATERAL ");
        }
        self.s("xmltable(");
        if !f.namespaces.is_empty() {
            self.s("xmlnamespaces(");
            for (i, t) in f.namespaces.iter().enumerate() {
                let Some(E::ResTarget(t)) = &t.node else {
                    return Err(Unhandled);
                };
                if t.name.is_empty() {
                    self.s("DEFAULT ");
                }
                self.bexpr(node(&t.val)?)?;
                if !t.name.is_empty() {
                    self.s(" AS ");
                    self.ident(&t.name);
                }
                if i + 1 < f.namespaces.len() {
                    self.s(", ");
                }
            }
            self.s("), ");
        }
        self.c('(');
        if let Some(r) = f.rowexpr.as_deref() {
            self.expr(r, Ctx::None)?;
        }
        self.c(')');
        self.s(" PASSING ");
        self.cexpr(node(&f.docexpr)?)?;
        self.s(" COLUMNS ");
        for (i, c) in f.columns.iter().enumerate() {
            let Some(E::RangeTableFuncCol(c)) = &c.node else {
                return Err(Unhandled);
            };
            self.ident_field(&c.colname)?;
            self.c(' ');
            if c.for_ordinality {
                self.s("FOR ORDINALITY ");
            } else {
                self.type_name(c.type_name.as_ref().ok_or(Unhandled)?)?;
                self.c(' ');
                if let Some(e) = c.colexpr.as_deref() {
                    self.s("PATH ");
                    self.bexpr(e)?;
                    self.c(' ');
                }
                if let Some(e) = c.coldefexpr.as_deref() {
                    self.s("DEFAULT ");
                    self.bexpr(e)?;
                    self.c(' ');
                }
                if c.is_not_null {
                    self.s("NOT NULL ");
                }
            }
            self.strip();
            if i + 1 < f.columns.len() {
                self.s(", ");
            }
        }
        self.s(") ");
        if let Some(a) = &f.alias {
            self.s("AS ");
            self.alias(a)?;
        }
        self.strip();
        Ok(())
    }

    fn json_format(&mut self, f: Option<&pb::JsonFormat>) {
        use pb::JsonEncoding as Enc;
        let Some(f) = f else { return };
        if matches!(
            en::<pb::JsonFormatType>(f.format_type),
            pb::JsonFormatType::Undefined | pb::JsonFormatType::JsFormatDefault
        ) {
            return;
        }
        self.s("FORMAT JSON ");
        match en::<Enc>(f.encoding) {
            Enc::JsEncUtf8 => self.s("ENCODING utf8 "),
            Enc::JsEncUtf16 => self.s("ENCODING utf16 "),
            Enc::JsEncUtf32 => self.s("ENCODING utf32 "),
            _ => {}
        }
    }

    pub(super) fn json_is_predicate(&mut self, j: &pb::JsonIsPredicate) -> R {
        use pb::JsonValueType as T;
        self.operand(node(&j.expr)?, Prec::Is, Side::Left, Ctx::AExpr)?;
        self.c(' ');
        self.json_format(j.format.as_ref());
        self.s("IS ");
        self.s(match en::<T>(j.item_type) {
            T::JsTypeArray => "JSON ARRAY ",
            T::JsTypeObject => "JSON OBJECT ",
            T::JsTypeScalar => "JSON SCALAR ",
            _ => "JSON ",
        });
        if j.unique_keys {
            self.s("WITH UNIQUE ");
        }
        self.strip();
        Ok(())
    }

    /// `json_value_expr`.
    fn json_value(&mut self, v: Option<&pb::JsonValueExpr>) -> R {
        let v = v.ok_or(Unhandled)?;
        if let Some(e) = v.raw_expr.as_deref() {
            self.expr(e, Ctx::AExpr)?;
        }
        self.c(' ');
        self.json_format(v.format.as_ref());
        Ok(())
    }

    fn json_key_value(&mut self, kv: &pb::JsonKeyValue) -> R {
        if let Some(k) = kv.key.as_deref() {
            self.expr(k, Ctx::AExpr)?;
        }
        self.s(": ");
        self.json_value(kv.value.as_deref())
    }

    fn json_output(&mut self, o: Option<&pb::JsonOutput>) -> R {
        let Some(o) = o else { return Ok(()) };
        let ret = o.returning.as_ref().ok_or(Unhandled)?;
        self.s("RETURNING ");
        self.type_name(o.type_name.as_ref().ok_or(Unhandled)?)?;
        self.c(' ');
        self.json_format(ret.format.as_ref());
        Ok(())
    }

    pub(super) fn json_object_agg(&mut self, j: &pb::JsonObjectAgg) -> R {
        let c = node(&j.constructor)?;
        self.s("JSON_OBJECTAGG(");
        self.json_key_value(node(&j.arg)?)?;
        if j.absent_on_null {
            self.s("ABSENT ON NULL ");
        }
        if j.unique {
            self.s("WITH UNIQUE ");
        }
        self.json_output(c.output.as_ref())?;
        self.strip();
        self.s(") ");
        self.filter_over(c.agg_filter.as_deref(), c.over.as_deref())
    }

    pub(super) fn json_array_agg(&mut self, j: &pb::JsonArrayAgg) -> R {
        let c = node(&j.constructor)?;
        self.s("JSON_ARRAYAGG(");
        self.json_value(j.arg.as_deref())?;
        self.sort_clause(&c.agg_order, Ctx::None)?;
        if !j.absent_on_null {
            self.s("NULL ON NULL ");
        }
        self.json_output(c.output.as_ref())?;
        self.strip();
        self.s(") ");
        self.filter_over(c.agg_filter.as_deref(), c.over.as_deref())
    }

    pub(super) fn json_object_constructor(&mut self, j: &pb::JsonObjectConstructor) -> R {
        self.s("JSON_OBJECT(");
        for (i, kv) in j.exprs.iter().enumerate() {
            let Some(E::JsonKeyValue(kv)) = &kv.node else {
                return Err(Unhandled);
            };
            self.json_key_value(kv)?;
            self.strip();
            if i + 1 < j.exprs.len() {
                self.s(", ");
            }
        }
        self.c(' ');
        if j.absent_on_null {
            self.s("ABSENT ON NULL ");
        }
        if j.unique {
            self.s("WITH UNIQUE ");
        }
        self.json_output(j.output.as_ref())?;
        self.strip();
        self.c(')');
        Ok(())
    }

    pub(super) fn json_array_constructor(&mut self, j: &pb::JsonArrayConstructor) -> R {
        self.s("JSON_ARRAY(");
        for (i, v) in j.exprs.iter().enumerate() {
            let Some(E::JsonValueExpr(v)) = &v.node else {
                return Err(Unhandled);
            };
            self.json_value(Some(v))?;
            self.strip();
            if i + 1 < j.exprs.len() {
                self.s(", ");
            }
        }
        self.c(' ');
        if !j.absent_on_null {
            self.s("NULL ON NULL ");
        }
        self.json_output(j.output.as_ref())?;
        self.strip();
        self.c(')');
        Ok(())
    }

    pub(super) fn json_array_query_constructor(
        &mut self,
        j: &pb::JsonArrayQueryConstructor,
    ) -> R {
        let Some(E::SelectStmt(q)) = j.query.as_deref().and_then(|n| n.node.as_ref()) else {
            return Err(Unhandled);
        };
        self.s("JSON_ARRAY(");
        self.select_stmt(q, Ctx::None)?;
        self.json_format(j.format.as_ref());
        self.json_output(j.output.as_ref())?;
        self.strip();
        self.c(')');
        Ok(())
    }

    pub(super) fn json_parse_expr(&mut self, j: &pb::JsonParseExpr) -> R {
        self.s("JSON(");
        self.json_value(j.expr.as_deref())?;
        if j.unique_keys {
            self.s(" WITH UNIQUE KEYS");
        }
        self.s(")");
        Ok(())
    }

    pub(super) fn json_scalar_expr(&mut self, j: &pb::JsonScalarExpr) -> R {
        self.s("JSON_SCALAR(");
        if let Some(e) = j.expr.as_deref() {
            self.expr(e, Ctx::AExpr)?;
        }
        self.s(")");
        Ok(())
    }

    pub(super) fn json_serialize_expr(&mut self, j: &pb::JsonSerializeExpr) -> R {
        self.s("JSON_SERIALIZE(");
        self.json_value(j.expr.as_deref())?;
        self.json_output(j.output.as_ref())?;
        self.s(")");
        Ok(())
    }

    /// `PASSING` arguments of JSON_EXISTS / JSON_QUERY / JSON_VALUE / JSON_TABLE.
    fn json_passing(&mut self, l: &[Node]) -> R {
        if !l.is_empty() {
            self.s(" PASSING ");
        }
        for (i, a) in l.iter().enumerate() {
            let Some(E::JsonArgument(a)) = &a.node else {
                return Err(Unhandled);
            };
            self.json_value(a.val.as_deref())?;
            self.s(" AS ");
            self.ident_field(&a.name)?;
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        Ok(())
    }

    /// Wrapper, quotes, ON EMPTY and ON ERROR clauses; `NONE` wrappers only where shown.
    fn json_clauses(
        &mut self,
        wrapper: i32,
        show_none: bool,
        quotes: i32,
        on_empty: Option<&pb::JsonBehavior>,
        on_error: Option<&pb::JsonBehavior>,
    ) -> R {
        use pb::JsonQuotes as Q;
        use pb::JsonWrapper as W;
        match en::<W>(wrapper) {
            W::JswNone if show_none => self.s(" WITHOUT WRAPPER"),
            W::JswConditional => self.s(" WITH CONDITIONAL WRAPPER"),
            W::JswUnconditional => self.s(" WITH UNCONDITIONAL WRAPPER"),
            _ => {}
        }
        match en::<Q>(quotes) {
            Q::JsQuotesKeep => self.s(" KEEP QUOTES"),
            Q::JsQuotesOmit => self.s(" OMIT QUOTES"),
            _ => {}
        }
        if let Some(b) = on_empty {
            self.c(' ');
            self.json_behavior(b)?;
            self.s(" ON EMPTY");
        }
        if let Some(b) = on_error {
            self.c(' ');
            self.json_behavior(b)?;
            self.s(" ON ERROR");
        }
        Ok(())
    }

    pub(super) fn json_func_expr(&mut self, j: &pb::JsonFuncExpr) -> R {
        use pb::JsonExprOp as Op;
        self.s(match en::<Op>(j.op) {
            Op::JsonQueryOp => "JSON_QUERY(",
            Op::JsonValueOp => "JSON_VALUE(",
            Op::JsonTableOp => "JSON_TABLE(",
            _ => "JSON_EXISTS(",
        });
        self.json_value(j.context_item.as_deref())?;
        self.s(", ");
        if let Some(p) = j.pathspec.as_deref() {
            self.expr(p, Ctx::AExpr)?;
        }
        self.json_passing(&j.passing)?;
        if let Some(o) = &j.output {
            self.c(' ');
            self.json_output(Some(o))?;
        }
        self.json_clauses(
            j.wrapper,
            true,
            j.quotes,
            j.on_empty.as_deref(),
            j.on_error.as_deref(),
        )?;
        self.c(')');
        Ok(())
    }

    fn json_path_spec(&mut self, p: Option<&pb::JsonTablePathSpec>) -> R {
        let p = p.ok_or(Unhandled)?;
        let Some(E::AConst(c)) = p.string.as_deref().and_then(|n| n.node.as_ref()) else {
            return Err(Unhandled);
        };
        let Some(pb::a_const::Val::Sval(s)) = c.val.as_ref().filter(|_| !c.isnull) else {
            return Err(Unhandled);
        };
        self.literal(&s.sval);
        if !p.name.is_empty() {
            self.s(" AS ");
            self.ident(&p.name);
        }
        Ok(())
    }

    fn json_behavior(&mut self, b: &pb::JsonBehavior) -> R {
        use pb::JsonBehaviorType as B;
        self.s(match en::<B>(b.btype) {
            B::JsonBehaviorError => "ERROR",
            B::JsonBehaviorEmpty => "EMPTY",
            B::JsonBehaviorTrue => "TRUE",
            B::JsonBehaviorFalse => "FALSE",
            B::JsonBehaviorEmptyArray => "EMPTY ARRAY",
            B::JsonBehaviorEmptyObject => "EMPTY OBJECT",
            B::JsonBehaviorUnknown => "UNKNOWN",
            B::JsonBehaviorDefault => {
                self.s("DEFAULT ");
                if let Some(e) = b.expr.as_deref() {
                    self.expr(e, Ctx::AExpr)?;
                }
                return Ok(());
            }
            _ => "NULL",
        });
        Ok(())
    }

    fn json_table_columns(&mut self, l: &[Node]) -> R {
        use pb::JsonTableColumnType as T;
        self.s(" COLUMNS (");
        for (i, c) in l.iter().enumerate() {
            let Some(E::JsonTableColumn(c)) = &c.node else {
                return Err(Unhandled);
            };
            let t = en::<T>(c.coltype);
            if t == T::JtcNested {
                self.s("NESTED PATH ");
                self.json_path_spec(c.pathspec.as_deref())?;
                self.json_table_columns(&c.columns)?;
            } else {
                self.ident_field(&c.name)?;
                self.c(' ');
                if t == T::JtcExists || t == T::JtcFormatted || t == T::JtcRegular {
                    self.type_name(c.type_name.as_ref().ok_or(Unhandled)?)?;
                    self.s(if t == T::JtcExists { " EXISTS " } else { " " });
                    self.json_format(c.format.as_ref());
                    if c.pathspec.is_some() {
                        self.s("PATH ");
                        self.json_path_spec(c.pathspec.as_deref())?;
                    }
                } else {
                    self.s(" FOR ORDINALITY");
                }
                self.json_clauses(
                    c.wrapper,
                    t == T::JtcRegular || t == T::JtcFormatted,
                    c.quotes,
                    c.on_empty.as_deref(),
                    c.on_error.as_deref(),
                )?;
            }
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        self.c(')');
        Ok(())
    }

    pub(super) fn json_table(&mut self, t: &pb::JsonTable) -> R {
        self.s("JSON_TABLE(");
        self.json_value(t.context_item.as_deref())?;
        self.s(", ");
        self.json_path_spec(t.pathspec.as_deref())?;
        self.json_passing(&t.passing)?;
        self.json_table_columns(&t.columns)?;
        if let Some(b) = t.on_error.as_deref() {
            self.json_behavior(b)?;
            self.s(" ON ERROR");
        }
        self.c(')');
        if let Some(a) = &t.alias {
            self.c(' ');
            self.alias(a)?;
        }
        Ok(())
    }
}
