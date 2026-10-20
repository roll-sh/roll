//! A port of the libxml2 2.13 XML parser (parser.c, parserInternals.c, SAX2.c), the subset
//! Postgres drives: documents and well-balanced chunks from memory, internal DTD subsets,
//! entity expansion, namespaces, and the error stream with libxml's exact messages,
//! line numbers and context lines, which Postgres copies into its error details.

use super::tree::*;
use std::collections::HashMap;
use std::rc::Rc;

/// `xmlErrorDomain`, as far as Postgres cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    Parser,
    Namespace,
    Dtd,
    Io,
}

/// `xmlErrorLevel`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Warning = 1,
    Error = 2,
    Fatal = 3,
}

/// The error codes callers look at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    Other,
    NotWellBalanced,
    UndeclaredEntityWarning,
    DocumentEmpty,
}

#[derive(Clone, Debug)]
pub struct XmlError {
    pub domain: Domain,
    pub code: Code,
    pub level: Level,
    pub line: i32,
    pub message: String,
    /// What `xmlParserPrintFileContext` printed: the line and the caret line.
    pub context: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    /// XML_PARSE_NOENT
    pub replace_entities: bool,
    /// Not XML_PARSE_NOBLANKS
    pub keep_blanks: bool,
    /// XML_PARSE_DTDATTR
    pub dtd_attr: bool,
}

struct Input {
    buf: Rc<Vec<u8>>,
    cur: usize,
    end: usize,
    line: i32,
    col: i32,
    entity: Option<usize>,
    id: i32,
    /// XML_INPUT_ENCODING_ERROR: its first invalid UTF-8 sequence was reported.
    enc_error: bool,
}

struct Tag {
    name: String,
    prefix: Option<String>,
    line: i32,
    ns_nr: usize,
}

struct DefAttr {
    name: String,
    prefix: Option<String>,
    value: String,
}

pub(crate) struct Parser {
    inputs: Vec<Input>,
    input_ids: i32,
    pub errors: Vec<XmlError>,
    pub well_formed: bool,
    disable_sax: u8,
    nb_errors: u32,
    nb_warnings: u32,
    pub doc: Doc,
    nodes: Vec<NodeId>,
    names: Vec<Tag>,
    spaces: Vec<i32>,
    ns: Vec<(Option<String>, String)>,
    min_ns: usize,
    in_subset: u8,
    has_external_subset: bool,
    has_perefs: bool,
    pub standalone: i32,
    pub version: Option<String>,
    pub encoding: Option<String>,
    opts: Options,
    atts_default: HashMap<(String, Option<String>), Vec<DefAttr>>,
    atts_special: HashMap<(String, String), AttrType>,
    load_subset: bool,
}

const MAX_NAME_LENGTH: usize = 50000;

fn is_blank(c: u8) -> bool {
    c == 0x20 || c == 0x09 || c == 0x0a || c == 0x0d
}

