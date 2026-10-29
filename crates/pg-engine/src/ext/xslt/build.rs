//! Writing the result tree: text merging (xsltCopyTextString, xsltCopyText), copies of
//! source nodes (xsltCopyTree, xsltShallowCopyElem, xsltShallowCopyAttr) and libxslt's
//! namespace fixup (namespaces.c).

use super::exec::Exec;
use super::style::*;
use crate::types::xml::tree::{Kind, Node, NodeId, Ns};
use crate::types::xml::xpath::{XNode, XValue};

const TEXT: &str = "text";
const NOENC: &str = "textnoenc";

impl Exec<'_, '_> {
    /// `xsltAddChild` of a new node to the insertion point.
    pub fn append(&mut self, id: NodeId) {
        let p = self.insert;
        self.out.append(p, id);
    }

    /// `xmlAddChild` of a new node: text merges into a last text child of the same name.
    pub fn add_node(&mut self, n: Node) {
        let p = self.insert;
        if n.kind == Kind::Text
            && let Some(&last) = self.out.nodes[p].children.last()
            && self.out.nodes[last].kind == Kind::Text
            && self.out.nodes[last].name == n.name
        {
            self.out.nodes[last].content.push_str(&n.content);
            return;
        }
        let id = self.out.add(n);
        self.out.append(p, id);
    }

    /// Whether text under the insertion point goes into CDATA sections
    /// (cdata-section-elements, XML output only).
    fn cdata_target(&self) -> bool {
        if self.html || self.s.output.cdata_elements.is_empty() {
            return false;
        }
        let t = &self.out.nodes[self.insert];
        if t.kind != Kind::Element {
            return false;
        }
        let uri = t.ns.as_ref().and_then(|n| n.href.clone());
        self.s.output.cdata_elements.iter().any(|(u, l)| *u == uri && *l == t.name)
    }

    fn last_child(&self) -> Option<NodeId> {
        self.out.nodes[self.insert].children.last().copied()
    }

    /// `xsltCopyTextString`.
    pub fn add_text(&mut self, s: &str, noesc: bool) {
        let last = self.last_child();
        let is = |e: &Self, k: Kind, name: Option<&str>| {
            last.is_some_and(|l| e.out.nodes[l].kind == k && name.is_none_or(|n| e.out.nodes[l].name == n))
        };
        if self.cdata_target() {
            if is(self, Kind::CData, None) {
                self.out.nodes[last.unwrap()].content.push_str(s);
                return;
            }
            let mut n = Node::new(Kind::CData, "");
            n.content = s.to_string();
            let id = self.out.add(n);
            self.append(id);
            return;
        }
        let name = if noesc { NOENC } else { TEXT };
        if is(self, Kind::Text, Some(name)) {
            self.out.nodes[last.unwrap()].content.push_str(s);
            return;
        }
        let mut n = Node::new(Kind::Text, name);
        n.content = s.to_string();
        let id = self.out.add(n);
        self.append(id);
    }

    /// `xsltCopyText` of a source text or CDATA node.
    pub fn copy_text(&mut self, src: NodeId) {
        let (content, name) = {
            let n = &self.x.doc.nodes[src];
            (n.content.clone(), if n.kind == Kind::Text { n.name.clone() } else { String::new() })
        };
        if self.cdata_target() {
            self.add_text(&content, false);
            return;
        }
        if let Some(l) = self.last_child() {
            let ln = &self.out.nodes[l];
            if (ln.kind == Kind::Text && ln.name == name) || (ln.kind == Kind::CData && name == NOENC) {
                self.out.nodes[l].content.push_str(&content);
                return;
            }
        }
        let mut n = Node::new(Kind::Text, if name == NOENC { NOENC } else { TEXT });
        n.content = content;
        self.add_node(n);
    }

    // --- namespaces in the result

    /// `xmlSearchNs` in the result tree.
    pub fn out_search_ns(&self, id: NodeId, prefix: Option<&str>) -> Option<Ns> {
        if prefix == Some("xml") {
            return Some(Ns { prefix: Some("xml".into()), href: Some(XML_NS.into()) });
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let n = &self.out.nodes[c];
            if n.kind == Kind::Element
                && let Some(d) = n.ns_defs.iter().find(|d| d.prefix.as_deref() == prefix)
            {
                return Some(d.clone());
            }
            cur = n.parent;
        }
        None
    }

