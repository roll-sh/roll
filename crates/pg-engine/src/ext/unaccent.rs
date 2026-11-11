//! unaccent (`contrib/unaccent/unaccent.c`): a text search dictionary template replacing
//! characters by the rules file's substitutions, and `unaccent([dictionary,] text)`.

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, text_arg};
use crate::types::Value;
use crate::types::tsearch::config::{DictOption, ExtDict, Lexized, TSL_FILTER, TsLexeme, read_data_file};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "unaccent_dict" => unaccent_dict,
        // (Their `internal` arguments come only from the text search machinery.)
        "unaccent_init" | "unaccent_lexize" => |_, _| Err(crate::error::internal("unaccent support functions take internal arguments")),
        _ => return None,
    }))
}

/// A byte trie node (`TrieChar`): per byte, the replacement of the sequence ending there and
/// the longer sequences.
#[derive(Default)]
struct Trie {
    next: Vec<Option<Box<Trie>>>,
    replace: Vec<Option<Vec<u8>>>,
}

impl Trie {
    fn node() -> Trie {
        Trie {
            next: (0..256).map(|_| None).collect(),
            replace: vec![None; 256],
        }
    }

    /// `placeChar`.
    fn place(&mut self, src: &[u8], to: &[u8], warnings: &mut Vec<Error>) {
        let c = src[0] as usize;
        if src.len() <= 1 {
            if self.replace[c].is_some() {
                warnings.push(crate::session::warning(
                    code::CONFIG_FILE_ERROR,
                    "duplicate source strings, first one will be used",
                ));
            } else {
                self.replace[c] = Some(to.to_vec());
            }
        } else {
            self.next[c].get_or_insert_with(|| Box::new(Trie::node())).place(&src[1..], to, warnings);
        }
    }

    /// `findReplaceTo`: the longest sequence at the start of `src` with a replacement.
    fn find(&self, src: &[u8]) -> Option<(&[u8], usize)> {
        let mut node = Some(self);
        let mut found = None;
        let mut len = 0;
        while let Some(n) = node {
            if len >= src.len() {
                break;
            }
            let c = src[len] as usize;
            len += 1;
            if let Some(r) = &n.replace[c] {
                found = Some((r.as_slice(), len));
            }
            node = n.next[c].as_deref();
        }
        found
    }
}

/// `initTrie`: the rules file's lines, "src [trg]" (trg may be double-quoted).
fn init_trie(name: &str) -> Result<Trie> {
    let (_, lines) = read_data_file(name, "rules", "unaccent")?;
    let mut root = Trie::node();
    let mut warnings = Vec::new();
    for line in lines {
        let b = line.as_bytes();
        let (mut state, mut src, mut trg): (i32, (usize, usize), (usize, usize)) = (0, (0, 0), (0, 0));
        let mut quoted = false;
        let mut i = 0;
        while i < b.len() {
            let len = crate::types::tsearch::mblen(b[i]).min(b.len() - i).max(1);
            let c = b[i];
            if c.is_ascii_whitespace() || c == 0x0b {
                if state == 1 {
                    state = 2;
                } else if state == 3 {
                    state = 5;
                }
                if state != 4 {
                    i += len;
                    continue;
                }
            }
            match state {
                0 => {
                    src = (i, len);
                    state = 1;
                }
                1 => src.1 += len,
                2 => {
                    if c == b'"' {
                        quoted = true;
                        state = 4;
                    } else {
                        state = 3;
                    }
                    trg = (i, len);
                }
                3 => trg.1 += len,
                4 => {
                    trg.1 += len;
                    if c == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            i += 1;
                            trg.1 += 1;
                        } else {
                            state = 5;
                        }
                    }
                }
                _ => state = -1,
            }
            i += len;
        }
        if state == 1 || state == 2 {
            trg = (0, 0);
        }
        if state == 4 {
            state = -2;
        }
        let t = &b[trg.0..trg.0 + trg.1];
        let store: Vec<u8> = if quoted && state > 0 {
            let mut out = Vec::new();
            let mut k = 1;
            while k + 1 < t.len() {
                out.push(t[k]);
                if t[k] == b'"' && t.get(k + 1) == Some(&b'"') {
                    k += 1;
                }
                k += 1;
            }
            out
        } else {
            t.to_vec()
        };
        if state > 0 {
            root.place(&b[src.0..src.0 + src.1], &store, &mut warnings);
        } else if state == -1 {
            warnings.push(crate::session::warning(code::CONFIG_FILE_ERROR, "invalid syntax: more than two strings in unaccent rule"));
        } else if state == -2 {
            warnings.push(crate::session::warning(code::CONFIG_FILE_ERROR, "invalid syntax: unfinished quoted string in unaccent rule"));
        }
    }
    for w in warnings {
        crate::types::tsearch::queue(w);
    }
    Ok(root)
}

