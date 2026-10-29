//! Applying templates (xsltProcessOneNode, xsltDefaultProcessOneNode, xsltGetTemplate,
//! xsltApplyXSLTTemplate) and the instructions that select nodes.

use super::exec::{Exec, R, Stop};
use super::style::*;
use crate::types::xml::tree::Kind;
use crate::types::xml::xpath::{self, XNode, XValue};
use std::rc::Rc;

/// The context an instruction runs in: the node and its position in the current node list.
#[derive(Clone)]
pub(super) struct Ctx {
    pub node: XNode,
    pub pos: i32,
    pub size: i32,
}

pub(super) type Params = Vec<(QName, XValue)>;

impl Exec<'_, '_> {
    fn kind_of(&self, n: &XNode) -> Option<Kind> {
        match n {
            XNode::Node(id) => Some(self.x.doc.nodes[*id].kind),
            XNode::Ns(_) => None,
        }
    }

    /// `xsltGetTemplate`: the template rule for `node` in the current mode: the highest
    /// priority, then the last in the stylesheet.
    pub fn find_template(&self, node: &XNode) -> R<Option<usize>> {
        if matches!(node, XNode::Ns(_)) {
            return Ok(None);
        }
        let mut best: Option<(f32, usize)> = None;
        for (i, t) in self.s.templates.iter().enumerate() {
            let Some(p) = &t.pattern else { continue };
            if t.mode != self.mode {
                continue;
            }
            for a in &p.alts {
                let pr = t.priority.unwrap_or(a.priority);
                if let Some((bp, bi)) = best
                    && (pr < bp || (pr == bp && i < bi))
                {
                    continue;
                }
                if a.matches(&self.x, node, self).map_err(|_| Stop::Stopped)? {
                    best = Some((pr, i));
                }
            }
        }
        Ok(best.map(|b| b.1))
    }

    /// `xsltProcessOneNode`.
    pub fn process_node(&mut self, c: &Ctx, params: &Params) -> R<()> {
        match self.find_template(&c.node)? {
            Some(t) => self.apply_template(t, c, params),
            None => self.builtin(c, params),
        }
    }

