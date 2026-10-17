//! Double Metaphone (`deps/phonetics/double_metaphone.c`) as the search module runs it:
//! ASCII uppercasing only, codes cut to four characters, prefixed `<`.

struct Word {
    s: Vec<u8>,
    length: i32,
    last: i32,
}

impl Word {
    fn at(&self, pos: i32) -> u8 {
        if pos < 0 || pos as usize >= self.s.len() { 0 } else { self.s[pos as usize] }
    }

    fn vowel(&self, pos: i32) -> bool {
        matches!(self.at(pos), b'A' | b'E' | b'I' | b'O' | b'U' | b'Y')
    }

    /// `StringAt`: any of `tests` (each `len` long) at `start`.
    fn has(&self, start: i32, tests: &[&str]) -> bool {
        if start < 0 || start as usize >= self.s.len() {
            return false;
        }
        let rest = &self.s[start as usize..];
        tests.iter().any(|t| rest.starts_with(t.as_bytes()))
    }

    fn slavo_germanic(&self) -> bool {
        let has = |n: &[u8]| self.s.windows(n.len()).any(|w| w == n);
        has(b"W") || has(b"K") || has(b"CZ")
    }
}

/// The primary code, `<`-prefixed (`PhoneticManager_ExpandPhonetics`), if any.
pub fn primary(s: &[u8]) -> Option<Vec<u8>> {
    let (p, _) = double_metaphone(s);
    if p.is_empty() {
        return None;
    }
    let mut out = vec![super::tokenize::PHONETIC_PREFIX];
    out.extend_from_slice(&p);
    Some(out)
}

