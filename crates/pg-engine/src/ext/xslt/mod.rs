//! A native XSLT 1.0 processor matching libxslt 1.1.43 (with libxml2 2.13), for xml2's
//! `xslt_process`: xslt_proc.c's steps, errors and parameter parsing, over the engine's
//! libxml model and XPath evaluator.
//!
//! Costs nothing unless xslt_process runs: no state outlives a call.

mod apply;
mod body;
mod build;
mod compile;
mod exec;
mod funcs;
mod instr;
mod numcount;
mod output;
mod pattern;
mod run;
mod style;
mod top;

mod html;
mod number;

use crate::Result;
use crate::error::{Error, code};
use crate::types::Value;
use crate::types::xml::parser::{Code, Domain, Options, Parser};
use crate::types::xml::tree::{DOC, Doc, Kind, NodeId};
use crate::types::xml::xpath::{self, XDoc, XNode};
use apply::Ctx;
use exec::{Exec, Global, Stop};
use style::{QName, Sheet};

/// The error detail xml2 collects (`err_buf` under PG_XML_STRICTNESS_LEGACY): every libxml
/// message of the call, parse warnings included, each on its own line.
#[derive(Default)]
pub(crate) struct Diag {
    buf: String,
}

impl Diag {
    pub fn push(&mut self, s: impl AsRef<str>) {
        let s = s.as_ref().trim_end_matches('\n');
        while self.buf.ends_with('\n') {
            self.buf.pop();
        }
        if !self.buf.is_empty() {
            self.buf.push('\n');
        }
        self.buf.push_str(s);
    }

    /// An XPath error as libxml reports it: its message.
    pub fn xpath_error(&mut self, e: &Error) {
        if let Some(d) = &e.detail {
            self.push(d);
        }
    }

    fn error(&self, c: &'static str, msg: &str) -> Error {
        let e = Error::new(c, msg);
        if self.buf.is_empty() { e } else { e.detail(self.buf.clone()) }
    }
}

/// The encoding an XML declaration names.
fn declared_encoding(s: &str) -> Option<&str> {
    let decl = &s[..s.strip_prefix("<?xml")?.find("?>")? + 5];
    let at = decl.find("encoding")? + 8;
    let rest = decl[at..].trim_start().strip_prefix('=')?.trim_start();
    let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let end = rest[1..].find(q)?;
    Some(&rest[1..1 + end])
}

/// The text libxml reads: with no encoding given, xmlReadMemory switches to the one the
/// declaration names, so the (UTF-8) bytes are read in it. Done for the encodings the
/// engine converts; others are read as they are.
fn input_text(s: &str) -> std::borrow::Cow<'_, str> {
    use crate::types::mbconv;
    let Some(e) = declared_encoding(s) else { return s.into() };
    let Some(id) = crate::types::encnames::encoding_id(e) else { return s.into() };
    if id == mbconv::UTF8 || id == mbconv::SQL_ASCII {
        return s.into();
    }
    match mbconv::to_utf8(s.as_bytes(), id) {
        Ok(t) => t.into(),
        Err(_) => s.into(),
    }
}

/// `xmlReadMemory(text, XML_PARSE_NOENT)`, its messages going to the detail.
fn read(s: &str, diag: &mut Diag) -> Option<Doc> {
    let opts = Options { replace_entities: true, keep_blanks: true, dtd_attr: false };
    let mut p = Parser::new(Doc::new(None), opts);
    let text = input_text(s);
    let ok = p.parse_document(text.as_bytes());
    for e in &p.errors {
        if matches!(e.domain, Domain::Parser | Domain::Io) && e.code == Code::UndeclaredEntityWarning {
            continue;
        }
        let mut m = String::new();
        if e.line > 0 {
            m.push_str(&format!("line {}: ", e.line));
        }
        m.push_str(&e.message);
        if let Some(c) = &e.context {
            while m.ends_with('\n') {
                m.pop();
            }
            m.push('\n');
            m.push_str(c);
        }
        diag.push(m);
    }
    (ok && p.well_formed).then_some(p.doc)
}

/// `parse_params`: `name=value` pairs separated by commas; a pair without `=` ends the list.
fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        match after.split_once(',') {
            Some((v, r)) => {
                out.push((name.to_string(), v.to_string()));
                rest = r;
            }
            None => {
                out.push((name.to_string(), after.to_string()));
                break;
            }
        }
    }
    out
}

/// `xslt_process(document, stylesheet[, params])`.
pub(crate) fn xslt_process(doc: &str, sheet: &str, params: Option<&str>) -> Result<Value> {
    let mut diag = Diag::default();
    let Some(src) = read(doc, &mut diag) else {
        return Err(diag.error(code::INVALID_XML_DOCUMENT, "error parsing XML document"));
    };
    let Some(ss) = read(sheet, &mut diag) else {
        return Err(diag.error(code::INVALID_XML_DOCUMENT, "error parsing stylesheet as XML document"));
    };
    let Some(sheet) = top::compile(ss, &mut diag) else {
        return Err(diag.error(code::INVALID_ARGUMENT_FOR_XQUERY, "failed to parse stylesheet"));
    };
    let params = parse_params(params.unwrap_or(""));
    let Some((result, html)) = transform(&sheet, src, &params, &mut diag) else {
        return Err(diag.error(code::INVALID_ARGUMENT_FOR_XQUERY, "failed to apply stylesheet"));
    };
    Ok(match output::save(&sheet, result, html)? {
        Some(s) => Value::Text(s.into()),
        None => Value::Null,
    })
}

