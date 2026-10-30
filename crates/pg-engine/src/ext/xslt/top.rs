//! The stylesheet's top level (xsltParseStylesheetProcess, xsltParseStylesheetTop and the
//! declarations it reads).

use super::Diag;
use super::compile::{Compiler, attr, is_blank_text, is_xslt, ns_list, search_ns};
use super::pattern;
use super::style::*;
use crate::types::xml::tree::{Doc, Kind, NodeId, NsRef};
use crate::types::xml::xpath;
use std::collections::HashMap;
use std::rc::Rc;

/// `xsltParseStylesheetDoc`: None when libxslt returns NULL.
pub(super) fn compile(doc: Doc, diag: &mut Diag) -> Option<Sheet> {
    let root = doc.root_element()?;
    let mut c = Compiler::new(doc, diag);
    let mut sheet = Sheet {
        doc: Doc::new(None),
        templates: Vec::new(),
        named: HashMap::new(),
        globals: Vec::new(),
        keys: Vec::new(),
        output: Output { omit_xml_declaration: -1, standalone: -1, indent: -1, ..Output::default() },
        space: HashMap::new(),
        strip_all: 0,
        has_space_rules: false,
        decimal_formats: Vec::new(),
        attr_sets: HashMap::new(),
        aliases: HashMap::new(),
    };
    let style_elem = is_xslt(&c.doc, root) && matches!(c.doc.nodes[root].name.as_str(), "stylesheet" | "transform");
    c.exclude_prefixes(root, style_elem);
    c.extension_prefixes(root, style_elem);
    c.preprocess_node(root, root, style_elem);
    if style_elem {
        c.parse_top(root, &mut sheet);
    } else {
        // A literal result element as the stylesheet: one template for "/".
        let version = super::compile::ns_attr(&c.doc, root, "version")?;
        c.set_forwards(&version);
        let body = c.body_of_doc();
        let pattern = pattern::compile(&mut c, root, "/").ok()?;
        sheet.templates.push(Template {
            pattern: Some(pattern),
            mode: None,
            priority: None,
            params: Vec::new(),
            body: Rc::new(body),
            inherited_ns: Vec::new(),
        });
    }
    if c.errors != 0 {
        return None;
    }
    sheet.doc = c.doc;
    Some(sheet)
}

