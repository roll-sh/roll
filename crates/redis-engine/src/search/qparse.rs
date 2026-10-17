//! RediSearch's query parsers (`RSQuery_ParseRaw_v1`/`_v2`): the scanner's tokens fed to the
//! lemon automaton of each dialect's grammar ([`super::qgrammar`]), with the grammar's actions
//! building the query tree ([`super::qast`]).
//!
//! The C keeps the query in one buffer that tokens point into, and the field-name actions
//! unescape their text in place; tokens here are spans of that buffer, so a later "near" text
//! reads the rewritten bytes as the C does. Scanning stops after the token whose parse left an
//! error, and the end of input is only fed when there is none.

use super::error::{Code, QErr};
use super::lalr::{Handler, Parser, RuleId, Symbol};
use super::qast::{self, Attr, GeoParam, Kind, Node, NumParam, Params, Target, TextTok, Val};
use super::qgrammar::{v1rule as r1, v1tok as t1, v2rule as r2, v2tok as t2};
use super::qlex::{self, Dialect, QType, Tk, Token};
use super::spec::{self, Spec};
use super::strconv::fmt_f;
use super::tokenize::StopWords;
use crate::util::cstr;

/// What the parse reads besides the query (`QueryParseCtx`'s options).
pub struct ParseOpts<'a> {
    /// The index's stopwords, unless NOSTOPWORDS.
    pub stopwords: Option<&'a StopWords>,
    pub params: Option<&'a Params>,
}

/// A successful parse: the tree (none for an empty query) and its counters.
#[derive(Debug)]
pub struct Parsed {
    pub root: Option<Node>,
    pub num_tokens: usize,
    pub num_params: usize,
}

/// A field name in a query (`FieldName`): its token and the field, by schema position.
#[derive(Clone, Debug)]
struct FieldName {
    tok: Token,
    fs: Option<usize>,
}

/// The grammar's semantic values (lemon's `YYMINORTYPE`).
#[derive(Debug)]
enum V {
    Empty,
    Tok(Token),
    Node(Option<Node>),
    /// `RangeNumber`.
    Num(f64, bool),
    Attr(Attr),
    Attrs(Option<Vec<Attr>>),
    Field(FieldName),
    /// v2's `modifierlist`.
    Fields(Option<Vec<FieldName>>),
    /// v1's `modifierlist`: the field names.
    Names(Vec<Vec<u8>>),
    NumRange(Option<NumParam>),
    Geo(Option<GeoParam>),
    /// A vector query's raw `NAME value` and whether the value names a parameter.
    VecAttr(Vec<u8>, Vec<u8>, bool),
    VecAttrs(VecParams),
}

/// A vector query's raw parameters and which name a query parameter.
type VecParams = (Vec<(Vec<u8>, Vec<u8>)>, Vec<bool>);

fn blank() -> Token {
    Token { kind: Tk::Term, off: 0, len: 0, pos: 0, numval: 0.0, sign: 1, qtype: QType::Term }
}

impl V {
    fn node(self) -> Option<Node> {
        match self {
            V::Node(n) => n,
            _ => None,
        }
    }

    fn tok(self) -> Token {
        match self {
            V::Tok(t) => t,
            _ => blank(),
        }
    }

    fn num(self) -> (f64, bool) {
        match self {
            V::Num(n, i) => (n, i),
            _ => (0.0, false),
        }
    }

    fn field(self) -> FieldName {
        match self {
            V::Field(f) => f,
            _ => FieldName { tok: blank(), fs: None },
        }
    }

    fn attrs(self) -> Option<Vec<Attr>> {
        match self {
            V::Attrs(a) => a,
            _ => None,
        }
    }
}

struct Ctx<'a> {
    /// The query and its terminating NUL.
    buf: Vec<u8>,
    spec: &'a Spec,
    opts: &'a ParseOpts<'a>,
    v2: bool,
    err: Option<QErr>,
    root: Option<Node>,
    num_tokens: usize,
    num_params: usize,
}

/// `unescapen`: drop each backslash that escapes punctuation or a space within the span,
/// rewriting the buffer in place; the new length.
fn unescapen(buf: &mut [u8], off: usize, len: usize) -> usize {
    let end = (off + len).min(buf.len());
    let mut dst = off;
    let mut src = off;
    while src < end {
        if buf[src] == b'\\' && src + 1 < end && (buf[src + 1].is_ascii_punctuation() || is_space(buf[src + 1])) {
            src += 1;
            continue;
        }
        buf[dst] = buf[src];
        dst += 1;
        src += 1;
    }
    dst - off
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `HiddenString_CompareC(name, s, len) == 0`: `strncmp` over the shorter length, then equal
/// lengths.
fn name_eq(name: &[u8], s: &[u8]) -> bool {
    if name.len() != s.len() {
        return false;
    }
    for (a, b) in name.iter().zip(s) {
        if a != b {
            return false;
        }
        if *a == 0 {
            return true;
        }
    }
    true
}

/// `IndexSpec_GetFieldWithLength`.
fn field_with_len(spec: &Spec, s: &[u8]) -> Option<usize> {
    spec.fields.iter().position(|f| name_eq(&f.name, s))
}

/// `toksep2` over a C string: the next token (possibly empty) and the rest, if any.
fn toksep2(s: &[u8]) -> (&[u8], Option<&[u8]>) {
    const SEPS: &[u8] = b" \t,./(){}[]:;~!@#%^&*-=+|'`\"<>?";
    let mut escaped = false;
    for (i, &c) in s.iter().enumerate() {
        if c == 0 {
            return (&s[..i], None);
        }
        if SEPS.contains(&c) && !escaped {
            let rest = &s[i + 1..];
            return (&s[..i], if rest.first().is_none_or(|&c| c == 0) { None } else { Some(rest) });
        }
        escaped = !escaped && c == b'\\';
    }
    (s, None)
}

impl Ctx<'_> {
    fn fail(&mut self, e: QErr) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }

    fn text(&self, t: &Token) -> Vec<u8> {
        t.text(&self.buf).to_vec()
    }

    /// `reportSyntaxError`: the token's text for terms, its number for numbers, else only
    /// its offset.
    fn report(&mut self, t: &Token, msg: &str) {
        let user = match t.qtype {
            QType::Term | QType::TermCase => format!(" at offset {} near {}", t.pos as i32, qast::lossy(t.text(&self.buf))),
            QType::Numeric => format!(" at offset {} near {}", t.pos as i32, fmt_f(t.numval)),
            _ => format!(" at offset {}", t.pos as i32),
        };
        self.fail(QErr::user(Code::Syntax, msg, user));
    }

    fn wrong_type(&mut self, f: &FieldName, ty: &str) {
        let t = f.tok.clone();
        self.report(&t, &format!("Expected a {ty} field"));
    }

    fn field_is(&self, fs: Option<usize>, t: u8) -> bool {
        fs.and_then(|i| self.spec.fields.get(i)).is_some_and(|f| f.is(t))
    }

    fn token_node(&mut self, t: &Token) -> Node {
        self.num_tokens += 1;
        let t = self.resolved(t);
        qast::token_node(&t, &mut self.num_params)
    }

    /// `NewTokenNode(ctx, s, -1)`.
    fn plain_token(&mut self, s: Vec<u8>) -> Node {
        self.num_tokens += 1;
        Node::token(s)
    }

    fn param_target(&mut self, params: &mut Vec<qast::Param>, target: Target, t: &Token) -> Option<Val> {
        let t = self.resolved(t);
        qast::set_param(params, &mut self.num_params, target, &t)
    }

    fn numeric(&mut self, min: Option<&Token>, max: Option<&Token>, min_inclusive: bool, max_inclusive: bool) -> NumParam {
        let min = min.map(|t| self.resolved(t));
        let max = max.map(|t| self.resolved(t));
        qast::numeric_param(min.as_ref(), max.as_ref(), min_inclusive, max_inclusive, &mut self.num_params)
    }

    /// The token with its text read out of the buffer, for the [`qast`] constructors.
    fn resolved(&self, t: &Token) -> TextTok {
        TextTok { tok: t.clone(), s: self.text(t) }
    }

    /// The span itself (`s`, `len`), NULs and all.
    fn raw(&self, t: &Token) -> Vec<u8> {
        let a = t.off.min(self.buf.len());
        self.buf[a..(t.off + t.len).min(self.buf.len())].to_vec()
    }
}

