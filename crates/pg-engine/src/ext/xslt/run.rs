//! Running sequence constructors (xsltApplySequenceConstructor) and the instructions that
//! write to the result.

use super::apply::Ctx;
use super::compile::search_ns;
use super::exec::{Exec, R, Stop};
use super::style::*;
use crate::types::xml::tree::{DOC, Doc, Kind, Node, Ns};
use crate::types::xml::xpath::{XNode, XValue};
use std::rc::Rc;

impl Exec<'_, '_> {
    /// A nested sequence constructor (an instruction's content).
    pub fn run(&mut self, body: &[Instr], c: &Ctx) -> R<()> {
        self.run_seq(body, c, false)
    }

    /// A template's body: its top-level literal result elements get the template's
    /// inherited namespaces.
    pub fn run_top(&mut self, body: &[Instr], c: &Ctx) -> R<()> {
        self.run_seq(body, c, true)
    }

    fn run_seq(&mut self, body: &[Instr], c: &Ctx, top: bool) -> R<()> {
        self.enter()?;
        let len = self.vars.len();
        let mut r = Ok(());
        for i in body {
            r = self.instr(i, c, top);
            if r.is_err() {
                break;
            }
        }
        self.vars.truncate(len.max(self.base));
        self.depth -= 1;
        r
    }

    /// A result tree fragment from a variable's content.
    pub fn fragment(&mut self, body: &Rc<Vec<Instr>>, c: &Ctx) -> R<XValue> {
        let old_out = std::mem::replace(&mut self.out, Doc::new(None));
        let old_insert = std::mem::replace(&mut self.insert, DOC);
        let old_templ = self.templ.take();
        let r = self.run(body, c);
        self.templ = old_templ;
        self.insert = old_insert;
        let mut frag = std::mem::replace(&mut self.out, old_out);
        r?;
        frag.nodes[DOC].name = " fake node libxslt".into();
        let root = self.x.graft(frag);
        Ok(XValue::Tree(vec![XNode::Node(root)]))
    }

    /// `xsltEvalTemplateString`: the content run into a scratch element, as its text.
    pub fn template_string(&mut self, body: &[Instr], c: &Ctx) -> R<String> {
        let tmp = self.out.add(Node::new(Kind::Element, "fake"));
        let old_insert = std::mem::replace(&mut self.insert, tmp);
        let r = self.run(body, c);
        self.insert = old_insert;
        r?;
        Ok(self.out.content(tmp))
    }

    fn instr(&mut self, i: &Instr, c: &Ctx, top: bool) -> R<()> {
        match i {
            Instr::Text(s, noesc) => {
                if !s.is_empty() {
                    self.add_text(s, *noesc);
                }
            }
            Instr::Literal(l) => self.literal(l, c, top)?,
            Instr::ApplyTemplates { select, mode, sorts, params } => {
                self.apply_templates(select.as_ref(), mode, sorts, params, c)?
            }
            Instr::CallTemplate { name, params } => self.call_template(name, params, c)?,
            Instr::ApplyImports => {
                // There are no imported stylesheets: the built-in rule applies.
                if self.rule.is_none() {
                    self.failed = true;
                } else {
                    self.builtin(c, &Vec::new())?;
                }
            }
            Instr::ForEach { select, sorts, body } => self.for_each(select, sorts, body, c)?,
            Instr::ValueOf(e, noesc) => {
                let v = self.eval_string(e, &c.node, c.pos, c.size)?;
                if !v.is_empty() {
                    self.add_text(&v, *noesc);
                }
            }
            Instr::CopyOf(e) => {
                let v = self.eval(e, &c.node, c.pos, c.size)?;
                self.copy_of(v);
            }
            Instr::Copy { use_sets, body } => self.copy(use_sets, body, c)?,
            Instr::If(t, body) => {
                if self.eval_bool(t, &c.node, c.pos, c.size)? {
                    self.run(body, c)?;
                }
            }
            Instr::Choose(whens, otherwise) => {
                let mut done = false;
                for (t, body) in whens {
                    if self.eval_bool(t, &c.node, c.pos, c.size)? {
                        self.run(body, c)?;
                        done = true;
                        break;
                    }
                }
                if !done && let Some(b) = otherwise {
                    self.run(b, c)?;
                }
            }
            Instr::Variable(d) | Instr::Param(d) => {
                if self.vars[self.base..].iter().any(|(q, _)| *q == d.name) {
                    self.failed = true;
                }
                let v = self.var_value(d, c)?;
                self.vars.push((d.name.clone(), v));
            }
            Instr::Element { node, name, namespace, use_sets, body } => {
                self.element(*node, name, namespace.as_ref(), use_sets, body, c)?
            }
            Instr::Attribute { node, name, namespace, body } => self.attribute(*node, name, namespace.as_ref(), body, c)?,
            Instr::Comment(body) => {
                let v = self.template_string(body, c)?;
                if v.ends_with('-') || v.contains("--") {
                    self.failed = true;
                }
                self.add_node(Node::new(Kind::Comment, "comment"), &v);
            }
            Instr::Pi(name, body) => {
                let name = self.avt(name, &c.node, c.pos, c.size)?;
                let v = self.template_string(body, c)?;
                if v.contains("?>") {
                    self.failed = true;
                    return Ok(());
                }
                self.add_node(Node::new(Kind::Pi, name), &v);
            }
            Instr::Number(n) => self.number(n, c)?,
            Instr::Message { terminate, body } => {
                self.template_string(body, c)?;
                match terminate {
                    Some(true) => {
                        self.failed = true;
                        return Err(Stop::Stopped);
                    }
                    Some(false) => {}
                    None => self.failed = true,
                }
            }
            Instr::Fallback(bodies) => {
                if bodies.is_empty() {
                    self.failed = true;
                }
                for b in bodies {
                    self.run(b, c)?;
                }
            }
            Instr::WriteFile => self.failed = true,
        }
        Ok(())
    }

