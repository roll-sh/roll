//! PostgreSQL's time zone library (`src/timezone/localtime.c`, `pgtz.c`) and its time zone
//! abbreviation table (`timezone_abbreviations`).
//!
//! Zone data is the reference server's tzdata (2026d), embedded as `zoneinfo.bin.zst`: every
//! TZif file of `/usr/share/zoneinfo` (v1 data stripped, duplicates shared), indexed by path.
//! It was built from the `roll-engine-ref` container by copying `/usr/share/zoneinfo` and
//! packing it as: `PGTZ1\n`, u32 name count, (u16 len, name, u32 blob index)*, u32 blob count,
//! (u32 len, TZif bytes)*, all little-endian, then `zstd -19`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

const TZ_MAX_TIMES: usize = 2000;
const TZ_MAX_TYPES: usize = 256;
const TZ_MAX_CHARS: usize = 50;
/// `sizeof(state.chars)`.
const CHARS_SIZE: usize = 2 * (TZ_STRLEN_MAX + 1);
pub const TZ_STRLEN_MAX: usize = 255;
const SECSPERDAY: i64 = 86_400;
const SECSPERHOUR: i32 = 3600;
const SECSPERMIN: i32 = 60;
const DAYSPERWEEK: i32 = 7;
const YEARSPERREPEAT: i64 = 400;
const AVGSECSPERYEAR: i64 = 31_556_952;
const SECSPERREPEAT: i64 = YEARSPERREPEAT * AVGSECSPERYEAR;
const EPOCH_YEAR: i32 = 1970;
/// The DST rules a POSIX TZ string without rules gets (US rules as of 2017).
const TZDEFRULESTRING: &str = ",M3.2.0,M11.1.0";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TtInfo {
    /// Seconds east of UTC.
    pub utoff: i32,
    pub isdst: bool,
    pub desigidx: usize,
    pub ttisstd: bool,
    pub ttisut: bool,
}

/// `struct state`.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub ats: Vec<i64>,
    pub types: Vec<u8>,
    pub ttis: Vec<TtInfo>,
    /// Abbreviations, each NUL-terminated (`charcnt` = `chars.len()`).
    pub chars: Vec<u8>,
    pub goback: bool,
    pub goahead: bool,
    pub defaulttype: usize,
}

impl State {
    fn abbr(&self, idx: usize) -> &str {
        let end = self.chars[idx..]
            .iter()
            .position(|&b| b == 0)
            .map_or(self.chars.len(), |p| idx + p);
        std::str::from_utf8(&self.chars[idx..end]).unwrap_or("")
    }

    fn timecnt(&self) -> usize {
        self.ats.len()
    }
}

/// A loaded zone (`pg_tz`).
#[derive(Debug)]
pub struct PgTz {
    /// Canonical spelling (`TZname`).
    pub name: String,
    pub state: State,
}

/// Broken-down local time as `pg_localtime` gives it (full year, month 1..12).
#[derive(Clone, Debug, Default)]
pub struct LocalTm {
    pub year: i32,
    pub mon: i32,
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    pub wday: i32,
    pub yday: i32,
    pub isdst: bool,
    /// Seconds east of UTC.
    pub gmtoff: i32,
    pub zone: String,
}

// ── embedded tzdata ──

struct ZoneDb {
    /// Upper-cased path → (canonical path, blob index).
    names: HashMap<String, (String, usize)>,
    /// Sorted canonical names.
    sorted: Vec<String>,
    blobs: Vec<Vec<u8>>,
}

fn zonedb() -> &'static ZoneDb {
    static DB: OnceLock<ZoneDb> = OnceLock::new();
    DB.get_or_init(|| {
        let raw = zstd::decode_all(&include_bytes!("zoneinfo.bin.zst")[..]).unwrap_or_default();
        let mut db = ZoneDb {
            names: HashMap::new(),
            sorted: Vec::new(),
            blobs: Vec::new(),
        };
        let mut p = 6usize;
        let u32_at = |p: usize| u32::from_le_bytes(raw[p..p + 4].try_into().unwrap()) as usize;
        if raw.len() < 10 || &raw[..6] != b"PGTZ1\n" {
            return db;
        }
        let n = u32_at(p);
        p += 4;
        for _ in 0..n {
            let len = u16::from_le_bytes(raw[p..p + 2].try_into().unwrap()) as usize;
            p += 2;
            let name = String::from_utf8_lossy(&raw[p..p + len]).into_owned();
            p += len;
            let idx = u32_at(p);
            p += 4;
            db.names
                .insert(name.to_ascii_uppercase(), (name.clone(), idx));
            db.sorted.push(name);
        }
        let nb = u32_at(p);
        p += 4;
        for _ in 0..nb {
            let len = u32_at(p);
            p += 4;
            db.blobs.push(raw[p..p + len].to_vec());
            p += len;
        }
        db.sorted.sort();
        db
    })
}

