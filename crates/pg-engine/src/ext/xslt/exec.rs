//! The transformation (libxslt's transform.c): the state, XPath evaluation in it, and the
//! XSLT functions (functions.c).

use super::Diag;
use super::style::*;
use crate::types::xml::tree::{DOC, Doc, NodeId};
use crate::types::xml::xpath::{self, Env, NsMap, XDoc, XNode, XValue};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Why the transformation stops (`XSLT_STATE_STOPPED`), or that a global variable must be
/// evaluated before the one being evaluated can be.
pub(super) enum Stop {
    Stopped,
    NeedGlobal(usize),
}

pub(super) type R<T> = Result<T, Stop>;

/// A global variable's state (`computed`, xsltComputingGlobalVarMarker).
#[derive(Clone)]
pub(super) enum Global {
    Pending,
    Busy,
    Done(XValue),
}

/// libxslt's limits (xsltMaxDepth, xsltMaxVars).
const MAX_DEPTH: usize = 3000;
const MAX_VARS: usize = 15000;

pub(super) struct Exec<'s, 'd> {
    pub s: &'s Sheet,
    /// The source document, with the result tree fragments grafted in as they're made.
    pub x: XDoc,
    /// The tree being written: the result, or a fragment.
    pub out: Doc,
    pub insert: NodeId,
    pub diag: &'d mut Diag,
    /// `XSLT_STATE_ERROR`: the transformation goes on, but fails.
    pub failed: bool,
    pub globals: Vec<Global>,
    /// User parameters (xsltEvalUserParams), by name.
    pub user: Vec<(QName, XValue)>,
    pub vars: Vec<(QName, XValue)>,
    pub base: usize,
    /// `ctxt->node`: what current() returns.
    pub current: XNode,
    /// The template rule being applied (apply-imports).
    pub rule: Option<usize>,
    pub mode: Option<QName>,
    pub depth: usize,
    /// A global variable an evaluation needed before it was computed.
    pub(super) need: Cell<Option<usize>>,
    /// key() tables: per key name and document, value → nodes in document order.
    pub(super) keys: RefCell<HashMap<(QName, NodeId), HashMap<String, Vec<XNode>>>>,
    /// generate-id()'s numbers (`currentId`).
    pub(super) ids: RefCell<HashMap<NodeId, u64>>,
    /// The template whose body is being run at its top level, for the namespaces its
    /// literal result elements inherit.
    pub templ: Option<usize>,
    pub html: bool,
    /// A function reported an error (xsltTransformError): the transformation fails.
    pub host_failed: Cell<bool>,
}

impl<'s, 'd> Exec<'s, 'd> {
    pub fn new(s: &'s Sheet, x: XDoc, diag: &'d mut Diag) -> Exec<'s, 'd> {
        Exec {
            s,
            x,
            out: Doc::new(None),
            insert: DOC,
            diag,
            failed: false,
            globals: vec![Global::Pending; s.globals.len()],
            user: Vec::new(),
            vars: Vec::new(),
            base: 0,
            current: XNode::Node(DOC),
            rule: None,
            mode: None,
            depth: 0,
            need: Cell::new(None),
            keys: RefCell::new(HashMap::new()),
            ids: RefCell::new(HashMap::new()),
            templ: None,
            html: false,
            host_failed: Cell::new(false),
        }
    }

    /// The document node of `n`'s tree (`xpctxt->doc`).
    fn root_of(&self, n: &XNode) -> NodeId {
        match n {
            XNode::Node(id) => self.x.doc_of(*id),
            XNode::Ns(ns) => ns.parent.map_or(DOC, |p| self.x.doc_of(p)),
        }
    }

    /// Evaluates an expression; an XPath error goes to the detail and stops.
    pub fn eval(&mut self, e: &Expr, node: &XNode, pos: i32, size: i32) -> R<XValue> {
        let r = self.eval_raw(&e.comp, &e.ns, node, pos, size);
        self.checked(r)
    }

    pub fn eval_raw(&self, comp: &xpath::Compiled, ns: &NsMap, node: &XNode, pos: i32, size: i32) -> Result<XValue, i32> {
        let env = Env { ns, pos, size, root: self.root_of(node), host: self };
        xpath::eval_env(comp, &self.x, node, &env)
    }

    /// An evaluation's outcome: a needed global unwinds, an error is reported and stops.
    pub fn checked<T>(&mut self, r: Result<T, i32>) -> R<T> {
        if let Some(g) = self.need.take() {
            return Err(Stop::NeedGlobal(g));
        }
        r.map_err(|code| {
            self.diag.push(xpath::error_message(code));
            Stop::Stopped
        })
    }

    pub fn eval_string(&mut self, e: &Expr, node: &XNode, pos: i32, size: i32) -> R<String> {
        let mut v = self.eval(e, node, pos, size)?;
        Ok(xpath::cast_to_string(&self.x, &mut v))
    }

    pub fn eval_bool(&mut self, e: &Expr, node: &XNode, pos: i32, size: i32) -> R<bool> {
        let v = self.eval(e, node, pos, size)?;
        Ok(match &v {
            XValue::NodeSet(s) | XValue::Tree(s) => !s.is_empty(),
            XValue::Bool(b) => *b,
            XValue::Number(n) => *n != 0.0 && !n.is_nan(),
            XValue::String(s) => !s.is_empty(),
        })
    }

    /// An attribute value template's value (`xsltEvalAVT` / xsltAttrTemplateValueProcess).
    pub fn avt(&mut self, a: &Avt, node: &XNode, pos: i32, size: i32) -> R<String> {
        let mut s = String::new();
        for p in &a.0 {
            match p {
                AvtPart::Lit(l) => s.push_str(l),
                AvtPart::Fail => self.failed = true,
                AvtPart::Expr(e) => s.push_str(&self.eval_string(e, node, pos, size)?),
            }
        }
        Ok(s)
    }

    pub fn enter(&mut self) -> R<()> {
        if self.depth >= MAX_DEPTH || self.vars.len() >= MAX_VARS {
            self.failed = true;
            return Err(Stop::Stopped);
        }
        self.depth += 1;
        Ok(())
    }

    // --- variables

    pub(super) fn lookup(&self, name: &str, uri: Option<&str>) -> Option<XValue> {
        let is = |q: &QName| q.1 == name && q.0.as_deref() == uri;
        if let Some((_, v)) = self.vars[self.base..].iter().rev().find(|(q, _)| is(q)) {
            return Some(v.clone());
        }
        if let Some(i) = self.s.globals.iter().position(|g| is(&g.decl.name)) {
            let g = &self.s.globals[i];
            // A user parameter replaces an xsl:param, not an xsl:variable.
            if g.is_param
                && let Some((_, v)) = self.user.iter().find(|(q, _)| is(q))
            {
                return Some(v.clone());
            }
            return match &self.globals[i] {
                Global::Done(v) => Some(v.clone()),
                Global::Busy => None,
                Global::Pending => {
                    if self.need.get().is_none() {
                        self.need.set(Some(i));
                    }
                    None
                }
            };
        }
        self.user.iter().find(|(q, _)| is(q)).map(|(_, v)| v.clone())
    }
}
