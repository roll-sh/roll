//! The XSLT functions (libxslt functions.c, extra.c) as the XPath evaluator's host.

use super::exec::Exec;
use super::style::*;
use crate::types::xml::tree::{EntityType, NodeId};
use crate::types::xml::xpath::{self, Host, HostCall, XNode, XValue, err};
use std::collections::HashMap;

const NODE_SET_NS: [&str; 3] = ["http://xmlsoft.org/XSLT/namespace", "http://icl.com/saxon", "http://www.jclark.com/xt"];

pub(super) fn xslt_function(name: &str, uri: Option<&str>) -> bool {
    match uri {
        None => matches!(
            name,
            "current"
                | "document"
                | "key"
                | "unparsed-entity-uri"
                | "format-number"
                | "generate-id"
                | "system-property"
                | "element-available"
                | "function-available"
        ),
        Some(u) => name == "node-set" && NODE_SET_NS.contains(&u),
    }
}

const XPATH_FUNCTIONS: [&str; 27] = [
    "boolean", "ceiling", "count", "concat", "contains", "id", "false", "floor", "last", "lang",
    "local-name", "not", "name", "namespace-uri", "normalize-space", "number", "position",
    "round", "string", "string-length", "starts-with", "substring", "substring-before",
    "substring-after", "sum", "true", "translate",
];

/// The XSLT elements libxslt has (xsltElementAvailableFunction looks them up).
const XSLT_ELEMENTS: [&str; 22] = [
    "apply-imports", "apply-templates", "attribute", "call-template", "choose", "comment",
    "copy", "copy-of", "document", "element", "fallback", "for-each", "if", "message", "number",
    "param", "processing-instruction", "text", "value-of", "variable", "when", "otherwise",
];

impl Host for Exec<'_, '_> {
    fn variable(&self, name: &str, uri: Option<&str>) -> Option<XValue> {
        self.lookup(name, uri)
    }

    fn has_function(&self, name: &str, uri: Option<&str>) -> bool {
        xslt_function(name, uri)
    }

    fn call(&self, c: &HostCall, name: &str, uri: Option<&str>, mut args: Vec<XValue>) -> Result<XValue, i32> {
        if uri.is_some() {
            // node-set(): a result tree fragment as a node set.
            if args.len() != 1 {
                return Err(err::INVALID_ARITY);
            }
            return match args.pop() {
                Some(XValue::Tree(s)) | Some(XValue::NodeSet(s)) => Ok(XValue::NodeSet(s)),
                Some(_) => {
                    self.host_failed.set(true);
                    Ok(XValue::NodeSet(Vec::new()))
                }
                None => Err(err::STACK),
            };
        }
        let nargs = args.len();
        match name {
            "current" => {
                if nargs != 0 {
                    return Err(err::INVALID_ARITY);
                }
                Ok(XValue::NodeSet(vec![self.current.clone()]))
            }
            "generate-id" => self.generate_id(c, args),
            "system-property" => {
                if nargs != 1 {
                    return Err(err::INVALID_ARITY);
                }
                let mut v = args.pop().unwrap();
                let s = xpath::cast_to_string(&self.x, &mut v);
                let (uri, local) = self.expand(c, &s);
                Ok(XValue::String(
                    match (uri.as_deref(), local.as_str()) {
                        (Some(XSLT_NS), "version") => "1.0",
                        (Some(XSLT_NS), "vendor") => "libxslt",
                        (Some(XSLT_NS), "vendor-url") => "http://xmlsoft.org/XSLT/",
                        _ => "",
                    }
                    .to_string(),
                ))
            }
            "element-available" | "function-available" => {
                if nargs != 1 {
                    return Err(err::INVALID_ARITY);
                }
                let mut v = args.pop().unwrap();
                if !matches!(v, XValue::String(_)) {
                    return Err(err::INVALID_TYPE);
                }
                let s = xpath::cast_to_string(&self.x, &mut v);
                let (uri, local) = self.expand(c, &s);
                Ok(XValue::Bool(if name == "element-available" {
                    match uri.as_deref() {
                        Some(XSLT_NS) => XSLT_ELEMENTS.contains(&local.as_str()),
                        Some("http://icl.com/saxon") => local == "output",
                        Some("org.apache.xalan.xslt.extensions.Redirect") => local == "write",
                        Some("http://www.jclark.com/xt") => local == "document",
                        Some("http://xmlsoft.org/XSLT/namespace") => local == "debug",
                        _ => false,
                    }
                } else {
                    match uri.as_deref() {
                        None => XPATH_FUNCTIONS.contains(&local.as_str()) || xslt_function(&local, None),
                        Some(u) => xslt_function(&local, Some(u)) || (u == xpath::XQUERY_NS && local == "escape-uri"),
                    }
                }))
            }
            "unparsed-entity-uri" => {
                if nargs != 1 {
                    return Err(err::INVALID_ARITY);
                }
                let mut v = args.pop().unwrap();
                let s = xpath::cast_to_string(&self.x, &mut v);
                let d = &self.x.doc;
                let uri = d
                    .entity(&s)
                    .map(|e| &d.entities[e])
                    .filter(|e| e.etype == EntityType::ExternalGeneralUnparsed)
                    .and_then(|e| e.system_id.clone());
                Ok(XValue::String(uri.unwrap_or_default()))
            }
            "document" => {
                if !(1..=2).contains(&nargs) {
                    return Err(err::INVALID_ARITY);
                }
                // Every load is a file or network read, which xml2's security preferences
                // forbid: an empty node set, and the transformation fails.
                let empty = matches!(&args[0], XValue::NodeSet(s) | XValue::Tree(s) if s.is_empty());
                if !empty {
                    self.host_failed.set(true);
                }
                Ok(XValue::NodeSet(Vec::new()))
            }
            "key" => self.key_function(c, args),
            "format-number" => self.format_number(c, args),
            _ => Err(err::UNKNOWN_FUNC),
        }
    }
}