/// All zone names, as `pg_timezone_names` enumerates them.
pub fn zone_names() -> &'static [String] {
    &zonedb().sorted
}

// ── tzload ──

fn detzcode(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn detzcode64(b: &[u8]) -> i64 {
    i64::from_be_bytes(b[..8].try_into().unwrap())
}

/// tzloadbody from TZif bytes, extended with the footer TZ string (`doextend`).
fn tzload_bytes(data: &[u8]) -> Option<State> {
    let mut sp = State::default();
    let mut buf: &[u8] = data;
    let mut footer: Option<&[u8]> = None;
    let mut stored = 4usize;
    while stored <= 8 {
        if buf.len() < 44 || &buf[..4] != b"TZif" {
            return None;
        }
        let version = buf[4];
        let ttisutcnt = detzcode(&buf[20..]) as usize;
        let ttisstdcnt = detzcode(&buf[24..]) as usize;
        let leapcnt = detzcode(&buf[28..]) as usize;
        let timecnt = detzcode(&buf[32..]) as usize;
        let typecnt = detzcode(&buf[36..]) as usize;
        let charcnt = detzcode(&buf[40..]) as usize;
        if !(leapcnt < 50
            && typecnt < TZ_MAX_TYPES
            && timecnt < TZ_MAX_TIMES
            && charcnt < TZ_MAX_CHARS
            && (ttisstdcnt == typecnt || ttisstdcnt == 0)
            && (ttisutcnt == typecnt || ttisutcnt == 0))
        {
            return None;
        }
        let need = 44
            + timecnt * stored
            + timecnt
            + typecnt * 6
            + charcnt
            + leapcnt * (stored + 4)
            + ttisstdcnt
            + ttisutcnt;
        if buf.len() < need {
            return None;
        }
        let mut p = 44;
        let mut ats: Vec<i64> = Vec::with_capacity(timecnt);
        let mut keep: Vec<bool> = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            let at = if stored == 4 {
                detzcode(&buf[p..]) as i64
            } else {
                detzcode64(&buf[p..])
            };
            keep.push(true);
            if let Some(&last) = ats.last()
                && at <= last
            {
                if at < last {
                    return None;
                }
                let k = keep.len();
                keep[k - 2] = false;
                ats.pop();
            }
            ats.push(at);
            p += stored;
        }
        let mut types = Vec::with_capacity(ats.len());
        for &k in keep.iter() {
            let typ = buf[p];
            p += 1;
            if typ as usize >= typecnt {
                return None;
            }
            if k {
                types.push(typ);
            }
        }
        let mut ttis = Vec::with_capacity(typecnt);
        for _ in 0..typecnt {
            let utoff = detzcode(&buf[p..]);
            let isdst = buf[p + 4];
            let desigidx = buf[p + 5] as usize;
            p += 6;
            if isdst >= 2 || desigidx >= charcnt {
                return None;
            }
            ttis.push(TtInfo {
                utoff,
                isdst: isdst == 1,
                desigidx,
                ttisstd: false,
                ttisut: false,
            });
        }
        let mut chars = buf[p..p + charcnt].to_vec();
        p += charcnt;
        if chars.last() != Some(&0) {
            chars.push(0);
        }
        // Leap seconds: the tzdata we embed has none.
        p += leapcnt * (stored + 4);
        for t in ttis.iter_mut().take(ttisstdcnt) {
            t.ttisstd = buf[p] == 1;
            p += 1;
        }
        for t in ttis.iter_mut().take(ttisutcnt) {
            t.ttisut = buf[p] == 1;
            p += 1;
        }
        sp.ats = ats;
        sp.types = types;
        sp.ttis = ttis;
        sp.chars = chars;
        if version == 0 {
            break;
        }
        buf = &buf[p..];
        if stored == 8 {
            footer = Some(buf);
        }
        stored *= 2;
    }
    // Extend with the footer's POSIX TZ string.
    if let Some(f) = footer
        && f.len() > 2
        && f[0] == b'\n'
        && f[f.len() - 1] == b'\n'
        && sp.ttis.len() + 2 <= TZ_MAX_TYPES
        && let Ok(tzs) = std::str::from_utf8(&f[1..f.len() - 1])
        && let Some(mut ts) = tzparse(tzs, false)
    {
        let mut gotabbr = 0;
        let mut charcnt = sp.chars.len();
        for i in 0..ts.ttis.len() {
            let tsabbr = ts.abbr(ts.ttis[i].desigidx).as_bytes().to_vec();
            let mut found = None;
            let mut j = 0;
            while j < charcnt {
                let end = sp.chars[j..]
                    .iter()
                    .position(|&b| b == 0)
                    .map_or(sp.chars.len(), |q| j + q);
                if sp.chars[j..end] == tsabbr[..] {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            match found {
                Some(j) => {
                    ts.ttis[i].desigidx = j;
                    gotabbr += 1;
                }
                None => {
                    let j = charcnt;
                    if j + tsabbr.len() < TZ_MAX_CHARS {
                        sp.chars.truncate(j);
                        sp.chars.extend_from_slice(&tsabbr);
                        sp.chars.push(0);
                        charcnt = j + tsabbr.len() + 1;
                        ts.ttis[i].desigidx = j;
                        gotabbr += 1;
                    }
                }
            }
        }
        if gotabbr == ts.ttis.len() {
            sp.chars.truncate(charcnt);
            while sp.types.len() > 1
                && sp.types[sp.types.len() - 1] == sp.types[sp.types.len() - 2]
            {
                sp.types.pop();
                sp.ats.pop();
            }
            let mut i = 0;
            while i < ts.ats.len() {
                if sp.ats.is_empty() || sp.ats[sp.ats.len() - 1] < ts.ats[i] {
                    break;
                }
                i += 1;
            }
            let base = sp.ttis.len() as u8;
            while i < ts.ats.len() && sp.ats.len() < TZ_MAX_TIMES {
                sp.ats.push(ts.ats[i]);
                sp.types.push(base + ts.types[i]);
                i += 1;
            }
            sp.ttis.extend(ts.ttis.iter().copied());
        }
    }
    if sp.ttis.is_empty() {
        return None;
    }
    let n = sp.timecnt();
    if n > 1 {
        for i in 1..n {
            if typesequiv(&sp, sp.types[i] as usize, sp.types[0] as usize)
                && sp.ats[i] - sp.ats[0] == SECSPERREPEAT
            {
                sp.goback = true;
                break;
            }
        }
        for i in (0..n - 1).rev() {
            if typesequiv(&sp, sp.types[n - 1] as usize, sp.types[i] as usize)
                && sp.ats[n - 1] - sp.ats[i] == SECSPERREPEAT
            {
                sp.goahead = true;
                break;
            }
        }
    }
    // defaulttype heuristics.
    let mut i: i64 = if sp.types.contains(&0) { -1 } else { 0 };
    if i < 0 && n > 0 && sp.ttis[sp.types[0] as usize].isdst {
        i = sp.types[0] as i64;
        loop {
            i -= 1;
            if i < 0 || !sp.ttis[i as usize].isdst {
                break;
            }
        }
    }
    if i < 0 {
        i = 0;
        while sp.ttis[i as usize].isdst {
            i += 1;
            if i as usize >= sp.ttis.len() {
                i = 0;
                break;
            }
        }
    }
    sp.defaulttype = i as usize;
    Some(sp)
}

fn typesequiv(sp: &State, a: usize, b: usize) -> bool {
    if a >= sp.ttis.len() || b >= sp.ttis.len() {
        return false;
    }
    let (ap, bp) = (&sp.ttis[a], &sp.ttis[b]);
    ap.utoff == bp.utoff
        && ap.isdst == bp.isdst
        && ap.ttisstd == bp.ttisstd
        && ap.ttisut == bp.ttisut
        && sp.abbr(ap.desigidx) == sp.abbr(bp.desigidx)
}

// ── tzparse ──

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const MON_LENGTHS: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];
const YEAR_LENGTHS: [i32; 2] = [365, 366];

#[derive(Clone, Copy, Default)]
struct Rule {
    kind: u8, // 0 julian, 1 day of year, 2 month/week/day
    day: i32,
    week: i32,
    mon: i32,
    time: i32,
}

fn getzname(s: &[u8], mut i: usize) -> usize {
    while i < s.len() {
        let c = s[i];
        if c.is_ascii_digit() || c == b',' || c == b'-' || c == b'+' {
            break;
        }
        i += 1;
    }
    i
}

fn getqzname(s: &[u8], mut i: usize, delim: u8) -> usize {
    while i < s.len() && s[i] != delim {
        i += 1;
    }
    i
}

fn getnum(s: &[u8], i: usize, min: i32, max: i32) -> Option<(usize, i32)> {
    let mut i = i;
    if i >= s.len() || !s[i].is_ascii_digit() {
        return None;
    }
    let mut num: i32 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        num = num * 10 + (s[i] - b'0') as i32;
        if num > max {
            return None;
        }
        i += 1;
    }
    if num < min {
        return None;
    }
    Some((i, num))
}

