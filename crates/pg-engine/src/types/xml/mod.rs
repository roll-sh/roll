//! The xml type (utils/adt/xml.c): values are stored as written, and checked, serialized
//! and queried with a port of the libxml2 2.13 behaviour Postgres builds on.

pub mod chvalid;
pub mod parser;
pub mod save;
pub mod tree;
pub mod xpath;

use crate::error::{Error, Result, Severity, code};
use parser::{Code, Domain, Level, Options, Parser, XmlError};
use tree::{Doc, NodeId};

/// `XmlOptionType`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmlOption {
    Document,
    Content,
}

/// `PgXmlStrictness`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    WellFormed,
    All,
}

/// libxml error codes `parse_xml_decl` returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclError {
    InvalidChar,
    SpaceRequired,
    StandaloneValue,
    VersionMissing,
    MissingEncoding,
    XmlDeclNotFinished,
}

impl DeclError {
    /// `errdetail_for_xml_code`.
    pub fn detail(self) -> &'static str {
        match self {
            DeclError::InvalidChar => "Invalid character value.",
            DeclError::SpaceRequired => "Space required.",
            DeclError::StandaloneValue => "standalone accepts only 'yes' or 'no'.",
            DeclError::VersionMissing => "Malformed declaration: missing version.",
            DeclError::MissingEncoding => "Missing encoding in text declaration.",
            DeclError::XmlDeclNotFinished => "Parsing XML declaration: '?>' expected.",
        }
    }
}

/// What `parse_xml_decl` found.
#[derive(Clone, Debug, Default)]
pub struct XmlDecl {
    pub len: usize,
    pub version: Option<String>,
    pub encoding: Option<String>,
    pub standalone: i32,
}

fn is_xml_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// `PG_XMLISNAMECHAR` for the character after "<?xml".
fn is_name_char(s: &str) -> bool {
    match s.chars().next() {
        Some(c) if c.is_ascii() => c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'),
        Some(c) => c.is_alphanumeric() || c == '\u{b7}',
        None => false,
    }
}

/// `parse_xml_decl`.
pub fn parse_xml_decl(s: &str) -> std::result::Result<XmlDecl, DeclError> {
    let mut d = XmlDecl {
        standalone: -1,
        ..Default::default()
    };
    parse_decl_into(s, &mut d)?;
    Ok(d)
}