/// `xsltApplyStripSpaces`: blank text children of elements xsl:strip-space names.
fn strip_spaces(sheet: &Sheet, doc: &mut Doc) {
    let Some(root) = doc.root_element() else { return };
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = &doc.nodes[id];
        if n.kind != Kind::Element {
            continue;
        }
        let uri = n.ns.as_ref().and_then(|ns| ns.href.clone());
        let rule = sheet.space.get(&(n.name.clone(), uri.clone())).copied().or_else(|| {
            uri.as_ref().and_then(|_| sheet.space.get(&("*".to_string(), uri.clone())).copied())
        });
        let strip = rule.unwrap_or(sheet.strip_all == 1);
        if strip {
            let kids: Vec<NodeId> = doc.nodes[id].children.clone();
            let keep: Vec<NodeId> = kids.into_iter().filter(|&c| !compile::is_blank_text(doc, c)).collect();
            doc.nodes[id].children = keep;
        }
        stack.extend(doc.nodes[id].children.iter().copied());
    }
}

/// The user parameters' names (`xsltProcessUserParamInternal`): `{uri}local`, or a QName
/// whose prefix the stylesheet's root element binds.
fn param_name(sheet: &Sheet, name: &str) -> Option<QName> {
    if let Some(rest) = name.strip_prefix('{') {
        let (u, l) = rest.split_once('}')?;
        return Some((Some(u.to_string()), l.to_string()));
    }
    match name.split_once(':') {
        Some((p, l)) => {
            let root = sheet.doc.root_element()?;
            let href = compile::search_ns(&sheet.doc, root, Some(p)).and_then(|ns| ns.href);
            Some((href, l.to_string()))
        }
        None => Some((None, name.to_string())),
    }
}

fn transform(sheet: &Sheet, mut src: Doc, params: &[(String, String)], diag: &mut Diag) -> Option<(Doc, bool)> {
    if sheet.output.method_uri.is_some() {
        return None;
    }
    if sheet.has_space_rules {
        strip_spaces(sheet, &mut src);
    }
    let html = output::html_result(sheet);
    let mut e = Exec::new(sheet, XDoc::new(src), diag);
    e.html = html;
    e.out = output::new_result(sheet, html);
    let root = Ctx { node: XNode::Node(DOC), pos: 1, size: 1 };
    for (n, v) in params {
        let Some(name) = param_name(sheet, n) else {
            e.failed = true;
            continue;
        };
        if e.user.iter().any(|(q, _)| *q == name) {
            e.failed = true;
            continue;
        }
        if sheet.globals.iter().any(|g| !g.is_param && g.decl.name == name) {
            continue;
        }
        let comp = match xpath::compile(v) {
            Ok(c) => c,
            Err(err) => {
                e.diag.xpath_error(&err);
                return None;
            }
        };
        let r = e.eval_raw(&comp, &xpath::NsMap::new(), &root.node, 1, 1);
        let v = e.checked(r).ok()?;
        e.user.push((name, v));
    }
    for i in 0..sheet.globals.len() {
        if eval_global(&mut e, i).is_err() {
            return None;
        }
    }
    if e.process_node(&root, &Vec::new()).is_err() || e.failed || e.host_failed.get() {
        return None;
    }
    let mut out = std::mem::replace(&mut e.out, Doc::new(None));
    let html = output::finish(sheet, &mut out, html);
    Some((out, html))
}

/// `xsltEvalGlobalVariable`, run up front; one that needs another global not computed yet
/// gives way to it and starts over.
fn eval_global(e: &mut Exec, i: usize) -> exec::R<()> {
    loop {
        match e.globals[i] {
            Global::Done(_) => return Ok(()),
            Global::Busy => return Err(Stop::Stopped),
            Global::Pending => {}
        }
        let decl = &e.s.globals[i];
        if decl.is_param
            && let Some((_, v)) = e.user.iter().find(|(q, _)| *q == decl.decl.name)
        {
            e.globals[i] = Global::Done(v.clone());
            return Ok(());
        }
        e.globals[i] = Global::Busy;
        let mark = e.diag.mark();
        let (old_base, old_len) = (e.base, e.vars.len());
        e.base = e.vars.len();
        let c = Ctx { node: XNode::Node(DOC), pos: 1, size: 1 };
        let d = e.s.globals[i].decl.clone();
        let r = e.var_value(&d, &c);
        e.vars.truncate(old_len);
        e.base = old_base;
        match r {
            Ok(v) => {
                e.globals[i] = Global::Done(v);
                return Ok(());
            }
            Err(Stop::NeedGlobal(j)) => {
                // (Still being computed while the one it needs is: a cycle back to it finds
                // it undefined, as libxslt's recursion check does.)
                e.diag.reset(mark);
                eval_global(e, j)?;
                e.globals[i] = Global::Pending;
            }
            Err(Stop::Stopped) => return Err(Stop::Stopped),
        }
    }
}

impl Diag {
    fn mark(&self) -> usize {
        self.buf.len()
    }

    fn reset(&mut self, mark: usize) {
        self.buf.truncate(mark);
    }
}