fn getsecs(s: &[u8], i: usize) -> Option<(usize, i32)> {
    let (mut i, num) = getnum(s, i, 0, 24 * DAYSPERWEEK - 1)?;
    let mut secs = num * SECSPERHOUR;
    if s.get(i) == Some(&b':') {
        let (j, m) = getnum(s, i + 1, 0, 59)?;
        i = j;
        secs += m * SECSPERMIN;
        if s.get(i) == Some(&b':') {
            let (j, sec) = getnum(s, i + 1, 0, 60)?;
            i = j;
            secs += sec;
        }
    }
    Some((i, secs))
}

fn getoffset(s: &[u8], mut i: usize) -> Option<(usize, i32)> {
    let mut neg = false;
    if s.get(i) == Some(&b'-') {
        neg = true;
        i += 1;
    } else if s.get(i) == Some(&b'+') {
        i += 1;
    }
    let (i, v) = getsecs(s, i)?;
    Some((i, if neg { -v } else { v }))
}

fn getrule(s: &[u8], mut i: usize) -> Option<(usize, Rule)> {
    let mut r = Rule::default();
    match s.get(i) {
        Some(b'J') => {
            r.kind = 0;
            let (j, d) = getnum(s, i + 1, 1, 365)?;
            i = j;
            r.day = d;
        }
        Some(b'M') => {
            r.kind = 2;
            let (j, m) = getnum(s, i + 1, 1, 12)?;
            r.mon = m;
            if s.get(j) != Some(&b'.') {
                return None;
            }
            let (j, w) = getnum(s, j + 1, 1, 5)?;
            r.week = w;
            if s.get(j) != Some(&b'.') {
                return None;
            }
            let (j, d) = getnum(s, j + 1, 0, DAYSPERWEEK - 1)?;
            r.day = d;
            i = j;
        }
        Some(c) if c.is_ascii_digit() => {
            r.kind = 1;
            let (j, d) = getnum(s, i, 0, 365)?;
            i = j;
            r.day = d;
        }
        _ => return None,
    }
    if s.get(i) == Some(&b'/') {
        let (j, t) = getoffset(s, i + 1)?;
        i = j;
        r.time = t;
    } else {
        r.time = 2 * SECSPERHOUR;
    }
    Some((i, r))
}

