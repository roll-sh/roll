//! `ts_headline` (the default parser's prsd_headline in wparser_def.c, generateHeadline in
//! ts_parse.c, and deserialize_deflist in tsearchcmds.c).

use super::TsQuery;
use super::config::{HlText, HlWord};
use super::exec::{EXEC_EMPTY, PhraseData, Tern, ts_execute, ts_execute_locations};
use super::parser::{
    ASCIIHWORD, DECIMAL_T, HWORD, NUMHWORD, PROTOCOL, SCIENTIFIC, SIGNEDINT, SPACE, TAG_T,
    UNSIGNEDINT, URL_T, VERSIONNUMBER, XMLENTITY,
};
use crate::bail;
use crate::error::Result;

fn hlidreplace(t: u8) -> bool {
    t == TAG_T
}

fn hlidskip(t: u8) -> bool {
    matches!(t, URL_T | NUMHWORD | ASCIIHWORD | HWORD)
}

fn nonwordtoken(t: u8) -> bool {
    t == SPACE || hlidreplace(t) || hlidskip(t)
}

fn ts_idignore(t: u8) -> bool {
    matches!(t, TAG_T | PROTOCOL | SPACE | XMLENTITY)
}

fn noendtoken(t: u8) -> bool {
    nonwordtoken(t)
        || matches!(
            t,
            SCIENTIFIC | VERSIONNUMBER | DECIMAL_T | SIGNEDINT | UNSIGNEDINT
        )
        || ts_idignore(t)
}

struct Sel<'a> {
    w: &'a mut Vec<HlWord>,
    shortword: i32,
}

impl Sel<'_> {
    fn interesting(&self, j: i32) -> bool {
        let w = &self.w[j as usize];
        w.item.is_some() && !w.repeated
    }

    fn badend(&self, j: i32) -> bool {
        let w = &self.w[j as usize];
        (noendtoken(w.ty) || (w.len as u16 as i32) <= self.shortword) && !self.interesting(j)
    }

    fn nonword(&self, j: i32) -> bool {
        nonwordtoken(self.w[j as usize].ty)
    }

    fn len(&self) -> i32 {
        self.w.len() as i32
    }
}

/// `checkcondition_HL`: the words of `words` that hold the operand, with their positions.
fn check_hl(words: &[HlWord], at: usize, data: Option<&mut PhraseData>) -> Tern {
    let mut data = data;
    for w in words {
        if w.item == Some(at) {
            match data.as_deref_mut() {
                None => return Tern::Yes,
                Some(d) => {
                    if d.pos.is_empty() || *d.pos.last().unwrap() < w.pos {
                        d.pos.push(w.pos);
                    }
                }
            }
        }
    }
    match data {
        Some(d) if !d.pos.is_empty() => Tern::Yes,
        _ => Tern::No,
    }
}

/// `hlCover`: the next minimal word range satisfying the query.
fn hl_cover(
    words: &[HlWord],
    query: &TsQuery,
    locations: &[PhraseData],
    nextpos: &mut i32,
) -> Option<(i32, i32)> {
    let mut pos = *nextpos;
    loop {
        let mut pose = -1;
        for pd in locations {
            let first = pd.pos.iter().map(|p| *p as i32).find(|p| *p >= pos)?;
            pose = pose.max(first);
        }
        if pose < 0 {
            return None;
        }
        let mut posb = i32::MAX - 1;
        for pd in locations {
            let last = pd
                .pos
                .iter()
                .rev()
                .map(|p| *p as i32 - pd.width)
                .find(|p| *p <= pose)
                .unwrap_or(-1);
            posb = posb.min(last);
        }
        posb = posb.max(pos);
        if posb <= pose {
            let (mut idxb, mut idxe) = (-1i32, -1i32);
            for (i, w) in words.iter().enumerate() {
                if w.item.is_none() {
                    continue;
                }
                if idxb < 0 && w.pos as i32 >= posb {
                    idxb = i as i32;
                }
                if w.pos as i32 <= pose {
                    idxe = i as i32;
                } else {
                    break;
                }
            }
            if idxb >= 0 && idxe >= idxb {
                let range = &words[idxb as usize..=idxe as usize];
                if ts_execute(&query.items, EXEC_EMPTY, &mut |at, _, d| {
                    check_hl(range, at, d)
                }) {
                    *nextpos = posb + 1;
                    return Some((idxb, idxe));
                }
            }
        }
        pos = posb + 1;
    }
}

