//! CSS selectors for extraction rules (docs/search/crawler.md, "Extraction rules"): the part of
//! Selectors Level 3 a page's fields are picked with, parsed with bounds and matched against the
//! arena DOM in one pass, without backtracking.
//!
//! - Simple selectors: a type or `*`, `#id`, `.class`, `[attr]` and `[attr=v]` with `~=`, `|=`,
//!   `^=`, `$=`, `*=` (and ` i` for a case-insensitive value), and the structural
//!   pseudo-classes `:root`, `:empty`, `:first-child`, `:last-child`, `:only-child`,
//!   `:first-of-type`, `:last-of-type`, `:only-of-type`, `:nth-child()`, `:nth-last-child()`,
//!   `:nth-of-type()`, `:nth-last-of-type()` and `:not()` of one simple selector.
//! - Combinators: descendant, `>`, `+` and `~`; a list is comma-separated.
//! - Nothing that looks at more than an element's ancestors and earlier siblings (no `:has()`),
//!   no pseudo-elements, no state (`:hover`), no XPath (a clear error), no escapes.
//!
//! Matching walks the elements once, in document order. Each element carries, per compound of
//! the list, whether the selector up to that compound matched it, whether it matched an ancestor
//! (inherited from the parent) and whether it matched an earlier sibling (carried from the
//! previous one), so a combinator is one bit test: the work is the elements times the compounds,
//! whatever the page's shape.

/// The longest selector text taken.
pub const MAX_TEXT: usize = 1024;
/// Selectors in one comma-separated list.
pub const MAX_SELECTORS: usize = 16;
/// Compound selectors (the parts between combinators) across a whole list: each is a bit.
pub const MAX_COMPOUNDS: usize = 32;
/// Simple selectors in one compound.
pub const MAX_SIMPLE: usize = 16;

/// A parsed selector list.
#[derive(Debug, Clone)]
pub struct Selectors {
    /// Each selector's compounds, leftmost first, with the combinator before each (the first's
    /// is unused).
    list: Vec<Vec<(Comb, Compound)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comb {
    Descendant,
    Child,
    Adjacent,
    Sibling,
}

#[derive(Debug, Clone, Default)]
struct Compound {
    /// A lowercase type, or `None` for `*` or none given.
    tag: Option<String>,
    simple: Vec<Simple>,
}

#[derive(Debug, Clone)]
enum Simple {
    Id(String),
    Class(String),
    Attr { name: String, op: Option<(AttrOp, String, bool)> },
    Root,
    Empty,
    /// `an+b` counted from the start or end (`last`), among all element siblings or those of the
    /// same type (`of_type`).
    Nth { a: i64, b: i64, last: bool, of_type: bool },
    /// Alone among its element siblings (`true`: of its type).
    Only(bool),
    Not(Box<Simple>),
    /// `:not(tag)`.
    NotTag(String),
}

#[derive(Debug, Clone, Copy)]
enum AttrOp {
    Equals,
    Word,
    Dash,
    Prefix,
    Suffix,
    Substring,
}

/// What matching needs of an element.
pub struct Element<'a> {
    pub tag: &'a str,
    pub attrs: &'a [(&'a str, &'a str)],
    pub is_root: bool,
    pub is_empty: bool,
    /// 1-based position among the parent's element children, and how many there are.
    pub index: usize,
    pub count: usize,
    /// The same among siblings of its type.
    pub type_index: usize,
    pub type_count: usize,
}

impl Element<'_> {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }
}

