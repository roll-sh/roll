//! XML for signed SAML documents: a namespace-aware tree and Exclusive XML Canonicalization 1.0
//! without comments (<https://www.w3.org/TR/xml-exc-c14n/>), which XML Signature
//! (<https://www.w3.org/TR/xmldsig-core1/>) needs to check an enveloped signature. No I/O and no
//! cryptography: `s3-server` checks digests and signature values over what this produces.
//!
//! The input is hostile by default (a SAML response comes from whoever calls STS):
//!
//! - no DTDs, so no entity definitions: only the predefined entities and character references;
//! - no processing instructions;
//! - size, depth and node-count limits;
//! - duplicate attributes (by expanded name) and duplicate `ID`s are errors, so a reference
//!   (`URI="#id"`) names exactly one element;
//! - an element's text is all of its text children together, so a comment can't cut a value
//!   short (`admin<!---->@example.com` is `admin@example.com`, as canonicalization sees it).

use quick_xml::events::{BytesStart, Event};
use std::collections::{BTreeMap, HashMap};

/// Largest document accepted (a SAML response is a few KB; IdP metadata can be bigger).
pub const MAX_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 20_000;
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Why a document was refused (never echoes the input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError(pub &'static str);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for XmlError {}

pub type NodeId = usize;

#[derive(Debug, Clone)]
pub struct Attr {
    /// `""` for an unprefixed attribute (no namespace).
    pub prefix: String,
    pub local: String,
    pub ns: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Element {
    pub prefix: String,
    pub local: String,
    /// The namespace URI (`""`: none).
    pub ns: String,
    pub attrs: Vec<Attr>,
    /// Namespace declarations on this element: (prefix, URI); prefix `""` is the default
    /// namespace (URI `""`: `xmlns=""`).
    pub decls: Vec<(String, String)>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

impl Element {
    /// An unprefixed attribute's value.
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.ns.is_empty() && a.local == local).map(|a| a.value.as_str())
    }

    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.ns == ns && self.local == local
    }

    fn qname(&self) -> String {
        if self.prefix.is_empty() { self.local.clone() } else { format!("{}:{}", self.prefix, self.local) }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Element(Element),
    Text(String),
}

/// A parsed document.
#[derive(Debug, Clone)]
pub struct Doc {
    nodes: Vec<Node>,
    root: NodeId,
    ids: HashMap<String, NodeId>,
}

fn bad(why: &'static str) -> XmlError {
    XmlError(why)
}

fn utf8(b: &[u8]) -> Result<&str, XmlError> {
    std::str::from_utf8(b).map_err(|_| bad("not UTF-8"))
}

/// XML 1.0 end-of-line handling: `\r\n` and lone `\r` become `\n`.
fn normalize_eol(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Attributes that are IDs (what `URI="#…"` references name): SAML's `ID`, XML Signature's `Id`.
fn is_id_attr(a: &Attr) -> bool {
    a.ns.is_empty() && matches!(a.local.as_str(), "ID" | "Id" | "id")
}

struct Scope {
    /// Frames of declarations, innermost last.
    frames: Vec<Vec<(String, String)>>,
}

impl Scope {
    fn lookup(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        for f in self.frames.iter().rev() {
            if let Some((_, uri)) = f.iter().find(|(p, _)| p == prefix) {
                return Some(uri.as_str());
            }
        }
        None
    }
}

fn split_qname(q: &str) -> Result<(String, String), XmlError> {
    match q.split_once(':') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() && !l.contains(':') => Ok((p.to_string(), l.to_string())),
        Some(_) => Err(bad("malformed qualified name")),
        None if q.is_empty() => Err(bad("empty name")),
        None => Ok((String::new(), q.to_string())),
    }
}

impl Doc {
    /// Parse a document (UTF-8, at most [`MAX_BYTES`]).
    pub fn parse(text: &str) -> Result<Doc, XmlError> {
        if text.len() > MAX_BYTES {
            return Err(bad("document too large"));
        }
        let mut reader = quick_xml::Reader::from_str(text);
        reader.config_mut().check_end_names = true;
        reader.config_mut().check_comments = true;
        let mut doc = Doc { nodes: Vec::new(), root: usize::MAX, ids: HashMap::new() };
        let mut stack: Vec<NodeId> = Vec::new();
        let mut scope = Scope { frames: Vec::new() };
        let mut done = false;
        loop {
            let ev = reader.read_event().map_err(|_| bad("not well-formed XML"))?;
            match ev {
                Event::Start(e) | Event::Empty(e) if done => {
                    let _ = e;
                    return Err(bad("more than one root element"));
                }
                Event::Start(e) => {
                    let id = doc.open(&e, &stack, &mut scope)?;
                    stack.push(id);
                    if stack.len() > MAX_DEPTH {
                        return Err(bad("too deeply nested"));
                    }
                }
                Event::Empty(e) => {
                    let id = doc.open(&e, &stack, &mut scope)?;
                    scope.frames.pop();
                    if stack.is_empty() {
                        doc.root = id;
                        done = true;
                    }
                }
                Event::End(_) => {
                    let id = stack.pop().ok_or_else(|| bad("unbalanced end tag"))?;
                    scope.frames.pop();
                    if stack.is_empty() {
                        doc.root = id;
                        done = true;
                    }
                }
                Event::Text(t) => {
                    let s = t.xml10_content().map_err(|_| bad("not UTF-8"))?;
                    doc.text(&stack, &s)?;
                }
                Event::CData(t) => {
                    let s = normalize_eol(&t.decode().map_err(|_| bad("not UTF-8"))?);
                    if stack.is_empty() {
                        return Err(bad("text outside the root element"));
                    }
                    doc.text(&stack, &s)?;
                }
                Event::GeneralRef(r) => {
                    let s = if r.is_char_ref() {
                        let c = r.resolve_char_ref().map_err(|_| bad("invalid character reference"))?.ok_or_else(|| bad("invalid character reference"))?;
                        c.to_string()
                    } else {
                        match utf8(&r)? {
                            "amp" => "&".into(),
                            "lt" => "<".into(),
                            "gt" => ">".into(),
                            "quot" => "\"".into(),
                            "apos" => "'".into(),
                            _ => return Err(bad("entities aren't allowed")),
                        }
                    };
                    if stack.is_empty() {
                        return Err(bad("text outside the root element"));
                    }
                    doc.text(&stack, &s)?;
                }
                Event::DocType(_) => return Err(bad("DTDs aren't allowed")),
                Event::PI(_) => return Err(bad("processing instructions aren't allowed")),
                Event::Decl(_) => {
                    if !doc.nodes.is_empty() {
                        return Err(bad("misplaced XML declaration"));
                    }
                }
                Event::Comment(_) => {}
                Event::Eof => break,
            }
        }
        if !stack.is_empty() || !done {
            return Err(bad("no complete root element"));
        }
        Ok(doc)
    }

    fn push(&mut self, n: Node) -> Result<NodeId, XmlError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(bad("too many nodes"));
        }
        self.nodes.push(n);
        Ok(self.nodes.len() - 1)
    }

    fn open(&mut self, e: &BytesStart, stack: &[NodeId], scope: &mut Scope) -> Result<NodeId, XmlError> {
        let qname = utf8(e.name().as_ref())?.to_string();
        let mut decls: Vec<(String, String)> = Vec::new();
        let mut raw: Vec<(String, String)> = Vec::new();
        for a in e.attributes().with_checks(true) {
            let a = a.map_err(|_| bad("malformed attribute"))?;
            let key = utf8(a.key.as_ref())?.to_string();
            let value = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map_err(|_| bad("malformed attribute value"))?.into_owned();
            if key == "xmlns" {
                if value == XML_NS || value == XMLNS_NS {
                    return Err(bad("reserved namespace"));
                }
                decls.push((String::new(), value));
            } else if let Some(p) = key.strip_prefix("xmlns:") {
                if p.is_empty() || p.contains(':') || p == "xmlns" || (p == "xml" && value != XML_NS) || (p != "xml" && (value == XML_NS || value == XMLNS_NS)) {
                    return Err(bad("reserved namespace prefix"));
                }
                if value.is_empty() {
                    return Err(bad("a prefix can't be undeclared in XML 1.0"));
                }
                if p != "xml" {
                    decls.push((p.to_string(), value));
                }
            } else {
                raw.push((key, value));
            }
        }
        if decls.iter().enumerate().any(|(i, (p, _))| decls[..i].iter().any(|(q, _)| q == p)) {
            return Err(bad("duplicate namespace declaration"));
        }
        scope.frames.push(decls.clone());
        let (prefix, local) = split_qname(&qname)?;
        let ns = match scope.lookup(&prefix) {
            Some(u) => u.to_string(),
            None if prefix.is_empty() => String::new(),
            None => return Err(bad("unbound namespace prefix")),
        };
        if prefix == "xmlns" {
            return Err(bad("reserved namespace prefix"));
        }
        let mut attrs = Vec::with_capacity(raw.len());
        for (k, v) in raw {
            let (p, l) = split_qname(&k)?;
            let ans = if p.is_empty() { String::new() } else { scope.lookup(&p).ok_or_else(|| bad("unbound namespace prefix"))?.to_string() };
            if attrs.iter().any(|x: &Attr| x.ns == ans && x.local == l) {
                return Err(bad("duplicate attribute"));
            }
            attrs.push(Attr { prefix: p, local: l, ns: ans, value: v });
        }
        let parent = stack.last().copied();
        let id = self.push(Node::Element(Element { prefix, local, ns, attrs, decls, children: Vec::new(), parent }))?;
        if let Some(p) = parent {
            self.el_mut(p).children.push(id);
        }
        let ids: Vec<String> = self.el(id).attrs.iter().filter(|a| is_id_attr(a)).map(|a| a.value.clone()).collect();
        for v in ids {
            if self.ids.insert(v, id).is_some() {
                return Err(bad("duplicate ID"));
            }
        }
        Ok(id)
    }

    fn text(&mut self, stack: &[NodeId], s: &str) -> Result<(), XmlError> {
        let Some(&parent) = stack.last() else {
            if s.chars().all(|c| matches!(c, ' ' | '\t' | '\n' | '\r')) {
                return Ok(());
            }
            return Err(bad("text outside the root element"));
        };
        if s.is_empty() {
            return Ok(());
        }
        // Adjacent text (split by references, CDATA or comments) is one text node.
        if let Some(&last) = self.el(parent).children.last()
            && let Node::Text(t) = &mut self.nodes[last]
        {
            t.push_str(s);
            return Ok(());
        }
        let id = self.push(Node::Text(s.to_string()))?;
        self.el_mut(parent).children.push(id);
        Ok(())
    }

    fn el_mut(&mut self, id: NodeId) -> &mut Element {
        match &mut self.nodes[id] {
            Node::Element(e) => e,
            Node::Text(_) => unreachable!("an element id"),
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    /// The element `id` (ids handed out by this API are always elements).
    pub fn el(&self, id: NodeId) -> &Element {
        match &self.nodes[id] {
            Node::Element(e) => e,
            Node::Text(_) => unreachable!("an element id"),
        }
    }

    /// Child elements, in order.
    pub fn elements(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.el(id).children.iter().copied().filter(|c| matches!(self.nodes[*c], Node::Element(_)))
    }

    /// Child elements named `{ns}local`.
    pub fn children<'a>(&'a self, id: NodeId, ns: &'a str, local: &'a str) -> impl Iterator<Item = NodeId> + 'a {
        self.elements(id).filter(move |c| self.el(*c).is(ns, local))
    }

    /// The one child element named `{ns}local`: `Ok(None)` if there's none, `Err` if more than one.
    pub fn only_child(&self, id: NodeId, ns: &str, local: &str) -> Result<Option<NodeId>, XmlError> {
        let mut it = self.children(id, ns, local);
        let first = it.next();
        if it.next().is_some() {
            return Err(bad("an element that may appear once appears more than once"));
        }
        Ok(first)
    }

    /// Every element of the document named `{ns}local`, in document order.
    pub fn find_all(&self, ns: &str, local: &str) -> Vec<NodeId> {
        self.descendants(self.root).into_iter().filter(|n| self.el(*n).is(ns, local)).collect()
    }

    /// `id` and every element below it, in document order.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut todo = vec![id];
        while let Some(n) = todo.pop() {
            out.push(n);
            let kids: Vec<NodeId> = self.elements(n).collect();
            todo.extend(kids.into_iter().rev());
        }
        out
    }

    /// Whether `id` is `ancestor` or below it.
    pub fn within(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.el(c).parent;
        }
        false
    }

    /// The element's text: all of its text children, concatenated (child elements' text isn't).
    pub fn text_of(&self, id: NodeId) -> String {
        self.el(id)
            .children
            .iter()
            .filter_map(|c| match &self.nodes[*c] {
                Node::Text(t) => Some(t.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }

    /// Whether the element has element children.
    pub fn has_elements(&self, id: NodeId) -> bool {
        self.elements(id).next().is_some()
    }

    /// The element whose `ID` (or `Id`) attribute is `value`.
    pub fn by_id(&self, value: &str) -> Option<NodeId> {
        self.ids.get(value).copied()
    }

    /// The namespace declarations in scope at `id` (prefix → URI; `""` the default namespace,
    /// URI `""` when it was undeclared), without `xml`.
    pub fn in_scope(&self, id: NodeId) -> BTreeMap<String, String> {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            chain.push(c);
            cur = self.el(c).parent;
        }
        let mut out = BTreeMap::new();
        for c in chain.into_iter().rev() {
            for (p, u) in &self.el(c).decls {
                out.insert(p.clone(), u.clone());
            }
        }
        out
    }

    /// The exclusive canonical form (without comments) of the subtree at `apex`, leaving out the
    /// subtree at `exclude` (the enveloped-signature transform) and treating `inclusive`'s
    /// prefixes (`InclusiveNamespaces PrefixList`; `#default` is the default namespace) as
    /// Canonical XML does.
    pub fn exc_c14n(&self, apex: NodeId, exclude: Option<NodeId>, inclusive: &[String]) -> String {
        let inclusive: Vec<String> = inclusive.iter().map(|p| if p == "#default" { String::new() } else { p.clone() }).collect();
        let mut out = String::new();
        let mut rendered: Vec<Vec<(String, String)>> = Vec::new();
        self.c14n(apex, exclude, &inclusive, &mut rendered, &mut out);
        out
    }

    fn c14n(&self, id: NodeId, exclude: Option<NodeId>, inclusive: &[String], rendered: &mut Vec<Vec<(String, String)>>, out: &mut String) {
        let e = self.el(id);
        let scope = self.in_scope(id);
        // Visibly utilized prefixes: the element's, its attributes', and the inclusive ones.
        let mut used: Vec<String> = vec![e.prefix.clone()];
        for a in &e.attrs {
            if !a.prefix.is_empty() {
                used.push(a.prefix.clone());
            }
        }
        for p in inclusive {
            if p.is_empty() || scope.contains_key(p) {
                used.push(p.clone());
            }
        }
        used.sort();
        used.dedup();
        let in_effect = |p: &str| -> Option<String> {
            for f in rendered.iter().rev() {
                if let Some((_, u)) = f.iter().find(|(q, _)| q == p) {
                    return Some(u.clone());
                }
            }
            None
        };
        let mut decls: Vec<(String, String)> = Vec::new();
        for p in used {
            if p == "xml" {
                continue;
            }
            let uri = scope.get(&p).cloned().unwrap_or_default();
            if p.is_empty() {
                // The default namespace: render it when it differs from what the output
                // ancestors put in effect (`xmlns=""` only to undo a rendered default).
                if in_effect("").unwrap_or_default() != uri {
                    decls.push((p, uri));
                }
            } else if !uri.is_empty() && in_effect(&p).as_deref() != Some(uri.as_str()) {
                decls.push((p, uri));
            }
        }
        out.push('<');
        out.push_str(&e.qname());
        for (p, u) in &decls {
            if p.is_empty() {
                out.push_str(" xmlns=\"");
            } else {
                out.push_str(" xmlns:");
                out.push_str(p);
                out.push_str("=\"");
            }
            escape_attr(u, out);
            out.push('"');
        }
        let mut attrs: Vec<&Attr> = e.attrs.iter().collect();
        attrs.sort_by(|a, b| (a.ns.as_str(), a.local.as_str()).cmp(&(b.ns.as_str(), b.local.as_str())));
        for a in attrs {
            out.push(' ');
            if !a.prefix.is_empty() {
                out.push_str(&a.prefix);
                out.push(':');
            }
            out.push_str(&a.local);
            out.push_str("=\"");
            escape_attr(&a.value, out);
            out.push('"');
        }
        out.push('>');
        rendered.push(decls);
        for c in &e.children {
            if Some(*c) == exclude {
                continue;
            }
            match &self.nodes[*c] {
                Node::Text(t) => escape_text(t, out),
                Node::Element(_) => self.c14n(*c, exclude, inclusive, rendered, out),
            }
        }
        rendered.pop();
        out.push_str("</");
        out.push_str(&e.qname());
        out.push('>');
    }
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c14n_of(xml: &str, local: &str) -> String {
        let d = Doc::parse(xml).unwrap();
        let n = d.descendants(d.root()).into_iter().find(|n| d.el(*n).local == local).unwrap();
        d.exc_c14n(n, None, &[])
    }

    /// The example of the Exclusive XML Canonicalization recommendation, section 2.2: the
    /// namespace an ancestor declares but the subtree doesn't use isn't rendered.
    #[test]
    fn spec_example() {
        let xml = r#"<n0:local xmlns:n0="foo:bar" xmlns:n3="ftp://example.org"><n1:elem2 xmlns:n1="http://example.net" xml:lang="en"><n3:stuff xmlns:n3="ftp://example.org"/></n1:elem2></n0:local>"#;
        assert_eq!(c14n_of(xml, "elem2"), r#"<n1:elem2 xmlns:n1="http://example.net" xml:lang="en"><n3:stuff xmlns:n3="ftp://example.org"></n3:stuff></n1:elem2>"#);
        let xml2 = r#"<n2:pdu xmlns:n1="http://example.com" xmlns:n2="http://foo.example" xml:lang="fr" xml:space="retain"><n1:elem2 xmlns:n1="http://example.net" xml:lang="en"><n3:stuff xmlns:n3="ftp://example.org"/></n1:elem2></n2:pdu>"#;
        assert_eq!(c14n_of(xml2, "elem2"), r#"<n1:elem2 xmlns:n1="http://example.net" xml:lang="en"><n3:stuff xmlns:n3="ftp://example.org"></n3:stuff></n1:elem2>"#);
    }

    #[test]
    fn namespaces_attributes_and_escaping() {
        // Ancestors' namespaces that the apex uses are rendered on it; attributes sort by
        // (namespace URI, local name); unprefixed attributes come first.
        let xml = "<r xmlns=\"urn:d\" xmlns:a=\"urn:a\" xmlns:b=\"urn:b\"><x b:z=\"1\" a:z=\"2\" y=\"&lt;&amp;&quot;\t\" a:y=\"3\">t &amp; &lt; &gt; \r\n<a:k/><k xmlns=\"\"/></x></r>";
        assert_eq!(
            c14n_of(xml, "x"),
            "<x xmlns=\"urn:d\" xmlns:a=\"urn:a\" xmlns:b=\"urn:b\" y=\"&lt;&amp;&quot; \" a:y=\"3\" a:z=\"2\" b:z=\"1\">t &amp; &lt; &gt; \n<a:k></a:k><k xmlns=\"\"></k></x>"
        );
        // A child re-declaring what's in effect renders nothing; character references survive
        // normalization.
        let xml = "<a:r xmlns:a=\"urn:a\"><a:c xmlns:a=\"urn:a\" v=\"&#10;&#9;\">&#13;</a:c></a:r>";
        assert_eq!(c14n_of(xml, "r"), "<a:r xmlns:a=\"urn:a\"><a:c v=\"&#xA;&#x9;\">&#xD;</a:c></a:r>");
        // Comments go; CDATA becomes text.
        let xml = "<r>a<!-- c -->b<![CDATA[<&>]]></r>";
        assert_eq!(c14n_of(xml, "r"), "<r>ab&lt;&amp;&gt;</r>");
    }

    #[test]
    fn inclusive_prefixes_and_exclusion() {
        let xml = r#"<r xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:s="urn:s"><s:a><s:sig/><s:b>x</s:b></s:a></r>"#;
        let d = Doc::parse(xml).unwrap();
        let a = d.find_all("urn:s", "a")[0];
        let sig = d.find_all("urn:s", "sig")[0];
        assert_eq!(d.exc_c14n(a, Some(sig), &[]), r#"<s:a xmlns:s="urn:s"><s:b>x</s:b></s:a>"#);
        assert_eq!(d.exc_c14n(a, Some(sig), &["xs".into(), "nope".into()]), r#"<s:a xmlns:s="urn:s" xmlns:xs="http://www.w3.org/2001/XMLSchema"><s:b>x</s:b></s:a>"#);
    }

    #[test]
    fn hostile_documents_are_refused() {
        for (xml, why) in [
            ("<!DOCTYPE r [<!ENTITY x \"y\">]><r>&x;</r>", "DTDs aren't allowed"),
            ("<r>&x;</r>", "entities aren't allowed"),
            ("<r><?pi x?></r>", "processing instructions aren't allowed"),
            ("<r a=\"1\" a=\"2\"/>", "malformed attribute"),
            ("<r xmlns:p=\"urn:x\" xmlns:q=\"urn:x\" p:a=\"1\" q:a=\"2\"/>", "duplicate attribute"),
            ("<r><a ID=\"x\"/><b ID=\"x\"/></r>", "duplicate ID"),
            ("<p:r/>", "unbound namespace prefix"),
            ("<r></s>", "not well-formed XML"),
            ("<r/><r/>", "more than one root element"),
            ("text<r/>", "text outside the root element"),
        ] {
            assert_eq!(Doc::parse(xml).unwrap_err().0, why, "{xml}");
        }
        let deep = "<a>".repeat(MAX_DEPTH + 1) + &"</a>".repeat(MAX_DEPTH + 1);
        assert!(Doc::parse(&deep).is_err());
        assert!(Doc::parse(&" ".repeat(MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn reading() {
        let d = Doc::parse("<?xml version=\"1.0\"?><r xmlns=\"urn:r\" ID=\"_1\"><n>admin<!---->@example.com</n><n>2</n><m/></r>").unwrap();
        let r = d.root();
        assert_eq!(d.by_id("_1"), Some(r));
        let n = d.children(r, "urn:r", "n").next().unwrap();
        assert_eq!(d.text_of(n), "admin@example.com", "a comment doesn't cut the value short");
        assert!(d.only_child(r, "urn:r", "n").is_err());
        assert!(d.only_child(r, "urn:r", "m").unwrap().is_some());
        assert_eq!(d.find_all("urn:r", "n").len(), 2);
        assert!(d.within(n, r) && !d.within(r, n));
        assert_eq!(d.in_scope(n).get(""), Some(&"urn:r".to_string()));
    }
}