/// `intersection_step` (v2): a plain phrase over all fields takes the other node, else both go
/// under a new phrase.
fn intersection_step(b: Option<Node>, c: Option<Node>) -> Option<Node> {
    match (b, c) {
        (Some(b), Some(c)) => {
            let open = |n: &Node| n.is_phrase(false) && n.opts.field_mask == qast::MASK_ALL;
            let (mut a, child) = if open(&b) {
                (b, c)
            } else if open(&c) {
                (c, b)
            } else {
                let mut a = Node::phrase(false);
                a.add_child(b);
                (a, c)
            };
            a.add_child(child);
            Some(a)
        }
        (b, c) => b.or(c),
    }
}

/// `union_step` (v2).
fn union_step(b: Option<Node>, c: Option<Node>) -> Option<Node> {
    match (b, c) {
        (Some(b), Some(c)) => {
            let open = |n: &Node| n.is_union() && n.opts.field_mask == qast::MASK_ALL;
            let (mut a, child) = if open(&b) {
                (b, c)
            } else if open(&c) {
                (c, b)
            } else {
                let mut a = Node::new(Kind::Union);
                a.add_child(b);
                (a, c)
            };
            a.add_child(child);
            Some(a)
        }
        (b, c) => b.or(c),
    }
}

/// `not_step`: NOT(NOT(x)) is x.
fn not_step(child: Option<Node>) -> Option<Node> {
    let child = child?;
    if child.is_not() {
        return child.children.into_iter().next();
    }
    Some(Node::with_child(Kind::Not, child))
}

fn optional(child: Option<Node>) -> Option<Node> {
    child.map(|c| Node::with_child(Kind::Optional, c))
}

/// v1's `expr expr`: a plain phrase on the left takes the right.
fn v1_and(b: Option<Node>, c: Option<Node>) -> Option<Node> {
    match (b, c) {
        (Some(b), Some(c)) => {
            let mut a = if b.is_phrase(false) && b.opts.field_mask == qast::MASK_ALL {
                b
            } else {
                let mut a = Node::phrase(false);
                a.add_child(b);
                a
            };
            a.add_child(c);
            Some(a)
        }
        (b, c) => b.or(c),
    }
}

/// v1's unions: a union over all fields on the left takes the right.
fn v1_or(b: Option<Node>, c: Option<Node>) -> Option<Node> {
    match (b, c) {
        (Some(b), Some(c)) => {
            let mut a = if b.is_union() && b.opts.field_mask == qast::MASK_ALL {
                b
            } else {
                let mut a = Node::new(Kind::Union);
                a.add_child(b);
                a
            };
            a.add_child(c);
            Some(a)
        }
        (b, c) => b.or(c),
    }
}

impl Ctx<'_> {
    /// `attribute ::= ATTRIBUTE COLON param_term` (v2 resolves a `$param` value).
    fn attribute(&mut self, name: &Token, value: &Token) -> Attr {
        let mut v = self.text(value);
        let mut vallen = value.len;
        if self.v2 && value.qtype == QType::ParamTerm {
            match qast::param_get(self.opts.params, &v) {
                Ok(found) => {
                    v = cstr(found).to_vec();
                    vallen = found.len();
                }
                Err(e) => self.fail(e),
            }
        }
        Attr { name: self.raw(name), value: v, vallen }
    }

    fn apply_attrs(&mut self, n: &mut Option<Node>, attrs: Option<Vec<Attr>>) {
        if let (Some(n), Some(attrs)) = (n.as_mut(), attrs)
            && let Err(e) = qast::apply_attributes(n, &attrs)
        {
            self.fail(e);
        }
    }

    /// `text_expr ::= EXACT`: the quoted text split at the parser's separators, each part
    /// normalized.
    fn exact(&mut self, t: &Token) -> Node {
        let text = self.text(t);
        let mut a = Node::phrase(false);
        let mut rest = Some(text.as_slice());
        while let Some(s) = rest {
            let (tok, r) = toksep2(s);
            if !tok.is_empty() {
                let n = self.plain_token(qast::normalize(tok));
                a.add_child(n);
            }
            rest = r;
        }
        a.kind = Kind::Phrase { exact: true };
        a.opts.flags |= qast::N_VERBATIM;
        a
    }

    /// `QUOTE ATTRIBUTE QUOTE`: the `$name` text itself, verbatim.
    fn quoted_attr(&mut self, t: &Token) -> Node {
        let mut s = vec![b'$'];
        s.extend_from_slice(&self.text(t));
        let mut n = self.plain_token(qast::normalize(&s));
        n.opts.flags |= qast::N_VERBATIM;
        n
    }

    fn prefix(&mut self, t: &Token, prefix: bool, suffix: bool) -> Node {
        self.num_tokens += 1;
        let tt = self.resolved(t);
        qast::prefix_node(&tt, prefix, suffix, &mut self.num_params)
    }

    fn fuzzy(&mut self, t: &Token, d: u8) -> Node {
        self.num_tokens += 1;
        let tt = self.resolved(t);
        qast::fuzzy_node(&tt, d, &mut self.num_params)
    }

    /// `modifier ::= MODIFIER`: unescape the name in place; v2 looks the field up.
    fn modifier(&mut self, mut t: Token) -> FieldName {
        t.len = unescapen(&mut self.buf, t.off, t.len);
        let mut fs = None;
        if self.v2 {
            fs = field_with_len(self.spec, &self.buf[t.off..t.off + t.len]);
            if fs.is_none() {
                self.report(&t, "Unknown field");
            }
        }
        FieldName { tok: t, fs }
    }

    /// v2's `modifierlist` items: TEXT fields only.
    fn list_field(&mut self, c: Token) -> Option<FieldName> {
        let text = self.text(&c);
        let second = FieldName { fs: field_with_len(self.spec, &text), tok: c };
        if second.fs.is_none() {
            self.report(&second.tok, "Unknown field");
            None
        } else if !self.field_is(second.fs, spec::T_TEXT) {
            self.wrong_type(&second, "TEXT");
            None
        } else {
            Some(second)
        }
    }

    /// `@f <op> num`: which bounds `c` gives, their inclusiveness, and whether it's negated.
    fn numeric_op(&mut self, b: FieldName, c: &Token, (min, max): (bool, bool), (min_inclusive, max_inclusive): (bool, bool), not: bool) -> Option<Node> {
        if !self.field_is(b.fs, spec::T_NUMERIC) {
            self.wrong_type(&b, "NUMERIC");
            return None;
        }
        let qp = self.numeric(min.then_some(c), max.then_some(c), min_inclusive, max_inclusive);
        let e = qast::numeric_node(qp, b.fs);
        if not { not_step(Some(e)) } else { Some(e) }
    }

    fn vector_node(&mut self, knn: bool, value: &Token, vec: &Token) -> Node {
        let v = self.resolved(value);
        let b = self.resolved(vec);
        qast::vector_node(knn, &v, &b, &mut self.num_params)
    }

    /// `vector_query`'s score field: a fresh parameter slot for the `AS` name.
    fn score_field(&mut self, n: &mut Option<Node>, d: &Token) {
        let Some(n) = n.as_mut() else { return };
        if let Kind::Vector(vq) = &mut n.kind {
            vq.score_field = None;
        }
        let mut params = std::mem::take(&mut n.params);
        if let Some(Val::Str(s)) = self.param_target(&mut params, Target::ScoreField, d)
            && let Kind::Vector(vq) = &mut n.kind
        {
            vq.score_field = Some(s);
        }
        n.params = params;
    }

    fn vector_params(n: &mut Option<Node>, attrs: VecParams) {
        if let Some(Node { kind: Kind::Vector(vq), .. }) = n.as_mut() {
            vq.params = attrs.0;
            vq.need_resolve = attrs.1;
        }
    }

    fn vector_root(&mut self, b: Option<Node>, a: Option<Node>, attrs: Option<Option<Vec<Attr>>>, star: bool) {
        let mut b = b;
        if star && let Some(Node { kind: Kind::Vector(vq), .. }) = b.as_mut() {
            vq.by_score = true;
        }
        if let Some(attrs) = attrs {
            self.apply_attrs(&mut b, attrs);
        }
        if let (Some(b), Some(a)) = (b.as_mut(), a) {
            b.add_child(a);
        }
        self.root = b;
    }
}