/// `parse_xml_decl`'s body: on failure `d` keeps what was parsed so far.
fn parse_decl_into(s: &str, d: &mut XmlDecl) -> std::result::Result<(), DeclError> {
    let b = s.as_bytes();
    let at = |p: usize| b.get(p).copied().unwrap_or(0);
    let mut p = 0;
    let finished = |p: usize, d: &mut XmlDecl| {
        if b[..p].iter().any(|&c| c > 127) {
            return Err(DeclError::InvalidChar);
        }
        d.len = p;
        Ok(())
    };
    if !s.starts_with("<?xml") || is_name_char(&s[5..]) {
        return finished(0, d);
    }
    p += 5;
    let skip = |p: &mut usize| {
        while is_xml_space(at(*p)) {
            *p += 1;
        }
    };
    // version
    if !is_xml_space(at(p)) {
        return Err(DeclError::SpaceRequired);
    }
    skip(&mut p);
    if !b[p..].starts_with(b"version") {
        return Err(DeclError::VersionMissing);
    }
    p += 7;
    skip(&mut p);
    if at(p) != b'=' {
        return Err(DeclError::VersionMissing);
    }
    p += 1;
    skip(&mut p);
    let q = at(p);
    if q == b'\'' || q == b'"' {
        let Some(e) = b[p + 1..].iter().position(|&c| c == q) else {
            return Err(DeclError::VersionMissing);
        };
        d.version = Some(String::from_utf8_lossy(&b[p + 1..p + 1 + e]).into_owned());
        p += e + 2;
    } else {
        return Err(DeclError::VersionMissing);
    }
    // encoding
    let save = p;
    skip(&mut p);
    if b[p..].starts_with(b"encoding") {
        if !is_xml_space(at(save)) {
            return Err(DeclError::SpaceRequired);
        }
        p += 8;
        skip(&mut p);
        if at(p) != b'=' {
            return Err(DeclError::MissingEncoding);
        }
        p += 1;
        skip(&mut p);
        let q = at(p);
        if q == b'\'' || q == b'"' {
            let Some(e) = b[p + 1..].iter().position(|&c| c == q) else {
                return Err(DeclError::MissingEncoding);
            };
            d.encoding = Some(String::from_utf8_lossy(&b[p + 1..p + 1 + e]).into_owned());
            p += e + 2;
        } else {
            return Err(DeclError::MissingEncoding);
        }
    } else {
        p = save;
    }
    // standalone
    let save = p;
    skip(&mut p);
    if b[p..].starts_with(b"standalone") {
        if !is_xml_space(at(save)) {
            return Err(DeclError::SpaceRequired);
        }
        p += 10;
        skip(&mut p);
        if at(p) != b'=' {
            return Err(DeclError::StandaloneValue);
        }
        p += 1;
        skip(&mut p);
        if b[p..].starts_with(b"'yes'") || b[p..].starts_with(b"\"yes\"") {
            d.standalone = 1;
            p += 5;
        } else if b[p..].starts_with(b"'no'") || b[p..].starts_with(b"\"no\"") {
            d.standalone = 0;
            p += 4;
        } else {
            return Err(DeclError::StandaloneValue);
        }
    } else {
        p = save;
    }
    skip(&mut p);
    if !b[p..].starts_with(b"?>") {
        return Err(DeclError::XmlDeclNotFinished);
    }
    p += 2;
    finished(p, d)
}

/// `print_xml_decl` (the target encoding is always UTF-8 here).
pub fn print_xml_decl(out: &mut String, version: Option<&str>, standalone: i32) -> bool {
    if version.is_some_and(|v| v != "1.0") || standalone != -1 {
        out.push_str("<?xml");
        out.push_str(&format!(" version=\"{}\"", version.unwrap_or("1.0")));
        if standalone == 1 {
            out.push_str(" standalone=\"yes\"");
        } else if standalone == 0 {
            out.push_str(" standalone=\"no\"");
        }
        out.push_str("?>");
        true
    } else {
        false
    }
}

/// `xml_out_internal`: the stored text with its declaration normalized.
pub fn xml_out(s: &str) -> String {
    match parse_xml_decl(s) {
        Ok(d) => {
            let mut out = String::new();
            let mut len = d.len;
            if !print_xml_decl(&mut out, d.version.as_deref(), d.standalone)
                && s.as_bytes().get(len) == Some(&b'\n')
            {
                len += 1;
            }
            out.push_str(&s[len..]);
            out
        }
        Err(e) => {
            let mut w = Error::new(
                code::DATA_CORRUPTED,
                "could not parse XML declaration in stored value",
            )
            .detail(e.detail());
            w.severity = Severity::Warning;
            crate::types::tsearch::queue(w);
            s.to_string()
        }
    }
}

/// `xml_doctype_in_content`.
fn doctype_in_content(s: &str) -> bool {
    let b = s.as_bytes();
    let mut p = 0;
    loop {
        while p < b.len() && is_xml_space(b[p]) {
            p += 1;
        }
        if b.get(p) != Some(&b'<') {
            return false;
        }
        p += 1;
        if b.get(p) == Some(&b'!') {
            p += 1;
            if b[p..].starts_with(b"DOCTYPE") {
                return true;
            }
            if !b[p..].starts_with(b"--") {
                return false;
            }
            match s[p + 2..].find("--") {
                Some(e) => {
                    p = p + 2 + e;
                    if b.get(p + 2) != Some(&b'>') {
                        return false;
                    }
                    p += 3;
                }
                None => return false,
            }
            continue;
        }
        if b.get(p) != Some(&b'?') {
            return false;
        }
        p += 1;
        match s[p..].find("?>") {
            Some(e) => p += e + 2,
            None => return false,
        }
    }
}

