//! Serialization (libxml2 xmlsave.c): `xmlSaveTree` / `xmlSaveDoc` with the options
//! Postgres uses, and `xmlNodeDump`.

use super::tree::{
    AttrDefault, AttrType, ContentKind, Decl, Doc, ElemContent, ElemType, EntityType, Kind, Node,
    NodeId, Ns, Occur,
};

/// `xmlTreeIndentString` repeated up to `MAX_INDENT` (60) bytes.
const INDENT_SIZE: i32 = 2;
const INDENT_NR: i32 = 30;

/// An `xmlSaveCtxt` writing to a string.
pub struct Saver<'d> {
    doc: &'d Doc,
    pub out: String,
    /// 1 with `XML_SAVE_FORMAT`.
    format: i32,
    level: i32,
    /// `ctxt->escape == xmlEscapeEntities` (no output encoding): text gets non-ASCII as
    /// character references. Otherwise `xmlEscapeContent`.
    escape_entities: bool,
    no_decl: bool,
    /// Attribute values and namespace URIs of a node without a document: non-ASCII as
    /// character references.
    ascii_attrs: bool,
    /// The declarations `xmlCopyNode` added to the copy's root, after its own.
    extra_ns: Option<(NodeId, Vec<Ns>)>,
    /// Dumping an `xmlCopyDoc` copy: the declaration says UTF-8 and the DTD keeps only what
    /// `xmlCopyDtd` copies.
    copy: bool,
}