    /// `xmlSearchNsByHref` in the result tree: a declaration of `href` whose prefix isn't
    /// shadowed at `id`.
    fn out_search_ns_by_href(&self, id: NodeId, href: &str) -> Option<Ns> {
        if href == XML_NS {
            return Some(Ns { prefix: Some("xml".into()), href: Some(XML_NS.into()) });
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            let n = &self.out.nodes[c];
            if n.kind == Kind::Element {
                for d in &n.ns_defs {
                    if d.href.as_deref() == Some(href)
                        && self.out_search_ns(id, d.prefix.as_deref()).is_some_and(|f| f.href.as_deref() == Some(href))
                    {
                        return Some(d.clone());
                    }
                }
            }
            cur = n.parent;
        }
        None
    }

    /// `xmlNewNs` on a result element: None if the prefix is declared there already.
    pub fn new_ns(&mut self, el: NodeId, href: Option<String>, prefix: Option<String>) -> Option<Ns> {
        if prefix.as_deref() == Some("xml") {
            return None;
        }
        let n = &mut self.out.nodes[el];
        if n.ns_defs.iter().any(|d| d.prefix == prefix) {
            return None;
        }
        let ns = Ns { prefix, href };
        n.ns_defs.push(ns.clone());
        Some(ns)
    }

    /// `xsltGetSpecialNamespace`: a binding of `uri` (preferably with `prefix`) in scope at
    /// `target`, declared there if need be.
    pub fn special_ns(&mut self, uri: Option<&str>, prefix: Option<&str>, target: NodeId) -> Option<Ns> {
        if self.out.nodes[target].kind != Kind::Element {
            return None;
        }
        if prefix.is_none() && uri.is_none_or(str::is_empty) {
            if let Some(d) = self.out.nodes[target].ns_defs.iter().find(|d| d.prefix.is_none()) {
                let _ = d;
                return None;
            }
            if let Some(p) = self.out.nodes[target].parent
                && self.out.nodes[p].kind == Kind::Element
            {
                self.out.nodes[p].ns.as_ref()?;
                let d = self.out_search_ns(p, None);
                if d.as_ref().is_none_or(|d| d.href.as_deref().is_none_or(str::is_empty)) {
                    return None;
                }
                self.new_ns(target, Some(String::new()), None);
            }
            return None;
        }
        if prefix == Some("xml") {
            return self.out_search_ns(target, Some("xml"));
        }
        let mut occupied = false;
        if let Some(d) = self.out.nodes[target].ns_defs.iter().find(|d| d.prefix.as_deref() == prefix) {
            if d.href.as_deref() == uri {
                return Some(d.clone());
            }
            occupied = true;
        }
        if occupied {
            if let Some(u) = uri
                && let Some(ns) = self.out_search_ns_by_href(target, u)
            {
                return Some(ns);
            }
            return self.declare_new_prefix(uri, prefix, target);
        }
        let parent = self.out.nodes[target].parent.filter(|&p| self.out.nodes[p].kind == Kind::Element);
        let Some(p) = parent else {
            return self.new_ns(target, uri.map(str::to_string), prefix.map(str::to_string));
        };
        if let Some(pns) = &self.out.nodes[p].ns
            && pns.prefix.is_some() == prefix.is_some()
            && pns.href.as_deref() == uri
            && (prefix.is_none() || pns.prefix.as_deref() == prefix)
        {
            return Some(pns.clone());
        }
        if let Some(ns) = self.out_search_ns(p, prefix) {
            if ns.href.as_deref() == uri {
                return Some(ns);
            }
            let attrs = self.out.nodes[target].attrs.clone();
            for a in attrs {
                if self.out.nodes[a].ns.as_ref().is_some_and(|n| n.prefix.as_deref() == prefix) {
                    if let Some(u) = uri
                        && let Some(ns) = self.out_search_ns_by_href(target, u)
                    {
                        return Some(ns);
                    }
                    return self.declare_new_prefix(uri, prefix, target);
                }
            }
        }
        self.new_ns(target, uri.map(str::to_string), prefix.map(str::to_string))
    }

    fn declare_new_prefix(&mut self, uri: Option<&str>, prefix: Option<&str>, target: NodeId) -> Option<Ns> {
        let base = prefix.unwrap_or("ns");
        for counter in 1..=1000 {
            let p = format!("{base}_{counter}");
            if self.out_search_ns(target, Some(&p)).is_none() {
                return self.new_ns(target, uri.map(str::to_string), Some(p));
            }
        }
        None
    }

    /// `xsltGetNamespace`: a stylesheet namespace (aliased) for a result element.
    pub fn get_namespace(&mut self, ns: &Ns, out: NodeId) -> Option<Ns> {
        let alias = ns.href.as_ref().and_then(|h| self.s.aliases.get(h)).cloned();
        match alias {
            Some(None) => self.special_ns(None, None, out),
            Some(Some(u)) => self.special_ns(Some(&u), ns.prefix.as_deref(), out),
            None => self.special_ns(ns.href.as_deref(), ns.prefix.as_deref(), out),
        }
    }