/// `mark_fragment`.
fn mark_fragment(w: &mut [HlWord], highlightall: bool, start: i32, end: i32) {
    for i in start.max(0)..=end {
        let x = &mut w[i as usize];
        if x.item.is_some() {
            x.selected = true;
        }
        if !highlightall {
            if hlidreplace(x.ty) {
                x.replace = true;
            } else if hlidskip(x.ty) {
                x.skip = true;
            }
        } else if hlidskip(x.ty) {
            x.skip = true;
        }
        x.in_ = !x.repeated;
    }
}

/// `get_next_fragment`.
fn next_fragment(s: &Sel, start: &mut i32, end: &mut i32, max_words: i32) -> (i32, i32) {
    let mut i = *start;
    while i <= *end {
        *start = i;
        if s.interesting(i) {
            break;
        }
        i += 1;
    }
    let (mut curlen, mut poslen) = (0, 0);
    i = *start;
    while i <= *end && curlen < max_words {
        if !s.nonword(i) {
            curlen += 1;
        }
        if s.interesting(i) {
            poslen += 1;
        }
        i += 1;
    }
    if *end > i {
        *end = i;
        while i >= *start {
            *end = i;
            if s.interesting(i) {
                break;
            }
            if !s.nonword(i) {
                curlen -= 1;
            }
            i -= 1;
        }
    }
    (curlen, poslen)
}

#[derive(Clone, Copy)]
struct CoverPos {
    startpos: i32,
    endpos: i32,
    poslen: i32,
    curlen: i32,
    chosen: bool,
    excluded: bool,
}

#[allow(clippy::too_many_arguments)]
fn mark_hl_fragments(
    s: &mut Sel,
    query: &TsQuery,
    locations: &[PhraseData],
    highlightall: bool,
    min_words: i32,
    max_words: i32,
    max_fragments: i32,
) {
    let mut covers: Vec<CoverPos> = Vec::new();
    let mut nextpos = 0;
    while let Some((p, q)) = hl_cover(s.w, query, locations, &mut nextpos) {
        let (mut startpos, mut endpos) = (p, q);
        while startpos <= endpos {
            let (curlen, poslen) = next_fragment(s, &mut startpos, &mut endpos, max_words);
            covers.push(CoverPos {
                startpos,
                endpos,
                curlen,
                poslen,
                chosen: false,
                excluded: false,
            });
            startpos = endpos + 1;
            endpos = q;
        }
    }
    let mut num_f = 0;
    for _ in 0..max_fragments {
        let (mut maxitems, mut minwords, mut min_i) = (0, i32::MAX, -1i32);
        for (i, c) in covers.iter().enumerate() {
            if !c.chosen
                && !c.excluded
                && (maxitems < c.poslen || (maxitems == c.poslen && minwords > c.curlen))
            {
                maxitems = c.poslen;
                minwords = c.curlen;
                min_i = i as i32;
            }
        }
        if min_i < 0 {
            break;
        }
        let m = min_i as usize;
        covers[m].chosen = true;
        let (mut startpos, mut endpos, mut curlen) =
            (covers[m].startpos, covers[m].endpos, covers[m].curlen);
        if curlen < max_words {
            let maxstretch = (max_words - curlen) / 2;
            let mut stretch = 0;
            let mut posmarker = startpos;
            let mut i = startpos - 1;
            while i >= 0 && stretch < maxstretch && !s.w[i as usize].in_ {
                if !s.nonword(i) {
                    curlen += 1;
                    stretch += 1;
                }
                posmarker = i;
                i -= 1;
            }
            i = posmarker;
            while i < startpos && s.badend(i) {
                if !s.nonword(i) {
                    curlen -= 1;
                }
                i += 1;
            }
            startpos = i;
            posmarker = endpos;
            i = endpos + 1;
            while i < s.len() && curlen < max_words && !s.w[i as usize].in_ {
                if !s.nonword(i) {
                    curlen += 1;
                }
                posmarker = i;
                i += 1;
            }
            i = posmarker;
            while i > endpos && s.badend(i) {
                if !s.nonword(i) {
                    curlen -= 1;
                }
                i -= 1;
            }
            endpos = i;
        }
        covers[m].startpos = startpos;
        covers[m].endpos = endpos;
        covers[m].curlen = curlen;
        mark_fragment(s.w, highlightall, startpos, endpos);
        num_f += 1;
        for (i, c) in covers.iter_mut().enumerate() {
            if i != m
                && ((c.startpos >= startpos && c.startpos <= endpos)
                    || (c.endpos >= startpos && c.endpos <= endpos)
                    || (c.startpos < startpos && c.endpos > endpos))
            {
                c.excluded = true;
            }
        }
    }
    if num_f <= 0 {
        let (mut curlen, mut endpos) = (0, -1);
        let mut i = 0;
        while i < s.len() && curlen < min_words {
            if !s.nonword(i) {
                curlen += 1;
            }
            endpos = i;
            i += 1;
        }
        mark_fragment(s.w, highlightall, 0, endpos);
    }
}