/// The libxml errors as Postgres's handler reports them: queued WARNINGs, and the
/// accumulated detail of errors (None if nothing counted as an error).
pub fn report(errors: &[XmlError], strictness: Strictness) -> Option<String> {
    let mut buf = String::new();
    let mut occurred = false;
    for e in errors {
        let accept = match e.domain {
            Domain::Parser | Domain::Io => {
                !(e.code == Code::NotWellBalanced && occurred)
                    && e.code != Code::UndeclaredEntityWarning
            }
            _ => strictness == Strictness::All,
        };
        if !accept {
            continue;
        }
        let mut s = String::new();
        if e.line > 0 {
            s.push_str(&format!("line {}: ", e.line));
        }
        s.push_str(&e.message);
        if let Some(c) = &e.context {
            line_separator(&mut s);
            s.push_str(c);
        }
        while s.ends_with('\n') {
            s.pop();
        }
        if e.level >= Level::Error {
            line_separator(&mut buf);
            buf.push_str(&s);
            occurred = true;
        } else {
            let mut w = Error::new(code::WARNING, s);
            w.severity = Severity::Warning;
            crate::types::tsearch::queue(w);
        }
    }
    occurred.then_some(buf)
}

/// `appendStringInfoLineSeparator`.
fn line_separator(s: &mut String) {
    while s.ends_with('\n') {
        s.pop();
    }
    if !s.is_empty() {
        s.push('\n');
    }
}

fn xml_error(c: &'static str, msg: &str, detail: Option<String>) -> Error {
    let e = Error::new(c, msg);
    match detail {
        Some(d) if !d.is_empty() => e.detail(d),
        _ => e,
    }
}

/// A parsed value: the document and, for content, the top-level nodes.
pub struct Parsed {
    pub doc: Doc,
    pub option: XmlOption,
    pub nodes: Vec<NodeId>,
}

/// `xml_parse`.
pub fn xml_parse(s: &str, option: XmlOption, preserve_whitespace: bool) -> Result<Parsed> {
    xml_parse_bytes(s.as_bytes(), option, preserve_whitespace, true)
}

/// `xml_parse` of bytes that should be UTF-8 (libxml reports where they aren't); without
/// `tree`, only checked (the document's content isn't built).
fn xml_parse_bytes(
    b: &[u8],
    option: XmlOption,
    preserve_whitespace: bool,
    tree: bool,
) -> Result<Parsed> {
    let s = &*String::from_utf8_lossy(b);
    let mut as_document = option == XmlOption::Document;
    let mut count = 0;
    let mut decl = XmlDecl {
        standalone: -1,
        ..Default::default()
    };
    if !as_document {
        decl = parse_xml_decl(s).map_err(|e| {
            xml_error(
                code::INVALID_XML_CONTENT,
                "invalid XML content: invalid XML declaration",
                Some(e.detail().into()),
            )
        })?;
        count = decl.len;
        if doctype_in_content(&s[count..]) {
            as_document = true;
        }
    }
    if as_document {
        let opts = Options {
            replace_entities: true,
            keep_blanks: preserve_whitespace,
            dtd_attr: true,
        };
        let mut p = Parser::new(Doc::new(None), opts);
        p.tree = tree;
        let ok = p.parse_document(b);
        let detail = report(&p.errors, Strictness::WellFormed);
        if !ok || detail.is_some() {
            return Err(if option == XmlOption::Document {
                xml_error(code::INVALID_XML_DOCUMENT, "invalid XML document", detail)
            } else {
                xml_error(code::INVALID_XML_CONTENT, "invalid XML content", detail)
            });
        }
        let mut doc = p.doc;
        // xmlCtxtReadDoc is told the input is UTF-8, which is what the document records.
        doc.encoding = Some("UTF-8".into());
        return Ok(Parsed {
            doc,
            option: XmlOption::Document,
            nodes: Vec::new(),
        });
    }
    let mut doc = Doc::new(Some(decl.version.as_deref().unwrap_or("1.0")));
    doc.encoding = Some("UTF-8".into());
    doc.standalone = decl.standalone;
    let rest = &b[count..];
    if rest.is_empty() {
        return Ok(Parsed {
            doc,
            option: XmlOption::Content,
            nodes: Vec::new(),
        });
    }
    let opts = Options {
        replace_entities: false,
        keep_blanks: preserve_whitespace,
        dtd_attr: false,
    };
    let mut p = Parser::new(doc, opts);
    p.tree = tree;
    let nodes = p.parse_balanced_chunk(rest);
    let detail = report(&p.errors, Strictness::WellFormed);
    if !p.well_formed || detail.is_some() {
        return Err(xml_error(
            code::INVALID_XML_CONTENT,
            "invalid XML content",
            detail,
        ));
    }
    Ok(Parsed {
        doc: p.doc,
        option: XmlOption::Content,
        nodes,
    })
}

