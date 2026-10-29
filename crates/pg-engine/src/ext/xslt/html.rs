//! `xsl:output method="html"` for xslt_process: the result document libxslt 1.1.43 creates
//! for HTML output (transform.c `xsltApplyStylesheetInternal`: `htmlNewDoc` /
//! `htmlNewDocNoDtD`, and the html-root default when no method is given) and its
//! serialization (xsltutils.c `xsltSaveResultTo` + `xsltSaveResultToString`): libxml2
//! 2.13.9's HTMLtree.c `htmlSetMetaEncoding` and `htmlNodeDumpFormatOutput`, with the
//! element descriptions of HTMLparser.c and the output encoders of encoding.c.
#![allow(dead_code)]

use crate::types::encnames::encoding_id;
use crate::types::mbconv;
use crate::types::xml::attr_escape;
use crate::types::xml::tree::{DOC, Decl, Doc, Kind, Node, NodeId};

/// `htmlNewDoc(NULL, NULL)`'s doctype.
const DEFAULT_PUBLIC: &str = "-//W3C//DTD HTML 4.0 Transitional//EN";
const DEFAULT_SYSTEM: &str = "http://www.w3.org/TR/REC-html40/loose.dtd";

/// `xsltHTMLVersions` (transform.c 5590): `xsl:output version` to doctype identifiers.
const HTML_VERSIONS: &[(&str, Option<&str>, Option<&str>)] = &[
    ("5", None, Some("about:legacy-compat")),
    (
        "4.01frame",
        Some("-//W3C//DTD HTML 4.01 Frameset//EN"),
        Some("http://www.w3.org/TR/1999/REC-html401-19991224/frameset.dtd"),
    ),
    (
        "4.01strict",
        Some("-//W3C//DTD HTML 4.01//EN"),
        Some("http://www.w3.org/TR/1999/REC-html401-19991224/strict.dtd"),
    ),
    (
        "4.01trans",
        Some("-//W3C//DTD HTML 4.01 Transitional//EN"),
        Some("http://www.w3.org/TR/1999/REC-html401-19991224/loose.dtd"),
    ),
    (
        "4.01",
        Some("-//W3C//DTD HTML 4.01 Transitional//EN"),
        Some("http://www.w3.org/TR/1999/REC-html401-19991224/loose.dtd"),
    ),
    (
        "4.0strict",
        Some("-//W3C//DTD HTML 4.01//EN"),
        Some("http://www.w3.org/TR/html4/strict.dtd"),
    ),
    (
        "4.0trans",
        Some("-//W3C//DTD HTML 4.01 Transitional//EN"),
        Some("http://www.w3.org/TR/html4/loose.dtd"),
    ),
    (
        "4.0frame",
        Some("-//W3C//DTD HTML 4.01 Frameset//EN"),
        Some("http://www.w3.org/TR/html4/frameset.dtd"),
    ),
    (
        "4.0",
        Some("-//W3C//DTD HTML 4.01 Transitional//EN"),
        Some("http://www.w3.org/TR/html4/loose.dtd"),
    ),
    ("3.2", Some("-//W3C//DTD HTML 3.2//EN"), None),
];

/// `xsltGetHTMLIDs`: (public, system) for a known version, compared case-insensitively.
fn html_ids(version: &str) -> Option<(Option<&'static str>, Option<&'static str>)> {
    HTML_VERSIONS
        .iter()
        .find(|(v, _, _)| v.eq_ignore_ascii_case(version))
        .map(|&(_, p, s)| (p, s))
}

/// `xmlCreateIntSubset` on an HTML document: the doctype node goes first.
fn add_doctype(doc: &mut Doc, name: &str, public: Option<&str>, system: Option<&str>) {
    let mut n = Node::new(Kind::Dtd, name);
    n.decl = Decl::Dtd {
        external_id: public.map(str::to_string),
        system_id: system.map(str::to_string),
    };
    n.parent = Some(DOC);
    let id = doc.add(n);
    doc.nodes[DOC].children.insert(0, id);
    doc.int_subset = Some(id);
}

/// `htmlNewDoc(URI, ExternalID)`: an empty HTML document with a doctype, the HTML 4.0
/// Transitional one when neither identifier is given.
fn html_new_doc(system: Option<&str>, public: Option<&str>) -> Doc {
    let mut doc = Doc::new(None);
    doc.standalone = 1;
    let (public, system) = if public.is_none() && system.is_none() {
        (Some(DEFAULT_PUBLIC), Some(DEFAULT_SYSTEM))
    } else {
        (public, system)
    };
    add_doctype(&mut doc, "html", public, system);
    doc
}

/// The result document `xsltApplyStylesheetInternal` creates for `method="html"`
/// (transform.c 5911-5944): with doctype-public or doctype-system, that doctype; else
/// without `version`, no doctype; else the doctype `xsltGetHTMLIDs` gives for the version,
/// or HTML 4.0 Transitional's for an unknown one.
pub(crate) fn new_html_doc(
    doctype_public: Option<&str>,
    doctype_system: Option<&str>,
    version: Option<&str>,
) -> Doc {
    if doctype_public.is_some() || doctype_system.is_some() {
        return html_new_doc(doctype_system, doctype_public);
    }
    match version {
        None => {
            let mut doc = Doc::new(None);
            doc.standalone = 1;
            doc
        }
        Some(v) => {
            let (p, s) = html_ids(v).unwrap_or((None, None));
            html_new_doc(s, p)
        }
    }
}

/// The result document for a QName method `xhtml` (transform.c 5950-5954): always
/// `htmlNewDoc`, so HTML 4.0 Transitional's doctype when none is given. Unreachable from
/// xslt_process: that branch raises "unsupported method xhtml, using html" through
/// `xsltTransformError`, which sets `ctxt->state` to error, so the transformation fails
/// ("failed to apply stylesheet").
pub(crate) fn new_xhtml_doc(doctype_public: Option<&str>, doctype_system: Option<&str>) -> Doc {
    html_new_doc(doctype_system, doctype_public)
}

/// The default method selection after an XML-method transformation with no method given
/// (transform.c 6094-6126): when the root element is `html` (any case) without a
/// namespace and nothing but blank text precedes it, the document becomes an HTML document
/// (return true) and gets a doctype named after the root from doctype-public /
/// doctype-system, or from `version` through `xsltGetHTMLIDs` (no doctype for an unknown
/// version). The caller then serializes with [`save_html`] (indent defaulting to on).
pub(crate) fn detect_html(
    doc: &mut Doc,
    doctype_public: Option<&str>,
    doctype_system: Option<&str>,
    version: Option<&str>,
) -> bool {
    let Some(root) = doc.root_element() else {
        return false;
    };
    let r = &doc.nodes[root];
    if r.ns.is_some() || !r.name.eq_ignore_ascii_case("html") {
        return false;
    }
    for &c in &doc.nodes[DOC].children {
        if c == root {
            break;
        }
        let n = &doc.nodes[c];
        match n.kind {
            Kind::Element => return false,
            Kind::Text
                if !n
                    .content
                    .bytes()
                    .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')) =>
            {
                return false;
            }
            _ => {}
        }
    }
    let name = doc.nodes[root].name.clone();
    let (public, system) = if doctype_public.is_some() || doctype_system.is_some() {
        (doctype_public, doctype_system)
    } else {
        version.and_then(html_ids).unwrap_or((None, None))
    };
    if public.is_some() || system.is_some() {
        add_doctype(doc, &name, public, system);
    }
    true
}

