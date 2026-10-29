//! Compiling a stylesheet document (xsltParseStylesheetDoc): preprocessing, the top-level
//! elements, and template bodies into instructions. Errors count as libxslt's
//! `style->errors`; any makes the stylesheet fail ("failed to parse stylesheet").

use super::Diag;
use super::style::*;
use crate::types::xml::tree::{Doc, Kind, NodeId, Ns};
use crate::types::xml::xpath::{self, NsMap};
use std::collections::HashMap;
use std::rc::Rc;

pub(super) struct Compiler<'d> {
    pub doc: Doc,
    pub diag: &'d mut Diag,
    pub errors: usize,
    pub forwards: bool,
    /// Excluded namespace URIs (exclude-result-prefixes) and extension ones.
    pub excluded: Vec<String>,
    extension: Vec<String>,
    ns_cache: HashMap<NodeId, Rc<NsMap>>,
}

/// `xmlGetNsList`: the declarations in scope at `id`, innermost first, one per prefix.
pub(super) fn ns_list(doc: &Doc, id: NodeId) -> Vec<Ns> {
    let mut out: Vec<Ns> = Vec::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        let n = &doc.nodes[c];
        if n.kind == Kind::Element {
            for d in &n.ns_defs {
                if !out.iter().any(|o| o.prefix == d.prefix) {
                    out.push(d.clone());
                }
            }
        }
        cur = n.parent;
    }
    out
}

/// `xmlSearchNs(doc, node, prefix)`.
pub(super) fn search_ns(doc: &Doc, id: NodeId, prefix: Option<&str>) -> Option<Ns> {
    if prefix == Some("xml") {
        return Some(Ns { prefix: Some("xml".into()), href: Some(XML_NS.into()) });
    }
    let mut cur = Some(id);
    while let Some(c) = cur {
        let n = &doc.nodes[c];
        if n.kind == Kind::Element
            && let Some(d) = n.ns_defs.iter().find(|d| d.prefix.as_deref() == prefix)
        {
            return Some(d.clone());
        }
        cur = n.parent;
    }
    None
}

/// An attribute without a namespace (`xmlGetNsProp(node, name, NULL)`).
pub(super) fn attr(doc: &Doc, id: NodeId, name: &str) -> Option<String> {
    doc.nodes[id]
        .attrs
        .iter()
        .find(|&&a| doc.nodes[a].name == name && doc.nodes[a].ns.is_none())
        .map(|&a| doc.content(a))
}

pub(super) fn is_xslt(doc: &Doc, id: NodeId) -> bool {
    let n = &doc.nodes[id];
    n.kind == Kind::Element && n.ns.as_ref().and_then(|ns| ns.href.as_deref()) == Some(XSLT_NS)
}

