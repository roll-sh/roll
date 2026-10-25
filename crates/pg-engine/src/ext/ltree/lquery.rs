//! ltree's `lquery` (`parse_lquery`, `deparse_lquery` in `ltree_io.c`) and its matching
//! (`lquery_op.c`).

use super::data::{LQL_COUNT, LQL_NOT, LTREE_MAX_LEVELS, LVAR_ANYEND, LVAR_INCASE, LVAR_SUBLEXEME, is_label};
use crate::error::{Error, Result, code};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variant {
    pub name: Vec<u8>,
    pub flag: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Level {
    pub flag: u16,
    pub low: u16,
    pub high: u16,
    pub vars: Vec<Variant>,
}

const MAX: u16 = LTREE_MAX_LEVELS as u16;

pub fn encode(levels: &[Level]) -> Vec<u8> {
    let mut out = Vec::new();
    let put = |out: &mut Vec<u8>, v: u16| out.extend_from_slice(&v.to_le_bytes());
    for l in levels {
        put(&mut out, l.flag);
        put(&mut out, l.low);
        put(&mut out, l.high);
        put(&mut out, l.vars.len() as u16);
        for v in &l.vars {
            put(&mut out, v.flag);
            put(&mut out, v.name.len() as u16);
            out.extend_from_slice(&v.name);
        }
    }
    out
}

pub fn decode(b: &[u8]) -> Vec<Level> {
    fn get(b: &[u8], i: &mut usize) -> u16 {
        let v = u16::from_le_bytes([b[*i], b[*i + 1]]);
        *i += 2;
        v
    }
    let mut i = 0;
    let mut out = Vec::new();
    while i + 8 <= b.len() {
        let (flag, low, high, n) = (get(b, &mut i), get(b, &mut i), get(b, &mut i), get(b, &mut i));
        let mut vars = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let (vf, len) = (get(b, &mut i), get(b, &mut i) as usize);
            vars.push(Variant { name: b[i..i + len].to_vec(), flag: vf });
            i += len;
        }
        out.push(Level { flag, low, high, vars });
    }
    out
}

#[derive(PartialEq, Clone, Copy)]
enum St {
    Level,
    Delim,
    Open,
    Fnum,
    Snum,
    Nd,
    Close,
    End,
    Var,
}

fn unchar(pos: i64) -> Error {
    Error::new(code::SYNTAX_ERROR, format!("lquery syntax error at character {pos}"))
}

/// atoi at byte `i`: the digits there.
fn atoi(b: &[u8], i: usize) -> i64 {
    let mut v: i64 = 0;
    for &c in &b[i..] {
        if !c.is_ascii_digit() {
            break;
        }
        v = (v * 10 + (c - b'0') as i64).min(i32::MAX as i64 + 1);
    }
    v
}

struct Var {
    start: usize,
    end: usize,
    flag: u16,
    wlen: i64,
}

/// `finish_nodeitem` for an lquery variant ending at byte `at`.
fn finish(b: &[u8], v: &mut Var, mut at: usize, mut pos: i64) -> Result<()> {
    while at > v.start && matches!(b[at - 1], b'@' | b'*' | b'%') {
        at -= 1;
        v.wlen -= 1;
        pos -= 1;
    }
    v.end = at;
    if v.end == v.start {
        return Err(unchar(pos).detail("Empty labels are not allowed."));
    }
    if v.wlen > 1000 {
        return Err(Error::new(code::NAME_TOO_LONG, "label string is too long").detail(format!(
            "Label length is {}, must be at most 1000, at character {pos}.",
            v.wlen
        )));
    }
    Ok(())
}