    /// `xsltDefaultProcessOneNode`: the built-in rules.
    pub fn builtin(&mut self, c: &Ctx, params: &Params) -> R<()> {
        let XNode::Node(id) = c.node else { return Ok(()) };
        match self.x.doc.nodes[id].kind {
            Kind::Document | Kind::Element => {}
            Kind::Text | Kind::CData => {
                self.copy_text(id);
                return Ok(());
            }
            Kind::Attribute => {
                let t = self.x.doc.nodes[id].children.iter().copied().find(|&t| self.x.doc.nodes[t].kind == Kind::Text);
                if let Some(t) = t {
                    self.copy_text(t);
                } else {
                    self.failed = true;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
        let kids = self.x.doc.nodes[id].children.clone();
        let real = |k: Kind| matches!(k, Kind::Element | Kind::Text | Kind::CData | Kind::Comment | Kind::Pi);
        let size = kids.iter().filter(|&&k| real(self.x.doc.nodes[k].kind)).count() as i32;
        for (i, &k) in kids.iter().enumerate() {
            let child = Ctx { node: XNode::Node(k), pos: i as i32 + 1, size };
            match self.x.doc.nodes[k].kind {
                Kind::Element => {
                    self.enter()?;
                    let r = self.process_node(&child, params);
                    self.depth -= 1;
                    r?;
                }
                Kind::CData => match self.find_template(&child.node)? {
                    // (The position stays the parent's here, as in libxslt.)
                    Some(t) => self.apply_template(t, &Ctx { node: child.node, ..c.clone() }, params)?,
                    None => self.copy_text(k),
                },
                Kind::Text => match self.find_template(&child.node)? {
                    Some(t) => self.apply_template(t, &child, params)?,
                    None => self.copy_text(k),
                },
                Kind::Comment | Kind::Pi => {
                    if let Some(t) = self.find_template(&child.node)? {
                        self.apply_template(t, &child, params)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `xsltApplyXSLTTemplate`: a new variable frame, the parameters (passed, else their
    /// defaults), then the body.
    pub fn apply_template(&mut self, t: usize, c: &Ctx, params: &Params) -> R<()> {
        crate::stack::grow(|| self.apply_template_inner(t, c, params))
    }

    fn apply_template_inner(&mut self, t: usize, c: &Ctx, params: &Params) -> R<()> {
        let tmpl = &self.s.templates[t];
        let (old_base, old_len) = (self.base, self.vars.len());
        let old_rule = self.rule;
        let old_current = std::mem::replace(&mut self.current, c.node.clone());
        if tmpl.pattern.is_some() {
            self.rule = Some(t);
        }
        self.base = self.vars.len();
        let mut r = Ok(());
        for p in &tmpl.params {
            if self.vars[self.base..].iter().any(|(q, _)| *q == p.name) {
                self.failed = true;
            }
            let v = match params.iter().rev().find(|(q, _)| *q == p.name) {
                Some((_, v)) => v.clone(),
                None => match self.var_value(p, c) {
                    Ok(v) => v,
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                },
            };
            self.vars.push((p.name.clone(), v));
        }
        if r.is_ok() {
            let body = tmpl.body.clone();
            let old_templ = self.templ.replace(t);
            r = self.run_top(&body, c);
            self.templ = old_templ;
        }
        self.vars.truncate(old_len);
        self.base = old_base;
        self.rule = old_rule;
        self.current = old_current;
        r
    }

    /// A variable's value (`xsltEvalVariable`): its select, else its content as a result
    /// tree fragment, else the empty string.
    pub fn var_value(&mut self, d: &VarDecl, c: &Ctx) -> R<XValue> {
        if let Some(e) = &d.select {
            return self.eval(e, &c.node, c.pos, c.size);
        }
        if d.body.is_empty() {
            return Ok(XValue::String(String::new()));
        }
        let body: Rc<Vec<Instr>> = d.body.clone();
        self.fragment(&body, c)
    }

    /// with-param values, evaluated in the caller's context.
    pub fn with_params(&mut self, ps: &[VarDecl], c: &Ctx) -> R<Params> {
        let mut out: Params = Vec::new();
        for p in ps {
            let v = self.var_value(p, c)?;
            out.push((p.name.clone(), v));
        }
        Ok(out)
    }

    /// The nodes an expression selects (apply-templates / for-each), sorted as asked.
    fn node_list(&mut self, select: &Expr, sorts: &[Sort], c: &Ctx) -> R<Option<Vec<XNode>>> {
        let v = self.eval(select, &c.node, c.pos, c.size)?;
        let XValue::NodeSet(nodes) = v else {
            self.failed = true;
            return Ok(None);
        };
        if sorts.is_empty() || nodes.len() < 2 {
            return Ok(Some(nodes));
        }
        self.sort(nodes, sorts, c).map(Some)
    }

    pub fn apply_templates(&mut self, select: Option<&Expr>, mode: &Option<QName>, sorts: &[Sort], ps: &[VarDecl], c: &Ctx) -> R<()> {
        let params = self.with_params(ps, c)?;
        let nodes = match select {
            Some(e) => match self.node_list(e, sorts, c)? {
                Some(n) => n,
                None => return Ok(()),
            },
            None => {
                let XNode::Node(id) = c.node else { return Ok(()) };
                let kids: Vec<XNode> = self.x.doc.nodes[id].children.iter().map(|&k| XNode::Node(k)).collect();
                if sorts.is_empty() || kids.len() < 2 { kids } else { self.sort(kids, sorts, c)? }
            }
        };
        let old_mode = std::mem::replace(&mut self.mode, mode.clone());
        let size = nodes.len() as i32;
        let mut r = Ok(());
        for (i, n) in nodes.into_iter().enumerate() {
            let k = self.kind_of(&n);
            if matches!(k, Some(Kind::Dtd | Kind::EntityDecl | Kind::ElementDecl | Kind::AttributeDecl)) {
                continue;
            }
            r = self.process_node(&Ctx { node: n, pos: i as i32 + 1, size }, &params);
            if r.is_err() {
                break;
            }
        }
        self.mode = old_mode;
        r
    }

    pub fn for_each(&mut self, select: &Expr, sorts: &[Sort], body: &[Instr], c: &Ctx) -> R<()> {
        let Some(nodes) = self.node_list(select, sorts, c)? else {
            return Err(Stop::Stopped);
        };
        let size = nodes.len() as i32;
        let old_current = self.current.clone();
        let mut r = Ok(());
        for (i, n) in nodes.into_iter().enumerate() {
            self.current = n.clone();
            r = self.run(body, &Ctx { node: n, pos: i as i32 + 1, size });
            if r.is_err() {
                break;
            }
        }
        self.current = old_current;
        r
    }

    pub fn call_template(&mut self, name: &QName, ps: &[VarDecl], c: &Ctx) -> R<()> {
        let params = self.with_params(ps, c)?;
        match self.s.named.get(name) {
            Some(&t) => self.apply_template(t, c, &params),
            None => {
                self.failed = true;
                Ok(())
            }
        }
    }

    /// `xsltDefaultSortFunction`: a stable sort on the keys, text by bytes, numbers with NaN
    /// first.
    fn sort(&mut self, nodes: Vec<XNode>, sorts: &[Sort], c: &Ctx) -> R<Vec<XNode>> {
        let len = nodes.len() as i32;
        let mut keys: Vec<Vec<(bool, f64, String)>> = vec![Vec::new(); nodes.len()];
        let mut desc = Vec::new();
        let mut numeric = Vec::new();
        for s in sorts {
            let num = match &s.data_type {
                Some(a) => self.avt(a, &c.node, c.pos, c.size)? == "number",
                None => false,
            };
            let d = match &s.order {
                Some(a) => self.avt(a, &c.node, c.pos, c.size)? == "descending",
                None => false,
            };
            numeric.push(num);
            desc.push(d);
            let old = self.current.clone();
            for (i, n) in nodes.iter().enumerate() {
                self.current = n.clone();
                let mut v = self.eval(&s.select, n, i as i32 + 1, len)?;
                let st = xpath::cast_to_string(&self.x, &mut v);
                let f = if num { xpath::string_to_number(&st) } else { 0.0 };
                keys[i].push((true, f, st));
            }
            self.current = old;
        }
        let mut idx: Vec<usize> = (0..nodes.len()).collect();
        idx.sort_by(|&a, &b| {
            for k in 0..sorts.len() {
                let (x, y) = (&keys[a][k], &keys[b][k]);
                let mut o = if numeric[k] {
                    match (x.1.is_nan(), y.1.is_nan()) {
                        (true, true) => std::cmp::Ordering::Equal,
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal),
                    }
                } else {
                    x.2.as_bytes().cmp(y.2.as_bytes())
                };
                if desc[k] {
                    o = o.reverse();
                }
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            a.cmp(&b)
        });
        Ok(idx.into_iter().map(|i| nodes[i].clone()).collect())
    }
}