impl<'d> Saver<'d> {
    /// `xmlSaveToBuffer(buf, NULL, options)`.
    pub fn new(doc: &'d Doc, format: bool, no_decl: bool) -> Saver<'d> {
        Saver {
            doc,
            out: String::new(),
            format: format as i32,
            level: 0,
            escape_entities: true,
            no_decl,
            ascii_attrs: false,
            extra_ns: None,
            copy: false,
        }
    }

    /// `xmlNodeDump(buf, NULL, copy, 0, 0)` of a copied node: content escaped with
    /// `xmlEscapeContent`, attributes as for a node without a document.
    pub fn node_dump(&mut self) {
        self.escape_entities = false;
        self.ascii_attrs = true;
    }

    /// `xmlNodeDumpOutput` with an encoding: content escaped with `xmlEscapeContent`;
    /// attributes with non-ASCII as character references when the document has no encoding.
    pub fn node_dump_output(&mut self, ascii_attrs: bool) {
        self.escape_entities = false;
        self.ascii_attrs = ascii_attrs;
    }

    /// Dumping a document copy (`xmlCopyDoc`) with `xmlNodeDump`.
    pub fn copy_dump(&mut self) {
        self.copy = true;
    }

    /// Namespace declarations to print on `id` after its own.
    pub fn set_extra_ns(&mut self, id: NodeId, ns: Vec<Ns>) {
        self.extra_ns = Some((id, ns));
    }

    fn indent(&mut self, level: i32) {
        let n = level.clamp(0, INDENT_NR) * INDENT_SIZE;
        self.out.extend(std::iter::repeat_n(' ', n as usize));
    }

    /// `xmlOutputBufferWriteEscape` for text content.
    fn escape_text(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                '&' => self.out.push_str("&amp;"),
                '\r' if self.escape_entities => self.out.push_str("&#xD;"),
                '\r' => self.out.push_str("&#13;"),
                c if !self.escape_entities => self.out.push(c),
                '\t' | '\n' | ' '..='\x7f' => self.out.push(c),
                c if (c as u32) < 0x80 => super::hex_char_ref(&mut self.out, 0xfffd),
                c => {
                    let v = c as u32;
                    super::hex_char_ref(
                        &mut self.out,
                        if matches!(v, 0xfffe | 0xffff) {
                            0xfffd
                        } else {
                            v
                        },
                    )
                }
            }
        }
    }

    /// `xmlBufAttrSerializeTxtContent` with the node's document: Postgres's documents always
    /// have an encoding, so non-ASCII stays as is.
    fn attr_text(&mut self, s: &str) {
        if self.ascii_attrs {
            super::attr_escape(&mut self.out, s);
            return;
        }
        for c in s.chars() {
            match c {
                '\n' => self.out.push_str("&#10;"),
                '\r' => self.out.push_str("&#13;"),
                '\t' => self.out.push_str("&#9;"),
                '"' => self.out.push_str("&quot;"),
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                '&' => self.out.push_str("&amp;"),
                c => self.out.push(c),
            }
        }
    }

    /// `xmlOutputBufferWriteQuotedString`.
    fn quoted(&mut self, s: &str) {
        if s.contains('"') {
            if s.contains('\'') {
                self.out.push('"');
                self.out.push_str(&s.replace('"', "&quot;"));
                self.out.push('"');
            } else {
                self.out.push('\'');
                self.out.push_str(s);
                self.out.push('\'');
            }
        } else {
            self.out.push('"');
            self.out.push_str(s);
            self.out.push('"');
        }
    }

    fn qname(&mut self, id: NodeId) {
        let q = self.doc.qname(id);
        self.out.push_str(&q);
    }

    /// `xmlAttrDumpOutput`.
    fn attr(&mut self, id: NodeId) {
        self.out.push(' ');
        self.qname(id);
        self.out.push_str("=\"");
        self.attr_content(id);
        self.out.push('"');
    }

    /// `xmlAttrSerializeContent`: an attribute's value, escaped.
    fn attr_content(&mut self, id: NodeId) {
        for &c in &self.doc.nodes[id].children {
            let n = &self.doc.nodes[c];
            match n.kind {
                Kind::Text => self.attr_text(&n.content),
                Kind::EntityRef => {
                    self.out.push('&');
                    self.out.push_str(&n.name);
                    self.out.push(';');
                }
                _ => {}
            }
        }
    }

    /// `xmlNsDumpOutput` for a namespace declaration.
    fn ns_def(&mut self, prefix: Option<&str>, href: Option<&str>) {
        let Some(href) = href else { return };
        if prefix == Some("xml") {
            return;
        }
        self.out.push(' ');
        match prefix {
            Some(p) => {
                self.out.push_str("xmlns:");
                self.out.push_str(p);
            }
            None => self.out.push_str("xmlns"),
        }
        self.out.push_str("=\"");
        self.attr_text(href);
        self.out.push('"');
    }

    /// `<` and the element's name and namespace declarations (`nsDef`); whether it has any.
    fn start_tag(&mut self, id: NodeId) -> bool {
        let n = &self.doc.nodes[id];
        self.out.push('<');
        self.qname(id);
        for ns in &n.ns_defs {
            self.ns_def(ns.prefix.as_deref(), ns.href.as_deref());
        }
        let mut any = !n.ns_defs.is_empty();
        if let Some((root, extra)) = self.extra_ns.take() {
            if root == id {
                for ns in &extra {
                    self.ns_def(ns.prefix.as_deref(), ns.href.as_deref());
                }
                any |= !extra.is_empty();
            }
            self.extra_ns = Some((root, extra));
        }
        any
    }

    /// `xmlSaveTree`.
    pub fn save_tree(&mut self, id: NodeId) {
        self.node(id, true);
    }

    /// `xmlNodeDumpOutputInternal`, recursively.
    fn node(&mut self, id: NodeId, is_root: bool) {
        let doc = self.doc;
        let n = &doc.nodes[id];
        match n.kind {
            Kind::Document => self.save_doc(),
            Kind::Dtd => self.dtd(id),
            Kind::ElementDecl => self.element_decl(id),
            Kind::AttributeDecl => self.attribute_decl(id),
            Kind::EntityDecl => self.entity_decl(id),
            Kind::Element => {
                if !is_root && self.format == 1 {
                    self.indent(self.level);
                }
                self.start_tag(id);
                for &a in &n.attrs {
                    self.attr(a);
                }
                if n.children.is_empty() {
                    self.out.push_str("/>");
                    return;
                }
                let saved = self.format;
                let mut unformatted = false;
                if self.format == 1
                    && n.children.iter().any(|&c| {
                        matches!(
                            doc.nodes[c].kind,
                            Kind::Text | Kind::CData | Kind::EntityRef
                        )
                    })
                {
                    self.format = 0;
                    unformatted = true;
                }
                self.out.push('>');
                if self.format == 1 {
                    self.out.push('\n');
                }
                if self.level >= 0 {
                    self.level += 1;
                }
                for &c in &n.children {
                    self.node(c, false);
                    if self.format == 1 {
                        self.out.push('\n');
                    }
                }
                if self.level > 0 {
                    self.level -= 1;
                }
                if self.format == 1 {
                    self.indent(self.level);
                }
                self.out.push_str("</");
                self.qname(id);
                self.out.push('>');
                if unformatted {
                    self.format = saved;
                }
            }
            // libxslt's disable-output-escaping text (xmlStringTextNoenc).
            Kind::Text if n.name == "textnoenc" => self.out.push_str(&n.content),
            Kind::Text => self.escape_text(&n.content),
            Kind::Pi => {
                if !is_root && self.format == 1 {
                    self.indent(self.level);
                }
                self.out.push_str("<?");
                self.out.push_str(&n.name);
                if !n.no_content {
                    self.out.push(' ');
                    self.out.push_str(&n.content);
                }
                self.out.push_str("?>");
            }
            Kind::Comment => {
                if !is_root && self.format == 1 {
                    self.indent(self.level);
                }
                self.out.push_str("<!--");
                self.out.push_str(&n.content);
                self.out.push_str("-->");
            }
            Kind::EntityRef => {
                self.out.push('&');
                self.out.push_str(&n.name);
                self.out.push(';');
            }
            Kind::CData => {
                let c = &n.content;
                if c.is_empty() {
                    self.out.push_str("<![CDATA[]]>");
                } else {
                    // Split at each "]]>" so the sections stay well-formed.
                    let mut start = 0;
                    let b = c.as_bytes();
                    let mut end = 0;
                    while end < b.len() {
                        if b[end..].starts_with(b"]]>") {
                            end += 2;
                            self.out.push_str("<![CDATA[");
                            self.out.push_str(&c[start..end]);
                            self.out.push_str("]]>");
                            start = end;
                        }
                        end += 1;
                    }
                    if start != end {
                        self.out.push_str("<![CDATA[");
                        self.out.push_str(&c[start..]);
                        self.out.push_str("]]>");
                    }
                }
            }
            Kind::Attribute => self.attr(id),
            Kind::Namespace => {
                let ns = n.ns.as_ref();
                self.ns_def(
                    ns.and_then(|n| n.prefix.as_deref()),
                    ns.and_then(|n| n.href.as_deref()),
                );
            }
        }
    }

    /// `xmlDocContentDumpOutput` (the document's encoding is always set).
    pub fn save_doc(&mut self) {
        let doc = self.doc;
        if !self.no_decl {
            self.escape_entities = false;
            self.out.push_str("<?xml version=");
            match &doc.version {
                Some(v) => self.quoted(v),
                None => self.out.push_str("\"1.0\""),
            }
            if self.copy {
                self.out.push_str(" encoding=\"UTF-8\"");
            } else if let Some(e) = &doc.encoding {
                self.out.push_str(" encoding=");
                self.quoted(e);
            }
            match doc.standalone {
                0 => self.out.push_str(" standalone=\"no\""),
                1 => self.out.push_str(" standalone=\"yes\""),
                _ => {}
            }
            self.out.push_str("?>\n");
        }
        let xhtml = self.is_xhtml();
        for &c in &doc.nodes[super::tree::DOC].children {
            self.level = 0;
            if xhtml {
                self.xhtml_node(c, true);
            } else {
                self.node(c, true);
            }
            self.out.push('\n');
        }
    }

    /// `xmlIsXHTML` on the internal subset's identifiers: an XHTML 1.0 document's children
    /// are dumped by the XHTML rules.
    fn is_xhtml(&self) -> bool {
        let Some(Decl::Dtd {
            external_id,
            system_id,
        }) = self.doc.int_subset.map(|d| &self.doc.nodes[d].decl)
        else {
            return false;
        };
        external_id.as_deref().is_some_and(|p| {
            matches!(
                p,
                "-//W3C//DTD XHTML 1.0 Strict//EN"
                    | "-//W3C//DTD XHTML 1.0 Frameset//EN"
                    | "-//W3C//DTD XHTML 1.0 Transitional//EN"
            )
        }) || system_id.as_deref().is_some_and(|s| {
            matches!(
                s,
                "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"
                    | "http://www.w3.org/TR/xhtml1/DTD/xhtml1-frameset.dtd"
                    | "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd"
            )
        })
    }

    /// `xhtmlNodeDumpOutput`: elements follow XHTML 1.0's compatibility guidelines; other
    /// nodes come out as in XML, but never indented.
    fn xhtml_node(&mut self, id: NodeId, is_root: bool) {
        if self.doc.nodes[id].kind == Kind::Element {
            self.xhtml_element(id, is_root);
        } else {
            self.node(id, true);
        }
    }

    fn xhtml_element(&mut self, id: NodeId, is_root: bool) {
        const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
        const META: &str =
            "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\" />";
        let doc = self.doc;
        let n = &doc.nodes[id];
        if !is_root && self.format == 1 {
            self.indent(self.level);
        }
        let has_ns_def = self.start_tag(id);
        // A.3.1.1: the root of a strictly conforming document declares the namespace.
        if n.name == "html" && n.ns.is_none() && !has_ns_def {
            self.out.push_str(" xmlns=\"");
            self.out.push_str(XHTML_NS);
            self.out.push('"');
        }
        self.xhtml_attrs(id);
        // The head of the root html element gets a Content-Type meta unless it has one.
        let addmeta = n.name == "head"
            && n.parent.is_some_and(|p| {
                doc.nodes[p].kind == Kind::Element
                    && doc.nodes[p].name == "html"
                    && doc.nodes[p].parent == Some(super::tree::DOC)
            })
            && !n.children.iter().any(|&c| {
                let m = &doc.nodes[c];
                m.kind == Kind::Element
                    && m.name == "meta"
                    && m.attrs.iter().any(|&a| {
                        doc.nodes[a].name == "http-equiv"
                            && doc.content(a).eq_ignore_ascii_case("Content-Type")
                    })
            });
        let prefixed = n.ns.as_ref().is_some_and(|ns| ns.prefix.is_some());
        if n.children.is_empty() {
            // C.2 (empty elements) and C.3 (no minimized form for the others).
            if !prefixed && !addmeta && xhtml_is_empty(n) {
                self.out.push_str(" />");
                return;
            }
            self.out.push('>');
            if addmeta {
                if self.format == 1 {
                    self.out.push('\n');
                    self.indent(self.level + 1);
                }
                self.out.push_str(META);
                if self.format == 1 {
                    self.out.push('\n');
                }
            }
            self.out.push_str("</");
            self.qname(id);
            self.out.push('>');
            return;
        }
        self.out.push('>');
        if addmeta {
            if self.format == 1 {
                self.out.push('\n');
                self.indent(self.level + 1);
            }
            self.out.push_str(META);
        }
        let saved = self.format;
        let mut unformatted = false;
        if self.format == 1
            && n.children
                .iter()
                .any(|&c| matches!(doc.nodes[c].kind, Kind::Text | Kind::EntityRef))
        {
            self.format = 0;
            unformatted = true;
        }
        if self.format == 1 {
            self.out.push('\n');
        }
        if self.level >= 0 {
            self.level += 1;
        }
        for &c in &n.children {
            self.xhtml_node(c, false);
            if self.format == 1 {
                self.out.push('\n');
            }
        }
        if self.level > 0 {
            self.level -= 1;
        }
        if self.format == 1 {
            self.indent(self.level);
        }
        self.out.push_str("</");
        self.qname(id);
        self.out.push('>');
        if unformatted {
            self.format = saved;
        }
    }

    /// `xhtmlAttrListDumpOutput`: the attributes, then an `id` copying `name` (C.8) and
    /// `lang` / `xml:lang` mirroring each other (C.7).
    fn xhtml_attrs(&mut self, id: NodeId) {
        let doc = self.doc;
        let (mut id_attr, mut name, mut lang, mut xml_lang) = (None, None, None, None);
        for &a in &doc.nodes[id].attrs {
            let an = &doc.nodes[a];
            match (&an.ns, an.name.as_str()) {
                (None, "id") => id_attr = Some(a),
                (None, "name") => name = Some(a),
                (None, "lang") => lang = Some(a),
                (Some(ns), "lang") if ns.prefix.as_deref() == Some("xml") => xml_lang = Some(a),
                _ => {}
            }
            self.attr(a);
        }
        if let Some(name) = name
            && id_attr.is_none()
            && matches!(
                doc.nodes[id].name.as_str(),
                "a" | "p" | "div" | "img" | "map" | "applet" | "form" | "frame" | "iframe"
            )
        {
            self.out.push_str(" id=\"");
            self.attr_content(name);
            self.out.push('"');
        }
        match (lang, xml_lang) {
            (Some(l), None) => {
                self.out.push_str(" xml:lang=\"");
                self.attr_content(l);
                self.out.push('"');
            }
            (None, Some(l)) => {
                self.out.push_str(" lang=\"");
                self.attr_content(l);
                self.out.push('"');
            }
            _ => {}
        }
    }

    /// `xmlDtdDumpOutput`.
    fn dtd(&mut self, id: NodeId) {
        let doc = self.doc;
        let n = &doc.nodes[id];
        self.out.push_str("<!DOCTYPE ");
        self.out.push_str(&n.name);
        if let Decl::Dtd {
            external_id,
            system_id,
        } = &n.decl
        {
            if let Some(e) = external_id {
                self.out.push_str(" PUBLIC ");
                self.quoted(e);
                self.out.push(' ');
                self.quoted(system_id.as_deref().unwrap_or(""));
            } else if let Some(s) = system_id {
                self.out.push_str(" SYSTEM ");
                self.quoted(s);
            }
        }
        if doc.entities.is_empty()
            && doc.elem_decls.is_empty()
            && doc.attr_decls.is_empty()
            && doc.notations.is_empty()
        {
            self.out.push('>');
            return;
        }
        self.out.push_str(" [\n");
        // libxml scans its notation hash table, whose seed comes from xmlRandom (time and
        // addresses at xmlInitParser): Postgres's order changes from one backend to the
        // next, so declaration order is as good as any.
        for (name, public, system) in &doc.notations {
            self.out.push_str("<!NOTATION ");
            self.out.push_str(name);
            if let Some(p) = public {
                self.out.push_str(" PUBLIC ");
                self.quoted(p);
                if let Some(s) = system {
                    self.out.push(' ');
                    self.quoted(s);
                }
            } else {
                self.out.push_str(" SYSTEM ");
                self.quoted(system.as_deref().unwrap_or(""));
            }
            self.out.push_str(" >\n");
        }
        let (format, level) = (self.format, self.level);
        self.format = 0;
        self.level = -1;
        for &c in &n.children {
            if self.copy
                && !matches!(
                    doc.nodes[c].kind,
                    Kind::EntityDecl | Kind::ElementDecl | Kind::AttributeDecl | Kind::Comment
                )
            {
                continue;
            }
            self.node(c, true);
        }
        self.format = format;
        self.level = level;
        self.out.push_str("]>");
    }

    fn occur(&mut self, o: Occur) {
        match o {
            Occur::Once => {}
            Occur::Opt => self.out.push('?'),
            Occur::Mult => self.out.push('*'),
            Occur::Plus => self.out.push('+'),
        }
    }

    /// `xmlBufDumpElementContent`'s walk.
    fn content(&mut self, c: &ElemContent, parent: Option<ContentKind>) {
        match c.kind {
            ContentKind::Pcdata => self.out.push_str("#PCDATA"),
            ContentKind::Element => {
                if let Some(p) = &c.prefix {
                    self.out.push_str(p);
                    self.out.push(':');
                }
                self.out.push_str(c.name.as_deref().unwrap_or(""));
            }
            ContentKind::Seq | ContentKind::Or => {
                let paren = parent.is_some_and(|p| p != c.kind || c.ocur != Occur::Once);
                if paren {
                    self.out.push('(');
                }
                if let Some(c1) = &c.c1 {
                    self.content(c1, Some(c.kind));
                }
                self.out.push_str(if c.kind == ContentKind::Seq {
                    " , "
                } else {
                    " | "
                });
                if let Some(c2) = &c.c2 {
                    self.content(c2, Some(c.kind));
                }
                if paren {
                    self.out.push(')');
                }
            }
        }
        if parent.is_some() {
            self.occur(c.ocur);
        }
    }

    /// `xmlBufDumpElementDecl`.
    fn element_decl(&mut self, id: NodeId) {
        let n = &self.doc.nodes[id];
        let Decl::Element {
            etype,
            content,
            prefix,
        } = &n.decl
        else {
            return;
        };
        self.out.push_str("<!ELEMENT ");
        if let Some(p) = prefix {
            self.out.push_str(p);
            self.out.push(':');
        }
        self.out.push_str(&n.name);
        self.out.push(' ');
        match etype {
            ElemType::Empty => self.out.push_str("EMPTY"),
            ElemType::Any => self.out.push_str("ANY"),
            ElemType::Mixed | ElemType::Element => {
                if let Some(c) = content {
                    self.out.push('(');
                    self.content(c, None);
                    self.out.push(')');
                    self.occur(c.ocur);
                }
            }
        }
        self.out.push_str(">\n");
    }

    /// `xmlBufDumpAttributeDecl`.
    fn attribute_decl(&mut self, id: NodeId) {
        let n = &self.doc.nodes[id];
        let Decl::Attribute {
            elem,
            prefix,
            atype,
            def,
            default,
            tree,
        } = &n.decl
        else {
            return;
        };
        self.out.push_str("<!ATTLIST ");
        self.out.push_str(elem);
        self.out.push(' ');
        if let Some(p) = prefix {
            self.out.push_str(p);
            self.out.push(':');
        }
        self.out.push_str(&n.name);
        match atype {
            AttrType::Cdata => self.out.push_str(" CDATA"),
            AttrType::Id => self.out.push_str(" ID"),
            AttrType::Idref => self.out.push_str(" IDREF"),
            AttrType::Idrefs => self.out.push_str(" IDREFS"),
            AttrType::Entity => self.out.push_str(" ENTITY"),
            AttrType::Entities => self.out.push_str(" ENTITIES"),
            AttrType::Nmtoken => self.out.push_str(" NMTOKEN"),
            AttrType::Nmtokens => self.out.push_str(" NMTOKENS"),
            AttrType::Enumeration | AttrType::Notation => {
                self.out.push_str(if *atype == AttrType::Enumeration {
                    " ("
                } else {
                    " NOTATION ("
                });
                self.out.push_str(&tree.join(" | "));
                self.out.push(')');
            }
        }
        match def {
            AttrDefault::None => {}
            AttrDefault::Required => self.out.push_str(" #REQUIRED"),
            AttrDefault::Implied => self.out.push_str(" #IMPLIED"),
            AttrDefault::Fixed => self.out.push_str(" #FIXED"),
        }
        if let Some(d) = default {
            self.out.push_str(" \"");
            self.attr_text(d);
            self.out.push('"');
        }
        self.out.push_str(">\n");
    }

    /// `xmlBufDumpEntityDecl`.
    fn entity_decl(&mut self, id: NodeId) {
        let Decl::Entity(e) = self.doc.nodes[id].decl else {
            return;
        };
        let ent = &self.doc.entities[e];
        let param = matches!(
            ent.etype,
            EntityType::InternalParameter | EntityType::ExternalParameter
        );
        self.out
            .push_str(if param { "<!ENTITY % " } else { "<!ENTITY " });
        self.out.push_str(&ent.name);
        self.out.push(' ');
        if matches!(
            ent.etype,
            EntityType::ExternalGeneralParsed
                | EntityType::ExternalGeneralUnparsed
                | EntityType::ExternalParameter
        ) {
            match &ent.external_id {
                Some(x) => {
                    self.out.push_str("PUBLIC ");
                    self.quoted(x);
                    self.out.push(' ');
                }
                None => self.out.push_str("SYSTEM "),
            }
            self.quoted(ent.system_id.as_deref().unwrap_or(""));
        }
        if ent.etype == EntityType::ExternalGeneralUnparsed
            && let Some(c) = &ent.content
        {
            self.out.push_str(" NDATA ");
            self.out.push_str(ent.orig.as_deref().unwrap_or(c));
        }
        if matches!(
            ent.etype,
            EntityType::InternalGeneral | EntityType::InternalParameter
        ) {
            if let Some(o) = &ent.orig {
                self.quoted(o);
            } else {
                let c = ent.content.as_deref().unwrap_or("");
                if c.contains('%') {
                    self.out.push('"');
                    self.out
                        .push_str(&c.replace('"', "&quot;").replace('%', "&#x25;"));
                    self.out.push('"');
                } else {
                    self.quoted(c);
                }
            }
        }
        self.out.push_str(">\n");
    }
}

/// `xhtmlIsEmpty`: a childless XHTML element whose content model is EMPTY.
fn xhtml_is_empty(n: &Node) -> bool {
    n.kind == Kind::Element
        && n.children.is_empty()
        && n.ns
            .as_ref()
            .is_none_or(|ns| ns.href.as_deref() == Some("http://www.w3.org/1999/xhtml"))
        && matches!(
            n.name.as_str(),
            "area"
                | "br"
                | "base"
                | "basefont"
                | "col"
                | "frame"
                | "hr"
                | "img"
                | "input"
                | "isindex"
                | "link"
                | "meta"
                | "param"
        )
}