    /// `xsltCopyNamespaceList` (literal result elements: XSLT's namespace left out, aliases
    /// applied) or `xsltCopyNamespaceListInternal` (copies).
    pub fn copy_ns_list(&mut self, el: NodeId, defs: &[Ns], lre: bool) {
        for d in defs {
            if let Some(ens) = &self.out.nodes[el].ns
                && ens.prefix == d.prefix
                && ens.href == d.href
            {
                continue;
            }
            if self.out_search_ns(el, d.prefix.as_deref()).is_some_and(|f| f.href == d.href) {
                continue;
            }
            if lre {
                if d.href.as_deref() == Some(XSLT_NS) {
                    continue;
                }
                match d.href.as_ref().and_then(|h| self.s.aliases.get(h)).cloned() {
                    Some(None) => continue,
                    Some(Some(u)) => {
                        self.new_ns(el, Some(u), d.prefix.clone());
                    }
                    None => {
                        self.new_ns(el, d.href.clone(), d.prefix.clone());
                    }
                }
            } else {
                self.new_ns(el, d.href.clone(), d.prefix.clone());
            }
        }
    }

    /// `xsltShallowCopyNsNode`: a namespace node onto the insertion element.
    pub fn copy_ns_node(&mut self, ns: &Ns) {
        let el = self.insert;
        if self.out.nodes[el].kind != Kind::Element {
            return;
        }
        if !self.out.nodes[el].children.is_empty() {
            self.failed = true;
            return;
        }
        if ns.prefix.is_none() {
            if self.out.nodes[el].ns.is_none() {
                return;
            }
        } else if ns.prefix.as_deref() == Some("xml") {
            return;
        }
        if let Some(d) = self.out.nodes[el].ns_defs.iter().find(|d| d.prefix == ns.prefix) {
            let _ = d;
            return;
        }
        if self.out_search_ns(el, ns.prefix.as_deref()).is_some_and(|f| f.href == ns.href) {
            return;
        }
        self.new_ns(el, ns.href.clone(), ns.prefix.clone());
    }

    /// `xmlSetNsProp`: replaces an attribute of the same name and namespace.
    pub fn set_attr(&mut self, el: NodeId, ns: Option<Ns>, name: &str, value: &str, noenc: bool) {
        let href = ns.as_ref().and_then(|n| n.href.clone());
        let existing = self.out.nodes[el].attrs.iter().copied().find(|&a| {
            let an = &self.out.nodes[a];
            an.name == name && an.ns.as_ref().and_then(|n| n.href.clone()) == href
        });
        let mut t = Node::new(Kind::Text, if noenc { NOENC } else { TEXT });
        t.content = value.to_string();
        let tid = self.out.add(t);
        let a = match existing {
            Some(a) => {
                self.out.nodes[a].children.clear();
                self.out.nodes[a].ns = ns;
                a
            }
            None => {
                let mut n = Node::new(Kind::Attribute, name);
                n.ns = ns;
                n.parent = Some(el);
                let a = self.out.add(n);
                self.out.nodes[el].attrs.push(a);
                a
            }
        };
        self.out.nodes[tid].parent = Some(a);
        self.out.nodes[a].children.push(tid);
    }

    /// `xsltShallowCopyAttr`.
    pub fn copy_attr(&mut self, src: NodeId) {
        let target = self.insert;
        if self.out.nodes[target].kind != Kind::Element || !self.out.nodes[target].children.is_empty() {
            self.failed = true;
            return;
        }
        let value = self.x.doc.content(src);
        let (name, ns) = (self.x.doc.nodes[src].name.clone(), self.x.doc.nodes[src].ns.clone());
        let ns = match ns {
            Some(n) => {
                let r = self.special_ns(n.href.as_deref(), n.prefix.as_deref(), target);
                if r.is_none() {
                    self.failed = true;
                }
                r
            }
            None => None,
        };
        self.set_attr(target, ns, &name, &value, false);
    }

    /// `xsltShallowCopyElem` for xsl:copy (not a literal result element).
    pub fn shallow_copy_element(&mut self, src: NodeId) -> NodeId {
        let s = &self.x.doc.nodes[src];
        let (name, ns, defs) = (s.name.clone(), s.ns.clone(), s.ns_defs.clone());
        let el = self.out.add(Node::new(Kind::Element, name));
        self.append(el);
        if !defs.is_empty() {
            self.copy_ns_list(el, &defs, false);
        }
        match ns {
            Some(n) => self.out.nodes[el].ns = self.special_ns(n.href.as_deref(), n.prefix.as_deref(), el),
            None => {
                if self.parent_ns(el) {
                    self.special_ns(None, None, el);
                }
            }
        }
        el
    }

