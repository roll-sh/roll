//! XSLT instructions in a template body (xsltStylePreCompute's per-instruction parts).

use super::compile::{Compiler, is_xslt};
use super::pattern;
use super::style::*;
use crate::types::xml::tree::NodeId;
use std::rc::Rc;

impl Compiler<'_> {
    fn required(&mut self, id: NodeId, name: &str) -> Option<Expr> {
        match self.attr(id, name) {
            Some(s) => self.xpath(id, &s),
            None => {
                self.errors += 1;
                None
            }
        }
    }

    fn opt_avt(&mut self, id: NodeId, name: &str) -> Option<Avt> {
        let v = self.attr(id, name)?;
        Some(if v.contains('{') { self.avt(id, &v) } else { Avt(vec![AvtPart::Lit(v)]) })
    }

    pub(super) fn xslt_instruction(&mut self, id: NodeId, name: &str) -> Option<Instr> {
        Some(match name {
            "apply-templates" => {
                let select = match self.attr(id, "select") {
                    Some(s) => Some(self.xpath(id, &s)?),
                    None => None,
                };
                let mode = match self.attr(id, "mode") {
                    Some(m) => Some(self.qname(id, &m)?),
                    None => None,
                };
                let (sorts, params, _) = self.sorts_and_params(id);
                Instr::ApplyTemplates { select, mode, sorts, params }
            }
            "call-template" => {
                let Some(n) = self.attr(id, "name") else {
                    self.errors += 1;
                    return None;
                };
                let name = self.qname(id, &n)?;
                let (_, params, _) = self.sorts_and_params(id);
                Instr::CallTemplate { name, params }
            }
            "apply-imports" => Instr::ApplyImports,
            "for-each" => {
                let select = self.required(id, "select")?;
                let kids = self.doc.nodes[id].children.clone();
                let n = kids.iter().take_while(|&&k| is_xslt(&self.doc, k) && self.doc.nodes[k].name == "sort").count();
                let (sorts, _, _) = self.sorts_of(&kids[..n]);
                let mut body = Vec::new();
                for &k in &kids[n..] {
                    if is_xslt(&self.doc, k) && self.doc.nodes[k].name == "sort" {
                        body.push(Instr::Fallback(Vec::new()));
                    } else if !(is_xslt(&self.doc, k) && self.doc.nodes[k].name == "param") {
                        body.extend(self.instruction(k));
                    }
                }
                Instr::ForEach { select, sorts, body }
            }
            "value-of" => {
                let e = self.required(id, "select")?;
                Instr::ValueOf(e, self.yes(id, "disable-output-escaping"))
            }
            "copy-of" => Instr::CopyOf(self.required(id, "select")?),
            "copy" => {
                let use_sets = self.use_sets_plain(id);
                Instr::Copy { use_sets, body: self.body(id) }
            }
            "if" => {
                let t = self.required(id, "test")?;
                Instr::If(t, self.body(id))
            }
            "choose" => {
                // Consecutive xsl:when from the first child, then an xsl:otherwise right
                // after them; nothing else runs. Without a first xsl:when it fails.
                let kids = self.doc.nodes[id].children.clone();
                let is = |c: &Self, k: usize, n: &str| {
                    kids.get(k).is_some_and(|&x| is_xslt(&c.doc, x) && c.doc.nodes[x].name == n)
                };
                if !is(self, 0, "when") {
                    return Some(Instr::Fallback(Vec::new()));
                }
                let mut whens = Vec::new();
                let mut k = 0;
                while is(self, k, "when") {
                    if let Some(t) = self.required(kids[k], "test") {
                        let b = self.body(kids[k]);
                        whens.push((t, b));
                    }
                    k += 1;
                }
                let otherwise = if is(self, k, "otherwise") { Some(self.body(kids[k])) } else { None };
                Instr::Choose(whens, otherwise)
            }
            "variable" => Instr::Variable(self.var_decl(id)?),
            "param" => Instr::Param(self.var_decl(id)?),
            "element" => {
                let Some(name) = self.opt_avt(id, "name") else {
                    self.errors += 1;
                    return None;
                };
                let namespace = self.opt_avt(id, "namespace");
                if let Some(n) = name.constant() {
                    if !super::run::valid_qname(&n) {
                        self.errors += 1;
                    } else if let Some((p, _)) = n.split_once(':') {
                        if namespace.is_none() && super::compile::search_ns(&self.doc, id, Some(p)).is_none() {
                            self.errors += 1;
                        }
                        // A prefix starting with "xml" makes the instruction do nothing.
                        if p.len() >= 3 && p[..3].eq_ignore_ascii_case("xml") {
                            return None;
                        }
                    }
                }
                let use_sets = self.use_sets_plain(id);
                Instr::Element { node: id, name, namespace, use_sets, body: self.body(id) }
            }
            "attribute" => {
                let Some(name) = self.opt_avt(id, "name") else {
                    self.errors += 1;
                    return None;
                };
                let namespace = self.opt_avt(id, "namespace");
                if let Some(n) = name.constant() {
                    if !super::run::valid_qname(&n) || n == "xmlns" {
                        self.errors += 1;
                    } else if let Some((p, _)) = n.split_once(':')
                        && namespace.is_none()
                        && super::compile::search_ns(&self.doc, id, Some(p)).is_none()
                    {
                        self.errors += 1;
                    }
                }
                Instr::Attribute { node: id, name, namespace, body: self.body(id) }
            }
            "comment" => Instr::Comment(self.body(id)),
            "processing-instruction" => {
                let Some(name) = self.opt_avt(id, "name") else {
                    self.errors += 1;
                    return None;
                };
                Instr::Pi(name, self.body(id))
            }
            "text" => {
                let noesc = self.yes(id, "disable-output-escaping");
                let mut s = String::new();
                for c in self.doc.nodes[id].children.clone() {
                    s.push_str(&self.doc.nodes[c].content);
                }
                Instr::Text(s, noesc)
            }
            "number" => self.number(id)?,
            "message" => {
                let terminate = match self.attr(id, "terminate").as_deref() {
                    Some("yes") => Some(true),
                    None | Some("no") => Some(false),
                    Some(_) => None,
                };
                Instr::Message { terminate, body: self.body(id) }
            }
            "fallback" => return None,
            "document" => Instr::WriteFile,
            "sort" | "with-param" | "when" | "otherwise" | "template" | "output" | "key"
            | "strip-space" | "preserve-space" | "attribute-set" | "namespace-alias" | "include"
            | "import" | "decimal-format" | "stylesheet" | "transform" => {
                self.errors += 1;
                return None;
            }
            _ => self.unknown(id),
        })
    }

    fn unknown(&mut self, id: NodeId) -> Instr {
        let mut bodies = Vec::new();
        for c in self.doc.nodes[id].children.clone() {
            if is_xslt(&self.doc, c) && self.doc.nodes[c].name == "fallback" {
                bodies.push(self.body(c));
            }
        }
        if bodies.is_empty() && self.forwards {
            return Instr::Fallback(vec![Vec::new()]);
        }
        Instr::Fallback(bodies)
    }

    fn use_sets_plain(&mut self, id: NodeId) -> Vec<QName> {
        let Some(v) = self.attr(id, "use-attribute-sets") else { return Vec::new() };
        v.split([' ', '\t', '\n', '\r'])
            .filter(|e| !e.is_empty())
            .filter_map(|e| self.qname(id, e))
            .collect()
    }

    fn number(&mut self, id: NodeId) -> Option<Instr> {
        let level = match self.attr(id, "level").as_deref() {
            None | Some("single") => Level::Single,
            Some("multiple") => Level::Multiple,
            Some("any") => Level::Any,
            Some(_) => Level::Single,
        };
        let count = match self.attr(id, "count") {
            Some(c) => Some(Rc::new(pattern::compile(self, id, &c).ok()?)),
            None => None,
        };
        let from = match self.attr(id, "from") {
            Some(c) => Some(Rc::new(pattern::compile(self, id, &c).ok()?)),
            None => None,
        };
        let value = match self.attr(id, "value") {
            Some(v) => Some(self.xpath(id, &v)?),
            None => None,
        };
        let format = self.opt_avt(id, "format").unwrap_or_else(|| Avt(vec![AvtPart::Lit("1".into())]));
        Some(Instr::Number(Box::new(NumberInstr {
            level,
            count,
            from,
            value,
            format,
            grouping_separator: self.opt_avt(id, "grouping-separator"),
            grouping_size: self.opt_avt(id, "grouping-size"),
        })))
    }
}
