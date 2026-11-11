//! The thesaurus dictionary template (Postgres's dict_thesaurus.c): phrases of sample words
//! replaced by substitutes, matched a word at a time through `DictSubState`. Postgres links
//! `LexemeInfo` nodes with pointers and relinks `nextvariant` while matching; here they live
//! in an arena and `nextvariant` sits behind a lock.

use super::config::Dict;
use super::config::{
    DictOption, DictSubState, Lexized, TSL_ADDPOS, TsLexeme, TsLookup, bsearch, file_context,
    lookup_dict, read_data_file,
};
use crate::error::Result;
use crate::{bail, err};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

/// `DT_USEASIS`: a substitute word not to normalize.
const DT_USEASIS: u16 = 0x1000;

#[derive(Clone, Copy, Debug)]
struct LexemeInfo {
    idsubst: u32,
    posinsubst: u16,
    tnvariant: u16,
    nextentry: Option<usize>,
}

struct TheLexeme {
    lexeme: Option<String>,
    entries: usize,
}

struct TheSubstitute {
    lastlexeme: u16,
    res: Vec<TsLexeme>,
}

pub struct Thesaurus {
    subdict: Arc<Dict>,
    wrds: Vec<TheLexeme>,
    infos: Vec<LexemeInfo>,
    nextvariant: Mutex<Vec<Option<usize>>>,
    subst: Vec<TheSubstitute>,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// A sample word as read: (word, idsubst, posinsubst).
type RawWord = (String, u32, u16);

/// `thesaurusRead`.
fn read(file: &str, wrds: &mut Vec<RawWord>, subst: &mut Vec<(u16, Vec<TsLexeme>)>) -> Result<()> {
    let (path, lines) = read_data_file(file, "ths", "thesaurus")?;
    let mut idsubst: u32 = 0;
    let mut useasis = false;
    for (n, line) in lines.iter().enumerate() {
        read_line(line, &mut idsubst, &mut useasis, wrds, subst)
            .map_err(|e| file_context(e, n + 1, &path, line))?;
    }
    Ok(())
}

fn read_line(
    line: &str,
    idsubst: &mut u32,
    useasis: &mut bool,
    wrds: &mut Vec<RawWord>,
    subst: &mut Vec<(u16, Vec<TsLexeme>)>,
) -> Result<()> {
    const WAITLEX: u8 = 1;
    const INLEX: u8 = 2;
    const WAITSUBS: u8 = 3;
    const INSUBS: u8 = 4;
    let b = line.as_bytes();
    let mut p = 0;
    while p < b.len() && is_space(b[p]) {
        p += super::mblen(b[p]);
    }
    if p >= b.len() || matches!(b[p], b'#' | b'\n' | b'\r') {
        return Ok(());
    }
    let (mut state, mut begin, mut posinsubst, mut nwrd) = (WAITLEX, 0usize, 0u32, 0u32);
    let mut add_wrd = |begin: usize, end: usize, nwrd: u32, posinsubst: u32, useasis: bool| {
        if nwrd == 0 {
            subst.resize_with((*idsubst as usize + 1).max(subst.len()), || (0, Vec::new()));
            subst[*idsubst as usize].1.clear();
        }
        let s = &mut subst[*idsubst as usize];
        s.0 = (posinsubst as u16).wrapping_sub(1);
        s.1.push(TsLexeme {
            nvariant: nwrd as u16,
            flags: if useasis { DT_USEASIS } else { 0 },
            lexeme: line[begin..end].to_string(),
        });
    };
    while p < b.len() {
        let c = b[p];
        match state {
            WAITLEX => {
                if c == b':' {
                    if posinsubst == 0 {
                        bail!(CONFIG_FILE_ERROR, "unexpected delimiter");
                    }
                    state = WAITSUBS;
                } else if !is_space(c) {
                    begin = p;
                    state = INLEX;
                }
            }
            INLEX => {
                if c == b':' || is_space(c) {
                    wrds.push((line[begin..p].to_string(), *idsubst, posinsubst as u16));
                    posinsubst += 1;
                    state = if c == b':' { WAITSUBS } else { WAITLEX };
                }
            }
            WAITSUBS => {
                if c == b'*' || c == b'\\' {
                    *useasis = c == b'*';
                    state = INSUBS;
                    begin = p + 1;
                } else if !is_space(c) {
                    *useasis = false;
                    begin = p;
                    state = INSUBS;
                }
            }
            _ => {
                if is_space(c) {
                    if p == begin {
                        bail!(CONFIG_FILE_ERROR, "unexpected end of line or lexeme");
                    }
                    add_wrd(begin, p, nwrd, posinsubst, *useasis);
                    nwrd += 1;
                    state = WAITSUBS;
                }
            }
        }
        p += super::mblen(c).min(b.len() - p);
    }
    if state == INSUBS {
        if p == begin {
            bail!(CONFIG_FILE_ERROR, "unexpected end of line or lexeme");
        }
        add_wrd(begin, p, nwrd, posinsubst, *useasis);
        nwrd += 1;
    }
    *idsubst += 1;
    if nwrd == 0 || posinsubst == 0 {
        bail!(CONFIG_FILE_ERROR, "unexpected end of line");
    }
    if nwrd > u16::MAX as u32 || posinsubst > u16::MAX as u32 {
        bail!(CONFIG_FILE_ERROR, "too many lexemes in thesaurus entry");
    }
    Ok(())
}

/// `cmpLexeme`: NULL (the stopword marker) sorts last.
fn cmp_lexeme(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.as_bytes().cmp(y.as_bytes()),
    }
}