    fn parent_ns(&self, el: NodeId) -> bool {
        self.out.nodes[el].parent.is_some_and(|p| self.out.nodes[p].kind == Kind::Element && self.out.nodes[p].ns.is_some())
    }

    /// xsl:copy-of (xsltCopyOf).
    pub fn copy_of(&mut self, v: XValue) {
        match v {
            XValue::NodeSet(set) | XValue::Tree(set) => {
                for n in set {
                    match n {
                        XNode::Ns(ns) => {
                            let ns = Ns { prefix: ns.prefix.clone(), href: ns.href.clone() };
                            self.copy_ns_node(&ns);
                        }
                        XNode::Node(id) => match self.x.doc.nodes[id].kind {
                            Kind::Document => {
                                for c in self.x.doc.nodes[id].children.clone() {
                                    self.copy_tree(c, false);
                                }
                            }
                            Kind::Attribute => self.copy_attr(id),
                            _ => self.copy_tree(id, false),
                        },
                    }
                }
            }
            mut v => {
                let s = crate::types::xml::xpath::cast_to_string(&self.x, &mut v);
                if !s.is_empty() {
                    self.add_text(&s, false);
                }
            }
        }
    }

    /// `xsltCopyTree` (isLRE 0).
    fn copy_tree(&mut self, src: NodeId, top_visited: bool) {
        let kind = self.x.doc.nodes[src].kind;
        match kind {
            Kind::Text => {
                let n = &self.x.doc.nodes[src];
                let (c, noenc) = (n.content.clone(), n.name == NOENC);
                self.add_text(&c, noenc);
                return;
            }
            Kind::CData => {
                let c = self.x.doc.nodes[src].content.clone();
                self.add_text(&c, false);
                return;
            }
            Kind::Attribute => {
                self.copy_attr(src);
                return;
            }
            Kind::Comment | Kind::Pi => {
                let n = &self.x.doc.nodes[src];
                let mut m = Node::new(kind, n.name.clone());
                m.content = n.content.clone();
                self.add_node(m);
                return;
            }
            Kind::Document => {
                for c in self.x.doc.nodes[src].children.clone() {
                    self.copy_tree(c, top_visited);
                }
                return;
            }
            Kind::Element => {}
            _ => return,
        }
        let (name, ns, defs, parent) = {
            let s = &self.x.doc.nodes[src];
            (s.name.clone(), s.ns.clone(), s.ns_defs.clone(), self.x.parent(src))
        };
        let el = self.out.add(Node::new(Kind::Element, name));
        self.append(el);
        let parent_is_doc = parent.is_none_or(|p| self.x.doc.nodes[p].kind == Kind::Document);
        if !top_visited && !parent_is_doc {
            for d in super::compile::ns_list(&self.x.doc, src) {
                let found = self.out_search_ns(self.insert, d.prefix.as_deref()).filter(|f| f.href == d.href);
                let bound = match found {
                    Some(f) => Some(f),
                    None => self.new_ns(el, d.href.clone(), d.prefix.clone()),
                };
                if ns.as_ref().is_some_and(|n| n.prefix == d.prefix && n.href == d.href) {
                    self.out.nodes[el].ns = bound;
                }
            }
        } else if !defs.is_empty() {
            self.copy_ns_list(el, &defs, false);
        }
        match &ns {
            Some(n) => {
                if self.out.nodes[el].ns.is_none() {
                    self.out.nodes[el].ns = self.special_ns(n.href.as_deref(), n.prefix.as_deref(), el);
                }
            }
            None => {
                if self.parent_ns(el) {
                    self.special_ns(None, None, el);
                }
            }
        }
        let attrs = self.x.doc.nodes[src].attrs.clone();
        let old = std::mem::replace(&mut self.insert, el);
        for a in attrs {
            let an = &self.x.doc.nodes[a];
            let (aname, ans) = (an.name.clone(), an.ns.clone());
            let value = self.x.doc.content(a);
            let cns = match ans {
                Some(n) => self.special_ns(n.href.as_deref(), n.prefix.as_deref(), el),
                None => None,
            };
            let mut node = Node::new(Kind::Attribute, aname);
            node.ns = cns;
            node.parent = Some(el);
            let aid = self.out.add(node);
            let mut t = Node::new(Kind::Text, TEXT);
            t.content = value;
            t.parent = Some(aid);
            let tid = self.out.add(t);
            self.out.nodes[aid].children.push(tid);
            self.out.nodes[el].attrs.push(aid);
        }
        for c in self.x.doc.nodes[src].children.clone() {
            self.copy_tree(c, true);
        }
        self.insert = old;
    }
}