/// `xml_in` / `xmlparse`: check the text, keep it as written. Nothing is built: the errors
/// are the parse's, whatever the tree would have been.
pub fn validate(s: &str, option: XmlOption, preserve_whitespace: bool) -> Result<()> {
    xml_parse_bytes(s.as_bytes(), option, preserve_whitespace, false).map(|_| ())
}

/// `xml_recv`: binary input is in the encoding its XML declaration names (UTF-8 without
/// one, unlike text input), converted from it, checked, and kept with the declaration.
pub fn xml_recv(b: &[u8], option: XmlOption) -> Result<String> {
    // The declaration is ASCII; parse_xml_decl only looks at it.
    let head = String::from_utf8_lossy(&b[..b.len().min(1024)]);
    let mut decl = XmlDecl::default();
    let _ = parse_decl_into(&head, &mut decl);
    let enc = match &decl.encoding {
        Some(name) => crate::types::encnames::encoding_id(name).ok_or_else(|| {
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid encoding name \"{name}\""),
            )
        })?,
        None => 6,
    };
    // UTF-8 goes to libxml unchecked; other encodings are converted first.
    if enc == 6 {
        xml_parse_bytes(b, option, true, false)?;
        return Ok(String::from_utf8_lossy(b).into_owned());
    }
    let s = crate::types::encnames::to_utf8(b, enc)?;
    validate(&s, option, true)?;
    Ok(s)
}

fn is_valid_xml_namefirst(c: u32) -> bool {
    chvalid::is_base_char(c) || chvalid::is_ideographic(c) || c == '_' as u32 || c == ':' as u32
}

fn is_valid_xml_namechar(c: u32) -> bool {
    chvalid::is_base_char(c)
        || chvalid::is_ideographic(c)
        || chvalid::is_digit(c)
        || matches!(char::from_u32(c), Some('.' | '-' | '_' | ':'))
        || chvalid::is_combining(c)
        || chvalid::is_extender(c)
}