fn mark_hl_words(
    s: &mut Sel,
    query: &TsQuery,
    locations: &[PhraseData],
    highlightall: bool,
    min_words: i32,
    max_words: i32,
) {
    let (mut bestb, mut beste, mut bestlen, mut bestcover) = (-1i32, -1i32, -1i32, false);
    if !highlightall {
        let mut nextpos = 0;
        while let Some((p, q)) = hl_cover(s.w, query, locations, &mut nextpos) {
            let (mut curlen, mut poslen) = (0, 0);
            let (mut posb, mut pose) = (p, p);
            let mut i = p;
            while i <= q && curlen < max_words {
                if !s.nonword(i) {
                    curlen += 1;
                }
                if s.interesting(i) {
                    poslen += 1;
                }
                pose = i;
                i += 1;
            }
            if curlen < max_words {
                i -= 1;
                while i < s.len() && curlen < max_words {
                    if i > q {
                        if !s.nonword(i) {
                            curlen += 1;
                        }
                        if s.interesting(i) {
                            poslen += 1;
                        }
                    }
                    pose = i;
                    if s.badend(i) {
                        i += 1;
                        continue;
                    }
                    if curlen >= min_words {
                        break;
                    }
                    i += 1;
                }
                if curlen < min_words {
                    i = p - 1;
                    while i >= 0 {
                        if !s.nonword(i) {
                            curlen += 1;
                        }
                        if s.interesting(i) {
                            poslen += 1;
                        }
                        if curlen >= max_words {
                            break;
                        }
                        if s.badend(i) {
                            i -= 1;
                            continue;
                        }
                        if curlen >= min_words {
                            break;
                        }
                        i -= 1;
                    }
                    posb = if i >= 0 { i } else { 0 };
                }
            } else {
                if i > q {
                    i = q;
                }
                while curlen > min_words {
                    if !s.badend(i) {
                        break;
                    }
                    if !s.nonword(i) {
                        curlen -= 1;
                    }
                    if s.interesting(i) {
                        poslen -= 1;
                    }
                    pose = i - 1;
                    i -= 1;
                }
            }
            let poscover = posb <= p && pose >= q;
            if (poscover && !bestcover)
                || (poscover == bestcover && poslen > bestlen)
                || (poscover == bestcover
                    && poslen == bestlen
                    && !s.badend(pose)
                    && s.badend(beste))
            {
                bestb = posb;
                beste = pose;
                bestlen = poslen;
                bestcover = poscover;
            }
        }
        if bestlen < 0 {
            let (mut curlen, mut pose) = (0, -1);
            let mut i = 0;
            while i < s.len() && curlen < min_words {
                if !s.nonword(i) {
                    curlen += 1;
                }
                pose = i;
                i += 1;
            }
            bestb = 0;
            beste = pose;
        }
    } else {
        bestb = 0;
        beste = s.len() - 1;
    }
    mark_fragment(s.w, highlightall, bestb, beste);
}