/// `xsltSaveResultToString` for method html (`xhtml == false`, `indent` resolved: libxslt
/// turns an unset indent on) or the QName method xhtml (always indented): the bytes
/// xslt_process returns. Empty when the document has no children or only a doctype.
/// `htmlSetMetaEncoding(doc, encoding or "UTF-8")` first adds or updates the Content-Type
/// meta of the head (so it changes `doc`).
///
/// The encoding converts the output as libxml's output buffer does: none / UTF-8 / UTF8
/// write UTF-8; libxml's own ISO-8859-1, ASCII and US-ASCII encoders write characters they
/// can't encode as decimal character references (`&#8364;`); UTF-16LE / UTF-16BE / UTF-16
/// (with a byte order mark, little-endian) are libxml's too. Other names go to iconv, which
/// in the reference (musl) writes `*` for characters it can't encode; that is done here
/// through Postgres's conversion tables for the names Postgres knows (latin1,
/// windows-1252, ISO-8859-2, KOI8-R, ...). Names neither knows (and "html") write UTF-8.
/// Postgres returns those bytes as text without checking them, so a non-UTF-8 encoding
/// yields text that is not valid UTF-8.
pub(crate) fn save_html_bytes(
    doc: &mut Doc,
    encoding: Option<&str>,
    indent: bool,
    xhtml: bool,
) -> Vec<u8> {
    let kids = &doc.nodes[DOC].children;
    if kids.is_empty() || (kids.len() == 1 && doc.nodes[kids[0]].kind == Kind::Dtd) {
        return Vec::new();
    }
    set_meta_encoding(doc, encoding.unwrap_or("UTF-8"));
    let mut d = Dumper {
        doc,
        out: String::new(),
        format: indent || xhtml,
    };
    d.document();
    encode_output(d.out, encoding)
}