impl Compiler<'_> {
    fn set_forwards(&mut self, version: &str) {
        if version != "1.0" && version != "1.1" {
            self.forwards = true;
        }
    }

    fn parse_top(&mut self, root: NodeId, sheet: &mut Sheet) {
        if let Some(v) = self.attr(root, "version") {
            self.set_forwards(&v);
        }
        let kids: Vec<NodeId> = self.doc.children(root).collect();
        let mut imports = true;
        let mut sets = Vec::new();
        for id in kids {
            if is_blank_text(&self.doc, id) {
                continue;
            }
            let n = &self.doc.nodes[id];
            if imports && is_xslt(&self.doc, id) && n.name == "import" {
                // Loading another stylesheet: xml2's documents come from strings, and the
                // engine reads no server files, so the import fails as a missing file does.
                self.load_failure(id);
                continue;
            }
            imports = false;
            match n.kind {
                Kind::Text | Kind::CData => {
                    self.errors += 1;
                    continue;
                }
                Kind::Element => {}
                _ => continue,
            }
            if n.ns.is_none() {
                self.errors += 1;
                continue;
            }
            if !is_xslt(&self.doc, id) {
                continue;
            }
            match n.name.clone().as_str() {
                "import" => self.errors += 1,
                "include" => self.load_failure(id),
                "strip-space" => self.space_rule(id, sheet, true),
                "preserve-space" => self.space_rule(id, sheet, false),
                "output" => self.output(id, sheet),
                "key" => self.key(id, sheet),
                "decimal-format" => self.decimal_format(id, sheet),
                "attribute-set" => self.attribute_set(id, &mut sets),
                "variable" | "param" => self.global(id, sheet),
                "template" => self.template(id, sheet),
                "namespace-alias" => self.namespace_alias(id, sheet),
                _ => {
                    if !self.forwards {
                        self.errors += 1;
                    }
                }
            }
        }
        self.resolve_sets(sets, sheet);
    }

    /// xsl:import / xsl:include: `xsltLoadStyleDocument` of the href, which fails.
    fn load_failure(&mut self, id: NodeId) {
        if self.attr(id, "href").is_some() {
            self.diag.push("line 1: Document is empty\n\n^");
        }
        self.errors += 1;
    }

    fn space_rule(&mut self, id: NodeId, sheet: &mut Sheet, strip: bool) {
        sheet.has_space_rules = true;
        let Some(v) = self.attr(id, "elements") else { return };
        for e in v.split([' ', '\t', '\n', '\r']).filter(|e| !e.is_empty()) {
            if e == "*" {
                sheet.strip_all = if strip { 1 } else { -1 };
                continue;
            }
            let Some((uri, local)) = self.qname(id, e) else { continue };
            sheet.space.entry((local, uri)).or_insert(strip);
        }
    }

    fn yes_no(&mut self, id: NodeId, name: &str, field: &mut i32) {
        match self.attr(id, name).as_deref() {
            None => {}
            Some("yes") => *field = 1,
            Some("no") => *field = 0,
            Some(_) => self.errors += 1,
        }
    }

    fn output(&mut self, id: NodeId, sheet: &mut Sheet) {
        let o = &mut sheet.output;
        for (name, field) in [
            ("version", &mut o.version),
            ("encoding", &mut o.encoding),
            ("doctype-system", &mut o.doctype_system),
            ("doctype-public", &mut o.doctype_public),
            ("media-type", &mut o.media_type),
        ] {
            if let Some(v) = attr(&self.doc, id, name) {
                *field = Some(v);
            }
        }
        if let Some(m) = self.attr(id, "method") {
            o.method = None;
            o.method_uri = None;
            match self.qname(id, &m) {
                None => {}
                Some((None, l)) if matches!(l.as_str(), "xml" | "html" | "text") => o.method = Some(l),
                Some((None, _)) => {}
                Some((uri, l)) => {
                    o.method = Some(l);
                    o.method_uri = uri;
                }
            }
        }
        let mut f = (o.standalone, o.indent, o.omit_xml_declaration);
        self.yes_no(id, "standalone", &mut f.0);
        self.yes_no(id, "indent", &mut f.1);
        self.yes_no(id, "omit-xml-declaration", &mut f.2);
        let o = &mut sheet.output;
        (o.standalone, o.indent, o.omit_xml_declaration) = f;
        if let Some(v) = self.attr(id, "cdata-section-elements") {
            for e in v.split([' ', '\t', '\n', '\r']).filter(|e| !e.is_empty()) {
                let name = match e.split_once(':') {
                    Some(_) => self.qname(id, e),
                    // An unprefixed name is in the default namespace here.
                    None => Some((search_ns(&self.doc, id, None).and_then(|ns| ns.href.clone()), e.to_string())),
                };
                if let Some(n) = name {
                    sheet.output.cdata_elements.push(n);
                }
            }
        }
    }

    fn key(&mut self, id: NodeId, sheet: &mut Sheet) {
        let (Some(name), Some(m), Some(u)) = (self.attr(id, "name"), self.attr(id, "match"), self.attr(id, "use"))
        else {
            self.errors += 1;
            return;
        };
        let Some(name) = self.qname(id, &name) else { return };
        let Ok(pattern) = pattern::compile(self, id, &m) else { return };
        let Some(use_) = self.xpath_novar(id, &u) else { return };
        sheet.keys.push(KeyDef { name, pattern, use_ });
    }

    fn decimal_format(&mut self, id: NodeId, sheet: &mut Sheet) {
        let name = match self.attr(id, "name") {
            Some(n) => match self.qname(id, &n) {
                Some(q) => Some(q),
                None => return,
            },
            None => None,
        };
        if sheet.decimal_formats.iter().any(|d| d.0 == name) {
            return;
        }
        let mut df = super::number::DecimalFormat::default();
        for (a, field) in [
            ("decimal-separator", &mut df.decimal_point),
            ("grouping-separator", &mut df.grouping),
            ("infinity", &mut df.infinity),
            ("minus-sign", &mut df.minus_sign),
            ("NaN", &mut df.nan),
            ("percent", &mut df.percent),
            ("per-mille", &mut df.permille),
            ("zero-digit", &mut df.zero_digit),
            ("digit", &mut df.digit),
            ("pattern-separator", &mut df.pattern_separator),
        ] {
            if let Some(v) = attr(&self.doc, id, a) {
                *field = v;
            }
        }
        sheet.decimal_formats.push((name, df));
    }

    fn use_sets(&mut self, id: NodeId, ns_attr: bool) -> Vec<QName> {
        let v = if ns_attr {
            super::compile::ns_attr(&self.doc, id, "use-attribute-sets")
        } else {
            self.attr(id, "use-attribute-sets")
        };
        let Some(v) = v else { return Vec::new() };
        v.split([' ', '\t', '\n', '\r'])
            .filter(|e| !e.is_empty())
            .filter_map(|e| self.qname(id, e))
            .collect()
    }

    fn attribute_set(&mut self, id: NodeId, sets: &mut Vec<RawSet>) {
        let Some(name) = self.attr(id, "name").filter(|n| !n.is_empty()) else { return };
        if !super::run::valid_qname(&name) {
            self.errors += 1;
            return;
        }
        let Some(name) = self.qname(id, &name) else { return };
        let i = match sets.iter().position(|s| s.name == name) {
            Some(i) => i,
            None => {
                sets.push(RawSet { name, attrs: Vec::new(), uses: Vec::new() });
                sets.len() - 1
            }
        };
        for c in self.doc.children(id).collect::<Vec<_>>() {
            if is_xslt(&self.doc, c) && self.doc.nodes[c].name == "attribute" {
                let key = self.static_attr_name(c);
                if let Some(ins) = self.instruction(c) {
                    sets[i].attrs.push((key, ins));
                }
            }
        }
        let uses = self.use_sets(id, false);
        sets[i].uses.extend(uses);
    }

    /// The (name, namespace) libxslt compares when merging sets: the static ones, else None.
    fn static_attr_name(&mut self, c: NodeId) -> (Option<String>, Option<String>) {
        let name = self.attr(c, "name").filter(|n| !n.contains('{'));
        let ns = self.attr(c, "namespace");
        let Some(name) = name else { return (None, None) };
        let (prefix, local) = match name.split_once(':') {
            Some((p, l)) => (Some(p.to_string()), l.to_string()),
            None => (None, name),
        };
        let uri = match ns {
            Some(n) if !n.contains('{') => (!n.is_empty()).then_some(n),
            Some(_) => None,
            None => prefix.and_then(|p| search_ns(&self.doc, c, Some(&p))).and_then(|n| n.href.clone()),
        };
        (Some(local), uri)
    }

    /// `xsltResolveAttrSet`: merges used sets in, depth first; a cycle is an error.
    fn resolve_sets(&mut self, sets: Vec<RawSet>, sheet: &mut Sheet) {
        let mut state = vec![0u8; sets.len()];
        let mut done: Vec<Vec<(AttrKey, Instr)>> = vec![Vec::new(); sets.len()];
        for i in 0..sets.len() {
            self.resolve_set(i, &sets, &mut state, &mut done, 1);
        }
        for (s, attrs) in sets.into_iter().zip(done) {
            sheet.attr_sets.insert(s.name, attrs.into_iter().map(|a| a.1).collect());
        }
    }

    fn resolve_set(&mut self, i: usize, sets: &[RawSet], state: &mut [u8], done: &mut [Vec<(AttrKey, Instr)>], depth: usize) {
        if state[i] == 2 {
            return;
        }
        if state[i] == 1 {
            self.errors += 1;
            state[i] = 2;
            return;
        }
        if depth > 100 {
            self.errors += 1;
            return;
        }
        state[i] = 1;
        let mut attrs = sets[i].attrs.clone();
        for u in &sets[i].uses {
            let Some(j) = sets.iter().position(|s| &s.name == u) else { continue };
            self.resolve_set(j, sets, state, done, depth + 1);
            for a in done[j].clone() {
                if !attrs.iter().any(|b| b.0 == a.0) {
                    attrs.push(a);
                }
            }
        }
        done[i] = attrs;
        state[i] = 2;
    }

    fn global(&mut self, id: NodeId, sheet: &mut Sheet) {
        let is_param = self.doc.nodes[id].name == "param";
        let Some(decl) = self.var_decl(id) else { return };
        if sheet.globals.iter().any(|g| g.decl.name == decl.name) {
            self.errors += 1;
            return;
        }
        sheet.globals.push(Global { decl, is_param });
    }

    fn namespace_alias(&mut self, id: NodeId, sheet: &mut Sheet) {
        let (Some(sp), Some(rp)) = (self.attr(id, "stylesheet-prefix"), self.attr(id, "result-prefix")) else {
            self.errors += 1;
            return;
        };
        let lookup = |c: &Self, p: &str| search_ns(&c.doc, id, (p != "#default").then_some(p));
        let Some(s) = lookup(self, &sp).and_then(|n| n.href.clone()) else {
            self.errors += 1;
            return;
        };
        let r = match lookup(self, &rp) {
            Some(n) => n.href.clone(),
            None if rp == "#default" => None,
            None => {
                self.errors += 1;
                return;
            }
        };
        sheet.aliases.insert(s, r);
    }

    fn template(&mut self, id: NodeId, sheet: &mut Sheet) {
        let inherited = self.inherited_ns(id);
        let mode = match self.attr(id, "mode") {
            Some(m) => match self.qname(id, &m) {
                Some(q) => Some(q),
                None => return,
            },
            None => None,
        };
        let pattern = match self.attr(id, "match") {
            Some(m) => match pattern::compile(self, id, &m) {
                Ok(p) => Some(p),
                Err(()) => return,
            },
            None => None,
        };
        let priority = self.attr(id, "priority").map(|p| xpath::string_to_number(&p) as f32);
        let name = match self.attr(id, "name") {
            Some(n) => match self.qname(id, &n) {
                Some(q) if valid_ncname(&q.1) => Some(q),
                Some(_) => {
                    self.errors += 1;
                    return;
                }
                None => return,
            },
            None => None,
        };
        if pattern.is_none() && name.is_none() {
            self.errors += 1;
            return;
        }
        let (params, body) = self.template_body(id);
        let idx = sheet.templates.len();
        if let Some(n) = &name {
            if sheet.named.contains_key(n) {
                self.errors += 1;
            } else {
                sheet.named.insert(n.clone(), idx);
            }
        }
        sheet.templates.push(Template { pattern, mode, priority, params, body: Rc::new(body), inherited_ns: inherited });
    }

    /// `xsltGetInheritedNsList`.
    fn inherited_ns(&self, id: NodeId) -> Vec<NsRef> {
        ns_list(&self.doc, id)
            .into_iter()
            .filter(|ns| {
                let h = ns.href.as_deref();
                h != Some(XSLT_NS)
                    && !(ns.prefix.is_some() && self.is_extension(h))
                    && !self.excluded.iter().any(|e| Some(e.as_str()) == h)
            })
            .collect()
    }
}

type AttrKey = (Option<String>, Option<String>);

/// An xsl:attribute-set as declared (definitions of one name together).
struct RawSet {
    name: QName,
    attrs: Vec<(AttrKey, Instr)>,
    uses: Vec<QName>,
}

/// `xmlValidateNCName`, near enough: XML name characters without a colon.
pub(super) fn valid_ncname(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_alphabetic() || c == '_')
        && cs.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{B7}'))
}