/// `parse_lquery`.
pub fn parse(s: &str) -> Result<Vec<Level>> {
    let b = s.as_bytes();
    let num = s.chars().filter(|&c| c == '.').count() + 1;
    if num > LTREE_MAX_LEVELS {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("number of lquery items ({num}) exceeds the maximum allowed ({LTREE_MAX_LEVELS})"),
        ));
    }
    let mut levels: Vec<(Level, Vec<Var>)> = (0..num).map(|_| (Level::default(), Vec::new())).collect();
    let mut cur = 0usize;
    let mut state = St::Level;
    let mut pos: i64 = 1;
    for (i, c) in s.char_indices() {
        let (lv, vars) = &mut levels[cur];
        match state {
            St::Level => {
                if is_label(c) {
                    vars.push(Var { start: i, end: i, flag: 0, wlen: 0 });
                    state = St::Delim;
                } else if c == '!' {
                    vars.push(Var { start: i + 1, end: i + 1, flag: 0, wlen: -1 });
                    state = St::Delim;
                    lv.flag |= LQL_NOT;
                } else if c == '*' {
                    state = St::Open;
                } else {
                    return Err(unchar(pos));
                }
            }
            St::Var => {
                if is_label(c) {
                    if vars.len() >= u16::MAX as usize {
                        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "lquery level has too many variants")
                            .detail(format!("Number of variants exceeds the maximum allowed ({}).", u16::MAX)));
                    }
                    vars.push(Var { start: i, end: i, flag: 0, wlen: 0 });
                    state = St::Delim;
                } else {
                    return Err(unchar(pos));
                }
            }
            St::Delim => {
                let v = vars.last_mut().unwrap();
                match c {
                    '@' => {
                        v.flag |= LVAR_INCASE;
                        lv.flag |= LVAR_INCASE;
                    }
                    '*' => {
                        v.flag |= LVAR_ANYEND;
                        lv.flag |= LVAR_ANYEND;
                    }
                    '%' => {
                        v.flag |= LVAR_SUBLEXEME;
                        lv.flag |= LVAR_SUBLEXEME;
                    }
                    '|' => {
                        finish(b, v, i, pos)?;
                        state = St::Var;
                    }
                    '{' => {
                        finish(b, v, i, pos)?;
                        lv.flag |= LQL_COUNT;
                        state = St::Fnum;
                    }
                    '.' => {
                        finish(b, v, i, pos)?;
                        state = St::Level;
                        cur += 1;
                    }
                    c if is_label(c) => {
                        if v.flag != 0 {
                            return Err(unchar(pos));
                        }
                    }
                    _ => return Err(unchar(pos)),
                }
            }
            St::Open => {
                if c == '{' {
                    state = St::Fnum;
                } else if c == '.' {
                    lv.low = 0;
                    lv.high = MAX;
                    cur += 1;
                    state = St::Level;
                } else {
                    return Err(unchar(pos));
                }
            }
            St::Fnum => {
                if c == ',' {
                    state = St::Snum;
                } else if c.is_ascii_digit() {
                    let low = atoi(b, i);
                    if low > LTREE_MAX_LEVELS as i64 {
                        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "lquery syntax error").detail(format!(
                            "Low limit ({}) exceeds the maximum allowed ({LTREE_MAX_LEVELS}), at character {pos}.",
                            low as i32
                        )));
                    }
                    lv.low = low as u16;
                    state = St::Nd;
                } else {
                    return Err(unchar(pos));
                }
            }
            St::Snum => {
                if c.is_ascii_digit() {
                    let high = atoi(b, i);
                    if high > LTREE_MAX_LEVELS as i64 {
                        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "lquery syntax error").detail(format!(
                            "High limit ({}) exceeds the maximum allowed ({LTREE_MAX_LEVELS}), at character {pos}.",
                            high as i32
                        )));
                    } else if (lv.low as i64) > high {
                        return Err(Error::new(code::SYNTAX_ERROR, "lquery syntax error").detail(format!(
                            "Low limit ({}) is greater than high limit ({high}), at character {pos}.",
                            lv.low
                        )));
                    }
                    lv.high = high as u16;
                    state = St::Close;
                } else if c == '}' {
                    lv.high = MAX;
                    state = St::End;
                } else {
                    return Err(unchar(pos));
                }
            }
            St::Close => {
                if c == '}' {
                    state = St::End;
                } else if !c.is_ascii_digit() {
                    return Err(unchar(pos));
                }
            }
            St::Nd => {
                if c == '}' {
                    lv.high = lv.low;
                    state = St::End;
                } else if c == ',' {
                    state = St::Snum;
                } else if !c.is_ascii_digit() {
                    return Err(unchar(pos));
                }
            }
            St::End => {
                if c == '.' {
                    state = St::Level;
                    cur += 1;
                } else {
                    return Err(unchar(pos));
                }
            }
        }
        if state == St::Delim {
            levels[cur].1.last_mut().unwrap().wlen += 1;
        }
        pos += 1;
    }
    match state {
        St::Delim => {
            let v = levels[cur].1.last_mut().unwrap();
            finish(b, v, b.len(), pos)?;
        }
        St::Open => levels[cur].0.high = MAX,
        St::End => {}
        _ => {
            return Err(Error::new(code::SYNTAX_ERROR, "lquery syntax error").detail("Unexpected end of input."));
        }
    }
    // A level's stored size (LQL_HDRSIZE, then each MAXALIGN'd variant) must fit its
    // uint16 totallen.
    let maxalign = |n: usize| (n + 7) & !7;
    for (_, vars) in &levels {
        let mut total = 16usize;
        for v in vars {
            total += maxalign(8 + (v.end - v.start));
            if total > u16::MAX as usize {
                return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "lquery level is too large")
                    .detail(format!("Total size of level exceeds the maximum allowed ({} bytes).", u16::MAX)));
            }
        }
    }
    Ok(levels
        .into_iter()
        .map(|(mut l, vars)| {
            l.vars = vars
                .into_iter()
                .map(|v| Variant { name: b[v.start..v.end].to_vec(), flag: v.flag })
                .collect();
            l
        })
        .collect())
}

/// `deparse_lquery`.
pub fn output(levels: &[Level]) -> String {
    let mut out = String::new();
    for (i, l) in levels.iter().enumerate() {
        if i != 0 {
            out.push('.');
        }
        if l.vars.is_empty() {
            out.push('*');
        } else {
            if l.flag & LQL_NOT != 0 {
                out.push('!');
            }
            for (j, v) in l.vars.iter().enumerate() {
                if j != 0 {
                    out.push('|');
                }
                out.push_str(&String::from_utf8_lossy(&v.name));
                if v.flag & LVAR_SUBLEXEME != 0 {
                    out.push('%');
                }
                if v.flag & LVAR_INCASE != 0 {
                    out.push('@');
                }
                if v.flag & LVAR_ANYEND != 0 {
                    out.push('*');
                }
            }
        }
        if l.flag & LQL_COUNT != 0 || l.vars.is_empty() {
            if l.low == l.high {
                out.push_str(&format!("{{{}}}", l.low));
            } else if l.low == 0 {
                if l.high == MAX {
                    if !l.vars.is_empty() {
                        out.push_str("{,}");
                    }
                } else {
                    out.push_str(&format!("{{,{}}}", l.high));
                }
            } else if l.high == MAX {
                out.push_str(&format!("{{{},}}", l.low));
            } else {
                out.push_str(&format!("{{{},{}}}", l.low, l.high));
            }
        }
    }
    out
}
