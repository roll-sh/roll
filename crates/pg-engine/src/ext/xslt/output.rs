//! Serializing the result (libxslt's xsltSaveResultTo) and the result document's creation
//! and finishing touches (xsltApplyStylesheetInternal).

use super::style::Sheet;
use crate::types::xml::save::Saver;
use crate::types::xml::tree::{DOC, Decl, Doc, Kind, Node};

/// Whether xsl:output asks for HTML.
pub(super) fn html_result(sheet: &Sheet) -> bool {
    sheet.output.method.as_deref() == Some("html") && sheet.output.method_uri.is_none()
}

/// The result document: `xmlNewDoc(version)`, or the HTML one.
pub(super) fn new_result(sheet: &Sheet, html: bool) -> Doc {
    let o = &sheet.output;
    let mut d = if html {
        super::html::new_html_doc(o.doctype_public.as_deref(), o.doctype_system.as_deref(), o.version.as_deref())
    } else {
        Doc::new(Some(o.version.as_deref().unwrap_or("1.0")))
    };
    d.encoding = o.encoding.clone();
    d
}

/// The end of xsltApplyStylesheetInternal: with no method, a result whose root is `html`
/// becomes HTML (true); XML output gets its doctype, named after the root element.
pub(super) fn finish(sheet: &Sheet, out: &mut Doc, html: bool) -> bool {
    let o = &sheet.output;
    if html || o.method.as_deref() == Some("text") {
        return html;
    }
    let (public, system) = (o.doctype_public.as_deref(), o.doctype_system.as_deref());
    if o.method.is_none() && super::html::detect_html(out, public, system, o.version.as_deref()) {
        return true;
    }
    if public.is_none() && system.is_none() {
        return false;
    }
    let Some(root) = out.root_element() else { return false };
    let r = &out.nodes[root];
    let name = match r.ns.as_ref().and_then(|ns| ns.prefix.as_deref()) {
        Some(p) => format!("{p}:{}", r.name),
        None => r.name.to_string(),
    };
    let mut dtd = Node::new(Kind::Dtd, name);
    dtd.set_decl(Decl::Dtd { external_id: o.doctype_public.clone(), system_id: o.doctype_system.clone() });
    let id = out.add(dtd);
    out.prepend(DOC, id);
    out.int_subset = Some(id);
    false
}

/// `xsltSaveResultToString`: None where it fails (PostgreSQL returns NULL then).
pub(super) fn save(sheet: &Sheet, mut out: Doc, html: bool) -> crate::Result<Option<String>> {
    let only_dtd = out.first_child(DOC).is_some_and(|f| {
        out.next_sibling(f).is_none() && out.nodes[f].kind == Kind::Dtd
    });
    if !out.has_children(DOC) || only_dtd {
        return Ok(Some(String::new()));
    }
    let o = &sheet.output;
    if html {
        let b = super::html::save_html_bytes(&mut out, o.encoding.as_deref(), o.indent != 0, false);
        return text(b).map(Some);
    }
    if o.method.as_deref() == Some("text") {
        let mut s = String::new();
        text_content(&out, DOC, &mut s);
        return encoded(s, o.encoding.as_deref()).map(Some);
    }
    let mut s = String::new();
    if o.omit_xml_declaration != 1 {
        s.push_str("<?xml version=");
        s.push('"');
        s.push_str(out.version.as_deref().unwrap_or("1.0"));
        s.push('"');
        if let Some(e) = &o.encoding {
            s.push_str(&format!(" encoding=\"{e}\""));
        }
        match o.standalone {
            0 => s.push_str(" standalone=\"no\""),
            1 => s.push_str(" standalone=\"yes\""),
            _ => {}
        }
        s.push_str("?>\n");
    }
    for c in out.children(DOC) {
        let mut sv = Saver::new(&out, o.indent == 1, false);
        sv.node_dump_output(out.encoding.is_none());
        sv.save_tree(c);
        s.push_str(&sv.out);
        let k = out.nodes[c].kind;
        if o.indent != 0 && (k == Kind::Dtd || (k == Kind::Comment && out.next_sibling(c).is_some())) {
            s.push('\n');
        }
    }
    if o.indent != 0 {
        s.push('\n');
    }
    // An output encoding other than UTF-8: the bytes libxml's encoder writes (PostgreSQL
    // returns them unchecked; what isn't UTF-8 can't be text here and is replaced).
    encoded(s, o.encoding.as_deref()).map(Some)
}

/// The output in another encoding than UTF-8: libxml's encoder's bytes, which PostgreSQL
/// returns as text unchecked. Text here is UTF-8: bytes that aren't become U+FFFD (an
/// accepted difference; ASCII-compatible encodings of ASCII-only results are exact).
fn encoded(s: String, encoding: Option<&str>) -> crate::Result<String> {
    match encoding {
        Some(e) if !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8") => {
            text(super::html::encode_output(s, Some(e)))
        }
        _ => Ok(s),
    }
}

fn text(b: Vec<u8>) -> crate::Result<String> {
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The text method: text nodes only (not CDATA), in document order.
fn text_content(d: &Doc, id: usize, out: &mut String) {
    for c in d.children(id) {
        let n = &d.nodes[c];
        match n.kind {
            Kind::Text => out.push_str(d.text(n)),
            Kind::Element => text_content(d, c, out),
            _ => {}
        }
    }
}