/// [`save_html_bytes`] as a string; bytes that aren't UTF-8 (a non-UTF-8 output encoding)
/// are replaced with U+FFFD.
pub(crate) fn save_html(
    doc: &mut Doc,
    encoding: Option<&str>,
    indent: bool,
    xhtml: bool,
) -> String {
    match String::from_utf8(save_html_bytes(doc, encoding, indent, xhtml)) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// htmlSetMetaEncoding

fn is_elem(doc: &Doc, id: NodeId, name: &str) -> bool {
    let n = &doc.nodes[id];
    n.kind == Kind::Element && n.name.eq_ignore_ascii_case(name)
}

/// The attribute's value when it is a single text node (what `htmlSetMetaEncoding` reads).
fn single_text(doc: &Doc, attr: NodeId) -> Option<&str> {
    match doc.nodes[attr].children.as_slice() {
        [t] if doc.nodes[*t].kind == Kind::Text => Some(&doc.nodes[*t].content),
        _ => None,
    }
}

fn new_attr(doc: &mut Doc, elem: NodeId, name: &str, value: &str) {
    let mut a = Node::new(Kind::Attribute, name);
    a.parent = Some(elem);
    let a = doc.add(a);
    let t = doc.text(value);
    doc.append(a, t);
    doc.nodes[elem].attrs.push(a);
}

/// `xmlStrcasestr`.
fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// `htmlSetMetaEncoding`: point the `http-equiv="Content-Type"` meta of the html head at
/// `encoding`, creating one as the head's first child when there is none.
fn set_meta_encoding(doc: &mut Doc, encoding: &str) {
    if encoding.eq_ignore_ascii_case("html") {
        return;
    }
    let mut newcontent = format!("text/html; charset={encoding}");
    if newcontent.len() > 99 {
        let mut end = 99;
        while !newcontent.is_char_boundary(end) {
            end -= 1;
        }
        newcontent.truncate(end);
    }
    // Where the search for meta elements starts: (parent, index of the first sibling).
    let mut head: Option<NodeId> = None;
    let mut scan: Option<(NodeId, usize)> = None;
    let mut html = None;
    for (i, &c) in doc.nodes[DOC].children.iter().enumerate() {
        if is_elem(doc, c, "html") {
            html = Some(c);
            break;
        }
        if is_elem(doc, c, "head") {
            head = Some(c);
            break;
        }
        if is_elem(doc, c, "meta") {
            scan = Some((DOC, i));
            break;
        }
    }
    if let Some(h) = html {
        for (i, &c) in doc.nodes[h].children.iter().enumerate() {
            if is_elem(doc, c, "head") {
                head = Some(c);
                break;
            }
            if is_elem(doc, c, "meta") {
                head = Some(h);
                scan = Some((h, i));
                break;
            }
        }
        if head.is_none() {
            return;
        }
    } else if head.is_none() && scan.is_none() {
        return;
    }
    if scan.is_none()
        && let Some(h) = head
    {
        scan = Some((h, 0));
    }
    let mut meta: Option<(NodeId, String)> = None;
    if let Some((parent, from)) = scan {
        'siblings: for &c in &doc.nodes[parent].children[from..] {
            if !is_elem(doc, c, "meta") {
                continue;
            }
            let mut http = false;
            let mut content: Option<&str> = None;
            for &a in &doc.nodes[c].attrs {
                let Some(value) = single_text(doc, a) else {
                    continue;
                };
                let name = &doc.nodes[a].name;
                if name.eq_ignore_ascii_case("http-equiv")
                    && value.eq_ignore_ascii_case("Content-Type")
                {
                    http = true;
                } else if name.eq_ignore_ascii_case("content") {
                    content = Some(value);
                }
                if http && let Some(content) = content {
                    meta = Some((c, content.to_string()));
                    break 'siblings;
                }
            }
        }
    }
    match meta {
        None => {
            let Some(head) = head else { return };
            let mut m = Node::new(Kind::Element, "meta");
            m.parent = Some(head);
            let m = doc.add(m);
            doc.nodes[head].children.insert(0, m);
            new_attr(doc, m, "http-equiv", "Content-Type");
            new_attr(doc, m, "content", &newcontent);
        }
        Some((m, content)) => {
            if contains_ignore_case(&content, encoding) {
                return;
            }
            // xmlSetProp: the un-namespaced attribute named exactly "content".
            let existing = doc.nodes[m]
                .attrs
                .iter()
                .copied()
                .find(|&a| doc.nodes[a].ns.is_none() && doc.nodes[a].name == "content");
            match existing {
                Some(a) => {
                    let t = doc.text(&newcontent);
                    doc.nodes[t].parent = Some(a);
                    doc.nodes[a].children = vec![t];
                }
                None => new_attr(doc, m, "content", &newcontent),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Element descriptions (HTMLparser.c html40ElementTable): saveEndTag, empty, isinline.

const S: u8 = 1;
const E: u8 = 2;
const I: u8 = 4;

/// Sorted by name, as `htmlTagLookup`'s bsearch needs.
const ELEMENTS: &[(&str, u8)] = &[
    ("a", I),
    ("abbr", I),
    ("acronym", I),
    ("address", 0),
    ("applet", I),
    ("area", S | E),
    ("b", I),
    ("base", S | E),
    ("basefont", S | E | I),
    ("bdo", I),
    ("big", I),
    ("blockquote", 0),
    ("body", 0),
    ("br", S | E | I),
    ("button", I),
    ("caption", 0),
    ("center", 0),
    ("cite", I),
    ("code", I),
    ("col", S | E),
    ("colgroup", 0),
    ("dd", 0),
    ("del", I),
    ("dfn", I),
    ("dir", 0),
    ("div", 0),
    ("dl", 0),
    ("dt", 0),
    ("em", I),
    ("embed", I),
    ("fieldset", 0),
    ("font", I),
    ("form", 0),
    ("frame", S | E),
    ("frameset", 0),
    ("h1", 0),
    ("h2", 0),
    ("h3", 0),
    ("h4", 0),
    ("h5", 0),
    ("h6", 0),
    ("head", 0),
    ("hr", S | E),
    ("html", 0),
    ("i", I),
    ("iframe", I),
    ("img", S | E | I),
    ("input", S | E | I),
    ("ins", I),
    ("isindex", S | E),
    ("kbd", I),
    ("label", I),
    ("legend", 0),
    ("li", S),
    ("link", S | E),
    ("map", I),
    ("menu", 0),
    ("meta", S | E),
    ("noframes", 0),
    ("noscript", 0),
    ("object", I),
    ("ol", 0),
    ("optgroup", 0),
    ("option", 0),
    ("p", 0),
    ("param", S | E),
    ("pre", 0),
    ("q", I),
    ("s", I),
    ("samp", I),
    ("script", I),
    ("select", I),
    ("small", I),
    ("span", I),
    ("strike", I),
    ("strong", I),
    ("style", 0),
    ("sub", I),
    ("sup", I),
    ("table", 0),
    ("tbody", 0),
    ("td", 0),
    ("textarea", I),
    ("tfoot", 0),
    ("th", 0),
    ("thead", 0),
    ("title", 0),
    ("tr", 0),
    ("tt", I),
    ("u", I),
    ("ul", 0),
    ("var", I),
];

#[derive(Clone, Copy)]
struct Desc {
    name: &'static str,
    flags: u8,
}

impl Desc {
    fn empty(self) -> bool {
        self.flags & E != 0
    }
    fn save_end_tag(self) -> bool {
        self.flags & S != 0
    }
    fn inline(self) -> bool {
        self.flags & I != 0
    }
}

/// `htmlTagLookup` (case-insensitive).
fn tag_lookup(name: &str) -> Option<Desc> {
    let lower = name.to_ascii_lowercase();
    ELEMENTS
        .binary_search_by(|(n, _)| (*n).cmp(lower.as_str()))
        .ok()
        .map(|i| Desc {
            name: ELEMENTS[i].0,
            flags: ELEMENTS[i].1,
        })
}

/// `htmlIsBooleanAttr`.
fn is_boolean_attr(name: &str) -> bool {
    [
        "checked", "compact", "declare", "defer", "disabled", "ismap", "multiple", "nohref",
        "noresize", "noshade", "nowrap", "readonly", "selected",
    ]
    .iter()
    .any(|b| b.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------------------
// The dump

/// `xmlEncodeEntitiesInternal` for an HTML document: `<`, `>`, `&` escaped, everything else
/// (non-ASCII and CR included) as is, other control characters dropped. In attribute values
/// (`attr`) `<!--...-->` (server-side includes) and `&{...}` (HTML 4 B.7.1) are kept.
fn encode_entities(out: &mut String, s: &str, attr: bool) {
    let mut i = 0;
    while let Some(c) = s[i..].chars().next() {
        let rest = &s[i..];
        match c {
            '<' if attr && rest.starts_with("<!--") && rest.contains("-->") => {
                let end = rest.find("-->").unwrap_or(0) + 3;
                out.push_str(&rest[..end]);
                i += end;
                continue;
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' if attr && rest[1..].starts_with('{') && rest.contains('}') => {
                let end = rest.find('}').unwrap_or(0) + 1;
                out.push_str(&rest[..end]);
                i += end;
                continue;
            }
            '&' => out.push_str("&amp;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
        i += c.len_utf8();
    }
}

/// `xmlURIEscapeStr(value, "\"#$%&+,/:;<=>?@[\\]^`{|}")`: bytes other than unreserved
/// characters and that list (spaces, controls, non-ASCII) as `%XX`.
fn uri_escape(out: &mut String, s: &str) {
    const KEEP: &[u8] = b"\"#$%&+,/:;<=>?@[\\]^`{|}-_.!~*'()";
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || KEEP.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// `xmlOutputBufferWriteQuotedString`.
fn quoted(out: &mut String, s: &str) {
    if s.contains('"') {
        if s.contains('\'') {
            out.push('"');
            out.push_str(&s.replace('"', "&quot;"));
            out.push('"');
        } else {
            out.push('\'');
            out.push_str(s);
            out.push('\'');
        }
    } else {
        out.push('"');
        out.push_str(s);
        out.push('"');
    }
}

struct Dumper<'d> {
    doc: &'d Doc,
    out: String,
    format: bool,
}

impl Dumper<'_> {
    /// The `name[0] != 'p'` test (p, pre, param, but any element name).
    fn name_starts_with_p(&self, id: NodeId) -> bool {
        self.doc.nodes[id].name.starts_with('p')
    }

    fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        let p = self.doc.nodes[id].parent?;
        let sibs = &self.doc.nodes[p].children;
        let i = sibs.iter().position(|&c| c == id)?;
        sibs.get(i + 1).copied()
    }

    fn is_text_or_ref(&self, id: NodeId) -> bool {
        matches!(self.doc.nodes[id].kind, Kind::Text | Kind::EntityRef)
    }

    /// `htmlNodeDumpFormatOutput` on the document node.
    fn document(&mut self) {
        let doc = self.doc;
        if let Some(dtd) = doc.int_subset {
            self.doctype(dtd);
        }
        for &c in &doc.nodes[DOC].children {
            self.node(c);
        }
        self.out.push('\n');
    }

    /// `htmlDtdDumpOutput`.
    fn doctype(&mut self, id: NodeId) {
        let n = &self.doc.nodes[id];
        self.out.push_str("<!DOCTYPE ");
        self.out.push_str(&n.name);
        if let Decl::Dtd {
            external_id,
            system_id,
        } = &n.decl
        {
            if let Some(p) = external_id {
                self.out.push_str(" PUBLIC ");
                quoted(&mut self.out, p);
                if let Some(s) = system_id {
                    self.out.push(' ');
                    quoted(&mut self.out, s);
                }
            } else if let Some(s) = system_id
                && s != "about:legacy-compat"
            {
                self.out.push_str(" SYSTEM ");
                quoted(&mut self.out, s);
            }
        }
        self.out.push_str(">\n");
    }

    fn qname(&mut self, id: NodeId) {
        let n = &self.doc.nodes[id];
        if let Some(p) = n.ns.as_ref().and_then(|ns| ns.prefix.as_deref()) {
            self.out.push_str(p);
            self.out.push(':');
        }
        self.out.push_str(&n.name);
    }

    /// `htmlAttrDumpOutput`.
    fn attr(&mut self, id: NodeId) {
        let doc = self.doc;
        let a = &doc.nodes[id];
        self.out.push(' ');
        self.qname(id);
        if a.children.is_empty() || is_boolean_attr(&a.name) {
            return;
        }
        // xmlNodeListGetString(doc, children, 0): attribute-escaped text, references kept.
        let mut value = String::new();
        for &c in &a.children {
            let n = &doc.nodes[c];
            match n.kind {
                Kind::Text | Kind::CData => encode_entities(&mut value, &n.content, true),
                Kind::EntityRef => {
                    value.push('&');
                    value.push_str(&n.name);
                    value.push(';');
                }
                _ => {}
            }
        }
        self.out.push('=');
        let parent = a.parent.map(|p| &doc.nodes[p]);
        let uri = a.ns.is_none()
            && parent.is_some_and(|p| p.ns.is_none())
            && (["href", "action", "src"]
                .iter()
                .any(|u| a.name.eq_ignore_ascii_case(u))
                || (a.name.eq_ignore_ascii_case("name")
                    && parent.is_some_and(|p| p.name.eq_ignore_ascii_case("a"))));
        if uri {
            let trimmed = value.trim_start_matches([' ', '\t', '\n', '\r']);
            let mut escaped = String::new();
            uri_escape(&mut escaped, trimmed);
            quoted(&mut self.out, &escaped);
        } else {
            quoted(&mut self.out, &value);
        }
    }

    /// One node of `htmlNodeDumpFormatOutput`'s walk.
    fn node(&mut self, id: NodeId) {
        let doc = self.doc;
        let n = &doc.nodes[id];
        match n.kind {
            Kind::Element => self.element(id),
            Kind::Attribute => self.attr(id),
            Kind::Text => {
                let raw = n.name == "textnoenc"
                    || n.parent.is_some_and(|p| {
                        let p = &doc.nodes[p];
                        p.kind == Kind::Element
                            && (p.name.eq_ignore_ascii_case("script")
                                || p.name.eq_ignore_ascii_case("style"))
                    });
                if raw {
                    self.out.push_str(&n.content);
                } else {
                    encode_entities(&mut self.out, &n.content, false);
                }
            }
            Kind::Comment => {
                self.out.push_str("<!--");
                self.out.push_str(&n.content);
                self.out.push_str("-->");
            }
            Kind::Pi => {
                self.out.push_str("<?");
                self.out.push_str(&n.name);
                if !n.no_content {
                    self.out.push(' ');
                    self.out.push_str(&n.content);
                }
                self.out.push('>');
            }
            Kind::EntityRef => {
                self.out.push('&');
                self.out.push_str(&n.name);
                self.out.push(';');
            }
            // HTML_PRESERVE_NODE is XML_CDATA_SECTION_NODE: its content as is.
            Kind::CData => self.out.push_str(&n.content),
            _ => {}
        }
    }

    fn element(&mut self, id: NodeId) {
        let doc = self.doc;
        let n = &doc.nodes[id];
        let info = if n.ns.is_none() {
            tag_lookup(&n.name)
        } else {
            None
        };
        let block = info.is_some_and(|d| !d.inline());
        self.out.push('<');
        self.qname(id);
        for ns in &n.ns_defs {
            // xmlNsListDumpOutput: no document, so non-ASCII as character references.
            let Some(href) = &ns.href else { continue };
            if ns.prefix.as_deref() == Some("xml") {
                continue;
            }
            match &ns.prefix {
                Some(p) => {
                    self.out.push_str(" xmlns:");
                    self.out.push_str(p);
                }
                None => self.out.push_str(" xmlns"),
            }
            self.out.push_str("=\"");
            attr_escape(&mut self.out, href);
            self.out.push('"');
        }
        for &a in &n.attrs {
            self.attr(a);
        }
        if info.is_some_and(Desc::empty) {
            self.out.push('>');
        } else if let (Some(&first), Some(&last)) = (n.children.first(), n.children.last()) {
            self.out.push('>');
            let several = n.children.len() > 1;
            let p_name = self.name_starts_with_p(id);
            if self.format && block && !self.is_text_or_ref(first) && several && !p_name {
                self.out.push('\n');
            }
            for &c in &n.children {
                self.node(c);
            }
            if self.format && block && !self.is_text_or_ref(last) && several && !p_name {
                self.out.push('\n');
            }
            self.out.push_str("</");
            self.qname(id);
            self.out.push('>');
        } else if info.is_some_and(|d| d.save_end_tag() && d.name != "html" && d.name != "body")
        {
            self.out.push('>');
        } else {
            self.out.push_str("></");
            self.qname(id);
            self.out.push('>');
        }
        if self.format
            && block
            && let Some(next) = self.next_sibling(id)
            && !self.is_text_or_ref(next)
            && let Some(p) = n.parent
            && p != DOC
            && !self.name_starts_with_p(p)
        {
            self.out.push('\n');
        }
    }
}

// ---------------------------------------------------------------------------------------
// Output encoding (xmlAllocOutputBuffer with xmlFindCharEncodingHandler(encoding))

/// libxml's single-byte encoders: characters from `limit` on as decimal references.
fn char_refs(s: &str, limit: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) < limit {
            out.push(c as u32 as u8);
        } else {
            out.extend_from_slice(format!("&#{};", c as u32).as_bytes());
        }
    }
    out
}

pub(super) fn encode_output(s: String, encoding: Option<&str>) -> Vec<u8> {
    let Some(enc) = encoding else {
        return s.into_bytes();
    };
    let is = |n: &str| enc.eq_ignore_ascii_case(n);
    if is("UTF-8") || is("UTF8") {
        return s.into_bytes();
    }
    if is("ISO-8859-1") {
        return char_refs(&s, 0x100);
    }
    if is("ASCII") || is("US-ASCII") {
        return char_refs(&s, 0x80);
    }
    if is("UTF-16LE") || is("UTF-16BE") || is("UTF-16") {
        let mut out = Vec::with_capacity(s.len() * 2 + 2);
        if is("UTF-16") {
            out.extend_from_slice(&[0xff, 0xfe]);
        }
        for u in s.encode_utf16() {
            if is("UTF-16BE") {
                out.extend_from_slice(&u.to_be_bytes());
            } else {
                out.extend_from_slice(&u.to_le_bytes());
            }
        }
        return out;
    }
    // iconv: an unknown name means no encoder (UTF-8); unencodable characters become '*'.
    let Some(id) = encoding_id(enc)
        .filter(|&id| !matches!(id, mbconv::UTF8 | mbconv::SQL_ASCII | mbconv::MULE_INTERNAL))
    else {
        return s.into_bytes();
    };
    if let Ok(b) = mbconv::convert(s.as_bytes(), mbconv::UTF8, id) {
        return b.into_owned();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let u = c.encode_utf8(&mut buf).as_bytes();
        match mbconv::convert(u, mbconv::UTF8, id) {
            Ok(b) => out.extend_from_slice(&b),
            Err(_) => out.push(b'*'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::xml::parser::{Options, Parser};

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        /// `method="html"`.
        Html,
        /// No method, html root: [`detect_html`].
        Auto,
    }
    use Mode::{Auto, Html};

    /// A reference case: `xslt_process` with `xsl:output` from the fields, whose result
    /// tree's top-level nodes are `xml` (`<doe>t</doe>` stands for a
    /// disable-output-escaping text node `t`), and the bytes it returned.
    struct Case {
        name: &'static str,
        xml: &'static str,
        mode: Mode,
        public: Option<&'static str>,
        system: Option<&'static str>,
        version: Option<&'static str>,
        encoding: Option<&'static str>,
        indent: Option<bool>,
        expected: &'static [u8],
    }

    fn parse(xml: &str) -> Doc {
        let mut p = Parser::new(
            Doc::new(None),
            Options {
                replace_entities: true,
                keep_blanks: true,
                dtd_attr: false,
            },
        );
        assert!(p.parse_document(format!("<r>{xml}</r>").as_bytes()), "{xml}");
        p.doc
    }

    /// Copy `id` of `src` under `parent` in `dst` as xsl:copy-of would (CDATA becomes text).
    fn graft(src: &Doc, id: NodeId, dst: &mut Doc, parent: NodeId) {
        let s = &src.nodes[id];
        if s.kind == Kind::Element && s.name == "doe" {
            for &c in &s.children {
                let mut t = src.nodes[c].clone();
                t.name = "textnoenc".into();
                t.children.clear();
                let t = dst.add(t);
                dst.append(parent, t);
            }
            return;
        }
        let mut n = s.clone();
        n.children.clear();
        n.attrs.clear();
        if n.kind == Kind::CData {
            n.kind = Kind::Text;
            n.name = "text".into();
        }
        let new = dst.add(n);
        dst.append(parent, new);
        for &a in &s.attrs {
            let mut an = src.nodes[a].clone();
            an.children.clear();
            an.parent = Some(new);
            let na = dst.add(an);
            dst.nodes[new].attrs.push(na);
            for &c in &src.nodes[a].children {
                graft(src, c, dst, na);
            }
        }
        for &c in &s.children {
            graft(src, c, dst, new);
        }
    }

    fn fill(dst: &mut Doc, xml: &str) {
        let src = parse(xml);
        let r = src.root_element().unwrap();
        for &c in &src.nodes[r].children {
            graft(&src, c, dst, DOC);
        }
    }

    fn run(c: &Case) -> Vec<u8> {
        let mut doc = match c.mode {
            Html => new_html_doc(c.public, c.system, c.version),
            Auto => Doc::new(Some("1.0")),
        };
        fill(&mut doc, c.xml);
        if let Auto = c.mode {
            assert!(
                detect_html(&mut doc, c.public, c.system, c.version),
                "{}",
                c.name
            );
        }
        save_html_bytes(&mut doc, c.encoding, c.indent.unwrap_or(true), false)
    }

    #[test]
    fn reference_cases() {
        let mut failed = Vec::new();
        for c in CASES {
            let got = run(c);
            if got != c.expected {
                failed.push(format!(
                    "{}:\n  got      {:?}\n  expected {:?}",
                    c.name,
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(c.expected)
                ));
            }
        }
        assert!(CASES.len() >= 80);
        assert!(
            failed.is_empty(),
            "{} of {} failed:\n{}",
            failed.len(),
            CASES.len(),
            failed.join("\n")
        );
    }

    #[test]
    fn xhtml_is_always_indented() {
        let xml =
            "<html><head><title>t</title></head><body><div><p>a</p><p>b</p></div></body></html>";
        let mut a = new_xhtml_doc(None, None);
        fill(&mut a, xml);
        let mut b = new_html_doc(None, None, Some("unknown"));
        fill(&mut b, xml);
        let x = save_html(&mut a, None, false, true);
        assert_eq!(x, save_html(&mut b, None, true, false));
        assert!(x.starts_with(
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.0 Transitional//EN\" \
             \"http://www.w3.org/TR/REC-html40/loose.dtd\">\n<html>\n<head>\n"
        ));
    }

    #[test]
    fn detection_needs_an_html_root_first() {
        for xml in [
            "x<html/>",
            "<x:html xmlns:x=\"urn:x\"/>",
            "<html xmlns=\"urn:x\"/>",
            "<body/>",
            "",
            "<!--c-->",
        ] {
            let mut doc = Doc::new(Some("1.0"));
            fill(&mut doc, xml);
            assert!(!detect_html(&mut doc, None, None, None), "{xml}");
        }
        let mut doc = Doc::new(Some("1.0"));
        fill(&mut doc, " \n<Html/>");
        assert!(detect_html(&mut doc, None, None, None));
        assert_eq!(
            save_html(&mut doc, None, true, false),
            " \n<Html></Html>\n"
        );
    }

    #[test]
    fn latin1_as_string() {
        let mut doc = new_html_doc(None, None, None);
        fill(&mut doc, "<p>é</p>");
        assert_eq!(
            save_html(&mut doc, Some("ISO-8859-1"), true, false),
            "<p>\u{fffd}</p>\n"
        );
    }

    const CASES: &[Case] = &[
    Case { name: "basic", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n<body><p>a<br>b</p></body>\n</html>\n".as_bytes() },
    Case { name: "basic_noindent", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"><title>t</title></head><body><p>a<br>b</p></body></html>\n".as_bytes() },
    Case { name: "basic_indent_yes", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(true), expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n<body><p>a<br>b</p></body>\n</html>\n".as_bytes() },
    Case { name: "empty_elems", xml: "<html><body><br/><hr/><img src=\"a.png\"/><input type=\"text\"/><area/><base/><basefont/><col/><frame/><isindex/><link/><meta name=\"a\" content=\"b\"/><param/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<br><hr>\n<img src=\"a.png\"><input type=\"text\"><area>\n<base>\n<basefont><col>\n<frame>\n<isindex>\n<link>\n<meta name=\"a\" content=\"b\">\n<param>\n</body></html>\n".as_bytes() },
    Case { name: "empty_elems_noindent", xml: "<html><body><br/><hr/><img src=\"a.png\"/><input type=\"text\"/><area/><base/><basefont/><col/><frame/><isindex/><link/><meta name=\"a\" content=\"b\"/><param/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html><body><br><hr><img src=\"a.png\"><input type=\"text\"><area><base><basefont><col><frame><isindex><link><meta name=\"a\" content=\"b\"><param></body></html>\n".as_bytes() },
    Case { name: "empty_with_children", xml: "<html><body><br>x</br><img><b>y</b></img><hr><p>z</p></hr><div/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<br><img><hr>\n<div></div>\n</body></html>\n".as_bytes() },
    Case { name: "li_save_end_tag", xml: "<html><body><ul><li/><li>a</li><li/></ul></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><ul>\n<li>\n<li>a</li>\n<li>\n</ul></body></html>\n".as_bytes() },
    Case { name: "childless_nonempty", xml: "<html><head/><body><p/><div/><span/><td/><script/><textarea/><foo/><a/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"></head>\n<body>\n<p></p>\n<div></div>\n<span></span><td></td>\n<script></script><textarea></textarea><foo></foo><a></a>\n</body>\n</html>\n".as_bytes() },
    Case { name: "html_only", xml: "<html/>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html></html>\n".as_bytes() },
    Case { name: "html_only_noindent", xml: "<html/>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html></html>\n".as_bytes() },
    Case { name: "body_root", xml: "<body/>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<body></body>\n".as_bytes() },
    Case { name: "boolean_attrs", xml: "<html><body><input checked=\"checked\" disabled=\"disabled\" readonly=\"x\"/><select multiple=\"multiple\"><option selected=\"selected\" value=\"1\">o</option></select><dl compact=\"compact\"/><hr noshade=\"noshade\"/><table><tr><td nowrap=\"nowrap\"/></tr></table><img ismap=\"ismap\"/><area nohref=\"nohref\"/><frame noresize=\"noresize\"/><script defer=\"defer\"/><object declare=\"declare\"/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<input checked disabled readonly><select multiple><option selected value=\"1\">o</option></select><dl compact></dl>\n<hr noshade>\n<table><tr><td nowrap></td></tr></table>\n<img ismap><area nohref>\n<frame noresize>\n<script defer></script><object declare></object>\n</body></html>\n".as_bytes() },
    Case { name: "boolean_case", xml: "<html><body><input CHECKED=\"x\" Selected=\"\"/><foo selected=\"no\" disabled=\"d\"/><x:y xmlns:x=\"urn:x\" x:checked=\"c\"/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<input CHECKED Selected><foo selected disabled></foo><x:y xmlns:x=\"urn:x\" x:checked></x:y>\n</body></html>\n".as_bytes() },
    Case { name: "uri_href", xml: "<html><body><a href=\"x y.html?a=1&amp;b=é\">l</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><a href=\"x%20y.html?a=1&amp;b=%C3%A9\">l</a></body></html>\n".as_bytes() },
    Case { name: "uri_action_src", xml: "<html><body><form action=\" /p q\"><img src=\"a b.png\"/></form><script src=\"é.js\"></script></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<form action=\"/p%20q\"><img src=\"a%20b.png\"></form>\n<script src=\"%C3%A9.js\"></script>\n</body></html>\n".as_bytes() },
    Case { name: "uri_name_on_a", xml: "<html><body><a name=\"n m\">x</a><p name=\"n m\">y</p><img name=\"a b\"/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<a name=\"n%20m\">x</a><p name=\"n m\">y</p>\n<img name=\"a b\">\n</body></html>\n".as_bytes() },
    Case { name: "uri_case", xml: "<html><body><a HREF=\"a b\" Src=\"c d\">x</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><a HREF=\"a%20b\" Src=\"c%20d\">x</a></body></html>\n".as_bytes() },
    Case { name: "uri_ssi", xml: "<html><body><a href=\"&lt;!--#x--&gt; y\">x</a><a href=\"&amp;{x y}\">z</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<a href=\"<!--#x-->%20y\">x</a><a href=\"&{x%20y}\">z</a>\n</body></html>\n".as_bytes() },
    Case { name: "uri_quotes", xml: "<html><body><a href=\"a&quot;b\">x</a><a href=\"a&quot;b'c\">y</a><a href=\"a'b\">z</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<a href='a\"b'>x</a><a href=\"a&quot;b'c\">y</a><a href=\"a'b\">z</a>\n</body></html>\n".as_bytes() },
    Case { name: "uri_blanks", xml: "<html><body><a href=\"  x\">1</a><a href=\"x  \">2</a><a href=\"&#9;&#10;x\">3</a><a href=\"a&#10;b&#13;c\">4</a><a href=\"\">5</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<a href=\"x\">1</a><a href=\"x%20%20\">2</a><a href=\"x\">3</a><a href=\"a%0Ab%0Dc\">4</a><a href=\"\">5</a>\n</body></html>\n".as_bytes() },
    Case { name: "uri_reserved", xml: "<html><body><a href=\"http://u:p@h:8/p;q/r?x=[1]&amp;y={2}|3^4`5\\6#f%20~!*()'\">x</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><a href=\"http://u:p@h:8/p;q/r?x=[1]&amp;y={2}|3^4`5\\6#f%20~!*()'\">x</a></body></html>\n".as_bytes() },
    Case { name: "uri_ns", xml: "<html xmlns:xl=\"http://www.w3.org/1999/xlink\"><body><a xl:href=\"a b\">x</a><q:a xmlns:q=\"urn:q\" href=\"a b\">y</q:a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html xmlns:xl=\"http://www.w3.org/1999/xlink\"><body>\n<a xl:href=\"a b\">x</a><q:a xmlns:q=\"urn:q\" href=\"a b\">y</q:a>\n</body></html>\n".as_bytes() },
    Case { name: "uri_emoji", xml: "<html><body><a href=\"&#128512; &#128;\">x</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><a href=\"%F0%9F%98%80%20%C2%80\">x</a></body></html>\n".as_bytes() },
    Case { name: "attr_escape", xml: "<html><body><p title=\"a&lt;b&gt;c&amp;d&quot;e f é\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title='a&lt;b&gt;c&amp;d\"e f é'>x</p></body></html>\n".as_bytes() },
    Case { name: "attr_escape_both_quotes", xml: "<html><body><p title=\"a&quot;b'c&lt;\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"a&quot;b'c&lt;\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_amp_brace", xml: "<html><body><p title=\"&amp;{x}\" class=\"&amp;{y\" id=\"&amp;x}\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"&{x}\" class=\"&amp;{y\" id=\"&amp;x}\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_ssi", xml: "<html><body><p title=\"&lt;!-- c --&gt;\" class=\"&lt;!-- open\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"<!-- c -->\" class=\"&lt;!-- open\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_ws", xml: "<html><body><p title=\"a&#10;b&#9;c&#13;d\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"a\nb\tc\rd\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_nonascii", xml: "<html><body><p title=\"é€&#160;&#128512;\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"é€ 😀\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_empty", xml: "<html><body><p title=\"\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p title=\"\">x</p></body></html>\n".as_bytes() },
    Case { name: "attr_prefixed", xml: "<html xmlns:x=\"urn:x\"><body><p x:title=\"v\" xml:lang=\"en\">x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html xmlns:x=\"urn:x\"><body><p x:title=\"v\" xml:lang=\"en\">x</p></body></html>\n".as_bytes() },
    Case { name: "text_escape", xml: "<html><body><p>a&lt;b&gt;c&amp;d\"e'f é € &#160;</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>a&lt;b&gt;c&amp;d\"e'f é €  </p></body></html>\n".as_bytes() },
    Case { name: "text_cr", xml: "<html><body><p>a&#13;b&#13;&#10;c</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>a\rb\r\nc</p></body></html>\n".as_bytes() },
    Case { name: "text_emoji", xml: "<html><body><p>&#128512;&#x10FFFF;</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>😀􏿿</p></body></html>\n".as_bytes() },
    Case { name: "script", xml: "<html><head><script>if (a &lt; b &amp;&amp; c &gt; d) x(\"é\");</script></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<script>if (a < b && c > d) x(\"é\");</script>\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "style", xml: "<html><head><style>a &gt; b { content: \"&amp;\" }</style></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<style>a > b { content: \"&\" }</style>\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "script_upper", xml: "<html><body><SCRIPT>a&lt;b</SCRIPT><Style>c&amp;d</Style></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<SCRIPT>a<b</SCRIPT><Style>c&d</Style>\n</body></html>\n".as_bytes() },
    Case { name: "script_nested", xml: "<html><body><script><b>x&lt;</b>y&lt;</script></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><script><b>x&lt;</b>y<</script></body></html>\n".as_bytes() },
    Case { name: "script_ns", xml: "<html><body><x:script xmlns:x=\"urn:x\">a&lt;b</x:script></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><x:script xmlns:x=\"urn:x\">a<b</x:script></body></html>\n".as_bytes() },
    Case { name: "comments", xml: "<html><!-- c --><body><!--x--><p>a<!--y-->b</p><div><!--z--><!--w--></div></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<!-- c --><body>\n<!--x--><p>a<!--y-->b</p>\n<div>\n<!--z--><!--w-->\n</div>\n</body>\n</html>\n".as_bytes() },
    Case { name: "comment_top", xml: "<!--top--><html><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<!--top--><html><body></body></html>\n".as_bytes() },
    Case { name: "comment_top_noindent", xml: "<!--top--><html><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<!--top--><html><body></body></html>\n".as_bytes() },
    Case { name: "pis", xml: "<?top t?><html><body><?pi data?><p>a<?x?>b</p><?y  z ?></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<?top t><html><body>\n<?pi data><p>a<?x>b</p>\n<?y z >\n</body></html>\n".as_bytes() },
    Case { name: "cdata", xml: "<html><body><p>a<![CDATA[x<y&z]]>b</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>ax&lt;y&amp;zb</p></body></html>\n".as_bytes() },
    Case { name: "doctype_public", xml: "<html><body/></html>", mode: Html, public: Some("-//W3C//DTD HTML 4.01//EN"), system: None, version: None, encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_system", xml: "<html><body/></html>", mode: Html, public: None, system: Some("http://www.w3.org/TR/html4/strict.dtd"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html SYSTEM \"http://www.w3.org/TR/html4/strict.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_both", xml: "<html><body/></html>", mode: Html, public: Some("-//W3C//DTD HTML 4.01//EN"), system: Some("http://www.w3.org/TR/html4/strict.dtd"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_legacy", xml: "<html><body/></html>", mode: Html, public: None, system: Some("about:legacy-compat"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html>\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_legacy_public", xml: "<html><body/></html>", mode: Html, public: Some("p"), system: Some("about:legacy-compat"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"p\" \"about:legacy-compat\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_quotes", xml: "<html><body/></html>", mode: Html, public: Some("a\"b"), system: Some("c'd"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC 'a\"b' \"c'd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_quotes_both", xml: "<html><body/></html>", mode: Html, public: None, system: Some("a\"b'c"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html SYSTEM \"a&quot;b'c\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_5", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("5"), encoding: None, indent: None, expected: "<!DOCTYPE html>\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_401", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("4.01"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/1999/REC-html401-19991224/loose.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_401strict", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("4.01STRICT"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/1999/REC-html401-19991224/strict.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_401frame", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("4.01frame"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Frameset//EN\" \"http://www.w3.org/TR/1999/REC-html401-19991224/frameset.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_40", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("4.0"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/html4/loose.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_40trans", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("4.0trans"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/html4/loose.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_32", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("3.2"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 3.2//EN\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_unknown", xml: "<html><body/></html>", mode: Html, public: None, system: None, version: Some("9"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.0 Transitional//EN\" \"http://www.w3.org/TR/REC-html40/loose.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "version_with_public", xml: "<html><body/></html>", mode: Html, public: Some("P"), system: None, version: Some("5"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"P\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "doctype_noindent", xml: "<html><body><p>x</p></body></html>", mode: Html, public: None, system: None, version: Some("5"), encoding: None, indent: Some(false), expected: "<!DOCTYPE html>\n<html><body><p>x</p></body></html>\n".as_bytes() },
    Case { name: "doctype_empty_result", xml: "", mode: Html, public: None, system: Some("s"), version: None, encoding: None, indent: None, expected: "".as_bytes() },
    Case { name: "doctype_only_comment", xml: "<!--c-->", mode: Html, public: None, system: Some("s"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html SYSTEM \"s\">\n<!--c-->\n".as_bytes() },
    Case { name: "empty_result", xml: "", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "".as_bytes() },
    Case { name: "meta_head_empty", xml: "<html><head/><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_head_title", xml: "<html><head><title>t</title><link rel=\"x\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n<link rel=\"x\">\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_existing_same", xml: "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/><title>t</title></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_existing_other", xml: "<html><head><title>t</title><meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-1\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<title>t</title>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_existing_case", xml: "<html><head><meta HTTP-EQUIV=\"content-type\" content=\"text/html; charset=utf-8\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head><meta HTTP-EQUIV=\"content-type\" content=\"text/html; charset=utf-8\"></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_existing_upper_content", xml: "<html><head><meta http-equiv=\"Content-Type\" CONTENT=\"text/plain\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" CONTENT=\"text/plain\" content=\"text/html; charset=UTF-8\"></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_other_http_equiv", xml: "<html><head><meta http-equiv=\"refresh\" content=\"5\"/><meta name=\"x\" content=\"y\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<meta http-equiv=\"refresh\" content=\"5\">\n<meta name=\"x\" content=\"y\">\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_no_content_attr", xml: "<html><head><meta http-equiv=\"Content-Type\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<meta http-equiv=\"Content-Type\">\n</head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_under_html", xml: "<html><meta name=\"a\" content=\"b\"/><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<meta name=\"a\" content=\"b\">\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_under_html_ct", xml: "<html><meta http-equiv=\"Content-Type\" content=\"x\"/><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_head_late", xml: "<html><body/><head><title>t</title></head></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<body></body>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n</html>\n".as_bytes() },
    Case { name: "meta_upper", xml: "<HTML><HEAD><TITLE>t</TITLE></HEAD><BODY/></HTML>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<HTML>\n<HEAD>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<TITLE>t</TITLE>\n</HEAD>\n<BODY></BODY>\n</HTML>\n".as_bytes() },
    Case { name: "meta_no_head", xml: "<html><body><p>x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>x</p></body></html>\n".as_bytes() },
    Case { name: "meta_root_head", xml: "<head><title>t</title></head>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n".as_bytes() },
    Case { name: "meta_root_div", xml: "<div><head/></div>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<div><head></head></div>\n".as_bytes() },
    Case { name: "meta_encoding_latin1", xml: "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/></head><body>é€</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("ISO-8859-1"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-1\"></head>\n<body>\xe9&#8364;</body>\n</html>\n" },
    Case { name: "meta_encoding_html", xml: "<html><head/><body>é</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("html"), indent: None, expected: "<html>\n<head></head>\n<body>é</body>\n</html>\n".as_bytes() },
    Case { name: "meta_encoding_long", xml: "<html><head/><body/></html>", mode: Html, public: None, system: None, version: None, encoding: Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_encoding_strcasestr", xml: "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1;x\"/></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: Some("ISO-8859-1"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1;x\"></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "meta_xhtml_ns", xml: "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head/><body><br/><p>x</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"></head><body><br></br><p>x</p></body></html>\n".as_bytes() },
    Case { name: "meta_two_heads", xml: "<html><head/><head><title>t</title></head><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"></head>\n<head><title>t</title></head>\n<body></body>\n</html>\n".as_bytes() },
    Case { name: "indent_blocks", xml: "<html><body><div><div><p>x</p><p>y</p></div></div><table><tr><td>1</td><td>2</td></tr></table><ul><li>a</li><li>b</li></ul></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<div><div>\n<p>x</p>\n<p>y</p>\n</div></div>\n<table><tr>\n<td>1</td>\n<td>2</td>\n</tr></table>\n<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n</body></html>\n".as_bytes() },
    Case { name: "indent_blocks_noindent", xml: "<html><body><div><div><p>x</p><p>y</p></div></div><table><tr><td>1</td><td>2</td></tr></table><ul><li>a</li><li>b</li></ul></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html><body><div><div><p>x</p><p>y</p></div></div><table><tr><td>1</td><td>2</td></tr></table><ul><li>a</li><li>b</li></ul></body></html>\n".as_bytes() },
    Case { name: "indent_inline", xml: "<html><body><p><b>x</b><i>y</i><span>z</span></p><div><b>x</b><i>y</i></div><div><em>a</em><div>b</div><em>c</em></div></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<p><b>x</b><i>y</i><span>z</span></p>\n<div>\n<b>x</b><i>y</i>\n</div>\n<div>\n<em>a</em><div>b</div>\n<em>c</em>\n</div>\n</body></html>\n".as_bytes() },
    Case { name: "indent_mixed", xml: "<html><body><div>t<div>a</div><div>b</div>u</div><div><div>a</div>t</div><div>t<div>a</div></div></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<div>t<div>a</div>\n<div>b</div>u</div>\n<div>\n<div>a</div>t</div>\n<div>t<div>a</div>\n</div>\n</body></html>\n".as_bytes() },
    Case { name: "indent_single_child", xml: "<html><body><div><div>x</div></div><div><div/></div></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<div><div>x</div></div>\n<div><div></div></div>\n</body></html>\n".as_bytes() },
    Case { name: "indent_pre_param", xml: "<html><body><pre><div>a</div><div>b</div></pre><object><param name=\"a\"/><param name=\"b\"/></object></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<pre><div>a</div><div>b</div></pre>\n<object><param name=\"a\">\n<param name=\"b\"></object>\n</body></html>\n".as_bytes() },
    Case { name: "indent_unknown", xml: "<html><body><foo><bar/><baz/></foo><div><foo/><foo/></div></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<foo><bar></bar><baz></baz></foo><div>\n<foo></foo><foo></foo>\n</div>\n</body></html>\n".as_bytes() },
    Case { name: "indent_p_parent", xml: "<html><body><price><div/><div/></price><Pdiv><div/><div/></Pdiv></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<price><div></div><div></div></price><Pdiv><div></div>\n<div></div></Pdiv>\n</body></html>\n".as_bytes() },
    Case { name: "indent_upper", xml: "<html><body><DIV><P>a</P><P>b</P></DIV></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><DIV>\n<P>a</P>\n<P>b</P>\n</DIV></body></html>\n".as_bytes() },
    Case { name: "indent_nested_inline", xml: "<html><body><p>a <b>b <i>c <a href=\"#\">d</a></i></b> e</p><span><div>x</div><div>y</div></span></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<p>a <b>b <i>c <a href=\"#\">d</a></i></b> e</p>\n<span><div>x</div>\n<div>y</div></span>\n</body></html>\n".as_bytes() },
    Case { name: "indent_whitespace", xml: "<html>\n  <body>\n    <div>\n      <p>x</p>\n    </div>\n  </body>\n</html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n  <body>\n    <div>\n      <p>x</p>\n    </div>\n  </body>\n</html>\n".as_bytes() },
    Case { name: "indent_whitespace_noindent", xml: "<html>\n  <body>\n    <div>\n      <p>x</p>\n    </div>\n  </body>\n</html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html>\n  <body>\n    <div>\n      <p>x</p>\n    </div>\n  </body>\n</html>\n".as_bytes() },
    Case { name: "indent_table", xml: "<html><body><table><thead><tr><th>h</th></tr></thead><tbody><tr><td>1</td></tr><tr><td>2</td></tr></tbody></table></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><table>\n<thead><tr><th>h</th></tr></thead>\n<tbody>\n<tr><td>1</td></tr>\n<tr><td>2</td></tr>\n</tbody>\n</table></body></html>\n".as_bytes() },
    Case { name: "indent_form", xml: "<html><body><form><input type=\"a\"/><input type=\"b\"/><select><option>1</option><option>2</option></select><textarea>t</textarea></form></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><form>\n<input type=\"a\"><input type=\"b\"><select><option>1</option>\n<option>2</option></select><textarea>t</textarea>\n</form></body></html>\n".as_bytes() },
    Case { name: "indent_head", xml: "<html><head><title>t</title><style>s</style><script>x</script><link/></head><body><h1>a</h1><h2>b</h2></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n<style>s</style>\n<script>x</script><link>\n</head>\n<body>\n<h1>a</h1>\n<h2>b</h2>\n</body>\n</html>\n".as_bytes() },
    Case { name: "ns_default", xml: "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>x<br/></p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>x<br></br></p></body></html>\n".as_bytes() },
    Case { name: "ns_prefixed", xml: "<x:html xmlns:x=\"urn:x\"><x:body><x:br/></x:body></x:html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<x:html xmlns:x=\"urn:x\"><x:body><x:br></x:br></x:body></x:html>\n".as_bytes() },
    Case { name: "ns_decl_on_html", xml: "<html xmlns:x=\"urn:x\" xmlns:y=\"urn:y&amp;é\"><body/></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html xmlns:x=\"urn:x\" xmlns:y=\"urn:y&amp;&#xE9;\"><body></body></html>\n".as_bytes() },
    Case { name: "nonascii_text", xml: "<html><body><p>árvíztűrő tükörfúrógép 日本語 &#128512;</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>árvíztűrő tükörfúrógép 日本語 😀</p></body></html>\n".as_bytes() },
    Case { name: "latin1", xml: "<html><head/><body><p title=\"é€\">é€&#160;</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("ISO-8859-1"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-1\"></head>\n<body><p title=\"\xe9&#8364;\">\xe9&#8364;\xa0</p></body>\n</html>\n" },
    Case { name: "latin1_lower", xml: "<html><head/><body><p>é€</p></body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("iso-8859-1"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1\"></head>\n<body><p>\xe9&#8364;</p></body>\n</html>\n" },
    Case { name: "ascii", xml: "<html><head/><body><p title=\"é€\">é€&lt;</p><a href=\"é\">x</a></body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("US-ASCII"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=US-ASCII\"></head>\n<body>\n<p title=\"&#233;&#8364;\">&#233;&#8364;&lt;</p>\n<a href=\"%C3%A9\">x</a>\n</body>\n</html>\n".as_bytes() },
    Case { name: "utf8_lower", xml: "<html><head/><body>é</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("utf-8"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\"></head>\n<body>é</body>\n</html>\n".as_bytes() },
    Case { name: "bogus_encoding", xml: "<html><head/><body>é</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("bogus"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=bogus\"></head>\n<body>é</body>\n</html>\n".as_bytes() },
    Case { name: "win1252", xml: "<html><head/><body>é€&#160;&#x2603;</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("windows-1252"), indent: None, expected: "<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\"></head>\n<body>造*</body>\n</html>\n".as_bytes() },
    Case { name: "latin1_iconv", xml: "<html><head/><body>é€</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("latin1"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=latin1\"></head>\n<body>\xe9*</body>\n</html>\n" },
    Case { name: "latin2_iconv", xml: "<html><head/><body>éő€</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("ISO-8859-2"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-2\"></head>\n<body>\xe9\xf5*</body>\n</html>\n" },
    Case { name: "utf16le", xml: "<html><head/><body>é</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("UTF-16LE"), indent: None, expected: b"<\x00h\x00t\x00m\x00l\x00>\x00\n\x00<\x00h\x00e\x00a\x00d\x00>\x00<\x00m\x00e\x00t\x00a\x00 \x00h\x00t\x00t\x00p\x00-\x00e\x00q\x00u\x00i\x00v\x00=\x00\"\x00C\x00o\x00n\x00t\x00e\x00n\x00t\x00-\x00T\x00y\x00p\x00e\x00\"\x00 \x00c\x00o\x00n\x00t\x00e\x00n\x00t\x00=\x00\"\x00t\x00e\x00x\x00t\x00/\x00h\x00t\x00m\x00l\x00;\x00 \x00c\x00h\x00a\x00r\x00s\x00e\x00t\x00=\x00U\x00T\x00F\x00-\x001\x006\x00L\x00E\x00\"\x00>\x00<\x00/\x00h\x00e\x00a\x00d\x00>\x00\n\x00<\x00b\x00o\x00d\x00y\x00>\x00\xe9\x00<\x00/\x00b\x00o\x00d\x00y\x00>\x00\n\x00<\x00/\x00h\x00t\x00m\x00l\x00>\x00\n\x00" },
    Case { name: "utf16", xml: "<html><head/><body>é</body></html>", mode: Html, public: None, system: None, version: None, encoding: Some("UTF-16"), indent: None, expected: b"\xff\xfe<\x00h\x00t\x00m\x00l\x00>\x00\n\x00<\x00h\x00e\x00a\x00d\x00>\x00<\x00m\x00e\x00t\x00a\x00 \x00h\x00t\x00t\x00p\x00-\x00e\x00q\x00u\x00i\x00v\x00=\x00\"\x00C\x00o\x00n\x00t\x00e\x00n\x00t\x00-\x00T\x00y\x00p\x00e\x00\"\x00 \x00c\x00o\x00n\x00t\x00e\x00n\x00t\x00=\x00\"\x00t\x00e\x00x\x00t\x00/\x00h\x00t\x00m\x00l\x00;\x00 \x00c\x00h\x00a\x00r\x00s\x00e\x00t\x00=\x00U\x00T\x00F\x00-\x001\x006\x00\"\x00>\x00<\x00/\x00h\x00e\x00a\x00d\x00>\x00\n\x00<\x00b\x00o\x00d\x00y\x00>\x00\xe9\x00<\x00/\x00b\x00o\x00d\x00y\x00>\x00\n\x00<\x00/\x00h\x00t\x00m\x00l\x00>\x00\n\x00" },
    Case { name: "auto_basic", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p></body></html>", mode: Auto, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n<body><p>a<br>b</p></body>\n</html>\n".as_bytes() },
    Case { name: "auto_noindent", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p></body></html>", mode: Auto, public: None, system: None, version: None, encoding: None, indent: Some(false), expected: "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"><title>t</title></head><body><p>a<br>b</p></body></html>\n".as_bytes() },
    Case { name: "auto_public", xml: "<html><body/></html>", mode: Auto, public: Some("P"), system: None, version: None, encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"P\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "auto_system", xml: "<html><body/></html>", mode: Auto, public: None, system: Some("S"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE html SYSTEM \"S\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "auto_version", xml: "<html><body/></html>", mode: Auto, public: None, system: None, version: Some("4.01"), encoding: None, indent: None, expected: "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://www.w3.org/TR/1999/REC-html401-19991224/loose.dtd\">\n<html><body></body></html>\n".as_bytes() },
    Case { name: "auto_version_unknown", xml: "<html><body/></html>", mode: Auto, public: None, system: None, version: Some("1.0"), encoding: None, indent: None, expected: "<html><body></body></html>\n".as_bytes() },
    Case { name: "auto_version_5", xml: "<html><body/></html>", mode: Auto, public: None, system: None, version: Some("5"), encoding: None, indent: None, expected: "<!DOCTYPE html>\n<html><body></body></html>\n".as_bytes() },
    Case { name: "auto_upper", xml: "<HTML><body/></HTML>", mode: Auto, public: None, system: Some("S"), version: None, encoding: None, indent: None, expected: "<!DOCTYPE HTML SYSTEM \"S\">\n<HTML><body></body></HTML>\n".as_bytes() },
    Case { name: "auto_comment_first", xml: "<!--c--><?p x?><html><body/></html>", mode: Auto, public: None, system: None, version: None, encoding: None, indent: None, expected: "<!--c--><?p x><html><body></body></html>\n".as_bytes() },
    Case { name: "auto_encoding", xml: "<html><head/><body>é€</body></html>", mode: Auto, public: None, system: None, version: None, encoding: Some("ISO-8859-1"), indent: None, expected: b"<html>\n<head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-1\"></head>\n<body>\xe9&#8364;</body>\n</html>\n" },
    Case { name: "lit_multi_top", xml: "<p>a</p><p>b</p>t<div><div>x</div><div>y</div></div>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<p>a</p><p>b</p>t<div>\n<div>x</div>\n<div>y</div>\n</div>\n".as_bytes() },
    Case { name: "lit_basic", xml: "<html><head><title>t</title></head><body><p>a<br/>b</p><hr/></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html>\n<head>\n<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\">\n<title>t</title>\n</head>\n<body>\n<p>a<br>b</p>\n<hr>\n</body>\n</html>\n".as_bytes() },
    Case { name: "lit_doe", xml: "<html><body><p><doe>&lt;b&gt;&amp;amp;é</doe></p><script><doe>a&lt;b</doe></script></body></html>", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body>\n<p><b>&amp;é</p>\n<script>a<b</script>\n</body></html>\n".as_bytes() },
    Case { name: "lit_auto", xml: "<html><body><p>x</p></body></html>", mode: Auto, public: None, system: None, version: None, encoding: None, indent: None, expected: "<html><body><p>x</p></body></html>\n".as_bytes() },
    Case { name: "lit_auto_ws_text", xml: " <html><body><p>x</p></body></html>", mode: Auto, public: None, system: None, version: None, encoding: None, indent: None, expected: " <html><body><p>x</p></body></html>\n".as_bytes() },
    Case { name: "lit_text_top", xml: "text&lt;", mode: Html, public: None, system: None, version: None, encoding: None, indent: None, expected: "text&lt;\n".as_bytes() },
    ];
}