fn transtime(year: i32, r: &Rule, offset: i32) -> i32 {
    let leap = is_leap(year as i64) as usize;
    let value = match r.kind {
        0 => {
            let mut v = (r.day - 1) * SECSPERDAY as i32;
            if leap == 1 && r.day >= 60 {
                v += SECSPERDAY as i32;
            }
            v
        }
        1 => r.day * SECSPERDAY as i32,
        _ => {
            let m1 = (r.mon + 9) % 12 + 1;
            let yy0 = if r.mon <= 2 { year - 1 } else { year };
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += DAYSPERWEEK;
            }
            let mut d = r.day - dow;
            if d < 0 {
                d += DAYSPERWEEK;
            }
            for _ in 1..r.week {
                if d + DAYSPERWEEK >= MON_LENGTHS[leap][(r.mon - 1) as usize] {
                    break;
                }
                d += DAYSPERWEEK;
            }
            let mut v = d * SECSPERDAY as i32;
            for len in MON_LENGTHS[leap].iter().take((r.mon - 1) as usize) {
                v += len * SECSPERDAY as i32;
            }
            v
        }
    };
    value + r.time + offset
}

/// tzparse: a POSIX TZ string ("EST5EDT", "<+05>-05", "PST8PDT,M3.2.0,M11.1.0").
pub fn tzparse(name: &str, lastditch: bool) -> Option<State> {
    let s = name.as_bytes();
    let mut sp = State::default();
    let (stdname, stdlen, mut i, stdoffset);
    if lastditch {
        stdname = 0;
        stdlen = s.len();
        i = s.len();
        stdoffset = 0;
    } else {
        if s.first() == Some(&b'<') {
            stdname = 1;
            let e = getqzname(s, 1, b'>');
            if s.get(e) != Some(&b'>') {
                return None;
            }
            stdlen = e - 1;
            i = e + 1;
        } else {
            stdname = 0;
            i = getzname(s, 0);
            stdlen = i;
        }
        if i >= s.len() {
            return None;
        }
        let (j, off) = getoffset(s, i)?;
        i = j;
        stdoffset = off;
    }
    let mut charcnt = stdlen + 1;
    if charcnt > CHARS_SIZE {
        return None;
    }
    let mut dst: Option<(usize, usize)> = None;
    if i < s.len() {
        let (dstname, dstlen);
        if s[i] == b'<' {
            dstname = i + 1;
            let e = getqzname(s, dstname, b'>');
            if s.get(e) != Some(&b'>') {
                return None;
            }
            dstlen = e - dstname;
            i = e + 1;
        } else {
            dstname = i;
            i = getzname(s, i);
            dstlen = i - dstname;
        }
        if dstlen == 0 {
            return None;
        }
        charcnt += dstlen + 1;
        if charcnt > CHARS_SIZE {
            return None;
        }
        let dstoffset;
        if i < s.len() && s[i] != b',' && s[i] != b';' {
            let (j, off) = getoffset(s, i)?;
            i = j;
            dstoffset = off;
        } else {
            dstoffset = stdoffset - SECSPERHOUR;
        }
        let rules: Vec<u8> = if i >= s.len() {
            TZDEFRULESTRING.as_bytes().to_vec()
        } else {
            s[i..].to_vec()
        };
        let r = &rules[..];
        if r.first() == Some(&b',') || r.first() == Some(&b';') {
            let (j, start) = getrule(r, 1)?;
            if r.get(j) != Some(&b',') {
                return None;
            }
            let (j, end) = getrule(r, j + 1)?;
            if j != r.len() {
                return None;
            }
            sp.ttis = vec![
                TtInfo {
                    utoff: -stdoffset,
                    isdst: false,
                    desigidx: 0,
                    ..Default::default()
                },
                TtInfo {
                    utoff: -dstoffset,
                    isdst: true,
                    desigidx: stdlen + 1,
                    ..Default::default()
                },
            ];
            sp.defaulttype = 0;
            let mut janfirst: i64 = 0;
            let mut janoffset: i64 = 0;
            let mut yearbeg = EPOCH_YEAR;
            loop {
                let yearsecs =
                    YEAR_LENGTHS[is_leap(yearbeg as i64 - 1) as usize] as i64 * SECSPERDAY;
                yearbeg -= 1;
                match janfirst.checked_add(-yearsecs) {
                    Some(v) => janfirst = v,
                    None => {
                        janoffset = -yearsecs;
                        break;
                    }
                }
                if EPOCH_YEAR - YEARSPERREPEAT as i32 / 2 >= yearbeg {
                    break;
                }
            }
            let mut yearlim = yearbeg + YEARSPERREPEAT as i32 + 1;
            let mut year = yearbeg;
            while year < yearlim {
                let mut starttime = transtime(year, &start, stdoffset);
                let mut endtime = transtime(year, &end, dstoffset);
                let yearsecs = YEAR_LENGTHS[is_leap(year as i64) as usize] * SECSPERDAY as i32;
                let reversed = endtime < starttime;
                if reversed {
                    std::mem::swap(&mut starttime, &mut endtime);
                }
                if reversed
                    || (starttime < endtime
                        && (endtime - starttime) < (yearsecs + (stdoffset - dstoffset)))
                {
                    if TZ_MAX_TIMES - 2 < sp.ats.len() {
                        break;
                    }
                    if let Some(t) = janfirst.checked_add(janoffset + starttime as i64) {
                        sp.ats.push(t);
                        sp.types.push(!reversed as u8);
                    }
                    if let Some(t) = janfirst.checked_add(janoffset + endtime as i64) {
                        sp.ats.push(t);
                        sp.types.push(reversed as u8);
                        yearlim = year + YEARSPERREPEAT as i32 + 1;
                    }
                }
                match janfirst.checked_add(janoffset + yearsecs as i64) {
                    Some(v) => janfirst = v,
                    None => break,
                }
                janoffset = 0;
                year += 1;
            }
            if sp.ats.is_empty() {
                sp.ttis[0] = sp.ttis[1];
                sp.ttis.truncate(1);
            } else if (YEARSPERREPEAT as i32) < year - yearbeg {
                sp.goback = true;
                sp.goahead = true;
            }
        } else {
            if !r.is_empty() {
                return None;
            }
            // Unreachable for us: no rules means TZDEFRULESTRING was substituted.
            sp.ttis = vec![
                TtInfo {
                    utoff: -stdoffset,
                    isdst: false,
                    desigidx: 0,
                    ..Default::default()
                },
                TtInfo {
                    utoff: -dstoffset,
                    isdst: true,
                    desigidx: stdlen + 1,
                    ..Default::default()
                },
            ];
        }
        dst = Some((dstname, dstlen));
    } else {
        sp.ttis = vec![TtInfo {
            utoff: -stdoffset,
            isdst: false,
            desigidx: 0,
            ..Default::default()
        }];
        sp.defaulttype = 0;
    }
    let mut chars = s[stdname..stdname + stdlen].to_vec();
    chars.push(0);
    if let Some((dn, dl)) = dst {
        chars.extend_from_slice(&s[dn..dn + dl]);
        chars.push(0);
    }
    let _ = charcnt;
    sp.chars = chars;
    Some(sp)
}