/// `IS_BLANK_NODE`: a text node of blanks only.
pub(super) fn is_blank_text(doc: &Doc, id: NodeId) -> bool {
    let n = &doc.nodes[id];
    n.kind == Kind::Text && n.content.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

fn detach(doc: &mut Doc, id: NodeId) {
    if let Some(p) = doc.nodes[id].parent.take() {
        doc.nodes[p].children.retain(|&c| c != id);
    }
}

/// `xmlNodeGetSpacePreserve`: 1 under xml:space="preserve", 0 under "default", -1 otherwise.
fn space_preserve(doc: &Doc, id: Option<NodeId>) -> i32 {
    let mut cur = id;
    while let Some(c) = cur {
        let n = &doc.nodes[c];
        if n.kind == Kind::Element {
            for &a in &n.attrs {
                let an = &doc.nodes[a];
                if an.name == "space" && an.ns.as_ref().and_then(|ns| ns.href.as_deref()) == Some(XML_NS) {
                    match doc.content(a).as_str() {
                        "preserve" => return 1,
                        "default" => return 0,
                        _ => {}
                    }
                }
            }
        }
        cur = n.parent;
    }
    -1
}

impl<'d> Compiler<'d> {
    pub fn new(doc: Doc, diag: &'d mut Diag) -> Compiler<'d> {
        Compiler {
            doc,
            diag,
            errors: 0,
            forwards: false,
            excluded: Vec::new(),
            extension: Vec::new(),
            ns_cache: HashMap::new(),
        }
    }

    fn error(&mut self) {
        self.errors += 1;
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<String> {
        attr(&self.doc, id, name)
    }

    /// The namespaces an XPath expression at `id` sees.
    fn nsmap(&mut self, id: NodeId) -> Rc<NsMap> {
        if let Some(m) = self.ns_cache.get(&id) {
            return m.clone();
        }
        let mut m = NsMap::new();
        for ns in ns_list(&self.doc, id).iter().rev() {
            if let (Some(p), Some(h)) = (&ns.prefix, &ns.href) {
                m.register(p, h);
            }
        }
        let m = Rc::new(m);
        self.ns_cache.insert(id, m.clone());
        m
    }

    /// `xsltXPathCompile`: a compile error goes to the detail and counts.
    pub fn xpath(&mut self, id: NodeId, s: &str) -> Option<Expr> {
        self.xpath_flags(id, s, false)
    }

    /// For patterns and xsl:key's use (`XML_XPATH_NOVAR`).
    pub fn xpath_novar(&mut self, id: NodeId, s: &str) -> Option<Expr> {
        self.xpath_flags(id, s, true)
    }

    fn xpath_flags(&mut self, id: NodeId, s: &str, novar: bool) -> Option<Expr> {
        match xpath::compile_flags(s, novar) {
            Ok(c) => Some(Expr { comp: Rc::new(c), ns: self.nsmap(id) }),
            Err(e) => {
                self.diag.xpath_error(&e);
                self.error();
                None
            }
        }
    }

    /// `xsltGetQNameURI`: the expanded name of a QName attribute value; None (an error) for
    /// an unbound prefix.
    pub fn qname(&mut self, id: NodeId, s: &str) -> Option<QName> {
        match s.split_once(':') {
            Some((p, l)) if !p.is_empty() => match search_ns(&self.doc, id, Some(p)) {
                Some(ns) => Some((ns.href, l.to_string())),
                None => {
                    self.error();
                    None
                }
            },
            _ => Some((None, s.to_string())),
        }
    }

    /// `xsltParseStylesheetExcludePrefix`: pushes the excluded URIs, returning how many.
    pub fn exclude_prefixes(&mut self, id: NodeId, xslt_elem: bool) -> usize {
        let v = if xslt_elem {
            self.attr(id, "exclude-result-prefixes")
        } else {
            ns_attr(&self.doc, id, "exclude-result-prefixes")
        };
        let Some(v) = v else { return 0 };
        let mut n = 0;
        for p in v.split([' ', '\t', '\n', '\r']).filter(|p| !p.is_empty()) {
            let prefix = (p != "#default").then_some(p);
            if let Some(ns) = search_ns(&self.doc, id, prefix)
                && let Some(h) = ns.href
            {
                self.excluded.push(h);
                n += 1;
            }
        }
        n
    }

    /// `xsltParseStylesheetExtPrefix`.
    pub fn extension_prefixes(&mut self, id: NodeId, xslt_elem: bool) {
        let v = if xslt_elem {
            self.attr(id, "extension-element-prefixes")
        } else {
            ns_attr(&self.doc, id, "extension-element-prefixes")
        };
        let Some(v) = v else { return };
        for p in v.split([' ', '\t', '\n', '\r']).filter(|p| !p.is_empty()) {
            let prefix = (p != "#default").then_some(p);
            if let Some(ns) = search_ns(&self.doc, id, prefix)
                && let Some(h) = ns.href
            {
                self.extension.push(h);
            }
        }
    }

    pub fn is_extension(&self, uri: Option<&str>) -> bool {
        uri.is_some_and(|u| self.extension.iter().any(|e| e == u))
    }

    /// `xsltPreprocessStylesheet`: drops blank text (outside xml:space="preserve"),
    /// comments and processing instructions, leaves xsl:text and top-level data elements
    /// alone, and moves namespace declarations of excluded URIs to the root element.
    pub fn preprocess_node(&mut self, id: NodeId, root: NodeId, style_elem: bool) {
        let mut pushed = 0;
        if is_xslt(&self.doc, id) {
            if self.doc.nodes[id].name == "text" {
                return;
            }
        } else {
            pushed = self.exclude_prefixes(id, false);
        }
        if id != root && !self.excluded.is_empty() && !self.doc.nodes[id].ns_defs.is_empty() {
            let defs = std::mem::take(&mut self.doc.nodes[id].ns_defs);
            let (moved, kept): (Vec<Ns>, Vec<Ns>) = defs.into_iter().partition(|ns| {
                ns.prefix.is_some() && self.excluded.iter().any(|e| Some(e) == ns.href.as_ref())
            });
            self.doc.nodes[id].ns_defs = kept;
            for ns in moved.into_iter().rev() {
                self.doc.nodes[root].ns_defs.insert(0, ns);
            }
        }
        let skip = style_elem && self.doc.nodes[id].parent == Some(root) && {
            let uri = self.doc.nodes[id].ns.as_ref().and_then(|ns| ns.href.clone());
            uri.is_some() && uri.as_deref() != Some(XSLT_NS) && !self.is_extension(uri.as_deref())
        };
        if !skip {
            let kids = self.doc.nodes[id].children.clone();
            for c in kids {
                match self.doc.nodes[c].kind {
                    Kind::Element => self.preprocess_node(c, root, style_elem),
                    Kind::Text => {
                        if is_blank_text(&self.doc, c) && space_preserve(&self.doc, Some(id)) != 1 {
                            detach(&mut self.doc, c);
                        }
                    }
                    Kind::CData => {}
                    _ => detach(&mut self.doc, c),
                }
            }
        }
        self.excluded.truncate(self.excluded.len() - pushed);
    }
}

/// An attribute in the XSLT namespace (`xmlGetNsProp(node, name, XSLT_NAMESPACE)`).
pub(super) fn ns_attr(doc: &Doc, id: NodeId, name: &str) -> Option<String> {
    doc.nodes[id]
        .attrs
        .iter()
        .find(|&&a| {
            let n = &doc.nodes[a];
            n.name == name && n.ns.as_ref().and_then(|ns| ns.href.as_deref()) == Some(XSLT_NS)
        })
        .map(|&a| doc.content(a))
}