    /// A literal result element (xsltShallowCopyElem with isLRE, the inherited namespaces,
    /// xsltAttrListTemplateProcess).
    fn literal(&mut self, l: &Literal, c: &Ctx, top: bool) -> R<()> {
        let sd = &self.s.doc;
        let src = &sd.nodes[l.node];
        let el = self.out.add(Node::new(Kind::Element, src.name.clone()));
        self.append(el);
        let defs = src.ns_defs().to_vec();
        if !defs.is_empty() {
            self.copy_ns_list(el, &defs, true);
        }
        if let Some(ns) = src.ns.clone() {
            self.out.nodes[el].ns = self.get_namespace(&ns, el);
        } else if self.parent_has_ns(el) {
            self.special_ns(None, None, el);
        }
        if top && let Some(t) = self.templ {
            for ns in self.s.templates[t].inherited_ns.clone() {
                let uri = match ns.href.as_ref().and_then(|h| self.s.aliases.get(h)) {
                    Some(None) => continue,
                    Some(Some(u)) => Some(u.clone()),
                    None => ns.href.clone(),
                };
                let found = self.out_search_ns(el, ns.prefix.as_deref());
                if found.as_ref().is_none_or(|f| f.href != uri) {
                    self.new_ns(el, uri, ns.prefix.clone());
                }
            }
            if let Some(ns) = src.ns.clone() {
                self.out.nodes[el].ns = self.get_namespace(&ns, el);
            }
        }
        let old_insert = std::mem::replace(&mut self.insert, el);
        let r = self.literal_content(l, c);
        self.insert = old_insert;
        r
    }

    fn literal_content(&mut self, l: &Literal, c: &Ctx) -> R<()> {
        if !l.use_sets.is_empty() {
            self.attribute_sets(&l.use_sets, c)?;
        }
        for (a, avt) in &l.attrs {
            let an = &self.s.doc.nodes[*a];
            let (name, ns) = (an.name.clone(), an.ns.clone());
            let v = self.avt(avt, &c.node, c.pos, c.size)?;
            let ns = match ns {
                Some(ns) => self.get_namespace(&ns, self.insert),
                None => None,
            };
            self.set_attr(self.insert, ns, &name, &v, false);
        }
        self.run(&l.body, c)
    }

    pub fn attribute_sets(&mut self, names: &[QName], c: &Ctx) -> R<()> {
        for n in names {
            // (An unknown set adds nothing.)
            let Some(attrs) = self.s.attr_sets.get(n) else { continue };
            let old_base = std::mem::replace(&mut self.base, self.vars.len());
            let r = self.run(attrs, c);
            self.base = old_base;
            r?;
        }
        Ok(())
    }

    fn parent_has_ns(&self, el: usize) -> bool {
        self.out.parent(el).is_some_and(|p| self.out.nodes[p].kind == Kind::Element && self.out.nodes[p].ns.is_some())
    }

