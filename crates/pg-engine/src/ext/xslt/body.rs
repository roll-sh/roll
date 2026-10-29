//! Template bodies (xsltParseTemplateContent and xsltStylePreCompute): stylesheet nodes to
//! instructions.

use super::compile::{Compiler, is_xslt, ns_attr};
use super::style::*;
use crate::types::xml::tree::{DOC, Kind, NodeId};
use std::rc::Rc;

/// The extension elements libxslt registers (xsltRegisterAllExtras) that write files.
const WRITERS: [(&str, &str); 4] = [
    ("http://icl.com/saxon", "output"),
    ("org.apache.xalan.xslt.extensions.Redirect", "write"),
    ("http://www.jclark.com/xt", "document"),
    (XSLT_NS, "document"),
];

impl Compiler<'_> {
    /// A template's leading xsl:param elements and the rest of its content.
    pub(super) fn template_body(&mut self, id: NodeId) -> (Vec<VarDecl>, Vec<Instr>) {
        let kids = self.doc.nodes[id].children.clone();
        let mut params = Vec::new();
        let (mut i, mut start) = (0, 0);
        while i < kids.len() {
            let k = kids[i];
            if self.doc.nodes[k].kind == Kind::Text {
                i += 1;
                continue;
            }
            if !(is_xslt(&self.doc, k) && self.doc.nodes[k].name == "param") {
                break;
            }
            if let Some(d) = self.var_decl(k) {
                params.push(d);
            }
            i += 1;
            start = i;
        }
        let body = self.sequence(&kids[start..]);
        (params, body)
    }

    /// The content of a literal-result-element stylesheet: its root element.
    pub(super) fn body_of_doc(&mut self) -> Vec<Instr> {
        let kids = self.doc.nodes[DOC].children.clone();
        self.sequence(&kids)
    }

    pub(super) fn body(&mut self, id: NodeId) -> Vec<Instr> {
        let kids = self.doc.nodes[id].children.clone();
        self.sequence(&kids)
    }

    fn sequence(&mut self, kids: &[NodeId]) -> Vec<Instr> {
        let mut out = Vec::new();
        for &c in kids {
            // A misplaced xsl:param is dropped with a warning.
            if is_xslt(&self.doc, c) && self.doc.nodes[c].name == "param" {
                continue;
            }
            out.extend(self.instruction(c));
        }
        out
    }

    pub(super) fn var_decl(&mut self, id: NodeId) -> Option<VarDecl> {
        let Some(name) = self.attr(id, "name") else {
            self.errors += 1;
            return None;
        };
        let name = self.qname(id, &name)?;
        let select = match self.attr(id, "select") {
            Some(s) => Some(self.xpath(id, &s)?),
            None => None,
        };
        // (Only xsl:variable: xsl:param and xsl:with-param warn.)
        if select.is_some() && !self.doc.nodes[id].children.is_empty() && self.doc.nodes[id].name == "variable" {
            self.errors += 1;
        }
        let body = if select.is_some() { Vec::new() } else { self.body(id) };
        Some(VarDecl { name, select, body: Rc::new(body) })
    }

    /// An attribute value template (`xsltCompileAttr` / the run-time AVTs).
    pub(super) fn avt(&mut self, id: NodeId, s: &str) -> Avt {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'{' if b.get(i + 1) == Some(&b'{') => {
                    lit.push('{');
                    i += 2;
                }
                b'{' => {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && b[j] != b'}' {
                        if b[j] == b'\'' || b[j] == b'"' {
                            let d = b[j];
                            j += 1;
                            while j < b.len() && b[j] != d {
                                j += 1;
                            }
                            if j < b.len() {
                                j += 1;
                            }
                        } else {
                            j += 1;
                        }
                    }
                    if j >= b.len() {
                        self.errors += 1;
                        break;
                    }
                    if j > start {
                        if !lit.is_empty() {
                            parts.push(AvtPart::Lit(std::mem::take(&mut lit)));
                        }
                        if let Some(e) = self.xpath(id, &s[start..j]) {
                            parts.push(AvtPart::Expr(e));
                        }
                    }
                    i = j + 1;
                }
                b'}' if b.get(i + 1) == Some(&b'}') => {
                    lit.push('}');
                    i += 2;
                }
                b'}' => {
                    // An unmatched '}' ends the template there, and fails it.
                    lit.clear();
                    parts.push(AvtPart::Fail);
                    break;
                }
                _ => {
                    let ch = s[i..].chars().next().unwrap_or('\0');
                    lit.push(ch);
                    i += ch.len_utf8().max(1);
                }
            }
        }
        if !lit.is_empty() {
            parts.push(AvtPart::Lit(lit));
        }
        Avt(parts)
    }

    fn avt_attr(&mut self, id: NodeId, name: &str) -> Option<Avt> {
        let v = self.attr(id, name)?;
        Some(if v.contains('{') { self.avt(id, &v) } else { Avt(vec![AvtPart::Lit(v)]) })
    }

    pub(super) fn sorts_and_params(&mut self, id: NodeId) -> (Vec<Sort>, Vec<VarDecl>, Vec<NodeId>) {
        let kids = self.doc.nodes[id].children.clone();
        self.sorts_of(&kids)
    }

    pub(super) fn sorts_of(&mut self, kids: &[NodeId]) -> (Vec<Sort>, Vec<VarDecl>, Vec<NodeId>) {
        let (mut sorts, mut params, mut rest) = (Vec::new(), Vec::new(), Vec::new());
        for &c in kids {
            if is_xslt(&self.doc, c) {
                match self.doc.nodes[c].name.as_str() {
                    "sort" => {
                        let select = match self.attr(c, "select") {
                            Some(s) => self.xpath(c, &s),
                            None => self.xpath(c, "."),
                        };
                        let data_type = self.avt_attr(c, "data-type");
                        let order = self.avt_attr(c, "order");
                        if let Some(select) = select {
                            sorts.push(Sort { select, data_type, order });
                        }
                        continue;
                    }
                    "with-param" => {
                        if let Some(d) = self.var_decl(c) {
                            params.push(d);
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            rest.push(c);
        }
        (sorts, params, rest)
    }

    pub(super) fn yes(&mut self, id: NodeId, name: &str) -> bool {
        match self.attr(id, name).as_deref() {
            Some("yes") => true,
            Some("no") | None => false,
            Some(_) => false,
        }
    }

    /// One stylesheet node of a sequence constructor.
    pub(super) fn instruction(&mut self, id: NodeId) -> Option<Instr> {
        let n = &self.doc.nodes[id];
        match n.kind {
            Kind::Text | Kind::CData => return Some(Instr::Text(n.content.clone(), false)),
            Kind::Element => {}
            _ => return None,
        }
        let uri = n.ns.as_ref().and_then(|ns| ns.href.clone());
        let name = n.name.clone();
        if uri.as_deref() == Some(XSLT_NS) {
            return self.xslt_instruction(id, &name);
        }
        if self.is_extension(uri.as_deref()) {
            if WRITERS.iter().any(|(u, l)| Some(*u) == uri.as_deref() && *l == name) {
                return Some(Instr::WriteFile);
            }
            if uri.as_deref() == Some("http://xmlsoft.org/XSLT/namespace") && name == "debug" {
                return None;
            }
            return Some(self.fallbacks(id));
        }
        let mut attrs = Vec::new();
        for a in n.attrs.clone() {
            let an = &self.doc.nodes[a];
            if an.ns.as_ref().and_then(|ns| ns.href.as_deref()) == Some(XSLT_NS) {
                continue;
            }
            let v = self.doc.content(a);
            let avt = self.avt(id, &v);
            attrs.push((a, avt));
        }
        let use_sets = self.use_sets_ns(id);
        let body = self.body(id);
        Some(Instr::Literal(Box::new(Literal { node: id, attrs, use_sets, body })))
    }

    fn use_sets_ns(&mut self, id: NodeId) -> Vec<QName> {
        let Some(v) = ns_attr(&self.doc, id, "use-attribute-sets") else { return Vec::new() };
        v.split([' ', '\t', '\n', '\r'])
            .filter(|e| !e.is_empty())
            .filter_map(|e| self.qname(id, e))
            .collect()
    }

    /// An element libxslt can't run: its xsl:fallback children's bodies.
    fn fallbacks(&mut self, id: NodeId) -> Instr {
        let mut bodies = Vec::new();
        for c in self.doc.nodes[id].children.clone() {
            if is_xslt(&self.doc, c) && self.doc.nodes[c].name == "fallback" {
                bodies.push(self.body(c));
            }
        }
        Instr::Fallback(bodies)
    }
}