impl Handler<V> for Ctx<'_> {
    fn reduce(&mut self, rule: RuleId, rhs: Vec<V>) -> V {
        if self.v2 { self.reduce_v2(rule, rhs) } else { self.reduce_v1(rule, rhs) }
    }

    fn syntax_error(&mut self, _token: Symbol, value: &V) {
        let t = match value {
            V::Tok(t) => t.clone(),
            _ => blank(),
        };
        let near = qast::lossy(t.text(&self.buf));
        self.fail(QErr::user(Code::Syntax, "Syntax error", format!(" at offset {} near {near}", t.pos as i32)));
    }

    fn stack_overflow(&mut self) {
        if self.v2 {
            self.fail(QErr::new(Code::Syntax, "Parser stack overflow. Try moving nested parentheses more to the left"));
        }
    }
}

impl Ctx<'_> {
    fn reduce_v2(&mut self, rule: RuleId, rhs: Vec<V>) -> V {
        let mut it = rhs.into_iter();
        let mut next = move || it.next().unwrap_or(V::Empty);
        match rule {
            r2::QUERY_EXPR => {
                self.root = next().node();
                V::Empty
            }
            r2::QUERY_EMPTY => {
                self.root = None;
                V::Empty
            }
            r2::QUERY_STAR => {
                self.root = Some(Node::new(Kind::Wildcard));
                V::Empty
            }
            r2::STAR_STAR | r2::STAR_LP_STAR_RP | r2::AS_AS_T => V::Empty,
            r2::EXPR_TEXT_EXPR | r2::EXPR_UNION | r2::TEXT_EXPR_TEXT_UNION | r2::TEXT_EXPR_AFFIX | r2::TEXT_EXPR_VERBATIM => next(),
            r2::EXPR_LP_EXPR_RP | r2::TEXT_EXPR_LP_TEXT_EXPR_RP => {
                next();
                next()
            }
            r2::EXPR_EXPR_EXPR | r2::EXPR_TEXT_EXPR_EXPR | r2::EXPR_EXPR_TEXT_EXPR | r2::TEXT_EXPR_TEXT_EXPR_TEXT_EXPR => {
                let b = next().node();
                let c = next().node();
                V::Node(intersection_step(b, c))
            }
            r2::UNION_EXPR_OR_EXPR
            | r2::UNION_UNION_OR_EXPR
            | r2::UNION_TEXT_EXPR_OR_EXPR
            | r2::UNION_EXPR_OR_TEXT_EXPR
            | r2::TEXT_UNION_TEXT_EXPR_OR_TEXT_EXPR
            | r2::TEXT_UNION_TEXT_UNION_OR_TEXT_EXPR => {
                let b = next().node();
                next();
                let c = next().node();
                V::Node(union_step(b, c))
            }
            r2::EXPR_MODIFIER_COLON_TEXT_EXPR => {
                let b = next().field();
                next();
                let c = next().node();
                if !self.field_is(b.fs, spec::T_TEXT) {
                    self.wrong_type(&b, "TEXT");
                    return V::Node(None);
                }
                let Some(mut c) = c else { return V::Node(None) };
                let f = &self.spec.fields[b.fs.expect("a TEXT field")];
                c.set_field_mask(qast::bit_of(f));
                V::Node(Some(c))
            }
            r2::EXPR_MODIFIERLIST_COLON_TEXT_EXPR => {
                let b = match next() {
                    V::Fields(f) => f,
                    _ => None,
                };
                next();
                let Some(mut c) = next().node() else { return V::Node(None) };
                let mut mask = 0;
                for f in b.iter().flatten() {
                    if let Some(f) = f.fs.and_then(|i| self.spec.fields.get(i)) {
                        mask |= qast::bit_of(f);
                    }
                }
                c.set_field_mask(mask);
                V::Node(Some(c))
            }
            r2::ATTRIBUTE_ATTRIBUTE_COLON_PARAM_TERM => {
                let b = next().tok();
                next();
                let c = next().tok();
                V::Attr(self.attribute(&b, &c))
            }
            r2::ATTRIBUTE_LIST_ATTRIBUTE => match next() {
                V::Attr(a) => V::Attrs(Some(vec![a])),
                _ => V::Attrs(None),
            },
            r2::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE => {
                let mut b = next().attrs();
                next();
                if let V::Attr(c) = next() {
                    // The C appends to a NULL list here (`=>{;…}`), which crashes it.
                    b.get_or_insert_with(Vec::new).push(c);
                }
                V::Attrs(b)
            }
            r2::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON => next(),
            r2::ATTRIBUTE_LIST_EMPTY => V::Attrs(None),
            r2::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB | r2::TEXT_EXPR_TEXT_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB => {
                let mut b = next().node();
                next();
                next();
                let c = next().attrs();
                self.apply_attrs(&mut b, c);
                V::Node(b)
            }
            r2::TEXT_EXPR_EXACT => {
                let b = next().tok();
                V::Node(Some(self.exact(&b)))
            }
            r2::TEXT_EXPR_QUOTE_ATTRIBUTE_QUOTE | r2::TEXT_EXPR_SQUOTE_ATTRIBUTE_SQUOTE => {
                next();
                let b = next().tok();
                V::Node(Some(self.quoted_attr(&b)))
            }
            r2::TEXT_EXPR_PARAM_TERM => {
                let b = next().tok();
                if b.qtype == QType::Term && self.opts.stopwords.is_some_and(|s| s.contains(b.text(&self.buf))) {
                    return V::Node(None);
                }
                V::Node(Some(self.token_node(&b)))
            }
            r2::TERMLIST_PARAM_TERM_PARAM_TERM => {
                let b = next().tok();
                let c = next().tok();
                let mut a = Node::phrase(false);
                let nb = self.token_node(&b);
                a.add_child(nb);
                let nc = self.token_node(&c);
                a.add_child(nc);
                V::Node(Some(a))
            }
            r2::TERMLIST_TERMLIST_PARAM_TERM => {
                let mut b = next().node();
                let c = next().tok();
                let nc = self.token_node(&c);
                if let Some(b) = b.as_mut() {
                    b.add_child(nc);
                }
                V::Node(b)
            }
            r2::EXPR_MINUS_EXPR | r2::TEXT_EXPR_MINUS_TEXT_EXPR => {
                next();
                V::Node(not_step(next().node()))
            }
            r2::EXPR_TILDE_EXPR | r2::TEXT_EXPR_TILDE_TEXT_EXPR => {
                next();
                V::Node(optional(next().node()))
            }
            r2::AFFIX_PREFIX => V::Node(Some(self.prefix(&next().tok(), true, false))),
            r2::AFFIX_SUFFIX => V::Node(Some(self.prefix(&next().tok(), false, true))),
            r2::AFFIX_CONTAINS => V::Node(Some(self.prefix(&next().tok(), true, true))),
            r2::VERBATIM_WILDCARD => {
                let b = next().tok();
                self.num_tokens += 1;
                let tt = self.resolved(&b);
                V::Node(Some(qast::wildcard_node(&tt, &mut self.num_params)))
            }
            r2::TEXT_EXPR_PERCENT_PARAM_TERM_PERCENT => {
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 1)))
            }
            r2::TEXT_EXPR_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT => {
                next();
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 2)))
            }
            r2::TEXT_EXPR_PERCENT_PERCENT_PERCENT_PARAM_TERM_PERCENT_PERCENT_PERCENT => {
                next();
                next();
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 3)))
            }
            r2::MODIFIER_MODIFIER => V::Field(self.modifier(next().tok())),
            r2::MODIFIERLIST_MODIFIER_OR_TERM => {
                let b = next().field();
                next();
                let c = next().tok();
                if !self.field_is(b.fs, spec::T_TEXT) {
                    self.wrong_type(&b, "TEXT");
                    return V::Fields(None);
                }
                V::Fields(self.list_field(c).map(|second| vec![b, second]))
            }
            r2::MODIFIERLIST_MODIFIERLIST_OR_TERM => {
                let b = match next() {
                    V::Fields(f) => f,
                    _ => None,
                };
                next();
                let c = next().tok();
                match self.list_field(c) {
                    Some(second) => {
                        let mut b = b.unwrap_or_default();
                        b.push(second);
                        V::Fields(Some(b))
                    }
                    None => V::Fields(None),
                }
            }
            r2::EXPR_ISMISSING_LP_MODIFIER_RP => {
                next();
                next();
                let b = next().field();
                let indexes = b.fs.and_then(|i| self.spec.fields.get(i)).is_some_and(|f| f.has(spec::F_INDEX_MISSING));
                if !indexes {
                    self.report(&b.tok, "'ismissing' requires defining the field with 'INDEXMISSING'");
                    return V::Node(None);
                }
                V::Node(Some(Node::new(Kind::Missing { field: b.fs })))
            }
            r2::EXPR_MODIFIER_COLON_LB_TAG_LIST_RB => {
                let b = next().field();
                next();
                next();
                let c = next().node();
                if !self.field_is(b.fs, spec::T_TAG) {
                    self.wrong_type(&b, "TAG");
                    return V::Node(None);
                }
                let Some(c) = c else { return V::Node(None) };
                let mut a = Node::new(Kind::Tag { field: b.fs });
                for ch in c.children {
                    a.add_child(ch);
                }
                V::Node(Some(a))
            }
            r2::TAG_LIST_PARAM_TERM_CASE => {
                let b = next().tok();
                let mut a = Node::phrase(false);
                let n = self.token_node(&b);
                a.add_child(n);
                V::Node(Some(a))
            }
            r2::TAG_LIST_AFFIX | r2::TAG_LIST_VERBATIM | r2::TAG_LIST_TERMLIST => {
                let mut a = Node::phrase(false);
                if let Some(b) = next().node() {
                    a.add_child(b);
                }
                V::Node(Some(a))
            }
            r2::TAG_LIST_TAG_LIST_OR_PARAM_TERM_CASE => {
                let mut b = next().node();
                next();
                let c = next().tok();
                let n = self.token_node(&c);
                if let Some(b) = b.as_mut() {
                    b.add_child(n);
                }
                V::Node(b)
            }
            r2::TAG_LIST_TAG_LIST_OR_AFFIX | r2::TAG_LIST_TAG_LIST_OR_VERBATIM | r2::TAG_LIST_TAG_LIST_OR_TERMLIST => {
                let mut b = next().node();
                next();
                if let (Some(b), Some(c)) = (b.as_mut(), next().node()) {
                    b.add_child(c);
                }
                V::Node(b)
            }
            r2::EXPR_MODIFIER_COLON_NUMERIC_RANGE => {
                let b = next().field();
                next();
                let c = match next() {
                    V::NumRange(c) => c,
                    _ => None,
                };
                if !self.field_is(b.fs, spec::T_NUMERIC) {
                    self.wrong_type(&b, "NUMERIC");
                    return V::Node(None);
                }
                V::Node(c.map(|c| qast::numeric_node(c, b.fs)))
            }
            r2::NUMERIC_RANGE_LSQB_PARAM_NUM_PARAM_NUM_RSQB
            | r2::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_PARAM_NUM_RSQB
            | r2::NUMERIC_RANGE_LSQB_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB
            | r2::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB => {
                next();
                let mut b = next().tok();
                let mut c = next().tok();
                if b.qtype == QType::ParamNumeric {
                    b.qtype = QType::ParamNumericMinRange;
                }
                if c.qtype == QType::ParamNumeric {
                    c.qtype = QType::ParamNumericMaxRange;
                }
                let (bi, ci) = match rule {
                    r2::NUMERIC_RANGE_LSQB_PARAM_NUM_PARAM_NUM_RSQB => (true, true),
                    r2::NUMERIC_RANGE_LSQB_EXCLUSIVE_PARAM_NUM_PARAM_NUM_RSQB => (false, true),
                    r2::NUMERIC_RANGE_LSQB_PARAM_NUM_EXCLUSIVE_PARAM_NUM_RSQB => (true, false),
                    _ => (false, false),
                };
                V::NumRange(Some(self.numeric(Some(&b), Some(&c), bi, ci)))
            }
            r2::NUMERIC_RANGE_LSQB_PARAM_NUM_RSQB => {
                next();
                let b = next().tok();
                V::NumRange(Some(self.numeric(Some(&b), Some(&b), true, true)))
            }
            r2::EXPR_MODIFIER_NOT_EQUAL_PARAM_NUM
            | r2::EXPR_MODIFIER_EQUALS_PARAM_NUM
            | r2::EXPR_MODIFIER_GT_PARAM_NUM
            | r2::EXPR_MODIFIER_GE_PARAM_NUM
            | r2::EXPR_MODIFIER_LT_PARAM_NUM
            | r2::EXPR_MODIFIER_LE_PARAM_NUM => {
                let b = next().field();
                next();
                let c = next().tok();
                let n = match rule {
                    r2::EXPR_MODIFIER_NOT_EQUAL_PARAM_NUM => self.numeric_op(b, &c, (true, true), (true, true), true),
                    r2::EXPR_MODIFIER_EQUALS_PARAM_NUM => self.numeric_op(b, &c, (true, true), (true, true), false),
                    r2::EXPR_MODIFIER_GT_PARAM_NUM => self.numeric_op(b, &c, (true, false), (false, true), false),
                    r2::EXPR_MODIFIER_GE_PARAM_NUM => self.numeric_op(b, &c, (true, false), (true, true), false),
                    r2::EXPR_MODIFIER_LT_PARAM_NUM => self.numeric_op(b, &c, (false, true), (true, false), false),
                    _ => self.numeric_op(b, &c, (false, true), (true, true), false),
                };
                V::Node(n)
            }
            r2::EXPR_MODIFIER_COLON_GEO_FILTER => {
                let b = next().field();
                next();
                let c = match next() {
                    V::Geo(c) => c,
                    _ => None,
                };
                if !self.field_is(b.fs, spec::T_GEO) {
                    self.wrong_type(&b, "GEO");
                    return V::Node(None);
                }
                V::Node(c.map(|mut c| {
                    c.gf.field = b.fs;
                    qast::geo_node(c)
                }))
            }
            r2::GEO_FILTER_LSQB_PARAM_NUM_PARAM_NUM_PARAM_NUM_PARAM_TERM_RSQB => {
                next();
                let mut b = next().tok();
                let mut c = next().tok();
                let d = next().tok();
                let mut e = next().tok();
                if b.qtype == QType::ParamNumeric {
                    b.qtype = QType::ParamGeoCoord;
                }
                if c.qtype == QType::ParamNumeric {
                    c.qtype = QType::ParamGeoCoord;
                }
                if e.qtype == QType::ParamTerm {
                    e.qtype = QType::ParamGeoUnit;
                }
                let (b, c, d, e) = (self.resolved(&b), self.resolved(&c), self.resolved(&d), self.resolved(&e));
                let (p, err) = qast::geo_param(&b, &c, &d, &e, &mut self.num_params);
                if let Some(err) = err {
                    self.fail(err);
                }
                V::Geo(Some(p))
            }
            r2::EXPR_MODIFIER_COLON_GEOMETRY_QUERY => {
                let b = next().field();
                next();
                let c = next().node();
                if !self.field_is(b.fs, spec::T_GEOMETRY) {
                    self.wrong_type(&b, "GEOSHAPE");
                    return V::Node(None);
                }
                V::Node(c.map(|mut c| {
                    if let Kind::Geometry { field, .. } = &mut c.kind {
                        *field = b.fs;
                    }
                    c
                }))
            }
            r2::GEOMETRY_QUERY_LSQB_TERM_ATTRIBUTE_RSQB => {
                next();
                let b = next().tok();
                let mut c = next().tok();
                c.qtype = QType::ParamTermCase;
                let pred = self.text(&b);
                let cc = self.resolved(&c);
                let n = qast::geometry_node(&pred, &cc, &mut self.num_params);
                if n.is_none() {
                    self.report(&c, "Syntax error: Expecting a geoshape predicate");
                }
                V::Node(n)
            }
            r2::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB | r2::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB => {
                let a = next().node();
                next();
                next();
                let b = next().node();
                self.vector_root(b, a, None, false);
                V::Empty
            }
            r2::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB => {
                next();
                next();
                next();
                let b = next().node();
                self.vector_root(b, None, None, true);
                V::Empty
            }
            r2::QUERY_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB
            | r2::QUERY_TEXT_EXPR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB
            | r2::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB => {
                let a = next().node();
                next();
                next();
                let b = next().node();
                next();
                next();
                next();
                let c = next().attrs();
                let star = rule == r2::QUERY_STAR_ARROW_LSQB_VECTOR_QUERY_RSQB_ARROW_LB_ATTRIBUTE_LIST_RB;
                self.vector_root(b, if star { None } else { a }, Some(c), star);
                V::Empty
            }
            r2::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST_VECTOR_SCORE_FIELD => {
                let mut b = next().node();
                let c = next();
                let d = next().tok();
                self.score_field(&mut b, &d);
                if let V::VecAttrs(p) = c {
                    Self::vector_params(&mut b, p);
                }
                V::Node(b)
            }
            r2::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_SCORE_FIELD => {
                let mut b = next().node();
                let d = next().tok();
                self.score_field(&mut b, &d);
                V::Node(b)
            }
            r2::VECTOR_QUERY_VECTOR_COMMAND_VECTOR_ATTRIBUTE_LIST => {
                let mut b = next().node();
                if let V::VecAttrs(p) = next() {
                    Self::vector_params(&mut b, p);
                }
                V::Node(b)
            }
            r2::VECTOR_QUERY_VECTOR_COMMAND => next(),
            r2::VECTOR_SCORE_FIELD_AS_PARAM_TERM_CASE => {
                next();
                next()
            }
            r2::VECTOR_COMMAND_TERM_PARAM_SIZE_MODIFIER_ATTRIBUTE => {
                let t = next().tok();
                let b = next().tok();
                let c = next().field();
                let mut d = next().tok();
                if !self.field_is(c.fs, spec::T_VECTOR) {
                    self.wrong_type(&c, "VECTOR");
                    return V::Node(None);
                }
                if !self.text(&t).eq_ignore_ascii_case(b"KNN") {
                    self.report(&t, "Syntax error: Expecting Vector Similarity command");
                    return V::Node(None);
                }
                d.qtype = QType::ParamVec;
                let mut a = self.vector_node(true, &b, &d);
                if let Kind::Vector(vq) = &mut a.kind {
                    vq.field = c.fs;
                    let mut sf = b"__".to_vec();
                    sf.extend_from_slice(&self.buf[c.tok.off..c.tok.off + c.tok.len]);
                    sf.extend_from_slice(b"_score");
                    vq.score_field = Some(cstr(&sf).to_vec());
                }
                V::Node(Some(a))
            }
            r2::VECTOR_ATTRIBUTE_TERM_PARAM_TERM => {
                let b = next().tok();
                let c = next().tok();
                V::VecAttr(self.text(&b), self.text(&c), c.qtype == QType::ParamTerm)
            }
            r2::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE => {
                let (mut p, mut r) = match next() {
                    V::VecAttrs(p) => p,
                    _ => (Vec::new(), Vec::new()),
                };
                if let V::VecAttr(n, v, res) = next() {
                    p.push((n, v));
                    r.push(res);
                }
                V::VecAttrs((p, r))
            }
            r2::VECTOR_ATTRIBUTE_LIST_VECTOR_ATTRIBUTE => match next() {
                V::VecAttr(n, v, res) => V::VecAttrs((vec![(n, v)], vec![res])),
                _ => V::VecAttrs((Vec::new(), Vec::new())),
            },
            r2::EXPR_MODIFIER_COLON_LSQB_VECTOR_RANGE_COMMAND_RSQB => {
                let b = next().field();
                next();
                next();
                let c = next().node();
                if !self.field_is(b.fs, spec::T_VECTOR) {
                    self.wrong_type(&b, "VECTOR");
                    return V::Node(None);
                }
                V::Node(c.map(|mut c| {
                    if let Kind::Vector(vq) = &mut c.kind {
                        vq.field = b.fs;
                    }
                    c
                }))
            }
            r2::VECTOR_RANGE_COMMAND_TERM_PARAM_NUM_ATTRIBUTE => {
                let t = next().tok();
                let b = next().tok();
                let mut c = next().tok();
                if !self.text(&t).eq_ignore_ascii_case(b"VECTOR_RANGE") {
                    self.report(&t, "Syntax error: expecting vector similarity range command");
                    return V::Node(None);
                }
                c.qtype = QType::ParamVec;
                V::Node(Some(self.vector_node(false, &b, &c)))
            }
            r2::NUM_SIZE | r2::NUM_NUMBER => V::Num(next().tok().numval, false),
            r2::NUM_MINUS_NUM => {
                next();
                let (n, i) = next().num();
                V::Num(-n, i)
            }
            r2::TERM_TERM | r2::TERM_NUMBER | r2::TERM_SIZE => {
                let mut b = next().tok();
                b.qtype = match rule {
                    r2::TERM_TERM => QType::Term,
                    r2::TERM_NUMBER => QType::Numeric,
                    _ => QType::Size,
                };
                V::Tok(b)
            }
            r2::PARAM_TERM_TERM => next(),
            r2::PARAM_TERM_ATTRIBUTE => retype(next(), QType::ParamTerm),
            r2::PARAM_TERM_CASE_TERM => retype(next(), QType::TermCase),
            r2::PARAM_TERM_CASE_ATTRIBUTE => retype(next(), QType::ParamTermCase),
            r2::PARAM_SIZE_SIZE => retype(next(), QType::Size),
            r2::PARAM_SIZE_ATTRIBUTE => retype(next(), QType::ParamSize),
            r2::PARAM_NUM_ATTRIBUTE => retype(next(), QType::ParamNumeric),
            r2::PARAM_NUM_MINUS_ATTRIBUTE | r2::EXCLUSIVE_PARAM_NUM_LP_MINUS_ATTRIBUTE => {
                next();
                if rule == r2::EXCLUSIVE_PARAM_NUM_LP_MINUS_ATTRIBUTE {
                    next();
                }
                let mut b = next().tok();
                b.qtype = QType::ParamNumeric;
                b.sign = -1;
                V::Tok(b)
            }
            r2::PARAM_NUM_NUM | r2::EXCLUSIVE_PARAM_NUM_LP_NUM => {
                if rule == r2::EXCLUSIVE_PARAM_NUM_LP_NUM {
                    next();
                }
                let (n, _) = next().num();
                // The token is the number's slot reinterpreted: only its value and type count.
                let mut t = blank();
                t.numval = n;
                t.qtype = QType::Numeric;
                V::Tok(t)
            }
            r2::EXCLUSIVE_PARAM_NUM_LP_ATTRIBUTE => {
                next();
                retype(next(), QType::ParamNumeric)
            }
            _ => unreachable!("v2 rule {rule}"),
        }
    }

    fn reduce_v1(&mut self, rule: RuleId, rhs: Vec<V>) -> V {
        let mut it = rhs.into_iter();
        let mut next = move || it.next().unwrap_or(V::Empty);
        match rule {
            r1::QUERY_EXPR => {
                self.root = next().node();
                V::Empty
            }
            r1::QUERY_EMPTY => {
                self.root = None;
                V::Empty
            }
            r1::QUERY_STAR => {
                self.root = Some(Node::new(Kind::Wildcard));
                V::Empty
            }
            r1::EXPR_EXPR_EXPR => {
                let b = next().node();
                let c = next().node();
                V::Node(v1_and(b, c))
            }
            r1::EXPR_UNION | r1::EXPR_AFFIX | r1::EXPR_TERMLIST => next(),
            r1::UNION_EXPR_OR_EXPR | r1::UNION_UNION_OR_EXPR => {
                let b = next().node();
                next();
                let c = next().node();
                V::Node(v1_or(b, c))
            }
            r1::EXPR_MODIFIER_COLON_EXPR => {
                let b = next().tok();
                next();
                let Some(mut c) = next().node() else { return V::Node(None) };
                let name = self.buf[b.off..b.off + b.len].to_vec();
                c.set_field_mask(qast::field_bit(self.spec, &name));
                V::Node(Some(c))
            }
            r1::EXPR_MODIFIERLIST_COLON_EXPR => {
                let b = match next() {
                    V::Names(n) => n,
                    _ => Vec::new(),
                };
                next();
                let Some(mut c) = next().node() else { return V::Node(None) };
                let mut mask = 0;
                for p in &b {
                    mask |= qast::field_bit(self.spec, cstr(p));
                }
                c.set_field_mask(mask);
                V::Node(Some(c))
            }
            r1::EXPR_LP_EXPR_RP => {
                next();
                next()
            }
            r1::ATTRIBUTE_ATTRIBUTE_COLON_TERM => {
                let b = next().tok();
                next();
                let c = next().tok();
                V::Attr(self.attribute(&b, &c))
            }
            r1::ATTRIBUTE_LIST_ATTRIBUTE => match next() {
                V::Attr(a) => V::Attrs(Some(vec![a])),
                _ => V::Attrs(None),
            },
            r1::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON_ATTRIBUTE => {
                let mut b = next().attrs();
                next();
                if let V::Attr(c) = next() {
                    b.get_or_insert_with(Vec::new).push(c);
                }
                V::Attrs(b)
            }
            r1::ATTRIBUTE_LIST_ATTRIBUTE_LIST_SEMICOLON => next(),
            r1::ATTRIBUTE_LIST_EMPTY => V::Attrs(None),
            r1::EXPR_EXPR_ARROW_LB_ATTRIBUTE_LIST_RB => {
                let mut b = next().node();
                next();
                next();
                let c = next().attrs();
                self.apply_attrs(&mut b, c);
                V::Node(b)
            }
            r1::EXPR_QUOTE_TERMLIST_QUOTE => {
                next();
                let mut b = next().node();
                if let Some(b) = b.as_mut() {
                    b.kind = Kind::Phrase { exact: true };
                    b.opts.flags |= qast::N_VERBATIM;
                }
                V::Node(b)
            }
            r1::EXPR_QUOTE_TERM_QUOTE => {
                next();
                let b = next().tok();
                let s = qast::normalize(&self.text(&b));
                let mut n = self.plain_token(s);
                n.opts.flags |= qast::N_VERBATIM;
                V::Node(Some(n))
            }
            r1::EXPR_TERM => {
                let b = next().tok();
                let s = qast::normalize(&self.text(&b));
                V::Node(Some(self.plain_token(s)))
            }
            r1::EXPR_STOPWORD => V::Node(None),
            r1::TERMLIST_TERM_TERM => {
                let b = next().tok();
                let c = next().tok();
                let mut a = Node::phrase(false);
                let nb = qast::normalize(&self.text(&b));
                let nb = self.plain_token(nb);
                a.add_child(nb);
                let nc = qast::normalize(&self.text(&c));
                let nc = self.plain_token(nc);
                a.add_child(nc);
                V::Node(Some(a))
            }
            r1::TERMLIST_TERMLIST_TERM => {
                let mut b = next().node();
                let c = next().tok();
                let s = qast::normalize(&self.text(&c));
                let nc = self.plain_token(s);
                if let Some(b) = b.as_mut() {
                    b.add_child(nc);
                }
                V::Node(b)
            }
            r1::TERMLIST_TERMLIST_STOPWORD => next(),
            r1::EXPR_MINUS_EXPR => {
                next();
                V::Node(not_step(next().node()))
            }
            r1::EXPR_TILDE_EXPR => {
                next();
                V::Node(optional(next().node()))
            }
            r1::AFFIX_PREFIX => V::Node(Some(self.prefix(&next().tok(), true, false))),
            r1::AFFIX_SUFFIX => V::Node(Some(self.prefix(&next().tok(), false, true))),
            r1::AFFIX_CONTAINS => V::Node(Some(self.prefix(&next().tok(), true, true))),
            r1::EXPR_PERCENT_TERM_PERCENT | r1::EXPR_PERCENT_STOPWORD_PERCENT => {
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 1)))
            }
            r1::EXPR_PERCENT_PERCENT_TERM_PERCENT_PERCENT | r1::EXPR_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT => {
                next();
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 2)))
            }
            r1::EXPR_PERCENT_PERCENT_PERCENT_TERM_PERCENT_PERCENT_PERCENT | r1::EXPR_PERCENT_PERCENT_PERCENT_STOPWORD_PERCENT_PERCENT_PERCENT => {
                next();
                next();
                next();
                V::Node(Some(self.fuzzy(&next().tok(), 3)))
            }
            r1::MODIFIER_MODIFIER => V::Tok(self.modifier(next().tok()).tok),
            r1::MODIFIERLIST_MODIFIER_OR_TERM => {
                let b = next().tok();
                next();
                let c = next().tok();
                V::Names(vec![self.text(&b), self.text(&c)])
            }
            r1::MODIFIERLIST_MODIFIERLIST_OR_TERM => {
                let mut b = match next() {
                    V::Names(n) => n,
                    _ => Vec::new(),
                };
                next();
                let c = next().tok();
                b.push(self.text(&c));
                V::Names(b)
            }
            r1::EXPR_MODIFIER_COLON_TAG_LIST => {
                let mut b = next().tok();
                next();
                let Some(c) = next().node() else { return V::Node(None) };
                let mut a = Node::new(Kind::Tag { field: None });
                for ch in c.children {
                    a.add_child(ch);
                }
                // Unescaped a second time, in place.
                b.len = unescapen(&mut self.buf, b.off, b.len);
                let fs = field_with_len(self.spec, &self.buf[b.off..b.off + b.len]);
                match fs {
                    Some(_) => {
                        a.kind = Kind::Tag { field: fs };
                        V::Node(Some(a))
                    }
                    None => V::Node(None),
                }
            }
            r1::TAG_LIST_LB_TERM | r1::TAG_LIST_LB_STOPWORD => {
                next();
                let b = next().tok();
                let mut a = Node::phrase(false);
                let s = self.text(&b);
                let n = self.plain_token(s);
                a.add_child(n);
                V::Node(Some(a))
            }
            r1::TAG_LIST_LB_AFFIX | r1::TAG_LIST_LB_TERMLIST => {
                next();
                let mut a = Node::phrase(false);
                if let Some(b) = next().node() {
                    a.add_child(b);
                }
                V::Node(Some(a))
            }
            r1::TAG_LIST_TAG_LIST_OR_TERM | r1::TAG_LIST_TAG_LIST_OR_STOPWORD => {
                let mut b = next().node();
                next();
                let c = next().tok();
                let s = self.text(&c);
                let n = self.plain_token(s);
                if let Some(b) = b.as_mut() {
                    b.add_child(n);
                }
                V::Node(b)
            }
            r1::TAG_LIST_TAG_LIST_OR_AFFIX | r1::TAG_LIST_TAG_LIST_OR_TERMLIST => {
                let mut b = next().node();
                next();
                if let (Some(b), Some(c)) = (b.as_mut(), next().node()) {
                    b.add_child(c);
                }
                V::Node(b)
            }
            r1::TAG_LIST_TAG_LIST_RB => next(),
            r1::EXPR_MODIFIER_COLON_NUMERIC_RANGE => {
                let b = next().tok();
                next();
                let c = match next() {
                    V::NumRange(c) => c,
                    _ => None,
                };
                let fs = field_with_len(self.spec, &self.buf[b.off..b.off + b.len]);
                match (fs, c) {
                    (Some(_), Some(c)) => V::Node(Some(qast::numeric_node(c, fs))),
                    _ => V::Node(None),
                }
            }
            r1::NUMERIC_RANGE_LSQB_NUM_NUM_RSQB => {
                next();
                let (bn, bi) = next().num();
                let (cn, ci) = next().num();
                let nf = qast::NumFilter { field: None, min: bn, max: cn, min_inclusive: bi, max_inclusive: ci };
                V::NumRange(Some(NumParam { nf, params: Vec::new() }))
            }
            r1::EXPR_MODIFIER_COLON_GEO_FILTER => {
                let b = next().tok();
                next();
                let c = match next() {
                    V::Geo(c) => c,
                    _ => None,
                };
                let Some(mut c) = c else { return V::Node(None) };
                let fs = field_with_len(self.spec, &self.buf[b.off..b.off + b.len]);
                if fs.is_none() {
                    return V::Node(None);
                }
                c.gf.field = fs;
                V::Node(Some(qast::geo_node(c)))
            }
            r1::GEO_FILTER_LSQB_NUM_NUM_NUM_TERM_RSQB => {
                next();
                let (lon, _) = next().num();
                let (lat, _) = next().num();
                let (radius, _) = next().num();
                let e = next().tok();
                let gf = qast::GeoFilter { field: None, lon, lat, radius, unit: qast::geo_unit_buffer(&self.text(&e)) };
                if let Err(err) = gf.validate() {
                    self.fail(err);
                }
                V::Geo(Some(GeoParam { gf, params: Vec::new() }))
            }
            r1::NUM_NUMBER => V::Num(next().tok().numval, true),
            r1::NUM_LP_NUM => {
                next();
                let (n, _) = next().num();
                V::Num(n, false)
            }
            r1::NUM_MINUS_NUM => {
                next();
                let (n, i) = next().num();
                V::Num(-n, i)
            }
            r1::TERM_TERM | r1::TERM_NUMBER => next(),
            _ => unreachable!("v1 rule {rule}"),
        }
    }
}