/// `map_sql_identifier_to_xml_name` (SQL/XML:2008 9.1).
pub fn map_sql_identifier_to_xml_name(
    ident: &str,
    fully_escaped: bool,
    escape_period: bool,
) -> String {
    let mut out = String::new();
    for (i, c) in ident.char_indices() {
        let rest = &ident[i..];
        if c == ':' && (i == 0 || fully_escaped) {
            out.push_str("_x003A_");
        } else if c == '_' && rest[1..].starts_with('x') {
            out.push_str("_x005F_");
        } else if fully_escaped
            && i == 0
            && rest.len() >= 3
            && rest.as_bytes()[..3].eq_ignore_ascii_case(b"xml")
        {
            out.push_str(if c == 'x' { "_x0078_" } else { "_x0058_" });
        } else if escape_period && c == '.' {
            out.push_str("_x002E_");
        } else if if i == 0 {
            !is_valid_xml_namefirst(c as u32)
        } else {
            !is_valid_xml_namechar(c as u32)
        } {
            out.push_str(&format!("_x{:04X}_", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `map_xml_name_to_sql_identifier` (SQL/XML:2008 9.3).
pub fn map_xml_name_to_sql_identifier(name: &str) -> String {
    let b = name.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_'
            && b.get(i + 1) == Some(&b'x')
            && b.len() >= i + 7
            && b[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit)
            && b[i + 6] == b'_'
        {
            let u = u32::from_str_radix(&name[i + 2..i + 6], 16).unwrap_or(0);
            if let Some(c) = char::from_u32(u).filter(|&c| c != '\0') {
                out.push(c);
            }
            i += 7;
        } else {
            let c = name[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// `escape_xml`.
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#x0d;"),
            c => out.push(c),
        }
    }
    out
}

/// `xmlEncodeSpecialChars`.
pub fn encode_special_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// `xmlSerializeHexCharRef`.
pub fn hex_char_ref(out: &mut String, c: u32) {
    out.push_str(&format!("&#x{c:X};"));
}

/// `xmlBufAttrSerializeTxtContent` for a document without an encoding: attribute values,
/// non-ASCII written as character references.
pub fn attr_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c if !c.is_ascii() => {
                let v = c as u32;
                hex_char_ref(
                    out,
                    if matches!(v, 0xfffe | 0xffff) {
                        0xfffd
                    } else {
                        v
                    },
                )
            }
            c => out.push(c),
        }
    }
}

/// The text/declaration split `xmlconcat` and `xmlroot` do: on a malformed declaration
/// Postgres keeps what it had parsed and an empty rest.
fn split_decl(s: &str) -> (XmlDecl, &str) {
    let mut d = XmlDecl {
        standalone: -1,
        ..Default::default()
    };
    match parse_decl_into(s, &mut d) {
        Ok(()) => {
            let rest = &s[d.len..];
            (d, rest)
        }
        Err(_) => (d, ""),
    }
}

/// `xmlconcat`'s merging of the values' declarations.
struct DeclFold {
    standalone: i32,
    version: Option<String>,
    version_no_value: bool,
}

impl DeclFold {
    fn new() -> DeclFold {
        DeclFold {
            standalone: 1,
            version: None,
            version_no_value: false,
        }
    }

    fn add(&mut self, d: XmlDecl) {
        if d.standalone == 0 && self.standalone == 1 {
            self.standalone = 0;
        }
        if d.standalone < 0 {
            self.standalone = -1;
        }
        match d.version {
            None => self.version_no_value = true,
            Some(ver) => match &self.version {
                None => self.version = Some(ver),
                Some(g) if *g != ver => self.version_no_value = true,
                _ => {}
            },
        }
    }

    /// The result's declaration.
    fn head(&self, out: &mut String) {
        if !self.version_no_value || self.standalone >= 0 {
            let version = if self.version_no_value {
                None
            } else {
                self.version.as_deref()
            };
            print_xml_decl(out, version, self.standalone);
        }
    }
}

/// `xmlconcat`.
pub fn concat(values: &[&str]) -> String {
    let mut fold = DeclFold::new();
    let mut buf = String::new();
    for v in values {
        let (d, rest) = split_decl(v);
        fold.add(d);
        buf.push_str(rest);
    }
    let mut out = String::new();
    fold.head(&mut out);
    if out.is_empty() {
        return buf;
    }
    out.push_str(&buf);
    out
}

/// `*acc = concat(&[acc, v])` (`xmlconcat2`, xmlagg's step), appending in place unless the
/// declaration changes: xmlagg over many rows is linear, not quadratic.
pub fn concat_into(acc: &mut String, v: &str) {
    let mut fold = DeclFold::new();
    let (d1, rest1) = split_decl(acc);
    let start = acc.len() - rest1.len();
    fold.add(d1);
    let (d2, rest2) = split_decl(v);
    fold.add(d2);
    let mut head = String::new();
    fold.head(&mut head);
    if acc[..start] != head {
        let body = acc.split_off(start);
        acc.clear();
        acc.push_str(&head);
        acc.push_str(&body);
    }
    acc.push_str(rest2);
}

/// `xmlroot`: `standalone` is the `XML_STANDALONE_*` value.
pub fn root(data: &str, version: Option<&str>, standalone: i32) -> String {
    let (d, rest) = split_decl(data);
    let sa = match standalone {
        0 => 1,
        1 => 0,
        2 => -1,
        _ => d.standalone,
    };
    let mut out = String::new();
    print_xml_decl(&mut out, version, sa);
    out.push_str(rest);
    out
}

/// `xmlpi`: None when the result is NULL.
pub fn pi(target: &str, arg: Option<Option<&str>>) -> Result<Option<String>> {
    if target.eq_ignore_ascii_case("xml") {
        return Err(Error::new(
            code::INVALID_XML_PROCESSING_INSTRUCTION,
            "invalid XML processing instruction",
        )
        .detail(format!(
            "XML processing instruction target name cannot be \"{target}\"."
        )));
    }
    let arg = match arg {
        Some(None) => return Ok(None),
        Some(Some(a)) => Some(a),
        None => None,
    };
    let mut out = format!("<?{target}");
    if let Some(a) = arg {
        if a.contains("?>") {
            return Err(Error::new(
                code::INVALID_XML_PROCESSING_INSTRUCTION,
                "invalid XML processing instruction",
            )
            .detail("XML processing instruction cannot contain \"?>\"."));
        }
        out.push(' ');
        out.push_str(a.trim_start_matches(' '));
    }
    out.push_str("?>");
    Ok(Some(out))
}

/// `xmlcomment`.
pub fn comment(s: &str) -> Result<String> {
    if s.contains("--") || s.ends_with('-') {
        return Err(Error::new(code::INVALID_XML_COMMENT, "invalid XML comment"));
    }
    Ok(format!("<!--{s}-->"))
}

/// `xml_is_document`.
pub fn is_document(s: &str) -> bool {
    validate(s, XmlOption::Document, true).is_ok()
}

/// `xmltotext_with_options` (XMLSERIALIZE).
pub fn to_text_with_options(s: &str, document: bool, indent: bool) -> Result<String> {
    if !document && !indent {
        return Ok(s.to_string());
    }
    let opt = if document {
        XmlOption::Document
    } else {
        XmlOption::Content
    };
    let not_doc = |_| Error::new(code::NOT_AN_XML_DOCUMENT, "not an XML document");
    if !indent {
        validate(s, opt, true).map_err(not_doc)?;
        return Ok(s.to_string());
    }
    let parsed = xml_parse(s, opt, false).map_err(not_doc)?;
    let decl_len = parse_xml_decl(s).map(|d| d.len).unwrap_or(0);
    let mut sv = save::Saver::new(&parsed.doc, true, decl_len == 0);
    if parsed.option == XmlOption::Document {
        sv.save_doc();
    } else {
        // Non-singly-rooted content: each node on its own, newlines between non-text nodes.
        for (i, &n) in parsed.nodes.iter().enumerate() {
            if parsed.doc.nodes[n].kind != tree::Kind::Text && i > 0 {
                sv.out.push('\n');
            }
            sv.save_tree(n);
        }
    }
    let mut out = sv.out;
    if document {
        while out.ends_with('\n') || out.ends_with('\r') {
            out.pop();
        }
    }
    Ok(out)
}