// ── pg_tzset ──

fn tz_cache() -> &'static Mutex<HashMap<String, Arc<PgTz>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<PgTz>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// pg_tzset: a zone by (case-insensitive) name, or a POSIX TZ spec.
pub fn pg_tzset(name: &str) -> Option<Arc<PgTz>> {
    if name.len() > TZ_STRLEN_MAX {
        return None;
    }
    let upper = name.to_ascii_uppercase();
    if let Some(tz) = tz_cache().lock().ok()?.get(&upper) {
        return Some(tz.clone());
    }
    let (canon, state) = if upper == "GMT" {
        (upper.clone(), tzparse(&upper, true)?)
    } else {
        let db = zonedb();
        let key = upper.strip_prefix(':').unwrap_or(&upper);
        match db
            .names
            .get(key)
            .and_then(|(canon, i)| Some((canon.clone(), tzload_bytes(&db.blobs[*i])?)))
        {
            Some(x) => x,
            None => {
                if upper.starts_with(':') {
                    return None;
                }
                (upper.clone(), tzparse(&upper, false)?)
            }
        }
    };
    let tz = Arc::new(PgTz { name: canon, state });
    if let Ok(mut c) = tz_cache().lock() {
        c.insert(upper, tz.clone());
    }
    Some(tz)
}