fn retype(v: V, t: QType) -> V {
    let mut b = v.tok();
    b.qtype = t;
    V::Tok(b)
}

/// The grammar terminal for a scanned token.
fn terminal(k: Tk, v2: bool) -> Symbol {
    if v2 {
        match k {
            Tk::Size => t2::SIZE,
            Tk::Number => t2::NUMBER,
            Tk::Modifier => t2::MODIFIER,
            Tk::Attribute => t2::ATTRIBUTE,
            Tk::NotEqual => t2::NOT_EQUAL,
            Tk::Equals => t2::EQUALS,
            Tk::Gt => t2::GT,
            Tk::Ge => t2::GE,
            Tk::Lt => t2::LT,
            Tk::Le => t2::LE,
            Tk::Arrow => t2::ARROW,
            Tk::As => t2::AS_T,
            Tk::Quote => t2::QUOTE,
            Tk::SQuote => t2::SQUOTE,
            Tk::Or => t2::OR,
            Tk::Lp => t2::LP,
            Tk::Rp => t2::RP,
            Tk::Lb => t2::LB,
            Tk::Rb => t2::RB,
            Tk::Colon => t2::COLON,
            Tk::Semicolon => t2::SEMICOLON,
            Tk::Minus => t2::MINUS,
            Tk::Tilde => t2::TILDE,
            Tk::Star => t2::STAR,
            Tk::Percent => t2::PERCENT,
            Tk::Lsqb => t2::LSQB,
            Tk::Rsqb => t2::RSQB,
            Tk::IsMissing => t2::ISMISSING,
            Tk::Term | Tk::StopWord => t2::TERM,
            Tk::Exact => t2::EXACT,
            Tk::Prefix => t2::PREFIX,
            Tk::Suffix => t2::SUFFIX,
            Tk::Contains => t2::CONTAINS,
            Tk::Verbatim => t2::VERBATIM,
            Tk::Wildcard => t2::WILDCARD,
        }
    } else {
        match k {
            Tk::Number | Tk::Size => t1::NUMBER,
            Tk::Modifier => t1::MODIFIER,
            Tk::Attribute => t1::ATTRIBUTE,
            Tk::Arrow => t1::ARROW,
            Tk::Quote => t1::QUOTE,
            Tk::Or => t1::OR,
            Tk::Lp => t1::LP,
            Tk::Rp => t1::RP,
            Tk::Lb => t1::LB,
            Tk::Rb => t1::RB,
            Tk::Colon => t1::COLON,
            Tk::Semicolon => t1::SEMICOLON,
            Tk::Minus => t1::MINUS,
            Tk::Tilde => t1::TILDE,
            Tk::Star => t1::STAR,
            Tk::Percent => t1::PERCENT,
            Tk::Lsqb => t1::LSQB,
            Tk::Rsqb => t1::RSQB,
            Tk::StopWord => t1::STOPWORD,
            Tk::Prefix => t1::PREFIX,
            Tk::Suffix => t1::SUFFIX,
            Tk::Contains => t1::CONTAINS,
            _ => t1::TERM,
        }
    }
}