fn is_char(c: i32) -> bool {
    matches!(c, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}

fn is_byte_char(c: u8) -> bool {
    c >= 0x20 || c == 0x9 || c == 0xa || c == 0xd
}

fn is_pubid(c: u8) -> bool {
    matches!(c, b'\n' | b'\r' | b' ' | b'!' | b'#' | b'$' | b'%' | b'\''..=b';' | b'=' | b'?'..=b'Z' | b'_' | b'a'..=b'z')
}

/// The fast character-data table: printable ASCII except `&`, `<` and `]`, plus tab.
fn test_char_data(c: u8) -> bool {
    c == 0x09 || ((0x20..=0x7f).contains(&c) && c != b'&' && c != b'<' && c != b']')
}

fn name_start(c: i32) -> bool {
    c != b' ' as i32
        && c != b'>' as i32
        && c != b'/' as i32
        && (matches!(c, 0x61..=0x7a | 0x41..=0x5a)
            || c == b'_' as i32
            || c == b':' as i32
            || matches!(c, 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0x2ff | 0x370..=0x37d | 0x37f..=0x1fff | 0x200c..=0x200d | 0x2070..=0x218f | 0x2c00..=0x2fef | 0x3001..=0xd7ff | 0xf900..=0xfdcf | 0xfdf0..=0xfffd | 0x10000..=0xeffff))
}

fn name_char(c: i32) -> bool {
    c != b' ' as i32
        && c != b'>' as i32
        && c != b'/' as i32
        && (matches!(c, 0x61..=0x7a | 0x41..=0x5a | 0x30..=0x39)
            || c == b'_' as i32
            || c == b':' as i32
            || c == b'-' as i32
            || c == b'.' as i32
            || c == 0xb7
            || matches!(c, 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0x2ff | 0x300..=0x36f | 0x370..=0x37d | 0x37f..=0x1fff | 0x200c..=0x200d | 0x203f..=0x2040 | 0x2070..=0x218f | 0x2c00..=0x2fef | 0x3001..=0xd7ff | 0xf900..=0xfdcf | 0xfdf0..=0xfffd | 0x10000..=0xeffff))
}

fn push_char(s: &mut Vec<u8>, c: i32) {
    let ch = char::from_u32(c as u32).unwrap_or('\u{fffd}');
    let mut b = [0u8; 4];
    s.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
}

fn utf8_len(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `%.50s`: at most 50 bytes, cut back to a character boundary.
fn trunc50(b: &[u8]) -> String {
    let mut n = b.len().min(50);
    while n > 0 && n < b.len() && (b[n] & 0xc0) == 0x80 {
        n -= 1;
    }
    lossy(&b[..n])
}

/// `xmlErrString` for the codes `xmlFatalErr` raises.
#[derive(Clone, Copy)]
enum E {
    AttlistNotFinished,
    AttlistNotStarted,
    AttributeNotStarted,
    CdataNotFinished,
    DoctypeNotFinished,
    DocumentEmpty,
    DocumentEnd,
    ElemcontentNotFinished,
    ElemcontentNotStarted,
    EncodingName,
    EntityLoop,
    EntityPeInternal,
    EntityrefSemicolMissing,
    EqualRequired,
    GtRequired,
    HyphenInComment,
    IntSubsetNotFinished,
    InvalidCharref,
    InvalidDecCharref,
    InvalidHexCharref,
    LiteralNotFinished,
    LiteralNotStarted,
    LtInAttribute,
    LtslashRequired,
    MisplacedCdataEnd,
    MixedNotStarted,
    NameTooLong,
    NmtokenRequired,
    NotWellBalanced,
    NotationNotFinished,
    NotationNotStarted,
    PcdataRequired,
    PerefSemicolMissing,
    PiNotStarted,
    PubidRequired,
    ReservedXmlName,
    StandaloneValue,
    StringNotClosed,
    StringNotStarted,
    UriFragment,
    UriRequired,
    ValueRequired,
    VersionMissing,
    XmldeclNotFinished,
}

impl E {
    fn msg(self) -> &'static str {
        use E::*;
        match self {
            AttlistNotFinished => "')' required to finish ATTLIST enumeration",
            AttlistNotStarted => "'(' required to start ATTLIST enumeration",
            AttributeNotStarted => "AttValue: \" or ' expected",
            CdataNotFinished => "Unregistered error message",
            DoctypeNotFinished => "DOCTYPE improperly terminated",
            DocumentEmpty => "Document is empty",
            DocumentEnd => "Extra content at the end of the document",
            ElemcontentNotFinished => "ContentDecl : ',' '|' or ')' expected",
            ElemcontentNotStarted => "ContentDecl : Name or '(' expected",
            EncodingName => "Invalid XML encoding name",
            EntityLoop => "Detected an entity reference loop",
            EntityPeInternal => "PEReferences forbidden in internal subset",
            EntityrefSemicolMissing => "EntityRef: expecting ';'",
            EqualRequired => "expected '='",
            GtRequired => "expected '>'",
            HyphenInComment => "Comment must not contain '--' (double-hyphen)",
            IntSubsetNotFinished => "Content error in the internal subset",
            InvalidCharref => "CharRef: invalid value",
            InvalidDecCharref => "CharRef: invalid decimal value",
            InvalidHexCharref => "CharRef: invalid hexadecimal value",
            LiteralNotFinished => "Unfinished System or Public ID \" or ' expected",
            LiteralNotStarted => "SystemLiteral \" or ' expected",
            LtInAttribute => "Unescaped '<' not allowed in attributes values",
            LtslashRequired => "EndTag: '</' not found",
            MisplacedCdataEnd => "Sequence ']]>' not allowed in content",
            MixedNotStarted => "MixedContentDecl : '|' or ')*' expected",
            NameTooLong => "Name too long",
            NmtokenRequired => "NmToken expected in ATTLIST enumeration",
            NotWellBalanced => "chunk is not well balanced",
            NotationNotFinished => "'>' required to close NOTATION declaration",
            NotationNotStarted => "NOTATION: Name expected here",
            PcdataRequired => "MixedContentDecl : '#PCDATA' expected",
            PerefSemicolMissing => "PEReference: expecting ';'",
            PiNotStarted => "xmlParsePI : no target name",
            PubidRequired => "PUBLIC, the Public Identifier is missing",
            ReservedXmlName => "Invalid PI name",
            StandaloneValue => "standalone accepts only 'yes' or 'no'",
            StringNotClosed => "String not closed expecting \" or '",
            StringNotStarted => "String not started expecting ' or \"",
            UriFragment => "Fragment not allowed",
            UriRequired => "SYSTEM or PUBLIC, the URI is missing",
            ValueRequired => "Entity value required",
            VersionMissing => "Malformed declaration expecting version",
            XmldeclNotFinished => "parsing XML declaration: '?>' expected",
        }
    }
}

impl Parser {
    pub fn new(doc: Doc, opts: Options) -> Parser {
        Parser {
            inputs: Vec::new(),
            input_ids: 0,
            errors: Vec::new(),
            well_formed: true,
            disable_sax: 0,
            nb_errors: 0,
            nb_warnings: 0,
            doc,
            nodes: Vec::new(),
            names: Vec::new(),
            spaces: vec![-1],
            ns: Vec::new(),
            min_ns: 0,
            in_subset: 0,
            has_external_subset: false,
            has_perefs: false,
            standalone: -1,
            version: None,
            encoding: None,
            opts,
            atts_default: HashMap::new(),
            atts_special: HashMap::new(),
            load_subset: opts.dtd_attr,
        }
    }

    fn new_input(&mut self, text: &[u8], entity: Option<usize>) -> Input {
        self.input_ids += 1;
        Input {
            buf: Rc::new(text.to_vec()),
            cur: 0,
            end: text.len(),
            line: 1,
            col: 1,
            entity,
            id: self.input_ids,
            enc_error: false,
        }
    }

    // ── input access (the CUR/NXT/SKIP/NEXT macros) ──

    fn inp(&self) -> &Input {
        self.inputs.last().expect("input")
    }

    fn inp_mut(&mut self) -> &mut Input {
        self.inputs.last_mut().expect("input")
    }

    fn b(&self, n: usize) -> u8 {
        let i = self.inp();
        let p = i.cur + n;
        if p < i.end { i.buf[p] } else { 0 }
    }

    fn cur(&self) -> u8 {
        self.b(0)
    }

    fn at_end(&self) -> bool {
        let i = self.inp();
        i.cur >= i.end
    }

    fn starts(&self, s: &[u8]) -> bool {
        s.iter().enumerate().all(|(k, &c)| self.b(k) == c)
    }

    fn skip(&mut self, n: usize) {
        let i = self.inp_mut();
        i.cur += n;
        i.col += n as i32;
    }

    fn next1(&mut self) {
        self.skip(1);
    }

    /// `xmlNextChar`.
    fn next(&mut self) {
        let i = self.inp_mut();
        if i.cur >= i.end {
            return;
        }
        let c = i.buf[i.cur];
        if c < 0x80 {
            if c == b'\n' {
                i.cur += 1;
                i.line += 1;
                i.col = 1;
            } else if c == b'\r' {
                i.cur += if i.cur + 1 < i.end && i.buf[i.cur + 1] == b'\n' {
                    2
                } else {
                    1
                };
                i.line += 1;
                i.col = 1;
            } else {
                i.cur += 1;
                i.col += 1;
            }
        } else {
            i.col += 1;
            i.cur += utf8_len(c);
        }
    }

    /// `xmlCurrentChar`: the character here and its length; a CR LF pair reads as LF (and,
    /// as in libxml, steps over the CR).
    fn cur_char(&mut self) -> (i32, usize) {
        let i = self.inp_mut();
        if i.cur >= i.end {
            return (0, 0);
        }
        let c = i.buf[i.cur];
        if c < 0x80 {
            if c == b'\r' {
                if i.cur + 1 < i.end && i.buf[i.cur + 1] == b'\n' {
                    i.cur += 1;
                }
                return (b'\n' as i32, 1);
            }
            return (c as i32, 1);
        }
        let n = utf8_len(c);
        let avail = &i.buf[i.cur..(i.cur + n).min(i.end)];
        let decoded = std::str::from_utf8(avail);
        if let Some(ch) = decoded.ok().and_then(|s| s.chars().next()) {
            return (ch as i32, n);
        }
        // A sequence the input's end cuts short stops the parse; other invalid bytes read
        // as U+FFFD, the first one reported.
        let (cut, avail) = (
            avail.len() < n && decoded.is_err_and(|e| e.error_len().is_none()),
            avail.len(),
        );
        if cut {
            let msg = format!("Incomplete UTF-8 sequence starting with {c:02X}\n");
            self.raise(Domain::Io, Code::Other, Level::Fatal, msg);
            self.halt();
            return (0xfffd, avail);
        }
        if !std::mem::replace(&mut i.enc_error, true) {
            let msg = "Invalid bytes in character encoding\n".to_string();
            self.raise(Domain::Io, Code::Other, Level::Fatal, msg);
        }
        (0xfffd, 1)
    }

    /// `NEXTL(l)`.
    fn nextl(&mut self, l: usize) {
        let i = self.inp_mut();
        if i.cur < i.end && i.buf[i.cur] == b'\n' {
            i.line += 1;
            i.col = 1;
        } else {
            i.col += 1;
        }
        i.cur += l;
    }

    /// `xmlSkipBlankChars`.
    fn skip_blanks(&mut self) -> usize {
        let mut n = 0;
        loop {
            let c = self.cur();
            if !is_blank(c) || self.at_end() {
                break;
            }
            let i = self.inp_mut();
            if c == b'\n' {
                i.line += 1;
                i.col = 1;
            } else {
                i.col += 1;
            }
            i.cur += 1;
            n += 1;
        }
        n
    }

    fn in_pe(&self) -> bool {
        self.inp().entity.is_some_and(|e| {
            matches!(
                self.doc.entities[e].etype,
                EntityType::InternalParameter | EntityType::ExternalParameter
            )
        })
    }

    fn external(&self) -> bool {
        self.in_subset == 2
            || self
                .inp()
                .entity
                .is_some_and(|e| self.doc.entities[e].etype == EntityType::ExternalParameter)
    }

    fn stopped(&self) -> bool {
        self.disable_sax > 1
    }

    fn sax(&self) -> bool {
        self.disable_sax == 0
    }

    /// `xmlSkipBlankCharsPE`.
    fn skip_blanks_pe(&mut self) -> usize {
        let mut in_param = self.in_pe();
        let mut expand = self.external();
        if !in_param && !expand {
            return self.skip_blanks();
        }
        let mut res = 0;
        while !self.stopped() {
            let c = self.cur();
            if is_blank(c) && !self.at_end() {
                self.next();
            } else if c == b'%' {
                if !expand || is_blank(self.b(1)) || self.b(1) == 0 {
                    break;
                }
                self.parse_pe_reference();
                in_param = self.in_pe();
                expand = self.external();
            } else if self.at_end() {
                if !in_param {
                    break;
                }
                self.pop_pe();
                in_param = self.in_pe();
                expand = self.external();
            } else {
                break;
            }
            res += 1;
        }
        res
    }

    fn pop_pe(&mut self) {
        if let Some(e) = self.inp().entity {
            self.doc.entities[e].expanding = false;
        }
        if self.inputs.len() > 1 {
            self.inputs.pop();
        }
    }

    fn halt(&mut self) {
        self.disable_sax = 2;
    }

    // ── errors ──

    /// `xmlCtxtVErr`, recording what Postgres's handler would format.
    fn raise(&mut self, domain: Domain, code: Code, level: Level, msg: String) {
        if self.stopped() {
            return;
        }
        let mut record = true;
        if level == Level::Warning {
            if self.nb_warnings >= 100 {
                record = false;
            } else {
                self.nb_warnings += 1;
            }
        } else if self.nb_errors >= 100 && (level < Level::Fatal || !self.well_formed) {
            record = false;
        } else {
            self.nb_errors += 1;
        }
        if record {
            let line = if self.inputs.len() > 1 {
                self.inputs[self.inputs.len() - 2].line
            } else {
                self.inputs.last().map_or(0, |i| i.line)
            };
            let context = self.inputs.last().map(file_context);
            self.errors.push(XmlError {
                domain,
                code,
                level,
                line,
                message: msg,
                context,
            });
        }
        if level == Level::Fatal {
            self.well_formed = false;
            self.disable_sax = 1;
        }
    }

    fn fatal(&mut self, e: E) {
        self.raise(
            Domain::Parser,
            if matches!(e, E::NotWellBalanced) {
                Code::NotWellBalanced
            } else {
                Code::Other
            },
            Level::Fatal,
            format!("{}\n", e.msg()),
        );
    }

    fn fatal_info(&mut self, e: E, info: &str) {
        self.raise(
            Domain::Parser,
            Code::Other,
            Level::Fatal,
            format!("{}: {}\n", e.msg(), info),
        );
    }

    fn fatal_msg(&mut self, msg: impl Into<String>) {
        self.raise(Domain::Parser, Code::Other, Level::Fatal, msg.into());
    }

    fn warning(&mut self, msg: impl Into<String>) {
        self.raise(Domain::Parser, Code::Other, Level::Warning, msg.into());
    }

    fn ns_err(&mut self, msg: impl Into<String>) {
        self.raise(Domain::Namespace, Code::Other, Level::Error, msg.into());
    }

    fn ns_warn(&mut self, msg: impl Into<String>) {
        self.raise(Domain::Namespace, Code::Other, Level::Warning, msg.into());
    }

    // ── names ──

    /// `xmlParseName`.
    fn parse_name(&mut self) -> Option<String> {
        let start = self.inp().cur;
        let c = self.cur();
        if c.is_ascii_alphabetic() || c == b'_' || c == b':' {
            let mut k = 1;
            while matches!(self.b(k), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':' | b'.')
            {
                k += 1;
            }
            let nb = self.b(k);
            if nb > 0 && nb < 0x80 {
                if k > MAX_NAME_LENGTH {
                    self.fatal_info(E::NameTooLong, "Name");
                    return None;
                }
                let s = lossy(&self.inp().buf[start..start + k]);
                self.skip(k);
                return Some(s);
            }
        }
        self.parse_name_complex()
    }

    fn parse_name_complex(&mut self) -> Option<String> {
        let (mut c, mut l) = self.cur_char();
        if !name_start(c) {
            return None;
        }
        let mut len = l;
        self.nextl(l);
        loop {
            (c, l) = self.cur_char();
            if !name_char(c) {
                break;
            }
            len += l;
            self.nextl(l);
        }
        if len > MAX_NAME_LENGTH {
            self.fatal_info(E::NameTooLong, "Name");
            return None;
        }
        let i = self.inp();
        let end =
            if i.cur < i.end && i.buf[i.cur] == b'\n' && i.cur > 0 && i.buf[i.cur - 1] == b'\r' {
                i.cur - 1
            } else {
                i.cur
            };
        Some(lossy(&i.buf[end - len..end]))
    }

    /// `xmlParseNCName`.
    fn parse_ncname(&mut self) -> Option<String> {
        let start = self.inp().cur;
        let c = self.cur();
        if (c.is_ascii_alphabetic() || c == b'_') && !self.at_end() {
            let mut k = 1;
            let end = self.inp().end;
            while start + k < end
                && matches!(self.b(k), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.')
            {
                k += 1;
            }
            if start + k < end {
                let nb = self.b(k);
                if nb > 0 && nb < 0x80 {
                    if k > MAX_NAME_LENGTH {
                        self.fatal_info(E::NameTooLong, "NCName");
                        return None;
                    }
                    let s = lossy(&self.inp().buf[start..start + k]);
                    self.skip(k);
                    return Some(s);
                }
            }
        }
        let (mut c, mut l) = self.cur_char();
        if !name_start(c) || c == b':' as i32 {
            return None;
        }
        let start = self.inp().cur;
        let mut len = 0;
        while name_char(c) && c != b':' as i32 {
            len += l;
            self.nextl(l);
            (c, l) = self.cur_char();
        }
        if len > MAX_NAME_LENGTH {
            self.fatal_info(E::NameTooLong, "NCName");
            return None;
        }
        Some(lossy(&self.inp().buf[start..start + len]))
    }

    /// `xmlParseQNameHashed`: (local, prefix).
    fn parse_qname(&mut self) -> Option<(String, Option<String>)> {
        let start = self.inp().cur;
        let mut l = self.parse_ncname();
        let mut p = None;
        let mut is_ncname = false;
        if l.is_some() {
            is_ncname = true;
            if self.cur() == b':' {
                self.next();
                p = l;
                l = self.parse_ncname();
            }
        }
        if l.is_none() || self.cur() == b':' {
            if !is_ncname && self.cur() != b':' {
                return None;
            }
            self.parse_nmtoken();
            let i = self.inp();
            let name = lossy(&i.buf[start..i.cur]);
            self.ns_err(format!("Failed to parse QName '{name}'\n"));
            return Some((name, None));
        }
        Some((l.unwrap(), p))
    }

    /// `xmlParseNmtoken`.
    fn parse_nmtoken(&mut self) -> Option<String> {
        let mut buf = Vec::new();
        let (mut c, mut l) = self.cur_char();
        while name_char(c) {
            push_char(&mut buf, c);
            self.nextl(l);
            (c, l) = self.cur_char();
            if buf.len() > MAX_NAME_LENGTH {
                self.fatal_info(E::NameTooLong, "NmToken");
                return None;
            }
        }
        if buf.is_empty() {
            return None;
        }
        Some(lossy(&buf))
    }

    /// `xmlParseNameAndCompare`: Ok(()) on a match, else the name found.
    fn parse_name_and_compare(&mut self, other: &str) -> Result<(), Option<String>> {
        let o = other.as_bytes();
        let mut k = 0;
        while self.b(k) != 0 && k < o.len() && self.b(k) == o[k] {
            k += 1;
        }
        if k == o.len() && (self.b(k) == b'>' || is_blank(self.b(k))) {
            self.skip(k);
            return Ok(());
        }
        match self.parse_name() {
            Some(n) if n == other => Ok(()),
            n => Err(n),
        }
    }

    fn parse_qname_and_compare(
        &mut self,
        name: &str,
        prefix: Option<&str>,
    ) -> Result<(), Option<String>> {
        let Some(prefix) = prefix else {
            return self.parse_name_and_compare(name);
        };
        let pb = prefix.as_bytes();
        let mut k = 0;
        while self.b(k) != 0 && k < pb.len() && self.b(k) == pb[k] {
            k += 1;
        }
        if k == pb.len() && self.b(k) == b':' {
            k += 1;
            let nb = name.as_bytes();
            let mut j = 0;
            while self.b(k) != 0 && j < nb.len() && self.b(k) == nb[j] {
                k += 1;
                j += 1;
            }
            if j == nb.len() && (self.b(k) == b'>' || is_blank(self.b(k))) {
                self.skip(k);
                return Ok(());
            }
        }
        match self.parse_qname() {
            None => Err(None),
            Some((l, p)) if l == name && p.as_deref() == Some(prefix) => Ok(()),
            Some((l, _)) => Err(Some(l)),
        }
    }

    // ── references ──

    /// `xmlParseCharRef`: the value, 0 on error.
    fn parse_char_ref(&mut self) -> i32 {
        let mut val: i32 = 0;
        let mut count = 0;
        if self.cur() == b'&' && self.b(1) == b'#' && self.b(2) == b'x' {
            self.skip(3);
            while self.cur() != b';' && !self.stopped() {
                if count > 20 {
                    count = 0;
                }
                count += 1;
                let c = self.cur();
                if c.is_ascii_digit() {
                    val = val * 16 + (c - b'0') as i32;
                } else if (b'a'..=b'f').contains(&c) && count < 20 {
                    val = val * 16 + (c - b'a') as i32 + 10;
                } else if (b'A'..=b'F').contains(&c) && count < 20 {
                    val = val * 16 + (c - b'A') as i32 + 10;
                } else {
                    self.fatal(E::InvalidHexCharref);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                self.next();
                count += 1;
            }
            if self.cur() == b';' {
                self.skip(1);
            }
        } else if self.cur() == b'&' && self.b(1) == b'#' {
            self.skip(2);
            while self.cur() != b';' {
                if count > 20 {
                    count = 0;
                }
                count += 1;
                let c = self.cur();
                if c.is_ascii_digit() {
                    val = val * 10 + (c - b'0') as i32;
                } else {
                    self.fatal(E::InvalidDecCharref);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                self.next();
                count += 1;
            }
            if self.cur() == b';' {
                self.skip(1);
            }
        } else {
            if self.cur() == b'&' {
                self.skip(1);
            }
            self.fatal(E::InvalidCharref);
        }
        if val >= 0x110000 {
            self.fatal_msg("xmlParseCharRef: character reference out of bounds\n".to_string());
            val = 0xfffd;
        } else if !is_char(val) {
            self.fatal_msg(format!("xmlParseCharRef: invalid xmlChar value {val}\n"));
        }
        if is_char(val) { val } else { 0 }
    }

    /// `xmlParseStringCharRef` over `s[*p..]`.
    fn parse_string_char_ref(&mut self, s: &[u8], p: &mut usize) -> i32 {
        let at = |k: usize| s.get(k).copied().unwrap_or(0);
        let mut ptr = *p;
        let mut val: i32 = 0;
        if at(ptr) == b'&' && at(ptr + 1) == b'#' && at(ptr + 2) == b'x' {
            ptr += 3;
            while at(ptr) != b';' {
                let c = at(ptr);
                if c.is_ascii_hexdigit() {
                    val = val * 16 + (c as char).to_digit(16).unwrap() as i32;
                } else {
                    self.fatal(E::InvalidHexCharref);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                ptr += 1;
            }
            if at(ptr) == b';' {
                ptr += 1;
            }
        } else if at(ptr) == b'&' && at(ptr + 1) == b'#' {
            ptr += 2;
            while at(ptr) != b';' {
                let c = at(ptr);
                if c.is_ascii_digit() {
                    val = val * 10 + (c - b'0') as i32;
                } else {
                    self.fatal(E::InvalidDecCharref);
                    val = 0;
                    break;
                }
                if val > 0x110000 {
                    val = 0x110000;
                }
                ptr += 1;
            }
            if at(ptr) == b';' {
                ptr += 1;
            }
        } else {
            self.fatal(E::InvalidCharref);
            return 0;
        }
        *p = ptr;
        if val >= 0x110000 {
            self.fatal_msg("xmlParseStringCharRef: character reference out of bounds\n");
        } else if is_char(val) {
            return val;
        } else {
            self.fatal_msg(format!(
                "xmlParseStringCharRef: invalid xmlChar value {val}\n"
            ));
        }
        0
    }

    /// `xmlParseStringName` over `s[*p..]`.
    fn parse_string_name(&self, s: &[u8], p: &mut usize) -> Option<String> {
        let rest = std::str::from_utf8(&s[*p..]).ok()?;
        let mut chars = rest.char_indices();
        let (_, c0) = chars.next()?;
        if !name_start(c0 as i32) {
            return None;
        }
        let mut end = c0.len_utf8();
        for (i, c) in chars {
            if !name_char(c as i32) {
                break;
            }
            end = i + c.len_utf8();
        }
        let name = rest[..end].to_string();
        *p += end;
        Some(name)
    }

    /// `xmlParseStringEntityRef`.
    fn parse_string_entity_ref(&mut self, s: &[u8], p: &mut usize) -> Option<String> {
        if s.get(*p) != Some(&b'&') {
            return None;
        }
        let mut ptr = *p + 1;
        let Some(name) = self.parse_string_name(s, &mut ptr) else {
            self.fatal_msg("xmlParseStringEntityRef: no name\n");
            *p = ptr;
            return None;
        };
        if s.get(ptr) != Some(&b';') {
            self.fatal(E::EntityrefSemicolMissing);
            *p = ptr;
            return None;
        }
        *p = ptr + 1;
        Some(name)
    }

    /// `xmlParseEntityRefInternal`.
    fn parse_entity_ref_internal(&mut self) -> Option<String> {
        if self.cur() != b'&' {
            return None;
        }
        self.next();
        let Some(name) = self.parse_name() else {
            self.fatal_msg("xmlParseEntityRef: no name\n");
            return None;
        };
        if self.cur() != b';' {
            self.fatal(E::EntityrefSemicolMissing);
            return None;
        }
        self.next();
        Some(name)
    }

    /// `xmlHandleUndeclaredEntity`.
    fn undeclared_entity(&mut self, name: &str) {
        if self.standalone == 1 || (!self.has_external_subset && !self.has_perefs) {
            self.fatal_msg(format!("Entity '{name}' not defined\n"));
        } else if self.load_subset || self.opts.replace_entities {
            self.raise(
                Domain::Parser,
                Code::UndeclaredEntityWarning,
                Level::Error,
                format!("Entity '{name}' not defined\n"),
            );
        } else {
            self.raise(
                Domain::Parser,
                Code::UndeclaredEntityWarning,
                Level::Warning,
                format!("Entity '{name}' not defined\n"),
            );
        }
    }

    /// `xmlLookupGeneralEntity`: Err for predefined (content), Ok(index) for declared ones.
    fn lookup_entity(&mut self, name: &str, in_attr: bool) -> Option<Result<usize, &'static str>> {
        if let Some(p) = Doc::predefined(name) {
            return Some(Err(p));
        }
        let Some(e) = self.doc.entity(name) else {
            self.undeclared_entity(name);
            return None;
        };
        match self.doc.entities[e].etype {
            EntityType::ExternalGeneralUnparsed => {
                self.fatal_msg(format!("Entity reference to unparsed entity {name}\n"));
                None
            }
            EntityType::ExternalGeneralParsed if in_attr => {
                self.fatal_msg(format!("Attribute references external entity '{name}'\n"));
                None
            }
            _ => Some(Ok(e)),
        }
    }

    // ── literals and values ──

    /// `xmlParseEntityValue`: (expanded, original).
    fn parse_entity_value(&mut self) -> Option<(String, String)> {
        let quote = self.cur();
        if quote != b'"' && quote != b'\'' {
            self.fatal(E::AttributeNotStarted);
            return None;
        }
        self.inp_mut().cur += 1;
        let mut length = 0;
        loop {
            if self.stopped() {
                return None;
            }
            if self.at_end() {
                self.fatal_msg("(null)");
                return None;
            }
            let c = self.cur();
            if c == quote {
                break;
            }
            self.nextl(1);
            length += 1;
        }
        let i = self.inp();
        let raw = i.buf[i.cur - length..i.cur].to_vec();
        let orig = lossy(&raw);
        let mut out = Vec::new();
        let depth = self.inputs.len();
        self.expand_pes_in_entity_value(&mut out, &raw, depth);
        self.nextl(1);
        Some((lossy(&out), orig))
    }

    fn expand_pes_in_entity_value(&mut self, out: &mut Vec<u8>, s: &[u8], depth: usize) {
        if depth + 1 > 20 {
            self.fatal_msg("Maximum entity nesting depth exceeded");
            return;
        }
        let mut p = 0;
        let mut chunk = 0;
        while p < s.len() && !self.stopped() {
            let c = s[p];
            if c >= 0x80 {
                p += utf8_len(c);
            } else if c == b'&' {
                if s.get(p + 1) == Some(&b'#') {
                    out.extend_from_slice(&s[chunk..p]);
                    let v = self.parse_string_char_ref(s, &mut p);
                    if v == 0 {
                        return;
                    }
                    push_char(out, v);
                    chunk = p;
                } else {
                    p += 1;
                    let name = self.parse_string_name(s, &mut p);
                    let ok = name.is_some() && s.get(p) == Some(&b';');
                    p += 1;
                    if !ok {
                        self.fatal_msg(
                            "EntityValue: '&' forbidden except for entities references\n",
                        );
                        return;
                    }
                }
            } else if c == b'%' {
                out.extend_from_slice(&s[chunk..p]);
                let Some(e) = self.parse_string_pe_reference(s, &mut p) else {
                    return;
                };
                if !self.external() {
                    self.fatal(E::EntityPeInternal);
                    return;
                }
                if self.doc.entities[e].expanding {
                    self.fatal(E::EntityLoop);
                    self.halt();
                    return;
                }
                let content = self.doc.entities[e].content.clone().unwrap_or_default();
                self.doc.entities[e].expanding = true;
                self.expand_pes_in_entity_value(out, content.as_bytes(), depth + 1);
                self.doc.entities[e].expanding = false;
                chunk = p;
            } else {
                if !is_byte_char(c) {
                    self.fatal_msg("invalid character in entity value\n");
                    out.extend_from_slice(&s[chunk..p]);
                    out.extend_from_slice("\u{fffd}".as_bytes());
                    p += 1;
                    chunk = p;
                } else {
                    p += 1;
                }
            }
        }
        out.extend_from_slice(&s[chunk..p.min(s.len())]);
    }

    /// `xmlParseStringPEReference`.
    fn parse_string_pe_reference(&mut self, s: &[u8], p: &mut usize) -> Option<usize> {
        let mut ptr = *p + 1;
        let Some(name) = self.parse_string_name(s, &mut ptr) else {
            self.fatal_msg("PEReference: no name\n");
            *p = ptr;
            return None;
        };
        if s.get(ptr) != Some(&b';') {
            self.fatal(E::PerefSemicolMissing);
            *p = ptr;
            return None;
        }
        ptr += 1;
        *p = ptr;
        self.has_perefs = true;
        match self.doc.parameter.get(&name).copied() {
            Some(e) => Some(e),
            None => {
                self.undeclared_entity(&name);
                None
            }
        }
    }

    /// `xmlParseAttValueInternal`: the value, and whether it was rebuilt (so may carry
    /// references).
    fn parse_att_value_internal(&mut self, normalize: bool, is_namespace: bool) -> Option<String> {
        let replace = self.opts.replace_entities || is_namespace;
        let quote = self.cur();
        if quote != b'"' && quote != b'\'' {
            self.fatal(E::AttributeNotStarted);
            return None;
        }
        self.nextl(1);
        let mut buf: Vec<u8> = Vec::new();
        let mut in_space = true;
        let mut chunk = 0usize;
        macro_rules! flush {
            () => {
                if chunk > 0 {
                    let i = self.inp();
                    buf.extend_from_slice(&i.buf[i.cur - chunk..i.cur]);
                    chunk = 0;
                }
            };
        }
        loop {
            if self.stopped() {
                return None;
            }
            if self.at_end() {
                self.fatal_msg("AttValue: ' expected\n");
                return None;
            }
            let c = self.cur();
            if c >= 0x80 {
                let l = utf8_len(c);
                let i = self.inp();
                if std::str::from_utf8(&i.buf[i.cur..(i.cur + l).min(i.end)]).is_ok() {
                    chunk += l;
                    self.nextl(l);
                } else {
                    // xmlUTF8MultibyteLen: a byte of invalid UTF-8 reads as U+FFFD.
                    flush!();
                    if !std::mem::replace(&mut self.inp_mut().enc_error, true) {
                        let msg = "Invalid bytes in character encoding\n".to_string();
                        self.raise(Domain::Io, Code::Other, Level::Fatal, msg);
                    }
                    buf.extend_from_slice("\u{fffd}".as_bytes());
                    self.nextl(1);
                }
                in_space = false;
            } else if c != b'&' {
                if c > 0x20 {
                    if c == quote {
                        break;
                    }
                    if c == b'<' {
                        self.fatal(E::LtInAttribute);
                    }
                    chunk += 1;
                    in_space = false;
                } else if !is_byte_char(c) {
                    self.fatal_msg("invalid character in attribute value\n");
                    flush!();
                    buf.extend_from_slice("\u{fffd}".as_bytes());
                    in_space = false;
                } else {
                    if normalize && in_space {
                        flush!();
                    } else if c < 0x20 {
                        flush!();
                        buf.push(b' ');
                    } else {
                        chunk += 1;
                    }
                    in_space = true;
                    if c == 0xd && self.b(1) == 0xa {
                        self.inp_mut().cur += 1;
                    }
                }
                self.nextl(1);
            } else if self.b(1) == b'#' {
                flush!();
                let val = self.parse_char_ref();
                if val == 0 {
                    return None;
                }
                if val == b'&' as i32 && !replace {
                    buf.extend_from_slice(b"&#38;");
                    in_space = false;
                } else if val == b' ' as i32 {
                    if !normalize || !in_space {
                        buf.push(b' ');
                    }
                    in_space = true;
                } else {
                    push_char(&mut buf, val);
                    in_space = false;
                }
            } else {
                flush!();
                let Some(name) = self.parse_entity_ref_internal() else {
                    continue;
                };
                let Some(ent) = self.lookup_entity(&name, true) else {
                    continue;
                };
                match ent {
                    Err(content) => {
                        if content == "&" && !replace {
                            buf.extend_from_slice(b"&#38;");
                        } else {
                            buf.extend_from_slice(content.as_bytes());
                        }
                        in_space = false;
                    }
                    Ok(e) => {
                        if replace {
                            let content = self.doc.entities[e].content.clone().unwrap_or_default();
                            let depth = self.inputs.len();
                            self.expand_entity_in_att_value(
                                &mut buf,
                                content.as_bytes(),
                                Some(e),
                                normalize,
                                &mut in_space,
                                depth,
                            );
                        } else {
                            let content = self.doc.entities[e].content.clone().unwrap_or_default();
                            let depth = self.inputs.len();
                            self.check_entity_in_att_value(e, &content, depth);
                            buf.push(b'&');
                            buf.extend_from_slice(self.doc.entities[e].name.clone().as_bytes());
                            buf.push(b';');
                            in_space = false;
                        }
                    }
                }
            }
        }
        if chunk > 0 {
            let i = self.inp();
            buf.extend_from_slice(&i.buf[i.cur - chunk..i.cur]);
        }
        if normalize && in_space && !buf.is_empty() && buf.last() == Some(&b' ') {
            buf.pop();
        }
        self.nextl(1);
        Some(lossy(&buf))
    }

    /// `xmlCheckEntityInAttValue`: only `<` and nesting are checked.
    fn check_entity_in_att_value(&mut self, e: usize, content: &str, depth: usize) {
        if depth + 1 > 20 {
            self.fatal_msg("Maximum entity nesting depth exceeded");
            return;
        }
        if self.doc.entities[e].expanding {
            self.fatal(E::EntityLoop);
            self.halt();
            return;
        }
        let s = content.as_bytes();
        let mut p = 0;
        while p < s.len() && !self.stopped() {
            let c = s[p];
            if c != b'&' {
                if c == b'<' {
                    let n = self.doc.entities[e].name.clone();
                    self.fatal_msg(format!(
                        "'<' in entity '{n}' is not allowed in attributes values\n"
                    ));
                }
                p += 1;
            } else if s.get(p + 1) == Some(&b'#') {
                if self.parse_string_char_ref(s, &mut p) == 0 {
                    break;
                }
            } else {
                let Some(name) = self.parse_string_entity_ref(s, &mut p) else {
                    break;
                };
                if let Some(Ok(sub)) = self.lookup_entity(&name, true) {
                    let c2 = self.doc.entities[sub].content.clone().unwrap_or_default();
                    self.doc.entities[e].expanding = true;
                    self.check_entity_in_att_value(sub, &c2, depth + 1);
                    self.doc.entities[e].expanding = false;
                }
            }
        }
    }

    /// `xmlExpandEntityInAttValue`.
    fn expand_entity_in_att_value(
        &mut self,
        buf: &mut Vec<u8>,
        s: &[u8],
        pent: Option<usize>,
        normalize: bool,
        in_space: &mut bool,
        depth: usize,
    ) {
        if depth + 1 > 20 {
            self.fatal_msg("Maximum entity nesting depth exceeded");
            return;
        }
        if let Some(pe) = pent
            && self.doc.entities[pe].expanding
        {
            self.fatal(E::EntityLoop);
            self.halt();
            return;
        }
        let mut p = 0;
        let mut chunk_start = 0;
        while p < s.len() && !self.stopped() {
            let c = s[p];
            if c != b'&' {
                if let Some(pe) = pent
                    && c == b'<'
                {
                    let n = self.doc.entities[pe].name.clone();
                    self.fatal_msg(format!(
                        "'<' in entity '{n}' is not allowed in attributes values\n"
                    ));
                    break;
                }
                if c <= 0x20 {
                    if normalize && *in_space {
                        buf.extend_from_slice(&s[chunk_start..p]);
                        chunk_start = p + 1;
                    } else if c < 0x20 {
                        buf.extend_from_slice(&s[chunk_start..p]);
                        buf.push(b' ');
                        chunk_start = p + 1;
                    }
                    *in_space = true;
                } else {
                    *in_space = false;
                }
                p += 1;
            } else if s.get(p + 1) == Some(&b'#') {
                buf.extend_from_slice(&s[chunk_start..p]);
                let val = self.parse_string_char_ref(s, &mut p);
                chunk_start = p;
                if val == 0 {
                    break;
                }
                if val == b' ' as i32 {
                    if !normalize || !*in_space {
                        buf.push(b' ');
                    }
                    *in_space = true;
                } else {
                    push_char(buf, val);
                    *in_space = false;
                }
            } else {
                buf.extend_from_slice(&s[chunk_start..p]);
                let name = self.parse_string_entity_ref(s, &mut p);
                chunk_start = p;
                let Some(name) = name else { break };
                match self.lookup_entity(&name, true) {
                    Some(Err(content)) => {
                        buf.extend_from_slice(content.as_bytes());
                        *in_space = false;
                    }
                    Some(Ok(e)) => {
                        if let Some(content) = self.doc.entities[e].content.clone() {
                            if let Some(pe) = pent {
                                self.doc.entities[pe].expanding = true;
                            }
                            self.expand_entity_in_att_value(
                                buf,
                                content.as_bytes(),
                                Some(e),
                                normalize,
                                in_space,
                                depth + 1,
                            );
                            if let Some(pe) = pent {
                                self.doc.entities[pe].expanding = false;
                            }
                        }
                    }
                    None => {}
                }
            }
        }
        buf.extend_from_slice(&s[chunk_start.min(s.len())..p.min(s.len())]);
    }

    /// `xmlParseSystemLiteral`.
    fn parse_system_literal(&mut self) -> Option<String> {
        let stop = self.cur();
        if stop != b'"' && stop != b'\'' {
            self.fatal(E::LiteralNotStarted);
            return None;
        }
        self.next();
        let mut buf = Vec::new();
        let (mut c, mut l) = self.cur_char();
        while is_char(c) && c != stop as i32 {
            push_char(&mut buf, c);
            if buf.len() > MAX_NAME_LENGTH {
                self.fatal_info(E::NameTooLong, "SystemLiteral");
                return None;
            }
            self.nextl(l);
            (c, l) = self.cur_char();
        }
        if !is_char(c) {
            self.fatal(E::LiteralNotFinished);
        } else {
            self.next();
        }
        Some(lossy(&buf))
    }

    /// `xmlParsePubidLiteral`.
    fn parse_pubid_literal(&mut self) -> Option<String> {
        let stop = self.cur();
        if stop != b'"' && stop != b'\'' {
            self.fatal(E::LiteralNotStarted);
            return None;
        }
        self.next();
        let mut buf = Vec::new();
        let mut c = self.cur();
        while is_pubid(c) && c != stop && !self.stopped() {
            buf.push(c);
            if buf.len() > MAX_NAME_LENGTH {
                self.fatal_info(E::NameTooLong, "Public ID");
                return None;
            }
            self.next();
            c = self.cur();
        }
        if c != stop {
            self.fatal(E::LiteralNotFinished);
        } else {
            self.nextl(1);
        }
        Some(lossy(&buf))
    }

    /// `xmlParseExternalID`: (system, public).
    fn parse_external_id(&mut self, strict: bool) -> (Option<String>, Option<String>) {
        let mut uri = None;
        let mut public = None;
        if self.starts(b"SYSTEM") {
            self.skip(6);
            if self.skip_blanks() == 0 {
                self.fatal_msg("Space required after 'SYSTEM'\n");
            }
            uri = self.parse_system_literal();
            if uri.is_none() {
                self.fatal(E::UriRequired);
            }
        } else if self.starts(b"PUBLIC") {
            self.skip(6);
            if self.skip_blanks() == 0 {
                self.fatal_msg("Space required after 'PUBLIC'\n");
            }
            public = self.parse_pubid_literal();
            if public.is_none() {
                self.fatal(E::PubidRequired);
            }
            if strict {
                if self.skip_blanks() == 0 {
                    self.fatal_msg("Space required after the Public Identifier\n");
                }
            } else {
                if self.skip_blanks() == 0 {
                    return (None, public);
                }
                if self.cur() != b'\'' && self.cur() != b'"' {
                    return (None, public);
                }
            }
            uri = self.parse_system_literal();
            if uri.is_none() {
                self.fatal(E::UriRequired);
            }
        }
        (uri, public)
    }

    // ── character data, comments, PIs, CDATA ──

    /// `areBlanks`.
    fn are_blanks(&self, s: &[u8], blank_chars: bool) -> bool {
        if self.opts.keep_blanks {
            return false;
        }
        let space = *self.spaces.last().unwrap_or(&-1);
        if space == 1 || space == -2 {
            return false;
        }
        if !blank_chars && !s.iter().all(|&c| is_blank(c)) {
            return false;
        }
        let Some(&node) = self.nodes.last() else {
            return false;
        };
        let n = &self.doc.nodes[node];
        let prefix = n.ns.as_ref().and_then(|ns| ns.prefix.clone());
        if let Some(&d) = self.doc.elem_decls.get(&(n.name.clone(), prefix))
            && let Decl::Element { etype, .. } = &self.doc.nodes[d].decl
        {
            match etype {
                ElemType::Element => return true,
                ElemType::Any | ElemType::Mixed => return false,
                ElemType::Empty => {}
            }
        }
        let raw = self.cur();
        if raw != b'<' && raw != 0xd {
            return false;
        }
        if n.children.is_empty() && raw == b'<' && self.b(1) == b'/' {
            return false;
        }
        match n.children.last() {
            None => {
                if n.kind != Kind::Element && !n.content.is_empty() {
                    return false;
                }
            }
            Some(&last) => {
                if self.doc.nodes[last].kind == Kind::Text {
                    return false;
                }
                if let Some(&first) = n.children.first()
                    && self.doc.nodes[first].kind == Kind::Text
                {
                    return false;
                }
            }
        }
        true
    }

    /// Characters reached the content handler (`characters` or `ignorableWhitespace`).
    fn chars(&mut self, s: &[u8], blank_check: Option<bool>) {
        if !self.sax() {
            return;
        }
        if let Some(bc) = blank_check
            && self.are_blanks(s, bc)
        {
            return;
        }
        self.sax_text(&lossy(s), Kind::Text);
        if blank_check.is_some()
            && !self.opts.keep_blanks
            && let Some(sp) = self.spaces.last_mut()
            && *sp == -1
        {
            *sp = -2;
        }
    }

    /// `xmlParseCharDataInternal`.
    fn parse_char_data(&mut self) {
        let mut line = self.inp().line;
        let mut col = self.inp().col;
        let mut k = self.inp().cur;
        let buf = self.inp().buf.clone();
        let end = self.inp().end;
        let at = |k: usize| if k < end { buf[k] } else { 0 };
        loop {
            // get_more_space
            loop {
                while at(k) == 0x20 {
                    k += 1;
                    self.inp_mut().col += 1;
                }
                if at(k) == 0xa {
                    while at(k) == 0xa {
                        let i = self.inp_mut();
                        i.line += 1;
                        i.col = 1;
                        k += 1;
                    }
                    continue;
                }
                break;
            }
            if at(k) == b'<' {
                let start = self.inp().cur;
                if k > start {
                    self.inp_mut().cur = k;
                    let s = buf[start..k].to_vec();
                    if self.sax() {
                        if !self.opts.keep_blanks {
                            if self.are_blanks(&s, true) {
                            } else {
                                self.sax_text(&lossy(&s), Kind::Text);
                                if let Some(sp) = self.spaces.last_mut()
                                    && *sp == -1
                                {
                                    *sp = -2;
                                }
                            }
                        } else {
                            self.sax_text(&lossy(&s), Kind::Text);
                        }
                    }
                }
                return;
            }
            // get_more
            loop {
                let mut ccol = self.inp().col;
                while test_char_data(at(k)) {
                    k += 1;
                    ccol += 1;
                }
                self.inp_mut().col = ccol;
                if at(k) == 0xa {
                    while at(k) == 0xa {
                        let i = self.inp_mut();
                        i.line += 1;
                        i.col = 1;
                        k += 1;
                    }
                    continue;
                }
                if at(k) == b']' {
                    if at(k + 1) == b']' && at(k + 2) == b'>' {
                        self.fatal(E::MisplacedCdataEnd);
                        self.inp_mut().cur = k + 1;
                        return;
                    }
                    k += 1;
                    self.inp_mut().col += 1;
                    continue;
                }
                break;
            }
            let start = self.inp().cur;
            if k > start {
                let s = buf[start..k].to_vec();
                if self.sax() && !self.opts.keep_blanks && is_blank(s[0]) {
                    self.inp_mut().cur = k;
                    if !self.are_blanks(&s, false) {
                        self.sax_text(&lossy(&s), Kind::Text);
                        if let Some(sp) = self.spaces.last_mut()
                            && *sp == -1
                        {
                            *sp = -2;
                        }
                    }
                    line = self.inp().line;
                    col = self.inp().col;
                } else if self.sax() {
                    self.sax_text(&lossy(&s), Kind::Text);
                    line = self.inp().line;
                    col = self.inp().col;
                }
            }
            self.inp_mut().cur = k;
            if at(k) == 0xd && at(k + 1) == 0xa {
                self.inp_mut().cur = k + 1;
                k += 2;
                let i = self.inp_mut();
                i.line += 1;
                i.col = 1;
                if (0x20..=0x7f).contains(&at(k)) || at(k) == 0x09 || at(k) == 0x0a {
                    continue;
                }
                break;
            }
            if at(k) == b'<' || at(k) == b'&' {
                return;
            }
            k = self.inp().cur;
            let c = at(k);
            if !((0x20..=0x7f).contains(&c) || c == 0x09 || c == 0x0a) {
                break;
            }
        }
        let i = self.inp_mut();
        i.line = line;
        i.col = col;
        self.parse_char_data_complex();
    }

    /// `xmlParseCharDataComplex`.
    fn parse_char_data_complex(&mut self) {
        let mut buf: Vec<u8> = Vec::new();
        let (mut c, mut l) = self.cur_char();
        while c != b'<' as i32 && c != b'&' as i32 && is_char(c) {
            if c == b']' as i32 && self.b(1) == b']' && self.b(2) == b'>' {
                self.fatal(E::MisplacedCdataEnd);
            }
            push_char(&mut buf, c);
            self.nextl(l);
            if buf.len() >= 300 {
                let s = std::mem::take(&mut buf);
                self.chars(&s, Some(false));
            }
            (c, l) = self.cur_char();
        }
        if !buf.is_empty() {
            self.chars(&buf, Some(false));
        }
        if !self.at_end() && c != b'<' as i32 && c != b'&' as i32 {
            self.fatal_msg(format!("PCDATA invalid Char value {c}\n"));
            self.nextl(l);
        }
    }

    /// `xmlParseComment`.
    fn parse_comment(&mut self) {
        if self.cur() != b'<' || self.b(1) != b'!' {
            return;
        }
        self.skip(2);
        if self.cur() != b'-' || self.b(1) != b'-' {
            return;
        }
        self.skip(2);
        let mut buf: Option<Vec<u8>> = None;
        let src = self.inp().buf.clone();
        let end = self.inp().end;
        let at = |k: usize| if k < end { src[k] } else { 0 };
        let mut k = self.inp().cur;
        loop {
            if at(k) == 0xa {
                while at(k) == 0xa {
                    let i = self.inp_mut();
                    i.line += 1;
                    i.col = 1;
                    k += 1;
                }
            }
            'get_more: loop {
                let mut ccol = self.inp().col;
                while (at(k) > b'-' && at(k) <= 0x7f)
                    || (at(k) >= 0x20 && at(k) < b'-')
                    || at(k) == 0x09
                {
                    k += 1;
                    ccol += 1;
                }
                self.inp_mut().col = ccol;
                if at(k) == 0xa {
                    while at(k) == 0xa {
                        let i = self.inp_mut();
                        i.line += 1;
                        i.col = 1;
                        k += 1;
                    }
                    continue 'get_more;
                }
                let start = self.inp().cur;
                if k > start {
                    buf.get_or_insert_with(Vec::new)
                        .extend_from_slice(&src[start..k]);
                }
                self.inp_mut().cur = k;
                if at(k) == 0xa {
                    k += 1;
                    let i = self.inp_mut();
                    i.line += 1;
                    i.col = 1;
                }
                if at(k) == 0xd && at(k + 1) == 0xa {
                    self.inp_mut().cur = k + 1;
                    k += 2;
                    let i = self.inp_mut();
                    i.line += 1;
                    i.col = 1;
                    continue 'get_more;
                }
                k = self.inp().cur;
                if at(k) == b'-' {
                    if at(k + 1) == b'-' {
                        if at(k + 2) == b'>' {
                            self.skip(3);
                            if self.sax() {
                                let s = buf.map(|b| lossy(&b)).unwrap_or_default();
                                self.sax_comment(&s);
                            }
                            return;
                        }
                        match &buf {
                            Some(b) => {
                                let t = trunc50(b);
                                self.fatal_msg(format!("Double hyphen within comment: <!--{t}\n"))
                            }
                            None => self.fatal_msg("Double hyphen within comment\n"),
                        }
                        k += 1;
                        self.inp_mut().col += 1;
                    }
                    k += 1;
                    self.inp_mut().col += 1;
                    continue 'get_more;
                }
                break;
            }
            let c = at(k);
            if !((0x20..=0x7f).contains(&c) || c == 0x09 || c == 0x0a) {
                break;
            }
        }
        self.parse_comment_complex(buf.unwrap_or_default());
    }

    fn parse_comment_complex(&mut self, mut buf: Vec<u8>) {
        let (mut q, mut ql) = self.cur_char();
        if q == 0 {
            self.fatal_msg("Comment not terminated\n");
            return;
        }
        if !is_char(q) {
            self.fatal_msg(format!("xmlParseComment: invalid xmlChar value {q}\n"));
            return;
        }
        self.nextl(ql);
        let (mut r, mut rl) = self.cur_char();
        if r == 0 {
            self.fatal_msg("Comment not terminated\n");
            return;
        }
        if !is_char(r) {
            self.fatal_msg(format!("xmlParseComment: invalid xmlChar value {r}\n"));
            return;
        }
        self.nextl(rl);
        let (mut c, mut l) = self.cur_char();
        if c == 0 {
            self.fatal_msg("Comment not terminated\n");
            return;
        }
        while is_char(c) && (c != b'>' as i32 || r != b'-' as i32 || q != b'-' as i32) {
            if r == b'-' as i32 && q == b'-' as i32 {
                self.fatal(E::HyphenInComment);
            }
            push_char(&mut buf, q);
            q = r;
            ql = rl;
            r = c;
            rl = l;
            self.nextl(l);
            (c, l) = self.cur_char();
        }
        let _ = (ql, rl);
        if c == 0 {
            let t = trunc50(&buf);
            self.fatal_msg(format!("Comment not terminated \n<!--{t}\n"));
        } else if !is_char(c) {
            self.fatal_msg(format!("xmlParseComment: invalid xmlChar value {c}\n"));
        } else {
            self.next();
            if self.sax() {
                self.sax_comment(&lossy(&buf));
            }
        }
    }

    /// `xmlParsePITarget`.
    fn parse_pi_target(&mut self) -> Option<String> {
        let name = self.parse_name()?;
        let b = name.as_bytes();
        if b.len() >= 3 && b[..3].eq_ignore_ascii_case(b"xml") {
            if name == "xml" {
                self.fatal_msg("XML declaration allowed only at the start of the document\n");
                return Some(name);
            } else if b.len() == 3 {
                self.fatal(E::ReservedXmlName);
                return Some(name);
            }
            if !matches!(name.as_str(), "xml-stylesheet" | "xml-model") {
                self.warning("xmlParsePITarget: invalid name prefix 'xml'\n");
            }
        }
        if name.contains(':') {
            self.ns_err(format!("colons are forbidden from PI names '{name}'\n"));
        }
        Some(name)
    }

    /// `xmlParsePI`.
    fn parse_pi(&mut self) {
        if self.cur() != b'<' || self.b(1) != b'?' {
            return;
        }
        self.skip(2);
        let Some(target) = self.parse_pi_target() else {
            self.fatal(E::PiNotStarted);
            return;
        };
        if self.cur() == b'?' && self.b(1) == b'>' {
            self.skip(2);
            if self.sax() {
                self.sax_pi(&target, None);
            }
            return;
        }
        if self.skip_blanks() == 0 {
            self.fatal_msg(format!("ParsePI: PI {target} space expected\n"));
        }
        let mut buf = Vec::new();
        let (mut c, mut l) = self.cur_char();
        while is_char(c) && (c != b'?' as i32 || self.b(1) != b'>') {
            push_char(&mut buf, c);
            self.nextl(l);
            (c, l) = self.cur_char();
        }
        if c != b'?' as i32 {
            self.fatal_msg(format!("ParsePI: PI {target} never end ...\n"));
        } else {
            self.skip(2);
            if self.sax() {
                self.sax_pi(&target, Some(&lossy(&buf)));
            }
        }
    }

    /// `xmlParseCDSect`.
    fn parse_cdsect(&mut self) {
        self.skip(9);
        let (r0, rl0) = self.cur_char();
        if !is_char(r0) {
            self.fatal(E::CdataNotFinished);
            return;
        }
        self.nextl(rl0);
        let (s0, sl0) = self.cur_char();
        if !is_char(s0) {
            self.fatal(E::CdataNotFinished);
            return;
        }
        self.nextl(sl0);
        let (mut r, mut s) = (r0, s0);
        let (mut c, mut l) = self.cur_char();
        let mut buf = Vec::new();
        while is_char(c) && (r != b']' as i32 || s != b']' as i32 || c != b'>' as i32) {
            push_char(&mut buf, r);
            r = s;
            s = c;
            self.nextl(l);
            (c, l) = self.cur_char();
        }
        if c != b'>' as i32 {
            let t = trunc50(&buf);
            self.fatal_msg(format!("CData section not finished\n{t}\n"));
            return;
        }
        self.nextl(l);
        if self.sax() {
            self.sax_text(&lossy(&buf), Kind::CData);
        }
    }

    // ── DTD ──

    /// `xmlParsePEReference`.
    fn parse_pe_reference(&mut self) {
        if self.cur() != b'%' {
            return;
        }
        self.next();
        let Some(name) = self.parse_name() else {
            self.fatal_msg("PEReference: no name\n");
            return;
        };
        if self.cur() != b';' {
            self.fatal(E::PerefSemicolMissing);
            return;
        }
        self.next();
        self.has_perefs = true;
        let Some(e) = self.doc.parameter.get(&name).copied() else {
            self.undeclared_entity(&name);
            return;
        };
        let ent = &self.doc.entities[e];
        if ent.etype == EntityType::ExternalParameter
            && !self.load_subset
            && !self.opts.replace_entities
        {
            return;
        }
        if ent.expanding {
            self.fatal(E::EntityLoop);
            self.halt();
            return;
        }
        let text = if ent.etype == EntityType::ExternalParameter {
            String::new()
        } else {
            ent.content.clone().unwrap_or_default()
        };
        if self.inputs.len() > 20 {
            self.fatal_msg("Maximum entity nesting depth exceeded");
            self.halt();
            return;
        }
        let input = self.new_input(text.as_bytes(), Some(e));
        self.inputs.push(input);
        self.doc.entities[e].expanding = true;
    }

    /// `xmlParseEntityDecl`.
    fn parse_entity_decl(&mut self) {
        self.skip(2);
        if !self.starts(b"ENTITY") {
            return;
        }
        let inputid = self.inp().id;
        self.skip(6);
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after '<!ENTITY'\n");
        }
        let mut is_param = false;
        if self.cur() == b'%' {
            self.next();
            if self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after '%'\n");
            }
            is_param = true;
        }
        let Some(name) = self.parse_name() else {
            self.fatal_msg("xmlParseEntityDecl: no name\n");
            return;
        };
        if name.contains(':') {
            self.ns_err(format!(
                "colons are forbidden from entities names '{name}'\n"
            ));
        }
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after the entity name\n");
        }
        let mut orig = None;
        if self.cur() == b'"' || self.cur() == b'\'' {
            let v = self.parse_entity_value();
            if let Some((value, o)) = v {
                orig = Some(o);
                if self.sax() {
                    let t = if is_param {
                        EntityType::InternalParameter
                    } else {
                        EntityType::InternalGeneral
                    };
                    self.sax_entity_decl(&name, t, None, None, Some(value));
                }
            } else if !is_param && self.sax() {
                self.sax_entity_decl(&name, EntityType::InternalGeneral, None, None, None);
            }
        } else {
            let (uri, literal) = self.parse_external_id(true);
            if uri.is_none() && literal.is_none() {
                self.fatal(E::ValueRequired);
            }
            let mut ok_uri = true;
            if let Some(u) = &uri
                && u.contains('#')
            {
                self.fatal(E::UriFragment);
                ok_uri = false;
            }
            if is_param {
                if uri.is_some() && ok_uri && self.sax() {
                    self.sax_entity_decl(&name, EntityType::ExternalParameter, literal, uri, None);
                }
            } else {
                if self.cur() != b'>' && self.skip_blanks_pe() == 0 {
                    self.fatal_msg("Space required before 'NDATA'\n");
                }
                if self.starts(b"NDATA") {
                    self.skip(5);
                    if self.skip_blanks_pe() == 0 {
                        self.fatal_msg("Space required after 'NDATA'\n");
                    }
                    let _ndata = self.parse_name();
                    if self.sax() {
                        self.sax_entity_decl(
                            &name,
                            EntityType::ExternalGeneralUnparsed,
                            literal,
                            uri,
                            None,
                        );
                    }
                } else if self.sax() {
                    self.sax_entity_decl(
                        &name,
                        EntityType::ExternalGeneralParsed,
                        literal,
                        uri,
                        None,
                    );
                }
            }
        }
        self.skip_blanks_pe();
        if self.cur() != b'>' {
            self.fatal_msg(format!(
                "xmlParseEntityDecl: entity {name} not terminated\n"
            ));
            self.halt();
        } else {
            if inputid != self.inp().id {
                self.fatal_msg("Entity declaration doesn't start and stop in the same entity\n");
            }
            self.next();
        }
        if let Some(o) = orig {
            let t = if is_param {
                self.doc.parameter.get(&name)
            } else {
                self.doc.general.get(&name)
            };
            if let Some(&e) = t
                && self.doc.entities[e].orig.is_none()
            {
                self.doc.entities[e].orig = Some(o);
            }
        }
    }

    /// `xmlParseNotationDecl`.
    fn parse_notation_decl(&mut self) {
        self.skip(2);
        if !self.starts(b"NOTATION") {
            return;
        }
        let inputid = self.inp().id;
        self.skip(8);
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after '<!NOTATION'\n");
            return;
        }
        let Some(name) = self.parse_name() else {
            self.fatal(E::NotationNotStarted);
            return;
        };
        if name.contains(':') {
            self.ns_err(format!(
                "colons are forbidden from notation names '{name}'\n"
            ));
        }
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after the NOTATION name'\n");
            return;
        }
        let (system, public) = self.parse_external_id(false);
        self.skip_blanks_pe();
        if self.cur() == b'>' {
            if inputid != self.inp().id {
                self.fatal_msg("Notation declaration doesn't start and stop in the same entity\n");
            }
            self.next();
            if self.sax() {
                self.doc.notations.push((name, public, system));
            }
        } else {
            self.fatal(E::NotationNotFinished);
        }
    }

    /// `xmlParseAttributeListDecl`.
    fn parse_attribute_list_decl(&mut self) {
        self.skip(2);
        if !self.starts(b"ATTLIST") {
            return;
        }
        let inputid = self.inp().id;
        self.skip(7);
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after '<!ATTLIST'\n");
        }
        let Some(elem) = self.parse_name() else {
            self.fatal_msg("ATTLIST: no name for Element\n");
            return;
        };
        self.skip_blanks_pe();
        while self.cur() != b'>' && !self.stopped() {
            let Some(attr) = self.parse_name() else {
                self.fatal_msg("ATTLIST: no name for Attribute\n");
                break;
            };
            if self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after the attribute name\n");
                break;
            }
            let Some((atype, tree)) = self.parse_attribute_type() else {
                break;
            };
            if self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after the attribute type\n");
                break;
            }
            let Some((def, mut default)) = self.parse_default_decl() else {
                break;
            };
            if atype != AttrType::Cdata
                && let Some(d) = &default
            {
                default = Some(normalize_space(d));
            }
            if self.cur() != b'>' && self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after the attribute default value\n");
                break;
            }
            if self.sax() {
                self.sax_attribute_decl(&elem, &attr, atype, def, default.clone(), tree);
            }
            if let Some(d) = default
                && def != AttrDefault::Implied
                && def != AttrDefault::Required
            {
                self.add_def_attr(&elem, &attr, d);
            }
            self.atts_special
                .entry((elem.clone(), attr.clone()))
                .or_insert(atype);
        }
        if self.cur() == b'>' {
            if inputid != self.inp().id {
                self.fatal_msg(
                    "Attribute list declaration doesn't start and stop in the same entity\n",
                );
            }
            self.next();
        }
    }

    fn add_def_attr(&mut self, elem: &str, attr: &str, value: String) {
        if self
            .atts_special
            .contains_key(&(elem.to_string(), attr.to_string()))
        {
            return;
        }
        let key = split_qname(elem);
        let (name, prefix) = split_qname(attr);
        self.atts_default
            .entry((key.0, key.1))
            .or_default()
            .push(DefAttr {
                name,
                prefix,
                value,
            });
    }

    fn parse_attribute_type(&mut self) -> Option<(AttrType, Vec<String>)> {
        let t = if self.starts(b"CDATA") {
            self.skip(5);
            AttrType::Cdata
        } else if self.starts(b"IDREFS") {
            self.skip(6);
            AttrType::Idrefs
        } else if self.starts(b"IDREF") {
            self.skip(5);
            AttrType::Idref
        } else if self.starts(b"ID") {
            self.skip(2);
            AttrType::Id
        } else if self.starts(b"ENTITY") {
            self.skip(6);
            AttrType::Entity
        } else if self.starts(b"ENTITIES") {
            self.skip(8);
            AttrType::Entities
        } else if self.starts(b"NMTOKENS") {
            self.skip(8);
            AttrType::Nmtokens
        } else if self.starts(b"NMTOKEN") {
            self.skip(7);
            AttrType::Nmtoken
        } else if self.starts(b"NOTATION") {
            self.skip(8);
            if self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after 'NOTATION'\n");
                return None;
            }
            let tree = self.parse_enumeration(true)?;
            return Some((AttrType::Notation, tree));
        } else {
            let tree = self.parse_enumeration(false)?;
            return Some((AttrType::Enumeration, tree));
        };
        Some((t, Vec::new()))
    }

    /// `xmlParseNotationType` / `xmlParseEnumerationType`.
    fn parse_enumeration(&mut self, notation: bool) -> Option<Vec<String>> {
        if self.cur() != b'(' {
            self.fatal(if notation {
                E::NotationNotStarted
            } else {
                E::AttlistNotStarted
            });
            return None;
        }
        let mut out: Vec<String> = Vec::new();
        loop {
            self.next();
            self.skip_blanks_pe();
            let name = if notation {
                self.parse_name()
            } else {
                self.parse_nmtoken()
            };
            let Some(name) = name else {
                if notation {
                    self.fatal_msg("Name expected in NOTATION declaration\n");
                    return None;
                }
                self.fatal(E::NmtokenRequired);
                return if out.is_empty() { None } else { Some(out) };
            };
            if out.contains(&name) {
                let what = if notation { "notation" } else { "enumeration" };
                self.raise(
                    Domain::Dtd,
                    Code::Other,
                    Level::Error,
                    format!("standalone: attribute {what} value token {name} duplicated\n"),
                );
            } else {
                out.push(name);
            }
            self.skip_blanks_pe();
            if self.cur() != b'|' {
                break;
            }
        }
        if self.cur() != b')' {
            if notation {
                self.fatal(E::NotationNotFinished);
                return None;
            }
            self.fatal(E::AttlistNotFinished);
            return Some(out);
        }
        self.next();
        Some(out)
    }

    /// `xmlParseDefaultDecl`.
    fn parse_default_decl(&mut self) -> Option<(AttrDefault, Option<String>)> {
        if self.starts(b"#REQUIRED") {
            self.skip(9);
            return Some((AttrDefault::Required, None));
        }
        if self.starts(b"#IMPLIED") {
            self.skip(8);
            return Some((AttrDefault::Implied, None));
        }
        let mut val = AttrDefault::None;
        if self.starts(b"#FIXED") {
            self.skip(6);
            val = AttrDefault::Fixed;
            if self.skip_blanks_pe() == 0 {
                self.fatal_msg("Space required after '#FIXED'\n");
            }
        }
        let in_subset = self.in_subset;
        let _ = in_subset;
        match self.parse_att_value_internal(false, false) {
            Some(v) => Some((val, Some(v))),
            None => {
                self.fatal_msg("Attribute default value declaration error\n");
                if val == AttrDefault::None {
                    None
                } else {
                    Some((val, None))
                }
            }
        }
    }

    /// `xmlParseElementDecl`.
    fn parse_element_decl(&mut self) {
        self.skip(2);
        if !self.starts(b"ELEMENT") {
            return;
        }
        let inputid = self.inp().id;
        self.skip(7);
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after 'ELEMENT'\n");
            return;
        }
        let Some(name) = self.parse_name() else {
            self.fatal_msg("xmlParseElementDecl: no name for Element\n");
            return;
        };
        if self.skip_blanks_pe() == 0 {
            self.fatal_msg("Space required after the element name\n");
        }
        let mut content = None;
        let etype = if self.starts(b"EMPTY") {
            self.skip(5);
            ElemType::Empty
        } else if self.starts(b"ANY") {
            self.skip(3);
            ElemType::Any
        } else if self.cur() == b'(' {
            match self.parse_element_content_decl(&name) {
                Some((t, c)) => {
                    content = c;
                    t
                }
                None => return,
            }
        } else {
            self.fatal_msg("xmlParseElementDecl: 'EMPTY', 'ANY' or '(' expected\n");
            return;
        };
        self.skip_blanks_pe();
        if self.cur() != b'>' {
            self.fatal(E::GtRequired);
        } else {
            if inputid != self.inp().id {
                self.fatal_msg("Element declaration doesn't start and stop in the same entity\n");
            }
            self.next();
            if self.sax() {
                self.sax_element_decl(&name, etype, content);
            }
        }
    }

    fn parse_element_content_decl(
        &mut self,
        name: &str,
    ) -> Option<(ElemType, Option<ElemContent>)> {
        let inputid = self.inp().id;
        if self.cur() != b'(' {
            self.fatal_msg(format!(
                "xmlParseElementContentDecl : {name} '(' expected\n"
            ));
            return None;
        }
        self.next();
        self.skip_blanks_pe();
        let r = if self.starts(b"#PCDATA") {
            (ElemType::Mixed, self.parse_mixed_content_decl(inputid))
        } else {
            (
                ElemType::Element,
                self.parse_children_content_decl(inputid, 1),
            )
        };
        self.skip_blanks_pe();
        Some(r)
    }

    fn content(kind: ContentKind, name: Option<String>) -> ElemContent {
        let (name, prefix) = match name {
            Some(n) => {
                let (l, p) = split_qname(&n);
                (Some(l), p)
            }
            None => (None, None),
        };
        ElemContent {
            kind,
            ocur: Occur::Once,
            name,
            prefix,
            c1: None,
            c2: None,
        }
    }

    /// `xmlParseElementMixedContentDecl`.
    fn parse_mixed_content_decl(&mut self, inputchk: i32) -> Option<ElemContent> {
        self.skip(7);
        self.skip_blanks_pe();
        if self.cur() == b')' {
            if self.inp().id != inputchk {
                self.fatal_msg(
                    "Element content declaration doesn't start and stop in the same entity\n",
                );
            }
            self.next();
            let mut ret = Self::content(ContentKind::Pcdata, None);
            if self.cur() == b'*' {
                ret.ocur = Occur::Mult;
                self.next();
            }
            return Some(ret);
        }
        // The list of names, kept flat then folded into the right-leaning OR tree libxml builds.
        let mut names: Vec<String> = Vec::new();
        if self.cur() == b'(' || self.cur() == b'|' {
            while self.cur() == b'|' && !self.stopped() {
                self.next();
                self.skip_blanks_pe();
                let Some(elem) = self.parse_name() else {
                    self.fatal_msg("xmlParseElementMixedContentDecl : Name expected\n");
                    return None;
                };
                names.push(elem);
                self.skip_blanks_pe();
            }
            if self.cur() == b')' && self.b(1) == b'*' {
                if self.inp().id != inputchk {
                    self.fatal_msg(
                        "Element content declaration doesn't start and stop in the same entity\n",
                    );
                }
                self.skip(2);
                let mut ret = Self::content(ContentKind::Pcdata, None);
                // ((#PCDATA | a) | b): each OR's c1 is the previous tree, c2 the next name.
                if !names.is_empty() {
                    let mut cur = Self::content(ContentKind::Or, None);
                    cur.c1 = Some(Box::new(ret));
                    let mut root = cur;
                    // libxml nests ORs to the right: OR(pcdata, OR(a, OR(b, c)))
                    let mut tail: Option<ElemContent> = None;
                    for (i, n) in names.iter().enumerate().rev() {
                        let leaf = Self::content(ContentKind::Element, Some(n.clone()));
                        tail = Some(match tail {
                            None => leaf,
                            Some(t) if i + 1 < names.len() => {
                                let mut o = Self::content(ContentKind::Or, None);
                                o.c1 = Some(Box::new(leaf));
                                o.c2 = Some(Box::new(t));
                                o
                            }
                            Some(t) => t,
                        });
                    }
                    root.c2 = tail.map(Box::new);
                    ret = root;
                }
                ret.ocur = Occur::Mult;
                return Some(ret);
            }
            self.fatal(E::MixedNotStarted);
            return None;
        }
        self.fatal(E::PcdataRequired);
        None
    }

    /// `xmlParseElementChildrenContentDeclPriv`, building the same left-to-right chain.
    fn parse_children_content_decl(&mut self, inputchk: i32, depth: i32) -> Option<ElemContent> {
        if depth > 256 {
            self.fatal_msg(format!(
                "xmlParseElementChildrenContentDecl : depth {depth} too deep, use XML_PARSE_HUGE\n"
            ));
            return None;
        }
        self.skip_blanks_pe();
        let mut items: Vec<ElemContent> = Vec::new();
        let first = if self.cur() == b'(' {
            let id = self.inp().id;
            self.next();
            self.skip_blanks_pe();
            let c = self.parse_children_content_decl(id, depth + 1)?;
            self.skip_blanks_pe();
            c
        } else {
            let Some(elem) = self.parse_name() else {
                self.fatal(E::ElemcontentNotStarted);
                return None;
            };
            let mut c = Self::content(ContentKind::Element, Some(elem));
            c.ocur = self.parse_occur();
            c
        };
        items.push(first);
        self.skip_blanks_pe();
        let mut sep: u8 = 0;
        while self.cur() != b')' && !self.stopped() {
            let c = self.cur();
            if c == b',' || c == b'|' {
                if sep == 0 {
                    sep = c;
                } else if sep != c {
                    self.fatal_msg(format!(
                        "xmlParseElementChildrenContentDecl : '{}' expected\n",
                        sep as char
                    ));
                    return None;
                }
                self.next();
            } else {
                self.fatal(E::ElemcontentNotFinished);
                return None;
            }
            self.skip_blanks_pe();
            if self.cur() == b'(' {
                let id = self.inp().id;
                self.next();
                self.skip_blanks_pe();
                let c = self.parse_children_content_decl(id, depth + 1)?;
                items.push(c);
                self.skip_blanks_pe();
            } else {
                let Some(elem) = self.parse_name() else {
                    self.fatal(E::ElemcontentNotStarted);
                    return None;
                };
                let mut c = Self::content(ContentKind::Element, Some(elem));
                c.ocur = self.parse_occur();
                items.push(c);
            }
            self.skip_blanks_pe();
        }
        if self.inp().id != inputchk {
            self.fatal_msg(
                "Element content declaration doesn't start and stop in the same entity\n",
            );
        }
        self.next();
        let kind = if sep == b'|' {
            ContentKind::Or
        } else {
            ContentKind::Seq
        };
        let mut ret = if items.len() == 1 {
            items.pop().unwrap()
        } else {
            // a , b , c => SEQ(a, SEQ(b, c))
            let mut it = items.into_iter().rev();
            let mut acc = it.next().unwrap();
            for x in it {
                let mut o = Self::content(kind, None);
                o.c1 = Some(Box::new(x));
                o.c2 = Some(Box::new(acc));
                acc = o;
            }
            acc
        };
        match self.cur() {
            b'?' => {
                ret.ocur = if matches!(ret.ocur, Occur::Plus | Occur::Mult) {
                    Occur::Mult
                } else {
                    Occur::Opt
                };
                self.next();
            }
            b'*' => {
                ret.ocur = Occur::Mult;
                let mut cur = Some(&mut ret);
                while let Some(c) = cur {
                    if c.kind != ContentKind::Or {
                        break;
                    }
                    if let Some(c1) = c.c1.as_mut()
                        && matches!(c1.ocur, Occur::Opt | Occur::Mult)
                    {
                        c1.ocur = Occur::Once;
                    }
                    if let Some(c2) = c.c2.as_mut()
                        && matches!(c2.ocur, Occur::Opt | Occur::Mult)
                    {
                        c2.ocur = Occur::Once;
                    }
                    cur = c.c2.as_deref_mut();
                }
                self.next();
            }
            b'+' => {
                ret.ocur = if matches!(ret.ocur, Occur::Opt | Occur::Mult) {
                    Occur::Mult
                } else {
                    Occur::Plus
                };
                let mut found = false;
                let mut cur = Some(&mut ret);
                while let Some(c) = cur {
                    if c.kind != ContentKind::Or {
                        break;
                    }
                    if let Some(c1) = c.c1.as_mut()
                        && matches!(c1.ocur, Occur::Opt | Occur::Mult)
                    {
                        c1.ocur = Occur::Once;
                        found = true;
                    }
                    if let Some(c2) = c.c2.as_mut()
                        && matches!(c2.ocur, Occur::Opt | Occur::Mult)
                    {
                        c2.ocur = Occur::Once;
                        found = true;
                    }
                    cur = c.c2.as_deref_mut();
                }
                if found {
                    ret.ocur = Occur::Mult;
                }
                self.next();
            }
            _ => {}
        }
        Some(ret)
    }

    fn parse_occur(&mut self) -> Occur {
        match self.cur() {
            b'?' => {
                self.next();
                Occur::Opt
            }
            b'*' => {
                self.next();
                Occur::Mult
            }
            b'+' => {
                self.next();
                Occur::Plus
            }
            _ => Occur::Once,
        }
    }

    /// `xmlParseMarkupDecl`.
    fn parse_markup_decl(&mut self) {
        if self.cur() != b'<' {
            return;
        }
        if self.b(1) == b'!' {
            match self.b(2) {
                b'E' => {
                    if self.b(3) == b'L' {
                        self.parse_element_decl();
                    } else if self.b(3) == b'N' {
                        self.parse_entity_decl();
                    } else {
                        self.skip(2);
                    }
                }
                b'A' => self.parse_attribute_list_decl(),
                b'N' => self.parse_notation_decl(),
                b'-' => self.parse_comment(),
                _ => {
                    self.fatal(E::IntSubsetNotFinished);
                    self.skip(2);
                }
            }
        } else if self.b(1) == b'?' {
            self.parse_pi();
        }
    }

    /// `xmlParseDocTypeDecl`.
    fn parse_doctype_decl(&mut self) {
        self.skip(9);
        self.skip_blanks();
        let name = self.parse_name();
        if name.is_none() {
            self.fatal_msg("xmlParseDocTypeDecl : no DOCTYPE name !\n");
        }
        self.skip_blanks();
        let (uri, public) = self.parse_external_id(true);
        if uri.is_some() || public.is_some() {
            self.has_external_subset = true;
        }
        self.skip_blanks();
        if self.sax() {
            self.sax_internal_subset(name.as_deref(), public.clone(), uri.clone());
        }
        if self.cur() == b'[' {
            return;
        }
        if self.cur() != b'>' {
            self.fatal(E::DoctypeNotFinished);
        }
        self.next();
        let _ = (uri, public);
    }

    /// `xmlParseInternalSubset`.
    fn parse_internal_subset(&mut self) {
        if self.cur() == b'[' {
            let old = self.inputs.len();
            self.next();
            self.skip_blanks();
            while (self.cur() != b']' || self.inputs.len() > old) && !self.stopped() {
                if self.at_end() && self.inputs.len() > old {
                    self.pop_pe();
                    continue;
                }
                if self.cur() == b'<' && (self.b(1) == b'!' || self.b(1) == b'?') {
                    self.parse_markup_decl();
                } else if self.cur() == b'%' {
                    self.parse_pe_reference();
                } else {
                    self.fatal(E::IntSubsetNotFinished);
                    break;
                }
                self.skip_blanks_pe();
            }
            while self.inputs.len() > old {
                self.pop_pe();
            }
            if self.cur() == b']' {
                self.next();
                self.skip_blanks();
            }
        }
        if self.well_formed && self.cur() != b'>' {
            self.fatal(E::DoctypeNotFinished);
            return;
        }
        self.next();
    }

    // ── elements ──

    fn ns_lookup(&self, prefix: Option<&str>) -> Option<usize> {
        (0..self.ns.len())
            .rev()
            .find(|&i| self.ns[i].0.as_deref() == prefix)
    }

    /// `xmlParserNsPush`: false when nothing was pushed.
    fn ns_push(
        &mut self,
        prefix: Option<String>,
        uri: String,
        elem_start: usize,
        defaulted: bool,
    ) -> bool {
        if prefix.as_deref() == Some("xml") {
            return false;
        }
        if (elem_start..self.ns.len()).any(|i| self.ns[i].0 == prefix) {
            if !defaulted {
                match &prefix {
                    Some(p) => self.fatal_msg(format!("Attribute xmlns:{p} redefined\n")),
                    None => self.fatal_msg("Attribute xmlns redefined\n"),
                }
            }
            return false;
        }
        self.ns.push((prefix, uri));
        true
    }

    /// The URI bound to `prefix` for an attribute (`xmlParserNsLookup` honouring minNsIndex).
    fn attr_ns(&mut self, prefix: Option<&str>, name: &str, elem: &str) -> Option<String> {
        match prefix {
            None => None,
            Some("xml") => Some(XML_NS.to_string()),
            Some(px) => match self.ns_lookup(Some(px)) {
                Some(i) if i >= self.min_ns => Some(self.ns[i].1.clone()),
                _ => {
                    self.ns_err(format!(
                        "Namespace prefix {px} for {name} on {elem} is not defined\n"
                    ));
                    None
                }
            },
        }
    }

    /// `xmlParseStartTag2`: (local name, prefix, uri, namespaces pushed).
    #[allow(clippy::type_complexity)]
    fn parse_start_tag2(&mut self) -> Option<(String, Option<String>, Option<String>, usize)> {
        if self.cur() != b'<' {
            return None;
        }
        self.next1();
        let ns_start = self.ns.len();
        let Some((localname, prefix)) = self.parse_qname() else {
            self.fatal_msg("StartTag: invalid element name\n");
            return None;
        };
        self.skip_blanks();
        let mut atts: Vec<(String, Option<String>, String)> = Vec::new();
        let mut nb_ns = 0;
        while self.cur() != b'>'
            && (self.cur() != b'/' || self.b(1) != b'>')
            && is_byte_char(self.cur())
            && !self.at_end()
            && !self.stopped()
        {
            let Some((attname, aprefix, value)) =
                self.parse_attribute2(&localname, prefix.as_deref())
            else {
                break;
            };
            if let Some(value) = value {
                if attname == "xmlns" && aprefix.is_none() {
                    let uri = value;
                    let mut skip = false;
                    if !uri.is_empty() {
                        if !uri_parses(&uri) {
                            self.ns_err(format!("xmlns: '{uri}' is not a valid URI\n"));
                        } else if !uri_has_scheme(&uri) {
                            self.ns_warn(format!("xmlns: URI {uri} is not absolute\n"));
                        }
                        if uri == XML_NS {
                            self.ns_err("xml namespace URI cannot be the default namespace\n");
                            skip = true;
                        } else if uri == "http://www.w3.org/2000/xmlns/" {
                            self.ns_err("reuse of the xmlns namespace name is forbidden\n");
                            skip = true;
                        }
                    }
                    if !skip && self.ns_push(None, uri, ns_start, false) {
                        nb_ns += 1;
                    }
                } else if aprefix.as_deref() == Some("xmlns") {
                    let uri = value;
                    if attname == "xml" {
                        if uri != XML_NS {
                            self.ns_err("xml namespace prefix mapped to wrong URI\n");
                        }
                    } else if uri == XML_NS {
                        self.ns_err("xml namespace URI mapped to wrong prefix\n");
                    } else if attname == "xmlns" {
                        self.ns_err("redefinition of the xmlns prefix is forbidden\n");
                    } else if uri == "http://www.w3.org/2000/xmlns/" {
                        self.ns_err("reuse of the xmlns namespace name is forbidden\n");
                    } else if uri.is_empty() {
                        self.ns_err(format!(
                            "xmlns:{attname}: Empty XML namespace is not allowed\n"
                        ));
                    } else {
                        if !uri_parses(&uri) {
                            self.ns_err(format!("xmlns:{attname}: '{uri}' is not a valid URI\n"));
                        }
                        if self.ns_push(Some(attname), uri, ns_start, false) {
                            nb_ns += 1;
                        }
                    }
                } else {
                    atts.push((attname, aprefix, value));
                }
            }
            if self.cur() == b'>' || (self.cur() == b'/' && self.b(1) == b'>') {
                break;
            }
            if self.skip_blanks() == 0 {
                self.fatal_msg("attributes construct error\n");
                break;
            }
        }
        // Namespaces from default attributes.
        let defaults: Vec<(String, Option<String>, String)> = self
            .atts_default
            .get(&(localname.clone(), prefix.clone()))
            .map(|v| {
                v.iter()
                    .map(|d| (d.name.clone(), d.prefix.clone(), d.value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (n, p, v) in &defaults {
            if n == "xmlns" && p.is_none() {
                if self.ns_push(None, v.clone(), ns_start, true) {
                    nb_ns += 1;
                }
            } else if p.as_deref() == Some("xmlns")
                && self.ns_push(Some(n.clone()), v.clone(), ns_start, true)
            {
                nb_ns += 1;
            }
        }
        // Resolve attribute namespaces.
        let mut resolved: Vec<(String, Option<String>, Option<String>, String, bool)> = Vec::new();
        for (n, p, v) in atts {
            let uri = self.attr_ns(p.as_deref(), &n, &localname);
            resolved.push((n, p, uri, v, false));
        }
        // Unique attribute names.
        let mut num_dup = 0;
        let mut num_ns_err = 0;
        let nratts = resolved.len();
        for j in 0..nratts {
            let (n, p, u) = (&resolved[j].0, &resolved[j].1, &resolved[j].2);
            if p.is_some() && u.is_none() {
                continue;
            }
            if let Some(k) = (0..j).find(|&k| {
                let o = &resolved[k];
                !(o.1.is_some() && o.2.is_none()) && &o.0 == n && &o.2 == u
            }) {
                if &resolved[k].1 == p {
                    let msg = match p {
                        Some(px) => format!("Attribute {px}:{n} redefined\n"),
                        None => format!("Attribute {n} redefined\n"),
                    };
                    self.fatal_msg(msg);
                    num_dup += 1;
                } else {
                    let uri = u.clone().unwrap_or_default();
                    self.ns_err(format!("Namespaced Attribute {n} in '{uri}' redefined\n"));
                    num_ns_err += 1;
                }
            }
        }
        // Default attributes.
        let mut nbdef = 0;
        for (n, p, v) in &defaults {
            if (n == "xmlns" && p.is_none()) || p.as_deref() == Some("xmlns") {
                continue;
            }
            let uri = self.attr_ns(p.as_deref(), n, &localname);
            if let Some(k) = resolved
                .iter()
                .position(|o| &o.0 == n && o.2 == uri && !(o.1.is_some() && o.2.is_none()))
            {
                if &resolved[k].1 == p {
                    continue;
                }
                let u = uri.clone().unwrap_or_default();
                self.ns_err(format!("Namespaced Attribute {n} in '{u}' redefined\n"));
            }
            resolved.push((n.clone(), p.clone(), uri, v.clone(), true));
            nbdef += 1;
        }
        if num_dup == 0 && num_ns_err > 1 {
            for j in 0..nratts {
                if resolved[j].1.is_none() {
                    continue;
                }
                if (0..j).any(|k| resolved[k].0 == resolved[j].0 && resolved[k].1 == resolved[j].1)
                {
                    let (n, p) = (resolved[j].0.clone(), resolved[j].1.clone().unwrap());
                    self.fatal_msg(format!("Attribute {p}:{n} redefined\n"));
                }
            }
        }
        let uri = if prefix.as_deref() == Some("xml") {
            Some(XML_NS.to_string())
        } else {
            match self.ns_lookup(prefix.as_deref()) {
                Some(i) if i >= self.min_ns && !self.ns[i].1.is_empty() => {
                    Some(self.ns[i].1.clone())
                }
                _ => None,
            }
        };
        if let Some(p) = &prefix
            && uri.is_none()
        {
            self.ns_err(format!(
                "Namespace prefix {p} on {localname} is not defined\n"
            ));
        }
        if self.sax() {
            let ns_defs: Vec<(Option<String>, String)> = self.ns[self.ns.len() - nb_ns..].to_vec();
            let _ = nbdef;
            self.sax_start_element_ns(
                &localname,
                prefix.as_deref(),
                uri.as_deref(),
                ns_defs,
                resolved,
            );
        }
        Some((localname, prefix, uri, nb_ns))
    }

    /// `xmlParseAttribute2`: (name, prefix, value).
    #[allow(clippy::type_complexity)]
    fn parse_attribute2(
        &mut self,
        elem: &str,
        pref: Option<&str>,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let Some((name, prefix)) = self.parse_qname() else {
            self.fatal_msg("error parsing attribute name\n");
            return None;
        };
        let full_elem = match pref {
            Some(p) => format!("{p}:{elem}"),
            None => elem.to_string(),
        };
        let full_attr = match &prefix {
            Some(p) => format!("{p}:{name}"),
            None => name.clone(),
        };
        let normalize = self
            .atts_special
            .get(&(full_elem, full_attr))
            .is_some_and(|t| *t != AttrType::Cdata);
        self.skip_blanks();
        let value = if self.cur() == b'=' {
            self.next();
            self.skip_blanks();
            let is_ns = (prefix.is_none() && name == "xmlns") || prefix.as_deref() == Some("xmlns");
            match self.parse_att_value_internal(normalize, is_ns) {
                Some(v) => v,
                None => return Some((name, prefix, None)),
            }
        } else {
            self.fatal_msg(format!(
                "Specification mandates value for attribute {name}\n"
            ));
            return Some((name, prefix, None));
        };
        if prefix.as_deref() == Some("xml") && name == "space" {
            match value.as_str() {
                "default" => {
                    if let Some(s) = self.spaces.last_mut() {
                        *s = 0;
                    }
                }
                "preserve" => {
                    if let Some(s) = self.spaces.last_mut() {
                        *s = 1;
                    }
                }
                _ => self.warning(format!("Invalid value \"{value}\" for xml:space : \"default\" or \"preserve\" expected\n")),
            }
        }
        Some((name, prefix, Some(value)))
    }

    /// `xmlParseEndTag2`.
    fn parse_end_tag2(&mut self) {
        if self.cur() != b'<' || self.b(1) != b'/' {
            self.fatal(E::LtslashRequired);
            return;
        }
        self.skip(2);
        let tag = self.names.last().expect("tag");
        let (tname, tprefix, tline, tns) =
            (tag.name.clone(), tag.prefix.clone(), tag.line, tag.ns_nr);
        let res = self.parse_qname_and_compare(&tname, tprefix.as_deref());
        self.skip_blanks();
        if !is_byte_char(self.cur()) || self.cur() != b'>' || self.at_end() {
            self.fatal(E::GtRequired);
        } else {
            self.next1();
        }
        if let Err(name) = res {
            let name = name.unwrap_or_else(|| "unparsable".into());
            self.fatal_msg(format!(
                "Opening and ending tag mismatch: {tname} line {tline} and {name}\n"
            ));
        }
        if self.sax() {
            self.nodes.pop();
        }
        self.spaces.pop();
        if tns != 0 {
            let n = self.ns.len() - tns;
            self.ns.truncate(n);
        }
    }

    /// `xmlParseElementStart`: 0 when content follows, 1 for an empty element, -1 on error.
    fn parse_element_start(&mut self) -> i32 {
        if self.names.len() > 256 {
            let n = self.names.len();
            self.fatal_msg(format!(
                "Excessive depth in document: {n} use XML_PARSE_HUGE option\n"
            ));
            self.halt();
            return -1;
        }
        let top = *self.spaces.last().unwrap_or(&-1);
        if self.spaces.is_empty() || top == -2 {
            self.spaces.push(-1);
        } else {
            self.spaces.push(top);
        }
        let line = self.inp().line;
        let depth = self.nodes.len();
        let Some((name, prefix, _uri, nb_ns)) = self.parse_start_tag2() else {
            self.spaces.pop();
            return -1;
        };
        let pushed = self.nodes.len() > depth;
        self.names.push(Tag {
            name: name.clone(),
            prefix,
            line,
            ns_nr: nb_ns,
        });
        if self.cur() == b'/' && self.b(1) == b'>' {
            self.skip(2);
            if self.sax() || pushed {
                self.nodes.truncate(depth);
            }
            self.names.pop();
            self.spaces.pop();
            if nb_ns > 0 {
                let n = self.ns.len() - nb_ns;
                self.ns.truncate(n);
            }
            return 1;
        }
        if self.cur() == b'>' && !self.at_end() {
            self.next1();
        } else {
            self.fatal_msg(format!(
                "Couldn't find end of Start Tag {name} line {line}\n"
            ));
            self.nodes.pop();
            self.names.pop();
            self.spaces.pop();
            if nb_ns > 0 {
                let n = self.ns.len() - nb_ns;
                self.ns.truncate(n);
            }
            return -1;
        }
        0
    }

    /// `xmlParseElementEnd`.
    fn parse_element_end(&mut self) {
        if self.names.is_empty() {
            if self.cur() == b'<' && self.b(1) == b'/' {
                self.skip(2);
            }
            return;
        }
        self.parse_end_tag2();
        self.names.pop();
    }

    /// `xmlParseReference`.
    fn parse_reference(&mut self) {
        if self.cur() != b'&' {
            return;
        }
        if self.b(1) == b'#' {
            let v = self.parse_char_ref();
            if v == 0 {
                return;
            }
            if self.sax() {
                let mut b = Vec::new();
                push_char(&mut b, v);
                self.sax_text(&lossy(&b), Kind::Text);
            }
            return;
        }
        let Some(name) = self.parse_entity_ref_internal() else {
            return;
        };
        let ent = self.lookup_entity(&name, false);
        let Some(ent) = ent else {
            if !self.opts.replace_entities && self.sax() {
                self.sax_reference(&name);
            }
            return;
        };
        if !self.well_formed {
            return;
        }
        let e = match ent {
            Err(content) => {
                if self.sax() {
                    self.sax_text(content, Kind::Text);
                }
                return;
            }
            Ok(e) => e,
        };
        let et = self.doc.entities[e].etype;
        if (et == EntityType::InternalGeneral || self.opts.replace_entities)
            && !self.doc.entities[e].parsed
        {
            self.parse_entity_content(e);
        }
        if !self.sax() {
            return;
        }
        if !self.opts.replace_entities {
            self.sax_reference(&name);
        } else if !self.nodes.is_empty() {
            let kids = self.doc.entities[e].children.clone();
            let n = kids.len();
            for (i, &k) in kids.iter().enumerate() {
                let kind = self.doc.nodes[k].kind;
                if (i == 0 || i + 1 == n) && matches!(kind, Kind::Text | Kind::CData) {
                    let c = self.doc.nodes[k].content.clone();
                    self.sax_text(&c, kind);
                    continue;
                }
                let copy = self.copy_node(k, true);
                let parent = *self.nodes.last().unwrap();
                self.doc.append(parent, copy);
            }
        }
    }

    /// `xmlCtxtParseEntity`.
    fn parse_entity_content(&mut self, e: usize) {
        if self.doc.entities[e].expanding {
            self.fatal(E::EntityLoop);
            self.halt();
            self.doc.entities[e].parsed = true;
            return;
        }
        let text = match self.doc.entities[e].etype {
            EntityType::InternalGeneral => self.doc.entities[e].content.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let build = !self.nodes.is_empty();
        let old_min = self.min_ns;
        if build {
            self.min_ns = self.ns.len();
        }
        self.doc.entities[e].expanding = true;
        if self.inputs.len() > 20 {
            self.fatal_msg("Maximum entity nesting depth exceeded");
            self.halt();
            self.doc.entities[e].expanding = false;
            self.doc.entities[e].parsed = true;
            self.min_ns = old_min;
            return;
        }
        let input = self.new_input(text.as_bytes(), Some(e));
        let list = self.parse_content_chunk(input, build);
        self.doc.entities[e].expanding = false;
        self.min_ns = old_min;
        if !self.doc.entities[e].parsed {
            self.doc.entities[e].children = list;
        }
        self.doc.entities[e].parsed = true;
    }

    /// `xmlCtxtParseContent`: parse a balanced chunk under a "#root" placeholder and return its
    /// top-level nodes (empty unless well formed).
    fn parse_content_chunk(&mut self, input: Input, build: bool) -> Vec<NodeId> {
        let root = if build {
            Some(self.doc.add(Node::new(Kind::Element, "#root")))
        } else {
            None
        };
        self.inputs.push(input);
        self.names.push(Tag {
            name: "#root".into(),
            prefix: None,
            line: 0,
            ns_nr: 0,
        });
        self.spaces.push(-1);
        if let Some(r) = root {
            self.nodes.push(r);
        }
        let depth = self.nodes.len();
        self.parse_content_internal();
        if !self.at_end() {
            self.fatal(E::NotWellBalanced);
        }
        let mut list = Vec::new();
        if self.well_formed
            && let Some(r) = root
        {
            list = std::mem::take(&mut self.doc.nodes[r].children);
            for &c in &list {
                self.doc.nodes[c].parent = None;
            }
        }
        if build {
            self.nodes.truncate(depth - 1);
        }
        self.names.pop();
        self.spaces.pop();
        self.inputs.pop();
        list
    }

    /// `xmlParseContentInternal`.
    fn parse_content_internal(&mut self) {
        let old_names = self.names.len();
        let old_spaces = self.spaces.len();
        let old_nodes = self.nodes.len();
        while !self.at_end() && !self.stopped() {
            let c = self.cur();
            if c == b'<' && self.b(1) == b'?' {
                self.parse_pi();
            } else if self.starts(b"<![CDATA[") {
                self.parse_cdsect();
            } else if c == b'<' && self.b(1) == b'!' && self.b(2) == b'-' && self.b(3) == b'-' {
                self.parse_comment();
            } else if c == b'<' {
                if self.b(1) == b'/' {
                    if self.names.len() <= old_names {
                        break;
                    }
                    self.parse_element_end();
                } else {
                    self.parse_element_start();
                }
            } else if c == b'&' {
                self.parse_reference();
            } else {
                self.parse_char_data();
            }
        }
        if self.names.len() > old_names && self.at_end() && self.well_formed {
            let t = self.names.last().unwrap();
            let (n, l) = (t.name.clone(), t.line);
            self.fatal_msg(format!("Premature end of data in tag {n} line {l}\n"));
        }
        self.nodes.truncate(old_nodes.min(self.nodes.len()));
        while self.names.len() > old_names {
            let t = self.names.pop().unwrap();
            if t.ns_nr != 0 {
                let n = self.ns.len().saturating_sub(t.ns_nr);
                self.ns.truncate(n);
            }
        }
        self.spaces.truncate(old_spaces);
    }

    /// `xmlParseElement`.
    fn parse_element(&mut self) {
        if self.parse_element_start() != 0 {
            return;
        }
        self.parse_content_internal();
        if self.at_end() {
            if self.well_formed {
                let t = self.names.last();
                if let Some(t) = t {
                    let (n, l) = (t.name.clone(), t.line);
                    self.fatal_msg(format!("Premature end of data in tag {n} line {l}\n"));
                }
            }
            return;
        }
        self.parse_element_end();
    }

    // ── the XML declaration and the document ──

    fn parse_version_num(&mut self) -> Option<String> {
        let c = self.cur();
        if !c.is_ascii_digit() {
            return None;
        }
        let mut buf = vec![c];
        self.next();
        if self.cur() != b'.' {
            return None;
        }
        buf.push(b'.');
        self.next();
        while self.cur().is_ascii_digit() {
            buf.push(self.cur());
            self.next();
        }
        Some(lossy(&buf))
    }

    fn parse_version_info(&mut self) -> Option<String> {
        if !self.starts(b"version") {
            return None;
        }
        self.skip(7);
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal(E::EqualRequired);
            return None;
        }
        self.next();
        self.skip_blanks();
        let q = self.cur();
        if q == b'"' || q == b'\'' {
            self.next();
            let v = self.parse_version_num();
            if self.cur() != q {
                self.fatal(E::StringNotClosed);
            } else {
                self.next();
            }
            v
        } else {
            self.fatal(E::StringNotStarted);
            None
        }
    }

    fn parse_enc_name(&mut self) -> Option<String> {
        let c = self.cur();
        if !c.is_ascii_alphabetic() {
            self.fatal(E::EncodingName);
            return None;
        }
        let mut buf = vec![c];
        self.next();
        while matches!(self.cur(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-') {
            buf.push(self.cur());
            self.next();
        }
        Some(lossy(&buf))
    }

    fn parse_encoding_decl(&mut self) -> Option<String> {
        self.skip_blanks();
        if !self.starts(b"encoding") {
            return None;
        }
        self.skip(8);
        self.skip_blanks();
        if self.cur() != b'=' {
            self.fatal(E::EqualRequired);
            return None;
        }
        self.next();
        self.skip_blanks();
        let q = self.cur();
        let enc = if q == b'"' || q == b'\'' {
            self.next();
            let e = self.parse_enc_name();
            if self.cur() != q {
                self.fatal(E::StringNotClosed);
                return None;
            }
            self.next();
            e
        } else {
            self.fatal(E::StringNotStarted);
            None
        };
        let enc = enc?;
        self.encoding = Some(enc.clone());
        Some(enc)
    }

    fn parse_sd_decl(&mut self) -> i32 {
        let mut standalone = -2;
        self.skip_blanks();
        if self.starts(b"standalone") {
            self.skip(10);
            self.skip_blanks();
            if self.cur() != b'=' {
                self.fatal(E::EqualRequired);
                return standalone;
            }
            self.next();
            self.skip_blanks();
            let q = self.cur();
            if q == b'\'' || q == b'"' {
                self.next();
                if self.cur() == b'n' && self.b(1) == b'o' {
                    standalone = 0;
                    self.skip(2);
                } else if self.starts(b"yes") {
                    standalone = 1;
                    self.skip(3);
                } else {
                    self.fatal(E::StandaloneValue);
                }
                if self.cur() != q {
                    self.fatal(E::StringNotClosed);
                } else {
                    self.next();
                }
            } else {
                self.fatal(E::StringNotStarted);
            }
        }
        standalone
    }

    /// `xmlParseXMLDecl`.
    fn parse_xml_decl(&mut self) {
        self.standalone = -2;
        self.skip(5);
        if !is_blank(self.cur()) {
            self.fatal_msg("Blank needed after '<?xml'\n");
        }
        self.skip_blanks();
        match self.parse_version_info() {
            None => self.fatal(E::VersionMissing),
            Some(v) => {
                if v != "1.0" {
                    if v.starts_with("1.") {
                        self.warning(format!("Unsupported version '{v}'\n"));
                    } else {
                        self.fatal_msg(format!("Unsupported version '{v}'\n"));
                    }
                }
                self.version = Some(v);
            }
        }
        if !is_blank(self.cur()) {
            if self.cur() == b'?' && self.b(1) == b'>' {
                self.skip(2);
                return;
            }
            self.fatal_msg("Blank needed here\n");
        }
        self.parse_encoding_decl();
        if self.encoding.is_some() && !is_blank(self.cur()) {
            if self.cur() == b'?' && self.b(1) == b'>' {
                self.skip(2);
                return;
            }
            self.fatal_msg("Blank needed here\n");
        }
        self.skip_blanks();
        self.standalone = self.parse_sd_decl();
        self.skip_blanks();
        if self.cur() == b'?' && self.b(1) == b'>' {
            self.skip(2);
        } else if self.cur() == b'>' {
            self.fatal(E::XmldeclNotFinished);
            self.next();
        } else {
            self.fatal(E::XmldeclNotFinished);
            while !self.stopped() && !self.at_end() {
                let c = self.cur();
                self.next();
                if c == b'>' {
                    break;
                }
            }
        }
    }

    /// `xmlParseMisc`.
    fn parse_misc(&mut self) {
        while !self.stopped() {
            self.skip_blanks();
            if self.cur() == b'<' && self.b(1) == b'?' {
                self.parse_pi();
            } else if self.starts(b"<!--") {
                self.parse_comment();
            } else {
                break;
            }
        }
    }

    /// `xmlParseDocument` over `text`; the parsed document is `self.doc`.
    pub fn parse_document(&mut self, text: &[u8]) -> bool {
        let mut input = self.new_input(text, None);
        // Switching to UTF-8 skips a byte order mark.
        if text.starts_with(b"\xEF\xBB\xBF") {
            input.cur = 3;
        }
        self.inputs.push(input);
        if self.at_end() {
            self.raise(
                Domain::Parser,
                Code::DocumentEmpty,
                Level::Fatal,
                format!("{}\n", E::DocumentEmpty.msg()),
            );
            return false;
        }
        if self.starts(b"<?xml") && is_blank(self.b(5)) {
            self.parse_xml_decl();
            self.skip_blanks();
        } else {
            self.version = Some("1.0".into());
        }
        if self.sax() {
            self.doc.version = self.version.clone();
            self.doc.standalone = self.standalone;
        }
        self.parse_misc();
        if self.starts(b"<!DOCTYPE") {
            self.in_subset = 1;
            self.parse_doctype_decl();
            if self.cur() == b'[' {
                self.parse_internal_subset();
            }
            self.in_subset = 2;
            if self.sax() && self.has_external_subset && self.load_subset && self.well_formed {
                self.doc.has_ext_subset = true;
            }
            self.in_subset = 0;
            self.atts_special.retain(|_, t| *t != AttrType::Cdata);
            self.parse_misc();
        }
        if self.cur() != b'<' {
            if self.well_formed {
                self.raise(
                    Domain::Parser,
                    Code::DocumentEmpty,
                    Level::Fatal,
                    "Start tag expected, '<' not found\n".into(),
                );
            }
        } else {
            self.parse_element();
            self.parse_misc();
            if !self.at_end() && self.well_formed {
                self.fatal(E::DocumentEnd);
            }
        }
        if self.doc.encoding.is_none() {
            self.doc.encoding = self.encoding.clone();
        }
        self.well_formed
    }

    /// `xmlParseBalancedChunkMemory` into `self.doc`, under a "#root" holder appended to
    /// the document: returns the parsed top-level nodes.
    pub fn parse_balanced_chunk(&mut self, text: &[u8]) -> Vec<NodeId> {
        let input = self.new_input(text, None);
        self.parse_content_chunk(input, true)
    }

    // ── tree building (SAX2.c) ──

    fn append_child(&mut self, node: NodeId) {
        let parent = match self.in_subset {
            1 => self.doc.int_subset,
            2 => None,
            _ => Some(*self.nodes.last().unwrap_or(&DOC)),
        };
        let Some(parent) = parent else { return };
        let line = self.inputs.last().map_or(0, |i| i.line.max(0) as u32);
        if self.doc.nodes[node].kind != Kind::Text {
            self.doc.nodes[node].line = line;
        }
        self.doc.append(parent, node);
    }

    fn sax_text(&mut self, s: &str, kind: Kind) {
        let Some(&node) = self.nodes.last() else {
            return;
        };
        if let Some(&last) = self.doc.nodes[node].children.last()
            && self.doc.nodes[last].kind == kind
        {
            self.doc.nodes[last].content.push_str(s);
            return;
        }
        let mut n = Node::new(kind, if kind == Kind::Text { "text" } else { "" });
        n.content = s.to_string();
        n.line = self.inputs.last().map_or(0, |i| i.line.max(0) as u32);
        let id = self.doc.add(n);
        self.doc.append(node, id);
    }

    fn sax_comment(&mut self, s: &str) {
        let mut n = Node::new(Kind::Comment, "comment");
        n.content = s.to_string();
        let id = self.doc.add(n);
        self.append_child(id);
    }

    fn sax_pi(&mut self, target: &str, data: Option<&str>) {
        let mut n = Node::new(Kind::Pi, target);
        n.content = data.unwrap_or("").to_string();
        n.no_content = data.is_none();
        let id = self.doc.add(n);
        self.append_child(id);
    }

    fn sax_reference(&mut self, name: &str) {
        let n = Node::new(Kind::EntityRef, name);
        let id = self.doc.add(n);
        self.append_child(id);
    }

    fn sax_internal_subset(
        &mut self,
        name: Option<&str>,
        external_id: Option<String>,
        system_id: Option<String>,
    ) {
        let mut n = Node::new(Kind::Dtd, name.unwrap_or(""));
        n.decl = Decl::Dtd {
            external_id,
            system_id,
        };
        let id = self.doc.add(n);
        // xmlCreateIntSubset puts the DTD before the root element.
        self.doc.nodes[id].parent = Some(DOC);
        let at = self.doc.nodes[DOC]
            .children
            .iter()
            .position(|&c| self.doc.nodes[c].kind == Kind::Element);
        match at {
            Some(i) => self.doc.nodes[DOC].children.insert(i, id),
            None => self.doc.nodes[DOC].children.push(id),
        }
        self.doc.int_subset = Some(id);
    }

    fn sax_entity_decl(
        &mut self,
        name: &str,
        etype: EntityType,
        external_id: Option<String>,
        system_id: Option<String>,
        content: Option<String>,
    ) {
        if self.in_subset != 1 || self.doc.int_subset.is_none() {
            return;
        }
        let general = !matches!(
            etype,
            EntityType::InternalParameter | EntityType::ExternalParameter
        );
        if general && let Some(p) = Doc::predefined(name) {
            let c = p.as_bytes()[0];
            let valid = match (etype, &content) {
                (EntityType::InternalGeneral, Some(v)) => {
                    let vb = v.as_bytes();
                    if vb.len() == 1 && vb[0] == c && matches!(c, b'>' | b'\'' | b'"') {
                        true
                    } else if vb.starts_with(b"&#x") {
                        v[3..].eq_ignore_ascii_case(&format!("{:02X};", c))
                    } else if vb.starts_with(b"&#") {
                        v[2..] == format!("{:02};", c)
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !valid {
                self.warning(format!(
                    "Invalid redeclaration of predefined entity '{name}'"
                ));
                return;
            }
        }
        let table = if general {
            &self.doc.general
        } else {
            &self.doc.parameter
        };
        if table.contains_key(name) {
            return;
        }
        let idx = self.doc.entities.len();
        self.doc.entities.push(Entity {
            name: name.to_string(),
            etype,
            external_id,
            system_id,
            content,
            orig: None,
            children: Vec::new(),
            expanding: false,
            parsed: false,
        });
        if general {
            self.doc.general.insert(name.to_string(), idx);
        } else {
            self.doc.parameter.insert(name.to_string(), idx);
        }
        let mut n = Node::new(Kind::EntityDecl, name);
        n.decl = Decl::Entity(idx);
        let id = self.doc.add(n);
        let dtd = self.doc.int_subset.unwrap();
        self.doc.append(dtd, id);
    }

    fn sax_attribute_decl(
        &mut self,
        elem: &str,
        full: &str,
        atype: AttrType,
        def: AttrDefault,
        default: Option<String>,
        tree: Vec<String>,
    ) {
        let Some(dtd) = self.doc.int_subset else {
            return;
        };
        if self.in_subset != 1 {
            return;
        }
        let (name, prefix) = split_qname(full);
        let key = (elem.to_string(), name.clone(), prefix.clone());
        if self.doc.attr_decls.contains_key(&key) {
            return;
        }
        let mut n = Node::new(Kind::AttributeDecl, name);
        n.decl = Decl::Attribute {
            elem: elem.to_string(),
            prefix,
            atype,
            def,
            default,
            tree,
        };
        let id = self.doc.add(n);
        self.doc.attr_decls.insert(key, id);
        self.doc.append(dtd, id);
    }

    fn sax_element_decl(&mut self, full: &str, etype: ElemType, content: Option<ElemContent>) {
        let Some(dtd) = self.doc.int_subset else {
            return;
        };
        if self.in_subset != 1 {
            return;
        }
        let (name, prefix) = split_qname(full);
        let key = (name.clone(), prefix.clone());
        if self.doc.elem_decls.contains_key(&key) {
            self.raise(
                Domain::Dtd,
                Code::Other,
                Level::Error,
                format!("Redefinition of element {full}\n"),
            );
            return;
        }
        let mut n = Node::new(Kind::ElementDecl, name);
        n.decl = Decl::Element {
            etype,
            content,
            prefix,
        };
        let id = self.doc.add(n);
        self.doc.elem_decls.insert(key, id);
        self.doc.append(dtd, id);
    }

    #[allow(clippy::type_complexity)]
    fn sax_start_element_ns(
        &mut self,
        localname: &str,
        prefix: Option<&str>,
        uri: Option<&str>,
        ns_defs: Vec<(Option<String>, String)>,
        atts: Vec<(String, Option<String>, Option<String>, String, bool)>,
    ) {
        let name = match (prefix, uri) {
            (Some(p), None) => format!("{p}:{localname}"),
            _ => localname.to_string(),
        };
        let mut n = Node::new(Kind::Element, name);
        for (p, u) in &ns_defs {
            let ns = Ns {
                prefix: p.clone(),
                href: Some(u.clone()),
            };
            if uri.is_some() && p.as_deref() == prefix && n.ns.is_none() {
                n.ns = Some(ns.clone());
            }
            n.ns_defs.push(ns);
        }
        let id = self.doc.add(n);
        self.append_child(id);
        self.nodes.push(id);
        if let Some(u) = uri
            && self.doc.nodes[id].ns.is_none()
        {
            self.doc.nodes[id].ns = Some(Ns {
                prefix: prefix.map(str::to_string),
                href: Some(u.to_string()),
            });
        }
        for (aname, aprefix, auri, value, defaulted) in atts {
            if defaulted && !self.opts.dtd_attr {
                continue;
            }
            let (aname, ns) = match (&aprefix, &auri) {
                (Some(p), None) => (format!("{p}:{aname}"), None),
                (Some(p), Some(u)) => (
                    aname,
                    Some(Ns {
                        prefix: Some(p.clone()),
                        href: Some(u.clone()),
                    }),
                ),
                _ => (aname, None),
            };
            let mut a = Node::new(Kind::Attribute, aname);
            a.ns = ns;
            let aid = self.doc.add(a);
            self.doc.nodes[aid].parent = Some(id);
            let kids = self.attr_children(&value);
            for k in kids {
                self.doc.append(aid, k);
            }
            self.doc.nodes[id].attrs.push(aid);
        }
    }

    /// The attribute's children: text, with `&name;` references kept as nodes when entities
    /// aren't substituted (`xmlNodeParseContent`).
    fn attr_children(&mut self, value: &str) -> Vec<NodeId> {
        if self.opts.replace_entities || !value.contains('&') {
            return vec![self.doc.text(value)];
        }
        let mut out = Vec::new();
        let mut text = String::new();
        let b = value.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'&'
                && let Some(end) = value[i..].find(';')
            {
                let r = &value[i + 1..i + end];
                if let Some(num) = r.strip_prefix('#') {
                    let v = if let Some(h) = num.strip_prefix('x') {
                        u32::from_str_radix(h, 16).ok()
                    } else {
                        num.parse().ok()
                    };
                    if let Some(c) = v.and_then(char::from_u32) {
                        text.push(c);
                    }
                } else if let Some(p) = Doc::predefined(r) {
                    text.push_str(p);
                } else {
                    if !text.is_empty() {
                        out.push(self.doc.text(&std::mem::take(&mut text)));
                    }
                    out.push(self.doc.add(Node::new(Kind::EntityRef, r)));
                }
                i += end + 1;
                continue;
            }
            let ch = value[i..].chars().next().unwrap();
            text.push(ch);
            i += ch.len_utf8();
        }
        if !text.is_empty() || out.is_empty() {
            out.push(self.doc.text(&text));
        }
        out
    }

    /// `xmlDocCopyNode(node, doc, 1)` within the same document.
    pub fn copy_node(&mut self, id: NodeId, recursive: bool) -> NodeId {
        copy_node(&mut self.doc, id, recursive)
    }
}

/// Deep copy of `id` (detached).
pub fn copy_node(doc: &mut Doc, id: NodeId, recursive: bool) -> NodeId {
    let mut n = doc.nodes[id].clone();
    n.parent = None;
    let kids = std::mem::take(&mut n.children);
    let attrs = std::mem::take(&mut n.attrs);
    let new = doc.add(n);
    for a in attrs {
        let c = copy_node(doc, a, true);
        doc.nodes[c].parent = Some(new);
        doc.nodes[new].attrs.push(c);
    }
    if recursive {
        for k in kids {
            let c = copy_node(doc, k, true);
            doc.append(new, c);
        }
    }
    new
}

pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

fn split_qname(s: &str) -> (String, Option<String>) {
    // xmlSplitQName3: "a:b" → (b, a); a leading or trailing colon isn't a prefix.
    match s.find(':') {
        Some(i) if i > 0 && i + 1 < s.len() => (s[i + 1..].to_string(), Some(s[..i].to_string())),
        _ => (s.to_string(), None),
    }
}

fn normalize_space(s: &str) -> String {
    s.split(' ')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn uri_parses(u: &str) -> bool {
    // xmlParseURISafe rejects spaces and stray characters outside RFC 3986.
    !u.bytes().any(|c| {
        c <= 0x20
            || matches!(
                c,
                b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}' | 0x7f
            )
    })
}

fn uri_has_scheme(u: &str) -> bool {
    let mut it = u.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in it {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// `xmlParserPrintFileContextInternal`: the input line around the cursor (at most 80
/// bytes) and a caret line under the position.
fn file_context(input: &Input) -> String {
    let buf = &input.buf;
    let at = |k: usize| if k < input.end { buf[k] } else { 0 };
    let base = 0usize;
    let mut cur = input.cur;
    while cur > base && (at(cur) == b'\n' || at(cur) == b'\r') {
        cur -= 1;
    }
    let mut n = 0;
    while n < 80 && cur > base && at(cur) != b'\n' && at(cur) != b'\r' {
        cur -= 1;
        n += 1;
    }
    if n > 0 && (at(cur) == b'\n' || at(cur) == b'\r') {
        cur += 1;
    } else {
        while cur < input.cur && (at(cur) & 0xc0) == 0x80 {
            cur += 1;
        }
    }
    let col = input.cur.saturating_sub(cur);
    let start = cur;
    let mut n = 0;
    while at(cur) != 0 && at(cur) != b'\n' && at(cur) != b'\r' {
        let len = utf8_len(at(cur));
        // xmlGetUTF8Char: the line stops at invalid UTF-8.
        if n + len > 80 || std::str::from_utf8(&buf[cur..(cur + len).min(input.end)]).is_err() {
            break;
        }
        cur += len;
        n += len;
    }
    let mut content: Vec<u8> = buf[start..start + n].to_vec();
    let line1 = lossy(&content);
    let mut k = 0;
    let mut i = 0;
    while k < col
        && {
            let ok = i < 79;
            k += 1;
            ok
        }
        && i < content.len()
    {
        if content[i] != b'\t' {
            content[i] = b' ';
        }
        i += 1;
    }
    content.truncate(i);
    content.push(b'^');
    format!("{line1}\n{}\n", lossy(&content))
}