impl Exec<'_, '_> {
    /// A QName argument resolved against the expression's namespaces.
    fn expand(&self, c: &HostCall, s: &str) -> (Option<String>, String) {
        match s.split_once(':') {
            Some((p, l)) => {
                let uri = c.ns.lookup(p).map(str::to_string);
                if uri.is_none() {
                    self.host_failed.set(true);
                }
                (uri, l.to_string())
            }
            None => (None, s.to_string()),
        }
    }

    fn generate_id(&self, c: &HostCall, mut args: Vec<XValue>) -> Result<XValue, i32> {
        let cur = match args.len() {
            0 => c.node.clone(),
            1 => match args.pop().unwrap() {
                XValue::NodeSet(s) => {
                    if s.is_empty() {
                        return Ok(XValue::String(String::new()));
                    }
                    let mut s = s;
                    self.x.sort_nodes(&mut s);
                    s[0].clone()
                }
                _ => return Err(err::INVALID_TYPE),
            },
            _ => return Err(err::INVALID_ARITY),
        };
        let (id, prefix) = match &cur {
            XNode::Node(id) => (*id, None),
            XNode::Ns(ns) => match ns.parent {
                Some(p) => (p, Some(ns.prefix.clone().unwrap_or_default())),
                None => return Err(err::INVALID_TYPE),
            },
        };
        let mut ids = self.ids.borrow_mut();
        let next = ids.len() as u64 + 1;
        let n = *ids.entry(id).or_insert(next);
        let mut s = format!("id{n}");
        if let Some(p) = prefix {
            s.push_str("ns");
            for b in p.bytes() {
                s.push_str(&format!("{b:02X}"));
            }
        }
        Ok(XValue::String(s))
    }

    fn format_number(&self, c: &HostCall, mut args: Vec<XValue>) -> Result<XValue, i32> {
        if !(2..=3).contains(&args.len()) {
            return Err(err::INVALID_ARITY);
        }
        let name = if args.len() == 3 {
            let mut v = args.pop().unwrap();
            Some(self.expand(c, &xpath::cast_to_string(&self.x, &mut v)))
        } else {
            None
        };
        let mut p = args.pop().unwrap();
        let pattern = xpath::cast_to_string(&self.x, &mut p);
        let mut n = args.pop().unwrap();
        let number = xpath::cast_to_number(&self.x, &mut n);
        let default = super::number::DecimalFormat::default();
        let df = match self.s.decimal_formats.iter().find(|d| d.0 == name) {
            Some(d) => &d.1,
            None if name.is_some() => return Err(err::STACK),
            None => &default,
        };
        Ok(XValue::String(super::number::format_number(df, &pattern, number)))
    }