    /// xsl:copy (xsltCopy).
    fn copy(&mut self, use_sets: &[QName], body: &[Instr], c: &Ctx) -> R<()> {
        let id = match &c.node {
            XNode::Ns(ns) => {
                let ns = Ns { prefix: ns.prefix.clone(), href: ns.href.clone() };
                self.copy_ns_node(&ns);
                return Ok(());
            }
            XNode::Node(id) => *id,
        };
        match self.x.doc.nodes[id].kind {
            Kind::Document => self.run(body, c),
            Kind::Element => {
                let el = self.shallow_copy_element(id);
                let old = std::mem::replace(&mut self.insert, el);
                let mut r = Ok(());
                if !use_sets.is_empty() {
                    r = self.attribute_sets(use_sets, c);
                }
                if r.is_ok() {
                    r = self.run(body, c);
                }
                self.insert = old;
                r
            }
            Kind::Attribute => {
                self.copy_attr(id);
                Ok(())
            }
            Kind::Text | Kind::CData => {
                self.copy_text(id);
                Ok(())
            }
            Kind::Comment | Kind::Pi => {
                let src = &self.x.doc.nodes[id];
                let (n, c) = (Node::new(src.kind, src.name.clone()), self.x.doc.text(src).to_string());
                self.add_node(n, &c);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// xsl:element (xsltElement).
    fn element(&mut self, inst: usize, name: &Avt, namespace: Option<&Avt>, use_sets: &[QName], body: &[Instr], c: &Ctx) -> R<()> {
        let qname = self.avt(name, &c.node, c.pos, c.size)?;
        if !valid_qname(&qname) {
            self.failed = true;
            return Ok(());
        }
        let (prefix, local) = match qname.split_once(':') {
            Some((p, l)) => (Some(p.to_string()), l.to_string()),
            None => (None, qname.clone()),
        };
        let uri = match namespace {
            Some(a) => {
                let u = self.avt(a, &c.node, c.pos, c.size)?;
                (!u.is_empty()).then_some(u)
            }
            None => match search_ns(&self.s.doc, inst, prefix.as_deref()) {
                Some(ns) => ns.href.clone(),
                None if prefix.is_some() => {
                    self.failed = true;
                    return Ok(());
                }
                None => None,
            },
        };
        let prefix = if uri.is_none() { None } else { prefix };
        let el = self.out.add(Node::new(Kind::Element, local));
        self.append(el);
        if uri.is_some() {
            let ns = self.special_ns(uri.as_deref(), prefix.as_deref(), el);
            self.out.nodes[el].ns = ns;
        } else if self.parent_has_ns(el) {
            self.special_ns(None, None, el);
        }
        let old = std::mem::replace(&mut self.insert, el);
        let mut r = Ok(());
        if !use_sets.is_empty() {
            r = self.attribute_sets(use_sets, c);
        }
        if r.is_ok() {
            r = self.run(body, c);
        }
        self.insert = old;
        r
    }

    /// xsl:attribute (xsltAttribute).
    fn attribute(&mut self, inst: usize, name: &Avt, namespace: Option<&Avt>, body: &[Instr], c: &Ctx) -> R<()> {
        let target = self.insert;
        if self.out.nodes[target].kind != Kind::Element {
            return Ok(());
        }
        if self.out.has_children(target) {
            self.failed = true;
            return Ok(());
        }
        let qname = self.avt(name, &c.node, c.pos, c.size)?;
        if !valid_qname(&qname) {
            self.failed = true;
        }
        if qname == "xmlns" {
            self.failed = true;
            return Ok(());
        }
        let (mut prefix, local) = match qname.split_once(':') {
            Some((p, l)) => (Some(p.to_string()), l.to_string()),
            None => (None, qname.clone()),
        };
        let mut uri = None;
        if let Some(a) = namespace {
            let u = self.avt(a, &c.node, c.pos, c.size)?;
            uri = (!u.is_empty()).then_some(u);
            if uri.as_deref() == Some("http://www.w3.org/2000/xmlns/") {
                self.failed = true;
                return Ok(());
            }
            if uri.as_deref() == Some(XML_NS) {
                prefix = Some("xml".into());
            } else if prefix.as_deref() == Some("xml") {
                prefix = None;
            }
        } else if let Some(p) = &prefix {
            match search_ns(&self.s.doc, inst, Some(p)) {
                Some(ns) => uri = ns.href.clone(),
                None => self.failed = true,
            }
        }
        let mut ns = None;
        if let Some(u) = &uri {
            let p = match prefix.as_deref() {
                None | Some("xmlns") => "ns_1",
                Some(p) => p,
            };
            ns = self.special_ns(Some(u), Some(p), target);
            if ns.is_none() {
                self.failed = true;
                return Ok(());
            }
        }
        let value = self.template_string(body, c)?;
        let noenc = matches!(body, [Instr::Text(_, true)]);
        self.set_attr(target, ns, &local, &value, noenc);
        Ok(())
    }
}

/// `xmlValidateQName`, near enough.
pub(super) fn valid_qname(s: &str) -> bool {
    match s.split_once(':') {
        Some((p, l)) => super::top::valid_ncname(p) && super::top::valid_ncname(l),
        None => super::top::valid_ncname(s),
    }
}