fn cmp_info(a: &LexemeInfo, b: &LexemeInfo) -> Ordering {
    a.idsubst
        .cmp(&b.idsubst)
        .then(a.posinsubst.cmp(&b.posinsubst))
        .then(a.tnvariant.cmp(&b.tnvariant))
}

impl Thesaurus {
    pub fn init(options: &[DictOption], lookup: &dyn TsLookup) -> Result<Thesaurus> {
        let mut raw: Vec<RawWord> = Vec::new();
        let mut subst: Vec<(u16, Vec<TsLexeme>)> = Vec::new();
        let (mut loaded, mut subname): (bool, Option<String>) = (false, None);
        for o in options {
            match o.name.as_str() {
                "dictfile" => {
                    if loaded {
                        bail!(INVALID_PARAMETER_VALUE, "multiple DictFile parameters");
                    }
                    read(&o.value, &mut raw, &mut subst)?;
                    loaded = true;
                }
                "dictionary" => {
                    if subname.is_some() {
                        bail!(INVALID_PARAMETER_VALUE, "multiple Dictionary parameters");
                    }
                    subname = Some(o.value.clone());
                }
                _ => bail!(
                    INVALID_PARAMETER_VALUE,
                    "unrecognized Thesaurus parameter: \"{}\"",
                    o.name
                ),
            }
        }
        if !loaded {
            bail!(INVALID_PARAMETER_VALUE, "missing DictFile parameter");
        }
        let Some(subname) = subname else {
            bail!(INVALID_PARAMETER_VALUE, "missing Dictionary parameter");
        };
        let subdict = lookup_dict(lookup, lookup.dict_oid(&subname)?)?;
        let mut t = Thesaurus {
            subdict,
            wrds: Vec::new(),
            infos: Vec::new(),
            nextvariant: Mutex::new(Vec::new()),
            subst: Vec::new(),
        };
        t.compile_lexemes(raw)?;
        t.compile_substitutes(subst)?;
        *t.nextvariant.lock().unwrap() = vec![None; t.infos.len()];
        Ok(t)
    }