impl Selectors {
    /// Parses `text`, refusing what's beyond the bounds above with the reason.
    pub fn parse(text: &str) -> Result<Selectors, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("the selector is empty".into());
        }
        if t.len() > MAX_TEXT {
            return Err(format!("the selector is longer than {MAX_TEXT} bytes"));
        }
        if t.starts_with('/') || t.starts_with("(/") || t.starts_with("./") || t.contains("//") {
            return Err(format!("[{t}] looks like XPath, which roll's crawler doesn't run yet: use a CSS selector"));
        }
        if t.contains('\\') {
            return Err(format!("[{t}]: escapes in selectors aren't supported"));
        }
        let mut p = Parser { s: t.as_bytes(), at: 0 };
        let mut list = Vec::new();
        let mut compounds = 0usize;
        loop {
            p.ws();
            let sel = p.selector()?;
            compounds += sel.len();
            list.push(sel);
            if list.len() > MAX_SELECTORS {
                return Err(format!("[{t}] lists more than {MAX_SELECTORS} selectors"));
            }
            if compounds > MAX_COMPOUNDS {
                return Err(format!("[{t}] has more than {MAX_COMPOUNDS} parts between combinators"));
            }
            p.ws();
            match p.peek() {
                None => break,
                Some(b',') => p.at += 1,
                Some(c) => return Err(format!("[{t}]: unexpected [{}] at byte {}", c as char, p.at)),
            }
        }
        Ok(Selectors { list })
    }

    /// The elements of `elements` (in document order, each with its parent's and previous
    /// sibling's position in the slice) the list matches, in document order. `stop` is asked
    /// every so often and ends the walk (`Err`) when it says so.
    pub fn select(&self, elements: &[(Element<'_>, Option<usize>, Option<usize>)], mut stop: impl FnMut() -> bool) -> Result<Vec<usize>, ()> {
        // Bit offsets of each selector's compounds in one word.
        let mut offsets = Vec::with_capacity(self.list.len());
        let mut n = 0u32;
        for sel in &self.list {
            offsets.push(n);
            n += sel.len() as u32;
        }
        let (mut matched, mut above, mut before) = (vec![0u32; elements.len()], vec![0u32; elements.len()], vec![0u32; elements.len()]);
        let mut out = Vec::new();
        for (k, (el, parent, prev)) in elements.iter().enumerate() {
            if k % 1024 == 1023 && stop() {
                return Err(());
            }
            let at_parent = parent.map_or(0, |p| matched[p]);
            let inherited = parent.map_or(0, |p| matched[p] | above[p]);
            let at_prev = prev.map_or(0, |q| matched[q]);
            let carried = prev.map_or(0, |q| matched[q] | before[q]);
            above[k] = inherited;
            before[k] = carried;
            let mut m = 0u32;
            let mut subject = false;
            for (sel, &off) in self.list.iter().zip(&offsets) {
                for (i, (comb, compound)) in sel.iter().enumerate() {
                    let bit = 1u32 << (off + i as u32);
                    let ok = match i {
                        0 => true,
                        _ => {
                            let need = 1u32 << (off + i as u32 - 1);
                            match comb {
                                Comb::Descendant => inherited & need != 0,
                                Comb::Child => at_parent & need != 0,
                                Comb::Adjacent => at_prev & need != 0,
                                Comb::Sibling => carried & need != 0,
                            }
                        }
                    };
                    if ok && compound.matches(el) {
                        m |= bit;
                        if i + 1 == sel.len() {
                            subject = true;
                        }
                    }
                }
            }
            matched[k] = m;
            if subject {
                out.push(k);
            }
        }
        Ok(out)
    }
}

impl Compound {
    fn matches(&self, el: &Element<'_>) -> bool {
        self.tag.as_deref().is_none_or(|t| t == el.tag) && self.simple.iter().all(|s| s.matches(el))
    }
}

impl Simple {
    fn matches(&self, el: &Element<'_>) -> bool {
        match self {
            Simple::Id(id) => el.attr("id") == Some(id.as_str()),
            Simple::Class(c) => el.attr("class").is_some_and(|v| v.split_ascii_whitespace().any(|w| w == c)),
            Simple::Attr { name, op } => {
                let Some(v) = el.attr(name) else { return false };
                let Some((op, want, fold)) = op else { return true };
                let (v, want) = match fold {
                    true => (v.to_lowercase(), want.to_lowercase()),
                    false => (v.to_string(), want.clone()),
                };
                match op {
                    AttrOp::Equals => v == want,
                    AttrOp::Word => !want.is_empty() && v.split_ascii_whitespace().any(|w| w == want),
                    AttrOp::Dash => v == want || v.starts_with(&format!("{want}-")),
                    AttrOp::Prefix => !want.is_empty() && v.starts_with(&want),
                    AttrOp::Suffix => !want.is_empty() && v.ends_with(&want),
                    AttrOp::Substring => !want.is_empty() && v.contains(&want),
                }
            }
            Simple::Root => el.is_root,
            Simple::Empty => el.is_empty,
            Simple::Nth { a, b, last, of_type } => {
                let (i, n) = if *of_type { (el.type_index, el.type_count) } else { (el.index, el.count) };
                let pos = if *last { n + 1 - i } else { i } as i64;
                // pos = a*k + b for some k >= 0.
                match a {
                    0 => pos == *b,
                    a => (pos - b) % a == 0 && (pos - b) / a >= 0,
                }
            }
            Simple::Only(of_type) => if *of_type { el.type_count == 1 } else { el.count == 1 },
            Simple::Not(s) => !s.matches(el),
            Simple::NotTag(t) => el.tag != t,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn ws(&mut self) -> bool {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
        self.at > start
    }

    fn ident(&mut self) -> Result<String, String> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80) {
            self.at += 1;
        }
        if self.at == start {
            return Err(format!("expected a name at byte {start}"));
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.at]).into_owned())
    }

    /// One selector: compounds and the combinators between them, up to a `,` or the end.
    fn selector(&mut self) -> Result<Vec<(Comb, Compound)>, String> {
        let mut out = vec![(Comb::Descendant, self.compound()?)];
        loop {
            let spaced = self.ws();
            let comb = match self.peek() {
                None | Some(b',') => break,
                Some(b'>') => Comb::Child,
                Some(b'+') => Comb::Adjacent,
                Some(b'~') => Comb::Sibling,
                Some(_) if spaced => Comb::Descendant,
                Some(c) => return Err(format!("unexpected [{}] at byte {}", c as char, self.at)),
            };
            if comb != Comb::Descendant {
                self.at += 1;
                self.ws();
            }
            out.push((comb, self.compound()?));
            if out.len() > MAX_COMPOUNDS {
                return Err(format!("more than {MAX_COMPOUNDS} parts between combinators"));
            }
        }
        Ok(out)
    }

    fn compound(&mut self) -> Result<Compound, String> {
        let mut c = Compound::default();
        let mut star = false;
        match self.peek() {
            Some(b'*') => {
                self.at += 1;
                star = true;
            }
            Some(ch) if ch.is_ascii_alphabetic() => c.tag = Some(self.ident()?.to_ascii_lowercase()),
            _ => {}
        }
        loop {
            match self.peek() {
                Some(b'#' | b'.' | b'[' | b':') => {
                    let s = self.simple()?;
                    c.simple.push(s);
                    if c.simple.len() > MAX_SIMPLE {
                        return Err(format!("more than {MAX_SIMPLE} conditions on one element"));
                    }
                }
                _ => break,
            }
        }
        if c.tag.is_none() && c.simple.is_empty() && !star {
            return Err(format!("expected a selector at byte {}", self.at));
        }
        Ok(c)
    }

    fn simple(&mut self) -> Result<Simple, String> {
        match self.peek() {
            Some(b'#') => {
                self.at += 1;
                Ok(Simple::Id(self.ident()?))
            }
            Some(b'.') => {
                self.at += 1;
                Ok(Simple::Class(self.ident()?))
            }
            Some(b'[') => {
                self.at += 1;
                self.ws();
                let name = self.ident()?.to_ascii_lowercase();
                self.ws();
                let op = match self.peek() {
                    Some(b']') => None,
                    Some(c) => {
                        let op = match c {
                            b'=' => AttrOp::Equals,
                            b'~' => AttrOp::Word,
                            b'|' => AttrOp::Dash,
                            b'^' => AttrOp::Prefix,
                            b'$' => AttrOp::Suffix,
                            b'*' => AttrOp::Substring,
                            c => return Err(format!("unexpected [{}] in an attribute selector at byte {}", c as char, self.at)),
                        };
                        self.at += 1;
                        if !matches!(op, AttrOp::Equals) {
                            if self.peek() != Some(b'=') {
                                return Err(format!("expected [=] at byte {}", self.at));
                            }
                            self.at += 1;
                        }
                        self.ws();
                        let value = match self.peek() {
                            Some(q @ (b'"' | b'\'')) => {
                                self.at += 1;
                                let start = self.at;
                                while self.peek().is_some_and(|c| c != q) {
                                    self.at += 1;
                                }
                                if self.peek() != Some(q) {
                                    return Err("an attribute value's quote isn't closed".into());
                                }
                                let v = String::from_utf8_lossy(&self.s[start..self.at]).into_owned();
                                self.at += 1;
                                v
                            }
                            _ => self.ident()?,
                        };
                        self.ws();
                        let fold = matches!(self.peek(), Some(b'i' | b'I'));
                        if fold {
                            self.at += 1;
                            self.ws();
                        }
                        Some((op, value, fold))
                    }
                    None => return Err("an attribute selector isn't closed".into()),
                };
                if self.peek() != Some(b']') {
                    return Err(format!("expected [ ] ] at byte {}", self.at));
                }
                self.at += 1;
                Ok(Simple::Attr { name, op })
            }
            Some(b':') => {
                self.at += 1;
                if self.peek() == Some(b':') {
                    return Err("pseudo-elements (::…) select no element: leave them out".into());
                }
                let name = self.ident()?.to_ascii_lowercase();
                let plain = |s: Simple| Ok(s);
                match name.as_str() {
                    "root" => plain(Simple::Root),
                    "empty" => plain(Simple::Empty),
                    "first-child" => plain(Simple::Nth { a: 0, b: 1, last: false, of_type: false }),
                    "last-child" => plain(Simple::Nth { a: 0, b: 1, last: true, of_type: false }),
                    "first-of-type" => plain(Simple::Nth { a: 0, b: 1, last: false, of_type: true }),
                    "last-of-type" => plain(Simple::Nth { a: 0, b: 1, last: true, of_type: true }),
                    "only-child" => plain(Simple::Only(false)),
                    "only-of-type" => plain(Simple::Only(true)),
                    "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                        let (a, b) = self.nth()?;
                        Ok(Simple::Nth { a, b, last: name.contains("last"), of_type: name.ends_with("of-type") })
                    }
                    "not" => {
                        if self.peek() != Some(b'(') {
                            return Err("expected [(] after :not".into());
                        }
                        self.at += 1;
                        self.ws();
                        let inner = match self.peek() {
                            Some(c) if c.is_ascii_alphabetic() => Simple::NotTag(self.ident()?.to_ascii_lowercase()),
                            Some(b'*') => return Err(":not(*) matches nothing".into()),
                            Some(b':') if self.s[self.at..].starts_with(b":not") => return Err(":not() takes one simple selector, not another :not()".into()),
                            _ => Simple::Not(Box::new(self.simple()?)),
                        };
                        self.ws();
                        if self.peek() != Some(b')') {
                            return Err(":not() takes one simple selector".into());
                        }
                        self.at += 1;
                        Ok(inner)
                    }
                    other => Err(format!(":{other} isn't supported (structural pseudo-classes and :not() are; nothing like :has(), states or pseudo-elements)")),
                }
            }
            _ => Err(format!("expected a selector at byte {}", self.at)),
        }
    }

    /// `(an+b)`, `(odd)`, `(even)` or `(b)`.
    fn nth(&mut self) -> Result<(i64, i64), String> {
        if self.peek() != Some(b'(') {
            return Err("expected [(]".into());
        }
        self.at += 1;
        let start = self.at;
        while self.peek().is_some_and(|c| c != b')') {
            self.at += 1;
        }
        if self.peek() != Some(b')') {
            return Err("an :nth-…() isn't closed".into());
        }
        let text: String = String::from_utf8_lossy(&self.s[start..self.at]).chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
        self.at += 1;
        let num = |s: &str| -> Result<i64, String> { s.parse::<i64>().ok().filter(|n| n.abs() <= 1_000_000).ok_or_else(|| format!("[{text}] isn't an+b")) };
        match text.as_str() {
            "odd" => Ok((2, 1)),
            "even" => Ok((2, 0)),
            t => match t.split_once('n') {
                None => Ok((0, num(t)?)),
                Some((a, b)) => {
                    let a = match a {
                        "" | "+" => 1,
                        "-" => -1,
                        a => num(a)?,
                    };
                    let b = if b.is_empty() { 0 } else { num(b.strip_prefix('+').unwrap_or(b))? };
                    Ok((a, b))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What's refused, and why: the bounds, XPath, `:has()` and its kind, pseudo-elements.
    #[test]
    fn parsing_is_bounded() {
        for ok in ["h1", "article .author", "div > p + span ~ em", "a[href^='https://'][rel~=nofollow i]", "li:nth-child(2n+1):not(.ad)", "ul li:last-of-type, ol > li:only-child", "*", "#main", ":root > body", "p:nth-last-of-type(-n+3)"] {
            assert!(Selectors::parse(ok).is_ok(), "{ok}: {:?}", Selectors::parse(ok).err());
        }
        let err = |t: &str| Selectors::parse(t).expect_err(t);
        assert!(err("//div[@class='x']").contains("XPath"));
        assert!(err("div:has(> p)").contains(":has isn't supported"));
        assert!(err("a:hover").contains(":hover isn't supported"));
        assert!(err("p::before").contains("pseudo-elements"));
        assert!(err("p:not(:not(.a))").contains("another :not()"));
        assert!(err(r"a\:b").contains("escapes"));
        assert!(err("").contains("empty"));
        assert!(err(&"a ".repeat(MAX_COMPOUNDS + 1)).contains("parts between combinators"));
        assert!(err(&vec!["a"; MAX_SELECTORS + 1].join(",")).contains("selectors"));
        assert!(err(&"a".repeat(MAX_TEXT + 1)).contains("longer"));
        assert!(err(&format!("a{}", ".c".repeat(MAX_SIMPLE + 1))).contains("conditions"));
        assert!(err("div >").contains("expected a selector"));
    }

    /// Matching against a small tree: combinators, structure and attributes, in document order.
    #[test]
    fn matches_in_one_pass() {
        // <html><body><div id=a class="x y"><p>1</p><p class=ad>2</p><span>3</span></div><p>4</p></body></html>
        const NONE: &[(&str, &str)] = &[];
        const DIV: &[(&str, &str)] = &[("id", "a"), ("class", "x y")];
        const AD: &[(&str, &str)] = &[("class", "ad")];
        let (none, div, ad) = (NONE, DIV, AD);
        let el = |tag: &'static str, attrs: &'static [(&'static str, &'static str)], index: usize, count: usize, type_index: usize, type_count: usize| Element { tag, attrs, is_root: tag == "html", is_empty: false, index, count, type_index, type_count };
        let els = vec![
            (el("html", none, 1, 1, 1, 1), None, None),
            (el("body", none, 1, 1, 1, 1), Some(0), None),
            (el("div", div, 1, 2, 1, 1), Some(1), None),
            (el("p", none, 1, 3, 1, 2), Some(2), None),
            (el("p", ad, 2, 3, 2, 2), Some(2), Some(3)),
            (el("span", none, 3, 3, 1, 1), Some(2), Some(4)),
            (el("p", none, 2, 2, 1, 1), Some(1), Some(2)),
        ];
        let sel = |t: &str| Selectors::parse(t).unwrap().select(&els, || false).unwrap();
        assert_eq!(sel("div p"), [3, 4]);
        assert_eq!(sel("body > p"), [6]);
        assert_eq!(sel("p + span"), [5]);
        assert_eq!(sel("div ~ p"), [6]);
        assert_eq!(sel("p:not(.ad)"), [3, 6]);
        assert_eq!(sel("#a.x.y > :last-child, :root"), [0, 5]);
        assert_eq!(sel("p:nth-of-type(2)"), [4]);
        assert_eq!(sel("div :only-of-type"), [5]);
        assert_eq!(sel("[class|=x], [class*=d]"), [4]);
        assert_eq!(sel("html body div p"), [3, 4]);
        assert!(Selectors::parse("p").unwrap().select(&els, || true).is_ok(), "fewer elements than a check");
    }
}