/// pg_tzset_offset: a fixed zone, `gmtoffset` seconds west of UTC (POSIX sign).
pub fn pg_tzset_offset(gmtoffset: i64) -> Option<Arc<PgTz>> {
    let mut abs = gmtoffset.unsigned_abs();
    let mut off = format!("{:02}", abs / 3600);
    abs %= 3600;
    if abs != 0 {
        off.push_str(&format!(":{:02}", abs / 60));
        abs %= 60;
        if abs != 0 {
            off.push_str(&format!(":{abs:02}"));
        }
    }
    let name = if gmtoffset > 0 {
        format!("<-{off}>+{off}")
    } else {
        format!("<+{off}>-{off}")
    };
    pg_tzset(&name)
}

/// A zone loaded without caching (pg_timezone_names).
pub fn load_zone(canon: &str) -> Option<PgTz> {
    let db = zonedb();
    let (c, i) = db.names.get(&canon.to_ascii_uppercase())?;
    Some(PgTz {
        name: c.clone(),
        state: tzload_bytes(&db.blobs[*i])?,
    })
}

// ── lookups ──

/// The ttinfo in effect at `t` (localsub's choice, with 400-year extrapolation).
fn ttinfo_at(sp: &State, t: i64) -> Option<&TtInfo> {
    let n = sp.timecnt();
    if (sp.goback && t < sp.ats[0]) || (sp.goahead && t > sp.ats[n - 1]) {
        let mut seconds = if t < sp.ats[0] {
            sp.ats[0] - t
        } else {
            t - sp.ats[n - 1]
        };
        seconds -= 1;
        let years = (seconds / SECSPERREPEAT + 1) * YEARSPERREPEAT;
        let seconds = years.checked_mul(AVGSECSPERYEAR)?;
        let newt = if t < sp.ats[0] {
            t.checked_add(seconds)?
        } else {
            t.checked_sub(seconds)?
        };
        if newt < sp.ats[0] || newt > sp.ats[n - 1] {
            return None;
        }
        return ttinfo_at(sp, newt);
    }
    let i = if n == 0 || t < sp.ats[0] {
        sp.defaulttype
    } else {
        let (mut lo, mut hi) = (1usize, n);
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if t < sp.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        sp.types[lo - 1] as usize
    };
    sp.ttis.get(i)
}

/// pg_localtime: `t` (Unix seconds) as local time in `tz`.
pub fn pg_localtime(t: i64, tz: &PgTz) -> Option<LocalTm> {
    let tt = ttinfo_at(&tz.state, t)?;
    let local = t.checked_add(tt.utoff as i64)?;
    let days = local.div_euclid(SECSPERDAY);
    let rem = local.rem_euclid(SECSPERDAY);
    let (y, m, d) = civil_from_days(days);
    let yday = days - days_from_civil(y, 1, 1);
    Some(LocalTm {
        year: i32::try_from(y).ok()?,
        mon: m as i32,
        mday: d as i32,
        hour: (rem / 3600) as i32,
        min: ((rem / 60) % 60) as i32,
        sec: (rem % 60) as i32,
        wday: (days + 4).rem_euclid(7) as i32,
        yday: yday as i32,
        isdst: tt.isdst,
        gmtoff: tt.utoff,
        zone: tz.state.abbr(tt.desigidx).to_string(),
    })
}