    /// `compileTheLexeme`: normalize the sample words, sort, merge duplicates' entries.
    fn compile_lexemes(&mut self, raw: Vec<RawWord>) -> Result<()> {
        let mut words: Vec<(Option<String>, LexemeInfo)> = Vec::new();
        for (word, idsubst, posinsubst) in raw {
            let info = |tnvariant| LexemeInfo {
                idsubst,
                posinsubst,
                tnvariant,
                nextentry: None,
            };
            if word == "?" {
                words.push((None, info(1)));
                continue;
            }
            match self.subdict.lexize(&word, None)? {
                None => bail!(
                    CONFIG_FILE_ERROR,
                    "thesaurus sample word \"{word}\" isn't recognized by subdictionary (rule {})",
                    idsubst + 1
                ),
                Some(r) if r.is_empty() => {
                    return Err(err!(
                        CONFIG_FILE_ERROR,
                        "thesaurus sample word \"{word}\" is a stop word (rule {})",
                        idsubst + 1
                    )
                    .hint("Use \"?\" to represent a stop word within a sample phrase."));
                }
                Some(r) => {
                    let mut k = 0;
                    while k < r.len() {
                        let cur = r[k].nvariant;
                        // The number of words in this variant.
                        let mut tnvar = 1;
                        let mut m = k + 1;
                        while m < r.len() && r[m].nvariant == r[m - 1].nvariant {
                            tnvar += 1;
                            m += 1;
                        }
                        let mut m = k;
                        while m < r.len() && r[m].nvariant == cur {
                            words.push((Some(r[m].lexeme.clone()), info(tnvar)));
                            m += 1;
                        }
                        k = m;
                    }
                }
            }
        }
        // By lexeme, then entries descending, so prepending builds ascending lists.
        words.sort_by(|a, b| cmp_lexeme(a.0.as_deref(), b.0.as_deref()).then(cmp_info(&b.1, &a.1)));
        let mut wrds: Vec<TheLexeme> = Vec::new();
        for (lexeme, info) in words {
            let same = wrds.last().is_some_and(|l| {
                cmp_lexeme(l.lexeme.as_deref(), lexeme.as_deref()) == Ordering::Equal
            });
            if same {
                let last = wrds.last_mut().unwrap();
                if cmp_info(&info, &self.infos[last.entries]) != Ordering::Equal {
                    self.infos.push(LexemeInfo {
                        nextentry: Some(last.entries),
                        ..info
                    });
                    last.entries = self.infos.len() - 1;
                }
            } else {
                self.infos.push(info);
                wrds.push(TheLexeme {
                    lexeme,
                    entries: self.infos.len() - 1,
                });
            }
        }
        self.wrds = wrds;
        Ok(())
    }

    /// `compileTheSubstitute`: normalize the substitutes (unless marked as-is).
    fn compile_substitutes(&mut self, subst: Vec<(u16, Vec<TsLexeme>)>) -> Result<()> {
        for (i, (lastlexeme, words)) in subst.into_iter().enumerate() {
            let mut out: Vec<TsLexeme> = Vec::new();
            for w in words {
                let lexized = if w.flags & DT_USEASIS != 0 {
                    Some(vec![TsLexeme {
                        flags: 0,
                        ..w.clone()
                    }])
                } else {
                    self.subdict.lexize(&w.lexeme, None)?
                };
                match lexized {
                    Some(l) if !l.is_empty() => {
                        let toset = if out.is_empty() {
                            None
                        } else {
                            Some(out.len())
                        };
                        out.extend(l);
                        if let Some(t) = toset {
                            out[t].flags |= TSL_ADDPOS;
                        }
                    }
                    Some(_) => bail!(
                        CONFIG_FILE_ERROR,
                        "thesaurus substitute word \"{}\" is a stop word (rule {})",
                        w.lexeme,
                        i + 1
                    ),
                    None => bail!(
                        CONFIG_FILE_ERROR,
                        "thesaurus substitute word \"{}\" isn't recognized by subdictionary (rule {})",
                        w.lexeme,
                        i + 1
                    ),
                }
            }
            if out.is_empty() {
                bail!(
                    CONFIG_FILE_ERROR,
                    "thesaurus substitute phrase is empty (rule {})",
                    i + 1
                );
            }
            self.subst.push(TheSubstitute {
                lastlexeme,
                res: out,
            });
        }
        Ok(())
    }

    fn find(&self, lexeme: Option<&str>) -> Option<usize> {
        if self.wrds.is_empty() {
            return None;
        }
        bsearch(&self.wrds, |w| cmp_lexeme(w.lexeme.as_deref(), lexeme)).map(|w| w.entries)
    }