/// `pg_strtoint32` of an option value.
fn option_int(v: &str) -> Result<i32> {
    Ok(crate::types::text::parse_int(v, "integer", i32::MIN as i64, i32::MAX as i64)? as i32)
}

/// `prsd_headline`: choose and mark the headline's words; sets the markers.
pub fn prsd_headline(prs: &mut HlText, opts: &[(String, String)], query: &TsQuery) -> Result<()> {
    let (mut min_words, mut max_words, mut shortword, mut max_fragments) = (15, 35, 3, 0);
    let mut highlightall = false;
    let (mut startsel, mut stopsel, mut fragdelim) = (None, None, None);
    for (name, val) in opts {
        match name.to_ascii_lowercase().as_str() {
            "maxwords" => max_words = option_int(val)?,
            "minwords" => min_words = option_int(val)?,
            "shortword" => shortword = option_int(val)?,
            "maxfragments" => max_fragments = option_int(val)?,
            "startsel" => startsel = Some(val.clone()),
            "stopsel" => stopsel = Some(val.clone()),
            "fragmentdelimiter" => fragdelim = Some(val.clone()),
            "highlightall" => {
                highlightall = matches!(
                    val.to_ascii_lowercase().as_str(),
                    "1" | "on" | "true" | "t" | "y" | "yes"
                )
            }
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "unrecognized headline parameter: \"{name}\""
            ),
        }
    }
    if !highlightall {
        if min_words >= max_words {
            bail!(
                INVALID_PARAMETER_VALUE,
                "MinWords must be less than MaxWords"
            );
        }
        if min_words <= 0 {
            bail!(INVALID_PARAMETER_VALUE, "MinWords must be positive");
        }
        if shortword < 0 {
            bail!(INVALID_PARAMETER_VALUE, "ShortWord must be >= 0");
        }
        if max_fragments < 0 {
            bail!(INVALID_PARAMETER_VALUE, "MaxFragments must be >= 0");
        }
    }
    let locations = if query.items.is_empty() {
        Vec::new()
    } else {
        let words = &prs.words;
        ts_execute_locations(&query.items, &mut |at, _, d| check_hl(words, at, d))
    };
    let mut s = Sel {
        w: &mut prs.words,
        shortword,
    };
    if max_fragments == 0 {
        mark_hl_words(
            &mut s,
            query,
            &locations,
            highlightall,
            min_words,
            max_words,
        );
    } else {
        mark_hl_fragments(
            &mut s,
            query,
            &locations,
            highlightall,
            min_words,
            max_words,
            max_fragments,
        );
    }
    prs.startsel = startsel.unwrap_or_else(|| "<b>".into());
    prs.stopsel = stopsel.unwrap_or_else(|| "</b>".into());
    prs.fragdelim = fragdelim.unwrap_or_else(|| " ... ".into());
    for (v, n) in [
        (&prs.startsel, "StartSel"),
        (&prs.stopsel, "StopSel"),
        (&prs.fragdelim, "FragmentDelimiter"),
    ] {
        if v.len() > i16::MAX as usize {
            bail!(INVALID_PARAMETER_VALUE, "value for \"{n}\" is too long");
        }
    }
    Ok(())
}