    fn key_function(&self, c: &HostCall, mut args: Vec<XValue>) -> Result<XValue, i32> {
        if args.len() != 2 {
            return Err(err::INVALID_ARITY);
        }
        let value = args.pop().unwrap();
        let mut n = args.pop().unwrap();
        let name = xpath::cast_to_string(&self.x, &mut n);
        let name = self.expand(c, &name);
        let root = self.root_doc(c.node);
        let values: Vec<String> = match &value {
            XValue::NodeSet(s) | XValue::Tree(s) => s.iter().map(|n| self.x.node_string(n)).collect(),
            v => {
                let mut v = v.clone();
                vec![xpath::cast_to_string(&self.x, &mut v)]
            }
        };
        let mut out: Vec<XNode> = Vec::new();
        for v in values {
            let found = self.key_lookup(&name, root, &v).map_err(|_| err::STACK)?;
            for n in found {
                if !out.iter().any(|o| same(o, &n)) {
                    out.push(n);
                }
            }
        }
        self.x.sort_nodes(&mut out);
        Ok(XValue::NodeSet(out))
    }

    pub(super) fn root_doc(&self, n: &XNode) -> NodeId {
        match n {
            XNode::Node(id) => self.x.doc_of(*id),
            XNode::Ns(ns) => ns.parent.map_or(0, |p| self.x.doc_of(p)),
        }
    }

    /// The nodes of key `name` with `value` in the document `root`, its table built on first
    /// use (xsltInitCtxtKey): every node of the document matching a definition's pattern,
    /// under each string its use expression gives.
    pub(super) fn key_lookup(&self, name: &QName, root: NodeId, value: &str) -> Result<Vec<XNode>, ()> {
        let k = (name.clone(), root);
        if let Some(t) = self.keys.borrow().get(&k) {
            return Ok(t.get(value).cloned().unwrap_or_default());
        }
        let mut table: HashMap<String, Vec<XNode>> = HashMap::new();
        for def in self.s.keys.iter().filter(|d| &d.name == name) {
            let mut stack = vec![root];
            let mut order = Vec::new();
            while let Some(id) = stack.pop() {
                order.push(id);
                let n = &self.x.doc.nodes[id];
                for &c in n.children.iter().rev() {
                    stack.push(c);
                }
                for &a in n.attrs.iter().rev() {
                    stack.push(a);
                }
            }
            for id in order {
                let node = XNode::Node(id);
                if !def.pattern.matches(&self.x, &node, self)? {
                    continue;
                }
                let v = self.eval_raw(&def.use_.comp, &def.use_.ns, &node, 1, 1).map_err(|_| ())?;
                let strings: Vec<String> = match &v {
                    XValue::NodeSet(s) | XValue::Tree(s) => s.iter().map(|n| self.x.node_string(n)).collect(),
                    v => {
                        let mut v = v.clone();
                        vec![xpath::cast_to_string(&self.x, &mut v)]
                    }
                };
                for s in strings {
                    let e = table.entry(s).or_default();
                    if !e.iter().any(|o| same(o, &node)) {
                        e.push(node.clone());
                    }
                }
            }
        }
        let r = table.get(value).cloned().unwrap_or_default();
        self.keys.borrow_mut().insert(k, table);
        Ok(r)
    }
}

fn same(a: &XNode, b: &XNode) -> bool {
    matches!((a, b), (XNode::Node(x), XNode::Node(y)) if x == y)
}

impl super::pattern::Env for Exec<'_, '_> {
    fn predicate(&self, e: &Expr, node: &XNode, pos: i32, size: i32) -> Result<bool, ()> {
        let v = self.eval_raw(&e.comp, &e.ns, node, pos, size).map_err(|_| ())?;
        Ok(match &v {
            XValue::Number(n) => *n == pos as f64,
            XValue::NodeSet(s) | XValue::Tree(s) => !s.is_empty(),
            XValue::Bool(b) => *b,
            XValue::String(s) => !s.is_empty(),
        })
    }

    fn id_nodes(&self, node: &XNode, ids: &str) -> Vec<XNode> {
        let _ = node;
        let comp = match xpath::compile(&format!("id('{}')", ids.replace('\'', ""))) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        match self.eval_raw(&comp, &xpath::NsMap::new(), &XNode::Node(0), 1, 1) {
            Ok(XValue::NodeSet(s)) => s,
            _ => Vec::new(),
        }
    }

    fn key_nodes(&self, node: &XNode, name: &QName, value: &str) -> Result<Vec<XNode>, ()> {
        self.key_lookup(name, self.root_doc(node), value)
    }
}
