//! Locations of fields in a JSON body, for the `[line:col]` prefixes of Elasticsearch's
//! `ObjectParser` errors (`[1:28] [index_template] unknown field [foo]`).
//!
//! Bodies are parsed into values without positions (keeping the hot path lean); when an error
//! needs a location, the raw body is scanned once more to find it. Columns are 1-based byte
//! offsets in the line, like Jackson's: a field name is located at its opening quote, a value
//! at its first byte.

/// One step of a path into a JSON document.
#[derive(Clone, Copy, Debug)]
pub enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

/// What to locate at the end of the path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// The field name of the last `Key`.
    Name,
    /// The value at the path.
    Value,
}

struct Scanner<'a> {
    s: &'a [u8],
    pos: usize,
    line: usize,
    row_start: usize,
}

impl<'a> Scanner<'a> {
    fn loc(&self, at: usize) -> (usize, usize) {
        (self.line, at - self.row_start + 1)
    }

    fn ws(&mut self) {
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.row_start = self.pos;
                }
                b'/' if self.s.get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    while self.pos + 1 < self.s.len() && !(self.s[self.pos] == b'*' && self.s[self.pos + 1] == b'/') {
                        if self.s[self.pos] == b'\n' {
                            self.line += 1;
                            self.row_start = self.pos + 1;
                        }
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.s.len());
                }
                b'/' if self.s.get(self.pos + 1) == Some(&b'/') => {
                    while self.pos < self.s.len() && self.s[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Reads a string starting at the quote; returns its unescaped text.
    fn string(&mut self) -> Option<String> {
        if self.s.get(self.pos) != Some(&b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            match c {
                b'"' => {
                    self.pos += 1;
                    return String::from_utf8(out).ok();
                }
                b'\\' => {
                    let e = *self.s.get(self.pos + 1)?;
                    self.pos += 2;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.pos..self.pos + 4)?).ok()?;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf8(&mut buf).as_bytes());
                            self.pos += 4;
                        }
                        other => out.push(other),
                    }
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        None
    }

    /// Skips one value.
    fn skip(&mut self) -> Option<()> {
        self.ws();
        match *self.s.get(self.pos)? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.pos += 1;
                loop {
                    self.ws();
                    if self.s.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        return Some(());
                    }
                    self.string()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return None;
                    }
                    self.pos += 1;
                    crate::stack::maybe_grow(|| self.skip())?;
                    self.ws();
                    match self.s.get(self.pos)? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                loop {
                    self.ws();
                    if self.s.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        return Some(());
                    }
                    crate::stack::maybe_grow(|| self.skip())?;
                    self.ws();
                    match self.s.get(self.pos)? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            _ => {
                while self.pos < self.s.len() && !matches!(self.s[self.pos], b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r') {
                    self.pos += 1;
                }
            }
        }
        Some(())
    }
}

/// Finds the location of a field name or value along `path`.
pub fn locate(src: &[u8], path: &[Seg], target: Target) -> Option<(usize, usize)> {
    let mut sc = Scanner { s: src, pos: 0, line: 1, row_start: 0 };
    sc.ws();
    for (depth, seg) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        match seg {
            Seg::Key(k) => {
                if sc.s.get(sc.pos) != Some(&b'{') {
                    return None;
                }
                sc.pos += 1;
                loop {
                    sc.ws();
                    if sc.s.get(sc.pos) == Some(&b'}') {
                        return None;
                    }
                    // Jackson locates a field name at its opening quote.
                    let name_at = sc.pos;
                    let name_loc = sc.loc(name_at);
                    let name = sc.string()?;
                    sc.ws();
                    if sc.s.get(sc.pos) != Some(&b':') {
                        return None;
                    }
                    sc.pos += 1;
                    sc.ws();
                    if name == *k {
                        if last && target == Target::Name {
                            return Some(name_loc);
                        }
                        break;
                    }
                    sc.skip()?;
                    sc.ws();
                    match sc.s.get(sc.pos)? {
                        b',' => sc.pos += 1,
                        _ => return None,
                    }
                }
            }
            Seg::Index(n) => {
                if sc.s.get(sc.pos) != Some(&b'[') {
                    return None;
                }
                sc.pos += 1;
                for _ in 0..*n {
                    sc.skip()?;
                    sc.ws();
                    if sc.s.get(sc.pos) != Some(&b',') {
                        return None;
                    }
                    sc.pos += 1;
                }
                sc.ws();
            }
        }
    }
    sc.ws();
    Some(sc.loc(sc.pos))
}

/// Location of the last byte of the value at `path` (where a parser that consumed the whole
/// value reports an error about it).
pub fn value_end(src: &[u8], path: &[Seg]) -> Option<(usize, usize)> {
    let (line, col) = locate(src, path, Target::Value)?;
    // Re-scan from the value's start to find where it ends.
    let mut sc = Scanner { s: src, pos: 0, line: 1, row_start: 0 };
    let mut l = 1;
    while l < line {
        if sc.s.get(sc.pos)? == &b'\n' {
            l += 1;
            sc.row_start = sc.pos + 1;
        }
        sc.pos += 1;
    }
    sc.line = line;
    sc.pos = sc.row_start + col - 1;
    sc.skip()?;
    let end = sc.pos.checked_sub(1)?;
    // A value spanning lines: count the newlines it crossed.
    let mut end_line = line;
    let mut end_row = sc.row_start;
    for (i, b) in src[sc.row_start..=end].iter().enumerate() {
        if *b == b'\n' {
            end_line += 1;
            end_row = sc.row_start + i + 1;
        }
    }
    Some((end_line, end - end_row + 1))
}

/// The byte range of the value at `path` (its raw text, e.g. a number's original digits).
pub fn value_span(src: &[u8], path: &[Seg]) -> Option<(usize, usize)> {
    let (line, col) = locate(src, path, Target::Value)?;
    let mut l = 1;
    let mut row_start = 0;
    let mut i = 0;
    while l < line {
        if *src.get(i)? == b'\n' {
            l += 1;
            row_start = i + 1;
        }
        i += 1;
    }
    let start = row_start + col - 1;
    let mut sc = Scanner { s: src, pos: start, line, row_start };
    sc.skip()?;
    Some((start, sc.pos))
}

/// Location of the field name `key` inside the object at `parent`.
pub fn key_location(src: &[u8], parent: &[Seg], key: &str) -> Option<(usize, usize)> {
    let mut path = parent.to_vec();
    path.push(Seg::Key(key));
    locate(src, &path, Target::Name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names() {
        let body = br#"{"index_patterns":["q-*"],"foo":1}"#;
        assert_eq!(key_location(body, &[], "foo"), Some((1, 27)));
        let body = b"{\n  \"a\": {\n    \"b\": [1, {\"c\": 2}]\n  }\n}";
        assert_eq!(key_location(body, &[Seg::Key("a")], "b"), Some((3, 5)));
        assert_eq!(locate(body, &[Seg::Key("a"), Seg::Key("b"), Seg::Index(1)], Target::Value), Some((3, 14)));
        assert_eq!(key_location(body, &[Seg::Key("a"), Seg::Key("b"), Seg::Index(1)], "c"), Some((3, 15)));
        assert_eq!(key_location(body, &[], "zzz"), None);
        let body = br#"{"mappings":{"_doc":{"properties":{}}}}"#;
        assert_eq!(value_end(body, &[Seg::Key("mappings")]), Some((1, 38)));
        assert_eq!(value_end(br#"{"a":"xyz","b":1}"#, &[Seg::Key("a")]), Some((1, 10)));
    }
}