/// `QAST_Parse`'s parsing step: dialect 1 or 2+.
pub fn parse(query: &[u8], dialect: u32, spec: &Spec, opts: &ParseOpts) -> Result<Parsed, QErr> {
    let q = cstr(query);
    let v2 = dialect >= 2;
    let d = if v2 { Dialect::V2 } else { Dialect::V1 };
    let stop = |w: &[u8]| opts.stopwords.is_some_and(|s| s.contains(w));
    let (toks, last) = qlex::scan(q, d, &stop);
    let mut buf = q.to_vec();
    buf.push(0);
    let tables = if v2 { super::qgrammar::v2_tables() } else { super::qgrammar::v1_tables() };
    let mut parser = Parser::new(tables);
    let mut cx = Ctx { buf, spec, opts, v2, err: None, root: None, num_tokens: 0, num_params: 0 };
    for t in toks {
        let sym = terminal(t.kind, v2);
        parser.feed(sym, V::Tok(t), &mut cx);
        if cx.err.is_some() {
            break;
        }
    }
    if cx.err.is_none() {
        parser.feed(0, V::Tok(last), &mut cx);
    }
    match cx.err {
        Some(e) => Err(e),
        None => Ok(Parsed { root: cx.root, num_tokens: cx.num_tokens, num_params: cx.num_params }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::spec::Spec;

    fn spec() -> Spec {
        let args: Vec<bytes::Bytes> = ["ON", "HASH", "SCHEMA", "t", "TEXT", "n", "NUMERIC", "tg", "TAG"].iter().map(|s| bytes::Bytes::from(*s)).collect();
        Spec::parse(bytes::Bytes::from_static(b"i"), &args, &crate::config::Config::default()).expect("a schema")
    }

    fn kinds(n: &Node) -> String {
        let k = match &n.kind {
            Kind::Phrase { exact } => if *exact { "EXACT" } else { "AND" }.to_string(),
            Kind::Union => "OR".into(),
            Kind::Token(t) => String::from_utf8_lossy(&t.s).into_owned(),
            Kind::Numeric(_) => "NUM".into(),
            Kind::Not => "NOT".into(),
            Kind::Tag { .. } => "TAG".into(),
            k => format!("{k:?}"),
        };
        if n.children.is_empty() { k } else { format!("{k}({})", n.children.iter().map(kinds).collect::<Vec<_>>().join(" ")) }
    }

    fn parse_ok(q: &str, dialect: u32) -> String {
        let opts = ParseOpts { stopwords: None, params: None };
        let p = parse(q.as_bytes(), dialect, &spec(), &opts).expect("parses");
        p.root.as_ref().map_or("NULL".into(), kinds)
    }

    fn parse_err(q: &str, dialect: u32) -> String {
        let opts = ParseOpts { stopwords: None, params: None };
        parse(q.as_bytes(), dialect, &spec(), &opts).expect_err("fails").text()
    }

    #[test]
    fn trees() {
        assert_eq!(parse_ok("a b c", 2), "AND(a b c)");
        assert_eq!(parse_ok("a | b c", 2), "OR(a AND(b c))");
        assert_eq!(parse_ok("--a", 2), "a");
        assert_eq!(parse_ok("@n:[1 2] -@tg:{x|y}", 2), "AND(NUM NOT(TAG(x y)))");
        assert_eq!(parse_ok("\"A, b\"", 2), "EXACT(a b)");
        assert_eq!(parse_ok("a b|c", 1), "OR(AND(a b) c)");
        assert_eq!(parse_ok("", 2), "NULL");
    }

    #[test]
    fn errors() {
        assert_eq!(parse_err("@x:a", 2), "SEARCH_SYNTAX Unknown field at offset 0 near x");
        assert_eq!(parse_err("@n:a", 2), "SEARCH_SYNTAX Expected a TEXT field at offset 0 near n");
        assert_eq!(parse_err("a )", 2), "SEARCH_SYNTAX Syntax error at offset 2 near a");
        let deep = format!("{}a", "(".repeat(300));
        assert_eq!(parse_err(&deep, 2), "SEARCH_SYNTAX Parser stack overflow. Try moving nested parentheses more to the left");
    }
}