/// `generateHeadline`.
pub fn generate(prs: &HlText) -> String {
    let mut out = String::new();
    let (mut infrag, mut numfragments) = (false, 0);
    for w in &prs.words {
        if w.in_ && !w.repeated {
            if !infrag {
                infrag = true;
                numfragments += 1;
                if numfragments > 1 {
                    out.push_str(&prs.fragdelim);
                }
            }
            if w.replace {
                out.push(' ');
            } else if !w.skip {
                if w.selected {
                    out.push_str(&prs.startsel);
                }
                out.push_str(&w.word);
                if w.selected {
                    out.push_str(&prs.stopsel);
                }
            }
        } else if !w.repeated {
            infrag = false;
        }
    }
    out
}

/// One parsed option: (name, value as `defGetString` gives it, was it quoted).
fn build_def_item(name: &str, val: &str, quoted: bool) -> (String, String) {
    if !quoted && !val.is_empty() {
        // An integer prints back without leading zeros or a plus sign.
        let t = val.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
        if let Ok(v) = t.parse::<i64>()
            && i32::try_from(v).is_ok()
            && !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'+')
        {
            return (name.to_string(), v.to_string());
        }
    }
    (name.to_string(), val.to_string())
}

/// `deserialize_deflist`: `name = value, ...` as `ts_headline` options take it.
pub fn deserialize_deflist(s: &str) -> Result<Vec<(String, String)>> {
    #[derive(PartialEq)]
    enum St {
        WaitKey,
        InKey,
        InQKey,
        WaitEq,
        WaitValue,
        InSqValue,
        InDqValue,
        InWValue,
    }
    let bad = || crate::err!(SYNTAX_ERROR, "invalid parameter list format: \"{s}\"");
    let isspace = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut key, mut val): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    let mut st = St::WaitKey;
    let mut i = 0;
    let text = |v: &[u8]| String::from_utf8_lossy(v).into_owned();
    while i < b.len() {
        let c = b[i];
        match st {
            St::WaitKey => {
                if !(isspace(c) || c == b',') {
                    key.clear();
                    if c == b'"' {
                        st = St::InQKey;
                    } else {
                        key.push(c);
                        st = St::InKey;
                    }
                }
            }
            St::InKey => {
                if isspace(c) {
                    st = St::WaitEq;
                } else if c == b'=' {
                    st = St::WaitValue;
                } else {
                    key.push(c);
                }
            }
            St::InQKey => {
                if c == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        key.push(c);
                        i += 1;
                    } else {
                        st = St::WaitEq;
                    }
                } else {
                    key.push(c);
                }
            }
            St::WaitEq => {
                if c == b'=' {
                    st = St::WaitValue;
                } else if !isspace(c) {
                    return Err(bad());
                }
            }
            St::WaitValue => {
                val.clear();
                if c == b'\'' {
                    st = St::InSqValue;
                } else if c == b'E' && b.get(i + 1) == Some(&b'\'') {
                    i += 1;
                    st = St::InSqValue;
                } else if c == b'"' {
                    st = St::InDqValue;
                } else if !isspace(c) {
                    val.push(c);
                    st = St::InWValue;
                }
            }
            St::InSqValue => {
                if c == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        val.push(c);
                        i += 1;
                    } else {
                        out.push(build_def_item(&text(&key), &text(&val), true));
                        st = St::WaitKey;
                    }
                } else if c == b'\\' {
                    if b.get(i + 1) == Some(&b'\\') {
                        i += 1;
                    }
                    val.push(c);
                } else {
                    val.push(c);
                }
            }
            St::InDqValue => {
                if c == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        val.push(c);
                        i += 1;
                    } else {
                        out.push(build_def_item(&text(&key), &text(&val), true));
                        st = St::WaitKey;
                    }
                } else {
                    val.push(c);
                }
            }
            St::InWValue => {
                if c == b',' || isspace(c) {
                    out.push(build_def_item(&text(&key), &text(&val), false));
                    st = St::WaitKey;
                } else {
                    val.push(c);
                }
            }
        }
        i += 1;
    }
    if st == St::InWValue {
        out.push(build_def_item(&text(&key), &text(&val), false));
    } else if st != St::WaitKey {
        return Err(bad());
    }
    Ok(out)
}