struct Unaccent(Trie);

impl ExtDict for Unaccent {
    /// `unaccent_lexize`: a lexeme only when something was replaced.
    fn lexize(&self, word: &str) -> Result<Lexized> {
        let src = word.as_bytes();
        let mut out: Option<Vec<u8>> = None;
        let mut i = 0;
        while i < src.len() {
            match self.0.find(&src[i..]) {
                Some((to, n)) => {
                    out.get_or_insert_with(|| src[..i].to_vec()).extend_from_slice(to);
                    i += n;
                }
                None => {
                    let n = crate::types::tsearch::mblen(src[i]).min(src.len() - i).max(1);
                    if let Some(o) = out.as_mut() {
                        o.extend_from_slice(&src[i..i + n]);
                    }
                    i += n;
                }
            }
        }
        Ok(out.map(|o| {
            vec![TsLexeme {
                nvariant: 0,
                flags: TSL_FILTER,
                lexeme: String::from_utf8_lossy(&o).into_owned(),
            }]
        }))
    }
}

/// `unaccent_init`: the `Rules` option's file.
pub(super) fn init(options: &[DictOption]) -> Result<Box<dyn ExtDict>> {
    let mut trie = None;
    for o in options {
        if o.name == "rules" {
            if trie.is_some() {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, "multiple Rules parameters"));
            }
            trie = Some(init_trie(&o.value)?);
        } else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized Unaccent parameter: \"{}\"", o.name),
            ));
        }
    }
    match trie {
        Some(t) => Ok(Box::new(Unaccent(t))),
        None => Err(Error::new(code::INVALID_PARAMETER_VALUE, "missing Rules parameter")),
    }
}

/// `unaccent_dict`: the dictionary's lexize of the text (the `unaccent` dictionary in the
/// function's schema without one), the text itself when nothing was replaced.
fn unaccent_dict(a: &[Value], c: &FnCtx) -> Result<Value> {
    let cat = c.exec.catalog();
    let (dict, s) = if a.len() == 1 {
        // (The function's schema is the extension's.)
        let ns = cat
            .extensions
            .iter()
            .find(|e| e.1 == "unaccent")
            .map_or(crate::catalog::PUBLIC_OID, |e| e.2);
        let found = cat.ts_oid_in(crate::catalog::tsearch::TsKind::Dict, ns, "unaccent");
        let Some(d) = found else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("text search dictionary \"{}.unaccent\" does not exist", cat.schema_name(ns)),
            ));
        };
        (d, text_arg(&a[0]))
    } else {
        (a[0].as_i64().unwrap_or(0) as u32, text_arg(&a[1]))
    };
    let lookup = crate::catalog::tsearch::TsCatalog {
        catalog: &cat,
        search_path: &c.exec.search_path,
    };
    let d = crate::types::tsearch::config::lookup_dict(&lookup, dict)?;
    Ok(match d.lexize(s, None)? {
        Some(l) if !l.is_empty() => Value::text(l[0].lexeme.clone()),
        _ => Value::text(s),
    })
}