/// Proleptic Gregorian (year, month, day) of a day count from 1970-01-01 (any sign).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days from 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// pg_next_dst_boundary. Ok(None) = no boundary after `t` (but `before` holds), Err = failure.
#[allow(clippy::type_complexity, clippy::result_unit_err)]
pub fn pg_next_dst_boundary(
    t: i64,
    tz: &PgTz,
) -> Result<((i64, bool), Option<(i64, (i64, bool))>), ()> {
    let sp = &tz.state;
    let n = sp.timecnt();
    let info = |i: usize| (sp.ttis[i].utoff as i64, sp.ttis[i].isdst);
    if n == 0 {
        return Ok((info(sp.defaulttype), None));
    }
    if (sp.goback && t < sp.ats[0]) || (sp.goahead && t > sp.ats[n - 1]) {
        let mut seconds = if t < sp.ats[0] {
            sp.ats[0] - t
        } else {
            t - sp.ats[n - 1]
        };
        seconds -= 1;
        let tcycles = seconds / YEARSPERREPEAT / AVGSECSPERYEAR + 1;
        let seconds = tcycles
            .checked_mul(YEARSPERREPEAT)
            .and_then(|s| s.checked_mul(AVGSECSPERYEAR))
            .ok_or(())?;
        let newt = if t < sp.ats[0] {
            t + seconds
        } else {
            t - seconds
        };
        if newt < sp.ats[0] || newt > sp.ats[n - 1] {
            return Err(());
        }
        let (before, after) = pg_next_dst_boundary(newt, tz)?;
        let after = after.map(|(b, a)| {
            (
                if t < sp.ats[0] {
                    b - seconds
                } else {
                    b + seconds
                },
                a,
            )
        });
        return Ok((before, after));
    }
    if t >= sp.ats[n - 1] {
        return Ok((info(sp.types[n - 1] as usize), None));
    }
    if t < sp.ats[0] {
        return Ok((
            info(sp.defaulttype),
            Some((sp.ats[0], info(sp.types[0] as usize))),
        ));
    }
    let (mut lo, mut hi) = (1usize, n - 1);
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if t < sp.ats[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let i = lo;
    Ok((
        info(sp.types[i - 1] as usize),
        Some((sp.ats[i], info(sp.types[i] as usize))),
    ))
}

fn find_abbr(sp: &State, abbrev: &str) -> Option<usize> {
    let mut idx = 0;
    while idx < sp.chars.len() {
        if sp.abbr(idx) == abbrev {
            return Some(idx);
        }
        while idx < sp.chars.len() && sp.chars[idx] != 0 {
            idx += 1;
        }
        idx += 1;
    }
    None
}

/// pg_interpret_timezone_abbrev: (gmtoff east, isdst) of an upper-case abbreviation at `t`.
pub fn pg_interpret_timezone_abbrev(abbrev: &str, t: i64, tz: &PgTz) -> Option<(i64, bool)> {
    let sp = &tz.state;
    let abbrind = find_abbr(sp, abbrev)?;
    let (mut lo, mut hi) = (0usize, sp.timecnt());
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if t < sp.ats[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let cutoff = lo;
    for i in (0..cutoff).rev() {
        let tt = &sp.ttis[sp.types[i] as usize];
        if tt.desigidx == abbrind {
            return Some((tt.utoff as i64, tt.isdst));
        }
    }
    let tt = &sp.ttis[sp.defaulttype];
    if tt.desigidx == abbrind {
        return Some((tt.utoff as i64, tt.isdst));
    }
    for i in cutoff..sp.timecnt() {
        let tt = &sp.ttis[sp.types[i] as usize];
        if tt.desigidx == abbrind {
            return Some((tt.utoff as i64, tt.isdst));
        }
    }
    None
}

/// pg_timezone_abbrev_is_known: Some((isfixed, gmtoff east, isdst)).
pub fn pg_timezone_abbrev_is_known(abbrev: &str, tz: &PgTz) -> Option<(bool, i64, bool)> {
    let sp = &tz.state;
    let abbrind = find_abbr(sp, abbrev)?;
    let mut result: Option<(bool, i64, bool)> = None;
    for tt in &sp.ttis {
        if tt.desigidx == abbrind {
            match result {
                None => result = Some((true, tt.utoff as i64, tt.isdst)),
                Some((_, g, d)) => {
                    if g != tt.utoff as i64 || d != tt.isdst {
                        result = Some((false, g, d));
                        break;
                    }
                }
            }
        }
    }
    result
}

/// All abbreviations a zone uses, in its `chars` order.
pub fn zone_abbrevs(tz: &PgTz) -> Vec<String> {
    let sp = &tz.state;
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < sp.chars.len() {
        out.push(sp.abbr(idx).to_string());
        while idx < sp.chars.len() && sp.chars[idx] != 0 {
            idx += 1;
        }
        idx += 1;
    }
    out
}

/// pg_get_timezone_offset: the single offset (east) if the zone only ever uses one.
pub fn pg_get_timezone_offset(tz: &PgTz) -> Option<i64> {
    let sp = &tz.state;
    let first = sp.ttis.first()?.utoff;
    if sp.ttis.iter().any(|t| t.utoff != first) {
        return None;
    }
    Some(first as i64)
}

/// pg_tz_acceptable: no leap-second timekeeping (2000-01-01 00:00 UTC has seconds = 0).
pub fn pg_tz_acceptable(tz: &PgTz) -> bool {
    pg_localtime(946_684_800, tz).is_some_and(|t| t.sec == 0)
}

// ── timezone_abbreviations ──

/// Kind of a configured abbreviation.
#[derive(Clone, Debug)]
pub enum AbbrevKind {
    /// Fixed offset (seconds east), DST flag.
    Fixed(i32, bool),
    /// Dynamic: meaning depends on the date, from this zone.
    Dynamic(String),
}

#[derive(Clone, Debug)]
pub struct AbbrevEntry {
    /// Lower-case, truncated to TOKMAXLEN.
    pub token: String,
    pub kind: AbbrevKind,
}

fn parse_abbrev_file(text: &str, out: &mut Vec<AbbrevEntry>, override_ok: bool) {
    for line in text.lines() {
        let mut f = line.split('\t');
        let (Some(abbr), Some(val)) = (f.next(), f.next()) else {
            continue;
        };
        let dst = f.next() == Some("D");
        let mut token = abbr.to_ascii_lowercase();
        token.truncate(10);
        let kind = match val.parse::<i32>() {
            Ok(o) => AbbrevKind::Fixed(o, dst),
            Err(_) => AbbrevKind::Dynamic(val.to_string()),
        };
        let e = AbbrevEntry { token, kind };
        match out.binary_search_by(|x| x.token.as_bytes().cmp(e.token.as_bytes())) {
            Ok(i) => {
                if override_ok {
                    out[i] = e;
                }
            }
            Err(i) => out.insert(i, e),
        }
    }
}

/// The abbreviation table for a `timezone_abbreviations` setting, sorted by token.
pub fn abbrev_table(set: &str) -> Option<Arc<Vec<AbbrevEntry>>> {
    static TABLES: OnceLock<Mutex<HashMap<String, Arc<Vec<AbbrevEntry>>>>> = OnceLock::new();
    let key = set.to_string();
    let tables = TABLES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(t) = tables.lock().ok()?.get(&key) {
        return Some(t.clone());
    }
    let default = include_str!("Default.tsv");
    let mut v = Vec::new();
    match set {
        "Default" => parse_abbrev_file(default, &mut v, false),
        "Australia" => {
            parse_abbrev_file(default, &mut v, false);
            parse_abbrev_file(
                "cst\t34200\t\neast\t36000\t\nest\t36000\t\nsast\t34200\t\nsat\t34200\t\nwst\t28800\t",
                &mut v,
                true,
            );
        }
        "India" => {
            parse_abbrev_file(default, &mut v, false);
            parse_abbrev_file("ist\t19800\t", &mut v, true);
        }
        _ => return None,
    }
    let t = Arc::new(v);
    if let Ok(mut m) = tables.lock() {
        m.insert(key, t.clone());
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_zones() {
        let ny = pg_tzset("america/new_york").unwrap();
        assert_eq!(ny.name, "America/New_York");
        // 2024-07-01 12:00 UTC is EDT.
        let t = pg_localtime(1_719_835_200, &ny).unwrap();
        assert_eq!((t.hour, t.isdst, t.zone.as_str()), (8, true, "EDT"));
        let p = pg_tzset("PST8PDT").unwrap();
        assert!(pg_localtime(1_719_835_200, &p).unwrap().isdst);
        let f = pg_tzset_offset(-3600 * 5).unwrap();
        assert_eq!(f.name, "<+05>-05");
        assert_eq!(pg_localtime(0, &f).unwrap().gmtoff, 18000);
        let h = pg_tzset("Europe/Helsinki").unwrap();
        // Year 205000 (far beyond the table) still has DST in July.
        let t = (205_000i64 - 1970) * 31_556_952 + 190 * 86400;
        assert!(pg_localtime(t, &h).unwrap().isdst);
    }
}