    fn match_id_subst(&self, nv: &[Option<usize>], stored: Option<usize>, idsubst: u32) -> bool {
        let Some(mut s) = stored else { return true };
        loop {
            if self.infos[s].idsubst == idsubst {
                return true;
            }
            match nv[s] {
                Some(n) => s = n,
                None => return false,
            }
        }
    }

    /// `findVariant`: the entries at `curpos` all of a variant's words share, prepended to `in`.
    fn find_variant(
        &self,
        nv: &mut [Option<usize>],
        mut inp: Option<usize>,
        stored: Option<usize>,
        curpos: u16,
        newin: &mut [Option<usize>],
    ) -> Option<usize> {
        let newn = newin.len();
        let next = |x: usize| self.infos[x].nextentry;
        loop {
            let Some(mut ptr) = newin[0] else { return inp };
            let mut i: isize = 0;
            while (i as usize) < newn {
                let k = i as usize;
                while let Some(x) = newin[k] {
                    if self.infos[x].idsubst < self.infos[ptr].idsubst {
                        newin[k] = next(x);
                    } else {
                        break;
                    }
                }
                let Some(mut x) = newin[k] else { return inp };
                if self.infos[x].idsubst > self.infos[ptr].idsubst {
                    ptr = x;
                    i = 0;
                    continue;
                }
                while self.infos[x].idsubst == self.infos[ptr].idsubst {
                    if self.infos[x].posinsubst == curpos
                        && self.infos[x].tnvariant as usize == newn
                    {
                        ptr = x;
                        break;
                    }
                    newin[k] = next(x);
                    match newin[k] {
                        Some(y) => x = y,
                        None => return inp,
                    }
                }
                if self.infos[x].idsubst != self.infos[ptr].idsubst {
                    ptr = x;
                    i = 0;
                    continue;
                }
                i += 1;
            }
            let id = self.infos[ptr].idsubst;
            if self.match_id_subst(nv, stored, id)
                && (inp.is_none() || !self.match_id_subst(nv, inp, id))
            {
                nv[ptr] = inp;
                inp = Some(ptr);
            }
            for n in newin.iter_mut() {
                *n = n.and_then(next);
            }
        }
    }

    pub fn lexize(&self, word: &str, state: Option<&mut DictSubState>) -> Result<Lexized> {
        let Some(st) = state else {
            return Err(crate::error::internal(
                "forbidden call of thesaurus or nested call",
            ));
        };
        if st.isend {
            return Ok(None);
        }
        let stored = st.private;
        let curpos = stored.map_or(0, |s| self.infos[s].posinsubst.wrapping_add(1));
        let res = self.subdict.lexize(word, None)?;
        let mut nv = self.nextvariant.lock().unwrap();
        let mut info = None;
        match &res {
            Some(r) if !r.is_empty() => {
                let mut k = 0;
                while k < r.len() {
                    let base = k;
                    let nvar = r[k].nvariant;
                    while k < r.len() && r[k].nvariant == nvar {
                        k += 1;
                    }
                    let mut infos: Vec<Option<usize>> = Vec::with_capacity(k - base);
                    for l in &r[base..k] {
                        match self.find(Some(&l.lexeme)) {
                            Some(e) => infos.push(Some(e)),
                            None => break,
                        }
                    }
                    if infos.len() < k - base {
                        continue;
                    }
                    info = self.find_variant(&mut nv, info, stored, curpos, &mut infos);
                }
            }
            Some(_) => {
                let mut infos = [self.find(None)];
                info = self.find_variant(&mut nv, None, stored, curpos, &mut infos);
            }
            None => {}
        }
        st.private = info;
        let Some(mut i) = info else {
            st.getnext = false;
            return Ok(None);
        };
        // `checkMatch`.
        let mut moreres = false;
        loop {
            if nv[i].is_some() {
                moreres = true;
            }
            let s = &self.subst[self.infos[i].idsubst as usize];
            if s.lastlexeme == curpos {
                st.getnext = moreres;
                return Ok(Some(s.res.clone()));
            }
            match nv[i] {
                Some(n) => i = n,
                None => break,
            }
        }
        st.getnext = true;
        Ok(None)
    }
}