/// `DoubleMetaphone`: (primary, secondary), each at most four characters.
pub fn double_metaphone(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let input = crate::util::cstr(input);
    let length = input.len() as i32;
    let mut s: Vec<u8> = input.iter().map(u8::to_ascii_uppercase).collect();
    s.extend_from_slice(b"     ");
    let w = Word { s, length, last: length - 1 };
    let (last, length) = (w.last, w.length);
    let mut p: Vec<u8> = Vec::new();
    let mut q: Vec<u8> = Vec::new();
    let mut cur: i32 = 0;
    macro_rules! add {
        ($a:expr, $b:expr) => {{
            p.extend_from_slice($a.as_bytes());
            q.extend_from_slice($b.as_bytes());
        }};
    }

    if w.has(0, &["GN", "KN", "PN", "WR", "PS"]) {
        cur += 1;
    }
    if w.at(0) == b'X' {
        add!("S", "S");
        cur += 1;
    }

    while p.len() < 4 || q.len() < 4 {
        if cur >= length {
            break;
        }
        match w.at(cur) {
            b'A' | b'E' | b'I' | b'O' | b'U' | b'Y' => {
                if cur == 0 {
                    add!("A", "A");
                }
                cur += 1;
            }
            b'B' => {
                add!("P", "P");
                cur += if w.at(cur + 1) == b'B' { 2 } else { 1 };
            }
            b'C' => 'c: {
                if cur > 1
                    && !w.vowel(cur - 2)
                    && w.has(cur - 1, &["ACH"])
                    && w.at(cur + 2) != b'I'
                    && (w.at(cur + 2) != b'E' || w.has(cur - 2, &["BACHER", "MACHER"]))
                {
                    add!("K", "K");
                    cur += 2;
                    break 'c;
                }
                if cur == 0 && w.has(cur, &["CAESAR"]) {
                    add!("S", "S");
                    cur += 2;
                    break 'c;
                }
                if w.has(cur, &["CHIA"]) {
                    add!("K", "K");
                    cur += 2;
                    break 'c;
                }
                if w.has(cur, &["CH"]) {
                    if cur > 0 && w.has(cur, &["CHAE"]) {
                        add!("K", "X");
                        cur += 2;
                        break 'c;
                    }
                    if cur == 0
                        && (w.has(cur + 1, &["HARAC", "HARIS"]) || w.has(cur + 1, &["HOR", "HYM", "HIA", "HEM"]))
                        && !w.has(0, &["CHORE"])
                    {
                        add!("K", "K");
                        cur += 2;
                        break 'c;
                    }
                    if (w.has(0, &["VAN ", "VON "]) || w.has(0, &["SCH"]))
                        || w.has(cur - 2, &["ORCHES", "ARCHIT", "ORCHID"])
                        || w.has(cur + 2, &["T", "S"])
                        || ((w.has(cur - 1, &["A", "O", "U", "E"]) || cur == 0)
                            && w.has(cur + 2, &["L", "R", "N", "M", "B", "H", "F", "V", "W", " "]))
                    {
                        add!("K", "K");
                    } else if cur > 0 {
                        if w.has(0, &["MC"]) {
                            add!("K", "K");
                        } else {
                            add!("X", "K");
                        }
                    } else {
                        add!("X", "X");
                    }
                    cur += 2;
                    break 'c;
                }
                if w.has(cur, &["CZ"]) && !w.has(cur - 2, &["WICZ"]) {
                    add!("S", "X");
                    cur += 2;
                    break 'c;
                }
                if w.has(cur + 1, &["CIA"]) {
                    add!("X", "X");
                    cur += 3;
                    break 'c;
                }
                if w.has(cur, &["CC"]) && !(cur == 1 && w.at(0) == b'M') {
                    if w.has(cur + 2, &["I", "E", "H"]) && !w.has(cur + 2, &["HU"]) {
                        if (cur == 1 && w.at(cur - 1) == b'A') || w.has(cur - 1, &["UCCEE", "UCCES"]) {
                            add!("KS", "KS");
                        } else {
                            add!("X", "X");
                        }
                        cur += 3;
                    } else {
                        add!("K", "K");
                        cur += 2;
                    }
                    break 'c;
                }
                if w.has(cur, &["CK", "CG", "CQ"]) {
                    add!("K", "K");
                    cur += 2;
                    break 'c;
                }
                if w.has(cur, &["CI", "CE", "CY"]) {
                    if w.has(cur, &["CIO", "CIE", "CIA"]) {
                        add!("S", "X");
                    } else {
                        add!("S", "S");
                    }
                    cur += 2;
                    break 'c;
                }
                add!("K", "K");
                if w.has(cur + 1, &[" C", " Q", " G"]) {
                    cur += 3;
                } else if w.has(cur + 1, &["C", "K", "Q"]) && !w.has(cur + 1, &["CE", "CI"]) {
                    cur += 2;
                } else {
                    cur += 1;
                }
            }
            b'D' => {
                if w.has(cur, &["DG"]) {
                    if w.has(cur + 2, &["I", "E", "Y"]) {
                        add!("J", "J");
                        cur += 3;
                    } else {
                        add!("TK", "TK");
                        cur += 2;
                    }
                } else if w.has(cur, &["DT", "DD"]) {
                    add!("T", "T");
                    cur += 2;
                } else {
                    add!("T", "T");
                    cur += 1;
                }
            }
            b'F' => {
                cur += if w.at(cur + 1) == b'F' { 2 } else { 1 };
                add!("F", "F");
            }
            b'G' => 'g: {
                if w.at(cur + 1) == b'H' {
                    if cur > 0 && !w.vowel(cur - 1) {
                        add!("K", "K");
                        cur += 2;
                        break 'g;
                    }
                    if cur == 0 {
                        if w.at(cur + 2) == b'I' {
                            add!("J", "J");
                        } else {
                            add!("K", "K");
                        }
                        cur += 2;
                        break 'g;
                    }
                    if (cur > 1 && w.has(cur - 2, &["B", "H", "D"]))
                        || (cur > 2 && w.has(cur - 3, &["B", "H", "D"]))
                        || (cur > 3 && w.has(cur - 4, &["B", "H"]))
                    {
                        cur += 2;
                        break 'g;
                    }
                    if cur > 2 && w.at(cur - 1) == b'U' && w.has(cur - 3, &["C", "G", "L", "R", "T"]) {
                        add!("F", "F");
                    } else if cur > 0 && w.at(cur - 1) != b'I' {
                        add!("K", "K");
                    }
                    cur += 2;
                    break 'g;
                }
                if w.at(cur + 1) == b'N' {
                    if cur == 1 && w.vowel(0) && !w.slavo_germanic() {
                        add!("KN", "N");
                    } else if !w.has(cur + 2, &["EY"]) && w.at(cur + 1) != b'Y' && !w.slavo_germanic() {
                        add!("N", "KN");
                    } else {
                        add!("KN", "KN");
                    }
                    cur += 2;
                    break 'g;
                }
                if w.has(cur + 1, &["LI"]) && !w.slavo_germanic() {
                    add!("KL", "L");
                    cur += 2;
                    break 'g;
                }
                if cur == 0
                    && (w.at(cur + 1) == b'Y'
                        || w.has(cur + 1, &["ES", "EP", "EB", "EL", "EY", "IB", "IL", "IN", "IE", "EI", "ER"]))
                {
                    add!("K", "J");
                    cur += 2;
                    break 'g;
                }
                if (w.has(cur + 1, &["ER"]) || w.at(cur + 1) == b'Y')
                    && !w.has(0, &["DANGER", "RANGER", "MANGER"])
                    && !w.has(cur - 1, &["E", "I"])
                    && !w.has(cur - 1, &["RGY", "OGY"])
                {
                    add!("K", "J");
                    cur += 2;
                    break 'g;
                }
                if w.has(cur + 1, &["E", "I", "Y"]) || w.has(cur - 1, &["AGGI", "OGGI"]) {
                    if (w.has(0, &["VAN ", "VON "]) || w.has(0, &["SCH"])) || w.has(cur + 1, &["ET"]) {
                        add!("K", "K");
                    } else if w.has(cur + 1, &["IER "]) {
                        add!("J", "J");
                    } else {
                        add!("J", "K");
                    }
                    cur += 2;
                    break 'g;
                }
                cur += if w.at(cur + 1) == b'G' { 2 } else { 1 };
                add!("K", "K");
            }
            b'H' => {
                if (cur == 0 || w.vowel(cur - 1)) && w.vowel(cur + 1) {
                    add!("H", "H");
                    cur += 2;
                } else {
                    cur += 1;
                }
            }
            b'J' => 'j: {
                if w.has(cur, &["JOSE"]) || w.has(0, &["SAN "]) {
                    if (cur == 0 && w.at(cur + 4) == b' ') || w.has(0, &["SAN "]) {
                        add!("H", "H");
                    } else {
                        add!("J", "H");
                    }
                    cur += 1;
                    break 'j;
                }
                if cur == 0 && !w.has(cur, &["JOSE"]) {
                    add!("J", "A");
                } else if w.vowel(cur - 1) && !w.slavo_germanic() && (w.at(cur + 1) == b'A' || w.at(cur + 1) == b'O') {
                    add!("J", "H");
                } else if cur == last {
                    add!("J", "");
                } else if !w.has(cur + 1, &["L", "T", "K", "S", "N", "M", "B", "Z"]) && !w.has(cur - 1, &["S", "K", "L"]) {
                    add!("J", "J");
                }
                cur += if w.at(cur + 1) == b'J' { 2 } else { 1 };
            }
            b'K' => {
                cur += if w.at(cur + 1) == b'K' { 2 } else { 1 };
                add!("K", "K");
            }
            b'L' => 'l: {
                if w.at(cur + 1) == b'L' {
                    if (cur == length - 3 && w.has(cur - 1, &["ILLO", "ILLA", "ALLE"]))
                        || ((w.has(last - 1, &["AS", "OS"]) || w.has(last, &["A", "O"])) && w.has(cur - 1, &["ALLE"]))
                    {
                        add!("L", "");
                        cur += 2;
                        break 'l;
                    }
                    cur += 2;
                } else {
                    cur += 1;
                }
                add!("L", "L");
            }
            b'M' => {
                if (w.has(cur - 1, &["UMB"]) && (cur + 1 == last || w.has(cur + 2, &["ER"]))) || w.at(cur + 1) == b'M' {
                    cur += 2;
                } else {
                    cur += 1;
                }
                add!("M", "M");
            }
            b'N' => {
                cur += if w.at(cur + 1) == b'N' { 2 } else { 1 };
                add!("N", "N");
            }
            b'P' => {
                if w.at(cur + 1) == b'H' {
                    add!("F", "F");
                    cur += 2;
                } else {
                    cur += if w.has(cur + 1, &["P", "B"]) { 2 } else { 1 };
                    add!("P", "P");
                }
            }
            b'Q' => {
                cur += if w.at(cur + 1) == b'Q' { 2 } else { 1 };
                add!("K", "K");
            }
            b'R' => {
                if cur == last && !w.slavo_germanic() && w.has(cur - 2, &["IE"]) && !w.has(cur - 4, &["ME", "MA"]) {
                    add!("", "R");
                } else {
                    add!("R", "R");
                }
                cur += if w.at(cur + 1) == b'R' { 2 } else { 1 };
            }
            b'S' => 's: {
                if w.has(cur - 1, &["ISL", "YSL"]) {
                    cur += 1;
                    break 's;
                }
                if cur == 0 && w.has(cur, &["SUGAR"]) {
                    add!("X", "S");
                    cur += 1;
                    break 's;
                }
                if w.has(cur, &["SH"]) {
                    if w.has(cur + 1, &["HEIM", "HOEK", "HOLM", "HOLZ"]) {
                        add!("S", "S");
                    } else {
                        add!("X", "X");
                    }
                    cur += 2;
                    break 's;
                }
                if w.has(cur, &["SIO", "SIA"]) || w.has(cur, &["SIAN"]) {
                    if !w.slavo_germanic() {
                        add!("S", "X");
                    } else {
                        add!("S", "S");
                    }
                    cur += 3;
                    break 's;
                }
                if (cur == 0 && w.has(cur + 1, &["M", "N", "L", "W"])) || w.has(cur + 1, &["Z"]) {
                    add!("S", "X");
                    cur += if w.has(cur + 1, &["Z"]) { 2 } else { 1 };
                    break 's;
                }
                if w.has(cur, &["SC"]) {
                    if w.at(cur + 2) == b'H' {
                        if w.has(cur + 3, &["OO", "ER", "EN", "UY", "ED", "EM"]) {
                            if w.has(cur + 3, &["ER", "EN"]) {
                                add!("X", "SK");
                            } else {
                                add!("SK", "SK");
                            }
                        } else if cur == 0 && !w.vowel(3) && w.at(3) != b'W' {
                            add!("X", "S");
                        } else {
                            add!("X", "X");
                        }
                        cur += 3;
                        break 's;
                    }
                    if w.has(cur + 2, &["I", "E", "Y"]) {
                        add!("S", "S");
                    } else {
                        add!("SK", "SK");
                    }
                    cur += 3;
                    break 's;
                }
                if cur == last && w.has(cur - 2, &["AI", "OI"]) {
                    add!("", "S");
                } else {
                    add!("S", "S");
                }
                cur += if w.has(cur + 1, &["S", "Z"]) { 2 } else { 1 };
            }
            b'T' => {
                if w.has(cur, &["TION"]) || w.has(cur, &["TIA", "TCH"]) {
                    add!("X", "X");
                    cur += 3;
                } else if w.has(cur, &["TH"]) || w.has(cur, &["TTH"]) {
                    if w.has(cur + 2, &["OM", "AM"]) || w.has(0, &["VAN ", "VON "]) || w.has(0, &["SCH"]) {
                        add!("T", "T");
                    } else {
                        add!("0", "T");
                    }
                    cur += 2;
                } else {
                    cur += if w.has(cur + 1, &["T", "D"]) { 2 } else { 1 };
                    add!("T", "T");
                }
            }
            b'V' => {
                cur += if w.at(cur + 1) == b'V' { 2 } else { 1 };
                add!("F", "F");
            }
            b'W' => 'w: {
                if w.has(cur, &["WR"]) {
                    add!("R", "R");
                    cur += 2;
                    break 'w;
                }
                if cur == 0 && (w.vowel(cur + 1) || w.has(cur, &["WH"])) {
                    if w.vowel(cur + 1) {
                        add!("A", "F");
                    } else {
                        add!("A", "A");
                    }
                }
                if (cur == last && w.vowel(cur - 1))
                    || w.has(cur - 1, &["EWSKI", "EWSKY", "OWSKI", "OWSKY"])
                    || w.has(0, &["SCH"])
                {
                    add!("", "F");
                    cur += 1;
                    break 'w;
                }
                if w.has(cur, &["WICZ", "WITZ"]) {
                    add!("TS", "FX");
                    cur += 4;
                    break 'w;
                }
                cur += 1;
            }
            b'X' => {
                if !(cur == last && (w.has(cur - 3, &["IAU", "EAU"]) || w.has(cur - 2, &["AU", "OU"]))) {
                    add!("KS", "KS");
                }
                cur += if w.has(cur + 1, &["C", "X"]) { 2 } else { 1 };
            }
            b'Z' => {
                if w.at(cur + 1) == b'H' {
                    add!("J", "J");
                    cur += 2;
                } else {
                    if w.has(cur + 1, &["ZO", "ZI", "ZA"]) || (w.slavo_germanic() && cur > 0 && w.at(cur - 1) != b'T') {
                        add!("S", "TS");
                    } else {
                        add!("S", "S");
                    }
                    cur += if w.at(cur + 1) == b'Z' { 2 } else { 1 };
                }
            }
            _ => cur += 1,
        }
    }
    p.truncate(4);
    q.truncate(4);
    (p, q)
}

#[cfg(test)]
mod tests {
    use super::double_metaphone;

    #[test]
    fn codes() {
        let dm = |s: &str| {
            let (p, q) = double_metaphone(s.as_bytes());
            (String::from_utf8(p).unwrap(), String::from_utf8(q).unwrap())
        };
        assert_eq!(dm("Thompson"), ("TMPS".into(), "TMPS".into()));
        assert_eq!(dm("smith"), ("SM0".into(), "XMT".into()));
        assert_eq!(dm("schmidt"), ("XMT".into(), "SMT".into()));
        assert_eq!(dm("john"), ("JN".into(), "AN".into()));
        assert_eq!(dm("xavier"), ("SF".into(), "SFR".into()));
    }
}
